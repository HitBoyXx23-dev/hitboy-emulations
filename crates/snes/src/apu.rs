//! SNES audio unit: SPC700 (our C++ core) with 64 KiB ARAM, timers, CPU ports,
//! and the S-DSP (8 BRR voices, ADSR/GAIN envelopes, noise, pitch modulation, echo).

use cpu65816::spc::{Bus as SpcBus, Spc};

/// SPC700 cycles per master clock (1.024 MHz vs 21.477 MHz).
const MASTER_PER_SPC: f64 = 21_477_272.0 / 1_024_000.0;

/// Our own boot program for FFC0-FFFF. It implements the standard upload protocol:
/// announce $AA/$BB, wait for $CC, then receive indexed bytes into ARAM and either
/// start another block or jump to the uploaded code. (Written for this emulator;
/// it is not the console's boot ROM.)
const BOOT: [u8; 64] = [
    0xCD, 0xEF, // FFC0 mov x,#$EF
    0xBD, // FFC2 mov sp,x
    0x8F, 0xAA, 0xF4, // FFC3 mov $F4,#$AA
    0x8F, 0xBB, 0xF5, // FFC6 mov $F5,#$BB
    0xE4, 0xF4, // FFC9 wait: mov a,$F4
    0x68, 0xCC, // FFCB cmp a,#$CC
    0xD0, 0xFA, // FFCD bne wait
    0xBA, 0xF6, // FFCF cmd: movw ya,$F6   ; destination / entry address
    0xDA, 0x00, // FFD1 movw $00,ya
    0xE4, 0xF5, // FFD3 mov a,$F5          ; 0 = run, else another block follows
    0xEB, 0xF4, // FFD5 mov y,$F4
    0xCB, 0xF4, // FFD7 mov $F4,y          ; acknowledge the command
    0x68, 0x00, // FFD9 cmp a,#$00
    0xF0, 0x17, // FFDB beq run
    0x8D, 0x00, // FFDD mov y,#$00
    0x7E, 0xF4, // FFDF data: cmp y,$F4    ; byte with index y ready?
    0xF0, 0x04, // FFE1 beq store
    0x10, 0xFA, // FFE3 bpl data           ; still the previous index: keep waiting
    0x2F, 0xE8, // FFE5 bra cmd            ; index jumped ahead: new command
    0xE4, 0xF5, // FFE7 store: mov a,$F5
    0xD7, 0x00, // FFE9 mov [$00]+y,a
    0xCB, 0xF4, // FFEB mov $F4,y
    0xFC, // FFED inc y
    0xD0, 0xEF, // FFEE bne data
    0xAB, 0x01, // FFF0 inc $01
    0x2F, 0xEB, // FFF2 bra data
    0xCD, 0x00, // FFF4 run: mov x,#$00
    0x1F, 0x00, 0x00, // FFF6 jmp [$0000+x]
    0x00, 0x00, 0x00, 0x00, 0x00, // FFF9 padding
    0xC0, 0xFF, // FFFE reset vector
];

#[derive(Default, Clone, Copy, state::Stateful)]
struct Timer {
    enabled: bool,
    target: u8,
    stage: u8,
    out: u8,
}

impl Timer {
    fn clock(&mut self) {
        if !self.enabled {
            return;
        }
        self.stage = self.stage.wrapping_add(1);
        if self.stage == self.target {
            self.stage = 0;
            self.out = (self.out + 1) & 0x0F;
        }
    }
}

// ------------------------------------------------------------------ DSP

#[derive(Clone, Copy, PartialEq, Eq, Default, state::Stateful)]
enum EnvMode {
    #[default]
    Release,
    Attack,
    Decay,
    Sustain,
}

#[derive(Default, Clone, Copy, state::Stateful)]
struct Voice {
    brr_addr: u16,
    loop_addr: u16,
    block: [i16; 16],
    block_pos: usize,
    block_end: bool,
    block_loop: bool,
    history: [i16; 4], // last four decoded samples, newest at [3]
    prev: [i16; 2],    // BRR filter state
    counter: u32,
    env: i32,
    mode: EnvMode,
    env_timer: u32,
    output: i32,
    started: bool,
}

/// Envelope step periods in 32 kHz samples (0 = never).
const RATES: [u32; 32] = [
    0, 2048, 1536, 1280, 1024, 768, 640, 512, 384, 320, 256, 192, 160, 128, 96, 80, 64, 48, 40, 32, 24, 20, 16, 12, 10, 8, 6, 5, 4, 3, 2, 1,
];

#[derive(state::Stateful)]
pub struct Dsp {
    regs: [u8; 128],
    voices: [Voice; 8],
    endx: u8,
    noise: i16,
    noise_timer: u32,
    echo_pos: usize,
    fir: [[i16; 8]; 2],
    fir_pos: usize,
    /// Samples produced since they were last collected (audio output, not state).
    #[state(skip)]
    pub out: Vec<(i16, i16)>,
}

impl Default for Dsp {
    fn default() -> Self {
        let mut regs = [0u8; 128];
        regs[0x6C] = 0xE0; // soft reset + mute + echo off
        Self { regs, voices: [Voice::default(); 8], endx: 0, noise: 0x4000, noise_timer: 0, echo_pos: 0, fir: [[0; 8]; 2], fir_pos: 0, out: Vec::new() }
    }
}

fn clamp16(v: i32) -> i32 {
    v.clamp(-32768, 32767)
}

impl Dsp {
    pub fn debug_reg(&self, a: u8) -> u8 {
        self.regs[(a & 0x7F) as usize]
    }
    pub fn debug_started(&self) -> Vec<bool> {
        self.voices.iter().map(|v| v.started).collect()
    }
    pub fn debug_env(&self) -> Vec<i32> {
        self.voices.iter().map(|v| v.env).collect()
    }

    fn read(&self, a: u8) -> u8 {
        let a = a & 0x7F;
        match a {
            0x7C => self.endx,
            _ if a & 0x0F == 0x08 => (self.voices[(a >> 4) as usize].env >> 4) as u8,
            _ if a & 0x0F == 0x09 => (self.voices[(a >> 4) as usize].output >> 8) as u8,
            _ => self.regs[a as usize],
        }
    }

    fn write(&mut self, a: u8, v: u8, ram: &[u8]) {
        if a >= 0x80 {
            return;
        }
        self.regs[a as usize] = v;
        match a {
            0x4C => {
                for i in 0..8 {
                    if v & (1 << i) != 0 {
                        self.key_on(i, ram);
                    }
                }
            }
            0x5C => {
                for i in 0..8 {
                    if v & (1 << i) != 0 {
                        self.voices[i].mode = EnvMode::Release;
                    }
                }
            }
            0x7C => self.endx = 0,
            _ => {}
        }
    }

    fn key_on(&mut self, i: usize, ram: &[u8]) {
        let dir = (self.regs[0x5D] as usize) << 8;
        let srcn = self.regs[i * 0x10 + 4] as usize;
        let entry = (dir + srcn * 4) & 0xFFFF;
        let start = u16::from_le_bytes([ram[entry], ram[(entry + 1) & 0xFFFF]]);
        let lp = u16::from_le_bytes([ram[(entry + 2) & 0xFFFF], ram[(entry + 3) & 0xFFFF]]);
        let v = &mut self.voices[i];
        *v = Voice { brr_addr: start, loop_addr: lp, mode: EnvMode::Attack, started: true, block_pos: 16, ..Voice::default() };
        self.endx &= !(1 << i);
    }

    fn decode_block(&mut self, i: usize, ram: &[u8]) {
        let v = &mut self.voices[i];
        let a = v.brr_addr as usize;
        let header = ram[a];
        let range = header >> 4;
        let filter = (header >> 2) & 3;
        v.block_end = header & 1 != 0;
        v.block_loop = header & 2 != 0;
        for n in 0..16 {
            let byte = ram[(a + 1 + n / 2) & 0xFFFF];
            let nib = if n % 2 == 0 { (byte as i8) >> 4 } else { ((byte << 4) as i8) >> 4 } as i32;
            let mut s = if range <= 12 { (nib << range) >> 1 } else if nib < 0 { -2048 } else { 0 };
            let (p1, p2) = (v.prev[0] as i32, v.prev[1] as i32);
            s += match filter {
                0 => 0,
                1 => p1 + ((-p1) >> 4),
                2 => (p1 << 1) + ((-p1 * 3) >> 5) - p2 + (p2 >> 4),
                _ => (p1 << 1) + ((-p1 * 13) >> 6) - p2 + ((p2 * 3) >> 4),
            };
            let s = ((clamp16(s) << 1) as i16) as i32; // hardware keeps 15 bits, doubled
            v.prev = [s as i16, v.prev[0]];
            v.block[n] = s as i16;
        }
        v.block_pos = 0;
        v.brr_addr = v.brr_addr.wrapping_add(9);
        if v.block_end {
            self.endx |= 1 << i;
            if v.block_loop {
                v.brr_addr = v.loop_addr;
            } else {
                v.mode = EnvMode::Release;
                v.env = 0;
            }
        }
    }

    fn envelope(&mut self, i: usize) {
        let base = i * 0x10;
        let (adsr1, adsr2, gain) = (self.regs[base + 5], self.regs[base + 6], self.regs[base + 7]);
        let v = &mut self.voices[i];
        if v.mode == EnvMode::Release {
            v.env = (v.env - 8).max(0);
            return;
        }
        let (rate, step): (u32, i32);
        if adsr1 & 0x80 != 0 {
            match v.mode {
                EnvMode::Attack => {
                    rate = ((adsr1 & 0x0F) as u32) * 2 + 1;
                    step = if rate == 31 { 1024 } else { 32 };
                }
                EnvMode::Decay => {
                    rate = (((adsr1 >> 4) & 7) as u32) * 2 + 16;
                    step = -(((v.env - 1) >> 8) + 1);
                }
                _ => {
                    rate = (adsr2 & 0x1F) as u32;
                    step = -(((v.env - 1) >> 8) + 1);
                }
            }
        } else if gain & 0x80 == 0 {
            v.env = ((gain & 0x7F) as i32) << 4;
            return;
        } else {
            rate = (gain & 0x1F) as u32;
            step = match (gain >> 5) & 3 {
                0 => -32,
                1 => -(((v.env - 1) >> 8) + 1),
                2 => 32,
                _ => if v.env < 0x600 { 32 } else { 8 },
            };
        }
        let period = RATES[rate as usize];
        if period == 0 {
            return;
        }
        v.env_timer += 1;
        if v.env_timer < period {
            return;
        }
        v.env_timer = 0;
        v.env = (v.env + step).clamp(0, 0x7FF);
        if adsr1 & 0x80 != 0 {
            match v.mode {
                EnvMode::Attack if v.env >= 0x7E0 => {
                    v.env = 0x7FF;
                    v.mode = EnvMode::Decay;
                }
                EnvMode::Decay if (v.env >> 8) as u8 <= (adsr2 >> 5) => v.mode = EnvMode::Sustain,
                _ => {}
            }
        }
    }

    /// Produces one 32 kHz stereo sample.
    pub fn sample(&mut self, ram: &mut [u8]) {
        let flg = self.regs[0x6C];
        if flg & 0x80 != 0 {
            for v in self.voices.iter_mut() {
                v.mode = EnvMode::Release;
                v.env = 0;
            }
        }
        // Noise generator (shared by all voices with NON set).
        let nrate = RATES[(flg & 0x1F) as usize];
        if nrate != 0 {
            self.noise_timer += 1;
            if self.noise_timer >= nrate {
                self.noise_timer = 0;
                let fb = (self.noise << 13) ^ (self.noise << 14);
                self.noise = ((fb & 0x4000) ^ (self.noise >> 1)) & 0x7FFF;
            }
        }

        let (pmon, non, eon) = (self.regs[0x2D], self.regs[0x3D], self.regs[0x4D]);
        let (mut main_l, mut main_r, mut echo_l, mut echo_r) = (0i32, 0i32, 0i32, 0i32);
        let mut prev_out = 0i32;
        for i in 0..8 {
            let base = i * 0x10;
            if !self.voices[i].started {
                prev_out = 0;
                continue;
            }
            // Pitch (with optional modulation by the previous voice's output).
            let mut pitch = (u16::from_le_bytes([self.regs[base + 2], self.regs[base + 3]]) & 0x3FFF) as i32;
            if i > 0 && pmon & (1 << i) != 0 {
                pitch += ((prev_out >> 5) * pitch) >> 10;
                pitch = pitch.clamp(0, 0x3FFF);
            }
            self.voices[i].counter += pitch as u32;
            while self.voices[i].counter >= 0x1000 {
                self.voices[i].counter -= 0x1000;
                if self.voices[i].block_pos >= 16 {
                    self.decode_block(i, ram);
                }
                let v = &mut self.voices[i];
                let s = v.block[v.block_pos];
                v.block_pos += 1;
                v.history = [v.history[1], v.history[2], v.history[3], s];
            }
            let v = &self.voices[i];
            let raw = if non & (1 << i) != 0 {
                (self.noise as i32) << 1
            } else {
                // 4-point Hermite interpolation between history[1] and history[2].
                let t = (v.counter & 0xFFF) as f32 / 4096.0;
                let [p0, p1, p2, p3] = v.history.map(|s| s as f32);
                let c1 = 0.5 * (p2 - p0);
                let c2 = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
                let c3 = 0.5 * (p3 - p0) + 1.5 * (p1 - p2);
                (((c3 * t + c2) * t + c1) * t + p1) as i32
            };
            self.envelope(i);
            let v = &mut self.voices[i];
            let out = clamp16(raw) * v.env >> 11;
            v.output = out;
            prev_out = out;
            let vl = self.regs[base] as i8 as i32;
            let vr = self.regs[base + 1] as i8 as i32;
            let (l, r) = ((out * vl) >> 7, (out * vr) >> 7);
            main_l += l;
            main_r += r;
            if eon & (1 << i) != 0 {
                echo_l += l;
                echo_r += r;
            }
        }

        // Echo: read the delayed sample, run the 8-tap FIR, write input + feedback back.
        let esa = (self.regs[0x6D] as usize) << 8;
        let edl = (self.regs[0x7D] & 0x0F) as usize;
        let len = if edl == 0 { 4 } else { edl * 0x800 };
        let ea = (esa + self.echo_pos) & 0xFFFF;
        let rd16 = |ram: &[u8], a: usize| i16::from_le_bytes([ram[a & 0xFFFF], ram[(a + 1) & 0xFFFF]]);
        self.fir[0][self.fir_pos] = rd16(ram, ea) >> 1;
        self.fir[1][self.fir_pos] = rd16(ram, ea + 2) >> 1;
        let mut fir_out = [0i32; 2];
        for (ch, out) in fir_out.iter_mut().enumerate() {
            let mut acc = 0i32;
            for tap in 0..8 {
                let coef = self.regs[tap * 0x10 + 0x0F] as i8 as i32;
                let s = self.fir[ch][(self.fir_pos + 1 + tap) % 8] as i32;
                acc += (s * coef) >> 6;
            }
            *out = clamp16(acc);
        }
        self.fir_pos = (self.fir_pos + 1) % 8;
        let efb = self.regs[0x0D] as i8 as i32;
        if flg & 0x20 == 0 {
            let wl = clamp16(echo_l + ((fir_out[0] * efb) >> 7)) & !1;
            let wr = clamp16(echo_r + ((fir_out[1] * efb) >> 7)) & !1;
            let wl = (wl as i16).to_le_bytes();
            let wr = (wr as i16).to_le_bytes();
            ram[ea] = wl[0];
            ram[(ea + 1) & 0xFFFF] = wl[1];
            ram[(ea + 2) & 0xFFFF] = wr[0];
            ram[(ea + 3) & 0xFFFF] = wr[1];
        }
        self.echo_pos = (self.echo_pos + 4) % len;

        let mvl = self.regs[0x0C] as i8 as i32;
        let mvr = self.regs[0x1C] as i8 as i32;
        let evl = self.regs[0x2C] as i8 as i32;
        let evr = self.regs[0x3C] as i8 as i32;
        let mut l = clamp16(((clamp16(main_l) * mvl) >> 7) + ((fir_out[0] * evl) >> 7));
        let mut r = clamp16(((clamp16(main_r) * mvr) >> 7) + ((fir_out[1] * evr) >> 7));
        if flg & 0x40 != 0 {
            l = 0;
            r = 0;
        }
        self.out.push((l as i16, r as i16));
    }
}

// ------------------------------------------------------------------ SPC bus

#[derive(state::Stateful)]
pub struct ApuBus {
    pub ram: Box<[u8; 0x10000]>,
    pub from_cpu: [u8; 4],
    pub to_cpu: [u8; 4],
    control: u8,
    dsp_addr: u8,
    pub dsp: Dsp,
    timers: [Timer; 3],
    pub cycles: u64,
    divider: u32,
}

impl ApuBus {
    #[inline]
    fn tick(&mut self) {
        self.cycles += 1;
        self.divider += 1;
        if self.divider % 16 == 0 {
            self.timers[2].clock(); // 64 kHz
        }
        if self.divider % 128 == 0 {
            self.timers[0].clock(); // 8 kHz
            self.timers[1].clock();
        }
        if self.divider % 32 == 0 {
            self.dsp.sample(&mut self.ram[..]);
        }
    }
}

impl ApuBus {
    /// Writes a DSP register directly (tests / tools).
    pub fn dsp_write_for_test(&mut self, a: u8, v: u8) {
        self.dsp.write(a, v, &self.ram[..]);
    }
}

impl SpcBus for ApuBus {
    fn read(&mut self, a: u16) -> u8 {
        self.tick();
        match a {
            0xF2 => self.dsp_addr,
            0xF3 => self.dsp.read(self.dsp_addr),
            0xF4..=0xF7 => self.from_cpu[(a - 0xF4) as usize],
            0xFD..=0xFF => {
                let t = &mut self.timers[(a - 0xFD) as usize];
                let v = t.out;
                t.out = 0;
                v
            }
            0xFFC0..=0xFFFF if self.control & 0x80 != 0 => BOOT[(a - 0xFFC0) as usize],
            _ => self.ram[a as usize],
        }
    }

    fn write(&mut self, a: u16, v: u8) {
        self.tick();
        match a {
            0xF1 => {
                for (i, t) in self.timers.iter_mut().enumerate() {
                    let on = v & (1 << i) != 0;
                    if on && !t.enabled {
                        t.stage = 0;
                        t.out = 0;
                    }
                    t.enabled = on;
                }
                if v & 0x10 != 0 {
                    self.from_cpu[0] = 0;
                    self.from_cpu[1] = 0;
                }
                if v & 0x20 != 0 {
                    self.from_cpu[2] = 0;
                    self.from_cpu[3] = 0;
                }
                self.control = v;
            }
            0xF2 => self.dsp_addr = v,
            0xF3 => self.dsp.write(self.dsp_addr, v, &self.ram[..]),
            0xF4..=0xF7 => self.to_cpu[(a - 0xF4) as usize] = v,
            0xFA..=0xFC => self.timers[(a - 0xFA) as usize].target = v,
            _ => {}
        }
        // Writes always land in RAM too (register addresses shadow into it).
        self.ram[a as usize] = v;
    }

    fn idle(&mut self) {
        self.tick();
    }
}

// ------------------------------------------------------------------ APU

pub struct Apu {
    pub spc: Spc<ApuBus>,
    owed: f64,
    pub out_rate: u32,
    resample_pos: f64,
    last: (f32, f32),
    pub samples: Vec<f32>,
}

impl state::Stateful for Apu {
    fn sync(&mut self, s: &mut state::State) {
        self.spc.sync_state(s);
        self.spc.bus_mut().sync(s);
        self.owed.sync(s);
        self.resample_pos.sync(s);
        self.last.sync(s);
    }
}

impl Default for Apu {
    fn default() -> Self {
        let bus = ApuBus {
            ram: Box::new([0; 0x10000]),
            from_cpu: [0; 4],
            to_cpu: [0; 4],
            control: 0x80,
            dsp_addr: 0,
            dsp: Dsp::default(),
            timers: [Timer::default(); 3],
            cycles: 0,
            divider: 0,
        };
        Self { spc: Spc::new(bus), owed: 0.0, out_rate: 48_000, resample_pos: 0.0, last: (0.0, 0.0), samples: Vec::new() }
    }
}

impl Apu {
    pub fn cpu_read(&mut self, port: usize) -> u8 {
        self.spc.bus().to_cpu[port & 3]
    }

    pub fn cpu_write(&mut self, port: usize, v: u8) {
        self.spc.bus_mut().from_cpu[port & 3] = v;
    }

    /// Runs the audio CPU for `master` main-clock cycles.
    pub fn advance(&mut self, master: u32) {
        self.owed += master as f64 / MASTER_PER_SPC;
        let target = self.spc.bus().cycles + self.owed as u64;
        self.owed -= self.owed.floor();
        while self.spc.bus().cycles < target {
            self.spc.step();
        }
        self.resample();
    }

    /// 32 kHz DSP output to the host rate (linear interpolation), interleaved stereo f32.
    fn resample(&mut self) {
        let src: Vec<(i16, i16)> = std::mem::take(&mut self.spc.bus_mut().dsp.out);
        let step = 32_000.0 / self.out_rate as f64;
        for (l, r) in src {
            let cur = (l as f32 / 32768.0, r as f32 / 32768.0);
            while self.resample_pos < 1.0 {
                let t = self.resample_pos as f32;
                self.samples.push(self.last.0 + (cur.0 - self.last.0) * t);
                self.samples.push(self.last.1 + (cur.1 - self.last.1) * t);
                self.resample_pos += step;
            }
            self.resample_pos -= 1.0;
            self.last = cur;
        }
    }
}
