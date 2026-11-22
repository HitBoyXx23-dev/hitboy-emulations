//! Safe Rust wrapper around the C++ SPC700 core (SNES audio CPU).

use std::ffi::c_void;

#[repr(C)]
struct RawSpc {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub psw: u8,
    pub pc: u16,
    pub stopped: u8,
}

type ReadFn = extern "C" fn(*mut c_void, u16) -> u8;
type WriteFn = extern "C" fn(*mut c_void, u16, u8);
type IdleFn = extern "C" fn(*mut c_void);

extern "C" {
    fn spc_new(ctx: *mut c_void, r: ReadFn, w: WriteFn, i: IdleFn) -> *mut RawSpc;
    fn spc_state_size() -> std::os::raw::c_ulong;
    fn spc_save_state(cpu: *const RawSpc, out: *mut u8);
    fn spc_load_state(cpu: *mut RawSpc, input: *const u8);
    fn spc_free(cpu: *mut RawSpc);
    fn spc_step(cpu: *mut RawSpc);
    fn spc_get_regs(cpu: *const RawSpc, out: *mut Regs);
    fn spc_set_regs(cpu: *mut RawSpc, r: *const Regs);
}

pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, value: u8);
    fn idle(&mut self);
}

extern "C" fn read_tramp<B: Bus>(ctx: *mut c_void, a: u16) -> u8 {
    // SAFETY: ctx is the Box<B> owned by Spc; only used during step().
    unsafe { (*(ctx as *mut B)).read(a) }
}
extern "C" fn write_tramp<B: Bus>(ctx: *mut c_void, a: u16, v: u8) {
    unsafe { (*(ctx as *mut B)).write(a, v) }
}
extern "C" fn idle_tramp<B: Bus>(ctx: *mut c_void) {
    unsafe { (*(ctx as *mut B)).idle() }
}

pub struct Spc<B: Bus> {
    raw: *mut RawSpc,
    bus: *mut B,
}

impl<B: Bus> Spc<B> {

    /// Saves or loads every register and internal flag (save states, rewind).
    pub fn sync_state(&mut self, s: &mut state::State) {
        let n = unsafe { spc_state_size() } as usize;
        // The size doubles as a check that the state came from this same build.
        s.check(n as u32);
        let mut buf = vec![0u8; n];
        if !s.is_load() {
            unsafe { spc_save_state(self.raw, buf.as_mut_ptr()) };
        }
        s.raw(&mut buf);
        if s.is_load() && !s.failed() {
            unsafe { spc_load_state(self.raw, buf.as_ptr()) };
        }
    }
    pub fn new(bus: B) -> Self {
        let bus = Box::into_raw(Box::new(bus));
        let raw = unsafe { spc_new(bus as *mut c_void, read_tramp::<B>, write_tramp::<B>, idle_tramp::<B>) };
        assert!(!raw.is_null());
        Self { raw, bus }
    }
    #[inline]
    pub fn step(&mut self) {
        unsafe { spc_step(self.raw) }
    }
    pub fn regs(&self) -> Regs {
        let mut r = Regs::default();
        unsafe { spc_get_regs(self.raw, &mut r) };
        r
    }
    pub fn set_regs(&mut self, r: Regs) {
        unsafe { spc_set_regs(self.raw, &r) }
    }
    pub fn bus(&self) -> &B {
        unsafe { &*self.bus }
    }
    pub fn bus_mut(&mut self) -> &mut B {
        unsafe { &mut *self.bus }
    }
}

impl<B: Bus> Drop for Spc<B> {
    fn drop(&mut self) {
        unsafe {
            spc_free(self.raw);
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
        fn idle(&mut self) {}
    }

    fn run(code: &[u8]) -> Spc<Ram> {
        let mut ram = vec![0u8; 0x10000];
        ram[0x0200..0x0200 + code.len()].copy_from_slice(code);
        let mut spc = Spc::new(Ram(ram));
        let mut r = spc.regs();
        r.pc = 0x0200;
        spc.set_regs(r);
        for _ in 0..10_000 {
            if spc.regs().stopped != 0 {
                return spc;
            }
            spc.step();
        }
        panic!("program did not STOP");
    }

    #[test]
    fn arithmetic_and_memory() {
        // MOV A,#$27; CLRC; ADC A,#$19 (=$40); MOV $10,A; MOV X,#$05;
        // loop: INC $10; DEC X; BNE loop; SETC; SBC A,#$01 ($3F); STOP
        let spc = run(&[0xE8, 0x27, 0x60, 0x88, 0x19, 0xC4, 0x10, 0xCD, 0x05, 0xAB, 0x10, 0x1D, 0xD0, 0xFB, 0x80, 0xA8, 0x01, 0xFF]);
        assert_eq!(spc.bus().0[0x10], 0x45);
        assert_eq!(spc.regs().a, 0x3F);
    }

    #[test]
    fn word_ops_mul_div() {
        // MOV A,#$34; MOV Y,#$12; MOVW $20,YA; INCW $20; MOVW YA,$20 (=$1235)
        // ADDW YA,$20 (=$246A); MOV $30,A; MOV $31,Y
        // MOV A,#$0C; MOV Y,#$0A; MUL YA (=120); MOV X,#$07; DIV YA,X (17 r1); STOP
        let spc = run(&[
            0xE8, 0x34, 0x8D, 0x12, 0xDA, 0x20, 0x3A, 0x20, 0xBA, 0x20, 0x7A, 0x20, 0xC4, 0x30, 0xCB, 0x31, 0xE8, 0x0C, 0x8D, 0x0A, 0xCF, 0xCD,
            0x07, 0x9E, 0xFF,
        ]);
        assert_eq!(&spc.bus().0[0x30..0x32], &[0x6A, 0x24]);
        let r = spc.regs();
        assert_eq!((r.a, r.y), (17, 1), "120 / 7 = 17 remainder 1");
    }

    #[test]
    fn bits_calls_and_stack() {
        // SET1 $40.3; CLR1 $40.0; BBS $40.3,+2 (skip MOV A,#$EE); MOV A,#$EE; CALL $0300; STOP
        // $0300: PUSH A; MOV A,#$77; MOV $41,A; POP A; RET
        let mut code = vec![0x62, 0x40, 0x12, 0x40, 0x63, 0x40, 0x02, 0xE8, 0xEE, 0x3F, 0x00, 0x03, 0xFF];
        code.resize(0x100, 0);
        code.extend_from_slice(&[0x2D, 0xE8, 0x77, 0xC4, 0x41, 0xAE, 0x6F]);
        let mut ram = vec![0u8; 0x10000];
        ram[0x40] = 0x01;
        ram[0x0200..0x0200 + code.len()].copy_from_slice(&code);
        let mut spc = Spc::new(Ram(ram));
        let mut r = spc.regs();
        r.pc = 0x0200;
        spc.set_regs(r);
        for _ in 0..1000 {
            if spc.regs().stopped != 0 {
                break;
            }
            spc.step();
        }
        assert_eq!(spc.bus().0[0x40], 0x08, "SET1 bit3, CLR1 bit0");
        assert_eq!(spc.bus().0[0x41], 0x77, "CALL ran the subroutine");
        assert_ne!(spc.regs().a, 0xEE, "BBS skipped the load");
        assert_eq!(spc.regs().sp, 0xEF, "stack balanced");
    }
}
