//! Motorola 68000 interpreter: all addressing modes and the full instruction set (moves,
//! arithmetic, logic, BCD, bit ops, shifts/rotates, MOVEM, branches, traps, exceptions and
//! autovectored interrupts). Timing is approximated as 4 cycles per bus access plus the extra
//! internal cycles of multiply, divide and shifts.

pub trait Bus {
    fn read8(&mut self, a: u32) -> u8;
    fn read16(&mut self, a: u32) -> u16;
    fn write8(&mut self, a: u32, v: u8);
    fn write16(&mut self, a: u32, v: u16);
}

const C: u16 = 1;
const V: u16 = 2;
const Z: u16 = 4;
const N: u16 = 8;
const X: u16 = 16;
const SUP: u16 = 0x2000;
const TRACE: u16 = 0x8000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sz {
    B,
    W,
    L,
}

impl Sz {
    fn bytes(self) -> u32 {
        match self {
            Sz::B => 1,
            Sz::W => 2,
            Sz::L => 4,
        }
    }
    fn mask(self) -> u32 {
        match self {
            Sz::B => 0xFF,
            Sz::W => 0xFFFF,
            Sz::L => 0xFFFF_FFFF,
        }
    }
    fn msb(self) -> u32 {
        match self {
            Sz::B => 0x80,
            Sz::W => 0x8000,
            Sz::L => 0x8000_0000,
        }
    }
    /// Standard size field (bits 7-6): 0 byte, 1 word, 2 long.
    fn from_bits(b: u16) -> Option<Sz> {
        match b & 3 {
            0 => Some(Sz::B),
            1 => Some(Sz::W),
            2 => Some(Sz::L),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Ea {
    D(usize),
    A(usize),
    Mem(u32),
    Imm(u32),
}

#[derive(state::Stateful)]
pub struct M68k {
    pub d: [u32; 8],
    pub a: [u32; 8],
    /// The inactive stack pointer (USP in supervisor mode, SSP in user mode).
    pub other_sp: u32,
    pub pc: u32,
    pub sr: u16,
    pub stopped: bool,
    /// Interrupt level requested by the hardware (0 = none).
    pub irq_level: u8,
    cycles: u32,
}

impl Default for M68k {
    fn default() -> Self {
        Self::new()
    }
}

fn sext8(v: u32) -> u32 {
    v as u8 as i8 as i32 as u32
}
fn sext16(v: u32) -> u32 {
    v as u16 as i16 as i32 as u32
}

impl M68k {
    pub fn new() -> M68k {
        M68k { d: [0; 8], a: [0; 8], other_sp: 0, pc: 0, sr: 0x2700, stopped: false, irq_level: 0, cycles: 0 }
    }

    /// Reset: SSP and PC from vectors 0 and 1.
    pub fn reset<B: Bus>(&mut self, bus: &mut B) {
        self.sr = 0x2700;
        self.stopped = false;
        self.a[7] = self.rd32(bus, 0);
        self.pc = self.rd32(bus, 4);
    }

    // ---------------------------------------------------------------- bus

    fn rd8<B: Bus>(&mut self, bus: &mut B, a: u32) -> u32 {
        self.cycles += 4;
        bus.read8(a & 0xFF_FFFF) as u32
    }
    fn rd16<B: Bus>(&mut self, bus: &mut B, a: u32) -> u32 {
        self.cycles += 4;
        bus.read16(a & 0xFF_FFFE) as u32
    }
    fn rd32<B: Bus>(&mut self, bus: &mut B, a: u32) -> u32 {
        (self.rd16(bus, a) << 16) | self.rd16(bus, a.wrapping_add(2))
    }
    fn wr8<B: Bus>(&mut self, bus: &mut B, a: u32, v: u32) {
        self.cycles += 4;
        bus.write8(a & 0xFF_FFFF, v as u8);
    }
    fn wr16<B: Bus>(&mut self, bus: &mut B, a: u32, v: u32) {
        self.cycles += 4;
        bus.write16(a & 0xFF_FFFE, v as u16);
    }
    fn wr32<B: Bus>(&mut self, bus: &mut B, a: u32, v: u32) {
        self.wr16(bus, a, v >> 16);
        self.wr16(bus, a.wrapping_add(2), v);
    }
    fn rd<B: Bus>(&mut self, bus: &mut B, a: u32, s: Sz) -> u32 {
        match s {
            Sz::B => self.rd8(bus, a),
            Sz::W => self.rd16(bus, a),
            Sz::L => self.rd32(bus, a),
        }
    }
    fn wr<B: Bus>(&mut self, bus: &mut B, a: u32, v: u32, s: Sz) {
        match s {
            Sz::B => self.wr8(bus, a, v),
            Sz::W => self.wr16(bus, a, v),
            Sz::L => self.wr32(bus, a, v),
        }
    }
    fn fetch16<B: Bus>(&mut self, bus: &mut B) -> u32 {
        let v = self.rd16(bus, self.pc);
        self.pc = self.pc.wrapping_add(2);
        v
    }
    fn fetch32<B: Bus>(&mut self, bus: &mut B) -> u32 {
        let hi = self.fetch16(bus);
        (hi << 16) | self.fetch16(bus)
    }
    fn push16<B: Bus>(&mut self, bus: &mut B, v: u32) {
        self.a[7] = self.a[7].wrapping_sub(2);
        self.wr16(bus, self.a[7], v);
    }
    fn push32<B: Bus>(&mut self, bus: &mut B, v: u32) {
        self.a[7] = self.a[7].wrapping_sub(4);
        self.wr32(bus, self.a[7], v);
    }
    fn pop16<B: Bus>(&mut self, bus: &mut B) -> u32 {
        let v = self.rd16(bus, self.a[7]);
        self.a[7] = self.a[7].wrapping_add(2);
        v
    }
    fn pop32<B: Bus>(&mut self, bus: &mut B) -> u32 {
        let v = self.rd32(bus, self.a[7]);
        self.a[7] = self.a[7].wrapping_add(4);
        v
    }

    // ---------------------------------------------------------------- status register

    pub fn set_sr(&mut self, v: u16) {
        let v = v & 0xA71F;
        if (v ^ self.sr) & SUP != 0 {
            std::mem::swap(&mut self.a[7], &mut self.other_sp);
        }
        self.sr = v;
    }
    fn flag(&mut self, f: u16, on: bool) {
        if on {
            self.sr |= f
        } else {
            self.sr &= !f
        }
    }
    fn nz(&mut self, v: u32, s: Sz) {
        let v = v & s.mask();
        self.flag(N, v & s.msb() != 0);
        self.flag(Z, v == 0);
    }
    fn logic(&mut self, v: u32, s: Sz) {
        self.nz(v, s);
        self.sr &= !(V | C);
    }
    fn cond(&self, c: u16) -> bool {
        let f = self.sr;
        let (cc, vv, zz, nn) = (f & C != 0, f & V != 0, f & Z != 0, f & N != 0);
        match c & 15 {
            0 => true,
            1 => false,
            2 => !cc && !zz,
            3 => cc || zz,
            4 => !cc,
            5 => cc,
            6 => !zz,
            7 => zz,
            8 => !vv,
            9 => vv,
            10 => !nn,
            11 => nn,
            12 => nn == vv,
            13 => nn != vv,
            14 => !zz && nn == vv,
            _ => zz || nn != vv,
        }
    }

    // ---------------------------------------------------------------- effective addresses

    fn index<B: Bus>(&mut self, bus: &mut B, base: u32) -> u32 {
        let ext = self.fetch16(bus);
        let r = ((ext >> 12) & 7) as usize;
        let xn = if ext & 0x8000 != 0 { self.a[r] } else { self.d[r] };
        let xn = if ext & 0x0800 != 0 { xn } else { sext16(xn) };
        base.wrapping_add(xn).wrapping_add(sext8(ext))
    }

    fn ea<B: Bus>(&mut self, bus: &mut B, mode: u16, reg: u16, s: Sz) -> Ea {
        let r = reg as usize;
        match mode {
            0 => Ea::D(r),
            1 => Ea::A(r),
            2 => Ea::Mem(self.a[r]),
            3 => {
                let a = self.a[r];
                let inc = if r == 7 && s == Sz::B { 2 } else { s.bytes() };
                self.a[r] = a.wrapping_add(inc);
                Ea::Mem(a)
            }
            4 => {
                let dec = if r == 7 && s == Sz::B { 2 } else { s.bytes() };
                self.a[r] = self.a[r].wrapping_sub(dec);
                self.cycles += 2;
                Ea::Mem(self.a[r])
            }
            5 => {
                let d = sext16(self.fetch16(bus));
                Ea::Mem(self.a[r].wrapping_add(d))
            }
            6 => {
                self.cycles += 2;
                let base = self.a[r];
                Ea::Mem(self.index(bus, base))
            }
            _ => match reg {
                0 => Ea::Mem(sext16(self.fetch16(bus))),
                1 => Ea::Mem(self.fetch32(bus)),
                2 => {
                    let base = self.pc;
                    let d = sext16(self.fetch16(bus));
                    Ea::Mem(base.wrapping_add(d))
                }
                3 => {
                    self.cycles += 2;
                    let base = self.pc;
                    Ea::Mem(self.index(bus, base))
                }
                _ => Ea::Imm(match s {
                    Sz::B => self.fetch16(bus) & 0xFF,
                    Sz::W => self.fetch16(bus),
                    Sz::L => self.fetch32(bus),
                }),
            },
        }
    }

    fn read_ea<B: Bus>(&mut self, bus: &mut B, e: Ea, s: Sz) -> u32 {
        match e {
            Ea::D(r) => self.d[r] & s.mask(),
            Ea::A(r) => self.a[r] & s.mask(),
            Ea::Mem(a) => self.rd(bus, a, s),
            Ea::Imm(v) => v & s.mask(),
        }
    }

    fn write_ea<B: Bus>(&mut self, bus: &mut B, e: Ea, v: u32, s: Sz) {
        match e {
            Ea::D(r) => self.d[r] = (self.d[r] & !s.mask()) | (v & s.mask()),
            Ea::A(r) => self.a[r] = if s == Sz::W { sext16(v) } else { v },
            Ea::Mem(a) => self.wr(bus, a, v, s),
            Ea::Imm(_) => {}
        }
    }

    // ---------------------------------------------------------------- arithmetic

    fn add(&mut self, a: u32, b: u32, x: u32, s: Sz, addx: bool) -> u32 {
        let m = s.mask();
        let r = (a & m).wrapping_add(b & m).wrapping_add(x);
        let res = r & m;
        let carry = if s == Sz::L { (a as u64 + b as u64 + x as u64) > 0xFFFF_FFFF } else { r > m };
        let over = (!(a ^ b) & (a ^ res)) & s.msb() != 0;
        self.flag(C, carry);
        self.flag(X, carry);
        self.flag(V, over);
        self.flag(N, res & s.msb() != 0);
        if addx {
            if res != 0 {
                self.sr &= !Z;
            }
        } else {
            self.flag(Z, res == 0);
        }
        res
    }

    fn sub(&mut self, a: u32, b: u32, x: u32, s: Sz, subx: bool, set_x: bool) -> u32 {
        let m = s.mask();
        let res = (a & m).wrapping_sub(b & m).wrapping_sub(x) & m;
        let borrow = ((b & m) as u64 + x as u64) > (a & m) as u64;
        let over = ((a ^ b) & (a ^ res)) & s.msb() != 0;
        self.flag(C, borrow);
        if set_x {
            self.flag(X, borrow);
        }
        self.flag(V, over);
        self.flag(N, res & s.msb() != 0);
        if subx {
            if res != 0 {
                self.sr &= !Z;
            }
        } else {
            self.flag(Z, res == 0);
        }
        res
    }

    fn bcd_add(&mut self, a: u32, b: u32) -> u32 {
        let x = ((self.sr & X) != 0) as u32;
        let mut lo = (a & 0xF) + (b & 0xF) + x;
        let mut carry = 0;
        if lo > 9 {
            lo += 6;
        }
        let mut hi = (a >> 4 & 0xF) + (b >> 4 & 0xF) + (lo >> 4);
        if hi > 9 {
            hi += 6;
            carry = 1;
        }
        let r = ((hi << 4) | (lo & 0xF)) & 0xFF;
        self.flag(C, carry != 0);
        self.flag(X, carry != 0);
        if r != 0 {
            self.sr &= !Z;
        }
        r
    }

    fn bcd_sub(&mut self, a: u32, b: u32) -> u32 {
        let x = ((self.sr & X) != 0) as i32;
        let mut lo = (a & 0xF) as i32 - (b & 0xF) as i32 - x;
        let mut hi = (a >> 4 & 0xF) as i32 - (b >> 4 & 0xF) as i32;
        if lo < 0 {
            lo += 10;
            hi -= 1;
        }
        let borrow = hi < 0;
        if borrow {
            hi += 10;
        }
        let r = (((hi as u32) << 4) | (lo as u32 & 0xF)) & 0xFF;
        self.flag(C, borrow);
        self.flag(X, borrow);
        if r != 0 {
            self.sr &= !Z;
        }
        r
    }

    // ---------------------------------------------------------------- exceptions

    fn exception<B: Bus>(&mut self, bus: &mut B, vector: u32) {
        let old = self.sr;
        self.set_sr((self.sr | SUP) & !TRACE);
        let pc = self.pc;
        self.push32(bus, pc);
        self.push16(bus, old as u32);
        self.pc = self.rd32(bus, vector * 4);
        self.cycles += 20;
    }

    /// Executes one instruction or takes an interrupt; returns cycles.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> u32 {
        self.cycles = 0;
        let level = self.irq_level & 7;
        let mask = ((self.sr >> 8) & 7) as u8;
        if level > mask {
            self.stopped = false;
            self.exception(bus, 24 + level as u32);
            self.sr = (self.sr & !0x0700) | (level as u16) << 8;
            return self.cycles + 24;
        }
        if self.stopped {
            return 4;
        }
        let op = self.fetch16(bus) as u16;
        self.execute(bus, op);
        self.cycles.max(4)
    }

    fn privileged<B: Bus>(&mut self, bus: &mut B) -> bool {
        if self.sr & SUP == 0 {
            self.pc = self.pc.wrapping_sub(2);
            self.exception(bus, 8);
            return false;
        }
        true
    }

    fn execute<B: Bus>(&mut self, bus: &mut B, op: u16) {
        let (mode, reg) = ((op >> 3) & 7, op & 7);
        let rx = ((op >> 9) & 7) as usize;
        match op >> 12 {
            0x0 => self.group0(bus, op),
            0x1 | 0x2 | 0x3 => {
                let s = match op >> 12 {
                    1 => Sz::B,
                    2 => Sz::L,
                    _ => Sz::W,
                };
                let src = self.ea(bus, mode, reg, s);
                let v = self.read_ea(bus, src, s);
                let dmode = (op >> 6) & 7;
                if dmode == 1 {
                    // MOVEA: no flags, word sign-extended.
                    self.a[rx] = if s == Sz::W { sext16(v) } else { v };
                } else {
                    let dst = self.ea(bus, dmode, rx as u16, s);
                    self.write_ea(bus, dst, v, s);
                    self.logic(v, s);
                }
            }
            0x4 => self.group4(bus, op),
            0x5 => {
                let size_bits = (op >> 6) & 3;
                if size_bits == 3 {
                    let c = (op >> 8) & 15;
                    if mode == 1 {
                        // DBcc
                        let disp = sext16(self.fetch16(bus));
                        if !self.cond(c) {
                            let r = reg as usize;
                            let cnt = (self.d[r] as u16).wrapping_sub(1);
                            self.d[r] = (self.d[r] & 0xFFFF_0000) | cnt as u32;
                            if cnt != 0xFFFF {
                                self.pc = self.pc.wrapping_sub(2).wrapping_add(disp);
                                self.cycles += 2;
                            }
                        }
                    } else {
                        // Scc
                        let e = self.ea(bus, mode, reg, Sz::B);
                        let v = if self.cond(c) { 0xFF } else { 0 };
                        self.write_ea(bus, e, v, Sz::B);
                    }
                } else {
                    let s = Sz::from_bits(size_bits).unwrap();
                    let q = match (op >> 9) & 7 {
                        0 => 8,
                        n => n as u32,
                    };
                    let sub = op & 0x0100 != 0;
                    if mode == 1 {
                        // ADDQ/SUBQ to An: whole register, no flags.
                        let r = reg as usize;
                        self.a[r] = if sub { self.a[r].wrapping_sub(q) } else { self.a[r].wrapping_add(q) };
                    } else {
                        let e = self.ea(bus, mode, reg, s);
                        let v = self.read_ea(bus, e, s);
                        let r = if sub { self.sub(v, q, 0, s, false, true) } else { self.add(v, q, 0, s, false) };
                        self.write_ea(bus, e, r, s);
                    }
                }
            }
            0x6 => {
                let c = (op >> 8) & 15;
                let base = self.pc;
                let disp = match op & 0xFF {
                    0 => sext16(self.fetch16(bus)),
                    d => sext8(d as u32),
                };
                if c == 1 {
                    // BSR
                    let ret = self.pc;
                    self.push32(bus, ret);
                    self.pc = base.wrapping_add(disp);
                } else if self.cond(c) {
                    self.pc = base.wrapping_add(disp);
                    self.cycles += 2;
                }
            }
            0x7 => {
                let v = sext8((op & 0xFF) as u32);
                self.d[rx] = v;
                self.logic(v, Sz::L);
            }
            0x8 | 0xC => self.group8c(bus, op),
            0x9 | 0xD => self.group9d(bus, op),
            0xB => {
                let opmode = (op >> 6) & 7;
                match opmode {
                    3 | 7 => {
                        // CMPA
                        let s = if opmode == 3 { Sz::W } else { Sz::L };
                        let e = self.ea(bus, mode, reg, s);
                        let v = self.read_ea(bus, e, s);
                        let v = if s == Sz::W { sext16(v) } else { v };
                        self.sub(self.a[rx], v, 0, Sz::L, false, false);
                    }
                    0..=2 => {
                        let s = Sz::from_bits(opmode).unwrap();
                        let e = self.ea(bus, mode, reg, s);
                        let v = self.read_ea(bus, e, s);
                        self.sub(self.d[rx], v, 0, s, false, false);
                    }
                    _ => {
                        let s = Sz::from_bits(opmode - 4).unwrap();
                        if mode == 1 {
                            // CMPM (Ay)+,(Ax)+
                            let src = self.ea(bus, 3, reg, s);
                            let a = self.read_ea(bus, src, s);
                            let dst = self.ea(bus, 3, rx as u16, s);
                            let b = self.read_ea(bus, dst, s);
                            self.sub(b, a, 0, s, false, false);
                        } else {
                            // EOR Dn,<ea>
                            let e = self.ea(bus, mode, reg, s);
                            let v = self.read_ea(bus, e, s) ^ self.d[rx];
                            self.write_ea(bus, e, v, s);
                            self.logic(v, s);
                        }
                    }
                }
            }
            0xE => self.group_e(bus, op),
            0xA => {
                self.pc = self.pc.wrapping_sub(2);
                self.exception(bus, 10);
            }
            _ => {
                self.pc = self.pc.wrapping_sub(2);
                self.exception(bus, 11);
            }
        }
    }

    fn bit_op<B: Bus>(&mut self, bus: &mut B, kind: u16, bitnum: u32, mode: u16, reg: u16) {
        let (s, bit) = if mode == 0 { (Sz::L, bitnum & 31) } else { (Sz::B, bitnum & 7) };
        let e = self.ea(bus, mode, reg, s);
        let v = self.read_ea(bus, e, s);
        self.flag(Z, v & (1 << bit) == 0);
        let r = match kind {
            1 => v ^ (1 << bit),
            2 => v & !(1 << bit),
            3 => v | (1 << bit),
            _ => return,
        };
        self.write_ea(bus, e, r, s);
    }

    fn group0<B: Bus>(&mut self, bus: &mut B, op: u16) {
        let (mode, reg) = ((op >> 3) & 7, op & 7);
        if op & 0x0100 != 0 {
            let dn = ((op >> 9) & 7) as usize;
            if mode == 1 {
                // MOVEP
                let disp = sext16(self.fetch16(bus));
                let addr = self.a[reg as usize].wrapping_add(disp);
                let long = op & 0x40 != 0;
                let n = if long { 4 } else { 2 };
                if op & 0x80 != 0 {
                    for i in 0..n {
                        let byte = self.d[dn] >> ((n - 1 - i) * 8);
                        self.wr8(bus, addr.wrapping_add(i * 2), byte);
                    }
                } else {
                    let mut v = 0;
                    for i in 0..n {
                        v = (v << 8) | self.rd8(bus, addr.wrapping_add(i * 2));
                    }
                    self.d[dn] = if long { v } else { (self.d[dn] & 0xFFFF_0000) | v };
                }
                return;
            }
            let bitnum = self.d[dn];
            self.bit_op(bus, (op >> 6) & 3, bitnum, mode, reg);
            return;
        }
        let kind = (op >> 9) & 7;
        if kind == 4 {
            let bitnum = self.fetch16(bus) & 0xFF;
            self.bit_op(bus, (op >> 6) & 3, bitnum, mode, reg);
            return;
        }
        // Immediate to CCR / SR.
        if op & 0xFF == 0x3C || op & 0xFF == 0x7C {
            let to_sr = op & 0xFF == 0x7C;
            if to_sr && !self.privileged(bus) {
                return;
            }
            let imm = self.fetch16(bus) as u16;
            let cur = if to_sr { self.sr } else { self.sr & 0xFF };
            let r = match kind {
                0 => cur | imm,
                1 => cur & imm,
                _ => cur ^ imm,
            };
            if to_sr {
                self.set_sr(r);
            } else {
                self.sr = (self.sr & 0xFF00) | (r & 0x1F);
            }
            return;
        }
        let Some(s) = Sz::from_bits(op >> 6) else { return };
        let imm = match s {
            Sz::B => self.fetch16(bus) & 0xFF,
            Sz::W => self.fetch16(bus),
            Sz::L => self.fetch32(bus),
        };
        let e = self.ea(bus, mode, reg, s);
        let v = self.read_ea(bus, e, s);
        let r = match kind {
            0 => {
                let r = v | imm;
                self.logic(r, s);
                r
            }
            1 => {
                let r = v & imm;
                self.logic(r, s);
                r
            }
            2 => self.sub(v, imm, 0, s, false, true),
            3 => self.add(v, imm, 0, s, false),
            5 => {
                let r = v ^ imm;
                self.logic(r, s);
                r
            }
            6 => {
                self.sub(v, imm, 0, s, false, false);
                return;
            }
            _ => return,
        };
        self.write_ea(bus, e, r, s);
    }

    fn group4<B: Bus>(&mut self, bus: &mut B, op: u16) {
        let (mode, reg) = ((op >> 3) & 7, op & 7);
        let rx = ((op >> 9) & 7) as usize;
        if op & 0x01C0 == 0x01C0 {
            // LEA
            let e = self.ea(bus, mode, reg, Sz::L);
            if let Ea::Mem(a) = e {
                self.a[rx] = a;
            }
            return;
        }
        if op & 0x01C0 == 0x0180 {
            // CHK.W
            let e = self.ea(bus, mode, reg, Sz::W);
            let bound = self.read_ea(bus, e, Sz::W) as u16 as i16;
            let v = self.d[rx] as u16 as i16;
            if v < 0 || v > bound {
                self.flag(N, v < 0);
                self.exception(bus, 6);
            }
            return;
        }
        match op {
            0x4AFC => {
                self.pc = self.pc.wrapping_sub(2);
                self.exception(bus, 4);
                return;
            }
            0x4E70 => {
                let _ = self.privileged(bus);
                return;
            }
            0x4E71 => return,
            0x4E72 => {
                if self.privileged(bus) {
                    let v = self.fetch16(bus) as u16;
                    self.set_sr(v);
                    self.stopped = true;
                }
                return;
            }
            0x4E73 => {
                if self.privileged(bus) {
                    let sr = self.pop16(bus) as u16;
                    self.pc = self.pop32(bus);
                    self.set_sr(sr);
                }
                return;
            }
            0x4E75 => {
                self.pc = self.pop32(bus);
                return;
            }
            0x4E76 => {
                if self.sr & V != 0 {
                    self.exception(bus, 7);
                }
                return;
            }
            0x4E77 => {
                let ccr = self.pop16(bus) as u16;
                self.sr = (self.sr & 0xFF00) | (ccr & 0x1F);
                self.pc = self.pop32(bus);
                return;
            }
            _ => {}
        }
        match op & 0xFFF0 {
            0x4E40 => {
                self.exception(bus, 32 + (op & 15) as u32);
                return;
            }
            0x4E50 => {
                let r = (op & 7) as usize;
                if op & 8 == 0 {
                    // LINK
                    let disp = sext16(self.fetch16(bus));
                    let v = self.a[r];
                    self.push32(bus, v);
                    self.a[r] = self.a[7];
                    self.a[7] = self.a[7].wrapping_add(disp);
                } else {
                    // UNLK
                    self.a[7] = self.a[r];
                    self.a[r] = self.pop32(bus);
                }
                return;
            }
            0x4E60 => {
                if self.privileged(bus) {
                    let r = (op & 7) as usize;
                    if op & 8 == 0 {
                        self.other_sp = self.a[r];
                    } else {
                        self.a[r] = self.other_sp;
                    }
                }
                return;
            }
            _ => {}
        }
        match op & 0xFFC0 {
            0x40C0 => {
                // MOVE from SR
                let e = self.ea(bus, mode, reg, Sz::W);
                let v = self.sr as u32;
                self.write_ea(bus, e, v, Sz::W);
                return;
            }
            0x44C0 => {
                let e = self.ea(bus, mode, reg, Sz::W);
                let v = self.read_ea(bus, e, Sz::W) as u16;
                self.sr = (self.sr & 0xFF00) | (v & 0x1F);
                return;
            }
            0x46C0 => {
                if self.privileged(bus) {
                    let e = self.ea(bus, mode, reg, Sz::W);
                    let v = self.read_ea(bus, e, Sz::W) as u16;
                    self.set_sr(v);
                }
                return;
            }
            0x4800 => {
                // NBCD
                let e = self.ea(bus, mode, reg, Sz::B);
                let v = self.read_ea(bus, e, Sz::B);
                let r = self.bcd_sub(0, v);
                self.write_ea(bus, e, r, Sz::B);
                return;
            }
            0x4840 => {
                if mode == 0 {
                    // SWAP
                    let r = reg as usize;
                    self.d[r] = self.d[r].rotate_left(16);
                    self.logic(self.d[r], Sz::L);
                } else {
                    // PEA
                    let e = self.ea(bus, mode, reg, Sz::L);
                    if let Ea::Mem(a) = e {
                        self.push32(bus, a);
                    }
                }
                return;
            }
            0x4880 | 0x48C0 if mode == 0 => {
                // EXT.W / EXT.L
                let r = reg as usize;
                if op & 0x40 == 0 {
                    let v = sext8(self.d[r]) & 0xFFFF;
                    self.d[r] = (self.d[r] & 0xFFFF_0000) | v;
                    self.logic(v, Sz::W);
                } else {
                    self.d[r] = sext16(self.d[r]);
                    self.logic(self.d[r], Sz::L);
                }
                return;
            }
            0x4880 | 0x48C0 | 0x4C80 | 0x4CC0 => {
                self.movem(bus, op);
                return;
            }
            0x4AC0 => {
                // TAS
                let e = self.ea(bus, mode, reg, Sz::B);
                let v = self.read_ea(bus, e, Sz::B);
                self.logic(v, Sz::B);
                self.write_ea(bus, e, v | 0x80, Sz::B);
                return;
            }
            0x4E80 | 0x4EC0 => {
                // JSR / JMP
                let e = self.ea(bus, mode, reg, Sz::L);
                if let Ea::Mem(a) = e {
                    if op & 0x40 == 0 {
                        let ret = self.pc;
                        self.push32(bus, ret);
                    }
                    self.pc = a;
                }
                return;
            }
            _ => {}
        }
        let Some(s) = Sz::from_bits(op >> 6) else { return };
        let e = self.ea(bus, mode, reg, s);
        match (op >> 8) & 15 {
            0x0 => {
                // NEGX
                let v = self.read_ea(bus, e, s);
                let x = ((self.sr & X) != 0) as u32;
                let r = self.sub(0, v, x, s, true, true);
                self.write_ea(bus, e, r, s);
            }
            0x2 => {
                // CLR
                self.write_ea(bus, e, 0, s);
                self.sr = (self.sr & !(N | V | C)) | Z;
            }
            0x4 => {
                let v = self.read_ea(bus, e, s);
                let r = self.sub(0, v, 0, s, false, true);
                self.write_ea(bus, e, r, s);
            }
            0x6 => {
                let v = !self.read_ea(bus, e, s) & s.mask();
                self.write_ea(bus, e, v, s);
                self.logic(v, s);
            }
            0xA => {
                let v = self.read_ea(bus, e, s);
                self.logic(v, s);
            }
            _ => {}
        }
    }

    fn movem<B: Bus>(&mut self, bus: &mut B, op: u16) {
        let (mode, reg) = ((op >> 3) & 7, op & 7);
        let s = if op & 0x40 != 0 { Sz::L } else { Sz::W };
        let mask = self.fetch16(bus);
        let to_mem = op & 0x0400 == 0;
        if to_mem && mode == 4 {
            // Predecrement: mask bit 0 = A7 ... bit 15 = D0; store downward.
            let mut addr = self.a[reg as usize];
            for i in 0..16 {
                if mask & (1 << i) != 0 {
                    let r = 15 - i;
                    let v = if r < 8 { self.d[r] } else { self.a[r - 8] };
                    addr = addr.wrapping_sub(s.bytes());
                    self.wr(bus, addr, v, s);
                }
            }
            self.a[reg as usize] = addr;
            return;
        }
        let mut addr = match mode {
            3 => self.a[reg as usize],
            _ => match self.ea(bus, mode, reg, s) {
                Ea::Mem(a) => a,
                _ => return,
            },
        };
        for i in 0..16 {
            if mask & (1 << i) == 0 {
                continue;
            }
            if to_mem {
                let v = if i < 8 { self.d[i] } else { self.a[i - 8] };
                self.wr(bus, addr, v, s);
            } else {
                let v = self.rd(bus, addr, s);
                let v = if s == Sz::W { sext16(v) } else { v };
                if i < 8 {
                    self.d[i] = v;
                } else {
                    self.a[i - 8] = v;
                }
            }
            addr = addr.wrapping_add(s.bytes());
        }
        if mode == 3 {
            self.a[reg as usize] = addr;
        }
    }

    /// OR / DIV / SBCD (0x8) and AND / MUL / ABCD / EXG (0xC).
    fn group8c<B: Bus>(&mut self, bus: &mut B, op: u16) {
        let (mode, reg) = ((op >> 3) & 7, op & 7);
        let rx = ((op >> 9) & 7) as usize;
        let is_and = op >> 12 == 0xC;
        let opmode = (op >> 6) & 7;
        if opmode == 3 || opmode == 7 {
            let e = self.ea(bus, mode, reg, Sz::W);
            let src = self.read_ea(bus, e, Sz::W);
            let signed = opmode == 7;
            if is_and {
                // MULU / MULS
                let r = if signed { (sext16(self.d[rx]) as i32).wrapping_mul(sext16(src) as i32) as u32 } else { (self.d[rx] & 0xFFFF) * src };
                self.d[rx] = r;
                self.logic(r, Sz::L);
                self.cycles += 34;
            } else {
                // DIVU / DIVS
                self.cycles += 120;
                if src == 0 {
                    self.exception(bus, 5);
                    return;
                }
                let dv = self.d[rx];
                if signed {
                    let (n, d) = (dv as i32 as i64, sext16(src) as i32 as i64);
                    let (q, r) = (n / d, n % d);
                    if q < -32768 || q > 32767 {
                        self.sr |= V;
                        return;
                    }
                    self.d[rx] = ((r as u32 & 0xFFFF) << 16) | (q as u32 & 0xFFFF);
                    self.logic(q as u32, Sz::W);
                } else {
                    let (q, r) = (dv / src, dv % src);
                    if q > 0xFFFF {
                        self.sr |= V;
                        return;
                    }
                    self.d[rx] = (r << 16) | q;
                    self.logic(q, Sz::W);
                }
            }
            return;
        }
        if opmode == 4 && mode <= 1 {
            // ABCD / SBCD
            let (a, b, dst) = if mode == 0 {
                (self.d[rx] & 0xFF, self.d[reg as usize] & 0xFF, Ea::D(rx))
            } else {
                let s = self.ea(bus, 4, reg, Sz::B);
                let b = self.read_ea(bus, s, Sz::B);
                let d = self.ea(bus, 4, rx as u16, Sz::B);
                (self.read_ea(bus, d, Sz::B), b, d)
            };
            let r = if is_and { self.bcd_add(a, b) } else { self.bcd_sub(a, b) };
            self.write_ea(bus, dst, r, Sz::B);
            return;
        }
        if is_and && (opmode == 5 || opmode == 6) && mode <= 1 {
            // EXG
            let ry = reg as usize;
            match (opmode, mode) {
                (5, 0) => self.d.swap(rx, ry),
                (5, 1) => self.a.swap(rx, ry),
                _ => std::mem::swap(&mut self.d[rx], &mut self.a[ry]),
            }
            return;
        }
        let s = Sz::from_bits(opmode & 3).unwrap();
        let e = self.ea(bus, mode, reg, s);
        let v = self.read_ea(bus, e, s);
        let r = if is_and { v & self.d[rx] } else { v | self.d[rx] };
        self.logic(r, s);
        if opmode & 4 != 0 {
            self.write_ea(bus, e, r, s);
        } else {
            self.write_ea(bus, Ea::D(rx), r, s);
        }
    }

    /// SUB/SUBA/SUBX (0x9) and ADD/ADDA/ADDX (0xD).
    fn group9d<B: Bus>(&mut self, bus: &mut B, op: u16) {
        let (mode, reg) = ((op >> 3) & 7, op & 7);
        let rx = ((op >> 9) & 7) as usize;
        let is_add = op >> 12 == 0xD;
        let opmode = (op >> 6) & 7;
        if opmode == 3 || opmode == 7 {
            let s = if opmode == 3 { Sz::W } else { Sz::L };
            let e = self.ea(bus, mode, reg, s);
            let v = self.read_ea(bus, e, s);
            let v = if s == Sz::W { sext16(v) } else { v };
            self.a[rx] = if is_add { self.a[rx].wrapping_add(v) } else { self.a[rx].wrapping_sub(v) };
            return;
        }
        let s = Sz::from_bits(opmode & 3).unwrap();
        if opmode >= 4 && mode <= 1 {
            // ADDX / SUBX
            let x = ((self.sr & X) != 0) as u32;
            if mode == 0 {
                let (a, b) = (self.d[rx], self.d[reg as usize]);
                let r = if is_add { self.add(a, b, x, s, true) } else { self.sub(a, b, x, s, true, true) };
                self.write_ea(bus, Ea::D(rx), r, s);
            } else {
                let src = self.ea(bus, 4, reg, s);
                let b = self.read_ea(bus, src, s);
                let dst = self.ea(bus, 4, rx as u16, s);
                let a = self.read_ea(bus, dst, s);
                let r = if is_add { self.add(a, b, x, s, true) } else { self.sub(a, b, x, s, true, true) };
                self.write_ea(bus, dst, r, s);
            }
            return;
        }
        let e = self.ea(bus, mode, reg, s);
        let v = self.read_ea(bus, e, s);
        if opmode & 4 != 0 {
            let r = if is_add { self.add(v, self.d[rx], 0, s, false) } else { self.sub(v, self.d[rx], 0, s, false, true) };
            self.write_ea(bus, e, r, s);
        } else {
            let r = if is_add { self.add(self.d[rx], v, 0, s, false) } else { self.sub(self.d[rx], v, 0, s, false, true) };
            self.write_ea(bus, Ea::D(rx), r, s);
        }
    }

    fn shift_value(&mut self, kind: u16, left: bool, v: u32, count: u32, s: Sz) -> u32 {
        let m = s.mask();
        let msb = s.msb();
        let bits = s.bytes() * 8;
        let mut r = v & m;
        let mut c = false;
        let mut over = false;
        let mut x = self.sr & X != 0;
        for _ in 0..count {
            match (kind, left) {
                (0, true) | (1, true) => {
                    c = r & msb != 0;
                    let nr = (r << 1) & m;
                    if kind == 0 && (nr & msb != 0) != (r & msb != 0) {
                        over = true;
                    }
                    r = nr;
                    x = c;
                }
                (0, false) => {
                    c = r & 1 != 0;
                    r = (r >> 1) | (r & msb);
                    x = c;
                }
                (1, false) => {
                    c = r & 1 != 0;
                    r >>= 1;
                    x = c;
                }
                (2, true) => {
                    c = r & msb != 0;
                    r = ((r << 1) | x as u32) & m;
                    x = c;
                }
                (2, false) => {
                    c = r & 1 != 0;
                    r = (r >> 1) | if x { msb } else { 0 };
                    x = c;
                }
                (_, true) => {
                    c = r & msb != 0;
                    r = ((r << 1) | c as u32) & m;
                }
                _ => {
                    c = r & 1 != 0;
                    r = (r >> 1) | if c { msb } else { 0 };
                }
            }
        }
        let _ = bits;
        self.nz(r, s);
        self.flag(V, over);
        if count == 0 {
            // A zero count clears C (ROX* copies X into C).
            self.flag(C, kind == 2 && self.sr & X != 0);
        } else {
            self.flag(C, c);
            if kind != 3 {
                self.flag(X, x);
            }
        }
        self.cycles += 2 * count;
        r
    }

    fn group_e<B: Bus>(&mut self, bus: &mut B, op: u16) {
        let left = op & 0x0100 != 0;
        if (op >> 6) & 3 == 3 {
            // Memory shift by one, word size.
            let kind = (op >> 9) & 3;
            let e = self.ea(bus, (op >> 3) & 7, op & 7, Sz::W);
            let v = self.read_ea(bus, e, Sz::W);
            let r = self.shift_value(kind, left, v, 1, Sz::W);
            self.write_ea(bus, e, r, Sz::W);
            return;
        }
        let s = Sz::from_bits(op >> 6).unwrap();
        let kind = (op >> 3) & 3;
        let rx = ((op >> 9) & 7) as usize;
        let count = if op & 0x20 != 0 {
            self.d[rx] & 63
        } else {
            match rx {
                0 => 8,
                n => n as u32,
            }
        };
        let r = (op & 7) as usize;
        let v = self.d[r];
        let res = self.shift_value(kind, left, v, count, s);
        self.d[r] = (self.d[r] & !s.mask()) | res;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Bus for Ram {
        fn read8(&mut self, a: u32) -> u8 {
            self.0[a as usize & 0xFFFF]
        }
        fn read16(&mut self, a: u32) -> u16 {
            let i = a as usize & 0xFFFE;
            u16::from_be_bytes([self.0[i], self.0[i + 1]])
        }
        fn write8(&mut self, a: u32, v: u8) {
            self.0[a as usize & 0xFFFF] = v;
        }
        fn write16(&mut self, a: u32, v: u16) {
            let i = a as usize & 0xFFFE;
            self.0[i..i + 2].copy_from_slice(&v.to_be_bytes());
        }
    }

    fn run(prog: &[u16], steps: usize) -> (M68k, Ram) {
        let mut m = Ram(vec![0; 0x1_0000]);
        m.0[0..4].copy_from_slice(&0x8000u32.to_be_bytes()); // SSP
        m.0[4..8].copy_from_slice(&0x1000u32.to_be_bytes()); // PC
        for (i, w) in prog.iter().enumerate() {
            m.0[0x1000 + i * 2..0x1002 + i * 2].copy_from_slice(&w.to_be_bytes());
        }
        let mut c = M68k::new();
        c.reset(&mut m);
        for _ in 0..steps {
            c.step(&mut m);
        }
        (c, m)
    }

    #[test]
    fn moveq_add_and_flags() {
        // MOVEQ #-1,D0 ; MOVEQ #1,D1 ; ADD.L D1,D0 -> 0, Z C X
        let (c, _) = run(&[0x70FF, 0x7201, 0xD081], 3);
        assert_eq!(c.d[0], 0);
        assert_eq!(c.sr & (Z | C | X), Z | C | X);
    }

    #[test]
    fn move_to_memory_and_back() {
        // MOVE.L #$12345678,D0 ; MOVE.L D0,$2000 ; MOVE.W $2002,D1
        let (c, m) = run(&[0x203C, 0x1234, 0x5678, 0x23C0, 0x0000, 0x2000, 0x3239, 0x0000, 0x2002], 3);
        assert_eq!(&m.0[0x2000..0x2004], &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(c.d[1] & 0xFFFF, 0x5678);
    }

    #[test]
    fn dbra_loop_and_postincrement() {
        // LEA $3000,A0 ; MOVEQ #3,D0 ; loop: MOVE.B D0,(A0)+ ; DBRA D0,loop
        let (c, m) = run(&[0x41F8, 0x3000, 0x7003, 0x10C0, 0x51C8, 0xFFFC], 2 + 8);
        assert_eq!(&m.0[0x3000..0x3004], &[3, 2, 1, 0]);
        assert_eq!(c.a[0], 0x3004);
        assert_eq!(c.d[0] & 0xFFFF, 0xFFFF);
    }

    #[test]
    fn jsr_rts_and_movem() {
        // MOVEQ #5,D3 ; JSR $1010 ; STOP-ish (BRA self)
        // $1010: MOVEM.L D3/A0,-(A7) ; MOVEQ #0,D3 ; MOVEM.L (A7)+,D3/A0 ; RTS
        let mut p = vec![0x7605, 0x4EB8, 0x1010, 0x60FE];
        p.resize(8, 0x4E71);
        p.extend([0x48E7, 0x1080, 0x7600, 0x4CDF, 0x0108, 0x4E75]);
        let (c, _) = run(&p, 7);
        assert_eq!(c.d[3], 5, "restored by MOVEM");
        assert_eq!(c.a[7], 0x8000);
        assert_eq!(c.pc, 0x1006);
    }

    #[test]
    fn mul_div_and_shifts() {
        // MOVE.W #300,D0 ; MULU #200,D0 (60000) ; DIVU #7,D0 ; MOVEQ #1,D1 ; LSL.L #4,D1
        let (c, _) = run(&[0x303C, 300, 0xC0FC, 200, 0x80FC, 7, 0x7201, 0xE989], 5);
        assert_eq!(c.d[0] & 0xFFFF, 60000 / 7);
        assert_eq!(c.d[0] >> 16, 60000 % 7);
        assert_eq!(c.d[1], 16);
    }

    #[test]
    fn autovectored_interrupt_and_rte() {
        // Level 6 autovector (30) -> $2000: MOVEQ #42,D7 ; RTE. Main: MOVE #$2000,SR ; BRA self.
        let mut m = Ram(vec![0; 0x1_0000]);
        m.0[0..4].copy_from_slice(&0x8000u32.to_be_bytes());
        m.0[4..8].copy_from_slice(&0x1000u32.to_be_bytes());
        m.0[30 * 4..30 * 4 + 4].copy_from_slice(&0x2000u32.to_be_bytes());
        for (i, w) in [0x46FCu16, 0x2000, 0x60FE].iter().enumerate() {
            m.0[0x1000 + i * 2..0x1002 + i * 2].copy_from_slice(&w.to_be_bytes());
        }
        for (i, w) in [0x7E2Au16, 0x4E73].iter().enumerate() {
            m.0[0x2000 + i * 2..0x2002 + i * 2].copy_from_slice(&w.to_be_bytes());
        }
        let mut c = M68k::new();
        c.reset(&mut m);
        c.step(&mut m);
        c.irq_level = 6;
        c.step(&mut m);
        c.irq_level = 0;
        c.step(&mut m);
        c.step(&mut m);
        assert_eq!(c.d[7], 42);
        assert_eq!(c.pc, 0x1004);
        assert_eq!(c.sr & 0x0700, 0);
    }

    #[test]
    fn bcd_and_bit_ops() {
        // MOVEQ #$19,D0 ; MOVEQ #$28,D1 ; ANDI #0,CCR ; ABCD D1,D0 ; BSET #7,D0 ; BTST #0,D0
        let (c, _) = run(&[0x7019, 0x7228, 0x023C, 0x0000, 0xC101, 0x08C0, 0x0007, 0x0800, 0x0000], 6);
        assert_eq!(c.d[0] & 0xFF, 0xC7, "0x47 | bit 7");
        assert_eq!(c.sr & Z, 0, "bit 0 is set");
    }
}
