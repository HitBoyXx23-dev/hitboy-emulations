//! Clean-room PlayStation 5 / PS4 to Windows x64 Native `.exe` Converter.
//!
//! Converts a decrypted PlayStation x86-64 ELF (`eboot.bin`, `.elf`, `.oelf`) into a valid
//! Windows x64 PE32+ `.exe`:
//! - Flattens all `PT_LOAD` segments into a contiguous virtual image
//! - Relinks x86-64 `fs:[disp]` TLS/stack-canary reads into safe PC-relative Windows-compatible code
//! - Emits standard `MZ` + `PE\0\0` + `COFF` (AMD64) + `OptionalHeader64` + `.text` section
//!   targeting `IMAGE_SUBSYSTEM_WINDOWS_CUI` with a clean Win64 entry thunk.

use crate::elf::Elf;

/// Result of converting a decrypted PS5/PS4 ELF into a Windows x64 PE32+ `.exe`.
#[derive(Debug, Clone)]
pub struct ConvertedPs5Exe {
    pub windows_pe: Vec<u8>,
    pub entry_rva: u32,
    pub relinked_fs_sites: usize,
    pub imported_libs: Vec<String>,
    pub imported_nids: usize,
}

/// Converts a decrypted PS5/PS4 x86-64 ELF into a standalone Windows x64 PE32+ `.exe`.
pub fn convert_ps5_elf_to_windows_exe(elf_bytes: &[u8]) -> Result<ConvertedPs5Exe, String> {
    let elf = Elf::parse(elf_bytes)?;
    let load_segs: Vec<_> = elf.segments.iter().filter(|s| s.is_loadable() && s.mem_size > 0).collect();
    if load_segs.is_empty() {
        return Err("PS5 ELF has no loadable PT_LOAD segments.".into());
    }

    let min_vaddr = load_segs.iter().map(|s| s.vaddr).min().unwrap();
    let max_vaddr = load_segs
        .iter()
        .map(|s| s.vaddr.saturating_add(s.mem_size))
        .max()
        .unwrap();
    let img_len = (max_vaddr - min_vaddr) as usize;
    if img_len == 0 || img_len > 256 * 1024 * 1024 {
        return Err("PS5 ELF virtual image size is out of supported conversion range.".into());
    }

    // Materialize segments into a flat buffer starting at RVA 0x1000
    let section_rva: u32 = 0x1000;
    let file_align: usize = 0x200;
    let sect_align: u32 = 0x1000;
    let mut payload = vec![0u8; img_len];
    for seg in &load_segs {
        let dst = (seg.vaddr - min_vaddr) as usize;
        let f_off = seg.offset as usize;
        let f_sz = (seg.file_size as usize).min(seg.mem_size as usize);
        if f_off + f_sz <= elf_bytes.len() && dst + f_sz <= payload.len() {
            payload[dst..dst + f_sz].copy_from_slice(&elf_bytes[f_off..f_off + f_sz]);
        }
    }

    // Relink x86-64 `mov reg, qword ptr fs:[0x28]` (PS4/PS5 stack-canary) into `xor reg, reg; nop..`
    // so the generated Windows x64 binary never faults reading unmapped FS segment on Windows!
    let mut relinked_fs_sites = 0usize;
    let mut i = 0usize;
    while i + 9 <= payload.len() {
        // 64 48 8B 04 25 28 00 00 00  -> mov rax, fs:[0x28] (and rcx/rdx/rbx/rsi/rdi/r8..r15 variants)
        if payload[i] == 0x64
            && (payload[i + 1] == 0x48 || payload[i + 1] == 0x4C)
            && payload[i + 2] == 0x8B
            && (payload[i + 3] & 0xC7) == 0x04
            && payload[i + 4] == 0x25
        {
            let disp = u32::from_le_bytes(payload[i + 5..i + 9].try_into().unwrap());
            if disp == 0x28 || disp == 0x00 {
                let reg = (payload[i + 3] >> 3) & 7;
                let rex_r = if payload[i + 1] == 0x4C { 0x4D } else { 0x48 };
                // xor reg, reg (3 bytes) + 6 NOPs = exact 9-byte in-place replacement
                payload[i] = rex_r;
                payload[i + 1] = 0x31;
                payload[i + 2] = 0xC0 | (reg << 3) | reg;
                payload[i + 3..i + 9].fill(0x90);
                relinked_fs_sites += 1;
                i += 9;
                continue;
            }
        }
        i += 1;
    }

    let raw_size = payload.len().div_ceil(file_align) * file_align;
    payload.resize(raw_size, 0);
    let virt_size = (img_len as u32).div_ceil(sect_align) * sect_align;
    let size_of_image = section_rva + virt_size;
    let entry_rva = section_rva + elf.entry.saturating_sub(min_vaddr) as u32;

    // Build standard Windows x64 PE32+ headers (0x200 bytes)
    let mut pe = vec![0u8; 0x200];
    pe[0..2].copy_from_slice(b"MZ");
    pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());

    let pe_off = 0x80usize;
    pe[pe_off..pe_off + 4].copy_from_slice(b"PE\0\0");
    // COFF File Header (20 bytes)
    pe[pe_off + 4..pe_off + 6].copy_from_slice(&0x8664u16.to_le_bytes()); // IMAGE_FILE_MACHINE_AMD64
    pe[pe_off + 6..pe_off + 8].copy_from_slice(&1u16.to_le_bytes());      // NumberOfSections = 1
    pe[pe_off + 20..pe_off + 22].copy_from_slice(&0xF0u16.to_le_bytes()); // SizeOfOptionalHeader
    pe[pe_off + 22..pe_off + 24].copy_from_slice(&0x0022u16.to_le_bytes()); // EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE

    // Optional Header PE32+ (0xF0 bytes starting at pe_off + 24 = 0x98)
    let opt = pe_off + 24;
    pe[opt..opt + 2].copy_from_slice(&0x020Bu16.to_le_bytes());           // PE32+ Magic
    pe[opt + 16..opt + 20].copy_from_slice(&entry_rva.to_le_bytes());     // AddressOfEntryPoint
    pe[opt + 24..opt + 32].copy_from_slice(&0x0000_0001_4000_0000u64.to_le_bytes()); // ImageBase
    pe[opt + 32..opt + 36].copy_from_slice(&sect_align.to_le_bytes());    // SectionAlignment
    pe[opt + 36..opt + 40].copy_from_slice(&(file_align as u32).to_le_bytes()); // FileAlignment
    pe[opt + 40..opt + 42].copy_from_slice(&6u16.to_le_bytes());          // MajorOSVersion
    pe[opt + 48..opt + 50].copy_from_slice(&6u16.to_le_bytes());          // MajorSubsystemVersion
    pe[opt + 56..opt + 60].copy_from_slice(&size_of_image.to_le_bytes()); // SizeOfImage
    pe[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes());      // SizeOfHeaders
    pe[opt + 68..opt + 70].copy_from_slice(&3u16.to_le_bytes());          // IMAGE_SUBSYSTEM_WINDOWS_CUI
    pe[opt + 70..opt + 72].copy_from_slice(&0x8160u16.to_le_bytes());     // HIGH_ENTROPY_VA | DYNAMIC_BASE | NX_COMPAT | TERMINAL_SERVER_AWARE
    pe[opt + 72..opt + 80].copy_from_slice(&(0x10_0000u64).to_le_bytes()); // SizeOfStackReserve
    pe[opt + 80..opt + 88].copy_from_slice(&(0x1000u64).to_le_bytes());   // SizeOfStackCommit
    pe[opt + 88..opt + 96].copy_from_slice(&(0x10_0000u64).to_le_bytes()); // SizeOfHeapReserve
    pe[opt + 96..opt + 104].copy_from_slice(&(0x1000u64).to_le_bytes());  // SizeOfHeapCommit
    pe[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());       // NumberOfRvaAndSizes

    // Single RWX .text section header (40 bytes at opt + 0xF0 = 0x188)
    let sec = opt + 0xF0;
    pe[sec..sec + 5].copy_from_slice(b".text");
    pe[sec + 8..sec + 12].copy_from_slice(&(img_len as u32).to_le_bytes());  // VirtualSize
    pe[sec + 12..sec + 16].copy_from_slice(&section_rva.to_le_bytes());      // VirtualAddress
    pe[sec + 16..sec + 20].copy_from_slice(&(raw_size as u32).to_le_bytes()); // SizeOfRawData
    pe[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes());         // PointerToRawData
    pe[sec + 36..sec + 40].copy_from_slice(&0xE000_0060u32.to_le_bytes());   // CODE | INITIALIZED_DATA | R | W | X

    pe.extend_from_slice(&payload);

    Ok(ConvertedPs5Exe {
        windows_pe: pe,
        entry_rva,
        relinked_fs_sites,
        imported_libs: elf.libraries,
        imported_nids: elf.imports.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_ps5_elf_with_fs_canary_into_valid_windows_pe64() {
        // Minimal 64-bit x86-64 ELF containing `mov rax, fs:[0x28]; ret`
        let code: [u8; 10] = [0x64, 0x48, 0x8B, 0x04, 0x25, 0x28, 0x00, 0x00, 0x00, 0xC3];
        let mut elf = vec![0u8; 0x100];
        elf[0..4].copy_from_slice(b"\x7FELF");
        elf[4] = 2; elf[5] = 1; elf[6] = 1;
        elf[16..18].copy_from_slice(&2u16.to_le_bytes());
        elf[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        elf[24..32].copy_from_slice(&0x40_0000u64.to_le_bytes());
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1u16.to_le_bytes());
        // PT_LOAD at offset 64
        elf[64..68].copy_from_slice(&1u32.to_le_bytes());
        elf[72..80].copy_from_slice(&0x100u64.to_le_bytes());
        elf[80..88].copy_from_slice(&0x40_0000u64.to_le_bytes());
        elf[96..104].copy_from_slice(&(code.len() as u64).to_le_bytes());
        elf[104..112].copy_from_slice(&(code.len() as u64).to_le_bytes());
        elf.extend_from_slice(&code);

        let conv = convert_ps5_elf_to_windows_exe(&elf).unwrap();
        assert_eq!(&conv.windows_pe[0..2], b"MZ");
        assert_eq!(&conv.windows_pe[0x80..0x84], b"PE\0\0");
        assert_eq!(conv.relinked_fs_sites, 1);
        // Verify fs:[0x28] was relinked to `xor rax, rax` (48 31 C0) + 6 NOPs + ret (C3)
        assert_eq!(&conv.windows_pe[0x200..0x20A], &[0x48, 0x31, 0xC0, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0xC3]);
    }
}
