//! Our x86 CPU core. One interpreter covers two models:
//!
//! - `Model::PentiumIII`: the original Xbox's CPU (32-bit, MMX/SSE, no SSE2).
//! - `Model::X86_64`: a 64-bit PC processor for the virtual machine (long mode, NX,
//!   SYSCALL, SSE2).
//!
//! Real mode, 16/32-bit protected mode with privilege rings, and 64-bit long mode; legacy,
//! PAE and 4-level paging with user/supervisor, read-only and no-execute protection; IDT
//! interrupts and exceptions with TSS stack switching; CPUID, RDTSC and MSRs. The x87 unit
//! lives in `fpu`, the MMX/SSE/SSE2 unit in `sse`.

pub mod fpu;
mod ops;
mod ops0f;
pub mod sse;
mod system;

use std::collections::BTreeMap;

pub trait Bus {
    fn read8(&mut self, addr: u64) -> u8;
    fn write8(&mut self, addr: u64, v: u8);
    fn read16(&mut self, a: u64) -> u16 {
        self.read8(a) as u16 | (self.read8(a.wrapping_add(1)) as u16) << 8
    }
    fn read32(&mut self, a: u64) -> u32 {
        self.read16(a) as u32 | (self.read16(a.wrapping_add(2)) as u32) << 16
    }
    fn write16(&mut self, a: u64, v: u16) {
        self.write8(a, v as u8);
        self.write8(a.wrapping_add(1), (v >> 8) as u8);
    }
    fn write32(&mut self, a: u64, v: u32) {
        self.write16(a, v as u16);
        self.write16(a.wrapping_add(2), (v >> 16) as u16);
    }
    fn io_in(&mut self, _port: u16, _size: u8) -> u32 {
        0xFFFF_FFFF
    }
    fn io_out(&mut self, _port: u16, _size: u8, _v: u32) {}
    /// Fast path: index into `ram()` for a physical address that is plain RAM.
    fn ram_index(&self, _phys: u64) -> Option<usize> {
        None
    }
    fn ram(&mut self) -> &mut [u8] {
        &mut []
    }
}

pub const EAX: usize = 0;
pub const ECX: usize = 1;
pub const EDX: usize = 2;
pub const EBX: usize = 3;
pub const ESP: usize = 4;
pub const EBP: usize = 5;
pub const ESI: usize = 6;
pub const EDI: usize = 7;

pub const ES: usize = 0;
pub const CS: usize = 1;
pub const SS: usize = 2;
pub const DS: usize = 3;
pub const FS: usize = 4;
pub const GS: usize = 5;

pub mod flags {
    pub const CF: u32 = 1;
    pub const PF: u32 = 1 << 2;
    pub const AF: u32 = 1 << 4;
    pub const ZF: u32 = 1 << 6;
    pub const SF: u32 = 1 << 7;
    pub const TF: u32 = 1 << 8;
    pub const IF: u32 = 1 << 9;
    pub const DF: u32 = 1 << 10;
    pub const OF: u32 = 1 << 11;
    pub const IOPL: u32 = 3 << 12;
    pub const NT: u32 = 1 << 14;
    pub const RF: u32 = 1 << 16;
    pub const VM: u32 = 1 << 17;
    pub const AC: u32 = 1 << 18;
    pub const ID: u32 = 1 << 21;
}
use flags::*;

pub mod msr {
    pub const TSC: u32 = 0x10;
    pub const APIC_BASE: u32 = 0x1B;
    pub const SYSENTER_CS: u32 = 0x174;
    pub const SYSENTER_ESP: u32 = 0x175;
    pub const SYSENTER_EIP: u32 = 0x176;
    pub const EFER: u32 = 0xC000_0080;
    pub const STAR: u32 = 0xC000_0081;
    pub const LSTAR: u32 = 0xC000_0082;
    pub const CSTAR: u32 = 0xC000_0083;
    pub const SFMASK: u32 = 0xC000_0084;
    pub const FS_BASE: u32 = 0xC000_0100;
    pub const GS_BASE: u32 = 0xC000_0101;
    pub const KERNEL_GS_BASE: u32 = 0xC000_0102;
    pub const TSC_AUX: u32 = 0xC000_0103;
}

pub const EFER_SCE: u64 = 1;
pub const EFER_LME: u64 = 1 << 8;
pub const EFER_LMA: u64 = 1 << 10;
pub const EFER_NXE: u64 = 1 << 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    PentiumIII,
    X86_64,
}

/// An exception: vector and optional error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ex(pub u8, pub Option<u32>);
type R<T> = Result<T, Ex>;

const UD: Ex = Ex(6, None);
const DE: Ex = Ex(0, None);
fn gp(code: u32) -> Ex {
    Ex(13, Some(code))
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Reg(u8),
    Mem(usize, u64),
}

/// Kind of memory access, for page protection checks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Acc {
    Read,
    Write,
    Exec,
}

/// One cached translation.
#[derive(Clone, Copy, Default)]
struct Tlb {
    /// Virtual page number + 1 (0 = empty).
    tag: u64,
    page: u64,
    writable: bool,
    user: bool,
    nx: bool,
    /// Dirty bit already set in the page tables.
    dirty: bool,
}

pub struct Cpu {
    pub model: Model,
    pub r: [u64; 16],
    pub rip: u64,
    pub eflags: u32,
    pub sel: [u16; 6],
    pub base: [u64; 6],
    /// Code segment is 32-bit (D bit).
    pub cs32: bool,
    /// Code segment is 64-bit (L bit, in long mode).
    pub code64: bool,
    /// Stack segment is 32-bit (B bit).
    pub ss32: bool,
    /// Current privilege level.
    pub cpl: u8,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
    pub gdtr: (u64, u16),
    pub idtr: (u64, u16),
    /// Task register: selector and base.
    pub tr: (u16, u64),
    /// Local descriptor table: selector, base, limit.
    pub ldtr: (u16, u64, u32),
    pub msr: BTreeMap<u32, u64>,
    pub halted: bool,
    pub tsc: u64,
    /// Set by the emulator-call opcode `0F FF nn` (used by high-level BIOS stubs); the
    /// machine handles call `nn` after the step and clears it.
    pub trap: Option<u8>,
    /// Why the CPU shut down (triple fault), for diagnostics.
    pub shutdown: Option<String>,
    pub fpu: fpu::Fpu,
    pub simd: sse::Simd,
    tlb: Box<[Tlb; TLB_SIZE]>,
    /// Instruction fetch cache: linear page + 1 (0 = empty) and its index in `Bus::ram()`.
    fpage: u64,
    fidx: usize,
    // Per-instruction decode state.
    osz: u32,
    asz: u32,
    rex: u8,
    seg: Option<usize>,
    rep: u8,
    lock: bool,
    /// Operand-size prefix seen (selects SSE2 forms).
    p66: bool,
    /// Opcode being decoded (map 0 = one byte, 1 = 0F) for RIP-relative immediates.
    cur_op: (u8, u32),
    start: u64,
    /// Blocks interrupts for one instruction after STI / MOV SS.
    shadow: bool,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

const TLB_SIZE: usize = 1024;

pub(crate) fn mask(sz: u32) -> u64 {
    match sz {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => u64::MAX,
    }
}
pub(crate) fn sign(sz: u32) -> u64 {
    1 << (sz * 8 - 1)
}
/// Sign-extends the low `sz` bytes of `v` to 64 bits.
pub(crate) fn sx(v: u64, sz: u32) -> u64 {
    match sz {
        1 => v as u8 as i8 as i64 as u64,
        2 => v as u16 as i16 as i64 as u64,
        4 => v as u32 as i32 as i64 as u64,
        _ => v,
    }
}

impl Cpu {
    /// A Pentium III at power-on: real mode, executing at F000:FFF0.
    pub fn new() -> Self {
        Self::with_model(Model::PentiumIII)
    }

    /// A 64-bit PC processor at power-on.
    pub fn new_x64() -> Self {
        Self::with_model(Model::X86_64)
    }

    pub fn with_model(model: Model) -> Self {
        let mut c = Cpu {
            model,
            r: [0; 16],
            rip: 0xFFF0,
            eflags: 2,
            sel: [0; 6],
            base: [0; 6],
            cs32: false,
            code64: false,
            ss32: false,
            cpl: 0,
            cr0: 0x6000_0010,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            efer: 0,
            gdtr: (0, 0xFFFF),
            idtr: (0, 0x3FF),
            tr: (0, 0),
            ldtr: (0, 0, 0),
            msr: BTreeMap::new(),
            halted: false,
            tsc: 0,
            trap: None,
            shutdown: None,
            fpu: fpu::Fpu::default(),
            simd: sse::Simd::default(),
            tlb: Box::new([Tlb::default(); TLB_SIZE]),
            fpage: 0,
            fidx: 0,
            osz: 2,
            asz: 2,
            rex: 0,
            seg: None,
            rep: 0,
            lock: false,
            p66: false,
            cur_op: (0, 0),
            start: 0,
            shadow: false,
        };
        c.sel[CS] = 0xF000;
        c.base[CS] = 0xFFFF_0000;
        c.r[EDX] = match model {
            Model::PentiumIII => 0x0683,
            Model::X86_64 => 0x06F6,
        };
        c
    }

    pub fn protected(&self) -> bool {
        self.cr0 & 1 != 0
    }
    pub fn long_mode(&self) -> bool {
        self.efer & EFER_LMA != 0
    }
    pub fn v86(&self) -> bool {
        self.eflags & VM != 0
    }
    /// Instruction pointer as a 32-bit value (for 16/32-bit code).
    pub fn eip(&self) -> u32 {
        self.rip as u32
    }

    // ------------------------------------------------------------ memory

    /// Drops all cached translations (CR0/CR3/CR4 writes, INVLPG).
    pub fn flush_tlb(&mut self) {
        self.tlb.fill(Tlb::default());
        self.fpage = 0;
    }

    /// Linear -> physical, with protection checks.
    #[inline]
    fn translate<B: Bus>(&mut self, bus: &mut B, lin: u64, acc: Acc) -> R<u64> {
        if self.cr0 & 0x8000_0000 == 0 {
            return Ok(lin as u32 as u64);
        }
        let vpn = lin >> 12;
        let e = self.tlb[vpn as usize % TLB_SIZE];
        if e.tag == vpn + 1 && self.allowed(&e, acc) && (acc != Acc::Write || e.dirty) {
            return Ok(e.page | (lin & 0xFFF));
        }
        let e = self.walk(bus, lin, acc)?;
        self.tlb[vpn as usize % TLB_SIZE] = e;
        Ok(e.page | (lin & 0xFFF))
    }

    fn allowed(&self, e: &Tlb, acc: Acc) -> bool {
        let user = self.cpl == 3;
        if user && !e.user {
            return false;
        }
        match acc {
            Acc::Write => e.writable || (!user && self.cr0 & (1 << 16) == 0),
            Acc::Exec => !e.nx,
            Acc::Read => true,
        }
    }

    fn page_fault(&mut self, lin: u64, acc: Acc, present: bool) -> Ex {
        self.cr2 = lin;
        let mut code = present as u32;
        if acc == Acc::Write {
            code |= 2;
        }
        if self.cpl == 3 {
            code |= 4;
        }
        if acc == Acc::Exec && self.efer & EFER_NXE != 0 {
            code |= 16;
        }
        Ex(14, Some(code))
    }

    /// Walks the page tables (legacy, PAE or 4-level) and sets accessed/dirty bits.
    fn walk<B: Bus>(&mut self, bus: &mut B, lin: u64, acc: Acc) -> R<Tlb> {
        let write = acc == Acc::Write;
        let nxe = self.efer & EFER_NXE != 0;
        let (mut rw, mut us, mut nx) = (true, true, false);
        let touch = |bus: &mut B, a: u64, e: u64, wide: bool, leaf: bool| {
            let want = 0x20 | if leaf && write { 0x40 } else { 0 };
            if e & want != want {
                if wide {
                    bus.write32(a, (e | want) as u32);
                } else {
                    bus.write32(a, (e | want) as u32);
                }
            }
        };
        let rd64 = |bus: &mut B, a: u64| bus.read32(a) as u64 | (bus.read32(a.wrapping_add(4)) as u64) << 32;

        if self.cr4 & 0x20 == 0 {
            // Legacy 32-bit paging.
            let lin32 = lin as u32;
            let pde_a = ((self.cr3 as u32 & !0xFFF).wrapping_add((lin32 >> 22) * 4)) as u64;
            let pde = bus.read32(pde_a) as u64;
            if pde & 1 == 0 {
                return Err(self.page_fault(lin, acc, false));
            }
            rw &= pde & 2 != 0;
            us &= pde & 4 != 0;
            if pde & 0x80 != 0 && self.cr4 & 0x10 != 0 {
                let e = self.leaf(lin, rw, us, nx, (((pde as u32) & 0xFFC0_0000) | (lin32 & 0x3F_F000)) as u64, acc)?;
                touch(bus, pde_a, pde, false, true);
                return Ok(e);
            }
            touch(bus, pde_a, pde, false, false);
            let pte_a = ((pde as u32 & !0xFFF).wrapping_add(((lin32 >> 12) & 0x3FF) * 4)) as u64;
            let pte = bus.read32(pte_a) as u64;
            if pte & 1 == 0 {
                return Err(self.page_fault(lin, acc, false));
            }
            rw &= pte & 2 != 0;
            us &= pte & 4 != 0;
            let e = self.leaf(lin, rw, us, nx, (pte as u32 & !0xFFF) as u64, acc)?;
            touch(bus, pte_a, pte, false, true);
            return Ok(e);
        }

        // PAE (3 levels) or 4-level long-mode paging, 64-bit entries.
        let addr = |e: u64| e & 0x000F_FFFF_FFFF_F000;
        let mut table;
        let levels: &[u32] = if self.long_mode() {
            let pml4_a = (self.cr3 & 0x000F_FFFF_FFFF_F000).wrapping_add(((lin >> 39) & 0x1FF) * 8);
            let e = rd64(bus, pml4_a);
            if e & 1 == 0 {
                return Err(self.page_fault(lin, acc, false));
            }
            rw &= e & 2 != 0;
            us &= e & 4 != 0;
            nx |= nxe && e >> 63 != 0;
            touch(bus, pml4_a, e, true, false);
            table = addr(e);
            &[30, 21, 12]
        } else {
            let pdpt_a = ((self.cr3 as u32 & !0x1F) as u64).wrapping_add(((lin >> 30) & 3) * 8);
            let e = rd64(bus, pdpt_a);
            if e & 1 == 0 {
                return Err(self.page_fault(lin, acc, false));
            }
            table = addr(e);
            &[21, 12]
        };
        for &shift in levels {
            let a = table.wrapping_add(((lin >> shift) & 0x1FF) * 8);
            let e = rd64(bus, a);
            if e & 1 == 0 {
                return Err(self.page_fault(lin, acc, false));
            }
            rw &= e & 2 != 0;
            us &= e & 4 != 0;
            nx |= nxe && e >> 63 != 0;
            let big = shift != 12 && e & 0x80 != 0;
            if shift == 12 || big {
                let size_mask = (1u64 << shift) - 1;
                let page = (e & 0x000F_FFFF_FFFF_F000 & !size_mask) | (lin & size_mask & !0xFFF);
                let t = self.leaf(lin, rw, us, nx, page, acc)?;
                touch(bus, a, e, true, true);
                return Ok(t);
            }
            touch(bus, a, e, true, false);
            table = addr(e);
        }
        unreachable!()
    }

    fn leaf(&mut self, lin: u64, rw: bool, us: bool, nx: bool, page: u64, acc: Acc) -> R<Tlb> {
        let t = Tlb { tag: (lin >> 12) + 1, page, writable: rw, user: us, nx, dirty: acc == Acc::Write };
        if !self.allowed(&t, acc) {
            return Err(self.page_fault(lin, acc, true));
        }
        Ok(t)
    }

    /// Reads `sz` bytes (1-8) at a linear address.
    pub fn read_lin<B: Bus>(&mut self, bus: &mut B, lin: u64, sz: u32) -> R<u64> {
        self.read_acc(bus, lin, sz, Acc::Read)
    }

    fn read_acc<B: Bus>(&mut self, bus: &mut B, lin: u64, sz: u32, acc: Acc) -> R<u64> {
        if (lin & 0xFFF) + sz as u64 <= 0x1000 {
            let p = self.translate(bus, lin, acc)?;
            if let Some(i) = bus.ram_index(p) {
                let r = bus.ram();
                if i + 8 <= r.len() {
                    let v = u64::from_le_bytes(r[i..i + 8].try_into().unwrap());
                    return Ok(v & mask(sz));
                }
            }
            return Ok(match sz {
                1 => bus.read8(p) as u64,
                2 => bus.read16(p) as u64,
                4 => bus.read32(p) as u64,
                _ => bus.read32(p) as u64 | (bus.read32(p.wrapping_add(4)) as u64) << 32,
            });
        }
        let mut v = 0;
        for i in 0..sz as u64 {
            let p = self.translate(bus, lin.wrapping_add(i), acc)?;
            v |= (bus.read8(p) as u64) << (i * 8);
        }
        Ok(v)
    }

    /// Implicit supervisor accesses: descriptor tables and the TSS are read with kernel
    /// rights whatever the current privilege level.
    pub(crate) fn read_sys<B: Bus>(&mut self, bus: &mut B, lin: u64, sz: u32) -> R<u64> {
        let cpl = std::mem::replace(&mut self.cpl, 0);
        let r = self.read_lin(bus, lin, sz);
        self.cpl = cpl;
        r
    }
    pub(crate) fn write_sys<B: Bus>(&mut self, bus: &mut B, lin: u64, sz: u32, v: u64) -> R<()> {
        let cpl = std::mem::replace(&mut self.cpl, 0);
        let r = self.write_lin(bus, lin, sz, v);
        self.cpl = cpl;
        r
    }

    pub fn write_lin<B: Bus>(&mut self, bus: &mut B, lin: u64, sz: u32, v: u64) -> R<()> {
        if (lin & 0xFFF) + sz as u64 <= 0x1000 {
            let p = self.translate(bus, lin, Acc::Write)?;
            if let Some(i) = bus.ram_index(p) {
                let r = bus.ram();
                if i + sz as usize <= r.len() {
                    r[i..i + sz as usize].copy_from_slice(&v.to_le_bytes()[..sz as usize]);
                    if self.fpage != 0 && (lin >> 12) + 1 == self.fpage {
                        // Self-modifying code stays correct: the fetch cache reads RAM directly.
                    }
                    return Ok(());
                }
            }
            match sz {
                1 => bus.write8(p, v as u8),
                2 => bus.write16(p, v as u16),
                4 => bus.write32(p, v as u32),
                _ => {
                    bus.write32(p, v as u32);
                    bus.write32(p.wrapping_add(4), (v >> 32) as u32);
                }
            }
            return Ok(());
        }
        // Translate every byte first so a fault leaves memory untouched.
        let mut ps = [0u64; 8];
        for i in 0..sz as usize {
            ps[i] = self.translate(bus, lin.wrapping_add(i as u64), Acc::Write)?;
        }
        for i in 0..sz as usize {
            bus.write8(ps[i], (v >> (i * 8)) as u8);
        }
        Ok(())
    }

    /// Linear address of `seg:off`.
    #[inline]
    fn lin(&self, seg: usize, off: u64) -> u64 {
        if self.code64 {
            if seg >= FS {
                self.base[seg].wrapping_add(off)
            } else {
                off
            }
        } else {
            self.base[seg].wrapping_add(off) & 0xFFFF_FFFF
        }
    }

    fn rd<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u64, sz: u32) -> R<u64> {
        let l = self.lin(seg, off);
        self.read_lin(bus, l, sz)
    }
    fn wr<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u64, sz: u32, v: u64) -> R<()> {
        let l = self.lin(seg, off);
        self.write_lin(bus, l, sz, v)
    }
    /// 32-bit helpers for the FPU / SIMD units.
    fn rd32<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u64, sz: u32) -> R<u32> {
        Ok(self.rd(bus, seg, off, sz)? as u32)
    }
    fn wr32<B: Bus>(&mut self, bus: &mut B, seg: usize, off: u64, sz: u32, v: u32) -> R<()> {
        self.wr(bus, seg, off, sz, v as u64)
    }

    fn mask_ip(&self, ip: u64) -> u64 {
        if self.code64 {
            ip
        } else if self.cs32 {
            ip & 0xFFFF_FFFF
        } else {
            ip & 0xFFFF
        }
    }

    #[inline]
    fn fetch<B: Bus>(&mut self, bus: &mut B, sz: u32) -> R<u64> {
        let lin = self.lin(CS, self.rip);
        let off = (lin & 0xFFF) as usize;
        let v = if off + sz as usize <= 0x1000 && self.code_page(bus, lin)? {
            let r = bus.ram();
            let i = self.fidx + off;
            match sz {
                1 => r[i] as u64,
                2 => u16::from_le_bytes([r[i], r[i + 1]]) as u64,
                4 => u32::from_le_bytes([r[i], r[i + 1], r[i + 2], r[i + 3]]) as u64,
                _ => u64::from_le_bytes(r[i..i + 8].try_into().unwrap()),
            }
        } else {
            let mut v = 0;
            for k in 0..sz as u64 {
                let p = self.translate(bus, lin.wrapping_add(k), Acc::Exec)?;
                v |= (bus.read8(p) as u64) << (k * 8);
            }
            v
        };
        self.rip = self.mask_ip(self.rip.wrapping_add(sz as u64));
        Ok(v)
    }

    /// Makes the fetch cache cover `lin`'s page; false if that page isn't plain RAM.
    #[inline]
    fn code_page<B: Bus>(&mut self, bus: &mut B, lin: u64) -> R<bool> {
        if self.fpage == (lin >> 12) + 1 {
            return Ok(true);
        }
        let p = self.translate(bus, lin & !0xFFF, Acc::Exec)?;
        match bus.ram_index(p) {
            Some(i) if i + 0x1000 <= bus.ram().len() => {
                self.fpage = (lin >> 12) + 1;
                self.fidx = i;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    #[inline]
    fn fetch8<B: Bus>(&mut self, bus: &mut B) -> R<u64> {
        self.fetch(bus, 1)
    }
    /// Immediate for an operand of `sz` bytes (64-bit operands take a sign-extended imm32).
    fn imm<B: Bus>(&mut self, bus: &mut B, sz: u32) -> R<u64> {
        if sz == 8 {
            Ok(sx(self.fetch(bus, 4)?, 4))
        } else {
            self.fetch(bus, sz)
        }
    }

    // ------------------------------------------------------------ registers

    /// Register `i` (0-15) at size `sz`. Without a REX prefix, byte registers 4-7 are
    /// AH/CH/DH/BH; with one they are SPL/BPL/SIL/DIL.
    pub fn reg(&self, i: u8, sz: u32) -> u64 {
        match sz {
            1 if i >= 4 && i < 8 && self.rex == 0 => (self.r[(i - 4) as usize] >> 8) & 0xFF,
            _ => self.r[i as usize] & mask(sz),
        }
    }
    pub fn set_reg(&mut self, i: u8, sz: u32, v: u64) {
        let r = &mut self.r;
        match sz {
            1 if i >= 4 && i < 8 && self.rex == 0 => {
                let j = (i - 4) as usize;
                r[j] = (r[j] & !0xFF00) | (v & 0xFF) << 8
            }
            1 => r[i as usize] = (r[i as usize] & !0xFF) | (v & 0xFF),
            2 => r[i as usize] = (r[i as usize] & !0xFFFF) | (v & 0xFFFF),
            // 32-bit writes zero the upper half.
            4 => r[i as usize] = v & 0xFFFF_FFFF,
            _ => r[i as usize] = v,
        }
    }

    fn get<B: Bus>(&mut self, bus: &mut B, op: Op, sz: u32) -> R<u64> {
        match op {
            Op::Reg(i) => Ok(self.reg(i, sz)),
            Op::Mem(s, o) => self.rd(bus, s, o, sz),
        }
    }
    fn set<B: Bus>(&mut self, bus: &mut B, op: Op, sz: u32, v: u64) -> R<()> {
        match op {
            Op::Reg(i) => {
                self.set_reg(i, sz, v);
                Ok(())
            }
            Op::Mem(s, o) => self.wr(bus, s, o, sz, v),
        }
    }

    fn ds(&self, default: usize) -> usize {
        self.seg.unwrap_or(default)
    }

    /// Bytes of immediate that follow the ModRM operand of the current opcode (needed to
    /// resolve RIP-relative addresses, which count from the end of the instruction).
    fn imm_after(&self, reg: u8) -> u64 {
        let (op, map) = self.cur_op;
        let full = self.osz.min(4) as u64;
        match (map, op) {
            (0, 0x69 | 0x81 | 0xC7) => full,
            (0, 0x6B | 0x80 | 0x82 | 0x83 | 0xC0 | 0xC1 | 0xC6) => 1,
            (0, 0xF6) if reg < 2 => 1,
            (0, 0xF7) if reg < 2 => full,
            (1, 0x70..=0x73 | 0xA4 | 0xAC | 0xBA | 0xC2 | 0xC4 | 0xC5 | 0xC6) => 1,
            (3, _) => 1, // 0F 3A xx: always an imm8
            _ => 0,
        }
    }

    /// Decodes a ModRM byte (plus SIB and displacement): (reg field, r/m operand).
    fn modrm<B: Bus>(&mut self, bus: &mut B) -> R<(u8, Op)> {
        let m = self.fetch8(bus)? as u8;
        let (md, reg, rm) = (m >> 6, (m >> 3) & 7 | (self.rex & 4) << 1, m & 7);
        let rex_b = (self.rex & 1) << 3;
        if md == 3 {
            return Ok((reg, Op::Reg(rm | rex_b)));
        }
        if self.asz == 2 {
            let r = |i: usize| self.r[i] & 0xFFFF;
            let (mut addr, mut def) = match rm {
                0 => (r(EBX) + r(ESI), DS),
                1 => (r(EBX) + r(EDI), DS),
                2 => (r(EBP) + r(ESI), SS),
                3 => (r(EBP) + r(EDI), SS),
                4 => (r(ESI), DS),
                5 => (r(EDI), DS),
                6 => (r(EBP), SS),
                _ => (r(EBX), DS),
            };
            if md == 0 && rm == 6 {
                addr = self.fetch(bus, 2)?;
                def = DS;
            }
            match md {
                1 => addr = addr.wrapping_add(sx(self.fetch8(bus)?, 1)),
                2 => addr = addr.wrapping_add(self.fetch(bus, 2)?),
                _ => {}
            }
            return Ok((reg, Op::Mem(self.ds(def), addr & 0xFFFF)));
        }
        let mut def = DS;
        let mut addr;
        if rm == 4 {
            let sib = self.fetch8(bus)? as u8;
            let (sc, idx, bs) = (sib >> 6, (sib >> 3) & 7 | (self.rex & 2) << 2, sib & 7);
            addr = if idx != 4 { self.r[idx as usize] << sc } else { 0 };
            if bs == 5 && md == 0 {
                addr = addr.wrapping_add(sx(self.fetch(bus, 4)?, 4));
            } else {
                addr = addr.wrapping_add(self.r[(bs | rex_b) as usize]);
                if bs == 4 || bs == 5 {
                    def = SS;
                }
            }
        } else if rm == 5 && md == 0 {
            let d = sx(self.fetch(bus, 4)?, 4);
            addr = if self.code64 {
                // RIP-relative: from the end of the whole instruction.
                self.rip.wrapping_add(d).wrapping_add(self.imm_after(reg & 7))
            } else {
                d
            };
        } else {
            addr = self.r[(rm | rex_b) as usize];
            if rm == 5 || rm == 4 {
                def = SS;
            }
        }
        match md {
            1 => addr = addr.wrapping_add(sx(self.fetch8(bus)?, 1)),
            2 => addr = addr.wrapping_add(sx(self.fetch(bus, 4)?, 4)),
            _ => {}
        }
        if self.asz == 4 {
            addr &= 0xFFFF_FFFF;
        }
        Ok((reg, Op::Mem(self.ds(def), addr)))
    }

    // ------------------------------------------------------------ stack

    /// Stack address size in bytes.
    fn stack_size(&self) -> u32 {
        if self.code64 {
            8
        } else if self.ss32 {
            4
        } else {
            2
        }
    }
    fn sp(&self) -> u64 {
        self.r[ESP] & mask(self.stack_size())
    }
    fn set_sp(&mut self, v: u64) {
        match self.stack_size() {
            8 => self.r[ESP] = v,
            4 => self.r[ESP] = v & 0xFFFF_FFFF,
            _ => self.r[ESP] = (self.r[ESP] & !0xFFFF) | (v & 0xFFFF),
        }
    }
    pub fn push<B: Bus>(&mut self, bus: &mut B, sz: u32, v: u64) -> R<()> {
        let sp = self.sp().wrapping_sub(sz as u64) & mask(self.stack_size());
        self.wr(bus, SS, sp, sz, v)?;
        self.set_sp(sp);
        Ok(())
    }
    pub fn pop<B: Bus>(&mut self, bus: &mut B, sz: u32) -> R<u64> {
        let v = self.rd(bus, SS, self.sp(), sz)?;
        self.set_sp(self.sp().wrapping_add(sz as u64));
        Ok(v)
    }
    /// Operand size of push/pop/near branches: 64-bit by default in 64-bit mode.
    fn stack_osz(&self) -> u32 {
        if self.code64 {
            if self.p66 {
                2
            } else {
                8
            }
        } else {
            self.osz
        }
    }

    // ------------------------------------------------------------ flags

    fn szp(&mut self, v: u64, sz: u32) {
        let v = v & mask(sz);
        self.eflags &= !(ZF | SF | PF);
        if v == 0 {
            self.eflags |= ZF;
        }
        if v & sign(sz) != 0 {
            self.eflags |= SF;
        }
        if (v as u8).count_ones() % 2 == 0 {
            self.eflags |= PF;
        }
    }
    fn setf(&mut self, f: u32, on: bool) {
        if on {
            self.eflags |= f
        } else {
            self.eflags &= !f
        }
    }
    fn cf(&self) -> u64 {
        (self.eflags & CF) as u64
    }

    /// The 8 classic ALU operations: add or adc sbb and sub xor cmp.
    fn alu(&mut self, op: u32, a: u64, b: u64, sz: u32) -> u64 {
        let (m, s) = (mask(sz), sign(sz));
        let (a, b) = (a & m, b & m);
        let r = match op {
            0 | 2 => {
                let c = if op == 2 { self.cf() } else { 0 };
                let r = a as u128 + b as u128 + c as u128;
                let res = r as u64 & m;
                self.setf(CF, r > m as u128);
                self.setf(OF, (a ^ res) & (b ^ res) & s != 0);
                self.setf(AF, (a ^ b ^ res) & 0x10 != 0);
                res
            }
            3 | 5 | 7 => {
                let c = if op == 3 { self.cf() } else { 0 };
                let res = a.wrapping_sub(b).wrapping_sub(c) & m;
                self.setf(CF, (b as u128 + c as u128) > a as u128);
                self.setf(OF, (a ^ b) & (a ^ res) & s != 0);
                self.setf(AF, (a ^ b ^ res) & 0x10 != 0);
                res
            }
            _ => {
                let res = match op {
                    1 => a | b,
                    4 => a & b,
                    _ => a ^ b,
                };
                self.eflags &= !(CF | OF | AF);
                res
            }
        };
        self.szp(r, sz);
        r
    }

    fn cond(&self, c: u32) -> bool {
        let f = self.eflags;
        let (cf, zf, sf, of, pf) = (f & CF != 0, f & ZF != 0, f & SF != 0, f & OF != 0, f & PF != 0);
        let r = match c >> 1 {
            0 => of,
            1 => cf,
            2 => zf,
            3 => cf || zf,
            4 => sf,
            5 => pf,
            6 => sf != of,
            _ => zf || sf != of,
        };
        r != (c & 1 != 0)
    }

    /// Group 2 shifts and rotates.
    fn shift(&mut self, op: u8, v: u64, cnt: u64, sz: u32) -> u64 {
        let (m, bits) = (mask(sz), sz as u64 * 8);
        let cnt = cnt & if sz == 8 { 0x3F } else { 0x1F };
        if cnt == 0 {
            return v & m;
        }
        let v = v & m;
        let r = match op {
            0 => {
                let c = cnt % bits;
                let r = if c == 0 { v } else { ((v << c) | (v >> (bits - c))) & m };
                self.setf(CF, r & 1 != 0);
                self.setf(OF, ((r >> (bits - 1)) ^ r) & 1 != 0);
                return r;
            }
            1 => {
                let c = cnt % bits;
                let r = if c == 0 { v } else { ((v >> c) | (v << (bits - c))) & m };
                self.setf(CF, r & sign(sz) != 0);
                self.setf(OF, ((r >> (bits - 1)) ^ (r >> (bits - 2))) & 1 != 0);
                return r;
            }
            2 | 3 => {
                let c = cnt % (bits + 1);
                let mut r = v as u128;
                let mut carry = self.cf() as u128;
                for _ in 0..c {
                    if op == 2 {
                        let out = (r >> (bits - 1)) & 1;
                        r = ((r << 1) | carry) & m as u128;
                        carry = out;
                    } else {
                        let out = r & 1;
                        r = (r >> 1) | carry << (bits - 1);
                        carry = out;
                    }
                }
                let r = r as u64;
                self.setf(CF, carry != 0);
                if op == 2 {
                    self.setf(OF, ((r >> (bits - 1)) & 1) as u128 != carry);
                } else {
                    self.setf(OF, ((r >> (bits - 1)) ^ (r >> (bits - 2))) & 1 != 0);
                }
                return r;
            }
            4 | 6 => {
                let r = if cnt >= bits { 0 } else { (v << cnt) & m };
                self.setf(CF, cnt <= bits && (v >> (bits - cnt)) & 1 != 0);
                self.setf(OF, ((r >> (bits - 1)) & 1 != 0) != (self.eflags & CF != 0));
                r
            }
            5 => {
                let r = if cnt >= bits { 0 } else { v >> cnt };
                self.setf(CF, cnt <= bits && (v >> (cnt - 1)) & 1 != 0);
                self.setf(OF, v & sign(sz) != 0);
                r
            }
            _ => {
                let s = sx(v, sz) as i64;
                let c = cnt.min(bits - 1);
                let r = (s >> c) as u64 & m;
                self.setf(CF, (s >> (cnt.min(bits) - 1)) & 1 != 0);
                self.setf(OF, false);
                r
            }
        };
        self.szp(r, sz);
        r
    }

    // ------------------------------------------------------------ execution

    /// Runs one instruction (or delivers the exception it raised).
    pub fn step<B: Bus>(&mut self, bus: &mut B) {
        self.tsc += 1;
        if self.halted {
            return;
        }
        self.shadow = false;
        self.start = self.rip;
        if let Err(e) = self.exec(bus) {
            self.rip = self.start;
            self.fault(bus, e);
        }
    }

    fn exec<B: Bus>(&mut self, bus: &mut B) -> R<()> {
        let (o, a) = if self.code64 {
            (4, 8)
        } else if self.cs32 {
            (4, 4)
        } else {
            (2, 2)
        };
        self.osz = o;
        self.asz = a;
        self.seg = None;
        self.rep = 0;
        self.lock = false;
        self.p66 = false;
        self.rex = 0;
        let mut op;
        loop {
            op = self.fetch8(bus)? as u8;
            // A legacy prefix after REX cancels the REX.
            match op {
                0x26 => self.seg = Some(ES),
                0x2E => self.seg = Some(CS),
                0x36 => self.seg = Some(SS),
                0x3E => self.seg = Some(DS),
                0x64 => self.seg = Some(FS),
                0x65 => self.seg = Some(GS),
                0x66 => {
                    self.osz = if o == 2 { 4 } else { 2 };
                    self.p66 = true;
                }
                0x67 => {
                    self.asz = match a {
                        8 => 4,
                        4 => 2,
                        _ => 4,
                    }
                }
                0xF0 => self.lock = true,
                0xF2 | 0xF3 => self.rep = op,
                // REX (64-bit mode only).
                0x40..=0x4F if self.code64 => {
                    self.rex = op;
                    continue;
                }
                _ => break,
            }
            self.rex = 0;
        }
        if self.rex & 8 != 0 {
            self.osz = 8;
        }
        self.cur_op = (op, 0);
        if self.code64 && matches!(op, 0xC4 | 0xC5) {
            return self.exec_vex(bus, op);
        }
        self.exec_op(bus, op)
    }

    fn jump_rel(&mut self, d: u64) {
        let ip = self.rip.wrapping_add(d);
        self.rip = match self.stack_osz() {
            2 => ip & 0xFFFF,
            4 => ip & 0xFFFF_FFFF,
            _ => ip,
        };
    }
}

#[cfg(test)]
mod tests;
