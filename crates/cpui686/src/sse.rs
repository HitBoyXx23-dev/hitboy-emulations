//! MMX, SSE and SSE2: 8 × 64-bit MMX registers, 16 × 128-bit XMM registers, packed and
//! scalar single/double-precision math, 64/128-bit integer SIMD, conversions, MXCSR and
//! FXSAVE/FXRSTOR. The Pentium III model has no SSE2: its 66h/F2h forms raise #UD.

use super::*;

#[derive(Clone, Debug)]
pub struct Simd {
    pub xmm: [u128; 16],
    pub ymm_hi: [u128; 16],
    pub mm: [u64; 8],
    pub mxcsr: u32,
}

impl Default for Simd {
    fn default() -> Self {
        Simd { xmm: [0; 16], ymm_hi: [0; 16], mm: [0; 8], mxcsr: 0x1F80 }
    }
}

/// Mandatory prefix class of an SSE instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pfx {
    None,
    P66,
    F3,
    F2,
}

fn integer_to_float(value: i32, rounding: u32) -> (u32, bool) {
    let negative = value < 0;
    let magnitude = (value as i64).unsigned_abs() as u32;
    if magnitude == 0 { return (0, false); }
    let mut exponent = 31 - magnitude.leading_zeros();
    let mut significand;
    let mut inexact = false;
    if exponent <= 23 {
        significand = magnitude << (23 - exponent);
    } else {
        let shift = exponent - 23;
        significand = magnitude >> shift;
        let remainder = magnitude & ((1 << shift) - 1);
        inexact = remainder != 0;
        let increment = match rounding {
            0 => remainder > 1 << (shift - 1) || remainder == 1 << (shift - 1) && significand & 1 != 0,
            1 => negative && inexact,
            2 => !negative && inexact,
            _ => false,
        };
        if increment { significand += 1; }
        if significand == 1 << 24 { significand >>= 1; exponent += 1; }
    }
    (((negative as u32) << 31) | ((exponent + 127) << 23) | (significand & 0x7FFFFF), inexact)
}

fn f32s(v: u128) -> [f32; 4] {
    [0, 1, 2, 3].map(|i| f32::from_bits((v >> (i * 32)) as u32))
}
fn from_f32s(a: [f32; 4]) -> u128 {
    a.iter().enumerate().fold(0, |v, (i, x)| v | (x.to_bits() as u128) << (i * 32))
}
fn f64s(v: u128) -> [f64; 2] {
    [f64::from_bits(v as u64), f64::from_bits((v >> 64) as u64)]
}
fn from_f64s(a: [f64; 2]) -> u128 {
    a[0].to_bits() as u128 | (a[1].to_bits() as u128) << 64
}

/// Splits the low `n` bytes of `v` into lanes of `w` bytes.
fn lanes(v: u128, w: u32, n: u32) -> Vec<u64> {
    let bits = w * 8;
    let m: u128 = if bits == 64 { u64::MAX as u128 } else { (1u128 << bits) - 1 };
    (0..n / w).map(|i| ((v >> (i * bits)) & m) as u64).collect()
}
fn join(l: &[u64], w: u32) -> u128 {
    let bits = w * 8;
    let m: u64 = if bits == 64 { u64::MAX } else { (1 << bits) - 1 };
    l.iter().enumerate().fold(0, |acc, (i, &x)| acc | ((x & m) as u128) << (i as u32 * bits))
}
fn sext(v: u64, w: u32) -> i64 {
    let sh = 64 - w * 8;
    ((v << sh) as i64) >> sh
}
fn sat_s(v: i64, w: u32) -> u64 {
    let lim = 1i64 << (w * 8 - 1);
    v.clamp(-lim, lim - 1) as u64
}
fn sat_u(v: i64, w: u32) -> u64 {
    v.clamp(0, (1i64 << (w * 8)) - 1) as u64
}

fn f2i32(v: f64) -> u32 {
    if v.is_nan() || v >= 2147483648.0 || v < -2147483648.0 {
        0x8000_0000
    } else {
        v as i32 as u32
    }
}
fn f2i64(v: f64) -> u64 {
    if v.is_nan() || v >= 9.223372036854775808e18 || v < -9.223372036854775808e18 {
        1 << 63
    } else {
        v as i64 as u64
    }
}

#[derive(Clone, Copy)]
enum ShuffleKind {
    Blend,
    Permute,
    Shuffle,
}

impl Cpu {
    pub(crate) fn exec_vex<B: Bus>(&mut self, bus: &mut B, prefix: u8) -> R<()> {
        if self.rex != 0 || self.p66 || self.rep != 0 {
            return Err(UD);
        }
        let first = self.fetch8(bus)? as u8;
        let map = if prefix == 0xC4 { first & 0x1F } else { 1 };
        let control = if prefix == 0xC4 {
            self.rex = 0x40 | ((!first >> 5) & 7);
            self.fetch8(bus)? as u8
        } else {
            self.rex = 0x40 | ((!first >> 5) & 4);
            first
        };
        let wide = control & 4 != 0;
        let op = self.fetch8(bus)? as u8;
        self.cur_op = (op, map as u32);
        let pp = control & 3;
        if map == 2 && op == 0xF3 && pp == 0 && !wide {
            if control & 0x80 != 0 { self.rex |= 8; }
            let size = if self.rex & 8 != 0 { 8 } else { 4 };
            let (group, operand) = self.modrm(bus)?;
            let source = self.get(bus, operand, size)?;
            let value = match group {
                1 => source & source.wrapping_sub(1),
                2 => source ^ source.wrapping_sub(1),
                3 => source & source.wrapping_neg(),
                _ => return Err(UD),
            } & mask(size);
            self.set_reg((!control >> 3) & 15, size, value);
            self.eflags &= !OF;
            self.setf(CF, if group == 3 { source != 0 } else { source == 0 });
            self.setf(ZF, value == 0);
            self.setf(SF, value & (1u64 << (size * 8 - 1)) != 0);
            return Ok(());
        }
        if map == 2 && op == 0xF2 && pp == 0 && !wide {
            if control & 0x80 != 0 { self.rex |= 8; }
            let size = if self.rex & 8 != 0 { 8 } else { 4 };
            let (destination, operand) = self.modrm(bus)?;
            let left = self.reg((!control >> 3) & 15, size);
            let right = self.get(bus, operand, size)?;
            let value = (!left & right) & mask(size);
            self.set_reg(destination, size, value);
            self.eflags &= !(CF | OF);
            self.setf(ZF, value == 0);
            self.setf(SF, value & (1u64 << (size * 8 - 1)) != 0);
            return Ok(());
        }
        if map == 2 && op == 0xF7 && pp == 0 && !wide {
            if control & 0x80 != 0 { self.rex |= 8; }
            let size = if self.rex & 8 != 0 { 8 } else { 4 };
            let (reg, rm) = self.modrm(bus)?;
            let source = self.get(bus, rm, size)?;
            let control_value = self.r[((!control >> 3) & 15) as usize];
            let start = (control_value & 0xFF) as u32;
            let length = ((control_value >> 8) & 0xFF) as u32;
            let width = size * 8;
            let result = if start >= width || length == 0 {
                0
            } else {
                let length = length.min(width - start);
                let bits = if length == 64 { u64::MAX } else { (1u64 << length) - 1 };
                (source >> start) & bits
            };
            self.set_reg(reg, size, result);
            self.eflags &= !(CF | OF);
            self.setf(ZF, result == 0);
            return Ok(());
        }
        if !matches!(map, 1 | 2 | 3) || self.cr0 & 4 != 0 {
            return Err(UD);
        }
        if self.cr0 & 8 != 0 {
            return Err(Ex(7, None));
        }
        if prefix == 0xC4 && control & 0x80 != 0 { self.rex |= 8; }
        if map == 1 && op == 0x77 && pp == 0 && control & 0x78 == 0x78 {
            self.simd.ymm_hi.fill(0);
            if wide { self.simd.xmm.fill(0); }
            return Ok(());
        }
        if (map == 2 && op == 0x2A && pp == 1) || (map == 1 && ((op == 0xE7 && pp == 1) || (op == 0x2B && pp <= 1))) {
            if control & 0x78 != 0x78 { return Err(UD); }
            let (reg, operand) = self.modrm(bus)?;
            let Op::Mem(segment, address) = operand else { return Err(UD); };
            let alignment = if wide { 32 } else { 16 };
            if self.base[segment].wrapping_add(address) & (alignment - 1) != 0 { return Err(gp(0)); }
            if map == 2 {
                let low = self.xmm_rm(bus, operand, 16)?;
                let high = if wide { self.xmm_rm(bus, Op::Mem(segment, address.wrapping_add(16)), 16)? } else { 0 };
                self.simd.xmm[reg as usize] = low;
                self.simd.ymm_hi[reg as usize] = high;
            } else {
                self.store_mem(bus, segment, address, self.simd.xmm[reg as usize], 16)?;
                if wide { self.store_mem(bus, segment, address.wrapping_add(16), self.simd.ymm_hi[reg as usize], 16)?; }
            }
            return Ok(());
        }
        if map == 1 && op == 0x5B && matches!(pp, 1 | 2) {
            if control & 0x78 != 0x78 { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            let low = self.xmm_rm(bus, operand, 16)?;
            let high = if wide {
                match operand {
                    Op::Reg(register) => self.simd.ymm_hi[register as usize],
                    Op::Mem(segment, address) => self.xmm_rm(bus, Op::Mem(segment, address.wrapping_add(16)), 16)?,
                }
            } else { 0 };
            let mut flags = 0u32;
            let mut converted = [0u128; 2];
            for (index, source) in [low, high].into_iter().enumerate().take(if wide { 2 } else { 1 }) {
                for lane in 0..4 {
                    let bits = (source >> (lane * 32)) as u32;
                    let value = if self.simd.mxcsr & 0x40 != 0 && bits & 0x7F800000 == 0 {
                        0.0
                    } else { f32::from_bits(bits) as f64 };
                    let rounded = if pp == 2 { value.trunc() } else { self.round_mx(value) };
                    let result = if !rounded.is_finite() || rounded < i32::MIN as f64 || rounded > i32::MAX as f64 {
                        flags |= 1;
                        i32::MIN
                    } else {
                        if rounded != value { flags |= 0x20; }
                        rounded as i32
                    };
                    converted[index] |= (result as u32 as u128) << (lane * 32);
                }
            }
            self.simd.mxcsr |= flags;
            if flags & !(self.simd.mxcsr >> 7) & 0x3F != 0 {
                return Err(if self.cr4 & (1 << 10) != 0 { Ex(19, None) } else { UD });
            }
            self.simd.xmm[destination as usize] = converted[0];
            self.simd.ymm_hi[destination as usize] = converted[1];
            return Ok(());
        }
        if map == 1 && op == 0x5B && pp == 0 {
            if wide || control & 0x78 != 0x78 { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            let source = self.xmm_rm(bus, operand, 16)?;
            let rounding = (self.simd.mxcsr >> 13) & 3;
            let converted = lanes(source, 4, 16).into_iter().map(|value| integer_to_float(value as u32 as i32, rounding)).collect::<Vec<_>>();
            if converted.iter().any(|(_, inexact)| *inexact) {
                self.simd.mxcsr |= 0x20;
                if self.simd.mxcsr & 0x1000 == 0 { return Err(if self.cr4 & (1 << 10) != 0 { Ex(19, None) } else { UD }); }
            }
            self.simd.xmm[destination as usize] = join(&converted.into_iter().map(|(bits, _)| bits as u64).collect::<Vec<_>>(), 4);
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if map == 2 && matches!(op, 0x18..=0x1A) && pp == 1 {
            if (!wide && op != 0x18) || control & 0x78 != 0x78 { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            if !matches!(operand, Op::Mem(_, _)) { return Err(UD); }
            let value = match op {
                0x18 => {
                    let scalar = self.xmm_rm(bus, operand, 4)?;
                    scalar | (scalar << 32) | (scalar << 64) | (scalar << 96)
                }
                0x19 => {
                    let scalar = self.xmm_rm(bus, operand, 8)?;
                    scalar | (scalar << 64)
                }
                _ => self.xmm_rm(bus, operand, 16)?,
            };
            self.simd.xmm[destination as usize] = value;
            self.simd.ymm_hi[destination as usize] = if wide { value } else { 0 };
            return Ok(());
        }
        if map == 2 && op == 0x17 && pp == 1 {
            if wide || control & 0x78 != 0x78 { return Err(UD); }
            let (reg, operand) = self.modrm(bus)?;
            let left = self.simd.xmm[reg as usize];
            let right = self.xmm_rm(bus, operand, 16)?;
            self.eflags &= !(OF | SF | AF | PF);
            self.setf(ZF, left & right == 0);
            self.setf(CF, !left & right == 0);
            return Ok(());
        }
        if map == 2 && pp == 1 && ((0x20..=0x25).contains(&op) || (0x30..=0x35).contains(&op)) {
            if wide || control & 0x78 != 0x78 { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            let (source_width, destination_width) = match op & 15 {
                0 => (1, 2),
                1 => (1, 4),
                2 => (1, 8),
                3 => (2, 4),
                4 => (2, 8),
                _ => (4, 8),
            };
            let source_size = 16 / destination_width * source_width;
            let source = self.xmm_rm(bus, operand, source_size)?;
            let values = lanes(source, source_width, source_size).into_iter().map(|value| if op < 0x30 { sext(value, source_width) as u64 } else { value }).collect::<Vec<_>>();
            self.simd.xmm[destination as usize] = join(&values, destination_width);
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if map == 2 && pp == 1 && op == 0x40 {
            if wide { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            let left = lanes(self.simd.xmm[((!control >> 3) & 15) as usize], 4, 16);
            let right = lanes(self.xmm_rm(bus, operand, 16)?, 4, 16);
            let values = left.iter().zip(right).map(|(left, right)| (*left as u32).wrapping_mul(right as u32) as u64).collect::<Vec<_>>();
            self.simd.xmm[destination as usize] = join(&values, 4);
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if map == 2 && pp == 1 && (0x38..=0x3F).contains(&op) {
            if wide { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            let width = match op & 3 { 0 => 1, 2 => 2, _ => 4 };
            let signed = op & 2 == 0;
            let maximum = op & 4 != 0;
            let left = lanes(self.simd.xmm[((!control >> 3) & 15) as usize], width, 16);
            let right = lanes(self.xmm_rm(bus, operand, 16)?, width, 16);
            let values = left.iter().zip(right).map(|(left, right)| {
                let smaller = if signed { sext(*left, width) < sext(right, width) } else { *left < right };
                if smaller != maximum { *left } else { right }
            }).collect::<Vec<_>>();
            self.simd.xmm[destination as usize] = join(&values, width);
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if map == 2 && pp == 1 && op == 0x29 {
            if wide { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            let left = lanes(self.simd.xmm[((!control >> 3) & 15) as usize], 8, 16);
            let right = lanes(self.xmm_rm(bus, operand, 16)?, 8, 16);
            let values = left.iter().zip(right).map(|(left, right)| if *left == right { u64::MAX } else { 0 }).collect::<Vec<_>>();
            self.simd.xmm[destination as usize] = join(&values, 8);
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if map == 2 {
            if wide || op != 0 || pp != 1 { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            let source = self.simd.xmm[((!control >> 3) & 15) as usize].to_le_bytes();
            let indices = self.xmm_rm(bus, operand, 16)?.to_le_bytes();
            let result = indices.map(|index| if index & 0x80 != 0 { 0 } else { source[(index & 15) as usize] });
            self.simd.xmm[destination as usize] = u128::from_le_bytes(result);
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if map == 1 && matches!(op, 0x6E | 0x7E) && pp == 1 {
            if wide || control & 0x78 != 0x78 { return Err(UD); }
            let size = if self.rex & 8 != 0 { 8 } else { 4 };
            let (reg, operand) = self.modrm(bus)?;
            if op == 0x6E {
                self.simd.xmm[reg as usize] = self.get(bus, operand, size)? as u128;
                self.simd.ymm_hi[reg as usize] = 0;
            } else {
                self.set(bus, operand, size, self.simd.xmm[reg as usize] as u64 & mask(size))?;
            }
            return Ok(());
        }
        if map == 1 && op == 0xD6 && pp == 1 && !wide {
            if control & 0x78 != 0x78 { return Err(UD); }
            let (source, operand) = self.modrm(bus)?;
            let value = self.simd.xmm[source as usize] & u64::MAX as u128;
            match operand {
                Op::Reg(destination) => {
                    self.simd.xmm[destination as usize] = value;
                    self.simd.ymm_hi[destination as usize] = 0;
                }
                Op::Mem(segment, address) => self.wr(bus, segment, address, 8, value as u64)?,
            }
            return Ok(());
        }
        if map == 1 && op == 0x7E && pp == 2 && !wide {
            if control & 0x78 != 0x78 { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            self.simd.xmm[destination as usize] = self.xmm_rm(bus, operand, 8)? & u64::MAX as u128;
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if map == 3 && op == 0x18 && pp == 1 && wide {
            let (destination, operand) = self.modrm(bus)?;
            let source = ((!control >> 3) & 15) as usize;
            let value = self.xmm_rm(bus, operand, 16)?;
            let high = self.fetch8(bus)? & 1 != 0;
            let low = if high { self.simd.xmm[source] } else { value };
            let high = if high { value } else { self.simd.ymm_hi[source] };
            self.simd.xmm[destination as usize] = low;
            self.simd.ymm_hi[destination as usize] = high;
            return Ok(());
        }
        if map == 1 && matches!(op, 0x7C | 0x7D) && matches!(pp, 1 | 3) {
            return self.vex_horizontal(bus, op == 0x7D, if pp == 1 { 8 } else { 4 }, ((!control >> 3) & 15) as usize, wide);
        }
        if map == 1 && pp <= 1 && matches!(op, 0x14 | 0x15) {
            return self.vex_unpack(bus, op, pp, ((!control >> 3) & 15) as usize, wide);
        }
        if map == 1 && matches!(op, 0x52 | 0x53) {
            return self.vex_reciprocal(bus, op, pp, ((!control >> 3) & 15) as usize, wide);
        }
        if map == 1 && op == 0xC6 && pp <= 1 {
            return self.vex_immediate_shuffle(bus, ((!control >> 3) & 15) as usize, wide, if pp == 0 { 4 } else { 8 }, ShuffleKind::Shuffle);
        }
        if map == 3 && op == 0x0E && pp == 1 {
            if wide { return Err(UD); }
            return self.vex_immediate_shuffle(bus, ((!control >> 3) & 15) as usize, false, 2, ShuffleKind::Blend);
        }
        if map == 3 && pp == 1 && matches!(op, 0x04 | 0x05 | 0x0C | 0x0D) {
            let permute = op < 8;
            if permute && control & 0x78 != 0x78 { return Err(UD); }
            return self.vex_immediate_shuffle(bus, ((!control >> 3) & 15) as usize, wide, if op & 1 == 0 { 4 } else { 8 }, if permute { ShuffleKind::Permute } else { ShuffleKind::Blend });
        }
        if map == 1 && op == 0xC2 {
            return self.vex_compare(bus, pp, ((!control >> 3) & 15) as usize, wide);
        }
        if map == 3 && pp == 1 && matches!(op, 0x4A | 0x4B) {
            return self.vex_blend(bus, ((!control >> 3) & 15) as usize, wide, if op == 0x4A { 4 } else { 8 });
        }
        if map == 3 && op == 6 && pp == 1 && wide {
            let (destination, operand) = self.modrm(bus)?;
            let first = ((!control >> 3) & 15) as usize;
            let low = self.xmm_rm(bus, operand, 16)?;
            let high = match operand {
                Op::Reg(reg) => self.simd.ymm_hi[reg as usize],
                Op::Mem(segment, address) => self.xmm_rm(bus, Op::Mem(segment, address.wrapping_add(16)), 16)?,
            };
            let values = [self.simd.xmm[first], self.simd.ymm_hi[first], low, high];
            let mode = self.fetch8(bus)? as u8;
            self.simd.xmm[destination as usize] = if mode & 8 != 0 { 0 } else { values[(mode & 3) as usize] };
            self.simd.ymm_hi[destination as usize] = if mode & 0x80 != 0 { 0 } else { values[((mode >> 4) & 3) as usize] };
            return Ok(());
        }
        if map == 3 && op == 0x19 && pp == 1 && wide {
            if control & 0x78 != 0x78 { return Err(UD); }
            let (source, operand) = self.modrm(bus)?;
            let high = self.fetch8(bus)? & 1 != 0;
            let value = if high { self.simd.ymm_hi[source as usize] } else { self.simd.xmm[source as usize] };
            match operand {
                Op::Reg(destination) => {
                    self.simd.xmm[destination as usize] = value;
                    self.simd.ymm_hi[destination as usize] = 0;
                }
                Op::Mem(segment, address) => self.store_mem(bus, segment, address, value, 16)?,
            }
            return Ok(());
        }
        if map == 1 && wide && ((pp <= 1 && matches!(op, 0x54..=0x57)) || (pp == 1 && matches!(op, 0xDB | 0xDF | 0xEB | 0xEF))) {
            let (destination, operand) = self.modrm(bus)?;
            let source = ((!control >> 3) & 15) as usize;
            let low = self.xmm_rm(bus, operand, 16)?;
            let high = match operand {
                Op::Reg(reg) => self.simd.ymm_hi[reg as usize],
                Op::Mem(segment, address) => self.xmm_rm(bus, Op::Mem(segment, address.wrapping_add(16)), 16)?,
            };
            let apply = |left: u128, right: u128| match op {
                0x54 | 0xDB => left & right,
                0x55 | 0xDF => !left & right,
                0x56 | 0xEB => left | right,
                _ => left ^ right,
            };
            let low = apply(self.simd.xmm[source], low);
            let high = apply(self.simd.ymm_hi[source], high);
            self.simd.xmm[destination as usize] = low;
            self.simd.ymm_hi[destination as usize] = high;
            return Ok(());
        }
        if wide && map == 1 && matches!(op, 0x58 | 0x59 | 0x5C | 0x5E) && pp <= 1 {
            return self.vex_packed_arithmetic(bus, if pp == 1 { 8 } else { 4 }, ((!control >> 3) & 15) as usize, op);
        }
        if map == 1 && op == 0x50 {
            if pp > 1 || control & 0x78 != 0x78 { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            let Op::Reg(source) = operand else { return Err(UD); };
            let width = if pp == 0 { 32 } else { 64 };
            let count = 128 / width;
            let mut result = 0;
            for (half, vector) in [self.simd.xmm[source as usize], self.simd.ymm_hi[source as usize]].into_iter().enumerate().take(if wide { 2 } else { 1 }) {
                for lane in 0..count {
                    result |= (((vector >> (lane * width + width - 1)) & 1) as u64) << (half * count + lane);
                }
            }
            self.set_reg(destination, 4, result);
            return Ok(());
        }
        if wide {
            if map != 1 || control & 0x78 != 0x78 { return Err(UD); }
            return self.vex_move256(bus, op, pp);
        }
        if map == 3 && matches!(op, 0x14 | 0x16 | 0x17) && pp == 1 {
            if control & 0x78 != 0x78 { return Err(UD); }
            let (source, operand) = self.modrm(bus)?;
            let width = if op == 0x14 { 1 } else if op == 0x16 && self.rex & 8 != 0 { 8 } else { 4 };
            let lane = self.fetch8(bus)? as u32 & (16 / width - 1);
            let value = (self.simd.xmm[source as usize] >> (lane * width * 8)) as u64 & mask(width);
            match operand {
                Op::Reg(destination) => self.set_reg(destination, width.max(4), value),
                Op::Mem(segment, address) => self.wr(bus, segment, address, width, value)?,
            }
            return Ok(());
        }
        if map == 3 && (0x60..=0x63).contains(&op) {
            if pp != 1 || control & 0x78 != 0x78 { return Err(UD); }
            return self.vex_string_index(bus, op & 2 == 0, op & 1 == 0);
        }
        if map == 3 && op == 0x22 && pp == 1 {
            let (destination, operand) = self.modrm(bus)?;
            let width = if self.rex & 8 != 0 { 8 } else { 4 };
            let value = self.get(bus, operand, width)? as u128;
            let lane = self.fetch8(bus)? as u32 & (16 / width - 1);
            let source = ((!control >> 3) & 15) as usize;
            let shift = lane * width * 8;
            let bits = (mask(width) as u128) << shift;
            self.simd.xmm[destination as usize] = (self.simd.xmm[source] & !bits) | (value << shift);
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if map == 3 && op == 0x21 && pp == 1 {
            let (destination, operand) = self.modrm(bus)?;
            let value = self.xmm_rm(bus, operand, 4)?;
            let mode = self.fetch8(bus)? as u8;
            let selected = if matches!(operand, Op::Reg(_)) { (mode >> 6) as u32 } else { 0 };
            let lane = (mode >> 4) as u32 & 3;
            let value = (value >> (selected * 32)) & u32::MAX as u128;
            let source = ((!control >> 3) & 15) as usize;
            let mut result = (self.simd.xmm[source] & !((u32::MAX as u128) << (lane * 32))) | (value << (lane * 32));
            for index in 0..4 {
                if mode & (1 << index) != 0 { result &= !((u32::MAX as u128) << (index * 32)); }
            }
            self.simd.xmm[destination as usize] = result;
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if map == 3 {
            return self.vex_round(bus, op, pp, ((!control >> 3) & 15) as usize);
        }
        if (pp == 2 && matches!(op, 0x12 | 0x16)) || (pp == 3 && op == 0x12) {
            if control & 0x78 != 0x78 { return Err(UD); }
            let (destination, operand) = self.modrm(bus)?;
            let source = self.xmm_rm(bus, operand, if pp == 3 { 8 } else { 16 })?;
            let value = if pp == 3 {
                let low = source & u64::MAX as u128;
                low | (low << 64)
            } else {
                let values = lanes(source, 4, 16);
                let offset = if op == 0x16 { 1 } else { 0 };
                join(&[values[offset], values[offset], values[offset + 2], values[offset + 2]], 4)
            };
            self.simd.xmm[destination as usize] = value;
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if pp <= 1 && matches!(op, 0x12 | 0x13 | 0x16 | 0x17) {
            let (reg, operand) = self.modrm(bus)?;
            let upper = op & 4 != 0;
            if op & 1 != 0 {
                if control & 0x78 != 0x78 { return Err(UD); }
                let Op::Mem(segment, address) = operand else { return Err(UD); };
                let value = self.simd.xmm[reg as usize] >> if upper { 64 } else { 0 };
                self.wr(bus, segment, address, 8, value as u64)?;
            } else {
                let source = self.simd.xmm[((!control >> 3) & 15) as usize];
                let value = match operand {
                    Op::Reg(other) if pp == 0 => self.simd.xmm[other as usize] >> if upper { 0 } else { 64 },
                    Op::Mem(segment, address) => self.rd(bus, segment, address, 8)? as u128,
                    _ => return Err(UD),
                } & u64::MAX as u128;
                self.simd.xmm[reg as usize] = if upper { (source & u64::MAX as u128) | (value << 64) } else { (source & !(u64::MAX as u128)) | value };
                self.simd.ymm_hi[reg as usize] = 0;
            }
            return Ok(());
        }
        if pp == 1 && matches!(op, 0x60..=0x6D | 0x74..=0x76 | 0xD4 | 0xF8..=0xFE | 0xDB | 0xDF | 0xEB | 0xEF) {
            let (destination, operand) = self.modrm(bus)?;
            let right = self.xmm_rm(bus, operand, 16)?;
            let left = self.simd.xmm[((!control >> 3) & 15) as usize];
            self.simd.xmm[destination as usize] = Self::int_op(op, left, right, 16).ok_or(UD)?;
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if matches!(op, 0x71..=0x73) && pp == 1 {
            let destination = ((!control >> 3) & 15) as usize;
            let (group, operand) = self.modrm(bus)?;
            let Op::Reg(source) = operand else { return Err(UD); };
            let count = self.fetch8(bus)? as u32;
            let value = self.simd.xmm[source as usize];
            let width = 2 << (op - 0x71);
            let result = match group & 7 {
                2 => Self::vshift(value, count as u64, width, 16, 0),
                4 if width < 8 => Self::vshift(value, count as u64, width, 16, 1),
                6 => Self::vshift(value, count as u64, width, 16, 2),
                3 if op == 0x73 => if count >= 16 { 0 } else { value >> (count * 8) },
                7 if op == 0x73 => if count >= 16 { 0 } else { value << (count * 8) },
                _ => return Err(UD),
            };
            self.simd.xmm[destination] = result;
            self.simd.ymm_hi[destination] = 0;
            return Ok(());
        }
        if op == 0x70 && pp == 1 && control & 0x78 == 0x78 {
            let (destination, operand) = self.modrm(bus)?;
            let source = self.xmm_rm(bus, operand, 16)?;
            let control = self.fetch8(bus)?;
            let values = lanes(source, 4, 16);
            let result: Vec<u64> = (0..4).map(|index| values[((control >> (index * 2)) & 3) as usize]).collect();
            self.simd.xmm[destination as usize] = join(&result, 4);
            self.simd.ymm_hi[destination as usize] = 0;
            return Ok(());
        }
        if self.vex_float(bus, op, pp, ((!control >> 3) & 15) as usize)? {
            return Ok(());
        }
        if matches!(op, 0x10 | 0x11) && pp >= 2 {
            let source = ((!control >> 3) & 15) as usize;
            let (reg, rm) = self.modrm(bus)?;
            let bytes = if pp == 2 { 4 } else { 8 };
            let mask = if bytes == 4 { u32::MAX as u128 } else { u64::MAX as u128 };
            match (op, rm) {
                (0x10, Op::Mem(segment, address)) => {
                    if source != 0 { return Err(UD); }
                    self.simd.xmm[reg as usize] = self.rd(bus, segment, address, bytes)? as u128;
                    self.simd.ymm_hi[reg as usize] = 0;
                }
                (0x11, Op::Mem(segment, address)) => {
                    if source != 0 { return Err(UD); }
                    self.wr(bus, segment, address, bytes, self.simd.xmm[reg as usize] as u64 & mask as u64)?;
                }
                (0x10, Op::Reg(other)) => {
                    self.simd.xmm[reg as usize] = (self.simd.xmm[source] & !mask) | (self.simd.xmm[other as usize] & mask);
                    self.simd.ymm_hi[reg as usize] = 0;
                }
                (_, Op::Reg(other)) => {
                    self.simd.xmm[other as usize] = (self.simd.xmm[source] & !mask) | (self.simd.xmm[reg as usize] & mask);
                    self.simd.ymm_hi[other as usize] = 0;
                }
                _ => return Err(UD),
            }
            return Ok(());
        }
        if control & 0x78 != 0x78 {
            return Err(UD);
        }
        let supported = match op {
            0x10 | 0x11 | 0x28 | 0x29 => pp <= 1,
            0x6F | 0x7F => matches!(pp, 1 | 2),
            _ => false,
        };
        if !supported {
            return Err(UD);
        }
        self.p66 = pp == 1;
        self.rep = if pp == 2 { 0xF3 } else { 0 };
        if prefix == 0xC4 && control & 0x80 != 0 {
            self.rex |= 8;
        }
        let (reg, rm) = self.modrm(bus)?;
        let load = matches!(op, 0x10 | 0x28 | 0x6F);
        if matches!(op, 0x28 | 0x29) || (matches!(op, 0x6F | 0x7F) && pp == 1) {
            if let Op::Mem(segment, address) = rm {
                if self.base[segment].wrapping_add(address) & 15 != 0 { return Err(gp(0)); }
            }
        }
        self.mov128(bus, load, reg as usize, rm)?;
        if load { self.simd.ymm_hi[reg as usize] = 0; }
        else if let Op::Reg(other) = rm { self.simd.ymm_hi[other as usize] = 0; }
        Ok(())
    }

    fn vector_operand<B: Bus>(&mut self, bus: &mut B, operand: Op, wide: bool) -> R<[u128; 2]> {
        let low = self.xmm_rm(bus, operand, 16)?;
        let high = if wide {
            match operand {
                Op::Reg(reg) => self.simd.ymm_hi[reg as usize],
                Op::Mem(segment, address) => self.xmm_rm(bus, Op::Mem(segment, address.wrapping_add(16)), 16)?,
            }
        } else { 0 };
        Ok([low, high])
    }

    fn vex_immediate_shuffle<B: Bus>(&mut self, bus: &mut B, source: usize, wide: bool, width: u32, kind: ShuffleKind) -> R<()> {
        let (destination, operand) = self.modrm(bus)?;
        let right = self.vector_operand(bus, operand, wide)?;
        let mode = self.fetch8(bus)? as u32;
        let left = [self.simd.xmm[source], self.simd.ymm_hi[source]];
        let mut result = [0u128; 2];
        for half in 0..if wide { 2 } else { 1 } {
            let a = lanes(left[half], width, 16);
            let b = lanes(right[half], width, 16);
            let mut selected = Vec::with_capacity(a.len());
            for lane in 0..a.len() {
                let value = match kind {
                    ShuffleKind::Blend => if mode & (1 << (half * a.len() + lane)) != 0 { b[lane] } else { a[lane] },
                    ShuffleKind::Permute => {
                        let index = if width == 4 { (mode >> (lane * 2)) & 3 } else { (mode >> (half * 2 + lane)) & 1 };
                        b[index as usize]
                    }
                    ShuffleKind::Shuffle => {
                        let index = if width == 4 { (mode >> (lane * 2)) & 3 } else { (mode >> (half * 2 + lane)) & 1 };
                        if lane < a.len() / 2 { a[index as usize] } else { b[index as usize] }
                    }
                };
                selected.push(value);
            }
            result[half] = join(&selected, width);
        }
        self.simd.xmm[destination as usize] = result[0];
        self.simd.ymm_hi[destination as usize] = result[1];
        Ok(())
    }

    fn vex_blend<B: Bus>(&mut self, bus: &mut B, source: usize, wide: bool, width: u32) -> R<()> {
        let (destination, operand) = self.modrm(bus)?;
        let right = self.vector_operand(bus, operand, wide)?;
        let mask = (self.fetch8(bus)? >> 4) as usize;
        let masks = [self.simd.xmm[mask], self.simd.ymm_hi[mask]];
        let left = [self.simd.xmm[source], self.simd.ymm_hi[source]];
        let mut result = [0u128; 2];
        for half in 0..if wide { 2 } else { 1 } {
            let a = lanes(left[half], width, 16);
            let b = lanes(right[half], width, 16);
            let masks = lanes(masks[half], width, 16);
            let selected: Vec<u64> = a.iter().zip(b).zip(masks).map(|((a, b), mask)| if mask & (1 << (width * 8 - 1)) != 0 { b } else { *a }).collect();
            result[half] = join(&selected, width);
        }
        self.simd.xmm[destination as usize] = result[0];
        self.simd.ymm_hi[destination as usize] = result[1];
        Ok(())
    }

    fn vex_packed_arithmetic<B: Bus>(&mut self, bus: &mut B, width: u32, source: usize, operation: u8) -> R<()> {
        let (destination, operand) = self.modrm(bus)?;
        let right = [self.xmm_rm(bus, operand, 16)?, match operand {
            Op::Reg(register) => self.simd.ymm_hi[register as usize],
            Op::Mem(segment, address) => self.xmm_rm(bus, Op::Mem(segment, address.wrapping_add(16)), 16)?,
        }];
        let left = [self.simd.xmm[source], self.simd.ymm_hi[source]];
        let mut result = [0; 2];
        let mut flags = 0;
        for half in 0..2 {
            let mut values = Vec::new();
            for (left, right) in lanes(left[half], width, 16).into_iter().zip(lanes(right[half], width, 16)) {
                let (value, status) = self.float_pair(left, right, width, operation)?;
                values.push(value);
                flags |= status;
            }
            result[half] = join(&values, width);
        }
        self.simd.mxcsr |= flags;
        if flags & !(self.simd.mxcsr >> 7) & 0x3F != 0 { return Err(if self.cr4 & (1 << 10) != 0 { Ex(19, None) } else { UD }); }
        self.simd.xmm[destination as usize] = result[0];
        self.simd.ymm_hi[destination as usize] = result[1];
        Ok(())
    }

    fn float_pair(&self, left: u64, right: u64, width: u32, operation: u8) -> R<(u64, u32)> {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::__m128i;
            let mut saved = 0u32;
            let mut status = 0u32;
            let control = (self.simd.mxcsr & 0xFFC0) | 0x1F80;
            let mut value = std::mem::transmute::<u128, __m128i>(left as u128);
            let operand = std::mem::transmute::<u128, __m128i>(right as u128);
            macro_rules! calculate {
                ($operation:literal) => {{
                    std::arch::asm!(
                        "stmxcsr [{saved}]",
                        "ldmxcsr [{control}]",
                        $operation,
                        "stmxcsr [{status}]",
                        "ldmxcsr [{saved}]",
                        saved = in(reg) &mut saved,
                        control = in(reg) &control,
                        status = in(reg) &mut status,
                        value = inout(xmm_reg) value,
                        operand = in(xmm_reg) operand,
                        options(nostack)
                    );
                }};
            }
            match (width, operation) {
                (4, 0x58) => calculate!("addss {value}, {operand}"),
                (4, 0x59) => calculate!("mulss {value}, {operand}"),
                (4, 0x5C) => calculate!("subss {value}, {operand}"),
                (4, 0x5E) => calculate!("divss {value}, {operand}"),
                (8, 0x58) => calculate!("addsd {value}, {operand}"),
                (8, 0x59) => calculate!("mulsd {value}, {operand}"),
                (8, 0x5C) => calculate!("subsd {value}, {operand}"),
                (8, 0x5E) => calculate!("divsd {value}, {operand}"),
                _ => return Err(UD),
            }
            return Ok((std::mem::transmute::<__m128i, u128>(value) as u64, status & 0x3F));
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (left, right, width, operation);
            Err(UD)
        }
    }

    fn vex_horizontal<B: Bus>(&mut self, bus: &mut B, subtract: bool, width: u32, source: usize, wide: bool) -> R<()> {
        let (destination, operand) = self.modrm(bus)?;
        let low = self.xmm_rm(bus, operand, 16)?;
        let high = if wide {
            match operand {
                Op::Reg(register) => self.simd.ymm_hi[register as usize],
                Op::Mem(segment, address) => self.xmm_rm(bus, Op::Mem(segment, address.wrapping_add(16)), 16)?,
            }
        } else { 0 };
        let left = [self.simd.xmm[source], self.simd.ymm_hi[source]];
        let right = [low, high];
        let mut result = [0u128; 2];
        let mut flags = 0;
        for half in 0..if wide { 2 } else { 1 } {
            let mut output = Vec::new();
            for vector in [left[half], right[half]] {
                let values = lanes(vector, width, 16);
                for pair in values.chunks_exact(2) {
                    let (value, status) = self.float_pair(pair[0], pair[1], width, if subtract { 0x5C } else { 0x58 })?;
                    output.push(value);
                    flags |= status;
                }
            }
            result[half] = join(&output, width);
        }
        self.simd.mxcsr |= flags;
        if flags & !(self.simd.mxcsr >> 7) & 0x3F != 0 { return Err(if self.cr4 & (1 << 10) != 0 { Ex(19, None) } else { UD }); }
        self.simd.xmm[destination as usize] = result[0];
        self.simd.ymm_hi[destination as usize] = result[1];
        Ok(())
    }

    fn vex_unpack<B: Bus>(&mut self, bus: &mut B, op: u8, prefix: u8, source: usize, wide: bool) -> R<()> {
        let (destination, operand) = self.modrm(bus)?;
        let operation = match (prefix, op) { (0, 0x14) => 0x62, (0, _) => 0x6A, (_, 0x14) => 0x6C, _ => 0x6D };
        let low = self.xmm_rm(bus, operand, 16)?;
        let high = if wide {
            match operand {
                Op::Reg(reg) => self.simd.ymm_hi[reg as usize],
                Op::Mem(segment, address) => self.xmm_rm(bus, Op::Mem(segment, address.wrapping_add(16)), 16)?,
            }
        } else { 0 };
        let low = Self::int_op(operation, self.simd.xmm[source], low, 16).ok_or(UD)?;
        let high = if wide { Self::int_op(operation, self.simd.ymm_hi[source], high, 16).ok_or(UD)? } else { 0 };
        self.simd.xmm[destination as usize] = low;
        self.simd.ymm_hi[destination as usize] = high;
        Ok(())
    }

    fn vex_move256<B: Bus>(&mut self, bus: &mut B, op: u8, prefix: u8) -> R<()> {
        let valid = match op {
            0x10 | 0x11 | 0x28 | 0x29 => prefix <= 1,
            0x6F | 0x7F => matches!(prefix, 1 | 2),
            _ => false,
        };
        if !valid { return Err(UD); }
        let (reg, rm) = self.modrm(bus)?;
        if matches!(op, 0x28 | 0x29) || (matches!(op, 0x6F | 0x7F) && prefix == 1) {
            if let Op::Mem(segment, address) = rm {
                if self.base[segment].wrapping_add(address) & 31 != 0 { return Err(gp(0)); }
            }
        }
        if matches!(op, 0x10 | 0x28 | 0x6F) {
            let low = self.xmm_rm(bus, rm, 16)?;
            let high = match rm {
                Op::Reg(other) => self.simd.ymm_hi[other as usize],
                Op::Mem(segment, address) => self.xmm_rm(bus, Op::Mem(segment, address.wrapping_add(16)), 16)?,
            };
            self.simd.xmm[reg as usize] = low;
            self.simd.ymm_hi[reg as usize] = high;
        } else {
            let (low, high) = (self.simd.xmm[reg as usize], self.simd.ymm_hi[reg as usize]);
            match rm {
                Op::Reg(other) => {
                    self.simd.xmm[other as usize] = low;
                    self.simd.ymm_hi[other as usize] = high;
                }
                Op::Mem(segment, address) => {
                    self.store_mem(bus, segment, address, low, 16)?;
                    self.store_mem(bus, segment, address.wrapping_add(16), high, 16)?;
                }
            }
        }
        Ok(())
    }

    fn vex_string_index<B: Bus>(&mut self, bus: &mut B, explicit: bool, output_mask: bool) -> R<()> {
        let (reg, operand) = self.modrm(bus)?;
        let left = self.simd.xmm[reg as usize];
        let right = self.xmm_rm(bus, operand, 16)?;
        let mode = self.fetch8(bus)? as u8;
        let aggregation = (mode >> 2) & 3;
        if !matches!(aggregation, 0 | 2) { return Err(UD); }
        let width = if mode & 1 == 0 { 1 } else { 2 };
        let left = lanes(left, width, 16);
        let right = lanes(right, width, 16);
        let count = left.len();
        let length = |value: u64, lanes: &[u64]| -> usize {
            if explicit {
                let value = if self.rex & 8 != 0 { value as i64 } else { value as i32 as i64 };
                value.unsigned_abs().min(count as u64) as usize
            } else { lanes.iter().position(|value| *value == 0).unwrap_or(count) }
        };
        let left_length = length(self.r[0], &left);
        let right_length = length(self.r[2], &right);
        let mut result = 0u32;
        for index in 0..count {
            let matched = if aggregation == 0 {
                index < right_length && left[..left_length].contains(&right[index])
            } else {
                match (index < left_length, index < right_length) {
                    (true, true) => left[index] == right[index],
                    (false, false) => true,
                    _ => false,
                }
            };
            if matched { result |= 1 << index; }
        }
        match (mode >> 4) & 3 {
            1 => result ^= (1 << count) - 1,
            3 => result ^= (1 << right_length) - 1,
            _ => {}
        }
        let index = if result == 0 { count as u32 } else if mode & 0x40 == 0 { result.trailing_zeros() } else { 31 - result.leading_zeros() };
        if output_mask {
            self.simd.xmm[0] = if mode & 0x40 == 0 { result as u128 } else {
                join(&(0..count).map(|lane| if result & (1 << lane) != 0 { mask(width) } else { 0 }).collect::<Vec<_>>(), width)
            };
            self.simd.ymm_hi[0] = 0;
        } else {
            self.set_reg(1, 4, index as u64);
        }
        self.eflags &= !(AF | PF);
        self.setf(CF, result != 0);
        self.setf(ZF, right_length < count);
        self.setf(SF, left_length < count);
        self.setf(OF, result & 1 != 0);
        Ok(())
    }

    fn vex_round<B: Bus>(&mut self, bus: &mut B, op: u8, prefix: u8, source: usize) -> R<()> {
        if prefix != 1 || !matches!(op, 0x08..=0x0B) || (op < 0x0A && source != 0) {
            return Err(UD);
        }
        let (reg, rm) = self.modrm(bus)?;
        let bytes = match op { 0x0A => 4, 0x0B => 8, _ => 16 };
        let value = self.xmm_rm(bus, rm, bytes)?;
        let mode = self.fetch8(bus)? as u8;
        let round = |value: f64| {
            if mode & 4 != 0 { return self.round_mx(value); }
            match mode & 3 {
                0 => value.round_ties_even(),
                1 => value.floor(),
                2 => value.ceil(),
                _ => value.trunc(),
            }
        };
        self.simd.xmm[reg as usize] = match op {
            0x08 => from_f32s(f32s(value).map(|lane| round(lane as f64) as f32)),
            0x09 => from_f64s(f64s(value).map(round)),
            0x0A => (self.simd.xmm[source] & !(u32::MAX as u128)) | (round(f32::from_bits(value as u32) as f64) as f32).to_bits() as u128,
            _ => (self.simd.xmm[source] & !(u64::MAX as u128)) | round(f64::from_bits(value as u64)).to_bits() as u128,
        };
        self.simd.ymm_hi[reg as usize] = 0;
        Ok(())
    }

    fn approximate_reciprocal(bits: u32, square_root: bool) -> u32 {
        let value = f32::from_bits(bits);
        if bits & 0x7F80_0000 == 0 { return (bits & 0x8000_0000) | 0x7F80_0000; }
        if value.is_nan() { return bits | 0x0040_0000; }
        if square_root && value < 0.0 { return 0xFFC0_0000; }
        let result = 1.0 / if square_root { value.sqrt() } else { value };
        if result.is_subnormal() { result.to_bits() & 0x8000_0000 } else { result.to_bits() }
    }

    fn vex_reciprocal<B: Bus>(&mut self, bus: &mut B, op: u8, prefix: u8, source: usize, wide: bool) -> R<()> {
        let scalar = prefix == 2;
        if !matches!(prefix, 0 | 2) || (wide && scalar) || (!scalar && source != 0) { return Err(UD); }
        let (destination, operand) = self.modrm(bus)?;
        let right = if scalar { [self.xmm_rm(bus, operand, 4)?, 0] } else { self.vector_operand(bus, operand, wide)? };
        let mut result = if scalar { [self.simd.xmm[source], 0] } else { [0u128; 2] };
        for half in 0..if wide { 2 } else { 1 } {
            for lane in 0..if scalar { 1 } else { 4 } {
                let value = Self::approximate_reciprocal((right[half] >> (lane * 32)) as u32, op == 0x52);
                result[half] = (result[half] & !((u32::MAX as u128) << (lane * 32))) | ((value as u128) << (lane * 32));
            }
        }
        self.simd.xmm[destination as usize] = result[0];
        self.simd.ymm_hi[destination as usize] = result[1];
        Ok(())
    }

    fn vex_compare<B: Bus>(&mut self, bus: &mut B, prefix: u8, source: usize, wide: bool) -> R<()> {
        let scalar = prefix >= 2;
        if scalar && wide { return Err(UD); }
        let width = if prefix & 1 == 0 { 4 } else { 8 };
        let (destination, operand) = self.modrm(bus)?;
        let right = if scalar { [self.xmm_rm(bus, operand, width)?, 0] } else { self.vector_operand(bus, operand, wide)? };
        let predicate = self.fetch8(bus)? & 31;
        if predicate > 7 { return Err(UD); }
        let left = [self.simd.xmm[source], self.simd.ymm_hi[source]];
        let mut result = if scalar { [left[0], 0] } else { [0u128; 2] };
        let mut invalid = false;
        for half in 0..if wide { 2 } else { 1 } {
            for lane in 0..if scalar { 1 } else { 16 / width } {
                let shift = lane * width * 8;
                let a = (left[half] >> shift) as u64 & mask(width);
                let b = (right[half] >> shift) as u64 & mask(width);
                let (x, y, nan, signaling_nan) = if width == 4 {
                    let x = f32::from_bits(a as u32);
                    let y = f32::from_bits(b as u32);
                    (x as f64, y as f64, x.is_nan() || y.is_nan(), (x.is_nan() && a & (1 << 22) == 0) || (y.is_nan() && b & (1 << 22) == 0))
                } else {
                    let x = f64::from_bits(a);
                    let y = f64::from_bits(b);
                    (x, y, x.is_nan() || y.is_nan(), (x.is_nan() && a & (1 << 51) == 0) || (y.is_nan() && b & (1 << 51) == 0))
                };
                invalid |= signaling_nan || (nan && matches!(predicate, 1 | 2 | 5 | 6));
                let bits = if Self::cmp_pred(predicate, x, y) { mask(width) as u128 } else { 0 };
                result[half] = (result[half] & !((mask(width) as u128) << shift)) | (bits << shift);
            }
        }
        if invalid {
            self.simd.mxcsr |= 1;
            if self.simd.mxcsr & 0x80 == 0 { return Err(if self.cr4 & (1 << 10) != 0 { Ex(19, None) } else { UD }); }
        }
        self.simd.xmm[destination as usize] = result[0];
        self.simd.ymm_hi[destination as usize] = result[1];
        Ok(())
    }

    fn vex_float<B: Bus>(&mut self, bus: &mut B, op: u8, prefix: u8, source: usize) -> R<bool> {
        if !matches!(op, 0x2A | 0x2C | 0x2D | 0x2E | 0x2F | 0x51 | 0x54..=0x5A | 0x5C..=0x5F) {
            return Ok(false);
        }
        let (reg, rm) = self.modrm(bus)?;
        let destination = reg as usize;
        let size = if self.rex & 8 != 0 { 8 } else { 4 };
        if op == 0x2A {
            if prefix < 2 { return Err(UD); }
            let value = self.get(bus, rm, size)?;
            let value = if size == 8 { value as i64 as f64 } else { value as i32 as f64 };
            let upper = self.simd.xmm[source];
            self.simd.xmm[destination] = if prefix == 3 {
                (upper & !(u64::MAX as u128)) | value.to_bits() as u128
            } else {
                (upper & !(u32::MAX as u128)) | (value as f32).to_bits() as u128
            };
            self.simd.ymm_hi[destination] = 0;
            return Ok(true);
        }
        if matches!(op, 0x2C | 0x2D) {
            if prefix < 2 || source != 0 { return Err(UD); }
            let value = self.xmm_rm(bus, rm, if prefix == 3 { 8 } else { 4 })?;
            let value = if prefix == 3 { f64::from_bits(value as u64) } else { f32::from_bits(value as u32) as f64 };
            let rounded = if op == 0x2C { value.trunc() } else { self.round_mx(value) };
            let integer = if size == 8 { f2i64(rounded) } else { f2i32(rounded) as u64 };
            self.set_reg(reg, size, integer);
            return Ok(true);
        }
        if matches!(op, 0x2E | 0x2F) {
            if prefix > 1 || source != 0 { return Err(UD); }
            let value = self.xmm_rm(bus, rm, if prefix == 1 { 8 } else { 4 })?;
            let (left, right) = if prefix == 1 {
                (f64::from_bits(self.simd.xmm[destination] as u64), f64::from_bits(value as u64))
            } else {
                (f32::from_bits(self.simd.xmm[destination] as u32) as f64, f32::from_bits(value as u32) as f64)
            };
            self.eflags &= !(OF | SF | AF | CF | PF | ZF);
            if left.is_nan() || right.is_nan() {
                self.eflags |= CF | PF | ZF;
            } else if left < right {
                self.eflags |= CF;
            } else if left == right {
                self.eflags |= ZF;
            }
            return Ok(true);
        }
        if op == 0x5A {
            let bytes = match prefix { 0 | 3 => 8, 1 => 16, _ => 4 };
            let value = self.xmm_rm(bus, rm, bytes)?;
            let upper = self.simd.xmm[source];
            self.simd.xmm[destination] = match prefix {
                0 if source == 0 => {
                    let values = f32s(value);
                    from_f64s([values[0] as f64, values[1] as f64])
                }
                1 if source == 0 => {
                    let values = f64s(value);
                    from_f32s([values[0] as f32, values[1] as f32, 0.0, 0.0])
                }
                2 => (upper & !(u64::MAX as u128)) | (f32::from_bits(value as u32) as f64).to_bits() as u128,
                3 => (upper & !(u32::MAX as u128)) | (f64::from_bits(value as u64) as f32).to_bits() as u128,
                _ => return Err(UD),
            };
            self.simd.ymm_hi[destination] = 0;
            return Ok(true);
        }
        if op == 0x51 && prefix <= 1 && source != 0 { return Err(UD); }
        let bytes = match prefix { 2 => 4, 3 => 8, _ => 16 };
        let right = self.xmm_rm(bus, rm, bytes)?;
        let left = self.simd.xmm[source];
        let result = if (0x54..=0x57).contains(&op) {
            if prefix >= 2 { return Err(UD); }
            match op { 0x54 => left & right, 0x55 => !left & right, 0x56 => left | right, _ => left ^ right }
        } else {
            match prefix {
                0 => from_f32s(std::array::from_fn(|index| {
                    f32::from_bits(Self::f32_op(op, (left >> (index * 32)) as u32, (right >> (index * 32)) as u32))
                })),
                1 => from_f64s(std::array::from_fn(|index| {
                    f64::from_bits(Self::f64_op(op, (left >> (index * 64)) as u64, (right >> (index * 64)) as u64))
                })),
                2 => (left & !(u32::MAX as u128)) | Self::f32_op(op, left as u32, right as u32) as u128,
                _ => (left & !(u64::MAX as u128)) | Self::f64_op(op, left as u64, right as u64) as u128,
            }
        };
        self.simd.xmm[destination] = result;
        self.simd.ymm_hi[destination] = 0;
        Ok(true)
    }

    fn round_mx(&self, v: f64) -> f64 {
        match (self.simd.mxcsr >> 13) & 3 {
            0 => {
                let r = v.round();
                if (v - v.trunc()).abs() == 0.5 {
                    2.0 * (v / 2.0).round()
                } else {
                    r
                }
            }
            1 => v.floor(),
            2 => v.ceil(),
            _ => v.trunc(),
        }
    }

    fn xmm_rm<B: Bus>(&mut self, bus: &mut B, rm: Op, bytes: u32) -> R<u128> {
        match rm {
            Op::Reg(i) => Ok(self.simd.xmm[i as usize]),
            Op::Mem(s, a) => {
                let mut v = 0u128;
                let mut k = 0;
                while k < bytes {
                    let w = (bytes - k).min(8);
                    let w = if w >= 8 { 8 } else if w >= 4 { 4 } else if w >= 2 { 2 } else { 1 };
                    v |= (self.rd(bus, s, a.wrapping_add(k as u64), w)? as u128) << (k * 8);
                    k += w;
                }
                Ok(v)
            }
        }
    }
    fn store_mem<B: Bus>(&mut self, bus: &mut B, s: usize, a: u64, v: u128, bytes: u32) -> R<()> {
        let mut k = 0;
        while k < bytes {
            let w = (bytes - k).min(8);
            self.wr(bus, s, a.wrapping_add(k as u64), w, (v >> (k * 8)) as u64 & mask(w))?;
            k += w;
        }
        Ok(())
    }
    fn mm_rm<B: Bus>(&mut self, bus: &mut B, rm: Op) -> R<u64> {
        match rm {
            Op::Reg(i) => Ok(self.simd.mm[(i & 7) as usize]),
            Op::Mem(s, a) => self.rd(bus, s, a, 8),
        }
    }

    /// Single-precision operation.
    fn f32_op(op: u8, a: u32, c: u32) -> u32 {
        let (x, y) = (f32::from_bits(a), f32::from_bits(c));
        match op {
            0x51 => y.sqrt().to_bits(),
            0x52 => (1.0 / y.sqrt()).to_bits(),
            0x53 => (1.0 / y).to_bits(),
            0x58 => (x + y).to_bits(),
            0x59 => (x * y).to_bits(),
            0x5C => (x - y).to_bits(),
            // MIN/MAX return the second operand for NaNs and equal zeros.
            0x5D => {
                if x < y {
                    a
                } else {
                    c
                }
            }
            0x5E => (x / y).to_bits(),
            _ => {
                if x > y {
                    a
                } else {
                    c
                }
            }
        }
    }
    fn f64_op(op: u8, a: u64, c: u64) -> u64 {
        let (x, y) = (f64::from_bits(a), f64::from_bits(c));
        match op {
            0x51 => y.sqrt().to_bits(),
            0x58 => (x + y).to_bits(),
            0x59 => (x * y).to_bits(),
            0x5C => (x - y).to_bits(),
            0x5D => {
                if x < y {
                    a
                } else {
                    c
                }
            }
            0x5E => (x / y).to_bits(),
            _ => {
                if x > y {
                    a
                } else {
                    c
                }
            }
        }
    }

    fn cmp_pred(p: u64, x: f64, y: f64) -> bool {
        let un = x.is_nan() || y.is_nan();
        match p & 7 {
            0 => x == y,
            1 => x < y,
            2 => x <= y,
            3 => un,
            4 => x != y || un,
            5 => !(x < y),
            6 => !(x <= y),
            _ => !un,
        }
    }

    /// 0F-prefixed MMX/SSE/SSE2 instruction `op`.
    pub(crate) fn sse_exec<B: Bus>(&mut self, bus: &mut B, op: u32) -> R<()> {
        if self.cr0 & 0x4 != 0 {
            return Err(UD); // CR0.EM
        }
        if self.cr0 & 0x8 != 0 {
            return Err(Ex(7, None)); // CR0.TS: lazy FPU switching
        }
        let op = op as u8;
        let pfx = match self.rep {
            0xF3 => Pfx::F3,
            0xF2 => Pfx::F2,
            _ if self.p66 => Pfx::P66,
            _ => Pfx::None,
        };
        if matches!(pfx, Pfx::P66 | Pfx::F2) && self.model == Model::PentiumIII {
            return Err(UD);
        }
        if op == 0x77 {
            self.fpu.empty = 0xFF; // EMMS
            return Ok(());
        }
        if op == 0xAE {
            return self.sse_ae(bus);
        }
        let (reg, rm) = self.modrm(bus)?;
        let r = reg as usize;
        let x = pfx == Pfx::P66; // XMM form of an MMX integer instruction
        match op {
            // ---------------------------------------------------------- moves
            0x10 | 0x11 => match pfx {
                Pfx::F3 | Pfx::F2 => {
                    let n = if pfx == Pfx::F3 { 4 } else { 8 };
                    let m: u128 = if n == 4 { 0xFFFF_FFFF } else { u64::MAX as u128 };
                    if op == 0x10 {
                        self.simd.xmm[r] = match rm {
                            Op::Reg(i) => (self.simd.xmm[r] & !m) | (self.simd.xmm[i as usize] & m),
                            Op::Mem(s, a) => self.rd(bus, s, a, n)? as u128,
                        };
                    } else {
                        let v = self.simd.xmm[r] & m;
                        match rm {
                            Op::Reg(i) => self.simd.xmm[i as usize] = (self.simd.xmm[i as usize] & !m) | v,
                            Op::Mem(s, a) => self.wr(bus, s, a, n, v as u64)?,
                        }
                    }
                }
                _ => self.mov128(bus, op == 0x10, r, rm)?,
            },
            0x28 | 0x29 | 0x2B => self.mov128(bus, op == 0x28, r, rm)?,
            0x12 | 0x13 | 0x16 | 0x17 => {
                if matches!(pfx, Pfx::F3 | Pfx::F2) {
                    return Err(UD); // SSE3 forms
                }
                let hi = op >= 0x16;
                let sh = if hi { 64 } else { 0 };
                let lane_mask: u128 = (u64::MAX as u128) << sh;
                match (op & 1, rm) {
                    // MOVHLPS / MOVLHPS (register forms of 12 / 16, no prefix).
                    (0, Op::Reg(i)) if pfx == Pfx::None => {
                        let src = self.simd.xmm[i as usize];
                        let lane = if hi { src << 64 } else { src >> 64 };
                        self.simd.xmm[r] = (self.simd.xmm[r] & !lane_mask) | (lane & lane_mask);
                    }
                    (0, Op::Mem(s, a)) => {
                        let v = self.rd(bus, s, a, 8)? as u128;
                        self.simd.xmm[r] = (self.simd.xmm[r] & !lane_mask) | v << sh;
                    }
                    (_, Op::Mem(s, a)) => {
                        let v = (self.simd.xmm[r] >> sh) as u64;
                        self.wr(bus, s, a, 8, v)?;
                    }
                    _ => return Err(UD),
                }
            }
            0x14 | 0x15 => {
                let (a, c) = (self.simd.xmm[r], self.xmm_rm(bus, rm, 16)?);
                self.simd.xmm[r] = if x {
                    let (a, c) = (lanes(a, 8, 16), lanes(c, 8, 16));
                    let k = (op == 0x15) as usize;
                    join(&[a[k], c[k]], 8)
                } else {
                    let (a, c) = (lanes(a, 4, 16), lanes(c, 4, 16));
                    let k = if op == 0x15 { 2 } else { 0 };
                    join(&[a[k], c[k], a[k + 1], c[k + 1]], 4)
                };
            }
            0x50 => {
                let Op::Reg(i) = rm else { return Err(UD) };
                let v = self.simd.xmm[i as usize];
                let m = if x { ((v >> 63) & 1) | ((v >> 126) & 2) } else { (0..4).fold(0, |m, k| m | ((v >> (k * 32 + 31)) & 1) << k) };
                self.r[r] = m as u64;
            }
            // ---------------------------------------------------------- conversions
            0x2A => match pfx {
                Pfx::None | Pfx::P66 => {
                    let v = self.mm_rm(bus, rm)?;
                    let (lo, hi) = (v as u32 as i32, (v >> 32) as u32 as i32);
                    if x {
                        self.simd.xmm[r] = from_f64s([lo as f64, hi as f64]);
                    } else {
                        let keep = !(u64::MAX as u128);
                        self.simd.xmm[r] = (self.simd.xmm[r] & keep) | (from_f32s([lo as f32, hi as f32, 0.0, 0.0]) & u64::MAX as u128);
                    }
                }
                _ => {
                    let w = if self.rex & 8 != 0 { 8 } else { 4 };
                    let i = sx(self.get(bus, rm, w)?, w) as i64;
                    if pfx == Pfx::F3 {
                        self.simd.xmm[r] = (self.simd.xmm[r] & !0xFFFF_FFFF) | (i as f32).to_bits() as u128;
                    } else {
                        self.simd.xmm[r] = (self.simd.xmm[r] & !(u64::MAX as u128)) | (i as f64).to_bits() as u128;
                    }
                }
            },
            0x2C | 0x2D => {
                let trunc = op == 0x2C;
                let cvt = |c: &Cpu, v: f64| if trunc { v.trunc() } else { c.round_mx(v) };
                match pfx {
                    Pfx::F3 | Pfx::F2 => {
                        let v = match rm {
                            Op::Reg(i) => self.simd.xmm[i as usize],
                            Op::Mem(s, a) => self.rd(bus, s, a, if pfx == Pfx::F3 { 4 } else { 8 })? as u128,
                        };
                        let f = if pfx == Pfx::F3 { f32::from_bits(v as u32) as f64 } else { f64::from_bits(v as u64) };
                        let f = cvt(self, f);
                        let w = if self.rex & 8 != 0 { 8 } else { 4 };
                        let out = if w == 8 { f2i64(f) } else { f2i32(f) as u64 };
                        self.set_reg(reg, w, out);
                    }
                    Pfx::P66 => {
                        let v = f64s(self.xmm_rm(bus, rm, 16)?);
                        self.simd.mm[r & 7] = f2i32(cvt(self, v[0])) as u64 | (f2i32(cvt(self, v[1])) as u64) << 32;
                    }
                    Pfx::None => {
                        let v = f32s(self.xmm_rm(bus, rm, 8)?);
                        self.simd.mm[r & 7] = f2i32(cvt(self, v[0] as f64)) as u64 | (f2i32(cvt(self, v[1] as f64)) as u64) << 32;
                    }
                }
            }
            0x2E | 0x2F => {
                let (a, b) = if x {
                    let c = match rm {
                        Op::Reg(i) => self.simd.xmm[i as usize] as u64,
                        Op::Mem(s, a) => self.rd(bus, s, a, 8)?,
                    };
                    (f64::from_bits(self.simd.xmm[r] as u64), f64::from_bits(c))
                } else {
                    let c = match rm {
                        Op::Reg(i) => self.simd.xmm[i as usize] as u32,
                        Op::Mem(s, a) => self.rd(bus, s, a, 4)? as u32,
                    };
                    (f32::from_bits(self.simd.xmm[r] as u32) as f64, f32::from_bits(c) as f64)
                };
                self.eflags &= !(ZF | PF | CF | OF | SF | AF);
                if a.is_nan() || b.is_nan() {
                    self.eflags |= ZF | PF | CF;
                } else if a < b {
                    self.eflags |= CF;
                } else if a == b {
                    self.eflags |= ZF;
                }
            }
            0x5A => {
                let v = match pfx {
                    Pfx::F3 | Pfx::F2 => match rm {
                        Op::Reg(i) => self.simd.xmm[i as usize],
                        Op::Mem(s, a) => self.rd(bus, s, a, if pfx == Pfx::F3 { 4 } else { 8 })? as u128,
                    },
                    Pfx::None => self.xmm_rm(bus, rm, 8)?,
                    Pfx::P66 => self.xmm_rm(bus, rm, 16)?,
                };
                let d = self.simd.xmm[r];
                self.simd.xmm[r] = match pfx {
                    Pfx::None => {
                        let f = f32s(v);
                        from_f64s([f[0] as f64, f[1] as f64])
                    }
                    Pfx::P66 => {
                        let f = f64s(v);
                        from_f32s([f[0] as f32, f[1] as f32, 0.0, 0.0])
                    }
                    Pfx::F3 => (d & !(u64::MAX as u128)) | (f32::from_bits(v as u32) as f64).to_bits() as u128,
                    Pfx::F2 => (d & !0xFFFF_FFFF) | (f64::from_bits(v as u64) as f32).to_bits() as u128,
                };
            }
            0x5B => {
                let v = self.xmm_rm(bus, rm, 16)?;
                self.simd.xmm[r] = match pfx {
                    Pfx::None => {
                        let l = lanes(v, 4, 16);
                        from_f32s([0, 1, 2, 3].map(|k| l[k] as u32 as i32 as f32))
                    }
                    Pfx::P66 | Pfx::F3 => {
                        let f = f32s(v);
                        let t = pfx == Pfx::F3;
                        join(&f.map(|q| f2i32(if t { (q as f64).trunc() } else { self.round_mx(q as f64) }) as u64), 4)
                    }
                    Pfx::F2 => return Err(UD),
                };
            }
            0xE6 => {
                if pfx == Pfx::None {
                    return Err(UD);
                }
                self.simd.xmm[r] = if pfx == Pfx::F3 {
                    // CVTDQ2PD
                    let l = lanes(self.xmm_rm(bus, rm, 8)?, 4, 8);
                    from_f64s([l[0] as u32 as i32 as f64, l[1] as u32 as i32 as f64])
                } else {
                    // CVTTPD2DQ (66) / CVTPD2DQ (F2)
                    let f = f64s(self.xmm_rm(bus, rm, 16)?);
                    let t = pfx == Pfx::P66;
                    let c = |q: f64| f2i32(if t { q.trunc() } else { self.round_mx(q) }) as u64;
                    join(&[c(f[0]), c(f[1])], 4)
                };
            }
            // ---------------------------------------------------------- float math
            0x51..=0x59 | 0x5C..=0x5F => self.float_math(bus, op, pfx, r, rm)?,
            0xC2 => {
                let (n, dbl) = match pfx {
                    Pfx::None => (4, false),
                    Pfx::P66 => (2, true),
                    Pfx::F3 => (1, false),
                    Pfx::F2 => (1, true),
                };
                let bytes = match (n, dbl) {
                    (1, false) => 4,
                    (1, true) => 8,
                    _ => 16,
                };
                let c = self.xmm_rm(bus, rm, bytes)?;
                let p = self.fetch8(bus)?;
                let a = self.simd.xmm[r];
                let w = if dbl { 8 } else { 4 };
                let (la, lc) = (lanes(a, w, 16), lanes(c, w, 16));
                let mut out = la.clone();
                for k in 0..n {
                    let (x0, y0) = if dbl { (f64::from_bits(la[k]), f64::from_bits(lc[k])) } else { (f32::from_bits(la[k] as u32) as f64, f32::from_bits(lc[k] as u32) as f64) };
                    out[k] = if Self::cmp_pred(p, x0, y0) { mask(w) } else { 0 };
                }
                self.simd.xmm[r] = join(&out, w);
            }
            0xC6 => {
                let (a, c) = (self.simd.xmm[r], self.xmm_rm(bus, rm, 16)?);
                let i = self.fetch8(bus)? as usize;
                self.simd.xmm[r] = if x {
                    let (a, c) = (lanes(a, 8, 16), lanes(c, 8, 16));
                    join(&[a[i & 1], c[(i >> 1) & 1]], 8)
                } else {
                    let (a, c) = (lanes(a, 4, 16), lanes(c, 4, 16));
                    join(&[a[i & 3], a[(i >> 2) & 3], c[(i >> 4) & 3], c[(i >> 6) & 3]], 4)
                };
            }
            // ---------------------------------------------------------- integer moves
            0x6E => {
                let w = if self.rex & 8 != 0 { 8 } else { 4 };
                let v = self.get(bus, rm, w)?;
                if x {
                    self.simd.xmm[r] = v as u128;
                } else {
                    self.simd.mm[r & 7] = v;
                }
            }
            0x7E => {
                if pfx == Pfx::F3 {
                    // MOVQ xmm, xmm/m64
                    let v = match rm {
                        Op::Reg(i) => self.simd.xmm[i as usize] as u64,
                        Op::Mem(s, a) => self.rd(bus, s, a, 8)?,
                    };
                    self.simd.xmm[r] = v as u128;
                } else {
                    let w = if self.rex & 8 != 0 { 8 } else { 4 };
                    let v = if x { self.simd.xmm[r] as u64 } else { self.simd.mm[r & 7] } & mask(w);
                    self.set(bus, rm, w, v)?;
                }
            }
            0x6F | 0x7F => match pfx {
                Pfx::None => {
                    if op == 0x6F {
                        self.simd.mm[r & 7] = self.mm_rm(bus, rm)?;
                    } else {
                        let v = self.simd.mm[r & 7];
                        match rm {
                            Op::Reg(i) => self.simd.mm[(i & 7) as usize] = v,
                            Op::Mem(s, a) => self.wr(bus, s, a, 8, v)?,
                        }
                    }
                }
                Pfx::P66 | Pfx::F3 => self.mov128(bus, op == 0x6F, r, rm)?,
                Pfx::F2 => return Err(UD),
            },
            0xD6 => match (pfx, rm) {
                (Pfx::P66, Op::Reg(i)) => self.simd.xmm[i as usize] = self.simd.xmm[r] & u64::MAX as u128,
                (Pfx::P66, Op::Mem(s, a)) => {
                    let v = self.simd.xmm[r] as u64;
                    self.wr(bus, s, a, 8, v)?;
                }
                (Pfx::F3, Op::Reg(i)) => self.simd.xmm[r] = self.simd.mm[(i & 7) as usize] as u128,
                (Pfx::F2, Op::Reg(i)) => self.simd.mm[r & 7] = self.simd.xmm[i as usize] as u64,
                _ => return Err(UD),
            },
            0xE7 => {
                let Op::Mem(s, a) = rm else { return Err(UD) };
                if x {
                    let v = self.simd.xmm[r];
                    self.store_mem(bus, s, a, v, 16)?;
                } else {
                    let v = self.simd.mm[r & 7];
                    self.wr(bus, s, a, 8, v)?;
                }
            }
            0x70 => {
                let n = if pfx == Pfx::None { 8 } else { 16 };
                let v = if n == 8 { self.mm_rm(bus, rm)? as u128 } else { self.xmm_rm(bus, rm, 16)? };
                let i = self.fetch8(bus)? as usize;
                let pick = |l: &[u64], base: usize| [l[base + (i & 3)], l[base + ((i >> 2) & 3)], l[base + ((i >> 4) & 3)], l[base + ((i >> 6) & 3)]];
                let out = match pfx {
                    Pfx::None => join(&pick(&lanes(v, 2, 8), 0), 2),
                    Pfx::P66 => join(&pick(&lanes(v, 4, 16), 0), 4),
                    Pfx::F3 => {
                        let l = lanes(v, 2, 16);
                        let hi = pick(&l, 4);
                        join(&[l[0], l[1], l[2], l[3], hi[0], hi[1], hi[2], hi[3]], 2)
                    }
                    Pfx::F2 => {
                        let l = lanes(v, 2, 16);
                        let lo = pick(&l, 0);
                        join(&[lo[0], lo[1], lo[2], lo[3], l[4], l[5], l[6], l[7]], 2)
                    }
                };
                if n == 8 {
                    self.simd.mm[r & 7] = out as u64;
                } else {
                    self.simd.xmm[r] = out;
                }
            }
            0x71..=0x73 => {
                let Op::Reg(i) = rm else { return Err(UD) };
                let cnt = self.fetch8(bus)?;
                let n = if x { 16 } else { 8 };
                let v = if x { self.simd.xmm[i as usize] } else { self.simd.mm[(i & 7) as usize] as u128 };
                let out = match (op, reg & 7) {
                    (0x73, 3) if x => {
                        if cnt > 15 {
                            0
                        } else {
                            v >> (cnt * 8)
                        }
                    }
                    (0x73, 7) if x => {
                        if cnt > 15 {
                            0
                        } else {
                            v << (cnt * 8)
                        }
                    }
                    (_, k) => {
                        let w = [2, 4, 8][(op - 0x71) as usize];
                        let kind = match k {
                            2 => 0,
                            4 if op != 0x73 => 1,
                            6 => 2,
                            _ => return Err(UD),
                        };
                        Self::vshift(v, cnt, w, n, kind)
                    }
                };
                if x {
                    self.simd.xmm[i as usize] = out;
                } else {
                    self.simd.mm[(i & 7) as usize] = out as u64;
                }
            }
            0xC4 => {
                let v = self.get(bus, rm, 2)? as u128;
                let n = if x { 8 } else { 4 };
                let i = (self.fetch8(bus)? % n) as u32 * 16;
                if x {
                    self.simd.xmm[r] = (self.simd.xmm[r] & !(0xFFFFu128 << i)) | v << i;
                } else {
                    let m = &mut self.simd.mm[r & 7];
                    *m = (*m & !(0xFFFF << i)) | (v as u64) << i;
                }
            }
            0xC5 => {
                let Op::Reg(src) = rm else { return Err(UD) };
                let n = if x { 8 } else { 4 };
                let i = (self.fetch8(bus)? % n) as u32 * 16;
                let v = if x { self.simd.xmm[src as usize] } else { self.simd.mm[(src & 7) as usize] as u128 };
                self.r[r] = ((v >> i) & 0xFFFF) as u64;
            }
            0xD7 => {
                let Op::Reg(src) = rm else { return Err(UD) };
                let (v, n) = if x { (self.simd.xmm[src as usize], 16) } else { (self.simd.mm[(src & 7) as usize] as u128, 8) };
                self.r[r] = (0..n).fold(0u64, |m, k| m | (((v >> (k * 8 + 7)) & 1) as u64) << k);
            }
            0xF7 => {
                // MASKMOVQ / MASKMOVDQU: store bytes selected by the mask register to DS:rDI.
                let Op::Reg(mi) = rm else { return Err(UD) };
                let (v, m, n) = if x { (self.simd.xmm[r], self.simd.xmm[mi as usize], 16) } else { (self.simd.mm[r & 7] as u128, self.simd.mm[(mi & 7) as usize] as u128, 8) };
                let s = self.ds(DS);
                let di = self.reg(EDI as u8, self.asz);
                for k in 0..n {
                    if (m >> (k * 8 + 7)) & 1 != 0 {
                        self.wr(bus, s, di.wrapping_add(k as u64), 1, ((v >> (k * 8)) & 0xFF) as u64)?;
                    }
                }
            }
            // ---------------------------------------------------------- integer arithmetic
            _ => {
                if matches!(pfx, Pfx::F3 | Pfx::F2) {
                    return Err(UD);
                }
                if x {
                    let a = self.simd.xmm[r];
                    let c = self.xmm_rm(bus, rm, 16)?;
                    self.simd.xmm[r] = Self::int_op(op, a, c, 16).ok_or(UD)?;
                } else {
                    let a = self.simd.mm[r & 7] as u128;
                    let c = self.mm_rm(bus, rm)? as u128;
                    self.simd.mm[r & 7] = Self::int_op(op, a, c, 8).ok_or(UD)? as u64;
                }
            }
        }
        Ok(())
    }

    fn mov128<B: Bus>(&mut self, bus: &mut B, load: bool, r: usize, rm: Op) -> R<()> {
        if load {
            self.simd.xmm[r] = self.xmm_rm(bus, rm, 16)?;
        } else {
            let v = self.simd.xmm[r];
            match rm {
                Op::Reg(i) => self.simd.xmm[i as usize] = v,
                Op::Mem(s, a) => self.store_mem(bus, s, a, v, 16)?,
            }
        }
        Ok(())
    }

    fn float_math<B: Bus>(&mut self, bus: &mut B, op: u8, pfx: Pfx, r: usize, rm: Op) -> R<()> {
        if (0x54..=0x57).contains(&op) {
            if matches!(pfx, Pfx::F3 | Pfx::F2) {
                return Err(UD);
            }
            let (a, c) = (self.simd.xmm[r], self.xmm_rm(bus, rm, 16)?);
            self.simd.xmm[r] = match op {
                0x54 => a & c,
                0x55 => !a & c,
                0x56 => a | c,
                _ => a ^ c,
            };
            return Ok(());
        }
        let a = self.simd.xmm[r];
        match pfx {
            Pfx::None => {
                let c = self.xmm_rm(bus, rm, 16)?;
                let (la, lc) = (lanes(a, 4, 16), lanes(c, 4, 16));
                let out: Vec<u64> = (0..4).map(|k| Self::f32_op(op, la[k] as u32, lc[k] as u32) as u64).collect();
                self.simd.xmm[r] = join(&out, 4);
            }
            Pfx::F3 => {
                let c = match rm {
                    Op::Reg(i) => self.simd.xmm[i as usize] as u32,
                    Op::Mem(s, ad) => self.rd(bus, s, ad, 4)? as u32,
                };
                self.simd.xmm[r] = (a & !0xFFFF_FFFF) | Self::f32_op(op, a as u32, c) as u128;
            }
            Pfx::P66 => {
                if matches!(op, 0x52 | 0x53) {
                    return Err(UD);
                }
                let c = self.xmm_rm(bus, rm, 16)?;
                let (la, lc) = (lanes(a, 8, 16), lanes(c, 8, 16));
                self.simd.xmm[r] = join(&[Self::f64_op(op, la[0], lc[0]), Self::f64_op(op, la[1], lc[1])], 8);
            }
            Pfx::F2 => {
                if matches!(op, 0x52 | 0x53) {
                    return Err(UD);
                }
                let c = match rm {
                    Op::Reg(i) => self.simd.xmm[i as usize] as u64,
                    Op::Mem(s, ad) => self.rd(bus, s, ad, 8)?,
                };
                self.simd.xmm[r] = (a & !(u64::MAX as u128)) | Self::f64_op(op, a as u64, c) as u128;
            }
        }
        Ok(())
    }

    /// Lane shifts: kind 0 = logical right, 1 = arithmetic right, 2 = left.
    fn vshift(v: u128, cnt: u64, w: u32, n: u32, kind: u8) -> u128 {
        let bits = (w * 8) as u64;
        let l: Vec<u64> = lanes(v, w, n)
            .into_iter()
            .map(|x| match kind {
                0 if cnt >= bits => 0,
                0 => x >> cnt,
                1 => (sext(x, w) >> cnt.min(bits - 1)) as u64,
                _ if cnt >= bits => 0,
                _ => x << cnt,
            })
            .collect();
        join(&l, w)
    }

    /// Integer SIMD over `n` bytes (8 for MMX, 16 for XMM).
    fn int_op(op: u8, a: u128, c: u128, n: u32) -> Option<u128> {
        let map2 = |w: u32, fun: &dyn Fn(u64, u64) -> u64| -> u128 {
            let (la, lc) = (lanes(a, w, n), lanes(c, w, n));
            join(&la.iter().zip(&lc).map(|(&x, &y)| fun(x, y)).collect::<Vec<_>>(), w)
        };
        let pack = |from: u32, signed: bool| -> u128 {
            let to = from / 2;
            let v: Vec<u64> = lanes(a, from, n).into_iter().chain(lanes(c, from, n)).map(|x| if signed { sat_s(sext(x, from), to) } else { sat_u(sext(x, from), to) }).collect();
            join(&v, to)
        };
        let unpack = |w: u32, high: bool| -> u128 {
            let (la, lc) = (lanes(a, w, n), lanes(c, w, n));
            let half = la.len() / 2;
            let off = if high { half } else { 0 };
            let v: Vec<u64> = (0..half).flat_map(|i| [la[off + i], lc[off + i]]).collect();
            join(&v, w)
        };
        // Shift by the low 64 bits of the source operand.
        let shift = |w: u32, kind: u8| Self::vshift(a, c as u64, w, n, kind);
        let full: u128 = if n == 8 { u64::MAX as u128 } else { u128::MAX };
        Some(match op {
            0x60 => unpack(1, false),
            0x61 => unpack(2, false),
            0x62 => unpack(4, false),
            0x63 => pack(2, true),
            0x64 => map2(1, &|x, y| if sext(x, 1) > sext(y, 1) { 0xFF } else { 0 }),
            0x65 => map2(2, &|x, y| if sext(x, 2) > sext(y, 2) { 0xFFFF } else { 0 }),
            0x66 => map2(4, &|x, y| if sext(x, 4) > sext(y, 4) { 0xFFFF_FFFF } else { 0 }),
            0x67 => pack(2, false),
            0x68 => unpack(1, true),
            0x69 => unpack(2, true),
            0x6A => unpack(4, true),
            0x6B => pack(4, true),
            0x6C if n == 16 => unpack(8, false),
            0x6D if n == 16 => unpack(8, true),
            0x74 => map2(1, &|x, y| if x == y { 0xFF } else { 0 }),
            0x75 => map2(2, &|x, y| if x == y { 0xFFFF } else { 0 }),
            0x76 => map2(4, &|x, y| if x == y { 0xFFFF_FFFF } else { 0 }),
            0xD1 => shift(2, 0),
            0xD2 => shift(4, 0),
            0xD3 => shift(8, 0),
            0xD4 => map2(8, &|x, y| x.wrapping_add(y)),
            0xD5 => map2(2, &|x, y| (sext(x, 2) * sext(y, 2)) as u64),
            0xD8 => map2(1, &|x, y| x.saturating_sub(y)),
            0xD9 => map2(2, &|x, y| x.saturating_sub(y)),
            0xDA => map2(1, &|x, y| x.min(y)),
            0xDB => a & c,
            0xDC => map2(1, &|x, y| (x + y).min(0xFF)),
            0xDD => map2(2, &|x, y| (x + y).min(0xFFFF)),
            0xDE => map2(1, &|x, y| x.max(y)),
            0xDF => !a & c & full,
            0xE0 => map2(1, &|x, y| (x + y + 1) >> 1),
            0xE1 => shift(2, 1),
            0xE2 => shift(4, 1),
            0xE3 => map2(2, &|x, y| (x + y + 1) >> 1),
            0xE4 => map2(2, &|x, y| (x * y) >> 16),
            0xE5 => map2(2, &|x, y| ((sext(x, 2) * sext(y, 2)) >> 16) as u64),
            0xE8 => map2(1, &|x, y| sat_s(sext(x, 1) - sext(y, 1), 1)),
            0xE9 => map2(2, &|x, y| sat_s(sext(x, 2) - sext(y, 2), 2)),
            0xEA => map2(2, &|x, y| if sext(x, 2) < sext(y, 2) { x } else { y }),
            0xEB => a | c,
            0xEC => map2(1, &|x, y| sat_s(sext(x, 1) + sext(y, 1), 1)),
            0xED => map2(2, &|x, y| sat_s(sext(x, 2) + sext(y, 2), 2)),
            0xEE => map2(2, &|x, y| if sext(x, 2) > sext(y, 2) { x } else { y }),
            0xEF => a ^ c,
            0xF1 => shift(2, 2),
            0xF2 => shift(4, 2),
            0xF3 => shift(8, 2),
            0xF4 => {
                // PMULUDQ: low dwords of each qword.
                let (la, lc) = (lanes(a, 8, n), lanes(c, 8, n));
                join(&la.iter().zip(&lc).map(|(&x, &y)| (x & 0xFFFF_FFFF) * (y & 0xFFFF_FFFF)).collect::<Vec<_>>(), 8)
            }
            0xF5 => {
                let (la, lc) = (lanes(a, 2, n), lanes(c, 2, n));
                let p: Vec<i64> = (0..la.len()).map(|i| sext(la[i], 2) * sext(lc[i], 2)).collect();
                join(&p.chunks(2).map(|q| (q[0] + q[1]) as u64).collect::<Vec<_>>(), 4)
            }
            0xF6 => {
                let (la, lc) = (lanes(a, 1, n), lanes(c, 1, n));
                let sums: Vec<u64> = la.chunks(8).zip(lc.chunks(8)).map(|(x, y)| x.iter().zip(y).map(|(&p, &q)| p.abs_diff(q)).sum()).collect();
                join(&sums, 8)
            }
            0xF8 => map2(1, &|x, y| x.wrapping_sub(y)),
            0xF9 => map2(2, &|x, y| x.wrapping_sub(y)),
            0xFA => map2(4, &|x, y| x.wrapping_sub(y)),
            0xFB => map2(8, &|x, y| x.wrapping_sub(y)),
            0xFC => map2(1, &|x, y| x + y),
            0xFD => map2(2, &|x, y| x + y),
            0xFE => map2(4, &|x, y| x + y),
            _ => return None,
        })
    }

    /// 0F AE: FXSAVE, FXRSTOR, LDMXCSR, STMXCSR, the fences and CLFLUSH.
    fn sse_ae<B: Bus>(&mut self, bus: &mut B) -> R<()> {
        let (reg, rm) = self.modrm(bus)?;
        let Op::Mem(s, a) = rm else {
            return if reg & 7 >= 5 { Ok(()) } else { Err(UD) }; // LFENCE / MFENCE / SFENCE
        };
        match reg & 7 {
            0 => {
                let tw = self.fpu.tag_word();
                // Abridged tag: one bit per register, 1 = valid.
                let ftw = (0..8).fold(0u64, |t, p| t | (((tw >> (p * 2)) & 3 != 3) as u64) << p);
                self.wr(bus, s, a, 2, self.fpu.cw as u64)?;
                let sw = self.fpu.status() as u64;
                self.wr(bus, s, a.wrapping_add(2), 2, sw)?;
                self.wr(bus, s, a.wrapping_add(4), 2, ftw)?;
                self.wr(bus, s, a.wrapping_add(6), 2, 0)?;
                self.wr(bus, s, a.wrapping_add(8), 8, 0)?;
                self.wr(bus, s, a.wrapping_add(16), 8, 0)?;
                self.wr(bus, s, a.wrapping_add(24), 4, self.simd.mxcsr as u64)?;
                self.wr(bus, s, a.wrapping_add(28), 4, 0xFFFF)?;
                for i in 0..8u8 {
                    let bytes = fpu::to_f80(self.fpu.get(i));
                    let mut v = [0u8; 16];
                    v[..10].copy_from_slice(&bytes);
                    self.store_mem(bus, s, a.wrapping_add(32 + i as u64 * 16), u128::from_le_bytes(v), 16)?;
                }
                let n = if self.code64 { 16 } else { 8 };
                for i in 0..n {
                    let v = self.simd.xmm[i];
                    self.store_mem(bus, s, a.wrapping_add(160 + i as u64 * 16), v, 16)?;
                }
            }
            1 => {
                self.fpu.cw = self.rd(bus, s, a, 2)? as u16;
                let sw = self.rd(bus, s, a.wrapping_add(2), 2)? as u16;
                self.fpu.top = ((sw >> 11) & 7) as u8;
                self.fpu.sw = sw & !0x3800;
                let ftw = self.rd(bus, s, a.wrapping_add(4), 1)?;
                self.simd.mxcsr = self.rd(bus, s, a.wrapping_add(24), 4)? as u32 & 0xFFFF;
                for i in 0..8u8 {
                    let v = self.xmm_rm(bus, Op::Mem(s, a.wrapping_add(32 + i as u64 * 16)), 16)?;
                    let bytes: [u8; 10] = v.to_le_bytes()[..10].try_into().unwrap();
                    let p = self.fpu.phys(i);
                    self.fpu.st[p] = fpu::from_f80(&bytes);
                }
                self.fpu.empty = !(ftw as u8);
                let n = if self.code64 { 16 } else { 8 };
                for i in 0..n {
                    self.simd.xmm[i] = self.xmm_rm(bus, Op::Mem(s, a.wrapping_add(160 + i as u64 * 16)), 16)?;
                }
            }
            2 => self.simd.mxcsr = self.rd(bus, s, a, 4)? as u32 & 0xFFFF,
            3 => {
                let v = self.simd.mxcsr as u64;
                self.wr(bus, s, a, 4, v)?;
            }
            7 => {} // CLFLUSH
            _ => return Err(UD),
        }
        Ok(())
    }
}
