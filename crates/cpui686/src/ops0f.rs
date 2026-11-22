//! Two-byte opcodes (0F xx).

use super::*;

impl Cpu {
    fn need_cpl0(&self) -> R<()> {
        if self.protected() && (self.cpl != 0 || self.v86()) {
            return Err(gp(0));
        }
        Ok(())
    }

    pub(crate) fn exec_0f<B: Bus>(&mut self, bus: &mut B) -> R<()> {
        let op = self.fetch8(bus)? as u8;
        self.cur_op = (op, 1);
        let osz = self.osz;
        let x64 = self.model == Model::X86_64;
        match op {
            0x00 => {
                if !self.protected() || self.v86() {
                    return Err(UD);
                }
                let (reg, rm) = self.modrm(bus)?;
                match reg & 7 {
                    0 => {
                        let v = self.ldtr.0 as u64;
                        self.set(bus, rm, if matches!(rm, Op::Reg(_)) { osz } else { 2 }, v)?
                    }
                    1 => {
                        let v = self.tr.0 as u64;
                        self.set(bus, rm, if matches!(rm, Op::Reg(_)) { osz } else { 2 }, v)?
                    }
                    2 | 3 => {
                        self.need_cpl0()?;
                        let sel = self.get(bus, rm, 2)? as u16;
                        self.load_system(bus, sel, reg & 7 == 3)?;
                    }
                    _ => {
                        // VERR / VERW: readable/writable if the descriptor is present.
                        let sel = self.get(bus, rm, 2)? as u16;
                        let ok = sel & 0xFFFC != 0 && self.descriptor(bus, sel).is_ok_and(|d| d & (1 << 47) != 0 && d & (1 << 44) != 0);
                        self.setf(ZF, ok);
                    }
                }
            }
            0x01 => {
                let (reg, rm) = self.modrm(bus)?;
                match (reg & 7, rm) {
                    (0 | 1, Op::Mem(s, a)) => {
                        let t = if reg & 7 == 0 { self.gdtr } else { self.idtr };
                        self.wr(bus, s, a, 2, t.1 as u64)?;
                        let bsz = if self.code64 { 8 } else { 4 };
                        self.wr(bus, s, a.wrapping_add(2), bsz, t.0)?;
                    }
                    (2 | 3, Op::Mem(s, a)) => {
                        self.need_cpl0()?;
                        let lim = self.rd(bus, s, a, 2)? as u16;
                        let base = if self.code64 {
                            self.rd(bus, s, a.wrapping_add(2), 8)?
                        } else {
                            let b = self.rd(bus, s, a.wrapping_add(2), 4)?;
                            if osz == 2 {
                                b & 0xFF_FFFF
                            } else {
                                b
                            }
                        };
                        if reg & 7 == 2 {
                            self.gdtr = (base, lim);
                        } else {
                            self.idtr = (base, lim);
                        }
                    }
                    (4, _) => {
                        let v = self.cr0;
                        let sz = if matches!(rm, Op::Reg(_)) { osz } else { 2 };
                        self.set(bus, rm, sz, v)?;
                    }
                    (6, _) => {
                        self.need_cpl0()?;
                        let v = self.get(bus, rm, 2)?;
                        let cr0 = (self.cr0 & !0xE) | (v & 0xF) | (self.cr0 & 1);
                        self.write_cr(0, cr0)?;
                    }
                    (7, Op::Mem(..)) => {
                        self.need_cpl0()?;
                        self.flush_tlb(); // INVLPG
                    }
                    (7, Op::Reg(0)) => self.swapgs()?,
                    (7, Op::Reg(1)) => {
                        // RDTSCP
                        self.r[EAX] = self.tsc & 0xFFFF_FFFF;
                        self.r[EDX] = self.tsc >> 32;
                        self.r[ECX] = self.msr.get(&msr::TSC_AUX).copied().unwrap_or(0) & 0xFFFF_FFFF;
                    }
                    // MONITOR / MWAIT: MWAIT behaves like HLT.
                    (1, Op::Reg(0)) => {}
                    (1, Op::Reg(1)) => {
                        self.need_cpl0()?;
                        self.halted = true;
                    }
                    _ => return Err(UD),
                }
            }
            0x02 | 0x03 => {
                // LAR / LSL
                if !self.protected() || self.v86() {
                    return Err(UD);
                }
                let (reg, rm) = self.modrm(bus)?;
                let sel = self.get(bus, rm, 2)? as u16;
                match (sel & 0xFFFC != 0).then(|| self.descriptor(bus, sel)).and_then(Result::ok) {
                    Some(d) => {
                        let v = if op == 0x02 { (d >> 32) & 0x00F0_FF00 } else { Self::limit_of(d) as u64 };
                        self.set_reg(reg, osz, v);
                        self.eflags |= ZF;
                    }
                    None => self.eflags &= !ZF,
                }
            }
            0x05 => self.syscall()?,
            0x06 => {
                self.need_cpl0()?;
                self.cr0 &= !8; // CLTS
            }
            0x07 => self.sysret()?,
            0x08 | 0x09 => self.need_cpl0()?, // INVD / WBINVD
            0x0B => return Err(UD),           // UD2
            0x0D | 0x18..=0x1F => {
                self.modrm(bus)?; // prefetch / hint NOPs
            }
            0x20 | 0x22 => {
                self.need_cpl0()?;
                let m = self.fetch8(bus)? as u8;
                let cr = ((m >> 3) & 7) | (self.rex & 4) << 1;
                let r = ((m & 7) | (self.rex & 1) << 3) as usize;
                let width = if self.code64 { u64::MAX } else { 0xFFFF_FFFF };
                if op == 0x20 {
                    self.r[r] = self.read_cr(cr)? & width;
                } else {
                    let v = self.r[r] & width;
                    self.write_cr(cr, v)?;
                }
            }
            0x21 | 0x23 => {
                self.need_cpl0()?;
                let m = self.fetch8(bus)? as usize;
                if op == 0x21 {
                    self.r[(m & 7) | ((self.rex & 1) << 3) as usize] = 0; // debug registers read as 0
                }
            }
            0x30 => {
                self.need_cpl0()?;
                let v = (self.r[EAX] & 0xFFFF_FFFF) | (self.r[EDX] & 0xFFFF_FFFF) << 32;
                self.write_msr(self.r[ECX] as u32, v);
            }
            0x31 => {
                self.r[EAX] = self.tsc & 0xFFFF_FFFF;
                self.r[EDX] = self.tsc >> 32;
            }
            0x32 => {
                self.need_cpl0()?;
                let v = self.read_msr(self.r[ECX] as u32);
                self.r[EAX] = v & 0xFFFF_FFFF;
                self.r[EDX] = v >> 32;
            }
            0x33 => {
                self.r[EAX] = 0;
                self.r[EDX] = 0;
            }
            0x34 => self.sysenter()?,
            0x35 => self.sysexit()?,
            0x40..=0x4F => {
                let (reg, rm) = self.modrm(bus)?;
                let v = self.get(bus, rm, osz)?;
                if self.cond(op as u32 - 0x40) {
                    self.set_reg(reg, osz, v);
                } else if osz == 4 {
                    // A 32-bit CMOV zero-extends even when the move doesn't happen.
                    let cur = self.reg(reg, 4);
                    self.set_reg(reg, 4, cur);
                }
            }
            0x38 if x64 => {
                let operation = self.fetch8(bus)? as u8;
                self.cur_op = (operation, 2);
                if !matches!(operation, 0xF0 | 0xF1) || self.rep != 0 || self.lock { return Err(UD); }
                let (register, operand) = self.modrm(bus)?;
                if !matches!(operand, Op::Mem(_, _)) { return Err(UD); }
                let value = if operation == 0xF0 { self.get(bus, operand, osz)? } else { self.reg(register, osz) };
                let value = match osz {
                    2 => (value as u16).swap_bytes() as u64,
                    4 => (value as u32).swap_bytes() as u64,
                    _ => value.swap_bytes(),
                };
                if operation == 0xF0 { self.set_reg(register, osz, value); }
                else { self.set(bus, operand, osz, value)?; }
            }
            0x77 => self.sse_exec(bus, op as u32)?,
            0x10..=0x17 | 0x28..=0x2F | 0x50..=0x7F | 0xC2..=0xC6 | 0xD0..=0xFE | 0xAE | 0x38 | 0x3A if op != 0xC3 => {
                if op == 0x38 || op == 0x3A {
                    return Err(UD); // SSSE3/SSE4 maps: not offered by CPUID
                }
                if op == 0xAE && !matches!(self.peek_modrm_reg(bus)?, 0..=3 | 5..=7) {
                    return Err(UD);
                }
                self.sse_exec(bus, op as u32)?
            }
            0xC3 => {
                // MOVNTI
                let (reg, rm) = self.modrm(bus)?;
                let v = self.reg(reg, osz);
                self.set(bus, rm, osz, v)?;
            }
            0x80..=0x8F => {
                let d = sx(self.fetch(bus, osz.min(4))?, osz.min(4));
                if self.cond(op as u32 - 0x80) {
                    self.jump_rel(d);
                }
            }
            0x90..=0x9F => {
                let (_, rm) = self.modrm(bus)?;
                let v = self.cond(op as u32 - 0x90) as u64;
                self.set(bus, rm, 1, v)?;
            }
            0xA0 | 0xA8 => {
                let sz = self.stack_osz();
                let v = self.sel[if op == 0xA0 { FS } else { GS }] as u64;
                self.push(bus, sz, v)?;
            }
            0xA1 | 0xA9 => {
                let sz = self.stack_osz();
                let v = self.pop(bus, sz)?;
                self.load_seg(bus, if op == 0xA1 { FS } else { GS }, v as u16)?;
            }
            0xA2 => self.cpuid(),
            0xA3 | 0xAB | 0xB3 | 0xBB | 0xBA => {
                let (reg, rm) = self.modrm(bus)?;
                let (kind, bit) = if op == 0xBA {
                    if reg & 7 < 4 {
                        return Err(UD);
                    }
                    ((reg & 7) - 4, self.fetch8(bus)?)
                } else {
                    (((op >> 3) & 3), self.reg(reg, osz))
                };
                let bits = osz as u64 * 8;
                // Register bit offsets reach beyond the operand for memory forms.
                let (rm, bit) = match rm {
                    Op::Mem(s, a) if op != 0xBA => {
                        let k = sx(bit, osz) as i64 >> bits.trailing_zeros();
                        (Op::Mem(s, a.wrapping_add((k * osz as i64) as u64)), bit & (bits - 1))
                    }
                    _ => (rm, bit & (bits - 1)),
                };
                let v = self.get(bus, rm, osz)?;
                self.setf(CF, (v >> bit) & 1 != 0);
                let nv = match kind {
                    1 => v | 1 << bit,
                    2 => v & !(1 << bit),
                    3 => v ^ 1 << bit,
                    _ => return Ok(()),
                };
                self.set(bus, rm, osz, nv)?;
            }
            0xA4 | 0xA5 | 0xAC | 0xAD => {
                let (reg, rm) = self.modrm(bus)?;
                let cnt = if op & 1 == 0 { self.fetch8(bus)? } else { self.reg(ECX as u8, 1) } & if osz == 8 { 63 } else { 31 };
                if cnt != 0 {
                    let bits = osz * 8;
                    let (d, s) = (self.get(bus, rm, osz)? as u128, self.reg(reg, osz) as u128);
                    let r = if op < 0xA8 {
                        let w = d << bits | s;
                        self.setf(CF, (d >> (bits as u64 - cnt)) & 1 != 0);
                        (w << cnt >> bits) as u64
                    } else {
                        let w = s << bits | d;
                        self.setf(CF, (d >> (cnt - 1)) & 1 != 0);
                        (w >> cnt) as u64
                    } & mask(osz);
                    self.setf(OF, (r ^ d as u64) & sign(osz) != 0);
                    self.szp(r, osz);
                    self.set(bus, rm, osz, r)?;
                }
            }
            0xAF => {
                let (reg, rm) = self.modrm(bus)?;
                let a = self.reg(reg, osz);
                let b = self.get(bus, rm, osz)?;
                let r = self.imul(a, b, osz);
                self.set_reg(reg, osz, r);
            }
            0xB0 | 0xB1 => {
                let sz = if op == 0xB0 { 1 } else { osz };
                let (reg, rm) = self.modrm(bus)?;
                let v = self.get(bus, rm, sz)?;
                let a = self.reg(0, sz);
                self.alu(7, a, v, sz);
                if self.eflags & ZF != 0 {
                    let n = self.reg(reg, sz);
                    self.set(bus, rm, sz, n)?;
                } else {
                    self.set(bus, rm, sz, v)?;
                    self.set_reg(0, sz, v);
                }
            }
            0xB2 | 0xB4 | 0xB5 => {
                let (reg, rm) = self.modrm(bus)?;
                let Op::Mem(s, a) = rm else { return Err(UD) };
                let off = self.rd(bus, s, a, osz)?;
                let sel = self.rd(bus, s, a.wrapping_add(osz as u64), 2)?;
                self.load_seg(bus, [SS, 0, FS, GS][(op - 0xB2) as usize], sel as u16)?;
                self.set_reg(reg, osz, off);
            }
            0xB6 | 0xB7 | 0xBE | 0xBF => {
                let sz = if op & 1 == 0 { 1 } else { 2 };
                let (reg, rm) = self.modrm(bus)?;
                let v = self.get(bus, rm, sz)?;
                self.set_reg(reg, osz, if op >= 0xBE { sx(v, sz) } else { v });
            }
            0xB8 if self.rep == 0xF3 && x64 => {
                // POPCNT
                let (reg, rm) = self.modrm(bus)?;
                let v = self.get(bus, rm, osz)?;
                self.set_reg(reg, osz, v.count_ones() as u64);
                self.eflags &= !(CF | PF | AF | SF | OF);
                self.setf(ZF, v == 0);
            }
            0xBC | 0xBD => {
                let (reg, rm) = self.modrm(bus)?;
                let v = self.get(bus, rm, osz)?;
                if self.rep == 0xF3 && x64 {
                    let width = osz * 8;
                    let count = if v == 0 { width } else if op == 0xBC { v.trailing_zeros() } else { v.leading_zeros() - (64 - width) };
                    self.set_reg(reg, osz, count as u64);
                    self.setf(CF, v == 0);
                    self.setf(ZF, count == 0);
                } else if v == 0 {
                    self.eflags |= ZF;
                } else {
                    self.eflags &= !ZF;
                    let i = if op == 0xBC { v.trailing_zeros() } else { 63 - v.leading_zeros() };
                    self.set_reg(reg, osz, i as u64);
                }
            }
            0xC0 | 0xC1 => {
                let sz = if op == 0xC0 { 1 } else { osz };
                let (reg, rm) = self.modrm(bus)?;
                let (a, b) = (self.get(bus, rm, sz)?, self.reg(reg, sz));
                let r = self.alu(0, a, b, sz);
                self.set_reg(reg, sz, a);
                self.set(bus, rm, sz, r)?;
            }
            0xC7 => {
                // CMPXCHG8B / CMPXCHG16B
                let (reg, rm) = self.modrm(bus)?;
                let Op::Mem(s, a) = rm else { return Err(UD) };
                if reg & 7 != 1 {
                    return Err(UD);
                }
                let (w, m) = if self.rex & 8 != 0 { (8, u64::MAX) } else { (4, 0xFFFF_FFFF) };
                let lo = self.rd(bus, s, a, w)?;
                let hi = self.rd(bus, s, a.wrapping_add(w as u64), w)?;
                if lo == self.r[EAX] & m && hi == self.r[EDX] & m {
                    let (b, c) = (self.r[EBX] & m, self.r[ECX] & m);
                    self.wr(bus, s, a, w, b)?;
                    self.wr(bus, s, a.wrapping_add(w as u64), w, c)?;
                    self.eflags |= ZF;
                } else {
                    self.set_reg(EAX as u8, w, lo);
                    self.set_reg(EDX as u8, w, hi);
                    self.eflags &= !ZF;
                }
            }
            0xC8..=0xCF => {
                let i = (op & 7) | (self.rex & 1) << 3;
                let v = self.reg(i, osz);
                let r = if osz == 8 { v.swap_bytes() } else { (v as u32).swap_bytes() as u64 };
                self.set_reg(i, osz.max(4), r);
            }
            0xFF => self.trap = Some(self.fetch8(bus)? as u8),
            _ => return Err(UD),
        }
        Ok(())
    }

    fn limit_of(d: u64) -> u32 {
        let l = (d & 0xFFFF) as u32 | ((d >> 32) as u32 & 0xF_0000);
        if d & (1 << 55) != 0 {
            l << 12 | 0xFFF
        } else {
            l
        }
    }

    /// The reg field of the next ModRM byte, without consuming it.
    fn peek_modrm_reg<B: Bus>(&mut self, bus: &mut B) -> R<u8> {
        let ip = self.rip;
        let m = self.fetch8(bus)? as u8;
        self.rip = ip;
        Ok((m >> 3) & 7)
    }
}
