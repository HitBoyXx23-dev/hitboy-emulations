//! Live Emulator Core for Xbox 360 (`.xex` executables on our Xenon PowerPC core + kernel HLE).
//! Runs frame-by-frame with live XInput button injection, pause, reset, and scanout.

use super::{Core, Loaded};
use std::path::Path;
use x360::{Machine, Stop};

const X360_STEPS_PER_FRAME: u64 = 150_000;

pub struct X360Core {
    data: Vec<u8>,
    title: String,
    machine: Machine,
    message: Option<String>,
}

impl X360Core {
    pub fn new(title: String, data: Vec<u8>) -> Result<Self, String> {
        let mut machine = Machine::new();
        machine.load_xex(&data)?;
        Ok(Self {
            data,
            title,
            machine,
            message: None,
        })
    }

    fn format_stop(&self, stop: &Stop) -> String {
        let mut s = format!(
            "{} stopped: {stop:?} ({} Xenon cycles).",
            self.title, self.machine.cycles
        );
        if !self.machine.log.is_empty() {
            let tail: Vec<&str> = self.machine.log.iter().rev().take(8).rev().map(String::as_str).collect();
            s.push_str(&format!("\n\nDebug log:\n{}", tail.join("\n")));
        }
        let unimpl = self.machine.unimplemented_calls();
        if !unimpl.is_empty() {
            let names: Vec<String> = unimpl.iter().take(8).map(|(n, c)| format!("{n} ({c}x)")).collect();
            s.push_str(&format!("\n\nUnimplemented kernel calls: {}.", names.join(", ")));
        }
        s
    }
}

impl Core for X360Core {
    fn run_frame(&mut self) {
        if self.message.is_some() {
            return;
        }
        let stop = self.machine.run(X360_STEPS_PER_FRAME);
        if stop != Stop::Budget {
            self.message = Some(self.format_stop(&stop));
        }
    }

    fn runtime_message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    fn frame(&self) -> &[u32] {
        &self.machine.framebuffer
    }

    fn size(&self) -> (usize, usize) {
        self.machine.fb_size
    }

    fn fps(&self) -> f64 {
        60.0
    }

    fn set_buttons(&mut self, pad: usize, buttons: u16) {
        if pad == 0 {
            self.machine.set_buttons(buttons);
        }
    }

    fn reset(&mut self) {
        let mut m = Machine::new();
        match m.load_xex(&self.data) {
            Ok(()) => {
                self.machine = m;
                self.message = None;
            }
            Err(e) => self.message = Some(e),
        }
    }

    fn tv_aspect(&self) -> f32 {
        16.0 / 9.0
    }

    fn take_audio(&mut self, _out: &mut Vec<f32>) {}

    fn set_sample_rate(&mut self, _rate: u32) {}
}

/// Loads an unencrypted Xbox 360 `.xex` executable into the live `X360Core`.
pub(super) fn load(path: &Path) -> Result<Loaded, String> {
    let data = std::fs::read(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    let xex = x360::xex::Xex::parse(&data).map_err(|e| format!("Couldn't parse Xbox 360 XEX: {e}"))?;
    let title = xex
        .pe_name
        .clone()
        .unwrap_or_else(|| crate::library::clean_title(&path.file_stem().unwrap_or_default().to_string_lossy()));
    let core = X360Core::new(title.clone(), data)?;
    Ok(Loaded {
        core: Box::new(core),
        title,
        console: "x360",
        save_path: path.with_extension("sav"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x360_machine_maps_frontend_buttons_to_xinput() {
        let mut m = Machine::new();
        m.set_buttons((1 << 0) | (1 << 3) | (1 << 4)); // A + Start + Up
        assert_eq!(m.pad_buttons, 0x1011);
    }
}
