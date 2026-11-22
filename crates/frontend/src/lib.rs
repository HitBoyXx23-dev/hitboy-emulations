//! Shared desktop app. Each branded edition (crates/apps/*) calls `run` with its theme.
//!
//!   <app> [game] [--tab games|vms|settings] [--vm image]   open the GUI
//!   <app> --headless <rom> <frames> <out_dir>              scripted run: PNG snapshots + summary.txt

mod app;
mod archive;
mod controls;
mod audio;
mod cores;
mod covers;
mod discover;
mod icons;
mod input;
mod library;
mod logo;
mod platform;
pub mod remote_play;
mod tools;
mod settings;
mod shell;
pub mod theme;
mod vm;
mod vm_player;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use nes::controller::button;

pub fn run(brand: &'static theme::Brand) -> eframe::Result<()> {
    theme::set(brand);
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--headless") {
        if args.len() != 4 {
            eprintln!("usage: --headless <rom> <frames> <out_dir>");
            std::process::exit(2);
        }
        let frames = args[2].parse().expect("frames must be a number");
        if let Err(e) = run_headless(Path::new(&args[1]), frames, Path::new(&args[3])) {
            let _ = std::fs::create_dir_all(&args[3]);
            let _ = std::fs::write(Path::new(&args[3]).join("summary.txt"), format!("error: {e}"));
            std::process::exit(1);
        }
        return Ok(());
    }

    if args.first().map(String::as_str) == Some("--export-logo") {
        // Writes <dir>/<id>-mark.png and <dir>/<id>-icon.png at 512 px.
        let dir = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| ".".into()));
        let _ = std::fs::create_dir_all(&dir);
        for (tile, suffix) in [(false, "mark"), (true, "icon")] {
            if let Some((rgba, n)) = logo::rasterize(brand, 512, tile) {
                let px: Vec<u32> = rgba.chunks(4).map(|c| (c[3] as u32) << 24 | (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).collect();
                let _ = save_png_rgba(&dir.join(format!("{}-{suffix}.png", brand.id)), &px, n as usize);
            }
        }
        // Windows icon: one PNG per size inside an .ico container.
        let _ = std::fs::write(dir.join(format!("{}.ico", brand.id)), ico(brand));
        return Ok(());
    }

    let tab = args.iter().position(|a| a == "--tab").and_then(|i| args.get(i + 1)).cloned();
    let vm = args.iter().position(|a| a == "--vm").and_then(|i| args.get(i + 1)).cloned();
    let initial = args
        .iter()
        .find(|a| !a.starts_with("--") && Some(*a) != tab.as_ref() && Some(*a) != vm.as_ref())
        .map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(brand.full_name())
            .with_icon(Arc::new(logo::window_icon(brand)))
            .with_inner_size([1240.0, 800.0])
            .with_min_inner_size([860.0, 580.0]),
        vsync: settings::Settings::load().vsync,
        ..Default::default()
    };
    eframe::run_native(brand.id, options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, initial, tab.as_deref(), vm.as_deref())))))
}

/// Plays through menus with scripted input, saves a PNG every 150 frames and a timing summary.
fn run_headless(rom: &Path, frames: usize, out: &Path) -> Result<(), String> {
    let mut loaded = cores::load(rom)?;
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let start = Instant::now();
    // Audio statistics: proves the sound chips are producing a signal.
    let mut audio: Vec<f32> = Vec::new();
    let (mut sum_sq, mut peak, mut samples) = (0f64, 0f32, 0u64);
    let mut wav: Vec<i16> = Vec::new();
    for f in 0..frames {
        let mut b = 0;
        if f < 1000 && f % 200 >= 190 {
            b |= button::START;
        }
        if f >= 1000 {
            b |= button::RIGHT;
            if f % 60 < 20 {
                b |= button::A;
            }
        }
        loaded.core.set_buttons(0, b as u16);
        loaded.core.run_frame_blocking();
        if let Some(message) = loaded.core.runtime_message() {
            return Err(message.to_string());
        }
        loaded.core.take_audio(&mut audio);
        for s in audio.drain(..) {
            sum_sq += (s as f64) * (s as f64);
            peak = peak.max(s.abs());
            samples += 1;
            if wav.len() < 48_000 * 2 * 60 {
                wav.push((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            }
        }
        if (f + 1) % 150 == 0 {
            let (w, h) = loaded.core.size();
            save_png(&out.join(format!("frame_{:05}.png", f + 1)), loaded.core.frame(), w, h)?;
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let summary = format!(
        "{}: {frames} frames in {secs:.2}s ({:.0} fps, {:.1}x realtime)\naudio: {samples} samples at 48 kHz stereo ({:.1} s), RMS {:.4}, peak {:.3}\n",
        loaded.title,
        frames as f64 / secs,
        frames as f64 / secs / loaded.core.fps(),
        samples as f64 / 2.0 / 48_000.0,
        (sum_sq / samples.max(1) as f64).sqrt(),
        peak
    );
    write_wav(&out.join("audio.wav"), &wav)?;
    std::fs::write(out.join("summary.txt"), summary).map_err(|e| e.to_string())
}

/// 16-bit 48 kHz stereo PCM WAV (first minute of the run).
fn write_wav(path: &Path, pcm: &[i16]) -> Result<(), String> {
    let data_len = (pcm.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&48_000u32.to_le_bytes());
    b.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        b.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, b).map_err(|e| e.to_string())
}

fn save_png(path: &Path, frame: &[u32], w: usize, h: usize) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    let rgb: Vec<u8> = frame.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
    writer.write_image_data(&rgb).map_err(|e| e.to_string())
}

/// ARGB pixels to an RGBA PNG.
fn png_bytes(rgba: &[u8], n: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, n, n);
        enc.set_color(png::ColorType::Rgba);
        if let Ok(mut w) = enc.write_header() {
            let _ = w.write_image_data(rgba);
        }
    }
    out
}

/// A multi-size .ico (16-256 px) of the brand's app icon.
fn ico(brand: &theme::Brand) -> Vec<u8> {
    let sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let images: Vec<(u32, Vec<u8>)> = sizes.iter().filter_map(|&n| logo::rasterize(brand, n, true).map(|(rgba, _)| (n, png_bytes(&rgba, n)))).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (n, data) in &images {
        let dim = if *n >= 256 { 0 } else { *n as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

fn save_png_rgba(path: &Path, px: &[u32], n: usize) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), n as u32, n as u32);
    enc.set_color(png::ColorType::Rgba);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    let rgba: Vec<u8> = px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, (p >> 24) as u8]).collect();
    w.write_image_data(&rgba).map_err(|e| e.to_string())
}

/// A command for a background helper (such as curl) that never opens a console window.
pub(crate) fn quiet(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod ps4_runtime_tests;
