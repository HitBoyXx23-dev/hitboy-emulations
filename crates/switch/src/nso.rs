//! NSO executables (the Switch's program format): a 0x100-byte header and text / rodata / data
//! segments, each optionally LZ4-compressed, followed by a zero-filled bss.

pub const SEGMENT_NAMES: [&str; 3] = ["text", "rodata", "data"];

pub struct Segment {
    pub mem_off: u32,
    pub data: Vec<u8>,
}

pub struct Nso {
    pub segments: [Segment; 3],
    pub bss_size: u32,
    pub build_id: [u8; 0x20],
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// LZ4 block decompression into exactly `out_len` bytes.
pub fn lz4_block(src: &[u8], out_len: usize) -> Result<Vec<u8>, String> {
    let bad = || "A compressed segment is damaged.".to_string();
    let mut out: Vec<u8> = Vec::with_capacity(out_len);
    let mut i = 0;
    while i < src.len() {
        let token = src[i];
        i += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *src.get(i).ok_or_else(bad)?;
                i += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        out.extend_from_slice(src.get(i..i + lit).ok_or_else(bad)?);
        i += lit;
        if i >= src.len() {
            break;
        }
        let off = u16::from_le_bytes([*src.get(i).ok_or_else(bad)?, *src.get(i + 1).ok_or_else(bad)?]) as usize;
        i += 2;
        let mut len = (token & 15) as usize;
        if len == 15 {
            loop {
                let b = *src.get(i).ok_or_else(bad)?;
                i += 1;
                len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        len += 4;
        if off == 0 || off > out.len() || out.len() + len > out_len {
            return Err(bad());
        }
        for _ in 0..len {
            out.push(out[out.len() - off]);
        }
    }
    if out.len() != out_len {
        return Err(bad());
    }
    Ok(out)
}

impl Nso {
    pub fn parse(b: &[u8]) -> Result<Nso, String> {
        if b.len() < 0x100 || &b[..4] != b"NSO0" {
            return Err("Not an NSO executable.".into());
        }
        let flags = le32(b, 0xC);
        let mut segs = Vec::new();
        for k in 0..3 {
            let h = 0x10 + k * 0x10;
            let (file_off, mem_off, size) = (le32(b, h) as usize, le32(b, h + 4), le32(b, h + 8) as usize);
            let comp = le32(b, 0x60 + k * 4) as usize;
            if size > 0x1000_0000 {
                return Err("An NSO segment is implausibly large.".into());
            }
            let data = if flags >> k & 1 != 0 {
                lz4_block(b.get(file_off..file_off + comp).ok_or("The NSO is truncated.")?, size)?
            } else {
                b.get(file_off..file_off + size).ok_or("The NSO is truncated.")?.to_vec()
            };
            segs.push(Segment { mem_off, data });
        }
        let mut build_id = [0u8; 0x20];
        build_id.copy_from_slice(&b[0x40..0x60]);
        let mut it = segs.into_iter();
        Ok(Nso { segments: [it.next().unwrap(), it.next().unwrap(), it.next().unwrap()], bss_size: le32(b, 0x3C), build_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lz4_literals_and_matches() {
        // "abcabcabc": 3 literals then a match of length 6 at offset 3.
        let c = [0x32, b'a', b'b', b'c', 3, 0];
        assert_eq!(lz4_block(&c, 9).unwrap(), b"abcabcabc");
        assert!(lz4_block(&c, 10).is_err());
    }

    #[test]
    fn parses_plain_and_compressed_segments() {
        let mut f = vec![0u8; 0x100];
        f[..4].copy_from_slice(b"NSO0");
        f[0xC..0x10].copy_from_slice(&2u32.to_le_bytes()); // rodata compressed
        let text = [1u8, 2, 3, 4];
        let ro_c = [0x32, b'a', b'b', b'c', 3, 0];
        let put = |f: &mut Vec<u8>, k: usize, off: usize, mem: u32, size: u32, comp: u32| {
            f[0x10 + k * 0x10..][..4].copy_from_slice(&(off as u32).to_le_bytes());
            f[0x14 + k * 0x10..][..4].copy_from_slice(&mem.to_le_bytes());
            f[0x18 + k * 0x10..][..4].copy_from_slice(&size.to_le_bytes());
            f[0x60 + k * 4..][..4].copy_from_slice(&comp.to_le_bytes());
        };
        put(&mut f, 0, 0x100, 0, 4, 4);
        put(&mut f, 1, 0x104, 0x1000, 9, ro_c.len() as u32);
        put(&mut f, 2, 0x10A, 0x2000, 2, 2);
        f[0x3C..0x40].copy_from_slice(&0x500u32.to_le_bytes());
        f.extend_from_slice(&text);
        f.extend_from_slice(&ro_c);
        f.extend_from_slice(&[9, 9]);
        let n = Nso::parse(&f).unwrap();
        assert_eq!(n.segments[0].data, text);
        assert_eq!(n.segments[1].data, b"abcabcabc");
        assert_eq!((n.segments[2].mem_off, n.bss_size), (0x2000, 0x500));
        assert!(Nso::parse(&f[..0x102]).is_err());
    }
}
