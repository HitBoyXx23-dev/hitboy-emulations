//! iNES ROM parsing and cartridge mappers.

impl state::Stateful for Mirroring {
    fn sync(&mut self, s: &mut state::State) {
        let mut tag: u8 = match self {
            Mirroring::Horizontal => 0,
            Mirroring::Vertical => 1,
            Mirroring::SingleLower => 2,
            Mirroring::SingleUpper => 3,
            Mirroring::FourScreen => 4,
            Mirroring::Custom(_) => 5,
        };
        let mut quads = if let Mirroring::Custom(q) = self { *q } else { [0; 4] };
        tag.sync(s);
        quads.sync(s);
        if s.is_load() {
            *self = match tag {
                0 => Mirroring::Horizontal,
                1 => Mirroring::Vertical,
                2 => Mirroring::SingleLower,
                3 => Mirroring::SingleUpper,
                4 => Mirroring::FourScreen,
                5 => Mirroring::Custom(quads),
                _ => {
                    s.fail();
                    return;
                }
            };
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mirroring {
    Horizontal,
    Vertical,
    SingleLower,
    SingleUpper,
    FourScreen,
    /// Nametable page (0/1) for each of the four quadrants (MMC5).
    Custom([u8; 4]),
}

#[derive(Debug)]
pub enum RomError {
    NotINes,
    Truncated,
    UnsupportedMapper(u8),
}

impl std::fmt::Display for RomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RomError::NotINes => write!(f, "not an iNES ROM (missing NES<1A> header)"),
            RomError::Truncated => write!(f, "ROM file is truncated"),
            RomError::UnsupportedMapper(m) => write!(f, "mapper {m} is not supported yet"),
        }
    }
}

impl std::error::Error for RomError {}

pub trait Mapper {
    /// Saves or loads this board's registers and RAM (save states, rewind). Every board
    /// must say how: a forgotten register would silently break loading.
    fn sync(&mut self, s: &mut state::State);
    fn cpu_read(&mut self, addr: u16) -> u8;
    fn cpu_write(&mut self, addr: u16, value: u8);
    fn ppu_read(&mut self, addr: u16) -> u8;
    fn ppu_write(&mut self, addr: u16, value: u8);
    fn mirroring(&self) -> Mirroring;
    /// Called once per rendered scanline (used by MMC3's IRQ counter).
    fn scanline(&mut self) {}
    fn irq(&self) -> bool {
        false
    }
    /// Advances CPU-clocked hardware (IRQ counters, the disk drive, expansion sound).
    fn tick(&mut self, _cpu_cycles: u32) {}
    /// Expansion sound level mixed into the APU output.
    fn audio(&self) -> f32 {
        0.0
    }
    /// Battery/disk data to keep between sessions.
    fn save(&self) -> Option<Vec<u8>> {
        None
    }
    fn load_save(&mut self, _data: &[u8]) {}
    /// Nametable bytes the cartridge supplies itself (MMC5 ExRAM / fill mode).
    fn nt_read(&mut self, _addr: u16) -> Option<u8> {
        None
    }
    /// Returns true if the cartridge took the nametable write.
    fn nt_write(&mut self, _addr: u16, _v: u8) -> bool {
        false
    }
    /// The PPU is about to fetch sprite (true) or background (false) patterns.
    fn ppu_fetch(&mut self, _sprites: bool, _tall: bool) {}
    /// Disk System only: flips to the next disk side; returns the new side index.
    fn next_disk_side(&mut self) -> Option<usize> {
        None
    }
}

/// Parses an iNES file and returns the matching mapper implementation.
pub fn load(rom: &[u8]) -> Result<Box<dyn Mapper>, RomError> {
    if rom.len() < 16 || &rom[0..4] != b"NES\x1A" {
        return Err(RomError::NotINes);
    }
    let prg_banks = rom[4] as usize;
    let chr_banks = rom[5] as usize;
    let flags6 = rom[6];
    let flags7 = rom[7];
    let mut mapper_id = (flags7 & 0xF0) | (flags6 >> 4);
    // Old dumps carry junk ("DiskDude!") in bytes 7-15; their upper mapper nibble is bogus.
    if flags7 & 0x0C == 0 && rom[12..16] != [0, 0, 0, 0] {
        mapper_id &= 0x0F;
    }
    let battery = flags6 & 0x02 != 0;
    let mirroring = if flags6 & 0x08 != 0 {
        Mirroring::FourScreen
    } else if flags6 & 0x01 != 0 {
        Mirroring::Vertical
    } else {
        Mirroring::Horizontal
    };

    let mut offset = 16;
    if flags6 & 0x04 != 0 {
        offset += 512; // trainer
    }
    let prg_len = prg_banks * 0x4000;
    let chr_len = chr_banks * 0x2000;
    if rom.len() < offset + prg_len + chr_len {
        return Err(RomError::Truncated);
    }
    let prg = rom[offset..offset + prg_len].to_vec();
    let (chr, chr_is_ram) = if chr_banks == 0 {
        (vec![0u8; 0x2000], true)
    } else {
        (rom[offset + prg_len..offset + prg_len + chr_len].to_vec(), false)
    };

    let base = Base { prg, chr, chr_is_ram, prg_ram: vec![0; 0x2000], mirroring };
    use crate::mappers::*;
    let m: Box<dyn Mapper> = match mapper_id {
        0 => Box::new(Nrom(base)),
        1 => Box::new(Mmc1::new(base)),
        2 => Box::new(Uxrom { base, bank: 0 }),
        3 => Box::new(Cnrom { base, bank: 0 }),
        4 => Box::new(Mmc3::new(base)),
        // MMC5 keeps its own (larger) save RAM.
        5 => return Ok(Box::new(Mmc5::new(base))),
        7 | 11 | 34 | 66 | 71 | 79 | 87 | 140 | 180 | 185 => Box::new(Simple::new(base, mapper_id)),
        9 => Box::new(Mmc2::new(base, false)),
        10 => Box::new(Mmc2::new(base, true)),
        16 => Box::new(Bandai::new(base)),
        21 | 22 | 23 | 25 => Box::new(Vrc4::new(base, mapper_id)),
        24 => Box::new(Vrc6::new(base, false)),
        26 => Box::new(Vrc6::new(base, true)),
        69 => Box::new(Fme7::new(base)),
        206 => Box::new(Namco108::new(base)),
        m => return Err(RomError::UnsupportedMapper(m)),
    };
    Ok(if battery { Box::new(Battery::new(m)) } else { m })
}

/// Mapper 16: Bandai FCG-1/2 and LZ93D50 (Dragon Ball, SD Gundam...): 16 KiB PRG banks,
/// eight 1 KiB CHR banks and a CPU-cycle IRQ counter. Registers answer at $6000-$FFFF.
#[derive(state::Stateful)]
struct Bandai {
    base: Base,
    chr: [u8; 8],
    prg: u8,
    mirroring: Mirroring,
    irq_enabled: bool,
    counter: u16,
    latch: u16,
    irq: bool,
}

impl Bandai {
    fn new(base: Base) -> Self {
        let mirroring = base.mirroring;
        Bandai { base, chr: [0, 1, 2, 3, 4, 5, 6, 7], prg: 0, mirroring, irq_enabled: false, counter: 0, latch: 0, irq: false }
    }

    fn chr_index(&self, addr: u16) -> usize {
        let bank = self.chr[(addr as usize >> 10) & 7] as usize;
        (bank * 0x400 + (addr as usize & 0x3FF)) % self.base.chr.len()
    }
}

impl Mapper for Bandai {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        let b = &self.base;
        match addr {
            // Serial EEPROM data line: reads as idle.
            0x6000..=0x7FFF => 0,
            0x8000..=0xBFFF => b.prg[(self.prg as usize % b.prg_bank_count_16k()) * 0x4000 + (addr as usize - 0x8000)],
            0xC000..=0xFFFF => b.prg[(b.prg_bank_count_16k() - 1) * 0x4000 + (addr as usize - 0xC000)],
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, v: u8) {
        if addr < 0x6000 {
            return;
        }
        match addr & 0xF {
            r @ 0..=7 => self.chr[r as usize] = v,
            8 => self.prg = v & 0x0F,
            9 => {
                self.mirroring = match v & 3 {
                    0 => Mirroring::Vertical,
                    1 => Mirroring::Horizontal,
                    2 => Mirroring::SingleLower,
                    _ => Mirroring::SingleUpper,
                }
            }
            0xA => {
                self.irq_enabled = v & 1 != 0;
                self.counter = self.latch; // LZ93D50 reloads from the latch
                self.irq = false;
            }
            // FCG boards write the counter directly; LZ93D50 boards latch it.
            0xB => {
                self.latch = (self.latch & 0xFF00) | v as u16;
                self.counter = (self.counter & 0xFF00) | v as u16;
            }
            0xC => {
                self.latch = (self.latch & 0x00FF) | (v as u16) << 8;
                self.counter = (self.counter & 0x00FF) | (v as u16) << 8;
            }
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        let i = self.chr_index(addr);
        self.base.chr[i]
    }
    fn ppu_write(&mut self, addr: u16, value: u8) {
        let i = self.chr_index(addr);
        self.base.chr_write(i, value);
    }
    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }
    fn irq(&self) -> bool {
        self.irq
    }
    fn tick(&mut self, cycles: u32) {
        if self.irq_enabled {
            if (self.counter as u32) < cycles {
                self.irq = true;
            }
            self.counter = self.counter.wrapping_sub(cycles as u16);
        }
    }
}

pub(crate) struct Base {
    pub(crate) prg: Vec<u8>,
    pub(crate) chr: Vec<u8>,
    pub(crate) chr_is_ram: bool,
    pub(crate) prg_ram: Vec<u8>,
    pub(crate) mirroring: Mirroring,
}

impl state::Stateful for Base {
    /// Only what changes: CHR when it's RAM, the PRG RAM and the mirroring.
    fn sync(&mut self, s: &mut state::State) {
        if self.chr_is_ram {
            self.chr.sync(s);
        }
        self.prg_ram.sync(s);
        self.mirroring.sync(s);
    }
}

impl Base {
    pub(crate) fn prg_bank_count_16k(&self) -> usize {
        (self.prg.len() / 0x4000).max(1)
    }
    pub(crate) fn prg_ram_read(&self, addr: u16) -> u8 {
        self.prg_ram[(addr - 0x6000) as usize]
    }
    pub(crate) fn prg_ram_write(&mut self, addr: u16, v: u8) {
        self.prg_ram[(addr - 0x6000) as usize] = v;
    }
    pub(crate) fn chr_write(&mut self, idx: usize, v: u8) {
        if self.chr_is_ram {
            let len = self.chr.len();
            self.chr[idx % len] = v;
        }
    }
}

/// Mapper 0: fixed 16/32 KiB PRG, 8 KiB CHR.
#[derive(state::Stateful)]
struct Nrom(Base);

impl Mapper for Nrom {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => self.0.prg_ram_read(addr),
            0x8000..=0xFFFF => self.0.prg[(addr as usize - 0x8000) % self.0.prg.len()],
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, value: u8) {
        if let 0x6000..=0x7FFF = addr {
            self.0.prg_ram_write(addr, value);
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.0.chr[addr as usize & 0x1FFF]
    }
    fn ppu_write(&mut self, addr: u16, value: u8) {
        self.0.chr_write(addr as usize & 0x1FFF, value);
    }
    fn mirroring(&self) -> Mirroring {
        self.0.mirroring
    }
}

/// Mapper 2: switchable 16 KiB bank at $8000, last bank fixed at $C000.
#[derive(state::Stateful)]
struct Uxrom {
    base: Base,
    bank: usize,
}

impl Mapper for Uxrom {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        let b = &self.base;
        match addr {
            0x6000..=0x7FFF => b.prg_ram_read(addr),
            0x8000..=0xBFFF => b.prg[self.bank * 0x4000 + (addr as usize - 0x8000)],
            0xC000..=0xFFFF => b.prg[(b.prg_bank_count_16k() - 1) * 0x4000 + (addr as usize - 0xC000)],
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, value: u8) {
        match addr {
            0x6000..=0x7FFF => self.base.prg_ram_write(addr, value),
            0x8000..=0xFFFF => self.bank = value as usize % self.base.prg_bank_count_16k(),
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.base.chr[addr as usize & 0x1FFF]
    }
    fn ppu_write(&mut self, addr: u16, value: u8) {
        self.base.chr_write(addr as usize & 0x1FFF, value);
    }
    fn mirroring(&self) -> Mirroring {
        self.base.mirroring
    }
}

/// Mapper 3: fixed PRG, switchable 8 KiB CHR bank.
#[derive(state::Stateful)]
struct Cnrom {
    base: Base,
    bank: usize,
}

impl Mapper for Cnrom {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => self.base.prg_ram_read(addr),
            0x8000..=0xFFFF => self.base.prg[(addr as usize - 0x8000) % self.base.prg.len()],
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, value: u8) {
        match addr {
            0x6000..=0x7FFF => self.base.prg_ram_write(addr, value),
            0x8000..=0xFFFF => {
                let banks = (self.base.chr.len() / 0x2000).max(1);
                self.bank = value as usize % banks;
            }
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.base.chr[self.bank * 0x2000 + (addr as usize & 0x1FFF)]
    }
    fn ppu_write(&mut self, addr: u16, value: u8) {
        self.base.chr_write(self.bank * 0x2000 + (addr as usize & 0x1FFF), value);
    }
    fn mirroring(&self) -> Mirroring {
        self.base.mirroring
    }
}

/// Mapper 1 (MMC1): serial-loaded bank registers. Used by Zelda, Metroid, Mega Man 2...
#[derive(state::Stateful)]
struct Mmc1 {
    base: Base,
    shift: u8,
    shift_count: u8,
    control: u8,
    chr0: u8,
    chr1: u8,
    prg: u8,
}

impl Mmc1 {
    fn new(base: Base) -> Self {
        Self { base, shift: 0, shift_count: 0, control: 0x0C, chr0: 0, chr1: 0, prg: 0 }
    }

    fn prg_offset(&self, addr: u16) -> usize {
        let banks = self.base.prg_bank_count_16k();
        let bank = self.prg as usize & 0x0F;
        let (bank, off) = match (self.control >> 2) & 3 {
            0 | 1 => ((bank & !1) + ((addr >= 0xC000) as usize), addr as usize & 0x3FFF),
            2 => if addr < 0xC000 { (0, addr as usize & 0x3FFF) } else { (bank, addr as usize & 0x3FFF) },
            _ => if addr < 0xC000 { (bank, addr as usize & 0x3FFF) } else { (banks - 1, addr as usize & 0x3FFF) },
        };
        (bank % banks) * 0x4000 + off
    }

    fn chr_offset(&self, addr: u16) -> usize {
        let a = addr as usize & 0x1FFF;
        let idx = if self.control & 0x10 == 0 {
            (self.chr0 as usize & !1) * 0x1000 + a
        } else if a < 0x1000 {
            self.chr0 as usize * 0x1000 + a
        } else {
            self.chr1 as usize * 0x1000 + (a - 0x1000)
        };
        idx % self.base.chr.len()
    }
}

impl Mapper for Mmc1 {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => self.base.prg_ram_read(addr),
            0x8000..=0xFFFF => self.base.prg[self.prg_offset(addr)],
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, value: u8) {
        match addr {
            0x6000..=0x7FFF => self.base.prg_ram_write(addr, value),
            0x8000..=0xFFFF => {
                if value & 0x80 != 0 {
                    self.shift = 0;
                    self.shift_count = 0;
                    self.control |= 0x0C;
                    return;
                }
                self.shift |= (value & 1) << self.shift_count;
                self.shift_count += 1;
                if self.shift_count == 5 {
                    let v = self.shift;
                    match addr {
                        0x8000..=0x9FFF => self.control = v,
                        0xA000..=0xBFFF => self.chr0 = v,
                        0xC000..=0xDFFF => self.chr1 = v,
                        _ => self.prg = v,
                    }
                    self.shift = 0;
                    self.shift_count = 0;
                }
            }
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.base.chr[self.chr_offset(addr)]
    }
    fn ppu_write(&mut self, addr: u16, value: u8) {
        let idx = self.chr_offset(addr);
        self.base.chr_write(idx, value);
    }
    fn mirroring(&self) -> Mirroring {
        match self.control & 3 {
            0 => Mirroring::SingleLower,
            1 => Mirroring::SingleUpper,
            2 => Mirroring::Vertical,
            _ => Mirroring::Horizontal,
        }
    }
}

/// Mapper 4 (MMC3): 8 KiB PRG / 1 KiB CHR banking plus a scanline IRQ counter.
/// Used by SMB2, SMB3, Kirby's Adventure, Mega Man 3-6...
#[derive(state::Stateful)]
struct Mmc3 {
    base: Base,
    bank_select: u8,
    regs: [u8; 8],
    mirror_horizontal: bool,
    irq_latch: u8,
    irq_counter: u8,
    irq_reload: bool,
    irq_enabled: bool,
    irq_pending: bool,
}

impl Mmc3 {
    fn new(base: Base) -> Self {
        Self {
            mirror_horizontal: base.mirroring == Mirroring::Horizontal,
            base,
            bank_select: 0,
            regs: [0, 2, 4, 5, 6, 7, 0, 1],
            irq_latch: 0,
            irq_counter: 0,
            irq_reload: false,
            irq_enabled: false,
            irq_pending: false,
        }
    }

    fn prg_offset(&self, addr: u16) -> usize {
        let banks = (self.base.prg.len() / 0x2000).max(1);
        let second_last = banks - 2;
        let slot = (addr as usize - 0x8000) / 0x2000;
        let swap = self.bank_select & 0x40 != 0;
        let bank = match (slot, swap) {
            (0, false) | (2, true) => self.regs[6] as usize,
            (0, true) | (2, false) => second_last,
            (1, _) => self.regs[7] as usize,
            _ => banks - 1,
        };
        (bank % banks) * 0x2000 + (addr as usize & 0x1FFF)
    }

    fn chr_offset(&self, addr: u16) -> usize {
        let mut a = addr as usize & 0x1FFF;
        if self.bank_select & 0x80 != 0 {
            a ^= 0x1000;
        }
        let bank = match a / 0x400 {
            0 => self.regs[0] as usize & !1,
            1 => self.regs[0] as usize | 1,
            2 => self.regs[1] as usize & !1,
            3 => self.regs[1] as usize | 1,
            n => self.regs[n - 2] as usize,
        };
        (bank * 0x400 + (a & 0x3FF)) % self.base.chr.len()
    }
}

impl Mapper for Mmc3 {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x6000..=0x7FFF => self.base.prg_ram_read(addr),
            0x8000..=0xFFFF => self.base.prg[self.prg_offset(addr)],
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, value: u8) {
        let even = addr & 1 == 0;
        match (addr, even) {
            (0x6000..=0x7FFF, _) => self.base.prg_ram_write(addr, value),
            (0x8000..=0x9FFF, true) => self.bank_select = value,
            (0x8000..=0x9FFF, false) => self.regs[(self.bank_select & 7) as usize] = value,
            (0xA000..=0xBFFF, true) => self.mirror_horizontal = value & 1 != 0,
            (0xA000..=0xBFFF, false) => {} // PRG-RAM protect: ignored
            (0xC000..=0xDFFF, true) => self.irq_latch = value,
            (0xC000..=0xDFFF, false) => {
                self.irq_counter = 0;
                self.irq_reload = true;
            }
            (0xE000..=0xFFFF, true) => {
                self.irq_enabled = false;
                self.irq_pending = false;
            }
            (0xE000..=0xFFFF, false) => self.irq_enabled = true,
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.base.chr[self.chr_offset(addr)]
    }
    fn ppu_write(&mut self, addr: u16, value: u8) {
        let idx = self.chr_offset(addr);
        self.base.chr_write(idx, value);
    }
    fn mirroring(&self) -> Mirroring {
        if self.base.mirroring == Mirroring::FourScreen {
            Mirroring::FourScreen
        } else if self.mirror_horizontal {
            Mirroring::Horizontal
        } else {
            Mirroring::Vertical
        }
    }
    fn scanline(&mut self) {
        if self.irq_counter == 0 || self.irq_reload {
            self.irq_counter = self.irq_latch;
            self.irq_reload = false;
        } else {
            self.irq_counter -= 1;
        }
        if self.irq_counter == 0 && self.irq_enabled {
            self.irq_pending = true;
        }
    }
    fn irq(&self) -> bool {
        self.irq_pending
    }
}
