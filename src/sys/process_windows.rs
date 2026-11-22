//! Windows cannot replace a process image, so the child runs and this
//! process ends with its status.

use std::io;
use std::process::{self, Command, ExitStatus};
use std::time::Duration;

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

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

/// This process's id.
pub fn id() -> u32 {
    process::id()
}

/// CPU time this thread has used, user and system.
pub fn thread_cpu_time() -> Duration {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
    // SAFETY: the pseudo handle is always valid and the out pointers are live.
    unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) };
    let ticks = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
    // FILETIME counts 100 ns
    Duration::from_nanos((ticks(k) + ticks(u)) * 100)
}
