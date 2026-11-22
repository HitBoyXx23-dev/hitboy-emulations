//! Mach-O executables (thin or fat/universal): CPU type, load commands, encryption status,
//! linked libraries and the minimum iOS version.

#[derive(Debug, Clone, Default)]
pub struct Slice {
    pub arch: String,
    pub encrypted: bool,
    pub dylibs: Vec<String>,
    pub min_os: String,
    /// Entry point (file offset for LC_MAIN, or initial PC from LC_UNIXTHREAD).
    pub entry: u64,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub name: String,
    pub vmaddr: u64,
    pub vmsize: u64,
    pub fileoff: u64,
    pub filesize: u64,
}

#[derive(Debug, Clone, Default)]
pub struct MachO {
    pub slices: Vec<Slice>,
}

fn arch_name(cputype: u32, sub: u32) -> String {
    match (cputype, sub & 0xFF) {
        (12, 6) => "armv6".into(),
        (12, 9) => "armv7".into(),
        (12, 11) => "armv7s".into(),
        (12, _) => "arm".into(),
        (0x0100_000C, 2) => "arm64e".into(),
        (0x0100_000C, _) => "arm64".into(),
        (7, _) => "i386".into(),
        (0x0100_0007, _) => "x86_64".into(),
        _ => format!("cpu {cputype:#x}"),
    }
}

fn version(v: u32) -> String {
    format!("{}.{}", v >> 16, (v >> 8) & 0xFF)
}

fn parse_thin(d: &[u8]) -> Result<Slice, String> {
    let le32 = |o: usize| -> Result<u32, String> { d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or_else(|| "Mach-O is truncated.".to_string()) };
    let le64 = |o: usize| -> Result<u64, String> { Ok(le32(o)? as u64 | (le32(o + 4)? as u64) << 32) };
    let magic = le32(0)?;
    let is64 = match magic {
        0xFEED_FACE => false,
        0xFEED_FACF => true,
        _ => return Err("Not a Mach-O executable.".into()),
    };
    let (cputype, sub, ncmds) = (le32(4)?, le32(8)?, le32(16)?);
    let mut s = Slice { arch: arch_name(cputype, sub), ..Default::default() };
    let mut p = if is64 { 32 } else { 28 };
    for _ in 0..ncmds.min(4096) {
        let (cmd, size) = (le32(p)?, le32(p + 4)? as usize);
        if size < 8 {
            break;
        }
        match cmd {
            0x1 => {
                let name = String::from_utf8_lossy(&d[p + 8..p + 24]).trim_end_matches('\0').to_string();
                s.segments.push(Segment { name, vmaddr: le32(p + 24)? as u64, vmsize: le32(p + 28)? as u64, fileoff: le32(p + 32)? as u64, filesize: le32(p + 36)? as u64 });
            }
            0x19 => {
                let name = String::from_utf8_lossy(&d[p + 8..p + 24]).trim_end_matches('\0').to_string();
                s.segments.push(Segment { name, vmaddr: le64(p + 24)?, vmsize: le64(p + 32)?, fileoff: le64(p + 40)?, filesize: le64(p + 48)? });
            }
            0xC | 0x18 | 0x8000_0018 => {
                // LC_LOAD_DYLIB / LC_LOAD_WEAK_DYLIB
                let off = le32(p + 8)? as usize;
                let raw = d.get(p + off..p + size).unwrap_or(&[]);
                let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                s.dylibs.push(String::from_utf8_lossy(&raw[..end]).into_owned());
            }
            0x21 | 0x2C => s.encrypted |= le32(p + 16)? != 0, // LC_ENCRYPTION_INFO(_64): cryptid
            0x25 => s.min_os = version(le32(p + 8)?),      // LC_VERSION_MIN_IPHONEOS
            0x32 => {
                // LC_BUILD_VERSION: platform 2 = iOS
                if le32(p + 8)? == 2 {
                    s.min_os = version(le32(p + 12)?);
                }
            }
            0x8000_0028 => s.entry = le64(p + 8)?, // LC_MAIN
            0x5 => {
                // LC_UNIXTHREAD: ARM32 (r15 at +16 + 15*4) or ARM64 (pc at +16 + 32*8).
                if cputype == 12 {
                    s.entry = le32(p + 16 + 15 * 4)? as u64;
                } else if cputype == 0x0100_000C {
                    s.entry = le64(p + 16 + 32 * 8)?;
                }
            }
            _ => {}
        }
        p += size;
    }
    Ok(s)
}

pub fn parse(d: &[u8]) -> Result<MachO, String> {
    if d.len() < 8 {
        return Err("The app's executable is empty.".into());
    }
    let magic = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
    if magic == 0xCAFE_BABE {
        // Fat (universal): big-endian header with one entry per architecture.
        let n = u32::from_be_bytes([d[4], d[5], d[6], d[7]]) as usize;
        let mut out = MachO::default();
        for i in 0..n.min(16) {
            let e = 8 + i * 20;
            let rd = |o: usize| d.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or("Truncated fat header");
            let (off, size) = (rd(e + 8)?, rd(e + 12)?);
            let slice = d.get(off..off + size).ok_or("Fat slice outside the file")?;
            out.slices.push(parse_thin(slice)?);
        }
        return Ok(out);
    }
    Ok(MachO { slices: vec![parse_thin(d)?] })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal armv7 executable: header + LC_LOAD_DYLIB (UIKit) + LC_ENCRYPTION_INFO +
    /// LC_VERSION_MIN_IPHONEOS 3.1.
    pub fn thin_armv7(encrypted: bool) -> Vec<u8> {
        let mut cmds = Vec::new();
        let lib = b"/System/Library/Frameworks/UIKit.framework/UIKit\0\0\0\0";
        cmds.extend(0xCu32.to_le_bytes());
        cmds.extend(((24 + lib.len()) as u32).to_le_bytes());
        cmds.extend(24u32.to_le_bytes());
        cmds.extend([0u8; 12]);
        cmds.extend(lib);
        cmds.extend(0x21u32.to_le_bytes());
        cmds.extend(20u32.to_le_bytes());
        cmds.extend(0u32.to_le_bytes());
        cmds.extend(0u32.to_le_bytes());
        cmds.extend((encrypted as u32).to_le_bytes());
        cmds.extend(0x25u32.to_le_bytes());
        cmds.extend(16u32.to_le_bytes());
        cmds.extend((3u32 << 16 | 1 << 8).to_le_bytes());
        cmds.extend(0u32.to_le_bytes());
        let mut d = Vec::new();
        d.extend(0xFEED_FACEu32.to_le_bytes());
        d.extend(12u32.to_le_bytes());
        d.extend(9u32.to_le_bytes());
        d.extend(2u32.to_le_bytes());
        d.extend(3u32.to_le_bytes());
        d.extend((cmds.len() as u32).to_le_bytes());
        d.extend(0u32.to_le_bytes());
        d.extend(cmds);
        d
    }

    #[test]
    fn thin_and_fat() {
        let m = parse(&thin_armv7(true)).unwrap();
        assert_eq!(m.slices[0].arch, "armv7");
        assert!(m.slices[0].encrypted);
        assert_eq!(m.slices[0].min_os, "3.1");
        // Wrap it in a fat header.
        let thin = thin_armv7(false);
        let mut fat = Vec::new();
        fat.extend(0xCAFE_BABEu32.to_be_bytes());
        fat.extend(1u32.to_be_bytes());
        fat.extend(12u32.to_be_bytes());
        fat.extend(9u32.to_be_bytes());
        fat.extend(28u32.to_be_bytes());
        fat.extend((thin.len() as u32).to_be_bytes());
        fat.extend(0u32.to_be_bytes());
        fat.extend(&thin);
        let m = parse(&fat).unwrap();
        assert_eq!(m.slices.len(), 1);
        assert!(!m.slices[0].encrypted);
    }
}
