//! CD-ROM controller: command/response protocol with timed interrupts, sector reads into
//! the data FIFO, XA-ADPCM streaming audio and CD-DA playback.

use crate::disc::{bcd, from_bcd, lba_to_msf, Disc, SECTOR};
use std::collections::VecDeque;

const CPU_HZ: i32 = 33_868_800;
const FIRST_RESPONSE: i32 = 25_000;
const SECOND_RESPONSE: i32 = 60_000;

#[derive(state::Stateful)]
#[derive(Default)]
struct Pending {
    delay: i32,
    int: u8,
    resp: Vec<u8>,
}

#[derive(PartialEq, Clone, Copy, state::Stateful)]
enum Drive {
    Idle,
    Reading,
    Playing,
}

#[derive(state::Stateful)]
pub struct Cdrom {
    #[state(skip)]
    pub disc: Option<Disc>,
    #[state(skip)]
    region: [u8; 4],
    index: u8,
    params: VecDeque<u8>,
    resp: VecDeque<u8>,
    data: Vec<u8>,
    data_pos: usize,
    ie: u8,
    iflag: u8,
    queue: VecDeque<Pending>,
    mode: u8,
    setloc: Option<u32>,
    lba: u32,
    drive: Drive,
    read_timer: i32,
    sector: Box<[u8; SECTOR]>,
    sector_ready: bool,
    filter: (u8, u8),
    muted: bool,
    xa_hist: [[i32; 2]; 2],
    xa_phase: u32,
    /// Sectors played since the last CD-DA position report.
    report_count: u32,
    /// Track being played (auto-pause fires when it changes).
    play_track: u8,
    /// 44.1 kHz stereo CD audio (XA + CD-DA) consumed by the SPU.
    pub audio: VecDeque<(i16, i16)>,
    /// Recent commands (diagnostics).
    #[state(skip)]
    pub log: VecDeque<String>,
    /// Diagnostics: data sectors signalled, XA sectors, sector loads into the FIFO, bytes read.
    #[state(skip)]
    pub stats: [u64; 7],
    /// Set when a data interrupt is delivered (diagnostic tracing).
    #[state(skip)]
    pub trace_request: bool,
}

impl Cdrom {
    pub fn new(mut disc: Option<Disc>) -> Self {
        let region = *disc.as_mut().map(|d| d.region()).unwrap_or(b"SCEA");
        Cdrom {
            disc,
            region,
            index: 0,
            params: VecDeque::new(),
            resp: VecDeque::new(),
            data: Vec::new(),
            data_pos: 0,
            ie: 0,
            iflag: 0,
            queue: VecDeque::new(),
            mode: 0,
            setloc: None,
            lba: 0,
            drive: Drive::Idle,
            read_timer: 0,
            sector: Box::new([0; SECTOR]),
            sector_ready: false,
            filter: (0, 0),
            muted: false,
            xa_hist: [[0; 2]; 2],
            xa_phase: 0,
            report_count: 0,
            play_track: 0,
            audio: VecDeque::new(),
            log: VecDeque::new(),
            stats: [0; 7],
            trace_request: false,
        }
    }

    fn stat(&self) -> u8 {
        if self.disc.is_none() {
            return 0x10; // shell open / no disc
        }
        0x02 | match self.drive {
            Drive::Reading => 0x20,
            Drive::Playing => 0x80,
            Drive::Idle => 0,
        }
    }

    fn push(&mut self, delay: i32, int: u8, resp: Vec<u8>) {
        self.queue.push_back(Pending { delay, int, resp });
    }

    fn ack(&mut self, delay: i32) {
        let s = self.stat();
        self.push(delay, 3, vec![s]);
    }

    fn period(&self) -> i32 {
        if self.mode & 0x80 != 0 {
            CPU_HZ / 150
        } else {
            CPU_HZ / 75
        }
    }

    // ------------------------------------------------------------------ registers

    pub fn read(&mut self, reg: u32) -> u8 {
        match reg & 3 {
            0 => {
                let mut s = self.index;
                s |= (self.params.is_empty() as u8) << 3;
                s |= ((self.params.len() < 16) as u8) << 4;
                s |= (!self.resp.is_empty() as u8) << 5;
                s |= ((self.data_pos < self.data.len()) as u8) << 6;
                s |= (self.queue.front().map(|p| p.int == 3).unwrap_or(false) as u8) << 7;
                s
            }
            1 => self.resp.pop_front().unwrap_or(0),
            2 => self.data_byte(),
            _ => {
                if self.index & 1 == 0 {
                    self.ie | 0xE0
                } else {
                    self.iflag | 0xE0
                }
            }
        }
    }

    pub fn data_byte(&mut self) -> u8 {
        self.stats[3] += 1;
        if self.data_pos < self.data.len() {
            self.data_pos += 1;
            self.data[self.data_pos - 1]
        } else {
            *self.data.last().unwrap_or(&0)
        }
    }

    pub fn dma_word(&mut self) -> u32 {
        u32::from_le_bytes([self.data_byte(), self.data_byte(), self.data_byte(), self.data_byte()])
    }

    pub fn write(&mut self, reg: u32, v: u8) {
        match (reg & 3, self.index) {
            (0, _) => self.index = v & 3,
            (1, 0) => self.command(v),
            (2, 0) => {
                if self.params.len() < 16 {
                    self.params.push_back(v)
                }
            }
            (2, 1) => self.ie = v & 0x1F,
            (3, 0) => {
                if v & 0x80 != 0 {
                    self.stats[5] += 1;

                    if self.data_pos >= self.data.len() && self.sector_ready {
                        self.stats[2] += 1;
                        let (off, len) = if self.mode & 0x20 != 0 { (12, 2340) } else { (24, 2048) };
                        self.data = self.sector[off..off + len].to_vec();
                        self.data_pos = 0;
                    }
                } else {
                    self.data.clear();
                    self.data_pos = 0;
                }
            }
            (3, 1) => {

                self.iflag &= !(v & 0x1F);
                if v & 0x40 != 0 {
                    self.params.clear();
                }
            }
            (3, 3) if v & 0x08 == 0 => {}
            _ => {}
        }
    }

    // ------------------------------------------------------------------ commands

    fn param(&mut self) -> u8 {
        self.params.pop_front().unwrap_or(0)
    }

    fn command(&mut self, cmd: u8) {
        if self.log.len() >= 400 {
            self.log.pop_front();
        }
        self.log.push_back(format!("cmd {cmd:02X} params {:02X?} lba {} mode {:02X}", self.params, self.lba, self.mode));
        let no_disc = self.disc.is_none();
        match cmd {
            0x01 => self.ack(FIRST_RESPONSE),
            0x02 => {
                let (m, s, f) = (from_bcd(self.param()), from_bcd(self.param()), from_bcd(self.param()));
                self.setloc = Some(((m * 60 + s) * 75 + f).saturating_sub(150));
                self.ack(FIRST_RESPONSE);
            }
            0x03 => {
                let track = self.params.pop_front();
                if let (Some(t), Some(d)) = (track, self.disc.as_ref()) {
                    let t = from_bcd(t);
                    if let Some(tr) = d.tracks.iter().find(|x| x.number as u32 == t) {
                        self.setloc = Some(tr.start);
                    }
                }
                if let Some(l) = self.setloc.take() {
                    self.lba = l;
                }
                self.play_track = self.disc.as_ref().map(|d| d.track_of(self.lba).number).unwrap_or(1);
                self.report_count = 0;
                self.drive = Drive::Playing;
                self.read_timer = self.period();
                self.ack(FIRST_RESPONSE);
            }
            // Forward / Backward: skip a little while playing.
            0x04 | 0x05 => {
                if cmd == 0x04 {
                    self.lba += 75;
                } else {
                    self.lba = self.lba.saturating_sub(75);
                }
                self.ack(FIRST_RESPONSE);
            }
            0x06 | 0x1B => {
                if no_disc {
                    return self.error(0x80);
                }
                if let Some(l) = self.setloc.take() {
                    self.lba = l;
                }
                self.ack(FIRST_RESPONSE);
                self.drive = Drive::Reading;
                self.sector_ready = false;
                self.read_timer = self.period() + 20_000;
            }
            0x07 => {
                self.ack(FIRST_RESPONSE);
                let s = self.stat();
                self.push(SECOND_RESPONSE, 2, vec![s]);
            }
            0x08 => {
                self.drive = Drive::Idle;
                self.ack(FIRST_RESPONSE);
                self.push(SECOND_RESPONSE, 2, vec![0x00]);
            }
            0x09 => {
                self.ack(FIRST_RESPONSE);
                let was = self.drive != Drive::Idle;
                self.drive = Drive::Idle;
                let s = self.stat();
                self.push(if was { 2_000_000 / 4 } else { 7_000 }, 2, vec![s]);
            }
            0x0A => {
                self.mode = 0;
                self.drive = Drive::Idle;
                self.queue.clear();
                self.ack(FIRST_RESPONSE);
                let s = self.stat();
                self.push(SECOND_RESPONSE, 2, vec![s]);
            }
            0x0B => {
                self.muted = true;
                self.ack(FIRST_RESPONSE)
            }
            0x0C => {
                self.muted = false;
                self.ack(FIRST_RESPONSE)
            }
            0x0D => {
                self.filter = (self.param(), self.param());
                self.ack(FIRST_RESPONSE);
            }
            0x0E => {
                self.mode = self.param();
                self.ack(FIRST_RESPONSE);
            }
            0x0F => {
                let s = self.stat();
                self.push(FIRST_RESPONSE, 3, vec![s, self.mode, 0, self.filter.0, self.filter.1]);
            }
            0x10 => {
                let h = self.sector[12..20].to_vec();
                self.push(FIRST_RESPONSE, 3, h);
            }
            0x11 => {
                let track = self.disc.as_ref().map(|d| d.track_of(self.lba));
                let (tn, start) = track.map(|t| (t.number as u32, t.start)).unwrap_or((1, 0));
                let (rm, rs, rf) = lba_to_msf(self.lba.saturating_sub(start));
                let (am, as_, af) = lba_to_msf(self.lba + 150);
                self.push(FIRST_RESPONSE, 3, vec![bcd(tn), 1, bcd(rm), bcd(rs), bcd(rf), bcd(am), bcd(as_), bcd(af)]);
            }
            0x12 => {
                if self.param() != 1 {
                    return self.error(0x10);
                }
                self.ack(FIRST_RESPONSE);
                let s = self.stat();
                self.push(SECOND_RESPONSE, 2, vec![s]);
            }
            0x13 => {
                let last = self.disc.as_ref().map(|d| d.tracks.last().map(|t| t.number).unwrap_or(1)).unwrap_or(1);
                let s = self.stat();
                self.push(FIRST_RESPONSE, 3, vec![s, 1, bcd(last as u32)]);
            }
            0x14 => {
                let t = from_bcd(self.param());
                let lba = match self.disc.as_ref() {
                    Some(d) if t == 0 => d.total,
                    Some(d) => d.tracks.iter().find(|x| x.number as u32 == t).map(|x| x.start).unwrap_or(0),
                    None => 0,
                };
                let (m, s, _) = lba_to_msf(lba + 150);
                let st = self.stat();
                self.push(FIRST_RESPONSE, 3, vec![st, bcd(m), bcd(s)]);
            }
            0x15 | 0x16 => {
                if let Some(l) = self.setloc.take() {
                    self.lba = l;
                }
                self.drive = Drive::Idle;
                self.ack(FIRST_RESPONSE);
                let s = self.stat();
                self.push(SECOND_RESPONSE, 2, vec![s]);
            }
            0x19 => {
                let sub = self.param();
                let r = if sub == 0x20 { vec![0x94, 0x09, 0x19, 0xC0] } else { vec![self.stat()] };
                self.push(FIRST_RESPONSE, 3, r);
            }
            0x1A => {
                self.ack(FIRST_RESPONSE);
                if no_disc {
                    self.push(SECOND_RESPONSE, 5, vec![0x08, 0x40, 0, 0, 0, 0, 0, 0]);
                } else {
                    let r = self.region;
                    self.push(SECOND_RESPONSE, 2, vec![0x02, 0x00, 0x20, 0x00, r[0], r[1], r[2], r[3]]);
                }
            }
            // GetQ: subchannel Q of a sector; answer with the current position.
            0x1D => {
                let (am, as_, af) = lba_to_msf(self.lba + 150);
                self.ack(FIRST_RESPONSE);
                self.push(SECOND_RESPONSE, 2, vec![0x01, 0x01, 0x01, 0, 0, 0, 0, bcd(am), bcd(as_), bcd(af)]);
            }
            0x1E => {
                self.ack(FIRST_RESPONSE);
                let s = self.stat();
                self.push(SECOND_RESPONSE * 4, 2, vec![s]);
            }
            _ => self.error(0x40),
        }
        self.params.clear();
    }

    fn error(&mut self, code: u8) {
        let s = self.stat() | 1;
        self.push(FIRST_RESPONSE, 5, vec![s, code]);
        self.params.clear();
    }

    // ------------------------------------------------------------------ timing

    /// Advances by `cycles`; returns true when an interrupt should be raised.
    pub fn tick(&mut self, cycles: u32) -> bool {
        let mut irq = false;
        if self.drive != Drive::Idle {
            self.read_timer -= cycles as i32;
            while self.read_timer <= 0 {
                self.read_timer += self.period();
                self.read_next();
            }
        }
        if let Some(p) = self.queue.front_mut() {
            p.delay -= cycles as i32;
            if p.delay <= 0 && self.iflag == 0 {
                let p = self.queue.pop_front().unwrap();
                self.resp = p.resp.into();
                self.iflag = p.int;
                if p.int == 1 {
                    self.stats[4] += 1;
                    self.trace_request = true;
                } else {

                }
                self.stats[6] = self.queue.len() as u64;
                irq = self.iflag & self.ie != 0;
            }
        }
        irq
    }

    fn read_next(&mut self) {
        let Some(disc) = self.disc.as_mut() else { return };
        disc.read(self.lba, &mut self.sector);
        let audio_track = disc.track_of(self.lba).audio;
        self.lba += 1;
        if self.drive == Drive::Playing || audio_track {
            if !self.muted {
                for s in self.sector.chunks_exact(4) {
                    self.audio.push_back((i16::from_le_bytes([s[0], s[1]]), i16::from_le_bytes([s[2], s[3]])));
                }
                self.trim_audio();
            }
            if self.drive == Drive::Playing {
                self.play_events();
                return;
            }
        }
        let sub = &self.sector[16..20];
        let (file, chan, submode, coding) = (sub[0], sub[1], sub[2], sub[3]);
        if self.mode & 0x40 != 0 && submode & 0x04 != 0 {
            let pass = self.mode & 0x08 == 0 || (file == self.filter.0 && chan == self.filter.1);
            self.stats[1] += 1;
            if pass && !self.muted {
                self.decode_xa(coding);
            }
            return; // XA audio sectors never reach the data FIFO
        }
        if self.mode & 0x08 != 0 && submode & 0x40 != 0 && (file != self.filter.0 || chan != self.filter.1) {
            return;
        }
        self.sector_ready = true;
        self.stats[0] += 1;
        // Drop a stale undelivered data interrupt (the game fell behind).
        if self.queue.len() > 1 && self.queue.back().map(|p| p.int == 1).unwrap_or(false) {
            self.queue.pop_back();
        }
        let s = self.stat();
        self.push(0, 1, vec![s]);
    }

    /// CD-DA: auto-pause at the end of a track (INT4) and position reports (INT1).
    fn play_events(&mut self) {
        let Some(disc) = self.disc.as_ref() else { return };
        let lba = self.lba;
        let t = disc.track_of(lba);
        if lba >= disc.total || (self.mode & 0x02 != 0 && t.number != self.play_track) {
            self.drive = Drive::Idle;
            let s = self.stat();
            self.push(0, 4, vec![s]);
            return;
        }
        if self.mode & 0x04 == 0 {
            return;
        }
        self.report_count += 1;
        if self.report_count % 10 != 0 {
            return;
        }
        // Alternates absolute and relative (bit 7 of seconds) positions, like the real drive.
        let abs = self.report_count % 20 == 10;
        let (m, sec, f) = if abs { lba_to_msf(lba + 150) } else { lba_to_msf(lba.saturating_sub(t.start)) };
        let s = self.stat();
        let sec = bcd(sec) | if abs { 0 } else { 0x80 };
        if !(self.queue.back().map(|p| p.int == 1).unwrap_or(false)) {
            self.push(0, 1, vec![s, bcd(t.number as u32), 1, bcd(m), sec, bcd(f), 0, 0]);
        }
    }

    fn trim_audio(&mut self) {
        while self.audio.len() > 44_100 / 2 {
            self.audio.pop_front();
        }
    }

    fn decode_xa(&mut self, coding: u8) {
        let stereo = coding & 1 != 0;
        let rate = if coding & 4 != 0 { 18_900 } else { 37_800 };
        let eight = coding & 0x10 != 0;
        const K0: [i32; 4] = [0, 60, 115, 98];
        const K1: [i32; 4] = [0, 0, -52, -55];
        let mut left = Vec::with_capacity(18 * 224);
        let mut right = Vec::with_capacity(18 * 224);
        for g in 0..18 {
            let grp = &self.sector[24 + g * 128..24 + (g + 1) * 128];
            let units = if eight { 4 } else { 8 };
            for u in 0..units {
                let hdr = grp[4 + u];
                let shift = (hdr & 0xF) as u32;
                let f = ((hdr >> 4) & 3) as usize;
                let ch = if stereo { u & 1 } else { 0 };
                for j in 0..28 {
                    let byte = grp[16 + j * 4 + if eight { u } else { u / 2 }];
                    let raw = if eight {
                        (byte as i8 as i32) << 8
                    } else {
                        let n = if u & 1 != 0 { byte >> 4 } else { byte & 0xF };
                        ((n as i32) << 28) >> 16
                    };
                    let h = &mut self.xa_hist[ch];
                    let s = ((raw >> shift.min(12)) + ((h[0] * K0[f] + h[1] * K1[f] + 32) >> 6)).clamp(-32768, 32767);
                    h[1] = h[0];
                    h[0] = s;
                    if ch == 0 {
                        left.push(s as i16);
                    } else {
                        right.push(s as i16);
                    }
                }
            }
        }
        let n = left.len();
        for i in 0..n {
            let l = left[i];
            let r = if stereo { *right.get(i).unwrap_or(&l) } else { l };
            self.xa_phase += 44_100;
            while self.xa_phase >= rate {
                self.xa_phase -= rate;
                self.audio.push_back((l, r));
            }
        }
        self.trim_audio();
    }
}
