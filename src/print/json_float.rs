//! Binary64 rendering compatible with nlohmann/json 3.12.0's to_chars.hpp.
//! Adapted from its MIT-licensed Grisu2 implementation.
//! Copyright (c) 2009 Florian Loitsch and 2013–2025 Niels Lohmann.
//! See `iets help builtins` for the permission notice.
use std::fmt::Write;
use std::iter::repeat_n;

/// Format the same digits as Nix's JSON serializer, including signed zero.
pub fn json_float(value: f64) -> String {
    if !value.is_finite() {
        return "null".into();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .into();
    }
    let (digits, exponent) = decimal_digits(value.abs());
    let mut buffer = itoa::Buffer::new();
    let digits = buffer.format(digits);
    let point = digits.len() as i32 + exponent;
    let mut out = String::with_capacity(25);
    if value.is_sign_negative() {
        out.push('-');
    }
    if (-3..=0).contains(&point) {
        out.push_str("0.");
        out.extend(repeat_n('0', (-point) as usize));
        out.push_str(digits);
    } else if (1..=15).contains(&point) {
        let point = point as usize;
        if point < digits.len() {
            out.push_str(&digits[..point]);
            out.push('.');
            out.push_str(&digits[point..]);
        } else {
            out.push_str(digits);
            out.extend(repeat_n('0', point - digits.len()));
            out.push_str(".0");
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        write!(out, "e{:+03}", point - 1).unwrap();
    }
    out
}

// Return decimal digits and their power of ten. Nix chooses digits inside the
// rounded, tightened interval of Grisu2, including its last-digit tie rule.
fn decimal_digits(value: f64) -> (u64, i32) {
    let bits = value.to_bits();
    let biased = (bits >> 52) as i32;
    let fraction = bits & ((1 << 52) - 1);
    let significand = if biased == 0 {
        fraction
    } else {
        fraction | (1 << 52)
    };
    let binary = biased.max(1) - 1075;
    // The lower midpoint is twice as close at normal powers of two, except
    // at the smallest normal. Align all three values to the upper exponent.
    let upper = significand * 2 + 1;
    let shift = upper.leading_zeros();
    let upper_exp = binary - 1 - shift as i32;
    let center = significand << (shift + 1);
    let lower = if fraction == 0 && biased > 1 {
        (significand * 4 - 1) << (shift - 1)
    } else {
        (significand * 2 - 1) << shift
    };
    // Reference fixed-point approximation for ceil(log10(2) * (-61-e)).
    let f = -61 - upper_exp;
    let power = f * 78913 / (1 << 18) + i32::from(f > 0);
    let index = (power + 307) / 8;
    let (multiplier, cached_exp) = POWERS[index as usize];
    // Rounded high half of the product. Move both scaled bounds inward
    // one unit to account for rounding the power and the multiplication.
    let scaled = |n: u64| ((u128::from(n) * u128::from(multiplier) + (1 << 63)) >> 64) as u64;
    let high = scaled(upper << shift) - 1;
    let mut width = high - (scaled(lower) + 1);
    let mut distance = high - scaled(center);
    let scale = -(upper_exp + cached_exp + 64);
    let one = 1u64 << scale;
    let places = (high >> scale).ilog10();
    let mut unit = 10u64.pow(places) << scale;
    let mut exponent = 300 - index * 8 + places as i32;
    let mut rest = high;
    let mut digits = 0;
    loop {
        digits = digits * 10 + rest / unit;
        rest %= unit;
        if rest <= width {
            while rest < distance
                && width - rest >= unit
                && (rest + unit < distance || distance - rest > rest + unit - distance)
            {
                digits -= 1;
                rest += unit;
            }
            return (digits, exponent);
        }
        exponent -= 1;
        if unit > one {
            unit /= 10;
        } else {
            // Here distance <= width < rest < one <= 2^60.
            rest *= 10;
            width *= 10;
            distance *= 10;
        }
    }
}

// Grisu2 uses the nearest 64-bit significand for 10^k, k = -300, -292, ..., 324.
const POWERS: [(u64, i32); 79] = {
    // floor(2^1093 / 10^300). The exact power lies in [lo, hi] times 2^exp.
    let mut lo = 0x1_56e1_fc2f_8f35_8d94_db7a_c614_u128;
    let mut hi = lo + 1;
    let mut exp = -1093;
    let mut powers = [(0, 0); 79];
    let mut i = 0;
    while i < powers.len() {
        // Round from 97 to 64 bits. Both bounds must select the same value.
        let mantissa = (lo + (1 << 32)) >> 33;
        assert!(mantissa == (hi + (1 << 32)) >> 33);
        powers[i] = (mantissa as u64, exp + 33);

        lo *= 100_000_000;
        hi *= 100_000_000;
        let shift = lo.ilog2() - 96;
        lo >>= shift;
        hi = hi.div_ceil(1 << shift);
        exp += shift as i32;
        i += 1;
    }
    powers
};
