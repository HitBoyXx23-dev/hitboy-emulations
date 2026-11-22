//! 8042 keyboard controller (ports 0x60 data, 0x64 status/command) with a keyboard and a
//! PS/2 mouse behind it. Keyboard bytes are delivered already translated to scan code set 1,
//! as the controller does with translation on.

use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct Kbc {
    /// Bytes waiting in the output buffer: (byte, from the mouse).
    out: VecDeque<(u8, bool)>,
    /// Controller configuration byte: bit 0 keyboard IRQ, bit 1 mouse IRQ, bit 2 system
    /// flag, bit 4 keyboard clock off, bit 5 mouse clock off, bit 6 translation.
    pub config: u8,
    /// Controller command waiting for its data byte.
    pending: Option<u8>,
    /// Keyboard command waiting for its data byte.
    kbd_pending: Option<u8>,
    mouse_pending: Option<u8>,
    pub kbd_enabled: bool,
    pub mouse_enabled: bool,
    /// The guest asked for a CPU reset (command 0xFE).
    pub reset_requested: bool,
    /// Interrupts to raise: IRQ 1 / IRQ 12.
    pub irq1: bool,
    pub irq12: bool,
    /// Byte currently presented at port 0x60 (stays readable until replaced).
    current: (u8, bool),
    full: bool,
}

impl Default for Kbc {
    fn default() -> Self {
        Kbc {
            out: VecDeque::new(),
            config: 0x45,
            pending: None,
            kbd_pending: None,
            mouse_pending: None,
            kbd_enabled: true,
            mouse_enabled: false,
            reset_requested: false,
            irq1: false,
            irq12: false,
            current: (0, false),
            full: false,
        }
    }
}

impl Kbc {
    fn push(&mut self, b: u8, mouse: bool) {
        if self.out.len() < 64 {
            self.out.push_back((b, mouse));
        }
        self.refill();
    }

    /// Moves the next queued byte into the output buffer and raises its interrupt.
    fn refill(&mut self) {
        if self.full {
            return;
        }
        if let Some((b, mouse)) = self.out.pop_front() {
            self.current = (b, mouse);
            self.full = true;
            if mouse {
                self.irq12 |= self.config & 2 != 0;
            } else {
                self.irq1 |= self.config & 1 != 0;
            }
        }
    }

    /// Whether the output buffer is empty (the host may queue another key).
    pub fn idle(&self) -> bool {
        !self.full && self.out.is_empty()
    }

    /// A key event from the host (set 1 scan code; bit 7 = release).
    pub fn key(&mut self, scan: u8) {
        if self.kbd_enabled && self.config & 0x10 == 0 {
            self.push(scan, false);
        }
    }

    /// Mouse movement / buttons as a standard 3-byte PS/2 packet.
    pub fn mouse(&mut self, dx: i32, dy: i32, buttons: u8) {
        if !self.mouse_enabled || self.config & 0x20 != 0 {
            return;
        }
        let (dx, dy) = (dx.clamp(-255, 255), (-dy).clamp(-255, 255));
        let b0 = 0x08 | (buttons & 7) | if dx < 0 { 0x10 } else { 0 } | if dy < 0 { 0x20 } else { 0 };
        for b in [b0, dx as u8, dy as u8] {
            self.push(b, true);
        }
    }

    pub fn read(&mut self, port: u16) -> u8 {
        if port == 0x64 {
            // OBF, system flag, not inhibited, mouse data flag.
            return self.full as u8 | 0x04 | 0x10 | if self.full && self.current.1 { 0x20 } else { 0 };
        }
        let b = self.current.0;
        self.full = false;
        self.refill();
        b
    }

    pub fn write(&mut self, port: u16, v: u8) {
        if port == 0x64 {
            self.command(v);
            return;
        }
        if let Some(cmd) = self.pending.take() {
            match cmd {
                0x60 => {
                    self.config = v;
                }
                0xD1 => {
                    if v & 1 == 0 {
                        self.reset_requested = true;
                    }
                }
                0xD2 => self.push(v, false),
                0xD3 => self.push(v, true),
                0xD4 => self.mouse_cmd(v),
                _ => {}
            }
            return;
        }
        self.keyboard_cmd(v);
    }

    fn command(&mut self, c: u8) {
        match c {
            0x20 => {
                let cfg = self.config;
                self.push(cfg, false);
            }
            0x60 | 0xD1 | 0xD2 | 0xD3 | 0xD4 => self.pending = Some(c),
            0xA7 => self.config |= 0x20,
            0xA8 => self.config &= !0x20,
            0xA9 | 0xAB => self.push(0x00, false), // interface tests pass
            0xAA => {
                self.push(0x55, false); // self-test passed
            }
            0xAD => self.config |= 0x10,
            0xAE => self.config &= !0x10,
            0xC0 => self.push(0xA0, false), // input port: keyboard not locked
            0xD0 => self.push(0x03, false), // output port: A20 on, no reset
            0xFE => self.reset_requested = true,
            _ => {}
        }
    }

    fn keyboard_cmd(&mut self, v: u8) {
        if let Some(cmd) = self.kbd_pending.take() {
            let _ = cmd; // LED state, typematic rate, scan code set: acknowledged
            if cmd == 0xF0 && v == 0 {
                self.push(0xFA, false);
                self.push(0x41, false); // set 2, reported as translated set 1 code
                return;
            }
            self.push(0xFA, false);
            return;
        }
        match v {
            0xED | 0xF3 | 0xF0 => {
                self.kbd_pending = Some(v);
                self.push(0xFA, false);
            }
            0xEE => self.push(0xEE, false),
            0xF2 => {
                for b in [0xFA, 0xAB, 0x41] {
                    self.push(b, false);
                }
            }
            0xF4 => {
                self.kbd_enabled = true;
                self.push(0xFA, false);
            }
            0xF5 => {
                self.kbd_enabled = false;
                self.push(0xFA, false);
            }
            0xFF => {
                self.kbd_enabled = true;
                self.push(0xFA, false);
                self.push(0xAA, false);
            }
            _ => self.push(0xFA, false),
        }
    }

    fn mouse_cmd(&mut self, v: u8) {
        if self.mouse_pending.take().is_some() {
            self.push(0xFA, true); // sample rate / resolution argument
            return;
        }
        match v {
            0xF3 | 0xE8 => {
                self.mouse_pending = Some(v);
                self.push(0xFA, true);
            }
            0xF2 => {
                self.push(0xFA, true);
                self.push(0x00, true); // standard PS/2 mouse
            }
            0xF4 => {
                self.mouse_enabled = true;
                self.push(0xFA, true);
            }
            0xF5 => {
                self.mouse_enabled = false;
                self.push(0xFA, true);
            }
            0xE9 => {
                for b in [0xFA, 0x00, 0x02, 0x64] {
                    self.push(b, true);
                }
            }
            0xFF => {
                self.mouse_enabled = false;
                for b in [0xFA, 0xAA, 0x00] {
                    self.push(b, true);
                }
            }
            _ => self.push(0xFA, true),
        }
    }

    pub fn take_irqs(&mut self) -> (bool, bool) {
        (std::mem::take(&mut self.irq1), std::mem::take(&mut self.irq12))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_test_config_and_keys() {
        let mut k = Kbc::default();
        k.write(0x64, 0xAA);
        assert_eq!(k.read(0x64) & 1, 1);
        assert_eq!(k.read(0x60), 0x55);
        k.write(0x64, 0x20);
        assert_eq!(k.read(0x60), 0x45);
        k.write(0x60, 0xFF); // keyboard reset
        assert_eq!((k.read(0x60), k.read(0x60)), (0xFA, 0xAA));
        k.take_irqs();
        k.key(0x1E);
        assert_eq!(k.take_irqs(), (true, false));
        assert_eq!(k.read(0x60), 0x1E);
        assert!(k.idle());
    }

    #[test]
    fn mouse_packets() {
        let mut k = Kbc::default();
        k.write(0x64, 0x60);
        k.write(0x60, 0x47); // both interrupts on
        k.write(0x64, 0xD4);
        k.write(0x60, 0xF4);
        assert_eq!(k.read(0x60), 0xFA);
        k.mouse(5, -3, 1);
        assert_eq!(k.read(0x64) & 0x21, 0x21);
        assert_eq!([k.read(0x60), k.read(0x60), k.read(0x60)], [0x09, 5, 3]);
    }
}
