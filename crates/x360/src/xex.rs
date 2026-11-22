//! XEX2 executables: header, optional headers, security info, import libraries and the
//! loadable image. All fields are big-endian.
//!
//! Supported so far: unencrypted images stored raw or with "basic" (zero-run) compression.
//! Encrypted retail images and LZX ("normal") compression are reported as unsupported.

use std::collections::BTreeMap;

pub const MAGIC: &[u8; 4] = b"XEX2";

pub mod key {
    pub const RESOURCE_INFO: u32 = 0x0000_02FF;
    pub const FILE_FORMAT_INFO: u32 = 0x0000_03FF;
    pub const ORIGINAL_BASE_ADDRESS: u32 = 0x0001_0001;
    pub const ENTRY_POINT: u32 = 0x0001_0100;
    pub const IMAGE_BASE_ADDRESS: u32 = 0x0001_0201;
    pub const IMPORT_LIBRARIES: u32 = 0x0001_03FF;
    pub const ORIGINAL_PE_NAME: u32 = 0x0001_83FF;
    pub const TLS_INFO: u32 = 0x0002_0104;
    pub const DEFAULT_STACK_SIZE: u32 = 0x0002_0200;
    pub const SYSTEM_FLAGS: u32 = 0x0003_0000;
    pub const EXECUTION_INFO: u32 = 0x0004_0006;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encryption {
    None,
    Normal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    /// (data size, zero-fill size) blocks.
    Basic(Vec<(u32, u32)>),
    /// LZX with a window size, stored as a chain of hashed blocks of 16-bit-length chunks.
    Normal { window: u32, first_block: u32 },
    Delta,
}

#[derive(Clone, Debug)]
pub struct ImportLibrary {
    pub name: String,
    pub version: u32,
    /// Guest addresses of import records; each holds (type << 24 | ordinal) once loaded.
    pub records: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct ExecutionInfo {
    pub media_id: u32,
    pub version: u32,
    pub base_version: u32,
    pub title_id: u32,
    pub platform: u8,
    pub executable_type: u8,
    pub disc_number: u8,
    pub disc_count: u8,
}

#[derive(Clone, Debug)]
pub struct Xex {
    pub module_flags: u32,
    /// Raw optional headers: key -> inline value or offset (see `header_data`).
    pub headers: BTreeMap<u32, u32>,
    pub load_address: u32,
    pub image_size: u32,
    pub image_flags: u32,
    pub entry_point: u32,
    pub stack_size: u32,
    pub encryption: Encryption,
    pub compression: Compression,
    /// Session key, still encrypted with the console key.
    pub file_key: [u8; 16],
    pub imports: Vec<ImportLibrary>,
    pub execution: Option<ExecutionInfo>,
    pub pe_name: Option<String>,
    data_offset: u32,
}

fn be32(d: &[u8], o: usize) -> Result<u32, String> {
    d.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).ok_or_else(|| format!("XEX truncated at {o:#x}"))
}

fn be16(d: &[u8], o: usize) -> Result<u16, String> {
    d.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).ok_or_else(|| format!("XEX truncated at {o:#x}"))
}

impl Xex {
    pub fn parse(d: &[u8]) -> Result<Xex, String> {
        if d.len() < 24 || &d[0..4] != MAGIC {
            return Err("Not an Xbox 360 executable (XEX2 header missing).".into());
        }
        let module_flags = be32(d, 4)?;
        let data_offset = be32(d, 8)?;
        let security = be32(d, 16)? as usize;
        let count = be32(d, 20)? as usize;
        if count > 0x400 {
            return Err("XEX header lists too many optional headers.".into());
        }
        let mut headers = BTreeMap::new();
        for i in 0..count {
            headers.insert(be32(d, 24 + i * 8)?, be32(d, 28 + i * 8)?);
        }

        // Security info: header size, image size, then the image info block.
        let image_size = be32(d, security + 4)?;
        let info = security + 8;
        let image_flags = be32(d, info + 0x104)?;
        let load_address = be32(d, info + 0x108)?;
        let mut file_key = [0u8; 16];
        file_key.copy_from_slice(d.get(info + 0x150..info + 0x160).ok_or("XEX security info truncated")?);

        let mut x = Xex {
            module_flags,
            headers,
            load_address,
            image_size,
            image_flags,
            entry_point: 0,
            stack_size: 0x40000,
            encryption: Encryption::None,
            compression: Compression::None,
            file_key,
            imports: Vec::new(),
            execution: None,
            pe_name: None,
            data_offset,
        };
        if let Some(v) = x.headers.get(&key::IMAGE_BASE_ADDRESS) {
            x.load_address = *v;
        }
        x.entry_point = x.headers.get(&key::ENTRY_POINT).copied().unwrap_or(0);
        if let Some(v) = x.headers.get(&key::DEFAULT_STACK_SIZE) {
            x.stack_size = *v;
        }
        if let Some(f) = x.header_data(d, key::FILE_FORMAT_INFO)? {
            let size = be32(f, 0)? as usize;
            x.encryption = if be16(f, 4)? == 0 { Encryption::None } else { Encryption::Normal };
            x.compression = match be16(f, 6)? {
                0 => Compression::None,
                1 => {
                    let mut blocks = Vec::new();
                    let mut o = 8;
                    while o + 8 <= size.min(f.len()) {
                        blocks.push((be32(f, o)?, be32(f, o + 4)?));
                        o += 8;
                    }
                    Compression::Basic(blocks)
                }
                2 => Compression::Normal { window: be32(f, 8)?, first_block: be32(f, 12)? },
                _ => Compression::Delta,
            };
        }
        if let Some(e) = x.header_data(d, key::EXECUTION_INFO)? {
            x.execution = Some(ExecutionInfo {
                media_id: be32(e, 0)?,
                version: be32(e, 4)?,
                base_version: be32(e, 8)?,
                title_id: be32(e, 12)?,
                platform: e[16],
                executable_type: e[17],
                disc_number: e[18],
                disc_count: e[19],
            });
        }
        if let Some(n) = x.header_data(d, key::ORIGINAL_PE_NAME)? {
            let s = &n[4..];
            x.pe_name = Some(String::from_utf8_lossy(&s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())]).into_owned());
        }
        if let Some(im) = x.header_data(d, key::IMPORT_LIBRARIES)? {
            x.imports = parse_imports(im)?;
        }
        Ok(x)
    }

    /// Data for an optional header. Keys ending in 0xFF point at a block that starts with its
    /// own size; 0x00/0x01 are inline values (None here); others point at (low byte * 4) bytes.
    pub fn header_data<'a>(&self, d: &'a [u8], k: u32) -> Result<Option<&'a [u8]>, String> {
        let Some(&v) = self.headers.get(&k) else { return Ok(None) };
        let len = match k & 0xFF {
            0x00 | 0x01 => return Ok(None),
            0xFF => be32(d, v as usize)? as usize,
            n => n as usize * 4,
        };
        d.get(v as usize..v as usize + len).map(Some).ok_or_else(|| format!("XEX header {k:#010x} out of range"))
    }

    /// Bytes of header (optional headers + security info) before the image data.
    pub fn headers_len(&self) -> u32 {
        self.data_offset
    }

    pub fn title_id(&self) -> Option<u32> {
        self.execution.as_ref().map(|e| e.title_id)
    }

    /// The loaded PE image, `image_size` bytes, to be mapped at `load_address`.
    pub fn image(&self, d: &[u8]) -> Result<Vec<u8>, String> {
        if self.encryption != Encryption::None {
            return Err("This XEX is encrypted (retail). Decryption isn't supported yet.".into());
        }
        let src = d.get(self.data_offset as usize..).ok_or("XEX image data missing")?;
        let mut out = Vec::with_capacity(self.image_size as usize);
        match &self.compression {
            Compression::None => out.extend_from_slice(&src[..src.len().min(self.image_size as usize)]),
            Compression::Basic(blocks) => {
                let mut o = 0usize;
                for &(data, zero) in blocks {
                    let chunk = src.get(o..o + data as usize).ok_or("XEX compressed block out of range")?;
                    out.extend_from_slice(chunk);
                    out.resize(out.len() + zero as usize, 0);
                    o += data as usize;
                }
            }
            Compression::Normal { window, first_block } => {
                let stream = deblock(src, *first_block)?;
                let bits = window.trailing_zeros();
                if !window.is_power_of_two() {
                    return Err(format!("XEX LZX window {window:#x} isn't a power of two"));
                }
                return crate::lzx::decompress(&stream, bits, self.image_size as usize);
            }
            Compression::Delta => return Err("Delta-patch XEX files (title updates) can't be run on their own.".into()),
        }
        out.resize(self.image_size as usize, 0);
        Ok(out)
    }
}

/// Joins the chunks of an LZX-compressed image. Each block starts with the size and SHA-1 of
/// the *next* block, followed by chunks prefixed with a big-endian u16 length (0 ends the block).
fn deblock(src: &[u8], first_block: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(src.len());
    let mut p = 0usize;
    let mut size = first_block as usize;
    while size != 0 {
        let next = p + size;
        let next_size = be32(src, p)? as usize;
        let mut q = p + 24;
        loop {
            let n = be16(src, q)? as usize;
            q += 2;
            if n == 0 {
                break;
            }
            out.extend_from_slice(src.get(q..q + n).ok_or("XEX LZX chunk out of range")?);
            q += n;
        }
        p = next;
        size = next_size;
    }
    Ok(out)
}

fn parse_imports(d: &[u8]) -> Result<Vec<ImportLibrary>, String> {
    let size = be32(d, 0)? as usize;
    let strings_size = be32(d, 4)? as usize;
    let string_count = be32(d, 8)? as usize;
    let mut names = Vec::new();
    let mut o = 12;
    let end = 12 + strings_size;
    while names.len() < string_count && o < end {
        let s = &d[o..end.min(d.len())];
        let n = s.iter().position(|&b| b == 0).unwrap_or(s.len());
        names.push(String::from_utf8_lossy(&s[..n]).into_owned());
        o += (n + 1 + 3) & !3;
    }
    let mut libs = Vec::new();
    let mut o = end;
    while o + 0x28 <= size.min(d.len()) {
        let lib_size = be32(d, o)? as usize;
        if lib_size < 0x28 {
            break;
        }
        let version = be32(d, o + 0x1C)?;
        let name_index = be16(d, o + 0x24)? as usize;
        let count = be16(d, o + 0x26)? as usize;
        let records = (0..count).map(|i| be32(d, o + 0x28 + i * 4)).collect::<Result<_, _>>()?;
        libs.push(ImportLibrary { name: names.get(name_index).cloned().unwrap_or_default(), version, records });
        o += lib_size;
    }
    Ok(libs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(v: &mut Vec<u8>, o: usize, x: u32) {
        if v.len() < o + 4 {
            v.resize(o + 4, 0);
        }
        v[o..o + 4].copy_from_slice(&x.to_be_bytes());
    }

    /// A tiny XEX: basic compression, an entry point and one import library.
    fn sample() -> Vec<u8> {
        let mut v = b"XEX2".to_vec();
        let (sec, ffi, imp, data) = (0x100usize, 0x300usize, 0x340usize, 0x400usize);
        put(&mut v, 4, 1);
        put(&mut v, 8, data as u32);
        put(&mut v, 16, sec as u32);
        put(&mut v, 20, 4);
        let hdrs = [(key::FILE_FORMAT_INFO, ffi as u32), (key::ENTRY_POINT, 0x8200_1000), (key::IMAGE_BASE_ADDRESS, 0x8200_0000), (key::IMPORT_LIBRARIES, imp as u32)];
        for (i, (k, val)) in hdrs.iter().enumerate() {
            put(&mut v, 24 + i * 8, *k);
            put(&mut v, 28 + i * 8, *val);
        }
        put(&mut v, sec + 4, 0x20); // image size
        // File format: size 0x18, no encryption, basic compression, two blocks.
        put(&mut v, ffi, 0x18);
        put(&mut v, ffi + 4, 0x0000_0001);
        put(&mut v, ffi + 8, 4);
        put(&mut v, ffi + 12, 8);
        put(&mut v, ffi + 16, 4);
        put(&mut v, ffi + 20, 0);
        // Imports: one string "xboxkrnl.exe", one library with two records.
        let name = b"xboxkrnl.exe\0\0\0\0";
        put(&mut v, imp + 4, name.len() as u32);
        put(&mut v, imp + 8, 1);
        v.resize(imp + 12 + name.len(), 0);
        v[imp + 12..imp + 12 + name.len()].copy_from_slice(name);
        let lib = imp + 12 + name.len();
        put(&mut v, lib, 0x30);
        put(&mut v, lib + 0x1C, 0x2000_0000);
        put(&mut v, lib + 0x24, 2); // name index 0, count 2
        put(&mut v, lib + 0x28, 0x8200_2000);
        put(&mut v, lib + 0x2C, 0x8200_2004);
        put(&mut v, imp, (lib + 0x30 - imp) as u32);
        put(&mut v, data, 0x11223344);
        put(&mut v, data + 4, 0x55667788);
        v
    }

    #[test]
    fn parses_headers_and_imports() {
        let d = sample();
        let x = Xex::parse(&d).unwrap();
        assert_eq!(x.entry_point, 0x8200_1000);
        assert_eq!(x.load_address, 0x8200_0000);
        assert_eq!(x.compression, Compression::Basic(vec![(4, 8), (4, 0)]));
        assert_eq!(x.imports.len(), 1);
        assert_eq!(x.imports[0].name, "xboxkrnl.exe");
        assert_eq!(x.imports[0].records, vec![0x8200_2000, 0x8200_2004]);
    }

    #[test]
    fn expands_basic_compression() {
        let d = sample();
        let img = Xex::parse(&d).unwrap().image(&d).unwrap();
        assert_eq!(img.len(), 0x20);
        assert_eq!(&img[0..4], &[0x11, 0x22, 0x33, 0x44]);
        assert!(img[4..12].iter().all(|&b| b == 0));
        assert_eq!(&img[12..16], &[0x55, 0x66, 0x77, 0x88]);
    }

    #[test]
    fn deblocks_lzx_chunks() {
        // Block 1 (size 36): next-size 30 + hash, chunks "ab","c", end. Block 2 (size 30): next 0, chunk "d".
        let mut v = Vec::new();
        v.extend_from_slice(&30u32.to_be_bytes());
        v.extend_from_slice(&[0; 20]);
        v.extend_from_slice(&[0, 2, b'a', b'b', 0, 1, b'c', 0, 0]);
        v.resize(36, 0);
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(&[0; 20]);
        v.extend_from_slice(&[0, 1, b'd', 0, 0]);
        v.resize(66, 0);
        assert_eq!(deblock(&v, 36).unwrap(), b"abcd");
    }

    #[test]
    fn rejects_non_xex() {
        assert!(Xex::parse(b"MZ\0\0 not a xex at all....").is_err());
    }
}
