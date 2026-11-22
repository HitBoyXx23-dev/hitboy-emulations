//! Clean-room MIPS32R2 Allegrex CPU interpreter for PlayStation Portable.
//!
//! Covers integer MIPS32R2 instructions, Allegrex extensions (`max`, `min`, `bitrev`,
//! `rotr`, `seb`, `seh`, `wsbh`, `ext`, `ins`, `clo`, `clz`, `madd`, `msub`, `movn`, `movz`),
//! single-precision COP1 FPU, delay-slot branches, and HLE `syscall` traps.

/// Memory bus interface implemented by the PSP machine.
pub trait Bus {
    fn read8(&self, addr: u32) -> u8;
    fn read16(&self, addr: u32) -> u16;
    fn read32(&self, addr: u32) -> u32;
    fn write8(&mut self, addr: u32, val: u8);
    fn write16(&mut self, addr: u32, val: u16);
    fn write32(&mut self, addr: u32, val: u32);
}

/// Reason `step` paused execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEvent {
    Continue,
    Syscall(u32),
    Break(u32),
}

/// Allegrex MIPS32R2 CPU state.
#[derive(Clone)]
pub struct Allegrex {
    pub r: [u32; 32],
    pub f: [f32; 32],
    pub fcr31: u32,
    pub hi: u32,
    pub lo: u32,
    pub pc: u32,
    pub next_pc: u32,
    pub cycles: u64,
}

impl Default for Allegrex {
    fn default() -> Self {
        Self::new()
    }
}

impl Allegrex {
    pub fn new() -> Self {
        Self {
            r: [0; 32],
            f: [0.0; 32],
            fcr31: 0,
            hi: 0,
            lo: 0,
            pc: 0x0880_4000,
            next_pc: 0x0880_4004,
            cycles: 0,
        }
    }

    pub fn set_pc(&mut self, addr: u32) {
        self.pc = addr;
        self.next_pc = addr.wrapping_add(4);
    }

    #[inline]
    fn set_r(&mut self, reg: usize, val: u32) {
        if reg != 0 {
            self.r[reg] = val;
        }
    }

    #[inline]
    fn fcc(&self) -> bool {
        (self.fcr31 & (1 << 23)) != 0
    }

    #[inline]
    fn set_fcc(&mut self, cond: bool) {
        if cond {
            self.fcr31 |= 1 << 23;
        } else {
            self.fcr31 &= !(1 << 23);
        }
    }

    /// Executes a single Allegrex instruction, respecting MIPS branch delay slots.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> StepEvent {
        let pc = self.pc;
        let op = bus.read32(pc);
        let delay_pc = self.next_pc;
        self.pc = delay_pc;
        self.next_pc = delay_pc.wrapping_add(4);
        self.cycles += 1;

        let opcode = op >> 26;
        let rs = ((op >> 21) & 0x1F) as usize;
        let rt = ((op >> 16) & 0x1F) as usize;
        let rd = ((op >> 11) & 0x1F) as usize;
        let shamt = (op >> 6) & 0x1F;
        let funct = op & 0x3F;
        let imm = (op & 0xFFFF) as u16;
        let simm = (imm as i16) as i32 as u32;
        let uimm = imm as u32;

        match opcode {
            // SPECIAL
            0x00 => match funct {
                0x00 => self.set_r(rd, self.r[rt] << shamt), // SLL
                0x02 => {
                    // SRL / ROTR (bit 21 = 1 is Allegrex/MIPS32R2 ROTR)
                    let v = self.r[rt];
                    let res = if rs & 1 != 0 { v.rotate_right(shamt) } else { v >> shamt };
                    self.set_r(rd, res);
                }
                0x03 => self.set_r(rd, ((self.r[rt] as i32) >> shamt) as u32), // SRA
                0x04 => self.set_r(rd, self.r[rt] << (self.r[rs] & 31)), // SLLV
                0x06 => {
                    let s = self.r[rs] & 31;
                    let v = self.r[rt];
                    let res = if shamt & 1 != 0 { v.rotate_right(s) } else { v >> s };
                    self.set_r(rd, res);
                }
                0x07 => self.set_r(rd, ((self.r[rt] as i32) >> (self.r[rs] & 31)) as u32), // SRAV
                0x08 => self.next_pc = self.r[rs], // JR
                0x09 => {
                    // JALR
                    let target = self.r[rs];
                    self.set_r(rd, delay_pc.wrapping_add(4));
                    self.next_pc = target;
                }
                0x0A => {
                    // MOVZ
                    if self.r[rt] == 0 {
                        self.set_r(rd, self.r[rs]);
                    }
                }
                0x0B => {
                    // MOVN
                    if self.r[rt] != 0 {
                        self.set_r(rd, self.r[rs]);
                    }
                }
                0x0C => return StepEvent::Syscall((op >> 6) & 0xF_FFFF), // SYSCALL
                0x0D => return StepEvent::Break((op >> 6) & 0xF_FFFF),   // BREAK
                0x0F => {}                                               // SYNC
                0x10 => self.set_r(rd, self.hi),                         // MFHI
                0x11 => self.hi = self.r[rs],                            // MTHI
                0x12 => self.set_r(rd, self.lo),                         // MFLO
                0x13 => self.lo = self.r[rs],                            // MTLO
                0x16 => self.set_r(rd, self.r[rs].leading_zeros()),      // CLZ (Allegrex)
                0x17 => self.set_r(rd, (!self.r[rs]).leading_zeros()),   // CLO (Allegrex)
                0x18 => {
                    // MULT
                    let prod = (self.r[rs] as i32 as i64).wrapping_mul(self.r[rt] as i32 as i64);
                    self.lo = prod as u32;
                    self.hi = (prod >> 32) as u32;
                }
                0x19 => {
                    // MULTU
                    let prod = (self.r[rs] as u64).wrapping_mul(self.r[rt] as u64);
                    self.lo = prod as u32;
                    self.hi = (prod >> 32) as u32;
                }
                0x1A => {
                    // DIV
                    let a = self.r[rs] as i32;
                    let b = self.r[rt] as i32;
                    if b != 0 {
                        self.lo = a.wrapping_div(b) as u32;
                        self.hi = a.wrapping_rem(b) as u32;
                    }
                }
                0x1B => {
                    // DIVU
                    let a = self.r[rs];
                    let b = self.r[rt];
                    if b != 0 {
                        self.lo = a / b;
                        self.hi = a % b;
                    }
                }
                0x1C => {
                    // MADD (Allegrex)
                    let acc = ((self.hi as u64) << 32) | (self.lo as u64);
                    let prod = (self.r[rs] as i32 as i64).wrapping_mul(self.r[rt] as i32 as i64);
                    let res = (acc as i64).wrapping_add(prod) as u64;
                    self.lo = res as u32;
                    self.hi = (res >> 32) as u32;
                }
                0x1D => {
                    // MADDU (Allegrex)
                    let acc = ((self.hi as u64) << 32) | (self.lo as u64);
                    let prod = (self.r[rs] as u64).wrapping_mul(self.r[rt] as u64);
                    let res = acc.wrapping_add(prod);
                    self.lo = res as u32;
                    self.hi = (res >> 32) as u32;
                }
                0x20 | 0x21 => self.set_r(rd, self.r[rs].wrapping_add(self.r[rt])), // ADD / ADDU
                0x22 | 0x23 => self.set_r(rd, self.r[rs].wrapping_sub(self.r[rt])), // SUB / SUBU
                0x24 => self.set_r(rd, self.r[rs] & self.r[rt]),                    // AND
                0x25 => self.set_r(rd, self.r[rs] | self.r[rt]),                    // OR
                0x26 => self.set_r(rd, self.r[rs] ^ self.r[rt]),                    // XOR
                0x27 => self.set_r(rd, !(self.r[rs] | self.r[rt])),                 // NOR
                0x2A => self.set_r(rd, ((self.r[rs] as i32) < (self.r[rt] as i32)) as u32), // SLT
                0x2B => self.set_r(rd, (self.r[rs] < self.r[rt]) as u32),           // SLTU
                0x2C => self.set_r(rd, (self.r[rs] as i32).max(self.r[rt] as i32) as u32), // MAX (Allegrex)
                0x2D => self.set_r(rd, (self.r[rs] as i32).min(self.r[rt] as i32) as u32), // MIN (Allegrex)
                0x2E => {
                    // MSUB (Allegrex)
                    let acc = ((self.hi as u64) << 32) | (self.lo as u64);
                    let prod = (self.r[rs] as i32 as i64).wrapping_mul(self.r[rt] as i32 as i64);
                    let res = (acc as i64).wrapping_sub(prod) as u64;
                    self.lo = res as u32;
                    self.hi = (res >> 32) as u32;
                }
                0x2F => {
                    // MSUBU (Allegrex)
                    let acc = ((self.hi as u64) << 32) | (self.lo as u64);
                    let prod = (self.r[rs] as u64).wrapping_mul(self.r[rt] as u64);
                    let res = acc.wrapping_sub(prod);
                    self.lo = res as u32;
                    self.hi = (res >> 32) as u32;
                }
                _ => {}
            },
            // REGIMM (BLTZ, BGEZ, BLTZL, BGEZL, BLTZAL, BGEZAL)
            0x01 => {
                let s = self.r[rs] as i32;
                let target = delay_pc.wrapping_add(simm << 2);
                match rt {
                    0x00 => {
                        if s < 0 {
                            self.next_pc = target;
                        }
                    }
                    0x01 => {
                        if s >= 0 {
                            self.next_pc = target;
                        }
                    }
                    0x02 => {
                        if s < 0 {
                            self.next_pc = target;
                        } else {
                            self.pc = delay_pc.wrapping_add(4);
                            self.next_pc = self.pc.wrapping_add(4);
                        }
                    }
                    0x03 => {
                        if s >= 0 {
                            self.next_pc = target;
                        } else {
                            self.pc = delay_pc.wrapping_add(4);
                            self.next_pc = self.pc.wrapping_add(4);
                        }
                    }
                    0x10 => {
                        self.set_r(31, delay_pc.wrapping_add(4));
                        if s < 0 {
                            self.next_pc = target;
                        }
                    }
                    0x11 => {
                        self.set_r(31, delay_pc.wrapping_add(4));
                        if s >= 0 {
                            self.next_pc = target;
                        }
                    }
                    _ => {}
                }
            }
            // J
            0x02 => self.next_pc = (delay_pc & 0xF000_0000) | ((op & 0x03FF_FFFF) << 2),
            // JAL
            0x03 => {
                self.set_r(31, delay_pc.wrapping_add(4));
                self.next_pc = (delay_pc & 0xF000_0000) | ((op & 0x03FF_FFFF) << 2);
            }
            // BEQ
            0x04 => {
                if self.r[rs] == self.r[rt] {
                    self.next_pc = delay_pc.wrapping_add(simm << 2);
                }
            }
            // BNE
            0x05 => {
                if self.r[rs] != self.r[rt] {
                    self.next_pc = delay_pc.wrapping_add(simm << 2);
                }
            }
            // BLEZ
            0x06 => {
                if (self.r[rs] as i32) <= 0 {
                    self.next_pc = delay_pc.wrapping_add(simm << 2);
                }
            }
            // BGTZ
            0x07 => {
                if (self.r[rs] as i32) > 0 {
                    self.next_pc = delay_pc.wrapping_add(simm << 2);
                }
            }
            // ADDI / ADDIU
            0x08 | 0x09 => self.set_r(rt, self.r[rs].wrapping_add(simm)),
            // SLTI
            0x0A => self.set_r(rt, ((self.r[rs] as i32) < (simm as i32)) as u32),
            // SLTIU
            0x0B => self.set_r(rt, (self.r[rs] < simm) as u32),
            // ANDI
            0x0C => self.set_r(rt, self.r[rs] & uimm),
            // ORI
            0x0D => self.set_r(rt, self.r[rs] | uimm),
            // XORI
            0x0E => self.set_r(rt, self.r[rs] ^ uimm),
            // LUI
            0x0F => self.set_r(rt, uimm << 16),
            // COP1 (FPU)
            0x11 => self.exec_cop1(op, rs, rt, rd, shamt, funct, delay_pc, simm),
            // BEQL
            0x14 => {
                if self.r[rs] == self.r[rt] {
                    self.next_pc = delay_pc.wrapping_add(simm << 2);
                } else {
                    self.pc = delay_pc.wrapping_add(4);
                    self.next_pc = self.pc.wrapping_add(4);
                }
            }
            // BNEL
            0x15 => {
                if self.r[rs] != self.r[rt] {
                    self.next_pc = delay_pc.wrapping_add(simm << 2);
                } else {
                    self.pc = delay_pc.wrapping_add(4);
                    self.next_pc = self.pc.wrapping_add(4);
                }
            }
            // BLEZL
            0x16 => {
                if (self.r[rs] as i32) <= 0 {
                    self.next_pc = delay_pc.wrapping_add(simm << 2);
                } else {
                    self.pc = delay_pc.wrapping_add(4);
                    self.next_pc = self.pc.wrapping_add(4);
                }
            }
            // BGTZL
            0x17 => {
                if (self.r[rs] as i32) > 0 {
                    self.next_pc = delay_pc.wrapping_add(simm << 2);
                } else {
                    self.pc = delay_pc.wrapping_add(4);
                    self.next_pc = self.pc.wrapping_add(4);
                }
            }
            // SPECIAL2 (MUL, CLZ, CLO)
            0x1C => match funct {
                0x02 => self.set_r(rd, self.r[rs].wrapping_mul(self.r[rt])),
                0x20 => self.set_r(rd, self.r[rs].leading_zeros()),
                0x21 => self.set_r(rd, (!self.r[rs]).leading_zeros()),
                _ => {}
            },
            // SPECIAL3 (EXT, INS, WSBH, SEB, SEH, BITREV)
            0x1F => match funct {
                0x00 => {
                    // EXT rt, rs, pos, size
                    let pos = shamt;
                    let size = rd as u32 + 1;
                    let mask = if size >= 32 { u32::MAX } else { (1u32 << size) - 1 };
                    self.set_r(rt, (self.r[rs] >> pos) & mask);
                }
                0x04 => {
                    // INS rt, rs, pos, size
                    let pos = shamt;
                    let msb = rd as u32;
                    if msb >= pos {
                        let size = msb - pos + 1;
                        let mask = if size >= 32 { u32::MAX } else { (1u32 << size) - 1 };
                        let val = (self.r[rt] & !(mask << pos)) | ((self.r[rs] & mask) << pos);
                        self.set_r(rt, val);
                    }
                }
                0x20 => match shamt {
                    0x02 => {
                        // WSBH
                        let v = self.r[rt];
                        let lo = (v as u16).swap_bytes() as u32;
                        let hi = ((v >> 16) as u16).swap_bytes() as u32;
                        self.set_r(rd, (hi << 16) | lo);
                    }
                    0x03 => self.set_r(rd, self.r[rt].swap_bytes()), // WSBW
                    0x10 => self.set_r(rd, self.r[rt] as i8 as i32 as u32), // SEB
                    0x14 => self.set_r(rd, self.r[rt].reverse_bits()), // BITREV (Allegrex)
                    0x18 => self.set_r(rd, self.r[rt] as i16 as i32 as u32), // SEH
                    _ => {}
                },
                _ => {}
            },
            // Loads
            0x20 => {
                let a = self.r[rs].wrapping_add(simm);
                self.set_r(rt, bus.read8(a) as i8 as i32 as u32);
            }
            0x21 => {
                let a = self.r[rs].wrapping_add(simm);
                self.set_r(rt, bus.read16(a) as i16 as i32 as u32);
            }
            0x22 => {
                // LWL
                let a = self.r[rs].wrapping_add(simm);
                let w = bus.read32(a & !3);
                let cur = self.r[rt];
                let res = match a & 3 {
                    0 => (cur & 0x00FF_FFFF) | (w << 24),
                    1 => (cur & 0x0000_FFFF) | (w << 16),
                    2 => (cur & 0x0000_00FF) | (w << 8),
                    _ => w,
                };
                self.set_r(rt, res);
            }
            0x23 => {
                let a = self.r[rs].wrapping_add(simm);
                self.set_r(rt, bus.read32(a));
            }
            0x24 => {
                let a = self.r[rs].wrapping_add(simm);
                self.set_r(rt, bus.read8(a) as u32);
            }
            0x25 => {
                let a = self.r[rs].wrapping_add(simm);
                self.set_r(rt, bus.read16(a) as u32);
            }
            0x26 => {
                // LWR
                let a = self.r[rs].wrapping_add(simm);
                let w = bus.read32(a & !3);
                let cur = self.r[rt];
                let res = match a & 3 {
                    0 => w,
                    1 => (cur & 0xFF00_0000) | (w >> 8),
                    2 => (cur & 0xFFFF_0000) | (w >> 16),
                    _ => (cur & 0xFFFF_FF00) | (w >> 24),
                };
                self.set_r(rt, res);
            }
            // Stores
            0x28 => bus.write8(self.r[rs].wrapping_add(simm), self.r[rt] as u8),
            0x29 => bus.write16(self.r[rs].wrapping_add(simm), self.r[rt] as u16),
            0x2A => {
                // SWL
                let a = self.r[rs].wrapping_add(simm);
                let aligned = a & !3;
                let cur = bus.read32(aligned);
                let v = self.r[rt];
                let out = match a & 3 {
                    0 => (cur & 0xFFFF_FF00) | (v >> 24),
                    1 => (cur & 0xFFFF_0000) | (v >> 16),
                    2 => (cur & 0xFF00_0000) | (v >> 8),
                    _ => v,
                };
                bus.write32(aligned, out);
            }
            0x2B => bus.write32(self.r[rs].wrapping_add(simm), self.r[rt]),
            0x2E => {
                // SWR
                let a = self.r[rs].wrapping_add(simm);
                let aligned = a & !3;
                let cur = bus.read32(aligned);
                let v = self.r[rt];
                let out = match a & 3 {
                    0 => v,
                    1 => (cur & 0x0000_00FF) | (v << 8),
                    2 => (cur & 0x0000_FFFF) | (v << 16),
                    _ => (cur & 0x00FF_FFFF) | (v << 24),
                };
                bus.write32(aligned, out);
            }
            0x2F => {} // CACHE
            0x30 => {
                // LL
                let a = self.r[rs].wrapping_add(simm);
                self.set_r(rt, bus.read32(a));
            }
            0x31 => {
                // LWC1
                let a = self.r[rs].wrapping_add(simm);
                self.f[rt] = f32::from_bits(bus.read32(a));
            }
            0x38 => {
                // SC
                let a = self.r[rs].wrapping_add(simm);
                bus.write32(a, self.r[rt]);
                self.set_r(rt, 1);
            }
            0x39 => {
                // SWC1
                let a = self.r[rs].wrapping_add(simm);
                bus.write32(a, self.f[rt].to_bits());
            }
            _ => {}
        }

        StepEvent::Continue
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_cop1(&mut self, _op: u32, fmt: usize, ft: usize, fs: usize, fd: u32, funct: u32, delay_pc: u32, simm: u32) {
        let fd = fd as usize;
        match fmt {
            0x00 => self.set_r(ft, self.f[fs].to_bits()), // MFC1
            0x02 => self.set_r(ft, self.fcr31),           // CFC1
            0x04 => self.f[fs] = f32::from_bits(self.r[ft]), // MTC1
            0x06 => self.fcr31 = self.r[ft],              // CTC1
            0x08 => {
                // BC1F / BC1T / BC1FL / BC1TL
                let cond = if ft & 1 != 0 { self.fcc() } else { !self.fcc() };
                let likely = ft & 2 != 0;
                if cond {
                    self.next_pc = delay_pc.wrapping_add(simm << 2);
                } else if likely {
                    self.pc = delay_pc.wrapping_add(4);
                    self.next_pc = self.pc.wrapping_add(4);
                }
            }
            0x10 => {
                // Single precision (.S)
                match funct {
                    0x00 => self.f[fd] = self.f[fs] + self.f[ft],
                    0x01 => self.f[fd] = self.f[fs] - self.f[ft],
                    0x02 => self.f[fd] = self.f[fs] * self.f[ft],
                    0x03 => self.f[fd] = self.f[fs] / self.f[ft],
                    0x04 => self.f[fd] = self.f[fs].sqrt(),
                    0x05 => self.f[fd] = self.f[fs].abs(),
                    0x06 => self.f[fd] = self.f[fs],
                    0x07 => self.f[fd] = -self.f[fs],
                    0x0C => self.f[fd] = f32::from_bits(self.f[fs].round() as i32 as u32),
                    0x0D => self.f[fd] = f32::from_bits(self.f[fs].trunc() as i32 as u32),
                    0x0E => self.f[fd] = f32::from_bits(self.f[fs].ceil() as i32 as u32),
                    0x0F => self.f[fd] = f32::from_bits(self.f[fs].floor() as i32 as u32),
                    0x24 => self.f[fd] = f32::from_bits(self.f[fs].round() as i32 as u32), // CVT.W.S
                    0x30..=0x3F => {
                        let a = self.f[fs];
                        let b = self.f[ft];
                        let unordered = a.is_nan() || b.is_nan();
                        let eq = !unordered && a == b;
                        let lt = !unordered && a < b;
                        let c = ((funct & 1 != 0) && unordered)
                            || ((funct & 2 != 0) && eq)
                            || ((funct & 4 != 0) && lt);
                        self.set_fcc(c);
                    }
                    _ => {}
                }
            }
            0x14 => {
                // Word (.W)
                if funct == 0x20 {
                    // CVT.S.W
                    self.f[fd] = (self.f[fs].to_bits() as i32) as f32;
                }
            }
            _ => {}
        }
    }
}
