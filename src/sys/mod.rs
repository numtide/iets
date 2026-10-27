//! What differs between operating systems, behind one interface. Callers
//! never test the platform. The Linux backend uses the kernel's own
//! facilities. The portable one gives the same answers more slowly.

use std::env;
use std::ffi::OsStr;
use std::fs::metadata;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg_attr(target_os = "linux", path = "linux.rs")]
#[cfg_attr(not(target_os = "linux"), path = "portable.rs")]
mod imp;

/// What to do about a fault that belongs to this process.
pub struct Fault {
    pub message: &'static [u8],
    /// Abort for a core dump, or exit with status 1.
    pub abort: bool,
}

pub mod context;
#[cfg_attr(windows, path = "errno_windows.rs")]
pub mod errno;
#[cfg_attr(windows, path = "fault_windows.rs")]
pub mod fault;
#[cfg_attr(windows, path = "ffi_windows.rs")]
mod ffi;
#[cfg_attr(windows, path = "fs_windows.rs")]
pub mod fs;
#[cfg_attr(windows, path = "net_windows.rs")]
pub mod net;
#[cfg_attr(windows, path = "process_windows.rs")]
pub mod process;
pub mod tmp;
#[cfg_attr(windows, path = "tty_windows.rs")]
pub mod tty;
#[cfg_attr(windows, path = "vm_windows.rs")]
pub mod vm;
pub mod winpath;

pub use ffi::{
    OsStrExt, OsStringExt, anchor_len, from_native, path_from_internal, path_to_internal,
};
pub use imp::{beneath, discard, futex_wait, futex_wake, prefault};
pub use vm::resident;

/// `p` in the internal form, for a test to put into an expression.
#[cfg(test)]
pub(crate) fn internal_string(p: &Path) -> String {
    String::from_utf8(path_to_internal(p)).expect("utf-8 path")
}

/// The temporary directory, resolved. Windows hands out the short `RUNNER~1`
/// form, which a resolved path never matches.
pub fn temp_dir() -> PathBuf {
    let t = env::temp_dir();
    fs::canonicalize(&t).unwrap_or(t)
}

/// `name` as a program to spawn: the caller's `PATH` decides, and the
/// directories the package bakes into `IETS_FALLBACK_PATH` (nix/iets.nix)
/// serve a caller with none, such as a systemd service. A wrapper that set
/// `PATH` instead would hand them to every child, the shell of `nix-shell`
/// included.
pub fn program(name: &OsStr) -> PathBuf {
    let find = |dirs: &OsStr| {
        env::split_paths(dirs)
            .map(|d| d.join(name))
            .find(|p| metadata(p).is_ok_and(|m| m.is_file() && fs::is_runnable(&m)))
    };
    let on_path = env::var_os("PATH").is_some_and(|p| find(&p).is_some());
    let fallback = || option_env!("IETS_FALLBACK_PATH").and_then(|d| find(OsStr::new(d)));
    match (on_path, fallback()) {
        (false, Some(p)) => p,
        _ => PathBuf::from(name),
    }
}

/// Fills `buf` from the system's random source. A failure leaves zeros: the
/// bytes are a cache key or a name suffix, and nothing here can act on it.
pub fn random(buf: &mut [u8]) {
    let _ = getrandom::fill(buf);
}

/// The time since `t0`, for timings and statistics. Zero in a fuzzing build:
/// libFuzzer records the values the code compares, and a clock reading in
/// them makes two runs with one seed keep different inputs.
pub fn elapsed(t0: Instant) -> Duration {
    if cfg!(fuzzing) {
        Duration::ZERO
    } else {
        t0.elapsed()
    }
}
