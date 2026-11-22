//! HuC6280 PSG: six wavetable channels (32 five-bit samples each) with 12-bit frequency,
//! volume and left/right balance; channels 5 and 6 can play noise.

#[derive(state::Stateful)]
pub struct Psg {
    sel: usize,
    main_vol: u8,
    freq: [u16; 6],
    ctrl: [u8; 6],
    balance: [u8; 6],
    wave: [[u8; 32]; 6],
    wave_pos: [usize; 6],
    counter: [u32; 6],
    noise: [u8; 6],
    lfsr: [u32; 6],
    #[state(skip)]
    pub samples: Vec<f32>,
    #[state(skip)]
    rate: f64,
    phase: f64,
    acc: (f32, f32),
    acc_n: u32,
    sub: u32,
}

/// Master clock / 2 drives the channel counters.
const CLOCK: f64 = 3_579_545.0;

impl Default for Psg {
    fn default() -> Self {
        Self::new()
    }
}

impl Psg {
    pub fn new() -> Psg {
        Psg {
            sel: 0,
            main_vol: 0,
            freq: [0; 6],
            ctrl: [0; 6],
            balance: [0xFF; 6],
            wave: [[0; 32]; 6],
            wave_pos: [0; 6],
            counter: [0; 6],
            noise: [0; 6],
            lfsr: [1; 6],
            samples: Vec::new(),
            rate: 48_000.0,
            phase: 0.0,
            acc: (0.0, 0.0),
            acc_n: 0,
            sub: 0,
        }
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.rate = r as f64;
    }

    pub fn write(&mut self, port: u32, v: u8) {
        let ch = self.sel;
        match port & 0xF {
            0 => self.sel = (v & 7).min(5) as usize,
            1 => self.main_vol = v,
            2 => self.freq[ch] = (self.freq[ch] & 0xF00) | v as u16,
            3 => self.freq[ch] = (self.freq[ch] & 0xFF) | ((v as u16 & 0xF) << 8),
            4 => {
                // Clearing "write enable + DDA" resets the waveform index.
                if self.ctrl[ch] & 0x40 != 0 && v & 0x40 == 0 {
                    self.wave_pos[ch] = 0;
                }
                self.ctrl[ch] = v;
            }
            5 => self.balance[ch] = v,
            6 => {
                if self.ctrl[ch] & 0x80 == 0 {
                    let pos = self.wave_pos[ch];
                    self.wave[ch][pos] = v & 0x1F;
                    self.wave_pos[ch] = (pos + 1) & 31;
                } else if self.ctrl[ch] & 0x40 != 0 {
                    // DDA: direct sample output
                    self.wave[ch][0] = v & 0x1F;
                }
            }
            7 => {
                if ch >= 4 {
                    self.noise[ch] = v;
                }
            }
            _ => {}
        }
    }

    fn channel_level(&self, ch: usize) -> f32 {
        let c = self.ctrl[ch];
        if c & 0x80 == 0 {
            return 0.0;
        }
        let v = if c & 0x40 != 0 {
            self.wave[ch][0]
        } else if ch >= 4 && self.noise[ch] & 0x80 != 0 {
            if self.lfsr[ch] & 1 != 0 {
                0x1F
            } else {
                0
            }
        } else {
            self.wave[ch][self.wave_pos[ch]]
        };
        let vol = (c & 0x1F) as f32 / 31.0;
        (v as f32 / 31.0 - 0.5) * vol
    }

    /// Advances by CPU cycles at 7.16 MHz.
    pub fn tick(&mut self, cycles: u32) {
        for _ in 0..cycles {
            self.sub += 1;
            if self.sub < 2 {
                continue;
            }
            self.sub = 0;
            for ch in 0..6 {
                if self.ctrl[ch] & 0x80 == 0 || self.ctrl[ch] & 0x40 != 0 {
                    continue;
                }
                self.counter[ch] += 1;
                if ch >= 4 && self.noise[ch] & 0x80 != 0 {
                    let period = ((!self.noise[ch] & 0x1F) as u32 + 1) * 64;
                    if self.counter[ch] >= period {
                        self.counter[ch] = 0;
                        let l = self.lfsr[ch];
                        let bit = (l ^ (l >> 1) ^ (l >> 11) ^ (l >> 12) ^ (l >> 17)) & 1;
                        self.lfsr[ch] = (l >> 1) | (bit << 17);
                    }
                } else {
                    let period = (self.freq[ch] as u32).max(1);
                    if self.counter[ch] >= period {
                        self.counter[ch] = 0;
                        self.wave_pos[ch] = (self.wave_pos[ch] + 1) & 31;
                    }
                }
            }
            let (ml, mr) = ((self.main_vol >> 4) as f32 / 15.0, (self.main_vol & 0xF) as f32 / 15.0);
            let (mut l, mut r) = (0.0, 0.0);
            for ch in 0..6 {
                let s = self.channel_level(ch);
                l += s * (self.balance[ch] >> 4) as f32 / 15.0;
                r += s * (self.balance[ch] & 0xF) as f32 / 15.0;
            }
            self.acc.0 += l * ml / 3.0;
            self.acc.1 += r * mr / 3.0;
            self.acc_n += 1;
            self.phase += self.rate;
            while self.phase >= CLOCK {
                self.phase -= CLOCK;
                let n = self.acc_n.max(1) as f32;
                self.samples.push(self.acc.0 / n);
                self.samples.push(self.acc.1 / n);
                self.acc = (0.0, 0.0);
                self.acc_n = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wavetable_tone() {
        let mut p = Psg::new();
        p.tick(10_000);
        assert!(p.samples.iter().all(|&s| s == 0.0));
        p.samples.clear();
        p.write(1, 0xFF);
        p.write(0, 0);
        p.write(4, 0x00); // write mode
        for i in 0..32 {
            p.write(6, if i < 16 { 0x1F } else { 0 });
        }
        p.write(2, 0x40);
        p.write(5, 0xFF);
        p.write(4, 0x9F); // on, full volume
        p.tick(100_000);
        let peak = p.samples.iter().cloned().fold(0.0f32, f32::max);
        let low = p.samples.iter().cloned().fold(0.0f32, f32::min);
        assert!(peak > 0.1 && low < -0.1, "square-ish wave {low}..{peak}");
    }
}
