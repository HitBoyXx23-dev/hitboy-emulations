//! In-app screen for VMs running on the built-in HitBoy x86 engine.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, ColorImage, FontId, Key, Rect, Sense, TextureHandle, TextureOptions, Vec2};
use pc::keys;
use pc::{Machine, Screen};

use crate::theme::t;

pub struct VmSession {
    pub name: String,
    pub machine: Machine,
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
            machine,
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
        self.forward_keys(ctx);
        let stats = self.machine.run_frame(dt, Duration::from_millis(14));
        self.mips_acc += stats.instructions;
        if self.mips_since.elapsed() >= Duration::from_secs(1) {
            self.mips = self.mips_acc as f64 / self.mips_since.elapsed().as_secs_f64() / 1e6;
            self.mips_acc = 0;
            self.mips_since = now;
        }
        ctx.request_repaint();
    }

    fn forward_keys(&mut self, ctx: &egui::Context) {
        // Modifier keys as make/break scan codes (Shift, Ctrl, Alt), for guests that read the
        // keyboard directly.
        let mods = ctx.input(|i| i.modifiers);
        for (now, before, scan) in [(mods.shift, self.mods.shift, 0x2Au8), (mods.ctrl, self.mods.ctrl, 0x1D), (mods.alt, self.mods.alt, 0x38)] {
            if now && !before {
                self.machine.key_down(scan, 0);
            } else if !now && before {
                self.machine.key_up(scan);
            }
        }
        self.mods = mods;
        // Mouse: relative movement and buttons while the pointer is over the screen.
        if self.hovered {
            let (delta, buttons) = ctx.input(|i| {
                let p = &i.pointer;
                let b = p.button_down(egui::PointerButton::Primary) as u8 | (p.button_down(egui::PointerButton::Secondary) as u8) << 1 | (p.button_down(egui::PointerButton::Middle) as u8) << 2;
                (p.delta(), b)
            });
            if delta != Vec2::ZERO || buttons != self.buttons {
                self.machine.mouse(delta.x.round() as i32, delta.y.round() as i32, buttons);
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
                        self.machine.key_down(scan, ascii);
                    }
                }
                egui::Event::Key { key, pressed, modifiers, repeat, .. } => {
                    let Some(scan) = scan_for_key(key) else { continue };
                    if !pressed {
                        self.machine.key_up(scan);
                        continue;
                    }
                    let _ = repeat;
                    if let Some(ascii) = special_ascii(key) {
                        self.machine.key_down(scan, ascii);
                    } else if modifiers.ctrl && is_letter(key) {
                        // Ctrl+letter produces control codes (Ctrl+C = 0x03).
                        let letter = key.name().as_bytes()[0].to_ascii_uppercase();
                        self.machine.key_down(scan, letter - b'@');
                    }
                    // Printable keys arrive through Event::Text.
                }
                _ => {}
            }
        }
    }

    /// Draws the guest display into `rect`.
    pub fn draw(&mut self, ui: &mut egui::Ui, avail: Rect) -> egui::Response {
        let resp = ui.allocate_rect(avail, Sense::click());
        self.hovered = resp.hovered();
        let painter = ui.painter_at(avail);
        match self.machine.screen() {
            Screen::Text { cols, rows, cells, cursor } => {
                // 9x16 character cells, like VGA text mode.
                let (w, h) = (cols as f32 * 9.0, rows as f32 * 16.0);
                let scale = (avail.width() / w).min(avail.height() / h);
                let (cw, ch) = (9.0 * scale, 16.0 * scale);
                let origin = avail.center() - Vec2::new(w * scale, h * scale) / 2.0;
                let font = FontId::monospace(ch * 0.78);
                // Backgrounds: one rectangle per run of equal colour (typically a few per row).
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
                // Preserve native aspect ratio (supports 4:3 VGA, 16:9 desktop Linux/Windows, and portrait phone VMs).
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
        if let Some(err) = self.machine.error() {
            let card = Rect::from_center_size(avail.center(), Vec2::new(460.0, 90.0));
            painter.rect_filled(card, 12.0, t().panel);
            painter.text(card.center() - Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, "Boot failed", FontId::proportional(20.0), t().danger);
            painter.text(card.center() + Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, err, FontId::proportional(13.0), t().muted);
        }
        resp
    }
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
