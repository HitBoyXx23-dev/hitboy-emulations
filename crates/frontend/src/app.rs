//! Main window: Games, Virtual Machines and Settings, plus the game player.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, ColorImage, CornerRadius, FontId, Layout, Margin, Rect, Response, RichText, Sense,
    Stroke, TextureHandle, TextureOptions, Ui, Vec2,
};
use gilrs::Gilrs;

use crate::controls;
use crate::cores::{self, Core};
use crate::covers::Covers;
use crate::audio::Audio;
use crate::icons::{self, Icon};
use crate::input;
use crate::library::{self, GameEntry, Status, CONSOLES};
use crate::settings::{Aspect, KeyBindings, RecentGame, Settings};
use crate::theme::t;
use crate::vm::{self, DeviceKind, Engine, VmConfig};
use crate::vm_player::VmSession;

mod shell_ui;

const SIDEBAR_W: f32 = 248.0;
/// Sidebar pseudo-console showing recently played games from every system.
const HOME: &str = "home";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Library,
    Console,
    Discover,
    RemotePlay,
    Machines,
    Devices,
    Settings,
}

struct Session {
    core: Box<dyn Core>,
    /// Game file; also the key for per-game settings.
    path: PathBuf,
    save_path: PathBuf,
    title: String,
    paused: bool,
    frames_this_tick: u32,
    frames_total: u64,
    accumulator: f64,
    last_tick: Instant,
    fps_frames: u32,
    fps_since: Instant,
    fps: u32,
    /// The last two game frames, for smooth-motion blending.
    prev_frame: Vec<u32>,
    cur_frame: Vec<u32>,
    /// Recent moments for rewind, and how far the held rewind button has advanced.
    rewind: state::Rewind,
    rewind_acc: f64,
    rewinding: bool,
    /// Save-state slot (1-9) the hotkeys use.
    slot: u8,
    /// Hash of the game save last written to disk (None = nothing written yet).
    saved_hash: Option<u64>,
    /// Where this game's save-state files live.
    state_dir: PathBuf,
}

/// A game snapshot is taken every this many frames (15 a second at 60 fps).
const SNAP_EVERY: u64 = 4;
/// Snapshots stepped back per second while rewind is held (twice real time).
const REWIND_RATE: f64 = 30.0;

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

impl Session {
    fn new(core: Box<dyn Core>, path: PathBuf, save_path: PathBuf, title: String, now: Instant) -> Session {
        Session {
            core,
            path,
            save_path,
            title,
            paused: false,
            frames_this_tick: 0,
            frames_total: 0,
            accumulator: 0.0,
            last_tick: now,
            fps_frames: 0,
            fps_since: now,
            fps: 0,
            prev_frame: Vec::new(),
            cur_frame: Vec::new(),
            rewind: state::Rewind::new(300, 96 << 20),
            rewind_acc: 0.0,
            rewinding: false,
            slot: 1,
            saved_hash: None,
            state_dir: crate::settings::config_dir().join("states"),
        }
    }

    /// Writes the game's own save (battery RAM, memory card) if it changed since last time.
    fn flush_save(&mut self) -> Result<(), String> {
        let Some(ram) = self.core.save_ram() else { return Ok(()) };
        let h = fnv(&ram);
        if self.saved_hash == Some(h) {
            return Ok(());
        }
        if let Some(dir) = self.save_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write to a temp file and rename, so a crash mid-write can't destroy the old save.
        let tmp = self.save_path.with_extension("sav.tmp");
        std::fs::write(&tmp, &ram).and_then(|_| std::fs::rename(&tmp, &self.save_path)).map_err(|e| format!("Couldn't write save file: {e}"))?;
        self.saved_hash = Some(h);
        Ok(())
    }

    fn state_file(&self, slot: u8) -> PathBuf {
        let console = cores::console_for_path(&self.path).unwrap_or("nes");
        let stem: String = self.title.chars().map(|c| if c.is_alphanumeric() || " ()-_.,'!&".contains(c) { c } else { '_' }).collect();
        self.state_dir.join(console).join(format!("{stem}.slot{slot}.state"))
    }

    /// Called after every emulated frame: every few frames a snapshot is kept for rewind.
    fn record_frame(&mut self) {
        if self.frames_total % SNAP_EVERY == 0 {
            if let Some(snap) = self.core.save_state() {
                self.rewind.push(snap);
            }
        }
    }

    /// One step back in time. Returns whether the machine changed (so the picture needs a
    /// refresh).
    fn rewind_step(&mut self) -> bool {
        match self.rewind.pop() {
            Some(snap) => self.core.load_state(&snap).is_ok(),
            None => false,
        }
    }

    /// Saves the whole game to a numbered slot.
    fn save_slot(&mut self, slot: u8) -> Result<(), String> {
        let data = self.core.save_state().ok_or("Save states aren't available for this console yet.")?;
        let path = self.state_file(slot);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut file = b"NSF1".to_vec();
        file.extend((fnv(&data) as u32).to_le_bytes());
        file.extend(&data);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, file).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| format!("Couldn't save the state: {e}"))
    }

    fn load_slot(&mut self, slot: u8) -> Result<(), String> {
        let file = std::fs::read(self.state_file(slot)).map_err(|_| format!("Slot {slot} is empty."))?;
        if file.len() < 8 || &file[..4] != b"NSF1" || u32::from_le_bytes(file[4..8].try_into().unwrap()) != fnv(&file[8..]) as u32 {
            return Err(format!("Slot {slot} is damaged."));
        }
        self.core.load_state(&file[8..])?;
        self.rewind.clear();
        self.accumulator = 0.0;
        Ok(())
    }

    /// When a slot was saved, as a short "5 min ago" for the menu.
    fn slot_age(&self, slot: u8) -> Option<String> {
        let t = std::fs::metadata(self.state_file(slot)).and_then(|m| m.modified()).ok()?;
        let secs = std::time::SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
        Some(match secs {
            0..=59 => "just now".into(),
            60..=3599 => format!("{} min ago", secs / 60),
            3600..=86_399 => format!("{} h ago", secs / 3600),
            _ => format!("{} days ago", secs / 86_400),
        })
    }
}

pub struct App {
    tab: Tab,
    settings: Settings,
    saved_settings: Settings,
    gilrs: Option<Gilrs>,

    console: &'static str,
    games: Vec<GameEntry>,
    game_filter: String,
    /// Console mode (power-on animation and home screen).
    shell: crate::shell::Shell,
    shell_games: Vec<GameEntry>,
    shell_games_for: Option<usize>,
    /// Controller buttons pressed since Console mode last looked.
    shell_pad: Vec<gilrs::Button>,

    session: Option<Session>,
    screen: Option<TextureHandle>,
    fullscreen: bool,

    selected_vm: Option<usize>,
    vm_procs: HashMap<usize, Child>,
    vm_session: Option<VmSession>,
    new_disk_gb: u32,

    rebinding: Option<&'static str>,
    toast: Option<(String, Instant)>,

    covers: Covers,
    discover: crate::discover::Discover,
    discover_view: DiscoverView,
    audio: Option<Audio>,
    audio_buf: Vec<f32>,
    download: Option<vm::Download>,
    download_view: DownloadView,

    /// Game whose controls editor is open.
    controls_for: Option<ControlsTarget>,
    modal_rebind: Option<&'static str>,
    /// The running game was auto-paused by opening the editor.
    resume_after_modal: bool,

    /// "Add files" copies games into the app's own folder in the background.
    copy_tx: std::sync::mpsc::Sender<(String, Result<PathBuf, String>)>,
    copy_rx: std::sync::mpsc::Receiver<(String, Result<PathBuf, String>)>,
    copying: usize,
    /// Screen scale used last frame (drives the sharp-pixel pre-scale).
    last_scale: f32,
    /// Delete waiting for confirmation in our own dialog.
    confirm_delete: Option<PendingDelete>,
    /// Disc conversion: the file picked and the chosen conversion, then the running job.
    convert_pick: Option<(PathBuf, Vec<crate::tools::Conversion>, usize)>,
    convert_job: Option<crate::tools::Job>,
    /// PlayStation BIOS search results (None = not searched yet).
    bios_search: Option<std::sync::mpsc::Receiver<Vec<crate::tools::FoundBios>>>,
    bios_found: Option<Vec<crate::tools::FoundBios>>,
    /// Result of opening an iPhone/iPad app (.ipa): (title, details).
    ios_report: Option<(String, String)>,
    /// PlayStation 5 / PlayStation 4 Remote Play state.
    remote_play: crate::remote_play::RemotePlayState,
}

/// Something the user asked to delete, shown in the confirmation dialog.
enum PendingDelete {
    Game { path: PathBuf, title: String, home: bool },
    Vm { idx: usize, name: String, disk: String, delete_disk: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiscoverView {
    Find,
    Sources,
    Guides,
}

/// UI snapshot of the installer download.
#[derive(Clone, PartialEq)]
enum DownloadView {
    Idle,
    Running(u64, u64, f64),
    Verifying,
    Failed(String),
}

#[derive(Clone)]
struct ControlsTarget {
    path: PathBuf,
    title: String,
    console: &'static str,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>, tab: Option<&str>, boot_vm: Option<&str>) -> Self {
        apply_style(&cc.egui_ctx);
        let tab = match tab {
            Some("console") => Tab::Console,
            Some("remote") | Some("remote-play") => Tab::RemotePlay,
            Some("vms") => Tab::Machines,
            Some("devices") => Tab::Devices,
            Some("settings") => Tab::Settings,
            Some("discover") => Tab::Discover,
            _ => Tab::Library,
        };
        let settings = Settings::load();
        let remote_play = crate::remote_play::RemotePlayState::new(&settings.remote_play_client);
        crate::cores::set_native_next_gen(settings.next_gen_mode == "native");
        let (copy_tx, copy_rx) = std::sync::mpsc::channel();
        let mut app = Self {
            tab,
            saved_settings: settings.clone(),
            settings,
            gilrs: Gilrs::new().ok(),
            console: HOME,
            games: Vec::new(),
            game_filter: String::new(),
            shell: if tab == Tab::Console { crate::shell::Shell::booting() } else { crate::shell::Shell::new() },
            shell_games: Vec::new(),
            shell_games_for: None,
            shell_pad: Vec::new(),
            session: None,
            screen: None,
            fullscreen: false,
            selected_vm: None,
            vm_procs: HashMap::new(),
            vm_session: None,
            new_disk_gb: 64,
            rebinding: None,
            toast: None,
            covers: Covers::new(crate::settings::config_dir().join("covers")),
            discover: crate::discover::Discover::new(crate::settings::config_dir().join("covers")),
            discover_view: DiscoverView::Find,
            audio: Audio::new().map_err(|e| eprintln!("audio unavailable: {e}")).ok(),
            audio_buf: Vec::with_capacity(8192),
            download: None,
            download_view: DownloadView::Idle,
            controls_for: None,
            modal_rebind: None,
            resume_after_modal: false,
            copy_tx,
            copy_rx,
            copying: 0,
            last_scale: 1.0,
            confirm_delete: None,
            convert_pick: None,
            convert_job: None,
            bios_search: None,
            bios_found: None,
            ios_report: None,
            remote_play,
        };
        app.rescan();
        if let Some(p) = initial {
            app.launch(&p);
            if std::env::args().any(|a| a == "--controls") {
                if let Some(s) = &app.session {
                    let (path, title) = (s.path.clone(), s.title.clone());
                    let console = cores::console_for_path(&path).unwrap_or("nes");
                    app.open_controls(&path, &title, console);
                }
            }
        }
        if let Some(img) = boot_vm {
            let name = Path::new(img).file_stem().unwrap_or_default().to_string_lossy().into_owned();
            match VmSession::start(&name, img, "", 0) {
                Ok(s) => app.vm_session = Some(s),
                Err(e) => app.notify(e),
            }
        }
        app
    }

    /// Keyboard layout for a game: its custom layout if it has one, else the global one.
    fn keys_for(&self, path: &Path) -> KeyBindings {
        if let Some(custom) = self.settings.game_keys.get(&*path.to_string_lossy()) {
            return custom.clone();
        }
        let mut k = self.settings.keys.clone();
        // Auto layout: the controller buttons that match this console's real pad.
        if self.settings.auto_pad_layout {
            k.pad = controls::default_pad(cores::console_for_path(path).unwrap_or("nes"));
        }
        k
    }

    /// The first connected controller: its name and family.
    fn pad_info(&self) -> Option<(String, controls::PadKind)> {
        let g = self.gilrs.as_ref()?;
        let (_, pad) = g.gamepads().find(|(_, p)| p.is_connected())?;
        Some((pad.name().to_string(), controls::detect(pad.name(), pad.vendor_id())))
    }

    fn open_controls(&mut self, path: &Path, title: &str, console: &'static str) {
        if let Some(s) = self.session.as_mut().filter(|s| s.path == path && !s.paused) {
            s.paused = true;
            self.resume_after_modal = true;
        }
        self.modal_rebind = None;
        self.controls_for = Some(ControlsTarget { path: path.to_path_buf(), title: title.to_owned(), console });
    }

    fn close_controls(&mut self) {
        self.controls_for = None;
        self.modal_rebind = None;
        if std::mem::take(&mut self.resume_after_modal) {
            if let Some(s) = self.session.as_mut() {
                s.paused = false;
                s.last_tick = Instant::now();
            }
        }
    }

    fn controls_modal(&mut self, ctx: &egui::Context) {
        let Some(target) = self.controls_for.clone() else { return };
        let game_key = target.path.to_string_lossy().into_owned();
        let global_only = game_key.is_empty();

        // ---- capture a new binding (keyboard key or controller button)
        if let Some(field) = self.modal_rebind {
            let key_pressed = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key { key, pressed: true, .. } => Some(*key),
                    _ => None,
                })
            });
            let mut pad_pressed = None;
            if let Some(g) = self.gilrs.as_mut() {
                while let Some(ev) = g.next_event() {
                    if let gilrs::EventType::ButtonPressed(b, _) = ev.event {
                        if input::PAD_BUTTONS.contains(&b) {
                            pad_pressed = Some(b);
                        }
                    }
                }
            }
            if field.starts_with("pad:")
                && self.settings.auto_pad_layout
                && !self.settings.game_keys.contains_key(&game_key)
                && (pad_pressed.is_some() || matches!(key_pressed, Some(egui::Key::Backspace | egui::Key::Delete)))
            {
                // Changing a button turns the automatic layout into a custom one, starting
                // from what was in use.
                self.settings.keys.pad = controls::default_pad(target.console);
                self.settings.auto_pad_layout = false;
            }
            let kb = match self.settings.game_keys.get_mut(&game_key) {
                Some(custom) => custom,
                None => &mut self.settings.keys,
            };
            if let Some(pad_field) = field.strip_prefix("pad:") {
                match (key_pressed, pad_pressed) {
                    (Some(egui::Key::Escape), _) => self.modal_rebind = None,
                    (Some(egui::Key::Backspace | egui::Key::Delete), _) => {
                        *pad_binding_mut(&mut kb.pad, pad_field) = String::new();
                        self.modal_rebind = None;
                    }
                    (_, Some(b)) => {
                        assign_swapping(&mut kb.pad, pad_field, input::pad_name(b), PAD_FIELDS, pad_binding_mut);
                        self.modal_rebind = None;
                    }
                    _ => {}
                }
            } else if let Some(k) = key_pressed {
                if k != egui::Key::Escape {
                    assign_swapping(kb, field, k.name().to_string(), KEY_FIELDS, binding_mut);
                }
                self.modal_rebind = None;
            }
        } else if let Some(g) = self.gilrs.as_mut() {
            while g.next_event().is_some() {}
        }

        let custom = self.settings.game_keys.contains_key(&game_key);
        let keys = self.keys_for(&target.path);
        let (live, turbo_a, turbo_b) = input::live_state(ctx, self.gilrs.as_ref(), &self.settings, &keys);
        let console = library::console(target.console);
        let layout = controls::layout(target.console);
        let pad_info = self.pad_info();
        controls::set_current(pad_info.as_ref().map(|p| p.1).unwrap_or(controls::PadKind::Generic));
        let auto_layout = self.settings.auto_pad_layout && !custom;
        let waiting = self.modal_rebind;
        let mut done = false;
        let mut set_custom: Option<bool> = None;
        let mut restore = false;
        let mut rebind: Option<&'static str> = None;
        let mut hovered_part: Option<&'static str> = None;
        let max_h = (ctx.screen_rect().height() - 80.0).max(300.0);

        let modal = egui::Modal::new(egui::Id::new("controls_modal"))
            .backdrop_color(Color32::from_black_alpha(180))
            .frame(
                egui::Frame::NONE
                    .fill(t().panel)
                    .stroke(Stroke::new(1.0_f32, t().border))
                    .corner_radius(CornerRadius::same(18))
                    .inner_margin(Margin::same(28)),
            )
            .show(ctx, |ui| {
                ui.set_width(780.0);
                // ---- header
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
                    ui.painter().rect_filled(r, CornerRadius::same(12), t().surface_hover);
                    icons::paint(ui.painter(), r.shrink(10.0), Icon::Gamepad, t().primary);
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.add_space(2.0);
                        ui.label(RichText::new("Controls").size(22.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                        let sub = if global_only { "The global layout, used by every game without its own controls".to_string() } else { format!("{} ({})", target.title, console.name) };
                        ui.label(RichText::new(sub).size(12.5).color(t().muted));
                    });
                    if !global_only {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if segment(ui, custom, "This game only").clicked() && !custom {
                                set_custom = Some(true);
                            }
                            if segment(ui, !custom, "Global").clicked() && custom {
                                set_custom = Some(false);
                            }
                        });
                    }
                });
                ui.add_space(16.0);

                egui::ScrollArea::vertical().max_height(max_h - 190.0).auto_shrink([false, true]).show(ui, |ui| {
                    // ---- live controller
                    let mut lit: Vec<&str> = ["up", "down", "left", "right", "a", "b", "select", "start", "x", "y", "l", "r"].into_iter().filter(|f| live & controls::bit_of(f) != 0).collect();
                    if turbo_a {
                        lit.push("a");
                    }
                    if turbo_b {
                        lit.push("b");
                    }
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(layout.name).size(13.0).color(t().muted));
                    });
                    let diagram_rect = ui.allocate_exact_size(Vec2::new(ui.available_width(), controls::diagram_height(&layout, ui.available_width())), Sense::hover()).0;
                    ui.add_space(6.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("Press any bound key or controller button to test it â€” it lights up above.").size(12.0).color(t().muted));
                        if !layout.note.is_empty() {
                            ui.label(RichText::new(layout.note).size(12.0).color(t().muted));
                        }
                    });
                    ui.add_space(10.0);
                    // The detected controller and how its buttons were placed.
                    ui.horizontal(|ui| {
                        match &pad_info {
                            Some((name, kind)) => {
                                ui.label(RichText::new(format!("Controller: {name}")).size(12.5).color(t().ok));
                                pill(ui, kind.name(), t().primary, t().surface);
                            }
                            None => {
                                ui.label(RichText::new("No controller connected. Plug one in or pair it over Bluetooth; it's recognised automatically.").size(12.5).color(t().muted));
                            }
                        }
                    });
                    if !global_only {
                        ui.label(
                            RichText::new(if auto_layout {
                                "Buttons are placed automatically to match this console's real pad. Change any controller button to make your own layout."
                            } else {
                                "Using your own controller layout."
                            })
                            .size(12.0)
                            .color(t().muted),
                        );
                    }
                    ui.add_space(10.0);

                    // ---- bindings table
                    let col_w = (ui.available_width() - 16.0) / 2.0;
                    ui.horizontal_top(|ui| {
                        for (heading, rows) in [("Movement", &layout.movement[..]), ("Buttons", &layout.rows[..])] {
                            ui.allocate_ui_with_layout(Vec2::new(col_w, 0.0), Layout::top_down(Align::Min), |ui| {
                                ui.set_width(col_w);
                                binding_card(ui, heading, rows, &keys, waiting, &mut rebind, &mut hovered_part);
                            });
                        }
                    });
                    ui.add_space(14.0);

                    // ---- hotkeys
                    egui::Frame::NONE.fill(t().surface).stroke(Stroke::new(1.0_f32, t().border)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(16)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new("Hotkeys").size(14.5).family(egui::FontFamily::Name("heading".into())).color(t().text));
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 14.0;
                            for (field, label) in [
                                ("pause", "Pause"),
                                ("fast_forward", "Fast-forward"),
                                ("rewind", "Rewind (hold)"),
                                ("save_state", "Save state"),
                                ("load_state", "Load state"),
                                ("next_slot", "Next slot"),
                                ("reset", "Reset"),
                                ("fullscreen", "Fullscreen"),
                            ] {
                                ui.label(RichText::new(label).color(t().muted));
                                let (r, _) = ui.allocate_exact_size(Vec2::new(104.0, 28.0), Sense::hover());
                                if key_chip(ui, r, field, &binding(&keys, field), waiting == Some(field)).clicked() {
                                    rebind = Some(field);
                                }
                                ui.add_space(6.0);
                            }
                        });
                    });

                    controls::paint(ui, diagram_rect, &layout, &lit, hovered_part);
                });

                // ---- footer
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if icon_button(ui, Icon::Reset, "Restore defaults", false).clicked() {
                        restore = true;
                    }
                    let hint = match waiting {
                        Some(f) if f.starts_with("pad:") => "Press a controller button. Backspace unbinds, Esc cancels.",
                        Some(_) => "Press a key. Esc cancels.",
                        None => "Click a binding to change it. Duplicates are swapped automatically.",
                    };
                    ui.label(RichText::new(hint).size(12.0).color(if waiting.is_some() { t().primary } else { t().muted }));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_button(ui, "Done").clicked() {
                            done = true;
                        }
                    });
                });
            });

        if let Some(f) = rebind {
            self.modal_rebind = Some(f);
        }
        match set_custom {
            Some(true) => {
                let effective = self.keys_for(&target.path);
                self.settings.game_keys.insert(game_key.clone(), effective);
            }
            Some(false) => {
                self.settings.game_keys.remove(&game_key);
            }
            None => {}
        }
        if restore {
            match self.settings.game_keys.get_mut(&game_key) {
                Some(k) => {
                    *k = KeyBindings::default();
                    k.pad = controls::default_pad(target.console);
                }
                None => {
                    self.settings.keys = KeyBindings::default();
                    self.settings.auto_pad_layout = true;
                }
            }
        }
        if done || (modal.should_close() && waiting.is_none()) {
            self.close_controls();
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    /// Creates a VM from a template: config, a fresh disk, and an already-downloaded ISO if present.
    fn create_from_preset(&mut self, p: &'static vm::Preset) {
        let mut cfg = p.config();
        let taken = |n: &str| self.settings.vms.iter().any(|v| v.name == n);
        let mut n = 1;
        while taken(&cfg.name) {
            n += 1;
            cfg.name = format!("{} {n}", p.name);
        }
        let dir = PathBuf::from(&self.settings.vm_dir);
        let _ = std::fs::create_dir_all(&dir);
        if !p.iso_file.is_empty() {
            let iso = dir.join(p.iso_file);
            if iso.exists() {
                cfg.iso = iso.to_string_lossy().into_owned();
            }
        }
        let disk = dir.join(format!("{}.img", cfg.name.replace(' ', "_")));
        if !disk.exists() {
            if let Err(e) = vm::create_disk(&disk, p.disk_gb) {
                self.notify(format!("Disk creation failed: {e}"));
            }
        }
        if disk.exists() {
            cfg.disk = disk.to_string_lossy().into_owned();
        }
        self.notify(format!("Created {} ({} GB disk)", cfg.name, p.disk_gb));
        self.settings.vms.push(cfg);
        self.selected_vm = Some(self.settings.vms.len() - 1);
    }

    fn poll_download(&mut self, ctx: &egui::Context) {
        let Some(d) = self.download.as_mut() else { return };
        ctx.request_repaint_after(Duration::from_millis(250));
        match d.poll() {
            vm::DownloadState::Running { done, total, rate } => self.download_view = DownloadView::Running(done, total, rate),
            vm::DownloadState::Verifying => self.download_view = DownloadView::Verifying,
            vm::DownloadState::Finished(path) => {
                let name = d.vm.clone();
                let iso = path.to_string_lossy().into_owned();
                for v in self.settings.vms.iter_mut().filter(|v| v.name == name || (v.iso.is_empty() && !v.preset.is_empty())) {
                    v.iso = iso.clone();
                }
                self.download = None;
                self.download_view = DownloadView::Idle;
                self.notify("Installer downloaded and verified");
            }
            vm::DownloadState::Failed(e) => {
                self.download = None;
                self.download_view = DownloadView::Failed(e);
            }
        }
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn rescan(&mut self) {
        if self.console == HOME {
            self.games = self
                .settings
                .recent
                .iter()
                .filter(|r| Path::new(&r.path).exists())
                .map(|r| GameEntry {
                    title: library::clean_title(&r.title),
                    path: PathBuf::from(&r.path),
                    size: std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0),
                    console: library::console(&r.console).id,
                })
                .collect();
            return;
        }
        let c = library::console(self.console);
        let dir = self.settings.rom_dirs.get(c.id).map(String::as_str);
        let added = self.settings.added_games.get(c.id).cloned().unwrap_or_default();
        self.games = library::scan(c, dir, &added, &self.settings.hidden_games);
    }

    fn launch(&mut self, path: &Path) {
        match cores::load(path) {
            Ok(mut loaded) => {
                loaded.core.set_stereo(self.settings.vr_side_by_side);
                let now = Instant::now();
                let title = loaded.title;
                self.notify(format!("Playing {title}"));
                self.settings.recent.retain(|r| Path::new(&r.path) != path);
                self.settings.recent.insert(
                    0,
                    RecentGame { path: path.to_string_lossy().into_owned(), console: loaded.console.into(), title: title.clone() },
                );
                self.settings.recent.truncate(24);
                self.screen = None;
                self.session = Some(Session::new(loaded.core, path.to_path_buf(), loaded.save_path, title, now));
                if let (Some(s), Some(a)) = (self.session.as_mut(), &self.audio) {
                    s.core.set_sample_rate(a.sample_rate);
                    a.clear();
                }
            }
            Err(e) => {
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("Couldn't start game")
                    .set_description(e)
                    .show();
            }
        }
    }

    fn stop_game(&mut self, ctx: &egui::Context) {
        if let Some(a) = &self.audio {
            a.clear();
        }
        if let Some(mut s) = self.session.take() {
            if let Err(e) = s.flush_save() {
                self.notify(e);
            }
        }
        if self.console == HOME {
            self.rescan();
        }
        self.set_fullscreen(ctx, false);
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(t().full_name()));
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        if self.fullscreen != on {
            self.fullscreen = on;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
        }
    }

    // ------------------------------------------------------------ game player

    fn run_emulation(&mut self, ctx: &egui::Context) {
        // Game keys must never activate a toolbar button that kept focus after a click.
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
        let keys = self.session.as_ref().map(|s| self.keys_for(&s.path)).unwrap_or_else(|| self.settings.keys.clone());
        // While the controls editor is open, key presses belong to it, not the game.
        if self.controls_for.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        // Turbo: ~15 presses per second (2 frames on, 2 off).
        let turbo_on = self.session.as_ref().is_some_and(|s| (s.frames_total / 2) % 2 == 0);
        let pads = input::poll(ctx, self.gilrs.as_mut(), &self.settings, &keys, turbo_on);
        let ff_speed = self.settings.fast_forward_speed.max(1) as f64;
        if input::key_pressed(ctx, &keys.fullscreen) {
            let on = !self.fullscreen;
            self.set_fullscreen(ctx, on);
        }
        let Some(s) = self.session.as_mut() else { return };

        if input::key_pressed(ctx, &keys.pause) {
            s.paused = !s.paused;
        }
        if input::key_pressed(ctx, &keys.reset) {
            s.core.reset();
            s.rewind.clear();
            s.paused = false;
        }
        // ---- save states: save, load, next slot
        if input::key_pressed(ctx, &keys.next_slot) {
            s.slot = s.slot % 9 + 1;
            self.toast = Some((format!("State slot {}", s.slot), Instant::now()));
        }
        if input::key_pressed(ctx, &keys.save_state) {
            let msg = match s.save_slot(s.slot) {
                Ok(()) => format!("Saved to slot {}", s.slot),
                Err(e) => e,
            };
            self.toast = Some((msg, Instant::now()));
        }
        if input::key_pressed(ctx, &keys.load_state) {
            let msg = match s.load_slot(s.slot) {
                Ok(()) => format!("Loaded slot {}", s.slot),
                Err(e) => e,
            };
            self.toast = Some((msg, Instant::now()));
        }
        // Game saves reach the disk every few seconds, not just when the game is closed.
        if s.frames_total % 300 == 299 {
            if let Err(e) = s.flush_save() {
                self.toast = Some((e, Instant::now()));
            }
        }

        let now = Instant::now();
        let dt = now.duration_since(s.last_tick).as_secs_f64().min(0.1);
        s.last_tick = now;
        if self.settings.pause_unfocused && !ctx.input(|i| i.viewport().focused.unwrap_or(true)) {
            if let Some(a) = &self.audio {
                a.clear();
            }
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        if s.paused {
            // Nothing changes while paused: skip emulation and texture uploads, repaint lazily.
            if let Some(a) = &self.audio {
                a.clear();
            }
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let rewind_on = self.settings.rewind_enabled && s.core.has_state();
        s.rewind.max_snapshots = (self.settings.rewind_seconds.clamp(1, 120) as u64 * 60 / SNAP_EVERY).max(2) as usize;
        let rewind_held = rewind_on && (input::key_down(ctx, &keys.rewind) || input::pad_held(self.gilrs.as_ref(), &keys.pad.rewind));
        if rewind_held {
            // Step back through recent moments; the picture is refreshed with one frame.
            s.rewind_acc += dt * REWIND_RATE;
            let mut stepped = false;
            let mut n = 0;
            while s.rewind_acc >= 1.0 && n < 4 {
                s.rewind_acc -= 1.0;
                n += 1;
                stepped |= s.rewind_step();
            }
            if stepped {
                s.core.run_frame();
            }
            s.accumulator = 0.0;
            s.frames_this_tick = stepped as u32;
            s.rewinding = true;
            // Silence while going backwards.
            s.core.take_audio(&mut self.audio_buf);
            self.audio_buf.clear();
            if let Some(a) = &self.audio {
                a.clear();
            }
        } else {
            s.rewinding = false;
            s.rewind_acc = 0.0;
            let ff = input::key_down(ctx, &keys.fast_forward);
            // Games run at their console's real speed times the chosen game speed; the display
            // frame rate is separate and never speeds the game up.
            let base = self.settings.game_speed.clamp(25, 400) as f64 / 100.0;
            let speed = if ff { ff_speed } else { base };
            s.accumulator += dt * s.core.fps() * speed;
            s.core.set_buttons(0, pads[0]);
            s.core.set_buttons(1, pads[1]);
            let mut ran = 0;
            while s.accumulator >= 1.0 && ran < 12 {
                s.core.run_frame();
                if let Some(message) = s.core.runtime_message() {
                    s.paused = true;
                    self.toast = Some((message.to_string(), Instant::now()));
                    break;
                }
                s.accumulator -= 1.0;
                s.frames_total += 1;
                ran += 1;
                s.fps_frames += 1;
                if rewind_on {
                    s.record_frame();
                }
            }
            if ran == 12 {
                s.accumulator = 0.0; // too far behind; drop time instead of spiralling
            }
            s.frames_this_tick = ran;
            // No official artwork? After ~8 s of play the title screen becomes the cover.
            if s.frames_total >= 480 && s.frames_total < 480 + ran as u64 {
                let console = cores::console_for_path(&s.path).unwrap_or("nes");
                if self.covers.is_missing(&s.path) {
                    let (w, h) = s.core.size();
                    let cache = crate::settings::config_dir().join("covers");
                    self.covers.set_from_frame(ctx, &cache, &s.path, console, &s.title, s.core.frame(), w, h);
                }
            }
            s.core.take_audio(&mut self.audio_buf);
            if let Some(a) = self.audio.as_mut() {
                // Muting ramps the gain to zero (no pop) instead of starving the output.
                a.volume = if self.settings.muted { 0.0 } else { self.settings.volume };
                if !ff {
                    if speed == 1.0 {
                        a.push(&self.audio_buf);
                    } else {
                        // Faster/slower frame rates: resample so sound keeps pace with the game.
                        let frames = self.audio_buf.len() / 2;
                        let out_n = (frames as f64 / speed) as usize;
                        let mut v = Vec::with_capacity(out_n * 2);
                        for i in 0..out_n {
                            let j = ((i as f64 * speed) as usize).min(frames.saturating_sub(1));
                            v.push(self.audio_buf[j * 2]);
                            v.push(self.audio_buf[j * 2 + 1]);
                        }
                        a.push(&v);
                    }
                }
            }
            self.audio_buf.clear();
        }
        if s.fps_since.elapsed() >= Duration::from_secs(1) {
            s.fps = s.fps_frames;
            s.fps_frames = 0;
            s.fps_since = now;
        }
        if s.frames_total <= 12 || s.fps_since == now {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(window_title(&s.path, &s.title)));
        }

        // Display pacing: match the monitor, or cap at the chosen rate.
        let repaint = |ctx: &egui::Context, fps: u32| {
            if fps == 0 {
                ctx.request_repaint();
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64(1.0 / fps as f64));
            }
        };
        let (w, h) = s.core.size();
        if ran_frames(s) > 0 {
            let f = s.core.frame();
            if s.cur_frame.len() == f.len() {
                std::mem::swap(&mut s.prev_frame, &mut s.cur_frame);
                s.cur_frame.copy_from_slice(f);
            } else {
                s.cur_frame = f.to_vec();
                s.prev_frame = f.to_vec();
            }
        }
        let smooth = self.settings.smooth_motion && !input::key_down(ctx, &keys.fast_forward) && s.prev_frame.len() == w * h && s.cur_frame.len() == w * h;
        let skip = self.settings.frame_skip as u64;
        if !smooth && (ran_frames(s) == 0 || (skip > 0 && s.frames_total % (skip + 1) != 0)) && self.screen.is_some() {
            repaint(ctx, self.settings.target_fps);
            return; // no new frame this tick: keep the current texture
        }
        // Smooth motion: cross-fade from the previous game frame to the latest by how far we are
        // towards the next one, so a 120/144 Hz display shows in-between steps.
        let blended: Vec<u32>;
        let frame: &[u32] = if smooth {
            let t = (s.accumulator.clamp(0.0, 1.0) * 256.0) as u32;
            blended = s
                .prev_frame
                .iter()
                .zip(&s.cur_frame)
                .map(|(&a, &b)| {
                    let mix = |sh: u32| (((a >> sh) & 255) * (256 - t) + ((b >> sh) & 255) * t) >> 8;
                    mix(16) << 16 | mix(8) << 8 | mix(0)
                })
                .collect();
            &blended
        } else if s.cur_frame.len() == w * h {
            &s.cur_frame
        } else {
            s.core.frame()
        };
        // Sharp pixels: blow the frame up by a whole number with nearest-neighbour, then let the
        // GPU smooth only the last fraction. Pixels stay square and even at any window size.
        let k = if self.settings.sharp_pixels && !self.settings.smooth_filter { (self.last_scale.ceil() as usize).clamp(1, 4) } else { 1 };
        let mut pixels = Vec::with_capacity(w * h * k * k);
        for y in 0..h * k {
            let row = &frame[(y / k) * w..(y / k + 1) * w];
            for x in 0..w * k {
                let p = row[x / k];
                pixels.push(Color32::from_rgb((p >> 16) as u8, (p >> 8) as u8, p as u8));
            }
        }
        let image = ColorImage { size: [w * k, h * k], pixels };
        let filter = if self.settings.smooth_filter || k > 1 { TextureOptions::LINEAR } else { TextureOptions::NEAREST };
        match &mut self.screen {
            Some(tex) => tex.set(image, filter),
            None => self.screen = Some(ctx.load_texture("screen", image, filter)),
        }
        repaint(ctx, self.settings.target_fps);
    }

    fn player_ui(&mut self, ctx: &egui::Context) {
        let mut stop = false;
        let mut open_controls: Option<(PathBuf, String)> = None;
        let mut toggle_fs = false;
        if !self.fullscreen {
            egui::TopBottomPanel::top("player_bar")
                .frame(egui::Frame::NONE.fill(t().panel).inner_margin(Margin::symmetric(16, 10)))
                .show_separator_line(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if icon_button(ui, Icon::Back, "Library", false).clicked() {
                            stop = true;
                        }
                        ui.add_space(8.0);
                        let Some(s) = self.session.as_mut() else { return };
                        let console = cores::console_for_path(&s.path).unwrap_or("nes");
                        let art = self.covers.get(ctx, &s.path, console, &s.title).cloned();
                        // The picture keeps its own shape: portrait box art is cover-cropped, wide
                        // logos and screenshots get a wider box (no shrinking to a speck).
                        let width = art.as_ref().map(|t| { let [w, h] = t.size(); (36.0 * w as f32 / h as f32).clamp(28.0, 64.0) }).unwrap_or(34.0);
                        let (r, _) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::hover());
                        match art {
                            Some(tex) => paint_thumb(ui, r, &tex, 5),
                            None => {
                                // No artwork (yet, or none exists): a badge with the console's code, so the
                                // spot is never empty.
                                ui.painter().rect_filled(r, CornerRadius::same(6), t().primary.gamma_multiply(0.22));
                                ui.painter().rect_stroke(r, CornerRadius::same(6), egui::Stroke::new(1.0_f32, t().primary.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                                let code = library::console(console).short;
                                ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, code, egui::FontId::proportional(if code.len() > 3 { 10.0 } else { 12.0 }), t().text);
                            }
                        }
                        ui.add_space(4.0);
                        let name = library::clean_title(&s.title);
                        let short = cores::console_for_path(&s.path).map(|c| library::console(c).name).unwrap_or("");
                        ui.vertical(|ui| {
                            ui.label(RichText::new(name).size(15.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                            ui.label(RichText::new(short).size(11.5).color(t().muted));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if icon_button(ui, Icon::Fullscreen, "Fullscreen", false).on_hover_text(&self.settings.keys.fullscreen).clicked() {
                                toggle_fs = true;
                            }
                            if icon_button(ui, Icon::Gamepad, "Controls", false).clicked() {
                                open_controls = Some((s.path.clone(), s.title.clone()));
                            }
                            // Save states: nine slots per game.
                            let states = icon_button(ui, Icon::Disk, "States", false).on_hover_text(format!("Save {} Â· Load {} Â· Next slot {} Â· Rewind (hold) {}", self.settings.keys.save_state, self.settings.keys.load_state, self.settings.keys.next_slot, self.settings.keys.rewind));
                            let pop = ui.make_persistent_id("states_menu");
                            if states.clicked() {
                                ui.memory_mut(|m| m.toggle_popup(pop));
                            }
                            egui::popup::popup_below_widget(ui, pop, &states, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
                                ui.set_min_width(300.0);
                                if !s.core.has_state() {
                                    ui.label(RichText::new("Save states and rewind aren't available for this console yet. The game's own saves still work.").size(12.5).color(t().muted));
                                    return;
                                }
                                ui.label(RichText::new("Save states").size(14.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                                ui.add_space(4.0);
                                for slot in 1..=9u8 {
                                    let age = s.slot_age(slot);
                                    ui.horizontal(|ui| {
                                        let current = s.slot == slot;
                                        ui.label(RichText::new(format!("Slot {slot}")).strong().color(if current { t().primary } else { t().text }));
                                        ui.label(RichText::new(age.clone().unwrap_or_else(|| "empty".into())).size(12.0).color(t().muted));
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            if age.is_some() && ui.small_button("Load").clicked() {
                                                s.slot = slot;
                                                let msg = match s.load_slot(slot) {
                                                    Ok(()) => format!("Loaded slot {slot}"),
                                                    Err(e) => e,
                                                };
                                                self.toast = Some((msg, Instant::now()));
                                            }
                                            if ui.small_button("Save").clicked() {
                                                s.slot = slot;
                                                let msg = match s.save_slot(slot) {
                                                    Ok(()) => format!("Saved to slot {slot}"),
                                                    Err(e) => e,
                                                };
                                                self.toast = Some((msg, Instant::now()));
                                            }
                                        });
                                    });
                                }
                            });
                            if icon_button(ui, Icon::Reset, "Reset", false).on_hover_text(&self.settings.keys.reset).clicked() {
                                s.core.reset();
                                s.paused = false;
                            }
                            let (icon, label) = if s.paused { (Icon::Play, "Resume") } else { (Icon::Pause, "Pause") };
                            if icon_button(ui, icon, label, true).on_hover_text(&self.settings.keys.pause).clicked() {
                                s.paused = !s.paused;
                            }
                            ui.add_space(10.0);
                            volume_control(ui, &mut self.settings.volume, &mut self.settings.muted);
                            if self.settings.show_fps {
                                ui.add_space(6.0);
                                ui.label(RichText::new(format!("{} FPS", s.fps)).monospace().size(12.0).color(t().muted));
                            }
                        });
                    });
                });
            egui::TopBottomPanel::top("player_rule").exact_height(2.0).frame(egui::Frame::NONE).show_separator_line(false).show(ctx, |ui| {
                gradient_rule(ui, ui.max_rect());
            });
        }

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::BLACK)).show(ctx, |ui| {
            if let Some(message) = self.session.as_ref().and_then(|session| session.core.runtime_message()) {
                ui.label(RichText::new(message).color(Color32::WHITE));
                ui.add_space(12.0);
            }
            let avail = ui.available_rect_before_wrap();
            let ((w, h), tv) = self.session.as_ref().map(|s| (s.core.size(), s.core.tv_aspect())).unwrap_or(((256, 240), 4.0 / 3.0));
            let (base_w, base_h) = match self.settings.aspect {
                Aspect::Square => (w as f32, h as f32),
                Aspect::Tv => (h as f32 * tv, h as f32),
            };
            let mut scale = (avail.width() / base_w).min(avail.height() / base_h);
            if self.settings.integer_scaling && scale >= 1.0 {
                scale = scale.floor();
            }
            let rect = Rect::from_center_size(avail.center(), Vec2::new(base_w * scale, base_h * scale));
            // Screen pixels per emulated pixel, for the sharp-pixel pre-scale.
            self.last_scale = (rect.width() / w as f32).max(rect.height() / h as f32) * ui.ctx().pixels_per_point();
            let resp = ui.allocate_rect(avail, Sense::click_and_drag());
            // Forward live pointer/touchscreen coordinates for dual-screen & touch consoles (3DS, NDS, Vita, Wii U, Wii).
            if let Some(s) = self.session.as_mut() {
                let (pressed, pos) = ui.input(|i| (i.pointer.primary_down(), i.pointer.interact_pos()));
                if let Some(p) = pos {
                    if rect.contains(p) {
                        let nx = ((p.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
                        let ny = ((p.y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0);
                        s.core.set_touch(pressed, nx, ny);
                    } else {
                        s.core.set_touch(false, 0.0, 0.0);
                    }
                } else {
                    s.core.set_touch(false, 0.0, 0.0);
                }
            }
            if let Some(tex) = &self.screen {
                ui.painter().image(tex.id(), rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            if resp.double_clicked() {
                toggle_fs = true;
            }
            if let Some(s) = &self.session {
                if s.paused {
                    ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha(150));
                    let card = Rect::from_center_size(rect.center(), Vec2::new(260.0, 96.0));
                    ui.painter().rect(card, CornerRadius::same(12), t().panel, Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
                    ui.painter().text(card.center() - Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, "Paused", FontId::proportional(26.0), t().text);
                    ui.painter().text(card.center() + Vec2::new(0.0, 20.0), Align2::CENTER_CENTER, format!("Press {} to resume", self.settings.keys.pause), FontId::proportional(13.0), t().muted);
                }
                if s.rewinding {
                    let secs = s.rewind.len() as f32 * SNAP_EVERY as f32 / 60.0;
                    let label = format!("<<  REWIND   {secs:.0}s");
                    let pos = rect.left_top() + Vec2::new(16.0, 16.0);
                    let galley = ui.painter().layout_no_wrap(label, FontId::proportional(18.0), Color32::WHITE);
                    let bg = Rect::from_min_size(pos, galley.size() + Vec2::new(24.0, 12.0));
                    ui.painter().rect_filled(bg, CornerRadius::same(10), Color32::from_black_alpha(190));
                    ui.painter().rect_stroke(bg, CornerRadius::same(10), Stroke::new(1.5_f32, t().primary), egui::StrokeKind::Inside);
                    ui.painter().galley(pos + Vec2::new(12.0, 6.0), galley, Color32::WHITE);
                }
                if self.fullscreen && self.settings.show_fps {
                    ui.painter().text(rect.left_top() + Vec2::new(10.0, 10.0), Align2::LEFT_TOP, format!("{} FPS", s.fps), FontId::monospace(13.0), Color32::from_white_alpha(150));
                }
            }
        });
        if toggle_fs {
            let on = !self.fullscreen;
            self.set_fullscreen(ctx, on);
        }
        if let Some((path, title)) = open_controls {
            let console = cores::console_for_path(&path).unwrap_or("nes");
            self.open_controls(&path, &title, console);
        }
        if stop {
            self.stop_game(ctx);
        } else if self.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.set_fullscreen(ctx, false);
        }
    }

    // ------------------------------------------------------------ built-in VM player

    fn vm_player_ui(&mut self, ctx: &egui::Context) {
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
        let mut stop = false;
        let mut toggle_fs = false;
        if let Some(s) = self.vm_session.as_mut() {
            s.tick(ctx);
        }
        if !self.fullscreen {
            egui::TopBottomPanel::top("vm_bar")
                .frame(egui::Frame::NONE.fill(t().panel).inner_margin(Margin::symmetric(16, 10)))
                .show_separator_line(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if icon_button(ui, Icon::Power, "Power off", false).clicked() {
                            stop = true;
                        }
                        ui.add_space(8.0);
                        let Some(s) = self.vm_session.as_mut() else { return };
                        let engine_label = s.engine_label();
                        let is_android = s.is_android();
                        ui.label(RichText::new(&s.name).size(15.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                        pill(ui, &engine_label, t().primary, t().surface);
                        let mut install_apk_clicked = false;
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if icon_button(ui, Icon::Fullscreen, "Fullscreen", false).clicked() {
                                toggle_fs = true;
                            }
                            if icon_button(ui, Icon::Keyboard, "Reboot", false).on_hover_text("Restart the guest").clicked() {
                                s.reboot();
                            }
                            if is_android {
                                if ghost_button(ui, "Rotate").clicked() {
                                    s.toggle_rotation();
                                }
                                if primary_button(ui, "Install APK").clicked() {
                                    install_apk_clicked = true;
                                }
                            } else {
                                let label = if s.paused { "Resume" } else { "Pause" };
                                if primary_button(ui, label).clicked() {
                                    s.paused = !s.paused;
                                }
                                ui.add_space(6.0);
                                ui.label(RichText::new(format!("{:.1} MIPS", s.mips())).monospace().size(12.0).color(t().muted));
                            }
                        });
                        if install_apk_clicked {
                            if let Some(path) = rfd::FileDialog::new().add_filter("Android Package", &["apk"]).pick_file() {
                                let vm_idx = s.vm_index();
                                match s.install_apk(&path) {
                                    Ok(label) => {
                                        if let Some(vi) = vm_idx {
                                            if let Some(vm) = self.settings.vms.get_mut(vi) {
                                                let ps = path.to_string_lossy().into_owned();
                                                if !vm.apks.contains(&ps) {
                                                    vm.apks.push(ps);
                                                }
                                            }
                                        }
                                        self.notify(format!("Installed {label}"));
                                    }
                                    Err(e) => self.notify(e),
                                }
                            }
                        }
                    });
                });
            egui::TopBottomPanel::top("vm_rule").exact_height(2.0).frame(egui::Frame::NONE).show_separator_line(false).show(ctx, |ui| {
                gradient_rule(ui, ui.max_rect());
            });
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::BLACK)).show(ctx, |ui| {
            let avail = ui.available_rect_before_wrap();
            if let Some(s) = self.vm_session.as_mut() {
                if s.draw(ui, avail).double_clicked() {
                    toggle_fs = true;
                }
                if s.paused {
                    ui.painter().rect_filled(avail, 0.0, Color32::from_black_alpha(150));
                    ui.painter().text(avail.center(), Align2::CENTER_CENTER, "Paused", FontId::proportional(26.0), t().text);
                }
            }
        });
        if toggle_fs {
            let on = !self.fullscreen;
            self.set_fullscreen(ctx, on);
        }
        if self.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::F11)) {
            self.set_fullscreen(ctx, false);
        }
        if stop {
            self.vm_session = None;
            self.set_fullscreen(ctx, false);
        }
    }

    // ------------------------------------------------------------ shell

    fn header_ui(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::NONE.fill(t().panel).inner_margin(Margin { left: 20, right: 20, top: 0, bottom: 0 }))
            .exact_height(60.0)
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    wordmark(ui);
                    ui.add_space(36.0);
                    for (tab, icon, label) in [
                        (Tab::Library, Icon::Gamepad, "Games"),
                        (Tab::Console, Icon::Power, "Console mode"),
                        (Tab::Discover, Icon::Search, "Discover"),
                        (Tab::RemotePlay, Icon::Remote, "Remote Play"),
                        (Tab::Machines, Icon::Monitor, "Virtual Machines"),
                        (Tab::Devices, Icon::Phone, "Phones & Tablets"),
                        (Tab::Settings, Icon::Gear, "Settings"),
                    ] {
                        if nav_tab(ui, self.tab == tab, icon, label).clicked() {
                            if tab == Tab::Console && self.tab != Tab::Console {
                                self.shell.restart(ctx.input(|i| i.time));
                                self.shell_games_for = None;
                            }
                            if tab == Tab::RemotePlay && self.remote_play.last_scan.is_none() && !self.remote_play.scanning {
                                self.remote_play.refresh_clients(&self.settings.remote_play_client);
                                self.remote_play.start_scan();
                            }
                            self.tab = tab;
                            self.rebinding = None;
                        }
                        ui.add_space(4.0);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let pads = self.gilrs.as_ref().map(|g| g.gamepads().filter(|(_, p)| p.is_connected()).count()).unwrap_or(0);
                        if pads == 0 {
                            pill(ui, "No controller", t().muted, t().surface);
                        } else {
                            pill(ui, &format!("{pads} controller{} connected", if pads > 1 { "s" } else { "" }), t().ok, t().surface);
                        }
                    });
                });
            });
        egui::TopBottomPanel::top("header_rule").exact_height(2.0).frame(egui::Frame::NONE).show_separator_line(false).show(ctx, |ui| {
            gradient_rule(ui, ui.max_rect());
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::NONE.fill(t().panel).inner_margin(Margin::symmetric(20, 6)))
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let msg = match &self.toast {
                        Some((msg, at)) if at.elapsed() < Duration::from_secs(4) => RichText::new(msg).color(t().text),
                        _ => RichText::new("Ready").color(t().muted),
                    };
                    ui.label(msg.size(12.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(t().full_name()).size(12.0).color(t().muted));
                        let running = self.vm_procs.len();
                        if running > 0 {
                            ui.label(RichText::new(format!("{running} VM running      ")).size(12.0).color(t().ok));
                        }
                    });
                });
            });
    }

    // ------------------------------------------------------------ games tab

    fn has_builtin(console: &str) -> bool {
        matches!(library::console(console).status, Status::Playable | Status::Experimental)
    }

    fn can_play(&self, console: &str) -> bool {
        Self::has_builtin(console)
    }

    /// Plays a game on our own cores. Consoles still being built report what they read.
    fn launch_any(&mut self, path: &Path, console: &'static str) {
        if Self::has_builtin(console) || matches!(console, "wii" | "switch" | "xbox" | "x360" | "xone" | "ps2") {
            self.launch(path);
            return;
        }
        self.notify(format!("{} isn't playable yet. Its core is still being built.", library::console(console).name));
    }

    fn library_ui(&mut self, ctx: &egui::Context) {
        sidebar(ctx, "consoles", |ui| {
            if side_row(ui, self.console == HOME, Icon::Clock, "Recently played", None).clicked() && self.console != HOME {
                self.console = HOME;
                self.game_filter.clear();
                self.rescan();
            }
            let mut last_maker = "";
            for c in CONSOLES {
                if c.maker != last_maker {
                    last_maker = c.maker;
                    ui.add_space(16.0);
                    ui.label(RichText::new(c.maker.to_uppercase()).strong().size(11.0).color(t().muted));
                    ui.add_space(4.0);
                }
                let dot = match c.status {
                    Status::Playable => Some(t().ok),
                    Status::Experimental => Some(t().primary),
                    Status::Planned => None,
                };
                let icon = if c.handheld { Icon::Handheld } else { Icon::Console };
                if side_row(ui, self.console == c.id, icon, c.name, dot).clicked() && self.console != c.id {
                    self.console = c.id;
                    self.game_filter.clear();
                    self.rescan();
                }
            }
        });

        content(ctx, |ui| {
            let home = self.console == HOME;
            let c = library::console(if home { "nes" } else { self.console });
            if home {
                // Sleek Home Hero Launchpad card with brand gradient accent
                let mut open_console_mode = false;
                let mut open_android_vm = false;
                let mut open_native_tools = false;
                let mut resume_first: Option<(PathBuf, &'static str)> = None;
                let first_recent = self.games.first().map(|g| (g.title.clone(), g.path.clone(), g.console));
                let active_cores = CONSOLES.iter().filter(|k| k.status != Status::Planned).count();

                let hero_frame = egui::Frame::NONE
                    .fill(t().panel)
                    .stroke(Stroke::new(1.0_f32, t().primary.gamma_multiply(0.45)))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::symmetric(24, 20));
                let hero_resp = hero_frame.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        pill(ui, &format!("{active_cores} Custom Cores Ready"), t().ok, t().surface);
                        pill(ui, "Xbox & PS5 Native + Emu", t().primary, t().surface);
                        pill(ui, "Android 14 & Multi-OS VMs", t().secondary, t().surface);
                    });
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("Welcome to {}", t().full_name()))
                            .size(26.0)
                            .family(egui::FontFamily::Name("heading".into()))
                            .color(t().text),
                    );
                    ui.label(
                        RichText::new("Play classic, handheld, and next-gen consoles on our clean-room cores, convert Xbox/PS5 binaries, or boot Android & desktop VMs.")
                            .size(13.5)
                            .color(t().muted),
                    );
                    ui.add_space(14.0);
                    ui.horizontal_wrapped(|ui| {
                        if let Some((title, path, cid)) = first_recent {
                            if icon_button(ui, Icon::Play, &format!("Resume {title}"), true).clicked() {
                                resume_first = Some((path, cid));
                            }
                        }
                        if icon_button(ui, Icon::Power, "Console Mode", false).clicked() {
                            open_console_mode = true;
                        }
                        if icon_button(ui, Icon::Phone, "Android 14 Phone", false).clicked() {
                            open_android_vm = true;
                        }
                        if icon_button(ui, Icon::Gear, "Xbox / PS5 Native Tools", false).clicked() {
                            open_native_tools = true;
                        }
                    });
                });
                // Paint subtle top Royal Blue <-> Royal Purple gradient stripe along hero card top edge
                let hr = hero_resp.response.rect;
                gradient_rule(ui, Rect::from_min_max(egui::pos2(hr.left() + 14.0, hr.top()), egui::pos2(hr.right() - 14.0, hr.top() + 3.0)));
                ui.add_space(18.0);

                if let Some((p, cid)) = resume_first {
                    self.launch_any(&p, cid);
                } else if open_console_mode {
                    self.shell.restart(ctx.input(|i| i.time));
                    self.shell_games_for = None;
                    self.tab = Tab::Console;
                } else if open_android_vm {
                    self.notify("Started Android 14 Phone");
                    self.vm_session = Some(VmSession::start_android("Android 14 Phone", true, None, &[]));
                } else if open_native_tools {
                    self.tab = Tab::Settings;
                }
            }

            ui.horizontal(|ui| {
                let title = if home { "Recently played" } else { c.name };
                ui.label(RichText::new(title).size(if home { 22.0 } else { 28.0 }).family(egui::FontFamily::Name("heading".into())).color(t().text));
                if !home {
                    ui.add_space(6.0);
                    match c.status {
                        Status::Playable => pill(ui, "Built-in core", t().ok, t().surface),
                        Status::Experimental => pill(ui, "Experimental", t().primary, t().surface),
                        Status::Planned => pill(ui, "Coming soon", t().primary, t().surface),
                    }
                }
            });
            let subtitle = if home {
                "Jump back into your games.".to_string()
            } else {
                match c.status {
                    Status::Playable | Status::Experimental => c.note.to_string(),
                    Status::Planned => format!("{} Games you add stay here until then.", c.note),
                }
            };
            ui.label(RichText::new(subtitle).color(t().muted));
            self.task_banner(ui);
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                search_box(ui, &mut self.game_filter);
                ui.label(RichText::new(match self.games.len() { 1 => "1 game".to_string(), n => format!("{n} games") }).color(t().muted));
                if self.copying > 0 {
                    ui.spinner();
                    let n = if self.copying == 1 { "1 game".to_string() } else { format!("{} games", self.copying) };
                    ui.label(RichText::new(format!("Copying {n} into your library...")).color(t().muted));
                }
                if home {
                    return;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, Icon::Reset, "Refresh", false).clicked() {
                        self.rescan();
                    }
                    if ["ps1", "gc", "wii", "ps2"].contains(&c.id) && icon_button(ui, Icon::Disk, "Convert discâ€¦", false).on_hover_text("ZIP/7Z with BIN/CUE â†’ ISO, BIN/CUE â†” ISO, merge multi-file CUE sheets").clicked() {
                        if let Some(f) = rfd::FileDialog::new().set_title("Disc image to convert").add_filter("Disc images and archives", &["zip", "7z", "rar", "cue", "iso"]).pick_file() {
                            let options = crate::tools::Conversion::for_input(&f);
                            if !options.is_empty() {
                                self.convert_pick = Some((f, options, 0));
                            }
                        }
                    }
                    if icon_button(ui, Icon::Plus, "Add files", false).clicked() {
                        let mut exts: Vec<&str> = c.extensions.to_vec();
                        if exts.contains(&"zip") {
                            exts.extend(["7z", "rar"]);
                        }
                        if let Some(files) = rfd::FileDialog::new().add_filter(c.name, &exts).pick_files() {
                            for f in files {
                                self.copy_game(c.id, f);
                            }
                        }
                    }
                    if icon_button(ui, Icon::Folder, "Choose game folder", true).clicked() {
                        if let Some(d) = rfd::FileDialog::new().set_title(format!("{} games folder", c.name)).pick_folder() {
                            self.settings.rom_dirs.insert(c.id.into(), d.to_string_lossy().into_owned());
                            self.rescan();
                        }
                    }
                });
            });
            if let Some(dir) = self.settings.rom_dirs.get(c.id).filter(|_| !home) {
                ui.label(RichText::new(format!("Folder: {dir}")).size(12.0).color(t().muted));
            }
            ui.add_space(14.0);

            if self.games.is_empty() {
                if home {
                    empty_state(ui, "Nothing played yet", "Pick a console on the left and start a game. It will show up here.");
                } else {
                    empty_state(ui, "No games yet", &format!("Choose a folder or add files ({}). Zip, 7z and rar archives work too.", c.extensions.iter().filter(|e| **e != "zip").copied().collect::<Vec<_>>().join(", ")));
                }
                return;
            }

            let filter = self.game_filter.to_lowercase();
            let mut to_launch = None;
            let mut to_remove = None;
            let mut open_controls = None;
            let playable_by: Vec<(&'static str, bool)> = CONSOLES.iter().map(|k| (k.id, self.can_play(k.id))).collect();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(18.0, 18.0);
                ui.horizontal_wrapped(|ui| {
                    for g in self.games.iter().filter(|g| g.title.to_lowercase().contains(&filter)) {
                        let gc = library::console(g.console);
                        let playable = playable_by.iter().any(|(id, ok)| *id == gc.id && *ok);
                        let art = self.covers.get(&ui.ctx().clone(), &g.path, g.console, &g.title).cloned();
                        let loading = self.covers.is_pending(&g.path);
                        let (resp, trash) = game_tile(ui, g, gc, playable, art.as_ref(), loading);
                        if trash {
                            to_remove = Some((g.path.clone(), g.title.clone(), None));
                        } else if resp.clicked() {
                            to_launch = Some((g.path.clone(), gc.id));
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Play").clicked() {
                                to_launch = Some((g.path.clone(), gc.id));
                                ui.close_menu();
                            }
                            if matches!(gc.status, Status::Playable | Status::Experimental) && ui.button("Controlsâ€¦").clicked() {
                                open_controls = Some((g.path.clone(), g.title.clone(), g.console));
                                ui.close_menu();
                            }
                            if ui.button("Show in folder").clicked() {
                                crate::platform::reveal(&g.path);
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui.button(if home { "Remove from recent" } else { "Remove from library" }).clicked() {
                                to_remove = Some((g.path.clone(), g.title.clone(), Some(false)));
                                ui.close_menu();
                            }
                            if ui.button("Delete game file...").clicked() {
                                to_remove = Some((g.path.clone(), g.title.clone(), Some(true)));
                                ui.close_menu();
                            }
                        });
                    }
                });
            });
            if let Some((p, title, choice)) = to_remove {
                self.remove_game(&p, &title, home, choice);
            }
            if let Some((path, title, console)) = open_controls {
                self.open_controls(&path, &title, console);
            }
            if let Some((p, console)) = to_launch {
                self.launch_any(&p, console);
            }
        });
    }

    /// Copies a picked game (plus a .cue's track files and any save) into the app's games folder.
    fn copy_game(&mut self, console: &'static str, src: PathBuf) {
        let dest_dir = crate::settings::games_dir().join(console);
        let tx = self.copy_tx.clone();
        self.copying += 1;
        std::thread::spawn(move || {
            let r = (|| -> Result<PathBuf, String> {
                std::fs::create_dir_all(&dest_dir).map_err(|e| format!("Couldn't create {}: {e}", dest_dir.display()))?;
                let name = src.file_name().ok_or("Not a file")?;
                let dest = dest_dir.join(name);
                if dest == src {
                    return Ok(dest);
                }
                let mut files = vec![src.clone()];
                if src.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue")) {
                    let text = std::fs::read_to_string(&src).unwrap_or_default();
                    for l in text.lines().filter(|l| l.trim_start().to_ascii_uppercase().starts_with("FILE")) {
                        if let (Some(a), Some(b)) = (l.find('"'), l.rfind('"')) {
                            if b > a {
                                files.push(src.with_file_name(&l[a + 1..b]));
                            }
                        }
                    }
                }
                files.push(src.with_extension("sav"));
                let size = |p: &Path| std::fs::metadata(p).ok().map(|m| m.len());
                for f in files.into_iter().filter(|f| f.exists()) {
                    let d = dest_dir.join(f.file_name().unwrap_or_default());
                    if !(d.exists() && size(&f) == size(&d)) {
                        std::fs::copy(&f, &d).map_err(|e| format!("Couldn't copy {}: {e}", f.display()))?;
                    }
                }
                Ok(dest)
            })();
            let _ = tx.send((console.to_string(), r));
        });
    }

    fn poll_copies(&mut self) {
        let mut changed = false;
        while let Ok((_, r)) = self.copy_rx.try_recv() {
            self.copying = self.copying.saturating_sub(1);
            match r {
                Ok(p) => {
                    let ps = p.to_string_lossy().into_owned();
                    self.settings.hidden_games.retain(|h| *h != ps);
                    let name = library::clean_title(&p.file_stem().unwrap_or_default().to_string_lossy());
                    self.notify(format!("Added {name}"));
                }
                Err(e) => self.notify(e),
            }
            changed = true;
        }
        if changed {
            self.rescan();
        }
    }

    /// Removes a game from the library (`Some(false)`), or asks to delete its file for good.
    fn remove_game(&mut self, path: &Path, title: &str, home: bool, choice: Option<bool>) {
        if choice == Some(false) {
            self.finish_remove(path, title, home, false);
        } else {
            self.confirm_delete = Some(PendingDelete::Game { path: path.to_path_buf(), title: title.to_string(), home });
        }
    }

    fn finish_remove(&mut self, path: &Path, title: &str, home: bool, delete: bool) {
        let name = library::clean_title(title);
        if delete {
            if let Err(e) = std::fs::remove_file(path) {
                self.notify(format!("Couldn't delete {name}: {e}"));
                return;
            }
            self.notify(format!("Deleted {name}"));
        } else {
            self.notify(format!("Removed {name} from the library"));
        }
        let ps = path.to_string_lossy().into_owned();
        self.settings.recent.retain(|r| Path::new(&r.path) != path);
        for list in self.settings.added_games.values_mut() {
            list.retain(|x| Path::new(x) != path);
        }
        if !home && !delete && !self.settings.hidden_games.contains(&ps) {
            self.settings.hidden_games.push(ps);
        }
        self.rescan();
    }

    fn delete_vm(&mut self, idx: usize, delete_disk: bool) {
        if idx >= self.settings.vms.len() {
            return;
        }
        if let Some(mut c) = self.vm_procs.remove(&idx) {
            let _ = c.kill();
        }
        let vm = self.settings.vms.remove(idx);
        if delete_disk && !vm.disk.is_empty() {
            match std::fs::remove_file(&vm.disk) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => self.notify(format!("Couldn't delete the disk image: {e}")),
            }
        }
        self.selected_vm = None;
        // Indices above the removed VM shift down.
        self.vm_procs = std::mem::take(&mut self.vm_procs).into_iter().map(|(i, c)| (if i > idx { i - 1 } else { i }, c)).collect();
        self.notify(format!("Deleted {}", vm.name));
    }

    /// Our own "are you sure?" dialog for deleting games and virtual machines.
    fn delete_modal(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.confirm_delete.as_mut() else { return };
        enum Choice {
            None,
            Cancel,
            RemoveOnly,
            Delete,
        }
        let mut choice = Choice::None;
        let (title, what) = match pending {
            PendingDelete::Game { title, .. } => (format!("Delete {}?", library::clean_title(title)), "game file"),
            PendingDelete::Vm { name, .. } => (format!("Delete {name}?"), "virtual machine"),
        };
        let modal = egui::Modal::new(egui::Id::new("confirm_delete")).frame(
            egui::Frame::NONE.fill(t().panel).stroke(Stroke::new(1.0_f32, t().border)).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(24)),
        );
        let resp = modal.show(ctx, |ui| {
            ui.set_width(440.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 20.0, t().danger.gamma_multiply(0.18));
                icons::paint(ui.painter(), r.shrink(11.0), Icon::Trash, t().danger);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(&title).size(19.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                    ui.label(RichText::new(format!("This permanently deletes the {what}.")).size(12.5).color(t().muted));
                });
            });
            ui.add_space(14.0);
            ui.label(RichText::new("Are you sure? It will be gone forever until you download it again. This can't be undone.").size(14.0).color(t().text));
            if let PendingDelete::Vm { disk, delete_disk, .. } = pending {
                ui.add_space(10.0);
                if !disk.is_empty() && Path::new(disk.as_str()).exists() {
                    let size = std::fs::metadata(disk.as_str()).map(|m| library::human_size(m.len())).unwrap_or_default();
                    ui.checkbox(delete_disk, format!("Also delete its disk image ({size})"));
                } else {
                    *delete_disk = false;
                }
            }
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                if ghost_button(ui, "Cancel").clicked() {
                    choice = Choice::Cancel;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let del = egui::Button::new(RichText::new("Delete forever").strong().color(Color32::WHITE)).fill(t().danger).min_size(Vec2::new(130.0, 34.0));
                    if ui.add(del).clicked() {
                        choice = Choice::Delete;
                    }
                    if let PendingDelete::Game { home, .. } = pending {
                        let label = if *home { "Just remove from Recently played" } else { "Just remove from library" };
                        if ghost_button(ui, label).clicked() {
                            choice = Choice::RemoveOnly;
                        }
                    }
                });
            });
        });
        if resp.should_close() && matches!(choice, Choice::None) {
            choice = Choice::Cancel;
        }
        match choice {
            Choice::None => {}
            Choice::Cancel => self.confirm_delete = None,
            Choice::RemoveOnly | Choice::Delete => {
                let delete = matches!(choice, Choice::Delete);
                match self.confirm_delete.take() {
                    Some(PendingDelete::Game { path, title, home }) => self.finish_remove(&path, &title, home, delete),
                    Some(PendingDelete::Vm { idx, delete_disk, .. }) => self.delete_vm(idx, delete_disk),
                    None => {}
                }
            }
        }
    }

    /// What's inside an iPhone/iPad app the user opened.
    fn ios_modal(&mut self, ctx: &egui::Context) {
        let Some((title, text)) = self.ios_report.clone() else { return };
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("ios_report")).frame(
            egui::Frame::NONE.fill(t().panel).stroke(Stroke::new(1.0_f32, t().border)).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(24)),
        );
        let resp = modal.show(ctx, |ui| {
            ui.set_width(460.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 20.0, t().primary.gamma_multiply(0.18));
                icons::paint(ui.painter(), r.shrink(11.0), Icon::Phone, t().primary);
                ui.add_space(6.0);
                ui.label(RichText::new(&title).size(19.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
            });
            ui.add_space(14.0);
            ui.label(RichText::new(&text).size(14.0).color(t().text));
            ui.add_space(18.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ghost_button(ui, "Close").clicked() {
                    close = true;
                }
            });
        });
        if close || resp.should_close() {
            self.ios_report = None;
        }
    }

    /// Choose-and-confirm dialog for disc conversions.
    fn convert_modal(&mut self, ctx: &egui::Context) {
        let Some((path, options, choice)) = self.convert_pick.as_mut() else { return };
        let mut go = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::new("convert_disc")).frame(
            egui::Frame::NONE.fill(t().panel).stroke(Stroke::new(1.0_f32, t().border)).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(24)),
        );
        let resp = modal.show(ctx, |ui| {
            ui.set_width(440.0);
            ui.label(RichText::new("Convert disc image").size(19.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
            ui.label(RichText::new(path.file_name().unwrap_or_default().to_string_lossy()).size(12.5).color(t().muted));
            ui.add_space(12.0);
            for (i, o) in options.iter().enumerate() {
                ui.radio_value(choice, i, o.label());
            }
            ui.add_space(6.0);
            let note = match options[*choice] {
                crate::tools::Conversion::CueToIso => "Keeps only the data track. CD music and PlayStation video/voice sectors are dropped, so some games lose sound or movies; keep the BIN/CUE for those.",
                crate::tools::Conversion::MergeCue => "Joins every track into one BIN with a matching CUE. Nothing is lost.",
                crate::tools::Conversion::IsoToCue => "Rebuilds full CD sectors (sync, headers and error correction) so the disc works in any emulator that wants BIN/CUE.",
                crate::tools::Conversion::ArchiveToIso => "Unpacks the archive, turns the BIN/CUE inside into one ISO (data track only) and saves it next to the archive.",
            };
            ui.label(RichText::new(note).size(12.0).color(t().muted));
            ui.add_space(4.0);
            ui.label(RichText::new("The new file is saved next to the original, which is kept.").size(12.0).color(t().muted));
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if ghost_button(ui, "Cancel").clicked() {
                    cancel = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(primary(egui::Button::new(RichText::new("Convert").strong().color(t().on_primary))).min_size(Vec2::new(120.0, 34.0))).clicked() {
                        go = true;
                    }
                });
            });
        });
        if resp.should_close() && !go {
            cancel = true;
        }
        if go {
            let (path, options, choice) = self.convert_pick.take().unwrap();
            if self.convert_job.is_some() {
                self.notify("Another conversion is still running.");
            } else {
                self.convert_job = Some(crate::tools::Job::start(path, options[choice]));
            }
        } else if cancel {
            self.convert_pick = None;
        }
    }

    /// Progress line for installs and game unpacking.
    fn task_banner(&mut self, ui: &mut Ui) {
        let mut lines: Vec<(String, bool)> = Vec::new();
        if let Some(j) = &self.convert_job {
            let pct = j.progress.load(std::sync::atomic::Ordering::Relaxed) as f32 / 10.0;
            lines.push((format!("Converting {} ({pct:.0}%)", j.input.file_name().unwrap_or_default().to_string_lossy()), false));
        }
        for (m, err) in lines {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if !err {
                    ui.spinner();
                }
                ui.label(RichText::new(m).color(if err { t().danger } else { t().text }));
            });
        }
    }

    // ------------------------------------------------------------ discover tab

    fn discover_ui(&mut self, ctx: &egui::Context) {
        let added = self.discover.poll(ctx);
        if !added.is_empty() {
            for path in added {
                let name = library::clean_title(&path.file_stem().unwrap_or_default().to_string_lossy());
                self.notify(format!("Added {name} to your library"));
            }
            self.rescan();
        }

        sidebar(ctx, "discover_nav", |ui| {
            for (view, icon, label) in [
                (DiscoverView::Find, Icon::Search, "Free games"),
                (DiscoverView::Sources, Icon::Folder, "Where to get games"),
                (DiscoverView::Guides, Icon::Gamepad, "Guides"),
            ] {
                if side_row(ui, self.discover_view == view, icon, label, None).clicked() {
                    self.discover_view = view;
                }
            }
        });

        content(ctx, |ui| match self.discover_view {
            DiscoverView::Find => self.discover_find(ui),
            DiscoverView::Sources => self.discover_sources(ui),
            DiscoverView::Guides => {
                ui.label(RichText::new("Guides").size(28.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                ui.label(RichText::new("Step-by-step help for the things people ask about most.").color(t().muted));
                ui.add_space(12.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_max_width(860.0);
                    for g in crate::discover::GUIDES {
                        section(ui, g.title, g.summary, |ui| {
                            for (i, step) in g.steps.iter().enumerate() {
                                ui.horizontal_top(|ui| {
                                    ui.add_sized([22.0, 18.0], egui::Label::new(RichText::new(format!("{}.", i + 1)).color(t().primary)));
                                    ui.add(egui::Label::new(RichText::new(*step).color(t().text)).wrap());
                                });
                                ui.add_space(2.0);
                            }
                        });
                    }
                });
            }
        });
    }

    fn discover_sources(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("Where to get games").size(28.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
        ui.label(RichText::new("Websites for games, tools to back up games you own, and more.").color(t().muted));
        ui.add_space(12.0);
        let row = |ui: &mut Ui, name: &str, url: &str, what: &str| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width() - 170.0);
                    ui.label(RichText::new(name).size(14.5).color(t().text));
                    ui.label(RichText::new(what).size(12.5).color(t().muted));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if url.is_empty() {
                        pill(ui, "No link yet", t().muted, t().surface);
                    } else if icon_button(ui, Icon::Search, "Open website", false).on_hover_text(url).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    }
                });
            });
            ui.add_space(6.0);
            ui.painter().hline(ui.min_rect().x_range(), ui.cursor().top(), Stroke::new(1.0_f32, t().border));
            ui.add_space(8.0);
        };
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(860.0);
            // Each site is listed once: the first entry with a given name or address wins.
            let mut seen: Vec<String> = Vec::new();
            for (group, sources) in crate::discover::SOURCES {
                let fresh: Vec<_> = sources
                    .iter()
                    .filter(|s| {
                        let name = s.name.to_lowercase();
                        let url = s.url.trim_end_matches('/').to_lowercase();
                        let dup = seen.contains(&name) || (!url.is_empty() && seen.contains(&url));
                        if !dup {
                            seen.push(name);
                            if !url.is_empty() {
                                seen.push(url);
                            }
                        }
                        !dup
                    })
                    .collect();
                if fresh.is_empty() {
                    continue;
                }
                ui.add_space(8.0);
                ui.label(RichText::new(*group).size(17.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                ui.add_space(6.0);
                for s in fresh {
                    row(ui, s.name, s.url, s.what);
                }
            }
        });
    }

    /// Free homebrew for consoles covered by itch.io's feeds.
    fn itch_grid(&mut self, ui: &mut Ui, console: &str) {
        ui.add(egui::TextEdit::singleline(&mut self.discover.filters.query).hint_text("Search title").desired_width(240.0).margin(Margin::symmetric(10, 7)));
        ui.add_space(10.0);
        let loading = self.discover.itch_is_loading(console);
        let games = match self.discover.itch_games(console) {
            Some(Ok(v)) => v.clone(),
            Some(Err(e)) => {
                ui.label(RichText::new(e).color(t().danger));
                return;
            }
            None => Vec::new(),
        };
        if loading || games.is_empty() {
            ui.horizontal(|ui| {
                if loading {
                    ui.spinner();
                    ui.label(RichText::new("Loading free games from itch.ioâ€¦").color(t().muted));
                } else {
                    ui.label(RichText::new("No console homebrew listed right now. Try again later.").color(t().muted));
                }
            });
            return;
        }
        let q = self.discover.filters.query.trim().to_lowercase();
        let shown: Vec<_> = games.into_iter().filter(|g| q.is_empty() || g.title.to_lowercase().contains(&q)).collect();
        ui.label(RichText::new(format!("{} free games", shown.len())).color(t().text));
        ui.add_space(10.0);
        egui::ScrollArea::vertical().id_salt(("itch_grid", console.to_string())).auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(18.0, 18.0);
            ui.horizontal_wrapped(|ui| {
                for g in &shown {
                    let tex = self.discover.itch_image(g).cloned();
                    let (cell, _) = ui.allocate_exact_size(Vec2::new(220.0, 290.0), Sense::hover());
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(cell).layout(Layout::top_down(Align::Min)));
                    let ui = &mut child;
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 4.0);
                    let (r, _) = ui.allocate_exact_size(Vec2::new(220.0, 174.0), Sense::hover());
                    match &tex {
                        Some(t2) => paint_art(ui, r, t2, 8),
                        None => {
                            ui.painter().rect(r, CornerRadius::same(8), t().surface, Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
                            icons::paint(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(36.0)), Icon::Console, t().border);
                        }
                    }
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(&g.title).size(14.0).color(t().text)).truncate()).on_hover_text(&g.title);
                    ui.add(egui::Label::new(RichText::new(&g.summary).size(11.5).color(t().muted)).truncate());
                    ui.add_space(4.0);
                    if icon_button(ui, Icon::Search, "Open on itch.io", false).on_hover_text(&g.link).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&g.link));
                    }
                }
            });
        });
    }

    fn discover_find(&mut self, ui: &mut Ui) {
        use crate::discover::{Sort, ITCH_CONSOLES, PER_PAGE, PLATFORMS};
        let itch_console = self.discover.filters.platform.strip_prefix("itch:").map(str::to_string);
        if itch_console.is_none() {
            self.discover.ensure_loaded();
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("Discover").size(28.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
            ui.add_space(8.0);
            pill(ui, "Free games", t().primary, t().surface);
        });
        let blurb = if itch_console.is_some() {
            "Free homebrew from independent developers on itch.io. Open a game's page to download it, then add the file to your library."
        } else {
            "Free games shared by their creators on Homebrew Hub. Add one and it goes straight into your library, ready to play."
        };
        ui.label(RichText::new(blurb).color(t().muted));
        ui.add_space(16.0);

        let before = self.discover.filters.clone();
        let (genres, years, makers) = self.discover.facets();
        // One row of consoles, whichever catalogue each one comes from.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            if segment(ui, self.discover.filters.platform.is_empty(), "All").clicked() {
                self.discover.filters.platform.clear();
                self.discover.page = 0;
            }
            for (code, name, _) in PLATFORMS {
                if segment(ui, self.discover.filters.platform == *code, name).clicked() {
                    self.discover.filters.platform = code.to_string();
                    self.discover.page = 0;
                }
            }
            for (id, name, _) in ITCH_CONSOLES {
                let code = format!("itch:{id}");
                if segment(ui, self.discover.filters.platform == code, name).clicked() {
                    self.discover.filters.platform = code;
                    self.discover.page = 0;
                }
            }
        });
        ui.add_space(12.0);
        if let Some(console) = itch_console {
            self.itch_grid(ui, &console);
            return;
        }
        // Row 2: search and filters.
        let f = &mut self.discover.filters;
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::TextEdit::singleline(&mut f.query).hint_text("Search title or maker").desired_width(190.0).margin(Margin::symmetric(10, 7)));
            let kind_label = match f.kind.as_str() {
                "game" => "Games",
                "demo" => "Demos",
                "tool" => "Tools",
                _ => "Games, demos and tools",
            };
            egui::ComboBox::from_id_salt("dk_kind").selected_text(kind_label).width(110.0).show_ui(ui, |ui| {
                for (v, l) in [("game", "Games"), ("demo", "Demos"), ("tool", "Tools"), ("", "Games, demos and tools")] {
                    ui.selectable_value(&mut f.kind, v.to_string(), l);
                }
            });
            let genre_label = if f.genre.is_empty() { "Any genre".to_string() } else { f.genre.clone() };
            egui::ComboBox::from_id_salt("dk_genre").selected_text(genre_label).width(115.0).height(360.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut f.genre, String::new(), "Any genre");
                for g in &genres {
                    ui.selectable_value(&mut f.genre, g.clone(), g);
                }
            });
            let year_label = f.year.map(|y| y.to_string()).unwrap_or_else(|| "Any year".into());
            egui::ComboBox::from_id_salt("dk_year").selected_text(year_label).width(90.0).height(360.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut f.year, None, "Any year");
                for y in &years {
                    ui.selectable_value(&mut f.year, Some(*y), y.to_string());
                }
            });
            let maker_label = if f.maker.is_empty() { "Any maker".to_string() } else { f.maker.clone() };
            egui::ComboBox::from_id_salt("dk_maker").selected_text(maker_label).width(140.0).height(360.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut f.maker, String::new(), "Any maker");
                for (m, n) in makers.iter().take(300) {
                    ui.selectable_value(&mut f.maker, m.clone(), format!("{m}  ({n})"));
                }
            });
            egui::ComboBox::from_id_salt("dk_sort").selected_text(f.sort.label()).width(150.0).show_ui(ui, |ui| {
                for s in Sort::ALL {
                    ui.selectable_value(&mut f.sort, s, s.label());
                }
            });
            let clean = crate::discover::Filters { platform: f.platform.clone(), ..Default::default() };
            if *f != clean && ghost_button(ui, "Clear filters").clicked() {
                *f = clean;
            }
        });
        if self.discover.filters != before {
            self.discover.page = 0;
        }
        ui.add_space(10.0);

        let results = self.discover.results();
        ui.horizontal(|ui| {
            let n = results.len();
            ui.label(RichText::new(if n == 1 { "1 free game".to_string() } else { format!("{n} free games") }).size(15.0).strong().color(t().text));
            if self.discover.is_loading() {
                ui.spinner();
                let pct = self.discover.progress().map(|p| format!(" {:.0}%", p * 100.0)).unwrap_or_default();
                ui.label(RichText::new(format!("Loading the catalog{pct}... (first time only)")).size(12.5).color(t().muted));
            }
            ui.add_space(8.0);
            if icon_button(ui, Icon::Reset, "Refresh", false).on_hover_text("Download the latest list from Homebrew Hub").clicked() {
                self.discover.refresh();
            }
        });
        for (p, e) in &self.discover.errors {
            ui.label(RichText::new(format!("{p}: {e}")).size(12.5).color(t().danger));
        }
        ui.add_space(10.0);
        if results.is_empty() {
            if !self.discover.is_loading() {
                empty_state(ui, "No games match", "Try another genre, year or maker, or clear the filters.");
            }
            return;
        }

        let pages = results.len().div_ceil(PER_PAGE);
        let page = self.discover.page.min(pages - 1);
        let shown = &results[page * PER_PAGE..((page + 1) * PER_PAGE).min(results.len())];
        let mut to_install = None;
        let mut to_play = None;
        let featured = if page == 0 && self.discover.filters.query.is_empty() { results.iter().find(|e| self.discover.image(e).is_some()).cloned() } else { None };
        egui::ScrollArea::vertical().id_salt(("discover_grid", page)).auto_shrink([false, false]).show(ui, |ui| {
            if let Some(f) = &featured {
                // Featured game: large art, details and the add button.
                let tex = self.discover.image(f).cloned();
                let w = ui.available_width().min(1100.0);
                let (banner, _) = ui.allocate_exact_size(Vec2::new(w, 230.0), Sense::hover());
                let p = ui.painter();
                p.rect(banner, CornerRadius::same(16), t().panel, Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
                let art = Rect::from_min_size(banner.min + Vec2::splat(16.0), Vec2::new(266.0, 198.0));
                if let Some(t2) = &tex {
                    paint_art(ui, art, t2, 10);
                }
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(egui::pos2(art.max.x + 24.0, banner.min.y + 22.0), banner.max - Vec2::splat(20.0))).layout(Layout::top_down(Align::Min)));
                child.label(RichText::new("FEATURED").size(11.5).strong().color(t().primary));
                child.add_space(2.0);
                child.add(egui::Label::new(RichText::new(&f.title).size(24.0).family(egui::FontFamily::Name("heading".into())).color(t().text)).truncate());
                let meta = match f.year {
                    Some(y) => format!("{}  Â·  {}  Â·  {y}", f.developer, f.platform_name()),
                    None => format!("{}  Â·  {}", f.developer, f.platform_name()),
                };
                child.label(RichText::new(meta).color(t().muted));
                child.add_space(8.0);
                child.horizontal_wrapped(|ui| {
                    for tag in f.tags.iter().take(4) {
                        pill(ui, tag, t().muted, t().surface);
                    }
                });
                child.add_space(14.0);
                let installing = self.discover.installing.contains_key(&f.slug);
                let done = self.discover.installed.iter().rev().find(|(s, _)| s == &f.slug).map(|(_, r)| r.clone());
                match (&done, installing) {
                    (Some(Ok(path)), _) => {
                        if f.playable() && icon_button(&mut child, Icon::Play, "Play now", true).clicked() {
                            to_play = Some(path.clone());
                        }
                    }
                    (_, true) => {
                        child.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Adding to your library...").color(t().muted));
                        });
                    }
                    _ => {
                        if child.add_enabled_ui(f.rom.is_some(), |ui| icon_button(ui, Icon::Plus, "Add to library", true)).inner.clicked() {
                            to_install = Some(f.clone());
                        }
                    }
                }
                ui.add_space(20.0);
                ui.label(RichText::new("All games").size(16.0).strong().color(t().text));
                ui.add_space(10.0);
            }
            ui.spacing_mut().item_spacing = Vec2::new(16.0, 16.0);
            ui.horizontal_wrapped(|ui| {
                for e in shown {
                    let tex = self.discover.image(e).cloned();
                    let installing = self.discover.installing.contains_key(&e.slug);
                    let done = self.discover.installed.iter().rev().find(|(s, _)| s == &e.slug).map(|(_, r)| r.clone());
                    let (cell, cell_resp) = ui.allocate_exact_size(Vec2::new(212.0, 318.0), Sense::hover());
                    // Card panel that lights up on hover.
                    let hovered = cell_resp.hovered();
                    ui.painter().rect(
                        cell,
                        CornerRadius::same(14),
                        if hovered { t().surface } else { t().panel },
                        Stroke::new(1.0_f32, if hovered { t().primary } else { t().border }),
                        egui::StrokeKind::Inside,
                    );
                    // Child UI inside the cell: draws the card without reserving extra grid space.
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(cell.shrink(6.0)).layout(Layout::top_down(Align::Min)));
                    let ui = &mut child;
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 4.0);
                    ui.set_width(200.0);
                    let (r, _) = ui.allocate_exact_size(Vec2::new(200.0, 170.0), Sense::hover());
                    match &tex {
                        Some(t2) => paint_art(ui, r, t2, 8),
                        None => {
                            ui.painter().rect(r, CornerRadius::same(8), t().surface, Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
                            let icon = if e.console() == "nes" { Icon::Console } else { Icon::Handheld };
                            icons::paint(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(36.0)), icon, t().border);
                        }
                    }
                    // Console chip.
                    let p = ui.painter();
                    let chip_font = FontId::proportional(10.5);
                    let cw = p.layout_no_wrap(e.platform.clone(), chip_font.clone(), Color32::WHITE).size().x + 12.0;
                    let chip = Rect::from_min_size(r.min + Vec2::new(8.0, 8.0), Vec2::new(cw, 18.0));
                    p.rect_filled(chip, CornerRadius::same(5), Color32::from_black_alpha(180));
                    p.text(chip.center(), Align2::CENTER_CENTER, &e.platform, chip_font, Color32::WHITE);
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(&e.title).size(14.0).color(t().text)).truncate()).on_hover_text(&e.title);
                    let meta = match e.year {
                        Some(y) => format!("{}  Â·  {y}", e.developer),
                        None => e.developer.clone(),
                    };
                    ui.add(egui::Label::new(RichText::new(meta).size(11.5).color(t().muted)).truncate());
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let tags: Vec<&String> = e.tags.iter().take(2).collect();
                        if tags.is_empty() {
                            pill(ui, e.platform_name(), t().muted, t().surface);
                        }
                        for tag in tags {
                            pill(ui, tag, t().muted, t().surface);
                        }
                    });
                    ui.add_space(6.0);
                    match (&done, installing) {
                        (Some(Ok(path)), _) => {
                            if e.playable() && icon_button(ui, Icon::Play, "Play", true).clicked() {
                                to_play = Some(path.clone());
                            }
                        }
                        (_, true) => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("Adding...").color(t().muted));
                            });
                        }
                        (Some(Err(err)), _) => {
                            ui.label(RichText::new(err).size(11.5).color(t().danger));
                        }
                        (None, false) => {
                            let ok = e.rom.is_some();
                            if ui.add_enabled_ui(ok, |ui| icon_button(ui, Icon::Plus, "Add free game", false)).inner.clicked() {
                                to_install = Some(e.clone());
                            }
                        }
                    }
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_enabled_ui(page > 0, |ui| icon_button(ui, Icon::Back, "Previous", false)).inner.clicked() {
                    self.discover.page = page - 1;
                }
                ui.label(RichText::new(format!("Page {} of {pages}", page + 1)).color(t().muted));
                if ui.add_enabled_ui(page + 1 < pages, |ui| icon_button(ui, Icon::Play, "Next", false)).inner.clicked() {
                    self.discover.page = page + 1;
                }
            });
        });
        if let Some(e) = to_install {
            let dir = crate::settings::games_dir().join(e.console());
            self.discover.install(&e, &dir);
        }
        if let Some(p) = to_play {
            self.launch(&p);
        }
    }

    // ------------------------------------------------------------ VM tab

    /// The Virtual Machines tab (`devices` = false) and the Phones & Tablets tab (true) share
    /// this page; each shows its own templates and machines.
    fn machines_ui(&mut self, ctx: &egui::Context, devices: bool) {
        // Reap VMs whose window was closed.
        self.vm_procs.retain(|_, child| matches!(child.try_wait(), Ok(None)));
        self.poll_download(ctx);
        let mut create_preset: Option<&'static vm::Preset> = None;
        let mine = |v: &VmConfig| (v.kind() != DeviceKind::Computer) == devices;

        sidebar(ctx, if devices { "device_list" } else { "vm_list" }, |ui| {
            if !devices {
                if ui.add_sized([ui.available_width(), 36.0], primary(egui::Button::new(RichText::new("New virtual machine").strong().color(t().on_primary)))).clicked() {
                    let mut vm = VmConfig::default();
                    vm.name = format!("Virtual Machine {}", self.settings.vms.len() + 1);
                    self.settings.vms.push(vm);
                    self.selected_vm = Some(self.settings.vms.len() - 1);
                }
                ui.add_space(14.0);
            }
            ui.label(RichText::new(if devices { "New device" } else { "Templates" }).size(12.5).color(t().muted));
            ui.add_space(4.0);
            for p in vm::PRESETS.iter().filter(|p| (p.kind != DeviceKind::Computer) == devices) {
                let icon = match p.kind {
                    DeviceKind::Phone => Icon::Phone,
                    DeviceKind::Tablet => Icon::Tablet,
                    DeviceKind::Computer => Icon::Plus,
                };
                if side_row(ui, false, icon, p.name, None).on_hover_text(p.description).clicked() {
                    create_preset = Some(p);
                }
            }
            ui.add_space(14.0);
            if self.settings.vms.iter().any(mine) {
                ui.label(RichText::new(if devices { "Your devices" } else { "Your machines" }).size(12.5).color(t().muted));
                ui.add_space(4.0);
            }
            for (i, vm) in self.settings.vms.iter().enumerate().filter(|(_, v)| mine(v)) {
                let dot = if self.vm_procs.contains_key(&i) { t().ok } else { t().border };
                let icon = match vm.kind() {
                    DeviceKind::Phone => Icon::Phone,
                    DeviceKind::Tablet => Icon::Tablet,
                    DeviceKind::Computer => Icon::Monitor,
                };
                if side_row(ui, self.selected_vm == Some(i), icon, &vm.name, Some(dot)).clicked() {
                    self.selected_vm = Some(i);
                }
            }
        });

        if let Some(p) = create_preset.take() {
            self.create_from_preset(p);
        }

        content(ctx, |ui| {
            let selected = self.selected_vm.filter(|&i| i < self.settings.vms.len() && mine(&self.settings.vms[i]));
            let Some(idx) = selected else {
                if devices {
                    let mut open_ios = false;
                    if let Some(p) = devices_hero(ui, &mut open_ios) {
                        create_preset = Some(p);
                    }
                    if open_ios {
                        if let Some(f) = rfd::FileDialog::new()
                            .set_title("iPhone / iPad app (.ipa) or firmware (.ipsw)")
                            .add_filter("iOS app or firmware", &["ipa", "ipsw"])
                            .pick_file()
                        {
                            let is_ipsw = f.extension().is_some_and(|e| e.eq_ignore_ascii_case("ipsw"));
                            if is_ipsw {
                                self.ios_report = Some(match ios::ipsw::read_ipsw_file(&f) {
                                    Ok(fw) => (
                                        format!("iOS {} ({})", fw.product_version, fw.product_build),
                                        fw.summary(),
                                    ),
                                    Err(e) => ("Couldn't open this iOS firmware".to_string(), e),
                                });
                            } else {
                                let r = std::fs::read(&f).map_err(|e| e.to_string()).and_then(|d| ios::run_ipa(&d, 250_000));
                                self.ios_report = Some(match r {
                                    Ok((app, run_res)) => {
                                        let mut body = app.summary();
                                        match run_res {
                                            Ok(rep) => {
                                                body.push_str(&format!(
                                                    "\n\nARM core execution ({}, entry {:#010X}): ran {} instructions â€” {}.",
                                                    rep.arch, rep.entry, rep.instructions, rep.stop_reason
                                                ));
                                                if !rep.stdout.is_empty() {
                                                    body.push_str(&format!("\n\nOutput:\n{}", rep.stdout));
                                                }
                                            }
                                            Err(msg) => {
                                                body.push_str(&format!("\n\nARM core: {msg}"));
                                            }
                                        }
                                        (app.name.clone(), body)
                                    }
                                    Err(e) => ("Couldn't open this app".to_string(), e),
                                });
                            }
                        }
                    }
                    return;
                }
                empty_state(ui, "Run operating systems from ISO and IMG files", "Create a virtual machine, attach an ISO or IMG, and press Start.");
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    pill(ui, "Runs on our own PC core", t().ok, t().surface);
                });
                return;
            };
            let running = self.vm_procs.contains_key(&idx);
            let mut start = false;
            let mut stop = false;
            let mut delete = false;
            let mut create_disk = false;
            let mut download: Option<&'static vm::Preset> = None;
            let mut cancel_download = false;
            let download_state = self.download_view.clone();
            let downloading_this = self.download.as_ref().is_some_and(|d| d.vm == self.settings.vms[idx].name);

            let vm = &mut self.settings.vms[idx];
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut vm.name).font(FontId::proportional(24.0)).frame(false).desired_width(420.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if running {
                        if ui.add(egui::Button::new(RichText::new("Stop").strong().color(Color32::WHITE)).fill(t().danger).min_size(Vec2::new(110.0, 36.0))).clicked() {
                            stop = true;
                        }
                        pill(ui, "Running", t().ok, t().surface);
                    } else {
                        if ui.add(primary(egui::Button::new(RichText::new("Start").strong().color(t().on_primary))).min_size(Vec2::new(110.0, 36.0))).clicked() {
                            start = true;
                        }
                        if icon_button(ui, Icon::Trash, "Delete", false).on_hover_text("Delete this virtual machine").clicked() {
                            delete = true;
                        }
                    }
                });
            });
            ui.add_space(12.0);

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                vm.engine = Engine::Builtin;
                if devices {
                    device_preview(ui, vm, running);
                }
                if let Some(p) = vm::preset(&vm.preset) {
                    setup_card(ui, p, vm, running, downloading_this, &download_state, &mut download, &mut cancel_download);
                }
                ui.add_enabled_ui(!running, |ui| {
                    if !devices {
                    section(ui, "Engine", "", |ui| {
                        ui.label(
                            RichText::new("Our own from-scratch PC: native BIOS, VGA text and 256-colour graphics. Boots DOS, FreeDOS and boot-disk tools today; \
                                           32/64-bit operating systems arrive as our PC core grows.")
                            .size(12.0)
                            .color(t().muted),
                        );
                    });
                    }
                    section(ui, "Boot media", "Installer or live image. ISO attaches as a DVD drive; IMG attaches as a bootable USB-style disk.", |ui| {
                        path_row(ui, "ISO / IMG", &mut vm.iso, &[("Disc & disk images", &["iso", "img"])]);
                    });
                    section(ui, "Hard disk", "Where the operating system gets installed.", |ui| {
                        path_row(ui, "Disk image", &mut vm.disk, &[("Disk images", &["img", "ima", "vfd"])]);
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Create a new blank disk").color(t().muted));
                            ui.add(egui::DragValue::new(&mut self.new_disk_gb).range(1..=4096).suffix(" GB"));
                            if icon_button(ui, Icon::Disk, "Create disk", false).clicked() {
                                create_disk = true;
                            }
                        });
                    });
                    section(ui, "Hardware", "Resources given to the guest operating system.", |ui| {
                        egui::Grid::new("hw").num_columns(2).spacing([24.0, 10.0]).show(ui, |ui| {
                            ui.label("Memory");
                            ui.add(egui::Slider::new(&mut vm.ram_mb, 16..=2048).logarithmic(true).suffix(" MB"));
                            ui.end_row();
                            ui.label("CPU cores");
                            ui.add(egui::Slider::new(&mut vm.cpus, 1..=16));
                            ui.end_row();
                            ui.label("Acceleration");
                            ui.checkbox(&mut vm.accelerate, "Windows Hypervisor Platform");
                            ui.end_row();
                            ui.label("Firmware");
                            ui.checkbox(&mut vm.uefi, "UEFI (Windows 11, modern Linux)");
                            ui.end_row();
                            ui.label("Network");
                            ui.checkbox(&mut vm.network, "Internet access (NAT)");
                            ui.end_row();
                            ui.label("Devices");
                            ui.checkbox(&mut vm.virtio, "Virtio disk, network and display (fastest for Linux)");
                            ui.end_row();
                        });
                    });
                    ui.add_space(4.0);
                    if icon_button(ui, Icon::Trash, "Delete virtual machine", false).clicked() {
                        delete = true;
                    }
                });
            });

            if create_disk {
                let _ = std::fs::create_dir_all(&self.settings.vm_dir);
                let default_name = format!("{}.img", self.settings.vms[idx].name.replace(' ', "_"));
                if let Some(path) = rfd::FileDialog::new().set_directory(&self.settings.vm_dir).set_file_name(&default_name).add_filter("Disk image", &["img"]).save_file() {
                    match vm::create_disk(&path, self.new_disk_gb) {
                        Ok(()) => {
                            self.settings.vms[idx].disk = path.to_string_lossy().into_owned();
                            self.notify(format!("Created {} GB disk", self.new_disk_gb));
                        }
                        Err(e) => self.notify(format!("Disk creation failed: {e}")),
                    }
                }
            }
            if start {
                let vm = self.settings.vms[idx].clone();
                let use_android_runner = vm.preset == "android-apk"
                    || vm.preset == "android-tablet-apk"
                    || (vm.iso.is_empty() && vm.kind() != DeviceKind::Computer);
                if use_android_runner {
                    let portrait = vm.kind() == DeviceKind::Phone;
                    self.notify(format!("Started {}", vm.name));
                    self.vm_session = Some(VmSession::start_android(&vm.name, portrait, Some(idx), &vm.apks));
                } else {
                    match VmSession::start(&vm.name, &vm.iso, &vm.disk, vm.ram_mb) {
                        Ok(s) => {
                            self.notify(format!("Started {}", vm.name));
                            self.vm_session = Some(s);
                        }
                        Err(e) => {
                            rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("Couldn't start VM").set_description(e).show();
                        }
                    }
                }
            }
            if let Some(p) = download {
                match vm::Download::start(p, &self.settings.vms[idx].name, Path::new(&self.settings.vm_dir)) {
                    Ok(d) => {
                        self.download = Some(d);
                        self.download_view = DownloadView::Running(0, 0, 0.0);
                    }
                    Err(e) => self.notify(e),
                }
            }
            if cancel_download {
                if let Some(mut d) = self.download.take() {
                    d.cancel();
                }
                self.download_view = DownloadView::Idle;
            }
            if stop {
                if let Some(mut c) = self.vm_procs.remove(&idx) {
                    let _ = c.kill();
                }
            }
            if delete {
                let vm = &self.settings.vms[idx];
                self.confirm_delete = Some(PendingDelete::Vm { idx, name: vm.name.clone(), disk: vm.disk.clone(), delete_disk: true });
            }
        });
        if let Some(p) = create_preset {
            self.create_from_preset(p);
        }
    }

    // ------------------------------------------------------------ remote play tab

    fn remote_play_ui(&mut self, ctx: &egui::Context) {
        use crate::remote_play::{
            self, ConsoleModel, ConsolePower, SavedRemoteConsole, CHIAKI_NG_URL, SONY_REMOTE_PLAY_URL,
        };

        let mut toast_msg: Option<String> = None;
        let mut save_console: Option<SavedRemoteConsole> = None;
        let mut remove_idx: Option<usize> = None;
        let mut wake_target: Option<(String, ConsoleModel, String)> = None;
        let mut connect_target: Option<Option<SavedRemoteConsole>> = None;

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(t().bg).inner_margin(Margin::symmetric(28, 22)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.set_max_width(920.0);

                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new("PS5 / PS4 Remote Play")
                                    .size(26.0)
                                    .family(egui::FontFamily::Name("heading".into()))
                                    .color(t().text),
                            );
                            ui.label(
                                RichText::new("Stream games directly from your own PlayStation 5 or PlayStation 4 over LAN or Internet.")
                                    .size(13.0)
                                    .color(t().muted),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ghost_button(ui, "Refresh clients").clicked() {
                                self.remote_play.refresh_clients(&self.settings.remote_play_client);
                                toast_msg = Some(format!("Found {} Remote Play client(s).", self.remote_play.clients.len()));
                            }
                            ui.add_space(6.0);
                            let scan_label = if self.remote_play.scanning { "Scanning..." } else { "Scan Network" };
                            if primary_button(ui, scan_label).clicked() && !self.remote_play.scanning {
                                self.remote_play.start_scan();
                            }
                        });
                    });

                    ui.add_space(18.0);

                    // 1. Discovered consoles on LAN
                    section(
                        ui,
                        "PlayStation Consoles on Your Network",
                        "Auto-discovers PS5 (UDP 9302) and PS4 (UDP 987) consoles via Sony's Device Discovery Protocol.",
                        |ui| {
                            if self.remote_play.scanning {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(RichText::new("Listening for PS5 / PS4 consoles on your local network...").color(t().muted));
                                });
                            } else if let Some(err) = &self.remote_play.scan_error {
                                ui.label(RichText::new(err).color(t().danger));
                            } else if self.remote_play.discovered.is_empty() {
                                ui.label(
                                    RichText::new(
                                        "No PlayStation consoles found on this LAN yet. Make sure your PS5 is on or in Rest Mode on the same network with Remote Play enabled, then click Scan Network.",
                                    )
                                    .color(t().muted),
                                );
                            } else {
                                for c in &self.remote_play.discovered {
                                    egui::Frame::NONE
                                        .fill(t().bg)
                                        .corner_radius(CornerRadius::same(8))
                                        .stroke(Stroke::new(1.0_f32, t().border))
                                        .inner_margin(Margin::symmetric(14, 10))
                                        .show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                pill(ui, c.model.label(), t().on_primary, t().primary);
                                                ui.add_space(6.0);
                                                ui.vertical(|ui| {
                                                    ui.horizontal(|ui| {
                                                        ui.label(RichText::new(&c.host_name).size(15.0).strong().color(t().text));
                                                        ui.label(RichText::new(&c.host_ip).size(12.5).color(t().muted));
                                                        let (p_fg, p_bg) = match c.power {
                                                            ConsolePower::Ready => (t().ok, t().surface),
                                                            ConsolePower::Standby => (Color32::from_rgb(0xF5, 0xB0, 0x41), t().surface),
                                                            ConsolePower::Unknown(_) => (t().muted, t().surface),
                                                        };
                                                        pill(ui, &c.power.label(), p_fg, p_bg);
                                                    });
                                                    let mut meta = Vec::new();
                                                    if !c.system_version.is_empty() {
                                                        meta.push(format!("Firmware {}", c.system_version));
                                                    }
                                                    if let Some(app) = &c.running_app {
                                                        meta.push(format!("Playing: {app}"));
                                                    }
                                                    if !c.host_id.is_empty() {
                                                        meta.push(format!("ID {}", c.host_id));
                                                    }
                                                    if !meta.is_empty() {
                                                        ui.label(RichText::new(meta.join("  â€¢  ")).size(12.0).color(t().muted));
                                                    }
                                                });
                                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                    let saved_ver = SavedRemoteConsole {
                                                        name: c.host_name.clone(),
                                                        host: c.host_ip.clone(),
                                                        model: c.model,
                                                        host_id: c.host_id.clone(),
                                                        regist_key: String::new(),
                                                        psn_account_id: String::new(),
                                                    };
                                                    if primary_button(ui, "Connect").clicked() {
                                                        connect_target = Some(Some(saved_ver.clone()));
                                                    }
                                                    ui.add_space(4.0);
                                                    if c.power == ConsolePower::Standby && ghost_button(ui, "Wake").clicked() {
                                                        wake_target = Some((c.host_ip.clone(), c.model, String::new()));
                                                    }
                                                    let already = self.settings.remote_consoles.iter().any(|s| s.host == c.host_ip);
                                                    if !already && ghost_button(ui, "Save").clicked() {
                                                        save_console = Some(saved_ver);
                                                    }
                                                });
                                            });
                                        });
                                    ui.add_space(6.0);
                                }
                            }
                        },
                    );

                    ui.add_space(14.0);

                    // 2. Saved consoles & manual entry
                    section(
                        ui,
                        "Saved Consoles",
                        "Pin your PS5 or PS4 by IP address for instant wake-up and one-click streaming.",
                        |ui| {
                            if self.settings.remote_consoles.is_empty() {
                                ui.label(RichText::new("No saved consoles yet. Save one from the network scan above or add an IP below.").color(t().muted));
                            } else {
                                for (i, c) in self.settings.remote_consoles.iter().enumerate() {
                                    egui::Frame::NONE
                                        .fill(t().bg)
                                        .corner_radius(CornerRadius::same(8))
                                        .stroke(Stroke::new(1.0_f32, t().border))
                                        .inner_margin(Margin::symmetric(14, 10))
                                        .show(ui, |ui| {
                                            ui.horizontal(|ui| {
                                                pill(ui, c.model.label(), t().on_primary, t().primary);
                                                ui.add_space(6.0);
                                                ui.vertical(|ui| {
                                                    ui.label(RichText::new(if c.name.is_empty() { &c.host } else { &c.name }).size(15.0).strong().color(t().text));
                                                    ui.label(RichText::new(&c.host).size(12.0).color(t().muted));
                                                });
                                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                    if ghost_button(ui, "Remove").clicked() {
                                                        remove_idx = Some(i);
                                                    }
                                                    ui.add_space(4.0);
                                                    if ghost_button(ui, "Wake").clicked() {
                                                        wake_target = Some((c.host.clone(), c.model, c.regist_key.clone()));
                                                    }
                                                    ui.add_space(4.0);
                                                    if primary_button(ui, "Connect").clicked() {
                                                        connect_target = Some(Some(c.clone()));
                                                    }
                                                });
                                            });
                                        });
                                    ui.add_space(6.0);
                                }
                            }

                            ui.add_space(8.0);
                            ui.label(RichText::new("Add console manually").size(13.5).strong().color(t().text));
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.remote_play.add_name).hint_text("Name (e.g. Living Room PS5)").desired_width(180.0));
                                ui.add(egui::TextEdit::singleline(&mut self.remote_play.add_host).hint_text("IP address (e.g. 192.168.1.50)").desired_width(190.0));
                                if segment(ui, self.remote_play.add_model == ConsoleModel::Ps5, "PS5").clicked() {
                                    self.remote_play.add_model = ConsoleModel::Ps5;
                                }
                                if segment(ui, self.remote_play.add_model == ConsoleModel::Ps4, "PS4").clicked() {
                                    self.remote_play.add_model = ConsoleModel::Ps4;
                                }
                                if primary_button(ui, "Add").clicked() {
                                    let host = self.remote_play.add_host.trim().to_string();
                                    if host.is_empty() {
                                        toast_msg = Some("Enter the console's IP address first.".into());
                                    } else {
                                        let name = self.remote_play.add_name.trim().to_string();
                                        save_console = Some(SavedRemoteConsole {
                                            name: if name.is_empty() { format!("{} ({host})", self.remote_play.add_model.label()) } else { name },
                                            host,
                                            model: self.remote_play.add_model,
                                            host_id: String::new(),
                                            regist_key: self.remote_play.add_regist_key.trim().to_string(),
                                            psn_account_id: self.remote_play.add_psn_id.trim().to_string(),
                                        });
                                        self.remote_play.add_name.clear();
                                        self.remote_play.add_host.clear();
                                        self.remote_play.add_regist_key.clear();
                                        self.remote_play.add_psn_id.clear();
                                    }
                                }
                            });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut self.remote_play.add_regist_key).hint_text("Optional Regist Key (for Wake-on-LAN / chiaki-ng)").desired_width(290.0));
                                ui.add(egui::TextEdit::singleline(&mut self.remote_play.add_psn_id).hint_text("Optional PSN Account ID (base64)").desired_width(240.0));
                            });
                        },
                    );

                    ui.add_space(14.0);

                    // 3. Remote Play Client & Setup
                    section(
                        ui,
                        "Streaming Client",
                        "Uses chiaki-ng (open-source, low-latency HDR/1080p60) or Sony's official PS Remote Play app.",
                        |ui| {
                            if self.remote_play.clients.is_empty() {
                                ui.label(
                                    RichText::new("No Remote Play client was detected on this PC yet. Install chiaki-ng or Sony PS Remote Play, or pick an executable manually:")
                                        .color(t().muted),
                                );
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    if primary_button(ui, "Download chiaki-ng").clicked() {
                                        remote_play::open_url(CHIAKI_NG_URL);
                                    }
                                    ui.add_space(6.0);
                                    if ghost_button(ui, "Download Sony PS Remote Play").clicked() {
                                        remote_play::open_url(SONY_REMOTE_PLAY_URL);
                                    }
                                    ui.add_space(6.0);
                                    if ghost_button(ui, "Locate executable...").clicked() {
                                        if let Some(p) = rfd::FileDialog::new().set_title("Choose Remote Play client").pick_file() {
                                            self.settings.remote_play_client = p.to_string_lossy().into_owned();
                                            self.remote_play.refresh_clients(&self.settings.remote_play_client);
                                        }
                                    }
                                });
                            } else {
                                ui.horizontal_wrapped(|ui| {
                                    for (i, cl) in self.remote_play.clients.iter().enumerate() {
                                        if segment(ui, self.remote_play.selected_client == i, &cl.name).clicked() {
                                            self.remote_play.selected_client = i;
                                        }
                                    }
                                });
                                if let Some(cl) = self.remote_play.active_client() {
                                    ui.add_space(6.0);
                                    ui.label(RichText::new(format!("Executable: {}", cl.path.display())).size(12.0).color(t().muted));
                                }
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    if primary_button(ui, "Launch Remote Play").clicked() {
                                        connect_target = Some(None);
                                    }
                                    ui.add_space(6.0);
                                    if ghost_button(ui, "Change executable...").clicked() {
                                        if let Some(p) = rfd::FileDialog::new().set_title("Choose Remote Play client").pick_file() {
                                            self.settings.remote_play_client = p.to_string_lossy().into_owned();
                                            self.remote_play.refresh_clients(&self.settings.remote_play_client);
                                        }
                                    }
                                    if !self.settings.remote_play_client.is_empty() && ghost_button(ui, "Clear custom").clicked() {
                                        self.settings.remote_play_client.clear();
                                        self.remote_play.refresh_clients("");
                                    }
                                });
                            }

                            ui.add_space(10.0);
                            ui.label(RichText::new("On your PS5: Settings > System > Remote Play > Enable Remote Play. For Rest Mode wakeup, also enable Stay Connected to the Internet and Enable Turning On PS5 from Network under Power Saving.").size(12.0).color(t().muted));
                        },
                    );
                });
            });

        if let Some(c) = save_console {
            let name = c.name.clone();
            self.settings.remote_consoles.push(c);
            self.notify(format!("Saved {name} to Remote Play."));
        }
        if let Some(i) = remove_idx {
            if i < self.settings.remote_consoles.len() {
                let removed = self.settings.remote_consoles.remove(i);
                self.notify(format!("Removed {}.", removed.name));
            }
        }
        if let Some((host, model, key)) = wake_target {
            match remote_play::wake_console(&host, model, &key) {
                Ok(()) => self.notify(format!("Sent wake-up signal to {host}.")),
                Err(e) => self.notify(e),
            }
        }
        if let Some(target) = connect_target {
            if let Some(client) = self.remote_play.active_client().cloned() {
                match remote_play::launch_client(&client, target.as_ref()) {
                    Ok(msg) => self.notify(msg),
                    Err(e) => self.notify(e),
                }
            } else {
                self.notify("Install chiaki-ng or Sony PS Remote Play (or locate its executable) to stream.");
            }
        }
        if let Some(msg) = toast_msg {
            self.notify(msg);
        }
    }

    // ------------------------------------------------------------ settings tab


    fn settings_ui(&mut self, ctx: &egui::Context) {
        // Capture the next key press when rebinding.
        if let Some(field) = self.rebinding {
            let pressed = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key { key, pressed: true, .. } => Some(*key),
                    _ => None,
                })
            });
            if let Some(k) = pressed {
                if k != egui::Key::Escape {
                    *binding_mut(&mut self.settings.keys, field) = k.name().to_string();
                }
                self.rebinding = None;
            }
        }

        let mut open_global = false;
        content(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                // A centred column: equal space on both sides at any window width.
                let col = ui.available_width().min(860.0);
                let pad = ((ui.available_width() - col) / 2.0).max(0.0);
                ui.horizontal_top(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                ui.set_width(col);
                ui.label(RichText::new("Settings").size(28.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                ui.add_space(8.0);

                section(ui, "Controls", "The global layout is used by every game without custom controls. Games can get their own layout from the player's Controls button or by right-clicking them.", |ui| {
                    ui.horizontal(|ui| {
                        if icon_button(ui, Icon::Gamepad, "Edit global controls", true).clicked() {
                            open_global = true;
                        }
                        let k = &self.settings.keys;
                        ui.label(
                            RichText::new(format!("A {}, B {}, Start {}, Select {}", pretty_key(&k.a), pretty_key(&k.b), pretty_key(&k.start), pretty_key(&k.select)))
                                .size(12.5)
                                .color(t().muted),
                        );
                    });
                    let custom = self.settings.game_keys.len();
                    if custom > 0 {
                        ui.add_space(4.0);
                        ui.label(RichText::new(format!("{custom} game{} use custom controls.", if custom == 1 { "" } else { "s" })).size(12.0).color(t().muted));
                    }
                });

                section(ui, "Gamepad", "Xbox, PlayStation, Switch Pro and generic controllers.", |ui| {
                    ui.checkbox(&mut self.settings.gamepad_enabled, "Enable gamepads");
                    ui.add_space(4.0);
                    ui.checkbox(&mut self.settings.auto_pad_layout, "Pick the button layout automatically for each console");
                    ui.label(
                        RichText::new("Nintendo consoles put A on the right and B at the bottom, Sega pads use the bottom row, and so on, whatever controller you plug in. Each game's Controls button shows the result and lets you change it.")
                            .size(12.0)
                            .color(t().muted),
                    );
                    if !self.settings.auto_pad_layout {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label("Fixed layout");
                            ui.add_space(8.0);
                            let pad = &mut self.settings.keys.pad;
                            if segment(ui, *pad == crate::settings::PadBindings::default(), "Xbox / PlayStation").clicked() {
                                *pad = crate::settings::PadBindings::default();
                            }
                            if segment(ui, *pad == crate::settings::PadBindings::nintendo(), "Nintendo").clicked() {
                                *pad = crate::settings::PadBindings::nintendo();
                            }
                        });
                        ui.label(RichText::new("Used for every game. Every button can also be remapped individually in the controls editor.").size(12.0).color(t().muted));
                    }
                    ui.add_space(4.0);
                    ui.checkbox(&mut self.settings.stick_as_dpad, "Left stick controls the D-pad");
                    ui.add_enabled(self.settings.stick_as_dpad, egui::Slider::new(&mut self.settings.stick_deadzone, 0.1..=0.9).text("Stick deadzone"));
                    ui.add_space(8.0);
                    ui.label(RichText::new("Connected controllers").strong());
                    if let Some(g) = self.gilrs.as_mut() {
                        while g.next_event().is_some() {}
                        let pads: Vec<String> = g
                            .gamepads()
                            .filter(|(_, p)| p.is_connected())
                            .enumerate()
                            .map(|(i, (_, p))| format!("Player {} â€” {} ({} layout)", i + 1, p.name(), controls::detect(p.name(), p.vendor_id()).name()))
                            .collect();
                        if pads.is_empty() {
                            ui.label(RichText::new("None detected. Plug in a controller or pair it over Bluetooth.").color(t().muted));
                        }
                        for p in pads {
                            ui.label(RichText::new(p).color(t().ok));
                        }
                    }
                });

                section(ui, "Saving and rewind", "Keep your progress and take back mistakes.", |ui| {
                    ui.label(RichText::new("Game saves (battery saves, memory cards) are written to disk every few seconds and when you close the game or the app.").size(12.5).color(t().muted));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "Save states: press {} to save, {} to load, {} to switch between 9 slots per game. The States button in the player shows them all.",
                            self.settings.keys.save_state, self.settings.keys.load_state, self.settings.keys.next_slot
                        ))
                        .size(12.5)
                        .color(t().muted),
                    );
                    ui.add_space(6.0);
                    ui.checkbox(&mut self.settings.rewind_enabled, "Rewind");
                    ui.add_enabled(self.settings.rewind_enabled, egui::Slider::new(&mut self.settings.rewind_seconds, 5..=60).text("History").suffix(" s"));
                    ui.label(
                        RichText::new(format!("Hold {} (or L3 on a controller) while playing to run backwards through the last few seconds. Uses a little memory.", self.settings.keys.rewind))
                            .size(12.0)
                            .color(t().muted),
                    );
                });

                section(ui, "Video", "How games are drawn on screen.", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Aspect ratio");
                        ui.add_space(8.0);
                        ui.selectable_value(&mut self.settings.aspect, Aspect::Square, "Square pixels");
                        ui.selectable_value(&mut self.settings.aspect, Aspect::Tv, "4:3 TV");
                    });
                    ui.checkbox(&mut self.settings.integer_scaling, "Integer scaling (sharpest pixels, may leave borders)");
                    ui.checkbox(&mut self.settings.sharp_pixels, "Sharp pixels (crisp, evenly sized pixels at any window size)");
                    if ui.checkbox(&mut self.settings.vr_side_by_side, "Virtual Boy 3D: both eyes side by side (for VR headsets and 3D displays)").changed() {
                        if let Some(s) = self.session.as_mut() {
                            s.core.set_stereo(self.settings.vr_side_by_side);
                        }
                    }
                    ui.label(
                        RichText::new("In a headset, show this window with a side-by-side 3D mode (for example SteamVR's desktop view or Virtual Desktop's SBS 3D) to see the real depth.")
                            .size(12.0)
                            .color(t().muted),
                    );
                    ui.checkbox(&mut self.settings.smooth_filter, "Soft filtering (blurs pixels, like an old TV)");
                    ui.checkbox(&mut self.settings.show_fps, "Show FPS");
                });

                section(ui, "Performance", "Frame rate, smoothness and speed.", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Display frame rate");
                        ui.add_space(8.0);
                        for fps in [0, 30, 60, 120, 144, 240] {
                            let label = if fps == 0 { "Match monitor".to_string() } else { fps.to_string() };
                            ui.selectable_value(&mut self.settings.target_fps, fps, label);
                        }
                    });
                    ui.checkbox(&mut self.settings.smooth_motion, "Smooth motion: blend between game frames on 120/144/240 Hz monitors");
                    ui.label(
                        RichText::new("Games always run at their real speed, whatever the display rate; the screen just refreshes more often. Smooth motion softens movement a little.")
                            .size(12.0)
                            .color(t().muted),
                    );
                    ui.add(egui::Slider::new(&mut self.settings.game_speed, 25..=300).text("Game speed").suffix("%"));
                    ui.label(RichText::new("100% is the console's real speed. Change it only if you want slow motion or a speed-up.").size(12.0).color(t().muted));
                    ui.horizontal(|ui| {
                        ui.label("Frame skip");
                        ui.add_space(8.0);
                        for (n, l) in [(0, "Off"), (1, "1"), (2, "2"), (3, "3")] {
                            ui.selectable_value(&mut self.settings.frame_skip, n, l);
                        }
                    });
                    ui.label(RichText::new("Draws fewer frames on slow PCs; the game itself still runs at full speed.").size(12.0).color(t().muted));
                    ui.checkbox(&mut self.settings.vsync, "VSync: match your monitor's refresh, no tearing (applies after restart)");
                    ui.checkbox(&mut self.settings.pause_unfocused, "Pause games when the window is in the background");
                    ui.add(egui::Slider::new(&mut self.settings.fast_forward_speed, 2..=10).text("Fast-forward speed").suffix("Ã—"));
                });

                section(ui, "BIOS files", "Some consoles start from their own BIOS or firmware. Add the files dumped from your consoles here; each console can have several (for example different regions) and you choose which one it uses.", |ui| {
                    let mut act: Option<(&'static str, BiosAction)> = None;
                    for def in cores::BIOS_DEFS {
                        let files = cores::installed_bios(def.id);
                        let active = cores::bios_for(def.id);
                        egui::Frame::NONE.fill(t().surface).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(12)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(def.name).size(15.0).strong().color(t().text));
                                match (&active, def.used) {
                                    (Some(_), true) => pill(ui, "Ready", t().ok, t().panel),
                                    (Some(_), false) => pill(ui, "Saved for when the core arrives", t().muted, t().panel),
                                    (None, true) => pill(ui, "Needed", t().danger, t().panel),
                                    (None, false) => pill(ui, "Not needed yet", t().muted, t().panel),
                                }
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if icon_button(ui, Icon::Plus, "Add BIOS", def.used && active.is_none()).clicked() {
                                        act = Some((def.id, BiosAction::Add));
                                    }
                                    if def.id == "ps1" {
                                        let searching = self.bios_search.is_some();
                                        if ui.add_enabled_ui(!searching, |ui| icon_button(ui, Icon::Search, "Find on this PC", false)).inner.clicked() {
                                            act = Some((def.id, BiosAction::Search));
                                        }
                                        if searching {
                                            ui.spinner();
                                        }
                                    }
                                });
                            });
                            ui.label(RichText::new(def.hint).size(12.0).color(t().muted));
                            for f in &files {
                                ui.horizontal(|ui| {
                                    let is_active = active.as_ref() == Some(f);
                                    if ui.radio(is_active, RichText::new(f.file_name().unwrap_or_default().to_string_lossy()).color(t().text)).clicked() && !is_active {
                                        act = Some((def.id, BiosAction::Use(f.clone())));
                                    }
                                    if def.id == "ps1" {
                                        if let Some(region) = crate::tools::is_ps1_bios(f) {
                                            pill(ui, region, t().muted, t().panel);
                                        }
                                    }
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ghost_button(ui, "Remove").clicked() {
                                            act = Some((def.id, BiosAction::Remove(f.clone())));
                                        }
                                    });
                                });
                            }
                            if def.id == "ps1" {
                                if let Some(found) = &self.bios_found {
                                    let new: Vec<_> = found.iter().filter(|b| !files.iter().any(|f| f.file_name() == b.path.file_name())).collect();
                                    if found.is_empty() {
                                        ui.label(RichText::new("No PlayStation BIOS found on this PC.").size(12.0).color(t().muted));
                                    } else if new.is_empty() {
                                        ui.label(RichText::new("Every BIOS found on this PC is already added.").size(12.0).color(t().muted));
                                    }
                                    for b in new {
                                        ui.horizontal(|ui| {
                                            ui.label(RichText::new(format!("Found: {}", b.path.file_name().unwrap_or_default().to_string_lossy())).color(t().text));
                                            pill(ui, b.region, t().muted, t().panel);
                                            ui.add(egui::Label::new(RichText::new(b.path.parent().map(|p| p.display().to_string()).unwrap_or_default()).size(11.5).color(t().muted)).truncate());
                                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                if ghost_button(ui, "Add").clicked() {
                                                    act = Some(("ps1", BiosAction::AddPath(b.path.clone())));
                                                }
                                            });
                                        });
                                    }
                                }
                            }
                            if def.id == "fds" && active.is_some() {
                                ui.label(RichText::new("While playing, press L to flip or change the disk side when the game asks.").size(12.0).color(t().muted));
                            }
                        });
                        ui.add_space(8.0);
                    }
                    ui.horizontal(|ui| {
                        if ghost_button(ui, "Open BIOS folder").clicked() {
                            let _ = std::fs::create_dir_all(cores::bios_dir());
                            crate::platform::reveal(&cores::bios_dir());
                        }
                        ui.label(RichText::new("BIOS files are copyrighted by the console makers, so the app can't download them; it can find dumps you already have.").size(12.0).color(t().muted));
                    });
                    if let Some((id, a)) = act {
                        let name = cores::BIOS_DEFS.iter().find(|d| d.id == id).map(|d| d.name).unwrap_or("");
                        match a {
                            BiosAction::Add => {
                                if let Some(f) = rfd::FileDialog::new().set_title(format!("{name} BIOS")).pick_file() {
                                    match cores::install_bios(id, &f) {
                                        Ok(_) => self.notify(format!("{name} BIOS added")),
                                        Err(e) => self.notify(e),
                                    }
                                }
                            }
                            BiosAction::AddPath(f) => match cores::install_bios(id, &f) {
                                Ok(_) => self.notify(format!("{name} BIOS added")),
                                Err(e) => self.notify(e),
                            },
                            BiosAction::Use(f) => {
                                cores::set_active_bios(id, &f);
                                self.notify(format!("{name} now uses {}", f.file_name().unwrap_or_default().to_string_lossy()));
                            }
                            BiosAction::Remove(f) => match std::fs::remove_file(&f) {
                                Ok(()) => self.notify("BIOS removed"),
                                Err(e) => self.notify(format!("Couldn't remove it: {e}")),
                            },
                            BiosAction::Search => {
                                let (tx, rx) = std::sync::mpsc::channel();
                                std::thread::spawn(move || {
                                    let _ = tx.send(crate::tools::find_ps1_bios());
                                });
                                self.bios_search = Some(rx);
                                self.bios_found = None;
                            }
                        }
                    }
                });

                section(ui, "PS4, PS5 and Xbox One/Series", "Run decrypted programs on our CPU core, convert them into native Windows .exe binaries, or inspect console firmware.", |ui| {
                    ui.radio_value(&mut self.settings.next_gen_mode, "emulate".to_string(), "Emulator: run on our own CPU core (works on every platform)");
                    ui.radio_value(&mut self.settings.next_gen_mode, "native".to_string(), "Native: convert the program to run on this PC's processor (faster, desktop only)");
                    crate::cores::set_native_next_gen(self.settings.next_gen_mode == "native");
                    ui.add_space(10.0);
                    let mut notice: Option<String> = None;
                    ui.horizontal_wrapped(|ui| {
                        if primary_button(ui, "Convert Xbox .exe -> Windows .exe").clicked() {
                            if let Some(src) = rfd::FileDialog::new().add_filter("Xbox Executable", &["exe"]).pick_file() {
                                let res = std::fs::read(&src)
                                    .map_err(|e| e.to_string())
                                    .and_then(|b| xboxone::exe_convert::convert_xbox_pe_to_windows(&b));
                                match res {
                                    Ok(conv) => {
                                        let out = src.with_file_name(format!("{}_windows.exe", src.file_stem().unwrap_or_default().to_string_lossy()));
                                        if let Err(e) = std::fs::write(&out, &conv.windows_pe) {
                                            notice = Some(format!("Failed to write {}: {e}", out.display()));
                                        } else {
                                            let dlls = if conv.rewritten_dlls.is_empty() {
                                                "none needed".to_string()
                                            } else {
                                                conv.rewritten_dlls.iter().map(|(a, b)| format!("{a}->{b}")).collect::<Vec<_>>().join(", ")
                                            };
                                            rfd::MessageDialog::new()
                                                .set_level(rfd::MessageLevel::Info)
                                                .set_title("Xbox -> Windows .exe Ready")
                                                .set_description(format!(
                                                    "Saved: {}\n\nRewritten Xbox DLLs: {}\nTotal imports: {} (Forwarded: {}, Shimmed: {}, Stubbed: {}, Missing: {})",
                                                    out.display(),
                                                    dlls,
                                                    conv.coverage.total_imports,
                                                    conv.coverage.forwarded,
                                                    conv.coverage.implemented,
                                                    conv.coverage.stubbed,
                                                    conv.coverage.missing
                                                ))
                                                .show();
                                            notice = Some(format!("Converted {}", out.file_name().unwrap_or_default().to_string_lossy()));
                                        }
                                    }
                                    Err(e) => {
                                        rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("Xbox conversion failed").set_description(e).show();
                                    }
                                }
                            }
                        }
                        if ghost_button(ui, "Convert PS5/PS4 ELF -> Windows .exe").clicked() {
                            if let Some(src) = rfd::FileDialog::new().add_filter("PlayStation ELF / eboot.bin", &["bin", "elf", "oelf"]).pick_file() {
                                let res = std::fs::read(&src)
                                    .map_err(|e| e.to_string())
                                    .and_then(|b| ps5::exe_convert::convert_ps5_elf_to_windows_exe(&b));
                                match res {
                                    Ok(conv) => {
                                        let out = src.with_file_name(format!("{}_windows.exe", src.file_stem().unwrap_or_default().to_string_lossy()));
                                        if let Err(e) = std::fs::write(&out, &conv.windows_pe) {
                                            notice = Some(format!("Failed to write {}: {e}", out.display()));
                                        } else {
                                            rfd::MessageDialog::new()
                                                .set_level(rfd::MessageLevel::Info)
                                                .set_title("PS5/PS4 -> Windows .exe Ready")
                                                .set_description(format!(
                                                    "Saved: {}\n\nRelinked fs: TLS/canary sites: {}\nImported PlayStation libraries: {} ({} NIDs)",
                                                    out.display(),
                                                    conv.relinked_fs_sites,
                                                    conv.imported_libs.len(),
                                                    conv.imported_nids
                                                ))
                                                .show();
                                            notice = Some(format!("Converted {}", out.file_name().unwrap_or_default().to_string_lossy()));
                                        }
                                    }
                                    Err(e) => {
                                        rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("PS5 conversion failed").set_description(e).show();
                                    }
                                }
                            }
                        }
                        if ghost_button(ui, "Inspect / Boot Firmware (Xbox / PS5)").clicked() {
                            if let Some(src) = rfd::FileDialog::new().add_filter("Console Firmware", &["xvd", "bin", "pup", "rom"]).pick_file() {
                                match std::fs::read(&src) {
                                    Ok(bytes) => {
                                        let report = xboxone::firmware::inspect_firmware(&bytes)
                                            .map(|f| f.summary)
                                            .or_else(|_| ps5::firmware::inspect_pup(&bytes).map(|p| p.summary));
                                        match report {
                                            Ok(summary) => {
                                                rfd::MessageDialog::new()
                                                    .set_level(rfd::MessageLevel::Info)
                                                    .set_title("Console Firmware Verified")
                                                    .set_description(summary)
                                                    .show();
                                            }
                                            Err(e) => {
                                                rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("Unknown firmware").set_description(e).show();
                                            }
                                        }
                                    }
                                    Err(e) => notice = Some(format!("Couldn't read firmware: {e}")),
                                }
                            }
                        }
                    });
                    if let Some(msg) = notice {
                        self.notify(msg);
                    }
                });

                section(ui, "Audio", "Sound from the console's audio chip, played through your default output device.", |ui| {
                    match &self.audio {
                        Some(a) => {
                            ui.add_enabled(!self.settings.muted, egui::Slider::new(&mut self.settings.volume, 0.0..=1.0).text("Volume").custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
                            ui.checkbox(&mut self.settings.muted, "Mute");
                            ui.label(RichText::new(format!("Output: {} Hz stereo. Audio is skipped while fast-forwarding.", a.sample_rate)).size(12.0).color(t().muted));
                        }
                        None => {
                            ui.label(RichText::new("No audio output device was found. Plug in speakers or headphones and restart the app.").color(t().danger));
                        }
                    }
                });

                section(ui, "Paths", "Where things live on disk.", |ui| {
                    egui::Grid::new("paths").num_columns(3).spacing([16.0, 8.0]).show(ui, |ui| {
                        ui.label("Virtual machine storage");
                        ui.add(egui::TextEdit::singleline(&mut self.settings.vm_dir).desired_width(340.0));
                        if ghost_button(ui, "Browse").clicked() {
                            if let Some(d) = rfd::FileDialog::new().pick_folder() {
                                self.settings.vm_dir = d.to_string_lossy().into_owned();
                            }
                        }
                        ui.end_row();
                    });
                });

                section(ui, "About", "", |ui| {
                    ui.label(RichText::new(t().full_name()).size(16.0).family(egui::FontFamily::Name("heading".into())).color(t().primary));
                    ui.label(RichText::new("Multi-system emulator and virtual machine manager.").color(t().text));
                    ui.label(RichText::new("C++ CPU cores with Rust systems and interface.").size(12.0).color(t().muted));
                    ui.label(RichText::new("Use only games, BIOS and firmware dumped from hardware you own.").size(12.0).color(t().muted));
                });
                });
                });
            });
        });
        if open_global {
            self.open_controls(Path::new(""), "All games", "nes");
        }
    }
}

fn step_badge(ui: &mut Ui, n: usize, done: bool) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
    let p = ui.painter();
    if done {
        p.circle_filled(r.center(), 12.0, t().ok);
        let c = r.center();
        p.line_segment([c + Vec2::new(-5.0, 0.0), c + Vec2::new(-1.5, 4.0)], Stroke::new(2.2_f32, t().bg));
        p.line_segment([c + Vec2::new(-1.5, 4.0), c + Vec2::new(5.5, -4.5)], Stroke::new(2.2_f32, t().bg));
    } else {
        p.circle_stroke(r.center(), 11.0, Stroke::new(1.5_f32, t().primary));
        p.text(r.center(), Align2::CENTER_CENTER, n.to_string(), FontId::proportional(12.5), t().text);
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / 1_048_576.0)
}

/// Guided setup for template VMs (e.g. Arch Linux): installer, disk, install, first boot.
#[allow(clippy::too_many_arguments)]
fn setup_card(
    ui: &mut Ui,
    p: &'static vm::Preset,
    vm: &mut VmConfig,
    running: bool,
    downloading: bool,
    state: &DownloadView,
    download: &mut Option<&'static vm::Preset>,
    cancel: &mut bool,
) {
    let iso_ok = !vm.iso.is_empty() && Path::new(&vm.iso).exists();
    let disk_ok = !vm.disk.is_empty() && Path::new(&vm.disk).exists();
    section(ui, &format!("{} setup", p.name), p.description, |ui| {
        // 1. Installer
        ui.horizontal(|ui| {
            step_badge(ui, 1, iso_ok);
            ui.vertical(|ui| {
                let android = p.kind != DeviceKind::Computer;
                ui.label(RichText::new(if android { "Android image" } else { "Installer ISO" }).strong().color(t().text));
                if iso_ok {
                    ui.label(RichText::new(format!("Attached: {}", Path::new(&vm.iso).file_name().unwrap_or_default().to_string_lossy())).size(12.0).color(t().muted));
                    return;
                }
                match (downloading, state) {
                    (true, DownloadView::Running(done, total, rate)) => {
                        let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
                        ui.add(egui::ProgressBar::new(frac).desired_width(360.0).fill(t().primary).corner_radius(CornerRadius::same(4)));
                        let eta = if *rate > 0.0 && *total > *done { format!(", {:.0} s left", (*total - *done) as f64 / rate) } else { String::new() };
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} of {}, {:.1} MB/s{eta}", mb(*done), if *total > 0 { mb(*total) } else { "?".into() }, rate / 1_048_576.0)).size(12.0).color(t().muted));
                            if ui.small_button("Cancel").clicked() {
                                *cancel = true;
                            }
                        });
                    }
                    (true, _) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new("Verifying the download against the official checksumâ€¦").size(12.0).color(t().muted));
                        });
                    }
                    (false, DownloadView::Failed(e)) => {
                        ui.label(RichText::new(e).size(12.0).color(t().danger));
                        if icon_button(ui, Icon::Reset, "Retry download", true).clicked() {
                            *download = Some(p);
                        }
                    }
                    _ => {
                        let note = if android {
                            "Official Android-x86 9.0 image (~900 MB) from SourceForge, checked against the SHA-1 published by android-x86.org. Or attach your own below."
                        } else {
                            "Official image from archlinux.org mirrors (~1.4 GB), checksum-verified. Or attach your own under Boot media."
                        };
                        ui.label(RichText::new(note).size(12.0).color(t().muted));
                        ui.add_space(2.0);
                        let label = if android { "Download Android".to_string() } else { format!("Download {} ISO", p.name) };
                        if icon_button(ui, Icon::Disk, &label, true).clicked() {
                            *download = Some(p);
                        }
                    }
                }
            });
        });
        ui.add_space(8.0);
        // 2. Disk
        ui.horizontal(|ui| {
            step_badge(ui, 2, disk_ok);
            ui.vertical(|ui| {
                ui.label(RichText::new("Virtual disk").strong().color(t().text));
                ui.label(
                    RichText::new(if disk_ok { format!("{} GB qcow2 disk ready (grows as you use it)", p.disk_gb) } else { "Create one under Hard disk below.".into() })
                        .size(12.0)
                        .color(t().muted),
                );
            });
        });
        ui.add_space(8.0);
        // 3. Install
        if p.kind != DeviceKind::Computer {
            ui.horizontal(|ui| {
                step_badge(ui, 3, false);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Install Android").strong().color(t().text));
                    ui.label(
                        RichText::new(
                            "Press Start and choose Installation. Create a partition (New, Primary, Bootable, Write, Quit), format it as ext4, \
                             install GRUB and make /system read-write. Or choose Live CD to try Android without installing.",
                        )
                        .size(12.0)
                        .color(t().muted),
                    );
                });
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                step_badge(ui, 4, false);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Use it").strong().color(t().text));
                    ui.label(
                        RichText::new("Pick Run Android-x86. Your mouse acts as touch. Install apps from a browser or app store inside Android, or with adb: adb connect localhost:5555")
                            .size(12.0)
                            .color(t().muted),
                    );
                    if iso_ok && !running && icon_button(ui, Icon::Disk, "Detach image", false).clicked() {
                        vm.iso.clear();
                    }
                });
            });
            return;
        }
        ui.horizontal(|ui| {
            step_badge(ui, 3, false);
            ui.vertical(|ui| {
                ui.label(RichText::new("Install").strong().color(t().text));
                ui.label(RichText::new("Press Start, pick \"Arch Linux install medium\", then at the root prompt run:").size(12.0).color(t().muted));
                egui::Frame::NONE.fill(t().bg).stroke(Stroke::new(1.0_f32, t().border)).corner_radius(CornerRadius::same(6)).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("archinstall").monospace().color(t().primary));
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text("archinstall".into());
                        }
                    });
                });
                ui.label(
                    RichText::new("Guided installer. Choose the virtio disk (/dev/vda), a bootloader (systemd-boot works with UEFI), a profile (e.g. Desktop â€º KDE Plasma or GNOME) and NetworkManager.")
                        .size(12.0)
                        .color(t().muted),
                );
            });
        });
        ui.add_space(8.0);
        // 4. First boot
        ui.horizontal(|ui| {
            step_badge(ui, 4, false);
            ui.vertical(|ui| {
                ui.label(RichText::new("Boot the installed system").strong().color(t().text));
                ui.label(RichText::new("After archinstall finishes, power off, detach the ISO and start again.").size(12.0).color(t().muted));
                if iso_ok && !running && icon_button(ui, Icon::Disk, "Detach ISO", false).clicked() {
                    vm.iso.clear();
                }
            });
        });
    });
}

/// Fills `rect` with a vertical two-colour gradient.
fn gradient_rect(p: &egui::Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(egui::Shape::mesh(mesh));
}

/// Draws a phone or tablet: bezel, a lit screen with a clock and app grid (or dark when off).
fn paint_device(p: &egui::Painter, rect: Rect, landscape: bool, on: bool) {
    let r = (rect.width().min(rect.height()) * 0.12) as u8;
    p.rect_filled(rect, CornerRadius::same(r.max(6)), Color32::from_rgb(0x14, 0x14, 0x1B));
    p.rect_stroke(rect, CornerRadius::same(r.max(6)), Stroke::new(1.5_f32, t().border), egui::StrokeKind::Inside);
    let bezel = rect.width().min(rect.height()) * 0.05;
    let screen = rect.shrink(bezel);
    if on {
        p.rect_filled(screen, CornerRadius::same((r / 2).max(3)), t().bg);
        gradient_rect(p, screen.shrink(1.0), t().primary.gamma_multiply(0.55), t().secondary.gamma_multiply(0.35));
        // Clock and a grid of app icons.
        let scale = screen.width().min(screen.height());
        p.text(screen.center_top() + Vec2::new(0.0, scale * 0.12), Align2::CENTER_TOP, "12:30", FontId::proportional((scale * 0.14).max(9.0)), Color32::WHITE);
        let (cols, rows) = if landscape { (6, 2) } else { (4, 4) };
        let icon = scale * if landscape { 0.12 } else { 0.13 };
        let gap = icon * 0.5;
        let gw = cols as f32 * icon + (cols - 1) as f32 * gap;
        let top = screen.top() + screen.height() * if landscape { 0.45 } else { 0.42 };
        for row in 0..rows {
            for col in 0..cols {
                let x = screen.center().x - gw / 2.0 + col as f32 * (icon + gap);
                let y = top + row as f32 * (icon + gap);
                let ir = Rect::from_min_size(egui::pos2(x, y), Vec2::splat(icon));
                if ir.bottom() < screen.bottom() - 2.0 {
                    p.rect_filled(ir, CornerRadius::same((icon * 0.28) as u8), Color32::from_white_alpha(if (row + col) % 3 == 0 { 70 } else { 40 }));
                }
            }
        }
    } else {
        p.rect_filled(screen, CornerRadius::same((r / 2).max(3)), Color32::from_rgb(0x07, 0x07, 0x0B));
    }
    // Camera dot.
    let cam = if landscape { egui::pos2(rect.left() + bezel * 0.5, rect.center().y) } else { egui::pos2(rect.center().x, rect.top() + bezel * 0.5) };
    p.circle_filled(cam, (bezel * 0.18).max(1.5), Color32::from_gray(60));
}

/// Landing view of the Phones & Tablets tab. Returns the template the user picked.
/// A change requested from the BIOS files section.
enum BiosAction {
    Add,
    AddPath(PathBuf),
    Use(PathBuf),
    Remove(PathBuf),
    Search,
}

fn devices_hero(ui: &mut Ui, open_ios: &mut bool) -> Option<&'static vm::Preset> {
    let mut pick = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(8.0);
        ui.label(RichText::new("Phones & Tablets").size(28.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
        ui.label(RichText::new("Run Android in a phone- or tablet-shaped window. Install apps from F-Droid or other stores inside Android, or from APK files.").color(t().muted));
        ui.add_space(18.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            for p in vm::PRESETS.iter().filter(|p| p.kind != DeviceKind::Computer) {
                let (card, _) = ui.allocate_exact_size(Vec2::new(300.0, 360.0), Sense::hover());
                let painter = ui.painter_at(card);
                painter.rect(card, CornerRadius::same(16), t().panel, Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
                let landscape = p.kind == DeviceKind::Tablet;
                let dev = if landscape { Vec2::new(200.0, 132.0) } else { Vec2::new(98.0, 180.0) };
                let area = Rect::from_min_size(card.min + Vec2::new(0.0, 20.0), Vec2::new(card.width(), 190.0));
                paint_device(&painter, Rect::from_center_size(area.center(), dev), landscape, true);
                let mut child = ui.new_child(
                    egui::UiBuilder::new().max_rect(Rect::from_min_max(card.min + Vec2::new(20.0, 222.0), card.max - Vec2::new(20.0, 16.0))).layout(Layout::top_down(Align::Min)),
                );
                child.label(RichText::new(p.name).size(18.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                child.label(RichText::new(format!("{} Ã— {} screen Â· {} GB RAM Â· {} GB storage", p.xres, p.yres, p.ram_mb / 1024, p.disk_gb)).size(12.0).color(t().muted));
                child.add_space(10.0);
                let label = if landscape { "Create tablet" } else { "Create phone" };
                if child.add_enabled(false, primary(egui::Button::new(RichText::new(label).strong().color(t().on_primary))).min_size(Vec2::new(150.0, 36.0))).clicked() {
                    pick = Some(p);
                }
            }
        });
        ui.add_space(14.0);
        pill(ui, "Coming soon", t().primary, t().surface);
        ui.label(RichText::new("Android phones and tablets arrive with our own 64-bit PC core; no outside software is used.").size(12.0).color(t().muted));
        ui.add_space(10.0);
        ui.add_space(8.0);
        ui.label(RichText::new("iPhone & iPad").size(20.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
        ui.label(RichText::new("Classic iPhone and iPad apps (.ipa) on our own core, no Apple firmware needed.").color(t().muted));
        ui.add_space(12.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            for (name, landscape) in [("iPhone", false), ("iPad", true)] {
                let (card, _) = ui.allocate_exact_size(Vec2::new(300.0, 356.0), Sense::hover());
                let painter = ui.painter_at(card);
                painter.rect(card, CornerRadius::same(16), t().panel, Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
                let dev = if landscape { Vec2::new(200.0, 140.0) } else { Vec2::new(92.0, 184.0) };
                let area = Rect::from_min_size(card.min + Vec2::new(0.0, 20.0), Vec2::new(card.width(), 190.0));
                paint_device(&painter, Rect::from_center_size(area.center(), dev), landscape, false);
                let mut child = ui.new_child(
                    egui::UiBuilder::new().max_rect(Rect::from_min_max(card.min + Vec2::new(20.0, 222.0), card.max - Vec2::new(20.0, 16.0))).layout(Layout::top_down(Align::Min)),
                );
                child.label(RichText::new(name).size(18.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                child.add_space(10.0);
                if icon_button(&mut child, Icon::Folder, "Open app (.ipa)", true).clicked() {
                    *open_ios = true;
                }
                child.label(RichText::new("Reads any app. Running classic apps is in progress.").size(12.0).color(t().muted));
            }
        });
    });
    pick
}

/// The device shape and state at the top of a phone or tablet's page.
fn device_preview(ui: &mut Ui, vm: &VmConfig, running: bool) {
    let landscape = vm.kind() == DeviceKind::Tablet;
    egui::Frame::NONE.fill(t().panel).stroke(Stroke::new(1.0_f32, t().border)).corner_radius(CornerRadius::same(14)).inner_margin(Margin::same(18)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let size = if landscape { Vec2::new(210.0, 138.0) } else { Vec2::new(96.0, 176.0) };
            let (r, _) = ui.allocate_exact_size(Vec2::new(size.x.max(120.0), 180.0), Sense::hover());
            paint_device(ui.painter(), Rect::from_center_size(r.center(), size), landscape, running);
            ui.add_space(18.0);
            ui.vertical(|ui| {
                ui.add_space(24.0);
                ui.label(RichText::new(if landscape { "Android tablet" } else { "Android phone" }).size(17.0).color(t().text));
                ui.label(RichText::new(format!("{} Ã— {} screen Â· {} MB RAM Â· {} cores", vm.xres, vm.yres, vm.ram_mb, vm.cpus)).size(12.5).color(t().muted));
                ui.add_space(6.0);
                if running {
                    pill(ui, "Running in its own window", t().ok, t().surface);
                } else {
                    pill(ui, "Off", t().muted, t().surface);
                }
            });
        });
    });
    ui.add_space(10.0);
}

fn ran_frames(s: &Session) -> u32 {
    s.frames_this_tick
}

/// Rows of the bindings table: (field, label, diagram part).
const KEY_FIELDS: &[&str] = &["up", "down", "left", "right", "a", "b", "start", "select", "x", "y", "l", "r", "turbo_a", "turbo_b", "fast_forward", "pause", "reset", "fullscreen", "rewind", "save_state", "load_state", "next_slot"];
const PAD_FIELDS: &[&str] = &["up", "down", "left", "right", "a", "b", "start", "select", "x", "y", "l", "r", "turbo_a", "turbo_b"];

/// Sets `field` to `value`; if another field already had `value`, it takes `field`'s old value.
fn assign_swapping<T>(target: &mut T, field: &str, value: String, fields: &[&str], get: for<'a> fn(&'a mut T, &str) -> &'a mut String) {
    let old = get(target, field).clone();
    if old == value {
        return;
    }
    if let Some(other) = fields.iter().find(|f| **f != field && *get(target, f) == value) {
        *get(target, other) = old;
    }
    *get(target, field) = value;
}

fn pad_binding_mut<'a>(p: &'a mut crate::settings::PadBindings, field: &str) -> &'a mut String {
    match field {
        "up" => &mut p.up,
        "down" => &mut p.down,
        "left" => &mut p.left,
        "right" => &mut p.right,
        "a" => &mut p.a,
        "b" => &mut p.b,
        "start" => &mut p.start,
        "select" => &mut p.select,
        "x" => &mut p.x,
        "y" => &mut p.y,
        "l" => &mut p.l,
        "r" => &mut p.r,
        "turbo_a" => &mut p.turbo_a,
        _ => &mut p.turbo_b,
    }
}

fn binding(k: &KeyBindings, field: &str) -> String {
    let mut copy = k.clone();
    if let Some(f) = field.strip_prefix("pad:") {
        return pad_binding_mut(&mut copy.pad, f).clone();
    }
    binding_mut(&mut copy, field).clone()
}

/// Friendly key names for display.
fn pretty_key(name: &str) -> String {
    match name {
        "" => "Unbound".into(),
        "ArrowUp" => "â†‘  Up".into(),
        "ArrowDown" => "â†“  Down".into(),
        "ArrowLeft" => "â†  Left".into(),
        "ArrowRight" => "â†’  Right".into(),
        other => other.to_string(),
    }
}

/// Keycap-style chip showing a binding; highlighted while waiting for input.
fn key_chip(ui: &mut Ui, rect: Rect, id: &str, value: &str, waiting: bool) -> Response {
    let resp = ui.interact(rect, ui.id().with(("chip", id)), Sense::click());
    let p = ui.painter();
    let hovered = resp.hovered();
    let (bg, fg, stroke) = if waiting {
        (t().primary, t().on_primary, Stroke::new(1.0_f32, t().primary))
    } else if hovered {
        (t().surface_hover, t().text, Stroke::new(1.0_f32, t().primary))
    } else {
        (t().bg, if value.is_empty() { t().muted } else { t().text }, Stroke::new(1.0_f32, t().border))
    };
    p.rect_filled(rect.translate(Vec2::new(0.0, 2.0)), CornerRadius::same(7), Color32::from_black_alpha(110));
    p.rect(rect, CornerRadius::same(7), bg, stroke, egui::StrokeKind::Inside);
    let text = if waiting { "Waitingâ€¦".to_string() } else if id.starts_with("pad:") { controls::glyph(controls::current(), value) } else { pretty_key(value) };
    p.text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(12.5), fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn segment(ui: &mut Ui, selected: bool, text: &str) -> Response {
    let font = FontId::proportional(13.5);
    let w = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE).size().x + 28.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 32.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(8), t().primary);
    } else {
        p.rect(rect, CornerRadius::same(8), if resp.hovered() { t().surface_hover } else { t().surface }, Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
    }
    p.text(rect.center(), Align2::CENTER_CENTER, text, font, if selected { t().on_primary } else { t().text });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A card listing bindings: label | keyboard chip | controller chip.
fn binding_card(
    ui: &mut Ui,
    heading: &str,
    rows: &[(&'static str, &'static str, &'static str)],
    keys: &KeyBindings,
    waiting: Option<&'static str>,
    rebind: &mut Option<&'static str>,
    hovered_part: &mut Option<&'static str>,
) {
    egui::Frame::NONE.fill(t().surface).stroke(Stroke::new(1.0_f32, t().border)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let chip_w = ((ui.available_width() - 92.0) / 2.0).max(90.0);
        ui.horizontal(|ui| {
            ui.add_sized([80.0, 18.0], egui::Label::new(RichText::new(heading).size(14.5).family(egui::FontFamily::Name("heading".into())).color(t().text)));
            ui.add_sized([chip_w, 18.0], egui::Label::new(RichText::new("Keyboard").size(12.0).color(t().muted)));
            ui.add_sized([chip_w, 18.0], egui::Label::new(RichText::new("Controller").size(12.0).color(t().muted)));
        });
        ui.add_space(4.0);
        for &(field, label, part) in rows {
            let row = ui.horizontal(|ui| {
                ui.add_sized([80.0, 30.0], egui::Label::new(RichText::new(label).size(13.5).color(t().text)));
                let (kr, _) = ui.allocate_exact_size(Vec2::new(chip_w, 28.0), Sense::hover());
                if key_chip(ui, kr, field, &binding(keys, field), waiting == Some(field)).clicked() {
                    *rebind = Some(field);
                }
                let pad_field: &'static str = PAD_IDS.iter().find(|(k, _)| *k == field).map(|(_, v)| *v).unwrap_or("pad:a");
                let (pr, _) = ui.allocate_exact_size(Vec2::new(chip_w, 28.0), Sense::hover());
                if key_chip(ui, pr, pad_field, &binding(keys, pad_field), waiting == Some(pad_field)).clicked() {
                    *rebind = Some(pad_field);
                }
            });
            if row.response.contains_pointer() || ui.rect_contains_pointer(row.response.rect) {
                *hovered_part = Some(part);
            }
        }
    });
}

const PAD_IDS: &[(&str, &str)] = &[
    ("up", "pad:up"),
    ("down", "pad:down"),
    ("left", "pad:left"),
    ("right", "pad:right"),
    ("a", "pad:a"),
    ("b", "pad:b"),
    ("start", "pad:start"),
    ("select", "pad:select"),
    ("x", "pad:x"),
    ("y", "pad:y"),
    ("l", "pad:l"),
    ("r", "pad:r"),
    ("turbo_a", "pad:turbo_a"),
    ("turbo_b", "pad:turbo_b"),
];

fn binding_mut<'a>(k: &'a mut KeyBindings, field: &str) -> &'a mut String {
    match field {
        "up" => &mut k.up,
        "down" => &mut k.down,
        "left" => &mut k.left,
        "right" => &mut k.right,
        "a" => &mut k.a,
        "b" => &mut k.b,
        "start" => &mut k.start,
        "select" => &mut k.select,
        "fast_forward" => &mut k.fast_forward,
        "pause" => &mut k.pause,
        "reset" => &mut k.reset,
        "rewind" => &mut k.rewind,
        "save_state" => &mut k.save_state,
        "load_state" => &mut k.load_state,
        "next_slot" => &mut k.next_slot,
        "x" => &mut k.x,
        "y" => &mut k.y,
        "l" => &mut k.l,
        "r" => &mut k.r,
        "turbo_a" => &mut k.turbo_a,
        "turbo_b" => &mut k.turbo_b,
        _ => &mut k.fullscreen,
    }
}

// ================================================================ widgets

fn sidebar(ctx: &egui::Context, id: &str, body: impl FnOnce(&mut Ui)) {
    egui::SidePanel::left(id.to_owned())
        .resizable(false)
        .exact_width(SIDEBAR_W)
        .frame(egui::Frame::NONE.fill(t().panel).inner_margin(Margin { left: 14, right: 14, top: 18, bottom: 14 }))
        .show_separator_line(false)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, body);
        });
}

fn content(ctx: &egui::Context, body: impl FnOnce(&mut Ui)) {
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(t().bg).inner_margin(Margin { left: 32, right: 32, top: 24, bottom: 16 }))
        .show(ctx, body);
}

fn section(ui: &mut Ui, title: &str, subtitle: &str, body: impl FnOnce(&mut Ui)) {
    egui::Frame::NONE
        .fill(t().surface)
        .stroke(Stroke::new(1.0_f32, t().border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(16.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
            if !subtitle.is_empty() {
                ui.label(RichText::new(subtitle).size(12.0).color(t().muted));
            }
            ui.add_space(10.0);
            body(ui);
        });
    ui.add_space(14.0);
}

fn empty_state(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.add_space(48.0);
    ui.vertical_centered(|ui| {
        egui::Frame::NONE
            .fill(t().panel)
            .stroke(Stroke::new(1.0_f32, t().border))
            .corner_radius(CornerRadius::same(16))
            .inner_margin(Margin::symmetric(36, 28))
            .show(ui, |ui| {
                ui.set_max_width(480.0);
                ui.vertical_centered(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 24.0, t().primary.gamma_multiply(0.18));
                    ui.painter().circle_stroke(r.center(), 24.0, Stroke::new(1.2_f32, t().primary.gamma_multiply(0.55)));
                    icons::paint(ui.painter(), r.shrink(12.0), Icon::Gamepad, t().primary);
                    ui.add_space(12.0);
                    ui.label(RichText::new(title).size(20.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
                    ui.add_space(6.0);
                    ui.label(RichText::new(subtitle).size(13.5).color(t().muted));
                });
            });
    });
}

fn primary(b: egui::Button<'_>) -> egui::Button<'_> {
    b.fill(t().primary).stroke(Stroke::NONE).corner_radius(CornerRadius::same(8))
}

fn primary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(primary(egui::Button::new(RichText::new(text).strong().color(t().on_primary))).min_size(Vec2::new(96.0, 32.0)))
}

fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).color(t().text))
            .fill(t().surface)
            .stroke(Stroke::new(1.0_f32, t().border))
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(0.0, 32.0)),
    )
}

/// Compact rounded badge; sized to its text regardless of the parent layout.
fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) {
    let font = FontId::proportional(11.5);
    let w = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), fg).size().x;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w + 22.0, 22.0), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(11), bg, Stroke::new(1.0_f32, fg.gamma_multiply(0.45)), egui::StrokeKind::Inside);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, font, fg);
}

/// Brand wordmark: gradient badge + name.
fn wordmark(ui: &mut Ui) {
    let (badge, _) = ui.allocate_exact_size(Vec2::new(34.0, 34.0), Sense::hover());
    // SVG mark rasterised once at 2x for crisp edges on high-DPI screens.
    let id = egui::Id::new("brand-mark-tex");
    let tex = ui.ctx().data_mut(|d| d.get_temp::<TextureHandle>(id)).or_else(|| {
        let t2 = crate::logo::texture(ui.ctx(), t(), 96)?;
        ui.ctx().data_mut(|d| d.insert_temp(id, t2.clone()));
        Some(t2)
    });
    if let Some(tex) = tex {
        egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), badge.size())).paint_at(ui, badge);
    }


    ui.add_space(4.0);
    ui.label(RichText::new(t().name).size(19.0).family(egui::FontFamily::Name("heading".into())).color(t().text));
    ui.label(RichText::new(t().suffix).size(19.0).color(t().muted));
}

fn nav_tab(ui: &mut Ui, selected: bool, icon: Icon, label: &str) -> Response {
    let tw = text_width(ui, label);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(tw + 26.0 + 24.0, 40.0), Sense::click());
    let hovered = resp.hovered();
    let p = ui.painter();
    if selected {
        p.rect(
            rect,
            CornerRadius::same(10),
            t().primary.gamma_multiply(0.18),
            Stroke::new(1.0_f32, t().primary.gamma_multiply(0.55)),
            egui::StrokeKind::Inside,
        );
    } else if hovered {
        p.rect_filled(rect, CornerRadius::same(10), t().surface);
    }
    let color = if selected || hovered { t().text } else { t().muted };
    let icon_color = if selected { t().primary } else { color };
    let left = rect.center().x - (tw + 22.0) / 2.0;
    icons::paint(p, Rect::from_center_size(egui::pos2(left + 8.0, rect.center().y), Vec2::splat(16.0)), icon, icon_color);
    p.text(egui::pos2(left + 22.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.5), color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn text_width(ui: &Ui, s: &str) -> f32 {
    ui.painter().layout_no_wrap(s.to_owned(), FontId::proportional(15.0), Color32::WHITE).size().x
}

/// Sidebar row: icon, label and an optional status dot on the right.
fn side_row(ui: &mut Ui, selected: bool, icon: Icon, label: &str, dot: Option<Color32>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect(
            rect,
            CornerRadius::same(9),
            t().primary.gamma_multiply(0.16),
            Stroke::new(1.0_f32, t().primary.gamma_multiply(0.45)),
            egui::StrokeKind::Inside,
        );
        let bar = Rect::from_min_size(rect.left_top() + Vec2::new(2.0, 8.0), Vec2::new(3.0, rect.height() - 16.0));
        p.rect_filled(bar, CornerRadius::same(2), t().primary);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(9), t().surface);
    }
    let fg = if selected || resp.hovered() { t().text } else { t().muted };
    icons::paint(p, Rect::from_center_size(egui::pos2(rect.left() + 22.0, rect.center().y), Vec2::splat(16.0)), icon, if selected { t().primary } else { fg });
    p.text(egui::pos2(rect.left() + 40.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.0), fg);
    if let Some(c) = dot {
        p.circle_filled(egui::pos2(rect.right() - 14.0, rect.center().y), 3.5, c);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Button with a leading icon. `primary` fills it with the brand colour.
fn icon_button(ui: &mut Ui, icon: Icon, text: &str, primary: bool) -> Response {
    let font = FontId::proportional(14.0);
    let tw = ui.painter().layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE).size().x;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(tw + 16.0 + 34.0, 34.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let enabled = ui.is_enabled();
    let hovered = resp.hovered() && enabled;
    let p = ui.painter();
    let (bg, fg, stroke) = if primary {
        (if hovered { lerp(t().primary, Color32::WHITE, 0.12) } else { t().primary }, t().on_primary, Stroke::NONE)
    } else {
        (if hovered { t().surface_hover } else { t().surface }, t().text, Stroke::new(1.0_f32, if hovered { t().primary } else { t().border }))
    };
    let (bg, fg) = if enabled { (bg, fg) } else { (bg.gamma_multiply(0.5), fg.gamma_multiply(0.45)) };
    p.rect(rect, CornerRadius::same(8), bg, stroke, egui::StrokeKind::Inside);
    icons::paint(p, Rect::from_center_size(egui::pos2(rect.left() + 20.0, rect.center().y), Vec2::splat(15.0)), icon, fg);
    p.text(egui::pos2(rect.left() + 34.0, rect.center().y), Align2::LEFT_CENTER, text, font, fg);
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

fn search_box(ui: &mut Ui, value: &mut String) {
    egui::Frame::NONE
        .fill(t().panel)
        .stroke(Stroke::new(1.0_f32, t().border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin { left: 10, right: 8, top: 4, bottom: 4 })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                icons::paint(ui.painter(), r, Icon::Search, t().muted);
                ui.add(egui::TextEdit::singleline(value).hint_text("Search games").frame(false).desired_width(240.0));
            });
        });
}

/// Window title while playing, e.g. "Super Mario Bros. 2 (NES) - HitBoy's Emulations".
fn window_title(path: &Path, title: &str) -> String {
    let name = library::clean_title(title);
    match cores::console_for_path(path) {
        Some(c) => format!("{name} ({}) - {}", library::console(c).short, t().full_name()),
        None => format!("{name} - {}", t().full_name()),
    }
}

/// Speaker toggle plus a compact volume slider for the player toolbar.
fn volume_control(ui: &mut Ui, volume: &mut f32, muted: &mut bool) {
    let icon = if *muted || *volume <= 0.001 { Icon::Muted } else { Icon::Volume };
    let (r, resp) = ui.allocate_exact_size(Vec2::new(30.0, 30.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, CornerRadius::same(6), t().surface);
    }
    icons::paint(ui.painter(), r.shrink(7.0), icon, if *muted { t().danger } else { t().text });
    if resp.on_hover_text(if *muted { "Unmute" } else { "Mute" }).clicked() {
        *muted = !*muted;
    }
    ui.spacing_mut().slider_width = 90.0;
    ui.add_enabled(!*muted, egui::Slider::new(volume, 0.0..=1.0).show_value(false)).on_hover_text(format!("Volume {:.0}%", *volume * 100.0));
}


/// Draws a texture filling `rect` (cropping to keep aspect) or fitted inside it for wide logos.
/// A small picture filling its box exactly (the box already has the picture's shape).
fn paint_thumb(ui: &Ui, rect: Rect, tex: &TextureHandle, radius: u8) {
    ui.painter().rect_filled(rect, CornerRadius::same(radius), Color32::from_rgb(0x0E, 0x0F, 0x15));
    egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), rect.size())).corner_radius(CornerRadius::same(radius)).paint_at(ui, rect);
}

fn paint_art(ui: &Ui, rect: Rect, tex: &TextureHandle, radius: u8) {
    let [w, h] = tex.size();
    let ia = w as f32 / h as f32;
    let ra = rect.width() / rect.height();
    if ia > 0.9 {
        // Logos, title screens and captured frames: fit inside, on a dark plate (box art is portrait).
        ui.painter().rect_filled(rect, CornerRadius::same(radius), Color32::from_rgb(0x0E, 0x0F, 0x15));
        let fit = rect.shrink(12.0);
        let size = if fit.width() / fit.height() < ia { Vec2::new(fit.width(), fit.width() / ia) } else { Vec2::new(fit.height() * ia, fit.height()) };
        egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), size)).paint_at(ui, Rect::from_center_size(rect.center(), size));
        return;
    }
    // Box art: cover-crop.
    let uv = if ia > ra {
        let f = ra / ia;
        Rect::from_min_max(egui::pos2((1.0 - f) / 2.0, 0.0), egui::pos2((1.0 + f) / 2.0, 1.0))
    } else {
        let f = ia / ra;
        Rect::from_min_max(egui::pos2(0.0, (1.0 - f) / 2.0), egui::pos2(1.0, (1.0 + f) / 2.0))
    };
    egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), rect.size())).uv(uv).corner_radius(CornerRadius::same(radius)).paint_at(ui, rect);
}

/// Game tile: official box art when found, otherwise a quiet placeholder with the title.
fn game_tile(ui: &mut Ui, g: &GameEntry, console: &library::Console, playable: bool, art: Option<&TextureHandle>, loading: bool) -> (Response, bool) {
    const W: f32 = 168.0;
    const COVER_H: f32 = 224.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(W, COVER_H + 54.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return (resp, false);
    }
    let hovered = resp.hovered();
    let cover = Rect::from_min_size(rect.min, Vec2::new(W, COVER_H));
    // Remove/delete button in the cover's top-right corner, shown on hover.
    let trash_rect = Rect::from_min_size(egui::pos2(cover.right() - 36.0, cover.top() + 8.0), Vec2::splat(28.0));
    let over_trash = hovered && ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| trash_rect.contains(p));
    let trash_clicked = over_trash && resp.clicked();
    let name = library::clean_title(&g.title);
    match art {
        Some(tex) => paint_art(ui, cover, tex, 10),
        None => {
            let p = ui.painter();
            p.rect(cover, CornerRadius::same(10), t().surface, Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
            let icon = if console.handheld { Icon::Handheld } else { Icon::Console };
            icons::paint(p, Rect::from_center_size(cover.center() - Vec2::new(0.0, 26.0), Vec2::splat(40.0)), icon, t().border);
            let galley = p.layout(name.clone(), FontId::proportional(15.0), t().muted, W - 28.0);
            p.galley(egui::pos2(cover.left() + 14.0, cover.center().y + 6.0), galley, t().muted);
            if loading {
                p.text(cover.left_bottom() + Vec2::new(14.0, -14.0), Align2::LEFT_BOTTOM, "Finding artworkâ€¦", FontId::proportional(11.0), t().muted);
            }
        }
    }
    let p = ui.painter();
    // Console badge pill with subtle brand accent border.
    let badge_font = FontId::proportional(11.0);
    let bw = p.layout_no_wrap(console.short.to_owned(), badge_font.clone(), Color32::WHITE).size().x + 14.0;
    let badge = Rect::from_min_size(cover.min + Vec2::new(8.0, 8.0), Vec2::new(bw, 20.0));
    p.rect(badge, CornerRadius::same(6), Color32::from_black_alpha(195), Stroke::new(1.0_f32, t().primary.gamma_multiply(0.55)), egui::StrokeKind::Inside);
    p.text(badge.center(), Align2::CENTER_CENTER, console.short, badge_font, Color32::WHITE);
    if hovered {
        p.rect_filled(cover, CornerRadius::same(12), Color32::from_black_alpha(if playable { 115 } else { 165 }));
        if playable {
            let c = cover.center();
            p.circle_filled(c, 31.0, t().secondary.gamma_multiply(0.32));
            p.circle_filled(c, 26.0, t().primary);
            icons::paint(p, Rect::from_center_size(c + Vec2::new(2.0, 0.0), Vec2::splat(22.0)), Icon::Play, t().on_primary);
        } else {
            p.text(cover.center(), Align2::CENTER_CENTER, "Coming soon", FontId::proportional(12.5), Color32::WHITE);
        }
        p.rect_stroke(cover.expand(2.0), CornerRadius::same(13), Stroke::new(3.0_f32, t().secondary.gamma_multiply(0.35)), egui::StrokeKind::Outside);
        p.rect_stroke(cover, CornerRadius::same(12), Stroke::new(2.0_f32, t().primary), egui::StrokeKind::Outside);
        p.rect_filled(trash_rect, CornerRadius::same(7), if over_trash { t().danger } else { Color32::from_black_alpha(195) });
        icons::paint(p, trash_rect.shrink(7.0), Icon::Trash, Color32::WHITE);
    }
    // Name (ellipsized) and file size.
    let title_galley = p.layout(name.clone(), FontId::proportional(14.0), t().text, W);
    let first_line = title_galley.rows.first().map(|r| r.text()).unwrap_or_default();
    let shown = if title_galley.rows.len() > 1 { format!("{}â€¦", first_line.trim_end()) } else { first_line };
    p.text(egui::pos2(rect.left() + 1.0, cover.bottom() + 10.0), Align2::LEFT_TOP, shown, FontId::proportional(14.0), t().text);
    let meta = library::human_size(g.size);
    p.text(egui::pos2(rect.left() + 1.0, cover.bottom() + 31.0), Align2::LEFT_TOP, meta, FontId::proportional(11.5), t().muted);
    let resp = if over_trash {
        resp.on_hover_text("Remove or delete")
    } else if playable {
        resp.on_hover_text(&name).on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    };
    (resp, trash_clicked)
}


/// Smooth Royal Blue <-> Royal Purple brand gradient ribbon under top bars & hero cards.
fn gradient_rule(ui: &mut Ui, rect: Rect) {
    let steps = 24usize;
    let sw = rect.width() / steps as f32;
    let p = ui.painter();
    for i in 0..steps {
        let f = i as f32 / (steps - 1) as f32;
        let col = lerp(t().primary, t().secondary, f);
        let seg = Rect::from_min_max(
            egui::pos2(rect.left() + i as f32 * sw, rect.top()),
            egui::pos2(rect.left() + (i + 1) as f32 * sw + 0.5, rect.bottom()),
        );
        p.rect_filled(seg, 0.0, col);
    }
}

fn lerp(a: Color32, b: Color32, x: f32) -> Color32 {
    let m = |p: u8, q: u8| (p as f32 + (q as f32 - p as f32) * x).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

fn path_row(ui: &mut Ui, label: &str, value: &mut String, filters: &[(&str, &[&str])]) {
    ui.horizontal(|ui| {
        ui.add_sized([90.0, 28.0], egui::Label::new(label));
        ui.add(egui::TextEdit::singleline(value).hint_text("None attached").desired_width((ui.available_width() - 190.0).max(120.0)).margin(Margin::symmetric(8, 6)));
        if ghost_button(ui, "Browse").clicked() {
            let mut d = rfd::FileDialog::new();
            for (name, exts) in filters {
                d = d.add_filter(*name, exts);
            }
            if let Some(p) = d.pick_file() {
                *value = p.to_string_lossy().into_owned();
            }
        }
        if !value.is_empty() && ghost_button(ui, "Detach").clicked() {
            value.clear();
        }
    });
}

/// Windows' native type family: Segoe UI for text, Semibold for headings, Cascadia Mono for code.
/// Missing files fall back to egui's bundled fonts.
fn install_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions, FontFamily};
    let mut fonts = FontDefinitions::default();
    // First font file found wins; egui's built-in font is the fallback everywhere.
    let mut load = |name: &str, files: &[&str]| -> bool {
        for file in files {
            if let Ok(bytes) = std::fs::read(file) {
                fonts.font_data.insert(name.into(), std::sync::Arc::new(FontData::from_owned(bytes)));
                return true;
            }
        }
        false
    };
    let ui_font = load(
        "ui",
        &[
            r"C:\Windows\Fonts\segoeui.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/Library/Fonts/Arial.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ],
    );
    let semibold = load(
        "ui-semibold",
        &[
            r"C:\Windows\Fonts\seguisb.ttf",
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-SemiBold.ttf",
            "/usr/share/fonts/noto/NotoSans-SemiBold.ttf",
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-M.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        ],
    );
    let mono = load(
        "mono",
        &[
            r"C:\Windows\Fonts\CascadiaMono.ttf",
            "/System/Library/Fonts/Supplemental/Courier New.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        ],
    );
    let symbols = load("symbols", &[r"C:\Windows\Fonts\seguisym.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"]);
    let proportional = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if ui_font {
        let fam = fonts.families.entry(FontFamily::Proportional).or_default();
        fam.insert(0, "ui".into());
        if symbols {
            fam.insert(1, "symbols".into());
        }
    }
    if mono {
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "mono".into());
    }
    let mut heading: Vec<String> = Vec::new();
    if semibold {
        heading.push("ui-semibold".into());
    }
    if symbols {
        heading.push("symbols".into());
    }
    heading.extend(proportional);
    fonts.families.insert(FontFamily::Name("heading".into()), heading);
    ctx.set_fonts(fonts);
}

fn apply_style(ctx: &egui::Context) {
    install_fonts(ctx);
    let th = t();
    let mut v = egui::Visuals::dark();
    v.panel_fill = th.bg;
    v.window_fill = th.panel;
    v.extreme_bg_color = th.panel;
    v.faint_bg_color = th.surface;
    v.override_text_color = Some(th.text);
    v.hyperlink_color = th.primary;
    v.selection.bg_fill = th.primary.gamma_multiply(0.55);
    v.selection.stroke = Stroke::new(1.0_f32, th.primary);
    v.window_corner_radius = CornerRadius::same(12);
    v.window_stroke = Stroke::new(1.0_f32, th.border);
    let r = CornerRadius::same(8);
    for (w, fill) in [
        (&mut v.widgets.noninteractive, th.surface),
        (&mut v.widgets.inactive, th.surface),
        (&mut v.widgets.hovered, th.surface_hover),
        (&mut v.widgets.active, th.surface_hover),
        (&mut v.widgets.open, th.surface_hover),
    ] {
        w.corner_radius = r;
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
    }
    // Rails (sliders) and checkbox wells need contrast against card surfaces.
    v.widgets.inactive.bg_fill = th.border;
    v.slider_trailing_fill = true;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, th.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, th.text);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, th.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, th.text);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, th.primary);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, th.text);
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, th.primary);
    v.widgets.active.fg_stroke = Stroke::new(1.5_f32, th.text);
    // Scroll bar handles use these fills: muted at rest, brand colour when hovered/dragged.
    v.widgets.hovered.bg_fill = lerp(th.surface_hover, th.primary, 0.55);
    v.widgets.active.bg_fill = th.primary;
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        // Slim scroll bars in the brand colours (not the default white).
        s.spacing.scroll = egui::style::ScrollStyle { bar_width: 8.0, floating: true, foreground_color: false, dormant_background_opacity: 0.0, active_background_opacity: 0.4, interact_background_opacity: 0.6, dormant_handle_opacity: 0.35, active_handle_opacity: 0.8, interact_handle_opacity: 1.0, ..egui::style::ScrollStyle::floating() };
        s.spacing.item_spacing = Vec2::new(10.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 6.0);
        s.spacing.interact_size.y = 28.0;
        s.spacing.slider_width = 220.0;
    });
}

impl eframe::App for App {
    /// Closing the window mid-game must not lose the game's save.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(s) = self.session.as_mut() {
            let _ = s.flush_save();
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.session.is_some() {
            self.run_emulation(ctx);
            self.player_ui(ctx);
        } else if self.vm_session.is_some() {
            self.vm_player_ui(ctx);
        } else {
            // Keep gilrs' event queue drained so controller hot-plug is noticed.
            if let Some(g) = self.gilrs.as_mut() {
                while let Some(ev) = g.next_event() {
                    if let gilrs::EventType::ButtonPressed(b, _) = ev.event {
                        if self.tab == Tab::Console && self.shell_pad.len() < 32 {
                            self.shell_pad.push(b);
                        }
                    }
                }
            }
            self.header_ui(ctx);
            self.status_bar(ctx);
            match self.tab {
                Tab::Library => self.library_ui(ctx),
                Tab::Console => self.shell_ui(ctx),
                Tab::Discover => self.discover_ui(ctx),
                Tab::RemotePlay => self.remote_play_ui(ctx),
                Tab::Machines => self.machines_ui(ctx, false),
                Tab::Devices => self.machines_ui(ctx, true),
                Tab::Settings => self.settings_ui(ctx),
            }
            if self.remote_play.scanning {
                self.remote_play.poll();
                ctx.request_repaint_after(Duration::from_millis(120));
            }
            // Poll for controller hot-plug and VM exits even when idle.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        self.controls_modal(ctx);
        self.delete_modal(ctx);
        self.convert_modal(ctx);
        self.ios_modal(ctx);
        if let Some(j) = self.convert_job.as_mut() {
            ctx.request_repaint_after(Duration::from_millis(200));
            if let Some(r) = j.poll() {
                self.convert_job = None;
                match r {
                    Ok(out) => {
                        self.notify(format!("Converted: {}", out.file_name().unwrap_or_default().to_string_lossy()));
                        crate::platform::reveal(&out);
                    }
                    Err(e) => self.notify(format!("Conversion failed: {e}")),
                }
            }
        }
        if let Some(rx) = &self.bios_search {
            ctx.request_repaint_after(Duration::from_millis(200));
            if let Ok(found) = rx.try_recv() {
                self.bios_found = Some(found);
                self.bios_search = None;
            }
        }
        self.poll_copies();
        if self.copying > 0 {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if self.settings != self.saved_settings {
            self.settings.save();
            self.saved_settings = self.settings.clone();
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use std::collections::HashMap;

    /// A Master System cartridge that counts frames in RAM, saved to a temp file.
    fn sms_game(dir: &Path) -> PathBuf {
        let mut rom = vec![0u8; 0x8000];
        let prog: &[u8] = &[
            0xF3, 0x31, 0xF0, 0xDF, 0xED, 0x56, 0x3E, 0xE0, 0xD3, 0xBF, 0x3E, 0x81, 0xD3, 0xBF, 0x3E, 0x00, 0xD3, 0xBF, 0x3E, 0x87, 0xD3, 0xBF, 0x3E, 0x00, 0xD3, 0xBF, 0x3E, 0xC0, 0xD3, 0xBF, 0x3E, 0x0C, 0xD3, 0xBE, 0xFB, 0x76,
            0x18, 0xFD,
        ];
        rom[..prog.len()].copy_from_slice(prog);
        rom[0x38..0x38 + 10].copy_from_slice(&[0x21, 0x00, 0xC0, 0x34, 0xDB, 0xBF, 0xFB, 0xED, 0x4D, 0x00]);
        let p = dir.join("Counter (USA).sms");
        std::fs::write(&p, rom).unwrap();
        p
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("session_test_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn open(dir: &Path) -> Session {
        let path = sms_game(dir);
        let loaded = cores::load(&path).unwrap();
        let mut s = Session::new(loaded.core, path, loaded.save_path, loaded.title, Instant::now());
        s.state_dir = dir.join("states");
        s
    }

    fn frame(s: &mut Session) {
        s.core.run_frame();
        s.frames_total += 1;
        s.record_frame();
    }

    #[test]
    fn rewind_steps_back_through_recorded_moments() {
        let dir = temp_dir("rewind");
        let mut s = open(&dir);
        assert!(s.core.has_state());
        let mut seen: HashMap<u64, Vec<u8>> = HashMap::new();
        for _ in 0..120 {
            frame(&mut s);
            if s.frames_total % SNAP_EVERY == 0 {
                seen.insert(s.frames_total, s.core.save_state().unwrap());
            }
        }
        // The newest snapshot is the current moment; each step after goes back 4 frames.
        for k in 0..20u64 {
            assert!(s.rewind_step(), "step {k}");
            let want = &seen[&(120 - SNAP_EVERY * k)];
            assert_eq!(&s.core.save_state().unwrap(), want, "after {k} steps back the machine is at frame {}", 120 - SNAP_EVERY * k);
        }
        // Forward again from the rewound moment: the game just carries on.
        for _ in 0..8 {
            frame(&mut s);
        }
        assert!(s.core.save_state().is_some());
    }

    #[test]
    fn rewind_history_is_capped() {
        let dir = temp_dir("cap");
        let mut s = open(&dir);
        s.rewind.max_snapshots = 10;
        for _ in 0..400 {
            frame(&mut s);
        }
        assert_eq!(s.rewind.len(), 10);
        let mut steps = 0;
        while s.rewind.len() > 1 && s.rewind_step() {
            steps += 1;
        }
        assert_eq!(steps, 9);
    }

    #[test]
    fn save_slots_round_trip_and_reject_damage() {
        let dir = temp_dir("slots");
        let mut s = open(&dir);
        for _ in 0..10 {
            frame(&mut s);
        }
        let at_save = s.core.save_state().unwrap();
        assert!(s.slot_age(3).is_none());
        assert_eq!(s.load_slot(3).unwrap_err(), "Slot 3 is empty.");
        s.save_slot(3).unwrap();
        assert_eq!(s.slot_age(3).as_deref(), Some("just now"));
        for _ in 0..30 {
            frame(&mut s);
        }
        assert_ne!(s.core.save_state().unwrap(), at_save);
        s.load_slot(3).unwrap();
        assert_eq!(s.core.save_state().unwrap(), at_save, "the machine is back where it was saved");
        assert!(s.rewind.is_empty(), "history from before the load no longer applies");

        // A damaged file is refused and leaves the game untouched.
        let file = s.state_file(3);
        let mut bytes = std::fs::read(&file).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&file, bytes).unwrap();
        frame(&mut s);
        let before = s.core.save_state().unwrap();
        assert_eq!(s.load_slot(3).unwrap_err(), "Slot 3 is damaged.");
        assert_eq!(s.core.save_state().unwrap(), before);
        // A different game's state (wrong size) is refused too.
        let other = s.state_file(4);
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        let junk = vec![7u8; 100];
        let mut file = b"NSF1".to_vec();
        file.extend((fnv(&junk) as u32).to_le_bytes());
        file.extend(&junk);
        std::fs::write(other, file).unwrap();
        assert!(s.load_slot(4).is_err());
        assert_eq!(s.core.save_state().unwrap(), before);
    }

    #[test]
    fn game_saves_are_written_safely_and_only_when_changed() {
        let dir = temp_dir("sav");
        // An NES cartridge with battery RAM.
        let mut image = b"NES\x1A\x01\x01\x12\x00".to_vec();
        image.resize(16, 0);
        let mut prg = vec![0u8; 0x4000];
        prg[0x3FFA..].copy_from_slice(&[0x00, 0xC1, 0x00, 0xC0, 0x00, 0xC0]);
        image.extend(prg);
        image.extend(vec![0u8; 0x2000]);
        let path = dir.join("Zelda-like.nes");
        std::fs::write(&path, image).unwrap();
        let loaded = cores::load(&path).unwrap();
        let mut s = Session::new(loaded.core, path.clone(), loaded.save_path, loaded.title, Instant::now());
        s.flush_save().unwrap();
        let sav = path.with_extension("sav");
        assert_eq!(std::fs::metadata(&sav).unwrap().len(), 8192);
        assert!(!path.with_extension("sav.tmp").exists(), "no temp file left behind");
        // Unchanged: nothing is rewritten.
        std::fs::remove_file(&sav).unwrap();
        s.flush_save().unwrap();
        assert!(!sav.exists(), "an unchanged save isn't written again");
        // The next session finds the save and gives it to the game.
        let mut ram = vec![0u8; 8192];
        ram[0] = 0xC3;
        std::fs::write(&sav, &ram).unwrap();
        let again = cores::load(&path).unwrap();
        assert_eq!(again.core.save_ram().unwrap()[0], 0xC3);
    }
}
