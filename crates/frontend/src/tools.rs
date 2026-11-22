//! Disc image conversion (BIN/CUE <-> ISO, merging multi-file CUE sheets) and finding a
//! PlayStation BIOS the user already has on this computer.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const RAW: usize = 2352;

// ---------------------------------------------------------------- CD sector error correction

/// EDC: CRC-32 with the CD-ROM polynomial (reversed 0xD8018001).
fn edc(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xD801_8001 } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    data.iter().fold(0u32, |c, &b| (c >> 8) ^ t[((c ^ b as u32) & 0xFF) as usize])
}

/// Reed-Solomon P and Q parity over the sector's header+data (bytes 12..2076).
fn ecc(sector: &mut [u8; RAW]) {
    static LUT: std::sync::OnceLock<([u8; 256], [u8; 256])> = std::sync::OnceLock::new();
    let (f, b) = LUT.get_or_init(|| {
        let mut f = [0u8; 256];
        let mut b = [0u8; 256];
        for i in 0..256u32 {
            let j = ((i << 1) ^ if i & 0x80 != 0 { 0x11D } else { 0 }) as u8;
            f[i as usize] = j;
            b[(i as u8 ^ j) as usize] = i as u8;
        }
        (f, b)
    });
    // (major count, minor count, major mult, minor inc, source offset from 12, dest offset)
    let mut pass = |major_count: usize, minor_count: usize, major_mult: usize, minor_inc: usize, dest: usize| {
        let size = major_count * minor_count;
        for major in 0..major_count {
            let mut index = (major >> 1) * major_mult + (major & 1);
            let (mut ea, mut eb) = (0u8, 0u8);
            for _ in 0..minor_count {
                let temp = sector[12 + index];
                index += minor_inc;
                if index >= size {
                    index -= size;
                }
                ea ^= temp;
                eb ^= temp;
                ea = f[ea as usize];
            }
            ea = b[(f[ea as usize] ^ eb) as usize];
            sector[dest + major] = ea;
            sector[dest + major + major_count] = ea ^ eb;
        }
    };
    pass(86, 24, 2, 86, 0x81C); // P
    pass(52, 43, 86, 88, 0x8C8); // Q
}

fn bcd(v: u32) -> u8 {
    ((v / 10) << 4 | v % 10) as u8
}

/// A complete Mode 2 Form 1 sector (as PlayStation discs use) around 2048 bytes of data.
fn mode2_form1(lba: u32, data: &[u8]) -> [u8; RAW] {
    let mut s = [0u8; RAW];
    s[1..11].fill(0xFF);
    let a = lba + 150;
    s[12] = bcd(a / 75 / 60);
    s[13] = bcd(a / 75 % 60);
    s[14] = bcd(a % 75);
    s[15] = 2;
    s[18] = 8; // subheader: data
    s[22] = 8;
    s[24..24 + 2048].copy_from_slice(data);
    let e = edc(&s[16..2072]);
    s[2072..2076].copy_from_slice(&e.to_le_bytes());
    // Mode 2 ECC is computed with the header treated as zero.
    let header: [u8; 4] = s[12..16].try_into().unwrap();
    s[12..16].fill(0);
    ecc(&mut s);
    s[12..16].copy_from_slice(&header);
    s
}

// ---------------------------------------------------------------- CUE sheets

struct CueFile {
    path: PathBuf,
    tracks: Vec<(u32, String, Vec<(u32, u32)>)>, // (number, type, [(index, frame)])
}

fn parse_cue(cue: &Path) -> Result<Vec<CueFile>, String> {
    let text = std::fs::read_to_string(cue).map_err(|e| format!("Can't read the cue sheet: {e}"))?;
    let dir = cue.parent().unwrap_or(Path::new("."));
    let mut files: Vec<CueFile> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        let up = l.to_ascii_uppercase();
        if up.starts_with("FILE") {
            let name = match (l.find('"'), l.rfind('"')) {
                (Some(a), Some(b)) if b > a => l[a + 1..b].to_string(),
                _ => l.split_whitespace().nth(1).unwrap_or("").to_string(),
            };
            files.push(CueFile { path: dir.join(name), tracks: Vec::new() });
        } else if up.starts_with("TRACK") {
            let mut it = up.split_whitespace().skip(1);
            let n = it.next().and_then(|v| v.parse().ok()).unwrap_or(1);
            let ty = it.next().unwrap_or("MODE2/2352").to_string();
            if let Some(f) = files.last_mut() {
                f.tracks.push((n, ty, Vec::new()));
            }
        } else if up.starts_with("INDEX") {
            let mut it = up.split_whitespace().skip(1);
            let idx = it.next().and_then(|v| v.parse().ok()).unwrap_or(1);
            let msf: Vec<u32> = it.next().unwrap_or("0:0:0").split(':').filter_map(|p| p.parse().ok()).collect();
            if msf.len() == 3 {
                if let Some(t) = files.last_mut().and_then(|f| f.tracks.last_mut()) {
                    t.2.push((idx, (msf[0] * 60 + msf[1]) * 75 + msf[2]));
                }
            }
        }
    }
    if files.is_empty() {
        return Err("The cue sheet doesn't list any files.".into());
    }
    Ok(files)
}

fn msf(frames: u32) -> String {
    format!("{:02}:{:02}:{:02}", frames / 75 / 60, frames / 75 % 60, frames % 75)
}

// ---------------------------------------------------------------- conversions

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conversion {
    /// Data track of a BIN/CUE to a 2048-byte-per-sector ISO.
    CueToIso,
    /// Several BIN files listed by one CUE into a single BIN + CUE.
    MergeCue,
    /// ISO to a single-track Mode 2 BIN/CUE with proper sync, headers, EDC and ECC.
    IsoToCue,
    /// A .zip/.7z holding a BIN/CUE (or an ISO) to one ISO saved next to the archive.
    ArchiveToIso,
}

impl Conversion {
    pub fn label(self) -> &'static str {
        match self {
            Conversion::CueToIso => "BIN/CUE → ISO (data track only)",
            Conversion::MergeCue => "Multi-file BIN/CUE → single BIN/CUE",
            Conversion::IsoToCue => "ISO → BIN/CUE",
            Conversion::ArchiveToIso => "ZIP/7Z (BIN/CUE inside) → ISO",
        }
    }

    /// Conversions that make sense for this input file.
    pub fn for_input(path: &Path) -> Vec<Conversion> {
        match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
            Some("cue") => vec![Conversion::CueToIso, Conversion::MergeCue],
            Some("iso") => vec![Conversion::IsoToCue],
            Some("zip") | Some("7z") | Some("rar") => vec![Conversion::ArchiveToIso],
            _ => Vec::new(),
        }
    }
}

/// Runs a conversion, reporting progress (0-1000). Returns the main output file.
pub fn convert(input: &Path, kind: Conversion, progress: &AtomicU32) -> Result<PathBuf, String> {
    match kind {
        Conversion::CueToIso => cue_to_iso(input, progress),
        Conversion::MergeCue => merge_cue(input, progress),
        Conversion::IsoToCue => iso_to_cue(input, progress),
        Conversion::ArchiveToIso => archive_to_iso(input, progress),
    }
}

/// Unpacks the archive to a temp folder, converts its CUE (or takes its ISO) and
/// writes "<archive name>.iso" beside the archive.
fn archive_to_iso(archive: &Path, progress: &AtomicU32) -> Result<PathBuf, String> {
    let tmp = std::env::temp_dir().join(format!("emu-convert-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    extract(archive, &tmp)?;
    let r = (|| {
        let mut files = Vec::new();
        let mut stack = vec![tmp.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).map_err(|e| e.to_string())?.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    files.push(p);
                }
            }
        }
        let has = |ext: &str| files.iter().find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(ext))).cloned();
        let made = if let Some(cue) = has("cue") {
            cue_to_iso(&cue, progress)?
        } else if let Some(iso) = has("iso").or_else(|| has("gcm")) {
            iso
        } else {
            return Err("The archive has no .cue or .iso disc image inside.".to_string());
        };
        let dest = out_path(archive, "", "iso");
        if std::fs::rename(&made, &dest).is_err() {
            std::fs::copy(&made, &dest).map_err(|e| format!("Couldn't save the ISO: {e}"))?;
        }
        Ok(dest)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

fn out_path(input: &Path, suffix: &str, ext: &str) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let mut p = input.with_file_name(format!("{stem}{suffix}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = input.with_file_name(format!("{stem}{suffix} ({n}).{ext}"));
        n += 1;
    }
    p
}

fn cue_to_iso(cue: &Path, progress: &AtomicU32) -> Result<PathBuf, String> {
    let files = parse_cue(cue)?;
    let first = &files[0];
    let ty = first.tracks.first().map(|t| t.1.clone()).unwrap_or_default();
    if ty == "AUDIO" {
        return Err("The first track is audio; there's no data track to convert.".into());
    }
    let (sector, offset) = if ty.contains("2048") {
        (2048usize, 0usize)
    } else if ty.starts_with("MODE1") {
        (RAW, 16)
    } else {
        (RAW, 24)
    };
    let mut src = File::open(&first.path).map_err(|e| format!("Can't open {}: {e}", first.path.display()))?;
    let total = src.metadata().map(|m| m.len()).unwrap_or(0);
    // The data track ends where the next track in the same file begins.
    let end_frame = first.tracks.get(1).and_then(|t| t.2.iter().find(|i| i.0 <= 1).map(|i| i.1)).map(|f| f as u64 * sector as u64).unwrap_or(total);
    let out = out_path(cue, "", "iso");
    let mut dst = BufWriter::new(File::create(&out).map_err(|e| e.to_string())?);
    let mut buf = vec![0u8; sector];
    let mut done = 0u64;
    while done + sector as u64 <= end_frame {
        src.read_exact(&mut buf).map_err(|e| e.to_string())?;
        dst.write_all(&buf[offset..offset + 2048]).map_err(|e| e.to_string())?;
        done += sector as u64;
        progress.store((done * 1000 / end_frame.max(1)) as u32, Ordering::Relaxed);
    }
    dst.flush().map_err(|e| e.to_string())?;
    Ok(out)
}

fn merge_cue(cue: &Path, progress: &AtomicU32) -> Result<PathBuf, String> {
    let files = parse_cue(cue)?;
    let out_bin = out_path(cue, " (merged)", "bin");
    let out_cue = out_bin.with_extension("cue");
    let bin_name = out_bin.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let total: u64 = files.iter().map(|f| std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0)).sum();
    let mut dst = BufWriter::new(File::create(&out_bin).map_err(|e| e.to_string())?);
    let mut sheet = format!("FILE \"{bin_name}\" BINARY\n");
    let mut base_frames = 0u32;
    let mut written = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    for f in &files {
        for (n, ty, idx) in &f.tracks {
            sheet.push_str(&format!("  TRACK {n:02} {ty}\n"));
            for (i, fr) in idx {
                sheet.push_str(&format!("    INDEX {i:02} {}\n", msf(base_frames + fr)));
            }
        }
        let mut src = File::open(&f.path).map_err(|e| format!("Can't open {}: {e}", f.path.display()))?;
        let len = src.metadata().map(|m| m.len()).unwrap_or(0);
        loop {
            let n = src.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            dst.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            written += n as u64;
            progress.store((written * 1000 / total.max(1)) as u32, Ordering::Relaxed);
        }
        base_frames += (len / RAW as u64) as u32;
    }
    dst.flush().map_err(|e| e.to_string())?;
    std::fs::write(&out_cue, sheet).map_err(|e| e.to_string())?;
    Ok(out_cue)
}

fn iso_to_cue(iso: &Path, progress: &AtomicU32) -> Result<PathBuf, String> {
    let mut src = File::open(iso).map_err(|e| e.to_string())?;
    let len = src.metadata().map(|m| m.len()).unwrap_or(0);
    if len % 2048 != 0 {
        return Err("This ISO's size isn't a whole number of 2048-byte sectors.".into());
    }
    let out_bin = out_path(iso, "", "bin");
    let out_cue = out_bin.with_extension("cue");
    let mut dst = BufWriter::new(File::create(&out_bin).map_err(|e| e.to_string())?);
    let sectors = (len / 2048) as u32;
    let mut data = vec![0u8; 2048];
    src.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    for lba in 0..sectors {
        src.read_exact(&mut data).map_err(|e| e.to_string())?;
        dst.write_all(&mode2_form1(lba, &data)).map_err(|e| e.to_string())?;
        if lba % 256 == 0 {
            progress.store((lba as u64 * 1000 / sectors.max(1) as u64) as u32, Ordering::Relaxed);
        }
    }
    dst.flush().map_err(|e| e.to_string())?;
    let name = out_bin.file_name().unwrap_or_default().to_string_lossy();
    std::fs::write(&out_cue, format!("FILE \"{name}\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n")).map_err(|e| e.to_string())?;
    Ok(out_cue)
}

/// A conversion running on a background thread.
pub struct Job {
    pub input: PathBuf,
    pub progress: Arc<AtomicU32>,
    handle: Option<std::thread::JoinHandle<Result<PathBuf, String>>>,
}

impl Job {
    pub fn start(input: PathBuf, kind: Conversion) -> Job {
        let progress = Arc::new(AtomicU32::new(0));
        let (p, i) = (progress.clone(), input.clone());
        let handle = std::thread::spawn(move || convert(&i, kind, &p));
        Job { input, progress, handle: Some(handle) }
    }

    /// Some(result) once finished.
    pub fn poll(&mut self) -> Option<Result<PathBuf, String>> {
        if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            return Some(self.handle.take().unwrap().join().unwrap_or_else(|_| Err("The conversion crashed.".into())));
        }
        None
    }
}

// ---------------------------------------------------------------- PlayStation BIOS finder

/// A PlayStation BIOS image found on disk.
pub struct FoundBios {
    pub path: PathBuf,
    pub region: &'static str,
}

/// Recognises a PlayStation BIOS: 512 KB with Sony's copyright string in the header.
pub fn is_ps1_bios(path: &Path) -> Option<&'static str> {
    if std::fs::metadata(path).ok()?.len() != 512 * 1024 {
        return None;
    }
    let mut head = vec![0u8; 0x200];
    File::open(path).ok()?.read_exact(&mut head).ok()?;
    let text = String::from_utf8_lossy(&head);
    if !text.contains("Sony Computer Entertainment") {
        return None;
    }
    // Region from the usual dump names (SCPH model numbers).
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    Some(if ["5502", "5552", "7502", "9002", "1002", "102"].iter().any(|m| name.contains(m)) {
        "Europe"
    } else if ["5500", "7000", "3000", "1000", "3500"].iter().any(|m| name.contains(m)) {
        "Japan"
    } else if ["1001", "5501", "7001", "7003", "101"].iter().any(|m| name.contains(m)) {
        "USA"
    } else {
        "Region unknown"
    })
}

/// Looks for PlayStation BIOS files in the Downloads folder, where a downloaded dump most likely is.
pub fn find_ps1_bios() -> Vec<FoundBios> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default();
    let roots = vec![home.join("Downloads")];
    let mut found = Vec::new();
    let mut visited = 0usize;
    for root in roots {
        let mut stack = vec![(root, 0u32)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                visited += 1;
                if visited > 200_000 {
                    return found;
                }
                let p = e.path();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    if depth < 6 {
                        stack.push((p, depth + 1));
                    }
                } else if ft.is_file() {
                    if let Some(region) = is_ps1_bios(&p) {
                        if !found.iter().any(|f: &FoundBios| f.path == p) {
                            found.push(FoundBios { path: p, region });
                        }
                    }
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_to_bin_and_back_round_trips() {
        let dir = std::env::temp_dir().join(format!("emu-convert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let iso = dir.join("game.iso");
        let data: Vec<u8> = (0..2048 * 20).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(&iso, &data).unwrap();
        let p = AtomicU32::new(0);
        let cue = convert(&iso, Conversion::IsoToCue, &p).unwrap();
        let bin = cue.with_extension("bin");
        assert_eq!(std::fs::metadata(&bin).unwrap().len(), 20 * RAW as u64);
        // Rename the round-tripped ISO so it doesn't collide with the source.
        let iso2 = convert(&cue, Conversion::CueToIso, &p).unwrap();
        assert_eq!(std::fs::read(&iso2).unwrap(), data);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sector_edc_matches_known_value() {
        // An all-zero Mode 2 Form 1 sector at LBA 0 has a fixed EDC.
        let s = mode2_form1(0, &[0u8; 2048]);
        let e = u32::from_le_bytes(s[2072..2076].try_into().unwrap());
        assert_eq!(e, edc(&s[16..2072]));
        assert_ne!(&s[0x81C..0x81C + 16], &[0u8; 16], "P parity is filled in");
    }
}

// ---------------------------------------------------------------- wrapped disc images

/// Files in the root directory of a 2048-byte-sector ISO 9660 image: (name, first sector, size).
fn iso_root_files(f: &mut std::fs::File) -> Option<Vec<(String, u64, u64)>> {
    use std::io::{Read, Seek, SeekFrom};
    let sec = |f: &mut std::fs::File, n: u64, len: usize| -> Option<Vec<u8>> {
        let mut b = vec![0u8; len];
        f.seek(SeekFrom::Start(n * 2048)).ok()?;
        f.read_exact(&mut b).ok()?;
        Some(b)
    };
    let pvd = sec(f, 16, 2048)?;
    if &pvd[1..6] != b"CD001" {
        return None;
    }
    let root = &pvd[156..190];
    let lba = u32::from_le_bytes(root[2..6].try_into().ok()?) as u64;
    let size = u32::from_le_bytes(root[10..14].try_into().ok()?) as usize;
    let dir = sec(f, lba, size.min(1 << 20).div_ceil(2048) * 2048)?;
    let mut out = Vec::new();
    let mut o = 0;
    while o < dir.len() {
        let len = dir[o] as usize;
        if len == 0 {
            o = (o / 2048 + 1) * 2048;
            continue;
        }
        let e = &dir[o..(o + len).min(dir.len())];
        if e.len() >= 34 && e[25] & 2 == 0 {
            let n = &e[33..(33 + e[32] as usize).min(e.len())];
            let name = String::from_utf8_lossy(n).split(';').next().unwrap_or("").to_string();
            let (l, s) = (u32::from_le_bytes(e[2..6].try_into().ok()?) as u64, u32::from_le_bytes(e[10..14].try_into().ok()?) as u64);
            if !name.is_empty() {
                out.push((name, l, s));
            }
        }
        o += len;
    }
    Some(out)
}

/// Some downloads are a data ISO that merely holds a game's .cue and .bin tracks. Those are
/// unpacked once into the app's cache; returns the .cue inside, or None for a normal disc.
pub fn unwrap_iso_container(path: &Path) -> Result<Option<PathBuf>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let Some(files) = iso_root_files(&mut f) else { return Ok(None) };
    let lower = |n: &str| n.to_ascii_lowercase();
    if files.iter().any(|(n, _, _)| lower(n) == "system.cnf") || !files.iter().any(|(n, _, _)| lower(n).ends_with(".cue")) {
        return Ok(None);
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let dir = crate::settings::config_dir().join("unpacked").join(stem);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut cue = None;
    for (name, lba, size) in &files {
        let dest = dir.join(name);
        if lower(name).ends_with(".cue") {
            cue = Some(dest.clone());
        }
        if std::fs::metadata(&dest).is_ok_and(|m| m.len() == *size) {
            continue;
        }
        f.seek(SeekFrom::Start(lba * 2048)).map_err(|e| e.to_string())?;
        let mut out = std::fs::File::create(&dest).map_err(|e| format!("Couldn't unpack {name}: {e}"))?;
        std::io::copy(&mut (&mut f).take(*size), &mut out).map_err(|e| format!("Couldn't unpack {name}: {e}"))?;
    }
    Ok(cue)
}

/// Unpacks a .zip, .7z or .rar archive.
pub fn extract(archive: &Path, dest: &Path) -> Result<(), String> {
    crate::archive::extract_all(archive, dest)
}
