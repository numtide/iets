//! Replacing the process image, reading how a child ended, and a
//! thread's CPU time.

use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus};
use std::time::Duration;

/// Runs `cmd` in place of this process, named `arg0`. Returns only on failure.
pub fn exec(cmd: &mut Command, arg0: &str) -> io::Error {
    cmd.arg0(arg0).exec()
}

/// The signal that ended the child, if one did.
pub fn signal(st: &ExitStatus) -> Option<i32> {
    st.signal()
}

/// CPU time this thread has used, user and system.
pub fn thread_cpu_time() -> Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}
