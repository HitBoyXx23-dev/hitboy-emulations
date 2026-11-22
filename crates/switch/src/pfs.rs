//! Switch package containers. All little-endian.
//!
//! - PFS0 (".nsp"): header, 0x18-byte file entries, a string table, then file data.
//! - HFS0: the same idea with 0x40-byte entries carrying SHA-256 hashes.
//! - XCI (game card image): a "HEAD" header at 0x100 pointing at a root HFS0 whose
//!   partitions ("update", "normal", "secure", "logo") are HFS0s themselves. Games are in "secure".

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// Absolute offset in the file.
    pub offset: u64,
    pub size: u64,
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn le64(b: &[u8], o: usize) -> u64 {
    le32(b, o) as u64 | (le32(b, o + 4) as u64) << 32
}

pub fn read_at(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; len];
    f.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    f.read_exact(&mut b).map_err(|_| "The package is truncated.".to_string())?;
    Ok(b)
}

/// Lists a PFS0 or HFS0 file system that starts at `base`.
pub fn list_fs(f: &mut File, base: u64) -> Result<Vec<Entry>, String> {
    let h = read_at(f, base, 0x10)?;
    let entry_size = entry_size(&h)?;
    let (count, strtab) = (le32(&h, 4) as usize, le32(&h, 8) as usize);
    check_counts(count, strtab)?;
    let table = read_at(f, base + 0x10, count * entry_size + strtab)?;
    Ok(entries(&table, count, strtab, entry_size, base + 0x10 + table.len() as u64))
}

/// Same, for a file system already in memory; offsets are relative to the start of `b`.
pub fn list_fs_bytes(b: &[u8]) -> Result<Vec<Entry>, String> {
    let h = b.get(..0x10).ok_or("The package is truncated.")?;
    let entry_size = entry_size(h)?;
    let (count, strtab) = (le32(h, 4) as usize, le32(h, 8) as usize);
    check_counts(count, strtab)?;
    let table = b.get(0x10..0x10 + count * entry_size + strtab).ok_or("The package is truncated.")?;
    Ok(entries(table, count, strtab, entry_size, 0x10 + table.len() as u64))
}

fn entry_size(h: &[u8]) -> Result<usize, String> {
    match &h[..4] {
        b"PFS0" => Ok(0x18),
        b"HFS0" => Ok(0x40),
        _ => Err("Not a Switch package (no PFS0/HFS0 header).".into()),
    }
}

fn check_counts(count: usize, strtab: usize) -> Result<(), String> {
    if count > 0x1_0000 || strtab > 0x100_0000 {
        return Err("The package header is damaged.".into());
    }
    Ok(())
}

fn entries(table: &[u8], count: usize, strtab: usize, entry_size: usize, data_start: u64) -> Vec<Entry> {
    let strings = &table[count * entry_size..count * entry_size + strtab];
    (0..count)
        .map(|i| {
            let e = &table[i * entry_size..];
            let name_off = le32(e, 16) as usize;
            let name = strings.get(name_off..).map(|s| String::from_utf8_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]).into_owned()).unwrap_or_default();
            Entry { name, offset: data_start + le64(e, 0), size: le64(e, 8) }
        })
        .collect()
}

/// The content files (NCAs etc.) of an .nsp or the "secure" partition of an .xci.
pub fn game_files(path: &Path) -> Result<Vec<Entry>, String> {
    let mut f = File::open(path).map_err(|e| format!("Can't open {}: {e}", path.display()))?;
    let magic = read_at(&mut f, 0, 4)?;
    if &magic == b"PFS0" {
        return list_fs(&mut f, 0);
    }
    let head = read_at(&mut f, 0x100, 0x40)?;
    if &head[..4] == b"HEAD" {
        let root = le64(&head, 0x30);
        let parts = list_fs(&mut f, root)?;
        let secure = parts.iter().find(|p| p.name == "secure").ok_or("This game card image has no secure partition.")?;
        return list_fs(&mut f, secure.offset);
    }
    Err("This isn't a Switch game package (.nsp or .xci).".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn pfs(files: &[(&str, &[u8])], hfs: bool) -> Vec<u8> {
        let es = if hfs { 0x40 } else { 0x18 };
        let mut strtab = Vec::new();
        let mut entries = Vec::new();
        let mut data = Vec::new();
        for (name, body) in files {
            let mut e = vec![0u8; es];
            e[0..8].copy_from_slice(&(data.len() as u64).to_le_bytes());
            e[8..16].copy_from_slice(&(body.len() as u64).to_le_bytes());
            e[16..20].copy_from_slice(&(strtab.len() as u32).to_le_bytes());
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
            entries.extend(e);
            data.extend_from_slice(body);
        }
        while strtab.len() % 0x20 != 0 {
            strtab.push(0);
        }
        let mut out = Vec::new();
        out.extend_from_slice(if hfs { b"HFS0" } else { b"PFS0" });
        out.extend_from_slice(&(files.len() as u32).to_le_bytes());
        out.extend_from_slice(&(strtab.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend(entries);
        out.extend(strtab);
        out.extend(data);
        out
    }

    fn temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        std::fs::File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }

    #[test]
    fn lists_nsp_files() {
        let p = temp("switch_test.nsp", &pfs(&[("a.nca", b"hello"), ("b.tik", b"xy")], false));
        let files = game_files(&p).unwrap();
        assert_eq!(files.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["a.nca", "b.tik"]);
        let bytes = std::fs::read(&p).unwrap();
        let e = &files[0];
        assert_eq!(&bytes[e.offset as usize..(e.offset + e.size) as usize], b"hello");
    }

    #[test]
    fn finds_xci_secure_partition() {
        let secure = pfs(&[("game.nca", b"NCA!")], true);
        let normal = pfs(&[], true);
        let root = pfs(&[("normal", &normal), ("secure", &secure)], true);
        let mut xci = vec![0u8; 0x200];
        xci[0x100..0x104].copy_from_slice(b"HEAD");
        xci[0x130..0x138].copy_from_slice(&0x200u64.to_le_bytes());
        xci.extend(root);
        let p = temp("switch_test.xci", &xci);
        let files = game_files(&p).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "game.nca");
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[files[0].offset as usize..files[0].offset as usize + 4], b"NCA!");
    }
}
