//! HitBoy x86: an IBM PC/AT-compatible machine built on our Pentium-class core (`cpui686`).
//!
//! The BIOS is implemented natively in Rust (high-level emulation): each BIOS
//! interrupt vector points at a tiny ROM stub `0F FF nn CF` whose trap opcode
//! calls `Pc::bios(nn)`, then IRETs. Everything else (IVT, BIOS Data Area,
//! video memory) lives in guest RAM exactly where DOS-era software expects it.

pub mod cpu;
pub mod disk;
pub mod ide;
pub mod pci;
pub mod keys;
pub mod video;

use std::collections::{BTreeMap, VecDeque};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cpu::{flags, Cpu, CpuRef, Host};
use disk::{CdBoot, Disk, Kind};
pub use video::Screen;

/// Default guest RAM: enough for modern installers.
pub const DEFAULT_RAM_MB: u32 = 512;

const BIOS_SEG: u16 = 0xF000;
/// VESA OEM string and mode list in the BIOS ROM.
const VBE_OEM: u16 = 0xE000;
const VBE_LIST: u16 = 0xE040;
const STUB_BASE: u16 = 0x0100;
const IRET_STUB: u16 = 0xFF53;
const DISKETTE_TABLE: u16 = 0xEFC7;
/// Nominal guest speed used as the machine's clock: instructions per second of guest time.
const GUEST_IPS: f64 = 50e6;

const BIOS_SERVICES: &[u8] = &[0x08, 0x09, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A];

#[derive(Default, Clone)]
pub struct Config {
    /// Boot media: .iso (CD) or .img (floppy / hard disk image).
    pub boot: Option<std::path::PathBuf>,
    /// Hard disk image.
    pub disk: Option<std::path::PathBuf>,
    /// Guest memory in MB (0 = `DEFAULT_RAM_MB`).
    pub ram_mb: u32,
}

pub struct Pc {
    drives: BTreeMap<u8, Disk>,
    boot_drive: u8,
    disk_status: u8,

    key_queue: VecDeque<(u8, u8)>,
    ascii_for_scan: [u8; 128],
    kbd_buffer: VecDeque<u16>,
    kbc: pchw::Kbc,
    pci: pci::Pci,
    /// IDE channels: primary (0x1F0, IRQ 14) and secondary (0x170, IRQ 15).
    ide: [ide::Channel; 2],
    empty_polls: u32,

    dac: [[u8; 3]; 256],
    dac_write: (u8, u8),
    dac_read: (u8, u8),
    retrace: bool,
    port61: u8,
    pics: pchw::Pics,
    pit: pchw::Pit,
    cmos: pchw::Cmos,
    crtc_index: u8,

    /// Set when the guest is waiting for input; the run loop yields early.
    idle: bool,
    boot_failed: Option<String>,
    /// Print BIOS calls (diagnostics).
    pub trace: bool,
    /// Current VESA mode (None = VGA text/graphics).
    vbe_mode: Option<u16>,
    ram_size: usize,
}

impl Pc {
    fn drive(&mut self, n: u8) -> Option<&mut Disk> {
        self.drives.get_mut(&n)
    }

    fn count(&self, kind: Kind) -> u8 {
        self.drives.values().filter(|d| d.kind == kind).count() as u8
    }

    fn equipment_word(&self) -> u16 {
        let floppies = self.count(Kind::Floppy);
        let mut w = 0x0020; // 80x25 colour
        if floppies > 0 {
            w |= 1 | (((floppies - 1) as u16 & 3) << 6);
        }
        w
    }
}

// ----------------------------------------------------------------------- BIOS

fn ret_carry(cpu: &mut CpuRef, r: &mut cpu::Regs, ok: bool, status: u8) {
    r.set_ah(status);
    cpu.set_regs(r);
    cpu.set_return_flag(flags::CF, !ok);
}

impl Pc {
    fn int10(&mut self, cpu: &mut CpuRef, mut r: cpu::Regs) {
        if r.ah() == 0x4F {
            self.vbe(cpu, r);
            return;
        }
        if r.ah() == 0x00 {
            self.vbe_mode = None;
        }
        let mem = cpu.mem_mut();
        match r.ah() {
            0x00 => video::set_mode(mem, r.al(), r.al() & 0x80 == 0),
            0x01 => {
                mem[video::BDA_CURSOR_SHAPE] = r.cl();
                mem[video::BDA_CURSOR_SHAPE + 1] = r.ch();
            }
            0x02 => video::set_cursor(mem, r.dh() as usize, r.dl() as usize),
            0x03 => {
                let (row, col) = video::cursor(mem);
                r.set_dh(row as u8);
                r.set_dl(col as u8);
                r.cx = u16::from_le_bytes([mem[video::BDA_CURSOR_SHAPE], mem[video::BDA_CURSOR_SHAPE + 1]]);
            }
            0x05 => mem[0x462] = r.al(),
            0x06 | 0x07 => video::scroll(
                mem,
                r.ah() == 0x06,
                r.al() as usize,
                r.bh(),
                r.ch() as usize,
                r.cl() as usize,
                r.dh() as usize,
                r.dl() as usize,
            ),
            0x08 => {
                let (row, col) = video::cursor(mem);
                let (ch, at) = video::read_cell(mem, row, col);
                r.ax = (at as u16) << 8 | ch as u16;
            }
            0x09 | 0x0A => {
                let (row, col) = video::cursor(mem);
                let cols = video::cols(mem);
                for i in 0..r.cx as usize {
                    let p = row * cols + col + i;
                    video::write_cell(mem, p / cols, p % cols, r.al(), (r.ah() == 0x09).then_some(r.bl()));
                }
            }
            0x0C => {
                if mem[video::BDA_MODE] == 0x13 && r.cx < 320 && r.dx < 200 {
                    mem[0xA0000 + r.dx as usize * 320 + r.cx as usize] = r.al();
                }
            }
            0x0D => {
                if mem[video::BDA_MODE] == 0x13 && r.cx < 320 && r.dx < 200 {
                    r.set_al(mem[0xA0000 + r.dx as usize * 320 + r.cx as usize]);
                }
            }
            0x0E => video::teletype(mem, r.al(), r.bl()),
            0x0F => {
                r.set_al(mem[video::BDA_MODE]);
                r.set_ah(video::cols(mem) as u8);
                r.set_bh(mem[0x462]);
            }
            0x10 => match r.al() {
                0x10 => self.dac[r.bx as usize & 0xFF] = [r.dh() & 63, r.ch() & 63, r.cl() & 63],
                0x12 => {
                    for i in 0..r.cx as usize {
                        let o = r.dx.wrapping_add((i * 3) as u16);
                        let idx = (r.bx as usize + i) & 0xFF;
                        self.dac[idx] = [
                            cpu.read8(r.es, o) & 63,
                            cpu.read8(r.es, o.wrapping_add(1)) & 63,
                            cpu.read8(r.es, o.wrapping_add(2)) & 63,
                        ];
                    }
                }
                0x15 => {
                    let c = self.dac[r.bx as usize & 0xFF];
                    r.set_dh(c[0]);
                    r.set_ch(c[1]);
                    r.set_cl(c[2]);
                }
                0x17 => {
                    for i in 0..r.cx as usize {
                        let c = self.dac[(r.bx as usize + i) & 0xFF];
                        for (k, v) in c.iter().enumerate() {
                            cpu.write8(r.es, r.dx.wrapping_add((i * 3 + k) as u16), *v);
                        }
                    }
                }
                _ => {}
            },
            0x11 if r.al() == 0x30 => {
                r.cx = 16;
                r.set_dl(video::rows(mem) as u8 - 1);
                r.es = BIOS_SEG;
                r.bp = 0xFA6E;
            }
            0x12 if r.bl() == 0x10 => {
                r.bx = 0x0003;
                r.cx = 0x0009;
            }
            0x13 => {
                let (sr, sc) = video::cursor(mem);
                video::set_cursor(mem, r.dh() as usize, r.dl() as usize);
                let with_attr = r.al() & 2 != 0;
                let mut off = r.bp;
                for _ in 0..r.cx {
                    let ch = cpu.read8(r.es, off);
                    off = off.wrapping_add(1);
                    let attr = if with_attr {
                        let a = cpu.read8(r.es, off);
                        off = off.wrapping_add(1);
                        a
                    } else {
                        r.bl()
                    };
                    let mem = cpu.mem_mut();
                    if !matches!(ch, 7 | 8 | 10 | 13) {
                        let (row, col) = video::cursor(mem);
                        video::write_cell(mem, row, col, ch, Some(attr));
                    }
                    video::teletype(mem, ch, attr);
                }
                if r.al() & 1 == 0 {
                    video::set_cursor(cpu.mem_mut(), sr, sc);
                }
            }
            0x1A if r.al() == 0 => {
                r.set_al(0x1A);
                r.bx = 0x0008;
            }
            _ => {}
        }
        cpu.set_regs(&r);
    }

    fn int13(&mut self, cpu: &mut CpuRef, mut r: cpu::Regs) {
        let drive_no = r.dl();
        let fixed = self.count(Kind::Hdd);
        let floppies = self.count(Kind::Floppy);
        let mut buf = vec![0u8; 0];
        let (ok, status): (bool, u8) = match r.ah() {
            0x00 => (true, 0),
            0x01 => {
                let s = self.disk_status;
                r.set_al(s);
                (true, 0)
            }
            0x02 | 0x03 | 0x04 => {
                let count = r.al() as u32;
                let cyl = r.ch() as u32 | ((r.cl() as u32 & 0xC0) << 2);
                let sector = r.cl() as u32 & 0x3F;
                let head = r.dh() as u32;
                let ah = r.ah();
                match self.drive(drive_no) {
                    None => (false, 0x80),
                    Some(d) if d.kind == Kind::Cdrom => (false, 0x01),
                    Some(d) => match d.chs_to_lba(cyl, head, sector) {
                        None => (false, 0x04),
                        Some(lba) => {
                            buf.resize(count as usize * 512, 0);
                            match ah {
                                0x02 => {
                                    if d.read(lba, count, &mut buf) {
                                        copy_to_guest(cpu, r.es, r.bx, &buf);
                                        (true, 0)
                                    } else {
                                        (false, 0x04)
                                    }
                                }
                                0x03 => {
                                    copy_from_guest(cpu, r.es, r.bx, &mut buf);
                                    if !d.writable {
                                        (false, 0x03)
                                    } else if d.write(lba, count, &buf) {
                                        (true, 0)
                                    } else {
                                        (false, 0x04)
                                    }
                                }
                                _ => (true, 0),
                            }
                        }
                    },
                }
            }
            0x08 => match self.drive(drive_no) {
                Some(d) if d.kind != Kind::Cdrom => {
                    let maxc = d.cylinders.saturating_sub(1).min(1023);
                    r.set_ch(maxc as u8);
                    r.set_cl((d.spt as u8 & 0x3F) | (((maxc >> 8) as u8) << 6));
                    r.set_dh(d.heads as u8 - 1);
                    if d.kind == Kind::Floppy {
                        r.set_bl(match d.sectors * 512 {
                            368_640 => 1,
                            1_228_800 => 2,
                            737_280 => 3,
                            2_949_120 => 5,
                            _ => 4,
                        });
                        r.set_dl(floppies);
                        r.es = BIOS_SEG;
                        r.di = DISKETTE_TABLE;
                    } else {
                        r.set_dl(fixed);
                    }
                    r.set_al(0);
                    (true, 0)
                }
                _ => (false, if drive_no & 0x80 != 0 { 0x07 } else { 0x80 }),
            },
            0x15 => match self.drive(drive_no) {
                Some(d) if d.kind == Kind::Hdd => {
                    r.cx = (d.sectors >> 16) as u16;
                    r.dx = d.sectors as u16;
                    r.set_ah(3);
                    cpu.set_regs(&r);
                    cpu.set_return_flag(flags::CF, false);
                    return;
                }
                Some(_) => {
                    r.set_ah(2);
                    cpu.set_regs(&r);
                    cpu.set_return_flag(flags::CF, false);
                    return;
                }
                None => {
                    r.set_ah(0);
                    cpu.set_regs(&r);
                    cpu.set_return_flag(flags::CF, false);
                    return;
                }
            },
            0x16 => (true, 0),
            0x41 => {
                if r.bx == 0x55AA && self.drives.contains_key(&drive_no) {
                    r.bx = 0xAA55;
                    r.cx = 0x0001;
                    r.set_ah(0x30);
                    cpu.set_regs(&r);
                    cpu.set_return_flag(flags::CF, false);
                    return;
                }
                (false, 0x01)
            }
            0x42 | 0x43 => {
                let pkt = |o: u16| cpu.read16(r.ds, r.si.wrapping_add(o));
                let count = pkt(2) as u32;
                let (off, seg) = (pkt(4), pkt(6));
                let lba = pkt(8) as u64 | (pkt(10) as u64) << 16 | (pkt(12) as u64) << 32;
                let write = r.ah() == 0x43;
                match self.drive(drive_no) {
                    None => (false, 0x80),
                    Some(d) => {
                        buf.resize(count as usize * d.sector_size as usize, 0);
                        if write {
                            copy_from_guest(cpu, seg, off, &mut buf);
                            if d.write(lba, count, &buf) { (true, 0) } else { (false, 0x04) }
                        } else if d.read(lba, count, &mut buf) {
                            copy_to_guest(cpu, seg, off, &buf);
                            (true, 0)
                        } else {
                            (false, 0x04)
                        }
                    }
                }
            }
            0x48 => match self.drive(drive_no) {
                Some(d) => {
                    let (c, h, s, n, ss) = (d.cylinders, d.heads, d.spt, d.sectors, d.sector_size as u16);
                    let base = r.si;
                    cpu.write16(r.ds, base, 0x1A);
                    cpu.write16(r.ds, base.wrapping_add(2), 0);
                    for (i, v) in [c, h, s].iter().enumerate() {
                        cpu.write16(r.ds, base.wrapping_add(4 + i as u16 * 4), *v as u16);
                        cpu.write16(r.ds, base.wrapping_add(6 + i as u16 * 4), (*v >> 16) as u16);
                    }
                    for i in 0..4 {
                        cpu.write16(r.ds, base.wrapping_add(16 + i * 2), (n >> (16 * i)) as u16);
                    }
                    cpu.write16(r.ds, base.wrapping_add(24), ss);
                    (true, 0)
                }
                None => (false, 0x80),
            },
            0x4B => (true, 0),
            _ => (false, 0x01),
        };
        self.disk_status = status;
        cpu.mem_mut()[0x441] = status;
        ret_carry(cpu, &mut r, ok, status);
    }

    fn int16(&mut self, cpu: &mut CpuRef, mut r: cpu::Regs) {
        match r.ah() {
            0x00 | 0x10 => match self.kbd_buffer.pop_front() {
                Some(k) => {
                    self.empty_polls = 0;
                    r.ax = k;
                }
                None => {
                    // Block: re-run this INT after the next interrupt, and let the host idle.
                    cpu.retry_int();
                    self.idle = true;
                    cpu.stop();
                    return;
                }
            },
            0x01 | 0x11 => match self.kbd_buffer.front() {
                Some(&k) => {
                    self.empty_polls = 0;
                    r.ax = k;
                    cpu.set_regs(&r);
                    cpu.set_return_flag(flags::ZF, false);
                    return;
                }
                None => {
                    self.empty_polls += 1;
                    if self.empty_polls > 200 {
                        self.idle = true;
                        cpu.stop();
                    }
                    cpu.set_return_flag(flags::ZF, true);
                    return;
                }
            },
            0x02 => r.set_al(0),
            0x12 => r.ax = 0,
            0x05 => {
                if self.kbd_buffer.len() < 16 {
                    self.kbd_buffer.push_back(r.cx);
                    r.set_al(0);
                } else {
                    r.set_al(1);
                }
            }
            _ => {}
        }
        cpu.set_regs(&r);
    }

    fn int15(&mut self, cpu: &mut CpuRef, mut r: cpu::Regs) {
        let ext_kb = (self.ram_size / 1024 - 1024) as u32;
        if r.ax == 0xE820 {
            self.e820(cpu);
            return;
        }
        let ok = match r.ah() {
            0x88 => {
                r.ax = ext_kb.min(0xFFFF) as u16;
                true
            }
            0xE8 if r.al() == 0x01 => {
                // E801: KB between 1 and 16 MB, 64 KB blocks above 16 MB.
                let low = ext_kb.min(15 * 1024) as u16;
                let high = ((self.ram_size as u32).saturating_sub(16 << 20) >> 16) as u16;
                r.ax = low;
                r.cx = low;
                r.bx = high;
                r.dx = high;
                true
            }
            0x24 => {
                // A20 gate: always enabled.
                match r.al() {
                    0x02 => r.set_al(1),
                    0x03 => r.bx = 3,
                    _ => {}
                }
                r.set_ah(0);
                true
            }
            0x86 => {
                self.idle = true;
                true
            }
            0x4F => {
                cpu.set_return_flag(flags::CF, true); // keep the scan code
                return;
            }
            _ => {
                r.set_ah(0x86);
                false
            }
        };
        cpu.set_regs(&r);
        cpu.set_return_flag(flags::CF, !ok);
    }

    /// VESA BIOS Extensions (INT 10h AH=4Fh): controller and mode information, mode set,
    /// linear framebuffer at `cpu::VRAM_BASE`.
    fn vbe(&mut self, cpu: &mut CpuRef, mut r: cpu::Regs) {
        let ok = match r.al() {
            0x00 => {
                let mut info = [0u8; 512];
                info[0..4].copy_from_slice(b"VESA");
                info[4..6].copy_from_slice(&0x0300u16.to_le_bytes());
                // OEM string and mode list live in the BIOS ROM (see `install_bios`).
                info[6..10].copy_from_slice(&((BIOS_SEG as u32) << 16 | VBE_OEM as u32).to_le_bytes());
                info[0x0E..0x12].copy_from_slice(&((BIOS_SEG as u32) << 16 | VBE_LIST as u32).to_le_bytes());
                info[0x12..0x14].copy_from_slice(&((cpu::VRAM_SIZE >> 16) as u16).to_le_bytes());
                for at in [0x16usize, 0x1A, 0x1E] {
                    info[at..at + 4].copy_from_slice(&((BIOS_SEG as u32) << 16 | VBE_OEM as u32).to_le_bytes());
                }
                copy_to_guest(cpu, r.es, r.di, &info);
                true
            }
            0x01 => match video::vbe_mode(r.cx) {
                Some((w, h, bpp)) => {
                    let mut m = [0u8; 256];
                    let pitch = w as usize * video::vbe_bytes(bpp);
                    let wr16 = |m: &mut [u8; 256], at: usize, v: u16| m[at..at + 2].copy_from_slice(&v.to_le_bytes());
                    wr16(&mut m, 0x00, 0x9B); // supported, colour, graphics, linear framebuffer
                    m[0x02] = 0x07;
                    wr16(&mut m, 0x04, 64);
                    wr16(&mut m, 0x06, 64);
                    wr16(&mut m, 0x08, 0xA000);
                    wr16(&mut m, 0x10, pitch as u16);
                    wr16(&mut m, 0x12, w);
                    wr16(&mut m, 0x14, h);
                    m[0x16] = 8;
                    m[0x17] = 16;
                    m[0x18] = 1;
                    m[0x19] = bpp;
                    m[0x1A] = 1;
                    m[0x1B] = if bpp == 8 { 4 } else { 6 }; // packed pixel / direct colour
                    m[0x1E] = 1;
                    let layout: [u8; 8] = match bpp {
                        15 => [5, 10, 5, 5, 5, 0, 1, 15],
                        16 => [5, 11, 6, 5, 5, 0, 0, 0],
                        32 => [8, 16, 8, 8, 8, 0, 8, 24],
                        _ => [0; 8],
                    };
                    m[0x1F..0x27].copy_from_slice(&layout);
                    m[0x28..0x2C].copy_from_slice(&cpu::VRAM_BASE.to_le_bytes());
                    wr16(&mut m, 0x32, pitch as u16);
                    m[0x34..0x3C].copy_from_slice(&layout); // linear-mode masks (VBE 3)
                    copy_to_guest(cpu, r.es, r.di, &m);
                    true
                }
                None => false,
            },
            0x02 => {
                let mode = r.bx & 0x1FF;
                if mode < 0x100 {
                    self.vbe_mode = None;
                    video::set_mode(cpu.mem_mut(), mode as u8, true);
                    true
                } else if let Some((w, h, bpp)) = video::vbe_mode(mode) {
                    if r.bx & 0x8000 == 0 {
                        let n = w as usize * h as usize * video::vbe_bytes(bpp);
                        cpu.vram[..n].fill(0);
                    }
                    self.vbe_mode = Some(mode);
                    true
                } else {
                    false
                }
            }
            0x03 => {
                r.bx = self.vbe_mode.unwrap_or(cpu.mem()[video::BDA_MODE] as u16) | 0x4000;
                true
            }
            0x05 | 0x07 => true, // window / display start: the linear framebuffer ignores them
            0x06 => {
                // Scan line length: report the current pitch.
                if let Some((w, _, bpp)) = self.vbe_mode.and_then(video::vbe_mode) {
                    let pitch = w as usize * video::vbe_bytes(bpp);
                    r.bx = pitch as u16;
                    r.cx = w;
                    r.dx = (cpu::VRAM_SIZE / pitch) as u16;
                }
                true
            }
            0x08 => {
                r.set_bh(8); // 8-bit DAC width is not supported: stay 6-bit
                r.set_bh(6);
                true
            }
            _ => false,
        };
        r.ax = if ok { 0x004F } else { 0x014F };
        cpu.set_regs(&r);
    }

    /// INT 15h AX=E820: one entry of the physical memory map per call (EBX = continuation).
    fn e820(&mut self, cpu: &mut CpuRef) {
        let map: [(u64, u64, u32); 4] = [
            (0, 0x9_FC00, 1),
            (0x9_FC00, 0x400, 2),
            (0xE_0000, 0x2_0000, 2),
            (0x10_0000, self.ram_size as u64 - 0x10_0000, 1),
        ];
        let c = &mut cpu.core;
        let i = c.r[cpui686::EBX] as usize;
        if c.r[cpui686::EDX] != 0x534D_4150 || i >= map.len() {
            cpu.set_return_flag(flags::CF, true);
            return;
        }
        let (base, len, kind) = map[i];
        let mut e = [0u8; 20];
        e[0..8].copy_from_slice(&base.to_le_bytes());
        e[8..16].copy_from_slice(&len.to_le_bytes());
        e[16..20].copy_from_slice(&kind.to_le_bytes());
        let (es, di) = (c.sel[cpui686::ES], c.r[cpui686::EDI] as u16);
        c.r[cpui686::EAX] = 0x534D_4150;
        c.r[cpui686::ECX] = 20;
        c.r[cpui686::EBX] = if i + 1 < map.len() { i as u64 + 1 } else { 0 };
        copy_to_guest(cpu, es, di, &e);
        cpu.set_return_flag(flags::CF, false);
    }

    fn int1a(&mut self, cpu: &mut CpuRef, mut r: cpu::Regs) {
        let bcd = |v: u64| ((v / 10) << 4 | (v % 10)) as u8;
        match r.ah() {
            0x00 => {
                let ticks = u32::from_le_bytes(cpu.mem()[0x46C..0x470].try_into().unwrap());
                r.cx = (ticks >> 16) as u16;
                r.dx = ticks as u16;
                r.set_al(cpu.mem()[0x470]);
                cpu.mem_mut()[0x470] = 0;
            }
            0x01 => {
                let t = (r.cx as u32) << 16 | r.dx as u32;
                cpu.mem_mut()[0x46C..0x470].copy_from_slice(&t.to_le_bytes());
            }
            0x02 => {
                let s = now_secs() % 86_400;
                r.set_ch(bcd(s / 3600));
                r.set_cl(bcd(s / 60 % 60));
                r.set_dh(bcd(s % 60));
                r.set_dl(0);
            }
            0x04 => {
                let (y, m, d) = civil_from_days((now_secs() / 86_400) as i64);
                r.set_ch(bcd(y as u64 / 100));
                r.set_cl(bcd(y as u64 % 100));
                r.set_dh(bcd(m as u64));
                r.set_dl(bcd(d as u64));
            }
            _ => {}
        }
        cpu.set_regs(&r);
        cpu.set_return_flag(flags::CF, false);
    }
}

impl Host for Pc {
    fn io_in(&mut self, port: u16, size: u8) -> u32 {
        match port {
            0x1F0 | 0x170 => return self.ide[(port == 0x170) as usize].read_data(size, &mut self.drives),
            0xCF8 if size == 4 => return self.pci.addr,
            0xCFC..=0xCFF => return self.pci.read(port - 0xCFC, size),
            _ => {}
        }
        (0..size as u16).fold(0, |v, k| v | (self.io_in8(port.wrapping_add(k)) as u32) << (k * 8))
    }

    fn io_out(&mut self, port: u16, size: u8, value: u32) {
        match port {
            0x1F0 | 0x170 => return self.ide[(port == 0x170) as usize].write_data(size, value, &mut self.drives),
            0xCF8 if size == 4 => {
                self.pci.addr = value;
                return;
            }
            0xCFC..=0xCFF => return self.pci.write(port - 0xCFC, size, value),
            _ => {}
        }
        for k in 0..size as u16 {
            self.io_out8(port.wrapping_add(k), (value >> (k * 8)) as u8);
        }
    }

    fn bios(&mut self, cpu: &mut CpuRef, n: u8) {
        self.bios_call(cpu, n)
    }
}

impl Pc {
    fn io_in8(&mut self, port: u16) -> u8 {
        (match port {
            0x20 | 0x21 => self.pics.m.read(port == 0x20),
            0xA0 | 0xA1 => self.pics.s.read(port == 0xA0),
            0x40..=0x42 => self.pit.read_port(port),
            0x70 | 0x71 => self.cmos.read(port),
            0x92 => 0x02, // fast A20 gate: always on
            0x60 | 0x64 => self.kbc.read(port),
            0x1F1..=0x1F7 => self.ide[0].read(port - 0x1F0, &mut self.drives),
            0x3F6 => self.ide[0].read(8, &mut self.drives),
            0x171..=0x177 => {
                let v = self.ide[1].read(port - 0x170, &mut self.drives);
                if std::env::var_os("IDETRACE").is_some() {
                    eprintln!("r {port:X} = {v:02X}");
                }
                v
            }
            0x376 => self.ide[1].read(8, &mut self.drives),
            0x61 => {
                // Bit 4: refresh toggle (delay loops); bit 5: timer channel 2 output.
                self.port61 ^= 0x10;
                (self.port61 & 0x1F) | (self.pit.out2() as u8) << 5
            }
            0x3C7 => 0,
            0x3C8 => self.dac_write.0,
            0x3C9 => {
                let (i, sub) = self.dac_read;
                let v = self.dac[i as usize][sub as usize];
                self.dac_read = if sub == 2 { (i.wrapping_add(1), 0) } else { (i, sub + 1) };
                v
            }
            0x3D4 => self.crtc_index,
            0x3BA | 0x3DA => {
                self.retrace = !self.retrace;
                if self.retrace { 0x09 } else { 0x00 }
            }
            _ => 0xFF,
        }) as u8
    }

    fn io_out8(&mut self, port: u16, v: u8) {
        match port {
            0x20 | 0x21 => self.pics.m.write(port == 0x20, v),
            0xA0 | 0xA1 => self.pics.s.write(port == 0xA0, v),
            0x40..=0x43 => self.pit.write(port, v),
            0x70 | 0x71 => self.cmos.write(port, v),
            0x60 | 0x64 => self.kbc.write(port, v),
            0x1F1..=0x1F7 => self.ide[0].write(port - 0x1F0, v, &mut self.drives),
            0x3F6 => self.ide[0].write(8, v, &mut self.drives),
            0x171..=0x177 => {
                if std::env::var_os("IDETRACE").is_some() {
                    eprintln!("w {port:X} <- {v:02X}");
                }
                self.ide[1].write(port - 0x170, v, &mut self.drives)
            }
            0x376 => self.ide[1].write(8, v, &mut self.drives),
            0x61 => {
                self.port61 = (self.port61 & 0x10) | (v & 0x0F);
                self.pit.set_gate2(v & 1 != 0);
            }
            0x3C7 => self.dac_read = (v, 0),
            0x3C8 => self.dac_write = (v, 0),
            0x3C9 => {
                let (i, sub) = self.dac_write;
                self.dac[i as usize][sub as usize] = v & 63;
                self.dac_write = if sub == 2 { (i.wrapping_add(1), 0) } else { (i, sub + 1) };
            }
            0x3D4 => self.crtc_index = v,
            _ => {}
        }
    }

    fn bios_call(&mut self, cpu: &mut CpuRef, n: u8) {
        let mut r = cpu.regs();
        if self.trace && n != 0x08 {
            eprintln!("int {n:02X} ax={:04X} bx={:04X} cx={:04X} dx={:04X}", r.ax, r.bx, r.cx, r.dx);
        }
        match n {
            0x08 => {
                self.pics.m.write(true, 0x20);
                let mem = cpu.mem_mut();
                let mut t = u32::from_le_bytes(mem[0x46C..0x470].try_into().unwrap()) + 1;
                if t >= 0x1800B0 {
                    t = 0;
                    mem[0x470] = 1;
                }
                mem[0x46C..0x470].copy_from_slice(&t.to_le_bytes());
            }
            0x09 => {
                self.pics.m.write(true, 0x20);
                let scan = self.kbc.read(0x60);
                if scan < 0x80 && self.kbd_buffer.len() < 16 {
                    self.kbd_buffer.push_back((scan as u16) << 8 | self.ascii_for_scan[scan as usize] as u16);
                }
            }
            0x10 => self.int10(cpu, r),
            0x11 => {
                r.ax = self.equipment_word();
                cpu.set_regs(&r);
            }
            0x12 => {
                r.ax = 640;
                cpu.set_regs(&r);
            }
            0x13 => self.int13(cpu, r),
            0x14 => {
                r.set_ah(0x80);
                cpu.set_regs(&r);
            }
            0x15 => self.int15(cpu, r),
            0x16 => self.int16(cpu, r),
            0x17 => {
                r.set_ah(0x90);
                cpu.set_regs(&r);
            }
            0x18 => {
                self.boot_failed = Some("No bootable operating system found.".into());
                self.idle = true;
                cpu.stop();
            }
            0x19 => {
                if let Err(e) = self.load_boot_sector(cpu) {
                    self.boot_failed = Some(e);
                    cpu.stop();
                }
            }
            0x1A => self.int1a(cpu, r),
            _ => {}
        }
    }
}

fn copy_to_guest(cpu: &mut CpuRef, seg: u16, off: u16, data: &[u8]) {
    let base = CpuRef::lin(seg, 0);
    let mem = cpu.mem_mut();
    for (i, b) in data.iter().enumerate() {
        mem[(base + ((off as usize + i) & 0xFFFF)) & 0xF_FFFF] = *b;
    }
}

fn copy_from_guest(cpu: &CpuRef, seg: u16, off: u16, data: &mut [u8]) {
    let base = CpuRef::lin(seg, 0);
    let mem = cpu.mem();
    for (i, b) in data.iter_mut().enumerate() {
        *b = mem[(base + ((off as usize + i) & 0xFFFF)) & 0xF_FFFF];
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

// ----------------------------------------------------------------------- boot

impl Pc {
    /// Loads the boot sector of `boot_drive` and points the CPU at it. Used at
    /// power-on and by INT 19h (in which case it rewrites the IRET frame).
    fn load_boot_sector(&mut self, cpu: &mut CpuRef) -> Result<(), String> {
        let drive_no = self.boot_drive;
        let d = self.drives.get_mut(&drive_no).ok_or("No boot drive")?;
        let (seg, ip);
        if d.kind == Kind::Cdrom {
            return Err("CD boot is set up at power-on only".into());
        } else {
            let mut sector = vec![0u8; 512];
            if !d.read(0, 1, &mut sector) {
                return Err("Couldn't read the boot sector.".into());
            }
            if d.kind != Kind::Floppy && (sector[510] != 0x55 || sector[511] != 0xAA) {
                return Err("The disk has no boot signature (not bootable).".into());
            }
            copy_to_guest(cpu, 0, 0x7C00, &sector);
            seg = 0u16;
            ip = 0x7C00u16;
        }
        self.jump_to_boot(cpu, seg, ip);
        Ok(())
    }

    fn jump_to_boot(&mut self, cpu: &mut CpuRef, seg: u16, ip: u16) {
        // Build an IRET frame so this works both at power-on and from INT 19h.
        let mut r = cpu.regs();
        r.ss = 0;
        r.sp = 0x7BFA;
        cpu.write16(0, 0x7BFA, ip);
        cpu.write16(0, 0x7BFC, seg);
        cpu.write16(0, 0x7BFE, 0x0202);
        r.ax = 0;
        r.bx = 0;
        r.cx = 0;
        r.dx = self.boot_drive as u16;
        r.si = 0;
        r.di = 0;
        r.bp = 0;
        r.ds = 0;
        r.es = 0;
        cpu.set_regs(&r);
    }
}

// ----------------------------------------------------------------------- machine

pub struct Machine {
    cpu: Cpu<Pc>,
    /// Fractional timer clocks carried between slices.
    pit_frac: f64,
    pub instructions: u64,
}

pub struct RunStats {
    pub instructions: u64,
    pub idle: bool,
}

impl Machine {
    pub fn new(cfg: &Config) -> Result<Machine, String> {
        let mut drives = BTreeMap::new();
        let mut boot_drive = None;
        let mut cd_boot = None;

        if let Some(path) = &cfg.boot {
            let is_iso = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso"));
            if is_iso {
                match disk::open_iso(path)? {
                    CdBoot::NoEmulation { cd, segment, lba, count } => {
                        drives.insert(0xE0, cd);
                        boot_drive = Some(0xE0);
                        cd_boot = Some((segment, lba, count));
                    }
                    CdBoot::Emulated { disk, cd } => {
                        let n = if disk.kind == Kind::Floppy { 0x00 } else { 0x80 };
                        drives.insert(n, disk);
                        drives.insert(0xE0, cd);
                        boot_drive = Some(n);
                    }
                }
            } else {
                let d = Disk::open_image(path)?;
                let n = if d.kind == Kind::Floppy { 0x00 } else { 0x80 };
                drives.insert(n, d);
                boot_drive = Some(n);
            }
        }
        if let Some(path) = &cfg.disk {
            let mut d = Disk::open_image(path)?;
            d.kind = Kind::Hdd;
            let n = if drives.contains_key(&0x80) { 0x81 } else { 0x80 };
            drives.insert(n, d);
            boot_drive.get_or_insert(n);
        }
        let boot_drive = boot_drive.ok_or("Attach an ISO, IMG or disk image to boot from.")?;

        let pc = Pc {
            drives,
            boot_drive,
            disk_status: 0,
            key_queue: VecDeque::new(),
            ascii_for_scan: [0; 128],
            kbd_buffer: VecDeque::new(),
            kbc: pchw::Kbc::default(),
            pci: pci::Pci::default(),
            ide: [ide::Channel::new(None, None), ide::Channel::new(None, None)],
            empty_polls: 0,
            dac: video::default_dac(),
            dac_write: (0, 0),
            dac_read: (0, 0),
            retrace: false,
            port61: 0,
            pics: pchw::Pics::default(),
            pit: pchw::Pit::default(),
            cmos: pchw::Cmos::default(),
            crtc_index: 0,
            idle: false,
            boot_failed: None,
            trace: false,
            vbe_mode: None,
            ram_size: (if cfg.ram_mb == 0 { DEFAULT_RAM_MB } else { cfg.ram_mb.clamp(4, 2048) } as usize) << 20,
        };
        let ram_size = pc.ram_size;
        let mut pc = pc;
        let has = |n: u8| pc.drives.get(&n).is_some_and(|d| d.kind != Kind::Floppy);
        let (m, sl, cd) = (has(0x80).then_some(0x80), has(0x81).then_some(0x81), pc.drives.contains_key(&0xE0).then_some(0xE0u8));
        pc.ide = [ide::Channel::new(m, sl), ide::Channel::new(cd, None)];
        let mut m = Machine { cpu: Cpu::new(pc, ram_size), pit_frac: 0.0, instructions: 0 };
        m.install_bios();

        let (pc, cpu) = m.cpu.split();
        match cd_boot {
            Some((segment, lba, count)) => {
                // No-emulation CD boot: copy the boot image straight into memory.
                let cd = pc.drives.get_mut(&0xE0).unwrap();
                let cd_sectors = (count as u32 * 512).div_ceil(2048);
                let mut buf = vec![0u8; cd_sectors as usize * 2048];
                if !cd.read(lba as u64, cd_sectors, &mut buf) {
                    return Err("Couldn't read the CD boot image.".into());
                }
                buf.truncate(count as usize * 512);
                copy_to_guest(cpu, segment, 0, &buf);
                pc.jump_to_boot(cpu, segment, 0);
            }
            None => pc.load_boot_sector(cpu)?,
        }
        // Enter via IRET frame: pop it now so execution starts at the boot code.
        let mut r = cpu.regs();
        r.ip = cpu.read16(r.ss, r.sp);
        r.cs = cpu.read16(r.ss, r.sp.wrapping_add(2));
        r.flags = cpu.read16(r.ss, r.sp.wrapping_add(4));
        r.sp = r.sp.wrapping_add(6);
        cpu.set_regs(&r);
        Ok(m)
    }

    fn install_bios(&mut self) {
        let (pc, cpu) = self.cpu.split();
        for (cmd, v) in [(true, 0x11), (false, 0x08), (false, 0x04), (false, 0x01), (false, 0xB8)] {
            pc.pics.m.write(cmd, v);
        }
        for (cmd, v) in [(true, 0x11), (false, 0x70), (false, 0x02), (false, 0x01), (false, 0xFF)] {
            pc.pics.s.write(cmd, v);
        }
        for v in [0x36, 0, 0] {
            pc.pit.write(if v == 0x36 { 0x43 } else { 0x40 }, v);
        }
        let ext_kb = (pc.ram_size / 1024 - 1024) as u32;
        let above16 = ((pc.ram_size as u32).saturating_sub(16 << 20) >> 16) as u16;
        let (floppies, disks) = (pc.count(Kind::Floppy), pc.count(Kind::Hdd));
        let c = &mut pc.cmos.ram;
        c[0x10] = if floppies > 0 { 0x40 } else { 0 }; // 1.44 MB drive A
        c[0x12] = if disks > 0 { 0xF0 } else { 0 };
        c[0x14] = 0x07;
        c[0x15..0x17].copy_from_slice(&640u16.to_le_bytes());
        for at in [0x17, 0x30] {
            c[at..at + 2].copy_from_slice(&(ext_kb.min(0xFFFF) as u16).to_le_bytes());
        }
        c[0x34..0x36].copy_from_slice(&above16.to_le_bytes());
        let equipment = pc.equipment_word();
        let hdds = pc.count(Kind::Hdd);
        let mem = cpu.mem_mut();
        let rom = |off: u16| 0xF0000 + off as usize;

        mem[rom(IRET_STUB)] = 0xCF;
        for v in 0..256usize {
            mem[v * 4..v * 4 + 2].copy_from_slice(&IRET_STUB.to_le_bytes());
            mem[v * 4 + 2..v * 4 + 4].copy_from_slice(&BIOS_SEG.to_le_bytes());
        }
        for &n in BIOS_SERVICES {
            let off = STUB_BASE + n as u16 * 8;
            let at = rom(off);
            let stub: &[u8] = if n == 0x08 { &[0x0F, 0xFF, 0x08, 0xCD, 0x1C, 0xCF] } else { &[0x0F, 0xFF, n, 0xCF] };
            mem[at..at + stub.len()].copy_from_slice(stub);
            let v = n as usize * 4;
            mem[v..v + 2].copy_from_slice(&off.to_le_bytes());
        }
        // INT 1Eh -> diskette parameter table.
        let t = rom(DISKETTE_TABLE);
        mem[t..t + 11].copy_from_slice(&[0xDF, 0x02, 0x25, 0x02, 0x12, 0x1B, 0xFF, 0x6C, 0xF6, 0x0F, 0x08]);
        mem[0x1E * 4..0x1E * 4 + 2].copy_from_slice(&DISKETTE_TABLE.to_le_bytes());
        // BIOS date and model byte (PC/AT).
        mem[rom(0xFFF5)..rom(0xFFF5) + 8].copy_from_slice(b"09/27/26");
        mem[rom(0xFFFE)] = 0xFC;

        // BIOS Data Area.
        mem[0x410..0x412].copy_from_slice(&equipment.to_le_bytes());
        mem[0x413..0x415].copy_from_slice(&640u16.to_le_bytes());
        mem[0x463..0x465].copy_from_slice(&0x3D4u16.to_le_bytes());
        mem[0x475] = hdds;
        mem[0x485] = 16;
        video::set_mode(mem, 3, true);
        // VESA OEM string and mode list.
        let oem = b"HitBoy VESA display\0";
        mem[rom(VBE_OEM)..rom(VBE_OEM) + oem.len()].copy_from_slice(oem);
        let mut at = rom(VBE_LIST);
        for m in video::VBE_MODES.iter().map(|m| m.0).chain([0xFFFF]) {
            mem[at..at + 2].copy_from_slice(&m.to_le_bytes());
            at += 2;
        }
    }

    /// Runs the machine for one host frame (`dt` seconds of guest time), spending at most
    /// `budget` of wall-clock time. Returns early when the guest is idle.
    ///
    /// Guest time advances with the instructions run (at `GUEST_IPS`); while the guest waits
    /// (HLT or polling the keyboard) it skips ahead to the next timer tick.
    pub fn run_frame(&mut self, dt: f64, budget: std::time::Duration) -> RunStats {
        let start = Instant::now();
        let mut left = dt;
        let mut executed = 0u64;
        let mut waited = false;
        while left > 0.0 {
            self.cpu.host_mut().idle = false;
            self.feed_keyboard();
            self.deliver_interrupts();
            let chunk = ((left * GUEST_IPS) as u32).clamp(1, 4000);
            let n = self.cpu.run(chunk);
            executed += n as u64;
            let mut t = n as f64 / GUEST_IPS;
            let pc = self.cpu.host();
            if pc.boot_failed.is_some() {
                break;
            }
            if pc.idle || self.cpu.halted() {
                // Nothing to do until the next timer tick (or a key press).
                waited = true;
                let next = pc.pit.count0() as f64 / pchw::PC_PIT_HZ as f64;
                t = t.max(next.min(left));
                let skipped = ((t - n as f64 / GUEST_IPS) * GUEST_IPS) as u64;
                self.cpu.state().core.tsc += skipped;
            }
            self.advance(t);
            left -= t;
            if start.elapsed() >= budget {
                break;
            }
        }
        self.instructions += executed;
        RunStats { instructions: executed, idle: waited }
    }

    /// Advances the timer by `secs` of guest time.
    fn advance(&mut self, secs: f64) {
        self.pit_frac += secs * pchw::PC_PIT_HZ as f64;
        let ticks = self.pit_frac.floor();
        self.pit_frac -= ticks;
        let pc = self.cpu.host_mut();
        if pc.pit.tick(ticks as u64) > 0 {
            pc.pics.raise(0);
        }
    }

    /// Hands the next interrupt to the CPU, which takes it as soon as its IF flag allows.
    fn deliver_interrupts(&mut self) {
        if self.cpu.has_pending() {
            return;
        }
        if let Some(v) = self.cpu.host_mut().pics.next_vector() {
            self.cpu.irq(v);
        }
    }

    /// Passes queued key events to the keyboard controller, one at a time, and forwards
    /// its interrupts (IRQ 1 keyboard, IRQ 12 mouse) to the interrupt controllers.
    fn feed_keyboard(&mut self) {
        let pc = self.cpu.host_mut();
        if pc.kbc.idle() {
            if let Some((scan, ascii)) = pc.key_queue.pop_front() {
                if scan < 0x80 {
                    pc.ascii_for_scan[scan as usize] = ascii;
                }
                pc.empty_polls = 0;
                pc.kbc.key(scan);
            }
        }
        for (i, irq) in [(0, 14), (1, 15)] {
            if std::mem::take(&mut pc.ide[i].irq) {
                pc.pics.raise(irq);
            }
        }
        let (k, m) = pc.kbc.take_irqs();
        if k {
            pc.pics.raise(1);
        }
        if m {
            pc.pics.raise(12);
        }
    }

    /// Relative mouse movement and buttons (bit 0 left, 1 right, 2 middle).
    pub fn mouse(&mut self, dx: i32, dy: i32, buttons: u8) {
        self.cpu.host_mut().kbc.mouse(dx, dy, buttons);
    }

    /// Key pressed: scan code (set 1) + the ASCII it produces (0 for special keys).
    pub fn key_down(&mut self, scan: u8, ascii: u8) {
        self.cpu.host_mut().key_queue.push_back((scan & 0x7F, ascii));
    }

    pub fn key_up(&mut self, scan: u8) {
        self.cpu.host_mut().key_queue.push_back((scan | 0x80, 0));
    }

    /// Ctrl+Alt+Del: warm reboot from the boot drive.
    pub fn reboot(&mut self) {
        let (pc, cpu) = self.cpu.split();
        pc.kbd_buffer.clear();
        pc.key_queue.clear();
        if pc.drives.get(&pc.boot_drive).is_some_and(|d| d.kind != Kind::Cdrom) {
            if let Err(e) = pc.load_boot_sector(cpu) {
                pc.boot_failed = Some(e);
                return;
            }
            let mut r = cpu.regs();
            r.ip = cpu.read16(r.ss, r.sp);
            r.cs = cpu.read16(r.ss, r.sp.wrapping_add(2));
            r.flags = cpu.read16(r.ss, r.sp.wrapping_add(4));
            r.sp = r.sp.wrapping_add(6);
            cpu.set_regs(&r);
            video::set_mode(cpu.mem_mut(), 3, true);
        }
    }

    pub fn screen(&mut self) -> Screen {
        let (pc, cpu) = self.cpu.split();
        match pc.vbe_mode {
            Some(m) => video::render_vbe(&cpu.vram, m, &pc.dac),
            None => video::render(cpu.mem(), &pc.dac),
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.cpu.host().boot_failed.as_deref()
    }

    pub fn set_trace(&mut self, on: bool) {
        self.cpu.host_mut().trace = on;
    }

    /// Guest RAM (diagnostics).
    pub fn ram(&mut self) -> &[u8] {
        self.cpu.state().mem()
    }

    /// CPU state for diagnostics.
    pub fn debug(&mut self) -> String {
        let pending = self.cpu.has_pending();
        let p = &self.cpu.host().pics;
        let pic = format!(" pic m irr={:02X} isr={:02X} imr={:02X} base={:02X} s irr={:02X} isr={:02X} imr={:02X} pending={pending} pit={}", p.m.irr, p.m.isr, p.m.imr, p.m.base, p.s.irr, p.s.isr, p.s.imr, self.cpu.host().pit.count0());
        let c = &self.cpu.state().core;
        pic + &format!(
            "cs:rip={:04X}:{:X} mode={} cpl={} eflags={:08X} cr0={:08X} cr3={:X} eax={:X} ebx={:X} ecx={:X} edx={:X} esp={:X} halted={}",
            c.sel[cpui686::CS],
            c.rip,
            if c.code64 { "long64" } else if c.protected() { if c.cs32 { "pm32" } else { "pm16" } } else { "real" },
            c.cpl,
            c.eflags,
            c.cr0,
            c.cr3,
            c.r[0],
            c.r[3],
            c.r[1],
            c.r[2],
            c.r[4],
            c.halted
        ) + &c.shutdown.as_ref().map(|s| format!("\n{s}")).unwrap_or_default()
    }

    pub fn boot_drive(&self) -> u8 {
        self.cpu.host().boot_drive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Boot sector: prints "HI" with INT 10h/0Eh, reads sector 2 with INT 13h/02h
    /// to 0000:8000 and jumps there; sector 2 writes "OK" straight into video RAM.
    fn test_floppy() -> Vec<u8> {
        let mut img = vec![0u8; 1_474_560];
        let boot: &[u8] = &[
            0xFA, 0x31, 0xC0, 0x8E, 0xD8, 0x8E, 0xD0, 0xBC, 0x00, 0x7C, 0xFB, 0xBE, 0x30, 0x7C, // setup, si=msg
            0xAC, 0x08, 0xC0, 0x74, 0x06, 0xB4, 0x0E, 0xCD, 0x10, 0xEB, 0xF5, // print loop
            0xB8, 0x01, 0x02, 0xBB, 0x00, 0x80, 0xB9, 0x02, 0x00, 0xB6, 0x00, 0xCD, 0x13, // read CHS 0/0/2
            0x72, 0x05, 0xEA, 0x00, 0x80, 0x00, 0x00, // jc fail; jmp 0000:8000
            0xF4, 0xEB, 0xFD, // fail: hlt loop
        ];
        img[..boot.len()].copy_from_slice(boot);
        img[0x30..0x33].copy_from_slice(b"HI\0");
        img[510] = 0x55;
        img[511] = 0xAA;
        let stage2: &[u8] = &[
            0xB8, 0x00, 0xB8, 0x8E, 0xC0, // mov ax,B800; mov es,ax
            0x26, 0xC6, 0x06, 0xA0, 0x00, b'O', // mov byte es:[00A0],'O'
            0x26, 0xC6, 0x06, 0xA2, 0x00, b'K', // mov byte es:[00A2],'K'
            0xFB, 0xB4, 0x00, 0xCD, 0x16, // sti; mov ah,0; int 16h (wait key)
            0x26, 0xA2, 0xA4, 0x00, // mov es:[00A4],al
            0xF4, 0xEB, 0xFD,
        ];
        img[512..512 + stage2.len()].copy_from_slice(stage2);
        img
    }

    /// Writes the demo boot floppy used by the GUI smoke test.
    #[test]
    #[ignore]
    fn write_demo_image() {
        let path = std::env::var("HITBOY_DEMO_IMG").unwrap_or_else(|_| "demo.img".into());
        let mut img = test_floppy();
        // Colourful banner on row 3 via INT 10h/13h (write string with attribute).
        img[0x30..0x33].copy_from_slice(b"HI\0");
        std::fs::write(path, img).unwrap();
    }

    fn text_row(m: &mut Machine, row: usize) -> String {
        match m.screen() {
            Screen::Text { cols, cells, .. } => cells[row * cols..(row + 1) * cols].iter().map(|c| (*c as u8) as char).collect(),
            _ => panic!("expected text mode"),
        }
    }

    #[test]
    fn boots_floppy_uses_bios_and_reads_keyboard() {
        let path = std::env::temp_dir().join("hitboy_test_boot.img");
        std::fs::write(&path, test_floppy()).unwrap();
        let mut m = Machine::new(&Config { boot: Some(path.clone()), ..Default::default() }).unwrap();
        assert_eq!(m.boot_drive(), 0x00);
        let stats = m.run_frame(1.0 / 60.0, Duration::from_millis(200));
        assert!(stats.idle, "guest should be waiting on INT 16h");
        assert!(text_row(&mut m, 0).starts_with("HI"));
        assert!(text_row(&mut m, 1).starts_with("OK"));

        m.key_down(keys::scan_for_char('z').unwrap(), b'z');
        m.key_up(keys::scan_for_char('z').unwrap());
        for _ in 0..5 {
            m.run_frame(1.0 / 60.0, Duration::from_millis(50));
        }
        assert!(text_row(&mut m, 1).starts_with("OKz"), "row1 = {:?}", text_row(&mut m, 1));
        assert!(m.error().is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn timer_ticks_advance_bda_clock() {
        let path = std::env::temp_dir().join("hitboy_test_timer.img");
        std::fs::write(&path, test_floppy()).unwrap();
        let mut m = Machine::new(&Config { boot: Some(path.clone()), ..Default::default() }).unwrap();
        for _ in 0..60 {
            m.run_frame(1.0 / 60.0, Duration::from_millis(20));
        }
        let (_, cpu) = m.cpu.split();
        let ticks = u32::from_le_bytes(cpu.mem()[0x46C..0x470].try_into().unwrap());
        assert!((17..=19).contains(&ticks), "ticks after 1s = {ticks}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cp437_table_is_complete() {
        assert_eq!(keys::CP437[0xDB], '█');
        assert_eq!(keys::CP437[b'A' as usize], 'A');
        assert_eq!(keys::CP437[0xC9], '╔');
    }

    #[test]
    fn civil_date() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_723), (2026, 9, 27));
    }
}
