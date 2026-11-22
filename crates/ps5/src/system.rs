//! Full-system bring-up for PlayStation 4 class hardware (x86-64 "Jaguar" cores).
//!
//! `machine.rs` runs a game by answering its system-library calls in Rust. This module is the
//! other approach: model the console itself, load the system software's kernel (an ELF you
//! dumped from your own console) and let it run until it touches hardware we haven't built yet.
//! Every such touch is recorded, grouped by region, so the output is the to-do list for booting:
//! which devices the kernel talks to, in what order, and what it expects back.
//!
//! The physical map follows a typical x86-64 console: RAM below the PCI hole (below 3 GB), the
//! hole itself for devices, then the rest of RAM above 4 GB. Nothing here is a PlayStation
//! secret; the kernel image, and any keys or firmware it needs, come from your own console.

use cpui686::{Bus, Cpu, EFER_LMA, EFER_LME, EFER_NXE, EFER_SCE};
use std::collections::{BTreeMap, HashMap};

const PAGE: u64 = 0x1000;
/// Start of the device hole (PCI memory space).
const HOLE_START: u64 = 0xC000_0000;
const HIGH_START: u64 = 0x1_0000_0000;
/// Where the kernel image is placed physically (2 MB aligned).
const KERNEL_PHYS: u64 = 0x0200_0000;
const PML4: u64 = 0x1000;
const TABLES: u64 = 0x10000;
const BOOT_STACK: u64 = 0x0008_0000;
const BOOT_PARAMS: u64 = 0x0009_0000;
const MAX_LOG: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    pub addr: u64,
    pub size: u8,
    pub write: bool,
    pub value: u32,
    pub rip: u64,
    /// Port number accesses are I/O, not memory.
    pub port: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Region {
    pub reads: u64,
    pub writes: u64,
    pub first_addr: u64,
    pub first_rip: u64,
}

pub struct Seg {
    pub vaddr: u64,
    pub offset: u64,
    pub file_size: u64,
    pub mem_size: u64,
}

pub struct KernelImage {
    pub entry: u64,
    pub segs: Vec<Seg>,
}

fn le16(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u64
}
fn le32(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Reads the loadable segments of a (decrypted) 64-bit x86 kernel ELF.
pub fn parse_kernel(d: &[u8]) -> Result<KernelImage, String> {
    if d.len() < 64 || &d[..4] != b"\x7FELF" {
        return Err("This isn't an ELF file. The kernel has to be decrypted first, from your own console.".into());
    }
    if d[4] != 2 || d[5] != 1 || le16(d, 18) != 0x3E {
        return Err("This ELF isn't a 64-bit x86 program.".into());
    }
    let (phoff, phentsize, phnum) = (le64(d, 32) as usize, le16(d, 54) as usize, le16(d, 56) as usize);
    let mut segs = Vec::new();
    for i in 0..phnum.min(64) {
        let o = phoff + i * phentsize;
        let ph = d.get(o..o + 56).ok_or("The ELF program headers are truncated.")?;
        if le32(ph, 0) == 1 {
            let s = Seg { vaddr: le64(ph, 16), offset: le64(ph, 8), file_size: le64(ph, 32), mem_size: le64(ph, 40) };
            if s.offset.saturating_add(s.file_size) > d.len() as u64 || s.mem_size > 1 << 30 {
                return Err("A segment of the kernel is truncated or implausibly large.".into());
            }
            segs.push(s);
        }
    }
    if segs.is_empty() {
        return Err("The ELF has nothing to load.".into());
    }
    Ok(KernelImage { entry: le64(d, 24), segs })
}

/// Physical memory (sparse) plus the log of everything that touched a device.
pub struct Hw {
    pages: HashMap<u64, Box<[u8; PAGE as usize]>>,
    ram_bytes: u64,
    pub log: Vec<Access>,
    pub regions: BTreeMap<u64, Region>,
    pub io_regions: BTreeMap<u16, Region>,
    cur_rip: u64,
    /// Next page-table page.
    next_table: u64,
}

impl Hw {
    fn new(ram_bytes: u64) -> Hw {
        Hw { pages: HashMap::new(), ram_bytes, log: Vec::new(), regions: BTreeMap::new(), io_regions: BTreeMap::new(), cur_rip: 0, next_table: TABLES }
    }

    pub fn is_ram(&self, a: u64) -> bool {
        let low = self.ram_bytes.min(HOLE_START);
        a < low || (a >= HIGH_START && a < HIGH_START + self.ram_bytes.saturating_sub(HOLE_START))
    }

    fn page_mut(&mut self, a: u64) -> &mut [u8; PAGE as usize] {
        self.pages.entry(a / PAGE).or_insert_with(|| Box::new([0; PAGE as usize]))
    }

    fn ram_read(&self, a: u64) -> u8 {
        self.pages.get(&(a / PAGE)).map_or(0, |p| p[(a % PAGE) as usize])
    }

    fn ram_write(&mut self, a: u64, v: u8) {
        self.page_mut(a)[(a % PAGE) as usize] = v;
    }

    fn write_bytes(&mut self, a: u64, data: &[u8]) {
        for (k, &b) in data.iter().enumerate() {
            self.ram_write(a + k as u64, b);
        }
    }

    fn put64(&mut self, a: u64, v: u64) {
        self.write_bytes(a, &v.to_le_bytes());
    }

    fn get64(&self, a: u64) -> u64 {
        (0..8).fold(0, |v, k| v | (self.ram_read(a + k) as u64) << (k * 8))
    }

    fn note(&mut self, a: u64, size: u8, write: bool, value: u32) {
        let r = self.regions.entry(a >> 16).or_insert_with(|| Region { first_addr: a, first_rip: self.cur_rip, ..Region::default() });
        if write {
            r.writes += 1;
        } else {
            r.reads += 1;
        }
        if self.log.len() < MAX_LOG {
            self.log.push(Access { addr: a, size, write, value, rip: self.cur_rip, port: false });
        }
    }

    fn table_page(&mut self) -> u64 {
        let t = self.next_table;
        self.next_table += PAGE;
        t
    }

    /// Maps one 2 MB page, creating the intermediate tables.
    fn map_2m(&mut self, vaddr: u64, phys: u64) {
        let mut table = PML4;
        for shift in [39u32, 30] {
            let slot = table + ((vaddr >> shift) & 0x1FF) * 8;
            let e = self.get64(slot);
            table = if e & 1 != 0 {
                e & 0x000F_FFFF_FFFF_F000
            } else {
                let t = self.table_page();
                self.put64(slot, t | 3);
                t
            };
        }
        self.put64(table + ((vaddr >> 21) & 0x1FF) * 8, (phys & !0x1F_FFFF) | 0x83);
    }
}

impl Bus for Hw {
    fn read8(&mut self, a: u64) -> u8 {
        if self.is_ram(a) {
            return self.ram_read(a);
        }
        self.note(a, 1, false, 0);
        0
    }
    fn write8(&mut self, a: u64, v: u8) {
        if self.is_ram(a) {
            return self.ram_write(a, v);
        }
        self.note(a, 1, true, v as u32);
    }
    fn read16(&mut self, a: u64) -> u16 {
        if self.is_ram(a) && self.is_ram(a + 1) {
            return self.ram_read(a) as u16 | (self.ram_read(a + 1) as u16) << 8;
        }
        self.note(a, 2, false, 0);
        0
    }
    fn write16(&mut self, a: u64, v: u16) {
        if self.is_ram(a) && self.is_ram(a + 1) {
            self.ram_write(a, v as u8);
            return self.ram_write(a + 1, (v >> 8) as u8);
        }
        self.note(a, 2, true, v as u32);
    }
    fn read32(&mut self, a: u64) -> u32 {
        if self.is_ram(a) && self.is_ram(a + 3) {
            return (0..4).fold(0, |v, k| v | (self.ram_read(a + k) as u32) << (k * 8));
        }
        self.note(a, 4, false, 0);
        0
    }
    fn write32(&mut self, a: u64, v: u32) {
        if self.is_ram(a) && self.is_ram(a + 3) {
            for k in 0..4 {
                self.ram_write(a + k, (v >> (k * 8)) as u8);
            }
            return;
        }
        self.note(a, 4, true, v);
    }
    fn io_in(&mut self, port: u16, size: u8) -> u32 {
        let r = self.io_regions.entry(port).or_insert_with(|| Region { first_addr: port as u64, first_rip: self.cur_rip, ..Region::default() });
        r.reads += 1;
        if self.log.len() < MAX_LOG {
            self.log.push(Access { addr: port as u64, size, write: false, value: 0, rip: self.cur_rip, port: true });
        }
        0xFFFF_FFFF
    }
    fn io_out(&mut self, port: u16, size: u8, v: u32) {
        let r = self.io_regions.entry(port).or_insert_with(|| Region { first_addr: port as u64, first_rip: self.cur_rip, ..Region::default() });
        r.writes += 1;
        if self.log.len() < MAX_LOG {
            self.log.push(Access { addr: port as u64, size, write: true, value: v, rip: self.cur_rip, port: true });
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The step limit was reached.
    Steps,
    /// The CPU executed `hlt` and is waiting for an interrupt nothing will send yet.
    Halted,
    /// The CPU shut down (an unhandled fault).
    Shutdown(String),
}

pub struct System {
    pub cpu: Cpu,
    pub hw: Hw,
    pub steps: u64,
}

impl System {
    /// A console with `ram_gb` of RAM and the kernel in `image` loaded and ready to start.
    pub fn new(elf: &[u8], ram_gb: u64) -> Result<System, String> {
        let img = parse_kernel(elf)?;
        let mut hw = Hw::new(ram_gb.clamp(1, 16) << 30);
        // Boot-time identity map of the first 4 GB (RAM and the device hole).
        for i in 0..2048u64 {
            hw.map_2m(i << 21, i << 21);
        }
        // The kernel: its virtual addresses map onto KERNEL_PHYS in 2 MB steps.
        let lo = img.segs.iter().map(|s| s.vaddr).min().unwrap() & !0x1F_FFFF;
        let hi = img.segs.iter().map(|s| s.vaddr + s.mem_size).max().unwrap();
        let mut v = lo;
        while v < hi {
            hw.map_2m(v, KERNEL_PHYS + (v - lo));
            v += 1 << 21;
        }
        for s in &img.segs {
            let data = &elf[s.offset as usize..(s.offset + s.file_size) as usize];
            hw.write_bytes(KERNEL_PHYS + (s.vaddr - lo), data);
        }
        let mut cpu = Cpu::new_x64();
        cpu.cr0 = 0x8000_0033;
        cpu.cr3 = PML4;
        cpu.cr4 = (1 << 5) | (1 << 9) | (1 << 10);
        cpu.efer = EFER_LME | EFER_LMA | EFER_NXE | EFER_SCE;
        cpu.code64 = true;
        cpu.cs32 = false;
        cpu.ss32 = true;
        cpu.cpl = 0;
        cpu.sel[1] = 0x08;
        cpu.sel[2] = 0x10;
        cpu.base = [0; 6];
        cpu.eflags = 0x2;
        cpu.rip = img.entry;
        cpu.r[4] = BOOT_STACK;
        cpu.r[7] = BOOT_PARAMS; // rdi: the boot parameters the loader would pass
        cpu.flush_tlb();
        Ok(System { cpu, hw, steps: 0 })
    }

    /// Runs until the step limit, a halt, or a shutdown.
    pub fn run(&mut self, max_steps: u64) -> Outcome {
        for _ in 0..max_steps {
            self.hw.cur_rip = self.cpu.rip;
            self.cpu.step(&mut self.hw);
            self.steps += 1;
            if let Some(msg) = self.cpu.shutdown.take() {
                return Outcome::Shutdown(msg);
            }
            if self.cpu.halted {
                return Outcome::Halted;
            }
        }
        Outcome::Steps
    }

    /// What the kernel touched, as a plain-language to-do list.
    pub fn report(&self, outcome: &Outcome) -> String {
        let mut s = format!("Ran {} instructions; stopped at {:#x}: {}.\n", self.steps, self.cpu.rip, match outcome {
            Outcome::Steps => "step limit reached".to_string(),
            Outcome::Halted => "the kernel halted, waiting for an interrupt from hardware we don't model".to_string(),
            Outcome::Shutdown(m) => format!("the CPU shut down ({m})"),
        });
        if self.hw.regions.is_empty() && self.hw.io_regions.is_empty() {
            s.push_str("It hasn't touched any device yet.\n");
            return s;
        }
        s.push_str("Devices it touched that we don't model yet (64 KB regions, in address order):\n");
        for (r, info) in &self.hw.regions {
            s.push_str(&format!("  memory {:#012x}  {} reads, {} writes  (first {:#x} from code at {:#x})\n", r << 16, info.reads, info.writes, info.first_addr, info.first_rip));
        }
        for (p, info) in &self.hw.io_regions {
            s.push_str(&format!("  port   {:#06x}  {} reads, {} writes  (first from code at {:#x})\n", p, info.reads, info.writes, info.first_rip));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny kernel ELF whose code is `code`, loaded at the PlayStation-style kernel address.
    fn kernel(code: &[u8]) -> Vec<u8> {
        const VADDR: u64 = 0xFFFF_FFFF_8220_0000;
        let mut f = vec![0u8; 0x1000 + code.len()];
        f[..4].copy_from_slice(b"\x7FELF");
        f[4] = 2;
        f[5] = 1;
        f[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        f[24..32].copy_from_slice(&VADDR.to_le_bytes());
        f[32..40].copy_from_slice(&64u64.to_le_bytes());
        f[54..56].copy_from_slice(&56u16.to_le_bytes());
        f[56..58].copy_from_slice(&1u16.to_le_bytes());
        let ph = 64;
        f[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes());
        f[ph + 8..ph + 16].copy_from_slice(&0x1000u64.to_le_bytes());
        f[ph + 16..ph + 24].copy_from_slice(&VADDR.to_le_bytes());
        f[ph + 32..ph + 40].copy_from_slice(&(code.len() as u64).to_le_bytes());
        f[ph + 40..ph + 48].copy_from_slice(&(code.len() as u64).to_le_bytes());
        f[0x1000..].copy_from_slice(code);
        f
    }

    #[test]
    fn the_kernel_runs_and_its_device_accesses_are_recorded() {
        let mut code = vec![0x48, 0xBB];
        code.extend_from_slice(&0xE000_0000u64.to_le_bytes()); // mov rbx, 0xE0000000 (inside the device hole)
        code.extend_from_slice(&[0x8B, 0x03]); // mov eax, [rbx]
        code.extend_from_slice(&[0xC7, 0x43, 0x04, 0x34, 0x12, 0, 0]); // mov dword [rbx+4], 0x1234
        code.extend_from_slice(&[0xBA, 0xF8, 0x0C, 0, 0, 0xB8, 0x78, 0x56, 0x34, 0x12, 0xEF]); // mov edx,0xCF8 ; mov eax,0x12345678 ; out dx,eax
        code.push(0xF4); // hlt
        let mut sys = System::new(&kernel(&code), 8).unwrap();
        let out = sys.run(1000);
        assert_eq!(out, Outcome::Halted);
        let first = &sys.hw.log[0];
        assert_eq!((first.addr, first.size, first.write), (0xE000_0000, 4, false));
        assert_eq!(first.rip, 0xFFFF_FFFF_8220_0000 + 10, "the read is the instruction after the 10-byte mov");
        let second = &sys.hw.log[1];
        assert_eq!((second.addr, second.write, second.value), (0xE000_0004, true, 0x1234));
        assert_eq!(sys.hw.regions[&(0xE000_0000 >> 16)].reads, 1);
        let port = sys.hw.log.iter().find(|a| a.port).expect("the port write is logged");
        assert_eq!((port.addr, port.write, port.value), (0xCF8, true, 0x1234_5678));
        let text = sys.report(&out);
        assert!(text.contains("0x00e0000000") && text.contains("port   0x0cf8"), "{text}");
    }

    #[test]
    fn ram_is_split_around_the_device_hole() {
        let hw = Hw::new(8 << 30);
        assert!(hw.is_ram(0x1000));
        assert!(!hw.is_ram(0xC000_0000), "the hole starts at 3 GB");
        assert!(!hw.is_ram(0xFFFF_FFFF));
        assert!(hw.is_ram(0x1_0000_0000), "the rest of RAM sits above 4 GB");
        assert!(hw.is_ram(0x2_3FFF_FFFF));
        assert!(!hw.is_ram(0x2_4000_0000));
    }

    #[test]
    fn rejects_files_that_are_not_a_kernel() {
        assert!(System::new(b"not an elf at all, just text", 8).is_err());
        let mut e = kernel(&[0xF4]);
        e[18] = 0x28; // ARM
        assert!(System::new(&e, 8).err().unwrap().contains("x86"));
    }
}
