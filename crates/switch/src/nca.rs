//! NCA (Nintendo Content Archive) files: the encrypted containers that hold a game's program
//! (ExeFS), assets (RomFS) and metadata. The header is AES-XTS encrypted with `header_key`; each
//! section is AES-CTR encrypted with a key unwrapped from the header's key area (or, for
//! content with rights, a title key from the game's ticket).

use crate::aes;
use crate::keys::Keys;
use crate::pfs::{self, Entry};
use std::collections::HashMap;
use std::fs::File;

pub type RightsId = [u8; 16];

#[derive(Debug, Clone)]
pub struct Section {
    /// Byte range of the section inside the NCA.
    pub start: u64,
    pub end: u64,
    /// 0 = RomFS, 1 = PFS0.
    pub fs_type: u8,
    /// 2 = hierarchical SHA-256 (PFS0), 3 = hierarchical integrity (RomFS).
    pub hash_type: u8,
    /// 1 = none, 2 = XTS, 3 = CTR, 4 = CTR with patch (BKTR).
    pub encryption: u8,
    pub counter_hi: [u8; 8],
    /// Where the file system itself sits inside the section (past the hash tables).
    pub data_offset: u64,
    pub data_size: u64,
}

pub struct Nca {
    /// Offset of the NCA inside the package file.
    pub base: u64,
    pub content_type: u8,
    pub program_id: u64,
    pub content_size: u64,
    pub rights_id: RightsId,
    pub key_gen: u8,
    pub key_area_kind: u8,
    pub encrypted_keys: [[u8; 16]; 4],
    pub sections: [Option<Section>; 4],
    key: Option<[u8; 16]>,
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn le64(b: &[u8], o: usize) -> u64 {
    le32(b, o) as u64 | (le32(b, o + 4) as u64) << 32
}

impl Nca {
    /// Reads and decrypts the header of the NCA described by `entry`.
    pub fn open(f: &mut File, entry: &Entry, keys: &Keys) -> Result<Nca, String> {
        if entry.size < 0xC00 {
            return Err(format!("{} is too small to be an NCA.", entry.name));
        }
        let header_key: [u8; 32] = keys.get("header_key").and_then(|k| k.try_into().ok()).ok_or("header_key must be 32 bytes.")?;
        let mut h = pfs::read_at(f, entry.offset, 0xC00)?;
        aes::xts_decrypt(&header_key, 0, 0x200, &mut h);
        match &h[0x200..0x204] {
            b"NCA3" => {}
            b"NCA2" | b"NCA0" => return Err(format!("{} uses an old NCA format that isn't supported.", entry.name)),
            _ => return Err(format!("Couldn't decrypt {}: the header_key in your keys file doesn't match this game.", entry.name)),
        }
        let mut rights_id = [0u8; 16];
        rights_id.copy_from_slice(&h[0x230..0x240]);
        let mut encrypted_keys = [[0u8; 16]; 4];
        for (k, ek) in encrypted_keys.iter_mut().enumerate() {
            ek.copy_from_slice(&h[0x300 + k * 16..0x310 + k * 16]);
        }
        let mut sections: [Option<Section>; 4] = [None, None, None, None];
        for (k, slot) in sections.iter_mut().enumerate() {
            let e = 0x240 + k * 0x10;
            let (start, end) = (le32(&h, e) as u64 * 0x200, le32(&h, e + 4) as u64 * 0x200);
            if end <= start {
                continue;
            }
            let fs = &h[0x400 + k * 0x200..0x600 + k * 0x200];
            let mut counter_hi = [0u8; 8];
            for i in 0..8 {
                counter_hi[i] = fs[0x140 + 7 - i];
            }
            let (hash_type, layers) = (fs[3], le32(fs, 0x2C) as usize);
            let (data_offset, data_size) = if hash_type == 2 && (1..=2).contains(&layers) {
                let r = 0x30 + (layers - 1) * 16;
                (le64(fs, r), le64(fs, r + 8))
            } else if hash_type == 3 && &fs[0x08..0x0C] == b"IVFC" {
                let max_levels = le32(fs, 0x10) as usize;
                if (1..=6).contains(&max_levels) {
                    let lvl = 0x18 + (max_levels - 1) * 24;
                    (le64(fs, lvl), le64(fs, lvl + 8))
                } else {
                    (0, end - start)
                }
            } else {
                (0, end - start)
            };
            *slot = Some(Section { start, end, fs_type: fs[2], hash_type, encryption: fs[4], counter_hi, data_offset, data_size });
        }
        Ok(Nca {
            base: entry.offset,
            content_type: h[0x205],
            program_id: le64(&h, 0x210),
            content_size: le64(&h, 0x208),
            rights_id,
            key_gen: h[0x206].max(h[0x220]),
            key_area_kind: h[0x207],
            encrypted_keys,
            sections,
            key: None,
        })
    }

    pub fn uses_rights(&self) -> bool {
        self.rights_id != [0; 16]
    }

    /// Works out the key the sections are encrypted with.
    pub fn unlock(&mut self, keys: &Keys, title_keys: &HashMap<RightsId, [u8; 16]>) -> Result<(), String> {
        if self.uses_rights() {
            let k = title_keys.get(&self.rights_id).ok_or("This content needs its ticket (.tik), which isn't in the package.")?;
            self.key = Some(*k);
            return Ok(());
        }
        let idx = self.key_gen.saturating_sub(1);
        let kind = ["application", "ocean", "system"].get(self.key_area_kind as usize).ok_or("The NCA names an unknown key area.")?;
        let name = format!("key_area_key_{kind}_{idx:02x}");
        let kek = keys.get(&name).and_then(aes::key16).ok_or_else(|| format!("Your keys file is missing {name}. This game needs a newer prod.keys from your Switch."))?;
        self.key = Some(aes::ecb_decrypt_block(&kek, &self.encrypted_keys[2]));
        Ok(())
    }

    /// Reads `len` bytes at `off` inside section `idx`, decrypted.
    pub fn read_section(&self, f: &mut File, idx: usize, off: u64, len: usize) -> Result<Vec<u8>, String> {
        let s = self.sections.get(idx).and_then(|s| s.as_ref()).ok_or("The NCA has no such section.")?;
        let abs = s.start + off;
        if abs + len as u64 > s.end {
            return Err("A read ran past the end of the section.".into());
        }
        let aligned = abs & !15;
        let pre = (abs - aligned) as usize;
        let mut buf = pfs::read_at(f, self.base + aligned, pre + len)?;
        match s.encryption {
            1 => {}
            3 => {
                let key = self.key.ok_or("The NCA hasn't been unlocked.")?;
                let mut ctr = [0u8; 16];
                ctr[..8].copy_from_slice(&s.counter_hi);
                ctr[8..].copy_from_slice(&(aligned >> 4).to_be_bytes());
                aes::ctr_xor(&key, &ctr, &mut buf);
            }
            2 => return Err("XTS-encrypted sections aren't supported.".into()),
            _ => return Err("Update (patch) sections aren't supported yet.".into()),
        }
        Ok(buf[pre..].to_vec())
    }

    /// The files of a Program NCA's ExeFS (main, rtld, sdk, subsdk*, main.npdm).
    pub fn exefs(&self, f: &mut File) -> Result<Vec<(String, Vec<u8>)>, String> {
        let s = self.sections[0].as_ref().filter(|s| s.fs_type == 1 && s.hash_type == 2).ok_or("This NCA has no ExeFS.")?;
        if s.data_size > 0x1000_0000 {
            return Err("The ExeFS is implausibly large.".into());
        }
        let data = self.read_section(f, 0, s.data_offset, s.data_size as usize)?;
        let list = pfs::list_fs_bytes(&data)?;
        list.iter()
            .map(|e| data.get(e.offset as usize..(e.offset + e.size) as usize).map(|b| (e.name.clone(), b.to_vec())).ok_or_else(|| "The ExeFS is damaged.".to_string()))
            .collect()
    }

    /// Reads the Program NCA's RomFS section (section 1) when present and under 256 MiB.
    pub fn romfs_bytes(&self, f: &mut File) -> Result<Option<Vec<u8>>, String> {
        let Some((idx, s)) = self
            .sections
            .iter()
            .enumerate()
            .find_map(|(i, s)| s.as_ref().filter(|s| s.fs_type == 0 && s.hash_type == 3).map(|s| (i, s)))
        else {
            return Ok(None);
        };
        if s.data_size == 0 || s.data_size > 0x1000_0000 {
            return Ok(None);
        }
        Ok(Some(self.read_section(f, idx, s.data_offset, s.data_size as usize)?))
    }
}

/// Reads a common (non-personalized) ticket and returns its rights id and decrypted title key.
pub fn parse_ticket(b: &[u8], keys: &Keys) -> Result<(RightsId, [u8; 16]), String> {
    let sig = le32(b.get(..4).ok_or("The ticket is truncated.")?, 0);
    let sig_len = match sig.checked_sub(0x10000).map(|t| t % 3) {
        Some(0) if sig <= 0x10005 => 0x200 + 0x3C,
        Some(1) if sig <= 0x10005 => 0x100 + 0x3C,
        Some(2) if sig <= 0x10005 => 0x3C + 0x40,
        _ => return Err("Unrecognised ticket signature type.".into()),
    };
    let body = b.get(4 + sig_len..).filter(|x| x.len() >= 0x170).ok_or("The ticket is truncated.")?;
    if body[0x141] != 0 {
        return Err("This is a personalized ticket (tied to one console); it can't be used without that console's RSA key.".into());
    }
    let idx = body[0x145].saturating_sub(1);
    let name = format!("titlekek_{idx:02x}");
    let kek = keys.get(&name).and_then(aes::key16).ok_or_else(|| format!("Your keys file is missing {name}."))?;
    let mut rights = [0u8; 16];
    rights.copy_from_slice(&body[0x160..0x170]);
    let enc = aes::key16(&body[0x40..]).unwrap();
    Ok((rights, aes::ecb_decrypt_block(&kek, &enc)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::aes::Aes128;
    use std::io::Write;

    pub const HEADER_KEY: [u8; 32] = [0x42; 32];
    pub const KEK: [u8; 16] = [0x24; 16];
    pub const AREA_KEY: [u8; 16] = [0x77; 16];

    pub fn keys_text(extra: &str) -> String {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        format!("header_key = {}\nkey_area_key_application_00 = {}\ntitlekek_00 = {}\n{extra}", hex(&HEADER_KEY), hex(&AREA_KEY), hex(&KEK))
    }

    pub fn pfs0(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut strtab = Vec::new();
        let (mut entries, mut data) = (Vec::new(), Vec::new());
        for (name, body) in files {
            let mut e = vec![0u8; 0x18];
            e[0..8].copy_from_slice(&(data.len() as u64).to_le_bytes());
            e[8..16].copy_from_slice(&(body.len() as u64).to_le_bytes());
            e[16..20].copy_from_slice(&(strtab.len() as u32).to_le_bytes());
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
            entries.extend(e);
            data.extend_from_slice(body);
        }
        while strtab.len() % 0x20 != 0 {
            strtab.push(0);
        }
        let mut out = b"PFS0".to_vec();
        out.extend_from_slice(&(files.len() as u32).to_le_bytes());
        out.extend_from_slice(&(strtab.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend(entries);
        out.extend(strtab);
        out.extend(data);
        out
    }

    /// Builds a program NCA with one PFS0 section, encrypted the way the console does.
    pub fn make_nca(exefs: &[(&str, &[u8])], rights_id: [u8; 16], section_key: [u8; 16]) -> Vec<u8> {
        let mut h = vec![0u8; 0xC00];
        h[0x200..0x204].copy_from_slice(b"NCA3");
        h[0x205] = 0; // program
        h[0x210..0x218].copy_from_slice(&0x0100_0000_0000_1000u64.to_le_bytes());
        h[0x230..0x240].copy_from_slice(&rights_id);
        h[0x240..0x244].copy_from_slice(&8u32.to_le_bytes()); // section 0: 0x1000..0x3000
        h[0x244..0x248].copy_from_slice(&0x18u32.to_le_bytes());
        // Wrap the section key with the key-area key (ECB encrypt) into slot 2.
        let mut wrapped = section_key;
        Aes128::new(&AREA_KEY).encrypt_block(&mut wrapped);
        h[0x320..0x330].copy_from_slice(&wrapped);
        // Section 0's file system header.
        let fs = &mut h[0x400..0x600];
        fs[2] = 1; // PFS0
        fs[3] = 2; // hierarchical SHA-256
        fs[4] = 3; // CTR
        fs[0x2C..0x30].copy_from_slice(&2u32.to_le_bytes());
        fs[0x40..0x48].copy_from_slice(&0x400u64.to_le_bytes()); // data offset in section
        let fsdata = pfs0(exefs);
        fs[0x48..0x50].copy_from_slice(&(fsdata.len() as u64).to_le_bytes());
        fs[0x140..0x148].copy_from_slice(&[9, 8, 7, 6, 5, 4, 3, 2]);
        let mut nca = h.clone();
        aes::xts_encrypt(&HEADER_KEY, 0, 0x200, &mut nca);
        nca.resize(0x1000, 0);
        let mut section = vec![0u8; 0x2000];
        section[0x400..0x400 + fsdata.len()].copy_from_slice(&fsdata);
        let mut ctr = [0u8; 16];
        ctr[..8].copy_from_slice(&[2, 3, 4, 5, 6, 7, 8, 9]);
        ctr[8..].copy_from_slice(&(0x1000u64 >> 4).to_be_bytes());
        aes::ctr_xor(&section_key, &ctr, &mut section);
        nca.extend(section);
        nca
    }

    fn write_tmp(name: &str, b: &[u8]) -> File {
        let p = std::env::temp_dir().join(name);
        std::fs::File::create(&p).unwrap().write_all(b).unwrap();
        File::open(p).unwrap()
    }

    #[test]
    fn opens_an_nca_and_reads_the_exefs() {
        let section_key = [0x31; 16];
        let bytes = make_nca(&[("main", b"MAINDATA"), ("rtld", b"RTLD")], [0; 16], section_key);
        let mut f = write_tmp("nca_basic.nca", &bytes);
        let keys = Keys::parse(&keys_text("")).unwrap();
        let entry = Entry { name: "x.nca".into(), offset: 0, size: bytes.len() as u64 };
        let mut nca = Nca::open(&mut f, &entry, &keys).unwrap();
        assert_eq!((nca.content_type, nca.program_id), (0, 0x0100_0000_0000_1000));
        nca.unlock(&keys, &HashMap::new()).unwrap();
        let files = nca.exefs(&mut f).unwrap();
        assert_eq!(files, vec![("main".to_string(), b"MAINDATA".to_vec()), ("rtld".to_string(), b"RTLD".to_vec())]);
    }

    #[test]
    fn wrong_header_key_is_explained() {
        let bytes = make_nca(&[("main", b"x")], [0; 16], [1; 16]);
        let mut f = write_tmp("nca_wrongkey.nca", &bytes);
        let keys = Keys::parse("header_key = 00000000000000000000000000000000000000000000000000000000000000ff\nkey_area_key_application_00 = 00000000000000000000000000000000\n").unwrap();
        let entry = Entry { name: "x.nca".into(), offset: 0, size: bytes.len() as u64 };
        let e = Nca::open(&mut f, &entry, &keys).err().unwrap();
        assert!(e.contains("header_key"));
    }

    #[test]
    fn ticket_titlekey_unlocks_rights_content() {
        let title_key = [0x5A; 16];
        let rights = [0xAB; 16];
        let bytes = make_nca(&[("main", b"RIGHTS")], rights, title_key);
        // A common RSA-2048/SHA-256 ticket carrying the title key encrypted with titlekek_00.
        let mut t = vec![0u8; 4 + 0x13C + 0x180];
        t[..4].copy_from_slice(&0x10004u32.to_le_bytes());
        let body = 4 + 0x13C;
        let mut enc = title_key;
        Aes128::new(&KEK).encrypt_block(&mut enc);
        t[body + 0x40..body + 0x50].copy_from_slice(&enc);
        t[body + 0x160..body + 0x170].copy_from_slice(&rights);
        let keys = Keys::parse(&keys_text("")).unwrap();
        let (rid, tk) = parse_ticket(&t, &keys).unwrap();
        assert_eq!((rid, tk), (rights, title_key));
        t[body + 0x141] = 1;
        assert!(parse_ticket(&t, &keys).unwrap_err().contains("personalized"));

        let mut f = write_tmp("nca_rights.nca", &bytes);
        let entry = Entry { name: "r.nca".into(), offset: 0, size: bytes.len() as u64 };
        let mut nca = Nca::open(&mut f, &entry, &keys).unwrap();
        assert!(nca.uses_rights());
        assert!(nca.unlock(&keys, &HashMap::new()).is_err());
        nca.unlock(&keys, &HashMap::from([(rid, tk)])).unwrap();
        assert_eq!(nca.exefs(&mut f).unwrap()[0].1, b"RIGHTS");
    }
}
