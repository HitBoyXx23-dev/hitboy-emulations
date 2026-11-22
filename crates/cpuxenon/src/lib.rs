//! Safe Rust wrapper around the C++ IBM Xenon core (Xbox 360 CPU, one hardware thread).

use std::ffi::c_void;

#[repr(C)]
struct RawCpu {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub r: [u64; 32],
    pub lr: u64,
    pub ctr: u64,
    pub cr: u32,
    pub xer: u32,
    pub pc: u32,
    pub fpscr: u32,
}

#[repr(C)]
struct RawBus {
    ctx: *mut c_void,
    read: extern "C" fn(*mut c_void, u32, i32) -> u64,
    write: extern "C" fn(*mut c_void, u32, u64, i32),
    pages: *mut *mut u8,
}

extern "C" {
    fn xenon_new(bus: RawBus) -> *mut RawCpu;
    fn xenon_free(cpu: *mut RawCpu);
    fn xenon_run(cpu: *mut RawCpu, cycles: u32) -> u32;
    fn xenon_step(cpu: *mut RawCpu) -> u32;
    fn xenon_take_event(cpu: *mut RawCpu, op: *mut u32) -> i32;
    fn xenon_get_regs(cpu: *const RawCpu, out: *mut Regs);
    fn xenon_set_regs(cpu: *mut RawCpu, r: *const Regs);
    fn xenon_fpr(cpu: *const RawCpu, i: i32) -> f64;
    fn xenon_set_fpr(cpu: *mut RawCpu, i: i32, v: f64);
    fn xenon_vr(cpu: *const RawCpu, i: i32, out: *mut u32);
    fn xenon_set_vr(cpu: *mut RawCpu, i: i32, v: *const u32);
    fn xenon_timebase(cpu: *const RawCpu) -> u64;
    fn xenon_set_timebase(cpu: *mut RawCpu, tb: u64);
    fn xenon_context_size() -> u32;
    fn xenon_save_context(cpu: *const RawCpu, buf: *mut u8);
    fn xenon_load_context(cpu: *mut RawCpu, buf: *const u8);
}

/// A saved guest thread (all registers), for switching threads on one core.
#[derive(Clone)]
pub struct Context(Box<[u64]>);

impl Default for Context {
    fn default() -> Self {
        let words = (unsafe { xenon_context_size() } as usize).div_ceil(8);
        Context(vec![0u64; words].into_boxed_slice())
    }
}

/// Why the run loop stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// `sc` executed; pc is already past it.
    Syscall,
    /// A trap condition matched; pc is on the trap instruction.
    Trap(u32),
    /// An instruction this core doesn't implement; pc is on it.
    Invalid(u32),
}

/// Guest memory outside the mapped pages (hardware registers).
pub trait Bus {
    fn read(&mut self, addr: u32, size: u32) -> u64;
    fn write(&mut self, addr: u32, value: u64, size: u32);
}

extern "C" fn read_t<B: Bus>(ctx: *mut c_void, a: u32, size: i32) -> u64 {
    // SAFETY: ctx is the Box<B> owned by Cpu; only used during run()/step().
    unsafe { (*(ctx as *mut B)).read(a, size as u32) }
}
extern "C" fn write_t<B: Bus>(ctx: *mut c_void, a: u32, v: u64, size: i32) {
    unsafe { (*(ctx as *mut B)).write(a, v, size as u32) }
}

pub const PAGE: usize = 0x1000;
const PAGES: usize = 1 << 20;

pub struct Cpu<B: Bus> {
    raw: *mut RawCpu,
    bus: *mut B,
    pages: Box<[*mut u8]>,
}

impl<B: Bus> Cpu<B> {
    pub fn new(bus: B) -> Self {
        let bus = Box::into_raw(Box::new(bus));
        let mut pages = vec![std::ptr::null_mut::<u8>(); PAGES].into_boxed_slice();
        let raw = unsafe { xenon_new(RawBus { ctx: bus as *mut c_void, read: read_t::<B>, write: write_t::<B>, pages: pages.as_mut_ptr() }) };
        assert!(!raw.is_null());
        Self { raw, bus, pages }
    }

    /// Maps guest `addr..addr + len` (page aligned) onto host memory starting at `host`.
    ///
    /// # Safety
    /// `host` must stay valid for `len` bytes, and not move, while this CPU can run.
    pub unsafe fn map(&mut self, addr: u32, host: *mut u8, len: usize) {
        for i in 0..len / PAGE {
            self.pages[(addr as usize / PAGE + i) & (PAGES - 1)] = host.add(i * PAGE);
        }
    }

    pub fn unmap(&mut self, addr: u32, len: usize) {
        for i in 0..len / PAGE {
            self.pages[(addr as usize / PAGE + i) & (PAGES - 1)] = std::ptr::null_mut();
        }
    }

    pub fn is_mapped(&self, addr: u32) -> bool {
        !self.pages[addr as usize / PAGE].is_null()
    }

    /// Runs until `cycles` elapse or an event stops it.
    pub fn run(&mut self, cycles: u32) -> (u32, Option<Event>) {
        let used = unsafe { xenon_run(self.raw, cycles) };
        (used, self.take_event())
    }

    pub fn step(&mut self) -> Option<Event> {
        unsafe { xenon_step(self.raw) };
        self.take_event()
    }

    fn take_event(&mut self) -> Option<Event> {
        let mut op = 0;
        match unsafe { xenon_take_event(self.raw, &mut op) } {
            1 => Some(Event::Syscall),
            2 => Some(Event::Trap(op)),
            3 => Some(Event::Invalid(op)),
            _ => None,
        }
    }

    pub fn regs(&self) -> Regs {
        let mut r = Regs::default();
        unsafe { xenon_get_regs(self.raw, &mut r) };
        r
    }
    pub fn set_regs(&mut self, r: Regs) {
        unsafe { xenon_set_regs(self.raw, &r) }
    }
    pub fn fpr(&self, i: usize) -> f64 {
        unsafe { xenon_fpr(self.raw, i as i32) }
    }
    pub fn set_fpr(&mut self, i: usize, v: f64) {
        unsafe { xenon_set_fpr(self.raw, i as i32, v) }
    }
    pub fn vr(&self, i: usize) -> [u32; 4] {
        let mut v = [0u32; 4];
        unsafe { xenon_vr(self.raw, i as i32, v.as_mut_ptr()) };
        v
    }
    pub fn set_vr(&mut self, i: usize, v: [u32; 4]) {
        unsafe { xenon_set_vr(self.raw, i as i32, v.as_ptr()) }
    }
    pub fn timebase(&self) -> u64 {
        unsafe { xenon_timebase(self.raw) }
    }
    pub fn set_timebase(&mut self, tb: u64) {
        unsafe { xenon_set_timebase(self.raw, tb) }
    }
    pub fn save(&self, ctx: &mut Context) {
        unsafe { xenon_save_context(self.raw, ctx.0.as_mut_ptr() as *mut u8) }
    }
    pub fn load(&mut self, ctx: &Context) {
        unsafe { xenon_load_context(self.raw, ctx.0.as_ptr() as *const u8) }
    }
    /// Host pointer for a mapped guest address (null when unmapped).
    pub fn host(&self, addr: u32) -> *mut u8 {
        let p = self.pages[addr as usize / PAGE];
        if p.is_null() {
            p
        } else {
            unsafe { p.add(addr as usize % PAGE) }
        }
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
            xenon_free(self.raw);
            drop(Box::from_raw(self.bus));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoIo;
    impl Bus for NoIo {
        fn read(&mut self, _: u32, _: u32) -> u64 {
            0
        }
        fn write(&mut self, _: u32, _: u64, _: u32) {}
    }

    const BASE: u32 = 0x8200_0000;

    fn run(prog: &[u32], steps: usize, setup: impl FnOnce(&mut Cpu<NoIo>, &mut [u8])) -> (Cpu<NoIo>, Vec<u8>) {
        let mut mem = vec![0u8; 0x10000];
        for (i, w) in prog.iter().enumerate() {
            mem[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        let mut cpu = Cpu::new(NoIo);
        unsafe { cpu.map(BASE, mem.as_mut_ptr(), mem.len()) };
        let mut r = cpu.regs();
        r.pc = BASE;
        cpu.set_regs(r);
        setup(&mut cpu, &mut mem);
        for _ in 0..steps {
            if let Some(e) = cpu.step() {
                panic!("unexpected {e:?}");
            }
        }
        (cpu, mem)
    }

    fn d(op: u32, rd: u32, ra: u32, imm: u16) -> u32 {
        op << 26 | rd << 21 | ra << 16 | imm as u32
    }
    fn x(rd: u32, ra: u32, rb: u32, xo: u32, rc: bool) -> u32 {
        31 << 26 | rd << 21 | ra << 16 | rb << 11 | xo << 1 | rc as u32
    }
    fn set_r(c: &mut Cpu<NoIo>, i: usize, v: u64) {
        let mut r = c.regs();
        r.r[i] = v;
        c.set_regs(r);
    }

    #[test]
    fn sixty_four_bit_arithmetic_and_cr0() {
        // li r3,-1 ; rldicl r4,r3,0,32 (zero-extend) ; add. r5,r3,r3 ; mulld r6,r4,r4
        let rldicl = 30 << 26 | 3 << 21 | 4 << 16 | (32 & 31) << 6 | (32 >> 5) << 5;
        let prog = [d(14, 3, 0, 0xFFFF), rldicl, x(5, 3, 3, 266, true), x(6, 4, 4, 233, false)];
        let (c, _) = run(&prog, 4, |_, _| {});
        let r = c.regs();
        assert_eq!(r.r[3], u64::MAX);
        assert_eq!(r.r[4], 0xFFFF_FFFF);
        assert_eq!(r.r[5] as i64, -2);
        assert_eq!(r.cr >> 28, 0b1000, "CR0 = LT on the full 64-bit result");
        assert_eq!(r.r[6], 0xFFFF_FFFEu64 << 32 | 1);
    }

    #[test]
    fn word_compare_ignores_upper_half() {
        // cmpwi r3,0 with r3 = 0x1_0000_0000 -> EQ ; cmpdi r3,0 -> GT
        let (c, _) = run(&[d(11, 0, 3, 0), d(11, 1 << 2 | 1, 3, 0)], 2, |c, _| set_r(c, 3, 1 << 32));
        let cr = c.regs().cr;
        assert_eq!(cr >> 28, 0b0010);
        assert_eq!((cr >> 24) & 0xF, 0b0100);
    }

    #[test]
    fn doubleword_load_store_and_update() {
        // std r3,8(r4) ; ldu r5,8(r4)
        let prog = [62 << 26 | 3 << 21 | 4 << 16 | 8, 58 << 26 | 5 << 21 | 4 << 16 | 8 | 1];
        let (c, mem) = run(&prog, 2, |c, _| {
            set_r(c, 3, 0x1122_3344_5566_7788);
            set_r(c, 4, (BASE + 0x100) as u64);
        });
        assert_eq!(&mem[0x108..0x110], &0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(c.regs().r[5], 0x1122_3344_5566_7788);
        assert_eq!(c.regs().r[4], (BASE + 0x108) as u64);
    }

    #[test]
    fn syscall_stops_the_run_loop() {
        let mut mem = vec![0u8; 0x1000];
        mem[0..4].copy_from_slice(&d(14, 0, 0, 42).to_be_bytes());
        mem[4..8].copy_from_slice(&(17u32 << 26 | 2).to_be_bytes());
        let mut c = Cpu::new(NoIo);
        unsafe { c.map(BASE, mem.as_mut_ptr(), mem.len()) };
        let mut r = c.regs();
        r.pc = BASE;
        c.set_regs(r);
        let (_, ev) = c.run(1000);
        assert_eq!(ev, Some(Event::Syscall));
        assert_eq!(c.regs().r[0], 42);
        assert_eq!(c.regs().pc, BASE + 8);
    }

    #[test]
    fn vector_madd_and_perm() {
        // vmaddfp v3,v1,v2,v0 (d = a*c + b) ; vperm v4,v1,v2,v5
        let vmadd = 4 << 26 | 3 << 21 | 1 << 16 | 0 << 11 | 2 << 6 | 46;
        let vperm = 4 << 26 | 4 << 21 | 1 << 16 | 2 << 11 | 5 << 6 | 43;
        let f = |a: [f32; 4]| a.map(f32::to_bits);
        let (c, _) = run(&[vmadd, vperm], 2, |c, _| {
            c.set_vr(0, f([1.0, 1.0, 1.0, 1.0]));
            c.set_vr(1, f([1.0, 2.0, 3.0, 4.0]));
            c.set_vr(2, f([2.0, 2.0, 2.0, 2.0]));
            // Pick word 3 of v1 then words 0..2 of v2.
            c.set_vr(5, [0x0C0D0E0F, 0x10111213, 0x14151617, 0x18191A1B]);
        });
        assert_eq!(c.vr(3), f([3.0, 5.0, 7.0, 9.0]));
        assert_eq!(c.vr(4), f([4.0, 2.0, 2.0, 2.0]));
    }

    // VMX128 register fields: vD/vA/vB reach v0-v127 through extra bits.
    fn v128(op: u32, xo: u32, d: u32, a: u32, b: u32) -> u32 {
        op << 26 | xo | (d & 31) << 21 | (d >> 5) << 2 | (a & 31) << 16 | ((a >> 5) & 1) << 5 | (a >> 6) << 10 | (b & 31) << 11 | b >> 5
    }

    #[test]
    fn vmx128_dot_product_and_high_registers() {
        // vmsum4fp128 v100, v70, v127
        let f = |a: [f32; 4]| a.map(f32::to_bits);
        let (c, _) = run(&[v128(5, 464, 100, 70, 127)], 1, |c, _| {
            c.set_vr(70, f([1.0, 2.0, 3.0, 4.0]));
            c.set_vr(127, f([5.0, 6.0, 7.0, 8.0]));
        });
        assert_eq!(c.vr(100), f([70.0; 4]));
    }

    #[test]
    fn vmx128_rlimi_and_permwi() {
        // vrlimi128 v1, v2, imm=0b0101 (y,w), z=1 ; vpermwi128 v3, v2, perm = 3,2,1,0
        let rlimi = 6 << 26 | 0x710 | 1 << 21 | 0b0101 << 16 | 2 << 11 | 1 << 6;
        let perm = 0b11_10_01_00u32;
        let permwi = 6 << 26 | 0x210 | 3 << 21 | (perm & 31) << 16 | 2 << 11 | (perm >> 5) << 6;
        let (c, _) = run(&[rlimi, permwi], 2, |c, _| {
            c.set_vr(1, [10, 11, 12, 13]);
            c.set_vr(2, [20, 21, 22, 23]);
        });
        assert_eq!(c.vr(1), [10, 22, 12, 20]);
        assert_eq!(c.vr(3), [23, 22, 21, 20]);
    }

    #[test]
    fn vmx128_load_store_and_pack() {
        // lvx128 v90,r0,r4 ; vpkuwum128 v91,v90,v90 ; stvx128 v91,r0,r5
        let lvx = 4 << 26 | 195 | (90 & 31) << 21 | (90 >> 5) << 2 | 4 << 11;
        let stvx = 4 << 26 | 451 | (91 & 31) << 21 | (91 >> 5) << 2 | 5 << 11;
        let (_, mem) = run(&[lvx, v128(5, 896, 91, 90, 90), stvx], 3, |c, m| {
            set_r(c, 4, (BASE + 0x200) as u64);
            set_r(c, 5, (BASE + 0x300) as u64);
            m[0x200..0x210].copy_from_slice(&[0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 8]);
        });
        assert_eq!(&mem[0x300..0x310], &[0, 2, 0, 4, 0, 6, 0, 8, 0, 2, 0, 4, 0, 6, 0, 8]);
    }

    #[test]
    fn fp_convert_round_trip() {
        // fctidz f2,f1 ; fcfid f3,f2
        let fctidz = 63 << 26 | 2 << 21 | 1 << 11 | 815 << 1;
        let fcfid = 63 << 26 | 3 << 21 | 2 << 11 | 846 << 1;
        let (c, _) = run(&[fctidz, fcfid], 2, |c, _| c.set_fpr(1, -7.9));
        assert_eq!(c.fpr(3), -7.0);
    }
}
