//! The R5900's multimedia instructions ("MMI"): the second multiply unit and parallel
//! operations on the 128-bit registers (four 32-bit, eight 16-bit or sixteen 8-bit lanes).

use crate::*;

fn pack(lo: u64, hi: u64) -> u128 {
    lo as u128 | (hi as u128) << 64
}

fn w(v: u128) -> [u32; 4] {
    [v as u32, (v >> 32) as u32, (v >> 64) as u32, (v >> 96) as u32]
}

fn unw(l: [u32; 4]) -> u128 {
    l[0] as u128 | (l[1] as u128) << 32 | (l[2] as u128) << 64 | (l[3] as u128) << 96
}

fn h(v: u128) -> [u16; 8] {
    let mut o = [0u16; 8];
    for (i, x) in o.iter_mut().enumerate() {
        *x = (v >> (16 * i)) as u16;
    }
    o
}

fn unh(l: [u16; 8]) -> u128 {
    l.iter().enumerate().fold(0u128, |a, (i, &x)| a | (x as u128) << (16 * i))
}

fn bytes(v: u128) -> [u8; 16] {
    v.to_le_bytes()
}

fn unb(l: [u8; 16]) -> u128 {
    u128::from_le_bytes(l)
}

impl Cpu {
    fn get128(&self, r: usize) -> u128 {
        pack(self.lo[r], self.hi_half[r])
    }

    fn set128(&mut self, r: usize, v: u128) {
        if r != 0 {
            self.lo[r] = v as u64;
            self.hi_half[r] = (v >> 64) as u64;
        }
    }

    pub(crate) fn mmi<B: Bus>(&mut self, _bus: &mut B, op: u32, rs: usize, rt: usize, rd: usize, sa: u32) {
        let (a, b) = (self.get128(rs), self.get128(rt));
        let s32 = |v: u32| v as i32 as i64 as u64;
        match op & 63 {
            0 | 1 | 32 | 33 => {
                // madd / maddu (unit 0) and madd1 / maddu1 (unit 1)
                let unit1 = op & 63 >= 32;
                let signed = op & 1 == 0;
                let (x, y) = (self.lo[rs] as u32, self.lo[rt] as u32);
                let p = if signed { (x as i32 as i64 * y as i32 as i64) as u64 } else { x as u64 * y as u64 };
                let (hi, lo) = if unit1 { (self.hi1, self.lo1) } else { (self.hi, self.lo_reg) };
                let acc = (hi as u32 as u64) << 32 | lo as u32 as u64;
                let r = acc.wrapping_add(p);
                let (nlo, nhi) = (s32(r as u32), s32((r >> 32) as u32));
                if unit1 {
                    self.lo1 = nlo;
                    self.hi1 = nhi;
                } else {
                    self.lo_reg = nlo;
                    self.hi = nhi;
                }
                self.set_reg(rd, nlo);
            }
            4 => {
                // plzcw: leading sign bits of the two low words
                let lz = |v: u32| (if (v as i32) < 0 { (!v).leading_zeros() } else { v.leading_zeros() }) as u64 - 1;
                let (l, hh) = (lz(self.lo[rs] as u32), lz((self.lo[rs] >> 32) as u32));
                self.set_reg(rd, l | hh << 32);
            }
            16 => self.set_reg(rd, self.hi1),
            17 => self.hi1 = self.lo[rs],
            18 => self.set_reg(rd, self.lo1),
            19 => self.lo1 = self.lo[rs],
            24 | 25 => {
                let p = if op & 63 == 24 { ((self.lo[rs] as u32 as i32 as i64) * (self.lo[rt] as u32 as i32 as i64)) as u64 } else { (self.lo[rs] as u32 as u64) * (self.lo[rt] as u32 as u64) };
                self.lo1 = s32(p as u32);
                self.hi1 = s32((p >> 32) as u32);
                self.set_reg(rd, self.lo1);
            }
            26 | 27 => {
                let (n, d) = (self.lo[rs] as u32, self.lo[rt] as u32);
                if op & 63 == 26 {
                    let (n, d) = (n as i32, d as i32);
                    if d == 0 {
                        self.lo1 = if n < 0 { 1 } else { u64::MAX };
                        self.hi1 = n as i64 as u64;
                    } else if n == i32::MIN && d == -1 {
                        self.lo1 = s32(i32::MIN as u32);
                        self.hi1 = 0;
                    } else {
                        self.lo1 = (n / d) as i64 as u64;
                        self.hi1 = (n % d) as i64 as u64;
                    }
                } else if d == 0 {
                    self.lo1 = u64::MAX;
                    self.hi1 = s32(n);
                } else {
                    self.lo1 = s32(n / d);
                    self.hi1 = s32(n % d);
                }
            }
            48 => {
                // pmfhl: only the .lw variant (sa = 0)
                if sa == 0 {
                    let v = unw([self.lo_reg as u32, self.hi as u32, self.lo1 as u32, self.hi1 as u32]);
                    self.set128(rd, v);
                } else {
                    self.reserved();
                }
            }
            52 => self.set128(rd, unh(h(b).map(|x| x << (sa & 15)))),
            54 => self.set128(rd, unh(h(b).map(|x| x >> (sa & 15)))),
            55 => self.set128(rd, unh(h(b).map(|x| ((x as i16) >> (sa & 15)) as u16))),
            60 => self.set128(rd, unw(w(b).map(|x| x << sa))),
            62 => self.set128(rd, unw(w(b).map(|x| x >> sa))),
            63 => self.set128(rd, unw(w(b).map(|x| ((x as i32) >> sa) as u32))),
            8 => self.mmi0(rd, a, b, sa),
            40 => self.mmi1(rd, a, b, sa),
            9 => self.mmi2(rd, rs, a, b, sa),
            41 => self.mmi3(rd, rs, a, b, sa),
            _ => self.reserved(),
        }
    }

    fn mmi0(&mut self, rd: usize, a: u128, b: u128, sel: u32) {
        let (aw, bw) = (w(a), w(b));
        let (ah, bh) = (h(a), h(b));
        let (ab, bb) = (bytes(a), bytes(b));
        let zipw = |x: [u32; 4], y: [u32; 4], hi: bool| -> u128 {
            let o = if hi { 2 } else { 0 };
            unw([y[o], x[o], y[o + 1], x[o + 1]])
        };
        let v = match sel {
            0 => unw(std::array::from_fn(|i| aw[i].wrapping_add(bw[i]))),
            1 => unw(std::array::from_fn(|i| aw[i].wrapping_sub(bw[i]))),
            2 => unw(std::array::from_fn(|i| if (aw[i] as i32) > (bw[i] as i32) { u32::MAX } else { 0 })),
            3 => unw(std::array::from_fn(|i| (aw[i] as i32).max(bw[i] as i32) as u32)),
            4 => unh(std::array::from_fn(|i| ah[i].wrapping_add(bh[i]))),
            5 => unh(std::array::from_fn(|i| ah[i].wrapping_sub(bh[i]))),
            6 => unh(std::array::from_fn(|i| if (ah[i] as i16) > (bh[i] as i16) { u16::MAX } else { 0 })),
            7 => unh(std::array::from_fn(|i| (ah[i] as i16).max(bh[i] as i16) as u16)),
            8 => unb(std::array::from_fn(|i| ab[i].wrapping_add(bb[i]))),
            9 => unb(std::array::from_fn(|i| ab[i].wrapping_sub(bb[i]))),
            10 => unb(std::array::from_fn(|i| if (ab[i] as i8) > (bb[i] as i8) { 0xFF } else { 0 })),
            16 => unw(std::array::from_fn(|i| (aw[i] as i32).saturating_add(bw[i] as i32) as u32)),
            17 => unw(std::array::from_fn(|i| (aw[i] as i32).saturating_sub(bw[i] as i32) as u32)),
            18 => zipw(aw, bw, false),
            19 => unw([bw[1], bw[3], aw[1], aw[3]]),
            20 => unh(std::array::from_fn(|i| (ah[i] as i16).saturating_add(bh[i] as i16) as u16)),
            21 => unh(std::array::from_fn(|i| (ah[i] as i16).saturating_sub(bh[i] as i16) as u16)),
            22 => unh([bh[0], ah[0], bh[1], ah[1], bh[2], ah[2], bh[3], ah[3]]),
            23 => unh([bh[0], bh[2], bh[4], bh[6], ah[0], ah[2], ah[4], ah[6]]),
            24 => unb(std::array::from_fn(|i| (ab[i] as i8).saturating_add(bb[i] as i8) as u8)),
            25 => unb(std::array::from_fn(|i| (ab[i] as i8).saturating_sub(bb[i] as i8) as u8)),
            26 => unb(std::array::from_fn(|i| if i % 2 == 0 { bb[i / 2] } else { ab[i / 2] })),
            27 => unb(std::array::from_fn(|i| if i < 8 { bb[i * 2] } else { ab[(i - 8) * 2] })),
            _ => {
                self.reserved();
                return;
            }
        };
        self.set128(rd, v);
    }

    fn mmi1(&mut self, rd: usize, a: u128, b: u128, sel: u32) {
        let (aw, bw) = (w(a), w(b));
        let (ah, bh) = (h(a), h(b));
        let (ab, bb) = (bytes(a), bytes(b));
        let v = match sel {
            1 => unw(std::array::from_fn(|i| (bw[i] as i32).saturating_abs() as u32)),
            2 => unw(std::array::from_fn(|i| if aw[i] == bw[i] { u32::MAX } else { 0 })),
            3 => unw(std::array::from_fn(|i| (aw[i] as i32).min(bw[i] as i32) as u32)),
            5 => unh(std::array::from_fn(|i| (bh[i] as i16).saturating_abs() as u16)),
            6 => unh(std::array::from_fn(|i| if ah[i] == bh[i] { u16::MAX } else { 0 })),
            7 => unh(std::array::from_fn(|i| (ah[i] as i16).min(bh[i] as i16) as u16)),
            10 => unb(std::array::from_fn(|i| if ab[i] == bb[i] { 0xFF } else { 0 })),
            16 => unw(std::array::from_fn(|i| aw[i].saturating_add(bw[i]))),
            17 => unw(std::array::from_fn(|i| aw[i].saturating_sub(bw[i]))),
            18 => unw([bw[2], aw[2], bw[3], aw[3]]),
            20 => unh(std::array::from_fn(|i| ah[i].saturating_add(bh[i]))),
            21 => unh(std::array::from_fn(|i| ah[i].saturating_sub(bh[i]))),
            22 => unh([bh[4], ah[4], bh[5], ah[5], bh[6], ah[6], bh[7], ah[7]]),
            24 => unb(std::array::from_fn(|i| ab[i].saturating_add(bb[i]))),
            25 => unb(std::array::from_fn(|i| ab[i].saturating_sub(bb[i]))),
            26 => unb(std::array::from_fn(|i| if i % 2 == 0 { bb[8 + i / 2] } else { ab[8 + i / 2] })),
            27 => {
                // qfsrv: funnel shift of rs:rt right by SA bits
                let sh = self.sa & 0x7F;
                let v = if sh == 0 { b } else if sh >= 128 { a } else { (b >> sh) | (a << (128 - sh)) };
                self.set128(rd, v);
                return;
            }
            _ => {
                self.reserved();
                return;
            }
        };
        self.set128(rd, v);
    }

    fn mmi2(&mut self, rd: usize, rs: usize, a: u128, b: u128, sel: u32) {
        let (aw, bw) = (w(a), w(b));
        let v = match sel {
            8 => pack(self.hi, self.hi1),
            9 => pack(self.lo_reg, self.lo1),
            10 => {
                let (ah, bh) = (h(a), h(b));
                unh([bh[0], ah[4], bh[1], ah[5], bh[2], ah[6], bh[3], ah[7]])
            }
            14 => pack(b as u64, a as u64),
            18 => a & b,
            19 => a ^ b,
            26 => {
                let bh = h(b);
                unh([bh[0], bh[2], bh[1], bh[3], bh[4], bh[6], bh[5], bh[7]])
            }
            27 => {
                let bh = h(b);
                unh([bh[3], bh[2], bh[1], bh[0], bh[7], bh[6], bh[5], bh[4]])
            }
            30 => unw([bw[0], bw[2], bw[1], bw[3]]),
            31 => unw([bw[1], bw[2], aw[0], bw[3]]),
            2 => unw([bw[0] << (aw[0] & 31), bw[1] << (aw[2] & 31), bw[2] << (aw[0] & 31), bw[3] << (aw[2] & 31)]),
            3 => unw([bw[0] >> (aw[0] & 31), bw[1] >> (aw[2] & 31), bw[2] >> (aw[0] & 31), bw[3] >> (aw[2] & 31)]),
            12 => {
                // pmultw: signed 32x32 -> 64 on words 0 and 2
                let p0 = (aw[0] as i32 as i64) * (bw[0] as i32 as i64);
                let p1 = (aw[2] as i32 as i64) * (bw[2] as i32 as i64);
                self.lo_reg = p0 as u32 as i32 as i64 as u64;
                self.hi = (p0 >> 32) as u32 as i32 as i64 as u64;
                self.lo1 = p1 as u32 as i32 as i64 as u64;
                self.hi1 = (p1 >> 32) as u32 as i32 as i64 as u64;
                pack(p0 as u64, p1 as u64)
            }
            _ => {
                self.reserved();
                return;
            }
        };
        let _ = rs;
        self.set128(rd, v);
    }

    fn mmi3(&mut self, rd: usize, rs: usize, a: u128, b: u128, sel: u32) {
        let (aw, bw) = (w(a), w(b));
        let v = match sel {
            8 => {
                self.hi = self.lo[rs];
                self.hi1 = self.hi_half[rs];
                return;
            }
            9 => {
                self.lo_reg = self.lo[rs];
                self.lo1 = self.hi_half[rs];
                return;
            }
            10 => {
                let (ah, bh) = (h(a), h(b));
                unh([bh[0], ah[1], bh[2], ah[3], bh[4], ah[5], bh[6], ah[7]])
            }
            14 => pack((a >> 64) as u64, (b >> 64) as u64), // pcpyud
            18 => a | b,
            19 => !(a | b),
            26 => {
                // pexch: swap the middle halfwords of each 64-bit half
                let bh = h(b);
                unh([bh[0], bh[2], bh[1], bh[3], bh[4], bh[6], bh[5], bh[7]])
            }
            27 => {
                let bh = h(b);
                unh([bh[0], bh[0], bh[0], bh[0], bh[4], bh[4], bh[4], bh[4]])
            }
            30 => unw([bw[0], bw[2], bw[1], bw[3]]),
            3 => unw([
                ((bw[0] as i32) >> (aw[0] & 31)) as u32,
                ((bw[1] as i32) >> (aw[2] & 31)) as u32,
                ((bw[2] as i32) >> (aw[0] & 31)) as u32,
                ((bw[3] as i32) >> (aw[2] & 31)) as u32,
            ]),
            _ => {
                self.reserved();
                return;
            }
        };
        self.set128(rd, v);
    }
}
