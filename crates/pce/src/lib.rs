//! NEC TurboGrafx-16 / PC Engine (HuCard games): our HuC6280 CPU, HuC6270 VDC, HuC6260 VCE
//! and the wavetable PSG. No BIOS is needed for HuCards.

pub mod cpu;
pub mod psg;
pub mod vdc;

use cpu::Cpu;
use psg::Psg;
use vdc::Vdc;

pub const FPS: f64 = 59.826;
/// CPU cycles per scanline at 7.16 MHz.
const CYCLES_PER_LINE: u32 = 455;

#[derive(state::Stateful)]
pub struct Bus {
    #[state(skip)]
    rom: Vec<u8>,
    ram: [u8; 0x2000],
    pub vdc: Vdc,
    pub psg: Psg,
    timer_reload: u8,
    timer_count: u8,
    timer_on: bool,
    timer_sub: u32,
    timer_irq: bool,
    irq_mask: u8,
    pad_sel: bool,
    pad: u8,
    /// Last value on the data bus (unmapped reads return it).
    open_bus: u8,
    /// Street Fighter II' mapper: 512 KB bank shown at $40-$7F.
    sf2_bank: u32,
    /// 2 KB battery backup RAM (TurboBooster-Plus / Tennokoe Bank) at $F7, unlocked by
    /// writing bit 7 of $1807.
    pub bram: Vec<u8>,
    bram_unlocked: bool,
    pub bram_used: bool,
}

impl Bus {
    fn rom_byte(&self, page: u32, off: u32) -> u8 {
        if self.rom.len() > 0x10_0000 && page >= 0x40 {
            // Street Fighter II': the first 512 KB is fixed, $40-$7F shows a switched bank.
            let at = 0x8_0000 * (1 + self.sf2_bank as usize) + ((page - 0x40) * 0x2000 + off) as usize;
            return self.rom[at % self.rom.len()];
        }
        let pages = (self.rom.len() / 0x2000).max(1) as u32;
        // 384 KB cards: the upper 128 KB appears at $40-$7F.
        let page = if pages == 0x30 && page >= 0x40 { 0x20 + (page & 0x0F) } else { page % pages };
        self.rom[(page * 0x2000 + off) as usize % self.rom.len()]
    }

    fn io_read(&mut self, off: u32) -> u8 {
        match off {
            0x0000..=0x03FF => self.vdc.read(off),
            0x0400..=0x07FF => self.vdc.vce_read(off),
            0x0C00..=0x0FFF => (self.timer_count & 0x7F) | (self.open_bus & 0x80),
            0x1000..=0x13FF => {
                // Low nibble: d-pad (SEL=1) or buttons (SEL=0), active low. $B0: US console.
                let n = if self.pad_sel { self.pad & 0x0F } else { self.pad >> 4 };
                0xB0 | (!n & 0x0F)
            }
            0x1400..=0x17FF => match off & 3 {
                2 => self.irq_mask | (self.open_bus & 0xF8),
                3 => (self.timer_irq as u8) << 2 | (self.vdc.irq as u8) << 1,
                _ => self.open_bus,
            },
            _ => self.open_bus,
        }
    }

    fn io_write(&mut self, off: u32, v: u8) {
        match off {
            0x0000..=0x03FF => self.vdc.write(off, v),
            0x0400..=0x07FF => self.vdc.vce_write(off, v),
            0x0800..=0x0BFF => self.psg.write(off, v),
            0x0C00..=0x0FFF => {
                if off & 1 == 0 {
                    self.timer_reload = v & 0x7F;
                } else {
                    let on = v & 1 != 0;
                    if on && !self.timer_on {
                        self.timer_count = self.timer_reload;
                        self.timer_sub = 0;
                    }
                    self.timer_on = on;
                }
            }
            0x1000..=0x13FF => self.pad_sel = v & 1 != 0,
            0x1400..=0x17FF => match off & 3 {
                2 => self.irq_mask = v & 7,
                3 => self.timer_irq = false,
                _ => {}
            },
            _ => {}
        }
    }

    fn tick_timer(&mut self, cycles: u32) {
        if !self.timer_on {
            return;
        }
        self.timer_sub += cycles;
        while self.timer_sub >= 1024 {
            self.timer_sub -= 1024;
            if self.timer_count == 0 {
                self.timer_count = self.timer_reload;
                self.timer_irq = true;
            } else {
                self.timer_count -= 1;
            }
        }
    }
}

impl cpu::Bus for Bus {
    fn read(&mut self, addr: u32) -> u8 {
        let (page, off) = (addr >> 13, addr & 0x1FFF);
        let v = match page {
            0x00..=0x7F => self.rom_byte(page, off),
            0xF8..=0xFB => self.ram[off as usize],
            0xF7 if off < 0x800 && self.bram_unlocked => self.bram[off as usize],
            0xFF if (0x1800..=0x1BFF).contains(&off) => {
                // BRAM status: bit 7 reports the lock; reading $1803 locks it again.
                if off & 0xF == 3 {
                    self.bram_unlocked = false;
                }
                if off & 0xF == 3 { 0x80 } else { 0 }
            }
            0xFF => self.io_read(off),
            _ => 0xFF,
        };
        self.open_bus = v;
        v
    }

    fn write(&mut self, addr: u32, v: u8) {
        let (page, off) = (addr >> 13, addr & 0x1FFF);
        match page {
            0x00 if (0x1FF0..=0x1FF3).contains(&off) && self.rom.len() > 0x10_0000 => self.sf2_bank = off & 3,
            0xF8..=0xFB => self.ram[off as usize] = v,
            0xF7 if off < 0x800 && self.bram_unlocked => {
                self.bram[off as usize] = v;
                self.bram_used = true;
            }
            0xFF if off & 0x1C0F == 0x1807 => self.bram_unlocked = v & 0x80 != 0,
            0xFF => self.io_write(off, v),
            _ => {}
        }
        self.open_bus = v;
    }

    fn vdc_store(&mut self, port: u32, v: u8) {
        self.vdc.write(port, v);
    }
}

/// A freshly formatted backup RAM ("HUBM" header, empty directory).
fn bram_formatted() -> Vec<u8> {
    let mut b = vec![0u8; 0x800];
    b[0..8].copy_from_slice(&[0x48, 0x55, 0x42, 0x4D, 0x00, 0x88, 0x10, 0x80]);
    b
}

pub struct PcEngine {
    cpu: Cpu,
    pub bus: Bus,
    picture: Vec<u32>,
}

impl state::Stateful for PcEngine {
    fn sync(&mut self, s: &mut state::State) {
        s.check(0x5043_0000 ^ self.bus.rom.len() as u32);
        self.cpu.sync(s);
        self.bus.sync(s);
    }
}

impl PcEngine {
    pub fn new(rom: &[u8]) -> Result<PcEngine, String> {
        let rom = if rom.len() % 0x2000 == 512 { &rom[512..] } else { rom };
        if rom.len() < 0x2000 || rom.len() > 0x28_0000 {
            return Err(format!("Not a TurboGrafx-16 HuCard ({} bytes).", rom.len()));
        }
        let mut bus = Bus {
            rom: rom.to_vec(),
            ram: [0; 0x2000],
            vdc: Vdc::new(),
            psg: Psg::new(),
            timer_reload: 0,
            timer_count: 0,
            timer_on: false,
            timer_sub: 0,
            timer_irq: false,
            irq_mask: 0,
            pad_sel: false,
            pad: 0,
            open_bus: 0,
            sf2_bank: 0,
            bram: bram_formatted(),
            bram_unlocked: false,
            bram_used: false,
        };
        let mut cpu = Cpu::new();
        cpu.reset(&mut bus);
        Ok(PcEngine { cpu, bus, picture: vec![0; 256 * 224] })
    }

    pub fn reset(&mut self) {
        self.cpu.reset(&mut self.bus);
    }

    pub fn run_frame(&mut self) {
        loop {
            let mut used = 0;
            while used < CYCLES_PER_LINE {
                self.cpu.irq = (self.bus.timer_irq as u8) << 2 | (self.bus.vdc.irq as u8) << 1;
                self.cpu.irq_mask = self.bus.irq_mask;
                let c = self.cpu.step(&mut self.bus);
                // At 1.79 MHz each cycle takes four master periods.
                let c = if self.cpu.fast { c } else { c * 4 };
                self.bus.tick_timer(c);
                self.bus.psg.tick(c);
                used += c;
            }
            if self.bus.vdc.end_line() {
                break;
            }
        }
        // Finish the frame's remaining lines so the next run starts at line 0.
        while self.bus.vdc.line != 0 {
            let mut used = 0;
            while used < CYCLES_PER_LINE {
                self.cpu.irq = (self.bus.timer_irq as u8) << 2 | (self.bus.vdc.irq as u8) << 1;
                self.cpu.irq_mask = self.bus.irq_mask;
                let c = self.cpu.step(&mut self.bus);
                let c = if self.cpu.fast { c } else { c * 4 };
                self.bus.tick_timer(c);
                self.bus.psg.tick(c);
                used += c;
            }
            self.bus.vdc.end_line();
        }
        self.picture = self.bus.vdc.picture();
    }

    pub fn frame(&self) -> &[u32] {
        &self.picture
    }

    pub fn size(&self) -> (usize, usize) {
        (self.bus.vdc.width, self.bus.vdc.height)
    }

    /// A = button I, B = button II, Select, Start = Run, plus the d-pad.
    pub fn set_buttons(&mut self, pad: usize, b: u16) {
        if pad != 0 {
            return;
        }
        let dpad = ((b >> 4) & 1) | ((b >> 7) & 1) << 1 | ((b >> 5) & 1) << 2 | ((b >> 6) & 1) << 3; // up right down left
        let buttons = (b & 1) | ((b >> 1) & 1) << 1 | ((b >> 2) & 1) << 2 | ((b >> 3) & 1) << 3; // I II select run
        self.bus.pad = dpad as u8 | (buttons as u8) << 4;
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.bus.psg.samples);
    }

    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    /// Backup RAM contents once a game has written to it.
    pub fn save_ram(&self) -> Option<&[u8]> {
        self.bus.bram_used.then_some(&self.bus.bram[..])
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        if data.len() == self.bus.bram.len() {
            self.bus.bram.copy_from_slice(data);
            self.bus.bram_used = true;
        }
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.bus.psg.set_sample_rate(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A HuCard that maps RAM and I/O, turns on the VDC background and vblank interrupt,
    /// sets colour 0 through the VCE, and counts vblank interrupts in RAM.
    fn test_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x2000];
        let prog: &[u8] = &[
            0x78, // SEI
            0xD4, // CSH (fast clock)
            0xA9, 0xFF, 0x53, 0x01, // TAM #$01: MPR0 = $FF (I/O)
            0xA9, 0xF8, 0x53, 0x02, // TAM #$02: MPR1 = $F8 (RAM)
            0xA2, 0xFF, 0x9A, // LDX #$FF ; TXS
            0x64, 0x00, // STZ $00 (counter)
            // VCE: colour 0 = green ($1C0).
            0xA9, 0x00, 0x8D, 0x02, 0x04, 0x8D, 0x03, 0x04,
            0xA9, 0xC0, 0x8D, 0x04, 0x04, 0xA9, 0x01, 0x8D, 0x05, 0x04,
            // VDC: CR = $88 (background + vblank IRQ), HDR = 31, VDW = 223.
            0x03, 0x05, 0x13, 0x88, 0x23, 0x00,
            0x03, 0x0B, 0x13, 0x1F, 0x23, 0x00,
            0x03, 0x0D, 0x13, 0xDF, 0x23, 0x00,
            0x9C, 0x02, 0x14, // STZ $1402 (unmask interrupts)
            0x58, // CLI
            0x80, 0xFE, // BRA *
        ];
        rom[..prog.len()].copy_from_slice(prog);
        // IRQ1 handler at $E100: LDA $0000 (VDC status, acknowledges) ; INC $00 ; RTI
        rom[0x100..0x107].copy_from_slice(&[0xAD, 0x00, 0x00, 0xE6, 0x00, 0x40, 0x00]);
        rom[0x1FF8..0x1FFA].copy_from_slice(&[0x00, 0xE1]); // IRQ1 vector
        rom[0x1FFE..0x2000].copy_from_slice(&[0x00, 0xE0]); // reset
        rom
    }

    #[test]
    fn boots_and_counts_vblanks() {
        let mut m = PcEngine::new(&test_rom()).unwrap();
        for _ in 0..5 {
            m.run_frame();
        }
        let n = m.bus.ram[0];
        assert!((4..=5).contains(&n), "vblank interrupts: {n}");
        assert_eq!(m.size(), (256, 224));
        assert_eq!(m.frame()[100 * 256 + 100], 0x00FF00, "background colour 0 = green");
    }

    #[test]
    fn save_state_resumes_identically() {
        let rom = test_rom();
        let mut a = PcEngine::new(&rom).unwrap();
        for _ in 0..3 {
            a.run_frame();
        }
        let at = a.bus.ram[0];
        let snap = a.save_state();
        for _ in 0..6 {
            a.run_frame();
        }
        let want = (a.bus.ram[0], a.frame().to_vec());
        assert!(want.0 > at);
        let mut b = PcEngine::new(&rom).unwrap();
        b.load_state(&snap).unwrap();
        assert_eq!(b.bus.ram[0], at);
        for _ in 0..6 {
            b.run_frame();
        }
        assert_eq!((b.bus.ram[0], b.frame().to_vec()), want);
    }

    #[test]
    fn joypad_nibbles() {
        let mut m = PcEngine::new(&test_rom()).unwrap();
        m.set_buttons(0, 1 | 0x10); // I + up
        m.bus.io_write(0x1000, 1);
        assert_eq!(m.bus.io_read(0x1000) & 0xF, 0b1110, "up pressed");
        m.bus.io_write(0x1000, 0);
        assert_eq!(m.bus.io_read(0x1000) & 0xF, 0b1110, "button I pressed");
    }
}
