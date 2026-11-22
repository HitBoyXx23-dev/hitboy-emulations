//! Serial (I2C) EEPROM saves used by some cartridges instead of battery RAM: NBA Jam,
//! Wonder Boy in Monster World, the EA sports games, Codemasters J-Carts and others. Each
//! board wires the chip's data and clock lines to different address bits; the table below
//! maps game serials to their wiring and chip size.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    /// X24C01: 7-bit address in the first byte, no device select.
    X24C01,
    /// 24C02-24C16: device select byte (with page bits) then an 8-bit address.
    C24 { size: usize },
    /// 24C32-24C65: device select then a 16-bit address.
    C24Big { size: usize },
}

/// Where the lines are: (address, bit) for SDA in (chip -> CPU), SDA out and SCL.
#[derive(Clone, Copy, Debug)]
pub struct Wiring {
    pub sda_in: (u32, u8),
    pub sda_out: (u32, u8),
    pub scl: (u32, u8),
}

const SEGA: Wiring = Wiring { sda_in: (0x20_0001, 0), sda_out: (0x20_0001, 0), scl: (0x20_0001, 1) };
const EA: Wiring = Wiring { sda_in: (0x20_0000, 7), sda_out: (0x20_0000, 7), scl: (0x20_0000, 6) };
const ACCLAIM1: Wiring = Wiring { sda_in: (0x20_0001, 1), sda_out: (0x20_0001, 0), scl: (0x20_0001, 1) };
const ACCLAIM2: Wiring = Wiring { sda_in: (0x20_0001, 0), sda_out: (0x20_0001, 0), scl: (0x20_0000, 0) };
const CODEMASTERS: Wiring = Wiring { sda_in: (0x38_0001, 7), sda_out: (0x30_0000, 0), scl: (0x30_0000, 1) };

/// (serial substring, chip, wiring).
const GAMES: &[(&str, Chip, Wiring)] = &[
    ("T-12046", Chip::X24C01, SEGA),   // Mega Man: The Wily Wars
    ("T-12053", Chip::X24C01, SEGA),   // Rockman Mega World
    ("MK-1215", Chip::X24C01, SEGA),   // Evander Holyfield's Real Deal Boxing
    ("MK-1228", Chip::X24C01, SEGA),   // Greatest Heavyweights
    ("G-5538", Chip::X24C01, SEGA),    // Greatest Heavyweights (J)
    ("PR-1993", Chip::X24C01, SEGA),   // Greatest Heavyweights (E)
    ("G-4060", Chip::X24C01, SEGA),    // Wonder Boy in Monster World
    ("00001211", Chip::X24C01, SEGA),  // Sports Talk Baseball
    ("00004076", Chip::X24C01, SEGA),  // Honoo no Toukyuuji Dodge Danpei
    ("G-4524", Chip::X24C01, SEGA),    // Ninja Burai Densetsu
    ("00054503", Chip::X24C01, SEGA),  // Game Toshokan
    ("T-50176", Chip::X24C01, EA),     // Rings of Power
    ("T-50396", Chip::X24C01, EA),     // NHLPA Hockey '93
    ("T-50446", Chip::X24C01, EA),     // John Madden Football '93
    ("T-50516", Chip::X24C01, EA),     // John Madden Football '93 Championship Edition
    ("T-50606", Chip::X24C01, EA),     // Bill Walsh College Football
    ("T-081326", Chip::C24 { size: 256 }, ACCLAIM1), // NBA Jam
    ("T-81033", Chip::C24 { size: 256 }, ACCLAIM1),  // NBA Jam (J)
    ("T-081276", Chip::C24 { size: 256 }, ACCLAIM2), // NFL Quarterback Club
    ("T-81406", Chip::C24 { size: 512 }, ACCLAIM2),  // NBA Jam Tournament Edition
    ("T-081586", Chip::C24 { size: 2048 }, ACCLAIM2), // NFL Quarterback Club '96
    ("T-81576", Chip::C24Big { size: 8192 }, ACCLAIM2), // College Slam
    ("T-81476", Chip::C24Big { size: 8192 }, ACCLAIM2), // Frank Thomas Big Hurt Baseball
    ("T-120096", Chip::C24 { size: 1024 }, CODEMASTERS), // Micro Machines 2
    ("T-120106", Chip::C24 { size: 1024 }, CODEMASTERS), // Micro Machines Military
    ("T-120146", Chip::C24Big { size: 8192 }, CODEMASTERS), // Micro Machines '96
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, state::Stateful)]
enum State {
    Idle,
    /// Receiving a byte (bits so far).
    Recv,
    /// Sending a byte.
    Send,
}

#[derive(Clone, Debug, state::Stateful)]
pub struct Eeprom {
    #[state(skip)]
    pub chip: Chip,
    #[state(skip)]
    pub wiring: Wiring,
    pub data: Vec<u8>,
    pub dirty: bool,
    sda: bool,
    scl: bool,
    out: bool,
    state: State,
    shift: u8,
    bits: u8,
    /// Bytes received in this transfer (device select, address...).
    stage: u8,
    reading: bool,
    addr: usize,
    ack_pending: bool,
}

impl Eeprom {
    /// The EEPROM for a cartridge, from its header serial number.
    pub fn for_rom(rom: &[u8]) -> Option<Eeprom> {
        let serial = String::from_utf8_lossy(rom.get(0x180..0x18E)?).into_owned();
        let &(_, chip, wiring) = GAMES.iter().find(|(s, _, _)| serial.contains(s))?;
        let size = match chip {
            Chip::X24C01 => 128,
            Chip::C24 { size } | Chip::C24Big { size } => size,
        };
        Some(Eeprom { chip, wiring, data: vec![0xFF; size], dirty: false, sda: true, scl: true, out: true, state: State::Idle, shift: 0, bits: 0, stage: 0, reading: false, addr: 0, ack_pending: false })
    }

    /// Whether the CPU address touches this board's EEPROM lines.
    pub fn claims(&self, a: u32) -> bool {
        let w = &self.wiring;
        a == w.sda_in.0 || a == w.sda_out.0 || a == w.scl.0
    }

    pub fn read(&self, a: u32) -> u8 {
        if a == self.wiring.sda_in.0 {
            (self.out as u8) << self.wiring.sda_in.1
        } else {
            0
        }
    }

    pub fn write(&mut self, a: u32, v: u8) {
        let mut sda = self.sda;
        let mut scl = self.scl;
        if a == self.wiring.sda_out.0 {
            sda = v >> self.wiring.sda_out.1 & 1 != 0;
        }
        if a == self.wiring.scl.0 {
            scl = v >> self.wiring.scl.1 & 1 != 0;
        }
        self.lines(sda, scl);
    }

    fn lines(&mut self, sda: bool, scl: bool) {
        let (old_sda, old_scl) = (self.sda, self.scl);
        self.sda = sda;
        self.scl = scl;
        if old_scl && scl && old_sda != sda {
            if !sda {
                // START
                self.state = State::Recv;
                self.bits = 0;
                self.shift = 0;
                self.stage = 0;
                self.ack_pending = false;
            } else {
                // STOP
                self.state = State::Idle;
            }
            return;
        }
        if !old_scl && scl {
            self.rising();
        } else if old_scl && !scl {
            self.falling();
        }
    }

    fn size(&self) -> usize {
        self.data.len()
    }

    /// SCL rising edge: the chip samples SDA.
    fn rising(&mut self) {
        match self.state {
            State::Recv if !self.ack_pending => {
                self.shift = self.shift << 1 | self.sda as u8;
                self.bits += 1;
            }
            State::Send if !self.ack_pending => {}
            State::Send => {
                // Host ACK (SDA low) continues the read; NACK ends it.
                if self.sda {
                    self.state = State::Idle;
                }
            }
            _ => {}
        }
    }

    /// SCL falling edge: the chip drives SDA for the next bit.
    fn falling(&mut self) {
        match self.state {
            State::Recv => {
                if self.ack_pending {
                    self.ack_pending = false;
                    self.out = true;
                    if self.reading {
                        self.state = State::Send;
                        self.bits = 0;
                        self.out = self.data[self.addr % self.size()] & 0x80 != 0;
                    }
                    return;
                }
                if self.bits == 8 {
                    self.byte(self.shift);
                    self.bits = 0;
                    self.shift = 0;
                    self.ack_pending = true;
                    self.out = false; // ACK
                }
            }
            State::Send => {
                if self.ack_pending {
                    // After the host's ACK: next byte.
                    self.ack_pending = false;
                    self.addr = (self.addr + 1) % self.size();
                    self.bits = 0;
                    self.out = self.data[self.addr] & 0x80 != 0;
                    return;
                }
                self.bits += 1;
                if self.bits == 8 {
                    self.ack_pending = true;
                    self.out = true; // release SDA for the host's ACK
                } else {
                    self.out = self.data[self.addr % self.size()] << self.bits & 0x80 != 0;
                }
            }
            State::Idle => self.out = true,
        }
    }

    fn byte(&mut self, b: u8) {
        let n = self.size();
        match self.chip {
            Chip::X24C01 => {
                if self.stage == 0 {
                    self.addr = (b >> 1) as usize % n;
                    self.reading = b & 1 != 0;
                } else {
                    self.store(b);
                }
            }
            Chip::C24 { .. } => match self.stage {
                0 => {
                    self.reading = b & 1 != 0;
                    // Page bits of the device select extend the address.
                    self.addr = (self.addr & 0xFF) | (((b >> 1) & 7) as usize) << 8;
                    self.addr %= n;
                }
                1 if !self.reading => self.addr = ((self.addr & !0xFF) | b as usize) % n,
                _ => self.store(b),
            },
            Chip::C24Big { .. } => match self.stage {
                0 => self.reading = b & 1 != 0,
                1 if !self.reading => self.addr = ((b as usize) << 8) % n,
                2 if !self.reading => self.addr = ((self.addr & !0xFF) | b as usize) % n,
                _ => self.store(b),
            },
        }
        self.stage = self.stage.saturating_add(1);
    }

    fn store(&mut self, b: u8) {
        let n = self.size();
        self.data[self.addr % n] = b;
        self.dirty = true;
        // Page writes wrap within 4/8/16 bytes; incrementing is close enough for games.
        self.addr = (self.addr + 1) % n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bit_out(e: &mut Eeprom, b: bool) {
        let w = e.wiring;
        let v = |sda: bool, scl: bool| -> [(u32, u8); 2] { [(w.sda_out.0, (sda as u8) << w.sda_out.1 | if w.scl.0 == w.sda_out.0 { (scl as u8) << w.scl.1 } else { 0 }), (w.scl.0, (scl as u8) << w.scl.1 | if w.scl.0 == w.sda_out.0 { (sda as u8) << w.sda_out.1 } else { 0 })] };
        for (a, x) in v(b, false) {
            e.write(a, x);
        }
        for (a, x) in v(b, true) {
            e.write(a, x);
        }
        for (a, x) in v(b, false) {
            e.write(a, x);
        }
    }

    fn start(e: &mut Eeprom) {
        e.lines(true, true);
        e.lines(false, true);
        e.lines(false, false);
    }
    fn stop(e: &mut Eeprom) {
        e.lines(false, false);
        e.lines(false, true);
        e.lines(true, true);
    }
    fn send(e: &mut Eeprom, byte: u8) {
        for i in (0..8).rev() {
            let b = byte >> i & 1 != 0;
            e.lines(b, false);
            e.lines(b, true);
            e.lines(b, false);
        }
        // ACK clock.
        e.lines(true, true);
        e.lines(true, false);
    }
    fn recv(e: &mut Eeprom) -> u8 {
        let mut v = 0;
        for _ in 0..8 {
            e.lines(true, true);
            v = v << 1 | e.out as u8;
            e.lines(true, false);
        }
        // NACK.
        e.lines(true, true);
        e.lines(true, false);
        v
    }

    #[test]
    fn x24c01_write_then_read() {
        let mut rom = vec![0u8; 0x200];
        rom[0x180..0x18E].copy_from_slice(b"GM G-4060 -00 ");
        let mut e = Eeprom::for_rom(&rom).unwrap();
        let _ = bit_out;
        start(&mut e);
        send(&mut e, 5 << 1); // address 5, write
        send(&mut e, 0xA7);
        stop(&mut e);
        assert_eq!(e.data[5], 0xA7);
        start(&mut e);
        send(&mut e, 5 << 1 | 1); // address 5, read
        assert_eq!(recv(&mut e), 0xA7);
        stop(&mut e);
    }
}
