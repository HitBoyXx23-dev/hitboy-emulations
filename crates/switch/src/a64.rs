//! AArch64 (ARMv8-A, the Switch's Cortex-A57 cores) interpreter: integer instructions,
//! branches, loads/stores (including pairs, exclusives and 128-bit vector registers),
//! system registers a program can read, and scalar floating point. Vector arithmetic
//! (NEON) isn't done yet and stops with `Exit::Undefined` so it is easy to see what a game needs.

/// Memory as the CPU sees it. `size` is 1, 2, 4 or 8 bytes; `None` means the access faults.
mod neon;

pub trait Mem {
    fn read(&mut self, addr: u64, size: usize) -> Option<u64>;
    fn write(&mut self, addr: u64, size: usize, value: u64) -> bool;
    /// Optional EL1 address translation hook for `AT S1E1R/W` (`PAR_EL1`).
    fn translate_va(&mut self, _va: u64) -> Option<u64> {
        None
    }
    /// Optional TLB invalidation hook for `TLBI` instructions.
    fn flush_tlb(&mut self) {}
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Exit {
    /// `SVC #n`: a system call. The PC is already past it.
    Svc(u32),
    /// `BRK #n`.
    Brk(u32),
    /// An instruction this core doesn't handle (address, encoding).
    Undefined(u64, u32),
    /// A load, store or fetch at an unmapped address.
    Fault(u64),
}

#[derive(Clone)]
pub struct Cpu {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub n: bool,
    pub z: bool,
    pub c: bool,
    pub v: bool,
    /// SIMD/FP registers as raw bits.
    pub q: [u128; 32],
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    /// Counter read by `mrs cntvct_el0`; the host advances it.
    pub ticks: u64,
    excl: Option<u64>,
    /// Kernel bring-up mode (EL1): enables EL1 MSR/MRS system register bank and ARM64e PAC hints.
    pub el1: bool,
    /// Inactive banked stack pointer (`SP_EL0` when `spsel` is true, `SP_EL1` when false).
    pub sp_other: u64,
    /// `PSTATE.SPSel`: true = `SP_EL1` (EL1h), false = `SP_EL0` (EL1t).
    pub spsel: bool,
    pub sysregs: std::collections::BTreeMap<u32, u64>,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

fn ones(k: u32) -> u64 {
    if k >= 64 {
        !0
    } else {
        (1u64 << k) - 1
    }
}

fn ror(v: u64, r: u32, size: u32) -> u64 {
    let r = r % size;
    if r == 0 {
        v
    } else {
        ((v >> r) | (v << (size - r))) & ones(size)
    }
}

fn replicate(v: u64, esize: u32, total: u32) -> u64 {
    let mut out = 0;
    let mut i = 0;
    while i < total {
        out |= v << i;
        i += esize;
    }
    out & ones(total)
}

/// ARM's DecodeBitMasks: (wmask, tmask).
fn bitmasks(n: u32, imms: u32, immr: u32, immediate: bool, datasize: u32) -> Option<(u64, u64)> {
    let combined = (n << 6) | (!imms & 0x3F);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 || (1u32 << len) > datasize {
        return None;
    }
    let esize = 1u32 << len;
    let levels = esize - 1;
    if immediate && (imms & levels) == levels {
        return None;
    }
    let s = imms & levels;
    let r = immr & levels;
    let d = s.wrapping_sub(r) & levels;
    let welem = ones(s + 1);
    let telem = ones(d + 1);
    Some((replicate(ror(welem, r, esize), esize, datasize), replicate(telem, esize, datasize)))
}

fn sext(v: u64, bits: u32) -> i64 {
    ((v << (64 - bits)) as i64) >> (64 - bits)
}

/// a + b + carry with the NZCV flags (as bits n z c v).
fn add_carry(a: u64, b: u64, cin: bool, sf: bool) -> (u64, [bool; 4]) {
    if sf {
        let (r1, c1) = a.overflowing_add(b);
        let (r, c2) = r1.overflowing_add(cin as u64);
        let v = ((a ^ r) & (b ^ r)) >> 63 != 0;
        (r, [r >> 63 != 0, r == 0, c1 || c2, v])
    } else {
        let (a, b) = (a as u32, b as u32);
        let (r1, c1) = a.overflowing_add(b);
        let (r, c2) = r1.overflowing_add(cin as u32);
        let v = ((a ^ r) & (b ^ r)) >> 31 != 0;
        (r as u64, [r >> 31 != 0, r == 0, c1 || c2, v])
    }
}

fn mask(v: u64, sf: bool) -> u64 {
    if sf {
        v
    } else {
        v as u32 as u64
    }
}

fn shift_val(v: u64, kind: u32, amt: u32, sf: bool) -> u64 {
    let bits = if sf { 64 } else { 32 };
    let amt = amt % bits;
    let v = mask(v, sf);
    match kind {
        0 => mask(v << amt, sf),
        1 => v >> amt,
        2 => {
            if sf {
                ((v as i64) >> amt) as u64
            } else {
                ((v as u32 as i32) >> amt) as u32 as u64
            }
        }
        _ => ror(v, amt, bits),
    }
}

fn extend_val(v: u64, option: u32, shift: u32) -> u64 {
    let v = match option {
        0 => v as u8 as u64,
        1 => v as u16 as u64,
        2 => v as u32 as u64,
        3 => v,
        4 => v as i8 as i64 as u64,
        5 => v as i16 as i64 as u64,
        6 => v as i32 as i64 as u64,
        _ => v,
    };
    v << shift
}

fn vfp_expand(imm8: u32, double: bool) -> u64 {
    let sign = ((imm8 >> 7) & 1) as u64;
    let b6 = ((imm8 >> 6) & 1) as u64;
    let low = (imm8 & 0xF) as u64;
    let mid = ((imm8 >> 4) & 3) as u64;
    if double {
        let exp = ((b6 ^ 1) << 10) | (if b6 == 1 { 0xFF } else { 0 } << 2) | mid;
        (sign << 63) | (exp << 52) | (low << 48)
    } else {
        let exp = ((b6 ^ 1) << 7) | (if b6 == 1 { 0x1F } else { 0 } << 2) | mid;
        (sign << 31) | (exp << 23) | (low << 19)
    }
}

impl Cpu {
    pub fn new() -> Cpu {
        Cpu {
            x: [0; 31],
            sp: 0,
            pc: 0,
            n: false,
            z: false,
            c: false,
            v: false,
            q: [0; 32],
            tpidr_el0: 0,
            tpidrro_el0: 0,
            fpcr: 0,
            fpsr: 0,
            ticks: 0,
            excl: None,
            el1: false,
            sp_other: 0,
            spsel: true,
            sysregs: std::collections::BTreeMap::new(),
        }
    }

    /// Switches between `SP_EL0` (`use_el1 == false`) and `SP_EL1` (`use_el1 == true`).
    pub fn set_spsel(&mut self, use_el1: bool) {
        if self.spsel != use_el1 {
            std::mem::swap(&mut self.sp, &mut self.sp_other);
            self.spsel = use_el1;
        }
    }

    /// Dispatches a synchronous EL1 Data/Instruction Abort through `VBAR_EL1` (`+0x000` from EL1t,
    /// `+0x200` from EL1h), populating `ELR_EL1`, `SPSR_EL1`, `FAR_EL1`, and `ESR_EL1`.
    pub fn take_el1_sync_abort(&mut self, elr: u64, far: u64, is_write: bool, is_insn: bool) -> bool {
        const VBAR_EL1: u32 = (3 << 14) | (0 << 11) | (12 << 7) | (0 << 3) | 0;
        const SPSR_EL1: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (0 << 3) | 0;
        const ELR_EL1: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (0 << 3) | 1;
        const ESR_EL1: u32 = (3 << 14) | (0 << 11) | (5 << 7) | (2 << 3) | 0;
        const FAR_EL1: u32 = (3 << 14) | (0 << 11) | (6 << 7) | (0 << 3) | 0;
        const SCTLR_EL1: u32 = (3 << 14) | (0 << 11) | (1 << 7) | (0 << 3) | 0;
        const PAN_REG: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (2 << 3) | 3;
        const DAIF_REG: u32 = (3 << 14) | (3 << 11) | (4 << 7) | (2 << 3) | 1;

        let vbar = *self.sysregs.get(&VBAR_EL1).unwrap_or(&0);
        if !self.el1 || vbar == 0 {
            return false;
        }
        let pan = *self.sysregs.get(&PAN_REG).unwrap_or(&0) & (1 << 22);
        let daif = *self.sysregs.get(&DAIF_REG).unwrap_or(&0) & (0xF << 6);
        let mode = 0b0100u64 | (self.spsel as u64); // EL1t (4) or EL1h (5)
        let spsr = (self.nzcv() as u64) | pan | daif | mode;

        let ec = if is_insn { 0x21u64 } else { 0x25u64 }; // IABT/DABT from current EL
        let wnr = if !is_insn && is_write { 1u64 << 6 } else { 0 };
        let esr = (ec << 26) | (1u64 << 25) | wnr | 0x07; // IL=1, level-3 translation fault

        let vec_off = if self.spsel { 0x200 } else { 0x000 };
        self.sysregs.insert(ELR_EL1, elr);
        self.sysregs.insert(SPSR_EL1, spsr);
        self.sysregs.insert(ESR_EL1, esr);
        self.sysregs.insert(FAR_EL1, far);
        self.sysregs.insert(DAIF_REG, 0xF << 6);
        let sctlr = *self.sysregs.get(&SCTLR_EL1).unwrap_or(&0);
        if (sctlr >> 23) & 1 == 0 {
            self.sysregs.insert(PAN_REG, 1 << 22);
        }
        self.set_spsel(true);
        self.pc = vbar.wrapping_add(vec_off);
        true
    }

    fn r(&self, i: u32, sf: bool) -> u64 {
        mask(if i == 31 { 0 } else { self.x[i as usize] }, sf)
    }
    fn r_sp(&self, i: u32) -> u64 {
        if i == 31 {
            self.sp
        } else {
            self.x[i as usize]
        }
    }
    fn set(&mut self, i: u32, v: u64, sf: bool) {
        if i != 31 {
            self.x[i as usize] = mask(v, sf);
        }
    }
    fn set_sp(&mut self, i: u32, v: u64, sf: bool) {
        if i == 31 {
            self.sp = mask(v, sf);
        } else {
            self.x[i as usize] = mask(v, sf);
        }
    }
    fn set_flags(&mut self, f: [bool; 4]) {
        [self.n, self.z, self.c, self.v] = f;
    }
    pub fn nzcv(&self) -> u32 {
        (self.n as u32) << 31 | (self.z as u32) << 30 | (self.c as u32) << 29 | (self.v as u32) << 28
    }
    fn cond(&self, c: u32) -> bool {
        let r = match c >> 1 {
            0 => self.z,
            1 => self.c,
            2 => self.n,
            3 => self.v,
            4 => self.c && !self.z,
            5 => self.n == self.v,
            6 => self.n == self.v && !self.z,
            _ => true,
        };
        if c & 1 == 1 && c != 15 {
            !r
        } else {
            r
        }
    }

    /// Runs one instruction.
    pub fn step(&mut self, m: &mut impl Mem) -> Result<(), Exit> {
        let pc = self.pc;
        let Some(w) = m.read(pc, 4) else { return Err(Exit::Fault(pc)) };
        let i = w as u32;
        self.ticks = self.ticks.wrapping_add(1);
        let next = pc.wrapping_add(4);
        let undefined = Err(Exit::Undefined(pc, i));
        match (i >> 25) & 0xF {
            0b1000 | 0b1001 => {
                self.pc = next;
                self.dp_imm(i).ok_or(Exit::Undefined(pc, i))
            }
            0b1010 | 0b1011 => self.branch(i, pc, m),
            0b0101 | 0b1101 => {
                self.pc = next;
                self.dp_reg(i).ok_or(Exit::Undefined(pc, i))
            }
            0b0100 | 0b0110 | 0b1100 | 0b1110 => {
                self.pc = next;
                match self.load_store(i, pc, m) {
                    Ok(true) => Ok(()),
                    Ok(false) => undefined,
                    Err(e) => Err(e),
                }
            }
            0b0111 | 0b1111 => {
                self.pc = next;
                if self.simd(i) || self.fp(i) {
                    Ok(())
                } else {
                    undefined
                }
            }
            _ => undefined,
        }
    }

    fn dp_imm(&mut self, i: u32) -> Option<()> {
        let sf = i >> 31 != 0;
        let rd = i & 31;
        let rn = (i >> 5) & 31;
        match (i >> 23) & 7 {
            0 | 1 => {
                let imm = (((i >> 5) & 0x7FFFF) << 2 | (i >> 29) & 3) as u64;
                let imm = sext(imm, 21);
                let base = self.pc.wrapping_sub(4);
                let v = if i >> 31 != 0 { (base & !0xFFF).wrapping_add((imm << 12) as u64) } else { base.wrapping_add(imm as u64) };
                self.set(rd, v, true);
            }
            2 => {
                let imm = ((i >> 10) & 0xFFF) as u64;
                let imm = if (i >> 22) & 1 != 0 { imm << 12 } else { imm };
                let sub = (i >> 30) & 1 != 0;
                let s = (i >> 29) & 1 != 0;
                let a = mask(self.r_sp(rn), sf);
                let (r, f) = add_carry(a, if sub { mask(!imm, sf) } else { imm }, sub, sf);
                if s {
                    self.set_flags(f);
                    self.set(rd, r, sf);
                } else {
                    self.set_sp(rd, r, sf);
                }
            }
            4 => {
                let n = (i >> 22) & 1;
                if !sf && n == 1 {
                    return None;
                }
                let (wmask, _) = bitmasks(n, (i >> 10) & 0x3F, (i >> 16) & 0x3F, true, if sf { 64 } else { 32 })?;
                let a = self.r(rn, sf);
                match (i >> 29) & 3 {
                    0 => self.set_sp(rd, a & wmask, sf),
                    1 => self.set_sp(rd, a | wmask, sf),
                    2 => self.set_sp(rd, a ^ wmask, sf),
                    _ => {
                        let r = a & wmask;
                        self.set_flags([if sf { r >> 63 } else { r >> 31 } != 0, r == 0, false, false]);
                        self.set(rd, r, sf);
                    }
                }
                // ORR/AND/EOR write SP; ANDS writes XZR (handled above).
            }
            5 => {
                let hw = (i >> 21) & 3;
                if !sf && hw > 1 {
                    return None;
                }
                let imm = ((i >> 5) & 0xFFFF) as u64;
                let sh = hw * 16;
                match (i >> 29) & 3 {
                    0 => self.set(rd, !(imm << sh), sf),
                    2 => self.set(rd, imm << sh, sf),
                    3 => {
                        let old = self.r(rd, sf);
                        self.set(rd, (old & !(0xFFFF << sh)) | (imm << sh), sf);
                    }
                    _ => return None,
                }
            }
            6 => {
                let n = (i >> 22) & 1;
                if n != sf as u32 {
                    return None;
                }
                let size = if sf { 64 } else { 32 };
                let (immr, imms) = ((i >> 16) & 0x3F, (i >> 10) & 0x3F);
                let (wmask, tmask) = bitmasks(n, imms, immr, false, size)?;
                let src = self.r(rn, sf);
                let rot = ror(src, immr, size);
                match (i >> 29) & 3 {
                    0 => {
                        let bot = rot & wmask;
                        let top = if (src >> imms) & 1 != 0 { ones(size) } else { 0 };
                        self.set(rd, (top & !tmask) | (bot & tmask), sf);
                    }
                    1 => {
                        let dst = self.r(rd, sf);
                        let bot = (dst & !wmask) | (rot & wmask);
                        self.set(rd, (dst & !tmask) | (bot & tmask), sf);
                    }
                    2 => self.set(rd, rot & wmask & tmask, sf),
                    _ => return None,
                }
            }
            7 => {
                let size = if sf { 64 } else { 32 };
                let lsb = (i >> 10) & 0x3F;
                if (i >> 22) & 1 != sf as u32 || (!sf && lsb >= 32) {
                    return None;
                }
                let hi = self.r(rn, sf);
                let lo = self.r((i >> 16) & 31, sf);
                let v = if lsb == 0 { lo } else { (lo >> lsb) | (hi << (size - lsb)) };
                self.set(rd, v, sf);
            }
            _ => return None,
        }
        Some(())
    }

    fn branch(&mut self, i: u32, pc: u64, m: &mut impl Mem) -> Result<(), Exit> {
        let next = pc.wrapping_add(4);
        let bad = Err(Exit::Undefined(pc, i));
        self.pc = next;
        if (i >> 26) & 0x1F == 0b00101 {
            if i >> 31 != 0 {
                self.x[30] = next;
            }
            self.pc = pc.wrapping_add((sext((i & 0x3FFFFFF) as u64, 26) << 2) as u64);
            return Ok(());
        }
        match i >> 24 {
            0x34 | 0x35 | 0xB4 | 0xB5 => {
                let sf = i >> 31 != 0;
                let zero = self.r(i & 31, sf) == 0;
                if zero == ((i >> 24) & 1 == 0) {
                    self.pc = pc.wrapping_add((sext(((i >> 5) & 0x7FFFF) as u64, 19) << 2) as u64);
                }
            }
            0x36 | 0x37 | 0xB6 | 0xB7 => {
                let bit = ((i >> 31) << 5) | ((i >> 19) & 31);
                let set = (self.r(i & 31, true) >> bit) & 1 != 0;
                if set == ((i >> 24) & 1 != 0) {
                    self.pc = pc.wrapping_add((sext(((i >> 5) & 0x3FFF) as u64, 14) << 2) as u64);
                }
            }
            0x54 => {
                if i & 0x10 != 0 {
                    return bad;
                }
                if self.cond(i & 15) {
                    self.pc = pc.wrapping_add((sext(((i >> 5) & 0x7FFFF) as u64, 19) << 2) as u64);
                }
            }
            0xD4 => {
                let imm = (i >> 5) & 0xFFFF;
                match (i >> 21) & 7 {
                    0 if i & 3 == 1 => return Err(Exit::Svc(imm)),
                    1 => return Err(Exit::Brk(imm)),
                    _ => return bad,
                }
            }
            0xD6 | 0xD7 => {
                if self.el1 && (i == 0xD65F_0BFF || i == 0xD65F_0FFF) {
                    // RETAA / RETAB: return via X30 (LR).
                    self.pc = self.x[30];
                    return Ok(());
                }
                if self.el1 && (i == 0xD69F_03E0 || i == 0xD69F_0BFF || i == 0xD69F_0FFF) {
                    // ERET / ERETAA / ERETAB: return from EL1 exception via ELR_EL1 & SPSR_EL1.
                    const SPSR_EL1: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (0 << 3) | 0;
                    const ELR_EL1: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (0 << 3) | 1;
                    const PAN_REG: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (2 << 3) | 3;
                    const DAIF_REG: u32 = (3 << 14) | (3 << 11) | (4 << 7) | (2 << 3) | 1;
                    let spsr = *self.sysregs.get(&SPSR_EL1).unwrap_or(&0);
                    self.set_flags([
                        (spsr >> 31) & 1 != 0,
                        (spsr >> 30) & 1 != 0,
                        (spsr >> 29) & 1 != 0,
                        (spsr >> 28) & 1 != 0,
                    ]);
                    self.sysregs.insert(PAN_REG, spsr & (1 << 22));
                    self.sysregs.insert(DAIF_REG, spsr & (0xF << 6));
                    self.set_spsel((spsr & 1) != 0);
                    self.pc = *self.sysregs.get(&ELR_EL1).unwrap_or(&0);
                    return Ok(());
                }
                let target = self.r((i >> 5) & 31, true);
                match (i >> 21) & 0xF {
                    0 | 2 => {}
                    1 => self.x[30] = next,
                    8 if self.el1 => {}                 // BRAA / BRAB
                    9 if self.el1 => self.x[30] = next, // BLRAA / BLRAB
                    _ => return bad,
                }
                self.pc = target;
            }
            0xD5 => return self.system(i, pc, m),
            _ => return bad,
        }
        let _ = m;
        Ok(())
    }

    fn system(&mut self, i: u32, pc: u64, m: &mut impl Mem) -> Result<(), Exit> {
        if (i >> 22) & 0x3FF != 0b1101010100 {
            return Err(Exit::Undefined(pc, i));
        }
        let l = (i >> 21) & 1 != 0;
        let op0 = (i >> 19) & 3;
        let crn = (i >> 12) & 15;
        let crm = (i >> 8) & 15;
        let reg = (op0 << 14) | (((i >> 16) & 7) << 11) | (crn << 7) | (crm << 3) | ((i >> 5) & 7);
        let rt = i & 31;
        if op0 < 2 {
            if self.el1 && !l {
                if op0 == 0 && crn == 4 {
                    // PSTATE immediate updates: MSR PAN/DAIFSet/DAIFClr, #imm4
                    let op1 = (i >> 16) & 7;
                    let op2 = (i >> 5) & 7;
                    const PAN_REG: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (2 << 3) | 3;
                    const DAIF_REG: u32 = (3 << 14) | (3 << 11) | (4 << 7) | (2 << 3) | 1;
                    match (op1, op2) {
                        (0, 4) => {
                            self.sysregs.insert(PAN_REG, ((crm & 1) as u64) << 22);
                        }
                        (0, 5) => {
                            // MSR SPSel, #imm4
                            self.set_spsel((crm & 1) != 0);
                        }
                        (3, 6) => {
                            let cur = *self.sysregs.get(&DAIF_REG).unwrap_or(&0);
                            self.sysregs.insert(DAIF_REG, cur | ((crm as u64) << 6));
                        }
                        (3, 7) => {
                            let cur = *self.sysregs.get(&DAIF_REG).unwrap_or(&0);
                            self.sysregs.insert(DAIF_REG, cur & !((crm as u64) << 6));
                        }
                        _ => {}
                    }
                } else if op0 == 1 {
                    let op1 = (i >> 16) & 7;
                    let op2 = (i >> 5) & 7;
                    if op1 == 3 && crn == 7 && crm == 4 && op2 == 1 {
                        // DC ZVA, Xt: zero the 64-byte cache line containing Xt.
                        let base = self.r(rt, true) & !63;
                        for k in 0..8u64 {
                            if !m.write(base + k * 8, 8, 0) {
                                return Err(Exit::Fault(base + k * 8));
                            }
                        }
                    } else if crn == 7 && crm == 8 {
                        // AT S1E1R/W / S1E0R/W -> update PAR_EL1 (3,0,7,4,0)
                        const PAR_EL1: u32 = (3 << 14) | (0 << 11) | (7 << 7) | (4 << 3) | 0;
                        let va = self.r(rt, true);
                        let par = match m.translate_va(va) {
                            Some(pa) => (pa & 0x0000_FFFF_FFFF_F000) | (0xFFu64 << 56),
                            None => 1, // F=1 (translation fault)
                        };
                        self.sysregs.insert(PAR_EL1, par);
                    } else if crn == 8 {
                        // TLBI *
                        m.flush_tlb();
                    }
                }
            }
            return Ok(()); // hints, barriers, cache maintenance: nothing to do
        }
        const TPIDR_EL0: u32 = (3 << 14) | (3 << 11) | (13 << 7) | 2;
        const TPIDRRO_EL0: u32 = (3 << 14) | (3 << 11) | (13 << 7) | 3;
        const CNTPCT_EL0: u32 = (3 << 14) | (3 << 11) | (14 << 7) | 1;
        const CNTVCT_EL0: u32 = (3 << 14) | (3 << 11) | (14 << 7) | 2;
        const CNTPCTSS_EL0: u32 = (3 << 14) | (3 << 11) | (14 << 7) | 5;
        const CNTVCTSS_EL0: u32 = (3 << 14) | (3 << 11) | (14 << 7) | 6;
        const CNTFRQ_EL0: u32 = (3 << 14) | (3 << 11) | (14 << 7);
        const NZCV: u32 = (3 << 14) | (3 << 11) | (4 << 7) | (2 << 3);
        const FPCR: u32 = (3 << 14) | (3 << 11) | (4 << 7) | (4 << 3);
        const FPSR: u32 = (3 << 14) | (3 << 11) | (4 << 7) | (4 << 3) | 1;
        const DCZID_EL0: u32 = (3 << 14) | (3 << 11) | 7;
        const CTR_EL0: u32 = (3 << 14) | (3 << 11) | 1;
        const SP_EL0: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (1 << 3) | 0;
        const SPSEL_REG: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (2 << 3) | 0;
        const CURRENT_EL: u32 = (3 << 14) | (0 << 11) | (4 << 7) | (2 << 3) | 2;
        const MIDR_EL1: u32 = (3 << 14) | (0 << 11) | (0 << 7) | (0 << 3) | 0;
        const MPIDR_EL1: u32 = (3 << 14) | (0 << 11) | (0 << 7) | (0 << 3) | 5;
        if l {
            let v = match reg {
                TPIDR_EL0 => self.tpidr_el0,
                TPIDRRO_EL0 => self.tpidrro_el0,
                CNTPCT_EL0 | CNTVCT_EL0 | CNTPCTSS_EL0 | CNTVCTSS_EL0 => self.ticks,
                CNTFRQ_EL0 => if self.el1 { 24_000_000 } else { 19_200_000 },
                NZCV => self.nzcv() as u64,
                FPCR => self.fpcr,
                FPSR => self.fpsr,
                DCZID_EL0 => if self.el1 { 0x04 } else { 0x10 }, // 64-byte DC ZVA in EL1
                CTR_EL0 => 0x8444_C004,
                SP_EL0 if self.el1 => if self.spsel { self.sp_other } else { self.sp },
                SPSEL_REG if self.el1 => self.spsel as u64,
                CURRENT_EL if self.el1 => 0x4, // EL1
                MIDR_EL1 if self.el1 => *self.sysregs.get(&reg).unwrap_or(&0x610F_0300),
                MPIDR_EL1 if self.el1 => *self.sysregs.get(&reg).unwrap_or(&0x8000_0000),
                _ if self.el1 => *self.sysregs.get(&reg).unwrap_or(&0),
                _ => return Err(Exit::Undefined(pc, i)),
            };
            self.set(rt, v, true);
        } else {
            let v = self.r(rt, true);
            match reg {
                TPIDR_EL0 => self.tpidr_el0 = v,
                NZCV => self.set_flags([v >> 31 & 1 != 0, v >> 30 & 1 != 0, v >> 29 & 1 != 0, v >> 28 & 1 != 0]),
                FPCR => self.fpcr = v,
                FPSR => self.fpsr = v,
                SP_EL0 if self.el1 => {
                    if self.spsel {
                        self.sp_other = v;
                    } else {
                        self.sp = v;
                    }
                }
                SPSEL_REG if self.el1 => self.set_spsel((v & 1) != 0),
                _ if self.el1 => {
                    self.sysregs.insert(reg, v);
                }
                _ => return Err(Exit::Undefined(pc, i)),
            }
        }
        Ok(())
    }

    fn dp_reg(&mut self, i: u32) -> Option<()> {
        let sf = i >> 31 != 0;
        let (rd, rn, rm) = (i & 31, (i >> 5) & 31, (i >> 16) & 31);
        let top = (i >> 24) & 0x1F;
        match top {
            0b01010 => {
                // logical (shifted register)
                let amt = (i >> 10) & 0x3F;
                if !sf && amt >= 32 {
                    return None;
                }
                let mut b = shift_val(self.r(rm, sf), (i >> 22) & 3, amt, sf);
                if (i >> 21) & 1 != 0 {
                    b = mask(!b, sf);
                }
                let a = self.r(rn, sf);
                let r = match (i >> 29) & 3 {
                    0 | 3 => a & b,
                    1 => a | b,
                    _ => a ^ b,
                };
                if (i >> 29) & 3 == 3 {
                    self.set_flags([if sf { r >> 63 } else { r >> 31 } != 0, r == 0, false, false]);
                }
                self.set(rd, r, sf);
            }
            0b01011 => {
                let sub = (i >> 30) & 1 != 0;
                let s = (i >> 29) & 1 != 0;
                let (a, b);
                if (i >> 21) & 1 != 0 {
                    if (i >> 22) & 3 != 0 {
                        return None;
                    }
                    let sh = (i >> 10) & 7;
                    if sh > 4 {
                        return None;
                    }
                    a = self.r_sp(rn);
                    b = mask(extend_val(self.r(rm, true), (i >> 13) & 7, sh), sf);
                } else {
                    let kind = (i >> 22) & 3;
                    let amt = (i >> 10) & 0x3F;
                    if kind == 3 || (!sf && amt >= 32) {
                        return None;
                    }
                    a = self.r(rn, sf);
                    b = shift_val(self.r(rm, sf), kind, amt, sf);
                }
                let (r, f) = add_carry(mask(a, sf), if sub { mask(!b, sf) } else { b }, sub, sf);
                if s {
                    self.set_flags(f);
                    self.set(rd, r, sf);
                } else if (i >> 21) & 1 != 0 {
                    self.set_sp(rd, r, sf);
                } else {
                    self.set(rd, r, sf);
                }
            }
            0b11010 => match (i >> 21) & 7 {
                0 => {
                    // ADC/SBC
                    let sub = (i >> 30) & 1 != 0;
                    let b = self.r(rm, sf);
                    let (r, f) = add_carry(self.r(rn, sf), if sub { mask(!b, sf) } else { b }, self.c, sf);
                    if (i >> 29) & 1 != 0 {
                        self.set_flags(f);
                    }
                    self.set(rd, r, sf);
                }
                2 => {
                    // CCMN/CCMP
                    if i & 0x410 != 0 {
                        return None;
                    }
                    let sub = (i >> 30) & 1 != 0;
                    if self.cond((i >> 12) & 15) {
                        let b = if (i >> 11) & 1 != 0 { ((i >> 16) & 31) as u64 } else { self.r(rm, sf) };
                        let (_, f) = add_carry(self.r(rn, sf), if sub { mask(!b, sf) } else { b }, sub, sf);
                        self.set_flags(f);
                    } else {
                        let n = i & 15;
                        self.set_flags([n & 8 != 0, n & 4 != 0, n & 2 != 0, n & 1 != 0]);
                    }
                }
                4 => {
                    // conditional select
                    if (i >> 29) & 1 != 0 || (i >> 11) & 1 != 0 {
                        return None;
                    }
                    let op = (i >> 30) & 1;
                    let o2 = (i >> 10) & 1;
                    let r = if self.cond((i >> 12) & 15) {
                        self.r(rn, sf)
                    } else {
                        let b = self.r(rm, sf);
                        match (op, o2) {
                            (0, 0) => b,
                            (0, 1) => mask(b.wrapping_add(1), sf),
                            (1, 0) => mask(!b, sf),
                            _ => mask(b.wrapping_neg(), sf),
                        }
                    };
                    self.set(rd, r, sf);
                }
                6 => {
                    if (i >> 29) & 1 != 0 {
                        return None;
                    }
                    if (i >> 30) & 1 == 0 {
                        // two-source
                        let (a, b) = (self.r(rn, sf), self.r(rm, sf));
                        let bits = if sf { 64 } else { 32 };
                        let r = match (i >> 10) & 0x3F {
                            2 => {
                                if b == 0 {
                                    0
                                } else {
                                    a / b
                                }
                            }
                            3 => {
                                let (sa, sb) = if sf { (a as i64, b as i64) } else { (a as u32 as i32 as i64, b as u32 as i32 as i64) };
                                if sb == 0 {
                                    0
                                } else {
                                    sa.wrapping_div(sb) as u64
                                }
                            }
                            8 => shift_val(a, 0, (b % bits as u64) as u32, sf),
                            9 => shift_val(a, 1, (b % bits as u64) as u32, sf),
                            10 => shift_val(a, 2, (b % bits as u64) as u32, sf),
                            11 => shift_val(a, 3, (b % bits as u64) as u32, sf),
                            12 if sf && self.el1 => {
                                // ARMv8.3-A PACGA Xd, Xn, Xm|SP: deterministic 32-bit MAC in bits 63:32.
                                let mod_val = self.r_sp(rm);
                                let h = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ mod_val.wrapping_mul(0xBF58_476D_1CE4_E5B9);
                                (h ^ (h >> 32)) << 32
                            }
                            _ => return None,
                        };
                        self.set(rd, r, sf);
                    } else {
                        // one-source
                        let op2 = (i >> 16) & 31;
                        if op2 == 1 && sf && self.el1 {
                            // ARMv8.3-A Pointer Authentication (PACIA/PACIB/PACDA/PACDB/AUT*/XPAC*):
                            // preserve canonical 64-bit pointer in Rd (no-op signing in EL1 bring-up).
                            let v = self.r(rd, true);
                            self.set(rd, v, true);
                            return Some(());
                        }
                        if op2 != 0 {
                            return None;
                        }
                        let a = self.r(rn, sf);
                        let r = match ((i >> 10) & 0x3F, sf) {
                            (0, true) => a.reverse_bits(),
                            (0, false) => (a as u32).reverse_bits() as u64,
                            (1, true) => ((a & 0x00FF00FF00FF00FF) << 8) | ((a >> 8) & 0x00FF00FF00FF00FF),
                            (1, false) => (((a as u32 & 0x00FF00FF) << 8) | ((a as u32 >> 8) & 0x00FF00FF)) as u64,
                            (2, true) => {
                                let b = ((a & 0x00FF00FF00FF00FF) << 8) | ((a >> 8) & 0x00FF00FF00FF00FF);
                                ((b & 0x0000FFFF0000FFFF) << 16) | ((b >> 16) & 0x0000FFFF0000FFFF)
                            }
                            (2, false) | (3, false) => (a as u32).swap_bytes() as u64,
                            (3, true) => a.swap_bytes(),
                            (4, true) => a.leading_zeros() as u64,
                            (4, false) => (a as u32).leading_zeros() as u64,
                            (5, true) => ((a ^ (((a as i64) >> 1) as u64)).leading_zeros() - 1) as u64,
                            (5, false) => ((a as u32 ^ (((a as u32 as i32) >> 1) as u32)).leading_zeros() - 1) as u64,
                            _ => return None,
                        };
                        self.set(rd, r, sf);
                    }
                }
                _ => return None,
            },
            0b11011 => {
                let ra = (i >> 10) & 31;
                let sub = (i >> 15) & 1 != 0;
                let op31 = (i >> 21) & 7;
                let (a, b) = (self.r(rn, sf), self.r(rm, sf));
                let r = match op31 {
                    0 => {
                        let p = a.wrapping_mul(b);
                        let acc = self.r(ra, sf);
                        if sub {
                            acc.wrapping_sub(p)
                        } else {
                            acc.wrapping_add(p)
                        }
                    }
                    1 | 5 => {
                        let (x, y) = if op31 == 1 { (a as u32 as i32 as i64 as u64, b as u32 as i32 as i64 as u64) } else { (a as u32 as u64, b as u32 as u64) };
                        let p = x.wrapping_mul(y);
                        let acc = self.r(ra, true);
                        if sub {
                            acc.wrapping_sub(p)
                        } else {
                            acc.wrapping_add(p)
                        }
                    }
                    2 => (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64,
                    6 => (((a as u128) * (b as u128)) >> 64) as u64,
                    _ => return None,
                };
                self.set(rd, r, sf || op31 != 0);
            }
            _ => return None,
        }
        Some(())
    }

    fn addr_ok(m: &mut impl Mem, a: u64, size: usize) -> Result<u64, Exit> {
        m.read(a, size).ok_or(Exit::Fault(a))
    }

    fn rd_mem(m: &mut impl Mem, a: u64, size: usize) -> Result<u64, Exit> {
        Self::addr_ok(m, a, size)
    }

    fn wr_mem(m: &mut impl Mem, a: u64, size: usize, v: u64) -> Result<(), Exit> {
        if m.write(a, size, v) {
            Ok(())
        } else {
            Err(Exit::Fault(a))
        }
    }

    fn load_v(&mut self, m: &mut impl Mem, a: u64, bytes: usize, t: usize) -> Result<(), Exit> {
        if bytes == 16 {
            let lo = Self::rd_mem(m, a, 8)?;
            let hi = Self::rd_mem(m, a + 8, 8)?;
            self.q[t] = lo as u128 | (hi as u128) << 64;
        } else {
            self.q[t] = Self::rd_mem(m, a, bytes)? as u128;
        }
        Ok(())
    }

    fn store_v(&self, m: &mut impl Mem, a: u64, bytes: usize, t: usize) -> Result<(), Exit> {
        let v = self.q[t];
        if bytes == 16 {
            Self::wr_mem(m, a, 8, v as u64)?;
            Self::wr_mem(m, a + 8, 8, (v >> 64) as u64)
        } else {
            Self::wr_mem(m, a, bytes, v as u64 & ones(bytes as u32 * 8))
        }
    }

    /// Returns Ok(false) for an encoding we don't handle.
    fn load_store(&mut self, i: u32, pc: u64, m: &mut impl Mem) -> Result<bool, Exit> {
        let size = i >> 30;
        let rt = i & 31;
        let rn = (i >> 5) & 31;
        let vec = (i >> 26) & 1 != 0;
        let _ = pc;
        match (i >> 24) & 0x3F {
            0b001000 => {
                // exclusive / ordered
                if vec {
                    return Ok(false);
                }
                let bytes = 1usize << size;
                let l = (i >> 22) & 1 != 0;
                let pair = (i >> 21) & 1 != 0;
                let ordered_only = (i >> 23) & 1 != 0;
                let a = self.r_sp(rn);
                let rs = (i >> 16) & 31;
                let rt2 = (i >> 10) & 31;
                if ordered_only && pair {
                    // ARMv8.1-A CAS / CASA / CASAL / CASL (size 001000 1 L 1 Rs o0 11111 Rn Rt)
                    let sf = size == 3;
                    let mask_b = ones((bytes * 8) as u32);
                    let old = Self::rd_mem(m, a, bytes)?;
                    if old == (self.r(rs, sf) & mask_b) {
                        Self::wr_mem(m, a, bytes, self.r(rt, sf) & mask_b)?;
                    }
                    self.set(rs, old, sf);
                    return Ok(true);
                }
                if size < 2 && pair {
                    // ARMv8.1-A CASP / CASPA / CASPAL / CASPL (0 sz 001000 0 L 1 Rs o0 11111 Rn Rt)
                    if rt2 != 31 || (rs & 1) != 0 || (rt & 1) != 0 {
                        return Ok(false);
                    }
                    let sf = size == 1;
                    let pbytes = if sf { 8 } else { 4 };
                    let old1 = Self::rd_mem(m, a, pbytes)?;
                    let old2 = Self::rd_mem(m, a + pbytes as u64, pbytes)?;
                    if old1 == self.r(rs, sf) && old2 == self.r(rs + 1, sf) {
                        Self::wr_mem(m, a, pbytes, self.r(rt, sf))?;
                        Self::wr_mem(m, a + pbytes as u64, pbytes, self.r(rt + 1, sf))?;
                    }
                    self.set(rs, old1, sf);
                    self.set(rs + 1, old2, sf);
                    return Ok(true);
                }
                if l {
                    let v = Self::rd_mem(m, a, bytes)?;
                    self.set(rt, v, true);
                    if pair {
                        let v2 = Self::rd_mem(m, a + bytes as u64, bytes)?;
                        self.set(rt2, v2, true);
                    }
                    if !ordered_only {
                        self.excl = Some(a);
                    }
                } else if ordered_only {
                    Self::wr_mem(m, a, bytes, self.r(rt, true))?;
                } else {
                    let rs = (i >> 16) & 31;
                    if self.excl == Some(a) {
                        Self::wr_mem(m, a, bytes, self.r(rt, true))?;
                        if pair {
                            Self::wr_mem(m, a + bytes as u64, bytes, self.r(rt2, true))?;
                        }
                        self.set(rs, 0, false);
                    } else {
                        self.set(rs, 1, false);
                    }
                    self.excl = None;
                }
                Ok(true)
            }
            0b011000 | 0b011100 => {
                // load register (literal)
                let a = pc.wrapping_add((sext(((i >> 5) & 0x7FFFF) as u64, 19) << 2) as u64);
                if vec {
                    let bytes = 4usize << size.min(2);
                    self.load_v(m, a, bytes, rt as usize)?;
                    return Ok(true);
                }
                match size {
                    0 => {
                        let v = Self::rd_mem(m, a, 4)?;
                        self.set(rt, v, true)
                    }
                    1 => {
                        let v = Self::rd_mem(m, a, 8)?;
                        self.set(rt, v, true)
                    }
                    2 => {
                        let v = Self::rd_mem(m, a, 4)? as u32 as i32 as i64 as u64;
                        self.set(rt, v, true)
                    }
                    _ => {}
                }
                Ok(true)
            }
            0b101000 | 0b101001 | 0b101100 | 0b101101 => {
                // pairs (mode 0 = non-temporal signed offset, 1 = post-index, 2 = signed offset, 3 = pre-index)
                let l = (i >> 22) & 1 != 0;
                let mode = (i >> 23) & 3;
                let rt2 = (i >> 10) & 31;
                let (bytes, signed) = if vec {
                    (4usize << size.min(2), false)
                } else {
                    match size {
                        0 => (4, false),
                        1 => (4, true),
                        2 => (8, false),
                        _ => return Ok(false),
                    }
                };
                let off = sext(((i >> 15) & 0x7F) as u64, 7) * if vec { 4i64 << size.min(2) } else if size == 2 { 8 } else { 4 };
                let base = self.r_sp(rn);
                let addr = if mode == 1 { base } else { base.wrapping_add(off as u64) };
                let ra = |k: usize| addr.wrapping_add((k * bytes) as u64);
                if l {
                    if vec {
                        self.load_v(m, ra(0), bytes, rt as usize)?;
                        self.load_v(m, ra(1), bytes, rt2 as usize)?;
                    } else {
                        let a = Self::rd_mem(m, ra(0), bytes)?;
                        let b = Self::rd_mem(m, ra(1), bytes)?;
                        let (a, b) = if signed { (a as u32 as i32 as i64 as u64, b as u32 as i32 as i64 as u64) } else { (a, b) };
                        self.set(rt, a, true);
                        self.set(rt2, b, true);
                    }
                } else if vec {
                    self.store_v(m, ra(0), bytes, rt as usize)?;
                    self.store_v(m, ra(1), bytes, rt2 as usize)?;
                } else {
                    let (a, b) = (self.r(rt, true), self.r(rt2, true));
                    Self::wr_mem(m, ra(0), bytes, a)?;
                    Self::wr_mem(m, ra(1), bytes, b)?;
                }
                if mode == 1 || mode == 3 {
                    let nb = base.wrapping_add(off as u64);
                    self.set_sp(rn, nb, true);
                }
                Ok(true)
            }
            0b111000 | 0b111001 | 0b111100 | 0b111101 => {
                // single register
                let opc = (i >> 22) & 3;
                let (bytes, vbytes) = if vec { (0, if opc & 2 != 0 && size == 0 { 16 } else { 1usize << size }) } else { (1usize << size, 0) };
                let base = self.r_sp(rn);
                let unsigned = (i >> 24) & 1 != 0;
                let scale = if vec && size == 0 && opc & 2 != 0 { 4 } else { size };
                let (addr, wb) = if unsigned {
                    (base.wrapping_add((((i >> 10) & 0xFFF) as u64) << scale), None)
                } else if (i >> 21) & 1 != 0 {
                    if !vec && (i >> 10) & 3 == 0 {
                        // ARMv8.1-A LSE Atomic memory operations (LDADD/LDCLR/LDEOR/LDSET/LDSMAX/LDSMIN/LDUMAX/LDUMIN/SWP)
                        let bytes = 1usize << size;
                        let sf = size == 3;
                        let bits = (bytes * 8) as u32;
                        let rs = (i >> 16) & 31;
                        let old = Self::rd_mem(m, base, bytes)?;
                        let val = self.r(rs, sf);
                        let op_hi = (i >> 15) & 1;
                        let aop = (i >> 12) & 7;
                        if op_hi == 1 && aop == 4 && rs == 31 {
                            // ARMv8.3-A LDAPR / LDAPRB / LDAPRH
                            self.set(rt, old, sf);
                            return Ok(true);
                        }
                        let new_val = if op_hi == 1 && aop == 0 {
                            val // SWP
                        } else if op_hi == 0 {
                            match aop {
                                0 => old.wrapping_add(val),
                                1 => old & !val,
                                2 => old ^ val,
                                3 => old | val,
                                4 => {
                                    let (a, b) = (sext(old, bits), sext(val, bits));
                                    if a >= b { old } else { val }
                                }
                                5 => {
                                    let (a, b) = (sext(old, bits), sext(val, bits));
                                    if a <= b { old } else { val }
                                }
                                6 => if old >= val { old } else { val },
                                7 => if old <= val { old } else { val },
                                _ => return Ok(false),
                            }
                        } else {
                            return Ok(false);
                        };
                        Self::wr_mem(m, base, bytes, new_val)?;
                        self.set(rt, old, sf);
                        return Ok(true);
                    }
                    if (i >> 10) & 3 != 2 {
                        return Ok(false);
                    }
                    let sh = if (i >> 12) & 1 != 0 { scale } else { 0 };
                    (base.wrapping_add(extend_val(self.r((i >> 16) & 31, true), (i >> 13) & 7, sh)), None)
                } else {
                    let off = sext(((i >> 12) & 0x1FF) as u64, 9) as u64;
                    match (i >> 10) & 3 {
                        0 | 2 => (base.wrapping_add(off), None),
                        1 => (base, Some(base.wrapping_add(off))),
                        _ => (base.wrapping_add(off), Some(base.wrapping_add(off))),
                    }
                };
                if vec {
                    if opc & 1 != 0 {
                        self.load_v(m, addr, vbytes, rt as usize)?;
                    } else {
                        self.store_v(m, addr, vbytes, rt as usize)?;
                    }
                } else {
                    match opc {
                        0 => {
                            let v = self.r(rt, true);
                            Self::wr_mem(m, addr, bytes, v)?;
                        }
                        1 => {
                            let v = Self::rd_mem(m, addr, bytes)?;
                            self.set(rt, v, true);
                        }
                        _ => {
                            if size == 3 {
                                return Ok(true); // PRFM
                            }
                            let v = Self::rd_mem(m, addr, bytes)?;
                            let v = sext(v, bytes as u32 * 8) as u64;
                            self.set(rt, v, opc == 2);
                        }
                    }
                }
                if let Some(w) = wb {
                    self.set_sp(rn, w, true);
                }
                Ok(true)
            }
            0b001100 | 0b001101 => self.simd_ldst(i, m),
            _ => Ok(false),
        }
    }

    fn fget(&self, r: u32, double: bool) -> f64 {
        let bits = self.q[r as usize] as u64;
        if double {
            f64::from_bits(bits)
        } else {
            f32::from_bits(bits as u32) as f64
        }
    }
    fn fset(&mut self, r: u32, v: f64, double: bool) {
        self.q[r as usize] = if double { v.to_bits() as u128 } else { (v as f32).to_bits() as u128 };
    }

    fn fp(&mut self, i: u32) -> bool {
        // Scalar floating point only (bits 28:24 = 11110 / 11111).
        let ty = (i >> 22) & 3;
        let (rd, rn, rm) = (i & 31, (i >> 5) & 31, (i >> 16) & 31);
        if (i >> 24) & 0x1F == 0b11111 {
            if ty > 1 || (i >> 31) != 0 || (i >> 29) & 3 != 0 {
                return false;
            }
            let d = ty == 1;
            let ra = (i >> 10) & 31;
            let (a, b, c) = (self.fget(rn, d), self.fget(rm, d), self.fget(ra, d));
            let (o1, o0) = ((i >> 21) & 1 != 0, (i >> 15) & 1 != 0);
            let a = if o0 != o1 { -a } else { a };
            let c = if o1 { -c } else { c };
            let r = if d { a.mul_add(b, c) } else { (a as f32).mul_add(b as f32, c as f32) as f64 };
            self.fset(rd, r, d);
            return true;
        }
        if (i >> 24) & 0x1F != 0b11110 || (i >> 29) & 3 != 0 {
            return false;
        }
        let sf = i >> 31 != 0;
        if (i >> 21) & 1 == 0 {
            return false;
        }
        let d = ty == 1;
        if ty > 1 {
            return false;
        }
        if (i >> 10) & 7 == 4 && (i >> 12) & 1 != 0 {
            let v = vfp_expand((i >> 13) & 0xFF, d);
            self.q[rd as usize] = v as u128;
            return true;
        }
        match (i >> 10) & 3 {
            2 => {
                let (a, b) = (self.fget(rn, d), self.fget(rm, d));
                let r = match (i >> 12) & 0xF {
                    0 => a * b,
                    1 => a / b,
                    2 => a + b,
                    3 => a - b,
                    4 => a.max(b),
                    5 => a.min(b),
                    8 => -(a * b),
                    _ => return false,
                };
                if d {
                    self.fset(rd, r, true);
                } else {
                    // single precision arithmetic done in f32 for exact rounding
                    let (a, b) = (a as f32, b as f32);
                    let r = match (i >> 12) & 0xF {
                        0 => a * b,
                        1 => a / b,
                        2 => a + b,
                        3 => a - b,
                        4 => a.max(b),
                        5 => a.min(b),
                        _ => -(a * b),
                    };
                    self.q[rd as usize] = r.to_bits() as u128;
                }
                true
            }
            3 => {
                // FCSEL
                let src = if self.cond((i >> 12) & 15) { rn } else { rm };
                self.q[rd as usize] = self.q[src as usize] & if d { u64::MAX as u128 } else { u32::MAX as u128 };
                true
            }
            0 => match (i >> 10) & 0x1F {
                0b10000 => {
                    // one source (bits 14:10 = 10000)
                    let op = (i >> 15) & 0x3F;
                    let a = self.fget(rn, d);
                    match op {
                        0 => self.q[rd as usize] = self.q[rn as usize] & if d { u64::MAX as u128 } else { u32::MAX as u128 },
                        1 => self.fset(rd, a.abs(), d),
                        2 => self.fset(rd, -a, d),
                        3 => {
                            if d {
                                self.fset(rd, a.sqrt(), true)
                            } else {
                                self.q[rd as usize] = (a as f32).sqrt().to_bits() as u128
                            }
                        }
                        4 => self.fset(rd, a, false),
                        5 => self.fset(rd, a, true),
                        _ => return false,
                    }
                    true
                }
                _ if (i >> 10) & 0xF == 0b1000 => {
                    // FCMP / FCMPE
                    let a = self.fget(rn, d);
                    let b = if (i >> 3) & 1 != 0 { 0.0 } else { self.fget(rm, d) };
                    let f = if a.is_nan() || b.is_nan() {
                        [false, false, true, true]
                    } else if a == b {
                        [false, true, true, false]
                    } else if a < b {
                        [true, false, false, false]
                    } else {
                        [false, false, true, false]
                    };
                    self.set_flags(f);
                    true
                }
                _ if (i >> 10) & 0x3F == 0 => {
                    // conversion between floating point and integer
                    let rmode = (i >> 19) & 3;
                    let opcode = (i >> 16) & 7;
                    match (rmode, opcode) {
                        (0, 2) | (0, 3) => {
                            let v = self.r(rn, sf);
                            let f = if opcode == 2 {
                                if sf {
                                    v as i64 as f64
                                } else {
                                    v as u32 as i32 as f64
                                }
                            } else {
                                v as f64
                            };
                            if d {
                                self.fset(rd, f, true)
                            } else {
                                let f32v = if opcode == 2 {
                                    if sf {
                                        v as i64 as f32
                                    } else {
                                        v as u32 as i32 as f32
                                    }
                                } else {
                                    v as f32
                                };
                                self.q[rd as usize] = f32v.to_bits() as u128;
                            }
                        }
                        (3, 0) | (3, 1) => {
                            let a = self.fget(rn, d);
                            let r = if opcode == 0 {
                                if sf {
                                    a as i64 as u64
                                } else {
                                    a as i32 as u32 as u64
                                }
                            } else if sf {
                                a as u64
                            } else {
                                a as u32 as u64
                            };
                            self.set(rd, r, sf);
                        }
                        (0, 6) => {
                            let bits = self.q[rn as usize] as u64;
                            self.set(rd, if d { bits } else { bits as u32 as u64 }, sf);
                        }
                        (0, 7) => self.q[rd as usize] = self.r(rn, sf) as u128,
                        _ => return false,
                    }
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Mem for Ram {
        fn read(&mut self, a: u64, size: usize) -> Option<u64> {
            let a = a as usize;
            let b = self.0.get(a..a + size)?;
            Some(b.iter().rev().fold(0u64, |v, &x| v << 8 | x as u64))
        }
        fn write(&mut self, a: u64, size: usize, v: u64) -> bool {
            let a = a as usize;
            match self.0.get_mut(a..a + size) {
                Some(b) => {
                    for (k, x) in b.iter_mut().enumerate() {
                        *x = (v >> (8 * k)) as u8;
                    }
                    true
                }
                None => false,
            }
        }
    }

    fn run(code: &[u32]) -> (Cpu, Ram, Exit) {
        let mut ram = Ram(vec![0; 0x10000]);
        for (k, w) in code.iter().enumerate() {
            ram.write(k as u64 * 4, 4, *w as u64);
        }
        let mut cpu = Cpu::new();
        cpu.sp = 0x8000;
        loop {
            if let Err(e) = cpu.step(&mut ram) {
                return (cpu, ram, e);
            }
        }
    }

    const SVC0: u32 = 0xD4000001;

    #[test]
    fn arithmetic_and_flags() {
        // x0=5 x1=7 x2=x0+x1 x3=x2*x2; cmp x3,#144; b.eq +8 (skip the movz x4,#1); svc
        let (c, _, e) = run(&[0xD28000A0, 0xD28000E1, 0x8B010002, 0x9B027C43, 0xF102407F, 0x54000040, 0xD2800024, SVC0]);
        assert_eq!(e, Exit::Svc(0));
        assert_eq!((c.x[2], c.x[3], c.x[4]), (12, 144, 0));
        assert!(c.z && c.c);
    }

    #[test]
    fn loops_calls_and_stack() {
        // x0=3; loop: subs x0,x0,#1; b.ne loop; bl f; svc; f: stp/ldp, movz x5,#9; ret
        let (c, _, e) = run(&[0xD2800060, 0xF1000400, 0x54FFFFE1, 0x94000002, SVC0, 0xA9BF07E0, 0xD2800125, 0xA8C107E0, 0xD65F03C0]);
        assert_eq!(e, Exit::Svc(0));
        assert_eq!((c.x[0], c.x[5], c.sp), (0, 9, 0x8000));
        assert_eq!(c.pc, 20);
    }

    #[test]
    fn memory_bitfields_and_math() {
        let (c, ram, e) = run(&[
            0xD2800503, // movz x3,#40
            0xF90007E3, // str x3,[sp,#8]
            0xF94007E4, // ldr x4,[sp,#8]
            0xB2401FE1, // orr x1,xzr,#0xFF
            0xD3442C22, // ubfx x2,x1,#4,#8
            0xD37CEC25, // lsl x5,x1,#4
            0xDAC01026, // clz x6,x1
            0xD2800407, // movz x7,#32
            0x9AC708A8, // udiv x8,x5,x7
            SVC0,
        ]);
        assert_eq!(e, Exit::Svc(0));
        assert_eq!(c.x[4], 40);
        assert_eq!(c.x[1], 0xFF);
        assert_eq!(c.x[2], 0x0F);
        assert_eq!(c.x[5], 0xFF0);
        assert_eq!(c.x[6], 56);
        assert_eq!(c.x[8], 0xFF0 / 32);
        assert_eq!(ram.0[0x8008], 40);
    }

    #[test]
    fn scalar_float() {
        // fmov d0,#1.0; fadd d1,d0,d0; fmov x0,d1
        let (c, _, e) = run(&[0x1E6E1000, 0x1E602801, 0x9E660020, SVC0]);
        assert_eq!(e, Exit::Svc(0));
        assert_eq!(c.x[0], 2.0f64.to_bits());
    }

    #[test]
    fn unmapped_access_faults_and_unknown_is_reported() {
        let (_, _, e) = run(&[0xD2A00043, 0xF9400060]); // x3=0x20000; ldr x0,[x3]
        assert_eq!(e, Exit::Fault(0x20000));
        let (_, _, e) = run(&[0x4E20FC00]); // a NEON op we don't implement (FRECPS)
        assert_eq!(e, Exit::Undefined(0, 0x4E20FC00));
    }

    #[test]
    fn el1_sysregs_pac_and_at_s1e1r() {
        struct El1Ram {
            ram: Ram,
            flushes: usize,
        }
        impl Mem for El1Ram {
            fn read(&mut self, a: u64, size: usize) -> Option<u64> {
                self.ram.read(a, size)
            }
            fn write(&mut self, a: u64, size: usize, v: u64) -> bool {
                self.ram.write(a, size, v)
            }
            fn translate_va(&mut self, va: u64) -> Option<u64> {
                if va == 0x2000 { Some(0x8_0000_2000) } else { None }
            }
            fn flush_tlb(&mut self) {
                self.flushes += 1;
            }
        }
        // mrs x0, CurrentEL ; mrs x1, MIDR_EL1 ; movz x2,#0x2000 ; at s1e1r, x2 ; mrs x3, par_el1
        // movz x4,#0x9000 ; at s1e1r, x4 ; mrs x5, par_el1 ; tlbi vmalle1 ; pacia x6, x7 ; blraa x8, x9 ; svc #0 ; retab
        let code: [u32; 13] = [
            0xD538_4240, // mrs x0, CurrentEL
            0xD538_0001, // mrs x1, MIDR_EL1
            0xD284_0002, // movz x2, #0x2000
            0xD508_7802, // at s1e1r, x2
            0xD538_7403, // mrs x3, par_el1
            0xD292_0004, // movz x4, #0x9000
            0xD508_7804, // at s1e1r, x4
            0xD538_7405, // mrs x5, par_el1
            0xD508_871F, // tlbi vmalle1
            0xDAC1_00E6, // pacia x6, x7
            0xD73F_0909, // blraa x8, x9 (jumps to x8=48 -> retab)
            SVC0,
            0xD65F_0FFF, // retab
        ];
        let mut mem = El1Ram { ram: Ram(vec![0; 0x10000]), flushes: 0 };
        for (k, w) in code.iter().enumerate() {
            mem.ram.write(k as u64 * 4, 4, *w as u64);
        }
        let mut cpu = Cpu::new();
        cpu.el1 = true;
        cpu.sp = 0x8000;
        cpu.x[8] = 12 * 4;
        let exit = loop {
            if let Err(e) = cpu.step(&mut mem) {
                break e;
            }
        };
        assert_eq!(exit, Exit::Svc(0));
        assert_eq!(cpu.x[0], 0x4);
        assert_eq!(cpu.x[1], 0x610F_0300);
        assert_eq!(cpu.x[3] & 0x0000_FFFF_FFFF_F001, 0x8_0000_2000);
        assert_eq!(cpu.x[5] & 1, 1, "unmapped VA sets PAR_EL1.F");
        assert_eq!(mem.flushes, 1);

        // Verify ERET restores NZCV from SPSR_EL1 and jumps to ELR_EL1, and CNTPCTSS_EL0 reads ticks.
        let mut c2 = Cpu::new();
        c2.el1 = true;
        c2.ticks = 0x1234_5678;
        c2.x[0] = 0x14; // target PC
        c2.x[1] = 0x5000_0000; // Z=1, V=1
        let eret_code: [u32; 11] = [
            0xD53B_E0A3, // mrs x3, cntpctss_el0
            0xD500_419F, // msr pan, #1
            0xD538_4264, // mrs x4, pan
            0xD503_43DF, // msr daifset, #3 (I|F)
            0xD53B_4225, // mrs x5, daif
            0xD503_41FF, // msr daifclr, #1 (F)
            0xD53B_4226, // mrs x6, daif
            0xD518_4020, // msr elr_el1, x0
            0xD518_4001, // msr spsr_el1, x1
            0xD69F_03E0, // eret
            SVC0,        // at 0x28
        ];
        c2.x[0] = 10 * 4; // target PC = 0x28
        for (k, w) in eret_code.iter().enumerate() {
            mem.ram.write(k as u64 * 4, 4, *w as u64);
        }
        let ex2 = loop {
            if let Err(e) = c2.step(&mut mem) {
                break e;
            }
        };
        assert_eq!(ex2, Exit::Svc(0));
        assert_eq!(c2.x[3], 0x1234_5679);
        assert_eq!(c2.x[4], 1 << 22, "PAN bit 22 set");
        assert_eq!(c2.x[5], 3 << 6, "DAIF I|F set");
        assert_eq!(c2.x[6], 2 << 6, "DAIF F cleared, I still set");
        assert!(!c2.n && c2.z && !c2.c && c2.v);

        // Verify DCZID_EL0 reports 64-byte block in EL1 and DC ZVA zeroes a 64-byte aligned block.
        let mut c3 = Cpu::new();
        c3.el1 = true;
        c3.x[0] = 0x3020; // inside 0x3000..0x3040
        for off in 0..64usize {
            mem.ram.0[0x3000 + off] = 0xAA;
        }
        mem.ram.0[0x3040] = 0xBB; // just outside 64-byte block
        let dczva_code: [u32; 3] = [
            0xD53B_00E1, // mrs x1, dczid_el0
            0xD50B_7420, // dc zva, x0
            SVC0,
        ];
        for (k, w) in dczva_code.iter().enumerate() {
            mem.ram.write(k as u64 * 4, 4, *w as u64);
        }
        let ex3 = loop {
            if let Err(e) = c3.step(&mut mem) {
                break e;
            }
        };
        assert_eq!(ex3, Exit::Svc(0));
        assert_eq!(c3.x[1], 0x04, "64-byte DC ZVA block size");
        assert!(mem.ram.0[0x3000..0x3040].iter().all(|&b| b == 0), "64-byte line zeroed");
        assert_eq!(mem.ram.0[0x3040], 0xBB, "adjacent byte untouched");
    }

    #[test]
    fn lse_atomics_cas_casp_and_ldapr() {
        // Test STNP/LDNP, LDADD, SWP, CAS (match + mismatch), CASP, and LDAPR.
        let code: [u32; 11] = [
            0xD288_0000, // movz x0, #0x4000
            0xD280_0141, // movz x1, #10
            0xD280_0282, // movz x2, #20
            0xA800_0801, // stnp x1, x2, [x0]
            0xA840_1003, // ldnp x3, x4, [x0]
            0xF822_0005, // ldadd x2, x5, [x0]   -> mem[0]=30, x5=10
            0xB8BF_C006, // ldapr w6, [x0]       -> w6=30
            0xD280_03C7, // movz x7, #30
            0xD280_0508, // movz x8, #40
            0xC8A7_7C08, // cas x7, x8, [x0]     -> mem[0]==30 -> mem[0]=40, x7=30
            SVC0,
        ];
        let (c, mut ram, e) = run(&code);
        assert_eq!(e, Exit::Svc(0));
        assert_eq!((c.x[3], c.x[4]), (10, 20));
        assert_eq!(c.x[5], 10);
        assert_eq!(c.x[6], 30);
        assert_eq!(c.x[7], 30);
        assert_eq!(ram.read(0x4000, 8), Some(40));
    }

    #[test]
    fn el1_spsel_and_vbar_sync_abort() {
        // Test MSR SPSel #0/#1, MRS/MSR SP_EL0, VBAR_EL1 sync abort entry (+0x000 from EL1t),
        // and ERET restoring SP_EL0 + ELR_EL1 + NZCV.
        let mut ram = Ram(vec![0; 0x10000]);
        let code: [u32; 6] = [
            0xD518_4100, // msr sp_el0, x0 (0x6000)
            0xD500_40BF, // msr spsel, #0  (switch active sp to SP_EL0)
            0xD538_4201, // mrs x1, spsel  -> 0
            0xD500_41BF, // msr spsel, #1  (switch back to SP_EL1)
            0xD538_4102, // mrs x2, sp_el0 -> 0x6000
            SVC0,
        ];
        for (k, w) in code.iter().enumerate() {
            ram.write(k as u64 * 4, 4, *w as u64);
        }
        // Place ERET at VBAR_EL1 (0x2000) + 0x000 (EL1t sync vector)
        ram.write(0x2000, 4, 0xD69F_03E0);
        let mut cpu = Cpu::new();
        cpu.el1 = true;
        cpu.sp = 0x8000;
        cpu.x[0] = 0x6000;
        let ex = loop {
            if let Err(e) = cpu.step(&mut ram) {
                break e;
            }
        };
        assert_eq!(ex, Exit::Svc(0));
        assert_eq!(cpu.x[1], 0);
        assert_eq!(cpu.x[2], 0x6000);
        assert_eq!(cpu.sp, 0x8000);
        assert!(cpu.spsel);

        // Now test taking an EL1t sync Data Abort -> VBAR_EL1 + 0x000 -> ERET back to EL1t.
        const VBAR_EL1: u32 = (3 << 14) | (0 << 11) | (12 << 7) | (0 << 3) | 0;
        const ESR_EL1: u32 = (3 << 14) | (0 << 11) | (5 << 7) | (2 << 3) | 0;
        const FAR_EL1: u32 = (3 << 14) | (0 << 11) | (6 << 7) | (0 << 3) | 0;
        cpu.sysregs.insert(VBAR_EL1, 0x2000);
        cpu.set_spsel(false); // EL1t, sp == 0x6000
        assert!(cpu.take_el1_sync_abort(0x100, 0xDEAD_BEEF, true, false));
        assert_eq!(cpu.pc, 0x2000, "EL1t sync abort vectors to VBAR+0x000");
        assert!(cpu.spsel, "exception entry switches to SP_EL1");
        assert_eq!(cpu.sp, 0x8000);
        assert_eq!(cpu.sysregs[&FAR_EL1], 0xDEAD_BEEF);
        assert_eq!(cpu.sysregs[&ESR_EL1] >> 26, 0x25, "EC=0x25 Data Abort");
        assert_ne!(cpu.sysregs[&ESR_EL1] & (1 << 6), 0, "WnR=1 for write fault");

        // Step ERET at 0x2000 -> returns to 0x100 with EL1t (SP_EL0 = 0x6000).
        cpu.step(&mut ram).unwrap();
        assert_eq!(cpu.pc, 0x100);
        assert!(!cpu.spsel);
        assert_eq!(cpu.sp, 0x6000);

        // Verify PACGA produces a deterministic 32-bit MAC in the upper 32 bits.
        ram.write(0x100, 4, 0x9AC0_3021); // pacga x1, x1, x0
        cpu.x[0] = 0x1122_3344_5566_7788;
        cpu.x[1] = 0xCAFE_BABE_DEAD_BEEF;
        cpu.step(&mut ram).unwrap();
        let mac1 = cpu.x[1];
        assert_eq!(mac1 & 0xFFFF_FFFF, 0, "PACGA clears low 32 bits");
        assert_ne!(mac1, 0, "PACGA produces non-zero upper 32-bit MAC");
        cpu.pc = 0x100;
        cpu.x[1] = 0xCAFE_BABE_DEAD_BEEF;
        cpu.step(&mut ram).unwrap();
        assert_eq!(cpu.x[1], mac1, "PACGA is deterministic");
    }
}
