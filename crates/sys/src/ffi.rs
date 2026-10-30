//! Paths and names as bytes, the way the store and the wire format see them.

pub use std::os::unix::ffi::{OsStrExt, OsStringExt};

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// The file system path an evaluator path names. The same bytes on Unix.
pub fn path_from_internal(b: &[u8]) -> PathBuf {
    PathBuf::from(<std::ffi::OsStr as OsStrExt>::from_bytes(b))
}

/// The evaluator's name for a file system path.
pub fn path_to_internal(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

/// An evaluator path from bytes that came from the OS, such as an argument,
/// the working directory or `$HOME`. The same bytes on Unix.
pub fn from_native(b: &[u8]) -> Cow<'_, [u8]> {
    Cow::Borrowed(b)
}

/// How many leading components of a path `..` cannot remove. Unix has none.
pub fn anchor_len(_comps: &[&[u8]]) -> usize {
    0
}
