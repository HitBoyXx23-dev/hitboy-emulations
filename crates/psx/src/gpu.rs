//! PlayStation GPU: 1 MiB VRAM (1024x512 16-bit), GP0 drawing commands with a software
//! rasterizer (flat/Gouraud/textured polygons, lines, rectangles, semi-transparency,
//! texture windows, mask bits, dithering) and GP1 display control.

pub const VRAM_W: usize = 1024;
pub const VRAM_H: usize = 512;

#[derive(Clone, Copy, Default)]
struct Vtx {
    x: i32,
    y: i32,
    r: i32,
    g: i32,
    b: i32,
    u: i32,
    v: i32,
}

#[derive(Clone, Copy)]
struct Tex {
    page: u16,
    clut: u16,
    raw: bool,
}

enum Load {
    None,
    /// CPU -> VRAM upload in progress.
    ToVram { x: u32, y: u32, w: u32, h: u32, i: u32 },
}

#[derive(state::Stateful)]
pub struct Gpu {
    pub vram: Vec<u16>,
    cmd: Vec<u32>,
    need: usize,
    load: Load,
    read_q: std::collections::VecDeque<u32>,
    gpuread: u32,
    // Drawing state (GP0 E1-E6)
    texpage: u16,
    tex_disable: bool,
    tw: (u32, u32, u32, u32),
    da: (i32, i32, i32, i32),
    off: (i32, i32),
    set_mask: bool,
    check_mask: bool,
    // Display state (GP1)
    pub disp_x: u32,
    pub disp_y: u32,
    h_range: (u32, u32),
    v_range: (u32, u32),
    hres: u32,
    vres480: bool,
    pub pal: bool,
    depth24: bool,
    interlace: bool,
    disp_off: bool,
    dma_dir: u32,
    irq: bool,
    irq_edge: bool,
    allow_tex_disable: bool,
    /// Interlace field / odd-line flag (GPUSTAT bit 31), driven by the timing code.
    pub odd: bool,
    pub in_vblank: bool,
    polyline: bool,
    #[state(skip)]
    frame: Vec<u32>,
    #[state(skip)]
    size: (usize, usize),
}

impl state::Stateful for Load {
    fn sync(&mut self, s: &mut state::State) {
        let mut tag = matches!(self, Load::ToVram { .. }) as u8;
        tag.sync(s);
        let (mut x, mut y, mut w, mut h, mut i) = match self {
            Load::ToVram { x, y, w, h, i } => (*x, *y, *w, *h, *i),
            Load::None => (0, 0, 0, 0, 0),
        };
        x.sync(s);
        y.sync(s);
        w.sync(s);
        h.sync(s);
        i.sync(s);
        if s.is_load() {
            *self = if tag == 0 { Load::None } else { Load::ToVram { x, y, w, h, i } };
        }
    }
}

const DITHER: [[i32; 4]; 4] = [[-4, 0, -3, 1], [2, -2, 3, -1], [-3, 1, -4, 0], [3, -1, 2, -2]];

impl Default for Gpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Gpu {
    pub fn new() -> Self {
        let mut g = Gpu {
            vram: vec![0; VRAM_W * VRAM_H],
            cmd: Vec::with_capacity(16),
            need: 0,
            load: Load::None,
            read_q: Default::default(),
            gpuread: 0,
            texpage: 0,
            tex_disable: false,
            tw: (0, 0, 0, 0),
            da: (0, 0, 0, 0),
            off: (0, 0),
            set_mask: false,
            check_mask: false,
            disp_x: 0,
            disp_y: 0,
            h_range: (0x200, 0xC00),
            v_range: (0x10, 0x100),
            hres: 320,
            vres480: false,
            pal: false,
            depth24: false,
            interlace: false,
            disp_off: true,
            dma_dir: 0,
            irq: false,
            irq_edge: false,
            allow_tex_disable: false,
            odd: false,
            in_vblank: false,
            polyline: false,
            frame: vec![0; 320 * 240],
            size: (320, 240),
        };
        g.gp1(0);
        g
    }

    // ------------------------------------------------------------------ status/ports

    pub fn status(&self) -> u32 {
        let mut s = (self.texpage & 0x7FF) as u32;
        s |= (self.set_mask as u32) << 11 | (self.check_mask as u32) << 12;
        s |= (!self.interlace || self.odd) as u32 * (1 << 13);
        s |= (self.tex_disable as u32) << 15;
        let (h2, h1) = match self.hres {
            368 => (1, 0),
            256 => (0, 0),
            320 => (0, 1),
            512 => (0, 2),
            _ => (0, 3),
        };
        s |= h2 << 16 | h1 << 17 | (self.vres480 as u32) << 19 | (self.pal as u32) << 20;
        s |= (self.depth24 as u32) << 21 | (self.interlace as u32) << 22 | (self.disp_off as u32) << 23;
        s |= (self.irq as u32) << 24;
        let ready_cmd = matches!(self.load, Load::None) && self.cmd.is_empty();
        s |= (ready_cmd as u32) << 26 | 1 << 27 | 1 << 28;
        let dreq = match self.dma_dir {
            0 => 0,
            1 => 1,
            2 => 1,
            _ => (!self.read_q.is_empty()) as u32,
        };
        s |= dreq << 25 | self.dma_dir << 29;
        if self.odd && !self.in_vblank {
            s |= 1 << 31;
        }
        s
    }

    pub fn read(&mut self) -> u32 {
        if let Some(v) = self.read_q.pop_front() {
            self.gpuread = v;
        }
        self.gpuread
    }

    pub fn take_irq(&mut self) -> bool {
        std::mem::take(&mut self.irq_edge)
    }

    // ------------------------------------------------------------------ GP1

    pub fn gp1(&mut self, v: u32) {
        match v >> 24 {
            0x00 => {
                self.cmd.clear();
                self.load = Load::None;
                self.irq = false;
                self.disp_off = true;
                self.dma_dir = 0;
                self.disp_x = 0;
                self.disp_y = 0;
                self.h_range = (0x200, 0xC00);
                self.v_range = (0x10, 0x100);
                self.hres = 256;
                self.vres480 = false;
                self.pal = false;
                self.depth24 = false;
                self.interlace = false;
                self.texpage = 0;
                self.tex_disable = false;
                self.tw = (0, 0, 0, 0);
                self.da = (0, 0, 0, 0);
                self.off = (0, 0);
                self.set_mask = false;
                self.check_mask = false;
            }
            0x01 => {
                self.cmd.clear();
                self.load = Load::None;
            }
            0x02 => self.irq = false,
            0x03 => self.disp_off = v & 1 != 0,
            0x04 => self.dma_dir = v & 3,
            0x05 => {
                self.disp_x = v & 0x3FE;
                self.disp_y = (v >> 10) & 0x1FF;
            }
            0x06 => self.h_range = (v & 0xFFF, (v >> 12) & 0xFFF),
            0x07 => self.v_range = (v & 0x3FF, (v >> 10) & 0x3FF),
            0x08 => {
                self.hres = if v & 0x40 != 0 {
                    368
                } else {
                    [256, 320, 512, 640][(v & 3) as usize]
                };
                self.vres480 = v & 4 != 0;
                self.pal = v & 8 != 0;
                self.depth24 = v & 0x10 != 0;
                self.interlace = v & 0x20 != 0;
            }
            0x09 => self.allow_tex_disable = v & 1 != 0,
            0x10..=0x1F => {
                self.gpuread = match v & 7 {
                    2 => self.tw.0 | self.tw.1 << 5 | self.tw.2 << 10 | self.tw.3 << 15,
                    3 => (self.da.0 as u32 & 0x3FF) | (self.da.1 as u32 & 0x3FF) << 10,
                    4 => (self.da.2 as u32 & 0x3FF) | (self.da.3 as u32 & 0x3FF) << 10,
                    5 => (self.off.0 as u32 & 0x7FF) | (self.off.1 as u32 & 0x7FF) << 11,
                    7 => 2,
                    _ => self.gpuread,
                }
            }
            _ => {}
        }
    }

    /// GPU clocks per dot for the current horizontal resolution (timer 0 dot clock).
    pub fn dot_divider(&self) -> u32 {
        match self.hres {
            256 => 10,
            320 => 8,
            368 => 7,
            512 => 5,
            _ => 4,
        }
    }

    // ------------------------------------------------------------------ GP0

    pub fn gp0(&mut self, w: u32) {
        if let Load::ToVram { x, y, w: lw, h, i } = &mut self.load {
            for half in [w & 0xFFFF, w >> 16] {
                let px = (*x + *i % *lw) as usize & 1023;
                let py = (*y + *i / *lw) as usize & 511;
                let idx = py * VRAM_W + px;
                if !(self.check_mask && self.vram[idx] & 0x8000 != 0) {
                    self.vram[idx] = half as u16 | if self.set_mask { 0x8000 } else { 0 };
                }
                *i += 1;
                if *i >= *lw * *h {
                    self.load = Load::None;
                    break;
                }
            }
            return;
        }
        if self.cmd.is_empty() {
            let op = w >> 24;
            self.need = cmd_len(op);
            self.polyline = (0x48..=0x5F).contains(&op) && op & 8 != 0;
        }
        self.cmd.push(w);
        if self.polyline {
            let gouraud = self.cmd[0] & 0x1000_0000 != 0;
            let n = self.cmd.len();
            // A polyline ends with a 0x5xxx5xxx word (checked where a vertex or colour would be).
            if n >= 3 && (w & 0xF000_F000) == 0x5000_5000 {
                self.cmd.clear();
                return;
            }
            let seg_ready = if gouraud { n >= 4 && n % 2 == 0 } else { n >= 3 };
            if seg_ready {
                let c = &self.cmd;
                let (a, b) = if gouraud {
                    let a = vtx_c(c[n - 4] & 0xFFFFFF, c[n - 3]);
                    let b = vtx_c(c[n - 2], c[n - 1]);
                    (a, b)
                } else {
                    (vtx_c(c[0], c[n - 2]), vtx_c(c[0], c[n - 1]))
                };
                let semi = self.cmd[0] & 0x0200_0000 != 0;
                self.line(a, b, semi, gouraud);
            }
            if seg_ready && self.cmd.len() > 64 {
                // Keep only what the next segment needs.
                let n = self.cmd.len();
                let op = self.cmd[0] & 0xFF00_0000;
                self.cmd = if gouraud {
                    vec![op | (self.cmd[n - 2] & 0xFFFFFF), self.cmd[n - 1]]
                } else {
                    vec![self.cmd[0], self.cmd[n - 1]]
                };
            }
            return;
        }
        if self.cmd.len() >= self.need {
            let c = std::mem::take(&mut self.cmd);
            self.execute(&c);
            self.cmd = c;
            self.cmd.clear();
        }
    }

    fn execute(&mut self, c: &[u32]) {
        let op = c[0] >> 24;
        match op {
            0x02 => self.fill(c),
            0x1F => {
                self.irq = true;
                self.irq_edge = true;
            }
            0x20..=0x3F => self.polygon(c),
            0x40..=0x5F => {
                let gouraud = op & 0x10 != 0;
                let (a, b) = if gouraud { (vtx_c(c[0], c[1]), vtx_c(c[2], c[3])) } else { (vtx_c(c[0], c[1]), vtx_c(c[0], c[2])) };
                self.line(a, b, op & 2 != 0, gouraud);
            }
            0x60..=0x7F => self.rect(c),
            0x80..=0x9F => self.copy(c),
            0xA0..=0xBF => {
                let (x, y) = (c[1] & 0x3FF, (c[1] >> 16) & 0x1FF);
                let w = ((c[2] & 0xFFFF).wrapping_sub(1) & 0x3FF) + 1;
                let h = ((c[2] >> 16).wrapping_sub(1) & 0x1FF) + 1;
                self.load = Load::ToVram { x, y, w, h, i: 0 };
            }
            0xC0..=0xDF => {
                let (x, y) = (c[1] & 0x3FF, (c[1] >> 16) & 0x1FF);
                let w = ((c[2] & 0xFFFF).wrapping_sub(1) & 0x3FF) + 1;
                let h = ((c[2] >> 16).wrapping_sub(1) & 0x1FF) + 1;
                let mut px = Vec::with_capacity((w * h) as usize);
                for j in 0..h {
                    for i in 0..w {
                        px.push(self.vram[((y + j) as usize & 511) * VRAM_W + ((x + i) as usize & 1023)]);
                    }
                }
                self.read_q.clear();
                for pair in px.chunks(2) {
                    self.read_q.push_back(pair[0] as u32 | (*pair.get(1).unwrap_or(&0) as u32) << 16);
                }
            }
            0xE1 => {
                self.texpage = (c[0] & 0x7FF) as u16;
                self.tex_disable = self.allow_tex_disable && c[0] & 0x800 != 0;
            }
            0xE2 => {
                let v = c[0];
                self.tw = (v & 0x1F, (v >> 5) & 0x1F, (v >> 10) & 0x1F, (v >> 15) & 0x1F);
            }
            0xE3 => self.da = ((c[0] & 0x3FF) as i32, ((c[0] >> 10) & 0x1FF) as i32, self.da.2, self.da.3),
            0xE4 => self.da = (self.da.0, self.da.1, (c[0] & 0x3FF) as i32, ((c[0] >> 10) & 0x1FF) as i32),
            0xE5 => self.off = (sext11(c[0]), sext11(c[0] >> 11)),
            0xE6 => {
                self.set_mask = c[0] & 1 != 0;
                self.check_mask = c[0] & 2 != 0;
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ primitives

    fn fill(&mut self, c: &[u32]) {
        let col = rgb15(c[0]);
        let x0 = (c[1] & 0x3F0) as usize;
        let y0 = ((c[1] >> 16) & 0x1FF) as usize;
        let w = (((c[2] & 0x3FF) + 0xF) & !0xF) as usize;
        let h = ((c[2] >> 16) & 0x1FF) as usize;
        for y in 0..h {
            let row = ((y0 + y) & 511) * VRAM_W;
            for x in 0..w {
                self.vram[row + ((x0 + x) & 1023)] = col;
            }
        }
    }

    fn copy(&mut self, c: &[u32]) {
        let (sx, sy) = (c[1] & 0x3FF, (c[1] >> 16) & 0x1FF);
        let (dx, dy) = (c[2] & 0x3FF, (c[2] >> 16) & 0x1FF);
        let w = ((c[3] & 0xFFFF).wrapping_sub(1) & 0x3FF) + 1;
        let h = ((c[3] >> 16).wrapping_sub(1) & 0x1FF) + 1;
        let mut buf = Vec::with_capacity(w as usize);
        for j in 0..h {
            buf.clear();
            for i in 0..w {
                buf.push(self.vram[((sy + j) as usize & 511) * VRAM_W + ((sx + i) as usize & 1023)]);
            }
            for i in 0..w {
                let d = ((dy + j) as usize & 511) * VRAM_W + ((dx + i) as usize & 1023);
                if self.check_mask && self.vram[d] & 0x8000 != 0 {
                    continue;
                }
                self.vram[d] = buf[i as usize] | if self.set_mask { 0x8000 } else { 0 };
            }
        }
    }

    fn polygon(&mut self, c: &[u32]) {
        let op = c[0] >> 24;
        let gouraud = op & 0x10 != 0;
        let quad = op & 8 != 0;
        let textured = op & 4 != 0;
        let semi = op & 2 != 0;
        let raw = op & 1 != 0;
        let n = if quad { 4 } else { 3 };
        let mut v = [Vtx::default(); 4];
        let mut i = 1;
        let mut clut = 0;
        let mut page = self.texpage;
        for (k, vt) in v.iter_mut().enumerate().take(n) {
            let col = if k == 0 {
                c[0]
            } else if gouraud {
                i += 1;
                c[i - 1]
            } else {
                c[0]
            };
            *vt = vtx_c(col, c[i]);
            i += 1;
            if textured {
                let uv = c[i];
                vt.u = (uv & 0xFF) as i32;
                vt.v = ((uv >> 8) & 0xFF) as i32;
                if k == 0 {
                    clut = (uv >> 16) as u16;
                }
                if k == 1 {
                    page = (uv >> 16) as u16;
                }
                i += 1;
            }
        }
        let tex = if textured {
            // The polygon's texpage also updates the global drawing mode bits.
            self.texpage = (self.texpage & !0x9FF) | (page & 0x9FF);
            Some(Tex { page, clut, raw })
        } else {
            None
        };
        let semi_mode = if textured { (page >> 5) & 3 } else { (self.texpage >> 5) & 3 };
        let dither = self.texpage & 0x200 != 0 && (gouraud || (textured && !raw));
        self.triangle([v[0], v[1], v[2]], tex, semi.then_some(semi_mode), gouraud, dither);
        if quad {
            self.triangle([v[1], v[2], v[3]], tex, semi.then_some(semi_mode), gouraud, dither);
        }
    }

    fn rect(&mut self, c: &[u32]) {
        let op = c[0] >> 24;
        let textured = op & 4 != 0;
        let semi = op & 2 != 0;
        let raw = op & 1 != 0;
        let base = vtx_c(c[0], c[1]);
        let mut i = 2;
        let (u0, v0, clut) = if textured {
            let uv = c[i];
            i += 1;
            ((uv & 0xFF) as i32, ((uv >> 8) & 0xFF) as i32, (uv >> 16) as u16)
        } else {
            (0, 0, 0)
        };
        let (w, h) = match (op >> 3) & 3 {
            0 => ((c[i] & 0x3FF) as i32, ((c[i] >> 16) & 0x1FF) as i32),
            1 => (1, 1),
            2 => (8, 8),
            _ => (16, 16),
        };
        let x0 = base.x + self.off.0;
        let y0 = base.y + self.off.1;
        let tex = textured.then_some(Tex { page: self.texpage, clut, raw });
        let semi_mode = semi.then_some((self.texpage >> 5) & 3);
        for dy in 0..h {
            let y = y0 + dy;
            if y < self.da.1 || y > self.da.3 {
                continue;
            }
            for dx in 0..w {
                let x = x0 + dx;
                if x < self.da.0 || x > self.da.2 {
                    continue;
                }
                self.shade(x, y, base.r, base.g, base.b, u0 + dx, v0 + dy, tex, semi_mode, false);
            }
        }
    }

    fn line(&mut self, mut a: Vtx, mut b: Vtx, semi: bool, gouraud: bool) {
        a.x += self.off.0;
        a.y += self.off.1;
        b.x += self.off.0;
        b.y += self.off.1;
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        if dx.abs() > 1023 || dy.abs() > 511 {
            return;
        }
        let steps = dx.abs().max(dy.abs()).max(1);
        let semi_mode = semi.then_some((self.texpage >> 5) & 3);
        let dither = gouraud && self.texpage & 0x200 != 0;
        for s in 0..=steps {
            let x = a.x + dx * s / steps;
            let y = a.y + dy * s / steps;
            if x < self.da.0 || x > self.da.2 || y < self.da.1 || y > self.da.3 {
                continue;
            }
            let (r, g, bl) = if gouraud {
                (a.r + (b.r - a.r) * s / steps, a.g + (b.g - a.g) * s / steps, a.b + (b.b - a.b) * s / steps)
            } else {
                (a.r, a.g, a.b)
            };
            self.shade(x, y, r, g, bl, 0, 0, None, semi_mode, dither);
        }
    }

    fn triangle(&mut self, mut v: [Vtx; 3], tex: Option<Tex>, semi: Option<u16>, gouraud: bool, dither: bool) {
        for p in v.iter_mut() {
            p.x += self.off.0;
            p.y += self.off.1;
        }
        let minx = v.iter().map(|p| p.x).min().unwrap();
        let maxx = v.iter().map(|p| p.x).max().unwrap();
        let miny = v.iter().map(|p| p.y).min().unwrap();
        let maxy = v.iter().map(|p| p.y).max().unwrap();
        if maxx - minx > 1023 || maxy - miny > 511 {
            return;
        }
        let edge = |a: &Vtx, b: &Vtx, x: i32, y: i32| (b.x - a.x) as i64 * (y - a.y) as i64 - (b.y - a.y) as i64 * (x - a.x) as i64;
        let mut area = edge(&v[0], &v[1], v[2].x, v[2].y);
        if area == 0 {
            return;
        }
        if area < 0 {
            v.swap(1, 2);
            area = -area;
        }
        let x0 = minx.max(self.da.0);
        let x1 = maxx.min(self.da.2 + 1) - 1;
        let y0 = miny.max(self.da.1);
        let y1 = maxy.min(self.da.3 + 1) - 1;
        if x0 > x1 || y0 > y1 {
            return;
        }
        // Top-left fill rule: pixels exactly on a right/bottom edge are left out.
        let bias = |a: &Vtx, b: &Vtx| -> i64 {
            let top_left = (a.y == b.y && b.x > a.x) || b.y < a.y;
            if top_left {
                0
            } else {
                -1
            }
        };
        let e = [(1usize, 2usize), (2, 0), (0, 1)];
        let biases = e.map(|(a, b)| bias(&v[a], &v[b]));
        let attrs = |f: fn(&Vtx) -> i32| [f(&v[0]) as i64, f(&v[1]) as i64, f(&v[2]) as i64];
        let rs = attrs(|p| p.r);
        let gs = attrs(|p| p.g);
        let bs = attrs(|p| p.b);
        let us = attrs(|p| p.u);
        let vs = attrs(|p| p.v);
        let interp = |w: [i64; 3], a: [i64; 3]| ((w[0] * a[0] + w[1] * a[1] + w[2] * a[2]) * 2 + area) / (2 * area);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let w = [
                    edge(&v[1], &v[2], x, y),
                    edge(&v[2], &v[0], x, y),
                    edge(&v[0], &v[1], x, y),
                ];
                if w[0] + biases[0] < 0 || w[1] + biases[1] < 0 || w[2] + biases[2] < 0 {
                    continue;
                }
                let (r, g, b) = if gouraud {
                    (interp(w, rs) as i32, interp(w, gs) as i32, interp(w, bs) as i32)
                } else {
                    (v[0].r, v[0].g, v[0].b)
                };
                let (u, tv) = if tex.is_some() { (interp(w, us) as i32, interp(w, vs) as i32) } else { (0, 0) };
                self.shade(x, y, r, g, b, u, tv, tex, semi, dither);
            }
        }
    }

    // ------------------------------------------------------------------ pixel pipeline

    fn texel(&self, t: &Tex, u: i32, v: i32) -> u16 {
        let (mx, my, ox, oy) = self.tw;
        let u = ((u as u32 & 0xFF) & !(mx * 8)) | ((ox & mx) * 8);
        let v = ((v as u32 & 0xFF) & !(my * 8)) | ((oy & my) * 8);
        let bx = (t.page as u32 & 0xF) * 64;
        let by = ((t.page as u32 >> 4) & 1) * 256;
        let row = ((by + v) as usize & 511) * VRAM_W;
        let cx = (t.clut as u32 & 0x3F) * 16;
        let cy = (t.clut as u32 >> 6) & 0x1FF;
        match (t.page >> 7) & 3 {
            0 => {
                let w = self.vram[row + ((bx + u / 4) as usize & 1023)];
                let i = (w >> ((u & 3) * 4)) & 0xF;
                self.vram[cy as usize * VRAM_W + ((cx + i as u32) as usize & 1023)]
            }
            1 => {
                let w = self.vram[row + ((bx + u / 2) as usize & 1023)];
                let i = (w >> ((u & 1) * 8)) & 0xFF;
                self.vram[cy as usize * VRAM_W + ((cx + i as u32) as usize & 1023)]
            }
            _ => self.vram[row + ((bx + u) as usize & 1023)],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn shade(&mut self, x: i32, y: i32, r: i32, g: i32, b: i32, u: i32, v: i32, tex: Option<Tex>, semi: Option<u16>, dither: bool) {
        let idx = (y as usize & 511) * VRAM_W + (x as usize & 1023);
        let back = self.vram[idx];
        if self.check_mask && back & 0x8000 != 0 {
            return;
        }
        let d = if dither { DITHER[(y & 3) as usize][(x & 3) as usize] } else { 0 };
        let (mut col, blend) = match tex {
            Some(t) if !self.tex_disable => {
                let tx = self.texel(&t, u, v);
                if tx == 0 {
                    return;
                }
                let c = if t.raw {
                    tx
                } else {
                    let m = |sh: u32, k: i32| {
                        let t5 = ((tx >> sh) & 0x1F) as i32;
                        (((t5 * k) >> 4) + d).clamp(0, 255) >> 3
                    };
                    (m(0, r) | m(5, g) << 5 | m(10, b) << 10) as u16 | (tx & 0x8000)
                };
                (c, tx & 0x8000 != 0)
            }
            _ => {
                let q = |k: i32| ((k + d).clamp(0, 255) >> 3) as u16;
                (q(r) | q(g) << 5 | q(b) << 10, true)
            }
        };
        if let (Some(mode), true) = (semi, blend) {
            let ch = |c: u16, sh: u16| ((c >> sh) & 0x1F) as i32;
            let mix = |bk: i32, f: i32| match mode {
                0 => (bk + f) >> 1,
                1 => (bk + f).min(31),
                2 => (bk - f).max(0),
                _ => (bk + (f >> 2)).min(31),
            };
            let out = mix(ch(back, 0), ch(col, 0)) | mix(ch(back, 5), ch(col, 5)) << 5 | mix(ch(back, 10), ch(col, 10)) << 10;
            col = out as u16 | (col & 0x8000);
        }
        if self.set_mask {
            col |= 0x8000;
        }
        self.vram[idx] = col;
    }

    // ------------------------------------------------------------------ output

    /// Converts the displayed VRAM area into the 0RGB frame.
    pub fn render_frame(&mut self) {
        let w = self.hres as usize;
        let lines = self.v_range.1.saturating_sub(self.v_range.0) as usize;
        let base_h = if lines == 0 || lines > 288 { if self.pal { 256 } else { 240 } } else { lines };
        let h = if self.vres480 && self.interlace { base_h * 2 } else { base_h };
        if self.size != (w, h) {
            self.size = (w, h);
            self.frame = vec![0; w * h];
        }
        if self.disp_off {
            self.frame.fill(0);
            return;
        }
        for y in 0..h {
            let vy = (self.disp_y as usize + y) & 511;
            let row = &mut self.frame[y * w..(y + 1) * w];
            if self.depth24 {
                let base = vy * VRAM_W * 2 + self.disp_x as usize * 2;
                for (x, px) in row.iter_mut().enumerate() {
                    let byte = |o: usize| {
                        let a = (base + x * 3 + o) % (VRAM_W * 2);
                        let hw = self.vram[vy * VRAM_W + a / 2];
                        if a & 1 == 0 { hw as u32 & 0xFF } else { hw as u32 >> 8 }
                    };
                    let _ = base;
                    *px = byte(0) << 16 | byte(1) << 8 | byte(2);
                }
            } else {
                for (x, px) in row.iter_mut().enumerate() {
                    let c = self.vram[vy * VRAM_W + ((self.disp_x as usize + x) & 1023)] as u32;
                    let ex = |v: u32| (v << 3) | (v >> 2);
                    *px = ex(c & 0x1F) << 16 | ex((c >> 5) & 0x1F) << 8 | ex((c >> 10) & 0x1F);
                }
            }
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn size(&self) -> (usize, usize) {
        self.size
    }
}

fn cmd_len(op: u32) -> usize {
    match op {
        0x02 => 3,
        0x20..=0x3F => {
            let n = if op & 8 != 0 { 4 } else { 3 };
            let per = 1 + (op & 4 != 0) as usize;
            1 + n * per + if op & 0x10 != 0 { n - 1 } else { 0 }
        }
        0x40..=0x5F => {
            if op & 0x10 != 0 {
                4
            } else {
                3
            }
        }
        0x60..=0x7F => 2 + (op & 4 != 0) as usize + ((op >> 3) & 3 == 0) as usize,
        0x80..=0x9F => 4,
        0xA0..=0xDF => 3,
        _ => 1,
    }
}

fn sext11(v: u32) -> i32 {
    ((v << 21) as i32) >> 21
}

fn rgb15(c: u32) -> u16 {
    ((c & 0xF8) >> 3 | (c & 0xF800) >> 6 | (c & 0xF8_0000) >> 9) as u16
}

fn vtx_c(col: u32, xy: u32) -> Vtx {
    Vtx {
        x: sext11(xy),
        y: sext11(xy >> 16),
        r: (col & 0xFF) as i32,
        g: ((col >> 8) & 0xFF) as i32,
        b: ((col >> 16) & 0xFF) as i32,
        u: 0,
        v: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Gpu {
        let mut g = Gpu::new();
        g.gp0(0xE300_0000);
        g.gp0(0xE400_0000 | 511 << 10 | 1023);
        g
    }

    #[test]
    fn flat_triangle_fills_inside_only() {
        let mut g = setup();
        // Red triangle (0,0) (16,0) (0,16).
        g.gp0(0x2000_00FF);
        g.gp0(0);
        g.gp0(16);
        g.gp0(16 << 16);
        assert_eq!(g.vram[2 * VRAM_W + 2], 0x1F);
        assert_eq!(g.vram[15 * VRAM_W + 15], 0);
        assert_eq!(g.vram[0], 0x1F, "top-left pixel is included");
    }

    #[test]
    fn fill_and_vram_roundtrip() {
        let mut g = setup();
        g.gp0(0x0200_FF00); // green fill
        g.gp0(0);
        g.gp0(16 | 2 << 16);
        assert_eq!(g.vram[VRAM_W + 5], 0x1F << 5);
        g.gp0(0xA000_0000);
        g.gp0(100 | 50 << 16);
        g.gp0(2 | 1 << 16);
        g.gp0(0x1234_5678);
        assert_eq!(g.vram[50 * VRAM_W + 100], 0x5678);
        g.gp0(0xC000_0000);
        g.gp0(100 | 50 << 16);
        g.gp0(2 | 1 << 16);
        assert_eq!(g.read(), 0x1234_5678);
    }

    #[test]
    fn textured_rect_uses_clut() {
        let mut g = setup();
        // 4-bit texture at page (0,0): texel index 1 everywhere; CLUT at (0,256) entry 1 = blue.
        for y in 0..16 {
            g.vram[y * VRAM_W] = 0x1111;
        }
        g.vram[256 * VRAM_W + 1] = 0x7C00;
        g.gp0(0xE100_0000); // texpage 0, 4-bit
        g.gp0(0x6500_0000); // raw textured variable-size rect
        g.gp0(200 | 100 << 16);
        g.gp0((256 << 6) << 16); // clut y=256
        g.gp0(4 | 4 << 16);
        assert_eq!(g.vram[101 * VRAM_W + 201], 0x7C00);
    }

    #[test]
    fn semi_transparency_average() {
        let mut g = setup();
        g.vram[10 * VRAM_W + 10] = 0x1F; // red background
        g.gp0(0x6A00_0000 | 0xF8 << 16); // semi-transparent 1x1 blue dot... 0x6A = rect 1x1 semi
        g.gp0(10 | 10 << 16);
        let v = g.vram[10 * VRAM_W + 10];
        assert_eq!((v & 0x1F, (v >> 10) & 0x1F), (15, 15));
    }
}
