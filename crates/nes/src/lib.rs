//! Nintendo Entertainment System. The CPU is the C++ `cpu6502` core; everything
//! else (bus, PPU, cartridges, input) lives here in Rust.

pub mod apu;
pub mod cartridge;
pub mod controller;
pub mod fds;
pub(crate) mod mappers;
pub mod ppu;

use cartridge::{Mapper, RomError};
use controller::Controller;
use cpu6502::Cpu;
use ppu::Ppu;

pub struct Bus {
    ram: [u8; 0x800],
    pub ppu: Ppu,
    pub apu: apu::Apu,
    cart: Box<dyn Mapper>,
    pub pads: [Controller; 2],
    /// CPU cycles stolen by OAM DMA, consumed by `Nes::step`.
    stall: u32,
}

impl cpu6502::Bus for Bus {
    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1FFF => self.ram[addr as usize & 0x7FF],
            0x2000..=0x3FFF => self.ppu.read_register(addr, self.cart.as_mut()),
            0x4015 => self.apu.read_status(),
            0x4016 => self.pads[0].read(),
            0x4017 => self.pads[1].read(),
            0x4000..=0x401F => 0,
            _ => self.cart.cpu_read(addr),
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.ram[addr as usize & 0x7FF] = value,
            0x2000..=0x3FFF => self.ppu.write_register(addr, value, self.cart.as_mut()),
            0x4014 => {
                let page = (value as u16) << 8;
                for i in 0..256u16 {
                    let b = self.read(page | i);
                    self.ppu.oam_dma_write(b);
                }
                self.stall += 513;
            }
            0x4016 => {
                self.pads[0].write(value);
                self.pads[1].write(value);
            }
            0x4000..=0x4013 | 0x4015 | 0x4017 => self.apu.write(addr, value),
            0x4000..=0x401F => {}
            _ => self.cart.cpu_write(addr, value),
        }
    }
}

impl state::Stateful for Bus {
    fn sync(&mut self, s: &mut state::State) {
        self.ram.sync(s);
        self.ppu.sync(s);
        self.apu.sync(s);
        self.cart.sync(s);
        self.pads.sync(s);
        self.stall.sync(s);
    }
}

pub struct Nes {
    cpu: Cpu<Bus>,
}

impl state::Stateful for Nes {
    fn sync(&mut self, s: &mut state::State) {
        s.check(0x4E45_5301);
        self.cpu.sync_state(s);
        self.cpu.bus_mut().sync(s);
    }
}

impl Nes {
    pub fn from_rom(rom: &[u8]) -> Result<Self, RomError> {
        let cart = cartridge::load(rom)?;
        Ok(Self::with_cart(cart))
    }

    /// A Famicom Disk System game (.fds) with the user's disksys.rom BIOS.
    pub fn from_fds(disk: &[u8], bios: Vec<u8>) -> Result<Self, String> {
        let sides = fds::load_sides(disk)?;
        let cart = fds::Fds::new(bios, sides)?;
        Ok(Self::with_cart(Box::new(cart)))
    }

    fn with_cart(cart: Box<dyn Mapper>) -> Self {
        let bus = Bus {
            ram: [0; 0x800],
            ppu: Ppu::default(),
            apu: apu::Apu::default(),
            cart,
            pads: Default::default(),
            stall: 0,
        };
        let mut cpu = Cpu::new(bus);
        cpu.reset();
        Self { cpu }
    }

    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    /// Disk data or battery RAM to keep between sessions.
    pub fn save_data(&self) -> Option<Vec<u8>> {
        self.cpu.bus().cart.save()
    }

    pub fn load_save_data(&mut self, data: &[u8]) {
        self.cpu.bus_mut().cart.load_save(data);
    }

    /// Famicom Disk System: eject and insert the next disk side.
    pub fn next_disk_side(&mut self) -> Option<usize> {
        self.cpu.bus_mut().cart.next_disk_side()
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    /// Runs one CPU instruction plus the matching PPU time.
    pub fn step(&mut self) -> u32 {
        let mut cycles = self.cpu.step();
        let bus = self.cpu.bus_mut();
        cycles += std::mem::take(&mut bus.stall);
        bus.ppu.tick(cycles * 3, bus.cart.as_mut());
        let Bus { apu, cart, .. } = bus;
        cart.tick(cycles);
        apu.expansion = cart.audio();
        apu.tick(cycles, &mut |a| cart.cpu_read(a));
        let irq = cart.irq() || apu.frame_irq;
        if bus.ppu.take_nmi() {
            self.cpu.nmi();
        }
        self.cpu.set_irq(irq);
        cycles
    }

    /// Runs until the PPU finishes a frame (enters vblank).
    pub fn run_frame(&mut self) {
        self.cpu.bus_mut().ppu.frame_complete = false;
        while !self.cpu.bus().ppu.frame_complete {
            self.step();
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.cpu.bus().ppu.frame
    }

    /// Sets the pressed buttons for pad 0 or 1 (bitmask of `controller::button::*`).
    pub fn set_buttons(&mut self, pad: usize, buttons: u8) {
        self.cpu.bus_mut().pads[pad].buttons = buttons;
    }

    /// Moves generated audio (interleaved stereo f32) into out.
    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.cpu.bus_mut().apu.samples);
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.cpu.bus_mut().apu.set_sample_rate(rate);
    }

    /// Reads a byte from CPU address space (debugging / tests).
    pub fn peek(&mut self, addr: u16) -> u8 {
        cpu6502::Bus::read(self.cpu.bus_mut(), addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a 16 KiB NROM image whose code starts at $C000.
    fn rom(code: &[u8], nmi: &[u8]) -> Vec<u8> {
        let mut prg = vec![0xEA; 0x4000];
        prg[..code.len()].copy_from_slice(code);
        prg[0x100..0x100 + nmi.len()].copy_from_slice(nmi);
        prg[0x3FFA..].copy_from_slice(&[0x00, 0xC1, 0x00, 0xC0, 0x00, 0xC0]);
        let mut out = b"NES\x1A\x01\x01\x00\x00".to_vec();
        out.resize(16, 0);
        out.extend(prg);
        out.extend(vec![0u8; 0x2000]);
        out
    }

    #[test]
    fn nmi_fires_once_per_frame() {
        // LDA #$80; STA $2000 (enable NMI); LDA #$1E; STA $2001; loop: JMP loop
        let code = [0xA9, 0x80, 0x8D, 0x00, 0x20, 0xA9, 0x1E, 0x8D, 0x01, 0x20, 0x4C, 0x0A, 0xC0];
        // NMI: INC $10; RTI
        let nmi = [0xE6, 0x10, 0x40];
        let mut nes = Nes::from_rom(&rom(&code, &nmi)).unwrap();
        for _ in 0..10 {
            nes.run_frame();
        }
        // The NMI handler runs just after each vblank start; allow for the last one pending.
        let n = nes.peek(0x10);
        assert!((9..=10).contains(&n), "nmi count {n}");
    }

    /// After a restore the game must carry on exactly as if it had never stopped: same
    /// registers, memory and picture, frame after frame.
    #[test]
    fn save_state_resumes_identically() {
        // Enable NMI + rendering, then count in a busy loop ($11) while the NMI counts frames ($10).
        let code = [0xA9, 0x80, 0x8D, 0x00, 0x20, 0xA9, 0x1E, 0x8D, 0x01, 0x20, 0xE6, 0x11, 0x4C, 0x0A, 0xC0];
        let nmi = [0xE6, 0x10, 0x40];
        let image = rom(&code, &nmi);
        let mut a = Nes::from_rom(&image).unwrap();
        for _ in 0..7 {
            a.run_frame();
        }
        let snap = a.save_state();
        for _ in 0..9 {
            a.run_frame();
        }
        let want = (a.peek(0x10), a.peek(0x11), a.cpu.regs(), a.frame().to_vec());

        let mut b = Nes::from_rom(&image).unwrap();
        b.load_state(&snap).unwrap();
        for _ in 0..9 {
            b.run_frame();
        }
        assert_eq!((b.peek(0x10), b.peek(0x11), b.cpu.regs(), b.frame().to_vec()), want);
        // Loading doesn't depend on what the machine was doing before.
        for _ in 0..30 {
            b.run_frame();
        }
        b.load_state(&snap).unwrap();
        for _ in 0..9 {
            b.run_frame();
        }
        assert_eq!((b.peek(0x10), b.peek(0x11)), (want.0, want.1));
    }

    /// Cartridge bank registers are part of the state, not just RAM.
    #[test]
    fn save_state_restores_bank_switching() {
        // UxROM (mapper 2): 2 x 16 KiB, bank 0 holds 0xAA, bank 1 holds 0xBB at $8010.
        let mut prg = vec![0u8; 0x8000];
        prg[0x10] = 0xAA;
        prg[0x4010] = 0xBB;
        prg[0x7FFA..].copy_from_slice(&[0x00, 0xC1, 0x00, 0xC0, 0x00, 0xC0]);
        let mut image = b"NES   ".to_vec();
        image.resize(16, 0);
        image.extend(prg);
        let mut nes = Nes::from_rom(&image).unwrap();
        cpu6502::Bus::write(nes.cpu.bus_mut(), 0x8000, 1);
        assert_eq!(nes.peek(0x8010), 0xBB);
        let snap = nes.save_state();
        cpu6502::Bus::write(nes.cpu.bus_mut(), 0x8000, 0);
        assert_eq!(nes.peek(0x8010), 0xAA);
        nes.load_state(&snap).unwrap();
        assert_eq!(nes.peek(0x8010), 0xBB, "the bank register came back");
    }

    /// Cartridge saves: a battery game keeps its RAM across a state load.
    #[test]
    fn battery_games_keep_their_ram_in_states() {
        // Mapper 1 (MMC1) with the battery flag; $6000 is PRG RAM.
        let mut image = b"NES ".to_vec();
        image.resize(16, 0);
        let mut prg = vec![0u8; 0x4000];
        prg[0x3FFA..].copy_from_slice(&[0x00, 0xC1, 0x00, 0xC0, 0x00, 0xC0]);
        image.extend(prg);
        image.extend(vec![0u8; 0x2000]);
        let mut nes = Nes::from_rom(&image).unwrap();
        cpu6502::Bus::write(nes.cpu.bus_mut(), 0x6000, 0x5A);
        let snap = nes.save_state();
        cpu6502::Bus::write(nes.cpu.bus_mut(), 0x6000, 0x00);
        nes.load_state(&snap).unwrap();
        assert_eq!(nes.peek(0x6000), 0x5A);
        assert!(nes.save_data().is_some(), "battery RAM is exposed for the .sav file");
    }

    #[test]
    fn controller_reads_serially() {
        let mut nes = Nes::from_rom(&rom(&[0x4C, 0x00, 0xC0], &[0x40])).unwrap();
        nes.set_buttons(0, controller::button::A | controller::button::START);
        cpu6502::Bus::write(nes.cpu.bus_mut(), 0x4016, 1);
        cpu6502::Bus::write(nes.cpu.bus_mut(), 0x4016, 0);
        let bits: Vec<u8> = (0..8).map(|_| nes.peek(0x4016) & 1).collect();
        assert_eq!(bits, [1, 0, 0, 1, 0, 0, 0, 0]);
    }
}
