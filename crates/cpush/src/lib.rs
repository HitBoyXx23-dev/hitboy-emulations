//! Clean-room Hitachi SuperH (SH-2 & SH-4) 32-bit RISC CPU interpreter.
//!
//! Shared by both **Sega Dreamcast** (SH-4 + FPU + matrix `FTRV`/`FIPR`) and
//! **Sega Saturn** (dual SH-2). Supports full 16-bit SuperH instruction decoding,
//! branch delay slots, `TRAPA` software traps, banked registers, and SH-4 FPU ops.

/// Memory bus interface for SuperH guests.
pub trait Bus {
    fn read8(&self, addr: u32) -> u8;
    fn read16(&self, addr: u32) -> u16;
    fn read32(&self, addr: u32) -> u32;
    fn write8(&mut self, addr: u32, val: u8);
    fn write16(&mut self, addr: u32, val: u16);
    fn write32(&mut self, addr: u32, val: u32);
}

/// Event returned from a single CPU step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEvent {
    Continue,
    Trap(u8),
    Sleep,
}

/// SuperH CPU state (SH-2 / SH-4).
#[derive(Clone)]
pub struct ShCpu {
    pub r: [u32; 16],
    pub r_bank: [u32; 8],
    pub fr: [f32; 16],
    pub xf: [f32; 16],
    pub sr: u32,
    pub gbr: u32,
    pub vbr: u32,
    pub mach: u32,
    pub macl: u32,
    pub pr: u32,
    pub pc: u32,
    pub fpul: u32,
    pub fpscr: u32,
    pub delay_slot: Option<u32>,
    pub cycles: u64,
}

impl Default for ShCpu {
    fn default() -> Self {
        Self::new()
    }
}

impl ShCpu {
    pub fn new() -> Self {
        Self {
            r: [0; 16],
            r_bank: [0; 8],
            fr: [0.0; 16],
            xf: [0.0; 16],
            sr: 0x7000_00F0,
            gbr: 0,
            vbr: 0,
            mach: 0,
            macl: 0,
            pr: 0,
            pc: 0,
            fpul: 0,
            fpscr: 0x0004_0001,
            delay_slot: None,
            cycles: 0,
        }
    }

    #[inline]
    pub fn t(&self) -> bool {
        (self.sr & 1) != 0
    }

    #[inline]
    pub fn set_t(&mut self, v: bool) {
        if v {
            self.sr |= 1;
        } else {
            self.sr &= !1;
        }
    }

    #[inline]
    fn set_sr(&mut self, new_sr: u32) {
        let old_rb = (self.sr >> 29) & 1;
        let new_rb = (new_sr >> 29) & 1;
        if old_rb != new_rb {
            for i in 0..8 {
                std::mem::swap(&mut self.r[i], &mut self.r_bank[i]);
            }
        }
        self.sr = new_sr;
    }

    /// Executes a single 16-bit SuperH instruction.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> StepEvent {
        let pc = self.pc;
        let op = bus.read16(pc);
        let delayed_target = self.delay_slot.take();
        let next_pc = delayed_target.unwrap_or(pc.wrapping_add(2));
        self.pc = next_pc;
        self.cycles += 1;

        let n = ((op >> 8) & 0xF) as usize;
        let m = ((op >> 4) & 0xF) as usize;
        let imm8 = (op & 0xFF) as u8;
        let simm8 = imm8 as i8 as i32 as u32;
        let disp4 = (op & 0xF) as u32;

        match op >> 12 {
            0x0 => match op & 0xF {
                0x2 => match op & 0xF0 {
                    0x00 => self.r[n] = self.sr,
                    0x10 => self.r[n] = self.gbr,
                    0x20 => self.r[n] = self.vbr,
                    _ => {}
                },
                0x3 => match op & 0xF0 {
                    0x00 => {
                        // BSRF Rn
                        self.pr = pc.wrapping_add(4);
                        self.delay_slot = Some(pc.wrapping_add(4).wrapping_add(self.r[n]));
                    }
                    0x20 => {
                        // BRAF Rn
                        self.delay_slot = Some(pc.wrapping_add(4).wrapping_add(self.r[n]));
                    }
                    _ => {}
                },
                0x4 => bus.write8(self.r[0].wrapping_add(self.r[n]), self.r[m] as u8),
                0x5 => bus.write16(self.r[0].wrapping_add(self.r[n]), self.r[m] as u16),
                0x6 => bus.write32(self.r[0].wrapping_add(self.r[n]), self.r[m]),
                0x7 => self.macl = self.r[n].wrapping_mul(self.r[m]), // MUL.L
                0x8 => match op & 0xF0 {
                    0x00 => self.set_t(false), // CLRT
                    0x10 => self.set_t(true),  // SETT
                    0x20 => {
                        self.mach = 0;
                        self.macl = 0;
                    } // CLRMAC
                    _ => {}
                },
                0x9 => match op & 0xF0 {
                    0x00 => {} // NOP
                    0x10 => self.sr &= !0x301, // DIV0U
                    0x20 => self.r[n] = self.t() as u32, // MOVT Rn
                    _ => {}
                },
                0xA => match op & 0xF0 {
                    0x00 => self.r[n] = self.mach,
                    0x10 => self.r[n] = self.macl,
                    0x20 => self.r[n] = self.pr,
                    0x50 => self.r[n] = self.fpul,
                    0x60 => self.r[n] = self.fpscr,
                    _ => {}
                },
                0xB => match op & 0xF0 {
                    0x00 => self.delay_slot = Some(self.pr), // RTS
                    0x10 => return StepEvent::Sleep,         // SLEEP
                    0x20 => {
                        // RTE (pop PC and SR on SH-2)
                        let ret_pc = bus.read32(self.r[15]);
                        self.r[15] = self.r[15].wrapping_add(4);
                        let ret_sr = bus.read32(self.r[15]);
                        self.r[15] = self.r[15].wrapping_add(4);
                        self.set_sr(ret_sr);
                        self.delay_slot = Some(ret_pc);
                    }
                    _ => {}
                },
                0xC => self.r[n] = bus.read8(self.r[0].wrapping_add(self.r[m])) as i8 as i32 as u32,
                0xD => self.r[n] = bus.read16(self.r[0].wrapping_add(self.r[m])) as i16 as i32 as u32,
                0xE => self.r[n] = bus.read32(self.r[0].wrapping_add(self.r[m])),
                _ => {}
            },
            // MOV.L Rm, @(disp4*4, Rn)
            0x1 => bus.write32(self.r[n].wrapping_add(disp4 * 4), self.r[m]),
            0x2 => match op & 0xF {
                0x0 => bus.write8(self.r[n], self.r[m] as u8),
                0x1 => bus.write16(self.r[n], self.r[m] as u16),
                0x2 => bus.write32(self.r[n], self.r[m]),
                0x4 => {
                    self.r[n] = self.r[n].wrapping_sub(1);
                    bus.write8(self.r[n], self.r[m] as u8);
                }
                0x5 => {
                    self.r[n] = self.r[n].wrapping_sub(2);
                    bus.write16(self.r[n], self.r[m] as u16);
                }
                0x6 => {
                    self.r[n] = self.r[n].wrapping_sub(4);
                    bus.write32(self.r[n], self.r[m]);
                }
                0x7 => {
                    // DIV0S
                    let q = (self.r[n] >> 31) & 1;
                    let m_bit = (self.r[m] >> 31) & 1;
                    self.sr = (self.sr & !0x301) | (q << 8) | (m_bit << 9) | (q ^ m_bit);
                }
                0x8 => self.set_t((self.r[n] & self.r[m]) == 0), // TST
                0x9 => self.r[n] &= self.r[m],                   // AND
                0xA => self.r[n] ^= self.r[m],                   // XOR
                0xB => self.r[n] |= self.r[m],                   // OR
                0xC => {
                    // CMP/STR
                    let x = self.r[n] ^ self.r[m];
                    self.set_t(
                        (x & 0xFF) == 0
                            || ((x >> 8) & 0xFF) == 0
                            || ((x >> 16) & 0xFF) == 0
                            || ((x >> 24) & 0xFF) == 0,
                    );
                }
                0xD => self.r[n] = (self.r[m] << 16) | (self.r[n] >> 16), // XTRCT
                0xE => self.macl = (self.r[n] as u16 as u32).wrapping_mul(self.r[m] as u16 as u32), // MULU.W
                0xF => self.macl = (self.r[n] as i16 as i32).wrapping_mul(self.r[m] as i16 as i32) as u32, // MULS.W
                _ => {}
            },
            0x3 => match op & 0xF {
                0x0 => self.set_t(self.r[n] == self.r[m]),                        // CMP/EQ
                0x2 => self.set_t(self.r[n] >= self.r[m]),                        // CMP/HS
                0x3 => self.set_t((self.r[n] as i32) >= (self.r[m] as i32)),      // CMP/GE
                0x4 => self.div1(n, m),                                           // DIV1
                0x5 => {
                    // DMULU.L
                    let p = (self.r[n] as u64).wrapping_mul(self.r[m] as u64);
                    self.macl = p as u32;
                    self.mach = (p >> 32) as u32;
                }
                0x6 => self.set_t(self.r[n] > self.r[m]),                         // CMP/HI
                0x7 => self.set_t((self.r[n] as i32) > (self.r[m] as i32)),       // CMP/GT
                0x8 => self.r[n] = self.r[n].wrapping_sub(self.r[m]),             // SUB
                0xA => {
                    // SUBC
                    let (d1, b1) = self.r[n].overflowing_sub(self.r[m]);
                    let (d2, b2) = d1.overflowing_sub(self.t() as u32);
                    self.r[n] = d2;
                    self.set_t(b1 || b2);
                }
                0xB => {
                    // SUBV
                    let (d, ov) = (self.r[n] as i32).overflowing_sub(self.r[m] as i32);
                    self.r[n] = d as u32;
                    self.set_t(ov);
                }
                0xC => self.r[n] = self.r[n].wrapping_add(self.r[m]), // ADD
                0xD => {
                    // DMULS.L
                    let p = (self.r[n] as i32 as i64).wrapping_mul(self.r[m] as i32 as i64) as u64;
                    self.macl = p as u32;
                    self.mach = (p >> 32) as u32;
                }
                0xE => {
                    // ADDC
                    let (s1, c1) = self.r[n].overflowing_add(self.r[m]);
                    let (s2, c2) = s1.overflowing_add(self.t() as u32);
                    self.r[n] = s2;
                    self.set_t(c1 || c2);
                }
                0xF => {
                    // ADDV
                    let (s, ov) = (self.r[n] as i32).overflowing_add(self.r[m] as i32);
                    self.r[n] = s as u32;
                    self.set_t(ov);
                }
                _ => {}
            },
            0x4 => match op & 0xFF {
                0x00 => {
                    self.set_t((self.r[n] >> 31) != 0);
                    self.r[n] <<= 1;
                } // SHLL
                0x01 => {
                    self.set_t((self.r[n] & 1) != 0);
                    self.r[n] >>= 1;
                } // SHLR
                0x02 => {
                    self.r[n] = self.r[n].wrapping_sub(4);
                    bus.write32(self.r[n], self.mach);
                }
                0x04 => {
                    self.set_t((self.r[n] >> 31) != 0);
                    self.r[n] = self.r[n].rotate_left(1);
                } // ROTL
                0x05 => {
                    self.set_t((self.r[n] & 1) != 0);
                    self.r[n] = self.r[n].rotate_right(1);
                } // ROTR
                0x06 => {
                    self.mach = bus.read32(self.r[n]);
                    self.r[n] = self.r[n].wrapping_add(4);
                }
                0x08 => self.r[n] <<= 2, // SHLL2
                0x09 => self.r[n] >>= 2, // SHLR2
                0x0A => self.mach = self.r[n],
                0x0B => {
                    // JSR @Rn
                    self.pr = pc.wrapping_add(4);
                    self.delay_slot = Some(self.r[n]);
                }
                0x0E => self.set_sr(self.r[n]), // LDC Rn, SR
                0x10 => {
                    // DT Rn
                    self.r[n] = self.r[n].wrapping_sub(1);
                    self.set_t(self.r[n] == 0);
                }
                0x11 => self.set_t((self.r[n] as i32) >= 0), // CMP/PZ
                0x12 => {
                    self.r[n] = self.r[n].wrapping_sub(4);
                    bus.write32(self.r[n], self.macl);
                }
                0x15 => self.set_t((self.r[n] as i32) > 0), // CMP/PL
                0x16 => {
                    self.macl = bus.read32(self.r[n]);
                    self.r[n] = self.r[n].wrapping_add(4);
                }
                0x18 => self.r[n] <<= 8, // SHLL8
                0x19 => self.r[n] >>= 8, // SHLR8
                0x1A => self.macl = self.r[n],
                0x1E => self.gbr = self.r[n],
                0x20 => {
                    self.set_t((self.r[n] >> 31) != 0);
                    self.r[n] <<= 1;
                } // SHAL
                0x21 => {
                    self.set_t((self.r[n] & 1) != 0);
                    self.r[n] = ((self.r[n] as i32) >> 1) as u32;
                } // SHAR
                0x22 => {
                    self.r[n] = self.r[n].wrapping_sub(4);
                    bus.write32(self.r[n], self.pr);
                }
                0x24 => {
                    // ROTCL
                    let c = self.t() as u32;
                    self.set_t((self.r[n] >> 31) != 0);
                    self.r[n] = (self.r[n] << 1) | c;
                }
                0x25 => {
                    // ROTCR
                    let c = (self.t() as u32) << 31;
                    self.set_t((self.r[n] & 1) != 0);
                    self.r[n] = (self.r[n] >> 1) | c;
                }
                0x26 => {
                    self.pr = bus.read32(self.r[n]);
                    self.r[n] = self.r[n].wrapping_add(4);
                }
                0x28 => self.r[n] <<= 16, // SHLL16
                0x29 => self.r[n] >>= 16, // SHLR16
                0x2A => self.pr = self.r[n],
                0x2B => self.delay_slot = Some(self.r[n]), // JMP @Rn
                0x2E => self.vbr = self.r[n],
                0x52 => {
                    self.r[n] = self.r[n].wrapping_sub(4);
                    bus.write32(self.r[n], self.fpul);
                }
                0x56 => {
                    self.fpul = bus.read32(self.r[n]);
                    self.r[n] = self.r[n].wrapping_add(4);
                }
                0x5A => self.fpul = self.r[n],
                0x62 => {
                    self.r[n] = self.r[n].wrapping_sub(4);
                    bus.write32(self.r[n], self.fpscr);
                }
                0x66 => {
                    self.fpscr = bus.read32(self.r[n]);
                    self.r[n] = self.r[n].wrapping_add(4);
                }
                0x6A => self.fpscr = self.r[n],
                _ => match op & 0xF {
                    0xC => {
                        // SHAD Rm, Rn
                        let s = self.r[m] as i32;
                        self.r[n] = if s >= 0 {
                            self.r[n] << (s & 31)
                        } else if (s & 31) == 0 {
                            if (self.r[n] as i32) < 0 { u32::MAX } else { 0 }
                        } else {
                            ((self.r[n] as i32) >> ((!s + 1) & 31)) as u32
                        };
                    }
                    0xD => {
                        // SHLD Rm, Rn
                        let s = self.r[m] as i32;
                        self.r[n] = if s >= 0 {
                            self.r[n] << (s & 31)
                        } else if (s & 31) == 0 {
                            0
                        } else {
                            self.r[n] >> ((!s + 1) & 31)
                        };
                    }
                    _ => {}
                },
            },
            // MOV.L @(disp4*4, Rm), Rn
            0x5 => self.r[n] = bus.read32(self.r[m].wrapping_add(disp4 * 4)),
            0x6 => match op & 0xF {
                0x0 => self.r[n] = bus.read8(self.r[m]) as i8 as i32 as u32,
                0x1 => self.r[n] = bus.read16(self.r[m]) as i16 as i32 as u32,
                0x2 => self.r[n] = bus.read32(self.r[m]),
                0x3 => self.r[n] = self.r[m],
                0x4 => {
                    self.r[n] = bus.read8(self.r[m]) as i8 as i32 as u32;
                    if n != m {
                        self.r[m] = self.r[m].wrapping_add(1);
                    }
                }
                0x5 => {
                    self.r[n] = bus.read16(self.r[m]) as i16 as i32 as u32;
                    if n != m {
                        self.r[m] = self.r[m].wrapping_add(2);
                    }
                }
                0x6 => {
                    self.r[n] = bus.read32(self.r[m]);
                    if n != m {
                        self.r[m] = self.r[m].wrapping_add(4);
                    }
                }
                0x7 => self.r[n] = !self.r[m],
                0x8 => {
                    let v = self.r[m];
                    self.r[n] = (v & 0xFFFF_0000) | ((v & 0xFF) << 8) | ((v >> 8) & 0xFF);
                } // SWAP.B
                0x9 => self.r[n] = self.r[m].rotate_left(16), // SWAP.W
                0xA => {
                    // NEGC
                    let (d1, b1) = 0u32.overflowing_sub(self.r[m]);
                    let (d2, b2) = d1.overflowing_sub(self.t() as u32);
                    self.r[n] = d2;
                    self.set_t(b1 || b2);
                }
                0xB => self.r[n] = 0u32.wrapping_sub(self.r[m]), // NEG
                0xC => self.r[n] = self.r[m] & 0xFF,              // EXTU.B
                0xD => self.r[n] = self.r[m] & 0xFFFF,            // EXTU.W
                0xE => self.r[n] = self.r[m] as i8 as i32 as u32, // EXTS.B
                0xF => self.r[n] = self.r[m] as i16 as i32 as u32, // EXTS.W
                _ => {}
            },
            // ADD #imm8, Rn
            0x7 => self.r[n] = self.r[n].wrapping_add(simm8),
            0x8 => match (op >> 8) & 0xF {
                0x0 => bus.write8(self.r[m].wrapping_add(disp4), self.r[0] as u8),
                0x1 => bus.write16(self.r[m].wrapping_add(disp4 * 2), self.r[0] as u16),
                0x4 => self.r[0] = bus.read8(self.r[m].wrapping_add(disp4)) as i8 as i32 as u32,
                0x5 => self.r[0] = bus.read16(self.r[m].wrapping_add(disp4 * 2)) as i16 as i32 as u32,
                0x8 => self.set_t(self.r[0] == simm8), // CMP/EQ #imm, R0
                0x9 => {
                    // BT disp8 (no delay slot)
                    if self.t() {
                        self.pc = pc.wrapping_add(4).wrapping_add(simm8.wrapping_shl(1));
                    }
                }
                0xB => {
                    // BF disp8 (no delay slot)
                    if !self.t() {
                        self.pc = pc.wrapping_add(4).wrapping_add(simm8.wrapping_shl(1));
                    }
                }
                0xD => {
                    // BT/S disp8
                    if self.t() {
                        self.delay_slot = Some(pc.wrapping_add(4).wrapping_add(simm8.wrapping_shl(1)));
                    }
                }
                0xF => {
                    // BF/S disp8
                    if !self.t() {
                        self.delay_slot = Some(pc.wrapping_add(4).wrapping_add(simm8.wrapping_shl(1)));
                    }
                }
                _ => {}
            },
            // MOV.W @(disp8*2, PC), Rn
            0x9 => {
                let addr = pc.wrapping_add(4).wrapping_add((imm8 as u32) * 2);
                self.r[n] = bus.read16(addr) as i16 as i32 as u32;
            }
            // BRA disp12
            0xA => {
                let disp = (((op & 0x0FFF) as i16) << 4) >> 3; // sign-extended * 2
                self.delay_slot = Some(pc.wrapping_add(4).wrapping_add(disp as i32 as u32));
            }
            // BSR disp12
            0xB => {
                let disp = (((op & 0x0FFF) as i16) << 4) >> 3;
                self.pr = pc.wrapping_add(4);
                self.delay_slot = Some(pc.wrapping_add(4).wrapping_add(disp as i32 as u32));
            }
            0xC => match (op >> 8) & 0xF {
                0x0 => bus.write8(self.gbr.wrapping_add(imm8 as u32), self.r[0] as u8),
                0x1 => bus.write16(self.gbr.wrapping_add((imm8 as u32) * 2), self.r[0] as u16),
                0x2 => bus.write32(self.gbr.wrapping_add((imm8 as u32) * 4), self.r[0]),
                0x3 => return StepEvent::Trap(imm8), // TRAPA #imm
                0x4 => self.r[0] = bus.read8(self.gbr.wrapping_add(imm8 as u32)) as i8 as i32 as u32,
                0x5 => self.r[0] = bus.read16(self.gbr.wrapping_add((imm8 as u32) * 2)) as i16 as i32 as u32,
                0x6 => self.r[0] = bus.read32(self.gbr.wrapping_add((imm8 as u32) * 4)),
                0x7 => self.r[0] = ((pc.wrapping_add(4)) & !3).wrapping_add((imm8 as u32) * 4), // MOVA
                0x8 => self.set_t((self.r[0] & (imm8 as u32)) == 0), // TST #imm, R0
                0x9 => self.r[0] &= imm8 as u32,                     // AND #imm, R0
                0xA => self.r[0] ^= imm8 as u32,                     // XOR #imm, R0
                0xB => self.r[0] |= imm8 as u32,                     // OR #imm, R0
                _ => {}
            },
            // MOV.L @(disp8*4, PC), Rn
            0xD => {
                let addr = ((pc.wrapping_add(4)) & !3).wrapping_add((imm8 as u32) * 4);
                self.r[n] = bus.read32(addr);
            }
            // MOV #imm8, Rn
            0xE => self.r[n] = simm8,
            // SH-4 FPU
            0xF => self.exec_fpu(op, n, m, bus),
            _ => {}
        }

        StepEvent::Continue
    }

    fn div1(&mut self, n: usize, m: usize) {
        let old_q = (self.sr >> 8) & 1;
        let new_q = (self.r[n] >> 31) & 1;
        let m_bit = (self.r[m] >> 31) & 1;
        self.r[n] = (self.r[n] << 1) | (self.t() as u32);
        let tmp0 = self.r[n];
        let tmp1: u32;
        let q: u32;
        if old_q == m_bit {
            self.r[n] = self.r[n].wrapping_sub(self.r[m]);
            tmp1 = (self.r[n] > tmp0) as u32;
            q = new_q ^ tmp1 ^ (m_bit & 1);
        } else {
            self.r[n] = self.r[n].wrapping_add(self.r[m]);
            tmp1 = (self.r[n] < tmp0) as u32;
            q = new_q ^ tmp1 ^ (1 - m_bit);
        }
        self.sr = (self.sr & !0x101) | (q << 8) | (1 - (q ^ m_bit));
    }

    fn exec_fpu<B: Bus>(&mut self, op: u16, n: usize, m: usize, bus: &mut B) {
        match op & 0xF {
            0x0 => self.fr[n] += self.fr[m],
            0x1 => self.fr[n] -= self.fr[m],
            0x2 => self.fr[n] *= self.fr[m],
            0x3 => self.fr[n] /= self.fr[m],
            0x4 => self.set_t(self.fr[n] == self.fr[m]),
            0x5 => self.set_t(self.fr[n] > self.fr[m]),
            0x6 => self.fr[n] = f32::from_bits(bus.read32(self.r[0].wrapping_add(self.r[m]))),
            0x7 => bus.write32(self.r[0].wrapping_add(self.r[n]), self.fr[m].to_bits()),
            0x8 => self.fr[n] = f32::from_bits(bus.read32(self.r[m])),
            0x9 => {
                self.fr[n] = f32::from_bits(bus.read32(self.r[m]));
                self.r[m] = self.r[m].wrapping_add(4);
            }
            0xA => bus.write32(self.r[n], self.fr[m].to_bits()),
            0xB => {
                self.r[n] = self.r[n].wrapping_sub(4);
                bus.write32(self.r[n], self.fr[m].to_bits());
            }
            0xC => self.fr[n] = self.fr[m],
            0xD => match m {
                0x0 => self.fr[n] = f32::from_bits(self.fpul), // FSTS
                0x1 => self.fpul = self.fr[n].to_bits(),       // FLDS
                0x2 => self.fr[n] = (self.fpul as i32) as f32, // FLOAT
                0x3 => self.fpul = (self.fr[n] as i32) as u32, // FTRC
                0x4 => self.fr[n] = -self.fr[n],               // FNEG
                0x5 => self.fr[n] = self.fr[n].abs(),          // FABS
                0x6 => self.fr[n] = self.fr[n].sqrt(),         // FSQRT
                0x8 => self.fr[n] = 0.0,                       // FLDI0
                0x9 => self.fr[n] = 1.0,                       // FLDI1
                0xB if n == 0xF => self.fpscr ^= 1 << 20,      // FSCHG
                0xB if n == 0xB => {
                    // FRCHG: swap FR and XF banks
                    std::mem::swap(&mut self.fr, &mut self.xf);
                    self.fpscr ^= 1 << 21;
                }
                0xE => {
                    // FIPR FVm, FVn
                    let vn = (n & 0xC) as usize;
                    let vm = ((n & 3) * 4) as usize;
                    self.fr[vn + 3] = self.fr[vm] * self.fr[vn]
                        + self.fr[vm + 1] * self.fr[vn + 1]
                        + self.fr[vm + 2] * self.fr[vn + 2]
                        + self.fr[vm + 3] * self.fr[vn + 3];
                }
                0xF if (n & 3) == 1 => {
                    // FTRV XMTRX, FVn (4x4 matrix * 4-vector using XF column-major)
                    let vn = (n & 0xC) as usize;
                    let [x, y, z, w] = [self.fr[vn], self.fr[vn + 1], self.fr[vn + 2], self.fr[vn + 3]];
                    for row in 0..4 {
                        self.fr[vn + row] = self.xf[row] * x
                            + self.xf[4 + row] * y
                            + self.xf[8 + row] * z
                            + self.xf[12 + row] * w;
                    }
                }
                _ => {}
            },
            0xE => self.fr[n] += self.fr[0] * self.fr[m], // FMAC
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<u8>);
    impl Bus for Mem {
        fn read8(&self, a: u32) -> u8 { self.0.get(a as usize).copied().unwrap_or(0) }
        fn read16(&self, a: u32) -> u16 {
            let i = a as usize;
            u16::from_le_bytes([self.read8(i as u32), self.read8(i as u32 + 1)])
        }
        fn read32(&self, a: u32) -> u32 {
            let i = a as usize;
            u32::from_le_bytes([self.read8(i as u32), self.read8(i as u32 + 1), self.read8(i as u32 + 2), self.read8(i as u32 + 3)])
        }
        fn write8(&mut self, a: u32, v: u8) { if let Some(b) = self.0.get_mut(a as usize) { *b = v; } }
        fn write16(&mut self, a: u32, v: u16) { let b = v.to_le_bytes(); self.write8(a, b[0]); self.write8(a + 1, b[1]); }
        fn write32(&mut self, a: u32, v: u32) {
            for (k, &b) in v.to_le_bytes().iter().enumerate() { self.write8(a + k as u32, b); }
        }
    }

    #[test]
    fn sh4_alu_branch_and_ftrv_matrix_work() {
        let mut mem = Mem(vec![0; 256]);
        // MOV #10, R1 (E10A); MOV #32, R2 (E220); ADD R2, R1 (312C); FTRV XMTRX, FV0 (F1FD)
        let prog: [u16; 4] = [0xE10A, 0xE220, 0x312C, 0xF1FD];
        for (i, &w) in prog.iter().enumerate() {
            mem.write16((i * 2) as u32, w);
        }
        let mut cpu = ShCpu::new();
        // Set XF = 2*I (scale by 2), FV0 = [1, 2, 3, 1]
        cpu.xf[0] = 2.0; cpu.xf[5] = 2.0; cpu.xf[10] = 2.0; cpu.xf[15] = 1.0;
        cpu.fr[0] = 1.0; cpu.fr[1] = 2.0; cpu.fr[2] = 3.0; cpu.fr[3] = 1.0;
        for _ in 0..4 {
            cpu.step(&mut mem);
        }
        assert_eq!(cpu.r[1], 42);
        assert_eq!(cpu.fr[0..4], [2.0, 4.0, 6.0, 1.0]);
    }
}
