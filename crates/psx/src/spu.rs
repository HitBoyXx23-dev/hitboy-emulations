//! Sound Processing Unit: 24 ADPCM voices with ADSR envelopes and pitch, 512 KiB sound RAM,
//! noise, and CD audio mixing. Output is 44.1 kHz stereo, resampled to the host rate.

use std::collections::VecDeque;

const RAM: usize = 512 * 1024;
const POS: [i32; 5] = [0, 60, 115, 98, 122];
const NEG: [i32; 5] = [0, 0, -52, -55, -60];

#[derive(Clone, Copy, PartialEq, state::Stateful)]
enum Phase {
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy, state::Stateful)]
struct Voice {
    vol_l: i16,
    vol_r: i16,
    pitch: u16,
    start: u32,
    repeat: u32,
    addr: u32,
    adsr1: u16,
    adsr2: u16,
    level: i32,
    phase: Phase,
    env_count: u32,
    counter: u32,
    block: [i16; 28],
    hist: [i32; 2],
    prev: i16,
    flags: u8,
    out: i32,
}

impl Default for Voice {
    fn default() -> Self {
        Voice { vol_l: 0, vol_r: 0, pitch: 0, start: 0, repeat: 0, addr: 0, adsr1: 0, adsr2: 0, level: 0, phase: Phase::Off, env_count: 0, counter: 0, block: [0; 28], hist: [0; 2], prev: 0, flags: 0, out: 0 }
    }
}

#[derive(state::Stateful)]
pub struct Spu {
    ram: Vec<u8>,
    regs: [u16; 0x200],
    v: [Voice; 24],
    ctrl: u16,
    trans_addr: u32,
    endx: u32,
    pmon: u32,
    non: u32,
    main_l: i16,
    main_r: i16,
    cd_l: i16,
    cd_r: i16,
    noise: i32,
    noise_count: i32,
    cycles: u32,
    rate: u32,
    phase: u32,
    #[state(skip)]
    out: Vec<f32>,
    pub irq: bool,
}

impl Default for Spu {
    fn default() -> Self {
        Self::new()
    }
}

fn vol(v: u16) -> i16 {
    if v & 0x8000 != 0 {
        0x3FFF // sweep mode: treated as a steady level
    } else {
        ((v << 1) as i16) >> 1
    }
}

impl Spu {
    pub fn new() -> Self {
        Spu {
            ram: vec![0; RAM],
            regs: [0; 0x200],
            v: [Voice::default(); 24],
            ctrl: 0,
            trans_addr: 0,
            endx: 0,
            pmon: 0,
            non: 0,
            main_l: 0,
            main_r: 0,
            cd_l: 0,
            cd_r: 0,
            noise: 1,
            noise_count: 0,
            cycles: 0,
            rate: 44_100,
            phase: 0,
            out: Vec::new(),
            irq: false,
        }
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.rate = r.max(8000);
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.out);
    }

    // ------------------------------------------------------------------ registers

    pub fn read(&mut self, addr: u32) -> u16 {
        let off = (addr - 0x1F80_1C00) as usize & 0x3FE;
        match off {
            o if o < 0x180 => {
                let (n, r) = (o / 16, o % 16);
                match r {
                    0x0C => self.v[n].level as u16,
                    0x0E => (self.v[n].repeat / 8) as u16,
                    _ => self.regs[o / 2],
                }
            }
            0x19C => self.endx as u16,
            0x19E => (self.endx >> 16) as u16,
            0x1A6 => (self.trans_addr / 8) as u16,
            0x1AA => self.ctrl,
            0x1AE => (self.ctrl & 0x3F) | if self.irq { 0x40 } else { 0 },
            _ => self.regs[off / 2],
        }
    }

    pub fn write(&mut self, addr: u32, val: u16) {
        let off = (addr - 0x1F80_1C00) as usize & 0x3FE;
        self.regs[off / 2] = val;
        if off < 0x180 {
            let (n, r) = (off / 16, off % 16);
            let v = &mut self.v[n];
            match r {
                0x0 => v.vol_l = vol(val),
                0x2 => v.vol_r = vol(val),
                0x4 => v.pitch = val,
                0x6 => v.start = val as u32 * 8,
                0x8 => v.adsr1 = val,
                0xA => v.adsr2 = val,
                0xC => v.level = val as i16 as i32,
                0xE => v.repeat = val as u32 * 8,
                _ => {}
            }
            return;
        }
        match off {
            0x180 => self.main_l = vol(val),
            0x182 => self.main_r = vol(val),
            0x188 => self.key_on(val as u32),
            0x18A => self.key_on((val as u32) << 16),
            0x18C => self.key_off(val as u32),
            0x18E => self.key_off((val as u32) << 16),
            0x190 => self.pmon = (self.pmon & 0xFFFF_0000) | val as u32,
            0x192 => self.pmon = (self.pmon & 0xFFFF) | (val as u32) << 16,
            0x194 => self.non = (self.non & 0xFFFF_0000) | val as u32,
            0x196 => self.non = (self.non & 0xFFFF) | (val as u32) << 16,
            0x1A6 => self.trans_addr = val as u32 * 8,
            0x1A8 => self.fifo_write(val),
            0x1AA => {
                self.ctrl = val;
                if val & 0x40 == 0 {
                    self.irq = false;
                }
            }
            0x1B0 => self.cd_l = val as i16,
            0x1B2 => self.cd_r = val as i16,
            _ => {}
        }
    }

    fn fifo_write(&mut self, val: u16) {
        let a = self.trans_addr as usize & (RAM - 1);
        self.ram[a] = val as u8;
        self.ram[(a + 1) & (RAM - 1)] = (val >> 8) as u8;
        self.trans_addr = (self.trans_addr + 2) & (RAM as u32 - 1);
    }

    pub fn dma_write(&mut self, w: u32) {
        self.fifo_write(w as u16);
        self.fifo_write((w >> 16) as u16);
    }

    pub fn dma_read(&mut self) -> u32 {
        let a = self.trans_addr as usize & (RAM - 1);
        let v = u32::from_le_bytes([self.ram[a], self.ram[(a + 1) & (RAM - 1)], self.ram[(a + 2) & (RAM - 1)], self.ram[(a + 3) & (RAM - 1)]]);
        self.trans_addr = (self.trans_addr + 4) & (RAM as u32 - 1);
        v
    }

    fn key_on(&mut self, bits: u32) {
        for i in 0..24 {
            if bits & (1 << i) != 0 {
                let v = &mut self.v[i];
                v.addr = v.start;
                v.counter = 0;
                v.hist = [0; 2];
                v.prev = 0;
                v.level = 0;
                v.env_count = 0;
                v.phase = Phase::Attack;
                self.endx &= !(1 << i);
                Self::decode(&self.ram, v);
            }
        }
    }

    fn key_off(&mut self, bits: u32) {
        for i in 0..24 {
            if bits & (1 << i) != 0 && self.v[i].phase != Phase::Off {
                self.v[i].phase = Phase::Release;
                self.v[i].env_count = 0;
            }
        }
    }

    // ------------------------------------------------------------------ synthesis

    fn decode(ram: &[u8], v: &mut Voice) {
        let a = v.addr as usize & (RAM - 1) & !0xF;
        let hdr = ram[a];
        v.flags = ram[a + 1];
        let shift = (hdr & 0xF).min(12) as u32;
        let f = ((hdr >> 4) & 7).min(4) as usize;
        if v.flags & 4 != 0 {
            v.repeat = a as u32;
        }
        for i in 0..28 {
            let b = ram[a + 2 + i / 2];
            let n = if i & 1 == 0 { b & 0xF } else { b >> 4 };
            let raw = (((n as i32) << 28) >> 16) >> shift;
            let s = (raw + ((v.hist[0] * POS[f] + v.hist[1] * NEG[f] + 32) >> 6)).clamp(-32768, 32767);
            v.hist[1] = v.hist[0];
            v.hist[0] = s;
            v.block[i] = s as i16;
        }
    }

    fn envelope(v: &mut Voice) {
        let (shift, step, dec, exp) = match v.phase {
            Phase::Off => return,
            Phase::Attack => (((v.adsr1 >> 10) & 0x1F) as i32, 7 - ((v.adsr1 >> 8) & 3) as i32, false, v.adsr1 & 0x8000 != 0),
            Phase::Decay => (((v.adsr1 >> 4) & 0xF) as i32, -8, true, true),
            Phase::Sustain => {
                let dec = v.adsr2 & 0x4000 != 0;
                let st = ((v.adsr2 >> 6) & 3) as i32;
                (((v.adsr2 >> 8) & 0x1F) as i32, if dec { -8 + st } else { 7 - st }, dec, v.adsr2 & 0x8000 != 0)
            }
            Phase::Release => ((v.adsr2 & 0x1F) as i32, -8, true, v.adsr2 & 0x20 != 0),
        };
        let mut cycles = 1u32 << (shift - 11).max(0);
        let mut step = step << (11 - shift).max(0);
        if exp && !dec && v.level > 0x6000 {
            cycles *= 4;
        }
        if exp && dec {
            step = step * v.level >> 15;
        }
        v.env_count += 1;
        if v.env_count < cycles {
            return;
        }
        v.env_count = 0;
        v.level = (v.level + step).clamp(0, 0x7FFF);
        match v.phase {
            Phase::Attack if v.level >= 0x7FFF => v.phase = Phase::Decay,
            Phase::Decay if v.level <= (((v.adsr1 & 0xF) as i32 + 1) * 0x800).min(0x7FFF) => v.phase = Phase::Sustain,
            Phase::Release if v.level == 0 => v.phase = Phase::Off,
            _ => {}
        }
    }

    fn sample(&mut self, cd: Option<(i16, i16)>) -> (i32, i32) {
        // Noise generator (shared by all voices with NON set).
        let nshift = ((self.ctrl >> 10) & 0xF) as i32;
        let nstep = ((self.ctrl >> 8) & 3) as i32 + 4;
        self.noise_count += nstep << 2;
        if self.noise_count >= (0x20000 >> nshift) {
            self.noise_count = 0;
            let n = self.noise;
            let bit = ((n >> 15) ^ (n >> 12) ^ (n >> 11) ^ (n >> 10) ^ 1) & 1;
            self.noise = ((n << 1) | bit) & 0xFFFF;
        }
        let (mut l, mut r) = (0i32, 0i32);
        let mut prev_out = 0i32;
        for i in 0..24 {
            let ram = &self.ram;
            let v = &mut self.v[i];
            if v.phase == Phase::Off {
                v.out = 0;
                prev_out = 0;
                continue;
            }
            let idx = (v.counter >> 12) as usize;
            let a = if idx == 0 { v.prev as i32 } else { v.block[idx - 1] as i32 };
            let b = v.block[idx.min(27)] as i32;
            let frac = (v.counter & 0xFFF) as i32;
            let s = if self.non & (1 << i) != 0 { self.noise as i16 as i32 } else { a + ((b - a) * frac >> 12) };
            let o = s * v.level >> 15;
            v.out = o;
            l += o * v.vol_l as i32 >> 15;
            r += o * v.vol_r as i32 >> 15;
            let mut step = v.pitch as i32;
            if i > 0 && self.pmon & (1 << i) != 0 {
                step = (step * (prev_out + 0x8000) >> 15).clamp(0, 0xFFFF);
            }
            prev_out = o;
            v.counter += step.min(0x4000) as u32;
            while v.counter >= 28 << 12 {
                v.counter -= 28 << 12;
                v.prev = v.block[27];
                if v.flags & 1 != 0 {
                    self.endx |= 1 << i;
                    v.addr = v.repeat;
                    if v.flags & 2 == 0 {
                        v.phase = Phase::Off;
                        v.level = 0;
                    }
                } else {
                    v.addr = (v.addr + 16) & (RAM as u32 - 1);
                }
                Self::decode(ram, v);
            }
            Self::envelope(v);
        }
        l = l * self.main_l as i32 >> 15;
        r = r * self.main_r as i32 >> 15;
        if let Some((cl, cr)) = cd {
            if self.ctrl & 1 != 0 {
                l += cl as i32;
                r += cr as i32;
            }
        }
        // SPUCNT bit 14 is "unmute": clear means silence.
        if self.ctrl & 0x4000 == 0 {
            (0, 0)
        } else {
            (l, r)
        }
    }

    /// Advances by CPU cycles; pulls CD audio from `cd` as needed.
    pub fn tick(&mut self, cycles: u32, cd: &mut VecDeque<(i16, i16)>) {
        self.cycles += cycles;
        while self.cycles >= 768 {
            self.cycles -= 768;
            if self.ctrl & 0x8000 == 0 {
                continue;
            }
            let c = cd.pop_front();
            let (l, r) = self.sample(c);
            // 44.1 kHz -> host rate (sample-and-hold resampling).
            self.phase += self.rate;
            while self.phase >= 44_100 {
                self.phase -= 44_100;
                self.out.push(l.clamp(-32768, 32767) as f32 / 32768.0);
                self.out.push(r.clamp(-32768, 32767) as f32 / 32768.0);
            }
        }
        if self.out.len() > self.rate as usize {
            let n = self.out.len() - self.rate as usize;
            self.out.drain(..n & !1);
        }
    }
}
