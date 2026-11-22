//! Disc images: .cue (single or multi-file), raw .bin (2352-byte sectors) and .iso (2048).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const SECTOR: usize = 2352;

struct Part {
    file: File,
    /// Disc LBA of the file's first sector.
    base: u32,
    sectors: u32,
    size: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Track {
    pub number: u8,
    pub audio: bool,
    /// LBA of INDEX 01.
    pub start: u32,
}

pub struct Disc {
    parts: Vec<Part>,
    pub tracks: Vec<Track>,
    pub total: u32,
}

impl Disc {
    pub fn open(path: &Path) -> Result<Disc, String> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "cue" => Self::from_cue(path),
            "iso" => Self::single(path, 2048),
            _ => {
                // .bin/.img: raw sectors; fall back to 2048 when the size doesn't fit.
                let len = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
                let size = if len % 2352 == 0 { 2352 } else { 2048 };
                Self::single(path, size)
            }
        }
    }

    fn single(path: &Path, size: u32) -> Result<Disc, String> {
        let file = File::open(path).map_err(|e| format!("Can't open {}: {e}", path.display()))?;
        let sectors = (file.metadata().map_err(|e| e.to_string())?.len() / size as u64) as u32;
        // A data-only image (a bare .iso or .bin) has no CD audio tracks, but the original
        // discs of many games do, and some programs divide by the track count to pick music at
        // random. So the drive reports silent audio tracks after the data, as a full disc
        // would; playing one gives silence.
        const FAKE_TRACKS: u8 = 20;
        const FAKE_LENGTH: u32 = 3 * 60 * 75;
        let mut tracks = vec![Track { number: 1, audio: false, start: 0 }];
        let mut start = sectors + 150;
        for n in 2..2 + FAKE_TRACKS {
            tracks.push(Track { number: n, audio: true, start });
            start += FAKE_LENGTH;
        }
        Ok(Disc { parts: vec![Part { file, base: 0, sectors, size }], tracks, total: start })
    }

    fn from_cue(path: &Path) -> Result<Disc, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("Can't read cue sheet: {e}"))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut parts: Vec<Part> = Vec::new();
        let mut tracks: Vec<Track> = Vec::new();
        let mut base = 0u32;
        let mut cur_audio = false;
        let mut cur_num = 0u8;
        for line in text.lines() {
            let l = line.trim();
            let up = l.to_ascii_uppercase();
            if up.starts_with("FILE") {
                let name = match (l.find('"'), l.rfind('"')) {
                    (Some(a), Some(b)) if b > a => l[a + 1..b].to_string(),
                    _ => l.split_whitespace().nth(1).unwrap_or("").to_string(),
                };
                let mut fp: PathBuf = dir.join(&name);
                if !fp.exists() {
                    // Cue sheets often differ in letter case from the files (matters on Mac/Linux).
                    if let Some(e) = std::fs::read_dir(dir).ok().and_then(|rd| rd.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(&name))) {
                        fp = e.path();
                    }
                }
                let file = File::open(&fp).map_err(|e| format!("Can't open {}: {e}", fp.display()))?;
                if let Some(p) = parts.last() {
                    base = p.base + p.sectors;
                }
                let sectors = (file.metadata().map_err(|e| e.to_string())?.len() / SECTOR as u64) as u32;
                parts.push(Part { file, base, sectors, size: SECTOR as u32 });
            } else if up.starts_with("TRACK") {
                let mut it = up.split_whitespace().skip(1);
                cur_num = it.next().and_then(|n| n.parse().ok()).unwrap_or(cur_num + 1);
                cur_audio = it.next().map(|t| t == "AUDIO").unwrap_or(false);
            } else if up.starts_with("INDEX") {
                let mut it = up.split_whitespace().skip(1);
                if it.next().and_then(|n| n.parse::<u32>().ok()) == Some(1) {
                    let msf: Vec<u32> = it.next().unwrap_or("0:0:0").split(':').filter_map(|p| p.parse().ok()).collect();
                    if msf.len() == 3 {
                        let off = (msf[0] * 60 + msf[1]) * 75 + msf[2];
                        let b = parts.last().map(|p| p.base).unwrap_or(0);
                        tracks.push(Track { number: cur_num, audio: cur_audio, start: b + off });
                    }
                }
            }
        }
        if parts.is_empty() || tracks.is_empty() {
            return Err("The cue sheet doesn't list any tracks.".into());
        }
        let total = parts.last().map(|p| p.base + p.sectors).unwrap_or(0);
        Ok(Disc { parts, tracks, total })
    }

    /// Reads one raw 2352-byte sector at disc LBA (0 = 00:02:00).
    pub fn read(&mut self, lba: u32, out: &mut [u8; SECTOR]) -> bool {
        let Some(p) = self.parts.iter_mut().find(|p| lba >= p.base && lba < p.base + p.sectors) else {
            out.fill(0);
            return false;
        };
        let rel = (lba - p.base) as u64;
        if p.size == SECTOR as u32 {
            let ok = p.file.seek(SeekFrom::Start(rel * SECTOR as u64)).is_ok() && p.file.read_exact(out).is_ok();
            return ok;
        }
        // 2048-byte sectors: synthesise a Mode 2 Form 1 raw sector around the user data.
        out.fill(0);
        out[1..11].fill(0xFF);
        let (m, s, f) = lba_to_msf(lba + 150);
        out[12] = bcd(m);
        out[13] = bcd(s);
        out[14] = bcd(f);
        out[15] = 2;
        out[18] = 8;
        out[22] = 8;
        p.file.seek(SeekFrom::Start(rel * 2048)).is_ok() && p.file.read_exact(&mut out[24..24 + 2048]).is_ok()
    }

    pub fn track_of(&self, lba: u32) -> Track {
        *self.tracks.iter().rev().find(|t| t.start <= lba).unwrap_or(&self.tracks[0])
    }

    /// Region letter from the licence text in sector 4 ("SCEA"/"SCEE"/"SCEI").
    pub fn region(&mut self) -> &'static [u8; 4] {
        let mut s = [0u8; SECTOR];
        self.read(4, &mut s);
        let text = String::from_utf8_lossy(&s[24..24 + 2048]).to_string();
        if text.contains("Euro") {
            b"SCEE"
        } else if text.contains("Amer") {
            b"SCEA"
        } else if text.contains("Inc.") {
            b"SCEI"
        } else {
            b"SCEA"
        }
    }
}

pub fn lba_to_msf(l: u32) -> (u32, u32, u32) {
    (l / 75 / 60, l / 75 % 60, l % 75)
}

pub fn bcd(v: u32) -> u8 {
    ((v / 10) << 4 | v % 10) as u8
}

pub fn from_bcd(v: u8) -> u32 {
    (v >> 4) as u32 * 10 + (v & 0xF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens a real multi-track disc when DISC_TEST_CUE points at one.
    #[test]
    fn real_cue_disc_layout() {
        let Ok(cue) = std::env::var("DISC_TEST_CUE") else { return };
        let mut d = Disc::open(Path::new(&cue)).unwrap();
        assert!(d.tracks.len() > 1);
        assert!(!d.tracks[0].audio && d.tracks[1].audio);
        let mut s = [0u8; SECTOR];
        assert!(d.read(16, &mut s));
        assert_eq!(&s[24 + 1..24 + 6], b"CD001", "ISO volume descriptor in the data track");
        let region = d.region();
        println!("tracks {:?} total {} region {}", d.tracks.iter().map(|t| (t.number, t.audio, t.start)).collect::<Vec<_>>(), d.total, String::from_utf8_lossy(region));
    }
}

#[cfg(test)]
mod track_tests {
    use super::*;

    #[test]
    fn a_data_only_image_reports_silent_audio_tracks_after_the_data() {
        let dir = std::env::temp_dir().join(format!("psx_disc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("game.iso");
        std::fs::write(&p, vec![0u8; 2048 * 100]).unwrap();
        let d = Disc::open(&p).unwrap();
        assert!(!d.tracks[0].audio);
        assert!(d.tracks.len() > 2, "games that pick a random music track need a track count above 1");
        assert!(d.tracks[1..].iter().all(|t| t.audio && t.start >= 100));
        assert!(d.total > d.tracks.last().unwrap().start, "the lead-out comes after the last track");
    }
}
