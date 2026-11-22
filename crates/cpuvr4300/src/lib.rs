//! Safe Rust wrapper around the C++ NEC VR4300 core (Nintendo 64 CPU).

use std::ffi::c_void;

#[repr(C)]
struct RawCpu {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub r: [u64; 32],
    pub hi: u64,
    pub lo: u64,
    pub pc: u64,
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
    fn vr4300_new(bus: RawBus) -> *mut RawCpu;
    fn vr4300_free(cpu: *mut RawCpu);
    fn vr4300_reset(cpu: *mut RawCpu);
    fn vr4300_step(cpu: *mut RawCpu) -> u32;
    fn vr4300_set_ip(cpu: *mut RawCpu, lines: u32);
    fn vr4300_run(cpu: *mut RawCpu, cycles: u32) -> u32;
    fn vr4300_get_regs(cpu: *const RawCpu, out: *mut Regs);
    fn vr4300_set_regs(cpu: *mut RawCpu, r: *const Regs);
    fn vr4300_cop0(cpu: *const RawCpu, reg: i32) -> u64;
    fn vr4300_set_cop0(cpu: *mut RawCpu, reg: i32, v: u64);
}

/// Physical memory as the CPU sees it; `addr` is aligned to `size` (1, 2, 4 or 8).
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
    // SAFETY: ctx is the Box<B> owned by Cpu; only used during step().
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
        let raw = unsafe { vr4300_new(RawBus { ctx: bus as *mut c_void, read: read_t::<B>, write: write_t::<B>, ram, ram_size }) };
        assert!(!raw.is_null());
        Self { raw, bus }
    }
    pub fn reset(&mut self) {
        unsafe { vr4300_reset(self.raw) }
    }
    #[inline]
    pub fn step(&mut self) -> u32 {
        unsafe { vr4300_step(self.raw) }
    }
    /// Runs for at least `cycles` cycles in one call; returns the cycles used.
    pub fn run(&mut self, cycles: u32) -> u32 {
        unsafe { vr4300_run(self.raw, cycles) }
    }
    /// Interrupt lines IP2..IP6 (bit 0 = IP2).
    pub fn set_ip(&mut self, lines: u32) {
        unsafe { vr4300_set_ip(self.raw, lines) }
    }
    pub fn regs(&self) -> Regs {
        let mut r = Regs::default();
        unsafe { vr4300_get_regs(self.raw, &mut r) };
        r
    }
    pub fn set_regs(&mut self, r: Regs) {
        unsafe { vr4300_set_regs(self.raw, &r) }
    }
    pub fn cop0(&self, reg: usize) -> u64 {
        unsafe { vr4300_cop0(self.raw, reg as i32) }
    }
    pub fn set_cop0(&mut self, reg: usize, v: u64) {
        unsafe { vr4300_set_cop0(self.raw, reg as i32, v) }
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
            vr4300_free(self.raw);
            drop(Box::from_raw(self.bus));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64 KiB of big-endian RAM (the N64 is big-endian), mirrored everywhere.
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
    }

    fn run(prog: &[u32], steps: usize) -> Cpu<Ram> {
        let mut m = vec![0u8; 0x10000];
        for (i, w) in prog.iter().enumerate() {
            m[0x1000 + i * 4..0x1004 + i * 4].copy_from_slice(&w.to_be_bytes());
        }
        let mut cpu = Cpu::new(Ram(m));
        let mut r = cpu.regs();
        r.pc = 0xFFFF_FFFF_8000_1000;
        cpu.set_regs(r);
        cpu.set_cop0(12, 0x3400_0000); // exceptions to 0x80000180
        for _ in 0..steps {
            cpu.step();
        }
        cpu
    }

    const fn i(op: u32, rs: u32, rt: u32, imm: u16) -> u32 {
        op << 26 | rs << 21 | rt << 16 | imm as u32
    }
    const fn r(rs: u32, rt: u32, rd: u32, sa: u32, f: u32) -> u32 {
        rs << 21 | rt << 16 | rd << 11 | sa << 6 | f
    }

    #[test]
    fn sign_extension_and_64bit_ops() {
        // lui r1,0x8000 (-> 0xFFFFFFFF80000000) ; daddiu r2,r0,1 ; dsll32 r3,r2,4 ; daddu r4,r1,r3
        let cpu = run(&[i(0x0F, 0, 1, 0x8000), i(0x19, 0, 2, 1), r(0, 2, 3, 4, 0x3C), r(1, 3, 4, 0, 0x2D)], 4);
        let g = cpu.regs();
        assert_eq!(g.r[1], 0xFFFF_FFFF_8000_0000);
        assert_eq!(g.r[3], 0x10_0000_0000);
        assert_eq!(g.r[4], 0xFFFF_FFFF_8000_0000u64.wrapping_add(0x10_0000_0000));
    }

    #[test]
    fn branch_delay_slot_and_likely_skip() {
        // beq r0,r0,+2 ; addiu r5,r5,1 (slot runs) ; addiu r5,r5,10 (skipped) ; bnel r0,r0,+2 ; addiu r5,r5,100 (skipped: likely not taken) ; addiu r6,r0,7
        let prog = [i(0x04, 0, 0, 2), i(0x09, 5, 5, 1), i(0x09, 5, 5, 10), i(0x15, 0, 0, 2), i(0x09, 5, 5, 100), i(0x09, 0, 6, 7)];
        let g = run(&prog, 4).regs();
        assert_eq!(g.r[5], 1);
        assert_eq!(g.r[6], 7);
    }

    #[test]
    fn big_endian_loads_and_stores() {
        // lui r1,0x8000 ; ori r1,r1,0x2000 (kseg0) ; lui r2,0x1122 ; ori r2,r2,0x3344 ; sw r2,0(r1) ; lbu r3,1(r1) ; lh r4,2(r1)
        let prog = [i(0x0F, 0, 1, 0x8000), i(0x0D, 1, 1, 0x2000), i(0x0F, 0, 2, 0x1122), i(0x0D, 2, 2, 0x3344), i(0x2B, 1, 2, 0), i(0x24, 1, 3, 1), i(0x21, 1, 4, 2)];
        let g = run(&prog, 7).regs();
        assert_eq!(g.r[3], 0x22);
        assert_eq!(g.r[4], 0x3344);
    }

    #[test]
    fn dmult_128_bit_product() {
        // daddiu r1,r0,-1 ; daddiu r2,r0,3 ; dmult r1,r2 ; mfhi r3 ; mflo r4
        let prog = [i(0x19, 0, 1, 0xFFFF), i(0x19, 0, 2, 3), r(1, 2, 0, 0, 0x1C), r(0, 0, 3, 0, 0x10), r(0, 0, 4, 0, 0x12)];
        let g = run(&prog, 5).regs();
        assert_eq!((g.r[3], g.r[4]), (u64::MAX, (-3i64) as u64));
    }

    #[test]
    fn unmapped_user_address_takes_tlb_refill() {
        // addiu r1,r0,0x2000 ; lw r2,0(r1)  -> TLB miss, refill vector 0x80000000
        let cpu = run(&[i(0x09, 0, 1, 0x2000), i(0x23, 1, 2, 0)], 2);
        assert_eq!(cpu.regs().pc, 0xFFFF_FFFF_8000_0000);
        assert_eq!((cpu.cop0(13) >> 2) & 31, 2, "TLBL");
        assert_eq!(cpu.cop0(8) as u32, 0x2000, "BadVAddr");
    }

    #[test]
    fn fpu_single_precision() {
        // lui r1,0x4040 (3.0f) ; mtc1 r1,f0 ; add.s f2,f0,f0 ; mfc1 r2,f2
        let prog = [i(0x0F, 0, 1, 0x4040), 0x4481_0000, 0x4600_0080, 0x4402_1000];
        let g = run(&prog, 4).regs();
        assert_eq!(g.r[2] as u32, 6.0f32.to_bits());
    }
}
