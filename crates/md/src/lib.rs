//! Sega Genesis / Mega Drive: our 68000 (main CPU), Z80 (sound CPU), VDP, YM2612 FM and the
//! SN76489 PSG, with 3-button controllers and battery saves. No BIOS/TMSS is needed.

pub mod eeprom;
pub mod vdp;
pub mod ym;

use cpu68k::M68k;
use cpuz80::Z80;
use vdp::{DmaRequest, Vdp};
use ym::Ym2612;

pub const FPS: f64 = 59.92;
const M68K_PER_LINE: u32 = 488;
const Z80_PER_LINE: u32 = 228;

pub struct Hw {
    rom: Vec<u8>,
    ram: Vec<u8>,
    zram: [u8; 0x2000],
    pub vdp: Vdp,
    ym: Ym2612,
    psg: sms::psg::Psg,
    bank: u32,
    busreq: bool,
    z80_reset: bool,
    /// Pad buttons (bit per Genesis button, active high): U D L R B C A Start, then
    /// Z Y X Mode (bits 8-11) on the 6-button pad.
    pads: [u16; 2],
    /// 6-button pads: TH rising edges since the last timeout (selects the extra reads).
    th_count: [u8; 2],
    /// Serial EEPROM save chip, if the cartridge has one.
    pub eeprom: Option<eeprom::Eeprom>,
    /// Super Street Fighter II mapper: 512 KB bank shown at each of eight 512 KB slots.
    ssf2: Option<[u8; 8]>,
    io_data: [u8; 3],
    io_ctrl: [u8; 3],
    pub sram: Vec<u8>,
    pub sram_used: bool,
    sram_range: Option<(u32, u32)>,
    sram_on: bool,
    line_cycle: u32,
    dma: Option<DmaRequest>,
}

impl Hw {
    fn pad_read(&self, i: usize) -> u8 {
        let p = self.pads[i];
        let th = self.io_data[i] & 0x40 != 0;
        let n = self.th_count[i];
        let v = if th && n == 3 {
            // Third TH-high read of a 6-button pad: C B Mode X Y Z.
            let bits = (((p >> 8) & 0x0F) | ((p >> 4) & 1) << 4 | ((p >> 5) & 1) << 5) as u8;
            0x40 | (!bits & 0x3F)
        } else if !th && n == 3 {
            // Identification: D-pad lines all low.
            let bits = ((p >> 6) & 1) << 4 | ((p >> 7) & 1) << 5;
            (!bits as u8) & 0x30
        } else if !th && n >= 4 {
            let bits = ((p >> 6) & 1) << 4 | ((p >> 7) & 1) << 5;
            (!bits as u8) & 0x30 | 0x0F
        } else if th {
            // TH high: C B R L D U
            let bits = (p & 0x0F) | ((p >> 4) & 1) << 4 | ((p >> 5) & 1) << 5;
            0x40 | (!bits as u8 & 0x3F)
        } else {
            // TH low: Start A 0 0 D U
            let bits = (p & 0x03) | ((p >> 6) & 1) << 4 | ((p >> 7) & 1) << 5;
            !bits as u8 & 0x33
        };
        (v & !self.io_ctrl[i]) | (self.io_data[i] & self.io_ctrl[i] & 0x7F)
    }

    fn in_sram(&self, a: u32) -> bool {
        match self.sram_range {
            Some((s, e)) => self.sram_on && a >= s && a <= e,
            None => false,
        }
    }

    /// ROM byte through the Super Street Fighter II bank registers when present.
    fn rom_byte(&self, a: u32) -> u8 {
        let i = match &self.ssf2 {
            Some(b) => (b[(a >> 19) as usize & 7] as usize) << 19 | (a as usize & 0x7_FFFF),
            None => a as usize,
        };
        self.rom[i % self.rom.len()]
    }

    pub fn read8(&mut self, a: u32) -> u8 {
        let a = a & 0xFF_FFFF;
        if let Some(e) = &self.eeprom {
            if e.claims(a) {
                return e.read(a);
            }
        }
        match a {
            0x00_0000..=0x3F_FFFF => {
                if self.in_sram(a) {
                    let (s, _) = self.sram_range.unwrap();
                    return self.sram[((a - s) as usize) % self.sram.len()];
                }
                self.rom_byte(a)
            }
            0xA0_0000..=0xA0_FFFF => match a & 0x7FFF {
                0x0000..=0x3FFF => self.zram[(a & 0x1FFF) as usize],
                0x4000..=0x5FFF => self.ym.read_status(),
                _ => 0xFF,
            },
            0xA1_0000..=0xA1_001F => match a & 0x1F {
                0x01 => 0xA0, // overseas, NTSC, no expansion, no TMSS
                0x03 => self.pad_read(0),
                0x05 => self.pad_read(1),
                0x07 => 0x7F,
                0x09 => self.io_ctrl[0],
                0x0B => self.io_ctrl[1],
                0x0D => self.io_ctrl[2],
                _ => 0,
            },
            0xA1_1100 | 0xA1_1101 => {
                let granted = self.busreq && !self.z80_reset;
                if a & 1 == 0 {
                    if granted { 0 } else { 1 }
                } else {
                    0
                }
            }
            0xC0_0000..=0xC0_001F => {
                let w = self.read16(a & !1);
                if a & 1 == 0 {
                    (w >> 8) as u8
                } else {
                    w as u8
                }
            }
            0xE0_0000..=0xFF_FFFF => self.ram[(a & 0xFFFF) as usize],
            _ => 0xFF,
        }
    }

    pub fn read16(&mut self, a: u32) -> u16 {
        let a = a & 0xFF_FFFE;
        match a {
            0xC0_0000..=0xC0_001F => match a & 0x1F {
                0x00 | 0x02 => self.vdp.read_data(),
                0x04 | 0x06 => self.vdp.read_status(self.line_cycle),
                0x08..=0x0E => self.vdp.hv_counter(self.line_cycle),
                _ => 0xFFFF,
            },
            0x00_0000..=0x3F_FFFF if !self.in_sram(a) && !self.eeprom.as_ref().is_some_and(|e| e.claims(a) || e.claims(a + 1)) => {
                if self.ssf2.is_some() {
                    return (self.rom_byte(a) as u16) << 8 | self.rom_byte(a + 1) as u16;
                }
                let i = (a as usize) % self.rom.len();
                u16::from_be_bytes([self.rom[i], self.rom[(i + 1) % self.rom.len()]])
            }
            0xE0_0000..=0xFF_FFFF => {
                let i = (a & 0xFFFF) as usize;
                u16::from_be_bytes([self.ram[i], self.ram[i + 1]])
            }
            _ => (self.read8(a) as u16) << 8 | self.read8(a + 1) as u16,
        }
    }

    pub fn write8(&mut self, a: u32, v: u8) {
        let a = a & 0xFF_FFFF;
        if let Some(e) = &mut self.eeprom {
            if e.claims(a) {
                e.write(a, v);
                return;
            }
        }
        match a {
            0x00_0000..=0x3F_FFFF => {
                if self.in_sram(a) {
                    let (s, _) = self.sram_range.unwrap();
                    let n = self.sram.len();
                    self.sram[((a - s) as usize) % n] = v;
                    self.sram_used = true;
                }
            }
            0xA0_0000..=0xA0_FFFF => match a & 0x7FFF {
                0x0000..=0x3FFF => self.zram[(a & 0x1FFF) as usize] = v,
                0x4000..=0x5FFF => self.ym.write((a & 3) as u8, v),
                _ => {}
            },
            0xA1_0000..=0xA1_001F => match a & 0x1F {
                0x03 | 0x05 => {
                    let i = ((a & 0x1F) as usize - 3) / 2;
                    let old_th = self.io_data[i] & 0x40 != 0;
                    self.io_data[i] = v;
                    // Counting TH rising edges selects the 6-button pad's extra reads.
                    if !old_th && v & 0x40 != 0 && self.io_ctrl[i] & 0x40 != 0 {
                        self.th_count[i] = (self.th_count[i] + 1).min(4);
                    }
                }
                0x07 => self.io_data[2] = v,
                0x09 => self.io_ctrl[0] = v,
                0x0B => self.io_ctrl[1] = v,
                0x0D => self.io_ctrl[2] = v,
                _ => {}
            },
            0xA1_1100 => self.busreq = v & 1 != 0,
            0xA1_1200 => {
                let reset = v & 1 == 0;
                self.z80_reset = reset;
            }
            0xA1_30F1 => self.sram_on = v & 1 != 0,
            0xA1_30F3..=0xA1_30FF if a & 1 == 1 => {
                if let Some(b) = &mut self.ssf2 {
                    b[((a - 0xA1_30F1) / 2) as usize] = v & 0x3F;
                }
            }
            0xC0_0011 | 0xC0_0013 | 0xC0_0015 | 0xC0_0017 => self.psg.write(v),
            0xC0_0000..=0xC0_001F => self.write16(a & !1, (v as u16) << 8 | v as u16),
            0xE0_0000..=0xFF_FFFF => self.ram[(a & 0xFFFF) as usize] = v,
            _ => {}
        }
    }

    pub fn write16(&mut self, a: u32, v: u16) {
        let a = a & 0xFF_FFFE;
        match a {
            0xC0_0000..=0xC0_0003 => self.vdp.write_data(v),
            0xC0_0004..=0xC0_0007 => {
                if let Some(req) = self.vdp.write_control(v) {
                    self.dma = Some(req);
                }
            }
            0xC0_0010..=0xC0_0017 => self.psg.write(v as u8),
            0xE0_0000..=0xFF_FFFF => {
                let i = (a & 0xFFFF) as usize;
                self.ram[i..i + 2].copy_from_slice(&v.to_be_bytes());
            }
            0xA1_1100 => self.busreq = v & 0x100 != 0,
            0xA1_1200 => self.z80_reset = v & 0x100 == 0,
            _ => {
                self.write8(a, (v >> 8) as u8);
                self.write8(a + 1, v as u8);
            }
        }
    }

    /// 68000 memory to VDP: runs a pending DMA transfer immediately.
    fn run_dma(&mut self) {
        if let Some(DmaRequest::FromBus { mut src, words }) = self.dma.take() {
            for _ in 0..words {
                let w = self.read16(src);
                self.vdp.dma_word(w);
                src = (src & 0xFE_0000) | (src.wrapping_add(2) & 0x1_FFFF);
            }
        }
    }
}

/// The 68000's view of the hardware.
struct MainBus<'a>(&'a mut Hw);

impl cpu68k::Bus for MainBus<'_> {
    fn read8(&mut self, a: u32) -> u8 {
        self.0.read8(a)
    }
    fn read16(&mut self, a: u32) -> u16 {
        self.0.read16(a)
    }
    fn write8(&mut self, a: u32, v: u8) {
        self.0.write8(a, v);
        self.0.run_dma();
    }
    fn write16(&mut self, a: u32, v: u16) {
        self.0.write16(a, v);
        self.0.run_dma();
    }
}

/// The Z80's view: its RAM, the YM2612, the bank register, the PSG and the 68000 window.
struct SoundBus<'a>(&'a mut Hw);

impl cpuz80::Bus for SoundBus<'_> {
    fn read(&mut self, a: u16) -> u8 {
        match a {
            0x0000..=0x3FFF => self.0.zram[(a & 0x1FFF) as usize],
            0x4000..=0x5FFF => self.0.ym.read_status(),
            0x8000..=0xFFFF => {
                let addr = (self.0.bank << 15) | (a as u32 & 0x7FFF);
                self.0.read8(addr)
            }
            _ => 0xFF,
        }
    }
    fn write(&mut self, a: u16, v: u8) {
        match a {
            0x0000..=0x3FFF => self.0.zram[(a & 0x1FFF) as usize] = v,
            0x4000..=0x5FFF => self.0.ym.write((a & 3) as u8, v),
            0x6000..=0x60FF => self.0.bank = ((self.0.bank >> 1) | ((v as u32 & 1) << 8)) & 0x1FF,
            0x7F11 => self.0.psg.write(v),
            0x8000..=0xFFFF => {
                let addr = (self.0.bank << 15) | (a as u32 & 0x7FFF);
                self.0.write8(addr, v);
            }
            _ => {}
        }
    }
    fn input(&mut self, _port: u16) -> u8 {
        0xFF
    }
    fn output(&mut self, _port: u16, _v: u8) {}
}

pub struct Genesis {
    m68k: M68k,
    z80: Z80,
    pub hw: Hw,
    ym_acc: u32,
    picture: Vec<u32>,
}

impl state::Stateful for Hw {
    fn sync(&mut self, s: &mut state::State) {
        // The cartridge (ROM, header-declared save areas, EEPROM chip) is fixed by the game;
        // everything else is state.
        self.ram.sync(s);
        self.zram.sync(s);
        self.vdp.sync(s);
        self.ym.sync(s);
        self.psg.sync(s);
        self.bank.sync(s);
        self.busreq.sync(s);
        self.z80_reset.sync(s);
        self.th_count.sync(s);
        if let Some(e) = &mut self.eeprom {
            e.sync(s);
        }
        if let Some(b) = &mut self.ssf2 {
            b.sync(s);
        }
        self.io_data.sync(s);
        self.io_ctrl.sync(s);
        self.sram.sync(s);
        self.sram_used.sync(s);
        self.sram_on.sync(s);
        self.line_cycle.sync(s);
        self.dma.sync(s);
    }
}

impl state::Stateful for Genesis {
    fn sync(&mut self, s: &mut state::State) {
        s.check(0x4D44_0000 ^ self.hw.rom.len() as u32);
        self.m68k.sync(s);
        self.z80.sync(s);
        self.hw.sync(s);
        self.ym_acc.sync(s);
    }
}

/// De-interleaves Super Magic Drive (.smd) dumps: 512-byte header, then 16 KB blocks storing
/// odd bytes in the first half and even bytes in the second.
fn unsmd(rom: &[u8]) -> Vec<u8> {
    let body = &rom[512..];
    let mut out = vec![0u8; body.len()];
    for (b, block) in body.chunks(16384).enumerate() {
        let half = block.len() / 2;
        for i in 0..half {
            out[b * 16384 + i * 2] = block[half + i];
            out[b * 16384 + i * 2 + 1] = block[i];
        }
    }
    out
}

impl Genesis {
    pub fn new(rom: &[u8]) -> Result<Genesis, String> {
        let rom = if rom.len() % 16384 == 512 && rom.len() > 520 && rom[8] == 0xAA && rom[9] == 0xBB { unsmd(rom) } else { rom.to_vec() };
        if rom.len() < 0x200 || rom.len() > 0x50_0000 {
            return Err(format!("Not a Genesis / Mega Drive cartridge ({} bytes).", rom.len()));
        }
        // Battery RAM declared in the header ("RA" at $1B0).
        let sram_range = if &rom[0x1B0..0x1B2] == b"RA" {
            let s = u32::from_be_bytes([rom[0x1B4], rom[0x1B5], rom[0x1B6], rom[0x1B7]]);
            let e = u32::from_be_bytes([rom[0x1B8], rom[0x1B9], rom[0x1BA], rom[0x1BB]]);
            (e > s && s >= 0x20_0000 && e < 0x40_0000).then_some((s & !1, e | 1))
        } else {
            None
        };
        let sram_on = sram_range.is_some_and(|(s, _)| rom.len() as u32 <= s);
        let hw = Hw {
            rom,
            ram: vec![0; 0x1_0000],
            zram: [0; 0x2000],
            vdp: Vdp::new(),
            ym: Ym2612::new(),
            psg: sms::psg::Psg::new(3_579_545.0),
            bank: 0,
            busreq: false,
            z80_reset: true,
            pads: [0; 2],
            io_data: [0x40; 3],
            io_ctrl: [0; 3],
            sram: vec![0; 0x1_0000],
            sram_used: false,
            th_count: [0; 2],
            eeprom: None,
            ssf2: None,
            sram_range,
            sram_on,
            line_cycle: 0,
            dma: None,
        };
        let mut hw = hw;
        hw.eeprom = eeprom::Eeprom::for_rom(&hw.rom);
        // Carts over 4 MB (Super Street Fighter II) use Sega's bank-switching mapper.
        if hw.rom.len() > 0x40_0000 {
            hw.ssf2 = Some([0, 1, 2, 3, 4, 5, 6, 7]);
        }
        let mut g = Genesis { m68k: M68k::new(), z80: Z80::new(), hw, ym_acc: 0, picture: vec![0; 320 * 224] };
        g.reset();
        Ok(g)
    }

    pub fn reset(&mut self) {
        self.m68k.reset(&mut MainBus(&mut self.hw));
        self.z80.reset();
    }

    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    pub fn run_frame(&mut self) {
        // The 6-button pad resets its TH counter after ~1.5 ms without toggles.
        self.hw.th_count = [0; 2];
        for _ in 0..vdp::LINES {
            let line = self.hw.vdp.line;
            // Main CPU.
            let mut used = 0;
            while used < M68K_PER_LINE {
                let level = self.hw.vdp.irq_level();
                let mask = ((self.m68k.sr >> 8) & 7) as u8;
                self.m68k.irq_level = level;
                self.hw.line_cycle = used;
                let c = self.m68k.step(&mut MainBus(&mut self.hw));
                if level > mask {
                    self.hw.vdp.ack(level);
                }
                used += c;
                self.ym_acc += c;
                while self.ym_acc >= 144 {
                    self.ym_acc -= 144;
                    self.hw.ym.clock_sample();
                }
            }
            // Sound CPU (runs unless the 68000 holds its bus or reset).
            self.z80.irq_line = line == vdp::HEIGHT as u32;
            let mut zused = 0;
            while zused < Z80_PER_LINE {
                let c = if self.hw.busreq || self.hw.z80_reset { Z80_PER_LINE } else { self.z80.step(&mut SoundBus(&mut self.hw)) };
                self.hw.psg.tick(c);
                zused += c;
            }
            if self.hw.z80_reset {
                self.z80.reset();
            }
            self.hw.vdp.end_line();
        }
        self.picture = self.hw.vdp.picture();
    }

    pub fn frame(&self) -> &[u32] {
        &self.picture
    }

    pub fn size(&self) -> (usize, usize) {
        (self.hw.vdp.width, vdp::HEIGHT)
    }

    /// B = Genesis A, A = Genesis B, Y = Genesis C, Start; d-pad.
    pub fn set_buttons(&mut self, pad: usize, b: u16) {
        if pad > 1 {
            return;
        }
        let bit = |i: u16| (b >> i) & 1;
        // Internal layout: U D L R B C A Start (bits 0-7), then Z Y X Mode. The bottom row
        // A B C is the app's B, A, Y; the top row X Y Z is L, X, R; Mode is Select.
        self.hw.pads[pad] = bit(4) | bit(5) << 1 | bit(6) << 2 | bit(7) << 3 | bit(0) << 4 | bit(9) << 5 | bit(1) << 6 | bit(3) << 7 | bit(11) << 8 | bit(8) << 9 | bit(10) << 10 | bit(2) << 11;
    }

    /// FM and PSG mixed to stereo.
    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        let ym = &mut self.hw.ym.samples;
        let psg = &mut self.hw.psg.samples;
        let n = ym.len().min(psg.len());
        for i in 0..n {
            out.push((ym[i] * 1.6 + psg[i] * 0.35).clamp(-1.0, 1.0));
        }
        ym.drain(..n);
        psg.drain(..n);
        // Keep the two streams from drifting apart.
        if ym.len() > 4096 {
            ym.clear();
        }
        if psg.len() > 4096 {
            psg.clear();
        }
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.hw.ym.set_sample_rate(r);
        self.hw.psg.set_sample_rate(r);
    }

    pub fn save_ram(&self) -> Option<&[u8]> {
        if let Some(e) = &self.hw.eeprom {
            return e.dirty.then_some(&e.data[..]);
        }
        self.hw.sram_used.then_some(&self.hw.sram[..])
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        if let Some(e) = &mut self.hw.eeprom {
            if data.len() == e.data.len() {
                e.data.copy_from_slice(data);
                e.dirty = true;
            }
            return;
        }
        if data.len() == self.hw.sram.len() {
            self.hw.sram.copy_from_slice(data);
            self.hw.sram_used = true;
        }
    }
}

/// Whether a ROM image looks like a Genesis cartridge ("SEGA" at $100, or an .smd dump).
pub fn is_genesis_rom(data: &[u8]) -> bool {
    (data.len() > 0x110 && &data[0x100..0x104] == b"SEGA") || (data.len() > 0x310 && data.len() % 16384 == 512 && data[8] == 0xAA && data[9] == 0xBB)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cartridge whose code turns on the display with a coloured backdrop, enables the
    /// vertical interrupt, and counts interrupts in RAM.
    fn test_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x1_0000];
        let put32 = |r: &mut Vec<u8>, a: usize, v: u32| r[a..a + 4].copy_from_slice(&v.to_be_bytes());
        put32(&mut rom, 0, 0x00FF_FE00); // SSP
        put32(&mut rom, 4, 0x0000_0200); // PC
        put32(&mut rom, 30 * 4, 0x0000_0300); // level 6 autovector
        rom[0x100..0x104].copy_from_slice(b"SEGA");
        let code: &[u16] = &[
            0x33FC, 0x8144, 0x00C0, 0x0004, // MOVE.W #$8144,$C00004 (display on, mode 5)
            0x33FC, 0x8C81, 0x00C0, 0x0004, // H40
            0x33FC, 0x8701, 0x00C0, 0x0004, // backdrop = colour 1
            0x23FC, 0xC002, 0x0000, 0x00C0, 0x0004, // CRAM write address 2
            0x33FC, 0x0E00, 0x00C0, 0x0000, // colour 1 = blue
            0x33FC, 0x8164, 0x00C0, 0x0004, // + vertical interrupt enable
            0x46FC, 0x2000, // MOVE #$2000,SR
            0x60FE, // BRA *
        ];
        for (i, w) in code.iter().enumerate() {
            rom[0x200 + i * 2..0x202 + i * 2].copy_from_slice(&w.to_be_bytes());
        }
        // Handler: ADDQ.B #1,$FF0000 ; RTE
        let h: &[u16] = &[0x5239, 0x00FF, 0x0000, 0x4E73];
        for (i, w) in h.iter().enumerate() {
            rom[0x300 + i * 2..0x302 + i * 2].copy_from_slice(&w.to_be_bytes());
        }
        rom
    }

    #[test]
    fn boots_draws_backdrop_and_counts_vblanks() {
        let rom = test_rom();
        assert!(is_genesis_rom(&rom));
        let mut g = Genesis::new(&rom).unwrap();
        for _ in 0..5 {
            g.run_frame();
        }
        let n = g.hw.ram[0];
        assert!((4..=5).contains(&n), "vertical interrupts: {n}");
        assert_eq!(g.size(), (320, 224));
        assert_eq!(g.frame()[100 * 320 + 100], 0x0000FC, "blue backdrop (7 x 36)");
    }

    /// Restoring a snapshot continues the game exactly as if it had never stopped, including
    /// the FM chip's phase and the picture.
    #[test]
    fn save_state_resumes_identically() {
        let rom = test_rom();
        let mut a = Genesis::new(&rom).unwrap();
        // Key on an FM voice so the sound chip has live state to restore.
        for (port, v) in [(0xA0_4000u32, 0x28u8), (0xA0_4001, 0xF0)] {
            a.hw.write8(port, v);
        }
        for _ in 0..4 {
            a.run_frame();
        }
        let at = a.hw.ram[0];
        let snap = a.save_state();
        let mut audio_a = Vec::new();
        a.take_audio(&mut audio_a);
        audio_a.clear();
        for _ in 0..6 {
            a.run_frame();
        }
        a.take_audio(&mut audio_a);
        let want = (a.hw.ram[0], a.frame().to_vec());
        assert!(want.0 > at);

        let mut b = Genesis::new(&rom).unwrap();
        b.load_state(&snap).unwrap();
        assert_eq!(b.hw.ram[0], at);
        for _ in 0..6 {
            b.run_frame();
        }
        let mut audio_b = Vec::new();
        b.take_audio(&mut audio_b);
        assert_eq!((b.hw.ram[0], b.frame().to_vec()), want);
        assert_eq!(audio_a, audio_b, "sound continues sample for sample");
    }

    #[test]
    fn three_button_pad() {
        let mut g = Genesis::new(&test_rom()).unwrap();
        g.set_buttons(0, 0x10 | 0x08); // up + start
        g.hw.write8(0xA1_0009, 0x40);
        g.hw.write8(0xA1_0003, 0x40);
        assert_eq!(g.hw.read8(0xA1_0003) & 0x3F, 0x3E, "TH high: up pressed");
        g.hw.write8(0xA1_0003, 0x00);
        assert_eq!(g.hw.read8(0xA1_0003) & 0x33, 0x12, "TH low: up + start pressed");
    }

    #[test]
    fn smd_deinterleave() {
        let mut smd = vec![0u8; 512 + 16384];
        smd[8] = 0xAA;
        smd[9] = 0xBB;
        smd[512] = 0x11; // odd byte 1
        smd[512 + 8192] = 0x22; // even byte 0
        let out = unsmd(&smd);
        assert_eq!(&out[..2], &[0x22, 0x11]);
    }
}
