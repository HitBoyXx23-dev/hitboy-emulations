//! MDEC: the motion decoder that turns compressed macroblocks (run-length coded DCT
//! coefficients) into pixels for FMV and still images. Fed by DMA0, drained by DMA1.

use std::collections::VecDeque;

const ZIGZAG: [usize; 64] = [
    0, 1, 5, 6, 14, 15, 27, 28, 2, 4, 7, 13, 16, 26, 29, 42, 3, 8, 12, 17, 25, 30, 41, 43, 9, 11, 18, 24, 31, 40, 44, 53, 10, 19, 23, 32, 39, 45, 52, 54, 20, 22, 33, 38, 46, 51, 55, 60, 21, 34,
    37, 47, 50, 56, 59, 61, 35, 36, 48, 49, 57, 58, 62, 63,
];

fn zagzig() -> [usize; 64] {
    let mut z = [0; 64];
    for (i, &k) in ZIGZAG.iter().enumerate() {
        z[k] = i;
    }
    z
}

fn s10(v: u16) -> i32 {
    ((v as i32) << 22) >> 22
}

#[derive(state::Stateful)]
pub struct Mdec {
    input: Vec<u32>,
    /// Words still expected for the current command (0 = idle).
    remaining: u32,
    cmd: u32,
    out: VecDeque<u32>,
    lum_q: [u8; 64],
    col_q: [u8; 64],
    scale: [i16; 64],
    dma_in: bool,
    dma_out: bool,
    #[state(skip)]
    zagzig: [usize; 64],
    /// Commands executed (diagnostics).
    #[state(skip)]
    pub commands: u32,
    #[state(skip)]
    pub blocks: std::cell::Cell<u32>,
}

impl Default for Mdec {
    fn default() -> Self {
        Self::new()
    }
}

impl Mdec {
    pub fn new() -> Mdec {
        Mdec { input: Vec::new(), remaining: 0, cmd: 0, out: VecDeque::new(), lum_q: [0; 64], col_q: [0; 64], scale: [0; 64], dma_in: false, dma_out: false, zagzig: zagzig(), commands: 0, blocks: Default::default() }
    }

    pub fn status(&self) -> u32 {
        let depth_bits = (self.cmd >> 25) & 0xF;
        let mut s = depth_bits << 23;
        if self.out.is_empty() {
            s |= 1 << 31;
        }
        if self.remaining > 0 {
            s |= 1 << 29;
        }
        if self.dma_in && self.remaining > 0 {
            s |= 1 << 28;
        }
        if self.dma_out && !self.out.is_empty() {
            s |= 1 << 27;
        }
        s | (self.remaining.wrapping_sub(1) & 0xFFFF)
    }

    pub fn control(&mut self, v: u32) {
        if v & 0x8000_0000 != 0 {
            self.input.clear();
            self.out.clear();
            self.remaining = 0;
            self.cmd = 0;
        }
        self.dma_in = v & 0x4000_0000 != 0;
        self.dma_out = v & 0x2000_0000 != 0;
    }

    pub fn available(&self) -> usize {
        self.out.len()
    }

    pub fn read(&mut self) -> u32 {
        self.out.pop_front().unwrap_or(0)
    }

    pub fn write(&mut self, w: u32) {
        if self.remaining == 0 {
            self.cmd = w;
            self.input.clear();
            self.remaining = match w >> 29 {
                1 => w & 0xFFFF,
                2 => if w & 1 != 0 { 32 } else { 16 },
                3 => 32,
                _ => 0,
            };
            return;
        }
        self.input.push(w);
        self.remaining -= 1;
        if self.remaining == 0 {
            self.execute();
        }
    }

    fn execute(&mut self) {
        self.commands += 1;
        let bytes: Vec<u8> = self.input.iter().flat_map(|w| w.to_le_bytes()).collect();
        match self.cmd >> 29 {
            2 => {
                self.lum_q.copy_from_slice(&bytes[..64]);
                if self.cmd & 1 != 0 {
                    self.col_q.copy_from_slice(&bytes[64..128]);
                }
            }
            3 => {
                for i in 0..64 {
                    self.scale[i] = i16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
                }
            }
            1 => {
                let halves: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                self.decode(&halves);
            }
            _ => {}
        }
        self.input.clear();
    }

    /// Decodes one run-length block into `blk` (raster order). Returns false at end of data.
    fn rl_block(&self, src: &[u16], pos: &mut usize, qt: &[u8; 64], blk: &mut [i32; 64]) -> bool {
        *blk = [0; 64];
        let mut n;
        loop {
            n = match src.get(*pos) {
                Some(&v) => v,
                None => return false,
            };
            *pos += 1;
            if n != 0xFE00 {
                break;
            }
        }
        let q = ((n >> 10) & 0x3F) as i32;
        let mut k = 0usize;
        let mut val = s10(n) * qt[0] as i32;
        loop {
            if q == 0 {
                val = s10(n) * 2;
            }
            let v = val.clamp(-0x400, 0x3FF);
            if q > 0 {
                blk[self.zagzig[k]] = v;
            } else {
                blk[k] = v;
            }
            n = match src.get(*pos) {
                Some(&v) => v,
                None => break,
            };
            *pos += 1;
            k += ((n >> 10) & 0x3F) as usize + 1;
            if k > 63 {
                break;
            }
            val = (s10(n) * qt[k] as i32 * q + 4) / 8;
        }
        self.idct(blk);
        self.blocks.set(self.blocks.get() + 1);
        true
    }

    fn idct(&self, blk: &mut [i32; 64]) {
        let mut tmp = [0i32; 64];
        let (mut src, mut dst) = (blk as &mut [i32; 64] as *mut [i32; 64], &mut tmp as *mut [i32; 64]);
        for _ in 0..2 {
            // SAFETY: src and dst point at two distinct live 64-entry arrays.
            let (s, d) = unsafe { (&*src, &mut *dst) };
            for x in 0..8 {
                for y in 0..8 {
                    let mut sum = 0i64;
                    for z in 0..8 {
                        sum += s[y + z * 8] as i64 * (self.scale[x + z * 8] as i64 / 8);
                    }
                    d[x + y * 8] = ((sum + 0xFFF) / 0x2000) as i32;
                }
            }
            std::mem::swap(&mut src, &mut dst);
        }
        // After two passes the result is back in `blk`.
    }

    fn decode(&mut self, src: &[u16]) {
        let depth = (self.cmd >> 27) & 3;
        let signed = self.cmd & (1 << 26) != 0;
        let bit15 = if self.cmd & (1 << 25) != 0 { 0x8000u32 } else { 0 };
        let mut pos = 0;
        let (lum, col) = (self.lum_q, self.col_q);
        if depth <= 1 {
            // Monochrome: one Y block per 8x8 output.
            let mut y = [0i32; 64];
            while self.rl_block(src, &mut pos, &lum, &mut y) {
                let px: Vec<u8> = y
                    .iter()
                    .map(|&v| {
                        let v = v.clamp(-128, 127) as u8;
                        if signed {
                            v
                        } else {
                            v ^ 0x80
                        }
                    })
                    .collect();
                if depth == 1 {
                    for c in px.chunks_exact(4) {
                        self.out.push_back(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                    }
                } else {
                    for c in px.chunks_exact(8) {
                        let mut w = 0u32;
                        for (i, &p) in c.iter().enumerate() {
                            w |= ((p >> 4) as u32) << (i * 4);
                        }
                        self.out.push_back(w);
                    }
                }
            }
            return;
        }
        loop {
            let (mut cr, mut cb) = ([0i32; 64], [0i32; 64]);
            let mut ys = [[0i32; 64]; 4];
            if !self.rl_block(src, &mut pos, &col, &mut cr) || !self.rl_block(src, &mut pos, &col, &mut cb) {
                return;
            }
            for yb in ys.iter_mut() {
                if !self.rl_block(src, &mut pos, &lum, yb) {
                    return;
                }
            }
            let mut rgb = [[0u8; 3]; 256];
            for (bi, yb) in ys.iter().enumerate() {
                let (xx, yy) = ((bi & 1) * 8, (bi >> 1) * 8);
                for y in 0..8 {
                    for x in 0..8 {
                        let ci = (x + xx) / 2 + ((y + yy) / 2) * 8;
                        let (r, b) = (cr[ci] as f32, cb[ci] as f32);
                        let g = -0.3437 * b - 0.7143 * r;
                        let (r, b) = (1.402 * r, 1.772 * b);
                        let lum = yb[x + y * 8] as f32;
                        let c = |v: f32| {
                            let v = (lum + v).round().clamp(-128.0, 127.0) as i32 as u8;
                            if signed {
                                v
                            } else {
                                v ^ 0x80
                            }
                        };
                        rgb[(x + xx) + (y + yy) * 16] = [c(r), c(g), c(b)];
                    }
                }
            }
            if depth == 2 {
                let bytes: Vec<u8> = rgb.iter().flatten().copied().collect();
                for c in bytes.chunks_exact(4) {
                    self.out.push_back(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                }
            } else {
                let px = |p: &[u8; 3]| (p[0] as u32 >> 3) | (p[1] as u32 >> 3) << 5 | (p[2] as u32 >> 3) << 10 | bit15;
                for pair in rgb.chunks_exact(2) {
                    self.out.push_back(px(&pair[0]) | px(&pair[1]) << 16);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat grey macroblock: only DC terms, identity-ish scale table.
    #[test]
    fn decodes_flat_macroblock_to_uniform_colour() {
        let mut m = Mdec::new();
        // Quant tables: all 1 (luminance + colour).
        m.write(2 << 29 | 1);
        for _ in 0..32 {
            m.write(0x0101_0101);
        }
        // Scale table: first row flat so DC spreads evenly (0x5A82 is the usual DC weight).
        m.write(3 << 29);
        let mut scale = [0i16; 64];
        for x in 0..8 {
            scale[x] = 0x5A82;
        }
        for w in scale.chunks_exact(2) {
            m.write((w[0] as u16 as u32) | (w[1] as u16 as u32) << 16);
        }
        // Six blocks: Cr = Cb = 0, Y = +64, each "DC then EOB", q_scale 1.
        let dc = |v: i32| (1u16 << 10) | (v as u16 & 0x3FF);
        let mut halves = Vec::new();
        for _ in 0..2 {
            halves.extend([dc(0), 0xFE00]);
        }
        for _ in 0..4 {
            halves.extend([dc(64), 0xFE00]);
        }
        m.write(1 << 29 | 2 << 27 | (halves.len() as u32 / 2));
        for h in halves.chunks_exact(2) {
            m.write(h[0] as u32 | (h[1] as u32) << 16);
        }
        assert_eq!(m.available(), 192, "16x16 pixels x 3 bytes");
        let first = m.read().to_le_bytes();
        assert_eq!(first[0], first[1], "grey: R = G");
        assert_eq!(first[1], first[2], "grey: G = B");
        let mut all = vec![first[0]];
        while m.available() > 0 {
            all.extend(m.read().to_le_bytes());
        }
        assert!(all.iter().all(|&b| b == first[0]), "uniform block");
        assert!(first[0] > 0x80, "brighter than mid grey");
    }
}
