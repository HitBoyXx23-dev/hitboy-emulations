//! Live Emulator-mode Core for Xbox One / Series X|S titles.
//! Runs decrypted PE32+ executables on our built-in sandboxed x86-64 core (`xboxone::x64_emu`)
//! frame-by-frame with live framebuffer scanout, controller input, pause, and reset.

use super::{Core, Loaded};
use std::path::Path;
use xboxone::x64_emu::{EmuStepState, XboxX64Emu};

/// Instructions stepped per 60 Hz frame so UI stays responsive while executing smoothly.
const XBOXONE_STEPS_PER_FRAME: u64 = 120_000;

pub struct XboxOneCore {
    exe: Vec<u8>,
    name: String,
    emu: XboxX64Emu,
    pixels: Vec<u32>,
    size: (usize, usize),
    message: Option<String>,
}

impl XboxOneCore {
    pub fn new(name: String, exe: Vec<u8>) -> Result<Self, String> {
        let emu = XboxX64Emu::load_pe(&exe)?;
        let size = (emu.framebuffer.width as usize, emu.framebuffer.height as usize);
        let pixels = emu.fb_pixels_0rgb();
        Ok(Self {
            exe,
            name,
            emu,
            pixels,
            size,
            message: None,
        })
    }

    fn format_stop_message(&self, headline: String) -> String {
        let mut s = format!(
            "{headline} ({} instructions executed).",
            self.emu.total_instructions()
        );
        let out = self.emu.output().trim_end();
        if !out.is_empty() {
            s.push_str(&format!("\n\nConsole output:\n{out}"));
        }
        if !self.emu.api_calls().is_empty() {
            s.push_str(&format!("\n\nXbox/Win32 calls handled: {}.", self.emu.api_calls().join(", ")));
        }
        if !self.emu.unimplemented_apis().is_empty() {
            s.push_str(&format!(
                "\n\nUnimplemented calls reached: {}.",
                self.emu.unimplemented_apis().join(", ")
            ));
        }
        s
    }
}

impl Core for XboxOneCore {
    fn run_frame(&mut self) {
        if self.message.is_some() {
            return;
        }
        match self.emu.step_budget(XBOXONE_STEPS_PER_FRAME) {
            EmuStepState::Running => {}
            EmuStepState::Exited(code) => {
                self.message = Some(self.format_stop_message(format!(
                    "{} exited with code {code}",
                    self.name
                )));
            }
            EmuStepState::Stopped(reason) => {
                self.message = Some(self.format_stop_message(format!(
                    "{} stopped: {reason}",
                    self.name
                )));
            }
        }
        self.size = (self.emu.framebuffer.width as usize, self.emu.framebuffer.height as usize);
        self.pixels = self.emu.fb_pixels_0rgb();
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
        self.emu.set_buttons(pad, buttons);
    }

    fn reset(&mut self) {
        match XboxX64Emu::load_pe(&self.exe) {
            Ok(emu) => {
                self.size = (emu.framebuffer.width as usize, emu.framebuffer.height as usize);
                self.pixels = emu.fb_pixels_0rgb();
                self.emu = emu;
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

/// Loads a decrypted Xbox One / Series X|S folder or manifest into the live Emulator core.
pub(super) fn load(path: &Path) -> Result<Loaded, String> {
    let report = xboxone::package::inspect(path);
    match report.verdict {
        xboxone::package::Verdict::Encrypted
        | xboxone::package::Verdict::NotAPackage
        | xboxone::package::Verdict::Unusable => return Err(report.text()),
        _ => {}
    }
    let (game, exe) = xboxone::load_for_native(path)?;
    let title = if game.name.is_empty() {
        crate::library::clean_title(&path.file_stem().unwrap_or_default().to_string_lossy())
    } else {
        game.name.clone()
    };
    let core = XboxOneCore::new(title.clone(), exe)?;
    Ok(Loaded {
        core: Box::new(core),
        title,
        console: "xone",
        save_path: path.with_extension("sav"),
    })
}

#[cfg(test)]
mod tests {
    /// Builds a minimal x86-64 PE32+ executable that calls `D3D12XBOX_CreateDevice`,
    /// `OutputDebugStringA`, and returns exit code 0x2A.
    fn synthetic_xbox_pe() -> Vec<u8> {
        let mut img = vec![0u8; 0x1000];
        img[0..2].copy_from_slice(b"MZ");
        img[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        let pe = 0x80usize;
        img[pe..pe + 4].copy_from_slice(b"PE\0\0");
        img[pe + 4..pe + 6].copy_from_slice(&0x8664u16.to_le_bytes());
        img[pe + 6..pe + 8].copy_from_slice(&1u16.to_le_bytes());
        img[pe + 20..pe + 22].copy_from_slice(&0xF0u16.to_le_bytes());
        img[pe + 22..pe + 24].copy_from_slice(&0x22u16.to_le_bytes());

        let opt = pe + 24;
        img[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        img[opt + 16..opt + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        img[opt + 24..opt + 32].copy_from_slice(&0x0000_0001_4000_0000u64.to_le_bytes());
        img[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes());
        img[opt + 36..opt + 40].copy_from_slice(&0x200u32.to_le_bytes());
        img[opt + 56..opt + 60].copy_from_slice(&0x2000u32.to_le_bytes());
        img[opt + 60..opt + 64].copy_from_slice(&0x400u32.to_le_bytes());
        img[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        // Import directory at RVA 0x1200.
        img[opt + 112 + 8..opt + 112 + 12].copy_from_slice(&0x1200u32.to_le_bytes());
        img[opt + 112 + 12..opt + 112 + 16].copy_from_slice(&40u32.to_le_bytes());

        let sec = opt + 0xF0;
        img[sec..sec + 5].copy_from_slice(b".text");
        img[sec + 8..sec + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        img[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        img[sec + 16..sec + 20].copy_from_slice(&0x800u32.to_le_bytes());
        img[sec + 20..sec + 24].copy_from_slice(&0x400u32.to_le_bytes());
        img[sec + 36..sec + 40].copy_from_slice(&0xE000_0020u32.to_le_bytes());

        // Code at file offset 0x400 (RVA 0x1000):
        //   sub rsp, 0x28
        //   call [rip + (0x1260 - 0x100A)]  ; D3D12XBOX_CreateDevice
        //   lea rcx, [rip + (0x1100 - 0x1011)]
        //   call [rip + (0x1268 - 0x1017)]  ; OutputDebugStringA
        //   add rsp, 0x28
        //   mov eax, 0x2A
        //   ret
        let mut code = Vec::new();
        code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]);
        code.extend_from_slice(&[0xFF, 0x15]);
        code.extend_from_slice(&(0x1260i32 - 0x100Ai32).to_le_bytes());
        code.extend_from_slice(&[0x48, 0x8D, 0x0D]);
        code.extend_from_slice(&(0x1100i32 - 0x1011i32).to_le_bytes());
        code.extend_from_slice(&[0xFF, 0x15]);
        code.extend_from_slice(&(0x1268i32 - 0x1017i32).to_le_bytes());
        code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]);
        code.extend_from_slice(&[0xB8, 0x2A, 0x00, 0x00, 0x00, 0xC3]);
        img[0x400..0x400 + code.len()].copy_from_slice(&code);

        // Message string at file offset 0x500 (RVA 0x1100)
        let msg = b"Xbox Series X Live Frame!\0";
        img[0x500..0x500 + msg.len()].copy_from_slice(msg);

        // Import descriptor at file offset 0x600 (RVA 0x1200)
        img[0x600..0x604].copy_from_slice(&0x1240u32.to_le_bytes()); // OriginalFirstThunk
        img[0x60C..0x610].copy_from_slice(&0x1280u32.to_le_bytes()); // Name RVA
        img[0x610..0x614].copy_from_slice(&0x1260u32.to_le_bytes()); // FirstThunk (IAT)

        // ILT (RVA 0x1240) and IAT (RVA 0x1260)
        img[0x640..0x648].copy_from_slice(&0x12A0u64.to_le_bytes());
        img[0x648..0x650].copy_from_slice(&0x12C0u64.to_le_bytes());
        img[0x660..0x668].copy_from_slice(&0x12A0u64.to_le_bytes());
        img[0x668..0x670].copy_from_slice(&0x12C0u64.to_le_bytes());

        let dll = b"d3d12_x.dll\0";
        img[0x680..0x680 + dll.len()].copy_from_slice(dll);
        let fn1 = b"D3D12XBOX_CreateDevice\0";
        img[0x6A2..0x6A2 + fn1.len()].copy_from_slice(fn1);
        let fn2 = b"OutputDebugStringA\0";
        img[0x6C2..0x6C2 + fn2.len()].copy_from_slice(fn2);

        img
    }

    #[test]
    fn loads_xbox_folder_runs_live_frame_and_resets() {
        let dir = std::env::temp_dir().join(format!("xone_live_core_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("MicrosoftGame.config");
        std::fs::write(
            &cfg,
            r#"<Game configVersion="0">
  <Identity Name="SeriesTest" Publisher="CN=Dev" Version="1.0.0.0"/>
  <ShellVisuals DefaultDisplayName="Series X Demo"/>
  <ExecutableList><Executable Name="Demo.exe" TargetDeviceFamily="Scarlett"/></ExecutableList>
</Game>"#,
        )
        .unwrap();
        std::fs::write(dir.join("Demo.exe"), synthetic_xbox_pe()).unwrap();

        let mut loaded = crate::cores::load(&cfg).expect("Xbox One / Series X|S folder should load live");
        assert_eq!(loaded.console, "xone");
        assert_eq!(loaded.title, "Series X Demo");
        loaded.core.set_buttons(0, 1 | (1 << 3));
        loaded.core.run_frame();

        assert!(loaded.core.frame().iter().any(|&px| px != 0), "D3D12XBOX_CreateDevice should present a non-black frame");
        let msg = loaded.core.runtime_message().expect("should report exit code and output");
        assert!(msg.contains("exited with code 42"), "{msg}");
        assert!(msg.contains("Xbox Series X Live Frame!"), "{msg}");

        loaded.core.reset();
        assert!(loaded.core.runtime_message().is_none());
        loaded.core.run_frame();
        assert!(loaded.core.runtime_message().is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
