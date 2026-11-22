//! NV2A graphics chip (a GeForce 3 relative) at MMIO 0xFD000000. It shares the Xbox's main
//! RAM (unified memory). This first layer is the register file the kernel programs while
//! booting: chip ID, memory config, timer, CRTC/RAMDAC video mode, vblank interrupt, and
//! scan-out of the framebuffer. The 3D engine (PFIFO / PGRAPH) sits behind these.

use std::collections::HashMap;

pub const PMC_BOOT_0: u32 = 0x00_0000;
pub const PMC_INTR_0: u32 = 0x00_0100;
pub const PMC_INTR_EN_0: u32 = 0x00_0140;
pub const PTIMER_INTR_0: u32 = 0x00_9100;
pub const PTIMER_TIME_0: u32 = 0x00_9400;
pub const PTIMER_TIME_1: u32 = 0x00_9410;
pub const PFB_CFG0: u32 = 0x10_0200;
pub const PFB_CSTATUS: u32 = 0x10_020C;
pub const PCRTC_INTR_0: u32 = 0x60_0100;
pub const PCRTC_INTR_EN_0: u32 = 0x60_0140;
pub const PCRTC_START: u32 = 0x60_0800;
pub const PRMCIO_CRX: u32 = 0x60_13D4;
pub const PRMCIO_CR: u32 = 0x60_13D5;
pub const PRAMDAC_GENERAL_CONTROL: u32 = 0x68_0600;

#[derive(Clone, Debug)]
pub struct Nv2a {
    regs: HashMap<u32, u32>,
    pub crtc: [u8; 256],
    crtc_index: u8,
    pub ram_size: u32,
}

impl Nv2a {
    pub fn new(ram_size: u32) -> Self {
        Nv2a { regs: HashMap::new(), crtc: [0; 256], crtc_index: 0, ram_size }
    }

    fn reg(&self, a: u32) -> u32 {
        self.regs.get(&a).copied().unwrap_or(0)
    }

    /// 32-bit register read. `ns` is the machine time in nanoseconds (for PTIMER).
    pub fn read32(&mut self, a: u32, ns: u64) -> u32 {
        match a {
            PMC_BOOT_0 => 0x02A0_00A1,
            PMC_INTR_0 => {
                let mut v = 0;
                if self.reg(PCRTC_INTR_0) & self.reg(PCRTC_INTR_EN_0) != 0 {
                    v |= 1 << 24;
                }
                if self.reg(PTIMER_INTR_0) != 0 {
                    v |= 1 << 20;
                }
                v
            }
            PTIMER_TIME_0 => (ns as u32) & !0x1F,
            PTIMER_TIME_1 => (ns >> 32) as u32,
            PFB_CFG0 => 0x0307_0103,
            PFB_CSTATUS => self.ram_size,
            _ if a & !3 == PRMCIO_CRX & !3 => {
                // CRX (index) at byte 0x13D4, CR (data) at 0x13D5, packed in one word.
                let v = self.crtc_index as u32 | (self.crtc[self.crtc_index as usize] as u32) << 8;
                v << ((PRMCIO_CRX & 3) * 8)
            }
            _ => self.reg(a),
        }
    }

    pub fn write32(&mut self, a: u32, v: u32) {
        match a {
            // Interrupt status registers: write 1 to clear.
            PCRTC_INTR_0 | PTIMER_INTR_0 => {
                let r = self.regs.entry(a).or_insert(0);
                *r &= !v;
            }
            _ => {
                self.regs.insert(a, v);
            }
        }
    }

    /// Byte access, needed for the VGA-style CRTC index/data ports.
    pub fn write8(&mut self, a: u32, v: u8) {
        match a {
            PRMCIO_CRX => self.crtc_index = v,
            PRMCIO_CR => self.crtc[self.crtc_index as usize] = v,
            _ => {
                let w = a & !3;
                let sh = (a & 3) * 8;
                let old = self.reg(w);
                self.write32(w, (old & !(0xFF << sh)) | (v as u32) << sh);
            }
        }
    }
    pub fn read8(&mut self, a: u32, ns: u64) -> u8 {
        match a {
            PRMCIO_CRX => self.crtc_index,
            PRMCIO_CR => self.crtc[self.crtc_index as usize],
            _ => (self.read32(a & !3, ns) >> ((a & 3) * 8)) as u8,
        }
    }

    /// Called at each vertical blank; returns true if the chip now requests its interrupt.
    pub fn vblank(&mut self) -> bool {
        *self.regs.entry(PCRTC_INTR_0).or_insert(0) |= 1;
        self.irq_pending()
    }

    pub fn irq_pending(&self) -> bool {
        self.reg(PMC_INTR_EN_0) & 1 != 0 && self.reg(PCRTC_INTR_0) & self.reg(PCRTC_INTR_EN_0) != 0
    }

    /// Scan-out: (RAM offset, pitch in bytes, width, height, bytes per pixel).
    pub fn scanout(&self) -> (u32, u32, u32, u32, u32) {
        let start = self.reg(PCRTC_START) & (self.ram_size - 1);
        // VGA CRTC: horizontal display end (0x01), vertical display end (0x12 + overflow
        // bits in 0x07), offset (0x13 + NV extension bits in 0x19).
        let c = &self.crtc;
        let width = (c[0x01] as u32 + 1) * 8;
        let height = (c[0x12] as u32 | (c[0x07] as u32 & 2) << 7 | (c[0x07] as u32 & 0x40) << 3 | (c[0x25] as u32 & 2) << 9) + 1;
        let offset = (c[0x13] as u32 | ((c[0x19] as u32 >> 5) & 7) << 8) * 8;
        // NV "pixel format" CR28: 1 = 8bpp, 2 = 16bpp, 3 = 32bpp.
        let bpp = match c[0x28] & 3 {
            2 => 2,
            1 => 1,
            _ => 4,
        };
        if width <= 8 || height <= 1 {
            // Not programmed yet: the kernel's default 640x480 32bpp mode.
            return (start, 640 * 4, 640, 480, 4);
        }
        (start, if offset == 0 { width * bpp } else { offset }, width, height, bpp)
    }
}
