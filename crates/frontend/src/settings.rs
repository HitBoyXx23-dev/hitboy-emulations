//! Persistent user settings, stored as JSON in %APPDATA%\emu\settings.json.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::vm::VmConfig;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyBindings {
    pub up: String,
    pub down: String,
    pub left: String,
    pub right: String,
    pub a: String,
    pub b: String,
    pub start: String,
    pub select: String,
    /// Extra buttons used by SNES games.
    pub x: String,
    pub y: String,
    pub l: String,
    pub r: String,
    /// Rapid-fire versions of A and B (held = repeated presses).
    pub turbo_a: String,
    pub turbo_b: String,
    pub fast_forward: String,
    pub pause: String,
    pub reset: String,
    pub fullscreen: String,
    /// Hold to rewind the game.
    pub rewind: String,
    /// Save states: save, load, and switch to the next of the 9 slots.
    pub save_state: String,
    pub load_state: String,
    pub next_slot: String,
    /// Controller buttons for the same actions (gilrs button names; "" = unbound).
    pub pad: PadBindings,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            up: "ArrowUp".into(),
            down: "ArrowDown".into(),
            left: "ArrowLeft".into(),
            right: "ArrowRight".into(),
            a: "X".into(),
            b: "Z".into(),
            start: "Enter".into(),
            select: "Backspace".into(),
            x: "V".into(),
            y: "N".into(),
            l: "Q".into(),
            r: "E".into(),
            turbo_a: String::new(),
            turbo_b: String::new(),
            fast_forward: "Space".into(),
            pause: "P".into(),
            reset: "F1".into(),
            fullscreen: "F11".into(),
            rewind: "R".into(),
            save_state: "F5".into(),
            load_state: "F8".into(),
            next_slot: "F6".into(),
            pad: PadBindings::default(),
        }
    }
}

impl KeyBindings {
    /// Turbo keys were added later; if their defaults clash with a key the user already
    /// uses (e.g. WASD movement), leave turbo unbound rather than double-binding.
    pub fn fix_turbo_conflicts(&mut self) {
        // Hotkeys that collide with a game button (e.g. Up = Space while fast-forward = Space)
        // would fire both at once; move the hotkey to the first free key.
        let game = [&self.up, &self.down, &self.left, &self.right, &self.a, &self.b, &self.start, &self.select].map(|s| s.clone());
        for hk in [&mut self.fast_forward, &mut self.pause, &mut self.reset, &mut self.fullscreen, &mut self.rewind, &mut self.save_state, &mut self.load_state, &mut self.next_slot] {
            if game.contains(hk) {
                hk.clear();
            }
        }
        // The new hotkeys fall back to their defaults if an old settings file lacks them or
        // they were cleared by a clash.
        let d = Self::default();
        for (cur, def) in [(&mut self.rewind, &d.rewind), (&mut self.save_state, &d.save_state), (&mut self.load_state, &d.load_state), (&mut self.next_slot, &d.next_slot)] {
            if cur.is_empty() && !game.contains(def) {
                *cur = def.clone();
            }
        }
        for cand in ["F", "G", "H", "F2", "F3", "F4", "F5", "F6"] {
            let taken = |k: &Self| {
                [&k.up, &k.down, &k.left, &k.right, &k.a, &k.b, &k.start, &k.select, &k.turbo_a, &k.turbo_b, &k.fast_forward, &k.pause, &k.reset, &k.fullscreen]
                    .iter()
                    .any(|v| v.as_str() == cand)
            };
            if taken(self) {
                continue;
            }
            if self.fast_forward.is_empty() {
                self.fast_forward = cand.into();
            } else if self.pause.is_empty() {
                self.pause = cand.into();
            } else if self.reset.is_empty() {
                self.reset = cand.into();
            } else if self.fullscreen.is_empty() {
                self.fullscreen = cand.into();
            }
        }
        let used = [&self.up, &self.down, &self.left, &self.right, &self.a, &self.b, &self.start, &self.select, &self.fast_forward, &self.pause, &self.reset, &self.fullscreen]
            .map(|s| s.clone());
        for extra in [&mut self.x, &mut self.y, &mut self.l, &mut self.r] {
            if used.contains(extra) {
                extra.clear();
            }
        }
        if used.contains(&self.turbo_a) {
            self.turbo_a.clear();
        }
        if used.contains(&self.turbo_b) || self.turbo_b == self.turbo_a && !self.turbo_b.is_empty() {
            self.turbo_b.clear();
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PadBindings {
    pub up: String,
    pub down: String,
    pub left: String,
    pub right: String,
    pub a: String,
    pub b: String,
    pub start: String,
    pub select: String,
    pub x: String,
    pub y: String,
    pub l: String,
    pub r: String,
    pub turbo_a: String,
    pub turbo_b: String,
    /// Hold to rewind.
    pub rewind: String,
}

impl Default for PadBindings {
    /// Xbox/PlayStation style: bottom = A (jump), left = B (run), top/right = turbo.
    fn default() -> Self {
        Self {
            up: "DPadUp".into(),
            down: "DPadDown".into(),
            left: "DPadLeft".into(),
            right: "DPadRight".into(),
            a: "South".into(),
            b: "West".into(),
            start: "Start".into(),
            select: "Select".into(),
            x: "North".into(),
            y: "East".into(),
            l: "LeftTrigger".into(),
            r: "RightTrigger".into(),
            turbo_a: "RightTrigger2".into(),
            turbo_b: "LeftTrigger2".into(),
            rewind: "LeftThumb".into(),
        }
    }
}

impl PadBindings {
    /// Settings saved before X/Y/L/R existed had turbo on face buttons; free them up.
    pub fn fix_conflicts(&mut self) {
        let face = [&self.up, &self.down, &self.left, &self.right, &self.a, &self.b, &self.start, &self.select, &self.x, &self.y, &self.l, &self.r].map(|s| s.clone());
        if face.contains(&self.turbo_a) {
            self.turbo_a = "RightTrigger2".into();
        }
        if face.contains(&self.turbo_b) {
            self.turbo_b = "LeftTrigger2".into();
        }
    }

    /// Nintendo positional layout: right = A, bottom = B.
    pub fn nintendo() -> Self {
        Self { a: "East".into(), b: "South".into(), x: "North".into(), y: "West".into(), ..Self::default() }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RecentGame {
    pub path: String,
    pub console: String,
    pub title: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Aspect {
    /// Square pixels, 256x240 (8:7-ish).
    #[default]
    Square,
    /// Stretch to the 4:3 a CRT TV showed.
    Tv,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub keys: KeyBindings,
    pub gamepad_enabled: bool,
    pub stick_as_dpad: bool,
    pub stick_deadzone: f32,
    /// Nintendo-style layout: right face button = A.
    pub nintendo_layout: bool,
    /// Pick controller buttons automatically for each console (positions that match the
    /// real pad). Off = use `keys.pad` for every game.
    pub auto_pad_layout: bool,
    /// Set once the old layout choice has been carried over.
    pub pad_migrated: bool,
    /// Rewind: keep recent moments so holding the rewind button steps back through them.
    pub rewind_enabled: bool,
    pub rewind_seconds: u32,

    pub integer_scaling: bool,
    pub smooth_filter: bool,
    /// Sharp pixels: integer pre-scale then smooth, so pixels stay square at any window size.
    pub sharp_pixels: bool,
    /// Virtual Boy: both eyes side by side for VR headsets and 3D displays.
    pub vr_side_by_side: bool,
    pub aspect: Aspect,
    pub show_fps: bool,
    /// Master volume 0..1.
    pub volume: f32,
    pub muted: bool,
    pub fast_forward_speed: u32,
    /// Display frame rate cap (0 = match the monitor). Never changes game speed.
    pub target_fps: u32,
    /// Blend between game frames so high-refresh monitors show smoother motion.
    pub smooth_motion: bool,
    /// Game speed in percent (100 = the console's real speed).
    pub game_speed: u32,
    /// Sync drawing to the monitor refresh (applies after restart).
    pub vsync: bool,
    /// Frames emulated but not drawn, per drawn frame (helps slow PCs).
    pub frame_skip: u32,
    /// Pause the game while the window is in the background.
    pub pause_unfocused: bool,

    /// Game folder per console id.
    pub rom_dirs: BTreeMap<String, String>,
    /// Individually added game files per console id.
    pub added_games: BTreeMap<String, Vec<String>>,
    /// Games removed from the library (files left on disk).
    pub hidden_games: Vec<String>,

    /// Most recently played first.
    pub recent: Vec<RecentGame>,
    /// Per-game keyboard layouts, keyed by game file path. Games without an entry use `keys`.
    pub game_keys: BTreeMap<String, KeyBindings>,

    /// PS4/PS5 and Xbox One/Series programs: "emulate" (our own CPU core) or "native" (relink to run on this PC's CPU).
    pub next_gen_mode: String,
    pub vm_dir: String,
    pub vms: Vec<VmConfig>,
    /// Saved PlayStation 5 / PlayStation 4 consoles for Remote Play.
    pub remote_consoles: Vec<crate::remote_play::SavedRemoteConsole>,
    /// Optional custom Remote Play client executable path (chiaki-ng, Sony PS Remote Play, etc.).
    pub remote_play_client: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keys: KeyBindings::default(),
            gamepad_enabled: true,
            stick_as_dpad: true,
            stick_deadzone: 0.5,
            nintendo_layout: false,
            auto_pad_layout: true,
            pad_migrated: false,
            rewind_enabled: true,
            rewind_seconds: 20,
            integer_scaling: false,
            smooth_filter: false,
            sharp_pixels: true,
            vr_side_by_side: false,
            aspect: Aspect::Square,
            show_fps: true,
            volume: 0.8,
            muted: false,
            fast_forward_speed: 4,
            target_fps: 0,
            smooth_motion: false,
            game_speed: 100,
            vsync: true,
            frame_skip: 0,
            pause_unfocused: false,
            rom_dirs: BTreeMap::new(),
            added_games: BTreeMap::new(),
            hidden_games: Vec::new(),
            recent: Vec::new(),
            game_keys: BTreeMap::new(),
            next_gen_mode: "emulate".into(),
            vm_dir: config_dir().join("vms").to_string_lossy().into_owned(),
            vms: Vec::new(),
            remote_consoles: Vec::new(),
            remote_play_client: String::new(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    crate::platform::config_base().join(crate::theme::t().full_name())
}

/// %LOCALAPPDATA%\<brand>\Games: games added with "Add files" are copied here.
pub fn games_dir() -> PathBuf {
    crate::platform::data_base().join(crate::theme::t().full_name()).join("Games")
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        let mut s: Settings = std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // Players who set their own controller buttons before auto layout existed keep them.
        if !s.pad_migrated {
            let k = &s.keys.pad;
            if *k != PadBindings::default() && *k != PadBindings::nintendo() {
                s.auto_pad_layout = false;
            }
            s.pad_migrated = true;
        }
        // The old turbo defaults (S and A) made a second key press the same button as X.
        if s.keys.turbo_a == "S" && s.keys.turbo_b == "A" {
            s.keys.turbo_a.clear();
            s.keys.turbo_b.clear();
        }
        s.keys.fix_turbo_conflicts();
        s.keys.pad.fix_conflicts();
        for k in s.game_keys.values_mut() {
            k.fix_turbo_conflicts();
            k.pad.fix_conflicts();
        }
        s
    }

    pub fn save(&self) {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}
