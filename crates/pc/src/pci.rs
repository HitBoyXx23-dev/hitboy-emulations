//! PCI configuration space (mechanism #1, ports 0xCF8/0xCFC): an Intel 440FX host bridge
//! and a PIIX3 south bridge (ISA bridge + IDE controller in legacy mode). Operating systems
//! find the disk controller here and then use the classic IDE ports and IRQs 14/15.

use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct Pci {
    pub addr: u32,
    space: BTreeMap<(u8, u8, u8), [u8; 256]>,
}

fn device(vendor: u16, id: u16, class: u32, header: u8) -> [u8; 256] {
    let mut c = [0u8; 256];
    c[0..2].copy_from_slice(&vendor.to_le_bytes());
    c[2..4].copy_from_slice(&id.to_le_bytes());
    c[4] = 0x07; // I/O, memory, bus master enabled
    c[6] = 0x80; // fast back-to-back
    c[8] = 0x00; // revision
    c[9..12].copy_from_slice(&class.to_le_bytes()[..3]);
    c[0x0E] = header;
    c
}

impl Default for Pci {
    fn default() -> Self {
        let mut space = BTreeMap::new();
        space.insert((0, 0, 0), device(0x8086, 0x1237, 0x06_0000, 0x00));
        space.insert((0, 1, 0), device(0x8086, 0x7000, 0x06_0100, 0x80));
        // IDE: legacy (compatibility) mode on both channels, bus mastering not offered.
        let mut ide = device(0x8086, 0x7010, 0x01_0180, 0x00);
        ide[4] = 0x05;
        // IDE timing registers with "decode enable" set for both channels.
        ide[0x41] = 0x80;
        ide[0x43] = 0x80;
        ide[0x3D] = 1; // interrupt pin A (legacy IRQs 14/15 are used in compatibility mode)
        space.insert((0, 1, 1), ide);
        Pci { addr: 0, space }
    }
}

impl Pci {
    fn target(&self) -> Option<((u8, u8, u8), usize)> {
        if self.addr & 0x8000_0000 == 0 {
            return None;
        }
        let bdf = ((self.addr >> 16) as u8, ((self.addr >> 11) & 0x1F) as u8, ((self.addr >> 8) & 7) as u8);
        Some((bdf, (self.addr & 0xFC) as usize))
    }

    /// Data port read (`off` = 0..3 within 0xCFC).
    pub fn read(&self, off: u16, size: u8) -> u32 {
        let Some((bdf, reg)) = self.target() else { return u32::MAX };
        let Some(c) = self.space.get(&bdf) else { return u32::MAX >> (32 - size as u32 * 8) };
        let o = reg + off as usize;
        (0..size as usize).fold(0, |v, i| v | (*c.get(o + i).unwrap_or(&0) as u32) << (i * 8))
    }

    pub fn write(&mut self, off: u16, size: u8, v: u32) {
        let Some((bdf, reg)) = self.target() else { return };
        let Some(c) = self.space.get_mut(&bdf) else { return };
        let o = reg + off as usize;
        for i in 0..size as usize {
            let at = o + i;
            // IDs, class and header type are read-only; BARs are unimplemented (read as 0).
            if (4..8).contains(&at) || (0x0C..0x0E).contains(&at) || at == 0x0F || (0x3C..0x3D).contains(&at) || at >= 0x40 {
                if at < 256 {
                    c[at] = (v >> (i * 8)) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_piix_ide() {
        let mut p = Pci::default();
        p.addr = 0x8000_0000 | 1 << 11 | 1 << 8;
        assert_eq!(p.read(0, 4), 0x7010_8086);
        p.addr = 0x8000_0000 | 1 << 11 | 1 << 8 | 0x08;
        assert_eq!(p.read(0, 4) >> 8, 0x01_0180);
        p.addr = 0x8000_0000 | 5 << 11;
        assert_eq!(p.read(0, 2), 0xFFFF);
    }
}
