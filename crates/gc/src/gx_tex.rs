//! GX texture formats: the GameCube stores textures in tiles (4x4, 8x4 or 8x8 texels,
//! 32 bytes per tile) in several pixel formats. This decodes one into plain ARGB.

pub const I4: u32 = 0;
pub const I8: u32 = 1;
pub const IA4: u32 = 2;
pub const IA8: u32 = 3;
pub const RGB565: u32 = 4;
pub const RGB5A3: u32 = 5;
pub const RGBA8: u32 = 6;
pub const C4: u32 = 8;
pub const C8: u32 = 9;
pub const C14X2: u32 = 10;
pub const CMPR: u32 = 14;

/// Tile size in texels (width, height) for a format.
fn tile(fmt: u32) -> (usize, usize) {
    match fmt {
        I4 | C4 | CMPR => (8, 8),
        I8 | IA4 | C8 => (8, 4),
        _ => (4, 4),
    }
}

fn argb(a: u32, r: u32, g: u32, b: u32) -> u32 {
    a << 24 | r << 16 | g << 8 | b
}

fn expand5(v: u32) -> u32 {
    (v << 3) | (v >> 2)
}

fn expand6(v: u32) -> u32 {
    (v << 2) | (v >> 4)
}

fn expand4(v: u32) -> u32 {
    v * 17
}

fn expand3(v: u32) -> u32 {
    (v << 5) | (v << 2) | (v >> 1)
}

fn rgb565(v: u16) -> u32 {
    let v = v as u32;
    argb(255, expand5(v >> 11), expand6((v >> 5) & 0x3F), expand5(v & 0x1F))
}

fn rgb5a3(v: u16) -> u32 {
    let v = v as u32;
    if v & 0x8000 != 0 {
        argb(255, expand5((v >> 10) & 0x1F), expand5((v >> 5) & 0x1F), expand5(v & 0x1F))
    } else {
        argb(expand3((v >> 12) & 7), expand4((v >> 8) & 0xF), expand4((v >> 4) & 0xF), expand4(v & 0xF))
    }
}

/// Palette entry formats (TLUT): 0 = IA8, 1 = RGB565, 2 = RGB5A3.
pub fn palette_color(fmt: u32, v: u16) -> u32 {
    match fmt {
        0 => argb((v >> 8) as u32, (v & 0xFF) as u32, (v & 0xFF) as u32, (v & 0xFF) as u32),
        1 => rgb565(v),
        _ => rgb5a3(v),
    }
}

/// Bytes the whole texture occupies in memory (tiles are padded to full size).
pub fn byte_len(fmt: u32, w: usize, h: usize) -> usize {
    let (tw, th) = tile(fmt);
    let tiles = w.div_ceil(tw) * h.div_ceil(th);
    tiles * if fmt == RGBA8 { 64 } else { 32 }
}

fn be16(d: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([*d.get(i).unwrap_or(&0), *d.get(i + 1).unwrap_or(&0)])
}

/// Decodes a `w` x `h` texture. `palette` supplies the colours for the C formats.
pub fn decode(data: &[u8], fmt: u32, w: usize, h: usize, palette: &dyn Fn(usize) -> u32) -> Vec<u32> {
    let mut out = vec![0u32; w * h];
    let (tw, th) = tile(fmt);
    let tiles_x = w.div_ceil(tw);
    let tiles_y = h.div_ceil(th);
    let mut at = 0usize;
    let byte = |i: usize| *data.get(i).unwrap_or(&0) as u32;
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let (x0, y0) = (tx * tw, ty * th);
            let mut put = |x: usize, y: usize, c: u32| {
                let (px, py) = (x0 + x, y0 + y);
                if px < w && py < h {
                    out[py * w + px] = c;
                }
            };
            match fmt {
                I4 | C4 => {
                    for y in 0..8 {
                        for x in 0..8 {
                            let b = byte(at + y * 4 + x / 2);
                            let n = if x & 1 == 0 { b >> 4 } else { b & 15 };
                            put(x, y, if fmt == I4 { let i = expand4(n); argb(i, i, i, i) } else { palette(n as usize) });
                        }
                    }
                    at += 32;
                }
                I8 | C8 => {
                    for y in 0..4 {
                        for x in 0..8 {
                            let n = byte(at + y * 8 + x);
                            put(x, y, if fmt == I8 { argb(n, n, n, n) } else { palette(n as usize) });
                        }
                    }
                    at += 32;
                }
                IA4 => {
                    for y in 0..4 {
                        for x in 0..8 {
                            let b = byte(at + y * 8 + x);
                            let (a, i) = (expand4(b >> 4), expand4(b & 15));
                            put(x, y, argb(a, i, i, i));
                        }
                    }
                    at += 32;
                }
                IA8 => {
                    for y in 0..4 {
                        for x in 0..4 {
                            let (a, i) = (byte(at + (y * 4 + x) * 2), byte(at + (y * 4 + x) * 2 + 1));
                            put(x, y, argb(a, i, i, i));
                        }
                    }
                    at += 32;
                }
                RGB565 | RGB5A3 | C14X2 => {
                    for y in 0..4 {
                        for x in 0..4 {
                            let v = be16(data, at + (y * 4 + x) * 2);
                            put(
                                x,
                                y,
                                match fmt {
                                    RGB565 => rgb565(v),
                                    RGB5A3 => rgb5a3(v),
                                    _ => palette((v & 0x3FFF) as usize),
                                },
                            );
                        }
                    }
                    at += 32;
                }
                RGBA8 => {
                    for y in 0..4 {
                        for x in 0..4 {
                            let i = (y * 4 + x) * 2;
                            let (a, r) = (byte(at + i), byte(at + i + 1));
                            let (g, b) = (byte(at + 32 + i), byte(at + 32 + i + 1));
                            put(x, y, argb(a, r, g, b));
                        }
                    }
                    at += 64;
                }
                CMPR => {
                    // Four 4x4 DXT1 blocks per 8x8 tile.
                    for blk in 0..4 {
                        let (bx, by) = ((blk & 1) * 4, (blk >> 1) * 4);
                        let base = at + blk * 8;
                        let c0 = be16(data, base);
                        let c1 = be16(data, base + 2);
                        let (a, b) = (rgb565(c0), rgb565(c1));
                        let ch = |c: u32, s: u32| (c >> s) & 0xFF;
                        let mix = |w0: u32, w1: u32, d: u32| {
                            let m = |s| (ch(a, s) * w0 + ch(b, s) * w1) / d;
                            argb(255, m(16), m(8), m(0))
                        };
                        let pal = if c0 > c1 {
                            [a, b, mix(2, 1, 3), mix(1, 2, 3)]
                        } else {
                            [a, b, mix(1, 1, 2), 0]
                        };
                        for y in 0..4 {
                            let row = byte(base + 4 + y);
                            for x in 0..4 {
                                let idx = (row >> (6 - 2 * x)) & 3;
                                put(bx + x, by + y, pal[idx as usize]);
                            }
                        }
                    }
                    at += 32;
                }
                _ => {
                    at += 32;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb565_tile_is_row_major_within_the_tile() {
        let mut d = vec![0u8; 32 * 2];
        // First tile, texel (1,0) = pure red; second tile, texel (0,0) = pure green.
        d[2..4].copy_from_slice(&0xF800u16.to_be_bytes());
        d[32..34].copy_from_slice(&0x07E0u16.to_be_bytes());
        let t = decode(&d, RGB565, 8, 4, &|_| 0);
        assert_eq!(t[1], 0xFFFF0000);
        assert_eq!(t[4], 0xFF00FF00);
        assert_eq!(t[0], 0xFF000000);
    }

    #[test]
    fn i4_and_palette() {
        let mut d = vec![0u8; 32];
        d[0] = 0xF1;
        let t = decode(&d, I4, 8, 8, &|_| 0);
        assert_eq!(t[0], 0xFFFFFFFF);
        assert_eq!(t[1], 0x11111111);
        let c = decode(&d, C4, 8, 8, &|n| 0xFF000000 | n as u32);
        assert_eq!(c[0], 0xFF00000F);
        assert_eq!(c[1], 0xFF000001);
    }

    #[test]
    fn cmpr_block_interpolates() {
        let mut d = vec![0u8; 32];
        d[0..2].copy_from_slice(&0xFFFFu16.to_be_bytes()); // white
        d[2..4].copy_from_slice(&0x0000u16.to_be_bytes()); // black
        d[4] = 0b00_01_10_11; // texels 0..3 of row 0 use indices 0,1,2,3
        let t = decode(&d, CMPR, 8, 8, &|_| 0);
        assert_eq!(t[0], 0xFFFFFFFF);
        assert_eq!(t[1], 0xFF000000);
        assert_eq!(t[2] & 0xFF, 0xAA, "two thirds white");
        assert_eq!(t[3] & 0xFF, 0x55);
    }
}
