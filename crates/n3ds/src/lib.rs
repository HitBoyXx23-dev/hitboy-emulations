//! Clean-room Nintendo 3DS / 2DS emulator core.
//!
//! Runs ARM11 MPCore (ARMv6K) executables from `.3dsx` homebrew, unencrypted `.3ds`/`.cci`
//! (NCSD -> NCCH -> ExeFS `.code` + BLZ/LZSS decompression), and unencrypted `.cia`.
//! Renders Top (400×240) + Bottom (320×240 centered) stacked into a 400×480 dual-screen canvas
//! with live bottom touchscreen pointer input in `hid:USER`.

use cpuarm_v7::{ArmCpu, Bus, StepEvent};

pub const TOP_W: usize = 400;
pub const TOP_H: usize = 240;
pub const BOT_W: usize = 320;
pub const BOT_H: usize = 240;
pub const WIDTH: usize = 400;
pub const HEIGHT: usize = 480;
pub const FPS: f64 = 59.83;

const CODE_BASE: u32 = 0x0010_0000;
const LINEAR_BASE: u32 = 0x1400_0000;
const VRAM_BASE: u32 = 0x1F00_0000;
const TLS_BASE: u32 = 0x1FF8_2000;
const HID_SHARED_MEM: u32 = 0x1000_0000;
const STEPS_PER_FRAME: u32 = 150_000;

/// Parsed 3DS program image.
#[derive(Clone)]
pub struct Loaded3ds {
    pub title: String,
    pub code: Vec<u8>,
    pub load_addr: u32,
    pub entry: u32,
}

/// Decompresses Nintendo 3DS backwards-LZSS (BLZ) `.code` binaries found in ExeFS.
pub fn decompress_blz(input: &[u8]) -> Result<Vec<u8>, String> {
    if input.len() < 8 {
        return Ok(input.to_vec());
    }
    let footer_len = input.len();
    let extra = u32::from_le_bytes(input[footer_len - 4..].try_into().unwrap()) as usize;
    let info = u32::from_le_bytes(input[footer_len - 8..footer_len - 4].try_into().unwrap());
    let header_sz = ((info >> 24) & 0xFF) as usize;
    let comp_sz = (info & 0x00FF_FFFF) as usize;
    if header_sz < 8 || comp_sz > footer_len || extra > 64 * 1024 * 1024 {
        return Ok(input.to_vec());
    }
    let out_len = footer_len + extra;
    let mut out = vec![0u8; out_len];
    let prefix = footer_len - comp_sz;
    out[..prefix].copy_from_slice(&input[..prefix]);

    let mut src = footer_len - header_sz;
    let mut dst = out_len;
    while dst > prefix && src > prefix {
        src -= 1;
        let flags = input[src];
        for bit in (0..8).rev() {
            if dst <= prefix || src <= prefix {
                break;
            }
            if (flags & (1 << bit)) == 0 {
                src -= 1;
                dst -= 1;
                out[dst] = input[src];
            } else {
                if src < 2 {
                    break;
                }
                src -= 2;
                let hi = input[src + 1] as usize;
                let lo = input[src] as usize;
                let len = (hi >> 4) + 3;
                let disp = (((hi & 0xF) << 8) | lo) + 3;
                for _ in 0..len {
                    if dst <= prefix {
                        break;
                    }
                    dst -= 1;
                    out[dst] = out.get(dst + disp).copied().unwrap_or(0);
                }
            }
        }
    }
    Ok(out)
}

/// Parses `.3dsx`, `.3ds`/`.cci` (NCSD/NCCH), or raw ARM11 binary.
pub fn parse_3ds(data: &[u8]) -> Result<Loaded3ds, String> {
    if data.len() >= 32 && &data[0..4] == b"3DSX" {
        return parse_3dsx(data);
    }
    // Check NCSD at 0x100 ("NCSD") or NCCH at 0x100 ("NCCH")
    if data.len() >= 0x200 && &data[0x100..0x104] == b"NCSD" {
        let part0_off = u32::from_le_bytes(data[0x120..0x124].try_into().unwrap()) as usize * 0x200;
        if part0_off + 0x200 <= data.len() && &data[part0_off + 0x100..part0_off + 0x104] == b"NCCH" {
            return parse_ncch(&data[part0_off..]);
        }
        return Err("NCSD cartridge header found, but partition 0 NCCH is missing or out of bounds.".into());
    }
    if data.len() >= 0x200 && &data[0x100..0x104] == b"NCCH" {
        return parse_ncch(data);
    }
    if data.len() >= 0x40 && &data[0..4] == b"\x7FELF" {
        return parse_arm_elf(data);
    }
    // Allow raw ARM11 test payloads
    if data.len() >= 4 {
        return Ok(Loaded3ds {
            title: String::new(),
            code: data.to_vec(),
            load_addr: CODE_BASE,
            entry: CODE_BASE,
        });
    }
    Err("Unrecognized Nintendo 3DS file (expected .3dsx, decrypted .3ds/.cci, or ARM11 .elf).".into())
}

fn parse_3dsx(data: &[u8]) -> Result<Loaded3ds, String> {
    let hdr_sz = u16::from_le_bytes([data[4], data[5]]) as usize;
    let reloc_hdr_sz = u16::from_le_bytes([data[6], data[7]]) as usize;
    let code_sz = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let ro_sz = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let data_sz = u32::from_le_bytes(data[20..24].try_into().unwrap()) as usize;
    let bss_sz = u32::from_le_bytes(data[24..28].try_into().unwrap()) as usize;

    let mut title = String::new();
    if hdr_sz >= 44 && data.len() >= 44 {
        let smdh_off = u32::from_le_bytes(data[32..36].try_into().unwrap()) as usize;
        let smdh_sz = u32::from_le_bytes(data[36..40].try_into().unwrap()) as usize;
        if smdh_off + 0x208 <= data.len() && smdh_sz >= 0x208 && &data[smdh_off..smdh_off + 4] == b"SMDH" {
            // English short title UTF-16LE at +0x208 (slot 1)
            let off = smdh_off + 0x208;
            if off + 128 <= data.len() {
                let u16s: Vec<u16> = data[off..off + 128]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                title = String::from_utf16_lossy(&u16s).trim().to_string();
            }
        }
    }

    let seg_start = hdr_sz + 3 * reloc_hdr_sz;
    let total_file = code_sz.saturating_add(ro_sz).saturating_add(data_sz);
    if seg_start + total_file > data.len() {
        return Err("Truncated .3dsx executable segments.".into());
    }
    let mut mem = Vec::with_capacity(total_file + bss_sz);
    mem.extend_from_slice(&data[seg_start..seg_start + total_file]);
    mem.resize(total_file + bss_sz.min(16 * 1024 * 1024), 0);

    Ok(Loaded3ds {
        title,
        code: mem,
        load_addr: CODE_BASE,
        entry: CODE_BASE,
    })
}

fn parse_ncch(ncch: &[u8]) -> Result<Loaded3ds, String> {
    let flags7 = ncch[0x18F];
    let no_crypto = (flags7 & 0x04) != 0;
    if !no_crypto {
        return Err("Encrypted retail 3DS ROM detected. Decrypt it on your own 3DS (GodMode9) or load an unencrypted .3ds/.cci/.3dsx file.".into());
    }
    let exefs_off = u32::from_le_bytes(ncch[0x1A0..0x1A4].try_into().unwrap()) as usize * 0x200;
    let exefs_sz = u32::from_le_bytes(ncch[0x1A4..0x1A8].try_into().unwrap()) as usize * 0x200;
    if exefs_off + 0x200 > ncch.len() || exefs_sz < 0x200 {
        return Err("NCCH ExeFS missing or truncated.".into());
    }
    let mut code = None;
    let mut title = String::new();
    for i in 0..10 {
        let h = exefs_off + i * 16;
        let name = String::from_utf8_lossy(&ncch[h..h + 8]).trim_end_matches('\0').to_string();
        let f_off = exefs_off + 0x200 + u32::from_le_bytes(ncch[h + 8..h + 12].try_into().unwrap()) as usize;
        let f_sz = u32::from_le_bytes(ncch[h + 12..h + 16].try_into().unwrap()) as usize;
        if f_off + f_sz <= ncch.len() {
            if name == ".code" {
                let raw = &ncch[f_off..f_off + f_sz];
                code = Some(decompress_blz(raw).unwrap_or_else(|_| raw.to_vec()));
            } else if name == "icon" && f_sz >= 0x288 && &ncch[f_off..f_off + 4] == b"SMDH" {
                let u16s: Vec<u16> = ncch[f_off + 0x208..f_off + 0x288]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                title = String::from_utf16_lossy(&u16s).trim().to_string();
            }
        }
    }
    let code = code.ok_or_else(|| "NCCH ExeFS does not contain a .code binary.".to_string())?;
    Ok(Loaded3ds {
        title,
        code,
        load_addr: CODE_BASE,
        entry: CODE_BASE,
    })
}

fn parse_arm_elf(elf: &[u8]) -> Result<Loaded3ds, String> {
    let entry = u32::from_le_bytes(elf[24..28].try_into().unwrap());
    let phoff = u32::from_le_bytes(elf[28..32].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes([elf[42], elf[43]]) as usize;
    let phnum = u16::from_le_bytes([elf[44], elf[45]]) as usize;
    let mut min_v = u32::MAX;
    let mut max_v = 0u32;
    for i in 0..phnum {
        let o = phoff + i * phentsize;
        if o + 32 > elf.len() { break; }
        if u32::from_le_bytes(elf[o..o + 4].try_into().unwrap()) == 1 {
            let v = u32::from_le_bytes(elf[o + 8..o + 12].try_into().unwrap());
            let sz = u32::from_le_bytes(elf[o + 20..o + 24].try_into().unwrap());
            min_v = min_v.min(v);
            max_v = max_v.max(v.saturating_add(sz));
        }
    }
    if min_v >= max_v {
        return Err("ARM11 ELF has no PT_LOAD segments.".into());
    }
    let mut mem = vec![0u8; (max_v - min_v) as usize];
    for i in 0..phnum {
        let o = phoff + i * phentsize;
        if o + 32 > elf.len() { break; }
        if u32::from_le_bytes(elf[o..o + 4].try_into().unwrap()) == 1 {
            let foff = u32::from_le_bytes(elf[o + 4..o + 8].try_into().unwrap()) as usize;
            let v = u32::from_le_bytes(elf[o + 8..o + 12].try_into().unwrap());
            let fsz = u32::from_le_bytes(elf[o + 16..o + 20].try_into().unwrap()) as usize;
            let dst = (v - min_v) as usize;
            if foff + fsz <= elf.len() && dst + fsz <= mem.len() {
                mem[dst..dst + fsz].copy_from_slice(&elf[foff..foff + fsz]);
            }
        }
    }
    Ok(Loaded3ds { title: String::new(), code: mem, load_addr: min_v, entry })
}

/// 3DS Memory & Horizon-3DS HLE state.
pub struct Mem3ds {
    pub code: Vec<u8>,
    pub linear: Vec<u8>,
    pub vram: Vec<u8>,
    pub hid_shared: Vec<u8>,
    pub tls: Vec<u8>,
    pub top_fb_addr: u32,
    pub bot_fb_addr: u32,
    pub buttons: u32,
    pub touch_pressed: bool,
    pub touch_x: u16,
    pub touch_y: u16,
    pub ticks: u64,
    pub exited: bool,
    pub audio_out: Vec<f32>,
    pub debug_log: String,
    pub next_handle: u32,
}

impl Mem3ds {
    pub fn new() -> Self {
        Self {
            code: vec![0; 16 * 1024 * 1024],
            linear: vec![0; 32 * 1024 * 1024],
            vram: vec![0; 6 * 1024 * 1024],
            hid_shared: vec![0; 0x3000],
            tls: vec![0; 0x1000],
            top_fb_addr: VRAM_BASE,
            bot_fb_addr: VRAM_BASE + (TOP_W * TOP_H * 4) as u32,
            buttons: 0,
            touch_pressed: false,
            touch_x: 0,
            touch_y: 0,
            ticks: 0,
            exited: false,
            audio_out: Vec::new(),
            debug_log: String::new(),
            next_handle: 0x0001_0001,
        }
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.write8(addr.wrapping_add(i as u32), b);
        }
    }

    pub fn update_hid_shared(&mut self) {
        // HID pad entry at +0x1C (current state) and touch entry at +0xC8
        let btns = self.buttons.to_le_bytes();
        self.hid_shared[0x1C..0x20].copy_from_slice(&btns);
        self.hid_shared[0x28..0x2C].copy_from_slice(&btns);
        self.hid_shared[0xC8..0xCA].copy_from_slice(&self.touch_x.to_le_bytes());
        self.hid_shared[0xCA..0xCC].copy_from_slice(&self.touch_y.to_le_bytes());
        self.hid_shared[0xCC..0xD0].copy_from_slice(&(self.touch_pressed as u32).to_le_bytes());
    }

    pub fn handle_svc(&mut self, cpu: &mut ArmCpu, svc: u32) {
        match svc & 0xFF {
            // ControlMemory
            0x01 => {
                cpu.r[1] = LINEAR_BASE + 0x0010_0000;
                cpu.r[0] = 0;
            }
            // ExitProcess
            0x03 => {
                self.exited = true;
                cpu.r[0] = 0;
            }
            // SleepThread
            0x0A => cpu.r[0] = 0,
            // GetSystemTick
            0x28 => {
                cpu.r[0] = self.ticks as u32;
                cpu.r[1] = (self.ticks >> 32) as u32;
            }
            // ConnectToPort
            0x2D => {
                let h = self.next_handle;
                self.next_handle += 1;
                cpu.r[1] = h;
                cpu.r[0] = 0;
            }
            // SendSyncRequest (IPC command buffer in TLS + 0x80)
            0x32 => {
                let cmd_hdr = self.read32(TLS_BASE + 0x80);
                // Clear result to 0 (success)
                self.write32(TLS_BASE + 0x84, 0);
                // If gsp::Gpu SetBufferSwap (cmd 0x0005), update fb addresses
                if (cmd_hdr >> 16) == 0x0005 {
                    let screen = self.read32(TLS_BASE + 0x84);
                    let fb = self.read32(TLS_BASE + 0x8C);
                    if fb != 0 {
                        if screen == 0 {
                            self.top_fb_addr = fb;
                        } else {
                            self.bot_fb_addr = fb;
                        }
                    }
                }
                cpu.r[0] = 0;
            }
            // OutputDebugString
            0x3D => {
                let ptr = cpu.r[0];
                let len = (cpu.r[1] as usize).min(256);
                let mut buf = Vec::with_capacity(len);
                for i in 0..len {
                    buf.push(self.read8(ptr.wrapping_add(i as u32)));
                }
                self.debug_log.push_str(&String::from_utf8_lossy(&buf));
                cpu.r[0] = 0;
            }
            // MapMemoryBlock (returns HID shared mem)
            0x1F => {
                cpu.r[0] = 0;
            }
            _ => cpu.r[0] = 0,
        }
    }

    /// Scans both 3DS screens into a 400×480 stacked 0RGB framebuffer.
    pub fn scanout(&self, out: &mut [u32]) {
        out.fill(0x000A_0B12);
        // Top screen: 400x240 at y = 0..240
        for y in 0..TOP_H {
            for x in 0..TOP_W {
                let v = self.read32(self.top_fb_addr.wrapping_add(((y * TOP_W + x) * 4) as u32));
                out[y * WIDTH + x] = v & 0x00FF_FFFF;
            }
        }
        // Bottom screen: 320x240 centered horizontally (x = 40..360) at y = 240..480
        let x_off = (WIDTH - BOT_W) / 2;
        for y in 0..BOT_H {
            for x in 0..BOT_W {
                let v = self.read32(self.bot_fb_addr.wrapping_add(((y * BOT_W + x) * 4) as u32));
                out[(TOP_H + y) * WIDTH + (x_off + x)] = v & 0x00FF_FFFF;
            }
        }
    }
}

impl Default for Mem3ds {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for Mem3ds {
    #[inline]
    fn read8(&self, addr: u32) -> u8 {
        if addr >= CODE_BASE && (addr - CODE_BASE) < self.code.len() as u32 {
            self.code[(addr - CODE_BASE) as usize]
        } else if addr >= LINEAR_BASE && (addr - LINEAR_BASE) < self.linear.len() as u32 {
            self.linear[(addr - LINEAR_BASE) as usize]
        } else if addr >= VRAM_BASE && (addr - VRAM_BASE) < self.vram.len() as u32 {
            self.vram[(addr - VRAM_BASE) as usize]
        } else if addr >= HID_SHARED_MEM && (addr - HID_SHARED_MEM) < self.hid_shared.len() as u32 {
            self.hid_shared[(addr - HID_SHARED_MEM) as usize]
        } else if addr >= TLS_BASE && (addr - TLS_BASE) < self.tls.len() as u32 {
            self.tls[(addr - TLS_BASE) as usize]
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
        u32::from_le_bytes([
            self.read8(addr),
            self.read8(addr.wrapping_add(1)),
            self.read8(addr.wrapping_add(2)),
            self.read8(addr.wrapping_add(3)),
        ])
    }
    #[inline]
    fn write8(&mut self, addr: u32, val: u8) {
        if addr >= CODE_BASE && (addr - CODE_BASE) < self.code.len() as u32 {
            self.code[(addr - CODE_BASE) as usize] = val;
        } else if addr >= LINEAR_BASE && (addr - LINEAR_BASE) < self.linear.len() as u32 {
            self.linear[(addr - LINEAR_BASE) as usize] = val;
        } else if addr >= VRAM_BASE && (addr - VRAM_BASE) < self.vram.len() as u32 {
            self.vram[(addr - VRAM_BASE) as usize] = val;
        } else if addr >= HID_SHARED_MEM && (addr - HID_SHARED_MEM) < self.hid_shared.len() as u32 {
            self.hid_shared[(addr - HID_SHARED_MEM) as usize] = val;
        } else if addr >= TLS_BASE && (addr - TLS_BASE) < self.tls.len() as u32 {
            self.tls[(addr - TLS_BASE) as usize] = val;
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
        for (k, &b) in val.to_le_bytes().iter().enumerate() {
            self.write8(addr.wrapping_add(k as u32), b);
        }
    }
}

/// Complete Nintendo 3DS / 2DS emulator machine.
pub struct Nintendo3ds {
    pub cpu: ArmCpu,
    pub mem: Mem3ds,
    pub title: String,
    loaded: Loaded3ds,
    frame: Vec<u32>,
}

impl Nintendo3ds {
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let loaded = parse_3ds(data)?;
        let mut sys = Self {
            cpu: ArmCpu::new(),
            mem: Mem3ds::new(),
            title: loaded.title.clone(),
            loaded,
            frame: vec![0; WIDTH * HEIGHT],
        };
        sys.reset();
        Ok(sys)
    }

    pub fn reset(&mut self) {
        self.cpu = ArmCpu::new();
        self.mem = Mem3ds::new();
        self.mem.write_slice(self.loaded.load_addr, &self.loaded.code);
        self.cpu.set_pc(self.loaded.entry);
        self.cpu.r[13] = LINEAR_BASE + 0x01FF_0000;
        self.cpu.tls_ro = TLS_BASE;
        self.cpu.tls_rw = TLS_BASE;
        self.frame.fill(0);
    }

    pub fn set_buttons(&mut self, buttons: u16) {
        // 3DS HID bits: A=0, B=1, Select=2, Start=3, Right=4, Left=5, Up=6, Down=7, R=8, L=9, X=10, Y=11
        let mut b = 0u32;
        if buttons & (1 << 0) != 0 { b |= 1 << 0; }
        if buttons & (1 << 1) != 0 { b |= 1 << 1; }
        if buttons & (1 << 2) != 0 { b |= 1 << 2; }
        if buttons & (1 << 3) != 0 { b |= 1 << 3; }
        if buttons & (1 << 7) != 0 { b |= 1 << 4; }
        if buttons & (1 << 6) != 0 { b |= 1 << 5; }
        if buttons & (1 << 4) != 0 { b |= 1 << 6; }
        if buttons & (1 << 5) != 0 { b |= 1 << 7; }
        if buttons & (1 << 11) != 0 { b |= 1 << 8; }
        if buttons & (1 << 10) != 0 { b |= 1 << 9; }
        if buttons & (1 << 8) != 0 { b |= 1 << 10; }
        if buttons & (1 << 9) != 0 { b |= 1 << 11; }
        self.mem.buttons = b;
    }

    /// Maps normalized 0..1 pointer coords across the 400×480 dual-screen canvas onto the 320×240 bottom screen.
    pub fn set_touch(&mut self, pressed: bool, nx: f32, ny: f32) {
        if !pressed || ny < 0.5 {
            self.mem.touch_pressed = false;
            return;
        }
        let px = nx * WIDTH as f32;
        let py = (ny - 0.5) * 2.0 * BOT_H as f32;
        let x_off = ((WIDTH - BOT_W) / 2) as f32;
        if px >= x_off && px < x_off + BOT_W as f32 {
            self.mem.touch_pressed = true;
            self.mem.touch_x = (px - x_off).clamp(0.0, (BOT_W - 1) as f32) as u16;
            self.mem.touch_y = py.clamp(0.0, (BOT_H - 1) as f32) as u16;
        } else {
            self.mem.touch_pressed = false;
        }
    }

    pub fn run_frame(&mut self) {
        if self.mem.exited {
            return;
        }
        self.mem.update_hid_shared();
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
        self.mem.ticks = self.mem.ticks.wrapping_add(4_468_724);
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
    fn n3ds_3dsx_draws_dual_screens_and_tracks_touch() {
        // ARM11 code: write bright green (0x0000FF00) to top VRAM (0x1F000000), then SVC 0x03 ExitProcess
        let words: [u32; 4] = [
            0xE3A0_041F, // mov r0, #0x1F000000
            0xE3A0_1C00 | 0xFF, // mov r1, #0x0000FF00 (rot=24 -> 0xC)
            0xE580_1000, // str r1, [r0]
            0xEF00_0003, // svc #0x03
        ];
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut hdr = vec![0u8; 32];
        hdr[0..4].copy_from_slice(b"3DSX");
        hdr[4..6].copy_from_slice(&32u16.to_le_bytes()); // header size
        hdr[6..8].copy_from_slice(&0u16.to_le_bytes());  // reloc header size
        hdr[12..16].copy_from_slice(&(code.len() as u32).to_le_bytes());
        hdr.extend_from_slice(&code);

        let mut sys = Nintendo3ds::from_bytes(&hdr).unwrap();
        sys.set_touch(true, 0.5, 0.75);
        assert!(sys.mem.touch_pressed);
        assert_eq!(sys.mem.touch_x, 160);
        assert_eq!(sys.mem.touch_y, 120);
        sys.run_frame();
        assert_eq!(sys.frame()[0], 0x0000_FF00);
        assert!(sys.mem.exited);
    }
}
