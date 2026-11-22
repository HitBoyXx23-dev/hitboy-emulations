//! PCI configuration space (mechanism #1, I/O 0xCF8 / 0xCFC) with the Xbox's devices:
//! the nForce host bridge, MCPX functions and the NV2A behind the AGP bridge.

use std::collections::BTreeMap;

/// (bus, device, function) -> (vendor, device id, class code).
const DEVICES: &[((u8, u8, u8), u16, u32)] = &[
    ((0, 0, 0), 0x02A5, 0x060000), // host bridge
    ((0, 0, 3), 0x02A6, 0x050000), // memory controller
    ((0, 1, 0), 0x01B2, 0x060100), // LPC / ISA bridge (MCPX)
    ((0, 1, 1), 0x01B4, 0x0C0500), // SMBus
    ((0, 2, 0), 0x01C2, 0x0C0310), // USB 0
    ((0, 3, 0), 0x01C2, 0x0C0310), // USB 1
    ((0, 4, 0), 0x01C3, 0x020000), // Ethernet
    ((0, 5, 0), 0x01B0, 0x040100), // audio processing unit
    ((0, 6, 0), 0x01B1, 0x040100), // AC'97
    ((0, 9, 0), 0x01BC, 0x01018A), // IDE
    ((0, 0x1E, 0), 0x01B7, 0x060400), // AGP bridge
    ((1, 0, 0), 0x02A0, 0x030000), // NV2A
];

#[derive(Clone, Debug)]
pub struct Pci {
    pub addr: u32,
    space: BTreeMap<(u8, u8, u8), [u8; 256]>,
    /// The MCPX hides its boot ROM once 2BL writes bit 1 of LPC register 0x80.
    pub mcpx_hidden: bool,
}

impl Default for Pci {
    fn default() -> Self {
        let mut space = BTreeMap::new();
        for &(bdf, dev, class) in DEVICES {
            let mut c = [0u8; 256];
            c[0..2].copy_from_slice(&0x10DEu16.to_le_bytes());
            c[2..4].copy_from_slice(&dev.to_le_bytes());
            c[8] = 0xA1; // revision
            c[9..12].copy_from_slice(&class.to_le_bytes()[..3]);
            if bdf == (0, 0x1E, 0) {
                c[0x0E] = 1; // bridge header
                c[0x19] = 1; // secondary bus
                c[0x1A] = 1;
            }
            space.insert(bdf, c);
        }
        Pci { addr: 0, space, mcpx_hidden: false }
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
        let Some(c) = self.space.get(&bdf) else { return u32::MAX };
        let o = reg + off as usize;
        (0..size as usize).fold(0, |v, i| v | (*c.get(o + i).unwrap_or(&0xFF) as u32) << (i * 8))
    }

    pub fn write(&mut self, off: u16, size: u8, v: u32) {
        let Some((bdf, reg)) = self.target() else { return };
        let Some(c) = self.space.get_mut(&bdf) else { return };
        let o = reg + off as usize;
        for i in 0..size as usize {
            if o + i >= 4 && o + i < 256 {
                c[o + i] = (v >> (i * 8)) as u8;
            }
        }
        if bdf == (0, 1, 0) && o <= 0x80 && o + size as usize > 0x80 && (v >> ((0x80 - o) * 8)) & 2 != 0 {
            self.mcpx_hidden = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_mcpx_hide() {
        let mut p = Pci::default();
        p.addr = 0x8001_0000; // bus 1 dev 0: NV2A
        assert_eq!(p.read(0, 4), 0x02A0_10DE);
        p.addr = 0x8000_0880;
        p.write(0, 1, 2);
        assert!(p.mcpx_hidden);
        p.addr = 0x8000_F800; // bus 0 dev 31: nothing
        assert_eq!(p.read(0, 4), u32::MAX);
    }
}
