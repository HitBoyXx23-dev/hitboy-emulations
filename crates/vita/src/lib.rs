//! Clean-room PlayStation Vita emulator core.
//!
//! Parses `.vpk` packages (ZIP with `eboot.bin`, `sce_sys/param.sfo`, and `app0:` files)
//! and decrypted ARMv7 `.elf` / `.velf` executables. Runs on `cpuarm_v7` with a native
//! 960×544 OLED framebuffer (`SceDisplay`), `SceCtrl`, `SceTouch`, `SceAudioOut`, and `SceSysmem`.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use cpuarm_v7::{ArmCpu, Bus, StepEvent};

pub const WIDTH: usize = 960;
pub const HEIGHT: usize = 544;
pub const FPS: f64 = 59.94;

const RAM_BASE: u32 = 0x8100_0000;
const RAM_SIZE: usize = 64 * 1024 * 1024;
const FB_BASE: u32 = 0x8400_0000;
const STEPS_PER_FRAME: u32 = 150_000;

#[derive(Clone)]
pub struct LoadedVita {
    pub title: String,
    pub title_id: String,
    pub elf: Vec<u8>,
    pub files: BTreeMap<String, Vec<u8>>,
}

fn parse_sfo_title(data: &[u8]) -> (String, String) {
    if data.len() < 20 || &data[0..4] != b"\x00PSF" {
        return (String::new(), String::new());
    }
    let kt = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let dt = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let mut title = String::new();
    let mut tid = String::new();
    for i in 0..count.min(128) {
        let o = 20 + i * 16;
        if o + 16 > data.len() { break; }
        let ko = u16::from_le_bytes([data[o], data[o + 1]]) as usize;
        let len = u32::from_le_bytes(data[o + 4..o + 8].try_into().unwrap()) as usize;
        let doff = u32::from_le_bytes(data[o + 12..o + 16].try_into().unwrap()) as usize;
        if kt + ko < data.len() && dt + doff < data.len() {
            let ke = data[kt + ko..].iter().position(|&b| b == 0).map(|n| kt + ko + n).unwrap_or(data.len());
            let key = String::from_utf8_lossy(&data[kt + ko..ke]);
            let val = String::from_utf8_lossy(&data[dt + doff..(dt + doff + len).min(data.len())])
                .trim_end_matches('\0')
                .trim()
                .to_string();
            match key.as_ref() {
                "TITLE" | "STITLE" if title.is_empty() => title = val,
                "TITLE_ID" => tid = val,
                _ => {}
            }
        }
    }
    (title, tid)
}

pub fn load_vita_package(data: &[u8]) -> Result<LoadedVita, String> {
    if data.len() >= 4 && &data[0..4] == b"\x7FELF" {
        return Ok(LoadedVita { title: String::new(), title_id: String::new(), elf: data.to_vec(), files: BTreeMap::new() });
    }
    if data.len() >= 4 && &data[0..4] == b"SCE\0" {
        // Vita SELF container: extract embedded ELF at offset stored at +0x28 if unencrypted
        if data.len() >= 0x40 {
            let elf_off = u64::from_le_bytes(data[0x28..0x30].try_into().unwrap()) as usize;
            if elf_off < data.len() && data[elf_off..].starts_with(b"\x7FELF") {
                return Ok(LoadedVita { title: String::new(), title_id: String::new(), elf: data[elf_off..].to_vec(), files: BTreeMap::new() });
            }
        }
        return Err("Encrypted retail Vita SELF cannot be decrypted without console keys. Use a decrypted homebrew .vpk or .elf.".into());
    }
    if data.len() >= 4 && &data[0..2] == b"PK" {
        let mut z = zip::ZipArchive::new(Cursor::new(data)).map_err(|e| format!("Invalid .vpk archive: {e}"))?;
        let mut files = BTreeMap::new();
        for i in 0..z.len() {
            let mut f = z.by_index(i).map_err(|e| e.to_string())?;
            if f.is_dir() { continue; }
            let name = f.name().replace('\\', "/");
            if f.size() <= 32 * 1024 * 1024 {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
                files.insert(name, buf);
            }
        }
        let (title, title_id) = files
            .get("sce_sys/param.sfo")
            .map(|b| parse_sfo_title(b))
            .unwrap_or_default();
        let eboot = files
            .get("eboot.bin")
            .cloned()
            .ok_or_else(|| "VPK package does not contain eboot.bin.".to_string())?;
        let inner = load_vita_package(&eboot)?;
        return Ok(LoadedVita { title, title_id, elf: inner.elf, files });
    }
    Err("Unrecognized PlayStation Vita file (expected .vpk, decrypted .self, or ARMv7 .elf).".into())
}

pub struct VitaMem {
    pub ram: Vec<u8>,
    pub fb_addr: u32,
    pub fb_stride: u32,
    pub buttons: u32,
    pub touch_pressed: bool,
    pub touch_x: u16,
    pub touch_y: u16,
    pub audio_out: Vec<f32>,
    pub exited: bool,
    pub exit_code: i32,
}

impl VitaMem {
    pub fn new() -> Self {
        Self {
            ram: vec![0; RAM_SIZE],
            fb_addr: FB_BASE,
            fb_stride: WIDTH as u32,
            buttons: 0,
            touch_pressed: false,
            touch_x: 0,
            touch_y: 0,
            audio_out: Vec::new(),
            exited: false,
            exit_code: 0,
        }
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.write8(addr.wrapping_add(i as u32), b);
        }
    }

    pub fn handle_svc(&mut self, cpu: &mut ArmCpu, svc: u32) {
        match svc {
            // HLE sceDisplaySetFrameBuf (r0=fb_addr, r1=stride)
            0x10 => {
                if cpu.r[0] != 0 { self.fb_addr = cpu.r[0]; }
                if cpu.r[1] != 0 { self.fb_stride = cpu.r[1]; }
                cpu.r[0] = 0;
            }
            // HLE sceCtrlPeekBufferPositive (r0=out_ptr)
            0x11 => {
                let p = cpu.r[0];
                self.write32(p + 8, self.buttons);
                cpu.r[0] = 1;
            }
            // HLE sceTouchPeek (r0=out_ptr)
            0x12 => {
                let p = cpu.r[0];
                self.write32(p + 8, self.touch_pressed as u32);
                self.write16(p + 12, self.touch_x);
                self.write16(p + 14, self.touch_y);
                cpu.r[0] = 1;
            }
            // HLE sceKernelExitProcess
            0x01 => {
                self.exited = true;
                self.exit_code = cpu.r[0] as i32;
                cpu.r[0] = 0;
            }
            _ => cpu.r[0] = 0,
        }
    }

    pub fn scanout(&self, out: &mut [u32]) {
        let stride = self.fb_stride.max(WIDTH as u32) as usize;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let v = self.read32(self.fb_addr.wrapping_add(((y * stride + x) * 4) as u32));
                // ABGR8888 -> 0RGB
                let r = v & 0xFF;
                let g = (v >> 8) & 0xFF;
                let b = (v >> 16) & 0xFF;
                out[y * WIDTH + x] = (r << 16) | (g << 8) | b;
            }
        }
    }
}

impl Default for VitaMem {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for VitaMem {
    #[inline]
    fn read8(&self, addr: u32) -> u8 {
        if addr >= RAM_BASE && (addr - RAM_BASE) < RAM_SIZE as u32 {
            self.ram[(addr - RAM_BASE) as usize]
        } else {
            0
        }
    }
    #[inline]
    fn read16(&self, addr: u32) -> u16 {
        u16::from_le_bytes([self.read8(addr), self.read8(addr.wrapping_add(1))])
    }
    #[inline]
    fn read32(&self, addr: u32) -> u32 {
        if addr >= RAM_BASE && (addr - RAM_BASE + 4) <= RAM_SIZE as u32 {
            let i = (addr - RAM_BASE) as usize;
            u32::from_le_bytes([self.ram[i], self.ram[i + 1], self.ram[i + 2], self.ram[i + 3]])
        } else {
            0
        }
    }
    #[inline]
    fn write8(&mut self, addr: u32, val: u8) {
        if addr >= RAM_BASE && (addr - RAM_BASE) < RAM_SIZE as u32 {
            self.ram[(addr - RAM_BASE) as usize] = val;
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
        if addr >= RAM_BASE && (addr - RAM_BASE + 4) <= RAM_SIZE as u32 {
            let i = (addr - RAM_BASE) as usize;
            self.ram[i..i + 4].copy_from_slice(&val.to_le_bytes());
        }
    }
}

pub struct PsVita {
    pub cpu: ArmCpu,
    pub mem: VitaMem,
    pub title: String,
    pub title_id: String,
    loaded: LoadedVita,
    frame: Vec<u32>,
}

impl PsVita {
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let loaded = load_vita_package(data)?;
        let mut vita = Self {
            cpu: ArmCpu::new(),
            mem: VitaMem::new(),
            title: loaded.title.clone(),
            title_id: loaded.title_id.clone(),
            loaded,
            frame: vec![0; WIDTH * HEIGHT],
        };
        vita.boot()?;
        Ok(vita)
    }

    fn boot(&mut self) -> Result<(), String> {
        self.cpu = ArmCpu::new();
        self.mem = VitaMem::new();
        let elf = &self.loaded.elf;
        if elf.len() < 52 || &elf[0..4] != b"\x7FELF" {
            return Err("Invalid PlayStation Vita ELF executable.".into());
        }
        let e_machine = u16::from_le_bytes([elf[18], elf[19]]);
        if e_machine != 40 {
            return Err(format!("Unsupported Vita ELF machine {e_machine} (expected ARM = 40)."));
        }
        let entry = u32::from_le_bytes(elf[24..28].try_into().unwrap());
        let phoff = u32::from_le_bytes(elf[28..32].try_into().unwrap()) as usize;
        let phentsize = u16::from_le_bytes([elf[42], elf[43]]) as usize;
        let phnum = u16::from_le_bytes([elf[44], elf[45]]) as usize;
        for i in 0..phnum {
            let o = phoff + i * phentsize;
            if o + 32 > elf.len() { break; }
            if u32::from_le_bytes(elf[o..o + 4].try_into().unwrap()) == 1 {
                let foff = u32::from_le_bytes(elf[o + 4..o + 8].try_into().unwrap()) as usize;
                let vaddr = u32::from_le_bytes(elf[o + 8..o + 12].try_into().unwrap());
                let fsz = u32::from_le_bytes(elf[o + 16..o + 20].try_into().unwrap()) as usize;
                if foff + fsz <= elf.len() {
                    self.mem.write_slice(vaddr, &elf[foff..foff + fsz]);
                }
            }
        }
        self.cpu.set_pc(entry);
        self.cpu.r[13] = RAM_BASE + 0x02FF_0000;
        Ok(())
    }

    pub fn reset(&mut self) {
        let _ = self.boot();
        self.frame.fill(0);
    }

    pub fn set_buttons(&mut self, buttons: u16) {
        let mut b = 0u32;
        if buttons & (1 << 2) != 0 { b |= 0x0001; } // Select
        if buttons & (1 << 3) != 0 { b |= 0x0008; } // Start
        if buttons & (1 << 4) != 0 { b |= 0x0010; } // Up
        if buttons & (1 << 7) != 0 { b |= 0x0020; } // Right
        if buttons & (1 << 5) != 0 { b |= 0x0040; } // Down
        if buttons & (1 << 6) != 0 { b |= 0x0080; } // Left
        if buttons & (1 << 10) != 0 { b |= 0x0100; } // L
        if buttons & (1 << 11) != 0 { b |= 0x0200; } // R
        if buttons & (1 << 8) != 0 { b |= 0x1000; } // Triangle
        if buttons & (1 << 1) != 0 { b |= 0x2000; } // Circle
        if buttons & (1 << 0) != 0 { b |= 0x4000; } // Cross
        if buttons & (1 << 9) != 0 { b |= 0x8000; } // Square
        self.mem.buttons = b;
    }

    pub fn set_touch(&mut self, pressed: bool, nx: f32, ny: f32) {
        self.mem.touch_pressed = pressed;
        self.mem.touch_x = (nx * 1920.0).clamp(0.0, 1919.0) as u16;
        self.mem.touch_y = (ny * 1088.0).clamp(0.0, 1087.0) as u16;
    }

    pub fn run_frame(&mut self) {
        if self.mem.exited {
            return;
        }
        for _ in 0..STEPS_PER_FRAME {
            match self.cpu.step(&mut self.mem) {
                StepEvent::Continue => {}
                StepEvent::Svc(id) => {
                    self.mem.handle_svc(&mut self.cpu, id);
                    if self.mem.exited {
                        break;
                    }
                }
                StepEvent::Bkpt(_) => {
                    self.mem.exited = true;
                    break;
                }
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
    use std::io::Write;

    #[test]
    fn vita_vpk_loads_draws_oled_and_handles_touch() {
        // Minimal ARMv7 ELF at 0x8100_0000: write ABGR8888 cyan (R=0,G=FF,B=FF -> 0xFFFF_FF00) to FB_BASE 0x8400_0000, exit
        let words: [u32; 5] = [
            0xE3A0_0484, // mov r0, #0x84000000
            0xE30F_1F00, // movw r1, #0xFF00
            0xE34F_1FFF, // movt r1, #0xFFFF
            0xE580_1000, // str r1, [r0]
            0xEF00_0001, // svc #1 (exit)
        ];
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut elf = vec![0u8; 0x100];
        elf[0..4].copy_from_slice(b"\x7FELF");
        elf[4] = 1; elf[5] = 1; elf[6] = 1;
        elf[16..18].copy_from_slice(&2u16.to_le_bytes());
        elf[18..20].copy_from_slice(&40u16.to_le_bytes()); // EM_ARM
        elf[24..28].copy_from_slice(&0x8100_0000u32.to_le_bytes());
        elf[28..32].copy_from_slice(&52u32.to_le_bytes());
        elf[42..44].copy_from_slice(&32u16.to_le_bytes());
        elf[44..46].copy_from_slice(&1u16.to_le_bytes());
        elf[52..56].copy_from_slice(&1u32.to_le_bytes());
        elf[56..60].copy_from_slice(&0x100u32.to_le_bytes());
        elf[60..64].copy_from_slice(&0x8100_0000u32.to_le_bytes());
        elf[64..68].copy_from_slice(&0x8100_0000u32.to_le_bytes());
        elf[68..72].copy_from_slice(&(code.len() as u32).to_le_bytes());
        elf[72..76].copy_from_slice(&(code.len() as u32).to_le_bytes());
        elf.extend_from_slice(&code);

        // Wrap into a .vpk ZIP
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut cursor);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("eboot.bin", opts).unwrap();
            zw.write_all(&elf).unwrap();
            zw.finish().unwrap();
        }
        let vpk = cursor.into_inner();
        let mut vita = PsVita::from_bytes(&vpk).unwrap();
        vita.set_touch(true, 0.5, 0.5);
        assert!(vita.mem.touch_pressed);
        vita.run_frame();
        assert_eq!(vita.frame()[0], 0x0000_FFFF);
        assert!(vita.mem.exited);
    }
}
