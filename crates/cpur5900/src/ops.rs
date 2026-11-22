//! Integer, branch, load/store and system instructions of the R5900.

use crate::*;

#[inline]
fn s32(v: u32) -> u64 {
    v as i32 as i64 as u64
}

#[inline]
fn simm(op: u32) -> i64 {
    (op & 0xFFFF) as i16 as i64
}

impl Cpu {
    pub(crate) fn reserved(&mut self) {
        self.unimplemented += 1;
        self.raise(exc::RI, 0, false);
    }

    fn branch_target(&self, op: u32) -> u32 {
        // Relative to the delay slot (the instruction after the branch), which is `pc` now.
        self.pc.wrapping_add((simm(op) << 2) as u32)
    }

    pub(crate) fn execute<B: Bus>(&mut self, bus: &mut B, op: u32) {
        let rs = ((op >> 21) & 31) as usize;
        let rt = ((op >> 16) & 31) as usize;
        let rd = ((op >> 11) & 31) as usize;
        let sa = (op >> 6) & 31;
        let imm = op & 0xFFFF;
        let (a, b) = (self.lo[rs], self.lo[rt]);
        match op >> 26 {
            0 => self.special(bus, op, rs, rt, rd, sa),
            1 => self.regimm(op, rs, rt),
            2 => {
                let t = (self.pc & 0xF000_0000) | ((op & 0x03FF_FFFF) << 2);
                self.branch_to(t);
            }
            3 => {
                self.set_reg(31, self.pc.wrapping_add(4) as u64);
                let t = (self.pc & 0xF000_0000) | ((op & 0x03FF_FFFF) << 2);
                self.branch_to(t);
            }
            4 | 20 => self.cond_branch(op, a == b, op >> 26 == 20),
            5 | 21 => self.cond_branch(op, a != b, op >> 26 == 21),
            6 | 22 => self.cond_branch(op, (a as i64) <= 0, op >> 26 == 22),
            7 | 23 => self.cond_branch(op, (a as i64) > 0, op >> 26 == 23),
            8 => match (a as u32 as i32).checked_add(simm(op) as i32) {
                Some(v) => self.set_reg(rt, v as i64 as u64),
                None => self.raise(exc::OV, 0, false),
            },
            9 => self.set_reg(rt, s32((a as u32).wrapping_add(simm(op) as u32))),
            10 => self.set_reg(rt, ((a as i64) < simm(op)) as u64),
            11 => self.set_reg(rt, (a < simm(op) as u64) as u64),
            12 => self.set_reg(rt, a & imm as u64),
            13 => self.set_reg(rt, a | imm as u64),
            14 => self.set_reg(rt, a ^ imm as u64),
            15 => self.set_reg(rt, s32(imm << 16)),
            16 => self.cop0_op(op, rs, rt, rd),
            17 => self.cop1_op(op, rs, rt, rd),
            18 => bus.cop2(op, self),
            24 => match (a as i64).checked_add(simm(op)) {
                Some(v) => self.set_reg(rt, v as u64),
                None => self.raise(exc::OV, 0, false),
            },
            25 => self.set_reg(rt, a.wrapping_add(simm(op) as u64)),
            26 => self.ldl_ldr(bus, op, rs, rt, true),
            27 => self.ldl_ldr(bus, op, rs, rt, false),
            28 => self.mmi(bus, op, rs, rt, rd, sa),
            30 => {
                // lq
                let ea = (a as u32).wrapping_add(simm(op) as u32) & !0xF;
                let (Ok(lo), Ok(hi)) = (self.load(bus, ea, 8), self.load(bus, ea.wrapping_add(8), 8)) else { return };
                if rt != 0 {
                    self.lo[rt] = lo;
                    self.hi_half[rt] = hi;
                }
            }
            31 => {
                // sq
                let ea = (a as u32).wrapping_add(simm(op) as u32) & !0xF;
                let (lo, hi) = (self.lo[rt], self.hi_half[rt]);
                if self.store(bus, ea, 8, lo).is_ok() {
                    let _ = self.store(bus, ea.wrapping_add(8), 8, hi);
                }
            }
            32 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                if let Ok(v) = self.load(bus, ea, 1) {
                    self.set_reg(rt, v as u8 as i8 as i64 as u64);
                }
            }
            33 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                if let Ok(v) = self.load(bus, ea, 2) {
                    self.set_reg(rt, v as u16 as i16 as i64 as u64);
                }
            }
            34 | 38 => self.lwl_lwr(bus, op, rs, rt, op >> 26 == 34),
            35 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                if let Ok(v) = self.load(bus, ea, 4) {
                    self.set_reg(rt, s32(v as u32));
                }
            }
            36 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                if let Ok(v) = self.load(bus, ea, 1) {
                    self.set_reg(rt, v & 0xFF);
                }
            }
            37 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                if let Ok(v) = self.load(bus, ea, 2) {
                    self.set_reg(rt, v & 0xFFFF);
                }
            }
            39 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                if let Ok(v) = self.load(bus, ea, 4) {
                    self.set_reg(rt, v & 0xFFFF_FFFF);
                }
            }
            40 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                let _ = self.store(bus, ea, 1, b);
            }
            41 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                let _ = self.store(bus, ea, 2, b);
            }
            42 | 46 => self.swl_swr(bus, op, rs, rt, op >> 26 == 42),
            43 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                let _ = self.store(bus, ea, 4, b);
            }
            44 | 45 => self.sdl_sdr(bus, op, rs, rt, op >> 26 == 44),
            47 | 51 => {} // cache, pref
            49 => {
                // lwc1
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                if let Ok(v) = self.load(bus, ea, 4) {
                    self.fpr[rt] = v as u32;
                }
            }
            54 => bus.cop2(op, self), // lqc2
            55 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                if let Ok(v) = self.load(bus, ea, 8) {
                    self.set_reg(rt, v);
                }
            }
            57 => {
                // swc1
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                let v = self.fpr[rt] as u64;
                let _ = self.store(bus, ea, 4, v);
            }
            62 => bus.cop2(op, self), // sqc2
            63 => {
                let ea = (a as u32).wrapping_add(simm(op) as u32);
                let _ = self.store(bus, ea, 8, b);
            }
            _ => self.reserved(),
        }
    }

    fn cond_branch(&mut self, op: u32, taken: bool, likely: bool) {
        let target = self.branch_target(op);
        if taken {
            self.branch_to(target);
        } else if likely {
            self.skip_delay_slot();
        } else {
            self.mark_delay();
        }
    }

    fn regimm(&mut self, op: u32, rs: usize, rt: usize) {
        let a = self.lo[rs] as i64;
        match rt {
            0 | 2 => self.cond_branch(op, a < 0, rt == 2),
            1 | 3 => self.cond_branch(op, a >= 0, rt == 3),
            16 | 18 => {
                self.set_reg(31, self.pc.wrapping_add(4) as u64);
                self.cond_branch(op, a < 0, rt == 18);
            }
            17 | 19 => {
                self.set_reg(31, self.pc.wrapping_add(4) as u64);
                self.cond_branch(op, a >= 0, rt == 19);
            }
            24 => self.sa = (self.lo[rs] as u32 & 0xF) ^ (((op & 0xFFFF) & 0xF) as u32), // mtsab
            25 => self.sa = ((self.lo[rs] as u32 & 7) ^ ((op & 0xFFFF) & 7)) * 2, // mtsah
            _ => self.reserved(),
        }
    }

    fn special<B: Bus>(&mut self, _bus: &mut B, op: u32, rs: usize, rt: usize, rd: usize, sa: u32) {
        let (a, b) = (self.lo[rs], self.lo[rt]);
        match op & 63 {
            0 => self.set_reg(rd, s32((b as u32) << sa)),
            2 => self.set_reg(rd, s32((b as u32) >> sa)),
            3 => self.set_reg(rd, s32(((b as u32 as i32) >> sa) as u32)),
            4 => self.set_reg(rd, s32((b as u32) << (a & 31))),
            6 => self.set_reg(rd, s32((b as u32) >> (a & 31))),
            7 => self.set_reg(rd, s32(((b as u32 as i32) >> (a & 31)) as u32)),
            8 => self.branch_to(a as u32),
            9 => {
                let ret = self.pc.wrapping_add(4) as u64;
                self.branch_to(a as u32);
                self.set_reg(rd, ret);
            }
            10 => {
                if b == 0 {
                    self.set_reg(rd, a);
                }
            }
            11 => {
                if b != 0 {
                    self.set_reg(rd, a);
                }
            }
            12 => self.raise(exc::SYS, 0, false),
            13 => self.raise(exc::BP, 0, false),
            15 => {} // sync
            16 => self.set_reg(rd, self.hi),
            17 => self.hi = a,
            18 => self.set_reg(rd, self.lo_reg),
            19 => self.lo_reg = a,
            20 => self.set_reg(rd, b << (a & 63)),
            22 => self.set_reg(rd, b >> (a & 63)),
            23 => self.set_reg(rd, ((b as i64) >> (a & 63)) as u64),
            24 => {
                let p = (a as u32 as i32 as i64) * (b as u32 as i32 as i64);
                self.lo_reg = s32(p as u32);
                self.hi = s32((p >> 32) as u32);
                self.set_reg(rd, self.lo_reg);
            }
            25 => {
                let p = (a as u32 as u64) * (b as u32 as u64);
                self.lo_reg = s32(p as u32);
                self.hi = s32((p >> 32) as u32);
                self.set_reg(rd, self.lo_reg);
            }
            26 => {
                let (n, d) = (a as u32 as i32, b as u32 as i32);
                if d == 0 {
                    self.lo_reg = if n < 0 { 1 } else { u64::MAX };
                    self.hi = n as i64 as u64;
                } else if n == i32::MIN && d == -1 {
                    self.lo_reg = s32(i32::MIN as u32);
                    self.hi = 0;
                } else {
                    self.lo_reg = (n / d) as i64 as u64;
                    self.hi = (n % d) as i64 as u64;
                }
            }
            27 => {
                let (n, d) = (a as u32, b as u32);
                if d == 0 {
                    self.lo_reg = u64::MAX;
                    self.hi = s32(n);
                } else {
                    self.lo_reg = s32(n / d);
                    self.hi = s32(n % d);
                }
            }
            32 => match (a as u32 as i32).checked_add(b as u32 as i32) {
                Some(v) => self.set_reg(rd, v as i64 as u64),
                None => self.raise(exc::OV, 0, false),
            },
            33 => self.set_reg(rd, s32((a as u32).wrapping_add(b as u32))),
            34 => match (a as u32 as i32).checked_sub(b as u32 as i32) {
                Some(v) => self.set_reg(rd, v as i64 as u64),
                None => self.raise(exc::OV, 0, false),
            },
            35 => self.set_reg(rd, s32((a as u32).wrapping_sub(b as u32))),
            36 => self.set_reg(rd, a & b),
            37 => self.set_reg(rd, a | b),
            38 => self.set_reg(rd, a ^ b),
            39 => self.set_reg(rd, !(a | b)),
            40 => self.set_reg(rd, self.sa as u64), // mfsa
            41 => self.sa = a as u32,               // mtsa
            42 => self.set_reg(rd, ((a as i64) < (b as i64)) as u64),
            43 => self.set_reg(rd, (a < b) as u64),
            44 => match (a as i64).checked_add(b as i64) {
                Some(v) => self.set_reg(rd, v as u64),
                None => self.raise(exc::OV, 0, false),
            },
            45 => self.set_reg(rd, a.wrapping_add(b)),
            46 => match (a as i64).checked_sub(b as i64) {
                Some(v) => self.set_reg(rd, v as u64),
                None => self.raise(exc::OV, 0, false),
            },
            47 => self.set_reg(rd, a.wrapping_sub(b)),
            48 => {
                if (a as i64) >= (b as i64) {
                    self.raise(exc::TR, 0, false)
                }
            }
            49 => {
                if a >= b {
                    self.raise(exc::TR, 0, false)
                }
            }
            50 => {
                if (a as i64) < (b as i64) {
                    self.raise(exc::TR, 0, false)
                }
            }
            51 => {
                if a < b {
                    self.raise(exc::TR, 0, false)
                }
            }
            52 => {
                if a == b {
                    self.raise(exc::TR, 0, false)
                }
            }
            54 => {
                if a != b {
                    self.raise(exc::TR, 0, false)
                }
            }
            56 => self.set_reg(rd, b << sa),
            58 => self.set_reg(rd, b >> sa),
            59 => self.set_reg(rd, ((b as i64) >> sa) as u64),
            60 => self.set_reg(rd, b << (sa + 32)),
            62 => self.set_reg(rd, b >> (sa + 32)),
            63 => self.set_reg(rd, ((b as i64) >> (sa + 32)) as u64),
            _ => self.reserved(),
        }
    }

    // ------------------------------------------------------------ unaligned accesses

    fn lwl_lwr<B: Bus>(&mut self, bus: &mut B, op: u32, rs: usize, rt: usize, left: bool) {
        let ea = (self.lo[rs] as u32).wrapping_add(simm(op) as u32);
        let aligned = ea & !3;
        let Ok(word) = self.load(bus, aligned, 4) else { return };
        let word = word as u32;
        let cur = self.lo[rt] as u32;
        let n = ea & 3;
        let v = if left {
            let sh = (3 - n) * 8;
            let mask = if sh == 0 { 0 } else { 0xFFFF_FFFFu32 >> (32 - sh) };
            (word << sh) | (cur & mask)
        } else {
            let sh = n * 8;
            let mask = if sh == 0 { 0 } else { 0xFFFF_FFFFu32 << (32 - sh) };
            (word >> sh) | (cur & mask)
        };
        self.set_reg(rt, s32(v));
    }

    fn swl_swr<B: Bus>(&mut self, bus: &mut B, op: u32, rs: usize, rt: usize, left: bool) {
        let ea = (self.lo[rs] as u32).wrapping_add(simm(op) as u32);
        let aligned = ea & !3;
        let Ok(word) = self.load(bus, aligned, 4) else { return };
        let word = word as u32;
        let reg = self.lo[rt] as u32;
        let n = ea & 3;
        let v = if left {
            let sh = (3 - n) * 8;
            let mask = if sh == 0 { 0 } else { 0xFFFF_FFFFu32 << (32 - sh) };
            (reg >> sh) | (word & mask)
        } else {
            let sh = n * 8;
            let mask = if sh == 0 { 0 } else { 0xFFFF_FFFFu32 >> (32 - sh) };
            (reg << sh) | (word & mask)
        };
        let _ = self.store(bus, aligned, 4, v as u64);
    }

    fn ldl_ldr<B: Bus>(&mut self, bus: &mut B, op: u32, rs: usize, rt: usize, left: bool) {
        let ea = (self.lo[rs] as u32).wrapping_add(simm(op) as u32);
        let aligned = ea & !7;
        let Ok(dw) = self.load(bus, aligned, 8) else { return };
        let cur = self.lo[rt];
        let n = ea & 7;
        let v = if left {
            let sh = (7 - n) * 8;
            let mask = if sh == 0 { 0 } else { u64::MAX >> (64 - sh) };
            (dw << sh) | (cur & mask)
        } else {
            let sh = n * 8;
            let mask = if sh == 0 { 0 } else { u64::MAX << (64 - sh) };
            (dw >> sh) | (cur & mask)
        };
        self.set_reg(rt, v);
    }

    fn sdl_sdr<B: Bus>(&mut self, bus: &mut B, op: u32, rs: usize, rt: usize, left: bool) {
        let ea = (self.lo[rs] as u32).wrapping_add(simm(op) as u32);
        let aligned = ea & !7;
        let Ok(dw) = self.load(bus, aligned, 8) else { return };
        let reg = self.lo[rt];
        let n = ea & 7;
        let v = if left {
            let sh = (7 - n) * 8;
            let mask = if sh == 0 { 0 } else { u64::MAX << (64 - sh) };
            (reg >> sh) | (dw & mask)
        } else {
            let sh = n * 8;
            let mask = if sh == 0 { 0 } else { u64::MAX >> (64 - sh) };
            (reg << sh) | (dw & mask)
        };
        let _ = self.store(bus, aligned, 8, v);
    }

    // ------------------------------------------------------------ coprocessor 0

    fn cop0_op(&mut self, op: u32, rs: usize, rt: usize, rd: usize) {
        match rs {
            0 => {
                // mfc0
                let v = self.cop0[rd];
                self.set_reg(rt, s32(v));
            }
            4 => {
                // mtc0
                let v = self.lo[rt] as u32;
                match rd {
                    COUNT | COMPARE => {
                        self.cop0[rd] = v;
                        if rd == COMPARE {
                            self.cop0[CAUSE] &= !(1 << 15);
                        }
                    }
                    CAUSE => self.cop0[CAUSE] = (self.cop0[CAUSE] & !0x300) | (v & 0x300),
                    RANDOM | PRID => {}
                    _ => self.cop0[rd] = v,
                }
            }
            16 => match op & 63 {
                1 => self.tlbr(),
                2 => self.tlbw(self.cop0[INDEX] as usize % TLB_ENTRIES),
                6 => self.tlbw(self.cop0[RANDOM] as usize % TLB_ENTRIES),
                8 => self.tlbp(),
                24 => {
                    // eret
                    let status = self.cop0[STATUS];
                    if status & ST_ERL != 0 {
                        self.cop0[STATUS] &= !ST_ERL;
                        self.pc = self.cop0[ERROREPC];
                    } else {
                        self.cop0[STATUS] &= !ST_EXL;
                        self.pc = self.cop0[EPC];
                    }
                    self.next_pc = self.pc.wrapping_add(4);
                    self.in_delay = false;
                }
                56 => self.cop0[STATUS] |= ST_EIE, // ei
                57 => self.cop0[STATUS] &= !ST_EIE, // di
                _ => self.reserved(),
            },
            _ => self.reserved(),
        }
    }

    fn tlbr(&mut self) {
        let e = self.tlb[self.cop0[INDEX] as usize % TLB_ENTRIES];
        self.cop0[PAGEMASK] = e.mask;
        self.cop0[ENTRYHI] = e.hi & !(e.mask >> 0 & 0x1FFF);
        self.cop0[ENTRYLO0] = (e.lo0 & !1) | (e.global() as u32);
        self.cop0[ENTRYLO1] = (e.lo1 & !1) | (e.global() as u32);
    }

    fn tlbw(&mut self, idx: usize) {
        let g = (self.cop0[ENTRYLO0] & self.cop0[ENTRYLO1]) & 1;
        self.tlb[idx] = TlbEntry {
            mask: self.cop0[PAGEMASK] & 0x01FF_E000,
            hi: self.cop0[ENTRYHI],
            lo0: (self.cop0[ENTRYLO0] & !1) | g,
            lo1: (self.cop0[ENTRYLO1] & !1) | g,
        };
    }

    fn tlbp(&mut self) {
        let hi = self.cop0[ENTRYHI];
        let asid = hi & 0xFF;
        let found = self.tlb.iter().position(|e| {
            let span = e.span();
            (hi & !span & 0xFFFF_E000) == (e.hi & !span & 0xFFFF_E000) && (e.global() || e.hi & 0xFF == asid)
        });
        self.cop0[INDEX] = match found {
            Some(i) => i as u32,
            None => 1 << 31,
        };
    }
}
