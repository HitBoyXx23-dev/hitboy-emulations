//! Nintendo Game Boy and Game Boy Color. CPU: the C++ `cpusm83` core; everything else is Rust.

pub mod apu;
pub mod cartridge;
pub mod ppu;

use cartridge::Cartridge;
use cpusm83::Cpu;
use ppu::Ppu;

pub use ppu::{HEIGHT, WIDTH};

/// Same bit layout as the NES pad, so frontends can share input code.
pub mod button {
    pub const A: u8 = 1 << 0;
    pub const B: u8 = 1 << 1;
    pub const SELECT: u8 = 1 << 2;
    pub const START: u8 = 1 << 3;
    pub const UP: u8 = 1 << 4;
    pub const DOWN: u8 = 1 << 5;
    pub const LEFT: u8 = 1 << 6;
    pub const RIGHT: u8 = 1 << 7;
}

const INT_TIMER: u8 = 4;
const INT_JOYPAD: u8 = 16;
pub const CYCLES_PER_FRAME: u32 = 70_224;
pub const FPS: f64 = 4_194_304.0 / CYCLES_PER_FRAME as f64;

#[derive(state::Stateful)]
pub struct Bus {
    pub cart: Cartridge,
    pub ppu: Ppu,
    pub apu: apu::Apu,
    /// 8 banks of 4 KiB (banks 2-7 exist on Game Boy Color only).
    wram: Box<[u8; 0x8000]>,
    svbk: u8,
    hram: [u8; 0x7F],
    ie: u8,
    #[state(skip)]
    cgb: bool,
    double_speed: bool,
    speed_armed: bool,
    /// Leftover CPU cycle when halving for double speed.
    half_carry: u32,
    hdma_src: u16,
    hdma_dst: u16,
    /// Remaining 16-byte blocks of an active H-Blank DMA.
    hdma_left: u8,
    hdma_active: bool,
    iflag: u8,
    // timer
    div_counter: u16,
    tima: u8,
    tma: u8,
    tac: u8,
    // input
    joyp_select: u8,
    #[state(skip)]
    pub buttons: u8,
    io: [u8; 0x80],
}

impl Bus {
    fn joyp(&self) -> u8 {
        let mut v = 0x0F;
        if self.joyp_select & 0x10 == 0 {
            // D-pad: Right, Left, Up, Down
            let b = self.buttons;
            v &= !(((b >> 7) & 1) | (((b >> 6) & 1) << 1) | (((b >> 4) & 1) << 2) | (((b >> 5) & 1) << 3));
        }
        if self.joyp_select & 0x20 == 0 {
            v &= !(self.buttons & 0x0F);
        }
        0xC0 | self.joyp_select | v
    }

    fn tick(&mut self, cycles: u32) {
        // Timer: TIMA increments on falling edges of a DIV bit selected by TAC.
        let bit = match self.tac & 3 {
            0 => 9,
            1 => 3,
            2 => 5,
            _ => 7,
        };
        for _ in 0..cycles / 4 {
            let old = self.div_counter;
            self.div_counter = self.div_counter.wrapping_add(4);
            if self.tac & 4 != 0 && (old >> bit) & 1 == 1 && (self.div_counter >> bit) & 1 == 0 {
                let (v, overflow) = self.tima.overflowing_add(1);
                if overflow {
                    self.tima = self.tma;
                    self.iflag |= INT_TIMER;
                } else {
                    self.tima = v;
                }
            }
        }
        // Double speed doubles the CPU and timer; the LCD and sound keep their pace.
        let dots = if self.double_speed {
            let total = cycles + self.half_carry;
            self.half_carry = total & 1;
            total / 2
        } else {
            cycles
        };
        self.iflag |= self.ppu.tick(dots);
        self.apu.tick(dots);
        if self.ppu.hblank_entered {
            self.ppu.hblank_entered = false;
            if self.hdma_active {
                self.hdma_block();
            }
        }
    }

    #[inline]
    fn wram_index(&self, addr: u16) -> usize {
        let off = addr as usize & 0x1FFF;
        if off < 0x1000 {
            off
        } else {
            let bank = if self.cgb { (self.svbk as usize & 7).max(1) } else { 1 };
            bank * 0x1000 + (off - 0x1000)
        }
    }

    /// Copies one 16-byte block for HDMA.
    fn hdma_block(&mut self) {
        for _ in 0..16 {
            let b = cpusm83::Bus::read(self, self.hdma_src);
            self.ppu.write_vram(0x8000 | (self.hdma_dst & 0x1FFF), b);
            self.hdma_src = self.hdma_src.wrapping_add(1);
            self.hdma_dst = self.hdma_dst.wrapping_add(1);
        }
        if self.hdma_left == 0 {
            self.hdma_active = false;
        } else {
            self.hdma_left -= 1;
        }
    }
}

impl cpusm83::Bus for Bus {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF | 0xA000..=0xBFFF => self.cart.read(addr),
            0x8000..=0x9FFF => self.ppu.read_vram(addr),
            0xC000..=0xFDFF => self.wram[self.wram_index(addr)],
            0xFE00..=0xFE9F => self.ppu.oam[addr as usize - 0xFE00],
            0xFF00 => self.joyp(),
            0xFF04 => (self.div_counter >> 8) as u8,
            0xFF05 => self.tima,
            0xFF06 => self.tma,
            0xFF07 => 0xF8 | self.tac,
            0xFF0F => 0xE0 | self.iflag,
            0xFF10..=0xFF3F => self.apu.read(addr),
            0xFF40 => self.ppu.lcdc,
            0xFF41 => 0x80 | self.ppu.stat,
            0xFF42 => self.ppu.scy,
            0xFF43 => self.ppu.scx,
            0xFF44 => self.ppu.ly,
            0xFF45 => self.ppu.lyc,
            0xFF47 => self.ppu.bgp,
            0xFF48 => self.ppu.obp0,
            0xFF49 => self.ppu.obp1,
            0xFF4A => self.ppu.wy,
            0xFF4B => self.ppu.wx,
            0xFF4D if self.cgb => 0x7E | (self.double_speed as u8) << 7 | self.speed_armed as u8,
            0xFF4F if self.cgb => 0xFE | self.ppu.vbk,
            0xFF55 if self.cgb => if self.hdma_active { self.hdma_left } else { 0xFF },
            0xFF68 if self.cgb => self.ppu.bcps,
            0xFF69 if self.cgb => self.ppu.read_pal(false),
            0xFF6A if self.cgb => self.ppu.ocps,
            0xFF6B if self.cgb => self.ppu.read_pal(true),
            0xFF70 if self.cgb => 0xF8 | self.svbk,
            0xFF80..=0xFFFE => self.hram[addr as usize - 0xFF80],
            0xFFFF => self.ie,
            0xFF00..=0xFF7F => self.io[addr as usize - 0xFF00],
            _ => 0xFF,
        }
    }

    #[inline]
    fn write(&mut self, addr: u16, v: u8) {
        match addr {
            0x0000..=0x7FFF | 0xA000..=0xBFFF => self.cart.write(addr, v),
            0x8000..=0x9FFF => self.ppu.write_vram(addr, v),
            0xC000..=0xFDFF => {
                let i = self.wram_index(addr);
                self.wram[i] = v;
            }
            0xFE00..=0xFE9F => self.ppu.oam[addr as usize - 0xFE00] = v,
            0xFF00 => self.joyp_select = v & 0x30,
            0xFF04 => self.div_counter = 0,
            0xFF05 => self.tima = v,
            0xFF06 => self.tma = v,
            0xFF07 => self.tac = v & 7,
            0xFF0F => self.iflag = v & 0x1F,
            0xFF10..=0xFF3F => self.apu.write(addr, v),
            0xFF40 => self.ppu.write_lcdc(v),
            0xFF41 => self.ppu.stat = (self.ppu.stat & 7) | (v & 0x78),
            0xFF42 => self.ppu.scy = v,
            0xFF43 => self.ppu.scx = v,
            0xFF44 => {}
            0xFF45 => self.ppu.lyc = v,
            0xFF46 => {
                // OAM DMA (instant).
                let src = (v as u16) << 8;
                for i in 0..0xA0u16 {
                    let b = self.read(src + i);
                    self.ppu.oam[i as usize] = b;
                }
            }
            0xFF47 => self.ppu.bgp = v,
            0xFF48 => self.ppu.obp0 = v,
            0xFF49 => self.ppu.obp1 = v,
            0xFF4A => self.ppu.wy = v,
            0xFF4B => self.ppu.wx = v,
            // KEY1. 0xFF is written by the CPU core's STOP instruction (the speed switch).
            0xFF4D if self.cgb => {
                if v == 0xFF {
                    if self.speed_armed {
                        self.double_speed = !self.double_speed;
                        self.speed_armed = false;
                        self.div_counter = 0;
                    }
                } else {
                    self.speed_armed = v & 1 != 0;
                }
            }
            0xFF4F if self.cgb => self.ppu.vbk = v & 1,
            0xFF51 if self.cgb => self.hdma_src = (self.hdma_src & 0x00FF) | (v as u16) << 8,
            0xFF52 if self.cgb => self.hdma_src = (self.hdma_src & 0xFF00) | (v & 0xF0) as u16,
            0xFF53 if self.cgb => self.hdma_dst = (self.hdma_dst & 0x00FF) | ((v & 0x1F) as u16) << 8,
            0xFF54 if self.cgb => self.hdma_dst = (self.hdma_dst & 0xFF00) | (v & 0xF0) as u16,
            0xFF55 if self.cgb => {
                if self.hdma_active && v & 0x80 == 0 {
                    self.hdma_active = false; // cancel H-Blank DMA
                } else if v & 0x80 != 0 {
                    self.hdma_left = v & 0x7F;
                    self.hdma_active = true;
                } else {
                    // General-purpose DMA: copy everything now.
                    self.hdma_left = v & 0x7F;
                    self.hdma_active = true;
                    while self.hdma_active {
                        self.hdma_block();
                    }
                }
            }
            0xFF68 if self.cgb => self.ppu.bcps = v,
            0xFF69 if self.cgb => self.ppu.write_pal(false, v),
            0xFF6A if self.cgb => self.ppu.ocps = v,
            0xFF6B if self.cgb => self.ppu.write_pal(true, v),
            0xFF70 if self.cgb => self.svbk = v & 7,
            0xFF80..=0xFFFE => self.hram[addr as usize - 0xFF80] = v,
            0xFFFF => self.ie = v,
            0xFF00..=0xFF7F => self.io[addr as usize - 0xFF00] = v,
            _ => {}
        }
    }
}

pub struct GameBoy {
    cpu: Cpu<Bus>,
}

impl state::Stateful for GameBoy {
    fn sync(&mut self, s: &mut state::State) {
        // Game Boy or Color, and which cartridge.
        let bus = self.cpu.bus();
        s.check((bus.cgb as u32) << 31 | bus.cart.rom_len() as u32 & 0x7FFF_FFFF);
        self.cpu.sync_state(s);
        self.cpu.bus_mut().sync(s);
    }
}

impl GameBoy {
    pub fn from_rom(rom: Vec<u8>) -> Result<Self, String> {
        let cart = Cartridge::load(rom)?;
        let cgb = cart.cgb;
        let mut ppu = Ppu::default();
        ppu.cgb = cgb;
        let bus = Bus {
            cart,
            ppu,
            apu: apu::Apu::default(),
            wram: Box::new([0; 0x8000]),
            svbk: 1,
            hram: [0; 0x7F],
            ie: 0,
            cgb,
            double_speed: false,
            speed_armed: false,
            half_carry: 0,
            hdma_src: 0,
            hdma_dst: 0,
            hdma_left: 0,
            hdma_active: false,
            iflag: 1,
            div_counter: 0xABCC,
            tima: 0,
            tma: 0,
            tac: 0xF8 & 7,
            joyp_select: 0x30,
            buttons: 0,
            io: [0; 0x80],
        };
        let mut gb = Self { cpu: Cpu::new(bus) };
        gb.reset();
        Ok(gb)
    }

    /// True when running in Game Boy Color mode.
    pub fn is_color(&self) -> bool {
        self.cpu.bus().cgb
    }

    pub fn title(&self) -> &str {
        &self.cpu.bus().cart.title
    }

    pub fn reset(&mut self) {
        let mut r = self.cpu.regs();
        // Post-boot register values; A = 0x11 tells games they're on a Game Boy Color.
        (r.a, r.f, r.b, r.c, r.d, r.e, r.h, r.l) = if self.cpu.bus().cgb {
            (0x11, 0x80, 0x00, 0x00, 0xFF, 0x56, 0x00, 0x0D)
        } else {
            (0x01, 0xB0, 0x00, 0x13, 0x00, 0xD8, 0x01, 0x4D)
        };
        r.sp = 0xFFFE;
        r.pc = 0x0100;
        r.ime = 0;
        r.halted = 0;
        self.cpu.set_regs(r);
    }

    pub fn run_frame(&mut self) {
        self.cpu.bus_mut().ppu.frame_ready = false;
        let mut spent = 0;
        // LCD may be off; bound the frame by cycle count as well.
        let budget = if self.cpu.bus().double_speed { CYCLES_PER_FRAME * 2 } else { CYCLES_PER_FRAME };
        while spent < budget && !self.cpu.bus().ppu.frame_ready {
            let c = self.cpu.step();
            self.cpu.bus_mut().tick(c);
            spent += c;
        }
    }

    pub fn frame(&self) -> &[u32] {
        &self.cpu.bus().ppu.frame
    }

    pub fn set_buttons(&mut self, buttons: u8) {
        let bus = self.cpu.bus_mut();
        if buttons & !bus.buttons != 0 {
            bus.iflag |= INT_JOYPAD;
        }
        bus.buttons = buttons;
    }

    /// Moves generated audio (interleaved stereo f32) into out.
    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.cpu.bus_mut().apu.samples);
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.cpu.bus_mut().apu.set_sample_rate(rate);
    }

    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    /// Battery-backed save RAM, if the cartridge has any. Carts with a clock append
    /// "RTC1", the clock's base time and its halted value.
    pub fn save_ram(&self) -> Option<Vec<u8>> {
        let c = &self.cpu.bus().cart;
        if !c.has_battery {
            return None;
        }
        let mut v = c.ram.clone();
        if c.rtc {
            v.extend_from_slice(b"RTC1");
            v.extend_from_slice(&c.rtc_base.to_le_bytes());
            v.extend_from_slice(&c.rtc_halted.unwrap_or(-1).to_le_bytes());
        }
        Some(v)
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        let c = &mut self.cpu.bus_mut().cart;
        let n = c.ram.len().min(data.len());
        c.ram[..n].copy_from_slice(&data[..n]);
        let tail = &data[n..];
        if c.rtc && tail.len() >= 20 && &tail[..4] == b"RTC1" {
            c.rtc_base = i64::from_le_bytes(tail[4..12].try_into().unwrap());
            let h = i64::from_le_bytes(tail[12..20].try_into().unwrap());
            c.rtc_halted = (h >= 0).then_some(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ROM that enables VBlank interrupts, counts them in HRAM and halts between frames.
    fn test_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x00;
        rom[0x134..0x138].copy_from_slice(b"TEST");
        // VBlank vector: INC (HL) where HL=FF80 ; RETI
        rom[0x40..0x42].copy_from_slice(&[0x34, 0xD9]);
        // Entry: LD HL,FF80; LD A,1; LDH (FF),A (IE=vblank); EI; loop: HALT; JR loop
        rom[0x100..0x10B].copy_from_slice(&[0x21, 0x80, 0xFF, 0x3E, 0x01, 0xE0, 0xFF, 0xFB, 0x76, 0x18, 0xFD]);
        rom
    }

    #[test]
    fn vblank_interrupt_fires_once_per_frame() {
        let mut gb = GameBoy::from_rom(test_rom()).unwrap();
        assert_eq!(gb.title(), "TEST");
        for _ in 0..30 {
            gb.run_frame();
        }
        let count = cpusm83::Bus::read(gb.cpu.bus_mut(), 0xFF80);
        assert!((29..=30).contains(&count), "vblanks = {count}");
    }

    #[test]
    fn square_channel_produces_a_tone() {
        let mut gb = GameBoy::from_rom(test_rom()).unwrap();
        let bus = gb.cpu.bus_mut();
        for (addr, v) in [(0xFF26, 0x80), (0xFF24, 0x77), (0xFF25, 0xFF), (0xFF16, 0x80), (0xFF17, 0xF0), (0xFF18, 0x00), (0xFF19, 0x87)] {
            cpusm83::Bus::write(bus, addr, v);
        }
        for _ in 0..30 {
            gb.run_frame();
        }
        let mut audio = Vec::new();
        gb.take_audio(&mut audio);
        let secs = audio.len() as f64 / 2.0 / 48_000.0;
        assert!((0.45..0.55).contains(&secs), "30 frames should be ~0.5 s of audio, got {secs}");
        let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).sqrt();
        assert!(rms > 0.05, "tone should be audible, rms {rms}");
        // Square frequency = 131072 / (2048 - x) Hz; x = 0x700 gives 512 Hz. Count zero crossings (left channel).
        let left: Vec<f32> = audio.iter().step_by(2).copied().collect();
        let crossings = left.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count() as f64;
        let hz = crossings / 2.0 / secs;
        assert!((490.0..535.0).contains(&hz), "expected ~512 Hz, measured {hz:.0} Hz");
    }

    /// A restored game carries on exactly as if it had never stopped (registers, memory,
    /// picture and sound), including the audio chip's internal state.
    #[test]
    fn save_state_resumes_identically() {
        let mut a = GameBoy::from_rom(test_rom()).unwrap();
        let bus = a.cpu.bus_mut();
        for (addr, v) in [(0xFF26, 0x80), (0xFF24, 0x77), (0xFF25, 0xFF), (0xFF16, 0x80), (0xFF17, 0xF0), (0xFF18, 0x00), (0xFF19, 0x87)] {
            cpusm83::Bus::write(bus, addr, v);
        }
        for _ in 0..5 {
            a.run_frame();
        }
        let snap = a.save_state();
        let mut want_audio = Vec::new();
        a.take_audio(&mut want_audio);
        want_audio.clear();
        for _ in 0..8 {
            a.run_frame();
        }
        a.take_audio(&mut want_audio);
        let want = (cpusm83::Bus::read(a.cpu.bus_mut(), 0xFF80), a.cpu.regs(), a.frame().to_vec());

        let mut b = GameBoy::from_rom(test_rom()).unwrap();
        b.load_state(&snap).unwrap();
        for _ in 0..8 {
            b.run_frame();
        }
        let mut got_audio = Vec::new();
        b.take_audio(&mut got_audio);
        assert_eq!((cpusm83::Bus::read(b.cpu.bus_mut(), 0xFF80), b.cpu.regs(), b.frame().to_vec()), want);
        assert_eq!(got_audio, want_audio, "the sound continues sample for sample");
        // A Game Boy Color state doesn't load into a Game Boy.
        let mut color = GameBoy::from_rom(cgb_rom()).unwrap();
        color.run_frame();
        assert!(a.load_state(&color.save_state()).is_err());
    }

    fn cgb_rom() -> Vec<u8> {
        let mut rom = test_rom();
        rom[0x143] = 0x80; // Game Boy Color compatible
        rom
    }

    #[test]
    fn color_palettes_reach_the_screen() {
        let mut gb = GameBoy::from_rom(cgb_rom()).unwrap();
        assert!(gb.is_color());
        assert_eq!(gb.cpu.regs().a, 0x11, "games detect CGB via A = 0x11");
        let bus = gb.cpu.bus_mut();
        use cpusm83::Bus as _;
        // Background palette 0, colour 1 = pure red (RGB555 0x001F), with auto-increment.
        bus.write(0xFF68, 0x80 | 2);
        bus.write(0xFF69, 0x1F);
        bus.write(0xFF69, 0x00);
        // Tile 0: every row uses colour 1 (low plane set).
        for row in 0..8 {
            bus.write(0x8000 + row * 2, 0xFF);
            bus.write(0x8001 + row * 2, 0x00);
        }
        gb.run_frame();
        gb.run_frame();
        assert_eq!(gb.frame()[0], 0xFF0000, "top-left pixel should be red");
    }

    #[test]
    fn wram_banks_and_hdma() {
        let mut gb = GameBoy::from_rom(cgb_rom()).unwrap();
        let bus = gb.cpu.bus_mut();
        use cpusm83::Bus as _;
        // Different values in WRAM banks 2 and 3 at the same address.
        bus.write(0xFF70, 2);
        bus.write(0xD000, 0xAA);
        bus.write(0xFF70, 3);
        bus.write(0xD000, 0xBB);
        bus.write(0xFF70, 2);
        assert_eq!(bus.read(0xD000), 0xAA);
        // General-purpose HDMA: copy 32 bytes from C100 to VRAM bank 1 at 8800.
        for i in 0..32u16 {
            bus.write(0xC100 + i, i as u8 + 1);
        }
        bus.write(0xFF4F, 1);
        for (reg, v) in [(0xFF51, 0xC1), (0xFF52, 0x00), (0xFF53, 0x08), (0xFF54, 0x00), (0xFF55, 0x01)] {
            bus.write(reg, v);
        }
        assert_eq!(bus.read(0x8800), 1);
        assert_eq!(bus.read(0x881F), 32);
        assert_eq!(bus.read(0xFF55), 0xFF, "transfer complete");
        bus.write(0xFF4F, 0);
        assert_eq!(bus.read(0x8800), 0, "bank 0 untouched");
    }

    #[test]
    fn joypad_matrix() {
        let mut gb = GameBoy::from_rom(test_rom()).unwrap();
        gb.set_buttons(button::START | button::LEFT);
        let bus = gb.cpu.bus_mut();
        cpusm83::Bus::write(bus, 0xFF00, 0x10); // select buttons (P15 low? no: P14 high, P15 low)
        let buttons = cpusm83::Bus::read(bus, 0xFF00) & 0x0F;
        assert_eq!(buttons, 0x0F & !0x08, "start pressed");
        cpusm83::Bus::write(bus, 0xFF00, 0x20); // select d-pad
        let dpad = cpusm83::Bus::read(bus, 0xFF00) & 0x0F;
        assert_eq!(dpad, 0x0F & !0x02, "left pressed");
    }
}
