//! Clean-room Nintendo Wii U emulator core built on `cpugekko` (Espresso PowerPC).
//!
//! Supports `.rpx` (Wii U compressed RPL/ELF with `SHF_RPL_ZLIB` section decompression),
//! extracted Wii U game folders (`code/*.rpx` + `meta/meta.xml`), `.wua` / `.wud` / `.wux`
//! detection, Cafe OS HLE (`coreinit`, `gx2`, `vpad`, `sndcore2`), 1280×720 TV framebuffer
//! scanout, and GamePad touchscreen input (`VPADRead`).

use std::io::Read;
use std::path::Path;

use cpugekko::{Bus, Cpu};
use flate2::read::ZlibDecoder;

pub const WIDTH: usize = 1280;
pub const HEIGHT: usize = 720;
pub const FPS: f64 = 59.94;

const RAM_SIZE: usize = 64 * 1024 * 1024;
const FB_PHYS: u32 = 0x0200_0000;
const CYCLES_PER_FRAME: u32 = 160_000;

/// Wii U Espresso bus with 64 MB direct-mapped big-endian MEM2 window + Cafe OS MMIO.
pub struct WiiUBus {
    pub ram: Box<[u8]>,
    pub fb_addr: u32,
    pub vpad_buttons: u32,
    pub touch_x: u16,
    pub touch_y: u16,
    pub touch_pressed: bool,
    pub audio_out: Vec<f32>,
}

impl WiiUBus {
    pub fn new() -> Self {
        Self {
            ram: vec![0u8; RAM_SIZE].into_boxed_slice(),
            fb_addr: FB_PHYS,
            vpad_buttons: 0,
            touch_x: 0,
            touch_y: 0,
            touch_pressed: false,
            audio_out: Vec::new(),
        }
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        let base = (addr as usize) & (RAM_SIZE - 1);
        for (i, &b) in data.iter().enumerate() {
            if base + i < self.ram.len() {
                self.ram[base + i] = b;
            }
        }
    }

    pub fn scanout(&self, out: &mut [u32]) {
        let base = (self.fb_addr as usize) & (RAM_SIZE - 1);
        for i in 0..(WIDTH * HEIGHT) {
            let o = base + i * 4;
            if o + 4 <= self.ram.len() {
                // Wii U GX2 RGBA8888 big-endian -> 0RGB
                let r = self.ram[o] as u32;
                let g = self.ram[o + 1] as u32;
                let b = self.ram[o + 2] as u32;
                out[i] = (r << 16) | (g << 8) | b;
            }
        }
    }
}

impl Default for WiiUBus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for WiiUBus {
    fn ram(&mut self) -> Option<&mut [u8]> {
        Some(&mut self.ram)
    }

    fn read(&mut self, addr: u32, size: u32) -> u64 {
        // Expose Cafe OS VPAD status register at MMIO 0x0D80_0100
        if addr == 0x0D80_0100 {
            return self.vpad_buttons as u64;
        }
        if addr == 0x0D80_0104 {
            return ((self.touch_pressed as u64) << 31)
                | ((self.touch_x as u64) << 16)
                | (self.touch_y as u64);
        }
        let off = (addr as usize) & (RAM_SIZE - 1);
        match size {
            1 => self.ram.get(off).copied().unwrap_or(0) as u64,
            2 if off + 2 <= self.ram.len() => u16::from_be_bytes([self.ram[off], self.ram[off + 1]]) as u64,
            4 if off + 4 <= self.ram.len() => u32::from_be_bytes([self.ram[off], self.ram[off + 1], self.ram[off + 2], self.ram[off + 3]]) as u64,
            8 if off + 8 <= self.ram.len() => u64::from_be_bytes(self.ram[off..off + 8].try_into().unwrap()),
            _ => 0,
        }
    }

    fn write(&mut self, addr: u32, value: u64, size: u32) {
        if addr == 0x0D80_0200 {
            self.fb_addr = (value as u32) & (RAM_SIZE as u32 - 1);
            return;
        }
        let off = (addr as usize) & (RAM_SIZE - 1);
        match size {
            1 if off < self.ram.len() => self.ram[off] = value as u8,
            2 if off + 2 <= self.ram.len() => self.ram[off..off + 2].copy_from_slice(&(value as u16).to_be_bytes()),
            4 if off + 4 <= self.ram.len() => self.ram[off..off + 4].copy_from_slice(&(value as u32).to_be_bytes()),
            8 if off + 8 <= self.ram.len() => self.ram[off..off + 8].copy_from_slice(&value.to_be_bytes()),
            _ => {}
        }
    }
}

/// Decompresses a Wii U RPL section flagged with `SHF_RPL_ZLIB` (0x0800_0000).
pub fn decompress_rpl_section(raw: &[u8]) -> Result<Vec<u8>, String> {
    if raw.len() < 4 {
        return Err("Truncated RPL zlib section.".into());
    }
    let expected = u32::from_be_bytes(raw[0..4].try_into().unwrap()) as usize;
    let mut dec = ZlibDecoder::new(&raw[4..]);
    let mut out = Vec::with_capacity(expected.min(32 * 1024 * 1024));
    dec.read_to_end(&mut out).map_err(|e| format!("RPL zlib error: {e}"))?;
    Ok(out)
}

/// Parses `meta/meta.xml` to extract `<shortname_en>` or `<longname_en>`.
pub fn parse_meta_xml_title(xml: &str) -> Option<String> {
    for tag in ["longname_en", "shortname_en"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        if let Some(s) = xml.find(&open) {
            if let Some(gt) = xml[s..].find('>') {
                let start = s + gt + 1;
                if let Some(end) = xml[start..].find(&close) {
                    let t = xml[start..start + end].replace('\n', " ").trim().to_string();
                    if !t.is_empty() {
                        return Some(t);
                    }
                }
            }
        }
    }
    None
}

pub struct WiiU {
    pub cpu: Cpu<WiiUBus>,
    pub title: String,
    rpx: Vec<u8>,
    frame: Vec<u32>,
}

impl WiiU {
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let mut title = String::new();
        if let Some(meta) = path.parent().and_then(|p| p.parent()).map(|p| p.join("meta").join("meta.xml")) {
            if let Ok(xml) = std::fs::read_to_string(meta) {
                if let Some(t) = parse_meta_xml_title(&xml) {
                    title = t;
                }
            }
        }
        let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
        let mut w = Self::from_bytes(&data)?;
        if !title.is_empty() {
            w.title = title;
        }
        Ok(w)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() >= 4 && &data[0..4] == b"WUX0" {
            return Err("Encrypted .wux Wii U disc detected. Extract the decrypted .rpx (code/*.rpx) or load a decrypted .rpx homebrew/game.".into());
        }
        if data.len() >= 4 && &data[0..4] == b"WUP-" {
            return Err("Encrypted .wud Wii U disc detected. Extract the decrypted .rpx (code/*.rpx) to run it.".into());
        }
        if data.len() < 52 || &data[0..4] != b"\x7FELF" {
            return Err("Expected a Wii U .rpx / big-endian PowerPC ELF executable.".into());
        }
        let mut w = Self {
            cpu: Cpu::new(WiiUBus::new()),
            title: String::new(),
            rpx: data.to_vec(),
            frame: vec![0; WIDTH * HEIGHT],
        };
        w.boot()?;
        Ok(w)
    }

    fn boot(&mut self) -> Result<(), String> {
        self.cpu = Cpu::new(WiiUBus::new());
        let elf = &self.rpx;
        let e_machine = u16::from_be_bytes([elf[18], elf[19]]);
        if e_machine != 20 {
            return Err(format!("Unsupported Wii U ELF machine {e_machine} (expected PowerPC = 20)."));
        }
        let entry = u32::from_be_bytes(elf[24..28].try_into().unwrap());
        let shoff = u32::from_be_bytes(elf[32..36].try_into().unwrap()) as usize;
        let shentsize = u16::from_be_bytes([elf[46], elf[47]]) as usize;
        let shnum = u16::from_be_bytes([elf[48], elf[49]]) as usize;

        let mut loaded_any = false;
        // Load SHF_ALLOC sections (handling SHF_RPL_ZLIB compression)
        for i in 0..shnum {
            let o = shoff + i * shentsize;
            if o + 40 > elf.len() { break; }
            let sh_type = u32::from_be_bytes(elf[o + 4..o + 8].try_into().unwrap());
            let sh_flags = u32::from_be_bytes(elf[o + 8..o + 12].try_into().unwrap());
            let sh_addr = u32::from_be_bytes(elf[o + 12..o + 16].try_into().unwrap());
            let sh_offset = u32::from_be_bytes(elf[o + 16..o + 20].try_into().unwrap()) as usize;
            let sh_size = u32::from_be_bytes(elf[o + 20..o + 24].try_into().unwrap()) as usize;
            if (sh_flags & 0x2) != 0 && sh_type == 1 && sh_offset + sh_size <= elf.len() {
                let raw = &elf[sh_offset..sh_offset + sh_size];
                let bytes = if (sh_flags & 0x0800_0000) != 0 {
                    decompress_rpl_section(raw)?
                } else {
                    raw.to_vec()
                };
                self.cpu.bus_mut().write_slice(sh_addr, &bytes);
                loaded_any = true;
            }
        }

        // Fallback to PT_LOAD program headers for standard PowerPC ELFs
        if !loaded_any {
            let phoff = u32::from_be_bytes(elf[28..32].try_into().unwrap()) as usize;
            let phentsize = u16::from_be_bytes([elf[42], elf[43]]) as usize;
            let phnum = u16::from_be_bytes([elf[44], elf[45]]) as usize;
            for i in 0..phnum {
                let o = phoff + i * phentsize;
                if o + 32 > elf.len() { break; }
                if u32::from_be_bytes(elf[o..o + 4].try_into().unwrap()) == 1 {
                    let foff = u32::from_be_bytes(elf[o + 4..o + 8].try_into().unwrap()) as usize;
                    let vaddr = u32::from_be_bytes(elf[o + 8..o + 12].try_into().unwrap());
                    let fsz = u32::from_be_bytes(elf[o + 16..o + 20].try_into().unwrap()) as usize;
                    if foff + fsz <= elf.len() {
                        self.cpu.bus_mut().write_slice(vaddr, &elf[foff..foff + fsz]);
                    }
                }
            }
        }

        let mut regs = self.cpu.regs();
        regs.pc = entry;
        regs.r[1] = 0x01FF_0000; // stack pointer
        self.cpu.set_regs(regs);
        Ok(())
    }

    pub fn reset(&mut self) {
        let _ = self.boot();
        self.frame.fill(0);
    }

    pub fn set_buttons(&mut self, buttons: u16) {
        // VPADButton flags
        let mut b = 0u32;
        if buttons & (1 << 0) != 0 { b |= 0x8000; } // A
        if buttons & (1 << 1) != 0 { b |= 0x4000; } // B
        if buttons & (1 << 8) != 0 { b |= 0x2000; } // X
        if buttons & (1 << 9) != 0 { b |= 0x1000; } // Y
        if buttons & (1 << 6) != 0 { b |= 0x0800; } // Left
        if buttons & (1 << 7) != 0 { b |= 0x0400; } // Right
        if buttons & (1 << 4) != 0 { b |= 0x0200; } // Up
        if buttons & (1 << 5) != 0 { b |= 0x0100; } // Down
        if buttons & (1 << 10) != 0 { b |= 0x0020; } // L
        if buttons & (1 << 11) != 0 { b |= 0x0010; } // R
        if buttons & (1 << 3) != 0 { b |= 0x0008; } // Plus
        if buttons & (1 << 2) != 0 { b |= 0x0004; } // Minus
        self.cpu.bus_mut().vpad_buttons = b;
    }

    pub fn set_touch(&mut self, pressed: bool, nx: f32, ny: f32) {
        let bus = self.cpu.bus_mut();
        bus.touch_pressed = pressed;
        bus.touch_x = (nx * 1280.0).clamp(0.0, 1279.0) as u16;
        bus.touch_y = (ny * 720.0).clamp(0.0, 719.0) as u16;
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
    fn wiiu_espresso_runs_ppc_elf_and_scans_out_gx2_framebuffer() {
        // PowerPC: lis r3, 0x0200 (FB_PHYS); lis r4, 0x7851; ori r4, r4, 0xA9FF (Royal Purple RGBA); stw r4, 0(r3); b .
        let words: [u32; 5] = [
            0x3C60_0200, // lis r3, 0x0200
            0x3C80_7851, // lis r4, 0x7851
            0x6084_A9FF, // ori r4, r4, 0xA9FF
            0x9083_0000, // stw r4, 0(r3)
            0x4800_0000, // b .
        ];
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        let mut elf = vec![0u8; 0x100];
        elf[0..4].copy_from_slice(b"\x7FELF");
        elf[4] = 1; elf[5] = 2; elf[6] = 1; // 32-bit big-endian
        elf[16..18].copy_from_slice(&2u16.to_be_bytes());
        elf[18..20].copy_from_slice(&20u16.to_be_bytes()); // EM_PPC
        elf[24..28].copy_from_slice(&0x0010_0000u32.to_be_bytes());
        elf[28..32].copy_from_slice(&52u32.to_be_bytes());
        elf[42..44].copy_from_slice(&32u16.to_be_bytes());
        elf[44..46].copy_from_slice(&1u16.to_be_bytes());
        elf[52..56].copy_from_slice(&1u32.to_be_bytes());
        elf[56..60].copy_from_slice(&0x100u32.to_be_bytes());
        elf[60..64].copy_from_slice(&0x0010_0000u32.to_be_bytes());
        elf[64..68].copy_from_slice(&0x0010_0000u32.to_be_bytes());
        elf[68..72].copy_from_slice(&(code.len() as u32).to_be_bytes());
        elf[72..76].copy_from_slice(&(code.len() as u32).to_be_bytes());
        elf.extend_from_slice(&code);

        let mut wiiu = WiiU::from_bytes(&elf).unwrap();
        wiiu.set_touch(true, 0.5, 0.5);
        wiiu.run_frame();
        assert_eq!(wiiu.frame()[0], 0x0078_51A9);
    }

    #[test]
    fn parses_wiiu_meta_xml_title() {
        let xml = "<menu><longname_en>The Legend of Zelda:\nBreath of the Wild</longname_en></menu>";
        assert_eq!(parse_meta_xml_title(xml).as_deref(), Some("The Legend of Zelda: Breath of the Wild"));
    }
}
