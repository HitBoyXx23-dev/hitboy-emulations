//! Atari 2600 (VCS): our 6507 (the 6502 core with decimal mode, 13 address lines), TIA,
//! RIOT and cartridge bank switching.
//!
//! Timing: a store's write lands on the last cycle of its instruction, so TIA writes are
//! queued during a CPU step and applied after the beam has advanced to that cycle.

pub mod cart;
pub mod riot;
pub mod tia;

use cart::Cart;
use cpu6502::Cpu;
use riot::Riot;
use tia::Tia;

pub const WIDTH: usize = tia::WIDTH;
pub const HEIGHT: usize = tia::HEIGHT;
pub const FPS: f64 = 3_579_545.0 / 228.0 / 262.0;

#[derive(state::Stateful)]
pub struct Bus {
    pub tia: Tia,
    pub riot: Riot,
    pub cart: Cart,
    pending: Vec<(u16, u8)>,
}

impl cpu6502::Bus for Bus {
    fn read(&mut self, addr: u16) -> u8 {
        let a = addr & 0x1FFF;
        let v = if a & 0x1000 != 0 {
            self.cart.read(a as usize)
        } else if a & 0x80 == 0 {
            self.tia.read(a)
        } else if a & 0x200 == 0 {
            self.riot.ram[(a & 0x7F) as usize]
        } else {
            self.riot.read_io(a)
        };
        self.cart.snoop(a, v);
        v
    }

    fn write(&mut self, addr: u16, v: u8) {
        let a = addr & 0x1FFF;
        self.cart.snoop(a, v);
        if a & 0x1000 != 0 {
            self.cart.write(a as usize, v);
        } else if a & 0x80 == 0 {
            self.cart.tia_write(a, v);
            self.pending.push((a, v));
        } else if a & 0x200 == 0 {
            self.riot.ram[(a & 0x7F) as usize] = v;
        } else {
            self.riot.write_io(a, v);
        }
    }
}

pub struct Atari {
    cpu: Cpu<Bus>,
}

impl state::Stateful for Atari {
    fn sync(&mut self, s: &mut state::State) {
        s.check(0x4132_0000 ^ self.cpu.bus().cart.rom_len() as u32);
        self.cpu.sync_state(s);
        self.cpu.bus_mut().sync(s);
    }
}

impl Atari {
    pub fn new(rom: &[u8]) -> Result<Atari, String> {
        let cart = Cart::new(rom)?;
        let mut cpu = Cpu::new(Bus { tia: Tia::new(), riot: Riot::new(), cart, pending: Vec::new() });
        cpu.set_decimal(true);
        cpu.reset();
        Ok(Atari { cpu })
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    /// One CPU instruction plus the TIA/RIOT time it takes (and any WSYNC stall).
    fn step(&mut self) {
        let cycles = self.cpu.step();
        let bus = self.cpu.bus_mut();
        bus.riot.tick(cycles);
        bus.cart.tick(cycles);
        bus.tia.run((cycles - 1) * 3);
        let mut wsync = false;
        for (a, v) in std::mem::take(&mut bus.pending) {
            match a & 0x3F {
                0x02 => wsync = true,
                0x03 => bus.tia.clock = 0, // RSYNC
                _ => bus.tia.write(a, v),
            }
        }
        bus.tia.run(3);
        if wsync {
            // The CPU halts until the start of the next line.
            let clocks = bus.tia.clocks_to_line_end() % tia::CLOCKS_PER_LINE;
            bus.tia.run(clocks);
            bus.riot.tick(clocks.div_ceil(3));
        }
    }

    /// Runs until the game starts its next frame (VSYNC), or a frame's worth of lines.
    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    pub fn run_frame(&mut self) {
        self.cpu.bus_mut().tia.frame_done = false;
        let start_line = self.cpu.bus().tia.line;
        loop {
            self.step();
            let t = &self.cpu.bus().tia;
            if t.frame_done || t.line.wrapping_sub(start_line) > 320 && t.line > 320 {
                break;
            }
        }
        if !self.cpu.bus().tia.frame_done {
            // A game that never sends VSYNC: wrap the beam ourselves.
            self.cpu.bus_mut().tia.line = 0;
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.cpu.bus().tia.frame
    }

    /// Buttons: A/B fire, Select, Start (= console Reset), Up, Down, Left, Right.
    pub fn set_buttons(&mut self, pad: usize, b: u16) {
        let bus = self.cpu.bus_mut();
        let dirs = ((b >> 4) & 1) | ((b >> 5) & 1) << 1 | ((b >> 6) & 1) << 2 | ((b >> 7) & 1) << 3; // up, down, left, right
        let nibble = !(dirs as u8) & 0x0F;
        match pad {
            0 => {
                bus.riot.swcha = (bus.riot.swcha & 0x0F) | nibble << 4;
                bus.tia.fire[0] = b & 3 != 0;
                bus.riot.swchb = (bus.riot.swchb & !0x03) | (b & 8 == 0) as u8 | ((b & 4 == 0) as u8) << 1;
            }
            1 => {
                bus.riot.swcha = (bus.riot.swcha & 0xF0) | nibble;
                bus.tia.fire[1] = b & 3 != 0;
            }
            _ => {}
        }
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.cpu.bus_mut().tia.samples);
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.cpu.bus_mut().tia.set_sample_rate(rate);
    }

    pub fn cpu(&self) -> &Cpu<Bus> {
        &self.cpu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-assembled 4 KB game: a kernel that shows a coloured band on some lines and
    /// counts frames in BCD, with WSYNC/VSYNC like a real cartridge.
    fn test_rom() -> Vec<u8> {
        let prog: &[u8] = &[
            0x78, 0xD8, // SEI, CLD
            0xA2, 0xFF, 0x9A, // LDX #$FF, TXS
            0xA9, 0x00, 0x85, 0x80, // LDA #0, STA $80 (BCD frame counter)
            // frame:
            0xA9, 0x02, 0x85, 0x00, // VSYNC on
            0x85, 0x02, 0x85, 0x02, 0x85, 0x02, // 3 lines
            0xA9, 0x00, 0x85, 0x00, // VSYNC off
            0xA2, 0x25, // LDX #37
            0x85, 0x02, 0xCA, 0xD0, 0xFB, // vblank lines: STA WSYNC, DEX, BNE
            0xA9, 0x00, 0x85, 0x01, // VBLANK off
            0xA9, 0x44, 0x85, 0x09, // COLUBK red
            0xA2, 0x60, // 96 lines red
            0x85, 0x02, 0xCA, 0xD0, 0xFB,
            0xA9, 0x00, 0x85, 0x09, // COLUBK black
            0xA2, 0x60, // 96 lines black
            0x85, 0x02, 0xCA, 0xD0, 0xFB,
            0xA9, 0x02, 0x85, 0x01, // VBLANK on
            0xA2, 0x1E, // 30 overscan lines
            0x85, 0x02, 0xCA, 0xD0, 0xFB,
            0xF8, 0x18, 0xA5, 0x80, 0x69, 0x01, 0x85, 0x80, 0xD8, // SED CLC LDA $80 ADC #1 STA $80 CLD
            0x4C, 0x09, 0xF0, // JMP frame
        ];
        let mut rom = vec![0xEAu8; 4096];
        rom[..prog.len()].copy_from_slice(prog);
        rom[0xFFC] = 0x00;
        rom[0xFFD] = 0xF0;
        rom
    }

    #[test]
    fn save_state_resumes_identically() {
        let rom = test_rom();
        let mut a = Atari::new(&rom).unwrap();
        for _ in 0..4 {
            a.run_frame();
        }
        let at = a.cpu().bus().riot.ram[0];
        let snap = a.save_state();
        for _ in 0..6 {
            a.run_frame();
        }
        let want = (a.cpu().bus().riot.ram[0], a.frame().to_vec());
        assert_ne!(want.0, at, "the frame counter advanced");
        let mut b = Atari::new(&rom).unwrap();
        b.load_state(&snap).unwrap();
        assert_eq!(b.cpu().bus().riot.ram[0], at);
        for _ in 0..6 {
            b.run_frame();
        }
        assert_eq!((b.cpu().bus().riot.ram[0], b.frame().to_vec()), want);
    }

    #[test]
    fn runs_a_frame_with_correct_picture_and_bcd() {
        let mut a = Atari::new(&test_rom()).unwrap();
        for _ in 0..12 {
            a.run_frame();
        }
        // Frame counter in BCD: 11 or 12 completed frames depending on the first partial one.
        let n = a.cpu().bus().riot.ram[0];
        assert!(n == 0x11 || n == 0x12, "BCD counter {n:#x}");
        // 37 vblank lines after VSYNC, TOP = 24: red starts at row 13 of our window.
        let f = a.frame();
        let red = f[20 * WIDTH + 80];
        assert_ne!(red, 0);
        assert_eq!(f[(13 + 95) * WIDTH + 10], red);
        assert_eq!(f[(13 + 97) * WIDTH + 10], 0, "black half");
        assert_eq!(f[5 * WIDTH + 10], 0, "vblank is black");
    }

    #[test]
    fn joystick_and_console_switches() {
        let mut a = Atari::new(&test_rom()).unwrap();
        a.set_buttons(0, 1 << 4 | 1 << 7 | 1 | 8); // up + right + fire + start
        let bus = a.cpu.bus_mut();
        assert_eq!(bus.riot.swcha >> 4, 0b0110, "right and up pulled low");
        assert_eq!(bus.riot.swchb & 1, 0, "reset pressed");
        assert_eq!(bus.tia.read(0x0C) & 0x80, 0, "fire pressed");
    }
}
