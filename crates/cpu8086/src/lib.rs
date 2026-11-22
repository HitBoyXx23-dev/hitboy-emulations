//! Safe Rust wrapper around the C++ 8086/80186 interpreter core.

use std::ffi::c_void;

#[repr(C)]
pub struct RawX86 {
    _private: [u8; 0],
}

pub const MEM_SIZE: usize = 0x10_0000;

pub mod flags {
    pub const CF: u16 = 1;
    pub const PF: u16 = 4;
    pub const AF: u16 = 16;
    pub const ZF: u16 = 64;
    pub const SF: u16 = 128;
    pub const IF: u16 = 512;
    pub const DF: u16 = 1024;
    pub const OF: u16 = 2048;
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub ax: u16,
    pub cx: u16,
    pub dx: u16,
    pub bx: u16,
    pub sp: u16,
    pub bp: u16,
    pub si: u16,
    pub di: u16,
    pub es: u16,
    pub cs: u16,
    pub ss: u16,
    pub ds: u16,
    pub ip: u16,
    pub flags: u16,
}

impl Regs {
    pub fn ah(&self) -> u8 {
        (self.ax >> 8) as u8
    }
    pub fn al(&self) -> u8 {
        self.ax as u8
    }
    pub fn bh(&self) -> u8 {
        (self.bx >> 8) as u8
    }
    pub fn bl(&self) -> u8 {
        self.bx as u8
    }
    pub fn ch(&self) -> u8 {
        (self.cx >> 8) as u8
    }
    pub fn cl(&self) -> u8 {
        self.cx as u8
    }
    pub fn dh(&self) -> u8 {
        (self.dx >> 8) as u8
    }
    pub fn dl(&self) -> u8 {
        self.dx as u8
    }
    pub fn set_ah(&mut self, v: u8) {
        self.ax = (self.ax & 0xFF) | ((v as u16) << 8);
    }
    pub fn set_al(&mut self, v: u8) {
        self.ax = (self.ax & 0xFF00) | v as u16;
    }
    pub fn set_bh(&mut self, v: u8) {
        self.bx = (self.bx & 0xFF) | ((v as u16) << 8);
    }
    pub fn set_bl(&mut self, v: u8) {
        self.bx = (self.bx & 0xFF00) | v as u16;
    }
    pub fn set_ch(&mut self, v: u8) {
        self.cx = (self.cx & 0xFF) | ((v as u16) << 8);
    }
    pub fn set_cl(&mut self, v: u8) {
        self.cx = (self.cx & 0xFF00) | v as u16;
    }
    pub fn set_dh(&mut self, v: u8) {
        self.dx = (self.dx & 0xFF) | ((v as u16) << 8);
    }
    pub fn set_dl(&mut self, v: u8) {
        self.dx = (self.dx & 0xFF00) | v as u16;
    }
}

type InFn = extern "C" fn(*mut c_void, u16, i32) -> u16;
type OutFn = extern "C" fn(*mut c_void, u16, u16, i32);
type TrapFn = extern "C" fn(*mut c_void, *mut RawX86, u8);

extern "C" {
    fn x86_new(ctx: *mut c_void, i: InFn, o: OutFn, t: TrapFn) -> *mut RawX86;
    fn x86_free(cpu: *mut RawX86);
    fn x86_mem(cpu: *mut RawX86) -> *mut u8;
    fn x86_get_regs(cpu: *const RawX86, out: *mut Regs);
    fn x86_set_regs(cpu: *mut RawX86, regs: *const Regs);
    fn x86_irq(cpu: *mut RawX86, vector: u8);
    fn x86_halted(cpu: *const RawX86) -> i32;
    fn x86_run(cpu: *mut RawX86, max: u32) -> u32;
    fn x86_stop(cpu: *mut RawX86);
}

/// Access to CPU state from inside a callback, or from the owner between runs.
pub struct CpuRef {
    raw: *mut RawX86,
}

impl CpuRef {
    pub fn regs(&self) -> Regs {
        let mut r = Regs::default();
        unsafe { x86_get_regs(self.raw, &mut r) };
        r
    }

    pub fn set_regs(&mut self, r: &Regs) {
        unsafe { x86_set_regs(self.raw, r) }
    }

    pub fn mem(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(x86_mem(self.raw), MEM_SIZE) }
    }

    pub fn mem_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(x86_mem(self.raw), MEM_SIZE) }
    }

    pub fn lin(seg: u16, off: u16) -> usize {
        (((seg as usize) << 4) + off as usize) & 0xF_FFFF
    }

    pub fn read8(&self, seg: u16, off: u16) -> u8 {
        self.mem()[Self::lin(seg, off)]
    }

    pub fn read16(&self, seg: u16, off: u16) -> u16 {
        self.read8(seg, off) as u16 | (self.read8(seg, off.wrapping_add(1)) as u16) << 8
    }

    pub fn write8(&mut self, seg: u16, off: u16, v: u8) {
        self.mem_mut()[Self::lin(seg, off)] = v;
    }

    pub fn write16(&mut self, seg: u16, off: u16, v: u16) {
        self.write8(seg, off, v as u8);
        self.write8(seg, off.wrapping_add(1), (v >> 8) as u8);
    }

    /// Inside a BIOS trap reached via INT: changes flags that IRET will restore.
    pub fn set_return_flag(&mut self, flag: u16, on: bool) {
        let r = self.regs();
        let off = r.sp.wrapping_add(4);
        let f = self.read16(r.ss, off);
        self.write16(r.ss, off, if on { f | flag } else { f & !flag });
    }

    /// Inside a BIOS trap reached via `INT n` (CD nn): re-execute that INT after IRET.
    pub fn retry_int(&mut self) {
        let r = self.regs();
        let ip = self.read16(r.ss, r.sp);
        self.write16(r.ss, r.sp, ip.wrapping_sub(2));
    }

    pub fn stop(&mut self) {
        unsafe { x86_stop(self.raw) }
    }
}

/// Machine-side hooks the CPU calls into.
pub trait Host {
    fn io_in(&mut self, port: u16, word: bool) -> u16;
    fn io_out(&mut self, port: u16, value: u16, word: bool);
    fn bios(&mut self, cpu: &mut CpuRef, n: u8);
}

extern "C" fn in_tramp<H: Host>(ctx: *mut c_void, port: u16, word: i32) -> u16 {
    // SAFETY: ctx is the Box<H> pointer owned by Cpu; only used while running.
    unsafe { (*(ctx as *mut H)).io_in(port, word != 0) }
}

extern "C" fn out_tramp<H: Host>(ctx: *mut c_void, port: u16, value: u16, word: i32) {
    unsafe { (*(ctx as *mut H)).io_out(port, value, word != 0) }
}

extern "C" fn trap_tramp<H: Host>(ctx: *mut c_void, raw: *mut RawX86, n: u8) {
    let mut cpu = CpuRef { raw };
    unsafe { (*(ctx as *mut H)).bios(&mut cpu, n) }
}

pub struct Cpu<H: Host> {
    cpu: CpuRef,
    host: *mut H,
}

impl<H: Host> Cpu<H> {
    pub fn new(host: H) -> Self {
        let host = Box::into_raw(Box::new(host));
        let raw = unsafe { x86_new(host as *mut c_void, in_tramp::<H>, out_tramp::<H>, trap_tramp::<H>) };
        assert!(!raw.is_null());
        Self { cpu: CpuRef { raw }, host }
    }

    pub fn state(&mut self) -> &mut CpuRef {
        &mut self.cpu
    }

    /// Host and CPU at once (e.g. to let the host poke guest memory).
    pub fn split(&mut self) -> (&mut H, &mut CpuRef) {
        (unsafe { &mut *self.host }, &mut self.cpu)
    }

    pub fn host(&self) -> &H {
        unsafe { &*self.host }
    }

    pub fn host_mut(&mut self) -> &mut H {
        unsafe { &mut *self.host }
    }

    pub fn irq(&mut self, vector: u8) {
        unsafe { x86_irq(self.cpu.raw, vector) }
    }

    pub fn halted(&self) -> bool {
        unsafe { x86_halted(self.cpu.raw) != 0 }
    }

    pub fn run(&mut self, max: u32) -> u32 {
        unsafe { x86_run(self.cpu.raw, max) }
    }
}

impl<H: Host> Drop for Cpu<H> {
    fn drop(&mut self) {
        unsafe {
            x86_free(self.cpu.raw);
            drop(Box::from_raw(self.host));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Null {
        traps: Vec<u8>,
        out: Vec<(u16, u16)>,
    }
    impl Host for Null {
        fn io_in(&mut self, _: u16, _: bool) -> u16 {
            0xFF
        }
        fn io_out(&mut self, port: u16, value: u16, _: bool) {
            self.out.push((port, value));
        }
        fn bios(&mut self, _: &mut CpuRef, n: u8) {
            self.traps.push(n);
        }
    }

    fn run(code: &[u8]) -> Cpu<Null> {
        let mut cpu = Cpu::new(Null::default());
        let st = cpu.state();
        st.mem_mut()[0x1000..0x1000 + code.len()].copy_from_slice(code);
        let mut r = st.regs();
        r.cs = 0x0100;
        r.ip = 0;
        r.ss = 0x2000;
        r.sp = 0xFFFE;
        st.set_regs(&r);
        cpu.run(10_000);
        cpu
    }

    #[test]
    fn arithmetic_and_flags() {
        // mov ax,0xFFFF; add ax,1 -> 0, CF ZF; mov bl,0x7F; inc bl -> OF SF; hlt
        let mut cpu = run(&[0xB8, 0xFF, 0xFF, 0x05, 0x01, 0x00, 0xB3, 0x7F, 0xFE, 0xC3, 0xF4]);
        let r = cpu.state().regs();
        assert_eq!(r.ax, 0);
        assert_eq!(r.bl(), 0x80);
        assert!(r.flags & flags::OF != 0 && r.flags & flags::SF != 0);
        assert!(r.flags & flags::CF != 0, "INC keeps CF from ADD");
        assert!(cpu.halted());
    }

    #[test]
    fn mul_div_and_loop() {
        // mov ax,1000; mov cx,300; mul cx (DX:AX=300000); mov bx,7; div bx; mov cx,5; xor si,si; l: add si,2; loop l; hlt
        let mut cpu = run(&[
            0xB8, 0xE8, 0x03, 0xB9, 0x2C, 0x01, 0xF7, 0xE1, 0xBB, 0x07, 0x00, 0xF7, 0xF3, 0xB9, 0x05, 0x00, 0x31, 0xF6,
            0x83, 0xC6, 0x02, 0xE2, 0xFB, 0xF4,
        ]);
        let r = cpu.state().regs();
        assert_eq!(r.ax, (300_000 / 7) as u16);
        assert_eq!(r.dx, (300_000 % 7) as u16);
        assert_eq!(r.si, 10);
        assert_eq!(r.cx, 0);
    }

    #[test]
    fn rep_movsb_and_string_compare() {
        // ds=es=cs; si=0x20; di=0x40; cx=4; cld; rep movsb; si=0x20; di=0x40; cx=4; repe cmpsb; hlt; data @0x20 "ABCD"
        let mut code = vec![
            0x8C, 0xC8, 0x8E, 0xD8, 0x8E, 0xC0, 0xBE, 0x20, 0x00, 0xBF, 0x40, 0x00, 0xB9, 0x04, 0x00, 0xFC, 0xF3, 0xA4,
            0xBE, 0x20, 0x00, 0xBF, 0x40, 0x00, 0xB9, 0x04, 0x00, 0xF3, 0xA6, 0xF4,
        ];
        code.resize(0x20, 0x90);
        code.extend_from_slice(b"ABCD");
        let mut cpu = run(&code);
        assert_eq!(&cpu.state().mem()[0x1040..0x1044], b"ABCD");
        let r = cpu.state().regs();
        assert_eq!(r.cx, 0);
        assert!(r.flags & flags::ZF != 0);
    }

    #[test]
    fn call_ret_push_pop_and_trap() {
        // call +3; hlt; (pad) ; sub: push 0x1234; pop dx; out 0x80,al; db 0F FF 42; ret
        let code = [0xE8, 0x01, 0x00, 0xF4, 0x68, 0x34, 0x12, 0x5A, 0xE6, 0x80, 0x0F, 0xFF, 0x42, 0xC3];
        let mut cpu = run(&code);
        assert_eq!(cpu.state().regs().dx, 0x1234);
        assert_eq!(cpu.host().traps, vec![0x42]);
        assert_eq!(cpu.host().out.len(), 1);
        assert_eq!(cpu.state().regs().sp, 0xFFFE);
    }

    #[test]
    fn software_interrupt_and_iret() {
        // IVT[0x21] -> 0100:0010. main: sti; int 21h; hlt. handler @0x10: mov bx,0xBEEF; iret
        let mut code = vec![0xFB, 0xCD, 0x21, 0xF4];
        code.resize(0x10, 0x90);
        code.extend_from_slice(&[0xBB, 0xEF, 0xBE, 0xCF]);
        let mut cpu = Cpu::new(Null::default());
        let st = cpu.state();
        st.mem_mut()[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        st.write16(0, 0x21 * 4, 0x0010);
        st.write16(0, 0x21 * 4 + 2, 0x0100);
        let mut r = st.regs();
        r.cs = 0x0100;
        r.ss = 0x2000;
        r.sp = 0x100;
        st.set_regs(&r);
        cpu.run(100);
        let r = cpu.state().regs();
        assert_eq!(r.bx, 0xBEEF);
        assert!(r.flags & flags::IF != 0, "IRET restores IF");
        assert_eq!(r.sp, 0x100);
    }
}
