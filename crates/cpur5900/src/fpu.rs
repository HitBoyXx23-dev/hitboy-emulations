//! The R5900's floating-point unit: single precision only, no NaN, infinity or denormals.
//! Overflow clamps to the largest finite value and denormals flush to zero.

use crate::*;

const MAX_BITS: u32 = 0x7F7F_FFFF;

/// Applies the PlayStation 2 float rules to a raw 32-bit pattern.
#[inline]
pub(crate) fn fix_bits(b: u32) -> u32 {
    let exp = (b >> 23) & 0xFF;
    if exp == 0 {
        b & 0x8000_0000
    } else if exp == 0xFF {
        (b & 0x8000_0000) | MAX_BITS
    } else {
        b
    }
}

#[inline]
fn f(b: u32) -> f32 {
    f32::from_bits(fix_bits(b))
}

#[inline]
fn out(v: f32) -> u32 {
    fix_bits(v.to_bits())
}

impl Cpu {
    pub(crate) fn cop1_op(&mut self, op: u32, rs: usize, rt: usize, rd: usize) {
        let fs = rd;
        let ft = rt;
        let fd = ((op >> 6) & 31) as usize;
        match rs {
            0 => {
                let v = self.fpr[fs];
                self.set_reg(rt, v as i32 as i64 as u64);
            }
            2 => {
                let v = match fs {
                    0 => 0x0000_2E00,
                    31 => self.fcr31,
                    _ => 0,
                };
                self.set_reg(rt, v as i32 as i64 as u64);
            }
            4 => self.fpr[fs] = self.lo[rt] as u32,
            6 => {
                if fs == 31 {
                    self.fcr31 = self.lo[rt] as u32 & 0x0183_FFFF;
                }
            }
            8 => {
                // bc1f / bc1t / bc1fl / bc1tl
                let cond = self.fcr31 & (1 << 23) != 0;
                let want = rt & 1 != 0;
                let likely = rt & 2 != 0;
                let target = self.pc.wrapping_add((((op & 0xFFFF) as i16 as i64) << 2) as u32);
                if cond == want {
                    self.branch_to(target);
                } else if likely {
                    self.skip_delay_slot();
                } else {
                    self.mark_delay();
                }
            }
            16 => self.fp_single(op & 63, fd, fs, ft),
            20 => {
                // W format: only cvt.s.w
                if op & 63 == 32 {
                    self.fpr[fd] = out(self.fpr[fs] as i32 as f32);
                } else {
                    self.reserved();
                }
            }
            _ => self.reserved(),
        }
    }

    fn fp_single(&mut self, funct: u32, fd: usize, fs: usize, ft: usize) {
        let (a, b) = (f(self.fpr[fs]), f(self.fpr[ft]));
        let acc = f(self.acc);
        match funct {
            0 => self.fpr[fd] = out(a + b),
            1 => self.fpr[fd] = out(a - b),
            2 => self.fpr[fd] = out(a * b),
            3 => {
                self.fpr[fd] = if b == 0.0 {
                    ((a.to_bits() ^ b.to_bits()) & 0x8000_0000) | MAX_BITS
                } else {
                    out(a / b)
                }
            }
            4 => self.fpr[fd] = out(b.abs().sqrt()),
            5 => self.fpr[fd] = out(a.abs()),
            6 => self.fpr[fd] = self.fpr[fs],
            7 => self.fpr[fd] = out(-a),
            22 => {
                let d = b.abs().sqrt();
                self.fpr[fd] = if d == 0.0 { MAX_BITS } else { out(a / d) };
            }
            24 => self.acc = out(a + b),
            25 => self.acc = out(a - b),
            26 => self.acc = out(a * b),
            28 => self.fpr[fd] = out(acc + a * b),
            29 => self.fpr[fd] = out(acc - a * b),
            30 => self.acc = out(acc + a * b),
            31 => self.acc = out(acc - a * b),
            36 => {
                // cvt.w.s: truncate toward zero, saturating
                let v = if a >= 2147483648.0 {
                    i32::MAX
                } else if a <= -2147483648.0 {
                    i32::MIN
                } else {
                    a as i32
                };
                self.fpr[fd] = v as u32;
            }
            40 => self.fpr[fd] = out(if a > b { a } else { b }),
            41 => self.fpr[fd] = out(if a < b { a } else { b }),
            48 | 50 | 52 | 54 => {
                let r = match funct {
                    48 => false,
                    50 => a == b,
                    52 => a < b,
                    _ => a <= b,
                };
                if r {
                    self.fcr31 |= 1 << 23;
                } else {
                    self.fcr31 &= !(1 << 23);
                }
            }
            _ => self.reserved(),
        }
    }
}
