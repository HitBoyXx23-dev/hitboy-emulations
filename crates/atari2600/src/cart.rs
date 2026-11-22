//! Cartridges and their bank-switching schemes. Offsets are within the 4 KB window at $1000.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// 2 KB or 4 KB, no switching.
    Plain,
    /// Atari 8 KB / 16 KB / 32 KB: hotspots at the top of the window.
    F8,
    F6,
    F4,
    /// CBS RAM Plus: 12 KB + 256 bytes of RAM.
    FA,
    /// Parker Brothers: four 1 KB windows, the last fixed.
    E0,
    /// Tigervision: writes to $3F pick the lower 2 KB bank.
    Tigervision,
    /// Tigervision + RAM: writes to $3E put a 1 KB RAM bank in the lower window.
    T3E,
    /// Activision: bank chosen by the stack traffic of JSR/RTS (Robot Tank, Decathlon).
    FE,
    /// M-Network: 2 KB ROM slices, 1 KB + 4 x 256 bytes of RAM (BurgerTime).
    E7,
    /// UA Ltd: hotspots at $0220 / $0240.
    UA,
    /// Commavid: 2 KB ROM + 1 KB RAM.
    CV,
    /// Megaboy 64 KB: each access to $1FF0 steps to the next 4 KB bank.
    F0,
    /// 64 KB with hotspots $1FE0-$1FEF.
    EF,
    /// Pitfall II's Display Processor Chip: data fetchers, random numbers and music.
    Dpc,
}

/// DPC state (Pitfall II).
#[derive(Clone, Debug, Default, state::Stateful)]
struct Dpc {
    tops: [u8; 8],
    bottoms: [u8; 8],
    counters: [u16; 8],
    flags: [u8; 8],
    music: [bool; 3],
    random: u8,
    /// Oscillator phase in units of CPU cycles (the music clock runs at ~20 kHz).
    osc_acc: u64,
}

#[derive(state::Stateful)]
pub struct Cart {
    #[state(skip)]
    rom: Vec<u8>,
    #[state(skip)]
    pub scheme: Scheme,
    bank: usize,
    slices: [usize; 4],
    ram: Vec<u8>,
    /// Size of the extra RAM window (Superchip 128, CBS 256), read at +size, written at 0.
    #[state(skip)]
    ram_size: usize,
    /// FE: the previous bus access was to $01FE.
    fe_armed: bool,
    /// E7: lower window shows RAM; 256-byte RAM bank in the upper window.
    e7_ram_lower: bool,
    e7_ram_bank: usize,
    /// 3E: lower window shows RAM bank `bank` instead of ROM.
    t3e_ram: bool,
    dpc: Dpc,
}

fn contains(rom: &[u8], pat: &[u8]) -> usize {
    rom.windows(pat.len()).filter(|w| *w == pat).count()
}

fn any(rom: &[u8], pats: &[&[u8]]) -> bool {
    pats.iter().any(|p| contains(rom, p) > 0)
}

impl Cart {
    pub fn new(rom: &[u8]) -> Result<Cart, String> {
        let dpc = matches!(rom.len(), 10240 | 10495);
        if rom.is_empty() || (!dpc && (rom.len() % 2048 != 0 || rom.len() > 64 * 1024)) {
            return Err(format!("Not an Atari 2600 cartridge ({} bytes).", rom.len()));
        }
        let is_e0 = any(rom, &[&[0x8D, 0xE0, 0x1F], &[0x8D, 0xE0, 0x5F], &[0x8D, 0xE9, 0xFF], &[0x0C, 0xE0, 0x1F], &[0xAD, 0xE0, 0x1F], &[0xAD, 0xE9, 0xFF], &[0xAD, 0xED, 0xFF], &[0xAD, 0xF3, 0xBF]]);
        let is_3f = contains(rom, &[0x85, 0x3F]) >= 2;
        let is_3e = contains(rom, &[0x85, 0x3E, 0xA9, 0x00]) > 0;
        let is_fe = any(rom, &[&[0x20, 0x00, 0xD0, 0xC6, 0xC5], &[0x20, 0xC3, 0xF8, 0xA5, 0x82], &[0xD0, 0xFB, 0x20, 0x73, 0xFE], &[0x20, 0x00, 0xF0, 0x84, 0xD6]]);
        let is_ua = any(rom, &[&[0x8D, 0x40, 0x02], &[0xAD, 0x40, 0x02], &[0xBD, 0x1F, 0x02], &[0x2C, 0xC0, 0x02], &[0x8D, 0xC0, 0x02], &[0xAD, 0xC0, 0x02]]);
        let is_e7 = any(rom, &[&[0xAD, 0xE2, 0xFF], &[0xAD, 0xE5, 0xFF], &[0xAD, 0xE5, 0x1F], &[0xAD, 0xE7, 0x1F], &[0x0C, 0xE7, 0x1F], &[0x8D, 0xE7, 0xFF], &[0x8D, 0xE7, 0x1F]]);
        let is_cv = any(rom, &[&[0x9D, 0xFF, 0xF3], &[0x99, 0x00, 0xF4]]);
        let is_ef = any(rom, &[&[0x0C, 0xE0, 0xFF], &[0xAD, 0xE0, 0xFF], &[0x0C, 0xE0, 0x1F], &[0xAD, 0xE0, 0x1F]]);
        let scheme = match rom.len() {
            _ if dpc => Scheme::Dpc,
            2048 | 4096 if is_cv => Scheme::CV,
            2048 | 4096 => Scheme::Plain,
            8192 if is_e0 => Scheme::E0,
            8192 if is_3e => Scheme::T3E,
            8192 if is_3f => Scheme::Tigervision,
            8192 if is_fe => Scheme::FE,
            8192 if is_ua => Scheme::UA,
            8192 => Scheme::F8,
            12288 => Scheme::FA,
            16384 if is_e7 => Scheme::E7,
            16384 => Scheme::F6,
            32768 if is_3e => Scheme::T3E,
            32768 if is_3f => Scheme::Tigervision,
            32768 => Scheme::F4,
            65536 if is_ef => Scheme::EF,
            65536 if is_3e => Scheme::T3E,
            65536 if is_3f => Scheme::Tigervision,
            65536 => Scheme::F0,
            _ if is_3e => Scheme::T3E,
            _ if is_3f => Scheme::Tigervision,
            n => return Err(format!("This {} KB cartridge uses a bank-switching scheme we don't support yet.", n / 1024)),
        };
        // Superchip: the first 256 bytes of every bank are identical filler (RAM shadow).
        let banks = (rom.len() / 4096).max(1);
        let sc = matches!(scheme, Scheme::F8 | Scheme::F6 | Scheme::F4 | Scheme::EF) && (1..banks).all(|b| rom[b * 4096..b * 4096 + 256] == rom[..256]) && rom[..256].iter().all(|&x| x == rom[0]);
        let ram_size = match scheme {
            Scheme::FA => 256,
            _ if sc => 128,
            _ => 0,
        };
        let ram = match scheme {
            Scheme::E7 => vec![0; 2048],
            Scheme::CV => vec![0; 1024],
            Scheme::T3E => vec![0; 32 * 1024],
            _ => vec![0; ram_size],
        };
        let bank = match scheme {
            Scheme::FE | Scheme::UA | Scheme::E7 | Scheme::F0 | Scheme::EF | Scheme::T3E => 0,
            Scheme::Dpc => 1,
            _ => banks - 1,
        };
        let mut dpc_state = Dpc::default();
        dpc_state.random = 1;
        Ok(Cart { rom: rom.to_vec(), scheme, bank, slices: [4, 5, 6, 7], ram, ram_size, fe_armed: false, e7_ram_lower: false, e7_ram_bank: 0, t3e_ram: false, dpc: dpc_state })
    }

    fn hotspot(&mut self, off: usize) {
        let o = off & 0xFFF;
        match self.scheme {
            Scheme::F8 | Scheme::Dpc if (0xFF8..=0xFF9).contains(&o) => self.bank = o - 0xFF8,
            Scheme::F6 if (0xFF6..=0xFF9).contains(&o) => self.bank = o - 0xFF6,
            Scheme::F4 if (0xFF4..=0xFFB).contains(&o) => self.bank = o - 0xFF4,
            Scheme::FA if (0xFF8..=0xFFA).contains(&o) => self.bank = o - 0xFF8,
            Scheme::E0 if (0xFE0..=0xFF7).contains(&o) => self.slices[(o - 0xFE0) / 8] = o & 7,
            Scheme::EF if (0xFE0..=0xFEF).contains(&o) => self.bank = o - 0xFE0,
            Scheme::F0 if o == 0xFF0 => self.bank = (self.bank + 1) & 15,
            Scheme::E7 if (0xFE0..=0xFE6).contains(&o) => {
                self.bank = o - 0xFE0;
                self.e7_ram_lower = false;
            }
            Scheme::E7 if o == 0xFE7 => self.e7_ram_lower = true,
            Scheme::E7 if (0xFE8..=0xFEB).contains(&o) => self.e7_ram_bank = o - 0xFE8,
            _ => {}
        }
    }

    pub fn rom_len(&self) -> usize {
        self.rom.len()
    }

    pub fn read(&mut self, off: usize) -> u8 {
        let o = off & 0xFFF;
        if self.scheme == Scheme::Dpc && o < 0x40 {
            return self.dpc_read(o);
        }
        self.hotspot(o);
        if self.ram_size > 0 && o >= self.ram_size && o < self.ram_size * 2 {
            return self.ram[o - self.ram_size];
        }
        match self.scheme {
            Scheme::Plain => self.rom[o % self.rom.len()],
            Scheme::E0 => self.rom[self.slices[o >> 10] * 1024 + (o & 0x3FF)],
            Scheme::Tigervision | Scheme::T3E => {
                let banks = self.rom.len() / 2048;
                if o < 0x800 {
                    if self.t3e_ram {
                        // RAM bank: read at $1000-$13FF, written at $1400-$17FF.
                        return if o < 0x400 { self.ram[(self.bank * 1024 + o) % self.ram.len()] } else { 0 };
                    }
                    self.rom[(self.bank % banks) * 2048 + o]
                } else {
                    self.rom[(banks - 1) * 2048 + (o & 0x7FF)]
                }
            }
            Scheme::E7 => {
                let last = self.rom.len() / 2048 - 1;
                match o {
                    0x000..=0x3FF if self.e7_ram_lower => 0, // write port
                    0x400..=0x7FF if self.e7_ram_lower => self.ram[o - 0x400],
                    0x000..=0x7FF => self.rom[self.bank * 2048 + o],
                    0x800..=0x8FF => 0, // RAM write port
                    0x900..=0x9FF => self.ram[1024 + self.e7_ram_bank * 256 + (o - 0x900)],
                    _ => self.rom[last * 2048 + (o & 0x7FF)],
                }
            }
            Scheme::CV => match o {
                0x000..=0x3FF => self.ram[o],
                0x400..=0x7FF => 0,
                _ => self.rom[(self.rom.len() - 2048) + (o & 0x7FF)],
            },
            Scheme::FE => {
                // Bank 0 is the upper 4 KB of the image ($F000 code), bank 1 the lower.
                let b = if self.bank == 0 { 1 } else { 0 };
                self.rom[b * 4096 + o]
            }
            Scheme::Dpc => self.rom[self.bank * 4096 + o],
            _ => self.rom[(self.bank * 4096 + o) % self.rom.len()],
        }
    }

    pub fn write(&mut self, off: usize, v: u8) {
        let o = off & 0xFFF;
        if self.scheme == Scheme::Dpc && (0x40..0x80).contains(&o) {
            return self.dpc_write(o, v);
        }
        self.hotspot(o);
        if self.ram_size > 0 && o < self.ram_size {
            self.ram[o] = v;
        }
        match self.scheme {
            Scheme::E7 => match o {
                0x000..=0x3FF if self.e7_ram_lower => self.ram[o] = v,
                0x800..=0x8FF => self.ram[1024 + self.e7_ram_bank * 256 + o - 0x800] = v,
                _ => {}
            },
            Scheme::CV if (0x400..0x800).contains(&o) => self.ram[o - 0x400] = v,
            Scheme::T3E if self.t3e_ram && (0x400..0x800).contains(&o) => {
                let len = self.ram.len();
                self.ram[(self.bank * 1024 + o - 0x400) % len] = v;
            }
            _ => {}
        }
    }

    /// Tigervision bank select: a write to $00-$3F (the TIA range).
    pub fn tia_write(&mut self, addr: u16, v: u8) {
        let a = addr & 0x1FFF;
        match self.scheme {
            Scheme::Tigervision if a <= 0x3F => self.bank = v as usize,
            Scheme::T3E if a == 0x3F => {
                self.bank = v as usize;
                self.t3e_ram = false;
            }
            Scheme::T3E if a == 0x3E => {
                self.bank = v as usize;
                self.t3e_ram = true;
            }
            _ => {}
        }
    }

    /// Sees every bus access (FE watches the stack, UA its hotspots in the TIA/RIOT space).
    pub fn snoop(&mut self, addr: u16, v: u8) {
        let a = addr & 0x1FFF;
        match self.scheme {
            Scheme::FE => {
                if self.fe_armed {
                    self.bank = if v & 0x20 != 0 { 0 } else { 1 };
                    self.fe_armed = false;
                }
                if a & 0x1FFF == 0x01FE {
                    self.fe_armed = true;
                }
            }
            Scheme::UA if a & 0x1000 == 0 => match a & 0x2FF {
                0x220 => self.bank = 0,
                0x240 => self.bank = 1,
                _ => {}
            },
            _ => {}
        }
    }

    /// Advances cartridge hardware clocked by the CPU (the DPC music oscillator).
    pub fn tick(&mut self, cpu_cycles: u32) {
        if self.scheme != Scheme::Dpc {
            return;
        }
        // ~20 kHz oscillator from the 1.19 MHz CPU clock.
        self.dpc.osc_acc += cpu_cycles as u64 * 20_000;
        let clocks = self.dpc.osc_acc / 1_193_182;
        self.dpc.osc_acc %= 1_193_182;
        if clocks == 0 {
            return;
        }
        let d = &mut self.dpc;
        for x in 5..8 {
            if !d.music[x - 5] {
                continue;
            }
            let top = d.tops[x] as i64 + 1;
            let mut low = (d.counters[x] & 0xFF) as i64;
            if top != 0 {
                low -= clocks as i64 % top;
                if low < 0 {
                    low += top;
                }
            } else {
                low = 0;
            }
            if low <= d.bottoms[x] as i64 {
                d.flags[x] = 0;
            } else if low <= d.tops[x] as i64 {
                d.flags[x] = 0xFF;
            }
            d.counters[x] = (d.counters[x] & 0x700) | low as u16;
        }
    }

    fn dpc_read(&mut self, o: usize) -> u8 {
        let d = &mut self.dpc;
        // The random number generator steps on every DPC access.
        let r = d.random;
        let bit = !((r >> 7) ^ (r >> 5) ^ (r >> 4) ^ (r >> 3)) & 1;
        d.random = r << 1 | bit;
        let (index, function) = (o & 7, (o >> 3) & 7);
        let low = (d.counters[index] & 0xFF) as u8;
        if low == d.tops[index] {
            d.flags[index] = 0xFF;
        } else if low == d.bottoms[index] {
            d.flags[index] = 0;
        }
        let image = |d: &Dpc, rom: &[u8]| rom.get(8192 + 2047 - (d.counters[index] as usize & 0x7FF)).copied().unwrap_or(0);
        let v = match function {
            0 if index < 4 => d.random,
            0 => {
                let i = (d.music[0] && d.flags[5] != 0) as usize | ((d.music[1] && d.flags[6] != 0) as usize) << 1 | ((d.music[2] && d.flags[7] != 0) as usize) << 2;
                [0x00, 0x04, 0x05, 0x09, 0x06, 0x0A, 0x0B, 0x0F][i]
            }
            1 => image(d, &self.rom),
            2 => image(d, &self.rom) & d.flags[index],
            7 => d.flags[index],
            _ => 0,
        };
        if index < 5 || !d.music[index - 5] {
            d.counters[index] = d.counters[index].wrapping_sub(1) & 0x7FF;
        }
        v
    }

    fn dpc_write(&mut self, o: usize, v: u8) {
        let d = &mut self.dpc;
        let (index, function) = (o & 7, (o >> 3) & 7);
        match function {
            0 => {
                d.tops[index] = v;
                d.flags[index] = 0;
            }
            1 => d.bottoms[index] = v,
            2 => d.counters[index] = (d.counters[index] & 0x700) | v as u16,
            3 => {
                d.counters[index] = ((v as u16 & 7) << 8) | (d.counters[index] & 0xFF);
                if index >= 5 {
                    d.music[index - 5] = v & 0x10 != 0;
                }
            }
            6 => d.random = 1,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f8_hotspots_switch_banks() {
        let mut rom = vec![0u8; 8192];
        rom[0x123] = 0xAA;
        rom[0x1123] = 0xBB;
        rom[0x200] = 1; // not Superchip filler
        let mut c = Cart::new(&rom).unwrap();
        assert_eq!(c.scheme, Scheme::F8);
        assert_eq!(c.read(0x123), 0xBB, "starts in the last bank");
        c.read(0xFF8);
        assert_eq!(c.read(0x123), 0xAA);
    }

    #[test]
    fn superchip_ram() {
        let mut rom = vec![0xFFu8; 8192];
        rom[0x300] = 0; // real code after the RAM shadow
        let mut c = Cart::new(&rom).unwrap();
        c.write(0x05, 0x42);
        assert_eq!(c.read(0x85), 0x42);
    }

    #[test]
    fn e7_rom_slices_and_ram() {
        let mut rom = vec![0u8; 16384];
        rom[0x0AD..0x0B0].copy_from_slice(&[0xAD, 0xE5, 0xFF]); // signature
        rom[2 * 2048 + 5] = 0x22;
        let mut c = Cart::new(&rom).unwrap();
        assert_eq!(c.scheme, Scheme::E7);
        c.read(0xFE2);
        assert_eq!(c.read(5), 0x22);
        c.read(0xFE9); // 256-byte RAM bank 1
        c.write(0x810, 0x77);
        assert_eq!(c.read(0x910), 0x77);
        c.read(0xFE7); // 1 KB RAM in the lower window
        c.write(0x003, 0x55);
        assert_eq!(c.read(0x403), 0x55);
    }

    #[test]
    fn fe_follows_the_stack() {
        let mut rom = vec![0u8; 8192];
        rom[0x100..0x105].copy_from_slice(&[0x20, 0x00, 0xD0, 0xC6, 0xC5]);
        rom[0x1000] = 0xF0; // bank 0 = upper image half
        rom[0x0000] = 0xD0;
        let mut c = Cart::new(&rom).unwrap();
        assert_eq!(c.scheme, Scheme::FE);
        assert_eq!(c.read(0), 0xF0);
        c.snoop(0x01FE, 0);
        c.snoop(0x01FF, 0xD0); // return address high byte $D0: bit 5 clear -> bank 1
        assert_eq!(c.read(0), 0xD0);
    }

    #[test]
    fn dpc_fetcher_counts_down() {
        let mut rom = vec![0u8; 10240];
        rom[8192 + 2047 - 5] = 0x5A;
        let mut c = Cart::new(&rom).unwrap();
        assert_eq!(c.scheme, Scheme::Dpc);
        c.write(0x050, 5); // fetcher 0 counter low
        c.write(0x058, 0); // counter high
        assert_eq!(c.read(0x008), 0x5A);
    }
}
