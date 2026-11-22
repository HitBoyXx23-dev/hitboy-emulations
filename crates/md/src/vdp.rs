//! Genesis VDP (Mode 5): 64 KB VRAM, 64-colour CRAM, vertical scroll RAM, two scrolling
//! planes, the window plane, 80 linked sprites, DMA (68000 transfer, fill, copy) and the
//! horizontal/vertical interrupts. Rendered one scanline at a time.

pub const MAX_W: usize = 320;
pub const HEIGHT: usize = 224;
pub const LINES: u32 = 262;

pub enum DmaRequest {
    /// Copy `words` 16-bit words from 68000 address `src` to the current target.
    FromBus { src: u32, words: u32 },
}

impl Default for DmaRequest {
    fn default() -> Self {
        DmaRequest::FromBus { src: 0, words: 0 }
    }
}

impl state::Stateful for DmaRequest {
    fn sync(&mut self, s: &mut state::State) {
        let DmaRequest::FromBus { src, words } = self;
        src.sync(s);
        words.sync(s);
    }
}

#[derive(state::Stateful)]
pub struct Vdp {
    pub vram: Vec<u8>,
    cram: [u16; 64],
    vsram: [u16; 40],
    pub regs: [u8; 24],
    addr: u16,
    code: u8,
    pending: Option<u16>,
    fill_pending: bool,
    read_buf: u16,
    pub line: u32,
    hint_counter: i32,
    pub vint_pending: bool,
    pub hint_pending: bool,
    pub vblank: bool,
    /// Line output (0RGB), `width` pixels valid.
    #[state(skip)]
    pub frame: Vec<u32>,
    pub width: usize,
}

impl Default for Vdp {
    fn default() -> Self {
        Self::new()
    }
}

fn cram_rgb(c: u16) -> u32 {
    let s = |v: u16| ((v & 7) as u32) * 36;
    s(c >> 1) << 16 | s(c >> 5) << 8 | s(c >> 9)
}

impl Vdp {
    pub fn new() -> Vdp {
        Vdp {
            vram: vec![0; 0x1_0000],
            cram: [0; 64],
            vsram: [0; 40],
            regs: [0; 24],
            addr: 0,
            code: 0,
            pending: None,
            fill_pending: false,
            read_buf: 0,
            line: 0,
            hint_counter: 0,
            vint_pending: false,
            hint_pending: false,
            vblank: false,
            frame: vec![0; MAX_W * HEIGHT],
            width: 320,
        }
    }

    fn h40(&self) -> bool {
        self.regs[12] & 0x81 != 0
    }

    fn inc(&self) -> u16 {
        self.regs[15] as u16
    }

    /// The level-6 (vertical) or level-4 (horizontal) interrupt the 68000 should see.
    pub fn irq_level(&self) -> u8 {
        if self.vint_pending && self.regs[1] & 0x20 != 0 {
            6
        } else if self.hint_pending && self.regs[0] & 0x10 != 0 {
            4
        } else {
            0
        }
    }

    pub fn ack(&mut self, level: u8) {
        if level == 6 {
            self.vint_pending = false;
        } else if level == 4 {
            self.hint_pending = false;
        }
    }

    // ---------------------------------------------------------------- ports

    pub fn read_status(&mut self, line_cycle: u32) -> u16 {
        self.pending = None;
        let mut s = 0x3400u16 | 0x0200; // FIFO empty
        if self.vint_pending {
            s |= 0x80;
        }
        if self.vblank || self.regs[1] & 0x40 == 0 {
            s |= 0x08;
        }
        if line_cycle > 400 {
            s |= 0x04;
        }
        s
    }

    pub fn hv_counter(&self, line_cycle: u32) -> u16 {
        let v = if self.line > 0xEA { self.line - 6 } else { self.line } as u16 & 0xFF;
        let h = ((line_cycle * if self.h40() { 210 } else { 171 }) / 488) as u16 & 0xFF;
        v << 8 | h
    }

    pub fn write_control(&mut self, w: u16) -> Option<DmaRequest> {
        if let Some(first) = self.pending.take() {
            self.addr = (first & 0x3FFF) | ((w & 3) << 14);
            self.code = ((first >> 14) as u8 & 3) | ((w >> 2) as u8 & 0x3C);
            if self.code & 0x20 != 0 && self.regs[1] & 0x10 != 0 {
                return self.start_dma();
            }
            if self.code & 0x0F == 0 || self.code & 0x0F == 4 || self.code & 0x0F == 8 {
                self.prefetch();
            }
            return None;
        }
        if w & 0xC000 == 0x8000 {
            let r = ((w >> 8) & 0x1F) as usize;
            if r < self.regs.len() {
                self.regs[r] = w as u8;
            }
            self.code = 0;
            return None;
        }
        self.pending = Some(w);
        self.addr = (self.addr & 0xC000) | (w & 0x3FFF);
        None
    }

    fn prefetch(&mut self) {
        self.read_buf = match self.code & 0x0F {
            0 => u16::from_be_bytes([self.vram[(self.addr & 0xFFFE) as usize], self.vram[(self.addr | 1) as usize]]),
            4 => self.vsram[((self.addr >> 1) as usize) % 40],
            _ => self.cram[((self.addr >> 1) & 0x3F) as usize],
        };
    }

    fn dma_len(&self) -> u32 {
        let l = self.regs[19] as u32 | (self.regs[20] as u32) << 8;
        if l == 0 {
            0x1_0000
        } else {
            l
        }
    }

    fn start_dma(&mut self) -> Option<DmaRequest> {
        match self.regs[23] >> 6 {
            0 | 1 => {
                let src = ((self.regs[23] as u32 & 0x7F) << 17) | (self.regs[22] as u32) << 9 | (self.regs[21] as u32) << 1;
                Some(DmaRequest::FromBus { src, words: self.dma_len() })
            }
            2 => {
                self.fill_pending = true;
                None
            }
            _ => {
                // VRAM copy (bytes).
                let mut src = (self.regs[21] as u16) | (self.regs[22] as u16) << 8;
                for _ in 0..self.dma_len() {
                    let v = self.vram[src as usize];
                    self.vram[(self.addr ^ 1) as usize] = v;
                    src = src.wrapping_add(1);
                    self.addr = self.addr.wrapping_add(self.inc());
                }
                None
            }
        }
    }

    /// One word of a 68000-to-VDP DMA transfer.
    pub fn dma_word(&mut self, v: u16) {
        self.store(v);
    }

    fn store(&mut self, v: u16) {
        match self.code & 0x0F {
            1 => {
                let a = self.addr as usize;
                let [hi, lo] = v.to_be_bytes();
                // Odd addresses write the bytes swapped.
                if a & 1 == 0 {
                    self.vram[a] = hi;
                    self.vram[a + 1] = lo;
                } else {
                    self.vram[a] = lo;
                    self.vram[a - 1] = hi;
                }
            }
            3 => self.cram[((self.addr >> 1) & 0x3F) as usize] = v & 0x0EEE,
            5 => {
                let i = ((self.addr >> 1) as usize) % 40;
                self.vsram[i] = v & 0x07FF;
            }
            _ => {}
        }
        self.addr = self.addr.wrapping_add(self.inc());
    }

    pub fn write_data(&mut self, v: u16) {
        self.pending = None;
        if self.fill_pending {
            self.fill_pending = false;
            self.store(v);
            let [hi, _] = v.to_be_bytes();
            for _ in 1..self.dma_len() {
                self.vram[(self.addr ^ 1) as usize] = hi;
                self.addr = self.addr.wrapping_add(self.inc());
            }
            return;
        }
        self.store(v);
    }

    pub fn read_data(&mut self) -> u16 {
        self.pending = None;
        let v = self.read_buf;
        self.addr = self.addr.wrapping_add(self.inc());
        self.prefetch();
        v
    }

    // ---------------------------------------------------------------- timing

    /// Called at the end of each scanline; renders visible lines and raises interrupts.
    pub fn end_line(&mut self) {
        let line = self.line;
        if (line as usize) < HEIGHT {
            self.render_line(line as usize);
            self.hint_counter -= 1;
            if self.hint_counter < 0 {
                self.hint_counter = self.regs[10] as i32;
                self.hint_pending = true;
            }
        } else {
            self.hint_counter = self.regs[10] as i32;
        }
        self.line = (line + 1) % LINES;
        if self.line == HEIGHT as u32 {
            self.vblank = true;
            self.vint_pending = true;
        }
        if self.line == 0 {
            self.vblank = false;
            self.width = if self.h40() { 320 } else { 256 };
        }
    }

    // ---------------------------------------------------------------- rendering

    fn plane_size(&self) -> (usize, usize) {
        let sz = |b: u8| match b & 3 {
            0 => 32,
            1 => 64,
            _ => 128,
        };
        (sz(self.regs[16]), sz(self.regs[16] >> 4))
    }

    /// Returns (colour index 0-63 or 0 for transparent, high priority).
    fn tile_pixel(&self, entry: u16, x: usize, y: usize) -> (u8, bool) {
        let tile = (entry & 0x7FF) as usize;
        let (hf, vf) = (entry & 0x800 != 0, entry & 0x1000 != 0);
        let (x, y) = (if hf { 7 - x } else { x }, if vf { 7 - y } else { y });
        let b = self.vram[(tile * 32 + y * 4 + x / 2) & 0xFFFF];
        let c = if x & 1 == 0 { b >> 4 } else { b & 0xF };
        let pal = ((entry >> 13) & 3) as u8;
        (if c == 0 { 0 } else { pal * 16 + c }, entry & 0x8000 != 0)
    }

    fn plane_pixel(&self, base: usize, pw: usize, ph: usize, x: usize, y: usize) -> (u8, bool) {
        let (cx, cy) = ((x / 8) % pw, (y / 8) % ph);
        let a = (base + (cy * pw + cx) * 2) & 0xFFFF;
        let entry = u16::from_be_bytes([self.vram[a], self.vram[(a + 1) & 0xFFFF]]);
        self.tile_pixel(entry, x % 8, y % 8)
    }

    fn hscroll(&self, line: usize) -> (i32, i32) {
        let base = ((self.regs[13] as usize) & 0x3F) << 10;
        let off = match self.regs[11] & 3 {
            0 => 0,
            2 => (line & !7) * 4,
            3 => line * 4,
            _ => (line & 7) * 4,
        };
        let a = (base + off) & 0xFFFF;
        let rd = |a: usize| u16::from_be_bytes([self.vram[a & 0xFFFF], self.vram[(a + 1) & 0xFFFF]]) as i32 & 0x3FF;
        (rd(a), rd(a + 2))
    }

    fn render_line(&mut self, y: usize) {
        let w = if self.h40() { 320 } else { 256 };
        let bg = cram_rgb(self.cram[(self.regs[7] & 0x3F) as usize]);
        let row = &mut vec![bg; w];
        if self.regs[1] & 0x40 == 0 {
            self.frame[y * MAX_W..y * MAX_W + w].copy_from_slice(row);
            return;
        }
        let (pw, ph) = self.plane_size();
        let base_a = ((self.regs[2] as usize) & 0x38) << 10;
        let base_b = ((self.regs[4] as usize) & 0x07) << 13;
        let base_w = if w == 320 { ((self.regs[3] as usize) & 0x3C) << 10 } else { ((self.regs[3] as usize) & 0x3E) << 10 };
        let (hs_a, hs_b) = self.hscroll(y);
        // Window region.
        let wh = self.regs[17];
        let wv = self.regs[18];
        let win_x = (wh & 0x1F) as usize * 16;
        let win_y = (wv & 0x1F) as usize * 8;
        let in_window = |x: usize| {
            let hx = if wh & 0x80 != 0 { x >= win_x } else { x < win_x };
            let vy = if wv & 0x80 != 0 { y >= win_y } else { y < win_y };
            (wh & 0x1F != 0 && hx) || (wv & 0x1F != 0 && vy)
        };
        // Layer pixels: (colour, priority) for B, A (or window).
        let mut layer_b = vec![(0u8, false); w];
        let mut layer_a = vec![(0u8, false); w];
        let col_scroll = self.regs[11] & 4 != 0;
        for x in 0..w {
            let col = x / 16;
            let (va, vb) = if col_scroll { (self.vsram[(col * 2) % 40], self.vsram[(col * 2 + 1) % 40]) } else { (self.vsram[0], self.vsram[1]) };
            let bx = ((x as i32 - hs_b).rem_euclid((pw * 8) as i32)) as usize;
            layer_b[x] = self.plane_pixel(base_b, pw, ph, bx, (y + vb as usize) % (ph * 8));
            if in_window(x) {
                let ww = if w == 320 { 64 } else { 32 };
                layer_a[x] = self.plane_pixel(base_w, ww, 32, x, y);
            } else {
                let ax = ((x as i32 - hs_a).rem_euclid((pw * 8) as i32)) as usize;
                layer_a[x] = self.plane_pixel(base_a, pw, ph, ax, (y + va as usize) % (ph * 8));
            }
        }
        // Sprites on this line (linked list from entry 0).
        let mut spr = vec![(0u8, false); w];
        let sat = if w == 320 { ((self.regs[5] as usize) & 0x7E) << 9 } else { ((self.regs[5] as usize) & 0x7F) << 9 };
        let max_sprites = if w == 320 { 80 } else { 64 };
        let max_line = if w == 320 { 20 } else { 16 };
        let mut idx = 0usize;
        let mut on_line = 0;
        let mut masked = false;
        for _ in 0..max_sprites {
            let a = (sat + idx * 8) & 0xFFFF;
            let word = |k: usize| u16::from_be_bytes([self.vram[(a + k * 2) & 0xFFFF], self.vram[(a + k * 2 + 1) & 0xFFFF]]);
            let (w0, w1, w2, w3) = (word(0), word(1), word(2), word(3));
            let sy = (w0 & 0x3FF) as i32 - 128;
            let hcells = ((w1 >> 10) & 3) as usize + 1;
            let vcells = ((w1 >> 8) & 3) as usize + 1;
            let link = (w1 & 0x7F) as usize;
            let raw_x = (w3 & 0x1FF) as i32;
            let r = y as i32 - sy;
            if r >= 0 && r < (vcells * 8) as i32 {
                if raw_x == 0 && on_line > 0 {
                    masked = true;
                }
                on_line += 1;
                if on_line > max_line {
                    break;
                }
                if !masked {
                    let sx = raw_x - 128;
                    let (hf, vf) = (w2 & 0x800 != 0, w2 & 0x1000 != 0);
                    let r = r as usize;
                    let ry = if vf { vcells * 8 - 1 - r } else { r };
                    for px in 0..hcells * 8 {
                        let x = sx + px as i32;
                        if !(0..w as i32).contains(&x) || spr[x as usize].0 != 0 {
                            continue;
                        }
                        let rx = if hf { hcells * 8 - 1 - px } else { px };
                        let tile = (w2 & 0x7FF) as usize + (rx / 8) * vcells + ry / 8;
                        let entry = (w2 & 0xE000) | (tile as u16 & 0x7FF);
                        let (c, p) = self.tile_pixel(entry, rx % 8, ry % 8);
                        if c != 0 {
                            spr[x as usize] = (c, p);
                        }
                    }
                }
            }
            idx = link;
            if idx == 0 {
                break;
            }
        }
        // Priority: backdrop < B lo < A lo < S lo < B hi < A hi < S hi.
        for x in 0..w {
            let mut best: Option<u8> = None;
            let mut rank = -1;
            for (i, (c, p)) in [layer_b[x], layer_a[x], spr[x]].iter().enumerate() {
                if *c == 0 {
                    continue;
                }
                let r = i as i32 + if *p { 3 } else { 0 };
                if r > rank {
                    rank = r;
                    best = Some(*c);
                }
            }
            if let Some(c) = best {
                row[x] = cram_rgb(self.cram[c as usize]);
            }
        }
        self.frame[y * MAX_W..y * MAX_W + w].copy_from_slice(row);
    }

    pub fn picture(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.width * HEIGHT);
        for y in 0..HEIGHT {
            out.extend_from_slice(&self.frame[y * MAX_W..y * MAX_W + self.width]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(v: &mut Vdp, r: u16, val: u8) {
        v.write_control(0x8000 | r << 8 | val as u16);
    }
    fn vram_addr(v: &mut Vdp, a: u16) {
        v.write_control(0x4000 | (a & 0x3FFF));
        v.write_control(a >> 14);
    }

    #[test]
    fn plane_tile_and_sprite() {
        let mut v = Vdp::new();
        reg(&mut v, 1, 0x44); // display on, mode 5
        reg(&mut v, 2, 0x30); // plane A at $C000
        reg(&mut v, 4, 0x07); // plane B at $E000
        reg(&mut v, 5, 0x78); // sprites at $F000
        reg(&mut v, 12, 0x81); // H40
        reg(&mut v, 15, 2);
        // CRAM 1 = red, CRAM 17 = green.
        v.write_control(0xC002);
        v.write_control(0x0000);
        v.write_data(0x000E);
        v.write_control(0xC022);
        v.write_control(0x0000);
        v.write_data(0x00E0);
        // Tile 1: colour 1 everywhere; tile 2: colour 1 everywhere (used with palette 1).
        vram_addr(&mut v, 32);
        for _ in 0..32 {
            v.write_data(0x1111);
        }
        // Plane A cell (0,0) = tile 1, palette 0.
        vram_addr(&mut v, 0xC000);
        v.write_data(0x0001);
        // Sprite 0: y=128 (screen 0), 1x1 cell, link 0, palette 1, tile 2, x=128+16.
        vram_addr(&mut v, 0xF000);
        for w in [128u16, 0x0000, 0x2002, 144] {
            v.write_data(w);
        }
        for _ in 0..LINES {
            v.end_line();
        }
        let pic = v.picture();
        assert_eq!(v.width, 320);
        assert_eq!(pic[0], cram_rgb(0x000E), "plane A tile is red");
        assert_eq!(pic[16], cram_rgb(0x00E0), "sprite is green");
        assert_eq!(pic[40], cram_rgb(0), "backdrop");
    }

    #[test]
    fn vblank_interrupt() {
        let mut v = Vdp::new();
        reg(&mut v, 1, 0x60);
        for _ in 0..HEIGHT + 1 {
            v.end_line();
        }
        assert_eq!(v.irq_level(), 6);
        v.ack(6);
        assert_eq!(v.irq_level(), 0);
    }
}
