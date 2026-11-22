//! More cartridge boards: AxROM, MMC2/MMC4, Color Dreams, GxROM, Camerica, BNROM/NINA,
//! Namco 108, Konami VRC2/VRC4/VRC6, Sunsoft FME-7, MMC5 and several discrete-logic boards,
//! plus the battery-backed RAM wrapper that keeps saves between sessions.

use crate::cartridge::{Base, Mapper, Mirroring};

fn bank(data: &[u8], size: usize, n: usize, off: usize) -> u8 {
    let count = (data.len() / size).max(1);
    data[(n % count) * size + (off % size)]
}

fn single(v: bool) -> Mirroring {
    if v {
        Mirroring::SingleUpper
    } else {
        Mirroring::SingleLower
    }
}

// ------------------------------------------------------------ battery RAM

/// Keeps a copy of the cartridge's $6000-$7FFF RAM for battery-backed boards.
pub struct Battery {
    pub inner: Box<dyn Mapper>,
    ram: Vec<u8>,
    loaded: bool,
}

impl Battery {
    pub fn new(inner: Box<dyn Mapper>) -> Self {
        Battery { inner, ram: vec![0; 0x2000], loaded: false }
    }
}

impl Mapper for Battery {
    fn sync(&mut self, s: &mut state::State) {
        self.inner.sync(s);
        state::Stateful::sync(&mut self.ram, s);
        state::Stateful::sync(&mut self.loaded, s);
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        self.inner.cpu_read(addr)
    }
    fn cpu_write(&mut self, addr: u16, v: u8) {
        if (0x6000..0x8000).contains(&addr) {
            self.ram[(addr - 0x6000) as usize] = v;
        }
        self.inner.cpu_write(addr, v)
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.inner.ppu_read(addr)
    }
    fn ppu_write(&mut self, addr: u16, v: u8) {
        self.inner.ppu_write(addr, v)
    }
    fn mirroring(&self) -> Mirroring {
        self.inner.mirroring()
    }
    fn scanline(&mut self) {
        self.inner.scanline()
    }
    fn irq(&self) -> bool {
        self.inner.irq()
    }
    fn tick(&mut self, c: u32) {
        self.inner.tick(c)
    }
    fn audio(&self) -> f32 {
        self.inner.audio()
    }
    fn nt_read(&mut self, addr: u16) -> Option<u8> {
        self.inner.nt_read(addr)
    }
    fn nt_write(&mut self, addr: u16, v: u8) -> bool {
        self.inner.nt_write(addr, v)
    }
    fn ppu_fetch(&mut self, sprites: bool, tall: bool) {
        self.inner.ppu_fetch(sprites, tall)
    }
    fn save(&self) -> Option<Vec<u8>> {
        Some(self.ram.clone())
    }
    fn load_save(&mut self, data: &[u8]) {
        if data.len() == self.ram.len() && !self.loaded {
            self.loaded = true;
            for (i, &b) in data.iter().enumerate() {
                self.ram[i] = b;
                self.inner.cpu_write(0x6000 + i as u16, b);
            }
        }
    }
}

// ------------------------------------------------------------ simple discrete boards

/// 32 KiB PRG and 8 KiB CHR selected by a register (AxROM 7, Color Dreams 11, GxROM 66,
/// BNROM/NINA-001 34, NINA-03/06 79, Jaleco 140, 87, Camerica 71, UN1ROM 180, CNROM 185).
#[derive(state::Stateful)]
pub struct Simple {
    base: Base,
    kind: u8,
    prg: usize,
    chr: usize,
    mirroring: Mirroring,
}

impl Simple {
    pub(crate) fn new(base: Base, kind: u8) -> Self {
        let mirroring = base.mirroring;
        let prg = if kind == 180 { 0 } else { 0 };
        Simple { base, kind, prg, chr: 0, mirroring }
    }
    fn chr_rom(&self) -> bool {
        !self.base.chr_is_ram
    }
}

impl Mapper for Simple {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        let b = &self.base;
        match addr {
            0x6000..=0x7FFF => b.prg_ram_read(addr),
            0x8000..=0xFFFF => match self.kind {
                // 16 KiB switchable + fixed banks.
                71 => {
                    let last = b.prg_bank_count_16k() - 1;
                    let n = if addr < 0xC000 { self.prg } else { last };
                    bank(&b.prg, 0x4000, n, addr as usize - 0x8000)
                }
                180 => {
                    let n = if addr < 0xC000 { 0 } else { self.prg };
                    bank(&b.prg, 0x4000, n, addr as usize - 0x8000)
                }
                185 => b.prg[(addr as usize - 0x8000) % b.prg.len()],
                _ => bank(&b.prg, 0x8000, self.prg, addr as usize - 0x8000),
            },
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, v: u8) {
        match (self.kind, addr) {
            (79, 0x4100..=0x5FFF) if addr & 0x4100 == 0x4100 => {
                self.prg = ((v >> 3) & 1) as usize;
                self.chr = (v & 7) as usize;
            }
            (34, 0x7FFD) => self.prg = v as usize,
            (34, 0x7FFE) => self.chr = (self.chr & !0xFF) | v as usize,
            (34, 0x7FFF) => self.chr = (self.chr & 0xFF) | (v as usize) << 8,
            (140, 0x6000..=0x7FFF) => {
                self.prg = ((v >> 4) & 3) as usize;
                self.chr = (v & 0xF) as usize;
            }
            (87, 0x6000..=0x7FFF) => self.chr = (((v & 1) << 1) | ((v >> 1) & 1)) as usize,
            (_, 0x6000..=0x7FFF) => self.base.prg_ram_write(addr, v),
            (7, 0x8000..) => {
                self.prg = (v & 7) as usize;
                self.mirroring = single(v & 0x10 != 0);
            }
            (11, 0x8000..) => {
                self.prg = (v & 3) as usize;
                self.chr = (v >> 4) as usize;
            }
            (66, 0x8000..) => {
                self.prg = ((v >> 4) & 3) as usize;
                self.chr = (v & 3) as usize;
            }
            (34, 0x8000..) if !self.chr_rom() || self.base.chr.len() <= 0x2000 => self.prg = v as usize,
            (71, 0x9000..=0x9FFF) => self.mirroring = single(v & 0x10 != 0),
            (71, 0xC000..) => self.prg = (v & 0xF) as usize,
            (180, 0x8000..) => self.prg = (v & 7) as usize,
            (185, 0x8000..) => self.chr = (v & 3) as usize,
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        let a = addr as usize & 0x1FFF;
        match self.kind {
            // NINA-001: two 4 KiB CHR banks.
            34 if self.chr_rom() && self.base.chr.len() > 0x2000 => {
                let n = if a < 0x1000 { self.chr & 0xFF } else { self.chr >> 8 };
                bank(&self.base.chr, 0x1000, n, a)
            }
            _ if self.chr_rom() => bank(&self.base.chr, 0x2000, self.chr, a),
            _ => self.base.chr[a % self.base.chr.len()],
        }
    }
    fn ppu_write(&mut self, addr: u16, v: u8) {
        self.base.chr_write(addr as usize & 0x1FFF, v);
    }
    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }
}

// ------------------------------------------------------------ MMC2 / MMC4

/// MMC2 (mapper 9, Punch-Out!!) and MMC4 (mapper 10, Fire Emblem): CHR banks switched by
/// latches that flip when the PPU fetches tiles $FD / $FE.
#[derive(state::Stateful)]
pub struct Mmc2 {
    base: Base,
    mmc4: bool,
    prg: usize,
    chr: [[usize; 2]; 2],
    latch: [usize; 2],
    horizontal: bool,
}

impl Mmc2 {
    pub(crate) fn new(base: Base, mmc4: bool) -> Self {
        Mmc2 { base, mmc4, prg: 0, chr: [[0, 0], [0, 0]], latch: [1, 1], horizontal: false }
    }
}

impl Mapper for Mmc2 {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        let b = &self.base;
        match addr {
            0x6000..=0x7FFF => b.prg_ram_read(addr),
            0x8000..=0xFFFF if self.mmc4 => {
                let last = b.prg_bank_count_16k() - 1;
                let n = if addr < 0xC000 { self.prg } else { last };
                bank(&b.prg, 0x4000, n, addr as usize - 0x8000)
            }
            0x8000..=0x9FFF => bank(&b.prg, 0x2000, self.prg, addr as usize),
            0xA000..=0xFFFF => {
                let count = b.prg.len() / 0x2000;
                let n = count - 3 + ((addr as usize - 0xA000) >> 13);
                bank(&b.prg, 0x2000, n, addr as usize)
            }
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, v: u8) {
        let v = v as usize;
        match addr {
            0x6000..=0x7FFF => self.base.prg_ram_write(addr, v as u8),
            0xA000..=0xAFFF => self.prg = v & 0xF,
            0xB000..=0xBFFF => self.chr[0][0] = v & 0x1F,
            0xC000..=0xCFFF => self.chr[0][1] = v & 0x1F,
            0xD000..=0xDFFF => self.chr[1][0] = v & 0x1F,
            0xE000..=0xEFFF => self.chr[1][1] = v & 0x1F,
            0xF000..=0xFFFF => self.horizontal = v & 1 != 0,
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        let half = (addr as usize >> 12) & 1;
        let v = bank(&self.base.chr, 0x1000, self.chr[half][self.latch[half]], addr as usize);
        // Latch updates after the fetch.
        match addr & 0x1FF8 {
            0x0FD8 => self.latch[0] = 0,
            0x0FE8 => self.latch[0] = 1,
            0x1FD8 => self.latch[1] = 0,
            0x1FE8 => self.latch[1] = 1,
            _ => {}
        }
        v
    }
    fn ppu_write(&mut self, addr: u16, v: u8) {
        self.base.chr_write(addr as usize & 0x1FFF, v);
    }
    fn mirroring(&self) -> Mirroring {
        if self.horizontal {
            Mirroring::Horizontal
        } else {
            Mirroring::Vertical
        }
    }
}

// ------------------------------------------------------------ Namco 108

/// Namco 108 / DxROM (mapper 206): MMC3's banking without its IRQ or mirroring control.
#[derive(state::Stateful)]
pub struct Namco108 {
    base: Base,
    select: usize,
    regs: [usize; 8],
}

impl Namco108 {
    pub(crate) fn new(base: Base) -> Self {
        Namco108 { base, select: 0, regs: [0, 2, 4, 5, 6, 7, 0, 1] }
    }
}

impl Mapper for Namco108 {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        let b = &self.base;
        let count = b.prg.len() / 0x2000;
        match addr {
            0x6000..=0x7FFF => b.prg_ram_read(addr),
            0x8000..=0x9FFF => bank(&b.prg, 0x2000, self.regs[6], addr as usize),
            0xA000..=0xBFFF => bank(&b.prg, 0x2000, self.regs[7], addr as usize),
            0xC000..=0xDFFF => bank(&b.prg, 0x2000, count - 2, addr as usize),
            0xE000..=0xFFFF => bank(&b.prg, 0x2000, count - 1, addr as usize),
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, v: u8) {
        match addr {
            0x6000..=0x7FFF => self.base.prg_ram_write(addr, v),
            0x8000..=0x9FFF if addr & 1 == 0 => self.select = (v & 7) as usize,
            0x8000..=0x9FFF => self.regs[self.select] = (v & 0x3F) as usize,
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        let a = addr as usize & 0x1FFF;
        let (n, size) = match a {
            0x0000..=0x07FF => (self.regs[0] >> 1, 0x800),
            0x0800..=0x0FFF => (self.regs[1] >> 1, 0x800),
            _ => (self.regs[2 + ((a - 0x1000) >> 10)], 0x400),
        };
        bank(&self.base.chr, size, n, a)
    }
    fn ppu_write(&mut self, addr: u16, v: u8) {
        self.base.chr_write(addr as usize & 0x1FFF, v);
    }
    fn mirroring(&self) -> Mirroring {
        self.base.mirroring
    }
}

// ------------------------------------------------------------ Konami VRC IRQ

/// The VRC4/VRC6/VRC7 IRQ counter: counts scanlines (via a 341/3-cycle prescaler) or CPU
/// cycles, and fires when it overflows from $FF.
#[derive(Default, state::Stateful)]
struct VrcIrq {
    latch: u8,
    counter: u8,
    enabled: bool,
    enable_after_ack: bool,
    cycle_mode: bool,
    prescaler: i32,
    pending: bool,
}

impl VrcIrq {
    fn control(&mut self, v: u8) {
        self.enable_after_ack = v & 1 != 0;
        self.enabled = v & 2 != 0;
        self.cycle_mode = v & 4 != 0;
        if self.enabled {
            self.counter = self.latch;
            self.prescaler = 341;
        }
        self.pending = false;
    }
    fn ack(&mut self) {
        self.pending = false;
        self.enabled = self.enable_after_ack;
    }
    fn clock(&mut self) {
        if self.counter == 0xFF {
            self.counter = self.latch;
            self.pending = true;
        } else {
            self.counter += 1;
        }
    }
    fn tick(&mut self, cycles: u32) {
        if !self.enabled {
            return;
        }
        for _ in 0..cycles {
            if self.cycle_mode {
                self.clock();
            } else {
                self.prescaler -= 3;
                if self.prescaler <= 0 {
                    self.prescaler += 341;
                    self.clock();
                }
            }
        }
    }
}

// ------------------------------------------------------------ VRC2 / VRC4

/// Konami VRC2/VRC4 (mappers 21, 22, 23, 25). Boards wire different address lines to the
/// chip's register-select pins; both common wirings of each mapper are accepted.
#[derive(state::Stateful)]
pub struct Vrc4 {
    base: Base,
    mapper: u8,
    prg: [usize; 2],
    swap: bool,
    chr: [usize; 8],
    mirroring: Mirroring,
    irq: VrcIrq,
}

impl Vrc4 {
    pub(crate) fn new(base: Base, mapper: u8) -> Self {
        Vrc4 { base, mapper, prg: [0, 1], swap: false, chr: [0; 8], mirroring: Mirroring::Vertical, irq: VrcIrq::default() }
    }

    /// Register index 0-3 from the address lines this board uses.
    fn reg(&self, addr: u16) -> usize {
        let a = addr as usize;
        let (lo, hi) = match self.mapper {
            21 => ((a >> 1 | a >> 6) & 1, (a >> 2 | a >> 7) & 1),
            22 => ((a >> 1) & 1, a & 1),
            23 => ((a | a >> 2) & 1, (a >> 1 | a >> 3) & 1),
            _ => ((a >> 1 | a >> 3) & 1, (a | a >> 2) & 1), // 25
        };
        lo | hi << 1
    }
}

impl Mapper for Vrc4 {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        let b = &self.base;
        let count = b.prg.len() / 0x2000;
        let n = match addr {
            0x6000..=0x7FFF => return b.prg_ram_read(addr),
            0x8000..=0x9FFF => {
                if self.swap {
                    count - 2
                } else {
                    self.prg[0]
                }
            }
            0xA000..=0xBFFF => self.prg[1],
            0xC000..=0xDFFF => {
                if self.swap {
                    self.prg[0]
                } else {
                    count - 2
                }
            }
            0xE000..=0xFFFF => count - 1,
            _ => return 0,
        };
        bank(&b.prg, 0x2000, n, addr as usize)
    }
    fn cpu_write(&mut self, addr: u16, v: u8) {
        if (0x6000..0x8000).contains(&addr) {
            return self.base.prg_ram_write(addr, v);
        }
        let r = self.reg(addr);
        let v4 = (v & 0xF) as usize;
        match addr & 0xF000 {
            0x8000 => self.prg[0] = (v & 0x1F) as usize,
            0x9000 => match r {
                0 | 1 if self.mapper == 22 => self.mirroring = if v & 1 != 0 { Mirroring::Horizontal } else { Mirroring::Vertical },
                0 => {
                    self.mirroring = match v & 3 {
                        0 => Mirroring::Vertical,
                        1 => Mirroring::Horizontal,
                        2 => Mirroring::SingleLower,
                        _ => Mirroring::SingleUpper,
                    }
                }
                2 => self.swap = v & 2 != 0,
                _ => {}
            },
            0xA000 => self.prg[1] = (v & 0x1F) as usize,
            0xB000..=0xE000 => {
                let slot = (((addr & 0xF000) - 0xB000) >> 11) as usize + (r >> 1);
                let c = &mut self.chr[slot];
                *c = if r & 1 == 0 { (*c & !0xF) | v4 } else { (*c & 0xF) | v4 << 4 };
            }
            0xF000 => match r {
                0 => self.irq.latch = (self.irq.latch & 0xF0) | (v & 0xF),
                1 => self.irq.latch = (self.irq.latch & 0x0F) | (v & 0xF) << 4,
                2 => self.irq.control(v),
                _ => self.irq.ack(),
            },
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        let a = addr as usize & 0x1FFF;
        let mut n = self.chr[a >> 10];
        if self.mapper == 22 {
            n >>= 1; // VRC2a ignores the lowest bank bit
        }
        bank(&self.base.chr, 0x400, n, a)
    }
    fn ppu_write(&mut self, addr: u16, v: u8) {
        self.base.chr_write(addr as usize & 0x1FFF, v);
    }
    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }
    fn irq(&self) -> bool {
        self.irq.pending
    }
    fn tick(&mut self, c: u32) {
        self.irq.tick(c);
    }
}

// ------------------------------------------------------------ VRC6

/// Konami VRC6 (mappers 24 and 26): 16+8 KiB PRG banks, 1 KiB CHR, IRQ, and two pulse
/// channels plus a sawtooth.
#[derive(state::Stateful)]
pub struct Vrc6 {
    base: Base,
    swapped: bool,
    prg16: usize,
    prg8: usize,
    chr: [usize; 8],
    mirroring: Mirroring,
    irq: VrcIrq,
    pulse: [(u8, u16, bool, u16, u8); 2], // (volume/duty, period, enabled, timer, step)
    saw: (u8, u16, bool, u16, u8, u8),    // (rate, period, enabled, timer, step, accumulator)
    halt: bool,
}

impl Vrc6 {
    pub(crate) fn new(base: Base, swapped: bool) -> Self {
        Vrc6 { base, swapped, prg16: 0, prg8: 0, chr: [0; 8], mirroring: Mirroring::Vertical, irq: VrcIrq::default(), pulse: [(0, 0, false, 0, 0); 2], saw: (0, 0, false, 0, 0, 0), halt: false }
    }
}

impl Mapper for Vrc6 {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        let b = &self.base;
        match addr {
            0x6000..=0x7FFF => b.prg_ram_read(addr),
            0x8000..=0xBFFF => bank(&b.prg, 0x4000, self.prg16, addr as usize),
            0xC000..=0xDFFF => bank(&b.prg, 0x2000, self.prg8, addr as usize),
            0xE000..=0xFFFF => bank(&b.prg, 0x2000, b.prg.len() / 0x2000 - 1, addr as usize),
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, v: u8) {
        if (0x6000..0x8000).contains(&addr) {
            return self.base.prg_ram_write(addr, v);
        }
        let mut r = (addr & 3) as usize;
        if self.swapped {
            r = (r >> 1) | (r & 1) << 1;
        }
        match (addr & 0xF000, r) {
            (0x8000, _) => self.prg16 = (v & 0xF) as usize,
            (0x9000 | 0xA000, 3) if addr & 0xF000 == 0x9000 => self.halt = v & 1 != 0,
            (0x9000 | 0xA000, _) => {
                let p = &mut self.pulse[((addr & 0xF000) == 0xA000) as usize];
                match r {
                    0 => p.0 = v,
                    1 => p.1 = (p.1 & 0xF00) | v as u16,
                    _ => {
                        p.1 = (p.1 & 0xFF) | ((v & 0xF) as u16) << 8;
                        p.2 = v & 0x80 != 0;
                    }
                }
            }
            (0xB000, 0) => self.saw.0 = v & 0x3F,
            (0xB000, 1) => self.saw.1 = (self.saw.1 & 0xF00) | v as u16,
            (0xB000, 2) => {
                self.saw.1 = (self.saw.1 & 0xFF) | ((v & 0xF) as u16) << 8;
                self.saw.2 = v & 0x80 != 0;
            }
            (0xB000, _) => {
                self.mirroring = match (v >> 2) & 3 {
                    0 => Mirroring::Vertical,
                    1 => Mirroring::Horizontal,
                    2 => Mirroring::SingleLower,
                    _ => Mirroring::SingleUpper,
                }
            }
            (0xC000, _) => self.prg8 = (v & 0x1F) as usize,
            (0xD000, _) => self.chr[r] = v as usize,
            (0xE000, _) => self.chr[4 + r] = v as usize,
            (0xF000, 0) => self.irq.latch = v,
            (0xF000, 1) => self.irq.control(v),
            (0xF000, 2) => self.irq.ack(),
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        let a = addr as usize & 0x1FFF;
        bank(&self.base.chr, 0x400, self.chr[a >> 10], a)
    }
    fn ppu_write(&mut self, addr: u16, v: u8) {
        self.base.chr_write(addr as usize & 0x1FFF, v);
    }
    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }
    fn irq(&self) -> bool {
        self.irq.pending
    }
    fn tick(&mut self, cycles: u32) {
        self.irq.tick(cycles);
        if self.halt {
            return;
        }
        for _ in 0..cycles {
            for p in self.pulse.iter_mut() {
                if !p.2 {
                    continue;
                }
                if p.3 == 0 {
                    p.3 = p.1;
                    p.4 = (p.4 + 1) & 15;
                } else {
                    p.3 -= 1;
                }
            }
            let s = &mut self.saw;
            if s.2 {
                if s.3 == 0 {
                    s.3 = s.1;
                    s.4 += 1;
                    if s.4 >= 14 {
                        s.4 = 0;
                        s.5 = 0;
                    } else if s.4 & 1 == 0 {
                        s.5 = s.5.wrapping_add(s.0);
                    }
                } else {
                    s.3 -= 1;
                }
            }
        }
    }
    fn audio(&self) -> f32 {
        let mut out = 0.0;
        for p in &self.pulse {
            if p.2 {
                let duty = (p.0 >> 4) & 7;
                let high = p.0 & 0x80 != 0 || p.4 <= duty;
                if high {
                    out += (p.0 & 0xF) as f32;
                }
            }
        }
        if self.saw.2 {
            out += (self.saw.5 >> 3) as f32;
        }
        out / 61.0 * 0.5
    }
}

// ------------------------------------------------------------ Sunsoft FME-7

/// Sunsoft FME-7 / 5B (mapper 69): command/parameter registers, 8 KiB PRG banks, 1 KiB
/// CHR, optional RAM at $6000 and a 16-bit CPU-cycle IRQ counter.
#[derive(state::Stateful)]
pub struct Fme7 {
    base: Base,
    cmd: u8,
    chr: [usize; 8],
    prg: [usize; 4],
    ram_enabled: bool,
    ram_selected: bool,
    mirroring: Mirroring,
    irq_enabled: bool,
    counter_enabled: bool,
    counter: u16,
    irq: bool,
}

impl Fme7 {
    pub(crate) fn new(base: Base) -> Self {
        Fme7 { base, cmd: 0, chr: [0; 8], prg: [0; 4], ram_enabled: false, ram_selected: false, mirroring: Mirroring::Vertical, irq_enabled: false, counter_enabled: false, counter: 0, irq: false }
    }
}

impl Mapper for Fme7 {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        let b = &self.base;
        match addr {
            0x6000..=0x7FFF if self.ram_selected => {
                if self.ram_enabled {
                    b.prg_ram_read(addr)
                } else {
                    0
                }
            }
            0x6000..=0x7FFF => bank(&b.prg, 0x2000, self.prg[0], addr as usize),
            0x8000..=0xDFFF => bank(&b.prg, 0x2000, self.prg[1 + ((addr as usize - 0x8000) >> 13)], addr as usize),
            0xE000..=0xFFFF => bank(&b.prg, 0x2000, b.prg.len() / 0x2000 - 1, addr as usize),
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, v: u8) {
        match addr {
            0x6000..=0x7FFF if self.ram_selected && self.ram_enabled => self.base.prg_ram_write(addr, v),
            0x8000..=0x9FFF => self.cmd = v & 0xF,
            0xA000..=0xBFFF => match self.cmd {
                c @ 0..=7 => self.chr[c as usize] = v as usize,
                8 => {
                    self.prg[0] = (v & 0x3F) as usize;
                    self.ram_selected = v & 0x40 != 0;
                    self.ram_enabled = v & 0x80 != 0;
                }
                c @ 9..=0xB => self.prg[(c - 8) as usize] = (v & 0x3F) as usize,
                0xC => {
                    self.mirroring = match v & 3 {
                        0 => Mirroring::Vertical,
                        1 => Mirroring::Horizontal,
                        2 => Mirroring::SingleLower,
                        _ => Mirroring::SingleUpper,
                    }
                }
                0xD => {
                    self.irq_enabled = v & 1 != 0;
                    self.counter_enabled = v & 0x80 != 0;
                    self.irq = false;
                }
                0xE => self.counter = (self.counter & 0xFF00) | v as u16,
                _ => self.counter = (self.counter & 0x00FF) | (v as u16) << 8,
            },
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        let a = addr as usize & 0x1FFF;
        bank(&self.base.chr, 0x400, self.chr[a >> 10], a)
    }
    fn ppu_write(&mut self, addr: u16, v: u8) {
        self.base.chr_write(addr as usize & 0x1FFF, v);
    }
    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }
    fn irq(&self) -> bool {
        self.irq
    }
    fn tick(&mut self, cycles: u32) {
        if !self.counter_enabled {
            return;
        }
        let (n, wrapped) = self.counter.overflowing_sub(cycles as u16);
        self.counter = n;
        if wrapped && self.irq_enabled {
            self.irq = true;
        }
    }
}

// ------------------------------------------------------------ MMC5

/// Nintendo MMC5 (mapper 5: Castlevania III, Uncharted Waters, Just Breed...): four PRG and
/// CHR banking modes, separate sprite/background CHR sets for 8x16 sprites, per-quadrant
/// nametables including ExRAM and fill mode, the scanline IRQ and the hardware multiplier.
#[derive(state::Stateful)]
pub struct Mmc5 {
    base: Base,
    prg_ram: Vec<u8>,
    prg_mode: u8,
    chr_mode: u8,
    exram_mode: u8,
    nt: u8,
    fill_tile: u8,
    fill_attr: u8,
    prg: [u8; 5],
    chr_a: [usize; 8],
    chr_b: [usize; 4],
    chr_upper: usize,
    /// Last CHR set written (used for $2007 access outside rendering).
    last_b: bool,
    exram: Vec<u8>,
    irq_target: u8,
    irq_enabled: bool,
    irq_pending: bool,
    in_frame: bool,
    line: u32,
    mul: [u8; 2],
    fetching_sprites: bool,
    tall: bool,
}

impl Mmc5 {
    pub(crate) fn new(base: Base) -> Self {
        Mmc5 {
            base,
            prg_ram: vec![0; 0x10000],
            prg_mode: 3,
            chr_mode: 0,
            exram_mode: 0,
            nt: 0,
            fill_tile: 0,
            fill_attr: 0,
            prg: [0, 0, 0, 0, 0xFF],
            chr_a: [0; 8],
            chr_b: [0; 4],
            chr_upper: 0,
            last_b: false,
            exram: vec![0; 0x400],
            irq_target: 0,
            irq_enabled: false,
            irq_pending: false,
            in_frame: false,
            line: 0,
            mul: [0xFF, 0xFF],
            fetching_sprites: false,
            tall: false,
        }
    }

    /// Resolves a CPU address in $6000-$FFFF to (is ROM, byte offset).
    fn prg_addr(&self, addr: u16) -> (bool, usize) {
        let a = addr as usize;
        if a < 0x8000 {
            return (false, ((self.prg[0] & 0x0F) as usize) * 0x2000 + (a & 0x1FFF));
        }
        let (reg, size) = match (self.prg_mode, a) {
            (0, _) => (4, 0x8000),
            (1, 0x8000..=0xBFFF) => (2, 0x4000),
            (1, _) => (4, 0x4000),
            (2, 0x8000..=0xBFFF) => (2, 0x4000),
            (2, 0xC000..=0xDFFF) => (3, 0x2000),
            (2, _) => (4, 0x2000),
            (_, _) => (1 + ((a - 0x8000) >> 13), 0x2000),
        };
        let v = self.prg[reg] as usize;
        let rom = reg == 4 || v & 0x80 != 0;
        let unit = size / 0x2000;
        let n = (v & 0x7F) & !(unit - 1);
        (rom, n * 0x2000 + (a & (size - 1)))
    }

    fn chr_index(&self, addr: u16) -> usize {
        let a = addr as usize & 0x1FFF;
        let use_b = if self.tall { !self.fetching_sprites } else { false };
        let use_b = use_b || (!self.in_frame && self.last_b && self.tall);
        let (n, size) = if use_b {
            let a4 = a & 0xFFF;
            match self.chr_mode {
                0 => (self.chr_b[3], 0x2000),
                1 => (self.chr_b[3], 0x1000),
                2 => (self.chr_b[1 + ((a4 >> 11) << 1)], 0x800),
                _ => (self.chr_b[a4 >> 10], 0x400),
            }
        } else {
            match self.chr_mode {
                0 => (self.chr_a[7], 0x2000),
                1 => (self.chr_a[3 + ((a >> 12) << 2)], 0x1000),
                2 => (self.chr_a[1 + ((a >> 11) << 1)], 0x800),
                _ => (self.chr_a[a >> 10], 0x400),
            }
        };
        let len = self.base.chr.len().max(1);
        (n * size + (a & (size - 1))) % len
    }
}

impl Mapper for Mmc5 {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x5204 => {
                let v = (self.irq_pending as u8) << 7 | (self.in_frame as u8) << 6;
                self.irq_pending = false;
                v
            }
            0x5205 => (self.mul[0] as u16 * self.mul[1] as u16) as u8,
            0x5206 => ((self.mul[0] as u16 * self.mul[1] as u16) >> 8) as u8,
            0x5C00..=0x5FFF if self.exram_mode >= 2 => self.exram[(addr - 0x5C00) as usize],
            0x6000..=0xFFFF => {
                let (rom, off) = self.prg_addr(addr);
                if rom {
                    self.base.prg[off % self.base.prg.len()]
                } else {
                    self.prg_ram[off % self.prg_ram.len()]
                }
            }
            _ => 0,
        }
    }
    fn cpu_write(&mut self, addr: u16, v: u8) {
        match addr {
            0x5100 => self.prg_mode = v & 3,
            0x5101 => self.chr_mode = v & 3,
            0x5104 => self.exram_mode = v & 3,
            0x5105 => self.nt = v,
            0x5106 => self.fill_tile = v,
            0x5107 => self.fill_attr = v & 3,
            0x5113..=0x5117 => self.prg[(addr - 0x5113) as usize] = v,
            0x5120..=0x5127 => {
                self.chr_a[(addr - 0x5120) as usize] = v as usize | self.chr_upper << 8;
                self.last_b = false;
            }
            0x5128..=0x512B => {
                self.chr_b[(addr - 0x5128) as usize] = v as usize | self.chr_upper << 8;
                self.last_b = true;
            }
            0x5130 => self.chr_upper = (v & 3) as usize,
            0x5203 => self.irq_target = v,
            0x5204 => self.irq_enabled = v & 0x80 != 0,
            0x5205 => self.mul[0] = v,
            0x5206 => self.mul[1] = v,
            0x5C00..=0x5FFF if self.exram_mode != 3 => self.exram[(addr - 0x5C00) as usize] = v,
            0x6000..=0xFFFF => {
                let (rom, off) = self.prg_addr(addr);
                if !rom {
                    let len = self.prg_ram.len();
                    self.prg_ram[off % len] = v;
                }
            }
            _ => {}
        }
    }
    fn ppu_read(&mut self, addr: u16) -> u8 {
        let i = self.chr_index(addr);
        self.base.chr[i]
    }
    fn ppu_write(&mut self, addr: u16, v: u8) {
        let i = self.chr_index(addr);
        self.base.chr_write(i, v);
    }
    fn mirroring(&self) -> Mirroring {
        let q = |i: u8| (self.nt >> (i * 2)) & 3;
        Mirroring::Custom([q(0) & 1, q(1) & 1, q(2) & 1, q(3) & 1])
    }
    fn nt_read(&mut self, addr: u16) -> Option<u8> {
        let quadrant = ((addr >> 10) & 3) as u8;
        match (self.nt >> (quadrant * 2)) & 3 {
            2 if self.exram_mode < 2 => Some(self.exram[(addr & 0x3FF) as usize]),
            2 => Some(0),
            3 => Some(if addr & 0x3FF >= 0x3C0 { self.fill_attr * 0x55 } else { self.fill_tile }),
            _ => None,
        }
    }
    fn nt_write(&mut self, addr: u16, v: u8) -> bool {
        let quadrant = ((addr >> 10) & 3) as u8;
        match (self.nt >> (quadrant * 2)) & 3 {
            2 => {
                if self.exram_mode < 2 {
                    self.exram[(addr & 0x3FF) as usize] = v;
                }
                true
            }
            3 => true,
            _ => false,
        }
    }
    fn ppu_fetch(&mut self, sprites: bool, tall: bool) {
        self.fetching_sprites = sprites;
        self.tall = tall;
    }
    fn scanline(&mut self) {
        // Called once per rendered line (pre-render line first).
        if !self.in_frame {
            self.in_frame = true;
            self.line = 0;
            return;
        }
        self.line += 1;
        if self.line == self.irq_target as u32 && self.irq_target != 0 {
            self.irq_pending = true;
        }
        if self.line >= 240 {
            self.in_frame = false;
        }
    }
    fn irq(&self) -> bool {
        self.irq_pending && self.irq_enabled
    }
    fn save(&self) -> Option<Vec<u8>> {
        Some(self.prg_ram.clone())
    }
    fn load_save(&mut self, data: &[u8]) {
        if data.len() == self.prg_ram.len() {
            self.prg_ram.copy_from_slice(data);
        }
    }
}
