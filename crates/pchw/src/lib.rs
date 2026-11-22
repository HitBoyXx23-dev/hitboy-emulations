//! PC support chips shared by our x86 machines (the PC and the Xbox MCPX): two 8259
//! interrupt controllers, the 8254 timer, and the ACPI power-management timer.

pub mod kbc;
pub use kbc::Kbc;

/// One 8259A programmable interrupt controller.
#[derive(Clone, Debug, Default)]
pub struct Pic {
    pub irr: u8,
    pub isr: u8,
    pub imr: u8,
    pub base: u8,
    init: u8,
    need_icw4: bool,
    read_isr: bool,
    auto_eoi: bool,
}

impl Pic {
    pub fn write(&mut self, cmd: bool, v: u8) {
        if cmd {
            if v & 0x10 != 0 {
                // ICW1: start initialisation.
                self.init = 2;
                self.need_icw4 = v & 1 != 0;
                self.imr = 0;
                self.isr = 0;
                self.irr = 0;
            } else if v & 0x18 == 0 {
                // OCW2: end of interrupt.
                if v & 0x60 == 0x60 {
                    self.isr &= !(1 << (v & 7));
                } else if v & 0x20 != 0 && self.isr != 0 {
                    self.isr &= self.isr - 1; // clear the highest-priority bit
                }
            } else if v & 0x18 == 0x08 && v & 2 != 0 {
                self.read_isr = v & 1 != 0;
            }
            return;
        }
        match self.init {
            2 => {
                self.base = v & 0xF8;
                self.init = 3;
            }
            3 => self.init = if self.need_icw4 { 4 } else { 0 },
            4 => {
                self.auto_eoi = v & 2 != 0;
                self.init = 0;
            }
            _ => self.imr = v,
        }
    }
    pub fn read(&self, cmd: bool) -> u8 {
        match (cmd, self.read_isr) {
            (false, _) => self.imr,
            (true, true) => self.isr,
            (true, false) => self.irr,
        }
    }
    /// Highest pending unmasked line that outranks everything in service.
    pub fn pending(&self) -> Option<u8> {
        let p = self.irr & !self.imr;
        if p == 0 {
            return None;
        }
        let line = p.trailing_zeros() as u8;
        let busy = if self.isr == 0 { 8 } else { self.isr.trailing_zeros() as u8 };
        (line < busy).then_some(line)
    }
    pub fn ack(&mut self, line: u8) {
        self.irr &= !(1 << line);
        if !self.auto_eoi {
            self.isr |= 1 << line;
        }
    }
}

/// Master + slave pair (slave cascaded on master IRQ 2).
#[derive(Clone, Debug, Default)]
pub struct Pics {
    pub m: Pic,
    pub s: Pic,
}

impl Pics {
    pub fn raise(&mut self, irq: u8) {
        if irq < 8 {
            self.m.irr |= 1 << irq;
        } else {
            self.s.irr |= 1 << (irq - 8);
            self.m.irr |= 1 << 2;
        }
    }
    /// Acknowledges the next interrupt and returns its CPU vector.
    pub fn next_vector(&mut self) -> Option<u8> {
        let line = self.m.pending()?;
        if line == 2 {
            if let Some(sl) = self.s.pending() {
                self.m.ack(2);
                self.s.ack(sl);
                if self.s.irr == 0 {
                    self.m.irr &= !(1 << 2);
                }
                return Some(self.s.base + sl);
            }
            self.m.irr &= !(1 << 2);
            return None;
        }
        self.m.ack(line);
        Some(self.m.base + line)
    }
}

/// One 8254 counter.
#[derive(Clone, Debug, Default)]
pub struct Channel {
    pub reload: u16,
    /// Current count (1..=0x10000 while running).
    pub count: u32,
    pub mode: u8,
    /// Output pin level (channel 2 feeds port 0x61 bit 5).
    pub out: bool,
    pub gate: bool,
    pub running: bool,
    access: u8,
    latch: Option<u16>,
    lo_next: bool,
    write_lo: Option<u8>,
}

impl Channel {
    fn period(&self) -> u64 {
        if self.reload == 0 {
            0x10000
        } else {
            self.reload as u64
        }
    }

    fn load(&mut self, r: u16) {
        self.reload = r;
        self.count = self.period() as u32;
        self.running = true;
        // One-shot modes (0, 1, 4, 5) signal terminal count with a rising edge; the
        // periodic modes start high.
        self.out = !matches!(self.mode, 0 | 1 | 4 | 5);
    }

    /// Advances by `ticks` clocks; returns rising edges of the output (interrupts for IRQ 0).
    fn tick(&mut self, mut ticks: u64) -> u32 {
        if !self.running || !self.gate {
            return 0;
        }
        let period = self.period();
        let mut edges = 0;
        match self.mode {
            0 | 1 | 4 | 5 => {
                // One-shot: the output rises at terminal count, then the counter wraps.
                if ticks >= self.count as u64 {
                    if !self.out {
                        edges = 1;
                    }
                    self.out = true;
                    ticks = (ticks - self.count as u64) % 0x10000;
                    self.count = 0x10000 - ticks as u32;
                } else {
                    self.count -= ticks as u32;
                }
            }
            _ => {
                // Rate generator / square wave: reloads every period.
                while ticks >= self.count as u64 {
                    ticks -= self.count as u64;
                    self.count = period as u32;
                    edges += 1;
                    if edges > 64 {
                        ticks %= period;
                    }
                }
                self.count -= ticks as u32;
                self.out = if self.mode & 3 == 3 { self.count as u64 > period / 2 } else { self.count != 1 };
            }
        }
        edges
    }

    fn write(&mut self, v: u8) {
        match self.access {
            1 => self.load(v as u16),
            2 => self.load((v as u16) << 8),
            _ => match self.write_lo.take() {
                None => {
                    self.write_lo = Some(v);
                    if matches!(self.mode, 0 | 1 | 4 | 5) {
                        self.out = false;
                    }
                }
                Some(lo) => self.load(lo as u16 | (v as u16) << 8),
            },
        }
    }

    fn read(&mut self) -> u8 {
        let v = self.latch.unwrap_or(self.count as u16);
        match self.access {
            1 => {
                self.latch = None;
                v as u8
            }
            2 => {
                self.latch = None;
                (v >> 8) as u8
            }
            _ => {
                self.lo_next = !self.lo_next;
                if self.lo_next {
                    v as u8
                } else {
                    self.latch = None;
                    (v >> 8) as u8
                }
            }
        }
    }
}

/// 8254 programmable interval timer: channel 0 drives IRQ 0, channel 2 the speaker and
/// the gate/output bits of port 0x61. The PC clocks it at 1.193182 MHz, the Xbox at
/// 1.125 MHz.
#[derive(Clone, Debug)]
pub struct Pit {
    pub ch: [Channel; 3],
}

impl Default for Pit {
    fn default() -> Self {
        let mut ch: [Channel; 3] = Default::default();
        ch[0].gate = true;
        ch[1].gate = true;
        Pit { ch }
    }
}

pub const PIT_HZ: u64 = 1_125_000;

impl Pit {
    /// Channel 0's current count (clocks until its next interrupt).
    pub fn count0(&self) -> u32 {
        self.ch[0].count
    }

    pub fn write(&mut self, port: u16, v: u8) {
        match port {
            0x40..=0x42 => self.ch[(port - 0x40) as usize].write(v),
            0x43 => {
                let sel = (v >> 6) as usize;
                if sel == 3 {
                    // Read-back: latch the counts of the selected channels.
                    for i in 0..3 {
                        if v & (2 << i) != 0 && v & 0x20 == 0 {
                            let c = &mut self.ch[i];
                            c.latch = Some(c.count as u16);
                            c.lo_next = false;
                        }
                    }
                    return;
                }
                let c = &mut self.ch[sel];
                match (v >> 4) & 3 {
                    0 => {
                        c.latch = Some(c.count as u16);
                        c.lo_next = false;
                    }
                    a => {
                        c.access = a;
                        c.mode = (v >> 1) & 7;
                        if c.mode > 5 {
                            c.mode -= 4;
                        }
                        c.lo_next = false;
                        c.write_lo = None;
                        c.latch = None;
                        c.running = false;
                        c.out = !matches!(c.mode, 0 | 1 | 4 | 5);
                    }
                }
            }
            _ => {}
        }
    }

    /// Reads a counter port (0x40-0x42).
    pub fn read_port(&mut self, port: u16) -> u8 {
        match port {
            0x40..=0x42 => self.ch[(port - 0x40) as usize].read(),
            _ => 0xFF,
        }
    }

    /// Channel 0 read.
    pub fn read(&mut self) -> u8 {
        self.read_port(0x40)
    }

    /// Channel 2 gate (port 0x61 bit 0). A rising gate restarts the count.
    pub fn set_gate2(&mut self, on: bool) {
        let c = &mut self.ch[2];
        if on && !c.gate && c.running {
            c.count = c.period() as u32;
            if c.mode == 0 {
                c.out = false;
            }
        }
        c.gate = on;
    }

    pub fn out2(&self) -> bool {
        self.ch[2].out
    }

    /// Advances every channel by `ticks` clocks; returns channel 0's interrupt edges.
    pub fn tick(&mut self, ticks: u64) -> u32 {
        self.ch[1].tick(ticks);
        self.ch[2].tick(ticks);
        self.ch[0].tick(ticks)
    }
}

/// ACPI power-management timer frequency (read at I/O 0x8008).
pub const ACPI_HZ: u64 = 3_579_545;

/// The PC's 8254 input clock.
pub const PC_PIT_HZ: u64 = 1_193_182;

/// MC146818 real-time clock and CMOS memory (I/O 0x70 index, 0x71 data). Time is read
/// from the host clock; the BIOS fills in memory size and drive types.
#[derive(Clone, Debug)]
pub struct Cmos {
    pub ram: [u8; 128],
    index: u8,
    /// Seconds since 1970 to report (None = the host's current time).
    pub fixed_time: Option<u64>,
}

impl Default for Cmos {
    fn default() -> Self {
        let mut ram = [0u8; 128];
        ram[0x0A] = 0x26; // 32.768 kHz base, 1024 Hz periodic rate
        ram[0x0B] = 0x02; // 24-hour, BCD
        ram[0x0D] = 0x80; // battery good
        Cmos { ram, index: 0, fixed_time: None }
    }
}

fn bcd(v: u64) -> u8 {
    ((v / 10) << 4 | (v % 10)) as u8
}

impl Cmos {
    pub fn write(&mut self, port: u16, v: u8) {
        if port == 0x70 {
            self.index = v & 0x7F;
        } else if !matches!(self.index, 0x0C | 0x0D) {
            self.ram[self.index as usize] = v;
        }
    }

    pub fn read(&mut self, port: u16) -> u8 {
        if port == 0x70 {
            return self.index;
        }
        let i = self.index;
        if i <= 0x09 || i == 0x32 {
            let t = self.fixed_time.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
            let (days, secs) = (t / 86_400, t % 86_400);
            let (y, m, d) = civil(days as i64);
            let binary = self.ram[0x0B] & 4 != 0;
            let enc = |v: u64| if binary { v as u8 } else { bcd(v) };
            return match i {
                0x00 => enc(secs % 60),
                0x02 => enc(secs / 60 % 60),
                0x04 => enc(secs / 3600),
                0x06 => enc((days + 4) % 7 + 1),
                0x07 => enc(d as u64),
                0x08 => enc(m as u64),
                0x09 => enc(y as u64 % 100),
                0x32 => enc(y as u64 / 100),
                _ => 0, // alarm registers
            };
        }
        let v = self.ram[i as usize];
        if i == 0x0C {
            self.ram[0x0C] = 0; // reading clears the interrupt flags
        }
        v
    }
}

/// Days since 1970-01-01 to (year, month, day).
pub fn civil(z: i64) -> (i64, u32, u32) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pic_init_and_priority() {
        let mut p = Pics::default();
        // Xbox kernel style: master base 0x30, slave 0x38.
        for (cmd, v) in [(true, 0x11), (false, 0x30), (false, 0x04), (false, 0x01)] {
            p.m.write(cmd, v);
        }
        for (cmd, v) in [(true, 0x11), (false, 0x38), (false, 0x02), (false, 0x01)] {
            p.s.write(cmd, v);
        }
        p.raise(3);
        p.raise(0);
        assert_eq!(p.next_vector(), Some(0x30));
        assert_eq!(p.next_vector(), None); // IRQ3 waits for IRQ0's EOI
        p.m.write(true, 0x20);
        assert_eq!(p.next_vector(), Some(0x33));
        p.m.write(true, 0x20);
        p.raise(14);
        assert_eq!(p.next_vector(), Some(0x3E));
    }

    #[test]
    fn cmos_clock_bcd() {
        let mut c = Cmos { fixed_time: Some(1_790_000_000), ..Default::default() }; // 2026-09-21 14:13:20 UTC
        let mut rd = |i| {
            c.write(0x70, i);
            c.read(0x71)
        };
        assert_eq!([rd(0x09), rd(0x08), rd(0x07), rd(0x04), rd(0x02), rd(0x00), rd(0x32)], [0x26, 0x09, 0x21, 0x14, 0x13, 0x20, 0x20]);
    }

    #[test]
    fn pit_channel2_one_shot() {
        let mut t = Pit::default();
        t.set_gate2(true);
        t.write(0x43, 0xB0); // channel 2, lo/hi, mode 0
        t.write(0x42, 100);
        t.write(0x42, 0);
        assert!(!t.out2());
        t.tick(99);
        assert!(!t.out2());
        t.tick(1);
        assert!(t.out2());
        // Latch and read the wrapped count.
        t.write(0x43, 0x80);
        let lo = t.read_port(0x42) as u16;
        let hi = t.read_port(0x42) as u16;
        assert_eq!(lo | hi << 8, 0);
    }

    #[test]
    fn pit_mode4_one_shot_interrupts() {
        // Linux's one-shot clockevent: mode 4, re-armed after every expiry.
        let mut t = Pit::default();
        t.write(0x43, 0x38);
        for _ in 0..3 {
            t.write(0x40, 50);
            t.write(0x40, 0);
            assert_eq!(t.tick(49), 0);
            assert_eq!(t.tick(1), 1);
            assert_eq!(t.tick(1000), 0, "fires once until re-armed");
        }
    }

    #[test]
    fn pit_wraps() {
        let mut t = Pit::default();
        t.write(0x43, 0x34);
        t.write(0x40, 0x65); // 1125 = 0x0465 -> 1 ms
        t.write(0x40, 0x04);
        assert_eq!(t.tick(1124), 0);
        assert_eq!(t.tick(1), 1);
        assert_eq!(t.tick(1125 * 3), 3);
    }
}
