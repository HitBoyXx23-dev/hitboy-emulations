//! Clean-room Nintendo DS / DSi emulator core.
//!
//! Parses Nitro `.nds`/`.dsi` ROM headers, loads the ARM9 binary into 4 MB Main RAM
//! (`0x0200_0000`), exposes dual 256×192 screens (`256×384` stacked) with live bottom
//! touchscreen input, keypad I/O (`KEYINPUT` at `0x0400_0130`), and LCDC/VRAM direct scanout.

use cpuarm_v7::{ArmCpu, Bus, StepEvent};

pub const SCREEN_W: usize = 256;
pub const SCREEN_H: usize = 192;
pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 384;
pub const FPS: f64 = 59.826;

const RAM_BASE: u32 = 0x0200_0000;
const RAM_SIZE: usize = 4 * 1024 * 1024;
const VRAM_BASE: u32 = 0x0680_0000;
const VRAM_SIZE: usize = 1024 * 1024;
const STEPS_PER_FRAME: u32 = 120_000;

/// Parsed Nitro `.nds` header metadata.
#[derive(Clone)]
pub struct NdsHeader {
    pub title: String,
    pub game_code: String,
    pub arm9_rom_off: usize,
    pub arm9_entry: u32,
    pub arm9_ram_addr: u32,
    pub arm9_size: usize,
}

pub fn parse_header(rom: &[u8]) -> Result<NdsHeader, String> {
    if rom.len() < 0x200 {
        return Err("File is too small to be a Nintendo DS ROM.".into());
    }
    let title = String::from_utf8_lossy(&rom[0..12]).trim_end_matches('\0').trim().to_string();
    let game_code = String::from_utf8_lossy(&rom[12..16]).trim_end_matches('\0').to_string();
    let arm9_rom_off = u32::from_le_bytes(rom[0x20..0x24].try_into().unwrap()) as usize;
    let arm9_entry = u32::from_le_bytes(rom[0x24..0x28].try_into().unwrap());
    let arm9_ram_addr = u32::from_le_bytes(rom[0x28..0x2C].try_into().unwrap());
    let arm9_size = u32::from_le_bytes(rom[0x2C..0x30].try_into().unwrap()) as usize;
    if arm9_rom_off + arm9_size > rom.len() || arm9_size == 0 {
        return Err("Nintendo DS ROM has an invalid ARM9 binary range.".into());
    }
    Ok(NdsHeader {
        title,
        game_code,
        arm9_rom_off,
        arm9_entry,
        arm9_ram_addr,
        arm9_size,
    })
}

pub struct NdsMem {
    pub ram: Vec<u8>,
    pub vram: Vec<u8>,
    pub palette: Vec<u8>,
    pub dispcnt_a: u32,
    pub dispcnt_b: u32,
    pub vcount: u16,
    pub keyinput: u16,  // active-low
    pub extkeyin: u16,  // active-low (X, Y, pen-down bit 6)
    pub touch_x: u16,
    pub touch_y: u16,
    pub audio_out: Vec<f32>,
}

impl NdsMem {
    pub fn new() -> Self {
        Self {
            ram: vec![0; RAM_SIZE],
            vram: vec![0; VRAM_SIZE],
            palette: vec![0; 2048],
            dispcnt_a: 0x0002_0000, // LCDC direct VRAM bank A by default
            dispcnt_b: 0x0002_0000,
            vcount: 0,
            keyinput: 0x03FF,
            extkeyin: 0x007F,
            touch_x: 0,
            touch_y: 0,
            audio_out: Vec::new(),
        }
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.write8(addr.wrapping_add(i as u32), b);
        }
    }

    pub fn scanout(&self, out: &mut [u32]) {
        // Engine A (Top 256x192) from VRAM bank A (offset 0), BGR555
        // Engine B (Bottom 256x192) from VRAM bank C (offset 0x40000), BGR555
        for screen in 0..2 {
            let vram_off = if screen == 0 { 0 } else { 0x40000 };
            let y_base = screen * SCREEN_H;
            for y in 0..SCREEN_H {
                for x in 0..SCREEN_W {
                    let o = vram_off + (y * SCREEN_W + x) * 2;
                    let v = u16::from_le_bytes([self.vram[o], self.vram[o + 1]]);
                    let r = ((v & 0x1F) as u32 * 255) / 31;
                    let g = (((v >> 5) & 0x1F) as u32 * 255) / 31;
                    let b = (((v >> 10) & 0x1F) as u32 * 255) / 31;
                    out[(y_base + y) * WIDTH + x] = (r << 16) | (g << 8) | b;
                }
            }
        }
    }
}

impl Default for NdsMem {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for NdsMem {
    #[inline]
    fn read8(&self, addr: u32) -> u8 {
        match addr {
            0x0200_0000..=0x02FF_FFFF => self.ram[(addr as usize) & (RAM_SIZE - 1)],
            0x0500_0000..=0x0500_07FF => self.palette[(addr - 0x0500_0000) as usize],
            0x0600_0000..=0x068F_FFFF => self.vram[(addr.wrapping_sub(VRAM_BASE) as usize) & (VRAM_SIZE - 1)],
            _ => 0,
        }
    }
    #[inline]
    fn read16(&self, addr: u32) -> u16 {
        match addr {
            0x0400_0006 => self.vcount,
            0x0400_0130 => self.keyinput,
            0x0400_0136 => self.extkeyin,
            _ => u16::from_le_bytes([self.read8(addr), self.read8(addr.wrapping_add(1))]),
        }
    }
    #[inline]
    fn read32(&self, addr: u32) -> u32 {
        match addr {
            0x0400_0000 => self.dispcnt_a,
            0x0400_1000 => self.dispcnt_b,
            _ => u32::from_le_bytes([
                self.read8(addr),
                self.read8(addr.wrapping_add(1)),
                self.read8(addr.wrapping_add(2)),
                self.read8(addr.wrapping_add(3)),
            ]),
        }
    }
    #[inline]
    fn write8(&mut self, addr: u32, val: u8) {
        match addr {
            0x0200_0000..=0x02FF_FFFF => self.ram[(addr as usize) & (RAM_SIZE - 1)] = val,
            0x0500_0000..=0x0500_07FF => self.palette[(addr - 0x0500_0000) as usize] = val,
            0x0600_0000..=0x068F_FFFF => self.vram[(addr.wrapping_sub(VRAM_BASE) as usize) & (VRAM_SIZE - 1)] = val,
            _ => {}
        }
    }
    #[inline]
    fn write16(&mut self, addr: u32, val: u16) {
        let b = val.to_le_bytes();
        self.write8(addr, b[0]);
        self.write8(addr.wrapping_add(1), b[1]);
    }
    #[inline]
    fn write32(&mut self, addr: u32, val: u32) {
        match addr {
            0x0400_0000 => self.dispcnt_a = val,
            0x0400_1000 => self.dispcnt_b = val,
            _ => {
                for (k, &b) in val.to_le_bytes().iter().enumerate() {
                    self.write8(addr.wrapping_add(k as u32), b);
                }
            }
        }
    }
}

pub struct NintendoDs {
    pub arm9: ArmCpu,
    pub mem: NdsMem,
    pub header: NdsHeader,
    rom: Vec<u8>,
    frame: Vec<u32>,
}

impl NintendoDs {
    pub fn from_bytes(rom: &[u8]) -> Result<Self, String> {
        let header = parse_header(rom)?;
        let mut nds = Self {
            arm9: ArmCpu::new(),
            mem: NdsMem::new(),
            header,
            rom: rom.to_vec(),
            frame: vec![0; WIDTH * HEIGHT],
        };
        nds.reset();
        Ok(nds)
    }

    pub fn reset(&mut self) {
        self.arm9 = ArmCpu::new();
        self.mem = NdsMem::new();
        let start = self.header.arm9_rom_off;
        let end = start + self.header.arm9_size;
        self.mem.write_slice(self.header.arm9_ram_addr, &self.rom[start..end]);
        self.arm9.set_pc(self.header.arm9_entry);
        self.arm9.r[13] = RAM_BASE + 0x003F_F000;
        self.frame.fill(0);
    }

    pub fn set_buttons(&mut self, buttons: u16) {
        let mut k = 0x03FFu16;
        if buttons & (1 << 0) != 0 { k &= !(1 << 0); } // A
        if buttons & (1 << 1) != 0 { k &= !(1 << 1); } // B
        if buttons & (1 << 2) != 0 { k &= !(1 << 2); } // Select
        if buttons & (1 << 3) != 0 { k &= !(1 << 3); } // Start
        if buttons & (1 << 7) != 0 { k &= !(1 << 4); } // Right
        if buttons & (1 << 6) != 0 { k &= !(1 << 5); } // Left
        if buttons & (1 << 4) != 0 { k &= !(1 << 6); } // Up
        if buttons & (1 << 5) != 0 { k &= !(1 << 7); } // Down
        if buttons & (1 << 11) != 0 { k &= !(1 << 8); } // R
        if buttons & (1 << 10) != 0 { k &= !(1 << 9); } // L
        self.mem.keyinput = k;

        let mut ek = (self.mem.extkeyin & (1 << 6)) | 0x003F;
        if buttons & (1 << 8) != 0 { ek &= !(1 << 0); } // X
        if buttons & (1 << 9) != 0 { ek &= !(1 << 1); } // Y
        self.mem.extkeyin = ek;
    }

    pub fn set_touch(&mut self, pressed: bool, nx: f32, ny: f32) {
        if pressed && ny >= 0.5 {
            self.mem.extkeyin &= !(1 << 6);
            self.mem.touch_x = (nx * SCREEN_W as f32).clamp(0.0, (SCREEN_W - 1) as f32) as u16;
            self.mem.touch_y = ((ny - 0.5) * 2.0 * SCREEN_H as f32).clamp(0.0, (SCREEN_H - 1) as f32) as u16;
        } else {
            self.mem.extkeyin |= 1 << 6;
        }
    }

    pub fn run_frame(&mut self) {
        for _ in 0..STEPS_PER_FRAME {
            match self.arm9.step(&mut self.mem) {
                StepEvent::Continue => {}
                StepEvent::Svc(0x05) => break, // VBlankIntrWait
                StepEvent::Svc(_) | StepEvent::Bkpt(_) => {}
            }
        }
        self.mem.vcount = (self.mem.vcount + 1) % 263;
        self.mem.scanout(&mut self.frame);
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.mem.audio_out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nds_boots_arm9_and_scans_out_vram() {
        // ARM9 writes pure blue BGR555 (0x7C00) to VRAM bank A (0x06800000) then SWI 5 (VBlankIntrWait)
        let words: [u32; 4] = [
            0xE3A0_0406, // mov r0, #0x06000000
            0xE280_0720, // add r0, r0, #0x00800000
            0xE3A0_1B1F, // mov r1, #0x7C00
            0xE1C0_10B0, // strh r1, [r0]
        ];
        let mut rom = vec![0u8; 0x400];
        rom[0..10].copy_from_slice(b"NDS TEST  ");
        rom[0x20..0x24].copy_from_slice(&0x200u32.to_le_bytes());
        rom[0x24..0x28].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        rom[0x28..0x2C].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        rom[0x2C..0x30].copy_from_slice(&16u32.to_le_bytes());
        for (i, w) in words.iter().enumerate() {
            rom[0x200 + i * 4..0x204 + i * 4].copy_from_slice(&w.to_le_bytes());
        }
        let mut nds = NintendoDs::from_bytes(&rom).unwrap();
        assert_eq!(nds.header.title, "NDS TEST");
        nds.run_frame();
        assert_eq!(nds.frame()[0], 0x0000_00FF);
    }
}
