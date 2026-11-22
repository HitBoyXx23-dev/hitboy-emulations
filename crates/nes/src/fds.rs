//! Famicom Disk System: the RAM adapter (32 KiB work RAM, 8 KiB CHR RAM, the user's BIOS),
//! the disk drive (read/write with gaps and CRCs, byte-transfer IRQs), the timer IRQ and the
//! wavetable sound channel with its frequency modulator.

use crate::cartridge::{Mapper, Mirroring};

pub const SIDE_BYTES: usize = 65_500;

/// Splits an .fds file (with or without its 16-byte header) into disk sides and lays each out
/// the way the drive head sees it: leading gap, then every block with a start mark, CRC and gap.
pub fn load_sides(file: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let body = if file.starts_with(b"FDS\x1A") { &file[16..] } else { file };
    if body.len() < SIDE_BYTES {
        return Err("This .fds file is too small to hold a disk side.".into());
    }
    let sides = body.len() / SIDE_BYTES;
    let mut out = Vec::with_capacity(sides);
    for s in 0..sides {
        let raw = &body[s * SIDE_BYTES..(s + 1) * SIDE_BYTES];
        if raw[0] != 1 || &raw[1..15] != b"*NINTENDO-HVC*" {
            return Err(format!("Disk side {} doesn't start with the Famicom Disk System header.", s + 1));
        }
        out.push(with_gaps(raw));
    }
    Ok(out)
}

fn with_gaps(raw: &[u8]) -> Vec<u8> {
    let mut side = vec![0u8; 28_300 / 8];
    let mut j = 0usize;
    let get = |i: usize| *raw.get(i).unwrap_or(&0);
    while j < raw.len() {
        let len = match raw[j] {
            1 => 56,
            2 => 2,
            3 => 16,
            4 => 1 + (get(j.wrapping_sub(3)) as usize | (get(j.wrapping_sub(2)) as usize) << 8),
            _ => break,
        };
        if j + len > raw.len() {
            break;
        }
        side.push(0x80);
        side.extend_from_slice(&raw[j..j + len]);
        side.extend_from_slice(&crc(&raw[j..j + len]).to_le_bytes());
        side.extend(std::iter::repeat_n(0u8, 976 / 8));
        j += len;
    }
    side.resize(side.len().max(SIDE_BYTES + 28_300 / 8), 0);
    side
}

fn crc_step(c: u16, v: u8) -> u16 {
    let mut c = c ^ v as u16;
    for _ in 0..8 {
        let carry = c & 1;
        c >>= 1;
        if carry != 0 {
            c ^= 0x8408;
        }
    }
    c
}

/// The drive's CRC over a block including its 0x80 start mark.
fn crc(block: &[u8]) -> u16 {
    let mut c = crc_step(0, 0x80);
    for &b in block {
        c = crc_step(c, b);
    }
    // Two trailing zero bytes flush the register, as the hardware does.
    c = crc_step(c, 0);
    crc_step(c, 0)
}

// ---------------------------------------------------------------- sound

#[derive(Default, state::Stateful)]
struct Envelope {
    speed: u8,
    gain: u8,
    off: bool,
    increase: bool,
    freq: u16,
    timer: u32,
    master: u8,
}

impl Envelope {
    fn write(&mut self, reg: u16, v: u8) {
        match reg & 3 {
            0 => {
                self.speed = v & 0x3F;
                self.increase = v & 0x40 != 0;
                self.off = v & 0x80 != 0;
                self.reset_timer();
                if self.off {
                    self.gain = self.speed;
                }
            }
            2 => self.freq = (self.freq & 0x0F00) | v as u16,
            3 => self.freq = (self.freq & 0xFF) | ((v as u16 & 0x0F) << 8),
            _ => {}
        }
    }

    fn reset_timer(&mut self) {
        self.timer = 8 * (self.speed as u32 + 1) * self.master as u32;
    }

    fn tick(&mut self) -> bool {
        if !self.off && self.master > 0 {
            self.timer = self.timer.saturating_sub(1);
            if self.timer == 0 {
                self.reset_timer();
                if self.increase && self.gain < 32 {
                    self.gain += 1;
                } else if !self.increase && self.gain > 0 {
                    self.gain -= 1;
                }
                return true;
            }
        }
        false
    }
}

#[derive(state::Stateful)]
struct Audio {
    wave: [u8; 64],
    wave_pos: u8,
    wave_acc: u16,
    wave_write: bool,
    halt: bool,
    no_env: bool,
    master_vol: u8,
    vol: Envelope,
    modu: Envelope,
    mod_table: [u8; 64],
    mod_pos: u8,
    mod_acc: u16,
    mod_counter: i8,
    mod_disabled: bool,
    mod_out: i32,
    level: u8,
}

impl Default for Audio {
    fn default() -> Self {
        Audio {
            wave: [0; 64],
            wave_pos: 0,
            wave_acc: 0,
            wave_write: false,
            halt: true,
            no_env: false,
            master_vol: 0,
            vol: Envelope { master: 0xE8, ..Default::default() },
            modu: Envelope { master: 0xE8, ..Default::default() },
            mod_table: [0; 64],
            mod_pos: 0,
            mod_acc: 0,
            mod_counter: 0,
            mod_disabled: true,
            mod_out: 0,
            level: 0,
        }
    }
}

const MOD_STEP: [i32; 8] = [0, 1, 2, 4, 0xFF, -4, -2, -1];
const WAVE_VOLUME: [u32; 4] = [36, 24, 17, 14];

impl Audio {
    fn set_counter(&mut self, v: i32) {
        let mut c = v;
        if c >= 64 {
            c -= 128;
        } else if c < -64 {
            c += 128;
        }
        self.mod_counter = c as i8;
    }

    fn update_mod(&mut self) {
        let pitch = self.vol.freq as i32;
        let mut temp = self.mod_counter as i32 * self.modu.gain as i32;
        let rem = temp & 0xF;
        temp >>= 4;
        if rem > 0 && temp & 0x80 == 0 {
            temp += if self.mod_counter < 0 { -1 } else { 2 };
        }
        if temp >= 192 {
            temp -= 256;
        } else if temp < -64 {
            temp += 256;
        }
        temp *= pitch;
        let rem = temp & 0x3F;
        temp >>= 6;
        if rem >= 32 {
            temp += 1;
        }
        self.mod_out = temp;
    }

    fn write(&mut self, addr: u16, v: u8) {
        match addr {
            0x4040..=0x407F => {
                if self.wave_write {
                    self.wave[(addr & 0x3F) as usize] = v & 0x3F;
                }
            }
            0x4080 | 0x4082 => {
                self.vol.write(addr, v);
                self.update_mod();
            }
            0x4083 => {
                self.no_env = v & 0x40 != 0;
                self.halt = v & 0x80 != 0;
                if self.halt {
                    self.wave_pos = 0;
                }
                if self.no_env {
                    self.vol.reset_timer();
                    self.modu.reset_timer();
                }
                self.vol.write(addr, v);
                self.update_mod();
            }
            0x4084 | 0x4086 => {
                self.modu.write(addr, v);
                self.update_mod();
            }
            0x4085 => {
                self.set_counter((v & 0x7F) as i32);
                self.update_mod();
            }
            0x4087 => {
                self.modu.write(addr, v);
                self.mod_disabled = v & 0x80 != 0;
                if self.mod_disabled {
                    self.mod_acc = 0;
                }
            }
            0x4088 => {
                if self.mod_disabled {
                    let p = self.mod_pos as usize;
                    self.mod_table[p & 0x3F] = v & 7;
                    self.mod_table[(p + 1) & 0x3F] = v & 7;
                    self.mod_pos = (self.mod_pos + 2) & 0x3F;
                }
            }
            0x4089 => {
                self.master_vol = v & 3;
                self.wave_write = v & 0x80 != 0;
            }
            0x408A => {
                self.vol.master = v;
                self.modu.master = v;
            }
            _ => {}
        }
    }

    fn read(&self, addr: u16) -> u8 {
        match addr {
            0x4040..=0x407F => {
                if self.wave_write {
                    self.wave[(addr & 0x3F) as usize]
                } else {
                    self.wave[self.wave_pos as usize]
                }
            }
            0x4090 => self.vol.gain,
            0x4092 => self.modu.gain,
            _ => 0,
        }
    }

    fn clock(&mut self) {
        let freq = self.vol.freq as i32;
        if !self.halt && !self.no_env {
            self.vol.tick();
            if self.modu.tick() {
                self.update_mod();
            }
        }
        if !self.mod_disabled && self.modu.freq > 0 {
            let before = self.mod_acc;
            self.mod_acc = self.mod_acc.wrapping_add(self.modu.freq);
            if self.mod_acc < before {
                let step = MOD_STEP[self.mod_table[self.mod_pos as usize] as usize];
                let next = if step == 0xFF { 0 } else { self.mod_counter as i32 + step };
                self.set_counter(next);
                self.mod_pos = (self.mod_pos + 1) & 0x3F;
                self.update_mod();
            }
        }
        if !self.wave_write {
            let level = (self.vol.gain.min(32) as u32) * WAVE_VOLUME[self.master_vol as usize];
            self.level = ((self.wave[self.wave_pos as usize] as u32 * level) / 1152) as u8;
        }
        let pitch = freq + self.mod_out;
        if !self.halt && pitch > 0 {
            let before = self.wave_acc;
            self.wave_acc = self.wave_acc.wrapping_add(pitch as u16);
            if self.wave_acc < before {
                self.wave_pos = (self.wave_pos + 1) & 0x3F;
            }
        }
    }
}

// ---------------------------------------------------------------- the RAM adapter and drive

#[derive(state::Stateful)]
pub struct Fds {
    #[state(skip)]
    bios: Vec<u8>,
    ram: Vec<u8>,
    chr: Vec<u8>,
    mirroring: Mirroring,
    pub sides: Vec<Vec<u8>>,
    /// Inserted side, or None while the disk is out (between side changes).
    pub disk: Option<usize>,
    eject_timer: u32,
    pending_side: usize,
    pub dirty: bool,
    // Timer IRQ
    irq_reload: u16,
    irq_counter: u16,
    irq_enabled: bool,
    irq_repeat: bool,
    timer_irq: bool,
    // Drive
    disk_regs: bool,
    sound_regs: bool,
    write_data: u8,
    motor_on: bool,
    reset_transfer: bool,
    read_mode: bool,
    crc_control: bool,
    disk_ready: bool,
    disk_irq_enabled: bool,
    disk_irq: bool,
    pos: usize,
    delay: i32,
    end_of_head: bool,
    gap_ended: bool,
    scanning: bool,
    transfer_complete: bool,
    read_data: u8,
    prev_crc_flag: bool,
    crc: u16,
    ext: u8,
    audio: Audio,
}

impl Fds {
    pub fn new(bios: Vec<u8>, sides: Vec<Vec<u8>>) -> Result<Fds, String> {
        if bios.len() != 0x2000 {
            return Err(format!("The Famicom Disk System BIOS (disksys.rom) should be 8 KB; this one is {} bytes.", bios.len()));
        }
        Ok(Fds {
            bios,
            ram: vec![0; 0x8000],
            chr: vec![0; 0x2000],
            mirroring: Mirroring::Horizontal,
            sides,
            disk: Some(0),
            eject_timer: 0,
            pending_side: 0,
            dirty: false,
            irq_reload: 0,
            irq_counter: 0,
            irq_enabled: false,
            irq_repeat: false,
            timer_irq: false,
            disk_regs: false,
            sound_regs: false,
            write_data: 0,
            motor_on: false,
            reset_transfer: false,
            read_mode: true,
            crc_control: false,
            disk_ready: false,
            disk_irq_enabled: false,
            disk_irq: false,
            pos: 0,
            delay: 0,
            end_of_head: true,
            gap_ended: false,
            scanning: false,
            transfer_complete: false,
            read_data: 0,
            prev_crc_flag: false,
            crc: 0,
            ext: 0x80,
            audio: Audio::default(),
        })
    }

    /// Ejects the disk and inserts `side` about half a second later, like a person would.
    pub fn change_side(&mut self, side: usize) {
        self.disk = None;
        self.pending_side = side % self.sides.len().max(1);
        self.eject_timer = 1_000_000;
    }

    fn clock_disk(&mut self) {
        if self.irq_enabled {
            if self.irq_counter == 0 {
                self.timer_irq = true;
                self.irq_counter = self.irq_reload;
                if !self.irq_repeat {
                    self.irq_enabled = false;
                }
            } else {
                self.irq_counter -= 1;
            }
        }
        if self.eject_timer > 0 {
            self.eject_timer -= 1;
            if self.eject_timer == 0 {
                self.disk = Some(self.pending_side);
            }
        }
        let Some(d) = self.disk else {
            self.end_of_head = true;
            self.scanning = false;
            return;
        };
        if !self.motor_on {
            self.end_of_head = true;
            self.scanning = false;
            return;
        }
        if self.reset_transfer && !self.scanning {
            return;
        }
        if self.end_of_head {
            self.delay = 50_000;
            self.end_of_head = false;
            self.pos = 0;
            self.gap_ended = false;
            return;
        }
        if self.delay > 0 {
            self.delay -= 1;
            return;
        }
        self.scanning = true;
        let mut irq = self.disk_irq_enabled;
        if self.read_mode {
            let byte = self.sides[d][self.pos];
            if !self.prev_crc_flag {
                self.crc = crc_step(self.crc, byte);
            }
            if !self.disk_ready {
                self.gap_ended = false;
                self.crc = 0;
            } else if byte != 0 && !self.gap_ended {
                self.gap_ended = true;
                irq = false;
            }
            if self.gap_ended {
                self.transfer_complete = true;
                self.read_data = byte;
                if irq {
                    self.disk_irq = true;
                }
            }
        } else {
            let mut byte = 0u8;
            if !self.crc_control {
                self.transfer_complete = true;
                byte = self.write_data;
                if irq {
                    self.disk_irq = true;
                }
            }
            if !self.disk_ready {
                byte = 0;
                self.crc = 0;
            }
            if !self.crc_control {
                self.crc = crc_step(self.crc, byte);
            } else {
                byte = self.crc as u8;
                self.crc >>= 8;
            }
            if self.sides[d][self.pos] != byte {
                self.sides[d][self.pos] = byte;
                self.dirty = true;
            }
            self.gap_ended = false;
        }
        self.prev_crc_flag = self.crc_control;
        self.pos += 1;
        if self.pos >= self.sides[d].len() {
            self.motor_on = false;
            if self.disk_irq_enabled {
                self.disk_irq = true;
            }
        } else {
            self.delay = 149;
        }
    }
}

impl Mapper for Fds {
    fn sync(&mut self, s: &mut state::State) {
        state::Stateful::sync(self, s)
    }
    fn cpu_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x4030 if self.disk_regs => {
                let mut v = 0u8;
                if self.timer_irq {
                    v |= 1;
                }
                if self.transfer_complete {
                    v |= 2;
                }
                if self.mirroring == Mirroring::Horizontal {
                    v |= 8;
                }
                self.transfer_complete = false;
                self.timer_irq = false;
                self.disk_irq = false;
                v
            }
            0x4031 if self.disk_regs => {
                self.transfer_complete = false;
                self.disk_irq = false;
                self.read_data
            }
            0x4032 if self.disk_regs => {
                let inserted = self.disk.is_some();
                let mut v = 0x40u8;
                if !inserted {
                    v |= 1 | 4;
                }
                if !inserted || !self.scanning {
                    v |= 2;
                }
                v
            }
            0x4033 if self.disk_regs => self.ext,
            0x4040..=0x4097 if self.sound_regs => self.audio.read(addr) | 0x40,
            0x6000..=0xDFFF => self.ram[(addr - 0x6000) as usize],
            0xE000..=0xFFFF => self.bios[(addr - 0xE000) as usize],
            _ => 0,
        }
    }

    fn cpu_write(&mut self, addr: u16, v: u8) {
        if (!self.disk_regs && (0x4024..=0x4026).contains(&addr)) || (!self.sound_regs && addr >= 0x4040 && addr < 0x6000) {
            return;
        }
        match addr {
            0x4020 => self.irq_reload = (self.irq_reload & 0xFF00) | v as u16,
            0x4021 => self.irq_reload = (self.irq_reload & 0x00FF) | (v as u16) << 8,
            0x4022 => {
                self.irq_repeat = v & 1 != 0;
                self.irq_enabled = v & 2 != 0 && self.disk_regs;
                if self.irq_enabled {
                    self.irq_counter = self.irq_reload;
                } else {
                    self.timer_irq = false;
                }
            }
            0x4023 => {
                self.disk_regs = v & 1 != 0;
                self.sound_regs = v & 2 != 0;
                if !self.disk_regs {
                    self.irq_enabled = false;
                    self.timer_irq = false;
                    self.disk_irq = false;
                }
            }
            0x4024 => {
                self.write_data = v;
                self.transfer_complete = false;
                self.disk_irq = false;
            }
            0x4025 => {
                self.motor_on = v & 1 != 0;
                self.reset_transfer = v & 2 != 0;
                self.read_mode = v & 4 != 0;
                self.mirroring = if v & 8 != 0 { Mirroring::Horizontal } else { Mirroring::Vertical };
                self.crc_control = v & 0x10 != 0;
                self.disk_ready = v & 0x40 != 0;
                self.disk_irq_enabled = v & 0x80 != 0;
                self.disk_irq = false;
            }
            0x4026 => self.ext = v,
            0x4040..=0x408A => self.audio.write(addr, v),
            0x6000..=0xDFFF => self.ram[(addr - 0x6000) as usize] = v,
            _ => {}
        }
    }

    fn ppu_read(&mut self, addr: u16) -> u8 {
        self.chr[addr as usize & 0x1FFF]
    }

    fn ppu_write(&mut self, addr: u16, v: u8) {
        self.chr[addr as usize & 0x1FFF] = v;
    }

    fn mirroring(&self) -> Mirroring {
        self.mirroring
    }

    fn irq(&self) -> bool {
        self.timer_irq || self.disk_irq
    }

    fn tick(&mut self, cycles: u32) {
        for _ in 0..cycles {
            self.clock_disk();
            self.audio.clock();
        }
    }

    fn audio(&self) -> f32 {
        // The wave channel peaks at 63; scale it to sit alongside the APU's own channels.
        self.audio.level as f32 / 63.0 * 0.45
    }

    fn save(&self) -> Option<Vec<u8>> {
        Some(self.sides.concat())
    }

    fn load_save(&mut self, data: &[u8]) {
        let total: usize = self.sides.iter().map(Vec::len).sum();
        if data.len() == total {
            let mut off = 0;
            for s in self.sides.iter_mut() {
                let n = s.len();
                s.copy_from_slice(&data[off..off + n]);
                off += n;
            }
        }
    }

    fn next_disk_side(&mut self) -> Option<usize> {
        let next = (self.disk.unwrap_or(self.pending_side) + 1) % self.sides.len().max(1);
        self.change_side(next);
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side() -> Vec<u8> {
        let mut raw = vec![0u8; SIDE_BYTES];
        raw[0] = 1;
        raw[1..15].copy_from_slice(b"*NINTENDO-HVC*");
        raw[56] = 2;
        raw[57] = 1;
        let h = 58;
        raw[h] = 3;
        raw[h + 13] = 4; // file size 4
        raw[h + 16] = 4;
        raw[h + 17..h + 21].copy_from_slice(&[9, 8, 7, 6]);
        raw
    }

    #[test]
    fn disk_layout_has_gaps_marks_and_blocks() {
        let s = load_sides(&side()).unwrap();
        assert_eq!(s.len(), 1);
        let d = &s[0];
        let first = d.iter().position(|&b| b != 0).unwrap();
        assert_eq!(first, 28_300 / 8, "leading gap");
        assert_eq!(d[first], 0x80, "start mark");
        assert_eq!(d[first + 1], 1, "disk info block");
        // The data block (type 4) follows with its 4 bytes.
        let p = d.windows(6).position(|w| w == [0x80, 4, 9, 8, 7, 6]).expect("data block present");
        assert!(p > first);
    }

    #[test]
    fn timer_irq_counts_cpu_cycles() {
        let mut f = Fds::new(vec![0; 0x2000], load_sides(&side()).unwrap()).unwrap();
        f.cpu_write(0x4023, 1);
        f.cpu_write(0x4020, 10);
        f.cpu_write(0x4021, 0);
        f.cpu_write(0x4022, 2);
        f.tick(10);
        assert!(!f.irq());
        f.tick(1);
        assert!(f.irq());
        f.cpu_read(0x4030);
        assert!(!f.irq(), "reading status acknowledges");
    }
}
