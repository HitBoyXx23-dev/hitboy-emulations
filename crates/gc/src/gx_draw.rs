//! GX drawing: vertex transform and lighting, clipping, rasterisation, the TEV colour
//! combiner, depth/alpha tests, blending, and copies out of the embedded frame buffer.

use crate::gx::*;
use crate::gx_tex;

#[derive(Clone, Copy, Default)]
struct Stage {
    ca: u32,
    cb: u32,
    cc: u32,
    cd: u32,
    cbias: u32,
    csub: bool,
    cclamp: bool,
    cscale: u32,
    cdst: usize,
    aa: u32,
    ab: u32,
    ac: u32,
    ad: u32,
    abias: u32,
    asub: bool,
    aclamp: bool,
    ascale: u32,
    adst: usize,
    ras_swap: usize,
    tex_swap: usize,
    map: usize,
    coord: usize,
    tex_en: bool,
    chan: u32,
    kc: u32,
    ka: u32,
}

/// Everything a draw call needs, read once from the registers.
struct Setup {
    stages: Vec<Stage>,
    swaps: [[usize; 4]; 4],
    regs: [[i32; 4]; 4],
    konst: [[i32; 4]; 4],
    ntex: usize,
    cull: u32,
    z_en: bool,
    z_func: u32,
    z_upd: bool,
    blend_en: bool,
    sub: bool,
    src_f: u32,
    dst_f: u32,
    color_upd: bool,
    alpha_upd: bool,
    a_comp: [u32; 2],
    a_ref: [i32; 2],
    a_logic: u32,
    vp: [f32; 6],
    zscale: f32,
    zoff: f32,
    scissor: (i32, i32, i32, i32),
    textures: [Option<usize>; 8],
    tc_scale: [(f32, f32); 8],
}

#[derive(Clone, Copy)]
struct TexMeta {
    wrap_s: u32,
    wrap_t: u32,
    linear: bool,
}

impl Gx {
    fn bp(&self, r: usize) -> u32 {
        self.bp[r]
    }

    // ------------------------------------------------------------ textures

    fn tex_regs(&self, n: usize) -> (u32, u32, u32, u32) {
        if n < 4 {
            (self.bp(0x80 + n), self.bp(0x88 + n), self.bp(0x94 + n), self.bp(0x98 + n))
        } else {
            (self.bp(0xA0 + n - 4), self.bp(0xA8 + n - 4), self.bp(0xB4 + n - 4), self.bp(0xB8 + n - 4))
        }
    }

    /// Finds (decoding if needed) the texture bound to map `n`.
    fn texture(&mut self, n: usize, ram: &[u8]) -> Option<usize> {
        let (_mode, img0, img3, settlut) = self.tex_regs(n);
        let w = ((img0 & 0x3FF) + 1) as usize;
        let h = (((img0 >> 10) & 0x3FF) + 1) as usize;
        let fmt = (img0 >> 20) & 0xF;
        let addr = ((img3 & 0x00FF_FFFF) << 5) as usize;
        if addr == 0 || addr >= ram.len() {
            return None;
        }
        let tl_off = settlut & 0x3FF;
        let tl_fmt = (settlut >> 10) & 3;
        let len = gx_tex::byte_len(fmt, w, h).min(ram.len() - addr);
        let data = &ram[addr..addr + len];
        let mut sum: u64 = 0xCBF2_9CE4_8422_2325;
        let step = (len / 8 / 2048).max(1) * 8;
        let mut i = 0;
        while i + 8 <= len {
            sum = (sum ^ u64::from_le_bytes(data[i..i + 8].try_into().unwrap())).wrapping_mul(0x100_0000_01B3);
            i += step;
        }
        let key = (addr as u32, (w as u32) << 16 | h as u32, fmt, tl_off, tl_fmt);
        if matches!(fmt, 8 | 9 | 10) {
            // Palette contents matter too.
            let base = tl_off as usize * 256;
            for k in 0..256.min(self.tlut.len().saturating_sub(base)) {
                sum = (sum ^ self.tlut[base + k] as u64).wrapping_mul(0x100_0000_01B3);
            }
        }
        for (i, e) in self.tex_cache.iter_mut().enumerate() {
            if e.key == key && e.sum == sum {
                e.age = 0;
                return Some(i);
            }
        }
        let tlut = &self.tlut;
        let base = tl_off as usize * 256;
        if std::env::var_os("GX_TEXLOG").is_some() {
            eprintln!("TEX {w}x{h} fmt={fmt} addr={addr:#x} tl_off={tl_off} tl_fmt={tl_fmt} pal={:04X?}", self.tlut.get(base_of(tl_off)..base_of(tl_off) + 4));
        }
        let px = gx_tex::decode(data, fmt, w, h, &|i| gx_tex::palette_color(tl_fmt, *tlut.get(base + i).unwrap_or(&0)));
        if let (Ok(dir), true) = (std::env::var("GX_TEXDUMP"), fmt == 8 || fmt == 6) {
            let mut o = format!("P6 {w} {h} 255
").into_bytes();
            for p in &px {
                o.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
            }
            let _ = std::fs::write(format!("{dir}/tex_{addr:x}.ppm"), o);
        }
        // Replace a stale entry for the same place, else add (bounded).
        if let Some(i) = self.tex_cache.iter().position(|e| e.key == key) {
            self.tex_cache[i] = TexEntry { key, sum, w, h, px, age: 0 };
            return Some(i);
        }
        if self.tex_cache.len() >= 192 {
            let oldest = (0..self.tex_cache.len()).max_by_key(|&i| self.tex_cache[i].age).unwrap();
            self.tex_cache[oldest] = TexEntry { key, sum, w, h, px, age: 0 };
            return Some(oldest);
        }
        self.tex_cache.push(TexEntry { key, sum, w, h, px, age: 0 });
        Some(self.tex_cache.len() - 1)
    }

    fn tex_meta(&self, n: usize) -> TexMeta {
        let (mode, ..) = self.tex_regs(n);
        TexMeta { wrap_s: mode & 3, wrap_t: (mode >> 2) & 3, linear: mode & 0x10 != 0 }
    }

    // ------------------------------------------------------------ setup

    fn build_setup(&mut self, ram: &[u8]) -> Setup {
        let gm = self.bp(0x00);
        let ntex = (gm & 0xF) as usize;
        let nst = (((gm >> 10) & 0xF) + 1) as usize;
        let mut stages = Vec::with_capacity(nst);
        for n in 0..nst {
            let c = self.bp(0xC0 + 2 * n);
            let a = self.bp(0xC1 + 2 * n);
            let t = self.bp(0x28 + n / 2);
            let tsel = if n & 1 == 0 { t & 0x7FFF } else { t >> 12 };
            let ks = self.bp(0xF6 + n / 2);
            let (kc, ka) = if n & 1 == 0 { ((ks >> 4) & 31, (ks >> 9) & 31) } else { ((ks >> 14) & 31, (ks >> 19) & 31) };
            stages.push(Stage {
                cd: c & 15,
                cc: (c >> 4) & 15,
                cb: (c >> 8) & 15,
                ca: (c >> 12) & 15,
                cbias: (c >> 16) & 3,
                csub: (c >> 18) & 1 != 0,
                cclamp: (c >> 19) & 1 != 0,
                cscale: (c >> 20) & 3,
                cdst: ((c >> 22) & 3) as usize,
                ras_swap: (a & 3) as usize,
                tex_swap: ((a >> 2) & 3) as usize,
                ad: (a >> 4) & 7,
                ac: (a >> 7) & 7,
                ab: (a >> 10) & 7,
                aa: (a >> 13) & 7,
                abias: (a >> 16) & 3,
                asub: (a >> 18) & 1 != 0,
                aclamp: (a >> 19) & 1 != 0,
                ascale: (a >> 20) & 3,
                adst: ((a >> 22) & 3) as usize,
                map: (tsel & 7) as usize,
                coord: ((tsel >> 3) & 7) as usize,
                tex_en: (tsel >> 6) & 1 != 0,
                chan: (tsel >> 7) & 7,
                kc,
                ka,
            });
        }
        let mut swaps = [[0usize, 1, 2, 3]; 4];
        for (i, s) in swaps.iter_mut().enumerate() {
            let lo = self.bp(0xF6 + 2 * i);
            let hi = self.bp(0xF7 + 2 * i);
            *s = [(lo & 3) as usize, ((lo >> 2) & 3) as usize, (hi & 3) as usize, ((hi >> 2) & 3) as usize];
        }
        let z = self.bp(0x40);
        let cm = self.bp(0x41);
        let ac = self.bp(0xF3);
        let tl = self.bp(0x20);
        let br = self.bp(0x21);
        let off = 342;
        let vp = [self.xff(0x101A), self.xff(0x101B), self.xff(0x101C), self.xff(0x101D), self.xff(0x101E), self.xff(0x101F)];
        let mut textures = [None; 8];
        let mut tc_scale = [(1.0f32, 1.0f32); 8];
        for st in &stages {
            if st.tex_en && textures[st.map].is_none() {
                textures[st.map] = self.texture(st.map, ram);
            }
        }
        for (i, sc) in tc_scale.iter_mut().enumerate() {
            let s = self.bp(0x30 + 2 * i) & 0xFFFF;
            let t = self.bp(0x31 + 2 * i) & 0xFFFF;
            *sc = (s as f32 + 1.0, t as f32 + 1.0);
        }
        Setup {
            stages,
            swaps,
            regs: self.tev_reg,
            konst: self.konst,
            ntex,
            cull: (gm >> 14) & 3,
            z_en: z & 1 != 0,
            z_func: (z >> 1) & 7,
            z_upd: (z >> 4) & 1 != 0,
            blend_en: cm & 1 != 0,
            sub: (cm >> 11) & 1 != 0,
            src_f: (cm >> 8) & 7,
            dst_f: (cm >> 5) & 7,
            color_upd: (cm >> 3) & 1 != 0,
            alpha_upd: (cm >> 4) & 1 != 0,
            a_comp: [(ac >> 16) & 7, (ac >> 19) & 7],
            a_ref: [(ac & 0xFF) as i32, ((ac >> 8) & 0xFF) as i32],
            a_logic: (ac >> 22) & 3,
            vp,
            zscale: vp[2],
            zoff: vp[5],
            scissor: (((tl >> 12) & 0x7FF) as i32 - off, (tl & 0x7FF) as i32 - off, ((br >> 12) & 0x7FF) as i32 - off, (br & 0x7FF) as i32 - off),
            textures,
            tc_scale,
        }
    }

    // ------------------------------------------------------------ transform and lighting

    fn transform(&self, v: &Vtx) -> CV {
        let m = (v.pmi as usize) * 4;
        let (x, y, z) = (v.pos[0], v.pos[1], v.pos[2]);
        let row = |r: usize| self.xff(m + r * 4) * x + self.xff(m + r * 4 + 1) * y + self.xff(m + r * 4 + 2) * z + self.xff(m + r * 4 + 3);
        let (vx, vy, vz) = (row(0), row(1), row(2));
        let p = |i: usize| self.xff(0x1020 + i);
        let ortho = self.xf[0x1026] != 0;
        let (cx, cy, cz, cw) = if ortho {
            (p(0) * vx + p(1), p(2) * vy + p(3), p(4) * vz + p(5), 1.0)
        } else {
            (p(0) * vx + p(1) * vz, p(2) * vy + p(3) * vz, p(4) * vz + p(5), -vz)
        };
        let mut out = CV { x: cx, y: cy, z: cz, w: cw, ..Default::default() };

        // Normal in view space.
        let nbase = 0x400 + (v.pmi as usize) * 3;
        let nrow = |r: usize| {
            self.xff(nbase + r * 3) * v.nrm[0] + self.xff(nbase + r * 3 + 1) * v.nrm[1] + self.xff(nbase + r * 3 + 2) * v.nrm[2]
        };
        let n = [nrow(0), nrow(1), nrow(2)];
        let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        let n = if nl > 1e-6 { [n[0] / nl, n[1] / nl, n[2] / nl] } else { n };

        // Colour channels.
        let nchan = (self.xf[0x1009] & 3) as usize;
        for ch in 0..2 {
            if ch >= nchan.max(1) && nchan == 0 {
                out.col[ch] = [255.0; 4];
                continue;
            }
            if ch >= nchan {
                out.col[ch] = [255.0; 4];
                continue;
            }
            let ctl = self.xf[0x100E + ch];
            let actl = self.xf[0x1010 + ch];
            let mat = unpack(self.xf[0x100C + ch]);
            let amb = unpack(self.xf[0x100A + ch]);
            let vcol = if v.has_col[ch] { v.col[ch] } else { [255.0; 4] };
            let msrc = if ctl & 1 != 0 { vcol } else { mat };
            let amsrc = if actl & 1 != 0 { vcol } else { mat };
            let mut rgb = msrc;
            let mut a = amsrc[3];
            if ctl & 2 != 0 {
                let asrc = if ctl & 0x40 != 0 { vcol } else { amb };
                let mut lit = [asrc[0], asrc[1], asrc[2]];
                self.lights(&mut lit, ctl, &(vx, vy, vz), &n);
                for i in 0..3 {
                    rgb[i] = (msrc[i] * lit[i].clamp(0.0, 255.0) / 255.0).clamp(0.0, 255.0);
                }
            }
            if actl & 2 != 0 {
                let asrc = if actl & 0x40 != 0 { vcol } else { amb };
                let mut lit = [asrc[3], 0.0, 0.0];
                self.lights_alpha(&mut lit, actl, &(vx, vy, vz), &n);
                a = (amsrc[3] * lit[0].clamp(0.0, 255.0) / 255.0).clamp(0.0, 255.0);
            }
            out.col[ch] = [rgb[0], rgb[1], rgb[2], a];
        }

        // Texture coordinates.
        let ntex = (self.bp(0x00) & 0xF) as usize;
        for t in 0..ntex.min(8) {
            let info = self.xf[0x1040 + t];
            let proj3 = (info >> 1) & 1 != 0;
            let abc1 = (info >> 2) & 3 == 1;
            let src = (info >> 7) & 31;
            let inp: [f32; 3] = match src {
                0 => [x, y, z],
                1 => v.nrm,
                2 => [v.col[0][0] / 255.0, v.col[0][1] / 255.0, 1.0],
                5..=12 => [v.tex[(src - 5) as usize][0], v.tex[(src - 5) as usize][1], 1.0],
                _ => [0.0, 0.0, 1.0],
            };
            let mi = (v.tmi[t] as usize) * 4;
            let w = if abc1 { inp[2] } else { 1.0 };
            let row = |r: usize| self.xff(mi + r * 4) * inp[0] + self.xff(mi + r * 4 + 1) * inp[1] + self.xff(mi + r * 4 + 2) * w + self.xff(mi + r * 4 + 3);
            let (mut s, mut tt, mut q) = (row(0), row(1), if proj3 { row(2) } else { 1.0 });
            // Optional post-transform matrix.
            let dual = self.xf[0x1050 + t];
            if self.xf[0x1012] != 0 || dual != 0 {
                let pi = 0x500 + ((dual & 0x3F) as usize) * 4;
                if pi + 12 <= self.xf.len() && dual & 0x3F != 0 {
                    let mut v3 = [s, tt, q];
                    if dual & 0x100 != 0 {
                        let l = (v3[0] * v3[0] + v3[1] * v3[1] + v3[2] * v3[2]).sqrt();
                        if l > 1e-9 {
                            v3 = [v3[0] / l, v3[1] / l, v3[2] / l];
                        }
                    }
                    let r = |k: usize| self.xff(pi + k * 4) * v3[0] + self.xff(pi + k * 4 + 1) * v3[1] + self.xff(pi + k * 4 + 2) * v3[2] + self.xff(pi + k * 4 + 3);
                    s = r(0);
                    tt = r(1);
                    q = r(2);
                }
            }
            out.tc[t] = [s, tt, if proj3 { q } else { 1.0 }];
        }
        out
    }

    fn light_params(&self, i: usize) -> ([f32; 3], [f32; 3], [f32; 3], [f32; 3], [f32; 3], [f32; 4]) {
        let b = 0x600 + i * 16;
        let c = self.xf[b + 3];
        (
            [self.xff(b + 4), self.xff(b + 5), self.xff(b + 6)],
            [self.xff(b + 7), self.xff(b + 8), self.xff(b + 9)],
            [self.xff(b + 10), self.xff(b + 11), self.xff(b + 12)],
            [self.xff(b + 13), self.xff(b + 14), self.xff(b + 15)],
            [0.0; 3],
            unpack(c),
        )
    }

    fn light_intensity(&self, i: usize, ctl: u32, vp: &(f32, f32, f32), n: &[f32; 3]) -> (f32, [f32; 4]) {
        let (a, k, pos, dir, _, col) = self.light_params(i);
        let mut l = [pos[0] - vp.0, pos[1] - vp.1, pos[2] - vp.2];
        let d = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt().max(1e-9);
        for c in l.iter_mut() {
            *c /= d;
        }
        let diff = match (ctl >> 7) & 3 {
            0 => 1.0,
            1 => n[0] * l[0] + n[1] * l[1] + n[2] * l[2],
            _ => (n[0] * l[0] + n[1] * l[1] + n[2] * l[2]).max(0.0),
        };
        let mut att = 1.0;
        if (ctl >> 9) & 1 != 0 {
            let c = -(l[0] * dir[0] + l[1] * dir[1] + l[2] * dir[2]);
            let ca = (a[0] + a[1] * c + a[2] * c * c).max(0.0);
            let da = k[0] + k[1] * d + k[2] * d * d;
            att = if da.abs() > 1e-9 { ca / da } else { 0.0 };
        }
        (diff * att, col)
    }

    fn light_mask(ctl: u32) -> u32 {
        ((ctl >> 2) & 0xF) | (((ctl >> 11) & 0xF) << 4)
    }

    fn lights(&self, lit: &mut [f32; 3], ctl: u32, vp: &(f32, f32, f32), n: &[f32; 3]) {
        let mask = Self::light_mask(ctl);
        for i in 0..8 {
            if mask & (1 << i) != 0 {
                let (f, c) = self.light_intensity(i, ctl, vp, n);
                for k in 0..3 {
                    lit[k] += c[k] * f;
                }
            }
        }
    }

    fn lights_alpha(&self, lit: &mut [f32; 3], ctl: u32, vp: &(f32, f32, f32), n: &[f32; 3]) {
        let mask = Self::light_mask(ctl);
        for i in 0..8 {
            if mask & (1 << i) != 0 {
                let (f, c) = self.light_intensity(i, ctl, vp, n);
                lit[0] += c[3] * f;
            }
        }
    }

    // ------------------------------------------------------------ drawing

    pub(crate) fn draw(&mut self, prim: u8, verts: &[Vtx], ram: &[u8]) {
        if self.skip_draw {
            return;
        }
        if verts.len() < 3 && !(prim == 0xA8 || prim == 0xB0 || prim == 0xB8) {
            return;
        }
        self.draws += 1;
        let su = self.build_setup(ram);
        let cv: Vec<CV> = verts.iter().map(|v| self.transform(v)).collect();
        if std::env::var("GX_FRAME").ok().and_then(|s| s.parse::<u64>().ok()) == Some(self.copies) {
            let t0 = su.stages.first().filter(|s| s.tex_en).and_then(|s| su.textures[s.map]).and_then(|i| self.tex_cache.get(i)).map(|e| format!("{}x{} f{}", e.w, e.h, e.key.2)).unwrap_or_else(|| "-".into());
            let p0 = verts.first().map(|v| v.pos).unwrap_or_default();
            eprintln!("D prim={prim:#x} n={} tex={t0} st={} ntex={} z={}/{}{} blend={} cull={} p0={:?}", verts.len(), su.stages.len(), su.ntex, su.z_en as u8, su.z_func, if su.z_upd { "w" } else { "" }, su.blend_en as u8, su.cull, p0);
        }
        if std::env::var_os("GX_DEBUG").is_some() && self.debug_draws < 14 && self.draws > std::env::var("GX_DEBUG_FROM").ok().and_then(|s| s.parse().ok()).unwrap_or(20) {
            self.debug_draws += 1;
            eprintln!("DRAW prim={prim:#x} n={} stages={} ntex={} cull={} z={}/{} blend={} vp={:?}", verts.len(), su.stages.len(), su.ntex, su.cull, su.z_en, su.z_func, su.blend_en, su.vp);
            eprintln!("  vcd={:08X}/{:08X} vat={:08X?} xf1040={:08X} matidxA={:08X}", self.vcd_lo, self.vcd_hi, self.vat[0], self.xf[0x1040], self.matidx_a);
            for (i, v) in verts.iter().take(3).enumerate() {
                eprintln!("  v{i} pos={:?} -> clip=({:.2},{:.2},{:.2},{:.2}) col0={:?} tex0={:?}", v.pos, cv[i].x, cv[i].y, cv[i].z, cv[i].w, cv[i].col[0], cv[i].tc[0]);
            }
            for (m, t) in su.textures.iter().enumerate() {
                if let Some(e) = t.and_then(|i| self.tex_cache.get(i)) {
                    let base = e.key.3 as usize * 256;
                    eprintln!("  tex{m}: {}x{} fmt={} addr={:#x} scale={:?} tlut_off={} tlut_fmt={} pal={:04X?} px0={:08X?}", e.w, e.h, e.key.2, e.key.0, su.tc_scale[0], e.key.3, e.key.4, self.tlut.get(base..base + 8), &e.px[..4]);
                }
            }
            for (i, st) in su.stages.iter().enumerate() {
                eprintln!("  stage{i}: c[{} {} {} {}] a[{} {} {} {}] tex={} map={} coord={} chan={}", st.ca, st.cb, st.cc, st.cd, st.aa, st.ab, st.ac, st.ad, st.tex_en, st.map, st.coord, st.chan);
            }
        }
        let mut tris: Vec<[usize; 3]> = Vec::new();
        let n = cv.len();
        match prim {
            0x80 => {
                for q in (0..n / 4 * 4).step_by(4) {
                    tris.push([q, q + 1, q + 2]);
                    tris.push([q, q + 2, q + 3]);
                }
            }
            0x90 => {
                for t in (0..n / 3 * 3).step_by(3) {
                    tris.push([t, t + 1, t + 2]);
                }
            }
            0x98 => {
                for i in 2..n {
                    if i & 1 == 0 {
                        tris.push([i - 2, i - 1, i]);
                    } else {
                        tris.push([i - 1, i - 2, i]);
                    }
                }
            }
            0xA0 => {
                for i in 2..n {
                    tris.push([0, i - 1, i]);
                }
            }
            _ => {}
        }
        for t in tris {
            self.triangles += 1;
            self.raster_triangle(&su, [cv[t[0]], cv[t[1]], cv[t[2]]]);
        }
    }

    fn project(&self, su: &Setup, v: &CV) -> SV {
        let iw = 1.0 / v.w;
        let nx = v.x * iw;
        let ny = v.y * iw;
        let nz = v.z * iw;
        let mut s = SV {
            x: nx * su.vp[0] + su.vp[3] - 342.0,
            y: ny * su.vp[1] + su.vp[4] - 342.0,
            z: nz * su.zscale + su.zoff,
            iw,
            col: v.col,
            tc: v.tc,
        };
        for c in 0..2 {
            for k in 0..4 {
                s.col[c][k] *= iw;
            }
        }
        for t in 0..8 {
            for k in 0..3 {
                s.tc[t][k] *= iw;
            }
        }
        s
    }

    fn raster_triangle(&mut self, su: &Setup, tri: [CV; 3]) {
        // Clip against the near plane (z >= -w in GX clip space).
        let mut poly: Vec<CV> = Vec::with_capacity(4);
        let inside = |v: &CV| v.z + v.w >= 0.0 && v.w > 1e-6;
        for i in 0..3 {
            let (a, b) = (tri[i], tri[(i + 1) % 3]);
            let (ia, ib) = (inside(&a), inside(&b));
            if ia {
                poly.push(a);
            }
            if ia != ib {
                let da = a.z + a.w;
                let db = b.z + b.w;
                let t = da / (da - db);
                poly.push(lerp_cv(&a, &b, t));
            }
        }
        if poly.len() < 3 {
            self.fate[1] += 1;
            return;
        }
        let sv: Vec<SV> = poly.iter().map(|v| self.project(su, v)).collect();
        for i in 1..sv.len() - 1 {
            self.fill(su, [sv[0], sv[i], sv[i + 1]]);
        }
    }

    fn fill(&mut self, su: &Setup, t: [SV; 3]) {
        let area = (t[1].x - t[0].x) * (t[2].y - t[0].y) - (t[2].x - t[0].x) * (t[1].y - t[0].y);
        if area == 0.0 || !area.is_finite() {
            return;
        }
        let back = area > 0.0;
        match su.cull {
            1 if back => {
                self.fate[0] += 1;
                return;
            }
            2 if !back => {
                self.fate[0] += 1;
                return;
            }
            3 => {
                self.fate[0] += 1;
                return;
            }
            _ => {}
        }
        let (sx0, sy0, sx1, sy1) = (su.scissor.0.max(0), su.scissor.1.max(0), su.scissor.2.min(EFB_W as i32 - 1), su.scissor.3.min(EFB_H as i32 - 1));
        let minx = t.iter().map(|v| v.x).fold(f32::MAX, f32::min).floor().max(sx0 as f32) as i32;
        let maxx = t.iter().map(|v| v.x).fold(f32::MIN, f32::max).ceil().min(sx1 as f32) as i32;
        let miny = t.iter().map(|v| v.y).fold(f32::MAX, f32::min).floor().max(sy0 as f32) as i32;
        let maxy = t.iter().map(|v| v.y).fold(f32::MIN, f32::max).ceil().min(sy1 as f32) as i32;
        if minx > maxx || miny > maxy {
            self.fate[2] += 1;
            return;
        }
        let inv_area = 1.0 / area;
        for y in miny..=maxy {
            for x in minx..=maxx {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let w0 = ((t[1].x - px) * (t[2].y - py) - (t[2].x - px) * (t[1].y - py)) * inv_area;
                let w1 = ((t[2].x - px) * (t[0].y - py) - (t[0].x - px) * (t[2].y - py)) * inv_area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let idx = y as usize * EFB_W + x as usize;
                let depth = ((w0 * t[0].z + w1 * t[1].z + w2 * t[2].z).clamp(0.0, 16_777_215.0)) as u32;
                // Early depth test.
                if su.z_en && !depth_pass(su.z_func, depth, self.efb_z[idx]) {
                    self.fate[4] += 1;
                    continue;
                }
                let iw = w0 * t[0].iw + w1 * t[1].iw + w2 * t[2].iw;
                if iw <= 0.0 {
                    continue;
                }
                let w = 1.0 / iw;
                let mut col = [[0.0f32; 4]; 2];
                for c in 0..2 {
                    for k in 0..4 {
                        col[c][k] = (w0 * t[0].col[c][k] + w1 * t[1].col[c][k] + w2 * t[2].col[c][k]) * w;
                    }
                }
                let mut tc = [[0.0f32; 3]; 8];
                for tt in 0..su.ntex.min(8) {
                    for k in 0..3 {
                        tc[tt][k] = (w0 * t[0].tc[tt][k] + w1 * t[1].tc[tt][k] + w2 * t[2].tc[tt][k]) * w;
                    }
                }
                let Some(out) = self.shade(su, &col, &tc) else {
                    self.fate[5] += 1;
                    continue;
                };
                self.fate[3] += 1;
                if su.z_en && su.z_upd {
                    self.efb_z[idx] = depth;
                }
                let dst = self.efb_c[idx];
                self.efb_c[idx] = blend(su, out, dst);
            }
        }
    }

    /// Runs the TEV stages for one pixel; None if the alpha test rejects it.
    fn shade(&self, su: &Setup, col: &[[f32; 4]; 2], tc: &[[f32; 3]; 8]) -> Option<[i32; 4]> {
        let mut reg = su.regs;
        let q = |v: f32| v.clamp(0.0, 255.0) as i32;
        let ras_c = [[q(col[0][0]), q(col[0][1]), q(col[0][2]), q(col[0][3])], [q(col[1][0]), q(col[1][1]), q(col[1][2]), q(col[1][3])]];
        for st in &su.stages {
            let ras_raw = match st.chan {
                0 => ras_c[0],
                1 => ras_c[1],
                _ => [0, 0, 0, 0],
            };
            let sw = su.swaps[st.ras_swap];
            let ras = [ras_raw[sw[0]], ras_raw[sw[1]], ras_raw[sw[2]], ras_raw[sw[3]]];
            let tex_raw = if st.tex_en {
                match su.textures[st.map].and_then(|i| self.tex_cache.get(i)) {
                    Some(tx) => {
                        let c = tc[st.coord];
                        let (sc_s, sc_t) = su.tc_scale[st.coord];
                        let (sc_s, sc_t) = if sc_s <= 1.0 && sc_t <= 1.0 { (tx.w as f32, tx.h as f32) } else { (sc_s, sc_t) };
                        let (s, t) = if c[2] != 0.0 && c[2] != 1.0 { (c[0] / c[2], c[1] / c[2]) } else { (c[0], c[1]) };
                        sample(tx, self.tex_meta(st.map), s * sc_s, t * sc_t)
                    }
                    None => [255, 255, 255, 255],
                }
            } else {
                [255, 255, 255, 255]
            };
            let sw = su.swaps[st.tex_swap];
            let tex = [tex_raw[sw[0]], tex_raw[sw[1]], tex_raw[sw[2]], tex_raw[sw[3]]];
            let konst_c = konst_color(st.kc, &su.konst);
            let konst_a = konst_alpha(st.ka, &su.konst);

            // Colour
            let pick = |sel: u32, ch: usize| -> i32 {
                match sel {
                    0 => reg[0][ch],
                    1 => reg[0][3],
                    2 => reg[1][ch],
                    3 => reg[1][3],
                    4 => reg[2][ch],
                    5 => reg[2][3],
                    6 => reg[3][ch],
                    7 => reg[3][3],
                    8 => tex[ch],
                    9 => tex[3],
                    10 => ras[ch],
                    11 => ras[3],
                    12 => 255,
                    13 => 128,
                    14 => konst_c[ch],
                    _ => 0,
                }
            };
            let mut res = [0i32; 3];
            for ch in 0..3 {
                let (a, b, c, d) = (pick(st.ca, ch).clamp(0, 255), pick(st.cb, ch).clamp(0, 255), pick(st.cc, ch).clamp(0, 255), pick(st.cd, ch));
                res[ch] = combine(a, b, c, d, st.csub, st.cbias, st.cscale, st.cclamp);
            }
            let apick = |sel: u32| -> i32 {
                match sel {
                    0 => reg[0][3],
                    1 => reg[1][3],
                    2 => reg[2][3],
                    3 => reg[3][3],
                    4 => tex[3],
                    5 => ras[3],
                    6 => konst_a,
                    _ => 0,
                }
            };
            let (a, b, c, d) = (apick(st.aa).clamp(0, 255), apick(st.ab).clamp(0, 255), apick(st.ac).clamp(0, 255), apick(st.ad));
            let ra = combine(a, b, c, d, st.asub, st.abias, st.ascale, st.aclamp);
            let dst = &mut reg[st.cdst];
            dst[0] = res[0];
            dst[1] = res[1];
            dst[2] = res[2];
            reg[st.adst][3] = ra;
        }
        let out = [reg[0][0].clamp(0, 255), reg[0][1].clamp(0, 255), reg[0][2].clamp(0, 255), reg[0][3].clamp(0, 255)];
        // Alpha test.
        let c0 = compare(su.a_comp[0], out[3], su.a_ref[0]);
        let c1 = compare(su.a_comp[1], out[3], su.a_ref[1]);
        let pass = match su.a_logic {
            0 => c0 && c1,
            1 => c0 || c1,
            2 => c0 != c1,
            _ => c0 == c1,
        };
        pass.then_some(out)
    }

    // ------------------------------------------------------------ EFB copy

    pub(crate) fn efb_copy(&mut self, v: u32, ram: &mut [u8]) {
        self.copies += 1;
        let src = self.bp(0x49);
        let size = self.bp(0x4A);
        let (x0, y0) = ((src & 0x3FF) as usize, ((src >> 10) & 0x3FF) as usize);
        let (w, h) = ((size & 0x3FF) as usize + 1, ((size >> 10) & 0x3FF) as usize + 1);
        let dst = (self.bp(0x4B) << 5) as usize;
        let stride = ((self.bp(0x4D) & 0x3FF) as usize) << 5;
        let to_xfb = v & (1 << 14) != 0;
        let clear = v & (1 << 11) != 0;
        if std::env::var_os("GX_DEBUG").is_some() && self.copies < 40 {
            eprintln!("COPY v={v:#x} src=({x0},{y0}) {w}x{h} dst={dst:#x} stride={stride} scale={:#x} xfb={to_xfb} clear={clear}", self.bp(0x4E));
        }
        if let (Ok(at), Ok(file)) = (std::env::var("GX_DUMP_AT"), std::env::var("GX_DUMP_FILE")) {
            if at.parse::<u64>().ok() == Some(self.copies) {
                let mut out = format!("P6 {EFB_W} {EFB_H} 255
").into_bytes();
                for p in &self.efb_c {
                    out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
                }
                let _ = std::fs::write(file, out);
            }
        }
        if to_xfb {
            self.last_copy = (dst, stride, w, h);
            let scale = (self.bp(0x4E) & 0x1FF).max(1) as usize;
            let out_h = (h * 256 / scale).max(1);
            let stride = if stride == 0 { w * 2 } else { stride };
            for dy in 0..out_h {
                let sy = (y0 + dy * scale / 256).min(EFB_H - 1);
                let row = dst + dy * stride;
                for dx in (0..w).step_by(2) {
                    let a = row + dx * 2;
                    if a + 4 > ram.len() {
                        break;
                    }
                    let p0 = self.efb_c[sy * EFB_W + (x0 + dx).min(EFB_W - 1)];
                    let p1 = self.efb_c[sy * EFB_W + (x0 + dx + 1).min(EFB_W - 1)];
                    let (y_0, u0, v0) = rgb_to_yuv(p0);
                    let (y_1, u1, v1) = rgb_to_yuv(p1);
                    ram[a] = y_0;
                    ram[a + 1] = ((u0 as u32 + u1 as u32) / 2) as u8;
                    ram[a + 2] = y_1;
                    ram[a + 3] = ((v0 as u32 + v1 as u32) / 2) as u8;
                }
            }
        } else {
            self.copy_to_texture(v, ram, x0, y0, w, h, dst);
        }
        if clear {
            let ar = self.bp(0x4F);
            let gb = self.bp(0x50);
            let color = (ar >> 8 & 0xFF) << 24 | (ar & 0xFF) << 16 | (gb >> 8 & 0xFF) << 8 | (gb & 0xFF);
            let z = self.bp(0x51) & 0xFF_FFFF;
            for y in y0..(y0 + h).min(EFB_H) {
                for x in x0..(x0 + w).min(EFB_W) {
                    self.efb_c[y * EFB_W + x] = color;
                    self.efb_z[y * EFB_W + x] = z;
                }
            }
        }
    }

    /// Copies a region of the EFB into main memory as a tiled texture (for effects).
    #[allow(clippy::too_many_arguments)]
    fn copy_to_texture(&mut self, v: u32, ram: &mut [u8], x0: usize, y0: usize, w: usize, h: usize, dst: usize) {
        let fmt = ((v >> 4) & 7) | ((v >> 3) & 1) << 3;
        let px = |x: usize, y: usize| self.efb_c[(y0 + y).min(EFB_H - 1) * EFB_W + (x0 + x).min(EFB_W - 1)];
        let (tw, th) = match fmt {
            0 | 0xE => (8, 8),
            1 | 2 | 0x8 | 0x9 | 0xA | 0xB | 0xC => (8, 4),
            _ => (4, 4),
        };
        if std::env::var_os("GX_TEXLOG").is_some() {
            eprintln!("COPYTEX fmt={fmt:#x} v={v:#x} x0={x0} y0={y0} {w}x{h} dst={dst:#x}");
        }
        let mut at = dst;
        let gray = |c: u32| ((c >> 16 & 0xFF) * 77 + (c >> 8 & 0xFF) * 150 + (c & 0xFF) * 29) >> 8;
        for ty in 0..h.div_ceil(th) {
            for tx in 0..w.div_ceil(tw) {
                let need = if fmt == 6 { 64 } else { 32 };
                if at + need > ram.len() {
                    return;
                }
                for y in 0..th {
                    for x in 0..tw {
                        let c = px(tx * tw + x, ty * th + y);
                        let (a, r, g, b) = (c >> 24, c >> 16 & 0xFF, c >> 8 & 0xFF, c & 0xFF);
                        match fmt {
                            0 | 0x8 => {
                                // I4
                                let i = gray(c) >> 4;
                                let o = at + y * 4 + x / 2;
                                ram[o] = if x & 1 == 0 { (ram[o] & 0x0F) | (i as u8) << 4 } else { (ram[o] & 0xF0) | i as u8 };
                            }
                            1 | 0x9 | 0xA | 0xB | 0xC => ram[at + y * 8 + x] = if fmt == 0xB || fmt == 0xC { gray(c) as u8 } else { gray(c) as u8 },
                            2 => ram[at + y * 8 + x] = (a >> 4 << 4 | gray(c) >> 4) as u8,
                            3 => {
                                let o = at + (y * 4 + x) * 2;
                                ram[o] = a as u8;
                                ram[o + 1] = gray(c) as u8;
                            }
                            4 => {
                                let o = at + (y * 4 + x) * 2;
                                let v = ((r >> 3) << 11 | (g >> 2) << 5 | b >> 3) as u16;
                                ram[o..o + 2].copy_from_slice(&v.to_be_bytes());
                            }
                            5 => {
                                let o = at + (y * 4 + x) * 2;
                                let v = (0x8000 | (r >> 3) << 10 | (g >> 3) << 5 | b >> 3) as u16;
                                ram[o..o + 2].copy_from_slice(&v.to_be_bytes());
                            }
                            6 => {
                                let o = at + (y * 4 + x) * 2;
                                ram[o] = a as u8;
                                ram[o + 1] = r as u8;
                                ram[o + 32] = g as u8;
                                ram[o + 33] = b as u8;
                            }
                            _ => {}
                        }
                    }
                }
                at += need;
            }
        }
    }
}

fn unpack(v: u32) -> [f32; 4] {
    [(v >> 24) as f32, ((v >> 16) & 0xFF) as f32, ((v >> 8) & 0xFF) as f32, (v & 0xFF) as f32]
}

fn lerp_cv(a: &CV, b: &CV, t: f32) -> CV {
    let l = |x: f32, y: f32| x + (y - x) * t;
    let mut o = CV { x: l(a.x, b.x), y: l(a.y, b.y), z: l(a.z, b.z), w: l(a.w, b.w), ..Default::default() };
    for c in 0..2 {
        for k in 0..4 {
            o.col[c][k] = l(a.col[c][k], b.col[c][k]);
        }
    }
    for t2 in 0..8 {
        for k in 0..3 {
            o.tc[t2][k] = l(a.tc[t2][k], b.tc[t2][k]);
        }
    }
    o
}

fn depth_pass(func: u32, new: u32, old: u32) -> bool {
    match func {
        0 => false,
        1 => new < old,
        2 => new == old,
        3 => new <= old,
        4 => new > old,
        5 => new != old,
        6 => new >= old,
        _ => true,
    }
}

fn compare(func: u32, v: i32, r: i32) -> bool {
    match func {
        0 => false,
        1 => v < r,
        2 => v == r,
        3 => v <= r,
        4 => v > r,
        5 => v != r,
        6 => v >= r,
        _ => true,
    }
}

fn combine(a: i32, b: i32, c: i32, d: i32, sub: bool, bias: u32, scale: u32, clamp: bool) -> i32 {
    let c2 = c + (c >> 7);
    let lerp = (a * 256 + (b - a) * c2 + 128) >> 8;
    let mut v = if sub { d - lerp } else { d + lerp };
    v += match bias {
        1 => 128,
        2 => -128,
        _ => 0,
    };
    v = match scale {
        1 => v * 2,
        2 => v * 4,
        3 => v >> 1,
        _ => v,
    };
    if clamp { v.clamp(0, 255) } else { v.clamp(-1024, 1023) }
}

fn konst_color(sel: u32, k: &[[i32; 4]; 4]) -> [i32; 3] {
    let fr = |n: i32| [n; 3];
    match sel {
        0 => fr(255),
        1 => fr(223),
        2 => fr(191),
        3 => fr(159),
        4 => fr(128),
        5 => fr(96),
        6 => fr(64),
        7 => fr(32),
        12..=15 => {
            let c = k[(sel - 12) as usize];
            [c[0], c[1], c[2]]
        }
        16..=19 => fr(k[(sel - 16) as usize][0]),
        20..=23 => fr(k[(sel - 20) as usize][1]),
        24..=27 => fr(k[(sel - 24) as usize][2]),
        28..=31 => fr(k[(sel - 28) as usize][3]),
        _ => fr(255),
    }
}

fn konst_alpha(sel: u32, k: &[[i32; 4]; 4]) -> i32 {
    match sel {
        0 => 255,
        1 => 223,
        2 => 191,
        3 => 159,
        4 => 128,
        5 => 96,
        6 => 64,
        7 => 32,
        16..=19 => k[(sel - 16) as usize][0],
        20..=23 => k[(sel - 20) as usize][1],
        24..=27 => k[(sel - 24) as usize][2],
        28..=31 => k[(sel - 28) as usize][3],
        _ => 255,
    }
}

fn blend(su: &Setup, s: [i32; 4], dst: u32) -> u32 {
    let d = [(dst >> 16 & 0xFF) as i32, (dst >> 8 & 0xFF) as i32, (dst & 0xFF) as i32, (dst >> 24) as i32];
    let mut o = [s[0], s[1], s[2], s[3]];
    if su.blend_en {
        let factor = |f: u32, is_src: bool, ch: usize| -> i32 {
            match f {
                0 => 0,
                1 => 255,
                2 => if is_src { d[ch] } else { s[ch] },
                3 => 255 - if is_src { d[ch] } else { s[ch] },
                4 => s[3],
                5 => 255 - s[3],
                6 => d[3],
                _ => 255 - d[3],
            }
        };
        for ch in 0..4 {
            let sf = factor(su.src_f, true, ch);
            let df = factor(su.dst_f, false, ch);
            let v = if su.sub { (d[ch] * df - s[ch] * sf) / 255 } else { (s[ch] * sf + d[ch] * df) / 255 };
            o[ch] = v.clamp(0, 255);
        }
    }
    let a = if su.alpha_upd { o[3] } else { d[3] };
    let (r, g, b) = if su.color_upd { (o[0], o[1], o[2]) } else { (d[0], d[1], d[2]) };
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn wrap(mode: u32, i: i32, n: i32) -> i32 {
    match mode {
        0 => i.clamp(0, n - 1),
        1 => i.rem_euclid(n),
        _ => {
            let m = i.rem_euclid(2 * n);
            if m < n { m } else { 2 * n - 1 - m }
        }
    }
}

fn sample(tx: &TexEntry, meta: TexMeta, s: f32, t: f32) -> [i32; 4] {
    let (w, h) = (tx.w as i32, tx.h as i32);
    let fetch = |x: i32, y: i32| -> [i32; 4] {
        let c = tx.px[wrap(meta.wrap_t, y, h) as usize * tx.w + wrap(meta.wrap_s, x, w) as usize];
        [(c >> 16 & 0xFF) as i32, (c >> 8 & 0xFF) as i32, (c & 0xFF) as i32, (c >> 24) as i32]
    };
    if !meta.linear {
        return fetch(s.floor() as i32, t.floor() as i32);
    }
    let (fs, ft) = (s - 0.5, t - 0.5);
    let (x0, y0) = (fs.floor() as i32, ft.floor() as i32);
    let (ax, ay) = (((fs - x0 as f32) * 256.0) as i32, ((ft - y0 as f32) * 256.0) as i32);
    let (p00, p10, p01, p11) = (fetch(x0, y0), fetch(x0 + 1, y0), fetch(x0, y0 + 1), fetch(x0 + 1, y0 + 1));
    let mut o = [0; 4];
    for k in 0..4 {
        let top = p00[k] * (256 - ax) + p10[k] * ax;
        let bot = p01[k] * (256 - ax) + p11[k] * ax;
        o[k] = (top * (256 - ay) + bot * ay) >> 16;
    }
    o
}

fn rgb_to_yuv(c: u32) -> (u8, u8, u8) {
    let (r, g, b) = ((c >> 16 & 0xFF) as f32, (c >> 8 & 0xFF) as f32, (c & 0xFF) as f32);
    let y = 16.0 + 0.257 * r + 0.504 * g + 0.098 * b;
    let u = 128.0 - 0.148 * r - 0.291 * g + 0.439 * b;
    let v = 128.0 + 0.439 * r - 0.368 * g - 0.071 * b;
    (y.clamp(0.0, 255.0) as u8, u.clamp(0.0, 255.0) as u8, v.clamp(0.0, 255.0) as u8)
}

fn base_of(tl_off: u32) -> usize {
    tl_off as usize * 256
}
