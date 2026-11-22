//! XDVDFS, the Xbox disc filesystem. Works on trimmed XISOs and on full Redump images,
//! where the game partition starts further into the disc.

const SECTOR: u64 = 2048;
const MAGIC: &[u8] = b"MICROSOFT*XBOX*MEDIA";
/// Where the game partition can begin: XISO, Redump (most games), early discs, XGD3.
const PARTITIONS: [u64; 4] = [0, 0x1830_0000, 0x0FD9_0000, 0x0208_0000];

/// Random-access source of disc bytes: an in-memory image or a file on disk.
pub trait Image {
    fn read_at(&mut self, off: u64, len: usize) -> Option<Vec<u8>>;
}

impl Image for [u8] {
    fn read_at(&mut self, off: u64, len: usize) -> Option<Vec<u8>> {
        let o = usize::try_from(off).ok()?;
        self.get(o..o.checked_add(len)?).map(<[u8]>::to_vec)
    }
}
impl Image for Vec<u8> {
    fn read_at(&mut self, off: u64, len: usize) -> Option<Vec<u8>> {
        self.as_mut_slice().read_at(off, len)
    }
}
impl Image for std::fs::File {
    fn read_at(&mut self, off: u64, len: usize) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        self.seek(SeekFrom::Start(off)).ok()?;
        let mut v = vec![0; len];
        self.read_exact(&mut v).ok()?;
        Some(v)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub sector: u32,
    pub size: u32,
    pub dir: bool,
}

#[derive(Debug, Clone)]
pub struct Disc {
    /// Byte offset of the game partition.
    pub base: u64,
    pub root_sector: u32,
    pub root_size: u32,
}

impl Disc {
    pub fn open<I: Image + ?Sized>(img: &mut I) -> Result<Disc, String> {
        for base in PARTITIONS {
            let Some(vd) = img.read_at(base + 32 * SECTOR, 0x20) else { continue };
            if vd.starts_with(MAGIC) {
                let rd = |o: usize| u32::from_le_bytes([vd[o], vd[o + 1], vd[o + 2], vd[o + 3]]);
                return Ok(Disc { base, root_sector: rd(0x14), root_size: rd(0x18) });
            }
        }
        Err("This isn't an Xbox disc image (no XDVDFS volume found).".into())
    }

    fn at(&self, sector: u32) -> u64 {
        self.base + sector as u64 * SECTOR
    }

    /// Lists a directory table (a binary tree stored in sector-aligned entries).
    pub fn list<I: Image + ?Sized>(&self, img: &mut I, sector: u32, size: u32) -> Vec<Entry> {
        let mut out = Vec::new();
        let Some(table) = img.read_at(self.at(sector), (size as usize).min(16 << 20)) else { return out };
        let mut stack = vec![0usize];
        let mut seen = std::collections::HashSet::new();
        while let Some(off) = stack.pop() {
            if off + 14 > table.len() || !seen.insert(off) {
                continue;
            }
            let e = &table[off..];
            let (left, right) = (u16::from_le_bytes([e[0], e[1]]), u16::from_le_bytes([e[2], e[3]]));
            if left == 0xFFFF && right == 0xFFFF {
                continue; // padding
            }
            let nlen = e[13] as usize;
            let Some(nm) = e.get(14..14 + nlen) else { continue };
            out.push(Entry {
                name: String::from_utf8_lossy(nm).into_owned(),
                sector: u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
                size: u32::from_le_bytes([e[8], e[9], e[10], e[11]]),
                dir: e[12] & 0x10 != 0,
            });
            if left != 0 {
                stack.push(left as usize * 4);
            }
            if right != 0 {
                stack.push(right as usize * 4);
            }
        }
        out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
        out
    }

    pub fn root<I: Image + ?Sized>(&self, img: &mut I) -> Vec<Entry> {
        self.list(img, self.root_sector, self.root_size)
    }

    /// Finds a file by path ("media/intro.xmv"), case-insensitively.
    pub fn find<I: Image + ?Sized>(&self, img: &mut I, path: &str) -> Option<Entry> {
        let (mut sector, mut size) = (self.root_sector, self.root_size);
        let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
        for (i, p) in parts.iter().enumerate() {
            let e = self.list(img, sector, size).into_iter().find(|e| e.name.eq_ignore_ascii_case(p))?;
            if i + 1 == parts.len() {
                return Some(e);
            }
            if !e.dir {
                return None;
            }
            (sector, size) = (e.sector, e.size);
        }
        None
    }

    pub fn read_file<I: Image + ?Sized>(&self, img: &mut I, path: &str) -> Option<Vec<u8>> {
        let e = self.find(img, path)?;
        img.read_at(self.at(e.sector), e.size as usize)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a small XISO: root holds default.xbe and a "media" dir with one file.
    pub fn build(xbe: &[u8]) -> Vec<u8> {
        let mut img = vec![0u8; 64 * 2048];
        let vd = 32 * 2048;
        img[vd..vd + 20].copy_from_slice(MAGIC);
        img[vd + 0x14..vd + 0x18].copy_from_slice(&33u32.to_le_bytes());
        img[vd + 0x18..vd + 0x1C].copy_from_slice(&2048u32.to_le_bytes());
        let entry = |img: &mut Vec<u8>, at: usize, right: u16, sector: u32, size: u32, attr: u8, name: &str| {
            img[at + 2..at + 4].copy_from_slice(&right.to_le_bytes());
            img[at + 4..at + 8].copy_from_slice(&sector.to_le_bytes());
            img[at + 8..at + 12].copy_from_slice(&size.to_le_bytes());
            img[at + 12] = attr;
            img[at + 13] = name.len() as u8;
            img[at + 14..at + 14 + name.len()].copy_from_slice(name.as_bytes());
        };
        // Root (sector 33): default.xbe -> right child "media" at dword 8.
        entry(&mut img, 33 * 2048, 8, 36, xbe.len() as u32, 0x20, "default.xbe");
        entry(&mut img, 33 * 2048 + 32, 0, 34, 2048, 0x10, "media");
        // media dir (sector 34): hello.txt at sector 35.
        entry(&mut img, 34 * 2048, 0, 35, 5, 0x20, "hello.txt");
        img[35 * 2048..35 * 2048 + 5].copy_from_slice(b"hello");
        img[36 * 2048..36 * 2048 + xbe.len()].copy_from_slice(xbe);
        img
    }

    #[test]
    fn walks_directories() {
        let mut img = build(b"XBEH");
        let d = Disc::open(&mut img).unwrap();
        let names: Vec<String> = d.root(&mut img).into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["default.xbe", "media"]);
        assert_eq!(d.read_file(&mut img, "MEDIA/hello.txt").unwrap(), b"hello");
        assert!(d.find(&mut img, "missing").is_none());
    }

    #[test]
    fn finds_redump_partition() {
        let small = build(b"XBEH");
        let mut img = vec![0u8; 0x0208_0000];
        img.extend(&small);
        let d = Disc::open(&mut img).unwrap();
        assert_eq!(d.base, 0x0208_0000);
        assert_eq!(d.read_file(&mut img, "media/hello.txt").unwrap(), b"hello");
    }
}
