//! The same errors by kind, since Windows has no errno values to name.

use std::io::{self, ErrorKind};

pub fn not_found() -> io::Error {
    ErrorKind::NotFound.into()
}

pub fn is_a_directory() -> io::Error {
    ErrorKind::IsADirectory.into()
}

pub fn not_a_directory() -> io::Error {
    ErrorKind::NotADirectory.into()
}

pub fn is_directory(e: &io::Error) -> bool {
    e.kind() == ErrorKind::IsADirectory
}

pub fn is_not_directory(e: &io::Error) -> bool {
    e.kind() == ErrorKind::NotADirectory
}
