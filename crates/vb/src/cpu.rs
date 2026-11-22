//! NEC V810 (the Virtual Boy's CPU): 32 general registers, 16/32-bit instructions in seven
//! formats, single-precision floating point, bit-string instructions, and the Virtual Boy's
//! extra opcodes (XB, XH, REV, MPYHW).

pub trait Bus {
    fn read8(&mut self, a: u32) -> u8;
    fn read16(&mut self, a: u32) -> u16;
    fn read32(&mut self, a: u32) -> u32 {
        self.read16(a) as u32 | (self.read16(a.wrapping_add(2)) as u32) << 16
    }
    fn write8(&mut self, a: u32, v: u8);
    fn write16(&mut self, a: u32, v: u16);
    fn write32(&mut self, a: u32, v: u32) {
        self.write16(a, v as u16);
        self.write16(a.wrapping_add(2), (v >> 16) as u16);
    }
}

const Z: u32 = 1 << 0;
const S: u32 = 1 << 1;
const OV: u32 = 1 << 2;
const CY: u32 = 1 << 3;
const ID: u32 = 1 << 12;
const EP: u32 = 1 << 14;
const NP: u32 = 1 << 15;

#[derive(state::Stateful)]
pub struct Cpu {
    pub r: [u32; 32],
    pub pc: u32,
    pub psw: u32,
    pub eipc: u32,
    pub eipsw: u32,
    pub fepc: u32,
    pub fepsw: u32,
    pub ecr: u32,
    pub chcw: u32,
    pub halted: bool,
    /// Pending interrupt level (0-4), as raised by the hardware.
    pub irq: Option<u8>,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

fn sext(v: u32, bits: u32) -> u32 {
    (((v << (32 - bits)) as i32) >> (32 - bits)) as u32
}

impl Cpu {
    pub fn new() -> Cpu {
        let mut c = Cpu { r: [0; 32], pc: 0xFFFF_FFF0, psw: NP, eipc: 0, eipsw: 0, fepc: 0, fepsw: 0, ecr: 0xFFF0, chcw: 0, halted: false, irq: None };
        c.reset();
        c
    }

    pub fn reset(&mut self) {
        self.pc = 0xFFFF_FFF0;
        self.psw = NP;
        self.ecr = 0x0000_FFF0;
        self.halted = false;
    }

    fn set(&mut self, i: usize, v: u32) {
        if i != 0 {
            self.r[i] = v;
        }
    }

    fn flags_zs(&mut self, v: u32) {
        self.psw = (self.psw & !(Z | S)) | if v == 0 { Z } else { 0 } | if v & 0x8000_0000 != 0 { S } else { 0 };
    }

    fn logic(&mut self, v: u32) -> u32 {
        self.psw &= !OV;
        self.flags_zs(v);
        v
    }

    fn add(&mut self, a: u32, b: u32) -> u32 {
        let (r, c) = a.overflowing_add(b);
        let ov = (!(a ^ b) & (a ^ r)) & 0x8000_0000 != 0;
        self.psw = (self.psw & !(OV | CY)) | if ov { OV } else { 0 } | if c { CY } else { 0 };
        self.flags_zs(r);
        r
    }

    fn sub(&mut self, a: u32, b: u32) -> u32 {
        let (r, c) = a.overflowing_sub(b);
        let ov = ((a ^ b) & (a ^ r)) & 0x8000_0000 != 0;
        self.psw = (self.psw & !(OV | CY)) | if ov { OV } else { 0 } | if c { CY } else { 0 };
        self.flags_zs(r);
        r
    }

    fn shift(&mut self, v: u32, n: u32, kind: u8) -> u32 {
        let n = n & 31;
        let (r, cy) = match kind {
            0 => (v.wrapping_shl(n), n != 0 && (v >> (32 - n)) & 1 != 0),
            1 => (v.wrapping_shr(n), n != 0 && (v >> (n - 1)) & 1 != 0),
            _ => (((v as i32) >> n) as u32, n != 0 && (v >> (n - 1)) & 1 != 0),
        };
        self.psw = (self.psw & !(OV | CY)) | if cy { CY } else { 0 };
        self.flags_zs(r);
        r
    }

    fn cond(&self, c: u32) -> bool {
        let p = self.psw;
        let (z, s, ov, cy) = (p & Z != 0, p & S != 0, p & OV != 0, p & CY != 0);
        let r = match c & 7 {
            0 => ov,
            1 => cy,
            2 => z,
            3 => cy || z,
            4 => s,
            5 => true,
            6 => s != ov,
            _ => (s != ov) || z,
        };
        r != (c & 8 != 0)
    }

    fn float_flags(&mut self, f: f32) {
        self.psw &= !(Z | S | OV | CY);
        if f == 0.0 {
            self.psw |= Z;
        }
        if f < 0.0 {
            self.psw |= S | CY;
        }
    }

    /// Takes an interrupt at `level` if enabled; returns true when taken.
    fn take_irq(&mut self, level: u8) -> bool {
        let mask = ((self.psw >> 16) & 0xF) as u8;
        if self.psw & (ID | EP | NP) != 0 || level < mask {
            return false;
        }
        self.eipc = self.pc;
        self.eipsw = self.psw;
        self.ecr = (self.ecr & 0xFFFF_0000) | 0xFE00 | (level as u32) << 4;
        self.psw = (self.psw & !(0xF << 16)) | EP | ID | (((level as u32) + 1).min(15) << 16);
        self.pc = 0xFFFF_FE00 | (level as u32) << 4;
        self.halted = false;
        true
    }

    fn exception<B: Bus>(&mut self, _bus: &mut B, code: u32, vector: u32, next_pc: u32) {
        self.eipc = next_pc;
        self.eipsw = self.psw;
        self.ecr = (self.ecr & 0xFFFF_0000) | code;
        self.psw |= EP | ID;
        self.pc = vector;
    }

    /// Bit-string instructions (sub-opcodes 0-3 search, 8-15 transfer) using r26-r30.
    fn bitstring<B: Bus>(&mut self, bus: &mut B, sub: u32) -> u32 {
        if sub < 4 {
            // Search for 0/1 upward/downward: rarely used; report "not found" after scanning.
            let want = (sub >> 1) & 1;
            let up = sub & 1 == 0;
            let mut len = self.r[28];
            let mut addr = self.r[30] & !3;
            let mut off = self.r[27] & 31;
            let mut skipped = 0u32;
            while len > 0 {
                let w = bus.read32(addr);
                let bit = (w >> off) & 1;
                if bit == want {
                    self.r[30] = addr;
                    self.r[27] = off;
                    self.r[28] = len;
                    self.r[29] = self.r[29].wrapping_add(skipped);
                    self.psw &= !Z;
                    return 20;
                }
                skipped += 1;
                len -= 1;
                if up {
                    off += 1;
                    if off == 32 {
                        off = 0;
                        addr = addr.wrapping_add(4);
                    }
                } else if off == 0 {
                    off = 31;
                    addr = addr.wrapping_sub(4);
                } else {
                    off -= 1;
                }
            }
            self.r[28] = 0;
            self.r[29] = self.r[29].wrapping_add(skipped);
            self.psw |= Z;
            return 20;
        }
        let (mut src, mut dst) = (self.r[30] & !3, self.r[29] & !3);
        let (mut so, mut dof) = (self.r[27] & 31, self.r[26] & 31);
        let mut len = self.r[28];
        let mut cycles = 20;
        while len > 0 {
            let sb = (bus.read32(src) >> so) & 1;
            let dw = bus.read32(dst);
            let db = (dw >> dof) & 1;
            let r = match sub {
                8 => sb | db,
                9 => sb & db,
                10 => sb ^ db,
                11 => sb,
                12 => (sb ^ 1) | db,
                13 => (sb ^ 1) & db,
                14 => (sb ^ 1) ^ db,
                _ => sb ^ 1,
            };
            bus.write32(dst, (dw & !(1 << dof)) | (r << dof));
            len -= 1;
            so += 1;
            if so == 32 {
                so = 0;
                src = src.wrapping_add(4);
            }
            dof += 1;
            if dof == 32 {
                dof = 0;
                dst = dst.wrapping_add(4);
            }
            cycles += 1;
        }
        self.r[30] = src;
        self.r[29] = dst;
        self.r[27] = so;
        self.r[26] = dof;
        self.r[28] = 0;
        cycles
    }

    /// Executes one instruction; returns CPU cycles.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> u32 {
        if let Some(level) = self.irq {
            if self.take_irq(level) {
                self.irq = None;
                return 4;
            }
        }
        if self.halted {
            return 4;
        }
        let pc = self.pc;
        let hw = bus.read16(pc) as u32;
        let op = hw >> 10;
        let (reg2, reg1) = (((hw >> 5) & 31) as usize, (hw & 31) as usize);
        self.pc = pc.wrapping_add(2);
        let imm5 = hw & 31;
        match op {
            0x00 => {
                let v = self.r[reg1];
                self.set(reg2, v);
                1
            }
            0x01 => {
                let v = self.add(self.r[reg2], self.r[reg1]);
                self.set(reg2, v);
                1
            }
            0x02 => {
                let v = self.sub(self.r[reg2], self.r[reg1]);
                self.set(reg2, v);
                1
            }
            0x03 => {
                self.sub(self.r[reg2], self.r[reg1]);
                1
            }
            0x04 | 0x05 | 0x07 => {
                let kind = match op {
                    0x04 => 0,
                    0x05 => 1,
                    _ => 2,
                };
                let v = self.shift(self.r[reg2], self.r[reg1], kind);
                self.set(reg2, v);
                1
            }
            0x06 => {
                self.pc = self.r[reg1] & !1;
                3
            }
            0x08 | 0x0A => {
                let (a, b) = (self.r[reg2], self.r[reg1]);
                let p = if op == 0x08 { (a as i32 as i64 * b as i32 as i64) as u64 } else { a as u64 * b as u64 };
                let lo = p as u32;
                self.r[30] = (p >> 32) as u32;
                self.set(reg2, lo);
                let ov = if op == 0x08 { (p as i64) != (lo as i32 as i64) } else { p >> 32 != 0 };
                self.psw = (self.psw & !OV) | if ov { OV } else { 0 };
                self.flags_zs(lo);
                13
            }
            0x09 | 0x0B => {
                let (a, b) = (self.r[reg2], self.r[reg1]);
                if b == 0 {
                    self.exception(bus, 0xFF80, 0xFFFF_FF80, self.pc);
                    return 38;
                }
                let (q, rem, ov) = if op == 0x09 {
                    if a == 0x8000_0000 && b == 0xFFFF_FFFF {
                        (a, 0, true)
                    } else {
                        (((a as i32) / (b as i32)) as u32, ((a as i32) % (b as i32)) as u32, false)
                    }
                } else {
                    (a / b, a % b, false)
                };
                self.r[30] = rem;
                self.set(reg2, q);
                self.psw = (self.psw & !OV) | if ov { OV } else { 0 };
                self.flags_zs(q);
                36
            }
            0x0C => {
                let v = self.logic(self.r[reg2] | self.r[reg1]);
                self.set(reg2, v);
                1
            }
            0x0D => {
                let v = self.logic(self.r[reg2] & self.r[reg1]);
                self.set(reg2, v);
                1
            }
            0x0E => {
                let v = self.logic(self.r[reg2] ^ self.r[reg1]);
                self.set(reg2, v);
                1
            }
            0x0F => {
                let v = self.logic(!self.r[reg1]);
                self.set(reg2, v);
                1
            }
            // ---- Format II (5-bit immediate)
            0x10 => {
                self.set(reg2, sext(imm5, 5));
                1
            }
            0x11 => {
                let v = self.add(self.r[reg2], sext(imm5, 5));
                self.set(reg2, v);
                1
            }
            0x12 => {
                let v = self.cond(imm5 & 0xF) as u32;
                self.set(reg2, v);
                1
            }
            0x13 => {
                self.sub(self.r[reg2], sext(imm5, 5));
                1
            }
            0x14 | 0x15 | 0x17 => {
                let kind = match op {
                    0x14 => 0,
                    0x15 => 1,
                    _ => 2,
                };
                let v = self.shift(self.r[reg2], imm5, kind);
                self.set(reg2, v);
                1
            }
            0x16 => {
                // CLI
                self.psw &= !ID;
                12
            }
            0x18 => {
                // TRAP
                let vector = if imm5 < 16 { 0xFFFF_FFA0 } else { 0xFFFF_FFB0 };
                self.exception(bus, 0xFFA0 + imm5, vector, self.pc);
                15
            }
            0x19 => {
                // RETI
                if self.psw & NP != 0 {
                    self.pc = self.fepc;
                    self.psw = self.fepsw;
                } else {
                    self.pc = self.eipc;
                    self.psw = self.eipsw;
                }
                10
            }
            0x1A => {
                self.halted = true;
                1
            }
            0x1C => {
                // LDSR reg2 -> system register imm5
                let v = self.r[reg2];
                match imm5 {
                    0 => self.eipc = v,
                    1 => self.eipsw = v,
                    2 => self.fepc = v,
                    3 => self.fepsw = v,
                    5 => self.psw = v,
                    24 => self.chcw = v,
                    _ => {}
                }
                8
            }
            0x1D => {
                let v = match imm5 {
                    0 => self.eipc,
                    1 => self.eipsw,
                    2 => self.fepc,
                    3 => self.fepsw,
                    4 => self.ecr,
                    5 => self.psw,
                    6 => 0x0000_5346, // PIR: V810
                    7 => 0x0000_00E0, // TKCW
                    24 => self.chcw,
                    _ => 0,
                };
                self.set(reg2, v);
                8
            }
            0x1E => {
                // SEI
                self.psw |= ID;
                12
            }
            0x1F => self.bitstring(bus, imm5),
            // ---- Format III: conditional branch, 9-bit displacement
            0x20..=0x27 => {
                let c = (hw >> 9) & 0xF;
                if self.cond(c) {
                    self.pc = pc.wrapping_add(sext(hw & 0x1FF, 9));
                    3
                } else {
                    1
                }
            }
            _ => {
                let hw2 = bus.read16(pc.wrapping_add(2)) as u32;
                self.pc = pc.wrapping_add(4);
                let imm16 = hw2;
                let disp = sext(hw2, 16);
                match op {
                    0x28 => {
                        let v = self.r[reg1].wrapping_add(disp);
                        self.set(reg2, v);
                        1
                    }
                    0x29 => {
                        let v = self.add(self.r[reg1], disp);
                        self.set(reg2, v);
                        1
                    }
                    0x2A | 0x2B => {
                        let d = sext((hw & 0x3FF) << 16 | hw2, 26);
                        if op == 0x2B {
                            self.r[31] = self.pc;
                        }
                        self.pc = pc.wrapping_add(d);
                        3
                    }
                    0x2C => {
                        let v = self.logic(self.r[reg1] | imm16);
                        self.set(reg2, v);
                        1
                    }
                    0x2D => {
                        let v = self.logic(self.r[reg1] & imm16);
                        self.set(reg2, v);
                        1
                    }
                    0x2E => {
                        let v = self.logic(self.r[reg1] ^ imm16);
                        self.set(reg2, v);
                        1
                    }
                    0x2F => {
                        let v = self.r[reg1].wrapping_add(imm16 << 16);
                        self.set(reg2, v);
                        1
                    }
                    0x30 | 0x38 => {
                        let v = bus.read8(self.r[reg1].wrapping_add(disp));
                        self.set(reg2, if op == 0x30 { v as i8 as i32 as u32 } else { v as u32 });
                        4
                    }
                    0x31 | 0x39 => {
                        let v = bus.read16(self.r[reg1].wrapping_add(disp) & !1);
                        self.set(reg2, if op == 0x31 { v as i16 as i32 as u32 } else { v as u32 });
                        4
                    }
                    0x33 | 0x3B => {
                        let v = bus.read32(self.r[reg1].wrapping_add(disp) & !3);
                        self.set(reg2, v);
                        4
                    }
                    0x34 | 0x3C => {
                        bus.write8(self.r[reg1].wrapping_add(disp), self.r[reg2] as u8);
                        4
                    }
                    0x35 | 0x3D => {
                        bus.write16(self.r[reg1].wrapping_add(disp) & !1, self.r[reg2] as u16);
                        4
                    }
                    0x37 | 0x3F => {
                        bus.write32(self.r[reg1].wrapping_add(disp) & !3, self.r[reg2]);
                        4
                    }
                    0x3A => {
                        // CAXI: compare and exchange with r30
                        let a = self.r[reg1].wrapping_add(disp) & !3;
                        let m = bus.read32(a);
                        self.sub(self.r[reg2], m);
                        if self.psw & Z != 0 {
                            bus.write32(a, self.r[30]);
                        } else {
                            bus.write32(a, m);
                        }
                        self.set(reg2, m);
                        26
                    }
                    0x3E => self.format7(reg1, reg2, hw2 >> 10),
                    _ => {
                        // Unused opcode: invalid-opcode exception.
                        self.exception(bus, 0xFF90, 0xFFFF_FF90, self.pc);
                        1
                    }
                }
            }
        }
    }

    fn format7(&mut self, reg1: usize, reg2: usize, sub: u32) -> u32 {
        let (a, b) = (f32::from_bits(self.r[reg2]), f32::from_bits(self.r[reg1]));
        match sub {
            0x00 => {
                self.float_flags(a - b);
                10
            }
            0x02 => {
                // CVT.WS: int -> float
                let f = self.r[reg1] as i32 as f32;
                self.float_flags(f);
                self.set(reg2, f.to_bits());
                16
            }
            0x03 | 0x0B => {
                // CVT.SW (round) / TRNC.SW (truncate)
                let v = if sub == 0x03 { b.round() } else { b.trunc() };
                let i = v as i32 as u32;
                self.set(reg2, i);
                self.flags_zs(i);
                self.psw &= !OV;
                14
            }
            0x04..=0x07 => {
                let r = match sub {
                    0x04 => a + b,
                    0x05 => a - b,
                    0x06 => a * b,
                    _ => {
                        if b == 0.0 {
                            f32::INFINITY
                        } else {
                            a / b
                        }
                    }
                };
                self.float_flags(r);
                self.set(reg2, r.to_bits());
                30
            }
            0x08 => {
                // XB: swap the low two bytes
                let v = self.r[reg2];
                self.set(reg2, (v & 0xFFFF_0000) | (v & 0xFF) << 8 | (v >> 8) & 0xFF);
                6
            }
            0x09 => {
                // XH: swap halfwords
                let v = self.r[reg2];
                self.set(reg2, v.rotate_left(16));
                1
            }
            0x0A => {
                // REV: reverse bits
                let v = self.r[reg1].reverse_bits();
                self.set(reg2, v);
                22
            }
            0x0C => {
                // MPYHW: 16 x 17-bit signed multiply
                let v = ((self.r[reg2] as i16 as i32).wrapping_mul(sext(self.r[reg1] & 0x1FFFF, 17) as i32)) as u32;
                self.set(reg2, v);
                9
            }
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<u8>);
    impl Bus for Mem {
        fn read8(&mut self, a: u32) -> u8 {
            self.0[(a & 0xFFFF) as usize]
        }
        fn read16(&mut self, a: u32) -> u16 {
            let i = (a & 0xFFFE) as usize;
            u16::from_le_bytes([self.0[i], self.0[i + 1]])
        }
        fn write8(&mut self, a: u32, v: u8) {
            self.0[(a & 0xFFFF) as usize] = v;
        }
        fn write16(&mut self, a: u32, v: u16) {
            let i = (a & 0xFFFE) as usize;
            self.0[i..i + 2].copy_from_slice(&v.to_le_bytes());
        }
    }

    fn i1(op: u32, reg2: u32, reg1: u32) -> Vec<u8> {
        ((op << 10 | reg2 << 5 | reg1) as u16).to_le_bytes().to_vec()
    }
    fn i5(op: u32, reg2: u32, reg1: u32, imm: u16) -> Vec<u8> {
        let mut v = i1(op, reg2, reg1);
        v.extend(imm.to_le_bytes());
        v
    }

    fn run(prog: Vec<u8>, steps: usize) -> (Cpu, Mem) {
        let mut m = Mem(vec![0; 0x1_0000]);
        m.0[..prog.len()].copy_from_slice(&prog);
        let mut c = Cpu::new();
        c.pc = 0;
        c.psw = 0;
        for _ in 0..steps {
            c.step(&mut m);
        }
        (c, m)
    }

    #[test]
    fn movhi_movea_add_and_flags() {
        // MOVHI 0x1234,r0,r10 ; MOVEA 0x5678,r10,r10 ; MOV -1,r11 ; ADD r11,r10
        let mut p = i5(0x2F, 10, 0, 0x1234);
        p.extend(i5(0x28, 10, 10, 0x5678));
        p.extend(i1(0x10, 11, 0x1F));
        p.extend(i1(0x01, 10, 11));
        let (c, _) = run(p, 4);
        assert_eq!(c.r[10], 0x1234_5677);
        assert!(c.psw & CY != 0, "adding -1 carries");
    }

    #[test]
    fn loop_with_branch_and_store() {
        // MOV 5,r5 ; MOV 0,r6 ; loop: ADD 3,r6 ; ADD -1,r5 ; BNZ loop ; ST.W r6,0x100[r0]
        let mut p = i1(0x10, 5, 5);
        p.extend(i1(0x10, 6, 0));
        p.extend(i1(0x11, 6, 3));
        p.extend(i1(0x11, 5, 0x1F));
        // BNZ (cond 0xA) back 4 bytes: 100 1010 disp9
        let disp = (-4i32 as u32) & 0x1FF;
        p.extend(((0x4 << 13 | 0xA << 9 | disp) as u16).to_le_bytes());
        p.extend(i5(0x37, 6, 0, 0x100));
        let (c, m) = run(p, 2 + 15 + 1);
        assert_eq!(c.r[6], 15);
        assert_eq!(u32::from_le_bytes(m.0[0x100..0x104].try_into().unwrap()), 15);
    }

    #[test]
    fn jal_and_jmp_return() {
        // JAL +8 ; HALT ; pad ; (8:) MOV 7,r1 ; JMP [r31]
        let mut p = vec![];
        let d: u32 = 8;
        p.extend(((0x2B << 10 | (d >> 16) & 0x3FF) as u16).to_le_bytes());
        p.extend((d as u16).to_le_bytes());
        p.extend(i1(0x1A, 0, 0));
        p.extend([0, 0]);
        p.extend(i1(0x10, 1, 7));
        p.extend(i1(0x06, 0, 31));
        let (c, _) = run(p, 4);
        assert_eq!(c.r[1], 7);
        assert!(c.halted);
    }

    #[test]
    fn float_ops() {
        // r1 = 3.0f, r2 = 0.5f (preset) ; MULF.S r2,r1 ; CVT.SW r1 -> r3
        let mut p = i5(0x3E, 1, 2, 0x06 << 10);
        p.extend(i5(0x3E, 3, 1, 0x03 << 10));
        let mut m = Mem(vec![0; 0x1_0000]);
        m.0[..p.len()].copy_from_slice(&p);
        let mut c = Cpu::new();
        c.pc = 0;
        c.psw = 0;
        c.r[1] = 3.0f32.to_bits();
        c.r[2] = 0.5f32.to_bits();
        c.step(&mut m);
        c.step(&mut m);
        assert_eq!(f32::from_bits(c.r[1]), 1.5);
        assert_eq!(c.r[3], 2, "1.5 rounds to 2");
    }

    #[test]
    fn interrupt_and_reti() {
        let mut m = Mem(vec![0; 0x1_0000]);
        // Handler for level 4 at 0xFFFFFE40 (masked to 0xFE40): MOV 9,r4 ; RETI
        let h = [i1(0x10, 4, 9), i1(0x19, 0, 0)].concat();
        m.0[0xFE40..0xFE40 + h.len()].copy_from_slice(&h);
        let mut c = Cpu::new();
        c.pc = 0;
        c.psw = 0;
        c.irq = Some(4);
        c.step(&mut m); // takes the interrupt
        c.step(&mut m);
        c.step(&mut m); // RETI
        assert_eq!(c.r[4], 9);
        assert_eq!(c.pc, 0);
        assert_eq!(c.psw & (EP | ID), 0);
    }
}
