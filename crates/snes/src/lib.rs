//! Super Nintendo. Main CPU and audio CPU are our C++ cores (65C816, SPC700);
//! the memory map, DMA/HDMA, PPU, DSP and timing are Rust.

pub mod apu;
pub mod cart;
mod gsu;
pub mod ppu;

use apu::Apu;
use cart::Cart;
use cpu65816::Cpu;
use ppu::Ppu;

pub use ppu::{HEIGHT, WIDTH};

/// SNES pad bits (as read from $4218/$4219 combined, high byte first).
pub mod button {
    pub const B: u16 = 1 << 15;
    pub const Y: u16 = 1 << 14;
    pub const SELECT: u16 = 1 << 13;
    pub const START: u16 = 1 << 12;
    pub const UP: u16 = 1 << 11;
    pub const DOWN: u16 = 1 << 10;
    pub const LEFT: u16 = 1 << 9;
    pub const RIGHT: u16 = 1 << 8;
    pub const A: u16 = 1 << 7;
    pub const X: u16 = 1 << 6;
    pub const L: u16 = 1 << 5;
    pub const R: u16 = 1 << 4;
}

const MASTER_PER_LINE: u32 = 1364;
const LINES: u16 = 262;
pub const FPS: f64 = 21_477_272.0 / (MASTER_PER_LINE as f64 * LINES as f64);

#[derive(Default, Clone, Copy, state::Stateful)]
struct Dma {
    dmap: u8,
    bbad: u8,
    a1t: u16,
    a1b: u8,
    das: u16,
    dasb: u8,
    a2a: u16,
    ntrl: u8,
    unused: u8,
    do_transfer: bool,
    terminated: bool,
}

/// B-bus register offsets for each DMA transfer mode.
const PATTERNS: [&[u8]; 8] = [&[0], &[0, 1], &[0, 0], &[0, 0, 1, 1], &[0, 1, 2, 3], &[0, 1, 0, 1], &[0, 0], &[0, 0, 1, 1]];

#[derive(state::Stateful)]
pub struct Bus {
    pub cart: Cart,
    wram: Box<[u8; 0x20000]>,
    wram_addr: u32,
    pub ppu: Ppu,
    pub apu: Apu,
    mdr: u8,

    nmitimen: u8,
    htime: u16,
    vtime: u16,
    wrmpya: u8,
    wrdiv: u16,
    rddiv: u16,
    rdmpy: u16,
    memsel: bool,
    wrio: u8,
    nmi_flag: bool,
    irq_flag: bool,
    pub nmi_pending: bool,

    #[state(skip)]
    pub pads: [u16; 2],
    auto_joy: [u16; 2],
    joy_latch: bool,
    joy_shift: [u16; 2],

    dma: [Dma; 8],
    hdma_enable: u8,

    hclock: u32,
    pub vcounter: u16,
    pub frame_ready: bool,
    apu_owed: u32,
}

impl Bus {
    fn new(cart: Cart) -> Self {
        Self {
            memsel: false,
            cart,
            wram: Box::new([0; 0x20000]),
            wram_addr: 0,
            ppu: Ppu::default(),
            apu: Apu::default(),
            mdr: 0,
            nmitimen: 0,
            htime: 0x1FF,
            vtime: 0x1FF,
            wrmpya: 0xFF,
            wrdiv: 0xFFFF,
            rddiv: 0,
            rdmpy: 0,
            wrio: 0xFF,
            nmi_flag: false,
            irq_flag: false,
            nmi_pending: false,
            pads: [0; 2],
            auto_joy: [0; 2],
            joy_latch: false,
            joy_shift: [0; 2],
            dma: [Dma { dmap: 0xFF, bbad: 0xFF, a1t: 0xFFFF, a1b: 0xFF, das: 0xFFFF, dasb: 0xFF, a2a: 0xFFFF, ntrl: 0xFF, unused: 0xFF, ..Dma::default() }; 8],
            hdma_enable: 0,
            hclock: 0,
            vcounter: 0,
            frame_ready: false,
            apu_owed: 0,
        }
    }

    /// Master clocks for one CPU access to `addr` (the SNES has 6/8/12-cycle regions).
    #[inline]
    fn speed(&self, addr: u32) -> u32 {
        let bank = (addr >> 16) as u8;
        let off = addr as u16;
        if bank & 0x40 == 0 {
            match off {
                0x0000..=0x1FFF => 8,
                0x2000..=0x3FFF => 6,
                0x4000..=0x41FF => 12,
                0x4200..=0x5FFF => 6,
                0x6000..=0x7FFF => 8,
                _ => if bank & 0x80 != 0 && self.memsel { 6 } else { 8 },
            }
        } else if bank >= 0xC0 && self.memsel {
            6
        } else {
            8
        }
    }

    /// Advances the machine clock: raster position, IRQ timers, HDMA, rendering, audio.
    fn advance(&mut self, cycles: u32) {
        let before = self.hclock;
        self.hclock += cycles;
        // H-IRQ (optionally only on line VTIME).
        let hv = (self.nmitimen >> 4) & 3;
        if hv & 1 != 0 {
            let ht = self.htime as u32 * 4;
            if before < ht && self.hclock >= ht && (hv == 1 || self.vcounter == self.vtime) {
                self.irq_flag = true;
            }
        }
        while self.hclock >= MASTER_PER_LINE {
            self.hclock -= MASTER_PER_LINE;
            self.end_line();
        }
    }

    fn end_line(&mut self) {
        if (1..=HEIGHT as u16).contains(&self.vcounter) {
            self.ppu.render_line(self.vcounter);
        }
        self.vcounter += 1;
        if self.vcounter == HEIGHT as u16 + 1 {
            // V-Blank.
            self.ppu.vblank_start();
            self.nmi_flag = true;
            if self.nmitimen & 0x80 != 0 {
                self.nmi_pending = true;
            }
            if self.nmitimen & 0x01 != 0 {
                self.auto_joy = self.pads;
            }
            self.frame_ready = true;
        }
        if self.vcounter == LINES {
            self.vcounter = 0;
            self.nmi_flag = false;
            self.hdma_init();
        }
        if (self.vcounter as usize) <= HEIGHT {
            self.hdma_line();
        }
        if (self.nmitimen >> 4) & 3 == 2 && self.vcounter == self.vtime {
            self.irq_flag = true;
        }
        self.cart.run_coprocessor_line();
        // Run the audio CPU once per line; it talks to us through latched ports.
        self.apu_owed += MASTER_PER_LINE;
        if self.apu_owed >= MASTER_PER_LINE {
            self.apu.advance(self.apu_owed);
            self.apu_owed = 0;
        }
    }

    // ------------------------------------------------------------ plain memory access

    /// A-bus access without timing (used by DMA and the timed CPU path).
    fn read_a(&mut self, addr: u32) -> u8 {
        let bank = (addr >> 16) as u8;
        let off = addr as u16;
        if bank == 0x7E || bank == 0x7F {
            return self.wram[(addr as usize) & 0x1FFFF];
        }
        if bank & 0x40 == 0 {
            match off {
                0x0000..=0x1FFF => return self.wram[off as usize],
                0x2100..=0x21FF => return self.read_b(off as u8),
                0x4016 => {
                    let v = (self.joy_shift[0] >> 15) as u8 & 1;
                    self.joy_shift[0] = self.joy_shift[0] << 1 | 1;
                    return (self.mdr & 0xFC) | v;
                }
                0x4017 => {
                    let v = (self.joy_shift[1] >> 15) as u8 & 1;
                    self.joy_shift[1] = self.joy_shift[1] << 1 | 1;
                    return (self.mdr & 0xE0) | 0x1C | v;
                }
                0x4200..=0x421F => return self.read_cpu_reg(off),
                0x4300..=0x437F => return self.read_dma_reg(off),
                _ => {}
            }
        }
        self.cart.read(bank, off).unwrap_or(self.mdr)
    }

    fn write_a(&mut self, addr: u32, v: u8) {
        let bank = (addr >> 16) as u8;
        let off = addr as u16;
        if bank == 0x7E || bank == 0x7F {
            self.wram[(addr as usize) & 0x1FFFF] = v;
            return;
        }
        if bank & 0x40 == 0 {
            match off {
                0x0000..=0x1FFF => {
                    self.wram[off as usize] = v;
                    return;
                }
                0x2100..=0x21FF => {
                    self.write_b(off as u8, v);
                    return;
                }
                0x4016 => {
                    let latch = v & 1 != 0;
                    if latch {
                        self.joy_shift = self.pads;
                    }
                    self.joy_latch = latch;
                    return;
                }
                0x4200..=0x421F => {
                    self.write_cpu_reg(off, v);
                    return;
                }
                0x4300..=0x437F => {
                    self.write_dma_reg(off, v);
                    return;
                }
                _ => {}
            }
        }
        self.cart.write(bank, off, v);
    }

    /// B-bus ($21xx): PPU, APU ports and the WRAM port.
    fn read_b(&mut self, reg: u8) -> u8 {
        match reg {
            0x37 => {
                self.ppu.latch_h = (self.hclock / 4) as u16;
                self.ppu.latch_v = self.vcounter;
                self.ppu.counters_latched = true;
                self.mdr
            }
            0x00..=0x3F => self.ppu.read(reg, self.mdr),
            0x40..=0x7F => self.apu.cpu_read((reg & 3) as usize),
            0x80 => {
                let v = self.wram[self.wram_addr as usize];
                self.wram_addr = (self.wram_addr + 1) & 0x1FFFF;
                v
            }
            _ => self.mdr,
        }
    }

    fn write_b(&mut self, reg: u8, v: u8) {
        match reg {
            0x00..=0x3F => self.ppu.write(reg, v),
            0x40..=0x7F => self.apu.cpu_write((reg & 3) as usize, v),
            0x80 => {
                self.wram[self.wram_addr as usize] = v;
                self.wram_addr = (self.wram_addr + 1) & 0x1FFFF;
            }
            0x81 => self.wram_addr = (self.wram_addr & 0x1FF00) | v as u32,
            0x82 => self.wram_addr = (self.wram_addr & 0x100FF) | (v as u32) << 8,
            0x83 => self.wram_addr = (self.wram_addr & 0x0FFFF) | ((v as u32 & 1) << 16),
            _ => {}
        }
    }

    // ------------------------------------------------------------ CPU registers

    fn read_cpu_reg(&mut self, off: u16) -> u8 {
        match off {
            0x4210 => {
                let v = (self.nmi_flag as u8) << 7 | 0x02 | (self.mdr & 0x70);
                self.nmi_flag = false;
                v
            }
            0x4211 => {
                let v = (self.irq_flag as u8) << 7 | (self.mdr & 0x7F);
                self.irq_flag = false;
                v
            }
            0x4212 => {
                let vblank = self.vcounter > HEIGHT as u16;
                let hblank = self.hclock >= 1096 || self.hclock < 4;
                (vblank as u8) << 7 | (hblank as u8) << 6 | (self.mdr & 0x3E)
            }
            0x4213 => self.wrio,
            0x4214 => self.rddiv as u8,
            0x4215 => (self.rddiv >> 8) as u8,
            0x4216 => self.rdmpy as u8,
            0x4217 => (self.rdmpy >> 8) as u8,
            0x4218 => self.auto_joy[0] as u8,
            0x4219 => (self.auto_joy[0] >> 8) as u8,
            0x421A => self.auto_joy[1] as u8,
            0x421B => (self.auto_joy[1] >> 8) as u8,
            0x421C..=0x421F => 0,
            _ => self.mdr,
        }
    }

    fn write_cpu_reg(&mut self, off: u16, v: u8) {
        match off {
            0x4200 => {
                // Enabling NMI during V-Blank with the flag set fires immediately.
                if v & 0x80 != 0 && self.nmitimen & 0x80 == 0 && self.nmi_flag {
                    self.nmi_pending = true;
                }
                self.nmitimen = v;
                if (v >> 4) & 3 == 0 {
                    self.irq_flag = false;
                }
            }
            0x4201 => self.wrio = v,
            0x4202 => self.wrmpya = v,
            0x4203 => {
                self.rdmpy = self.wrmpya as u16 * v as u16;
                self.rddiv = v as u16;
            }
            0x4204 => self.wrdiv = (self.wrdiv & 0xFF00) | v as u16,
            0x4205 => self.wrdiv = (self.wrdiv & 0x00FF) | (v as u16) << 8,
            0x4206 => {
                if v == 0 {
                    self.rddiv = 0xFFFF;
                    self.rdmpy = self.wrdiv;
                } else {
                    self.rddiv = self.wrdiv / v as u16;
                    self.rdmpy = self.wrdiv % v as u16;
                }
            }
            0x4207 => self.htime = (self.htime & 0x100) | v as u16,
            0x4208 => self.htime = (self.htime & 0xFF) | ((v as u16 & 1) << 8),
            0x4209 => self.vtime = (self.vtime & 0x100) | v as u16,
            0x420A => self.vtime = (self.vtime & 0xFF) | ((v as u16 & 1) << 8),
            0x420B => self.run_dma(v),
            0x420C => self.hdma_enable = v,
            0x420D => self.memsel = v & 1 != 0,
            _ => {}
        }
    }

    fn read_dma_reg(&self, off: u16) -> u8 {
        let d = &self.dma[((off >> 4) & 7) as usize];
        match off & 0x0F {
            0 => d.dmap,
            1 => d.bbad,
            2 => d.a1t as u8,
            3 => (d.a1t >> 8) as u8,
            4 => d.a1b,
            5 => d.das as u8,
            6 => (d.das >> 8) as u8,
            7 => d.dasb,
            8 => d.a2a as u8,
            9 => (d.a2a >> 8) as u8,
            0xA => d.ntrl,
            0xB | 0xF => d.unused,
            _ => self.mdr,
        }
    }

    fn write_dma_reg(&mut self, off: u16, v: u8) {
        let d = &mut self.dma[((off >> 4) & 7) as usize];
        match off & 0x0F {
            0 => d.dmap = v,
            1 => d.bbad = v,
            2 => d.a1t = (d.a1t & 0xFF00) | v as u16,
            3 => d.a1t = (d.a1t & 0x00FF) | (v as u16) << 8,
            4 => d.a1b = v,
            5 => d.das = (d.das & 0xFF00) | v as u16,
            6 => d.das = (d.das & 0x00FF) | (v as u16) << 8,
            7 => d.dasb = v,
            8 => d.a2a = (d.a2a & 0xFF00) | v as u16,
            9 => d.a2a = (d.a2a & 0x00FF) | (v as u16) << 8,
            0xA => d.ntrl = v,
            0xB | 0xF => d.unused = v,
            _ => {}
        }
    }

    // ------------------------------------------------------------ DMA / HDMA

    fn run_dma(&mut self, channels: u8) {
        self.advance(8);
        for ch in 0..8 {
            if channels & (1 << ch) == 0 {
                continue;
            }
            self.advance(8);
            let d = self.dma[ch];
            let pattern = PATTERNS[(d.dmap & 7) as usize];
            let to_a = d.dmap & 0x80 != 0;
            let step: i32 = match (d.dmap >> 3) & 3 {
                0 => 1,
                2 => -1,
                _ => 0,
            };
            let mut count = if d.das == 0 { 0x10000u32 } else { d.das as u32 };
            let mut addr = d.a1t;
            let mut i = 0;
            while count > 0 {
                let b = d.bbad.wrapping_add(pattern[i % pattern.len()]);
                let a = (d.a1b as u32) << 16 | addr as u32;
                if to_a {
                    let v = self.read_b(b);
                    self.write_a(a, v);
                } else {
                    let v = self.read_a(a);
                    self.write_b(b, v);
                }
                addr = (addr as i32 + step) as u16;
                self.advance(8);
                count -= 1;
                i += 1;
            }
            self.dma[ch].a1t = addr;
            self.dma[ch].das = 0;
        }
    }

    fn hdma_read_table(&mut self, ch: usize) -> u8 {
        let d = self.dma[ch];
        let v = self.read_a((d.a1b as u32) << 16 | d.a2a as u32);
        self.dma[ch].a2a = d.a2a.wrapping_add(1);
        v
    }

    fn hdma_load_entry(&mut self, ch: usize) {
        let ntrl = self.hdma_read_table(ch);
        self.dma[ch].ntrl = ntrl;
        self.dma[ch].terminated = ntrl == 0;
        if self.dma[ch].dmap & 0x40 != 0 {
            let lo = self.hdma_read_table(ch) as u16;
            let hi = self.hdma_read_table(ch) as u16;
            self.dma[ch].das = lo | hi << 8;
        }
        self.dma[ch].do_transfer = true;
    }

    fn hdma_init(&mut self) {
        for ch in 0..8 {
            if self.hdma_enable & (1 << ch) != 0 {
                self.dma[ch].a2a = self.dma[ch].a1t;
                self.hdma_load_entry(ch);
            }
        }
    }

    fn hdma_line(&mut self) {
        for ch in 0..8 {
            if self.hdma_enable & (1 << ch) == 0 || self.dma[ch].terminated {
                continue;
            }
            let d = self.dma[ch];
            if d.do_transfer {
                let pattern = PATTERNS[(d.dmap & 7) as usize];
                for &off in pattern {
                    let v = if d.dmap & 0x40 != 0 {
                        let a = (self.dma[ch].dasb as u32) << 16 | self.dma[ch].das as u32;
                        self.dma[ch].das = self.dma[ch].das.wrapping_add(1);
                        self.read_a(a)
                    } else {
                        self.hdma_read_table(ch)
                    };
                    self.write_b(d.bbad.wrapping_add(off), v);
                }
            }
            // The line counter decrements as a whole byte; "repeat" is the new value's bit 7.
            // So $80 means 128 lines written once, and $85 means 5 lines written every line.
            let n = self.dma[ch].ntrl.wrapping_sub(1);
            self.dma[ch].ntrl = n;
            self.dma[ch].do_transfer = n & 0x80 != 0;
            if n & 0x7F == 0 {
                self.hdma_load_entry(ch);
            }
        }
    }
}

impl cpu65816::Bus for Bus {
    #[inline]
    fn read(&mut self, addr: u32) -> u8 {
        let c = self.speed(addr);
        self.advance(c);
        let v = self.read_a(addr);
        self.mdr = v;
        v
    }

    #[inline]
    fn write(&mut self, addr: u32, value: u8) {
        let c = self.speed(addr);
        self.advance(c);
        self.mdr = value;
        self.write_a(addr, value);
    }

    #[inline]
    fn idle(&mut self) {
        self.advance(6);
    }
}

pub struct Snes {
    cpu: Cpu<Bus>,
}

impl state::Stateful for Snes {
    fn sync(&mut self, s: &mut state::State) {
        s.check(0x534E_4500 ^ self.cpu.bus().cart.rom_len() as u32);
        self.cpu.sync_state(s);
        self.cpu.bus_mut().sync(s);
    }
}

impl Snes {
    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    pub fn from_rom(rom: Vec<u8>) -> Result<Self, String> {
        let cart = Cart::load(rom)?;
        let mut cpu = Cpu::new(Bus::new(cart));
        cpu.reset();
        Ok(Self { cpu })
    }

    pub fn title(&self) -> &str {
        &self.cpu.bus().cart.title
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    pub fn run_frame(&mut self) {
        self.cpu.bus_mut().frame_ready = false;
        // Bounded so a crashed game can't hang the host.
        for _ in 0..2_000_000 {
            self.cpu.step();
            let bus = self.cpu.bus_mut();
            if bus.nmi_pending {
                bus.nmi_pending = false;
                self.cpu.nmi();
            }
            let irq = {
                let b = self.cpu.bus();
                (b.irq_flag && (b.nmitimen & 0x30) != 0) || b.cart.irq()
            };
            self.cpu.set_irq(irq);
            if self.cpu.bus().frame_ready {
                break;
            }
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.cpu.bus().ppu.frame
    }

    pub fn set_buttons(&mut self, pad: usize, buttons: u16) {
        self.cpu.bus_mut().pads[pad & 1] = buttons;
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.cpu.bus_mut().apu.samples);
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.cpu.bus_mut().apu.out_rate = rate;
    }

    pub fn save_ram(&self) -> Option<&[u8]> {
        let s = &self.cpu.bus().cart.sram;
        (!s.is_empty()).then_some(s.as_slice())
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        let s = &mut self.cpu.bus_mut().cart.sram;
        let n = s.len().min(data.len());
        s[..n].copy_from_slice(&data[..n]);
    }

    /// Reads WRAM (debugging / tests).
    pub fn peek_wram(&self, addr: usize) -> u8 {
        self.cpu.bus().wram[addr & 0x1FFFF]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A LoROM test cartridge (hand-assembled, emulation mode):
    /// draws a solid red BG1 via VRAM writes + DMA, uploads a tiny program to the
    /// SPC700 through the boot protocol, waits for its reply, then enables NMI.
    fn test_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        let code: &[u8] = &[
            0x78, 0xA9, 0x80, 0x8D, 0x00, 0x21, // sei; forced blank
            0x9C, 0x21, 0x21, 0x9C, 0x22, 0x21, 0x9C, 0x22, 0x21, // CGRAM[0] = black
            0xA9, 0x1F, 0x8D, 0x22, 0x21, 0x9C, 0x22, 0x21, // CGRAM[1] = red
            0xA9, 0x01, 0x8D, 0x05, 0x21, // mode 1
            0xA9, 0x04, 0x8D, 0x07, 0x21, // BG1 map at $0400
            0x9C, 0x0B, 0x21, // BG1 chars at 0
            0xA9, 0x80, 0x8D, 0x15, 0x21, // VRAM increment after high byte
            0xA9, 0x00, 0x8D, 0x16, 0x21, 0x9C, 0x17, 0x21, // VRAM addr $0000 = tile 0
            0xA2, 0x08, // ldx #8
            0xA9, 0xFF, 0x8D, 0x18, 0x21, 0x9C, 0x19, 0x21, 0xCA, 0xD0, 0xF5, // 8 rows of plane0 = $FF
            0x9C, 0x16, 0x21, 0xA9, 0x04, 0x8D, 0x17, 0x21, // VRAM addr $0400
            0xA9, 0x09, 0x8D, 0x00, 0x43, // DMA: fixed source, mode 1
            0xA9, 0x18, 0x8D, 0x01, 0x43, // -> $2118/$2119
            0xA9, 0xF0, 0x8D, 0x02, 0x43, 0xA9, 0x80, 0x8D, 0x03, 0x43, 0x9C, 0x04, 0x43, // source $00:80F0
            0x9C, 0x05, 0x43, 0xA9, 0x08, 0x8D, 0x06, 0x43, // $0800 bytes
            0xA9, 0x01, 0x8D, 0x0B, 0x42, // start DMA channel 0
            0xA9, 0x01, 0x8D, 0x2C, 0x21, // main screen: BG1
            0xA9, 0x0F, 0x8D, 0x00, 0x21, // display on, full brightness
            // --- SPC700 upload
            0xAD, 0x40, 0x21, 0xC9, 0xAA, 0xD0, 0xF9, // wait $AA
            0xAD, 0x41, 0x21, 0xC9, 0xBB, 0xD0, 0xF2, // wait $BB
            0x9C, 0x42, 0x21, 0xA9, 0x02, 0x8D, 0x43, 0x21, // dest $0200
            0xA9, 0x01, 0x8D, 0x41, 0x21, 0xA9, 0xCC, 0x8D, 0x40, 0x21, // kick + $CC
            0xCD, 0x40, 0x21, 0xD0, 0xFB, // wait echo
            0xA0, 0x00, // ldy #0
            0xB9, 0xE0, 0x80, 0x8D, 0x41, 0x21, 0x98, 0x8D, 0x40, 0x21, // send byte y
            0xCD, 0x40, 0x21, 0xD0, 0xFB, // wait echo
            0xC8, 0xC0, 0x05, 0xD0, 0xEC, // 5 bytes
            0x9C, 0x42, 0x21, 0xA9, 0x02, 0x8D, 0x43, 0x21, 0x9C, 0x41, 0x21, // entry $0200, kick 0 = run
            0xA9, 0x07, 0x8D, 0x40, 0x21, // command
            0xAD, 0x40, 0x21, 0xC9, 0x55, 0xD0, 0xF9, // wait for the program's $55
            0xA9, 0x01, 0x85, 0x20, // $20 = 1: audio CPU answered
            0xA9, 0x80, 0x8D, 0x00, 0x42, // enable NMI
            0x80, 0xFE, // loop forever
            0xE6, 0x10, 0x40, // NMI handler at $80D1: inc $10; rti
        ];
        rom[..code.len()].copy_from_slice(code);
        assert_eq!(code.len(), 0xD4, "code layout drifted");
        rom[0xE0..0xE5].copy_from_slice(&[0x8F, 0x55, 0xF4, 0x2F, 0xFE]); // SPC: mov $F4,#$55; bra *
        rom[0xF0..0xF2].copy_from_slice(&[0x00, 0x00]); // tilemap entry: tile 0 (fixed-source DMA sends this byte to both halves)
        rom[0x7FC0..0x7FC9].copy_from_slice(b"TEST CART");
        rom[0x7FD5] = 0x20; // LoROM
        rom[0x7FDC..0x7FE0].copy_from_slice(&[0xFF, 0xFF, 0x00, 0x00]); // complement + checksum
        rom[0x7FFA..0x7FFC].copy_from_slice(&[0xD1, 0x80]); // NMI (emulation)
        rom[0x7FFC..0x7FFE].copy_from_slice(&[0x00, 0x80]); // RESET
        rom
    }

    #[test]
    fn boots_draws_talks_to_apu_and_takes_nmi() {
        let mut snes = Snes::from_rom(test_rom()).unwrap();
        assert_eq!(snes.title(), "TEST CART");
        for _ in 0..10 {
            snes.run_frame();
        }
        assert_eq!(snes.peek_wram(0x20), 1, "SPC700 ran the uploaded program");
        assert!(snes.peek_wram(0x10) >= 3, "NMIs: {}", snes.peek_wram(0x10));
        let f = snes.frame();
        assert_eq!(f[0], 0xFF0000, "top-left is red");
        assert_eq!(f[WIDTH * HEIGHT - 1], 0xFF0000, "bottom-right is red");
    }

    /// After a restore the game continues exactly as if it had never stopped, including the
    /// sound CPU's program and the picture.
    #[test]
    fn save_state_resumes_identically() {
        let mut a = Snes::from_rom(test_rom()).unwrap();
        for _ in 0..3 {
            a.run_frame();
        }
        let snap = a.save_state();
        for _ in 0..8 {
            a.run_frame();
        }
        let want = (a.peek_wram(0x10), a.peek_wram(0x20), a.frame().to_vec());
        assert_eq!(want.1, 1, "the sound CPU answered");
        let mut b = Snes::from_rom(test_rom()).unwrap();
        b.load_state(&snap).unwrap();
        for _ in 0..8 {
            b.run_frame();
        }
        assert_eq!((b.peek_wram(0x10), b.peek_wram(0x20), b.frame().to_vec()), want);
    }

    /// Diagnostic: SNES_ROM=path cargo test -p snes diag -- --ignored --nocapture
    #[test]
    #[ignore]
    fn diag() {
        let rom = std::fs::read(std::env::var("SNES_ROM").unwrap()).unwrap();
        let mut snes = Snes::from_rom(rom).unwrap();
        for f in 0..900 {
            if f % 120 == 60 {
                snes.set_buttons(0, button::START);
            } else {
                snes.set_buttons(0, 0);
            }
            snes.cpu.bus_mut().ppu.line_log.clear();
            snes.run_frame();
            if f % 150 == 149 {
                println!("  ppu: {}", snes.cpu.bus().ppu.debug_state());
                let log = std::mem::take(&mut snes.cpu.bus_mut().ppu.line_log);
                let tail = &log[log.len().saturating_sub(224)..];
                let mut runs: Vec<(u8, u8, usize)> = Vec::new();
                for &(m, tm) in tail {
                    match runs.last_mut() {
                        Some(r) if r.0 == m && r.1 == tm => r.2 += 1,
                        _ => runs.push((m, tm, 1)),
                    }
                }
                println!("  lines (mode, TM, count): {runs:02X?}");
                let bus = snes.cpu.bus();
                for ch in 0..8 {
                    if bus.hdma_enable & (1 << ch) != 0 {
                        let d = bus.dma[ch];
                        println!("  hdma ch{ch}: dmap={:02X} bbad=21{:02X} table={:02X}:{:04X} indirect-bank={:02X}", d.dmap, d.bbad, d.a1b, d.a1t, d.dasb);
                    }
                }
                let bus = snes.cpu.bus_mut();
                let apu = &mut bus.apu;
                let r = apu.spc.regs();
                let b = apu.spc.bus();
                let d = &b.dsp;
                println!(
                    "frame {f}: spc pc={:04X} ports cpu->spc {:02X?} spc->cpu {:02X?} | dsp FLG={:02X} MVOL={:02X}/{:02X} KON-started={:?} env={:?} DIR={:02X}",
                    r.pc,
                    b.from_cpu,
                    b.to_cpu,
                    d.debug_reg(0x6C),
                    d.debug_reg(0x0C),
                    d.debug_reg(0x1C),
                    d.debug_started(),
                    d.debug_env(),
                    d.debug_reg(0x5D)
                );
            }
        }
    }

    #[test]
    fn dsp_plays_a_brr_sample() {
        // Drive the DSP directly: one voice, looping BRR block of a square wave.
        let mut apu = Apu::default();
        {
            let bus = apu.spc.bus_mut();
            // Sample directory at $0300: start = loop = $0400.
            bus.ram[0x300..0x304].copy_from_slice(&[0x00, 0x04, 0x00, 0x04]);
            // One BRR block: range 12, filter 0, loop+end; alternating +7/-8 nibbles.
            bus.ram[0x400] = 0xC3;
            for i in 0..8 {
                bus.ram[0x401 + i] = if i < 4 { 0x77 } else { 0x88 };
            }
            let regs = [(0x6C, 0x20), (0x0C, 0x7F), (0x1C, 0x7F), (0x5D, 0x03), (0x00, 0x7F), (0x01, 0x7F), (0x02, 0x00), (0x03, 0x10), (0x04, 0x00), (0x07, 0x7F), (0x4C, 0x01)];
            for (a, v) in regs {
                bus.dsp_write_for_test(a, v);
            }
        }
        apu.advance(21_477_272 / 10); // 0.1 s
        let n = apu.samples.len() / 2;
        assert!((4700..4900).contains(&n), "0.1 s at 48 kHz, got {n}");
        let rms = (apu.samples.iter().map(|s| s * s).sum::<f32>() / apu.samples.len() as f32).sqrt();
        assert!(rms > 0.05, "voice should be audible, rms {rms}");
    }
}
