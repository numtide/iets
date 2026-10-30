//! Replacing the process image and reading how a child ended.

use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus};

/// Runs `cmd` in place of this process, named `arg0`. Returns only on failure.
pub fn exec(cmd: &mut Command, arg0: &str) -> io::Error {
    cmd.arg0(arg0).exec()
}

/// The signal that ended the child, if one did.
pub fn signal(st: &ExitStatus) -> Option<i32> {
    st.signal()
}
