//! Game archives: .zip, .7z and .rar. Lists what's inside, pulls a game out, and works out
//! which console an archive belongs to (so a Game Gear game in a zip isn't shown as NES).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

pub const EXTENSIONS: &[&str] = &["zip", "7z", "rar"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Zip,
    SevenZ,
    Rar,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub size: u64,
}

pub fn is_archive_ext(ext: &str) -> bool {
    EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

pub fn is_archive(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(is_archive_ext)
}

/// What an archive file is, from its first bytes (file names can lie).
pub fn kind_of(path: &Path) -> Option<Kind> {
    let mut head = [0u8; 8];
    std::fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    if head.starts_with(b"PK") {
        Some(Kind::Zip)
    } else if head.starts_with(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
        Some(Kind::SevenZ)
    } else if head.starts_with(b"Rar!") {
        Some(Kind::Rar)
    } else {
        None
    }
}

/// Files inside an archive (directories left out).
pub fn list(path: &Path) -> Result<Vec<Entry>, String> {
    match kind_of(path).ok_or("This isn't a zip, 7z or rar archive.")? {
        Kind::Zip => {
            let mut z = zip::ZipArchive::new(std::fs::File::open(path).map_err(|e| e.to_string())?).map_err(|e| format!("Bad zip: {e}"))?;
            let mut out = Vec::new();
            for i in 0..z.len() {
                if let Ok(f) = z.by_index(i) {
                    if f.is_file() {
                        out.push(Entry { name: f.name().to_string(), size: f.size() });
                    }
                }
            }
            Ok(out)
        }
        Kind::SevenZ => {
            let r = sevenz_rust::SevenZReader::open(path, sevenz_rust::Password::empty()).map_err(|e| format!("Bad 7z: {e}"))?;
            Ok(r.archive().files.iter().filter(|f| !f.is_directory() && f.has_stream()).map(|f| Entry { name: f.name().to_string(), size: f.size() }).collect())
        }
        Kind::Rar => {
            let data = std::fs::read(path).map_err(|e| e.to_string())?;
            let a = rars::ArchiveReader::read(&data).map_err(|e| format!("Bad rar: {e}"))?;
            Ok(a.members().filter(|m| !m.meta.is_directory).map(|m| Entry { name: m.meta.name_lossy(), size: m.meta.unpacked_size }).collect())
        }
    }
}

/// Reads one file out of an archive.
pub fn read(path: &Path, name: &str) -> Result<Vec<u8>, String> {
    match kind_of(path).ok_or("This isn't a zip, 7z or rar archive.")? {
        Kind::Zip => {
            let mut z = zip::ZipArchive::new(std::fs::File::open(path).map_err(|e| e.to_string())?).map_err(|e| format!("Bad zip: {e}"))?;
            let mut f = z.by_name(name).map_err(|e| e.to_string())?;
            let mut v = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut v).map_err(|e| e.to_string())?;
            Ok(v)
        }
        Kind::SevenZ => {
            let mut r = sevenz_rust::SevenZReader::open(path, sevenz_rust::Password::empty()).map_err(|e| format!("Bad 7z: {e}"))?;
            let mut out = None;
            r.for_each_entries(|e, rd| {
                if e.name() == name {
                    let mut v = Vec::new();
                    rd.read_to_end(&mut v)?;
                    out = Some(v);
                    return Ok(false);
                }
                // Skip over the data of other entries.
                std::io::copy(rd, &mut std::io::sink())?;
                Ok(true)
            })
            .map_err(|e| format!("7z extraction failed: {e}"))?;
            out.ok_or_else(|| format!("{name} isn't in this archive."))
        }
        Kind::Rar => {
            let data = std::fs::read(path).map_err(|e| e.to_string())?;
            let a = rars::ArchiveReader::read(&data).map_err(|e| format!("Bad rar: {e}"))?;
            a.read_member(name.as_bytes(), None)
                .map_err(|e| format!("rar extraction failed (encrypted or damaged?): {e}"))?
                .ok_or_else(|| format!("{name} isn't in this archive."))
        }
    }
}

/// The console one game file belongs to, from its name; `.bin` is settled by its contents.
fn console_of_file(name: &str, size: u64, peek: impl FnOnce() -> Option<Vec<u8>>) -> Option<&'static str> {
    let c = crate::cores::console_for(name)?;
    if c == "ps1" && name.to_ascii_lowercase().ends_with(".bin") {
        if size <= 65536 {
            return Some("a2600");
        }
        if size < 16 << 20 {
            if let Some(b) = peek() {
                if md::is_genesis_rom(&b) {
                    return Some("md");
                }
            }
        }
    }
    Some(c)
}

type Cache = HashMap<PathBuf, (u64, Option<SystemTime>, Option<&'static str>)>;
static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// Which console an archive's game is for (None if it holds nothing we run).
pub fn console_of(path: &Path) -> Option<&'static str> {
    let meta = std::fs::metadata(path).ok()?;
    let key = (meta.len(), meta.modified().ok());
    if let Some((len, mt, c)) = CACHE.lock().ok()?.as_ref().and_then(|m| m.get(path)).copied() {
        if (len, mt) == key {
            return c;
        }
    }
    let result = (|| {
        let entries = list(path).ok()?;
        entries.iter().find_map(|e| console_of_file(&e.name, e.size, || read(path, &e.name).ok()))
    })();
    if let Ok(mut g) = CACHE.lock() {
        g.get_or_insert_with(HashMap::new).insert(path.to_path_buf(), (key.0, key.1, result));
    }
    result
}

/// The first game file in an archive, extracted: (bytes, inner file name).
pub fn first_game(path: &Path) -> Result<(Vec<u8>, String), String> {
    let entries = list(path)?;
    let e = entries
        .iter()
        .find(|e| crate::cores::console_for(&e.name).is_some())
        .ok_or("This archive doesn't contain a game we can run (.nes, .sfc, .gb, .gba, .gg, .sms, .md, .pce, .ws, ...).")?;
    Ok((read(path, &e.name)?, e.name.clone()))
}

/// Unpacks everything (used for disc images with several files).
pub fn extract_all(path: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    match kind_of(path).ok_or("Not an archive.")? {
        Kind::Zip => {
            let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
            zip::ZipArchive::new(f).map_err(|e| format!("Bad zip: {e}"))?.extract(dest).map_err(|e| format!("Unzip failed: {e}"))
        }
        Kind::SevenZ => sevenz_rust::decompress_file(path, dest).map_err(|e| format!("7z extraction failed: {e}")),
        Kind::Rar => {
            for e in list(path)? {
                // Guard against names that climb out of the destination.
                let rel: PathBuf = Path::new(&e.name).components().filter(|c| matches!(c, std::path::Component::Normal(_))).collect();
                let out = dest.join(rel);
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                std::fs::write(out, read(path, &e.name)?).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("archive_test_{}_{name}", std::process::id()))
    }

    #[test]
    fn zip_with_a_game_gear_game_is_not_nes() {
        let p = temp("gg.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
            let o: zip::write::SimpleFileOptions = Default::default();
            z.start_file("readme.txt", o).unwrap();
            z.write_all(b"hi").unwrap();
            z.start_file("Shaq Fu (USA).gg", o).unwrap();
            z.write_all(&[0u8; 32768]).unwrap();
            z.finish().unwrap();
        }
        assert_eq!(kind_of(&p), Some(Kind::Zip));
        assert_eq!(console_of(&p), Some("gg"));
        let (bytes, name) = first_game(&p).unwrap();
        assert_eq!((bytes.len(), name.as_str()), (32768, "Shaq Fu (USA).gg"));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn zip_without_a_game_is_ignored() {
        let p = temp("doc.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
            z.start_file("notes.txt", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(b"x").unwrap();
            z.finish().unwrap();
        }
        assert_eq!(console_of(&p), None);
        assert!(first_game(&p).is_err());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn sevenz_round_trip() {
        let dir = temp("src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Sonic.md"), vec![7u8; 4096]).unwrap();
        let p = temp("sonic.7z");
        sevenz_rust::compress_to_path(&dir, &p).unwrap();
        assert_eq!(kind_of(&p), Some(Kind::SevenZ));
        let entries = list(&p).unwrap();
        assert!(entries.iter().any(|e| e.name.ends_with("Sonic.md")));
        assert_eq!(console_of(&p), Some("md"));
        let (bytes, _) = first_game(&p).unwrap();
        assert_eq!(bytes.len(), 4096);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn rar_round_trip_new_and_old_formats() {
        // A compressible fake cartridge, so the compressed path is really exercised.
        let game: Vec<u8> = (0..20000u32).map(|i| (i / 40) as u8).collect();
        for (v, tag) in [(rars::ArchiveVersion::Rar50, "50"), (rars::ArchiveVersion::Rar29, "29")] {
            let mut b = rars::builder::Builder::new(v);
            b.add_bytes(b"Contra (USA).nes".to_vec(), game.clone(), None, None).unwrap();
            let p = temp(&format!("c{tag}.rar"));
            std::fs::write(&p, b.to_bytes().unwrap()).unwrap();
            assert_eq!(kind_of(&p), Some(Kind::Rar));
            assert_eq!(console_of(&p), Some("nes"), "rar{tag}");
            let (bytes, name) = first_game(&p).unwrap();
            assert_eq!((bytes == game, name.as_str()), (true, "Contra (USA).nes"), "rar{tag}");
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn rar_is_detected_by_signature() {
        let p = temp("x.rar");
        std::fs::write(&p, b"Rar!\x1a\x07\x01\x00junk").unwrap();
        assert_eq!(kind_of(&p), Some(Kind::Rar));
        assert!(list(&p).is_err()); // truncated archive: reported, not a panic
        let _ = std::fs::remove_file(p);
    }
}
