//! PS5 (and PS4) executables from a game you decrypted on your own console.
//!
//! Retail PlayStation executables ("SELF" files) are signed and encrypted with keys that
//! never leave the console, so a PC can't open them directly. A game dumped from a console
//! you own comes out as a plain 64-bit x86 ELF program plus a few Sony-specific segments.
//! This crate reads such a program: its segments, the system libraries it imports and
//! exactly which of their functions it calls (Sony names every function by a 64-bit hash,
//! a "NID"), so the app can say what the game needs from our reimplementation of those
//! libraries. Running the program is the next stage and builds on this loader.

pub mod elf;
pub mod image;
pub mod metadata;
pub mod machine;
pub mod native;
pub mod nid;
pub mod sha1;
pub mod system;

use std::path::Path;

pub use elf::{Elf, Import, Segment};

/// Magic of a signed/encrypted PS4 SELF container.
pub const SELF_MAGIC_PS4: [u8; 4] = [0x4F, 0x15, 0x3D, 0x1D];
/// Magic of a signed/encrypted PS5 SELF container.
pub const SELF_MAGIC_PS5: [u8; 4] = [0x54, 0x14, 0xF5, 0xEE];

pub const ENCRYPTED_MESSAGE: &str = "This PlayStation program contains encrypted SELF segments. This emulator requires a decrypted executable.";

/// What we learn about a game.
#[derive(Debug, Clone, Default)]
pub struct GameInfo {
    pub title: String,
    pub title_id: String,
    pub version: String,
    pub elf: Option<Elf>,
}

impl GameInfo {
    pub fn summary(&self) -> String {
        let mut s = String::new();
        if !self.title.is_empty() || !self.title_id.is_empty() {
            s.push_str(&format!(
                "{}{} ",
                if self.title.is_empty() { "This game".to_string() } else { self.title.clone() },
                if self.title_id.is_empty() { String::new() } else { format!(" ({})", self.title_id) }
            ));
        } else {
            s.push_str("This program ");
        }
        match &self.elf {
            Some(e) => {
                s.push_str(&format!(
                    "is a decrypted {} program: {} segments, {} system libraries, {} imported functions ({} we already know by name). ",
                    e.kind(),
                    e.segments.len(),
                    e.libraries.len(),
                    e.imports.len(),
                    e.imports.iter().filter(|i| i.name.is_some()).count()
                ));
                if !e.libraries.is_empty() {
                    let mut l = e.libraries.clone();
                    l.sort();
                    s.push_str(&format!("It uses {}. ", l.join(", ")));
                }
            }
            None => s.push_str("was found, but its executable wasn't. "),
        }
        s.push_str("Running it needs our own PlayStation system libraries and graphics backend, which are in progress.");
        s
    }
}

/// Finds a value for `"key": "value"` in a JSON file (param.json); enough for the few flat
/// fields we read, with no JSON library.
pub fn json_str(text: &str, key: &str) -> Option<String> {
    let k = format!("\"{key}\"");
    let at = text.find(&k)? + k.len();
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    if let Some(r) = rest.strip_prefix('"') {
        let end = r.find('"')?;
        Some(r[..end].to_string())
    } else {
        let end = rest.find([',', '}', '\n']).unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    }
}

/// Reads a game folder (containing `eboot.bin`, and `sce_sys/param.json` for the title) or
/// an executable file directly.
pub fn read_game(path: &Path) -> Result<GameInfo, String> {
    let exe = if path.is_dir() { path.join("eboot.bin") } else { path.to_path_buf() };
    let mut info = metadata::read(path)?;
    let data = std::fs::read(&exe).map_err(|e| {
        if path.is_dir() {
            format!("This folder has no eboot.bin ({e}). Choose a game folder dumped from your own PlayStation.")
        } else {
            format!("{}: {e}", exe.display())
        }
    })?;
    let image = image::program_image(&data)?;
    info.elf = Some(Elf::parse(&image)?);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_param_json_fields() {
        let t = r#"{ "titleId": "PPSA01234", "contentVersion": "01.002.000", "localizedParameters": { "en-US": { "titleName": "Demo Game" } } }"#;
        assert_eq!(json_str(t, "titleId").as_deref(), Some("PPSA01234"));
        assert_eq!(json_str(t, "titleName").as_deref(), Some("Demo Game"));
        assert_eq!(json_str(t, "missing"), None);
    }
}
