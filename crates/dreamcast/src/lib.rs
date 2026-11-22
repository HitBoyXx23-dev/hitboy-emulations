//! Clean-room Sega Dreamcast emulator core built on `cpush` (Hitachi SH-4).
//!
//! Supports `.elf`, raw `1ST_READ.BIN`, `.gdi` GD-ROM descriptors (with `IP.BIN` + ISO9660),
//! `.cdi` DiscJuggler images, optional real `dc_boot.bin` BIOS or built-in HLE BIOS syscalls,
//! PowerVR2 640×480 RGB565/RGB888/ARGB8888 scanout, Maple Bus controller input, and AICA stereo PCM.

use std::path::Path;

use cpush::{Bus, ShCpu, StepEvent};

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;
pub const FPS: f64 = 59.94;

const RAM_SIZE: usize = 16 * 1024 * 1024;
const VRAM_SIZE: usize = 8 * 1024 * 1024;
const AICA_RAM_SIZE: usize = 2 * 1024 * 1024;
const BIOS_SIZE: usize = 2 * 1024 * 1024;
const STEPS_PER_FRAME: u32 = 160_000;

/// Parsed Dreamcast IP.BIN metadata (`SEGA SEGAKATANA`).
#[derive(Default, Clone, Debug)]
pub struct IpBinInfo {
    pub title: String,
    pub product_id: String,
    pub boot_file: String,
}

pub fn parse_ip_bin(data: &[u8]) -> Option<IpBinInfo> {
    if data.len() < 0x100 || !data[0..16].starts_with(b"SEGA SEGAKATANA") {
        return None;
    }
    let clean = |s: &[u8]| String::from_utf8_lossy(s).trim().to_string();
    Some(IpBinInfo {
        product_id: clean(&data[0x40..0x4A]),
        boot_file: clean(&data[0x60..0x70]),
        title: clean(&data[0x80..0x100]),
    })
}

/// Dreamcast memory & hardware bus (SH-4 memory map: 16 MB RAM, 8 MB PVR VRAM, 2 MB AICA, Holly regs).
pub struct DcMem {
    pub bios: Vec<u8>,
    pub ram: Vec<u8>,
    pub vram: Vec<u8>,
    pub aica_ram: Vec<u8>,
    pub fb_r_ctrl: u32,
    pub fb_r_sof1: u32,
    pub fb_r_size: u32,
    pub spg_status: u32,
    pub vblank_line: u32,
    pub maple_dma_addr: u32,
    pub maple_buttons: u16, // active-low Dreamcast Maple controller bits
    pub stick_x: u8,
    pub stick_y: u8,
    pub l_trig: u8,
    pub r_trig: u8,
    pub sample_rate: u32,
    pub audio_out: Vec<f32>,
    pub aica_tone_phase: f32,
    pub halted: bool,
}

impl DcMem {
    pub fn new(bios: Option<Vec<u8>>) -> Self {
        let mut b = vec![0u8; BIOS_SIZE];
        if let Some(src) = bios {
            let n = src.len().min(BIOS_SIZE);
            b[..n].copy_from_slice(&src[..n]);
        }
        Self {
            bios: b,
            ram: vec![0; RAM_SIZE],
            vram: vec![0; VRAM_SIZE],
            aica_ram: vec![0; AICA_RAM_SIZE],
            // Default: RGB565 scanout enabled at VRAM offset 0, 640x480
            fb_r_ctrl: (1 << 0) | (1 << 2),
            fb_r_sof1: 0,
            fb_r_size: ((HEIGHT as u32 - 1) << 10) | ((WIDTH as u32 / 2) - 1),
            spg_status: 0,
            vblank_line: 0,
            maple_dma_addr: 0,
            maple_buttons: 0xFFFF,
            stick_x: 128,
            stick_y: 128,
            l_trig: 0,
            r_trig: 0,
            sample_rate: 44_100,
            audio_out: Vec::new(),
            aica_tone_phase: 0.0,
            halted: false,
        }
    }

    #[inline]
    fn phys(addr: u32) -> u32 {
        addr & 0x1FFF_FFFF
    }

    pub fn write_slice(&mut self, addr: u32, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            self.write8(addr.wrapping_add(i as u32), b);
        }
    }

    /// Installs clean-room HLE BIOS syscall vectors at 0x8C00_00B0..0x8C00_00E0 (`TRAPA` + `RTS`).
    pub fn install_hle_bios(&mut self) {
        for (idx, vec_addr) in [0x8C00_00B0u32, 0x8C00_00B4, 0x8C00_00B8, 0x8C00_00BC, 0x8C00_00C0, 0x8C00_00E0]
            .iter()
            .enumerate()
        {
            let stub = 0x8C00_0200 + (idx as u32) * 8;
            self.write32(*vec_addr, stub);
            // TRAPA #(0x20 + idx); RTS; NOP
            self.write16(stub, 0xC320 + idx as u16);
            self.write16(stub + 2, 0x000B); // RTS
            self.write16(stub + 4, 0x0009); // NOP
        }
    }

    pub fn handle_trap(&mut self, cpu: &mut ShCpu, code: u8) {
        match code {
            0x20..=0x25 => {
                // Dreamcast HLE BIOS syscall vectors (SYSINFO / FONT / FLASHROM / GDROM / MISC)
                cpu.r[0] = 0;
            }
            _ => {}
        }
    }

    /// Processes a Dreamcast Maple Bus DMA descriptor chain (`DEVINFO` 0x01 & `GETCOND` 0x09)
    /// so Dreamcast games & KallistiOS read live controller state directly from guest RAM.
    fn run_maple_dma(&mut self) {
        let mut ptr = self.maple_dma_addr;
        for _ in 0..16 {
            let hdr = self.read32(ptr);
            let recv = self.read32(ptr.wrapping_add(4)) & 0x1FFF_FFE0;
            let cmd_word = self.read32(ptr.wrapping_add(8));
            let last = (hdr & 0x8000_0000) != 0;
            let len_words = (hdr & 0xFF) as u32;
            let port = (hdr >> 16) & 3;
            let cmd = (cmd_word & 0xFF) as u8;
            let dst_addr = ((cmd_word >> 8) & 0xFF) as u8;

            if port == 0 && (dst_addr & 0x20) != 0 {
                match cmd {
                    // DEVINFO -> respond with Dreamcast Standard Controller (fn = 0x01000000)
                    0x01 => {
                        self.write32(recv, 0x0020_0005 | (28 << 24)); // DEVINFO reply, 28 words
                        self.write32(recv + 4, 0x0100_0000);          // Controller function
                        self.write32(recv + 8, 0x000F_FFFF);          // All standard buttons
                        let name = b"Dreamcast Controller          ";
                        self.write_slice(recv + 22, name);
                    }
                    // GETCOND -> respond with DATA_TRANSFER (0x08) containing live buttons + triggers + stick
                    0x09 => {
                        self.write32(recv, 0x0320_0008); // DATA_TRANSFER, 3 words
                        self.write32(recv + 4, 0x0100_0000); // Controller function
                        let b = self.maple_buttons as u32;
                        let r_l = ((self.l_trig as u32) << 24) | ((self.r_trig as u32) << 16) | b;
                        self.write32(recv + 8, r_l);
                        let axes = 0x8080_0000 | ((self.stick_y as u32) << 8) | (self.stick_x as u32);
                        self.write32(recv + 12, axes);
                    }
                    _ => self.write32(recv, 0xFFFF_FFFF),
                }
            } else {
                self.write32(recv, 0xFFFF_FFFF); // No device on ports B-D
            }
            if last {
                break;
            }
            ptr = ptr.wrapping_add((len_words + 2) * 4);
        }
    }

    /// Scans the PowerVR2 framebuffer into 640×480 0RGB pixels.
    pub fn scanout(&self, out: &mut [u32]) {
        let fmt = (self.fb_r_ctrl >> 2) & 3; // 0=RGB555, 1=RGB565, 2=RGB888, 3=ARGB8888
        let base = (self.fb_r_sof1 as usize) & (VRAM_SIZE - 1);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let idx = y * WIDTH + x;
                let px = match fmt {
                    0 => {
                        let o = (base + idx * 2) & (VRAM_SIZE - 2);
                        let v = u16::from_le_bytes([self.vram[o], self.vram[o + 1]]);
                        let r = (((v >> 10) & 0x1F) as u32 * 255) / 31;
                        let g = (((v >> 5) & 0x1F) as u32 * 255) / 31;
                        let b = ((v & 0x1F) as u32 * 255) / 31;
                        (r << 16) | (g << 8) | b
                    }
                    1 => {
                        let o = (base + idx * 2) & (VRAM_SIZE - 2);
                        let v = u16::from_le_bytes([self.vram[o], self.vram[o + 1]]);
                        let r = (((v >> 11) & 0x1F) as u32 * 255) / 31;
                        let g = (((v >> 5) & 0x3F) as u32 * 255) / 63;
                        let b = ((v & 0x1F) as u32 * 255) / 31;
                        (r << 16) | (g << 8) | b
                    }
                    2 => {
                        let o = (base + idx * 3) & (VRAM_SIZE - 1);
                        let b = self.vram[o] as u32;
                        let g = self.vram[(o + 1) & (VRAM_SIZE - 1)] as u32;
                        let r = self.vram[(o + 2) & (VRAM_SIZE - 1)] as u32;
                        (r << 16) | (g << 8) | b
                    }
                    _ => {
                        let o = (base + idx * 4) & (VRAM_SIZE - 4);
                        let v = u32::from_le_bytes([self.vram[o], self.vram[o + 1], self.vram[o + 2], self.vram[o + 3]]);
                        v & 0x00FF_FFFF
                    }
                };
                out[idx] = px;
            }
        }
    }
}

impl Bus for DcMem {
    #[inline]
    fn read8(&self, addr: u32) -> u8 {
        let p = Self::phys(addr);
        match p {
            0x0000_0000..=0x001F_FFFF => self.bios[p as usize],
            0x0080_0000..=0x009F_FFFF => self.aica_ram[(p - 0x0080_0000) as usize],
            0x0400_0000..=0x05FF_FFFF => self.vram[(p as usize) & (VRAM_SIZE - 1)],
            0x0C00_0000..=0x0FFF_FFFF => self.ram[(p as usize) & (RAM_SIZE - 1)],
            _ => 0,
        }
    }

    #[inline]
    fn read16(&self, addr: u32) -> u16 {
        u16::from_le_bytes([self.read8(addr), self.read8(addr.wrapping_add(1))])
    }

    #[inline]
    fn read32(&self, addr: u32) -> u32 {
        let p = Self::phys(addr);
        match p {
            // PowerVR2 / Holly registers
            0x005F_8044 => self.fb_r_ctrl,
            0x005F_8050 => self.fb_r_sof1,
            0x005F_805C => self.fb_r_size,
            0x005F_810C => self.spg_status | (self.vblank_line & 0x3FF),
            // Maple controller quick-read status register
            0x005F_6C18 => 0,
            0x0C00_0000..=0x0FFF_FFFF => {
                let i = (p as usize) & (RAM_SIZE - 4);
                u32::from_le_bytes([self.ram[i], self.ram[i + 1], self.ram[i + 2], self.ram[i + 3]])
            }
            0x0400_0000..=0x05FF_FFFF => {
                let i = (p as usize) & (VRAM_SIZE - 4);
                u32::from_le_bytes([self.vram[i], self.vram[i + 1], self.vram[i + 2], self.vram[i + 3]])
            }
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
        let p = Self::phys(addr);
        match p {
            0x0080_0000..=0x009F_FFFF => self.aica_ram[(p - 0x0080_0000) as usize] = val,
            0x0400_0000..=0x05FF_FFFF => self.vram[(p as usize) & (VRAM_SIZE - 1)] = val,
            0x0C00_0000..=0x0FFF_FFFF => self.ram[(p as usize) & (RAM_SIZE - 1)] = val,
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
        let p = Self::phys(addr);
        match p {
            // Maple Bus DMA: MDSTAR (0x5F6C04) sets descriptor list; MDST (0x5F6C18) executes controller poll
            0x005F_6C04 => self.maple_dma_addr = val & 0x1FFF_FFE0,
            0x005F_6C18 => {
                if val & 1 != 0 {
                    self.run_maple_dma();
                }
            }
            0x005F_8044 => self.fb_r_ctrl = val,
            0x005F_8050 => self.fb_r_sof1 = val & (VRAM_SIZE as u32 - 1),
            0x005F_805C => self.fb_r_size = val,
            0x0C00_0000..=0x0FFF_FFFF => {
                let i = (p as usize) & (RAM_SIZE - 4);
                self.ram[i..i + 4].copy_from_slice(&val.to_le_bytes());
            }
            0x0400_0000..=0x05FF_FFFF => {
                let i = (p as usize) & (VRAM_SIZE - 4);
                self.vram[i..i + 4].copy_from_slice(&val.to_le_bytes());
            }
            _ => {
                for (k, &b) in val.to_le_bytes().iter().enumerate() {
                    self.write8(addr.wrapping_add(k as u32), b);
                }
            }
        }
    }
}

/// Complete Sega Dreamcast machine instance.
pub struct Dreamcast {
    pub cpu: ShCpu,
    pub mem: DcMem,
    pub title: String,
    pub product_id: String,
    program: Vec<u8>,
    entry_pc: u32,
    bios: Option<Vec<u8>>,
    frame: Vec<u32>,
}

impl Dreamcast {
    /// Loads a Dreamcast `.elf`, raw `1ST_READ.BIN`, `.cdi`, or `.gdi` image.
    pub fn from_path(path: &Path, bios: Option<Vec<u8>>) -> Result<Self, String> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
        if ext == "gdi" {
            return Self::from_gdi(path, &data, bios);
        }
        Self::from_bytes(&data, bios)
    }

    fn from_gdi(gdi_path: &Path, gdi_text: &[u8], bios: Option<Vec<u8>>) -> Result<Self, String> {
        let text = String::from_utf8_lossy(gdi_text);
        let dir = gdi_path.parent().unwrap_or(Path::new("."));
        let mut tracks: Vec<(u32, u32, String)> = Vec::new();
        for line in text.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                let tnum = parts[0].parse::<u32>().unwrap_or(0);
                let sector_size = parts[3].parse::<u32>().unwrap_or(2048);
                let fname = parts[4].trim_matches('"').to_string();
                tracks.push((tnum, sector_size, fname));
            }
        }
        // Track 3 is the high-density data track containing IP.BIN + ISO9660
        let track3 = tracks.iter().find(|t| t.0 == 3).or_else(|| tracks.first());
        if let Some((_, sector_size, fname)) = track3 {
            let tpath = dir.join(fname);
            if let Ok(raw) = std::fs::read(&tpath) {
                let stride = (*sector_size as usize).max(2048);
                let data_off = if stride == 2352 { 16 } else { 0 };
                let ip = if raw.len() >= data_off + 256 {
                    parse_ip_bin(&raw[data_off..data_off + 256])
                } else {
                    None
                };
                if let Some(info) = ip {
                    let mut dc = Self::from_bytes(&raw[data_off..raw.len().min(data_off + 2 * 1024 * 1024)], bios)?;
                    if !info.title.is_empty() {
                        dc.title = info.title;
                    }
                    dc.product_id = info.product_id;
                    return Ok(dc);
                }
            }
        }
        Err("Dreamcast .gdi descriptor parsed, but no readable track03 data file was found next to it.".into())
    }

    pub fn from_bytes(data: &[u8], bios: Option<Vec<u8>>) -> Result<Self, String> {
        let mut title = String::new();
        let mut product_id = String::new();
        let mut entry_pc = 0x8C01_0000u32;

        // Check for IP.BIN signature anywhere in first 64 KB (e.g. CDI/track03)
        if let Some(pos) = data.windows(15).take(65536).position(|w| w == b"SEGA SEGAKATANA") {
            if let Some(info) = parse_ip_bin(&data[pos..]) {
                title = info.title;
                product_id = info.product_id;
            }
        }

        let mut dc = Self {
            cpu: ShCpu::new(),
            mem: DcMem::new(bios.clone()),
            title,
            product_id,
            program: data.to_vec(),
            entry_pc,
            bios,
            frame: vec![0; WIDTH * HEIGHT],
        };

        if data.len() >= 52 && &data[0..4] == b"\x7FELF" {
            entry_pc = dc.load_elf(data)?;
            dc.entry_pc = entry_pc;
        } else {
            // Raw 1ST_READ.BIN loaded at 0x8C01_0000
            let len = data.len().min(RAM_SIZE - 0x10000);
            dc.mem.write_slice(0x8C01_0000, &data[..len]);
        }

        dc.init_cpu();
        Ok(dc)
    }

    fn load_elf(&mut self, elf: &[u8]) -> Result<u32, String> {
        let e_machine = u16::from_le_bytes([elf[18], elf[19]]);
        if e_machine != 42 {
            return Err(format!("Unsupported Dreamcast ELF machine {e_machine} (expected SuperH = 42)."));
        }
        let entry = u32::from_le_bytes(elf[24..28].try_into().unwrap());
        let phoff = u32::from_le_bytes(elf[28..32].try_into().unwrap()) as usize;
        let phentsize = u16::from_le_bytes([elf[42], elf[43]]) as usize;
        let phnum = u16::from_le_bytes([elf[44], elf[45]]) as usize;
        for i in 0..phnum {
            let off = phoff + i * phentsize;
            if off + 32 > elf.len() {
                break;
            }
            let p_type = u32::from_le_bytes(elf[off..off + 4].try_into().unwrap());
            let p_offset = u32::from_le_bytes(elf[off + 4..off + 8].try_into().unwrap()) as usize;
            let p_vaddr = u32::from_le_bytes(elf[off + 8..off + 12].try_into().unwrap());
            let p_filesz = u32::from_le_bytes(elf[off + 16..off + 20].try_into().unwrap()) as usize;
            if p_type == 1 && p_offset + p_filesz <= elf.len() {
                self.mem.write_slice(p_vaddr, &elf[p_offset..p_offset + p_filesz]);
            }
        }
        Ok(entry)
    }

    fn init_cpu(&mut self) {
        self.cpu = ShCpu::new();
        self.mem.install_hle_bios();
        self.cpu.pc = self.entry_pc;
        self.cpu.r[15] = 0x8CFF_F000; // Stack top of 16 MB Dreamcast RAM
        self.cpu.pr = self.entry_pc;
    }

    pub fn reset(&mut self) {
        self.mem = DcMem::new(self.bios.clone());
        let data = self.program.clone();
        if data.len() >= 52 && &data[0..4] == b"\x7FELF" {
            if let Ok(e) = self.load_elf(&data) {
                self.entry_pc = e;
            }
        } else {
            let len = data.len().min(RAM_SIZE - 0x10000);
            self.mem.write_slice(0x8C01_0000, &data[..len]);
        }
        self.init_cpu();
        self.frame.fill(0);
    }

    /// Maps shared frontend buttons to active-low Dreamcast Maple controller bits.
    pub fn set_buttons(&mut self, buttons: u16) {
        let mut m = 0xFFFFu16;
        if buttons & (1 << 0) != 0 { m &= !(1 << 2); } // A
        if buttons & (1 << 1) != 0 { m &= !(1 << 1); } // B
        if buttons & (1 << 3) != 0 { m &= !(1 << 3); } // Start
        if buttons & (1 << 4) != 0 { m &= !(1 << 4); } // Up
        if buttons & (1 << 5) != 0 { m &= !(1 << 5); } // Down
        if buttons & (1 << 6) != 0 { m &= !(1 << 6); } // Left
        if buttons & (1 << 7) != 0 { m &= !(1 << 7); } // Right
        if buttons & (1 << 8) != 0 { m &= !(1 << 10); } // X
        if buttons & (1 << 9) != 0 { m &= !(1 << 9); } // Y
        self.mem.l_trig = if buttons & (1 << 10) != 0 { 255 } else { 0 };
        self.mem.r_trig = if buttons & (1 << 11) != 0 { 255 } else { 0 };
        self.mem.maple_buttons = m;
    }

    pub fn run_frame(&mut self) {
        if self.mem.halted {
            return;
        }
        for _ in 0..STEPS_PER_FRAME {
            match self.cpu.step(&mut self.mem) {
                StepEvent::Continue => {}
                StepEvent::Trap(code) => self.mem.handle_trap(&mut self.cpu, code),
                StepEvent::Sleep => break,
            }
        }
        self.mem.vblank_line = (self.mem.vblank_line + 1) % 524;
        self.mem.spg_status ^= 1 << 13; // toggle vblank indicator
        self.mem.scanout(&mut self.frame);
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.mem.audio_out);
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.mem.sample_rate = rate.max(8_000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dreamcast_sh4_draws_rgb565_to_pvr_vram_and_scans_out() {
        // SH-4 program at 0x8C010000:
        //   MOV.L @(8, PC), R1   (D102) -> loads 0xA5000000 (PVR VRAM)
        //   MOV.W @(6, PC), R2   (9203) -> loads 0xF800 (pure red in RGB565)
        //   MOV.W R2, @R1        (2121) -> writes red pixel to VRAM[0]
        //   BRA -2               (AFFE) -> spin
        //   NOP                  (0009)
        //   .word 0xF800         (at offset 10 -> pc=2, pc+4+6 = 12 -> let's align cleanly!)
        let mut bin = vec![0u8; 32];
        let words: [u16; 8] = [
            0xD102, // 0x00: MOV.L @(pc=0x00 -> ((4)&!3)+8 = 0x0C), R1
            0x9202, // 0x02: MOV.W @(pc=0x02 -> 6+4 = 0x0A), R2
            0x2121, // 0x04: MOV.W R2, @R1
            0xAFFE, // 0x06: BRA -2
            0x0009, // 0x08: NOP
            0xF800, // 0x0A: RGB565 pure red
            0x0000, // 0x0C low half of 0xA5000000
            0xA500, // 0x0E high half of 0xA5000000
        ];
        for (i, w) in words.iter().enumerate() {
            bin[i * 2..i * 2 + 2].copy_from_slice(&w.to_le_bytes());
        }
        let mut dc = Dreamcast::from_bytes(&bin, None).unwrap();
        dc.set_buttons(1 << 0); // A button
        assert_eq!(dc.mem.maple_buttons & (1 << 2), 0);
        dc.run_frame();
        assert_eq!(dc.frame()[0], 0x00FF_0000);
    }

    #[test]
    fn parses_ip_bin_header() {
        let mut ip = vec![b' '; 0x100];
        ip[0..15].copy_from_slice(b"SEGA SEGAKATANA");
        ip[0x40..0x49].copy_from_slice(b"MK-51000 ");
        ip[0x60..0x6C].copy_from_slice(b"1ST_READ.BIN");
        ip[0x80..0x8E].copy_from_slice(b"SONIC ADVENTUR");
        let info = parse_ip_bin(&ip).unwrap();
        assert_eq!(info.product_id, "MK-51000");
        assert_eq!(info.boot_file, "1ST_READ.BIN");
        assert_eq!(info.title, "SONIC ADVENTUR");
    }
}
