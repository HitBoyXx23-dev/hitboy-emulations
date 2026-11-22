//! Hudson HuC6280: a 65C02 with an 8-entry memory mapper (MPR), block-transfer
//! instructions, the T flag (memory-to-memory ALU ops), fast/slow clock switching and
//! VDC store instructions (ST0/ST1/ST2).

pub trait Bus {
    /// Physical address (21 bits).
    fn read(&mut self, addr: u32) -> u8;
    fn write(&mut self, addr: u32, v: u8);
    /// ST0/ST1/ST2 write straight to the VDC ports (0 = register select, 2/3 = data).
    fn vdc_store(&mut self, port: u32, v: u8);
}

const C: u8 = 0x01;
const Z: u8 = 0x02;
const I: u8 = 0x04;
const D: u8 = 0x08;
const B: u8 = 0x10;
const T: u8 = 0x20;
const V: u8 = 0x40;
const N: u8 = 0x80;

#[derive(state::Stateful)]
pub struct Cpu {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub s: u8,
    pub p: u8,
    pub pc: u16,
    pub mpr: [u8; 8],
    /// 7.16 MHz (true) or 1.79 MHz.
    pub fast: bool,
    /// Interrupt request lines: bit 0 IRQ2, bit 1 IRQ1 (VDC), bit 2 timer.
    pub irq: u8,
    /// Interrupt disable mask from $1402 (bits as above).
    pub irq_mask: u8,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Cpu {
        Cpu { a: 0, x: 0, y: 0, s: 0xFF, p: I, pc: 0, mpr: [0xFF, 0xF8, 0, 0, 0, 0, 0, 0], fast: false, irq: 0, irq_mask: 0 }
    }

    pub fn reset<B: Bus>(&mut self, bus: &mut B) {
        self.mpr[7] = 0;
        self.p = I;
        self.fast = false;
        self.pc = self.read16(bus, 0xFFFE);
    }

    fn phys(&self, a: u16) -> u32 {
        (self.mpr[(a >> 13) as usize] as u32) << 13 | (a & 0x1FFF) as u32
    }
    fn rd<B: Bus>(&self, bus: &mut B, a: u16) -> u8 {
        bus.read(self.phys(a))
    }
    fn wr<B: Bus>(&self, bus: &mut B, a: u16, v: u8) {
        bus.write(self.phys(a), v)
    }
    fn read16<B: Bus>(&self, bus: &mut B, a: u16) -> u16 {
        self.rd(bus, a) as u16 | (self.rd(bus, a.wrapping_add(1)) as u16) << 8
    }
    /// Zero page and stack live in the page mapped by MPR1 ($2000-$21FF).
    fn zp<B: Bus>(&self, bus: &mut B, a: u8) -> u8 {
        self.rd(bus, 0x2000 | a as u16)
    }
    fn zp_w<B: Bus>(&self, bus: &mut B, a: u8, v: u8) {
        self.wr(bus, 0x2000 | a as u16, v)
    }
    fn zp16<B: Bus>(&self, bus: &mut B, a: u8) -> u16 {
        self.zp(bus, a) as u16 | (self.zp(bus, a.wrapping_add(1)) as u16) << 8
    }
    fn fetch<B: Bus>(&mut self, bus: &mut B) -> u8 {
        let v = self.rd(bus, self.pc);
        self.pc = self.pc.wrapping_add(1);
        v
    }
    fn fetch16<B: Bus>(&mut self, bus: &mut B) -> u16 {
        let lo = self.fetch(bus) as u16;
        lo | (self.fetch(bus) as u16) << 8
    }
    fn push<B: Bus>(&mut self, bus: &mut B, v: u8) {
        self.wr(bus, 0x2100 | self.s as u16, v);
        self.s = self.s.wrapping_sub(1);
    }
    fn pop<B: Bus>(&mut self, bus: &mut B) -> u8 {
        self.s = self.s.wrapping_add(1);
        self.rd(bus, 0x2100 | self.s as u16)
    }
    fn nz(&mut self, v: u8) {
        self.p = (self.p & !(N | Z)) | (v & N) | if v == 0 { Z } else { 0 };
    }
    fn flag(&mut self, f: u8, on: bool) {
        if on {
            self.p |= f
        } else {
            self.p &= !f
        }
    }

    fn adc(&mut self, a: u8, v: u8) -> u8 {
        let c = (self.p & C) as u16;
        if self.p & D != 0 {
            let mut lo = (a & 0xF) as u16 + (v & 0xF) as u16 + c;
            let mut hi = (a >> 4) as u16 + (v >> 4) as u16;
            if lo > 9 {
                lo += 6;
            }
            if lo > 0xF {
                hi += 1;
            }
            if hi > 9 {
                hi += 6;
            }
            self.flag(C, hi > 0xF);
            let r = ((hi << 4) | (lo & 0xF)) as u8;
            self.nz(r);
            r
        } else {
            let s = a as u16 + v as u16 + c;
            let r = s as u8;
            self.flag(C, s > 0xFF);
            self.flag(V, (!(a ^ v) & (a ^ r)) & 0x80 != 0);
            self.nz(r);
            r
        }
    }

    fn sbc(&mut self, a: u8, v: u8) -> u8 {
        if self.p & D != 0 {
            let b = 1 - (self.p & C) as i16;
            let mut lo = (a & 0xF) as i16 - (v & 0xF) as i16 - b;
            let mut hi = (a >> 4) as i16 - (v >> 4) as i16;
            if lo < 0 {
                lo -= 6;
                hi -= 1;
            }
            if hi < 0 {
                hi -= 6;
            }
            let bin = a as i16 - v as i16 - b;
            self.flag(C, bin >= 0);
            let r = ((hi << 4) | (lo & 0xF)) as u8;
            self.nz(r);
            r
        } else {
            self.adc(a, !v)
        }
    }

    fn cmp(&mut self, r: u8, v: u8) {
        self.flag(C, r >= v);
        self.nz(r.wrapping_sub(v));
    }

    fn bit(&mut self, v: u8) {
        self.p = (self.p & !(N | V | Z)) | (v & (N | V)) | if self.a & v == 0 { Z } else { 0 };
    }

    fn branch<B: Bus>(&mut self, bus: &mut B, cond: bool) -> u32 {
        let off = self.fetch(bus) as i8;
        if cond {
            self.pc = self.pc.wrapping_add(off as u16);
            4
        } else {
            2
        }
    }

    fn interrupt<B: Bus>(&mut self, bus: &mut B, vector: u16, brk: bool) {
        let pc = self.pc;
        self.push(bus, (pc >> 8) as u8);
        self.push(bus, pc as u8);
        let flags = if brk { self.p | B } else { self.p & !B };
        self.push(bus, flags & !T);
        self.p = (self.p | I) & !(D | T);
        self.pc = self.read16(bus, vector);
    }

    /// Operand address for the classic addressing modes (group 1 decoding).
    fn addr<B: Bus>(&mut self, bus: &mut B, mode: u8) -> (u16, u32) {
        match mode {
            0 => {
                // (zp,X)
                let z = self.fetch(bus).wrapping_add(self.x);
                (self.zp16(bus, z), 7)
            }
            1 => (0x2000 | self.fetch(bus) as u16, 4),
            3 => (self.fetch16(bus), 5),
            4 => {
                // (zp),Y
                let z = self.fetch(bus);
                (self.zp16(bus, z).wrapping_add(self.y as u16), 7)
            }
            5 => (0x2000 | self.fetch(bus).wrapping_add(self.x) as u16, 4),
            6 => (self.fetch16(bus).wrapping_add(self.y as u16), 5),
            7 => (self.fetch16(bus).wrapping_add(self.x as u16), 5),
            _ => {
                // (zp) - opcode $x2 on the 65C02
                let z = self.fetch(bus);
                (self.zp16(bus, z), 7)
            }
        }
    }

    /// Group-1 ALU op (ORA AND EOR ADC STA LDA CMP SBC) with T-flag support.
    fn alu<B: Bus>(&mut self, bus: &mut B, op: u8, v: u8, t: bool) {
        // With T set, the accumulator is replaced by the zero-page byte at X.
        let a = if t { self.zp(bus, self.x) } else { self.a };
        let r = match op >> 5 {
            0 => {
                let r = a | v;
                self.nz(r);
                r
            }
            1 => {
                let r = a & v;
                self.nz(r);
                r
            }
            2 => {
                let r = a ^ v;
                self.nz(r);
                r
            }
            3 => self.adc(a, v),
            6 => {
                self.cmp(self.a, v);
                return;
            }
            _ => self.sbc(a, v),
        };
        if t {
            self.zp_w(bus, self.x, r);
        } else {
            self.a = r;
        }
    }

    fn rmw(&mut self, kind: u8, v: u8) -> u8 {
        let r = match kind {
            0 => {
                self.flag(C, v & 0x80 != 0);
                v << 1
            }
            1 => {
                let c = self.p & C;
                self.flag(C, v & 0x80 != 0);
                v << 1 | c
            }
            2 => {
                self.flag(C, v & 1 != 0);
                v >> 1
            }
            3 => {
                let c = self.p & C;
                self.flag(C, v & 1 != 0);
                v >> 1 | c << 7
            }
            6 => v.wrapping_sub(1),
            _ => v.wrapping_add(1),
        };
        self.nz(r);
        r
    }

    fn block<B: Bus>(&mut self, bus: &mut B, op: u8) -> u32 {
        let src = self.fetch16(bus);
        let dst = self.fetch16(bus);
        let len = self.fetch16(bus);
        let n = if len == 0 { 0x1_0000u32 } else { len as u32 };
        let (mut s, mut d) = (src, dst);
        let mut alt = false;
        for _ in 0..n {
            let v = self.rd(bus, s);
            self.wr(bus, d, v);
            match op {
                0x73 => {
                    // TII: both increment
                    s = s.wrapping_add(1);
                    d = d.wrapping_add(1);
                }
                0xC3 => {
                    // TDD: both decrement
                    s = s.wrapping_sub(1);
                    d = d.wrapping_sub(1);
                }
                0xD3 => s = s.wrapping_add(1), // TIN: fixed destination
                0xE3 => {
                    // TIA: destination alternates
                    s = s.wrapping_add(1);
                    d = if alt { d.wrapping_sub(1) } else { d.wrapping_add(1) };
                    alt = !alt;
                }
                _ => {
                    // TAI: source alternates
                    s = if alt { s.wrapping_sub(1) } else { s.wrapping_add(1) };
                    alt = !alt;
                    d = d.wrapping_add(1);
                }
            }
        }
        17 + 6 * n
    }

    /// Executes one instruction; returns cycles at the current clock.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> u32 {
        let pending = self.irq & !self.irq_mask;
        if pending != 0 && self.p & I == 0 {
            let vector = if pending & 4 != 0 {
                0xFFFA
            } else if pending & 2 != 0 {
                0xFFF8
            } else {
                0xFFF6
            };
            self.interrupt(bus, vector, false);
            return 8;
        }
        let t = self.p & T != 0;
        self.p &= !T;
        let op = self.fetch(bus);
        match op {
            // ---- HuC6280 / 65C02 specials first
            0x00 => {
                self.pc = self.pc.wrapping_add(1);
                self.interrupt(bus, 0xFFF6, true);
                8
            }
            0x02 => {
                std::mem::swap(&mut self.x, &mut self.y);
                3
            }
            0x22 => {
                std::mem::swap(&mut self.a, &mut self.x);
                3
            }
            0x42 => {
                std::mem::swap(&mut self.a, &mut self.y);
                3
            }
            0x62 => {
                self.a = 0;
                2
            }
            0x82 => {
                self.x = 0;
                2
            }
            0xC2 => {
                self.y = 0;
                2
            }
            0x03 | 0x13 | 0x23 => {
                let v = self.fetch(bus);
                bus.vdc_store(match op {
                    0x03 => 0,
                    0x13 => 2,
                    _ => 3,
                }, v);
                5
            }
            0x43 => {
                // TMA: A = MPR[lowest set bit]
                let m = self.fetch(bus);
                if let Some(i) = (0..8).find(|i| m & (1 << i) != 0) {
                    self.a = self.mpr[i];
                }
                4
            }
            0x53 => {
                // TAM: MPR[bits] = A
                let m = self.fetch(bus);
                for i in 0..8 {
                    if m & (1 << i) != 0 {
                        self.mpr[i] = self.a;
                    }
                }
                5
            }
            0x73 | 0xC3 | 0xD3 | 0xE3 | 0xF3 => self.block(bus, op),
            0x83 | 0xA3 | 0x93 | 0xB3 => {
                // TST #imm, zp / zp,X / abs / abs,X
                let imm = self.fetch(bus);
                let a = match op {
                    0x83 => 0x2000 | self.fetch(bus) as u16,
                    0xA3 => 0x2000 | self.fetch(bus).wrapping_add(self.x) as u16,
                    0x93 => self.fetch16(bus),
                    _ => self.fetch16(bus).wrapping_add(self.x as u16),
                };
                let v = self.rd(bus, a);
                self.p = (self.p & !(N | V | Z)) | (v & (N | V)) | if imm & v == 0 { Z } else { 0 };
                7
            }
            0x44 => {
                // BSR
                let off = self.fetch(bus) as i8;
                let ret = self.pc.wrapping_sub(1);
                self.push(bus, (ret >> 8) as u8);
                self.push(bus, ret as u8);
                self.pc = self.pc.wrapping_add(off as u16);
                8
            }
            0x54 => {
                self.fast = false;
                3
            }
            0xD4 => {
                self.fast = true;
                3
            }
            0xF4 => {
                self.p |= T;
                return 2;
            }
            0x80 => self.branch(bus, true),
            0x0F | 0x1F | 0x2F | 0x3F | 0x4F | 0x5F | 0x6F | 0x7F | 0x8F | 0x9F | 0xAF | 0xBF | 0xCF | 0xDF | 0xEF | 0xFF => {
                // BBRn / BBSn zp, rel
                let z = self.fetch(bus);
                let v = self.zp(bus, z);
                let bit = (op >> 4) & 7;
                let set = v & (1 << bit) != 0;
                let off = self.fetch(bus) as i8;
                if set == (op & 0x80 != 0) {
                    self.pc = self.pc.wrapping_add(off as u16);
                    8
                } else {
                    6
                }
            }
            0x07 | 0x17 | 0x27 | 0x37 | 0x47 | 0x57 | 0x67 | 0x77 | 0x87 | 0x97 | 0xA7 | 0xB7 | 0xC7 | 0xD7 | 0xE7 | 0xF7 => {
                // RMBn / SMBn
                let z = self.fetch(bus);
                let bit = (op >> 4) & 7;
                let v = self.zp(bus, z);
                let r = if op & 0x80 != 0 { v | (1 << bit) } else { v & !(1 << bit) };
                self.zp_w(bus, z, r);
                7
            }
            0x04 | 0x0C | 0x14 | 0x1C => {
                // TSB / TRB
                let a = if op & 0x08 != 0 { self.fetch16(bus) } else { 0x2000 | self.fetch(bus) as u16 };
                let v = self.rd(bus, a);
                self.flag(Z, v & self.a == 0);
                self.p = (self.p & !(N | V)) | (v & (N | V));
                let r = if op & 0x10 == 0 { v | self.a } else { v & !self.a };
                self.wr(bus, a, r);
                6
            }
            0x64 | 0x74 | 0x9C | 0x9E => {
                // STZ
                let a = match op {
                    0x64 => 0x2000 | self.fetch(bus) as u16,
                    0x74 => 0x2000 | self.fetch(bus).wrapping_add(self.x) as u16,
                    0x9C => self.fetch16(bus),
                    _ => self.fetch16(bus).wrapping_add(self.x as u16),
                };
                self.wr(bus, a, 0);
                4
            }
            0x1A => {
                self.a = self.a.wrapping_add(1);
                self.nz(self.a);
                2
            }
            0x3A => {
                self.a = self.a.wrapping_sub(1);
                self.nz(self.a);
                2
            }
            0x5A => {
                self.push(bus, self.y);
                3
            }
            0x7A => {
                self.y = self.pop(bus);
                self.nz(self.y);
                4
            }
            0xDA => {
                self.push(bus, self.x);
                3
            }
            0xFA => {
                self.x = self.pop(bus);
                self.nz(self.x);
                4
            }
            0x89 => {
                // BIT #imm (Z only on the 65C02; the HuC6280 sets N/V from the operand too)
                let v = self.fetch(bus);
                self.bit(v);
                2
            }
            0x34 => {
                let a = 0x2000 | self.fetch(bus).wrapping_add(self.x) as u16;
                let v = self.rd(bus, a);
                self.bit(v);
                4
            }
            0x3C => {
                let a = self.fetch16(bus).wrapping_add(self.x as u16);
                let v = self.rd(bus, a);
                self.bit(v);
                5
            }
            0x7C => {
                let a = self.fetch16(bus).wrapping_add(self.x as u16);
                self.pc = self.read16(bus, a);
                7
            }
            0x12 | 0x32 | 0x52 | 0x72 | 0x92 | 0xB2 | 0xD2 | 0xF2 => {
                // (zp) addressing for the group-1 ops
                let (a, c) = self.addr(bus, 8);
                if op == 0x92 {
                    self.wr(bus, a, self.a);
                } else if op == 0xB2 {
                    self.a = self.rd(bus, a);
                    self.nz(self.a);
                } else {
                    let v = self.rd(bus, a);
                    self.alu(bus, op & 0xE0, v, t);
                }
                c
            }
            _ => self.classic(bus, op, t),
        }
    }

    /// The original 6502 instruction set.
    fn classic<B: Bus>(&mut self, bus: &mut B, op: u8, t: bool) -> u32 {
        let (aaa, bbb, cc) = (op >> 5, (op >> 2) & 7, op & 3);
        match op {
            0x20 => {
                let a = self.fetch16(bus);
                let ret = self.pc.wrapping_sub(1);
                self.push(bus, (ret >> 8) as u8);
                self.push(bus, ret as u8);
                self.pc = a;
                return 7;
            }
            0x40 => {
                self.p = self.pop(bus);
                let lo = self.pop(bus) as u16;
                self.pc = lo | (self.pop(bus) as u16) << 8;
                return 7;
            }
            0x60 => {
                let lo = self.pop(bus) as u16;
                self.pc = (lo | (self.pop(bus) as u16) << 8).wrapping_add(1);
                return 7;
            }
            0x4C => {
                self.pc = self.fetch16(bus);
                return 4;
            }
            0x6C => {
                let a = self.fetch16(bus);
                self.pc = self.read16(bus, a);
                return 7;
            }
            0x08 => {
                self.push(bus, self.p | B);
                return 3;
            }
            0x28 => {
                self.p = self.pop(bus);
                return 4;
            }
            0x48 => {
                self.push(bus, self.a);
                return 3;
            }
            0x68 => {
                self.a = self.pop(bus);
                self.nz(self.a);
                return 4;
            }
            0x18 | 0x38 | 0x58 | 0x78 | 0xB8 | 0xD8 | 0xF8 => {
                match op {
                    0x18 => self.p &= !C,
                    0x38 => self.p |= C,
                    0x58 => self.p &= !I,
                    0x78 => self.p |= I,
                    0xB8 => self.p &= !V,
                    0xD8 => self.p &= !D,
                    _ => self.p |= D,
                }
                return 2;
            }
            0xAA => {
                self.x = self.a;
                self.nz(self.x);
                return 2;
            }
            0x8A => {
                self.a = self.x;
                self.nz(self.a);
                return 2;
            }
            0xA8 => {
                self.y = self.a;
                self.nz(self.y);
                return 2;
            }
            0x98 => {
                self.a = self.y;
                self.nz(self.a);
                return 2;
            }
            0xBA => {
                self.x = self.s;
                self.nz(self.x);
                return 2;
            }
            0x9A => {
                self.s = self.x;
                return 2;
            }
            0xE8 => {
                self.x = self.x.wrapping_add(1);
                self.nz(self.x);
                return 2;
            }
            0xC8 => {
                self.y = self.y.wrapping_add(1);
                self.nz(self.y);
                return 2;
            }
            0xCA => {
                self.x = self.x.wrapping_sub(1);
                self.nz(self.x);
                return 2;
            }
            0x88 => {
                self.y = self.y.wrapping_sub(1);
                self.nz(self.y);
                return 2;
            }
            0xEA => return 2,
            0x10 => return self.branch(bus, self.p & N == 0),
            0x30 => return self.branch(bus, self.p & N != 0),
            0x50 => return self.branch(bus, self.p & V == 0),
            0x70 => return self.branch(bus, self.p & V != 0),
            0x90 => return self.branch(bus, self.p & C == 0),
            0xB0 => return self.branch(bus, self.p & C != 0),
            0xD0 => return self.branch(bus, self.p & Z == 0),
            0xF0 => return self.branch(bus, self.p & Z != 0),
            _ => {}
        }
        match cc {
            1 => {
                if bbb == 2 {
                    // immediate
                    let v = self.fetch(bus);
                    if aaa == 4 {
                        return 2; // STA #imm doesn't exist
                    }
                    if aaa == 5 {
                        self.a = v;
                        self.nz(v);
                    } else {
                        self.alu(bus, op & 0xE0, v, t);
                    }
                    return 2;
                }
                let (a, c) = self.addr(bus, bbb);
                match aaa {
                    4 => self.wr(bus, a, self.a),
                    5 => {
                        self.a = self.rd(bus, a);
                        self.nz(self.a);
                    }
                    _ => {
                        let v = self.rd(bus, a);
                        self.alu(bus, op & 0xE0, v, t);
                    }
                }
                c
            }
            2 => {
                // shifts, INC/DEC, LDX/STX
                match aaa {
                    4 | 5 => {
                        let (a, c) = match bbb {
                            0 => {
                                let v = self.fetch(bus);
                                self.x = v;
                                self.nz(v);
                                return 2;
                            }
                            1 => (0x2000 | self.fetch(bus) as u16, 4),
                            3 => (self.fetch16(bus), 5),
                            5 => (0x2000 | self.fetch(bus).wrapping_add(self.y) as u16, 4),
                            _ => (self.fetch16(bus).wrapping_add(self.y as u16), 5),
                        };
                        if aaa == 4 {
                            self.wr(bus, a, self.x);
                        } else {
                            self.x = self.rd(bus, a);
                            self.nz(self.x);
                        }
                        c
                    }
                    _ => {
                        if bbb == 2 {
                            // accumulator forms (ASL A etc.)
                            self.a = self.rmw(aaa, self.a);
                            return 2;
                        }
                        let a = match bbb {
                            1 => 0x2000 | self.fetch(bus) as u16,
                            3 => self.fetch16(bus),
                            5 => 0x2000 | self.fetch(bus).wrapping_add(self.x) as u16,
                            _ => self.fetch16(bus).wrapping_add(self.x as u16),
                        };
                        let v = self.rd(bus, a);
                        let r = self.rmw(aaa, v);
                        self.wr(bus, a, r);
                        6
                    }
                }
            }
            0 => {
                // BIT, STY, LDY, CPY, CPX
                let (a, imm) = match bbb {
                    0 => (0, Some(self.fetch(bus))),
                    1 => (0x2000 | self.fetch(bus) as u16, None),
                    3 => (self.fetch16(bus), None),
                    5 => (0x2000 | self.fetch(bus).wrapping_add(self.x) as u16, None),
                    _ => (self.fetch16(bus).wrapping_add(self.x as u16), None),
                };
                let val = |cpu: &Cpu, bus: &mut B| imm.unwrap_or_else(|| cpu.rd(bus, a));
                match aaa {
                    1 => {
                        let v = val(self, bus);
                        self.bit(v);
                    }
                    4 => self.wr(bus, a, self.y),
                    5 => {
                        self.y = val(self, bus);
                        self.nz(self.y);
                    }
                    6 => {
                        let v = val(self, bus);
                        self.cmp(self.y, v);
                    }
                    7 => {
                        let v = val(self, bus);
                        self.cmp(self.x, v);
                    }
                    _ => {}
                }
                4
            }
            _ => 2, // unused opcodes act as NOPs
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem {
        m: Vec<u8>,
        vdc: Vec<(u32, u8)>,
    }
    impl Bus for Mem {
        fn read(&mut self, a: u32) -> u8 {
            self.m[a as usize & 0x1F_FFFF]
        }
        fn write(&mut self, a: u32, v: u8) {
            self.m[a as usize & 0x1F_FFFF] = v;
        }
        fn vdc_store(&mut self, p: u32, v: u8) {
            self.vdc.push((p, v));
        }
    }

    /// Program in ROM page 0 at $E000 (MPR7 = 0), RAM page $F8 at $2000 (MPR1).
    fn run(prog: &[u8], steps: usize) -> (Cpu, Mem) {
        let mut m = Mem { m: vec![0; 0x20_0000], vdc: vec![] };
        m.m[..prog.len()].copy_from_slice(prog);
        m.m[0x1FFE] = 0x00;
        m.m[0x1FFF] = 0xE0;
        let mut c = Cpu::new();
        c.reset(&mut m);
        for _ in 0..steps {
            c.step(&mut m);
        }
        (c, m)
    }

    #[test]
    fn zero_page_is_ram_page_f8() {
        // LDA #$42 ; STA $10 ; LDX $10
        let (c, m) = run(&[0xA9, 0x42, 0x85, 0x10, 0xA6, 0x10], 3);
        assert_eq!(m.m[0xF8 << 13 | 0x10], 0x42);
        assert_eq!(c.x, 0x42);
    }

    #[test]
    fn tam_tma_and_mapping() {
        // LDA #$05 ; TAM #$04 (MPR2) ; LDA #$00 ; TMA #$04 ; STA $4001 (page 5)
        let (c, m) = run(&[0xA9, 0x05, 0x53, 0x04, 0xA9, 0x00, 0x43, 0x04, 0x8D, 0x01, 0x40], 5);
        assert_eq!(c.mpr[2], 5);
        assert_eq!(c.a, 5);
        assert_eq!(m.m[5 << 13 | 1], 5);
    }

    #[test]
    fn block_transfer_and_t_flag() {
        // TII $E100,$2000,4 ; LDX #$00 ; SET ; ADC #$10 (adds to zp[X] instead of A)
        let mut p = vec![0x73, 0x00, 0xE1, 0x00, 0x20, 0x04, 0x00, 0xA2, 0x00, 0xF4, 0x69, 0x10];
        p.resize(0x100, 0);
        p.extend([1, 2, 3, 4]);
        let (c, m) = run(&p, 4);
        let ram = 0xF8 << 13;
        assert_eq!(&m.m[ram + 1..ram + 4], &[2, 3, 4]);
        assert_eq!(m.m[ram], 0x11, "T flag: zp[X] = 1 + $10");
        assert_eq!(c.a, 0, "A untouched");
    }

    #[test]
    fn st0_st1_reach_the_vdc() {
        let (_, m) = run(&[0x03, 0x05, 0x13, 0xAA, 0x23, 0x55], 3);
        assert_eq!(m.vdc, vec![(0, 5), (2, 0xAA), (3, 0x55)]);
    }

    #[test]
    fn jsr_rts_and_decimal() {
        // JSR $E010 ; STA $00 ; BRA * ... at $E010: SED ; CLC ; LDA #$19 ; ADC #$28 ; CLD ; RTS
        let mut p = vec![0x20, 0x10, 0xE0, 0x85, 0x00, 0x80, 0xFE];
        p.resize(0x10, 0);
        p.extend([0xF8, 0x18, 0xA9, 0x19, 0x69, 0x28, 0xD8, 0x60]);
        let (_, m) = run(&p, 9);
        assert_eq!(m.m[0xF8 << 13], 0x47);
    }
}
