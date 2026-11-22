//! GX, the GameCube's graphics chip, in software.
//!
//! Games write a stream of commands to the write-gather pipe. Three register files are
//! loaded by it: the command processor (CP: vertex layout, array locations), the transform
//! unit (XF: matrices, viewport, lights) and the "blitter" (BP: texture setup, the colour
//! combiner called TEV, blending, depth, copies out of the embedded frame buffer). Drawing
//! commands carry vertices in whatever layout CP describes. We transform and light them,
//! clip, rasterise into the EFB, and an EFB copy command writes the finished picture into
//! main memory as the YUV frame the video interface scans out.

use crate::gx_tex;

pub const EFB_W: usize = 640;
pub const EFB_H: usize = 528;
pub const XF_WORDS: usize = 0x1100;

/// A vertex after decoding, before transformation.
#[derive(Clone, Copy, Default)]
pub(crate) struct Vtx {
    pub pos: [f32; 3],
    pub nrm: [f32; 3],
    pub col: [[f32; 4]; 2],
    pub has_col: [bool; 2],
    pub tex: [[f32; 2]; 8],
    pub pmi: u32,
    pub tmi: [u32; 8],
}

/// A vertex in clip space with everything the rasteriser interpolates.
#[derive(Clone, Copy, Default)]
pub(crate) struct CV {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
    pub col: [[f32; 4]; 2],
    pub tc: [[f32; 3]; 8],
}

#[derive(Clone, Copy, Default)]
pub(crate) struct SV {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub iw: f32,
    pub col: [[f32; 4]; 2],
    pub tc: [[f32; 3]; 8],
}

pub struct Gx {
    // Processor interface
    pub pe_ctrl: u16,
    pub pe_token: u16,
    pub pe_regs: [u16; 16],
    pub cp_regs: [u16; 0x40],
    // Command processor state
    pub(crate) matidx_a: u32,
    pub(crate) matidx_b: u32,
    pub(crate) vcd_lo: u32,
    pub(crate) vcd_hi: u32,
    pub(crate) vat: [[u32; 3]; 8],
    pub(crate) arr_base: [u32; 16],
    pub(crate) arr_stride: [u32; 16],
    pub(crate) xf: Vec<u32>,
    pub(crate) bp: [u32; 256],
    bp_mask: u32,
    pending: Vec<u8>,
    // Embedded frame buffer
    pub(crate) efb_c: Vec<u32>,
    pub(crate) efb_z: Vec<u32>,
    pub(crate) tlut: Vec<u16>,
    pub(crate) tev_reg: [[i32; 4]; 4],
    pub(crate) konst: [[i32; 4]; 4],
    pub(crate) tex_cache: Vec<TexEntry>,
    /// Statistics for diagnostics.
    pub draws: u64,
    pub triangles: u64,
    pub copies: u64,
    pub unknown: [u32; 4],
    /// Triangle fate counters: [culled, clipped away, off screen, pixels drawn, depth-rejected, alpha-rejected].
    pub fate: [u64; 6],
    pub debug_draws: u32,
    pub skip_draw: bool,
    /// Last XFB copy: (dest address, stride, width, height).
    pub last_copy: (usize, usize, usize, usize),
}

pub(crate) struct TexEntry {
    pub key: (u32, u32, u32, u32, u32),
    pub sum: u64,
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
    pub age: u32,
}

fn be32(d: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}

impl Default for Gx {
    fn default() -> Self {
        Self::new()
    }
}

impl Gx {
    pub fn new() -> Gx {
        let mut gx = Gx {
            pe_ctrl: 0,
            pe_token: 0,
            pe_regs: [0; 16],
            cp_regs: [0; 0x40],
            matidx_a: 0,
            matidx_b: 0,
            vcd_lo: 0,
            vcd_hi: 0,
            vat: [[0; 3]; 8],
            arr_base: [0; 16],
            arr_stride: [0; 16],
            xf: vec![0; XF_WORDS],
            bp: [0; 256],
            bp_mask: 0x00FF_FFFF,
            pending: Vec::new(),
            efb_c: vec![0xFF00_0000; EFB_W * EFB_H],
            efb_z: vec![0x00FF_FFFF; EFB_W * EFB_H],
            tlut: vec![0; 0x4_0000 + 0x4000],
            tev_reg: [[0; 4]; 4],
            konst: [[255; 4]; 4],
            tex_cache: Vec::new(),
            draws: 0,
            triangles: 0,
            copies: 0,
            unknown: [0; 4],
            fate: [0; 6],
            debug_draws: 0,
            skip_draw: std::env::var_os("GX_SKIP_DRAW").is_some(),
            last_copy: (0, 0, 0, 0),
        };
        gx.cp_regs[0] = 0x000C; // idle
        gx
    }

    // ------------------------------------------------------------ interrupts / registers

    /// Interrupt causes the processor interface should show: (token, finish).
    pub fn irq_bits(&self) -> (bool, bool) {
        let c = self.pe_ctrl;
        (c & 4 != 0 && c & 1 != 0, c & 8 != 0 && c & 2 != 0)
    }

    pub fn pe_read(&self, off: usize) -> u16 {
        match off {
            0x0A => self.pe_ctrl,
            0x0E => self.pe_token,
            _ => self.pe_regs[(off / 2) & 15],
        }
    }

    pub fn pe_write(&mut self, off: usize, v: u16) {
        match off {
            0x0A => {
                // Status bits clear when written as 1; the enables are plain bits.
                let status = self.pe_ctrl & 0xC & !(v & 0xC);
                self.pe_ctrl = status | (v & 3);
            }
            _ => self.pe_regs[(off / 2) & 15] = v,
        }
    }

    pub fn cp_read(&self, off: usize) -> u16 {
        match off {
            0x00 => 0x000C | (self.cp_regs[0] & 0x13),
            _ => self.cp_regs.get(off / 2).copied().unwrap_or(0),
        }
    }

    pub fn cp_write(&mut self, off: usize, v: u16) {
        if let Some(r) = self.cp_regs.get_mut(off / 2) {
            *r = v;
        }
    }

    // ------------------------------------------------------------ command stream

    /// Appends bytes from the write-gather pipe and runs every complete command.
    pub fn feed(&mut self, ram: &mut [u8], bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let data = std::mem::take(&mut self.pending);
        let used = self.process(ram, &data);
        self.pending = data;
        self.pending.drain(..used);
    }

    /// Runs commands from `d`; returns how many bytes were consumed (a partial command at
    /// the end is left for later).
    fn process(&mut self, ram: &mut [u8], d: &[u8]) -> usize {
        let mut p = 0usize;
        while p < d.len() {
            let op = d[p];
            let left = d.len() - p;
            match op {
                0x00 | 0x44 | 0x48 => p += 1,
                0x08 => {
                    if left < 6 {
                        break;
                    }
                    self.cp_load(d[p + 1], be32(d, p + 2));
                    p += 6;
                }
                0x10 => {
                    if left < 5 {
                        break;
                    }
                    let h = be32(d, p + 1);
                    let n = ((h >> 16) & 0xF) as usize + 1;
                    if left < 5 + n * 4 {
                        break;
                    }
                    let addr = (h & 0xFFFF) as usize;
                    for i in 0..n {
                        self.xf_write(addr + i, be32(d, p + 5 + i * 4));
                    }
                    p += 5 + n * 4;
                }
                0x20 | 0x28 | 0x30 | 0x38 => {
                    if left < 5 {
                        break;
                    }
                    let h = be32(d, p + 1);
                    let idx = (h >> 16) as usize;
                    let n = ((h >> 12) & 0xF) as usize + 1;
                    let addr = (h & 0xFFF) as usize;
                    let arr = 12 + ((op - 0x20) >> 3) as usize;
                    let base = (self.arr_base[arr] & 0x01FF_FFFF) as usize + idx * self.arr_stride[arr] as usize;
                    for i in 0..n {
                        let a = base + i * 4;
                        if a + 4 <= ram.len() {
                            let v = be32(ram, a);
                            self.xf_write(addr + i, v);
                        }
                    }
                    p += 5;
                }
                0x40 => {
                    if left < 9 {
                        break;
                    }
                    let addr = (be32(d, p + 1) & 0x01FF_FFFF) as usize;
                    let size = be32(d, p + 5) as usize;
                    p += 9;
                    if addr + size <= ram.len() && size > 0 {
                        let list = ram[addr..addr + size].to_vec();
                        self.process(ram, &list);
                    }
                }
                0x61 => {
                    if left < 5 {
                        break;
                    }
                    let v = be32(d, p + 1);
                    self.bp_load((v >> 24) as u8, v & 0x00FF_FFFF, ram);
                    p += 5;
                }
                0x80..=0xBF => {
                    if left < 3 {
                        break;
                    }
                    let n = u16::from_be_bytes([d[p + 1], d[p + 2]]) as usize;
                    let vat = (op & 7) as usize;
                    let size = self.vertex_size(vat);
                    if left < 3 + n * size {
                        break;
                    }
                    let verts: Vec<Vtx> = (0..n).map(|i| self.read_vertex(ram, vat, &d[p + 3 + i * size..p + 3 + (i + 1) * size])).collect();
                    self.draw(op & 0xF8, &verts, ram);
                    p += 3 + n * size;
                }
                _ => {
                    // Unknown opcode: skip a byte and keep going so we resynchronise.
                    self.unknown[0] += 1;
                    self.unknown[1] = op as u32;
                    p += 1;
                }
            }
        }
        p
    }

    fn cp_load(&mut self, reg: u8, v: u32) {
        match reg {
            0x30 => self.matidx_a = v,
            0x40 => self.matidx_b = v,
            0x50 => self.vcd_lo = v,
            0x60 => self.vcd_hi = v,
            0x70..=0x77 => self.vat[(reg & 7) as usize][0] = v,
            0x80..=0x87 => self.vat[(reg & 7) as usize][1] = v,
            0x90..=0x97 => self.vat[(reg & 7) as usize][2] = v,
            0xA0..=0xAF => self.arr_base[(reg & 15) as usize] = v & 0x03FF_FFFF,
            0xB0..=0xBF => self.arr_stride[(reg & 15) as usize] = v & 0xFF,
            _ => {}
        }
    }

    fn xf_write(&mut self, addr: usize, v: u32) {
        if addr < XF_WORDS {
            self.xf[addr] = v;
        }
    }

    pub(crate) fn xff(&self, addr: usize) -> f32 {
        f32::from_bits(*self.xf.get(addr).unwrap_or(&0))
    }

    // ------------------------------------------------------------ BP (blitter) registers

    fn bp_load(&mut self, reg: u8, data: u32, ram: &mut [u8]) {
        if reg == 0xFE {
            self.bp_mask = data;
            return;
        }
        let masked = (self.bp[reg as usize] & !self.bp_mask) | (data & self.bp_mask);
        self.bp_mask = 0x00FF_FFFF;
        self.bp[reg as usize] = masked;
        let v = masked;
        match reg {
            0x45 => {
                // Draw done.
                self.pe_ctrl |= 8;
            }
            0x47 => self.pe_token = v as u16,
            0x48 => {
                self.pe_token = v as u16;
                self.pe_ctrl |= 4;
            }
            0x52 => self.efb_copy(v, ram),
            0xE0..=0xE7 => {
                let i = ((reg - 0xE0) / 2) as usize;
                let sx = |v: u32| ((v << 21) as i32) >> 21;
                let (lo, hi) = (sx(v & 0x7FF), sx((v >> 12) & 0x7FF));
                let t = if v >> 23 & 1 != 0 { &mut self.konst[i] } else { &mut self.tev_reg[i] };
                if reg & 1 == 0 {
                    t[0] = lo;
                    t[3] = hi;
                } else {
                    t[2] = lo;
                    t[1] = hi;
                }
            }
            0x64 => {}
            0x65 => self.load_tlut(ram),
            _ => {}
        }
    }

    /// LOADTLUT: copies palette entries from main memory.
    fn load_tlut(&mut self, ram: &[u8]) {
        let src = ((self.bp[0x64] & 0x00FF_FFFF) << 5) as usize;
        let off = (self.bp[0x65] & 0x3FF) as usize * 256;
        let count = ((self.bp[0x65] >> 10) & 0x3FF) as usize * 16;
        for i in 0..count {
            let a = src + i * 2;
            if a + 2 <= ram.len() && off + i < self.tlut.len() {
                self.tlut[off + i] = u16::from_be_bytes([ram[a], ram[a + 1]]);
            }
        }
    }

    // ------------------------------------------------------------ vertex decoding

    fn vertex_size(&self, vat: usize) -> usize {
        let (lo, hi) = (self.vcd_lo, self.vcd_hi);
        let [a, b, c] = self.vat[vat];
        let mut n = 0usize;
        n += (lo & 1) as usize; // position matrix index
        n += ((lo >> 1) & 0xFF).count_ones() as usize; // texture matrix indices
        let idx = |mode: u32| match mode {
            2 => 1,
            3 => 2,
            _ => 0,
        };
        // Position
        let pm = (lo >> 9) & 3;
        n += if pm == 1 { comp_size((a >> 1) & 7) * if a & 1 != 0 { 3 } else { 2 } } else { idx(pm) };
        // Normal
        let nm = (lo >> 11) & 3;
        let nfmt = (a >> 10) & 7;
        let nn = if (a >> 9) & 1 != 0 { 9 } else { 3 };
        n += if nm == 1 { comp_size(nfmt) * nn } else { idx(nm) * if (a >> 9) & 1 != 0 && a >> 31 != 0 { 3 } else { 1 } };
        // Colours
        for (i, shift) in [(0, 13u32), (1, 15)] {
            let mode = (lo >> shift) & 3;
            let fmt = if i == 0 { (a >> 14) & 7 } else { (a >> 18) & 7 };
            n += if mode == 1 { color_size(fmt) } else { idx(mode) };
        }
        // Texture coordinates
        for t in 0..8 {
            let mode = (hi >> (t * 2)) & 3;
            let (cnt, fmt) = tex_fmt(a, b, c, t);
            n += if mode == 1 { comp_size(fmt) * (cnt + 1) } else { idx(mode) };
        }
        n
    }

    fn read_array(&self, ram: &[u8], arr: usize, index: usize, len: usize) -> Vec<u8> {
        let a = (self.arr_base[arr] & 0x01FF_FFFF) as usize + index * self.arr_stride[arr] as usize;
        (0..len).map(|i| *ram.get(a + i).unwrap_or(&0)).collect()
    }

    fn read_vertex(&self, ram: &[u8], vat: usize, d: &[u8]) -> Vtx {
        let (lo, hi) = (self.vcd_lo, self.vcd_hi);
        let [a, b, c] = self.vat[vat];
        let mut v = Vtx::default();
        let mut p = 0usize;
        let mut take = |n: usize| -> &[u8] {
            let s = &d[p..p + n];
            p += n;
            s
        };
        if lo & 1 != 0 {
            v.pmi = take(1)[0] as u32;
        } else {
            v.pmi = self.matidx_a & 0x3F;
        }
        for t in 0..8 {
            v.tmi[t] = if (lo >> (1 + t)) & 1 != 0 {
                take(1)[0] as u32
            } else if t < 4 {
                (self.matidx_a >> (6 + 6 * t)) & 0x3F
            } else {
                (self.matidx_b >> (6 * (t - 4))) & 0x3F
            };
        }
        // Position
        let pm = (lo >> 9) & 3;
        let (pn, pf, ps) = (if a & 1 != 0 { 3 } else { 2 }, (a >> 1) & 7, (a >> 4) & 31);
        match pm {
            1 => {
                let n = comp_size(pf) * pn;
                let raw = take(n).to_vec();
                v.pos = read_floats3(&raw, pf, pn, ps);
            }
            2 | 3 => {
                let ix = read_index(take(pm as usize - 1));
                let raw = self.read_array(ram, 0, ix, comp_size(pf) * pn);
                v.pos = read_floats3(&raw, pf, pn, ps);
            }
            _ => {}
        }
        // Normal
        let nm = (lo >> 11) & 3;
        let nfmt = (a >> 10) & 7;
        let nbt = (a >> 9) & 1 != 0;
        let nshift = if nfmt == 1 { 6 } else { 14 };
        match nm {
            1 => {
                let n = comp_size(nfmt) * if nbt { 9 } else { 3 };
                let raw = take(n).to_vec();
                v.nrm = read_floats3(&raw, nfmt, 3, nshift);
            }
            2 | 3 => {
                let cnt = if nbt && a >> 31 != 0 { 3 } else { 1 };
                let mut first = 0;
                for k in 0..cnt {
                    let ix = read_index(take(nm as usize - 1));
                    if k == 0 {
                        first = ix;
                    }
                }
                let raw = self.read_array(ram, 1, first, comp_size(nfmt) * 3);
                v.nrm = read_floats3(&raw, nfmt, 3, nshift);
            }
            _ => {}
        }
        // Colours
        for i in 0..2 {
            let mode = (lo >> (13 + 2 * i)) & 3;
            let fmt = if i == 0 { (a >> 14) & 7 } else { (a >> 18) & 7 };
            match mode {
                1 => {
                    let raw = take(color_size(fmt)).to_vec();
                    v.col[i] = read_color(&raw, fmt);
                    v.has_col[i] = true;
                }
                2 | 3 => {
                    let ix = read_index(take(mode as usize - 1));
                    let raw = self.read_array(ram, 2 + i, ix, color_size(fmt));
                    v.col[i] = read_color(&raw, fmt);
                    v.has_col[i] = true;
                }
                _ => {}
            }
        }
        // Texture coordinates
        for t in 0..8 {
            let mode = (hi >> (t * 2)) & 3;
            let (cnt, fmt) = tex_fmt(a, b, c, t);
            let shift = tex_shift(a, b, c, t);
            let n = comp_size(fmt) * (cnt + 1);
            let raw = match mode {
                1 => take(n).to_vec(),
                2 | 3 => {
                    let ix = read_index(take(mode as usize - 1));
                    self.read_array(ram, 4 + t, ix, n)
                }
                _ => continue,
            };
            let f = read_floats3(&raw, fmt, cnt + 1, shift);
            v.tex[t] = [f[0], if cnt >= 1 { f[1] } else { 0.0 }];
        }
        v
    }
}

fn comp_size(fmt: u32) -> usize {
    match fmt {
        0 | 1 => 1,
        2 | 3 => 2,
        _ => 4,
    }
}

fn color_size(fmt: u32) -> usize {
    match fmt {
        0 | 3 => 2,
        1 | 4 => 3,
        _ => 4,
    }
}

fn read_index(b: &[u8]) -> usize {
    if b.len() == 1 { b[0] as usize } else { u16::from_be_bytes([b[0], b[1]]) as usize }
}

fn read_floats3(raw: &[u8], fmt: u32, n: usize, shift: u32) -> [f32; 3] {
    let mut out = [0.0f32; 3];
    let sz = comp_size(fmt);
    let scale = 1.0 / (1u32 << shift.min(31)) as f32;
    for (i, o) in out.iter_mut().enumerate().take(n.min(3)) {
        let s = &raw[i * sz..(i + 1) * sz];
        *o = match fmt {
            0 => s[0] as f32 * scale,
            1 => s[0] as i8 as f32 * scale,
            2 => u16::from_be_bytes([s[0], s[1]]) as f32 * scale,
            3 => i16::from_be_bytes([s[0], s[1]]) as f32 * scale,
            _ => f32::from_be_bytes([s[0], s[1], s[2], s[3]]),
        };
    }
    out
}

fn read_color(r: &[u8], fmt: u32) -> [f32; 4] {
    let e5 = |v: u32| ((v << 3) | (v >> 2)) as f32;
    let e6 = |v: u32| ((v << 2) | (v >> 4)) as f32;
    let e4 = |v: u32| (v * 17) as f32;
    match fmt {
        0 => {
            let v = u16::from_be_bytes([r[0], r[1]]) as u32;
            [e5(v >> 11), e6((v >> 5) & 63), e5(v & 31), 255.0]
        }
        1 => [r[0] as f32, r[1] as f32, r[2] as f32, 255.0],
        2 => [r[0] as f32, r[1] as f32, r[2] as f32, 255.0],
        3 => {
            let v = u16::from_be_bytes([r[0], r[1]]) as u32;
            [e4(v >> 12), e4((v >> 8) & 15), e4((v >> 4) & 15), e4(v & 15)]
        }
        4 => {
            let v = (r[0] as u32) << 16 | (r[1] as u32) << 8 | r[2] as u32;
            [e6(v >> 18), e6((v >> 12) & 63), e6((v >> 6) & 63), e6(v & 63)]
        }
        _ => [r[0] as f32, r[1] as f32, r[2] as f32, r[3] as f32],
    }
}

/// (components - 1, format) of texture coordinate set `t` in VAT words A/B/C.
fn tex_fmt(a: u32, b: u32, c: u32, t: usize) -> (usize, u32) {
    match t {
        0 => (((a >> 21) & 1) as usize, (a >> 22) & 7),
        1 => ((b & 1) as usize, (b >> 1) & 7),
        2 => (((b >> 9) & 1) as usize, (b >> 10) & 7),
        3 => (((b >> 18) & 1) as usize, (b >> 19) & 7),
        4 => (((b >> 27) & 1) as usize, (b >> 28) & 7),
        5 => (((c >> 5) & 1) as usize, (c >> 6) & 7),
        6 => (((c >> 14) & 1) as usize, (c >> 15) & 7),
        _ => (((c >> 23) & 1) as usize, (c >> 24) & 7),
    }
}

fn tex_shift(a: u32, b: u32, c: u32, t: usize) -> u32 {
    match t {
        0 => (a >> 25) & 31,
        1 => (b >> 4) & 31,
        2 => (b >> 13) & 31,
        3 => (b >> 22) & 31,
        4 => c & 31,
        5 => (c >> 9) & 31,
        6 => (c >> 18) & 31,
        _ => (c >> 27) & 31,
    }
}

#[allow(dead_code)]
pub(crate) fn decode_tex_for_tests(data: &[u8], fmt: u32, w: usize, h: usize) -> Vec<u32> {
    gx_tex::decode(data, fmt, w, h, &|_| 0)
}
