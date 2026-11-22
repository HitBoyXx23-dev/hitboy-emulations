//! LZX decompression (the Microsoft format used by XEX images and CAB archives).
//!
//! The input is one continuous stream of 16-bit little-endian words read MSB first. Output is
//! produced in 32 KB frames; the bit stream realigns to a word boundary after each frame.

const FRAME: usize = 32768;
const MIN_MATCH: usize = 2;
const NUM_CHARS: usize = 256;
const PRETREE: usize = 20;
const LENGTH_TREE: usize = 249;
const ALIGNED: usize = 8;
const MAX_BITS: u32 = 16;

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    left: u32,
}

impl<'a> Bits<'a> {
    fn ensure(&mut self, n: u32) {
        while self.left < n {
            let lo = *self.data.get(self.pos).unwrap_or(&0) as u64;
            let hi = *self.data.get(self.pos + 1).unwrap_or(&0) as u64;
            self.pos += 2;
            self.buf |= (hi << 8 | lo) << (48 - self.left);
            self.left += 16;
        }
    }
    fn peek(&self, n: u32) -> u32 {
        (self.buf >> (64 - n)) as u32
    }
    fn remove(&mut self, n: u32) {
        self.buf <<= n;
        self.left -= n;
    }
    fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        self.ensure(n);
        let v = self.peek(n);
        self.remove(n);
        v
    }
    fn overrun(&self) -> bool {
        self.pos > self.data.len() + 4
    }
}

/// Canonical Huffman decoder with a direct 16-bit lookup table.
struct Tree {
    lens: Vec<u8>,
    table: Vec<u16>,
}

impl Tree {
    fn new(n: usize) -> Tree {
        Tree { lens: vec![0; n], table: Vec::new() }
    }

    fn build(&mut self) -> Result<(), String> {
        self.table.clear();
        if self.lens.iter().all(|&l| l == 0) {
            return Ok(()); // empty tree: valid as long as it's never used
        }
        self.table.resize(1 << MAX_BITS, u16::MAX);
        let mut code = 0u32;
        for len in 1..=MAX_BITS {
            for (sym, &l) in self.lens.iter().enumerate() {
                if l as u32 != len {
                    continue;
                }
                let span = 1u32 << (MAX_BITS - len);
                let start = code << (MAX_BITS - len);
                if start + span > 1 << MAX_BITS {
                    return Err("LZX: over-subscribed Huffman table".into());
                }
                self.table[start as usize..(start + span) as usize].fill(sym as u16);
                code += 1;
            }
            code <<= 1;
        }
        Ok(())
    }

    fn decode(&self, b: &mut Bits) -> Result<usize, String> {
        b.ensure(MAX_BITS);
        let sym = *self.table.get(b.peek(MAX_BITS) as usize).ok_or("LZX: empty Huffman table used")?;
        if sym == u16::MAX {
            return Err("LZX: invalid Huffman code".into());
        }
        b.remove(self.lens[sym as usize] as u32);
        Ok(sym as usize)
    }
}

fn read_lens(b: &mut Bits, lens: &mut [u8], first: usize, last: usize) -> Result<(), String> {
    let mut pre = Tree::new(PRETREE);
    for l in pre.lens.iter_mut() {
        *l = b.read(4) as u8;
    }
    pre.build()?;
    let mut x = first;
    while x < last {
        let z = pre.decode(b)?;
        match z {
            17 | 18 => {
                let run = if z == 17 { b.read(4) + 4 } else { b.read(5) + 20 } as usize;
                for _ in 0..run.min(last - x) {
                    lens[x] = 0;
                    x += 1;
                }
            }
            19 => {
                let run = (b.read(1) + 4) as usize;
                let z = pre.decode(b)? as i32;
                let v = ((lens[x] as i32 - z) + 17) % 17;
                for _ in 0..run.min(last - x) {
                    lens[x] = v as u8;
                    x += 1;
                }
            }
            _ => {
                lens[x] = (((lens[x] as i32 - z as i32) + 17) % 17) as u8;
                x += 1;
            }
        }
    }
    Ok(())
}

/// Decompresses `input` into exactly `out_len` bytes with a 2^`window_bits` window.
pub fn decompress(input: &[u8], window_bits: u32, out_len: usize) -> Result<Vec<u8>, String> {
    if !(15..=21).contains(&window_bits) {
        return Err(format!("LZX: unsupported window size 2^{window_bits}"));
    }
    let slots = match window_bits {
        20 => 42,
        21 => 50,
        n => n as usize * 2,
    };
    let mut extra = [0u32; 51];
    let mut base = [0u32; 51];
    let mut j = 0;
    for i in (0..51).step_by(2) {
        extra[i] = j;
        if i + 1 < 51 {
            extra[i + 1] = j;
        }
        if i != 0 && j < 17 {
            j += 1;
        }
    }
    let mut acc = 0u32;
    for i in 0..51 {
        base[i] = acc;
        acc = acc.wrapping_add(1 << extra[i]);
    }

    let mut b = Bits { data: input, pos: 0, buf: 0, left: 0 };
    let mut out: Vec<u8> = Vec::with_capacity(out_len);
    let mut main = Tree::new(NUM_CHARS + slots * 8);
    let mut length = Tree::new(LENGTH_TREE);
    let mut aligned = Tree::new(ALIGNED);
    let (mut r0, mut r1, mut r2) = (1usize, 1usize, 1usize);
    let mut block_type = 0u32;
    let mut block_len = 0usize;
    let mut block_left = 0usize;

    let intel_size = if b.read(1) == 1 { (b.read(16) << 16 | b.read(16)) as i64 } else { 0 };

    let mut frame_start = 0usize;
    let mut fin: Vec<u8> = Vec::with_capacity(out_len);
    while out.len() < out_len {
        let frame_end = (frame_start + FRAME).min(out_len);
        while out.len() < frame_end {
            if block_left == 0 {
                if block_type == 3 && block_len & 1 == 1 {
                    b.pos += 1; // uncompressed blocks are padded to a word
                }
                block_type = b.read(3);
                block_len = (b.read(16) << 8 | b.read(8)) as usize;
                block_left = block_len;
                match block_type {
                    2 | 1 => {
                        if block_type == 2 {
                            for l in aligned.lens.iter_mut() {
                                *l = b.read(3) as u8;
                            }
                            aligned.build()?;
                        }
                        let n = main.lens.len();
                        read_lens(&mut b, &mut main.lens, 0, NUM_CHARS)?;
                        read_lens(&mut b, &mut main.lens, NUM_CHARS, n)?;
                        main.build()?;
                        read_lens(&mut b, &mut length.lens, 0, LENGTH_TREE)?;
                        length.build()?;
                    }
                    3 => {
                        if b.left == 0 {
                            b.ensure(16);
                        }
                        b.left = 0;
                        b.buf = 0;
                        let rd = |p: usize| input.get(p..p + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize).unwrap_or(0);
                        r0 = rd(b.pos);
                        r1 = rd(b.pos + 4);
                        r2 = rd(b.pos + 8);
                        b.pos += 12;
                    }
                    t => return Err(format!("LZX: bad block type {t}")),
                }
                if block_len == 0 {
                    return Err("LZX: empty block".into());
                }
            }
            if b.overrun() {
                return Err("LZX: input ended early".into());
            }
            // Decode up to the end of this frame or block, whichever comes first.
            let run_end = (out.len() + block_left).min(frame_end);
            let before = out.len();
            if block_type == 3 {
                let n = run_end - out.len();
                let src = input.get(b.pos..b.pos + n).ok_or("LZX: uncompressed block truncated")?;
                out.extend_from_slice(src);
                b.pos += n;
            } else {
                while out.len() < run_end {
                    let sym = main.decode(&mut b)?;
                    if sym < NUM_CHARS {
                        out.push(sym as u8);
                        continue;
                    }
                    let m = sym - NUM_CHARS;
                    let mut len = m & 7;
                    if len == 7 {
                        len += length.decode(&mut b)?;
                    }
                    len += MIN_MATCH;
                    let slot = m >> 3;
                    let offset = if slot > 2 {
                        let x = extra[slot.min(50)];
                        let mut o = base[slot.min(50)] as usize - 2;
                        if block_type == 2 {
                            if x > 3 {
                                o += (b.read(x - 3) as usize) << 3;
                                o += aligned.decode(&mut b)?;
                            } else if x == 3 {
                                o += aligned.decode(&mut b)?;
                            } else if x > 0 {
                                o += b.read(x) as usize;
                            } else {
                                o = 1;
                            }
                        } else if slot != 3 {
                            o += b.read(x) as usize;
                        } else {
                            o = 1;
                        }
                        r2 = r1;
                        r1 = r0;
                        r0 = o;
                        o
                    } else if slot == 0 {
                        r0
                    } else if slot == 1 {
                        std::mem::swap(&mut r0, &mut r1);
                        r0
                    } else {
                        std::mem::swap(&mut r0, &mut r2);
                        r0
                    };
                    if offset == 0 || offset > out.len() {
                        return Err("LZX: match reaches before the start of the data".into());
                    }
                    let start = out.len() - offset;
                    for i in 0..len {
                        let c = out[start + i];
                        out.push(c);
                    }
                }
            }
            let produced = out.len() - before;
            block_left = block_left.saturating_sub(produced);
        }
        // Intel E8 call translation, undone per frame on the output copy only: later matches
        // must still see the untranslated window.
        let end = out.len().min(frame_start + FRAME);
        let from = fin.len();
        fin.extend_from_slice(&out[from..end]);
        if intel_size != 0 && frame_start < FRAME * 32768 {
            let mut i = frame_start;
            while i + 10 < end {
                if fin[i] == 0xE8 {
                    let abs = i32::from_le_bytes([fin[i + 1], fin[i + 2], fin[i + 3], fin[i + 4]]) as i64;
                    let cur = i as i64;
                    if abs >= -cur && abs < intel_size {
                        let rel = if abs >= 0 { abs - cur } else { abs + intel_size };
                        fin[i + 1..i + 5].copy_from_slice(&(rel as i32).to_le_bytes());
                    }
                    i += 5;
                } else {
                    i += 1;
                }
            }
        }
        // Realign the bit stream to a word boundary after each frame.
        if b.left > 0 {
            b.ensure(16);
        }
        let pad = b.left & 15;
        if pad != 0 {
            b.remove(pad);
        }
        frame_start += FRAME;
    }
    fin.truncate(out_len);
    Ok(fin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncompressed_block() {
        // Header: no E8 (0), type 3, length 5; then pad to word, R0..R2, raw bytes, pad byte.
        let mut bits: Vec<u8> = Vec::new();
        // Bit string: 0 011 0000000000000000 00000101 -> 28 bits, padded to 32.
        let v: u32 = 0b0_011_0000000000000000_00000101 << 4;
        let hi = (v >> 16) as u16;
        let lo = v as u16;
        bits.extend_from_slice(&hi.to_le_bytes());
        bits.extend_from_slice(&lo.to_le_bytes());
        bits.extend_from_slice(&[1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]);
        bits.extend_from_slice(b"hello\0");
        assert_eq!(decompress(&bits, 16, 5).unwrap(), b"hello");
    }

    /// Reads a CAB's single LZX folder and compares with files extracted by Windows' expand.exe.
    fn check_cab(cab: &std::path::Path, extracted: &std::path::Path) {
        let d = std::fs::read(cab).unwrap();
        let u16le = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]) as usize;
        let u32le = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) as usize;
        let files_off = u32le(16);
        let nfiles = u16le(28);
        let flags = u16le(30);
        let mut folder = 36;
        let mut data_reserve = 0;
        if flags & 4 != 0 {
            data_reserve = d[folder + 3] as usize;
            folder += 4 + u16le(36);
        }
        let first_data = u32le(folder);
        let ndata = u16le(folder + 4);
        let ty = u16le(folder + 6);
        assert_eq!(ty & 0xF, 3, "LZX folder");
        let window = ((ty >> 8) & 0x1F) as u32;
        let mut stream = Vec::new();
        let mut total = 0;
        let mut p = first_data;
        for _ in 0..ndata {
            let cb = u16le(p + 4);
            let ucb = u16le(p + 6);
            let s = p + 8 + data_reserve;
            stream.extend_from_slice(&d[s..s + cb]);
            total += ucb;
            p = s + cb;
        }
        let out = decompress(&stream, window, total).unwrap();
        let mut p = files_off;
        let mut checked = 0;
        for _ in 0..nfiles {
            let size = u32le(p);
            let off = u32le(p + 4);
            let name_end = d[p + 16..].iter().position(|&c| c == 0).unwrap();
            let name = String::from_utf8_lossy(&d[p + 16..p + 16 + name_end]).into_owned();
            p += 16 + name_end + 1;
            if let Ok(want) = std::fs::read(extracted.join(&name)) {
                let got = &out[off..off + size];
                if let Some(i) = got.iter().zip(&want).position(|(a, b)| a != b) {
                    panic!("{name}: first mismatch at file byte {i} (folder byte {}), got {:?} want {:?}", off + i, &got[i..(i + 8).min(size)], &want[i..(i + 8).min(size)]);
                }
                assert_eq!(got.len(), want.len());
                checked += 1;
            }
        }
        assert!(checked > 0, "no reference files found");
    }

    #[test]
    fn matches_windows_expand_on_real_cabs() {
        let Ok(dir) = std::env::var("LZX_TEST_DIR") else { return };
        let dir = std::path::Path::new(&dir);
        check_cab(&dir.join("a.cab"), &dir.join("a"));
        check_cab(&dir.join("b.cab"), &dir.join("b"));
    }
}
