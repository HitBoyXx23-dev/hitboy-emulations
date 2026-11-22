//! Segmentation, descriptor tables, interrupts and exceptions, privilege changes, far
//! transfers, fast system calls, control registers, MSRs and CPUID.

use super::*;

impl Cpu {
    // ------------------------------------------------------------ descriptors

    /// Reads the 8-byte descriptor for `sel` from the GDT or LDT.
    pub(crate) fn descriptor<B: Bus>(&mut self, bus: &mut B, sel: u16) -> R<u64> {
        let idx = (sel & !7) as u64;
        let (base, limit) = if sel & 4 != 0 { (self.ldtr.1, self.ldtr.2) } else { (self.gdtr.0, self.gdtr.1 as u32) };
        if idx + 7 > limit as u64 {
            return Err(gp(sel as u32 & !3));
        }
        self.read_sys(bus, base.wrapping_add(idx), 8)
    }

    fn desc_base(d: u64) -> u64 {
        ((d >> 16) & 0xFF_FFFF) | ((d >> 56) & 0xFF) << 24
    }
    fn desc_limit(d: u64) -> u32 {
        let l = (d & 0xFFFF) as u32 | ((d >> 32) as u32 & 0xF_0000);
        if d & (1 << 55) != 0 {
            l << 12 | 0xFFF
        } else {
            l
        }
    }

    /// Loads a segment register (real mode: base = sel*16; protected: from the descriptor).
    pub fn load_seg<B: Bus>(&mut self, bus: &mut B, s: usize, sel: u16) -> R<()> {
        if !self.protected() || self.v86() {
            self.sel[s] = sel;
            self.base[s] = (sel as u64) << 4;
            if s == CS {
                self.cs32 = false;
                self.code64 = false;
                self.cpl = if self.v86() { 3 } else { 0 };
            }
            return Ok(());
        }
        if sel & 0xFFFC == 0 {
            // Null selector: allowed for data segments (and SS in 64-bit mode at CPL < 3).
            if s == CS || (s == SS && !(self.code64 && self.cpl < 3)) {
                return Err(gp(0));
            }
            self.sel[s] = sel;
            if s != FS && s != GS || !self.code64 {
                self.base[s] = 0;
            }
            return Ok(());
        }
        let d = self.descriptor(bus, sel)?;
        if d & (1 << 47) == 0 {
            return Err(Ex(if s == SS { 12 } else { 11 }, Some(sel as u32 & !3)));
        }
        self.sel[s] = sel;
        self.base[s] = Self::desc_base(d);
        let big = d & (1 << 54) != 0;
        match s {
            CS => {
                self.code64 = self.long_mode() && d & (1 << 53) != 0;
                self.cs32 = big && !self.code64;
                self.cpl = (sel & 3) as u8;
                self.fpage = 0;
            }
            SS => self.ss32 = big,
            _ => {}
        }
        Ok(())
    }

    /// Flat code/stack segments for SYSCALL / SYSENTER style transitions.
    fn flat_cs(&mut self, sel: u16, long: bool, cpl: u8) {
        self.sel[CS] = sel;
        self.base[CS] = 0;
        self.code64 = long;
        self.cs32 = !long;
        self.cpl = cpl;
        self.fpage = 0;
    }
    fn flat_ss(&mut self, sel: u16) {
        self.sel[SS] = sel;
        self.base[SS] = 0;
        self.ss32 = true;
    }

    /// LTR / LLDT: system descriptors are 16 bytes in long mode.
    pub(crate) fn load_system<B: Bus>(&mut self, bus: &mut B, sel: u16, tss: bool) -> R<()> {
        if sel & 0xFFFC == 0 {
            if tss {
                return Err(gp(0));
            }
            self.ldtr = (sel, 0, 0);
            return Ok(());
        }
        let d = self.descriptor(bus, sel)?;
        let mut base = Self::desc_base(d);
        if self.long_mode() {
            let hi = self.read_sys(bus, self.gdtr.0.wrapping_add((sel & !7) as u64 + 8), 4)?;
            base |= hi << 32;
        }
        if tss {
            // Mark busy.
            let a = self.gdtr.0.wrapping_add((sel & !7) as u64 + 5);
            let t = self.read_sys(bus, a, 1)?;
            self.write_sys(bus, a, 1, t | 2)?;
            self.tr = (sel, base);
        } else {
            self.ldtr = (sel, base, Self::desc_limit(d));
        }
        Ok(())
    }

    // ------------------------------------------------------------ interrupts

    /// Delivers an interrupt or exception through the IVT (real mode) or IDT.
    pub fn interrupt<B: Bus>(&mut self, bus: &mut B, vec: u8, code: Option<u32>) -> R<()> {
        self.interrupt_ex(bus, vec, code, false)
    }

    fn interrupt_ex<B: Bus>(&mut self, bus: &mut B, vec: u8, code: Option<u32>, soft: bool) -> R<()> {
        self.halted = false;
        if !self.protected() {
            let a = self.idtr.0.wrapping_add(vec as u64 * 4);
            let off = self.read_lin(bus, a, 2)?;
            let seg = self.read_lin(bus, a + 2, 2)?;
            let (f, cs, ip) = (self.eflags as u64, self.sel[CS] as u64, self.rip);
            self.push(bus, 2, f)?;
            self.push(bus, 2, cs)?;
            self.push(bus, 2, ip)?;
            self.eflags &= !(IF | TF | AC);
            self.load_seg(bus, CS, seg as u16)?;
            self.rip = off;
            return Ok(());
        }
        if self.long_mode() {
            return self.interrupt64(bus, vec, code);
        }
        let n = vec as u64 * 8;
        if n + 7 > self.idtr.1 as u64 {
            return Err(gp(n as u32 + 2));
        }
        let g = self.read_sys(bus, self.idtr.0.wrapping_add(n), 8)?;
        if g & (1 << 47) == 0 {
            return Err(Ex(11, Some(n as u32 + 2)));
        }
        let dpl = ((g >> 45) & 3) as u8;
        if soft && dpl < self.cpl {
            return Err(gp(n as u32 + 2));
        }
        let off = (g & 0xFFFF) | ((g >> 32) & 0xFFFF_0000);
        let sel = (g >> 16) as u16;
        let kind = ((g >> 40) & 0x1F) as u32;
        let gsz = if kind & 8 != 0 { 4 } else { 2 };
        let target = self.descriptor(bus, sel)?;
        let new_cpl = ((target >> 45) & 3) as u8;
        let (old_ss, old_sp, old_flags, old_cs, old_ip) = (self.sel[SS] as u64, self.r[ESP], self.eflags, self.sel[CS] as u64, self.rip);
        let was_v86 = self.v86();
        if new_cpl < self.cpl || was_v86 {
            // Inner privilege: stack from the TSS (32-bit TSS: ESP0 at +4, SS0 at +8).
            let esp0 = self.read_sys(bus, self.tr.1.wrapping_add(4 + new_cpl as u64 * 8), 4)?;
            let ss0 = self.read_sys(bus, self.tr.1.wrapping_add(8 + new_cpl as u64 * 8), 2)? as u16;
            self.eflags &= !VM;
            self.cpl = new_cpl;
            self.load_seg(bus, SS, ss0)?;
            self.r[ESP] = esp0;
            if was_v86 {
                for s in [GS, FS, DS, ES] {
                    let v = self.sel[s] as u64;
                    self.push(bus, gsz, v)?;
                    self.sel[s] = 0;
                    self.base[s] = 0;
                }
            }
            self.push(bus, gsz, old_ss)?;
            self.push(bus, gsz, old_sp)?;
        }
        self.push(bus, gsz, old_flags as u64)?;
        self.push(bus, gsz, old_cs)?;
        self.push(bus, gsz, old_ip)?;
        if let Some(c) = code {
            self.push(bus, gsz, c as u64)?;
        }
        if kind & 7 == 6 {
            self.eflags &= !IF; // interrupt gate
        }
        self.eflags &= !(TF | VM | RF | NT);
        self.load_seg(bus, CS, (sel & !3) | new_cpl as u16)?;
        self.rip = off;
        Ok(())
    }

    /// Long-mode interrupt: 16-byte gates, 64-bit frame, IST and RSP0 stacks.
    fn interrupt64<B: Bus>(&mut self, bus: &mut B, vec: u8, code: Option<u32>) -> R<()> {
        let n = vec as u64 * 16;
        if n + 15 > self.idtr.1 as u64 {
            return Err(gp(n as u32 + 2));
        }
        let lo = self.read_sys(bus, self.idtr.0.wrapping_add(n), 8)?;
        let hi = self.read_sys(bus, self.idtr.0.wrapping_add(n + 8), 8)?;
        if lo & (1 << 47) == 0 {
            return Err(Ex(11, Some(n as u32 + 2)));
        }
        let off = (lo & 0xFFFF) | ((lo >> 32) & 0xFFFF_0000) | (hi & 0xFFFF_FFFF) << 32;
        let sel = (lo >> 16) as u16;
        let ist = ((lo >> 32) & 7) as u64;
        let kind = ((lo >> 40) & 0xF) as u32;
        let target = self.descriptor(bus, sel)?;
        let new_cpl = ((target >> 45) & 3) as u8;
        let (old_ss, old_sp, old_flags, old_cs, old_ip) = (self.sel[SS] as u64, self.r[ESP], self.eflags as u64, self.sel[CS] as u64, self.rip);
        let mut rsp = old_sp;
        if ist != 0 {
            rsp = self.read_sys(bus, self.tr.1.wrapping_add(0x24 + (ist - 1) * 8), 8)?;
        } else if new_cpl < self.cpl {
            rsp = self.read_sys(bus, self.tr.1.wrapping_add(4 + new_cpl as u64 * 8), 8)?;
        }
        if new_cpl < self.cpl {
            self.sel[SS] = new_cpl as u16;
            self.base[SS] = 0;
        }
        self.cpl = new_cpl;
        // Switch to the 64-bit handler first so pushes are 8 bytes wide on a flat stack.
        self.flat_cs((sel & !3) | new_cpl as u16, target & (1 << 53) != 0, new_cpl);
        self.r[ESP] = rsp & !0xF;
        for v in [old_ss, old_sp, old_flags, old_cs, old_ip] {
            self.push(bus, 8, v)?;
        }
        if let Some(c) = code {
            self.push(bus, 8, c as u64)?;
        }
        if kind == 0xE {
            self.eflags &= !IF;
        }
        self.eflags &= !(TF | VM | RF | NT);
        self.load_seg(bus, CS, (sel & !3) | new_cpl as u16)?;
        self.rip = off;
        Ok(())
    }

    /// Whether an external interrupt would be accepted right now.
    pub fn can_interrupt(&self) -> bool {
        self.eflags & IF != 0 && !self.shadow
    }

    /// External hardware interrupt. Returns false if interrupts are masked.
    pub fn irq<B: Bus>(&mut self, bus: &mut B, vec: u8) -> bool {
        if !self.can_interrupt() {
            return false;
        }
        if let Err(e) = self.interrupt(bus, vec, None) {
            self.fault(bus, e);
        }
        true
    }

    /// Software INT n (checks the gate's privilege).
    pub(crate) fn soft_int<B: Bus>(&mut self, bus: &mut B, vec: u8) -> R<()> {
        self.interrupt_ex(bus, vec, None, true)
    }

    pub(crate) fn fault<B: Bus>(&mut self, bus: &mut B, e: Ex) {
        let at = (self.sel[CS], self.rip, self.cpl, self.r[ESP]);
        if let Err(e2) = self.interrupt(bus, e.0, e.1) {
            // Double fault; a fault while delivering that shuts the CPU down.
            let e3 = if e.0 == 8 { Some(e2) } else { self.interrupt(bus, 8, Some(0)).err() };
            if let Some(e3) = e3 {
                self.shutdown = Some(format!(
                    "shutdown: exception {:?} at {:04X}:{:X} (CPL {}, RSP {:X}); delivering it raised {:?}, then {:?}. CR2={:X} IDTR={:X}/{:X} TR base={:X}",
                    e, at.0, at.1, at.2, at.3, e2, e3, self.cr2, self.idtr.0, self.idtr.1, self.tr.1
                ));
                self.halted = true;
                self.eflags &= !IF;
            }
        }
    }

    /// IRET / IRETD / IRETQ.
    pub(crate) fn iret<B: Bus>(&mut self, bus: &mut B, osz: u32) -> R<()> {
        if !self.protected() || (self.v86() && self.eflags & IOPL == IOPL) {
            let ip = self.pop(bus, osz)?;
            let cs = self.pop(bus, osz)?;
            let f = self.pop(bus, osz)?;
            self.load_seg(bus, CS, cs as u16)?;
            self.rip = ip;
            self.set_flags(f as u32, osz);
            return Ok(());
        }
        if self.v86() {
            return Err(gp(0));
        }
        let long = self.code64;
        let ip = self.pop(bus, osz)?;
        let cs = self.pop(bus, osz)? as u16;
        let f = self.pop(bus, osz)? as u32;
        if !long && osz == 4 && f & VM != 0 && self.cpl == 0 {
            // Return to virtual-8086 mode.
            let sp = self.pop(bus, 4)?;
            let ss = self.pop(bus, 4)?;
            let mut segs = [0u64; 4];
            for v in segs.iter_mut() {
                *v = self.pop(bus, 4)?;
            }
            self.eflags = f | 2;
            self.cpl = 3;
            for (s, v) in [ES, DS, FS, GS].into_iter().zip(segs) {
                self.sel[s] = v as u16;
                self.base[s] = (v & 0xFFFF) << 4;
            }
            self.load_seg(bus, SS, ss as u16)?;
            self.load_seg(bus, CS, cs)?;
            self.r[ESP] = sp & 0xFFFF;
            self.rip = ip & 0xFFFF;
            return Ok(());
        }
        let new_cpl = (cs & 3) as u8;
        let outer = new_cpl > self.cpl || long;
        let (sp, ss) = if outer {
            (Some(self.pop(bus, osz)?), Some(self.pop(bus, osz)? as u16))
        } else {
            (None, None)
        };
        let old_cpl = self.cpl;
        self.set_flags_cpl(f, osz, old_cpl);
        self.load_seg(bus, CS, cs)?;
        self.rip = ip;
        if let (Some(sp), Some(ss)) = (sp, ss) {
            self.load_seg(bus, SS, ss).or_else(|e| if ss & 0xFFFC == 0 && self.code64 { Ok(()) } else { Err(e) })?;
            self.sel[SS] = ss;
            self.r[ESP] = sp;
            if !self.code64 && !self.ss32 {
                self.r[ESP] &= 0xFFFF;
            }
        }
        if new_cpl > old_cpl {
            // Data segments the new privilege may not use are nulled.
            for s in [ES, DS, FS, GS] {
                if self.sel[s] & 3 < new_cpl as u16 && self.sel[s] & 0xFFFC != 0 && !self.code64 {
                    self.sel[s] = 0;
                    self.base[s] = 0;
                }
            }
        }
        Ok(())
    }

    /// POPF/IRET flag update honouring CPL and IOPL.
    pub(crate) fn set_flags(&mut self, v: u32, sz: u32) {
        let cpl = self.cpl;
        self.set_flags_cpl(v, sz, cpl);
    }
    fn set_flags_cpl(&mut self, v: u32, sz: u32, cpl: u8) {
        let mut keep = VM | RF;
        if sz == 2 {
            keep |= 0xFFFF_0000;
        }
        if self.protected() {
            let iopl = (self.eflags & IOPL) >> 12;
            if cpl > 0 {
                keep |= IOPL;
            }
            if cpl as u32 > iopl {
                keep |= IF;
            }
        }
        let allowed = 0x003F_7FD5 & !keep;
        self.eflags = (self.eflags & !allowed) | (v & allowed) | 2;
    }

    /// Far CALL / JMP to `sel:off` (code segments only; call gates are not used by the
    /// systems this machine targets).
    pub(crate) fn far_transfer<B: Bus>(&mut self, bus: &mut B, sel: u16, off: u64, call: bool) -> R<()> {
        if call {
            let osz = self.osz;
            let (cs, ip) = (self.sel[CS] as u64, self.rip);
            self.push(bus, osz, cs)?;
            self.push(bus, osz, ip)?;
        }
        let cpl = self.cpl;
        self.load_seg(bus, CS, if self.protected() && !self.v86() { (sel & !3) | cpl as u16 } else { sel })?;
        self.rip = off;
        Ok(())
    }

    /// RETF n: may return to an outer privilege level.
    pub(crate) fn far_ret<B: Bus>(&mut self, bus: &mut B, n: u64) -> R<()> {
        let osz = self.osz;
        let ip = self.pop(bus, osz)?;
        let cs = self.pop(bus, osz)? as u16;
        self.set_sp(self.sp().wrapping_add(n));
        let outer = self.protected() && !self.v86() && (cs & 3) as u8 > self.cpl;
        if outer {
            let sp = self.pop(bus, osz)?;
            let ss = self.pop(bus, osz)? as u16;
            self.load_seg(bus, CS, cs)?;
            self.load_seg(bus, SS, ss)?;
            self.r[ESP] = sp.wrapping_add(n);
        } else {
            self.load_seg(bus, CS, cs)?;
        }
        self.rip = ip;
        Ok(())
    }

    // ------------------------------------------------------------ fast system calls

    pub(crate) fn syscall(&mut self) -> R<()> {
        if self.efer & EFER_SCE == 0 || !self.long_mode() {
            return Err(UD);
        }
        let star = self.msr.get(&msr::STAR).copied().unwrap_or(0);
        let cs = ((star >> 32) & 0xFFFC) as u16;
        self.r[ECX] = self.rip;
        self.r[11] = self.eflags as u64;
        let target = if self.code64 { msr::LSTAR } else { msr::CSTAR };
        self.rip = self.msr.get(&target).copied().unwrap_or(0);
        let fmask = self.msr.get(&msr::SFMASK).copied().unwrap_or(0) as u32;
        self.eflags &= !(fmask | RF);
        self.flat_cs(cs, true, 0);
        self.flat_ss(cs + 8);
        Ok(())
    }

    pub(crate) fn sysret(&mut self) -> R<()> {
        if self.efer & EFER_SCE == 0 || !self.long_mode() || self.cpl != 0 {
            return Err(if self.cpl != 0 { gp(0) } else { UD });
        }
        let star = self.msr.get(&msr::STAR).copied().unwrap_or(0);
        let base = ((star >> 48) & 0xFFFF) as u16;
        let to64 = self.rex & 8 != 0;
        if to64 {
            self.rip = self.r[ECX];
            self.flat_cs((base + 16) | 3, true, 3);
        } else {
            self.rip = self.r[ECX] & 0xFFFF_FFFF;
            self.flat_cs(base | 3, false, 3);
        }
        self.eflags = (self.r[11] as u32 & 0x003C_7FD7) | 2;
        self.flat_ss((base + 8) | 3);
        Ok(())
    }

    pub(crate) fn sysenter(&mut self) -> R<()> {
        let cs = self.msr.get(&msr::SYSENTER_CS).copied().unwrap_or(0) as u16 & 0xFFFC;
        if !self.protected() || cs == 0 {
            return Err(gp(0));
        }
        self.eflags &= !(VM | IF | RF);
        self.flat_cs(cs, self.long_mode(), 0);
        self.flat_ss(cs + 8);
        self.r[ESP] = self.msr.get(&msr::SYSENTER_ESP).copied().unwrap_or(0);
        self.rip = self.msr.get(&msr::SYSENTER_EIP).copied().unwrap_or(0);
        if !self.long_mode() {
            self.r[ESP] &= 0xFFFF_FFFF;
            self.rip &= 0xFFFF_FFFF;
        }
        Ok(())
    }

    pub(crate) fn sysexit(&mut self) -> R<()> {
        let cs = self.msr.get(&msr::SYSENTER_CS).copied().unwrap_or(0) as u16 & 0xFFFC;
        if !self.protected() || cs == 0 || self.cpl != 0 {
            return Err(gp(0));
        }
        let to64 = self.rex & 8 != 0;
        let (ccs, css) = if to64 { (cs + 32, cs + 40) } else { (cs + 16, cs + 24) };
        self.flat_cs(ccs | 3, to64, 3);
        self.flat_ss(css | 3);
        let m = if to64 { u64::MAX } else { 0xFFFF_FFFF };
        self.rip = self.r[EDX] & m;
        self.r[ESP] = self.r[ECX] & m;
        Ok(())
    }

    // ------------------------------------------------------------ control registers, MSRs

    pub(crate) fn write_cr(&mut self, cr: u8, v: u64) -> R<()> {
        match cr {
            0 => {
                let was_pg = self.cr0 & 0x8000_0000 != 0;
                self.cr0 = v | 0x10;
                let pg = v & 0x8000_0000 != 0;
                if pg && !was_pg && self.efer & EFER_LME != 0 {
                    if self.cr4 & 0x20 == 0 {
                        return Err(gp(0));
                    }
                    self.efer |= EFER_LMA;
                } else if !pg {
                    self.efer &= !EFER_LMA;
                }
            }
            2 => self.cr2 = v,
            3 => self.cr3 = v,
            4 => self.cr4 = v,
            8 => {} // task priority: no local APIC
            _ => return Err(UD),
        }
        self.flush_tlb();
        Ok(())
    }

    pub(crate) fn read_cr(&self, cr: u8) -> R<u64> {
        Ok(match cr {
            0 => self.cr0,
            2 => self.cr2,
            3 => self.cr3,
            4 => self.cr4,
            8 => 0,
            _ => return Err(UD),
        })
    }

    pub fn read_msr(&self, n: u32) -> u64 {
        match n {
            msr::TSC => self.tsc,
            msr::EFER => self.efer,
            msr::FS_BASE => self.base[FS],
            msr::GS_BASE => self.base[GS],
            // No local APIC on this machine (the PIC handles interrupts).
            msr::APIC_BASE => 0,
            _ => self.msr.get(&n).copied().unwrap_or(0),
        }
    }

    pub fn write_msr(&mut self, n: u32, v: u64) {
        match n {
            msr::TSC => self.tsc = v,
            msr::EFER => {
                let lma = self.efer & EFER_LMA;
                self.efer = (v & !EFER_LMA) | lma;
                self.flush_tlb();
            }
            msr::FS_BASE => self.base[FS] = v,
            msr::GS_BASE => self.base[GS] = v,
            _ => {
                self.msr.insert(n, v);
            }
        }
    }

    pub(crate) fn swapgs(&mut self) -> R<()> {
        if !self.code64 || self.cpl != 0 {
            return Err(UD);
        }
        let k = self.msr.get(&msr::KERNEL_GS_BASE).copied().unwrap_or(0);
        self.msr.insert(msr::KERNEL_GS_BASE, self.base[GS]);
        self.base[GS] = k;
        Ok(())
    }

    pub(crate) fn cpuid(&mut self) {
        let leaf = self.r[EAX] as u32;
        let sub = self.r[ECX] as u32;
        let _ = sub;
        let s = |b: &[u8; 4]| u32::from_le_bytes(*b);
        let (a, b, c, d): (u32, u32, u32, u32) = match self.model {
            Model::PentiumIII => match leaf {
                0 => (2, s(b"Genu"), s(b"ntel"), s(b"ineI")),
                // Family 6 model 8 (Coppermine): FPU VME DE PSE TSC MSR PAE MCE CX8 SEP MTRR
                // PGE MCA CMOV PAT PSE36 MMX FXSR SSE.
                1 => (0x0683, 0, 0, 0x0383_FBFF),
                2 => (0x0302_0101, 0, 0, 0x0C04_0843),
                _ => (0, 0, 0, 0),
            },
            Model::X86_64 => match leaf {
                0 => (1, s(b"Genu"), s(b"ntel"), s(b"ineI")),
                // Family 6 model 15: FPU DE PSE TSC MSR PAE CX8 SEP MTRR PGE CMOV PAT CLFLUSH
                // MMX FXSR SSE SSE2 (no local APIC).
                1 => (0x06F6, 0x0001_0800, 1 << 22, 0x0781_B17D),
                0x8000_0000 => (0x8000_0008, 0, 0, 0),
                // LAHF in 64-bit mode; SYSCALL, NX, long mode.
                0x8000_0001 => (0, 0, 1, (1 << 11) | (1 << 20) | (1 << 29)),
                0x8000_0002..=0x8000_0004 => {
                    let brand = b"HitBoy x86-64 Processor\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
                    let i = ((leaf - 0x8000_0002) * 16) as usize;
                    let w = |k: usize| u32::from_le_bytes(brand[i + k * 4..i + k * 4 + 4].try_into().unwrap());
                    (w(0), w(1), w(2), w(3))
                }
                // 39-bit physical, 48-bit virtual addresses.
                0x8000_0008 => (0x3027, 0, 0, 0),
                _ => (0, 0, 0, 0),
            },
        };
        self.r[EAX] = a as u64;
        self.r[EBX] = b as u64;
        self.r[ECX] = c as u64;
        self.r[EDX] = d as u64;
    }
}
