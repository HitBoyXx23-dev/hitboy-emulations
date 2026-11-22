//! Seiko S-3511A real-time clock on the cartridge's GPIO port ($80000C4 data, $C6
//! direction, $C8 control): Pokémon Ruby/Sapphire/Emerald, Boktai and others. Time comes
//! from the host clock plus an offset the game can set.

#[derive(Clone, Debug, Default, state::Stateful)]
pub struct Rtc {
    #[state(skip)]
    pub present: bool,
    /// GPIO pin values, direction (1 = output from the GBA) and read-enable.
    data: u8,
    dir: u8,
    pub readable: bool,
    /// Serial state.
    cmd: Option<u8>,
    shift: u8,
    bits: u8,
    out: Vec<u8>,
    out_bit: usize,
    input: Vec<u8>,
    need: usize,
    control: u8,
    /// Seconds added to the host clock (set when the game writes the time). The clock keeps
    /// following real time across a state load.
    #[state(skip)]
    pub offset: i64,
}

fn now() -> i64 {
    state::wall_clock_secs()
}

fn bcd(v: i64) -> u8 {
    ((v / 10) << 4 | (v % 10)) as u8
}
fn from_bcd(v: u8) -> i64 {
    ((v >> 4) * 10 + (v & 0xF)) as i64
}

/// Days since 1970-01-01 to (year, month, day).
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}
/// (year, month, day) to days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl Rtc {
    pub fn for_rom(rom: &[u8]) -> Rtc {
        let present = rom.windows(8).any(|w| w == b"SIIRTC_V");
        Rtc { present, control: 0x40, ..Default::default() }
    }

    /// Year/month/day/weekday/hour/minute/second as BCD.
    fn datetime(&self) -> [u8; 7] {
        let t = now() + self.offset;
        let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
        let (y, m, d) = civil(days);
        let hour = secs / 3600;
        let h = if self.control & 0x40 != 0 { bcd(hour) } else { bcd(hour % 12) };
        let pm = if hour >= 12 { 0x80 } else { 0 };
        [bcd(y % 100), bcd(m), bcd(d), bcd((days + 4).rem_euclid(7)), h | pm, bcd(secs / 60 % 60), bcd(secs % 60)]
    }

    fn set_datetime(&mut self, b: &[u8]) {
        let (y, m, d, h, mi, s) = (2000 + from_bcd(b[0]), from_bcd(b[1]).max(1), from_bcd(b[2]).max(1), from_bcd(b[4] & 0x3F), from_bcd(b[5]), from_bcd(b[6]));
        let t = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s;
        self.offset = t - now();
    }

    pub fn read(&self, reg: u32) -> u16 {
        match reg {
            0xC4 => (self.data & !self.dir) as u16 | (self.data & self.dir) as u16,
            0xC6 => self.dir as u16,
            _ => self.readable as u16,
        }
    }

    pub fn write(&mut self, reg: u32, v: u16) {
        match reg {
            0xC4 => self.pins(v as u8 & 0xF),
            0xC6 => self.dir = v as u8 & 0xF,
            0xC8 => self.readable = v & 1 != 0,
            _ => {}
        }
    }

    /// Pin 0 = SCK, 1 = SIO, 2 = CS.
    fn pins(&mut self, v: u8) {
        let old = self.data;
        // Bits the GBA drives come from it; SIO reads back what the chip drives.
        self.data = (v & self.dir) | (self.data & !self.dir);
        let cs = self.data & 4 != 0;
        if !cs {
            self.cmd = None;
            self.bits = 0;
            self.shift = 0;
            return;
        }
        let rising = old & 1 == 0 && self.data & 1 != 0;
        if !rising {
            return;
        }
        let sio = (v >> 1) & 1;
        if self.dir & 2 == 0 {
            // Chip -> GBA: present the next output bit.
            let byte = self.out.get(self.out_bit / 8).copied().unwrap_or(0xFF);
            let bit = (byte >> (self.out_bit % 8)) & 1;
            self.out_bit += 1;
            self.data = (self.data & !2) | bit << 1;
            return;
        }
        // GBA -> chip: bits arrive LSB first.
        self.shift |= sio << self.bits;
        self.bits += 1;
        if self.bits < 8 {
            return;
        }
        let byte = self.shift;
        self.bits = 0;
        self.shift = 0;
        match self.cmd {
            None => {
                // The command byte is 0110 CCC R; some games send it bit-reversed.
                let c = if byte >> 4 == 6 { byte } else { byte.reverse_bits() };
                self.cmd = Some(c);
                let kind = (c >> 1) & 7;
                let read = c & 1 != 0;
                self.out.clear();
                self.out_bit = 0;
                self.input.clear();
                match (kind, read) {
                    (0, _) => {
                        self.control = 0x40;
                        self.offset = 0;
                    }
                    (2, true) => self.out = self.datetime().to_vec(),
                    (6, true) => self.out = self.datetime()[4..].to_vec(),
                    (4, true) => self.out = vec![self.control],
                    (2, false) => self.need = 7,
                    (6, false) => self.need = 3,
                    (4, false) => self.need = 1,
                    _ => {}
                }
            }
            Some(c) => {
                self.input.push(byte);
                if self.input.len() == self.need {
                    match (c >> 1) & 7 {
                        2 => {
                            let b = self.input.clone();
                            self.set_datetime(&b);
                        }
                        6 => {
                            let mut dt = self.datetime();
                            dt[4..].copy_from_slice(&self.input);
                            self.set_datetime(&dt);
                        }
                        4 => self.control = self.input[0],
                        _ => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(r: &mut Rtc, byte: u8) {
        for i in 0..8 {
            let b = ((byte >> i) & 1) as u16;
            r.write(0xC4, 4 | b << 1);
            r.write(0xC4, 4 | b << 1 | 1);
        }
    }
    fn recv(r: &mut Rtc) -> u8 {
        let mut v = 0;
        for i in 0..8 {
            r.write(0xC4, 4);
            r.write(0xC4, 5);
            v |= ((r.read(0xC4) >> 1) & 1) << i;
        }
        v as u8
    }

    #[test]
    fn set_and_read_the_date() {
        let mut r = Rtc { present: true, control: 0x40, ..Default::default() };
        r.write(0xC6, 7);
        r.write(0xC4, 4); // CS high
        send(&mut r, 0x64); // write date/time
        for b in [0x26, 0x09, 0x29, 0x02, 0x13, 0x45, 0x00] {
            send(&mut r, b);
        }
        r.write(0xC4, 0);
        r.write(0xC4, 4);
        send(&mut r, 0x65); // read date/time
        r.write(0xC6, 5); // SIO becomes an input
        let got: Vec<u8> = (0..7).map(|_| recv(&mut r)).collect();
        assert_eq!(&got[..3], &[0x26, 0x09, 0x29]);
        assert_eq!(got[4], 0x13 | 0x80);
        assert_eq!(got[5], 0x45);
    }
}
