use std::borrow::Cow;

const LIMIT: usize = 1 << 30;

fn bytes(data: &[u8], offset: usize, length: usize) -> Result<&[u8], String> {
    let end = offset.checked_add(length).ok_or("Executable offset overflow.")?;
    data.get(offset..end).ok_or_else(|| "The executable is truncated.".into())
}

fn u16_at(data: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(bytes(data, offset, 2)?.try_into().unwrap()))
}

fn u64_at(data: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(bytes(data, offset, 8)?.try_into().unwrap()))
}

fn offset_at(data: &[u8], offset: usize) -> Result<usize, String> {
    usize::try_from(u64_at(data, offset)?).map_err(|_| "The executable offset exceeds this host's address range.".into())
}

pub fn program_image(data: &[u8]) -> Result<Cow<'_, [u8]>, String> {
    if !data.starts_with(&crate::SELF_MAGIC_PS4) {
        if data.starts_with(&crate::SELF_MAGIC_PS5) { return Err("PS5 SELF containers are not supported yet.".into()); }
        return Ok(Cow::Borrowed(data));
    }
    bytes(data, 0, 32)?;
    if data[6] != 1 { return Err("The SELF container is not little-endian.".into()); }
    let count = u16_at(data, 24)? as usize;
    if count == 0 || count > 256 { return Err("The SELF segment table is invalid.".into()); }
    let elf_offset = 32 + count * 32;
    let header = bytes(data, elf_offset, 64)?;
    if &header[..6] != b"\x7FELF\x02\x01" { return Err("The SELF container has no supported ELF header.".into()); }
    let program_offset = offset_at(header, 32)?;
    let entry_size = u16_at(header, 54)? as usize;
    let program_count = u16_at(header, 56)? as usize;
    if program_count == 0 || program_count > 256 || entry_size < 56 || entry_size > 256 {
        return Err("The embedded ELF program table is invalid.".into());
    }
    let program_length = program_count * entry_size;
    let table_offset = elf_offset.checked_add(program_offset).ok_or("ELF program table overflow.")?;
    let table = bytes(data, table_offset, program_length)?;
    let header_end = program_offset.checked_add(program_length).ok_or("ELF header size overflow.")?;
    let mut length = header_end.max(64);
    for program in table.chunks_exact(entry_size) {
        let offset = offset_at(program, 8)?;
        let size = offset_at(program, 32)?;
        length = length.max(offset.checked_add(size).ok_or("ELF segment size overflow.")?);
    }
    if length > LIMIT { return Err("The executable exceeds the supported image size.".into()); }
    let mut copies = Vec::new();
    for index in 0..count {
        let entry = 32 + index * 32;
        let flags = u64_at(data, entry)?;
        if flags & 0x800 == 0 { continue; }
        if flags & 2 != 0 { return Err(crate::ENCRYPTED_MESSAGE.into()); }
        if flags & 8 != 0 { return Err("Compressed SELF segments are not supported yet.".into()); }
        let program_index = ((flags >> 20) & 0xFFF) as usize;
        if program_index >= program_count { return Err("A SELF segment references a missing ELF program header.".into()); }
        let program = &table[program_index * entry_size..(program_index + 1) * entry_size];
        let source = offset_at(data, entry + 8)?;
        let source_length = offset_at(data, entry + 16)?;
        let destination = offset_at(program, 8)?;
        let size = offset_at(program, 32)?;
        if source_length < size { return Err("A SELF segment is shorter than its ELF program header declares.".into()); }
        bytes(data, source, size)?;
        copies.push((source, destination, size));
    }
    for program in table.chunks_exact(entry_size) {
        let kind = u32::from_le_bytes(program[..4].try_into().unwrap());
        if !matches!(kind, 1 | 2 | 0x6100_0000 | 0x6100_0010) { continue; }
        let (start, size) = (offset_at(program, 8)?, offset_at(program, 32)?);
        if size != 0 && !copies.iter().any(|&(_, destination, length)| start >= destination && start + size <= destination + length) {
            return Err("The SELF container is missing a required executable segment.".into());
        }
    }
    let mut image = vec![0u8; length];
    image[..64].copy_from_slice(header);
    image[program_offset..header_end].copy_from_slice(table);
    for (source, destination, size) in copies {
        image[destination..destination + size].copy_from_slice(bytes(data, source, size)?);
    }
    image[40..48].fill(0);
    image[58..64].fill(0);
    Ok(Cow::Owned(image))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(flags: u64) -> Vec<u8> {
        let mut data = vec![0u8; 0x106];
        data[..4].copy_from_slice(&crate::SELF_MAGIC_PS4);
        data[6] = 1;
        data[24..26].copy_from_slice(&1u16.to_le_bytes());
        data[32..40].copy_from_slice(&flags.to_le_bytes());
        data[40..48].copy_from_slice(&0x100u64.to_le_bytes());
        data[48..56].copy_from_slice(&6u64.to_le_bytes());
        let header = 64;
        data[header..header + 6].copy_from_slice(b"\x7FELF\x02\x01");
        data[header + 16..header + 18].copy_from_slice(&0xFE10u16.to_le_bytes());
        data[header + 18..header + 20].copy_from_slice(&62u16.to_le_bytes());
        data[header + 24..header + 32].copy_from_slice(&0x1000u64.to_le_bytes());
        data[header + 32..header + 40].copy_from_slice(&64u64.to_le_bytes());
        data[header + 54..header + 56].copy_from_slice(&56u16.to_le_bytes());
        data[header + 56..header + 58].copy_from_slice(&1u16.to_le_bytes());
        let program = header + 64;
        data[program..program + 4].copy_from_slice(&1u32.to_le_bytes());
        data[program + 4..program + 8].copy_from_slice(&5u32.to_le_bytes());
        data[program + 8..program + 16].copy_from_slice(&0x100u64.to_le_bytes());
        data[program + 16..program + 24].copy_from_slice(&0x1000u64.to_le_bytes());
        data[program + 32..program + 40].copy_from_slice(&6u64.to_le_bytes());
        data[program + 40..program + 48].copy_from_slice(&6u64.to_le_bytes());
        data[0x100..].copy_from_slice(&[0xB8, 7, 0, 0, 0, 0xC3]);
        data
    }

    #[test]
    fn loads_plaintext_self_through_the_cpu() {
        let file = container(0x804);
        let mut machine = crate::machine::Machine::load(&file).unwrap();
        assert_eq!(machine.run(100).unwrap(), crate::machine::Stop::Exited(7));
    }

    #[test]
    fn refuses_encrypted_compressed_and_damaged_segments() {
        assert!(program_image(&container(0x806)).unwrap_err().contains("encrypted"));
        assert!(program_image(&container(0x80C)).unwrap_err().contains("Compressed"));
        assert!(program_image(&container(0x100804)).is_err());
        let mut file = container(0x804);
        file.truncate(0x102);
        assert!(program_image(&file).is_err());
    }
}
