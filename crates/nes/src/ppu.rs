//! 2C02 PPU. Uses the "loopy" scroll registers and renders one full scanline
//! at a time, which handles mid-frame scroll splits (status bars) correctly
//! for the vast majority of games while staying fast.

use crate::cartridge::{Mapper, Mirroring};

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 240;

const PALETTE: [u32; 64] = [
    0x666666, 0x002A88, 0x1412A7, 0x3B00A4, 0x5C007E, 0x6E0040, 0x6C0600, 0x561D00,
    0x333500, 0x0B4800, 0x005200, 0x004F08, 0x00404D, 0x000000, 0x000000, 0x000000,
    0xADADAD, 0x155FD9, 0x4240FF, 0x7527FE, 0xA01ACC, 0xB71E7B, 0xB53120, 0x994E00,
    0x6B6D00, 0x388700, 0x0C9300, 0x008F32, 0x007C8D, 0x000000, 0x000000, 0x000000,
    0xFFFEFF, 0x64B0FF, 0x9290FF, 0xC676FF, 0xF36AFF, 0xFE6ECC, 0xFE8170, 0xEA9E22,
    0xBCBE00, 0x88D800, 0x5CE430, 0x45E082, 0x48CDDE, 0x4F4F4F, 0x000000, 0x000000,
    0xFFFEFF, 0xC0DFFF, 0xD3D2FF, 0xE8C8FF, 0xFBC2FF, 0xFEC4EA, 0xFECCC5, 0xF7D8A5,
    0xE4E594, 0xCFEF96, 0xBDF4AB, 0xB3F3CC, 0xB5EBF2, 0xB8B8B8, 0x000000, 0x000000,
];

#[derive(state::Stateful)]
pub struct Ppu {
    ctrl: u8,
    mask: u8,
    status: u8,
    oam_addr: u8,
    pub oam: [u8; 256],
    vram: [u8; 0x1000],
    palette: [u8; 32],
    read_buffer: u8,
    // loopy registers
    v: u16,
    t: u16,
    fine_x: u8,
    w: bool,

    scanline: u16,
    dot: u16,
    odd_frame: bool,
    nmi: bool,
    pub frame_complete: bool,
    /// 0RGB pixels, ready for a window/texture.
    #[state(skip)]
    pub frame: Vec<u32>,
}

impl Default for Ppu {
    fn default() -> Self {
        Self {
            ctrl: 0,
            mask: 0,
            status: 0,
            oam_addr: 0,
            oam: [0; 256],
            vram: [0; 0x1000],
            palette: [0; 32],
            read_buffer: 0,
            v: 0,
            t: 0,
            fine_x: 0,
            w: false,
            scanline: 261,
            dot: 0,
            odd_frame: false,
            nmi: false,
            frame_complete: false,
            frame: vec![0; WIDTH * HEIGHT],
        }
    }
}

impl Ppu {
    fn rendering(&self) -> bool {
        self.mask & 0x18 != 0
    }

    fn nt_index(addr: u16, mirroring: Mirroring) -> usize {
        let a = (addr - 0x2000) as usize & 0x0FFF;
        let table = a / 0x400;
        let off = a & 0x3FF;
        let page = match mirroring {
            Mirroring::Horizontal => table / 2,
            Mirroring::Vertical => table & 1,
            Mirroring::SingleLower => 0,
            Mirroring::SingleUpper => 1,
            Mirroring::FourScreen => table,
            Mirroring::Custom(q) => q[table] as usize,
        };
        page * 0x400 + off
    }

    fn palette_index(addr: u16) -> usize {
        let mut i = addr as usize & 0x1F;
        if i >= 16 && i % 4 == 0 {
            i -= 16;
        }
        i
    }

    fn mem_read(&mut self, addr: u16, cart: &mut dyn Mapper) -> u8 {
        let addr = addr & 0x3FFF;
        match addr {
            0x0000..=0x1FFF => cart.ppu_read(addr),
            0x2000..=0x3EFF => match cart.nt_read(addr) {
                Some(v) => v,
                None => self.vram[Self::nt_index(addr, cart.mirroring())],
            },
            _ => self.palette[Self::palette_index(addr)],
        }
    }

    fn mem_write(&mut self, addr: u16, value: u8, cart: &mut dyn Mapper) {
        let addr = addr & 0x3FFF;
        match addr {
            0x0000..=0x1FFF => cart.ppu_write(addr, value),
            0x2000..=0x3EFF => {
                if !cart.nt_write(addr, value) {
                    self.vram[Self::nt_index(addr, cart.mirroring())] = value;
                }
            }
            _ => self.palette[Self::palette_index(addr)] = value & 0x3F,
        }
    }

    pub fn read_register(&mut self, reg: u16, cart: &mut dyn Mapper) -> u8 {
        match reg & 7 {
            2 => {
                let v = (self.status & 0xE0) | (self.read_buffer & 0x1F);
                self.status &= !0x80;
                self.w = false;
                v
            }
            4 => self.oam[self.oam_addr as usize],
            7 => {
                let addr = self.v;
                let value = if addr & 0x3FFF >= 0x3F00 {
                    self.read_buffer = self.mem_read(addr - 0x1000, cart);
                    self.mem_read(addr, cart)
                } else {
                    let buffered = self.read_buffer;
                    self.read_buffer = self.mem_read(addr, cart);
                    buffered
                };
                self.increment_v();
                value
            }
            _ => 0,
        }
    }

    pub fn write_register(&mut self, reg: u16, value: u8, cart: &mut dyn Mapper) {
        match reg & 7 {
            0 => {
                let was_enabled = self.ctrl & 0x80 != 0;
                self.ctrl = value;
                self.t = (self.t & 0xF3FF) | ((value as u16 & 3) << 10);
                if !was_enabled && value & 0x80 != 0 && self.status & 0x80 != 0 {
                    self.nmi = true;
                }
            }
            1 => self.mask = value,
            3 => self.oam_addr = value,
            4 => {
                self.oam[self.oam_addr as usize] = value;
                self.oam_addr = self.oam_addr.wrapping_add(1);
            }
            5 => {
                if !self.w {
                    self.t = (self.t & 0xFFE0) | (value as u16 >> 3);
                    self.fine_x = value & 7;
                } else {
                    self.t = (self.t & 0x8C1F) | ((value as u16 & 7) << 12) | ((value as u16 & 0xF8) << 2);
                }
                self.w = !self.w;
            }
            6 => {
                if !self.w {
                    self.t = (self.t & 0x80FF) | ((value as u16 & 0x3F) << 8);
                } else {
                    self.t = (self.t & 0xFF00) | value as u16;
                    self.v = self.t;
                }
                self.w = !self.w;
            }
            7 => {
                self.mem_write(self.v, value, cart);
                self.increment_v();
            }
            _ => {}
        }
    }

    pub fn oam_dma_write(&mut self, value: u8) {
        self.oam[self.oam_addr as usize] = value;
        self.oam_addr = self.oam_addr.wrapping_add(1);
    }

    fn increment_v(&mut self) {
        let step = if self.ctrl & 0x04 != 0 { 32 } else { 1 };
        self.v = self.v.wrapping_add(step) & 0x7FFF;
    }

    fn increment_y(&mut self) {
        if self.v & 0x7000 != 0x7000 {
            self.v += 0x1000;
        } else {
            self.v &= !0x7000;
            let mut y = (self.v & 0x03E0) >> 5;
            if y == 29 {
                y = 0;
                self.v ^= 0x0800;
            } else if y == 31 {
                y = 0;
            } else {
                y += 1;
            }
            self.v = (self.v & !0x03E0) | (y << 5);
        }
    }

    fn copy_x(&mut self) {
        self.v = (self.v & !0x041F) | (self.t & 0x041F);
    }

    fn copy_y(&mut self) {
        self.v = (self.v & !0x7BE0) | (self.t & 0x7BE0);
    }

    /// Returns true if an NMI should be raised on the CPU.
    pub fn take_nmi(&mut self) -> bool {
        std::mem::take(&mut self.nmi)
    }

    /// Next dot on the current scanline where `tick_one` does something other than count.
    #[inline]
    fn next_event(&self) -> u16 {
        let events: &[u16] = match self.scanline {
            0..=239 => &[256, 257, 260, 340],
            241 => &[1, 340],
            261 => &[1, 257, 260, 280, 339, 340],
            _ => &[340],
        };
        events.iter().copied().find(|&e| e >= self.dot).unwrap_or(340)
    }

    /// Advances the PPU by `dots` PPU cycles, jumping directly between event dots.
    pub fn tick(&mut self, dots: u32, cart: &mut dyn Mapper) {
        let mut remaining = dots;
        while remaining > 0 {
            let gap = (self.next_event() - self.dot) as u32;
            if gap > 0 {
                let step = gap.min(remaining);
                self.dot += step as u16;
                remaining -= step;
                continue;
            }
            self.tick_one(cart);
            remaining -= 1;
        }
    }

    fn tick_one(&mut self, cart: &mut dyn Mapper) {
        let rendering = self.rendering();
        match (self.scanline, self.dot) {
            (0..=239, 256) if rendering => {
                self.render_scanline(self.scanline as usize, cart);
                self.increment_y();
            }
            (0..=239, 256) => self.render_backdrop(self.scanline as usize),
            (0..=239, 257) | (261, 257) if rendering => self.copy_x(),
            // MMC3 sees PPU A12 rise here, when sprite pattern fetches begin.
            (0..=239, 260) | (261, 260) if rendering => cart.scanline(),
            (241, 1) => {
                self.status |= 0x80;
                self.frame_complete = true;
                if self.ctrl & 0x80 != 0 {
                    self.nmi = true;
                }
            }
            (261, 1) => self.status &= !0xE0,
            (261, 280) if rendering => self.copy_y(),
            _ => {}
        }

        self.dot += 1;
        // Odd frames skip the last dot of the pre-render line when rendering.
        if self.scanline == 261 && self.dot == 340 && self.odd_frame && rendering {
            self.dot = 341;
        }
        if self.dot > 340 {
            self.dot = 0;
            self.scanline += 1;
            if self.scanline > 261 {
                self.scanline = 0;
                self.odd_frame = !self.odd_frame;
            }
        }
    }

    fn render_backdrop(&mut self, y: usize) {
        let c = PALETTE[self.palette[0] as usize & 0x3F];
        self.frame[y * WIDTH..(y + 1) * WIDTH].fill(c);
    }

    fn render_scanline(&mut self, y: usize, cart: &mut dyn Mapper) {
        cart.ppu_fetch(false, self.ctrl & 0x20 != 0);
        // ---- background: 2-bit pixel + palette select per x ----
        let mut bg = [0u8; WIDTH + 16];
        if self.mask & 0x08 != 0 {
            let mut v = self.v;
            let fine_y = (v >> 12) & 7;
            let table = if self.ctrl & 0x10 != 0 { 0x1000 } else { 0 };
            for tile in 0..33 {
                let nt = self.mem_read(0x2000 | (v & 0x0FFF), cart) as u16;
                let attr_addr = 0x23C0 | (v & 0x0C00) | ((v >> 4) & 0x38) | ((v >> 2) & 0x07);
                let attr = self.mem_read(attr_addr, cart);
                let shift = ((v >> 4) & 4) | (v & 2);
                let pal = (attr >> shift) & 3;
                let lo = self.mem_read(table + nt * 16 + fine_y, cart);
                let hi = self.mem_read(table + nt * 16 + fine_y + 8, cart);
                for bit in 0..8 {
                    let p = ((lo >> (7 - bit)) & 1) | (((hi >> (7 - bit)) & 1) << 1);
                    bg[tile * 8 + bit] = if p == 0 { 0 } else { (pal << 2) | p };
                }
                if v & 0x1F == 31 {
                    v &= !0x1F;
                    v ^= 0x0400;
                } else {
                    v += 1;
                }
            }
        }
        let fx = self.fine_x as usize;

        // ---- sprites: evaluate up to 8 on this line ----
        let tall = self.ctrl & 0x20 != 0;
        cart.ppu_fetch(true, tall);
        let height = if tall { 16 } else { 8 };
        let mut spr = [0u8; WIDTH]; // palette|pixel (0 = transparent)
        let mut spr_behind = [false; WIDTH];
        let mut spr_zero = [false; WIDTH];
        if self.mask & 0x10 != 0 {
            let mut count = 0;
            for i in 0..64 {
                let sy = self.oam[i * 4] as usize;
                let row = y as isize - sy as isize - 1;
                if row < 0 || row >= height as isize {
                    continue;
                }
                count += 1;
                if count > 8 {
                    self.status |= 0x20;
                    break;
                }
                let tile = self.oam[i * 4 + 1] as u16;
                let attr = self.oam[i * 4 + 2];
                let sx = self.oam[i * 4 + 3] as usize;
                let mut row = row as u16;
                if attr & 0x80 != 0 {
                    row = height as u16 - 1 - row;
                }
                let addr = if tall {
                    let bank = (tile & 1) * 0x1000;
                    let mut t = tile & 0xFE;
                    if row >= 8 {
                        t += 1;
                        row -= 8;
                    }
                    bank + t * 16 + row
                } else {
                    let bank = if self.ctrl & 0x08 != 0 { 0x1000 } else { 0 };
                    bank + tile * 16 + row
                };
                let lo = self.mem_read(addr, cart);
                let hi = self.mem_read(addr + 8, cart);
                for col in 0..8 {
                    let x = sx + col;
                    if x >= WIDTH || spr[x] != 0 {
                        continue;
                    }
                    let bit = if attr & 0x40 != 0 { col } else { 7 - col };
                    let p = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
                    if p != 0 {
                        spr[x] = 0x10 | ((attr & 3) << 2) | p;
                        spr_behind[x] = attr & 0x20 != 0;
                        spr_zero[x] = i == 0;
                    }
                }
            }
        }

        // ---- compose ----
        let show_bg_left = self.mask & 0x02 != 0;
        let show_spr_left = self.mask & 0x04 != 0;
        for x in 0..WIDTH {
            let mut b = bg[x + fx];
            let mut s = spr[x];
            if x < 8 {
                if !show_bg_left {
                    b = 0;
                }
                if !show_spr_left {
                    s = 0;
                }
            }
            if spr_zero[x] && s != 0 && b != 0 && x != 255 {
                self.status |= 0x40;
            }
            let idx = match (b != 0, s != 0) {
                (false, false) => 0,
                (true, false) => b,
                (false, true) => s,
                (true, true) => if spr_behind[x] { b } else { s },
            };
            self.frame[y * WIDTH + x] = PALETTE[self.palette[Self::palette_index(idx as u16)] as usize & 0x3F];
        }
    }
}
