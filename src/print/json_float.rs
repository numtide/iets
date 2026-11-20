//! Binary64 rendering compatible with nlohmann/json 3.12.0's to_chars.hpp.
//! Adapted from its MIT-licensed Grisu2 implementation.
//! Copyright (c) 2009 Florian Loitsch and 2013–2025 Niels Lohmann.
//! See `iets help json-float-notice` for the permission notice.
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
    let digits = digits.to_string();
    let point = digits.len() as i32 + exponent;
    let mut out = String::with_capacity(25);
    if value.is_sign_negative() {
        out.push('-');
    }
    if (-3..=0).contains(&point) {
        out.push_str("0.");
        out.extend(repeat_n('0', (-point) as usize));
        out.push_str(&digits);
    } else if (1..=15).contains(&point) {
        let point = point as usize;
        if point < digits.len() {
            out.push_str(&digits[..point]);
            out.push('.');
            out.push_str(&digits[point..]);
        } else {
            out.push_str(&digits);
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

// Rounded binary approximations to 10^k for k = -300, -292, ..., 324.
// Each pair stores a normalized 64-bit significand and its binary exponent.
const POWERS: [(u64, i32); 79] = [
    (0xAB70FE17C79AC6CA, -1060),
    (0xFF77B1FCBEBCDC4F, -1034),
    (0xBE5691EF416BD60C, -1007),
    (0x8DD01FAD907FFC3C, -980),
    (0xD3515C2831559A83, -954),
    (0x9D71AC8FADA6C9B5, -927),
    (0xEA9C227723EE8BCB, -901),
    (0xAECC49914078536D, -874),
    (0x823C12795DB6CE57, -847),
    (0xC21094364DFB5637, -821),
    (0x9096EA6F3848984F, -794),
    (0xD77485CB25823AC7, -768),
    (0xA086CFCD97BF97F4, -741),
    (0xEF340A98172AACE5, -715),
    (0xB23867FB2A35B28E, -688),
    (0x84C8D4DFD2C63F3B, -661),
    (0xC5DD44271AD3CDBA, -635),
    (0x936B9FCEBB25C996, -608),
    (0xDBAC6C247D62A584, -582),
    (0xA3AB66580D5FDAF6, -555),
    (0xF3E2F893DEC3F126, -529),
    (0xB5B5ADA8AAFF80B8, -502),
    (0x87625F056C7C4A8B, -475),
    (0xC9BCFF6034C13053, -449),
    (0x964E858C91BA2655, -422),
    (0xDFF9772470297EBD, -396),
    (0xA6DFBD9FB8E5B88F, -369),
    (0xF8A95FCF88747D94, -343),
    (0xB94470938FA89BCF, -316),
    (0x8A08F0F8BF0F156B, -289),
    (0xCDB02555653131B6, -263),
    (0x993FE2C6D07B7FAC, -236),
    (0xE45C10C42A2B3B06, -210),
    (0xAA242499697392D3, -183),
    (0xFD87B5F28300CA0E, -157),
    (0xBCE5086492111AEB, -130),
    (0x8CBCCC096F5088CC, -103),
    (0xD1B71758E219652C, -77),
    (0x9C40000000000000, -50),
    (0xE8D4A51000000000, -24),
    (0xAD78EBC5AC620000, 3),
    (0x813F3978F8940984, 30),
    (0xC097CE7BC90715B3, 56),
    (0x8F7E32CE7BEA5C70, 83),
    (0xD5D238A4ABE98068, 109),
    (0x9F4F2726179A2245, 136),
    (0xED63A231D4C4FB27, 162),
    (0xB0DE65388CC8ADA8, 189),
    (0x83C7088E1AAB65DB, 216),
    (0xC45D1DF942711D9A, 242),
    (0x924D692CA61BE758, 269),
    (0xDA01EE641A708DEA, 295),
    (0xA26DA3999AEF774A, 322),
    (0xF209787BB47D6B85, 348),
    (0xB454E4A179DD1877, 375),
    (0x865B86925B9BC5C2, 402),
    (0xC83553C5C8965D3D, 428),
    (0x952AB45CFA97A0B3, 455),
    (0xDE469FBD99A05FE3, 481),
    (0xA59BC234DB398C25, 508),
    (0xF6C69A72A3989F5C, 534),
    (0xB7DCBF5354E9BECE, 561),
    (0x88FCF317F22241E2, 588),
    (0xCC20CE9BD35C78A5, 614),
    (0x98165AF37B2153DF, 641),
    (0xE2A0B5DC971F303A, 667),
    (0xA8D9D1535CE3B396, 694),
    (0xFB9B7CD9A4A7443C, 720),
    (0xBB764C4CA7A44410, 747),
    (0x8BAB8EEFB6409C1A, 774),
    (0xD01FEF10A657842C, 800),
    (0x9B10A4E5E9913129, 827),
    (0xE7109BFBA19C0C9D, 853),
    (0xAC2820D9623BF429, 880),
    (0x80444B5E7AA7CF85, 907),
    (0xBF21E44003ACDD2D, 933),
    (0x8E679C2F5E44FF8F, 960),
    (0xD433179D9C8CB841, 986),
    (0x9E19DB92B4E31BA9, 1013),
];

#[cfg(test)]
mod tests {
    use super::json_float;

    #[test]
    fn matches_nix_json_tokens() {
        for (value, token) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1e23, "9.999999999999999e+22"),
            (-2.1861718343533215e-172, "-2.1861718343533216e-172"),
            (5.319342696817445e-196, "5.3193426968174445e-196"),
            (f64::from_bits(0x01a03d85a2cb5768), "7.578250078125e-301"),
            (f64::from_bits(1), "5e-324"),
            (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
            (f64::MAX, "1.7976931348623157e+308"),
            (1e-5, "1e-05"),
            (1e-4, "0.0001"),
            (1e14, "100000000000000.0"),
            (1e15, "1e+15"),
            (f64::INFINITY, "null"),
            (f64::NEG_INFINITY, "null"),
            (f64::NAN, "null"),
        ] {
            assert_eq!(json_float(value), token);
        }
    }
}
