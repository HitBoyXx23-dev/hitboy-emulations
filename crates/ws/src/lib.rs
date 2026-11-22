//! Bandai WonderSwan and WonderSwan Color: our V30MZ, the 224x144 display (two tile layers,
//! 128 sprites, windows; 2-bit greyscale or 4-bit colour tiles), 4-channel wavetable sound,
//! timers and bank switching. No boot ROM is needed.

pub mod cpu;

use cpu::Cpu;

pub const WIDTH: usize = 224;
pub const HEIGHT: usize = 144;
const LINES: u32 = 159;
const CYCLES_PER_LINE: u32 = 256;
pub const FPS: f64 = 3_072_000.0 / (CYCLES_PER_LINE as f64 * LINES as f64);
const CLOCK: f64 = 3_072_000.0;

#[derive(state::Stateful)]
pub struct Bus {
    ram: Vec<u8>,
    pub sram: Vec<u8>,
    pub sram_used: bool,
    #[state(skip)]
    rom: Vec<u8>,
    io: [u8; 0x100],
    #[state(skip)]
    color: bool,
    pub line: u32,
    #[state(skip)]
    frame: Vec<u32>,
    #[state(skip)]
    keys: u16,
    irq_status: u8,
    h_timer: u16,
    v_timer: u16,
    // sound
    snd_pos: [u32; 4],
    snd_counter: [u32; 4],
    lfsr: u16,
    #[state(skip)]
    pub samples: Vec<f32>,
    #[state(skip)]
    rate: f64,
    phase: f64,
    acc: (f32, f32),
    acc_n: u32,
}

impl Bus {
    fn rom_bank(&self, bank: usize, off: usize) -> u8 {
        let banks = (self.rom.len() / 0x1_0000).max(1);
        self.rom[((bank % banks) * 0x1_0000 + off) % self.rom.len()]
    }

    fn io_read(&mut self, p: u8) -> u8 {
        match p {
            0x02 => self.line.min(255) as u8,
            0xA0 => 0x80 | if self.color { 0x02 } else { 0 },
            0xA8 => self.h_timer as u8,
            0xA9 => (self.h_timer >> 8) as u8,
            0xAA => self.v_timer as u8,
            0xAB => (self.v_timer >> 8) as u8,
            0xB4 => self.irq_status,
            0xB5 => {
                let sel = self.io[0xB5] & 0x70;
                let mut n = 0u8;
                if sel & 0x10 != 0 {
                    n |= ((self.keys >> 8) & 0xF) as u8; // Y1-Y4
                }
                if sel & 0x20 != 0 {
                    n |= (self.keys & 0xF) as u8; // X1-X4
                }
                if sel & 0x40 != 0 {
                    n |= ((self.keys >> 4) & 0xF) as u8; // buttons
                }
                sel | n
            }
            _ => self.io[p as usize],
        }
    }

    fn io_write(&mut self, p: u8, v: u8) {
        match p {
            0xA2 => {
                self.io[0xA2] = v;
                if v & 1 != 0 && self.h_timer == 0 {
                    self.h_timer = self.reload(0xA4);
                }
                if v & 4 != 0 && self.v_timer == 0 {
                    self.v_timer = self.reload(0xA6);
                }
            }
            0xA4 | 0xA5 => {
                self.io[p as usize] = v;
                self.h_timer = self.reload(0xA4);
            }
            0xA6 | 0xA7 => {
                self.io[p as usize] = v;
                self.v_timer = self.reload(0xA6);
            }
            0xB6 => self.irq_status &= !v,
            _ => self.io[p as usize] = v,
        }
    }

    fn reload(&self, p: usize) -> u16 {
        self.io[p] as u16 | (self.io[p + 1] as u16) << 8
    }

    fn raise(&mut self, bit: u8) {
        if self.io[0xB2] & (1 << bit) != 0 {
            self.irq_status |= 1 << bit;
        }
    }

    // ---------------------------------------------------------------- display

    fn colour(&self, pal: usize, c: usize) -> u32 {
        if self.color && self.io[0x60] & 0x80 != 0 {
            let a = 0xFE00 + pal * 32 + c * 2;
            let w = self.ram[a] as u32 | (self.ram[a + 1] as u32) << 8;
            let (r, g, b) = ((w >> 8) & 0xF, (w >> 4) & 0xF, w & 0xF);
            (r * 17) << 16 | (g * 17) << 8 | b * 17
        } else {
            let pv = self.io[0x20 + pal * 2 + c / 2];
            let idx = if c & 1 == 0 { pv & 7 } else { (pv >> 4) & 7 } as usize;
            let shade = self.io[0x1C + idx / 2];
            let level = if idx & 1 == 0 { shade & 0xF } else { shade >> 4 } as u32;
            let g = (15 - level) * 17;
            g << 16 | g << 8 | g
        }
    }

    fn back_colour(&self) -> u32 {
        let v = self.io[0x01] as usize;
        if self.color && self.io[0x60] & 0x80 != 0 {
            self.colour(v >> 4, v & 0xF)
        } else {
            let shade = self.io[0x1C + (v & 7) / 2];
            let level = if v & 1 == 0 { shade & 0xF } else { shade >> 4 } as u32;
            let g = (15 - level) * 17;
            g << 16 | g << 8 | g
        }
    }

    /// Pixel colour index (0 = transparent) of `tile` at (x, y) within the tile.
    fn tile_pixel(&self, tile: usize, x: usize, y: usize) -> usize {
        let mode = self.io[0x60];
        let four = self.color && mode & 0x40 != 0;
        let packed = mode & 0x20 != 0;
        if four {
            let base = 0x4000 + tile * 32 + y * 4;
            if packed {
                let b = self.ram[(base + x / 2) & 0xFFFF];
                (if x & 1 == 0 { b >> 4 } else { b & 0xF }) as usize
            } else {
                let bit = 7 - x;
                (0..4).map(|p| ((self.ram[(base + p) & 0xFFFF] >> bit) & 1) as usize).enumerate().map(|(p, b)| b << p).sum()
            }
        } else {
            let base = 0x2000 + tile * 16 + y * 2;
            if packed {
                let b = self.ram[(base + x / 4) & 0xFFFF];
                ((b >> (6 - (x % 4) * 2)) & 3) as usize
            } else {
                let bit = 7 - x;
                (((self.ram[base & 0xFFFF] >> bit) & 1) | ((self.ram[(base + 1) & 0xFFFF] >> bit) & 1) << 1) as usize
            }
        }
    }

    fn transparent(&self, pal: usize, c: usize) -> bool {
        if c != 0 {
            return false;
        }
        // In 2-bit mode, palettes 4-7 and 12-15 draw colour 0; everything else is see-through.
        self.io[0x60] & 0x40 != 0 || !matches!(pal, 4..=7 | 12..=15)
    }

    fn layer_pixel(&self, map_base: usize, sx: u8, sy: u8, x: usize, y: usize) -> Option<u32> {
        let px = (x + sx as usize) & 0xFF;
        let py = (y + sy as usize) & 0xFF;
        let a = map_base + ((py / 8) * 32 + px / 8) * 2;
        let e = self.ram[a & 0xFFFF] as usize | (self.ram[(a + 1) & 0xFFFF] as usize) << 8;
        let mut tile = e & 0x1FF;
        if self.color && e & 0x2000 != 0 {
            tile |= 0x200;
        }
        let pal = (e >> 9) & 0xF;
        let tx = if e & 0x4000 != 0 { 7 - px % 8 } else { px % 8 };
        let ty = if e & 0x8000 != 0 { 7 - py % 8 } else { py % 8 };
        let c = self.tile_pixel(tile, tx, ty);
        if self.transparent(pal, c) {
            None
        } else {
            Some(self.colour(pal, c))
        }
    }

    fn render_line(&mut self, y: usize) {
        let ctrl = self.io[0x00];
        let back = self.back_colour();
        let mut line = [back; WIDTH];
        if self.io[0x14] & 1 == 0 {
            self.frame[y * WIDTH..(y + 1) * WIDTH].copy_from_slice(&line);
            return;
        }
        let map_mask = if self.color { 0xF } else { 0x7 };
        let scr1 = ((self.io[0x07] & map_mask) as usize) << 11;
        let scr2 = (((self.io[0x07] >> 4) & map_mask) as usize) << 11;
        if ctrl & 1 != 0 {
            for (x, px) in line.iter_mut().enumerate() {
                if let Some(c) = self.layer_pixel(scr1, self.io[0x10], self.io[0x11], x, y) {
                    *px = c;
                }
            }
        }
        // Sprites behind SCR2 first, then SCR2, then sprites in front.
        let sprites = self.line_sprites(y);
        for &(x, c, front) in &sprites {
            if !front {
                line[x] = c;
            }
        }
        if ctrl & 2 != 0 {
            let (x0, y0, x1, y1) = (self.io[0x08] as usize, self.io[0x09] as usize, self.io[0x0A] as usize, self.io[0x0B] as usize);
            let win = ctrl & 0x20 != 0;
            let outside = ctrl & 0x10 != 0;
            for (x, px) in line.iter_mut().enumerate() {
                if win {
                    let inside = (x0..=x1).contains(&x) && (y0..=y1).contains(&y);
                    if inside == outside {
                        continue;
                    }
                }
                if let Some(c) = self.layer_pixel(scr2, self.io[0x12], self.io[0x13], x, y) {
                    *px = c;
                }
            }
        }
        for &(x, c, front) in &sprites {
            if front {
                line[x] = c;
            }
        }
        self.frame[y * WIDTH..(y + 1) * WIDTH].copy_from_slice(&line);
    }

    /// Sprite pixels on this line: (x, colour, in front of SCR2). Lower-numbered sprites win.
    fn line_sprites(&self, y: usize) -> Vec<(usize, u32, bool)> {
        let mut out: Vec<(usize, u32, bool)> = Vec::new();
        if self.io[0x00] & 4 == 0 {
            return out;
        }
        let base = ((self.io[0x04] & if self.color { 0x3F } else { 0x1F }) as usize) << 9;
        let first = self.io[0x05] as usize & 0x7F;
        let count = (self.io[0x06] as usize).min(128);
        let mut taken = [false; WIDTH];
        let mut n = 0;
        for i in first..(first + count).min(128) {
            let a = base + i * 4;
            let w = self.ram[a & 0xFFFF] as usize | (self.ram[(a + 1) & 0xFFFF] as usize) << 8;
            let sy = self.ram[(a + 2) & 0xFFFF] as usize;
            let sx = self.ram[(a + 3) & 0xFFFF] as usize;
            let row = (y + 256 - sy) & 0xFF;
            if row >= 8 {
                continue;
            }
            n += 1;
            if n > 32 {
                break;
            }
            let tile = w & 0x1FF;
            let pal = 8 + ((w >> 9) & 7);
            let front = w & 0x2000 != 0;
            let ty = if w & 0x8000 != 0 { 7 - row } else { row };
            for px in 0..8 {
                let x = (sx + px) & 0xFF;
                if x >= WIDTH || taken[x] {
                    continue;
                }
                let tx = if w & 0x4000 != 0 { 7 - px } else { px };
                let c = self.tile_pixel(tile, tx, ty);
                if self.transparent(pal, c) {
                    continue;
                }
                taken[x] = true;
                out.push((x, self.colour(pal, c), front));
            }
        }
        out
    }

    // ---------------------------------------------------------------- sound

    fn sound(&mut self, cycles: u32) {
        let ctrl = self.io[0x90];
        let wave_base = (self.io[0x8F] as usize) << 6;
        for _ in 0..cycles {
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for ch in 0..4 {
                if ctrl & (1 << ch) == 0 {
                    continue;
                }
                let f = (self.io[0x80 + ch * 2] as u32 | ((self.io[0x81 + ch * 2] as u32 & 7) << 8)).min(2047);
                self.snd_counter[ch] += 1;
                if self.snd_counter[ch] >= 2048 - f {
                    self.snd_counter[ch] = 0;
                    self.snd_pos[ch] = (self.snd_pos[ch] + 1) & 31;
                    if ch == 3 && ctrl & 0x80 != 0 {
                        let bit = (self.lfsr ^ (self.lfsr >> 1)) & 1;
                        self.lfsr = (self.lfsr >> 1) | (bit << 14);
                    }
                }
                let vol = self.io[0x88 + ch];
                let s = if ch == 1 && ctrl & 0x20 != 0 {
                    // Voice mode: the volume register is an 8-bit sample.
                    (vol as f32 - 128.0) / 128.0 * 15.0
                } else if ch == 3 && ctrl & 0x80 != 0 {
                    if self.lfsr & 1 != 0 { 15.0 } else { 0.0 }
                } else {
                    let pos = self.snd_pos[ch] as usize;
                    let b = self.ram[(wave_base + ch * 16 + pos / 2) & 0xFFFF];
                    (if pos & 1 == 0 { b & 0xF } else { b >> 4 }) as f32
                } - 7.5;
                if ch == 1 && ctrl & 0x20 != 0 {
                    l += s;
                    r += s;
                } else {
                    l += s * (vol >> 4) as f32 / 15.0;
                    r += s * (vol & 0xF) as f32 / 15.0;
                }
            }
            self.acc.0 += l / 30.0;
            self.acc.1 += r / 30.0;
            self.acc_n += 1;
            self.phase += self.rate;
            while self.phase >= CLOCK {
                self.phase -= CLOCK;
                let n = self.acc_n.max(1) as f32;
                self.samples.push(self.acc.0 / n);
                self.samples.push(self.acc.1 / n);
                self.acc = (0.0, 0.0);
                self.acc_n = 0;
            }
        }
    }
}

impl cpu::Bus for Bus {
    fn read(&mut self, a: u32) -> u8 {
        let a = a as usize;
        match a {
            0x0_0000..=0x0_FFFF => self.ram[a],
            0x1_0000..=0x1_FFFF => {
                let bank = self.io[0xC1] as usize;
                self.sram[(bank * 0x1_0000 + (a & 0xFFFF)) % self.sram.len()]
            }
            0x2_0000..=0x2_FFFF => self.rom_bank(self.io[0xC2] as usize, a & 0xFFFF),
            0x3_0000..=0x3_FFFF => self.rom_bank(self.io[0xC3] as usize, a & 0xFFFF),
            _ => self.rom_bank(((self.io[0xC0] as usize) << 4) | (a >> 16), a & 0xFFFF),
        }
    }

    fn write(&mut self, a: u32, v: u8) {
        let a = a as usize;
        match a {
            0x0_0000..=0x0_FFFF => self.ram[a] = v,
            0x1_0000..=0x1_FFFF => {
                let bank = self.io[0xC1] as usize;
                let n = self.sram.len();
                self.sram[(bank * 0x1_0000 + (a & 0xFFFF)) % n] = v;
                self.sram_used = true;
            }
            _ => {}
        }
    }

    fn input(&mut self, port: u16) -> u8 {
        self.io_read(port as u8)
    }

    fn output(&mut self, port: u16, v: u8) {
        self.io_write(port as u8, v)
    }
}

pub struct WonderSwan {
    cpu: Cpu,
    pub bus: Bus,
}

impl state::Stateful for WonderSwan {
    fn sync(&mut self, s: &mut state::State) {
        // Mono or Color, and which cartridge.
        s.check((self.bus.color as u32) << 31 | self.bus.rom.len() as u32 & 0x7FFF_FFFF);
        self.cpu.sync(s);
        self.bus.sync(s);
    }
}

impl WonderSwan {
    pub fn new(rom: &[u8]) -> Result<WonderSwan, String> {
        if rom.len() < 0x1_0000 || rom.len() > 16 * 1024 * 1024 {
            return Err(format!("Not a WonderSwan cartridge ({} bytes).", rom.len()));
        }
        let color = rom[rom.len() - 9] & 1 != 0;
        let mut io = [0u8; 0x100];
        io[0xC0] = 0xFF;
        io[0xC2] = 0xFF;
        io[0xC3] = 0xFF;
        io[0x14] = 1; // LCD on
        // A readable greyscale ramp for games that never set the shade table.
        for (i, v) in [0x20u8, 0x64, 0xA8, 0xFC].iter().enumerate() {
            io[0x1C + i] = *v;
        }
        let bus = Bus {
            ram: vec![0; 0x1_0000],
            sram: vec![0; 0x2_0000],
            sram_used: false,
            rom: rom.to_vec(),
            io,
            color,
            line: 0,
            frame: vec![0xFFFFFF; WIDTH * HEIGHT],
            keys: 0,
            irq_status: 0,
            h_timer: 0,
            v_timer: 0,
            snd_pos: [0; 4],
            snd_counter: [0; 4],
            lfsr: 1,
            samples: Vec::new(),
            rate: 48_000.0,
            phase: 0.0,
            acc: (0.0, 0.0),
            acc_n: 0,
        };
        Ok(WonderSwan { cpu: Cpu::new(), bus })
    }

    pub fn is_color(&self) -> bool {
        self.bus.color
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    fn check_irq(&mut self) {
        // The interrupt line is a level: re-evaluated before every instruction.
        let pending = self.bus.irq_status & self.bus.io[0xB2];
        self.cpu.irq = if pending != 0 {
            let bit = 7 - pending.leading_zeros() as u8;
            Some((self.bus.io[0xB0] & 0xF8) + bit)
        } else {
            None
        };
    }

    pub fn run_frame(&mut self) {
        for _ in 0..LINES {
            let line = self.bus.line;
            let mut used = 0;
            while used < CYCLES_PER_LINE {
                self.check_irq();
                let c = self.cpu.step(&mut self.bus);
                self.bus.sound(c);
                used += c;
            }
            if (line as usize) < HEIGHT {
                self.bus.render_line(line as usize);
            }
            // Horizontal blank timer.
            let ctrl = self.bus.io[0xA2];
            if ctrl & 1 != 0 && self.bus.h_timer > 0 {
                self.bus.h_timer -= 1;
                if self.bus.h_timer == 0 {
                    self.bus.raise(7);
                    if ctrl & 2 != 0 {
                        self.bus.h_timer = self.bus.reload(0xA4);
                    }
                }
            }
            if line as usize == HEIGHT {
                self.bus.raise(6);
                if ctrl & 4 != 0 && self.bus.v_timer > 0 {
                    self.bus.v_timer -= 1;
                    if self.bus.v_timer == 0 {
                        self.bus.raise(5);
                        if ctrl & 8 != 0 {
                            self.bus.v_timer = self.bus.reload(0xA6);
                        }
                    }
                }
            }
            self.bus.line = (line + 1) % LINES;
            if self.bus.line == self.bus.io[0x03] as u32 {
                self.bus.raise(4);
            }
        }
    }

    /// Snapshot of the whole machine (save states, rewind).
    pub fn save_state(&mut self) -> Vec<u8> {
        state::save(self)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        state::load(self, data)
    }

    pub fn frame(&self) -> &[u32] {
        &self.bus.frame
    }

    /// Whether the game is played with the console held vertically: the screen's
    /// vertical/horizontal indicator (port 0x15) when the game sets one, otherwise the
    /// cartridge header's orientation flag.
    pub fn vertical(&self) -> bool {
        let icons = self.bus.io[0x15];
        if icons & 0x06 != 0 {
            return icons & 0x02 != 0;
        }
        let r = &self.bus.rom;
        r[r.len() - 4] & 1 != 0
    }

    /// The picture upright: landscape as is, or turned a quarter turn for vertical games
    /// (144 wide, 224 tall). Returns the size.
    pub fn upright_frame(&self, out: &mut Vec<u32>) -> (usize, usize) {
        let f = &self.bus.frame;
        if !self.vertical() {
            out.clear();
            out.extend_from_slice(f);
            return (WIDTH, HEIGHT);
        }
        // Held rotated counter-clockwise: the landscape right edge is the top.
        out.resize(WIDTH * HEIGHT, 0);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                out[(WIDTH - 1 - x) * HEIGHT + y] = f[y * WIDTH + x];
            }
        }
        (HEIGHT, WIDTH)
    }

    /// Landscape: D-pad = X1-X4, A/B, Start; X/Y/L/R = Y1-Y4.
    /// Vertical: the D-pad drives the Y pad (turned with the console) and X/Y/L/R the X pad,
    /// so directions match the upright picture.
    pub fn set_buttons(&mut self, pad: usize, b: u16) {
        if pad != 0 {
            return;
        }
        let bit = |n: u16| (b >> n) & 1;
        // Directions as (up, right, down, left) from the D-pad and from X/Y/L/R.
        let dpad = [bit(4), bit(7), bit(5), bit(6)];
        let face = [bit(8), bit(11), bit(9), bit(10)];
        // Pad bits are (1 up, 2 right, 3 down, 4 left) in landscape.
        let pack = |d: [u16; 4]| d[0] | d[1] << 1 | d[2] << 2 | d[3] << 3;
        // Turned counter-clockwise, screen-up is the pad's right, and so on.
        let turn = |d: [u16; 4]| [d[3], d[0], d[1], d[2]];
        let (x, y) = if self.vertical() { (pack(turn(face)), pack(turn(dpad))) } else { (pack(dpad), pack(face)) };
        let buttons = bit(3) << 1 | bit(0) << 2 | bit(1) << 3; // start A B
        let old = self.bus.keys;
        self.bus.keys = x | buttons << 4 | y << 8;
        if self.bus.keys & !old != 0 {
            self.bus.raise(1);
        }
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.bus.samples);
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.bus.rate = r as f64;
    }

    pub fn save_ram(&self) -> Option<&[u8]> {
        self.bus.sram_used.then_some(&self.bus.sram[..])
    }

    pub fn load_save_ram(&mut self, data: &[u8]) {
        if data.len() == self.bus.sram.len() {
            self.bus.sram.copy_from_slice(data);
            self.bus.sram_used = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64 KB cartridge: sets up a mono palette and one background tile, enables SCR1 and the
    /// vblank interrupt, and counts interrupts in RAM.
    fn test_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x1_0000];
        let code: Vec<u8> = vec![
            0xFA, // CLI
            0x31, 0xC0, // XOR AX,AX
            0x8E, 0xD8, // MOV DS,AX
            0x8E, 0xC0, // MOV ES,AX
            0x8E, 0xD0, // MOV SS,AX
            0xBC, 0x00, 0x20, // MOV SP,0x2000
            0xC7, 0x06, 0x98, 0x00, 0x00, 0x01, // MOV WORD [0x0098],0x0100
            0xC7, 0x06, 0x9A, 0x00, 0x00, 0xF0, // MOV WORD [0x009A],0xF000
            0xBF, 0x10, 0x20, // MOV DI,0x2010
            0xB9, 0x08, 0x00, // MOV CX,8
            0xB8, 0xFF, 0x00, // MOV AX,0x00FF
            0xFC, 0xF3, 0xAB, // CLD ; REP STOSW (tile rows: plane0 = FF, plane1 = 00)
            0xC7, 0x06, 0x00, 0x08, 0x01, 0x00, // MOV WORD [0x0800],0x0001 (SCR1 map at 0x0800, entry 0 = tile 1)
            0xB0, 0x01, 0xE6, 0x07, // MOV AL,1 ; OUT 0x07,AL (SCR1 map base 0x0800)
            0xB0, 0x70, 0xE6, 0x20, // MOV AL,0x70 ; OUT 0x20,AL (palette 0: c0 = 0, c1 = 7)
            0xB0, 0x01, 0xE6, 0x00, // MOV AL,1 ; OUT 0x00,AL (SCR1 on)
            0xB0, 0x20, 0xE6, 0xB0, // MOV AL,0x20 ; OUT 0xB0,AL (interrupt base)
            0xB0, 0x40, 0xE6, 0xB2, // MOV AL,0x40 ; OUT 0xB2,AL (vblank interrupt)
            0xFB, // STI
            0xF4, // HLT
            0xEB, 0xFD, // JMP back to HLT
        ];
        rom[..code.len()].copy_from_slice(&code);
        // Handler at F000:0100 -> ROM offset 0x0100: INC BYTE [0x3000] ; MOV AL,0x40 ; OUT 0xB6,AL ; IRET
        rom[0x100..0x10A].copy_from_slice(&[0xFE, 0x06, 0x00, 0x30, 0xB0, 0x40, 0xE6, 0xB6, 0xCF, 0x00]);
        // Reset at FFFF:0000 -> ROM offset 0xFFF0: JMP FAR F000:0000
        rom[0xFFF0..0xFFF5].copy_from_slice(&[0xEA, 0x00, 0x00, 0x00, 0xF0]);
        rom
    }

    #[test]
    fn boots_draws_and_counts_vblanks() {
        let mut w = WonderSwan::new(&test_rom()).unwrap();
        for _ in 0..4 {
            w.run_frame();
        }
        let n = w.bus.ram[0x3000];
        assert!((3..=4).contains(&n), "vblank interrupts: {n}");
        // Colour 1 -> shade index 7 -> level from port 0x1F high nibble (0xF) -> black.
        assert_eq!(w.frame()[0], 0x000000, "tile pixel");
        assert!(!w.is_color());
    }

    #[test]
    fn save_state_resumes_identically() {
        let rom = test_rom();
        let mut a = WonderSwan::new(&rom).unwrap();
        for _ in 0..2 {
            a.run_frame();
        }
        let at = a.bus.ram[0x3000];
        let snap = a.save_state();
        for _ in 0..5 {
            a.run_frame();
        }
        let want = (a.bus.ram[0x3000], a.frame().to_vec());
        assert!(want.0 > at);
        let mut b = WonderSwan::new(&rom).unwrap();
        b.load_state(&snap).unwrap();
        assert_eq!(b.bus.ram[0x3000], at);
        for _ in 0..5 {
            b.run_frame();
        }
        assert_eq!((b.bus.ram[0x3000], b.frame().to_vec()), want);
    }

    #[test]
    fn keypad_groups() {
        let mut w = WonderSwan::new(&test_rom()).unwrap();
        w.set_buttons(0, 1 | 0x10); // A + up
        use cpu::Bus as _;
        w.bus.output(0xB5, 0x20);
        assert_eq!(w.bus.input(0xB5) & 0xF, 1, "X1 (up)");
        w.bus.output(0xB5, 0x40);
        assert_eq!(w.bus.input(0xB5) & 0xF, 4, "A");
    }
}
