//! The binary under test, cut off from the machine: it gets only the
//! environment below and, from the [`bin`] fixture, a home of its own
//! per test, so a `nix.conf` or a cache from an earlier run cannot change
//! what it prints.

// each test crate uses what it needs
#![allow(dead_code)]

use rstest::fixture;
use std::env;
use std::ffi::OsStr;
use std::process::Command;
use tempfile::TempDir;

/// The IETS_ settings every run gets. tests/e2e gives them to Nix too, so a
/// shell either one starts has the same environment.
pub const ENV: &[(&str, &str)] = &[("IETS_PROGRESS", "false")];

/// What a process needs to start, and nothing that changes what iets does.
const INHERIT: &[&str] = &[
    "PATH",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SystemRoot",
    "SystemDrive",
    "PATHEXT",
    "ComSpec",
];

/// iets with a home of its own, kept for the whole test so runs within it
/// share a cache.
pub struct Iets {
    home: TempDir,
}

#[fixture]
pub fn bin() -> Iets {
    let tmp = env!("CARGO_TARGET_TMPDIR");
    let home = TempDir::with_prefix_in("home-", tmp).expect("create the test home");
    Iets { home }
}

impl Iets {
    pub fn cmd(&self) -> Command {
        self.cmd_as(env!("CARGO_BIN_EXE_iets"))
    }

    /// `iets` started as `prog`, a link to it under another name.
    pub fn cmd_as(&self, prog: impl AsRef<OsStr>) -> Command {
        let mut c = Command::new(prog);
        isolate(&mut c);
        let home = self.home.path();
        // its own so a test can fake HOME
        c.env("XDG_CACHE_HOME", home.join(".cache"))
            .env("HOME", home)
            .env("XDG_CONFIG_DIRS", "/nonexistent")
            .env("NIX_CONF_DIR", "/nonexistent")
            // the expected paths are Nix's under /nix/store, on every platform
            .env("NIX_STORE_DIR", "/nix/store");
        c
    }
}

/// Only [`INHERIT`] and [`ENV`]. tests/e2e starts Nix with this too.
pub fn isolate(c: &mut Command) {
    c.env_clear();
    for k in INHERIT {
        if let Some(v) = env::var_os(k) {
            c.env(k, v);
        }
    }
    c.envs(ENV.iter().copied());
}
