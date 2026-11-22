//! SNES PPU (S-PPU1/S-PPU2): registers, VRAM/CGRAM/OAM, and a per-scanline renderer
//! covering background modes 0-7, sprites, priorities, color math and brightness.

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 224;

const OPAQUE: u16 = 0x8000;

#[derive(state::Stateful)]
pub struct Ppu {
    pub vram: Vec<u16>,
    pub cgram: [u16; 256],
    pub oam: [u8; 544],

    // $2100
    forced_blank: bool,
    brightness: u8,
    // sprites
    obj_size: u8,
    obj_base: u16,
    obj_gap: u16,
    oam_addr: u16,
    oam_reload: u16,
    oam_latch: u8,
    oam_priority: bool,
    // backgrounds
    mode: u8,
    bg3_prio: bool,
    big_tiles: [bool; 4],
    sc_base: [u16; 4],
    sc_size: [u8; 4],
    char_base: [u16; 4],
    hofs: [u16; 4],
    vofs: [u16; 4],
    scroll_latch: u8,
    hscroll_latch: u8,
    // VRAM port
    vmain_high: bool,
    vmain_step: u16,
    vaddr: u16,
    vread: u16,
    // mode 7
    m7sel: u8,
    m7: [i16; 6], // A B C D X Y
    m7_latch: u8,
    m7_hofs: i16,
    m7_vofs: i16,
    // CGRAM port
    cg_addr: u8,
    cg_latch: Option<u8>,
    cg_read_high: bool,
    // screens / windows / color math
    tm: u8,
    ts: u8,
    tmw: u8,
    tsw: u8,
    /// Window select nibbles for BG1-4, OBJ, color (invert1, enable1, invert2, enable2).
    wsel: [u8; 6],
    /// Window combine logic per layer (0 OR, 1 AND, 2 XOR, 3 XNOR).
    wlog: [u8; 6],
    wh: [u8; 4],
    mosaic_size: u8,
    /// SETINI: interlace (hi-res modes then show 448 lines; we take the even field).
    interlace: bool,
    /// Per-line (mode, TM) log for diagnostics.
    #[state(skip)]
    pub line_log: Vec<(u8, u8)>,
    mosaic_mask: u8,
    cgwsel: u8,
    cgadsub: u8,
    fixed: u16,
    // counters latched by $2137
    pub latch_h: u16,
    pub latch_v: u16,
    pub counters_latched: bool,
    ophct_high: bool,
    opvct_high: bool,

    #[state(skip)]
    pub frame: Vec<u32>,
}

impl Default for Ppu {
    fn default() -> Self {
        Self {
            vram: vec![0; 0x8000],
            cgram: [0; 256],
            oam: [0; 544],
            forced_blank: true,
            brightness: 0,
            obj_size: 0,
            obj_base: 0,
            obj_gap: 0,
            oam_addr: 0,
            oam_reload: 0,
            oam_latch: 0,
            oam_priority: false,
            mode: 0,
            bg3_prio: false,
            big_tiles: [false; 4],
            sc_base: [0; 4],
            sc_size: [0; 4],
            char_base: [0; 4],
            hofs: [0; 4],
            vofs: [0; 4],
            scroll_latch: 0,
            hscroll_latch: 0,
            vmain_high: false,
            vmain_step: 1,
            vaddr: 0,
            vread: 0,
            m7sel: 0,
            m7: [0; 6],
            m7_latch: 0,
            m7_hofs: 0,
            m7_vofs: 0,
            cg_addr: 0,
            cg_latch: None,
            cg_read_high: false,
            tm: 0,
            ts: 0,
            tmw: 0,
            tsw: 0,
            wsel: [0; 6],
            wlog: [0; 6],
            wh: [0; 4],
            mosaic_size: 1,
            interlace: false,
            line_log: Vec::new(),
            mosaic_mask: 0,
            cgwsel: 0,
            cgadsub: 0,
            fixed: 0,
            latch_h: 0,
            latch_v: 0,
            counters_latched: false,
            ophct_high: false,
            opvct_high: false,
            frame: vec![0; WIDTH * HEIGHT],
        }
    }
}

fn sext13(v: u16) -> i16 {
    ((v << 3) as i16) >> 3
}

impl Ppu {
    pub fn debug_state(&self) -> String {
        format!(
            "mode={} bg3prio={} big={:?} sc={:04X?} size={:?} chr={:04X?} hofs={:?} vofs={:?} tm={:02X} ts={:02X} cgwsel={:02X} cgadsub={:02X} m7sel={:02X} m7={:?} bright={} blank={}",
            self.mode, self.bg3_prio, self.big_tiles, self.sc_base, self.sc_size, self.char_base, self.hofs, self.vofs, self.tm, self.ts, self.cgwsel, self.cgadsub, self.m7sel, self.m7, self.brightness, self.forced_blank
        )
    }

    pub fn vblank_start(&mut self) {
        if !self.forced_blank {
            self.oam_addr = self.oam_reload;
        }
    }

    // ------------------------------------------------------------ registers

    pub fn write(&mut self, reg: u8, v: u8) {
        match reg {
            0x00 => {
                self.forced_blank = v & 0x80 != 0;
                self.brightness = v & 0x0F;
            }
            0x01 => {
                self.obj_size = v >> 5;
                self.obj_gap = ((v >> 3) & 3) as u16;
                self.obj_base = ((v & 7) as u16) << 13;
            }
            0x02 => {
                self.oam_reload = (self.oam_reload & 0x200) | ((v as u16) << 1);
                self.oam_addr = self.oam_reload;
            }
            0x03 => {
                self.oam_reload = (self.oam_reload & 0x1FE) | (((v & 1) as u16) << 9);
                self.oam_priority = v & 0x80 != 0;
                self.oam_addr = self.oam_reload;
            }
            0x04 => {
                let a = self.oam_addr as usize & 0x3FF;
                if a >= 0x200 {
                    self.oam[0x200 + (a & 0x1F)] = v;
                } else if a & 1 == 0 {
                    self.oam_latch = v;
                } else {
                    self.oam[a - 1] = self.oam_latch;
                    self.oam[a] = v;
                }
                self.oam_addr = (self.oam_addr + 1) & 0x3FF;
            }
            0x05 => {
                self.mode = v & 7;
                self.bg3_prio = v & 8 != 0;
                for i in 0..4 {
                    self.big_tiles[i] = v & (0x10 << i) != 0;
                }
            }
            0x06 => {
                self.mosaic_size = (v >> 4) + 1;
                self.mosaic_mask = v & 0x0F;
            }
            0x07..=0x0A => {
                let i = (reg - 0x07) as usize;
                self.sc_base[i] = ((v & 0xFC) as u16) << 8;
                self.sc_size[i] = v & 3;
            }
            0x0B => {
                self.char_base[0] = ((v & 0x0F) as u16) << 12;
                self.char_base[1] = ((v >> 4) as u16) << 12;
            }
            0x0C => {
                self.char_base[2] = ((v & 0x0F) as u16) << 12;
                self.char_base[3] = ((v >> 4) as u16) << 12;
            }
            0x0D..=0x14 => {
                let i = ((reg - 0x0D) / 2) as usize;
                if (reg - 0x0D) % 2 == 0 {
                    self.hofs[i] = ((v as u16) << 8) | (self.scroll_latch as u16 & !7) | (self.hscroll_latch as u16 & 7);
                    self.hscroll_latch = v;
                    if i == 0 {
                        self.m7_hofs = sext13(((v as u16) << 8) | self.m7_latch as u16);
                    }
                } else {
                    self.vofs[i] = ((v as u16) << 8) | self.scroll_latch as u16;
                    if i == 0 {
                        self.m7_vofs = sext13(((v as u16) << 8) | self.m7_latch as u16);
                    }
                }
                self.scroll_latch = v;
                if i == 0 {
                    self.m7_latch = v;
                }
            }
            0x15 => {
                self.vmain_high = v & 0x80 != 0;
                self.vmain_step = match v & 3 {
                    0 => 1,
                    1 => 32,
                    _ => 128,
                };
            }
            0x16 => {
                self.vaddr = (self.vaddr & 0xFF00) | v as u16;
                self.vread = self.vram[self.vaddr as usize & 0x7FFF];
            }
            0x17 => {
                self.vaddr = (self.vaddr & 0x00FF) | ((v as u16) << 8);
                self.vread = self.vram[self.vaddr as usize & 0x7FFF];
            }
            0x18 => {
                let a = self.vaddr as usize & 0x7FFF;
                self.vram[a] = (self.vram[a] & 0xFF00) | v as u16;
                if !self.vmain_high {
                    self.vaddr = self.vaddr.wrapping_add(self.vmain_step);
                }
            }
            0x19 => {
                let a = self.vaddr as usize & 0x7FFF;
                self.vram[a] = (self.vram[a] & 0x00FF) | ((v as u16) << 8);
                if self.vmain_high {
                    self.vaddr = self.vaddr.wrapping_add(self.vmain_step);
                }
            }
            0x1A => self.m7sel = v,
            0x1B..=0x20 => {
                let i = (reg - 0x1B) as usize;
                let val = ((v as u16) << 8) | self.m7_latch as u16;
                self.m7[i] = if i >= 4 { sext13(val) } else { val as i16 };
                self.m7_latch = v;
            }
            0x21 => {
                self.cg_addr = v;
                self.cg_latch = None;
                self.cg_read_high = false;
            }
            0x22 => match self.cg_latch.take() {
                None => self.cg_latch = Some(v),
                Some(lo) => {
                    self.cgram[self.cg_addr as usize] = (((v & 0x7F) as u16) << 8) | lo as u16;
                    self.cg_addr = self.cg_addr.wrapping_add(1);
                }
            },
            0x23 => {
                self.wsel[0] = v & 0x0F;
                self.wsel[1] = v >> 4;
            }
            0x24 => {
                self.wsel[2] = v & 0x0F;
                self.wsel[3] = v >> 4;
            }
            0x25 => {
                self.wsel[4] = v & 0x0F;
                self.wsel[5] = v >> 4;
            }
            0x26..=0x29 => self.wh[(reg - 0x26) as usize] = v,
            0x2A => {
                for i in 0..4 {
                    self.wlog[i] = (v >> (i * 2)) & 3;
                }
            }
            0x2B => {
                self.wlog[4] = v & 3;
                self.wlog[5] = (v >> 2) & 3;
            }
            0x33 => self.interlace = v & 1 != 0,
            0x2E => self.tmw = v,
            0x2F => self.tsw = v,
            0x2C => self.tm = v,
            0x2D => self.ts = v,
            0x30 => self.cgwsel = v,
            0x31 => self.cgadsub = v,
            0x32 => {
                let intensity = (v & 0x1F) as u16;
                if v & 0x20 != 0 {
                    self.fixed = (self.fixed & !0x001F) | intensity;
                }
                if v & 0x40 != 0 {
                    self.fixed = (self.fixed & !0x03E0) | (intensity << 5);
                }
                if v & 0x80 != 0 {
                    self.fixed = (self.fixed & !0x7C00) | (intensity << 10);
                }
            }
            _ => {} // windows, mosaic, SETINI: not yet
        }
    }

    pub fn read(&mut self, reg: u8, open_bus: u8) -> u8 {
        match reg {
            0x34..=0x36 => {
                let p = self.m7[0] as i32 * ((self.m7[1] as u16 >> 8) as i8 as i32);
                (p >> ((reg - 0x34) * 8)) as u8
            }
            0x37 => open_bus,
            0x38 => {
                let a = self.oam_addr as usize & 0x3FF;
                let v = if a >= 0x200 { self.oam[0x200 + (a & 0x1F)] } else { self.oam[a] };
                self.oam_addr = (self.oam_addr + 1) & 0x3FF;
                v
            }
            0x39 => {
                let v = self.vread as u8;
                if !self.vmain_high {
                    self.vread = self.vram[self.vaddr as usize & 0x7FFF];
                    self.vaddr = self.vaddr.wrapping_add(self.vmain_step);
                }
                v
            }
            0x3A => {
                let v = (self.vread >> 8) as u8;
                if self.vmain_high {
                    self.vread = self.vram[self.vaddr as usize & 0x7FFF];
                    self.vaddr = self.vaddr.wrapping_add(self.vmain_step);
                }
                v
            }
            0x3B => {
                let c = self.cgram[self.cg_addr as usize];
                let v = if self.cg_read_high {
                    self.cg_addr = self.cg_addr.wrapping_add(1);
                    (c >> 8) as u8 & 0x7F
                } else {
                    c as u8
                };
                self.cg_read_high = !self.cg_read_high;
                v
            }
            0x3C => {
                let v = if self.ophct_high { (self.latch_h >> 8) as u8 & 1 } else { self.latch_h as u8 };
                self.ophct_high = !self.ophct_high;
                v
            }
            0x3D => {
                let v = if self.opvct_high { (self.latch_v >> 8) as u8 & 1 } else { self.latch_v as u8 };
                self.opvct_high = !self.opvct_high;
                v
            }
            0x3E => 0x01, // STAT77: PPU1 version 1
            0x3F => {
                let v = 0x03 | ((self.counters_latched as u8) << 6);
                self.counters_latched = false;
                self.ophct_high = false;
                self.opvct_high = false;
                v
            }
            _ => open_bus,
        }
    }

    // ------------------------------------------------------------ rendering

    /// Bits per pixel for BG `i` (0-based) in the current mode; 0 = layer absent.
    fn bpp(&self, i: usize) -> u8 {
        match (self.mode, i) {
            (0, _) => 2,
            (1, 0) | (1, 1) => 4,
            (1, 2) => 2,
            (2, 0) | (2, 1) => 4,
            (3, 0) => 8,
            (3, 1) => 4,
            (4, 0) => 8,
            (4, 1) => 2,
            (5, 0) => 4,
            (5, 1) => 2,
            (6, 0) => 4,
            (7, 0) => 8,
            _ => 0,
        }
    }

    /// Decodes one pixel (color number within the tile) from VRAM.
    #[inline]
    fn tile_pixel(&self, base: u16, tile: u16, bpp: u8, x: u16, y: u16) -> u16 {
        let words = bpp as u16 * 4; // words per 8x8 tile
        let addr = base.wrapping_add(tile.wrapping_mul(words)).wrapping_add(y) as usize;
        let bit = 7 - x;
        let mut c = 0u16;
        for plane in 0..(bpp as usize / 2) {
            let w = self.vram[(addr + plane * 8) & 0x7FFF];
            c |= ((w >> bit) & 1) << (plane * 2);
            c |= ((w >> (8 + bit)) & 1) << (plane * 2 + 1);
        }
        c
    }

    /// Color of one BG pixel at background coordinates (bx, by); 0 = transparent.
    /// `tw`/`th` are the tile width/height (hi-res modes use 16-pixel-wide tiles).
    #[inline]
    fn bg_pixel(&self, i: usize, bpp: u8, bx: u16, by: u16, tw: u16, th: u16) -> (u16, bool) {
        let (wide, tall) = (self.sc_size[i] & 1 != 0, self.sc_size[i] & 2 != 0);
        let (tx, ty) = (bx / tw, by / th);
        // Tilemaps are 32x32 entries; larger screens tile them 2x1 / 1x2 / 2x2.
        let mut map = self.sc_base[i] as usize + ((ty as usize & 31) << 5) + (tx as usize & 31);
        if wide && tx & 32 != 0 {
            map += 0x400;
        }
        if tall && ty & 32 != 0 {
            map += if wide { 0x800 } else { 0x400 };
        }
        let entry = self.vram[map & 0x7FFF];
        let mut tile = entry & 0x3FF;
        let mut fx = bx % tw;
        let mut fy = by % th;
        if entry & 0x4000 != 0 {
            fx = tw - 1 - fx;
        }
        if entry & 0x8000 != 0 {
            fy = th - 1 - fy;
        }
        if fx >= 8 {
            tile += 1;
        }
        if fy >= 8 {
            tile += 16;
        }
        let c = self.tile_pixel(self.char_base[i], tile & 0x3FF, bpp, fx & 7, fy & 7);
        if c == 0 {
            return (0, false);
        }
        let pal = (entry >> 10) & 7;
        let idx = match bpp {
            2 => (if self.mode == 0 { i as u16 * 32 } else { 0 }) + pal * 4 + c,
            4 => pal * 16 + c,
            _ => c,
        };
        (self.cgram[idx as usize & 0xFF] | OPAQUE, entry & 0x2000 != 0)
    }

    /// One background line: per-pixel (BGR555|OPAQUE, high-priority flag).
    fn render_bg(&self, i: usize, line: u16, out: &mut [(u16, bool); WIDTH]) {
        let bpp = self.bpp(i);
        let th: u16 = if self.big_tiles[i] { 16 } else { 8 };
        let hires = self.mode == 5 || self.mode == 6;
        let hofs = self.hofs[i] & 0x3FF;
        let vofs = self.vofs[i] & 0x3FF;
        // Mosaic: snap to the top-left pixel of each size x size block.
        let mosaic = if self.mosaic_mask & (1 << i) != 0 { self.mosaic_size as u16 } else { 1 };
        let mline = line - (line - 1) % mosaic;
        let vline = if hires && self.interlace { mline * 2 } else { mline };
        let by = vline.wrapping_add(vofs);
        for (x, o) in out.iter_mut().enumerate() {
            let mx = (x as u16) - (x as u16) % mosaic;
            if hires {
                // 512 pixels across: sample both half-pixels and average them down to 256.
                let base = (mx.wrapping_add(hofs)) << 1;
                let a = self.bg_pixel(i, bpp, base, by, 16, th);
                let b = self.bg_pixel(i, bpp, base + 1, by, 16, th);
                *o = match (a.0 != 0, b.0 != 0) {
                    (true, true) => (blend(a.0, b.0) | OPAQUE, a.1 || b.1),
                    (true, false) => a,
                    (false, true) => b,
                    _ => (0, false),
                };
            } else {
                let tw = th;
                *o = self.bg_pixel(i, bpp, mx.wrapping_add(hofs), by, tw, th);
            }
        }
    }

    /// Whether pixel `x` is inside the window configuration of `layer` (0-3 BGs, 4 OBJ, 5 color).
    #[inline]
    fn in_window(&self, layer: usize, x: u8) -> bool {
        let sel = self.wsel[layer];
        let w = |enable: u8, invert: u8, l: u8, r: u8| -> Option<bool> {
            if sel & enable == 0 {
                return None;
            }
            let inside = l <= x && x <= r;
            Some(inside != (sel & invert != 0))
        };
        let w1 = w(0x02, 0x01, self.wh[0], self.wh[1]);
        let w2 = w(0x08, 0x04, self.wh[2], self.wh[3]);
        match (w1, w2) {
            (None, None) => false,
            (Some(a), None) | (None, Some(a)) => a,
            (Some(a), Some(b)) => match self.wlog[layer] {
                0 => a || b,
                1 => a && b,
                2 => a != b,
                _ => a == b,
            },
        }
    }

    fn render_mode7(&self, line: u16, out: &mut [(u16, bool); WIDTH]) {
        let [a, b, c, d, cx, cy] = self.m7.map(|v| v as i32);
        let (hofs, vofs) = (self.m7_hofs as i32, self.m7_vofs as i32);
        let clip = |v: i32| if v & 0x2000 != 0 { v | !0x3FF } else { v & 0x3FF };
        let y = if self.m7sel & 2 != 0 { 255 - line as i32 } else { line as i32 };
        let ox = clip(hofs - cx);
        let oy = clip(vofs - cy);
        let fill = self.m7sel >> 6;
        for (sx, o) in out.iter_mut().enumerate() {
            let x = if self.m7sel & 1 != 0 { 255 - sx as i32 } else { sx as i32 };
            let px = ((a * (x + ox) + b * (y + oy)) >> 8) + cx;
            let py = ((c * (x + ox) + d * (y + oy)) >> 8) + cy;
            let outside = !(0..1024).contains(&px) || !(0..1024).contains(&py);
            let color = if outside && fill == 2 {
                0
            } else {
                let (px, py) = (px & 1023, py & 1023);
                let tile = if outside && fill == 3 { 0 } else { self.vram[((py >> 3) * 128 + (px >> 3)) as usize] & 0xFF };
                self.vram[(tile as usize * 64 + ((py & 7) * 8 + (px & 7)) as usize) & 0x7FFF] >> 8
            };
            *o = if color == 0 { (0, false) } else { (self.cgram[color as usize] | OPAQUE, false) };
        }
    }

    /// Sprites on a line: per pixel (BGR555|OPAQUE, priority 0-3, palette >= 4).
    fn render_sprites(&self, line: u16, out: &mut [(u16, u8, bool); WIDTH]) {
        const SIZES: [(u16, u16); 8] = [(8, 16), (8, 32), (8, 64), (16, 32), (16, 64), (32, 64), (16, 32), (16, 32)];
        let (small, large) = SIZES[self.obj_size as usize];
        let mut count = 0;
        for i in 0..128usize {
            let hi = self.oam[0x200 + i / 4] >> ((i % 4) * 2);
            let size = if hi & 2 != 0 { large } else { small };
            let mut x = self.oam[i * 4] as i32 | (((hi & 1) as i32) << 8);
            if x >= 256 {
                x -= 512;
            }
            let y = self.oam[i * 4 + 1] as u16;
            let row = line.wrapping_sub(y).wrapping_sub(1) & 0xFF;
            if row >= size {
                continue;
            }
            count += 1;
            if count > 32 {
                break;
            }
            let tile = self.oam[i * 4 + 2] as u16;
            let attr = self.oam[i * 4 + 3];
            let name = (attr & 1) as u16;
            let pal = ((attr >> 1) & 7) as u16;
            let prio = (attr >> 4) & 3;
            let (hflip, vflip) = (attr & 0x40 != 0, attr & 0x80 != 0);
            let ry = if vflip { size - 1 - row } else { row };
            let base = self.obj_base.wrapping_add(if name != 0 { (self.obj_gap + 1) << 12 } else { 0 });
            for col in 0..size as i32 {
                let sx = x + col;
                if !(0..WIDTH as i32).contains(&sx) || out[sx as usize].0 != 0 {
                    continue;
                }
                let rx = if hflip { size as i32 - 1 - col } else { col } as u16;
                // 16 tiles per row in the name table; column/row wrap within the tile's nibbles.
                let t = (((tile & 0xF0) + (ry / 8) * 16) & 0xF0) | ((tile + rx / 8) & 0x0F);
                let c = self.tile_pixel(base, t, 4, rx & 7, ry & 7);
                if c != 0 {
                    out[sx as usize] = (self.cgram[(128 + pal * 16 + c) as usize] | OPAQUE, prio, pal >= 4);
                }
            }
        }
    }

    /// Renders visible line `line` (1-224) into the frame buffer.
    pub fn render_line(&mut self, line: u16) {
        let row = line as usize - 1;
        if row >= HEIGHT {
            return;
        }
        if self.line_log.len() < 4096 {
            self.line_log.push((if self.forced_blank { 0xFF } else { self.mode }, self.tm));
        }
        if self.forced_blank {
            self.frame[row * WIDTH..(row + 1) * WIDTH].fill(0);
            return;
        }
        let mut bg = [[(0u16, false); WIDTH]; 4];
        for i in 0..4 {
            if (self.tm | self.ts) & (1 << i) == 0 {
                continue;
            }
            if self.mode == 7 {
                if i == 0 {
                    self.render_mode7(line, &mut bg[0]);
                }
            } else if self.bpp(i) != 0 {
                self.render_bg(i, line, &mut bg[i]);
            }
        }
        let mut obj = [(0u16, 0u8, false); WIDTH];
        if (self.tm | self.ts) & 0x10 != 0 {
            self.render_sprites(line, &mut obj);
        }

        // Layer order front-to-back, as (layer id, which priority).  Layer ids: 0-3 BGs, 4 OBJ.
        // For BGs `p` selects the high(1)/low(0) tile priority; for OBJ it is the sprite priority.
        let order: &[(u8, u8)] = match self.mode {
            0 => &[(4, 3), (0, 1), (1, 1), (4, 2), (0, 0), (1, 0), (4, 1), (2, 1), (3, 1), (4, 0), (2, 0), (3, 0)],
            1 if self.bg3_prio => &[(2, 1), (4, 3), (0, 1), (1, 1), (4, 2), (0, 0), (1, 0), (4, 1), (4, 0), (2, 0)],
            1 => &[(4, 3), (0, 1), (1, 1), (4, 2), (0, 0), (1, 0), (4, 1), (2, 1), (4, 0), (2, 0)],
            7 => &[(4, 3), (4, 2), (4, 1), (0, 0), (4, 0)],
            _ => &[(4, 3), (0, 1), (4, 2), (1, 1), (4, 1), (0, 0), (4, 0), (1, 0)],
        };

        // Window masks: per layer, per pixel, is the layer clipped on the main / sub screen?
        let mut win = [[false; WIDTH]; 6];
        let windows_used = (self.tmw | self.tsw) & 0x1F != 0 || self.cgwsel & 0xF0 != 0;
        if windows_used {
            for (layer, mask) in win.iter_mut().enumerate() {
                for (x, m) in mask.iter_mut().enumerate() {
                    *m = self.in_window(layer, x as u8);
                }
            }
        }

        let pick = |x: usize, screen: u8, masked: u8| -> (u16, u8, bool) {
            for &(layer, p) in order {
                if screen & (1 << layer) == 0 || (masked & (1 << layer) != 0 && win[layer as usize][x]) {
                    continue;
                }
                if layer == 4 {
                    let (c, prio, math_pal) = obj[x];
                    if c != 0 && prio == p {
                        return (c, 4, math_pal);
                    }
                } else {
                    let (c, hi) = bg[layer as usize][x];
                    if c != 0 && (hi as u8) == p {
                        return (c, layer, true);
                    }
                }
            }
            (self.cgram[0] | OPAQUE, 5, true)
        };

        let bright = self.brightness as u32;
        let region = |mode: u8, inside: bool| match mode & 3 {
            0 => false,
            1 => !inside,
            2 => inside,
            _ => true,
        };
        for x in 0..WIDTH {
            let (main, layer, math_ok) = pick(x, self.tm, self.tmw);
            let mut c = main & 0x7FFF;
            let color_win = win[5][x];
            // "Clip to black" region, then color math unless prevented by the color window.
            if region(self.cgwsel >> 6, color_win) {
                c = 0;
            }
            let math_allowed = !region(self.cgwsel >> 4, color_win);
            if math_allowed && self.cgadsub & (1 << layer) != 0 && math_ok {
                let other = if self.cgwsel & 2 != 0 {
                    let (s, _, _) = pick(x, self.ts, self.tsw);
                    s & 0x7FFF
                } else {
                    self.fixed
                };
                let sub = self.cgadsub & 0x80 != 0;
                let half = self.cgadsub & 0x40 != 0;
                let mut r = 0u16;
                for shift in [0, 5, 10] {
                    let a = ((c >> shift) & 31) as i32;
                    let b = ((other >> shift) & 31) as i32;
                    let mut v = if sub { a - b } else { a + b };
                    if half {
                        v >>= 1;
                    }
                    r |= (v.clamp(0, 31) as u16) << shift;
                }
                c = r;
            }
            let conv = |v: u16| {
                let v = v as u32 & 31;
                ((v << 3 | v >> 2) * (bright + 1) / 16).min(255)
            };
            self.frame[row * WIDTH + x] = conv(c) << 16 | conv(c >> 5) << 8 | conv(c >> 10);
        }
    }
}

/// Averages two BGR555 colors (used to fold 512-wide hi-res lines into 256).
#[inline]
fn blend(a: u16, b: u16) -> u16 {
    let mut r = 0;
    for shift in [0, 5, 10] {
        r |= ((((a >> shift) & 31) + ((b >> shift) & 31)) / 2) << shift;
    }
    r
}
