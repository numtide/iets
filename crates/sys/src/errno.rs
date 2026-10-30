//! The OS errors the file-like layers answer with and test for.

use std::io::{self, ErrorKind};

use rustix::io::Errno;

pub fn not_found() -> io::Error {
    Errno::NOENT.into()
}

pub fn is_a_directory() -> io::Error {
    Errno::ISDIR.into()
}

pub fn not_a_directory() -> io::Error {
    Errno::NOTDIR.into()
}

pub fn is_directory(e: &io::Error) -> bool {
    e.kind() == ErrorKind::IsADirectory
}

pub fn is_not_directory(e: &io::Error) -> bool {
    e.kind() == ErrorKind::NotADirectory
}
