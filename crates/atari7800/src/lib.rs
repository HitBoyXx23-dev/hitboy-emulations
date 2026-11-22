//! Atari 7800 ProSystem: our 6502 (SALLY), the MARIA graphics chip, and the 2600's TIA
//! (sound, fire buttons) and RIOT (joysticks, console switches, timer).
//!
//! MARIA draws from a display-list list (DLL): the screen is split into zones, each with a
//! display list of objects (graphics address, palette, width, horizontal position).
//! No BIOS is needed: games start at their reset vector, as the BIOS would leave them.

use atari2600::riot::Riot;
use atari2600::tia::Tia;
use cpu6502::Cpu;

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 240;
pub const FPS: f64 = 59.92;
const LINES: u32 = 263;
/// First line of MARIA DMA, and the first line shown.
const DMA_START: u32 = 16;
const DMA_END: u32 = 259;
const CYCLES_PER_LINE: u32 = 114;

fn palette() -> [u32; 256] {
    const HUE: [f32; 16] = [0.0, 50.0, 35.0, 20.0, 355.0, 320.0, 285.0, 255.0, 232.0, 212.0, 192.0, 168.0, 135.0, 100.0, 72.0, 45.0];
    let mut p = [0u32; 256];
    for (i, c) in p.iter_mut().enumerate() {
        let (h, l) = (i >> 4, i & 15);
        let v = if h == 0 { l as f32 / 15.0 } else { 0.3 + l as f32 / 15.0 * 0.7 };
        let s = if h == 0 { 0.0 } else { 0.75 - l as f32 * 0.03 };
        let hh = HUE[h] / 60.0;
        let x = 1.0 - ((hh % 2.0) - 1.0).abs();
        let (r, g, b) = match hh as u32 {
            0 => (1.0, x, 0.0),
            1 => (x, 1.0, 0.0),
            2 => (0.0, 1.0, x),
            3 => (0.0, x, 1.0),
            4 => (x, 0.0, 1.0),
            _ => (1.0, 0.0, x),
        };
        let mix = |c: f32| ((v * (1.0 - s + s * c)).clamp(0.0, 1.0) * 255.0) as u32;
        *c = mix(r) << 16 | mix(g) << 8 | mix(b);
    }
    p
}

/// Cartridge boards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, state::Stateful)]
pub enum Kind {
    /// Up to 48 KB, mapped to the top of memory.
    Flat,
    /// SuperGame: 16 KB banks at $8000, the last bank at $C000, the one before it at $4000.
    SuperGame,
    /// SuperGame whose $4000 window is bank 6 (the "large" 144 KB variant) or plain ROM.
    SuperGameBank6,
    /// Absolute (F-18 Hornet): two 16 KB banks at $4000, last 32 KB fixed.
    Absolute,
    /// Activision (Double Dragon, Rampage): 16 KB banks at $A000 chosen at $FF80-$FF87.
    Activision,
}

#[derive(state::Stateful)]
pub struct Bus {
    pub tia: Tia,
    pub riot: Riot,
    ram: [u8; 0x1000],
    maria: [u8; 0x20],
    #[state(skip)]
    rom: Vec<u8>,
    /// Board type from the .a78 header (or guessed from the size).
    #[state(skip)]
    kind: Kind,
    bank: usize,
    /// SuperGame boards with 16 KB of RAM at $4000.
    ext_ram: Vec<u8>,
    /// POKEY sound chip at $4000 (its RANDOM register is emulated).
    #[state(skip)]
    pokey: bool,
    lfsr: u32,
    wsync: bool,
    /// Two-button joystick buttons for INPT0/INPT1 (player 1 right/left).
    #[state(skip)]
    pub buttons: [bool; 2],
}

impl Bus {
    fn rom_read(&mut self, a: usize) -> u8 {
        let banks = (self.rom.len() / 0x4000).max(1);
        let k16 = |b: usize, a: usize| (b % banks) * 0x4000 + (a & 0x3FFF);
        if self.pokey && (0x4000..0x4010).contains(&a) {
            if a & 0xF == 0xA {
                // RANDOM: 17-bit polynomial counter.
                self.lfsr = (self.lfsr >> 1) | (((self.lfsr ^ (self.lfsr >> 5)) & 1) << 16);
                return self.lfsr as u8;
            }
            return 0;
        }
        if !self.ext_ram.is_empty() && (0x4000..0x8000).contains(&a) {
            return self.ext_ram[a - 0x4000];
        }
        match self.kind {
            Kind::SuperGame | Kind::SuperGameBank6 => {
                let b = match a {
                    0x4000..=0x7FFF if self.kind == Kind::SuperGameBank6 => 6,
                    0x4000..=0x7FFF => banks.saturating_sub(2),
                    0x8000..=0xBFFF => self.bank,
                    _ => banks - 1,
                };
                return self.rom[k16(b, a)];
            }
            Kind::Absolute => {
                let b = match a {
                    0x4000..=0x7FFF => self.bank,
                    0x8000..=0xBFFF => 2,
                    _ => 3,
                };
                return self.rom[k16(b, a)];
            }
            Kind::Activision => {
                // $4000 first half of bank 6, $6000/$8000 bank 7, $A000-$DFFF banked,
                // $E000 second half of bank 6.
                let off = match a {
                    0x4000..=0x5FFF => 6 * 0x4000 + (a - 0x4000),
                    0x6000..=0x9FFF => 7 * 0x4000 + (a - 0x6000),
                    0xA000..=0xDFFF => (self.bank % banks) * 0x4000 + (a - 0xA000),
                    _ => 6 * 0x4000 + 0x2000 + (a - 0xE000),
                };
                return self.rom[off % self.rom.len()];
            }
            Kind::Flat => {}
        }
        let start = 0x1_0000 - self.rom.len().min(0xC000);
        if a >= start {
            self.rom[a - start + self.rom.len().saturating_sub(0xC000)]
        } else {
            0xFF
        }
    }

    /// RAM: $1800-$27FF, with $40-$FF and $140-$1FF mirroring $2040-$20FF / $2140-$21FF.
    fn ram_index(a: usize) -> Option<usize> {
        match a {
            0x0040..=0x00FF => Some(a + 0x2000 - 0x1800),
            0x0140..=0x01FF => Some(a + 0x2000 - 0x1800),
            0x1800..=0x27FF => Some(a - 0x1800),
            _ => None,
        }
    }

    fn peek(&mut self, a: u16) -> u8 {
        self.peek_inner(a)
    }

    fn peek_inner(&mut self, a: u16) -> u8 {
        let a = a as usize;
        if let Some(i) = Self::ram_index(a) {
            return self.ram[i];
        }
        match a {
            0x0000..=0x001F => match a & 0xF {
                0x8 => (self.buttons[0] as u8) << 7,
                0x9 => (self.buttons[1] as u8) << 7,
                _ => self.tia.read(a as u16),
            },
            0x0020..=0x003F => self.maria[a - 0x20],
            0x0280..=0x02FF => self.riot.read_io(a as u16),
            0x4000..=0xFFFF => self.rom_read(a),
            _ => 0,
        }
    }
}

impl cpu6502::Bus for Bus {
    fn read(&mut self, a: u16) -> u8 {
        self.peek(a)
    }

    fn write(&mut self, a: u16, v: u8) {
        let a = a as usize;
        if let Some(i) = Self::ram_index(a) {
            self.ram[i] = v;
            return;
        }
        match a {
            // $01 is INPTCTRL (locks 7800 mode) and sits on TIA VBLANK: games only use the
            // TIA here for sound.
            0x0000..=0x001F => {
                if a >= 0x15 {
                    self.tia.write(a as u16, v)
                }
            }
            0x0020..=0x003F => {
                if a == 0x24 {
                    self.wsync = true;
                }
                self.maria[a - 0x20] = v;
            }
            0x0280..=0x02FF => self.riot.write_io(a as u16, v),
            0x4000..=0x7FFF if !self.ext_ram.is_empty() => self.ext_ram[a - 0x4000] = v,
            0x8000..=0xBFFF if matches!(self.kind, Kind::SuperGame | Kind::SuperGameBank6) => self.bank = v as usize,
            0x8000..=0xBFFF if self.kind == Kind::Absolute => self.bank = if v & 2 != 0 { 1 } else { 0 },
            0xFF80..=0xFF87 if self.kind == Kind::Activision => self.bank = a & 7,
            _ => {}
        }
    }
}

pub struct Atari7800 {
    cpu: Cpu<Bus>,
    line: u32,
    dll: u16,
    zone_offset: u8,
    zone_flags: u8,
    dl: u16,
    palette: [u32; 256],
    pub frame: Vec<u32>,
}

impl state::Stateful for Atari7800 {
    fn sync(&mut self, s: &mut state::State) {
        s.check(0x4137_0000 ^ self.cpu.bus().rom.len() as u32);
        self.cpu.sync_state(s);
        self.cpu.bus_mut().sync(s);
        self.line.sync(s);
        self.dll.sync(s);
        self.zone_offset.sync(s);
        self.zone_flags.sync(s);
        self.dl.sync(s);
    }
}

impl Atari7800 {
    pub fn new(rom: &[u8]) -> Result<Atari7800, String> {
        // .a78 header cart type: bit 0 POKEY, 1 SuperGame banking, 2 RAM at $4000,
        // 3 ROM at $4000, 4 bank 6 at $4000, 8 Absolute, 9 Activision.
        let (body, ty) = if rom.len() > 128 && &rom[1..10] == b"ATARI7800" {
            (&rom[128..], u16::from_be_bytes([rom[53], rom[54]]))
        } else {
            (rom, if rom.len() > 0xC000 { 0x02 } else { 0 })
        };
        if body.len() < 0x1000 || body.len() > 0x8_0000 {
            return Err(format!("Not an Atari 7800 cartridge ({} bytes).", body.len()));
        }
        let kind = if ty & 0x0100 != 0 {
            Kind::Absolute
        } else if ty & 0x0200 != 0 {
            Kind::Activision
        } else if ty & 0x02 != 0 && ty & 0x10 != 0 {
            Kind::SuperGameBank6
        } else if ty & 0x02 != 0 {
            Kind::SuperGame
        } else {
            Kind::Flat
        };
        if kind != Kind::Flat && body.len() % 0x4000 != 0 {
            return Err("This banked 7800 cartridge has an unexpected size.".into());
        }
        let ext_ram = if ty & 0x04 != 0 { vec![0; 0x4000] } else { Vec::new() };
        let pokey = ty & 0x01 != 0;
        let mut riot = Riot::new();
        riot.swchb = 0xFF;
        let bus = Bus { tia: Tia::new(), riot, ram: [0; 0x1000], maria: [0; 0x20], rom: body.to_vec(), kind, bank: 0, ext_ram, pokey, lfsr: 0x1FFFF, wsync: false, buttons: [false; 2] };
        let mut cpu = Cpu::new(bus);
        cpu.set_decimal(true);
        cpu.reset();
        Ok(Atari7800 { cpu, line: 0, dll: 0, zone_offset: 0, zone_flags: 0, dl: 0, palette: palette(), frame: vec![0; WIDTH * HEIGHT] })
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    fn maria(&self, r: usize) -> u8 {
        self.cpu.bus().maria[r - 0x20]
    }

    /// Reads the next zone's DLL entry.
    fn next_zone(&mut self) {
        let bus = self.cpu.bus_mut();
        let a = self.dll;
        let b0 = bus.peek(a);
        let hi = bus.peek(a.wrapping_add(1));
        let lo = bus.peek(a.wrapping_add(2));
        self.dll = a.wrapping_add(3);
        self.zone_flags = b0;
        self.zone_offset = b0 & 0x0F;
        self.dl = (hi as u16) << 8 | lo as u16;
    }

    /// Draws the current zone's objects for this line into a 320-wide buffer.
    fn render_line(&mut self, out: &mut [u32; 320]) {
        let bg = self.palette[self.maria(0x20) as usize];
        out.fill(bg);
        let ctrl = self.maria(0x3C);
        if ctrl & 0x60 != 0x40 {
            return; // DMA off
        }
        let mut color = [[0u32; 4]; 8];
        for (p, c) in color.iter_mut().enumerate() {
            for i in 1..4 {
                c[i] = self.palette[self.maria(0x21 + p * 4 + i - 1) as usize];
            }
        }
        let read_mode = ctrl & 3;
        let char_wide = ctrl & 0x10 != 0;
        let kangaroo = ctrl & 0x04 != 0;
        let charbase = self.maria(0x34) as u16;
        let offset = self.zone_offset as u16;
        let holey16 = self.zone_flags & 0x40 != 0;
        let holey8 = self.zone_flags & 0x20 != 0;
        let mut p = self.dl;
        for _ in 0..128 {
            let bus = self.cpu.bus_mut();
            let (b0, b1) = (bus.peek(p), bus.peek(p.wrapping_add(1)));
            if b1 & 0x5F == 0 {
                break;
            }
            let (lo, hi, pal, width, hpos, wm, ind);
            if b1 & 0x5F == 0x40 {
                // Extended 5-byte header.
                let pw = bus.peek(p.wrapping_add(3));
                lo = b0;
                hi = bus.peek(p.wrapping_add(2));
                pal = (pw >> 5) as usize;
                width = ((pw & 0x1F) ^ 0x1F) as u16 + 1;
                hpos = bus.peek(p.wrapping_add(4));
                wm = b1 & 0x80 != 0;
                ind = b1 & 0x20 != 0;
                p = p.wrapping_add(5);
            } else {
                lo = b0;
                hi = bus.peek(p.wrapping_add(2));
                pal = (b1 >> 5) as usize;
                width = ((b1 & 0x1F) ^ 0x1F) as u16 + 1;
                hpos = bus.peek(p.wrapping_add(3));
                wm = false;
                ind = false;
                p = p.wrapping_add(4);
            }
            let base = ((hi as u16).wrapping_add(offset)) << 8 | lo as u16;
            // Holey DMA: graphics in odd 4 KB / 2 KB blocks read as zero.
            let holey = |a: u16| (holey16 && a & 0x9000 == 0x9000) || (holey8 && a & 0x8800 == 0x8800);
            let mut bytes = Vec::with_capacity(width as usize * 2);
            for i in 0..width {
                if ind {
                    let ch = bus.peek(((hi as u16) << 8 | lo as u16).wrapping_add(i));
                    let caddr = (charbase.wrapping_add(offset)) << 8 | ch as u16;
                    for k in 0..if char_wide { 2 } else { 1 } {
                        let a = caddr.wrapping_add(k);
                        bytes.push(if holey(a) { 0 } else { bus.peek(a) });
                    }
                } else {
                    let a = base.wrapping_add(i);
                    bytes.push(if holey(a) { 0 } else { bus.peek(a) });
                }
            }
            // Plot pixels; x is in 160-res units, each covering two output pixels.
            let mut x160 = hpos as usize;
            let mut plot320 = |x: usize, c: u32| {
                let x = x % 512;
                if x < 320 {
                    out[x] = c;
                }
            };
            for &b in &bytes {
                match (read_mode, wm) {
                    (0, false) => {
                        // 160A: 4 pixels of 2 bits.
                        for k in 0..4 {
                            let v = (b >> (6 - k * 2)) & 3;
                            if v != 0 || kangaroo {
                                let c = if v == 0 { bg } else { color[pal][v as usize] };
                                plot320((x160 + k) * 2, c);
                                plot320((x160 + k) * 2 + 1, c);
                            }
                        }
                        x160 += 4;
                    }
                    (0, true) => {
                        // 160B: 2 pixels of 2 bits with per-pixel palette bits.
                        for k in 0..2 {
                            let v = if k == 0 { (b >> 6) & 3 } else { (b >> 4) & 3 };
                            let pb = if k == 0 { (b >> 2) & 3 } else { b & 3 };
                            if v != 0 || kangaroo {
                                let pp = (pal & 4) | pb as usize;
                                let c = if v == 0 { bg } else { color[pp][v as usize] };
                                plot320((x160 + k) * 2, c);
                                plot320((x160 + k) * 2 + 1, c);
                            }
                        }
                        x160 += 2;
                    }
                    (3, false) | (2, false) => {
                        // 320A / 320D: 8 single-bit pixels in colour 2.
                        for k in 0..8 {
                            if (b >> (7 - k)) & 1 != 0 {
                                plot320(x160 * 2 + k, color[pal][2]);
                            } else if kangaroo {
                                plot320(x160 * 2 + k, bg);
                            }
                        }
                        x160 += 4;
                    }
                    (2, true) => {
                        // 320B: 4 pixels of 2 bits taken from bits (7,3) (6,2) (5,1) (4,0).
                        for k in 0..4 {
                            let v = ((b >> (7 - k)) & 1) << 1 | ((b >> (3 - k)) & 1);
                            if v != 0 {
                                plot320(x160 * 2 + k, color[pal][v as usize]);
                            } else if kangaroo {
                                plot320(x160 * 2 + k, bg);
                            }
                        }
                        x160 += 2;
                    }
                    _ => {
                        // 320C: 4 pixels on/off, palette bits per pixel pair.
                        for k in 0..4 {
                            let on = (b >> (7 - k)) & 1 != 0;
                            let pb = if k < 2 { (b >> 2) & 3 } else { b & 3 };
                            if on {
                                plot320(x160 * 2 + k, color[(pal & 4) | pb as usize][2]);
                            } else if kangaroo {
                                plot320(x160 * 2 + k, bg);
                            }
                        }
                        x160 += 2;
                    }
                }
            }
        }
    }

    /// Runs the CPU for one scanline, honouring WSYNC; TIA/RIOT keep time with it.
    fn run_cpu(&mut self, cycles: u32) {
        let mut used = 0;
        while used < cycles {
            let c = self.cpu.step();
            let bus = self.cpu.bus_mut();
            bus.riot.tick(c);
            bus.tia.run(c * 2);
            used += c;
            if std::mem::take(&mut bus.wsync) {
                let rest = cycles.saturating_sub(used);
                bus.riot.tick(rest);
                bus.tia.run(rest * 2);
                return;
            }
        }
    }

    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    pub fn run_frame(&mut self) {
        for _ in 0..LINES {
            let line = self.line;
            let vblank = !(DMA_START..DMA_END).contains(&line);
            self.cpu.bus_mut().maria[0x28 - 0x20] = if vblank { 0x80 } else { 0 };
            let mut nmi = false;
            if line == DMA_START {
                self.dll = (self.maria(0x2C) as u16) << 8 | self.maria(0x30) as u16;
                self.next_zone();
            }
            if !vblank {
                let mut buf = [0u32; 320];
                self.render_line(&mut buf);
                let row = (line - DMA_START) as usize;
                if row < HEIGHT {
                    self.frame[row * WIDTH..(row + 1) * WIDTH].copy_from_slice(&buf);
                }
                // End of the zone's last line: DLI interrupt, then move to the next zone.
                if self.zone_offset == 0 {
                    nmi = self.zone_flags & 0x80 != 0;
                    self.next_zone();
                } else {
                    self.zone_offset -= 1;
                }
            }
            if nmi {
                self.cpu.nmi();
            }
            // MARIA takes the bus for part of each active line.
            let cycles = if vblank { CYCLES_PER_LINE } else { CYCLES_PER_LINE - 24 };
            self.run_cpu(cycles);
            self.line = (line + 1) % LINES;
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    /// Joystick (up/down/left/right), A/B = the two fire buttons, Start = Reset,
    /// Select = Select, X = Pause.
    pub fn set_buttons(&mut self, pad: usize, b: u16) {
        let bus = self.cpu.bus_mut();
        let dirs = ((b >> 4) & 1) | ((b >> 5) & 1) << 1 | ((b >> 6) & 1) << 2 | ((b >> 7) & 1) << 3;
        let nibble = !(dirs as u8) & 0x0F;
        match pad {
            0 => {
                bus.riot.swcha = (bus.riot.swcha & 0x0F) | nibble << 4;
                bus.tia.fire[0] = b & 3 != 0;
                bus.buttons = [b & 1 != 0, b & 2 != 0];
                let (reset, select, pause) = (b & 8 != 0, b & 4 != 0, b & 0x100 != 0);
                bus.riot.swchb = (bus.riot.swchb & !0x0B) | (!reset as u8) | ((!select as u8) << 1) | ((!pause as u8) << 3);
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

    pub fn set_sample_rate(&mut self, r: u32) {
        self.cpu.bus_mut().tia.set_sample_rate(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16 KB cartridge that builds a one-zone display list showing a 160A object.
    fn test_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x4000];
        let base = 0xC000usize;
        let put = |rom: &mut Vec<u8>, a: usize, bytes: &[u8]| rom[a - base..a - base + bytes.len()].copy_from_slice(bytes);
        // Graphics at $E000 + offset: a solid byte (four pixels of colour 1) on every line.
        for z in 0..16 {
            put(&mut rom, 0xE000 + z * 0x100, &[0x55, 0x55]);
        }
        // Display list at $D000: object lo=$00 pal0|width2 hi=$E0 hpos=10, then end marker.
        put(&mut rom, 0xD000, &[0x00, 0x1E, 0xE0, 10, 0x00, 0x00]);
        // DLL at $D100: 20 zones of 16 lines each pointing at the same list.
        for z in 0..20 {
            put(&mut rom, 0xD100 + z * 3, &[0x0F, 0xD0, 0x00]);
        }
        let prog: &[u8] = &[
            0x78, 0xD8, // SEI CLD
            0xA9, 0x0E, 0x85, 0x20, // BACKGRND = $0E (white)
            0xA9, 0x44, 0x85, 0x21, // P0C1 = $44
            0xA9, 0xD1, 0x85, 0x2C, // DPPH
            0xA9, 0x00, 0x85, 0x30, // DPPL
            0xA9, 0x40, 0x85, 0x3C, // CTRL: DMA on, 160A
            0x4C, 0x16, 0xC0, // JMP self
        ];
        put(&mut rom, 0xC000, prog);
        put(&mut rom, 0xFFFC, &[0x00, 0xC0]);
        rom
    }

    #[test]
    fn save_state_resumes_identically() {
        let rom = test_rom();
        let mut a = Atari7800::new(&rom).unwrap();
        for _ in 0..3 {
            a.run_frame();
        }
        let snap = a.save_state();
        for _ in 0..4 {
            a.run_frame();
        }
        let want = a.frame().to_vec();
        let mut b = Atari7800::new(&rom).unwrap();
        b.load_state(&snap).unwrap();
        for _ in 0..4 {
            b.run_frame();
        }
        assert_eq!(b.frame().to_vec(), want);
        assert_eq!(want[5 * WIDTH + 20], palette()[0x44], "and it is the real picture, not blank");
    }

    #[test]
    fn draws_display_list_object() {
        let mut a = Atari7800::new(&test_rom()).unwrap();
        a.run_frame();
        a.run_frame();
        let p = palette();
        let row = &a.frame()[5 * WIDTH..6 * WIDTH];
        assert_eq!(row[0], p[0x0E], "background");
        // hpos 10 (160-res) = pixel 20; 2 bytes x 4 pixels = 8 wide = 16 output pixels.
        assert_eq!(row[20], p[0x44]);
        assert_eq!(row[35], p[0x44]);
        assert_eq!(row[36], p[0x0E]);
    }

    #[test]
    fn supergame_banking() {
        let mut rom = vec![0u8; 0x2_0000];
        rom[3 * 0x4000] = 0x77;
        let mut hdr = vec![0u8; 128];
        hdr[1..10].copy_from_slice(b"ATARI7800");
        hdr[54] = 0x02;
        hdr.extend(rom);
        let mut a = Atari7800::new(&hdr).unwrap();
        use cpu6502::Bus as _;
        a.cpu.bus_mut().write(0x8000, 3);
        assert_eq!(a.cpu.bus_mut().read(0x8000), 0x77);
    }
}
