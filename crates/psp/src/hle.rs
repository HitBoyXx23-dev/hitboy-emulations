//! PSP HLE Kernel, Memory Map, GE Display List rasterizer, Audio, and Controller services.

use std::collections::BTreeMap;

use crate::allegrex::{Allegrex, Bus};
use crate::format::NidImport;

pub const WIDTH: usize = 480;
pub const HEIGHT: usize = 272;
pub const FPS: f64 = 59.94;

const SCRATCH_BASE: u32 = 0x0001_0000;
const SCRATCH_SIZE: usize = 16 * 1024;
const VRAM_BASE: u32 = 0x0400_0000;
const VRAM_SIZE: usize = 2 * 1024 * 1024;
const RAM_BASE: u32 = 0x0800_0000;
const RAM_SIZE: usize = 64 * 1024 * 1024;

/// Pixel formats used by `sceDisplaySetFrameBuf`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb565 = 0,
    Abgr5551 = 1,
    Abgr4444 = 2,
    Abgr8888 = 3,
}

impl PixelFormat {
    fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::Rgb565,
            1 => Self::Abgr5551,
            2 => Self::Abgr4444,
            _ => Self::Abgr8888,
        }
    }
}

struct OpenFile {
    data: Vec<u8>,
    pos: usize,
}

/// PSP memory + HLE hardware state.
pub struct PspMem {
    pub scratch: Vec<u8>,
    pub vram: Vec<u8>,
    pub ram: Vec<u8>,
    pub fb_addr: u32,
    pub fb_stride: u32,
    pub fb_fmt: PixelFormat,
    pub vcount: u32,
    pub pad_buttons: u32,
    pub stick_x: u8,
    pub stick_y: u8,
    pub sample_rate: u32,
    pub audio_out: Vec<f32>,
    pub exited: bool,
    pub exit_code: i32,
    pub heap_ptr: u32,
    pub files: BTreeMap<String, Vec<u8>>,
    open_files: BTreeMap<i32, OpenFile>,
    next_fd: i32,
    next_thread: i32,
    pub nid_by_syscall: BTreeMap<u32, (String, u32)>,
    pub log: String,
    pub vblank_wait: bool,
}

impl PspMem {
    pub fn new(files: BTreeMap<String, Vec<u8>>) -> Self {
        Self {
            scratch: vec![0; SCRATCH_SIZE],
            vram: vec![0; VRAM_SIZE],
            ram: vec![0; RAM_SIZE],
            fb_addr: VRAM_BASE,
            fb_stride: 512,
            fb_fmt: PixelFormat::Abgr8888,
            vcount: 0,
            pad_buttons: 0,
            stick_x: 128,
            stick_y: 128,
            sample_rate: 44_100,
            audio_out: Vec::new(),
            exited: false,
            exit_code: 0,
            heap_ptr: 0x0A00_0000,
            files,
            open_files: BTreeMap::new(),
            next_fd: 3,
            next_thread: 0x100,
            nid_by_syscall: BTreeMap::new(),
            log: String::new(),
            vblank_wait: false,
        }
    }

    #[inline]
    fn norm(addr: u32) -> u32 {
        addr & 0x3FFF_FFFF
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.write8(addr.wrapping_add(i as u32), b);
        }
    }

    pub fn read_cstr(&self, addr: u32) -> String {
        let mut out = Vec::new();
        for i in 0..256u32 {
            let b = self.read8(addr.wrapping_add(i));
            if b == 0 {
                break;
            }
            out.push(b);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Patches each NID import stub with `jr $ra` + `syscall <id>` (in delay slot).
    pub fn patch_imports(&mut self, imports: &[NidImport]) {
        for (idx, imp) in imports.iter().enumerate() {
            let sc = 0x2000 + idx as u32;
            self.nid_by_syscall.insert(sc, (imp.module.clone(), imp.nid));
            // jr $ra (0x03E00008), followed in delay slot by syscall sc
            self.write32(imp.stub_addr, 0x03E0_0008);
            self.write32(imp.stub_addr.wrapping_add(4), (sc << 6) | 0x0C);
        }
    }

    /// Dispatches a PSP HLE syscall triggered by `syscall <code>`.
    pub fn handle_syscall(&mut self, cpu: &mut Allegrex, code: u32) {
        let nid = self.nid_by_syscall.get(&code).map(|(_, n)| *n).unwrap_or(code);
        let a0 = cpu.r[4];
        let a1 = cpu.r[5];
        let a2 = cpu.r[6];
        let a3 = cpu.r[7];

        let ret: u32 = match nid {
            // sceDisplaySetMode(mode, width, height)
            0x0E20_F177 => 0,
            // sceDisplaySetFrameBuf(topaddr, bufferwidth, pixelformat, sync)
            0x289D_82FE => {
                if a0 != 0 {
                    self.fb_addr = a0;
                }
                if a1 != 0 {
                    self.fb_stride = a1;
                }
                self.fb_fmt = PixelFormat::from_u32(a2);
                0
            }
            // sceDisplayGetFrameBuf
            0xEEDA_2E54 => {
                if a0 != 0 {
                    self.write32(a0, self.fb_addr);
                }
                if a1 != 0 {
                    self.write32(a1, self.fb_stride);
                }
                if a2 != 0 {
                    self.write32(a2, self.fb_fmt as u32);
                }
                0
            }
            // sceDisplayWaitVblankStart / WaitVblankStartCB / WaitVblank
            0x984C_27E7 | 0x46F1_86C3 | 0x36CD_FADE | 0x8EB9_EC49 => {
                self.vblank_wait = true;
                0
            }
            // sceDisplayGetVcount
            0x9C6E_AAD7 => self.vcount,

            // sceCtrlSetSamplingCycle / SetSamplingMode
            0x6A27_74F3 | 0x1F40_11E6 => 0,
            // sceCtrlReadBufferPositive / PeekBufferPositive / ReadBufferNegative
            0x1F80_3938 | 0x3A62_2550 | 0x60B8_1F86 => {
                // SceCtrlData: u32 TimeStamp, u32 Buttons, u8 Lx, u8 Ly, u8 Rsrv[6]
                let count = a1.clamp(1, 64);
                let btns = if nid == 0x60B8_1F86 { !self.pad_buttons } else { self.pad_buttons };
                for i in 0..count {
                    let base = a0.wrapping_add(i * 16);
                    self.write32(base, self.vcount.wrapping_mul(16_683));
                    self.write32(base + 4, btns);
                    self.write8(base + 8, self.stick_x);
                    self.write8(base + 9, self.stick_y);
                }
                count
            }

            // sceGeEdramGetAddr
            0xE47E_40E4 => VRAM_BASE,
            // sceGeEdramGetSize
            0x1F67_52AD => VRAM_SIZE as u32,
            // sceGeListEnQueue / ListEnQueueHead
            0xAB49_E76A | 0x1C0D_95A6 => {
                self.run_ge_list(a0, a1);
                1
            }
            // sceGeListSync / DrawSync
            0x0344_4EB4 | 0xB287_BD61 => 0,

            // sceAudioChReserve(channel, samplecount, format)
            0x5EC8_1C55 => {
                if (a0 as i32) < 0 { 0 } else { a0 }
            }
            // sceAudioChRelease
            0x6FC4_6853 => 0,
            // sceAudioOutput / OutputBlocking / OutputPanned / OutputPannedBlocking
            0x8C10_09B2 | 0x136C_AF51 | 0xE2D5_6B2D | 0x13F5_92BC => {
                let buf = if nid == 0xE2D5_6B2D || nid == 0x13F5_92BC { a3 } else { a2 };
                let samples = 256usize;
                for i in 0..samples {
                    let l = self.read16(buf.wrapping_add((i * 4) as u32)) as i16 as f32 / 32768.0;
                    let r = self.read16(buf.wrapping_add((i * 4 + 2) as u32)) as i16 as f32 / 32768.0;
                    self.audio_out.push(l);
                    self.audio_out.push(r);
                }
                0
            }

            // sceKernelExitGame
            0x0557_2A5F | 0x2AC9_954B => {
                self.exited = true;
                self.exit_code = a0 as i32;
                0
            }
            // sceKernelCreateThread
            0x446D_8DE6 => {
                let id = self.next_thread;
                self.next_thread += 1;
                id as u32
            }
            // sceKernelStartThread / RegisterExitCallback / CreateCallback / SleepThread / DelayThread
            0xF475_845D | 0x4AC5_7943 | 0xE81C_AF8F | 0x9ACE_131E | 0xCEAD_EB47 | 0x68DA_9E36 => 0,
            // sceKernelGetSystemTimeLow
            0x369E_DBDF => self.vcount.wrapping_mul(16_683),
            // sceKernelAllocPartitionMemory
            0x237D_BD4F => {
                let size = (a3 + 15) & !15;
                let ptr = self.heap_ptr;
                self.heap_ptr = self.heap_ptr.wrapping_add(size.max(64));
                ptr
            }
            // sceKernelGetBlockHeadAddr
            0x9D9A_5BA1 => a0,
            // sceKernelFreePartitionMemory
            0xB6D6_1D02 => 0,

            // sceIoOpen
            0x109F_50BC => {
                let path = self.read_cstr(a0);
                let norm = path
                    .trim_start_matches("disc0:/")
                    .trim_start_matches("ms0:/")
                    .trim_start_matches('/')
                    .to_ascii_uppercase();
                if let Some(data) = self.files.get(&norm).cloned() {
                    let fd = self.next_fd;
                    self.next_fd += 1;
                    self.open_files.insert(fd, OpenFile { data, pos: 0 });
                    fd as u32
                } else {
                    0x8001_0002 // ENOENT
                }
            }
            // sceIoRead
            0x6A63_8D83 => {
                let fd = a0 as i32;
                let dst = a1;
                let len = a2 as usize;
                if let Some(f) = self.open_files.get_mut(&fd) {
                    let avail = f.data.len().saturating_sub(f.pos).min(len);
                    let chunk = f.data[f.pos..f.pos + avail].to_vec();
                    f.pos += avail;
                    self.write_slice(dst, &chunk);
                    avail as u32
                } else {
                    0x8002_032B
                }
            }
            // sceIoWrite (stdout/stderr goes to log)
            0x42EC_03AC => {
                let fd = a0 as i32;
                if fd == 1 || fd == 2 {
                    let len = (a2 as usize).min(512);
                    let mut buf = Vec::with_capacity(len);
                    for i in 0..len {
                        buf.push(self.read8(a1.wrapping_add(i as u32)));
                    }
                    self.log.push_str(&String::from_utf8_lossy(&buf));
                    len as u32
                } else {
                    a2
                }
            }
            // sceIoLseek / sceIoLseek32
            0x27EB_32EC | 0x6896_3324 => {
                let fd = a0 as i32;
                let off = a1 as i32;
                let whence = a2;
                if let Some(f) = self.open_files.get_mut(&fd) {
                    let target: i64 = match whence {
                        0 => off as i64,
                        1 => f.pos as i64 + off as i64,
                        2 => f.data.len() as i64 + off as i64,
                        _ => f.pos as i64,
                    };
                    f.pos = target.clamp(0, f.data.len() as i64) as usize;
                    f.pos as u32
                } else {
                    0x8002_032B
                }
            }
            // sceIoClose
            0x810C_4BC3 => {
                self.open_files.remove(&(a0 as i32));
                0
            }

            // scePowerSetClockFrequency / scePowerGetBatteryLifePercent
            0x7374_86F2 => 0,
            0x2085_D15D => 100,

            _ => 0,
        };

        cpu.r[2] = ret; // $v0
    }

    /// Minimal GE (Graphics Engine) display-list interpreter supporting clears, framebuffer
    /// setup, and 2D screen-space sprite/triangle fills.
    fn run_ge_list(&mut self, list_addr: u32, stall_addr: u32) {
        let mut pc = list_addr;
        let mut base_addr: u32 = 0;
        let mut fb_ptr: u32 = self.fb_addr;
        let mut fb_w: u32 = self.fb_stride.max(64);
        let mut vaddr: u32 = 0;
        let mut vtype: u32 = 0;
        let mut color: u32 = 0xFFFF_FFFF;

        for _ in 0..4096 {
            if stall_addr != 0 && pc == stall_addr {
                break;
            }
            let cmd = self.read32(pc);
            pc = pc.wrapping_add(4);
            let op = (cmd >> 24) as u8;
            let arg = cmd & 0x00FF_FFFF;
            match op {
                0x00 => {} // NOP
                0x01 => vaddr = base_addr | arg,
                0x04 => {
                    // PRIM: draw 2D sprite (type 6) with through-mode vertices
                    let prim = (arg >> 16) & 7;
                    let count = (arg & 0xFFFF) as usize;
                    if prim == 6 && (vtype & (1 << 23)) != 0 && count >= 2 {
                        let mut ptr = vaddr;
                        let pos_fmt = (vtype >> 7) & 3;
                        let col_fmt = (vtype >> 2) & 7;
                        for _ in (0..count).step_by(2) {
                            let (c0, x0, y0, step0) = self.read_ge_vertex(ptr, pos_fmt, col_fmt, color);
                            ptr = ptr.wrapping_add(step0);
                            let (c1, x1, y1, step1) = self.read_ge_vertex(ptr, pos_fmt, col_fmt, c0);
                            ptr = ptr.wrapping_add(step1);
                            let (lx, rx) = (x0.min(x1).max(0) as usize, x0.max(x1).min(WIDTH as i32) as usize);
                            let (ty, by) = (y0.min(y1).max(0) as usize, y0.max(y1).min(HEIGHT as i32) as usize);
                            for y in ty..by {
                                for x in lx..rx {
                                    self.write32(fb_ptr.wrapping_add(((y as u32) * fb_w + x as u32) * 4), c1);
                                }
                            }
                        }
                    }
                }
                0x0B | 0x0C => break, // RET / END
                0x0F => {}            // FINISH
                0x10 => base_addr = (arg & 0x000F_0000) << 8,
                0x12 => vtype = arg,
                0x54 => color = (color & 0xFF00_0000) | arg,
                0x58 => color = (color & 0x00FF_FFFF) | ((arg & 0xFF) << 24),
                0x9C => fb_ptr = (fb_ptr & 0xFF00_0000) | arg,
                0x9D => {
                    fb_ptr = (fb_ptr & 0x00FF_FFFF) | ((arg & 0x00FF_0000) << 8);
                    fb_w = (arg & 0xFFFF).max(64);
                }
                _ => {}
            }
        }
    }

    fn read_ge_vertex(&self, addr: u32, pos_fmt: u32, col_fmt: u32, default_col: u32) -> (u32, i32, i32, u32) {
        let mut off = 0u32;
        let col = match col_fmt {
            7 => {
                let c = self.read32(addr + off);
                off += 4;
                c
            }
            4..=6 => {
                let c = self.read16(addr + off);
                off += 2;
                rgb565_to_argb(c)
            }
            _ => default_col,
        };
        let (x, y) = match pos_fmt {
            1 => {
                let x = self.read8(addr + off) as i8 as i32;
                let y = self.read8(addr + off + 1) as i8 as i32;
                off += 3;
                (x, y)
            }
            2 => {
                off = (off + 1) & !1;
                let x = self.read16(addr + off) as i16 as i32;
                let y = self.read16(addr + off + 2) as i16 as i32;
                off += 6;
                (x, y)
            }
            3 => {
                off = (off + 3) & !3;
                let x = f32::from_bits(self.read32(addr + off)) as i32;
                let y = f32::from_bits(self.read32(addr + off + 4)) as i32;
                off += 12;
                (x, y)
            }
            _ => (0, 0),
        };
        (col, x, y, (off + 3) & !3)
    }

    /// Scans the active PSP framebuffer into `out` as 0RGB `u32` pixels (480×272).
    pub fn scanout(&self, out: &mut [u32]) {
        let stride = self.fb_stride.max(WIDTH as u32) as usize;
        let base = self.fb_addr;
        for y in 0..HEIGHT {
            let row = y * stride;
            for x in 0..WIDTH {
                let idx = row + x;
                let px = match self.fb_fmt {
                    PixelFormat::Abgr8888 => {
                        let v = self.read32(base.wrapping_add((idx * 4) as u32));
                        let r = v & 0xFF;
                        let g = (v >> 8) & 0xFF;
                        let b = (v >> 16) & 0xFF;
                        (r << 16) | (g << 8) | b
                    }
                    PixelFormat::Rgb565 => {
                        let v = self.read16(base.wrapping_add((idx * 2) as u32));
                        let r = ((v & 0x1F) as u32 * 255) / 31;
                        let g = (((v >> 5) & 0x3F) as u32 * 255) / 63;
                        let b = (((v >> 11) & 0x1F) as u32 * 255) / 31;
                        (r << 16) | (g << 8) | b
                    }
                    PixelFormat::Abgr5551 => {
                        let v = self.read16(base.wrapping_add((idx * 2) as u32));
                        let r = ((v & 0x1F) as u32 * 255) / 31;
                        let g = (((v >> 5) & 0x1F) as u32 * 255) / 31;
                        let b = (((v >> 10) & 0x1F) as u32 * 255) / 31;
                        (r << 16) | (g << 8) | b
                    }
                    PixelFormat::Abgr4444 => {
                        let v = self.read16(base.wrapping_add((idx * 2) as u32));
                        let r = ((v & 0xF) as u32) * 17;
                        let g = (((v >> 4) & 0xF) as u32) * 17;
                        let b = (((v >> 8) & 0xF) as u32) * 17;
                        (r << 16) | (g << 8) | b
                    }
                };
                out[y * WIDTH + x] = px;
            }
        }
    }
}

fn rgb565_to_argb(v: u16) -> u32 {
    let r = ((v & 0x1F) as u32 * 255) / 31;
    let g = (((v >> 5) & 0x3F) as u32 * 255) / 63;
    let b = (((v >> 11) & 0x1F) as u32 * 255) / 31;
    0xFF00_0000 | (b << 16) | (g << 8) | r
}

impl Bus for PspMem {
    #[inline]
    fn read8(&self, addr: u32) -> u8 {
        let a = Self::norm(addr);
        if a >= RAM_BASE && (a - RAM_BASE) < RAM_SIZE as u32 {
            self.ram[(a - RAM_BASE) as usize]
        } else if a >= VRAM_BASE && (a - VRAM_BASE) < VRAM_SIZE as u32 {
            self.vram[(a - VRAM_BASE) as usize]
        } else if a >= SCRATCH_BASE && (a - SCRATCH_BASE) < SCRATCH_SIZE as u32 {
            self.scratch[(a - SCRATCH_BASE) as usize]
        } else {
            0
        }
    }

    #[inline]
    fn read16(&self, addr: u32) -> u16 {
        let lo = self.read8(addr) as u16;
        let hi = self.read8(addr.wrapping_add(1)) as u16;
        lo | (hi << 8)
    }

    #[inline]
    fn read32(&self, addr: u32) -> u32 {
        let a = Self::norm(addr);
        if a >= RAM_BASE && (a - RAM_BASE + 4) <= RAM_SIZE as u32 {
            let i = (a - RAM_BASE) as usize;
            u32::from_le_bytes([self.ram[i], self.ram[i + 1], self.ram[i + 2], self.ram[i + 3]])
        } else if a >= VRAM_BASE && (a - VRAM_BASE + 4) <= VRAM_SIZE as u32 {
            let i = (a - VRAM_BASE) as usize;
            u32::from_le_bytes([self.vram[i], self.vram[i + 1], self.vram[i + 2], self.vram[i + 3]])
        } else {
            let b0 = self.read8(addr) as u32;
            let b1 = self.read8(addr.wrapping_add(1)) as u32;
            let b2 = self.read8(addr.wrapping_add(2)) as u32;
            let b3 = self.read8(addr.wrapping_add(3)) as u32;
            b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
        }
    }

    #[inline]
    fn write8(&mut self, addr: u32, val: u8) {
        let a = Self::norm(addr);
        if a >= RAM_BASE && (a - RAM_BASE) < RAM_SIZE as u32 {
            self.ram[(a - RAM_BASE) as usize] = val;
        } else if a >= VRAM_BASE && (a - VRAM_BASE) < VRAM_SIZE as u32 {
            self.vram[(a - VRAM_BASE) as usize] = val;
        } else if a >= SCRATCH_BASE && (a - SCRATCH_BASE) < SCRATCH_SIZE as u32 {
            self.scratch[(a - SCRATCH_BASE) as usize] = val;
        }
    }

    #[inline]
    fn write16(&mut self, addr: u32, val: u16) {
        self.write8(addr, val as u8);
        self.write8(addr.wrapping_add(1), (val >> 8) as u8);
    }

    #[inline]
    fn write32(&mut self, addr: u32, val: u32) {
        let a = Self::norm(addr);
        let b = val.to_le_bytes();
        if a >= RAM_BASE && (a - RAM_BASE + 4) <= RAM_SIZE as u32 {
            let i = (a - RAM_BASE) as usize;
            self.ram[i..i + 4].copy_from_slice(&b);
        } else if a >= VRAM_BASE && (a - VRAM_BASE + 4) <= VRAM_SIZE as u32 {
            let i = (a - VRAM_BASE) as usize;
            self.vram[i..i + 4].copy_from_slice(&b);
        } else {
            self.write8(addr, b[0]);
            self.write8(addr.wrapping_add(1), b[1]);
            self.write8(addr.wrapping_add(2), b[2]);
            self.write8(addr.wrapping_add(3), b[3]);
        }
    }
}
