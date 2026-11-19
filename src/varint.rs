//! Unsigned LEB128 shared by the trace, spool index and compiled IR formats.

pub(crate) fn put_u64(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Accepts redundant zero groups for compatibility. Advances only on success.
pub(crate) fn read_u64(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0;
    for (i, &byte) in data.get(*at..)?.iter().take(10).enumerate() {
        if i == 9 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            *at += i + 1;
            return Some(value);
        }
    }
    None
}
