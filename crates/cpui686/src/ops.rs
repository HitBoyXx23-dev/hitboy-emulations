//! One-byte opcodes.

use super::*;

impl Cpu {
    /// I/O instructions need CPL <= IOPL in protected mode (the TSS permission bitmap is
    /// not consulted).
    fn io_allowed(&self) -> R<()> {
        if self.protected() && (self.v86() || self.cpl as u32 > (self.eflags & IOPL) >> 12) {
            return Err(gp(0));
        }
        Ok(())
    }

    pub(crate) fn exec_op<B: Bus>(&mut self, bus: &mut B, op: u8) -> R<()> {
        let osz = self.osz;
        let op32 = op as u32;
        if self.code64 && matches!(op, 0x06 | 0x07 | 0x0E | 0x16 | 0x17 | 0x1E | 0x1F | 0x27 | 0x2F | 0x37 | 0x3F | 0x60 | 0x61 | 0x62 | 0x82 | 0x9A | 0xC4 | 0xC5 | 0xCE | 0xD4 | 0xD5 | 0xD6 | 0xEA) {
            return Err(UD);
        }
        let rex_b = (self.rex & 1) << 3;
        match op {
            // ALU r/m, reg forms
            0x00..=0x3F if op & 7 < 6 => {
                let alu = op32 >> 3;
                let sz = if op & 1 == 0 { 1 } else { osz };
                match op & 7 {
                    0..=3 => {
                        let (reg, rm) = self.modrm(bus)?;
                        let (dst, src) = if op & 2 == 0 { (rm, Op::Reg(reg)) } else { (Op::Reg(reg), rm) };
                        let (a, b) = (self.get(bus, dst, sz)?, self.get(bus, src, sz)?);
                        let r = self.alu(alu, a, b, sz);
                        if alu != 7 {
                            self.set(bus, dst, sz, r)?;
                        }
                    }
                    _ => {
                        let b = self.imm(bus, sz)?;
                        let a = self.reg(0, sz);
                        let r = self.alu(alu, a, b, sz);
                        if alu != 7 {
                            self.set_reg(0, sz, r);
                        }
                    }
                }
            }
            0x06 | 0x0E | 0x16 | 0x1E => {
                let v = self.sel[(op >> 3) as usize] as u64;
                self.push(bus, osz, v)?;
            }
            0x07 | 0x17 | 0x1F => {
                let v = self.pop(bus, osz)?;
                self.load_seg(bus, (op >> 3) as usize, v as u16)?;
                if op == 0x17 {
                    self.shadow = true;
                }
            }
            0x0F => self.exec_0f(bus)?,
            0x27 | 0x2F => {
                // DAA / DAS
                let al = self.reg(0, 1);
                let (cf, af) = (self.cf() != 0, self.eflags & AF != 0);
                let mut v = al;
                let mut ncf = false;
                if al & 0xF > 9 || af {
                    v = if op == 0x27 { v.wrapping_add(6) } else { v.wrapping_sub(6) } & 0xFF;
                    self.eflags |= AF;
                } else {
                    self.eflags &= !AF;
                }
                if al > 0x99 || cf {
                    v = if op == 0x27 { v.wrapping_add(0x60) } else { v.wrapping_sub(0x60) } & 0xFF;
                    ncf = true;
                }
                self.setf(CF, ncf);
                self.set_reg(0, 1, v);
                self.szp(v, 1);
            }
            0x37 | 0x3F => {
                // AAA / AAS
                let ax = self.reg(0, 2);
                if ax & 0xF > 9 || self.eflags & AF != 0 {
                    let v = if op == 0x37 { ax.wrapping_add(0x106) } else { ax.wrapping_sub(0x106) };
                    self.set_reg(0, 2, v & 0xFF0F);
                    self.eflags |= AF | CF;
                } else {
                    self.set_reg(0, 2, ax & 0xFF0F);
                    self.eflags &= !(AF | CF);
                }
            }
            0x40..=0x4F => {
                // INC / DEC r (32-bit modes only; these bytes are REX in 64-bit mode)
                let i = op & 7;
                let a = self.reg(i, osz);
                let c = self.cf();
                let r = self.alu(if op < 0x48 { 0 } else { 5 }, a, 1, osz);
                self.setf(CF, c != 0);
                self.set_reg(i, osz, r);
            }
            0x50..=0x57 => {
                let sz = self.stack_osz();
                let v = self.reg((op & 7) | rex_b, sz);
                self.push(bus, sz, v)?;
            }
            0x58..=0x5F => {
                let sz = self.stack_osz();
                let v = self.pop(bus, sz)?;
                self.set_reg((op & 7) | rex_b, sz, v);
            }
            0x60 => {
                let sp = self.reg(4, osz);
                for i in 0..8u8 {
                    let v = if i == 4 { sp } else { self.reg(i, osz) };
                    self.push(bus, osz, v)?;
                }
            }
            0x61 => {
                for i in (0..8u8).rev() {
                    let v = self.pop(bus, osz)?;
                    if i != 4 {
                        self.set_reg(i, osz, v);
                    }
                }
            }
            0x62 => {
                // BOUND
                let (reg, rm) = self.modrm(bus)?;
                let Op::Mem(s, a) = rm else { return Err(UD) };
                let v = sx(self.reg(reg, osz), osz) as i64;
                let lo = sx(self.rd(bus, s, a, osz)?, osz) as i64;
                let hi = sx(self.rd(bus, s, a.wrapping_add(osz as u64), osz)?, osz) as i64;
                if v < lo || v > hi {
                    return Err(Ex(5, None));
                }
            }
            0x63 => {
                let (reg, rm) = self.modrm(bus)?;
                if self.code64 {
                    // MOVSXD
                    let v = self.get(bus, rm, 4)?;
                    self.set_reg(reg, osz, sx(v, 4));
                } else if !self.protected() || self.v86() {
                    return Err(UD);
                } else {
                    // ARPL
                    let (d, s) = (self.get(bus, rm, 2)?, self.reg(reg, 2));
                    if d & 3 < s & 3 {
                        self.set(bus, rm, 2, (d & !3) | (s & 3))?;
                        self.eflags |= ZF;
                    } else {
                        self.eflags &= !ZF;
                    }
                }
            }
            0x68 => {
                let sz = self.stack_osz();
                let v = self.imm(bus, sz)?;
                self.push(bus, sz, v)?;
            }
            0x6A => {
                let sz = self.stack_osz();
                let v = sx(self.fetch8(bus)?, 1);
                self.push(bus, sz, v)?;
            }
            0x69 | 0x6B => {
                let (reg, rm) = self.modrm(bus)?;
                let b = if op == 0x69 { self.imm(bus, osz)? } else { sx(self.fetch8(bus)?, 1) };
                let a = self.get(bus, rm, osz)?;
                let r = self.imul(a, b, osz);
                self.set_reg(reg, osz, r);
            }
            0x6C..=0x6F => {
                self.io_allowed()?;
                self.string(bus, op32)?
            }
            0x70..=0x7F => {
                let d = sx(self.fetch8(bus)?, 1);
                if self.cond(op32 - 0x70) {
                    self.jump_rel(d);
                }
            }
            0x80..=0x83 => {
                let sz = if op == 0x80 || op == 0x82 { 1 } else { osz };
                let (reg, rm) = self.modrm(bus)?;
                let b = if op == 0x83 { sx(self.fetch8(bus)?, 1) } else { self.imm(bus, sz)? };
                let a = self.get(bus, rm, sz)?;
                let r = self.alu(reg as u32 & 7, a, b, sz);
                if reg & 7 != 7 {
                    self.set(bus, rm, sz, r)?;
                }
            }
            0x84 | 0x85 => {
                let sz = if op == 0x84 { 1 } else { osz };
                let (reg, rm) = self.modrm(bus)?;
                let a = self.get(bus, rm, sz)?;
                let b = self.reg(reg, sz);
                self.alu(4, a, b, sz);
            }
            0x86 | 0x87 => {
                let sz = if op == 0x86 { 1 } else { osz };
                let (reg, rm) = self.modrm(bus)?;
                let a = self.get(bus, rm, sz)?;
                let b = self.reg(reg, sz);
                self.set(bus, rm, sz, b)?;
                self.set_reg(reg, sz, a);
            }
            0x88..=0x8B => {
                let sz = if op & 1 == 0 { 1 } else { osz };
                let (reg, rm) = self.modrm(bus)?;
                if op & 2 == 0 {
                    let v = self.reg(reg, sz);
                    self.set(bus, rm, sz, v)?;
                } else {
                    let v = self.get(bus, rm, sz)?;
                    self.set_reg(reg, sz, v);
                }
            }
            0x8C => {
                let (reg, rm) = self.modrm(bus)?;
                let v = *self.sel.get((reg & 7) as usize).ok_or(UD)? as u64;
                let sz = if matches!(rm, Op::Reg(_)) { osz } else { 2 };
                self.set(bus, rm, sz, v)?;
            }
            0x8D => {
                let (reg, rm) = self.modrm(bus)?;
                let Op::Mem(_, a) = rm else { return Err(UD) };
                self.set_reg(reg, osz, a);
            }
            0x8E => {
                let (reg, rm) = self.modrm(bus)?;
                let s = (reg & 7) as usize;
                if s == CS || s > 5 {
                    return Err(UD);
                }
                let v = self.get(bus, rm, 2)?;
                self.load_seg(bus, s, v as u16)?;
                if s == SS {
                    self.shadow = true;
                }
            }
            0x8F => {
                let sz = self.stack_osz();
                let v = self.pop(bus, sz)?;
                let (_, rm) = self.modrm(bus)?;
                self.set(bus, rm, sz, v)?;
            }
            0x90 if rex_b == 0 => {} // NOP / PAUSE
            0x90..=0x97 => {
                let i = (op & 7) | rex_b;
                let (a, b) = (self.reg(0, osz), self.reg(i, osz));
                self.set_reg(0, osz, b);
                self.set_reg(i, osz, a);
            }
            0x98 => {
                // CBW / CWDE / CDQE
                let half = osz / 2;
                let v = sx(self.reg(0, half), half);
                self.set_reg(0, osz, v);
            }
            0x99 => {
                // CWD / CDQ / CQO
                let neg = self.reg(0, osz) & sign(osz) != 0;
                self.set_reg(EDX as u8, osz, if neg { u64::MAX } else { 0 });
            }
            0x9A => {
                let off = self.imm(bus, osz)?;
                let sel = self.fetch(bus, 2)?;
                self.far_transfer(bus, sel as u16, off, true)?;
            }
            0x9B => {}
            0x9C => {
                if self.v86() && self.eflags & IOPL != IOPL {
                    return Err(gp(0));
                }
                let sz = self.stack_osz();
                let f = (self.eflags & 0x00FC_FFFF) as u64;
                self.push(bus, sz, f)?;
            }
            0x9D => {
                if self.v86() && self.eflags & IOPL != IOPL {
                    return Err(gp(0));
                }
                let sz = self.stack_osz();
                let v = self.pop(bus, sz)?;
                self.set_flags(v as u32, sz.min(4));
            }
            0x9E => {
                let ah = self.reg(4, 1) as u32;
                self.eflags = (self.eflags & !0xD5) | (ah & 0xD5) | 2;
            }
            0x9F => {
                let f = (self.eflags & 0xFF) as u64;
                self.r[0] = (self.r[0] & !0xFF00) | f << 8;
            }
            0xA0..=0xA3 => {
                let sz = if op & 1 == 0 { 1 } else { osz };
                let off = self.fetch(bus, self.asz)?;
                let s = self.ds(DS);
                if op < 0xA2 {
                    let v = self.rd(bus, s, off, sz)?;
                    self.set_reg(0, sz, v);
                } else {
                    let v = self.reg(0, sz);
                    self.wr(bus, s, off, sz, v)?;
                }
            }
            0xA4..=0xA7 | 0xAA..=0xAF => self.string(bus, op32)?,
            0xA8 | 0xA9 => {
                let sz = if op == 0xA8 { 1 } else { osz };
                let b = self.imm(bus, sz)?;
                let a = self.reg(0, sz);
                self.alu(4, a, b, sz);
            }
            0xB0..=0xB7 => {
                let v = self.fetch8(bus)?;
                self.set_reg((op & 7) | rex_b, 1, v);
            }
            0xB8..=0xBF => {
                // MOV r, imm (a full 64-bit immediate with REX.W)
                let v = self.fetch(bus, osz)?;
                self.set_reg((op & 7) | rex_b, osz, v);
            }
            0xC0 | 0xC1 | 0xD0..=0xD3 => {
                let sz = if op & 1 == 0 { 1 } else { osz };
                let (reg, rm) = self.modrm(bus)?;
                let cnt = match op {
                    0xC0 | 0xC1 => self.fetch8(bus)?,
                    0xD0 | 0xD1 => 1,
                    _ => self.reg(ECX as u8, 1),
                };
                let v = self.get(bus, rm, sz)?;
                let r = self.shift(reg & 7, v, cnt, sz);
                self.set(bus, rm, sz, r)?;
            }
            0xC2 | 0xC3 => {
                let n = if op == 0xC2 { self.fetch(bus, 2)? } else { 0 };
                let sz = self.stack_osz();
                let ip = self.pop(bus, sz)?;
                self.rip = ip;
                self.set_sp(self.sp().wrapping_add(n));
            }
            0xC4 | 0xC5 => {
                let (reg, rm) = self.modrm(bus)?;
                let Op::Mem(s, a) = rm else { return Err(UD) };
                let off = self.rd(bus, s, a, osz)?;
                let sel = self.rd(bus, s, a.wrapping_add(osz as u64), 2)?;
                self.load_seg(bus, if op == 0xC4 { ES } else { DS }, sel as u16)?;
                self.set_reg(reg, osz, off);
            }
            0xC6 | 0xC7 => {
                let sz = if op == 0xC6 { 1 } else { osz };
                let (_, rm) = self.modrm(bus)?;
                let v = self.imm(bus, sz)?;
                self.set(bus, rm, sz, v)?;
            }
            0xC8 => {
                let size = self.fetch(bus, 2)?;
                let level = self.fetch8(bus)? & 31;
                let sz = self.stack_osz();
                let bp = self.reg(EBP as u8, sz);
                self.push(bus, sz, bp)?;
                let frame = self.sp();
                for i in 1..level {
                    let a = self.reg(EBP as u8, self.stack_size()).wrapping_sub(i * sz as u64);
                    let v = self.rd(bus, SS, a, sz)?;
                    self.push(bus, sz, v)?;
                }
                if level > 0 {
                    self.push(bus, sz, frame)?;
                }
                self.set_reg(EBP as u8, self.stack_size().min(sz), frame);
                self.set_sp(self.sp().wrapping_sub(size));
            }
            0xC9 => {
                let bp = self.reg(EBP as u8, self.stack_size());
                self.set_sp(bp);
                let sz = self.stack_osz();
                let v = self.pop(bus, sz)?;
                self.set_reg(EBP as u8, sz, v);
            }
            0xCA | 0xCB => {
                let n = if op == 0xCA { self.fetch(bus, 2)? } else { 0 };
                self.far_ret(bus, n)?;
            }
            0xCC => return self.soft_int(bus, 3),
            0xCD => {
                let v = self.fetch8(bus)?;
                if self.v86() && self.eflags & IOPL != IOPL {
                    return Err(gp(0));
                }
                return self.soft_int(bus, v as u8);
            }
            0xCE => {
                if self.eflags & OF != 0 {
                    return self.soft_int(bus, 4);
                }
            }
            0xCF => self.iret(bus, osz)?,
            0xD4 => {
                let b = self.fetch8(bus)?;
                if b == 0 {
                    return Err(DE);
                }
                let al = self.reg(0, 1);
                self.set_reg(0, 2, (al / b) << 8 | (al % b));
                self.szp(al % b, 1);
            }
            0xD5 => {
                let b = self.fetch8(bus)?;
                let v = (self.reg(0, 1) + self.reg(4, 1) * b) & 0xFF;
                self.set_reg(0, 2, v);
                self.szp(v, 1);
            }
            0xD6 => {
                let v = if self.cf() != 0 { 0xFF } else { 0 };
                self.set_reg(0, 1, v);
            }
            0xD7 => {
                let a = self.reg(EBX as u8, self.asz).wrapping_add(self.r[0] & 0xFF) & mask(self.asz);
                let s = self.ds(DS);
                let v = self.rd(bus, s, a, 1)?;
                self.set_reg(0, 1, v);
            }
            0xD8..=0xDF => self.fpu_exec(bus, op32)?,
            0xE0..=0xE3 => {
                let d = sx(self.fetch8(bus)?, 1);
                let csz = self.asz;
                let take = if op == 0xE3 {
                    self.reg(ECX as u8, csz) == 0
                } else {
                    let c = self.reg(ECX as u8, csz).wrapping_sub(1) & mask(csz);
                    self.set_reg(ECX as u8, csz, c);
                    let z = self.eflags & ZF != 0;
                    c != 0 && (op == 0xE2 || (op == 0xE1 && z) || (op == 0xE0 && !z))
                };
                if take {
                    self.jump_rel(d);
                }
            }
            0xE4 | 0xE5 | 0xEC | 0xED => {
                self.io_allowed()?;
                let sz = if op & 1 == 0 { 1 } else { osz.min(4) };
                let port = if op < 0xE8 { self.fetch8(bus)? } else { self.reg(EDX as u8, 2) };
                let v = bus.io_in(port as u16, sz as u8) as u64;
                self.set_reg(0, sz, v);
            }
            0xE6 | 0xE7 | 0xEE | 0xEF => {
                self.io_allowed()?;
                let sz = if op & 1 == 0 { 1 } else { osz.min(4) };
                let port = if op < 0xE8 { self.fetch8(bus)? } else { self.reg(EDX as u8, 2) };
                bus.io_out(port as u16, sz as u8, self.reg(0, sz) as u32);
            }
            0xE8 => {
                let d = sx(self.fetch(bus, osz.min(4))?, osz.min(4));
                let sz = self.stack_osz();
                let ip = self.rip;
                self.push(bus, sz, ip)?;
                self.jump_rel(d);
            }
            0xE9 => {
                let d = sx(self.fetch(bus, osz.min(4))?, osz.min(4));
                self.jump_rel(d);
            }
            0xEA => {
                let off = self.fetch(bus, osz)?;
                let sel = self.fetch(bus, 2)?;
                self.far_transfer(bus, sel as u16, off, false)?;
            }
            0xEB => {
                let d = sx(self.fetch8(bus)?, 1);
                self.jump_rel(d);
            }
            0xF1 => return self.interrupt(bus, 1, None), // INT1
            0xF4 => {
                if self.protected() && self.cpl != 0 {
                    return Err(gp(0));
                }
                self.halted = true;
            }
            0xF5 => self.eflags ^= CF,
            0xF6 | 0xF7 => {
                let sz = if op == 0xF6 { 1 } else { osz };
                let (reg, rm) = self.modrm(bus)?;
                self.group3(bus, reg & 7, rm, sz)?;
            }
            0xF8 => self.eflags &= !CF,
            0xF9 => self.eflags |= CF,
            0xFA => {
                self.io_allowed()?;
                self.eflags &= !IF;
            }
            0xFB => {
                self.io_allowed()?;
                if self.eflags & IF == 0 {
                    self.shadow = true;
                }
                self.eflags |= IF;
            }
            0xFC => self.eflags &= !DF,
            0xFD => self.eflags |= DF,
            0xFE | 0xFF => {
                let sz = if op == 0xFE { 1 } else { osz };
                let (reg, rm) = self.modrm(bus)?;
                match reg & 7 {
                    0 | 1 => {
                        let a = self.get(bus, rm, sz)?;
                        let c = self.cf();
                        let r = self.alu(if reg & 7 == 0 { 0 } else { 5 }, a, 1, sz);
                        self.setf(CF, c != 0);
                        self.set(bus, rm, sz, r)?;
                    }
                    _ if op == 0xFE => return Err(UD),
                    2 => {
                        let bsz = self.stack_osz();
                        let t = self.get(bus, rm, bsz)?;
                        let ip = self.rip;
                        self.push(bus, bsz, ip)?;
                        self.rip = t;
                    }
                    3 | 5 => {
                        let Op::Mem(s, a) = rm else { return Err(UD) };
                        let off = self.rd(bus, s, a, osz)?;
                        let sel = self.rd(bus, s, a.wrapping_add(osz as u64), 2)?;
                        self.far_transfer(bus, sel as u16, off, reg & 7 == 3)?;
                    }
                    4 => {
                        let bsz = self.stack_osz();
                        self.rip = self.get(bus, rm, bsz)?;
                    }
                    6 => {
                        let bsz = self.stack_osz();
                        let v = self.get(bus, rm, bsz)?;
                        self.push(bus, bsz, v)?;
                    }
                    _ => return Err(UD),
                }
            }
            _ => return Err(UD),
        }
        Ok(())
    }

    /// Signed multiply with CF/OF on overflow.
    pub(crate) fn imul(&mut self, a: u64, b: u64, sz: u32) -> u64 {
        let p = sx(a, sz) as i64 as i128 * sx(b, sz) as i64 as i128;
        let r = p as u64 & mask(sz);
        let over = sx(r, sz) as i64 as i128 != p;
        self.setf(CF, over);
        self.setf(OF, over);
        r
    }

    fn group3<B: Bus>(&mut self, bus: &mut B, reg: u8, rm: Op, sz: u32) -> R<()> {
        let m = mask(sz);
        let bits = sz * 8;
        match reg {
            0 | 1 => {
                let b = self.imm(bus, sz)?;
                let a = self.get(bus, rm, sz)?;
                self.alu(4, a, b, sz);
            }
            2 => {
                let a = self.get(bus, rm, sz)?;
                self.set(bus, rm, sz, !a & m)?;
            }
            3 => {
                let a = self.get(bus, rm, sz)?;
                let r = self.alu(5, 0, a, sz);
                self.setf(CF, a & m != 0);
                self.set(bus, rm, sz, r)?;
            }
            4 | 5 => {
                let b = self.get(bus, rm, sz)?;
                let a = self.reg(0, sz);
                let (lo, hi, over) = if reg == 4 {
                    let p = a as u128 * b as u128;
                    let hi = (p >> bits) as u64 & m;
                    (p as u64 & m, hi, hi != 0)
                } else {
                    let p = sx(a, sz) as i64 as i128 * sx(b, sz) as i64 as i128;
                    let lo = p as u64 & m;
                    (lo, (p >> bits) as u64 & m, sx(lo, sz) as i64 as i128 != p)
                };
                self.setf(CF, over);
                self.setf(OF, over);
                if sz == 1 {
                    self.set_reg(0, 2, hi << 8 | lo);
                } else {
                    self.set_reg(0, sz, lo);
                    self.set_reg(EDX as u8, sz, hi);
                }
            }
            _ => {
                let d = self.get(bus, rm, sz)?;
                if d == 0 {
                    return Err(DE);
                }
                let n: u128 = if sz == 1 { self.reg(0, 2) as u128 } else { (self.reg(EDX as u8, sz) as u128) << bits | self.reg(0, sz) as u128 };
                let (q, rem) = if reg == 6 {
                    let q = n / d as u128;
                    if q > m as u128 {
                        return Err(DE);
                    }
                    (q as u64, (n % d as u128) as u64)
                } else {
                    let nb = bits * 2;
                    let n = ((n << (128 - nb)) as i128) >> (128 - nb);
                    let d = sx(d, sz) as i64 as i128;
                    let q = n / d;
                    let lim = sign(sz) as i128 - 1;
                    if q > lim || q < -lim - 1 {
                        return Err(DE);
                    }
                    (q as u64 & m, (n % d) as u64 & m)
                };
                if sz == 1 {
                    self.set_reg(0, 2, rem << 8 | q);
                } else {
                    self.set_reg(0, sz, q);
                    self.set_reg(EDX as u8, sz, rem);
                }
            }
        }
        Ok(())
    }

    /// String instructions with REP / REPE / REPNE.
    pub(crate) fn string<B: Bus>(&mut self, bus: &mut B, op: u32) -> R<()> {
        let sz = if op & 1 == 0 { 1 } else { self.osz };
        let io_sz = sz.min(4);
        let sz = if matches!(op, 0x6C..=0x6F) { io_sz } else { sz };
        let asz = self.asz;
        let step = if self.eflags & DF != 0 { (sz as u64).wrapping_neg() } else { sz as u64 };
        let src = self.ds(DS);
        let cmp = matches!(op, 0xA6 | 0xA7 | 0xAE | 0xAF);
        let mut budget = 4096u32;
        loop {
            if self.rep != 0 && self.reg(ECX as u8, asz) == 0 {
                break;
            }
            let (si, di) = (self.reg(ESI as u8, asz), self.reg(EDI as u8, asz));
            match op {
                0x6C | 0x6D => {
                    let v = bus.io_in(self.reg(EDX as u8, 2) as u16, sz as u8) as u64;
                    self.wr(bus, ES, di, sz, v)?;
                }
                0x6E | 0x6F => {
                    let v = self.rd(bus, src, si, sz)?;
                    bus.io_out(self.reg(EDX as u8, 2) as u16, sz as u8, v as u32);
                }
                0xA4 | 0xA5 => {
                    let v = self.rd(bus, src, si, sz)?;
                    self.wr(bus, ES, di, sz, v)?;
                }
                0xA6 | 0xA7 => {
                    let a = self.rd(bus, src, si, sz)?;
                    let b = self.rd(bus, ES, di, sz)?;
                    self.alu(7, a, b, sz);
                }
                0xAA | 0xAB => {
                    let v = self.reg(0, sz);
                    self.wr(bus, ES, di, sz, v)?;
                }
                0xAC | 0xAD => {
                    let v = self.rd(bus, src, si, sz)?;
                    self.set_reg(0, sz, v);
                }
                _ => {
                    let a = self.reg(0, sz);
                    let b = self.rd(bus, ES, di, sz)?;
                    self.alu(7, a, b, sz);
                }
            }
            if matches!(op, 0x6E | 0x6F | 0xA4..=0xA7 | 0xAC | 0xAD) {
                self.set_reg(ESI as u8, asz, si.wrapping_add(step));
            }
            if !matches!(op, 0x6E | 0x6F | 0xAC | 0xAD) {
                self.set_reg(EDI as u8, asz, di.wrapping_add(step));
            }
            if self.rep == 0 {
                break;
            }
            let c = self.reg(ECX as u8, asz).wrapping_sub(1);
            self.set_reg(ECX as u8, asz, c);
            if cmp {
                let z = self.eflags & ZF != 0;
                if (self.rep == 0xF3 && !z) || (self.rep == 0xF2 && z) {
                    break;
                }
            }
            // Long copies yield so interrupts are serviced; the instruction restarts.
            budget -= 1;
            if budget == 0 && self.reg(ECX as u8, asz) != 0 {
                self.rip = self.start;
                break;
            }
        }
        Ok(())
    }
}
