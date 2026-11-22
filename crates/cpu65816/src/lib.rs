//! Safe Rust wrappers around the C++ SNES CPU cores: 65C816 (main) and SPC700 (audio).

pub mod spc;

use std::ffi::c_void;

#[repr(C)]
struct RawCpu {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub a: u16,
    pub x: u16,
    pub y: u16,
    pub s: u16,
    pub d: u16,
    pub pc: u16,
    pub db: u8,
    pub pb: u8,
    pub p: u8,
    pub e: u8,
    pub waiting: u8,
    pub stopped: u8,
}

type ReadFn = extern "C" fn(*mut c_void, u32) -> u8;
type WriteFn = extern "C" fn(*mut c_void, u32, u8);
type IdleFn = extern "C" fn(*mut c_void);

extern "C" {
    fn w65_new(ctx: *mut c_void, r: ReadFn, w: WriteFn, i: IdleFn) -> *mut RawCpu;
    fn w65_state_size() -> std::os::raw::c_ulong;
    fn w65_save_state(cpu: *const RawCpu, out: *mut u8);
    fn w65_load_state(cpu: *mut RawCpu, input: *const u8);
    fn w65_free(cpu: *mut RawCpu);
    fn w65_reset(cpu: *mut RawCpu);
    fn w65_step(cpu: *mut RawCpu);
    fn w65_nmi(cpu: *mut RawCpu);
    fn w65_set_irq(cpu: *mut RawCpu, asserted: i32);
    fn w65_get_regs(cpu: *const RawCpu, out: *mut Regs);
    fn w65_set_regs(cpu: *mut RawCpu, r: *const Regs);
}

/// The system side: 24-bit memory plus internal-cycle timing.
pub trait Bus {
    fn read(&mut self, addr: u32) -> u8;
    fn write(&mut self, addr: u32, value: u8);
    /// One internal CPU cycle with no bus access.
    fn idle(&mut self);
}

extern "C" fn read_tramp<B: Bus>(ctx: *mut c_void, addr: u32) -> u8 {
    // SAFETY: ctx is the Box<B> owned by Cpu; only dereferenced during step()/reset().
    unsafe { (*(ctx as *mut B)).read(addr) }
}
extern "C" fn write_tramp<B: Bus>(ctx: *mut c_void, addr: u32, v: u8) {
    unsafe { (*(ctx as *mut B)).write(addr, v) }
}
extern "C" fn idle_tramp<B: Bus>(ctx: *mut c_void) {
    unsafe { (*(ctx as *mut B)).idle() }
}

pub struct Cpu<B: Bus> {
    raw: *mut RawCpu,
    bus: *mut B,
}

impl<B: Bus> Cpu<B> {

    /// Saves or loads every register and internal flag (save states, rewind).
    pub fn sync_state(&mut self, s: &mut state::State) {
        let n = unsafe { w65_state_size() } as usize;
        // The size doubles as a check that the state came from this same build.
        s.check(n as u32);
        let mut buf = vec![0u8; n];
        if !s.is_load() {
            unsafe { w65_save_state(self.raw, buf.as_mut_ptr()) };
        }
        s.raw(&mut buf);
        if s.is_load() && !s.failed() {
            unsafe { w65_load_state(self.raw, buf.as_ptr()) };
        }
    }
    pub fn new(bus: B) -> Self {
        let bus = Box::into_raw(Box::new(bus));
        let raw = unsafe { w65_new(bus as *mut c_void, read_tramp::<B>, write_tramp::<B>, idle_tramp::<B>) };
        assert!(!raw.is_null());
        Self { raw, bus }
    }
    pub fn reset(&mut self) {
        unsafe { w65_reset(self.raw) }
    }
    #[inline]
    pub fn step(&mut self) {
        unsafe { w65_step(self.raw) }
    }
    pub fn nmi(&mut self) {
        unsafe { w65_nmi(self.raw) }
    }
    pub fn set_irq(&mut self, on: bool) {
        unsafe { w65_set_irq(self.raw, on as i32) }
    }
    pub fn regs(&self) -> Regs {
        let mut r = Regs::default();
        unsafe { w65_get_regs(self.raw, &mut r) };
        r
    }
    pub fn set_regs(&mut self, r: Regs) {
        unsafe { w65_set_regs(self.raw, &r) }
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
            w65_free(self.raw);
            drop(Box::from_raw(self.bus));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem {
        ram: Vec<u8>,
        idle: u32,
    }
    impl Bus for Mem {
        fn read(&mut self, a: u32) -> u8 {
            self.ram[a as usize]
        }
        fn write(&mut self, a: u32, v: u8) {
            self.ram[a as usize] = v;
        }
        fn idle(&mut self) {
            self.idle += 1;
        }
    }

    /// Loads `code` at 00:8000, points the reset vector there and runs until STP.
    fn run(code: &[u8]) -> Cpu<Mem> {
        let mut ram = vec![0u8; 1 << 24];
        ram[0x8000..0x8000 + code.len()].copy_from_slice(code);
        ram[0xFFFC] = 0x00;
        ram[0xFFFD] = 0x80;
        let mut cpu = Cpu::new(Mem { ram, idle: 0 });
        cpu.reset();
        for _ in 0..10_000 {
            if cpu.regs().stopped != 0 {
                return cpu;
            }
            cpu.step();
        }
        panic!("program did not reach STP");
    }

    const CLC: u8 = 0x18;
    const XCE: u8 = 0xFB;
    const STP: u8 = 0xDB;

    #[test]
    fn native_mode_16bit_arithmetic() {
        // CLC; XCE; REP #$30; LDA #$1234; CLC; ADC #$1111; STA $0010; LDX #$FFFF; INX; STP
        let cpu = run(&[CLC, XCE, 0xC2, 0x30, 0xA9, 0x34, 0x12, CLC, 0x69, 0x11, 0x11, 0x8D, 0x10, 0x00, 0xA2, 0xFF, 0xFF, 0xE8, STP]);
        let r = cpu.regs();
        assert_eq!(r.e, 0, "native mode");
        assert_eq!(r.a, 0x2345);
        assert_eq!(&cpu.bus().ram[0x10..0x12], &[0x45, 0x23]);
        assert_eq!(r.x, 0);
        assert!(r.p & 0x02 != 0, "Z after INX wraps to 0");
    }

    #[test]
    fn decimal_mode_8_and_16_bit() {
        // SED; LDA #$45; CLC; ADC #$38 (=$83); SEC; SBC #$04 (=$79); STA $20
        // CLC; XCE; REP #$20; LDA #$1234; CLC; ADC #$8766 (=$0000, C=1); STA $22; STP
        let cpu = run(&[
            0xF8, 0xA9, 0x45, CLC, 0x69, 0x38, 0x38, 0xE9, 0x04, 0x85, 0x20, CLC, XCE, 0xC2, 0x20, 0xA9, 0x34, 0x12, CLC, 0x69, 0x66, 0x87, 0x85,
            0x22, STP,
        ]);
        assert_eq!(cpu.bus().ram[0x20], 0x79);
        assert_eq!(&cpu.bus().ram[0x22..0x24], &[0x00, 0x00]);
        assert!(cpu.regs().p & 0x01 != 0, "BCD carry out of 9999");
    }

    #[test]
    fn long_calls_block_move_and_indirect() {
        // Native, 16-bit index. Put a subroutine in bank $12 and call it with JSL.
        // main: CLC; XCE; REP #$10; JSL $12:0000; ... then MVN copies 4 bytes 7E:1000 -> 7F:2000
        //       LDX #$1000; LDY #$2000; LDA #$03 (A=count-1, 8-bit A so XBA/clear high byte first)
        let mut code = vec![CLC, XCE, 0xC2, 0x10, 0x22, 0x00, 0x00, 0x12];
        code.extend_from_slice(&[0xC2, 0x20, 0xA9, 0x03, 0x00, 0xA2, 0x00, 0x10, 0xA0, 0x00, 0x20, 0x54, 0x7F, 0x7E]);
        // (dp),Y: D=$0000, pointer at $30 -> $4000, Y=$0002: LDA ($30),Y -> reads $00:4002 (DB = $7F after MVN, so set DB=0 first)
        code.extend_from_slice(&[0xE2, 0x20, 0xA9, 0x00, 0x48, 0xAB, 0xA0, 0x02, 0x00, 0xB1, 0x30, STP]);
        let mut cpu = {
            let mut ram = vec![0u8; 1 << 24];
            ram[0x8000..0x8000 + code.len()].copy_from_slice(&code);
            ram[0xFFFC] = 0x00;
            ram[0xFFFD] = 0x80;
            // Subroutine at $12:0000: LDA #$5A (8-bit A in native with M=1); STA $7E0050 (long); RTL
            ram[0x120000..0x120007].copy_from_slice(&[0xA9, 0x5A, 0x8F, 0x50, 0x00, 0x7E, 0x6B]);
            ram[0x7E1000..0x7E1004].copy_from_slice(b"SNES");
            ram[0x30] = 0x00;
            ram[0x31] = 0x40;
            ram[0x4002] = 0x99;
            let mut cpu = Cpu::new(Mem { ram, idle: 0 });
            cpu.reset();
            cpu
        };
        for _ in 0..1000 {
            if cpu.regs().stopped != 0 {
                break;
            }
            cpu.step();
        }
        let r = cpu.regs();
        assert_eq!(cpu.bus().ram[0x7E0050], 0x5A, "JSL subroutine ran in bank $12");
        assert_eq!(&cpu.bus().ram[0x7F2000..0x7F2004], b"SNES", "MVN copied the block");
        assert_eq!(r.a & 0xFF, 0x99, "(dp),Y indirect load");
        assert_eq!(r.s, 0x01FF, "stack balanced");
        assert!(cpu.bus().idle > 0, "internal cycles reported");
    }

    #[test]
    fn nmi_in_native_mode_pushes_bank_and_returns() {
        // main: CLC; XCE; loop: BRA loop.  NMI handler at $9000: INC $40; RTI
        let mut ram = vec![0u8; 1 << 24];
        ram[0x8000..0x8004].copy_from_slice(&[CLC, XCE, 0x80, 0xFE]);
        ram[0x9000..0x9003].copy_from_slice(&[0xE6, 0x40, 0x40]);
        ram[0xFFFC] = 0x00;
        ram[0xFFFD] = 0x80;
        ram[0xFFEA] = 0x00;
        ram[0xFFEB] = 0x90;
        let mut cpu = Cpu::new(Mem { ram, idle: 0 });
        cpu.reset();
        for _ in 0..10 {
            cpu.step();
        }
        for _ in 0..3 {
            cpu.nmi();
            for _ in 0..10 {
                cpu.step();
            }
        }
        assert_eq!(cpu.bus().ram[0x40], 3);
        assert_eq!(cpu.regs().s, 0x01FF);
        assert_eq!(cpu.regs().pc, 0x8002);
    }
}
