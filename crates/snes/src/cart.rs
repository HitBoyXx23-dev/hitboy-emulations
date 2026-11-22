//! SNES cartridges: LoROM / HiROM mapping and battery-backed SRAM.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Map {
    LoRom,
    HiRom,
    /// Carts over 4 MB (Tales of Phantasia, Star Ocean): header at $40FFC0.
    ExHiRom,
    /// Super FX games: LoROM-style banks $00-$3F, linear ROM at $40-$5F, work RAM at $70-$71.
    Gsu,
}

#[derive(state::Stateful)]
pub struct Cart {
    #[state(skip)]
    rom: Vec<u8>,
    pub sram: Vec<u8>,
    #[state(skip)]
    pub map: Map,
    #[state(skip)]
    pub title: String,
    #[state(skip)]
    pub fast_rom: bool,
    pub gsu: Option<crate::gsu::Gsu>,
}

/// Scores a candidate header location; higher means more likely.
fn score(rom: &[u8], at: usize, map: Map) -> i32 {
    if rom.len() < at + 0x40 {
        return i32::MIN;
    }
    let h = &rom[at..at + 0x40];
    let mut s = 0;
    let checksum = u16::from_le_bytes([h[0x1E], h[0x1F]]);
    let complement = u16::from_le_bytes([h[0x1C], h[0x1D]]);
    if checksum.wrapping_add(complement) == 0xFFFF {
        s += 8;
    }
    let mode = h[0x15] & 0x0F;
    if (map == Map::LoRom && mode == 0) || (map == Map::HiRom && mode == 1) || (map == Map::ExHiRom && mode == 5) {
        s += 4;
    }
    if h[0x10..0x15].iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        s += 2;
    }
    // Reset vector should point into ROM.
    let reset = u16::from_le_bytes([h[0x3C], h[0x3D]]);
    if reset >= 0x8000 {
        s += 2;
    }
    s
}

impl Cart {
    pub fn load(mut rom: Vec<u8>) -> Result<Self, String> {
        if rom.len() % 1024 == 512 {
            rom.drain(..512); // copier header
        }
        if rom.len() < 0x8000 {
            return Err("File is too small to be a SNES game.".into());
        }
        let lo = score(&rom, 0x7FC0, Map::LoRom);
        let hi = score(&rom, 0xFFC0, Map::HiRom);
        let ex = if rom.len() > 0x40_0000 { score(&rom, 0x40_FFC0, Map::ExHiRom) } else { i32::MIN };
        let (map, at) = if ex >= hi.max(lo) {
            (Map::ExHiRom, 0x40_FFC0)
        } else if hi > lo {
            (Map::HiRom, 0xFFC0)
        } else {
            (Map::LoRom, 0x7FC0)
        };
        let h = &rom[at..at + 0x40];
        let title: String = h[..21].iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { ' ' }).collect::<String>().trim().to_string();
        let sram_size = match h[0x18] {
            0 => 0,
            n if n <= 7 => 1024usize << n,
            _ => 0x2000,
        };
        let fast_rom = h[0x15] & 0x10 != 0;
        // Cartridge type: high nibble 1 with a coprocessor flag is the Super FX.
        let chip = h[0x16];
        if map == Map::LoRom && chip >> 4 == 1 && chip & 0x0F >= 3 {
            let ram = if sram_size == 0 { 0x8000 } else { sram_size };
            return Ok(Self { rom, sram: vec![0; ram], map: Map::Gsu, title, fast_rom, gsu: Some(crate::gsu::Gsu::default()) });
        }
        Ok(Self { rom, sram: vec![0xFF; sram_size], map, title, fast_rom, gsu: None })
    }

    /// Gives the Super FX (if any) one scanline of running time.
    pub fn run_coprocessor_line(&mut self) {
        if let Some(g) = self.gsu.as_mut() {
            let budget = g.clocks_per_line();
            g.run(&self.rom, &mut self.sram, budget);
        }
    }

    /// The cartridge's own interrupt line (Super FX finished).
    pub fn irq(&self) -> bool {
        self.gsu.as_ref().is_some_and(|g| g.irq())
    }

    pub fn rom_len(&self) -> usize {
        self.rom.len()
    }

    #[inline]
    fn rom_at(&self, off: usize) -> u8 {
        // Addresses past the end mirror back into the ROM.
        self.rom[off % self.rom.len()]
    }

    /// Returns None for addresses the cartridge doesn't answer (open bus).
    pub fn read(&mut self, bank: u8, addr: u16) -> Option<u8> {
        let b = bank & 0x7F;
        match self.map {
            Map::Gsu => {
                if b < 0x40 {
                    if (0x3000..0x3300).contains(&addr) {
                        return Some(self.gsu.as_mut().map_or(0, |g| g.read(addr)));
                    }
                    if addr >= 0x8000 {
                        return Some(self.rom_at(b as usize * 0x8000 + (addr as usize - 0x8000)));
                    }
                    if (0x6000..0x8000).contains(&addr) {
                        return Some(self.sram[(addr as usize - 0x6000) % self.sram.len()]);
                    }
                    return None;
                }
                if b < 0x60 {
                    return Some(self.rom_at((b as usize - 0x40) * 0x10000 + addr as usize));
                }
                if (0x70..=0x71).contains(&b) {
                    return Some(self.sram[((b as usize - 0x70) * 0x10000 + addr as usize) % self.sram.len()]);
                }
                None
            }
            Map::LoRom => {
                if addr >= 0x8000 && !(bank == 0x7E || bank == 0x7F) {
                    return Some(self.rom_at(b as usize * 0x8000 + (addr as usize - 0x8000)));
                }
                if (0x70..=0x7D).contains(&b) && addr < 0x8000 && !self.sram.is_empty() {
                    let off = ((b as usize - 0x70) * 0x8000 + addr as usize) % self.sram.len();
                    return Some(self.sram[off]);
                }
                if (0x40..=0x6F).contains(&b) && addr < 0x8000 {
                    return Some(self.rom_at(b as usize * 0x8000 + addr as usize));
                }
                None
            }
            Map::ExHiRom => {
                // $C0-$FF (and $80-$BF upper halves) show the first 4 MB; $40-$7D (and
                // $00-$3F upper halves) the rest.
                let high = bank < 0x80;
                let base = if high { 0x40_0000 } else { 0 };
                if (0x40..=0x7D).contains(&b) || bank >= 0xC0 || addr >= 0x8000 {
                    return Some(self.rom_at(base + (b as usize & 0x3F) * 0x10000 + addr as usize));
                }
                if (0x20..=0x3F).contains(&b) && (0x6000..0x8000).contains(&addr) && !self.sram.is_empty() {
                    let off = ((b as usize - 0x20) * 0x2000 + (addr as usize - 0x6000)) % self.sram.len();
                    return Some(self.sram[off]);
                }
                None
            }
            Map::HiRom => {
                if (0x40..=0x7D).contains(&b) || bank >= 0xC0 {
                    return Some(self.rom_at((b as usize & 0x3F) * 0x10000 + addr as usize));
                }
                if addr >= 0x8000 {
                    return Some(self.rom_at((b as usize & 0x3F) * 0x10000 + addr as usize));
                }
                if (0x20..=0x3F).contains(&b) && (0x6000..0x8000).contains(&addr) && !self.sram.is_empty() {
                    let off = ((b as usize - 0x20) * 0x2000 + (addr as usize - 0x6000)) % self.sram.len();
                    return Some(self.sram[off]);
                }
                None
            }
        }
    }

    pub fn write(&mut self, bank: u8, addr: u16, v: u8) {
        if self.map == Map::Gsu {
            let b = bank & 0x7F;
            if b < 0x40 && (0x3000..0x3300).contains(&addr) {
                if let Some(g) = self.gsu.as_mut() {
                    g.write(addr, v, &self.rom, &self.sram);
                }
            } else if b < 0x40 && (0x6000..0x8000).contains(&addr) {
                let n = self.sram.len();
                self.sram[(addr as usize - 0x6000) % n] = v;
            } else if (0x70..=0x71).contains(&b) {
                let n = self.sram.len();
                self.sram[((b as usize - 0x70) * 0x10000 + addr as usize) % n] = v;
            }
            return;
        }
        if self.sram.is_empty() {
            return;
        }
        let b = bank & 0x7F;
        let len = self.sram.len();
        match self.map {
            Map::LoRom if (0x70..=0x7D).contains(&b) && addr < 0x8000 => {
                self.sram[((b as usize - 0x70) * 0x8000 + addr as usize) % len] = v;
            }
            Map::HiRom | Map::ExHiRom if (0x20..=0x3F).contains(&b) && (0x6000..0x8000).contains(&addr) => {
                self.sram[((b as usize - 0x20) * 0x2000 + (addr as usize - 0x6000)) % len] = v;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom_with_chip(chip: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 0x10_0000];
        let h = 0x7FC0;
        rom[h..h + 5].copy_from_slice(b"STAR ");
        rom[h + 0x15] = 0x20;
        rom[h + 0x16] = chip;
        rom[h + 0x1C..h + 0x1E].copy_from_slice(&0u16.to_le_bytes());
        rom[h + 0x1E..h + 0x20].copy_from_slice(&0xFFFFu16.to_le_bytes());
        rom[h + 0x3C..h + 0x3E].copy_from_slice(&0x8000u16.to_le_bytes());
        rom
    }

    #[test]
    fn super_fx_carts_are_recognised_and_get_work_ram() {
        let mut c = Cart::load(rom_with_chip(0x13)).unwrap();
        assert_eq!(c.map, Map::Gsu);
        assert!(c.gsu.is_some());
        assert_eq!(c.sram.len(), 0x8000);
        c.write(0x70, 0x0010, 0xAB);
        assert_eq!(c.read(0x70, 0x0010), Some(0xAB));
        assert_eq!(c.read(0x00, 0x6010), Some(0xAB), "the first 8 KB also appears at $6000");
        assert_eq!(c.read(0x00, 0x303B), Some(4), "version register");
        // Plain carts are unaffected.
        let plain = Cart::load(rom_with_chip(0x00)).unwrap();
        assert!(plain.gsu.is_none());
    }
}
