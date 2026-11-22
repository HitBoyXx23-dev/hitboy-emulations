//! Game Boy / Game Boy Color picture processing unit, rendered one scanline at a time.

pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 144;

/// DMG shades: soft green-tinted greys, easy on the eyes on large screens.
const SHADES: [u32; 4] = [0xE3EEC0, 0xAEBA89, 0x5E6745, 0x202020];

/// Interrupt requests raised while ticking (IF bits).
pub const INT_VBLANK: u8 = 1;
pub const INT_STAT: u8 = 2;

#[derive(state::Stateful)]
pub struct Ppu {
    /// Two 8 KiB banks (bank 1 exists on Game Boy Color only).
    pub vram: [u8; 0x4000],
    pub vbk: u8,
    pub oam: [u8; 0xA0],
    pub lcdc: u8,
    pub stat: u8,
    pub scy: u8,
    pub scx: u8,
    pub ly: u8,
    pub lyc: u8,
    pub bgp: u8,
    pub obp0: u8,
    pub obp1: u8,
    pub wy: u8,
    pub wx: u8,
    /// Game Boy Color mode.
    #[state(skip)]
    pub cgb: bool,
    pub bg_pal: [u8; 64],
    pub obj_pal: [u8; 64],
    pub bcps: u8,
    pub ocps: u8,
    /// Set when mode 0 (H-Blank) starts; drives H-Blank DMA.
    pub hblank_entered: bool,
    window_line: u8,
    dot: u32,
    #[state(skip)]
    pub frame: Vec<u32>,
    pub frame_ready: bool,
}

impl Default for Ppu {
    fn default() -> Self {
        Self {
            vram: [0; 0x4000],
            vbk: 0,
            oam: [0; 0xA0],
            lcdc: 0x91,
            stat: 0x85,
            scy: 0,
            scx: 0,
            ly: 0,
            lyc: 0,
            bgp: 0xFC,
            obp0: 0xFF,
            obp1: 0xFF,
            wy: 0,
            wx: 0,
            cgb: false,
            bg_pal: [0xFF; 64],
            obj_pal: [0xFF; 64],
            bcps: 0,
            ocps: 0,
            hblank_entered: false,
            window_line: 0,
            dot: 0,
            frame: vec![SHADES[0]; WIDTH * HEIGHT],
            frame_ready: false,
        }
    }
}

/// RGB555 palette entry to 0RGB888.
#[inline]
fn cgb_rgb(pal: &[u8; 64], palette: u8, id: u8) -> u32 {
    let i = palette as usize * 8 + id as usize * 2;
    let c = pal[i] as u32 | (pal[i + 1] as u32) << 8;
    let ex = |v: u32| (v << 3) | (v >> 2);
    (ex(c & 31) << 16) | (ex((c >> 5) & 31) << 8) | ex((c >> 10) & 31)
}

impl Ppu {
    fn mode(&self) -> u8 {
        self.stat & 3
    }

    pub fn read_vram(&self, addr: u16) -> u8 {
        self.vram[(self.vbk as usize & 1) * 0x2000 + (addr as usize & 0x1FFF)]
    }

    pub fn write_vram(&mut self, addr: u16, v: u8) {
        self.vram[(self.vbk as usize & 1) * 0x2000 + (addr as usize & 0x1FFF)] = v;
    }

    /// Palette data ports (FF69 / FF6B) with auto-increment.
    pub fn read_pal(&self, obj: bool) -> u8 {
        if obj { self.obj_pal[(self.ocps & 0x3F) as usize] } else { self.bg_pal[(self.bcps & 0x3F) as usize] }
    }

    pub fn write_pal(&mut self, obj: bool, v: u8) {
        let (spec, mem) = if obj { (&mut self.ocps, &mut self.obj_pal) } else { (&mut self.bcps, &mut self.bg_pal) };
        mem[(*spec & 0x3F) as usize] = v;
        if *spec & 0x80 != 0 {
            *spec = 0x80 | ((*spec + 1) & 0x3F);
        }
    }

    fn set_mode(&mut self, m: u8) -> u8 {
        self.stat = (self.stat & !3) | m;
        let src = match m {
            0 => 0x08,
            1 => 0x10,
            2 => 0x20,
            _ => 0,
        };
        if self.stat & src != 0 { INT_STAT } else { 0 }
    }

    fn check_lyc(&mut self) -> u8 {
        if self.ly == self.lyc {
            self.stat |= 4;
            if self.stat & 0x40 != 0 {
                return INT_STAT;
            }
        } else {
            self.stat &= !4;
        }
        0
    }

    /// Advances by `cycles` dots; returns interrupt bits to raise.
    pub fn tick(&mut self, cycles: u32) -> u8 {
        if self.lcdc & 0x80 == 0 {
            return 0;
        }
        let mut irq = 0;
        self.dot += cycles;
        loop {
            match self.mode() {
                2 if self.dot >= 80 => {
                    self.dot -= 80;
                    irq |= self.set_mode(3);
                }
                3 if self.dot >= 172 => {
                    self.dot -= 172;
                    self.render_line();
                    irq |= self.set_mode(0);
                    self.hblank_entered = true;
                }
                0 if self.dot >= 204 => {
                    self.dot -= 204;
                    self.ly += 1;
                    irq |= self.check_lyc();
                    if self.ly == 144 {
                        irq |= INT_VBLANK | self.set_mode(1);
                        self.frame_ready = true;
                    } else {
                        irq |= self.set_mode(2);
                    }
                }
                1 if self.dot >= 456 => {
                    self.dot -= 456;
                    self.ly += 1;
                    if self.ly > 153 {
                        self.ly = 0;
                        self.window_line = 0;
                        irq |= self.set_mode(2);
                    }
                    irq |= self.check_lyc();
                }
                _ => break,
            }
        }
        irq
    }

    pub fn write_lcdc(&mut self, v: u8) {
        let was_on = self.lcdc & 0x80 != 0;
        self.lcdc = v;
        if was_on && v & 0x80 == 0 {
            self.ly = 0;
            self.dot = 0;
            self.window_line = 0;
            self.stat &= !3;
        } else if !was_on && v & 0x80 != 0 {
            self.dot = 0;
            self.set_mode(2);
        }
    }

    #[inline]
    fn tile_row(&self, tile_addr: usize, row: usize) -> (u8, u8) {
        (self.vram[tile_addr + row * 2], self.vram[tile_addr + row * 2 + 1])
    }

    fn bg_tile_addr(&self, index: u8) -> usize {
        if self.lcdc & 0x10 != 0 {
            index as usize * 16
        } else {
            (0x1000 + (index as i8 as i32) * 16) as usize
        }
    }

    fn render_line(&mut self) {
        if (self.ly as usize) < HEIGHT {
            if self.cgb {
                self.render_line_cgb();
            } else {
                self.render_line_dmg();
            }
        }
    }

    /// Sprites on this line (max 10), in OAM order.
    fn line_sprites(&self, y: usize, height: i32) -> Vec<usize> {
        (0..40)
            .filter(|&i| {
                let sy = self.oam[i * 4] as i32 - 16;
                (sy..sy + height).contains(&(y as i32))
            })
            .take(10)
            .collect()
    }

    fn render_line_dmg(&mut self) {
        let y = self.ly as usize;
        let mut color_ids = [0u8; WIDTH];

        if self.lcdc & 0x01 != 0 {
            let map = if self.lcdc & 0x08 != 0 { 0x1C00 } else { 0x1800 };
            let py = (y + self.scy as usize) & 0xFF;
            for (x, id) in color_ids.iter_mut().enumerate() {
                let px = (x + self.scx as usize) & 0xFF;
                let idx = self.vram[map + (py / 8) * 32 + px / 8];
                let (lo, hi) = self.tile_row(self.bg_tile_addr(idx), py % 8);
                let bit = 7 - (px % 8);
                *id = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
            }
            if self.lcdc & 0x20 != 0 && y >= self.wy as usize && self.wx <= 166 {
                let map = if self.lcdc & 0x40 != 0 { 0x1C00 } else { 0x1800 };
                let wy = self.window_line as usize;
                let wx0 = self.wx as i32 - 7;
                let mut drew = false;
                for x in wx0.max(0) as usize..WIDTH {
                    let wx = (x as i32 - wx0) as usize;
                    let idx = self.vram[map + (wy / 8) * 32 + wx / 8];
                    let (lo, hi) = self.tile_row(self.bg_tile_addr(idx), wy % 8);
                    let bit = 7 - (wx % 8);
                    color_ids[x] = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
                    drew = true;
                }
                if drew {
                    self.window_line += 1;
                }
            }
        }

        for x in 0..WIDTH {
            self.frame[y * WIDTH + x] = SHADES[((self.bgp >> (color_ids[x] * 2)) & 3) as usize];
        }

        if self.lcdc & 0x02 != 0 {
            let height = if self.lcdc & 0x04 != 0 { 16 } else { 8 };
            let mut sprites = self.line_sprites(y, height);
            // DMG priority: lower X wins, ties go to lower OAM index. Draw lowest priority first.
            sprites.sort_by_key(|&i| std::cmp::Reverse((self.oam[i * 4 + 1], i)));
            for i in sprites {
                let sy = self.oam[i * 4] as i32 - 16;
                let sx = self.oam[i * 4 + 1] as i32 - 8;
                let mut tile = self.oam[i * 4 + 2] as usize;
                let attr = self.oam[i * 4 + 3];
                let mut row = (y as i32 - sy) as usize;
                if attr & 0x40 != 0 {
                    row = height as usize - 1 - row;
                }
                if height == 16 {
                    tile &= 0xFE;
                }
                let (lo, hi) = self.tile_row(tile * 16, row);
                let pal = if attr & 0x10 != 0 { self.obp1 } else { self.obp0 };
                for col in 0..8 {
                    let x = sx + col;
                    if !(0..WIDTH as i32).contains(&x) {
                        continue;
                    }
                    let bit = if attr & 0x20 != 0 { col } else { 7 - col };
                    let id = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
                    if id == 0 || (attr & 0x80 != 0 && color_ids[x as usize] != 0) {
                        continue;
                    }
                    self.frame[y * WIDTH + x as usize] = SHADES[((pal >> (id * 2)) & 3) as usize];
                }
            }
        }
    }

    /// Reads one background/window pixel using its CGB attribute byte (bank 1 of the map).
    #[inline]
    fn cgb_bg_pixel(&self, map: usize, tx: usize, ty: usize) -> (u8, u8, bool) {
        let ti = map + (ty / 8) * 32 + tx / 8;
        let idx = self.vram[ti];
        let attr = self.vram[0x2000 + ti];
        let mut row = ty % 8;
        if attr & 0x40 != 0 {
            row = 7 - row;
        }
        let bank = if attr & 0x08 != 0 { 0x2000 } else { 0 };
        let (lo, hi) = self.tile_row(bank + self.bg_tile_addr(idx), row);
        let bit = if attr & 0x20 != 0 { tx % 8 } else { 7 - tx % 8 };
        (((lo >> bit) & 1) | (((hi >> bit) & 1) << 1), attr & 7, attr & 0x80 != 0)
    }

    fn render_line_cgb(&mut self) {
        let y = self.ly as usize;
        let mut bg_id = [0u8; WIDTH];
        let mut bg_prio = [false; WIDTH];

        let map = if self.lcdc & 0x08 != 0 { 0x1C00 } else { 0x1800 };
        let py = (y + self.scy as usize) & 0xFF;
        for x in 0..WIDTH {
            let px = (x + self.scx as usize) & 0xFF;
            let (id, pal, prio) = self.cgb_bg_pixel(map, px, py);
            bg_id[x] = id;
            bg_prio[x] = prio;
            self.frame[y * WIDTH + x] = cgb_rgb(&self.bg_pal, pal, id);
        }
        if self.lcdc & 0x20 != 0 && y >= self.wy as usize && self.wx <= 166 {
            let wmap = if self.lcdc & 0x40 != 0 { 0x1C00 } else { 0x1800 };
            let wy = self.window_line as usize;
            let wx0 = self.wx as i32 - 7;
            let mut drew = false;
            for x in wx0.max(0) as usize..WIDTH {
                let wx = (x as i32 - wx0) as usize;
                let (id, pal, prio) = self.cgb_bg_pixel(wmap, wx, wy);
                bg_id[x] = id;
                bg_prio[x] = prio;
                self.frame[y * WIDTH + x] = cgb_rgb(&self.bg_pal, pal, id);
                drew = true;
            }
            if drew {
                self.window_line += 1;
            }
        }

        if self.lcdc & 0x02 != 0 {
            let height = if self.lcdc & 0x04 != 0 { 16 } else { 8 };
            let master = self.lcdc & 0x01 != 0;
            // CGB priority is OAM order: draw from the last so index 0 ends on top.
            for i in self.line_sprites(y, height).into_iter().rev() {
                let sy = self.oam[i * 4] as i32 - 16;
                let sx = self.oam[i * 4 + 1] as i32 - 8;
                let mut tile = self.oam[i * 4 + 2] as usize;
                let attr = self.oam[i * 4 + 3];
                let mut row = (y as i32 - sy) as usize;
                if attr & 0x40 != 0 {
                    row = height as usize - 1 - row;
                }
                if height == 16 {
                    tile &= 0xFE;
                }
                let bank = if attr & 0x08 != 0 { 0x2000 } else { 0 };
                let (lo, hi) = self.tile_row(bank + tile * 16, row);
                for col in 0..8 {
                    let x = sx + col;
                    if !(0..WIDTH as i32).contains(&x) {
                        continue;
                    }
                    let xu = x as usize;
                    let bit = if attr & 0x20 != 0 { col } else { 7 - col };
                    let id = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
                    if id == 0 {
                        continue;
                    }
                    if master && bg_id[xu] != 0 && (bg_prio[xu] || attr & 0x80 != 0) {
                        continue;
                    }
                    self.frame[y * WIDTH + xu] = cgb_rgb(&self.obj_pal, attr & 7, id);
                }
            }
        }
    }
}
