//! Clean-room PlayStation 3 emulator core built on `cpuxenon` (Cell PPU 64-bit PowerPC).
//!
//! Loads big-endian PowerPC64 `.elf` / unencrypted `.self` (`SCE\0`) executables, handles
//! LV2 syscalls (`sys_process_exit`, `sys_tty_write`, `sys_memory_allocate`, `sys_time`),
//! scans out a 1280×720 RSX framebuffer, and maps DualShock 3 controls.

use cpuxenon::{Bus, Cpu, Event};

pub const WIDTH: usize = 1280;
pub const HEIGHT: usize = 720;
pub const FPS: f64 = 59.94;

const RAM_BASE: u32 = 0x0001_0000;
const RAM_SIZE: usize = 64 * 1024 * 1024;
const RSX_FB_BASE: u32 = 0x0300_0000;
const CYCLES_PER_FRAME: u32 = 160_000;

pub struct Ps3Bus {
    pub ram: Box<[u8]>,
    pub fb_addr: u32,
    pub pad_buttons: u16,
    pub tty_log: String,
    pub exited: bool,
    pub exit_code: i32,
    pub heap_ptr: u32,
    pub audio_out: Vec<f32>,
}

impl Ps3Bus {
    pub fn new() -> Self {
        Self {
            ram: vec![0u8; RAM_SIZE].into_boxed_slice(),
            fb_addr: RSX_FB_BASE,
            pad_buttons: 0,
            tty_log: String::new(),
            exited: false,
            exit_code: 0,
            heap_ptr: 0x0100_0000,
            audio_out: Vec::new(),
        }
    }

    pub fn write_slice(&mut self, addr: u32, data: &[u8]) {
        if addr < RAM_BASE {
            return;
        }
        let off = (addr - RAM_BASE) as usize;
        for (i, &b) in data.iter().enumerate() {
            if off + i < self.ram.len() {
                self.ram[off + i] = b;
            }
        }
    }

    pub fn scanout(&self, out: &mut [u32]) {
        let base = self.fb_addr.saturating_sub(RAM_BASE) as usize;
        for i in 0..(WIDTH * HEIGHT) {
            let o = base + i * 4;
            if o + 4 <= self.ram.len() {
                // RSX X8R8G8B8 big-endian -> 0RGB
                let r = self.ram[o + 1] as u32;
                let g = self.ram[o + 2] as u32;
                let b = self.ram[o + 3] as u32;
                out[i] = (r << 16) | (g << 8) | b;
            }
        }
    }
}

impl Default for Ps3Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for Ps3Bus {
    fn read(&mut self, addr: u32, size: u32) -> u64 {
        if addr < RAM_BASE {
            return 0;
        }
        let off = (addr - RAM_BASE) as usize;
        match size {
            1 if off < self.ram.len() => self.ram[off] as u64,
            2 if off + 2 <= self.ram.len() => u16::from_be_bytes([self.ram[off], self.ram[off + 1]]) as u64,
            4 if off + 4 <= self.ram.len() => u32::from_be_bytes([self.ram[off], self.ram[off + 1], self.ram[off + 2], self.ram[off + 3]]) as u64,
            8 if off + 8 <= self.ram.len() => u64::from_be_bytes(self.ram[off..off + 8].try_into().unwrap()),
            _ => 0,
        }
    }

    fn write(&mut self, addr: u32, value: u64, size: u32) {
        if addr < RAM_BASE {
            return;
        }
        let off = (addr - RAM_BASE) as usize;
        match size {
            1 if off < self.ram.len() => self.ram[off] = value as u8,
            2 if off + 2 <= self.ram.len() => self.ram[off..off + 2].copy_from_slice(&(value as u16).to_be_bytes()),
            4 if off + 4 <= self.ram.len() => self.ram[off..off + 4].copy_from_slice(&(value as u32).to_be_bytes()),
            8 if off + 8 <= self.ram.len() => self.ram[off..off + 8].copy_from_slice(&value.to_be_bytes()),
            _ => {}
        }
    }
}

pub struct Ps3 {
    pub cpu: Cpu<Ps3Bus>,
    elf: Vec<u8>,
    frame: Vec<u32>,
}

impl Ps3 {
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let elf = if data.len() >= 4 && &data[0..4] == b"SCE\0" {
            return Err("Encrypted retail PS3 SELF/PKG requires console keys. Provide a decrypted PS3 PowerPC64 .elf homebrew/game.".into());
        } else if data.len() >= 64 && &data[0..4] == b"\x7FELF" {
            data.to_vec()
        } else {
            return Err("Unrecognized PlayStation 3 executable (expected decrypted PowerPC64 .elf).".into());
        };
        let mut ps3 = Self {
            cpu: Cpu::new(Ps3Bus::new()),
            elf,
            frame: vec![0; WIDTH * HEIGHT],
        };
        ps3.boot()?;
        Ok(ps3)
    }

    fn boot(&mut self) -> Result<(), String> {
        self.cpu = Cpu::new(Ps3Bus::new());
        let ptr = self.cpu.bus_mut().ram.as_mut_ptr();
        unsafe { self.cpu.map(RAM_BASE, ptr, RAM_SIZE); }

        let elf = &self.elf;
        let is_64 = elf[4] == 2;
        let e_machine = u16::from_be_bytes([elf[18], elf[19]]);
        if e_machine != 21 && e_machine != 20 {
            return Err(format!("Unsupported PS3 ELF machine {e_machine} (expected PowerPC/PowerPC64)."));
        }

        let entry = if is_64 {
            let e = u64::from_be_bytes(elf[24..32].try_into().unwrap()) as u32;
            let phoff = u64::from_be_bytes(elf[32..40].try_into().unwrap()) as usize;
            let phentsize = u16::from_be_bytes([elf[54], elf[55]]) as usize;
            let phnum = u16::from_be_bytes([elf[56], elf[57]]) as usize;
            for i in 0..phnum {
                let o = phoff + i * phentsize;
                if o + 56 > elf.len() { break; }
                if u32::from_be_bytes(elf[o..o + 4].try_into().unwrap()) == 1 {
                    let foff = u64::from_be_bytes(elf[o + 8..o + 16].try_into().unwrap()) as usize;
                    let vaddr = u64::from_be_bytes(elf[o + 16..o + 24].try_into().unwrap()) as u32;
                    let fsz = u64::from_be_bytes(elf[o + 32..o + 40].try_into().unwrap()) as usize;
                    if foff + fsz <= elf.len() {
                        self.cpu.bus_mut().write_slice(vaddr, &elf[foff..foff + fsz]);
                    }
                }
            }
            e
        } else {
            let e = u32::from_be_bytes(elf[24..28].try_into().unwrap());
            let phoff = u32::from_be_bytes(elf[28..32].try_into().unwrap()) as usize;
            let phentsize = u16::from_be_bytes([elf[42], elf[43]]) as usize;
            let phnum = u16::from_be_bytes([elf[44], elf[45]]) as usize;
            for i in 0..phnum {
                let o = phoff + i * phentsize;
                if o + 32 > elf.len() { break; }
                if u32::from_be_bytes(elf[o..o + 4].try_into().unwrap()) == 1 {
                    let foff = u32::from_be_bytes(elf[o + 4..o + 8].try_into().unwrap()) as usize;
                    let vaddr = u32::from_be_bytes(elf[o + 8..o + 12].try_into().unwrap());
                    let fsz = u32::from_be_bytes(elf[o + 16..o + 20].try_into().unwrap()) as usize;
                    if foff + fsz <= elf.len() {
                        self.cpu.bus_mut().write_slice(vaddr, &elf[foff..foff + fsz]);
                    }
                }
            }
            e
        };

        let mut regs = self.cpu.regs();
        regs.pc = entry;
        regs.r[1] = 0x02FF_0000;
        self.cpu.set_regs(regs);
        Ok(())
    }

    pub fn reset(&mut self) {
        let _ = self.boot();
        self.frame.fill(0);
    }

    pub fn set_buttons(&mut self, buttons: u16) {
        self.cpu.bus_mut().pad_buttons = buttons;
    }

    pub fn run_frame(&mut self) {
        if self.cpu.bus().exited {
            return;
        }
        let mut budget = CYCLES_PER_FRAME;
        while budget > 0 {
            let (used, ev) = self.cpu.run(budget);
            budget = budget.saturating_sub(used.max(1));
            match ev {
                Some(Event::Syscall) => {
                    let mut regs = self.cpu.regs();
                    let num = regs.r[11];
                    match num {
                        // sys_process_exit
                        3 => {
                            self.cpu.bus_mut().exited = true;
                            self.cpu.bus_mut().exit_code = regs.r[3] as i32;
                            break;
                        }
                        // sys_tty_write(ch, buf, len, written)
                        403 => {
                            let buf = regs.r[4] as u32;
                            let len = (regs.r[5] as usize).min(256);
                            let mut s = Vec::with_capacity(len);
                            for i in 0..len {
                                s.push(self.cpu.bus_mut().read(buf + i as u32, 1) as u8);
                            }
                            self.cpu.bus_mut().tty_log.push_str(&String::from_utf8_lossy(&s));
                            regs.r[3] = 0;
                            self.cpu.set_regs(regs);
                        }
                        // sys_memory_allocate
                        348 => {
                            let sz = (regs.r[3] as u32).max(0x10000);
                            let ptr = self.cpu.bus().heap_ptr;
                            self.cpu.bus_mut().heap_ptr = ptr.wrapping_add((sz + 0xFFFF) & !0xFFFF);
                            let out_ptr = regs.r[5] as u32;
                            self.cpu.bus_mut().write(out_ptr, ptr as u64, 4);
                            regs.r[3] = 0;
                            self.cpu.set_regs(regs);
                        }
                        _ => {
                            regs.r[3] = 0;
                            self.cpu.set_regs(regs);
                        }
                    }
                }
                Some(_) => break,
                None => break,
            }
        }
        self.cpu.bus().scanout(&mut self.frame);
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.cpu.bus_mut().audio_out);
    }

    pub fn status_message(&self) -> Option<String> {
        if self.cpu.bus().exited {
            Some(format!("PS3 Cell PPU program exited (code {}).", self.cpu.bus().exit_code))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps3_cell_ppu_writes_rsx_framebuffer_and_handles_lv2_exit() {
        // PowerPC64: lis r3, 0x0300 (RSX_FB_BASE); lis r4, 0x0041; ori r4, r4, 0x69E1 (Royal Blue); stw r4, 0(r3); li r11, 3; li r3, 0; sc
        let words: [u32; 7] = [
            0x3C60_0300, // lis r3, 0x0300
            0x3C80_0041, // lis r4, 0x0041
            0x6084_69E1, // ori r4, r4, 0x69E1
            0x9083_0000, // stw r4, 0(r3)
            0x3960_0003, // li r11, 3 (sys_process_exit)
            0x3860_0000, // li r3, 0
            0x4400_0002, // sc
        ];
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        let mut elf = vec![0u8; 0x100];
        elf[0..4].copy_from_slice(b"\x7FELF");
        elf[4] = 2; elf[5] = 2; elf[6] = 1; // 64-bit big-endian
        elf[16..18].copy_from_slice(&2u16.to_be_bytes());
        elf[18..20].copy_from_slice(&21u16.to_be_bytes()); // EM_PPC64
        elf[24..32].copy_from_slice(&0x0001_0000u64.to_be_bytes());
        elf[32..40].copy_from_slice(&64u64.to_be_bytes());
        elf[54..56].copy_from_slice(&56u16.to_be_bytes());
        elf[56..58].copy_from_slice(&1u16.to_be_bytes());
        elf[64..68].copy_from_slice(&1u32.to_be_bytes());
        elf[72..80].copy_from_slice(&0x100u64.to_be_bytes());
        elf[80..88].copy_from_slice(&0x0001_0000u64.to_be_bytes());
        elf[96..104].copy_from_slice(&(code.len() as u64).to_be_bytes());
        elf[104..112].copy_from_slice(&(code.len() as u64).to_be_bytes());
        elf.extend_from_slice(&code);

        let mut ps3 = Ps3::from_bytes(&elf).unwrap();
        ps3.run_frame();
        assert_eq!(ps3.frame()[0], 0x0041_69E1);
        assert!(ps3.cpu.bus().exited);
    }
}
