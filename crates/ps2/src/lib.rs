//! PlayStation 2, work in progress.
//!
//! Built so far: the Emotion Engine CPU (`cpur5900`) wired to the console's physical memory
//! map: 32 MB main memory, the 4 MB boot ROM (the user's BIOS), the 16 KB scratchpad,
//! the interrupt controller (INTC), and a register file for the other hardware blocks
//! (timers, DMA, GIF, VIF, SIF) that remembers what software wrote and logs every register
//! we don't model yet. That log is how the missing pieces get found.
//!
//! Not built yet: the I/O processor (the PlayStation 1 CPU inside) and its devices, DMA
//! transfers, the vector units, the Graphics Synthesizer and the sound chip.

pub mod iop;
pub mod sif;

use cpur5900::{Bus, Cpu};
use iop::IopBus;
use sif::Sif;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

pub const RAM_SIZE: usize = 32 << 20;
pub const ROM_SIZE: usize = 4 << 20;
/// Emotion Engine clock, divided down: we run fewer instructions per second than the real chip.
pub const EE_HZ: u32 = 294_912_000;
pub const FPS: f64 = 59.94;

/// Physical memory map and hardware registers seen by the Emotion Engine.
pub struct EeBus {
    pub ram: Vec<u8>,
    pub rom: Rc<Vec<u8>>,
    pub spr: Vec<u8>,
    pub sif: Rc<RefCell<Sif>>,
    /// Backing store for 0x10000000..0x1000FFFF: software sees back what it wrote.
    regs: Vec<u32>,
    pub intc_stat: u32,
    pub intc_mask: u32,
    /// The four EE timers: counter, mode, compare, and the leftover clock ticks.
    pub t_count: [u32; 4],
    pub t_mode: [u32; 4],
    pub t_comp: [u32; 4],
    t_frac: [u32; 4],
    /// The Rambus memory controller (MCH): last command and drd value, and how many memory
    /// devices have been numbered so far.
    mch_ricm: u32,
    mch_drd: u32,
    rdram_sdevid: u32,
    /// First memory-controller accesses, for diagnostics.
    pub mch_log: Vec<String>,
    /// First hardware-register accesses of any kind (address, read/write, value).
    pub reg_log: Vec<String>,
    /// Register accesses to blocks we don't model: address -> (reads, writes).
    pub unhandled: BTreeMap<u32, (u32, u32)>,
    pub console_output: String,
}

impl EeBus {
    fn new(rom: Rc<Vec<u8>>, sif: Rc<RefCell<Sif>>) -> EeBus {
        let mut regs = vec![0u32; 0x4000];
        regs[(0xF520 >> 2) as usize] = 0x1201; // D_ENABLER at power-on
        EeBus { ram: vec![0; RAM_SIZE], rom, sif, spr: vec![0; 0x4000], regs, intc_stat: 0, intc_mask: 0, t_count: [0; 4], t_mode: [0; 4], t_comp: [0; 4], t_frac: [0; 4], mch_ricm: 0, mch_drd: 0, rdram_sdevid: 0, mch_log: Vec::new(), reg_log: Vec::new(), unhandled: BTreeMap::new(), console_output: String::new() }
    }

    fn note(&mut self, addr: u32, write: bool) {
        let e = self.unhandled.entry(addr & !3).or_insert((0, 0));
        if write {
            e.1 += 1;
        } else {
            e.0 += 1;
        }
    }

    /// The INTC line to the CPU (IP2): any unmasked pending cause.
    pub fn intc_line(&self) -> bool {
        self.intc_stat & self.intc_mask != 0
    }

    /// Raises an INTC cause (bit numbers as on the console: 0 GS, 1 SBUS, 2 VBON, 3 VBOF ...).
    pub fn raise_intc(&mut self, cause: u32) {
        self.intc_stat |= 1 << cause;
    }

    /// Advances the timers by `cycles` EE clocks. They count the bus clock (half the EE
    /// clock) divided by 1, 16 or 256, or horizontal sync lines, and raise INTC 9..12 when
    /// they reach their compare value or overflow.
    pub fn tick_timers(&mut self, cycles: u32) {
        for n in 0..4 {
            let mode = self.t_mode[n];
            if mode & 0x80 == 0 {
                continue; // counting disabled
            }
            let div = match mode & 3 {
                0 => 2,
                1 => 32,
                2 => 512,
                _ => 18_700, // hsync: about 15.7 kHz
            };
            self.t_frac[n] += cycles;
            let ticks = self.t_frac[n] / div;
            self.t_frac[n] %= div;
            for _ in 0..ticks.min(1 << 20) {
                let c = self.t_count[n].wrapping_add(1) & 0xFFFF;
                if c == self.t_comp[n] & 0xFFFF {
                    self.t_mode[n] |= 1 << 10;
                    if mode & 0x100 != 0 {
                        self.raise_intc(9 + n as u32);
                    }
                    if mode & 0x40 != 0 {
                        self.t_count[n] = 0;
                        continue;
                    }
                }
                if c == 0 {
                    self.t_mode[n] |= 1 << 11;
                    if mode & 0x200 != 0 {
                        self.raise_intc(9 + n as u32);
                    }
                }
                self.t_count[n] = c;
            }
        }
    }

    fn timer_read(&self, addr: u32) -> Option<u32> {
        let n = ((addr >> 11) & 3) as usize;
        Some(match addr & 0x7FF {
            0x00 => self.t_count[n],
            0x10 => self.t_mode[n],
            0x20 => self.t_comp[n],
            0x30 => 0,
            _ => return None,
        })
    }

    fn timer_write(&mut self, addr: u32, v: u32) -> bool {
        let n = ((addr >> 11) & 3) as usize;
        match addr & 0x7FF {
            0x00 => self.t_count[n] = v & 0xFFFF,
            // The two flag bits clear when written as 1; the rest is stored.
            0x10 => self.t_mode[n] = (self.t_mode[n] & 0xC00 & !v) | (v & 0x3FF),
            0x20 => self.t_comp[n] = v & 0xFFFF,
            0x30 => {}
            _ => return false,
        }
        true
    }

    fn reg_read(&mut self, addr: u32, size: u32) -> u64 {
        let v = self.reg_read_inner(addr, size);
        if self.reg_log.len() < 400 && addr != 0x1000_0000 && addr != 0x1000_F130 {
            self.reg_log.push(format!("R{size} {addr:08x} -> {v:x}"));
        }
        v
    }

    fn reg_read_inner(&mut self, addr: u32, size: u32) -> u64 {
        if addr < 0x1000_2000 {
            if let Some(v) = self.timer_read(addr) {
                return v as u64;
            }
        }
        if let Some(v) = (0x1000_F200..0x1000_F270).contains(&addr).then(|| self.sif.borrow().ee_read(addr)).flatten() {
            return v as u64;
        }
        match addr {
            0x1000_F000 => self.intc_stat as u64,
            0x1000_F010 => self.intc_mask as u64,
            // Memory controller: answers the start-up probe of the two memory devices.
            0x1000_F440 => {
                if self.mch_log.len() < 120 {
                    self.mch_log.push(format!("read DRD (ricm={:08x})", self.mch_ricm));
                }
                if (self.mch_ricm >> 6) & 0xF == 0 {
                    match (self.mch_ricm >> 16) & 0xFFF {
                        0x21 => {
                            if self.rdram_sdevid < 2 {
                                self.rdram_sdevid += 1;
                                return 0x1F;
                            }
                            return 0;
                        }
                        0x23 => return 0x0D0D,
                        0x24 => return 0x0090,
                        0x40 => return (self.mch_drd & 0x1F) as u64,
                        _ => {}
                    }
                }
                0
            }
            0x1000_F430 => self.mch_ricm as u64,
            _ => {
                self.note(addr, false);
                let i = ((addr & 0xFFFF) >> 2) as usize;
                let lo = self.regs[i] as u64;
                if size == 8 {
                    lo | (self.regs[(i + 1) % self.regs.len()] as u64) << 32
                } else {
                    lo
                }
            }
        }
    }

    fn reg_write(&mut self, addr: u32, v: u64, size: u32) {
        if self.reg_log.len() < 400 {
            self.reg_log.push(format!("W{size} {addr:08x} = {v:x}"));
        }
        if addr < 0x1000_2000 && self.timer_write(addr, v as u32) {
            return;
        }
        if (0x1000_F200..0x1000_F270).contains(&addr) && self.sif.borrow_mut().ee_write(addr, v as u32) {
            return;
        }
        match addr {
            0x1000_F430 => {
                if self.mch_log.len() < 120 {
                    self.mch_log.push(format!("write RICM {:08x}", v as u32));
                }
                // Starting the numbering sequence: the init register written (not read) while
                // the data register's bit 7 is clear resets the device counter.
                if (v as u32 >> 16) & 0xFFF == 0x21 && (v as u32 >> 6) & 0xF == 1 && (self.mch_drd >> 7) & 1 == 0 {
                    self.rdram_sdevid = 0;
                }
                self.mch_ricm = v as u32 & !0x8000_0000;
            }
            // Memory-controller command registers: bit 31 is a "start" bit that the hardware
            // clears when the command finishes (immediately here).
            0x1000_F400..=0x1000_F42F => {
                self.note(addr, true);
                let i = ((addr & 0xFFFF) >> 2) as usize;
                self.regs[i] = v as u32 & !0x8000_0000;
            }
            0x1000_F440 => {
                if self.mch_log.len() < 120 {
                    self.mch_log.push(format!("write DRD {:08x}", v as u32));
                }
                self.mch_drd = v as u32
            }
            // Writing 1s clears pending causes; writing 1s to the mask toggles those bits.
            0x1000_F000 => self.intc_stat &= !(v as u32),
            0x1000_F010 => self.intc_mask ^= v as u32,
            _ => {
                self.note(addr, true);
                let i = ((addr & 0xFFFF) >> 2) as usize;
                self.regs[i] = v as u32;
                if size == 8 {
                    let n = (i + 1) % self.regs.len();
                    self.regs[n] = (v >> 32) as u32;
                }
            }
        }
    }
}

fn get(buf: &[u8], off: usize, size: u32) -> u64 {
    let mut b = [0u8; 8];
    let n = size as usize;
    if let Some(s) = buf.get(off..off + n) {
        b[..n].copy_from_slice(s);
    }
    u64::from_le_bytes(b)
}

fn put(buf: &mut [u8], off: usize, v: u64, size: u32) {
    let n = size as usize;
    if let Some(s) = buf.get_mut(off..off + n) {
        s.copy_from_slice(&v.to_le_bytes()[..n]);
    }
}

impl Bus for EeBus {
    fn read(&mut self, addr: u32, size: u32) -> u64 {
        match addr {
            0x0000_0000..=0x01FF_FFFF => get(&self.ram, addr as usize, size),
            0x1000_0000..=0x1000_FFFF => self.reg_read(addr, size),
            0x1FC0_0000..=0x1FFF_FFFF => get(&self.rom, (addr - 0x1FC0_0000) as usize, size),
            cpur5900::SCRATCHPAD_BASE..=0x7000_3FFF => get(&self.spr, (addr - cpur5900::SCRATCHPAD_BASE) as usize, size),
            _ => {
                self.note(addr, false);
                0
            }
        }
    }

    fn write(&mut self, addr: u32, v: u64, size: u32) {
        match addr {
            0x0000_0000..=0x01FF_FFFF => put(&mut self.ram, addr as usize, v, size),
            // The kernel's debug console (SIO) shows boot messages.
            0x1000_F180 => self.console_output.push(v as u8 as char),
            0x1000_0000..=0x1000_FFFF => self.reg_write(addr, v, size),
            0x7000_0000..=0x7000_3FFF => put(&mut self.spr, (addr - 0x7000_0000) as usize, v, size),
            _ => self.note(addr, true),
        }
    }
}

pub struct Ps2 {
    pub cpu: Cpu,
    pub bus: EeBus,
    pub iop: cpur3000::Cpu<IopBus>,
    pub sif: Rc<RefCell<Sif>>,
    frame: Vec<u32>,
    pub frames: u64,
}

impl Ps2 {
    /// A console with the user's BIOS (a 4 MB boot ROM dump, or a 2/3 MB one).
    pub fn new(bios: Vec<u8>) -> Result<Ps2, String> {
        if bios.len() < 1 << 20 || bios.len() > ROM_SIZE {
            return Err(format!("The PlayStation 2 BIOS should be 2 to 4 MB (this file is {} bytes).", bios.len()));
        }
        let mut rom = bios;
        rom.resize(ROM_SIZE, 0xFF);
        let rom = Rc::new(rom);
        let sif = Rc::new(RefCell::new(Sif::default()));
        let mut iop = cpur3000::Cpu::new(IopBus::new(rom.clone(), sif.clone()));
        iop.reset();
        Ok(Ps2 { cpu: Cpu::new(), bus: EeBus::new(rom, sif.clone()), iop, sif, frame: vec![0; 640 * 448], frames: 0 })
    }

    /// Runs about one video frame of Emotion Engine instructions.
    pub fn run_frame(&mut self) {
        // The real chip runs ~4.9M cycles per frame; an interpreter does a fraction.
        const PER_FRAME: u32 = 600_000;
        // The IOP runs at one eighth of the EE's clock.
        const CHUNK: u32 = 4000;
        let mut left = PER_FRAME;
        while left > 0 {
            self.cpu.irq = (self.bus.intc_line() as u32) | self.cpu.irq & !1;
            let chunk = CHUNK.min(left);
            let used = self.cpu.run(&mut self.bus, chunk);
            self.bus.tick_timers(used);
            left = left.saturating_sub(used);
            self.iop.set_irq(self.iop.bus().irq_line());
            for _ in 0..used / 8 {
                self.iop.step();
            }
            self.iop.bus_mut().ticks += (used / 8) as u64;
        }
        // Vertical blank: INTC cause 2 (VBON), then 3 (VBOF); the IOP's IRQ 0 is its VBLANK.
        self.bus.raise_intc(2);
        self.bus.raise_intc(3);
        self.iop.bus_mut().raise(0);
        self.frames += 1;
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn size(&self) -> (usize, usize) {
        (640, 448)
    }

    pub fn pc(&self) -> u32 {
        self.cpu.pc
    }

    /// The I/O processor's program counter.
    pub fn iop_pc(&self) -> u32 {
        self.iop.regs().pc
    }

    /// The hardware registers the program has touched that we don't model, busiest first.
    pub fn unhandled(&self) -> Vec<(u32, u32, u32)> {
        let mut v: Vec<_> = self.bus.unhandled.iter().map(|(a, (r, w))| (*a, *r, *w)).collect();
        v.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom_with(words: &[u32]) -> Vec<u8> {
        let mut rom = vec![0u8; 2 << 20];
        for (i, w) in words.iter().enumerate() {
            rom[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        rom
    }

    fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
        op << 26 | rs << 21 | rt << 16 | (imm & 0xFFFF)
    }

    #[test]
    fn boots_from_the_rom_and_uses_memory_and_hardware() {
        // Writes to RAM and to an unmodelled register, then spins.
        let rom = rom_with(&[
            i(15, 0, 1, 0xA000),     // lui r1,0xA000        (uncached RAM at 0)
            i(9, 0, 2, 0x55),        // addiu r2,r0,0x55
            i(43, 1, 2, 0x1000),     // sw r2,0x1000(r1)
            i(15, 0, 3, 0xB000),     // lui r3,0xB000        (uncached 0x10000000: hardware registers)
            i(43, 3, 2, 0x3000),     // sw r2,0x3000(r3)     (GIF block: not modelled)
            i(4, 0, 0, 0xFFFF),      // beq r0,r0,-1  (spin)
            0,
        ]);
        let mut m = Ps2::new(rom).unwrap();
        for _ in 0..20 {
            m.cpu.step(&mut m.bus);
        }
        assert_eq!(&m.bus.ram[0x1000..0x1004], &[0x55, 0, 0, 0]);
        assert_eq!(m.unhandled().first().map(|u| (u.0, u.2)), Some((0x1000_3000, 1)));
    }

    /// The two processors boot from the same ROM; this one tells them apart by the processor
    /// id, then each side sets a flag and a command word for the other and waits for the
    /// other's flag (the handshake the real BIOS performs at start-up).
    #[test]
    fn the_processors_talk_through_the_sif() {
        let r = |funct: u32, rs: u32, rt: u32, rd: u32, sa: u32| rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct;
        let ori = |rt: u32, rs: u32, imm: u32| i(13, rs, rt, imm);
        let words = [
            0x4000_0000 | (8 << 16) | (15 << 11), // 0: mfc0 t0,PRId
            r(2, 0, 8, 8, 8),                      // 1: srl t0,t0,8
            i(5, 8, 0, 15),                        // 2: bne t0,zero,+15 -> EE path (index 18)
            0,                                     // 3: nop
            // ---- I/O processor (indices 4..17)
            i(15, 0, 9, 0x1D00),                   // 4: lui t1,0x1D00
            ori(10, 0, 0x1234),                    // 5
            i(43, 9, 10, 0x10),                    // 6: sw t2,0x10(t1)   smcom = 0x1234
            ori(10, 0, 2),                         // 7
            i(43, 9, 10, 0x30),                    // 8: sw t2,0x30(t1)   smflg |= 2
            i(35, 9, 12, 0x20),                    // 9: lw t4,0x20(t1)   loop: msflg
            0,                                     // 10: load delay
            i(12, 12, 13, 4),                      // 11: andi t5,t4,4
            i(4, 13, 0, 0xFFFC),                   // 12: beq t5,zero,-4
            0,                                     // 13
            i(35, 9, 11, 0),                       // 14: lw t3,0(t1)     mscom
            0,                                     // 15
            i(4, 0, 0, 0xFFFF),                    // 16: spin
            0,                                     // 17
            // ---- Emotion Engine (indices 18..)
            i(15, 0, 9, 0xB000),                   // 18: lui t1,0xB000
            ori(9, 9, 0xF200),                     // 19: ori t1,t1,0xF200
            ori(10, 0, 0x5678),                    // 20
            i(43, 9, 10, 0),                       // 21: sw t2,0(t1)     mscom = 0x5678
            ori(11, 0, 4),                         // 22
            i(43, 9, 11, 0x20),                    // 23: sw t3,0x20(t1)  msflg |= 4
            i(35, 9, 12, 0x30),                    // 24: lw t4,0x30(t1)  loop: smflg
            i(12, 12, 13, 2),                      // 25: andi t5,t4,2
            i(4, 13, 0, 0xFFFD),                   // 26: beq t5,zero,-3
            0,                                     // 27
            i(35, 9, 14, 0x10),                    // 28: lw t6,0x10(t1)  smcom
            i(4, 0, 0, 0xFFFF),                    // 29: spin
            0,                                     // 30
        ];
        let mut m = Ps2::new(rom_with(&words)).unwrap();
        m.run_frame();
        assert_eq!(m.cpu.reg(14), 0x1234, "the EE read the IOP's command word");
        assert_eq!(m.iop.regs().r[11], 0x5678, "the IOP read the EE's command word");
        let s = m.sif.borrow();
        assert_eq!(s.smflg, 2);
        assert_eq!(s.msflg, 4, "the EE's flag stays set until the IOP clears it");
        drop(s);
        // The IOP clears an EE flag by writing its bits to the same register.
        m.sif.borrow_mut().iop_write(0x1D00_0020, 4);
        assert_eq!(m.sif.borrow().msflg, 0);
    }

    #[test]
    fn timers_count_and_interrupt() {
        let mut m = Ps2::new(rom_with(&[0])).unwrap();
        m.bus.write(0x1000_0020, 100, 4); // compare
        m.bus.write(0x1000_0010, 0x80 | 0x100, 4); // enabled, bus clock, compare interrupt
        m.bus.tick_timers(2 * 50);
        assert_eq!(m.bus.read(0x1000_0000, 4), 50);
        assert!(!m.bus.intc_line());
        m.bus.write(0x1000_F010, 1 << 9, 4); // unmask timer 0
        m.bus.tick_timers(2 * 50);
        assert!(m.bus.intc_line(), "reaching the compare value raises INTC 9");
        assert!(m.bus.read(0x1000_0010, 4) & (1 << 10) != 0, "compare flag is set");
        m.bus.write(0x1000_0010, 0x80 | 0x100 | (1 << 10), 4);
        assert!(m.bus.read(0x1000_0010, 4) & (1 << 10) == 0, "writing 1 clears it");
    }

    #[test]
    fn interrupt_controller_masks_and_clears() {
        let mut m = Ps2::new(rom_with(&[0])).unwrap();
        m.bus.raise_intc(2);
        assert!(!m.bus.intc_line(), "masked by default");
        m.bus.write(0x1000_F010, 1 << 2, 4); // toggling the mask bit enables it
        assert!(m.bus.intc_line());
        m.bus.write(0x1000_F000, 1 << 2, 4); // writing 1 clears the cause
        assert!(!m.bus.intc_line());
    }

    #[test]
    fn rejects_files_that_are_not_bios_dumps() {
        assert!(Ps2::new(vec![0; 100]).is_err());
        assert!(Ps2::new(vec![0; 8 << 20]).is_err());
    }
}
