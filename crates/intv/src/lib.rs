//! Mattel Intellivision: our CP1610 CPU, STIC video, AY-3-8914 sound and hand controllers.
//! Needs the console's EXEC (exec.bin, 8 KB) and GROM (grom.bin, 2 KB) from the user.

pub mod cpu;
pub mod psg;
pub mod stic;

use cpu::Cpu;
use psg::Psg;
use stic::Stic;

pub const WIDTH: usize = stic::WIDTH;
pub const HEIGHT: usize = stic::HEIGHT;
pub const FPS: f64 = 59.92;
const CYCLES_PER_FRAME: u32 = 14_934;
/// CPU time inside vertical blank, when games update the STIC and GRAM.
const VBLANK_CYCLES: u32 = 3_791;

pub struct Bus {
    pub stic: Stic,
    pub psg: Psg,
    scratch: [u8; 240],
    pub sysram: [u16; 352],
    exec: Vec<u16>,
    grom: Vec<u8>,
    gram: [u8; 512],
    cart: Vec<u16>,
    mapped: Vec<bool>,
}

impl cpu::Bus for Bus {
    fn read(&mut self, a: u16) -> u16 {
        match a {
            0x0000..=0x003F => self.stic.read(a),
            0x0100..=0x01EF => self.scratch[(a - 0x100) as usize] as u16,
            0x01F0..=0x01FF => self.psg.read(a),
            0x0200..=0x035F => self.sysram[(a - 0x200) as usize],
            0x1000..=0x1FFF => self.exec[(a - 0x1000) as usize],
            0x3000..=0x37FF => self.grom[(a - 0x3000) as usize] as u16,
            0x3800..=0x3FFF => self.gram[(a as usize - 0x3800) & 0x1FF] as u16,
            _ if self.mapped[a as usize] => self.cart[a as usize],
            _ => 0xFFFF,
        }
    }

    fn write(&mut self, a: u16, v: u16) {
        match a {
            0x0000..=0x003F => self.stic.write(a, v),
            0x0100..=0x01EF => self.scratch[(a - 0x100) as usize] = v as u8,
            0x01F0..=0x01FF => self.psg.write(a, v),
            0x0200..=0x035F => self.sysram[(a - 0x200) as usize] = v,
            0x3800..=0x3FFF => self.gram[(a as usize - 0x3800) & 0x1FF] = v as u8,
            _ => {}
        }
    }
}

fn be_words(b: &[u8]) -> Vec<u16> {
    b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect()
}

/// Places a cartridge in memory. Handles Intellicart .rom files (with their own memory map)
/// and raw .int/.bin dumps (standard map: $5000-$6FFF, then $D000-$DFFF, then $F000-$FFFF).
fn load_cart(rom: &[u8], cart: &mut [u16], mapped: &mut [bool]) -> Result<(), String> {
    let mut put = |addr: usize, w: u16| {
        cart[addr & 0xFFFF] = w;
        mapped[addr & 0xFFFF] = true;
    };
    if rom.len() > 3 && rom[0] == 0xA8 && rom[1] == !rom[2] {
        let segments = rom[1] as usize;
        let mut p = 3;
        for _ in 0..segments {
            let (lo, hi) = (*rom.get(p).ok_or("Truncated .rom file")? as usize, *rom.get(p + 1).ok_or("Truncated .rom file")? as usize);
            p += 2;
            let (start, end) = (lo << 8, (hi << 8) | 0xFF);
            if end < start {
                return Err("This .rom file's memory map is damaged.".into());
            }
            for a in start..=end {
                let w = u16::from_be_bytes([*rom.get(p).ok_or("Truncated .rom file")?, *rom.get(p + 1).ok_or("Truncated .rom file")?]);
                put(a, w);
                p += 2;
            }
            p += 2; // CRC
        }
        return Ok(());
    }
    if rom.is_empty() || rom.len() % 2 != 0 || rom.len() > 0x2_0000 {
        return Err(format!("Not an Intellivision cartridge ({} bytes).", rom.len()));
    }
    let words = be_words(rom);
    let windows: [(usize, usize); 3] = [(0x5000, 0x2000), (0xD000, 0x1000), (0xF000, 0x1000)];
    let mut i = 0;
    for (base, len) in windows {
        for k in 0..len {
            if i >= words.len() {
                return Ok(());
            }
            put(base + k, words[i]);
            i += 1;
        }
    }
    Ok(())
}

pub struct Intellivision {
    cpu: Cpu,
    pub bus: Bus,
}

impl Intellivision {
    pub fn new(exec: &[u8], grom: &[u8], rom: &[u8]) -> Result<Intellivision, String> {
        if exec.len() != 8192 {
            return Err("The Intellivision EXEC BIOS (exec.bin) must be 8 KB.".into());
        }
        if grom.len() != 2048 {
            return Err("The Intellivision GROM (grom.bin) must be 2 KB.".into());
        }
        let mut cart = vec![0u16; 0x1_0000];
        let mut mapped = vec![false; 0x1_0000];
        load_cart(rom, &mut cart, &mut mapped)?;
        let bus = Bus {
            stic: Stic::new(),
            psg: Psg::new(),
            scratch: [0; 240],
            sysram: [0; 352],
            exec: be_words(exec),
            grom: grom.to_vec(),
            gram: [0; 512],
            cart,
            mapped,
        };
        let mut cpu = Cpu::new();
        cpu.reset();
        Ok(Intellivision { cpu, bus })
    }

    pub fn reset(&mut self) {
        self.cpu.reset();
    }

    fn run_cycles(&mut self, n: u32) {
        let mut done = 0;
        while done < n {
            let c = self.cpu.step(&mut self.bus);
            self.bus.psg.tick(c);
            done += c;
        }
    }

    /// One video frame: vertical-blank interrupt, the game's VBLANK work, then the picture.
    pub fn run_frame(&mut self) {
        self.cpu.irq = true;
        self.run_cycles(VBLANK_CYCLES);
        let backtab = self.bus.sysram[..240].to_vec();
        let Bus { stic, grom, gram, .. } = &mut self.bus;
        stic.render(&backtab, grom, gram);
        self.run_cycles(CYCLES_PER_FRAME - VBLANK_CYCLES);
    }

    pub fn frame(&self) -> &[u32] {
        &self.bus.stic.frame
    }

    /// Pad 0 = left controller ($1FF), pad 1 = right ($1FE).
    pub fn set_buttons(&mut self, pad: usize, b: u16) {
        let bits = psg::controller_bits(b);
        match pad {
            0 => self.bus.psg.ports[1] = bits,
            1 => self.bus.psg.ports[0] = bits,
            _ => {}
        }
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.bus.psg.samples);
    }

    pub fn set_sample_rate(&mut self, r: u32) {
        self.bus.psg.set_sample_rate(r);
    }

    pub fn cpu(&self) -> &Cpu {
        &self.cpu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in EXEC: at reset it jumps to the cartridge at $5000; its interrupt handler
    /// ($1004) enables the display and returns. The test cartridge fills BACKTAB.
    fn fake_exec() -> Vec<u8> {
        let mut w = vec![0u16; 4096];
        // $1000: J $5000
        w[0] = 0x004;
        w[1] = 0x300 | (0x5000 >> 10) << 2;
        w[2] = 0x000;
        // $1004: MVO R0,$0020 (display enable) ; MVI@ R6,R7 (return)
        w[4] = 0x240;
        w[5] = 0x020;
        w[6] = 0x2B7;
        w.iter().flat_map(|x| x.to_be_bytes()).collect()
    }

    #[test]
    fn boots_cart_and_draws_backtab() {
        let mut grom = vec![0u8; 2048];
        grom[8..16].fill(0xFF); // card 1 solid
        // Cart: MVII #$2F0,R6 ; EIS ; MVII #(card1|white),R0 ; MVO R0,$200 ; loop: B loop
        let prog: [u16; 9] = [0x2BE, 0x2F0, 0x002, 0x2B8, 1 << 3 | 7, 0x240, 0x200, 0x220, 1];
        let rom: Vec<u8> = prog.iter().flat_map(|x| x.to_be_bytes()).collect();
        let mut m = Intellivision::new(&fake_exec(), &grom, &rom).unwrap();
        m.run_frame();
        m.run_frame();
        assert_eq!(m.bus.sysram[0], 1 << 3 | 7);
        assert_eq!(m.frame()[0], stic::PALETTE[7], "white card at the top-left");
        assert_eq!(m.frame()[8], stic::PALETTE[0], "next card: colour stack 0 (black)");
    }

    #[test]
    fn rejects_bad_bios() {
        assert!(Intellivision::new(&[0; 100], &[0; 2048], &[0; 2]).is_err());
    }
}
