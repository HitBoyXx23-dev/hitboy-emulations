//! The whole Xbox: Pentium III + 64 MB unified RAM + MCPX (boot ROM, interrupt
//! controllers, timers, SMBus) + NV2A, with the flash BIOS at the top of the address space.
//!
//! Boot: the CPU starts at 0xFFFFFFF0 inside the 512-byte MCPX ROM, which runs the flash's
//! "X-code" init table, decrypts the second-stage loader and jumps to it. That loader hides
//! the MCPX ROM, turns on paging and starts the kernel. We execute every step natively.

use crate::chips::{Pics, Pit, ACPI_HZ, PIT_HZ};
use crate::nv2a::Nv2a;
use crate::pci::Pci;
use crate::smbus::SmBus;
use cpui686::{Bus, Cpu};

pub const RAM_SIZE: usize = 64 << 20;
pub const CPU_HZ: u64 = 733_333_333;

pub const IRQ_TIMER: u8 = 0;
pub const IRQ_NV2A: u8 = 3;
pub const IRQ_SMBUS: u8 = 11;

pub struct Hw {
    pub ram: Vec<u8>,
    pub flash: Vec<u8>,
    pub mcpx: Option<Vec<u8>>,
    pub pci: Pci,
    pub pics: Pics,
    pub pit: Pit,
    pub smbus: SmBus,
    pub nv2a: Nv2a,
    /// Emulated time in CPU cycles.
    pub cycles: u64,
    /// POST / debug codes written to port 0x80, most recent last.
    pub post: Vec<u8>,
    port61: u8,
}

impl Hw {
    fn ns(&self) -> u64 {
        (self.cycles as u128 * 1_000_000_000 / CPU_HZ as u128) as u64
    }

    fn rom_byte(&self, a: u32) -> u8 {
        if let (Some(m), false) = (&self.mcpx, self.pci.mcpx_hidden) {
            if a >= 0xFFFF_FE00 {
                return m[(a - 0xFFFF_FE00) as usize];
            }
        }
        if self.flash.is_empty() {
            return 0xFF;
        }
        // The flash chip is mirrored across the top 16 MB.
        self.flash[(a as usize - 0xFF00_0000) % self.flash.len()]
    }

    fn ram_index(a: u32) -> Option<usize> {
        match a {
            0..=0x07FF_FFFF | 0xF000_0000..=0xF7FF_FFFF => Some(a as usize & (RAM_SIZE - 1)),
            _ => None,
        }
    }
}

impl Bus for Hw {
    #[inline]
    fn ram_index(&self, a: u64) -> Option<usize> {
        let Ok(a) = u32::try_from(a) else { return None };

        Self::ram_index(a)
    }
    fn ram(&mut self) -> &mut [u8] {
        &mut self.ram
    }
    fn read8(&mut self, a: u64) -> u8 {
        let Ok(a) = u32::try_from(a) else { return 0 };

        if let Some(i) = Self::ram_index(a) {
            return self.ram[i];
        }
        match a {
            0xFD00_0000..=0xFDFF_FFFF => {
                let ns = self.ns();
                self.nv2a.read8(a - 0xFD00_0000, ns)
            }
            0xFF00_0000..=0xFFFF_FFFF => self.rom_byte(a),
            _ => 0,
        }
    }
    fn write8(&mut self, a: u64, v: u8) {
        let Ok(a) = u32::try_from(a) else { return };

        if let Some(i) = Self::ram_index(a) {
            self.ram[i] = v;
            return;
        }
        if let 0xFD00_0000..=0xFDFF_FFFF = a {
            self.nv2a.write8(a - 0xFD00_0000, v);
        }
    }
    fn read32(&mut self, a: u64) -> u32 {
        let Ok(a) = u32::try_from(a) else { return 0 };

        if let Some(i) = Self::ram_index(a) {
            if i + 4 <= RAM_SIZE {
                return u32::from_le_bytes(self.ram[i..i + 4].try_into().unwrap());
            }
        }
        if (0xFD00_0000..=0xFDFF_FFFF).contains(&a) && a & 3 == 0 {
            let ns = self.ns();
            return self.nv2a.read32(a - 0xFD00_0000, ns);
        }
        (0..4).fold(0, |v, k| v | (self.read8(u64::from(a.wrapping_add(k))) as u32) << (k * 8))
    }
    fn write32(&mut self, a: u64, v: u32) {
        let Ok(a) = u32::try_from(a) else { return };

        if let Some(i) = Self::ram_index(a) {
            if i + 4 <= RAM_SIZE {
                self.ram[i..i + 4].copy_from_slice(&v.to_le_bytes());
                return;
            }
        }
        if (0xFD00_0000..=0xFDFF_FFFF).contains(&a) && a & 3 == 0 {
            self.nv2a.write32(a - 0xFD00_0000, v);
            return;
        }
        for k in 0..4 {
            self.write8(u64::from(a.wrapping_add(k)), (v >> (k * 8)) as u8);
        }
    }

    fn io_in(&mut self, port: u16, size: u8) -> u32 {
        match port {
            0x20 | 0x21 => self.pics.m.read(port == 0x20) as u32,
            0xA0 | 0xA1 => self.pics.s.read(port == 0xA0) as u32,
            0x40 => self.pit.read() as u32,
            0x61 => {
                // Refresh toggle (bit 4) flips so delay loops polling it make progress.
                self.port61 ^= 0x10;
                self.port61 as u32
            }
            0xCF8 => self.pci.addr,
            0xCFC..=0xCFF => self.pci.read(port - 0xCFC, size),
            // ACPI PM timer (24-bit).
            0x8008 => ((self.cycles as u128 * ACPI_HZ as u128 / CPU_HZ as u128) as u32) & 0xFF_FFFF,
            0xC000..=0xC00F => {
                let mut v = 0;
                for k in 0..size as u16 {
                    v |= (self.smbus.read(port - 0xC000 + k) as u32) << (k * 8);
                }
                v
            }
            _ => match size {
                1 => 0xFF,
                2 => 0xFFFF,
                _ => u32::MAX,
            },
        }
    }

    fn io_out(&mut self, port: u16, size: u8, v: u32) {
        match port {
            0x20 | 0x21 => self.pics.m.write(port == 0x20, v as u8),
            0xA0 | 0xA1 => self.pics.s.write(port == 0xA0, v as u8),
            0x40..=0x43 => self.pit.write(port, v as u8),
            0x61 => self.port61 = v as u8,
            0x80 => {
                self.post.push(v as u8);
                if self.post.len() > 256 {
                    self.post.drain(..128);
                }
            }
            0xCF8 => self.pci.addr = v,
            0xCFC..=0xCFF => self.pci.write(port - 0xCFC, size, v),
            0xC000..=0xC00F => {
                for k in 0..size as u16 {
                    self.smbus.write(port - 0xC000 + k, (v >> (k * 8)) as u8);
                }
            }
            _ => {}
        }
    }
}

pub struct Xbox {
    pub cpu: Cpu,
    pub hw: Hw,
    pit_acc: u64,
    next_vblank: u64,
}

/// Steps between timer updates.
const SLICE: u64 = 256;

impl Xbox {
    /// Creates the machine from a flash BIOS image and, for retail flashes, the MCPX ROM.
    pub fn new(flash: Vec<u8>, mcpx: Option<Vec<u8>>) -> Result<Self, String> {
        if !flash.is_empty() && !matches!(flash.len(), 0x40000 | 0x80000 | 0x100000) {
            return Err("The Xbox flash BIOS must be 256 KB, 512 KB or 1 MB.".into());
        }
        if mcpx.as_ref().is_some_and(|m| m.len() != 512) {
            return Err("The MCPX boot ROM must be exactly 512 bytes.".into());
        }
        let hw = Hw {
            ram: vec![0; RAM_SIZE],
            flash,
            mcpx,
            pci: Pci::default(),
            pics: Pics::default(),
            pit: Pit::default(),
            smbus: SmBus::default(),
            nv2a: Nv2a::new(RAM_SIZE as u32),
            cycles: 0,
            post: Vec::new(),
            port61: 0,
        };
        Ok(Xbox { cpu: Cpu::new(), hw, pit_acc: 0, next_vblank: CPU_HZ / 60 })
    }

    /// Runs `n` instructions, advancing timers and delivering interrupts.
    pub fn run(&mut self, n: u64) {
        let mut left = n;
        while left > 0 {
            let chunk = left.min(SLICE);
            for _ in 0..chunk {
                self.cpu.step(&mut self.hw);
                if self.cpu.halted && self.cpu.eflags & cpui686::flags::IF == 0 {
                    break;
                }
            }
            left -= chunk;
            self.advance(chunk);
            self.deliver();
        }
    }

    fn advance(&mut self, cycles: u64) {
        self.hw.cycles += cycles;
        self.pit_acc += cycles * PIT_HZ;
        let ticks = self.pit_acc / CPU_HZ;
        self.pit_acc %= CPU_HZ;
        if self.hw.pit.tick(ticks) > 0 {
            self.hw.pics.raise(IRQ_TIMER);
        }
        if self.hw.cycles >= self.next_vblank {
            self.next_vblank += CPU_HZ / 60;
            if self.hw.nv2a.vblank() {
                self.hw.pics.raise(IRQ_NV2A);
            }
        }
        if self.hw.smbus.take_irq() {
            self.hw.pics.raise(IRQ_SMBUS);
        }
    }

    fn deliver(&mut self) {
        if !self.cpu.can_interrupt() {
            return;
        }
        if let Some(v) = self.hw.pics.next_vector() {
            self.cpu.irq(&mut self.hw, v);
        }
    }

    /// The picture the NV2A is scanning out, as 0x00RRGGBB pixels.
    pub fn frame(&self) -> (Vec<u32>, u32, u32) {
        let (start, pitch, w, h, bpp) = self.hw.nv2a.scanout();
        let (w, h) = (w.min(1920), h.min(1080));
        let mut out = vec![0u32; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let a = (start + y * pitch + x * bpp) as usize & (RAM_SIZE - 1);
                let r = &self.hw.ram;
                out[(y * w + x) as usize] = match bpp {
                    4 => u32::from_le_bytes([r[a], r[a + 1], r[a + 2], 0]),
                    2 => {
                        let p = u16::from_le_bytes([r[a], r[a + 1]]) as u32;
                        let (r5, g6, b5) = ((p >> 11) & 31, (p >> 5) & 63, p & 31);
                        (r5 * 255 / 31) << 16 | (g6 * 255 / 63) << 8 | b5 * 255 / 31
                    }
                    _ => r[a] as u32 * 0x010101,
                };
            }
        }
        (out, w, h)
    }

    /// Last POST code written to port 0x80 (shows how far the boot got).
    pub fn post_code(&self) -> Option<u8> {
        self.hw.post.last().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 256 KB flash with a tiny boot program (no MCPX ROM, like a modded BIOS):
    /// real mode -> protected mode -> draw a pixel -> set the scan-out address.
    fn test_flash() -> Vec<u8> {
        let mut f = vec![0xFFu8; 0x40000];
        let at = |a: u32| (a - 0xFFFC_0000) as usize;
        // Code at 0xFFFFF000 (16-bit, reached from the reset vector with CS base 0xFFFF0000).
        let code16: &[u8] = &[
            0xB0, 0x10, 0xE6, 0x80, // out 0x80, 0x10
            0x66, 0x2E, 0x0F, 0x01, 0x16, 0x00, 0xF1, // lgdt cs:[0xF100]
            0x0F, 0x20, 0xC0, 0x0C, 0x01, 0x0F, 0x22, 0xC0, // cr0 |= 1
            0x66, 0xEA, 0x00, 0xF2, 0xFF, 0xFF, 0x08, 0x00, // jmp far 0x08:0xFFFFF200
        ];
        f[at(0xFFFF_F000)..at(0xFFFF_F000) + code16.len()].copy_from_slice(code16);
        // GDT pointer at 0xFFFFF100 -> GDT at 0xFFFFF110.
        f[at(0xFFFF_F100)..at(0xFFFF_F102)].copy_from_slice(&23u16.to_le_bytes());
        f[at(0xFFFF_F102)..at(0xFFFF_F106)].copy_from_slice(&0xFFFF_F110u32.to_le_bytes());
        for (i, d) in [0u64, 0x00CF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF].iter().enumerate() {
            f[at(0xFFFF_F110) + i * 8..at(0xFFFF_F118) + i * 8].copy_from_slice(&d.to_le_bytes());
        }
        // 32-bit code at 0xFFFFF200.
        let code32: &[u8] = &[
            0x66, 0xB8, 0x10, 0x00, 0x8E, 0xD8, // mov ax,0x10; mov ds,ax
            0xC7, 0x05, 0x04, 0x00, 0x10, 0x00, 0x56, 0x34, 0x12, 0x00, // mov dword [0x100004], 0x123456
            0xC7, 0x05, 0x00, 0x08, 0x60, 0xFD, 0x00, 0x00, 0x10, 0x00, // mov dword [0xFD600800], 0x100000
            0xB0, 0x20, 0xE6, 0x80, // out 0x80, 0x20
            0xF4,
        ];
        f[at(0xFFFF_F200)..at(0xFFFF_F200) + code32.len()].copy_from_slice(code32);
        // Reset vector: jmp 0xF000 (near, within CS = F000:).
        let rv = at(0xFFFF_FFF0);
        f[rv..rv + 3].copy_from_slice(&[0xE9, 0x0D, 0xF0]); // 0xFFF3 + 0xF00D = 0xF000 (mod 64K)
        f
    }

    #[test]
    fn boots_a_flash_program_to_the_screen() {
        let mut x = Xbox::new(test_flash(), None).unwrap();
        x.run(1000);
        assert_eq!(x.hw.post, vec![0x10, 0x20]);
        assert!(x.cpu.protected());
        let (px, w, h) = x.frame();
        assert_eq!((w, h), (640, 480));
        assert_eq!(px[1], 0x123456);
    }

    #[test]
    fn mcpx_rom_overlays_the_flash_until_hidden() {
        let mut x = Xbox::new(vec![0xAA; 0x40000], Some(vec![0x55; 512])).unwrap();
        assert_eq!(x.hw.read8(0xFFFF_FFF0), 0x55);
        assert_eq!(x.hw.read8(0xFFFF_FD00), 0xAA);
        x.hw.io_out(0xCF8, 4, 0x8000_0880);
        x.hw.io_out(0xCFC, 1, 2);
        assert_eq!(x.hw.read8(0xFFFF_FFF0), 0xAA);
    }

    #[test]
    fn timer_interrupts_reach_the_cpu() {
        let mut x = Xbox::new(test_flash(), None).unwrap();
        // Program the PICs (base 0x30) and PIT (1 ms), IDT with vector 0x30 -> handler.
        let h = &mut x.hw;
        for (cmd, v) in [(true, 0x11u8), (false, 0x30), (false, 0x04), (false, 0x01), (false, 0xFE)] {
            h.pics.m.write(cmd, v);
        }
        h.pit.write(0x43, 0x34);
        h.pit.write(0x40, 0x65);
        h.pit.write(0x40, 0x04);
        // Flat protected mode, code at 0x1000: sti; l: hlt; jmp l. Handler at 0x2000: inc ebx; iret.
        h.ram[0x1000..0x1004].copy_from_slice(&[0xFB, 0xF4, 0xEB, 0xFD]);
        h.ram[0x2000..0x2002].copy_from_slice(&[0x43, 0xCF]);
        let gate: u64 = 0x2000 | (8 << 16) | (0x8E00u64 << 32);
        h.ram[0x800 + 0x30 * 8..0x808 + 0x30 * 8].copy_from_slice(&gate.to_le_bytes());
        for (i, d) in [0u64, 0x00CF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF].iter().enumerate() {
            h.ram[0x100 + i * 8..0x108 + i * 8].copy_from_slice(&d.to_le_bytes());
        }
        let c = &mut x.cpu;
        c.gdtr = (0x100, 23);
        c.idtr = (0x800, 0x7FF);
        c.cr0 |= 1;
        c.load_seg(&mut x.hw, cpui686::CS, 8).unwrap();
        for s in [cpui686::DS, cpui686::ES, cpui686::SS] {
            x.cpu.load_seg(&mut x.hw, s, 0x10).unwrap();
        }
        x.cpu.rip = 0x1000;
        x.cpu.r[cpui686::ESP] = 0x9000;
        // 10 ms of emulated time -> about 10 timer ticks. EOIs aren't sent, so the handler
        // only runs once until the PIC is acknowledged; send them between slices.
        for _ in 0..10 {
            x.run(CPU_HZ / 1000);
            x.hw.pics.m.write(true, 0x20);
        }
        assert!((9..=11).contains(&x.cpu.r[cpui686::EBX]), "{}", x.cpu.r[cpui686::EBX]);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// cargo test -p xbox --release -- --ignored --nocapture bench
    #[test]
    #[ignore]
    fn bench_mips() {
        let mut x = Xbox::new(vec![0xFF; 0x40000], None).unwrap();
        // Flat protected mode loop: add eax,[ebx]; inc ebx; and ebx,0xFFFF; dec ecx; jmp back.
        let h = &mut x.hw;
        h.ram[0x1000..0x100D].copy_from_slice(&[0x03, 0x03, 0x43, 0x81, 0xE3, 0xFF, 0xFF, 0, 0, 0x49, 0xEB, 0xF4, 0xF4]);
        for (i, d) in [0u64, 0x00CF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF].iter().enumerate() {
            h.ram[0x100 + i * 8..0x108 + i * 8].copy_from_slice(&d.to_le_bytes());
        }
        x.cpu.gdtr = (0x100, 23);
        x.cpu.cr0 |= 1;
        x.cpu.load_seg(&mut x.hw, cpui686::CS, 8).unwrap();
        for s in [cpui686::DS, cpui686::SS] {
            x.cpu.load_seg(&mut x.hw, s, 0x10).unwrap();
        }
        x.cpu.rip = 0x1000;
        let t = std::time::Instant::now();
        x.run(50_000_000);
        let s = t.elapsed().as_secs_f64();
        println!("{:.1} million instructions/s", 50.0 / s);
        // Raw CPU stepping without the machine loop, on NOPs.
        for i in 0..0x1000 {
            x.hw.ram[0x3000 + i] = 0x90;
        }
        x.hw.ram[0x4000..0x4005].copy_from_slice(&[0xE9, 0xFB, 0xEF, 0xFF, 0xFF]);
        x.cpu.rip = 0x3000;
        let t = std::time::Instant::now();
        for _ in 0..50_000_000 {
            x.cpu.step(&mut x.hw);
        }
        println!("nop: {:.1} million/s", 50.0 / t.elapsed().as_secs_f64());
    }
}
