//! Clean-room Sega Saturn emulator core built on `cpush` (Hitachi SH-2).
//!
//! Maps Saturn Work RAM High (1 MB at `0x0600_0000`), Work RAM Low (1 MB at `0x0020_0000`),
//! VDP2 VRAM (512 KB at `0x05E0_0000`) with 320×224 BGR555 framebuffer scanout, and SMPC input.

use cpush::{Bus, ShCpu, StepEvent};

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 224;
pub const FPS: f64 = 59.94;

const STEPS_PER_FRAME: u32 = 120_000;

pub struct SaturnMem {
    pub wram_lo: Vec<u8>,
    pub wram_hi: Vec<u8>,
    pub vdp2_vram: Vec<u8>,
    pub pad_buttons: u16,
    pub audio_out: Vec<f32>,
}

impl SaturnMem {
    pub fn new() -> Self {
        Self {
            wram_lo: vec![0; 1024 * 1024],
            wram_hi: vec![0; 1024 * 1024],
            vdp2_vram: vec![0; 512 * 1024],
            pad_buttons: 0xFFFF,
            audio_out: Vec::new(),
        }
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.write8(addr.wrapping_add(i as u32), b);
        }
    }

    pub fn scanout(&self, out: &mut [u32]) {
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let o = (y * WIDTH + x) * 2;
                let v = u16::from_be_bytes([self.vdp2_vram[o], self.vdp2_vram[o + 1]]);
                let r = ((v & 0x1F) as u32 * 255) / 31;
                let g = (((v >> 5) & 0x1F) as u32 * 255) / 31;
                let b = (((v >> 10) & 0x1F) as u32 * 255) / 31;
                out[y * WIDTH + x] = (r << 16) | (g << 8) | b;
            }
        }
    }
}

impl Default for SaturnMem {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for SaturnMem {
    fn read8(&self, addr: u32) -> u8 {
        let p = addr & 0x1FFF_FFFF;
        match p {
            0x0020_0000..=0x002F_FFFF => self.wram_lo[(p - 0x0020_0000) as usize],
            0x05E0_0000..=0x05E7_FFFF => self.vdp2_vram[(p - 0x05E0_0000) as usize],
            0x0600_0000..=0x060F_FFFF => self.wram_hi[(p - 0x0600_0000) as usize],
            _ => 0,
        }
    }
    fn read16(&self, addr: u32) -> u16 {
        // Saturn SH-2 is big-endian
        u16::from_be_bytes([self.read8(addr), self.read8(addr.wrapping_add(1))])
    }
    fn read32(&self, addr: u32) -> u32 {
        u32::from_be_bytes([
            self.read8(addr),
            self.read8(addr.wrapping_add(1)),
            self.read8(addr.wrapping_add(2)),
            self.read8(addr.wrapping_add(3)),
        ])
    }
    fn write8(&mut self, addr: u32, val: u8) {
        let p = addr & 0x1FFF_FFFF;
        match p {
            0x0020_0000..=0x002F_FFFF => self.wram_lo[(p - 0x0020_0000) as usize] = val,
            0x05E0_0000..=0x05E7_FFFF => self.vdp2_vram[(p - 0x05E0_0000) as usize] = val,
            0x0600_0000..=0x060F_FFFF => self.wram_hi[(p - 0x0600_0000) as usize] = val,
            _ => {}
        }
    }
    fn write16(&mut self, addr: u32, val: u16) {
        let b = val.to_be_bytes();
        self.write8(addr, b[0]);
        self.write8(addr.wrapping_add(1), b[1]);
    }
    fn write32(&mut self, addr: u32, val: u32) {
        for (k, &b) in val.to_be_bytes().iter().enumerate() {
            self.write8(addr.wrapping_add(k as u32), b);
        }
    }
}

pub struct Saturn {
    pub sh2: ShCpu,
    pub mem: SaturnMem,
    pub title: String,
    program: Vec<u8>,
    frame: Vec<u32>,
}

impl Saturn {
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let mut title = String::new();
        if let Some(pos) = data.windows(15).take(65536).position(|w| w == b"SEGA SEGASATURN") {
            if pos + 0xD0 <= data.len() {
                title = String::from_utf8_lossy(&data[pos + 0x60..pos + 0xD0]).trim().to_string();
            }
        }
        let mut s = Self {
            sh2: ShCpu::new(),
            mem: SaturnMem::new(),
            title,
            program: data.to_vec(),
            frame: vec![0; WIDTH * HEIGHT],
        };
        s.reset();
        Ok(s)
    }

    pub fn reset(&mut self) {
        self.sh2 = ShCpu::new();
        self.mem = SaturnMem::new();
        let len = self.program.len().min(1024 * 1024 - 0x4000);
        self.mem.write_slice(0x0600_4000, &self.program[..len]);
        self.sh2.pc = 0x0600_4000;
        self.sh2.r[15] = 0x060F_F000;
        self.frame.fill(0);
    }

    pub fn set_buttons(&mut self, buttons: u16) {
        self.mem.pad_buttons = !buttons;
    }

    pub fn run_frame(&mut self) {
        for _ in 0..STEPS_PER_FRAME {
            if matches!(self.sh2.step(&mut self.mem), StepEvent::Sleep) {
                break;
            }
        }
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
    fn saturn_sh2_draws_vdp2_bgr555_pixel() {
        // Big-endian SH-2 at 0x06004000: write red (0x001F) to VDP2 VRAM 0x05E00000
        let words: [u16; 8] = [
            0xD102, // 0x00: MOV.L @(0x0C), R1 -> 0x05E00000
            0xE21F, // 0x02: MOV #0x1F, R2
            0x2121, // 0x04: MOV.W R2, @R1
            0xAFFE, // 0x06: BRA -2
            0x0009, // 0x08: NOP
            0x0000, // 0x0A: pad
            0x05E0, // 0x0C: high16 of 0x05E00000
            0x0000, // 0x0E: low16
        ];
        let bin: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        let mut sat = Saturn::from_bytes(&bin).unwrap();
        sat.run_frame();
        assert_eq!(sat.frame()[0], 0x00FF_0000);
    }
}
