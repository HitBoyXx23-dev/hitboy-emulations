//! IDE controller (two channels: 0x1F0/0x3F6 on IRQ 14, 0x170/0x376 on IRQ 15) with ATA
//! hard disks and ATAPI CD-ROM drives, programmed I/O. The disk images themselves live in
//! the machine's drive table (shared with the BIOS disk services).

use std::collections::BTreeMap;

use crate::disk::{Disk, Kind};

#[cfg(test)]
const BSY: u8 = 0x80;
const DRDY: u8 = 0x40;
const DSC: u8 = 0x10;
const DRQ: u8 = 0x08;
const ERR: u8 = 0x01;

/// What the data port is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Xfer {
    None,
    /// Sending sectors to the host; `left` sectors remain after the buffer.
    Read { next: u64, left: u32 },
    /// Receiving sectors from the host.
    Write { next: u64, left: u32 },
    /// Receiving a 12-byte ATAPI command packet.
    Packet,
    /// Sending an ATAPI data reply.
    AtapiIn,
    /// Receiving IDENTIFY / other single-block data (then idle).
    Pio,
}

#[derive(Clone, Debug)]
pub struct Channel {
    /// BIOS drive numbers of master and slave (keys into the drive table).
    pub drives: [Option<u8>; 2],
    sel: usize,
    feature: u8,
    count: u16,
    lba: [u16; 3],
    device: u8,
    status: u8,
    error: u8,
    nien: bool,
    buf: Vec<u8>,
    pos: usize,
    xfer: Xfer,
    multiple: u32,
    /// Sectors per DRQ block for the current READ/WRITE.
    block: u32,
    pub irq: bool,
    // ATAPI state.
    sense: (u8, u8),
    atapi_reply: Vec<u8>,
    atapi_pos: usize,
    atapi_limit: usize,
    media_changed: bool,
}

impl Channel {
    pub fn new(master: Option<u8>, slave: Option<u8>) -> Self {
        Channel {
            drives: [master, slave],
            sel: 0,
            feature: 0,
            count: 1,
            lba: [1, 0, 0],
            device: 0,
            status: DRDY | DSC,
            error: 1,
            nien: false,
            buf: Vec::new(),
            pos: 0,
            xfer: Xfer::None,
            multiple: 16,
            block: 1,
            irq: false,
            sense: (0, 0),
            atapi_reply: Vec::new(),
            atapi_pos: 0,
            atapi_limit: 0,
            media_changed: true,
        }
    }

    fn drive<'a>(&self, drives: &'a mut BTreeMap<u8, Disk>) -> Option<&'a mut Disk> {
        self.drives[self.sel].and_then(move |n| drives.get_mut(&n))
    }

    fn present(&self, drives: &BTreeMap<u8, Disk>) -> bool {
        self.drives[self.sel].is_some_and(|n| drives.contains_key(&n))
    }

    fn atapi(&self, drives: &BTreeMap<u8, Disk>) -> bool {
        self.drives[self.sel].and_then(|n| drives.get(&n)).is_some_and(|d| d.kind == Kind::Cdrom)
    }

    fn raise(&mut self) {
        if !self.nien {
            self.irq = true;
        }
    }

    fn done(&mut self) {
        self.status = DRDY | DSC;
        self.xfer = Xfer::None;
        self.raise();
    }

    fn abort(&mut self) {
        self.status = DRDY | DSC | ERR;
        self.error = 0x04;
        self.xfer = Xfer::None;
        self.raise();
    }

    /// Current LBA from the task file (28-bit, or 48-bit with the high-order bytes).
    fn lba28(&self) -> u64 {
        (self.lba[0] as u64 & 0xFF) | (self.lba[1] as u64 & 0xFF) << 8 | (self.lba[2] as u64 & 0xFF) << 16 | ((self.device & 0xF) as u64) << 24
    }
    fn lba48(&self) -> u64 {
        let lo = (self.lba[0] as u64 & 0xFF) | (self.lba[1] as u64 & 0xFF) << 8 | (self.lba[2] as u64 & 0xFF) << 16;
        let hi = (self.lba[0] as u64 >> 8) | (self.lba[1] as u64 >> 8) << 8 | (self.lba[2] as u64 >> 8) << 16;
        lo | hi << 24
    }

    pub fn read(&mut self, reg: u16, drives: &mut BTreeMap<u8, Disk>) -> u8 {
        if !self.present(drives) && reg != 7 && reg != 8 {
            return 0;
        }
        match reg {
            1 => self.error,
            2 => self.count as u8,
            3 => self.lba[0] as u8,
            4 => self.lba[1] as u8,
            5 => self.lba[2] as u8,
            6 => self.device | 0xA0,
            7 => {
                if !self.present(drives) {
                    return 0;
                }
                self.irq = false;
                self.status
            }
            8 => {
                if !self.present(drives) {
                    return 0;
                }
                self.status // alternate status: no interrupt acknowledge
            }
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, reg: u16, v: u8, drives: &mut BTreeMap<u8, Disk>) {
        // Task-file writes push the previous byte into the high-order half (LBA48).
        let push = |r: &mut u16| *r = (*r << 8) | v as u16;
        match reg {
            1 => self.feature = v,
            2 => push(&mut self.count),
            3 => push(&mut self.lba[0]),
            4 => push(&mut self.lba[1]),
            5 => push(&mut self.lba[2]),
            6 => {
                self.device = v;
                self.sel = ((v >> 4) & 1) as usize;
            }
            7 => self.command(v, drives),
            8 => {
                if std::env::var_os("IDETRACE").is_some() {
                    eprintln!("ide ctl {v:02X} sel={}", self.sel);
                }
                self.nien = v & 2 != 0;
                if v & 4 != 0 {
                    // Software reset: both devices return their signatures.
                    self.sel = 0;
                    self.set_signature(drives);
                    self.status = DRDY | DSC;
                    self.error = 1;
                    self.xfer = Xfer::None;
                }
            }
            _ => {}
        }
    }

    fn set_signature(&mut self, drives: &BTreeMap<u8, Disk>) {
        self.count = 1;
        self.lba[0] = 1;
        if self.atapi(drives) {
            self.lba[1] = 0x14;
            self.lba[2] = 0xEB;
        } else {
            self.lba[1] = 0;
            self.lba[2] = 0;
        }
    }

    fn command(&mut self, c: u8, drives: &mut BTreeMap<u8, Disk>) {
        if std::env::var_os("IDETRACE").is_some() {
            eprintln!("ide cmd {c:02X} sel={} present={} drives={:?}", self.sel, self.present(drives), self.drives);
        }
        if !self.present(drives) {
            return;
        }
        let atapi = self.atapi(drives);
        self.error = 0;
        match c {
            0xEC if atapi => {
                self.set_signature(drives);
                self.abort();
            }
            0xEC | 0xA1 => {
                if (c == 0xA1) != atapi {
                    return self.abort();
                }
                let id = self.identify(drives);
                self.send_block(id);
            }
            0x20 | 0x21 | 0x24 | 0xC4 | 0x29 if !atapi => {
                let ext = c == 0x24 || c == 0x29;
                let (lba, n) = self.span(ext);
                self.block = if c == 0xC4 || c == 0x29 { self.multiple.max(1) } else { 1 };
                self.xfer = Xfer::Read { next: lba, left: n };
                self.fill_read(drives);
            }
            0x30 | 0x31 | 0x34 | 0xC5 | 0x39 if !atapi => {
                let ext = c == 0x34 || c == 0x39;
                let (lba, n) = self.span(ext);
                self.block = if c == 0xC5 || c == 0x39 { self.multiple.max(1) } else { 1 };
                let first = n.min(self.block);
                self.buf = vec![0; first as usize * 512];
                self.pos = 0;
                self.xfer = Xfer::Write { next: lba, left: n };
                self.status = DRDY | DSC | DRQ;
            }
            0xC6 if !atapi => {
                self.multiple = (self.count & 0xFF) as u32;
                self.done();
            }
            0x40 | 0x41 | 0x42 | 0x10..=0x1F | 0x91 | 0xEF | 0xE7 | 0xEA | 0xE0 | 0xE1 | 0xE2 | 0xE3 | 0xE6 if !atapi => self.done(),
            0xE5 => {
                self.count = 0xFF; // CHECK POWER MODE: active
                self.done();
            }
            0xF8 | 0x27 if !atapi => {
                // READ NATIVE MAX ADDRESS (EXT)
                let max = self.drive(drives).map(|d| d.sectors.saturating_sub(1)).unwrap_or(0);
                self.lba = [(max & 0xFF | (max >> 16) & 0xFF00) as u16, ((max >> 8) & 0xFF | (max >> 24) & 0xFF00) as u16, ((max >> 16) & 0xFF | (max >> 32) & 0xFF00) as u16];
                self.device = (self.device & 0xF0) | ((max >> 24) & 0xF) as u8;
                self.done();
            }
            0x08 if atapi => {
                self.set_signature(drives);
                self.status = DRDY | DSC;
            }
            0xA0 if atapi => {
                self.buf = vec![0; 12];
                self.pos = 0;
                self.xfer = Xfer::Packet;
                self.count = 1; // CoD = 1, IO = 0: send the command
                self.atapi_limit = ((self.lba[1] & 0xFF) | (self.lba[2] & 0xFF) << 8) as usize;
                if self.atapi_limit == 0 || self.atapi_limit == 0xFFFF {
                    self.atapi_limit = 0xFFFE;
                }
                self.status = DRDY | DSC | DRQ;
            }
            _ => self.abort(),
        }
    }

    /// (first LBA, sector count) of a READ/WRITE command.
    fn span(&self, ext: bool) -> (u64, u32) {
        if ext {
            let n = self.count as u32;
            (self.lba48(), if n == 0 { 65536 } else { n })
        } else {
            let n = (self.count & 0xFF) as u32;
            (self.lba28(), if n == 0 { 256 } else { n })
        }
    }

    fn send_block(&mut self, data: Vec<u8>) {
        self.buf = data;
        self.pos = 0;
        self.xfer = Xfer::Pio;
        self.status = DRDY | DSC | DRQ;
        self.raise();
    }

    fn fill_read(&mut self, drives: &mut BTreeMap<u8, Disk>) {
        let Xfer::Read { next, left } = self.xfer else { return };
        if left == 0 {
            return self.done();
        }
        let n = left.min(self.block);
        let mut data = vec![0u8; n as usize * 512];
        let ok = self.drive(drives).is_some_and(|d| d.read(next, n, &mut data));
        if !ok {
            self.error = 0x40; // uncorrectable
            self.status = DRDY | DSC | ERR;
            self.xfer = Xfer::None;
            return self.raise();
        }
        self.buf = data;
        self.pos = 0;
        self.xfer = Xfer::Read { next: next + n as u64, left: left - n };
        self.status = DRDY | DSC | DRQ;
        self.raise();
    }

    /// 16/32-bit data port read.
    pub fn read_data(&mut self, size: u8, drives: &mut BTreeMap<u8, Disk>) -> u32 {
        if self.status & DRQ == 0 {
            return 0;
        }
        let mut v = 0u32;
        for k in 0..size as usize {
            v |= (*self.buf.get(self.pos + k).unwrap_or(&0) as u32) << (k * 8);
        }
        self.pos += size as usize;
        if self.pos >= self.buf.len() {
            match self.xfer {
                Xfer::Read { .. } => self.fill_read(drives),
                Xfer::AtapiIn => self.atapi_next_chunk(),
                _ => {
                    self.status = DRDY | DSC;
                    self.xfer = Xfer::None;
                }
            }
        }
        v
    }

    /// 16/32-bit data port write.
    pub fn write_data(&mut self, size: u8, v: u32, drives: &mut BTreeMap<u8, Disk>) {
        if self.status & DRQ == 0 {
            return;
        }
        for k in 0..size as usize {
            if let Some(b) = self.buf.get_mut(self.pos + k) {
                *b = (v >> (k * 8)) as u8;
            }
        }
        self.pos += size as usize;
        if self.pos < self.buf.len() {
            return;
        }
        match self.xfer {
            Xfer::Write { next, left } => {
                let n = (self.buf.len() / 512) as u32;
                let data = std::mem::take(&mut self.buf);
                let ok = self.drive(drives).is_some_and(|d| d.write(next, n, &data));
                if !ok {
                    self.error = 0x40;
                    self.status = DRDY | DSC | ERR;
                    self.xfer = Xfer::None;
                    return self.raise();
                }
                let left = left - n;
                if left == 0 {
                    self.done();
                } else {
                    let m = left.min(self.block);
                    self.buf = vec![0; m as usize * 512];
                    self.pos = 0;
                    self.xfer = Xfer::Write { next: next + n as u64, left };
                    self.status = DRDY | DSC | DRQ;
                    self.raise();
                }
            }
            Xfer::Packet => {
                let pkt = std::mem::take(&mut self.buf);
                self.packet(&pkt, drives);
            }
            _ => {}
        }
    }

    fn identify(&self, drives: &mut BTreeMap<u8, Disk>) -> Vec<u8> {
        let mut w = [0u16; 256];
        let d = self.drive(drives).map(|d| (d.kind, d.sectors, d.cylinders, d.heads, d.spt));
        let (kind, sectors, cyl, heads, spt) = d.unwrap_or((Kind::Hdd, 0, 0, 0, 0));
        let text = |w: &mut [u16; 256], at: usize, len: usize, s: &str| {
            let mut b = s.as_bytes().to_vec();
            b.resize(len * 2, b' ');
            for i in 0..len {
                w[at + i] = (b[i * 2] as u16) << 8 | b[i * 2 + 1] as u16;
            }
        };
        if kind == Kind::Cdrom {
            w[0] = 0x85C0; // ATAPI, CD-ROM, removable, 12-byte packets
            text(&mut w, 10, 10, "HITBOY-CD-0001");
            text(&mut w, 23, 4, "1.0");
            text(&mut w, 27, 20, "HitBoy DVD-ROM");
            w[49] = 0x0200; // LBA
            w[53] = 0x0006;
            w[64] = 0x0003;
            w[80] = 0x007E;
        } else {
            let cyl = cyl.min(16383) as u16;
            w[0] = 0x0040;
            w[1] = cyl;
            w[3] = heads as u16;
            w[6] = spt as u16;
            text(&mut w, 10, 10, "HITBOY-HD-0001");
            text(&mut w, 23, 4, "1.0");
            text(&mut w, 27, 20, "HitBoy Hard Disk");
            w[47] = 0x8010; // READ/WRITE MULTIPLE up to 16 sectors
            w[49] = 0x0200; // LBA
            w[53] = 0x0007;
            w[54] = cyl;
            w[55] = heads as u16;
            w[56] = spt as u16;
            let chs = cyl as u32 * heads * spt;
            w[57] = chs as u16;
            w[58] = (chs >> 16) as u16;
            w[59] = 0x0100 | self.multiple as u16;
            let s28 = sectors.min(0x0FFF_FFFF) as u32;
            w[60] = s28 as u16;
            w[61] = (s28 >> 16) as u16;
            w[64] = 0x0003;
            w[80] = 0x007E; // ATA-1..6
            w[82] = 0x4000;
            w[83] = 0x4400; // LBA48
            w[84] = 0x4000;
            w[85] = 0x4000;
            w[86] = 0x0400;
            w[87] = 0x4000;
            for i in 0..4 {
                w[100 + i] = (sectors >> (16 * i)) as u16;
            }
        }
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    // ------------------------------------------------------------ ATAPI

    fn atapi_reply(&mut self, data: Vec<u8>, alloc: usize) {
        let mut data = data;
        data.truncate(alloc);
        if data.is_empty() {
            return self.atapi_ok();
        }
        self.atapi_reply = data;
        self.atapi_pos = 0;
        self.atapi_next_chunk();
    }

    fn atapi_next_chunk(&mut self) {
        let left = self.atapi_reply.len() - self.atapi_pos;
        if left == 0 {
            self.atapi_reply.clear();
            return self.atapi_ok();
        }
        let n = left.min(self.atapi_limit & !1).max(2.min(left));
        self.buf = self.atapi_reply[self.atapi_pos..self.atapi_pos + n].to_vec();
        self.atapi_pos += n;
        self.pos = 0;
        self.lba[1] = (n & 0xFF) as u16;
        self.lba[2] = (n >> 8) as u16;
        self.count = 2; // IO = 1, CoD = 0: data to the host
        self.xfer = Xfer::AtapiIn;
        self.status = DRDY | DSC | DRQ;
        self.raise();
    }

    fn atapi_ok(&mut self) {
        self.sense = (0, 0);
        self.count = 3; // IO = 1, CoD = 1: command complete
        self.status = DRDY | DSC;
        self.xfer = Xfer::None;
        self.raise();
    }

    fn atapi_error(&mut self, key: u8, asc: u8) {
        self.sense = (key, asc);
        self.error = key << 4;
        self.count = 3;
        self.status = DRDY | DSC | ERR;
        self.xfer = Xfer::None;
        self.raise();
    }

    fn packet(&mut self, p: &[u8], drives: &mut BTreeMap<u8, Disk>) {
        if std::env::var_os("IDETRACE").is_some() {
            eprintln!("atapi {:02X?}", &p[..12.min(p.len())]);
        }
        let be16 = |i: usize| u16::from_be_bytes([p[i], p[i + 1]]) as usize;
        let be32 = |i: usize| u32::from_be_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]]) as u64;
        let blocks = self.drive(drives).map(|d| d.sectors * d.sector_size as u64 / 2048).unwrap_or(0);
        // A fresh disc reports "medium may have changed" once, like a real drive.
        if self.media_changed && !matches!(p[0], 0x03 | 0x12 | 0x46 | 0x4A | 0xBD) {
            self.media_changed = false;
            return self.atapi_error(6, 0x28);
        }
        match p[0] {
            0x00 | 0x1E | 0x1B | 0xBB | 0x35 => self.atapi_ok(),
            0x03 => {
                let mut s = vec![0u8; 18];
                s[0] = 0x70;
                s[2] = self.sense.0;
                s[7] = 10;
                s[12] = self.sense.1;
                self.sense = (0, 0);
                self.atapi_reply(s, p[4] as usize);
            }
            0x12 => {
                let mut d = vec![0u8; 36];
                d[0] = 0x05; // CD/DVD
                d[1] = 0x80; // removable
                d[2] = 0x05;
                d[3] = 0x32;
                d[4] = 31;
                d[8..16].copy_from_slice(b"HITBOY  ");
                d[16..32].copy_from_slice(b"DVD-ROM         ");
                d[32..36].copy_from_slice(b"1.0 ");
                self.atapi_reply(d, p[4] as usize);
            }
            0x25 => {
                let mut d = vec![0u8; 8];
                d[0..4].copy_from_slice(&(blocks.saturating_sub(1) as u32).to_be_bytes());
                d[4..8].copy_from_slice(&2048u32.to_be_bytes());
                self.atapi_reply(d, 8);
            }
            0x28 | 0xA8 => {
                let lba = be32(2);
                let n = if p[0] == 0x28 { be16(7) as u64 } else { be32(6) };
                if n == 0 {
                    return self.atapi_ok();
                }
                if lba + n > blocks {
                    return self.atapi_error(5, 0x21); // LBA out of range
                }
                let mut data = vec![0u8; n as usize * 2048];
                let ok = self.drive(drives).is_some_and(|d| {
                    let per = 2048 / d.sector_size as u64;
                    d.read(lba * per, (n * per) as u32, &mut data)
                });
                if !ok {
                    return self.atapi_error(3, 0x11);
                }
                self.atapi_reply(data, usize::MAX);
            }
            0x43 => {
                // READ TOC (format 0): one data track plus the lead-out.
                let msf = p[1] & 2 != 0;
                let addr = |lba: u64| -> [u8; 4] {
                    if msf {
                        let f = lba + 150;
                        [0, (f / 4500) as u8, (f / 75 % 60) as u8, (f % 75) as u8]
                    } else {
                        (lba as u32).to_be_bytes()
                    }
                };
                let mut d = vec![0, 18, 1, 1];
                d.extend([0, 0x14, 1, 0]);
                d.extend(addr(0));
                d.extend([0, 0x14, 0xAA, 0]);
                d.extend(addr(blocks));
                self.atapi_reply(d, be16(7));
            }
            0x46 => {
                // GET CONFIGURATION: current profile DVD-ROM.
                let mut d = vec![0u8; 8];
                d[3] = 4;
                d[6..8].copy_from_slice(&0x0010u16.to_be_bytes());
                self.atapi_reply(d, be16(7));
            }
            0x4A => {
                // GET EVENT STATUS NOTIFICATION: no events.
                self.atapi_reply(vec![0, 2, 0x80, 0x10], be16(7));
            }
            0x51 => {
                let mut d = vec![0u8; 34];
                d[1] = 32;
                d[2] = 0x0E; // complete, finalized
                d[3] = 1;
                d[4] = 1;
                d[5] = 1;
                d[6] = 1;
                self.atapi_reply(d, be16(7));
            }
            0x5A | 0x1A => {
                // MODE SENSE: an empty mode parameter header.
                let d = if p[0] == 0x5A { vec![0, 6, 0x70, 0, 0, 0, 0, 0] } else { vec![3, 0x70, 0, 0] };
                let alloc = if p[0] == 0x5A { be16(7) } else { p[4] as usize };
                self.atapi_reply(d, alloc);
            }
            0xBD => {
                let mut d = vec![0u8; 8];
                d[5] = 1;
                self.atapi_reply(d, be16(8));
            }
            _ => self.atapi_error(5, 0x20), // invalid command
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdd() -> BTreeMap<u8, Disk> {
        let path = std::env::temp_dir().join(format!("ide_test_{}.img", std::process::id()));
        let mut img = vec![0u8; 1 << 20];
        for (i, s) in img.chunks_mut(512).enumerate() {
            s[0] = i as u8;
        }
        std::fs::write(&path, img).unwrap();
        let mut d = Disk::open_image(&path).unwrap();
        d.kind = Kind::Hdd;
        d.writable = true;
        let mut m = BTreeMap::new();
        m.insert(0x80, d);
        m
    }

    #[test]
    fn identify_and_read_write_sectors() {
        let mut drives = hdd();
        let mut ch = Channel::new(Some(0x80), None);
        ch.write(6, 0xA0, &mut drives);
        ch.write(7, 0xEC, &mut drives);
        assert_ne!(ch.read(7, &mut drives) & DRQ, 0);
        let id: Vec<u16> = (0..256).map(|_| ch.read_data(2, &mut drives) as u16).collect();
        assert_eq!(id[0], 0x0040);
        assert_eq!(id[60] as u32 | (id[61] as u32) << 16, 2048);
        // READ SECTORS, LBA 5, 2 sectors.
        ch.write(2, 2, &mut drives);
        ch.write(3, 5, &mut drives);
        ch.write(4, 0, &mut drives);
        ch.write(5, 0, &mut drives);
        ch.write(6, 0xE0, &mut drives);
        ch.write(7, 0x20, &mut drives);
        let first: Vec<u8> = (0..256).flat_map(|_| (ch.read_data(2, &mut drives) as u16).to_le_bytes()).collect();
        assert_eq!(first[0], 5);
        assert_ne!(ch.read(7, &mut drives) & DRQ, 0);
        let second = ch.read_data(2, &mut drives);
        assert_eq!(second & 0xFF, 6);
        for _ in 1..256 {
            ch.read_data(2, &mut drives);
        }
        assert_eq!(ch.read(7, &mut drives) & (DRQ | BSY), 0);
        // WRITE SECTORS then read back.
        ch.write(2, 1, &mut drives);
        ch.write(3, 9, &mut drives);
        ch.write(7, 0x30, &mut drives);
        for _ in 0..256 {
            ch.write_data(2, 0xBEEF, &mut drives);
        }
        let mut b = vec![0u8; 512];
        drives.get_mut(&0x80).unwrap().read(9, 1, &mut b);
        assert_eq!(&b[..2], &[0xEF, 0xBE]);
    }
}
