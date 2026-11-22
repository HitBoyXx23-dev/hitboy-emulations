//! Sony PlayStation on our own cores: R3000A CPU (C++), GTE, GPU, SPU, CD-ROM, DMA,
//! timers, interrupt controller, controllers and memory cards.
//!
//! Booting needs a BIOS image dumped from the user's own console.

pub mod cdrom;
pub mod disc;
pub mod gpu;
pub mod mdec;
pub mod gte;
pub mod spu;

use cpur3000::{Bus, Cpu};
use std::path::Path;

pub const CPU_HZ: u32 = 33_868_800;
pub const MEMCARD_SIZE: usize = 128 * 1024;

/// Controller buttons (bit set = pressed).
pub mod button {
    pub const SELECT: u16 = 1 << 0;
    pub const L3: u16 = 1 << 1;
    pub const R3: u16 = 1 << 2;
    pub const START: u16 = 1 << 3;
    pub const UP: u16 = 1 << 4;
    pub const RIGHT: u16 = 1 << 5;
    pub const DOWN: u16 = 1 << 6;
    pub const LEFT: u16 = 1 << 7;
    pub const L2: u16 = 1 << 8;
    pub const R2: u16 = 1 << 9;
    pub const L1: u16 = 1 << 10;
    pub const R1: u16 = 1 << 11;
    pub const TRIANGLE: u16 = 1 << 12;
    pub const CIRCLE: u16 = 1 << 13;
    pub const CROSS: u16 = 1 << 14;
    pub const SQUARE: u16 = 1 << 15;
}

// ---------------------------------------------------------------------- timers

#[derive(Default, Clone, Copy, state::Stateful)]
struct Timer {
    count: u32,
    mode: u32,
    target: u32,
    fired: bool,
}

impl Timer {
    fn write_mode(&mut self, v: u32) {
        self.mode = (v & 0x3FF) | 0x400;
        self.count = 0;
        self.fired = false;
    }

    fn read_mode(&mut self) -> u32 {
        let m = self.mode;
        self.mode &= !(0x1800);
        m
    }

    /// Adds `n` ticks; returns true when an interrupt fires.
    fn advance(&mut self, n: u32) -> bool {
        if n == 0 {
            return false;
        }
        let mut irq = false;
        let old = self.count;
        let mut c = old + n;
        let t = self.target;
        if old < t && c >= t || (t == 0 && old == 0) {
            self.mode |= 1 << 11;
            irq |= self.mode & 0x10 != 0;
        }
        if self.mode & 8 != 0 && t > 0 && c > t {
            c %= t + 1;
        }
        if c > 0xFFFF {
            self.mode |= 1 << 12;
            irq |= self.mode & 0x20 != 0;
            c &= 0xFFFF;
        }
        self.count = c;
        if irq && (self.mode & 0x40 != 0 || !self.fired) {
            self.fired = true;
            if self.mode & 0x80 != 0 {
                self.mode ^= 0x400;
                return self.mode & 0x400 == 0;
            }
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------- controller port

#[derive(Clone, Copy, PartialEq)]
enum Dev {
    None,
    Pad(u8),
    Card(u8),
    Done,
}

impl state::Stateful for Dev {
    fn sync(&mut self, s: &mut state::State) {
        let (mut tag, mut n) = match *self {
            Dev::None => (0u8, 0u8),
            Dev::Pad(n) => (1, n),
            Dev::Card(n) => (2, n),
            Dev::Done => (3, 0),
        };
        tag.sync(s);
        n.sync(s);
        if s.is_load() {
            *self = match tag {
                0 => Dev::None,
                1 => Dev::Pad(n),
                2 => Dev::Card(n),
                3 => Dev::Done,
                _ => {
                    s.fail();
                    Dev::None
                }
            };
        }
    }
}

#[derive(state::Stateful)]
struct Sio {
    ctrl: u16,
    mode: u16,
    baud: u16,
    rx: Option<u8>,
    ack: bool,
    irq: bool,
    irq_delay: i32,
    /// Cycles left of the /ACK pulse the device holds low after a byte (the DSR bit).
    ack_hold: i32,
    dev: Dev,
    pub buttons: [u16; 2],
    pub cards: [Vec<u8>; 2],
    card_flag: [u8; 2],
    card_addr: u16,
    card_cmd: u8,
    card_sum: u8,
    card_last: u8,
    pub card_dirty: bool,
}

impl Sio {
    fn new() -> Self {
        Sio {
            ctrl: 0,
            mode: 0,
            baud: 0,
            rx: None,
            ack: false,
            irq: false,
            irq_delay: 0,
            ack_hold: 0,
            dev: Dev::None,
            buttons: [0; 2],
            cards: [formatted_card(), formatted_card()],
            card_flag: [0x08; 2],
            card_addr: 0,
            card_cmd: 0,
            card_sum: 0,
            card_last: 0,
            card_dirty: false,
        }
    }

    fn port(&self) -> usize {
        ((self.ctrl >> 13) & 1) as usize
    }

    fn write_data(&mut self, v: u8) {
        if self.ctrl & 2 == 0 {
            self.rx = Some(0xFF);
            return;
        }
        let (resp, ack) = self.transfer(v);
        self.rx = Some(resp);
        if ack {
            self.irq_delay = 450;
        }
    }

    fn transfer(&mut self, v: u8) -> (u8, bool) {
        let p = self.port();
        match self.dev {
            Dev::None => match v {
                0x01 => {
                    self.dev = Dev::Pad(0);
                    (0xFF, true)
                }
                0x81 => {
                    self.dev = Dev::Card(0);
                    (0xFF, true)
                }
                _ => (0xFF, false),
            },
            Dev::Pad(step) => {
                let b = !self.buttons[p];
                self.dev = if step >= 3 { Dev::Done } else { Dev::Pad(step + 1) };
                match step {
                    0 => (0x41, true),
                    1 => (0x5A, true),
                    2 => (b as u8, true),
                    _ => ((b >> 8) as u8, false),
                }
            }
            Dev::Card(step) => self.card(step, v, p),
            Dev::Done => (0xFF, false),
        }
    }

    fn card(&mut self, step: u8, v: u8, p: usize) -> (u8, bool) {
        let next = |s: &mut Self| s.dev = Dev::Card(step.wrapping_add(1));
        if step == 0 {
            self.card_cmd = v;
            if v != b'R' && v != b'W' {
                self.dev = Dev::Done;
                return (self.card_flag[p], false);
            }
            next(self);
            return (self.card_flag[p], true);
        }
        let read = self.card_cmd == b'R';
        let sector = |a: u16| (a as usize & 0x3FF) * 128;
        let r = match (read, step) {
            (_, 1) => 0x5A,
            (_, 2) => 0x5D,
            (_, 3) => {
                self.card_addr = (v as u16) << 8;
                self.card_last = v;
                0x00
            }
            (_, 4) => {
                self.card_addr |= v as u16;
                self.card_sum = (self.card_addr >> 8) as u8 ^ v;
                let prev = self.card_last;
                self.card_last = v;
                prev
            }
            (true, 5) => 0x5C,
            (true, 6) => 0x5D,
            (true, 7) => (self.card_addr >> 8) as u8,
            (true, 8) => self.card_addr as u8,
            (true, 9..=136) => {
                let b = self.cards[p][sector(self.card_addr) + (step - 9) as usize];
                self.card_sum ^= b;
                b
            }
            (true, 137) => self.card_sum,
            (true, _) => {
                self.dev = Dev::Done;
                return (0x47, false);
            }
            (false, 5..=132) => {
                let i = sector(self.card_addr) + (step - 5) as usize;
                self.cards[p][i] = v;
                self.card_sum ^= v;
                let prev = self.card_last;
                self.card_last = v;
                prev
            }
            (false, 133) => self.card_last,
            (false, 134) => 0x5C,
            (false, 135) => 0x5D,
            (false, _) => {
                self.card_flag[p] &= !0x08;
                self.card_dirty = true;
                self.dev = Dev::Done;
                return (0x47, false);
            }
        };
        next(self);
        (r, true)
    }

    fn stat(&self) -> u32 {
        5 | (self.rx.is_some() as u32) << 1 | (self.ack as u32) << 7 | (self.irq as u32) << 9
    }

    fn write_ctrl(&mut self, v: u16) {
        if v & 0x10 != 0 {
            self.irq = false;
            self.ack = false;
        }
        if v & 0x40 != 0 {
            self.ctrl = 0;
            self.mode = 0;
            self.rx = None;
            self.dev = Dev::None;
            return;
        }
        if v & 2 == 0 {
            self.dev = Dev::None;
        }
        self.ctrl = v & !0x50;
    }

    /// Returns true when the /ACK interrupt fires.
    fn tick(&mut self, cycles: u32) -> bool {
        if self.ack_hold > 0 {
            self.ack_hold -= cycles as i32;
            if self.ack_hold <= 0 {
                self.ack = false;
            }
        }
        if self.irq_delay > 0 {
            self.irq_delay -= cycles as i32;
            if self.irq_delay <= 0 {
                self.ack = true;
                self.ack_hold = 100;
                if self.ctrl & 0x1000 != 0 {
                    self.irq = true;
                    return true;
                }
            }
        }
        false
    }
}

/// A blank, formatted memory card image.
fn formatted_card() -> Vec<u8> {
    let mut c = vec![0u8; MEMCARD_SIZE];
    let frame = |c: &mut Vec<u8>, i: usize, f: &dyn Fn(&mut [u8])| {
        let b = &mut c[i * 128..(i + 1) * 128];
        f(b);
        b[127] = b[..127].iter().fold(0, |a, x| a ^ x);
    };
    frame(&mut c, 0, &|b| {
        b[0] = b'M';
        b[1] = b'C';
    });
    for i in 1..16 {
        frame(&mut c, i, &|b| {
            b[0] = 0xA0;
            b[8] = 0xFF;
            b[9] = 0xFF;
        });
    }
    for i in 16..36 {
        frame(&mut c, i, &|b| {
            b[..4].fill(0xFF);
            b[8] = 0xFF;
            b[9] = 0xFF;
        });
    }
    let f0: Vec<u8> = c[..128].to_vec();
    c[63 * 128..64 * 128].copy_from_slice(&f0);
    c
}

// ---------------------------------------------------------------------- DMA

#[derive(Default, Clone, Copy, state::Stateful)]
struct Chan {
    madr: u32,
    bcr: u32,
    chcr: u32,
}

// ---------------------------------------------------------------------- the machine's bus

#[derive(state::Stateful)]
pub struct Hw {
    ram: Vec<u8>,
    #[state(skip)]
    bios: Vec<u8>,
    scratch: Vec<u8>,
    pub gte: gte::Gte,
    pub gpu: gpu::Gpu,
    pub cd: cdrom::Cdrom,
    pub spu: spu::Spu,
    pub mdec: mdec::Mdec,
    mdec_out_pending: bool,
    sio: Sio,
    timers: [Timer; 3],
    t2_div: u32,
    dot_acc: u32,
    i_stat: u32,
    i_mask: u32,
    dma: [Chan; 7],
    dpcr: u32,
    dicr: u32,
    io: Vec<u32>,
    extra: u32,
}

const MASKS: [u32; 8] = [0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, 0x7FFF_FFFF, 0x1FFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF];

fn rd(m: &[u8], a: usize, size: u32) -> u32 {
    match size {
        1 => m[a] as u32,
        2 => u16::from_le_bytes([m[a], m[a + 1]]) as u32,
        _ => u32::from_le_bytes([m[a], m[a + 1], m[a + 2], m[a + 3]]),
    }
}

fn wr(m: &mut [u8], a: usize, v: u32, size: u32) {
    match size {
        1 => m[a] = v as u8,
        2 => m[a..a + 2].copy_from_slice(&(v as u16).to_le_bytes()),
        _ => m[a..a + 4].copy_from_slice(&v.to_le_bytes()),
    }
}

impl Hw {
    fn ram32(&self, a: u32) -> u32 {
        rd(&self.ram, (a & 0x1F_FFFC) as usize, 4)
    }

    fn io_read(&mut self, p: u32, size: u32) -> u32 {
        match p {
            0x1F80_1040 => self.sio.rx.take().unwrap_or(0xFF) as u32,
            0x1F80_1044 => self.sio.stat(),
            0x1F80_1048 => self.sio.mode as u32,
            0x1F80_104A => self.sio.ctrl as u32,
            0x1F80_104E => self.sio.baud as u32,
            0x1F80_1070 => self.i_stat,
            0x1F80_1074 => self.i_mask,
            0x1F80_1080..=0x1F80_10EF => {
                let ch = ((p - 0x1F80_1080) >> 4) as usize;
                match p & 0xC {
                    0 => self.dma[ch].madr,
                    4 => self.dma[ch].bcr,
                    8 => self.dma[ch].chcr,
                    _ => 0,
                }
            }
            0x1F80_10F0 => self.dpcr,
            0x1F80_10F4..=0x1F80_10F7 => self.dicr_read() >> ((p & 3) * 8),
            0x1F80_1100..=0x1F80_112F => {
                let t = ((p - 0x1F80_1100) >> 4) as usize;
                match p & 0xF {
                    0 => self.timers[t].count,
                    4 => self.timers[t].read_mode(),
                    8 => self.timers[t].target,
                    _ => 0,
                }
            }
            0x1F80_1800..=0x1F80_1803 => {
                if size == 4 {
                    let b = self.cd.read(p);
                    return b as u32 * 0x0101_0101;
                }
                self.cd.read(p) as u32
            }
            0x1F80_1810 => self.gpu.read(),
            0x1F80_1814 => self.gpu.status(),
            0x1F80_1820 => self.mdec.read(),
            0x1F80_1824 => self.mdec.status(),
            0x1F80_1C00..=0x1F80_1FFF => {
                let lo = self.spu.read(p & !1) as u32;
                if size == 4 {
                    lo | (self.spu.read((p & !1) + 2) as u32) << 16
                } else {
                    lo
                }
            }
            _ => {
                let i = ((p - 0x1F80_1000) >> 2) as usize;
                self.io.get(i).copied().unwrap_or(0) >> ((p & 3) * 8)
            }
        }
    }

    fn io_write(&mut self, p: u32, v: u32, size: u32) {
        match p {
            0x1F80_1040 => self.sio.write_data(v as u8),
            0x1F80_1048 => self.sio.mode = v as u16,
            0x1F80_104A => self.sio.write_ctrl(v as u16),
            0x1F80_104E => self.sio.baud = v as u16,
            0x1F80_1070 => self.i_stat &= v,
            0x1F80_1074 => self.i_mask = v & 0x7FF,
            0x1F80_1080..=0x1F80_10EF => {
                let ch = ((p - 0x1F80_1080) >> 4) as usize;
                match p & 0xC {
                    0 => self.dma[ch].madr = v & 0xFF_FFFF,
                    4 => self.dma[ch].bcr = v,
                    8 => {
                        self.dma[ch].chcr = if ch == 6 { (v & 0x5100_0000) | 2 } else { v };
                        let c = self.dma[ch].chcr;
                        let sync = (c >> 9) & 3;
                        if c & 0x0100_0000 != 0 && (sync != 0 || c & 0x1000_0000 != 0) {
                            self.run_dma(ch);
                        }
                    }
                    _ => {}
                }
            }
            0x1F80_10F0 => self.dpcr = v,
            0x1F80_10F4..=0x1F80_10F7 => {
                // Byte and halfword writes are common (the BIOS acknowledges flags with sb/sh).
                let sh = (p & 3) * 8;
                let mask = match size {
                    1 => 0xFFu32,
                    2 => 0xFFFF,
                    _ => 0xFFFF_FFFF,
                } << sh;
                let v = (v << sh) & mask;
                let kept = self.dicr & !mask & 0x00FF_803F;
                let flags = (self.dicr & 0x7F00_0000) & !(v & 0x7F00_0000);
                self.dicr = kept | (v & 0x00FF_803F) | flags;
            }
            0x1F80_1100..=0x1F80_112F => {
                let t = ((p - 0x1F80_1100) >> 4) as usize;
                match p & 0xF {
                    0 => self.timers[t].count = v & 0xFFFF,
                    4 => self.timers[t].write_mode(v),
                    8 => self.timers[t].target = v & 0xFFFF,
                    _ => {}
                }
            }
            0x1F80_1800..=0x1F80_1803 => self.cd.write(p, v as u8),
            0x1F80_1810 => self.gpu.gp0(v),
            0x1F80_1814 => self.gpu.gp1(v),
            0x1F80_1820 => {
                self.mdec.write(v);
                self.flush_mdec_out();
            }
            0x1F80_1824 => self.mdec.control(v),
            0x1F80_1C00..=0x1F80_1FFF => {
                self.spu.write(p & !1, v as u16);
                if size == 4 {
                    self.spu.write((p & !1) + 2, (v >> 16) as u16);
                }
            }
            _ => {
                let i = ((p - 0x1F80_1000) >> 2) as usize;
                if let Some(r) = self.io.get_mut(i) {
                    *r = v << ((p & 3) * 8);
                }
            }
        }
    }

    fn dicr_read(&self) -> u32 {
        let master = self.dicr & 0x8000 != 0 || (self.dicr & 0x80_0000 != 0 && (self.dicr >> 24) & (self.dicr >> 16) & 0x7F != 0);
        self.dicr | (master as u32) << 31
    }

    /// Finishes a DMA1 (MDEC out) transfer that was started before its pixels existed.
    fn flush_mdec_out(&mut self) {
        if self.mdec_out_pending {
            let c = self.dma[1];
            let words = if (c.chcr >> 9) & 3 == 0 { (c.bcr & 0xFFFF).max(1) } else { (c.bcr & 0xFFFF) * (c.bcr >> 16) };
            if self.mdec.available() >= words as usize {
                self.mdec_out_pending = false;
                self.run_dma(1);
            }
        }
    }

    fn run_dma(&mut self, ch: usize) {
        let c = self.dma[ch];
        if ch == 1 {
            let words = if (c.chcr >> 9) & 3 == 0 { (c.bcr & 0xFFFF).max(1) } else { (c.bcr & 0xFFFF) * (c.bcr >> 16) };
            if self.mdec.available() < words as usize {
                // Wait for DMA0 / CPU writes to produce the pixels.
                self.mdec_out_pending = true;
                return;
            }
        }
        let sync = (c.chcr >> 9) & 3;
        let from_ram = c.chcr & 1 != 0;
        let step: u32 = if c.chcr & 2 != 0 { 4u32.wrapping_neg() } else { 4 };
        let mut a = c.madr & 0x1F_FFFC;
        if sync == 2 {
            // Linked list (GPU command lists).
            for _ in 0..0x10_0000 {
                let hdr = self.ram32(a);
                for i in 1..=(hdr >> 24) {
                    let w = self.ram32(a + 4 * i);
                    self.gpu.gp0(w);
                }
                if hdr & 0x80_0000 != 0 {
                    break;
                }
                a = hdr & 0x1F_FFFC;
            }
            self.dma[ch].madr = 0xFF_FFFF;
        } else {
            let words = if sync == 0 {
                match c.bcr & 0xFFFF {
                    0 => 0x1_0000,
                    n => n,
                }
            } else {
                // Block mode: a block count of 0 means 0x10000 blocks on the real hardware.
                let blocks = match c.bcr >> 16 {
                    0 => 0x1_0000,
                    n => n,
                };
                (c.bcr & 0xFFFF) * blocks
            };
            for i in 0..words {
                if ch == 6 {
                    let v = if i == words - 1 { 0xFF_FFFF } else { a.wrapping_sub(4) & 0x1F_FFFF };
                    wr(&mut self.ram, a as usize, v, 4);
                } else if from_ram {
                    let v = self.ram32(a);
                    match ch {
                        0 => self.mdec.write(v),
                        2 => self.gpu.gp0(v),
                        4 => self.spu.dma_write(v),
                        _ => {}
                    }
                } else {
                    let v = match ch {
                        1 => self.mdec.read(),
                        2 => self.gpu.read(),
                        3 => self.cd.dma_word(),
                        4 => self.spu.dma_read(),
                        _ => 0,
                    };
                    wr(&mut self.ram, a as usize, v, 4);
                }
                a = a.wrapping_add(step) & 0x1F_FFFC;
            }
            if sync == 1 {
                self.dma[ch].madr = a;
                self.dma[ch].bcr &= 0xFFFF;
            }
        }
        self.dma[ch].chcr &= !0x1100_0000;
        if ch == 0 {
            self.flush_mdec_out();
        }
        if self.dicr & (1 << (16 + ch)) != 0 {
            // Every enabled completion raises IRQ3 (not only the master flag's 0 -> 1 edge):
            // stale flags from the BIOS must not swallow later transfer interrupts.
            self.dicr |= 1 << (24 + ch);
            if self.dicr & 0x80_0000 != 0 {
                self.i_stat |= 1 << 3;
            }
        }
    }
}

impl Bus for Hw {
    fn read(&mut self, addr: u32, size: u32) -> u32 {
        let p = addr & MASKS[(addr >> 29) as usize];
        match p {
            0..=0x007F_FFFF => rd(&self.ram, (p & 0x1F_FFFF) as usize, size),
            0x1F80_0000..=0x1F80_03FF => rd(&self.scratch, (p & 0x3FF) as usize, size),
            0x1FC0_0000..=0x1FC7_FFFF => {
                self.extra += 4;
                rd(&self.bios, (p & 0x7_FFFF) as usize, size)
            }
            0x1F80_1000..=0x1F80_2FFF => {
                self.extra += 2;
                self.io_read(p, size)
            }
            0x1F00_0000..=0x1F7F_FFFF => 0xFFFF_FFFF >> (32 - size * 8),
            _ => 0,
        }
    }

    fn write(&mut self, addr: u32, v: u32, size: u32) {
        let p = addr & MASKS[(addr >> 29) as usize];

        match p {
            0..=0x007F_FFFF => wr(&mut self.ram, (p & 0x1F_FFFF) as usize, v, size),
            0x1F80_0000..=0x1F80_03FF => wr(&mut self.scratch, (p & 0x3FF) as usize, v, size),
            0x1F80_1000..=0x1F80_2FFF => self.io_write(p, v, size),
            _ => {}
        }
    }

    fn gte_read(&mut self, reg: u32) -> u32 {
        self.gte.read(reg)
    }
    fn gte_write(&mut self, reg: u32, v: u32) {
        self.gte.write(reg, v)
    }
    fn gte_command(&mut self, cmd: u32) {
        self.gte.command(cmd)
    }
}

// ---------------------------------------------------------------------- the console

pub struct Psx {
    cpu: Cpu<Hw>,
    line_cycles: u32,
    line: u32,
    exe: Option<Vec<u8>>,
    irq_line: bool,
    /// Diagnostics: PCs executed after a traced event.
    pub trace: Vec<u32>,
    pub trace_budget: u32,
    /// Diagnostics: when set, counts calls into the BIOS tables (A0/B0/C0 and function number).
    pub bios_trace: bool,
    pub bios_calls: std::collections::BTreeMap<(u32, u32), u32>,
    /// The CPU state the first time the BIOS reported an unhandled exception.
    pub first_crash: Option<cpur3000::Regs>,
    pub trace_after: u64,
}

impl state::Stateful for Psx {
    /// Everything that changes while a game runs. The BIOS and disc are the same ones the
    /// state was saved with, so they aren't stored.
    fn sync(&mut self, s: &mut state::State) {
        s.check(0x5053_5831);
        self.cpu.sync_state(s);
        state::Stateful::sync(self.cpu.bus_mut(), s);
        state::Stateful::sync(&mut self.line_cycles, s);
        state::Stateful::sync(&mut self.line, s);
        state::Stateful::sync(&mut self.irq_line, s);
    }
}

impl Psx {
    /// Snapshot of the running console (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    /// Restores a snapshot; refused untouched if it doesn't fit this console.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    /// A console with the user's BIOS and an optional disc image (.cue/.bin/.iso).
    pub fn new(bios: Vec<u8>, disc: Option<&Path>) -> Result<Psx, String> {
        if bios.len() != 512 * 1024 {
            return Err(format!("The BIOS file should be exactly 512 KB (this one is {} bytes).", bios.len()));
        }
        let disc = match disc {
            Some(p) => Some(disc::Disc::open(p)?),
            None => None,
        };
        let hw = Hw {
            ram: vec![0; 2 * 1024 * 1024],
            bios,
            scratch: vec![0; 1024],
            gte: gte::Gte::default(),
            gpu: gpu::Gpu::new(),
            cd: cdrom::Cdrom::new(disc),
            spu: spu::Spu::new(),
            mdec: mdec::Mdec::new(),
            mdec_out_pending: false,
            sio: Sio::new(),
            timers: [Timer::default(); 3],
            t2_div: 0,
            dot_acc: 0,
            i_stat: 0,
            i_mask: 0,
            dma: [Chan::default(); 7],
            dpcr: 0x0765_4321,
            dicr: 0,
            io: vec![0; 0x800],
            extra: 0,
        };
        let mut cpu = Cpu::new(hw);
        // SAFETY: the RAM vector is allocated once, is never resized, and lives as long as the CPU.
        unsafe {
            let ram = cpu.bus_mut().ram.as_mut_ptr();
            cpu.set_ram(ram);
        }
        cpu.reset();
        Ok(Psx { cpu, line_cycles: 0, line: 0, exe: None, irq_line: false, trace: Vec::new(), trace_budget: 0, bios_trace: false, bios_calls: Default::default(), first_crash: None, trace_after: u64::MAX })
    }

    /// Boots a PS-X EXE (homebrew/test program) once the BIOS has initialised the kernel.
    pub fn load_exe(&mut self, exe: Vec<u8>) -> Result<(), String> {
        if exe.len() < 0x800 || &exe[..8] != b"PS-X EXE" {
            return Err("Not a PlayStation executable (PS-X EXE).".into());
        }
        self.exe = Some(exe);
        Ok(())
    }

    fn inject_exe(&mut self) {
        let Some(exe) = self.exe.take() else { return };
        let w = |o: usize| u32::from_le_bytes([exe[o], exe[o + 1], exe[o + 2], exe[o + 3]]);
        let (pc, gp, dest, size, sp_base, sp_size) = (w(0x10), w(0x14), w(0x18), w(0x1C), w(0x30), w(0x34));
        let hw = self.cpu.bus_mut();
        for i in 0..size.min(exe.len() as u32 - 0x800) {
            hw.ram[((dest + i) & 0x1F_FFFF) as usize] = exe[0x800 + i as usize];
        }
        let mut r = self.cpu.regs();
        r.pc = pc;
        r.next_pc = pc.wrapping_add(4);
        r.r[28] = gp;
        if sp_base != 0 {
            r.r[29] = sp_base.wrapping_add(sp_size);
            r.r[30] = r.r[29];
        }
        self.cpu.set_regs(r);
    }

    pub fn is_pal(&self) -> bool {
        self.cpu.bus().gpu.pal
    }

    pub fn fps(&self) -> f64 {
        if self.is_pal() {
            49.76
        } else {
            59.29
        }
    }

    /// Runs until the next vertical blank.
    pub fn run_frame(&mut self) {
        let pal = self.is_pal();
        let (lines, per_line) = if pal { (314, 2169) } else { (263, 2172) };
        let vblank_start = if pal { 256 } else { 240 };
        let mut pending = 0u32;
        loop {
            if self.exe.is_some() && self.cpu.regs().pc == 0x8003_0000 {
                self.inject_exe();
            }
            if self.bios_trace {
                let r = self.cpu.regs();
                if matches!(r.pc, 0xA0 | 0xB0 | 0xC0) {
                    *self.bios_calls.entry((r.pc, r.r[9])).or_insert(0) += 1;
                    if r.pc == 0xA0 && r.r[9] == 0x40 && self.first_crash.is_none() {
                        self.first_crash = Some(r);
                    }
                }
            }
            if self.trace_budget > 0 {
                self.trace.push(self.cpu.regs().pc);
                self.trace_budget -= 1;
            }
            let c = self.cpu.step();
            let hw = self.cpu.bus_mut();
            if hw.cd.trace_request {
                hw.cd.trace_request = false;
                if hw.cd.stats[4] == self.trace_after {
                    self.trace_budget = 3000;
                }
            }
            let c = c + std::mem::take(&mut hw.extra);
            // Hardware is advanced in small batches: it keeps the timers, disc, sound and
            // controller close enough to the CPU while costing a fraction of a tick per step.
            pending += c;
            self.line_cycles += c;
            if pending >= 64 || self.line_cycles >= per_line {
                self.tick(pending);
                pending = 0;
            }
            if self.line_cycles >= per_line {
                self.line_cycles -= per_line;
                self.line += 1;
                let hw = self.cpu.bus_mut();
                // Timer 1 counting horizontal blanks.
                if matches!((hw.timers[1].mode >> 8) & 3, 1 | 3) && hw.timers[1].advance(1) {
                    hw.i_stat |= 1 << 5;
                }
                hw.gpu.odd = !hw.gpu.odd;
                if self.line == vblank_start {
                    hw.gpu.in_vblank = true;
                    hw.i_stat |= 1;
                    hw.gpu.render_frame();
                    if hw.sio.card_dirty {
                        // Saved state is picked up by the frontend through memcard().
                    }
                    self.line_update_irq();
                    return self.finish_frame(lines);
                }
                if self.line >= lines {
                    self.line = 0;
                    let hw = self.cpu.bus_mut();
                    hw.gpu.in_vblank = false;
                }
            }
        }
    }

    fn finish_frame(&mut self, _lines: u32) {}

    fn tick(&mut self, c: u32) {
        let hw = self.cpu.bus_mut();
        // Timer 0: system clock or GPU dot clock.
        let t0 = if matches!((hw.timers[0].mode >> 8) & 3, 1 | 3) {
            hw.dot_acc += c * 11;
            let div = 7 * hw.gpu.dot_divider();
            let n = hw.dot_acc / div;
            hw.dot_acc %= div;
            n
        } else {
            c
        };
        if hw.timers[0].advance(t0) {
            hw.i_stat |= 1 << 4;
        }
        if !matches!((hw.timers[1].mode >> 8) & 3, 1 | 3) && hw.timers[1].advance(c) {
            hw.i_stat |= 1 << 5;
        }
        let m2 = hw.timers[2].mode;
        let stopped = m2 & 1 != 0 && matches!((m2 >> 1) & 3, 0 | 3);
        if !stopped {
            let n = if matches!((m2 >> 8) & 3, 2 | 3) {
                hw.t2_div += c;
                let n = hw.t2_div / 8;
                hw.t2_div %= 8;
                n
            } else {
                c
            };
            if hw.timers[2].advance(n) {
                hw.i_stat |= 1 << 6;
            }
        }
        if hw.cd.tick(c) {
            hw.i_stat |= 1 << 2;
        }
        if hw.sio.tick(c) {
            hw.i_stat |= 1 << 7;
        }
        if hw.gpu.take_irq() {
            hw.i_stat |= 1 << 1;
        }
        let Hw { spu, cd, .. } = hw;
        spu.tick(c, &mut cd.audio);
        self.line_update_irq();
    }

    fn line_update_irq(&mut self) {
        let hw = self.cpu.bus();
        let on = hw.i_stat & hw.i_mask != 0;
        if on != self.irq_line {
            self.irq_line = on;
            self.cpu.set_irq(on);
        }
    }

    pub fn frame(&self) -> &[u32] {
        self.cpu.bus().gpu.frame()
    }

    pub fn size(&self) -> (usize, usize) {
        self.cpu.bus().gpu.size()
    }

    pub fn set_buttons(&mut self, pad: usize, b: u16) {
        if pad < 2 {
            self.cpu.bus_mut().sio.buttons[pad] = b;
        }
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    pub fn memcard(&self) -> &[u8] {
        &self.cpu.bus().sio.cards[0]
    }

    pub fn load_memcard(&mut self, data: &[u8]) {
        if data.len() == MEMCARD_SIZE {
            let sio = &mut self.cpu.bus_mut().sio;
            sio.cards[0].copy_from_slice(data);
            sio.card_flag[0] = 0;
        }
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        self.cpu.bus_mut().spu.take_audio(out);
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.cpu.bus_mut().spu.set_sample_rate(r);
    }

    /// CPU program counter (diagnostics).
    pub fn pc(&self) -> u32 {
        self.cpu.regs().pc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny "BIOS" written by hand: fills the screen via GP0, then loops.
    #[test]
    fn hand_written_bios_draws_through_the_gpu() {
        let mut bios = vec![0u8; 512 * 1024];
        let lui = |rt: u32, imm: u32| 0x0F << 26 | rt << 16 | imm;
        let ori = |rt: u32, rs: u32, imm: u32| 0x0D << 26 | rs << 21 | rt << 16 | imm;
        let sw = |rt: u32, base: u32, off: u32| 0x2B << 26 | base << 21 | rt << 16 | (off & 0xFFFF);
        let mut prog = vec![lui(1, 0x1F80)];
        let mut put = |reg_off: u32, val: u32| {
            prog.push(lui(2, val >> 16));
            prog.push(ori(2, 2, val & 0xFFFF));
            prog.push(sw(2, 1, reg_off));
        };
        put(0x1814, 0x0300_0000); // display on
        put(0x1814, 0x0800_0001); // 320x240
        put(0x1810, 0x0200_00FF); // fill red
        put(0x1810, 0);
        put(0x1810, 240 << 16 | 320);
        let mut prog = prog;
        prog.push(0x1000_FFFF); // b .
        prog.push(0);
        for (i, w) in prog.iter().enumerate() {
            bios[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        let mut psx = Psx::new(bios, None).unwrap();
        psx.run_frame();
        assert_eq!(psx.size(), (320, 240));
        let px = psx.frame()[120 * 320 + 160];
        assert_eq!(px, 0xFF0000, "screen is red");
    }

    /// A BIOS that counts in a register and writes it to RAM and the GPU forever.
    fn counting_bios() -> Vec<u8> {
        let mut bios = vec![0u8; 512 * 1024];
        let prog: [u32; 8] = [
            0x3C01_1F80, // lui r1,0x1F80
            0x2442_0001, // addiu r2,r2,1
            0xAC22_0000 | 0x0100, // sw r2,0x100(r1)  (scratchpad)
            0xAC22_1810, // sw r2,0x1810(r1) (GP0)
            0x8C23_0100, // lw r3,0x100(r1)
            0x0043_2021, // addu r4,r2,r3
            0x1000_FFFA, // b 0x...
            0,
        ];
        for (i, w) in prog.iter().enumerate() {
            bios[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        bios
    }

    #[test]
    fn state_round_trip_is_deterministic() {
        let mut a = Psx::new(counting_bios(), None).unwrap();
        for _ in 0..3 {
            a.run_frame();
        }
        let snap = state::save(&mut a);
        for _ in 0..4 {
            a.run_frame();
        }
        let want = state::save(&mut a);
        assert_ne!(want, snap);
        // Rewind the same machine, then a fresh one, to the snapshot and run on.
        state::load(&mut a, &snap).unwrap();
        for _ in 0..4 {
            a.run_frame();
        }
        assert_eq!(state::save(&mut a), want);
        let mut b = Psx::new(counting_bios(), None).unwrap();
        state::load(&mut b, &snap).unwrap();
        for _ in 0..4 {
            b.run_frame();
        }
        assert_eq!(state::save(&mut b), want);
    }

    #[test]
    fn controller_protocol() {
        let mut s = Sio::new();
        s.buttons[0] = button::CROSS | button::START;
        s.write_ctrl(0x1003);
        let mut xfer = |v: u8| {
            s.write_data(v);
            s.rx.take().unwrap()
        };
        assert_eq!(xfer(0x01), 0xFF);
        assert_eq!(xfer(0x42), 0x41);
        assert_eq!(xfer(0x00), 0x5A);
        let lo = xfer(0);
        let hi = xfer(0);
        assert_eq!(!(lo as u16 | (hi as u16) << 8), button::CROSS | button::START);
    }

    #[test]
    fn memory_card_write_then_read() {
        let mut s = Sio::new();
        s.write_ctrl(0x1003);
        let xfer = |s: &mut Sio, v: u8| {
            s.write_data(v);
            s.rx.take().unwrap()
        };
        // Write sector 0x40.
        for b in [0x81, b'W', 0, 0, 0x00, 0x40] {
            xfer(&mut s, b);
        }
        for i in 0..128u8 {
            xfer(&mut s, i);
        }
        for b in [0, 0, 0, 0] {
            xfer(&mut s, b);
        }
        s.write_ctrl(0x0000);
        s.write_ctrl(0x1003);
        for b in [0x81, b'R', 0, 0, 0x00, 0x40, 0, 0, 0, 0] {
            xfer(&mut s, b);
        }
        let data: Vec<u8> = (0..128).map(|_| xfer(&mut s, 0)).collect();
        assert_eq!(data, (0..128u8).collect::<Vec<_>>());
        assert!(s.card_dirty);
    }
}

#[cfg(test)]
mod boot_tests {
    use super::*;

    /// Boots a real disc (PSX_BIOS + PSX_CUE) and reports where the game ends up.
    #[test]
    fn boot_real_disc() {
        let (Ok(bios), Ok(cue)) = (std::env::var("PSX_BIOS"), std::env::var("PSX_CUE")) else { return };
        let mut p = Psx::new(std::fs::read(bios).unwrap(), Some(Path::new(&cue))).unwrap();
        p.trace_after = std::env::var("PSX_TRACE_INT1").ok().and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
        let frames: u32 = std::env::var("PSX_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(900);
        // PSX_PAD="frame:start|cross:frames,..." presses buttons at given frames.
        let script: Vec<(u32, u16, u32)> = std::env::var("PSX_PAD")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| {
                let q: Vec<&str> = s.split(':').collect();
                let mask = q[1].split('|').fold(0, |a, n| a | match n { "start" => button::START, "cross" => button::CROSS, "circle" => button::CIRCLE, "down" => button::DOWN, "up" => button::UP, _ => 0 });
                (q[0].parse().unwrap(), mask, q[2].parse().unwrap())
            })
            .collect();
        p.bios_trace = std::env::var_os("PSX_BIOSTRACE").is_some();
        for f in 0..frames {
            if f == frames.saturating_sub(300) {
                p.bios_calls.clear();
            }
            let b = script.iter().filter(|(at, _, len)| f >= *at && f < at + len).fold(0, |acc, (_, m, _)| acc | m);
            p.set_buttons(0, b);
            p.run_frame();
            if f % 60 == 59 {
                let hw = p.cpu.bus();
                let lit = hw.gpu.frame().iter().filter(|&&c| c & 0xFFFFFF != 0).count();
                println!("frame {} pc {:08X} size {:?} lit {} i_mask {:03X} mdec cmds {} blocks {} status {:08X} dma1 {:08X} pend {}", f + 1, p.pc(), p.size(), lit, hw.i_mask, hw.mdec.commands, hw.mdec.blocks.get(), hw.mdec.status(), hw.dma[1].chcr, hw.mdec_out_pending);
                println!("   cd stats {:?} dicr {:08X} istat {:03X} dma1 madr {:06X} bcr {:08X}", hw.cd.stats, hw.dicr_read(), hw.i_stat, hw.dma[1].madr, hw.dma[1].bcr);
            }
        }
        if let Ok(t) = std::env::var("PSX_TRACE_OUT") {
            std::fs::write(t, p.trace.iter().map(|pc| format!("{pc:08X}
")).collect::<String>()).unwrap();
        }
        if let Ok(dump) = std::env::var("PSX_DUMP") {
            let hw = p.cpu.bus();
            std::fs::write(dump, &hw.ram).unwrap();
        }
        for l in &p.cpu.bus().cd.log {
            println!("{l}");
        }
        if let Some(c) = p.first_crash {
            println!("FIRST CRASH: cause={:08X} (code {}) epc={:08X} badvaddr={:08X} sr={:08X} ra={:08X} sp={:08X}", c.cause, (c.cause >> 2) & 31, c.epc, c.badvaddr, c.sr, c.r[31], c.r[29]);
            let hw = p.cpu.bus();
            let at = (c.epc & 0x1FFF_FFFF) as usize;
            if at + 16 <= hw.ram.len() {
                println!("  code at epc: {:08X} {:08X} {:08X}", u32::from_le_bytes(hw.ram[at..at + 4].try_into().unwrap()), u32::from_le_bytes(hw.ram[at + 4..at + 8].try_into().unwrap()), u32::from_le_bytes(hw.ram[at + 8..at + 12].try_into().unwrap()));
            }
        }
        let mut calls: Vec<_> = p.bios_calls.iter().collect();
        calls.sort_by(|a, b| b.1.cmp(a.1));
        for ((table, func), n) in calls.into_iter().take(12) {
            println!("BIOS call table {table:#x} function {func:#04x}: {n} times in the last 300 frames");
        }
    }
}
