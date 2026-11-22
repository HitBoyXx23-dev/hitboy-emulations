//! MOS 6532 RIOT: 128 bytes of RAM, the interval timer and the two I/O ports
//! (joysticks on port A, console switches on port B).

#[derive(state::Stateful)]
pub struct Riot {
    pub ram: [u8; 128],
    timer: u8,
    interval: u32,
    sub: u32,
    /// After the timer passes zero it counts down every cycle.
    underflowed: bool,
    timer_flag: bool,
    /// Joystick lines, active low: P0 in bits 7-4 (right, left, down, up), P1 in bits 3-0.
    pub swcha: u8,
    /// Console switches: bit 0 reset, 1 select (active low), 3 colour, 6/7 difficulty.
    pub swchb: u8,
    swacnt: u8,
    swbcnt: u8,
}

impl Default for Riot {
    fn default() -> Self {
        Self::new()
    }
}

impl Riot {
    pub fn new() -> Riot {
        Riot { ram: [0; 128], timer: 0, interval: 1024, sub: 1024, underflowed: false, timer_flag: false, swcha: 0xFF, swchb: 0x0B, swacnt: 0, swbcnt: 0 }
    }

    pub fn tick(&mut self, cycles: u32) {
        for _ in 0..cycles {
            if self.underflowed {
                self.timer = self.timer.wrapping_sub(1);
                continue;
            }
            self.sub -= 1;
            if self.sub == 0 {
                self.sub = self.interval;
                if self.timer == 0 {
                    self.underflowed = true;
                    self.timer_flag = true;
                    self.timer = 0xFF;
                } else {
                    self.timer -= 1;
                }
            }
        }
    }

    /// Registers at $280-$29F (A2 selects timer vs ports).
    pub fn read_io(&mut self, addr: u16) -> u8 {
        match addr & 0x07 {
            0 => self.swcha & !self.swacnt,
            1 => self.swacnt,
            2 => (self.swchb & !self.swbcnt) | 0x34,
            3 => self.swbcnt,
            4 | 6 => {
                self.timer_flag = false;
                self.timer
            }
            _ => (self.timer_flag as u8) << 7,
        }
    }

    pub fn write_io(&mut self, addr: u16, v: u8) {
        if addr & 0x04 != 0 {
            if addr & 0x10 != 0 {
                self.interval = [1, 8, 64, 1024][(addr & 3) as usize];
                self.timer = v;
                self.sub = self.interval;
                self.underflowed = false;
                self.timer_flag = false;
            }
            return;
        }
        match addr & 0x03 {
            1 => self.swacnt = v,
            3 => self.swbcnt = v,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_counts_by_interval_then_fast() {
        let mut r = Riot::new();
        r.write_io(0x296, 2); // TIM64T = 2
        r.tick(64);
        assert_eq!(r.read_io(0x284), 1);
        r.tick(64);
        assert_eq!(r.read_io(0x284), 0);
        r.tick(64);
        assert_eq!(r.read_io(0x285) & 0x80, 0x80, "underflow flag");
        assert_eq!(r.read_io(0x284), 0xFF);
        r.tick(3);
        assert_eq!(r.read_io(0x284), 0xFC, "one per cycle after underflow");
    }
}
