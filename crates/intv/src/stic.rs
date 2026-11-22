//! STIC (AY-3-8900): a 20 x 12 grid of 8x8 background cards read from BACKTAB, drawn from
//! the GROM (built-in shapes) or GRAM (64 game-defined shapes), plus 8 moving objects (MOBs)
//! with collision detection. MOBs have twice the vertical resolution of the background, so the
//! frame is rendered as 160 x 192 (each background row covers two lines).

pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 192;

pub const PALETTE: [u32; 16] = [
    0x0C0005, 0x002DFF, 0xFF3D10, 0xC9CFAB, 0x386B3F, 0x00A756, 0xFAEA50, 0xFFFCFF, 0xBDACC8, 0x24B8FF, 0xFFB41F, 0x546E00, 0xFF4E57, 0xA496FF, 0x75CC80, 0xB51A58,
];

pub struct Stic {
    pub regs: [u16; 0x40],
    /// Written during this vertical blank: display on for the next frame.
    pub display_enable: bool,
    /// Foreground/background mode (set by writing $21) vs colour-stack mode (reading $21).
    pub fgbg: bool,
    pub frame: Vec<u32>,
}

impl Default for Stic {
    fn default() -> Self {
        Self::new()
    }
}

impl Stic {
    pub fn new() -> Stic {
        Stic { regs: [0; 0x40], display_enable: false, fgbg: false, frame: vec![0; WIDTH * HEIGHT] }
    }

    pub fn read(&mut self, a: u16) -> u16 {
        let r = (a & 0x3F) as usize;
        match r {
            0x21 => {
                self.fgbg = false;
                0x3FFF
            }
            0x00..=0x07 => self.regs[r] | 0x3800,
            0x08..=0x0F => self.regs[r] | 0x3000,
            0x10..=0x17 => self.regs[r] | 0x0000,
            0x18..=0x1F => self.regs[r] | 0x3C00,
            _ => self.regs[r],
        }
    }

    pub fn write(&mut self, a: u16, v: u16) {
        let r = (a & 0x3F) as usize;
        match r {
            0x20 => self.display_enable = true,
            0x21 => self.fgbg = true,
            0x00..=0x07 => self.regs[r] = v & 0x7FF,
            0x08..=0x0F => self.regs[r] = v & 0xFFF,
            0x10..=0x17 => self.regs[r] = v & 0x3FFF,
            0x18..=0x1F => self.regs[r] = v & 0x3FF,
            0x28..=0x2C => self.regs[r] = v & 0xF,
            0x30 | 0x31 => self.regs[r] = v & 7,
            0x32 => self.regs[r] = v & 3,
            _ => {}
        }
    }

    /// Row `y` (0-7) of a card's 8-pixel pattern.
    fn card_row(grom: &[u8], gram: &[u8], card: u16, gram_sel: bool, y: usize) -> u8 {
        if gram_sel {
            gram[((card as usize & 0x3F) * 8 + y) & 0x1FF]
        } else {
            *grom.get((card as usize & 0xFF) * 8 + y).unwrap_or(&0)
        }
    }

    /// Renders one frame from BACKTAB ($200-$2EF), GROM and GRAM.
    pub fn render(&mut self, backtab: &[u16], grom: &[u8], gram: &[u8]) {
        let border = PALETTE[self.regs[0x2C] as usize & 15];
        if !self.display_enable {
            self.frame.fill(PALETTE[0]);
            return;
        }
        // Background colour of each pixel (index), and whether it's foreground (for MOB priority/collisions).
        let mut bg = vec![0u8; 160 * 96];
        let mut fg = vec![false; 160 * 96];
        let mut stack = 0usize;
        for row in 0..12 {
            for col in 0..20 {
                let w = backtab[row * 20 + col];
                let gram_sel = w & 0x800 != 0;
                let (fgc, bgc, card, squares);
                if self.fgbg {
                    fgc = (w & 7) as u8;
                    bgc = ((w >> 9) & 3 | ((w >> 13) & 1) << 2 | ((w >> 12) & 1) << 3) as u8;
                    card = (w >> 3) & 0x3F;
                    squares = false;
                } else {
                    if w & 0x2000 != 0 {
                        stack = (stack + 1) & 3;
                    }
                    squares = w & 0x1800 == 0x1000;
                    fgc = ((w & 7) | if gram_sel { (w >> 9) & 8 } else { 0 }) as u8;
                    bgc = self.regs[0x28 + stack] as u8;
                    card = (w >> 3) & 0xFF;
                }
                for y in 0..8 {
                    let bits = if squares { 0 } else { Self::card_row(grom, gram, card, gram_sel, y) };
                    for x in 0..8 {
                        let (c, is_fg) = if squares {
                            let q = (y / 4) * 2 + x / 4;
                            let col = match q {
                                0 => w & 7,
                                1 => (w >> 3) & 7,
                                2 => (w >> 6) & 7,
                                _ => (w >> 9) & 3 | ((w >> 13) & 1) << 2,
                            } as u8;
                            if col == 7 {
                                (self.regs[0x28 + stack] as u8, false)
                            } else {
                                (col, true)
                            }
                        } else if bits & (0x80 >> x) != 0 {
                            (fgc, true)
                        } else {
                            (bgc, false)
                        };
                        let i = (row * 8 + y) * 160 + col * 8 + x;
                        bg[i] = c & 15;
                        fg[i] = is_fg;
                    }
                }
            }
        }
        // Border extension masks the left column / top row.
        let (dx, dy) = (self.regs[0x30] as usize, self.regs[0x31] as usize);
        let ext = self.regs[0x32];
        let mut px = vec![0u32; WIDTH * HEIGHT];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let (sx, sy) = (x as isize - dx as isize, y as isize / 2 - dy as isize);
                let masked = (ext & 1 != 0 && x < 8) || (ext & 2 != 0 && y < 16) || sx < 0 || sy < 0;
                px[y * WIDTH + x] = if masked { border } else { PALETTE[bg[sy as usize * 160 + sx as usize] as usize] };
            }
        }
        // MOBs, lowest priority (7) first so MOB 0 ends on top.
        let mut hit = [0u16; 8];
        let mut cover = vec![0u8; WIDTH * HEIGHT];
        for m in (0..8).rev() {
            let (xr, yr, ar) = (self.regs[m], self.regs[8 + m], self.regs[0x10 + m]);
            let visible = xr & 0x200 != 0;
            let interact = xr & 0x100 != 0;
            if !visible && !interact {
                continue;
            }
            let x0 = (xr & 0xFF) as isize - 8 + dx as isize;
            let y0 = ((yr & 0x7F) as isize - 8 + dy as isize) * 2;
            let tall = yr & 0x80 != 0;
            let ysize = 1 << ((yr >> 8) & 3); // 1, 2, 4, 8 lines per MOB row
            let xwide = if xr & 0x400 != 0 { 2 } else { 1 };
            let (xflip, yflip) = (yr & 0x400 != 0, yr & 0x800 != 0);
            let gram_sel = ar & 0x800 != 0;
            let mut card = (ar >> 3) & if gram_sel { 0x3F } else { 0xFF };
            if tall {
                card &= !1;
            }
            let color = PALETTE[((ar & 7) | (ar >> 9) & 8) as usize];
            let behind = ar & 0x2000 != 0;
            let rows = if tall { 16 } else { 8 };
            for r in 0..rows {
                let src = if yflip { rows - 1 - r } else { r };
                let bits = Self::card_row(grom, gram, card + (src / 8) as u16, gram_sel, src % 8);
                for line in 0..ysize {
                    let y = y0 + (r * ysize + line) as isize;
                    if !(0..HEIGHT as isize).contains(&y) {
                        continue;
                    }
                    for b in 0..8 {
                        let bit = if xflip { 0x01 << b } else { 0x80 >> b };
                        if bits & bit == 0 {
                            continue;
                        }
                        for w in 0..xwide {
                            let x = x0 + (b * xwide + w) as isize;
                            if !(0..WIDTH as isize).contains(&x) {
                                if interact {
                                    hit[m] |= 0x200; // border
                                }
                                continue;
                            }
                            let i = y as usize * WIDTH + x as usize;
                            if interact {
                                let others = cover[i];
                                for o in 0..8 {
                                    if others & (1 << o) != 0 {
                                        hit[m] |= 1 << o;
                                        hit[o] |= 1 << m;
                                    }
                                }
                                let (sx, sy) = (x - dx as isize, y / 2 - dy as isize);
                                if sx >= 0 && sy >= 0 && fg[sy as usize * 160 + sx as usize] {
                                    hit[m] |= 0x100;
                                }
                                cover[i] |= 1 << m;
                            }
                            if visible {
                                let (sx, sy) = (x - dx as isize, y / 2 - dy as isize);
                                let over_fg = sx >= 0 && sy >= 0 && fg[sy as usize * 160 + sx as usize];
                                if !(behind && over_fg) {
                                    px[i] = color;
                                }
                            }
                        }
                    }
                }
            }
        }
        for m in 0..8 {
            self.regs[0x18 + m] |= hit[m];
        }
        self.frame = px;
        self.display_enable = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_stack_card_and_mob() {
        let mut s = Stic::new();
        let mut grom = vec![0u8; 2048];
        grom[1 * 8..1 * 8 + 8].fill(0xF0); // card 1: left half set
        let gram = vec![0xFFu8; 512];
        let mut backtab = vec![0u16; 240];
        backtab[0] = 1 << 3 | 2; // card 1, red
        s.write(0x28, 1); // stack colour 0: blue
        s.write(0x20, 0);
        // MOB 0: GRAM card 0 (solid), yellow, visible at screen (0, 0) => X = 8, Y = 8.
        s.write(0x00, 0x200 | 0x100 | 8);
        s.write(0x08, 8);
        s.write(0x10, 0x800 | 6);
        // MOB 1 overlapping MOB 0, interact only.
        s.write(0x01, 0x100 | 10);
        s.write(0x09, 8);
        s.write(0x11, 0x800);
        s.render(&backtab, &grom, &gram);
        assert_eq!(s.frame[0], PALETTE[6], "MOB drawn over the card");
        assert_eq!(s.frame[12 * WIDTH + 3], PALETTE[2], "card foreground below the MOB (line 12)");
        assert_eq!(s.frame[12 * WIDTH + 6], PALETTE[1], "stack colour behind");
        assert_eq!(s.regs[0x18] & 0x102, 0x102, "MOB0 hit MOB1 and the background");
    }
}
