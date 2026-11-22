//! Live Emulator Core for Nintendo Switch (`.nsp` / `.xci` packages decrypted with `prod.keys`).
//! Runs Horizon OS ExeFS programs frame-by-frame on our ARM64+NEON core with HID input,
//! RomFS mounting, pause, reset, and live framebuffer scanout.

use super::{Core, Loaded};
use std::path::{Path, PathBuf};
use switch::horizon::{Process, Stop};
use switch::keys::Keys;

const SWITCH_STEPS_PER_FRAME: u64 = 150_000;

pub struct SwitchCore {
    path: PathBuf,
    keys: Keys,
    process: Process,
    message: Option<String>,
    total_steps: u64,
}

impl SwitchCore {
    pub fn from_process(path: PathBuf, keys: Keys, process: Process) -> Self {
        Self {
            path,
            keys,
            process,
            message: None,
            total_steps: 0,
        }
    }

    fn format_stop(&self, stop: &Stop) -> String {
        let mut msg = format!(
            "Switch Program ID {:016X} stopped: {stop:?} ({} ARM64 instructions).",
            self.process.program_id, self.total_steps
        );
        if !self.process.debug_output.is_empty() {
            let mut tail: Vec<&str> = self.process.debug_output.lines().rev().take(10).collect();
            tail.reverse();
            msg.push_str(&format!("\n\nHorizon log output:\n{}", tail.join("\n")));
        }
        msg
    }
}

impl Core for SwitchCore {
    fn run_frame(&mut self) {
        if self.message.is_some() {
            return;
        }
        let stop = self.process.run(SWITCH_STEPS_PER_FRAME);
        self.total_steps += SWITCH_STEPS_PER_FRAME;
        if stop != Stop::Budget {
            self.message = Some(self.format_stop(&stop));
        }
    }

    fn runtime_message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    fn frame(&self) -> &[u32] {
        &self.process.framebuffer
    }

    fn size(&self) -> (usize, usize) {
        self.process.fb_size
    }

    fn fps(&self) -> f64 {
        60.0
    }

    fn set_buttons(&mut self, pad: usize, buttons: u16) {
        if pad == 0 {
            self.process.set_buttons(buttons);
        }
    }

    fn reset(&mut self) {
        match switch::launch::boot_game(&self.path, &self.keys) {
            Ok(proc) => {
                self.process = proc;
                self.message = None;
                self.total_steps = 0;
            }
            Err(e) => self.message = Some(e),
        }
    }

    fn tv_aspect(&self) -> f32 {
        16.0 / 9.0
    }

    fn take_audio(&mut self, out: &mut Vec<f32>) {
        self.process.take_audio(out);
    }

    fn set_sample_rate(&mut self, _rate: u32) {}
}

/// Loads a `.nsp` or `.xci` Switch package with `prod.keys` into the live `SwitchCore`.
pub(super) fn load(path: &Path, key_path: &Path) -> Result<Loaded, String> {
    let keys = Keys::parse(&std::fs::read_to_string(key_path).map_err(|e| e.to_string())?)?;
    let process = switch::launch::boot_game(path, &keys)?;
    let title = crate::library::clean_title(&path.file_stem().unwrap_or_default().to_string_lossy());
    Ok(Loaded {
        core: Box::new(SwitchCore::from_process(path.to_path_buf(), keys, process)),
        title,
        console: "switch",
        save_path: path.with_extension("sav"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_core_steps_maps_buttons_and_reports_exit() {
        // Minimal ARM64 NSO: mov x0, #0; svc #0x07 (ExitProcess)
        let words: [u32; 2] = [0xD280_0000, 0xD400_00E1];
        let text: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut raw = vec![0u8; 0x100];
        raw[..4].copy_from_slice(b"NSO0");
        raw[0x10..0x14].copy_from_slice(&0x100u32.to_le_bytes());
        raw[0x14..0x18].copy_from_slice(&0u32.to_le_bytes());
        raw[0x18..0x1C].copy_from_slice(&(text.len() as u32).to_le_bytes());
        raw[0x60..0x64].copy_from_slice(&(text.len() as u32).to_le_bytes());
        raw.extend_from_slice(&text);

        let nso = switch::nso::Nso::parse(&raw).unwrap();
        let mut proc = Process::load(&nso).unwrap();
        proc.program_id = 0x0100_0000_0001_0000;
        let keys = Keys::parse(
            "header_key = 0000000000000000000000000000000000000000000000000000000000000000\n\
             key_area_key_application_00 = 00000000000000000000000000000000\n",
        )
        .unwrap();
        let mut core = SwitchCore::from_process(PathBuf::new(), keys, proc);
        core.set_buttons(0, 1 | (1 << 3)); // A + Plus
        assert_eq!(core.process.pad_buttons, (1 << 0) | (1 << 10));
        core.run_frame();
        let msg = core.runtime_message().expect("ExitProcess should surface status message");
        assert!(msg.contains("Exited(0)"), "{msg}");
    }
}
