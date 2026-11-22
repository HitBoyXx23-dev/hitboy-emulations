//! 32-bit ARM iOS Mach-O loader and Darwin BSD syscall runner built on `cpuarm7`.
//!
//! Loads decrypted 32-bit ARM (`armv6`/`armv7`/`armv7s`) Mach-O segments into a paged
//! virtual address space, maps a user stack, and executes instructions with Darwin
//! `svc #0x80` BSD syscall interception (`exit`, `write`, `getpid`, `getuid`, `getgid`,
//! `mmap`, `sysctl`).

use crate::macho;
use cpuarm7::{Bus, Cpu, Regs};
use std::collections::HashMap;

const PAGE_SIZE: u32 = 0x1000;
const STACK_BASE: u32 = 0x2FF0_0000;
const STACK_SIZE: u32 = 0x0004_0000; // 256 KB
const EXIT_SENTINEL: u32 = 0xFFFF_F000;
const MMAP_START: u32 = 0x4000_0000;

#[derive(Debug, Clone)]
pub struct RunReport {
    pub arch: String,
    pub entry: u32,
    pub instructions: u64,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stop_reason: String,
}

struct IosBus {
    pages: HashMap<u32, Box<[u8; PAGE_SIZE as usize]>>,
    mmap_next: u32,
    stdout: String,
    exit_code: Option<i32>,
    stop_reason: Option<String>,
}

impl IosBus {
    fn new() -> Self {
        Self {
            pages: HashMap::new(),
            mmap_next: MMAP_START,
            stdout: String::new(),
            exit_code: None,
            stop_reason: None,
        }
    }

    fn map_range(&mut self, addr: u32, len: u32) {
        if len == 0 {
            return;
        }
        let start = addr & !(PAGE_SIZE - 1);
        let end = addr.saturating_add(len).saturating_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let mut p = start;
        while p < end {
            self.pages.entry(p / PAGE_SIZE).or_insert_with(|| Box::new([0; PAGE_SIZE as usize]));
            let Some(next) = p.checked_add(PAGE_SIZE) else { break };
            p = next;
        }
    }

    fn write_bytes(&mut self, addr: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            let a = addr.wrapping_add(i as u32);
            let page = self.pages.entry(a / PAGE_SIZE).or_insert_with(|| Box::new([0; PAGE_SIZE as usize]));
            page[(a % PAGE_SIZE) as usize] = b;
        }
    }

    fn read_bytes(&self, addr: u32, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let a = addr.wrapping_add(i as u32);
            let b = self
                .pages
                .get(&(a / PAGE_SIZE))
                .map(|p| p[(a % PAGE_SIZE) as usize])
                .unwrap_or(0);
            out.push(b);
        }
        out
    }
}

impl Bus for IosBus {
    fn read(&mut self, addr: u32, size: u32) -> u32 {
        let mut v = 0u32;
        for k in 0..size.min(4) {
            let a = addr.wrapping_add(k);
            let b = self
                .pages
                .get(&(a / PAGE_SIZE))
                .map(|p| p[(a % PAGE_SIZE) as usize])
                .unwrap_or(0);
            v |= (b as u32) << (k * 8);
        }
        v
    }

    fn write(&mut self, addr: u32, value: u32, size: u32) {
        for k in 0..size.min(4) {
            let a = addr.wrapping_add(k);
            if let Some(p) = self.pages.get_mut(&(a / PAGE_SIZE)) {
                p[(a % PAGE_SIZE) as usize] = (value >> (k * 8)) as u8;
            }
        }
    }

    fn swi(&mut self, number: u32, regs: &mut Regs) -> bool {
        // Darwin ARM32 BSD syscalls use `svc #0x80` with syscall number in r12 (or r12=0 -> indirect in r0).
        let sys = if number == 0x80 || number == 0 {
            let r12 = regs.get(12);
            if r12 == 0 { regs.get(0) } else { r12 }
        } else {
            number
        };
        match sys {
            1 => {
                // exit(status)
                let code = regs.get(0) as i32;
                self.exit_code = Some(code);
                self.stop_reason = Some(format!("Exited with code {code}"));
                true
            }
            4 => {
                // write(fd, buf, count)
                let _fd = regs.get(0);
                let buf = regs.get(1);
                let len = (regs.get(2) as usize).min(0x10000);
                let bytes = self.read_bytes(buf, len);
                self.stdout.push_str(&String::from_utf8_lossy(&bytes));
                regs.set(0, len as u32);
                true
            }
            20 => {
                // getpid()
                regs.set(0, 100);
                true
            }
            24 | 25 | 43 | 47 => {
                // getuid / geteuid / getegid / getgid -> mobile (501)
                regs.set(0, 501);
                true
            }
            197 => {
                // mmap(addr, len, ...)
                let len = regs.get(1).max(PAGE_SIZE);
                let aligned = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
                let base = self.mmap_next;
                self.mmap_next = self.mmap_next.saturating_add(aligned);
                self.map_range(base, aligned);
                regs.set(0, base);
                true
            }
            202 => {
                // sysctl -> 0
                regs.set(0, 0);
                true
            }
            other => {
                self.stop_reason = Some(format!("Stopped on unimplemented Darwin syscall #{other}"));
                true
            }
        }
    }
}

/// Loads and runs a decrypted 32-bit ARM iOS Mach-O binary for up to `max_steps` instructions.
pub fn run_macho(data: &[u8], max_steps: u64) -> Result<RunReport, String> {
    let m = macho::parse(data)?;
    let slice = m
        .slices
        .iter()
        .find(|s| matches!(s.arch.as_str(), "armv6" | "armv7" | "armv7s" | "arm"))
        .ok_or_else(|| {
            let archs: Vec<&str> = m.slices.iter().map(|s| s.arch.as_str()).collect();
            format!("No 32-bit ARM slice found (architectures: {}). Only 32-bit ARM iOS apps run on the ARM7 core today.", archs.join(", "))
        })?;
    if slice.encrypted {
        return Err("This executable is FairPlay-encrypted (App Store DRM) and cannot be run until decrypted on a device you own.".into());
    }

    let mut bus = IosBus::new();
    let mut text_base: u32 = 0x1000;
    for seg in &slice.segments {
        if seg.name == "__PAGEZERO" || seg.vmsize == 0 {
            continue;
        }
        let vm = seg.vmaddr as u32;
        if seg.name == "__TEXT" {
            text_base = vm;
        }
        let vmsize = (seg.vmsize as u32).min(64 << 20);
        bus.map_range(vm, vmsize);
        let foff = seg.fileoff as usize;
        let fsz = (seg.filesize as usize).min(vmsize as usize);
        if fsz > 0 {
            if let Some(chunk) = data.get(foff..foff.saturating_add(fsz)) {
                bus.write_bytes(vm, chunk);
            }
        }
    }
    bus.map_range(STACK_BASE, STACK_SIZE);

    let entry = if slice.entry == 0 {
        text_base
    } else if slice.entry < text_base as u64 {
        text_base.wrapping_add(slice.entry as u32)
    } else {
        slice.entry as u32
    };

    let mut cpu = Cpu::new(bus);
    cpu.reset(entry);
    cpu.set_reg(13, STACK_BASE + STACK_SIZE - 16);
    cpu.set_reg(14, EXIT_SENTINEL);

    let mut instructions = 0u64;
    for _ in 0..max_steps {
        let pc = cpu.reg(15);
        if pc == EXIT_SENTINEL || pc == EXIT_SENTINEL + 4 || pc == 0 {
            let code = cpu.reg(0) as i32;
            cpu.bus_mut().exit_code = Some(code);
            cpu.bus_mut().stop_reason = Some(format!("Returned cleanly with code {code}"));
            break;
        }
        cpu.step();
        instructions += 1;
        if cpu.bus().stop_reason.is_some() {
            break;
        }
    }

    let b = cpu.bus();
    let stop_reason = b
        .stop_reason
        .clone()
        .unwrap_or_else(|| format!("Reached step budget ({max_steps} instructions)"));
    Ok(RunReport {
        arch: slice.arch.clone(),
        entry,
        instructions,
        exit_code: b.exit_code,
        stdout: b.stdout.clone(),
        stop_reason,
    })
}

const SCTLR_EL1: u32 = (3 << 14) | (0 << 11) | (1 << 7) | (0 << 3) | 0;
const TTBR0_EL1: u32 = (3 << 14) | (0 << 11) | (2 << 7) | (0 << 3) | 0;
const TTBR1_EL1: u32 = (3 << 14) | (0 << 11) | (2 << 7) | (0 << 3) | 1;
const TCR_EL1: u32 = (3 << 14) | (0 << 11) | (2 << 7) | (0 << 3) | 2;

const XNU_VIRT_BASE: u64 = 0xFFFF_FFF0_0000_0000;
const XNU_PHYS_BASE: u64 = 0x0000_0008_0000_0000;

/// 64-bit ARM64 kernel/app bring-up report.
#[derive(Debug, Clone)]
pub struct Arm64Report {
    pub arch: String,
    pub entry: u64,
    pub final_pc: u64,
    pub final_inst: u32,
    pub instructions: u64,
    pub sysregs_written: usize,
    pub mmu_enabled: bool,
    pub uart_output: String,
    pub mmio_regions: Vec<u64>,
    pub stop_reason: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct MmuRegs {
    sctlr: u64,
    tcr: u64,
    ttbr0: u64,
    ttbr1: u64,
}

#[derive(Clone, Copy, Default)]
struct TlbEntry {
    va_page: u64,
    pa_page: u64,
    valid: bool,
}

const TLB_SIZE: usize = 256;

struct Arm64Mem {
    pages: HashMap<u64, Box<[u8; 4096]>>,
    mmu: MmuRegs,
    tlb: [TlbEntry; TLB_SIZE],
    uart_output: String,
    uart_bases: Vec<u64>,
    mmio_regs: HashMap<u64, u64>,
    mmio_regions: Vec<u64>,
    last_fault_write: bool,
    trace: bool,
}

impl Arm64Mem {
    fn new(trace: bool) -> Self {
        Self {
            pages: HashMap::new(),
            mmu: MmuRegs::default(),
            tlb: [TlbEntry::default(); TLB_SIZE],
            uart_output: String::new(),
            // Default Apple Silicon S5L UART0 physical bases (T8103/T8110/T8120).
            uart_bases: vec![0x2_3520_0000],
            mmio_regs: HashMap::new(),
            mmio_regions: Vec::new(),
            last_fault_write: false,
            trace,
        }
    }

    fn is_uart_page(&self, pa_page: u64) -> bool {
        self.uart_bases.contains(&pa_page)
            // Apple S5L UARTs sit at 0x...20_0000 (uart0) .. 0x...22_0000
            || (pa_page & 0xFFFF_F000) == 0x3520_0000
            || (pa_page & 0xFFFF_F000) == 0x9B20_0000
    }

    fn set_mmu(&mut self, next: MmuRegs) {
        if self.mmu != next {
            self.mmu = next;
            self.tlb = [TlbEntry::default(); TLB_SIZE];
        }
    }

    fn note_mmio(&mut self, addr: u64) {
        let region = addr & !0xFFF;
        if self.mmio_regions.len() < 16 && !self.mmio_regions.contains(&region) {
            self.mmio_regions.push(region);
        }
    }

    /// Maps pre-MMU virtual kernel addresses (`XNU_VIRT_BASE..+4GiB`) onto physical RAM
    /// (`XNU_PHYS_BASE..+4GiB`) so XNU's early page-table writes and physical reads share backing.
    fn virt_to_phys_linear(addr: u64) -> u64 {
        if (XNU_VIRT_BASE..XNU_VIRT_BASE + (4u64 << 30)).contains(&addr) {
            addr - XNU_VIRT_BASE + XNU_PHYS_BASE
        } else {
            addr
        }
    }

    fn is_dram(pa: u64) -> bool {
        (XNU_PHYS_BASE..XNU_PHYS_BASE + (4u64 << 30)).contains(&pa)
    }

    fn read_phys_u64(&self, pa: u64) -> Option<u64> {
        let mut v = 0u64;
        for k in 0..8u64 {
            let a = pa.wrapping_add(k);
            let b = match self.pages.get(&(a >> 12)) {
                Some(pg) => pg[(a & 0xFFF) as usize],
                None if Self::is_dram(a) => 0,
                None => return None,
            };
            v |= (b as u64) << (k * 8);
        }
        Some(v)
    }

    /// Walks ARMv8-A 16 KB or 4 KB translation tables (`TTBR0_EL1` / `TTBR1_EL1`) with a 256-entry TLB.
    fn translate(&mut self, va: u64) -> Option<u64> {
        if self.mmu.sctlr & 1 == 0 {
            return Some(Self::virt_to_phys_linear(va));
        }
        let va_page = va & !0xFFF;
        let slot = ((va_page >> 12) as usize) & (TLB_SIZE - 1);
        let ent = self.tlb[slot];
        if ent.valid && ent.va_page == va_page {
            return Some(ent.pa_page | (va & 0xFFF));
        }
        let pa = self.translate_walk(va)?;
        self.tlb[slot] = TlbEntry { va_page, pa_page: pa & !0xFFF, valid: true };
        Some(pa)
    }

    fn translate_walk(&self, va: u64) -> Option<u64> {
        let upper = (va >> 55) & 1 != 0;
        let (ttbr, tsz, is_16k) = if upper {
            let tg1 = (self.mmu.tcr >> 30) & 3;
            let t1sz = ((self.mmu.tcr >> 16) & 0x3F) as u32;
            (self.mmu.ttbr1, t1sz, tg1 == 1)
        } else {
            let tg0 = (self.mmu.tcr >> 14) & 3;
            let t0sz = (self.mmu.tcr & 0x3F) as u32;
            (self.mmu.ttbr0, t0sz, tg0 == 2)
        };
        let mut table_pa = ttbr & 0x0000_FFFF_FFFF_F000;
        if table_pa == 0 {
            return Some(Self::virt_to_phys_linear(va));
        }

        let (page_bits, stride) = if is_16k { (14u32, 11u32) } else { (12u32, 9u32) };
        let ia_bits = 64u32.saturating_sub(tsz).clamp(16, 48);
        // Determine starting level (0..=3) from IA width and granule size.
        let mut level: i32 = if is_16k {
            if ia_bits > 47 { 0 } else if ia_bits > 36 { 1 } else if ia_bits > 25 { 2 } else { 3 }
        } else if ia_bits > 39 {
            0
        } else if ia_bits > 30 {
            1
        } else if ia_bits > 21 {
            2
        } else {
            3
        };

        let index_mask = (1u64 << stride) - 1;
        let oa_mask: u64 = if is_16k { 0x0000_FFFF_FFFF_C000 } else { 0x0000_FFFF_FFFF_F000 };

        while level <= 3 {
            let shift = page_bits + stride * (3 - level) as u32;
            let idx = if level == 0 && is_16k { (va >> 47) & 1 } else { (va >> shift) & index_mask };
            let desc = self.read_phys_u64(table_pa + idx * 8)?;
            if desc & 1 == 0 {
                return None; // Invalid descriptor
            }
            let is_table = (desc & 2) != 0;
            if level == 3 {
                if !is_table {
                    return None;
                }
                let page_mask = (1u64 << page_bits) - 1;
                return Some((desc & oa_mask) | (va & page_mask));
            }
            if !is_table {
                // Block descriptor at L1/L2
                let block_mask = (1u64 << shift) - 1;
                return Some((desc & oa_mask & !block_mask) | (va & block_mask));
            }
            table_pa = desc & oa_mask;
            level += 1;
        }
        None
    }

    fn write_bytes(&mut self, addr: u64, data: &[u8]) {
        let pa_base = Self::virt_to_phys_linear(addr);
        let mut i = 0usize;
        while i < data.len() {
            let a = pa_base.wrapping_add(i as u64);
            let pg = a >> 12;
            let off = (a & 0xFFF) as usize;
            let chunk = (4096 - off).min(data.len() - i);
            let buf = self.pages.entry(pg).or_insert_with(|| Box::new([0; 4096]));
            buf[off..off + chunk].copy_from_slice(&data[i..i + chunk]);
            i += chunk;
        }
    }

    fn map_zero(&mut self, addr: u64, len: u64) {
        let pa_base = Self::virt_to_phys_linear(addr);
        let start = pa_base >> 12;
        let end = pa_base.saturating_add(len).div_ceil(4096);
        for pg in start..end {
            self.pages.entry(pg).or_insert_with(|| Box::new([0; 4096]));
        }
    }
}

impl switch::a64::Mem for Arm64Mem {
    fn read(&mut self, addr: u64, size: usize) -> Option<u64> {
        let pa = self.translate(addr).or_else(|| {
            self.last_fault_write = false;
            self.note_mmio(addr);
            None
        })?;
        if !Self::is_dram(pa) {
            let page = pa & !0xFFF;
            if self.trace && !self.mmio_regions.contains(&page) {
                eprintln!("MMIO READ  va={addr:#018x} -> pa={pa:#014x} (size={size})");
            }
            self.note_mmio(pa);
            if self.is_uart_page(page) {
                return Some(match pa & 0xFFF {
                    0x10 => 0x6, // UTRSTAT: TX buffer & shifter empty
                    0x18 => 0x0, // UFSTAT: TX FIFO not full, RX empty
                    _ => *self.mmio_regs.get(&pa).unwrap_or(&0),
                });
            }
            return Some(*self.mmio_regs.get(&pa).unwrap_or(&0));
        }
        let mut v = 0u64;
        for k in 0..size.min(8) {
            let a = pa.wrapping_add(k as u64);
            let b = match self.pages.get(&(a >> 12)) {
                Some(pg) => pg[(a & 0xFFF) as usize],
                None => 0,
            };
            v |= (b as u64) << (k * 8);
        }
        Some(v)
    }

    fn write(&mut self, addr: u64, size: usize, value: u64) -> bool {
        let Some(pa) = self.translate(addr) else {
            self.last_fault_write = true;
            self.note_mmio(addr);
            return false;
        };
        if !Self::is_dram(pa) {
            let page = pa & !0xFFF;
            if self.trace && !self.mmio_regions.contains(&page) {
                eprintln!("MMIO WRITE va={addr:#018x} -> pa={pa:#014x} = {value:#x} (size={size})");
            }
            self.note_mmio(pa);
            // Apple S5L UART: if ULCON (+0x00) is written with 0x3 (8N1), register page as UART.
            if (pa & 0xFFF) == 0x00 && (value & 0xFF) == 0x3 && !self.uart_bases.contains(&page) {
                self.uart_bases.push(page);
            }
            // Apple S5L UART TX FIFO (+0x20): capture serial kernel printf bytes!
            if self.is_uart_page(page) && (pa & 0xFFF) == 0x20 {
                let ch = (value & 0xFF) as u8;
                if (0x20..=0x7E).contains(&ch) || ch == b'\n' || ch == b'\r' || ch == b'\t' {
                    self.uart_output.push(ch as char);
                    if self.trace {
                        eprint!("{}", ch as char);
                    }
                }
                return true;
            }
            self.mmio_regs.insert(pa, value);
            return true;
        }
        for k in 0..size.min(8) {
            let a = pa.wrapping_add(k as u64);
            let p = self.pages.entry(a >> 12).or_insert_with(|| Box::new([0; 4096]));
            p[(a & 0xFFF) as usize] = (value >> (k * 8)) as u8;
        }
        true
    }

    fn translate_va(&mut self, va: u64) -> Option<u64> {
        self.translate(va)
    }

    fn flush_tlb(&mut self) {
        self.tlb = [TlbEntry::default(); TLB_SIZE];
    }
}

/// Maps a decrypted 64-bit ARM64 / ARM64e Mach-O kernel or binary onto our AArch64 core and
/// executes up to `max_steps` bring-up instructions, optionally passing an unwrapped Apple
/// Device Tree (ADT) in XNU's `boot_args` struct.
pub fn run_macho_arm64(data: &[u8], device_tree: Option<&[u8]>, max_steps: u64) -> Result<Arm64Report, String> {
    let m = macho::parse(data)?;
    let slice = m
        .slices
        .iter()
        .find(|s| s.arch == "arm64" || s.arch == "arm64e")
        .ok_or("No arm64/arm64e slice found in this Mach-O.")?;
    if slice.encrypted {
        return Err("This arm64 Mach-O is encrypted.".into());
    }

    let trace = std::env::var_os("IOS_XNU_TRACE").is_some();
    let mut mem = Arm64Mem::new(trace);
    for seg in &slice.segments {
        if seg.name == "__PAGEZERO" || seg.vmsize == 0 {
            continue;
        }
        let foff = seg.fileoff as usize;
        let fsz = (seg.filesize as usize).min(seg.vmsize as usize);
        if fsz > 0 {
            if let Some(chunk) = data.get(foff..foff.saturating_add(fsz)) {
                mem.write_bytes(seg.vmaddr, chunk);
            }
        }
        if seg.vmsize > fsz as u64 {
            let extra = (seg.vmsize - fsz as u64).min(16 << 20);
            mem.map_zero(seg.vmaddr.wrapping_add(fsz as u64), extra);
        }
    }
    // Map a minimal boot stack, Apple Device Tree (if supplied), and iBoot boot_args page (x0).
    // Like real Apple iBoot, start XNU at its physical entry address with MMU off; XNU builds
    // its 16 KB TTBR0/TTBR1 page tables, enables SCTLR_EL1.M, and jumps to virtual space.
    let mem_size: u64 = 512 << 20; // 512 MiB keeps early pmap/vm_page_bootstrap fast
    let boot_stack_va: u64 = XNU_VIRT_BASE + 0x1000_0000;
    let boot_args_va: u64 = XNU_VIRT_BASE + 0x0FF0_0000;
    let dt_va: u64 = XNU_VIRT_BASE + 0x0F00_0000;
    let boot_stack_pa = Arm64Mem::virt_to_phys_linear(boot_stack_va);
    let boot_args_pa = Arm64Mem::virt_to_phys_linear(boot_args_va);
    let entry_pa = Arm64Mem::virt_to_phys_linear(slice.entry);

    mem.map_zero(boot_stack_va - 0x4_0000, 0x4_0000);
    mem.map_zero(boot_args_va, 0x4000);

    let mut ba = [0u8; 0x300];
    ba[0..2].copy_from_slice(&2u16.to_le_bytes()); // revision = 2
    ba[2..4].copy_from_slice(&2u16.to_le_bytes()); // version = 2
    ba[8..16].copy_from_slice(&XNU_VIRT_BASE.to_le_bytes()); // virtBase
    ba[16..24].copy_from_slice(&XNU_PHYS_BASE.to_le_bytes()); // physBase
    ba[24..32].copy_from_slice(&mem_size.to_le_bytes()); // memSize = 512 MiB
    ba[32..40].copy_from_slice(&boot_stack_pa.to_le_bytes()); // topOfKernelData (physical!)
    // Boot_Video at 0x28 (48 bytes = 6 x u64): framebuffer at physBase + 0x1E00_0000 (1179x2556x32)
    let fb_pa = XNU_PHYS_BASE + 0x1E00_0000;
    ba[0x28..0x30].copy_from_slice(&fb_pa.to_le_bytes()); // v_baseAddr
    ba[0x30..0x38].copy_from_slice(&1u64.to_le_bytes()); // v_display = GRAPHICS_MODE
    ba[0x38..0x40].copy_from_slice(&(1179u64 * 4).to_le_bytes()); // v_rowBytes
    ba[0x40..0x48].copy_from_slice(&1179u64.to_le_bytes()); // v_width
    ba[0x48..0x50].copy_from_slice(&2556u64.to_le_bytes()); // v_height
    ba[0x50..0x58].copy_from_slice(&32u64.to_le_bytes()); // v_depth
    // 0x58: machineType (u32) + 4 bytes pad; 0x60: deviceTreeP; 0x68: deviceTreeLength; 0x6C: CommandLine[608]
    if let Some(dt) = device_tree {
        mem.write_bytes(dt_va, dt);
        ba[0x60..0x68].copy_from_slice(&dt_va.to_le_bytes()); // deviceTreeP
        ba[0x68..0x6C].copy_from_slice(&(dt.len() as u32).to_le_bytes()); // deviceTreeLength
    }
    let cmdline = b"-v debug=0x8 serial=3\0";
    ba[0x6C..0x6C + cmdline.len()].copy_from_slice(cmdline);
    ba[0x2D8..0x2E0].copy_from_slice(&mem_size.to_le_bytes()); // memSizeActual
    mem.write_bytes(boot_args_va, &ba);

    let mut cpu = switch::a64::Cpu::new();
    cpu.el1 = true;
    // Apple Silicon A16 early-boot status register S3_6_C15_C12_4 (bit 0 = ready).
    cpu.sysregs.insert((3 << 14) | (6 << 11) | (15 << 7) | (12 << 3) | 4, 1);
    cpu.pc = entry_pa;
    cpu.sp = boot_stack_pa - 0x100;
    cpu.x[0] = boot_args_pa;

    let mut ring = [0u64; 16];
    let mut ring_idx = 0usize;
    let mut last_abort = (0u64, 0u64);
    let mut repeat_aborts = 0u32;
    let mut total_aborts = 0u32;
    let mut instructions = 0u64;
    let mut stop_reason = format!("Reached step budget ({max_steps} instructions)");
    for _ in 0..max_steps {
        let cur_pc = cpu.pc;
        match cpu.step(&mut mem) {
            Ok(()) => {
                if trace {
                    ring[ring_idx & 15] = cur_pc;
                    ring_idx = ring_idx.wrapping_add(1);
                }
                instructions += 1;
                cpu.ticks = cpu.ticks.wrapping_add(1);
                let next_mmu = MmuRegs {
                    sctlr: *cpu.sysregs.get(&SCTLR_EL1).unwrap_or(&0),
                    tcr: *cpu.sysregs.get(&TCR_EL1).unwrap_or(&0),
                    ttbr0: *cpu.sysregs.get(&TTBR0_EL1).unwrap_or(&0),
                    ttbr1: *cpu.sysregs.get(&TTBR1_EL1).unwrap_or(&0),
                };
                mem.set_mmu(next_mmu);
            }
            Err(switch::a64::Exit::Undefined(pc, w)) => {
                if trace {
                    let read_cstr = |mem: &mut Arm64Mem, ptr: u64| -> String {
                        let mut s = Vec::new();
                        for k in 0..160u64 {
                            let Some(ch) = switch::a64::Mem::read(mem, ptr + k, 1) else { break };
                            if ch == 0 { break; }
                            s.push(ch as u8);
                        }
                        String::from_utf8_lossy(&s).into_owned()
                    };
                    eprintln!("UNDEF {w:#010x} at {pc:#x} (lr={:#x} sp={:#x})", cpu.x[30], cpu.sp);
                    eprintln!("LAST 16:");
                    for k in 0..16 {
                        let rpc = ring[(ring_idx.wrapping_add(k)) & 15];
                        if rpc != 0 {
                            let rw = switch::a64::Mem::read(&mut mem, rpc, 4).unwrap_or(0) as u32;
                            eprintln!("  {rpc:#018x}: {rw:08x}");
                        }
                    }
                    for idx in [0usize, 1, 2, 3, 4, 5, 16, 17, 19, 20, 21, 22, 23, 29, 30] {
                        let v = cpu.x[idx];
                        let s = if v >= 0xFFFF_FF80_0000_0000 { read_cstr(&mut mem, v) } else { String::new() };
                        if !s.is_empty() && s.bytes().all(|b| (0x20..=0x7E).contains(&b) || b == b'\n') {
                            eprintln!("  x{idx:02}={v:#018x} {s:?}");
                        } else {
                            eprintln!("  x{idx:02}={v:#018x}");
                        }
                    }
                    if w == 0xE7FF_DEFF && cpu.x[3] >= 0xFFFF_FF80_0000_0000 {
                        eprintln!("  panic va_list (x03={:#x}):", cpu.x[3]);
                        for k in 0..6u64 {
                            let v = switch::a64::Mem::read(&mut mem, cpu.x[3] + k * 8, 8).unwrap_or(0);
                            let s = if v >= 0xFFFF_FF80_0000_0000 { read_cstr(&mut mem, v) } else { String::new() };
                            if !s.is_empty() && s.bytes().all(|b| (0x20..=0x7E).contains(&b) || b == b'\n') {
                                eprintln!("    arg[{k}]={v:#018x} {s:?}");
                            } else {
                                eprintln!("    arg[{k}]={v:#018x} ({v})");
                            }
                        }
                    }
                }
                stop_reason = format!("Reached unimplemented ARM64 instruction {w:#010X} at {pc:#018X}");
                break;
            }
            Err(switch::a64::Exit::Fault(a)) => {
                if mem.mmu.sctlr & 1 != 0 && a >= 0xFFFF_FF80_0000_0000 {
                    if last_abort == (cur_pc, a) {
                        repeat_aborts += 1;
                    } else {
                        last_abort = (cur_pc, a);
                        repeat_aborts = 1;
                    }
                    let is_insn = a == cur_pc && !mem.last_fault_write;
                    if repeat_aborts <= 8 && cpu.take_el1_sync_abort(cur_pc, a, mem.last_fault_write, is_insn) {
                        total_aborts += 1;
                        if trace && total_aborts <= 16 {
                            eprintln!(
                                "EL1 ABORT #{total_aborts}: far={a:#018x} elr={cur_pc:#018x} wr={} -> vec={:#018x} sp={:#018x}",
                                mem.last_fault_write, cpu.pc, cpu.sp
                            );
                        }
                        continue;
                    }
                }
                if std::env::var_os("IOS_XNU_TRACE").is_some() {
                    let prev_pc = cpu.pc.wrapping_sub(4);
                    let mut around = Vec::new();
                    for off in [-16i64, -12, -8, -4, 0, 4, 8] {
                        let p = (prev_pc as i64 + off) as u64;
                        let w = switch::a64::Mem::read(&mut mem, p, 4).unwrap_or(0) as u32;
                        around.push(format!("{p:#x}:{w:08x}"));
                    }
                    eprintln!("FAULT addr={a:#x} pc={:#x} sp={:#x}", cpu.pc, cpu.sp);
                    eprintln!("CODE: {}", around.join(" "));
                    for row in 0..8 {
                        let b = row * 4;
                        let (i1, i2, i3) = (b + 1, b + 2, b + 3);
                        let v3 = if i3 < 31 { cpu.x[i3] } else { cpu.sp };
                        eprintln!(
                            "  x{b:02}={:#018x} x{i1:02}={:#018x} x{i2:02}={:#018x} x{i3:02}={:#018x}",
                            cpu.x[b], cpu.x[i1], cpu.x[i2], v3
                        );
                    }
                    let read_cstr = |mem: &mut Arm64Mem, ptr: u64| -> String {
                        let mut s = Vec::new();
                        for k in 0..120u64 {
                            let Some(ch) = switch::a64::Mem::read(mem, ptr + k, 1) else { break };
                            if ch == 0 { break; }
                            s.push(ch as u8);
                        }
                        String::from_utf8_lossy(&s).into_owned()
                    };
                    for &(reg_idx, val) in &[(0, cpu.x[0]), (1, cpu.x[1]), (19, cpu.x[19]), (21, cpu.x[21]), (23, cpu.x[23])] {
                        if val >= XNU_VIRT_BASE {
                            let s = read_cstr(&mut mem, val);
                            if !s.is_empty() && s.bytes().all(|b| (0x20..=0x7E).contains(&b) || b == b'\n') {
                                eprintln!("  x{reg_idx:02} str: {s:?}");
                            }
                        }
                    }
                    if cpu.x[0] >= XNU_VIRT_BASE {
                        let fmt = read_cstr(&mut mem, cpu.x[0]);
                        if !fmt.is_empty() {
                            eprintln!("  x00 str: {:?}", fmt);
                            if cpu.x[5] >= XNU_VIRT_BASE {
                                let mut caller = Vec::new();
                                for off in (-40i64..=8).step_by(4) {
                                    let p = (cpu.x[5] as i64 + off) as u64;
                                    let w = switch::a64::Mem::read(&mut mem, p, 4).unwrap_or(0) as u32;
                                    caller.push(format!("{p:#x}:{w:08x}"));
                                }
                                eprintln!("  caller(x05): {}", caller.join(" "));
                            }
                            if cpu.x[8] >= XNU_VIRT_BASE {
                                for idx in 0..4u64 {
                                    let v = switch::a64::Mem::read(&mut mem, cpu.x[8] + idx * 8, 8).unwrap_or(0);
                                    let s = if v >= XNU_VIRT_BASE { read_cstr(&mut mem, v) } else { String::new() };
                                    eprintln!("  arg[{idx}]={v:#018x} ({v}) {s:?}");
                                }
                            }
                        }
                    }
                }
                stop_reason = if mem.mmu.sctlr & 1 != 0 {
                    format!(
                        "MMU fault on {a:#018X} at PC {:#018X} (TCR={:#x} TTBR0={:#x} TTBR1={:#x})",
                        cpu.pc, mem.mmu.tcr, mem.mmu.ttbr0, mem.mmu.ttbr1
                    )
                } else {
                    format!("Accessed unmapped address/MMIO {a:#018X} at PC {:#018X}", cpu.pc)
                };
                break;
            }
            Err(switch::a64::Exit::Svc(n)) => {
                stop_reason = format!("Executed SVC #{n:#x} at PC {:#018X}", cpu.pc);
                break;
            }
            Err(switch::a64::Exit::Brk(n)) => {
                stop_reason = format!("Hit BRK #{n:#x} at PC {:#018X}", cpu.pc);
                break;
            }
        }
    }

    let final_inst = switch::a64::Mem::read(&mut mem, cpu.pc, 4).unwrap_or(0) as u32;
    if stop_reason.starts_with("Reached step budget") {
        let mut nearby = Vec::new();
        for off in [-8i64, -4, 0, 4, 8] {
            let a = (cpu.pc as i64 + off) as u64;
            let w = switch::a64::Mem::read(&mut mem, a, 4).unwrap_or(0) as u32;
            nearby.push(format!("{a:#x}:{w:08x}"));
        }
        stop_reason.push_str(&format!(" [{}]", nearby.join(" ")));
    }
    Ok(Arm64Report {
        arch: slice.arch.clone(),
        entry: slice.entry,
        final_pc: cpu.pc,
        final_inst,
        instructions,
        sysregs_written: cpu.sysregs.len(),
        mmu_enabled: mem.mmu.sctlr & 1 != 0,
        uart_output: mem.uart_output,
        mmio_regions: mem.mmio_regions,
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal 32-bit ARM Mach-O that writes "Hello iOS!\n" via Darwin `svc #0x80`
    /// (`write` = 4) and exits with code 0 (`exit` = 1).
    fn hello_ios_macho() -> Vec<u8> {
        let mut d = vec![0u8; 0x2000];
        let put = |d: &mut [u8], o: usize, v: u32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        // Mach-O 32-bit header: ARMv7, 2 load commands (LC_SEGMENT + LC_UNIXTHREAD)
        put(&mut d, 0, 0xFEED_FACE);
        put(&mut d, 4, 12); // CPU_TYPE_ARM
        put(&mut d, 8, 9); // CPU_SUBTYPE_ARM_V7
        put(&mut d, 12, 2); // MH_EXECUTE
        put(&mut d, 16, 2); // ncmds
        put(&mut d, 20, 56 + 80); // sizeofcmds

        // Command 1: LC_SEGMENT "__TEXT" at vmaddr 0x1000, vmsize 0x2000, fileoff 0, filesize 0x2000
        let s = 28;
        put(&mut d, s, 0x1);
        put(&mut d, s + 4, 56);
        d[s + 8..s + 14].copy_from_slice(b"__TEXT");
        put(&mut d, s + 24, 0x1000);
        put(&mut d, s + 28, 0x2000);
        put(&mut d, s + 32, 0);
        put(&mut d, s + 36, 0x2000);

        // Command 2: LC_UNIXTHREAD (r15 = 0x2000 -> file offset 0x1000)
        let u = s + 56;
        put(&mut d, u, 0x5);
        put(&mut d, u + 4, 80);
        put(&mut d, u + 16 + 15 * 4, 0x2000);

        let code: [u32; 8] = [
            0xE3A0_0001, // mov r0, #1
            0xE3A0_1C21, // mov r1, #0x2100
            0xE3A0_200B, // mov r2, #11
            0xE3A0_C004, // mov r12, #4
            0xEF00_0080, // svc #0x80
            0xE3A0_0000, // mov r0, #0
            0xE3A0_C001, // mov r12, #1
            0xEF00_0080, // svc #0x80
        ];
        for (i, w) in code.iter().enumerate() {
            put(&mut d, 0x1000 + i * 4, *w);
        }
        d[0x1100..0x110B].copy_from_slice(b"Hello iOS!\n");
        d
    }

    #[test]
    fn runs_arm32_ios_macho_with_darwin_syscalls() {
        let bin = hello_ios_macho();
        let report = run_macho(&bin, 1000).expect("runs cleanly");
        assert_eq!(report.arch, "armv7");
        assert_eq!(report.entry, 0x2000);
        assert_eq!(report.exit_code, Some(0));
        assert_eq!(report.stdout, "Hello iOS!\n");
    }

    #[test]
    fn arm64_mmu_16k_translation_and_s5l_uart() {
        use switch::a64::Mem;
        let mut mem = Arm64Mem::new(false);
        // Build a 16 KB granule TTBR1_EL1 translation table (T1SZ=28 -> 36-bit IA -> starts at L2).
        // VA = 0xFFFF_FFF0_0000_4000 -> L2 idx 0, L3 idx 1 -> PA 0x2_3520_0000 (Apple S5L UART0).
        let l2_pa = XNU_PHYS_BASE + 0x1_0000;
        let l3_pa = XNU_PHYS_BASE + 0x2_0000;
        let uart_pa = 0x2_3520_0000u64;
        mem.write_bytes(l2_pa, &(l3_pa | 3).to_le_bytes()); // L2[0] -> table at l3_pa
        mem.write_bytes(l3_pa + 8, &(uart_pa | 3).to_le_bytes()); // L3[1] -> 16 KB page at uart_pa
        mem.set_mmu(MmuRegs {
            sctlr: 1,
            tcr: (1u64 << 30) | (28u64 << 16), // TG1 = 16 KB (0b01), T1SZ = 28
            ttbr0: 0,
            ttbr1: l2_pa,
        });
        let uart_va = 0xFFFF_FFF0_0000_4000u64;
        assert_eq!(mem.translate_va(uart_va), Some(uart_pa));
        assert_eq!(mem.read(uart_va + 0x10, 4), Some(0x6)); // UTRSTAT TX empty
        assert!(mem.write(uart_va, 4, 0x3)); // ULCON = 8N1
        for &b in b"XNU\n" {
            assert!(mem.write(uart_va + 0x20, 4, b as u64)); // UTXH
        }
        assert_eq!(mem.uart_output, "XNU\n");
    }
}
