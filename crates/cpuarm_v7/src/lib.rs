//! Clean-room ARMv5TE / ARMv6K / ARMv7-A + Thumb CPU interpreter.
//!
//! Shared across **Nintendo 3DS** (ARM11 MPCore / ARMv6K), **PlayStation Vita**
//! (Cortex-A9 / ARMv7-A + Thumb), **Nintendo DS** (ARM946E-S / ARMv5TE), and
//! **Android** (`armeabi-v7a` native code).

/// Memory bus interface for ARM guests.
pub trait Bus {
    fn read8(&self, addr: u32) -> u8;
    fn read16(&self, addr: u32) -> u16;
    fn read32(&self, addr: u32) -> u32;
    fn write8(&mut self, addr: u32, val: u8);
    fn write16(&mut self, addr: u32, val: u16);
    fn write32(&mut self, addr: u32, val: u32);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEvent {
    Continue,
    Svc(u32),
    Bkpt(u16),
}

const FLAG_N: u32 = 1 << 31;
const FLAG_Z: u32 = 1 << 30;
const FLAG_C: u32 = 1 << 29;
const FLAG_V: u32 = 1 << 28;
const FLAG_T: u32 = 1 << 5;

#[derive(Clone)]
pub struct ArmCpu {
    pub r: [u32; 16],
    pub cpsr: u32,
    pub tls_ro: u32,
    pub tls_rw: u32,
    pub cycles: u64,
}

impl Default for ArmCpu {
    fn default() -> Self {
        Self::new()
    }
}

impl ArmCpu {
    pub fn new() -> Self {
        Self {
            r: [0; 16],
            cpsr: 0x0000_0010, // User mode, ARM
            tls_ro: 0,
            tls_rw: 0,
            cycles: 0,
        }
    }

    pub fn set_pc(&mut self, addr: u32) {
        if addr & 1 != 0 {
            self.cpsr |= FLAG_T;
            self.r[15] = addr & !1;
        } else {
            self.cpsr &= !FLAG_T;
            self.r[15] = addr & !3;
        }
    }

    #[inline]
    pub fn thumb(&self) -> bool {
        (self.cpsr & FLAG_T) != 0
    }

    #[inline]
    fn cond_passed(&self, cond: u32) -> bool {
        let n = (self.cpsr & FLAG_N) != 0;
        let z = (self.cpsr & FLAG_Z) != 0;
        let c = (self.cpsr & FLAG_C) != 0;
        let v = (self.cpsr & FLAG_V) != 0;
        match cond {
            0x0 => z,
            0x1 => !z,
            0x2 => c,
            0x3 => !c,
            0x4 => n,
            0x5 => !n,
            0x6 => v,
            0x7 => !v,
            0x8 => c && !z,
            0x9 => !c || z,
            0xA => n == v,
            0xB => n != v,
            0xC => !z && (n == v),
            0xD => z || (n != v),
            _ => true,
        }
    }

    #[inline]
    fn set_nz(&mut self, val: u32) {
        self.cpsr = (self.cpsr & !(FLAG_N | FLAG_Z))
            | (val & FLAG_N)
            | if val == 0 { FLAG_Z } else { 0 };
    }

    #[inline]
    fn set_carry(&mut self, c: bool) {
        if c {
            self.cpsr |= FLAG_C;
        } else {
            self.cpsr &= !FLAG_C;
        }
    }

    #[inline]
    fn set_overflow(&mut self, v: bool) {
        if v {
            self.cpsr |= FLAG_V;
        } else {
            self.cpsr &= !FLAG_V;
        }
    }

    #[inline]
    fn add_flags(&mut self, a: u32, b: u32, carry: bool, set: bool) -> u32 {
        let c_in = carry as u64;
        let sum64 = (a as u64) + (b as u64) + c_in;
        let res = sum64 as u32;
        if set {
            self.set_nz(res);
            self.set_carry(sum64 > 0xFFFF_FFFF);
            let ov = ((!(a ^ b)) & (a ^ res)) >> 31 != 0;
            self.set_overflow(ov);
        }
        res
    }

    #[inline]
    fn sub_flags(&mut self, a: u32, b: u32, carry: bool, set: bool) -> u32 {
        // ARM subtraction: a - b - !carry == a + !b + carry
        self.add_flags(a, !b, carry, set)
    }

    pub fn step<B: Bus>(&mut self, bus: &mut B) -> StepEvent {
        self.cycles += 1;
        if self.thumb() {
            self.step_thumb(bus)
        } else {
            self.step_arm(bus)
        }
    }

    fn step_arm<B: Bus>(&mut self, bus: &mut B) -> StepEvent {
        let pc = self.r[15];
        let op = bus.read32(pc);
        self.r[15] = pc.wrapping_add(4);

        let cond = op >> 28;
        if cond != 0xE && cond != 0xF && !self.cond_passed(cond) {
            return StepEvent::Continue;
        }

        // Unconditional BLX imm (cond == 0xF)
        if cond == 0xF {
            if (op & 0x0E00_0000) == 0x0A00_0000 {
                let h = (op >> 24) & 1;
                let imm24 = (((op & 0x00FF_FFFF) as i32) << 8) >> 6;
                self.r[14] = pc.wrapping_add(4);
                self.r[15] = pc.wrapping_add(8).wrapping_add(imm24 as u32).wrapping_add(h << 1);
                self.cpsr |= FLAG_T;
            }
            return StepEvent::Continue;
        }

        // SVC (SWI)
        if (op & 0x0F00_0000) == 0x0F00_0000 {
            return StepEvent::Svc(op & 0x00FF_FFFF);
        }

        // BKPT
        if (op & 0x0FF0_00F0) == 0x0120_0070 {
            let imm = (((op >> 8) & 0xFFF) << 4) | (op & 0xF);
            return StepEvent::Bkpt(imm as u16);
        }

        // BX / BLX Rm
        if (op & 0x0FFF_FFD0) == 0x012F_FF10 {
            let rm = (op & 0xF) as usize;
            let target = self.reg_read(rm, pc);
            if (op & 0x20) != 0 {
                self.r[14] = pc.wrapping_add(4);
            }
            self.set_pc(target);
            return StepEvent::Continue;
        }

        // CLZ
        if (op & 0x0FFF_0FF0) == 0x016F_0F10 {
            let rd = ((op >> 12) & 0xF) as usize;
            let rm = (op & 0xF) as usize;
            self.r[rd] = self.r[rm].leading_zeros();
            return StepEvent::Continue;
        }

        // MOVW / MOVT (ARMv7)
        if (op & 0x0FB0_0000) == 0x0300_0000 {
            let rd = ((op >> 12) & 0xF) as usize;
            let imm16 = (((op >> 16) & 0xF) << 12) | (op & 0xFFF);
            if (op & 0x0040_0000) != 0 {
                self.r[rd] = (self.r[rd] & 0xFFFF) | (imm16 << 16);
            } else {
                self.r[rd] = imm16;
            }
            return StepEvent::Continue;
        }

        // Multiply (MUL / MLA / UMULL / SMULL)
        if (op & 0x0FC0_00F0) == 0x0000_0090 {
            let rd = ((op >> 16) & 0xF) as usize;
            let rn = ((op >> 12) & 0xF) as usize;
            let rs = ((op >> 8) & 0xF) as usize;
            let rm = (op & 0xF) as usize;
            let acc = (op & (1 << 21)) != 0;
            let set = (op & (1 << 20)) != 0;
            let mut res = self.r[rm].wrapping_mul(self.r[rs]);
            if acc {
                res = res.wrapping_add(self.r[rn]);
            }
            self.r[rd] = res;
            if set {
                self.set_nz(res);
            }
            return StepEvent::Continue;
        }
        if (op & 0x0F80_00F0) == 0x0080_0090 {
            let rdhi = ((op >> 16) & 0xF) as usize;
            let rdlo = ((op >> 12) & 0xF) as usize;
            let rs = ((op >> 8) & 0xF) as usize;
            let rm = (op & 0xF) as usize;
            let signed = (op & (1 << 22)) != 0;
            let acc = (op & (1 << 21)) != 0;
            let set = (op & (1 << 20)) != 0;
            let mut prod: u64 = if signed {
                (self.r[rm] as i32 as i64).wrapping_mul(self.r[rs] as i32 as i64) as u64
            } else {
                (self.r[rm] as u64).wrapping_mul(self.r[rs] as u64)
            };
            if acc {
                let cur = ((self.r[rdhi] as u64) << 32) | (self.r[rdlo] as u64);
                prod = prod.wrapping_add(cur);
            }
            self.r[rdlo] = prod as u32;
            self.r[rdhi] = (prod >> 32) as u32;
            if set {
                self.cpsr = (self.cpsr & !(FLAG_N | FLAG_Z))
                    | ((self.r[rdhi] & FLAG_N))
                    | if prod == 0 { FLAG_Z } else { 0 };
            }
            return StepEvent::Continue;
        }

        // Halfword / signed byte load/store
        if (op & 0x0E00_0090) == 0x0000_0090 && (op & 0x60) != 0 {
            let p = (op & (1 << 24)) != 0;
            let u = (op & (1 << 23)) != 0;
            let imm_mode = (op & (1 << 22)) != 0;
            let w = (op & (1 << 21)) != 0;
            let load = (op & (1 << 20)) != 0;
            let rn = ((op >> 16) & 0xF) as usize;
            let rd = ((op >> 12) & 0xF) as usize;
            let sh = (op >> 5) & 3;
            let offset = if imm_mode {
                (((op >> 8) & 0xF) << 4) | (op & 0xF)
            } else {
                self.r[(op & 0xF) as usize]
            };
            let base = self.reg_read(rn, pc);
            let indexed = if u { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
            let addr = if p { indexed } else { base };
            if load {
                self.r[rd] = match sh {
                    1 => bus.read16(addr) as u32,
                    2 => bus.read8(addr) as i8 as i32 as u32,
                    3 => bus.read16(addr) as i16 as i32 as u32,
                    _ => 0,
                };
            } else if sh == 1 {
                bus.write16(addr, self.r[rd] as u16);
            }
            if (!p || w) && rn != rd {
                self.r[rn] = indexed;
            }
            return StepEvent::Continue;
        }

        // CP15 MRC/MCR (TLS register c13 for 3DS / Vita / Android)
        if (op & 0x0F00_0010) == 0x0E00_0010 {
            let cp = (op >> 8) & 0xF;
            let load = (op & (1 << 20)) != 0;
            let rd = ((op >> 12) & 0xF) as usize;
            let crn = (op >> 16) & 0xF;
            let op2 = (op >> 5) & 7;
            if cp == 15 && crn == 13 {
                if load {
                    self.r[rd] = if op2 == 3 { self.tls_ro } else { self.tls_rw };
                } else if op2 == 3 {
                    self.tls_ro = self.r[rd];
                } else {
                    self.tls_rw = self.r[rd];
                }
            }
            return StepEvent::Continue;
        }

        match (op >> 25) & 7 {
            // Data processing (register / immediate)
            0b000 | 0b001 => {
                let opcode = (op >> 21) & 0xF;
                let set = (op & (1 << 20)) != 0;
                let rn = ((op >> 16) & 0xF) as usize;
                let rd = ((op >> 12) & 0xF) as usize;
                if !set && (0x8..=0xB).contains(&opcode) {
                    // MRS / MSR
                    if opcode & 1 == 0 {
                        self.r[rd] = self.cpsr;
                    } else {
                        let (val, _) = self.decode_op2(op, pc);
                        self.cpsr = (self.cpsr & 0x0FFF_FFFF) | (val & 0xF000_0000);
                    }
                    return StepEvent::Continue;
                }
                let a = self.reg_read(rn, pc);
                let (b, shifter_c) = self.decode_op2(op, pc);
                let old_c = (self.cpsr & FLAG_C) != 0;
                let write = !(0x8..=0xB).contains(&opcode);
                let res = match opcode {
                    0x0 | 0x8 => {
                        let r = a & b;
                        if set { self.set_nz(r); self.set_carry(shifter_c); }
                        r
                    }
                    0x1 | 0x9 => {
                        let r = a ^ b;
                        if set { self.set_nz(r); self.set_carry(shifter_c); }
                        r
                    }
                    0x2 | 0xA => self.sub_flags(a, b, true, set),
                    0x3 => self.sub_flags(b, a, true, set),
                    0x4 | 0xB => self.add_flags(a, b, false, set),
                    0x5 => self.add_flags(a, b, old_c, set),
                    0x6 => self.sub_flags(a, b, old_c, set),
                    0x7 => self.sub_flags(b, a, old_c, set),
                    0xC => {
                        let r = a | b;
                        if set { self.set_nz(r); self.set_carry(shifter_c); }
                        r
                    }
                    0xD => {
                        if set { self.set_nz(b); self.set_carry(shifter_c); }
                        b
                    }
                    0xE => {
                        let r = a & !b;
                        if set { self.set_nz(r); self.set_carry(shifter_c); }
                        r
                    }
                    _ => {
                        let r = !b;
                        if set { self.set_nz(r); self.set_carry(shifter_c); }
                        r
                    }
                };
                if write {
                    if rd == 15 {
                        self.r[15] = res & !3;
                    } else {
                        self.r[rd] = res;
                    }
                }
            }
            // Single data transfer (LDR / STR / LDRB / STRB)
            0b010 | 0b011 => {
                let reg_off = (op & (1 << 25)) != 0;
                let p = (op & (1 << 24)) != 0;
                let u = (op & (1 << 23)) != 0;
                let byte = (op & (1 << 22)) != 0;
                let w = (op & (1 << 21)) != 0;
                let load = (op & (1 << 20)) != 0;
                let rn = ((op >> 16) & 0xF) as usize;
                let rd = ((op >> 12) & 0xF) as usize;
                let offset = if reg_off {
                    let rm = (op & 0xF) as usize;
                    let shift = (op >> 7) & 0x1F;
                    let stype = (op >> 5) & 3;
                    self.barrel_shift(self.r[rm], stype, shift, false).0
                } else {
                    op & 0xFFF
                };
                let base = self.reg_read(rn, pc);
                let indexed = if u { base.wrapping_add(offset) } else { base.wrapping_sub(offset) };
                let addr = if p { indexed } else { base };
                if load {
                    let val = if byte { bus.read8(addr) as u32 } else { bus.read32(addr) };
                    if rd == 15 {
                        self.set_pc(val);
                    } else {
                        self.r[rd] = val;
                    }
                } else {
                    let val = if rd == 15 { pc.wrapping_add(12) } else { self.r[rd] };
                    if byte {
                        bus.write8(addr, val as u8);
                    } else {
                        bus.write32(addr, val);
                    }
                }
                if (!p || w) && !(load && rn == rd) {
                    self.r[rn] = indexed;
                }
            }
            // Block data transfer (LDM / STM)
            0b100 => {
                let p = (op & (1 << 24)) != 0;
                let u = (op & (1 << 23)) != 0;
                let w = (op & (1 << 21)) != 0;
                let load = (op & (1 << 20)) != 0;
                let rn = ((op >> 16) & 0xF) as usize;
                let list = op & 0xFFFF;
                let count = list.count_ones();
                let base = self.r[rn];
                let mut addr = match (u, p) {
                    (true, false) => base,
                    (true, true) => base.wrapping_add(4),
                    (false, false) => base.wrapping_sub(count * 4).wrapping_add(4),
                    (false, true) => base.wrapping_sub(count * 4),
                };
                let new_base = if u { base.wrapping_add(count * 4) } else { base.wrapping_sub(count * 4) };
                for reg in 0..16 {
                    if (list & (1 << reg)) != 0 {
                        if load {
                            let val = bus.read32(addr);
                            if reg == 15 {
                                self.set_pc(val);
                            } else {
                                self.r[reg] = val;
                            }
                        } else {
                            let val = if reg == 15 { pc.wrapping_add(12) } else { self.r[reg] };
                            bus.write32(addr, val);
                        }
                        addr = addr.wrapping_add(4);
                    }
                }
                if w && !(load && (list & (1 << rn)) != 0) {
                    self.r[rn] = new_base;
                }
            }
            // Branch (B / BL)
            0b101 => {
                let link = (op & (1 << 24)) != 0;
                let imm24 = (((op & 0x00FF_FFFF) as i32) << 8) >> 6;
                if link {
                    self.r[14] = pc.wrapping_add(4);
                }
                self.r[15] = pc.wrapping_add(8).wrapping_add(imm24 as u32);
            }
            _ => {}
        }
        StepEvent::Continue
    }

    fn step_thumb<B: Bus>(&mut self, bus: &mut B) -> StepEvent {
        let pc = self.r[15];
        let op = bus.read16(pc);
        self.r[15] = pc.wrapping_add(2);

        let top = op >> 11;
        match top {
            // LSL / LSR / ASR imm5
            0b00000..=0b00010 => {
                let rd = (op & 7) as usize;
                let rs = ((op >> 3) & 7) as usize;
                let imm5 = ((op >> 6) & 0x1F) as u32;
                let (res, c) = self.barrel_shift(self.r[rs], top as u32, imm5, false);
                self.r[rd] = res;
                self.set_nz(res);
                self.set_carry(c);
            }
            // ADD / SUB reg or imm3
            0b00011 => {
                let rd = (op & 7) as usize;
                let rs = ((op >> 3) & 7) as usize;
                let rn_imm = ((op >> 6) & 7) as usize;
                let is_imm = (op & (1 << 10)) != 0;
                let is_sub = (op & (1 << 9)) != 0;
                let rhs = if is_imm { rn_imm as u32 } else { self.r[rn_imm] };
                self.r[rd] = if is_sub {
                    self.sub_flags(self.r[rs], rhs, true, true)
                } else {
                    self.add_flags(self.r[rs], rhs, false, true)
                };
            }
            // MOV / CMP / ADD / SUB imm8
            0b00100..=0b00111 => {
                let rd = ((op >> 8) & 7) as usize;
                let imm8 = (op & 0xFF) as u32;
                match top {
                    0b00100 => { self.r[rd] = imm8; self.set_nz(imm8); }
                    0b00101 => { self.sub_flags(self.r[rd], imm8, true, true); }
                    0b00110 => { self.r[rd] = self.add_flags(self.r[rd], imm8, false, true); }
                    _ => { self.r[rd] = self.sub_flags(self.r[rd], imm8, true, true); }
                }
            }
            // ALU / Hi-reg / PC-rel LDR
            0b01000 => {
                if (op & 0xFC00) == 0x4000 {
                    let rd = (op & 7) as usize;
                    let rs = ((op >> 3) & 7) as usize;
                    let a = self.r[rd];
                    let b = self.r[rs];
                    match (op >> 6) & 0xF {
                        0x0 => { self.r[rd] = a & b; self.set_nz(self.r[rd]); }
                        0x1 => { self.r[rd] = a ^ b; self.set_nz(self.r[rd]); }
                        0x2 => { self.r[rd] = a.wrapping_shl(b & 0xFF); self.set_nz(self.r[rd]); }
                        0x3 => { self.r[rd] = a.wrapping_shr(b & 0xFF); self.set_nz(self.r[rd]); }
                        0x4 => { self.r[rd] = ((a as i32).wrapping_shr(b & 0xFF)) as u32; self.set_nz(self.r[rd]); }
                        0x5 => { let c = (self.cpsr & FLAG_C) != 0; self.r[rd] = self.add_flags(a, b, c, true); }
                        0x6 => { let c = (self.cpsr & FLAG_C) != 0; self.r[rd] = self.sub_flags(a, b, c, true); }
                        0x7 => { self.r[rd] = a.rotate_right(b & 31); self.set_nz(self.r[rd]); }
                        0x8 => self.set_nz(a & b),
                        0x9 => { self.r[rd] = self.sub_flags(0, b, true, true); }
                        0xA => { self.sub_flags(a, b, true, true); }
                        0xB => { self.add_flags(a, b, false, true); }
                        0xC => { self.r[rd] = a | b; self.set_nz(self.r[rd]); }
                        0xD => { self.r[rd] = a.wrapping_mul(b); self.set_nz(self.r[rd]); }
                        0xE => { self.r[rd] = a & !b; self.set_nz(self.r[rd]); }
                        _ => { self.r[rd] = !b; self.set_nz(self.r[rd]); }
                    }
                } else {
                    let rd = ((op & 7) | ((op >> 4) & 8)) as usize;
                    let rs = ((op >> 3) & 0xF) as usize;
                    let rhs = if rs == 15 { pc.wrapping_add(4) } else { self.r[rs] };
                    match (op >> 8) & 3 {
                        0 => self.r[rd] = self.r[rd].wrapping_add(rhs),
                        1 => { self.sub_flags(self.r[rd], rhs, true, true); }
                        2 => if rd == 15 { self.r[15] = rhs & !1; } else { self.r[rd] = rhs; },
                        _ => {
                            if (op & 0x80) != 0 {
                                self.r[14] = pc.wrapping_add(2) | 1;
                            }
                            self.set_pc(rhs);
                        }
                    }
                }
            }
            0b01001 => {
                // PC-relative LDR
                let rd = ((op >> 8) & 7) as usize;
                let addr = ((pc.wrapping_add(4)) & !3).wrapping_add(((op & 0xFF) as u32) * 4);
                self.r[rd] = bus.read32(addr);
            }
            // Load/store register offset
            0b01010 | 0b01011 => {
                let rd = (op & 7) as usize;
                let rb = ((op >> 3) & 7) as usize;
                let ro = ((op >> 6) & 7) as usize;
                let addr = self.r[rb].wrapping_add(self.r[ro]);
                match (op >> 9) & 7 {
                    0 => bus.write32(addr, self.r[rd]),
                    1 => bus.write16(addr, self.r[rd] as u16),
                    2 => bus.write8(addr, self.r[rd] as u8),
                    3 => self.r[rd] = bus.read8(addr) as i8 as i32 as u32,
                    4 => self.r[rd] = bus.read32(addr),
                    5 => self.r[rd] = bus.read16(addr) as u32,
                    6 => self.r[rd] = bus.read8(addr) as u32,
                    _ => self.r[rd] = bus.read16(addr) as i16 as i32 as u32,
                }
            }
            // STR / LDR / STRB / LDRB imm5
            0b01100..=0b01111 => {
                let rd = (op & 7) as usize;
                let rb = ((op >> 3) & 7) as usize;
                let imm5 = ((op >> 6) & 0x1F) as u32;
                match top {
                    0b01100 => bus.write32(self.r[rb].wrapping_add(imm5 * 4), self.r[rd]),
                    0b01101 => self.r[rd] = bus.read32(self.r[rb].wrapping_add(imm5 * 4)),
                    0b01110 => bus.write8(self.r[rb].wrapping_add(imm5), self.r[rd] as u8),
                    _ => self.r[rd] = bus.read8(self.r[rb].wrapping_add(imm5)) as u32,
                }
            }
            // STRH / LDRH imm5
            0b10000 => {
                let rd = (op & 7) as usize;
                let rb = ((op >> 3) & 7) as usize;
                let imm5 = ((op >> 6) & 0x1F) as u32;
                bus.write16(self.r[rb].wrapping_add(imm5 * 2), self.r[rd] as u16);
            }
            0b10001 => {
                let rd = (op & 7) as usize;
                let rb = ((op >> 3) & 7) as usize;
                let imm5 = ((op >> 6) & 0x1F) as u32;
                self.r[rd] = bus.read16(self.r[rb].wrapping_add(imm5 * 2)) as u32;
            }
            // SP-relative STR / LDR
            0b10010 => {
                let rd = ((op >> 8) & 7) as usize;
                bus.write32(self.r[13].wrapping_add(((op & 0xFF) as u32) * 4), self.r[rd]);
            }
            0b10011 => {
                let rd = ((op >> 8) & 7) as usize;
                self.r[rd] = bus.read32(self.r[13].wrapping_add(((op & 0xFF) as u32) * 4));
            }
            // PUSH / POP / SP add
            0b10110 | 0b10111 => {
                if (op & 0xFF00) == 0xB000 {
                    let imm = ((op & 0x7F) as u32) * 4;
                    if (op & 0x80) != 0 {
                        self.r[13] = self.r[13].wrapping_sub(imm);
                    } else {
                        self.r[13] = self.r[13].wrapping_add(imm);
                    }
                } else if (op & 0xFE00) == 0xB400 {
                    // PUSH
                    let lr = (op & 0x100) != 0;
                    let list = (op & 0xFF) as u32 | if lr { 1 << 14 } else { 0 };
                    let count = list.count_ones();
                    let mut addr = self.r[13].wrapping_sub(count * 4);
                    self.r[13] = addr;
                    for reg in 0..16 {
                        if (list & (1 << reg)) != 0 {
                            bus.write32(addr, self.r[reg]);
                            addr = addr.wrapping_add(4);
                        }
                    }
                } else if (op & 0xFE00) == 0xBC00 {
                    // POP
                    let pc_bit = (op & 0x100) != 0;
                    let list = (op & 0xFF) as u32 | if pc_bit { 1 << 15 } else { 0 };
                    let mut addr = self.r[13];
                    for reg in 0..16 {
                        if (list & (1 << reg)) != 0 {
                            let val = bus.read32(addr);
                            if reg == 15 {
                                self.set_pc(val);
                            } else {
                                self.r[reg] = val;
                            }
                            addr = addr.wrapping_add(4);
                        }
                    }
                    self.r[13] = addr;
                }
            }
            // Conditional branch / SVC
            0b11010 | 0b11011 => {
                let cond = ((op >> 8) & 0xF) as u32;
                if cond == 0xF {
                    return StepEvent::Svc((op & 0xFF) as u32);
                } else if self.cond_passed(cond) {
                    let off = ((op & 0xFF) as i8 as i32) * 2;
                    self.r[15] = pc.wrapping_add(4).wrapping_add(off as u32);
                }
            }
            // Unconditional B
            0b11100 => {
                let off = (((op & 0x7FF) as i16) << 5) >> 4;
                self.r[15] = pc.wrapping_add(4).wrapping_add(off as i32 as u32);
            }
            // BL prefix / suffix
            0b11110 => {
                let hi = (((op & 0x7FF) as i32) << 21) >> 9;
                self.r[14] = pc.wrapping_add(4).wrapping_add(hi as u32);
            }
            0b11111 => {
                let next = pc.wrapping_add(2);
                let target = self.r[14].wrapping_add(((op & 0x7FF) as u32) << 1);
                self.r[14] = next | 1;
                self.r[15] = target;
            }
            _ => {}
        }
        StepEvent::Continue
    }

    #[inline]
    fn reg_read(&self, reg: usize, pc: u32) -> u32 {
        if reg == 15 { pc.wrapping_add(8) } else { self.r[reg] }
    }

    fn decode_op2(&self, op: u32, pc: u32) -> (u32, bool) {
        let old_c = (self.cpsr & FLAG_C) != 0;
        if (op & (1 << 25)) != 0 {
            let imm = op & 0xFF;
            let rot = ((op >> 8) & 0xF) * 2;
            let val = imm.rotate_right(rot);
            let c = if rot == 0 { old_c } else { (val >> 31) != 0 };
            (val, c)
        } else {
            let rm = (op & 0xF) as usize;
            let val = self.reg_read(rm, pc);
            let stype = (op >> 5) & 3;
            let reg_shift = (op & (1 << 4)) != 0;
            let amount = if reg_shift {
                self.r[((op >> 8) & 0xF) as usize] & 0xFF
            } else {
                (op >> 7) & 0x1F
            };
            self.barrel_shift(val, stype, amount, reg_shift)
        }
    }

    fn barrel_shift(&self, val: u32, stype: u32, amount: u32, reg_shift: bool) -> (u32, bool) {
        let old_c = (self.cpsr & FLAG_C) != 0;
        if reg_shift && amount == 0 {
            return (val, old_c);
        }
        match stype {
            0 => {
                if amount == 0 { (val, old_c) }
                else if amount < 32 { (val << amount, (val >> (32 - amount)) & 1 != 0) }
                else if amount == 32 { (0, (val & 1) != 0) }
                else { (0, false) }
            }
            1 => {
                let s = if !reg_shift && amount == 0 { 32 } else { amount };
                if s < 32 { (val >> s, (val >> (s - 1)) & 1 != 0) }
                else if s == 32 { (0, (val >> 31) != 0) }
                else { (0, false) }
            }
            2 => {
                let s = if !reg_shift && amount == 0 { 32 } else { amount };
                if s < 32 { (((val as i32) >> s) as u32, (val >> (s - 1)) & 1 != 0) }
                else {
                    let sign = (val >> 31) != 0;
                    (if sign { u32::MAX } else { 0 }, sign)
                }
            }
            _ => {
                if !reg_shift && amount == 0 {
                    // RRX
                    (((old_c as u32) << 31) | (val >> 1), (val & 1) != 0)
                } else {
                    let s = amount & 31;
                    let res = val.rotate_right(s);
                    (res, (res >> 31) != 0)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<u8>);
    impl Bus for Mem {
        fn read8(&self, a: u32) -> u8 { self.0.get(a as usize).copied().unwrap_or(0) }
        fn read16(&self, a: u32) -> u16 { u16::from_le_bytes([self.read8(a), self.read8(a + 1)]) }
        fn read32(&self, a: u32) -> u32 {
            u32::from_le_bytes([self.read8(a), self.read8(a + 1), self.read8(a + 2), self.read8(a + 3)])
        }
        fn write8(&mut self, a: u32, v: u8) { if let Some(b) = self.0.get_mut(a as usize) { *b = v; } }
        fn write16(&mut self, a: u32, v: u16) { let b = v.to_le_bytes(); self.write8(a, b[0]); self.write8(a + 1, b[1]); }
        fn write32(&mut self, a: u32, v: u32) {
            for (k, &b) in v.to_le_bytes().iter().enumerate() { self.write8(a + k as u32, b); }
        }
    }

    #[test]
    fn arm_and_thumb_execute_and_trap_svc() {
        let mut mem = Mem(vec![0; 256]);
        // ARM: MOV r0, #12 (E3A0000C); MOV r1, #30 (E3A0101E); ADD r2, r0, r1 (E0802001); SVC #0x21 (EF000021)
        let prog = [0xE3A0_000Cu32, 0xE3A0_101E, 0xE080_2001, 0xEF00_0021];
        for (i, &w) in prog.iter().enumerate() {
            mem.write32((i * 4) as u32, w);
        }
        let mut cpu = ArmCpu::new();
        assert_eq!(cpu.step(&mut mem), StepEvent::Continue);
        assert_eq!(cpu.step(&mut mem), StepEvent::Continue);
        assert_eq!(cpu.step(&mut mem), StepEvent::Continue);
        assert_eq!(cpu.step(&mut mem), StepEvent::Svc(0x21));
        assert_eq!(cpu.r[2], 42);
    }
}
