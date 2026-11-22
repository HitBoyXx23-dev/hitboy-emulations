//! Texas Instruments SN76489 (Sega variant): three square-wave tones and a noise channel,
//! each with 16 attenuation steps of 2 dB. Clocked at the Z80 rate / 16.

#[derive(state::Stateful)]
pub struct Psg {
    period: [u16; 4],
    volume: [u8; 4],
    counter: [u16; 4],
    out: [bool; 4],
    latch: usize,
    lfsr: u16,
    prescale: u32,
    rate: f64,
    phase: f64,
    acc: f32,
    acc_n: u32,
    #[state(skip)]
    pub samples: Vec<f32>,
    #[state(skip)]
    clock: f64,
}

/// Output level for each attenuation value (15 = silent).
fn level(v: u8) -> f32 {
    if v >= 15 {
        0.0
    } else {
        10f32.powf(-(v as f32) * 2.0 / 20.0)
    }
}

impl Psg {
    pub fn new(clock: f64) -> Psg {
        Psg { period: [0; 4], volume: [15; 4], counter: [0; 4], out: [true; 4], latch: 0, lfsr: 0x8000, prescale: 0, rate: 48_000.0, phase: 0.0, acc: 0.0, acc_n: 0, samples: Vec::new(), clock }
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.rate = r as f64;
    }

    pub fn write(&mut self, v: u8) {
        if v & 0x80 != 0 {
            self.latch = ((v >> 4) & 7) as usize;
            let (ch, vol) = (self.latch >> 1, self.latch & 1 != 0);
            if vol {
                self.volume[ch] = v & 0xF;
            } else if ch == 3 {
                self.period[3] = (v & 7) as u16;
                self.lfsr = 0x8000;
            } else {
                self.period[ch] = (self.period[ch] & 0x3F0) | (v & 0xF) as u16;
            }
        } else {
            let (ch, vol) = (self.latch >> 1, self.latch & 1 != 0);
            if vol {
                self.volume[ch] = v & 0xF;
            } else if ch == 3 {
                self.period[3] = (v & 7) as u16;
                self.lfsr = 0x8000;
            } else {
                self.period[ch] = (self.period[ch] & 0xF) | ((v & 0x3F) as u16) << 4;
            }
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        for _ in 0..cycles {
            self.prescale += 1;
            if self.prescale < 16 {
                continue;
            }
            self.prescale = 0;
            for ch in 0..3 {
                if self.counter[ch] == 0 {
                    self.counter[ch] = self.period[ch].max(1);
                    self.out[ch] = !self.out[ch];
                } else {
                    self.counter[ch] -= 1;
                }
            }
            if self.counter[3] == 0 {
                self.counter[3] = match self.period[3] & 3 {
                    0 => 0x10,
                    1 => 0x20,
                    2 => 0x40,
                    _ => self.period[2].max(1),
                };
                self.out[3] = !self.out[3];
                if self.out[3] {
                    let white = self.period[3] & 4 != 0;
                    let fb = if white { (self.lfsr & 1) ^ ((self.lfsr >> 3) & 1) } else { self.lfsr & 1 };
                    self.lfsr = (self.lfsr >> 1) | (fb << 15);
                }
            } else {
                self.counter[3] -= 1;
            }
            let mut s = 0.0;
            for ch in 0..3 {
                // Periods of 0/1 give a constant level (used for sample playback).
                let high = self.out[ch] || self.period[ch] <= 1;
                if high {
                    s += level(self.volume[ch]);
                }
            }
            if self.lfsr & 1 != 0 {
                s += level(self.volume[3]);
            }
            self.acc += s / 4.0;
            self.acc_n += 1;
            self.phase += self.rate;
            let step = self.clock / 16.0;
            while self.phase >= step {
                self.phase -= step;
                let v = if self.acc_n > 0 { self.acc / self.acc_n as f32 } else { 0.0 };
                self.samples.push(v);
                self.samples.push(v);
                self.acc = 0.0;
                self.acc_n = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_by_default_then_tone() {
        let mut p = Psg::new(3_579_545.0);
        p.tick(50_000);
        assert!(p.samples.iter().all(|&s| s == 0.0));
        p.samples.clear();
        p.write(0x80 | 0x0E); // channel 0 period low = 14
        p.write(0x04); // period high -> 0x4E
        p.write(0x90); // channel 0 volume 0 (loudest)
        p.tick(50_000);
        let peak = p.samples.iter().cloned().fold(0.0f32, f32::max);
        let low = p.samples.iter().cloned().fold(1.0f32, f32::min);
        assert!(peak > 0.2 && low < 0.05, "square wave, got {low}..{peak}");
    }
}
