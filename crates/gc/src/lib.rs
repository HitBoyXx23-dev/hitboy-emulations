//! Nintendo GameCube on our own cores: Gekko CPU (C++), 24 MiB main RAM, 16 MiB ARAM,
//! the video interface (YUV framebuffer output), processor-interface interrupts, serial
//! controllers, EXI, the DSP's DMA/mailbox registers and audio DMA. Programs start from their
//! .dol executable with the machine state the boot ROM leaves, so no IPL dump is needed.
//!
//! The GX graphics pipeline is next; writes to its FIFO are collected in `gx_fifo`.

use cpugekko::{Bus, Cpu, Regs};
pub mod gx;
mod gx_draw;
mod gx_tex;
pub mod wii;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const CPU_HZ: u32 = 486_000_000;
pub const FPS: f64 = 59.94;
/// Real-chip cycles per emulated instruction budget unit (see `run_frame`).
const SCALE: u32 = 12;
const RAM: usize = 24 * 1024 * 1024;
const ARAM: usize = 16 * 1024 * 1024;

/// Controller buttons in the pad's own bit order (bit set = pressed).
pub mod button {
    pub const LEFT: u16 = 0x0001;
    pub const RIGHT: u16 = 0x0002;
    pub const DOWN: u16 = 0x0004;
    pub const UP: u16 = 0x0008;
    pub const Z: u16 = 0x0010;
    pub const R: u16 = 0x0020;
    pub const L: u16 = 0x0040;
    pub const A: u16 = 0x0100;
    pub const B: u16 = 0x0200;
    pub const X: u16 = 0x0400;
    pub const Y: u16 = 0x0800;
    pub const START: u16 = 0x1000;
}

#[derive(Clone, Copy, Default)]
pub struct Pad {
    pub buttons: u16,
    pub stick: (i8, i8),
    pub cstick: (i8, i8),
    pub triggers: (u8, u8),
}

pub struct Hw {
    pub ram: Vec<u8>,
    aram: Vec<u8>,
    // Processor interface
    pi_intsr: u32,
    pi_intmr: u32,
    pi_fifo: [u32; 3],
    // Video interface (16-bit registers stored as bytes)
    vi: Vec<u8>,
    vline: u32,
    // Serial interface
    si: [u32; 16],
    si_buf: [u8; 128],
    pub pads: [Pad; 4],
    // EXI
    exi: [[u32; 5]; 3],
    // DSP / ARAM / audio DMA
    dsp: Vec<u8>,
    ai_pos: u32,
    ai_phase: f64,
    dsp_mail_out: std::collections::VecDeque<u32>,
    dsp_expect: bool,
    dsp_cmd: u32,
    ai_cr: u32,
    ai_scnt: u32,
    ai_scnt_phase: f64,
    ai_it: u32,
    audio: Vec<f32>,
    dsp_cmdlist: bool,
    out_rate: u32,
    resample: f64,
    // Graphics FIFO writes (the GX pipeline consumes these next).
    pub gx: gx::Gx,
    pub gx_count: usize,
    /// Diagnostics: a watched RAM address (0 = off) and whether it was just written.
    di_countdown: i32,
    /// Diagnostics: set when a disc read was started with a destination near address 0.
    di_null: bool,
    watch: u32,
    watch_hit: Option<u64>,
    // Disc drive
    disc: Option<File>,
    di: [u32; 10],
}

fn be(m: &[u8], a: usize, size: u32) -> u64 {
    (0..size as usize).fold(0u64, |v, i| (v << 8) | *m.get(a + i).unwrap_or(&0) as u64)
}

fn set_be(m: &mut [u8], a: usize, v: u64, size: u32) {
    for i in 0..size as usize {
        if let Some(b) = m.get_mut(a + i) {
            *b = (v >> (8 * (size as usize - 1 - i))) as u8;
        }
    }
}

mod pi_bit {
    pub const DI: u32 = 1 << 2;
    pub const SI: u32 = 1 << 3;
    pub const EXI: u32 = 1 << 4;
    pub const DSP: u32 = 1 << 6;
    pub const VI: u32 = 1 << 8;
}

impl Hw {
    fn new() -> Hw {
        Hw {
            ram: vec![0; RAM],
            aram: vec![0; ARAM],
            pi_intsr: 0,
            pi_intmr: 0,
            pi_fifo: [0; 3],
            vi: vec![0; 0x100],
            vline: 1,
            si: [0; 16],
            si_buf: [0; 128],
            pads: [Pad::default(); 4],
            exi: [[0; 5]; 3],
            dsp: {
                let mut d = vec![0u8; 0x200];
                d[0x17] = 1; // ARAM controller: initialised
                d
            },
            ai_pos: 0,
            ai_phase: 0.0,
            dsp_mail_out: std::collections::VecDeque::new(),
            dsp_expect: false,
            dsp_cmd: 0,
            ai_cr: 0,
            ai_scnt: 0,
            ai_scnt_phase: 0.0,
            ai_it: 0,
            audio: Vec::new(),
            dsp_cmdlist: false,
            out_rate: 48_000,
            resample: 0.0,
            gx: gx::Gx::new(),
            gx_count: 0,
            di_countdown: 0,
            di_null: false,
            watch: 0,
            watch_hit: None,
            disc: None,
            di: [0; 10],
        }
    }

    /// Mirrors the graphics chip's token / finish interrupts into the processor interface.
    fn update_gx_irq(&mut self) {
        let (token, finish) = self.gx.irq_bits();
        self.pi_intsr = (self.pi_intsr & !(3 << 9)) | (token as u32) << 9 | (finish as u32) << 10;
    }

    pub fn irq(&self) -> bool {
        self.pi_intsr & self.pi_intmr != 0
    }

    fn vi16(&self, off: usize) -> u32 {
        be(&self.vi, off, 2) as u32
    }
    fn vi32(&self, off: usize) -> u32 {
        be(&self.vi, off, 4) as u32
    }

    fn update_vi_irq(&mut self) {
        let any = (0..4).any(|i| self.vi32(0x30 + i * 4) & 0x8000_0000 != 0);
        if any {
            self.pi_intsr |= pi_bit::VI;
        } else {
            self.pi_intsr &= !pi_bit::VI;
        }
    }

    /// Advances the video beam one line; display interrupts fire on their programmed line.
    fn vi_line(&mut self) {
        if self.di_countdown > 0 {
            self.di_countdown -= 1;
            if self.di_countdown == 0 {
                self.di_complete();
            }
        }
        self.vline = if self.vline >= 525 { 1 } else { self.vline + 1 };
        set_be(&mut self.vi, 0x2C, self.vline as u64, 2);
        for i in 0..4 {
            let di = self.vi32(0x30 + i * 4);
            let vct = (di >> 16) & 0x3FF;
            if di & 0x1000_0000 != 0 && vct == self.vline {
                set_be(&mut self.vi, 0x30 + i * 4, (di | 0x8000_0000) as u64, 4);
            }
        }
        self.update_vi_irq();
    }

    // ---------------------------------------------------------------- serial controllers

    fn pad_inbuf(&self, ch: usize) -> (u32, u32) {
        let p = self.pads[ch];
        let hi = (p.buttons as u32 & 0x1F7F) << 16 | 0x0080_0000 | ((p.stick.0 as i32 + 128) as u32 & 0xFF) << 8 | ((p.stick.1 as i32 + 128) as u32 & 0xFF);
        let lo = ((p.cstick.0 as i32 + 128) as u32 & 0xFF) << 24 | ((p.cstick.1 as i32 + 128) as u32 & 0xFF) << 16 | (p.triggers.0 as u32) << 8 | p.triggers.1 as u32;
        (hi, lo)
    }

    fn si_transfer(&mut self) {
        let csr = self.si[0x34 / 4];
        let ch = ((csr >> 1) & 3) as usize;
        let cmd = self.si_buf[0];
        let connected = ch == 0;
        let resp: Vec<u8> = if !connected {
            Vec::new()
        } else {
            match cmd {
                0x00 | 0xFF => vec![0x09, 0x00, 0x20],
                0x41 | 0x42 => vec![0x00, 0x80, 0x80, 0x80, 0x80, 0x80, 0x1F, 0x1F, 0x00, 0x00],
                0x40 => {
                    let (h, l) = self.pad_inbuf(ch);
                    let mut v = h.to_be_bytes().to_vec();
                    v.extend_from_slice(&l.to_be_bytes());
                    v
                }
                _ => Vec::new(),
            }
        };
        for (i, b) in resp.iter().enumerate() {
            self.si_buf[i] = *b;
        }
        // Each transfer reports its own result: clear the previous error first.
        let mut csr = csr & !1 & !(1 << 29);
        if resp.is_empty() {
            csr |= 1 << 29; // communication error: no device
            self.si[0x38 / 4] |= 8 << ((3 - ch) * 8); // NOREP on this channel
        }
        csr |= 1 << 31; // transfer complete
        self.si[0x34 / 4] = csr;
        if csr & (1 << 30) != 0 {
            self.pi_intsr |= pi_bit::SI;
        }
    }

    /// Automatic controller polling: fresh input for each enabled channel once per field.
    fn si_poll(&mut self) {
        let poll = self.si[0x30 / 4];
        let mut any = false;
        for ch in 0..4 {
            if poll & (0x80 >> ch) != 0 && ch == 0 {
                self.si[0x38 / 4] |= 0x20 << ((3 - ch) * 8); // RDST: new data waiting
                any = true;
            }
        }
        if any {
            self.si[13] |= 1 << 28; // RDSTINT
            if self.si[13] & (1 << 27) != 0 {
                self.pi_intsr |= pi_bit::SI;
            }
        }
    }

    // ---------------------------------------------------------------- disc drive

    fn disc_read(&mut self, offset: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        if let Some(f) = self.disc.as_mut() {
            if f.seek(SeekFrom::Start(offset)).is_ok() {
                let _ = f.read(&mut buf);
            }
        }
        buf
    }

    /// The game started a drive command. A real drive takes milliseconds (a seek plus the
    /// transfer), and games rely on that: they finish setting up after starting a read and
    /// before its completion interrupt. So the command is queued and completes a realistic
    /// number of scanlines later (see `vi_line`).
    fn di_command(&mut self) {
        let cmd = self.di[2] >> 24;
        let len = if cmd == 0xA8 { self.di[4] as i64 } else { 0 };
        if cmd == 0xA8 && (self.di[5] & 0x03FF_FFE0) < 0x1_0000 {
            self.di_null = true;
        }
        // About 3 ms to start, then 8 MB/s; one scanline is 63 microseconds.
        let lines = if cmd == 0xA8 { 48 + len / 507 } else { 2 };
        self.di[7] |= 1;
        self.di_countdown = lines.clamp(2, 6000) as i32;
    }

    fn di_complete(&mut self) {
        let cmd = self.di[2] >> 24;
        match cmd {
            0xA8 => {
                // Read: disc offset in 4-byte units, DMA into RAM.
                let off = (self.di[3] as u64) << 2;
                let len = self.di[4] as usize;
                let dst = (self.di[5] & 0x03FF_FFE0) as usize;
                // A transfer into the first few KB of memory would wipe the processor's exception
                // vectors (the console's own firmware never does this); a program that asks for
                // one has lost track of a buffer, so the data is dropped instead.
                if dst >= 0x2000 {
                    let data = self.disc_read(off, len);
                    for (i, b) in data.iter().enumerate() {
                        if let Some(d) = self.ram.get_mut(dst + i) {
                            *d = *b;
                        }
                    }
                }
                self.di[5] = self.di[5].wrapping_add(len as u32);
                self.di[6] = 0;
            }
            0x12 => {
                // Drive inquiry: revision/date block.
                let dst = (self.di[5] & 0x03FF_FFE0) as usize;
                let info = [0u8, 0, 0, 2, 0x20, 0x02, 0x04, 0x02, 0x61, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                for (i, b) in info.iter().enumerate() {
                    if let Some(d) = self.ram.get_mut(dst + i) {
                        *d = *b;
                    }
                }
                self.di[6] = 0;
            }
            0xE0 => self.di[8] = 0, // request error: none
            _ => {}
        }
        self.di[7] &= !1; // DICR: transfer finished
        self.di[0] |= 0x10; // DISR: TCINT
        self.update_di_irq();
    }

    /// The drive's interrupt is a level: a pending cause (break, transfer complete, error)
    /// whose enable bit is set.
    fn update_di_irq(&mut self) {
        let d = self.di[0];
        let pending = (d & 0x04 != 0 && d & 0x02 != 0) || (d & 0x10 != 0 && d & 0x08 != 0) || (d & 0x40 != 0 && d & 0x20 != 0);
        if pending {
            self.pi_intsr |= pi_bit::DI;
        } else {
            self.pi_intsr &= !pi_bit::DI;
        }
    }

    // ---------------------------------------------------------------- DSP, ARAM and audio

    fn dsp16(&self, off: usize) -> u32 {
        be(&self.dsp, off, 2) as u32
    }

    fn dsp_write16(&mut self, off: usize, v: u32) {
        match off {
            0x0A => {
                // DSP control: acknowledge interrupt bits written as 1; reset completes at once.
                let cur = self.dsp16(0x0A);
                let ack = v & (0x08 | 0x20 | 0x80);
                let mut n = (cur & !ack & (0x08 | 0x20 | 0x80)) | (v & !(0x08 | 0x20 | 0x80 | 0x01 | 0x800));
                n &= !0x01;
                set_be(&mut self.dsp, 0x0A, n as u64, 2);
                if v & 0x01 != 0 || (cur & 0x800 != 0 && v & 0x800 == 0) || (cur & 0x04 != 0 && v & 0x04 == 0) {
                    // After a reset the DSP's boot ROM posts 0x8071FEED to the CPU mailbox.
                    self.dsp_mail_out.clear();
                    self.dsp_mail_out.push_back(0x8071_FEED);
                    self.dsp_expect = false;
                }
                self.update_dsp_irq();
            }
            0x00 => set_be(&mut self.dsp, off, v as u64, 2),
            0x02 => {
                // The low half completes a mail; the (stand-in) DSP takes it at once.
                set_be(&mut self.dsp, off, v as u64, 2);
                let mail = (self.dsp16(0x00) << 16) | (v & 0xFFFF);
                // The DSP has read it: the "mail waiting" bit clears.
                let hi = self.dsp16(0x00) & 0x7FFF;
                set_be(&mut self.dsp, 0x00, hi as u64, 2);
                self.dsp_mail_in(mail);
            }
            0x28 => set_be(&mut self.dsp, off, v as u64, 2),
            0x2A => {
                set_be(&mut self.dsp, off, v as u64, 2);
                self.aram_dma();
            }
            0x36 => {
                set_be(&mut self.dsp, off, v as u64, 2);
                if v & 0x8000 != 0 && self.ai_pos == 0 {
                    // Audio DMA starts: the "block started" interrupt fires straight away.
                    self.raise_dsp(0x08);
                }
            }
            _ => set_be(&mut self.dsp, off, v as u64, 2),
        }
    }

    /// Next mail from the DSP: (high half with the valid bit, low half). Reading the low
    /// half takes the mail off the queue.
    fn dsp_mail_read(&mut self, o: usize, size: u32) -> u64 {
        let head = self.dsp_mail_out.front().copied();
        let hi = head.map_or(0, |m| (m >> 16) | 0x8000) & 0xFFFF;
        let lo = head.map_or(0, |m| m & 0xFFFF);
        match (o, size) {
            (4, 4) => {
                self.dsp_mail_out.pop_front();
                ((hi << 16) | lo) as u64
            }
            (4, _) => hi as u64,
            _ => {
                self.dsp_mail_out.pop_front();
                lo as u64
            }
        }
    }

    /// The audio processor's side of the boot handshake. A real DSP runs a program the game
    /// uploads; until that's emulated this answers the way the boot ROM and the audio
    /// microcode do, so the game's start-up proceeds.
    fn dsp_mail_in(&mut self, mail: u32) {
        if io_log() {
            eprintln!("DSP mail from CPU: {mail:08X}");
        }
        if self.dsp_cmdlist {
            // The audio microcode's command list for one 5 ms frame. The stand-in doesn't mix
            // yet (the game hears silence) but finishes the frame the way the real one does,
            // so the game's audio callbacks keep running.
            self.dsp_cmdlist = false;
            self.dsp_mail_out.push_back(0xDCD1_0002);
            self.raise_dsp(0x80);
            return;
        }
        if mail >> 16 == 0xBABE {
            self.dsp_cmdlist = true;
            return;
        }
        if self.dsp_expect {
            self.dsp_expect = false;
            if self.dsp_cmd == 0x80F3_D001 {
                // Initialisation vector: the uploaded program starts and announces itself.
                self.dsp_mail_out.push_back(0xDCD1_0000);
                self.raise_dsp(0x80);
            }
            return;
        }
        if mail >> 16 == 0x80F3 {
            self.dsp_expect = true;
            self.dsp_cmd = mail;
        }
    }

    fn raise_dsp(&mut self, bit: u32) {
        let v = self.dsp16(0x0A) | bit;
        set_be(&mut self.dsp, 0x0A, v as u64, 2);
        self.update_dsp_irq();
    }

    fn update_dsp_irq(&mut self) {
        let c = self.dsp16(0x0A);
        // Interrupt bit << 1 is its mask bit (AID 3/4, AR 5/6, DSP 7/8).
        let pending = (c & 0x08 != 0 && c & 0x10 != 0) || (c & 0x20 != 0 && c & 0x40 != 0) || (c & 0x80 != 0 && c & 0x100 != 0);
        if pending {
            self.pi_intsr |= pi_bit::DSP;
        } else {
            self.pi_intsr &= !pi_bit::DSP;
        }
    }

    fn aram_dma(&mut self) {
        let mm = (be(&self.dsp, 0x20, 4) as u32) & 0x03FF_FFE0;
        let ar = (be(&self.dsp, 0x24, 4) as u32) & 0x03FF_FFE0;
        let cnt = be(&self.dsp, 0x28, 4) as u32;
        let len = (cnt & 0x7FFF_FFE0) as usize;
        let to_ram = cnt & 0x8000_0000 != 0;
        for i in 0..len {
            let (m, a) = ((mm as usize + i) % RAM, (ar as usize + i) % ARAM);
            if to_ram {
                self.ram[m] = self.aram[a];
            } else {
                self.aram[a] = self.ram[m];
            }
        }
        set_be(&mut self.dsp, 0x28, (cnt & 0x8000_0000) as u64, 4);
        self.raise_dsp(0x20);
    }

    /// Plays the audio DMA buffer (16-bit big-endian stereo) for `cycles` CPU cycles.
    fn ai_tick(&mut self, cycles: u32) {
        // The sample counter runs whenever the audio interface is playing.
        if self.ai_cr & 1 != 0 {
            let rate = if self.ai_cr & 2 != 0 { 48_000.0 } else { 32_000.0 };
            self.ai_scnt_phase += cycles as f64 * rate / CPU_HZ as f64;
            let whole = self.ai_scnt_phase as u32;
            if whole > 0 {
                self.ai_scnt_phase -= whole as f64;
                let before = self.ai_scnt;
                self.ai_scnt = self.ai_scnt.wrapping_add(whole);
                if self.ai_cr & 0x14 == 0x14 && before < self.ai_it && self.ai_scnt >= self.ai_it {
                    self.ai_cr |= 8;
                    self.pi_intsr |= 1 << 5;
                }
            }
        }
        let ctrl = self.dsp16(0x36);
        if ctrl & 0x8000 == 0 {
            return;
        }
        let start = (be(&self.dsp, 0x30, 4) as u32) & 0x03FF_FFE0;
        let len = (ctrl & 0x7FFF) * 32;
        let rate = if self.ai_cr & 0x40 != 0 { 32_000.0 } else { 48_000.0 };
        self.ai_phase += cycles as f64 * rate / CPU_HZ as f64;
        while self.ai_phase >= 1.0 {
            self.ai_phase -= 1.0;
            let a = (start + self.ai_pos) as usize % RAM;
            let l = i16::from_be_bytes([self.ram[a], self.ram[a + 1]]) as f32 / 32768.0;
            let r = i16::from_be_bytes([self.ram[a + 2], self.ram[a + 3]]) as f32 / 32768.0;
            self.resample += self.out_rate as f64;
            while self.resample >= rate {
                self.resample -= rate;
                self.audio.push(r);
                self.audio.push(l);
            }
            self.ai_pos += 4;
            if self.ai_pos >= len.max(32) {
                self.ai_pos = 0;
                self.raise_dsp(0x08);
            }
            set_be(&mut self.dsp, 0x3A, ((len.saturating_sub(self.ai_pos)) / 32) as u64, 2);
        }
    }

    // ---------------------------------------------------------------- register dispatch

    fn io_read(&mut self, a: u32, size: u32) -> u64 {
        let off = (a & 0xFFFF) as usize;
        if io_log() && !io_writes_only() {
            eprintln!("R{size} {a:08X}");
        }
        match a & 0xFFFF_0000 {
            0x0C00_0000 => match off {
                0x0000..=0x0FFF => self.gx.cp_read(off) as u64,
                0x1000..=0x10FF => self.gx.pe_read(off - 0x1000) as u64,
                0x2000..=0x20FF => be(&self.vi, off - 0x2000, size),
                0x3000 => (self.pi_intsr | 1 << 16) as u64,
                0x3004 => self.pi_intmr as u64,
                0x300C..=0x3014 => self.pi_fifo[(off - 0x300C) / 4] as u64,
                0x302C => 0x2465_00B1, // console revision
                0x5000..=0x51FF => {
                    if off == 0x5004 || off == 0x5006 {
                        self.dsp_mail_read(off - 0x5000, size)
                    } else {
                        be(&self.dsp, off - 0x5000, size)
                    }
                }
                0x6000..=0x6027 => {
                    let i = (off - 0x6000) / 4;
                    if i == 1 {
                        // Cover status: closed with a disc inserted (bit 0 = open).
                        (self.di[1] & !1) as u64
                    } else {
                        self.di[i] as u64
                    }
                }
                0x6400..=0x643F => {
                    let i = (off - 0x6400) / 4;
                    let ch = i / 3;
                    match (i % 3, ch < 4 && i < 12) {
                        (1, true) => {
                            // Reading the input clears this channel's "new data" flag.
                            self.si[0x38 / 4] &= !(0x20 << ((3 - ch) * 8));
                            if self.si[0x38 / 4] & 0x2020_2020 == 0 {
                                self.si[13] &= !(1 << 28);
                                if self.si[13] & (1 << 31) == 0 {
                                    self.pi_intsr &= !pi_bit::SI;
                                }
                            }
                            self.pad_inbuf(ch).0 as u64
                        }
                        (2, true) => self.pad_inbuf(ch).1 as u64,
                        _ => self.si[i] as u64,
                    }
                }
                0x6480..=0x64FF => be(&self.si_buf, off - 0x6480, size),
                0x6800..=0x683B => {
                    let ch = (off - 0x6800) / 0x14;
                    let r = ((off - 0x6800) % 0x14) / 4;
                    let v = self.exi[ch][r];
                    // No devices: reads of the data register come back empty.
                    if r == 4 {
                        0xFFFF_FFFF
                    } else {
                        v as u64
                    }
                }
                0x6C00 => self.ai_cr as u64,
                0x6C08 => self.ai_scnt as u64,
                0x6C0C => self.ai_it as u64,
                _ => 0,
            },
            _ => 0,
        }
    }

    fn io_write(&mut self, a: u32, v: u64, size: u32) {
        let off = (a & 0xFFFF) as usize;
        if io_log() && !(0x0C00_8000..0x0C00_9000).contains(&a) {
            eprintln!("W{size} {a:08X} = {v:X}");
        }
        if a & 0xFFFF_0000 != 0x0C00_0000 {
            return;
        }
        match off {
            0x2000..=0x20FF => {
                let o = off - 0x2000;
                if (0x30..0x40).contains(&o) && size == 4 {
                    // Display interrupts: writing bit 31 as 0 acknowledges.
                    set_be(&mut self.vi, o, v, 4);
                } else if (0x30..0x40).contains(&o) && size == 2 && o % 4 == 0 {
                    set_be(&mut self.vi, o, v, 2);
                } else {
                    set_be(&mut self.vi, o, v, size);
                }
                // The display-configuration reset bit completes immediately.
                if (o..o + size as usize).contains(&0x03) {
                    self.vi[0x03] &= !0x02;
                }
                self.update_vi_irq();
            }
            0x0000..=0x0FFF => self.gx.cp_write(off, v as u16),
            0x1000..=0x10FF => {
                self.gx.pe_write(off - 0x1000, v as u16);
                self.update_gx_irq();
            }
            0x3000 => self.pi_intsr &= !(v as u32 & 0x3), // only the error/reset bits are write-to-clear
            0x3004 => self.pi_intmr = v as u32,
            0x300C..=0x3014 => self.pi_fifo[(off - 0x300C) / 4] = v as u32,
            0x5000..=0x51FF => {
                if size == 4 {
                    self.dsp_write16(off - 0x5000, (v >> 16) as u32);
                    self.dsp_write16(off - 0x5000 + 2, v as u32 & 0xFFFF);
                } else {
                    self.dsp_write16(off - 0x5000, v as u32);
                }
            }
            0x6000..=0x6027 => {
                let i = (off - 0x6000) / 4;
                let v = v as u32;
                match i {
                    0 => {
                        // DISR: interrupt bits (2, 4, 6) acknowledge on write.
                        let keep = self.di[0] & 0x54 & !(v & 0x54);
                        self.di[0] = (v & !0x54) | keep;
                        self.update_di_irq();
                    }
                    7 => {
                        self.di[7] = v;
                        if v & 1 != 0 {
                            self.di_command();
                        }
                    }
                    _ => self.di[i] = v,
                }
            }
            0x6400..=0x643F => {
                let i = (off - 0x6400) / 4;
                match i {
                    13 => {
                        // SICOMCSR: acknowledge interrupt bits, start a transfer.
                        let v = v as u32;
                        let mut cur = self.si[13] & !(v & 0xB000_0000 & 0x9000_0000);
                        if v & (1 << 31) != 0 {
                            cur &= !(1 << 31);
                            self.pi_intsr &= !pi_bit::SI;
                        }
                        cur = (cur & 0xB000_0000 & !(v & 0x9000_0000)) | (v & 0x4FFF_FFFF);
                        self.si[13] = cur;
                        if v & 1 != 0 {
                            self.si_transfer();
                        }
                    }
                    14 => self.si[14] &= !(v as u32),
                    _ => self.si[i] = v as u32,
                }
            }
            0x6480..=0x64FF => set_be(&mut self.si_buf, off - 0x6480, v, size),
            0x6800..=0x683B => {
                let ch = (off - 0x6800) / 0x14;
                let r = ((off - 0x6800) % 0x14) / 4;
                let v = v as u32;
                match r {
                    0 => {
                        // Status bits (1, 3, 11) acknowledge on write.
                        let keep = self.exi[ch][0] & 0x80A & !(v & 0x80A);
                        self.exi[ch][0] = (v & !0x80A) | keep;
                    }
                    3 => {
                        self.exi[ch][3] = v & !1;
                        if v & 1 != 0 {
                            self.exi[ch][0] |= 8; // transfer complete
                            if self.exi[ch][0] & 4 != 0 {
                                self.pi_intsr |= pi_bit::EXI;
                            }
                        }
                    }
                    _ => self.exi[ch][r] = v,
                }
                if !self.exi.iter().any(|c| c[0] & 0x0C == 0x0C) {
                    self.pi_intsr &= !pi_bit::EXI;
                }
            }
            0x6C00 => {
                let v = v as u32;
                if v & 0x20 != 0 {
                    self.ai_scnt = 0;
                    self.ai_scnt_phase = 0.0;
                }
                // The interrupt status bit clears when written as 1.
                self.ai_cr = (v & !0x28) | (self.ai_cr & 8 & !v);
                if self.ai_cr & 8 == 0 {
                    self.pi_intsr &= !(1 << 5);
                }
            }
            0x6C0C => self.ai_it = v as u32,
            0x8000..=0x8FFF => {
                // Write-gather pipe: graphics commands for the GX FIFO.
                let mut bytes = [0u8; 8];
                for i in 0..size as usize {
                    bytes[i] = (v >> (8 * (size as usize - 1 - i))) as u8;
                }
                self.gx_count += size as usize;
                self.gx.feed(&mut self.ram, &bytes[..size as usize]);
                self.update_gx_irq();
                self.pi_fifo[2] = self.pi_fifo[2].wrapping_add(size);
            }
            _ => {}
        }
    }
}

/// Set GC_IO_LOG=1 to print every hardware register access (debugging aid).
fn io_writes_only() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("GC_IO_LOG").map(|v| v == "w").unwrap_or(false))
}

fn io_log() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GC_IO_LOG").is_some())
}

impl Bus for Hw {
    fn ram(&mut self) -> Option<&mut [u8]> {
        Some(&mut self.ram)
    }

    fn read(&mut self, a: u32, size: u32) -> u64 {
        match a {
            0x0000_0000..=0x017F_FFFF => be(&self.ram, a as usize, size),
            0x0C00_0000..=0x0C00_FFFF => self.io_read(a, size),
            0xE000_0000..=0xE000_3FFF => be(&self.aram[..0x4000], (a & 0x3FFF) as usize, size), // locked L1 cache
            _ => 0,
        }
    }

    fn write(&mut self, a: u32, v: u64, size: u32) {
        match a {
            0x0000_0000..=0x017F_FFFF => {
                if self.watch != 0 && a <= (self.watch & 0x01FF_FFFF) && (self.watch & 0x01FF_FFFF) < a + size {
                    self.watch_hit = Some(v);
                }
                set_be(&mut self.ram, a as usize, v, size)
            }
            0x0C00_0000..=0x0C00_FFFF => self.io_write(a, v, size),
            0xE000_0000..=0xE000_3FFF => set_be(&mut self.aram[..0x4000], (a & 0x3FFF) as usize, v, size),
            _ => {}
        }
    }
}

pub struct GameCube {
    cpu: Cpu<Hw>,
    frame: Vec<u32>,
    size: (usize, usize),
    irq: bool,
    frames: u64,
    /// How many times fewer instructions than the real chip we run per frame.
    scale: u32,
    /// When set, the instruction budget follows the host's speed (see `adapt`).
    adaptive: bool,
    frame_timer: (f64, u32),
    watch_log: Vec<(u64, u32, u32, u64)>,
    watch_last: u32,
    sampling: bool,
    break_on_null: bool,
    stopped: bool,
    pc_hist: std::collections::BTreeMap<u32, u32>,
    pc_watch: Vec<u32>,
    pc_log: Vec<(u64, u32, u32, u64)>,
}

impl GameCube {
    /// Boots a .dol executable (homebrew or a game's main program).
    pub fn from_dol(dol: &[u8]) -> Result<GameCube, String> {
        if dol.len() < 0x100 {
            return Err("This file is too small to be a GameCube program (.dol).".into());
        }
        let w = |o: usize| u32::from_be_bytes([dol[o], dol[o + 1], dol[o + 2], dol[o + 3]]);
        let mut hw = Hw::new();
        // Clear BSS first: its range often overlaps the small-data sections, whose contents
        // must survive.
        let (bss, bss_size) = ((w(0xD8) & 0x01FF_FFFF) as usize, w(0xDC) as usize);
        if bss + bss_size <= RAM {
            hw.ram[bss..bss + bss_size].fill(0);
        }
        for i in 0..18 {
            let (off, addr, size) = (w(i * 4), w(0x48 + i * 4), w(0x90 + i * 4));
            if size == 0 {
                continue;
            }
            let (off, size) = (off as usize, size as usize);
            if off + size > dol.len() {
                return Err("The .dol file is damaged (a section runs past the end).".into());
            }
            let pa = (addr & 0x01FF_FFFF) as usize;
            if pa + size > RAM {
                return Err("The .dol file loads outside main memory.".into());
            }
            hw.ram[pa..pa + size].copy_from_slice(&dol[off..off + size]);
        }
        let entry = w(0xE0);
        // Globals the boot ROM leaves in low memory.
        set_be(&mut hw.ram, 0x20, 0x0D15_EA5E, 4);
        set_be(&mut hw.ram, 0x24, 1, 4);
        set_be(&mut hw.ram, 0x28, RAM as u64, 4);
        set_be(&mut hw.ram, 0x2C, 1, 4); // retail console
        set_be(&mut hw.ram, 0x30, 0, 4); // arena low (0 = use the program's end)
        set_be(&mut hw.ram, 0x34, 0x817F_E000, 4); // arena high
        set_be(&mut hw.ram, 0xCC, 0, 4); // NTSC
        set_be(&mut hw.ram, 0xF0, RAM as u64, 4);
        set_be(&mut hw.ram, 0xF8, 162_000_000, 4); // bus clock
        set_be(&mut hw.ram, 0xFC, CPU_HZ as u64, 4);
        // The video interface starts switched off, so the program's video setup programs it
        // (including the retrace interrupts) from scratch.
        set_be(&mut hw.vi, 0x00, 0x0F06, 2);
        let mut cpu = Cpu::new(hw);
        // We run about a sixth of the real instruction rate (see run_frame), so the time base
        // ticks every 2 instructions instead of every 12 to keep real-time pace.
        cpu.set_tb_divisor(1);
        cpu.set_tb_multiplier((SCALE / 12).max(1));
        let mut r = Regs::default();
        r.pc = entry;
        r.msr = 0x0000_2032; // FP, IR, DR, RI
        r.r[1] = 0x816F_FFF0;
        cpu.set_regs(r);
        // Block address translation as set up by the boot ROM.
        for (spr, v) in [(528, 0x8000_1FFF), (529, 0x0000_0002), (530, 0x0000_0000), (531, 0x0000_0000), (536, 0x8000_1FFF), (537, 0x0000_0002), (538, 0xC000_1FFF), (539, 0x0000_002A)] {
            cpu.set_spr(spr, v);
        }
        cpu.set_spr(920, 0xE000_0000); // HID2: paired singles, locked cache
        cpu.set_spr(1008, 0x0011_0C64); // HID0
        Ok(GameCube { cpu, frame: vec![0; 640 * 480], size: (640, 480), irq: false, frames: 0, scale: SCALE, adaptive: false, frame_timer: (0.0, 0), watch_log: Vec::new(), watch_last: 0, sampling: false, break_on_null: false, stopped: false, pc_hist: Default::default(), pc_watch: Vec::new(), pc_log: Vec::new() })
    }

    /// Boots a disc image (.iso / .gcm): loads the game's main program and file table the way
    /// the boot ROM and the disc's apploader would, then runs it with the drive attached.
    pub fn from_disc(path: &Path) -> Result<GameCube, String> {
        let mut f = File::open(path).map_err(|e| format!("Can't open {}: {e}", path.display()))?;
        let mut hdr = vec![0u8; 0x440];
        f.read_exact(&mut hdr).map_err(|_| "This disc image is too small.".to_string())?;
        if u32::from_be_bytes([hdr[0x1C], hdr[0x1D], hdr[0x1E], hdr[0x1F]]) != 0xC233_9F3D {
            return Err("This doesn't look like a GameCube disc image (missing disc magic).".into());
        }
        let w = |o: usize| u32::from_be_bytes([hdr[o], hdr[o + 1], hdr[o + 2], hdr[o + 3]]);
        let (dol_off, fst_off, fst_size, fst_max) = (w(0x420) as u64, w(0x424) as u64, w(0x428) as usize, w(0x42C) as usize);
        // The main program's size is the furthest section end in its header.
        let mut dh = vec![0u8; 0x100];
        f.seek(SeekFrom::Start(dol_off)).map_err(|e| e.to_string())?;
        f.read_exact(&mut dh).map_err(|e| e.to_string())?;
        let dw = |o: usize| u32::from_be_bytes([dh[o], dh[o + 1], dh[o + 2], dh[o + 3]]) as usize;
        let dol_len = (0..18).map(|i| dw(i * 4) + dw(0x90 + i * 4)).max().unwrap_or(0x100).max(0x100);
        let mut dol = vec![0u8; dol_len];
        f.seek(SeekFrom::Start(dol_off)).map_err(|e| e.to_string())?;
        f.read_exact(&mut dol).map_err(|_| "The disc's main program is damaged.".to_string())?;
        let mut gc = GameCube::from_dol(&dol)?;
        let hw = gc.cpu.bus_mut();
        hw.disc = Some(f);
        // Disc ID block at the start of memory.
        hw.ram[..0x20].copy_from_slice(&hdr[..0x20]);
        // File system table at the top of memory, as the apploader leaves it.
        let fst_addr = (0x0180_0000 - fst_max.max(fst_size) - 0x20) & !0x1F;
        let fst = hw.disc_read(fst_off, fst_size);
        hw.ram[fst_addr..fst_addr + fst_size].copy_from_slice(&fst);
        set_be(&mut hw.ram, 0x38, (0x8000_0000 | fst_addr) as u64, 4);
        set_be(&mut hw.ram, 0x3C, fst_max as u64, 4);
        set_be(&mut hw.ram, 0x34, (0x8000_0000 | fst_addr) as u64, 4);
        Ok(gc)
    }

    /// Game title from the disc header (empty for .dol programs).
    pub fn disc_title(&mut self) -> String {
        let hw = self.cpu.bus_mut();
        if hw.disc.is_none() {
            return String::new();
        }
        let b = hw.disc_read(0x20, 0x3E0);
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).trim().to_string()
    }

    /// Runs one video field (1/60 s).
    /// Diagnostics: watch writes to a RAM word; see `watch_log`.
    pub fn watch(&mut self, addr: u32) {
        self.cpu.bus_mut().watch = addr;
        self.watch_last = self.peek(addr);
    }

    /// Diagnostics: record (frame, pc, lr, r3) each time one of these instructions executes.
    pub fn watch_pc_list(&mut self, pcs: &[u32]) {
        self.pc_watch = pcs.to_vec();
    }

    /// Diagnostics: stop `run_frame` the moment a disc read is started with a NULL-ish destination.
    pub fn break_on_null_read(&mut self, on: bool) {
        self.break_on_null = on;
    }

    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Diagnostics: start/stop sampling the PC (every ~200 instructions) into a histogram.
    pub fn sample_pcs(&mut self, on: bool) {
        self.sampling = on;
        if on {
            self.pc_hist.clear();
        }
    }

    pub fn pc_histogram(&self) -> Vec<(u32, u32)> {
        let mut v: Vec<_> = self.pc_hist.iter().map(|(a, b)| (*a, *b)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }

    pub fn pc_log(&self) -> &[(u64, u32, u32, u64)] {
        &self.pc_log
    }

    /// Diagnostics: (frame, pc after the write, lr, value) for each watched write.
    pub fn watch_log(&self) -> &[(u64, u32, u32, u64)] {
        &self.watch_log
    }

    /// Lets the core trade emulated CPU speed for frame rate: a slow host runs the game's
    /// processor slower (half at worst) to hold 60 frames a second, a fast one runs it at
    /// full emulation speed.
    pub fn set_adaptive(&mut self, on: bool) {
        self.adaptive = on;
    }

    fn adapt(&mut self, took: f64) {
        self.frame_timer.0 += took;
        self.frame_timer.1 += 1;
        if self.frame_timer.1 < 20 {
            return;
        }
        let ms = self.frame_timer.0 * 1000.0 / 20.0;
        self.frame_timer = (0.0, 0);
        let next = if ms > 15.5 && self.scale < 24 {
            self.scale + 12
        } else if ms < 8.5 && self.scale > 12 {
            self.scale - 12
        } else {
            self.scale
        };
        if next != self.scale {
            self.scale = next;
            self.cpu.set_tb_multiplier((next / 12).max(1));
        }
    }

    pub fn run_frame(&mut self) {
        let t = std::time::Instant::now();
        self.run_frame_inner();
        if self.adaptive {
            self.adapt(t.elapsed().as_secs_f64());
        }
    }

    fn run_frame_inner(&mut self) {
        self.frames += 1;
        self.cpu.bus_mut().si_poll();
        // Budget per line in CPU cycles; each instruction counts several cycles because an
        // interpreter can't sustain the real 486 MHz.
        const LINES: u32 = 263;
        let scale = self.scale;
        let per_line = CPU_HZ / 60 / LINES / self.scale;
        for _ in 0..LINES {
            let mut left = per_line as i64;
            while left > 0 {
                let watching = self.cpu.bus().watch != 0 || !self.pc_watch.is_empty() || self.break_on_null;
                let before = if watching { self.cpu.regs().pc } else { 0 };
                let c = self.cpu.run(if watching { 1 } else { (left as u32).min(200) });
                if self.sampling {
                    *self.pc_hist.entry(self.cpu.regs().pc & !0xF).or_insert(0) += 1;
                }
                if self.break_on_null && self.cpu.bus().di_null {
                    self.stopped = true;
                    return;
                }
                if watching && !self.pc_watch.is_empty() {
                    let r = self.cpu.regs();
                    if self.pc_watch.contains(&r.pc) {
                        self.pc_log.push((self.frames, r.pc, r.lr, r.r[3] as u64));
                    }
                }
                if watching && self.cpu.bus().watch != 0 {
                    let (addr, now) = {
                        let hw = self.cpu.bus();
                        let a = (hw.watch & 0x01FF_FFFF) as usize;
                        (a, u32::from_be_bytes([hw.ram[a], hw.ram[a + 1], hw.ram[a + 2], hw.ram[a + 3]]))
                    };
                    let _ = addr;
                    if now != self.watch_last {
                        let r = self.cpu.regs();
                        // The address of the instruction that made the change.
                        self.watch_log.push((self.frames, before, r.lr, now as u64));
                        self.watch_last = now;
                    }
                }
                let hw = self.cpu.bus_mut();
                hw.ai_tick(c * scale);
                left -= c as i64;
                let irq = hw.irq();
                if irq != self.irq {
                    self.irq = irq;
                    self.cpu.set_irq(irq);
                }
            }
            self.cpu.bus_mut().vi_line();
            let irq = self.cpu.bus().irq();
            if irq != self.irq {
                self.irq = irq;
                self.cpu.set_irq(irq);
            }
        }
        self.render();
    }

    /// Converts the YUV 4:2:2 framebuffer the video interface scans out into RGB.
    fn render(&mut self) {
        let hw = self.cpu.bus();
        let tfbl = hw.vi32(0x1C);
        let base = if tfbl & (1 << 28) != 0 { (tfbl & 0x00FF_FFFF) << 5 } else { tfbl & 0x00FF_FFFF } as usize;
        let picconf = hw.vi16(0x48);
        let width = (((picconf >> 8) & 0x7F) as usize * 16).clamp(0, 720);
        let width = if width == 0 { 640 } else { width };
        let interlaced = hw.vi16(0x02) & 4 == 0;
        let acv = ((hw.vi16(0x00) >> 4) & 0x3FF) as usize;
        let lines = if acv == 0 { 240 } else { acv };
        // The framebuffer holds whole frames with lines packed back to back.
        let h = if interlaced { lines * 2 } else { lines };
        let line_bytes = width * 2;
        let h = h.min(576);
        if self.size != (width, h) {
            self.size = (width, h);
            self.frame = vec![0; width * h];
        }
        if base == 0 || base >= RAM {
            self.frame.fill(0);
            return;
        }
        let clamp = |v: f32| v.clamp(0.0, 255.0) as u32;
        for y in 0..h {
            let row = base + y * line_bytes;
            for x in (0..width).step_by(2) {
                let a = row + x * 2;
                if a + 4 > RAM {
                    break;
                }
                let (y0, u, y1, v) = (hw.ram[a] as f32, hw.ram[a + 1] as f32 - 128.0, hw.ram[a + 2] as f32, hw.ram[a + 3] as f32 - 128.0);
                for (i, yy) in [y0, y1].into_iter().enumerate() {
                    let yy = 1.164 * (yy - 16.0);
                    let r = clamp(yy + 1.596 * v);
                    let g = clamp(yy - 0.392 * u - 0.813 * v);
                    let b = clamp(yy + 2.017 * u);
                    self.frame[y * width + x + i] = r << 16 | g << 8 | b;
                }
            }
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    pub fn set_pad(&mut self, port: usize, pad: Pad) {
        if port < 4 {
            self.cpu.bus_mut().pads[port] = pad;
        }
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.cpu.bus_mut().out_rate = rate;
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.cpu.bus_mut().audio);
    }

    pub fn pc(&self) -> u32 {
        self.cpu.regs().pc
    }

    /// Interrupt and video state for diagnostics.
    pub fn debug(&self) -> String {
        let hw = self.cpu.bus();
        let r = self.cpu.regs();
        format!(
            "intsr={:08X} intmr={:08X} msr={:08X} dec={:08X} DI0={:08X} DI1={:08X} DCR={:04X} vline={} TFBL={:08X}",
            hw.pi_intsr, hw.pi_intmr, r.msr, self.cpu.spr(22), hw.vi32(0x30), hw.vi32(0x34), hw.vi16(0x02), hw.vline, hw.vi32(0x1C)
        )
    }

    /// Diagnostics: runs `n` single instructions and returns (pc, opcode) for each.
    pub fn trace(&mut self, n: usize) -> Vec<(u32, u32)> {
        let mut v = Vec::new();
        for _ in 0..n {
            let pc = self.cpu.regs().pc;
            let a = (pc & 0x01FF_FFFF) as usize;
            let w = u32::from_be_bytes([self.cpu.bus().ram[a], self.cpu.bus().ram[a + 1], self.cpu.bus().ram[a + 2], self.cpu.bus().ram[a + 3]]);
            v.push((pc, w));
            self.cpu.run(1);
        }
        v
    }

    /// Diagnostics: runs `n` instructions, counting how many ran with external interrupts on.
    pub fn ee_stats(&mut self, n: usize) -> (u32, u32, u32) {
        let (mut on, mut vecs, mut last) = (0, 0, 0);
        for _ in 0..n {
            let r = self.cpu.regs();
            if r.msr & 0x8000 != 0 {
                on += 1;
            }
            if r.pc == 0x8000_0500 {
                vecs += 1;
            }
            last = r.msr;
            self.cpu.run(1);
        }
        (on, vecs, last)
    }

    /// Diagnostics: single-steps `n` instructions and reports each time the PC is in `pcs`:
    /// (step number, pc, r3, r4, r5, lr, msr).
    pub fn watch_pcs(&mut self, pcs: &[u32], n: u64) -> Vec<(u64, u32, u32, u32, u32, u32, u32)> {
        let mut out = Vec::new();
        for i in 0..n {
            let r = self.cpu.regs();
            let hit = if pcs.is_empty() {
                // Every mtmsr / rfi.
                let a = (r.pc & 0x01FF_FFFF) as usize;
                let ram = &self.cpu.bus().ram;
                let w = u32::from_be_bytes([ram[a], ram[a + 1], ram[a + 2], ram[a + 3]]);
                (w & 0xFC00_07FE == 0x7C00_0124 && r.r[((w >> 21) & 31) as usize] & 0x8000 != 0) || w == 0x4C00_0064
            } else {
                pcs.contains(&r.pc)
            };
            if hit {
                let a = (r.pc & 0x01FF_FFFF) as usize;
                let ram = &self.cpu.bus().ram;
                let w = u32::from_be_bytes([ram[a], ram[a + 1], ram[a + 2], ram[a + 3]]);
                let rs = r.r[((w >> 21) & 31) as usize];
                out.push((i, r.pc, if pcs.is_empty() { rs } else { r.r[3] }, r.r[4], r.r[5], r.lr, r.msr));
                if out.len() >= 200000 {
                    break;
                }
            }
            self.cpu.run(1);
        }
        out
    }

    /// Diagnostics: single-steps and reports each change of the word at `addr`:
    /// (step, pc before the instruction, old, new, lr).
    pub fn watch_mem(&mut self, addr: u32, n: u64, max: usize) -> Vec<(u64, u32, u32, u32, u32)> {
        let mut out = Vec::new();
        let mut last = self.peek(addr);
        for i in 0..n {
            let r = self.cpu.regs();
            self.cpu.run(1);
            let now = self.peek(addr);
            if now != last {
                out.push((i, r.pc, last, now, r.lr));
                last = now;
                if out.len() >= max {
                    break;
                }
            }
        }
        out
    }

    /// Diagnostics: general registers and a word of memory.
    pub fn gpr(&self, i: usize) -> u32 {
        self.cpu.regs().r[i]
    }

    pub fn peek(&self, addr: u32) -> u32 {
        let a = (addr & 0x01FF_FFFF) as usize;
        let r = &self.cpu.bus().ram;
        u32::from_be_bytes([r[a], r[a + 1], r[a + 2], r[a + 3]])
    }

    /// Diagnostics: the graphics chip's frame buffer as (width, height, pixels).
    pub fn efb(&self) -> (usize, usize, &[u32]) {
        (gx::EFB_W, gx::EFB_H, &self.cpu.bus().gx.efb_c)
    }

    /// Diagnostics: how triangles fared: [culled, clipped away, off screen, pixels, depth-rejected, alpha-rejected].
    pub fn gx_fate(&self) -> [u64; 6] {
        self.cpu.bus().gx.fate
    }

    /// Diagnostics: main memory.
    pub fn ram(&self) -> &[u8] {
        &self.cpu.bus().ram
    }

    /// Diagnostics: the last EFB->XFB copy and the video interface's view of the framebuffer.
    pub fn video_debug(&self) -> String {
        let hw = self.cpu.bus();
        let (d, s, w, h) = hw.gx.last_copy;
        format!(
            "last copy dst={d:#x} stride={s} {w}x{h}; VI: TFBL={:08X} TFBR={:08X} picconf={:04X} dcr={:04X} acv={:04X} width={} size={:?}",
            hw.vi32(0x1C),
            hw.vi32(0x24),
            hw.vi16(0x48),
            hw.vi16(0x02),
            hw.vi16(0x00),
            ((hw.vi16(0x48) >> 8) & 0x7F) * 16,
            self.size
        )
    }

    /// Graphics chip counters: (draw calls, triangles, EFB copies, unknown opcodes, last unknown).
    pub fn gx_stats(&self) -> (u64, u64, u64, u32, u32) {
        let g = &self.cpu.bus().gx;
        (g.draws, g.triangles, g.copies, g.unknown[0], g.unknown[1])
    }

    /// Bytes written to the graphics FIFO so far (diagnostics until GX is emulated).
    pub fn gx_bytes(&self) -> usize {
        self.cpu.bus().gx_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny .dol that fills the framebuffer with one colour, points VI at it and loops.
    fn test_dol() -> Vec<u8> {
        let mut d = vec![0u8; 0x100];
        let code_off = 0x100u32;
        let d_ = |op: u32, rd: u32, ra: u32, imm: u32| op << 26 | rd << 21 | ra << 16 | (imm & 0xFFFF);
        let code = [
            d_(15, 3, 0, 0x8010), // lis r3,0x8010 (framebuffer 0x00100000)
            d_(15, 4, 0, 0x2080), // lis r4,0x2080 : YUYV = Y 0x20, U 0x80 ...
            d_(24, 4, 4, 0x2080), // ori r4,r4,0x2080 -> black-ish 0x20802080
            d_(14, 5, 0, 0x100),  // li r5,256 words
            d_(36, 4, 3, 0),      // loop: stw r4,0(r3)
            d_(14, 3, 3, 4),      // addi r3,r3,4
            d_(14, 5, 5, 0xFFFF), // addi r5,r5,-1
            d_(11, 0, 5, 0),      // cmpwi r5,0
            16 << 26 | 4 << 21 | 2 << 16 | (0xFFF0 & 0xFFFC), // bne loop (-16)
            d_(15, 6, 0, 0xCC00), // lis r6,0xCC00
            d_(15, 7, 0, 0x1000), // lis r7,0x1000 (POFF)
            d_(24, 7, 7, 0x8000), // ori r7,r7,0x8000 -> 0x10008000 = 0x100000 >> 5
            d_(36, 7, 6, 0x201C), // stw r7,0x201C(r6)  TFBL
            18 << 26,             // b .
        ];
        for (i, w) in code.iter().enumerate() {
            d.extend_from_slice(&w.to_be_bytes());
            let _ = i;
        }
        d[0..4].copy_from_slice(&code_off.to_be_bytes());
        d[0x48..0x4C].copy_from_slice(&0x8000_3100u32.to_be_bytes());
        d[0x90..0x94].copy_from_slice(&((code.len() * 4) as u32).to_be_bytes());
        d[0xE0..0xE4].copy_from_slice(&0x8000_3100u32.to_be_bytes());
        d
    }

    #[test]
    fn dol_boots_and_video_scans_out_its_framebuffer() {
        let mut g = GameCube::from_dol(&test_dol()).unwrap();
        g.run_frame();
        assert_eq!(g.size(), (640, 480));
        let px = g.frame()[0];
        // Y=0x20 with neutral chroma is a dark grey, not black and not garbage.
        let (r, gr, b) = (px >> 16 & 255, px >> 8 & 255, px & 255);
        assert!(r == gr && gr == b && (15..25).contains(&r), "got {px:06X}");
        // Past the filled area RAM is zero, which in YUV is a green, not the grey we wrote.
        assert_ne!(g.frame()[640 * 400], px);
    }

    #[test]
    fn controller_poll_format() {
        let mut hw = Hw::new();
        hw.pads[0] = Pad { buttons: button::A | button::START, stick: (10, -20), cstick: (0, 0), triggers: (0, 255) };
        let (h, l) = hw.pad_inbuf(0);
        assert_eq!(h >> 16, (button::A | button::START) as u32 | 0x80);
        assert_eq!((h >> 8) & 0xFF, 138);
        assert_eq!(h & 0xFF, 108);
        assert_eq!(l & 0xFF, 255);
    }
}
