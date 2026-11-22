//! Yamaha YM2612 (OPN2): 6 FM channels of 4 operators with 8 algorithms, feedback, ADSR
//! envelopes, multipliers, stereo panning, the DAC on channel 6, and timers A/B.
//! Generated at the chip's native rate (master clock / 144 ≈ 53.3 kHz).

use std::f32::consts::PI;

pub const RATE: f64 = 7_670_454.0 / 144.0;

#[derive(Clone, Copy, PartialEq, state::Stateful)]
enum Stage {
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy, state::Stateful)]
struct Op {
    mul: u8,
    tl: u8,
    ar: u8,
    d1r: u8,
    d2r: u8,
    sl: u8,
    rr: u8,
    ks: u8,
    phase: u32,
    /// Attenuation 0 (loud) .. 1023 (silent), in steps of 0.09375 dB.
    env: f32,
    stage: Stage,
    out: [f32; 2],
}

impl Default for Op {
    fn default() -> Self {
        Op { mul: 1, tl: 127, ar: 0, d1r: 0, d2r: 0, sl: 0, rr: 0, ks: 0, phase: 0, env: 1023.0, stage: Stage::Release, out: [0.0; 2] }
    }
}

#[derive(Clone, Copy, Default, state::Stateful)]
struct Channel {
    ops: [Op; 4],
    fnum: u16,
    block: u8,
    fb: u8,
    alg: u8,
    left: bool,
    right: bool,
    latch_hi: u8,
}

#[derive(state::Stateful)]
pub struct Ym2612 {
    ch: [Channel; 6],
    addr: [u8; 2],
    dac: u8,
    dac_on: bool,
    timer_a: u16,
    timer_b: u8,
    timer_ctrl: u8,
    ta_count: i32,
    tb_count: i32,
    status: u8,
    #[state(skip)]
    pub samples: Vec<f32>,
    #[state(skip)]
    rate: f64,
    phase: f64,
    acc: (f32, f32),
    acc_n: u32,
    #[state(skip)]
    sine: Vec<f32>,
}

impl Default for Ym2612 {
    fn default() -> Self {
        Self::new()
    }
}

/// Samples for a full-range envelope sweep at effective rate `r` (0-63).
fn env_step(r: u32) -> f32 {
    if r < 4 {
        return 0.0;
    }
    // A full 96 dB decay takes about 110 s at rate 4, halving every 4 rate steps.
    let seconds = 110.0 / 2f32.powf((r as f32 - 4.0) / 4.0);
    1024.0 / (seconds * RATE as f32)
}

impl Ym2612 {
    pub fn new() -> Ym2612 {
        let sine = (0..1024).map(|i| (i as f32 / 1024.0 * 2.0 * PI).sin()).collect();
        let mut ch = [Channel::default(); 6];
        for c in &mut ch {
            c.left = true;
            c.right = true;
        }
        Ym2612 { ch, addr: [0; 2], dac: 0x80, dac_on: false, timer_a: 0, timer_b: 0, timer_ctrl: 0, ta_count: 0, tb_count: 0, status: 0, samples: Vec::new(), rate: 48_000.0, phase: 0.0, acc: (0.0, 0.0), acc_n: 0, sine }
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.rate = r as f64;
    }

    pub fn read_status(&self) -> u8 {
        self.status
    }

    /// Port write: `port` 0/2 set the address for part I/II, 1/3 write data.
    pub fn write(&mut self, port: u8, v: u8) {
        let part = ((port >> 1) & 1) as usize;
        if port & 1 == 0 {
            self.addr[part] = v;
            return;
        }
        let a = self.addr[part];
        if part == 0 && a < 0x30 {
            self.global(a, v);
            return;
        }
        let c = (a & 3) as usize;
        if c == 3 {
            return;
        }
        let ch = c + part * 3;
        if a >= 0xA0 {
            let chan = &mut self.ch[ch];
            match a & 0xFC {
                0xA0 => {
                    chan.fnum = (chan.latch_hi as u16 & 7) << 8 | v as u16;
                    chan.block = (chan.latch_hi >> 3) & 7;
                }
                0xA4 => chan.latch_hi = v,
                0xB0 => {
                    chan.fb = (v >> 3) & 7;
                    chan.alg = v & 7;
                }
                0xB4 => {
                    chan.left = v & 0x80 != 0;
                    chan.right = v & 0x40 != 0;
                }
                _ => {}
            }
            return;
        }
        // Operator registers: slot order in the register map is op1, op3, op2, op4.
        let slot = [0, 2, 1, 3][((a >> 2) & 3) as usize];
        let op = &mut self.ch[ch].ops[slot];
        match a & 0xF0 {
            0x30 => op.mul = v & 0xF,
            0x40 => op.tl = v & 0x7F,
            0x50 => {
                op.ar = v & 0x1F;
                op.ks = v >> 6;
            }
            0x60 => op.d1r = v & 0x1F,
            0x70 => op.d2r = v & 0x1F,
            0x80 => {
                op.sl = v >> 4;
                op.rr = v & 0xF;
            }
            _ => {}
        }
    }

    fn global(&mut self, a: u8, v: u8) {
        match a {
            0x24 => self.timer_a = (self.timer_a & 3) | (v as u16) << 2,
            0x25 => self.timer_a = (self.timer_a & 0x3FC) | (v as u16 & 3),
            0x26 => self.timer_b = v,
            0x27 => {
                if v & 0x10 != 0 {
                    self.status &= !1;
                }
                if v & 0x20 != 0 {
                    self.status &= !2;
                }
                if v & 1 != 0 && self.timer_ctrl & 1 == 0 {
                    self.ta_count = 1024 - self.timer_a as i32;
                }
                if v & 2 != 0 && self.timer_ctrl & 2 == 0 {
                    self.tb_count = (256 - self.timer_b as i32) * 16;
                }
                self.timer_ctrl = v;
            }
            0x28 => {
                let c = (v & 7) as usize;
                let ch = match c {
                    0..=2 => c,
                    4..=6 => c - 1,
                    _ => return,
                };
                for (i, op) in self.ch[ch].ops.iter_mut().enumerate() {
                    let on = v & (0x10 << i) != 0;
                    if on && op.stage == Stage::Release {
                        op.stage = Stage::Attack;
                        op.phase = 0;
                    } else if !on && op.stage != Stage::Release {
                        op.stage = Stage::Release;
                    }
                }
            }
            0x2A => self.dac = v,
            0x2B => self.dac_on = v & 0x80 != 0,
            _ => {}
        }
    }

    fn envelope(op: &mut Op, keycode: u32) {
        let rate = |r: u8| if r == 0 { 0 } else { (2 * r as u32 + (keycode >> (3 - op.ks))).min(63) };
        match op.stage {
            Stage::Attack => {
                let r = rate(op.ar);
                if r >= 62 {
                    op.env = 0.0;
                } else if r >= 4 {
                    // Exponential approach to full volume.
                    let k = env_step(r) / 1024.0 * 8.0;
                    op.env -= (op.env + 16.0) * k.min(1.0);
                }
                if op.env <= 0.0 {
                    op.env = 0.0;
                    op.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                op.env += env_step(rate(op.d1r));
                let sl = if op.sl == 15 { 1023.0 } else { op.sl as f32 * 32.0 };
                if op.env >= sl {
                    op.env = sl;
                    op.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => op.env = (op.env + env_step(rate(op.d2r))).min(1023.0),
            Stage::Release => op.env = (op.env + env_step(rate(op.rr * 2 + 1))).min(1023.0),
        }
    }

    fn op_out(&self, op: &Op, modulation: f32) -> f32 {
        let atten = op.env + op.tl as f32 * 8.0;
        if atten >= 1023.0 {
            return 0.0;
        }
        let amp = 10f32.powf(-(atten * 0.09375) / 20.0);
        let p = (op.phase >> 22) as f32 + modulation * 1024.0 / (2.0 * PI);
        let idx = (p as i32).rem_euclid(1024) as usize;
        self.sine[idx] * amp
    }

    fn channel_sample(&mut self, c: usize) -> f32 {
        if c == 5 && self.dac_on {
            return (self.dac as f32 - 128.0) / 128.0;
        }
        let chan = self.ch[c];
        let keycode = (chan.block as u32) << 2 | (chan.fnum as u32 >> 9) & 3;
        let inc = ((chan.fnum as u32) << chan.block) << 11 >> 1;
        let mut ch = chan;
        for op in ch.ops.iter_mut() {
            let mul = if op.mul == 0 { inc / 2 } else { inc.wrapping_mul(op.mul as u32) };
            op.phase = op.phase.wrapping_add(mul);
            Self::envelope(op, keycode);
        }
        const FB: [f32; 8] = [0.0, PI / 16.0, PI / 8.0, PI / 4.0, PI / 2.0, PI, 2.0 * PI, 4.0 * PI];
        let m = 4.0 * PI;
        let o = &ch.ops;
        let fbv = if ch.fb == 0 { 0.0 } else { (o[0].out[0] + o[0].out[1]) / 2.0 * FB[ch.fb as usize] };
        let s1 = self.op_out(&o[0], fbv);
        let (out, s2, s3, s4);
        match ch.alg {
            0 => {
                s2 = self.op_out(&o[1], s1 * m);
                s3 = self.op_out(&o[2], s2 * m);
                s4 = self.op_out(&o[3], s3 * m);
                out = s4;
            }
            1 => {
                s2 = self.op_out(&o[1], 0.0);
                s3 = self.op_out(&o[2], (s1 + s2) * m);
                s4 = self.op_out(&o[3], s3 * m);
                out = s4;
            }
            2 => {
                s2 = self.op_out(&o[1], 0.0);
                s3 = self.op_out(&o[2], s2 * m);
                s4 = self.op_out(&o[3], (s1 + s3) * m);
                out = s4;
            }
            3 => {
                s2 = self.op_out(&o[1], s1 * m);
                s3 = self.op_out(&o[2], 0.0);
                s4 = self.op_out(&o[3], (s2 + s3) * m);
                out = s4;
            }
            4 => {
                s2 = self.op_out(&o[1], s1 * m);
                s3 = self.op_out(&o[2], 0.0);
                s4 = self.op_out(&o[3], s3 * m);
                out = s2 + s4;
            }
            5 => {
                s2 = self.op_out(&o[1], s1 * m);
                s3 = self.op_out(&o[2], s1 * m);
                s4 = self.op_out(&o[3], s1 * m);
                out = s2 + s3 + s4;
            }
            6 => {
                s2 = self.op_out(&o[1], s1 * m);
                s3 = self.op_out(&o[2], 0.0);
                s4 = self.op_out(&o[3], 0.0);
                out = s2 + s3 + s4;
            }
            _ => {
                s2 = self.op_out(&o[1], 0.0);
                s3 = self.op_out(&o[2], 0.0);
                s4 = self.op_out(&o[3], 0.0);
                out = s1 + s2 + s3 + s4;
            }
        }
        let _ = (s2, s3, s4);
        ch.ops[0].out = [s1, ch.ops[0].out[0]];
        self.ch[c] = ch;
        out.clamp(-1.0, 1.0)
    }

    /// Advances the chip by one native sample and resamples to the host rate.
    pub fn clock_sample(&mut self) {
        if self.timer_ctrl & 1 != 0 {
            self.ta_count -= 1;
            if self.ta_count <= 0 {
                self.ta_count = 1024 - self.timer_a as i32;
                if self.timer_ctrl & 4 != 0 {
                    self.status |= 1;
                }
            }
        }
        if self.timer_ctrl & 2 != 0 {
            self.tb_count -= 1;
            if self.tb_count <= 0 {
                self.tb_count = (256 - self.timer_b as i32) * 16;
                if self.timer_ctrl & 8 != 0 {
                    self.status |= 2;
                }
            }
        }
        let (mut l, mut r) = (0.0, 0.0);
        for c in 0..6 {
            let s = self.channel_sample(c);
            if self.ch[c].left {
                l += s;
            }
            if self.ch[c].right {
                r += s;
            }
        }
        self.acc.0 += l / 6.0;
        self.acc.1 += r / 6.0;
        self.acc_n += 1;
        self.phase += self.rate;
        while self.phase >= RATE {
            self.phase -= RATE;
            let n = self.acc_n.max(1) as f32;
            self.samples.push(self.acc.0 / n);
            self.samples.push(self.acc.1 / n);
            self.acc = (0.0, 0.0);
            self.acc_n = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(y: &mut Ym2612, part: u8, a: u8, v: u8) {
        y.write(part * 2, a);
        y.write(part * 2 + 1, v);
    }

    #[test]
    fn silent_until_key_on_then_tone() {
        let mut y = Ym2612::new();
        for _ in 0..2000 {
            y.clock_sample();
        }
        assert!(y.samples.iter().all(|&s| s == 0.0));
        y.samples.clear();
        // Channel 0, algorithm 7 (all carriers), op4 loud with instant attack.
        w(&mut y, 0, 0xB0, 0x07);
        w(&mut y, 0, 0xB4, 0xC0);
        w(&mut y, 0, 0x3C, 0x01); // op4 MUL 1
        w(&mut y, 0, 0x4C, 0x00); // op4 TL 0
        w(&mut y, 0, 0x5C, 0x1F); // op4 AR max
        w(&mut y, 0, 0x8C, 0x0F);
        w(&mut y, 0, 0xA4, 0x22); // block 4, fnum hi
        w(&mut y, 0, 0xA0, 0x69);
        w(&mut y, 0, 0x28, 0x80); // key on op4, channel 0
        for _ in 0..5000 {
            y.clock_sample();
        }
        let peak = y.samples.iter().cloned().fold(0.0f32, f32::max);
        assert!(peak > 0.1, "peak {peak}");
    }

    #[test]
    fn dac_and_timer() {
        let mut y = Ym2612::new();
        w(&mut y, 0, 0x2B, 0x80);
        w(&mut y, 0, 0x2A, 0xFF);
        w(&mut y, 0, 0x24, 0xFF);
        w(&mut y, 0, 0x25, 0x03); // timer A = 1023: overflow every sample
        w(&mut y, 0, 0x27, 0x05);
        for _ in 0..500 {
            y.clock_sample();
        }
        assert_eq!(y.read_status() & 1, 1);
        assert!(y.samples.iter().any(|&s| s > 0.1), "DAC output");
    }
}
