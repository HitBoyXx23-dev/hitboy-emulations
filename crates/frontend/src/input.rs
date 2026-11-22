//! Keyboard + gamepad -> console button mapping (with turbo and per-button pad remapping).

use eframe::egui::{Context, Key};
use gilrs::{Axis, Button, Gilrs};
use nes::controller::button;

use crate::settings::{KeyBindings, PadBindings, Settings};

/// Extra buttons beyond the NES layout (bits 8-11), used by SNES games.
pub mod extra {
    pub const X: u16 = 1 << 8;
    pub const Y: u16 = 1 << 9;
    pub const L: u16 = 1 << 10;
    pub const R: u16 = 1 << 11;
}

pub fn key(name: &str) -> Option<Key> {
    Key::from_name(name)
}

/// Held now, or pressed at any point since the last frame (so quick taps aren't lost).
pub fn key_down(ctx: &Context, name: &str) -> bool {
    key(name).is_some_and(|k| ctx.input(|i| i.key_down(k) || i.key_pressed(k)))
}

pub fn key_pressed(ctx: &Context, name: &str) -> bool {
    key(name).is_some_and(|k| ctx.input(|i| i.key_pressed(k)))
}

/// Every controller button that can be bound, in display order.
pub const PAD_BUTTONS: &[Button] = &[
    Button::South,
    Button::East,
    Button::West,
    Button::North,
    Button::LeftTrigger,
    Button::RightTrigger,
    Button::LeftTrigger2,
    Button::RightTrigger2,
    Button::Select,
    Button::Start,
    Button::LeftThumb,
    Button::RightThumb,
    Button::DPadUp,
    Button::DPadDown,
    Button::DPadLeft,
    Button::DPadRight,
];

pub fn pad_button(name: &str) -> Option<Button> {
    PAD_BUTTONS.iter().copied().find(|b| format!("{b:?}") == name)
}

pub fn pad_name(b: Button) -> String {
    format!("{b:?}")
}

/// Console buttons currently held on the keyboard. `turbo_on` is the rapid-fire phase.
pub fn keyboard_buttons(ctx: &Context, k: &KeyBindings, turbo_on: bool) -> u16 {
    let mut out = [
        (&k.a, button::A as u16),
        (&k.b, button::B as u16),
        (&k.select, button::SELECT as u16),
        (&k.start, button::START as u16),
        (&k.up, button::UP as u16),
        (&k.down, button::DOWN as u16),
        (&k.left, button::LEFT as u16),
        (&k.right, button::RIGHT as u16),
        (&k.x, extra::X),
        (&k.y, extra::Y),
        (&k.l, extra::L),
        (&k.r, extra::R),
    ]
    .iter()
    .filter(|(name, _)| key_down(ctx, name))
    .fold(0, |acc, (_, b)| acc | b);
    if turbo_on {
        if key_down(ctx, &k.turbo_a) {
            out |= button::A as u16;
        }
        if key_down(ctx, &k.turbo_b) {
            out |= button::B as u16;
        }
    }
    out
}

fn held(pad: &gilrs::Gamepad, name: &str) -> bool {
    pad_button(name).is_some_and(|b| pad.is_pressed(b))
}

pub fn gamepad_buttons(pad: &gilrs::Gamepad, s: &Settings, pb: &PadBindings, turbo_on: bool) -> u16 {
    let mut out = [
        (&pb.a, button::A as u16),
        (&pb.b, button::B as u16),
        (&pb.select, button::SELECT as u16),
        (&pb.start, button::START as u16),
        (&pb.up, button::UP as u16),
        (&pb.down, button::DOWN as u16),
        (&pb.left, button::LEFT as u16),
        (&pb.right, button::RIGHT as u16),
        (&pb.x, extra::X),
        (&pb.y, extra::Y),
        (&pb.l, extra::L),
        (&pb.r, extra::R),
    ]
    .iter()
    .filter(|(name, _)| held(pad, name))
    .fold(0, |acc, (_, b)| acc | b);
    if turbo_on {
        if held(pad, &pb.turbo_a) {
            out |= button::A as u16;
        }
        if held(pad, &pb.turbo_b) {
            out |= button::B as u16;
        }
    }
    if s.stick_as_dpad {
        let dz = s.stick_deadzone;
        let x = pad.value(Axis::LeftStickX);
        let y = pad.value(Axis::LeftStickY);
        if x < -dz {
            out |= button::LEFT as u16;
        }
        if x > dz {
            out |= button::RIGHT as u16;
        }
        if y > dz {
            out |= button::UP as u16;
        }
        if y < -dz {
            out |= button::DOWN as u16;
        }
    }
    // Up+Down / Left+Right together crash some games; cancel them out.
    let (ud, lr) = ((button::UP | button::DOWN) as u16, (button::LEFT | button::RIGHT) as u16);
    if out & ud == ud {
        out &= !ud;
    }
    if out & lr == lr {
        out &= !lr;
    }
    out
}

/// Whether any connected controller is holding the named button (used for hold-to-rewind).
pub fn pad_held(g: Option<&Gilrs>, name: &str) -> bool {
    g.is_some_and(|g| g.gamepads().filter(|(_, p)| p.is_connected()).any(|(_, p)| held(&p, name)))
}

/// Player 1 = keyboard + first pad, player 2 = second pad.
pub fn poll(ctx: &Context, gilrs: Option<&mut Gilrs>, s: &Settings, keys: &KeyBindings, turbo_on: bool) -> [u16; 2] {
    let mut pads = [keyboard_buttons(ctx, keys, turbo_on), 0];
    if let Some(g) = gilrs {
        while g.next_event().is_some() {}
        if s.gamepad_enabled {
            for (slot, (_, pad)) in g.gamepads().filter(|(_, p)| p.is_connected()).take(2).enumerate() {
                pads[slot] |= gamepad_buttons(&pad, s, &keys.pad, turbo_on);
            }
        }
    }
    pads
}

/// Raw "is it held" state for the live tester: (keyboard mask, pad mask, turbo_a, turbo_b).
pub fn live_state(ctx: &Context, gilrs: Option<&Gilrs>, s: &Settings, keys: &KeyBindings) -> (u16, bool, bool) {
    let mut mask = keyboard_buttons(ctx, keys, false);
    let mut ta = key_down(ctx, &keys.turbo_a);
    let mut tb = key_down(ctx, &keys.turbo_b);
    if let Some(g) = gilrs {
        for (_, pad) in g.gamepads().filter(|(_, p)| p.is_connected()) {
            mask |= gamepad_buttons(&pad, s, &keys.pad, false);
            ta |= held(&pad, &keys.pad.turbo_a);
            tb |= held(&pad, &keys.pad.turbo_b);
        }
    }
    (mask, ta, tb)
}
