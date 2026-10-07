//! What a command line or a nix.conf asked for and iets does not do. Each
//! is one level-1 warning per key and process that says why, never silence
//! (docs/reference/errors.md: `ignored-flag`, `ignored-setting`,
//! `unknown-setting`). A warning waits for `flush`, so
//! a later `--log-format` still applies to it, as Nix warns only once every
//! flag is read (common-args.cc:initialFlagsProcessed).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::log;
use crate::session::Session;

struct Note {
    code: &'static str,
    key: String,
    raw: String,
    warned: bool,
}

/// The notes of one run, and `hushed`. The session owns it.
#[derive(Default)]
pub struct Ignored {
    hushed: AtomicBool,
    noted: Mutex<Vec<Note>>,
}

/// No warnings from this run: an `eval-jobs` worker, whose parent already
/// gave them.
pub fn hush() {
    let s = Session::current();
    s.ignored.hushed.store(true, Ordering::Relaxed);
}

/// Remembers the warning `raw`, once per `code` and `key`.
pub fn note(code: &'static str, key: &str, raw: String) {
    let s = Session::current();
    let mut noted = s.ignored.noted.lock().unwrap_or_else(|e| e.into_inner());
    if !noted.iter().any(|n| n.code == code && n.key == key) {
        noted.push(Note {
            code,
            key: key.to_string(),
            raw,
            warned: false,
        });
    }
}

/// `warning: 'FLAG' is ignored: WHY`, an `ignored-flag` record.
pub fn flag(flag: &str, why: &str) {
    note("ignored-flag", flag, format!("'{flag}' is ignored: {why}"));
}

/// Every note not yet given, in the order they were made.
pub fn flush() {
    let s = Session::current();
    let hushed = s.ignored.hushed.load(Ordering::Relaxed);
    let mut noted = s.ignored.noted.lock().unwrap_or_else(|e| e.into_inner());
    for n in noted.iter_mut().filter(|n| !n.warned) {
        n.warned = true;
        if !hushed {
            log::warning(n.code, n.raw.clone());
        }
    }
}

/// The keys noted under `code`, for the tests.
#[cfg(test)]
pub fn noted(code: &str) -> Vec<String> {
    let s = Session::current();
    let noted = s.ignored.noted.lock().unwrap_or_else(|e| e.into_inner());
    let keys = noted.iter().filter(|n| n.code == code);
    keys.map(|n| n.key.clone()).collect()
}

/// Why `-v`, `--verbose` and `--debug` change nothing.
pub const ONE_LOG_LEVEL: &str = "iets has one log level, the one Nix starts at";

/// Why `--accept-flake-config` and the `accept-flake-config` setting change
/// nothing (docs/reference/divergences.md).
pub const NO_NIX_CONFIG: &str = "iets never applies a flake's nixConfig, so a flake that sets extra-substituters or allow-import-from-derivation there runs without them; put the settings in nix.conf";
