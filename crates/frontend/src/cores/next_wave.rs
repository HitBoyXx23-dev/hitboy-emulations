//! Live frontend `Core` wrappers for the next-wave consoles:
//! PSP, PlayStation Vita, Nintendo 3DS, Nintendo DS, Wii U, Wii, Dreamcast, Saturn, Neo Geo, and PS3.

use std::path::{Path, PathBuf};

use super::{bios_for, Core, Loaded};

// ---- PSP ----
pub struct PspCore {
    emu: psp::Psp,
    status: Option<String>,
}
impl Core for PspCore {
    fn run_frame(&mut self) {
        self.emu.run_frame();
        self.status = self.emu.status_message();
    }
    fn runtime_message(&self) -> Option<&str> { self.status.as_deref() }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (psp::WIDTH, psp::HEIGHT) }
    fn fps(&self) -> f64 { psp::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn reset(&mut self) { self.emu.reset(); self.status = None; }
    fn tv_aspect(&self) -> f32 { 16.0 / 9.0 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, rate: u32) { self.emu.set_sample_rate(rate); }
}

// ---- PlayStation Vita ----
pub struct VitaCore {
    emu: vita::PsVita,
}
impl Core for VitaCore {
    fn run_frame(&mut self) { self.emu.run_frame(); }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (vita::WIDTH, vita::HEIGHT) }
    fn fps(&self) -> f64 { vita::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn set_touch(&mut self, pressed: bool, x: f32, y: f32) { self.emu.set_touch(pressed, x, y); }
    fn reset(&mut self) { self.emu.reset(); }
    fn tv_aspect(&self) -> f32 { 16.0 / 9.0 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, _rate: u32) {}
}

// ---- Nintendo 3DS ----
pub struct N3dsCore {
    emu: n3ds::Nintendo3ds,
}
impl Core for N3dsCore {
    fn run_frame(&mut self) { self.emu.run_frame(); }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (n3ds::WIDTH, n3ds::HEIGHT) }
    fn fps(&self) -> f64 { n3ds::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn set_touch(&mut self, pressed: bool, x: f32, y: f32) { self.emu.set_touch(pressed, x, y); }
    fn reset(&mut self) { self.emu.reset(); }
    fn tv_aspect(&self) -> f32 { n3ds::WIDTH as f32 / n3ds::HEIGHT as f32 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, _rate: u32) {}
}

// ---- Nintendo DS ----
pub struct NdsCore {
    emu: nds::NintendoDs,
}
impl Core for NdsCore {
    fn run_frame(&mut self) { self.emu.run_frame(); }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (nds::WIDTH, nds::HEIGHT) }
    fn fps(&self) -> f64 { nds::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn set_touch(&mut self, pressed: bool, x: f32, y: f32) { self.emu.set_touch(pressed, x, y); }
    fn reset(&mut self) { self.emu.reset(); }
    fn tv_aspect(&self) -> f32 { nds::WIDTH as f32 / nds::HEIGHT as f32 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, _rate: u32) {}
}

// ---- Nintendo Wii U ----
pub struct WiiUCore {
    emu: wiiu::WiiU,
}
impl Core for WiiUCore {
    fn run_frame(&mut self) { self.emu.run_frame(); }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (wiiu::WIDTH, wiiu::HEIGHT) }
    fn fps(&self) -> f64 { wiiu::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn set_touch(&mut self, pressed: bool, x: f32, y: f32) { self.emu.set_touch(pressed, x, y); }
    fn reset(&mut self) { self.emu.reset(); }
    fn tv_aspect(&self) -> f32 { 16.0 / 9.0 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, _rate: u32) {}
}

// ---- Nintendo Wii ----
pub struct WiiCore {
    emu: wii::Wii,
}
impl Core for WiiCore {
    fn run_frame(&mut self) { self.emu.run_frame(); }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (wii::WIDTH, wii::HEIGHT) }
    fn fps(&self) -> f64 { wii::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn set_touch(&mut self, pressed: bool, x: f32, y: f32) { self.emu.set_touch(pressed, x, y); }
    fn reset(&mut self) { self.emu.reset(); }
    fn tv_aspect(&self) -> f32 { 16.0 / 9.0 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, _rate: u32) {}
}

// ---- Sega Dreamcast ----
pub struct DreamcastCore {
    emu: dreamcast::Dreamcast,
}
impl Core for DreamcastCore {
    fn run_frame(&mut self) { self.emu.run_frame(); }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (dreamcast::WIDTH, dreamcast::HEIGHT) }
    fn fps(&self) -> f64 { dreamcast::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn reset(&mut self) { self.emu.reset(); }
    fn tv_aspect(&self) -> f32 { 4.0 / 3.0 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, rate: u32) { self.emu.set_sample_rate(rate); }
}

// ---- Sega Saturn ----
pub struct SaturnCore {
    emu: saturn::Saturn,
}
impl Core for SaturnCore {
    fn run_frame(&mut self) { self.emu.run_frame(); }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (saturn::WIDTH, saturn::HEIGHT) }
    fn fps(&self) -> f64 { saturn::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn reset(&mut self) { self.emu.reset(); }
    fn tv_aspect(&self) -> f32 { 4.0 / 3.0 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, _rate: u32) {}
}

// ---- Neo Geo AES / MVS ----
pub struct NeoGeoCore {
    emu: neogeo::NeoGeo,
}
impl Core for NeoGeoCore {
    fn run_frame(&mut self) { self.emu.run_frame(); }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (neogeo::WIDTH, neogeo::HEIGHT) }
    fn fps(&self) -> f64 { neogeo::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn reset(&mut self) { self.emu.reset(); }
    fn tv_aspect(&self) -> f32 { 4.0 / 3.0 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, _rate: u32) {}
}

// ---- PlayStation 3 ----
pub struct Ps3Core {
    emu: ps3::Ps3,
    status: Option<String>,
}
impl Core for Ps3Core {
    fn run_frame(&mut self) {
        self.emu.run_frame();
        self.status = self.emu.status_message();
    }
    fn runtime_message(&self) -> Option<&str> { self.status.as_deref() }
    fn frame(&self) -> &[u32] { self.emu.frame() }
    fn size(&self) -> (usize, usize) { (ps3::WIDTH, ps3::HEIGHT) }
    fn fps(&self) -> f64 { ps3::FPS }
    fn set_buttons(&mut self, pad: usize, buttons: u16) { if pad == 0 { self.emu.set_buttons(buttons); } }
    fn reset(&mut self) { self.emu.reset(); self.status = None; }
    fn tv_aspect(&self) -> f32 { 16.0 / 9.0 }
    fn take_audio(&mut self, out: &mut Vec<f32>) { self.emu.take_audio(out); }
    fn set_sample_rate(&mut self, _rate: u32) {}
}

/// Inspects an `.elf` file header to see if it belongs to PSP, Vita, Dreamcast, Wii U, or PS3
/// (returning `None` for x86-64 PS4/PS5 ELFs so those stay on `playstation`).
pub fn elf_console_id(path: &Path) -> Option<&'static str> {
    use std::io::Read;
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("elf")) {
        return None;
    }
    let mut hdr = [0u8; 20];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut hdr)).ok()?;
    if &hdr[0..4] != b"\x7FELF" {
        return None;
    }
    let big_endian = hdr[5] == 2;
    let machine = if big_endian {
        u16::from_be_bytes([hdr[18], hdr[19]])
    } else {
        u16::from_le_bytes([hdr[18], hdr[19]])
    };
    match (big_endian, machine) {
        (false, 8) => Some("psp"),
        (false, 40) => Some("vita"),
        (false, 42) => Some("dc"),
        (true, 20) => Some("wiiu"),
        (true, 21) => Some("ps3"),
        _ => None,
    }
}

/// Detects a PSP UMD `.iso` (contains `PSP_GAME` or `UMD_DATA.BIN` near the ISO9660 volume descriptor).
pub fn is_psp_iso(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso")) {
        return false;
    }
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut buf = vec![0u8; 0x14000];
    let _ = f.seek(SeekFrom::Start(0x8000));
    let Ok(n) = f.read(&mut buf) else { return false };
    buf[..n].windows(8).any(|w| w == b"PSP_GAME") || buf[..n].windows(12).any(|w| w == b"UMD_DATA.BIN")
}

/// Detects a Sega Saturn disc image (`SEGA SEGASATURN` in first 64 KB).
pub fn is_saturn_disc(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut buf = vec![0u8; 0x10000];
    let Ok(n) = f.read(&mut buf) else { return false };
    buf[..n].windows(15).any(|w| w == b"SEGA SEGASATURN")
}

fn clean_or_stem(preferred: &str, path: &Path) -> String {
    if !preferred.trim().is_empty() {
        crate::library::clean_title(preferred)
    } else {
        crate::library::clean_title(&path.file_stem().unwrap_or_default().to_string_lossy())
    }
}

/// Attempts to load any next-wave console format (PSP, Vita, 3DS, NDS, Wii U, Dreamcast, Saturn, Neo Geo, PS3).
pub fn try_load(path: &Path) -> Option<Result<Loaded, String>> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let save_path: PathBuf = path.with_extension("sav");

    if let Some(elf_console) = elf_console_id(path) {
        return Some((|| {
            let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
            let title = clean_or_stem("", path);
            let core: Box<dyn Core> = match elf_console {
                "psp" => Box::new(PspCore { emu: psp::Psp::from_bytes(&data)?, status: None }),
                "vita" => Box::new(VitaCore { emu: vita::PsVita::from_bytes(&data)? }),
                "dc" => Box::new(DreamcastCore { emu: dreamcast::Dreamcast::from_bytes(&data, bios_for("dc").and_then(|p| std::fs::read(p).ok()))? }),
                "wiiu" => Box::new(WiiUCore { emu: wiiu::WiiU::from_bytes(&data)? }),
                "ps3" => Box::new(Ps3Core { emu: ps3::Ps3::from_bytes(&data)?, status: None }),
                _ => return Err("Unsupported ELF architecture.".into()),
            };
            Ok(Loaded { core, title, console: elf_console, save_path })
        })());
    }

    match ext.as_str() {
        "pbp" | "cso" => Some((|| {
            let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
            let emu = psp::Psp::from_bytes(&data)?;
            let title = clean_or_stem(&emu.title, path);
            Ok(Loaded { core: Box::new(PspCore { emu, status: None }), title, console: "psp", save_path })
        })()),
        "iso" if is_psp_iso(path) => Some((|| {
            let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
            let emu = psp::Psp::from_bytes(&data)?;
            let title = clean_or_stem(&emu.title, path);
            Ok(Loaded { core: Box::new(PspCore { emu, status: None }), title, console: "psp", save_path })
        })()),
        "vpk" | "velf" => Some((|| {
            let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
            let emu = vita::PsVita::from_bytes(&data)?;
            let title = clean_or_stem(&emu.title, path);
            Ok(Loaded { core: Box::new(VitaCore { emu }), title, console: "vita", save_path })
        })()),
        "3dsx" | "3ds" | "cci" | "cia" => Some((|| {
            let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
            let emu = n3ds::Nintendo3ds::from_bytes(&data)?;
            let title = clean_or_stem(&emu.title, path);
            Ok(Loaded { core: Box::new(N3dsCore { emu }), title, console: "3ds", save_path })
        })()),
        "nds" | "dsi" => Some((|| {
            let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
            let emu = nds::NintendoDs::from_bytes(&data)?;
            let title = clean_or_stem(&emu.header.title, path);
            Ok(Loaded { core: Box::new(NdsCore { emu }), title, console: "nds", save_path })
        })()),
        "rpx" | "wua" | "wud" | "wux" => Some((|| {
            let emu = wiiu::WiiU::from_path(path)?;
            let title = clean_or_stem(&emu.title, path);
            Ok(Loaded { core: Box::new(WiiUCore { emu }), title, console: "wiiu", save_path })
        })()),
        "gdi" | "cdi" => Some((|| {
            let bios = bios_for("dc").and_then(|p| std::fs::read(p).ok());
            let emu = dreamcast::Dreamcast::from_path(path, bios)?;
            let title = clean_or_stem(&emu.title, path);
            Ok(Loaded { core: Box::new(DreamcastCore { emu }), title, console: "dc", save_path })
        })()),
        "neo" => Some((|| {
            let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
            let emu = neogeo::NeoGeo::from_bytes(&data)?;
            let title = clean_or_stem(&emu.title, path);
            Ok(Loaded { core: Box::new(NeoGeoCore { emu }), title, console: "neogeo", save_path })
        })()),
        "bin" | "iso" if is_saturn_disc(path) => Some((|| {
            let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
            let emu = saturn::Saturn::from_bytes(&data)?;
            let title = clean_or_stem(&emu.title, path);
            Ok(Loaded { core: Box::new(SaturnCore { emu }), title, console: "saturn", save_path })
        })()),
        _ => None,
    }
}

/// Instantiates a next-wave console `Core` directly from in-memory bytes (used for zipped/7z/rar archives).
pub fn from_bytes_for(console: &str, data: &[u8]) -> Result<Box<dyn Core>, String> {
    Ok(match console {
        "psp" => Box::new(PspCore { emu: psp::Psp::from_bytes(data)?, status: None }),
        "vita" => Box::new(VitaCore { emu: vita::PsVita::from_bytes(data)? }),
        "3ds" => Box::new(N3dsCore { emu: n3ds::Nintendo3ds::from_bytes(data)? }),
        "nds" => Box::new(NdsCore { emu: nds::NintendoDs::from_bytes(data)? }),
        "wiiu" => Box::new(WiiUCore { emu: wiiu::WiiU::from_bytes(data)? }),
        "wii" => Box::new(WiiCore { emu: wii::Wii::from_bytes(data)? }),
        "dc" => {
            let bios = bios_for("dc").and_then(|p| std::fs::read(p).ok());
            Box::new(DreamcastCore { emu: dreamcast::Dreamcast::from_bytes(data, bios)? })
        }
        "saturn" => Box::new(SaturnCore { emu: saturn::Saturn::from_bytes(data)? }),
        "neogeo" => Box::new(NeoGeoCore { emu: neogeo::NeoGeo::from_bytes(data)? }),
        "ps3" => Box::new(Ps3Core { emu: ps3::Ps3::from_bytes(data)?, status: None }),
        _ => return Err(format!("Unsupported console id: {console}")),
    })
}

/// Boots a Wii `.dol` binary on our dedicated Broadway `WiiCore`.
#[allow(dead_code)]
pub fn load_wii_dol(path: &Path) -> Result<Loaded, String> {
    let data = std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
    let emu = wii::Wii::from_bytes(&data)?;
    let title = clean_or_stem("", path);
    Ok(Loaded {
        core: Box::new(WiiCore { emu }),
        title,
        console: "wii",
        save_path: path.with_extension("sav"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_next_wave_cores_wire_buttons_touch_frames_and_reset() {
        // 1. NDS via Core trait
        let mut nds_rom = vec![0u8; 0x300];
        nds_rom[0..8].copy_from_slice(b"NDSTEST\0");
        nds_rom[0x20..0x24].copy_from_slice(&0x200u32.to_le_bytes());
        nds_rom[0x24..0x28].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        nds_rom[0x28..0x2C].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        nds_rom[0x2C..0x30].copy_from_slice(&16u32.to_le_bytes());
        let mut nds = from_bytes_for("nds", &nds_rom).unwrap();
        assert_eq!(nds.size(), (256, 384));
        nds.set_buttons(0, 1);
        nds.set_touch(true, 0.5, 0.75);
        nds.run_frame();
        nds.reset();

        // 2. 3DS via Core trait (.3dsx)
        let mut t3ds = vec![0u8; 48];
        t3ds[0..4].copy_from_slice(b"3DSX");
        t3ds[4..6].copy_from_slice(&32u16.to_le_bytes());
        t3ds[12..16].copy_from_slice(&16u32.to_le_bytes());
        let mut n3 = from_bytes_for("3ds", &t3ds).unwrap();
        assert_eq!(n3.size(), (400, 480));
        n3.set_touch(true, 0.5, 0.75);
        n3.run_frame();
        n3.reset();

        // 3. Dreamcast via Core trait
        let dc_bin = vec![0x09, 0x00, 0xFE, 0xAF]; // NOP; BRA -2
        let mut dc = from_bytes_for("dc", &dc_bin).unwrap();
        assert_eq!(dc.size(), (640, 480));
        dc.set_buttons(0, 1);
        dc.run_frame();
        dc.reset();

        // 4. Saturn via Core trait
        let sat_bin = vec![0x00, 0x09, 0xAF, 0xFE]; // BE NOP; BRA -2
        let mut sat = from_bytes_for("saturn", &sat_bin).unwrap();
        assert_eq!(sat.size(), (320, 224));
        sat.run_frame();
        sat.reset();

        // 5. Neo Geo via Core trait
        let mut ng_bin = vec![0u8; 0x120];
        ng_bin[0..4].copy_from_slice(&0x0010_F000u32.to_be_bytes());
        ng_bin[4..8].copy_from_slice(&0x0000_0100u32.to_be_bytes());
        ng_bin[0x100..0x102].copy_from_slice(&0x60FEu16.to_be_bytes());
        let mut ng = from_bytes_for("neogeo", &ng_bin).unwrap();
        assert_eq!(ng.size(), (320, 224));
        ng.run_frame();
        ng.reset();
    }
}
