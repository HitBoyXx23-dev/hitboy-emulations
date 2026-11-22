//! Nintendo 64 on our own cores: VR4300 CPU (C++), memory map, cartridge DMA, the
//! controller/EEPROM protocol, video output and audio. The boot ROM's job (loading the
//! game into RAM) is done directly, so no PIF ROM dump is needed.
//!
//! The Reality Co-Processor is emulated at the instruction level: the RSP runs the game's own
//! microcode and the RDP rasterises the command lists it produces.

pub mod pif;
pub mod rdp;
pub mod rsp;

use cpuvr4300::{Bus, Cpu, Regs};

pub const CPU_HZ: u32 = 93_750_000;
pub const FPS: f64 = 60.0;
const RDRAM: usize = 8 * 1024 * 1024;
const VI_CLOCK: f64 = 48_681_812.0;

/// Buttons in the controller's own bit order (bit set = pressed).
pub mod button {
    pub const A: u16 = 0x8000;
    pub const B: u16 = 0x4000;
    pub const Z: u16 = 0x2000;
    pub const START: u16 = 0x1000;
    pub const D_UP: u16 = 0x0800;
    pub const D_DOWN: u16 = 0x0400;
    pub const D_LEFT: u16 = 0x0200;
    pub const D_RIGHT: u16 = 0x0100;
    pub const L: u16 = 0x0020;
    pub const R: u16 = 0x0010;
    pub const C_UP: u16 = 0x0008;
    pub const C_DOWN: u16 = 0x0004;
    pub const C_LEFT: u16 = 0x0002;
    pub const C_RIGHT: u16 = 0x0001;
}

/// Converts .v64 (byte-swapped) and .n64 (little-endian) dumps to the native .z64 order.
pub fn normalize_rom(mut rom: Vec<u8>) -> Result<Vec<u8>, String> {
    if rom.len() < 0x1000 {
        return Err("This file is too small to be a Nintendo 64 game.".into());
    }
    match &rom[..4] {
        [0x80, 0x37, 0x12, 0x40] => {}
        [0x37, 0x80, 0x40, 0x12] => rom.chunks_exact_mut(2).for_each(|c| c.swap(0, 1)),
        [0x40, 0x12, 0x37, 0x80] => rom.chunks_exact_mut(4).for_each(|c| c.reverse()),
        _ => return Err("This doesn't look like a Nintendo 64 ROM (unknown header).".into()),
    }
    Ok(rom)
}

pub struct Hw {
    pub rdram: Vec<u8>,
    pub(crate) dmem: Vec<u8>,
    pub(crate) imem: Vec<u8>,
    rom: Vec<u8>,
    pub pif: pif::Pif,
    // MI
    pub(crate) mi_intr: u32,
    mi_mask: u32,
    mi_mode: u32,
    // SP
    pub(crate) sp_status: u32,
    sp_regs: [u32; 8],
    pub(crate) sp_pc: u32,
    // DP
    dp_regs: [u32; 8],
    dp_status: u32,
    pub rdp: rdp::Rdp,
    // VI
    pub vi: [u32; 14],
    // AI
    ai_dram: u32,
    ai_queue: Vec<(u32, u32)>,
    ai_dacrate: u32,
    ai_pos: u32,
    ai_phase: f64,
    audio: Vec<f32>,
    out_rate: u32,
    // PI
    pi: [u32; 13],
    // SI
    si_dram: u32,
    si_status: u32,
    // Cart save RAM (SRAM / FlashRAM region) at 0x08000000
    pub sram: Vec<u8>,
    extra: u32,
    resample_acc: f64,
}

fn be_read(m: &[u8], a: usize, size: u32) -> u64 {
    (0..size as usize).fold(0u64, |v, i| (v << 8) | *m.get(a + i).unwrap_or(&0) as u64)
}

fn be_write(m: &mut [u8], a: usize, v: u64, size: u32) {
    for i in 0..size as usize {
        if let Some(b) = m.get_mut(a + i) {
            *b = (v >> (8 * (size as usize - 1 - i))) as u8;
        }
    }
}

impl Hw {
    fn raise(&mut self, bit: u32) {
        self.mi_intr |= 1 << bit;
    }

    pub fn irq(&self) -> bool {
        self.mi_intr & self.mi_mask != 0
    }

    pub(crate) fn reg_read(&mut self, a: u32) -> u32 {
        match a {
            0x0404_0000..=0x0404_001F => {
                let i = ((a >> 2) & 7) as usize;
                match i {
                    4 => self.sp_status,
                    5 | 6 => 0,
                    7 => {
                        let v = self.sp_regs[7];
                        self.sp_regs[7] = 1;
                        v
                    }
                    _ => self.sp_regs[i],
                }
            }
            0x0408_0000 => self.sp_pc,
            0x0410_0000..=0x0410_001F => {
                let i = ((a >> 2) & 7) as usize;
                if i == 3 {
                    self.dp_status | 0x80 // command buffer ready
                } else {
                    self.dp_regs[i]
                }
            }
            0x0430_0000 => self.mi_mode,
            0x0430_0004 => 0x0202_0102,
            0x0430_0008 => self.mi_intr,
            0x0430_000C => self.mi_mask,
            0x0440_0000..=0x0440_0034 => self.vi[((a >> 2) & 15) as usize],
            0x0450_0004 => self.ai_remaining(),
            0x0450_000C => {
                let full = self.ai_queue.len() >= 2;
                let busy = !self.ai_queue.is_empty();
                (full as u32) << 31 | (busy as u32) << 30 | (full as u32)
            }
            0x0460_0000..=0x0460_0030 => {
                let i = ((a >> 2) & 15) as usize;
                if i == 4 {
                    0
                } else {
                    self.pi[i]
                }
            }
            0x0470_0000..=0x0470_001F => {
                // RDRAM interface: report initialised memory.
                match a & 0x1F {
                    0x00 => 0x0E,
                    0x04 => 0x40,
                    0x0C => 0x14,
                    0x10 => 0x0063_4634,
                    _ => 0,
                }
            }
            0x0480_0000 => self.si_dram,
            0x0480_0018 => self.si_status,
            _ => 0,
        }
    }

    pub(crate) fn reg_write(&mut self, a: u32, v: u32) {
        match a {
            0x0404_0000..=0x0404_001F => {
                let i = ((a >> 2) & 7) as usize;
                match i {
                    2 => {
                        self.sp_regs[2] = v;
                        self.sp_dma(v, true);
                    }
                    3 => {
                        self.sp_regs[3] = v;
                        self.sp_dma(v, false);
                    }
                    4 => self.sp_status_write(v),
                    7 => self.sp_regs[7] = 0,
                    _ => self.sp_regs[i] = v,
                }
            }
            0x0408_0000 => self.sp_pc = v & 0xFFC,
            0x0410_0000..=0x0410_001F => {
                let i = ((a >> 2) & 7) as usize;
                match i {
                    0 => {
                        self.dp_regs[0] = v & 0x00FF_FFF8;
                        self.dp_regs[2] = v & 0x00FF_FFF8;
                    }
                    1 => {
                        self.dp_regs[1] = v & 0x00FF_FFF8;
                        self.run_rdp();
                    }
                    3 => {
                        if v & 1 != 0 {
                            self.dp_status &= !1;
                        }
                        if v & 2 != 0 {
                            self.dp_status |= 1;
                        }
                    }
                    _ => {}
                }
            }
            0x0430_0000 => {
                self.mi_mode = v & 0x7F;
                if v & 0x800 != 0 {
                    self.mi_intr &= !(1 << 5);
                }
            }
            0x0430_000C => {
                for bit in 0..6 {
                    if v & (1 << (bit * 2)) != 0 {
                        self.mi_mask &= !(1 << bit);
                    }
                    if v & (2 << (bit * 2)) != 0 {
                        self.mi_mask |= 1 << bit;
                    }
                }
            }
            0x0440_0000..=0x0440_0034 => {
                let i = ((a >> 2) & 15) as usize;
                if i == 4 {
                    self.mi_intr &= !(1 << 3);
                } else {
                    self.vi[i] = v;
                }
            }
            0x0450_0000 => self.ai_dram = v & 0x00FF_FFF8,
            0x0450_0004 => {
                let len = v & 0x3FFF8;
                if len > 0 && self.ai_queue.len() < 2 {
                    self.ai_queue.push((self.ai_dram, len));
                    if self.ai_queue.len() == 1 {
                        self.ai_pos = 0;
                        self.raise(2);
                    }
                }
            }
            0x0450_000C => self.mi_intr &= !(1 << 2),
            0x0450_0010 => self.ai_dacrate = v & 0x3FFF,
            0x0460_0000..=0x0460_0030 => {
                let i = ((a >> 2) & 15) as usize;
                match i {
                    2 => self.pi_dma(v, false),
                    3 => self.pi_dma(v, true),
                    4 => {
                        if v & 2 != 0 {
                            self.mi_intr &= !(1 << 4);
                        }
                    }
                    _ => self.pi[i] = v,
                }
            }
            0x0480_0000 => self.si_dram = v & 0x00FF_FFF8,
            0x0480_0004 => {
                // PIF RAM -> RDRAM (the PIF runs the pending joybus commands first).
                self.pif.run_commands();
                let d = self.si_dram as usize;
                let n = self.pif.ram.len();
                if d + n <= RDRAM {
                    self.rdram[d..d + n].copy_from_slice(&self.pif.ram);
                }
                self.si_status |= 0x1000;
                self.raise(1);
            }
            0x0480_0010 => {
                let d = self.si_dram as usize;
                let n = self.pif.ram.len();
                if d + n <= RDRAM {
                    let src = self.rdram[d..d + n].to_vec();
                    self.pif.ram.copy_from_slice(&src);
                }
                self.si_status |= 0x1000;
                self.raise(1);
            }
            0x0480_0018 => {
                self.si_status &= !0x1000;
                self.mi_intr &= !(1 << 1);
            }
            _ => {}
        }
    }

    fn sp_status_write(&mut self, v: u32) {
        if v & 1 != 0 {
            self.sp_status &= !1;
        }
        if v & 2 != 0 {
            self.sp_status |= 1;
        }
        if v & 4 != 0 {
            self.sp_status &= !2;
        }
        if v & 8 != 0 {
            self.mi_intr &= !1;
        }
        if v & 16 != 0 {
            self.raise(0);
        }
        for (clear, set, bit) in [(5, 6, 5), (7, 8, 6)] {
            if v & (1 << clear) != 0 {
                self.sp_status &= !(1 << bit);
            }
            if v & (1 << set) != 0 {
                self.sp_status |= 1 << bit;
            }
        }
        for sig in 0..8 {
            if v & (1 << (9 + sig * 2)) != 0 {
                self.sp_status &= !(1 << (7 + sig));
            }
            if v & (1 << (10 + sig * 2)) != 0 {
                self.sp_status |= 1 << (7 + sig);
            }
        }
    }

    /// Feeds the RDP the commands between CURRENT and END (from RDRAM, or DMEM via the XBUS).
    fn run_rdp(&mut self) {
        let (start, end) = (self.dp_regs[2], self.dp_regs[1]);
        if end <= start {
            return;
        }
        let xbus = self.dp_status & 1 != 0;
        let words: Vec<u64> = (start..end)
            .step_by(8)
            .map(|a| if xbus { be_read(&self.dmem, (a & 0xFF8) as usize, 8) } else { be_read(&self.rdram, (a as usize) & (RDRAM - 1), 8) })
            .collect();
        let t = prof_start();
        self.rdp.run(&words, &mut self.rdram);
        prof_add(1, t);
        self.dp_regs[2] = end;
        if std::mem::take(&mut self.rdp.full_sync) {
            self.raise(5);
        }
    }

    fn sp_dma(&mut self, len_reg: u32, to_mem: bool) {
        let mem_addr = self.sp_regs[0] as usize;
        let dram = (self.sp_regs[1] & 0x00FF_FFF8) as usize;
        let len = ((len_reg & 0xFFF) | 7) as usize + 1;
        let count = ((len_reg >> 12) & 0xFF) as usize + 1;
        let skip = ((len_reg >> 20) & 0xFFF) as usize;
        let (mut m, mut d) = (mem_addr & 0xFF8, dram);
        let imem = mem_addr & 0x1000 != 0;
        for _ in 0..count {
            for i in 0..len {
                let mi = (m + i) & 0xFFF;
                let di = (d + i) & (RDRAM - 1);
                let mem = if imem { &mut self.imem } else { &mut self.dmem };
                if to_mem {
                    mem[mi] = self.rdram[di];
                } else {
                    self.rdram[di] = mem[mi];
                }
            }
            m += len;
            d += len + skip;
        }
        self.sp_regs[0] = (mem_addr & 0x1000) as u32 | (m & 0xFFF) as u32;
        self.sp_regs[1] = d as u32;
    }

    fn pi_dma(&mut self, len_reg: u32, to_rdram: bool) {
        let dram = (self.pi[0] & 0x00FF_FFFE) as usize;
        let cart = self.pi[1] & 0xFFFF_FFFE;
        let len = (len_reg & 0x00FF_FFFF) as usize + 1;
        for i in 0..len {
            let di = dram + i;
            if di >= RDRAM {
                break;
            }
            let ca = cart as usize + i;
            if to_rdram {
                self.rdram[di] = if (0x1000_0000..0x1FC0_0000).contains(&ca) {
                    *self.rom.get(ca - 0x1000_0000).unwrap_or(&0)
                } else if (0x0800_0000..0x0802_0000).contains(&ca) {
                    self.sram[ca - 0x0800_0000]
                } else {
                    0
                };
            } else if (0x0800_0000..0x0802_0000).contains(&ca) {
                self.sram[ca - 0x0800_0000] = self.rdram[di];
            }
        }
        self.pi[0] = (dram + len) as u32;
        self.pi[1] = cart + len as u32;
        self.extra += (len / 8) as u32;
        self.raise(4);
    }

    fn ai_remaining(&self) -> u32 {
        self.ai_queue.first().map(|(_, l)| l.saturating_sub(self.ai_pos)).unwrap_or(0)
    }

    /// Plays queued audio for `cycles` CPU cycles, resampling to the host rate.
    fn ai_tick(&mut self, cycles: u32) {
        let Some(&(addr, len)) = self.ai_queue.first() else { return };
        let freq = VI_CLOCK / (self.ai_dacrate.max(1) + 1) as f64;
        // Frames the DAC consumes during `cycles`.
        self.ai_phase += cycles as f64 * freq / CPU_HZ as f64;
        while self.ai_phase >= 1.0 {
            self.ai_phase -= 1.0;
            let a = (addr + self.ai_pos) as usize & (RDRAM - 1);
            let l = i16::from_be_bytes([self.rdram[a], self.rdram[a + 1]]) as f32 / 32768.0;
            let r = i16::from_be_bytes([self.rdram[a + 2], self.rdram[a + 3]]) as f32 / 32768.0;
            self.push_sample(l, r, freq);
            self.ai_pos += 4;
            if self.ai_pos >= len {
                self.ai_queue.remove(0);
                self.ai_pos = 0;
                if !self.ai_queue.is_empty() {
                    self.raise(2);
                }
                return;
            }
        }
    }

    fn push_sample(&mut self, l: f32, r: f32, src_rate: f64) {
        // Nearest-sample resampling from the DAC rate to the output rate.
        self.resample_acc += self.out_rate as f64;
        while self.resample_acc >= src_rate {
            self.resample_acc -= src_rate;
            self.audio.push(l);
            self.audio.push(r);
        }
    }
}

// Resampler state lives with the audio fields.
impl Hw {
    #[allow(clippy::field_reassign_with_default)]
    fn new(rom: Vec<u8>) -> Hw {
        Hw {
            rdram: vec![0; RDRAM],
            dmem: vec![0; 0x1000],
            imem: vec![0; 0x1000],
            pif: pif::Pif::new(pif::detect_eeprom(&rom)),
            rom,
            mi_intr: 0,
            mi_mask: 0,
            mi_mode: 0,
            sp_status: 1,
            sp_regs: [0; 8],
            sp_pc: 0,
            dp_regs: [0; 8],
            dp_status: 0,
            rdp: rdp::Rdp::default(),
            vi: [0; 14],
            ai_dram: 0,
            ai_queue: Vec::new(),
            ai_dacrate: 0,
            ai_pos: 0,
            ai_phase: 0.0,
            audio: Vec::new(),
            out_rate: 48_000,
            pi: [0; 13],
            si_dram: 0,
            si_status: 0,
            sram: vec![0xFF; 0x20000],
            extra: 0,
            resample_acc: 0.0,
        }
    }
}

impl Bus for Hw {
    fn ram(&mut self) -> Option<&mut [u8]> {
        Some(&mut self.rdram)
    }

    fn read(&mut self, a: u32, size: u32) -> u64 {
        match a {
            0x0000_0000..=0x007F_FFFF => be_read(&self.rdram, a as usize, size),
            0x0400_0000..=0x0400_0FFF => be_read(&self.dmem, (a & 0xFFF) as usize, size),
            0x0400_1000..=0x0400_1FFF => be_read(&self.imem, (a & 0xFFF) as usize, size),
            0x0404_0000..=0x048F_FFFF => {
                let v = self.reg_read(a & !3) as u64;
                match size {
                    8 => v << 32 | self.reg_read((a & !3) + 4) as u64,
                    4 => v,
                    2 => (v >> (16 - (a & 2) * 8)) & 0xFFFF,
                    _ => (v >> (24 - (a & 3) * 8)) & 0xFF,
                }
            }
            0x0800_0000..=0x0801_FFFF => be_read(&self.sram, (a - 0x0800_0000) as usize, size),
            0x1000_0000..=0x1FBF_FFFF => {
                self.extra += 4;
                be_read(&self.rom, (a - 0x1000_0000) as usize, size)
            }
            0x1FC0_07C0..=0x1FC0_07FF => be_read(&self.pif.ram, (a - 0x1FC0_07C0) as usize, size),
            _ => 0,
        }
    }

    fn write(&mut self, a: u32, v: u64, size: u32) {
        match a {
            0x0000_0000..=0x007F_FFFF => be_write(&mut self.rdram, a as usize, v, size),
            0x0400_0000..=0x0400_0FFF => be_write(&mut self.dmem, (a & 0xFFF) as usize, v, size),
            0x0400_1000..=0x0400_1FFF => be_write(&mut self.imem, (a & 0xFFF) as usize, v, size),
            0x0404_0000..=0x048F_FFFF => {
                if size == 8 {
                    self.reg_write(a & !3, (v >> 32) as u32);
                } else {
                    self.reg_write(a & !3, v as u32);
                }
            }
            0x0800_0000..=0x0801_FFFF => be_write(&mut self.sram, (a - 0x0800_0000) as usize, v, size),
            0x1FC0_07C0..=0x1FC0_07FF => {
                be_write(&mut self.pif.ram, (a - 0x1FC0_07C0) as usize, v, size);
            }
            _ => {}
        }
    }
}

/// Optional timing breakdown (set N64_PROF=1): RSP and RDP time in nanoseconds.
static PROF: [std::sync::atomic::AtomicU64; 2] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

fn prof_start() -> Option<std::time::Instant> {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    ON.get_or_init(|| std::env::var_os("N64_PROF").is_some()).then(std::time::Instant::now)
}

fn prof_add(i: usize, t: Option<std::time::Instant>) {
    if let Some(t) = t {
        PROF[i].fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

/// (RSP ms, RDP ms) accumulated since start when N64_PROF is set.
pub fn profile_ms() -> (u64, u64) {
    (PROF[0].load(std::sync::atomic::Ordering::Relaxed) / 1_000_000, PROF[1].load(std::sync::atomic::Ordering::Relaxed) / 1_000_000)
}

pub struct N64 {
    cpu: Cpu<Hw>,
    rsp: rsp::Rsp,
    rsp_budget: u32,
    irq_line: bool,
    frame: Vec<u32>,
    size: (usize, usize),
    line: u32,
}

impl N64 {
    pub fn from_rom(rom: Vec<u8>) -> Result<N64, String> {
        let rom = normalize_rom(rom)?;
        let mut hw = Hw::new(rom);
        // Stand in for the PIF boot ROM and the cartridge boot code: copy the first megabyte of
        // the game to its entry point and set up the registers the boot code leaves behind.
        let entry = u32::from_be_bytes([hw.rom[8], hw.rom[9], hw.rom[10], hw.rom[11]]);
        let load = (entry & 0x1FFF_FFFF) as usize;
        let n = (hw.rom.len() - 0x1000).min(0x10_0000).min(RDRAM - load);
        let chunk = hw.rom[0x1000..0x1000 + n].to_vec();
        hw.rdram[load..load + n].copy_from_slice(&chunk);
        let boot = hw.rom[0x40..0x1000].to_vec();
        hw.dmem[0x40..0x1000].copy_from_slice(&boot);
        // RDRAM size the boot code reports to the game (8 MiB with the Expansion Pak).
        be_write(&mut hw.rdram, 0x318, RDRAM as u64, 4);
        be_write(&mut hw.rdram, 0x3F0, RDRAM as u64, 4);
        hw.vi[0] = 0;
        let mut cpu = Cpu::new(hw);
        let mut r = Regs::default();
        let sx = |v: u32| v as i32 as i64 as u64;
        r.r[11] = sx(0xA400_0040);
        r.r[20] = 1; // NTSC
        r.r[22] = 0x3F; // CIC-6102 seed
        r.r[29] = sx(0xA400_1FF0);
        r.r[31] = sx(0xA400_1550);
        r.pc = sx(entry);
        cpu.set_regs(r);
        cpu.set_cop0(12, 0x3400_0000);
        cpu.set_cop0(16, 0x7006_E463);
        Ok(N64 { cpu, rsp: rsp::Rsp::default(), rsp_budget: 0, irq_line: false, frame: vec![0; 320 * 240], size: (320, 240), line: 0 })
    }

    pub fn title(&self) -> String {
        let t = &self.cpu.bus().rom[0x20..0x34];
        String::from_utf8_lossy(t).trim_end_matches('\0').trim().to_string()
    }

    /// Runs one video field (1/60 s).
    pub fn run_frame(&mut self) {
        const LINES: u32 = 262;
        let per_line = CPU_HZ / 60 / LINES / 2; // two cycles per instruction on average
        for _ in 0..LINES {
            let mut left = per_line as i64;
            while left > 0 {
                // Run the CPU in short slices; devices and the RSP catch up between them.
                let slice = (left as u32).min(64);
                let c = self.cpu.run(slice);
                let hw = self.cpu.bus_mut();
                let c = c + std::mem::take(&mut hw.extra);
                hw.ai_tick(c * 2);
                // The RSP runs at about two thirds of the CPU clock.
                if hw.sp_status & 1 == 0 {
                    self.rsp_budget += c * 4 / 3;
                    let n = std::mem::take(&mut self.rsp_budget);
                    let t = prof_start();
                    self.rsp.run(hw, n);
                    prof_add(0, t);
                } else {
                    self.rsp_budget = 0;
                }
                left -= c as i64;
                let irq = hw.irq();
                if irq != self.irq_line {
                    self.irq_line = irq;
                    self.cpu.set_ip(irq as u32);
                }
            }
            self.line = (self.line + 1) % LINES;
            let hw = self.cpu.bus_mut();
            let half = self.line * 2;
            hw.vi[4] = half;
            if half == hw.vi[3] & 0x3FE {
                hw.raise(3);
            }
        }
        self.render();
    }

    /// Converts the VI framebuffer in RDRAM to the output frame.
    fn render(&mut self) {
        let hw = self.cpu.bus();
        let ctrl = hw.vi[0];
        let origin = (hw.vi[1] & 0x00FF_FFFF) as usize;
        let width = (hw.vi[2] & 0xFFF) as usize;
        if ctrl & 3 < 2 || width == 0 || origin == 0 {
            self.frame.fill(0);
            return;
        }
        let vstart = hw.vi[10];
        let lines = (((vstart & 0x3FF) as i32 - ((vstart >> 16) & 0x3FF) as i32) / 2).max(0) as usize;
        let yscale = (hw.vi[13] & 0xFFF) as usize;
        let mut h = if lines > 0 && yscale > 0 { lines * yscale / 1024 } else { width * 3 / 4 };
        if h == 0 || h > 576 {
            h = width * 3 / 4;
        }
        let w = width.min(1024);
        if self.size != (w, h) {
            self.size = (w, h);
            self.frame = vec![0; w * h];
        }
        let bpp32 = ctrl & 3 == 3;
        for y in 0..h {
            for x in 0..w {
                let px = if bpp32 {
                    let a = (origin + (y * width + x) * 4) & (RDRAM - 1);
                    let v = be_read(&hw.rdram, a, 4) as u32;
                    v >> 8
                } else {
                    let a = (origin + (y * width + x) * 2) & (RDRAM - 1);
                    let v = be_read(&hw.rdram, a, 2) as u32;
                    let ex = |c: u32| (c << 3) | (c >> 2);
                    ex((v >> 11) & 31) << 16 | ex((v >> 6) & 31) << 8 | ex((v >> 1) & 31)
                };
                self.frame[y * w + x] = px;
            }
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    /// Controller 1-4: buttons plus analog stick (-80..80).
    pub fn set_input(&mut self, pad: usize, buttons: u16, x: i8, y: i8) {
        self.cpu.bus_mut().pif.set_input(pad, buttons, x, y);
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.cpu.bus_mut().out_rate = rate;
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.cpu.bus_mut().audio);
    }

    pub fn save_ram(&self) -> Vec<u8> {
        let hw = self.cpu.bus();
        let mut v = hw.pif.eeprom.clone();
        v.extend_from_slice(&hw.sram);
        v
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        let hw = self.cpu.bus_mut();
        let e = hw.pif.eeprom.len();
        if data.len() >= e {
            hw.pif.eeprom.copy_from_slice(&data[..e]);
            let rest = &data[e..];
            let n = rest.len().min(hw.sram.len());
            hw.sram[..n].copy_from_slice(&rest[..n]);
        }
    }

    pub fn pc(&self) -> u64 {
        self.cpu.regs().pc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny game: sets up a 320x240 16-bit framebuffer and fills its first pixel with red.
    fn test_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x2000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        rom[8..12].copy_from_slice(&0x8000_0400u32.to_be_bytes());
        rom[0x20..0x28].copy_from_slice(b"TESTROM ");
        let i = |op: u32, rs: u32, rt: u32, imm: u32| op << 26 | rs << 21 | rt << 16 | (imm & 0xFFFF);
        let code = [
            i(0x0F, 0, 1, 0xA440), // lui r1,0xA440 (VI)
            i(0x09, 0, 2, 0x0002), // addiu r2,r0,2   (16-bit mode)
            i(0x2B, 1, 2, 0x0000), // sw r2,0(r1)
            i(0x0F, 0, 2, 0x0010), // lui r2,0x0010   (origin 0x100000)
            i(0x2B, 1, 2, 0x0004), // sw r2,4(r1)
            i(0x09, 0, 2, 320),    // addiu r2,r0,320
            i(0x2B, 1, 2, 0x0008), // sw r2,8(r1)
            i(0x0F, 0, 3, 0xA010), // lui r3,0xA010   (framebuffer)
            i(0x0D, 0, 4, 0xF801), // ori r4,r0,0xF801 (red, RGBA5551)
            i(0x29, 3, 4, 0x0000), // sh r4,0(r3)
            i(0x04, 0, 0, 0xFFFF), // b .
            0,
        ];
        for (k, w) in code.iter().enumerate() {
            rom[0x1000 + k * 4..0x1004 + k * 4].copy_from_slice(&w.to_be_bytes());
        }
        rom
    }

    #[test]
    fn boots_and_shows_the_cpu_framebuffer() {
        let mut n = N64::from_rom(test_rom()).unwrap();
        assert_eq!(n.title(), "TESTROM");
        n.run_frame();
        assert_eq!(n.size().0, 320);
        assert_eq!(n.frame()[0], 0xFF0000);
    }

    #[test]
    fn byte_orders_are_normalised() {
        let z = test_rom();
        let mut v = z.clone();
        v.chunks_exact_mut(2).for_each(|c| c.swap(0, 1));
        assert_eq!(normalize_rom(v).unwrap(), z);
        let mut n = z.clone();
        n.chunks_exact_mut(4).for_each(|c| c.reverse());
        assert_eq!(normalize_rom(n).unwrap(), z);
    }
}
