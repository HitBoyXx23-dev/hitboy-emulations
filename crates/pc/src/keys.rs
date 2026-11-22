//! PC/AT keyboard scan codes (set 1) and code page 437.

pub const ESC: u8 = 0x01;
pub const BACKSPACE: u8 = 0x0E;
pub const TAB: u8 = 0x0F;
pub const ENTER: u8 = 0x1C;
pub const CTRL: u8 = 0x1D;
pub const LSHIFT: u8 = 0x2A;
pub const ALT: u8 = 0x38;
pub const SPACE: u8 = 0x39;
pub const F1: u8 = 0x3B;
pub const F11: u8 = 0x57;
pub const F12: u8 = 0x58;
pub const HOME: u8 = 0x47;
pub const UP: u8 = 0x48;
pub const PGUP: u8 = 0x49;
pub const LEFT: u8 = 0x4B;
pub const RIGHT: u8 = 0x4D;
pub const END: u8 = 0x4F;
pub const DOWN: u8 = 0x50;
pub const PGDN: u8 = 0x51;
pub const INSERT: u8 = 0x52;
pub const DELETE: u8 = 0x53;

const ROWS: [(u8, &str, &str); 4] = [
    (0x02, "1234567890-=", "!@#$%^&*()_+"),
    (0x10, "qwertyuiop[]", "QWERTYUIOP{}"),
    (0x1E, "asdfghjkl;'`", "ASDFGHJKL:\"~"),
    (0x2B, "\\zxcvbnm,./", "|ZXCVBNM<>?"),
];

/// Scan code of the key that types `c` on a US keyboard.
pub fn scan_for_char(c: char) -> Option<u8> {
    if c == ' ' {
        return Some(SPACE);
    }
    ROWS.iter().find_map(|(base, lower, upper)| {
        lower.chars().position(|x| x == c).or_else(|| upper.chars().position(|x| x == c)).map(|i| base + i as u8)
    })
}

/// Code page 437 to Unicode, for drawing text-mode screens.
pub const CP437: [char; 256] = {
    const S: &str = " ☺☻♥♦♣♠•◘○◙♂♀♪♫☼►◄↕‼¶§▬↨↑↓→←∟↔▲▼ !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~⌂ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■ ";
    let bytes = S.as_bytes();
    let mut out = [' '; 256];
    let (mut i, mut n) = (0, 0);
    while i < bytes.len() && n < 256 {
        // Minimal UTF-8 decode (const context).
        let b0 = bytes[i] as u32;
        let (cp, len) = if b0 < 0x80 {
            (b0, 1)
        } else if b0 < 0xE0 {
            (((b0 & 0x1F) << 6) | (bytes[i + 1] as u32 & 0x3F), 2)
        } else {
            (((b0 & 0x0F) << 12) | ((bytes[i + 1] as u32 & 0x3F) << 6) | (bytes[i + 2] as u32 & 0x3F), 3)
        };
        out[n] = match char::from_u32(cp) {
            Some(c) => c,
            None => ' ',
        };
        i += len;
        n += 1;
    }
    out
};
