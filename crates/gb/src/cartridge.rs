//! Game Boy cartridges: ROM-only, MBC1, MBC2, MBC3 with its real-time clock, MBC5 (with
//! rumble), HuC1 and HuC3.

fn now_secs() -> i64 {
    state::wall_clock_secs()
}

#[derive(state::Stateful)]
pub struct Cartridge {
    #[state(skip)]
    rom: Vec<u8>,
    pub ram: Vec<u8>,
    #[state(skip)]
    kind: Mbc,
    #[state(skip)]
    pub title: String,
    #[state(skip)]
    pub has_battery: bool,
    #[state(skip)]
    pub cgb_only: bool,
    /// Cartridge supports Game Boy Color features (header byte 0x143 bit 7).
    #[state(skip)]
    pub cgb: bool,
    ram_enabled: bool,
    rom_bank: usize,
    ram_bank: usize,
    mbc1_mode: bool,
    /// MBC3 real-time clock present.
    #[state(skip)]
    pub rtc: bool,
    /// Host time (Unix seconds) at which the clock read zero.
    #[state(skip)]
    pub rtc_base: i64,
    /// Clock value while halted.
    #[state(skip)]
    pub rtc_halted: Option<i64>,
    /// Day counter overflowed past 511.
    #[state(skip)]
    rtc_carry: bool,
    rtc_latched: [u8; 5],
    rtc_latch_prev: u8,
    /// MBC5 rumble cartridges use RAM-bank bit 3 for the motor.
    #[state(skip)]
    rumble: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mbc {
    None,
    Mbc1,
    Mbc2,
    Mbc3,
    Mbc5,
    HuC1,
}

impl Cartridge {
    pub fn load(rom: Vec<u8>) -> Result<Self, String> {
        if rom.len() < 0x150 {
            return Err("File is too small to be a Game Boy ROM.".into());
        }
        let title: String = rom[0x134..0x143]
            .iter()
            .take_while(|&&b| b != 0)
            .filter(|b| b.is_ascii_graphic() || **b == b' ')
            .map(|&b| b as char)
            .collect();
        let (kind, battery) = match rom[0x147] {
            0x00 | 0x08 | 0x09 => (Mbc::None, rom[0x147] == 0x09),
            0x01 | 0x02 => (Mbc::Mbc1, false),
            0x03 => (Mbc::Mbc1, true),
            0x05 => (Mbc::Mbc2, false),
            0x06 => (Mbc::Mbc2, true),
            0x0F | 0x10 | 0x13 => (Mbc::Mbc3, true),
            0x11 | 0x12 => (Mbc::Mbc3, false),
            0x19 | 0x1A | 0x1C | 0x1D => (Mbc::Mbc5, false),
            0x1B | 0x1E => (Mbc::Mbc5, true),
            // HuC1 (infrared port ignored) and HuC3 (clock ignored) bank like MBC1/MBC3.
            0xFF => (Mbc::HuC1, true),
            0xFE => (Mbc::Mbc3, true),
            // MMM01 multicarts, MBC6/MBC7 and the Pocket Camera: run their menu bank as MBC5.
            0x0B..=0x0D | 0x20 | 0x22 | 0xFC | 0xFD => (Mbc::Mbc5, true),
            t => return Err(format!("Cartridge type 0x{t:02X} isn't supported yet.")),
        };
        let ram_size = match (kind, rom[0x149]) {
            (Mbc::Mbc2, _) => 512,
            (_, 0x02) => 0x2000,
            (_, 0x03) => 0x8000,
            (_, 0x04) => 0x20000,
            (_, 0x05) => 0x10000,
            _ => 0x2000,
        };
        let cart_type = rom[0x147];
        Ok(Self {
            cgb_only: rom[0x143] == 0xC0,
            cgb: rom[0x143] & 0x80 != 0,
            rom,
            ram: vec![0; ram_size],
            kind,
            title,
            has_battery: battery,
            ram_enabled: false,
            rom_bank: 1,
            ram_bank: 0,
            mbc1_mode: false,
            rtc: matches!(cart_type, 0x0F | 0x10),
            rtc_base: now_secs(),
            rtc_halted: None,
            rtc_carry: false,
            rtc_latched: [0; 5],
            rtc_latch_prev: 0xFF,
            rumble: matches!(cart_type, 0x1C..=0x1E),
        })
    }

    /// RAM bank in use (MBC1 in ROM-banking mode always uses bank 0; rumble carts'
    /// bit 3 drives the motor).
    fn ram_bank(&self) -> usize {
        match self.kind {
            Mbc::Mbc1 if !self.mbc1_mode => 0,
            Mbc::Mbc5 if self.rumble => self.ram_bank & 7,
            _ => self.ram_bank,
        }
    }

    /// Current clock in seconds.
    fn rtc_now(&self) -> i64 {
        self.rtc_halted.unwrap_or_else(|| now_secs() - self.rtc_base).max(0)
    }

    /// Seconds, minutes, hours, day low, day high/halt/carry.
    fn rtc_regs(&mut self) -> [u8; 5] {
        let c = self.rtc_now();
        let days = c / 86_400;
        if days > 511 {
            self.rtc_carry = true;
        }
        let d = days & 0x1FF;
        [(c % 60) as u8, (c / 60 % 60) as u8, (c / 3600 % 24) as u8, d as u8, (d >> 8) as u8 | (self.rtc_halted.is_some() as u8) << 6 | (self.rtc_carry as u8) << 7]
    }

    fn rtc_write(&mut self, reg: usize, v: u8) {
        let mut r = self.rtc_regs();
        r[reg] = v;
        self.rtc_carry = r[4] & 0x80 != 0;
        let c = r[0].min(59) as i64 + r[1].min(59) as i64 * 60 + r[2].min(23) as i64 * 3600 + ((r[3] as i64) | ((r[4] as i64 & 1) << 8)) * 86_400;
        if r[4] & 0x40 != 0 {
            self.rtc_halted = Some(c);
        } else {
            self.rtc_halted = None;
            self.rtc_base = now_secs() - c;
        }
    }

    pub fn rom_len(&self) -> usize {
        self.rom.len()
    }

    fn rom_banks(&self) -> usize {
        (self.rom.len() / 0x4000).max(1)
    }

    #[inline]
    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x3FFF => {
                let bank = if self.kind == Mbc::Mbc1 && self.mbc1_mode { (self.ram_bank << 5) % self.rom_banks() } else { 0 };
                self.rom[bank * 0x4000 + addr as usize]
            }
            0x4000..=0x7FFF => {
                let bank = self.rom_bank % self.rom_banks();
                self.rom[bank * 0x4000 + (addr as usize - 0x4000)]
            }
            0xA000..=0xBFFF => {
                if !self.ram_enabled || self.ram.is_empty() {
                    return 0xFF;
                }
                if self.kind == Mbc::Mbc2 {
                    return self.ram[(addr as usize - 0xA000) & 0x1FF] | 0xF0;
                }
                if self.kind == Mbc::Mbc3 && (0x08..=0x0C).contains(&self.ram_bank) {
                    return self.rtc_latched[self.ram_bank - 8];
                }
                let i = (self.ram_bank() * 0x2000 + (addr as usize - 0xA000)) % self.ram.len();
                self.ram[i]
            }
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, v: u8) {
        match (self.kind, addr) {
            (Mbc::None, _) if (0xA000..=0xBFFF).contains(&addr) => {
                let i = (addr as usize - 0xA000) % self.ram.len();
                self.ram[i] = v;
            }
            (Mbc::None, _) => {}
            (Mbc::Mbc2, 0x0000..=0x3FFF) => {
                if addr & 0x100 == 0 {
                    self.ram_enabled = v & 0x0F == 0x0A;
                } else {
                    self.rom_bank = ((v & 0x0F) as usize).max(1);
                }
            }
            (Mbc::HuC1, 0x0000..=0x1FFF) => self.ram_enabled = v != 0x0E,
            (_, 0x0000..=0x1FFF) => self.ram_enabled = v & 0x0F == 0x0A,
            (Mbc::Mbc1, 0x2000..=0x3FFF) => {
                let lo = ((v & 0x1F) as usize).max(1);
                self.rom_bank = (self.rom_bank & !0x1F) | lo;
            }
            (Mbc::Mbc3, 0x2000..=0x3FFF) => self.rom_bank = ((v & 0x7F) as usize).max(1),
            (Mbc::HuC1, 0x2000..=0x3FFF) => self.rom_bank = ((v & 0x3F) as usize).max(1),
            (Mbc::HuC1, 0x4000..=0x5FFF) => self.ram_bank = (v & 3) as usize,
            (Mbc::Mbc3, 0x6000..=0x7FFF) => {
                if self.rtc_latch_prev == 0 && v == 1 {
                    self.rtc_latched = self.rtc_regs();
                }
                self.rtc_latch_prev = v;
            }
            (Mbc::Mbc5, 0x2000..=0x2FFF) => self.rom_bank = (self.rom_bank & 0x100) | v as usize,
            (Mbc::Mbc5, 0x3000..=0x3FFF) => self.rom_bank = (self.rom_bank & 0xFF) | ((v as usize & 1) << 8),
            (Mbc::Mbc1, 0x4000..=0x5FFF) => {
                self.ram_bank = (v & 3) as usize;
                self.rom_bank = (self.rom_bank & 0x1F) | ((v as usize & 3) << 5);
            }
            (Mbc::Mbc3, 0x4000..=0x5FFF) => self.ram_bank = (v & 0x0F) as usize,
            (Mbc::Mbc5, 0x4000..=0x5FFF) => self.ram_bank = (v & 0x0F) as usize,
            (Mbc::Mbc1, 0x6000..=0x7FFF) => self.mbc1_mode = v & 1 != 0,
            (_, 0xA000..=0xBFFF) => {
                if !self.ram_enabled || self.ram.is_empty() {
                    return;
                }
                if self.kind == Mbc::Mbc2 {
                    self.ram[(addr as usize - 0xA000) & 0x1FF] = v & 0x0F;
                    return;
                }
                if self.kind == Mbc::Mbc3 && (0x08..=0x0C).contains(&self.ram_bank) {
                    self.rtc_write(self.ram_bank - 8, v);
                    return;
                }
                let i = (self.ram_bank() * 0x2000 + (addr as usize - 0xA000)) % self.ram.len();
                self.ram[i] = v;
            }
            _ => {}
        }
    }
}
