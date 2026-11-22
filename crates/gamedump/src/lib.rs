//! Game Dump Helper.
//!
//! This only ever works with games you own, from discs and consoles you own. It does not
//! contain keys, exploits or ways around a console's protection. What it does:
//!  * `identify`: look at a file or folder and say which console it is for, which game it
//!    is, and whether the dump looks complete and usable (and what to do if not);
//!  * `hash`: SHA-1 and CRC-32 of a file, to compare with the public Redump database;
//!  * drive listing and copying a plain data disc (2048-byte sectors) to an ISO file.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub mod receive;
pub mod crc32;

pub use ps5::sha1::sha1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Looks complete and usable.
    Good,
    /// Readable, but with something to look at.
    Warning,
    /// Not usable as it is.
    Problem,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub console: &'static str,
    pub what: String,
    pub title: String,
    pub verdict: Verdict,
    pub notes: Vec<String>,
}

impl Report {
    fn new(console: &'static str, what: &str) -> Report {
        Report { console, what: what.to_string(), title: String::new(), verdict: Verdict::Good, notes: Vec::new() }
    }

    fn warn(&mut self, n: impl Into<String>) {
        if self.verdict == Verdict::Good {
            self.verdict = Verdict::Warning;
        }
        self.notes.push(n.into());
    }

    fn fail(&mut self, n: impl Into<String>) {
        self.verdict = Verdict::Problem;
        self.notes.push(n.into());
    }

    pub fn text(&self) -> String {
        let mut s = format!("{}: {}", self.console, self.what);
        if !self.title.is_empty() {
            s.push_str(&format!(" \"{}\"", self.title));
        }
        s.push_str(match self.verdict {
            Verdict::Good => "\n  Looks complete.",
            Verdict::Warning => "\n  Usable, but check the notes.",
            Verdict::Problem => "\n  Not usable as it is.",
        });
        for n in &self.notes {
            s.push_str(&format!("\n  - {n}"));
        }
        s
    }
}

fn be32(d: &[u8], o: usize) -> u32 {
    d.get(o..o + 4).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn le32(d: &[u8], o: usize) -> u32 {
    d.get(o..o + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_head(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let mut v = vec![0u8; n];
    let got = f.read(&mut v)?;
    v.truncate(got);
    Ok(v)
}

/// Reads `len` bytes at `sector * 2048` from a plain ISO.
fn read_sector_bytes(f: &mut File, sector: u64, len: usize) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(sector * 2048))?;
    let mut v = vec![0u8; len];
    let got = f.read(&mut v)?;
    v.truncate(got);
    Ok(v)
}

/// Finds a file in the root directory of an ISO 9660 image (2048-byte sectors).
fn iso9660_root_file(f: &mut File, name: &str) -> Option<Vec<u8>> {
    let pvd = read_sector_bytes(f, 16, 2048).ok()?;
    if pvd.len() < 190 || &pvd[1..6] != b"CD001" {
        return None;
    }
    let (extent, size) = (le32(&pvd, 156 + 2) as u64, le32(&pvd, 156 + 10) as usize);
    let dir = read_sector_bytes(f, extent, size.min(1 << 20)).ok()?;
    let mut o = 0;
    while o < dir.len() {
        let len = dir[o] as usize;
        if len == 0 {
            // Records don't cross sector boundaries: skip to the next sector.
            o = (o / 2048 + 1) * 2048;
            continue;
        }
        let nlen = *dir.get(o + 32)? as usize;
        let ident = String::from_utf8_lossy(dir.get(o + 33..o + 33 + nlen)?).to_string();
        if ident.trim_end_matches(";1").eq_ignore_ascii_case(name) {
            let (ext, sz) = (le32(&dir, o + 2) as u64, le32(&dir, o + 10) as usize);
            return read_sector_bytes(f, ext, sz.min(1 << 20)).ok();
        }
        o += len;
    }
    None
}

/// "BOOT = cdrom:\SLUS_007.92;1" -> "SLUS_007.92"
fn boot_code(system_cnf: &str) -> Option<(bool, String)> {
    for line in system_cnf.lines() {
        let l = line.trim();
        let ps2 = l.to_ascii_uppercase().starts_with("BOOT2");
        if ps2 || l.to_ascii_uppercase().starts_with("BOOT") {
            let v = l.split('=').nth(1)?.trim();
            let v = v.trim_start_matches("cdrom0:").trim_start_matches("cdrom:").trim_start_matches('\\').trim_start_matches('/');
            let v = v.split(';').next().unwrap_or(v).trim();
            return Some((ps2, v.to_string()));
        }
    }
    None
}

fn identify_iso(path: &Path, size: u64, head: &[u8]) -> Report {
    // GameCube / Wii discs carry magic words in their header.
    if be32(head, 0x1C) == 0xC233_9F3D {
        let mut r = Report::new("GameCube", "disc image");
        r.title = String::from_utf8_lossy(head.get(0x20..0x60).unwrap_or(&[])).trim_end_matches('\0').trim().to_string();
        if size != 1_459_978_240 {
            r.warn(format!(
                "It is {size} bytes. A full GameCube disc image is exactly 1459978240. A smaller file is a trimmed or compressed image (such as NKit), which drops the disc's padding and can break games that read it; a full image from your own disc is the safe choice."
            ));
        }
        if size % 2048 != 0 {
            r.warn("The size isn't a whole number of sectors, so the file may be cut short.");
        }
        return r;
    }
    if be32(head, 0x18) == 0x5D1C_9EA3 {
        let mut r = Report::new("Wii", "disc image");
        r.title = String::from_utf8_lossy(head.get(0x20..0x60).unwrap_or(&[])).trim_end_matches('\0').trim().to_string();
        if size < 4_000_000_000 {
            r.warn("Wii discs are 4.7 GB or 8.5 GB; this is smaller, so it may be trimmed or compressed.");
        }
        return r;
    }
    if xbox::is_xbox_disc(path) {
        let mut r = Report::new("Xbox", "disc image (XISO)");
        if let Ok(mut f) = File::open(path) {
            if let Ok((_, xbe)) = xbox::read_disc(&mut f) {
                r.title = xbe.title.clone();
                r.notes.push(format!("Title code {}, {} sections.", xbe.title_code(), xbe.sections.len()));
            } else {
                r.fail("The disc's file system was found, but its default.xbe couldn't be read.");
            }
        }
        return r;
    }
    // PlayStation discs are ISO 9660 with a SYSTEM.CNF naming the boot file.
    if let Ok(mut f) = File::open(path) {
        if size % 2048 == 0 {
            if let Some(cnf) = iso9660_root_file(&mut f, "SYSTEM.CNF") {
                let text = String::from_utf8_lossy(&cnf).to_string();
                if let Some((ps2, code)) = boot_code(&text) {
                    let mut r = Report::new(if ps2 { "PlayStation 2" } else { "PlayStation" }, "disc image");
                    r.title = code;
                    if !ps2 {
                        r.warn("A plain .iso keeps only the data. PlayStation games with music or video tracks need the whole disc as .bin and .cue.");
                    }
                    return r;
                }
            }
        }
    }
    if size % 2048 == 0 && head.len() > 0x8006 && head.get(0x8001..0x8006) == Some(b"CD001".as_slice()) {
        let mut r = Report::new("Unknown", "ISO 9660 disc image");
        r.warn("It's a valid disc image, but not one of the consoles we recognise (no SYSTEM.CNF, not an Xbox or GameCube disc).");
        return r;
    }
    let mut r = Report::new("Unknown", "file");
    r.fail("This isn't a disc image we recognise. A .iso should start with a disc header; the file may be damaged or from another system.");
    r
}

fn identify_cue(path: &Path) -> Report {
    let mut r = Report::new("PlayStation", "cue sheet");
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            r.fail(format!("Can't read it: {e}"));
            return r;
        }
    };
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut files = 0;
    let mut tracks = 0;
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("FILE") {
            files += 1;
            let name = rest.trim().trim_start_matches('"');
            let name = name.split('"').next().unwrap_or(name);
            let p: PathBuf = dir.join(name);
            match std::fs::metadata(&p) {
                Ok(m) => {
                    if m.len() % 2352 != 0 {
                        r.warn(format!("{name} is {} bytes, not a whole number of 2352-byte sectors, so it may be cut short.", m.len()));
                    }
                }
                Err(_) => r.fail(format!("The cue sheet refers to {name}, but that file is missing from the same folder.")),
            }
        } else if l.starts_with("TRACK") {
            tracks += 1;
        }
    }
    if files == 0 {
        r.fail("No FILE lines: this isn't a usable cue sheet.");
    }
    r.notes.push(format!("{files} file(s), {tracks} track(s)."));
    r
}

fn identify_raw_bin(size: u64, head: &[u8]) -> Report {
    let mut r = Report::new("PlayStation", "raw disc image (.bin)");
    let sync = [0u8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];
    if size % 2352 != 0 || head.get(..12) != Some(&sync[..]) {
        r.fail("This .bin doesn't look like a raw 2352-byte-per-sector CD image.");
        return r;
    }
    r.notes.push(format!("{} sectors (about {} MB).", size / 2352, size / 1_000_000));
    r.warn("Keep the matching .cue sheet next to it; the app needs it to find the tracks.");
    r
}

fn identify_dir(path: &Path) -> Report {
    if path.join("eboot.bin").is_file() {
        let mut r = Report::new("PlayStation 4 / 5", "game folder");
        match ps5::read_game(path) {
            Ok(g) => {
                r.title = g.title.clone();
                r.notes.push(g.summary());
                if g.title_id.is_empty() {
                    r.warn("No sce_sys/param.json was found, so the title can't be named. A complete dump includes the sce_sys folder.");
                }
            }
            Err(e) => r.fail(e),
        }
        return r;
    }
    if path.join("MicrosoftGame.config").is_file() || path.join("appxmanifest.xml").is_file() || path.join("AppxManifest.xml").is_file() {
        let mut r = Report::new("Xbox One / Series", "package");
        let p = xboxone::package::inspect(path);
        r.title = p.game.name.clone();
        // Match the shared verdict scale so the command line and the app agree.
        r.verdict = match p.verdict {
            xboxone::package::Verdict::Complete => Verdict::Good,
            xboxone::package::Verdict::Incomplete => Verdict::Warning,
            _ => Verdict::Problem,
        };
        r.notes.push(format!(
            "{} {} ({} package, for {}), {} executable using {}.",
            p.game.name,
            p.game.version,
            p.game.kit,
            p.game.devices.join(" and "),
            if p.game.x64 { "an x86-64" } else { "an unidentified" },
            p.game.graphics()
        ));
        r.notes.extend(p.notes.iter().cloned());
        if p.verdict == xboxone::package::Verdict::Encrypted {
            r.fail(xboxone::ENCRYPTED_MESSAGE);
        } else if !p.blocking().is_empty() {
            r.warn(format!(
                "Nothing on this PC provides: {}. A game stops when it calls one of these.",
                p.blocking().iter().map(|d| d.dll.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        return r;
    }
    let mut r = Report::new("Unknown", "folder");
    r.fail("This folder holds no eboot.bin (PlayStation 4/5) and no MicrosoftGame.config or appxmanifest.xml (Xbox One/Series). Choose the game's top folder.");
    r
}

/// Looks at a file or folder and reports what it is and whether it's usable.
pub fn identify(path: &Path) -> Report {
    if path.is_dir() {
        return identify_dir(path);
    }
    let Ok(meta) = std::fs::metadata(path) else {
        let mut r = Report::new("Unknown", "file");
        r.fail("That file can't be found or opened.");
        return r;
    };
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let head = read_head(path, 0x10000).unwrap_or_default();
    match ext.as_str() {
        "cue" => return identify_cue(path),
        "nsp" | "xci" => {
            let mut r = Report::new("Nintendo Switch", if ext == "nsp" { "game package (NSP)" } else { "game card image (XCI)" });
            match switch::pfs::game_files(path) {
                Ok(files) => {
                    let nca = files.iter().filter(|f| f.name.ends_with(".nca")).count();
                    let has_ticket = files.iter().any(|f| f.name.ends_with(".tik"));
                    r.notes.push(format!("{} files inside, {nca} of them game content (NCA).", files.len()));
                    if nca == 0 {
                        r.fail("There is no game content in this package.");
                    }
                    if ext == "nsp" && !has_ticket {
                        r.notes.push("No ticket (.tik) is included. That is normal for a package made from a game card, but a package from the eShop needs its ticket to be playable.".into());
                    }
                    r.notes.push("Switch game content is encrypted with keys from your own console. This tool never asks for or handles those keys.".into());
                }
                Err(e) => r.fail(e),
            }
            return r;
        }
        "xbe" => {
            let mut r = Report::new("Xbox", "program (XBE)");
            match xbox::xbe::parse(&std::fs::read(path).unwrap_or_default()) {
                Ok(x) => r.title = x.title,
                Err(e) => r.fail(e),
            }
            return r;
        }
        _ => {}
    }
    if head.starts_with(b"XEX2") || head.starts_with(b"XEX1") {
        let mut r = Report::new("Xbox 360", "program (XEX)");
        r.notes.push("A 360 game also needs its other files next to the XEX; dump the whole disc, not only this file.".into());
        return r;
    }
    // An Xbox One / Series retail package. Always encrypted, so there is nothing to read.
    if xboxone::is_encrypted_package(&head) || ext == "xvc" || ext == "msixvc" {
        let mut r = Report::new("Xbox One / Series", "retail package");
        r.fail(xboxone::ENCRYPTED_MESSAGE);
        return r;
    }
    if head.starts_with(b"CON ") || head.starts_with(b"LIVE") || head.starts_with(b"PIRS") {
        return Report::new("Xbox 360", "game package (STFS / Games on Demand)");
    }
    if head.starts_with(&ps5::SELF_MAGIC_PS4) || head.starts_with(&ps5::SELF_MAGIC_PS5) {
        let mut r = Report::new("PlayStation 4 / 5", "program");
        r.fail(ps5::ENCRYPTED_MESSAGE);
        return r;
    }
    if head.starts_with(b"\x7FELF") {
        let mut r = Report::new("PlayStation 4 / 5", "program (decrypted ELF)");
        match ps5::Elf::parse(&head.iter().copied().chain(std::fs::read(path).unwrap_or_default().into_iter().skip(head.len())).collect::<Vec<u8>>()) {
            Ok(e) => r.notes.push(format!("{} segments, {} imported functions.", e.segments.len(), e.imports.len())),
            Err(e) => r.fail(e),
        }
        return r;
    }
    if ext == "bin" && meta.len() % 2352 == 0 {
        return identify_raw_bin(meta.len(), &head);
    }
    identify_iso(path, meta.len(), &head)
}

// ---------------------------------------------------------------- hashing

pub struct Hashes {
    pub bytes: u64,
    pub sha1: String,
    pub crc32: String,
}

/// SHA-1 and CRC-32 of a file, streaming (discs are gigabytes).
pub fn hash_file(path: &Path, progress: &mut dyn FnMut(u64)) -> std::io::Result<Hashes> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut sha = ps5::sha1::Sha1::new();
    let mut crc = crc32::Crc32::new();
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        crc.update(&buf[..n]);
        total += n as u64;
        progress(total);
    }
    Ok(Hashes { bytes: total, sha1: hex(&sha.finish()), crc32: format!("{:08x}", crc.finish()) })
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------- drives and data discs

/// Optical drive letters on this PC (Windows); empty elsewhere.
pub fn optical_drives() -> Vec<String> {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetDriveTypeW(root: *const u16) -> u32;
        }
        let mut out = Vec::new();
        for c in b'A'..=b'Z' {
            let root: Vec<u16> = format!("{}:\\", c as char).encode_utf16().chain(Some(0)).collect();
            // DRIVE_CDROM = 5
            if unsafe { GetDriveTypeW(root.as_ptr()) } == 5 {
                out.push(format!("{}:", c as char));
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

pub struct CopyResult {
    pub sectors: u64,
    pub bad_sectors: Vec<u64>,
}

/// Copies a plain data disc (2048-byte sectors) from `source` (a drive such as `\\.\E:`, or
/// an existing file) to `dest`. Unreadable sectors are written as zeros and reported, so a
/// scratched disc still gives a complete file you can then check against Redump.
pub fn copy_data_disc(source: &str, dest: &Path, progress: &mut dyn FnMut(u64, u64)) -> std::io::Result<CopyResult> {
    let mut src = File::open(source)?;
    let total = src.seek(SeekFrom::End(0)).unwrap_or(0) / 2048;
    src.seek(SeekFrom::Start(0))?;
    let mut out = File::create(dest)?;
    let mut bad = Vec::new();
    let mut sector = 0u64;
    let mut buf = vec![0u8; 2048 * 16];
    loop {
        if total > 0 && sector >= total {
            break;
        }
        let want = if total > 0 { (total - sector).min(16) as usize } else { 16 };
        src.seek(SeekFrom::Start(sector * 2048))?;
        match src.read(&mut buf[..want * 2048]) {
            Ok(0) => break,
            Ok(n) => {
                let whole = n / 2048 * 2048;
                if whole == 0 {
                    bad.push(sector);
                    out.write_all(&[0u8; 2048])?;
                    sector += 1;
                } else {
                    out.write_all(&buf[..whole])?;
                    sector += (whole / 2048) as u64;
                }
            }
            Err(_) => {
                // Retry one sector at a time and zero-fill what can't be read.
                for k in 0..want as u64 {
                    let mut one = [0u8; 2048];
                    let _ = src.seek(SeekFrom::Start((sector + k) * 2048));
                    match src.read(&mut one) {
                        Ok(2048) => out.write_all(&one)?,
                        _ => {
                            bad.push(sector + k);
                            out.write_all(&[0u8; 2048])?;
                        }
                    }
                }
                sector += want as u64;
            }
        }
        progress(sector, total);
    }
    out.flush()?;
    Ok(CopyResult { sectors: sector, bad_sectors: bad })
}

/// The written guide, shared by the command line and the app.
pub const GUIDE: &str = include_str!("guide.txt");

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gamedump_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A Developer Mode package built from the xboxone test PE, so identify and the real parser
    /// agree on the same file.
    fn xbox_package(dir: &Path, dlls: &[&str]) {
        std::fs::write(
            dir.join("MicrosoftGame.config"),
            r#"<Game><ExecutableList><Executable Name="Demo.exe" TargetDeviceFamily="Scarlett" Id="G"/></ExecutableList><TitleId>AAAA</TitleId></Game>"#,
        )
        .unwrap();
        std::fs::write(dir.join("Demo.exe"), xboxone::pe::sample_pe(dlls)).unwrap();
    }

    #[test]
    fn identifies_a_decrypted_xbox_package_and_says_what_it_needs() {
        let d = temp("xbox_ok");
        xbox_package(&d, &["kernelx.dll", "d3d12_x.dll"]);
        let r = identify(&d);
        assert_eq!(r.console, "Xbox One / Series");
        // kernelx and d3d12_x both resolve to real Windows DLLs, so nothing is blocking.
        assert_eq!(r.verdict, Verdict::Good, "{}", r.text());
        assert!(r.text().contains("x86-64"));
    }

    #[test]
    fn a_package_needing_an_unbuilt_library_is_only_a_warning() {
        let d = temp("xbox_block");
        xbox_package(&d, &["kernelx.dll", "xboxnet.dll"]);
        let r = identify(&d);
        assert_eq!(r.verdict, Verdict::Warning);
        assert!(r.text().contains("xboxnet.dll"));
    }

    #[test]
    fn an_encrypted_xbox_package_is_a_problem_with_the_reason() {
        let d = temp("xbox_enc");
        let mut xvc = vec![0u8; 0x600];
        xvc[0x200..0x208].copy_from_slice(b"msft-xvd");
        std::fs::write(d.join("g.xvc"), &xvc).unwrap();
        let r = identify(&d.join("g.xvc"));
        assert_eq!(r.console, "Xbox One / Series");
        assert_eq!(r.verdict, Verdict::Problem);
        assert!(r.text().contains("no PC can unlock"));
    }

    /// A tiny ISO 9660 image with a SYSTEM.CNF in the root.
    fn ps_iso(cnf: &str) -> Vec<u8> {
        let mut img = vec![0u8; 2048 * 24];
        let pvd = 16 * 2048;
        img[pvd] = 1;
        img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        let root = 156 + pvd;
        img[root + 2..root + 6].copy_from_slice(&20u32.to_le_bytes());
        img[root + 10..root + 14].copy_from_slice(&2048u32.to_le_bytes());
        let dir = 20 * 2048;
        let name = b"SYSTEM.CNF;1";
        let rec = 33 + name.len();
        img[dir] = rec as u8;
        img[dir + 2..dir + 6].copy_from_slice(&22u32.to_le_bytes());
        img[dir + 10..dir + 14].copy_from_slice(&(cnf.len() as u32).to_le_bytes());
        img[dir + 32] = name.len() as u8;
        img[dir + 33..dir + 33 + name.len()].copy_from_slice(name);
        img[22 * 2048..22 * 2048 + cnf.len()].copy_from_slice(cnf.as_bytes());
        img
    }

    #[test]
    fn recognises_a_playstation_disc_and_its_game_code() {
        let d = temp("ps1");
        let p = d.join("game.iso");
        std::fs::write(&p, ps_iso("BOOT = cdrom:\\SLUS_007.92;1\r\nTCB = 4\r\n")).unwrap();
        let r = identify(&p);
        assert_eq!(r.console, "PlayStation");
        assert_eq!(r.title, "SLUS_007.92");
        let p2 = d.join("game2.iso");
        std::fs::write(&p2, ps_iso("BOOT2 = cdrom0:\\SLES_500.00;1\r\nVER = 1.00\r\n")).unwrap();
        assert_eq!(identify(&p2).console, "PlayStation 2");
    }

    #[test]
    fn recognises_gamecube_and_flags_trimmed_images() {
        let d = temp("gc");
        let p = d.join("g.iso");
        let mut h = vec![0u8; 0x10000];
        h[0x1C..0x20].copy_from_slice(&0xC233_9F3Du32.to_be_bytes());
        h[0x20..0x28].copy_from_slice(b"Test Gam");
        std::fs::write(&p, h).unwrap();
        let r = identify(&p);
        assert_eq!(r.console, "GameCube");
        assert_eq!(r.verdict, Verdict::Warning, "a 64 KB file can't be a full disc");
    }

    #[test]
    fn cue_with_a_missing_bin_is_a_problem() {
        let d = temp("cue");
        let cue = d.join("g.cue");
        std::fs::write(&cue, "FILE \"g.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n").unwrap();
        assert_eq!(identify(&cue).verdict, Verdict::Problem);
        std::fs::write(d.join("g.bin"), vec![0u8; 2352 * 4]).unwrap();
        assert_eq!(identify(&cue).verdict, Verdict::Good);
    }

    #[test]
    fn encrypted_playstation_programs_are_refused_with_the_reason() {
        let d = temp("self");
        let p = d.join("eboot.bin");
        let mut b = ps5::SELF_MAGIC_PS5.to_vec();
        b.resize(0x400, 0);
        std::fs::write(&p, b).unwrap();
        let r = identify(&p);
        assert_eq!(r.verdict, Verdict::Problem);
        assert!(r.text().contains("encrypted SELF"), "{}", r.text());
    }

    #[test]
    fn hashes_match_known_values() {
        let d = temp("hash");
        let p = d.join("a.bin");
        std::fs::write(&p, b"abc").unwrap();
        let h = hash_file(&p, &mut |_| {}).unwrap();
        assert_eq!(h.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(h.crc32, "352441c2");
        assert_eq!(h.bytes, 3);
    }

    #[test]
    fn copies_a_data_disc_and_reports_nothing_bad() {
        let d = temp("copy");
        let src = d.join("src.iso");
        let data: Vec<u8> = (0..2048 * 40).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let dst = d.join("dst.iso");
        let r = copy_data_disc(src.to_str().unwrap(), &dst, &mut |_, _| {}).unwrap();
        assert_eq!(r.sectors, 40);
        assert!(r.bad_sectors.is_empty());
        assert_eq!(std::fs::read(&dst).unwrap(), data);
    }
}

#[cfg(test)]
mod switch_tests {
    use super::*;

    #[test]
    fn nsp_is_read_and_missing_content_is_a_problem() {
        // A PFS0 with one NCA-named file.
        let name = b"a.nca\0";
        let mut d = Vec::new();
        d.extend_from_slice(b"PFS0");
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&(name.len() as u32).to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&0u64.to_le_bytes()); // offset
        d.extend_from_slice(&4u64.to_le_bytes()); // size
        d.extend_from_slice(&0u32.to_le_bytes()); // name offset
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(name);
        d.extend_from_slice(&[1, 2, 3, 4]);
        let p = std::env::temp_dir().join(format!("gd-test-{}.nsp", std::process::id()));
        std::fs::write(&p, d).unwrap();
        let r = identify(&p);
        assert_eq!(r.console, "Nintendo Switch");
        assert!(r.text().contains("game content"), "{}", r.text());
        let _ = std::fs::remove_file(&p);
    }
}
