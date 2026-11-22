//! Wii discs: raw .iso or .wbfs images, the partition table, and the game partition's
//! AES-encrypted data (32 KB clusters: 1 KB of hashes + 31 KB of data).
//!
//! Each partition's title key is itself encrypted with the console's common key, which the
//! user dumps from their own Wii (16 bytes); nothing here ships with a key.

use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const WII_MAGIC: u32 = 0x5D1C_9EA3;
const CLUSTER: u64 = 0x8000;
const CLUSTER_DATA: u64 = 0x7C00;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

pub fn aes_cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
    let _ = Aes128CbcDec::new(key.into(), iv.into()).decrypt_padded_mut::<NoPadding>(data);
}

/// Byte access to the whole (unencrypted-layout) disc, whatever the file format.
pub trait Raw: Send {
    fn read_at(&mut self, off: u64, buf: &mut [u8]);
}

pub struct IsoRaw(File);

impl Raw for IsoRaw {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) {
        buf.fill(0);
        if self.0.seek(SeekFrom::Start(off)).is_ok() {
            let _ = self.0.read(buf);
        }
    }
}

/// WBFS: the disc is stored as large sectors listed in a lookup table (unused ones omitted).
pub struct WbfsRaw {
    f: File,
    shift: u32,
    table: Vec<u16>,
}

impl WbfsRaw {
    pub fn open(mut f: File) -> Result<WbfsRaw, String> {
        let mut h = [0u8; 16];
        f.read_exact(&mut h).map_err(|e| e.to_string())?;
        if &h[..4] != b"WBFS" {
            return Err("Not a WBFS file.".into());
        }
        let hd_shift = h[8] as u32;
        let shift = h[9] as u32;
        if !(9..=31).contains(&hd_shift) || !(15..=31).contains(&shift) {
            return Err("This WBFS file's header is damaged.".into());
        }
        let hd_sec = 1u64 << hd_shift;
        // A dual-layer Wii disc is 143432 * 2 Wii sectors of 32 KB.
        let sectors = ((143_432u64 * 2) << 15 >> shift) as usize;
        let mut t = vec![0u8; sectors * 2];
        f.seek(SeekFrom::Start(hd_sec + 0x100)).map_err(|e| e.to_string())?;
        f.read_exact(&mut t).map_err(|_| "This WBFS file is truncated.".to_string())?;
        let table = t.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        Ok(WbfsRaw { f, shift, table })
    }
}

impl Raw for WbfsRaw {
    fn read_at(&mut self, mut off: u64, buf: &mut [u8]) {
        let mask = (1u64 << self.shift) - 1;
        let mut done = 0;
        while done < buf.len() {
            let idx = (off >> self.shift) as usize;
            let within = off & mask;
            let n = ((mask + 1 - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            out.fill(0);
            if let Some(&lba) = self.table.get(idx) {
                if lba != 0 && self.f.seek(SeekFrom::Start(((lba as u64) << self.shift) + within)).is_ok() {
                    let _ = self.f.read(out);
                }
            }
            done += n;
            off += n as u64;
        }
    }
}

pub fn open_raw(path: &Path) -> Result<Box<dyn Raw>, String> {
    let mut f = File::open(path).map_err(|e| format!("Can't open {}: {e}", path.display()))?;
    let mut magic = [0u8; 4];
    let _ = f.read(&mut magic);
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    if &magic == b"WBFS" {
        Ok(Box::new(WbfsRaw::open(f)?))
    } else {
        Ok(Box::new(IsoRaw(f)))
    }
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Whether the file is a Wii disc (.iso or .wbfs).
pub fn is_wii_disc(path: &Path) -> bool {
    let Ok(mut raw) = open_raw(path) else { return false };
    let mut h = [0u8; 0x20];
    raw.read_at(0, &mut h);
    be32(&h, 0x18) == WII_MAGIC
}

/// The decrypted view of a disc's game partition.
pub struct Partition {
    raw: Box<dyn Raw>,
    data_off: u64,
    data_size: u64,
    key: [u8; 16],
    cached: Option<(u64, Vec<u8>)>,
    pub title_id: [u8; 8],
}

impl Partition {
    /// Opens the first game-data partition using the user's common key.
    pub fn open(mut raw: Box<dyn Raw>, common_key: &[u8; 16]) -> Result<Partition, String> {
        let mut h = [0u8; 0x20];
        raw.read_at(0, &mut h);
        if be32(&h, 0x18) != WII_MAGIC {
            return Err("This isn't a Wii disc (missing Wii magic).".into());
        }
        let mut info = [0u8; 0x20];
        raw.read_at(0x4_0000, &mut info);
        let mut game = None;
        for g in 0..4 {
            let (count, table) = (be32(&info, g * 8) as usize, (be32(&info, g * 8 + 4) as u64) << 2);
            let mut entries = vec![0u8; count.min(64) * 8];
            raw.read_at(table, &mut entries);
            for e in entries.chunks_exact(8) {
                if be32(e, 4) == 0 && game.is_none() {
                    game = Some((be32(e, 0) as u64) << 2);
                }
            }
        }
        let part = game.ok_or("This Wii disc has no game partition.")?;
        let mut head = vec![0u8; 0x2C0];
        raw.read_at(part, &mut head);
        let ticket = &head[..0x2A4];
        if ticket[0x1F1] != 0 {
            return Err("This disc uses the Korean common key, which isn't supported yet.".into());
        }
        let mut key = [0u8; 16];
        key.copy_from_slice(&ticket[0x1BF..0x1CF]);
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&ticket[0x1DC..0x1E4]);
        let mut title_id = [0u8; 8];
        title_id.copy_from_slice(&ticket[0x1DC..0x1E4]);
        aes_cbc_decrypt(common_key, &iv, &mut key);
        let data_off = part + ((be32(&head, 0x2B8) as u64) << 2);
        let data_size = (be32(&head, 0x2BC) as u64) << 2;
        let mut p = Partition { raw, data_off, data_size, key, cached: None, title_id };
        // A wrong common key produces garbage instead of a disc header.
        let hdr = p.read(0x18, 4);
        if be32(&hdr, 0) != WII_MAGIC {
            return Err("The Wii common key doesn't decrypt this disc. Check the key in Settings > BIOS files > Wii.".into());
        }
        Ok(p)
    }

    fn cluster(&mut self, c: u64) -> &[u8] {
        if self.cached.as_ref().map(|x| x.0) != Some(c) {
            let mut enc = vec![0u8; CLUSTER as usize];
            self.raw.read_at(self.data_off + c * CLUSTER, &mut enc);
            let mut iv = [0u8; 16];
            iv.copy_from_slice(&enc[0x3D0..0x3E0]);
            let mut data = enc[0x400..].to_vec();
            aes_cbc_decrypt(&self.key, &iv, &mut data);
            self.cached = Some((c, data));
        }
        &self.cached.as_ref().unwrap().1
    }

    /// Reads decrypted partition data (offsets as the game sees them).
    pub fn read(&mut self, off: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let mut done = 0;
        while done < len {
            let pos = off + done as u64;
            if self.data_size > 0 && pos >= self.data_size {
                break;
            }
            let (c, within) = (pos / CLUSTER_DATA, (pos % CLUSTER_DATA) as usize);
            let n = (CLUSTER_DATA as usize - within).min(len - done);
            let data = self.cluster(c);
            out[done..done + n].copy_from_slice(&data[within..within + n]);
            done += n;
        }
        out
    }

    /// Game ID (e.g. "RMGE01") and title from the decrypted header.
    pub fn game_info(&mut self) -> (String, String) {
        let h = self.read(0, 0x60);
        let id = String::from_utf8_lossy(&h[..6]).to_string();
        let name = String::from_utf8_lossy(&h[0x20..0x60]).trim_end_matches('\0').trim().to_string();
        (id, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    type Enc = cbc::Encryptor<aes::Aes128>;

    fn enc(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
        let n = data.len();
        let _ = Enc::new(key.into(), iv.into()).encrypt_padded_mut::<NoPadding>(data, n);
    }

    struct Mem(Vec<u8>);
    impl Raw for Mem {
        fn read_at(&mut self, off: u64, buf: &mut [u8]) {
            buf.fill(0);
            let o = off as usize;
            if o < self.0.len() {
                let n = buf.len().min(self.0.len() - o);
                buf[..n].copy_from_slice(&self.0[o..o + n]);
            }
        }
    }

    /// Builds a tiny encrypted disc with a test common key and checks it decrypts.
    #[test]
    fn decrypts_game_partition() {
        let common = [0x11u8; 16];
        let title_key = [0x22u8; 16];
        let title_id = *b"\x00\x01\x00\x00TEST";
        let mut disc = vec![0u8; 0x60_0000];
        disc[0..6].copy_from_slice(b"RTSTE1");
        disc[0x18..0x1C].copy_from_slice(&WII_MAGIC.to_be_bytes());
        // Partition table: group 0 has one entry at 0x40020.
        disc[0x4_0000..0x4_0004].copy_from_slice(&1u32.to_be_bytes());
        disc[0x4_0004..0x4_0008].copy_from_slice(&(0x4_0020u32 >> 2).to_be_bytes());
        let part = 0x5_0000usize;
        disc[0x4_0020..0x4_0024].copy_from_slice(&((part as u32) >> 2).to_be_bytes());
        // Ticket: encrypted title key and title id.
        let mut ek = title_key;
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&title_id);
        enc(&common, &iv, &mut ek);
        disc[part + 0x1BF..part + 0x1CF].copy_from_slice(&ek);
        disc[part + 0x1DC..part + 0x1E4].copy_from_slice(&title_id);
        let data_off = 0x2_0000usize;
        disc[part + 0x2B8..part + 0x2BC].copy_from_slice(&((data_off as u32) >> 2).to_be_bytes());
        disc[part + 0x2BC..part + 0x2C0].copy_from_slice(&((0x10_0000u32) >> 2).to_be_bytes());
        // Two clusters of plaintext: a game header, then a marker spanning the cluster boundary.
        let mut plain = vec![0u8; 2 * CLUSTER_DATA as usize];
        plain[..6].copy_from_slice(b"RTSTE1");
        plain[0x18..0x1C].copy_from_slice(&WII_MAGIC.to_be_bytes());
        plain[0x20..0x29].copy_from_slice(b"Test Game");
        plain[0x7BFE..0x7C02].copy_from_slice(b"ABCD");
        for c in 0..2 {
            let base = part + data_off + c * CLUSTER as usize;
            let civ = [c as u8 + 5; 16];
            disc[base + 0x3D0..base + 0x3E0].copy_from_slice(&civ);
            let mut d = plain[c * CLUSTER_DATA as usize..(c + 1) * CLUSTER_DATA as usize].to_vec();
            enc(&title_key, &civ, &mut d);
            disc[base + 0x400..base + CLUSTER as usize].copy_from_slice(&d);
        }
        let mut p = Partition::open(Box::new(Mem(disc.clone())), &common).unwrap();
        assert_eq!(p.game_info(), ("RTSTE1".to_string(), "Test Game".to_string()));
        assert_eq!(p.read(0x7BFE, 4), b"ABCD");
        // The wrong key is caught with a clear message.
        assert!(Partition::open(Box::new(Mem(disc)), &[0u8; 16]).is_err());
    }
}
