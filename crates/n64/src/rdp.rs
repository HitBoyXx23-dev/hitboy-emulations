//! Reality Display Processor: a software rasteriser for the RDP command stream: shaded,
//! textured and z-buffered triangles, fill and texture rectangles, the colour combiner,
//! the blender, and 4 KiB texture memory with all N64 texel formats.

#[derive(Clone, Copy, Default)]
struct Tile {
    fmt: u32,
    size: u32,
    line: u32,
    tmem: u32,
    palette: u32,
    ct: bool,
    mt: bool,
    mask_t: u32,
    shift_t: u32,
    cs: bool,
    ms: bool,
    mask_s: u32,
    shift_s: u32,
    sl: u32,
    tl: u32,
    sh: u32,
    th: u32,
}

#[derive(Clone, Copy, Default)]
struct Image {
    #[allow(dead_code)]
    fmt: u32,
    size: u32,
    width: u32,
    addr: u32,
}

#[derive(Clone, Copy, Default, Debug)]
struct Rgba {
    r: i32,
    g: i32,
    b: i32,
    a: i32,
}

impl Rgba {
    fn from_u32(c: u32) -> Rgba {
        Rgba { r: (c >> 24) as i32 & 255, g: (c >> 16) as i32 & 255, b: (c >> 8) as i32 & 255, a: c as i32 & 255 }
    }
}

pub struct Rdp {
    tmem: Vec<u8>,
    tiles: [Tile; 8],
    color_img: Image,
    tex_img: Image,
    z_addr: u32,
    other: u64,
    combine: u64,
    fill: u32,
    fog: Rgba,
    blend: Rgba,
    prim: Rgba,
    env: Rgba,
    prim_z: u32,
    scissor: (i32, i32, i32, i32),
    /// Leftover words of a command split across submissions.
    pending: Vec<u64>,
    pub full_sync: bool,
}

impl Default for Rdp {
    fn default() -> Self {
        Rdp {
            tmem: vec![0; 0x1000],
            tiles: [Tile::default(); 8],
            color_img: Image::default(),
            tex_img: Image::default(),
            z_addr: 0,
            other: 0,
            combine: 0,
            fill: 0,
            fog: Rgba::default(),
            blend: Rgba::default(),
            prim: Rgba::default(),
            env: Rgba::default(),
            prim_z: 0,
            scissor: (0, 0, 320, 240),
            pending: Vec::new(),
            full_sync: false,
        }
    }
}

fn cmd_words(op: u32) -> usize {
    match op {
        0x08 => 4,
        0x09 => 6,
        0x0A => 12,
        0x0B => 14,
        0x0C => 12,
        0x0D => 14,
        0x0E => 20,
        0x0F => 22,
        0x24 | 0x25 => 2,
        _ => 1,
    }
}

fn rd16(m: &[u8], a: usize) -> u16 {
    let a = a & (m.len() - 1) & !1;
    u16::from_be_bytes([m[a], m[a + 1]])
}
fn rd32(m: &[u8], a: usize) -> u32 {
    let a = a & (m.len() - 1) & !3;
    u32::from_be_bytes([m[a], m[a + 1], m[a + 2], m[a + 3]])
}
fn wr16(m: &mut [u8], a: usize, v: u16) {
    let a = a & (m.len() - 1) & !1;
    m[a..a + 2].copy_from_slice(&v.to_be_bytes());
}
fn wr32(m: &mut [u8], a: usize, v: u32) {
    let a = a & (m.len() - 1) & !3;
    m[a..a + 4].copy_from_slice(&v.to_be_bytes());
}

fn bits(w: u64, lo: u32, n: u32) -> u32 {
    ((w >> lo) & ((1u64 << n) - 1)) as u32
}

fn sext(v: u32, n: u32) -> i32 {
    ((v << (32 - n)) as i32) >> (32 - n)
}

/// Per-pixel inputs gathered while walking a primitive.
#[derive(Clone, Copy, Default)]
struct Frag {
    shade: Rgba,
    s: i32,
    t: i32,
    w: i32,
    z: i32,
}

impl Rdp {
    /// Executes the commands in `words` (64-bit, already in host order).
    pub fn run(&mut self, words: &[u64], rdram: &mut [u8]) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(words);
        let mut i = 0;
        while i < buf.len() {
            let op = ((buf[i] >> 56) & 0x3F) as u32;
            let n = cmd_words(op);
            if i + n > buf.len() {
                break;
            }
            self.command(op, &buf[i..i + n], rdram);
            i += n;
        }
        self.pending = buf[i..].to_vec();
    }

    fn command(&mut self, op: u32, w: &[u64], rdram: &mut [u8]) {
        let w0 = w[0];
        if log_enabled() {
            eprintln!("rdp {op:02X} {:016X?}", w);
        }
        match op {
            0x08..=0x0F => self.triangle(op, w, rdram),
            0x24 | 0x25 => self.tex_rect(w0, w[1], op == 0x25, rdram),
            0x29 => self.full_sync = true,
            0x2A => {}
            0x2D => {
                self.scissor = (bits(w0, 44, 12) as i32 >> 2, bits(w0, 32, 12) as i32 >> 2, bits(w0, 12, 12) as i32 >> 2, bits(w0, 0, 12) as i32 >> 2);
            }
            0x2E => self.prim_z = bits(w0, 16, 16),
            0x2F => self.other = w0,
            0x30 => self.load_tlut(w0, rdram),
            0x32 => {
                let t = &mut self.tiles[bits(w0, 24, 3) as usize];
                t.sl = bits(w0, 44, 12);
                t.tl = bits(w0, 32, 12);
                t.sh = bits(w0, 12, 12);
                t.th = bits(w0, 0, 12);
            }
            0x33 => self.load_block(w0, rdram),
            0x34 => self.load_tile(w0, rdram),
            0x35 => {
                let t = &mut self.tiles[bits(w0, 24, 3) as usize];
                t.fmt = bits(w0, 53, 3);
                t.size = bits(w0, 51, 2);
                t.line = bits(w0, 41, 9);
                t.tmem = bits(w0, 32, 9);
                t.palette = bits(w0, 20, 4);
                t.ct = w0 & (1 << 19) != 0;
                t.mt = w0 & (1 << 18) != 0;
                t.mask_t = bits(w0, 14, 4);
                t.shift_t = bits(w0, 10, 4);
                t.cs = w0 & (1 << 9) != 0;
                t.ms = w0 & (1 << 8) != 0;
                t.mask_s = bits(w0, 4, 4);
                t.shift_s = bits(w0, 0, 4);
            }
            0x36 => self.fill_rect(w0, rdram),
            0x37 => self.fill = w0 as u32,
            0x38 => self.fog = Rgba::from_u32(w0 as u32),
            0x39 => self.blend = Rgba::from_u32(w0 as u32),
            0x3A => self.prim = Rgba::from_u32(w0 as u32),
            0x3B => self.env = Rgba::from_u32(w0 as u32),
            0x3C => self.combine = w0,
            0x3D => self.tex_img = image(w0),
            0x3E => self.z_addr = bits(w0, 0, 26),
            0x3F => self.color_img = image(w0),
            _ => {}
        }
    }

    fn cycle_type(&self) -> u32 {
        bits(self.other, 52, 2)
    }

    // ---------------------------------------------------------------- texture loads

    fn load_tile(&mut self, w0: u64, rdram: &[u8]) {
        let ti = bits(w0, 24, 3) as usize;
        let (sl, tl, sh, th) = (bits(w0, 44, 12) >> 2, bits(w0, 32, 12) >> 2, bits(w0, 12, 12) >> 2, bits(w0, 0, 12) >> 2);
        {
            let t = &mut self.tiles[ti];
            t.sl = bits(w0, 44, 12);
            t.tl = bits(w0, 32, 12);
            t.sh = bits(w0, 12, 12);
            t.th = bits(w0, 0, 12);
        }
        let tile = self.tiles[ti];
        let img = self.tex_img;
        let bpp = [4u32, 8, 16, 32][img.size as usize];
        for y in tl..=th {
            for x in sl..=sh {
                let src = img.addr + ((y * img.width + x) * bpp) / 8;
                let row = y - tl;
                let xr = x - sl;
                if img.size == 3 {
                    let v = rd32(rdram, src as usize);
                    let a = (tile.tmem * 8 + row * tile.line * 8 + xr * 2) as usize;
                    let xo = if row & 1 != 0 { 4 } else { 0 };
                    wr16(&mut self.tmem, (a ^ xo) & 0x7FF, (v >> 16) as u16);
                    wr16(&mut self.tmem, ((a ^ xo) & 0x7FF) | 0x800, v as u16);
                } else {
                    let a = (tile.tmem * 8 + row * tile.line * 8 + (xr * bpp) / 8) as usize;
                    let xo = if row & 1 != 0 { 4 } else { 0 };
                    match img.size {
                        2 => {
                            let v = rd16(rdram, src as usize);
                            wr16(&mut self.tmem, (a ^ xo) & 0xFFF, v);
                        }
                        1 => {
                            self.tmem[(a ^ xo) & 0xFFF] = rdram[src as usize & (rdram.len() - 1)];
                        }
                        _ => {
                            if xr & 1 == 0 {
                                self.tmem[(a ^ xo) & 0xFFF] = rdram[src as usize & (rdram.len() - 1)];
                            }
                        }
                    }
                }
            }
        }
    }

    fn load_block(&mut self, w0: u64, rdram: &[u8]) {
        let ti = bits(w0, 24, 3) as usize;
        let sl = bits(w0, 44, 12);
        let tl = bits(w0, 32, 12);
        let sh = bits(w0, 12, 12);
        let dxt = bits(w0, 0, 12);
        let tile = self.tiles[ti];
        let img = self.tex_img;
        let bpp = [4u32, 8, 16, 32][img.size as usize];
        let texels = sh.saturating_sub(sl) + 1;
        let bytes = (texels * bpp).div_ceil(8);
        let src = img.addr + ((tl * img.width + sl) * bpp) / 8;
        let base = tile.tmem * 8;
        let words = bytes.div_ceil(8);
        let mut t_acc: u32 = 0;
        for wi in 0..words {
            let odd = (t_acc >> 11) & 1 != 0;
            for b in 0..8u32 {
                let s = (src + wi * 8 + b) as usize & (rdram.len() - 1);
                let mut d = base + wi * 8 + b;
                if odd {
                    d ^= if img.size == 3 { 8 } else { 4 };
                }
                if img.size == 3 {
                    // 32-bit texels: red/green halves in low TMEM, blue/alpha in high.
                    let within = (wi * 8 + b) % 4;
                    let texel = (wi * 8 + b) / 4;
                    let hi_half = within < 2;
                    let a = ((base + texel * 2 + (within & 1)) ^ if odd { 4 } else { 0 }) as usize & 0x7FF;
                    self.tmem[if hi_half { a } else { a | 0x800 }] = rdram[s];
                } else {
                    self.tmem[d as usize & 0xFFF] = rdram[s];
                }
            }
            t_acc = t_acc.wrapping_add(dxt);
        }
        let t = &mut self.tiles[ti];
        t.sl = sl << 2;
        t.tl = tl << 2;
        t.sh = sh << 2;
        t.th = tl << 2;
    }

    fn load_tlut(&mut self, w0: u64, rdram: &[u8]) {
        let ti = bits(w0, 24, 3) as usize;
        let (sl, sh) = (bits(w0, 44, 12) >> 2, bits(w0, 12, 12) >> 2);
        let tile = self.tiles[ti];
        let img = self.tex_img;
        for i in sl..=sh {
            let v = rd16(rdram, (img.addr + i * 2) as usize);
            let a = (tile.tmem * 8 + (i - sl) * 8) as usize;
            for k in 0..4 {
                wr16(&mut self.tmem, (a + k * 2) & 0xFFF, v);
            }
        }
    }

    // ---------------------------------------------------------------- texture sampling

    fn wrap(coord: i32, mask: u32, clamp: bool, mirror: bool, lo: i32, hi: i32) -> i32 {
        let mut c = coord;
        let clamp = clamp || mask == 0;
        if clamp {
            c = c.clamp(0, (hi - lo).max(0));
        }
        if mask > 0 {
            let size = 1i32 << mask;
            if mirror && (c >> mask) & 1 != 0 {
                c = !c;
            }
            c &= size - 1;
        }
        c
    }

    fn texel(&self, ti: usize, si: i32, tj: i32) -> Rgba {
        let t = &self.tiles[ti];
        let s = Self::wrap(si, t.mask_s, t.cs, t.ms, (t.sl >> 2) as i32, (t.sh >> 2) as i32) as u32;
        let tt = Self::wrap(tj, t.mask_t, t.ct, t.mt, (t.tl >> 2) as i32, (t.th >> 2) as i32) as u32;
        let row = (t.tmem * 8 + tt * t.line * 8) as usize;
        let xo = if tt & 1 != 0 { 4 } else { 0 };
        let tm = &self.tmem;
        let tlut_on = self.other & (1 << 47) != 0;
        let tlut_ia = self.other & (1 << 46) != 0;
        let from16 = |v: u16, ia: bool| -> Rgba {
            if ia {
                let i = (v >> 8) as i32;
                Rgba { r: i, g: i, b: i, a: (v & 0xFF) as i32 }
            } else {
                let ex = |c: u16| ((c << 3) | (c >> 2)) as i32;
                Rgba { r: ex((v >> 11) & 31), g: ex((v >> 6) & 31), b: ex((v >> 1) & 31), a: if v & 1 != 0 { 255 } else { 0 } }
            }
        };
        let tlut = |idx: u32| -> Rgba { from16(rd16(tm, 0x800 + (idx as usize & 0xFF) * 8), tlut_ia) };
        match (t.size, t.fmt) {
            (3, _) => {
                let a = ((row + s as usize * 2) ^ xo) & 0x7FF;
                let hi = rd16(tm, a);
                let lo = rd16(tm, a | 0x800);
                Rgba { r: (hi >> 8) as i32, g: (hi & 0xFF) as i32, b: (lo >> 8) as i32, a: (lo & 0xFF) as i32 }
            }
            (2, fmt) => {
                let v = rd16(tm, (row + s as usize * 2) ^ xo);
                if fmt == 3 {
                    from16(v, true)
                } else if tlut_on && fmt == 2 {
                    tlut(v as u32 >> 8)
                } else {
                    from16(v, false)
                }
            }
            (1, fmt) => {
                let v = tm[((row + s as usize) ^ xo) & 0xFFF] as u32;
                match fmt {
                    2 => tlut(v),
                    3 => {
                        let i = ((v >> 4) * 17) as i32;
                        Rgba { r: i, g: i, b: i, a: ((v & 15) * 17) as i32 }
                    }
                    _ if tlut_on => tlut(v),
                    _ => {
                        let i = v as i32;
                        Rgba { r: i, g: i, b: i, a: i }
                    }
                }
            }
            (_, fmt) => {
                let b = tm[((row + s as usize / 2) ^ xo) & 0xFFF] as u32;
                let n = if s & 1 == 0 { b >> 4 } else { b & 15 };
                match fmt {
                    2 => tlut((t.palette << 4) | n),
                    3 => {
                        let i = ((n >> 1) * 36) as i32;
                        Rgba { r: i, g: i, b: i, a: if n & 1 != 0 { 255 } else { 0 } }
                    }
                    _ if tlut_on => tlut((t.palette << 4) | n),
                    _ => {
                        let i = (n * 17) as i32;
                        Rgba { r: i, g: i, b: i, a: i }
                    }
                }
            }
        }
    }

    /// Samples tile `ti` at s,t in 10.5 fixed point (bilinear when enabled).
    fn sample(&self, ti: usize, s: i32, t: i32) -> Rgba {
        let tile = &self.tiles[ti];
        let shift = |v: i32, sh: u32| -> i32 {
            match sh {
                0 => v,
                1..=10 => v >> sh,
                _ => v << (16 - sh),
            }
        };
        let s = shift(s, tile.shift_s) - ((tile.sl as i32) << 3);
        let t = shift(t, tile.shift_t) - ((tile.tl as i32) << 3);
        let bilinear = self.other & (1 << 45) != 0 && self.cycle_type() < 2;
        if !bilinear {
            return self.texel(ti, s >> 5, t >> 5);
        }
        let (s0, t0) = ((s - 16) >> 5, (t - 16) >> 5);
        let (fs, ft) = ((s - 16) & 31, (t - 16) & 31);
        let a = self.texel(ti, s0, t0);
        let b = self.texel(ti, s0 + 1, t0);
        let c = self.texel(ti, s0, t0 + 1);
        let d = self.texel(ti, s0 + 1, t0 + 1);
        let mix = |p: i32, q: i32, r: i32, u: i32| {
            let top = p * (32 - fs) + q * fs;
            let bot = r * (32 - fs) + u * fs;
            (top * (32 - ft) + bot * ft) >> 10
        };
        Rgba { r: mix(a.r, b.r, c.r, d.r), g: mix(a.g, b.g, c.g, d.g), b: mix(a.b, b.b, c.b, d.b), a: mix(a.a, b.a, c.a, d.a) }
    }

    // ---------------------------------------------------------------- combiner and blender

    fn combine(&self, cycle: u32, comb: Rgba, t0: Rgba, t1: Rgba, shade: Rgba) -> Rgba {
        let c = self.combine;
        let (sa_r, mul_r, sa_a, mul_a, sb_r, sb_a, add_r, add_a) = if cycle == 0 {
            (bits(c, 52, 4), bits(c, 47, 5), bits(c, 44, 3), bits(c, 41, 3), bits(c, 28, 4), bits(c, 12, 3), bits(c, 15, 3), bits(c, 9, 3))
        } else {
            (bits(c, 37, 4), bits(c, 32, 5), bits(c, 21, 3), bits(c, 18, 3), bits(c, 24, 4), bits(c, 3, 3), bits(c, 6, 3), bits(c, 0, 3))
        };
        let one = Rgba { r: 256, g: 256, b: 256, a: 256 };
        let zero = Rgba::default();
        let pick = |n: u32| match n {
            0 => comb,
            1 => t0,
            2 => t1,
            3 => self.prim,
            4 => shade,
            5 => self.env,
            _ => zero,
        };
        let a_rgb = if sa_r == 6 { one } else { pick(sa_r) };
        let b_rgb = pick(sb_r);
        let splat = |v: i32| Rgba { r: v, g: v, b: v, a: v };
        let m_rgb = match mul_r {
            0..=5 => pick(mul_r),
            7 => splat(comb.a),
            8 => splat(t0.a),
            9 => splat(t1.a),
            10 => splat(self.prim.a),
            11 => splat(shade.a),
            12 => splat(self.env.a),
            13 | 14 => splat(if self.other & (1 << 48) != 0 { 0 } else { 255 }),
            _ => zero,
        };
        let d_rgb = if add_r == 6 { one } else { pick(add_r) };
        let pa = |n: u32| match n {
            0 => comb.a,
            1 => t0.a,
            2 => t1.a,
            3 => self.prim.a,
            4 => shade.a,
            5 => self.env.a,
            6 => 256,
            _ => 0,
        };
        // LOD fraction: without texture LOD there is no mip blend, so it acts as full weight.
        let lod_frac = if self.other & (1 << 48) != 0 { 0 } else { 255 };
        let ma = match mul_a {
            0 => lod_frac,
            1..=5 => pa(mul_a),
            6 => lod_frac,
            _ => 0,
        };
        let f = |a: i32, b: i32, m: i32, d: i32| (((a - b) * m + (d << 8) + 0x80) >> 8).clamp(0, 255);
        Rgba {
            r: f(a_rgb.r, b_rgb.r, m_rgb.r, d_rgb.r),
            g: f(a_rgb.g, b_rgb.g, m_rgb.g, d_rgb.g),
            b: f(a_rgb.b, b_rgb.b, m_rgb.b, d_rgb.b),
            a: f(pa(sa_a), pa(sb_a), ma, pa(add_a)),
        }
    }

    fn read_pixel(&self, rdram: &[u8], x: i32, y: i32) -> Rgba {
        let img = &self.color_img;
        let idx = (y as u32 * img.width + x as u32) as usize;
        if img.size == 3 {
            Rgba::from_u32(rd32(rdram, img.addr as usize + idx * 4))
        } else {
            let v = rd16(rdram, img.addr as usize + idx * 2);
            let ex = |c: u16| ((c << 3) | (c >> 2)) as i32;
            Rgba { r: ex((v >> 11) & 31), g: ex((v >> 6) & 31), b: ex((v >> 1) & 31), a: if v & 1 != 0 { 255 } else { 0 } }
        }
    }

    fn write_pixel(&self, rdram: &mut [u8], x: i32, y: i32, c: Rgba) {
        let img = &self.color_img;
        let idx = (y as u32 * img.width + x as u32) as usize;
        if img.size == 3 {
            let v = (c.r as u32) << 24 | (c.g as u32) << 16 | (c.b as u32) << 8 | c.a as u32;
            wr32(rdram, img.addr as usize + idx * 4, v);
        } else {
            let v = ((c.r as u16 >> 3) << 11) | ((c.g as u16 >> 3) << 6) | ((c.b as u16 >> 3) << 1) | (c.a > 0) as u16;
            wr16(rdram, img.addr as usize + idx * 2, v);
        }
    }

    fn blend(&self, rdram: &[u8], x: i32, y: i32, pix: Rgba, shade_a: i32) -> Rgba {
        let o = self.other;
        let force = o & (1 << 14) != 0;
        let (p, a, m, b) = (bits(o, 30, 2), bits(o, 26, 2), bits(o, 22, 2), bits(o, 18, 2));
        let mem = self.read_pixel(rdram, x, y);
        let col = |n: u32| match n {
            0 => pix,
            1 => mem,
            2 => self.blend,
            _ => self.fog,
        };
        let av = match a {
            0 => pix.a,
            1 => self.fog.a,
            2 => shade_a,
            _ => 0,
        };
        let bv = match b {
            0 => 255 - av,
            1 => 255,
            2 => 255,
            _ => 0,
        };
        let (pc, mc) = (col(p), col(m));
        if !force {
            // Fully covered pixel without force-blend: the first blender input passes through.
            return Rgba { a: pix.a, ..pc };
        }
        let sum = (av + bv).max(1);
        Rgba {
            r: ((pc.r * av + mc.r * bv) / sum).clamp(0, 255),
            g: ((pc.g * av + mc.g * bv) / sum).clamp(0, 255),
            b: ((pc.b * av + mc.b * bv) / sum).clamp(0, 255),
            a: pix.a,
        }
    }

    /// Shades one pixel through the whole pipeline.
    fn pixel(&self, rdram: &mut [u8], x: i32, y: i32, f: Frag, tile: usize, textured: bool) {
        let (sx0, sy0, sx1, sy1) = self.scissor;
        if x < sx0 || x >= sx1 || y < sy0 || y >= sy1 || x < 0 || y < 0 || x as u32 >= self.color_img.width {
            return;
        }
        if self.cycle_type() == 3 {
            // Fill mode: the fill register supplies the pixels directly.
            let img = &self.color_img;
            let idx = (y as u32 * img.width + x as u32) as usize;
            if img.size == 3 {
                wr32(rdram, img.addr as usize + idx * 4, self.fill);
            } else {
                let v = if x & 1 == 0 { (self.fill >> 16) as u16 } else { self.fill as u16 };
                wr16(rdram, img.addr as usize + idx * 2, v);
            }
            return;
        }
        // Depth test.
        let z_on = self.other & (1 << 4) != 0;
        let z_upd = self.other & (1 << 5) != 0;
        let z = if self.other & (1 << 2) != 0 { self.prim_z as i32 } else { (f.z >> 16).clamp(0, 0x7FFF) << 1 };
        let za = (self.z_addr + (y as u32 * self.color_img.width + x as u32) * 2) as usize;
        if z_on && z > rd16(rdram, za) as i32 {
            return;
        }
        let (t0, t1) = if textured {
            let (mut s, mut t) = (f.s, f.t);
            if self.other & (1 << 51) != 0 && f.w > 0 {
                // Perspective correction: divide by the interpolated 1/w (0x7FFF.0000 ~ 1.0).
                s = (((f.s as i64) << 31) / f.w as i64) as i32;
                t = (((f.t as i64) << 31) / f.w as i64) as i32;
            }
            let (s5, t5) = (s >> 16, t >> 16);
            let a = self.sample(tile, s5, t5);
            let b = if self.cycle_type() == 1 { self.sample((tile + 1) & 7, s5, t5) } else { a };
            (a, b)
        } else {
            (Rgba::default(), Rgba::default())
        };
        let shade = Rgba { r: (f.shade.r >> 16).clamp(0, 255), g: (f.shade.g >> 16).clamp(0, 255), b: (f.shade.b >> 16).clamp(0, 255), a: (f.shade.a >> 16).clamp(0, 255) };
        // One-cycle mode uses the second-cycle combiner settings; two-cycle runs both.
        let c = if self.cycle_type() == 1 {
            let first = self.combine(0, Rgba::default(), t0, t1, shade);
            self.combine(1, first, t0, t1, shade)
        } else {
            self.combine(1, Rgba::default(), t0, t1, shade)
        };
        // Alpha compare against the blend colour's alpha.
        if self.other & 1 != 0 && c.a < self.blend.a.max(1) {
            return;
        }
        let out = self.blend(rdram, x, y, c, shade.a);
        self.write_pixel(rdram, x, y, out);
        if z_on && z_upd || (!z_on && z_upd) {
            wr16(rdram, za, z as u16);
        }
    }

    // ---------------------------------------------------------------- primitives

    fn fill_rect(&mut self, w0: u64, rdram: &mut [u8]) {
        let (xl, yl, xh, yh) = (bits(w0, 44, 12) as i32, bits(w0, 32, 12) as i32, bits(w0, 12, 12) as i32, bits(w0, 0, 12) as i32);
        let ct = self.cycle_type();
        let (x0, y0) = (xh >> 2, yh >> 2);
        let (x1, y1) = if ct >= 2 { ((xl >> 2) + 1, (yl >> 2) + 1) } else { ((xl + 3) >> 2, (yl + 3) >> 2) };
        let (sx0, sy0, sx1, sy1) = self.scissor;
        let img = self.color_img;
        for y in y0.max(sy0)..y1.min(sy1) {
            for x in x0.max(sx0)..x1.min(sx1) {
                if x < 0 || y < 0 || x as u32 >= img.width {
                    continue;
                }
                if ct == 3 {
                    let idx = (y as u32 * img.width + x as u32) as usize;
                    if img.size == 3 {
                        wr32(rdram, img.addr as usize + idx * 4, self.fill);
                    } else {
                        let v = if x & 1 == 0 { (self.fill >> 16) as u16 } else { self.fill as u16 };
                        wr16(rdram, img.addr as usize + idx * 2, v);
                    }
                } else {
                    let f = Frag { shade: Rgba::default(), z: (self.prim_z as i32) << 16, ..Default::default() };
                    self.pixel(rdram, x, y, f, 0, false);
                }
            }
        }
    }

    fn tex_rect(&mut self, w0: u64, w1: u64, flip: bool, rdram: &mut [u8]) {
        let (xl, yl, xh, yh) = (bits(w0, 44, 12) as i32, bits(w0, 32, 12) as i32, bits(w0, 12, 12) as i32, bits(w0, 0, 12) as i32);
        let tile = bits(w0, 24, 3) as usize;
        let s0 = sext(bits(w1, 48, 16), 16);
        let t0 = sext(bits(w1, 32, 16), 16);
        let mut dsdx = sext(bits(w1, 16, 16), 16);
        let dtdy = sext(bits(w1, 0, 16), 16);
        let ct = self.cycle_type();
        if ct == 2 {
            dsdx /= 4; // copy mode steps four texels per clock
        }
        let (x0, y0) = (xh >> 2, yh >> 2);
        let (x1, y1) = if ct >= 2 { ((xl >> 2) + 1, (yl >> 2) + 1) } else { ((xl + 3) >> 2, (yl + 3) >> 2) };
        for y in y0..y1 {
            for x in x0..x1 {
                // s,t are 10.5; the steps are 5.10.
                let dx = x - x0;
                let dy = y - y0;
                let (mut s, mut t) = (s0 + ((dsdx * dx) >> 5), t0 + ((dtdy * dy) >> 5));
                if flip {
                    s = s0 + ((dsdx * dy) >> 5);
                    t = t0 + ((dtdy * dx) >> 5);
                }
                if ct == 2 {
                    let (sx0, sy0, sx1, sy1) = self.scissor;
                    if x < sx0 || x >= sx1 || y < sy0 || y >= sy1 || x < 0 || y < 0 || x as u32 >= self.color_img.width {
                        continue;
                    }
                    let c = self.sample(tile, s, t);
                    if self.other & 1 != 0 && c.a == 0 {
                        continue;
                    }
                    self.write_pixel(rdram, x, y, c);
                } else {
                    let f = Frag { s: s << 16, t: t << 16, w: 0, z: (self.prim_z as i32) << 16, shade: Rgba::default() };
                    let saved = self.other;
                    self.other &= !(1u64 << 51);
                    self.pixel(rdram, x, y, f, tile, true);
                    self.other = saved;
                }
            }
        }
    }

    fn triangle(&mut self, op: u32, w: &[u64], rdram: &mut [u8]) {
        let shade = op & 4 != 0;
        let tex = op & 2 != 0;
        let zbuf = op & 1 != 0;
        let w0 = w[0];
        let lft = w0 & (1 << 55) != 0;
        let tile = bits(w0, 48, 3) as usize;
        let yl = sext(bits(w0, 32, 14), 14);
        let ym = sext(bits(w0, 16, 14), 14);
        let yh = sext(bits(w0, 0, 14), 14);
        let e = |i: usize| (w[i] >> 32) as u32 as i32;
        let d = |i: usize| w[i] as u32 as i32;
        let (xl, dxldy) = (e(1), d(1));
        let (xh, dxhdy) = (e(2), d(2));
        let (xm, dxmdy) = (e(3), d(3));
        let mut idx = 4;
        // Attribute sets: value, d/dx, d/de (along the major edge).
        let mut sh = [[0i32; 3]; 4];
        if shade {
            let c = &w[idx..idx + 8];
            for k in 0..4 {
                let hi = |word: u64| ((word >> (48 - 16 * k)) & 0xFFFF) as u32;
                let fix = |i: usize, f: usize| ((hi(c[i]) << 16) | hi(c[f])) as i32;
                sh[k] = [fix(0, 2), fix(1, 3), fix(4, 6)];
            }
            idx += 8;
        }
        let mut st = [[0i32; 3]; 3];
        if tex {
            let c = &w[idx..idx + 8];
            for k in 0..3 {
                let hi = |word: u64| ((word >> (48 - 16 * k)) & 0xFFFF) as u32;
                let fix = |i: usize, f: usize| ((hi(c[i]) << 16) | hi(c[f])) as i32;
                st[k] = [fix(0, 2), fix(1, 3), fix(4, 6)];
            }
            idx += 8;
        }
        let mut zz = [0i32; 3];
        if zbuf {
            let (a, b) = (w[idx], w[idx + 1]);
            zz = [(a >> 32) as u32 as i32, a as u32 as i32, (b >> 32) as u32 as i32];
        }
        let y_start = yh >> 2;
        let y_end = (yl + 3) >> 2;
        let (sx0, sy0, sx1, sy1) = self.scissor;
        for y in y_start.max(sy0)..y_end.min(sy1) {
            let dy = y - y_start;
            let x_major = xh.wrapping_add(dxhdy.wrapping_mul(dy));
            let x_minor = if y < (ym >> 2) { xm.wrapping_add(dxmdy.wrapping_mul(dy)) } else { xl.wrapping_add(dxldy.wrapping_mul(y - (ym >> 2))) };
            let (left, right) = if lft { (x_major, x_minor) } else { (x_minor, x_major) };
            let xa = ((left as i64 + 0xFFFF) >> 16) as i32;
            let xb = (right as i64 >> 16) as i32;
            if xb < xa && (right - left) < 0 {
                continue;
            }
            // Attributes at the major edge on this line.
            let at = |a: [i32; 3]| a[0].wrapping_add(a[2].wrapping_mul(dy));
            for x in xa.max(sx0)..=xb.min(sx1 - 1) {
                let fx = ((x as i64) << 16) - x_major as i64;
                let step = |a: [i32; 3]| at(a).wrapping_add(((a[1] as i64 * fx) >> 16) as i32);
                let f = Frag {
                    shade: if shade {
                        Rgba { r: step(sh[0]), g: step(sh[1]), b: step(sh[2]), a: step(sh[3]) }
                    } else {
                        Rgba { r: self.prim.r << 16, g: self.prim.g << 16, b: self.prim.b << 16, a: self.prim.a << 16 }
                    },
                    s: if tex { step(st[0]) } else { 0 },
                    t: if tex { step(st[1]) } else { 0 },
                    w: if tex { step(st[2]) } else { 0 },
                    z: if zbuf { zz[0].wrapping_add(zz[2].wrapping_mul(dy)).wrapping_add(((zz[1] as i64 * fx) >> 16) as i32) } else { 0 },
                };
                self.pixel(rdram, x, y, f, tile, tex);
            }
        }
    }
}

/// Set N64_RDP_LOG=1 to print every RDP command (debugging aid).
fn log_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("N64_RDP_LOG").is_some())
}

fn image(w0: u64) -> Image {
    Image { fmt: bits(w0, 53, 3), size: bits(w0, 51, 2), width: bits(w0, 32, 10) + 1, addr: bits(w0, 0, 26) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_rectangle_in_fill_mode() {
        let mut ram = vec![0u8; 0x100000];
        let mut r = Rdp::default();
        let cmds = [
            0x3F10_013F_0001_0000u64, // colour image: RGBA16, width 320, at 0x10000
            0x2D00_0000_0050_03C0,     // scissor 0,0 - 320,240
            0x2F30_0000_0000_0000,     // fill mode
            0x3700_0000_F801_F801,     // fill colour red
            0x36u64 << 56 | (39u64 << 2) << 44 | (9u64 << 2) << 32, // fill 0,0 - 39,9
            0x2900_0000_0000_0000,
        ];
        r.run(&cmds, &mut ram);
        assert!(r.full_sync);
        assert_eq!(rd16(&ram, 0x10000), 0xF801);
        assert_eq!(rd16(&ram, 0x10000 + (9 * 320 + 39) * 2), 0xF801);
        assert_eq!(rd16(&ram, 0x10000 + (10 * 320) * 2), 0);
    }
}
