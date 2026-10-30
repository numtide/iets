//! What a command line or a nix.conf asked for and iets does not do. Each
//! is one level-1 warning per key and process that says why, never silence
//! (docs/reference/errors.md: `ignored-flag`, `ignored-setting`,
//! `unknown-setting`). A warning waits for `flush`, so
//! a later `--log-format` still applies to it, as Nix warns only once every
//! flag is read (common-args.cc:initialFlagsProcessed).
//!
//! `accept-flake-config` waits longer: Nix does nothing with it until a
//! top-level flake has a `nixConfig`, so neither does iets's warning
//! ([`nix_config_seen`]).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::log;
use crate::session::Session;

struct Note {
    code: &'static str,
    key: String,
    raw: String,
    warned: bool,
    for_nix_config: bool,
}

/// The notes of one run, `hushed`, and whether a top-level flake had a
/// `nixConfig`. The session owns it.
#[derive(Default)]
pub struct Ignored {
    hushed: AtomicBool,
    nix_config: AtomicBool,
    noted: Mutex<Vec<Note>>,
}

/// No warnings from this run: an `eval-jobs` worker, whose parent already
/// gave them.
pub fn hush() {
    let s = Session::current();
    s.ignored.hushed.store(true, Ordering::Relaxed);
}

fn push(code: &'static str, key: &str, raw: String, for_nix_config: bool) {
    let s = Session::current();
    let mut noted = s.ignored.noted.lock().unwrap_or_else(|e| e.into_inner());
    if !noted.iter().any(|n| n.code == code && n.key == key) {
        noted.push(Note {
            code,
            key: key.to_string(),
            raw,
            warned: false,
            for_nix_config,
        });
    }
}

/// Remembers the warning `raw`, once per `code` and `key`.
pub fn note(code: &'static str, key: &str, raw: String) {
    push(code, key, raw, false);
}

/// [`note`] for `accept-flake-config`: `flush` gives it only once
/// [`nix_config_seen`] said a top-level flake has a `nixConfig`.
pub fn note_for_nix_config(code: &'static str, key: &str, raw: String) {
    push(code, key, raw, true);
}

/// `warning: 'FLAG' is ignored: WHY`, an `ignored-flag` record.
pub fn flag(flag: &str, why: &str) {
    note("ignored-flag", flag, format!("'{flag}' is ignored: {why}"));
}

/// [`flag`] for `--accept-flake-config`, which waits like
/// [`note_for_nix_config`].
pub fn flag_for_nix_config(flag: &str) {
    let raw = format!("'{flag}' is ignored: {NO_NIX_CONFIG}");
    note_for_nix_config("ignored-flag", flag, raw);
}

/// A top-level flake of this run has a `nixConfig`: the notes that waited
/// for one are given now. An `eval-jobs` worker is hushed, so it tells its
/// collector instead ([`nix_config_was_seen`]).
pub fn nix_config_seen() {
    let s = Session::current();
    s.ignored.nix_config.store(true, Ordering::Relaxed);
    flush();
}

/// Whether [`nix_config_seen`] was called in this run.
pub fn nix_config_was_seen() -> bool {
    let s = Session::current();
    s.ignored.nix_config.load(Ordering::Relaxed)
}

/// Every note not yet given, in the order they were made.
pub fn flush() {
    let s = Session::current();
    let hushed = s.ignored.hushed.load(Ordering::Relaxed);
    let nix_config = s.ignored.nix_config.load(Ordering::Relaxed);
    let mut noted = s.ignored.noted.lock().unwrap_or_else(|e| e.into_inner());
    let due = |n: &&mut Note| !n.warned && (nix_config || !n.for_nix_config);
    for n in noted.iter_mut().filter(due) {
        n.warned = true;
        if !hushed {
            log::warning(n.code, n.raw.clone());
        }
    }
}

/// The keys noted under `code`, for the tests.
#[cfg(any(test, feature = "testkit"))]
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
