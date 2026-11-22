//! Uniform interface over the console cores so the player works for every system.

use std::path::{Path, PathBuf};

mod ios_core;
mod playstation;
mod switch_core;
mod x360_core;
mod xboxone_core;

pub(crate) fn ps_console_id(path: &Path) -> &'static str {
    playstation::console_id(path)
}

pub trait Core {
    fn run_frame(&mut self);
    fn run_frame_blocking(&mut self) {
        self.run_frame();
    }
    fn runtime_message(&self) -> Option<&str> {
        None
    }
    /// 0RGB pixels, `size()` wide/high.
    fn frame(&self) -> &[u32];
    fn size(&self) -> (usize, usize);
    fn fps(&self) -> f64;
    /// Bitmask shared by all cores: A, B, Select, Start, Up, Down, Left, Right (bits 0-7),
    /// then X, Y, L, R (bits 8-11) for consoles that have them.
    fn set_buttons(&mut self, pad: usize, buttons: u16);
    fn reset(&mut self);
    fn save_ram(&self) -> Option<Vec<u8>> {
        None
    }
    fn load_save_ram(&mut self, _data: &[u8]) {}
    /// Display aspect ratio when "TV aspect" is enabled.
    fn tv_aspect(&self) -> f32;
    /// Moves generated interleaved-stereo samples into out.
    fn take_audio(&mut self, out: &mut Vec<f32>);
    fn set_sample_rate(&mut self, rate: u32);
    /// Stereoscopic consoles: show both eyes side by side (VR headsets, 3D displays).
    fn set_stereo(&mut self, _on: bool) {}
    /// Whether this console can save its whole state (save states and rewind).
    fn has_state(&self) -> bool {
        false
    }
    /// A snapshot of the whole machine.
    fn save_state(&mut self) -> Option<Vec<u8>> {
        None
    }
    fn load_state(&mut self, _data: &[u8]) -> Result<(), String> {
        Err("Save states aren't available for this console yet.".into())
    }
}

/// Fills in the three state methods of a `Core` impl for a type with `save_state` and
/// `load_state` of its own.
macro_rules! core_state {
    () => {
        fn has_state(&self) -> bool {
            true
        }
        fn save_state(&mut self) -> Option<Vec<u8>> {
            Some(Self::save_state(self))
        }
        fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
            Self::load_state(self, data)
        }
    };
}

/// The original Xbox booting its own BIOS. The picture is whatever the NV2A scans out.
pub struct XboxCore {
    xbox: xbox::Xbox,
    frame: Vec<u32>,
    size: (usize, usize),
    status: String,
}

/// Instructions run per 60 Hz frame (about 15 ms of work for the interpreter).
const XBOX_STEPS_PER_FRAME: u64 = 300_000;

impl Core for XboxCore {
    fn run_frame(&mut self) {
        self.xbox.run(XBOX_STEPS_PER_FRAME);
        let (px, w, h) = self.xbox.frame();
        self.frame = px;
        self.size = (w as usize, h as usize);
        if let Some(&last_post) = self.xbox.hw.post.last() {
            self.status = format!(
                "Xbox BIOS booting (POST {last_post:#04X}, {} cycles, EIP {:#010X})",
                self.xbox.hw.cycles,
                self.xbox.cpu.eip()
            );
        } else {
            self.status.clear();
        }
    }
    fn runtime_message(&self) -> Option<&str> {
        let lit = self.frame.iter().any(|&p| p & 0x00FF_FFFF != 0);
        if !lit && !self.status.is_empty() { Some(&self.status) } else { None }
    }
    fn frame(&self) -> &[u32] {
        &self.frame
    }
    fn size(&self) -> (usize, usize) {
        self.size
    }
    fn fps(&self) -> f64 {
        60.0
    }
    fn set_buttons(&mut self, _pad: usize, _buttons: u16) {}
    fn reset(&mut self) {
        let flash = std::mem::take(&mut self.xbox.hw.flash);
        let mcpx = self.xbox.hw.mcpx.take();
        let eeprom = self.xbox.hw.smbus.eeprom;
        if let Ok(x) = xbox::Xbox::new(flash, mcpx) {
            self.xbox = x;
            self.xbox.hw.smbus.eeprom = eeprom;
            self.status.clear();
        }
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, _out: &mut Vec<f32>) {}
    fn set_sample_rate(&mut self, _rate: u32) {}
}

/// Boots the Xbox with the user's flash BIOS and MCPX ROM, if both are installed.
fn boot_xbox(path: &Path) -> Option<Result<Loaded, String>> {
    let (flash, mcpx) = (bios_for("xbox")?, bios_for("xbox_mcpx")?);
    Some((|| {
        let flash = std::fs::read(flash).map_err(|e| e.to_string())?;
        let mcpx = std::fs::read(mcpx).map_err(|e| e.to_string())?;
        let mut xbox = xbox::Xbox::new(flash, Some(mcpx))?;
        if let Some(e) = bios_for("xbox_eeprom").and_then(|p| std::fs::read(p).ok()) {
            if e.len() == 256 {
                xbox.hw.smbus.eeprom.copy_from_slice(&e);
            }
        }
        let title = crate::library::clean_title(&path.file_stem().unwrap_or_default().to_string_lossy());
        Ok(Loaded { core: Box::new(XboxCore { xbox, frame: vec![0; 640 * 480], size: (640, 480), status: String::new() }), title, console: "xbox", save_path: path.with_extension("sav") })
    })())
}

impl Core for intv::Intellivision {
    fn run_frame(&mut self) {
        intv::Intellivision::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        intv::Intellivision::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        (intv::WIDTH, intv::HEIGHT)
    }
    fn fps(&self) -> f64 {
        intv::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        intv::Intellivision::set_buttons(self, pad, b)
    }
    fn reset(&mut self) {
        intv::Intellivision::reset(self)
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        intv::Intellivision::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        intv::Intellivision::set_sample_rate(self, rate)
    }
}

/// The NES plus a little input state: L flips the Famicom Disk System disk side.
pub struct NesCore {
    nes: nes::Nes,
    prev_l: bool,
}

impl Core for NesCore {
    fn has_state(&self) -> bool {
        true
    }
    fn save_state(&mut self) -> Option<Vec<u8>> {
        Some(self.nes.save_state())
    }
    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        self.nes.load_state(data)
    }
    fn run_frame(&mut self) {
        self.nes.run_frame()
    }
    fn frame(&self) -> &[u32] {
        self.nes.frame()
    }
    fn size(&self) -> (usize, usize) {
        (nes::ppu::WIDTH, nes::ppu::HEIGHT)
    }
    fn fps(&self) -> f64 {
        60.0988
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        let l = pad == 0 && b & 1024 != 0;
        if l && !self.prev_l {
            self.nes.next_disk_side();
        }
        if pad == 0 {
            self.prev_l = l;
        }
        self.nes.set_buttons(pad, b as u8)
    }
    fn reset(&mut self) {
        self.nes.reset()
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        self.nes.save_data()
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        self.nes.load_save_data(data)
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        self.nes.take_audio(out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        self.nes.set_sample_rate(rate)
    }
}

/// The Famicom Disk System BIOS the user installed (disksys.rom, 8 KB).
pub fn fds_bios() -> Option<std::path::PathBuf> {
    bios_for("fds")
}

impl Core for atari2600::Atari {
    core_state!();
    fn run_frame(&mut self) {
        atari2600::Atari::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        atari2600::Atari::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        (atari2600::WIDTH, atari2600::HEIGHT)
    }
    fn fps(&self) -> f64 {
        atari2600::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        atari2600::Atari::set_buttons(self, pad, b)
    }
    fn reset(&mut self) {
        atari2600::Atari::reset(self)
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        atari2600::Atari::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        atari2600::Atari::set_sample_rate(self, rate)
    }
}


/// Master System / Game Gear.
impl Core for sms::MasterSystem {
    core_state!();
    fn run_frame(&mut self) {
        sms::MasterSystem::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        sms::MasterSystem::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        sms::MasterSystem::size(self)
    }
    fn fps(&self) -> f64 {
        sms::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        sms::MasterSystem::set_buttons(self, pad, b)
    }
    fn reset(&mut self) {
        sms::MasterSystem::reset(self)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        sms::MasterSystem::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        sms::MasterSystem::load_save_ram(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        if self.size().0 == 160 {
            10.0 / 9.0
        } else {
            4.0 / 3.0
        }
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        sms::MasterSystem::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        sms::MasterSystem::set_sample_rate(self, rate)
    }
}

impl Core for md::Genesis {
    core_state!();
    fn run_frame(&mut self) {
        md::Genesis::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        md::Genesis::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        md::Genesis::size(self)
    }
    fn fps(&self) -> f64 {
        md::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        md::Genesis::set_buttons(self, pad, b)
    }
    fn reset(&mut self) {
        md::Genesis::reset(self)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        md::Genesis::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        md::Genesis::load_save_ram(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        md::Genesis::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        md::Genesis::set_sample_rate(self, rate)
    }
}

impl Core for vb::VirtualBoy {
    core_state!();
    fn run_frame(&mut self) {
        vb::VirtualBoy::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        vb::VirtualBoy::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        vb::VirtualBoy::size(self)
    }
    fn set_stereo(&mut self, on: bool) {
        vb::VirtualBoy::set_stereo(self, on)
    }
    fn fps(&self) -> f64 {
        vb::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        vb::VirtualBoy::set_buttons(self, pad, b)
    }
    fn reset(&mut self) {
        vb::VirtualBoy::reset(self)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        vb::VirtualBoy::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        vb::VirtualBoy::load_save_ram(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        vb::VirtualBoy::size(self).0 as f32 / vb::HEIGHT as f32
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        vb::VirtualBoy::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        vb::VirtualBoy::set_sample_rate(self, rate)
    }
}

/// WonderSwan with its picture kept upright for vertical games.
pub struct WsCore {
    ws: ws::WonderSwan,
    frame: Vec<u32>,
    size: (usize, usize),
}

impl std::ops::Deref for WsCore {
    type Target = ws::WonderSwan;
    fn deref(&self) -> &ws::WonderSwan {
        &self.ws
    }
}
impl std::ops::DerefMut for WsCore {
    fn deref_mut(&mut self) -> &mut ws::WonderSwan {
        &mut self.ws
    }
}

impl WsCore {
    fn new(rom: &[u8]) -> Result<Self, String> {
        let ws = ws::WonderSwan::new(rom)?;
        let mut c = WsCore { ws, frame: Vec::new(), size: (ws::WIDTH, ws::HEIGHT) };
        c.size = c.ws.upright_frame(&mut c.frame);
        Ok(c)
    }
}

impl Core for WsCore {
    fn has_state(&self) -> bool {
        true
    }
    fn save_state(&mut self) -> Option<Vec<u8>> {
        Some(self.ws.save_state())
    }
    fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        self.ws.load_state(data)
    }
    fn run_frame(&mut self) {
        self.ws.run_frame();
        self.size = self.ws.upright_frame(&mut self.frame);
    }
    fn frame(&self) -> &[u32] {
        &self.frame
    }
    fn size(&self) -> (usize, usize) {
        self.size
    }
    fn fps(&self) -> f64 {
        ws::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        ws::WonderSwan::set_buttons(self, pad, b)
    }
    fn reset(&mut self) {
        ws::WonderSwan::reset(self)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        ws::WonderSwan::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        ws::WonderSwan::load_save_ram(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        // Square pixels: the real screen is 224x144 (or 144x224 held vertically).
        self.size.0 as f32 / self.size.1 as f32
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        ws::WonderSwan::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        ws::WonderSwan::set_sample_rate(self, rate)
    }
}

impl Core for pce::PcEngine {
    core_state!();
    fn run_frame(&mut self) {
        pce::PcEngine::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        pce::PcEngine::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        pce::PcEngine::size(self)
    }
    fn fps(&self) -> f64 {
        pce::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        pce::PcEngine::set_buttons(self, pad, b)
    }
    fn reset(&mut self) {
        pce::PcEngine::reset(self)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        pce::PcEngine::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        pce::PcEngine::load_save_ram(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        pce::PcEngine::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        pce::PcEngine::set_sample_rate(self, rate)
    }
}

impl Core for atari7800::Atari7800 {
    core_state!();
    fn run_frame(&mut self) {
        atari7800::Atari7800::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        atari7800::Atari7800::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        (atari7800::WIDTH, atari7800::HEIGHT)
    }
    fn fps(&self) -> f64 {
        atari7800::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        atari7800::Atari7800::set_buttons(self, pad, b)
    }
    fn reset(&mut self) {
        atari7800::Atari7800::reset(self)
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        atari7800::Atari7800::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        atari7800::Atari7800::set_sample_rate(self, rate)
    }
}

impl Core for gb::GameBoy {
    core_state!();
    fn run_frame(&mut self) {
        gb::GameBoy::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        gb::GameBoy::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        (gb::WIDTH, gb::HEIGHT)
    }
    fn fps(&self) -> f64 {
        gb::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        if pad == 0 {
            gb::GameBoy::set_buttons(self, b as u8)
        }
    }
    fn reset(&mut self) {
        gb::GameBoy::reset(self)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        gb::GameBoy::save_ram(self)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        gb::GameBoy::load_save_ram(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        10.0 / 9.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        gb::GameBoy::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        gb::GameBoy::set_sample_rate(self, rate)
    }
}

impl Core for snes::Snes {
    core_state!();
    fn run_frame(&mut self) {
        snes::Snes::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        snes::Snes::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        (snes::WIDTH, snes::HEIGHT)
    }
    fn fps(&self) -> f64 {
        snes::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        use snes::button as s;
        let map = [(1u16, s::A), (2, s::B), (4, s::SELECT), (8, s::START), (16, s::UP), (32, s::DOWN), (64, s::LEFT), (128, s::RIGHT), (256, s::X), (512, s::Y), (1024, s::L), (2048, s::R)];
        let v = map.iter().filter(|(ours, _)| b & ours != 0).fold(0, |acc, (_, theirs)| acc | theirs);
        snes::Snes::set_buttons(self, pad, v)
    }
    fn reset(&mut self) {
        snes::Snes::reset(self)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        snes::Snes::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        snes::Snes::load_save_ram(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        snes::Snes::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        snes::Snes::set_sample_rate(self, rate)
    }
}

impl Core for gc::GameCube {
    fn run_frame(&mut self) {
        gc::GameCube::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        gc::GameCube::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        gc::GameCube::size(self)
    }
    fn fps(&self) -> f64 {
        gc::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        use gc::button as g;
        // The D-pad also drives the main stick; Select is Z.
        let map = [(1u16, g::A), (2, g::B), (4, g::Z), (8, g::START), (16, g::UP), (32, g::DOWN), (64, g::LEFT), (128, g::RIGHT), (256, g::X), (512, g::Y), (1024, g::L), (2048, g::R)];
        let v = map.iter().filter(|(ours, _)| b & ours != 0).fold(0, |acc, (_, theirs)| acc | theirs);
        let x = if b & 64 != 0 { -100 } else if b & 128 != 0 { 100 } else { 0 };
        let y = if b & 16 != 0 { 100 } else if b & 32 != 0 { -100 } else { 0 };
        let trig = |on: bool| if on { 255 } else { 0 };
        let pad_state = gc::Pad { buttons: v, stick: (x, y), cstick: (0, 0), triggers: (trig(b & 1024 != 0), trig(b & 2048 != 0)) };
        gc::GameCube::set_pad(self, pad, pad_state)
    }
    fn reset(&mut self) {}
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        gc::GameCube::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        gc::GameCube::set_sample_rate(self, rate)
    }
}

impl Core for n64::N64 {
    fn run_frame(&mut self) {
        n64::N64::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        n64::N64::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        n64::N64::size(self)
    }
    fn fps(&self) -> f64 {
        n64::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        use n64::button as n;
        // The D-pad drives the analog stick; X/Y are C-up/C-left, Select is Z.
        let map = [(1u16, n::A), (2, n::B), (4, n::Z), (8, n::START), (256, n::C_UP), (512, n::C_LEFT), (1024, n::L), (2048, n::R)];
        let v = map.iter().filter(|(ours, _)| b & ours != 0).fold(0, |acc, (_, theirs)| acc | theirs);
        let x = if b & 64 != 0 { -80 } else if b & 128 != 0 { 80 } else { 0 };
        let y = if b & 16 != 0 { 80 } else if b & 32 != 0 { -80 } else { 0 };
        n64::N64::set_input(self, pad, v, x, y)
    }
    fn reset(&mut self) {}
    fn save_ram(&self) -> Option<Vec<u8>> {
        Some(n64::N64::save_ram(self))
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        n64::N64::load_save_ram(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        n64::N64::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        n64::N64::set_sample_rate(self, rate)
    }
}

impl Core for gba::Gba {
    core_state!();
    fn run_frame(&mut self) {
        gba::Gba::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        gba::Gba::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        (gba::WIDTH, gba::HEIGHT)
    }
    fn fps(&self) -> f64 {
        gba::FPS
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        use gba::button as g;
        if pad != 0 {
            return;
        }
        let map = [(1u16, g::A), (2, g::B), (4, g::SELECT), (8, g::START), (16, g::UP), (32, g::DOWN), (64, g::LEFT), (128, g::RIGHT), (1024, g::L), (2048, g::R)];
        let v = map.iter().filter(|(ours, _)| b & ours != 0).fold(0, |acc, (_, theirs)| acc | theirs);
        gba::Gba::set_buttons(self, v)
    }
    fn reset(&mut self) {
        gba::Gba::reset(self)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        gba::Gba::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        gba::Gba::load_save_ram(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        3.0 / 2.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        gba::Gba::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        gba::Gba::set_sample_rate(self, rate)
    }
}

impl Core for psx::Psx {
    core_state!();
    fn run_frame(&mut self) {
        psx::Psx::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        psx::Psx::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        psx::Psx::size(self)
    }
    fn fps(&self) -> f64 {
        psx::Psx::fps(self)
    }
    fn set_buttons(&mut self, pad: usize, b: u16) {
        use psx::button as p;
        let map = [(1u16, p::CROSS), (2, p::SQUARE), (4, p::SELECT), (8, p::START), (16, p::UP), (32, p::DOWN), (64, p::LEFT), (128, p::RIGHT), (256, p::TRIANGLE), (512, p::CIRCLE), (1024, p::L1), (2048, p::R1)];
        let v = map.iter().filter(|(ours, _)| b & ours != 0).fold(0, |acc, (_, theirs)| acc | theirs);
        psx::Psx::set_buttons(self, pad, v)
    }
    fn reset(&mut self) {
        psx::Psx::reset(self)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        Some(psx::Psx::memcard(self).to_vec())
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        psx::Psx::load_memcard(self, data)
    }
    fn tv_aspect(&self) -> f32 {
        4.0 / 3.0
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        psx::Psx::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        psx::Psx::set_sample_rate(self, rate)
    }
}

/// Folder for the user's PlayStation BIOS dump: %LOCALAPPDATA%\<brand>\BIOS.
pub fn bios_dir() -> PathBuf {
    crate::settings::games_dir().parent().map(|p| p.join("BIOS")).unwrap_or_else(|| PathBuf::from("BIOS"))
}

/// A console that boots from a BIOS/firmware file the user provides.
pub struct BiosDef {
    pub id: &'static str,
    pub name: &'static str,
    /// Accepted file sizes in bytes (empty = any).
    pub sizes: &'static [u64],
    pub hint: &'static str,
    /// Whether one of our cores uses it today.
    pub used: bool,
}

pub const BIOS_DEFS: &[BiosDef] = &[
    BiosDef { id: "ps1", name: "PlayStation", sizes: &[512 * 1024], hint: "512 KB, e.g. SCPH1001.BIN (USA), SCPH7502 (Europe), SCPH5500 (Japan)", used: true },
    BiosDef { id: "fds", name: "Famicom Disk System", sizes: &[8 * 1024], hint: "8 KB disksys.rom from the RAM adapter", used: true },
    BiosDef { id: "wii", name: "Wii (common key)", sizes: &[16], hint: "16-byte common key (common-key.bin) dumped from your own Wii; it unlocks Wii discs", used: true },
    BiosDef { id: "switch", name: "Switch (keys)", sizes: &[], hint: "prod.keys dumped from your own Switch; it unlocks Switch games", used: true },
    BiosDef { id: "xbox", name: "Xbox (flash BIOS)", sizes: &[256 * 1024, 512 * 1024, 1024 * 1024], hint: "256 KB or 1 MB flash BIOS dumped from your own Xbox", used: true },
    BiosDef { id: "xbox_mcpx", name: "Xbox (MCPX boot ROM)", sizes: &[512], hint: "512 bytes mcpx_1.0.bin dumped from your own Xbox; it starts the flash BIOS", used: true },
    BiosDef { id: "xbox_eeprom", name: "Xbox (EEPROM)", sizes: &[256], hint: "256-byte eeprom.bin from your own Xbox: its serial, region and video settings", used: true },
    BiosDef { id: "intv_exec", name: "Intellivision (EXEC)", sizes: &[8 * 1024], hint: "8 KB exec.bin dumped from your own Intellivision", used: true },
    BiosDef { id: "intv_grom", name: "Intellivision (GROM)", sizes: &[2 * 1024], hint: "2 KB grom.bin dumped from your own Intellivision", used: true },
    BiosDef { id: "ps2", name: "PlayStation 2", sizes: &[4 * 1024 * 1024], hint: "4 MB, e.g. SCPH-39001.bin", used: false },
    BiosDef { id: "segacd", name: "Sega CD / Mega-CD", sizes: &[128 * 1024], hint: "128 KB, e.g. bios_CD_U.bin", used: false },
    BiosDef { id: "saturn", name: "Saturn", sizes: &[512 * 1024], hint: "512 KB, e.g. sega_101.bin", used: false },
    BiosDef { id: "dc", name: "Dreamcast", sizes: &[2 * 1024 * 1024], hint: "2 MB dc_boot.bin", used: false },
    BiosDef { id: "3do", name: "3DO", sizes: &[1024 * 1024], hint: "1 MB, e.g. panafz10.bin", used: false },
    BiosDef { id: "lynx", name: "Atari Lynx", sizes: &[512], hint: "512 bytes lynxboot.img", used: false },
    BiosDef { id: "neogeo", name: "Neo Geo", sizes: &[], hint: "neogeo.zip (AES/MVS system ROMs)", used: false },
];

fn active_file(id: &str) -> PathBuf {
    bios_dir().join(id).join("active.txt")
}

/// BIOS files installed for a console (its own folder, plus older installs in the BIOS root).
pub fn installed_bios(id: &str) -> Vec<PathBuf> {
    let Some(def) = BIOS_DEFS.iter().find(|d| d.id == id) else { return Vec::new() };
    let fits = |p: &Path| {
        p.is_file() && p.file_name().is_some_and(|n| n != "active.txt") && (def.sizes.is_empty() || std::fs::metadata(p).is_ok_and(|m| def.sizes.contains(&m.len())))
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(bios_dir().join(id)).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| fits(p)).collect();
    // Before per-console folders, BIOS files lived directly in the BIOS folder.
    let legacy = |p: &Path| match id {
        "ps1" => crate::tools::is_ps1_bios(p).is_some(),
        "fds" => p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("disksys.rom")),
        _ => false,
    };
    files.extend(std::fs::read_dir(bios_dir()).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| fits(p) && legacy(p)));
    files.sort();
    files
}

/// The BIOS a console boots with: the one chosen in Settings, else the first installed.
pub fn bios_for(id: &str) -> Option<PathBuf> {
    let files = installed_bios(id);
    let chosen = std::fs::read_to_string(active_file(id)).ok();
    chosen.and_then(|c| files.iter().find(|p| p.to_string_lossy() == c.trim()).cloned()).or_else(|| files.into_iter().next())
}

pub fn set_active_bios(id: &str, path: &Path) {
    let _ = std::fs::create_dir_all(bios_dir().join(id));
    let _ = std::fs::write(active_file(id), path.to_string_lossy().as_bytes());
}

/// Copies a BIOS into the console's folder after checking its size. Returns the new path.
pub fn install_bios(id: &str, src: &Path) -> Result<PathBuf, String> {
    let def = BIOS_DEFS.iter().find(|d| d.id == id).ok_or("Unknown console")?;
    let len = std::fs::metadata(src).map_err(|e| e.to_string())?.len();
    if !def.sizes.is_empty() && !def.sizes.contains(&len) {
        return Err(format!("That file doesn't look like a {} BIOS ({}).", def.name, def.hint));
    }
    if id == "ps1" && crate::tools::is_ps1_bios(src).is_none() {
        return Err("That 512 KB file isn't a PlayStation BIOS (no Sony header found).".into());
    }
    let dir = bios_dir().join(id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(src.file_name().ok_or("Not a file")?);
    if dest != src {
        std::fs::copy(src, &dest).map_err(|e| format!("Couldn't copy the BIOS: {e}"))?;
    }
    set_active_bios(id, &dest);
    Ok(dest)
}

/// The PlayStation BIOS to boot with.
pub fn find_ps1_bios() -> Option<PathBuf> {
    bios_for("ps1")
}

fn load_ps1(path: &Path) -> Result<Loaded, String> {
    let bios_path = find_ps1_bios().ok_or_else(|| {
        format!(
            "PlayStation games need a BIOS dumped from your own console.

Open Settings > BIOS files > PlayStation and add the file (512 KB, e.g. SCPH1001.BIN), or copy it into:
{}",
            bios_dir().display()
        )
    })?;
    let bios = std::fs::read(&bios_path).map_err(|e| format!("Can't read BIOS: {e}"))?;
    let title = crate::library::clean_title(&path.file_stem().unwrap_or_default().to_string_lossy());
    let is_exe = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    let mut core = if is_exe {
        let mut c = psx::Psx::new(bios, None)?;
        c.load_exe(std::fs::read(path).map_err(|e| e.to_string())?)?;
        c
    } else {
        psx::Psx::new(bios, Some(path))?
    };
    let save_path = path.with_extension("mcd");
    if let Ok(data) = std::fs::read(&save_path) {
        core.load_memcard(&data);
    }
    Ok(Loaded { core: Box::new(core), title, console: "ps1", save_path })
}

/// True when an .iso has the GameCube disc magic (so PlayStation .iso files still go to the PS1 core).
pub fn is_gamecube_iso(path: &Path) -> bool {
    use std::io::Read;
    let mut b = [0u8; 0x20];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut b)).is_ok() && b[0x1C..0x20] == [0xC2, 0x33, 0x9F, 0x3D]
}

/// A PlayStation 2 disc image (its SYSTEM.CNF names a BOOT2 program).
pub fn is_ps2_iso(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso")) && gamedump::identify(path).console == "PlayStation 2"
}

/// A Wii disc image (magic word at 0x18).
pub fn is_wii_iso(path: &Path) -> bool {
    use std::io::Read;
    let mut b = [0u8; 0x20];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut b)).is_ok() && b[0x18..0x1C] == [0x5D, 0x1C, 0x9E, 0xA3]
}

pub struct Loaded {
    pub core: Box<dyn Core>,
    pub title: String,
    pub console: &'static str,
    /// Where battery saves live (next to the game file).
    pub save_path: PathBuf,
}

const KNOWN: &[(&str, &str)] = &[("a26", "a2600"), ("a78", "a7800"), ("sms", "sms"), ("gg", "gg"), ("pce", "pce"), ("ws", "ws"), ("wsc", "ws"), ("int", "intv"), ("rom", "intv"), ("vb", "vb"), ("vboy", "vb"), ("md", "md"), ("gen", "md"), ("smd", "md"), ("nes", "nes"), ("fds", "nes"), ("gb", "gb"), ("gbc", "gb"), ("sfc", "snes"), ("smc", "snes"), ("gba", "gba"), ("agb", "gba"), ("z64", "n64"), ("n64", "n64"), ("v64", "n64"), ("dol", "gc"), ("gcm", "gc"), ("nsp", "switch"), ("xci", "switch"), ("ipsw", "ios"), ("ipa", "ios"), ("xbe", "xbox"), ("xiso", "xbox"), ("xex", "x360"), ("xvc", "xone"), ("msixvc", "xone"), ("msix", "xone"), ("appx", "xone"), ("msixbundle", "xone"), ("appxbundle", "xone"), ("cue", "ps1"), ("bin", "ps1"), ("iso", "ps1"), ("exe", "ps1")];

pub fn console_for(name: &str) -> Option<&'static str> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    KNOWN.iter().find(|(e, _)| *e == ext).map(|(_, c)| *c)
}

/// Console id for a game file path (zips are assumed to hold NES games unless named otherwise).
pub fn console_for_path(path: &Path) -> Option<&'static str> {
    if is_ps2_iso(path) {
        return Some("ps2");
    }
    if is_ps5_file(path) {
        return Some(playstation::console_id(path));
    }
    if is_genesis_bin(path) {
        return Some("md");
    }
    if is_small_bin(path) {
        return Some("a2600");
    }
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso")) && is_gamecube_iso(path) {
        return Some("gc");
    }
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso")) && xbox::is_xbox_disc(path) {
        return Some("xbox");
    }
    if crate::archive::is_archive(path) {
        // Look inside: the game's own file type says which console it is for.
        return crate::archive::console_of(path);
    }
    console_for(&path.to_string_lossy())
}

fn lower_ext_of(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// A PlayStation 4/5 program: an eboot.bin, a SELF/ELF file, or a game folder holding an eboot.bin.
fn is_ps5_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let ext = lower_ext_of(path);
    name == "eboot.bin" || ext == "self" || ext == "elf" || ext == "oelf" || (path.is_dir() && path.join("eboot.bin").is_file())
}

static NATIVE_NEXT_GEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Chooses how PS4/PS5 and Xbox One/Series programs run: false = on our own CPU core,
/// true = relinked to run natively on this PC.
pub fn set_native_next_gen(on: bool) {
    NATIVE_NEXT_GEN.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn native_note() -> &'static str {
    if NATIVE_NEXT_GEN.load(std::sync::atomic::Ordering::Relaxed) {
        "

Mode: Native. This runs the program's own x86-64 code on your PC's processor, redirecting its Xbox system libraries to the Windows ones."
    } else {
        "

Mode: Emulator. The program runs on our own sandboxed x86-64 core, which works on Windows, Mac, Linux and cross-architecture hosts."
    }
}

fn ps5_report(path: &Path) -> Option<String> {
    if !is_ps5_file(path) {
        return None;
    }
    if NATIVE_NEXT_GEN.load(std::sync::atomic::Ordering::Relaxed) {
        return Some(ps5_native_report(path));
    }
    None
}

/// Native mode: runs the program's own code on this PC's processor (see `ps5::native`) and
/// reports what it printed. Only reached when the player chose Native mode and opened the file.
fn ps5_native_report(path: &Path) -> String {
    let file = if path.is_dir() { path.join("eboot.bin") } else { path.to_path_buf() };
    let data = match std::fs::read(&file) {
        Ok(d) => d,
        Err(e) => return format!("Couldn't read the program: {e}"),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ps5::native::run(&data));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(15)) {
        Ok(Ok(r)) => format!(
            "The program ran natively on your PC and exited with code {}.

It printed:
{}

{}
Native mode runs the program's own code with our replacement system libraries. Graphics, audio and input libraries aren't built yet, so nothing is drawn.",
            r.exit_code,
            if r.output.is_empty() { "(nothing)".into() } else { r.output },
            if r.unimplemented.is_empty() { String::new() } else { format!("System functions it called that we don't provide yet: {}.
", r.unimplemented.join(", ")) }
        ),
        Ok(Err(e)) => e,
        Err(_) => "The program was still running after 15 seconds, so it was left running in the background. Without graphics and input libraries it can't show anything yet.".into(),
    }
}

/// Native mode for Xbox One / Series: maps the game's own x86-64 code into this process and
/// runs it on this PC's processor, with `kernelx.dll` and the Direct3D libraries redirected to
/// their Windows equivalents (see `xboxone::native`). Reports how far it got.
fn xbox_native_report(path: &Path) -> String {
    // Inspect first: this says whether the package is readable at all before we try to load it,
    // which is what turns "it crashed" into "here is exactly what's missing".
    let report = xboxone::package::inspect(path);
    match report.verdict {
        xboxone::package::Verdict::Encrypted => return report.text(),
        xboxone::package::Verdict::NotAPackage => return report.text(),
        xboxone::package::Verdict::Unusable => return report.text(),
        _ => {}
    }
    let (game, exe) = match xboxone::load_for_native(path) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(xboxone::native::run(&exe));
    });
    let r = match rx.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return e,
        Err(_) => {
            return format!(
                "{} was still running after 20 seconds and was left running in the background.

Its code executes on your PC's processor, so it is genuinely emulating; without the Xbox graphics and input libraries it cannot open a window yet.",
                game.name
            );
        }
    };
    let mut s = format!(
        "{} ran natively on this PC and exited with code {}.

The loader mapped its x86-64 code and bound {} of its imported functions to this PC's own libraries.",
        game.name,
        r.exit_code,
        r.resolved
    );
    if !r.modules.is_empty() {
        s.push_str(&format!("\n\nIt imports: {}.", r.modules.join(", ")));
    }
    if !r.output.is_empty() {
        s.push_str(&format!("\n\nIt printed:\n{}", r.output));
    }
    if !r.shimmed.is_empty() {
        s.push_str(&format!("\n\nOur own replacements answered: {}.", r.shimmed.join(", ")));
    }
    if !r.missing_modules.is_empty() {
        s.push_str(&format!("\n\nLibraries we don't have yet: {}.", r.missing_modules.join(", ")));
    }
    if !r.unimplemented.is_empty() {
        s.push_str(&format!(
            "\n\nFunctions still to implement: {}. A game that calls one of these will stop here.",
            r.unimplemented.join(", ")
        ));
    }
    s
}

/// Emulator mode for Xbox One / Series X|S: loads the decrypted PE32+ into our sandboxed 64-bit
/// address space and runs its x86-64 machine code on our built-in CPU interpreter with HLE Xbox
/// system calls (XGameRuntime, XSystem, XUser, XMemAlloc, TitleMemoryStatus, graphics bridge).
fn xbox_emu_report(path: &Path) -> String {
    let report = xboxone::package::inspect(path);
    match report.verdict {
        xboxone::package::Verdict::Encrypted
        | xboxone::package::Verdict::NotAPackage
        | xboxone::package::Verdict::Unusable => return report.text(),
        _ => {}
    }
    let (game, exe) = match xboxone::load_for_native(path) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut emu = match xboxone::x64_emu::XboxX64Emu::load_pe(&exe) {
        Ok(e) => e,
        Err(e) => return format!("{} couldn't load in Emulator mode: {e}", game.name),
    };
    let r = emu.run(5_000_000);
    let mut s = format!(
        "{} ran in our built-in x86-64 Xbox One / Series X|S emulator ({} instructions, exit code {}).",
        game.name, r.instructions, r.exit_code
    );
    if !r.output.is_empty() {
        s.push_str(&format!("\n\nConsole output:\n{}", r.output.trim_end()));
    }
    if !r.api_calls.is_empty() {
        s.push_str(&format!("\n\nXbox/Win32 calls handled: {}.", r.api_calls.join(", ")));
    }
    if !r.unimplemented_apis.is_empty() {
        s.push_str(&format!("\n\nUnimplemented calls reached: {}.", r.unimplemented_apis.join(", ")));
    }
    if let Some(reason) = r.stopped_reason {
        s.push_str(&format!("\n\nStopped: {reason}."));
    }
    s.push_str(native_note());
    s
}

/// Opens an .msix / .appx package or a .msixbundle / .appxbundle (which are zip files) into a
/// cache folder and returns the folder holding the app's manifest. A bundle holds one package
/// per architecture; the 64-bit one is used. Encrypted .msixvc/.xvc files are not zips and never
/// get here.
fn unpack_msix(path: &Path) -> Result<PathBuf, String> {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.to_string_lossy().bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    let dest = std::env::temp_dir().join(format!("{}-packages", crate::theme::t().id)).join(format!("{stem}-{h:08x}"));
    if !(dest.join("AppxManifest.xml").is_file() || dest.join("appxmanifest.xml").is_file() || dest.join("MicrosoftGame.config").is_file()) {
        let _ = std::fs::remove_dir_all(&dest);
        crate::archive::extract_all(path, &dest).map_err(|e| format!("Couldn't open the package: {e}"))?;
    }
    let has_manifest = |d: &Path| d.join("AppxManifest.xml").is_file() || d.join("appxmanifest.xml").is_file() || d.join("MicrosoftGame.config").is_file();
    if has_manifest(&dest) {
        return Ok(dest);
    }
    // A bundle: pick the x64 package (else the largest) and open that too.
    let mut inner: Vec<(u64, PathBuf)> = std::fs::read_dir(&dest)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| matches!(lower_ext_of(&e.path()).as_str(), "msix" | "appx"))
        .map(|e| (e.metadata().map(|m| m.len()).unwrap_or(0), e.path()))
        .collect();
    inner.sort_by_key(|(len, p)| (p.to_string_lossy().to_ascii_lowercase().contains("x64") as u8, *len));
    let (_, pkg) = inner.pop().ok_or("This bundle has no app package inside it.")?;
    let sub = dest.join("app");
    if !has_manifest(&sub) {
        crate::archive::extract_all(&pkg, &sub).map_err(|e| format!("Couldn't open the package inside the bundle: {e}"))?;
    }
    if has_manifest(&sub) {
        Ok(sub)
    } else {
        Err("The package has no AppxManifest.xml or MicrosoftGame.config, so it isn't a game we can read.".into())
    }
}

/// Xbox family files: read and described. Returns None when the file isn't one.
fn xbox_report(path: &Path) -> Option<String> {
    let mut ext = lower_ext_of(path);
    let unpacked;
    let path = if matches!(ext.as_str(), "msix" | "appx" | "msixbundle" | "appxbundle") {
        match unpack_msix(path) {
            Ok(dir) => {
                unpacked = dir;
                ext.clear();
                unpacked.as_path()
            }
            Err(e) => return Some(e),
        }
    } else {
        path
    };
    let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    // Xbox One / Series: encrypted packages, or a decrypted game folder's manifest.
    if ext == "xvc" || ext == "msixvc" || name == "microsoftgame.config" || name == "appxmanifest.xml" || path.is_dir() {
        if NATIVE_NEXT_GEN.load(std::sync::atomic::Ordering::Relaxed) {
            return Some(xbox_native_report(path));
        }
        return Some(xbox_emu_report(path));
    }
    // Original Xbox: boot the real machine when the user's BIOS files are installed.
    let is_xbox = ext == "xbe" || ext == "xiso" || (ext == "iso" && xbox::is_xbox_disc(path));
    if is_xbox && bios_for("xbox").is_some() && bios_for("xbox_mcpx").is_some() {
        return None;
    }
    let info = if ext == "xbe" {
        std::fs::read(path).map_err(|e| e.to_string()).and_then(|d| xbox::xbe::parse(&d))
    } else if ext == "xiso" || (ext == "iso" && xbox::is_xbox_disc(path)) {
        std::fs::File::open(path).map_err(|e| e.to_string()).and_then(|mut f| xbox::read_disc(&mut f).map(|(_, x)| x))
    } else {
        return None;
    };
    Some(match info {
        Ok(x) => format!(
            "{} ({}) was read correctly: {} sections, starting at {:#010X}.

To start the Xbox, add your console's flash BIOS and MCPX boot ROM in Settings > BIOS files. Games themselves can't play yet: the disc drive, hard disk and the NV2A's 3D engine are still being built.",
            if x.title.is_empty() { "This game".to_string() } else { x.title.clone() },
            x.title_code(),
            x.sections.len(),
            x.entry
        ),
        Err(e) => e,
    })
}

/// A .bin of cartridge size (64 KB or less) is an Atari 2600 game, not a CD image.
fn is_small_bin(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("bin")) && std::fs::metadata(path).is_ok_and(|m| m.len() <= 65536) && !is_genesis_bin(path)
}

/// A .bin with the Genesis header ("SEGA" at $100).
pub fn is_genesis_bin(path: &Path) -> bool {
    use std::io::Read;
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("bin")) {
        return false;
    }
    let mut b = [0u8; 0x110];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut b)).is_ok() && md::is_genesis_rom(&b)
}

/// If `path` is an Xbox One / Series X|S package or decrypted game folder and we are in
/// Emulator mode, loads it into the live `XboxOneCore` player.
fn try_load_xboxone(path: &Path) -> Option<Result<Loaded, String>> {
    if NATIVE_NEXT_GEN.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let mut ext = lower_ext_of(path);
    let unpacked;
    let target = if matches!(ext.as_str(), "msix" | "appx" | "msixbundle" | "appxbundle") {
        match unpack_msix(path) {
            Ok(dir) => {
                unpacked = dir;
                ext.clear();
                unpacked.as_path()
            }
            Err(e) => return Some(Err(e)),
        }
    } else {
        path
    };
    let name = target.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if ext == "xvc" || ext == "msixvc" || name == "microsoftgame.config" || name == "appxmanifest.xml" || target.is_dir() {
        return Some(xboxone_core::load(target));
    }
    None
}

/// Loads a game file (or the first game inside a .zip) with the right core.
pub fn load(path: &Path) -> Result<Loaded, String> {
    if let Some(msg) = ps5_report(path) {
        return Err(msg);
    }
    if is_ps5_file(path) {
        return playstation::load(path);
    }
    if lower_ext_of(path) == "pkg" {
        return Err("PlayStation PKG installation is not supported yet. Open a decrypted ELF/OELF executable or a game folder containing eboot.bin with the experimental PS4 core.".into());
    }
    if lower_ext_of(path) == "xex" {
        return x360_core::load(path);
    }
    if matches!(lower_ext_of(path).as_str(), "ipsw" | "ipa") {
        return ios_core::load(path);
    }
    if let Some(res) = try_load_xboxone(path) {
        return res;
    }
    if let Some(msg) = xbox_report(path) {
        return Err(msg);
    }
    let ext = lower_ext_of(path);
    if ext == "xbe" || ext == "xiso" || (ext == "iso" && xbox::is_xbox_disc(path)) {
        if let Some(r) = boot_xbox(path) {
            return r;
        }
    }
    // A data ISO wrapping a .cue/.bin game: boot the disc inside.
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso")) {
        if let Some(cue) = crate::tools::unwrap_iso_container(path)? {
            let mut loaded = load(&cue)?;
            loaded.save_path = path.with_extension("mcd");
            return Ok(loaded);
        }
    }
    // Disc images are read sector by sector, never loaded whole.
    // (A GameCube or Wii disc is also a .iso: check its header first, or it would be given to
    // the PlayStation core.)
    let is_other_disc = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso")) && (is_gamecube_iso(path) || gc::wii::is_wii_disc(path) || is_ps2_iso(path));
    if is_ps2_iso(path) {
        return Err(format!(
            "{} is a PlayStation 2 disc.

The PlayStation 2 core is in progress: the main processor (Emotion Engine) and memory map are built and tested, but the I/O processor, DMA, vector units and graphics chip are not yet, so no game can start. Add your PS2 BIOS under Settings > BIOS files when it's ready.",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    if console_for(&path.to_string_lossy()) == Some("ps1") && !is_small_bin(path) && !is_genesis_bin(path) && !is_other_disc {
        return load_ps1(path);
    }
    // Switch packages: unlock with prod.keys and boot ExeFS live on our ARM64+NEON SwitchCore.
    if lower_ext_of(path) == "nsp" || lower_ext_of(path) == "xci" {
        let key_path = bios_for("switch").ok_or("Switch games are encrypted. Add prod.keys (dumped from your own Switch) in Settings > BIOS files > Switch.")?;
        return switch_core::load(path, &key_path);
    }
    // Wii discs (.iso / .wbfs): read and decrypted with the user's common key.
    let lower_ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if (lower_ext == "iso" || lower_ext == "wbfs") && gc::wii::is_wii_disc(path) {
        let key_path = bios_for("wii").ok_or("Wii discs are encrypted. Add the Wii common key (16 bytes, dumped from your own Wii) in Settings > BIOS files > Wii.")?;
        let bytes = std::fs::read(key_path).map_err(|e| e.to_string())?;
        let key: [u8; 16] = bytes.as_slice().try_into().map_err(|_| "The Wii common key must be exactly 16 bytes.".to_string())?;
        let mut part = gc::wii::Partition::open(gc::wii::open_raw(path)?, &key)?;
        let (id, name) = part.game_info();
        return Err(format!(
            "{name} ({id}) was read and decrypted correctly.\n\nPlaying Wii games isn't possible yet: the Wii's security processor, extra memory and graphics chip are still being built on top of the GameCube core."
        ));
    }
    // GameCube discs are read on demand; programs (.dol) load whole.
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ext == "gcm" || (ext == "iso" && is_gamecube_iso(path)) || ext == "dol" {
        let mut core = if ext == "dol" {
            gc::GameCube::from_dol(&std::fs::read(path).map_err(|e| e.to_string())?)?
        } else {
            gc::GameCube::from_disc(path)?
        };
        core.set_adaptive(true);
        let disc_title = core.disc_title();
        let _ = disc_title;
        let title = crate::library::clean_title(&path.file_stem().unwrap_or_default().to_string_lossy());
        return Ok(Loaded { core: Box::new(core), title, console: "gc", save_path: path.with_extension("sav") });
    }
    let (rom, name) = if crate::archive::kind_of(path).is_some() {
        crate::archive::first_game(path)?
    } else {
        (std::fs::read(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?, path.file_name().unwrap_or_default().to_string_lossy().into_owned())
    };
    let title = crate::library::clean_title(&Path::new(&name).file_stem().unwrap_or_default().to_string_lossy());
    let mut console = console_for(&name).ok_or("Unsupported file type.")?;
    if console == "ps1" && name.to_ascii_lowercase().ends_with(".bin") {
        if md::is_genesis_rom(&rom) {
            console = "md";
        } else if rom.len() <= 65536 {
            console = "a2600";
        }
    }
    let mut core: Box<dyn Core> = match console {
        "nes" if name.to_ascii_lowercase().ends_with(".fds") => {
            let bios = fds_bios().ok_or_else(|| {
                "Famicom Disk System games need the Disk System BIOS (disksys.rom) from your own console.\n\nOpen Settings > BIOS files > Famicom Disk System and add the 8 KB file.".to_string()
            })?;
            let bios = std::fs::read(bios).map_err(|e| e.to_string())?;
            Box::new(NesCore { nes: nes::Nes::from_fds(&rom, bios)?, prev_l: false })
        }
        "nes" => Box::new(NesCore { nes: nes::Nes::from_rom(&rom).map_err(|e| format!("{title}: {e}"))?, prev_l: false }),
        "snes" => Box::new(snes::Snes::from_rom(rom)?),
        "gba" => Box::new(gba::Gba::from_rom(rom)?),
        "a2600" => Box::new(atari2600::Atari::new(&rom)?),
        "a7800" => Box::new(atari7800::Atari7800::new(&rom)?),
        "sms" => Box::new(sms::MasterSystem::new(&rom, false)?),
        "pce" => Box::new(pce::PcEngine::new(&rom)?),
        "ws" => Box::new(WsCore::new(&rom)?),
        "intv" => {
            let exec_path = bios_for("intv_exec").ok_or_else(|| {
                "Intellivision games need the EXEC BIOS (exec.bin, 8 KB) from your own console.\n\nOpen Settings > BIOS files > Intellivision (EXEC) and add it.".to_string()
            })?;
            let grom_path = bios_for("intv_grom").ok_or_else(|| {
                "Intellivision games need the GROM BIOS (grom.bin, 2 KB) from your own console.\n\nOpen Settings > BIOS files > Intellivision (GROM) and add it.".to_string()
            })?;
            let exec = std::fs::read(exec_path).map_err(|e| format!("Can't read EXEC BIOS: {e}"))?;
            let grom = std::fs::read(grom_path).map_err(|e| format!("Can't read GROM BIOS: {e}"))?;
            Box::new(intv::Intellivision::new(&exec, &grom, &rom)?)
        }
        "vb" => Box::new(vb::VirtualBoy::new(&rom)?),
        "md" => Box::new(md::Genesis::new(&rom)?),
        "gg" => Box::new(sms::MasterSystem::new(&rom, true)?),
        "n64" => Box::new(n64::N64::from_rom(rom)?),
        _ => Box::new(gb::GameBoy::from_rom(rom)?),
    };
    let save_path = path.with_extension("sav");
    if let Ok(data) = std::fs::read(&save_path) {
        core.load_save_ram(&data);
    }
    Ok(Loaded { core, title, console, save_path })
}

#[cfg(test)]
mod console_tests {
    use super::*;

    /// A GameCube image is recognised by its header whatever the file is called.
    #[test]
    fn gamecube_iso_is_not_taken_for_a_playstation_disc() {
        let dir = std::env::temp_dir().join(format!("gc_detect_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("Resident_Evil (USA) Disc 1.nkit.iso");
        let mut h = vec![0u8; 0x1000];
        h[0x1C..0x20].copy_from_slice(&[0xC2, 0x33, 0x9F, 0x3D]);
        std::fs::write(&p, h).unwrap();
        assert_eq!(console_for_path(&p), Some("gc"));
    }

    #[test]
    fn real_disc_if_present() {
        let Ok(raw) = std::env::var("HITBOY_GC_TEST_DISC") else { return };
        let p = Path::new(&raw);
        if p.exists() {
            assert_eq!(console_for_path(p), Some("gc"));
        }
    }
}

#[cfg(test)]
mod real_disc {
    use super::*;

    /// Loads Resident Evil the way the app does and checks the memory-card screen shows.
    #[test]
    #[ignore]
    fn resident_evil_reaches_its_first_menu() {
        let Ok(raw) = std::env::var("HITBOY_GC_TEST_DISC") else { return };
        let p = Path::new(&raw);
        if !p.exists() {
            return;
        }
        let mut loaded = load(p).expect("loads");
        loaded.core.set_sample_rate(48_000);
        let mut lit = 0;
        let mut audio = Vec::new();
        for f in 0..1400 {
            loaded.core.set_buttons(0, 0);
            loaded.core.run_frame();
            audio.clear();
            loaded.core.take_audio(&mut audio);
            loaded.core.take_audio(&mut audio);
            audio.clear();
            if f % 100 == 0 {
                let (w, h) = loaded.core.size();
                let l = loaded.core.frame().iter().filter(|&&p| p & 0xFFFFFF > 0x303030).count();
                eprintln!("f{f} {w}x{h} lit={l}");
            }
            if f == 1399 {
                lit = loaded.core.frame().iter().filter(|&&p| p & 0xFFFFFF > 0x303030).count();
                let (w, h) = loaded.core.size();
                eprintln!("frame size {w}x{h}, lit pixels {lit}");
            }
        }
        assert!(lit > 1000, "the menu text should be on screen, saw {lit} lit pixels");
    }
}

#[cfg(test)]
mod ps1_disc {
    use super::*;

    /// Runs a PlayStation disc from the user's folder for a while and reports what shows.
    #[test]
    #[ignore]
    fn ps1_iso_boots() {
        let Ok(path) = std::env::var("PS1_ISO") else { return };
        eprintln!("BIOS: {:?}", find_ps1_bios());
        let mut loaded = match load(Path::new(&path)) {
            Ok(l) => l,
            Err(e) => panic!("load failed: {e}"),
        };
        // PS1_PAD="frame:buttons:frames,..." (buttons in hex, in our own bit order).
        let script: Vec<(usize, u16, usize)> = std::env::var("PS1_PAD")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| {
                let p: Vec<&str> = s.split(':').collect();
                (p[0].parse().unwrap(), u16::from_str_radix(p[1], 16).unwrap(), p[2].parse().unwrap())
            })
            .collect();
        let frames: usize = std::env::var("PS1_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(1800);
        let mut audio = Vec::new();
        for f in 0..frames {
            let b = script.iter().filter(|(at, _, len)| f >= *at && f < at + len).fold(0, |acc, (_, b, _)| acc | b);
            loaded.core.set_buttons(0, b);
            loaded.core.run_frame();
            audio.clear();
            loaded.core.take_audio(&mut audio);
            if f % 100 == 0 {
                let (w, h) = loaded.core.size();
                let lit = loaded.core.frame().iter().filter(|&&p| p & 0xFFFFFF > 0x202020).count();
                eprintln!("f{f} {w}x{h} lit={lit}");
                if let Ok(dir) = std::env::var("PS1_OUT") {
                    let mut out = format!("P6 {w} {h} 255
").into_bytes();
                    for p in loaded.core.frame() {
                        out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
                    }
                    let _ = std::fs::write(format!("{dir}/ps1_{f:04}.ppm"), out);
                }
            }
        }
    }
}

#[cfg(test)]
mod msix_tests {
    use super::*;
    use std::io::Write;

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o: zip::write::SimpleFileOptions = Default::default();
            for (n, d) in files {
                z.start_file(*n, o).unwrap();
                z.write_all(d).unwrap();
            }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn msixbundle_is_opened_and_its_x64_package_read() {
        let manifest = br#"<Package><Identity Name="Test.Game" Version="1.0.0.0"/><Properties><DisplayName>Bundle Test Game</DisplayName></Properties><Application Executable="game.exe"/></Package>"#;
        let x64 = zip_of(&[("AppxManifest.xml", manifest)]);
        let arm = zip_of(&[("AppxManifest.xml", b"<Package/>")]);
        let bundle = zip_of(&[("Game_x64.msix", &x64), ("Game_arm.msix", &arm), ("AppxMetadata/AppxBundleManifest.xml", b"<Bundle/>")]);
        let dir = std::env::temp_dir().join(format!("msix-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("Game.msixbundle");
        std::fs::write(&p, bundle).unwrap();
        let out = unpack_msix(&p).unwrap();
        assert!(std::fs::read_to_string(out.join("AppxManifest.xml")).unwrap().contains("Bundle Test Game"));
        let text = xbox_report(&p).unwrap();
        assert!(text.contains("Bundle Test Game"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
