//! Command-line arguments as `String`s that keep every byte. Nix reads argv as
//! bytes, so a path may hold bytes that are not UTF-8. [`encode`] maps each such
//! byte `b` to the character U+10FF00 + `b`, and [`bytes`] maps it back where
//! the bytes matter: paths, expressions, `--arg`/`--argstr` values and the argv
//! of a fallback. A U+10FF80..U+10FFFF typed on the command line also comes back
//! as one byte: those are the last private-use code points and noncharacters.

use crate::OsStringExt;
use std::ffi::OsString;

const BASE: u32 = 0x10FF00;

pub fn encode(arg: OsString) -> String {
    let raw = arg.into_vec();
    let mut s = String::with_capacity(raw.len());
    for chunk in raw.utf8_chunks() {
        s.push_str(chunk.valid());
        s.extend(
            chunk
                .invalid()
                .iter()
                .filter_map(|&b| char::from_u32(BASE + u32::from(b))),
        );
    }
    s
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
}
