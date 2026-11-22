//! Geometry Transformation Engine (COP2): the PlayStation's fixed-point 3D math unit.
//! Register semantics and command math follow the documented hardware behaviour,
//! including its saturation flags and Newton-Raphson division.

/// Reciprocal seed table for the division, generated from its defining formula.
fn unr_table() -> &'static [u8; 257] {
    use std::sync::OnceLock;
    static T: OnceLock<[u8; 257]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u8; 257];
        for (i, e) in t.iter_mut().enumerate() {
            let v = ((0x40000 / (i as i32 + 0x100) + 1) / 2 - 0x101).max(0);
            *e = v as u8;
        }
        t
    })
}

#[derive(Default, state::Stateful)]
pub struct Gte {
    v: [[i16; 3]; 3],
    rgbc: [u8; 4],
    otz: u16,
    ir: [i16; 4],
    sxy: [(i16, i16); 3],
    sz: [u16; 4],
    rgb: [[u8; 4]; 3],
    res1: u32,
    mac: [i32; 4],
    lzcs: u32,
    rt: [[i16; 3]; 3],
    tr: [i32; 3],
    llm: [[i16; 3]; 3],
    bk: [i32; 3],
    lcm: [[i16; 3]; 3],
    fc: [i32; 3],
    ofx: i32,
    ofy: i32,
    h: u16,
    dqa: i16,
    dqb: i32,
    zsf3: i16,
    zsf4: i16,
    flag: u32,
}

fn lo(v: u32) -> i16 {
    v as i16
}
fn hi(v: u32) -> i16 {
    (v >> 16) as i16
}
fn pack(a: i16, b: i16) -> u32 {
    (a as u16 as u32) | (b as u16 as u32) << 16
}

impl Gte {
    // ------------------------------------------------------------ registers

    pub fn read(&self, reg: u32) -> u32 {
        match reg {
            0 | 2 | 4 => pack(self.v[reg as usize / 2][0], self.v[reg as usize / 2][1]),
            1 | 3 | 5 => self.v[reg as usize / 2][2] as i32 as u32,
            6 => u32::from_le_bytes(self.rgbc),
            7 => self.otz as u32,
            8..=11 => self.ir[reg as usize - 8] as i32 as u32,
            12..=14 => pack(self.sxy[reg as usize - 12].0, self.sxy[reg as usize - 12].1),
            15 => pack(self.sxy[2].0, self.sxy[2].1),
            16..=19 => self.sz[reg as usize - 16] as u32,
            20..=22 => u32::from_le_bytes(self.rgb[reg as usize - 20]),
            23 => self.res1,
            24..=27 => self.mac[reg as usize - 24] as u32,
            28 | 29 => {
                let c = |i: usize| ((self.ir[i] as i32 >> 7).clamp(0, 0x1F)) as u32;
                c(1) | c(2) << 5 | c(3) << 10
            }
            30 => self.lzcs,
            31 => {
                let v = self.lzcs;
                if v as i32 >= 0 { v.leading_zeros() } else { (!v).leading_zeros() }
            }
            // control registers
            32..=36 => {
                let m = &self.rt;
                self.ctrl_matrix_read(m, reg - 32)
            }
            37..=39 => self.tr[(reg - 37) as usize] as u32,
            40..=44 => {
                let m = &self.llm;
                self.ctrl_matrix_read(m, reg - 40)
            }
            45..=47 => self.bk[(reg - 45) as usize] as u32,
            48..=52 => {
                let m = &self.lcm;
                self.ctrl_matrix_read(m, reg - 48)
            }
            53..=55 => self.fc[(reg - 53) as usize] as u32,
            56 => self.ofx as u32,
            57 => self.ofy as u32,
            58 => self.h as i16 as i32 as u32, // hardware quirk: H reads sign-extended
            59 => self.dqa as i32 as u32,
            60 => self.dqb as u32,
            61 => self.zsf3 as i32 as u32,
            62 => self.zsf4 as i32 as u32,
            63 => {
                let err = self.flag & 0x7F87_E000 != 0;
                self.flag | (err as u32) << 31
            }
            _ => 0,
        }
    }

    fn ctrl_matrix_read(&self, m: &[[i16; 3]; 3], i: u32) -> u32 {
        match i {
            0 => pack(m[0][0], m[0][1]),
            1 => pack(m[0][2], m[1][0]),
            2 => pack(m[1][1], m[1][2]),
            3 => pack(m[2][0], m[2][1]),
            _ => m[2][2] as i32 as u32,
        }
    }

    fn ctrl_matrix_write(m: &mut [[i16; 3]; 3], i: u32, v: u32) {
        match i {
            0 => (m[0][0], m[0][1]) = (lo(v), hi(v)),
            1 => (m[0][2], m[1][0]) = (lo(v), hi(v)),
            2 => (m[1][1], m[1][2]) = (lo(v), hi(v)),
            3 => (m[2][0], m[2][1]) = (lo(v), hi(v)),
            _ => m[2][2] = lo(v),
        }
    }

    pub fn write(&mut self, reg: u32, v: u32) {
        match reg {
            0 | 2 | 4 => {
                let i = reg as usize / 2;
                self.v[i][0] = lo(v);
                self.v[i][1] = hi(v);
            }
            1 | 3 | 5 => self.v[reg as usize / 2][2] = lo(v),
            6 => self.rgbc = v.to_le_bytes(),
            7 => self.otz = v as u16,
            8..=11 => self.ir[reg as usize - 8] = lo(v),
            12..=14 => self.sxy[reg as usize - 12] = (lo(v), hi(v)),
            15 => self.push_sxy(lo(v), hi(v)),
            16..=19 => self.sz[reg as usize - 16] = v as u16,
            20..=22 => self.rgb[reg as usize - 20] = v.to_le_bytes(),
            23 => self.res1 = v,
            24..=27 => self.mac[reg as usize - 24] = v as i32,
            28 => {
                self.ir[1] = ((v & 0x1F) * 0x80) as i16;
                self.ir[2] = (((v >> 5) & 0x1F) * 0x80) as i16;
                self.ir[3] = (((v >> 10) & 0x1F) * 0x80) as i16;
            }
            30 => self.lzcs = v,
            29 | 31 => {}
            32..=36 => Self::ctrl_matrix_write(&mut self.rt, reg - 32, v),
            37..=39 => self.tr[(reg - 37) as usize] = v as i32,
            40..=44 => Self::ctrl_matrix_write(&mut self.llm, reg - 40, v),
            45..=47 => self.bk[(reg - 45) as usize] = v as i32,
            48..=52 => Self::ctrl_matrix_write(&mut self.lcm, reg - 48, v),
            53..=55 => self.fc[(reg - 53) as usize] = v as i32,
            56 => self.ofx = v as i32,
            57 => self.ofy = v as i32,
            58 => self.h = v as u16,
            59 => self.dqa = v as i16,
            60 => self.dqb = v as i32,
            61 => self.zsf3 = v as i16,
            62 => self.zsf4 = v as i16,
            63 => self.flag = v & 0x7FFF_F000,
            _ => {}
        }
    }

    // ------------------------------------------------------------ saturation helpers

    /// 44-bit accumulator overflow check, then shift into MAC1-3.
    fn set_mac(&mut self, i: usize, v: i64, shift: u32) -> i64 {
        if v > 0x7FF_FFFF_FFFF {
            self.flag |= 1 << (31 - i);
        } else if v < -0x800_0000_0000 {
            self.flag |= 1 << (28 - i);
        }
        let r = v >> shift;
        self.mac[i] = r as i32;
        r
    }

    fn set_mac0(&mut self, v: i64) -> i64 {
        if v > 0x7FFF_FFFF {
            self.flag |= 1 << 16;
        } else if v < -0x8000_0000 {
            self.flag |= 1 << 15;
        }
        self.mac[0] = v as i32;
        v
    }

    fn set_ir(&mut self, i: usize, v: i64, lm: bool) {
        let min = if lm { 0 } else { -0x8000 };
        let c = v.clamp(min, 0x7FFF);
        if c != v {
            self.flag |= 1 << (25 - i);
        }
        self.ir[i] = c as i16;
    }

    fn set_ir0(&mut self, v: i64) {
        let c = v.clamp(0, 0x1000);
        if c != v {
            self.flag |= 1 << 12;
        }
        self.ir[0] = c as i16;
    }

    fn push_sz(&mut self, v: i64) {
        let c = v.clamp(0, 0xFFFF);
        if c != v {
            self.flag |= 1 << 18;
        }
        self.sz = [self.sz[1], self.sz[2], self.sz[3], c as u16];
    }

    fn push_sxy(&mut self, x: i16, y: i16) {
        self.sxy = [self.sxy[1], self.sxy[2], (x, y)];
    }

    fn push_color(&mut self) {
        let mut c = [0u8; 4];
        for (i, flag_bit) in [(1usize, 21), (2, 20), (3, 19)] {
            let v = self.mac[i] >> 4;
            let cl = v.clamp(0, 255);
            if cl != v {
                self.flag |= 1 << flag_bit;
            }
            c[i - 1] = cl as u8;
        }
        c[3] = self.rgbc[3];
        self.rgb = [self.rgb[1], self.rgb[2], c];
    }

    /// Unsigned Newton-Raphson division H / SZ3, as the hardware computes it.
    fn divide(&mut self, h: u32, sz3: u32) -> u32 {
        if h < sz3 * 2 {
            let z = (sz3 as u16).leading_zeros();
            let n = (h as u64) << z;
            let d = (sz3 << z) as u64;
            let u = unr_table()[((d - 0x7FC0) >> 7) as usize] as u64 + 0x101;
            let d2 = (0x200_0080u64.wrapping_sub(d * u)) >> 8;
            let d3 = (0x80 + d2 * u) >> 8;
            (((n * d3) + 0x8000) >> 16).min(0x1FFFF) as u32
        } else {
            self.flag |= 1 << 17;
            0x1FFFF
        }
    }

    // ------------------------------------------------------------ commands

    pub fn command(&mut self, cmd: u32) {
        self.flag = 0;
        let sf = if cmd & (1 << 19) != 0 { 12 } else { 0 };
        let lm = cmd & (1 << 10) != 0;
        match cmd & 0x3F {
            0x01 => self.rtp(0, sf, lm, true),
            0x30 => {
                self.rtp(0, sf, lm, false);
                self.rtp(1, sf, lm, false);
                self.rtp(2, sf, lm, true);
            }
            0x06 => {
                let [(x0, y0), (x1, y1), (x2, y2)] = self.sxy.map(|(x, y)| (x as i64, y as i64));
                self.set_mac0(x0 * y1 + x1 * y2 + x2 * y0 - x0 * y2 - x1 * y0 - x2 * y1);
            }
            0x0C => {
                let (d1, d2, d3) = (self.rt[0][0] as i64, self.rt[1][1] as i64, self.rt[2][2] as i64);
                let [_, i1, i2, i3] = self.ir.map(|v| v as i64);
                let a = self.set_mac(1, i3 * d2 - i2 * d3, sf);
                let b = self.set_mac(2, i1 * d3 - i3 * d1, sf);
                let c = self.set_mac(3, i2 * d1 - i1 * d2, sf);
                self.set_ir(1, a, lm);
                self.set_ir(2, b, lm);
                self.set_ir(3, c, lm);
            }
            0x10 => {
                let c = self.rgbc;
                let mac = [(c[0] as i64) << 16, (c[1] as i64) << 16, (c[2] as i64) << 16];
                self.depth_cue(mac, sf, lm);
            }
            0x2A => {
                for _ in 0..3 {
                    let c = self.rgb[0];
                    let mac = [(c[0] as i64) << 16, (c[1] as i64) << 16, (c[2] as i64) << 16];
                    self.depth_cue(mac, sf, lm);
                }
            }
            0x11 => {
                let mac = [1usize, 2, 3].map(|i| (self.ir[i] as i64) << 12);
                self.depth_cue(mac, sf, lm);
            }
            0x29 => {
                let c = self.rgbc;
                let mac = [1usize, 2, 3].map(|i| ((c[i - 1] as i64) * self.ir[i] as i64) << 4);
                self.depth_cue(mac, sf, lm);
            }
            0x12 => self.mvmva(cmd, sf, lm),
            0x13 => self.nc(0, sf, lm, Tail::Depth),
            0x16 => (0..3).for_each(|i| self.nc(i, sf, lm, Tail::Depth)),
            0x1B => self.nc(0, sf, lm, Tail::Color),
            0x3F => (0..3).for_each(|i| self.nc(i, sf, lm, Tail::Color)),
            0x1E => self.nc(0, sf, lm, Tail::Plain),
            0x20 => (0..3).for_each(|i| self.nc(i, sf, lm, Tail::Plain)),
            0x1C => {
                self.light_color(sf, lm);
                let c = self.rgbc;
                let m = [1usize, 2, 3].map(|i| ((c[i - 1] as i64) * self.ir[i] as i64) << 4);
                for i in 0..3 {
                    let v = self.set_mac(i + 1, m[i], sf);
                    self.set_ir(i + 1, v, lm);
                }
                self.push_color();
            }
            0x14 => {
                self.light_color(sf, lm);
                let c = self.rgbc;
                let m = [1usize, 2, 3].map(|i| ((c[i - 1] as i64) * self.ir[i] as i64) << 4);
                self.depth_cue(m, sf, lm);
            }
            0x28 => {
                for i in 1..4 {
                    let v = self.ir[i] as i64;
                    let m = self.set_mac(i, v * v, sf);
                    self.set_ir(i, m, lm);
                }
            }
            0x2D => {
                let s = self.sz[1] as i64 + self.sz[2] as i64 + self.sz[3] as i64;
                let m = self.set_mac0(self.zsf3 as i64 * s);
                self.set_otz(m >> 12);
            }
            0x2E => {
                let s = self.sz.iter().map(|&z| z as i64).sum::<i64>();
                let m = self.set_mac0(self.zsf4 as i64 * s);
                self.set_otz(m >> 12);
            }
            0x3D => {
                let ir0 = self.ir[0] as i64;
                for i in 1..4 {
                    let m = self.set_mac(i, ir0 * self.ir[i] as i64, sf);
                    self.set_ir(i, m, lm);
                }
                self.push_color();
            }
            0x3E => {
                let ir0 = self.ir[0] as i64;
                for i in 1..4 {
                    let base = (self.mac[i] as i64) << sf;
                    let m = self.set_mac(i, base + ir0 * self.ir[i] as i64, sf);
                    self.set_ir(i, m, lm);
                }
                self.push_color();
            }
            _ => {}
        }
    }

    fn set_otz(&mut self, v: i64) {
        let c = v.clamp(0, 0xFFFF);
        if c != v {
            self.flag |= 1 << 18;
        }
        self.otz = c as u16;
    }

    /// Rotate-translate-perspective for vertex `n`.
    fn rtp(&mut self, n: usize, sf: u32, lm: bool, last: bool) {
        let v = self.v[n].map(|x| x as i64);
        let mut z_full = 0;
        for i in 0..3 {
            let r = &self.rt[i];
            let sum = ((self.tr[i] as i64) << 12) + r[0] as i64 * v[0] + r[1] as i64 * v[1] + r[2] as i64 * v[2];
            let m = self.set_mac(i + 1, sum, sf);
            self.set_ir(i + 1, m, lm);
            if i == 2 {
                z_full = sum >> 12;
            }
        }
        self.push_sz(z_full);
        let div = self.divide(self.h as u32, self.sz[3] as u32) as i64;
        let sx = self.set_mac0(div * self.ir[1] as i64 + self.ofx as i64) >> 16;
        let sy = self.set_mac0(div * self.ir[2] as i64 + self.ofy as i64) >> 16;
        let cx = sx.clamp(-0x400, 0x3FF);
        let cy = sy.clamp(-0x400, 0x3FF);
        if cx != sx {
            self.flag |= 1 << 14;
        }
        if cy != sy {
            self.flag |= 1 << 13;
        }
        self.push_sxy(cx as i16, cy as i16);
        if last {
            let m = self.set_mac0(div * self.dqa as i64 + self.dqb as i64);
            self.set_ir0(m >> 12);
        }
    }

    fn mvmva(&mut self, cmd: u32, sf: u32, lm: bool) {
        let mx = (cmd >> 17) & 3;
        let vx = (cmd >> 15) & 3;
        let tx = (cmd >> 13) & 3;
        let m = match mx {
            0 => self.rt,
            1 => self.llm,
            2 => self.lcm,
            _ => [[0; 3]; 3],
        };
        let v = if vx == 3 { [self.ir[1], self.ir[2], self.ir[3]] } else { self.v[vx as usize] }.map(|x| x as i64);
        let t = match tx {
            0 => self.tr,
            1 => self.bk,
            2 => self.fc,
            _ => [0; 3],
        };
        for i in 0..3 {
            let sum = ((t[i] as i64) << 12) + m[i][0] as i64 * v[0] + m[i][1] as i64 * v[1] + m[i][2] as i64 * v[2];
            let r = self.set_mac(i + 1, sum, sf);
            self.set_ir(i + 1, r, lm);
        }
    }

    /// Normal -> light intensities -> light colour (first two stages of every NC* command).
    fn light_normal(&mut self, n: usize, sf: u32, lm: bool) {
        let v = self.v[n].map(|x| x as i64);
        for i in 0..3 {
            let l = &self.llm[i];
            let s = l[0] as i64 * v[0] + l[1] as i64 * v[1] + l[2] as i64 * v[2];
            let m = self.set_mac(i + 1, s, sf);
            self.set_ir(i + 1, m, lm);
        }
        self.light_color(sf, lm);
    }

    fn light_color(&mut self, sf: u32, lm: bool) {
        let ir = [self.ir[1], self.ir[2], self.ir[3]].map(|x| x as i64);
        for i in 0..3 {
            let c = &self.lcm[i];
            let s = ((self.bk[i] as i64) << 12) + c[0] as i64 * ir[0] + c[1] as i64 * ir[1] + c[2] as i64 * ir[2];
            let m = self.set_mac(i + 1, s, sf);
            self.set_ir(i + 1, m, lm);
        }
    }

    fn nc(&mut self, n: usize, sf: u32, lm: bool, tail: Tail) {
        self.light_normal(n, sf, lm);
        match tail {
            Tail::Plain => {
                for i in 1..4 {
                    self.mac[i] = self.ir[i] as i32;
                }
                self.push_color();
            }
            Tail::Color => {
                let c = self.rgbc;
                let m = [1usize, 2, 3].map(|i| ((c[i - 1] as i64) * self.ir[i] as i64) << 4);
                for i in 0..3 {
                    let v = self.set_mac(i + 1, m[i], sf);
                    self.set_ir(i + 1, v, lm);
                }
                self.push_color();
            }
            Tail::Depth => {
                let c = self.rgbc;
                let m = [1usize, 2, 3].map(|i| ((c[i - 1] as i64) * self.ir[i] as i64) << 4);
                self.depth_cue(m, sf, lm);
            }
        }
    }

    /// Interpolates MAC1-3 (full precision) toward the far colour FC by IR0, then pushes a colour.
    fn depth_cue(&mut self, mac: [i64; 3], sf: u32, lm: bool) {
        for i in 0..3 {
            let diff = ((self.fc[i] as i64) << 12) - mac[i];
            let d = self.set_mac(i + 1, diff, sf);
            self.set_ir(i + 1, d, false);
        }
        let ir0 = self.ir[0] as i64;
        for i in 0..3 {
            let v = self.set_mac(i + 1, self.ir[i + 1] as i64 * ir0 + mac[i], sf);
            self.set_ir(i + 1, v, lm);
        }
        self.push_color();
    }
}

#[derive(Clone, Copy)]
enum Tail {
    Plain,
    Color,
    Depth,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(g: &mut Gte) {
        g.write(32, 0x0000_1000); // RT11=1.0, RT12=0
        g.write(33, 0);
        g.write(34, 0x0000_1000); // RT22=1.0
        g.write(35, 0);
        g.write(36, 0x1000); // RT33=1.0
    }

    #[test]
    fn rtps_projects_a_point() {
        let mut g = Gte::default();
        identity(&mut g);
        g.write(58, 1000); // H: projection plane distance
        g.write(0, pack(100, 50)); // V0 = (100, 50, 1000)
        g.write(1, 1000);
        g.command(0x0008_0001); // RTPS, sf=1
        let sxy = g.read(14);
        assert_eq!((lo(sxy), hi(sxy)), (100, 50), "x' = x*H/z");
        assert_eq!(g.read(19), 1000, "SZ3");
        assert_eq!(g.read(63) & 0x7FFF_F000, 0, "no saturation flags");
    }

    #[test]
    fn perspective_halves_at_double_depth() {
        let mut g = Gte::default();
        identity(&mut g);
        g.write(58, 500);
        g.write(56, 160 << 16); // OFX = 160 (screen centre)
        g.write(57, 120 << 16); // OFY = 120
        g.write(0, pack(200, -100));
        g.write(1, 1000);
        g.command(0x0008_0001);
        let sxy = g.read(14);
        assert_eq!((lo(sxy), hi(sxy)), (160 + 100, 120 - 50));
    }

    #[test]
    fn nclip_and_avsz3() {
        let mut g = Gte::default();
        // Counter-clockwise triangle (0,0) (10,0) (0,10): positive winding area.
        g.write(12, pack(0, 0));
        g.write(13, pack(10, 0));
        g.write(14, pack(0, 10));
        g.command(0x06);
        assert_eq!(g.read(24) as i32, 100);
        g.write(17, 300);
        g.write(18, 600);
        g.write(19, 900);
        g.write(61, 0x1000 / 3); // ZSF3 = 1/3
        g.command(0x2D);
        assert!((598..=600).contains(&g.read(7)), "OTZ = average depth, got {}", g.read(7));
    }

    #[test]
    fn saturation_sets_flags() {
        let mut g = Gte::default();
        identity(&mut g);
        g.write(58, 1000);
        g.write(0, pack(30000, 0));
        g.write(1, 10); // very close: projection overflows the screen range
        g.command(0x0008_0001);
        let f = g.read(63);
        assert!(f & (1 << 14) != 0, "SX2 saturated");
        assert!(f & (1 << 31) != 0, "error summary bit");
        assert_eq!(lo(g.read(14)), 0x3FF);
    }

    #[test]
    fn division_table_matches_formula_endpoints() {
        let t = unr_table();
        assert_eq!(t[0], 0xFF);
        assert_eq!(t[256], 0x00);
    }
}
