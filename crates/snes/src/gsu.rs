//! Super FX (GSU): the 16-bit RISC processor inside Star Fox, Yoshi's Island, Doom and
//! Stunt Race FX. It runs its own program from the cartridge ROM, draws into the cartridge
//! RAM, and the SNES then displays that RAM as ordinary tiles.
//!
//! Registers R0-R15 (R15 is the program counter), a status word, and a prefix scheme: TO,
//! FROM, WITH and ALT1-3 change what the next instruction does. Branches and jumps run the
//! one instruction after them first (a delay slot).

const Z: u16 = 1 << 1;
const CY: u16 = 1 << 2;
const S: u16 = 1 << 3;
const OV: u16 = 1 << 4;
const G: u16 = 1 << 5;
const ALT1: u16 = 1 << 8;
const ALT2: u16 = 1 << 9;
const B: u16 = 1 << 12;
const IRQ: u16 = 1 << 15;

#[derive(Default, state::Stateful)]
pub struct Gsu {
    r: [u16; 16],
    sfr: u16,
    pbr: u8,
    rombr: u8,
    rambr: u8,
    cbr: u16,
    scbr: u8,
    scmr: u8,
    colr: u8,
    por: u8,
    bramr: u8,
    cfgr: u8,
    clsr: u8,
    sreg: u8,
    dreg: u8,
    /// The byte already fetched for the next instruction.
    pipe: u8,
    ramaddr: u16,
    /// Set when an instruction wrote R15 (a jump), so it isn't stepped on afterwards.
    r15_set: bool,
    /// Cycles left over (negative = owed) between scanlines.
    credit: i32,
}

const BPP: [u32; 4] = [2, 4, 4, 8];

impl Gsu {
    pub fn irq(&self) -> bool {
        self.sfr & IRQ != 0
    }

    pub fn running(&self) -> bool {
        self.sfr & G != 0
    }

    // ------------------------------------------------------------ SNES-side registers

    pub fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x3000..=0x301F => {
                let r = self.r[((addr - 0x3000) >> 1) as usize];
                if addr & 1 == 0 { r as u8 } else { (r >> 8) as u8 }
            }
            0x3030 => self.sfr as u8,
            0x3031 => {
                let v = (self.sfr >> 8) as u8;
                self.sfr &= !IRQ;
                v
            }
            0x3034 => self.pbr,
            0x3036 => self.rombr,
            0x303B => 4,
            0x303C => self.rambr,
            0x303E => self.cbr as u8,
            0x303F => (self.cbr >> 8) as u8,
            _ => 0,
        }
    }

    pub fn write(&mut self, addr: u16, v: u8, rom: &[u8], ram: &[u8]) {
        match addr {
            0x3000..=0x301F => {
                let i = ((addr - 0x3000) >> 1) as usize;
                if addr & 1 == 0 {
                    self.r[i] = (self.r[i] & 0xFF00) | v as u16;
                } else {
                    self.r[i] = (self.r[i] & 0x00FF) | (v as u16) << 8;
                }
                if addr == 0x301F {
                    // Writing the program counter's high byte starts the processor.
                    self.sfr |= G;
                    self.start(rom, ram);
                }
            }
            0x3030 => {
                let was = self.sfr & G != 0;
                self.sfr = (self.sfr & !0x7E) | (v as u16 & 0x7E);
                if was && self.sfr & G == 0 {
                    self.cbr = 0;
                }
            }
            0x3031 => self.sfr = (self.sfr & 0x00FF) | (v as u16) << 8 & 0x9F00,
            0x3033 => self.bramr = v,
            0x3034 => self.pbr = v & 0x7F,
            0x3037 => self.cfgr = v,
            0x3038 => self.scbr = v,
            0x3039 => self.clsr = v & 1,
            0x303A => self.scmr = v,
            _ => {}
        }
    }

    fn start(&mut self, rom: &[u8], ram: &[u8]) {
        self.sfr &= !(ALT1 | ALT2 | B);
        self.sreg = 0;
        self.dreg = 0;
        self.pipe = self.fetch(rom, ram, self.pbr, self.r[15]);
        self.r[15] = self.r[15].wrapping_add(1);
    }

    // ------------------------------------------------------------ memory seen by the GSU

    fn fetch(&self, rom: &[u8], ram: &[u8], bank: u8, addr: u16) -> u8 {
        if bank < 0x40 {
            rom[(bank as usize * 0x8000 + (addr as usize & 0x7FFF)) % rom.len()]
        } else if bank < 0x70 {
            rom[((bank as usize - 0x40) * 0x10000 + addr as usize) % rom.len()]
        } else if !ram.is_empty() {
            ram[((((bank & 1) as usize) << 16) | addr as usize) % ram.len()]
        } else {
            0
        }
    }

    fn ram_read(&self, ram: &[u8], addr: u16) -> u8 {
        if ram.is_empty() {
            return 0;
        }
        ram[((((self.rambr & 1) as usize) << 16) | addr as usize) % ram.len()]
    }

    fn ram_write(&self, ram: &mut [u8], addr: u16, v: u8) {
        if ram.is_empty() {
            return;
        }
        let n = ram.len();
        ram[((((self.rambr & 1) as usize) << 16) | addr as usize) % n] = v;
    }

    fn ram_word(&self, ram: &[u8], addr: u16) -> u16 {
        self.ram_read(ram, addr) as u16 | (self.ram_read(ram, addr ^ 1) as u16) << 8
    }

    fn ram_set_word(&self, ram: &mut [u8], addr: u16, v: u16) {
        self.ram_write(ram, addr, v as u8);
        self.ram_write(ram, addr ^ 1, (v >> 8) as u8);
    }

    // ------------------------------------------------------------ instruction stream

    /// Next byte of the instruction stream (an operand), refilling the pipeline.
    fn operand(&mut self, rom: &[u8], ram: &[u8]) -> u8 {
        let v = self.pipe;
        self.r[15] = self.r[15].wrapping_add(1);
        self.pipe = self.fetch(rom, ram, self.pbr, self.r[15]);
        v
    }

    // ------------------------------------------------------------ flags and registers

    fn flag(&mut self, f: u16, on: bool) {
        if on {
            self.sfr |= f;
        } else {
            self.sfr &= !f;
        }
    }

    fn zs(&mut self, v: u16) {
        self.flag(Z, v == 0);
        self.flag(S, v & 0x8000 != 0);
    }

    fn src(&self) -> u16 {
        self.r[self.sreg as usize]
    }

    fn put(&mut self, v: u16) {
        let d = self.dreg as usize;
        self.r[d] = v;
        if d == 15 {
            self.r15_set = true;
        }
    }

    fn put_zs(&mut self, v: u16) {
        self.zs(v);
        self.put(v);
    }

    /// Clears the prefix state after an ordinary instruction.
    fn reset_prefix(&mut self) {
        self.sfr &= !(ALT1 | ALT2 | B);
        self.sreg = 0;
        self.dreg = 0;
    }

    fn set_color(&self, v: u8) -> u8 {
        if self.por & 4 != 0 {
            (self.colr & 0xF0) | (v >> 4)
        } else if self.por & 8 != 0 {
            (self.colr & 0xF0) | (v & 0x0F)
        } else {
            v
        }
    }

    // ------------------------------------------------------------ the pixel unit

    fn pixel_addr(&self, x: u16, y: u16) -> (usize, u32) {
        let (x, y) = (x as usize & 0xFF, y as usize & 0xFF);
        let ht = ((self.scmr >> 2) & 1) | ((self.scmr >> 4) & 2);
        let cn = match ht {
            0 => ((x & 0xF8) << 1) + ((y & 0xF8) >> 3),
            1 => ((x & 0xF8) << 1) + ((x & 0xF8) >> 1) + ((y & 0xF8) >> 3),
            2 => ((x & 0xF8) << 1) + (x & 0xF8) + ((y & 0xF8) >> 3),
            _ => ((y & 0x80) << 2) + ((x & 0x80) << 1) + ((y & 0x78) << 1) + ((x & 0x78) >> 3),
        };
        let bpp = BPP[(self.scmr & 3) as usize];
        (self.scbr as usize * 0x400 + cn * (bpp as usize * 8) + (y & 7) * 2, bpp)
    }

    fn plot(&mut self, ram: &mut [u8]) {
        let (x, y) = (self.r[1], self.r[2]);
        let md = self.scmr & 3;
        if self.por & 1 == 0 {
            let transparent = if md == 3 {
                if self.por & 8 != 0 { self.colr & 15 == 0 } else { self.colr == 0 }
            } else {
                self.colr & 15 == 0
            };
            if transparent {
                self.r[1] = x.wrapping_add(1);
                return;
            }
        }
        let mut color = self.colr;
        if self.por & 2 != 0 && md != 3 {
            color = if (x ^ y) & 1 != 0 { color >> 4 } else { color & 15 };
        }
        let (base, bpp) = self.pixel_addr(x, y);
        let bit = 7 - (x & 7);
        for p in 0..bpp as usize {
            let at = (base + ((p >> 1) << 4) + (p & 1)) % ram.len().max(1);
            if ram.is_empty() {
                break;
            }
            let mut byte = ram[at];
            byte = (byte & !(1 << bit)) | (((color >> p) & 1) << bit);
            ram[at] = byte;
        }
        self.r[1] = x.wrapping_add(1);
    }

    fn read_pixel(&self, ram: &[u8], x: u16, y: u16) -> u8 {
        if ram.is_empty() {
            return 0;
        }
        let (base, bpp) = self.pixel_addr(x, y);
        let bit = 7 - (x & 7);
        let mut c = 0u8;
        for p in 0..bpp as usize {
            let at = (base + ((p >> 1) << 4) + (p & 1)) % ram.len();
            c |= ((ram[at] >> bit) & 1) << p;
        }
        c
    }

    // ------------------------------------------------------------ running

    /// Runs the processor for about `budget` GSU clocks.
    pub fn run(&mut self, rom: &[u8], ram: &mut [u8], budget: i32) {
        if self.sfr & G == 0 {
            self.credit = 0;
            return;
        }
        self.credit += budget;
        while self.credit > 0 && self.sfr & G != 0 {
            let c = self.step(rom, ram);
            self.credit -= c;
        }
    }

    /// Clocks per scanline at the selected speed.
    pub fn clocks_per_line(&self) -> i32 {
        if self.clsr & 1 != 0 { 1364 } else { 682 }
    }

    fn branch(&mut self, rom: &[u8], ram: &[u8], take: bool) {
        let e = self.operand(rom, ram) as i8 as i16;
        if take {
            self.r[15] = self.r[15].wrapping_add(e as u16);
            self.r15_set = true;
        }
    }

    fn step(&mut self, rom: &[u8], ram: &mut [u8]) -> i32 {
        let op = self.pipe;
        self.pipe = self.fetch(rom, ram, self.pbr, self.r[15]);
        self.r15_set = false;
        let alt = (self.sfr >> 8) as u8 & 3;
        let n = (op & 15) as usize;
        let mut cost = 1;
        let mut prefix = false;
        match op {
            0x00 => {
                if self.cfgr & 0x80 == 0 {
                    self.sfr |= IRQ;
                }
                self.sfr &= !G;
            }
            0x01 => {}
            0x02 => self.cbr = self.r[15].wrapping_sub(1) & 0xFFF0,
            0x03 => {
                let a = self.src();
                self.flag(CY, a & 1 != 0);
                self.put_zs(a >> 1);
            }
            0x04 => {
                let a = self.src();
                let v = (a << 1) | (self.sfr & CY != 0) as u16;
                self.flag(CY, a & 0x8000 != 0);
                self.put_zs(v);
            }
            0x05..=0x0F => {
                let s = self.sfr & S != 0;
                let o = self.sfr & OV != 0;
                let z = self.sfr & Z != 0;
                let c = self.sfr & CY != 0;
                let take = match op {
                    0x05 => true,
                    0x06 => s == o,
                    0x07 => s != o,
                    0x08 => !z,
                    0x09 => z,
                    0x0A => !s,
                    0x0B => s,
                    0x0C => !c,
                    0x0D => c,
                    0x0E => !o,
                    _ => o,
                };
                self.branch(rom, ram, take);
            }
            0x10..=0x1F => {
                prefix = true;
                if self.sfr & B == 0 {
                    self.dreg = n as u8;
                } else {
                    // MOVE Rn,Rs
                    self.r[n] = self.src();
                    if n == 15 {
                        self.r15_set = true;
                    }
                    prefix = false;
                }
            }
            0x20..=0x2F => {
                prefix = true;
                self.sreg = n as u8;
                self.dreg = n as u8;
                self.sfr |= B;
            }
            0x30..=0x3B => {
                self.ramaddr = self.r[n];
                let v = self.src();
                if alt & 1 != 0 {
                    self.ram_write(ram, self.ramaddr, v as u8);
                } else {
                    self.ram_set_word(ram, self.ramaddr, v);
                }
                cost = 3;
            }
            0x3C => {
                self.r[12] = self.r[12].wrapping_sub(1);
                let v = self.r[12];
                self.zs(v);
                if v != 0 {
                    self.r[15] = self.r[13];
                    self.r15_set = true;
                }
            }
            0x3D..=0x3F => {
                prefix = true;
                self.sfr |= ((op - 0x3C) as u16) << 8 & (ALT1 | ALT2);
            }
            0x40..=0x4B => {
                self.ramaddr = self.r[n];
                let v = if alt & 1 != 0 { self.ram_read(ram, self.ramaddr) as u16 } else { self.ram_word(ram, self.ramaddr) };
                self.put(v);
                cost = 3;
            }
            0x4C => {
                if alt & 1 != 0 {
                    let v = self.read_pixel(ram, self.r[1], self.r[2]) as u16;
                    self.put_zs(v);
                } else {
                    self.plot(ram);
                }
            }
            0x4D => {
                let a = self.src();
                self.put_zs(a.rotate_left(8));
            }
            0x4E => {
                let a = self.src() as u8;
                if alt & 1 != 0 {
                    self.por = a & 0x1F;
                } else {
                    self.colr = self.set_color(a);
                }
            }
            0x4F => {
                let a = !self.src();
                self.put_zs(a);
            }
            0x50..=0x5F => {
                let a = self.src() as u32;
                let b = if alt & 2 != 0 { n as u32 } else { self.r[n] as u32 };
                let c = if alt & 1 != 0 { (self.sfr & CY != 0) as u32 } else { 0 };
                let r = a + b + c;
                self.flag(OV, !(a ^ b) & (a ^ r) & 0x8000 != 0);
                self.flag(CY, r > 0xFFFF);
                self.put_zs(r as u16);
            }
            0x60..=0x6F => {
                let a = self.src() as i32;
                let b = if alt == 2 { n as i32 } else { self.r[n] as i32 };
                let c = if alt == 1 { (self.sfr & CY == 0) as i32 } else { 0 };
                let r = a - b - c;
                self.flag(OV, (a ^ b) & (a ^ r) & 0x8000 != 0);
                self.flag(CY, r >= 0);
                if alt == 3 {
                    self.zs(r as u16);
                } else {
                    self.put_zs(r as u16);
                }
            }
            0x70 => {
                let v = (self.r[7] & 0xFF00) | (self.r[8] >> 8);
                self.flag(OV, v & 0xC0C0 != 0);
                self.flag(S, v & 0x8080 != 0);
                self.flag(CY, v & 0xE0E0 != 0);
                self.flag(Z, v & 0xF0F0 == 0);
                self.put(v);
            }
            0x71..=0x7F => {
                let a = self.src();
                let b = if alt & 2 != 0 { n as u16 } else { self.r[n] };
                self.put_zs(if alt & 1 != 0 { a & !b } else { a & b });
            }
            0x80..=0x8F => {
                let a = self.src();
                let b = if alt & 2 != 0 { n as u16 } else { self.r[n] };
                let v = if alt & 1 != 0 { (a & 0xFF) * (b & 0xFF) } else { ((a as i8 as i16) * (b as i8 as i16)) as u16 };
                self.put_zs(v);
                cost = 2;
            }
            0x90 => {
                let v = self.src();
                self.ram_set_word(ram, self.ramaddr, v);
                cost = 3;
            }
            0x91..=0x94 => self.r[11] = self.r[15].wrapping_add(n as u16),
            0x95 => {
                let a = self.src();
                self.put_zs(a as i8 as i16 as u16);
            }
            0x96 => {
                let a = self.src();
                self.flag(CY, a & 1 != 0);
                let v = if alt & 1 != 0 && a == 0xFFFF { 0 } else { ((a as i16) >> 1) as u16 };
                self.put_zs(v);
            }
            0x97 => {
                let a = self.src();
                let v = (a >> 1) | ((self.sfr & CY != 0) as u16) << 15;
                self.flag(CY, a & 1 != 0);
                self.put_zs(v);
            }
            0x98..=0x9D => {
                if alt & 1 != 0 {
                    self.pbr = (self.r[n] & 0x7F) as u8;
                    self.r[15] = self.src();
                    self.cbr = self.r[15] & 0xFFF0;
                } else {
                    self.r[15] = self.r[n];
                }
                self.r15_set = true;
            }
            0x9E => {
                let v = self.src() & 0xFF;
                self.flag(S, v & 0x80 != 0);
                self.flag(Z, v == 0);
                self.put(v);
            }
            0x9F => {
                let p = (self.src() as i16 as i32) * (self.r[6] as i16 as i32);
                if alt & 1 != 0 {
                    self.r[4] = p as u16;
                }
                self.flag(CY, p & 0x8000 != 0);
                self.put_zs((p >> 16) as u16);
                cost = 4;
            }
            0xA0..=0xAF => {
                let imm = self.operand(rom, ram);
                match alt {
                    1 => {
                        self.ramaddr = (imm as u16) << 1;
                        self.r[n] = self.ram_word(ram, self.ramaddr);
                        cost = 4;
                    }
                    2 => {
                        self.ramaddr = (imm as u16) << 1;
                        let v = self.r[n];
                        self.ram_set_word(ram, self.ramaddr, v);
                        cost = 4;
                    }
                    _ => self.r[n] = imm as i8 as i16 as u16,
                }
                if n == 15 {
                    self.r15_set = true;
                }
            }
            0xB0..=0xBF => {
                prefix = true;
                if self.sfr & B == 0 {
                    self.sreg = n as u8;
                } else {
                    // MOVES Rn: copies with flags.
                    let v = self.r[n];
                    self.flag(OV, v & 0x80 != 0);
                    self.zs(v);
                    self.put(v);
                    prefix = false;
                }
            }
            0xC0 => {
                let v = self.src() >> 8;
                self.flag(S, v & 0x80 != 0);
                self.flag(Z, v == 0);
                self.put(v);
            }
            0xC1..=0xCF => {
                let a = self.src();
                let b = if alt & 2 != 0 { n as u16 } else { self.r[n] };
                self.put_zs(if alt & 1 != 0 { a ^ b } else { a | b });
            }
            0xD0..=0xDE => {
                self.r[n] = self.r[n].wrapping_add(1);
                let v = self.r[n];
                self.zs(v);
                if n == 15 {
                    self.r15_set = true;
                }
            }
            0xDF => {
                if alt == 2 {
                    self.rambr = (self.src() & 1) as u8;
                } else if alt == 3 {
                    self.rombr = (self.src() & 0x7F) as u8;
                } else {
                    let b = self.fetch(rom, ram, self.rombr, self.r[14]);
                    self.colr = self.set_color(b);
                    cost = 3;
                }
            }
            0xE0..=0xEE => {
                self.r[n] = self.r[n].wrapping_sub(1);
                let v = self.r[n];
                self.zs(v);
                if n == 15 {
                    self.r15_set = true;
                }
            }
            0xEF => {
                let b = self.fetch(rom, ram, self.rombr, self.r[14]) as u16;
                let s = self.src();
                let v = match alt {
                    1 => (b << 8) | (s & 0xFF),
                    2 => (s & 0xFF00) | b,
                    3 => b as u8 as i8 as i16 as u16,
                    _ => b,
                };
                self.put(v);
                cost = 3;
            }
            _ => {
                // 0xF0..=0xFF: IWT / LM / SM.
                let lo = self.operand(rom, ram) as u16;
                let hi = self.operand(rom, ram) as u16;
                let imm = lo | hi << 8;
                match alt {
                    1 => {
                        self.ramaddr = imm;
                        self.r[n] = self.ram_word(ram, imm);
                        cost = 6;
                    }
                    2 => {
                        self.ramaddr = imm;
                        let v = self.r[n];
                        self.ram_set_word(ram, imm, v);
                        cost = 6;
                    }
                    _ => self.r[n] = imm,
                }
                if n == 15 {
                    self.r15_set = true;
                }
            }
        }
        if !prefix {
            self.reset_prefix();
        }
        if !self.r15_set {
            self.r[15] = self.r[15].wrapping_add(1);
        }
        cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program in "ROM" bank 0 at $8000; returns the finished processor and RAM.
    fn run(prog: &[u8]) -> (Gsu, Vec<u8>) {
        let mut rom = vec![0u8; 0x10000];
        rom[..prog.len()].copy_from_slice(prog);
        let mut ram = vec![0u8; 0x8000];
        let mut g = Gsu::default();
        g.r[15] = 0x8000;
        g.sfr |= G;
        g.start(&rom, &ram);
        g.run(&rom, &mut ram, 100_000);
        (g, ram)
    }

    #[test]
    fn arithmetic_and_flags() {
        // IBT R1,#5; IBT R2,#7; FROM R1; TO R3; ADD R2; STOP
        let (g, _) = run(&[0xA1, 5, 0xA2, 7, 0xB1, 0x13, 0x52, 0x00, 0x01]);
        assert_eq!(g.r[3], 12);
        assert!(!g.running());
        assert!(g.irq());
    }

    #[test]
    fn branches_run_the_delay_slot() {
        // IBT R1,#0; BRA +2 (skips the next two bytes after the delay slot); INC R1 (delay
        // slot, runs); INC R1; INC R1 (skipped); STOP
        let (g, _) = run(&[0xA1, 0, 0x05, 0x02, 0xD1, 0xD1, 0xD1, 0x00, 0x01]);
        assert_eq!(g.r[1], 2, "delay slot ran once, then the branch landed after the skipped INC");
    }

    #[test]
    fn loop_counts_down() {
        // IBT R12,#4; IBT R1,#0; MOVE R13,R15 (loop target); INC R1; LOOP; NOP; STOP
        let (g, _) = run(&[0xAC, 4, 0xA1, 0, 0x2F, 0x1D, 0xD1, 0x3C, 0x01, 0x00, 0x01]);
        assert_eq!(g.r[1], 4);
        assert_eq!(g.r[12], 0);
    }

    #[test]
    fn ram_round_trip_and_multiply() {
        // IBT R1,#-3; IBT R2,#5; FROM R1; TO R3; MULT R2; IWT R4,#$0100; FROM R3; STW (R4); STOP
        let (g, ram) = run(&[0xA1, 0xFD, 0xA2, 5, 0xB1, 0x13, 0x82, 0xF4, 0x00, 0x01, 0xB3, 0x34, 0x00, 0x01]);
        assert_eq!(g.r[3] as i16, -15);
        assert_eq!(u16::from_le_bytes([ram[0x100], ram[0x101]]) as i16, -15);
    }

    #[test]
    fn plot_draws_into_tile_memory() {
        // 4 colours (2bpp), 128 high: COLOR #3, plot at (1,0) then (2,0).
        // IBT R0,#3; COLOR; IBT R1,#1; IBT R2,#0; PLOT; PLOT; STOP
        let mut rom = vec![0u8; 0x10000];
        let prog = [0xA0, 3, 0x4E, 0xA1, 1, 0xA2, 0, 0x4C, 0x4C, 0x00, 0x01];
        rom[..prog.len()].copy_from_slice(&prog);
        let mut ram = vec![0u8; 0x8000];
        let mut g = Gsu::default();
        g.scmr = 0; // 2bpp, 128 high
        g.r[15] = 0x8000;
        g.sfr |= G;
        g.start(&rom, &ram);
        g.run(&rom, &mut ram, 100_000);
        // Pixels 1 and 2 of row 0, both planes: bits 6 and 5.
        assert_eq!(ram[0], 0b0110_0000);
        assert_eq!(ram[1], 0b0110_0000);
        assert_eq!(g.r[1], 3);
    }
}
