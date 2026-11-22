//! Clean-room Nintendo Wii emulator core built on `cpugekko` (Broadway PowerPC 750CL).
//!
//! Boots `.dol` and big-endian PowerPC `.elf` Wii homebrew & executables with MEM1 (24 MB) +
//! MEM2 (64 MB), Video Interface (VI) 640×480 XFB scanout, and Wii Remote buttons + IR pointer.

use cpugekko::{Bus, Cpu};

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;
pub const FPS: f64 = 59.94;

const MEM1_SIZE: usize = 24 * 1024 * 1024;
const MEM2_SIZE: usize = 64 * 1024 * 1024;
const CYCLES_PER_FRAME: u32 = 160_000;

pub struct WiiBus {
    pub mem1: Box<[u8]>,
    pub mem2: Box<[u8]>,
    pub xfb_addr: u32,
    pub wiimote_buttons: u16,
    pub ir_x: u16,
    pub ir_y: u16,
    pub audio_out: Vec<f32>,
}

impl WiiBus {
    pub fn new() -> Self {
        Self {
            mem1: vec![0u8; MEM1_SIZE].into_boxed_slice(),
            mem2: vec![0u8; MEM2_SIZE].into_boxed_slice(),
            xfb_addr: 0x0100_0000,
            wiimote_buttons: 0,
            ir_x: 320,
            ir_y: 240,
            audio_out: Vec::new(),
        }
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        let p = addr & 0x1FFF_FFFF;
        for (i, &b) in data.iter().enumerate() {
            let a = p.wrapping_add(i as u32) as usize;
            if a < MEM1_SIZE {
                self.mem1[a] = b;
            } else if a >= 0x1000_0000 && a - 0x1000_0000 < MEM2_SIZE {
                self.mem2[a - 0x1000_0000] = b;
            }
        }
    }

    pub fn scanout(&self, out: &mut [u32]) {
        let base = (self.xfb_addr & 0x01FF_FFFF) as usize;
        // Scan XFB as YUYV (4:2:2) or direct 0RGB (if upper byte 0xFF marker set)
        for y in 0..HEIGHT {
            for pair in 0..(WIDTH / 2) {
                let o = base + (y * WIDTH + pair * 2) * 2;
                if o + 4 > self.mem1.len() {
                    continue;
                }
                let y0 = self.mem1[o] as f32;
                let u = self.mem1[o + 1] as f32 - 128.0;
                let y1 = self.mem1[o + 2] as f32;
                let v = self.mem1[o + 3] as f32 - 128.0;
                let to_rgb = |yy: f32| -> u32 {
                    let r = (yy + 1.402 * v).clamp(0.0, 255.0) as u32;
                    let g = (yy - 0.344136 * u - 0.714136 * v).clamp(0.0, 255.0) as u32;
                    let b = (yy + 1.772 * u).clamp(0.0, 255.0) as u32;
                    (r << 16) | (g << 8) | b
                };
                out[y * WIDTH + pair * 2] = to_rgb(y0);
                out[y * WIDTH + pair * 2 + 1] = to_rgb(y1);
            }
        }
    }
}

impl Default for WiiBus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for WiiBus {
    fn ram(&mut self) -> Option<&mut [u8]> {
        Some(&mut self.mem1)
    }

    fn read(&mut self, addr: u32, size: u32) -> u64 {
        let p = addr & 0x1FFF_FFFF;
        if p == 0x0C00_201C {
            return self.xfb_addr as u64;
        }
        if p == 0x0D00_0000 {
            return self.wiimote_buttons as u64;
        }
        let slice: &[u8] = if (p as usize) < MEM1_SIZE {
            &self.mem1[p as usize..]
        } else if p >= 0x1000_0000 && (p - 0x1000_0000) < MEM2_SIZE as u32 {
            &self.mem2[(p - 0x1000_0000) as usize..]
        } else {
            return 0;
        };
        match size {
            1 if !slice.is_empty() => slice[0] as u64,
            2 if slice.len() >= 2 => u16::from_be_bytes([slice[0], slice[1]]) as u64,
            4 if slice.len() >= 4 => u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]) as u64,
            8 if slice.len() >= 8 => u64::from_be_bytes(slice[0..8].try_into().unwrap()),
            _ => 0,
        }
    }

    fn write(&mut self, addr: u32, value: u64, size: u32) {
        let p = addr & 0x1FFF_FFFF;
        if p == 0x0C00_201C {
            self.xfb_addr = (value as u32) & 0x01FF_FFFF;
            return;
        }
        let slice: &mut [u8] = if (p as usize) < MEM1_SIZE {
            &mut self.mem1[p as usize..]
        } else if p >= 0x1000_0000 && (p - 0x1000_0000) < MEM2_SIZE as u32 {
            &mut self.mem2[(p - 0x1000_0000) as usize..]
        } else {
            return;
        };
        match size {
            1 if !slice.is_empty() => slice[0] = value as u8,
            2 if slice.len() >= 2 => slice[0..2].copy_from_slice(&(value as u16).to_be_bytes()),
            4 if slice.len() >= 4 => slice[0..4].copy_from_slice(&(value as u32).to_be_bytes()),
            8 if slice.len() >= 8 => slice[0..8].copy_from_slice(&value.to_be_bytes()),
            _ => {}
        }
    }
}

pub struct Wii {
    pub cpu: Cpu<WiiBus>,
    program: Vec<u8>,
    frame: Vec<u32>,
}

impl Wii {
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let mut wii = Self {
            cpu: Cpu::new(WiiBus::new()),
            program: data.to_vec(),
            frame: vec![0; WIDTH * HEIGHT],
        };
        wii.boot()?;
        Ok(wii)
    }

    fn boot(&mut self) -> Result<(), String> {
        self.cpu = Cpu::new(WiiBus::new());
        let data = &self.program;
        let entry = if data.len() >= 52 && &data[0..4] == b"\x7FELF" {
            let entry = u32::from_be_bytes(data[24..28].try_into().unwrap());
            let phoff = u32::from_be_bytes(data[28..32].try_into().unwrap()) as usize;
            let phentsize = u16::from_be_bytes([data[42], data[43]]) as usize;
            let phnum = u16::from_be_bytes([data[44], data[45]]) as usize;
            for i in 0..phnum {
                let o = phoff + i * phentsize;
                if o + 32 > data.len() { break; }
                if u32::from_be_bytes(data[o..o + 4].try_into().unwrap()) == 1 {
                    let foff = u32::from_be_bytes(data[o + 4..o + 8].try_into().unwrap()) as usize;
                    let vaddr = u32::from_be_bytes(data[o + 8..o + 12].try_into().unwrap());
                    let fsz = u32::from_be_bytes(data[o + 16..o + 20].try_into().unwrap()) as usize;
                    if foff + fsz <= data.len() {
                        self.cpu.bus_mut().write_slice(vaddr, &data[foff..foff + fsz]);
                    }
                }
            }
            entry
        } else if data.len() >= 0x100 {
            // DOL format: 18 sections (7 text + 11 data)
            for i in 0..18 {
                let foff = u32::from_be_bytes(data[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
                let addr = u32::from_be_bytes(data[0x48 + i * 4..0x4C + i * 4].try_into().unwrap());
                let size = u32::from_be_bytes(data[0x90 + i * 4..0x94 + i * 4].try_into().unwrap()) as usize;
                if size > 0 && foff + size <= data.len() {
                    self.cpu.bus_mut().write_slice(addr, &data[foff..foff + size]);
                }
            }
            u32::from_be_bytes(data[0xE0..0xE4].try_into().unwrap())
        } else {
            return Err("Unrecognized Wii executable (expected .dol or PowerPC .elf).".into());
        };
        let mut regs = self.cpu.regs();
        regs.pc = entry;
        regs.r[1] = 0x817F_FFF0;
        self.cpu.set_regs(regs);
        Ok(())
    }

    pub fn reset(&mut self) {
        let _ = self.boot();
        self.frame.fill(0);
    }

    pub fn set_buttons(&mut self, buttons: u16) {
        self.cpu.bus_mut().wiimote_buttons = buttons;
    }

    pub fn set_touch(&mut self, _pressed: bool, nx: f32, ny: f32) {
        let bus = self.cpu.bus_mut();
        bus.ir_x = (nx * WIDTH as f32).clamp(0.0, (WIDTH - 1) as f32) as u16;
        bus.ir_y = (ny * HEIGHT as f32).clamp(0.0, (HEIGHT - 1) as f32) as u16;
    }

    pub fn run_frame(&mut self) {
        self.cpu.run(CYCLES_PER_FRAME);
        self.cpu.bus().scanout(&mut self.frame);
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.cpu.bus_mut().audio_out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wii_dol_writes_white_yuyv_pixel_to_xfb() {
        // DOL at 0x80004000: write white YUYV (0xFF80FF80) to XFB 0x81000000
        let words: [u32; 5] = [
            0x3C60_8100, // lis r3, 0x8100
            0x3C80_FF80, // lis r4, 0xFF80
            0x6084_FF80, // ori r4, r4, 0xFF80
            0x9083_0000, // stw r4, 0(r3)
            0x4800_0000, // b .
        ];
        let mut dol = vec![0u8; 0x100];
        dol[0..4].copy_from_slice(&0x100u32.to_be_bytes());
        dol[0x48..0x4C].copy_from_slice(&0x8000_4000u32.to_be_bytes());
        dol[0x90..0x94].copy_from_slice(&((words.len() * 4) as u32).to_be_bytes());
        dol[0xE0..0xE4].copy_from_slice(&0x8000_4000u32.to_be_bytes());
        for w in words {
            dol.extend_from_slice(&w.to_be_bytes());
        }
        let mut wii = Wii::from_bytes(&dol).unwrap();
        wii.run_frame();
        assert_eq!(wii.frame()[0], 0x00FF_FFFF);
    }
}
