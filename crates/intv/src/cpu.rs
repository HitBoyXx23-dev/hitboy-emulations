//! General Instrument CP1610: 8 x 16-bit registers (R6 = stack, R7 = program counter),
//! 10-bit instruction words ("decles") and the S/Z/O/C flags.

pub trait Bus {
    fn read(&mut self, addr: u16) -> u16;
    fn write(&mut self, addr: u16, v: u16);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub s: bool,
    pub z: bool,
    pub o: bool,
    pub c: bool,
}

pub struct Cpu {
    pub r: [u16; 8],
    pub f: Flags,
    /// Interrupts enabled.
    pub i: bool,
    /// SDBD prefix active for the next instruction.
    d: bool,
    pub halted: bool,
    /// An interrupt is waiting (the STIC's vertical blank).
    pub irq: bool,
    /// Instructions after which interrupts can't be taken (shifts, SDBD, EIS/DIS...).
    no_irq_next: bool,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Cpu {
        Cpu { r: [0; 8], f: Flags::default(), i: false, d: false, halted: false, irq: false, no_irq_next: false }
    }

    pub fn reset(&mut self) {
        *self = Cpu::new();
        self.r[7] = 0x1000;
    }

    fn fetch<B: Bus>(&mut self, bus: &mut B) -> u16 {
        let v = bus.read(self.r[7]);
        self.r[7] = self.r[7].wrapping_add(1);
        v
    }

    fn sz(&mut self, v: u16) {
        self.f.s = v & 0x8000 != 0;
        self.f.z = v == 0;
    }

    /// a + b + cin with all four flags.
    fn add(&mut self, a: u16, b: u16, cin: u16) -> u16 {
        let wide = a as u32 + b as u32 + cin as u32;
        let r = wide as u16;
        self.f.c = wide > 0xFFFF;
        self.f.o = (!(a ^ b) & (a ^ r)) & 0x8000 != 0;
        self.sz(r);
        r
    }

    fn sub(&mut self, a: u16, b: u16) -> u16 {
        self.add(a, !b, 1)
    }

    fn gswd(&self) -> u16 {
        let n = (self.f.s as u16) << 3 | (self.f.z as u16) << 2 | (self.f.o as u16) << 1 | self.f.c as u16;
        n << 12 | n << 4
    }

    fn cond(&self, c: u16) -> bool {
        let f = self.f;
        let base = match c & 7 {
            0 => true,
            1 => f.c,
            2 => f.o,
            3 => !f.s,
            4 => f.z,
            5 => f.s != f.o,
            6 => f.z || (f.s != f.o),
            _ => f.c != f.s,
        };
        base != (c & 8 != 0)
    }

    /// Takes a pending interrupt: push R7, jump to the EXEC's handler at $1004.
    fn interrupt<B: Bus>(&mut self, bus: &mut B) {
        bus.write(self.r[6], self.r[7]);
        self.r[6] = self.r[6].wrapping_add(1);
        self.r[7] = 0x1004;
        self.irq = false;
    }

    /// Executes one instruction; returns CPU cycles.
    pub fn step<B: Bus>(&mut self, bus: &mut B) -> u32 {
        if self.irq && self.i && !self.no_irq_next && !self.d {
            self.interrupt(bus);
            self.halted = false;
            return 28;
        }
        if self.halted {
            return 4;
        }
        let d = std::mem::take(&mut self.d);
        self.no_irq_next = false;
        let op = self.fetch(bus) & 0x3FF;
        match op {
            0x000 => {
                self.halted = true;
                4
            }
            0x001 => {
                self.d = true;
                self.no_irq_next = true;
                4
            }
            0x002 | 0x003 => {
                self.i = op == 0x002;
                self.no_irq_next = true;
                4
            }
            0x004 => {
                let w1 = self.fetch(bus) & 0x3FF;
                let w2 = self.fetch(bus) & 0x3FF;
                let target = ((w1 >> 2) & 0x3F) << 10 | w2;
                let rr = (w1 >> 8) & 3;
                if rr != 3 {
                    self.r[4 + rr as usize] = self.r[7];
                }
                match w1 & 3 {
                    1 => self.i = true,
                    2 => self.i = false,
                    _ => {}
                }
                self.r[7] = target;
                12
            }
            0x005 => 4, // TCI
            0x006 => {
                self.f.c = false;
                4
            }
            0x007 => {
                self.f.c = true;
                4
            }
            0x008..=0x03F => self.single_reg(op),
            0x040..=0x07F => {
                self.no_irq_next = true;
                self.shift(op)
            }
            0x080..=0x1FF => self.reg_reg(op),
            0x200..=0x23F => {
                let disp = self.fetch(bus);
                if op & 0x10 == 0 && self.cond(op & 0xF) {
                    self.r[7] = if op & 0x20 != 0 { self.r[7].wrapping_add(!disp) } else { self.r[7].wrapping_add(disp) };
                    9
                } else {
                    7
                }
            }
            _ => self.memory(bus, op, d),
        }
    }

    fn single_reg(&mut self, op: u16) -> u32 {
        let n = (op & 7) as usize;
        match op & 0x3F8 {
            0x008 => {
                self.r[n] = self.r[n].wrapping_add(1);
                self.sz(self.r[n]);
            }
            0x010 => {
                self.r[n] = self.r[n].wrapping_sub(1);
                self.sz(self.r[n]);
            }
            0x018 => {
                self.r[n] = !self.r[n];
                self.sz(self.r[n]);
            }
            0x020 => self.r[n] = self.sub(0, self.r[n]),
            0x028 => self.r[n] = self.add(self.r[n], 0, self.f.c as u16),
            0x030 => match op & 7 {
                0..=3 => self.r[n & 3] = self.gswd(),
                // NOP / SIN
                _ => {}
            },
            _ => {
                // 0x038 RSWD
                let v = self.r[n];
                self.f = Flags { s: v & 0x80 != 0, z: v & 0x40 != 0, o: v & 0x20 != 0, c: v & 0x10 != 0 };
            }
        }
        if n >= 6 && matches!(op & 0x3F8, 0x008 | 0x010) {
            7
        } else {
            6
        }
    }

    fn shift(&mut self, op: u16) -> u32 {
        let n = (op & 3) as usize;
        let two = op & 4 != 0;
        let v = self.r[n];
        let (c, o) = (self.f.c as u16, self.f.o as u16);
        let r = match (op >> 3) & 7 {
            0 => {
                // SWAP (",2" duplicates the low byte)
                let r = if two { (v & 0xFF) << 8 | (v & 0xFF) } else { v.rotate_left(8) };
                self.f.s = r & 0x80 != 0;
                self.f.z = r == 0;
                self.r[n] = r;
                return if two { 8 } else { 6 };
            }
            1 => {
                if two {
                    v << 2
                } else {
                    v << 1
                }
            }
            2 => {
                // RLC through carry (and overflow for ,2)
                if two {
                    self.f.c = v & 0x8000 != 0;
                    self.f.o = v & 0x4000 != 0;
                    v << 2 | c << 1 | o
                } else {
                    self.f.c = v & 0x8000 != 0;
                    v << 1 | c
                }
            }
            3 => {
                // SLLC: shifted-out bits go to C (and O)
                if two {
                    self.f.c = v & 0x8000 != 0;
                    self.f.o = v & 0x4000 != 0;
                    v << 2
                } else {
                    self.f.c = v & 0x8000 != 0;
                    v << 1
                }
            }
            4 => {
                if two {
                    v >> 2
                } else {
                    v >> 1
                }
            }
            5 => {
                let s = v as i16;
                (if two { s >> 2 } else { s >> 1 }) as u16
            }
            6 => {
                // RRC through carry (and overflow)
                if two {
                    self.f.c = v & 2 != 0;
                    self.f.o = v & 1 != 0;
                    v >> 2 | o << 14 | c << 15
                } else {
                    self.f.c = v & 1 != 0;
                    v >> 1 | c << 15
                }
            }
            _ => {
                // SARC: arithmetic right, shifted-out bits to C (and O)
                let s = v as i16;
                if two {
                    self.f.c = v & 2 != 0;
                    self.f.o = v & 1 != 0;
                    (s >> 2) as u16
                } else {
                    self.f.c = v & 1 != 0;
                    (s >> 1) as u16
                }
            }
        };
        self.r[n] = r;
        self.f.z = r == 0;
        // Right shifts report the sign of the low byte's bit 7; left shifts bit 15.
        self.f.s = if (op >> 3) & 7 >= 4 { r & 0x80 != 0 } else { r & 0x8000 != 0 };
        if two {
            8
        } else {
            6
        }
    }

    fn reg_reg(&mut self, op: u16) -> u32 {
        let (s, d) = (((op >> 3) & 7) as usize, (op & 7) as usize);
        let (a, b) = (self.r[d], self.r[s]);
        match op & 0x1C0 {
            0x080 => {
                self.r[d] = b;
                self.sz(b);
            }
            0x0C0 => self.r[d] = self.add(a, b, 0),
            0x100 => self.r[d] = self.sub(a, b),
            0x140 => {
                self.sub(a, b);
            }
            0x180 => {
                self.r[d] = a & b;
                self.sz(self.r[d]);
            }
            _ => {
                self.r[d] = a ^ b;
                self.sz(self.r[d]);
            }
        }
        if d >= 6 {
            7
        } else {
            6
        }
    }

    /// Operand read for memory-class instructions (with SDBD double-byte reads).
    fn operand<B: Bus>(&mut self, bus: &mut B, m: usize, d: bool) -> (u16, u32) {
        let read_ptr = |cpu: &mut Cpu, bus: &mut B, m: usize| -> u16 {
            let a = cpu.r[m];
            let v = bus.read(a);
            if m >= 4 {
                cpu.r[m] = a.wrapping_add(1);
            }
            v
        };
        match m {
            0 => {
                let a = self.fetch(bus);
                (bus.read(a), 10)
            }
            6 => {
                self.r[6] = self.r[6].wrapping_sub(1);
                (bus.read(self.r[6]), 11)
            }
            _ if d => {
                let lo = read_ptr(self, bus, m) & 0xFF;
                let hi = read_ptr(self, bus, m) & 0xFF;
                (hi << 8 | lo, if m == 7 { 10 } else { 10 })
            }
            _ => (read_ptr(self, bus, m), 8),
        }
    }

    fn memory<B: Bus>(&mut self, bus: &mut B, op: u16, d: bool) -> u32 {
        let (m, r) = (((op >> 3) & 7) as usize, (op & 7) as usize);
        let kind = (op >> 6) & 7;
        if kind == 1 {
            // MVO
            let v = self.r[r];
            let a = match m {
                0 => self.fetch(bus),
                _ => {
                    let a = self.r[m];
                    if m >= 4 {
                        self.r[m] = a.wrapping_add(1);
                    }
                    a
                }
            };
            bus.write(a, v);
            self.no_irq_next = true;
            return if m == 0 { 11 } else { 9 };
        }
        let (v, cycles) = self.operand(bus, m, d);
        let a = self.r[r];
        match kind {
            2 => self.r[r] = v,
            3 => self.r[r] = self.add(a, v, 0),
            4 => self.r[r] = self.sub(a, v),
            5 => {
                self.sub(a, v);
            }
            6 => {
                self.r[r] = a & v;
                self.sz(self.r[r]);
            }
            _ => {
                self.r[r] = a ^ v;
                self.sz(self.r[r]);
            }
        }
        cycles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram(Vec<u16>);
    impl Bus for Ram {
        fn read(&mut self, a: u16) -> u16 {
            self.0[a as usize]
        }
        fn write(&mut self, a: u16, v: u16) {
            self.0[a as usize] = v;
        }
    }

    fn run(prog: &[u16], steps: usize) -> (Cpu, Ram) {
        let mut ram = Ram(vec![0; 0x10000]);
        ram.0[0x5000..0x5000 + prog.len()].copy_from_slice(prog);
        let mut c = Cpu::new();
        c.r[7] = 0x5000;
        c.r[6] = 0x2F0;
        for _ in 0..steps {
            c.step(&mut ram);
        }
        (c, ram)
    }

    #[test]
    fn immediate_arithmetic_and_flags() {
        // MVII #5,R0 ; MVII #7,R1 ; SUBR R1,R0 ; ADDI #2,R0 ; CMPR R0,R0
        let (c, _) = run(&[0x2B8, 5, 0x2B9, 7, 0x108, 0x2F8, 2, 0x140], 5);
        assert_eq!(c.r[0], 0);
        assert!(c.f.z && c.f.c, "equal compare: Z and no borrow");
        let (c, _) = run(&[0x2B8, 5, 0x2B9, 7, 0x108], 3);
        assert_eq!(c.r[0], 0xFFFE);
        assert!(c.f.s && !c.f.c, "5 - 7 borrows");
    }

    #[test]
    fn loop_with_backward_branch() {
        // MVII #3,R1 ; loop: DECR R1 ; BNEQ loop (backward) ; MVO R1,$200
        // The branch word sits at $5003; R7 after the displacement is $5005 and the target
        // $5002 = $5005 + !disp  ->  disp = 2.
        let (c, ram) = run(&[0x2B9, 3, 0x011, 0x22C, 2, 0x241, 0x200], 1 + 2 * 3 + 1);
        assert_eq!(c.r[1], 0);
        assert_eq!(ram.0[0x200], 0);
        assert_eq!(c.r[7], 0x5007);
    }

    #[test]
    fn jsr_and_stack() {
        // JSR R5,$5010 ; at $5010: PSHR R5 (MVO@ R5,R6) ; PULR R7 (MVI@ R6,R7)
        let mut prog = vec![0x004, 0x150, 0x010];
        prog.resize(0x10, 0);
        prog.extend([0x275, 0x2B7]);
        let (c, ram) = run(&prog, 3);
        assert_eq!(ram.0[0x2F0], 0x5003, "return address pushed");
        assert_eq!(c.r[7], 0x5003, "returned");
        assert_eq!(c.r[6], 0x2F0);
    }

    #[test]
    fn sdbd_reads_two_bytes() {
        // SDBD ; MVII #$1234,R0 (two decles, low byte first)
        let (c, _) = run(&[0x001, 0x2B8, 0x34, 0x12], 2);
        assert_eq!(c.r[0], 0x1234);
    }

    #[test]
    fn shifts_and_gswd() {
        // MVII #$8001,R0 ; RRC R0 (C=0) -> $4000, C=1 ; GSWD R1
        let (c, _) = run(&[0x001, 0x2B8, 0x01, 0x80, 0x070, 0x031], 4);
        assert_eq!(c.r[0], 0x4000);
        assert!(c.f.c);
        assert_eq!(c.r[1], 0x1010, "only C set, mirrored in both nibbles");
    }
}
