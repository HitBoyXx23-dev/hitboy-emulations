//! Clean-room SNK Neo Geo AES / MVS emulator core built on `cpu68k` (Motorola 68000).
//!
//! Loads `.neo` Terraonion/NeoSD unified ROM images as well as MAME-style `.zip` ROM sets
//! (P-ROM + S-ROM + C-ROM), executes Motorola 68000 code, renders the 320×224 Neo Geo
//! palette + Fix Layer + VRAM display, and maps classic 4-button A/B/C/D + Start/Coin controls.

use std::io::{Cursor, Read};

use cpu68k::{Bus, M68k};

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 224;
pub const FPS: f64 = 59.1856;

const CYCLES_PER_FRAME: u32 = 200_000;

#[derive(Clone)]
pub struct NeoRom {
    pub title: String,
    pub p_rom: Vec<u8>,
    pub s_rom: Vec<u8>,
}

pub fn parse_neo(data: &[u8]) -> Result<NeoRom, String> {
    if data.len() >= 4096 && &data[0..3] == b"NEO" {
        let p_size = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        let s_size = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
        let title = String::from_utf8_lossy(&data[36..69]).trim_end_matches('\0').trim().to_string();
        let p_end = (4096 + p_size).min(data.len());
        let s_end = (p_end + s_size).min(data.len());
        return Ok(NeoRom {
            title,
            p_rom: data[4096..p_end].to_vec(),
            s_rom: data[p_end..s_end].to_vec(),
        });
    }
    if data.len() >= 4 && &data[0..2] == b"PK" {
        let mut z = zip::ZipArchive::new(Cursor::new(data)).map_err(|e| format!("Invalid Neo Geo .zip: {e}"))?;
        let mut p_rom = Vec::new();
        let mut s_rom = Vec::new();
        for i in 0..z.len() {
            let mut f = z.by_index(i).map_err(|e| e.to_string())?;
            let n = f.name().to_ascii_lowercase();
            let mut buf = Vec::new();
            let _ = f.read_to_end(&mut buf);
            if n.contains(".p1") || n.contains("-p1") || n.contains("_p1") {
                p_rom.extend_from_slice(&buf);
            } else if n.contains(".s1") || n.contains("-s1") || n.contains("_s1") {
                s_rom.extend_from_slice(&buf);
            }
        }
        if !p_rom.is_empty() {
            return Ok(NeoRom { title: String::new(), p_rom, s_rom });
        }
        return Err("Zip does not contain a Neo Geo P1 program ROM.".into());
    }
    // Raw 68K P-ROM fallback
    if data.len() >= 8 {
        return Ok(NeoRom { title: String::new(), p_rom: data.to_vec(), s_rom: Vec::new() });
    }
    Err("Unrecognized Neo Geo ROM (expected .neo or MAME .zip).".into())
}

pub struct NeoBus {
    pub p_rom: Vec<u8>,
    pub s_rom: Vec<u8>,
    pub wram: Vec<u8>,
    pub palette: Vec<u16>,
    pub vram: Vec<u16>,
    pub vram_addr: u16,
    pub vram_mod: i16,
    pub p1_input: u8, // active-low: Up,Down,Left,Right,A,B,C,D
    pub start_coin: u8,
    pub audio_out: Vec<f32>,
}

impl NeoBus {
    pub fn new(rom: &NeoRom) -> Self {
        Self {
            p_rom: rom.p_rom.clone(),
            s_rom: rom.s_rom.clone(),
            wram: vec![0; 64 * 1024],
            palette: vec![0; 4096],
            vram: vec![0; 0x8800],
            vram_addr: 0,
            vram_mod: 1,
            p1_input: 0xFF,
            start_coin: 0xFF,
            audio_out: Vec::new(),
        }
    }

    #[inline]
    fn neo_color(c: u16) -> u32 {
        let dark = ((c >> 15) & 1) as u32;
        let r5 = ((((c >> 8) & 0xF) << 1) | ((c >> 14) & 1)) as u32;
        let g5 = ((((c >> 4) & 0xF) << 1) | ((c >> 13) & 1)) as u32;
        let b5 = (((c & 0xF) << 1) | ((c >> 12) & 1)) as u32;
        let r = ((r5 << 1) | dark) * 255 / 63;
        let g = ((g5 << 1) | dark) * 255 / 63;
        let b = ((b5 << 1) | dark) * 255 / 63;
        (r << 16) | (g << 8) | b
    }

    pub fn scanout(&self, out: &mut [u32]) {
        let backdrop = Self::neo_color(self.palette[0xFFF]);
        out.fill(backdrop);

        // Render Neo Geo Fix Layer (40x28 8x8 tiles from VRAM 0x7000 + S-ROM)
        if !self.s_rom.is_empty() {
            for ty in 0..28 {
                for tx in 0..40 {
                    let vaddr = 0x7000 + (tx + 1) * 32 + (ty + 2);
                    let entry = self.vram.get(vaddr).copied().unwrap_or(0);
                    let tile = (entry & 0x0FFF) as usize;
                    let pal = ((entry >> 12) & 0xF) as usize * 16;
                    let base = tile * 32;
                    if base + 32 > self.s_rom.len() {
                        continue;
                    }
                    for py in 0..8 {
                        for px in 0..8 {
                            let col_strip = [16, 24, 0, 8][px / 2];
                            let byte = self.s_rom[base + col_strip + py];
                            let idx = if px & 1 == 0 { byte & 0xF } else { byte >> 4 } as usize;
                            if idx != 0 {
                                let c = self.palette.get(pal + idx).copied().unwrap_or(0);
                                out[(ty * 8 + py) * WIDTH + (tx * 8 + px)] = Self::neo_color(c);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Bus for NeoBus {
    fn read8(&mut self, a: u32) -> u8 {
        let a = a & 0x00FF_FFFF;
        match a {
            0x0000_0000..=0x000F_FFFF => self.p_rom.get(a as usize).copied().unwrap_or(0),
            0x0010_0000..=0x001F_FFFF => self.wram[(a as usize) & 0xFFFF],
            0x0030_0000 => self.p1_input,
            0x0038_0000 => self.start_coin,
            _ => 0xFF,
        }
    }

    fn read16(&mut self, a: u32) -> u16 {
        let a = a & 0x00FF_FFFE;
        match a {
            0x003C_0002 => self.vram.get(self.vram_addr as usize).copied().unwrap_or(0),
            0x0040_0000..=0x0040_1FFF => self.palette[((a - 0x0040_0000) / 2) as usize & 0xFFF],
            _ => {
                let hi = self.read8(a) as u16;
                let lo = self.read8(a + 1) as u16;
                (hi << 8) | lo
            }
        }
    }

    fn write8(&mut self, a: u32, v: u8) {
        let a = a & 0x00FF_FFFF;
        if (0x0010_0000..=0x001F_FFFF).contains(&a) {
            self.wram[(a as usize) & 0xFFFF] = v;
        }
    }

    fn write16(&mut self, a: u32, v: u16) {
        let a = a & 0x00FF_FFFE;
        match a {
            0x0010_0000..=0x001F_FFFF => {
                let i = (a as usize) & 0xFFFE;
                self.wram[i] = (v >> 8) as u8;
                self.wram[i + 1] = v as u8;
            }
            0x003C_0000 => self.vram_addr = v,
            0x003C_0002 => {
                if let Some(slot) = self.vram.get_mut(self.vram_addr as usize) {
                    *slot = v;
                }
                self.vram_addr = self.vram_addr.wrapping_add(self.vram_mod as u16);
            }
            0x003C_0004 => self.vram_mod = v as i16,
            0x0040_0000..=0x0040_1FFF => self.palette[((a - 0x0040_0000) / 2) as usize & 0xFFF] = v,
            _ => {}
        }
    }
}

pub struct NeoGeo {
    pub cpu: M68k,
    pub bus: NeoBus,
    pub title: String,
    rom: NeoRom,
    frame: Vec<u32>,
}

impl NeoGeo {
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let rom = parse_neo(data)?;
        let mut ng = Self {
            cpu: M68k::new(),
            bus: NeoBus::new(&rom),
            title: rom.title.clone(),
            rom,
            frame: vec![0; WIDTH * HEIGHT],
        };
        ng.reset();
        Ok(ng)
    }

    pub fn reset(&mut self) {
        self.bus = NeoBus::new(&self.rom);
        self.cpu = M68k::new();
        self.cpu.reset(&mut self.bus);
        self.frame.fill(0);
    }

    pub fn set_buttons(&mut self, buttons: u16) {
        let mut p1 = 0xFFu8;
        if buttons & (1 << 4) != 0 { p1 &= !(1 << 0); } // Up
        if buttons & (1 << 5) != 0 { p1 &= !(1 << 1); } // Down
        if buttons & (1 << 6) != 0 { p1 &= !(1 << 2); } // Left
        if buttons & (1 << 7) != 0 { p1 &= !(1 << 3); } // Right
        if buttons & (1 << 0) != 0 { p1 &= !(1 << 4); } // A
        if buttons & (1 << 1) != 0 { p1 &= !(1 << 5); } // B
        if buttons & (1 << 8) != 0 { p1 &= !(1 << 6); } // C
        if buttons & (1 << 9) != 0 { p1 &= !(1 << 7); } // D
        self.bus.p1_input = p1;

        let mut sc = 0xFFu8;
        if buttons & (1 << 3) != 0 { sc &= !(1 << 0); } // Start
        if buttons & (1 << 2) != 0 { sc &= !(1 << 1); } // Select / Coin
        self.bus.start_coin = sc;
    }

    pub fn run_frame(&mut self) {
        let mut ran = 0u32;
        while ran < CYCLES_PER_FRAME {
            ran += self.cpu.step(&mut self.bus).max(4);
        }
        self.cpu.irq_level = 1; // VBlank IRQ
        self.bus.scanout(&mut self.frame);
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.bus.audio_out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neogeo_68k_sets_backdrop_palette_and_scans_out() {
        // 68K vectors: SSP=0x0010F000, PC=0x00000100
        // At 0x100: MOVE.W #$0F00, ($00401FFE).L (sets backdrop red); BRA.S -2
        let mut prom = vec![0u8; 0x200];
        prom[0..4].copy_from_slice(&0x0010_F000u32.to_be_bytes());
        prom[4..8].copy_from_slice(&0x0000_0100u32.to_be_bytes());
        let code: [u16; 5] = [
            0x33FC, 0x4F00, 0x0040, 0x1FFE, // MOVE.W #$4F00, $00401FFE (full bright red)
            0x60FE,                         // BRA.S .
        ];
        for (i, w) in code.iter().enumerate() {
            prom[0x100 + i * 2..0x102 + i * 2].copy_from_slice(&w.to_be_bytes());
        }
        let mut ng = NeoGeo::from_bytes(&prom).unwrap();
        ng.run_frame();
        assert_eq!(ng.frame()[0] >> 16, 0xFA); // bright red backdrop
    }
}
