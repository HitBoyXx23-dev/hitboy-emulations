//! Nintendo Switch RomFS parser (Horizon 64-bit RomFS format).
//!
//! Layout:
//! - 0x50-byte header:
//!   - 0x00: header_size (u64, must be 0x50)
//!   - 0x08: dir_hash_table_offset (u64)
//!   - 0x10: dir_hash_table_size (u64)
//!   - 0x18: dir_meta_table_offset (u64)
//!   - 0x20: dir_meta_table_size (u64)
//!   - 0x28: file_hash_table_offset (u64)
//!   - 0x30: file_hash_table_size (u64)
//!   - 0x38: file_meta_table_offset (u64)
//!   - 0x40: file_meta_table_size (u64)
//!   - 0x48: file_data_offset (u64)

use std::collections::BTreeMap;

const NO_ENTRY: u32 = 0xFFFF_FFFF;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct RomFs {
    pub file_data_offset: u64,
    files: BTreeMap<String, FileEntry>,
}

fn le32(b: &[u8], o: usize) -> Result<u32, String> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| format!("RomFS truncated at {o:#x}"))
}

fn le64(b: &[u8], o: usize) -> Result<u64, String> {
    b.get(o..o + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| format!("RomFS truncated at {o:#x}"))
}

impl RomFs {
    /// Parses a Switch RomFS image and indexes all files by their normalized `/path/to/file` path.
    pub fn parse(data: &[u8]) -> Result<RomFs, String> {
        if data.len() < 0x50 {
            return Err("RomFS image is too small for a header.".into());
        }
        let header_size = le64(data, 0x00)?;
        if header_size != 0x50 {
            return Err(format!("Unexpected RomFS header size {header_size:#x} (expected 0x50)."));
        }
        let dir_meta_off = le64(data, 0x18)? as usize;
        let dir_meta_size = le64(data, 0x20)? as usize;
        let file_meta_off = le64(data, 0x38)? as usize;
        let file_meta_size = le64(data, 0x40)? as usize;
        let file_data_offset = le64(data, 0x48)?;

        let dir_meta = data
            .get(dir_meta_off..dir_meta_off.saturating_add(dir_meta_size))
            .ok_or("RomFS directory metadata table runs past the end of the image.")?;
        let file_meta = data
            .get(file_meta_off..file_meta_off.saturating_add(file_meta_size))
            .ok_or("RomFS file metadata table runs past the end of the image.")?;

        let mut files = BTreeMap::new();
        if !dir_meta.is_empty() {
            walk_dir(dir_meta, file_meta, 0, "", &mut files, 0)?;
        }
        Ok(RomFs { file_data_offset, files })
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Lists all indexed files in sorted path order.
    pub fn list_files(&self) -> Vec<&FileEntry> {
        self.files.values().collect()
    }

    /// Looks up a file by path (leading `/` is optional).
    pub fn get(&self, path: &str) -> Option<&FileEntry> {
        let norm = normalize_path(path);
        self.files.get(&norm)
    }

    /// Reads a file's bytes out of the same RomFS buffer that was passed to `parse`.
    pub fn read_file<'a>(&self, data: &'a [u8], path: &str) -> Option<&'a [u8]> {
        let e = self.get(path)?;
        let start = self.file_data_offset.checked_add(e.offset)? as usize;
        let end = start.checked_add(e.size as usize)?;
        data.get(start..end)
    }
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    format!("/{trimmed}")
}

fn walk_dir(
    dir_meta: &[u8],
    file_meta: &[u8],
    dir_off: u32,
    prefix: &str,
    out: &mut BTreeMap<String, FileEntry>,
    depth: usize,
) -> Result<(), String> {
    if depth > 64 {
        return Err("RomFS directory nesting exceeded safety limit.".into());
    }
    let mut cur = dir_off;
    let mut guard = 0usize;
    while cur != NO_ENTRY {
        guard += 1;
        if guard > 65_536 {
            return Err("RomFS directory sibling chain exceeded safety limit.".into());
        }
        let o = cur as usize;
        let sibling = le32(dir_meta, o + 4)?;
        let child_dir = le32(dir_meta, o + 8)?;
        let child_file = le32(dir_meta, o + 12)?;
        let name_len = le32(dir_meta, o + 20)? as usize;
        let name_bytes = dir_meta
            .get(o + 24..o + 24 + name_len)
            .ok_or("RomFS directory name runs past metadata table.")?;
        let name = String::from_utf8_lossy(name_bytes);
        let dir_path = if prefix.is_empty() && name.is_empty() {
            String::new()
        } else if prefix.is_empty() {
            format!("/{name}")
        } else {
            format!("{prefix}/{name}")
        };

        // Walk files in this directory
        let mut fcur = child_file;
        let mut fguard = 0usize;
        while fcur != NO_ENTRY {
            fguard += 1;
            if fguard > 262_144 {
                return Err("RomFS file sibling chain exceeded safety limit.".into());
            }
            let fo = fcur as usize;
            let fsibling = le32(file_meta, fo + 4)?;
            let offset = le64(file_meta, fo + 8)?;
            let size = le64(file_meta, fo + 16)?;
            let fname_len = le32(file_meta, fo + 28)? as usize;
            let fname_bytes = file_meta
                .get(fo + 32..fo + 32 + fname_len)
                .ok_or("RomFS file name runs past metadata table.")?;
            let fname = String::from_utf8_lossy(fname_bytes);
            let full = if dir_path.is_empty() {
                format!("/{fname}")
            } else {
                format!("{dir_path}/{fname}")
            };
            out.insert(full.clone(), FileEntry { path: full, offset, size });
            fcur = fsibling;
        }

        if child_dir != NO_ENTRY {
            walk_dir(dir_meta, file_meta, child_dir, &dir_path, out, depth + 1)?;
        }
        cur = sibling;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal valid RomFS image with root dir -> `assets` subdir -> `hello.txt`.
    fn build_sample_romfs() -> Vec<u8> {
        let mut dir_meta = Vec::new();
        // Root dir at offset 0 (24 bytes): parent=0, sibling=NO_ENTRY, child_dir=24, child_file=NO_ENTRY, hash=NO_ENTRY, name_len=0
        for w in [0u32, NO_ENTRY, 24, NO_ENTRY, NO_ENTRY, 0] {
            dir_meta.extend_from_slice(&w.to_le_bytes());
        }
        // Child dir "assets" at offset 24: parent=0, sibling=NO_ENTRY, child_dir=NO_ENTRY, child_file=0, hash=NO_ENTRY, name_len=6
        for w in [0u32, NO_ENTRY, NO_ENTRY, 0, NO_ENTRY, 6] {
            dir_meta.extend_from_slice(&w.to_le_bytes());
        }
        dir_meta.extend_from_slice(b"assets\0\0");

        let payload = b"Switch RomFS OK!";
        let mut file_meta = Vec::new();
        // File "hello.txt" at offset 0: parent=24, sibling=NO_ENTRY, off=0, size=payload.len(), hash=NO_ENTRY, name_len=9
        file_meta.extend_from_slice(&24u32.to_le_bytes());
        file_meta.extend_from_slice(&NO_ENTRY.to_le_bytes());
        file_meta.extend_from_slice(&0u64.to_le_bytes());
        file_meta.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        file_meta.extend_from_slice(&NO_ENTRY.to_le_bytes());
        file_meta.extend_from_slice(&9u32.to_le_bytes());
        file_meta.extend_from_slice(b"hello.txt\0\0\0");

        let dir_meta_off = 0x50u64;
        let file_meta_off = dir_meta_off + dir_meta.len() as u64;
        let data_off = (file_meta_off + file_meta.len() as u64 + 15) & !15;

        let mut img = vec![0u8; data_off as usize];
        img[0x00..0x08].copy_from_slice(&0x50u64.to_le_bytes());
        img[0x18..0x20].copy_from_slice(&dir_meta_off.to_le_bytes());
        img[0x20..0x28].copy_from_slice(&(dir_meta.len() as u64).to_le_bytes());
        img[0x38..0x40].copy_from_slice(&file_meta_off.to_le_bytes());
        img[0x40..0x48].copy_from_slice(&(file_meta.len() as u64).to_le_bytes());
        img[0x48..0x50].copy_from_slice(&data_off.to_le_bytes());

        img[dir_meta_off as usize..dir_meta_off as usize + dir_meta.len()].copy_from_slice(&dir_meta);
        img[file_meta_off as usize..file_meta_off as usize + file_meta.len()].copy_from_slice(&file_meta);
        img.extend_from_slice(payload);
        img
    }

    #[test]
    fn parses_directories_and_reads_files() {
        let img = build_sample_romfs();
        let fs = RomFs::parse(&img).expect("valid romfs");
        assert_eq!(fs.len(), 1);
        let bytes = fs.read_file(&img, "/assets/hello.txt").expect("file found");
        assert_eq!(bytes, b"Switch RomFS OK!");
        assert_eq!(fs.read_file(&img, "assets/hello.txt"), Some(&b"Switch RomFS OK!"[..]));
        assert!(fs.read_file(&img, "/assets/missing.txt").is_none());
    }

    #[test]
    fn rejects_invalid_header() {
        assert!(RomFs::parse(&[0u8; 16]).is_err());
        assert!(RomFs::parse(&[0u8; 0x50]).is_err());
    }
}
