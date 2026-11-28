//! JSON Unicode escapes shared by fromJSON and the derivation reader.

fn hex4(input: &[u8], cursor: &mut usize) -> Result<u32, &'static str> {
    let mut value = 0;
    for _ in 0..4 {
        let digit = input
            .get(*cursor)
            .and_then(|&c| char::from(c).to_digit(16))
            .ok_or("'\\u' must be followed by 4 hex digits")?;
        value = 16 * value + digit;
        *cursor += 1;
    }
    Ok(value)
}

/// Read the four digits after a JSON Unicode escape and its surrogate partner, if any.
#[inline(always)]
pub(crate) fn unicode_escape(
    input: &[u8],
    cursor: &mut usize,
    missing_low: &'static str,
    unpaired_low: &'static str,
) -> Result<char, &'static str> {
    let mut cp = hex4(input, cursor)?;
    if (0xd800..0xdc00).contains(&cp) {
        if !input[*cursor..].starts_with(b"\\u") {
            return Err(missing_low);
        }
        *cursor += 2;
        let partner = hex4(input, cursor)?;
        if !(0xdc00..0xe000).contains(&partner) {
            return Err(missing_low);
        }
        cp = 0x10000 + ((cp - 0xd800) << 10) + partner - 0xdc00;
    } else if (0xdc00..0xe000).contains(&cp) {
        return Err(unpaired_low);
    }
    Ok(char::from_u32(cp).expect("valid scalar"))
}
