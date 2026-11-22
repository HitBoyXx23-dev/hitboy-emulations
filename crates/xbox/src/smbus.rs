//! The MCPX SMBus controller (I/O 0xC000) and what hangs off it: the system management
//! controller (PIC16LC: version, tray, AV pack, LEDs, power), the 256-byte EEPROM, the
//! video encoder and the temperature sensor.

pub const SMC: u8 = 0x10;
pub const EEPROM: u8 = 0x54;
pub const ENCODER: u8 = 0x45;
pub const TEMP: u8 = 0x4C;

/// AV pack reported to the kernel (0x06 = standard composite/RCA).
pub const AV_COMPOSITE: u8 = 0x06;

#[derive(Clone, Debug)]
pub struct SmBus {
    status: u8,
    control: u8,
    addr: u8,
    data: u16,
    cmd: u8,
    pub eeprom: [u8; 256],
    encoder: [u8; 256],
    smc_ver_idx: usize,
    smc_scratch: u8,
    pub tray_open: bool,
    /// Last request written to the SMC power register (1 = reset, 0x40 = cycle, 0x80 = off).
    pub power_request: Option<u8>,
    pub led: u8,
    /// Raise the SMBus interrupt (IRQ 11) when a cycle completes.
    pub irq: bool,
}

impl Default for SmBus {
    fn default() -> Self {
        SmBus {
            status: 0,
            control: 0,
            addr: 0,
            data: 0,
            cmd: 0,
            eeprom: [0; 256],
            encoder: [0; 256],
            smc_ver_idx: 0,
            smc_scratch: 0,
            tray_open: false,
            power_request: None,
            led: 0,
            irq: false,
        }
    }
}

const STS_ABORT: u8 = 0x01;
const STS_PROTO_ERR: u8 = 0x04;
const STS_DONE: u8 = 0x10;

impl SmBus {
    pub fn read(&mut self, off: u16) -> u8 {
        match off {
            0 => self.status,
            2 => self.control & !0x08,
            4 => self.addr,
            6 => self.data as u8,
            7 => (self.data >> 8) as u8,
            8 => self.cmd,
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u16, v: u8) {
        match off {
            0 => self.status &= !v, // write 1 to clear
            2 => {
                self.control = v;
                if v & 0x08 != 0 {
                    self.cycle();
                }
            }
            4 => self.addr = v,
            6 => self.data = (self.data & 0xFF00) | v as u16,
            7 => self.data = (self.data & 0xFF) | (v as u16) << 8,
            8 => self.cmd = v,
            _ => {}
        }
    }

    fn cycle(&mut self) {
        let dev = self.addr >> 1;
        let read = self.addr & 1 != 0;
        let word = self.control & 7 == 3;
        let ok = if read {
            match self.device_read(dev, self.cmd, word) {
                Some(v) => {
                    self.data = v;
                    true
                }
                None => false,
            }
        } else {
            self.device_write(dev, self.cmd, self.data, word)
        };
        self.status |= if ok { STS_DONE } else { STS_PROTO_ERR | STS_ABORT };
        if self.control & 0x10 != 0 {
            self.irq = true;
        }
    }

    fn device_read(&mut self, dev: u8, cmd: u8, word: bool) -> Option<u16> {
        Some(match dev {
            SMC => self.smc_read(cmd) as u16,
            EEPROM => {
                let lo = self.eeprom[cmd as usize] as u16;
                if word {
                    lo | (self.eeprom[cmd.wrapping_add(1) as usize] as u16) << 8
                } else {
                    lo
                }
            }
            ENCODER => self.encoder[cmd as usize] as u16,
            TEMP => 40,
            _ => return None,
        })
    }

    fn device_write(&mut self, dev: u8, cmd: u8, v: u16, word: bool) -> bool {
        match dev {
            SMC => self.smc_write(cmd, v as u8),
            EEPROM => {
                self.eeprom[cmd as usize] = v as u8;
                if word {
                    self.eeprom[cmd.wrapping_add(1) as usize] = (v >> 8) as u8;
                }
            }
            ENCODER => self.encoder[cmd as usize] = v as u8,
            TEMP => {}
            _ => return false,
        }
        true
    }

    fn smc_read(&mut self, reg: u8) -> u8 {
        match reg {
            // Version string, one character per read ("P01" on retail 1.0 boards).
            0x01 => {
                let c = b"P01"[self.smc_ver_idx % 3];
                self.smc_ver_idx += 1;
                c
            }
            // Tray state: 0x10 closed with nothing, 0x60 closed with media, 0x01 open.
            0x03 => {
                if self.tray_open {
                    0x01
                } else {
                    0x60
                }
            }
            0x04 => AV_COMPOSITE,
            0x09 => 40, // CPU temperature, °C
            0x0A => 35, // board temperature
            0x0F => self.smc_scratch,
            0x11 => 0, // interrupt reason
            0x1B => self.smc_scratch,
            _ => 0,
        }
    }

    fn smc_write(&mut self, reg: u8, v: u8) {
        match reg {
            0x01 => self.smc_ver_idx = 0,
            0x02 => self.power_request = Some(v),
            0x08 | 0x07 => self.led = v,
            0x0C => self.tray_open = v == 0,
            0x1B => self.smc_scratch = v,
            _ => {}
        }
    }

    pub fn take_irq(&mut self) -> bool {
        std::mem::take(&mut self.irq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xfer(s: &mut SmBus, dev: u8, read: bool, cmd: u8, data: u16, word: bool) -> (u8, u16) {
        s.write(0, 0xFF);
        s.write(4, dev << 1 | read as u8);
        s.write(8, cmd);
        s.write(6, data as u8);
        s.write(7, (data >> 8) as u8);
        s.write(2, if word { 0x0B } else { 0x0A });
        (s.read(0), s.read(6) as u16 | (s.read(7) as u16) << 8)
    }

    #[test]
    fn smc_version_and_eeprom() {
        let mut s = SmBus::default();
        let v: Vec<u8> = (0..3).map(|_| xfer(&mut s, SMC, true, 0x01, 0, false).1 as u8).collect();
        assert_eq!(&v, b"P01");
        xfer(&mut s, EEPROM, false, 0x10, 0xBEEF, true);
        let (st, d) = xfer(&mut s, EEPROM, true, 0x10, 0, true);
        assert_eq!(st & STS_DONE, STS_DONE);
        assert_eq!(d, 0xBEEF);
        let (st, _) = xfer(&mut s, 0x33, true, 0, 0, false);
        assert_ne!(st & STS_PROTO_ERR, 0);
    }
}
