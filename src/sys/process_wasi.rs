//! WASI has no child processes, so nothing replaces this one and no child ends.

use std::io;
use std::process::{Command, ExitStatus};
use std::time::Duration;

pub fn exec(_cmd: &mut Command, _arg0: &str) -> io::Error {
    io::ErrorKind::Unsupported.into()
}

pub fn signal(_st: &ExitStatus) -> Option<i32> {
    None
}

/// The only process there is: WASI has no ids, and `std::process::id` panics.
pub fn id() -> u32 {
    1
}

/// WASI's CPU clock is optional, and browsers do not have one.
pub fn thread_cpu_time() -> Duration {
    Duration::ZERO
}
