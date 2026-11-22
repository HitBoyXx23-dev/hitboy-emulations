//! The SIF: the registers the two processors use to talk to each other.
//!
//! The Emotion Engine sees them at 0x1000F2xx and the I/O processor at 0x1D0000xx. Each side
//! has a command word it writes and the other reads, and a flag word where the two sides set
//! and clear bits in opposite ways (one side's write sets bits; the other side's clears them).
//! The BIOS uses these to hand work back and forth and to wait for the other processor to
//! finish starting up.

#[derive(Default, Debug, Clone)]
pub struct Sif {
    /// Main -> sub command word (EE writes, IOP reads).
    pub mscom: u32,
    /// Sub -> main command word (IOP writes, EE reads).
    pub smcom: u32,
    /// Main flags: EE writes set bits, IOP writes clear them.
    pub msflg: u32,
    /// Sub flags: IOP writes set bits, EE writes clear them.
    pub smflg: u32,
    pub ctrl: u32,
    pub bd6: u32,
}

impl Sif {
    // ---- Emotion Engine side (0x1000F200..)
    pub fn ee_read(&self, addr: u32) -> Option<u32> {
        Some(match addr & 0xFF0 {
            0x200 => self.mscom,
            0x210 => self.smcom,
            0x220 => self.msflg,
            0x230 => self.smflg,
            0x240 => self.ctrl,
            0x260 => self.bd6,
            _ => return None,
        })
    }

    pub fn ee_write(&mut self, addr: u32, v: u32) -> bool {
        match addr & 0xFF0 {
            0x200 => self.mscom = v,
            0x210 => {} // read-only for the EE
            0x220 => self.msflg |= v,
            0x230 => self.smflg &= !v,
            0x240 => self.ctrl = v,
            0x260 => self.bd6 = v,
            _ => return false,
        }
        true
    }

    // ---- I/O processor side (0x1D0000..)
    pub fn iop_read(&mut self, addr: u32) -> u32 {
        match addr & 0xFF {
            0x00 => self.mscom,
            0x10 => self.smcom,
            0x20 => self.msflg,
            0x30 => self.smflg,
            0x40 => self.ctrl,
            0x60 => self.bd6,
            _ => 0,
        }
    }

    pub fn iop_write(&mut self, addr: u32, v: u32) {
        match addr & 0xFF {
            0x00 => {} // read-only for the IOP
            0x10 => self.smcom = v,
            0x20 => self.msflg &= !v,
            0x30 => self.smflg |= v,
            0x40 => self.ctrl = v,
            0x60 => self.bd6 = v,
            _ => {}
        }
    }
}
