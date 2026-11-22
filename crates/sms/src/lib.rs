//! Sega Master System and Game Gear: our Z80, the Mode 4 VDP, SN76489 sound and the Sega
//! mapper (plus the Codemasters mapper). No BIOS is needed; the machine starts as the BIOS
//! would leave it.

pub mod psg;
pub mod vdp;

use cpuz80::Z80;
use psg::Psg;
use vdp::Vdp;

const CLOCK: f64 = 3_579_545.0;
const CYCLES_PER_LINE: u32 = 228;
pub const FPS: f64 = CLOCK / (CYCLES_PER_LINE as f64 * vdp::LINES as f64);

#[derive(Clone, Copy, PartialEq, Eq, Debug, state::Stateful)]
pub enum Mapper {
    Sega,
    Codemasters,
}

#[derive(state::Stateful)]
pub struct Bus {
    #[state(skip)]
    rom: Vec<u8>,
    ram: [u8; 0x2000],
    /// 32 KB of battery-backed cartridge RAM (two 16 KB pages).
    pub cart_ram: Vec<u8>,
    pub cart_ram_used: bool,
    banks: [usize; 3],
    ram_ctrl: u8,
    mapper: Mapper,
    pub vdp: Vdp,
    pub psg: Psg,
    /// Port $DC / $DD values (active low).
    pad: [u8; 2],
    gg_start: bool,
    #[state(skip)]
    gg: bool,
}

impl Bus {
    fn rom_byte(&self, bank: usize, off: usize) -> u8 {
        let banks = (self.rom.len() / 0x4000).max(1);
        self.rom[((bank % banks) * 0x4000 + off) % self.rom.len()]
    }
}

impl cpuz80::Bus for Bus {
    fn read(&mut self, a: u16) -> u8 {
        let a = a as usize;
        match a {
            0x0000..=0x03FF if self.mapper == Mapper::Sega => self.rom[a % self.rom.len()],
            0x0000..=0x3FFF => self.rom_byte(self.banks[0], a),
            0x4000..=0x7FFF => self.rom_byte(self.banks[1], a - 0x4000),
            0x8000..=0xBFFF => {
                if self.ram_ctrl & 0x08 != 0 {
                    let page = ((self.ram_ctrl >> 2) & 1) as usize;
                    self.cart_ram[page * 0x4000 + (a - 0x8000)]
                } else {
                    self.rom_byte(self.banks[2], a - 0x8000)
                }
            }
            _ => self.ram[a & 0x1FFF],
        }
    }

    fn write(&mut self, a: u16, v: u8) {
        let a = a as usize;
        match self.mapper {
            Mapper::Codemasters => match a {
                0x0000 => self.banks[0] = v as usize,
                0x4000 => self.banks[1] = v as usize,
                0x8000 => self.banks[2] = v as usize,
                _ => {}
            },
            Mapper::Sega => {
                if a >= 0xFFFC {
                    match a {
                        0xFFFC => self.ram_ctrl = v,
                        0xFFFD => self.banks[0] = v as usize,
                        0xFFFE => self.banks[1] = v as usize,
                        _ => self.banks[2] = v as usize,
                    }
                }
            }
        }
        match a {
            0x8000..=0xBFFF if self.ram_ctrl & 0x08 != 0 => {
                let page = ((self.ram_ctrl >> 2) & 1) as usize;
                self.cart_ram[page * 0x4000 + (a - 0x8000)] = v;
                self.cart_ram_used = true;
            }
            0xC000..=0xFFFF => self.ram[a & 0x1FFF] = v,
            _ => {}
        }
    }

    fn input(&mut self, port: u16) -> u8 {
        let p = port as u8;
        if self.gg && p == 0x00 {
            return if self.gg_start { 0x40 } else { 0xC0 };
        }
        match p & 0xC1 {
            0x40 => self.vdp.v_counter(),
            0x41 => 0, // H counter (latched by light guns only)
            0x80 => self.vdp.read_data(),
            0x81 => self.vdp.read_status(),
            0xC0 => self.pad[0],
            0xC1 => self.pad[1],
            _ => 0xFF,
        }
    }

    fn output(&mut self, port: u16, v: u8) {
        let p = port as u8;
        match p & 0xC1 {
            0x40 | 0x41 => self.psg.write(v),
            0x80 => self.vdp.write_data(v),
            0x81 => self.vdp.write_control(v),
            _ => {}
        }
    }
}

pub struct MasterSystem {
    cpu: Z80,
    pub bus: Bus,
    pause_prev: bool,
}

impl state::Stateful for MasterSystem {
    fn sync(&mut self, s: &mut state::State) {
        // Which machine and which cartridge this state belongs to.
        s.check((self.bus.gg as u32) << 31 | self.bus.rom.len() as u32 & 0x7FFF_FFFF);
        self.cpu.sync(s);
        self.bus.sync(s);
        self.pause_prev.sync(s);
    }
}

impl MasterSystem {
    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    /// `gg` selects the Game Gear (smaller screen, 12-bit colour, Start button).
    pub fn new(rom: &[u8], gg: bool) -> Result<MasterSystem, String> {
        // Some dumps carry a 512-byte copier header.
        let rom = if rom.len() % 0x4000 == 512 { &rom[512..] } else { rom };
        if rom.len() < 0x2000 || rom.len() > 8 * 1024 * 1024 {
            return Err(format!("Not a {} cartridge ({} bytes).", if gg { "Game Gear" } else { "Master System" }, rom.len()));
        }
        let mapper = if is_codemasters(rom) { Mapper::Codemasters } else { Mapper::Sega };
        let bus = Bus {
            rom: rom.to_vec(),
            ram: [0; 0x2000],
            cart_ram: vec![0; 0x8000],
            cart_ram_used: false,
            banks: [0, 1, 2],
            ram_ctrl: 0,
            mapper,
            vdp: Vdp::new(gg),
            psg: Psg::new(CLOCK),
            pad: [0xFF; 2],
            gg_start: false,
            gg,
        };
        let mut cpu = Z80::new();
        cpu.sp = 0xDFF0;
        cpu.im = 1;
        Ok(MasterSystem { cpu, bus, pause_prev: false })
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
        self.cpu.sp = 0xDFF0;
        self.cpu.im = 1;
    }

    pub fn run_frame(&mut self) {
        for _ in 0..vdp::LINES {
            let mut used = 0;
            while used < CYCLES_PER_LINE {
                self.cpu.irq_line = self.bus.vdp.irq();
                let c = self.cpu.step(&mut self.bus);
                self.bus.psg.tick(c);
                used += c;
            }
            self.bus.vdp.end_line();
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.bus.vdp.frame
    }

    pub fn size(&self) -> (usize, usize) {
        self.bus.vdp.size()
    }

    /// Buttons: A = 1, B = 2, Start = Pause (Master System) or Start (Game Gear).
    pub fn set_buttons(&mut self, pad: usize, b: u16) {
        let dirs = |b: u16| -> u8 { ((b >> 4) & 1) as u8 | (((b >> 5) & 1) as u8) << 1 | (((b >> 6) & 1) as u8) << 2 | (((b >> 7) & 1) as u8) << 3 | ((b & 1) as u8) << 4 | (((b >> 1) & 1) as u8) << 5 };
        let d = dirs(b);
        match pad {
            0 => {
                self.bus.pad[0] = (self.bus.pad[0] & 0xC0) | (!d & 0x3F);
                let start = b & 8 != 0;
                if self.bus.gg {
                    self.bus.gg_start = start;
                } else if start && !self.pause_prev {
                    self.cpu.nmi();
                }
                self.pause_prev = start;
            }
            1 => {
                // Player 2: up/down in $DC bits 6-7, the rest in $DD bits 0-3.
                self.bus.pad[0] = (self.bus.pad[0] & 0x3F) | (!(d & 3) & 3) << 6;
                self.bus.pad[1] = (self.bus.pad[1] & 0xF0) | (!(d >> 2) & 0x0F);
            }
            _ => {}
        }
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.bus.psg.samples);
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.bus.psg.set_sample_rate(r);
    }

    pub fn save_ram(&self) -> Option<&[u8]> {
        self.bus.cart_ram_used.then_some(&self.bus.cart_ram[..])
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        if data.len() == self.bus.cart_ram.len() {
            self.bus.cart_ram.copy_from_slice(data);
            self.bus.cart_ram_used = true;
        }
    }
}

/// Codemasters cartridges carry a checksum pair at $7FE6/$7FE8 that adds up to 0x10000.
fn is_codemasters(rom: &[u8]) -> bool {
    if rom.len() < 0x8000 {
        return false;
    }
    let w = |o: usize| u16::from_le_bytes([rom[o], rom[o + 1]]) as u32;
    let (sum, inv) = (w(0x7FE6), w(0x7FE8));
    sum != 0 && sum + inv == 0x10000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny cartridge: sets up the VDP, writes a palette colour, turns the display on and
    /// waits for frame interrupts, counting them in RAM.
    fn test_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        let prog: &[u8] = &[
            0xF3, // DI
            0x31, 0xF0, 0xDF, // LD SP,$DFF0
            0xED, 0x56, // IM 1
            // Register 1 = $E0 (display, frame interrupts), register 7 = 0 (backdrop colour 16).
            0x3E, 0xE0, 0xD3, 0xBF, 0x3E, 0x81, 0xD3, 0xBF,
            0x3E, 0x00, 0xD3, 0xBF, 0x3E, 0x87, 0xD3, 0xBF,
            // CRAM[0] = green (0x0C): colour 0 of the background palette.
            0x3E, 0x00, 0xD3, 0xBF, 0x3E, 0xC0, 0xD3, 0xBF, 0x3E, 0x0C, 0xD3, 0xBE,
            0xFB, // EI
            0x76, // HALT
            0x18, 0xFD, // JR -3 (back to HALT)
        ];
        rom[..prog.len()].copy_from_slice(prog);
        // IM 1 handler at $38: INC ($C000) ; IN A,($BF) (acknowledge) ; EI ; RETI
        rom[0x38..0x38 + 10].copy_from_slice(&[0x21, 0x00, 0xC0, 0x34, 0xDB, 0xBF, 0xFB, 0xED, 0x4D, 0x00]);
        rom
    }

    #[test]
    fn runs_frames_with_interrupts() {
        let mut m = MasterSystem::new(&test_rom(), false).unwrap();
        for _ in 0..5 {
            m.run_frame();
        }
        let count = m.bus.ram[0];
        assert!((4..=5).contains(&count), "frame interrupts counted: {count}");
        assert_eq!(m.frame()[100 * 256 + 100], 0x00FF00, "empty tiles show background colour 0");
    }

    /// Saving and loading must be exact: after a restore the game continues exactly as if it
    /// had never stopped.
    #[test]
    fn save_state_resumes_identically() {
        let mut a = MasterSystem::new(&test_rom(), false).unwrap();
        for _ in 0..3 {
            a.run_frame();
        }
        let snap = a.save_state();
        for _ in 0..4 {
            a.run_frame();
        }
        let (counter, picture) = (a.bus.ram[0], a.frame().to_vec());

        let mut b = MasterSystem::new(&test_rom(), false).unwrap();
        b.load_state(&snap).unwrap();
        assert_eq!(b.bus.ram[0], 3, "state restored mid-run");
        for _ in 0..4 {
            b.run_frame();
        }
        assert_eq!((b.bus.ram[0], b.frame().to_vec()), (counter, picture));
        // A state from a Game Gear can't be loaded into a Master System.
        let mut gg = MasterSystem::new(&test_rom(), true).unwrap();
        gg.run_frame();
        assert!(a.load_state(&gg.save_state()).is_err());
    }

    #[test]
    fn game_gear_window() {
        let mut m = MasterSystem::new(&test_rom(), true).unwrap();
        m.run_frame();
        m.run_frame();
        assert_eq!(m.size(), (160, 144));
        assert_eq!(m.frame().len(), 160 * 144);
    }

    #[test]
    fn sega_mapper_switches_banks() {
        let mut rom = vec![0u8; 0x10000];
        rom[0x3 * 0x4000] = 0xAB;
        let mut m = MasterSystem::new(&rom, false).unwrap();
        use cpuz80::Bus as _;
        m.bus.write(0xFFFF, 3);
        assert_eq!(m.bus.read(0x8000), 0xAB);
    }
}
