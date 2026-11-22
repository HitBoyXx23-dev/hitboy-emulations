//! The browser player: our emulation cores compiled to WebAssembly behind a small API the
//! web page drives (load a game, run frames, read pixels and sound, set buttons).

use wasm_bindgen::prelude::*;

// Keeps the C library pieces (malloc/free) our C++ CPU cores call.
use wasmlibc as _;

/// What every core offers the page.
trait Core {
    fn run_frame(&mut self);
    fn frame(&self) -> &[u32];
    fn size(&self) -> (usize, usize);
    fn fps(&self) -> f64;
    fn set_buttons(&mut self, pad: usize, b: u16);
    fn reset(&mut self);
    fn take_audio(&mut self, out: &mut Vec<f32>);
    fn save_ram(&self) -> Option<Vec<u8>>;
    fn load_save_ram(&mut self, data: &[u8]);
    fn set_sample_rate(&mut self, rate: u32);
}

impl Core for sms::MasterSystem {
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
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        sms::MasterSystem::take_audio(self, out)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        sms::MasterSystem::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        sms::MasterSystem::load_save_ram(self, data)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        sms::MasterSystem::set_sample_rate(self, rate)
    }
}

impl Core for pce::PcEngine {
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
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        pce::PcEngine::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        pce::PcEngine::set_sample_rate(self, rate)
    }
}

/// WonderSwan with its picture kept upright for vertical games.
struct WsCore {
    ws: ws::WonderSwan,
    frame: Vec<u32>,
    size: (usize, usize),
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
        self.ws.set_buttons(pad, b)
    }
    fn reset(&mut self) {
        self.ws.reset()
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        self.ws.save_ram().map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        self.ws.load_save_ram(data)
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        self.ws.take_audio(out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        self.ws.set_sample_rate(rate)
    }
}

impl Core for vb::VirtualBoy {
    fn run_frame(&mut self) {
        vb::VirtualBoy::run_frame(self)
    }
    fn frame(&self) -> &[u32] {
        vb::VirtualBoy::frame(self)
    }
    fn size(&self) -> (usize, usize) {
        (vb::WIDTH, vb::HEIGHT)
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
    
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        vb::VirtualBoy::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        vb::VirtualBoy::set_sample_rate(self, rate)
    }
}

impl Core for md::Genesis {
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
    
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        md::Genesis::take_audio(self, out)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        md::Genesis::set_sample_rate(self, rate)
    }
}

// ---- cores with C++ CPUs (built for the browser with our small C library) ----

struct NesCore {
    nes: nes::Nes,
}

impl Core for NesCore {
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
        self.nes.set_buttons(pad, b as u8)
    }
    fn reset(&mut self) {
        self.nes.reset()
    }
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        self.nes.take_audio(out)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        self.nes.save_data()
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        self.nes.load_save_data(data)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        self.nes.set_sample_rate(rate)
    }
}

impl Core for gb::GameBoy {
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
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        gb::GameBoy::take_audio(self, out)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        gb::GameBoy::save_ram(self)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        gb::GameBoy::load_save_ram(self, data)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        gb::GameBoy::set_sample_rate(self, rate)
    }
}

impl Core for gba::Gba {
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
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        gba::Gba::take_audio(self, out)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        gba::Gba::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        gba::Gba::load_save_ram(self, data)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        gba::Gba::set_sample_rate(self, rate)
    }
}

impl Core for snes::Snes {
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
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        snes::Snes::take_audio(self, out)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        snes::Snes::save_ram(self).map(<[u8]>::to_vec)
    }
    fn load_save_ram(&mut self, data: &[u8]) {
        snes::Snes::load_save_ram(self, data)
    }
    fn set_sample_rate(&mut self, rate: u32) {
        snes::Snes::set_sample_rate(self, rate)
    }
}

impl Core for atari2600::Atari {
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
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        atari2600::Atari::take_audio(self, out)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        None
    }
    fn load_save_ram(&mut self, _data: &[u8]) {}
    fn set_sample_rate(&mut self, rate: u32) {
        atari2600::Atari::set_sample_rate(self, rate)
    }
}

impl Core for atari7800::Atari7800 {
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
    fn take_audio(&mut self, out: &mut Vec<f32>) {
        atari7800::Atari7800::take_audio(self, out)
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        None
    }
    fn load_save_ram(&mut self, _data: &[u8]) {}
    fn set_sample_rate(&mut self, rate: u32) {
        atari7800::Atari7800::set_sample_rate(self, rate)
    }
}

/// Consoles the browser build can play: (file extension, console id, console name).
const SUPPORTED: &[(&str, &str, &str)] = &[("sms", "sms", "Master System"), ("gg", "gg", "Game Gear"), ("pce", "pce", "TurboGrafx-16"), ("ws", "ws", "WonderSwan"), ("wsc", "ws", "WonderSwan Color"), ("vb", "vb", "Virtual Boy"), ("md", "md", "Genesis"), ("gen", "md", "Genesis"), ("smd", "md", "Genesis"), ("nes", "nes", "NES"), ("gb", "gb", "Game Boy"), ("gbc", "gb", "Game Boy Color"), ("gba", "gba", "Game Boy Advance"), ("sfc", "snes", "SNES"), ("smc", "snes", "SNES"), ("a26", "a2600", "Atari 2600"), ("a78", "a7800", "Atari 7800")];

fn ext_of(name: &str) -> String {
    name.rsplit('.').next().unwrap_or("").to_ascii_lowercase()
}

/// Unpacks the first supported game from a .zip.
fn unzip(data: &[u8]) -> Result<(String, Vec<u8>), String> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| format!("This zip can't be opened: {e}"))?;
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().to_string();
        if SUPPORTED.iter().any(|s| s.0 == ext_of(&name)) {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut buf).map_err(|e| e.to_string())?;
            return Ok((name, buf));
        }
    }
    Err("This zip doesn't contain a game the online player supports.".into())
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = Date, js_name = now)]
    fn js_now() -> f64;
}

#[wasm_bindgen]
pub struct Emulator {
    core: Box<dyn Core>,
    console: String,
    title: String,
    audio: Vec<f32>,
    rgba: Vec<u8>,
}

#[wasm_bindgen]
impl Emulator {
    /// Loads a game from its file name and bytes; `sample_rate` is the page's audio rate.
    #[wasm_bindgen(constructor)]
    pub fn new(name: &str, data: &[u8], sample_rate: u32) -> Result<Emulator, JsValue> {
        state::set_wall_clock((js_now() / 1000.0) as i64);
        let (name, data) = if ext_of(name) == "zip" { unzip(data)? } else { (name.to_string(), data.to_vec()) };
        let ext = ext_of(&name);
        let (_, id, cname) = SUPPORTED.iter().find(|s| s.0 == ext).ok_or_else(|| JsValue::from_str(&format!("The online player can't open .{ext} files yet. Supported: {}.", supported_list())))?;
        let mut core: Box<dyn Core> = match *id {
            "sms" => Box::new(sms::MasterSystem::new(&data, false)?),
            "pce" => Box::new(pce::PcEngine::new(&data)?),
            "ws" => Box::new(WsCore::new(&data)?),
            "vb" => Box::new(vb::VirtualBoy::new(&data)?),
            "md" => Box::new(md::Genesis::new(&data)?),
            "nes" => Box::new(NesCore { nes: nes::Nes::from_rom(&data).map_err(|e| e.to_string())? }),
            "gb" => Box::new(gb::GameBoy::from_rom(data.clone())?),
            "gba" => Box::new(gba::Gba::from_rom(data.clone())?),
            "snes" => Box::new(snes::Snes::from_rom(data.clone())?),
            "a2600" => Box::new(atari2600::Atari::new(&data)?),
            "a7800" => Box::new(atari7800::Atari7800::new(&data)?),
            _ => Box::new(sms::MasterSystem::new(&data, true)?),
        };
        core.set_sample_rate(sample_rate);
        let title = name.rsplit(['/', '\\']).next().unwrap_or(&name).rsplit_once('.').map(|(t, _)| t.to_string()).unwrap_or(name.clone());
        Ok(Emulator { core, console: cname.to_string(), title, audio: Vec::new(), rgba: Vec::new() })
    }

    pub fn run_frame(&mut self) {
        self.core.run_frame();
    }

    pub fn width(&self) -> u32 {
        self.core.size().0 as u32
    }

    pub fn height(&self) -> u32 {
        self.core.size().1 as u32
    }

    pub fn fps(&self) -> f64 {
        self.core.fps()
    }

    pub fn console(&self) -> String {
        self.console.clone()
    }

    pub fn title(&self) -> String {
        self.title.clone()
    }

    /// The current picture as RGBA bytes for an ImageData.
    pub fn frame_rgba(&mut self) -> Vec<u8> {
        let px = self.core.frame();
        self.rgba.clear();
        self.rgba.reserve(px.len() * 4);
        for &p in px {
            self.rgba.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8, 255]);
        }
        self.rgba.clone()
    }

    /// Buttons bitmask: A, B, Select, Start, Up, Down, Left, Right, X, Y, L, R (bits 0-11).
    pub fn set_buttons(&mut self, pad: usize, buttons: u16) {
        self.core.set_buttons(pad, buttons);
    }

    pub fn reset(&mut self) {
        self.core.reset();
    }

    /// Interleaved stereo samples produced since the last call.
    pub fn take_audio(&mut self) -> Vec<f32> {
        self.audio.clear();
        self.core.take_audio(&mut self.audio);
        self.audio.clone()
    }

    pub fn save_ram(&self) -> Option<Vec<u8>> {
        self.core.save_ram()
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        self.core.load_save_ram(data);
    }
}

/// "Master System (.sms), Game Gear (.gg)" for the page.
#[wasm_bindgen]
pub fn supported_list() -> String {
    SUPPORTED.iter().map(|(e, _, n)| format!("{n} (.{e})")).collect::<Vec<_>>().join(", ")
}

/// Extensions for the file picker's accept attribute.
#[wasm_bindgen]
pub fn accept() -> String {
    let mut v: Vec<String> = SUPPORTED.iter().map(|(e, _, _)| format!(".{e}")).collect();
    v.push(".zip".into());
    v.join(",")
}
