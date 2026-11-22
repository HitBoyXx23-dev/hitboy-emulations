//! Product branding: each edition is the same app with its own name and palette.

use std::sync::OnceLock;

use eframe::egui::Color32;

pub struct Brand {
    /// Used for the settings folder name.
    pub id: &'static str,
    pub name: &'static str,
    pub suffix: &'static str,
    pub primary: Color32,
    pub secondary: Color32,
    /// Text drawn on top of `primary` fills.
    pub on_primary: Color32,

    pub bg: Color32,
    pub panel: Color32,
    pub surface: Color32,
    pub surface_hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub ok: Color32,
    pub danger: Color32,
}

const ROYAL_BLUE: Color32 = Color32::from_rgb(0x41, 0x69, 0xE1);
const ROYAL_PURPLE: Color32 = Color32::from_rgb(0x78, 0x51, 0xA9);

const fn brand(
    id: &'static str,
    name: &'static str,
    primary: Color32,
    secondary: Color32,
    on_primary: Color32,
) -> Brand {
    Brand {
        id,
        name,
        suffix: "Emulations",
        primary,
        secondary,
        on_primary,
        bg: Color32::from_rgb(0x06, 0x06, 0x0A),
        panel: Color32::from_rgb(0x0C, 0x0D, 0x13),
        surface: Color32::from_rgb(0x13, 0x15, 0x1E),
        surface_hover: Color32::from_rgb(0x1B, 0x1E, 0x2B),
        border: Color32::from_rgb(0x23, 0x26, 0x36),
        text: Color32::from_rgb(0xE8, 0xEA, 0xF2),
        muted: Color32::from_rgb(0x8A, 0x90, 0xA6),
        ok: Color32::from_rgb(0x3D, 0xDC, 0x97),
        danger: Color32::from_rgb(0xF0, 0x6A, 0x6A),
    }
}

pub static HITBOY: Brand = brand("hitboy", "HitBoy's", ROYAL_PURPLE, ROYAL_BLUE, Color32::WHITE);

static ACTIVE: OnceLock<&'static Brand> = OnceLock::new();

pub fn set(brand: &'static Brand) {
    let _ = ACTIVE.set(brand);
}

/// The active brand/theme.
pub fn t() -> &'static Brand {
    ACTIVE.get().copied().unwrap_or(&HITBOY)
}

impl Brand {
    pub fn full_name(&self) -> String {
        format!("{} {}", self.name, self.suffix)
    }
}
