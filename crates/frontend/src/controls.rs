//! Controllers: which kind is plugged in, what each console's own pad looks like and calls
//! its buttons, and the default buttons for every console and controller pairing.

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Stroke, Ui, Vec2};

use crate::settings::PadBindings;
use crate::theme::t;

/// The family a connected controller belongs to (decides the button names shown).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PadKind {
    Xbox,
    PlayStation,
    Nintendo,
    Generic,
}

impl PadKind {
    pub fn name(self) -> &'static str {
        match self {
            PadKind::Xbox => "Xbox",
            PadKind::PlayStation => "PlayStation",
            PadKind::Nintendo => "Nintendo",
            PadKind::Generic => "Generic",
        }
    }
}

static CURRENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(3);

/// The family of the controller in use, remembered so every button label can use its names.
pub fn set_current(kind: PadKind) {
    CURRENT.store(kind as u8, std::sync::atomic::Ordering::Relaxed);
}

pub fn current() -> PadKind {
    match CURRENT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => PadKind::Xbox,
        1 => PadKind::PlayStation,
        2 => PadKind::Nintendo,
        _ => PadKind::Generic,
    }
}

/// Works out the family from the controller's name and USB vendor.
pub fn detect(name: &str, vendor: Option<u16>) -> PadKind {
    match vendor {
        Some(0x045E) => return PadKind::Xbox,
        Some(0x054C) => return PadKind::PlayStation,
        Some(0x057E) => return PadKind::Nintendo,
        _ => {}
    }
    let n = name.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    if has(&["xbox", "x-box", "xinput", "microsoft"]) {
        PadKind::Xbox
    } else if has(&["playstation", "dualshock", "dualsense", "ps4", "ps5", "ps3", "sony", "wireless controller"]) {
        PadKind::PlayStation
    } else if has(&["nintendo", "switch", "pro controller", "joy-con", "joycon"]) {
        PadKind::Nintendo
    } else {
        PadKind::Generic
    }
}

/// What a controller button is printed as on this family of pad.
pub fn glyph(kind: PadKind, button: &str) -> String {
    use PadKind::*;
    let pick = |x: &str, p: &str, n: &str| -> String {
        match kind {
            PlayStation => p.into(),
            Nintendo => n.into(),
            Xbox | Generic => x.into(),
        }
    };
    match button {
        "" => "Unbound".into(),
        // Face buttons by position (South = bottom).
        "South" => pick("A", "Cross", "B"),
        "East" => pick("B", "Circle", "A"),
        "West" => pick("X", "Square", "Y"),
        "North" => pick("Y", "Triangle", "X"),
        "LeftTrigger" => pick("LB", "L1", "L"),
        "RightTrigger" => pick("RB", "R1", "R"),
        "LeftTrigger2" => pick("LT", "L2", "ZL"),
        "RightTrigger2" => pick("RT", "R2", "ZR"),
        "Select" => pick("View", "Share", "-"),
        "Start" => pick("Menu", "Options", "+"),
        "LeftThumb" => pick("LS click", "L3", "LS click"),
        "RightThumb" => pick("RS click", "R3", "RS click"),
        "DPadUp" => "D-pad Up".into(),
        "DPadDown" => "D-pad Down".into(),
        "DPadLeft" => "D-pad Left".into(),
        "DPadRight" => "D-pad Right".into(),
        other => other.into(),
    }
}

/// Default controller buttons for a console. Nintendo-style consoles put A on the right and
/// B at the bottom (the real positions); Sega consoles use the classic bottom-row layout.
pub fn default_pad(console: &str) -> PadBindings {
    let base = PadBindings::default();
    match console {
        "nes" | "snes" | "gb" | "gba" | "vb" | "pce" | "ws" | "n64" | "gc" | "wii" | "wiiu" | "nds" | "3ds" | "switch" | "ngp" => PadBindings::nintendo(),
        // Two-button pads: button 1 at the bottom, 2 to its right.
        "sms" | "gg" | "a2600" | "a7800" => PadBindings { b: "East".into(), ..base },
        _ => base,
    }
}

/// Console button bit for a binding field (the shared button layout the cores read).
pub fn bit_of(field: &str) -> u16 {
    match field {
        "a" => 1,
        "b" => 2,
        "select" => 4,
        "start" => 8,
        "up" => 16,
        "down" => 32,
        "left" => 64,
        "right" => 128,
        "x" => 256,
        "y" => 512,
        "l" => 1024,
        "r" => 2048,
        _ => 0,
    }
}

// ---------------------------------------------------------------- layouts

#[derive(Clone, Copy)]
pub enum Kind {
    Round(f32),
    Pill(f32, f32),
    Bar(f32, f32),
}

#[derive(Clone)]
pub struct Btn {
    pub field: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub x: f32,
    pub y: f32,
    pub tint: Option<[u8; 3]>,
}

#[derive(Clone)]
pub struct DPad {
    pub x: f32,
    pub y: f32,
    /// Binding fields for up, down, left, right.
    pub fields: [&'static str; 4],
}

pub type Row = (&'static str, &'static str, &'static str);

pub struct Layout {
    pub name: &'static str,
    /// Body size in layout units.
    pub size: (f32, f32),
    pub round: f32,
    pub dpads: Vec<DPad>,
    pub buttons: Vec<Btn>,
    /// Screen (x, y, w, h) for handhelds.
    pub screen: Option<(f32, f32, f32, f32)>,
    pub movement: Vec<Row>,
    pub rows: Vec<Row>,
    /// A line explaining how this console's buttons map, shown under the diagram.
    pub note: &'static str,
}

const RED: Option<[u8; 3]> = Some([0xD0, 0x30, 0x38]);
const YELLOW: Option<[u8; 3]> = Some([0xE8, 0xB8, 0x30]);
const BLUE: Option<[u8; 3]> = Some([0x30, 0x60, 0xD0]);
const GREEN: Option<[u8; 3]> = Some([0x30, 0xA0, 0x50]);
const GREY: Option<[u8; 3]> = Some([0x70, 0x72, 0x7C]);

fn round(field: &'static str, label: &'static str, x: f32, y: f32, tint: Option<[u8; 3]>) -> Btn {
    Btn { field, label, kind: Kind::Round(11.0), x, y, tint }
}
fn pill(field: &'static str, label: &'static str, x: f32, y: f32) -> Btn {
    Btn { field, label, kind: Kind::Pill(22.0, 7.0), x, y, tint: None }
}
fn bar(field: &'static str, label: &'static str, x: f32, y: f32) -> Btn {
    Btn { field, label, kind: Kind::Bar(46.0, 7.0), x, y, tint: None }
}
fn dpad(x: f32, y: f32) -> DPad {
    DPad { x, y, fields: ["up", "down", "left", "right"] }
}
fn mv() -> Vec<Row> {
    vec![("up", "Up", "up"), ("down", "Down", "down"), ("left", "Left", "left"), ("right", "Right", "right")]
}
fn rows(list: &[(&'static str, &'static str)]) -> Vec<Row> {
    list.iter().map(|&(f, l)| (f, l, f)).collect()
}

/// The pad of a console: its shape, button names and which bindings it uses.
pub fn layout(console: &str) -> Layout {
    match console {
        "nes" => Layout {
            name: "NES controller",
            size: (200.0, 80.0),
            round: 10.0,
            dpads: vec![dpad(40.0, 40.0)],
            buttons: vec![pill("select", "SELECT", 84.0, 50.0), pill("start", "START", 116.0, 50.0), round("b", "B", 150.0, 42.0, RED), round("a", "A", 178.0, 42.0, RED)],
            screen: None,
            movement: mv(),
            rows: rows(&[("b", "B"), ("a", "A"), ("select", "Select"), ("start", "Start"), ("turbo_b", "Turbo B"), ("turbo_a", "Turbo A")]),
            note: "Famicom Disk System games use the same pad.",
        },
        "snes" => Layout {
            name: "Super Nintendo controller",
            size: (250.0, 110.0),
            round: 40.0,
            dpads: vec![dpad(50.0, 56.0)],
            buttons: vec![
                bar("l", "L", 38.0, 6.0),
                bar("r", "R", 212.0, 6.0),
                pill("select", "SELECT", 106.0, 66.0),
                pill("start", "START", 144.0, 66.0),
                round("x", "X", 196.0, 36.0, BLUE),
                round("y", "Y", 172.0, 56.0, GREEN),
                round("a", "A", 220.0, 56.0, RED),
                round("b", "B", 196.0, 76.0, YELLOW),
            ],
            screen: None,
            movement: mv(),
            rows: rows(&[("a", "A"), ("b", "B"), ("x", "X"), ("y", "Y"), ("l", "L"), ("r", "R"), ("select", "Select"), ("start", "Start"), ("turbo_a", "Turbo A"), ("turbo_b", "Turbo B")]),
            note: "",
        },
        "gb" => Layout {
            name: "Game Boy / Color",
            size: (110.0, 170.0),
            round: 12.0,
            dpads: vec![dpad(32.0, 108.0)],
            buttons: vec![round("b", "B", 68.0, 122.0, Some([0x9A, 0x30, 0x60])), round("a", "A", 92.0, 108.0, Some([0x9A, 0x30, 0x60])), pill("select", "SELECT", 40.0, 154.0), pill("start", "START", 74.0, 154.0)],
            screen: Some((12.0, 12.0, 86.0, 74.0)),
            movement: mv(),
            rows: rows(&[("a", "A"), ("b", "B"), ("select", "Select"), ("start", "Start"), ("turbo_a", "Turbo A"), ("turbo_b", "Turbo B")]),
            note: "",
        },
        "gba" => Layout {
            name: "Game Boy Advance",
            size: (250.0, 110.0),
            round: 26.0,
            dpads: vec![dpad(30.0, 52.0)],
            buttons: vec![bar("l", "L", 36.0, 6.0), bar("r", "R", 214.0, 6.0), round("b", "B", 198.0, 62.0, GREY), round("a", "A", 224.0, 48.0, GREY), pill("select", "SELECT", 108.0, 96.0), pill("start", "START", 142.0, 96.0)],
            screen: Some((62.0, 18.0, 126.0, 66.0)),
            movement: mv(),
            rows: rows(&[("a", "A"), ("b", "B"), ("l", "L"), ("r", "R"), ("select", "Select"), ("start", "Start"), ("turbo_a", "Turbo A"), ("turbo_b", "Turbo B")]),
            note: "",
        },
        "md" => Layout {
            name: "Genesis / Mega Drive 6-button pad",
            size: (240.0, 96.0),
            round: 34.0,
            dpads: vec![dpad(46.0, 50.0)],
            buttons: vec![
                pill("start", "START", 112.0, 78.0),
                round("b", "A", 146.0, 62.0, GREY),
                round("a", "B", 172.0, 52.0, GREY),
                round("y", "C", 198.0, 42.0, GREY),
                round("l", "X", 146.0, 32.0, Some([0x50, 0x52, 0x5C])),
                round("x", "Y", 172.0, 22.0, Some([0x50, 0x52, 0x5C])),
                round("r", "Z", 198.0, 12.0, Some([0x50, 0x52, 0x5C])),
                pill("select", "MODE", 112.0, 30.0),
            ],
            screen: None,
            movement: mv(),
            rows: rows(&[("b", "A"), ("a", "B"), ("y", "C"), ("l", "X"), ("x", "Y"), ("r", "Z"), ("start", "Start"), ("select", "Mode")]),
            note: "X, Y, Z and Mode are only used by 6-button games (Street Fighter II, Mortal Kombat).",
        },
        "sms" => Layout {
            name: "Master System pad",
            size: (170.0, 90.0),
            round: 12.0,
            dpads: vec![dpad(44.0, 46.0)],
            buttons: vec![round("a", "1", 118.0, 54.0, GREY), round("b", "2", 144.0, 40.0, GREY), pill("start", "PAUSE", 84.0, 76.0)],
            screen: None,
            movement: mv(),
            rows: rows(&[("a", "Button 1"), ("b", "Button 2"), ("start", "Pause")]),
            note: "Pause is on the console itself.",
        },
        "gg" => Layout {
            name: "Game Gear",
            size: (240.0, 104.0),
            round: 30.0,
            dpads: vec![dpad(32.0, 52.0)],
            buttons: vec![round("a", "1", 196.0, 58.0, GREY), round("b", "2", 220.0, 44.0, GREY), pill("start", "START", 214.0, 14.0)],
            screen: Some((62.0, 12.0, 116.0, 80.0)),
            movement: mv(),
            rows: rows(&[("a", "Button 1"), ("b", "Button 2"), ("start", "Start")]),
            note: "",
        },
        "a2600" => Layout {
            name: "Atari 2600 joystick",
            size: (170.0, 110.0),
            round: 14.0,
            dpads: vec![dpad(44.0, 52.0)],
            buttons: vec![round("a", "FIRE", 130.0, 52.0, RED), pill("start", "RESET", 70.0, 94.0), pill("select", "SELECT", 104.0, 94.0)],
            screen: None,
            movement: mv(),
            rows: rows(&[("a", "Fire"), ("start", "Reset"), ("select", "Select")]),
            note: "",
        },
        "a7800" => Layout {
            name: "Atari 7800 joystick",
            size: (190.0, 110.0),
            round: 14.0,
            dpads: vec![dpad(44.0, 52.0)],
            buttons: vec![round("a", "FIRE 1", 120.0, 56.0, RED), round("b", "FIRE 2", 156.0, 56.0, RED), pill("start", "RESET", 70.0, 94.0), pill("select", "SELECT", 104.0, 94.0), pill("x", "PAUSE", 140.0, 94.0)],
            screen: None,
            movement: mv(),
            rows: rows(&[("a", "Fire 1"), ("b", "Fire 2"), ("start", "Reset"), ("select", "Select"), ("x", "Pause")]),
            note: "",
        },
        "pce" => Layout {
            name: "TurboGrafx-16 / PC Engine pad",
            size: (210.0, 74.0),
            round: 12.0,
            dpads: vec![dpad(38.0, 37.0)],
            buttons: vec![pill("select", "SELECT", 88.0, 44.0), pill("start", "RUN", 120.0, 44.0), round("b", "II", 162.0, 40.0, GREY), round("a", "I", 188.0, 34.0, GREY)],
            screen: None,
            movement: mv(),
            rows: rows(&[("a", "Button I"), ("b", "Button II"), ("select", "Select"), ("start", "Run"), ("turbo_a", "Turbo I"), ("turbo_b", "Turbo II")]),
            note: "",
        },
        "ws" => Layout {
            name: "WonderSwan",
            size: (260.0, 112.0),
            round: 20.0,
            dpads: vec![DPad { x: 34.0, y: 56.0, fields: ["up", "down", "left", "right"] }, DPad { x: 226.0, y: 28.0, fields: ["x", "y", "l", "r"] }],
            buttons: vec![round("b", "B", 208.0, 84.0, GREY), round("a", "A", 234.0, 70.0, GREY), pill("start", "START", 130.0, 98.0)],
            screen: Some((66.0, 12.0, 128.0, 78.0)),
            movement: mv(),
            rows: rows(&[("a", "A"), ("b", "B"), ("start", "Start"), ("x", "Y up"), ("y", "Y down"), ("l", "Y left"), ("r", "Y right")]),
            note: "The Y pad is for games held upright; those games also turn the picture.",
        },
        "vb" => Layout {
            name: "Virtual Boy controller",
            size: (260.0, 100.0),
            round: 24.0,
            dpads: vec![dpad(46.0, 50.0), DPad { x: 214.0, y: 50.0, fields: ["x", "y", "l", "r"] }],
            buttons: vec![round("a", "A", 150.0, 56.0, RED), round("b", "B", 114.0, 56.0, RED), pill("select", "SELECT", 100.0, 84.0), pill("start", "START", 160.0, 84.0)],
            screen: None,
            movement: mv(),
            rows: rows(&[("a", "A"), ("b", "B"), ("select", "Select"), ("start", "Start"), ("x", "Right pad up"), ("y", "Right pad down"), ("l", "L trigger"), ("r", "R trigger")]),
            note: "L and R are the triggers under the pad; the right D-pad is used by some games.",
        },
        _ => Layout {
            name: "Controller",
            size: (230.0, 100.0),
            round: 34.0,
            dpads: vec![dpad(50.0, 56.0)],
            buttons: vec![
                bar("l", "L", 44.0, 6.0),
                bar("r", "R", 186.0, 6.0),
                pill("select", "SELECT", 98.0, 70.0),
                pill("start", "START", 132.0, 70.0),
                round("y", "Y", 168.0, 56.0, None),
                round("x", "X", 192.0, 40.0, None),
                round("b", "B", 192.0, 72.0, None),
                round("a", "A", 216.0, 56.0, None),
            ],
            screen: None,
            movement: mv(),
            rows: rows(&[("a", "A"), ("b", "B"), ("x", "X"), ("y", "Y"), ("l", "L"), ("r", "R"), ("start", "Start"), ("select", "Select"), ("turbo_a", "Turbo A"), ("turbo_b", "Turbo B")]),
            note: "",
        },
    }
}

fn tint(c: Option<[u8; 3]>, fallback: Color32) -> Color32 {
    c.map(|[r, g, b]| Color32::from_rgb(r, g, b)).unwrap_or(fallback)
}

fn mix(a: Color32, b: Color32, f: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Height the diagram wants for a layout at a given width.
pub fn diagram_height(l: &Layout, width: f32) -> f32 {
    let scale = (width / l.size.0).min(2.2);
    (l.size.1 * scale + 24.0).clamp(150.0, 300.0)
}

/// Draws a console's controller. Buttons in `lit` (held) glow; `focus` (the hovered binding
/// row) is outlined.
pub fn paint(ui: &Ui, area: Rect, l: &Layout, lit: &[&str], focus: Option<&str>) {
    let p = ui.painter();
    let scale = ((area.width() - 8.0) / l.size.0).min((area.height() - 14.0) / l.size.1).min(2.2).max(0.5);
    let body = Rect::from_center_size(area.center(), Vec2::new(l.size.0, l.size.1) * scale);
    let at = |x: f32, y: f32| -> Pos2 { body.min + Vec2::new(x, y) * scale };
    let dark = Color32::from_rgb(0x0A, 0x0B, 0x10);
    let on = |f: &str| lit.contains(&f);
    let focused = |f: &str| focus == Some(f);
    let glow = |c: Pos2, r: f32| {
        for i in 0..4 {
            p.circle_filled(c, r + (4.0 + i as f32 * 3.0) * scale / 1.5, t().primary.gamma_multiply(0.10 - i as f32 * 0.02));
        }
    };

    // Body with depth.
    let rad = (l.round * scale) as u8;
    p.rect_filled(body.translate(Vec2::new(0.0, 7.0)), CornerRadius::same(rad), Color32::from_black_alpha(110));
    p.rect(body, CornerRadius::same(rad), t().surface_hover, Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
    if let Some((x, y, w, h)) = l.screen {
        let r = Rect::from_min_size(at(x, y), Vec2::new(w, h) * scale);
        p.rect_filled(r, CornerRadius::same((4.0 * scale) as u8), Color32::from_rgb(0x12, 0x18, 0x14));
        p.rect_stroke(r, CornerRadius::same((4.0 * scale) as u8), Stroke::new(1.0_f32, t().border), egui::StrokeKind::Inside);
    }

    // D-pads.
    for d in &l.dpads {
        let c = at(d.x, d.y);
        let arm = 12.0 * scale;
        p.rect_filled(Rect::from_center_size(c, Vec2::splat(arm)), CornerRadius::ZERO, dark);
        for (i, (dx, dy)) in [(0.0, -1.0), (0.0, 1.0), (-1.0, 0.0), (1.0, 0.0)].into_iter().enumerate() {
            let f = d.fields[i];
            let r = Rect::from_center_size(c + Vec2::new(dx, dy) * arm, Vec2::splat(arm));
            p.rect_filled(r, CornerRadius::same((2.0 * scale) as u8), if on(f) { t().primary } else { dark });
            if focused(f) {
                p.rect_stroke(r.expand(2.0), CornerRadius::same((4.0 * scale) as u8), Stroke::new(2.0_f32, t().primary), egui::StrokeKind::Outside);
            }
        }
    }

    // Buttons.
    for b in &l.buttons {
        let c = at(b.x, b.y);
        let (lit_now, foc) = (on(b.field), focused(b.field));
        match b.kind {
            Kind::Round(r) => {
                let r = r * scale;
                if lit_now {
                    glow(c, r);
                }
                let base = tint(b.tint, t().primary);
                p.circle_filled(c + Vec2::new(0.0, 2.5 * scale), r, Color32::from_black_alpha(120));
                p.circle_filled(c, r, if lit_now { mix(base, Color32::WHITE, 0.35) } else { mix(base, dark, 0.30) });
                if foc {
                    p.circle_stroke(c, r + 3.0, Stroke::new(2.0_f32, t().primary));
                }
                p.text(c, Align2::CENTER_CENTER, b.label, FontId::proportional((r * 1.0).clamp(8.0, 18.0)), Color32::WHITE);
            }
            Kind::Pill(w, h) | Kind::Bar(w, h) => {
                let bar = matches!(b.kind, Kind::Bar(..));
                let r = Rect::from_center_size(c, Vec2::new(w, h) * scale);
                let fill = if lit_now { t().primary } else if bar { mix(dark, t().surface, 0.4) } else { dark };
                p.rect_filled(r, CornerRadius::same((h * scale / 2.0) as u8 + 1), fill);
                if foc {
                    p.rect_stroke(r.expand(3.0), CornerRadius::same((h * scale / 2.0) as u8 + 3), Stroke::new(2.0_f32, t().primary), egui::StrokeKind::Outside);
                }
                if bar {
                    p.text(r.center(), Align2::CENTER_CENTER, b.label, FontId::proportional((9.0 * scale).clamp(8.0, 13.0)), if lit_now { t().on_primary } else { t().muted });
                } else {
                    p.text(c + Vec2::new(0.0, (h * scale / 2.0) + 7.0 * scale.min(1.4)), Align2::CENTER_CENTER, b.label, FontId::proportional((8.0 * scale).clamp(7.5, 11.0)), t().muted);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_controller_families() {
        assert_eq!(detect("Xbox Wireless Controller", None), PadKind::Xbox);
        assert_eq!(detect("Controller (XBOX 360 For Windows)", None), PadKind::Xbox);
        assert_eq!(detect("Wireless Controller", None), PadKind::PlayStation);
        assert_eq!(detect("DualSense Wireless Controller", None), PadKind::PlayStation);
        assert_eq!(detect("Pro Controller", None), PadKind::Nintendo);
        assert_eq!(detect("USB Gamepad", None), PadKind::Generic);
        // The USB vendor wins over a misleading name.
        assert_eq!(detect("Gamepad", Some(0x054C)), PadKind::PlayStation);
        assert_eq!(detect("Gamepad", Some(0x057E)), PadKind::Nintendo);
    }

    #[test]
    fn buttons_are_named_for_the_pad_in_hand() {
        assert_eq!(glyph(PadKind::Xbox, "South"), "A");
        assert_eq!(glyph(PadKind::PlayStation, "South"), "Cross");
        assert_eq!(glyph(PadKind::Nintendo, "South"), "B");
        assert_eq!(glyph(PadKind::Nintendo, "East"), "A");
        assert_eq!(glyph(PadKind::PlayStation, "RightTrigger"), "R1");
        assert_eq!(glyph(PadKind::Xbox, ""), "Unbound");
    }

    #[test]
    fn consoles_get_their_own_default_buttons() {
        // Nintendo: A on the right (East), B at the bottom (South).
        let n = default_pad("snes");
        assert_eq!((n.a.as_str(), n.b.as_str()), ("East", "South"));
        // Genesis keeps the classic row: B bottom, A left, C right.
        let g = default_pad("md");
        assert_eq!((g.a.as_str(), g.b.as_str(), g.y.as_str()), ("South", "West", "East"));
        // Two-button Sega/Atari pads: 1 at the bottom, 2 to its right.
        let s = default_pad("gg");
        assert_eq!((s.a.as_str(), s.b.as_str()), ("South", "East"));
    }

    #[test]
    fn every_console_layout_is_complete() {
        for c in ["nes", "snes", "gb", "gba", "md", "sms", "gg", "a2600", "a7800", "pce", "ws", "vb", "other"] {
            let l = layout(c);
            assert_eq!(l.movement.len(), 4, "{c}");
            assert!(!l.rows.is_empty(), "{c}");
            // Every row can be lit on the diagram: its field is drawn as a button or pad arm.
            for (field, _, _) in l.movement.iter().chain(&l.rows) {
                if field.starts_with("turbo") {
                    continue;
                }
                let drawn = l.buttons.iter().any(|b| b.field == *field) || l.dpads.iter().any(|d| d.fields.contains(field));
                assert!(drawn, "{c}: {field} has no place on the diagram");
            }
            // Parts stay inside the body.
            for b in &l.buttons {
                assert!(b.x >= 0.0 && b.x <= l.size.0 && b.y >= 0.0 && b.y <= l.size.1, "{c}: {} is off the body", b.label);
            }
        }
    }
}
