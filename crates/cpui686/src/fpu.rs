//! x87 floating-point unit. Registers are kept as f64 (the 80-bit format is converted on
//! load/store), which is what games compiled for the Xbox expect in practice.

use super::*;

#[derive(Clone, Debug)]
pub struct Fpu {
    pub st: [f64; 8],
    /// Bit i set = physical register i is empty.
    pub empty: u8,
    pub top: u8,
    pub cw: u16,
    /// Status word without TOP (C0-C3, exception flags).
    pub sw: u16,
}

impl Default for Fpu {
    fn default() -> Self {
        Fpu { st: [0.0; 8], empty: 0xFF, top: 0, cw: 0x037F, sw: 0 }
    }
}

const C0: u16 = 1 << 8;
const C1: u16 = 1 << 9;
const C2: u16 = 1 << 10;
const C3: u16 = 1 << 14;

/// x * 2^e without intermediate overflow.
pub fn ldexp(mut x: f64, mut e: i32) -> f64 {
    while e > 1000 {
        x *= 2f64.powi(1000);
        e -= 1000;
    }
    while e < -1000 {
        x *= 2f64.powi(-1000);
        e += 1000;
    }
    x * 2f64.powi(e)
}

/// 80-bit extended precision -> f64.
pub fn from_f80(b: &[u8; 10]) -> f64 {
    let mant = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let se = u16::from_le_bytes([b[8], b[9]]);
    let (neg, exp) = (se & 0x8000 != 0, (se & 0x7FFF) as i32);
    let v = if exp == 0x7FFF {
        if mant << 1 == 0 {
            f64::INFINITY
        } else {
            f64::NAN
        }
    } else if exp == 0 && mant == 0 {
        0.0
    } else {
        ldexp(mant as f64, exp - 16383 - 63)
    };
    if neg {
        -v
    } else {
        v
    }
}

/// f64 -> 80-bit extended precision.
pub fn to_f80(v: f64) -> [u8; 10] {
    let bits = v.to_bits();
    let sign = ((bits >> 63) as u16) << 15;
    let exp = ((bits >> 52) & 0x7FF) as i32;
    let frac = bits & ((1 << 52) - 1);
    let (e, m): (u16, u64) = if exp == 0x7FF {
        (0x7FFF, 1 << 63 | frac << 11)
    } else if exp == 0 && frac == 0 {
        (0, 0)
    } else if exp == 0 {
        // Subnormal double: normalise.
        let shift = frac.leading_zeros() - 11;
        ((1 - 1023 + 16383 - shift as i32) as u16, (frac << shift) << 11)
    } else {
        ((exp - 1023 + 16383) as u16, 1 << 63 | frac << 11)
    };
    let mut out = [0u8; 10];
    out[0..8].copy_from_slice(&m.to_le_bytes());
    out[8..10].copy_from_slice(&(e | sign).to_le_bytes());
    out
}

impl Fpu {
    pub fn status(&self) -> u16 {
        (self.sw & !0x3800) | (self.top as u16) << 11
    }
    pub(crate) fn phys(&self, i: u8) -> usize {
        ((self.top + i) & 7) as usize
    }
    pub fn get(&self, i: u8) -> f64 {
        self.st[self.phys(i)]
    }
    pub fn set(&mut self, i: u8, v: f64) {
        let p = self.phys(i);
        self.st[p] = v;
        self.empty &= !(1 << p);
    }
    pub fn push(&mut self, v: f64) {
        self.top = self.top.wrapping_sub(1) & 7;
        self.set(0, v);
    }
    pub fn pop(&mut self) -> f64 {
        let v = self.get(0);
        self.empty |= 1 << self.phys(0);
        self.top = (self.top + 1) & 7;
        v
    }
    /// Full 16-bit tag word (00 valid, 01 zero, 10 special, 11 empty).
    pub fn tag_word(&self) -> u16 {
        let mut t = 0;
        for p in 0..8 {
            let tag = if self.empty & (1 << p) != 0 {
                3
            } else if self.st[p] == 0.0 {
                1
            } else if !self.st[p].is_finite() {
                2
            } else {
                0
            };
            t |= tag << (p * 2);
        }
        t
    }
    pub fn set_tag_word(&mut self, t: u16) {
        self.empty = (0..8).filter(|p| (t >> (p * 2)) & 3 == 3).fold(0, |e, p| e | 1 << p);
    }

    fn round(&self, v: f64) -> f64 {
        match (self.cw >> 10) & 3 {
            0 => {
                let r = v.round();
                // Ties to even.
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

    /// Condition codes for a compare (C3 C2 C0).
    fn compare(&mut self, a: f64, b: f64) {
        self.sw &= !(C0 | C2 | C3);
        if a.is_nan() || b.is_nan() {
            self.sw |= C0 | C2 | C3;
        } else if a < b {
            self.sw |= C0;
        } else if a == b {
            self.sw |= C3;
        }
    }
}

fn arith(op: u8, a: f64, b: f64) -> f64 {
    match op {
        0 => a + b,
        1 => a * b,
        4 => a - b,
        5 => b - a,
        6 => a / b,
        7 => b / a,
        _ => a,
    }
}

impl Cpu {
    fn fcomi(&mut self, a: f64, b: f64) {
        self.eflags &= !(ZF | PF | CF);
        if a.is_nan() || b.is_nan() {
            self.eflags |= ZF | PF | CF;
        } else if a < b {
            self.eflags |= CF;
        } else if a == b {
            self.eflags |= ZF;
        }
    }

    fn fist(&mut self, v: f64, bytes: u32) -> u64 {
        let r = self.fpu.round(v);
        let (lo, hi) = match bytes {
            2 => (i16::MIN as f64, i16::MAX as f64),
            4 => (i32::MIN as f64, i32::MAX as f64),
            _ => (i64::MIN as f64, i64::MAX as f64),
        };
        if r.is_nan() || r < lo || r > hi {
            self.fpu.sw |= 1; // invalid operation
            return 1 << (bytes * 8 - 1);
        }
        r as i64 as u64
    }

    fn read_f80<B: Bus>(&mut self, bus: &mut B, s: usize, a: u64) -> R<f64> {
        let mut b = [0u8; 10];
        for (i, x) in b.iter_mut().enumerate() {
            *x = self.rd32(bus, s, a.wrapping_add(i as u64), 1)? as u8;
        }
        Ok(from_f80(&b))
    }
    fn write_f80<B: Bus>(&mut self, bus: &mut B, s: usize, a: u64, v: f64) -> R<()> {
        for (i, x) in to_f80(v).iter().enumerate() {
            self.wr32(bus, s, a.wrapping_add(i as u64), 1, *x as u32)?;
        }
        Ok(())
    }
    fn read_f64<B: Bus>(&mut self, bus: &mut B, s: usize, a: u64) -> R<f64> {
        let lo = self.rd32(bus, s, a, 4)? as u64;
        let hi = self.rd32(bus, s, a.wrapping_add(4), 4)? as u64;
        Ok(f64::from_bits(lo | hi << 32))
    }
    fn write_u64<B: Bus>(&mut self, bus: &mut B, s: usize, a: u64, v: u64) -> R<()> {
        self.wr32(bus, s, a, 4, v as u32)?;
        self.wr32(bus, s, a.wrapping_add(4), 4, (v >> 32) as u32)
    }

    /// FNSTENV / FLDENV layout (28 bytes in 32-bit mode, 14 in 16-bit).
    fn store_env<B: Bus>(&mut self, bus: &mut B, s: usize, a: u64) -> R<u32> {
        let (w, n) = if self.osz == 4 { (4, 28) } else { (2, 14) };
        let vals = [self.fpu.cw as u32 | 0xFFFF_0000, self.fpu.status() as u32 | 0xFFFF_0000, self.fpu.tag_word() as u32 | 0xFFFF_0000, 0, 0, 0, 0];
        for (i, v) in vals.iter().enumerate() {
            self.wr32(bus, s, a.wrapping_add(i as u64 * w as u64), w, *v)?;
        }
        Ok(n)
    }
    fn load_env<B: Bus>(&mut self, bus: &mut B, s: usize, a: u64) -> R<u32> {
        let (w, n) = if self.osz == 4 { (4, 28) } else { (2, 14) };
        self.fpu.cw = self.rd32(bus, s, a, 2)? as u16;
        let sw = self.rd32(bus, s, a.wrapping_add(w as u64), 2)? as u16;
        self.fpu.top = ((sw >> 11) & 7) as u8;
        self.fpu.sw = sw & !0x3800;
        let tw = self.rd32(bus, s, a.wrapping_add(2 * w as u64), 2)? as u16;
        self.fpu.set_tag_word(tw);
        Ok(n)
    }

    /// Executes an x87 instruction (escape opcodes D8-DF).
    pub(crate) fn fpu_exec<B: Bus>(&mut self, bus: &mut B, op: u32) -> R<()> {
        if self.cr0 & 0xC != 0 {
            return Err(Ex(7, None)); // #NM: FPU disabled or task switched
        }
        let (reg, rm) = self.modrm(bus)?;
        let esc = (op & 7) as u8;
        match rm {
            Op::Mem(s, a) => self.fpu_mem(bus, esc, reg, s, a),
            Op::Reg(i) => self.fpu_reg(esc, reg, i),
        }
    }

    fn fpu_mem<B: Bus>(&mut self, bus: &mut B, esc: u8, reg: u8, s: usize, a: u64) -> R<()> {
        match (esc, reg) {
            // Arithmetic with a memory operand: D8 m32real, DA m32int, DC m64real, DE m16int.
            (0 | 2 | 4 | 6, _) => {
                let v = match esc {
                    0 => f32::from_bits(self.rd32(bus, s, a, 4)?) as f64,
                    2 => self.rd32(bus, s, a, 4)? as i32 as f64,
                    4 => self.read_f64(bus, s, a)?,
                    _ => self.rd32(bus, s, a, 2)? as u16 as i16 as f64,
                };
                let st0 = self.fpu.get(0);
                match reg {
                    2 => self.fpu.compare(st0, v),
                    3 => {
                        self.fpu.compare(st0, v);
                        self.fpu.pop();
                    }
                    _ => self.fpu.set(0, arith(reg, st0, v)),
                }
            }
            (1, 0) => {
                let v = f32::from_bits(self.rd32(bus, s, a, 4)?) as f64;
                self.fpu.push(v);
            }
            (1, 2 | 3) => {
                let v = (self.fpu.get(0) as f32).to_bits();
                self.wr32(bus, s, a, 4, v)?;
                if reg == 3 {
                    self.fpu.pop();
                }
            }
            (1, 4) => {
                self.load_env(bus, s, a)?;
            }
            (1, 5) => self.fpu.cw = self.rd32(bus, s, a, 2)? as u16,
            (1, 6) => {
                self.store_env(bus, s, a)?;
                self.fpu.cw |= 0x3F; // FNSTENV masks all exceptions
            }
            (1, 7) => {
                let cw = self.fpu.cw as u32;
                self.wr32(bus, s, a, 2, cw)?;
            }
            (3, 0) => {
                let v = self.rd32(bus, s, a, 4)? as i32 as f64;
                self.fpu.push(v);
            }
            (3, 1) | (7, 1) | (5, 1) => {
                // FISTTP (truncating store, SSE3) -- kept for completeness.
                let v = self.fpu.pop().trunc();
                match esc {
                    3 => self.wr32(bus, s, a, 4, v as i32 as u32)?,
                    7 => self.wr32(bus, s, a, 2, v as i16 as u32)?,
                    _ => self.write_u64(bus, s, a, v as i64 as u64)?,
                }
            }
            (3, 2 | 3) => {
                let v = self.fpu.get(0);
                let r = self.fist(v, 4);
                self.wr32(bus, s, a, 4, r as u32)?;
                if reg == 3 {
                    self.fpu.pop();
                }
            }
            (3, 5) => {
                let v = self.read_f80(bus, s, a)?;
                self.fpu.push(v);
            }
            (3, 7) => {
                let v = self.fpu.pop();
                self.write_f80(bus, s, a, v)?;
            }
            (5, 0) => {
                let v = self.read_f64(bus, s, a)?;
                self.fpu.push(v);
            }
            (5, 2 | 3) => {
                let v = self.fpu.get(0).to_bits();
                self.write_u64(bus, s, a, v)?;
                if reg == 3 {
                    self.fpu.pop();
                }
            }
            (5, 4) => {
                // FRSTOR: environment then ST0..ST7.
                let n = self.load_env(bus, s, a)?;
                let empty = self.fpu.empty;
                for i in 0..8u8 {
                    let v = self.read_f80(bus, s, a.wrapping_add(n as u64 + i as u64 * 10))?;
                    let p = self.fpu.phys(i);
                    self.fpu.st[p] = v;
                }
                self.fpu.empty = empty;
            }
            (5, 6) => {
                // FNSAVE, then FNINIT.
                let n = self.store_env(bus, s, a)?;
                for i in 0..8u8 {
                    let v = self.fpu.get(i);
                    self.write_f80(bus, s, a.wrapping_add(n as u64 + i as u64 * 10), v)?;
                }
                self.fpu = Fpu::default();
            }
            (5, 7) => {
                let v = self.fpu.status() as u32;
                self.wr32(bus, s, a, 2, v)?;
            }
            (7, 0) => {
                let v = self.rd32(bus, s, a, 2)? as u16 as i16 as f64;
                self.fpu.push(v);
            }
            (7, 2 | 3) => {
                let v = self.fpu.get(0);
                let r = self.fist(v, 2);
                self.wr32(bus, s, a, 2, r as u32)?;
                if reg == 3 {
                    self.fpu.pop();
                }
            }
            (7, 4) => {
                // FBLD: 18 packed BCD digits + sign.
                let mut v = 0f64;
                for i in (0..9).rev() {
                    let b = self.rd32(bus, s, a.wrapping_add(i), 1)?;
                    v = v * 100.0 + ((b >> 4) * 10 + (b & 0xF)) as f64;
                }
                let neg = self.rd32(bus, s, a.wrapping_add(9), 1)? & 0x80 != 0;
                self.fpu.push(if neg { -v } else { v });
            }
            (7, 5) => {
                let lo = self.rd32(bus, s, a, 4)? as u64;
                let hi = self.rd32(bus, s, a.wrapping_add(4), 4)? as u64;
                self.fpu.push((lo | hi << 32) as i64 as f64);
            }
            (7, 6) => {
                let v = self.fpu.pop();
                let mut n = self.fpu.round(v).abs() as u64;
                for i in 0..9 {
                    let b = (n % 10) | ((n / 10) % 10) << 4;
                    n /= 100;
                    self.wr32(bus, s, a.wrapping_add(i), 1, b as u32)?;
                }
                self.wr32(bus, s, a.wrapping_add(9), 1, if v < 0.0 { 0x80 } else { 0 })?;
            }
            (7, 7) => {
                let v = self.fpu.get(0);
                let r = self.fist(v, 8);
                self.write_u64(bus, s, a, r)?;
                self.fpu.pop();
            }
            _ => return Err(UD),
        }
        Ok(())
    }

    fn fpu_reg(&mut self, esc: u8, reg: u8, i: u8) -> R<()> {
        let f = &mut self.fpu;
        match (esc, reg) {
            (0, 2 | 3) | (4, 2 | 3) => {
                let (a, b) = (f.get(0), f.get(i));
                f.compare(a, b);
                if reg == 3 {
                    f.pop();
                }
            }
            (0, _) => {
                let v = arith(reg, f.get(0), f.get(i));
                f.set(0, v);
            }
            (6, 3) if i == 1 => {
                let (a, b) = (f.get(0), f.get(1));
                f.compare(a, b);
                f.pop();
                f.pop();
            }
            // DC / DE: destination ST(i); the subtract/divide directions swap.
            (4 | 6, _) => {
                let op = if reg >= 4 { reg ^ 1 } else { reg };
                let v = arith(op, f.get(i), f.get(0));
                f.set(i, v);
                if esc == 6 {
                    f.pop();
                }
            }
            (1, 0) => {
                let v = f.get(i);
                f.push(v);
            }
            (1, 1) => {
                let (a, b) = (f.get(0), f.get(i));
                f.set(0, b);
                f.set(i, a);
            }
            (1, 2) => {} // FNOP
            (1, 3) => {
                let v = f.get(0);
                f.set(i, v);
                f.pop();
            }
            (1, 4) => match i {
                0 => {
                    let v = -f.get(0);
                    f.set(0, v)
                }
                1 => {
                    let v = f.get(0).abs();
                    f.set(0, v)
                }
                4 => {
                    let v = f.get(0);
                    f.compare(v, 0.0)
                }
                5 => {
                    // FXAM
                    let v = f.get(0);
                    f.sw &= !(C0 | C1 | C2 | C3);
                    if v.is_sign_negative() {
                        f.sw |= C1;
                    }
                    f.sw |= if f.empty & (1 << f.phys(0)) != 0 {
                        C3 | C0
                    } else if v.is_nan() {
                        C0
                    } else if v.is_infinite() {
                        C2 | C0
                    } else if v == 0.0 {
                        C3
                    } else if v.is_subnormal() {
                        C3 | C2
                    } else {
                        C2
                    };
                }
                _ => return Err(UD),
            },
            (1, 5) => {
                let v = match i {
                    0 => 1.0,
                    1 => std::f64::consts::LOG2_10,
                    2 => std::f64::consts::LOG2_E,
                    3 => std::f64::consts::PI,
                    4 => std::f64::consts::LOG10_2,
                    5 => std::f64::consts::LN_2,
                    6 => 0.0,
                    _ => return Err(UD),
                };
                f.push(v);
            }
            (1, 6) => {
                let x = f.get(0);
                match i {
                    0 => f.set(0, x.exp2() - 1.0),
                    1 => {
                        let y = f.get(1);
                        f.set(1, y * x.log2());
                        f.pop();
                    }
                    2 => {
                        f.set(0, x.tan());
                        f.push(1.0);
                        f.sw &= !C2;
                    }
                    3 => {
                        let y = f.get(1);
                        f.set(1, y.atan2(x));
                        f.pop();
                    }
                    4 => {
                        // FXTRACT: exponent then significand.
                        if x == 0.0 {
                            f.set(0, f64::NEG_INFINITY);
                            f.push(x);
                        } else {
                            let e = x.abs().log2().floor();
                            f.set(0, e);
                            f.push(x / 2f64.powf(e));
                        }
                    }
                    5 => {
                        let y = f.get(1);
                        f.set(0, x - (x / y).round() * y);
                        f.sw &= !C2;
                    }
                    6 => f.top = f.top.wrapping_sub(1) & 7,
                    _ => f.top = (f.top + 1) & 7,
                }
            }
            (1, 7) => {
                let x = f.get(0);
                match i {
                    0 => {
                        // FPREM: truncating partial remainder; low quotient bits in C0/C3/C1.
                        let y = f.get(1);
                        let q = (x / y).trunc();
                        f.set(0, x - q * y);
                        let qi = q.abs() as u64;
                        f.sw &= !(C0 | C1 | C2 | C3);
                        if qi & 1 != 0 {
                            f.sw |= C1;
                        }
                        if qi & 2 != 0 {
                            f.sw |= C3;
                        }
                        if qi & 4 != 0 {
                            f.sw |= C0;
                        }
                    }
                    1 => {
                        let y = f.get(1);
                        f.set(1, y * (x + 1.0).log2());
                        f.pop();
                    }
                    2 => f.set(0, x.sqrt()),
                    3 => {
                        f.set(0, x.sin());
                        f.push(x.cos());
                        f.sw &= !C2;
                    }
                    4 => {
                        let r = f.round(x);
                        f.set(0, r)
                    }
                    5 => {
                        let y = f.get(1).trunc();
                        f.set(0, ldexp(x, y.clamp(-40000.0, 40000.0) as i32));
                    }
                    6 => {
                        f.set(0, x.sin());
                        f.sw &= !C2;
                    }
                    _ => {
                        f.set(0, x.cos());
                        f.sw &= !C2;
                    }
                }
            }
            (2, 0..=3) | (3, 0..=3) => {
                // FCMOVcc: B E BE U (DA) and their negations (DB).
                let fl = self.eflags;
                let c = match reg {
                    0 => fl & CF != 0,
                    1 => fl & ZF != 0,
                    2 => fl & (CF | ZF) != 0,
                    _ => fl & PF != 0,
                };
                if c == (esc == 2) {
                    let v = self.fpu.get(i);
                    self.fpu.set(0, v);
                }
            }
            (2, 5) if i == 1 => {
                let (a, b) = (f.get(0), f.get(1));
                f.compare(a, b);
                f.pop();
                f.pop();
            }
            (3, 4) => match i {
                0 | 1 | 4 => {} // FENI / FDISI / FSETPM (no-ops on a 387+)
                2 => f.sw &= !0x80FF,
                3 => *f = Fpu::default(),
                _ => return Err(UD),
            },
            (3, 5 | 6) | (7, 5 | 6) => {
                let (a, b) = (f.get(0), f.get(i));
                self.fcomi(a, b);
                if esc == 7 {
                    self.fpu.pop();
                }
            }
            (5, 0) => {
                let p = f.phys(i);
                f.empty |= 1 << p;
            }
            (5, 2) => {
                let v = f.get(0);
                f.set(i, v);
            }
            (5, 3) => {
                let v = f.get(0);
                f.set(i, v);
                f.pop();
            }
            (5, 4 | 5) => {
                let (a, b) = (f.get(0), f.get(i));
                f.compare(a, b);
                if reg == 5 {
                    f.pop();
                }
            }
            (7, 4) if i == 0 => {
                let v = f.status() as u32;
                self.set_reg(0, 2, v as u64);
            }
            (7, 0) => {
                // FFREEP
                let p = f.phys(i);
                f.empty |= 1 << p;
                f.pop();
            }
            _ => return Err(UD),
        }
        Ok(())
    }
}
