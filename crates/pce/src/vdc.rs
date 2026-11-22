//! HuC6270 VDC (tiles and sprites in 64 KB of VRAM) and HuC6260 VCE (512-entry colour table).
//! Rendering is per scanline, so games changing scroll mid-frame (raster interrupts) work.

pub const MAX_W: usize = 512;
pub const MAX_H: usize = 242;
pub const LINES: u32 = 263;
/// First line of the active display (VDC timing as most games program it).
const TOP: u32 = 14;

#[derive(state::Stateful)]
pub struct Vdc {
    pub vram: Vec<u16>,
    regs: [u16; 20],
    reg_sel: usize,
    status: u8,
    satb: [u16; 256],
    satb_pending: bool,
    read_buf: u16,
    /// Scroll latched at the start of each line.
    bg_y: u16,
    pub line: u32,
    pub irq: bool,
    // VCE
    cram: [u16; 512],
    vce_addr: u16,
    pub dot_clock: u8,
    palette: [u32; 512],
    pub width: usize,
    pub height: usize,
    #[state(skip)]
    pub frame: Vec<u32>,
}

impl Default for Vdc {
    fn default() -> Self {
        Self::new()
    }
}

impl Vdc {
    pub fn new() -> Vdc {
        let mut palette = [0u32; 512];
        for (i, c) in palette.iter_mut().enumerate() {
            let (b, r, g) = (i & 7, (i >> 3) & 7, (i >> 6) & 7);
            let s = |v: usize| (v * 255 / 7) as u32;
            *c = s(r) << 16 | s(g) << 8 | s(b);
        }
        Vdc {
            vram: vec![0; 0x8000],
            regs: [0; 20],
            reg_sel: 0,
            status: 0,
            satb: [0; 256],
            satb_pending: false,
            read_buf: 0,
            bg_y: 0,
            line: 0,
            irq: false,
            cram: [0; 512],
            vce_addr: 0,
            dot_clock: 0,
            palette,
            width: 256,
            height: 224,
            frame: vec![0; MAX_W * MAX_H],
        }
    }

    // ---------------------------------------------------------------- VDC ports

    pub fn read(&mut self, port: u32) -> u8 {
        match port & 3 {
            0 => {
                let s = self.status;
                self.status = 0;
                self.update_irq();
                s
            }
            2 => self.read_buf as u8,
            3 => {
                let v = (self.read_buf >> 8) as u8;
                if self.reg_sel == 2 {
                    self.regs[1] = self.regs[1].wrapping_add(self.increment());
                    self.read_buf = self.vram[(self.regs[1] & 0x7FFF) as usize];
                }
                v
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, port: u32, v: u8) {
        match port & 3 {
            0 => self.reg_sel = (v & 0x1F) as usize,
            2 => {
                if let Some(r) = self.regs.get_mut(self.reg_sel) {
                    *r = (*r & 0xFF00) | v as u16;
                }
            }
            3 => {
                let r = self.reg_sel;
                if r >= self.regs.len() {
                    return;
                }
                self.regs[r] = (self.regs[r] & 0x00FF) | (v as u16) << 8;
                match r {
                    1 => self.read_buf = self.vram[(self.regs[1] & 0x7FFF) as usize],
                    2 => {
                        let a = self.regs[0];
                        self.vram[(a & 0x7FFF) as usize] = self.regs[2];
                        self.regs[0] = a.wrapping_add(self.increment());
                    }
                    8 => self.bg_y = self.regs[8],
                    18 => self.vram_dma(),
                    19 => self.satb_pending = true,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn increment(&self) -> u16 {
        match (self.regs[5] >> 11) & 3 {
            0 => 1,
            1 => 32,
            2 => 64,
            _ => 128,
        }
    }

    fn vram_dma(&mut self) {
        let dcr = self.regs[15];
        let (mut src, mut dst) = (self.regs[16], self.regs[17]);
        let len = self.regs[18] as u32 + 1;
        for _ in 0..len {
            self.vram[(dst & 0x7FFF) as usize] = self.vram[(src & 0x7FFF) as usize];
            src = if dcr & 0x08 != 0 { src.wrapping_sub(1) } else { src.wrapping_add(1) };
            dst = if dcr & 0x04 != 0 { dst.wrapping_sub(1) } else { dst.wrapping_add(1) };
        }
        self.regs[16] = src;
        self.regs[17] = dst;
        if dcr & 0x02 != 0 {
            self.status |= 0x10;
            self.update_irq();
        }
    }

    fn update_irq(&mut self) {
        let cr = self.regs[5];
        self.irq = (self.status & 0x01 != 0 && cr & 0x01 != 0)
            || (self.status & 0x02 != 0 && cr & 0x02 != 0)
            || (self.status & 0x04 != 0 && cr & 0x04 != 0)
            || (self.status & 0x20 != 0 && cr & 0x08 != 0)
            || (self.status & 0x18 != 0);
    }

    // ---------------------------------------------------------------- VCE ports ($0400-$07FF)

    pub fn vce_read(&mut self, port: u32) -> u8 {
        match port & 7 {
            4 => self.cram[(self.vce_addr & 0x1FF) as usize] as u8,
            5 => {
                let v = (self.cram[(self.vce_addr & 0x1FF) as usize] >> 8) as u8 | 0xFE;
                self.vce_addr = (self.vce_addr + 1) & 0x1FF;
                v
            }
            _ => 0xFF,
        }
    }

    pub fn vce_write(&mut self, port: u32, v: u8) {
        match port & 7 {
            0 => self.dot_clock = v & 3,
            2 => self.vce_addr = (self.vce_addr & 0x100) | v as u16,
            3 => self.vce_addr = (self.vce_addr & 0xFF) | ((v as u16 & 1) << 8),
            4 => {
                let a = (self.vce_addr & 0x1FF) as usize;
                self.cram[a] = (self.cram[a] & 0x100) | v as u16;
            }
            5 => {
                let a = (self.vce_addr & 0x1FF) as usize;
                self.cram[a] = (self.cram[a] & 0xFF) | ((v as u16 & 1) << 8);
                self.vce_addr = (self.vce_addr + 1) & 0x1FF;
            }
            _ => {}
        }
    }

    fn colour(&self, i: usize) -> u32 {
        self.palette[(self.cram[i & 0x1FF] & 0x1FF) as usize]
    }

    // ---------------------------------------------------------------- timing and rendering

    fn display_width(&self) -> usize {
        (((self.regs[11] & 0x7F) as usize + 1) * 8).clamp(128, MAX_W)
    }

    fn display_height(&self) -> usize {
        (((self.regs[13] & 0x1FF) as usize) + 1).clamp(128, MAX_H)
    }

    /// Advances one scanline; returns true when a frame has been completed.
    pub fn end_line(&mut self) -> bool {
        let line = self.line;
        let active = line >= TOP && ((line - TOP) as usize) < self.height;
        // Raster compare: RCR counts from 64 at the first active line.
        let rcr = (self.regs[6] & 0x3FF) as u32;
        if active && rcr >= 64 && line - TOP == rcr - 64 {
            self.status |= 0x04;
        }
        if active {
            self.render_line((line - TOP) as usize);
            self.bg_y = self.bg_y.wrapping_add(1);
        }
        let mut frame_done = false;
        if line == TOP + self.height as u32 {
            self.status |= 0x20;
            if self.satb_pending || self.regs[15] & 0x10 != 0 {
                let src = self.regs[19] as usize;
                for i in 0..256 {
                    self.satb[i] = self.vram[(src + i) & 0x7FFF];
                }
                self.satb_pending = false;
                if self.regs[15] & 0x01 != 0 {
                    self.status |= 0x08;
                }
            }
            frame_done = true;
        }
        self.update_irq();
        self.line = (line + 1) % LINES;
        if self.line == 0 {
            self.width = self.display_width();
            self.height = self.display_height();
            self.bg_y = self.regs[8];
        }
        if self.line == TOP {
            self.bg_y = self.regs[8];
        }
        frame_done
    }

    fn bg_pixel(&self, x: u32, y: u32) -> u8 {
        let mwr = self.regs[9];
        let map_w = match (mwr >> 4) & 3 {
            0 => 32,
            1 => 64,
            _ => 128,
        };
        let map_h = if mwr & 0x40 != 0 { 64 } else { 32 };
        let tx = ((x / 8) % map_w) as usize;
        let ty = ((y / 8) % map_h) as usize;
        let entry = self.vram[(ty * map_w as usize + tx) & 0x7FFF];
        let tile = (entry & 0x7FF) as usize;
        let pal = (entry >> 12) as u8;
        let row = (y % 8) as usize;
        let w01 = self.vram[(tile * 16 + row) & 0x7FFF];
        let w23 = self.vram[(tile * 16 + 8 + row) & 0x7FFF];
        let bit = 7 - (x % 8);
        let c = ((w01 >> bit) & 1) | ((w01 >> (8 + bit)) & 1) << 1 | ((w23 >> bit) & 1) << 2 | ((w23 >> (8 + bit)) & 1) << 3;
        if c == 0 {
            0
        } else {
            pal << 4 | c as u8
        }
    }

    fn render_line(&mut self, y: usize) {
        let w = self.width;
        let cr = self.regs[5];
        let backdrop = self.colour(0);
        let mut line = vec![backdrop; w];
        let mut bg_opaque = vec![false; w];
        if cr & 0x80 != 0 {
            let sx = self.regs[7] as u32;
            let sy = self.bg_y as u32;
            for (x, px) in line.iter_mut().enumerate() {
                let c = self.bg_pixel(sx + x as u32, sy);
                if c != 0 {
                    *px = self.colour(c as usize);
                    bg_opaque[x] = true;
                }
            }
        }
        if cr & 0x40 != 0 {
            // Sprites: lower index wins; up to 16 per line.
            let mut drawn = vec![false; w];
            let mut count = 0;
            for i in 0..64 {
                let e = &self.satb[i * 4..i * 4 + 4];
                let sy = (e[0] & 0x3FF) as i32 - 64;
                let sx = (e[1] & 0x3FF) as i32 - 32;
                let attr = e[3];
                let h = match (attr >> 12) & 3 {
                    0 => 16,
                    1 => 32,
                    _ => 64,
                };
                let wide = attr & 0x100 != 0;
                let sw = if wide { 32 } else { 16 };
                let row = y as i32 - sy;
                if !(0..h).contains(&row) {
                    continue;
                }
                count += 1;
                if count > 16 {
                    self.status |= 0x02;
                    break;
                }
                let (xflip, yflip) = (attr & 0x800 != 0, attr & 0x8000 != 0);
                let row = if yflip { h - 1 - row } else { row } as usize;
                let mut pattern = ((e[2] >> 1) & 0x3FF) as usize;
                if wide {
                    pattern &= !1;
                }
                if h >= 32 {
                    pattern &= !2;
                }
                if h == 64 {
                    pattern &= !4;
                }
                let pal = 16 + ((attr & 0xF) as usize) * 16;
                let front = attr & 0x80 != 0;
                for px in 0..sw {
                    let x = sx + px as i32;
                    if !(0..w as i32).contains(&x) {
                        continue;
                    }
                    let col = if xflip { sw - 1 - px } else { px };
                    let cell = pattern + (col / 16) + (row / 16) * 2;
                    let base = cell * 64 + row % 16;
                    let bit = 15 - (col % 16);
                    let mut c = 0usize;
                    for plane in 0..4 {
                        c |= (((self.vram[(base + plane * 16) & 0x7FFF] >> bit) & 1) as usize) << plane;
                    }
                    if c == 0 {
                        continue;
                    }
                    let xi = x as usize;
                    if drawn[xi] {
                        if i == 0 || count == 1 {
                            self.status |= 0x01;
                        }
                        continue;
                    }
                    drawn[xi] = true;
                    if front || !bg_opaque[xi] {
                        line[xi] = self.colour(pal + c);
                    }
                }
            }
        }
        if y < MAX_H {
            self.frame[y * MAX_W..y * MAX_W + w].copy_from_slice(&line);
        }
    }

    /// The visible picture, packed to `width` x `height`.
    pub fn picture(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.width * self.height);
        for y in 0..self.height {
            out.extend_from_slice(&self.frame[y * MAX_W..y * MAX_W + self.width]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(v: &mut Vdc, r: u8, val: u16) {
        v.write(0, r);
        v.write(2, val as u8);
        v.write(3, (val >> 8) as u8);
    }

    #[test]
    fn background_tile_colour() {
        let mut v = Vdc::new();
        // Colour 0x11 (palette 1, colour 1) = pure red (GRB 0b000_111_000).
        v.vce_write(2, 0x11);
        v.vce_write(3, 0);
        v.vce_write(4, 0x38);
        v.vce_write(5, 0);
        reg(&mut v, 5, 0x80); // background on
        reg(&mut v, 11, 31); // 256 wide
        reg(&mut v, 13, 223);
        // BAT (0,0) = tile 0x100, palette 1.
        reg(&mut v, 0, 0);
        reg(&mut v, 2, 0x1100);
        // Tile 0x100 row 0: plane 0 all set.
        reg(&mut v, 0, 0x1000);
        reg(&mut v, 2, 0x00FF);
        for _ in 0..LINES * 2 {
            v.end_line();
        }
        let pic = v.picture();
        assert_eq!(v.width, 256);
        assert_eq!(pic[0], 0xFF0000);
        assert_eq!(pic[8], v.colour(0), "next tile is empty");
    }

    #[test]
    fn vblank_irq_when_enabled() {
        let mut v = Vdc::new();
        reg(&mut v, 5, 0x08);
        reg(&mut v, 13, 223);
        let mut seen = false;
        for _ in 0..LINES * 2 {
            v.end_line();
            seen |= v.irq;
        }
        assert!(seen);
        assert_eq!(v.read(0) & 0x20, 0x20);
        assert!(!v.irq, "reading status clears it");
    }
}
