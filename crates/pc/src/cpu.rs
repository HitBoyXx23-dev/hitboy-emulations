//! The PC's CPU: our Pentium-class core (`cpui686`) plus the machine's RAM, with a small
//! interface for the high-level BIOS: 16-bit register views, real-mode memory helpers, and
//! BIOS traps (`0F FF nn`) routed to `Host::bios`.

pub use cpui686::flags as flags32;

/// Real-mode register file as the BIOS sees it.
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

macro_rules! byte_regs {
    ($($get:ident $set:ident $r:ident $shift:expr;)*) => {
        impl Regs {
            $(
                pub fn $get(&self) -> u8 {
                    (self.$r >> $shift) as u8
                }
                pub fn $set(&mut self, v: u8) {
                    self.$r = (self.$r & !(0xFF << $shift)) | (v as u16) << $shift;
                }
            )*
        }
    };
}
byte_regs! {
    ah set_ah ax 8; al set_al ax 0;
    bh set_bh bx 8; bl set_bl bx 0;
    ch set_ch cx 8; cl set_cl cx 0;
    dh set_dh dx 8; dl set_dl dx 0;
}

/// Machine-side hooks.
pub trait Host {
    fn io_in(&mut self, port: u16, size: u8) -> u32;
    fn io_out(&mut self, port: u16, size: u8, value: u32);
    fn bios(&mut self, cpu: &mut CpuRef, n: u8);
}

/// CPU state + RAM, as seen from a BIOS call or the machine between runs.
pub struct CpuRef {
    pub core: cpui686::Cpu,
    pub mem: Vec<u8>,
    /// Linear framebuffer of the VESA display adapter, at `VRAM_BASE`.
    pub vram: Vec<u8>,
    stopped: bool,
    pending: Vec<u8>,
}

impl CpuRef {
    pub fn regs(&self) -> Regs {
        let c = &self.core;
        let w = |i: usize| c.r[i] as u16;
        Regs {
            ax: w(0),
            cx: w(1),
            dx: w(2),
            bx: w(3),
            sp: w(4),
            bp: w(5),
            si: w(6),
            di: w(7),
            es: c.sel[cpui686::ES],
            cs: c.sel[cpui686::CS],
            ss: c.sel[cpui686::SS],
            ds: c.sel[cpui686::DS],
            ip: c.rip as u16,
            flags: c.eflags as u16,
        }
    }

    /// Writes the low 16 bits of each register (real-mode segment loads).
    pub fn set_regs(&mut self, r: &Regs) {
        let c = &mut self.core;
        for (i, v) in [r.ax, r.cx, r.dx, r.bx, r.sp, r.bp, r.si, r.di].into_iter().enumerate() {
            c.r[i] = (c.r[i] & !0xFFFF) | v as u64;
        }
        for (s, v) in [(cpui686::ES, r.es), (cpui686::CS, r.cs), (cpui686::SS, r.ss), (cpui686::DS, r.ds)] {
            c.sel[s] = v;
            c.base[s] = (v as u64) << 4;
        }
        c.cs32 = false;
        c.code64 = false;
        c.rip = r.ip as u64;
        c.eflags = (c.eflags & 0xFFFF_0000) | r.flags as u32 | 2;
    }

    pub fn mem(&self) -> &[u8] {
        &self.mem
    }
    pub fn mem_mut(&mut self) -> &mut [u8] {
        &mut self.mem
    }

    pub fn lin(seg: u16, off: u16) -> usize {
        (((seg as usize) << 4) + off as usize) & 0xF_FFFF
    }
    pub fn read8(&self, seg: u16, off: u16) -> u8 {
        self.mem[Self::lin(seg, off)]
    }
    pub fn read16(&self, seg: u16, off: u16) -> u16 {
        self.read8(seg, off) as u16 | (self.read8(seg, off.wrapping_add(1)) as u16) << 8
    }
    pub fn write8(&mut self, seg: u16, off: u16, v: u8) {
        self.mem[Self::lin(seg, off)] = v;
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

    /// Ends the current `run` early.
    pub fn stop(&mut self) {
        self.stopped = true;
    }
}

/// Physical address of the display adapter's linear framebuffer.
pub const VRAM_BASE: u32 = 0xE000_0000;
pub const VRAM_SIZE: usize = 16 << 20;

/// RAM + devices seen by the CPU core.
struct BusView<'a, H: Host> {
    mem: &'a mut Vec<u8>,
    vram: &'a mut Vec<u8>,
    host: &'a mut H,
}

impl<H: Host> cpui686::Bus for BusView<'_, H> {
    fn read8(&mut self, a: u64) -> u8 {
        let Ok(a) = u32::try_from(a) else { return 0xFF };

        if a >= VRAM_BASE {
            return self.vram.get((a - VRAM_BASE) as usize).copied().unwrap_or(0xFF);
        }
        self.mem.get(a as usize).copied().unwrap_or(0xFF)
    }
    fn write8(&mut self, a: u64, v: u8) {
        let Ok(a) = u32::try_from(a) else { return };

        if a >= VRAM_BASE {
            if let Some(b) = self.vram.get_mut((a - VRAM_BASE) as usize) {
                *b = v;
            }
            return;
        }
        // The BIOS ROM area (F0000-FFFFF) is read-only.
        if !(0xF_0000..0x10_0000).contains(&a) {
            if let Some(b) = self.mem.get_mut(a as usize) {
                *b = v;
            }
        }
    }
    fn write32(&mut self, a: u64, v: u32) {
        let Ok(a) = u32::try_from(a) else { return };

        if a >= VRAM_BASE {
            let i = (a - VRAM_BASE) as usize;
            if i + 4 <= self.vram.len() {
                self.vram[i..i + 4].copy_from_slice(&v.to_le_bytes());
            }
            return;
        }
        for k in 0..4 {
            self.write8(u64::from(a.wrapping_add(k)), (v >> (k * 8)) as u8);
        }
    }
    fn ram_index(&self, a: u64) -> Option<usize> {
        let Ok(a) = u32::try_from(a) else { return None };

        let i = a as usize;
        (i < self.mem.len() && !(0xF_0000..0x10_0000).contains(&i)).then_some(i)
    }
    fn ram(&mut self) -> &mut [u8] {
        self.mem
    }
    fn io_in(&mut self, port: u16, size: u8) -> u32 {
        self.host.io_in(port, size)
    }
    fn io_out(&mut self, port: u16, size: u8, v: u32) {
        self.host.io_out(port, size, v)
    }
}

pub struct Cpu<H: Host> {
    state: CpuRef,
    host: H,
}

impl<H: Host> Cpu<H> {
    pub fn new(host: H, ram: usize) -> Self {
        let mut core = cpui686::Cpu::new_x64();
        core.r[2] = 0; // no CPU signature in DX for a warm-started PC
        Cpu { state: CpuRef { core, mem: vec![0; ram], vram: vec![0; VRAM_SIZE], stopped: false, pending: Vec::new() }, host }
    }

    pub fn state(&mut self) -> &mut CpuRef {
        &mut self.state
    }
    pub fn split(&mut self) -> (&mut H, &mut CpuRef) {
        (&mut self.host, &mut self.state)
    }
    pub fn host(&self) -> &H {
        &self.host
    }
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// Queues an external interrupt, delivered when the CPU accepts one.
    pub fn irq(&mut self, vector: u8) {
        self.state.pending.push(vector);
        self.state.core.halted = false;
    }

    pub fn has_pending(&self) -> bool {
        !self.state.pending.is_empty()
    }

    pub fn halted(&self) -> bool {
        self.state.core.halted
    }

    /// Runs up to `max` instructions; returns how many ran.
    pub fn run(&mut self, max: u32) -> u32 {
        self.state.stopped = false;
        let mut n = 0;
        while n < max && !self.state.stopped {
            let st = &mut self.state;
            if !st.pending.is_empty() && st.core.can_interrupt() {
                let v = st.pending.remove(0);
                let mut bus = BusView { mem: &mut st.mem, vram: &mut st.vram, host: &mut self.host };
                st.core.irq(&mut bus, v);
            }
            if st.core.halted {
                break;
            }
            let mut bus = BusView { mem: &mut st.mem, vram: &mut st.vram, host: &mut self.host };
            st.core.step(&mut bus);
            n += 1;
            if let Some(t) = st.core.trap.take() {
                self.host.bios(&mut self.state, t);
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Null {
        traps: Vec<u8>,
        out: Vec<(u16, u32)>,
    }
    impl Host for Null {
        fn io_in(&mut self, _: u16, _: u8) -> u32 {
            0xFF
        }
        fn io_out(&mut self, port: u16, _: u8, value: u32) {
            self.out.push((port, value));
        }
        fn bios(&mut self, _: &mut CpuRef, n: u8) {
            self.traps.push(n);
        }
    }

    #[test]
    fn trap_int_and_iret() {
        // IVT[0x21] -> 0100:0010. main: sti; int 21h; call sub; hlt.
        // handler: mov bx,0xBEEF; iret. sub: out 0x80,al; 0F FF 42; ret
        let mut code = vec![0xFB, 0xCD, 0x21, 0xE8, 0x0E, 0x00, 0xF4];
        code.resize(0x10, 0x90);
        code.extend_from_slice(&[0xBB, 0xEF, 0xBE, 0xCF, 0xE6, 0x80, 0x0F, 0xFF, 0x42, 0xC3]);
        let mut cpu = Cpu::new(Null::default(), 1 << 20);
        let st = cpu.state();
        st.mem_mut()[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        st.write16(0, 0x21 * 4, 0x0010);
        st.write16(0, 0x21 * 4 + 2, 0x0100);
        let mut r = st.regs();
        r.cs = 0x0100;
        r.ip = 0;
        r.ss = 0x2000;
        r.sp = 0x100;
        st.set_regs(&r);
        cpu.run(100);
        let r = cpu.state().regs();
        assert_eq!(r.bx, 0xBEEF);
        assert!(r.flags & flags::IF != 0);
        assert_eq!(r.sp, 0x100);
        assert_eq!(cpu.host().traps, vec![0x42]);
        assert_eq!(cpu.host().out.len(), 1);
        assert!(cpu.halted());
    }
}
