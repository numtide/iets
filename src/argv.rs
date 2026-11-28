//! Command-line arguments as `String`s that keep every byte. Nix reads argv as
//! bytes, so a path may hold bytes that are not UTF-8. [`encode`] maps each such
//! byte `b` to the character U+10FF00 + `b`, and [`bytes`] maps it back where
//! the bytes matter: paths, expressions, `--arg`/`--argstr` values and the argv
//! of a fallback. A U+10FF80..U+10FFFF typed on the command line is encoded
//! byte by byte as well, so it comes back as its own four bytes.

use crate::sys::OsStringExt;
use std::borrow::Cow;
use std::ffi::OsString;

const BASE: u32 = 0x10FF00;

pub fn encode(arg: OsString) -> String {
    let raw = arg.into_vec();
    let mut s = String::with_capacity(raw.len());
    for chunk in raw.utf8_chunks() {
        for c in chunk.valid().chars() {
            if matches!(u32::from(c).checked_sub(BASE), Some(0x80..=0xff)) {
                s.extend(c.encode_utf8(&mut [0; 4]).bytes().filter_map(carrier));
            } else {
                s.push(c);
            }
        }
        s.extend(chunk.invalid().iter().copied().filter_map(carrier));
    }
    s
}

fn carrier(b: u8) -> Option<char> {
    char::from_u32(BASE + u32::from(b))
}

pub fn bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match u32::from(c).checked_sub(BASE) {
            Some(b @ 0x80..=0xff) => out.push(b as u8),
            _ => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    out
}

/// The argument as text, for a consumer that takes no arbitrary bytes.
pub fn text(s: &str) -> Cow<'_, str> {
    if s.chars().any(|c| u32::from(c) >= BASE + 0x80) {
        Cow::Owned(String::from_utf8_lossy(&bytes(s)).into_owned())
    } else {
        Cow::Borrowed(s)
    }
}

pub fn os(s: &str) -> OsString {
    OsString::from_vec(bytes(s))
}

// TODO: Windows names are UTF-8 here, so arbitrary bytes do not round-trip.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn every_byte_round_trips() {
        let raw: Vec<u8> = b"/tmp/\xff\xfe.nix caf\xc3\xa9 \xc3".to_vec();
        let s = encode(OsString::from_vec(raw.clone()));
        assert!(s.starts_with("/tmp/") && s.contains("café"));
        assert_eq!(bytes(&s), raw);
        assert_eq!(os(&s).into_vec(), raw);
        assert_eq!(encode(OsString::from("-E")), "-E");
    }

    #[test]
    fn a_typed_carrier_character_keeps_its_four_bytes() {
        for c in ['\u{10FF80}', '\u{10FFFF}'] {
            let raw = format!("a{c}b").into_bytes();
            assert_eq!(bytes(&encode(OsString::from_vec(raw.clone()))), raw);
        }
    }
}
