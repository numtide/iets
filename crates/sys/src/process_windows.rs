//! Windows cannot replace a process image, so the child runs and this
//! process ends with its status.

use std::io;
use std::process::{self, Command, ExitStatus};

/// Runs `cmd` to the end and exits with its code. Returns only on failure.
pub fn exec(cmd: &mut Command, _arg0: &str) -> io::Error {
    match cmd.status() {
        Ok(st) => process::exit(st.code().unwrap_or(1)),
        Err(e) => e,
    }
}

pub fn signal(_st: &ExitStatus) -> Option<i32> {
    None
}
