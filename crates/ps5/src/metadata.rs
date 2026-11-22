use crate::GameInfo;
use std::io::Read;
use std::path::Path;

fn u32_at(data: &[u8], offset: usize) -> Result<u32, String> {
    let bytes = data.get(offset..offset + 4).ok_or("The SFO header is truncated.")?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

pub fn parse_sfo(data: &[u8]) -> Result<GameInfo, String> {
    if data.len() > 1 << 20 || data.get(..4) != Some(b"\0PSF") || u32_at(data, 4)? != 0x101 { return Err("The PS4 SFO header is invalid.".into()); }
    let keys = u32_at(data, 8)? as usize;
    let values = u32_at(data, 12)? as usize;
    let count = u32_at(data, 16)? as usize;
    if count > 1024 || keys < 20 + count * 16 || values < keys || values > data.len() { return Err("The SFO tables are invalid.".into()); }
    let mut info = GameInfo::default();
    for index in 0..count {
        let entry = data.get(20 + index * 16..36 + index * 16).ok_or("The SFO entry is truncated.")?;
        let key = keys + u16::from_le_bytes(entry[..2].try_into().unwrap()) as usize;
        let name = data.get(key..values).ok_or("The SFO key offset is invalid.")?;
        let end = name.iter().position(|byte| *byte == 0).ok_or("The SFO key is not terminated.")?;
        let name = std::str::from_utf8(&name[..end]).map_err(|_| "The SFO key is not UTF-8.")?;
        let length = u32_at(entry, 4)? as usize;
        let capacity = u32_at(entry, 8)? as usize;
        let offset = values.checked_add(u32_at(entry, 12)? as usize).ok_or("SFO value offset overflow.")?;
        let end = offset.checked_add(capacity).ok_or("SFO value length overflow.")?;
        let bytes = data.get(offset..end).ok_or("The SFO value is truncated.")?;
        if length > capacity { return Err("The SFO value exceeds its declared capacity.".into()); }
        if !matches!(name, "TITLE" | "TITLE_ID" | "APP_VER") { continue; }
        if u16::from_le_bytes(entry[2..4].try_into().unwrap()) != 0x204 || length > 512 { return Err("The SFO title field has an unsupported format.".into()); }
        let value = std::str::from_utf8(&bytes[..length]).map_err(|_| "The SFO title is not UTF-8.")?.trim_end_matches('\0').to_string();
        match name { "TITLE" => info.title = value, "TITLE_ID" => info.title_id = value, _ => info.version = value }
    }
    Ok(info)
}

pub fn read(path: &Path) -> Result<GameInfo, String> {
    let root = if path.is_dir() { path } else { path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")) };
    let root = root.canonicalize().map_err(|error| format!("Could not open the game folder: {error}"))?;
    for name in ["sce_sys/param.sfo", "param.sfo", "sce_sys/param.json", "param.json"] {
        let file = root.join(name);
        if !file.is_file() { continue; }
        let file = file.canonicalize().map_err(|error| error.to_string())?;
        if !file.starts_with(&root) { return Err("Game metadata must remain inside the game folder.".into()); }
        let mut bytes = Vec::new();
        std::fs::File::open(file).map_err(|error| error.to_string())?.take((1 << 20) + 1).read_to_end(&mut bytes).map_err(|error| error.to_string())?;
        if bytes.len() > 1 << 20 { return Err("The game metadata is too large.".into()); }
        if name.ends_with(".sfo") { return parse_sfo(&bytes); }
        let text = std::str::from_utf8(&bytes).map_err(|_| "The game metadata is not UTF-8.")?;
        return Ok(GameInfo {
            title_id: crate::json_str(text, "titleId").or_else(|| crate::json_str(text, "title_id")).unwrap_or_default(),
            version: crate::json_str(text, "contentVersion").unwrap_or_default(),
            title: crate::json_str(text, "titleName").unwrap_or_default(),
            elf: None,
        });
    }
    Ok(GameInfo::default())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture(id: &str) -> Vec<u8> {
        let fields = [("TITLE_ID", id), ("TITLE", "Test title"), ("APP_VER", "01.00")];
        let mut keys = Vec::new();
        let mut values = Vec::new();
        let mut entries = Vec::new();
        for (key, value) in fields {
            entries.extend_from_slice(&(keys.len() as u16).to_le_bytes());
            entries.extend_from_slice(&0x204u16.to_le_bytes());
            entries.extend_from_slice(&(value.len() as u32 + 1).to_le_bytes());
            entries.extend_from_slice(&(value.len() as u32 + 1).to_le_bytes());
            entries.extend_from_slice(&(values.len() as u32).to_le_bytes());
            keys.extend_from_slice(key.as_bytes());
            keys.push(0);
            values.extend_from_slice(value.as_bytes());
            values.push(0);
        }
        let mut bytes = b"\0PSF".to_vec();
        for value in [0x101u32, 20 + entries.len() as u32, 20 + entries.len() as u32 + keys.len() as u32, 3] { bytes.extend_from_slice(&value.to_le_bytes()); }
        bytes.extend(entries);
        bytes.extend(keys);
        bytes.extend(values);
        bytes
    }

    #[test]
    fn reads_ps4_metadata_and_rejects_damaged_tables() {
        let bytes = fixture("CUSA07669");
        let info = parse_sfo(&bytes).unwrap();
        assert_eq!(info.title_id, "CUSA07669");
        assert_eq!(info.title, "Test title");
        assert_eq!(info.version, "01.00");
        for length in 0..bytes.len() { assert!(parse_sfo(&bytes[..length]).is_err()); }
        let mut malformed = bytes.clone();
        malformed[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_sfo(&malformed).is_err());
    }
}
