//! Safe Rust wrapper around the C++ SM83 (Game Boy CPU) core.

use std::ffi::c_void;

#[repr(C)]
struct RawCpu {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,
    pub ime: u8,
    pub halted: u8,
}

type ReadFn = extern "C" fn(*mut c_void, u16) -> u8;
type WriteFn = extern "C" fn(*mut c_void, u16, u8);

extern "C" {
    fn sm83_new(ctx: *mut c_void, r: ReadFn, w: WriteFn) -> *mut RawCpu;
    fn sm83_state_size() -> std::os::raw::c_ulong;
    fn sm83_save_state(cpu: *const RawCpu, out: *mut u8);
    fn sm83_load_state(cpu: *mut RawCpu, input: *const u8);
    fn sm83_free(cpu: *mut RawCpu);
    fn sm83_step(cpu: *mut RawCpu) -> u32;
    fn sm83_get_regs(cpu: *const RawCpu, out: *mut Regs);
    fn sm83_set_regs(cpu: *mut RawCpu, r: *const Regs);
}

pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, value: u8);
}

extern "C" fn read_tramp<B: Bus>(ctx: *mut c_void, addr: u16) -> u8 {
    // SAFETY: ctx is the Box<B> owned by Cpu; only dereferenced during step().
    unsafe { (*(ctx as *mut B)).read(addr) }
}

extern "C" fn write_tramp<B: Bus>(ctx: *mut c_void, addr: u16, v: u8) {
    unsafe { (*(ctx as *mut B)).write(addr, v) }
}

pub struct Cpu<B: Bus> {
    raw: *mut RawCpu,
    bus: *mut B,
}

impl<B: Bus> Cpu<B> {

    /// Saves or loads every register and internal flag (save states, rewind).
    pub fn sync_state(&mut self, s: &mut state::State) {
        let n = unsafe { sm83_state_size() } as usize;
        // The size doubles as a check that the state came from this same build.
        s.check(n as u32);
        let mut buf = vec![0u8; n];
        if !s.is_load() {
            unsafe { sm83_save_state(self.raw, buf.as_mut_ptr()) };
        }
        s.raw(&mut buf);
        if s.is_load() && !s.failed() {
            unsafe { sm83_load_state(self.raw, buf.as_ptr()) };
        }
    }
    pub fn new(bus: B) -> Self {
        let bus = Box::into_raw(Box::new(bus));
        let raw = unsafe { sm83_new(bus as *mut c_void, read_tramp::<B>, write_tramp::<B>) };
        assert!(!raw.is_null());
        Self { raw, bus }
    }

    /// One instruction; returns T-cycles.
    #[inline]
    pub fn step(&mut self) -> u32 {
        unsafe { sm83_step(self.raw) }
    }

    pub fn regs(&self) -> Regs {
        let mut r = Regs::default();
        unsafe { sm83_get_regs(self.raw, &mut r) };
        r
    }

    pub fn set_regs(&mut self, r: Regs) {
        unsafe { sm83_set_regs(self.raw, &r) }
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
            sm83_free(self.raw);
            drop(Box::from_raw(self.bus));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Bus for Ram {
        fn read(&mut self, a: u16) -> u8 {
            self.0[a as usize]
        }
        fn write(&mut self, a: u16, v: u8) {
            self.0[a as usize] = v;
        }
    }

    fn run(code: &[u8], steps: usize) -> Cpu<Ram> {
        let mut mem = vec![0u8; 0x10000];
        mem[0x100..0x100 + code.len()].copy_from_slice(code);
        let mut cpu = Cpu::new(Ram(mem));
        for _ in 0..steps {
            cpu.step();
        }
        cpu
    }

    #[test]
    fn alu_flags_and_loads() {
        // LD A,0x0F; ADD A,0x01 -> 0x10 H; LD B,A; SUB 0x10 -> 0 Z N; LD (0xC000),A... use LD HL
        let cpu = run(&[0x3E, 0x0F, 0xC6, 0x01, 0x47, 0xD6, 0x10], 4);
        let r = cpu.regs();
        assert_eq!(r.b, 0x10);
        assert_eq!(r.a, 0);
        assert_eq!(r.f & 0xC0, 0xC0); // Z and N
    }

    #[test]
    fn loop_call_ret_and_cb() {
        // LD B,5; loop: DEC B; JR NZ,loop; CALL 0x0110; HALT ... @0x110: LD A,0x81; RLCA? use CB SWAP A; RET
        let mut code = vec![0x06, 0x05, 0x05, 0x20, 0xFD, 0xCD, 0x10, 0x01, 0x76];
        code.resize(0x10, 0);
        code.extend_from_slice(&[0x3E, 0x12, 0xCB, 0x37, 0xC9]);
        let cpu = run(&code, 5 * 2 + 1 + 1 + 3 + 1);
        let r = cpu.regs();
        assert_eq!(r.b, 0);
        assert_eq!(r.a, 0x21, "SWAP A");
        assert_eq!(r.halted, 1);
        assert_eq!(r.sp, 0xFFFE);
    }

    #[test]
    fn daa_after_bcd_add() {
        // LD A,0x45; ADD A,0x38; DAA -> 0x83
        let cpu = run(&[0x3E, 0x45, 0xC6, 0x38, 0x27], 3);
        assert_eq!(cpu.regs().a, 0x83);
    }
}
