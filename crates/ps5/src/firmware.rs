//! Clean-room PlayStation 5 / PS4 System Update (`PS5UPDATE.PUP` / `SLB2`) Firmware Inspector.

/// Parsed PlayStation `SLB2` / `PUP` firmware container metadata.
#[derive(Debug, Clone)]
pub struct PupInfo {
    pub magic: String,
    pub version: u32,
    pub entry_count: u32,
    pub entries: Vec<(String, u64)>, // (filename, size_bytes)
    pub summary: String,
}

/// Inspects a Sony `SLB2` / `PUP` firmware image (`PS5UPDATE.PUP` or `PS4UPDATE.PUP`).
pub fn inspect_pup(data: &[u8]) -> Result<PupInfo, String> {
    if data.len() < 0x20 {
        return Err("File is too small to be a PlayStation firmware update (.PUP).".into());
    }
    if &data[0..4] == b"SLB2" {
        let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let entry_count = u32::from_le_bytes(data[16..20].try_into().unwrap());
        let mut entries = Vec::new();
        for i in 0..(entry_count as usize).min(64) {
            let off = 0x20 + i * 0x30;
            if off + 0x30 > data.len() {
                break;
            }
            let size = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as u64;
            let name = String::from_utf8_lossy(&data[off + 16..off + 48])
                .trim_end_matches('\0')
                .trim()
                .to_string();
            if !name.is_empty() {
                entries.push((name, size));
            }
        }
        let summary = format!(
            "PlayStation SLB2/PUP firmware container (v{version}, {} entries, {:.1} MB).",
            entries.len(),
            data.len() as f64 / (1024.0 * 1024.0)
        );
        return Ok(PupInfo {
            magic: "SLB2".into(),
            version,
            entry_count,
            entries,
            summary,
        });
    }
    if &data[0..4] == &[0x4F, 0x15, 0x3D, 0x1D] || &data[0..4] == &[0x54, 0x14, 0xF5, 0xEE] {
        return Ok(PupInfo {
            magic: "SELF".into(),
            version: 1,
            entry_count: 1,
            entries: vec![("system.self".into(), data.len() as u64)],
            summary: "Signed PlayStation SELF system module detected (requires decrypted ELF/module to run on PC).".into(),
        });
    }
    Err("Unrecognized PlayStation firmware file (expected SLB2 / PS5UPDATE.PUP / PS4UPDATE.PUP).".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slb2_pup_firmware_container() {
        let mut pup = vec![0u8; 0x80];
        pup[0..4].copy_from_slice(b"SLB2");
        pup[4..8].copy_from_slice(&1u32.to_le_bytes());
        pup[16..20].copy_from_slice(&1u32.to_le_bytes());
        pup[0x24..0x28].copy_from_slice(&4096u32.to_le_bytes());
        pup[0x30..0x3E].copy_from_slice(b"PS5UPDATE1.PUP");
        let info = inspect_pup(&pup).unwrap();
        assert_eq!(info.magic, "SLB2");
        assert_eq!(info.entries, vec![("PS5UPDATE1.PUP".into(), 4096)]);
    }
}
