//! Live Emulator Core for PlayStation 2 (Emotion Engine + IOP + SIF BIOS boot).
//! Runs `ps2::Ps2` frame-by-frame with live BIOS console output & hardware status scanout.

use super::{Core, Loaded};
use std::path::Path;

pub struct Ps2Core {
    bios: Vec<u8>,
    ps2: ps2::Ps2,
    frame: Vec<u32>,
    frames: u64,
    status: String,
}

impl Ps2Core {
    pub fn new(bios: Vec<u8>) -> Result<Self, String> {
        let ps2 = ps2::Ps2::new(bios.clone())?;
        Ok(Self {
            bios,
            ps2,
            frame: vec![0x0008_0C18; 640 * 360],
            frames: 0,
            status: String::new(),
        })
    }
}

impl Core for Ps2Core {
    fn run_frame(&mut self) {
        self.ps2.run_frame();
        self.frames += 1;
        let sif = self.ps2.sif.borrow();
        let tail = self
            .ps2
            .bus
            .console_output
            .lines()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        self.status = if tail.is_empty() {
            format!(
                "PS2 BIOS running (frame {}, EE PC {:#010X}, IOP PC {:#010X}, SIF msflg={:#x} smflg={:#x})",
                self.frames,
                self.ps2.cpu.pc,
                self.ps2.iop.regs().pc,
                sif.msflg,
                sif.smflg
            )
        } else {
            format!(
                "PS2 BIOS running (frame {}, EE PC {:#010X}, IOP PC {:#010X})\n{tail}",
                self.frames,
                self.ps2.cpu.pc,
                self.ps2.iop.regs().pc
            )
        };
    }

    fn runtime_message(&self) -> Option<&str> {
        if self.status.is_empty() { None } else { Some(&self.status) }
    }

    fn frame(&self) -> &[u32] {
        &self.frame
    }

    fn size(&self) -> (usize, usize) {
        (640, 360)
    }

    fn fps(&self) -> f64 {
        ps2::FPS
    }

    fn set_buttons(&mut self, _pad: usize, _buttons: u16) {}

    fn reset(&mut self) {
        if let Ok(m) = ps2::Ps2::new(self.bios.clone()) {
            self.ps2 = m;
            self.frames = 0;
            self.status.clear();
        }
    }

    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }

    fn take_audio(&mut self, _out: &mut Vec<f32>) {}

    fn set_sample_rate(&mut self, _rate: u32) {}
}

/// Boots the PlayStation 2 core with the user's PS2 BIOS ROM if configured.
pub(super) fn load(path: &Path, bios_path: &Path) -> Result<Loaded, String> {
    let bios = std::fs::read(bios_path).map_err(|e| format!("Couldn't read PS2 BIOS: {e}"))?;
    let core = Ps2Core::new(bios)?;
    let title = crate::library::clean_title(&path.file_stem().unwrap_or_default().to_string_lossy());
    Ok(Loaded {
        core: Box::new(core),
        title,
        console: "ps2",
        save_path: path.with_extension("ps2"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps2_core_runs_frame_and_resets_cleanly() {
        let mut rom = vec![0u8; ps2::ROM_SIZE];
        // Spin loop at reset vector: beq r0, r0, -1; nop
        rom[0..4].copy_from_slice(&0x1000_FFFFu32.to_le_bytes());
        let mut core = Ps2Core::new(rom).unwrap();
        core.run_frame();
        assert!(core.runtime_message().is_some_and(|s| s.contains("PS2 BIOS running")));
        core.reset();
        assert!(core.runtime_message().is_none());
    }
}
