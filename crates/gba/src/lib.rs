//! Game Boy Advance on our own cores: ARM7TDMI CPU (C++), picture processor, sound
//! (the four Game Boy channels plus two sample FIFOs), DMA, timers, cartridge saves and a
//! high-level BIOS written from the documented behaviour of each BIOS call, so no
//! Nintendo BIOS file is needed.

pub mod ppu;
pub mod rtc;
pub mod save;

use cpuarm7::{Bus, Cpu, Regs};
use std::collections::VecDeque;

pub use ppu::{HEIGHT, WIDTH};

pub const CPU_HZ: f64 = 16_777_216.0;
pub const FPS: f64 = CPU_HZ / 280_896.0;

/// Buttons (bit set = pressed), in KEYINPUT order.
pub mod button {
    pub const A: u16 = 1 << 0;
    pub const B: u16 = 1 << 1;
    pub const SELECT: u16 = 1 << 2;
    pub const START: u16 = 1 << 3;
    pub const RIGHT: u16 = 1 << 4;
    pub const LEFT: u16 = 1 << 5;
    pub const UP: u16 = 1 << 6;
    pub const DOWN: u16 = 1 << 7;
    pub const R: u16 = 1 << 8;
    pub const L: u16 = 1 << 9;
}

#[derive(Default, Clone, Copy, state::Stateful)]
struct Dma {
    sad: u32,
    dad: u32,
    cnt: u32,
    ctl: u16,
    isad: u32,
    idad: u32,
    icnt: u32,
}

#[derive(Default, Clone, Copy, state::Stateful)]
struct Timer {
    counter: u32,
    reload: u16,
    ctl: u16,
    acc: u32,
}

/// Our replacement BIOS: exception vectors and the IRQ dispatcher (ARM code we wrote);
/// every SWI is handled in Rust.
fn bios_image() -> Vec<u8> {
    let mut b = vec![0u8; 0x4000];
    let mut put = |a: usize, w: u32| b[a..a + 4].copy_from_slice(&w.to_le_bytes());
    put(0x00, 0xE3A0_F302); // mov pc, #0x08000000
    put(0x08, 0xE1B0_F00E); // movs pc, lr
    put(0x18, 0xEA00_0042); // b 0x128
    put(0x128, 0xE92D_500F); // stmfd sp!, {r0-r3, r12, lr}
    put(0x12C, 0xE3A0_0301); // mov r0, #0x04000000
    put(0x130, 0xE28F_E000); // add lr, pc, #0
    put(0x134, 0xE510_F004); // ldr pc, [r0, #-4]   (user handler at 0x03007FFC)
    put(0x138, 0xE8BD_500F); // ldmfd sp!, {r0-r3, r12, lr}
    put(0x13C, 0xE25E_F004); // subs pc, lr, #4
    b
}

#[derive(state::Stateful)]
pub struct Hw {
    #[state(skip)]
    bios: Vec<u8>,
    ewram: Vec<u8>,
    iwram: Vec<u8>,
    io: Vec<u8>,
    pub ppu: ppu::Ppu,
    #[state(skip)]
    rom: Vec<u8>,
    pub save: save::Save,
    /// Cartridge real-time clock (GPIO), when the game has one.
    pub rtc: rtc::Rtc,
    dma: [Dma; 4],
    timers: [Timer; 4],
    apu: gb::apu::Apu,
    apu_div: u32,
    fifo: [VecDeque<i8>; 2],
    ds: [i8; 2],
    #[state(skip)]
    out: Vec<f32>,
    #[state(skip)]
    keys: u16,
    halted: bool,
    intr_wait: bool,
    extra: u32,
}

impl Hw {
    fn rom_len(&self) -> usize {
        self.rom.len()
    }
}

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

/// GBA sound register byte -> Game Boy APU register (the PSG channels are the same hardware).
fn psg_addr(off: usize) -> Option<u16> {
    Some(match off {
        0x60 => 0xFF10,
        0x62 => 0xFF11,
        0x63 => 0xFF12,
        0x64 => 0xFF13,
        0x65 => 0xFF14,
        0x68 => 0xFF16,
        0x69 => 0xFF17,
        0x6C => 0xFF18,
        0x6D => 0xFF19,
        0x70 => 0xFF1A,
        0x72 => 0xFF1B,
        0x73 => 0xFF1C,
        0x74 => 0xFF1D,
        0x75 => 0xFF1E,
        0x78 => 0xFF20,
        0x79 => 0xFF21,
        0x7C => 0xFF22,
        0x7D => 0xFF23,
        0x80 => 0xFF24,
        0x81 => 0xFF25,
        0x84 => 0xFF26,
        0x90..=0x9F => 0xFF30 + (off - 0x90) as u16,
        _ => return None,
    })
}

impl Hw {
    fn io16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.io[off], self.io[off + 1]])
    }

    fn set_io16(&mut self, off: usize, v: u16) {
        self.io[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn raise(&mut self, bit: u16) {
        let v = self.io16(0x202) | (1 << bit);
        self.set_io16(0x202, v);
    }

    fn irq_pending(&self) -> bool {
        self.io16(0x200) & self.io16(0x202) & 0x3FFF != 0
    }

    // ---------------------------------------------------------------- IO

    fn io_read8(&mut self, off: usize) -> u8 {
        match off {
            0x100..=0x10F if off & 3 < 2 => {
                let t = self.timers[(off - 0x100) / 4];
                (t.counter >> ((off & 1) * 8)) as u8
            }
            0x130 => !self.keys as u8,
            0x131 => (!self.keys >> 8) as u8 & 3,
            0x60..=0x9F => match psg_addr(off) {
                Some(a) => self.apu.read(a),
                None => self.io[off],
            },
            0x400.. => 0,
            _ => self.io[off],
        }
    }

    fn io_write8(&mut self, off: usize, v: u8) {
        if off >= 0x400 {
            return;
        }
        match off {
            0x004 => {
                self.io[4] = (self.io[4] & 0x07) | (v & 0xF8);
                return;
            }
            0x006 | 0x007 => return,
            0x028..=0x02B | 0x038..=0x03B => {
                self.io[off] = v;
                self.ppu.latch_ref(&self.io, 0);
                self.ppu.latch_ref(&self.io, 1);
                return;
            }
            0x02C..=0x02F | 0x03C..=0x03F => {
                self.io[off] = v;
                self.ppu.latch_ref(&self.io, 0);
                self.ppu.latch_ref(&self.io, 1);
                return;
            }
            0x60..=0x9F => {
                self.io[off] = v;
                if let Some(a) = psg_addr(off) {
                    let v = if off == 0x70 { v & 0x80 } else { v };
                    self.apu.write(a, v);
                }
                if off == 0x83 {
                    if v & 0x08 != 0 {
                        self.fifo[0].clear();
                    }
                    if v & 0x80 != 0 {
                        self.fifo[1].clear();
                    }
                    self.io[off] = v & 0x77;
                }
                return;
            }
            0xA0..=0xA7 => {
                let f = &mut self.fifo[(off - 0xA0) / 4];
                if f.len() < 32 {
                    f.push_back(v as i8);
                }
                return;
            }
            0xB0..=0xDF => {
                self.io[off] = v;
                let ch = (off - 0xB0) / 12;
                let r = (off - 0xB0) % 12;
                let b = 0xB0 + ch * 12;
                let d = &mut self.dma[ch];
                let rd32 = |io: &[u8], a: usize| u32::from_le_bytes([io[a], io[a + 1], io[a + 2], io[a + 3]]);
                d.sad = rd32(&self.io, b);
                d.dad = rd32(&self.io, b + 4);
                d.cnt = u16::from_le_bytes([self.io[b + 8], self.io[b + 9]]) as u32;
                if r == 11 {
                    let old = d.ctl;
                    d.ctl = u16::from_le_bytes([self.io[b + 10], self.io[b + 11]]);
                    if old & 0x8000 == 0 && d.ctl & 0x8000 != 0 {
                        let smask = if ch == 0 { 0x07FF_FFFF } else { 0x0FFF_FFFF };
                        let dmask = if ch == 3 { 0x0FFF_FFFF } else { 0x07FF_FFFF };
                        d.isad = d.sad & smask;
                        d.idad = d.dad & dmask;
                        let c = self.dma_count(ch);
                        self.dma[ch].icnt = c;
                        if (self.dma[ch].ctl >> 12) & 3 == 0 {
                            self.run_dma(ch);
                        }
                    }
                } else if r == 10 {
                    self.dma[ch].ctl = u16::from_le_bytes([self.io[b + 10], self.io[b + 11]]);
                }
                return;
            }
            0x100..=0x10F => {
                let t = (off - 0x100) / 4;
                match off & 3 {
                    0 => self.timers[t].reload = (self.timers[t].reload & 0xFF00) | v as u16,
                    1 => self.timers[t].reload = (self.timers[t].reload & 0x00FF) | (v as u16) << 8,
                    2 => {
                        let was = self.timers[t].ctl & 0x80 != 0;
                        self.timers[t].ctl = v as u16;
                        if !was && v & 0x80 != 0 {
                            self.timers[t].counter = self.timers[t].reload as u32;
                            self.timers[t].acc = 0;
                        }
                    }
                    _ => {}
                }
                self.io[off] = v;
                return;
            }
            0x202 | 0x203 => {
                self.io[off] &= !v;
                return;
            }
            0x301 => {
                self.halted = true;
                return;
            }
            _ => {}
        }
        self.io[off] = v;
    }

    fn dma_count(&self, ch: usize) -> u32 {
        let c = self.dma[ch].cnt & if ch == 3 { 0xFFFF } else { 0x3FFF };
        if c == 0 {
            if ch == 3 {
                0x10000
            } else {
                0x4000
            }
        } else {
            c
        }
    }

    fn run_dma(&mut self, ch: usize) {
        let d = self.dma[ch];
        let timing = (d.ctl >> 12) & 3;
        let fifo = (ch == 1 || ch == 2) && timing == 3;
        let word = d.ctl & 0x400 != 0 || fifo;
        let unit = if word { 4 } else { 2 };
        let count = if fifo { 4 } else { d.icnt };
        let step = |ctl: u16, shift: u32| -> i32 {
            match (ctl >> shift) & 3 {
                0 | 3 => unit as i32,
                1 => -(unit as i32),
                _ => 0,
            }
        };
        let sstep = step(d.ctl, 7);
        let dstep = if fifo { 0 } else { step(d.ctl, 5) };
        let (mut s, mut dd) = (d.isad, d.idad);
        if ch == 3 && (0x0D00_0000..0x0E00_0000).contains(&dd) && self.save.kind == save::Kind::Eeprom {
            // The transfer length tells us the EEPROM's address width.
            match count {
                9 | 73 => self.save.ee_addr_bits = 6,
                17 | 81 => self.save.ee_addr_bits = 14,
                _ => {}
            }
        }
        for _ in 0..count {
            let v = self.read(s & !(unit - 1), unit);
            self.write(dd & !(unit - 1), v, unit);
            s = (s as i32 + sstep) as u32;
            dd = (dd as i32 + dstep) as u32;
        }
        self.extra += count * 2 + 2;
        let dm = &mut self.dma[ch];
        dm.isad = s;
        if (dm.ctl >> 5) & 3 != 3 {
            dm.idad = dd;
        }
        if dm.ctl & 0x200 != 0 && timing != 0 {
            if (dm.ctl >> 5) & 3 == 3 {
                dm.idad = dm.dad & if ch == 3 { 0x0FFF_FFFF } else { 0x07FF_FFFF };
            }
            let c = self.dma_count(ch);
            self.dma[ch].icnt = c;
        } else {
            self.dma[ch].ctl &= !0x8000;
            let b = 0xB0 + ch * 12 + 11;
            self.io[b] &= 0x7F;
        }
        if self.dma[ch].ctl & 0x4000 != 0 {
            self.raise(8 + ch as u16);
        }
    }

    fn trigger_dma(&mut self, timing: u16) {
        for ch in 0..4 {
            let c = self.dma[ch].ctl;
            if c & 0x8000 != 0 && (c >> 12) & 3 == timing && !(timing == 3 && (ch == 0 || ch == 3)) {
                self.run_dma(ch);
            }
        }
    }

    // ---------------------------------------------------------------- timers and sound

    fn tick(&mut self, cycles: u32) {
        let mut overflow_prev = 0u32;
        for i in 0..4 {
            let t = self.timers[i];
            if t.ctl & 0x80 == 0 {
                overflow_prev = 0;
                continue;
            }
            let incs = if i > 0 && t.ctl & 4 != 0 {
                overflow_prev
            } else {
                let period = [1, 64, 256, 1024][(t.ctl & 3) as usize];
                let acc = t.acc + cycles;
                self.timers[i].acc = acc % period;
                acc / period
            };
            let mut c = self.timers[i].counter + incs;
            let mut overflows = 0;
            while c > 0xFFFF {
                c = c - 0x10000 + self.timers[i].reload as u32;
                overflows += 1;
            }
            self.timers[i].counter = c;
            overflow_prev = overflows;
            if overflows > 0 {
                if t.ctl & 0x40 != 0 {
                    self.raise(3 + i as u16);
                }
                if i < 2 {
                    for _ in 0..overflows.min(8) {
                        self.fifo_step(i);
                    }
                }
            }
        }
        // Game Boy channels run at a quarter of the GBA clock.
        self.apu_div += cycles;
        let n = self.apu_div / 4;
        self.apu_div %= 4;
        if n > 0 {
            let before = self.apu.samples.len();
            self.apu.tick(n);
            self.mix_new(before);
        }
    }

    fn fifo_step(&mut self, timer: usize) {
        let cnt = self.io16(0x82);
        for k in 0..2 {
            let sel = (cnt >> (10 + k * 4)) & 1;
            if sel as usize != timer {
                continue;
            }
            if let Some(s) = self.fifo[k].pop_front() {
                self.ds[k] = s;
            }
            if self.fifo[k].len() <= 16 {
                let target = 0x0400_00A0 + k as u32 * 4;
                for ch in 1..=2 {
                    let d = self.dma[ch];
                    if d.ctl & 0x8000 != 0 && (d.ctl >> 12) & 3 == 3 && d.dad & 0x0FFF_FFFF == target {
                        self.run_dma(ch);
                        break;
                    }
                }
            }
        }
    }

    /// Adds the sample channels and the master mix to samples the PSG just produced.
    fn mix_new(&mut self, from: usize) {
        let cnt = self.io16(0x82);
        let psg = [0.25f32, 0.5, 1.0, 1.0][(cnt & 3) as usize];
        let vol = |bit: u16| if cnt & bit != 0 { 1.0f32 } else { 0.5 };
        let a = self.ds[0] as f32 / 128.0 * vol(4);
        let b = self.ds[1] as f32 / 128.0 * vol(8);
        let (mut l, mut r) = (0.0f32, 0.0f32);
        if cnt & 0x200 != 0 {
            l += a;
        }
        if cnt & 0x100 != 0 {
            r += a;
        }
        if cnt & 0x2000 != 0 {
            l += b;
        }
        if cnt & 0x1000 != 0 {
            r += b;
        }
        let s = &self.apu.samples[from..];
        for pair in s.chunks_exact(2) {
            self.out.push(((pair[0] * psg + l) * 0.5).clamp(-1.0, 1.0));
            self.out.push(((pair[1] * psg + r) * 0.5).clamp(-1.0, 1.0));
        }
        self.apu.samples.truncate(from);
    }

    // ---------------------------------------------------------------- high-level BIOS

    fn r8(&mut self, a: u32) -> u32 {
        self.read(a, 1)
    }
    fn r16(&mut self, a: u32) -> u32 {
        self.read(a & !1, 2)
    }
    fn r32(&mut self, a: u32) -> u32 {
        self.read(a & !3, 4)
    }

    fn bios_call(&mut self, n: u32, regs: &mut Regs) {
        let g = |i| regs.get(i);
        match n {
            0x00 => {
                // SoftReset
                for a in 0x0300_7E00..0x0300_8000u32 {
                    self.write(a, 0, 1);
                }
                regs.set(15, 0x0800_0000);
            }
            0x01 => {
                let f = g(0);
                if f & 1 != 0 {
                    self.ewram.fill(0);
                }
                if f & 2 != 0 {
                    self.iwram[..0x7E00].fill(0);
                }
                if f & 4 != 0 {
                    self.ppu.pal.fill(0);
                }
                if f & 8 != 0 {
                    self.ppu.vram.fill(0);
                }
                if f & 16 != 0 {
                    self.ppu.oam.fill(0);
                }
            }
            0x02 => self.halted = true,
            0x03 => self.halted = true,
            0x04 | 0x05 => {
                let (discard, flags) = if n == 5 { (1, 1) } else { (g(0), g(1) & 0x3FFF) };
                if !self.intr_wait && discard != 0 {
                    let v = self.r16(0x0300_7FF8) & !flags;
                    self.write(0x0300_7FF8, v, 2);
                }
                let got = self.r16(0x0300_7FF8) & flags;
                if got != 0 {
                    let v = self.r16(0x0300_7FF8) & !got;
                    self.write(0x0300_7FF8, v, 2);
                    self.intr_wait = false;
                } else {
                    self.intr_wait = true;
                    self.io[0x208] = 1; // IME on
                    self.halted = true;
                    regs.repeat();
                }
            }
            0x06 | 0x07 => {
                let (num, den) = if n == 6 { (g(0) as i32, g(1) as i32) } else { (g(1) as i32, g(0) as i32) };
                if den != 0 {
                    let q = num.wrapping_div(den);
                    regs.set(0, q as u32);
                    regs.set(1, num.wrapping_rem(den) as u32);
                    regs.set(3, q.unsigned_abs());
                }
            }
            0x08 => regs.set(0, (g(0) as f64).sqrt() as u32),
            0x09 => {
                let x = g(0) as i16 as i32;
                let a = -((x * x) >> 14);
                let mut b = ((0xA9 * a) >> 14) + 0x390;
                for k in [0x91C, 0xFB6, 0x16AA, 0x2081, 0x3651, 0xA2F9] {
                    b = ((b * a) >> 14) + k;
                }
                regs.set(0, ((x * b) >> 16) as u32);
            }
            0x0A => {
                let (x, y) = (g(0) as i16 as f64, g(1) as i16 as f64);
                let ang = y.atan2(x);
                let v = ((ang / std::f64::consts::TAU) * 65536.0).rem_euclid(65536.0) as u32;
                regs.set(0, v & 0xFFFF);
            }
            0x0B | 0x0C => {
                let (mut s, mut d, c) = (g(0), g(1), g(2));
                let fill = c & (1 << 24) != 0;
                let fast = n == 0x0C;
                let word = fast || c & (1 << 26) != 0;
                let mut count = c & 0x1F_FFFF;
                if fast {
                    count = (count + 7) & !7;
                }
                let unit = if word { 4 } else { 2 };
                let first = self.read(s & !(unit - 1), unit);
                for _ in 0..count {
                    let v = if fill { first } else { self.read(s & !(unit - 1), unit) };
                    self.write(d & !(unit - 1), v, unit);
                    if !fill {
                        s += unit;
                    }
                    d += unit;
                }
                self.extra += count * 2;
            }
            0x0D => regs.set(0, 0xBAAE_187F),
            0x0E => {
                let (mut s, mut d) = (g(0), g(1));
                for _ in 0..g(2) {
                    let ox = self.r32(s) as i32;
                    let oy = self.r32(s + 4) as i32;
                    let cx = self.r16(s + 8) as i16 as i32;
                    let cy = self.r16(s + 10) as i16 as i32;
                    let sx = self.r16(s + 12) as i16 as i32;
                    let sy = self.r16(s + 14) as i16 as i32;
                    let th = (self.r16(s + 16) >> 8) as f64 * std::f64::consts::TAU / 256.0;
                    let (sin, cos) = ((th.sin() * 16384.0) as i32, (th.cos() * 16384.0) as i32);
                    let pa = (sx * cos) >> 14;
                    let pb = -((sx * sin) >> 14);
                    let pc = (sy * sin) >> 14;
                    let pd = (sy * cos) >> 14;
                    for (k, v) in [pa, pb, pc, pd].into_iter().enumerate() {
                        self.write(d + k as u32 * 2, v as u32 & 0xFFFF, 2);
                    }
                    self.write(d + 8, (ox - pa * cx - pb * cy) as u32, 4);
                    self.write(d + 12, (oy - pc * cx - pd * cy) as u32, 4);
                    s += 20;
                    d += 16;
                }
            }
            0x0F => {
                let (mut s, mut d, stride) = (g(0), g(1), g(3));
                for _ in 0..g(2) {
                    let sx = self.r16(s) as i16 as i32;
                    let sy = self.r16(s + 2) as i16 as i32;
                    let th = (self.r16(s + 4) >> 8) as f64 * std::f64::consts::TAU / 256.0;
                    let (sin, cos) = ((th.sin() * 16384.0) as i32, (th.cos() * 16384.0) as i32);
                    let vals = [(sx * cos) >> 14, -((sx * sin) >> 14), (sy * sin) >> 14, (sy * cos) >> 14];
                    for (k, v) in vals.into_iter().enumerate() {
                        self.write(d + k as u32 * stride, v as u32 & 0xFFFF, 2);
                    }
                    s += 8;
                    d += stride * 4;
                }
            }
            0x10 => self.bit_unpack(g(0), g(1), g(2)),
            0x11 | 0x12 => {
                let data = self.lz77(g(0));
                self.store(g(1), &data, n == 0x12);
            }
            0x13 => {
                let data = self.huffman(g(0));
                self.store(g(1), &data, true);
            }
            0x14 | 0x15 => {
                let data = self.run_length(g(0));
                self.store(g(1), &data, n == 0x15);
            }
            0x16..=0x18 => {
                let s = g(0);
                let hdr = self.r32(s);
                let size = hdr >> 8;
                let wide = n == 0x18;
                let mut out = Vec::with_capacity(size as usize);
                let mut acc = 0u32;
                let mut i = 0;
                while (out.len() as u32) < size {
                    if wide {
                        acc = acc.wrapping_add(self.r16(s + 4 + i * 2));
                        out.extend_from_slice(&(acc as u16).to_le_bytes());
                    } else {
                        acc = acc.wrapping_add(self.r8(s + 4 + i));
                        out.push(acc as u8);
                    }
                    i += 1;
                }
                self.store(g(1), &out, n != 0x16);
            }
            0x1F => {
                let freq = self.r32(g(0) + 4) as f64;
                let key = g(1) as f64 + g(2) as f64 / 256.0;
                regs.set(0, (freq / 2f64.powf((180.0 - key) / 12.0)) as u32);
            }
            _ => {}
        }
    }

    fn store(&mut self, dest: u32, data: &[u8], halfwords: bool) {
        if halfwords {
            for (i, pair) in data.chunks(2).enumerate() {
                let v = pair[0] as u32 | (*pair.get(1).unwrap_or(&0) as u32) << 8;
                self.write((dest & !1) + i as u32 * 2, v, 2);
            }
        } else {
            for (i, &b) in data.iter().enumerate() {
                self.write(dest + i as u32, b as u32, 1);
            }
        }
        self.extra += data.len() as u32;
    }

    fn lz77(&mut self, src: u32) -> Vec<u8> {
        let size = (self.r32(src) >> 8) as usize;
        let mut out: Vec<u8> = Vec::with_capacity(size);
        let mut p = src + 4;
        while out.len() < size {
            let flags = self.r8(p);
            p += 1;
            for bit in (0..8).rev() {
                if out.len() >= size {
                    break;
                }
                if flags & (1 << bit) != 0 {
                    let b0 = self.r8(p);
                    let b1 = self.r8(p + 1);
                    p += 2;
                    let len = (b0 >> 4) as usize + 3;
                    let disp = (((b0 & 0xF) << 8) | b1) as usize + 1;
                    for _ in 0..len {
                        let v = if disp <= out.len() { out[out.len() - disp] } else { 0 };
                        out.push(v);
                    }
                } else {
                    let v = self.r8(p) as u8;
                    p += 1;
                    out.push(v);
                }
            }
        }
        out.truncate(size);
        out
    }

    fn run_length(&mut self, src: u32) -> Vec<u8> {
        let size = (self.r32(src) >> 8) as usize;
        let mut out = Vec::with_capacity(size);
        let mut p = src + 4;
        while out.len() < size {
            let f = self.r8(p);
            p += 1;
            if f & 0x80 != 0 {
                let v = self.r8(p) as u8;
                p += 1;
                for _ in 0..(f & 0x7F) + 3 {
                    out.push(v);
                }
            } else {
                for _ in 0..(f & 0x7F) + 1 {
                    let v = self.r8(p) as u8;
                    p += 1;
                    out.push(v);
                }
            }
        }
        out.truncate(size);
        out
    }

    fn huffman(&mut self, src: u32) -> Vec<u8> {
        let hdr = self.r32(src);
        let bits = (hdr & 0xF) as u32;
        let bits = if bits == 4 || bits == 8 { bits } else { 8 };
        let size = (hdr >> 8) as usize;
        let tree_size = (self.r8(src + 4) + 1) * 2;
        let root = src + 5;
        let mut stream = src + 4 + tree_size;
        let mut out = Vec::with_capacity(size);
        let (mut acc, mut acc_bits) = (0u32, 0u32);
        let mut node_addr = root;
        let mut node = self.r8(node_addr);
        'outer: while out.len() < size {
            let word = self.r32(stream);
            stream += 4;
            for i in (0..32).rev() {
                let bit = (word >> i) & 1;
                let child = (node_addr & !1) + ((node & 0x3F) + 1) * 2 + bit;
                let is_data = node & (if bit == 0 { 0x80 } else { 0x40 }) != 0;
                if is_data {
                    let v = self.r8(child);
                    acc |= v << acc_bits;
                    acc_bits += bits;
                    if acc_bits >= 8 {
                        out.push(acc as u8);
                        acc >>= 8;
                        acc_bits -= 8;
                        if out.len() >= size {
                            break 'outer;
                        }
                    }
                    node_addr = root;
                    node = self.r8(root);
                } else {
                    node_addr = child;
                    node = self.r8(child);
                }
            }
        }
        out
    }

    fn bit_unpack(&mut self, src: u32, dst: u32, info: u32) {
        let len = self.r16(info);
        let sw = self.r8(info + 2);
        let dw = self.r8(info + 3);
        let off = self.r32(info + 4);
        let zero_flag = off & 0x8000_0000 != 0;
        let off = off & 0x7FFF_FFFF;
        if !matches!(sw, 1 | 2 | 4 | 8) || !matches!(dw, 1 | 2 | 4 | 8 | 16 | 32) {
            return;
        }
        let (mut acc, mut acc_bits, mut d) = (0u32, 0u32, dst);
        for i in 0..len {
            let byte = self.r8(src + i);
            for k in 0..(8 / sw) {
                let mut v = (byte >> (k * sw)) & ((1 << sw) - 1);
                if v != 0 || zero_flag {
                    v += off;
                }
                acc |= (v & if dw == 32 { u32::MAX } else { (1 << dw) - 1 }) << acc_bits;
                acc_bits += dw;
                if acc_bits >= 32 {
                    self.write(d, acc, 4);
                    d += 4;
                    acc = 0;
                    acc_bits = 0;
                }
            }
        }
    }
}

impl Bus for Hw {
    fn read(&mut self, a: u32, size: u32) -> u32 {
        match a >> 24 {
            0x00 => {
                if a < 0x4000 {
                    rd(&self.bios, a as usize, size)
                } else {
                    0
                }
            }
            0x02 => {
                self.extra += 2;
                rd(&self.ewram, (a & 0x3_FFFF) as usize, size)
            }
            0x03 => rd(&self.iwram, (a & 0x7FFF) as usize, size),
            0x04 => {
                let off = (a & 0x00FF_FFFF) as usize;
                if off >= 0x400 {
                    return 0;
                }
                (0..size).fold(0, |v, i| v | (self.io_read8(off + i as usize) as u32) << (8 * i))
            }
            0x05 => rd(&self.ppu.pal, (a & 0x3FF) as usize, size),
            0x06 => {
                let mut off = (a & 0x1_FFFF) as usize;
                if off >= 0x18000 {
                    off -= 0x8000;
                }
                rd(&self.ppu.vram, off, size)
            }
            0x07 => rd(&self.ppu.oam, (a & 0x3FF) as usize, size),
            0x08..=0x0D => {
                if a >> 24 == 0x0D && self.save.kind == save::Kind::Eeprom && (self.rom.len() <= 0x100_0000 || a & 0x00FF_FF00 == 0x00FF_FF00) {
                    return self.save.ee_read() as u32;
                }
                self.extra += if self.io16(0x204) & 0x4000 != 0 { 1 } else { 3 };
                let off = (a & 0x01FF_FFFF) as usize;
                if self.rtc.present && self.rtc.readable && (0xC4..=0xC9).contains(&off) {
                    let v = self.rtc.read(off as u32 & !1) as u32;
                    return if size == 1 && off & 1 == 1 { v >> 8 } else { v };
                }
                if off + size as usize <= self.rom.len() {
                    rd(&self.rom, off, size)
                } else {
                    // Open bus past the end of the ROM returns the address pattern.
                    let h = (off / 2) as u32 & 0xFFFF;
                    match size {
                        4 => h | (h + 1) << 16,
                        2 => h,
                        _ => (h >> ((off & 1) * 8)) & 0xFF,
                    }
                }
            }
            0x0E | 0x0F => {
                self.extra += 4;
                let b = self.save.read8(a & 0xFFFF) as u32;
                b * match size {
                    4 => 0x0101_0101,
                    2 => 0x0101,
                    _ => 1,
                }
            }
            _ => 0,
        }
    }

    fn write(&mut self, a: u32, v: u32, size: u32) {
        if a >> 24 == 0x08 && self.rtc.present && (0xC4..=0xC9).contains(&(a & 0x01FF_FFFF)) {
            self.rtc.write(a & 0xFE, v as u16);
            return;
        }
        match a >> 24 {
            0x02 => {
                self.extra += 2;
                wr(&mut self.ewram, (a & 0x3_FFFF) as usize, v, size)
            }
            0x03 => wr(&mut self.iwram, (a & 0x7FFF) as usize, v, size),
            0x04 => {
                let off = (a & 0x00FF_FFFF) as usize;
                for i in 0..size as usize {
                    self.io_write8(off + i, (v >> (8 * i)) as u8);
                }
            }
            0x05 => {
                let off = (a & 0x3FF) as usize;
                if size == 1 {
                    wr(&mut self.ppu.pal, off & !1, (v & 0xFF) * 0x101, 2)
                } else {
                    wr(&mut self.ppu.pal, off, v, size)
                }
            }
            0x06 => {
                let mut off = (a & 0x1_FFFF) as usize;
                if off >= 0x18000 {
                    off -= 0x8000;
                }
                if size == 1 {
                    let obj_start = if self.io[0] & 7 >= 3 { 0x14000 } else { 0x10000 };
                    if off < obj_start {
                        wr(&mut self.ppu.vram, off & !1, (v & 0xFF) * 0x101, 2);
                    }
                } else {
                    wr(&mut self.ppu.vram, off, v, size)
                }
            }
            0x07 => {
                if size != 1 {
                    wr(&mut self.ppu.oam, (a & 0x3FF) as usize, v, size)
                }
            }
            0x0D if self.save.kind == save::Kind::Eeprom => self.save.ee_write(v as u16),
            0x0E | 0x0F => self.save.write8(a & 0xFFFF, (v >> ((a & 3) * 8)) as u8),
            _ => {}
        }
    }

    fn swi(&mut self, n: u32, regs: &mut Regs) -> bool {
        self.bios_call(n & 0xFF, regs);
        true
    }
}

// ---------------------------------------------------------------------- the console

pub struct Gba {
    cpu: Cpu<Hw>,
    irq: bool,
    rate: u32,
    resample: f64,
}

impl state::Stateful for Gba {
    fn sync(&mut self, s: &mut state::State) {
        s.check(0x4742_4100 ^ self.cpu.bus().rom_len() as u32);
        self.cpu.sync_state(s);
        self.cpu.bus_mut().sync(s);
        self.irq.sync(s);
    }
}

impl Gba {
    pub fn from_rom(rom: Vec<u8>) -> Result<Gba, String> {
        if rom.len() < 0xC0 {
            return Err("This file is too small to be a Game Boy Advance game.".into());
        }
        let kind = save::detect(&rom);
        let rom_len = rom.len();
        let rtc = rtc::Rtc::for_rom(&rom);
        let mut hw = Hw {
            bios: bios_image(),
            ewram: vec![0; 0x4_0000],
            iwram: vec![0; 0x8000],
            io: vec![0; 0x400],
            ppu: ppu::Ppu::new(),
            rom,
            save: save::Save::new(kind, rom_len),
            rtc,
            dma: [Dma::default(); 4],
            timers: [Timer::default(); 4],
            apu: gb::apu::Apu::default(),
            apu_div: 0,
            fifo: [VecDeque::new(), VecDeque::new()],
            ds: [0; 2],
            out: Vec::new(),
            keys: 0,
            halted: false,
            intr_wait: false,
            extra: 0,
        };
        // Register state the real BIOS leaves behind.
        hw.io[0x20] = 0x00;
        hw.io[0x21] = 0x01; // BG2PA = 1.0
        hw.io[0x26] = 0x00;
        hw.io[0x27] = 0x01; // BG2PD = 1.0
        hw.io[0x30] = 0x00;
        hw.io[0x31] = 0x01;
        hw.io[0x36] = 0x00;
        hw.io[0x37] = 0x01;
        hw.io[0x88] = 0x00;
        hw.io[0x89] = 0x02; // SOUNDBIAS
        hw.io[0x300] = 1; // POSTFLG
        let mut cpu = Cpu::new(hw);
        cpu.reset(0x0800_0000);
        Ok(Gba { cpu, irq: false, rate: 48_000, resample: 0.0 })
    }

    pub fn title(&self) -> String {
        let t = &self.cpu.bus().rom[0xA0..0xAC];
        String::from_utf8_lossy(t).trim_end_matches('\0').trim().to_string()
    }

    pub fn reset(&mut self) {
        let rom = std::mem::take(&mut self.cpu.bus_mut().rom);
        let save = std::mem::take(&mut self.cpu.bus_mut().save.data);
        if let Ok(mut g) = Gba::from_rom(rom) {
            g.load_save_ram(&save);
            g.set_sample_rate(self.rate);
            *self = g;
        }
    }

    fn update_irq(&mut self) {
        let hw = self.cpu.bus();
        let on = hw.io[0x208] & 1 != 0 && hw.irq_pending();
        if on != self.irq {
            self.irq = on;
            self.cpu.set_irq(on);
        }
    }

    fn run_cycles(&mut self, budget: u32) {
        let mut left = budget as i64;
        while left > 0 {
            let hw = self.cpu.bus_mut();
            let c = if hw.halted {
                if hw.irq_pending() {
                    hw.halted = false;
                    0
                } else {
                    (left as u32).min(32)
                }
            } else {
                0
            };
            let c = if c == 0 && !self.cpu.bus().halted {
                let c = self.cpu.step();
                c + std::mem::take(&mut self.cpu.bus_mut().extra)
            } else {
                c.max(1)
            };
            self.cpu.bus_mut().tick(c);
            self.update_irq();
            left -= c as i64;
        }
    }

    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    pub fn run_frame(&mut self) {
        for line in 0..228usize {
            {
                let hw = self.cpu.bus_mut();
                hw.io[6] = line as u8;
                hw.io[7] = 0;
                let stat = hw.io16(4);
                let vmatch = (stat >> 8) as usize == line;
                let mut s = stat & !0x7;
                if (160..227).contains(&line) {
                    s |= 1;
                }
                if vmatch {
                    s |= 4;
                }
                hw.set_io16(4, s);
                if line == 160 {
                    if stat & 8 != 0 {
                        hw.raise(0);
                    }
                    hw.trigger_dma(1);
                    let io = std::mem::take(&mut hw.io);
                    hw.ppu.latch_ref(&io, 0);
                    hw.ppu.latch_ref(&io, 1);
                    hw.io = io;
                }
                if vmatch && stat & 0x20 != 0 {
                    hw.raise(2);
                }
            }
            self.update_irq();
            self.run_cycles(960);
            {
                let hw = self.cpu.bus_mut();
                let s = hw.io16(4) | 2;
                hw.set_io16(4, s);
                if line < 160 {
                    let io = std::mem::take(&mut hw.io);
                    hw.ppu.render_line(line, &io);
                    hw.io = io;
                    hw.trigger_dma(2);
                }
                if s & 0x10 != 0 {
                    hw.raise(1);
                }
            }
            self.update_irq();
            self.run_cycles(272);
        }
    }

    pub fn frame(&self) -> &[u32] {
        self.cpu.bus().ppu.frame()
    }

    pub fn set_buttons(&mut self, b: u16) {
        self.cpu.bus_mut().keys = b & 0x3FF;
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.rate = rate;
        self.cpu.bus_mut().apu.set_sample_rate(rate);
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        let _ = self.resample;
        out.append(&mut self.cpu.bus_mut().out);
    }

    pub fn save_ram(&self) -> Option<&[u8]> {
        Some(&self.cpu.bus().save.data)
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        self.cpu.bus_mut().save.load(data);
    }

    pub fn pc(&self) -> u32 {
        self.cpu.reg(15)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tiny ROM from ARM words at 0x08000000 (header area left zero apart from the jump).
    fn rom(code: &[u32]) -> Vec<u8> {
        let mut r = vec![0u8; 0x400];
        for (i, w) in code.iter().enumerate() {
            r[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        r
    }

    #[test]
    fn mode3_pixel_reaches_the_screen() {
        // r0 = 0x04000000 ; r1 = 0x0403 (mode 3 + BG2) ; strh r1,[r0]
        // r2 = 0x06000000 ; r3 = 0x001F (red) ; strh r3,[r2] ; b .
        let g = rom(&[0xE3A00301, 0xE3A01B01, 0xE2811003, 0xE1C010B0, 0xE3A02406, 0xE3A0301F, 0xE1C230B0, 0xEAFFFFFE]);
        let mut gba = Gba::from_rom(g).unwrap();
        gba.run_frame();
        assert_eq!(gba.frame()[0], 0xFF0000);
        assert_eq!(gba.frame()[1], 0x000000);
    }

    #[test]
    fn vblank_interrupt_through_our_bios() {
        // Install handler address (0x08000100) at 0x03007FFC, enable VBlank IRQ, then
        // VBlankIntrWait in a loop; the handler sets BIOS_IF and counts in r5 via memory.
        let mut code = vec![
            0xE3A00403, // mov r0,#0x03000000
            0xE2800C7F, // add r0,r0,#0x7F00
            0xE28000FC, // add r0,r0,#0xFC   -> 0x03007FFC
            0xE3A01302, // mov r1,#0x08000000
            0xE2811C01, // add r1,r1,#0x100  -> handler
            0xE5801000, // str r1,[r0]
            0xE3A00301, // mov r0,#0x04000000
            0xE3A01008, // mov r1,#8          DISPSTAT vblank irq
            0xE1C010B4, // strh r1,[r0,#4]
            0xE2802C02, // add r2,r0,#0x200
            0xE3A01001, // mov r1,#1
            0xE1C210B0, // strh r1,[r2]       IE = vblank
            0xE1C210B8, // strh r1,[r2,#8]    IME = 1
            0xEF050000, // swi 0x05 (VBlankIntrWait)
            0xE2844001, // add r4,r4,#1
            0xEAFFFFFC, // b swi
        ];
        code.resize(0x40, 0);
        // Handler at 0x100: ack IF and BIOS_IF, return.
        code.extend_from_slice(&[
            0xE3A00301, // mov r0,#0x04000000
            0xE2800C02, // add r0,r0,#0x200
            0xE3A01001, // mov r1,#1
            0xE1C010B2, // strh r1,[r0,#2]   IF ack
            0xE3A00403, // mov r0,#0x03000000
            0xE2800C7F, // add r0,r0,#0x7F00
            0xE28000F8, // add r0,r0,#0xF8   -> 0x03007FF8 (BIOS_IF)
            0xE1C010B0, // strh r1,[r0]
            0xE12FFF1E, // bx lr
        ]);
        let mut gba = Gba::from_rom(rom(&code)).unwrap();
        for _ in 0..5 {
            gba.run_frame();
        }
        let r4 = gba.cpu.reg(4);
        assert!((4..=5).contains(&r4), "one wake-up per frame, got {r4}");
    }

    #[test]
    fn save_state_resumes_identically() {
        // The VBlank counter program from above, with the count kept in r4.
        let mut code = vec![
            0xE3A00403, 0xE2800C7F, 0xE28000FC, 0xE3A01302, 0xE2811C01, 0xE5801000, 0xE3A00301, 0xE3A01008, 0xE1C010B4, 0xE2802C02, 0xE3A01001, 0xE1C210B0, 0xE1C210B8, 0xEF050000, 0xE2844001, 0xEAFFFFFC,
        ];
        code.resize(0x40, 0);
        code.extend_from_slice(&[0xE3A00301, 0xE2800C02, 0xE3A01001, 0xE1C010B2, 0xE3A00403, 0xE2800C7F, 0xE28000F8, 0xE1C010B0, 0xE12FFF1E]);
        let image = rom(&code);
        let mut a = Gba::from_rom(image.clone()).unwrap();
        for _ in 0..4 {
            a.run_frame();
        }
        let at_snap = a.cpu.reg(4);
        let snap = a.save_state();
        for _ in 0..6 {
            a.run_frame();
        }
        assert!(a.cpu.reg(4) > at_snap, "the game kept running after the snapshot");
        let want = (a.cpu.reg(4), a.cpu.reg(15), a.frame().to_vec());
        let mut b = Gba::from_rom(image).unwrap();
        b.load_state(&snap).unwrap();
        assert_eq!(b.cpu.reg(4), at_snap, "the interrupt count was restored mid-run");
        for _ in 0..6 {
            b.run_frame();
        }
        assert_eq!((b.cpu.reg(4), b.cpu.reg(15), b.frame().to_vec()), want);
    }

    #[test]
    fn lz77_decompression() {
        let mut gba = Gba::from_rom(rom(&[0xEAFFFFFE])).unwrap();
        // "ABCABCABC": literal A B C, then a copy of 6 bytes from 3 back.
        let src = [0x10, 9, 0, 0, 0x10, b'A', b'B', b'C', 0x30, 0x02];
        let hw = gba.cpu.bus_mut();
        hw.ewram[..src.len()].copy_from_slice(&src);
        let out = hw.lz77(0x0200_0000);
        assert_eq!(out, b"ABCABCABC");
    }

    #[test]
    fn flash_save_protocol() {
        let mut s = save::Save::new(save::Kind::Flash64, 0);
        s.write8(0x5555, 0xAA);
        s.write8(0x2AAA, 0x55);
        s.write8(0x5555, 0x90);
        assert_eq!((s.read8(0), s.read8(1)), (0x32, 0x1B));
        s.write8(0x5555, 0xAA);
        s.write8(0x2AAA, 0x55);
        s.write8(0x5555, 0xF0);
        s.write8(0x5555, 0xAA);
        s.write8(0x2AAA, 0x55);
        s.write8(0x5555, 0xA0);
        s.write8(0x1234, 0x5A);
        assert_eq!(s.read8(0x1234), 0x5A);
    }
}
