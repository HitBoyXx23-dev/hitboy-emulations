//! 2A03 APU: two pulse channels, triangle, noise and DMC, mixed with the
//! hardware's non-linear mixer and resampled to the host rate.

const CPU_HZ: f64 = 1_789_773.0;

const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, 12, 16, 24, 18, 48, 20, 96, 22, 192, 24, 72, 26, 16, 28, 32, 30,
];
const DUTY: [[u8; 8]; 4] = [[0, 1, 0, 0, 0, 0, 0, 0], [0, 1, 1, 0, 0, 0, 0, 0], [0, 1, 1, 1, 1, 0, 0, 0], [1, 0, 0, 1, 1, 1, 1, 1]];
const TRIANGLE: [u8; 32] = [15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
const NOISE_PERIOD: [u16; 16] = [4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068];
const DMC_RATE: [u16; 16] = [428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54];

#[derive(Default, state::Stateful)]
struct Envelope {
    start: bool,
    loop_: bool,
    constant: bool,
    volume: u8,
    divider: u8,
    decay: u8,
}

impl Envelope {
    fn clock(&mut self) {
        if self.start {
            self.start = false;
            self.decay = 15;
            self.divider = self.volume;
        } else if self.divider == 0 {
            self.divider = self.volume;
            if self.decay > 0 {
                self.decay -= 1;
            } else if self.loop_ {
                self.decay = 15;
            }
        } else {
            self.divider -= 1;
        }
    }
    fn output(&self) -> u8 {
        if self.constant { self.volume } else { self.decay }
    }
}

#[derive(Default, state::Stateful)]
struct Pulse {
    second: bool,
    enabled: bool,
    duty: u8,
    step: u8,
    timer: u16,
    period: u16,
    length: u8,
    halt: bool,
    env: Envelope,
    sweep_enabled: bool,
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    sweep_divider: u8,
    sweep_reload: bool,
}

impl Pulse {
    fn target(&self) -> u16 {
        let delta = self.period >> self.sweep_shift;
        if self.sweep_negate {
            self.period.saturating_sub(delta + if self.second { 0 } else { 1 })
        } else {
            self.period + delta
        }
    }
    fn muted(&self) -> bool {
        self.period < 8 || self.target() > 0x7FF
    }
    fn clock_timer(&mut self) {
        if self.timer == 0 {
            self.timer = self.period;
            self.step = (self.step + 1) & 7;
        } else {
            self.timer -= 1;
        }
    }
    fn clock_sweep(&mut self) {
        if self.sweep_divider == 0 && self.sweep_enabled && self.sweep_shift > 0 && !self.muted() {
            self.period = self.target();
        }
        if self.sweep_divider == 0 || self.sweep_reload {
            self.sweep_divider = self.sweep_period;
            self.sweep_reload = false;
        } else {
            self.sweep_divider -= 1;
        }
    }
    fn clock_length(&mut self) {
        if !self.halt && self.length > 0 {
            self.length -= 1;
        }
    }
    fn output(&self) -> u8 {
        if !self.enabled || self.length == 0 || self.muted() || DUTY[self.duty as usize][self.step as usize] == 0 {
            0
        } else {
            self.env.output()
        }
    }
    fn write(&mut self, reg: u16, v: u8) {
        match reg & 3 {
            0 => {
                self.duty = v >> 6;
                self.halt = v & 0x20 != 0;
                self.env.loop_ = self.halt;
                self.env.constant = v & 0x10 != 0;
                self.env.volume = v & 0x0F;
            }
            1 => {
                self.sweep_enabled = v & 0x80 != 0;
                self.sweep_period = (v >> 4) & 7;
                self.sweep_negate = v & 0x08 != 0;
                self.sweep_shift = v & 7;
                self.sweep_reload = true;
            }
            2 => self.period = (self.period & 0x700) | v as u16,
            _ => {
                self.period = (self.period & 0xFF) | ((v as u16 & 7) << 8);
                if self.enabled {
                    self.length = LENGTH_TABLE[(v >> 3) as usize];
                }
                self.step = 0;
                self.env.start = true;
            }
        }
    }
}

#[derive(Default, state::Stateful)]
struct Triangle {
    enabled: bool,
    control: bool,
    linear_reload_value: u8,
    linear: u8,
    linear_reload: bool,
    length: u8,
    timer: u16,
    period: u16,
    step: u8,
}

impl Triangle {
    fn clock_timer(&mut self) {
        if self.timer == 0 {
            self.timer = self.period;
            if self.length > 0 && self.linear > 0 && self.period >= 2 {
                self.step = (self.step + 1) & 31;
            }
        } else {
            self.timer -= 1;
        }
    }
    fn clock_linear(&mut self) {
        if self.linear_reload {
            self.linear = self.linear_reload_value;
        } else if self.linear > 0 {
            self.linear -= 1;
        }
        if !self.control {
            self.linear_reload = false;
        }
    }
    fn clock_length(&mut self) {
        if !self.control && self.length > 0 {
            self.length -= 1;
        }
    }
    fn output(&self) -> u8 {
        if self.enabled { TRIANGLE[self.step as usize] } else { 0 }
    }
}

#[derive(state::Stateful)]
struct Noise {
    enabled: bool,
    mode: bool,
    shift: u16,
    timer: u16,
    period: u16,
    length: u8,
    halt: bool,
    env: Envelope,
}

impl Default for Noise {
    fn default() -> Self {
        Self { enabled: false, mode: false, shift: 1, timer: 0, period: 4, length: 0, halt: false, env: Envelope::default() }
    }
}

impl Noise {
    fn clock_timer(&mut self) {
        if self.timer == 0 {
            self.timer = self.period;
            let bit = if self.mode { 6 } else { 1 };
            let fb = (self.shift & 1) ^ ((self.shift >> bit) & 1);
            self.shift = (self.shift >> 1) | (fb << 14);
        } else {
            self.timer -= 1;
        }
    }
    fn output(&self) -> u8 {
        if !self.enabled || self.length == 0 || self.shift & 1 != 0 { 0 } else { self.env.output() }
    }
}

#[derive(Default, state::Stateful)]
struct Dmc {
    enabled: bool,
    loop_: bool,
    rate: u16,
    timer: u16,
    output: u8,
    sample_addr: u16,
    sample_len: u16,
    addr: u16,
    remaining: u16,
    buffer: Option<u8>,
    shift: u8,
    bits: u8,
    silent: bool,
}

#[derive(state::Stateful)]
pub struct Apu {
    pulse1: Pulse,
    pulse2: Pulse,
    triangle: Triangle,
    noise: Noise,
    dmc: Dmc,
    frame_mode5: bool,
    irq_inhibit: bool,
    pub frame_irq: bool,
    frame_cycle: u32,
    odd: bool,
    // resampling
    #[state(skip)]
    sample_rate: f64,
    #[state(skip)]
    cycles_per_sample: f64,
    sample_clock: f64,
    acc: f32,
    acc_n: u32,
    hp_prev_in: f32,
    hp_prev_out: f32,
    #[state(skip)]
    pub samples: Vec<f32>,
    /// Cartridge/disk-system sound mixed in with the APU (0 = silent).
    pub expansion: f32,
}

impl Default for Apu {
    fn default() -> Self {
        let mut a = Self {
            pulse1: Pulse::default(),
            pulse2: Pulse { second: true, ..Pulse::default() },
            triangle: Triangle::default(),
            noise: Noise::default(),
            dmc: Dmc { rate: DMC_RATE[0], ..Dmc::default() },
            frame_mode5: false,
            irq_inhibit: true,
            frame_irq: false,
            frame_cycle: 0,
            odd: false,
            sample_rate: 48_000.0,
            cycles_per_sample: 0.0,
            sample_clock: 0.0,
            acc: 0.0,
            acc_n: 0,
            hp_prev_in: 0.0,
            hp_prev_out: 0.0,
            samples: Vec::with_capacity(2048),
            expansion: 0.0,
        };
        a.set_sample_rate(48_000);
        a
    }
}

impl Apu {
    pub fn set_sample_rate(&mut self, rate: u32) {
        self.sample_rate = rate as f64;
        self.cycles_per_sample = CPU_HZ / self.sample_rate;
    }

    pub fn write(&mut self, addr: u16, v: u8) {
        match addr {
            0x4000..=0x4003 => self.pulse1.write(addr, v),
            0x4004..=0x4007 => self.pulse2.write(addr, v),
            0x4008 => {
                self.triangle.control = v & 0x80 != 0;
                self.triangle.linear_reload_value = v & 0x7F;
            }
            0x400A => self.triangle.period = (self.triangle.period & 0x700) | v as u16,
            0x400B => {
                self.triangle.period = (self.triangle.period & 0xFF) | ((v as u16 & 7) << 8);
                if self.triangle.enabled {
                    self.triangle.length = LENGTH_TABLE[(v >> 3) as usize];
                }
                self.triangle.linear_reload = true;
            }
            0x400C => {
                self.noise.halt = v & 0x20 != 0;
                self.noise.env.loop_ = self.noise.halt;
                self.noise.env.constant = v & 0x10 != 0;
                self.noise.env.volume = v & 0x0F;
            }
            0x400E => {
                self.noise.mode = v & 0x80 != 0;
                self.noise.period = NOISE_PERIOD[(v & 0x0F) as usize];
            }
            0x400F => {
                if self.noise.enabled {
                    self.noise.length = LENGTH_TABLE[(v >> 3) as usize];
                }
                self.noise.env.start = true;
            }
            0x4010 => {
                self.dmc.loop_ = v & 0x40 != 0;
                self.dmc.rate = DMC_RATE[(v & 0x0F) as usize];
            }
            0x4011 => self.dmc.output = v & 0x7F,
            0x4012 => self.dmc.sample_addr = 0xC000 | ((v as u16) << 6),
            0x4013 => self.dmc.sample_len = ((v as u16) << 4) | 1,
            0x4015 => {
                self.pulse1.enabled = v & 1 != 0;
                self.pulse2.enabled = v & 2 != 0;
                self.triangle.enabled = v & 4 != 0;
                self.noise.enabled = v & 8 != 0;
                self.dmc.enabled = v & 16 != 0;
                if !self.pulse1.enabled {
                    self.pulse1.length = 0;
                }
                if !self.pulse2.enabled {
                    self.pulse2.length = 0;
                }
                if !self.triangle.enabled {
                    self.triangle.length = 0;
                }
                if !self.noise.enabled {
                    self.noise.length = 0;
                }
                if !self.dmc.enabled {
                    self.dmc.remaining = 0;
                } else if self.dmc.remaining == 0 {
                    self.dmc.addr = self.dmc.sample_addr;
                    self.dmc.remaining = self.dmc.sample_len;
                }
            }
            0x4017 => {
                self.frame_mode5 = v & 0x80 != 0;
                self.irq_inhibit = v & 0x40 != 0;
                if self.irq_inhibit {
                    self.frame_irq = false;
                }
                self.frame_cycle = 0;
                if self.frame_mode5 {
                    self.quarter_frame();
                    self.half_frame();
                }
            }
            _ => {}
        }
    }

    pub fn read_status(&mut self) -> u8 {
        let v = (self.pulse1.length > 0) as u8
            | ((self.pulse2.length > 0) as u8) << 1
            | ((self.triangle.length > 0) as u8) << 2
            | ((self.noise.length > 0) as u8) << 3
            | ((self.dmc.remaining > 0) as u8) << 4
            | (self.frame_irq as u8) << 6;
        self.frame_irq = false;
        v
    }

    fn quarter_frame(&mut self) {
        self.pulse1.env.clock();
        self.pulse2.env.clock();
        self.noise.env.clock();
        self.triangle.clock_linear();
    }

    fn half_frame(&mut self) {
        self.pulse1.clock_length();
        self.pulse2.clock_length();
        self.triangle.clock_length();
        if !self.noise.halt && self.noise.length > 0 {
            self.noise.length -= 1;
        }
        self.pulse1.clock_sweep();
        self.pulse2.clock_sweep();
    }

    /// Advances by `cycles` CPU cycles. `read` fetches DMC sample bytes from CPU memory.
    pub fn tick(&mut self, cycles: u32, read: &mut dyn FnMut(u16) -> u8) {
        for _ in 0..cycles {
            // Frame sequencer (~240 Hz steps), in CPU cycles.
            self.frame_cycle += 1;
            match self.frame_cycle {
                7457 | 22371 => self.quarter_frame(),
                14913 => {
                    self.quarter_frame();
                    self.half_frame();
                }
                29829 if !self.frame_mode5 => {
                    self.quarter_frame();
                    self.half_frame();
                    if !self.irq_inhibit {
                        self.frame_irq = true;
                    }
                    self.frame_cycle = 0;
                }
                37281 => {
                    self.quarter_frame();
                    self.half_frame();
                    self.frame_cycle = 0;
                }
                _ => {}
            }

            self.triangle.clock_timer();
            self.odd = !self.odd;
            if self.odd {
                self.pulse1.clock_timer();
                self.pulse2.clock_timer();
                self.noise.clock_timer();
            }
            self.clock_dmc(read);

            // Mix and box-filter down to the output rate.
            let p = (self.pulse1.output() + self.pulse2.output()) as f32;
            let pulse_out = if p == 0.0 { 0.0 } else { 95.88 / (8128.0 / p + 100.0) };
            let (t, n, d) = (self.triangle.output() as f32, self.noise.output() as f32, self.dmc.output as f32);
            let tnd_in = t / 8227.0 + n / 12241.0 + d / 22638.0;
            let tnd_out = if tnd_in == 0.0 { 0.0 } else { 159.79 / (1.0 / tnd_in + 100.0) };
            self.acc += pulse_out + tnd_out + self.expansion;
            self.acc_n += 1;
            self.sample_clock += 1.0;
            if self.sample_clock >= self.cycles_per_sample {
                self.sample_clock -= self.cycles_per_sample;
                let raw = self.acc / self.acc_n as f32;
                self.acc = 0.0;
                self.acc_n = 0;
                // DC-blocking high-pass (~20 Hz), like the console's output capacitors.
                let out = raw - self.hp_prev_in + 0.9974 * self.hp_prev_out;
                self.hp_prev_in = raw;
                self.hp_prev_out = out;
                let s = (out * 1.6).clamp(-1.0, 1.0);
                self.samples.push(s);
                self.samples.push(s);
            }
        }
    }

    fn clock_dmc(&mut self, read: &mut dyn FnMut(u16) -> u8) {
        let d = &mut self.dmc;
        if d.buffer.is_none() && d.remaining > 0 {
            d.buffer = Some(read(d.addr));
            d.addr = if d.addr == 0xFFFF { 0x8000 } else { d.addr + 1 };
            d.remaining -= 1;
            if d.remaining == 0 && d.loop_ {
                d.addr = d.sample_addr;
                d.remaining = d.sample_len;
            }
        }
        if d.timer > 0 {
            d.timer -= 1;
            return;
        }
        d.timer = d.rate - 1;
        if !d.silent {
            if d.shift & 1 != 0 {
                if d.output <= 125 {
                    d.output += 2;
                }
            } else if d.output >= 2 {
                d.output -= 2;
            }
        }
        d.shift >>= 1;
        if d.bits > 0 {
            d.bits -= 1;
        }
        if d.bits == 0 {
            d.bits = 8;
            match d.buffer.take() {
                Some(b) => {
                    d.silent = false;
                    d.shift = b;
                }
                None => d.silent = true,
            }
        }
    }
}
