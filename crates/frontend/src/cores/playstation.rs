use super::{Core, Loaded};
use ps5::machine::Machine;
use std::path::Path;

mod worker;

pub(super) fn console_id(path: &Path) -> &'static str {
    let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
    if dir.join("sce_sys/param.json").is_file() || dir.join("param.json").is_file() {
        "ps5"
    } else {
        "ps4"
    }
}

pub(super) fn load(path: &Path) -> Result<Loaded, String> {
    let info = ps5::read_game(path)?;
    let file = if path.is_dir() { path.join("eboot.bin") } else { path.to_path_buf() };
    let bytes = std::fs::read(&file).map_err(|error| format!("Could not read the PlayStation executable: {error}"))?;
    let mut machine = Machine::load_game_image(&bytes, file.parent().unwrap_or_else(|| Path::new(".")))?;
    register_library_titles(&mut machine)?;
    let title = if info.title.is_empty() {
        path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
    } else {
        info.title
    };
    Ok(Loaded {
        core: Box::new(PlayStationCore::new(bytes, machine)),
        title,
        console: console_id(path),
        save_path: path.with_extension("sav"),
    })
}

fn register_library_titles(machine: &mut Machine) -> Result<(), String> {
    let settings = crate::settings::Settings::load();
    let mut candidates = Vec::new();
    let mut roots = Vec::new();
    for id in ["ps4", "ps5"] {
        if let Some(added) = settings.added_games.get(id) {
            candidates.extend(added.iter().map(std::path::PathBuf::from));
        }
        roots.push(crate::settings::games_dir().join(id));
        if let Some(root) = settings.rom_dirs.get(id) {
            roots.push(root.into());
        }
    }
    for root in roots {
        candidates.push(root.clone());
        if let Ok(entries) = std::fs::read_dir(root) {
            candidates.extend(entries.take(512).filter_map(Result::ok).map(|entry| entry.path()));
        }
    }
    candidates.sort();
    candidates.dedup();
    for path in candidates {
        if settings.hidden_games.iter().any(|hidden| Path::new(hidden) == path) { continue; }
        if !(path.is_dir() && path.join("eboot.bin").is_file() || path.is_file() && path.file_name().is_some_and(|name| name == "eboot.bin" || path.extension().is_some_and(|ext| ext == "elf" || ext == "oelf"))) { continue; }
        if ps5::metadata::read(&path).is_ok_and(|info| !info.title_id.is_empty()) { machine.register_title(&path)?; }
    }
    Ok(())
}

struct PlayStationCore {
    image: Vec<u8>,
    worker: worker::Worker,
    pixels: Vec<u32>,
    size: (usize, usize),
    message: Option<String>,
}

impl PlayStationCore {
    fn new(image: Vec<u8>, machine: Machine) -> Self {
        Self { image, worker: worker::Worker::new(machine), pixels: vec![0; 640 * 360], size: (640, 360), message: None }
    }

    fn refresh_frame(&mut self) {
        let (frame, message) = self.worker.update();
        if let Some(frame) = frame {
            self.pixels = frame.pixels;
            self.size = frame.size;
        }
        if message.is_some() { self.message = message; }
    }

}

impl Core for PlayStationCore {
    fn run_frame(&mut self) {
        self.refresh_frame();
        if self.message.is_none() {
            if let Err(message) = self.worker.run() { self.message = Some(message); }
        }
    }

    fn run_frame_blocking(&mut self) {
        if self.message.is_none() {
            if let Err(message) = self.worker.run_blocking() { self.message = Some(message); }
        }
        self.refresh_frame();
    }

    fn runtime_message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    fn frame(&self) -> &[u32] {
        &self.pixels
    }

    fn size(&self) -> (usize, usize) {
        self.size
    }

    fn fps(&self) -> f64 {
        60.0
    }

    fn set_buttons(&mut self, pad: usize, buttons: u16) {
        if pad != 0 {
            return;
        }
        let mapping = [0x4000, 0x2000, 0x0010_0000, 0x8, 0x10, 0x40, 0x80, 0x20, 0x8000, 0x1000, 0x400, 0x800];
        self.worker.set_buttons(mapping.iter().enumerate().fold(0, |value, (bit, mask)| {
            value | if buttons & (1 << bit) != 0 { *mask } else { 0 }
        }));
    }

    fn reset(&mut self) {
        match self.worker.reset(&self.image) {
            Ok(()) => {
                self.message = None;
                self.pixels.fill(0);
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn tv_aspect(&self) -> f32 {
        self.size.0 as f32 / self.size.1 as f32
    }

    fn take_audio(&mut self, _out: &mut Vec<f32>) {}

    fn set_sample_rate(&mut self, _rate: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program() -> Vec<u8> {
        let mut image = vec![0u8; 0x1008];
        image[..4].copy_from_slice(b"\x7fELF");
        image[4] = 2;
        image[5] = 1;
        image[16..18].copy_from_slice(&0xFE10u16.to_le_bytes());
        image[18..20].copy_from_slice(&62u16.to_le_bytes());
        image[24..32].copy_from_slice(&0x1000u64.to_le_bytes());
        image[32..40].copy_from_slice(&64u64.to_le_bytes());
        image[54..56].copy_from_slice(&56u16.to_le_bytes());
        image[56..58].copy_from_slice(&1u16.to_le_bytes());
        image[64..68].copy_from_slice(&1u32.to_le_bytes());
        image[68..72].copy_from_slice(&5u32.to_le_bytes());
        let file_size = image.len() as u64;
        image[96..104].copy_from_slice(&file_size.to_le_bytes());
        image[104..112].copy_from_slice(&0x2000u64.to_le_bytes());
        image[0x1000..0x1006].copy_from_slice(&[0xB8, 7, 0, 0, 0, 0xC3]);
        image
    }

    #[test]
    fn program_runs_reports_exit_and_resets() {
        let image = program();
        let machine = Machine::load(&image).unwrap();
        let mut core = PlayStationCore::new(image, machine);
        core.set_buttons(0, 1 | 1 << 4 | 1 << 8);
        assert_eq!(core.worker.buttons(), 0xC010);
        core.run_frame_blocking();
        assert_eq!(core.runtime_message(), Some("PlayStation program exited with code 7."));
        core.reset();
        assert!(core.runtime_message().is_none());
        core.run_frame_blocking();
        assert_eq!(core.runtime_message(), Some("PlayStation program exited with code 7."));
    }

    #[test]
    fn a_running_worker_shuts_down_when_the_player_closes() {
        let mut image = program();
        image[0x1000..0x1002].copy_from_slice(&[0xEB, 0xFE]);
        let machine = Machine::load(&image).unwrap();
        let mut core = PlayStationCore::new(image, machine);
        for _ in 0..20 { core.run_frame(); }
        core.run_frame_blocking();
        assert!(core.runtime_message().is_none());
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            drop(core);
            sender.send(()).unwrap();
        });
        receiver.recv_timeout(std::time::Duration::from_secs(2)).expect("player close must stop CPU execution");
        thread.join().unwrap();
    }

    #[test]
    fn video_out_pixels_reach_the_desktop_frame() {
        let image = program();
        let mut machine = Machine::load(&image).unwrap();
        machine.mem.map(0x50000000, 4096, true, false).unwrap();
        machine.mem.write(0x50000000, &[10, 20, 30, 255]);
        machine.video.buffers.push(0x50000000);
        machine.video.width = 1;
        machine.video.height = 1;
        machine.video.pitch = 1;
        machine.video.current = Some(0);
        machine.video.pixel_format = 0x80000000;
        machine.video.flips = 1;
        let mut core = PlayStationCore::new(image, machine);
        core.run_frame_blocking();
        assert_eq!(core.size(), (1, 1));
        assert_eq!(core.frame(), &[0x1E140A]);
    }

    #[test]
    fn ps5_game_folder_loads_runs_and_resets_through_frontend() {
        let dir = std::env::temp_dir().join(format!("ps5_live_core_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sce_sys")).unwrap();
        std::fs::write(
            dir.join("sce_sys/param.json"),
            r#"{"titleId":"PPSA01234","localizedParameters":{"en-US":{"titleName":"PS5 Live Demo"}}}"#,
        )
        .unwrap();
        let eboot = dir.join("eboot.bin");
        std::fs::write(&eboot, program()).unwrap();

        let mut loaded = crate::cores::load(&eboot).expect("PS5 folder should load cleanly");
        assert_eq!(loaded.console, "ps5");
        assert_eq!(loaded.title, "PS5 Live Demo");
        loaded.core.run_frame_blocking();
        assert_eq!(loaded.core.runtime_message(), Some("PlayStation program exited with code 7."));
        loaded.core.reset();
        assert!(loaded.core.runtime_message().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    #[ignore]
    fn real_launcher_cross_loads_registered_fortnite() {
        let launcher = std::env::var_os("HITBOY_PS4_TEST_GAME").expect("Set HITBOY_PS4_TEST_GAME to the launcher executable.");
        let target = std::env::var_os("HITBOY_PS4_TARGET").expect("Set HITBOY_PS4_TARGET to the unpacked game folder.");
        let launcher = Path::new(&launcher);
        let image = std::fs::read(launcher).unwrap();
        let mut machine = Machine::load_game(launcher).unwrap();
        let title = machine.register_title(Path::new(&target)).unwrap();
        let mut core = PlayStationCore::new(image, machine);
        let started = std::time::Instant::now();
        while core.size() != (1920, 1080) && started.elapsed().as_secs() < 150 {
            core.run_frame_blocking();
            assert!(core.runtime_message().is_none(), "{:?}", core.runtime_message());
        }
        assert_eq!(core.size(), (1920, 1080));
        println!("Real launcher menu displayed; pressing Cross.");
        core.set_buttons(0, 1);
        let started = std::time::Instant::now();
        while core.runtime_message().is_none() && started.elapsed().as_secs() < 150 { core.run_frame_blocking(); }
        let message = core.runtime_message().expect("The target must report its startup result.");
        assert!(message.starts_with(&format!("{title}:")), "{message}");
        assert!(message.contains("_sceKernelSetThreadAtexitCount"), "{message}");
        assert!(!message.contains("sceSystemServiceLaunchApp"), "{message}");
        assert_eq!(core.size(), (640, 360));
        assert!(core.frame().iter().all(|pixel| *pixel == 0));
        println!("Cross reached the target runtime, cleared the launcher frame, and reported: {message}");
    }

}
