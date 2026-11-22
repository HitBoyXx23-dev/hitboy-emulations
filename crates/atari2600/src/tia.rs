//! TIA: the 2600's video and sound chip. The picture is built one colour clock at a time as
//! the beam sweeps (228 clocks per line, 160 visible), from the playfield, two players, two
//! missiles and the ball. Games change registers mid-line to "race the beam".

pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 228;
/// First line shown, counted from the end of VSYNC.
const TOP: u32 = 24;
const HBLANK: u32 = 68;
pub const CLOCKS_PER_LINE: u32 = 228;
/// Audio is clocked twice per line.
pub const AUDIO_RATE: f64 = 3_579_545.0 / 228.0 * 2.0;

/// Player/missile copy offsets for NUSIZ 0-7.
const COPIES: [&[u32]; 8] = [&[0], &[0, 16], &[0, 32], &[0, 16, 32], &[0, 64], &[0], &[0, 32, 64], &[0]];

#[derive(state::Stateful)]
pub struct Tia {
    pub clock: u32,
    pub line: u32,
    #[state(skip)]
    pub frame: Vec<u32>,
    pub frame_done: bool,
    vsync: bool,
    vblank: u8,
    colup0: u8,
    colup1: u8,
    colupf: u8,
    colubk: u8,
    ctrlpf: u8,
    pf: [u8; 3],
    nusiz: [u8; 2],
    refp: [bool; 2],
    grp_new: [u8; 2],
    grp_old: [u8; 2],
    vdelp: [bool; 2],
    enam: [bool; 2],
    enabl_new: bool,
    enabl_old: bool,
    vdelbl: bool,
    resmp: [bool; 2],
    pos_p: [u32; 2],
    pos_m: [u32; 2],
    pos_bl: u32,
    hmp: [i8; 2],
    hmm: [i8; 2],
    hmbl: i8,
    hmove_blank: bool,
    cx: [u8; 8],
    /// Fire buttons (true = pressed) for INPT4/INPT5.
    pub fire: [bool; 2],
    latch_fire: bool,
    latched: [bool; 2],
    #[state(skip)]
    palette: [u32; 128],
    audio: [Channel; 2],
    #[state(skip)]
    pub samples: Vec<f32>,
    #[state(skip)]
    out_rate: f64,
    phase: f64,
    acc: f32,
    acc_n: u32,
}

#[derive(Default, Clone, Copy, state::Stateful)]
struct Channel {
    audc: u8,
    audf: u8,
    audv: u8,
    div: u32,
    p4: u8,
    p5: u8,
    p9: u16,
    d31: u8,
    out: u8,
}

impl Channel {
    fn clock(&mut self) {
        let max = (self.audf as u32 + 1) * if self.audc & 0x0C == 0x0C { 3 } else { 1 };
        self.div += 1;
        if self.div < max {
            return;
        }
        self.div = 0;
        let c = self.audc & 0x0F;
        if c == 0 || c == 0x0B {
            self.out = 1; // constant level
            return;
        }
        // The 5-bit polynomial (x^5 + x^3 + 1) and a divide-by-31 pulse gate many modes.
        let bit5 = (self.p5 ^ (self.p5 >> 2)) & 1;
        self.p5 = (self.p5 >> 1) | (bit5 << 4);
        self.d31 = (self.d31 + 1) % 31;
        let gate = if c & 0x02 == 0 {
            true
        } else if c & 0x01 == 0 {
            self.d31 == 0 || self.d31 == 18
        } else {
            self.p5 & 1 == 1
        };
        if !gate {
            return;
        }
        if c & 0x04 != 0 {
            self.out ^= 1; // pure tones
        } else if c & 0x08 != 0 {
            if c == 0x08 {
                let bit = (self.p9 ^ (self.p9 >> 4)) & 1;
                self.p9 = (self.p9 >> 1) | (bit << 8);
                self.out = (self.p9 & 1) as u8;
            } else {
                self.out = self.p5 & 1;
            }
        } else {
            // 4-bit polynomial (x^4 + x^3 + 1).
            let bit = (self.p4 ^ (self.p4 >> 1)) & 1;
            self.p4 = (self.p4 >> 1) | (bit << 3);
            self.out = self.p4 & 1;
        }
    }

    fn level(&self) -> u8 {
        if self.out != 0 {
            self.audv
        } else {
            0
        }
    }
}

fn hm(v: u8) -> i8 {
    (v as i8) >> 4
}

/// NTSC colours: 16 hues x 8 luminances.
fn palette() -> [u32; 128] {
    // Hue angles (degrees on an RGB colour wheel) for hues 1-15; hue 0 is grey.
    const HUE: [f32; 16] = [0.0, 50.0, 35.0, 20.0, 355.0, 320.0, 285.0, 255.0, 232.0, 212.0, 192.0, 168.0, 135.0, 100.0, 72.0, 45.0];
    let mut p = [0u32; 128];
    for (i, c) in p.iter_mut().enumerate() {
        let (h, l) = (i >> 3, i & 7);
        let v = if h == 0 { l as f32 / 7.0 } else { 0.3 + l as f32 / 7.0 * 0.7 };
        let s = if h == 0 { 0.0 } else { 0.72 - l as f32 * 0.06 };
        let hh = HUE[h] / 60.0;
        let x = 1.0 - ((hh % 2.0) - 1.0).abs();
        let (r, g, b) = match hh as u32 {
            0 => (1.0, x, 0.0),
            1 => (x, 1.0, 0.0),
            2 => (0.0, 1.0, x),
            3 => (0.0, x, 1.0),
            4 => (x, 0.0, 1.0),
            _ => (1.0, 0.0, x),
        };
        let mix = |c: f32| ((v * (1.0 - s + s * c)).clamp(0.0, 1.0) * 255.0) as u32;
        *c = mix(r) << 16 | mix(g) << 8 | mix(b);
    }
    p
}

impl Default for Tia {
    fn default() -> Self {
        Self::new()
    }
}

impl Tia {
    pub fn new() -> Tia {
        Tia {
            clock: 0,
            line: 0,
            frame: vec![0; WIDTH * HEIGHT],
            frame_done: false,
            vsync: false,
            vblank: 0,
            colup0: 0,
            colup1: 0,
            colupf: 0,
            colubk: 0,
            ctrlpf: 0,
            pf: [0; 3],
            nusiz: [0; 2],
            refp: [false; 2],
            grp_new: [0; 2],
            grp_old: [0; 2],
            vdelp: [false; 2],
            enam: [false; 2],
            enabl_new: false,
            enabl_old: false,
            vdelbl: false,
            resmp: [false; 2],
            pos_p: [0; 2],
            pos_m: [0; 2],
            pos_bl: 0,
            hmp: [0; 2],
            hmm: [0; 2],
            hmbl: 0,
            hmove_blank: false,
            cx: [0; 8],
            fire: [false; 2],
            latch_fire: false,
            latched: [false; 2],
            palette: palette(),
            audio: [Channel { p4: 0x0F, p5: 0x1F, p9: 0x1FF, ..Default::default() }; 2],
            samples: Vec::new(),
            out_rate: 48_000.0,
            phase: 0.0,
            acc: 0.0,
            acc_n: 0,
        }
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.out_rate = rate as f64;
    }

    /// Colour clocks left until the next line starts (for WSYNC).
    pub fn clocks_to_line_end(&self) -> u32 {
        CLOCKS_PER_LINE - self.clock
    }

    pub fn read(&mut self, addr: u16) -> u8 {
        match addr & 0x0F {
            r @ 0..=7 => self.cx[r as usize],
            0x0C | 0x0D => {
                let i = (addr & 1) as usize;
                let low = self.fire[i] || (self.latch_fire && self.latched[i]);
                if low {
                    0
                } else {
                    0x80
                }
            }
            _ => 0,
        }
    }

    fn pos_now(&self, delay: u32) -> u32 {
        if self.clock < HBLANK {
            3
        } else {
            (self.clock - HBLANK + delay) % 160
        }
    }

    pub fn write(&mut self, addr: u16, v: u8) {
        match addr & 0x3F {
            0x00 => {
                let on = v & 2 != 0;
                if on && !self.vsync {
                    self.frame_done = true;
                }
                if !on && self.vsync {
                    self.line = 0;
                }
                self.vsync = on;
            }
            0x01 => {
                self.vblank = v;
                self.latch_fire = v & 0x40 != 0;
                if !self.latch_fire {
                    self.latched = [false; 2];
                }
            }
            0x02 | 0x03 => {} // WSYNC / RSYNC: handled by the machine
            0x04 => self.nusiz[0] = v,
            0x05 => self.nusiz[1] = v,
            0x06 => self.colup0 = v >> 1,
            0x07 => self.colup1 = v >> 1,
            0x08 => self.colupf = v >> 1,
            0x09 => self.colubk = v >> 1,
            0x0A => self.ctrlpf = v,
            0x0B => self.refp[0] = v & 8 != 0,
            0x0C => self.refp[1] = v & 8 != 0,
            0x0D => self.pf[0] = v,
            0x0E => self.pf[1] = v,
            0x0F => self.pf[2] = v,
            0x10 => self.pos_p[0] = self.pos_now(5),
            0x11 => self.pos_p[1] = self.pos_now(5),
            0x12 => self.pos_m[0] = self.pos_now(4).saturating_sub(if self.clock < HBLANK { 1 } else { 0 }),
            0x13 => self.pos_m[1] = self.pos_now(4).saturating_sub(if self.clock < HBLANK { 1 } else { 0 }),
            0x14 => self.pos_bl = self.pos_now(4).saturating_sub(if self.clock < HBLANK { 1 } else { 0 }),
            0x15 => self.audio[0].audc = v & 0x0F,
            0x16 => self.audio[1].audc = v & 0x0F,
            0x17 => self.audio[0].audf = v & 0x1F,
            0x18 => self.audio[1].audf = v & 0x1F,
            0x19 => self.audio[0].audv = v & 0x0F,
            0x1A => self.audio[1].audv = v & 0x0F,
            0x1B => {
                self.grp_new[0] = v;
                self.grp_old[1] = self.grp_new[1];
            }
            0x1C => {
                self.grp_new[1] = v;
                self.grp_old[0] = self.grp_new[0];
                self.enabl_old = self.enabl_new;
            }
            0x1D => self.enam[0] = v & 2 != 0,
            0x1E => self.enam[1] = v & 2 != 0,
            0x1F => self.enabl_new = v & 2 != 0,
            0x20 => self.hmp[0] = hm(v),
            0x21 => self.hmp[1] = hm(v),
            0x22 => self.hmm[0] = hm(v),
            0x23 => self.hmm[1] = hm(v),
            0x24 => self.hmbl = hm(v),
            0x25 => self.vdelp[0] = v & 1 != 0,
            0x26 => self.vdelp[1] = v & 1 != 0,
            0x27 => self.vdelbl = v & 1 != 0,
            0x28 | 0x29 => {
                let i = (addr & 1) as usize;
                let on = v & 2 != 0;
                if self.resmp[i] && !on {
                    let centre = match self.nusiz[i] & 7 {
                        5 => 8,
                        7 => 16,
                        _ => 4,
                    };
                    self.pos_m[i] = (self.pos_p[i] + centre) % 160;
                }
                self.resmp[i] = on;
            }
            0x2A => {
                let mv = |p: &mut u32, m: i8| *p = ((*p as i32 - m as i32).rem_euclid(160)) as u32;
                mv(&mut self.pos_p[0], self.hmp[0]);
                mv(&mut self.pos_p[1], self.hmp[1]);
                mv(&mut self.pos_m[0], self.hmm[0]);
                mv(&mut self.pos_m[1], self.hmm[1]);
                mv(&mut self.pos_bl, self.hmbl);
                if self.clock < HBLANK {
                    self.hmove_blank = true;
                }
            }
            0x2B => {
                self.hmp = [0; 2];
                self.hmm = [0; 2];
                self.hmbl = 0;
            }
            0x2C => self.cx = [0; 8],
            _ => {}
        }
    }

    fn player_at(&self, i: usize, x: u32) -> bool {
        let g = if self.vdelp[i] { self.grp_old[i] } else { self.grp_new[i] };
        if g == 0 {
            return false;
        }
        let n = (self.nusiz[i] & 7) as usize;
        let scale = match n {
            5 => 2,
            7 => 4,
            _ => 1,
        };
        let dx = (x + 160 - self.pos_p[i]) % 160;
        for &o in COPIES[n] {
            let d = (dx + 160 - o) % 160;
            if d < 8 * scale {
                let bit = d / scale;
                let mask = if self.refp[i] { 1 << bit } else { 0x80 >> bit };
                return g & mask != 0;
            }
        }
        false
    }

    fn missile_at(&self, i: usize, x: u32) -> bool {
        if !self.enam[i] || self.resmp[i] {
            return false;
        }
        let n = (self.nusiz[i] & 7) as usize;
        let w = 1 << ((self.nusiz[i] >> 4) & 3);
        let dx = (x + 160 - self.pos_m[i]) % 160;
        COPIES[n].iter().any(|&o| (dx + 160 - o) % 160 < w)
    }

    fn ball_at(&self, x: u32) -> bool {
        let on = if self.vdelbl { self.enabl_old } else { self.enabl_new };
        on && (x + 160 - self.pos_bl) % 160 < 1 << ((self.ctrlpf >> 4) & 3)
    }

    fn pf_at(&self, x: u32) -> bool {
        let mut i = x / 4;
        if i >= 20 {
            i = if self.ctrlpf & 1 != 0 { 39 - i } else { i - 20 };
        }
        match i {
            0..=3 => self.pf[0] & (0x10 << i) != 0,
            4..=11 => self.pf[1] & (0x80 >> (i - 4)) != 0,
            _ => self.pf[2] & (1 << (i - 12)) != 0,
        }
    }

    fn pixel(&mut self, x: u32) -> u32 {
        let p0 = self.player_at(0, x);
        let p1 = self.player_at(1, x);
        let m0 = self.missile_at(0, x);
        let m1 = self.missile_at(1, x);
        let bl = self.ball_at(x);
        let pf = self.pf_at(x);
        let set = |cx: &mut [u8; 8], r: usize, b7: bool, b6: bool| cx[r] |= (b7 as u8) << 7 | (b6 as u8) << 6;
        set(&mut self.cx, 0, m0 && p1, m0 && p0);
        set(&mut self.cx, 1, m1 && p0, m1 && p1);
        set(&mut self.cx, 2, p0 && pf, p0 && bl);
        set(&mut self.cx, 3, p1 && pf, p1 && bl);
        set(&mut self.cx, 4, m0 && pf, m0 && bl);
        set(&mut self.cx, 5, m1 && pf, m1 && bl);
        set(&mut self.cx, 6, bl && pf, false);
        set(&mut self.cx, 7, p0 && p1, m0 && m1);

        let score = self.ctrlpf & 2 != 0 && self.ctrlpf & 4 == 0;
        let pf_col = if score { if x < 80 { self.colup0 } else { self.colup1 } } else { self.colupf };
        let c = if self.ctrlpf & 4 != 0 {
            if pf || bl {
                self.colupf
            } else if p0 || m0 {
                self.colup0
            } else if p1 || m1 {
                self.colup1
            } else {
                self.colubk
            }
        } else if p0 || m0 {
            self.colup0
        } else if p1 || m1 {
            self.colup1
        } else if pf {
            pf_col
        } else if bl {
            self.colupf
        } else {
            self.colubk
        };
        self.palette[c as usize & 127]
    }

    fn audio_tick(&mut self) {
        for ch in &mut self.audio {
            ch.clock();
        }
        let s = (self.audio[0].level() as f32 + self.audio[1].level() as f32) / 30.0;
        self.acc += s;
        self.acc_n += 1;
        self.phase += self.out_rate;
        while self.phase >= AUDIO_RATE {
            self.phase -= AUDIO_RATE;
            let v = if self.acc_n > 0 { self.acc / self.acc_n as f32 } else { 0.0 } * 0.6;
            self.samples.push(v);
            self.samples.push(v);
            self.acc = 0.0;
            self.acc_n = 0;
        }
    }

    /// Advances `n` colour clocks, drawing as the beam moves.
    pub fn run(&mut self, n: u32) {
        for _ in 0..n {
            if self.clock == 0 || self.clock == 114 {
                self.audio_tick();
            }
            if self.clock >= HBLANK {
                let x = self.clock - HBLANK;
                let row = self.line.wrapping_sub(TOP);
                let blank = self.vblank & 2 != 0 || self.vsync || (self.hmove_blank && x < 8);
                let c = if blank { 0 } else { self.pixel(x) };
                if (row as usize) < HEIGHT {
                    self.frame[row as usize * WIDTH + x as usize] = c;
                }
            }
            if self.latch_fire {
                for i in 0..2 {
                    self.latched[i] |= self.fire[i];
                }
            }
            self.clock += 1;
            if self.clock == CLOCKS_PER_LINE {
                self.clock = 0;
                self.line += 1;
                self.hmove_blank = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playfield_left_half_and_reflection() {
        let mut t = Tia::new();
        t.line = TOP;
        t.write(0x08, 0x0E); // COLUPF white-ish
        t.write(0x0D, 0x10); // PF0 bit 4 -> first 4 pixels
        t.write(0x0A, 0x01); // reflect
        t.run(CLOCKS_PER_LINE);
        let bg = t.frame[0 + 4];
        assert_ne!(t.frame[0], bg);
        assert_eq!(t.frame[159], t.frame[0], "reflected into the last 4 pixels");
        assert_eq!(t.frame[80], bg, "right half starts with PF2 when reflected");
    }

    #[test]
    fn player_draws_at_reset_position_and_collides() {
        let mut t = Tia::new();
        t.line = TOP;
        t.write(0x06, 0x1E);
        t.write(0x1B, 0x80); // GRP0 leftmost pixel
        t.write(0x0D, 0xF0); // playfield under the left edge
        t.write(0x08, 0x40);
        t.run(HBLANK + 20);
        t.write(0x10, 0); // RESP0 at pixel 20 (+5)
        t.run(CLOCKS_PER_LINE - HBLANK - 20);
        t.write(0x2C, 0); // CXCLR: forget the first line, drawn before RESP0
        t.run(CLOCKS_PER_LINE); // next line draws it
        let row = &t.frame[WIDTH..2 * WIDTH];
        let p = row.iter().position(|&c| c == t.palette[0x0F]).unwrap();
        assert_eq!(p, 25);
        assert_eq!(t.read(0x02) & 0x80, 0, "no P0-PF collision at x=25 (PF0 covers 0-15)");
        t.write(0x0D, 0xFF);
        t.write(0x0E, 0xFF);
        t.run(CLOCKS_PER_LINE);
        assert_eq!(t.read(0x02) & 0x80, 0x80, "P0-PF collision");
    }
}
