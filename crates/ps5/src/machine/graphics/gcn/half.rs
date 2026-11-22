pub(super) fn from_half(bits: u16) -> f32 {
    let sign = (bits as u32 & 0x8000) << 16;
    let exponent = (bits >> 10) & 31;
    let fraction = bits & 1023;
    if exponent == 0 { return if fraction == 0 { f32::from_bits(sign) } else { (if sign == 0 { 1.0 } else { -1.0 }) * fraction as f32 * 2.0f32.powi(-24) }; }
    let value = if exponent == 31 { sign | 0x7f800000 | (fraction as u32) << 13 } else { sign | ((exponent as u32 + 112) << 23) | (fraction as u32) << 13 };
    f32::from_bits(value)
}

pub(super) fn to_half_rtz(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let raw_exponent = (bits >> 23) & 255;
    let fraction = bits & 0x7fffff;
    if raw_exponent == 255 { return sign | if fraction == 0 { 0x7c00 } else { 0x7e00 }; }
    let exponent = raw_exponent as i32 - 112;
    if exponent >= 31 { return sign | 0x7bff; }
    if exponent <= 0 {
        if exponent <= -10 { return sign; }
        return sign | (((fraction | 0x800000) >> (14 - exponent)) as u16);
    }
    sign | (exponent as u16) << 10 | (fraction >> 13) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_finite_half_round_trips_and_overflow_rounds_toward_zero() {
        for bits in 0u16..=u16::MAX {
            if bits & 0x7c00 != 0x7c00 { assert_eq!(to_half_rtz(from_half(bits)), bits); }
        }
        assert_eq!(to_half_rtz(f32::MAX), 0x7bff);
        assert_eq!(to_half_rtz(-f32::MAX), 0xfbff);
        assert_eq!(to_half_rtz(f32::INFINITY), 0x7c00);
        assert_eq!(to_half_rtz(-0.0), 0x8000);
        assert_eq!(to_half_rtz(2.0f32.powi(-24)), 1);
        assert_eq!(to_half_rtz(2.0f32.powi(-25)), 0);
        assert_eq!(to_half_rtz(1.0009), 0x3c00);
    }
}
