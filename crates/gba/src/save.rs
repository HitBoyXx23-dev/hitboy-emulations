//! Cartridge backup memory: SRAM, Flash (64/128 KB) and serial EEPROM, detected from the
//! library tags that Nintendo's SDK embeds in every ROM.


#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Sram,
    Flash64,
    Flash128,
    Eeprom,
}

pub fn detect(rom: &[u8]) -> Kind {
    let has = |tag: &[u8]| rom.windows(tag.len()).any(|w| w == tag);
    if has(b"EEPROM_V") {
        Kind::Eeprom
    } else if has(b"FLASH1M_V") {
        Kind::Flash128
    } else if has(b"FLASH_V") || has(b"FLASH512_V") {
        Kind::Flash64
    } else {
        Kind::Sram
    }
}

impl state::Stateful for Ee {
    fn sync(&mut self, s: &mut state::State) {
        let (mut tag, mut flag, mut pos): (u8, bool, u32) = match self {
            Ee::Idle => (0, false, 0),
            Ee::Addr { write } => (1, *write, 0),
            Ee::Data => (2, false, 0),
            Ee::End { write } => (3, *write, 0),
            Ee::Out { pos } => (4, false, *pos),
        };
        tag.sync(s);
        flag.sync(s);
        pos.sync(s);
        if s.is_load() {
            *self = match tag {
                0 => Ee::Idle,
                1 => Ee::Addr { write: flag },
                2 => Ee::Data,
                3 => Ee::End { write: flag },
                4 => Ee::Out { pos },
                _ => {
                    s.fail();
                    return;
                }
            };
        }
    }
}

#[derive(PartialEq)]
enum Ee {
    Idle,
    Addr { write: bool },
    Data,
    End { write: bool },
    Out { pos: u32 },
}

#[derive(state::Stateful)]
pub struct Save {
    #[state(skip)]
    pub kind: Kind,
    pub data: Vec<u8>,
    pub dirty: bool,
    // Flash command state
    step: u8,
    id_mode: bool,
    erase_armed: bool,
    write_next: bool,
    bank_next: bool,
    bank: usize,
    // EEPROM serial state
    ee: Ee,
    ee_bits: u32,
    ee_count: u32,
    ee_addr: usize,
    ee_data: u64,
    #[state(skip)]
    pub ee_addr_bits: u32,
}

impl Save {
    pub fn new(kind: Kind, rom_len: usize) -> Save {
        let size = match kind {
            Kind::Sram => 0x8000,
            Kind::Flash64 => 0x10000,
            Kind::Flash128 => 0x20000,
            Kind::Eeprom => 0x2000,
        };
        Save {
            kind,
            data: vec![0xFF; size],
            dirty: false,
            step: 0,
            id_mode: false,
            erase_armed: false,
            write_next: false,
            bank_next: false,
            bank: 0,
            ee: Ee::Idle,
            ee_bits: 0,
            ee_count: 0,
            ee_addr: 0,
            ee_data: 0,
            ee_addr_bits: if rom_len > 16 * 1024 * 1024 { 14 } else { 6 },
        }
    }

    pub fn load(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.data.len());
        self.data[..n].copy_from_slice(&bytes[..n]);
        if self.kind == Kind::Eeprom && bytes.len() > 512 {
            self.ee_addr_bits = 14;
        }
    }

    // ---------------------------------------------------------------- SRAM / Flash (8-bit bus)

    pub fn read8(&self, off: u32) -> u8 {
        let off = off as usize & 0xFFFF;
        match self.kind {
            Kind::Sram => self.data[off & 0x7FFF],
            Kind::Flash64 | Kind::Flash128 => {
                if self.id_mode && off < 2 {
                    let id: [u8; 2] = if self.kind == Kind::Flash128 { [0x62, 0x13] } else { [0x32, 0x1B] };
                    id[off]
                } else {
                    self.data[self.bank * 0x10000 + off]
                }
            }
            Kind::Eeprom => 0xFF,
        }
    }

    pub fn write8(&mut self, off: u32, v: u8) {
        let off = off as usize & 0xFFFF;
        match self.kind {
            Kind::Sram => {
                self.data[off & 0x7FFF] = v;
                self.dirty = true;
            }
            Kind::Flash64 | Kind::Flash128 => self.flash_write(off, v),
            Kind::Eeprom => {}
        }
    }

    fn flash_write(&mut self, off: usize, v: u8) {
        if self.write_next {
            self.write_next = false;
            self.data[self.bank * 0x10000 + off] &= v;
            self.dirty = true;
            return;
        }
        if self.bank_next && off == 0 {
            self.bank_next = false;
            if self.kind == Kind::Flash128 {
                self.bank = (v & 1) as usize;
            }
            return;
        }
        match (self.step, off, v) {
            (0, 0x5555, 0xAA) => self.step = 1,
            (1, 0x2AAA, 0x55) => self.step = 2,
            (2, 0x5555, cmd) => {
                self.step = 0;
                match cmd {
                    0x90 => self.id_mode = true,
                    0xF0 => self.id_mode = false,
                    0x80 => self.erase_armed = true,
                    0x10 if self.erase_armed => {
                        self.data.fill(0xFF);
                        self.erase_armed = false;
                        self.dirty = true;
                    }
                    0xA0 => self.write_next = true,
                    0xB0 => self.bank_next = true,
                    _ => {}
                }
            }
            (2, sector, 0x30) if self.erase_armed => {
                self.step = 0;
                let s = self.bank * 0x10000 + (sector & 0xF000);
                self.data[s..s + 0x1000].fill(0xFF);
                self.erase_armed = false;
                self.dirty = true;
            }
            (_, _, 0xF0) => {
                self.step = 0;
                self.id_mode = false;
            }
            _ => self.step = 0,
        }
    }

    // ---------------------------------------------------------------- EEPROM (serial, via DMA)

    pub fn ee_read(&mut self) -> u16 {
        if let Ee::Out { pos } = self.ee {
            let bit = if pos < 4 {
                0
            } else {
                let i = pos - 4;
                let byte = self.data[(self.ee_addr * 8 + i as usize / 8) & (self.data.len() - 1)];
                (byte >> (7 - i % 8)) & 1
            };
            self.ee = if pos + 1 >= 68 { Ee::Idle } else { Ee::Out { pos: pos + 1 } };
            return bit as u16;
        }
        1
    }

    pub fn ee_write(&mut self, v: u16) {
        let bit = (v & 1) as u32;
        match self.ee {
            Ee::Idle | Ee::Out { .. } => {
                if self.ee != Ee::Idle {
                    self.ee = Ee::Idle;
                }
                self.ee_bits = (self.ee_bits << 1) | bit;
                self.ee_count += 1;
                if self.ee_count == 2 {
                    let cmd = self.ee_bits & 3;
                    self.ee_count = 0;
                    self.ee_bits = 0;
                    if cmd & 2 != 0 {
                        self.ee = Ee::Addr { write: cmd == 2 };
                    }
                }
            }
            Ee::Addr { write } => {
                self.ee_bits = (self.ee_bits << 1) | bit;
                self.ee_count += 1;
                if self.ee_count == self.ee_addr_bits {
                    self.ee_addr = (self.ee_bits & 0x3FF) as usize;
                    self.ee_count = 0;
                    self.ee_bits = 0;
                    self.ee_data = 0;
                    self.ee = if write { Ee::Data } else { Ee::End { write: false } };
                }
            }
            Ee::Data => {
                self.ee_data = (self.ee_data << 1) | bit as u64;
                self.ee_count += 1;
                if self.ee_count == 64 {
                    self.ee_count = 0;
                    self.ee = Ee::End { write: true };
                }
            }
            Ee::End { write } => {
                if write {
                    let a = (self.ee_addr * 8) & (self.data.len() - 1);
                    self.data[a..a + 8].copy_from_slice(&self.ee_data.to_be_bytes());
                    self.dirty = true;
                    self.ee = Ee::Idle;
                } else {
                    self.ee = Ee::Out { pos: 0 };
                }
            }
        }
    }
}
