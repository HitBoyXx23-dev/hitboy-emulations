//! AArch64 Advanced SIMD (NEON): vector integer and floating-point arithmetic, lane moves,
//! shuffles, shifts, immediates, table lookups, and the vector load/store forms.
//!
//! Vector registers are `Cpu::q[n]` as raw 128 bits. A 64-bit ("Q = 0") operation works on the
//! low half and clears the upper half of its destination.

use super::*;

fn lmask(bits: u32) -> u64 {
    if bits >= 64 {
        !0
    } else {
        (1u64 << bits) - 1
    }
}

fn lane(v: u128, esize: u32, i: u32) -> u64 {
    ((v >> (i * esize)) as u64) & lmask(esize)
}

fn put(v: &mut u128, esize: u32, i: u32, x: u64) {
    let sh = i * esize;
    let m = (lmask(esize) as u128) << sh;
    *v = (*v & !m) | (((x & lmask(esize)) as u128) << sh);
}

fn width(q: bool) -> u32 {
    if q {
        128
    } else {
        64
    }
}

fn keep(v: u128, q: bool) -> u128 {
    if q {
        v
    } else {
        v & u64::MAX as u128
    }
}

fn fl(esize: u32, x: u64) -> f64 {
    if esize == 64 {
        f64::from_bits(x)
    } else {
        f32::from_bits(x as u32) as f64
    }
}

fn fb(esize: u32, v: f64) -> u64 {
    if esize == 64 {
        v.to_bits()
    } else {
        (v as f32).to_bits() as u64
    }
}

/// Fills a register by repeating `elem` (of `esize` bits) across `bits`.
fn replicate_elem(elem: u64, esize: u32, bits: u32) -> u128 {
    let mut v = 0u128;
    for k in 0..bits / esize {
        put(&mut v, esize, k, elem);
    }
    v
}

fn shl(x: u64, s: u32) -> u64 {
    if s >= 64 {
        0
    } else {
        x << s
    }
}

fn shr(x: u64, s: u32) -> u64 {
    if s >= 64 {
        0
    } else {
        x >> s
    }
}

fn sar(x: i64, s: u32) -> i64 {
    x >> s.min(63)
}

impl Cpu {
    fn set_vec(&mut self, r: u32, v: u128, q: bool) {
        self.q[r as usize] = keep(v, q);
    }

    /// Decodes a vector (Advanced SIMD) instruction. False if it isn't one this core handles.
    pub(super) fn simd(&mut self, i: u32) -> bool {
        if i >> 31 != 0 {
            return false;
        }
        let q = (i >> 30) & 1 != 0;
        let u = (i >> 29) & 1 != 0;
        let (rd, rn, rm) = (i & 31, (i >> 5) & 31, (i >> 16) & 31);
        let size = (i >> 22) & 3;
        if (i >> 28) & 1 != 0 {
            // ARMv8 Cryptography SHA-1 / SHA-256: 0101 1110 00...
            if q && !u && (i >> 24) & 0xF == 0xE && size == 0 {
                if self.simd_crypto_sha(i, rd, rn, rm) {
                    return true;
                }
            }
            // Advanced SIMD scalar encodings: 01 U 11110 ...
            if q && (i >> 24) & 0xF == 0xE && (i >> 21) & 1 == 1 && (i >> 17) & 0xF == 0 && (i >> 10) & 3 == 0b10 {
                return self.simd_scalar_two_reg(i, u, size, rd, rn);
            }
            return false;
        }
        match (i >> 24) & 0xF {
            0xF => {
                if (i >> 19) & 0x1F == 0 && (i >> 10) & 1 == 1 {
                    self.simd_modified_imm(i, q, rd)
                } else if (i >> 23) & 1 == 0 && (i >> 19) & 0xF != 0 && (i >> 10) & 1 == 1 {
                    self.simd_shift_imm(i, q, u, rd, rn)
                } else if (i >> 10) & 1 == 0 {
                    self.simd_indexed(i, q, u, size, rd, rn)
                } else {
                    false
                }
            }
            0xE => {
                if (i >> 21) & 1 == 1 {
                    // Bit 10 set: three registers of the same type. Otherwise bit 11 (an opcode bit in
                    // the long form, fixed in the two-register forms) tells them apart.
                    if (i >> 10) & 1 == 1 {
                        self.simd_three_same(i, q, u, size, rd, rn, rm)
                    } else if (i >> 11) & 1 == 1 {
                        match (i >> 17) & 0xF {
                            0 => self.simd_two_reg(i, q, u, size, rd, rn),
                            0b0100 if q && !u && size == 0 => self.simd_crypto_aes(i, rd, rn),
                            0b1000 => self.simd_across(i, q, u, size, rd, rn),
                            _ => false,
                        }
                    } else {
                        self.simd_three_diff(i, q, u, size, rd, rn, rm)
                    }
                } else if (i >> 21) & 0xFF == 0b0111_0000 && (i >> 15) & 1 == 0 && (i >> 10) & 1 == 1 {
                    self.simd_copy(i, q, u, rd, rn)
                } else if (i >> 15) & 1 == 0 {
                    // EXT keeps its immediate in bits 14:11, so only bit 10 identifies it.
                    match (u, (i >> 10) & 3) {
                        (true, 0 | 2) if size == 0 && (i >> 10) & 1 == 0 => self.simd_ext(i, q, rd, rn, rm),
                        (false, 0) if size == 0 => self.simd_tbl(i, q, rd, rn, rm),
                        (false, 2) => self.simd_permute(i, q, size, rd, rn, rm),
                        _ => false,
                    }
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------ three registers, same type

    fn simd_three_same(&mut self, i: u32, q: bool, u: bool, size: u32, rd: u32, rn: u32, rm: u32) -> bool {
        let opc = (i >> 11) & 0x1F;
        let (a, b, old) = (self.q[rn as usize], self.q[rm as usize], self.q[rd as usize]);
        let w = width(q);
        if opc == 0x03 {
            let r = match (u, size) {
                (false, 0) => a & b,
                (false, 1) => a & !b,
                (false, 2) => a | b,
                (false, _) => a | !b,
                (true, 0) => a ^ b,
                (true, 1) => (old & a) | (!old & b),
                (true, 2) => (a & b) | (old & !b),
                (true, _) => (a & !b) | (old & b),
            };
            self.set_vec(rd, r, q);
            return true;
        }
        if opc >= 0x18 {
            // Floating point: bit 22 picks single/double, bit 23 picks the variant.
            let es = if size & 1 == 1 { 64 } else { 32 };
            if es == 64 && !q {
                return false;
            }
            let alt = size >> 1 == 1;
            let n = w / es;
            let mut r = 0u128;
            for k in 0..n {
                let (x, y, d) = (fl(es, lane(a, es, k)), fl(es, lane(b, es, k)), fl(es, lane(old, es, k)));
                let v = match (opc, u, alt) {
                    (0x1A, false, false) => fb(es, x + y),
                    (0x1A, false, true) => fb(es, x - y),
                    (0x1A, true, true) => fb(es, (x - y).abs()),
                    (0x1B, true, false) => fb(es, x * y),
                    (0x1F, true, false) => fb(es, x / y),
                    (0x19, false, false) => fb(es, if es == 64 { x.mul_add(y, d) } else { (x as f32).mul_add(y as f32, d as f32) as f64 }),
                    (0x19, false, true) => fb(es, if es == 64 { (-x).mul_add(y, d) } else { (-(x as f32)).mul_add(y as f32, d as f32) as f64 }),
                    (0x1E, false, false) => fb(es, x.max(y)),
                    (0x1E, false, true) => fb(es, x.min(y)),
                    (0x1C, false, false) => {
                        if x == y {
                            lmask(es)
                        } else {
                            0
                        }
                    }
                    (0x1C, true, false) => {
                        if x >= y {
                            lmask(es)
                        } else {
                            0
                        }
                    }
                    (0x1C, true, true) => {
                        if x > y {
                            lmask(es)
                        } else {
                            0
                        }
                    }
                    (0x1A, true, false) if k * 2 < n => {
                        // FADDP: pairs from Vn then Vm
                        let (p, qq) = (fl(es, lane(a, es, 2 * k)), fl(es, lane(a, es, 2 * k + 1)));
                        fb(es, p + qq)
                    }
                    (0x1A, true, false) => {
                        let j = k - n / 2;
                        let (p, qq) = (fl(es, lane(b, es, 2 * j)), fl(es, lane(b, es, 2 * j + 1)));
                        fb(es, p + qq)
                    }
                    _ => return false,
                };
                put(&mut r, es, k, v);
            }
            self.set_vec(rd, r, q);
            return true;
        }
        let es = 8 << size;
        if es == 64 && !q && opc != 0x17 {
            return false;
        }
        let n = w / es;
        let sgn = |x: u64| sext(x, es);
        let mut r = 0u128;
        if opc == 0x17 && !u {
            // ADDP: add adjacent pairs of Vn:Vm
            for k in 0..n {
                let (src, j) = if k < n / 2 { (a, k) } else { (b, k - n / 2) };
                put(&mut r, es, k, lane(src, es, 2 * j).wrapping_add(lane(src, es, 2 * j + 1)));
            }
            self.set_vec(rd, r, q);
            return true;
        }
        for k in 0..n {
            let (x, y, d) = (lane(a, es, k), lane(b, es, k), lane(old, es, k));
            let v = match (opc, u) {
                (0x10, false) => x.wrapping_add(y),
                (0x10, true) => x.wrapping_sub(y),
                (0x11, false) => {
                    if x & y != 0 {
                        lmask(es)
                    } else {
                        0
                    }
                }
                (0x11, true) => {
                    if x == y {
                        lmask(es)
                    } else {
                        0
                    }
                }
                (0x06, false) => (sgn(x) > sgn(y)) as u64 * lmask(es),
                (0x06, true) => (x > y) as u64 * lmask(es),
                (0x07, false) => (sgn(x) >= sgn(y)) as u64 * lmask(es),
                (0x07, true) => (x >= y) as u64 * lmask(es),
                (0x13, false) if size < 3 => x.wrapping_mul(y),
                (0x12, false) if size < 3 => d.wrapping_add(x.wrapping_mul(y)),
                (0x12, true) if size < 3 => d.wrapping_sub(x.wrapping_mul(y)),
                (0x0C, false) => if sgn(x) >= sgn(y) { x } else { y },
                (0x0C, true) => x.max(y),
                (0x0D, false) => if sgn(x) <= sgn(y) { x } else { y },
                (0x0D, true) => x.min(y),
                (0x08, _) => {
                    // SSHL / USHL: shift by the signed low byte of Vm
                    let s = y as u8 as i8 as i32;
                    if s >= 0 {
                        shl(x, s as u32)
                    } else if u {
                        shr(x, (-s) as u32)
                    } else {
                        sar(sgn(x), (-s) as u32) as u64
                    }
                }
                _ => return false,
            };
            put(&mut r, es, k, v);
        }
        self.set_vec(rd, r, q);
        true
    }

    // ------------------------------------------------------------ three registers, long forms

    fn simd_three_diff(&mut self, i: u32, q: bool, u: bool, size: u32, rd: u32, rn: u32, rm: u32) -> bool {
        let opc = (i >> 12) & 0xF;
        let (a, b, old) = (self.q[rn as usize], self.q[rm as usize], self.q[rd as usize]);
        if opc == 0xE && !u && (size == 0 || size == 3) {
            // PMULL / PMULL2: carry-less (polynomial) multiply long (8x8->16 or 64x64->128)
            if size == 3 {
                let x = if q { (a >> 64) as u64 } else { a as u64 };
                let y = if q { (b >> 64) as u64 } else { b as u64 };
                let mut acc = 0u128;
                for bit in 0..64u32 {
                    if (y >> bit) & 1 != 0 {
                        acc ^= (x as u128) << bit;
                    }
                }
                self.q[rd as usize] = acc;
                return true;
            }
            let base = if q { 8 } else { 0 };
            let mut out = 0u128;
            for k in 0..8u32 {
                let x = lane(a, 8, base + k);
                let y = lane(b, 8, base + k);
                let mut p = 0u64;
                for bit in 0..8u32 {
                    if (y >> bit) & 1 != 0 {
                        p ^= x << bit;
                    }
                }
                put(&mut out, 16, k, p);
            }
            self.q[rd as usize] = out;
            return true;
        }
        if size > 2 {
            return false;
        }
        let es = 8 << size;
        let n = 64 / es;
        let base = if q { n } else { 0 };
        let mut r = 0u128;
        for k in 0..n {
            let (x, y) = (lane(a, es, base + k), lane(b, es, base + k));
            let (x, y) = if u { (x as i128, y as i128) } else { (sext(x, es) as i128, sext(y, es) as i128) };
            let d = lane(old, es * 2, k) as i128;
            let v = match opc {
                0x0 => x + y,
                0x2 => x - y,
                0xC => x * y,
                0x8 => d + x * y,
                0xA => d - x * y,
                _ => return false,
            };
            put(&mut r, es * 2, k, v as u64);
        }
        self.q[rd as usize] = r;
        true
    }

    // ------------------------------------------------------------ two registers

    /// ARMv8-A Cryptography AES instructions (`AESE`, `AESD`, `AESMC`, `AESIMC`).
    fn simd_crypto_aes(&mut self, i: u32, rd: u32, rn: u32) -> bool {
        if ((i >> 14) & 7) != 0b001 {
            return false;
        }
        const SBOX: [u8; 256] = [
            0x63,0x7c,0x77,0x7b,0xf2,0x6b,0x6f,0xc5,0x30,0x01,0x67,0x2b,0xfe,0xd7,0xab,0x76,
            0xca,0x82,0xc9,0x7d,0xfa,0x59,0x47,0xf0,0xad,0xd4,0xa2,0xaf,0x9c,0xa4,0x72,0xc0,
            0xb7,0xfd,0x93,0x26,0x36,0x3f,0xf7,0xcc,0x34,0xa5,0xe5,0xf1,0x71,0xd8,0x31,0x15,
            0x04,0xc7,0x23,0xc3,0x18,0x96,0x05,0x9a,0x07,0x12,0x80,0xe2,0xeb,0x27,0xb2,0x75,
            0x09,0x83,0x2c,0x1a,0x1b,0x6e,0x5a,0xa0,0x52,0x3b,0xd6,0xb3,0x29,0xe3,0x2f,0x84,
            0x53,0xd1,0x00,0xed,0x20,0xfc,0xb1,0x5b,0x6a,0xcb,0xbe,0x39,0x4a,0x4c,0x58,0xcf,
            0xd0,0xef,0xaa,0xfb,0x43,0x4d,0x33,0x85,0x45,0xf9,0x02,0x7f,0x50,0x3c,0x9f,0xa8,
            0x51,0xa3,0x40,0x8f,0x92,0x9d,0x38,0xf5,0xbc,0xb6,0xda,0x21,0x10,0xff,0xf3,0xd2,
            0xcd,0x0c,0x13,0xec,0x5f,0x97,0x44,0x17,0xc4,0xa7,0x7e,0x3d,0x64,0x5d,0x19,0x73,
            0x60,0x81,0x4f,0xdc,0x22,0x2a,0x90,0x88,0x46,0xee,0xb8,0x14,0xde,0x5e,0x0b,0xdb,
            0xe0,0x32,0x3a,0x0a,0x49,0x06,0x24,0x5c,0xc2,0xd3,0xac,0x62,0x91,0x95,0xe4,0x79,
            0xe7,0xc8,0x37,0x6d,0x8d,0xd5,0x4e,0xa9,0x6c,0x56,0xf4,0xea,0x65,0x7a,0xae,0x08,
            0xba,0x78,0x25,0x2e,0x1c,0xa6,0xb4,0xc6,0xe8,0xdd,0x74,0x1f,0x4b,0xbd,0x8b,0x8a,
            0x70,0x3e,0xb5,0x66,0x48,0x03,0xf6,0x0e,0x61,0x35,0x57,0xb9,0x86,0xc1,0x1d,0x9e,
            0xe1,0xf8,0x98,0x11,0x69,0xd9,0x8e,0x94,0x9b,0x1e,0x87,0xe9,0xce,0x55,0x28,0xdf,
            0x8c,0xa1,0x89,0x0d,0xbf,0xe6,0x42,0x68,0x41,0x99,0x2d,0x0f,0xb0,0x54,0xbb,0x16,
        ];
        const INV_SBOX: [u8; 256] = [
            0x52,0x09,0x6a,0xd5,0x30,0x36,0xa5,0x38,0xbf,0x40,0xa3,0x9e,0x81,0xf3,0xd7,0xfb,
            0x7c,0xe3,0x39,0x82,0x9b,0x2f,0xff,0x87,0x34,0x8e,0x43,0x44,0xc4,0xde,0xe9,0xcb,
            0x54,0x7b,0x94,0x32,0xa6,0xc2,0x23,0x3d,0xee,0x4c,0x95,0x0b,0x42,0xfa,0xc3,0x4e,
            0x08,0x2e,0xa1,0x66,0x28,0xd9,0x24,0xb2,0x76,0x5b,0xa2,0x49,0x6d,0x8b,0xd1,0x25,
            0x72,0xf8,0xf6,0x64,0x86,0x68,0x98,0x16,0xd4,0xa4,0x5c,0xcc,0x5d,0x65,0xb6,0x92,
            0x6c,0x70,0x48,0x50,0xfd,0xed,0xb9,0xda,0x5e,0x15,0x46,0x57,0xa7,0x8d,0x9d,0x84,
            0x90,0xd8,0xab,0x00,0x8c,0xbc,0xd3,0x0a,0xf7,0xe4,0x58,0x05,0xb8,0xb3,0x45,0x06,
            0xd0,0x2c,0x1e,0x8f,0xca,0x3f,0x0f,0x02,0xc1,0xaf,0xbd,0x03,0x01,0x13,0x8a,0x6b,
            0x3a,0x91,0x11,0x41,0x4f,0x67,0xdc,0xea,0x97,0xf2,0xcf,0xce,0xf0,0xb4,0xe6,0x73,
            0x96,0xac,0x74,0x22,0xe7,0xad,0x35,0x85,0xe2,0xf9,0x37,0xe8,0x1c,0x75,0xdf,0x6e,
            0x47,0xf1,0x1a,0x71,0x1d,0x29,0xc5,0x89,0x6f,0xb7,0x62,0x0e,0xaa,0x18,0xbe,0x1b,
            0xfc,0x56,0x3e,0x4b,0xc6,0xd2,0x79,0x20,0x9a,0xdb,0xc0,0xfe,0x78,0xcd,0x5a,0xf4,
            0x1f,0xdd,0xa8,0x33,0x88,0x07,0xc7,0x31,0xb1,0x12,0x10,0x59,0x27,0x80,0xec,0x5f,
            0x60,0x51,0x7f,0xa9,0x19,0xb5,0x4a,0x0d,0x2d,0xe5,0x7a,0x9f,0x93,0xc9,0x9c,0xef,
            0xa0,0xe0,0x3b,0x4d,0xae,0x2a,0xf5,0xb0,0xc8,0xeb,0xbb,0x3c,0x83,0x53,0x99,0x61,
            0x17,0x2b,0x04,0x7e,0xba,0x77,0xd6,0x26,0xe1,0x69,0x14,0x63,0x55,0x21,0x0c,0x7d,
        ];
        let xtime = |x: u8| -> u8 { (x << 1) ^ if x & 0x80 != 0 { 0x1B } else { 0 } };
        let gmul = |mut a: u8, mut b: u8| -> u8 {
            let mut p = 0u8;
            for _ in 0..8 {
                if b & 1 != 0 {
                    p ^= a;
                }
                a = xtime(a);
                b >>= 1;
            }
            p
        };
        let opc = (i >> 12) & 3;
        let mut st = [0u8; 16];
        match opc {
            0 => {
                // AESE: AddRoundKey -> ShiftRows -> SubBytes
                let x = (self.q[rd as usize] ^ self.q[rn as usize]).to_le_bytes();
                for c in 0..4 {
                    for r in 0..4 {
                        st[c * 4 + r] = SBOX[x[((c + r) & 3) * 4 + r] as usize];
                    }
                }
            }
            1 => {
                // AESD: AddRoundKey -> InvShiftRows -> InvSubBytes
                let x = (self.q[rd as usize] ^ self.q[rn as usize]).to_le_bytes();
                for c in 0..4 {
                    for r in 0..4 {
                        st[c * 4 + r] = INV_SBOX[x[((c + 4 - r) & 3) * 4 + r] as usize];
                    }
                }
            }
            2 => {
                // AESMC: MixColumns
                let x = self.q[rn as usize].to_le_bytes();
                for c in 0..4 {
                    let (b0, b1, b2, b3) = (x[c * 4], x[c * 4 + 1], x[c * 4 + 2], x[c * 4 + 3]);
                    st[c * 4] = gmul(2, b0) ^ gmul(3, b1) ^ b2 ^ b3;
                    st[c * 4 + 1] = b0 ^ gmul(2, b1) ^ gmul(3, b2) ^ b3;
                    st[c * 4 + 2] = b0 ^ b1 ^ gmul(2, b2) ^ gmul(3, b3);
                    st[c * 4 + 3] = gmul(3, b0) ^ b1 ^ b2 ^ gmul(2, b3);
                }
            }
            _ => {
                // AESIMC: InvMixColumns
                let x = self.q[rn as usize].to_le_bytes();
                for c in 0..4 {
                    let (b0, b1, b2, b3) = (x[c * 4], x[c * 4 + 1], x[c * 4 + 2], x[c * 4 + 3]);
                    st[c * 4] = gmul(14, b0) ^ gmul(11, b1) ^ gmul(13, b2) ^ gmul(9, b3);
                    st[c * 4 + 1] = gmul(9, b0) ^ gmul(14, b1) ^ gmul(11, b2) ^ gmul(13, b3);
                    st[c * 4 + 2] = gmul(13, b0) ^ gmul(9, b1) ^ gmul(14, b2) ^ gmul(11, b3);
                    st[c * 4 + 3] = gmul(11, b0) ^ gmul(13, b1) ^ gmul(9, b2) ^ gmul(14, b3);
                }
            }
        }
        self.q[rd as usize] = u128::from_le_bytes(st);
        true
    }

    /// ARMv8-A Cryptography SHA-1 and SHA-256 instructions (`SHA1C/P/M/H/SU0/SU1`, `SHA256H/H2/SU0/SU1`).
    fn simd_crypto_sha(&mut self, i: u32, rd: u32, rn: u32, rm: u32) -> bool {
        let unpack = |v: u128| -> [u32; 4] {
            [v as u32, (v >> 32) as u32, (v >> 64) as u32, (v >> 96) as u32]
        };
        let pack = |a: [u32; 4]| -> u128 {
            (a[0] as u128) | ((a[1] as u128) << 32) | ((a[2] as u128) << 64) | ((a[3] as u128) << 96)
        };
        // Two-register SHA: 0101 1110 0010 1000 00 opc(3) 10 Rn Rd
        if ((i >> 15) & 0x7F) == 0b101_0000 && ((i >> 10) & 3) == 0b10 {
            let opc = (i >> 12) & 7;
            let n = self.q[rn as usize];
            match opc {
                0 => {
                    // SHA1H Sd, Sn
                    self.q[rd as usize] = (n as u32).rotate_left(30) as u128;
                    return true;
                }
                1 => {
                    // SHA1SU1 Vd.4S, Vn.4S
                    let d = unpack(self.q[rd as usize]);
                    let nv = unpack(n);
                    let t0 = [d[0] ^ nv[1], d[1] ^ nv[2], d[2] ^ nv[3], d[3]];
                    let t1 = [t0[0].rotate_left(1), t0[1].rotate_left(1), t0[2].rotate_left(1), t0[3].rotate_left(1)];
                    self.q[rd as usize] = pack([t1[0], t1[1], t1[2], t1[3] ^ t1[0].rotate_left(1)]);
                    return true;
                }
                2 => {
                    // SHA256SU0 Vd.4S, Vn.4S
                    let x = unpack(self.q[rd as usize]);
                    let y = unpack(n);
                    let t = [x[1], x[2], x[3], y[0]];
                    let mut out = [0u32; 4];
                    for e in 0..4 {
                        let sig0 = t[e].rotate_right(7) ^ t[e].rotate_right(18) ^ (t[e] >> 3);
                        out[e] = x[e].wrapping_add(sig0);
                    }
                    self.q[rd as usize] = pack(out);
                    return true;
                }
                _ => return false,
            }
        }
        // Three-register SHA: 0101 1110 000 Rm 0 opc(3) 00 Rn Rd
        if ((i >> 21) & 1) == 0 && ((i >> 15) & 1) == 0 && ((i >> 10) & 3) == 0 {
            let opc = (i >> 12) & 7;
            let d = unpack(self.q[rd as usize]);
            let n = unpack(self.q[rn as usize]);
            let m = unpack(self.q[rm as usize]);
            match opc {
                0 | 1 | 2 => {
                    // SHA1C / SHA1P / SHA1M Qd, Sn, Vm.4S
                    let mut x = d;
                    let mut y = n[0];
                    for &we in &m {
                        let t = match opc {
                            0 => (x[1] & x[2]) | (!x[1] & x[3]),
                            1 => x[1] ^ x[2] ^ x[3],
                            _ => (x[1] & x[2]) | (x[1] & x[3]) | (x[2] & x[3]),
                        };
                        y = y.wrapping_add(x[0].rotate_left(5)).wrapping_add(t).wrapping_add(we);
                        x[1] = x[1].rotate_left(30);
                        let ny = x[3];
                        x = [y, x[0], x[1], x[2]];
                        y = ny;
                    }
                    self.q[rd as usize] = pack(x);
                    return true;
                }
                3 => {
                    // SHA1SU0 Vd.4S, Vn.4S, Vm.4S
                    let t = [d[2], d[3], n[0], n[1]];
                    self.q[rd as usize] = pack([t[0] ^ d[0] ^ m[0], t[1] ^ d[1] ^ m[1], t[2] ^ d[2] ^ m[2], t[3] ^ d[3] ^ m[3]]);
                    return true;
                }
                4 | 5 => {
                    // SHA256H (opc=4): x=Qd(a..d), y=Qn(e..h) -> returns x
                    // SHA256H2 (opc=5): x=Qn(a..d), y=Qd(e..h) -> returns y
                    let (mut x, mut y) = if opc == 4 { (d, n) } else { (n, d) };
                    for &we in &m {
                        let ch = (y[0] & y[1]) ^ (!y[0] & y[2]);
                        let maj = (x[0] & x[1]) ^ (x[0] & x[2]) ^ (x[1] & x[2]);
                        let s1 = y[0].rotate_right(6) ^ y[0].rotate_right(11) ^ y[0].rotate_right(25);
                        let s0 = x[0].rotate_right(2) ^ x[0].rotate_right(13) ^ x[0].rotate_right(22);
                        let t1 = y[3].wrapping_add(s1).wrapping_add(ch).wrapping_add(we);
                        let t2 = s0.wrapping_add(maj);
                        let nx = [t1.wrapping_add(t2), x[0], x[1], x[2]];
                        let ny = [x[3].wrapping_add(t1), y[0], y[1], y[2]];
                        x = nx;
                        y = ny;
                    }
                    self.q[rd as usize] = pack(if opc == 4 { x } else { y });
                    return true;
                }
                6 => {
                    // SHA256SU1 Vd.4S, Vn.4S, Vm.4S
                    let t0 = [n[1], n[2], n[3], m[0]];
                    let mut out = [0u32; 4];
                    for e in 0..2 {
                        let z = m[2 + e];
                        let sig1 = z.rotate_right(17) ^ z.rotate_right(19) ^ (z >> 10);
                        out[e] = d[e].wrapping_add(t0[e]).wrapping_add(sig1);
                    }
                    for e in 0..2 {
                        let z = out[e];
                        let sig1 = z.rotate_right(17) ^ z.rotate_right(19) ^ (z >> 10);
                        out[2 + e] = d[2 + e].wrapping_add(t0[2 + e]).wrapping_add(sig1);
                    }
                    self.q[rd as usize] = pack(out);
                    return true;
                }
                _ => return false,
            }
        }
        false
    }

    fn simd_scalar_two_reg(&mut self, i: u32, u: bool, size: u32, rd: u32, rn: u32) -> bool {
        let opc = (i >> 12) & 0x1F;
        let raw = self.q[rn as usize] as u64;
        let fes = if size & 1 == 1 { 64 } else { 32 };
        let alt = size >> 1 == 1;
        let v = match (opc, u, alt) {
            (0x1D, false, false) => fb(fes, if fes == 64 { raw as i64 as f64 } else { raw as i32 as f32 as f64 }),
            (0x1D, true, false) => fb(fes, if fes == 64 { raw as f64 } else { raw as u32 as f32 as f64 }),
            (0x1B, false, true) => {
                let f = fl(fes, raw);
                if fes == 64 { f as i64 as u64 } else { (f as i32 as u32) as u64 }
            }
            (0x1B, true, true) => {
                let f = fl(fes, raw);
                if fes == 64 { f as u64 } else { (f as u32) as u64 }
            }
            (0x1A, false, false) => {
                let f = fl(fes, raw).round();
                if fes == 64 { f as i64 as u64 } else { (f as i32 as u32) as u64 }
            }
            (0x1A, true, false) => {
                let f = fl(fes, raw).round();
                if fes == 64 { f as u64 } else { (f as u32) as u64 }
            }
            (0x0B, false, _) if size == 3 => (raw as i64).wrapping_abs() as u64,
            (0x0B, true, _) if size == 3 => (raw as i64).wrapping_neg() as u64,
            _ => return false,
        };
        self.q[rd as usize] = (v & lmask(if opc == 0x0B { 64 } else { fes })) as u128;
        true
    }

    fn simd_two_reg(&mut self, i: u32, q: bool, u: bool, size: u32, rd: u32, rn: u32) -> bool {
        let opc = (i >> 12) & 0x1F;
        let a = self.q[rn as usize];
        let w = width(q);
        let es = 8 << size;
        let sgn = |x: u64| sext(x, es);
        // Floating-point forms use bit 22 for the precision and bit 23 as an extra opcode bit.
        if matches!(opc, 0x0F | 0x1F | 0x1D | 0x1B) {
            let fes = if size & 1 == 1 { 64 } else { 32 };
            if fes == 64 && !q {
                return false;
            }
            let alt = size >> 1 == 1;
            let mut r = 0u128;
            for k in 0..w / fes {
                let raw = lane(a, fes, k);
                let v = match (opc, u, alt) {
                    (0x0F, false, true) => fb(fes, fl(fes, raw).abs()),
                    (0x0F, true, true) => fb(fes, -fl(fes, raw)),
                    (0x1F, true, true) => fb(fes, if fes == 64 { fl(fes, raw).sqrt() } else { (fl(fes, raw) as f32).sqrt() as f64 }),
                    (0x1D, false, false) => fb(fes, if fes == 64 { sext(raw, 64) as f64 } else { sext(raw, 32) as f32 as f64 }),
                    (0x1D, true, false) => fb(fes, if fes == 64 { raw as f64 } else { raw as u32 as f32 as f64 }),
                    (0x1B, false, true) => {
                        let f = fl(fes, raw);
                        if fes == 64 { f as i64 as u64 } else { (f as i32 as u32) as u64 }
                    }
                    (0x1B, true, true) => {
                        let f = fl(fes, raw);
                        if fes == 64 { f as u64 } else { (f as u32) as u64 }
                    }
                    _ => return false,
                };
                put(&mut r, fes, k, v);
            }
            self.set_vec(rd, r, q);
            return true;
        }
        let mut r = 0u128;
        match (opc, u) {
            (0x13, true) => {
                // SHLL / SHLL2: widen low/high half elements and shift left by element width
                if size > 2 {
                    return false;
                }
                let src = 8 << size;
                let dst = src * 2;
                let n = 64 / src;
                let base = if q { n } else { 0 };
                for k in 0..n {
                    put(&mut r, dst, k, lane(a, src, base + k) << src);
                }
                self.q[rd as usize] = r;
                return true;
            }
            (0x12, false) => {
                // XTN / XTN2: narrow each wide lane to half its width
                if size > 2 {
                    return false;
                }
                let (src, dst) = (16 << size, 8 << size);
                let mut narrow = 0u128;
                for k in 0..64 / dst {
                    put(&mut narrow, dst, k, lane(a, src, k));
                }
                self.q[rd as usize] = if q { (self.q[rd as usize] & u64::MAX as u128) | (narrow << 64) } else { narrow & u64::MAX as u128 };
                return true;
            }
            (0x05, true) if size == 1 => {
                // RBIT: reverse bits in every 8-bit byte (size=01 in encoding, always 8B/16B)
                for k in 0..w / 8 {
                    put(&mut r, 8, k, (lane(a, 8, k) as u8).reverse_bits() as u64);
                }
                self.set_vec(rd, r, q);
                return true;
            }
            (0x00, _) | (0x01, false) => {
                // REV64 / REV32 / REV16: reverse the elements inside each group
                let group = match (opc, u) {
                    (0x00, false) => 64,
                    (0x00, true) => 32,
                    _ => 16,
                };
                if es >= group {
                    return false;
                }
                let per = group / es;
                for g in 0..w / group {
                    for k in 0..per {
                        put(&mut r, es, g * per + k, lane(a, es, g * per + (per - 1 - k)));
                    }
                }
                self.set_vec(rd, r, q);
                return true;
            }
            _ => {}
        }
        if es == 64 && !q {
            return false;
        }
        for k in 0..w / es {
            let x = lane(a, es, k);
            let v = match (opc, u, size) {
                (0x05, false, 0) => x.count_ones() as u64,
                (0x05, true, 0) => !x,
                (0x0B, false, _) => sgn(x).wrapping_abs() as u64,
                (0x0B, true, _) => (sgn(x).wrapping_neg()) as u64,
                (0x08, false, _) => (sgn(x) > 0) as u64 * lmask(es),
                (0x08, true, _) => (sgn(x) >= 0) as u64 * lmask(es),
                (0x09, false, _) => (x == 0) as u64 * lmask(es),
                (0x09, true, _) => (sgn(x) <= 0) as u64 * lmask(es),
                (0x0A, false, _) => (sgn(x) < 0) as u64 * lmask(es),
                _ => return false,
            };
            put(&mut r, es, k, v);
        }
        self.set_vec(rd, r, q);
        true
    }

    // ------------------------------------------------------------ across lanes

    fn simd_across(&mut self, i: u32, q: bool, u: bool, size: u32, rd: u32, rn: u32) -> bool {
        let opc = (i >> 12) & 0x1F;
        let es = 8 << size;
        if es == 64 || (es == 32 && !q) {
            return false;
        }
        let a = self.q[rn as usize];
        let n = width(q) / es;
        let vals: Vec<u64> = (0..n).map(|k| lane(a, es, k)).collect();
        let sg = |x: u64| sext(x, es);
        let (out, out_es) = match (opc, u) {
            (0x1B, false) => (vals.iter().fold(0u64, |s, &x| s.wrapping_add(x)), es),
            (0x03, false) => (vals.iter().fold(0i64, |s, &x| s.wrapping_add(sg(x))) as u64, es * 2),
            (0x03, true) => (vals.iter().fold(0u64, |s, &x| s.wrapping_add(x)), es * 2),
            (0x0A, false) => (vals.iter().map(|&x| sg(x)).max().unwrap() as u64, es),
            (0x0A, true) => (*vals.iter().max().unwrap(), es),
            (0x1A, false) => (vals.iter().map(|&x| sg(x)).min().unwrap() as u64, es),
            (0x1A, true) => (*vals.iter().min().unwrap(), es),
            _ => return false,
        };
        self.q[rd as usize] = (out & lmask(out_es)) as u128;
        true
    }

    // ------------------------------------------------------------ copy: DUP, INS, UMOV, SMOV

    fn simd_copy(&mut self, i: u32, q: bool, op: bool, rd: u32, rn: u32) -> bool {
        let imm5 = (i >> 16) & 0x1F;
        if imm5 & 0xF == 0 && imm5 != 0b10000 {
            return false;
        }
        let tz = imm5.trailing_zeros();
        if tz > 3 {
            return false;
        }
        let es = 8u32 << tz;
        let index = imm5 >> (tz + 1);
        let imm4 = (i >> 11) & 0xF;
        let src = self.q[rn as usize];
        if op {
            // INS (element)
            let from = imm4 >> tz;
            let v = lane(src, es, from);
            let mut d = self.q[rd as usize];
            put(&mut d, es, index, v);
            self.q[rd as usize] = d;
            return true;
        }
        match imm4 {
            0 => {
                let v = replicate_elem(lane(src, es, index), es, width(q));
                self.set_vec(rd, v, q);
            }
            1 => {
                let v = replicate_elem(self.r(rn, true), es, width(q));
                self.set_vec(rd, v, q);
            }
            3 => {
                let mut d = self.q[rd as usize];
                put(&mut d, es, index, self.r(rn, true));
                self.q[rd as usize] = d;
            }
            5 => {
                let v = sext(lane(src, es, index), es) as u64;
                self.set(rd, v, q);
            }
            7 => {
                let v = lane(src, es, index);
                self.set(rd, v, q && es == 64);
            }
            _ => return false,
        }
        true
    }

    // ------------------------------------------------------------ modified immediate

    fn simd_modified_imm(&mut self, i: u32, q: bool, rd: u32) -> bool {
        let imm8 = ((((i >> 16) & 7) << 5) | ((i >> 5) & 0x1F)) as u64;
        let cmode = (i >> 12) & 0xF;
        let op = (i >> 29) & 1 != 0;
        let w = width(q);
        let (pattern, logical): (u128, bool) = match cmode {
            0..=7 => (replicate_elem(imm8 << (8 * (cmode >> 1)), 32, w), cmode & 1 == 1),
            8..=11 => (replicate_elem(imm8 << (8 * ((cmode >> 1) & 1)), 16, w), cmode & 1 == 1),
            12 => (replicate_elem((imm8 << 8) | 0xFF, 32, w), false),
            13 => (replicate_elem((imm8 << 16) | 0xFFFF, 32, w), false),
            14 if !op => (replicate_elem(imm8, 8, w), false),
            14 => {
                let mut v = 0u64;
                for b in 0..8 {
                    if imm8 >> b & 1 == 1 {
                        v |= 0xFFu64 << (8 * b);
                    }
                }
                (replicate_elem(v, 64, w), false)
            }
            _ if !op => (replicate_elem(vfp_expand(imm8 as u32, false), 32, w), false),
            _ if q => (replicate_elem(vfp_expand(imm8 as u32, true), 64, w), false),
            _ => return false,
        };
        let old = self.q[rd as usize];
        let r = if logical {
            if op {
                old & !pattern
            } else {
                old | pattern
            }
        } else if op && cmode < 14 {
            !pattern
        } else {
            pattern
        };
        self.set_vec(rd, r, q);
        true
    }

    // ------------------------------------------------------------ shifts by immediate

    fn simd_shift_imm(&mut self, i: u32, q: bool, u: bool, rd: u32, rn: u32) -> bool {
        let immh = (i >> 19) & 0xF;
        let immhb = (immh << 3) | ((i >> 16) & 7);
        let es = 8u32 << (31 - immh.leading_zeros());
        let opc = (i >> 11) & 0x1F;
        let a = self.q[rn as usize];
        let w = width(q);
        let right = 2 * es - immhb;
        let left = immhb - es;
        let sgn = |x: u64| sext(x, es);
        let mut r = 0u128;
        match opc {
            0x00 | 0x02 | 0x0A => {
                if es == 64 && !q {
                    return false;
                }
                let old = self.q[rd as usize];
                for k in 0..w / es {
                    let x = lane(a, es, k);
                    let v = match opc {
                        0x0A if !u => shl(x, left),
                        0x0A => {
                            // SLI: shift left and insert
                            let m = shl(lmask(es), left);
                            (lane(old, es, k) & !m) | (shl(x, left) & m)
                        }
                        _ => {
                            let sh = if u { shr(x, right) } else { sar(sgn(x), right) as u64 };
                            if opc == 0x02 {
                                lane(old, es, k).wrapping_add(sh)
                            } else {
                                sh
                            }
                        }
                    };
                    put(&mut r, es, k, v);
                }
                self.set_vec(rd, r, q);
            }
            0x14 => {
                // SSHLL / USHLL (UXTL, SXTL when the shift is 0)
                if es == 64 {
                    return false;
                }
                let n = 64 / es;
                let base = if q { n } else { 0 };
                for k in 0..n {
                    let x = lane(a, es, base + k);
                    let wide = if u { x } else { sext(x, es) as u64 };
                    put(&mut r, es * 2, k, shl(wide, left));
                }
                self.q[rd as usize] = r;
            }
            0x10 if !u => {
                // SHRN / SHRN2: the source lanes are twice the destination lane size
                if es == 64 {
                    return false;
                }
                let mut narrow = 0u128;
                for k in 0..64 / es {
                    put(&mut narrow, es, k, shr(lane(a, es * 2, k), right));
                }
                self.q[rd as usize] = if q { (self.q[rd as usize] & u64::MAX as u128) | (narrow << 64) } else { narrow };
            }
            _ => return false,
        }
        true
    }

    // ------------------------------------------------------------ by-element forms

    fn simd_indexed(&mut self, i: u32, q: bool, u: bool, size: u32, rd: u32, rn: u32) -> bool {
        let opc = (i >> 12) & 0xF;
        let (h, l, m) = ((i >> 11) & 1, (i >> 21) & 1, (i >> 20) & 1);
        let rm4 = (i >> 16) & 0xF;
        let a = self.q[rn as usize];
        let old = self.q[rd as usize];
        let w = width(q);
        if matches!(opc, 0x1 | 0x5 | 0x9) && size >= 2 {
            let es = if size & 1 == 1 { 64 } else { 32 };
            if es == 64 && !q {
                return false;
            }
            let (idx, reg) = if es == 32 { ((h << 1) | l, (m << 4) | rm4) } else { (h, (m << 4) | rm4) };
            let y = fl(es, lane(self.q[reg as usize], es, idx));
            let mut r = 0u128;
            for k in 0..w / es {
                let x = fl(es, lane(a, es, k));
                let d = fl(es, lane(old, es, k));
                let v = match (opc, u) {
                    (0x9, false) => x * y,
                    (0x1, false) => if es == 64 { x.mul_add(y, d) } else { (x as f32).mul_add(y as f32, d as f32) as f64 },
                    (0x5, false) => if es == 64 { (-x).mul_add(y, d) } else { (-(x as f32)).mul_add(y as f32, d as f32) as f64 },
                    _ => return false,
                };
                put(&mut r, es, k, fb(es, v));
            }
            self.set_vec(rd, r, q);
            return true;
        }
        // Integer MUL / MLA / MLS by element (16- and 32-bit lanes)
        if !matches!((opc, u), (0x8, false) | (0x0, true) | (0x4, true)) || !(1..=2).contains(&size) {
            return false;
        }
        let es = 8 << size;
        let (idx, reg) = if size == 1 { ((h << 2) | (l << 1) | m, rm4) } else { ((h << 1) | l, (m << 4) | rm4) };
        let y = lane(self.q[reg as usize], es, idx);
        let mut r = 0u128;
        for k in 0..w / es {
            let x = lane(a, es, k);
            let d = lane(old, es, k);
            let p = x.wrapping_mul(y);
            let v = match (opc, u) {
                (0x8, _) => p,
                (0x0, _) => d.wrapping_add(p),
                _ => d.wrapping_sub(p),
            };
            put(&mut r, es, k, v);
        }
        self.set_vec(rd, r, q);
        true
    }

    // ------------------------------------------------------------ shuffles

    fn simd_permute(&mut self, i: u32, q: bool, size: u32, rd: u32, rn: u32, rm: u32) -> bool {
        let es = 8 << size;
        if es == 64 && !q {
            return false;
        }
        let n = width(q) / es;
        let (a, b) = (self.q[rn as usize], self.q[rm as usize]);
        let mut r = 0u128;
        let half = n / 2;
        match (i >> 12) & 7 {
            1 | 5 => {
                let odd = (i >> 14) & 1;
                for k in 0..half {
                    put(&mut r, es, k, lane(a, es, 2 * k + odd));
                    put(&mut r, es, half + k, lane(b, es, 2 * k + odd));
                }
            }
            2 | 6 => {
                let odd = (i >> 14) & 1;
                for k in 0..half {
                    put(&mut r, es, 2 * k, lane(a, es, 2 * k + odd));
                    put(&mut r, es, 2 * k + 1, lane(b, es, 2 * k + odd));
                }
            }
            3 | 7 => {
                let base = if (i >> 14) & 1 == 1 { half } else { 0 };
                for k in 0..half {
                    put(&mut r, es, 2 * k, lane(a, es, base + k));
                    put(&mut r, es, 2 * k + 1, lane(b, es, base + k));
                }
            }
            _ => return false,
        }
        self.set_vec(rd, r, q);
        true
    }

    fn simd_ext(&mut self, i: u32, q: bool, rd: u32, rn: u32, rm: u32) -> bool {
        let pos = (i >> 11) & 0xF;
        let bytes = if q { 16 } else { 8 };
        if !q && pos >= 8 {
            return false;
        }
        let (a, b) = (self.q[rn as usize], self.q[rm as usize]);
        let mut r = 0u128;
        for k in 0..bytes {
            let src = k + pos;
            let byte = if src < bytes { lane(a, 8, src) } else { lane(b, 8, src - bytes) };
            put(&mut r, 8, k, byte);
        }
        self.set_vec(rd, r, q);
        true
    }

    fn simd_tbl(&mut self, i: u32, q: bool, rd: u32, rn: u32, rm: u32) -> bool {
        let len = ((i >> 13) & 3) + 1;
        let ext = (i >> 12) & 1 == 1;
        let idx = self.q[rm as usize];
        let old = self.q[rd as usize];
        let mut r = 0u128;
        for k in 0..if q { 16 } else { 8 } {
            let ix = lane(idx, 8, k) as u32;
            let byte = if ix < 16 * len {
                lane(self.q[((rn + ix / 16) % 32) as usize], 8, ix % 16)
            } else if ext {
                lane(old, 8, k)
            } else {
                0
            };
            put(&mut r, 8, k, byte);
        }
        self.set_vec(rd, r, q);
        true
    }

    // ------------------------------------------------------------ vector loads and stores

    /// LD1-LD4 / ST1-ST4 (multiple and single structure) and LD1R.
    pub(super) fn simd_ldst(&mut self, i: u32, m: &mut impl Mem) -> Result<bool, Exit> {
        let q = (i >> 30) & 1 != 0;
        let load = (i >> 22) & 1 != 0;
        let post = (i >> 23) & 1 != 0;
        let (rn, rt, rm) = ((i >> 5) & 31, i & 31, (i >> 16) & 31);
        let base = self.r_sp(rn);
        let single = (i >> 24) & 1 != 0;
        let total;
        if !single {
            let opcode = (i >> 12) & 0xF;
            let size = (i >> 10) & 3;
            let (regs, selem) = match opcode {
                0x7 => (1, 1),
                0xA => (2, 1),
                0x6 => (3, 1),
                0x2 => (4, 1),
                0x8 => (2, 2),
                0x4 => (3, 3),
                0x0 => (4, 4),
                _ => return Ok(false),
            };
            if size == 3 && !q && selem > 1 {
                return Ok(false);
            }
            let ebytes = 1u64 << size;
            let lanes = (if q { 16 } else { 8 }) / ebytes;
            for r in 0..regs as u64 {
                let reg = ((rt as u64 + r) % 32) as usize;
                let mut v = if load { 0u128 } else { self.q[reg] };
                for e in 0..lanes {
                    let slot = if selem == 1 { r * lanes + e } else { e * selem as u64 + r };
                    let addr = base.wrapping_add(slot * ebytes);
                    if load {
                        let x = Self::rd_mem(m, addr, ebytes as usize)?;
                        put(&mut v, ebytes as u32 * 8, e as u32, x);
                    } else {
                        Self::wr_mem(m, addr, ebytes as usize, lane(v, ebytes as u32 * 8, e as u32))?;
                    }
                }
                if load {
                    self.q[reg] = keep(v, q);
                }
            }
            total = regs as u64 * if q { 16 } else { 8 };
        } else {
            let opcode = (i >> 13) & 7;
            let (s, size, r) = ((i >> 12) & 1, (i >> 10) & 3, (i >> 21) & 1);
            if r != 0 {
                return Ok(false);
            }
            let reg = rt as usize;
            if opcode == 0b110 && load && s == 0 {
                // LD1R
                let ebytes = 1u64 << size;
                let x = Self::rd_mem(m, base, ebytes as usize)?;
                self.q[reg] = replicate_elem(x, ebytes as u32 * 8, width(q));
                total = ebytes;
            } else {
                let (ebytes, index) = match opcode {
                    0b000 => (1u64, (q as u32) << 3 | s << 2 | size),
                    0b010 if size & 1 == 0 => (2, (q as u32) << 2 | s << 1 | size >> 1),
                    0b100 if size == 0 => (4, (q as u32) << 1 | s),
                    0b100 if size == 1 && s == 0 => (8, q as u32),
                    _ => return Ok(false),
                };
                let bits = ebytes as u32 * 8;
                if load {
                    let x = Self::rd_mem(m, base, ebytes as usize)?;
                    let mut v = self.q[reg];
                    put(&mut v, bits, index, x);
                    self.q[reg] = v;
                } else {
                    Self::wr_mem(m, base, ebytes as usize, lane(self.q[reg], bits, index))?;
                }
                total = ebytes;
            }
        }
        if post {
            let step = if rm == 31 { total } else { self.r(rm, true) };
            self.set_sp(rn, base.wrapping_add(step), true);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Mem for Ram {
        fn read(&mut self, a: u64, size: usize) -> Option<u64> {
            let b = self.0.get(a as usize..a as usize + size)?;
            Some(b.iter().rev().fold(0u64, |v, &x| v << 8 | x as u64))
        }
        fn write(&mut self, a: u64, size: usize, v: u64) -> bool {
            match self.0.get_mut(a as usize..a as usize + size) {
                Some(b) => {
                    for (k, x) in b.iter_mut().enumerate() {
                        *x = (v >> (8 * k)) as u8;
                    }
                    true
                }
                None => false,
            }
        }
    }

    const SVC0: u32 = 0xD4000001;

    fn run(setup: impl FnOnce(&mut Cpu, &mut Ram), code: &[u32]) -> (Cpu, Ram) {
        let mut ram = Ram(vec![0; 0x10000]);
        for (k, w) in code.iter().chain(&[SVC0]).enumerate() {
            ram.write(k as u64 * 4, 4, *w as u64);
        }
        let mut cpu = Cpu::new();
        cpu.sp = 0x8000;
        setup(&mut cpu, &mut ram);
        loop {
            match cpu.step(&mut ram) {
                Ok(()) => {}
                Err(Exit::Svc(_)) => return (cpu, ram),
                Err(e) => panic!("stopped with {e:?}"),
            }
        }
    }

    fn lanes32(v: u128) -> [u32; 4] {
        std::array::from_fn(|k| (v >> (32 * k)) as u32)
    }

    #[test]
    fn integer_vector_arithmetic() {
        // movi v0.4s,#1 ; add v1.4s,v0.4s,v0.4s ; mul v2.4s,v1.4s,v1.4s ; sub v3.4s,v2.4s,v0.4s
        let (c, _) = run(|_, _| {}, &[0x4F000420, 0x4EA08401, 0x4EA19C22, 0x6EA08443]);
        assert_eq!(lanes32(c.q[0]), [1; 4]);
        assert_eq!(lanes32(c.q[1]), [2; 4]);
        assert_eq!(lanes32(c.q[2]), [4; 4]);
        assert_eq!(lanes32(c.q[3]), [3; 4]);
    }

    #[test]
    fn dup_addv_umov_and_ins() {
        // dup v0.4s,w1 ; addv s2,v0.4s ; umov w3,v0.s[1] ; mov v4.s[2],w1
        let (c, _) = run(|c, _| c.x[1] = 7, &[0x4E040C20, 0x4EB1B802, 0x0E0C3C03, 0x4E141C24]);
        assert_eq!(lanes32(c.q[0]), [7; 4]);
        assert_eq!(c.q[2], 28);
        assert_eq!(c.x[3], 7);
        assert_eq!(lanes32(c.q[4]), [0, 0, 7, 0]);
    }

    #[test]
    fn logic_popcount_and_float_vectors() {
        // eor v0.16b,v1.16b,v2.16b ; cnt v3.16b,v1.16b ; fadd v4.4s,v5.4s,v6.4s ; fmul v7.2d,v8.2d,v9.2d
        let (c, _) = run(
            |c, _| {
                c.q[1] = 0x0F0F_0F0F_FFFF_FFFF_0000_0000_F0F0_F0F0;
                c.q[2] = 0xFFFF_0000_FFFF_0000_FFFF_0000_FFFF_0000;
                for k in 0..4 {
                    c.q[5] |= ((1.5f32 + k as f32).to_bits() as u128) << (32 * k);
                    c.q[6] |= ((0.25f32).to_bits() as u128) << (32 * k);
                }
                c.q[8] = (2.0f64.to_bits() as u128) | (3.0f64.to_bits() as u128) << 64;
                c.q[9] = (4.0f64.to_bits() as u128) | (0.5f64.to_bits() as u128) << 64;
            },
            &[0x6E221C20, 0x4E205823, 0x4E26D4A4, 0x6E69DD07],
        );
        assert_eq!(c.q[0], 0x0F0F_0F0F_FFFF_FFFF_0000_0000_F0F0_F0F0 ^ 0xFFFF_0000_FFFF_0000_FFFF_0000_FFFF_0000);
        assert_eq!(c.q[3] & 0xFF, 4, "popcount of 0xF0");
        assert_eq!(f32::from_bits(c.q[4] as u32), 1.75);
        assert_eq!(f32::from_bits((c.q[4] >> 96) as u32), 4.75);
        assert_eq!(f64::from_bits(c.q[7] as u64), 8.0);
        assert_eq!(f64::from_bits((c.q[7] >> 64) as u64), 1.5);
    }

    #[test]
    fn shifts_widening_and_shuffles() {
        // shl v0.4s,v1.4s,#3 ; ushll v2.8h,v3.8b,#0 ; zip1 v4.4s,v5.4s,v6.4s ; tbl v7.16b,{v8.16b},v9.16b
        let (c, _) = run(
            |c, _| {
                c.q[1] = 5 | 6 << 32 | 7 << 64 | 8 << 96;
                c.q[3] = 0x0807_0605_0403_0201;
                c.q[5] = 1 | 2 << 32 | 3 << 64 | 4 << 96;
                c.q[6] = 10 | 20 << 32 | 30 << 64 | 40 << 96;
                c.q[8] = 0x0F0E_0D0C_0B0A_0908_0706_0504_0302_0100;
                c.q[9] = 0xFF00_0F01_0E02_0D03_0C04_0B05_0A06_0907;
            },
            &[0x4F235420, 0x2F08A462, 0x4E8638A4, 0x4E090107],
        );
        assert_eq!(lanes32(c.q[0]), [40, 48, 56, 64]);
        assert_eq!((c.q[2] as u16, (c.q[2] >> 112) as u16), (1, 8), "bytes widen to halfwords");
        assert_eq!(lanes32(c.q[4]), [1, 10, 2, 20]);
        assert_eq!(c.q[7] & 0xFF, 0x07, "index 7 picks byte 7");
        assert_eq!((c.q[7] >> 120) as u8, 0, "an index past the table gives 0");
    }

    #[test]
    fn vector_loads_stores_and_by_element() {
        // ld1 {v0.16b},[x1] ; st1 {v0.16b},[x2],#16 ; ld1r {v3.4s},[x1] ; fmla v4.4s,v5.4s,v6.s[1]
        let (c, ram) = run(
            |c, ram| {
                c.x[1] = 0x4000;
                c.x[2] = 0x5000;
                for k in 0..16u8 {
                    ram.0[0x4000 + k as usize] = k + 1;
                }
                for k in 0..4 {
                    c.q[5] |= (2.0f32.to_bits() as u128) << (32 * k);
                }
                c.q[6] = (1.0f32.to_bits() as u128) | (3.0f32.to_bits() as u128) << 32;
                c.q[4] = 0;
            },
            &[0x4C407020, 0x4C9F7040, 0x4D40C823, 0x4FA610A4],
        );
        assert_eq!(&ram.0[0x5000..0x5010], &(1..=16u8).collect::<Vec<_>>()[..]);
        assert_eq!(c.x[2], 0x5010, "post-index advanced by 16");
        assert_eq!(lanes32(c.q[3]), [0x0403_0201; 4], "LD1R fills every lane");
        assert_eq!(f32::from_bits(c.q[4] as u32), 6.0, "2.0 * element 1 (3.0)");
    }

    #[test]
    fn shll_and_scalar_cvtf() {
        // shll v1.4s, v0.4h, #16 ; shll2 v2.4s, v0.8h, #16 ; ucvtf d3, d3 ; fcvtzu d4, d3
        let (c, _) = run(
            |c, _| {
                c.q[0] = (0x0001u128)
                    | (0x0002u128 << 16)
                    | (0x0003u128 << 32)
                    | (0x0004u128 << 48)
                    | (0x0005u128 << 64)
                    | (0x0006u128 << 80)
                    | (0x0007u128 << 96)
                    | (0x0008u128 << 112);
                c.q[3] = 24_000_000u128;
            },
            &[0x2E61_3801, 0x6E61_3802, 0x7E61_D863, 0x7EE1_B864],
        );
        assert_eq!(lanes32(c.q[1]), [0x0001_0000, 0x0002_0000, 0x0003_0000, 0x0004_0000]);
        assert_eq!(lanes32(c.q[2]), [0x0005_0000, 0x0006_0000, 0x0007_0000, 0x0008_0000]);
        assert_eq!(f64::from_bits(c.q[3] as u64), 24_000_000.0);
        assert_eq!(c.q[4] as u64, 24_000_000);
    }

    #[test]
    fn crypto_sha1_and_sha256() {
        // Test SHA1H, SHA1SU0, SHA1SU1, SHA256SU0, SHA256SU1, SHA256H.
        let (c, _) = run(
            |c, _| {
                c.q[0] = 0x0000_0001u128 | (0x0000_0002u128 << 32) | (0x0000_0003u128 << 64) | (0x0000_0004u128 << 96);
                c.q[1] = 0x0000_0010u128 | (0x0000_0020u128 << 32) | (0x0000_0030u128 << 64) | (0x0000_0040u128 << 96);
                c.q[2] = 0x0000_0100u128 | (0x0000_0200u128 << 32) | (0x0000_0300u128 << 64) | (0x0000_0400u128 << 96);
                c.q[3] = 0x6745_2301u128;
            },
            &[
                0x5E28_0864, // sha1h s4, s3
                0x5E02_3020, // sha1su0 v0.4s, v1.4s, v2.4s
                0x5E28_1820, // sha1su1 v0.4s, v1.4s
                0x5E28_2825, // sha256su0 v5.4s, v1.4s
                0x5E02_6025, // sha256su1 v5.4s, v1.4s, v2.4s
            ],
        );
        assert_eq!(c.q[4], 0x6745_2301u32.rotate_left(30) as u128, "SHA1H rotates left by 30 and clears upper bits");
        assert_ne!(c.q[0], 0, "SHA1SU0+SHA1SU1 updates schedule");
        assert_ne!(c.q[5], 0, "SHA256SU0+SHA256SU1 updates schedule");

        // Test AESE -> AESMC -> AESIMC -> AESD round-trip restores plaintext ^ key.
        let pt: u128 = 0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF;
        let key: u128 = 0x0F0E_0D0C_0B0A_0908_0706_0504_0302_0100;
        let (c2, _) = run(
            |c, _| {
                c.q[0] = pt;
                c.q[1] = key;
                c.q[2] = 0;
            },
            &[
                0x4E28_4820, // aese v0.16b, v1.16b
                0x4E28_6800, // aesmc v0.16b, v0.16b
                0x4E28_7800, // aesimc v0.16b, v0.16b
                0x4E28_5840, // aesd v0.16b, v2.16b (zero key)
            ],
        );
        assert_eq!(c2.q[0], pt ^ key, "AESE+AESMC+AESIMC+AESD round-trips cleanly");

        // Test PMULL / PMULL2 (64x64 -> 128 carry-less multiply): (x^2 + 1) * (x + 1) = x^3 + x^2 + x + 1 (0xF)
        let (c3, _) = run(
            |c, _| {
                c.q[0] = 0b101u128 | (0b1010u128 << 64);
                c.q[2] = 0b011u128 | (0b0101u128 << 64);
            },
            &[
                0x0EE2_E001, // pmull  v1.1q, v0.1d, v2.1d -> 0b101 * 0b011 = 0b1111
                0x4EE2_E003, // pmull2 v3.1q, v0.2d, v2.2d -> 0b1010 * 0b0101 = 0b100010
            ],
        );
        assert_eq!(c3.q[1], 0b1111);
        assert_eq!(c3.q[3], 0b10_0010);

        // Test RBIT V1.16B, V0.16B reverses all 16 bytes (including odd bytes).
        let (c4, _) = run(
            |c, _| {
                c.q[0] = u128::from_le_bytes([0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x03, 0x05, 0x09, 0x11, 0x21, 0x41, 0x81, 0xF0]);
            },
            &[0x6E60_5801], // rbit v1.16b, v0.16b
        );
        assert_eq!(
            c4.q[1].to_le_bytes(),
            [0x80, 0x40, 0x20, 0x10, 0x08, 0x04, 0x02, 0x01, 0xC0, 0xA0, 0x90, 0x88, 0x84, 0x82, 0x81, 0x0F]
        );

        // Test SHA256H + SHA256H2 with distinct Qd(a..d) and Qn(e..h) to verify operand order.
        let h0: u128 = 0x6a09e667u128 | (0xbb67ae85u128 << 32) | (0x3c6ef372u128 << 64) | (0xa54ff53au128 << 96);
        let h1: u128 = 0x510e527fu128 | (0x9b05688cu128 << 32) | (0x1f83d9abu128 << 64) | (0x5be0cd19u128 << 96);
        let wk: u128 = 0x428a2f98u128 | (0x71374491u128 << 32) | (0xb5c0fbcfu128 << 64) | (0xe9b5dba5u128 << 96);
        let (c5, _) = run(
            |c, _| {
                c.q[0] = h0;
                c.q[1] = h1;
                c.q[2] = wk;
                c.q[3] = h0; // copy for sha256h2
            },
            &[
                0x5E02_4020, // sha256h  q0, q1, v2.4s -> new a..d
                0x5E02_5061, // sha256h2 q1, q3, v2.4s -> new e..h (using original a..d in q3)
            ],
        );
        assert_ne!(c5.q[0], h0);
        assert_ne!(c5.q[1], h1);
        assert_ne!(c5.q[0], c5.q[1], "SHA256H and SHA256H2 produce distinct halves");
    }
}
