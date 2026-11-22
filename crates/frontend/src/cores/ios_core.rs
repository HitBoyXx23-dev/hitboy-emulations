//! Live Emulator Core for Apple iOS (`.ipsw` firmware kernelcache bring-up & `.ipa` apps).
//! Runs ARM64e XNU kernels incrementally on a dedicated background thread so the UI stays at
//! 60 FPS while streaming live Apple S5L UART0 serial console output onto the player screen.

use super::{Core, Loaded};
use ios::runtime::Arm64Machine;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

struct SharedState {
    pixels: Vec<u32>,
    message: Option<String>,
}

pub struct IosCore {
    kernel: Vec<u8>,
    device_tree: Option<Vec<u8>>,
    title: String,
    banner: Option<String>,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<SharedState>>,
    thread: Option<JoinHandle<()>>,
    pixels: Vec<u32>,
    message: Option<String>,
}

impl IosCore {
    pub fn start_xnu(
        title: String,
        kernel: Vec<u8>,
        device_tree: Option<Vec<u8>>,
        banner: Option<String>,
    ) -> Result<Self, String> {
        let initial = Arm64Machine::load(&kernel, device_tree.as_deref())?;
        let first_frame = initial.render_console_frame(&title, banner.as_deref());
        let state = Arc::new(Mutex::new(SharedState {
            pixels: first_frame.clone(),
            message: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = Self::spawn_worker(
            initial,
            title.clone(),
            banner.clone(),
            stop.clone(),
            state.clone(),
        );
        Ok(Self {
            kernel,
            device_tree,
            title,
            banner,
            stop,
            state,
            thread: Some(thread),
            pixels: first_frame,
            message: None,
        })
    }

    fn spawn_worker(
        mut vm: Arm64Machine,
        title: String,
        banner: Option<String>,
        stop: Arc<AtomicBool>,
        state: Arc<Mutex<SharedState>>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let halted = vm.step_budget(500_000).map(str::to_string);
                let frame = vm.render_console_frame(&title, banner.as_deref());
                if let Ok(mut guard) = state.lock() {
                    guard.pixels = frame;
                    if halted.is_some() {
                        guard.message = halted.clone();
                    }
                }
                if halted.is_some() {
                    break;
                }
            }
        })
    }

    fn stop_worker(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for IosCore {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl Core for IosCore {
    fn run_frame(&mut self) {
        if let Ok(guard) = self.state.lock() {
            self.pixels = guard.pixels.clone();
            self.message = guard.message.clone();
        }
    }

    fn runtime_message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    fn frame(&self) -> &[u32] {
        &self.pixels
    }

    fn size(&self) -> (usize, usize) {
        (640, 360)
    }

    fn fps(&self) -> f64 {
        60.0
    }

    fn set_buttons(&mut self, _pad: usize, _buttons: u16) {}

    fn reset(&mut self) {
        self.stop_worker();
        match Arm64Machine::load(&self.kernel, self.device_tree.as_deref()) {
            Ok(vm) => {
                let frame = vm.render_console_frame(&self.title, self.banner.as_deref());
                self.pixels = frame.clone();
                self.message = None;
                self.stop = Arc::new(AtomicBool::new(false));
                self.state = Arc::new(Mutex::new(SharedState {
                    pixels: frame,
                    message: None,
                }));
                self.thread = Some(Self::spawn_worker(
                    vm,
                    self.title.clone(),
                    self.banner.clone(),
                    self.stop.clone(),
                    self.state.clone(),
                ));
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

/// Loads an `.ipsw` firmware archive into the live XNU ARM64e `IosCore`, or reports `.ipa` analysis.
pub(super) fn load(path: &Path) -> Result<Loaded, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ext == "ipa" {
        let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
        let info = ios::read_ipa(&bytes)?;
        return Err(info.summary());
    }
    let images = ios::ipsw::extract_boot_images(path)?;
    let title = images.title.clone();
    let core = IosCore::start_xnu(title.clone(), images.kernel, images.device_tree, images.banner)?;
    Ok(Loaded {
        core: Box::new(core),
        title,
        console: "ios",
        save_path: path.with_extension("sav"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal ARM64 Mach-O that writes 'OK' to Apple S5L UART0 (0x23520020) and hits BRK #0.
    fn minimal_arm64_uart_macho() -> Vec<u8> {
        let mut d = vec![0u8; 0x5000];
        let put32 = |d: &mut [u8], o: usize, v: u32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        let put64 = |d: &mut [u8], o: usize, v: u64| d[o..o + 8].copy_from_slice(&v.to_le_bytes());
        put32(&mut d, 0, 0xFEED_FACF); // MH_MAGIC_64
        put32(&mut d, 4, 0x0100_000C); // CPU_TYPE_ARM64
        put32(&mut d, 16, 2);          // ncmds = 2
        put32(&mut d, 20, 72 + 272);   // sizeofcmds

        // LC_SEGMENT_64 (__TEXT at 0xFFFFFFF007004000, fileoff 0x4000, size 0x1000)
        put32(&mut d, 32, 0x19);
        put32(&mut d, 36, 72);
        d[40..46].copy_from_slice(b"__TEXT");
        put64(&mut d, 56, 0xFFFF_FFF0_0700_4000);
        put64(&mut d, 64, 0x1000);
        put64(&mut d, 72, 0x4000);
        put64(&mut d, 80, 0x1000);

        // LC_UNIXTHREAD (entry PC = 0xFFFFFFF007004000 at offset +16 + 32*8)
        let lc2 = 32 + 72;
        put32(&mut d, lc2, 0x05);
        put32(&mut d, lc2 + 4, 272);
        put32(&mut d, lc2 + 8, 6);
        put32(&mut d, lc2 + 12, 68);
        put64(&mut d, lc2 + 16 + 32 * 8, 0xFFFF_FFF0_0700_4000);

        // Code at fileoff 0x4000:
        //   movz x1, #0x2020
        //   movk x1, #0x3520, lsl #16   -> x1 = 0x35202020? wait: 0x23520020:
        //   movz x1, #0x0020
        //   movk x1, #0x3520, lsl #16
        //   movk x1, #0x0002, lsl #32   -> x1 = 0x0000_0002_3520_0020 (Apple S5L UART0 UTXH)
        //   movz w2, #'O'
        //   str w2, [x1]
        //   movz w2, #'K'
        //   str w2, [x1]
        //   brk #0
        let code: [u32; 8] = [
            0xD280_0401, // movz x1, #0x0020
            0xF2A6_A401, // movk x1, #0x3520, lsl #16
            0xF2C0_0041, // movk x1, #0x0002, lsl #32
            0x5280_09E2, // movz w2, #0x4F ('O')
            0xB900_0022, // str w2, [x1]
            0x5280_0962, // movz w2, #0x4B ('K')
            0xB900_0022, // str w2, [x1]
            0xD420_0000, // brk #0
        ];
        for (i, w) in code.iter().enumerate() {
            put32(&mut d, 0x4000 + i * 4, *w);
        }
        d
    }

    #[test]
    fn ios_core_boots_arm64_streams_uart_and_shuts_down_cleanly() {
        let kernel = minimal_arm64_uart_macho();
        let mut core = IosCore::start_xnu("iOS 16 Test".into(), kernel, None, Some("Darwin Kernel Test".into())).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while core.runtime_message().is_none() && std::time::Instant::now() < deadline {
            core.run_frame();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(core.runtime_message().is_some_and(|m| m.contains("BRK #0x0")), "{:?}", core.runtime_message());
        assert_eq!(core.size(), (640, 360));
        assert!(core.frame().iter().any(|&p| p != 0x000A_0D14), "console text pixels should be drawn");
        drop(core);
    }
}
