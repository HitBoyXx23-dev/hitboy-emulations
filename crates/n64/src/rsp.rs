//! Reality Signal Processor: a 32-bit MIPS scalar core running from 4 KiB IMEM with 4 KiB
//! DMEM, plus the COP2 vector unit (32 x 8 lanes of 16 bits, 48-bit accumulators, VCO/VCC/VCE
//! flags and the reciprocal / inverse-square-root unit). Games upload microcode to it for
//! 3D transforms, lighting and audio mixing.

use crate::Hw;

/// Reciprocal and inverse-square-root lookup tables, generated from their defining formulas.
struct Tables {
    rcp: [u16; 512],
    rsq: [u16; 512],
}

fn tables() -> &'static Tables {
    use std::sync::OnceLock;
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut rcp = [0u16; 512];
        let mut rsq = [0u16; 512];
        for i in 0..512u64 {
            // 1.0 / (1 + i/512) in 1.16 fixed point, leading one dropped.
            rcp[i as usize] = (((1u64 << 34) / (i + 512) + 1) >> 8) as u16;
            let a = (i + 512) >> (i % 2);
            let mut b = 1u64 << 17;
            while a * (b + 1) * (b + 1) < (1u64 << 44) {
                b += 1;
            }
            rsq[i as usize] = (b >> 1) as u16;
        }
        Tables { rcp, rsq }
    })
}

pub struct Rsp {
    r: [u32; 32],
    branch_to: Option<u32>,
    /// Vector registers as 16 big-endian bytes each.
    vr: [[u8; 16]; 32],
    acc: [i64; 8],
    vco: u16,
    vcc: u16,
    vce: u8,
    div_in: i32,
    div_out: i32,
    div_dp: bool,
    pub cycles: u64,
}

impl Default for Rsp {
    fn default() -> Self {
        Rsp { r: [0; 32], branch_to: None, vr: [[0; 16]; 32], acc: [0; 8], vco: 0, vcc: 0, vce: 0, div_in: 0, div_out: 0, div_dp: false, cycles: 0 }
    }
}

#[inline]
fn lane(v: &[u8; 16], i: usize) -> u16 {
    u16::from_be_bytes([v[i * 2], v[i * 2 + 1]])
}

#[inline]
fn set_lane(v: &mut [u8; 16], i: usize, x: u16) {
    v[i * 2..i * 2 + 2].copy_from_slice(&x.to_be_bytes());
}

/// Element broadcast pattern: which lane of vt feeds lane i.
#[inline]
fn elem(e: u32, i: usize) -> usize {
    match e {
        0 | 1 => i,
        2 => i & 6,
        3 => (i & 6) | 1,
        4 => i & 4,
        5 => (i & 4) | 1,
        6 => (i & 4) | 2,
        7 => (i & 4) | 3,
        _ => (e - 8) as usize,
    }
}

#[inline]
fn sext48(v: i64) -> i64 {
    (v << 16) >> 16
}

impl Rsp {
    fn acc_lo(&self, i: usize) -> u16 {
        self.acc[i] as u16
    }
    fn acc_md(&self, i: usize) -> u16 {
        (self.acc[i] >> 16) as u16
    }
    fn acc_hi(&self, i: usize) -> u16 {
        (self.acc[i] >> 32) as u16
    }
    fn set_acc_lo(&mut self, i: usize, v: u16) {
        self.acc[i] = (self.acc[i] & !0xFFFF) | v as i64;
    }
    fn clamp_signed(&self, i: usize) -> u16 {
        let v = (self.acc[i] >> 16) as i32;
        v.clamp(-32768, 32767) as i16 as u16
    }
    fn clamp_low(&self, i: usize) -> u16 {
        let v = (self.acc[i] >> 16) as i32;
        if v < -32768 {
            0
        } else if v > 32767 {
            0xFFFF
        } else {
            self.acc_lo(i)
        }
    }
    fn clamp_unsigned(&self, i: usize) -> u16 {
        let v = (self.acc[i] >> 16) as i32;
        if v < 0 {
            0
        } else if v > 32767 {
            0xFFFF
        } else {
            v as u16
        }
    }

    // ---------------------------------------------------------------- memory

    fn dmem8(hw: &Hw, a: u32) -> u8 {
        hw.dmem[(a & 0xFFF) as usize]
    }
    fn set_dmem8(hw: &mut Hw, a: u32, v: u8) {
        hw.dmem[(a & 0xFFF) as usize] = v;
    }
    fn load(hw: &Hw, a: u32, n: u32) -> u32 {
        (0..n).fold(0, |v, k| (v << 8) | Self::dmem8(hw, a + k) as u32)
    }
    fn store(hw: &mut Hw, a: u32, v: u32, n: u32) {
        for k in 0..n {
            Self::set_dmem8(hw, a + k, (v >> (8 * (n - 1 - k))) as u8);
        }
    }

    fn cop0_read(hw: &mut Hw, rd: u32) -> u32 {
        let a = if rd < 8 { 0x0404_0000 + rd * 4 } else { 0x0410_0000 + (rd - 8) * 4 };
        hw.reg_read(a)
    }

    fn cop0_write(hw: &mut Hw, rd: u32, v: u32) {
        let a = if rd < 8 { 0x0404_0000 + rd * 4 } else { 0x0410_0000 + (rd - 8) * 4 };
        hw.reg_write(a, v);
    }

    // ---------------------------------------------------------------- run

    /// Runs up to `budget` instructions while the RSP isn't halted.
    pub fn run(&mut self, hw: &mut Hw, budget: u32) {
        for _ in 0..budget {
            if hw.sp_status & 1 != 0 {
                return;
            }
            self.step(hw);
            if hw.sp_status & 0x20 != 0 {
                // Single-step mode: stop after one instruction.
                hw.sp_status |= 1;
                return;
            }
        }
    }

    fn step(&mut self, hw: &mut Hw) {
        let pc = hw.sp_pc & 0xFFC;
        let op = u32::from_be_bytes([hw.imem[pc as usize], hw.imem[pc as usize + 1], hw.imem[pc as usize + 2], hw.imem[pc as usize + 3]]);
        let pending = self.branch_to.take();
        self.execute(hw, op, pc);
        self.r[0] = 0;
        self.cycles += 1;
        hw.sp_pc = match pending {
            Some(t) => t & 0xFFC,
            None => (pc + 4) & 0xFFC,
        };
    }

    fn branch(&mut self, cond: bool, target: u32) {
        if cond {
            self.branch_to = Some(target);
        }
    }

    fn execute(&mut self, hw: &mut Hw, op: u32, pc: u32) {
        let rs = ((op >> 21) & 31) as usize;
        let rt = ((op >> 16) & 31) as usize;
        let rd = ((op >> 11) & 31) as usize;
        let s = self.r[rs];
        let t = self.r[rt];
        let imm = op as i16 as i32 as u32;
        let target = pc.wrapping_add(4).wrapping_add(imm << 2);
        match op >> 26 {
            0x00 => {
                let sa = (op >> 6) & 31;
                match op & 63 {
                    0x00 => self.r[rd] = t << sa,
                    0x02 => self.r[rd] = t >> sa,
                    0x03 => self.r[rd] = ((t as i32) >> sa) as u32,
                    0x04 => self.r[rd] = t << (s & 31),
                    0x06 => self.r[rd] = t >> (s & 31),
                    0x07 => self.r[rd] = ((t as i32) >> (s & 31)) as u32,
                    0x08 => self.branch(true, s),
                    0x09 => {
                        self.r[rd] = (pc + 8) & 0xFFC;
                        self.branch(true, s);
                    }
                    0x0D => {
                        // BREAK: halt and signal the CPU.
                        hw.sp_status |= 1 | 2;
                        if hw.sp_status & 0x40 != 0 {
                            hw.mi_intr |= 1;
                        }
                    }
                    0x20 | 0x21 => self.r[rd] = s.wrapping_add(t),
                    0x22 | 0x23 => self.r[rd] = s.wrapping_sub(t),
                    0x24 => self.r[rd] = s & t,
                    0x25 => self.r[rd] = s | t,
                    0x26 => self.r[rd] = s ^ t,
                    0x27 => self.r[rd] = !(s | t),
                    0x2A => self.r[rd] = ((s as i32) < (t as i32)) as u32,
                    0x2B => self.r[rd] = (s < t) as u32,
                    _ => {}
                }
            }
            0x01 => {
                let neg = (s as i32) < 0;
                match rt {
                    0x00 => self.branch(neg, target),
                    0x01 => self.branch(!neg, target),
                    0x10 => {
                        self.r[31] = (pc + 8) & 0xFFC;
                        self.branch(neg, target);
                    }
                    0x11 => {
                        self.r[31] = (pc + 8) & 0xFFC;
                        self.branch(!neg, target);
                    }
                    _ => {}
                }
            }
            0x02 => self.branch(true, (op & 0x3FF) << 2),
            0x03 => {
                self.r[31] = (pc + 8) & 0xFFC;
                self.branch(true, (op & 0x3FF) << 2);
            }
            0x04 => self.branch(s == t, target),
            0x05 => self.branch(s != t, target),
            0x06 => self.branch((s as i32) <= 0, target),
            0x07 => self.branch((s as i32) > 0, target),
            0x08 | 0x09 => self.r[rt] = s.wrapping_add(imm),
            0x0A => self.r[rt] = ((s as i32) < (imm as i32)) as u32,
            0x0B => self.r[rt] = (s < imm) as u32,
            0x0C => self.r[rt] = s & (op & 0xFFFF),
            0x0D => self.r[rt] = s | (op & 0xFFFF),
            0x0E => self.r[rt] = s ^ (op & 0xFFFF),
            0x0F => self.r[rt] = (op & 0xFFFF) << 16,
            0x10 => match rs {
                0x00 => self.r[rt] = Self::cop0_read(hw, rd as u32 & 15),
                0x04 => Self::cop0_write(hw, rd as u32 & 15, t),
                _ => {}
            },
            0x12 => self.cop2(op),
            0x20 => self.r[rt] = Self::load(hw, s.wrapping_add(imm), 1) as i8 as i32 as u32,
            0x21 => self.r[rt] = Self::load(hw, s.wrapping_add(imm), 2) as i16 as i32 as u32,
            0x23 => self.r[rt] = Self::load(hw, s.wrapping_add(imm), 4),
            0x24 => self.r[rt] = Self::load(hw, s.wrapping_add(imm), 1),
            0x25 => self.r[rt] = Self::load(hw, s.wrapping_add(imm), 2),
            0x27 => self.r[rt] = Self::load(hw, s.wrapping_add(imm), 4),
            0x28 => Self::store(hw, s.wrapping_add(imm), t, 1),
            0x29 => Self::store(hw, s.wrapping_add(imm), t, 2),
            0x2B => Self::store(hw, s.wrapping_add(imm), t, 4),
            0x32 => self.lwc2(hw, op, s),
            0x3A => self.swc2(hw, op, s),
            _ => {}
        }
    }

    // ---------------------------------------------------------------- vector loads / stores

    fn lwc2(&mut self, hw: &Hw, op: u32, base: u32) {
        let vt = ((op >> 16) & 31) as usize;
        let e = ((op >> 7) & 15) as usize;
        let kind = (op >> 11) & 31;
        let off = ((op & 0x7F) as i32) << 25 >> 25;
        let size: i32 = match kind {
            0 => 1,
            1 => 2,
            2 => 4,
            3 => 8,
            4 | 5 | 8 | 9 | 10 | 11 => 16,
            _ => 8,
        };
        let a = base.wrapping_add((off * size) as u32);
        let v = &mut self.vr[vt];
        match kind {
            0..=3 => {
                for k in 0..size as usize {
                    if e + k < 16 {
                        v[e + k] = Self::dmem8(hw, a + k as u32);
                    }
                }
            }
            4 => {
                let end = (a & !15) + 16;
                for (k, aa) in (a..end).enumerate() {
                    if e + k < 16 {
                        v[e + k] = Self::dmem8(hw, aa);
                    }
                }
            }
            5 => {
                let start = a & !15;
                let mut idx = 16 - (a & 15) as usize + e;
                for aa in start..a {
                    if idx < 16 {
                        v[idx] = Self::dmem8(hw, aa);
                    }
                    idx += 1;
                }
            }
            6 | 7 => {
                let shift = if kind == 6 { 8 } else { 7 };
                let aligned = a & !7;
                for i in 0..8 {
                    let b = Self::dmem8(hw, aligned + (((a & 7) as usize + 16 - e + i) & 15) as u32);
                    set_lane(v, i, (b as u16) << shift);
                }
            }
            8 => {
                let aligned = a & !7;
                for i in 0..8 {
                    let b = Self::dmem8(hw, aligned + (((a & 7) as usize + 16 - e + i * 2) & 15) as u32);
                    set_lane(v, i, (b as u16) << 7);
                }
            }
            9 => {
                let aligned = a & !7;
                for i in 0..4 {
                    let b = Self::dmem8(hw, aligned + (((a & 7) as usize + 16 - e + i * 4) & 15) as u32);
                    let l = (e / 2 + i) & 7;
                    set_lane(v, l, (b as u16) << 7);
                }
            }
            11 => {
                // LTV: eight registers, one lane each, walking a 16-byte line.
                let begin = a & !7;
                let mut addr = begin + ((e as u32 + (a & 8)) & 15);
                let group = vt & !7;
                let mut off = e / 2;
                for i in 0..8 {
                    let hi = Self::dmem8(hw, addr);
                    let lo = Self::dmem8(hw, addr + 1);
                    addr += 2;
                    if addr >= begin + 16 {
                        addr -= 16;
                    }
                    let r = &mut self.vr[group + off];
                    r[i * 2] = hi;
                    r[i * 2 + 1] = lo;
                    off = (off + 1) & 7;
                }
            }
            _ => {}
        }
    }

    fn swc2(&mut self, hw: &mut Hw, op: u32, base: u32) {
        let vt = ((op >> 16) & 31) as usize;
        let e = ((op >> 7) & 15) as usize;
        let kind = (op >> 11) & 31;
        let off = ((op & 0x7F) as i32) << 25 >> 25;
        let size: i32 = match kind {
            0 => 1,
            1 => 2,
            2 => 4,
            3 => 8,
            4 | 5 | 8 | 9 | 10 | 11 => 16,
            _ => 8,
        };
        let a = base.wrapping_add((off * size) as u32);
        let v = self.vr[vt];
        match kind {
            0..=3 => {
                for k in 0..size as usize {
                    Self::set_dmem8(hw, a + k as u32, v[(e + k) & 15]);
                }
            }
            4 => {
                let end = (a & !15) + 16;
                for (k, aa) in (a..end).enumerate() {
                    Self::set_dmem8(hw, aa, v[(e + k) & 15]);
                }
            }
            5 => {
                let start = a & !15;
                let base_idx = 16 - (a & 15) as usize + e;
                for (k, aa) in (start..a).enumerate() {
                    Self::set_dmem8(hw, aa, v[(base_idx + k) & 15]);
                }
            }
            6 | 7 => {
                for i in 0..8 {
                    let idx = (e + i) & 15;
                    let l = lane(&v, idx & 7);
                    let packed_hi = (kind == 6) == (idx < 8);
                    let b = if packed_hi { (l >> 8) as u8 } else { (l >> 7) as u8 };
                    Self::set_dmem8(hw, a + i as u32, b);
                }
            }
            8 => {
                let aligned = a & !7;
                for i in 0..8 {
                    let l = lane(&v, (e / 2 + i) & 7);
                    Self::set_dmem8(hw, aligned + (((a & 7) as usize + i * 2) & 15) as u32, (l >> 7) as u8);
                }
            }
            9 => {
                let aligned = a & !7;
                for i in 0..4 {
                    let l = lane(&v, (e / 2 + i) & 7);
                    Self::set_dmem8(hw, aligned + (((a & 7) as usize + i * 4) & 15) as u32, (l >> 7) as u8);
                }
            }
            10 => {
                // SWV: wrap within the 16-byte line.
                let aligned = a & !7;
                for k in 0..16 {
                    Self::set_dmem8(hw, aligned + (((a & 7) as usize + k) & 15) as u32, v[(e + k) & 15]);
                }
            }
            11 => {
                // STV: the mirror of LTV - lane i of register (e/2 + i) goes to word i of the line.
                let begin = a & !7;
                let start = a & 8;
                let group = vt & !7;
                for i in 0..8usize {
                    let r = group + ((e / 2 + i) & 7);
                    let (hi, lo) = (self.vr[r][i * 2], self.vr[r][i * 2 + 1]);
                    let o = (start + i as u32 * 2) & 15;
                    Self::set_dmem8(hw, begin + o, hi);
                    Self::set_dmem8(hw, begin + ((o + 1) & 15), lo);
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- COP2

    fn cop2(&mut self, op: u32) {
        let rs = (op >> 21) & 31;
        let rt = ((op >> 16) & 31) as usize;
        let rd = ((op >> 11) & 31) as usize;
        if rs & 0x10 == 0 {
            let e = ((op >> 7) & 15) as usize;
            match rs {
                0x00 => {
                    let v = &self.vr[rd];
                    let x = u16::from_be_bytes([v[e], v[(e + 1) & 15]]);
                    self.r[rt] = x as i16 as i32 as u32;
                }
                0x02 => {
                    let x = match rd & 3 {
                        0 => self.vco,
                        1 => self.vcc,
                        _ => self.vce as u16,
                    };
                    self.r[rt] = x as i16 as i32 as u32;
                }
                0x04 => {
                    let x = (self.r[rt] as u16).to_be_bytes();
                    self.vr[rd][e] = x[0];
                    if e + 1 < 16 {
                        self.vr[rd][e + 1] = x[1];
                    }
                }
                0x06 => match rd & 3 {
                    0 => self.vco = self.r[rt] as u16,
                    1 => self.vcc = self.r[rt] as u16,
                    _ => self.vce = self.r[rt] as u8,
                },
                _ => {}
            }
            return;
        }
        let e = (op >> 21) & 15;
        let vt = ((op >> 16) & 31) as usize;
        let vs = ((op >> 11) & 31) as usize;
        let vd = ((op >> 6) & 31) as usize;
        let s: [u16; 8] = std::array::from_fn(|i| lane(&self.vr[vs], i));
        let t: [u16; 8] = std::array::from_fn(|i| lane(&self.vr[vt], elem(e, i)));
        let mut d = [0u16; 8];
        let f = op & 63;
        match f {
            0x00..=0x0F => {
                for i in 0..8 {
                    let (a, b) = (s[i] as i16 as i64, t[i] as i16 as i64);
                    let (au, bu) = (s[i] as i64, t[i] as i64);
                    let acc = self.acc[i];
                    self.acc[i] = sext48(match f {
                        0x00 | 0x01 => a * b * 2 + 0x8000,
                        0x04 => (au * bu) >> 16,
                        0x05 => a * bu,
                        0x06 => au * b,
                        0x07 => (a * b) << 16,
                        0x08 | 0x09 => acc + a * b * 2,
                        0x0C => acc + ((au * bu) >> 16),
                        0x0D => acc + a * bu,
                        0x0E => acc + au * b,
                        0x0F => acc + ((a * b) << 16),
                        _ => acc,
                    });
                    d[i] = match f {
                        0x00 | 0x08 | 0x07 | 0x0F | 0x0D => self.clamp_signed(i),
                        0x01 | 0x09 => self.clamp_unsigned(i),
                        0x04 | 0x06 => self.acc_lo(i),
                        0x05 => self.acc_md(i),
                        0x0C | 0x0E => self.clamp_low(i),
                        _ => 0,
                    };
                }
            }
            0x10 | 0x11 => {
                for i in 0..8 {
                    let c = ((self.vco >> i) & 1) as i32;
                    let (a, b) = (s[i] as i16 as i32, t[i] as i16 as i32);
                    let r = if f == 0x10 { a + b + c } else { a - b - c };
                    self.set_acc_lo(i, r as u16);
                    d[i] = r.clamp(-32768, 32767) as u16;
                }
                self.vco = 0;
            }
            0x13 => {
                for i in 0..8 {
                    let (a, b) = (s[i] as i16, t[i] as i16);
                    let r = if a < 0 {
                        if b == i16::MIN {
                            self.set_acc_lo(i, 0x8000);
                            d[i] = 0x7FFF;
                            continue;
                        }
                        -b
                    } else if a == 0 {
                        0
                    } else {
                        b
                    };
                    self.set_acc_lo(i, r as u16);
                    d[i] = r as u16;
                }
            }
            0x14 => {
                self.vco = 0;
                for i in 0..8 {
                    let r = s[i] as u32 + t[i] as u32;
                    self.set_acc_lo(i, r as u16);
                    d[i] = r as u16;
                    if r > 0xFFFF {
                        self.vco |= 1 << i;
                    }
                }
            }
            0x15 => {
                self.vco = 0;
                for i in 0..8 {
                    let r = s[i] as i32 - t[i] as i32;
                    self.set_acc_lo(i, r as u16);
                    d[i] = r as u16;
                    if r < 0 {
                        self.vco |= 1 << i;
                    }
                    if r != 0 {
                        self.vco |= 0x100 << i;
                    }
                }
            }
            0x1D => {
                for i in 0..8 {
                    d[i] = match e {
                        8 => self.acc_hi(i),
                        9 => self.acc_md(i),
                        10 => self.acc_lo(i),
                        _ => 0,
                    };
                }
            }
            0x20..=0x23 => {
                let mut vcc = 0u16;
                for i in 0..8 {
                    let (a, b) = (s[i] as i16, t[i] as i16);
                    let ne = (self.vco >> (8 + i)) & 1 != 0;
                    let c = (self.vco >> i) & 1 != 0;
                    let cond = match f {
                        0x20 => a < b || (a == b && ne && c),
                        0x21 => a == b && !ne,
                        0x22 => a != b || ne,
                        _ => a > b || (a == b && !(ne && c)),
                    };
                    if cond {
                        vcc |= 1 << i;
                    }
                    d[i] = if f == 0x21 || f == 0x22 { if cond { s[i] } else { t[i] } } else if cond { s[i] } else { t[i] };
                    self.set_acc_lo(i, d[i]);
                }
                self.vcc = vcc;
                self.vco = 0;
            }
            0x24 => {
                // VCL
                let mut vcc = 0u16;
                for i in 0..8 {
                    let (a, b) = (s[i] as u32, t[i] as u32);
                    let sign = (self.vco >> i) & 1 != 0;
                    let ne = (self.vco >> (8 + i)) & 1 != 0;
                    let mut le = (self.vcc >> i) & 1 != 0;
                    let mut ge = (self.vcc >> (8 + i)) & 1 != 0;
                    let vce = (self.vce >> i) & 1 != 0;
                    if sign {
                        if !ne {
                            let sum = a + b;
                            let carry = sum > 0xFFFF;
                            let zero = sum & 0xFFFF == 0;
                            le = if vce { zero || !carry } else { zero && !carry };
                        }
                        d[i] = if le { (t[i] as i16).wrapping_neg() as u16 } else { s[i] };
                    } else {
                        if !ne {
                            ge = a as i32 - b as i32 >= 0;
                        }
                        d[i] = if ge { t[i] } else { s[i] };
                    }
                    if le {
                        vcc |= 1 << i;
                    }
                    if ge {
                        vcc |= 0x100 << i;
                    }
                    self.set_acc_lo(i, d[i]);
                }
                self.vcc = vcc;
                self.vco = 0;
                self.vce = 0;
            }
            0x25 => {
                // VCH
                let (mut vcc, mut vco, mut vce) = (0u16, 0u16, 0u8);
                for i in 0..8 {
                    let (a, b) = (s[i] as i16 as i32, t[i] as i16 as i32);
                    let sign = (a ^ b) < 0;
                    let (le, ge, ne, ce);
                    if sign {
                        let sum = a + b;
                        le = sum <= 0;
                        ge = b < 0;
                        ce = sum == -1;
                        ne = sum != 0 && !ce;
                        d[i] = if le { (-b) as u16 } else { s[i] };
                    } else {
                        let diff = a - b;
                        le = b < 0;
                        ge = diff >= 0;
                        ce = false;
                        ne = diff != 0;
                        d[i] = if ge { t[i] } else { s[i] };
                    }
                    if le {
                        vcc |= 1 << i;
                    }
                    if ge {
                        vcc |= 0x100 << i;
                    }
                    if sign {
                        vco |= 1 << i;
                    }
                    if ne {
                        vco |= 0x100 << i;
                    }
                    if ce {
                        vce |= 1 << i;
                    }
                    self.set_acc_lo(i, d[i]);
                }
                self.vcc = vcc;
                self.vco = vco;
                self.vce = vce;
            }
            0x26 => {
                // VCR
                let mut vcc = 0u16;
                for i in 0..8 {
                    let (a, b) = (s[i] as i16 as i32, t[i] as i16 as i32);
                    let sign = (a ^ b) < 0;
                    let (le, ge);
                    if sign {
                        ge = b < 0;
                        le = a + b + 1 <= 0;
                        d[i] = if le { !t[i] } else { s[i] };
                    } else {
                        le = b < 0;
                        ge = a - b >= 0;
                        d[i] = if ge { t[i] } else { s[i] };
                    }
                    if le {
                        vcc |= 1 << i;
                    }
                    if ge {
                        vcc |= 0x100 << i;
                    }
                    self.set_acc_lo(i, d[i]);
                }
                self.vcc = vcc;
                self.vco = 0;
                self.vce = 0;
            }
            0x27 => {
                for i in 0..8 {
                    d[i] = if (self.vcc >> i) & 1 != 0 { s[i] } else { t[i] };
                    self.set_acc_lo(i, d[i]);
                }
                self.vco = 0;
            }
            0x28..=0x2D => {
                for i in 0..8 {
                    d[i] = match f {
                        0x28 => s[i] & t[i],
                        0x29 => !(s[i] & t[i]),
                        0x2A => s[i] | t[i],
                        0x2B => !(s[i] | t[i]),
                        0x2C => s[i] ^ t[i],
                        _ => !(s[i] ^ t[i]),
                    };
                    self.set_acc_lo(i, d[i]);
                }
            }
            0x30..=0x36 => {
                let de = (vs & 7) as usize;
                let input_lane = lane(&self.vr[vt], (e & 7) as usize);
                let mut out = self.vr[vd];
                match f {
                    0x30 | 0x31 | 0x34 | 0x35 => {
                        let wide = f == 0x31 || f == 0x35;
                        let input: i32 = if wide && self.div_dp { (self.div_in << 16) | input_lane as i32 } else { input_lane as i16 as i32 };
                        let r = if f < 0x34 { rcp(input, wide && self.div_dp) } else { rsq(input, wide && self.div_dp) };
                        self.div_out = r >> 16;
                        set_lane(&mut out, de, r as u16);
                        self.div_dp = false;
                    }
                    0x32 | 0x36 => {
                        self.div_in = input_lane as i16 as i32;
                        set_lane(&mut out, de, self.div_out as u16);
                        self.div_dp = true;
                    }
                    0x33 => set_lane(&mut out, de, t[de]),
                    _ => {}
                }
                for i in 0..8 {
                    self.set_acc_lo(i, t[i]);
                }
                self.vr[vd] = out;
                return;
            }
            _ => return, // VNOP and reserved
        }
        for i in 0..8 {
            set_lane(&mut self.vr[vd], i, d[i]);
        }
    }
}

fn rcp(input: i32, wide: bool) -> i32 {
    let t = tables();
    if input == 0 {
        return 0x7FFF_FFFF;
    }
    if !wide && input == -32768 {
        return 0xFFFF_0000u32 as i32;
    }
    let mask = input >> 31;
    let data = ((input ^ mask) - mask) as u32;
    let shift = data.leading_zeros();
    let idx = ((data << shift) & 0x7FC0_0000) >> 22;
    let r = (((0x10000 | t.rcp[idx as usize] as u32) << 14) >> (31 - shift)) as i32;
    r ^ mask
}

fn rsq(input: i32, wide: bool) -> i32 {
    let t = tables();
    if input == 0 {
        return 0x7FFF_FFFF;
    }
    if !wide && input == -32768 {
        return 0xFFFF_0000u32 as i32;
    }
    let mask = input >> 31;
    let data = ((input ^ mask) - mask) as u32;
    let shift = data.leading_zeros();
    let idx = ((((data << shift) & 0x7FC0_0000) >> 22) & 0x1FE) | (shift & 1);
    let r = (((0x10000 | t.rsq[idx as usize] as u32) << 14) >> ((31 - shift) >> 1)) as i32;
    r ^ mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reciprocal_table_is_sane() {
        // 1/1.0 and 1/2.0 in the RSP's fixed-point format.
        assert_eq!(rcp(1, false), 0x4000_0000);
        assert_eq!(rcp(2, false), 0x2000_0000);
        assert!(rcp(-2, false) < 0);
        assert_eq!(rcp(0, false), 0x7FFF_FFFF);
        // 1/sqrt(4) is half of 1/sqrt(1).
        assert_eq!(rsq(4, false), rsq(1, false) / 2);
    }
}
