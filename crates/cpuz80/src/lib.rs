//! Zilog Z80 interpreter with all documented instructions, the CB/DD/ED/FD prefixes,
//! undocumented IXH/IXL/IYH/IYL forms and the X/Y (bits 3/5) flag behaviour.
//!
//! Decoding follows the classic x/y/z/p/q split of the opcode byte.

pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, v: u8);
    fn input(&mut self, port: u16) -> u8;
    fn output(&mut self, port: u16, v: u8);
}

const FC: u8 = 0x01;
const FN: u8 = 0x02;
const FP: u8 = 0x04;
const FX: u8 = 0x08;
const FH: u8 = 0x10;
const FY: u8 = 0x20;
const FZ: u8 = 0x40;
const FS: u8 = 0x80;

fn parity(v: u8) -> bool {
    v.count_ones() % 2 == 0
}

/// S, Z, X, Y and parity for logic results.
fn szp(v: u8) -> u8 {
    (v & (FS | FX | FY)) | if v == 0 { FZ } else { 0 } | if parity(v) { FP } else { 0 }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Idx {
    Hl,
    Ix,
    Iy,
}

#[derive(Default, Clone, state::Stateful)]
pub struct Z80 {
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    /// Alternate AF, BC, DE, HL.
    pub alt: [u16; 4],
    pub ix: u16,
    pub iy: u16,
    pub sp: u16,
    pub pc: u16,
    pub i: u8,
    pub r: u8,
    pub iff1: bool,
    pub iff2: bool,
    pub im: u8,
    pub halted: bool,
    nmi_pending: bool,
    pub irq_line: bool,
    /// EI delays interrupts by one instruction.
    ei_delay: bool,
    /// Data bus value for IM 2 / IM 0 (usually 0xFF).
    pub irq_data: u8,
}

impl Z80 {
    pub fn new() -> Z80 {
        let mut z = Z80::default();
        z.reset();
        z
    }

    pub fn reset(&mut self) {
        self.pc = 0;
        self.sp = 0xFFFF;
        self.a = 0xFF;
        self.f = 0xFF;
        self.iff1 = false;
        self.iff2 = false;
        self.im = 0;
        self.i = 0;
        self.r = 0;
        self.halted = false;
        self.irq_data = 0xFF;
    }

    pub fn nmi(&mut self) {
        self.nmi_pending = true;
    }

    pub fn bc(&self) -> u16 {
        (self.b as u16) << 8 | self.c as u16
    }
    pub fn de(&self) -> u16 {
        (self.d as u16) << 8 | self.e as u16
    }
    pub fn hl(&self) -> u16 {
        (self.h as u16) << 8 | self.l as u16
    }
    pub fn af(&self) -> u16 {
        (self.a as u16) << 8 | self.f as u16
    }
    fn set_bc(&mut self, v: u16) {
        self.b = (v >> 8) as u8;
        self.c = v as u8;
    }
    fn set_de(&mut self, v: u16) {
        self.d = (v >> 8) as u8;
        self.e = v as u8;
    }
    fn set_hl(&mut self, v: u16) {
        self.h = (v >> 8) as u8;
        self.l = v as u8;
    }
    fn set_af(&mut self, v: u16) {
        self.a = (v >> 8) as u8;
        self.f = v as u8;
    }

    fn fetch<B: Bus>(&mut self, bus: &mut B) -> u8 {
        let v = bus.read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        v
    }
    fn fetch16<B: Bus>(&mut self, bus: &mut B) -> u16 {
        let lo = self.fetch(bus) as u16;
        lo | (self.fetch(bus) as u16) << 8
    }
    fn read16<B: Bus>(bus: &mut B, a: u16) -> u16 {
        bus.read(a) as u16 | (bus.read(a.wrapping_add(1)) as u16) << 8
    }
    fn write16<B: Bus>(bus: &mut B, a: u16, v: u16) {
        bus.write(a, v as u8);
        bus.write(a.wrapping_add(1), (v >> 8) as u8);
    }
    fn push<B: Bus>(&mut self, bus: &mut B, v: u16) {
        self.sp = self.sp.wrapping_sub(2);
        Self::write16(bus, self.sp, v);
    }
    fn pop<B: Bus>(&mut self, bus: &mut B) -> u16 {
        let v = Self::read16(bus, self.sp);
        self.sp = self.sp.wrapping_add(2);
        v
    }
    fn inc_r(&mut self) {
        self.r = (self.r & 0x80) | (self.r.wrapping_add(1) & 0x7F);
    }

    // ---------------------------------------------------------------- register access

    fn idx_reg(&self, x: Idx) -> u16 {
        match x {
            Idx::Hl => self.hl(),
            Idx::Ix => self.ix,
            Idx::Iy => self.iy,
        }
    }
    fn set_idx_reg(&mut self, x: Idx, v: u16) {
        match x {
            Idx::Hl => self.set_hl(v),
            Idx::Ix => self.ix = v,
            Idx::Iy => self.iy = v,
        }
    }

    /// 8-bit register r (0-7: B C D E H L (HL) A); H/L become IXH/IXL etc. under a prefix.
    /// Index 6 is memory and handled by the caller.
    fn get8(&self, r: u8, x: Idx) -> u8 {
        match r {
            0 => self.b,
            1 => self.c,
            2 => self.d,
            3 => self.e,
            4 => (self.idx_reg(x) >> 8) as u8,
            5 => self.idx_reg(x) as u8,
            7 => self.a,
            _ => unreachable!(),
        }
    }
    fn set8(&mut self, r: u8, x: Idx, v: u8) {
        match r {
            0 => self.b = v,
            1 => self.c = v,
            2 => self.d = v,
            3 => self.e = v,
            4 => {
                let w = self.idx_reg(x);
                self.set_idx_reg(x, (w & 0xFF) | (v as u16) << 8);
            }
            5 => {
                let w = self.idx_reg(x);
                self.set_idx_reg(x, (w & 0xFF00) | v as u16);
            }
            7 => self.a = v,
            _ => unreachable!(),
        }
    }

    /// rp table: BC DE HL/IX/IY SP.
    fn rp(&self, p: u8, x: Idx) -> u16 {
        match p {
            0 => self.bc(),
            1 => self.de(),
            2 => self.idx_reg(x),
            _ => self.sp,
        }
    }
    fn set_rp(&mut self, p: u8, x: Idx, v: u16) {
        match p {
            0 => self.set_bc(v),
            1 => self.set_de(v),
            2 => self.set_idx_reg(x, v),
            _ => self.sp = v,
        }
    }
    /// rp2 table: BC DE HL/IX/IY AF.
    fn rp2(&self, p: u8, x: Idx) -> u16 {
        if p == 3 {
            self.af()
        } else {
            self.rp(p, x)
        }
    }
    fn set_rp2(&mut self, p: u8, x: Idx, v: u16) {
        if p == 3 {
            self.set_af(v)
        } else {
            self.set_rp(p, x, v)
        }
    }

    fn cond(&self, y: u8) -> bool {
        match y {
            0 => self.f & FZ == 0,
            1 => self.f & FZ != 0,
            2 => self.f & FC == 0,
            3 => self.f & FC != 0,
            4 => self.f & FP == 0,
            5 => self.f & FP != 0,
            6 => self.f & FS == 0,
            _ => self.f & FS != 0,
        }
    }

    // ---------------------------------------------------------------- ALU

    fn add8(&mut self, v: u8, carry: bool) {
        let c = (carry && self.f & FC != 0) as u16;
        let a = self.a;
        let r = a as u16 + v as u16 + c;
        let res = r as u8;
        self.f = (res & (FS | FX | FY))
            | if res == 0 { FZ } else { 0 }
            | if (a & 0xF) + (v & 0xF) + c as u8 > 0xF { FH } else { 0 }
            | if (!(a ^ v) & (a ^ res)) & 0x80 != 0 { FP } else { 0 }
            | if r > 0xFF { FC } else { 0 };
        self.a = res;
    }

    fn sub8(&mut self, v: u8, carry: bool, store: bool) {
        let c = (carry && self.f & FC != 0) as u16;
        let a = self.a;
        let r = (a as u16).wrapping_sub(v as u16).wrapping_sub(c);
        let res = r as u8;
        let xy = if store { res } else { v };
        self.f = (res & FS)
            | (xy & (FX | FY))
            | if res == 0 { FZ } else { 0 }
            | if (a & 0xF) < (v & 0xF) + c as u8 { FH } else { 0 }
            | if ((a ^ v) & (a ^ res)) & 0x80 != 0 { FP } else { 0 }
            | FN
            | if r > 0xFF { FC } else { 0 };
        if store {
            self.a = res;
        }
    }

    fn alu(&mut self, y: u8, v: u8) {
        match y {
            0 => self.add8(v, false),
            1 => self.add8(v, true),
            2 => self.sub8(v, false, true),
            3 => self.sub8(v, true, true),
            4 => {
                self.a &= v;
                self.f = szp(self.a) | FH;
            }
            5 => {
                self.a ^= v;
                self.f = szp(self.a);
            }
            6 => {
                self.a |= v;
                self.f = szp(self.a);
            }
            _ => self.sub8(v, false, false),
        }
    }

    fn inc8(&mut self, v: u8) -> u8 {
        let r = v.wrapping_add(1);
        self.f = (self.f & FC) | (r & (FS | FX | FY)) | if r == 0 { FZ } else { 0 } | if v & 0xF == 0xF { FH } else { 0 } | if v == 0x7F { FP } else { 0 };
        r
    }
    fn dec8(&mut self, v: u8) -> u8 {
        let r = v.wrapping_sub(1);
        self.f = (self.f & FC) | (r & (FS | FX | FY)) | if r == 0 { FZ } else { 0 } | if v & 0xF == 0 { FH } else { 0 } | if v == 0x80 { FP } else { 0 } | FN;
        r
    }

    fn add16(&mut self, a: u16, b: u16) -> u16 {
        let r = a as u32 + b as u32;
        let res = r as u16;
        self.f = (self.f & (FS | FZ | FP)) | ((res >> 8) as u8 & (FX | FY)) | if (a & 0xFFF) + (b & 0xFFF) > 0xFFF { FH } else { 0 } | if r > 0xFFFF { FC } else { 0 };
        res
    }
    fn adc16(&mut self, a: u16, b: u16) -> u16 {
        let c = (self.f & FC) as u32;
        let r = a as u32 + b as u32 + c;
        let res = r as u16;
        self.f = ((res >> 8) as u8 & (FS | FX | FY))
            | if res == 0 { FZ } else { 0 }
            | if (a & 0xFFF) as u32 + (b & 0xFFF) as u32 + c > 0xFFF { FH } else { 0 }
            | if (!(a ^ b) & (a ^ res)) & 0x8000 != 0 { FP } else { 0 }
            | if r > 0xFFFF { FC } else { 0 };
        res
    }
    fn sbc16(&mut self, a: u16, b: u16) -> u16 {
        let c = (self.f & FC) as u32;
        let r = (a as u32).wrapping_sub(b as u32).wrapping_sub(c);
        let res = r as u16;
        self.f = ((res >> 8) as u8 & (FS | FX | FY))
            | if res == 0 { FZ } else { 0 }
            | if ((a & 0xFFF) as u32) < (b & 0xFFF) as u32 + c { FH } else { 0 }
            | if ((a ^ b) & (a ^ res)) & 0x8000 != 0 { FP } else { 0 }
            | FN
            | if r > 0xFFFF { FC } else { 0 };
        res
    }

    fn rot(&mut self, y: u8, v: u8) -> u8 {
        let c = self.f & FC;
        let (r, out) = match y {
            0 => (v.rotate_left(1), v >> 7),
            1 => (v.rotate_right(1), v & 1),
            2 => (v << 1 | c, v >> 7),
            3 => (v >> 1 | c << 7, v & 1),
            4 => (v << 1, v >> 7),
            5 => ((v >> 1) | (v & 0x80), v & 1),
            6 => (v << 1 | 1, v >> 7), // SLL (undocumented)
            _ => (v >> 1, v & 1),
        };
        self.f = szp(r) | out;
        r
    }

    fn daa(&mut self) {
        let a = self.a;
        let (n, c, h) = (self.f & FN != 0, self.f & FC != 0, self.f & FH != 0);
        let mut diff = 0u8;
        let mut carry = c;
        if h || a & 0xF > 9 {
            diff |= 0x06;
        }
        if c || a > 0x99 {
            diff |= 0x60;
            carry = true;
        }
        let r = if n { a.wrapping_sub(diff) } else { a.wrapping_add(diff) };
        let half = if n { h && (a & 0xF) < 6 } else { (a & 0xF) > 9 };
        self.a = r;
        self.f = szp(r) | (self.f & FN) | if half { FH } else { 0 } | if carry { FC } else { 0 };
    }

    // ---------------------------------------------------------------- execution

    /// Services pending NMI/IRQ; returns cycles if one was taken.
    fn interrupts<B: Bus>(&mut self, bus: &mut B) -> Option<u32> {
        if self.nmi_pending {
            self.nmi_pending = false;
            self.halted = false;
            self.iff2 = self.iff1;
            self.iff1 = false;
            self.inc_r();
            let pc = self.pc;
            self.push(bus, pc);
            self.pc = 0x66;
            return Some(11);
        }
        if self.irq_line && self.iff1 && !self.ei_delay {
            self.halted = false;
            self.iff1 = false;
            self.iff2 = false;
            self.inc_r();
            let pc = self.pc;
            self.push(bus, pc);
            return Some(match self.im {
                2 => {
                    let v = ((self.i as u16) << 8) | self.irq_data as u16;
                    self.pc = Self::read16(bus, v);
                    19
                }
                // IM 1, and IM 0 with RST 38h on the bus (the usual case).
                _ => {
                    self.pc = 0x38;
                    13
                }
            });
        }
        None
    }

    /// Executes one instruction (or interrupt); returns T-states.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> u32 {
        if let Some(c) = self.interrupts(bus) {
            return c;
        }
        self.ei_delay = false;
        if self.halted {
            self.inc_r();
            return 4;
        }
        self.inc_r();
        let op = self.fetch(bus);
        match op {
            0xCB => self.exec_cb(bus, Idx::Hl, 0),
            0xDD | 0xFD => {
                let x = if op == 0xDD { Idx::Ix } else { Idx::Iy };
                self.inc_r();
                let op2 = self.fetch(bus);
                match op2 {
                    0xCB => {
                        let d = self.fetch(bus) as i8;
                        4 + self.exec_cb(bus, x, d)
                    }
                    // A prefix before another prefix or ED acts as a NOP.
                    0xDD | 0xFD | 0xED => {
                        self.pc = self.pc.wrapping_sub(1);
                        4
                    }
                    _ => 4 + self.exec_main(bus, op2, x),
                }
            }
            0xED => {
                self.inc_r();
                let op2 = self.fetch(bus);
                self.exec_ed(bus, op2)
            }
            _ => self.exec_main(bus, op, Idx::Hl),
        }
    }

    /// Effective address for (HL) or (IX+d)/(IY+d); fetches d when indexed.
    fn mem_addr<B: Bus>(&mut self, bus: &mut B, x: Idx) -> u16 {
        match x {
            Idx::Hl => self.hl(),
            _ => {
                let d = self.fetch(bus) as i8;
                self.idx_reg(x).wrapping_add(d as u16)
            }
        }
    }

    fn exec_main<B: Bus>(&mut self, bus: &mut B, op: u8, x: Idx) -> u32 {
        let (xx, y, z) = (op >> 6, (op >> 3) & 7, op & 7);
        let (p, q) = (y >> 1, y & 1);
        let idx_extra = if x == Idx::Hl { 0 } else { 8 };
        match xx {
            0 => match z {
                0 => match y {
                    0 => 4,
                    1 => {
                        let af = self.af();
                        self.set_af(self.alt[0]);
                        self.alt[0] = af;
                        4
                    }
                    2 => {
                        let d = self.fetch(bus) as i8;
                        self.b = self.b.wrapping_sub(1);
                        if self.b != 0 {
                            self.pc = self.pc.wrapping_add(d as u16);
                            13
                        } else {
                            8
                        }
                    }
                    3 => {
                        let d = self.fetch(bus) as i8;
                        self.pc = self.pc.wrapping_add(d as u16);
                        12
                    }
                    _ => {
                        let d = self.fetch(bus) as i8;
                        if self.cond(y - 4) {
                            self.pc = self.pc.wrapping_add(d as u16);
                            12
                        } else {
                            7
                        }
                    }
                },
                1 => {
                    if q == 0 {
                        let v = self.fetch16(bus);
                        self.set_rp(p, x, v);
                        10
                    } else {
                        let r = self.add16(self.idx_reg(x), self.rp(p, x));
                        self.set_idx_reg(x, r);
                        11
                    }
                }
                2 => match (q, p) {
                    (0, 0) => {
                        bus.write(self.bc(), self.a);
                        7
                    }
                    (0, 1) => {
                        bus.write(self.de(), self.a);
                        7
                    }
                    (0, 2) => {
                        let a = self.fetch16(bus);
                        Self::write16(bus, a, self.idx_reg(x));
                        16
                    }
                    (0, _) => {
                        let a = self.fetch16(bus);
                        bus.write(a, self.a);
                        13
                    }
                    (_, 0) => {
                        self.a = bus.read(self.bc());
                        7
                    }
                    (_, 1) => {
                        self.a = bus.read(self.de());
                        7
                    }
                    (_, 2) => {
                        let a = self.fetch16(bus);
                        let v = Self::read16(bus, a);
                        self.set_idx_reg(x, v);
                        16
                    }
                    _ => {
                        let a = self.fetch16(bus);
                        self.a = bus.read(a);
                        13
                    }
                },
                3 => {
                    let v = self.rp(p, x);
                    self.set_rp(p, x, if q == 0 { v.wrapping_add(1) } else { v.wrapping_sub(1) });
                    6
                }
                4 | 5 => {
                    if y == 6 {
                        let a = self.mem_addr(bus, x);
                        let v = bus.read(a);
                        let r = if z == 4 { self.inc8(v) } else { self.dec8(v) };
                        bus.write(a, r);
                        11 + idx_extra
                    } else {
                        let v = self.get8(y, x);
                        let r = if z == 4 { self.inc8(v) } else { self.dec8(v) };
                        self.set8(y, x, r);
                        4
                    }
                }
                6 => {
                    if y == 6 {
                        let a = self.mem_addr(bus, x);
                        let n = self.fetch(bus);
                        bus.write(a, n);
                        if x == Idx::Hl {
                            10
                        } else {
                            15
                        }
                    } else {
                        let n = self.fetch(bus);
                        self.set8(y, x, n);
                        7
                    }
                }
                _ => {
                    match y {
                        0..=3 => {
                            // RLCA RRCA RLA RRA: only C, H, N and X/Y change.
                            let a = self.a;
                            let c = self.f & FC;
                            let (r, out) = match y {
                                0 => (a.rotate_left(1), a >> 7),
                                1 => (a.rotate_right(1), a & 1),
                                2 => (a << 1 | c, a >> 7),
                                _ => (a >> 1 | c << 7, a & 1),
                            };
                            self.a = r;
                            self.f = (self.f & (FS | FZ | FP)) | (r & (FX | FY)) | out;
                        }
                        4 => self.daa(),
                        5 => {
                            self.a = !self.a;
                            self.f = (self.f & (FS | FZ | FP | FC)) | FH | FN | (self.a & (FX | FY));
                        }
                        6 => self.f = (self.f & (FS | FZ | FP)) | (self.a & (FX | FY)) | FC,
                        _ => {
                            let c = self.f & FC;
                            self.f = (self.f & (FS | FZ | FP)) | (self.a & (FX | FY)) | if c != 0 { FH } else { FC };
                        }
                    }
                    4
                }
            },
            1 => {
                if y == 6 && z == 6 {
                    self.halted = true;
                    return 4;
                }
                if z == 6 {
                    let a = self.mem_addr(bus, x);
                    let v = bus.read(a);
                    self.set8(y, Idx::Hl, v);
                    7 + idx_extra
                } else if y == 6 {
                    let a = self.mem_addr(bus, x);
                    bus.write(a, self.get8(z, Idx::Hl));
                    7 + idx_extra
                } else {
                    let v = self.get8(z, x);
                    self.set8(y, x, v);
                    4
                }
            }
            2 => {
                if z == 6 {
                    let a = self.mem_addr(bus, x);
                    let v = bus.read(a);
                    self.alu(y, v);
                    7 + idx_extra
                } else {
                    let v = self.get8(z, x);
                    self.alu(y, v);
                    4
                }
            }
            _ => match z {
                0 => {
                    if self.cond(y) {
                        self.pc = self.pop(bus);
                        11
                    } else {
                        5
                    }
                }
                1 => {
                    if q == 0 {
                        let v = self.pop(bus);
                        self.set_rp2(p, x, v);
                        10
                    } else {
                        match p {
                            0 => {
                                self.pc = self.pop(bus);
                                10
                            }
                            1 => {
                                for i in 1..4 {
                                    let cur = match i {
                                        1 => self.bc(),
                                        2 => self.de(),
                                        _ => self.hl(),
                                    };
                                    let other = self.alt[i];
                                    self.alt[i] = cur;
                                    match i {
                                        1 => self.set_bc(other),
                                        2 => self.set_de(other),
                                        _ => self.set_hl(other),
                                    }
                                }
                                4
                            }
                            2 => {
                                self.pc = self.idx_reg(x);
                                4
                            }
                            _ => {
                                self.sp = self.idx_reg(x);
                                6
                            }
                        }
                    }
                }
                2 => {
                    let a = self.fetch16(bus);
                    if self.cond(y) {
                        self.pc = a;
                    }
                    10
                }
                3 => match y {
                    0 => {
                        self.pc = self.fetch16(bus);
                        10
                    }
                    1 => unreachable!("CB handled earlier"),
                    2 => {
                        let n = self.fetch(bus);
                        bus.output((self.a as u16) << 8 | n as u16, self.a);
                        11
                    }
                    3 => {
                        let n = self.fetch(bus);
                        self.a = bus.input((self.a as u16) << 8 | n as u16);
                        11
                    }
                    4 => {
                        let v = Self::read16(bus, self.sp);
                        Self::write16(bus, self.sp, self.idx_reg(x));
                        self.set_idx_reg(x, v);
                        19
                    }
                    5 => {
                        let de = self.de();
                        let hl = self.hl();
                        self.set_de(hl);
                        self.set_hl(de);
                        4
                    }
                    6 => {
                        self.iff1 = false;
                        self.iff2 = false;
                        4
                    }
                    _ => {
                        self.iff1 = true;
                        self.iff2 = true;
                        self.ei_delay = true;
                        4
                    }
                },
                4 => {
                    let a = self.fetch16(bus);
                    if self.cond(y) {
                        let pc = self.pc;
                        self.push(bus, pc);
                        self.pc = a;
                        17
                    } else {
                        10
                    }
                }
                5 => {
                    if q == 0 {
                        let v = self.rp2(p, x);
                        self.push(bus, v);
                        11
                    } else {
                        // p == 0: CALL nn (DD/ED/FD prefixes are dispatched in step()).
                        let a = self.fetch16(bus);
                        let pc = self.pc;
                        self.push(bus, pc);
                        self.pc = a;
                        17
                    }
                }
                6 => {
                    let n = self.fetch(bus);
                    self.alu(y, n);
                    7
                }
                _ => {
                    let pc = self.pc;
                    self.push(bus, pc);
                    self.pc = (y as u16) * 8;
                    11
                }
            },
        }
    }

    fn exec_cb<B: Bus>(&mut self, bus: &mut B, x: Idx, d: i8) -> u32 {
        let (addr, op) = if x == Idx::Hl {
            self.inc_r();
            (self.hl(), self.fetch(bus))
        } else {
            (self.idx_reg(x).wrapping_add(d as u16), self.fetch(bus))
        };
        let (xx, y, z) = (op >> 6, (op >> 3) & 7, op & 7);
        let mem = z == 6 || x != Idx::Hl;
        let v = if mem { bus.read(addr) } else { self.get8(z, Idx::Hl) };
        let r = match xx {
            0 => self.rot(y, v),
            1 => {
                let bit = v & (1 << y);
                self.f = (self.f & FC) | FH | if bit == 0 { FZ | FP } else { 0 } | (bit & FS);
                // X/Y come from the value (or the address high byte for indexed/memory forms).
                self.f |= if mem { (addr >> 8) as u8 & (FX | FY) } else { v & (FX | FY) };
                return if mem { if x == Idx::Hl { 12 } else { 16 } } else { 8 };
            }
            2 => v & !(1 << y),
            _ => v | (1 << y),
        };
        if mem {
            bus.write(addr, r);
            // Undocumented: indexed forms also copy the result into a register.
            if x != Idx::Hl && z != 6 {
                self.set8(z, Idx::Hl, r);
            }
            if x == Idx::Hl {
                15
            } else {
                19
            }
        } else {
            self.set8(z, Idx::Hl, r);
            8
        }
    }

    fn block_io_flags(&mut self, v: u8, k: u16) {
        self.f = (self.b & (FS | FX | FY)) | if self.b == 0 { FZ } else { 0 } | if v & 0x80 != 0 { FN } else { 0 } | if k > 0xFF { FH | FC } else { 0 } | if parity(((k & 7) as u8) ^ self.b) { FP } else { 0 };
    }

    fn exec_ed<B: Bus>(&mut self, bus: &mut B, op: u8) -> u32 {
        let (xx, y, z) = (op >> 6, (op >> 3) & 7, op & 7);
        let (p, q) = (y >> 1, y & 1);
        if xx == 1 {
            return match z {
                0 => {
                    let v = bus.input(self.bc());
                    if y != 6 {
                        self.set8(y, Idx::Hl, v);
                    }
                    self.f = (self.f & FC) | szp(v);
                    12
                }
                1 => {
                    let v = if y == 6 { 0 } else { self.get8(y, Idx::Hl) };
                    bus.output(self.bc(), v);
                    12
                }
                2 => {
                    let r = if q == 0 { self.sbc16(self.hl(), self.rp(p, Idx::Hl)) } else { self.adc16(self.hl(), self.rp(p, Idx::Hl)) };
                    self.set_hl(r);
                    15
                }
                3 => {
                    let a = self.fetch16(bus);
                    if q == 0 {
                        Self::write16(bus, a, self.rp(p, Idx::Hl));
                    } else {
                        let v = Self::read16(bus, a);
                        self.set_rp(p, Idx::Hl, v);
                    }
                    20
                }
                4 => {
                    let a = self.a;
                    self.a = 0;
                    self.sub8(a, false, true);
                    8
                }
                5 => {
                    // RETN / RETI
                    self.iff1 = self.iff2;
                    self.pc = self.pop(bus);
                    14
                }
                6 => {
                    self.im = match y & 3 {
                        0 | 1 => 0,
                        2 => 1,
                        _ => 2,
                    };
                    8
                }
                _ => match y {
                    0 => {
                        self.i = self.a;
                        9
                    }
                    1 => {
                        self.r = self.a;
                        9
                    }
                    2 | 3 => {
                        self.a = if y == 2 { self.i } else { self.r };
                        self.f = (self.f & FC) | (self.a & (FS | FX | FY)) | if self.a == 0 { FZ } else { 0 } | if self.iff2 { FP } else { 0 };
                        9
                    }
                    4 | 5 => {
                        // RRD / RLD
                        let hl = self.hl();
                        let m = bus.read(hl);
                        let (nm, na) = if y == 4 { ((self.a << 4) | (m >> 4), (self.a & 0xF0) | (m & 0xF)) } else { ((m << 4) | (self.a & 0xF), (self.a & 0xF0) | (m >> 4)) };
                        bus.write(hl, nm);
                        self.a = na;
                        self.f = (self.f & FC) | szp(self.a);
                        18
                    }
                    _ => 8,
                },
            };
        }
        if xx == 2 && z <= 3 && y >= 4 {
            let (inc, repeat) = (y & 1 == 0, y >= 6);
            let step = |v: u16| if inc { v.wrapping_add(1) } else { v.wrapping_sub(1) };
            let mut cycles = 16;
            match z {
                0 => {
                    // LDI/LDD/LDIR/LDDR
                    let v = bus.read(self.hl());
                    bus.write(self.de(), v);
                    self.set_hl(step(self.hl()));
                    self.set_de(step(self.de()));
                    self.set_bc(self.bc().wrapping_sub(1));
                    let n = v.wrapping_add(self.a);
                    self.f = (self.f & (FS | FZ | FC)) | (n & FX) | ((n << 4) & FY) | if self.bc() != 0 { FP } else { 0 };
                    if repeat && self.bc() != 0 {
                        self.pc = self.pc.wrapping_sub(2);
                        cycles = 21;
                    }
                }
                1 => {
                    // CPI/CPD/CPIR/CPDR
                    let v = bus.read(self.hl());
                    let r = self.a.wrapping_sub(v);
                    let h = (self.a & 0xF) < (v & 0xF);
                    self.set_hl(step(self.hl()));
                    self.set_bc(self.bc().wrapping_sub(1));
                    let n = r.wrapping_sub(h as u8);
                    self.f = (self.f & FC) | FN | (r & FS) | if r == 0 { FZ } else { 0 } | if h { FH } else { 0 } | if self.bc() != 0 { FP } else { 0 } | (n & FX) | ((n << 4) & FY);
                    if repeat && self.bc() != 0 && r != 0 {
                        self.pc = self.pc.wrapping_sub(2);
                        cycles = 21;
                    }
                }
                2 => {
                    // INI/IND/INIR/INDR
                    let v = bus.input(self.bc());
                    bus.write(self.hl(), v);
                    self.set_hl(step(self.hl()));
                    self.b = self.b.wrapping_sub(1);
                    let k = v as u16 + (if inc { self.c.wrapping_add(1) } else { self.c.wrapping_sub(1) }) as u16;
                    self.block_io_flags(v, k);
                    if repeat && self.b != 0 {
                        self.pc = self.pc.wrapping_sub(2);
                        cycles = 21;
                    }
                }
                _ => {
                    // OUTI/OUTD/OTIR/OTDR
                    let v = bus.read(self.hl());
                    self.b = self.b.wrapping_sub(1);
                    bus.output(self.bc(), v);
                    self.set_hl(step(self.hl()));
                    let k = v as u16 + self.l as u16;
                    self.block_io_flags(v, k);
                    if repeat && self.b != 0 {
                        self.pc = self.pc.wrapping_sub(2);
                        cycles = 21;
                    }
                }
            }
            return cycles;
        }
        8 // undefined ED opcodes act as NOPs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem {
        ram: Vec<u8>,
        out: Vec<(u16, u8)>,
    }
    impl Bus for Mem {
        fn read(&mut self, a: u16) -> u8 {
            self.ram[a as usize]
        }
        fn write(&mut self, a: u16, v: u8) {
            self.ram[a as usize] = v;
        }
        fn input(&mut self, p: u16) -> u8 {
            p as u8
        }
        fn output(&mut self, p: u16, v: u8) {
            self.out.push((p, v));
        }
    }

    fn run(prog: &[u8], steps: usize) -> (Z80, Mem) {
        let mut m = Mem { ram: vec![0; 0x10000], out: vec![] };
        m.ram[..prog.len()].copy_from_slice(prog);
        let mut z = Z80::new();
        z.sp = 0xF000;
        for _ in 0..steps {
            z.step(&mut m);
        }
        (z, m)
    }

    #[test]
    fn loads_arithmetic_and_flags() {
        // LD A,0x7F ; ADD A,1 -> 0x80, overflow and half carry ; LD B,A ; SUB 0x80 -> 0, Z
        let (z, _) = run(&[0x3E, 0x7F, 0xC6, 0x01, 0x47], 3);
        assert_eq!(z.a, 0x80);
        assert_eq!(z.f & (FS | FP | FH | FZ | FC), FS | FP | FH);
        let (z, _) = run(&[0x3E, 0x80, 0xD6, 0x80], 2);
        assert_eq!(z.a, 0);
        assert_eq!(z.f & (FZ | FN | FC), FZ | FN);
    }

    #[test]
    fn djnz_loop_and_16bit() {
        // LD B,5 ; LD HL,0 ; loop: INC HL ; DJNZ loop ; LD (0x8000),HL
        let (z, m) = run(&[0x06, 5, 0x21, 0, 0, 0x23, 0x10, 0xFD, 0x22, 0x00, 0x80], 2 + 10 + 1);
        assert_eq!(z.hl(), 5);
        assert_eq!(m.ram[0x8000], 5);
    }

    #[test]
    fn call_ret_push_pop() {
        // LD BC,0x1234 ; PUSH BC ; POP DE ; CALL 0x0010 ; HALT ; (0x10:) LD A,0x42 ; RET
        let mut p = vec![0x01, 0x34, 0x12, 0xC5, 0xD1, 0xCD, 0x10, 0x00, 0x76];
        p.resize(0x10, 0);
        p.extend([0x3E, 0x42, 0xC9]);
        let (z, _) = run(&p, 7);
        assert_eq!(z.de(), 0x1234);
        assert_eq!(z.a, 0x42);
        assert!(z.halted);
        assert_eq!(z.sp, 0xF000);
    }

    #[test]
    fn indexed_and_cb() {
        // LD IX,0x8000 ; LD (IX+5),0x81 ; RLC (IX+5) ; BIT 0,(IX+5) ; LD A,(IX+5)
        let (z, m) = run(&[0xDD, 0x21, 0x00, 0x80, 0xDD, 0x36, 0x05, 0x81, 0xDD, 0xCB, 0x05, 0x06, 0xDD, 0xCB, 0x05, 0x46, 0xDD, 0x7E, 0x05], 5);
        assert_eq!(m.ram[0x8005], 0x03);
        assert_eq!(z.a, 0x03);
        assert_eq!(z.f & FZ, 0, "bit 0 set");
        assert_eq!(z.f & FC, FC, "RLC carried out bit 7");
    }

    #[test]
    fn ldir_and_out() {
        // LD HL,0x100 ; LD DE,0x200 ; LD BC,3 ; LDIR ; LD A,7 ; OUT (0xBE),A
        let mut p = vec![0x21, 0x00, 0x01, 0x11, 0x00, 0x02, 0x01, 0x03, 0x00, 0xED, 0xB0, 0x3E, 0x07, 0xD3, 0xBE];
        p.resize(0x100, 0);
        p.extend([9, 8, 7]);
        let (z, m) = run(&p, 3 + 3 + 2);
        assert_eq!(&m.ram[0x200..0x203], &[9, 8, 7]);
        assert_eq!(z.bc(), 0);
        assert_eq!(m.out, vec![(0x07BE, 7)]);
    }

    #[test]
    fn daa_after_bcd_add() {
        // LD A,0x19 ; ADD A,0x28 ; DAA -> 0x47
        let (z, _) = run(&[0x3E, 0x19, 0xC6, 0x28, 0x27], 3);
        assert_eq!(z.a, 0x47);
    }

    #[test]
    fn im1_interrupt() {
        // EI ; IM 1 ; HALT   with an interrupt handler at 0x38: LD A,0x55 ; HALT
        let mut p = vec![0xFB, 0xED, 0x56, 0x76];
        p.resize(0x38, 0);
        p.extend([0x3E, 0x55, 0x76]);
        let mut m = Mem { ram: vec![0; 0x10000], out: vec![] };
        m.ram[..p.len()].copy_from_slice(&p);
        let mut z = Z80::new();
        z.sp = 0xF000;
        for _ in 0..3 {
            z.step(&mut m);
        }
        assert!(z.halted);
        z.irq_line = true;
        z.step(&mut m);
        z.irq_line = false;
        z.step(&mut m);
        assert_eq!(z.a, 0x55);
    }
}
