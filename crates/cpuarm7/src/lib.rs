//! Safe Rust wrapper around the C++ ARM7TDMI core (Game Boy Advance CPU).

use std::ffi::c_void;

#[repr(C)]
struct RawCpu {
    _private: [u8; 0],
}

#[repr(C)]
struct RawBus {
    ctx: *mut c_void,
    read: extern "C" fn(*mut c_void, u32, i32) -> u32,
    write: extern "C" fn(*mut c_void, u32, u32, i32),
    swi: extern "C" fn(*mut c_void, u32) -> i32,
}

extern "C" {
    fn arm7_new(bus: RawBus) -> *mut RawCpu;
    fn arm7_state_size() -> std::os::raw::c_ulong;
    fn arm7_save_state(cpu: *const RawCpu, out: *mut u8);
    fn arm7_load_state(cpu: *mut RawCpu, input: *const u8);
    fn arm7_free(cpu: *mut RawCpu);
    fn arm7_reset(cpu: *mut RawCpu, entry: u32);
    fn arm7_step(cpu: *mut RawCpu) -> u32;
    fn arm7_set_irq(cpu: *mut RawCpu, asserted: i32);
    fn arm7_reg(cpu: *const RawCpu, i: i32) -> u32;
    fn arm7_set_reg(cpu: *mut RawCpu, i: i32, v: u32);
    fn arm7_cpsr(cpu: *const RawCpu) -> u32;
    fn arm7_repeat(cpu: *mut RawCpu);
}

/// Register access handed to the host while it services a software interrupt.
pub struct Regs {
    raw: *mut RawCpu,
}

impl Regs {
    pub fn get(&self, i: usize) -> u32 {
        unsafe { arm7_reg(self.raw, i as i32) }
    }
    pub fn set(&mut self, i: usize, v: u32) {
        unsafe { arm7_set_reg(self.raw, i as i32, v) }
    }
    pub fn cpsr(&self) -> u32 {
        unsafe { arm7_cpsr(self.raw) }
    }
    /// Re-runs the SWI instruction next step (for BIOS calls that wait for an interrupt).
    pub fn repeat(&mut self) {
        unsafe { arm7_repeat(self.raw) }
    }
}

pub trait Bus {
    /// `addr` is aligned to `size` (1, 2 or 4).
    fn read(&mut self, addr: u32, size: u32) -> u32;
    fn write(&mut self, addr: u32, value: u32, size: u32);
    /// Return true if handled (high-level BIOS); false takes the hardware SWI exception.
    fn swi(&mut self, number: u32, regs: &mut Regs) -> bool;
}

struct Ctx<B> {
    bus: B,
    raw: *mut RawCpu,
}

extern "C" fn read_t<B: Bus>(ctx: *mut c_void, a: u32, size: i32) -> u32 {
    // SAFETY: ctx is the Box<Ctx<B>> owned by Cpu, used only during step().
    unsafe { (*(ctx as *mut Ctx<B>)).bus.read(a, size as u32) }
}
extern "C" fn write_t<B: Bus>(ctx: *mut c_void, a: u32, v: u32, size: i32) {
    unsafe { (*(ctx as *mut Ctx<B>)).bus.write(a, v, size as u32) }
}
extern "C" fn swi_t<B: Bus>(ctx: *mut c_void, n: u32) -> i32 {
    unsafe {
        let c = &mut *(ctx as *mut Ctx<B>);
        let mut regs = Regs { raw: c.raw };
        c.bus.swi(n, &mut regs) as i32
    }
}

pub struct Cpu<B: Bus> {
    raw: *mut RawCpu,
    ctx: *mut Ctx<B>,
}

impl<B: Bus> Cpu<B> {

    /// Saves or loads every register and internal flag (save states, rewind).
    pub fn sync_state(&mut self, s: &mut state::State) {
        let n = unsafe { arm7_state_size() } as usize;
        // The size doubles as a check that the state came from this same build.
        s.check(n as u32);
        let mut buf = vec![0u8; n];
        if !s.is_load() {
            unsafe { arm7_save_state(self.raw, buf.as_mut_ptr()) };
        }
        s.raw(&mut buf);
        if s.is_load() && !s.failed() {
            unsafe { arm7_load_state(self.raw, buf.as_ptr()) };
        }
    }
    pub fn new(bus: B) -> Self {
        let ctx = Box::into_raw(Box::new(Ctx { bus, raw: std::ptr::null_mut() }));
        let raw = unsafe { arm7_new(RawBus { ctx: ctx as *mut c_void, read: read_t::<B>, write: write_t::<B>, swi: swi_t::<B> }) };
        assert!(!raw.is_null());
        unsafe { (*ctx).raw = raw };
        Self { raw, ctx }
    }
    pub fn reset(&mut self, entry: u32) {
        unsafe { arm7_reset(self.raw, entry) }
    }
    #[inline]
    pub fn step(&mut self) -> u32 {
        unsafe { arm7_step(self.raw) }
    }
    pub fn set_irq(&mut self, on: bool) {
        unsafe { arm7_set_irq(self.raw, on as i32) }
    }
    pub fn reg(&self, i: usize) -> u32 {
        unsafe { arm7_reg(self.raw, i as i32) }
    }
    pub fn set_reg(&mut self, i: usize, v: u32) {
        unsafe { arm7_set_reg(self.raw, i as i32, v) }
    }
    pub fn cpsr(&self) -> u32 {
        unsafe { arm7_cpsr(self.raw) }
    }
    pub fn bus(&self) -> &B {
        unsafe { &(*self.ctx).bus }
    }
    pub fn bus_mut(&mut self) -> &mut B {
        unsafe { &mut (*self.ctx).bus }
    }
}

impl<B: Bus> Drop for Cpu<B> {
    fn drop(&mut self) {
        unsafe {
            arm7_free(self.raw);
            drop(Box::from_raw(self.ctx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>, Vec<u32>);
    impl Bus for Ram {
        fn read(&mut self, a: u32, size: u32) -> u32 {
            let a = (a & 0xFFFF) as usize;
            (0..size as usize).fold(0, |v, i| v | (self.0[a + i] as u32) << (8 * i))
        }
        fn write(&mut self, a: u32, v: u32, size: u32) {
            let a = (a & 0xFFFF) as usize;
            for i in 0..size as usize {
                self.0[a + i] = (v >> (8 * i)) as u8;
            }
        }
        fn swi(&mut self, n: u32, regs: &mut Regs) -> bool {
            self.1.push(n);
            regs.set(0, regs.get(0) * 2);
            true
        }
    }

    fn run(arm: &[u32], thumb: &[u16], steps: usize) -> Cpu<Ram> {
        let mut m = vec![0u8; 0x10000];
        for (i, w) in arm.iter().enumerate() {
            m[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        for (i, h) in thumb.iter().enumerate() {
            m[0x1000 + i * 2..0x1002 + i * 2].copy_from_slice(&h.to_le_bytes());
        }
        let mut cpu = Cpu::new(Ram(m, Vec::new()));
        cpu.reset(0);
        for _ in 0..steps {
            cpu.step();
        }
        cpu
    }

    #[test]
    fn arm_data_processing_and_flags() {
        // mov r0,#5 ; mov r1,#3 ; subs r2,r0,r1 ; rsbs r3,r0,r1 ; add r4,r0,r1,lsl #2
        let cpu = run(&[0xE3A00005, 0xE3A01003, 0xE0502001, 0xE0703001, 0xE0804101], &[], 5);
        assert_eq!((cpu.reg(2), cpu.reg(3), cpu.reg(4)), (2, (-2i32) as u32, 17));
        assert!(cpu.cpsr() & (1 << 31) != 0, "N set by the negative result");
        assert!(cpu.cpsr() & (1 << 29) == 0, "borrow clears C");
    }

    #[test]
    fn load_store_and_branch_link() {
        // mov r0,#0x200 ; mov r1,#0x41 ; strb r1,[r0,#1] ; ldr r2,[r0] ; bl +0 ; mov r3,#9
        let cpu = run(&[0xE3A00C02, 0xE3A01041, 0xE5C01001, 0xE5902000, 0xEB000000, 0xE3A03001, 0xE3A03009], &[], 6);
        assert_eq!(cpu.reg(2), 0x4100);
        assert_eq!(cpu.reg(14), 0x14, "BL saves the return address");
        assert_eq!(cpu.reg(3), 9, "BL skipped the next instruction");
    }

    #[test]
    fn thumb_via_bx_and_swi_to_host() {
        // ARM: add r0,pc,#0x1000-8+1 would be awkward; use mov r0,#0x1000 ; orr r0,r0,#1 ; bx r0
        // Thumb at 0x1000: movs r0,#21 ; swi 6 ; lsls r1,r0,#1
        let mut cpu = run(&[0xE3A00A01, 0xE3800001, 0xE12FFF10], &[0x2015, 0xDF06, 0x0041], 6);
        assert!(cpu.cpsr() & 0x20 != 0, "in Thumb state");
        assert_eq!(cpu.reg(0), 42, "host SWI doubled r0");
        assert_eq!(cpu.reg(1), 84);
        assert_eq!(cpu.bus_mut().1, vec![6]);
    }

    #[test]
    fn multiply_long_signed() {
        // mvn r0,#0 (-1) ; mov r1,#5 ; smull r2,r3,r0,r1
        let cpu = run(&[0xE3E00000, 0xE3A01005, 0xE0C32190], &[], 3);
        assert_eq!((cpu.reg(2), cpu.reg(3)), ((-5i32) as u32, 0xFFFF_FFFF));
    }
}
