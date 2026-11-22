//! Nintendo Virtual Boy: our V810 CPU, the VIP (32 "worlds" of tile maps, H-bias, affine and
//! sprite layers), the VSU (6-channel wavetable sound), the timer and the controller.
//! The display is stereoscopic; we show the left eye in the console's red.

pub mod cpu;

use cpu::Cpu;

pub const WIDTH: usize = 384;
pub const HEIGHT: usize = 224;
pub const FPS: f64 = 50.27;
const CPU_HZ: u32 = 20_000_000;
const CYCLES_PER_FRAME: u32 = (CPU_HZ as f64 / FPS) as u32;

// VIP interrupt bits.
const INT_LFBEND: u16 = 0x0002;
const INT_RFBEND: u16 = 0x0004;
const INT_GAMESTART: u16 = 0x0008;
const INT_FRAMESTART: u16 = 0x0010;
const INT_XPEND: u16 = 0x4000;

fn sext(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

#[derive(state::Stateful)]
struct Vsu {
    regs: Vec<u8>,
    counter: [u32; 6],
    pos: [usize; 6],
    env: [u8; 6],
    env_count: [u32; 6],
    length: [u32; 6],
    on: [bool; 6],
    lfsr: u16,
    sub: u32,
    #[state(skip)]
    samples: Vec<f32>,
    #[state(skip)]
    rate: f64,
    phase: f64,
    acc: (f32, f32),
    acc_n: u32,
}

impl Vsu {
    fn new() -> Vsu {
        Vsu { regs: vec![0; 0x600], counter: [0; 6], pos: [0; 6], env: [0; 6], env_count: [0; 6], length: [0; 6], on: [false; 6], lfsr: 1, sub: 0, samples: Vec::new(), rate: 48_000.0, phase: 0.0, acc: (0.0, 0.0), acc_n: 0 }
    }

    fn reg(&self, ch: usize, r: usize) -> u8 {
        self.regs[0x400 + ch * 0x40 + r]
    }

    fn write(&mut self, off: usize, v: u8) {
        let off = off & 0x7FF;
        if off < self.regs.len() {
            self.regs[off] = v;
        }
        if off == 0x580 && v & 1 != 0 {
            self.on = [false; 6];
        }
        if (0x400..0x580).contains(&off) {
            let ch = (off - 0x400) / 0x40;
            match (off - 0x400) % 0x40 {
                0x00 => {
                    self.on[ch] = v & 0x80 != 0;
                    self.length[ch] = if v & 0x20 != 0 { ((v & 0x1F) as u32 + 1) * 76_800 } else { 0 };
                    self.pos[ch] = 0;
                    self.counter[ch] = 0;
                    self.env[ch] = self.reg(ch, 0x10) >> 4;
                    self.env_count[ch] = 0;
                }
                0x10 => self.env[ch] = v >> 4,
                _ => {}
            }
        }
    }

    fn freq(&self, ch: usize) -> u32 {
        self.reg(ch, 0x08) as u32 | ((self.reg(ch, 0x0C) as u32 & 7) << 8)
    }

    /// Advances by CPU cycles (the VSU runs at a quarter of the CPU clock).
    fn tick(&mut self, cycles: u32) {
        for _ in 0..cycles {
            self.sub += 1;
            if self.sub < 4 {
                continue;
            }
            self.sub = 0;
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for ch in 0..6 {
                if !self.on[ch] {
                    continue;
                }
                if self.length[ch] > 0 {
                    self.length[ch] = self.length[ch].saturating_sub(4);
                    if self.length[ch] == 0 {
                        self.on[ch] = false;
                        continue;
                    }
                }
                let period = (2048 - self.freq(ch)).max(1) * if ch == 5 { 10 } else { 1 };
                self.counter[ch] += 1;
                if self.counter[ch] >= period {
                    self.counter[ch] = 0;
                    if ch == 5 {
                        const TAPS: [u16; 8] = [14, 10, 13, 4, 8, 6, 9, 11];
                        let tap = TAPS[((self.reg(5, 0x14) >> 4) & 7) as usize];
                        let bit = ((self.lfsr >> 7) ^ (self.lfsr >> tap)) & 1;
                        self.lfsr = ((self.lfsr << 1) | bit) & 0x7FFF;
                    } else {
                        self.pos[ch] = (self.pos[ch] + 1) & 31;
                    }
                }
                // Envelope.
                let ev0 = self.reg(ch, 0x10);
                if self.reg(ch, 0x14) & 1 != 0 {
                    self.env_count[ch] += 4;
                    if self.env_count[ch] >= ((ev0 & 7) as u32 + 1) * 76_800 {
                        self.env_count[ch] = 0;
                        if ev0 & 8 != 0 {
                            self.env[ch] = (self.env[ch] + 1).min(15);
                        } else {
                            self.env[ch] = self.env[ch].saturating_sub(1);
                        }
                    }
                }
                let sample = if ch == 5 {
                    if self.lfsr & 1 != 0 { 63 } else { 0 }
                } else {
                    let table = (self.reg(ch, 0x18) & 7).min(4) as usize;
                    self.regs[table * 0x80 + self.pos[ch] * 4] & 0x3F
                } as f32
                    - 31.5;
                let lrv = self.reg(ch, 0x04);
                let e = self.env[ch] as f32 / 15.0;
                l += sample * e * (lrv >> 4) as f32 / 15.0;
                r += sample * e * (lrv & 0xF) as f32 / 15.0;
            }
            self.acc.0 += l / 6.0 / 32.0;
            self.acc.1 += r / 6.0 / 32.0;
            self.acc_n += 1;
            self.phase += self.rate;
            let step = CPU_HZ as f64 / 4.0;
            while self.phase >= step {
                self.phase -= step;
                let n = self.acc_n.max(1) as f32;
                self.samples.push(self.acc.0 / n);
                self.samples.push(self.acc.1 / n);
                self.acc = (0.0, 0.0);
                self.acc_n = 0;
            }
        }
    }
}

#[derive(state::Stateful)]
pub struct Bus {
    #[state(skip)]
    rom: Vec<u8>,
    wram: Vec<u8>,
    pub sram: Vec<u8>,
    pub sram_used: bool,
    vram: Vec<u8>,
    vip: [u16; 0x40],
    int_pending: u16,
    vsu: Vsu,
    hw: [u8; 0x30],
    #[state(skip)]
    keys: u16,
    timer: u16,
    timer_sub: u32,
    drawing: bool,
}

impl Bus {
    /// Byte offset of a character's 16 bytes in VRAM.
    fn char_addr(i: usize) -> usize {
        let i = i & 0x7FF;
        [0x06000, 0x0E000, 0x16000, 0x1E000][i / 512] + (i % 512) * 16
    }

    fn vram_index(off: usize) -> Option<usize> {
        match off {
            0x00000..=0x3FFFF => Some(off),
            0x78000..=0x7FFFF => {
                let rel = off - 0x78000;
                Some(Self::char_addr(rel / 16) + rel % 16)
            }
            _ => None,
        }
    }

    fn vip_read(&mut self, off: usize) -> u16 {
        let r = (off - 0x5F800) / 2;
        match r {
            0x00 => self.int_pending,
            0x10 => (self.vip[0x11] & 2) | 0x40, // DPSTTS: DISP + SCANRDY
            0x20 => (self.vip[0x21] & 2) | if self.drawing { 0x0C } else { 0 }, // XPSTTS
            0x22 => 2, // VER
            _ => *self.vip.get(r).unwrap_or(&0),
        }
    }

    fn vip_write(&mut self, off: usize, v: u16) {
        let r = (off - 0x5F800) / 2;
        match off - 0x5F800 {
            0x04 => self.int_pending &= !v, // INTCLR
            0x00 | 0x20 | 0x40 | 0x44 => {}  // read-only status registers
            _ => {
                if let Some(x) = self.vip.get_mut(r) {
                    *x = v;
                }
            }
        }
    }

    fn hw_read(&mut self, off: usize) -> u8 {
        match off & 0x3F {
            0x10 => self.keys as u8,
            0x14 => (self.keys >> 8) as u8,
            0x18 => self.timer as u8,
            0x1C => (self.timer >> 8) as u8,
            0x20 => self.hw[0x20] | 0xE4,
            0x28 => 0x4C,
            o => *self.hw.get(o).unwrap_or(&0),
        }
    }

    fn hw_write(&mut self, off: usize, v: u8) {
        let o = off & 0x3F;
        match o {
            0x18 | 0x1C => {
                self.hw[o] = v;
                self.timer = self.hw[0x18] as u16 | (self.hw[0x1C] as u16) << 8;
            }
            0x20 => {
                // Writing bit 2 clears the zero flag (bit 1).
                let mut t = (self.hw[0x20] & 0x02) | (v & 0x19);
                if v & 0x04 != 0 {
                    t &= !0x02;
                }
                self.hw[0x20] = t;
            }
            _ => {
                if o < self.hw.len() {
                    self.hw[o] = v;
                }
            }
        }
    }

    fn tick_timer(&mut self, cycles: u32) -> bool {
        let tcr = self.hw[0x20];
        if tcr & 1 == 0 {
            return false;
        }
        let interval = if tcr & 0x10 != 0 { 400 } else { 2000 };
        self.timer_sub += cycles;
        let mut fired = false;
        while self.timer_sub >= interval {
            self.timer_sub -= interval;
            if self.timer == 0 {
                self.timer = self.hw[0x18] as u16 | (self.hw[0x1C] as u16) << 8;
            } else {
                self.timer -= 1;
                if self.timer == 0 {
                    self.hw[0x20] |= 0x02;
                    fired |= tcr & 0x08 != 0;
                }
            }
        }
        fired
    }
}

impl cpu::Bus for Bus {
    fn read8(&mut self, a: u32) -> u8 {
        let w = self.read16(a & !1);
        if a & 1 == 0 {
            w as u8
        } else {
            (w >> 8) as u8
        }
    }

    fn read16(&mut self, a: u32) -> u16 {
        let a = a & 0x07FF_FFFE;
        let off = (a & 0x00FF_FFFF) as usize;
        match a >> 24 {
            0 => {
                let off = off & 0x7FFFF;
                if (0x5F800..0x5F880).contains(&off) {
                    return self.vip_read(off);
                }
                match Self::vram_index(off) {
                    Some(i) => u16::from_le_bytes([self.vram[i], self.vram[i + 1]]),
                    None => 0,
                }
            }
            2 => self.hw_read(off) as u16,
            5 => {
                let i = off & 0xFFFF;
                u16::from_le_bytes([self.wram[i], self.wram[i + 1]])
            }
            6 => {
                let i = off % self.sram.len();
                u16::from_le_bytes([self.sram[i], self.sram[(i + 1) % self.sram.len()]])
            }
            7 => {
                let i = off % self.rom.len();
                u16::from_le_bytes([self.rom[i], self.rom[(i + 1) % self.rom.len()]])
            }
            _ => 0,
        }
    }

    fn write8(&mut self, a: u32, v: u8) {
        let a = a & 0x07FF_FFFF;
        let off = (a & 0x00FF_FFFF) as usize;
        match a >> 24 {
            0 => {
                let off = off & 0x7FFFF;
                if (0x5F800..0x5F880).contains(&off) {
                    let cur = self.vip_read(off & !1);
                    let nv = if off & 1 == 0 { (cur & 0xFF00) | v as u16 } else { (cur & 0xFF) | (v as u16) << 8 };
                    self.vip_write(off & !1, nv);
                } else if let Some(i) = Self::vram_index(off) {
                    self.vram[i] = v;
                }
            }
            1 => self.vsu.write(off, v),
            2 => self.hw_write(off, v),
            5 => self.wram[off & 0xFFFF] = v,
            6 => {
                let n = self.sram.len();
                self.sram[off % n] = v;
                self.sram_used = true;
            }
            _ => {}
        }
    }

    fn write16(&mut self, a: u32, v: u16) {
        let a = a & 0x07FF_FFFE;
        let off = (a & 0x00FF_FFFF) as usize;
        if a >> 24 == 0 && (0x5F800..0x5F880).contains(&(off & 0x7FFFF)) {
            self.vip_write(off & 0x7FFFF, v);
            return;
        }
        if a >> 24 == 1 || a >> 24 == 2 {
            // Byte-wide devices: only the low byte is connected.
            self.write8(a, v as u8);
            return;
        }
        self.write8(a, v as u8);
        self.write8(a + 1, (v >> 8) as u8);
    }
}

pub struct VirtualBoy {
    cpu: Cpu,
    pub bus: Bus,
    frame: Vec<u32>,
    frame_count: u32,
    stereo: bool,
}

impl state::Stateful for VirtualBoy {
    fn sync(&mut self, s: &mut state::State) {
        s.check(0x5642_0000 ^ self.bus.rom.len() as u32);
        self.cpu.sync(s);
        self.bus.sync(s);
        self.frame_count.sync(s);
    }
}

impl VirtualBoy {
    pub fn new(rom: &[u8]) -> Result<VirtualBoy, String> {
        if rom.len() < 0x400 || rom.len() > 0x100_0000 || !rom.len().is_power_of_two() {
            return Err(format!("Not a Virtual Boy cartridge ({} bytes).", rom.len()));
        }
        let bus = Bus {
            rom: rom.to_vec(),
            wram: vec![0; 0x1_0000],
            sram: vec![0; 0x2000],
            sram_used: false,
            vram: vec![0; 0x4_0000],
            vip: [0; 0x40],
            int_pending: 0,
            vsu: Vsu::new(),
            hw: [0; 0x30],
            keys: 0x0002,
            timer: 0,
            timer_sub: 0,
            drawing: false,
        };
        Ok(VirtualBoy { cpu: Cpu::new(), bus, frame: vec![0; WIDTH * HEIGHT], frame_count: 0, stereo: false })
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    fn vip_irq(&mut self, bits: u16) {
        self.bus.int_pending |= bits;
    }

    fn run(&mut self, cycles: u32) {
        let mut used = 0;
        while used < cycles {
            let vip_level = self.bus.int_pending & self.bus.vip[0x01] != 0;
            let timer = self.bus.hw[0x20] & 0x0A == 0x0A;
            self.cpu.irq = if vip_level { Some(4) } else if timer { Some(1) } else { None };
            let c = self.cpu.step(&mut self.bus);
            self.bus.vsu.tick(c);
            self.bus.tick_timer(c);
            used += c;
        }
    }

    /// One 20 ms display frame: frame start, drawing, drawing-finished, then display.
    pub fn run_frame(&mut self) {
        self.vip_irq(INT_FRAMESTART);
        let game_frame = self.frame_count % ((self.bus.vip[0x17] & 0xF) as u32 + 1) == 0;
        self.frame_count += 1;
        let drawing = game_frame && self.bus.vip[0x21] & 2 != 0;
        if game_frame {
            self.vip_irq(INT_GAMESTART);
        }
        if drawing {
            self.bus.drawing = true;
            self.render();
        }
        self.run(CYCLES_PER_FRAME / 4);
        if drawing {
            self.bus.drawing = false;
            self.vip_irq(INT_XPEND);
        }
        self.run(CYCLES_PER_FRAME / 4);
        self.vip_irq(INT_LFBEND | INT_RFBEND);
        self.run(CYCLES_PER_FRAME - 2 * (CYCLES_PER_FRAME / 4));
    }

    fn level_colour(&self, idx: usize) -> u32 {
        let v = &self.bus.vip;
        let (a, b, c) = (v[0x12] as u32 & 0xFF, v[0x13] as u32 & 0xFF, v[0x14] as u32 & 0xFF);
        let level = match idx & 3 {
            0 => 0,
            1 => a,
            2 => b,
            _ => a + b + c,
        };
        let red = (level * 2).min(255);
        red << 16
    }

    fn char_pixel(&self, ch: usize, x: usize, y: usize) -> usize {
        let a = Bus::char_addr(ch) + y * 2;
        let row = u16::from_le_bytes([self.bus.vram[a], self.bus.vram[a + 1]]);
        ((row >> (x * 2)) & 3) as usize
    }

    /// Colour index (0 = transparent) from a BG map, with the palette number.
    fn bg_texel(&self, base: usize, scx: u32, scy: u32, px: i32, py: i32) -> (usize, usize) {
        let (w, h) = (512i32 << scx, 512i32 << scy);
        let (px, py) = (px.rem_euclid(w) as usize, py.rem_euclid(h) as usize);
        let seg = (base + (py / 512) * (1 << scx) + px / 512) & 0xF;
        let cell = ((py % 512) / 8) * 64 + (px % 512) / 8;
        let a = 0x20000 + seg * 0x2000 + cell * 2;
        let e = u16::from_le_bytes([self.bus.vram[a], self.bus.vram[a + 1]]) as usize;
        let (hf, vf) = (e & 0x2000 != 0, e & 0x1000 != 0);
        let (tx, ty) = (if hf { 7 - px % 8 } else { px % 8 }, if vf { 7 - py % 8 } else { py % 8 });
        (self.char_pixel(e & 0x7FF, tx, ty), e >> 14)
    }

    /// Draws both eyes; the frame is the left eye, or both side by side in stereo mode.
    fn render(&mut self) {
        let left = self.render_eye(false);
        if !self.stereo {
            self.frame = left;
            return;
        }
        let right = self.render_eye(true);
        let mut out = Vec::with_capacity(WIDTH * 2 * HEIGHT);
        for y in 0..HEIGHT {
            out.extend_from_slice(&left[y * WIDTH..(y + 1) * WIDTH]);
            out.extend_from_slice(&right[y * WIDTH..(y + 1) * WIDTH]);
        }
        self.frame = out;
    }

    /// One eye's picture. Parallax moves layers left for the left eye and right for the right.
    fn render_eye(&self, right_eye: bool) -> Vec<u32> {
        let side: i32 = if right_eye { 1 } else { -1 };
        let eye_bit = if right_eye { 0x4000 } else { 0x8000 };
        let v = &self.bus.vip;
        let bk = self.level_colour((v[0x38] & 3) as usize);
        let gplt: [u16; 4] = [v[0x30], v[0x31], v[0x32], v[0x33]];
        let jplt: [u16; 4] = [v[0x34], v[0x35], v[0x36], v[0x37]];
        let spt: [u16; 4] = [v[0x24], v[0x25], v[0x26], v[0x27]];
        let mut out = vec![bk; WIDTH * HEIGHT];
        let plot = |out: &mut Vec<u32>, x: i32, y: i32, c: u32| {
            if (0..WIDTH as i32).contains(&x) && (0..HEIGHT as i32).contains(&y) {
                out[y as usize * WIDTH + x as usize] = c;
            }
        };
        let mut obj_group: i32 = 3;
        for w in (0..32).rev() {
            let base = 0x3D800 + w * 32;
            let hw = |i: usize| u16::from_le_bytes([self.bus.vram[base + i * 2], self.bus.vram[base + i * 2 + 1]]) as u32;
            let h0 = hw(0);
            if h0 & 0x40 != 0 {
                break; // END
            }
            let bgm = (h0 >> 12) & 3;
            let left = h0 & eye_bit != 0;
            if bgm == 3 {
                // Sprite world: draws OAM entries SPT[g] down to SPT[g-1]+1.
                if obj_group < 0 {
                    continue;
                }
                let g = obj_group as usize;
                obj_group -= 1;
                if !left {
                    continue;
                }
                let start = (spt[g] & 0x3FF) as i32;
                let end = if g > 0 { ((spt[g - 1] & 0x3FF) as i32 + 1) & 0x3FF } else { 0 };
                let mut i = start;
                loop {
                    let a = 0x3E000 + i as usize * 8;
                    let o = |k: usize| u16::from_le_bytes([self.bus.vram[a + k * 2], self.bus.vram[a + k * 2 + 1]]) as u32;
                    let (o0, o1, o2, o3) = (o(0), o(1), o(2), o(3));
                    if o1 & eye_bit != 0 {
                        let jx = sext(o0 & 0x3FF, 10);
                        let jp = sext(o1 & 0x3FF, 10);
                        let jy = sext(o2 & 0xFF, 8);
                        let pal = jplt[(o3 >> 14) as usize];
                        let (hf, vf) = (o3 & 0x2000 != 0, o3 & 0x1000 != 0);
                        for y in 0..8 {
                            for x in 0..8 {
                                let c = self.char_pixel((o3 & 0x7FF) as usize, if hf { 7 - x } else { x }, if vf { 7 - y } else { y });
                                if c != 0 {
                                    let col = self.level_colour(((pal >> (c * 2)) & 3) as usize);
                                    plot(&mut out, jx + side * jp + x as i32, jy + y as i32, col);
                                }
                            }
                        }
                    }
                    if i == end {
                        break;
                    }
                    i = (i - 1).rem_euclid(1024);
                }
                continue;
            }
            if !left {
                continue;
            }
            let (map_base, scx, scy) = ((h0 & 0xF) as usize, (h0 >> 10) & 3, (h0 >> 8) & 3);
            let gx = sext(hw(1), 10);
            let gp = sext(hw(2), 10);
            let gy = hw(3) as i16 as i32;
            let mx = sext(hw(4), 13);
            let mp = sext(hw(5), 15);
            let my = sext(hw(6), 13);
            let width = (hw(7) & 0x1FFF) as i32 + 1;
            let height = (hw(8) as i32 + 1).max(1);
            let param = 0x20000 + (hw(9) as usize) * 2;
            for row in 0..height.min(HEIGHT as i32 + 64) {
                let sy = gy + row;
                if !(0..HEIGHT as i32).contains(&sy) {
                    continue;
                }
                for col in 0..width.min(WIDTH as i32 + 64) {
                    let sx = gx + side * gp + col;
                    if !(0..WIDTH as i32).contains(&sx) {
                        continue;
                    }
                    let (c, pal) = match bgm {
                        1 => {
                            // H-bias: a per-row horizontal offset for each eye.
                            let pa = param + row as usize * 4 + if right_eye { 2 } else { 0 };
                            let off = sext(u16::from_le_bytes([self.bus.vram[pa & 0x3FFFF], self.bus.vram[(pa + 1) & 0x3FFFF]]) as u32, 13);
                            self.bg_texel(map_base, scx, scy, mx + side * mp + col + off, my + row)
                        }
                        2 => {
                            // Affine: per-row start point (13.3) and step (7.9).
                            let pa = param + row as usize * 16;
                            let p = |k: usize| u16::from_le_bytes([self.bus.vram[(pa + k * 2) & 0x3FFFF], self.bus.vram[(pa + k * 2 + 1) & 0x3FFFF]]) as i16 as i32;
                            let (amx, amy, dx, dy) = (p(0), p(2), p(3), p(4));
                            let fx = (amx << 6) + dx * col;
                            let fy = (amy << 6) + dy * col;
                            self.bg_texel(map_base, scx, scy, fx >> 9, fy >> 9)
                        }
                        _ => self.bg_texel(map_base, scx, scy, mx + side * mp + col, my + row),
                    };
                    if c != 0 {
                        let colr = self.level_colour(((gplt[pal] >> (c * 2)) & 3) as usize);
                        plot(&mut out, sx, sy, colr);
                    }
                }
            }
        }
        out
    }

    /// Side-by-side 3D (both eyes, 768 x 224) for VR headsets and 3D displays.
    pub fn set_stereo(&mut self, on: bool) {
        self.stereo = on;
        self.frame = vec![0; self.size().0 * HEIGHT];
    }

    pub fn size(&self) -> (usize, usize) {
        (if self.stereo { WIDTH * 2 } else { WIDTH }, HEIGHT)
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    /// A/B, Start, Select, left d-pad; X/Y/L/R: L and R triggers and the right d-pad up/down.
    pub fn set_buttons(&mut self, pad: usize, b: u16) {
        if pad != 0 {
            return;
        }
        let bit = |i: u16| (b >> i) & 1;
        let mut k = 0x0002u16; // signature bit
        k |= bit(0) << 2 | bit(1) << 3; // A, B
        k |= bit(11) << 4 | bit(10) << 5; // R, L triggers
        k |= bit(8) << 6 | bit(9) << 15; // right pad up (X) / down (Y)
        k |= bit(7) << 8 | bit(6) << 9 | bit(5) << 10 | bit(4) << 11; // left pad right, left, down, up
        k |= bit(3) << 12 | bit(2) << 13; // start, select
        self.bus.keys = k;
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.bus.vsu.samples);
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.bus.vsu.rate = r as f64;
    }

    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    pub fn save_ram(&self) -> Option<&[u8]> {
        self.bus.sram_used.then_some(&self.bus.sram[..])
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        if data.len() == self.bus.sram.len() {
            self.bus.sram.copy_from_slice(data);
            self.bus.sram_used = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpu::Bus as _;

    #[test]
    fn world_renders_char_in_red() {
        let rom = vec![0u8; 0x1000];
        let mut v = VirtualBoy::new(&rom).unwrap();
        let b = &mut v.bus;
        // Brightness A = 50, palette GPLT0: colour 1 -> level 1.
        b.write16(0x0005_F824, 50);
        b.write16(0x0005_F860, 0b0000_0100);
        b.write16(0x0005_F842, 2); // XPEN
        // Char 1: row 0 all pixels = 1.
        b.write16(Bus::char_addr(1) as u32, 0x5555);
        // BG map segment 0, cell (0,0) = char 1.
        b.write16(0x0002_0000, 1);
        // World 31: left on, normal BG, map 0; 8x8 window at (0,0). World 30: END.
        let w = 0x3D800 + 31 * 32;
        b.write16(w as u32, 0x8000);
        b.write16((w + 14) as u32, 7); // width - 1
        b.write16((w + 16) as u32, 7); // height - 1
        b.write16((0x3D800 + 30 * 32) as u32, 0x0040);
        v.run_frame();
        assert_eq!(v.frame()[0], 100 << 16, "brightness 50 x 2 in red");
        assert_eq!(v.frame()[WIDTH], 0, "row 1 of the char is empty");
    }

    #[test]
    fn save_state_resumes_identically() {
        // The world scene from the test above; the second run changes the brightness.
        let scene = |v: &mut VirtualBoy| {
            let b = &mut v.bus;
            b.write16(0x0005_F824, 50);
            b.write16(0x0005_F860, 0b0000_0100);
            b.write16(0x0005_F842, 2);
            b.write16(Bus::char_addr(1) as u32, 0x5555);
            b.write16(0x0002_0000, 1);
            let w = 0x3D800 + 31 * 32;
            b.write16(w as u32, 0x8000);
            b.write16((w + 14) as u32, 7);
            b.write16((w + 16) as u32, 7);
            b.write16((0x3D800 + 30 * 32) as u32, 0x0040);
        };
        let rom = vec![0u8; 0x1000];
        let mut a = VirtualBoy::new(&rom).unwrap();
        scene(&mut a);
        a.run_frame();
        let snap = a.save_state();
        a.bus.write16(0x0005_F824, 20);
        a.run_frame();
        a.run_frame();
        let want = a.frame().to_vec();
        assert_eq!(want[0], 40 << 16, "brightness 20 x 2 in red");
        // A fresh machine takes the snapshot, then the same brightness change.
        let mut b = VirtualBoy::new(&rom).unwrap();
        b.load_state(&snap).unwrap();
        b.bus.write16(0x0005_F824, 20);
        b.run_frame();
        b.run_frame();
        assert_eq!(b.frame().to_vec(), want);
        // Loading restores the scene, not just the registers.
        b.load_state(&snap).unwrap();
        b.run_frame();
        assert_eq!(b.frame()[0], 100 << 16);
    }

    #[test]
    fn stereo_side_by_side() {
        let rom = vec![0u8; 0x1000];
        let mut v = VirtualBoy::new(&rom).unwrap();
        v.set_stereo(true);
        let b = &mut v.bus;
        b.write16(0x0005_F824, 50);
        b.write16(0x0005_F860, 0b0000_0100);
        b.write16(0x0005_F842, 2);
        b.write16(Bus::char_addr(1) as u32, 0x5555);
        b.write16(0x0002_0000, 1);
        let w = 0x3D800 + 31 * 32;
        b.write16(w as u32, 0xC000); // both eyes
        b.write16((w + 2) as u32, 0); // GX
        b.write16((w + 4) as u32, 4); // GP: 4 pixels of parallax
        b.write16((w + 14) as u32, 7);
        b.write16((w + 16) as u32, 7);
        b.write16((0x3D800 + 30 * 32) as u32, 0x0040);
        v.run_frame();
        assert_eq!(v.size(), (768, 224));
        let row = &v.frame()[..768];
        // Left eye at GX - GP = -4 (columns 0-3 visible); right eye at GX + GP = 4.
        assert_eq!(row[0], 100 << 16);
        assert_eq!(row[4], 0);
        assert_eq!(row[384 + 4], 100 << 16);
        assert_eq!(row[384 + 3], 0);
    }

    #[test]
    fn vip_interrupt_reaches_cpu() {
        // ROM at 0x07000000 mirrors; reset vector 0xFFFFFFF0 -> end of ROM: JR to 0x07000000 region start.
        let mut rom = vec![0u8; 0x1000];
        // At rom[0]: set INTENB = FRAMESTART via MOVHI/MOVEA + ST.H, then CLI and HALT.
        let ins = |op: u32, r2: u32, r1: u32| ((op << 10 | r2 << 5 | r1) as u16).to_le_bytes();
        let mut p: Vec<u8> = vec![];
        p.extend(ins(0x2F, 10, 0)); // MOVHI 0x0006, r0, r10
        p.extend(0x0006u16.to_le_bytes());
        p.extend(ins(0x28, 10, 10)); // MOVEA 0xF802, r10, r10  -> 0x0005F802
        p.extend(0xF802u16.to_le_bytes());
        p.extend(ins(0x10, 11, 0x10)); // MOV 0x10 (sign-extended -16 -> use 0x10 via MOVEA)
        p.extend(ins(0x28, 11, 0)); // MOVEA 0x0010, r0, r11
        p.extend(0x0010u16.to_le_bytes());
        p.extend(ins(0x35, 11, 10)); // ST.H r11, 0[r10]
        p.extend(0u16.to_le_bytes());
        p.extend(ins(0x1C, 0, 5)); // LDSR r0 -> PSW (clear NP/ID)
        p.extend(ins(0x1A, 0, 0)); // HALT
        rom[..p.len()].copy_from_slice(&p);
        // Reset at 0xFFFFFFF0 = rom[0xFF0]: JR back to rom[0] (displacement -0xFF0).
        let d = (-0xFF0i32 as u32) & 0x3FF_FFFF;
        rom[0xFF0..0xFF2].copy_from_slice(&((0x2A << 10 | (d >> 16)) as u16).to_le_bytes());
        rom[0xFF2..0xFF4].copy_from_slice(&(d as u16).to_le_bytes());
        // VIP handler (level 4) at 0xFFFFFE40 = rom[0xE40]: MOV 1,r20 ; HALT
        rom[0xE40..0xE42].copy_from_slice(&ins(0x10, 20, 1));
        rom[0xE42..0xE44].copy_from_slice(&ins(0x1A, 0, 0));
        let mut v = VirtualBoy::new(&rom).unwrap();
        v.run_frame();
        v.run_frame();
        assert_eq!(v.cpu.r[20], 1, "frame-start interrupt handled");
    }
}
