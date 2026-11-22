//! Display adapter: text modes (B8000/B0000), VGA mode 13h (A0000) and CGA graphics.
//! Video state lives in guest memory (BIOS Data Area) exactly like on real hardware.

pub const BDA_MODE: usize = 0x449;
pub const BDA_COLS: usize = 0x44A;
pub const BDA_CURSOR: usize = 0x450;
pub const BDA_CURSOR_SHAPE: usize = 0x460;
pub const BDA_ROWS: usize = 0x484;

/// CGA/EGA 16-colour text palette.
pub const TEXT_PALETTE: [u32; 16] = [
    0x000000, 0x0000AA, 0x00AA00, 0x00AAAA, 0xAA0000, 0xAA00AA, 0xAA5500, 0xAAAAAA,
    0x555555, 0x5555FF, 0x55FF55, 0x55FFFF, 0xFF5555, 0xFF55FF, 0xFFFF55, 0xFFFFFF,
];

/// VESA modes offered by the BIOS: (mode number, width, height, bits per pixel).
pub const VBE_MODES: &[(u16, u16, u16, u8)] = &[
    (0x101, 640, 480, 8),
    (0x110, 640, 480, 15),
    (0x111, 640, 480, 16),
    (0x112, 640, 480, 32),
    (0x103, 800, 600, 8),
    (0x113, 800, 600, 15),
    (0x114, 800, 600, 16),
    (0x115, 800, 600, 32),
    (0x105, 1024, 768, 8),
    (0x116, 1024, 768, 15),
    (0x117, 1024, 768, 16),
    (0x118, 1024, 768, 32),
    (0x11A, 1280, 1024, 16),
    (0x11B, 1280, 1024, 32),
];

pub fn vbe_mode(n: u16) -> Option<(u16, u16, u8)> {
    VBE_MODES.iter().find(|m| m.0 == n & 0x1FF).map(|m| (m.1, m.2, m.3))
}

/// Bytes per pixel for a VESA bit depth.
pub fn vbe_bytes(bpp: u8) -> usize {
    (bpp as usize).div_ceil(8)
}

/// The VESA linear framebuffer as RGB pixels.
pub fn render_vbe(vram: &[u8], mode: u16, dac: &[[u8; 3]; 256]) -> Screen {
    let Some((w, h, bpp)) = vbe_mode(mode) else { return Screen::Graphics { width: 640, height: 480, pixels: vec![0; 640 * 480] } };
    let (w, h) = (w as usize, h as usize);
    let bytes = vbe_bytes(bpp);
    let mut pixels = vec![0u32; w * h];
    for (i, p) in pixels.iter_mut().enumerate() {
        let a = i * bytes;
        if a + bytes > vram.len() {
            break;
        }
        *p = match bpp {
            8 => {
                let c = dac[vram[a] as usize];
                ((c[0] as u32 * 255 / 63) << 16) | ((c[1] as u32 * 255 / 63) << 8) | (c[2] as u32 * 255 / 63)
            }
            15 => {
                let v = u16::from_le_bytes([vram[a], vram[a + 1]]) as u32;
                ((v >> 10) & 31) * 255 / 31 << 16 | ((v >> 5) & 31) * 255 / 31 << 8 | (v & 31) * 255 / 31
            }
            16 => {
                let v = u16::from_le_bytes([vram[a], vram[a + 1]]) as u32;
                ((v >> 11) & 31) * 255 / 31 << 16 | ((v >> 5) & 63) * 255 / 63 << 8 | (v & 31) * 255 / 31
            }
            _ => u32::from_le_bytes([vram[a], vram[a + 1], vram[a + 2], 0]),
        };
    }
    Screen::Graphics { width: w, height: h, pixels }
}

pub enum Screen {
    Text { cols: usize, rows: usize, cells: Vec<u16>, cursor: Option<(usize, usize)> },
    Graphics { width: usize, height: usize, pixels: Vec<u32> },
}

/// Default VGA DAC palette (6-bit components): 16 EGA colours, a grey ramp, then a colour cube.
pub fn default_dac() -> [[u8; 3]; 256] {
    let mut p = [[0u8; 3]; 256];
    for (i, c) in TEXT_PALETTE.iter().enumerate() {
        p[i] = [((c >> 16) & 0xFF) as u8 >> 2, ((c >> 8) & 0xFF) as u8 >> 2, (c & 0xFF) as u8 >> 2];
    }
    for i in 0..16 {
        let v = (i * 63 / 15) as u8;
        p[16 + i] = [v, v, v];
    }
    let levels = [0u8, 16, 31, 47, 63, 55];
    let mut i = 32;
    'cube: for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                if i >= 248 {
                    break 'cube;
                }
                p[i] = [levels[r], levels[g], levels[b]];
                i += 1;
            }
        }
    }
    p
}

fn text_base(mode: u8) -> usize {
    if mode == 7 { 0xB0000 } else { 0xB8000 }
}

pub fn is_text(mode: u8) -> bool {
    matches!(mode, 0..=3 | 7)
}

pub fn cols(mem: &[u8]) -> usize {
    (mem[BDA_COLS] as usize).max(40)
}

pub fn rows(mem: &[u8]) -> usize {
    mem[BDA_ROWS] as usize + 1
}

pub fn cursor(mem: &[u8]) -> (usize, usize) {
    (mem[BDA_CURSOR + 1] as usize, mem[BDA_CURSOR] as usize)
}

pub fn set_cursor(mem: &mut [u8], row: usize, col: usize) {
    mem[BDA_CURSOR] = col as u8;
    mem[BDA_CURSOR + 1] = row as u8;
}

pub fn set_mode(mem: &mut [u8], mode: u8, clear: bool) {
    let m = mode & 0x7F;
    mem[BDA_MODE] = m;
    mem[BDA_COLS] = if matches!(m, 0 | 1 | 4 | 5 | 0x0D | 0x13) { 40 } else { 80 };
    mem[BDA_ROWS] = 24;
    mem[0x44C..0x44E].copy_from_slice(&(if is_text(m) { 4000u16 } else { 0 }).to_le_bytes());
    set_cursor(mem, 0, 0);
    mem[BDA_CURSOR_SHAPE] = 0x0E;
    mem[BDA_CURSOR_SHAPE + 1] = 0x0D;
    if clear {
        if is_text(m) {
            let base = text_base(m);
            for i in 0..(80 * 25) {
                mem[base + i * 2] = b' ';
                mem[base + i * 2 + 1] = 0x07;
            }
        } else if m == 0x13 {
            mem[0xA0000..0xAFA00].fill(0);
        } else {
            mem[0xB8000..0xBC000].fill(0);
        }
    }
}

/// Scrolls a window up (or down) by `lines`; 0 clears it.
#[allow(clippy::too_many_arguments)]
pub fn scroll(mem: &mut [u8], up: bool, lines: usize, attr: u8, top: usize, left: usize, bottom: usize, right: usize) {
    let mode = mem[BDA_MODE];
    if !is_text(mode) {
        if mode == 0x13 && lines == 0 {
            mem[0xA0000..0xAFA00].fill(0);
        }
        return;
    }
    let (c, r) = (cols(mem), rows(mem));
    let base = text_base(mode);
    let (bottom, right) = (bottom.min(r - 1), right.min(c - 1));
    if top > bottom || left > right {
        return;
    }
    let height = bottom - top + 1;
    let n = if lines == 0 || lines > height { height } else { lines };
    let cell = |row: usize, col: usize| base + (row * c + col) * 2;
    let rows_iter: Vec<usize> = if up { (top..=bottom).collect() } else { (top..=bottom).rev().collect() };
    for &row in &rows_iter {
        let src = if up { row + n } else { row.wrapping_sub(n) };
        let valid = if up { src <= bottom } else { row >= top + n };
        for col in left..=right {
            let (ch, at) = if valid { (mem[cell(src, col)], mem[cell(src, col) + 1]) } else { (b' ', attr) };
            mem[cell(row, col)] = ch;
            mem[cell(row, col) + 1] = at;
        }
    }
}

/// Writes one character at the cursor like INT 10h/0Eh (teletype), handling control codes.
pub fn teletype(mem: &mut [u8], ch: u8, gfx_color: u8) {
    let mode = mem[BDA_MODE];
    let (c, r) = (cols(mem), rows(mem));
    let (mut row, mut col) = cursor(mem);
    match ch {
        7 => {}
        8 => col = col.saturating_sub(1),
        b'\n' => row += 1,
        b'\r' => col = 0,
        _ => {
            if is_text(mode) {
                let at = text_base(mode) + (row * c + col) * 2;
                mem[at] = ch;
            } else {
                let _ = gfx_color; // no font in graphics modes yet
            }
            col += 1;
        }
    }
    if col >= c {
        col = 0;
        row += 1;
    }
    if row >= r {
        let attr = if is_text(mode) { mem[text_base(mode) + ((r - 1) * c + c - 1) * 2 + 1] } else { 0 };
        scroll(mem, true, 1, if attr == 0 { 0x07 } else { attr }, 0, 0, r - 1, c - 1);
        row = r - 1;
    }
    set_cursor(mem, row, col);
}

pub fn write_cell(mem: &mut [u8], row: usize, col: usize, ch: u8, attr: Option<u8>) {
    let mode = mem[BDA_MODE];
    if !is_text(mode) {
        return;
    }
    let c = cols(mem);
    if row >= rows(mem) || col >= c {
        return;
    }
    let at = text_base(mode) + (row * c + col) * 2;
    mem[at] = ch;
    if let Some(a) = attr {
        mem[at + 1] = a;
    }
}

pub fn read_cell(mem: &[u8], row: usize, col: usize) -> (u8, u8) {
    let mode = mem[BDA_MODE];
    if !is_text(mode) {
        return (0, 0);
    }
    let at = text_base(mode) + (row * cols(mem) + col) * 2;
    (mem[at], mem[at + 1])
}

pub fn render(mem: &[u8], dac: &[[u8; 3]; 256]) -> Screen {
    let mode = mem[BDA_MODE];
    let rgb = |c: [u8; 3]| ((c[0] as u32 * 255 / 63) << 16) | ((c[1] as u32 * 255 / 63) << 8) | (c[2] as u32 * 255 / 63);
    match mode {
        0x13 => Screen::Graphics { width: 320, height: 200, pixels: mem[0xA0000..0xAFA00].iter().map(|&i| rgb(dac[i as usize])).collect() },
        4 | 5 => {
            // CGA 320x200x4, interleaved even/odd scanlines. Palette 1 (cyan/magenta/white).
            let pal = [0x000000, 0x55FFFF, 0xFF55FF, 0xFFFFFF];
            let mut pixels = vec![0u32; 320 * 200];
            for y in 0..200 {
                let row = 0xB8000 + (y & 1) * 0x2000 + (y / 2) * 80;
                for x in 0..320 {
                    let b = mem[row + x / 4];
                    pixels[y * 320 + x] = pal[((b >> (6 - (x % 4) * 2)) & 3) as usize];
                }
            }
            Screen::Graphics { width: 320, height: 200, pixels }
        }
        6 => {
            let mut pixels = vec![0u32; 640 * 200];
            for y in 0..200 {
                let row = 0xB8000 + (y & 1) * 0x2000 + (y / 2) * 80;
                for x in 0..640 {
                    if mem[row + x / 8] & (0x80 >> (x % 8)) != 0 {
                        pixels[y * 640 + x] = 0xFFFFFF;
                    }
                }
            }
            Screen::Graphics { width: 640, height: 200, pixels }
        }
        _ => {
            let (c, r) = (cols(mem), rows(mem).min(50));
            let base = text_base(mode);
            let cells = (0..c * r).map(|i| u16::from_le_bytes([mem[base + i * 2], mem[base + i * 2 + 1]])).collect();
            let hidden = mem[BDA_CURSOR_SHAPE + 1] & 0x20 != 0;
            let (row, col) = cursor(mem);
            Screen::Text { cols: c, rows: r, cells, cursor: (!hidden && row < r && col < c).then_some((row, col)) }
        }
    }
}
