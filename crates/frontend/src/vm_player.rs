//! In-app screen for VMs running on our built-in x86 PC core and Android Device / APK Runtime.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use android::{AndroidDevice, NavKey};
use eframe::egui::{self, Align2, Color32, ColorImage, CornerRadius, FontId, Key, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2};
use pc::keys;
use pc::{Machine, Screen};

use crate::theme::t;

enum Backend {
    Pc(Machine),
    Android(AndroidDevice),
}

pub struct VmSession {
    pub name: String,
    backend: Backend,
    pub paused: bool,
    last_tick: Instant,
    gfx: Option<TextureHandle>,
    mips: f64,
    mips_acc: u64,
    mips_since: Instant,
    blink: Instant,
    /// Modifier keys last sent to the guest.
    mods: egui::Modifiers,
    /// Mouse buttons last sent to the guest.
    buttons: u8,
    hovered: bool,
}

impl VmSession {
    pub fn start(name: &str, boot: &str, disk: &str, ram_mb: u32) -> Result<Self, String> {
        let cfg = pc::Config {
            boot: (!boot.is_empty()).then(|| PathBuf::from(boot)),
            disk: (!disk.is_empty()).then(|| PathBuf::from(disk)),
            ram_mb,
        };
        let machine = Machine::new(&cfg)?;
        let now = Instant::now();
        Ok(Self {
            name: name.to_owned(),
            backend: Backend::Pc(machine),
            paused: false,
            last_tick: now,
            gfx: None,
            mips: 0.0,
            mips_acc: 0,
            mips_since: now,
            blink: now,
            mods: egui::Modifiers::NONE,
            buttons: 0,
            hovered: false,
        })
    }

    pub fn start_android(name: &str, portrait: bool) -> Self {
        let now = Instant::now();
        Self {
            name: name.to_owned(),
            backend: Backend::Android(AndroidDevice::new(name, portrait)),
            paused: false,
            last_tick: now,
            gfx: None,
            mips: 0.0,
            mips_acc: 0,
            mips_since: now,
            blink: now,
            mods: egui::Modifiers::NONE,
            buttons: 0,
            hovered: false,
        }
    }

    pub fn is_android(&self) -> bool {
        matches!(self.backend, Backend::Android(_))
    }

    pub fn engine_label(&self) -> String {
        if self.is_android() {
            format!("{} Android", t().name)
        } else {
            format!("{} x86", t().name)
        }
    }

    pub fn reboot(&mut self) {
        match &mut self.backend {
            Backend::Pc(m) => m.reboot(),
            Backend::Android(dev) => {
                dev.active_app = None;
                dev.recents_open = false;
                dev.log.push("Rebooted Android device.".into());
            }
        }
    }

    pub fn install_apk(&mut self, path: &Path) -> Result<String, String> {
        if let Backend::Android(dev) = &mut self.backend {
            let idx = dev.install_apk(path)?;
            let label = dev.apps[idx].label.clone();
            dev.launch_app(idx);
            Ok(label)
        } else {
            Err("Install APK is available on Android VMs.".into())
        }
    }

    pub fn toggle_rotation(&mut self) {
        if let Backend::Android(dev) = &mut self.backend {
            dev.toggle_rotation();
        }
    }

    pub fn press_nav(&mut self, key: NavKey) {
        match &mut self.backend {
            Backend::Android(dev) => dev.press_nav(key),
            Backend::Pc(m) => match key {
                NavKey::Back => {
                    m.key_down(keys::ESC, 0x1B);
                    m.key_up(keys::ESC);
                }
                NavKey::Home => {
                    m.key_down(keys::HOME, 0);
                    m.key_up(keys::HOME);
                }
                NavKey::Recents => {
                    m.key_down(keys::TAB, 0x09);
                    m.key_up(keys::TAB);
                }
            },
        }
    }

    pub fn mips(&self) -> f64 {
        self.mips
    }

    /// Forwards keyboard input and runs the guest for this frame.
    pub fn tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f64().min(0.1);
        self.last_tick = now;
        if self.paused {
            return;
        }
        if matches!(self.backend, Backend::Android(_)) {
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.press_nav(NavKey::Back);
            }
            return;
        }
        self.forward_keys(ctx);
        if let Backend::Pc(machine) = &mut self.backend {
            let stats = machine.run_frame(dt, Duration::from_millis(14));
            self.mips_acc += stats.instructions;
            if self.mips_since.elapsed() >= Duration::from_secs(1) {
                self.mips = self.mips_acc as f64 / self.mips_since.elapsed().as_secs_f64() / 1e6;
                self.mips_acc = 0;
                self.mips_since = now;
            }
            ctx.request_repaint();
        }
    }

    fn forward_keys(&mut self, ctx: &egui::Context) {
        let Backend::Pc(machine) = &mut self.backend else { return };
        let mods = ctx.input(|i| i.modifiers);
        for (now, before, scan) in [(mods.shift, self.mods.shift, 0x2Au8), (mods.ctrl, self.mods.ctrl, 0x1D), (mods.alt, self.mods.alt, 0x38)] {
            if now && !before {
                machine.key_down(scan, 0);
            } else if !now && before {
                machine.key_up(scan);
            }
        }
        self.mods = mods;
        if self.hovered {
            let (delta, buttons) = ctx.input(|i| {
                let p = &i.pointer;
                let b = p.button_down(egui::PointerButton::Primary) as u8 | (p.button_down(egui::PointerButton::Secondary) as u8) << 1 | (p.button_down(egui::PointerButton::Middle) as u8) << 2;
                (p.delta(), b)
            });
            if delta != Vec2::ZERO || buttons != self.buttons {
                machine.mouse(delta.x.round() as i32, delta.y.round() as i32, buttons);
                self.buttons = buttons;
            }
        }
        let events = ctx.input(|i| i.events.clone());
        for e in events {
            match e {
                egui::Event::Text(text) => {
                    for c in text.chars() {
                        let ascii = if c.is_ascii() { c as u8 } else { b'?' };
                        let scan = keys::scan_for_char(c).unwrap_or(0);
                        machine.key_down(scan, ascii);
                    }
                }
                egui::Event::Key { key, pressed, modifiers, repeat, .. } => {
                    let Some(scan) = scan_for_key(key) else { continue };
                    if !pressed {
                        machine.key_up(scan);
                        continue;
                    }
                    let _ = repeat;
                    if let Some(ascii) = special_ascii(key) {
                        machine.key_down(scan, ascii);
                    } else if modifiers.ctrl && is_letter(key) {
                        let letter = key.name().as_bytes()[0].to_ascii_uppercase();
                        machine.key_down(scan, letter - b'@');
                    }
                }
                _ => {}
            }
        }
    }

    /// Draws the guest display into `rect`.
    pub fn draw(&mut self, ui: &mut egui::Ui, avail: Rect) -> egui::Response {
        if let Backend::Android(dev) = &mut self.backend {
            return draw_android_device(ui, avail, dev);
        }
        let resp = ui.allocate_rect(avail, Sense::click());
        self.hovered = resp.hovered();
        let painter = ui.painter_at(avail);
        let Backend::Pc(machine) = &mut self.backend else { return resp };
        match machine.screen() {
            Screen::Text { cols, rows, cells, cursor } => {
                let (w, h) = (cols as f32 * 9.0, rows as f32 * 16.0);
                let scale = (avail.width() / w).min(avail.height() / h);
                let (cw, ch) = (9.0 * scale, 16.0 * scale);
                let origin = avail.center() - Vec2::new(w * scale, h * scale) / 2.0;
                let font = FontId::monospace(ch * 0.78);
                for row in 0..rows {
                    let line = &cells[row * cols..(row + 1) * cols];
                    let mut start = 0;
                    while start < cols {
                        let bg = (line[start] >> 12) & 7;
                        let mut end = start + 1;
                        while end < cols && (line[end] >> 12) & 7 == bg {
                            end += 1;
                        }
                        if bg != 0 {
                            let min = origin + Vec2::new(start as f32 * cw, row as f32 * ch);
                            let r = Rect::from_min_size(min, Vec2::new((end - start) as f32 * cw + 0.5, ch + 0.5));
                            painter.rect_filled(r, 0.0, rgb(pc::video::TEXT_PALETTE[bg as usize]));
                        }
                        start = end;
                    }
                }
                for (i, &cell) in cells.iter().enumerate() {
                    let (c, attr) = ((cell & 0xFF) as u8, (cell >> 8) as u8);
                    let (row, col) = (i / cols, i % cols);
                    let min = origin + Vec2::new(col as f32 * cw, row as f32 * ch);
                    let r = Rect::from_min_size(min, Vec2::new(cw + 0.5, ch + 0.5));
                    if c != 0 && c != b' ' && c != 0xFF {
                        let glyph = keys::CP437[c as usize];
                        painter.text(r.center(), Align2::CENTER_CENTER, glyph, font.clone(), rgb(pc::video::TEXT_PALETTE[(attr & 0x0F) as usize]));
                    }
                }
                if let Some((row, col)) = cursor {
                    if (self.blink.elapsed().as_millis() / 400) % 2 == 0 {
                        let min = origin + Vec2::new(col as f32 * cw, row as f32 * ch + ch * 0.82);
                        painter.rect_filled(Rect::from_min_size(min, Vec2::new(cw, ch * 0.12)), 0.0, Color32::from_gray(220));
                    }
                }
            }
            Screen::Graphics { width, height, pixels } => {
                let image = ColorImage {
                    size: [width, height],
                    pixels: pixels.iter().map(|&p| Color32::from_rgb((p >> 16) as u8, (p >> 8) as u8, p as u8)).collect(),
                };
                match &mut self.gfx {
                    Some(tex) => tex.set(image, TextureOptions::NEAREST),
                    None => self.gfx = Some(ui.ctx().load_texture("vm_screen", image, TextureOptions::NEAREST)),
                }
                let (w, h) = if width == 320 && height == 200 {
                    (4.0, 3.0)
                } else {
                    (width.max(1) as f32, height.max(1) as f32)
                };
                let scale = (avail.width() / w).min(avail.height() / h);
                let rect = Rect::from_center_size(avail.center(), Vec2::new(w * scale, h * scale));
                if let Some(tex) = &self.gfx {
                    painter.image(tex.id(), rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                }
            }
        }
        if let Some(err) = machine.error() {
            let card = Rect::from_center_size(avail.center(), Vec2::new(460.0, 90.0));
            painter.rect_filled(card, 12.0, t().panel);
            painter.text(card.center() - Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, "Boot failed", FontId::proportional(20.0), t().danger);
            painter.text(card.center() + Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, err, FontId::proportional(13.0), t().muted);
        }
        resp
    }
}

/// Interactive Android Phone / Tablet screen with launcher, APK runtime, and Android nav bar.
fn draw_android_device(ui: &mut egui::Ui, avail: Rect, dev: &mut AndroidDevice) -> egui::Response {
    let resp = ui.allocate_rect(avail, Sense::hover());
    let (aspect_w, aspect_h) = if dev.portrait { (9.0_f32, 16.0_f32) } else { (16.0_f32, 10.0_f32) };
    let pad = 20.0;
    let max_w = (avail.width() - pad * 2.0).max(260.0);
    let max_h = (avail.height() - pad * 2.0).max(320.0);
    let scale = (max_w / aspect_w).min(max_h / aspect_h);
    let bezel = Rect::from_center_size(avail.center(), Vec2::new(aspect_w * scale, aspect_h * scale));
    let screen = bezel.shrink(10.0);

    let p = ui.painter();
    p.rect_filled(bezel, CornerRadius::same(26), Color32::from_rgb(0x14, 0x16, 0x20));
    p.rect_stroke(bezel, CornerRadius::same(26), Stroke::new(2.0_f32, Color32::from_rgb(0x2E, 0x33, 0x48)), egui::StrokeKind::Inside);
    p.rect_filled(screen, CornerRadius::same(18), Color32::from_rgb(0x0B, 0x0E, 0x18));

    // Inner interactive UI inside the phone/tablet screen
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(screen.shrink(12.0)));
    // Status bar
    child.horizontal(|ui| {
        ui.label(RichText::new("12:00").strong().size(12.0).color(Color32::WHITE));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new("100%  Wi-Fi  5G").size(11.5).color(Color32::from_rgb(0xA6, 0xB4, 0xD0)));
        });
    });
    child.add_space(8.0);

    let body_h = (screen.height() - 104.0).max(120.0);
    let mut launch_idx = None;
    let mut nav_pressed = None;

    egui::ScrollArea::vertical().max_height(body_h).show(&mut child, |ui| {
        if dev.recents_open {
            ui.label(RichText::new("Recent apps").strong().size(16.0).color(Color32::WHITE));
            ui.add_space(8.0);
            if dev.recent_apps.is_empty() {
                ui.label(RichText::new("No recent apps yet.").color(t().muted));
            } else {
                for &idx in &dev.recent_apps {
                    if let Some(app) = dev.apps.get(idx) {
                        if ui.button(RichText::new(format!("{}  ({})", app.label, app.package)).size(14.0)).clicked() {
                            launch_idx = Some(idx);
                        }
                    }
                }
            }
        } else if let Some(idx) = dev.active_app {
            if let Some(app) = dev.apps.get(idx) {
                ui.label(RichText::new(&app.label).strong().size(20.0).color(Color32::WHITE));
                ui.label(RichText::new(format!("{}  v{}", app.package, if app.version_name.is_empty() { "1.0" } else { &app.version_name })).size(12.5).color(t().muted));
                ui.add_space(10.0);
                egui::Frame::NONE
                    .fill(Color32::from_rgb(0x13, 0x18, 0x26))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(0x26, 0x2E, 0x45)))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.label(RichText::new("Android Runtime & Dalvik VM").strong().color(t().primary));
                        ui.add_space(4.0);
                        ui.label(RichText::new(format!("Activity: {}", if app.launcher_activity.is_empty() { "MainActivity" } else { &app.launcher_activity })).size(12.5).color(Color32::WHITE));
                        ui.label(RichText::new(format!("DEX files: {} · Classes: {} · Methods: {}", app.dex_count, app.dex_classes, app.dex_methods)).size(12.0).color(t().muted));
                        let abis = if app.abis.is_empty() { "Universal (Dalvik/ART)".to_string() } else { app.abis.join(", ") };
                        ui.label(RichText::new(format!("Native ABIs: {abis}")).size(12.0).color(t().muted));
                    });
                ui.add_space(10.0);
                if !dev.log.is_empty() {
                    ui.label(RichText::new("Logcat").strong().size(12.0).color(t().muted));
                    for line in dev.log.iter().rev().take(6).rev() {
                        ui.label(RichText::new(line).monospace().size(11.0).color(Color32::from_rgb(0x9E, 0xE6, 0xB8)));
                    }
                }
            }
        } else {
            ui.label(RichText::new("Android 14").strong().size(22.0).color(Color32::WHITE));
            ui.label(RichText::new("Click any app to launch, or click Install APK in the top bar.").size(12.0).color(t().muted));
            ui.add_space(12.0);
            let cols = if dev.portrait { 3 } else { 5 };
            egui::Grid::new("android_apps_grid").spacing(Vec2::new(10.0, 12.0)).show(ui, |ui| {
                for (i, app) in dev.apps.iter().enumerate() {
                    let btn = egui::Button::new(RichText::new(&app.label).strong().size(12.5).color(Color32::WHITE))
                        .fill(if i < 3 { Color32::from_rgb(0x1B, 0x22, 0x36) } else { t().primary })
                        .corner_radius(CornerRadius::same(14))
                        .min_size(Vec2::new(82.0, 64.0));
                    if ui.add(btn).on_hover_text(&app.package).clicked() {
                        launch_idx = Some(i);
                    }
                    if (i + 1) % cols == 0 {
                        ui.end_row();
                    }
                }
            });
        }
    });

    // Bottom Android navigation bar: Back / Home / Recents
    let nav_rect = Rect::from_min_max(egui::pos2(screen.left() + 12.0, screen.bottom() - 42.0), egui::pos2(screen.right() - 12.0, screen.bottom() - 8.0));
    let mut nav_ui = ui.new_child(egui::UiBuilder::new().max_rect(nav_rect));
    nav_ui.columns(3, |cols| {
        if cols[0].centered_and_justified(|ui| ui.button(RichText::new("< Back").color(Color32::WHITE))).inner.clicked() {
            nav_pressed = Some(NavKey::Back);
        }
        if cols[1].centered_and_justified(|ui| ui.button(RichText::new("O Home").color(Color32::WHITE))).inner.clicked() {
            nav_pressed = Some(NavKey::Home);
        }
        if cols[2].centered_and_justified(|ui| ui.button(RichText::new("[] Recents").color(Color32::WHITE))).inner.clicked() {
            nav_pressed = Some(NavKey::Recents);
        }
    });

    if let Some(idx) = launch_idx {
        dev.launch_app(idx);
    }
    if let Some(k) = nav_pressed {
        dev.press_nav(k);
    }
    resp
}

fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn is_letter(k: Key) -> bool {
    let n = k.name();
    n.len() == 1 && n.as_bytes()[0].is_ascii_alphabetic()
}

fn special_ascii(k: Key) -> Option<u8> {
    Some(match k {
        Key::Enter => 0x0D,
        Key::Backspace => 0x08,
        Key::Tab => 0x09,
        Key::Escape => 0x1B,
        Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight | Key::Home | Key::End | Key::PageUp | Key::PageDown | Key::Insert | Key::Delete => 0,
        Key::F1 | Key::F2 | Key::F3 | Key::F4 | Key::F5 | Key::F6 | Key::F7 | Key::F8 | Key::F9 | Key::F10 | Key::F11 | Key::F12 => 0,
        _ => return None,
    })
}

fn scan_for_key(k: Key) -> Option<u8> {
    use keys::*;
    Some(match k {
        Key::Escape => ESC,
        Key::Backspace => BACKSPACE,
        Key::Tab => TAB,
        Key::Enter => ENTER,
        Key::Space => SPACE,
        Key::ArrowUp => UP,
        Key::ArrowDown => DOWN,
        Key::ArrowLeft => LEFT,
        Key::ArrowRight => RIGHT,
        Key::Home => HOME,
        Key::End => END,
        Key::PageUp => PGUP,
        Key::PageDown => PGDN,
        Key::Insert => INSERT,
        Key::Delete => DELETE,
        Key::F11 => F11,
        Key::F12 => F12,
        Key::F1 | Key::F2 | Key::F3 | Key::F4 | Key::F5 | Key::F6 | Key::F7 | Key::F8 | Key::F9 | Key::F10 => {
            let n: u8 = k.name()[1..].parse().ok()?;
            F1 + n - 1
        }
        other => {
            let name = other.name();
            if name.chars().count() == 1 {
                scan_for_char(name.chars().next()?.to_ascii_lowercase())?
            } else {
                match other {
                    Key::Minus => scan_for_char('-')?,
                    Key::Equals => scan_for_char('=')?,
                    Key::Comma => scan_for_char(',')?,
                    Key::Period => scan_for_char('.')?,
                    Key::Semicolon => scan_for_char(';')?,
                    Key::Slash => scan_for_char('/')?,
                    Key::Backslash => scan_for_char('\\')?,
                    Key::OpenBracket => scan_for_char('[')?,
                    Key::CloseBracket => scan_for_char(']')?,
                    Key::Backtick => scan_for_char('`')?,
                    Key::Quote => scan_for_char('\'')?,
                    _ => return None,
                }
            }
        }
    })
}
