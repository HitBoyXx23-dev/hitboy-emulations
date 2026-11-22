//! PSP game container parsers: EBOOT.PBP, UMD ISO9660 (`.iso`), CSO compressed ISO (`.cso`),
//! `PARAM.SFO` metadata, and MIPS32 ELF / PRX executable loading with NID import discovery.

use std::collections::BTreeMap;
use std::io::Read;

use flate2::read::DeflateDecoder;

/// Parsed PSP game image ready to load into RAM.
#[derive(Clone)]
pub struct LoadedImage {
    pub title: String,
    pub disc_id: String,
    pub elf: Vec<u8>,
    pub files: BTreeMap<String, Vec<u8>>,
}

/// Parsed `PARAM.SFO` key/value metadata.
#[derive(Default, Clone, Debug)]
pub struct ParamSfo {
    pub title: String,
    pub disc_id: String,
    pub category: String,
}

pub fn parse_sfo(data: &[u8]) -> Option<ParamSfo> {
    if data.len() < 20 || &data[0..4] != b"\x00PSF" {
        return None;
    }
    let key_table = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
    let data_table = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
    let count = u32::from_le_bytes(data[16..20].try_into().ok()?) as usize;
    let mut out = ParamSfo::default();

    for i in 0..count.min(256) {
        let off = 20 + i * 16;
        if off + 16 > data.len() {
            break;
        }
        let k_off = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
        let fmt = u16::from_le_bytes([data[off + 2], data[off + 3]]);
        let len = u32::from_le_bytes(data[off + 4..off + 8].try_into().ok()?) as usize;
        let d_off = u32::from_le_bytes(data[off + 12..off + 16].try_into().ok()?) as usize;

        let k_start = key_table.checked_add(k_off)?;
        if k_start >= data.len() {
            continue;
        }
        let k_end = data[k_start..].iter().position(|&b| b == 0).map(|n| k_start + n).unwrap_or(data.len());
        let key = String::from_utf8_lossy(&data[k_start..k_end]);

        // UTF-8 string entry (0x0204 or 0x0004)
        if fmt == 0x0204 || fmt == 0x0004 {
            let d_start = data_table.checked_add(d_off)?;
            let d_end = d_start.saturating_add(len).min(data.len());
            if d_start <= d_end {
                let val = String::from_utf8_lossy(&data[d_start..d_end])
                    .trim_end_matches('\0')
                    .trim()
                    .to_string();
                match key.as_ref() {
                    "TITLE" => out.title = val,
                    "DISC_ID" => out.disc_id = val,
                    "CATEGORY" => out.category = val,
                    _ => {}
                }
            }
        }
    }
    Some(out)
}

/// Detects and loads `.pbp`, `.cso`, `.iso`, or raw MIPS `.elf` PSP executables.
pub fn load_image(data: &[u8]) -> Result<LoadedImage, String> {
    if data.len() >= 40 && &data[0..4] == b"\x00PBP" {
        return parse_pbp(data);
    }
    if data.len() >= 24 && &data[0..4] == b"CISO" {
        let iso = decompress_cso(data)?;
        return parse_iso(&iso);
    }
    if data.len() >= 52 && &data[0..4] == b"\x7FELF" {
        return Ok(LoadedImage {
            title: String::new(),
            disc_id: String::new(),
            elf: data.to_vec(),
            files: BTreeMap::new(),
        });
    }
    if data.len() >= 0x8800 && &data[0x8001..0x8006] == b"CD001" {
        return parse_iso(data);
    }
    Err("Unrecognized PSP file (expected .pbp, .iso, .cso, or decrypted .elf).".into())
}

fn parse_pbp(data: &[u8]) -> Result<LoadedImage, String> {
    // PBP header: magic (4), version (4), 8 u32 offsets:
    // 0: PARAM.SFO, 1: ICON0.PNG, ..., 6: DATA.PSP (ELF/PRX), 7: DATA.PSAR
    let mut offs = [0usize; 9];
    for i in 0..8 {
        let b = 8 + i * 4;
        offs[i] = u32::from_le_bytes(data[b..b + 4].try_into().unwrap()) as usize;
    }
    offs[8] = data.len();

    let slice = |idx: usize| -> &[u8] {
        let s = offs[idx].min(data.len());
        let e = offs[idx + 1].min(data.len());
        if s <= e { &data[s..e] } else { &[] }
    };

    let sfo = parse_sfo(slice(0)).unwrap_or_default();
    let elf = slice(6);
    if elf.len() < 4 {
        return Err("EBOOT.PBP does not contain a DATA.PSP executable.".into());
    }
    if &elf[0..4] == b"~PSP" {
        return Err("Encrypted retail ~PSP PRX cannot be decrypted without console keys. Use a decrypted ISO/ELF or homebrew EBOOT.PBP.".into());
    }
    if &elf[0..4] != b"\x7FELF" {
        return Err("DATA.PSP inside EBOOT.PBP is not a valid MIPS ELF executable.".into());
    }
    let mut files = BTreeMap::new();
    let icon = slice(1);
    if !icon.is_empty() {
        files.insert("ICON0.PNG".into(), icon.to_vec());
    }
    Ok(LoadedImage {
        title: sfo.title,
        disc_id: sfo.disc_id,
        elf: elf.to_vec(),
        files,
    })
}

/// Decompresses a CSO (CISO) compressed UMD image into a standard ISO9660 byte stream.
pub fn decompress_cso(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 24 || &data[0..4] != b"CISO" {
        return Err("Invalid CSO header.".into());
    }
    let total_bytes = u64::from_le_bytes(data[8..16].try_into().unwrap()) as usize;
    let block_size = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    let align = data[21];
    if block_size == 0 || total_bytes > 2 * 1024 * 1024 * 1024 {
        return Err("Invalid CSO block size or uncompressed size.".into());
    }
    let total_blocks = total_bytes.div_ceil(block_size);
    let idx_bytes = (total_blocks + 1) * 4;
    if 24 + idx_bytes > data.len() {
        return Err("Truncated CSO index table.".into());
    }

    let mut out = vec![0u8; total_bytes];
    for b in 0..total_blocks {
        let i0 = u32::from_le_bytes(data[24 + b * 4..28 + b * 4].try_into().unwrap());
        let i1 = u32::from_le_bytes(data[28 + b * 4..32 + b * 4].try_into().unwrap());
        let uncompressed = (i0 & 0x8000_0000) != 0;
        let off = ((i0 & 0x7FFF_FFFF) as usize) << align;
        let next = ((i1 & 0x7FFF_FFFF) as usize) << align;
        let dst_start = b * block_size;
        let dst_end = (dst_start + block_size).min(total_bytes);
        if off >= data.len() {
            break;
        }
        let src_end = next.min(data.len()).max(off);
        if uncompressed {
            let copy_len = (dst_end - dst_start).min(data.len() - off);
            out[dst_start..dst_start + copy_len].copy_from_slice(&data[off..off + copy_len]);
        } else {
            let mut dec = DeflateDecoder::new(&data[off..src_end]);
            let mut buf = Vec::with_capacity(block_size);
            dec.read_to_end(&mut buf).map_err(|e| format!("CSO deflate error at block {b}: {e}"))?;
            let copy_len = (dst_end - dst_start).min(buf.len());
            out[dst_start..dst_start + copy_len].copy_from_slice(&buf[..copy_len]);
        }
    }
    Ok(out)
}

fn parse_iso(iso: &[u8]) -> Result<LoadedImage, String> {
    if iso.len() < 0x8800 || &iso[0x8001..0x8006] != b"CD001" {
        return Err("Invalid ISO9660 UMD image.".into());
    }
    let root_lba = u32::from_le_bytes(iso[0x809E..0x80A2].try_into().unwrap()) as usize;
    let root_size = u32::from_le_bytes(iso[0x80A6..0x80AA].try_into().unwrap()) as usize;

    let mut files = BTreeMap::new();
    walk_iso_dir(iso, root_lba, root_size, "", &mut files, 0);

    let sfo = files
        .get("PSP_GAME/PARAM.SFO")
        .and_then(|b| parse_sfo(b))
        .unwrap_or_default();

    let elf = files
        .get("PSP_GAME/SYSDIR/BOOT.BIN")
        .filter(|b| b.len() >= 4 && &b[0..4] == b"\x7FELF")
        .or_else(|| files.get("PSP_GAME/SYSDIR/EBOOT.BIN"))
        .cloned()
        .ok_or_else(|| "UMD ISO does not contain PSP_GAME/SYSDIR/BOOT.BIN or EBOOT.BIN.".to_string())?;

    if elf.len() >= 4 && &elf[0..4] == b"~PSP" {
        return Err(format!(
            "UMD '{}' ({}) has an encrypted ~PSP EBOOT.BIN. Provide a decrypted BOOT.BIN/ISO or homebrew PBP.",
            if sfo.title.is_empty() { "PSP game" } else { &sfo.title },
            sfo.disc_id
        ));
    }

    Ok(LoadedImage {
        title: sfo.title,
        disc_id: sfo.disc_id,
        elf,
        files,
    })
}

fn walk_iso_dir(iso: &[u8], lba: usize, size: usize, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>, depth: u32) {
    if depth > 6 {
        return;
    }
    let base = lba.saturating_mul(2048);
    let end = base.saturating_add(size).min(iso.len());
    let mut pos = base;
    while pos < end {
        let len = iso[pos] as usize;
        if len == 0 {
            let next_sector = ((pos / 2048) + 1) * 2048;
            if next_sector <= pos || next_sector >= end {
                break;
            }
            pos = next_sector;
            continue;
        }
        if pos + len > end || len < 34 {
            break;
        }
        let entry_lba = u32::from_le_bytes(iso[pos + 2..pos + 6].try_into().unwrap()) as usize;
        let entry_size = u32::from_le_bytes(iso[pos + 10..pos + 14].try_into().unwrap()) as usize;
        let flags = iso[pos + 25];
        let name_len = iso[pos + 32] as usize;
        if pos + 33 + name_len <= pos + len {
            let raw_name = &iso[pos + 33..pos + 33 + name_len];
            if raw_name != [0] && raw_name != [1] {
                let name = String::from_utf8_lossy(raw_name)
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase();
                let full = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
                if (flags & 0x02) != 0 {
                    walk_iso_dir(iso, entry_lba, entry_size, &full, out, depth + 1);
                } else {
                    let f_start = entry_lba.saturating_mul(2048);
                    let f_end = f_start.saturating_add(entry_size).min(iso.len());
                    if f_start <= f_end && entry_size <= 64 * 1024 * 1024 {
                        out.insert(full, iso[f_start..f_end].to_vec());
                    }
                }
            }
        }
        pos += len;
    }
}

/// NID import entry patched into a MIPS stub (`jr $ra; syscall <id>`).
#[derive(Debug, Clone)]
pub struct NidImport {
    pub module: String,
    pub nid: u32,
    pub stub_addr: u32,
}

/// Loads a 32-bit little-endian MIPS ELF (or PSP PRX) into guest RAM and resolves PSP NID stubs.
pub fn load_elf_into_ram<W: FnMut(u32, &[u8])>(
    elf: &[u8],
    mut write_slice: W,
    read_u32: impl Fn(u32) -> u32,
    read_cstr: impl Fn(u32) -> String,
) -> Result<(u32, u32, Vec<NidImport>), String> {
    if elf.len() < 52 || &elf[0..4] != b"\x7FELF" {
        return Err("Not a valid ELF executable.".into());
    }
    let e_type = u16::from_le_bytes([elf[16], elf[17]]);
    let e_machine = u16::from_le_bytes([elf[18], elf[19]]);
    if e_machine != 8 {
        return Err(format!("Unsupported ELF machine {e_machine} (expected MIPS = 8)."));
    }
    let reloc_base: u32 = if e_type == 0xFFA0 { 0x0880_4000 } else { 0 };
    let entry = u32::from_le_bytes(elf[24..28].try_into().unwrap()).wrapping_add(reloc_base);
    let phoff = u32::from_le_bytes(elf[28..32].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes([elf[42], elf[43]]) as usize;
    let phnum = u16::from_le_bytes([elf[44], elf[45]]) as usize;

    let mut gp = 0u32;
    let mut modinfo_paddr = None;

    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if off + 32 > elf.len() {
            break;
        }
        let p_type = u32::from_le_bytes(elf[off..off + 4].try_into().unwrap());
        let p_offset = u32::from_le_bytes(elf[off + 4..off + 8].try_into().unwrap()) as usize;
        let p_vaddr = u32::from_le_bytes(elf[off + 8..off + 12].try_into().unwrap()).wrapping_add(reloc_base);
        let p_paddr = u32::from_le_bytes(elf[off + 12..off + 16].try_into().unwrap());
        let p_filesz = u32::from_le_bytes(elf[off + 16..off + 20].try_into().unwrap()) as usize;
        let p_memsz = u32::from_le_bytes(elf[off + 20..off + 24].try_into().unwrap()) as usize;

        if p_type == 1 {
            // PT_LOAD
            if p_offset + p_filesz <= elf.len() {
                write_slice(p_vaddr, &elf[p_offset..p_offset + p_filesz]);
            }
            if p_memsz > p_filesz {
                let zeros = vec![0u8; p_memsz - p_filesz];
                write_slice(p_vaddr.wrapping_add(p_filesz as u32), &zeros);
            }
            if i == 0 && p_paddr != 0 {
                modinfo_paddr = Some(p_paddr);
            }
        }
    }

    // Parse section headers for `.rodata.sceModuleInfo` and `.lib.stub`
    let shoff = u32::from_le_bytes(elf[32..36].try_into().unwrap()) as usize;
    let shentsize = u16::from_le_bytes([elf[46], elf[47]]) as usize;
    let shnum = u16::from_le_bytes([elf[48], elf[49]]) as usize;
    let shstrndx = u16::from_le_bytes([elf[50], elf[51]]) as usize;

    let mut stub_range: Option<(u32, u32)> = None;
    if shoff > 0 && shnum > 0 && shstrndx < shnum {
        let str_off_hdr = shoff + shstrndx * shentsize;
        if str_off_hdr + 24 <= elf.len() {
            let str_off = u32::from_le_bytes(elf[str_off_hdr + 16..str_off_hdr + 20].try_into().unwrap()) as usize;
            let str_sz = u32::from_le_bytes(elf[str_off_hdr + 20..str_off_hdr + 24].try_into().unwrap()) as usize;
            let strtab = if str_off + str_sz <= elf.len() { &elf[str_off..str_off + str_sz] } else { &[] };

            for i in 0..shnum {
                let off = shoff + i * shentsize;
                if off + 24 > elf.len() {
                    break;
                }
                let name_idx = u32::from_le_bytes(elf[off..off + 4].try_into().unwrap()) as usize;
                let sh_addr = u32::from_le_bytes(elf[off + 12..off + 16].try_into().unwrap()).wrapping_add(reloc_base);
                let sh_size = u32::from_le_bytes(elf[off + 20..off + 24].try_into().unwrap());
                if name_idx < strtab.len() {
                    let end = strtab[name_idx..].iter().position(|&b| b == 0).map(|n| name_idx + n).unwrap_or(strtab.len());
                    let name = String::from_utf8_lossy(&strtab[name_idx..end]);
                    if name == ".rodata.sceModuleInfo" && sh_size >= 52 {
                        gp = read_u32(sh_addr + 32);
                        let stub_top = read_u32(sh_addr + 44);
                        let stub_end = read_u32(sh_addr + 48);
                        if stub_end > stub_top {
                            stub_range = Some((stub_top, stub_end));
                        }
                    } else if name == ".lib.stub" && sh_size >= 20 && stub_range.is_none() {
                        stub_range = Some((sh_addr, sh_addr.wrapping_add(sh_size)));
                    }
                }
            }
        }
    }

    if stub_range.is_none() {
        if let Some(paddr) = modinfo_paddr {
            let info_addr = (paddr & 0x7FFF_FFFF).wrapping_add(reloc_base);
            gp = read_u32(info_addr + 32);
            let stub_top = read_u32(info_addr + 44);
            let stub_end = read_u32(info_addr + 48);
            if stub_end > stub_top && stub_end - stub_top < 0x10000 {
                stub_range = Some((stub_top, stub_end));
            }
        }
    }

    let mut imports = Vec::new();
    if let Some((mut cur, end)) = stub_range {
        while cur + 20 <= end {
            let name_ptr = read_u32(cur);
            let w1 = read_u32(cur + 4);
            let w2 = read_u32(cur + 8);
            let nid_table = read_u32(cur + 12);
            let stub_table = read_u32(cur + 16);
            let size_words = ((w2 & 0xFF) as u32).max(5);
            let num_funcs = (w2 >> 16) as u32;
            let _ver = w1;
            let module = if name_ptr != 0 { read_cstr(name_ptr) } else { String::new() };
            for i in 0..num_funcs.min(256) {
                let nid = read_u32(nid_table.wrapping_add(i * 4));
                let stub_addr = stub_table.wrapping_add(i * 8);
                imports.push(NidImport {
                    module: module.clone(),
                    nid,
                    stub_addr,
                });
            }
            cur = cur.wrapping_add(size_words * 4);
        }
    }

    Ok((entry, gp, imports))
}
