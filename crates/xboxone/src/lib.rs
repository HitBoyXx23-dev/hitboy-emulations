//! Xbox One / Series X|S games. Retail packages (XVC / MSIXVC) are encrypted with keys that
//! never leave the console, so only folders already decrypted on the owner's console (or
//! Developer Mode apps) can run. This crate reads such a folder: the package manifest
//! (`appxmanifest.xml` for XDK titles, `MicrosoftGame.config` for GDK titles) and the game's
//! x86-64 executable, so the app can say exactly what the game needs from our own
//! reimplementation of the Xbox system libraries.

pub mod apu_audio;
pub mod d3d12x_com;
pub mod dstorage;
pub mod era_mem;
pub mod exe_convert;
pub mod firmware;
pub mod gpu_bridge;
pub mod modules;
pub mod native;
#[cfg(all(windows, target_arch = "x86_64"))]
pub mod native_pages;
pub mod package;
pub mod pe;
mod rdna2;
pub mod rdna2_disasm;
pub mod shims;
pub mod x64_emu;

use std::path::{Path, PathBuf};

pub use modules::Provider;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GameInfo {
    pub name: String,
    pub identity: String,
    pub publisher: String,
    pub version: String,
    pub title_id: String,
    /// "Xbox One", "Xbox Series X|S" or both.
    pub devices: Vec<String>,
    /// "XDK" (appxmanifest) or "GDK" (MicrosoftGame.config).
    pub kit: &'static str,
    pub executable: String,
    pub imports: Vec<String>,
    /// Executable is x86-64 (AMD64).
    pub x64: bool,
}

impl GameInfo {
    /// Graphics API the game was written for.
    pub fn graphics(&self) -> &'static str {
        let has = |n: &str| self.imports.iter().any(|i| i.eq_ignore_ascii_case(n));
        if has("d3d12_x.dll") || has("d3d12_xs.dll") || has("d3d12.dll") {
            "Direct3D 12"
        } else if has("d3d11_x.dll") || has("d3d11.dll") {
            "Direct3D 11.X"
        } else {
            "unknown"
        }
    }

    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} {} for {} ({} package{}). {} executable using {}.",
            self.name,
            self.version,
            if self.devices.is_empty() { "Xbox One".to_string() } else { self.devices.join(" and ") },
            self.kit,
            if self.title_id.is_empty() { String::new() } else { format!(", title {}", self.title_id) },
            if self.x64 { "x86-64" } else { "Unknown" },
            self.graphics()
        );
        s.push_str(" Ready for Native mode (Windows DLL redirection, SEH unwind, GDK/XDK/D3D12.X shims) and Emulator mode (sandboxed x86-64 core + Era memory + graphics bridge).");
        s
    }
}

/// Magic of an XVD / XVC container (at offset 0x200): an encrypted retail package.
pub fn is_encrypted_package(head: &[u8]) -> bool {
    head.get(0x200..0x208) == Some(b"msft-xvd")
}

pub const ENCRYPTED_MESSAGE: &str = "This is an encrypted Xbox package. Xbox One and Series games are locked to the console they were installed on, and no PC can unlock them. Only games decrypted on your own console, or Developer Mode apps, can be opened.";

/// Value of `name="..."` inside the first `<tag ...>` element.
fn attr(xml: &str, tag: &str, name: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open) {
        let start = from + i + open.len();
        // Must be the whole tag name (not a prefix of a longer one).
        let next = xml[start..].chars().next()?;
        if next.is_whitespace() || next == '>' || next == '/' {
            let end = xml[start..].find('>')? + start;
            let body = &xml[start..end];
            let key = format!("{name}=");
            let mut p = 0;
            while let Some(k) = body[p..].find(&key) {
                let at = p + k;
                // Attribute names are preceded by whitespace.
                if at == 0 || body.as_bytes()[at - 1].is_ascii_whitespace() {
                    let rest = &body[at + key.len()..];
                    let q = rest.chars().next()?;
                    let val = &rest[1..];
                    return Some(unescape(&val[..val.find(q)?]));
                }
                p = at + key.len();
            }
            return None;
        }
        from = start;
    }
    None
}

/// Text inside the first `<tag>...</tag>`.
fn text(xml: &str, tag: &str) -> Option<String> {
    let s = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let e = xml[s..].find(&format!("</{tag}>"))? + s;
    Some(unescape(xml[s..e].trim()))
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Reads an XDK `appxmanifest.xml`.
pub fn parse_appx_manifest(xml: &str) -> GameInfo {
    let mut g = GameInfo { kit: "XDK", ..Default::default() };
    g.identity = attr(xml, "Identity", "Name").unwrap_or_default();
    g.publisher = attr(xml, "Identity", "Publisher").unwrap_or_default();
    g.version = attr(xml, "Identity", "Version").unwrap_or_default();
    g.name = text(xml, "DisplayName").filter(|s| !s.starts_with("ms-resource:")).unwrap_or_else(|| g.identity.clone());
    g.executable = attr(xml, "Application", "Executable").unwrap_or_default();
    g.title_id = text(xml, "mx:TitleId").or_else(|| text(xml, "TitleId")).unwrap_or_default();
    g.devices = vec!["Xbox One".into()];
    g
}

/// Reads a GDK `MicrosoftGame.config`.
pub fn parse_game_config(xml: &str) -> GameInfo {
    let mut g = GameInfo { kit: "GDK", ..Default::default() };
    g.identity = attr(xml, "Identity", "Name").unwrap_or_default();
    g.publisher = attr(xml, "Identity", "Publisher").unwrap_or_default();
    g.version = attr(xml, "Identity", "Version").unwrap_or_default();
    g.name = attr(xml, "ShellVisuals", "DefaultDisplayName").filter(|s| !s.starts_with("ms-resource:")).unwrap_or_else(|| g.identity.clone());
    g.executable = attr(xml, "Executable", "Name").unwrap_or_default();
    g.title_id = text(xml, "TitleId").unwrap_or_default();
    let fam = attr(xml, "Executable", "TargetDeviceFamily").unwrap_or_default();
    g.devices = match fam.as_str() {
        "Scarlett" => vec!["Xbox Series X|S".into()],
        "XboxOne" => vec!["Xbox One".into()],
        "PC" => vec!["PC".into()],
        _ => vec!["Xbox One".into(), "Xbox Series X|S".into()],
    };
    g
}

/// Reads a decrypted game folder (or any file inside it).
pub fn read_game(path: &Path) -> Result<GameInfo, String> {
    let dir: PathBuf = if path.is_dir() { path.to_path_buf() } else { path.parent().map(Path::to_path_buf).unwrap_or_default() };
    if path.is_file() {
        let mut head = vec![0u8; 0x208];
        use std::io::Read;
        let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
        if is_encrypted_package(&head[..n]) {
            return Err(ENCRYPTED_MESSAGE.into());
        }
    }
    let Some((manifest, is_gdk, _)) = package::find_manifest(&dir) else {
        return Err("This folder has no appxmanifest.xml or MicrosoftGame.config. Choose the folder of a game decrypted on your own Xbox, or a Developer Mode app.".into());
    };
    let xml = std::fs::read_to_string(&manifest).map_err(|e| format!("Couldn't read the manifest: {e}"))?;
    let mut g = if is_gdk { parse_game_config(&xml) } else { parse_appx_manifest(&xml) };
    if !g.executable.is_empty() {
        let exe = std::fs::read(dir.join(&g.executable)).map_err(|e| format!("{}: {e}", g.executable))?;
        let p = pe::parse(&exe)?;
        g.x64 = p.machine == 0x8664;
        g.imports = p.dll_names();
    }
    Ok(g)
}

/// Full path of a game's x86-64 executable, which is what native mode maps and runs.
pub fn executable_of(game: &GameInfo, dir: &Path) -> PathBuf {
    dir.join(&game.executable)
}

/// Reads a decrypted game folder and returns both its details and its executable's bytes, ready
/// to hand to [`native::run`].
pub fn load_for_native(path: &Path) -> Result<(GameInfo, Vec<u8>), String> {
    let dir = if path.is_dir() { path.to_path_buf() } else { path.parent().map(Path::to_path_buf).unwrap_or_default() };
    let game = read_game(path)?;
    if game.executable.is_empty() {
        return Err("This package's manifest doesn't name an executable, so there is nothing to run.".into());
    }
    if !game.x64 {
        return Err(format!("{} isn't an x86-64 program. Xbox One and Series X|S only run 64-bit code.", game.executable));
    }
    let exe = std::fs::read(executable_of(&game, &dir)).map_err(|e| format!("Couldn't open {}: {e}", game.executable))?;
    Ok((game, exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdk_manifest() {
        let x = r#"<?xml version="1.0"?><Package xmlns:mx="http://schemas.microsoft.com/appx/2013/xbox/manifest">
<Identity Name="Contoso.Demo" Publisher="CN=Contoso" Version="1.0.2.0" ProcessorArchitecture="x64"/>
<Properties><DisplayName>Demo &amp; Co</DisplayName><PublisherDisplayName>Contoso</PublisherDisplayName></Properties>
<Applications><Application Id="App" Executable="Demo.exe" EntryPoint="Demo.App"/></Applications>
<Extensions><mx:TitleId>1A2B3C4D</mx:TitleId></Extensions></Package>"#;
        let g = parse_appx_manifest(x);
        assert_eq!(g.name, "Demo & Co");
        assert_eq!(g.identity, "Contoso.Demo");
        assert_eq!(g.version, "1.0.2.0");
        assert_eq!(g.executable, "Demo.exe");
        assert_eq!(g.title_id, "1A2B3C4D");
    }

    #[test]
    fn gdk_config() {
        let x = r#"<Game configVersion="1"><Identity Name="Contoso.Game" Publisher="CN=Contoso" Version="2.0.0.0"/>
<ExecutableList><Executable Name="Game.exe" TargetDeviceFamily="Scarlett" Id="Game"/></ExecutableList>
<ShellVisuals DefaultDisplayName="Contoso Game" PublisherDisplayName="Contoso"/><TitleId>ABCD1234</TitleId></Game>"#;
        let g = parse_game_config(x);
        assert_eq!(g.name, "Contoso Game");
        assert_eq!(g.executable, "Game.exe");
        assert_eq!(g.devices, vec!["Xbox Series X|S"]);
        assert_eq!(g.title_id, "ABCD1234");
        assert_eq!(g.kit, "GDK");
    }

    #[test]
    fn detects_encrypted_xvc() {
        let mut h = vec![0u8; 0x300];
        h[0x200..0x208].copy_from_slice(b"msft-xvd");
        assert!(is_encrypted_package(&h));
        assert!(!is_encrypted_package(&[0u8; 0x300]));
    }

    #[test]
    fn reads_a_game_folder() {
        let dir = std::env::temp_dir().join(format!("xboxone_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("appxmanifest.xml"), r#"<Package><Identity Name="T.Game" Version="1.0.0.0"/><Properties><DisplayName>T</DisplayName></Properties><Application Executable="t.exe"/></Package>"#).unwrap();
        std::fs::write(dir.join("t.exe"), pe::sample_pe(&["kernelx.dll", "d3d11_x.dll"])).unwrap();
        let g = read_game(&dir).unwrap();
        assert!(g.x64);
        assert_eq!(g.graphics(), "Direct3D 11.X");
        assert_eq!(g.imports, vec!["kernelx.dll", "d3d11_x.dll"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
