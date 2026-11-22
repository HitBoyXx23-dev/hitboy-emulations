//! XBE, the Xbox executable format: header, certificate (title name and ID), sections,
//! entry point and kernel import thunk (both XOR-scrambled with retail or debug keys).

pub const ENTRY_RETAIL: u32 = 0xA8FC_57AB;
pub const ENTRY_DEBUG: u32 = 0x9485_9D4B;
pub const THUNK_RETAIL: u32 = 0x5B6D_40B6;
pub const THUNK_DEBUG: u32 = 0xEFB1_F152;

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub vaddr: u32,
    pub vsize: u32,
    pub raw_off: u32,
    pub raw_size: u32,
}

#[derive(Debug, Clone)]
pub struct Xbe {
    pub base: u32,
    pub entry: u32,
    pub kernel_thunk: u32,
    pub debug: bool,
    pub title_id: u32,
    pub title: String,
    pub sections: Vec<Section>,
}

fn u32_at(d: &[u8], o: usize) -> Result<u32, String> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or_else(|| "The XBE is truncated.".into())
}

impl Xbe {
    /// Title ID as the usual "XX-123" form (publisher letters + game number).
    pub fn title_code(&self) -> String {
        let (a, b) = ((self.title_id >> 24) as u8, (self.title_id >> 16) as u8);
        let pubc = if a.is_ascii_alphanumeric() && b.is_ascii_alphanumeric() { format!("{}{}", a as char, b as char) } else { format!("{:04X}", self.title_id >> 16) };
        format!("{pubc}-{:03}", self.title_id & 0xFFFF)
    }

    /// Copies every section into memory at its virtual address (plus the headers at base).
    pub fn load(&self, file: &[u8], mut write: impl FnMut(u32, &[u8])) -> Result<(), String> {
        let hdr = u32_at(file, 0x108)? as usize;
        write(self.base, file.get(..hdr).ok_or("The XBE is truncated.")?);
        for s in &self.sections {
            let raw = file.get(s.raw_off as usize..(s.raw_off + s.raw_size) as usize).ok_or_else(|| format!("Section {} lies outside the file.", s.name))?;
            let n = raw.len().min(s.vsize as usize);
            write(s.vaddr, &raw[..n]);
        }
        Ok(())
    }
}

pub fn parse(d: &[u8]) -> Result<Xbe, String> {
    if d.get(0..4) != Some(b"XBEH") {
        return Err("Not an Xbox executable (no XBEH header).".into());
    }
    let base = u32_at(d, 0x104)?;
    let off = |va: u32| va.checked_sub(base).map(|o| o as usize).ok_or("XBE address below its base.");
    let raw_entry = u32_at(d, 0x128)?;
    // A debug XBE decodes to an address inside the image with the debug key.
    let img_size = u32_at(d, 0x10C)?;
    let dbg_entry = raw_entry ^ ENTRY_DEBUG;
    let debug = dbg_entry >= base && dbg_entry < base.wrapping_add(img_size);
    let entry = raw_entry ^ if debug { ENTRY_DEBUG } else { ENTRY_RETAIL };
    let kernel_thunk = u32_at(d, 0x158)? ^ if debug { THUNK_DEBUG } else { THUNK_RETAIL };
    let cert = off(u32_at(d, 0x118)?)?;
    let title_id = u32_at(d, cert + 8)?;
    let name: Vec<u16> = (0..40).map(|i| u16::from_le_bytes([d.get(cert + 12 + i * 2).copied().unwrap_or(0), d.get(cert + 13 + i * 2).copied().unwrap_or(0)])).take_while(|&c| c != 0).collect();
    let n = u32_at(d, 0x11C)? as usize;
    let sh = off(u32_at(d, 0x120)?)?;
    let mut sections = Vec::new();
    for i in 0..n.min(256) {
        let s = sh + i * 56;
        let name_va = u32_at(d, s + 20)?;
        let name = off(name_va)
            .ok()
            .and_then(|o| d.get(o..))
            .map(|r| String::from_utf8_lossy(&r[..r.iter().position(|&c| c == 0).unwrap_or(r.len()).min(32)]).into_owned())
            .unwrap_or_default();
        sections.push(Section { name, vaddr: u32_at(d, s + 4)?, vsize: u32_at(d, s + 8)?, raw_off: u32_at(d, s + 12)?, raw_size: u32_at(d, s + 16)? });
    }
    Ok(Xbe { base, entry, kernel_thunk, debug, title_id, title: String::from_utf16_lossy(&name), sections })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tiny retail XBE: base 0x10000, one ".text" section holding `code`.
    pub fn build(title: &str, code: &[u8]) -> Vec<u8> {
        let base = 0x10000u32;
        let mut d = vec![0u8; 0x1000];
        d[0..4].copy_from_slice(b"XBEH");
        let w = |d: &mut Vec<u8>, o: usize, v: u32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        w(&mut d, 0x104, base);
        w(&mut d, 0x108, 0x400);
        w(&mut d, 0x10C, 0x2000);
        w(&mut d, 0x118, base + 0x200); // certificate
        w(&mut d, 0x11C, 1);
        w(&mut d, 0x120, base + 0x300); // section headers
        w(&mut d, 0x128, (base + 0x1000) ^ ENTRY_RETAIL);
        w(&mut d, 0x158, (base + 0x1800) ^ THUNK_RETAIL);
        w(&mut d, 0x208, 0x4D53_0004); // "MS-004"
        for (i, c) in title.encode_utf16().enumerate() {
            d[0x20C + i * 2..0x20E + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        w(&mut d, 0x304, base + 0x1000);
        w(&mut d, 0x308, 0x1000);
        w(&mut d, 0x30C, 0x800);
        w(&mut d, 0x310, code.len() as u32);
        w(&mut d, 0x314, base + 0x380);
        d[0x380..0x385].copy_from_slice(b".text");
        d.resize(0x800, 0);
        d.extend(code);
        d
    }

    #[test]
    fn header_and_sections() {
        let f = build("Test Game", &[0xF4]);
        let x = parse(&f).unwrap();
        assert!(!x.debug);
        assert_eq!(x.entry, 0x11000);
        assert_eq!(x.kernel_thunk, 0x11800);
        assert_eq!(x.title, "Test Game");
        assert_eq!(x.title_code(), "MS-004");
        assert_eq!(x.sections[0].name, ".text");
        let mut mem = vec![0u8; 0x20000];
        x.load(&f, |a, b| mem[a as usize..a as usize + b.len()].copy_from_slice(b)).unwrap();
        assert_eq!(mem[0x11000], 0xF4);
        assert_eq!(&mem[0x10000..0x10004], b"XBEH");
    }
}
