//! Sega Master System / Game Gear VDP in Mode 4: 16 KB VRAM, a 32 x 28 tile map with
//! horizontal/vertical scrolling, 64 sprites (8 per line), two 16-colour palettes, and the
//! frame/line interrupts. The Game Gear shows the centre 160 x 144 with 12-bit colour.

pub const SMS_W: usize = 256;
pub const SMS_H: usize = 192;
pub const GG_W: usize = 160;
pub const GG_H: usize = 144;
pub const LINES: u32 = 262;

#[derive(state::Stateful)]
pub struct Vdp {
    pub vram: Vec<u8>,
    cram: [u8; 64],
    pub regs: [u8; 16],
    addr: u16,
    code: u8,
    latch: Option<u8>,
    buffer: u8,
    cram_latch: u8,
    status: u8,
    line_counter: u8,
    line_irq: bool,
    pub line: u32,
    #[state(skip)]
    pub gg: bool,
    /// 256 x 192 (SMS) or 160 x 144 (Game Gear) 0RGB pixels.
    #[state(skip)]
    pub frame: Vec<u32>,
}

impl Vdp {
    pub fn new(gg: bool) -> Vdp {
        let (w, h) = if gg { (GG_W, GG_H) } else { (SMS_W, SMS_H) };
        let mut regs = [0u8; 16];
        // Defaults the BIOS leaves behind.
        regs[0] = 0x36;
        regs[1] = 0x80;
        regs[2] = 0xFF;
        regs[5] = 0xFF;
        regs[6] = 0xFB;
        regs[10] = 0xFF;
        Vdp { vram: vec![0; 0x4000], cram: [0; 64], regs, addr: 0, code: 0, latch: None, buffer: 0, cram_latch: 0, status: 0, line_counter: 0xFF, line_irq: false, line: 0, gg, frame: vec![0; w * h] }
    }

    pub fn size(&self) -> (usize, usize) {
        if self.gg {
            (GG_W, GG_H)
        } else {
            (SMS_W, SMS_H)
        }
    }

    // ---------------------------------------------------------------- ports

    pub fn write_control(&mut self, v: u8) {
        match self.latch.take() {
            None => {
                self.latch = Some(v);
                self.addr = (self.addr & 0x3F00) | v as u16;
            }
            Some(lo) => {
                self.addr = (lo as u16 | (v as u16 & 0x3F) << 8) & 0x3FFF;
                self.code = v >> 6;
                match self.code {
                    0 => {
                        self.buffer = self.vram[self.addr as usize];
                        self.addr = (self.addr + 1) & 0x3FFF;
                    }
                    2 => {
                        self.regs[(v & 0xF) as usize] = lo;
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn write_data(&mut self, v: u8) {
        self.latch = None;
        if self.code == 3 {
            if self.gg {
                if self.addr & 1 == 0 {
                    self.cram_latch = v;
                } else {
                    let i = (self.addr & 0x3E) as usize;
                    self.cram[i] = self.cram_latch;
                    self.cram[i + 1] = v & 0x0F;
                }
            } else {
                self.cram[(self.addr & 0x1F) as usize] = v & 0x3F;
            }
        } else {
            self.vram[self.addr as usize] = v;
        }
        self.buffer = v;
        self.addr = (self.addr + 1) & 0x3FFF;
    }

    pub fn read_data(&mut self) -> u8 {
        self.latch = None;
        let v = self.buffer;
        self.buffer = self.vram[self.addr as usize];
        self.addr = (self.addr + 1) & 0x3FFF;
        v
    }

    pub fn read_status(&mut self) -> u8 {
        self.latch = None;
        let s = self.status | 0x1F;
        self.status = 0;
        self.line_irq = false;
        s
    }

    /// V counter for NTSC 192-line mode (0x00-0xDA, then 0xD5-0xFF).
    pub fn v_counter(&self) -> u8 {
        if self.line <= 0xDA {
            self.line as u8
        } else {
            (self.line - 6) as u8
        }
    }

    /// The Z80's /INT line: frame or line interrupt pending and enabled.
    pub fn irq(&self) -> bool {
        (self.status & 0x80 != 0 && self.regs[1] & 0x20 != 0) || (self.line_irq && self.regs[0] & 0x10 != 0)
    }

    // ---------------------------------------------------------------- colours

    fn colour(&self, i: usize) -> u32 {
        if self.gg {
            let lo = self.cram[i * 2] as u32;
            let hi = self.cram[i * 2 + 1] as u32;
            let (r, g, b) = (lo & 0xF, lo >> 4, hi & 0xF);
            (r * 17) << 16 | (g * 17) << 8 | b * 17
        } else {
            let c = self.cram[i] as u32;
            let (r, g, b) = (c & 3, (c >> 2) & 3, (c >> 4) & 3);
            (r * 85) << 16 | (g * 85) << 8 | b * 85
        }
    }

    // ---------------------------------------------------------------- rendering

    /// Advances to the next line: renders visible lines, raises the line and frame interrupts.
    pub fn end_line(&mut self) {
        let line = self.line;
        if line < SMS_H as u32 {
            self.render_line(line as usize);
        }
        if line <= SMS_H as u32 {
            if self.line_counter == 0 {
                self.line_counter = self.regs[10];
                self.line_irq = true;
            } else {
                self.line_counter -= 1;
            }
        } else {
            self.line_counter = self.regs[10];
        }
        if line == SMS_H as u32 {
            self.status |= 0x80;
        }
        self.line = (line + 1) % LINES;
    }

    fn tile_pixel(&self, pattern: usize, x: usize, y: usize) -> usize {
        let base = (pattern * 32 + y * 4) & 0x3FFF;
        let bit = 7 - x;
        let mut c = 0;
        for plane in 0..4 {
            c |= ((self.vram[(base + plane) & 0x3FFF] >> bit) & 1) << plane;
        }
        c as usize
    }

    fn render_line(&mut self, y: usize) {
        let mut line = [0u32; 256];
        let backdrop = self.colour(16 + (self.regs[7] & 0xF) as usize);
        if self.regs[1] & 0x40 == 0 {
            line.fill(backdrop);
            self.store_line(y, &line);
            return;
        }
        // Background.
        let name_base = ((self.regs[2] as usize & 0x0E) << 10) & 0x3FFF;
        let hscroll = if self.regs[0] & 0x40 != 0 && y < 16 { 0 } else { self.regs[8] as usize };
        let mut prio = [false; 256];
        let mut opaque = [false; 256];
        for x in 0..256 {
            let vscroll = if self.regs[0] & 0x80 != 0 && x >= 192 { 0 } else { self.regs[9] as usize };
            let sx = (x + 256 - hscroll) & 0xFF;
            let sy = (y + vscroll) % 224;
            let entry_addr = name_base + ((sy / 8) * 32 + sx / 8) * 2;
            let entry = self.vram[entry_addr & 0x3FFF] as u16 | (self.vram[(entry_addr + 1) & 0x3FFF] as u16) << 8;
            let tile = (entry & 0x1FF) as usize;
            let (hf, vf) = (entry & 0x200 != 0, entry & 0x400 != 0);
            let pal = if entry & 0x800 != 0 { 16 } else { 0 };
            let (tx, ty) = (if hf { 7 - sx % 8 } else { sx % 8 }, if vf { 7 - sy % 8 } else { sy % 8 });
            let c = self.tile_pixel(tile, tx, ty);
            line[x] = self.colour(pal + c);
            opaque[x] = c != 0;
            prio[x] = entry & 0x1000 != 0 && c != 0;
        }
        // Sprites: up to 8 on this line, earlier entries win.
        let sat = ((self.regs[5] as usize & 0x7E) << 7) & 0x3FFF;
        let tall = self.regs[1] & 0x02 != 0;
        let zoom = if self.regs[1] & 0x01 != 0 { 2 } else { 1 };
        let h = if tall { 16 } else { 8 } * zoom;
        let pat_base = if self.regs[6] & 0x04 != 0 { 256 } else { 0 };
        let shift = if self.regs[0] & 0x08 != 0 { 8 } else { 0 };
        let mut drawn = [false; 256];
        let mut count = 0;
        for i in 0..64 {
            let sy = self.vram[sat + i] as usize;
            if sy == 0xD0 {
                break;
            }
            let top = (sy + 1) & 0xFF;
            let row = (y + 256 - top) & 0xFF;
            if row >= h {
                continue;
            }
            count += 1;
            if count > 8 {
                self.status |= 0x40;
                break;
            }
            let sx = self.vram[sat + 0x80 + i * 2] as isize - shift;
            let mut tile = self.vram[sat + 0x81 + i * 2] as usize;
            if tall {
                tile &= !1;
            }
            let r = row / zoom;
            let pattern = pat_base + tile + r / 8;
            for px in 0..8 * zoom {
                let x = sx + px as isize;
                if !(0..256).contains(&x) {
                    continue;
                }
                let x = x as usize;
                let c = self.tile_pixel(pattern, px / zoom, r % 8);
                if c == 0 {
                    continue;
                }
                if drawn[x] {
                    self.status |= 0x20;
                    continue;
                }
                drawn[x] = true;
                if !prio[x] {
                    line[x] = self.colour(16 + c);
                }
            }
        }
        let _ = opaque;
        if self.regs[0] & 0x20 != 0 {
            line[..8].fill(backdrop);
        }
        self.store_line(y, &line);
    }

    fn store_line(&mut self, y: usize, line: &[u32; 256]) {
        if self.gg {
            // The Game Gear LCD shows columns 48-207 of lines 24-167.
            if (24..24 + GG_H).contains(&y) {
                let row = (y - 24) * GG_W;
                self.frame[row..row + GG_W].copy_from_slice(&line[48..48 + GG_W]);
            }
        } else {
            self.frame[y * SMS_W..(y + 1) * SMS_W].copy_from_slice(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_vram(v: &mut Vdp, addr: u16, data: &[u8]) {
        v.write_control(addr as u8);
        v.write_control(0x40 | (addr >> 8) as u8);
        for &b in data {
            v.write_data(b);
        }
    }

    #[test]
    fn draws_tile_and_sprite() {
        let mut v = Vdp::new(false);
        // Registers: display on, name table at $3800, SAT at $3F00, sprite patterns at $0000.
        for (r, val) in [(0u8, 0x06u8), (1, 0xC0), (2, 0xFF), (5, 0xFF), (6, 0xFB), (7, 0x00)] {
            v.write_control(val);
            v.write_control(0x80 | r);
        }
        // Palette: colour 1 = red, sprite colour 2 = blue.
        v.write_control(0x01);
        v.write_control(0xC0);
        v.write_data(0x03);
        v.write_control(0x12);
        v.write_control(0xC0);
        v.write_data(0x30);
        // Tile 1: plane 0 set on every row -> colour 1.
        let mut t1 = vec![0u8; 32];
        for r in 0..8 {
            t1[r * 4] = 0xFF;
        }
        write_vram(&mut v, 32, &t1);
        // Tile 2: plane 1 set -> colour 2.
        let mut t2 = vec![0u8; 32];
        for r in 0..8 {
            t2[r * 4 + 1] = 0xFF;
        }
        write_vram(&mut v, 64, &t2);
        // Name table entry (0,0) = tile 1.
        write_vram(&mut v, 0x3800, &[1, 0]);
        // Sprite 0 at x=16, y=0 (stored y = -1), tile 2; end marker after it.
        write_vram(&mut v, 0x3F00, &[0xFF, 0xD0]);
        write_vram(&mut v, 0x3F80, &[16, 2]);
        for _ in 0..8 {
            v.end_line();
        }
        assert_eq!(v.frame[0], 0xFF0000, "tile colour 1 = red");
        assert_eq!(v.frame[16], 0x0000FF, "sprite colour 2 = blue");
        assert_eq!(v.frame[8], 0x000000, "empty tile uses palette colour 0 (black)");
    }

    #[test]
    fn frame_interrupt_at_line_192() {
        let mut v = Vdp::new(false);
        v.regs[1] = 0x60;
        for _ in 0..193 {
            v.end_line();
        }
        assert!(v.irq());
        assert_eq!(v.read_status() & 0x80, 0x80);
        assert!(!v.irq(), "reading status acknowledges it");
    }
}
