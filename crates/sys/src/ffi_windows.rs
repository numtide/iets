//! Paths and names as bytes, with the signatures of `std::os::unix::ffi`.
//!
//! Windows names are WTF-8 here. Bytes that are not UTF-8 cannot name
//! anything, so they become a name with a NUL in it, which every file
//! operation refuses with an `io::Error`.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::str;

use super::winpath;

/// The file system path an evaluator path names.
pub fn path_from_internal(b: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(winpath::to_native(b)))
}

/// The evaluator's name for a file system path.
pub fn path_to_internal(p: &Path) -> Vec<u8> {
    winpath::to_internal(p.as_os_str().as_bytes())
}

/// An evaluator path from bytes that came from the OS, such as an argument,
/// the working directory or `$HOME`.
pub fn from_native(b: &[u8]) -> Cow<'_, [u8]> {
    Cow::Owned(winpath::to_internal(b))
}

pub use super::winpath::anchor_len;

const UNREPRESENTABLE: &str = "\0";

pub trait OsStrExt {
    fn from_bytes(b: &[u8]) -> &Self;
    fn as_bytes(&self) -> &[u8];
}

impl OsStrExt for OsStr {
    fn from_bytes(b: &[u8]) -> &OsStr {
        OsStr::new(str::from_utf8(b).unwrap_or(UNREPRESENTABLE))
    }

    fn as_bytes(&self) -> &[u8] {
        self.as_encoded_bytes()
    }
}

pub trait OsStringExt {
    fn from_vec(v: Vec<u8>) -> Self;
    fn into_vec(self) -> Vec<u8>;
}

impl OsStringExt for OsString {
    fn from_vec(v: Vec<u8>) -> OsString {
        OsString::from(String::from_utf8(v).unwrap_or_else(|_| UNREPRESENTABLE.to_owned()))
    }

    fn into_vec(self) -> Vec<u8> {
        self.into_encoded_bytes()
    }
}
