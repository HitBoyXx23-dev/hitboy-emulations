//! Clean-room PlayStation Portable (PSP / PSP Go) emulator core.

pub mod allegrex;
pub mod format;
pub mod hle;

pub use hle::{FPS, HEIGHT, WIDTH};

use allegrex::{Allegrex, Bus, StepEvent};
use format::{load_elf_into_ram, load_image, LoadedImage};
use hle::PspMem;

const STEPS_PER_FRAME: u32 = 150_000;

/// PSP controller button bitflags (`SceCtrlButtons`).
const PSP_CTRL_SELECT: u32 = 0x0000_0001;
const PSP_CTRL_START: u32 = 0x0000_0008;
const PSP_CTRL_UP: u32 = 0x0000_0010;
const PSP_CTRL_RIGHT: u32 = 0x0000_0020;
const PSP_CTRL_DOWN: u32 = 0x0000_0040;
const PSP_CTRL_LEFT: u32 = 0x0000_0080;
const PSP_CTRL_LTRIGGER: u32 = 0x0000_0100;
const PSP_CTRL_RTRIGGER: u32 = 0x0000_0200;
const PSP_CTRL_TRIANGLE: u32 = 0x0000_1000;
const PSP_CTRL_CIRCLE: u32 = 0x0000_2000;
const PSP_CTRL_CROSS: u32 = 0x0000_4000;
const PSP_CTRL_SQUARE: u32 = 0x0000_8000;

/// Complete PSP machine instance ready for frame-by-frame emulation.
pub struct Psp {
    pub cpu: Allegrex,
    pub mem: PspMem,
    pub title: String,
    pub disc_id: String,
    image: LoadedImage,
    frame: Vec<u32>,
}

impl Psp {
    /// Loads a `.pbp`, `.iso`, `.cso`, or decrypted MIPS `.elf` PSP program.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let image = load_image(data)?;
        let mut psp = Self {
            cpu: Allegrex::new(),
            mem: PspMem::new(image.files.clone()),
            title: image.title.clone(),
            disc_id: image.disc_id.clone(),
            image,
            frame: vec![0; WIDTH * HEIGHT],
        };
        psp.boot()?;
        Ok(psp)
    }

    fn boot(&mut self) -> Result<(), String> {
        self.cpu = Allegrex::new();
        self.mem = PspMem::new(self.image.files.clone());

        // Write ELF into guest RAM first so read_u32/read_cstr can inspect module info & stubs.
        let mut writes: Vec<(u32, Vec<u8>)> = Vec::new();
        let elf = &self.image.elf;
        if elf.len() < 52 || &elf[0..4] != b"\x7FELF" {
            return Err("Invalid PSP ELF image.".into());
        }
        let e_type = u16::from_le_bytes([elf[16], elf[17]]);
        let reloc_base: u32 = if e_type == 0xFFA0 { 0x0880_4000 } else { 0 };
        let phoff = u32::from_le_bytes(elf[28..32].try_into().unwrap()) as usize;
        let phentsize = u16::from_le_bytes([elf[42], elf[43]]) as usize;
        let phnum = u16::from_le_bytes([elf[44], elf[45]]) as usize;
        for i in 0..phnum {
            let off = phoff + i * phentsize;
            if off + 32 > elf.len() {
                break;
            }
            let p_type = u32::from_le_bytes(elf[off..off + 4].try_into().unwrap());
            let p_offset = u32::from_le_bytes(elf[off + 4..off + 8].try_into().unwrap()) as usize;
            let p_vaddr = u32::from_le_bytes(elf[off + 8..off + 12].try_into().unwrap()).wrapping_add(reloc_base);
            let p_filesz = u32::from_le_bytes(elf[off + 16..off + 20].try_into().unwrap()) as usize;
            if p_type == 1 && p_offset + p_filesz <= elf.len() {
                writes.push((p_vaddr, elf[p_offset..p_offset + p_filesz].to_vec()));
            }
        }
        for (addr, bytes) in &writes {
            self.mem.write_slice(*addr, bytes);
        }

        let (entry, gp, imports) = {
            let mem_ref = &self.mem;
            load_elf_into_ram(
                &self.image.elf,
                |_, _| {},
                |a| mem_ref.read32(a),
                |a| mem_ref.read_cstr(a),
            )?
        };
        self.mem.patch_imports(&imports);
        self.cpu.set_pc(entry);
        self.cpu.r[28] = gp;          // $gp
        self.cpu.r[29] = 0x09FF_F000; // $sp (top of 32 MB user RAM)
        self.cpu.r[31] = 0x0800_0000; // $ra -> exit stub
        // Put sceKernelExitGame stub at 0x0800_0000
        self.mem.write32(0x0800_0000, (0x0557_2A5F_u32 & 0xF_FFFF) << 6 | 0x0C);
        Ok(())
    }

    pub fn reset(&mut self) {
        let _ = self.boot();
        self.frame.fill(0);
    }

    /// Maps the shared frontend button bitmask into `SceCtrlButtons`.
    pub fn set_buttons(&mut self, buttons: u16) {
        let mut b = 0u32;
        if buttons & (1 << 0) != 0 { b |= PSP_CTRL_CROSS; }
        if buttons & (1 << 1) != 0 { b |= PSP_CTRL_CIRCLE; }
        if buttons & (1 << 2) != 0 { b |= PSP_CTRL_SELECT; }
        if buttons & (1 << 3) != 0 { b |= PSP_CTRL_START; }
        if buttons & (1 << 4) != 0 { b |= PSP_CTRL_UP; }
        if buttons & (1 << 5) != 0 { b |= PSP_CTRL_DOWN; }
        if buttons & (1 << 6) != 0 { b |= PSP_CTRL_LEFT; }
        if buttons & (1 << 7) != 0 { b |= PSP_CTRL_RIGHT; }
        if buttons & (1 << 8) != 0 { b |= PSP_CTRL_TRIANGLE; }
        if buttons & (1 << 9) != 0 { b |= PSP_CTRL_SQUARE; }
        if buttons & (1 << 10) != 0 { b |= PSP_CTRL_LTRIGGER; }
        if buttons & (1 << 11) != 0 { b |= PSP_CTRL_RTRIGGER; }
        self.mem.pad_buttons = b;
    }

    pub fn run_frame(&mut self) {
        if self.mem.exited {
            return;
        }
        self.mem.vblank_wait = false;
        for _ in 0..STEPS_PER_FRAME {
            match self.cpu.step(&mut self.mem) {
                StepEvent::Continue => {}
                StepEvent::Syscall(code) => {
                    self.mem.handle_syscall(&mut self.cpu, code);
                    if self.mem.exited || self.mem.vblank_wait {
                        break;
                    }
                }
                StepEvent::Break(_) => {
                    self.mem.exited = true;
                    break;
                }
            }
        }
        self.mem.vcount = self.mem.vcount.wrapping_add(1);
        self.mem.scanout(&mut self.frame);
    }

    pub fn frame(&self) -> &[u32] {
        &self.frame
    }

    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.mem.audio_out);
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.mem.sample_rate = rate.max(8_000);
    }

    pub fn status_message(&self) -> Option<String> {
        if self.mem.exited {
            Some(format!("PSP program exited (code {}).", self.mem.exit_code))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::DeflateEncoder, Compression};
    use std::io::Write;

    fn make_mips_elf(entry: u32, words: &[u32]) -> Vec<u8> {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut elf = vec![0u8; 0x100];
        elf[0..4].copy_from_slice(b"\x7FELF");
        elf[4] = 1; // 32-bit
        elf[5] = 1; // little endian
        elf[6] = 1;
        elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        elf[18..20].copy_from_slice(&8u16.to_le_bytes()); // EM_MIPS
        elf[24..28].copy_from_slice(&entry.to_le_bytes());
        elf[28..32].copy_from_slice(&52u32.to_le_bytes()); // phoff
        elf[42..44].copy_from_slice(&32u16.to_le_bytes()); // phentsize
        elf[44..46].copy_from_slice(&1u16.to_le_bytes());  // phnum
        // PT_LOAD at offset 52
        elf[52..56].copy_from_slice(&1u32.to_le_bytes());
        elf[56..60].copy_from_slice(&0x100u32.to_le_bytes());
        elf[60..64].copy_from_slice(&entry.to_le_bytes());
        elf[64..68].copy_from_slice(&entry.to_le_bytes());
        elf[68..72].copy_from_slice(&(code.len() as u32).to_le_bytes());
        elf[72..76].copy_from_slice(&(code.len() as u32).to_le_bytes());
        elf.extend_from_slice(&code);
        elf
    }

    fn make_pbp(title: &str, elf: &[u8]) -> Vec<u8> {
        // Build minimal PARAM.SFO with TITLE
        let mut sfo = vec![0u8; 20 + 16];
        sfo[0..4].copy_from_slice(b"\x00PSF");
        let key_table = 36u32;
        let data_table = 44u32;
        sfo[8..12].copy_from_slice(&key_table.to_le_bytes());
        sfo[12..16].copy_from_slice(&data_table.to_le_bytes());
        sfo[16..20].copy_from_slice(&1u32.to_le_bytes());
        // Entry 0: TITLE
        let val_len = (title.len() + 1) as u32;
        sfo[20..22].copy_from_slice(&0u16.to_le_bytes());
        sfo[22..24].copy_from_slice(&0x0204u16.to_le_bytes());
        sfo[24..28].copy_from_slice(&val_len.to_le_bytes());
        sfo[28..32].copy_from_slice(&val_len.to_le_bytes());
        sfo[32..36].copy_from_slice(&0u32.to_le_bytes());
        sfo.extend_from_slice(b"TITLE\0\0\0");
        sfo.extend_from_slice(title.as_bytes());
        sfo.push(0);

        let mut pbp = vec![0u8; 40];
        pbp[0..4].copy_from_slice(b"\x00PBP");
        let sfo_off = 40u32;
        let elf_off = sfo_off + sfo.len() as u32;
        let end_off = elf_off + elf.len() as u32;
        let offs = [sfo_off, elf_off, elf_off, elf_off, elf_off, elf_off, elf_off, end_off];
        for (i, o) in offs.iter().enumerate() {
            pbp[8 + i * 4..12 + i * 4].copy_from_slice(&o.to_le_bytes());
        }
        pbp.extend_from_slice(&sfo);
        pbp.extend_from_slice(elf);
        pbp
    }

    #[test]
    fn pbp_draws_pixels_to_vram_and_scans_out() {
        // Write ABGR8888 orange-red pixel (R=0xFF, G=0x80, B=0x20 -> 0xFF2080FF) to VRAM 0x04000000,
        // then loop cleanly via beq $0, $0, -1 (delay slot nop).
        let words = [
            0x3C08_0400, // lui $t0, 0x0400
            0x3C09_FF20, // lui $t1, 0xFF20
            0x3529_80FF, // ori $t1, $t1, 0x80FF
            0xAD09_0000, // sw $t1, 0($t0)
            0x1000_FFFF, // beq $zero, $zero, -1
            0x0000_0000, // nop
        ];
        let elf = make_mips_elf(0x0880_4000, &words);
        let pbp = make_pbp("PSP Test Homebrew", &elf);
        let mut psp = Psp::from_bytes(&pbp).unwrap();
        assert_eq!(psp.title, "PSP Test Homebrew");
        psp.set_buttons(1 << 0); // Cross
        assert_eq!(psp.mem.pad_buttons, PSP_CTRL_CROSS);
        psp.run_frame();
        assert_eq!(psp.frame()[0], 0x00FF_8020);
    }

    #[test]
    fn cso_decompression_round_trips() {
        let raw = vec![0x5Au8; 4096];
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&raw[..2048]).unwrap();
        let blk0 = enc.finish().unwrap();

        let mut cso = vec![0u8; 24 + 3 * 4];
        cso[0..4].copy_from_slice(b"CISO");
        cso[8..16].copy_from_slice(&(4096u64).to_le_bytes());
        cso[16..20].copy_from_slice(&(2048u32).to_le_bytes());
        let off0 = cso.len() as u32;
        let off1 = off0 + blk0.len() as u32;
        let off2 = off1 + 2048;
        cso[24..28].copy_from_slice(&off0.to_le_bytes());
        cso[28..32].copy_from_slice(&(off1 | 0x8000_0000).to_le_bytes());
        cso[32..36].copy_from_slice(&off2.to_le_bytes());
        cso.extend_from_slice(&blk0);
        cso.extend_from_slice(&raw[2048..]);

        let out = format::decompress_cso(&cso).unwrap();
        assert_eq!(out, raw);
    }
}
