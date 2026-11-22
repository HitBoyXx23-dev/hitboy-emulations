//! The 64-bit x86 ELF a decrypted PlayStation game is made of, with Sony's extra segments.
//!
//! Sony's dynamic-linking data lives in one extra segment (`PT_SCE_DYNLIBDATA`). Its tables
//! (string table, symbol table, relocations, library and module lists) are located by
//! `DT_SCE_*` entries in the ordinary dynamic section, as offsets into that segment. Each
//! imported function's symbol is named `NID#library#module`, where the two trailing numbers
//! are base64 indices into the library and module lists.

use crate::nid;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_TLS: u32 = 7;
const PT_SCE_DYNLIBDATA: u32 = 0x6100_0000;
const PT_SCE_PROCPARAM: u32 = 0x6100_0001;
const PT_SCE_MODULE_PARAM: u32 = 0x6100_0002;
const PT_SCE_RELRO: u32 = 0x6100_0010;

const DT_NEEDED: i64 = 1;
const DT_SCE_ORIGINAL_FILENAME: i64 = 0x6100_0009;
const DT_SCE_MODULE_INFO: i64 = 0x6100_000D;
const DT_SCE_NEEDED_MODULE: i64 = 0x6100_000F;
const DT_SCE_IMPORT_LIB: i64 = 0x6100_0015;
const DT_SCE_JMPREL: i64 = 0x6100_0029;
const DT_SCE_PLTRELSZ: i64 = 0x6100_002D;
const DT_SCE_RELA: i64 = 0x6100_002F;
const DT_SCE_RELASZ: i64 = 0x6100_0031;
const DT_SCE_STRTAB: i64 = 0x6100_0035;
const DT_SCE_STRSIZE: i64 = 0x6100_0037;
const DT_SCE_SYMTAB: i64 = 0x6100_0039;
const DT_SCE_SYMTABSZ: i64 = 0x6100_003F;

#[derive(Debug, Clone)]
pub struct Segment {
    pub kind: &'static str,
    pub offset: u64,
    pub vaddr: u64,
    pub file_size: u64,
    pub mem_size: u64,
    pub flags: u32,
}

impl Segment {
    pub fn is_loadable(&self) -> bool {
        matches!(self.kind, "load" | "sce relro")
    }
}

#[derive(Debug, Clone)]
pub struct Import {
    /// The hash from the symbol name.
    pub nid: String,
    /// The function's name when we know it.
    pub name: Option<&'static str>,
    pub library: String,
}

/// A relocation: patch the 8 bytes at `offset` (relative to the load base).
#[derive(Debug, Clone, Copy)]
pub struct Reloc {
    pub offset: u64,
    /// x86-64 relocation type (8 = relative, 1 = absolute, 6 = global data, 7 = jump slot).
    pub kind: u32,
    pub sym: u32,
    pub addend: i64,
}

/// An entry of the symbol table.
#[derive(Debug, Clone)]
pub struct Sym {
    pub name: String,
    pub value: u64,
    pub kind: u8,
    pub size: u64,
    /// False for imports (undefined here, provided by a system library).
    pub defined: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Elf {
    pub kind: u16,
    pub machine: u16,
    pub entry: u64,
    pub init: Option<u64>,
    pub segments: Vec<Segment>,
    pub module_name: String,
    pub libraries: Vec<String>,
    pub imports: Vec<Import>,
    /// Number of relocations (ordinary + procedure linkage).
    pub relocations: usize,
    pub relocs: Vec<Reloc>,
    pub symbols: Vec<Sym>,
}

fn rd16(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
fn rd32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}
fn rd64(d: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?))
}

fn cstr(d: &[u8], o: usize) -> String {
    let s = d.get(o..).unwrap_or(&[]);
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..end]).into_owned()
}

fn seg_kind(t: u32) -> &'static str {
    match t {
        PT_LOAD => "load",
        PT_DYNAMIC => "dynamic",
        PT_TLS => "tls",
        PT_SCE_DYNLIBDATA => "sce dynlibdata",
        PT_SCE_PROCPARAM => "sce procparam",
        PT_SCE_MODULE_PARAM => "sce moduleparam",
        PT_SCE_RELRO => "sce relro",
        0x6474_E550 => "eh frame",
        0x6474_E551 => "gnu stack",
        _ => "other",
    }
}

impl Elf {
    /// "executable", "dynamic executable" or "library".
    pub fn kind(&self) -> &'static str {
        match self.kind {
            0xFE00 => "PlayStation executable",
            0xFE10 => "PlayStation dynamic executable",
            0xFE18 => "PlayStation library",
            _ => "x86-64",
        }
    }

    pub fn parse(d: &[u8]) -> Result<Elf, String> {
        if d.len() < 64 || &d[..4] != b"\x7FELF" {
            return Err("This isn't a PlayStation program (no ELF header). If it came from your console it may still be encrypted.".into());
        }
        if d[4] != 2 || d[5] != 1 {
            return Err("This program isn't 64-bit little-endian, so it isn't a PlayStation 4 or 5 program.".into());
        }
        let mut e = Elf { kind: rd16(d, 16).unwrap_or(0), machine: rd16(d, 18).unwrap_or(0), entry: rd64(d, 24).unwrap_or(0), ..Default::default() };
        if e.machine != 0x3E {
            return Err("This program isn't for x86-64, so it isn't a PlayStation 4 or 5 program.".into());
        }
        let phoff = rd64(d, 32).unwrap_or(0) as usize;
        let phentsize = rd16(d, 54).unwrap_or(56) as usize;
        let phnum = rd16(d, 56).unwrap_or(0) as usize;
        if phnum == 0 || phnum > 256 || phentsize < 56 {
            return Err("The program header table is missing or damaged.".into());
        }
        let mut dynamic: Option<(usize, usize)> = None;
        let mut dynlib: Option<(usize, usize)> = None;
        for i in 0..phnum {
            let o = phoff + i * phentsize;
            let (Some(t), Some(flags), Some(off), Some(va), Some(fsz), Some(msz)) = (rd32(d, o), rd32(d, o + 4), rd64(d, o + 8), rd64(d, o + 16), rd64(d, o + 32), rd64(d, o + 40)) else {
                return Err("The program header table runs past the end of the file.".into());
            };
            if t == PT_DYNAMIC {
                dynamic = Some((off as usize, fsz as usize));
            }
            if t == PT_SCE_DYNLIBDATA {
                dynlib = Some((off as usize, fsz as usize));
            }
            e.segments.push(Segment { kind: seg_kind(t), offset: off, vaddr: va, file_size: fsz, mem_size: msz, flags });
        }
        if let (Some((doff, dsz)), Some((loff, lsz))) = (dynamic, dynlib) {
            e.read_dynamic(d, doff, dsz, loff, lsz);
        }
        Ok(e)
    }

    fn read_dynamic(&mut self, d: &[u8], doff: usize, dsz: usize, loff: usize, lsz: usize) {
        let lib = match d.get(loff..loff.saturating_add(lsz).min(d.len())) {
            Some(l) => l,
            None => return,
        };
        let mut tags: Vec<(i64, u64)> = Vec::new();
        for k in 0..dsz / 16 {
            let o = doff + k * 16;
            let (Some(tag), Some(val)) = (rd64(d, o), rd64(d, o + 8)) else { break };
            if tag == 0 {
                break;
            }
            tags.push((tag as i64, val));
        }
        let get = |t: i64| tags.iter().find(|x| x.0 == t).map(|x| x.1);
        self.init = get(12);
        let strtab = get(DT_SCE_STRTAB).unwrap_or(0) as usize;
        let name_at = |off: u64| cstr(lib, strtab + (off & 0xFFFF_FFFF) as usize);
        // Module and library lists: value = (id << 48) | name offset in the string table.
        let mut libs: Vec<(u32, String)> = Vec::new();
        for (t, v) in &tags {
            match *t {
                DT_SCE_IMPORT_LIB | DT_SCE_NEEDED_MODULE => {
                    let id = (*v >> 48) as u32;
                    let n = name_at(*v);
                    if *t == DT_SCE_IMPORT_LIB {
                        libs.push((id, n.clone()));
                        if !self.libraries.contains(&n) {
                            self.libraries.push(n);
                        }
                    }
                }
                DT_SCE_MODULE_INFO | DT_SCE_ORIGINAL_FILENAME if self.module_name.is_empty() => {
                    self.module_name = name_at(*v);
                }
                DT_NEEDED if self.module_name.is_empty() => {}
                _ => {}
            }
        }
        // Imported functions: undefined symbols named "NID#lib#module".
        let (symtab, symsz) = (get(DT_SCE_SYMTAB).unwrap_or(0) as usize, get(DT_SCE_SYMTABSZ).unwrap_or(0) as usize);
        let _ = get(DT_SCE_STRSIZE);
        for k in 0..symsz / 24 {
            let o = symtab + k * 24;
            let (Some(name_off), Some(shndx)) = (rd32(lib, o), rd16(lib, o + 6)) else { break };
            let name = cstr(lib, strtab + name_off as usize);
            self.symbols.push(Sym { name: name.clone(), value: rd64(lib, o + 8).unwrap_or(0), kind: lib[o + 4] & 15, size: rd64(lib, o + 16).unwrap_or(0), defined: shndx != 0 });
            let mut parts = name.split('#');
            let (Some(n), Some(libid), Some(_modid)) = (parts.next(), parts.next(), parts.next()) else { continue };
            if shndx != 0 || n.len() != 11 {
                continue;
            }
            let lid = nid::b64_number(libid).unwrap_or(0);
            let library = libs.iter().find(|(id, _)| *id == lid).map(|(_, n)| n.clone()).unwrap_or_else(|| format!("library {lid}"));
            self.imports.push(Import { nid: n.to_string(), name: nid::lookup(n), library });
        }
        // Relocation counts.
        let relasz = get(DT_SCE_RELASZ).unwrap_or(0) as usize;
        let pltsz = get(DT_SCE_PLTRELSZ).unwrap_or(0) as usize;
        self.relocations = (relasz + pltsz) / 24;
        for (tag_off, size) in [(get(DT_SCE_RELA), relasz), (get(DT_SCE_JMPREL), pltsz)] {
            let Some(base) = tag_off else { continue };
            for k in 0..size / 24 {
                let o = base as usize + k * 24;
                let (Some(offset), Some(info), Some(addend)) = (rd64(lib, o), rd64(lib, o + 8), rd64(lib, o + 16)) else { break };
                self.relocs.push(Reloc { offset, kind: info as u32, sym: (info >> 32) as u32, addend: addend as i64 });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal program with one import library and two imported functions, built by hand
    /// in the layout the loader expects.
    pub fn sample_program() -> Vec<u8> {
        let mut dynlib = Vec::new();
        // String table.
        let mut strtab = vec![0u8];
        let add = |s: &str, tab: &mut Vec<u8>| -> u32 {
            let at = tab.len() as u32;
            tab.extend_from_slice(s.as_bytes());
            tab.push(0);
            at
        };
        let lib_name = add("libkernel", &mut strtab);
        let sym1 = add(&format!("{}#A#A", crate::nid::nid("malloc")), &mut strtab);
        let sym2 = add("zzzzzzzzzzz#A#A", &mut strtab);
        let strtab_off = 0usize;
        dynlib.extend_from_slice(&strtab);
        while dynlib.len() % 8 != 0 {
            dynlib.push(0);
        }
        let symtab_off = dynlib.len();
        for (n, shndx) in [(0u32, 0u16), (sym1, 0), (sym2, 0)] {
            dynlib.extend_from_slice(&n.to_le_bytes());
            dynlib.push(0x12);
            dynlib.push(0);
            dynlib.extend_from_slice(&shndx.to_le_bytes());
            dynlib.extend_from_slice(&[0u8; 16]);
        }
        let symtab_size = 3 * 24;

        let dyn_entries: Vec<(i64, u64)> = vec![
            (0x6100_0035, strtab_off as u64),
            (0x6100_0037, strtab.len() as u64),
            (0x6100_0039, symtab_off as u64),
            (0x6100_003B, 24),
            (0x6100_003F, symtab_size as u64),
            (0x6100_0015, (0u64 << 48) | lib_name as u64),
            (0, 0),
        ];
        let mut dynamic = Vec::new();
        for (t, v) in dyn_entries {
            dynamic.extend_from_slice(&t.to_le_bytes());
            dynamic.extend_from_slice(&v.to_le_bytes());
        }

        let phnum = 3usize;
        let hdr_size = 64 + phnum * 56;
        let dyn_off = hdr_size;
        let lib_off = dyn_off + dynamic.len();
        let mut f = vec![0u8; hdr_size];
        f[..4].copy_from_slice(b"\x7FELF");
        f[4] = 2;
        f[5] = 1;
        f[16..18].copy_from_slice(&0xFE10u16.to_le_bytes());
        f[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        f[24..32].copy_from_slice(&0x400000u64.to_le_bytes());
        f[32..40].copy_from_slice(&64u64.to_le_bytes());
        f[54..56].copy_from_slice(&56u16.to_le_bytes());
        f[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
        let mut ph = |i: usize, t: u32, off: usize, sz: usize| {
            let o = 64 + i * 56;
            f[o..o + 4].copy_from_slice(&t.to_le_bytes());
            f[o + 8..o + 16].copy_from_slice(&(off as u64).to_le_bytes());
            f[o + 32..o + 40].copy_from_slice(&(sz as u64).to_le_bytes());
            f[o + 40..o + 48].copy_from_slice(&(sz as u64).to_le_bytes());
        };
        ph(0, PT_LOAD, 0, hdr_size);
        ph(1, PT_DYNAMIC, dyn_off, dynamic.len());
        ph(2, PT_SCE_DYNLIBDATA, lib_off, dynlib.len());
        f.extend_from_slice(&dynamic);
        f.extend_from_slice(&dynlib);
        f
    }

    #[test]
    fn reads_segments_libraries_and_imports() {
        let e = Elf::parse(&sample_program()).unwrap();
        assert_eq!(e.kind(), "PlayStation dynamic executable");
        assert_eq!(e.entry, 0x400000);
        assert_eq!(e.segments.len(), 3);
        assert_eq!(e.libraries, vec!["libkernel".to_string()]);
        assert_eq!(e.imports.len(), 2);
        assert_eq!(e.imports[0].name, Some("malloc"));
        assert_eq!(e.imports[0].library, "libkernel");
        assert_eq!(e.imports[1].name, None);
    }

    #[test]
    fn rejects_other_files() {
        assert!(Elf::parse(b"MZ not an elf at all, just some bytes padded out to sixty-four bytes!!!!").is_err());
        let mut arm = sample_program();
        arm[18] = 0xB7;
        assert!(Elf::parse(&arm).unwrap_err().contains("x86-64"));
    }
}
