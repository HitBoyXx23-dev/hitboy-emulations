//! Portable Executable (PE32 / PE32+) headers, sections, imports, relocations and the other
//! directories needed to map an Xbox One / Series X|S image into a Windows process.

/// Data-directory indices we care about (IMAGE_DIRECTORY_ENTRY_*).
const DD_IMPORT: usize = 1;
const DD_BASERELOC: usize = 5;
const DD_TLS: usize = 9;
const DD_IAT: usize = 12;
const DD_DELAY_IMPORT: usize = 13;

/// IMAGE_FILE_MACHINE_AMD64.
pub const MACHINE_AMD64: u16 = 0x8664;

/// A section's raw permissions, folded into one flag set.
// Windows memory-protection constants (the PAGE_* family). These are not the MEM_* allocation
// constants of the same numbers' namesakes: protection describes what a mapped page may be used
// for, and is what `VirtualProtect` takes.
pub const PAGE_READONLY: u32 = 0x02;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_EXECUTE_READ: u32 = 0x20;

/// IMAGE_SUBSYSTEM_NATIVE - what Xbox One and Series X|S executables are marked as. They are
/// not windowed apps; the game is started by its own entry point, not by WinMain.
pub const SUBSYSTEM_NATIVE: u16 = 1;

#[derive(Debug, Clone, Default)]
pub struct Section {
    pub name: String,
    pub vaddr: u32,
    pub vsize: u32,
    pub raw_off: u32,
    pub raw_size: u32,
    pub flags: u32,
}

impl Section {
    /// Windows page protection for this section, so the loader can map it the way it would a normal
    /// PE (`.text` readable and executable, `.rdata` readable, `.data` writable).
    pub fn protect(&self) -> u32 {
        // IMAGE_SCN_MEM_EXECUTE is 0x2000_0000 and IMAGE_SCN_MEM_WRITE 0x8000_0000.
        const EXECUTE: u32 = 0x2000_0000;
        const WRITE: u32 = 0x8000_0000;
        if self.flags & EXECUTE != 0 {
            PAGE_EXECUTE_READ
        } else if self.flags & WRITE != 0 {
            PAGE_READWRITE
        } else {
            PAGE_READONLY
        }
    }
}

/// One function pulled in from another module: by name, or by ordinal when the export has none.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportFunc {
    pub name: String,
    pub ordinal: Option<u16>,
    /// Position in the import address table, where the loader writes the resolved address.
    pub slot: u32,
}

/// One imported module and the functions taken from it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Import {
    pub dll: String,
    pub functions: Vec<ImportFunc>,
}

/// Thread-local storage directory: where the loader must find and register the image's TLS block.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tls {
    /// Start of the raw template data.
    pub raw_start: u32,
    pub raw_end: u32,
    /// Address of the `_tls_index` slot for this image.
    pub index_va: u64,
    /// Array of TLS callbacks (VA, called on attach and detach).
    pub callbacks_va: u64,
    /// Extra bytes zeroed after the template.
    pub zero_fill: u32,
}

impl Tls {
    pub fn size(&self) -> u32 {
        self.raw_end.saturating_sub(self.raw_start) + self.zero_fill
    }
}

#[derive(Debug, Clone, Default)]
pub struct Pe {
    pub machine: u16,
    pub plus: bool,
    pub image_base: u64,
    pub entry: u32,
    pub entry_va: u64,
    pub subsystem: u16,
    pub image_size: u32,
    pub headers_size: u32,
    /// Preferred load address of the headers, i.e. where the image wants to start.
    pub sections: Vec<Section>,
    pub imports: Vec<Import>,
    /// Base relocations: (rva, kind), kind 10 = DIR64.
    pub relocs: Vec<(u32, u8)>,
    pub tls: Option<Tls>,
    /// Directory of the image's exception (`.pdata`) table, for x64 unwinding.
    pub exception_rva: u32,
    pub exception_size: u32,
}

fn u16_at(d: &[u8], o: usize) -> Result<u16, String> {
    d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or_else(|| "The executable is truncated.".into())
}
fn u32_at(d: &[u8], o: usize) -> Result<u32, String> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).ok_or_else(|| "The executable is truncated.".into())
}
fn u64_at(d: &[u8], o: usize) -> Result<u64, String> {
    d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| "The executable is truncated.".into())
}

/// A zero-length NUL-terminated string at a file offset, capped so corrupt data can't run away.
fn cstr(d: &[u8], o: usize) -> String {
    let raw = d.get(o..).unwrap_or(&[]);
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len()).min(260);
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

impl Pe {
    /// File offset of a relative virtual address.
    pub fn rva_to_off(&self, rva: u32) -> Option<usize> {
        self.sections.iter().find(|s| rva >= s.vaddr && rva < s.vaddr + s.vsize.max(s.raw_size)).map(|s| (rva - s.vaddr + s.raw_off) as usize)
    }

    /// Span of the import address table: lowest slot to just past the highest. The loader makes
    /// this range read-only once every slot is filled, so the guest can't rewrite its own imports.
    pub fn iat_range(&self) -> Option<(u32, u32)> {
        let mut it = self.imports.iter().flat_map(|i| i.functions.first().zip(i.functions.last()));
        let (lo, hi) = it.next()?;
        let mut range = (lo.slot, hi.slot + 8);
        for (first, last) in it {
            range.0 = range.0.min(first.slot);
            range.1 = range.1.max(last.slot + 8);
        }
        Some(range)
    }

    /// Every DLL this image imports from, in order, without repeats.
    pub fn dll_names(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for i in &self.imports {
            if !v.iter().any(|d| d.eq_ignore_ascii_case(&i.dll)) {
                v.push(i.dll.clone());
            }
        }
        v
    }

    /// Total number of imported functions.
    pub fn import_count(&self) -> usize {
        self.imports.iter().map(|i| i.functions.len()).sum()
    }

    /// True when the file uses the x86-64 instruction set, the only thing Xbox One / Series run.
    pub fn is_x64(&self) -> bool {
        self.machine == MACHINE_AMD64
    }
}

pub fn parse(d: &[u8]) -> Result<Pe, String> {
    if d.get(0..2) != Some(b"MZ") {
        return Err("Not a Windows/Xbox executable (no MZ header).".into());
    }
    let pe = u32_at(d, 0x3C)? as usize;
    if d.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("Not a PE executable.".into());
    }
    let coff = pe + 4;
    let machine = u16_at(d, coff)?;
    let nsec = u16_at(d, coff + 2)? as usize;
    let opt_size = u16_at(d, coff + 16)? as usize;
    let opt = coff + 20;
    let plus = u16_at(d, opt)? == 0x20B;
    let entry = u32_at(d, opt + 16)?;
    let image_base = if plus { u64_at(d, opt + 24)? } else { u32_at(d, opt + 28)? as u64 };
    // SizeOfImage, SizeOfHeaders and Subsystem sit at the same offsets in PE32 and PE32+.
    let image_size = u32_at(d, opt + 56)?;
    let headers_size = u32_at(d, opt + 60)?;
    let subsystem = u16_at(d, opt + 68)?;
    let ndirs = u32_at(d, opt + if plus { 108 } else { 92 })? as usize;
    let dirs = opt + if plus { 112 } else { 96 };
    let mut p = Pe { machine, plus, image_base, entry, entry_va: image_base + entry as u64, subsystem, image_size, headers_size, ..Default::default() };

    let sh = opt + opt_size;
    for i in 0..nsec.min(96) {
        let s = sh + i * 40;
        let name = cstr(d, s);
        p.sections.push(Section {
            name,
            vsize: u32_at(d, s + 8)?,
            vaddr: u32_at(d, s + 12)?,
            raw_size: u32_at(d, s + 16)?,
            raw_off: u32_at(d, s + 20)?,
            flags: u32_at(d, s + 36)?,
        });
    }

    let dir = |i: usize| -> (u32, u32) {
        if i >= ndirs {
            return (0, 0);
        }
        (u32_at(d, dirs + i * 8).unwrap_or(0), u32_at(d, dirs + i * 8 + 4).unwrap_or(0))
    };

    let mut imports = read_imports(d, &p, dir(DD_IMPORT).0)?;
    read_delay_imports(d, &p, dir(DD_DELAY_IMPORT).0, &mut imports);
    p.imports = imports;
    p.relocs = read_relocs(d, &p, dir(DD_BASERELOC).0)?;
    p.tls = read_tls(d, &p, dir(DD_TLS).0, plus)?;
    let (exc, excsz) = dir(3);
    p.exception_rva = exc;
    p.exception_size = excsz;
    // The IAT directory is only worth keeping when the import descriptors gave us nothing.
    if p.imports.is_empty() {
        let (r, _sz) = dir(DD_IAT);
        p.imports = read_imports(d, &p, r).unwrap_or_default();
    }
    Ok(p)
}

/// Walks an import directory (20-byte descriptors, all-zero terminated).
fn read_imports(d: &[u8], p: &Pe, rva: u32) -> Result<Vec<Import>, String> {
    let mut out = Vec::new();
    if rva == 0 {
        return Ok(out);
    }
    let Some(mut o) = p.rva_to_off(rva) else { return Ok(out) };
    for _ in 0..4096 {
        let oft = u32_at(d, o)?;
        let name_rva = u32_at(d, o + 12)?;
        let iat = u32_at(d, o + 16)?;
        if name_rva == 0 && iat == 0 && oft == 0 {
            break;
        }
        // The lookup table is preferred; without it the loader must read the (still unresolved)
        // address table, which is only valid before we patch it.
        let lookup = if oft != 0 { oft } else { iat };
        out.push(Import { dll: p.rva_to_off(name_rva).map_or_else(String::new, |n| cstr(d, n)), functions: read_thunks(d, p, lookup, iat)? });
        o += 20;
    }
    Ok(out)
}

/// Delay-loaded descriptors (ImgDelayDescr) point at the same thunks but keep the name in the
/// descriptor instead of the directory.
fn read_delay_imports(d: &[u8], p: &Pe, rva: u32, into: &mut Vec<Import>) {
    if rva == 0 {
        return;
    }
    let Some(mut o) = p.rva_to_off(rva) else { return };
    for _ in 0..1024 {
        let attrs = match u32_at(d, o) {
            Ok(v) => v,
            Err(_) => return,
        };
        let name_rva = u32_at(d, o + 4).unwrap_or(0);
        if name_rva == 0 {
            return;
        }
        // attrs 1 means RVAs; 0 means VMAs, which still need the image base added.
        let delta = if attrs & 1 == 0 { p.image_base } else { 0 };
        let module = name_rva.checked_add(delta as u32).unwrap_or(name_rva);
        let oft = u32_at(d, o + 8).unwrap_or(0);
        let iat = u32_at(d, o + 16).unwrap_or(0);
        if let Ok(fns) = read_thunks(d, p, oft.checked_add(delta as u32).unwrap_or(oft), iat.checked_add(delta as u32).unwrap_or(iat)) {
            into.push(Import { dll: p.rva_to_off(module).map_or_else(String::new, |n| cstr(d, n)), functions: fns });
        }
        o += 32;
    }
}

/// The array of thunks behind one imported module: 8 bytes per entry in PE32+.
fn read_thunks(d: &[u8], p: &Pe, lookup_rva: u32, iat_rva: u32) -> Result<Vec<ImportFunc>, String> {
    let width = if p.plus { 8 } else { 4 };
    let mut out = Vec::new();
    if lookup_rva == 0 {
        return Ok(out);
    }
    let Some(mut o) = p.rva_to_off(lookup_rva) else { return Ok(out) };
    for i in 0..8192 {
        let v = if p.plus { u64_at(d, o)? } else { u32_at(d, o)? as u64 };
        if v == 0 {
            break;
        }
        let slot = iat_rva + (i as u32) * width as u32;
        // The high bit marks an import by ordinal; everything else points at a hint + name.
        if (if p.plus { v >> 63 } else { v >> 31 }) == 1 {
            out.push(ImportFunc { name: format!("#{}", v & 0xFFFF), ordinal: Some(v as u16), slot });
        } else {
            let name_rva = (v & 0x7FFF_FFFF) as u32;
            let name = p.rva_to_off(name_rva).map_or_else(|| format!("{name_rva:#x}"), |n| cstr(d, n + 2));
            out.push(ImportFunc { name, ordinal: None, slot });
        }
        o += width;
    }
    Ok(out)
}

/// Base relocation blocks: a page RVA, a block size, then 16-bit (type, offset) pairs.
fn read_relocs(d: &[u8], p: &Pe, rva: u32) -> Result<Vec<(u32, u8)>, String> {
    let mut out = Vec::new();
    if rva == 0 {
        return Ok(out);
    }
    let Some(mut o) = p.rva_to_off(rva) else { return Ok(out) };
    for _ in 0..8192 {
        let page = u32_at(d, o)?;
        let size = u32_at(d, o + 4)?;
        if size < 8 {
            break;
        }
        let n = (size as usize - 8) / 2;
        for i in 0..n {
            let e = u16_at(d, o + 8 + i * 2)?;
            let (kind, off) = ((e >> 12) as u8, (e & 0xFFF) as u32);
            // 0 is IMAGE_REL_BASED_ABSOLUTE padding, 10 is DIR64.
            if kind != 0 {
                out.push((page + off as u32, kind));
            }
        }
        o += size as usize;
    }
    Ok(out)
}

/// TLS directory: four virtual addresses, then SizeOfZeroFill and Characteristics.
fn read_tls(d: &[u8], p: &Pe, rva: u32, plus: bool) -> Result<Option<Tls>, String> {
    if rva == 0 {
        return Ok(None);
    }
    let Some(o) = p.rva_to_off(rva) else { return Ok(None) };
    let to_rva = |v: u64| -> Result<u32, String> {
        if v == 0 {
            return Ok(0);
        }
        v.checked_sub(p.image_base)
            .and_then(|rva| u32::try_from(rva).ok())
            .ok_or_else(|| "PE TLS template address is outside the image".into())
    };
    let read_v = |at: usize| -> u64 {
        if plus {
            u64_at(d, at).unwrap_or(0)
        } else {
            u32_at(d, at).unwrap_or(0) as u64
        }
    };
    Ok(Some(Tls {
        raw_start: to_rva(read_v(o))?,
        raw_end: to_rva(read_v(o + 8))?,
        index_va: read_v(o + 16),
        callbacks_va: read_v(o + 24),
        zero_fill: u32_at(d, o + if plus { 32 } else { 16 })?,
    }))
}

/// A minimal but valid PE32+ (AMD64) image whose single section holds an import table for
/// `dlls`. Public because the Game Dump Helper's tests use it to build a package folder this
/// crate will accept, which keeps the two in step.
pub fn sample_pe(dlls: &[&str]) -> Vec<u8> {
    // Room for headers, the section data and every name.
    let mut d = vec![0u8; 0x800];
        d[0..2].copy_from_slice(b"MZ");
        d[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        d[0x80..0x84].copy_from_slice(b"PE\0\0");
        let coff = 0x84;
        d[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes());
        d[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        d[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes());
        let opt = coff + 20;
        d[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        d[opt + 24..opt + 32].copy_from_slice(&0x1_4000_0000u64.to_le_bytes());
        d[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes
        // Import directory -> RVA 0x1000.
        d[opt + 112 + 8..opt + 112 + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        let sh = opt + 240;
        d[sh..sh + 5].copy_from_slice(b".data");
        d[sh + 8..sh + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        d[sh + 12..sh + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        d[sh + 16..sh + 20].copy_from_slice(&0x400u32.to_le_bytes());
        d[sh + 20..sh + 24].copy_from_slice(&0x200u32.to_le_bytes());
        // Section at file 0x200: descriptors, then the lookup/address tables, then names.
        // Everything (descriptors, IAT, lookup table, names) lives in this one section, so keep the
        // RVAs inside it: the section starts at RVA 0x1000.
        let iat_rva = 0x1100u32;
        let _iat_off = 0x200 + (dlls.len() + 1) * 20;
        // Each module gets a 2-entry table: its one name, then the zero that ends the list.
        let lookup_rva = 0x1200u32;
        let lookup_off = 0x200 + (dlls.len() + 1) * 20 + (dlls.len() + 1) * 16;
        let mut name_off = lookup_off + (dlls.len() + 1) * 8;
        for (i, n) in dlls.iter().enumerate() {
            let desc = 0x200 + i * 20;
            let this_lookup = lookup_rva + i as u32 * 16;
            let this_iat = iat_rva + i as u32 * 16;
            let name_rva = (name_off - 0x200 + 0x1000) as u32;
            d[desc..desc + 4].copy_from_slice(&this_lookup.to_le_bytes()); // OriginalFirstThunk (lookup)
            d[desc + 12..desc + 16].copy_from_slice(&name_rva.to_le_bytes()); // Name
            d[desc + 16..desc + 20].copy_from_slice(&this_iat.to_le_bytes()); // FirstThunk (IAT)
            // The lookup entry the parser reads names from, then the zero terminator.
            let at = data_start_for(this_lookup);
            d[at..at + 8].copy_from_slice(&(name_rva as u64).to_le_bytes());
            // The IAT starts out identical to the lookup table, so the loader has a slot to fill.
            let iat = data_start_for(this_iat);
            d[iat..iat + 8].copy_from_slice(&(name_rva as u64).to_le_bytes());
            d[name_off..name_off + n.len()].copy_from_slice(n.as_bytes());
            name_off += n.len() + 1;
        }
        d
}

/// File offset of a one-section PE whose data starts at 0x200 and maps to RVA 0x1000.
fn data_start_for(rva: u32) -> usize {
    (rva - 0x1000) as usize + 0x200
}

#[cfg(test)]
mod tests {
    use super::sample_pe as tiny_pe;

    #[test]
    fn parses_imports() {
        let p = super::parse(&tiny_pe(&["kernelx.dll"])).unwrap();
        assert_eq!(p.machine, 0x8664);
        assert_eq!(p.image_base, 0x1_4000_0000);
        assert_eq!(p.dll_names(), vec!["kernelx.dll"]);
    }
}
