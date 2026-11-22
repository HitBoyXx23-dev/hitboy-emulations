//! NEC V30MZ: an 8086/80186-compatible 16-bit CPU (the WonderSwan's). Full 8086 instruction
//! set plus the 80186 additions (PUSHA/POPA, PUSH imm, IMUL imm, shifts by imm, ENTER/LEAVE,
//! BOUND, INS/OUTS), segment overrides and REP prefixes.

pub trait Bus {
    fn read(&mut self, addr: u32) -> u8;
    fn write(&mut self, addr: u32, v: u8);
    fn input(&mut self, port: u16) -> u8;
    fn output(&mut self, port: u16, v: u8);
}

pub const CF: u16 = 1 << 0;
pub const PF: u16 = 1 << 2;
pub const AF: u16 = 1 << 4;
pub const ZF: u16 = 1 << 6;
pub const SF: u16 = 1 << 7;
pub const TF: u16 = 1 << 8;
pub const IF: u16 = 1 << 9;
pub const DF: u16 = 1 << 10;
pub const OF: u16 = 1 << 11;

pub const AX: usize = 0;
pub const CX: usize = 1;
pub const DX: usize = 2;
pub const BX: usize = 3;
pub const SP: usize = 4;
pub const BP: usize = 5;
pub const SI: usize = 6;
pub const DI: usize = 7;
pub const ES: usize = 0;
pub const CS: usize = 1;
pub const SS: usize = 2;
pub const DS: usize = 3;

#[derive(Clone, Copy)]
enum Loc {
    Reg(usize),
    Mem(u32),
}

#[derive(state::Stateful)]
pub struct Cpu {
    pub r: [u16; 8],
    pub seg: [u16; 4],
    pub ip: u16,
    pub flags: u16,
    pub halted: bool,
    /// Segment override for the current instruction.
    ovr: Option<usize>,
    /// Pending maskable interrupt vector.
    pub irq: Option<u8>,
    /// Cycles of the current instruction.
    cyc: u32,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

fn parity(v: u8) -> bool {
    v.count_ones() % 2 == 0
}

impl Cpu {
    pub fn new() -> Cpu {
        Cpu { r: [0; 8], seg: [0, 0xFFFF, 0, 0], ip: 0, flags: 0xF002, halted: false, ovr: None, irq: None, cyc: 0 }
    }

    pub fn reset(&mut self) {
        *self = Cpu::new();
    }

    fn lin(seg: u16, off: u16) -> u32 {
        (((seg as u32) << 4) + off as u32) & 0xF_FFFF
    }

    fn fetch<B: Bus>(&mut self, bus: &mut B) -> u8 {
        let v = bus.read(Self::lin(self.seg[CS], self.ip));
        self.ip = self.ip.wrapping_add(1);
        v
    }
    fn fetch16<B: Bus>(&mut self, bus: &mut B) -> u16 {
        let lo = self.fetch(bus) as u16;
        lo | (self.fetch(bus) as u16) << 8
    }
    fn rd16<B: Bus>(bus: &mut B, a: u32) -> u16 {
        bus.read(a) as u16 | (bus.read((a + 1) & 0xF_FFFF) as u16) << 8
    }
    fn wr16<B: Bus>(bus: &mut B, a: u32, v: u16) {
        bus.write(a, v as u8);
        bus.write((a + 1) & 0xF_FFFF, (v >> 8) as u8);
    }
    fn push<B: Bus>(&mut self, bus: &mut B, v: u16) {
        self.r[SP] = self.r[SP].wrapping_sub(2);
        Self::wr16(bus, Self::lin(self.seg[SS], self.r[SP]), v);
    }
    fn pop<B: Bus>(&mut self, bus: &mut B) -> u16 {
        let v = Self::rd16(bus, Self::lin(self.seg[SS], self.r[SP]));
        self.r[SP] = self.r[SP].wrapping_add(2);
        v
    }

    fn get8(&self, i: usize) -> u8 {
        if i < 4 {
            self.r[i] as u8
        } else {
            (self.r[i - 4] >> 8) as u8
        }
    }
    fn set8(&mut self, i: usize, v: u8) {
        if i < 4 {
            self.r[i] = (self.r[i] & 0xFF00) | v as u16;
        } else {
            self.r[i - 4] = (self.r[i - 4] & 0x00FF) | (v as u16) << 8;
        }
    }

    fn flag(&mut self, f: u16, on: bool) {
        if on {
            self.flags |= f
        } else {
            self.flags &= !f
        }
    }
    fn szp8(&mut self, v: u8) {
        self.flag(SF, v & 0x80 != 0);
        self.flag(ZF, v == 0);
        self.flag(PF, parity(v));
    }
    fn szp16(&mut self, v: u16) {
        self.flag(SF, v & 0x8000 != 0);
        self.flag(ZF, v == 0);
        self.flag(PF, parity(v as u8));
    }

    /// Decodes a ModRM byte: returns (reg field, operand location).
    fn modrm<B: Bus>(&mut self, bus: &mut B) -> (usize, Loc) {
        let m = self.fetch(bus);
        let (md, reg, rm) = (m >> 6, ((m >> 3) & 7) as usize, (m & 7) as usize);
        if md == 3 {
            return (reg, Loc::Reg(rm));
        }
        let r = &self.r;
        let (base, bp_based) = match rm {
            0 => (r[BX].wrapping_add(r[SI]), false),
            1 => (r[BX].wrapping_add(r[DI]), false),
            2 => (r[BP].wrapping_add(r[SI]), true),
            3 => (r[BP].wrapping_add(r[DI]), true),
            4 => (r[SI], false),
            5 => (r[DI], false),
            6 => (r[BP], true),
            _ => (r[BX], false),
        };
        let (off, bp_based) = match md {
            0 if rm == 6 => (self.fetch16(bus), false),
            0 => (base, bp_based),
            1 => (base.wrapping_add(self.fetch(bus) as i8 as u16), bp_based),
            _ => (base.wrapping_add(self.fetch16(bus)), bp_based),
        };
        let seg = self.ovr.unwrap_or(if bp_based { SS } else { DS });
        self.cyc += 1;
        (reg, Loc::Mem(Self::lin(self.seg[seg], off)))
    }

    /// Offset part of a memory ModRM (for LEA/LES/LDS).
    fn modrm_offset<B: Bus>(&mut self, bus: &mut B) -> (usize, u16, u32) {
        let save_ip = self.ip;
        let m = bus.read(Self::lin(self.seg[CS], save_ip));
        let (reg, loc) = self.modrm(bus);
        let Loc::Mem(lin) = loc else { return (reg, 0, 0) };
        // Recompute the 16-bit offset for LEA.
        let r = &self.r;
        let (md, rm) = (m >> 6, m & 7);
        let base = match rm {
            0 => r[BX].wrapping_add(r[SI]),
            1 => r[BX].wrapping_add(r[DI]),
            2 => r[BP].wrapping_add(r[SI]),
            3 => r[BP].wrapping_add(r[DI]),
            4 => r[SI],
            5 => r[DI],
            6 => r[BP],
            _ => r[BX],
        };
        let disp_at = save_ip.wrapping_add(1);
        let off = match md {
            0 if rm == 6 => Self::rd16(bus, Self::lin(self.seg[CS], disp_at)),
            0 => base,
            1 => base.wrapping_add(bus.read(Self::lin(self.seg[CS], disp_at)) as i8 as u16),
            _ => base.wrapping_add(Self::rd16(bus, Self::lin(self.seg[CS], disp_at))),
        };
        (reg, off, lin)
    }

    fn ld8<B: Bus>(&self, bus: &mut B, l: Loc) -> u8 {
        match l {
            Loc::Reg(i) => self.get8(i),
            Loc::Mem(a) => bus.read(a),
        }
    }
    fn st8<B: Bus>(&mut self, bus: &mut B, l: Loc, v: u8) {
        match l {
            Loc::Reg(i) => self.set8(i, v),
            Loc::Mem(a) => bus.write(a, v),
        }
    }
    fn ld16<B: Bus>(&self, bus: &mut B, l: Loc) -> u16 {
        match l {
            Loc::Reg(i) => self.r[i],
            Loc::Mem(a) => Self::rd16(bus, a),
        }
    }
    fn st16<B: Bus>(&mut self, bus: &mut B, l: Loc, v: u16) {
        match l {
            Loc::Reg(i) => self.r[i] = v,
            Loc::Mem(a) => Self::wr16(bus, a, v),
        }
    }

    // ---------------------------------------------------------------- ALU

    fn alu8(&mut self, op: usize, a: u8, b: u8) -> u8 {
        let c = (self.flags & CF) as u16;
        let (r, carry, over, aux) = match op {
            0 | 2 => {
                let cin = if op == 2 { c } else { 0 };
                let s = a as u16 + b as u16 + cin;
                let r = s as u8;
                (r, s > 0xFF, (!(a ^ b) & (a ^ r)) & 0x80 != 0, ((a & 0xF) + (b & 0xF) + cin as u8) > 0xF)
            }
            3 | 5 | 7 => {
                let cin = if op == 3 { c } else { 0 };
                let s = (a as u16).wrapping_sub(b as u16).wrapping_sub(cin);
                let r = s as u8;
                (r, s > 0xFF, ((a ^ b) & (a ^ r)) & 0x80 != 0, (a & 0xF) < (b & 0xF) + cin as u8)
            }
            1 => (a | b, false, false, false),
            4 => (a & b, false, false, false),
            _ => (a ^ b, false, false, false),
        };
        self.flag(CF, carry);
        self.flag(OF, over);
        self.flag(AF, aux);
        self.szp8(r);
        if op == 7 {
            a
        } else {
            r
        }
    }

    fn alu16(&mut self, op: usize, a: u16, b: u16) -> u16 {
        let c = (self.flags & CF) as u32;
        let (r, carry, over, aux) = match op {
            0 | 2 => {
                let cin = if op == 2 { c } else { 0 };
                let s = a as u32 + b as u32 + cin;
                let r = s as u16;
                (r, s > 0xFFFF, (!(a ^ b) & (a ^ r)) & 0x8000 != 0, ((a & 0xF) + (b & 0xF) + cin as u16) > 0xF)
            }
            3 | 5 | 7 => {
                let cin = if op == 3 { c } else { 0 };
                let s = (a as u32).wrapping_sub(b as u32).wrapping_sub(cin);
                let r = s as u16;
                (r, s > 0xFFFF, ((a ^ b) & (a ^ r)) & 0x8000 != 0, (a & 0xF) < (b & 0xF) + cin as u16)
            }
            1 => (a | b, false, false, false),
            4 => (a & b, false, false, false),
            _ => (a ^ b, false, false, false),
        };
        self.flag(CF, carry);
        self.flag(OF, over);
        self.flag(AF, aux);
        self.szp16(r);
        if op == 7 {
            a
        } else {
            r
        }
    }

    fn inc8(&mut self, v: u8, dec: bool) -> u8 {
        let r = if dec { v.wrapping_sub(1) } else { v.wrapping_add(1) };
        self.flag(OF, if dec { v == 0x80 } else { v == 0x7F });
        self.flag(AF, if dec { v & 0xF == 0 } else { v & 0xF == 0xF });
        self.szp8(r);
        r
    }
    fn inc16(&mut self, v: u16, dec: bool) -> u16 {
        let r = if dec { v.wrapping_sub(1) } else { v.wrapping_add(1) };
        self.flag(OF, if dec { v == 0x8000 } else { v == 0x7FFF });
        self.flag(AF, if dec { v & 0xF == 0 } else { v & 0xF == 0xF });
        self.szp16(r);
        r
    }

    /// Rotate/shift group (ROL ROR RCL RCR SHL SHR SAL SAR), `wide` = 16-bit.
    fn shift(&mut self, op: usize, v: u32, count: u8, wide: bool) -> u32 {
        let bits = if wide { 16 } else { 8 };
        let mask = if wide { 0xFFFF } else { 0xFF };
        let msb = 1u32 << (bits - 1);
        let n = count & 0x1F;
        if n == 0 {
            return v;
        }
        let mut r = v;
        let mut cf = self.flags & CF != 0;
        for _ in 0..n {
            match op {
                0 => {
                    cf = r & msb != 0;
                    r = ((r << 1) | cf as u32) & mask;
                }
                1 => {
                    cf = r & 1 != 0;
                    r = (r >> 1) | if cf { msb } else { 0 };
                }
                2 => {
                    let out = r & msb != 0;
                    r = ((r << 1) | cf as u32) & mask;
                    cf = out;
                }
                3 => {
                    let out = r & 1 != 0;
                    r = (r >> 1) | if cf { msb } else { 0 };
                    cf = out;
                }
                4 | 6 => {
                    cf = r & msb != 0;
                    r = (r << 1) & mask;
                }
                5 => {
                    cf = r & 1 != 0;
                    r >>= 1;
                }
                _ => {
                    cf = r & 1 != 0;
                    r = (r >> 1) | (r & msb);
                }
            }
        }
        self.flag(CF, cf);
        let top = r & msb != 0;
        let of = match op {
            0 | 2 | 4 | 6 => top != cf,
            1 | 3 => top != ((r & (msb >> 1)) != 0),
            5 => v & msb != 0,
            _ => false,
        };
        self.flag(OF, of);
        if op >= 4 {
            if wide {
                self.szp16(r as u16)
            } else {
                self.szp8(r as u8)
            }
        }
        r
    }

    fn interrupt<B: Bus>(&mut self, bus: &mut B, n: u8) {
        let f = self.flags;
        self.push(bus, f);
        self.flags &= !(IF | TF);
        let cs = self.seg[CS];
        self.push(bus, cs);
        let ip = self.ip;
        self.push(bus, ip);
        let v = (n as u32) * 4;
        self.ip = Self::rd16(bus, v);
        self.seg[CS] = Self::rd16(bus, v + 2);
        self.halted = false;
    }

    fn cond(&self, c: u8) -> bool {
        let f = self.flags;
        let (cf, zf, sf, of, pf) = (f & CF != 0, f & ZF != 0, f & SF != 0, f & OF != 0, f & PF != 0);
        let r = match c >> 1 {
            0 => of,
            1 => cf,
            2 => zf,
            3 => cf || zf,
            4 => sf,
            5 => pf,
            6 => sf != of,
            _ => zf || (sf != of),
        };
        r != (c & 1 != 0)
    }

    // ---------------------------------------------------------------- string ops

    fn string_op<B: Bus>(&mut self, bus: &mut B, op: u8, rep: Option<bool>) {
        let wide = op & 1 != 0;
        let step: u16 = if wide { 2 } else { 1 };
        let delta = |cpu: &Cpu| if cpu.flags & DF != 0 { step.wrapping_neg() } else { step };
        let src_seg = self.ovr.unwrap_or(DS);
        loop {
            if rep.is_some() && self.r[CX] == 0 {
                break;
            }
            let si = Self::lin(self.seg[src_seg], self.r[SI]);
            let di = Self::lin(self.seg[ES], self.r[DI]);
            let d = delta(self);
            let mut stop = false;
            match op & 0xFE {
                0xA4 => {
                    // MOVS
                    if wide {
                        let v = Self::rd16(bus, si);
                        Self::wr16(bus, di, v);
                    } else {
                        let v = bus.read(si);
                        bus.write(di, v);
                    }
                    self.r[SI] = self.r[SI].wrapping_add(d);
                    self.r[DI] = self.r[DI].wrapping_add(d);
                }
                0xA6 => {
                    // CMPS
                    if wide {
                        let (a, b) = (Self::rd16(bus, si), Self::rd16(bus, di));
                        self.alu16(7, a, b);
                    } else {
                        let (a, b) = (bus.read(si), bus.read(di));
                        self.alu8(7, a, b);
                    }
                    self.r[SI] = self.r[SI].wrapping_add(d);
                    self.r[DI] = self.r[DI].wrapping_add(d);
                    stop = true;
                }
                0xAA => {
                    // STOS
                    if wide {
                        Self::wr16(bus, di, self.r[AX]);
                    } else {
                        bus.write(di, self.r[AX] as u8);
                    }
                    self.r[DI] = self.r[DI].wrapping_add(d);
                }
                0xAC => {
                    // LODS
                    if wide {
                        self.r[AX] = Self::rd16(bus, si);
                    } else {
                        let v = bus.read(si);
                        self.set8(0, v);
                    }
                    self.r[SI] = self.r[SI].wrapping_add(d);
                }
                0xAE => {
                    // SCAS
                    if wide {
                        let b = Self::rd16(bus, di);
                        self.alu16(7, self.r[AX], b);
                    } else {
                        let b = bus.read(di);
                        self.alu8(7, self.r[AX] as u8, b);
                    }
                    self.r[DI] = self.r[DI].wrapping_add(d);
                    stop = true;
                }
                0x6C => {
                    // INS
                    let v = bus.input(self.r[DX]);
                    bus.write(di, v);
                    if wide {
                        let v = bus.input(self.r[DX].wrapping_add(1));
                        bus.write((di + 1) & 0xF_FFFF, v);
                    }
                    self.r[DI] = self.r[DI].wrapping_add(d);
                }
                _ => {
                    // OUTS
                    let v = bus.read(si);
                    bus.output(self.r[DX], v);
                    if wide {
                        let v = bus.read((si + 1) & 0xF_FFFF);
                        bus.output(self.r[DX].wrapping_add(1), v);
                    }
                    self.r[SI] = self.r[SI].wrapping_add(d);
                }
            }
            self.cyc += 2;
            let Some(z) = rep else { break };
            self.r[CX] = self.r[CX].wrapping_sub(1);
            if stop && (self.flags & ZF != 0) != z {
                break;
            }
        }
    }

    // ---------------------------------------------------------------- execution

    /// Executes one instruction (or takes an interrupt); returns cycles.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> u32 {
        if let Some(n) = self.irq {
            if self.flags & IF != 0 {
                self.irq = None;
                self.interrupt(bus, n);
                return 10;
            }
        }
        if self.halted {
            return 4;
        }
        self.ovr = None;
        self.cyc = 1;
        let mut rep: Option<bool> = None;
        let mut op = self.fetch(bus);
        loop {
            match op {
                0x26 => self.ovr = Some(ES),
                0x2E => self.ovr = Some(CS),
                0x36 => self.ovr = Some(SS),
                0x3E => self.ovr = Some(DS),
                0xF0 => {}
                0xF2 => rep = Some(false),
                0xF3 => rep = Some(true),
                _ => break,
            }
            op = self.fetch(bus);
        }
        self.exec(bus, op, rep);
        self.cyc
    }

    fn exec<B: Bus>(&mut self, bus: &mut B, op: u8, rep: Option<bool>) {
        match op {
            // ALU r/m, reg forms: 00-05, 08-0D, ... 38-3D
            _ if op < 0x40 && op & 7 < 6 => {
                let kind = (op >> 3) as usize;
                match op & 7 {
                    0 => {
                        let (reg, l) = self.modrm(bus);
                        let a = self.ld8(bus, l);
                        let r = self.alu8(kind, a, self.get8(reg));
                        if kind != 7 {
                            self.st8(bus, l, r);
                        }
                    }
                    1 => {
                        let (reg, l) = self.modrm(bus);
                        let a = self.ld16(bus, l);
                        let r = self.alu16(kind, a, self.r[reg]);
                        if kind != 7 {
                            self.st16(bus, l, r);
                        }
                    }
                    2 => {
                        let (reg, l) = self.modrm(bus);
                        let b = self.ld8(bus, l);
                        let r = self.alu8(kind, self.get8(reg), b);
                        if kind != 7 {
                            self.set8(reg, r);
                        }
                    }
                    3 => {
                        let (reg, l) = self.modrm(bus);
                        let b = self.ld16(bus, l);
                        let r = self.alu16(kind, self.r[reg], b);
                        if kind != 7 {
                            self.r[reg] = r;
                        }
                    }
                    4 => {
                        let b = self.fetch(bus);
                        let r = self.alu8(kind, self.r[AX] as u8, b);
                        if kind != 7 {
                            self.set8(0, r);
                        }
                    }
                    _ => {
                        let b = self.fetch16(bus);
                        let r = self.alu16(kind, self.r[AX], b);
                        if kind != 7 {
                            self.r[AX] = r;
                        }
                    }
                }
            }
            0x06 | 0x0E | 0x16 | 0x1E => {
                let v = self.seg[(op >> 3) as usize];
                self.push(bus, v);
            }
            0x07 | 0x17 | 0x1F => {
                let v = self.pop(bus);
                self.seg[(op >> 3) as usize] = v;
            }
            0x0F => {
                // POP CS doesn't exist on the V30MZ; treat as a NOP.
            }
            0x27 | 0x2F => {
                // DAA / DAS
                let a = self.r[AX] as u8;
                let (mut r, mut cf) = (a, self.flags & CF != 0);
                let af = self.flags & AF != 0 || a & 0xF > 9;
                if af {
                    r = if op == 0x27 { r.wrapping_add(6) } else { r.wrapping_sub(6) };
                }
                if cf || a > 0x99 {
                    r = if op == 0x27 { r.wrapping_add(0x60) } else { r.wrapping_sub(0x60) };
                    cf = true;
                }
                self.set8(0, r);
                self.flag(AF, af);
                self.flag(CF, cf);
                self.szp8(r);
            }
            0x37 | 0x3F => {
                // AAA / AAS
                let al = self.r[AX] as u8;
                if al & 0xF > 9 || self.flags & AF != 0 {
                    let ax = if op == 0x37 { self.r[AX].wrapping_add(0x106) } else { self.r[AX].wrapping_sub(6).wrapping_sub(0x100) };
                    self.r[AX] = ax;
                    self.flag(AF, true);
                    self.flag(CF, true);
                } else {
                    self.flag(AF, false);
                    self.flag(CF, false);
                }
                self.r[AX] &= 0xFF0F;
            }
            0x40..=0x47 => {
                let i = (op & 7) as usize;
                self.r[i] = self.inc16(self.r[i], false);
            }
            0x48..=0x4F => {
                let i = (op & 7) as usize;
                self.r[i] = self.inc16(self.r[i], true);
            }
            0x50..=0x57 => {
                let i = (op & 7) as usize;
                // PUSH SP pushes the value before the decrement on 186+.
                let v = self.r[i];
                self.push(bus, v);
            }
            0x58..=0x5F => {
                let v = self.pop(bus);
                self.r[(op & 7) as usize] = v;
            }
            0x60 => {
                let sp = self.r[SP];
                for i in 0..8 {
                    let v = if i == SP { sp } else { self.r[i] };
                    self.push(bus, v);
                }
            }
            0x61 => {
                for i in (0..8).rev() {
                    let v = self.pop(bus);
                    if i != SP {
                        self.r[i] = v;
                    }
                }
            }
            0x62 => {
                let (reg, _, lin) = self.modrm_offset(bus);
                let lo = Self::rd16(bus, lin) as i16;
                let hi = Self::rd16(bus, (lin + 2) & 0xF_FFFF) as i16;
                let v = self.r[reg] as i16;
                if v < lo || v > hi {
                    self.interrupt(bus, 5);
                }
            }
            0x68 => {
                let v = self.fetch16(bus);
                self.push(bus, v);
            }
            0x6A => {
                let v = self.fetch(bus) as i8 as u16;
                self.push(bus, v);
            }
            0x69 | 0x6B => {
                let (reg, l) = self.modrm(bus);
                let a = self.ld16(bus, l) as i16 as i32;
                let b = if op == 0x69 { self.fetch16(bus) as i16 as i32 } else { self.fetch(bus) as i8 as i32 };
                let p = a * b;
                self.r[reg] = p as u16;
                let over = p != (p as i16) as i32;
                self.flag(CF, over);
                self.flag(OF, over);
                self.cyc += 3;
            }
            0x6C..=0x6F => self.string_op(bus, op, rep),
            0x70..=0x7F => {
                let d = self.fetch(bus) as i8;
                if self.cond(op & 0xF) {
                    self.ip = self.ip.wrapping_add(d as u16);
                    self.cyc += 3;
                }
            }
            0x80..=0x83 => {
                let (kind, l) = self.modrm(bus);
                if op & 1 == 0 {
                    let a = self.ld8(bus, l);
                    let b = self.fetch(bus);
                    let r = self.alu8(kind, a, b);
                    if kind != 7 {
                        self.st8(bus, l, r);
                    }
                } else {
                    let a = self.ld16(bus, l);
                    let b = if op == 0x83 { self.fetch(bus) as i8 as u16 } else { self.fetch16(bus) };
                    let r = self.alu16(kind, a, b);
                    if kind != 7 {
                        self.st16(bus, l, r);
                    }
                }
            }
            0x84 => {
                let (reg, l) = self.modrm(bus);
                let a = self.ld8(bus, l);
                self.alu8(4, a, self.get8(reg));
            }
            0x85 => {
                let (reg, l) = self.modrm(bus);
                let a = self.ld16(bus, l);
                self.alu16(4, a, self.r[reg]);
            }
            0x86 => {
                let (reg, l) = self.modrm(bus);
                let a = self.ld8(bus, l);
                let b = self.get8(reg);
                self.st8(bus, l, b);
                self.set8(reg, a);
            }
            0x87 => {
                let (reg, l) = self.modrm(bus);
                let a = self.ld16(bus, l);
                let b = self.r[reg];
                self.st16(bus, l, b);
                self.r[reg] = a;
            }
            0x88 => {
                let (reg, l) = self.modrm(bus);
                let v = self.get8(reg);
                self.st8(bus, l, v);
            }
            0x89 => {
                let (reg, l) = self.modrm(bus);
                let v = self.r[reg];
                self.st16(bus, l, v);
            }
            0x8A => {
                let (reg, l) = self.modrm(bus);
                let v = self.ld8(bus, l);
                self.set8(reg, v);
            }
            0x8B => {
                let (reg, l) = self.modrm(bus);
                self.r[reg] = self.ld16(bus, l);
            }
            0x8C => {
                let (reg, l) = self.modrm(bus);
                let v = self.seg[reg & 3];
                self.st16(bus, l, v);
            }
            0x8D => {
                let (reg, off, _) = self.modrm_offset(bus);
                self.r[reg] = off;
            }
            0x8E => {
                let (reg, l) = self.modrm(bus);
                self.seg[reg & 3] = self.ld16(bus, l);
            }
            0x8F => {
                let v = self.pop(bus);
                let (_, l) = self.modrm(bus);
                self.st16(bus, l, v);
            }
            0x90..=0x97 => {
                let i = (op & 7) as usize;
                self.r.swap(AX, i);
            }
            0x98 => self.r[AX] = self.r[AX] as u8 as i8 as i16 as u16,
            0x99 => self.r[DX] = if self.r[AX] & 0x8000 != 0 { 0xFFFF } else { 0 },
            0x9A => {
                let ip = self.fetch16(bus);
                let cs = self.fetch16(bus);
                let (ocs, oip) = (self.seg[CS], self.ip);
                self.push(bus, ocs);
                self.push(bus, oip);
                self.ip = ip;
                self.seg[CS] = cs;
                self.cyc += 8;
            }
            0x9B => {} // WAIT
            0x9C => {
                let f = self.flags;
                self.push(bus, f);
            }
            0x9D => self.flags = (self.pop(bus) & 0x0FD5) | 0xF002,
            0x9E => self.flags = (self.flags & 0xFF00) | (self.r[AX] >> 8) & 0xD5 | 2,
            0x9F => self.set8(4, self.flags as u8),
            0xA0..=0xA3 => {
                let off = self.fetch16(bus);
                let a = Self::lin(self.seg[self.ovr.unwrap_or(DS)], off);
                match op {
                    0xA0 => {
                        let v = bus.read(a);
                        self.set8(0, v);
                    }
                    0xA1 => self.r[AX] = Self::rd16(bus, a),
                    0xA2 => bus.write(a, self.r[AX] as u8),
                    _ => Self::wr16(bus, a, self.r[AX]),
                }
            }
            0xA4..=0xA7 | 0xAA..=0xAF => self.string_op(bus, op, rep),
            0xA8 => {
                let b = self.fetch(bus);
                self.alu8(4, self.r[AX] as u8, b);
            }
            0xA9 => {
                let b = self.fetch16(bus);
                self.alu16(4, self.r[AX], b);
            }
            0xB0..=0xB7 => {
                let v = self.fetch(bus);
                self.set8((op & 7) as usize, v);
            }
            0xB8..=0xBF => self.r[(op & 7) as usize] = self.fetch16(bus),
            0xC0 | 0xC1 | 0xD0..=0xD3 => {
                let (kind, l) = self.modrm(bus);
                let count = match op {
                    0xC0 | 0xC1 => self.fetch(bus),
                    0xD0 | 0xD1 => 1,
                    _ => self.r[CX] as u8,
                };
                if op & 1 == 0 {
                    let v = self.ld8(bus, l) as u32;
                    let r = self.shift(kind, v, count, false);
                    self.st8(bus, l, r as u8);
                } else {
                    let v = self.ld16(bus, l) as u32;
                    let r = self.shift(kind, v, count, true);
                    self.st16(bus, l, r as u16);
                }
            }
            0xC2 | 0xC3 => {
                let n = if op == 0xC2 { self.fetch16(bus) } else { 0 };
                self.ip = self.pop(bus);
                self.r[SP] = self.r[SP].wrapping_add(n);
                self.cyc += 5;
            }
            0xC4 | 0xC5 => {
                let (reg, _, lin) = self.modrm_offset(bus);
                self.r[reg] = Self::rd16(bus, lin);
                self.seg[if op == 0xC4 { ES } else { DS }] = Self::rd16(bus, (lin + 2) & 0xF_FFFF);
            }
            0xC6 => {
                let (_, l) = self.modrm(bus);
                let v = self.fetch(bus);
                self.st8(bus, l, v);
            }
            0xC7 => {
                let (_, l) = self.modrm(bus);
                let v = self.fetch16(bus);
                self.st16(bus, l, v);
            }
            0xC8 => {
                let size = self.fetch16(bus);
                let level = self.fetch(bus) & 0x1F;
                let bp = self.r[BP];
                self.push(bus, bp);
                let frame = self.r[SP];
                for i in 1..level {
                    let v = Self::rd16(bus, Self::lin(self.seg[SS], self.r[BP].wrapping_sub(2 * i as u16)));
                    self.push(bus, v);
                }
                if level > 0 {
                    self.push(bus, frame);
                }
                self.r[BP] = frame;
                self.r[SP] = self.r[SP].wrapping_sub(size);
            }
            0xC9 => {
                self.r[SP] = self.r[BP];
                self.r[BP] = self.pop(bus);
            }
            0xCA | 0xCB => {
                let n = if op == 0xCA { self.fetch16(bus) } else { 0 };
                self.ip = self.pop(bus);
                self.seg[CS] = self.pop(bus);
                self.r[SP] = self.r[SP].wrapping_add(n);
                self.cyc += 8;
            }
            0xCC => self.interrupt(bus, 3),
            0xCD => {
                let n = self.fetch(bus);
                self.interrupt(bus, n);
            }
            0xCE => {
                if self.flags & OF != 0 {
                    self.interrupt(bus, 4);
                }
            }
            0xCF => {
                self.ip = self.pop(bus);
                self.seg[CS] = self.pop(bus);
                self.flags = (self.pop(bus) & 0x0FD5) | 0xF002;
                self.cyc += 8;
            }
            0xD4 => {
                let base = self.fetch(bus);
                let al = self.r[AX] as u8;
                if base == 0 {
                    self.interrupt(bus, 0);
                } else {
                    self.set8(4, al / base);
                    self.set8(0, al % base);
                    self.szp8(al % base);
                }
                self.cyc += 16;
            }
            0xD5 => {
                let base = self.fetch(bus);
                let r = (self.r[AX] as u8).wrapping_add(((self.r[AX] >> 8) as u8).wrapping_mul(base));
                self.r[AX] = r as u16;
                self.szp8(r);
            }
            0xD6 => self.set8(0, if self.flags & CF != 0 { 0xFF } else { 0 }),
            0xD7 => {
                let a = Self::lin(self.seg[self.ovr.unwrap_or(DS)], self.r[BX].wrapping_add(self.r[AX] & 0xFF));
                let v = bus.read(a);
                self.set8(0, v);
            }
            0xD8..=0xDF => {
                let _ = self.modrm(bus); // FPU escape: no coprocessor
            }
            0xE0..=0xE3 => {
                let d = self.fetch(bus) as i8;
                let take = match op {
                    0xE3 => self.r[CX] == 0,
                    _ => {
                        self.r[CX] = self.r[CX].wrapping_sub(1);
                        let nz = self.r[CX] != 0;
                        match op {
                            0xE0 => nz && self.flags & ZF == 0,
                            0xE1 => nz && self.flags & ZF != 0,
                            _ => nz,
                        }
                    }
                };
                if take {
                    self.ip = self.ip.wrapping_add(d as u16);
                    self.cyc += 3;
                }
            }
            0xE4 => {
                let p = self.fetch(bus) as u16;
                let v = bus.input(p);
                self.set8(0, v);
            }
            0xE5 => {
                let p = self.fetch(bus) as u16;
                self.r[AX] = bus.input(p) as u16 | (bus.input(p + 1) as u16) << 8;
            }
            0xE6 => {
                let p = self.fetch(bus) as u16;
                bus.output(p, self.r[AX] as u8);
            }
            0xE7 => {
                let p = self.fetch(bus) as u16;
                bus.output(p, self.r[AX] as u8);
                bus.output(p + 1, (self.r[AX] >> 8) as u8);
            }
            0xE8 => {
                let d = self.fetch16(bus);
                let ip = self.ip;
                self.push(bus, ip);
                self.ip = self.ip.wrapping_add(d);
                self.cyc += 4;
            }
            0xE9 => {
                let d = self.fetch16(bus);
                self.ip = self.ip.wrapping_add(d);
                self.cyc += 3;
            }
            0xEA => {
                let ip = self.fetch16(bus);
                let cs = self.fetch16(bus);
                self.ip = ip;
                self.seg[CS] = cs;
                self.cyc += 6;
            }
            0xEB => {
                let d = self.fetch(bus) as i8;
                self.ip = self.ip.wrapping_add(d as u16);
                self.cyc += 3;
            }
            0xEC => {
                let v = bus.input(self.r[DX]);
                self.set8(0, v);
            }
            0xED => {
                let p = self.r[DX];
                self.r[AX] = bus.input(p) as u16 | (bus.input(p.wrapping_add(1)) as u16) << 8;
            }
            0xEE => bus.output(self.r[DX], self.r[AX] as u8),
            0xEF => {
                let p = self.r[DX];
                bus.output(p, self.r[AX] as u8);
                bus.output(p.wrapping_add(1), (self.r[AX] >> 8) as u8);
            }
            0xF4 => self.halted = true,
            0xF5 => self.flags ^= CF,
            0xF6 | 0xF7 => self.group3(bus, op),
            0xF8 => self.flags &= !CF,
            0xF9 => self.flags |= CF,
            0xFA => self.flags &= !IF,
            0xFB => self.flags |= IF,
            0xFC => self.flags &= !DF,
            0xFD => self.flags |= DF,
            0xFE => {
                let (kind, l) = self.modrm(bus);
                let v = self.ld8(bus, l);
                let r = self.inc8(v, kind == 1);
                self.st8(bus, l, r);
            }
            0xFF => {
                let (kind, l) = self.modrm(bus);
                match kind {
                    0 | 1 => {
                        let v = self.ld16(bus, l);
                        let r = self.inc16(v, kind == 1);
                        self.st16(bus, l, r);
                    }
                    2 => {
                        let t = self.ld16(bus, l);
                        let ip = self.ip;
                        self.push(bus, ip);
                        self.ip = t;
                    }
                    3 => {
                        if let Loc::Mem(a) = l {
                            let (ip, cs) = (Self::rd16(bus, a), Self::rd16(bus, (a + 2) & 0xF_FFFF));
                            let (ocs, oip) = (self.seg[CS], self.ip);
                            self.push(bus, ocs);
                            self.push(bus, oip);
                            self.ip = ip;
                            self.seg[CS] = cs;
                        }
                    }
                    4 => self.ip = self.ld16(bus, l),
                    5 => {
                        if let Loc::Mem(a) = l {
                            self.ip = Self::rd16(bus, a);
                            self.seg[CS] = Self::rd16(bus, (a + 2) & 0xF_FFFF);
                        }
                    }
                    _ => {
                        let v = self.ld16(bus, l);
                        self.push(bus, v);
                    }
                }
            }
            _ => {} // unused opcodes act as NOPs on the V30MZ
        }
    }

    fn group3<B: Bus>(&mut self, bus: &mut B, op: u8) {
        let (kind, l) = self.modrm(bus);
        let wide = op & 1 != 0;
        match kind {
            0 | 1 => {
                if wide {
                    let (a, b) = (self.ld16(bus, l), self.fetch16(bus));
                    self.alu16(4, a, b);
                } else {
                    let (a, b) = (self.ld8(bus, l), self.fetch(bus));
                    self.alu8(4, a, b);
                }
            }
            2 => {
                if wide {
                    let v = !self.ld16(bus, l);
                    self.st16(bus, l, v);
                } else {
                    let v = !self.ld8(bus, l);
                    self.st8(bus, l, v);
                }
            }
            3 => {
                if wide {
                    let v = self.ld16(bus, l);
                    let r = self.alu16(5, 0, v);
                    self.st16(bus, l, r);
                } else {
                    let v = self.ld8(bus, l);
                    let r = self.alu8(5, 0, v);
                    self.st8(bus, l, r);
                }
            }
            4 | 5 => {
                self.cyc += 3;
                if wide {
                    let v = self.ld16(bus, l);
                    let (p, over) = if kind == 4 {
                        let p = self.r[AX] as u32 * v as u32;
                        (p, p >> 16 != 0)
                    } else {
                        let p = (self.r[AX] as i16 as i32 * v as i16 as i32) as u32;
                        (p, (p as i32) != (p as i16) as i32)
                    };
                    self.r[AX] = p as u16;
                    self.r[DX] = (p >> 16) as u16;
                    self.flag(CF, over);
                    self.flag(OF, over);
                } else {
                    let v = self.ld8(bus, l);
                    let (p, over) = if kind == 4 {
                        let p = (self.r[AX] as u8) as u16 * v as u16;
                        (p, p >> 8 != 0)
                    } else {
                        let p = ((self.r[AX] as u8 as i8 as i16) * (v as i8 as i16)) as u16;
                        (p, (p as i16) != (p as u8 as i8 as i16))
                    };
                    self.r[AX] = p;
                    self.flag(CF, over);
                    self.flag(OF, over);
                }
            }
            _ => {
                self.cyc += 15;
                if wide {
                    let v = self.ld16(bus, l);
                    let num = (self.r[DX] as u32) << 16 | self.r[AX] as u32;
                    if kind == 6 {
                        if v == 0 || num / v as u32 > 0xFFFF {
                            self.interrupt(bus, 0);
                            return;
                        }
                        self.r[AX] = (num / v as u32) as u16;
                        self.r[DX] = (num % v as u32) as u16;
                    } else {
                        let (n, d) = (num as i32, v as i16 as i32);
                        if d == 0 || n / d > 0x7FFF || n / d < -0x8000 {
                            self.interrupt(bus, 0);
                            return;
                        }
                        self.r[AX] = (n / d) as u16;
                        self.r[DX] = (n % d) as u16;
                    }
                } else {
                    let v = self.ld8(bus, l);
                    let num = self.r[AX];
                    if kind == 6 {
                        if v == 0 || num / v as u16 > 0xFF {
                            self.interrupt(bus, 0);
                            return;
                        }
                        self.set8(0, (num / v as u16) as u8);
                        self.set8(4, (num % v as u16) as u8);
                    } else {
                        let (n, d) = (num as i16 as i32, v as i8 as i32);
                        if d == 0 || n / d > 0x7F || n / d < -0x80 {
                            self.interrupt(bus, 0);
                            return;
                        }
                        self.set8(0, (n / d) as u8);
                        self.set8(4, (n % d) as u8);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem {
        m: Vec<u8>,
        io: Vec<(u16, u8)>,
    }
    impl Bus for Mem {
        fn read(&mut self, a: u32) -> u8 {
            self.m[a as usize]
        }
        fn write(&mut self, a: u32, v: u8) {
            self.m[a as usize] = v;
        }
        fn input(&mut self, p: u16) -> u8 {
            p as u8 ^ 0xFF
        }
        fn output(&mut self, p: u16, v: u8) {
            self.io.push((p, v));
        }
    }

    fn run(prog: &[u8], steps: usize) -> (Cpu, Mem) {
        let mut m = Mem { m: vec![0; 0x10_0000], io: vec![] };
        m.m[0x1000..0x1000 + prog.len()].copy_from_slice(prog);
        let mut c = Cpu::new();
        c.seg[CS] = 0x0100;
        c.ip = 0;
        c.seg[SS] = 0x0800;
        c.r[SP] = 0x1000;
        for _ in 0..steps {
            c.step(&mut m);
        }
        (c, m)
    }

    #[test]
    fn mov_add_and_flags() {
        // MOV AX,0x7FFF ; ADD AX,1 ; MOV BL,AL ; SUB BL,1
        let (c, _) = run(&[0xB8, 0xFF, 0x7F, 0x05, 0x01, 0x00, 0x88, 0xC3, 0x80, 0xEB, 0x01], 4);
        assert_eq!(c.r[AX], 0x8000);
        assert_eq!(c.r[BX] & 0xFF, 0xFF);
        assert!(c.flags & CF != 0, "0 - 1 borrows");
    }

    #[test]
    fn memory_modrm_and_loop() {
        // MOV BX,0x2000 ; MOV CX,5 ; l: ADD BYTE [BX],3 ; LOOP l ; MOV AL,[BX]
        let (c, m) = run(&[0xBB, 0x00, 0x20, 0xB9, 0x05, 0x00, 0x80, 0x07, 0x03, 0xE2, 0xFB, 0x8A, 0x07], 2 + 10 + 1);
        assert_eq!(m.m[0x2000], 15);
        assert_eq!(c.r[AX] & 0xFF, 15);
        assert_eq!(c.r[CX], 0);
    }

    #[test]
    fn call_ret_push_pop_and_186() {
        // CALL +4 ; HLT(pad) ; ... ; at target: PUSH 0x1234 ; POP DX ; PUSHA ; POPA ; RET
        let (c, _) = run(&[0xE8, 0x01, 0x00, 0xF4, 0x68, 0x34, 0x12, 0x5A, 0x60, 0x61, 0xC3], 7);
        assert_eq!(c.r[DX], 0x1234);
        assert!(c.halted, "returned to the HLT");
        assert_eq!(c.r[SP], 0x1000);
    }

    #[test]
    fn rep_movsb_and_mul_div() {
        // MOV SI,0x3000 ; MOV DI,0x3100 ; MOV CX,4 ; CLD ; REP MOVSB ; MOV AX,100 ; MOV BL,7 ; DIV BL
        let mut p = vec![0xBE, 0x00, 0x30, 0xBF, 0x00, 0x31, 0xB9, 0x04, 0x00, 0xFC, 0xF3, 0xA4, 0xB8, 0x64, 0x00, 0xB3, 0x07, 0xF6, 0xF3];
        p.resize(0x2000, 0);
        let mut mem = Mem { m: vec![0; 0x10_0000], io: vec![] };
        mem.m[0x1000..0x1000 + p.len()].copy_from_slice(&p);
        mem.m[0x3000..0x3004].copy_from_slice(&[1, 2, 3, 4]);
        let mut c = Cpu::new();
        c.seg[CS] = 0x0100;
        for _ in 0..8 {
            c.step(&mut mem);
        }
        assert_eq!(&mem.m[0x3100..0x3104], &[1, 2, 3, 4]);
        assert_eq!(c.r[AX] & 0xFF, 14, "100 / 7");
        assert_eq!(c.r[AX] >> 8, 2, "remainder");
    }

    #[test]
    fn interrupt_vector_and_iret() {
        // Vector 0x10 -> 0x0000:0x2000 ; at 0x2000: MOV AL,0x55 ; IRET ; main: INT 0x10 ; HLT
        let mut mem = Mem { m: vec![0; 0x10_0000], io: vec![] };
        mem.m[0x40..0x44].copy_from_slice(&[0x00, 0x20, 0x00, 0x00]);
        mem.m[0x2000..0x2003].copy_from_slice(&[0xB0, 0x55, 0xCF]);
        mem.m[0x1000..0x1003].copy_from_slice(&[0xCD, 0x10, 0xF4]);
        let mut c = Cpu::new();
        c.seg[CS] = 0x0100;
        c.r[SP] = 0x8000;
        for _ in 0..4 {
            c.step(&mut mem);
        }
        assert_eq!(c.r[AX] & 0xFF, 0x55);
        assert!(c.halted);
    }

    #[test]
    fn io_ports() {
        // MOV AL,0x42 ; OUT 0x60,AL ; IN AL,0x10
        let (c, m) = run(&[0xB0, 0x42, 0xE6, 0x60, 0xE4, 0x10], 3);
        assert_eq!(m.io, vec![(0x60, 0x42)]);
        assert_eq!(c.r[AX] & 0xFF, 0xEF);
    }
}
