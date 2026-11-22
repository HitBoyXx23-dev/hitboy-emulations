//! Disk images: floppy / hard disk (.img, raw) and CD-ROM (.iso with El Torito boot).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Floppy,
    Hdd,
    Cdrom,
}

pub struct Disk {
    file: File,
    /// Byte offset of this drive's data inside the file (El Torito floppy emulation).
    base: u64,
    pub kind: Kind,
    pub sector_size: u32,
    pub sectors: u64,
    pub cylinders: u32,
    pub heads: u32,
    pub spt: u32,
    pub writable: bool,
}

fn floppy_geometry(bytes: u64) -> Option<(u32, u32, u32)> {
    Some(match bytes {
        163_840 => (40, 1, 8),
        184_320 => (40, 1, 9),
        327_680 => (40, 2, 8),
        368_640 => (40, 2, 9),
        737_280 => (80, 2, 9),
        1_228_800 => (80, 2, 15),
        1_474_560 => (80, 2, 18),
        1_720_320 => (80, 2, 21),
        2_949_120 => (80, 2, 36),
        _ => return None,
    })
}

fn hdd_geometry(sectors: u64) -> (u32, u32, u32) {
    let (heads, spt) = (16u32, 63u32);
    let cyl = (sectors / (heads as u64 * spt as u64)).clamp(1, 1024) as u32;
    (cyl, heads, spt)
}

fn open(path: &Path) -> std::io::Result<(File, bool)> {
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => Ok((f, true)),
        Err(_) => Ok((File::open(path)?, false)),
    }
}

impl Disk {
    /// A raw disk image: floppy if its size matches a floppy format, otherwise a hard disk.
    pub fn open_image(path: &Path) -> Result<Disk, String> {
        let (file, writable) = open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        let sectors = len / 512;
        if sectors == 0 {
            return Err(format!("{} is empty", path.display()));
        }
        let (kind, (c, h, s)) = match floppy_geometry(len) {
            Some(g) => (Kind::Floppy, g),
            None => (Kind::Hdd, hdd_geometry(sectors)),
        };
        Ok(Disk { file, base: 0, kind, sector_size: 512, sectors, cylinders: c, heads: h, spt: s, writable })
    }

    pub fn read(&mut self, lba: u64, count: u32, buf: &mut [u8]) -> bool {
        let n = count as u64 * self.sector_size as u64;
        if lba + count as u64 > self.sectors || buf.len() < n as usize {
            return false;
        }
        self.file.seek(SeekFrom::Start(self.base + lba * self.sector_size as u64)).is_ok()
            && self.file.read_exact(&mut buf[..n as usize]).is_ok()
    }

    pub fn write(&mut self, lba: u64, count: u32, buf: &[u8]) -> bool {
        let n = count as u64 * self.sector_size as u64;
        if !self.writable || lba + count as u64 > self.sectors || buf.len() < n as usize {
            return false;
        }
        self.file.seek(SeekFrom::Start(self.base + lba * self.sector_size as u64)).is_ok()
            && self.file.write_all(&buf[..n as usize]).is_ok()
    }

    pub fn chs_to_lba(&self, c: u32, h: u32, s: u32) -> Option<u64> {
        if s == 0 || s > self.spt || h >= self.heads {
            return None;
        }
        Some(((c as u64 * self.heads as u64) + h as u64) * self.spt as u64 + (s as u64 - 1))
    }
}

/// How an ISO wants to boot.
pub enum CdBoot {
    /// "No emulation": load `count` 512-byte sectors from CD sector `lba` to `segment:0`.
    NoEmulation { cd: Disk, segment: u16, lba: u32, count: u16 },
    /// Boot image behaves like a floppy (drive 0x00) or hard disk (drive 0x80).
    Emulated { disk: Disk, cd: Disk },
}

/// Parses the El Torito boot catalog of an ISO 9660 image.
pub fn open_iso(path: &Path) -> Result<CdBoot, String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let mut sec = [0u8; 2048];
    let read_sector = |f: &mut File, lba: u64, buf: &mut [u8; 2048]| -> Result<(), String> {
        f.seek(SeekFrom::Start(lba * 2048)).map_err(|e| e.to_string())?;
        f.read_exact(buf).map_err(|_| "ISO is truncated".to_string())
    };
    read_sector(&mut file, 17, &mut sec)?;
    if &sec[1..6] != b"CD001" || &sec[7..30] != b"EL TORITO SPECIFICATION" {
        return Err("This ISO isn't bootable (no El Torito boot record).".into());
    }
    let catalog = u32::from_le_bytes(sec[0x47..0x4B].try_into().unwrap()) as u64;
    read_sector(&mut file, catalog, &mut sec)?;
    let entry = &sec[32..64];
    if entry[0] != 0x88 {
        return Err("The ISO's default boot entry is not bootable.".into());
    }
    let media = entry[1];
    let segment = match u16::from_le_bytes([entry[2], entry[3]]) {
        0 => 0x07C0,
        s => s,
    };
    let count = u16::from_le_bytes([entry[6], entry[7]]);
    let lba = u32::from_le_bytes(entry[8..12].try_into().unwrap());

    let cd = Disk {
        file: File::open(path).map_err(|e| e.to_string())?,
        base: 0,
        kind: Kind::Cdrom,
        sector_size: 2048,
        sectors: len / 2048,
        cylinders: 0,
        heads: 0,
        spt: 0,
        writable: false,
    };
    if media == 0 {
        return Ok(CdBoot::NoEmulation { cd, segment, lba, count: count.max(1) });
    }
    let base = lba as u64 * 2048;
    let (kind, bytes) = match media {
        1 => (Kind::Floppy, 1_228_800u64),
        2 => (Kind::Floppy, 1_474_560),
        3 => (Kind::Floppy, 2_949_120),
        _ => (Kind::Hdd, len.saturating_sub(base)),
    };
    let sectors = bytes / 512;
    let (c, h, s) = if kind == Kind::Floppy { floppy_geometry(bytes).unwrap() } else { hdd_geometry(sectors) };
    let disk = Disk { file, base, kind, sector_size: 512, sectors, cylinders: c, heads: h, spt: s, writable: false };
    Ok(CdBoot::Emulated { disk, cd })
}
