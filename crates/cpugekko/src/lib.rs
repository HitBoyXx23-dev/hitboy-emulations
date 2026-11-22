//! Safe Rust wrapper around the C++ IBM Gekko core (GameCube CPU).

use std::ffi::c_void;

#[repr(C)]
struct RawCpu {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub r: [u32; 32],
    pub pc: u32,
    pub lr: u32,
    pub ctr: u32,
    pub cr: u32,
    pub xer: u32,
    pub msr: u32,
}

#[repr(C)]
struct RawBus {
    ctx: *mut c_void,
    read: extern "C" fn(*mut c_void, u32, i32) -> u64,
    write: extern "C" fn(*mut c_void, u32, u64, i32),
    ram: *mut u8,
    ram_size: u32,
}

extern "C" {
    fn gekko_new(bus: RawBus) -> *mut RawCpu;
    fn gekko_free(cpu: *mut RawCpu);
    fn gekko_reset(cpu: *mut RawCpu);
    fn gekko_run(cpu: *mut RawCpu, cycles: u32) -> u32;
    fn gekko_step(cpu: *mut RawCpu) -> u32;
    fn gekko_set_tb_divisor(cpu: *mut RawCpu, divisor: u32);
    fn gekko_set_tb_multiplier(cpu: *mut RawCpu, multiplier: u32);
    fn gekko_set_irq(cpu: *mut RawCpu, on: i32);
    fn gekko_get_regs(cpu: *const RawCpu, out: *mut Regs);
    fn gekko_set_regs(cpu: *mut RawCpu, r: *const Regs);
    fn gekko_fpr(cpu: *const RawCpu, i: i32, ps1: i32) -> f64;
    fn gekko_set_fpr(cpu: *mut RawCpu, i: i32, ps0: f64, ps1: f64);
    fn gekko_spr(cpu: *const RawCpu, n: i32) -> u32;
    fn gekko_set_spr(cpu: *mut RawCpu, n: i32, v: u32);
}

/// Physical memory; `addr` is aligned to `size` (1, 2, 4 or 8), values big-endian.
pub trait Bus {
    fn read(&mut self, addr: u32, size: u32) -> u64;
    fn write(&mut self, addr: u32, value: u64, size: u32);
    /// Main RAM (big-endian bytes at physical 0) for the CPU's direct fast path.
    /// The buffer must not be reallocated while the CPU exists.
    fn ram(&mut self) -> Option<&mut [u8]> {
        None
    }
}

extern "C" fn read_t<B: Bus>(ctx: *mut c_void, a: u32, size: i32) -> u64 {
    // SAFETY: ctx is the Box<B> owned by Cpu; only used during run()/step().
    unsafe { (*(ctx as *mut B)).read(a, size as u32) }
}
extern "C" fn write_t<B: Bus>(ctx: *mut c_void, a: u32, v: u64, size: i32) {
    unsafe { (*(ctx as *mut B)).write(a, v, size as u32) }
}

pub struct Cpu<B: Bus> {
    raw: *mut RawCpu,
    bus: *mut B,
}

impl<B: Bus> Cpu<B> {
    pub fn new(bus: B) -> Self {
        let bus = Box::into_raw(Box::new(bus));
        let (ram, ram_size) = match unsafe { (*bus).ram() } {
            Some(r) => (r.as_mut_ptr(), r.len() as u32),
            None => (std::ptr::null_mut(), 0),
        };
        let raw = unsafe { gekko_new(RawBus { ctx: bus as *mut c_void, read: read_t::<B>, write: write_t::<B>, ram, ram_size }) };
        assert!(!raw.is_null());
        Self { raw, bus }
    }
    pub fn reset(&mut self) {
        unsafe { gekko_reset(self.raw) }
    }
    /// Instructions per time-base tick. The real chip needs 12 (the time base runs at 1/12 of
    /// the CPU clock); a host that runs fewer instructions per second than the real chip
    /// uses a smaller number so the clock keeps real-time pace.
    /// Time-base ticks credited per tick event, for cores that run fewer instructions than the real chip.
    pub fn set_tb_multiplier(&mut self, multiplier: u32) {
        unsafe { gekko_set_tb_multiplier(self.raw, multiplier) }
    }
    pub fn set_tb_divisor(&mut self, divisor: u32) {
        unsafe { gekko_set_tb_divisor(self.raw, divisor) }
    }

    pub fn run(&mut self, cycles: u32) -> u32 {
        unsafe { gekko_run(self.raw, cycles) }
    }
    pub fn step(&mut self) -> u32 {
        unsafe { gekko_step(self.raw) }
    }
    pub fn set_irq(&mut self, on: bool) {
        unsafe { gekko_set_irq(self.raw, on as i32) }
    }
    pub fn regs(&self) -> Regs {
        let mut r = Regs::default();
        unsafe { gekko_get_regs(self.raw, &mut r) };
        r
    }
    pub fn set_regs(&mut self, r: Regs) {
        unsafe { gekko_set_regs(self.raw, &r) }
    }
    pub fn fpr(&self, i: usize) -> (f64, f64) {
        unsafe { (gekko_fpr(self.raw, i as i32, 0), gekko_fpr(self.raw, i as i32, 1)) }
    }
    pub fn set_fpr(&mut self, i: usize, ps0: f64, ps1: f64) {
        unsafe { gekko_set_fpr(self.raw, i as i32, ps0, ps1) }
    }
    pub fn spr(&self, n: usize) -> u32 {
        unsafe { gekko_spr(self.raw, n as i32) }
    }
    pub fn set_spr(&mut self, n: usize, v: u32) {
        unsafe { gekko_set_spr(self.raw, n as i32, v) }
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
            gekko_free(self.raw);
            drop(Box::from_raw(self.bus));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u8>);
    impl Bus for Ram {
        fn read(&mut self, a: u32, size: u32) -> u64 {
            let a = (a & 0xFFFF) as usize;
            (0..size as usize).fold(0, |v, i| (v << 8) | self.0[a + i] as u64)
        }
        fn write(&mut self, a: u32, v: u64, size: u32) {
            let a = (a & 0xFFFF) as usize;
            for i in 0..size as usize {
                self.0[a + i] = (v >> (8 * (size as usize - 1 - i))) as u8;
            }
        }
        fn ram(&mut self) -> Option<&mut [u8]> {
            Some(&mut self.0)
        }
    }

    fn run(prog: &[u32], steps: usize, setup: impl FnOnce(&mut Cpu<Ram>)) -> Cpu<Ram> {
        let mut m = vec![0u8; 0x10000];
        for (i, w) in prog.iter().enumerate() {
            m[0x1000 + i * 4..0x1004 + i * 4].copy_from_slice(&w.to_be_bytes());
        }
        let mut cpu = Cpu::new(Ram(m));
        let mut r = cpu.regs();
        r.pc = 0x1000;
        r.msr = 0x2000; // FP available, translation off
        cpu.set_regs(r);
        setup(&mut cpu);
        for _ in 0..steps {
            cpu.step();
        }
        cpu
    }

    // Encoders for the handful of instructions the tests use.
    fn d(op: u32, rd: u32, ra: u32, imm: u16) -> u32 {
        op << 26 | rd << 21 | ra << 16 | imm as u32
    }
    fn x(rd: u32, ra: u32, rb: u32, xo: u32, rc: bool) -> u32 {
        31 << 26 | rd << 21 | ra << 16 | rb << 11 | xo << 1 | rc as u32
    }

    #[test]
    fn arithmetic_rotate_and_cr0() {
        // li r3,-5 ; li r4,7 ; add. r5,r3,r4 ; rlwinm r6,r4,4,0,27 ; subfc r7,r4,r3
        let prog = [d(14, 3, 0, 0xFFFB), d(14, 4, 0, 7), x(5, 3, 4, 266, true), 21 << 26 | 4 << 21 | 6 << 16 | 4 << 11 | 27 << 1, x(7, 4, 3, 8, false)];
        let c = run(&prog, 5, |_| {});
        let r = c.regs();
        assert_eq!(r.r[5], 2);
        assert_eq!(r.cr >> 28, 0b0100, "CR0 = GT");
        assert_eq!(r.r[6], 0x70);
        assert_eq!(r.r[7], (-12i32) as u32);
    }

    #[test]
    fn branch_and_link_with_ctr_loop() {
        // li r3,0 ; li r4,5 ; mtctr r4 ; loop: addi r3,r3,2 ; bdnz loop
        let mtctr = x(4, 9, 0, 467, false); // mtspr CTR (spr 9 -> split field 9<<16)
        let prog = [d(14, 3, 0, 0), d(14, 4, 0, 5), mtctr, d(14, 3, 3, 2), 16 << 26 | 16 << 21 | 0xFFFC];
        let c = run(&prog, 3 + 10, |_| {});
        assert_eq!(c.regs().r[3], 10);
        assert_eq!(c.regs().ctr, 0);
    }

    #[test]
    fn big_endian_loads_stores() {
        // lis r3,0x1122 ; ori r3,r3,0x3344 ; li r4,0x2000 ; stw r3,0(r4) ; lbz r5,1(r4) ; lha r6,2(r4)
        let prog = [d(15, 3, 0, 0x1122), d(24, 3, 3, 0x3344), d(14, 4, 0, 0x2000), d(36, 3, 4, 0), d(34, 5, 4, 1), d(42, 6, 4, 2)];
        let r = run(&prog, 6, |_| {}).regs();
        assert_eq!(r.r[5], 0x22);
        assert_eq!(r.r[6], 0x3344);
    }

    #[test]
    fn floating_point_and_paired_singles() {
        // fadd f3,f1,f2 ; ps_mul f4,f1,f2 (a=f1, c=f2) ; ps_merge10 f5,f1,f2
        let fadd = 63 << 26 | 3 << 21 | 1 << 16 | 2 << 11 | 21 << 1;
        let ps_mul = 4 << 26 | 4 << 21 | 1 << 16 | 2 << 6 | 25 << 1;
        let merge10 = 4 << 26 | 5 << 21 | 1 << 16 | 2 << 11 | 592 << 1;
        let c = run(&[fadd, ps_mul, merge10], 3, |c| {
            c.set_fpr(1, 1.5, 2.0);
            c.set_fpr(2, 4.0, 3.0);
        });
        assert_eq!(c.fpr(3).0, 5.5);
        assert_eq!(c.fpr(4), (6.0, 6.0));
        assert_eq!(c.fpr(5), (2.0, 4.0));
    }

    #[test]
    fn quantised_paired_load() {
        // GQR0: load type s16 (7), scale 8 -> value/256. psq_l f1,0(r4),0,0
        let psq_l = 56 << 26 | 1 << 21 | 4 << 16;
        let c = run(&[d(14, 4, 0, 0x3000), psq_l], 2, |c| {
            c.set_spr(912, 7 << 16 | 8 << 24);
            let m = &mut c.bus_mut().0;
            m[0x3000..0x3004].copy_from_slice(&[0x01, 0x00, 0xFF, 0x80]); // 256, -128
        });
        assert_eq!(c.fpr(1), (1.0, -0.5));
    }

    #[test]
    fn system_call_vectors_to_0xc00() {
        let c = run(&[17 << 26 | 2], 1, |_| {});
        assert_eq!(c.regs().pc, 0xC00);
        assert_eq!(c.spr(26), 0x1004, "SRR0 = next instruction");
    }
}
