//! Game artwork (box art, falling back to logos) from the libretro-thumbnails
//! project on GitHub, matched by No-Intro file name and cached on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

/// libretro-thumbnails repositories for each console id (first match wins).
fn repos(console: &str) -> &'static [&'static str] {
    match console {
        "nes" => &["Nintendo_-_Nintendo_Entertainment_System", "Nintendo_-_Family_Computer_Disk_System"],
        "snes" => &["Nintendo_-_Super_Nintendo_Entertainment_System", "Nintendo_-_Satellaview"],
        "gb" => &["Nintendo_-_Game_Boy", "Nintendo_-_Game_Boy_Color"],
        "gba" => &["Nintendo_-_Game_Boy_Advance"],
        "vb" => &["Nintendo_-_Virtual_Boy"],
        "n64" => &["Nintendo_-_Nintendo_64"],
        "gc" => &["Nintendo_-_GameCube"],
        "wii" => &["Nintendo_-_Wii"],
        "wiiu" => &["Nintendo_-_Wii_U"],
        "nds" => &["Nintendo_-_Nintendo_DS"],
        "vectrex" => &["GCE_-_Vectrex"],
        "3ds" => &["Nintendo_-_Nintendo_3DS"],
        "switch" => &["Nintendo_-_Nintendo_Switch"],
        "ps1" => &["Sony_-_PlayStation"],
        "ps2" => &["Sony_-_PlayStation_2"],
        "ps3" => &["Sony_-_PlayStation_3"],
        "ps4" => &["Sony_-_PlayStation_4"],
        "psp" => &["Sony_-_PlayStation_Portable"],
        "vita" => &["Sony_-_PlayStation_Vita"],
        "xbox" => &["Microsoft_-_Xbox"],
        "x360" => &["Microsoft_-_Xbox_360"],
        "sms" => &["Sega_-_Master_System_-_Mark_III"],
        "md" => &["Sega_-_Mega_Drive_-_Genesis"],
        "gg" => &["Sega_-_Game_Gear"],
        "segacd" => &["Sega_-_Mega-CD_-_Sega_CD"],
        "32x" => &["Sega_-_32X"],
        "saturn" => &["Sega_-_Saturn"],
        "dc" => &["Sega_-_Dreamcast"],
        "a2600" => &["Atari_-_2600"],
        "a7800" => &["Atari_-_7800"],
        "lynx" => &["Atari_-_Lynx"],
        "pce" => &["NEC_-_PC_Engine_-_TurboGrafx_16", "NEC_-_PC_Engine_SuperGrafx", "NEC_-_PC_Engine_CD_-_TurboGrafx-CD"],
        "ws" => &["Bandai_-_WonderSwan", "Bandai_-_WonderSwan_Color"],
        "neogeo" => &["SNK_-_Neo_Geo"],
        "ngp" => &["SNK_-_Neo_Geo_Pocket", "SNK_-_Neo_Geo_Pocket_Color"],
        "3do" => &["The_3DO_Company_-_3DO"],
        _ => &[],
    }
}

/// libretro's file-name rule: these characters become underscores.
pub fn thumb_name(title: &str) -> String {
    title.chars().map(|c| if "&*/:`<>?\\|\"".contains(c) { '_' } else { c }).collect()
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'(' | b')' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Names to try: exact, without revision tags, then with a (USA) region if none was given.
fn candidates(title: &str) -> Vec<String> {
    let mut out = Vec::new();
    // "Resident Evil (USA) Disc 1" is named "Resident Evil (USA) (Disc 1)" in the artwork sets.
    if let Some(i) = title.rfind(" Disc ") {
        let (head, tail) = (&title[..i], title[i + 1..].trim());
        if !tail.contains('(') && tail.len() < 10 {
            out.push(format!("{head} ({tail})"));
            out.push(head.to_string());
        }
    }
    out.push(title.to_string());
    // Dump tags such as "[!]", "[h1]" or "[T+Eng]" aren't part of artwork names.
    if title.contains('[') {
        let clean = title.split('[').next().unwrap_or(title).trim().to_string();
        if !clean.is_empty() {
            out.push(clean);
        }
    }
    let no_rev: String = title
        .split(" (")
        .enumerate()
        .filter(|(i, part)| *i == 0 || !(part.starts_with("Rev") || part.starts_with("v1") || part.starts_with("Beta")))
        .map(|(i, p)| if i == 0 { p.to_string() } else { format!(" ({p}") })
        .collect();
    if no_rev != title {
        out.push(no_rev.clone());
    }
    // Drop language lists such as "(En,Es)" that artwork names often omit.
    let no_lang: String = no_rev
        .split(" (")
        .enumerate()
        .filter(|(i, part)| *i == 0 || !part.trim_end_matches(')').split(',').all(|l| l.len() == 2 && l.chars().next().is_some_and(|c| c.is_ascii_uppercase())))
        .map(|(i, p)| if i == 0 { p.to_string() } else { format!(" ({p}") })
        .collect();
    if no_lang != no_rev {
        out.push(no_lang);
    }
    // Disc sets are often named with a version tag (Redump style); try the common ones.
    let base = out.last().cloned().unwrap_or_else(|| title.to_string());
    for v in ["v1.00", "v1.01", "v1.02", "v1.03", "v2.00", "v3.00", "Rev 1"] {
        out.push(format!("{base} ({v})"));
    }
    if !title.contains('(') {
        out.push(format!("{title} (USA)"));
        out.push(format!("{title} (World)"));
        out.push(format!("{title} (USA, Europe)"));
        out.push(format!("{title} (Europe)"));
        out.push(format!("{title} (Japan)"));
    }
    out
}

struct Decoded {
    key: PathBuf,
    image: Option<ColorImage>,
}

enum Entry {
    Pending,
    Ready(TextureHandle),
    Missing,
}

pub struct Covers {
    entries: HashMap<PathBuf, Entry>,
    tx: Sender<(PathBuf, String, String)>,
    rx: Receiver<Decoded>,
}

impl Covers {
    pub fn new(cache_dir: PathBuf) -> Self {
        let (tx, job_rx) = channel::<(PathBuf, String, String)>();
        let (done_tx, rx) = channel::<Decoded>();
        let dir = cache_dir;
        // One worker thread: fetches are sequential so we stay polite to GitHub.
        std::thread::spawn(move || {
            while let Ok((key, console, title)) = job_rx.recv() {
                let image = fetch(&dir, &console, &title);
                if done_tx.send(Decoded { key, image }).is_err() {
                    break;
                }
            }
        });
        Self { entries: HashMap::new(), tx, rx }
    }

    /// Returns the texture for a game if ready; queues a fetch the first time it's asked for.
    pub fn get(&mut self, ctx: &egui::Context, game: &Path, console: &str, title: &str) -> Option<&TextureHandle> {
        while let Ok(done) = self.rx.try_recv() {
            let entry = match done.image {
                Some(img) => Entry::Ready(ctx.load_texture(done.key.to_string_lossy(), img, TextureOptions::LINEAR)),
                None => Entry::Missing,
            };
            self.entries.insert(done.key, entry);
            ctx.request_repaint();
        }
        if !self.entries.contains_key(game) {
            if repos(console).is_empty() {
                self.entries.insert(game.to_path_buf(), Entry::Missing);
            } else {
                self.entries.insert(game.to_path_buf(), Entry::Pending);
                let _ = self.tx.send((game.to_path_buf(), console.to_string(), title.to_string()));
            }
        }
        match self.entries.get(game) {
            Some(Entry::Ready(t)) => Some(t),
            Some(Entry::Pending) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
                None
            }
            _ => None,
        }
    }

    /// True when no official artwork exists for this game (lookup finished without a match).
    pub fn is_missing(&self, game: &Path) -> bool {
        matches!(self.entries.get(game), Some(Entry::Missing))
    }

    /// Uses a captured frame (0RGB pixels) as the game's artwork and saves it to the cache.
    pub fn set_from_frame(&mut self, ctx: &egui::Context, cache: &Path, game: &Path, console: &str, title: &str, px: &[u32], w: usize, h: usize) {
        let folder = cache.join(console);
        let _ = std::fs::create_dir_all(&folder);
        let base = thumb_name(title);
        let _ = std::fs::remove_file(folder.join(format!("{base}.miss2")));
        let rgba: Vec<u8> = px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255]).collect();
        if let Ok(file) = std::fs::File::create(folder.join(format!("{base}.png"))) {
            let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
            enc.set_color(png::ColorType::Rgba);
            if let Ok(mut wr) = enc.write_header() {
                let _ = wr.write_image_data(&rgba);
            }
        }
        let img = ColorImage::from_rgba_unmultiplied([w, h], &rgba);
        let tex = ctx.load_texture(game.to_string_lossy(), img, TextureOptions::NEAREST);
        self.entries.insert(game.to_path_buf(), Entry::Ready(tex));
    }

    pub fn is_pending(&self, game: &Path) -> bool {
        matches!(self.entries.get(game), Some(Entry::Pending))
    }

}

/// Finds artwork on disk or GitHub. A `.none` marker remembers misses so we don't re-ask.
fn fetch(dir: &Path, console: &str, title: &str) -> Option<ColorImage> {
    if repos(console).is_empty() {
        return None;
    }
    let folder = dir.join(console);
    let _ = std::fs::create_dir_all(&folder);
    let base = thumb_name(title);
    let cached = folder.join(format!("{base}.png"));
    let miss = folder.join(format!("{base}.miss2"));
    if cached.exists() {
        return decode(&cached);
    }
    // A remembered miss expires after a day, so fixed name matching gets another go.
    let fresh = std::fs::metadata(&miss).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e.as_secs() < 86_400);
    if fresh {
        return None;
    }
    // Box art first, then the title logo, then the title screen, across every repository.
    for kind in ["Named_Boxarts", "Named_Logos", "Named_Titles"] {
        for repo in repos(console) {
            for name in candidates(title) {
                let url = format!("https://raw.githubusercontent.com/libretro-thumbnails/{repo}/master/{kind}/{}.png", url_encode(&thumb_name(&name)));
                let tmp = folder.join(format!("{base}.part"));
                let ok = crate::quiet(crate::platform::curl())
                    .args(["-sfL", "--max-time", "20", "-o"])
                    .arg(&tmp)
                    .arg(&url)
                    .status()
                    .is_ok_and(|s| s.success());
                if ok && decode(&tmp).is_some() {
                    let _ = std::fs::rename(&tmp, &cached);
                    return decode(&cached);
                }
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    let _ = std::fs::write(&miss, b"");
    None
}

pub fn decode(path: &Path) -> Option<ColorImage> {
    let file = std::fs::File::open(path).ok()?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        _ => return None,
    };
    Some(ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

#[cfg(test)]
mod name_tests {
    use super::candidates;

    #[test]
    fn disc_suffix_gets_artwork_style_name() {
        assert_eq!(candidates("Resident Evil (USA) Disc 1")[0], "Resident Evil (USA) (Disc 1)");
    }
}
