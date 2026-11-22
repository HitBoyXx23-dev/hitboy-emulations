//! Console catalogue and game discovery.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Playable,
    Experimental,
    Planned,
}

pub struct Console {
    pub id: &'static str,
    pub name: &'static str,
    /// Badge text on cover tiles.
    pub short: &'static str,
    pub maker: &'static str,
    pub handheld: bool,
    pub status: Status,
    pub extensions: &'static [&'static str],
    pub note: &'static str,
}

macro_rules! console {
    ($id:literal, $name:literal, $short:literal, $maker:literal, $hh:literal, $st:ident, [$($e:literal),*], $note:literal) => {
        Console { id: $id, name: $name, short: $short, maker: $maker, handheld: $hh, status: Status::$st, extensions: &[$($e),*], note: $note }
    };
}

pub const CONSOLES: &[Console] = &[
    console!("nes", "NES / Famicom", "NES", "Nintendo", false, Playable, ["nes", "fds", "zip"], "Includes Famicom Disk System games."),
    console!("snes", "SNES", "SNES", "Nintendo", false, Playable, ["sfc", "smc", "bs", "zip"], "Super Nintendo games with full sound, plus Super FX games (Star Fox, Yoshi's Island). Games using the SA-1, DSP-1 or Cx4 chips are not supported yet."),
    console!("vb", "Virtual Boy", "VB", "Nintendo", false, Playable, ["vb", "vboy", "zip"], "No BIOS needed. Red like the real console; turn on Settings > Video > Virtual Boy 3D for side-by-side VR. L/R are the triggers; X/Y the right pad."),
    console!("n64", "Nintendo 64", "N64", "Nintendo", false, Playable, ["n64", "z64", "v64", "ndd", "zip"], "Early: some 3D games run below full speed."),
    console!("gb", "Game Boy / Color", "GB", "Nintendo", true, Playable, ["gb", "gbc", "zip"], "Game Boy and Game Boy Color, with saves."),
    console!("gba", "Game Boy Advance", "GBA", "Nintendo", true, Playable, ["gba", "zip"], "No BIOS file needed."),
    console!("nds", "Nintendo DS / DSi", "NDS", "Nintendo", true, Planned, ["nds", "dsi"], "Coming soon."),
    console!("3ds", "Nintendo 3DS / 2DS", "3DS", "Nintendo", true, Planned, ["3ds", "cci", "cia"], "Coming soon."),
    console!("gc", "GameCube", "GC", "Nintendo", false, Playable, ["dol", "iso", "gcm"], "Early: homebrew runs. Store-bought games load but show a black screen until the graphics chip (GX) is built."),
    console!("wii", "Wii", "WII", "Nintendo", false, Planned, ["iso", "wbfs"], "In progress: discs are read and decrypted with your common key; games can't run yet."),
    console!("wiiu", "Wii U", "WIIU", "Nintendo", false, Planned, ["wud", "wux", "rpx", "wua"], "Coming soon. The GamePad screen will show beside the TV picture; motion can use your webcam."),
    console!("switch", "Switch", "SWITCH", "Nintendo", true, Experimental, ["nsp", "xci"], "Experimental: .nsp/.xci packages decrypt with your prod.keys and run live on our ARM64+NEON Switch core with RomFS, HID controller input, and Horizon IPC services (sm:, set, set:sys, lm, time:u, apm, appletOE, hid)."),
    console!("ps1", "PlayStation", "PS1", "Sony", false, Playable, ["cue", "bin", "iso", "exe"], "Needs a BIOS from your own console."),
    console!("ps2", "PlayStation 2", "PS2", "Sony", false, Experimental, ["iso", "chd", "7z"], "Experimental: boots your console's 4 MB PS2 BIOS live on our Emotion Engine + IOP + SIF dual-CPU core. Add your PS2 BIOS in Settings > BIOS files; Graphics Synthesizer 3D rendering is next."),
    console!("psp", "PSP / PSP Go", "PSP", "Sony", true, Planned, ["iso", "cso", "pbp"], "Coming soon."),
    console!("vita", "PlayStation Vita", "VITA", "Sony", true, Planned, ["vpk"], "Coming soon."),
    console!("ps3", "PlayStation 3", "PS3", "Sony", false, Planned, ["iso", "pkg"], "Coming later."),
    console!("ps4", "PlayStation 4", "PS4", "Sony", false, Experimental, ["pkg", "bin", "elf", "oelf", "self"], "Experimental homebrew support: open a decrypted ELF/OELF or game folder containing eboot.bin. CPU, controller buttons and VideoOut are available. PKG installation, GPU commands, audio and many system services remain unsupported."),
    console!("ps5", "PlayStation 5", "PS5", "Sony", false, Experimental, ["bin", "elf", "self"], "Experimental: open a decrypted PS5 eboot.bin, ELF or SELF on our x86-64 core with VideoOut and controller input, or use the Remote Play tab to stream from your own PS5."),
    console!("xbox", "Xbox", "XBOX", "Microsoft", false, Experimental, ["iso", "xiso", "xbe"], "Experimental: boots your console's flash BIOS and MCPX boot ROM on our Pentium III core with NV2A scanout. Disc drive, HDD and 3D acceleration are still being built."),
    console!("x360", "Xbox 360 / S / E", "X360", "Microsoft", false, Experimental, ["iso", "xex"], "Experimental: unencrypted .xex executables run live on our Xenon PowerPC core with kernel HLE and XInput controller support; Xenos 3D rendering is in progress."),
    console!("xone", "Xbox One / Series X|S", "XBOX", "Microsoft", false, Experimental, ["xvc", "msixvc", "msix", "msixbundle", "appx", "appxbundle", "config", "xml"], "Experimental: decrypted game folders and Developer Mode packages run in Native mode (SEH, DLL redirection, GDK/XDK/D3D12.X shims) or Emulator mode (sandboxed x86-64 core, Era memory manager, DXBC/tiling bridge). Encrypted retail packages cannot be unlocked."),
    console!("ios", "iPhone / iOS", "iOS", "Apple", true, Experimental, ["ipsw", "ipa"], "Experimental: unpacks real Apple .ipsw kernelcaches (IM4P + LZFSE/complzss) with Apple DeviceTree sanitization and boots XNU live on our ARM64e EL1 core with 16 KB MMU translation and Apple S5L UART0 serial console output."),
    console!("steamdeck", "Steam Deck", "DECK", "Valve", true, Planned, ["exe"], "Steam Deck games are PC games: they run on this PC through Steam, no emulation needed."),
    console!("sms", "Master System / Mark III", "SMS", "Sega", false, Playable, ["sms", "zip"], "No BIOS needed. Start = Pause; A and B are buttons 1 and 2."),
    console!("md", "Genesis / Mega Drive", "MD", "Sega", false, Playable, ["md", "gen", "smd", "bin", "zip"], "No BIOS needed. Buttons: B = A, A = B, Y = C, Start."),
    console!("segacd", "Sega CD / Mega-CD", "SCD", "Sega", false, Planned, ["cue", "chd"], "Coming soon."),
    console!("32x", "32X", "32X", "Sega", false, Planned, ["32x"], "Coming soon."),
    console!("saturn", "Saturn", "SAT", "Sega", false, Planned, ["cue", "chd"], "Coming soon."),
    console!("dc", "Dreamcast", "DC", "Sega", false, Planned, ["gdi", "cdi", "chd"], "Coming soon."),
    console!("gg", "Game Gear", "GG", "Sega", true, Playable, ["gg", "zip"], "No BIOS needed. A and B are buttons 1 and 2."),
    console!("a2600", "Atari 2600", "2600", "Atari", false, Playable, ["a26", "bin", "zip"], "Joystick games with sound. Start = Reset, Select = Select."),
    console!("a7800", "Atari 7800", "7800", "Atari", false, Playable, ["a78", "zip"], "No BIOS needed. Start = Reset, Select = Select, X = Pause."),
    console!("lynx", "Atari Lynx", "LYNX", "Atari", true, Planned, ["lnx"], "Coming soon."),
    console!("pce", "TurboGrafx-16", "PCE", "NEC", false, Playable, ["pce", "zip"], "HuCard games, no BIOS needed. A/B = buttons I/II, Start = Run."),
    console!("neogeo", "Neo Geo AES / MVS", "NEO", "SNK", false, Planned, ["zip"], "Coming soon."),
    console!("ngp", "Neo Geo Pocket / Color", "NGP", "SNK", true, Planned, ["ngp", "ngc"], "Coming soon."),
    console!("ws", "WonderSwan", "WS", "Bandai", true, Playable, ["ws", "wsc", "zip"], "Plays WonderSwan, WonderSwan Color and SwanCrystal games. No BIOS needed. X/Y/L/R are the Y buttons for games held upright."),
    console!("intv", "Intellivision", "INTV", "Mattel", false, Playable, ["int", "rom", "zip"], "Needs your console's EXEC (exec.bin, 8 KB) and GROM (grom.bin, 2 KB) in Settings > BIOS files."),
    console!("vectrex", "Vectrex", "VEC", "GCE", false, Planned, ["vec"], "Coming soon."),
    console!("3do", "3DO", "3DO", "3DO", false, Planned, ["iso", "cue", "chd"], "Coming soon."),
];

pub fn console(id: &str) -> &'static Console {
    CONSOLES.iter().find(|c| c.id == id).unwrap_or(&CONSOLES[0])
}

#[derive(Clone)]
pub struct GameEntry {
    pub title: String,
    pub path: PathBuf,
    pub size: u64,
    pub console: &'static str,
}

/// .bin is shared: cartridge-sized files (64 KB or less) are Atari 2600 games, larger ones CD images.
fn bin_fits(path: &Path, console: &str) -> bool {
    let fname = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if fname == "eboot.bin" {
        return (console == "ps4" || console == "ps5") && crate::cores::ps_console_id(path) == console;
    }
    if console == "ps5" {
        return path.extension().is_some_and(|e| e.eq_ignore_ascii_case("elf") || e.eq_ignore_ascii_case("self"));
    }
    if console == "ps4" {
        return path.extension().is_some_and(|e| {
            e.eq_ignore_ascii_case("elf")
                || e.eq_ignore_ascii_case("oelf")
                || e.eq_ignore_ascii_case("self")
                || e.eq_ignore_ascii_case("pkg")
        });
    }
    if console == "xone" {
        let n = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        return n.ends_with(".xvc") || n.ends_with(".msixvc") || n.ends_with(".msix") || n.ends_with(".msixbundle") || n.ends_with(".appx") || n.ends_with(".appxbundle") || n == "microsoftgame.config" || n == "appxmanifest.xml";
    }
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso")) {
        // Several consoles use .iso: sort them by what the disc's own header says.
        let gc = crate::cores::is_gamecube_iso(path);
        let xb = xbox::is_xbox_disc(path);
        let wii = crate::cores::is_wii_iso(path);
        let ps2 = crate::cores::is_ps2_iso(path);
        match console {
            "gc" => return gc,
            "wii" => return wii,
            "xbox" => return xb,
            "ps2" => return ps2,
            // Any other disc console: not a GameCube, Wii or Xbox disc.
            _ => return !gc && !xb && !wii && !ps2,
        }
    }
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("bin")) {
        return true;
    }
    let genesis = crate::cores::is_genesis_bin(path);
    let small = std::fs::metadata(path).is_ok_and(|m| m.len() <= 65536);
    match console {
        "md" => genesis,
        "a2600" => small && !genesis,
        "ps1" => !small && !genesis,
        _ => true,
    }
}

/// A readable title from a game's file name: drops dump-tool leftovers such as a numeric
/// "1790675259906-" prefix and a ".nkit" tag, and turns underscores into spaces.
pub fn clean_title(stem: &str) -> String {
    let mut t = stem.to_string();
    for tag in [".nkit", ".NKit", ".NKIT"] {
        if let Some(i) = t.find(tag) {
            t.truncate(i);
        }
    }
    // A long run of digits then a dash at the very start.
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits >= 6 && t[digits..].starts_with('-') {
        t = t[digits + 1..].to_string();
    }
    let t = t.replace('_', " ");
    // Collapse the runs of spaces that "__" and "___" leave.
    let mut out = String::new();
    let mut prev_space = false;
    for c in t.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(c);
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let out = out.trim().to_string();
    if out.is_empty() { stem.to_string() } else { out }
}

fn matches(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Scans a folder (two levels deep) plus individually added files.
pub fn scan(console: &Console, dir: Option<&str>, added: &[String], hidden: &[String]) -> Vec<GameEntry> {
    let mut paths: Vec<PathBuf> = added.iter().map(PathBuf::from).filter(|p| p.exists()).collect();
    // The app's own copy of added games, plus the user's chosen folder.
    let local = crate::settings::games_dir().join(console.id);
    for root in [Some(local), dir.filter(|d| !d.is_empty()).map(PathBuf::from)].into_iter().flatten() {
        let mut stack = vec![(root, 0)];
        while let Some((d, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() && depth < 1 {
                    stack.push((p, depth + 1));
                } else if crate::archive::is_archive(&p) {
                    if console.extensions.contains(&"zip") && crate::archive::console_of(&p) == Some(console.id) {
                        paths.push(p);
                    }
                } else if matches(&p, console.extensions) && bin_fits(&p, console.id) {
                    paths.push(p);
                }
            }
        }
    }
    paths.retain(|p| !hidden.iter().any(|h| Path::new(h) == p));
    paths.sort();
    paths.dedup();
    let mut games: Vec<GameEntry> = paths
        .into_iter()
        .map(|p| {
            let fname = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            let title = if fname == "microsoftgame.config" || fname == "appxmanifest.xml" {
                xboxone::read_game(&p)
                    .ok()
                    .map(|g| g.name)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        let folder = p.parent().and_then(|d| d.file_name()).unwrap_or_default().to_string_lossy();
                        clean_title(&folder)
                    })
            } else if fname == "eboot.bin" {
                ps5::metadata::read(&p)
                    .ok()
                    .map(|m| m.title)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        let folder = p.parent().and_then(|d| d.file_name()).unwrap_or_default().to_string_lossy();
                        clean_title(&folder)
                    })
            } else {
                clean_title(&p.file_stem().unwrap_or_default().to_string_lossy())
            };
            GameEntry {
                title,
                size: std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0),
                path: p,
                console: console.id,
            }
        })
        .collect();
    games.sort_by_key(|g| g.title.to_lowercase());
    games
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

#[cfg(test)]
mod title_tests {
    use super::clean_title;

    #[test]
    fn cleans_dump_tool_names() {
        assert_eq!(clean_title("1790675259906-Resident_Evil__USA___Disc_1_.nkit"), "Resident Evil USA Disc 1");
        assert_eq!(clean_title("Grand Theft Auto (USA)"), "Grand Theft Auto (USA)");
        assert_eq!(clean_title("Super Mario Bros. 3 (USA)"), "Super Mario Bros. 3 (USA)");
        assert_eq!(clean_title("12345-x"), "12345-x", "short digit runs are left alone");
    }

    #[test]
    fn scans_ps4_ps5_and_xbox_one_folders_with_clean_titles() {
        let root = std::env::temp_dir().join(format!("lib_scan_nextgen_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ps4_dir = root.join("My PS4 Game");
        let ps5_dir = root.join("My PS5 Game");
        let xone_dir = root.join("My Series X Game");
        std::fs::create_dir_all(&ps4_dir).unwrap();
        std::fs::create_dir_all(ps5_dir.join("sce_sys")).unwrap();
        std::fs::create_dir_all(&xone_dir).unwrap();

        std::fs::write(ps4_dir.join("eboot.bin"), b"stub").unwrap();
        std::fs::write(ps5_dir.join("eboot.bin"), b"stub").unwrap();
        std::fs::write(
            ps5_dir.join("sce_sys/param.json"),
            r#"{"titleId":"PPSA99999","localizedParameters":{"en-US":{"titleName":"PS5 Scanner Title"}}}"#,
        )
        .unwrap();
        std::fs::write(
            xone_dir.join("MicrosoftGame.config"),
            r#"<Game><Identity Name="X1" Version="1.0.0.0"/><ShellVisuals DefaultDisplayName="Series X Scanner Title"/></Game>"#,
        )
        .unwrap();

        let root_str = root.to_string_lossy().into_owned();
        let ps4_games = super::scan(super::console("ps4"), Some(&root_str), &[], &[]);
        assert!(ps4_games.iter().any(|g| g.title == "My PS4 Game" && g.console == "ps4"));
        assert!(!ps4_games.iter().any(|g| g.title == "PS5 Scanner Title"));

        let ps5_games = super::scan(super::console("ps5"), Some(&root_str), &[], &[]);
        assert!(ps5_games.iter().any(|g| g.title == "PS5 Scanner Title" && g.console == "ps5"));
        assert!(!ps5_games.iter().any(|g| g.title == "My PS4 Game"));

        let xone_games = super::scan(super::console("xone"), Some(&root_str), &[], &[]);
        assert!(xone_games.iter().any(|g| g.title == "Series X Scanner Title" && g.console == "xone"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
