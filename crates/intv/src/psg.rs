//! AY-3-8914 sound chip: three square-wave tone channels, a noise generator and an envelope,
//! at $01F0-$01FD, plus the two hand-controller ports at $01FE/$01FF.

/// Rough logarithmic output levels for volumes 0-15.
const LEVEL: [f32; 16] = [0.0, 0.0078, 0.011, 0.0156, 0.022, 0.031, 0.044, 0.0625, 0.088, 0.125, 0.177, 0.25, 0.354, 0.5, 0.707, 1.0];

pub struct Psg {
    pub regs: [u16; 16],
    count: [u32; 3],
    out: [bool; 3],
    noise_count: u32,
    lfsr: u32,
    env_count: u32,
    env_step: u32,
    env_hold: bool,
    prescale: u32,
    /// Controller lines, active low: [$1FE (right), $1FF (left)].
    pub ports: [u8; 2],
    pub samples: Vec<f32>,
    rate: f64,
    phase: f64,
    acc: f32,
    acc_n: u32,
}

/// CPU clock; the PSG runs from the same clock.
pub const CLOCK: f64 = 894_886.25;

impl Default for Psg {
    fn default() -> Self {
        Self::new()
    }
}

impl Psg {
    pub fn new() -> Psg {
        Psg { regs: [0; 16], count: [0; 3], out: [false; 3], noise_count: 0, lfsr: 1, env_count: 0, env_step: 0, env_hold: false, prescale: 0, ports: [0xFF; 2], samples: Vec::new(), rate: 48_000.0, phase: 0.0, acc: 0.0, acc_n: 0 }
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.rate = r as f64;
    }

    pub fn read(&mut self, a: u16) -> u16 {
        match a & 0xF {
            0xE => self.ports[0] as u16,
            0xF => self.ports[1] as u16,
            r => self.regs[r as usize],
        }
    }

    pub fn write(&mut self, a: u16, v: u16) {
        let r = (a & 0xF) as usize;
        self.regs[r] = v & 0xFF;
        if r == 0xA {
            self.env_step = 0;
            self.env_count = 0;
            self.env_hold = false;
        }
    }

    fn period(&self, ch: usize) -> u32 {
        ((self.regs[ch] | (self.regs[4 + ch] & 0xF) << 8) as u32).max(1)
    }

    fn envelope_level(&self) -> u32 {
        let shape = self.regs[0xA];
        let (cont, attack, alt, hold) = (shape & 8 != 0, shape & 4 != 0, shape & 2 != 0, shape & 1 != 0);
        let cycle = self.env_step / 16;
        let pos = self.env_step % 16;
        let rising = attack != (alt && cycle % 2 == 1);
        if cycle >= 1 && (!cont || hold) {
            // Held after the first ramp.
            let end_high = if !cont { false } else { attack != alt };
            return if end_high { 15 } else { 0 };
        }
        if rising {
            pos
        } else {
            15 - pos
        }
    }

    /// Advances by CPU cycles, producing host-rate samples.
    pub fn tick(&mut self, cycles: u32) {
        for _ in 0..cycles {
            self.prescale += 1;
            if self.prescale < 8 {
                continue;
            }
            self.prescale = 0;
            for ch in 0..3 {
                self.count[ch] += 1;
                if self.count[ch] >= self.period(ch) {
                    self.count[ch] = 0;
                    self.out[ch] = !self.out[ch];
                }
            }
            self.noise_count += 1;
            if self.noise_count >= ((self.regs[9] & 0x1F) as u32).max(1) * 2 {
                self.noise_count = 0;
                let bit = (self.lfsr ^ (self.lfsr >> 3)) & 1;
                self.lfsr = (self.lfsr >> 1) | (bit << 16);
            }
            self.env_count += 1;
            let ep = ((self.regs[3] | self.regs[7] << 8) as u32).max(1) * 2;
            if self.env_count >= ep {
                self.env_count = 0;
                self.env_step = (self.env_step + 1).min(10_000);
            }
            let enable = self.regs[8];
            let noise = self.lfsr & 1 != 0;
            let env = self.envelope_level();
            let mut s = 0.0;
            for ch in 0..3 {
                let tone_on = enable & (1 << ch) == 0;
                let noise_on = enable & (8 << ch) == 0;
                let high = (!tone_on || self.out[ch]) && (!noise_on || noise);
                if high {
                    let v = self.regs[0xB + ch];
                    let vol = if v & 0x30 != 0 { env } else { (v & 0xF) as u32 };
                    s += LEVEL[vol as usize];
                }
            }
            self.acc += s / 3.0;
            self.acc_n += 1;
            self.phase += self.rate;
            let step = CLOCK / 8.0;
            while self.phase >= step {
                self.phase -= step;
                let v = if self.acc_n > 0 { self.acc / self.acc_n as f32 } else { 0.0 } * 0.8;
                self.samples.push(v);
                self.samples.push(v);
                self.acc = 0.0;
                self.acc_n = 0;
            }
        }
    }
}

/// Hand-controller codes (a pressed input pulls these bits low).
pub fn controller_bits(buttons: u16) -> u8 {
    let (up, down, left, right) = (buttons & 0x10 != 0, buttons & 0x20 != 0, buttons & 0x40 != 0, buttons & 0x80 != 0);
    let mut code: u8 = match (up, down, left, right) {
        (true, _, false, true) => 0x16,
        (true, _, true, false) => 0x1C,
        (_, true, false, true) => 0x13,
        (_, true, true, false) => 0x19,
        (true, _, _, _) => 0x04,
        (_, true, _, _) => 0x01,
        (_, _, true, _) => 0x08,
        (_, _, _, true) => 0x02,
        _ => 0,
    };
    // Side buttons: A = top, B = lower left, X = lower right.
    if buttons & 1 != 0 {
        code |= 0xA0;
    }
    if buttons & 2 != 0 {
        code |= 0x60;
    }
    if buttons & 0x100 != 0 {
        code |= 0xC0;
    }
    // Keypad: Start = Enter, Select = Clear, Y/L/R = 1/2/3.
    if buttons & 8 != 0 {
        code |= 0x28;
    }
    if buttons & 4 != 0 {
        code |= 0x88;
    }
    if buttons & 0x200 != 0 {
        code |= 0x81;
    }
    if buttons & 0x400 != 0 {
        code |= 0x41;
    }
    if buttons & 0x800 != 0 {
        code |= 0x21;
    }
    !code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_produces_sound_and_silence_is_zero() {
        let mut p = Psg::new();
        p.tick(10_000);
        assert!(p.samples.iter().all(|&s| s == 0.0));
        p.samples.clear();
        p.write(0x1F0, 100); // channel A period
        p.write(0x1F8, 0x3E); // tone A on only
        p.write(0x1FB, 15);
        p.tick(20_000);
        let peak = p.samples.iter().cloned().fold(0.0f32, f32::max);
        assert!(peak > 0.2, "peak {peak}");
    }

    #[test]
    fn controller_codes() {
        assert_eq!(controller_bits(0), 0xFF);
        assert_eq!(!controller_bits(0x10), 0x04, "north");
        assert_eq!(!controller_bits(0x10 | 0x80), 0x16, "north-east");
        assert_eq!(!controller_bits(8), 0x28, "enter");
    }
}
