//! Safe Rust wrapper around the C++ MIPS R3000A core (PlayStation CPU).

use std::ffi::c_void;

#[repr(C)]
struct RawCpu {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub r: [u32; 32],
    pub hi: u32,
    pub lo: u32,
    pub pc: u32,
    pub next_pc: u32,
    pub sr: u32,
    pub cause: u32,
    pub epc: u32,
    pub badvaddr: u32,
}

#[repr(C)]
struct RawBus {
    ctx: *mut c_void,
    read: extern "C" fn(*mut c_void, u32, i32) -> u32,
    write: extern "C" fn(*mut c_void, u32, u32, i32),
    gte_read: extern "C" fn(*mut c_void, i32) -> u32,
    gte_write: extern "C" fn(*mut c_void, i32, u32),
    gte_command: extern "C" fn(*mut c_void, u32),
}

extern "C" {
    fn r3k_new(bus: RawBus) -> *mut RawCpu;
    fn r3k_free(cpu: *mut RawCpu);
    fn r3k_reset(cpu: *mut RawCpu);
    fn r3k_set_ram(cpu: *mut RawCpu, ram: *mut u8);
    fn r3k_step(cpu: *mut RawCpu) -> u32;
    fn r3k_set_irq(cpu: *mut RawCpu, asserted: i32);
    fn r3k_get_regs(cpu: *const RawCpu, out: *mut Regs);
    fn r3k_set_regs(cpu: *mut RawCpu, r: *const Regs);
    fn r3k_state_size() -> std::os::raw::c_ulong;
    fn r3k_save_state(cpu: *const RawCpu, out: *mut u8);
    fn r3k_load_state(cpu: *mut RawCpu, input: *const u8);
}

/// The PlayStation side: memory (virtual addresses; the host maps segments) and the GTE.
pub trait Bus {
    fn read(&mut self, addr: u32, size: u32) -> u32;
    fn write(&mut self, addr: u32, value: u32, size: u32);
    /// GTE registers: 0-31 data, 32-63 control.
    fn gte_read(&mut self, reg: u32) -> u32;
    fn gte_write(&mut self, reg: u32, value: u32);
    fn gte_command(&mut self, cmd: u32);
}

extern "C" fn read_t<B: Bus>(ctx: *mut c_void, a: u32, size: i32) -> u32 {
    // SAFETY: ctx is the Box<B> owned by Cpu; only dereferenced during step()/reset().
    unsafe { (*(ctx as *mut B)).read(a, size as u32) }
}
extern "C" fn write_t<B: Bus>(ctx: *mut c_void, a: u32, v: u32, size: i32) {
    unsafe { (*(ctx as *mut B)).write(a, v, size as u32) }
}
extern "C" fn gte_read_t<B: Bus>(ctx: *mut c_void, r: i32) -> u32 {
    unsafe { (*(ctx as *mut B)).gte_read(r as u32) }
}
extern "C" fn gte_write_t<B: Bus>(ctx: *mut c_void, r: i32, v: u32) {
    unsafe { (*(ctx as *mut B)).gte_write(r as u32, v) }
}
extern "C" fn gte_cmd_t<B: Bus>(ctx: *mut c_void, c: u32) {
    unsafe { (*(ctx as *mut B)).gte_command(c) }
}

pub struct Cpu<B: Bus> {
    raw: *mut RawCpu,
    bus: *mut B,
}

impl<B: Bus> Cpu<B> {
    pub fn new(bus: B) -> Self {
        let bus = Box::into_raw(Box::new(bus));
        let raw = unsafe {
            r3k_new(RawBus {
                ctx: bus as *mut c_void,
                read: read_t::<B>,
                write: write_t::<B>,
                gte_read: gte_read_t::<B>,
                gte_write: gte_write_t::<B>,
                gte_command: gte_cmd_t::<B>,
            })
        };
        assert!(!raw.is_null());
        Self { raw, bus }
    }
    /// Saves or loads every register and internal flag (save states, rewind).
    pub fn sync_state(&mut self, s: &mut state::State) {
        let n = unsafe { r3k_state_size() } as usize;
        s.check(n as u32);
        let mut buf = vec![0u8; n];
        if !s.is_load() {
            unsafe { r3k_save_state(self.raw, buf.as_mut_ptr()) };
        }
        s.raw(&mut buf);
        if s.is_load() && !s.failed() {
            unsafe { r3k_load_state(self.raw, buf.as_ptr()) };
        }
    }
    /// Lets the core read and write main RAM directly. `ram` must point at 2 MB that stays
    /// at the same address for the life of the CPU.
    ///
    /// # Safety
    /// The pointer must stay valid and never be resized or moved.
    pub unsafe fn set_ram(&mut self, ram: *mut u8) {
        r3k_set_ram(self.raw, ram)
    }
    pub fn reset(&mut self) {
        unsafe { r3k_reset(self.raw) }
    }
    #[inline]
    pub fn step(&mut self) -> u32 {
        unsafe { r3k_step(self.raw) }
    }
    pub fn set_irq(&mut self, on: bool) {
        unsafe { r3k_set_irq(self.raw, on as i32) }
    }
    pub fn regs(&self) -> Regs {
        let mut r = Regs::default();
        unsafe { r3k_get_regs(self.raw, &mut r) };
        r
    }
    pub fn set_regs(&mut self, r: Regs) {
        unsafe { r3k_set_regs(self.raw, &r) }
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
            r3k_free(self.raw);
            drop(Box::from_raw(self.bus));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flat 1 MiB RAM, all segments mirrored onto it.
    struct Ram(Vec<u8>);
    impl Bus for Ram {
        fn read(&mut self, a: u32, size: u32) -> u32 {
            let a = (a & 0xFFFFF) as usize;
            (0..size as usize).fold(0, |v, i| v | (self.0[a + i] as u32) << (8 * i))
        }
        fn write(&mut self, a: u32, v: u32, size: u32) {
            let a = (a & 0xFFFFF) as usize;
            for i in 0..size as usize {
                self.0[a + i] = (v >> (8 * i)) as u8;
            }
        }
        fn gte_read(&mut self, _: u32) -> u32 {
            0
        }
        fn gte_write(&mut self, _: u32, _: u32) {}
        fn gte_command(&mut self, _: u32) {}
    }

    fn addiu(rt: u32, rs: u32, imm: u16) -> u32 {
        9 << 26 | rs << 21 | rt << 16 | imm as u32
    }
    fn addu(rd: u32, rs: u32, rt: u32) -> u32 {
        rs << 21 | rt << 16 | rd << 11 | 0x21
    }
    fn add(rd: u32, rs: u32, rt: u32) -> u32 {
        rs << 21 | rt << 16 | rd << 11 | 0x20
    }
    fn mem(op: u32, rt: u32, base: u32, off: u16) -> u32 {
        op << 26 | base << 21 | rt << 16 | off as u32
    }
    fn lui(rt: u32, imm: u16) -> u32 {
        0x0F << 26 | rt << 16 | imm as u32
    }

    fn run(prog: &[u32], steps: usize, setup: impl FnOnce(&mut Vec<u8>)) -> Cpu<Ram> {
        let mut ram = vec![0u8; 0x100000];
        for (i, w) in prog.iter().enumerate() {
            ram[0x1000 + i * 4..0x1004 + i * 4].copy_from_slice(&w.to_le_bytes());
        }
        setup(&mut ram);
        let mut cpu = Cpu::new(Ram(ram));
        cpu.reset();
        let mut r = cpu.regs();
        r.pc = 0x1000;
        r.next_pc = 0x1004;
        r.sr = 0; // BEV clear: exceptions go to 0x80000080
        cpu.set_regs(r);
        for _ in 0..steps {
            cpu.step();
        }
        cpu
    }

    #[test]
    fn load_delay_slot() {
        let prog = [
            addiu(2, 0, 0x55),
            mem(0x2B, 2, 0, 0x100), // sw r2, 0x100
            addiu(3, 0, 1),
            mem(0x23, 3, 0, 0x100), // lw r3, 0x100  (delayed)
            addu(4, 3, 0),          // still sees r3 = 1
            addu(5, 3, 0),          // sees 0x55
        ];
        let r = run(&prog, 6, |_| {}).regs();
        assert_eq!((r.r[4], r.r[5]), (1, 0x55));
    }

    #[test]
    fn branch_delay_slot_and_link() {
        // beq r0,r0,+3 ; addiu r2,r2,1 (delay slot runs) ; addiu r2,r2,10 (skipped) ; x ; addiu r2,r2,100
        let prog = [addiu(2, 0, 0), 0x1000_0003, addiu(2, 2, 1), addiu(2, 2, 10), 0, addiu(2, 2, 100)];
        let r = run(&prog, 4, |_| {}).regs();
        assert_eq!(r.r[2], 101);
    }

    #[test]
    fn unaligned_lwl_lwr_merge() {
        // lwr r8,0x201 ; lwl r8,0x204 ; nop ; -> 0x55443322
        let prog = [mem(0x26, 8, 0, 0x201), mem(0x22, 8, 0, 0x204), 0, 0];
        let r = run(&prog, 4, |m| {
            m[0x200..0x208].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        })
        .regs();
        assert_eq!(r.r[8], 0x5544_3322);
    }

    #[test]
    fn overflow_exception_vectors_and_saves_epc() {
        // lui r1,0x7FFF ; ori r1,r1,0xFFFF ; addiu r2,r0,1 ; add r3,r1,r2 (overflow!)
        let prog = [lui(1, 0x7FFF), 0x0D << 26 | 1 << 21 | 1 << 16 | 0xFFFF, addiu(2, 0, 1), add(3, 1, 2)];
        let cpu = run(&prog, 6, |m| {
            // Handler at 0x80000080: addiu r7,r0,0x77
            m[0x80..0x84].copy_from_slice(&addiu(7, 0, 0x77).to_le_bytes());
        });
        let r = cpu.regs();
        assert_eq!(r.r[3], 0, "destination untouched on overflow");
        assert_eq!(r.r[7], 0x77, "handler ran");
        assert_eq!((r.cause >> 2) & 0x1F, 12, "overflow code");
        assert_eq!(r.epc, 0x100C);
    }
}
