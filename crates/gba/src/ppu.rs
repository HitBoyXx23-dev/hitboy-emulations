//! GBA picture processor: tile modes 0-2 (text and affine backgrounds), bitmap modes 3-5,
//! 128 sprites (regular and affine), windows (0, 1, sprite window) and colour effects.

pub const WIDTH: usize = 240;
pub const HEIGHT: usize = 160;

const OBJ_SIZES: [[(i32, i32); 4]; 3] = [
    [(8, 8), (16, 16), (32, 32), (64, 64)],
    [(16, 8), (32, 8), (32, 16), (64, 32)],
    [(8, 16), (8, 32), (16, 32), (32, 64)],
];

/// A drawn pixel: 15-bit colour plus the "opaque" flag in bit 15.
const OPAQUE: u16 = 0x8000;

#[derive(Clone, Copy, Default)]
struct ObjPx {
    color: u16,
    prio: u8,
    semi: bool,
}

#[derive(state::Stateful)]
pub struct Ppu {
    pub vram: Vec<u8>,
    pub pal: Vec<u8>,
    pub oam: Vec<u8>,
    /// Internal affine reference points for BG2/BG3 (20.8 fixed point).
    pub ref_x: [i32; 2],
    pub ref_y: [i32; 2],
    #[state(skip)]
    frame: Vec<u32>,
    // Per-scanline work buffers: rebuilt for every line, never carried across a frame.
    #[state(skip)]
    bg: [[u16; WIDTH]; 4],
    #[state(skip)]
    obj: [ObjPx; WIDTH],
    #[state(skip)]
    obj_win: [bool; WIDTH],
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new()
    }
}

fn r16(io: &[u8], a: usize) -> u16 {
    u16::from_le_bytes([io[a], io[a + 1]])
}

fn to_rgb(c: u16) -> u32 {
    let ex = |v: u32| (v << 3) | (v >> 2);
    let c = c as u32;
    ex(c & 31) << 16 | ex((c >> 5) & 31) << 8 | ex((c >> 10) & 31)
}

impl Ppu {
    pub fn new() -> Self {
        Ppu {
            vram: vec![0; 0x18000],
            pal: vec![0; 0x400],
            oam: vec![0; 0x400],
            ref_x: [0; 2],
            ref_y: [0; 2],
            frame: vec![0; WIDTH * HEIGHT],
            bg: [[0; WIDTH]; 4],
            obj: [ObjPx::default(); WIDTH],
            obj_win: [false; WIDTH],
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    fn pal16(&self, i: usize) -> u16 {
        u16::from_le_bytes([self.pal[i * 2], self.pal[i * 2 + 1]]) & 0x7FFF
    }

    /// Reloads the affine reference points from their registers (at VBlank or on write).
    pub fn latch_ref(&mut self, io: &[u8], n: usize) {
        let base = 0x28 + n * 0x10;
        let x = i32::from_le_bytes([io[base], io[base + 1], io[base + 2], io[base + 3]]);
        let y = i32::from_le_bytes([io[base + 4], io[base + 5], io[base + 6], io[base + 7]]);
        self.ref_x[n] = (x << 4) >> 4;
        self.ref_y[n] = (y << 4) >> 4;
    }

    pub fn render_line(&mut self, y: usize, io: &[u8]) {
        let dispcnt = r16(io, 0);
        let row = &mut self.frame[y * WIDTH..(y + 1) * WIDTH];
        if dispcnt & 0x80 != 0 {
            row.fill(0xFFFFFF);
            return;
        }
        let mode = dispcnt & 7;
        for l in self.bg.iter_mut() {
            l.fill(0);
        }
        let on = |n: u16| dispcnt & (0x100 << n) != 0;
        match mode {
            0 => {
                for n in 0..4 {
                    if on(n) {
                        self.text_bg(n as usize, y, io);
                    }
                }
            }
            1 => {
                for n in 0..2 {
                    if on(n) {
                        self.text_bg(n as usize, y, io);
                    }
                }
                if on(2) {
                    self.affine_bg(2, io);
                }
            }
            2 => {
                for n in 2..4 {
                    if on(n) {
                        self.affine_bg(n as usize, io);
                    }
                }
            }
            3..=5 => {
                if on(2) {
                    self.bitmap_bg(mode, dispcnt, io);
                }
            }
            _ => {}
        }
        // Affine reference points advance by PB/PD once per line.
        for n in 0..2 {
            let base = 0x20 + n * 0x10;
            self.ref_x[n] += r16(io, base + 2) as i16 as i32;
            self.ref_y[n] += r16(io, base + 6) as i16 as i32;
        }
        self.obj.iter_mut().for_each(|p| *p = ObjPx::default());
        self.obj_win.fill(false);
        if dispcnt & 0x1000 != 0 {
            self.sprites(y as i32, dispcnt);
        }
        self.compose(y, dispcnt, io);
    }

    fn text_bg(&mut self, n: usize, y: usize, io: &[u8]) {
        let cnt = r16(io, 8 + n * 2) as usize;
        let hofs = r16(io, 0x10 + n * 4) as usize & 0x1FF;
        let vofs = r16(io, 0x12 + n * 4) as usize & 0x1FF;
        let char_base = ((cnt >> 2) & 3) * 0x4000;
        let screen_base = ((cnt >> 8) & 0x1F) * 0x800;
        let bpp8 = cnt & 0x80 != 0;
        let size = (cnt >> 14) & 3;
        let w = if size & 1 != 0 { 512 } else { 256 };
        let h = if size & 2 != 0 { 512 } else { 256 };
        let yy = (y + vofs) & (h - 1);
        let vram = &self.vram;
        let line = &mut self.bg[n];
        for (x, out) in line.iter_mut().enumerate() {
            let xx = (x + hofs) & (w - 1);
            let block = (xx / 256) + (yy / 256) * (w / 256);
            let ma = screen_base + block * 0x800 + ((yy & 255) / 8 * 32 + (xx & 255) / 8) * 2;
            if ma + 1 >= vram.len() {
                continue;
            }
            let e = u16::from_le_bytes([vram[ma], vram[ma + 1]]) as usize;
            let tile = e & 0x3FF;
            let px = if e & 0x400 != 0 { 7 - (xx & 7) } else { xx & 7 };
            let py = if e & 0x800 != 0 { 7 - (yy & 7) } else { yy & 7 };
            let idx = if bpp8 {
                let a = char_base + tile * 64 + py * 8 + px;
                if a >= 0x10000 {
                    continue;
                }
                vram[a] as usize
            } else {
                let a = char_base + tile * 32 + py * 4 + px / 2;
                if a >= 0x10000 {
                    continue;
                }
                let b = vram[a] as usize;
                let i = if px & 1 != 0 { b >> 4 } else { b & 0xF };
                if i == 0 {
                    continue;
                }
                (e >> 12) * 16 + i
            };
            if idx != 0 {
                *out = u16::from_le_bytes([self.pal[idx * 2], self.pal[idx * 2 + 1]]) & 0x7FFF | OPAQUE;
            }
        }
    }

    fn affine_bg(&mut self, n: usize, io: &[u8]) {
        let cnt = r16(io, 8 + n * 2) as usize;
        let k = n - 2;
        let pa = r16(io, 0x20 + k * 0x10) as i16 as i32;
        let pc = r16(io, 0x24 + k * 0x10) as i16 as i32;
        let char_base = ((cnt >> 2) & 3) * 0x4000;
        let screen_base = ((cnt >> 8) & 0x1F) * 0x800;
        let size = 128i32 << ((cnt >> 14) & 3);
        let wrap = cnt & 0x2000 != 0;
        let (rx, ry) = (self.ref_x[k], self.ref_y[k]);
        for x in 0..WIDTH {
            let mut tx = (rx + pa * x as i32) >> 8;
            let mut ty = (ry + pc * x as i32) >> 8;
            if wrap {
                tx &= size - 1;
                ty &= size - 1;
            } else if tx < 0 || ty < 0 || tx >= size || ty >= size {
                continue;
            }
            let ma = screen_base + (ty as usize / 8) * (size as usize / 8) + tx as usize / 8;
            let tile = *self.vram.get(ma).unwrap_or(&0) as usize;
            let a = char_base + tile * 64 + (ty as usize & 7) * 8 + (tx as usize & 7);
            let idx = *self.vram.get(a).unwrap_or(&0) as usize;
            if idx != 0 {
                self.bg[n][x] = self.pal16(idx) | OPAQUE;
            }
        }
    }

    fn bitmap_bg(&mut self, mode: u16, dispcnt: u16, io: &[u8]) {
        let pa = r16(io, 0x20) as i16 as i32;
        let pc = r16(io, 0x24) as i16 as i32;
        let (rx, ry) = (self.ref_x[0], self.ref_y[0]);
        let page = if dispcnt & 0x10 != 0 { 0xA000 } else { 0 };
        let (w, h) = if mode == 5 { (160, 128) } else { (240, 160) };
        for x in 0..WIDTH {
            let tx = (rx + pa * x as i32) >> 8;
            let ty = (ry + pc * x as i32) >> 8;
            if tx < 0 || ty < 0 || tx >= w || ty >= h {
                continue;
            }
            let i = (ty * w + tx) as usize;
            let c = match mode {
                3 => u16::from_le_bytes([self.vram[i * 2], self.vram[i * 2 + 1]]) & 0x7FFF,
                4 => {
                    let idx = self.vram[page + i] as usize;
                    if idx == 0 {
                        continue;
                    }
                    self.pal16(idx)
                }
                _ => u16::from_le_bytes([self.vram[page + i * 2], self.vram[page + i * 2 + 1]]) & 0x7FFF,
            };
            self.bg[2][x] = c | OPAQUE;
        }
    }

    fn sprites(&mut self, line: i32, dispcnt: u16) {
        let one_d = dispcnt & 0x40 != 0;
        let bitmap_mode = (dispcnt & 7) >= 3;
        for i in 0..128 {
            let o = i * 8;
            let a0 = u16::from_le_bytes([self.oam[o], self.oam[o + 1]]);
            let a1 = u16::from_le_bytes([self.oam[o + 2], self.oam[o + 3]]);
            let a2 = u16::from_le_bytes([self.oam[o + 4], self.oam[o + 5]]);
            let affine = a0 & 0x100 != 0;
            if !affine && a0 & 0x200 != 0 {
                continue;
            }
            let shape = (a0 >> 14) as usize;
            if shape == 3 {
                continue;
            }
            let (w, h) = OBJ_SIZES[shape][(a1 >> 14) as usize];
            let double = affine && a0 & 0x200 != 0;
            let (bw, bh) = if double { (w * 2, h * 2) } else { (w, h) };
            let mut y = (a0 & 0xFF) as i32;
            if y >= 160 {
                y -= 256;
            }
            if line < y || line >= y + bh {
                continue;
            }
            let x = ((a1 & 0x1FF) as i32) << 23 >> 23;
            let mode = (a0 >> 10) & 3;
            let bpp8 = a0 & 0x2000 != 0;
            let tile = (a2 & 0x3FF) as i32;
            let prio = ((a2 >> 10) & 3) as u8;
            let palbank = (a2 >> 12) as usize;
            let (pa, pb, pc, pd) = if affine {
                let g = ((a1 >> 9) & 0x1F) as usize * 32;
                let p = |k: usize| i16::from_le_bytes([self.oam[g + k], self.oam[g + k + 1]]) as i32;
                (p(6), p(14), p(22), p(30))
            } else {
                (0, 0, 0, 0)
            };
            let dy = line - y;
            for sx in 0..bw {
                let px = x + sx;
                if !(0..WIDTH as i32).contains(&px) {
                    continue;
                }
                let (u, v) = if affine {
                    let cx = sx - bw / 2;
                    let cy = dy - bh / 2;
                    (((pa * cx + pb * cy) >> 8) + w / 2, ((pc * cx + pd * cy) >> 8) + h / 2)
                } else {
                    let u = if a1 & 0x1000 != 0 { w - 1 - sx } else { sx };
                    let v = if a1 & 0x2000 != 0 { h - 1 - dy } else { dy };
                    (u, v)
                };
                if u < 0 || v < 0 || u >= w || v >= h {
                    continue;
                }
                let step = if bpp8 { 2 } else { 1 };
                let row_tiles = if one_d { (w / 8) * step } else { 32 };
                let tnum = ((tile + (v / 8) * row_tiles + (u / 8) * step) & 0x3FF) as usize;
                if bitmap_mode && tnum < 512 {
                    continue;
                }
                let base = 0x10000 + tnum * 32;
                let idx = if bpp8 {
                    self.vram[(base + (v as usize & 7) * 8 + (u as usize & 7)).min(0x17FFF)] as usize
                } else {
                    let b = self.vram[(base + (v as usize & 7) * 4 + (u as usize & 7) / 2).min(0x17FFF)];
                    let n = if u & 1 != 0 { b >> 4 } else { b & 0xF } as usize;
                    if n == 0 {
                        0
                    } else {
                        palbank * 16 + n
                    }
                };
                if idx == 0 {
                    continue;
                }
                let p = px as usize;
                if mode == 2 {
                    self.obj_win[p] = true;
                    continue;
                }
                let color = self.pal16(256 + idx) | OPAQUE;
                let cur = &mut self.obj[p];
                if cur.color & OPAQUE == 0 || prio < cur.prio {
                    *cur = ObjPx { color, prio, semi: mode == 1 };
                }
            }
        }
    }

    fn compose(&mut self, y: usize, dispcnt: u16, io: &[u8]) {
        let bldcnt = r16(io, 0x50);
        let bldalpha = r16(io, 0x52);
        let eva = (bldalpha & 0x1F).min(16) as u32;
        let evb = ((bldalpha >> 8) & 0x1F).min(16) as u32;
        let evy = (r16(io, 0x54) & 0x1F).min(16) as u32;
        let effect = (bldcnt >> 6) & 3;
        let winin = r16(io, 0x48);
        let winout = r16(io, 0x4A);
        let any_win = dispcnt & 0xE000 != 0;
        let inside = |h: u16, v: u16, x: usize| {
            let (x1, x2) = ((h >> 8) as usize, (h & 0xFF) as usize);
            let (y1, y2) = ((v >> 8) as usize, (v & 0xFF) as usize);
            let inx = if x1 <= x2 { x >= x1 && x < x2 } else { x >= x1 || x < x2 };
            let iny = if y1 <= y2 { y >= y1 && y < y2 } else { y >= y1 || y < y2 };
            inx && iny
        };
        let backdrop = self.pal16(0);
        let prios: [u8; 4] = std::array::from_fn(|n| (r16(io, 8 + n * 2) & 3) as u8);
        for x in 0..WIDTH {
            let mask = if !any_win {
                0x3F
            } else if dispcnt & 0x2000 != 0 && inside(r16(io, 0x40), r16(io, 0x44), x) {
                winin & 0x3F
            } else if dispcnt & 0x4000 != 0 && inside(r16(io, 0x42), r16(io, 0x46), x) {
                (winin >> 8) & 0x3F
            } else if dispcnt & 0x8000 != 0 && self.obj_win[x] {
                (winout >> 8) & 0x3F
            } else {
                winout & 0x3F
            };
            // Top two layers: (priority, layer id, colour). OBJ wins ties; lower BG number wins ties.
            let mut top = (4u8, 5u8, backdrop);
            let mut second = (4u8, 5u8, backdrop);
            let mut consider = |p: u8, id: u8, c: u16| {
                let better = |a: (u8, u8), b: (u8, u8)| a.0 < b.0 || (a.0 == b.0 && a.1 < b.1);
                let key = (p, if id == 4 { 0 } else { id + 1 });
                let tkey = (top.0, if top.1 == 4 { 0 } else { top.1 + 1 });
                let skey = (second.0, if second.1 == 4 { 0 } else { second.1 + 1 });
                if better(key, tkey) {
                    second = top;
                    top = (p, id, c);
                } else if better(key, skey) {
                    second = (p, id, c);
                }
            };
            for n in 0..4 {
                let c = self.bg[n][x];
                if c & OPAQUE != 0 && mask & (1 << n) != 0 {
                    consider(prios[n], n as u8, c & 0x7FFF);
                }
            }
            let o = self.obj[x];
            if o.color & OPAQUE != 0 && mask & 0x10 != 0 {
                consider(o.prio, 4, o.color & 0x7FFF);
            }
            let first_t = |id: u8| bldcnt & (1 << id) != 0;
            let second_t = |id: u8| bldcnt & (0x100 << id) != 0;
            let semi_obj = top.1 == 4 && o.semi && o.color & OPAQUE != 0;
            let fx = mask & 0x20 != 0 || !any_win;
            let ch = |c: u16, s: u32| ((c >> s) & 31) as u32;
            let color = if fx && semi_obj && second_t(second.1) {
                blend(top.2, second.2, eva, evb)
            } else if fx && effect == 1 && first_t(top.1) && second_t(second.1) {
                blend(top.2, second.2, eva, evb)
            } else if fx && effect == 2 && first_t(top.1) {
                let f = |s| {
                    let c = ch(top.2, s);
                    c + ((31 - c) * evy >> 4)
                };
                (f(0) | f(5) << 5 | f(10) << 10) as u16
            } else if fx && effect == 3 && first_t(top.1) {
                let f = |s| {
                    let c = ch(top.2, s);
                    c - (c * evy >> 4)
                };
                (f(0) | f(5) << 5 | f(10) << 10) as u16
            } else {
                top.2
            };
            self.frame[y * WIDTH + x] = to_rgb(color);
        }
    }
}

fn blend(a: u16, b: u16, eva: u32, evb: u32) -> u16 {
    let ch = |c: u16, s: u32| ((c >> s) & 31) as u32;
    let f = |s| ((ch(a, s) * eva + ch(b, s) * evb) >> 4).min(31);
    (f(0) | f(5) << 5 | f(10) << 10) as u16
}
