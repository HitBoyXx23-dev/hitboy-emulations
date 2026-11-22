//! The PlayStation 2's main processor, the "Emotion Engine" core (MIPS R5900).
//!
//! A 64-bit MIPS with 32 general registers that are 128 bits wide (the upper half is only used
//! by the multimedia instructions and 128-bit loads and stores), a second multiply unit
//! (HI1/LO1), a single-precision floating-point unit with PlayStation 2 rounding quirks,
//! a 48-entry TLB, and a 16 KB scratchpad that appears at 0x70000000.
//!
//! This crate is the processor only. The machine around it (memory map, interrupts, DMA,
//! graphics) supplies a [`Bus`].

mod fpu;
mod mmi;
mod ops;

pub const SCRATCHPAD_BASE: u32 = 0x7000_0000;
pub const SCRATCHPAD_SIZE: u32 = 0x4000;

/// Where the processor reads and writes. Addresses are physical (already translated) except
/// the scratchpad, which is passed through as `0x7000_0000 + offset`.
pub trait Bus {
    fn read(&mut self, addr: u32, size: u32) -> u64;
    fn write(&mut self, addr: u32, value: u64, size: u32);
    /// Called for the VU0 "macro mode" coprocessor-2 instructions; the default ignores them.
    fn cop2(&mut self, _op: u32, _cpu: &mut Cpu) {}
}

/// Exception codes (Cause.ExcCode).
pub mod exc {
    pub const INT: u32 = 0;
    pub const MOD: u32 = 1;
    pub const TLBL: u32 = 2;
    pub const TLBS: u32 = 3;
    pub const ADEL: u32 = 4;
    pub const ADES: u32 = 5;
    pub const SYS: u32 = 8;
    pub const BP: u32 = 9;
    pub const RI: u32 = 10;
    pub const CPU: u32 = 11;
    pub const OV: u32 = 12;
    pub const TR: u32 = 13;
}

// COP0 register numbers.
pub const INDEX: usize = 0;
pub const RANDOM: usize = 1;
pub const ENTRYLO0: usize = 2;
pub const ENTRYLO1: usize = 3;
pub const CONTEXT: usize = 4;
pub const PAGEMASK: usize = 5;
pub const WIRED: usize = 6;
pub const BADVADDR: usize = 8;
pub const COUNT: usize = 9;
pub const ENTRYHI: usize = 10;
pub const COMPARE: usize = 11;
pub const STATUS: usize = 12;
pub const CAUSE: usize = 13;
pub const EPC: usize = 14;
pub const PRID: usize = 15;
pub const CONFIG: usize = 16;
pub const ERROREPC: usize = 30;

pub const ST_IE: u32 = 1;
pub const ST_EXL: u32 = 2;
pub const ST_ERL: u32 = 4;
pub const ST_EIE: u32 = 1 << 16;
pub const ST_BEV: u32 = 1 << 22;

const TLB_ENTRIES: usize = 48;

#[derive(Clone, Copy, Default)]
pub struct TlbEntry {
    pub mask: u32,
    pub hi: u32,
    pub lo0: u32,
    pub lo1: u32,
}

impl TlbEntry {
    fn global(&self) -> bool {
        self.lo0 & self.lo1 & 1 != 0
    }
    fn span(&self) -> u32 {
        self.mask | 0x1FFF
    }
}

pub struct Cpu {
    /// Low and high 64 bits of each general register.
    pub lo: [u64; 32],
    pub hi_half: [u64; 32],
    pub pc: u32,
    pub next_pc: u32,
    pub hi: u64,
    pub lo_reg: u64,
    pub hi1: u64,
    pub lo1: u64,
    pub sa: u32,
    pub cop0: [u32; 32],
    pub fpr: [u32; 32],
    pub acc: u32,
    pub fcr31: u32,
    pub tlb: [TlbEntry; TLB_ENTRIES],
    /// The instruction just executed was a branch: the next one is its delay slot.
    in_delay: bool,
    /// The instruction being executed is in a delay slot.
    cur_delay: bool,
    cur_pc: u32,
    /// External interrupt lines: bit 0 = INTC (IP2), bit 1 = DMAC (IP3).
    pub irq: u32,
    pub cycles: u64,
    /// Instructions we don't implement yet (counted, then treated as reserved).
    pub unimplemented: u64,
    exception_taken: bool,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a memory access that may fault (the exception has already been raised).
pub(crate) type Mem<T> = Result<T, ()>;

impl Cpu {
    pub fn new() -> Cpu {
        let mut c = Cpu {
            lo: [0; 32],
            hi_half: [0; 32],
            pc: 0xBFC0_0000,
            next_pc: 0xBFC0_0004,
            hi: 0,
            lo_reg: 0,
            hi1: 0,
            lo1: 0,
            sa: 0,
            cop0: [0; 32],
            fpr: [0; 32],
            acc: 0,
            fcr31: 0,
            tlb: [TlbEntry::default(); TLB_ENTRIES],
            in_delay: false,
            cur_delay: false,
            cur_pc: 0,
            irq: 0,
            cycles: 0,
            unimplemented: 0,
            exception_taken: false,
        };
        c.cop0[STATUS] = 0x7040_0004; // BEV, ERL, coprocessors usable at reset
        c.cop0[PRID] = 0x0000_2E20; // R5900
        c.cop0[CONFIG] = 0x0000_0440;
        c.cop0[RANDOM] = (TLB_ENTRIES - 1) as u32;
        c
    }

    pub fn reset(&mut self) {
        *self = Cpu::new();
    }

    pub fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
        self.in_delay = false;
    }

    #[inline]
    pub fn reg(&self, i: usize) -> u64 {
        self.lo[i]
    }

    #[inline]
    pub fn set_reg(&mut self, i: usize, v: u64) {
        if i != 0 {
            self.lo[i] = v;
        }
    }

    // ------------------------------------------------------------ addresses

    /// Virtual -> physical. On failure the TLB exception has been raised.
    pub(crate) fn translate(&mut self, vaddr: u32, write: bool) -> Mem<u32> {
        match vaddr {
            0x8000_0000..=0x9FFF_FFFF | 0xA000_0000..=0xBFFF_FFFF => return Ok(vaddr & 0x1FFF_FFFF),
            SCRATCHPAD_BASE..=0x7000_3FFF => return Ok(vaddr),
            _ => {}
        }
        let asid = self.cop0[ENTRYHI] & 0xFF;
        for e in self.tlb.iter() {
            let span = e.span();
            if (vaddr & !span) != (e.hi & !span & 0xFFFF_E000) {
                continue;
            }
            if !e.global() && (e.hi & 0xFF) != asid {
                continue;
            }
            let odd_bit = (span + 1) >> 1;
            let lo = if vaddr & odd_bit != 0 { e.lo1 } else { e.lo0 };
            if lo & 2 == 0 {
                self.tlb_exception(vaddr, write, true);
                return Err(());
            }
            if write && lo & 4 == 0 {
                self.raise(exc::MOD, vaddr, false);
                return Err(());
            }
            return Ok(((lo >> 6) << 12) | (vaddr & (odd_bit - 1)));
        }
        self.tlb_exception(vaddr, write, false);
        Err(())
    }

    fn tlb_exception(&mut self, vaddr: u32, write: bool, invalid: bool) {
        self.cop0[CONTEXT] = (self.cop0[CONTEXT] & 0xFF80_0000) | ((vaddr >> 13) << 4);
        self.cop0[ENTRYHI] = (self.cop0[ENTRYHI] & 0xFF) | (vaddr & 0xFFFF_E000);
        self.raise(if write { exc::TLBS } else { exc::TLBL }, vaddr, !invalid);
    }

    // ------------------------------------------------------------ exceptions

    /// Takes an exception. `refill` selects the TLB-refill vector.
    pub(crate) fn raise(&mut self, code: u32, bad: u32, refill: bool) {
        self.exception_taken = true;
        if matches!(code, exc::TLBL | exc::TLBS | exc::MOD | exc::ADEL | exc::ADES) {
            self.cop0[BADVADDR] = bad;
        }
        let status = self.cop0[STATUS];
        let was_exl = status & ST_EXL != 0;
        if !was_exl {
            self.cop0[EPC] = if self.cur_delay { self.cur_pc.wrapping_sub(4) } else { self.cur_pc };
            if self.cur_delay {
                self.cop0[CAUSE] |= 1 << 31;
            } else {
                self.cop0[CAUSE] &= !(1 << 31);
            }
        }
        self.cop0[CAUSE] = (self.cop0[CAUSE] & !0x7C) | (code << 2);
        self.cop0[STATUS] |= ST_EXL;
        let bev = status & ST_BEV != 0;
        let vec = match (refill && !was_exl, bev, code == exc::INT) {
            (true, true, _) => 0xBFC0_0200,
            (true, false, _) => 0x8000_0000,
            (false, true, true) => 0xBFC0_0400,
            (false, true, false) => 0xBFC0_0380,
            (false, false, true) => 0x8000_0200,
            (false, false, false) => 0x8000_0180,
        };
        self.pc = vec;
        self.next_pc = vec.wrapping_add(4);
        self.in_delay = false;
    }

    // ------------------------------------------------------------ memory access

    pub(crate) fn load<B: Bus>(&mut self, bus: &mut B, vaddr: u32, size: u32) -> Mem<u64> {
        if vaddr & (size - 1) != 0 {
            self.raise(exc::ADEL, vaddr, false);
            return Err(());
        }
        let p = self.translate(vaddr, false)?;
        Ok(bus.read(p, size))
    }

    pub(crate) fn store<B: Bus>(&mut self, bus: &mut B, vaddr: u32, size: u32, v: u64) -> Mem<()> {
        if vaddr & (size - 1) != 0 {
            self.raise(exc::ADES, vaddr, false);
            return Err(());
        }
        let p = self.translate(vaddr, true)?;
        bus.write(p, v, size);
        Ok(())
    }

    pub(crate) fn fetch<B: Bus>(&mut self, bus: &mut B, vaddr: u32) -> Mem<u32> {
        if vaddr & 3 != 0 {
            self.raise(exc::ADEL, vaddr, false);
            return Err(());
        }
        let p = self.translate(vaddr, false)?;
        Ok(bus.read(p, 4) as u32)
    }

    // ------------------------------------------------------------ interrupts and timer

    fn interrupt_pending(&self) -> bool {
        let status = self.cop0[STATUS];
        if status & ST_IE == 0 || status & ST_EIE == 0 || status & (ST_EXL | ST_ERL) != 0 {
            return false;
        }
        let ip = (self.cop0[CAUSE] >> 8) & 0xFF;
        let mask = (status >> 8) & 0xFF;
        ip & mask != 0
    }

    /// Runs one instruction, or takes a pending interrupt. Returns the cycles used.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> u32 {
        self.cycles += 1;
        self.cop0[COUNT] = self.cop0[COUNT].wrapping_add(1);
        // Compare match: IP7.
        if self.cop0[COUNT] == self.cop0[COMPARE] {
            self.cop0[CAUSE] |= 1 << 15;
        }
        // Wired/random register.
        let wired = self.cop0[WIRED] & 0x3F;
        let r = self.cop0[RANDOM];
        self.cop0[RANDOM] = if r <= wired { (TLB_ENTRIES - 1) as u32 } else { r - 1 };
        // External lines drive IP2 / IP3.
        let cause = (self.cop0[CAUSE] & !(3 << 10)) | ((self.irq & 3) << 10);
        self.cop0[CAUSE] = cause;
        self.exception_taken = false;
        self.cur_pc = self.pc;
        self.cur_delay = self.in_delay;
        if self.interrupt_pending() {
            self.raise(exc::INT, 0, false);
            return 1;
        }
        let Ok(op) = self.fetch(bus, self.pc) else { return 1 };
        self.in_delay = false;
        self.pc = self.next_pc;
        self.next_pc = self.pc.wrapping_add(4);
        self.execute(bus, op);
        1
    }

    /// Runs about `budget` instructions.
    pub fn run<B: Bus>(&mut self, bus: &mut B, budget: u32) -> u32 {
        let mut used = 0;
        while used < budget {
            used += self.step(bus);
        }
        used
    }

    // ------------------------------------------------------------ branches (used by ops)

    /// A taken branch: the delay slot runs, then control goes to `target`.
    pub(crate) fn branch_to(&mut self, target: u32) {
        self.next_pc = target;
        self.in_delay = true;
    }

    /// A branch with a delay slot whose outcome is "not taken" for a *likely* branch: the
    /// delay slot is skipped.
    pub(crate) fn skip_delay_slot(&mut self) {
        self.pc = self.next_pc.wrapping_add(0);
        self.next_pc = self.pc.wrapping_add(4);
    }

    /// An ordinary branch that isn't taken still has a delay slot.
    pub(crate) fn mark_delay(&mut self) {
        self.in_delay = true;
    }
}

#[cfg(test)]
mod tests;
