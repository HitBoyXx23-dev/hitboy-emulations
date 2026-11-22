//! Safe Rust wrapper around the C++ 6502 interpreter core.

use std::ffi::c_void;

#[repr(C)]
struct RawCpu {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub p: u8,
    pub pc: u16,
}

type ReadFn = extern "C" fn(*mut c_void, u16) -> u8;
type WriteFn = extern "C" fn(*mut c_void, u16, u8);

extern "C" {
    fn cpu6502_new(ctx: *mut c_void, read: ReadFn, write: WriteFn) -> *mut RawCpu;
    fn cpu6502_state_size() -> std::os::raw::c_ulong;
    fn cpu6502_save_state(cpu: *const RawCpu, out: *mut u8);
    fn cpu6502_load_state(cpu: *mut RawCpu, input: *const u8);
    fn cpu6502_free(cpu: *mut RawCpu);
    fn cpu6502_reset(cpu: *mut RawCpu);
    fn cpu6502_nmi(cpu: *mut RawCpu);
    fn cpu6502_set_decimal(cpu: *mut RawCpu, enabled: i32);
    fn cpu6502_set_irq(cpu: *mut RawCpu, asserted: i32);
    fn cpu6502_step(cpu: *mut RawCpu) -> u32;
    fn cpu6502_get_regs(cpu: *const RawCpu, out: *mut Regs);
    fn cpu6502_set_regs(cpu: *mut RawCpu, regs: *const Regs);
}

/// Memory bus the CPU core reads and writes through.
pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, value: u8);
}

extern "C" fn read_trampoline<B: Bus>(ctx: *mut c_void, addr: u16) -> u8 {
    // SAFETY: ctx is the pointer from Box::into_raw in Cpu::new; it is only
    // dereferenced while the CPU is stepping, when no Rust borrow of the bus exists.
    unsafe { (*(ctx as *mut B)).read(addr) }
}

extern "C" fn write_trampoline<B: Bus>(ctx: *mut c_void, addr: u16, value: u8) {
    unsafe { (*(ctx as *mut B)).write(addr, value) }
}

/// A 6502 CPU that owns its bus.
pub struct Cpu<B: Bus> {
    raw: *mut RawCpu,
    bus: *mut B,
}

impl<B: Bus> Cpu<B> {

    /// Saves or loads every register and internal flag (save states, rewind).
    pub fn sync_state(&mut self, s: &mut state::State) {
        let n = unsafe { cpu6502_state_size() } as usize;
        // The size doubles as a check that the state came from this same build.
        s.check(n as u32);
        let mut buf = vec![0u8; n];
        if !s.is_load() {
            unsafe { cpu6502_save_state(self.raw, buf.as_mut_ptr()) };
        }
        s.raw(&mut buf);
        if s.is_load() && !s.failed() {
            unsafe { cpu6502_load_state(self.raw, buf.as_ptr()) };
        }
    }
    pub fn new(bus: B) -> Self {
        let bus = Box::into_raw(Box::new(bus));
        let raw = unsafe {
            cpu6502_new(bus as *mut c_void, read_trampoline::<B>, write_trampoline::<B>)
        };
        assert!(!raw.is_null(), "cpu6502_new failed");
        Self { raw, bus }
    }

    pub fn reset(&mut self) {
        unsafe { cpu6502_reset(self.raw) }
    }

    /// Turns on decimal-mode arithmetic (Atari 2600 and other full 6502s).
    pub fn set_decimal(&mut self, on: bool) {
        unsafe { cpu6502_set_decimal(self.raw, on as i32) }
    }

    pub fn nmi(&mut self) {
        unsafe { cpu6502_nmi(self.raw) }
    }

    pub fn set_irq(&mut self, asserted: bool) {
        unsafe { cpu6502_set_irq(self.raw, asserted as i32) }
    }

    /// Executes one instruction and returns the number of CPU cycles it took.
    pub fn step(&mut self) -> u32 {
        unsafe { cpu6502_step(self.raw) }
    }

    pub fn regs(&self) -> Regs {
        let mut r = Regs::default();
        unsafe { cpu6502_get_regs(self.raw, &mut r) };
        r
    }

    pub fn set_regs(&mut self, regs: Regs) {
        unsafe { cpu6502_set_regs(self.raw, &regs) }
    }

    pub fn bus(&self) -> &B {
        unsafe { &*self.bus }
    }

    pub fn bus_mut(&mut self) -> &mut B {
        unsafe { &mut *self.bus }
    }
}

impl<B: Bus> Drop for Cpu<B> {
    fn drop(&mut self) {
        unsafe {
            cpu6502_free(self.raw);
            drop(Box::from_raw(self.bus));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Bus for Ram {
        fn read(&mut self, addr: u16) -> u8 {
            self.0[addr as usize]
        }
        fn write(&mut self, addr: u16, value: u8) {
            self.0[addr as usize] = value;
        }
    }

    fn cpu_with(program: &[u8]) -> Cpu<Ram> {
        let mut mem = vec![0u8; 0x10000];
        mem[0x8000..0x8000 + program.len()].copy_from_slice(program);
        mem[0xFFFC] = 0x00;
        mem[0xFFFD] = 0x80;
        let mut cpu = Cpu::new(Ram(mem));
        cpu.reset();
        cpu
    }

    #[test]
    fn adds_with_carry_and_stores() {
        // LDA #$F0; CLC; ADC #$20; STA $10
        let mut cpu = cpu_with(&[0xA9, 0xF0, 0x18, 0x69, 0x20, 0x85, 0x10]);
        for _ in 0..4 {
            cpu.step();
        }
        assert_eq!(cpu.bus().0[0x10], 0x10);
        assert_eq!(cpu.regs().p & 1, 1, "carry set");
    }

    #[test]
    fn loop_counts_down_with_correct_cycles() {
        // LDX #5; loop: DEX; BNE loop; STX $00
        let mut cpu = cpu_with(&[0xA2, 0x05, 0xCA, 0xD0, 0xFD, 0x86, 0x00]);
        let mut cycles = 0;
        while cpu.regs().pc != 0x8007 {
            cycles += cpu.step();
        }
        assert_eq!(cpu.regs().x, 0);
        // LDX + 5*DEX + 4 taken BNE + 1 untaken BNE + STX
        assert_eq!(cycles, 2 + 5 * 2 + 4 * 3 + 2 + 3);
    }

    #[test]
    fn jsr_rts_round_trip() {
        // JSR $8010; LDY #1 ... at $8010: LDX #7; RTS
        let mut prog = vec![0x20, 0x10, 0x80, 0xA0, 0x01];
        prog.resize(0x10, 0xEA);
        prog.extend_from_slice(&[0xA2, 0x07, 0x60]);
        let mut cpu = cpu_with(&prog);
        for _ in 0..4 {
            cpu.step();
        }
        let r = cpu.regs();
        assert_eq!((r.x, r.y, r.sp), (7, 1, 0xFD));
    }

    #[test]
    fn decimal_mode_only_when_enabled() {
        // SED; CLC; LDA #$19; ADC #$28 ; SEC; LDA #$50; SBC #$01 -> $47 then $49
        let prog = [0xF8, 0x18, 0xA9, 0x19, 0x69, 0x28, 0x85, 0x10, 0x38, 0xA9, 0x50, 0xE9, 0x01];
        let mut cpu = cpu_with(&prog);
        cpu.set_decimal(true);
        for _ in 0..8 {
            cpu.step();
        }
        assert_eq!(cpu.bus().0[0x10], 0x47);
        assert_eq!(cpu.regs().a, 0x49);
        assert_eq!(cpu.regs().p & 1, 1, "no borrow");

        // BCD wrap: $99 + $01 = $00 with carry.
        let mut cpu = cpu_with(&[0xF8, 0x18, 0xA9, 0x99, 0x69, 0x01]);
        cpu.set_decimal(true);
        for _ in 0..4 {
            cpu.step();
        }
        assert_eq!((cpu.regs().a, cpu.regs().p & 1), (0x00, 1));

        // Without decimal support (NES) the flag is ignored.
        let mut cpu = cpu_with(&[0xF8, 0x18, 0xA9, 0x19, 0x69, 0x28]);
        for _ in 0..4 {
            cpu.step();
        }
        assert_eq!(cpu.regs().a, 0x41);
    }
}
