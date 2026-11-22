//! The PlayStation 2's second processor: the I/O processor (IOP), a PlayStation 1 CPU
//! (MIPS R3000A, our `cpur3000` core) with 2 MB of its own memory. It runs the console's
//! drivers (disc, controllers, memory cards, sound) and talks to the Emotion Engine through
//! the SIF registers.

use crate::sif::Sif;
use cpur3000::Bus;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

pub const IOP_RAM: usize = 2 << 20;
pub const IOP_HZ: u32 = 36_864_000;

pub struct IopBus {
    pub ram: Vec<u8>,
    pub spr: Vec<u8>,
    rom: Rc<Vec<u8>>,
    sif: Rc<RefCell<Sif>>,
    pub i_stat: u32,
    pub i_mask: u32,
    pub i_ctrl: u32,
    /// Registers of blocks we don't model (disc, sound, controllers, DMA...), by address.
    regs: BTreeMap<u32, u32>,
    pub unhandled: BTreeMap<u32, (u32, u32)>,
    /// Free-running counters (the root counters) advance with the CPU.
    pub ticks: u64,
}

impl IopBus {
    pub fn new(rom: Rc<Vec<u8>>, sif: Rc<RefCell<Sif>>) -> IopBus {
        IopBus { ram: vec![0; IOP_RAM], spr: vec![0; 0x400], rom, sif, i_stat: 0, i_mask: 0, i_ctrl: 0, regs: BTreeMap::new(), unhandled: BTreeMap::new(), ticks: 0 }
    }

    pub fn irq_line(&self) -> bool {
        self.i_ctrl & 1 != 0 && self.i_stat & self.i_mask != 0
    }

    pub fn raise(&mut self, cause: u32) {
        self.i_stat |= 1 << cause;
    }

    fn note(&mut self, a: u32, write: bool) {
        let e = self.unhandled.entry(a & !3).or_insert((0, 0));
        if write {
            e.1 += 1;
        } else {
            e.0 += 1;
        }
    }
}

fn get(buf: &[u8], off: usize, size: u32) -> u32 {
    let mut b = [0u8; 4];
    let n = size as usize;
    if let Some(s) = buf.get(off..off + n) {
        b[..n].copy_from_slice(s);
    }
    u32::from_le_bytes(b)
}

fn put(buf: &mut [u8], off: usize, v: u32, size: u32) {
    let n = size as usize;
    if let Some(s) = buf.get_mut(off..off + n) {
        s.copy_from_slice(&v.to_le_bytes()[..n]);
    }
}

impl Bus for IopBus {
    fn read(&mut self, addr: u32, size: u32) -> u32 {
        let a = addr & 0x1FFF_FFFF;
        match a {
            0x0000_0000..=0x007F_FFFF => get(&self.ram, a as usize & (IOP_RAM - 1), size),
            0x1D00_0000..=0x1D00_FFFF => self.sif.borrow_mut().iop_read(a),
            0x1F80_0000..=0x1F80_03FF => get(&self.spr, (a & 0x3FF) as usize, size),
            0x1F80_1070 => self.i_stat,
            0x1F80_1074 => self.i_mask,
            0x1F80_1078 => {
                // Reading the master enable also clears it (a quirk the kernel relies on).
                let v = self.i_ctrl;
                self.i_ctrl = 0;
                v
            }
            // Root counters 0..2 (0x1F801100..) and 3..5 (0x1F801480..): free-running.
            0x1F80_1100..=0x1F80_112F | 0x1F80_1480..=0x1F80_14AF => {
                let n = if a < 0x1F80_1200 { (a - 0x1F80_1100) / 0x10 } else { (a - 0x1F80_1480) / 0x10 + 3 };
                match a & 0xF {
                    0 => (self.ticks >> n.min(2)) as u32,
                    _ => self.regs.get(&a).copied().unwrap_or(0),
                }
            }
            0x1F80_1000..=0x1FFF_FFFF if a >= 0x1FC0_0000 => get(&self.rom, (a - 0x1FC0_0000) as usize, size),
            // Sound chip status (both cores): ready for transfers (bit 7), not busy (bit 10 clear).
            0x1F80_1344 | 0x1F80_1DAC | 0x1F80_1DAE | 0x1F80_1744 => 0x0080,
            0x1F80_1000..=0x1FBF_FFFF => {
                self.note(a, false);
                self.regs.get(&(a & !3)).copied().unwrap_or(0)
            }
            _ => {
                self.note(a, false);
                0
            }
        }
    }

    fn write(&mut self, addr: u32, v: u32, size: u32) {
        let a = addr & 0x1FFF_FFFF;
        match a {
            0x0000_0000..=0x007F_FFFF => put(&mut self.ram, a as usize & (IOP_RAM - 1), v, size),
            0x1D00_0000..=0x1D00_FFFF => self.sif.borrow_mut().iop_write(a, v),
            0x1F80_0000..=0x1F80_03FF => put(&mut self.spr, (a & 0x3FF) as usize, v, size),
            // Writing 0 to a bit of I_STAT clears it.
            0x1F80_1070 => self.i_stat &= v,
            0x1F80_1074 => self.i_mask = v,
            0x1F80_1078 => self.i_ctrl = v & 1,
            0x1F80_1000..=0x1FBF_FFFF => {
                self.note(a, true);
                self.regs.insert(a & !3, v);
            }
            _ => self.note(a, true),
        }
    }

    fn gte_read(&mut self, _reg: u32) -> u32 {
        0
    }
    fn gte_write(&mut self, _reg: u32, _v: u32) {}
    fn gte_command(&mut self, _cmd: u32) {}
}
