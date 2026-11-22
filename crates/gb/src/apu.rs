//! DMG APU: two square channels (one with sweep), wave and noise, stereo-panned.

const CPU_HZ: f64 = 4_194_304.0;
const DUTY: [[u8; 8]; 4] = [[0, 0, 0, 0, 0, 0, 0, 1], [1, 0, 0, 0, 0, 0, 0, 1], [1, 0, 0, 0, 0, 1, 1, 1], [0, 1, 1, 1, 1, 1, 1, 0]];
const NOISE_DIV: [u32; 8] = [8, 16, 32, 48, 64, 80, 96, 112];

#[derive(Default, Clone, Copy, state::Stateful)]
struct Envelope {
    initial: u8,
    up: bool,
    period: u8,
    volume: u8,
    timer: u8,
}

impl Envelope {
    fn set(&mut self, v: u8) {
        self.initial = v >> 4;
        self.up = v & 8 != 0;
        self.period = v & 7;
    }
    fn trigger(&mut self) {
        self.volume = self.initial;
        self.timer = self.period;
    }
    fn clock(&mut self) {
        if self.period == 0 {
            return;
        }
        if self.timer > 0 {
            self.timer -= 1;
        }
        if self.timer == 0 {
            self.timer = self.period;
            if self.up && self.volume < 15 {
                self.volume += 1;
            } else if !self.up && self.volume > 0 {
                self.volume -= 1;
            }
        }
    }
    fn dac_on(&self) -> bool {
        self.initial != 0 || self.up
    }
}

#[derive(Default, state::Stateful)]
struct Square {
    on: bool,
    duty: u8,
    step: u8,
    freq: u16,
    timer: u32,
    length: u16,
    length_enable: bool,
    env: Envelope,
    // sweep (channel 1 only)
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    sweep_timer: u8,
    sweep_enabled: bool,
    shadow: u16,
}

impl Square {
    fn period(&self) -> u32 {
        (2048 - self.freq as u32) * 4
    }
    fn tick(&mut self, cycles: u32) {
        let mut c = cycles;
        while c > 0 {
            if self.timer > c {
                self.timer -= c;
                break;
            }
            c -= self.timer;
            self.timer = self.period().max(4);
            self.step = (self.step + 1) & 7;
        }
    }
    fn trigger(&mut self) {
        self.on = self.env.dac_on();
        if self.length == 0 {
            self.length = 64;
        }
        self.timer = self.period();
        self.env.trigger();
        self.shadow = self.freq;
        self.sweep_timer = if self.sweep_period == 0 { 8 } else { self.sweep_period };
        self.sweep_enabled = self.sweep_period > 0 || self.sweep_shift > 0;
        if self.sweep_shift > 0 && self.sweep_calc() > 2047 {
            self.on = false;
        }
    }
    fn sweep_calc(&self) -> u16 {
        let d = self.shadow >> self.sweep_shift;
        if self.sweep_negate { self.shadow.wrapping_sub(d) } else { self.shadow + d }
    }
    fn clock_sweep(&mut self) {
        if self.sweep_timer > 0 {
            self.sweep_timer -= 1;
        }
        if self.sweep_timer == 0 {
            self.sweep_timer = if self.sweep_period == 0 { 8 } else { self.sweep_period };
            if self.sweep_enabled && self.sweep_period > 0 {
                let f = self.sweep_calc();
                if f > 2047 {
                    self.on = false;
                } else if self.sweep_shift > 0 {
                    self.shadow = f;
                    self.freq = f;
                    if self.sweep_calc() > 2047 {
                        self.on = false;
                    }
                }
            }
        }
    }
    fn clock_length(&mut self) {
        if self.length_enable && self.length > 0 {
            self.length -= 1;
            if self.length == 0 {
                self.on = false;
            }
        }
    }
    fn output(&self) -> f32 {
        if !self.on || !self.env.dac_on() {
            return 0.0;
        }
        let bit = DUTY[self.duty as usize][self.step as usize];
        (if bit == 1 { self.env.volume as f32 } else { 0.0 }) / 15.0
    }
}

#[derive(Default, state::Stateful)]
struct Wave {
    on: bool,
    dac: bool,
    freq: u16,
    timer: u32,
    pos: u8,
    length: u16,
    length_enable: bool,
    shift: u8,
    ram: [u8; 16],
}

impl Wave {
    fn period(&self) -> u32 {
        (2048 - self.freq as u32) * 2
    }
    fn tick(&mut self, cycles: u32) {
        let mut c = cycles;
        while c > 0 {
            if self.timer > c {
                self.timer -= c;
                break;
            }
            c -= self.timer;
            self.timer = self.period().max(2);
            self.pos = (self.pos + 1) & 31;
        }
    }
    fn output(&self) -> f32 {
        if !self.on || !self.dac || self.shift == 0 {
            return 0.0;
        }
        let byte = self.ram[(self.pos / 2) as usize];
        let sample = if self.pos & 1 == 0 { byte >> 4 } else { byte & 0x0F };
        (sample >> (self.shift - 1)) as f32 / 15.0
    }
}

#[derive(state::Stateful)]
struct Noise {
    on: bool,
    lfsr: u16,
    width7: bool,
    div: u8,
    shift: u8,
    timer: u32,
    length: u16,
    length_enable: bool,
    env: Envelope,
}

impl Default for Noise {
    fn default() -> Self {
        Self { on: false, lfsr: 0x7FFF, width7: false, div: 0, shift: 0, timer: 8, length: 0, length_enable: false, env: Envelope::default() }
    }
}

impl Noise {
    fn period(&self) -> u32 {
        NOISE_DIV[self.div as usize] << self.shift
    }
    fn tick(&mut self, cycles: u32) {
        let mut c = cycles;
        while c > 0 {
            if self.timer > c {
                self.timer -= c;
                break;
            }
            c -= self.timer;
            self.timer = self.period().max(8);
            let x = (self.lfsr & 1) ^ ((self.lfsr >> 1) & 1);
            self.lfsr = (self.lfsr >> 1) | (x << 14);
            if self.width7 {
                self.lfsr = (self.lfsr & !(1 << 6)) | (x << 6);
            }
        }
    }
    fn output(&self) -> f32 {
        if !self.on || !self.env.dac_on() || self.lfsr & 1 != 0 {
            0.0
        } else {
            self.env.volume as f32 / 15.0
        }
    }
}

#[derive(state::Stateful)]
pub struct Apu {
    power: bool,
    ch1: Square,
    ch2: Square,
    ch3: Wave,
    ch4: Noise,
    nr50: u8,
    nr51: u8,
    regs: [u8; 0x30],
    seq_timer: u32,
    seq_step: u8,
    #[state(skip)]
    cycles_per_sample: f64,
    sample_clock: f64,
    acc: (f32, f32),
    acc_n: u32,
    hp: [(f32, f32); 2],
    #[state(skip)]
    pub samples: Vec<f32>,
}

impl Default for Apu {
    fn default() -> Self {
        let mut a = Self {
            power: true,
            ch1: Square::default(),
            ch2: Square::default(),
            ch3: Wave::default(),
            ch4: Noise::default(),
            nr50: 0x77,
            nr51: 0xF3,
            regs: [0; 0x30],
            seq_timer: 8192,
            seq_step: 0,
            cycles_per_sample: 0.0,
            sample_clock: 0.0,
            acc: (0.0, 0.0),
            acc_n: 0,
            hp: [(0.0, 0.0); 2],
            samples: Vec::with_capacity(2048),
        };
        a.set_sample_rate(48_000);
        a
    }
}

impl Apu {
    pub fn set_sample_rate(&mut self, rate: u32) {
        self.cycles_per_sample = CPU_HZ / rate as f64;
    }

    pub fn read(&self, addr: u16) -> u8 {
        const MASK: [u8; 0x17] = [
            0x80, 0x3F, 0x00, 0xFF, 0xBF, 0xFF, 0x3F, 0x00, 0xFF, 0xBF, 0x7F, 0xFF, 0x9F, 0xFF, 0xBF, 0xFF, 0xFF, 0x00, 0x00, 0xBF, 0x00, 0x00, 0x70,
        ];
        match addr {
            0xFF26 => {
                0x70 | (self.power as u8) << 7
                    | self.ch1.on as u8
                    | (self.ch2.on as u8) << 1
                    | (self.ch3.on as u8) << 2
                    | (self.ch4.on as u8) << 3
            }
            0xFF30..=0xFF3F => self.ch3.ram[(addr - 0xFF30) as usize],
            0xFF10..=0xFF26 => self.regs[(addr - 0xFF10) as usize] | MASK[(addr - 0xFF10) as usize],
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, v: u8) {
        if (0xFF30..=0xFF3F).contains(&addr) {
            self.ch3.ram[(addr - 0xFF30) as usize] = v;
            return;
        }
        if addr == 0xFF26 {
            let on = v & 0x80 != 0;
            if !on {
                let ram = self.ch3.ram;
                *self = Self { power: false, cycles_per_sample: self.cycles_per_sample, samples: std::mem::take(&mut self.samples), ..Self::default() };
                self.ch3.ram = ram;
                self.nr50 = 0;
                self.nr51 = 0;
            }
            self.power = on;
            return;
        }
        if !self.power || !(0xFF10..=0xFF25).contains(&addr) {
            return;
        }
        self.regs[(addr - 0xFF10) as usize] = v;
        match addr {
            0xFF10 => {
                self.ch1.sweep_period = (v >> 4) & 7;
                self.ch1.sweep_negate = v & 8 != 0;
                self.ch1.sweep_shift = v & 7;
            }
            0xFF11 => {
                self.ch1.duty = v >> 6;
                self.ch1.length = 64 - (v & 0x3F) as u16;
            }
            0xFF12 => {
                self.ch1.env.set(v);
                if !self.ch1.env.dac_on() {
                    self.ch1.on = false;
                }
            }
            0xFF13 => self.ch1.freq = (self.ch1.freq & 0x700) | v as u16,
            0xFF14 => {
                self.ch1.freq = (self.ch1.freq & 0xFF) | ((v as u16 & 7) << 8);
                self.ch1.length_enable = v & 0x40 != 0;
                if v & 0x80 != 0 {
                    self.ch1.trigger();
                }
            }
            0xFF16 => {
                self.ch2.duty = v >> 6;
                self.ch2.length = 64 - (v & 0x3F) as u16;
            }
            0xFF17 => {
                self.ch2.env.set(v);
                if !self.ch2.env.dac_on() {
                    self.ch2.on = false;
                }
            }
            0xFF18 => self.ch2.freq = (self.ch2.freq & 0x700) | v as u16,
            0xFF19 => {
                self.ch2.freq = (self.ch2.freq & 0xFF) | ((v as u16 & 7) << 8);
                self.ch2.length_enable = v & 0x40 != 0;
                if v & 0x80 != 0 {
                    self.ch2.trigger();
                }
            }
            0xFF1A => {
                self.ch3.dac = v & 0x80 != 0;
                if !self.ch3.dac {
                    self.ch3.on = false;
                }
            }
            0xFF1B => self.ch3.length = 256 - v as u16,
            0xFF1C => self.ch3.shift = match (v >> 5) & 3 {
                0 => 0,
                1 => 1,
                2 => 2,
                _ => 3,
            },
            0xFF1D => self.ch3.freq = (self.ch3.freq & 0x700) | v as u16,
            0xFF1E => {
                self.ch3.freq = (self.ch3.freq & 0xFF) | ((v as u16 & 7) << 8);
                self.ch3.length_enable = v & 0x40 != 0;
                if v & 0x80 != 0 {
                    self.ch3.on = self.ch3.dac;
                    if self.ch3.length == 0 {
                        self.ch3.length = 256;
                    }
                    self.ch3.timer = self.ch3.period();
                    self.ch3.pos = 0;
                }
            }
            0xFF20 => self.ch4.length = 64 - (v & 0x3F) as u16,
            0xFF21 => {
                self.ch4.env.set(v);
                if !self.ch4.env.dac_on() {
                    self.ch4.on = false;
                }
            }
            0xFF22 => {
                self.ch4.shift = v >> 4;
                self.ch4.width7 = v & 8 != 0;
                self.ch4.div = v & 7;
            }
            0xFF23 => {
                self.ch4.length_enable = v & 0x40 != 0;
                if v & 0x80 != 0 {
                    self.ch4.on = self.ch4.env.dac_on();
                    if self.ch4.length == 0 {
                        self.ch4.length = 64;
                    }
                    self.ch4.env.trigger();
                    self.ch4.lfsr = 0x7FFF;
                    self.ch4.timer = self.ch4.period();
                }
            }
            0xFF24 => self.nr50 = v,
            0xFF25 => self.nr51 = v,
            _ => {}
        }
    }

    fn sequencer_step(&mut self) {
        let s = self.seq_step;
        if s % 2 == 0 {
            self.ch1.clock_length();
            self.ch2.clock_length();
            if self.ch3.length_enable && self.ch3.length > 0 {
                self.ch3.length -= 1;
                if self.ch3.length == 0 {
                    self.ch3.on = false;
                }
            }
            if self.ch4.length_enable && self.ch4.length > 0 {
                self.ch4.length -= 1;
                if self.ch4.length == 0 {
                    self.ch4.on = false;
                }
            }
        }
        if s == 2 || s == 6 {
            self.ch1.clock_sweep();
        }
        if s == 7 {
            self.ch1.env.clock();
            self.ch2.env.clock();
            self.ch4.env.clock();
        }
        self.seq_step = (s + 1) & 7;
    }

    /// Advances by `cycles` T-cycles, producing stereo samples.
    pub fn tick(&mut self, cycles: u32) {
        if !self.power {
            // Keep time moving so audio output doesn't stall while the APU is off.
            self.emit(cycles, (0.0, 0.0));
            return;
        }
        // Advance in small slices so the mix is sampled often enough for clean resampling.
        let mut left = cycles;
        while left > 0 {
            let slice = left.min(4);
            left -= slice;
            if self.seq_timer <= slice {
                self.seq_timer += 8192 - slice;
                self.sequencer_step();
            } else {
                self.seq_timer -= slice;
            }
            self.ch1.tick(slice);
            self.ch2.tick(slice);
            self.ch3.tick(slice);
            self.ch4.tick(slice);
            let outs = [self.ch1.output(), self.ch2.output(), self.ch3.output(), self.ch4.output()];
            let (mut l, mut r) = (0.0, 0.0);
            for (i, o) in outs.iter().enumerate() {
                if self.nr51 & (0x10 << i) != 0 {
                    l += o;
                }
                if self.nr51 & (1 << i) != 0 {
                    r += o;
                }
            }
            let lv = (((self.nr50 >> 4) & 7) + 1) as f32 / 8.0;
            let rv = ((self.nr50 & 7) + 1) as f32 / 8.0;
            self.emit(slice, (l * lv / 4.0, r * rv / 4.0));
        }
    }

    fn emit(&mut self, cycles: u32, (l, r): (f32, f32)) {
        self.acc.0 += l * cycles as f32;
        self.acc.1 += r * cycles as f32;
        self.acc_n += cycles;
        self.sample_clock += cycles as f64;
        while self.sample_clock >= self.cycles_per_sample {
            self.sample_clock -= self.cycles_per_sample;
            let n = self.acc_n.max(1) as f32;
            let raw = [self.acc.0 / n, self.acc.1 / n];
            self.acc = (0.0, 0.0);
            self.acc_n = 0;
            for (ch, x) in raw.iter().enumerate() {
                let (pi, po) = self.hp[ch];
                let y = x - pi + 0.996 * po;
                self.hp[ch] = (*x, y);
                self.samples.push((y * 1.4).clamp(-1.0, 1.0));
            }
        }
    }
}
