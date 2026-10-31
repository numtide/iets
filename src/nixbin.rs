//! The real Nix binaries. Under the classic symlink farm, `nix` on `PATH` is
//! iets itself, so every subprocess and the fallback resolve past it.

use crate::sys::fs::is_runnable;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use crate::session::Session;
use crate::source;

/// Written by the farm derivation next to its `bin/`: the directory that
/// holds the nix the farm shadows.
const REAL_DIR_FILE: &str = "share/iets/real-nix-bin";

fn executable(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && is_runnable(&m))
}

/// The bin directory a farm entry points at: every hop of the symlink chain
/// from `p` is a directory that may carry `REAL_DIR_FILE` beside it. The
/// system path is itself a symlink farm over ours, so the file is one or
/// more hops away.
fn farm_real_dir(p: &Path) -> Option<PathBuf> {
    let mut cur = p.to_path_buf();
    for _ in 0..16 {
        let dir = cur.parent()?;
        let marker = dir.join("..").join(REAL_DIR_FILE);
        if let Ok(s) = fs::read_to_string(&marker) {
            return Some(PathBuf::from(s.trim()));
        }
        let target = fs::read_link(&cur).ok()?;
        cur = if target.is_absolute() {
            target
        } else {
            dir.join(target)
        };
    }
    None
}

/// A candidate that is iets, whatever build of it: this executable, or a
/// symlink chain that ends in a file named `iets`. A host can hold several
/// builds of the same source, and a farm made by one of them points at its
/// own `bin/iets`, which is not `/proc/self/exe`.
fn is_iets(canonical: &Path, me: Option<&Path>) -> bool {
    Some(canonical) == me || canonical.file_name() == Some(OsStr::new("iets"))
}

/// `dir/<name>` as a real nix: the file itself when it is not iets, else the
/// bin directory the farm entry for that iets records. `None` means the
/// directory offers no real nix, so the caller must look further. Without
/// this every candidate would be a candidate to exec, and a directory that
/// holds iets's own classic mode commands makes the fallback exec iets again.
fn candidate(dir: &Path, name: &str, me: Option<&Path>) -> Option<PathBuf> {
    let p = dir.join(name);
    if !executable(&p) {
        return None;
    }
    if !fs::canonicalize(&p).is_ok_and(|c| is_iets(&c, me)) {
        return Some(p);
    }
    let r = farm_real_dir(&p)?.join(name);
    executable(&r).then_some(r)
}

/// The first `<name>` in `dirs` that is not iets, or the bin directory the
/// farm entry for an iets records. Every iets is skipped like ourselves,
/// so a second build of iets under the name `nix` never wins.
fn on_path(name: &str, dirs: impl Iterator<Item = PathBuf>, me: Option<&Path>) -> Option<PathBuf> {
    dirs.filter_map(|dir| candidate(&dir, name, me)).next()
}

/// The real `<name>`: `$IETS_NIX_BIN_DIR/<name>` when it exists there, else
/// the bin directory the farm on `PATH` was built for, else the first
/// `<name>` on `PATH` that is not iets.
pub fn real(name: &str) -> Option<PathBuf> {
    let me = this_exe();
    if let Some(dir) = &Session::current().settings().nix_bin_dir
        && let Some(p) = candidate(dir, name, me.as_deref())
    {
        return Some(p);
    }
    let dirs = env::split_paths(&env::var_os("PATH")?).collect::<Vec<_>>();
    on_path(name, dirs.into_iter(), me.as_deref()).or_else(|| sibling(name))
}

/// This executable, resolved, so that it compares with what a candidate
/// resolves to.
fn this_exe() -> Option<PathBuf> {
    fs::canonicalize(env::current_exe().ok()?).ok()
}

/// The farm never shadows `nix-store`, so its real location names the nix
/// installation even on a system path built without the marker.
fn sibling(name: &str) -> Option<PathBuf> {
    let me = this_exe();
    for dir in env::split_paths(&env::var_os("PATH")?) {
        let probe = dir.join("nix-store");
        if !executable(&probe) {
            continue;
        }
        let real = fs::canonicalize(&probe).ok()?;
        let p = real.parent()?.join(name);
        if executable(&p) && !fs::canonicalize(&p).is_ok_and(|c| is_iets(&c, me.as_deref())) {
            return Some(p);
        }
    }
    None
}

/// `nix` for a subprocess. A bare `nix` when none is found, so the spawn
/// error names it.
pub fn nix() -> PathBuf {
    real("nix").unwrap_or_else(|| PathBuf::from("nix"))
}

/// `nix` run on iets's behalf against the eval store: a fetch whose result
/// iets reads from `/nix/store`, or a `path-info` on what is there.
pub fn on_eval_store(nix: impl AsRef<OsStr>) -> Command {
    let mut c = Command::new(nix);
    let s = Session::current();
    let store = source::nix_conf(&s).store();
    if let Some(remote) = eval_store_remote(env::var_os("NIX_REMOTE").as_deref(), &store) {
        c.env("NIX_REMOTE", remote);
    }
    c
}

/// What `NIX_REMOTE` must say for a subprocess on the eval store, given
/// the inherited value and nix.conf's `store`; `None` leaves it alone. iets
/// answers `auto`, the default, with the daemon (`Transport::parse`), where
/// Nix takes `NIX_REMOTE` instead. nixbot exports its build store there
/// beside `--eval-store daemon` (numtide/iets#305), and a fetch into that
/// store is not in `/nix/store`.
fn eval_store_remote(inherited: Option<&OsStr>, store: &str) -> Option<String> {
    inherited?;
    Some(match store {
        "auto" | "" => "daemon".into(),
        s => s.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::{set_executable, symlink};
    use tempfile::tempdir;

    #[test]
    fn a_subprocess_on_the_eval_store_does_not_follow_nix_remote() {
        let farm = OsStr::new("grpc://farm:50051");
        assert_eq!(
            eval_store_remote(Some(farm), "auto").as_deref(),
            Some("daemon")
        );
        assert_eq!(
            eval_store_remote(Some(farm), "unix:///run/d").as_deref(),
            Some("unix:///run/d")
        );
        // without NIX_REMOTE, Nix's `auto` finds the same store as iets's
        assert_eq!(eval_store_remote(None, "auto"), None);
    }

    #[test]
    fn farm_marker_is_found_through_two_hops() {
        let tmp = tempdir().unwrap();
        let farm = tmp.path().join("farm");
        let sys = tmp.path().join("sys");
        fs::create_dir_all(farm.join("bin")).unwrap();
        fs::create_dir_all(farm.join("share/iets")).unwrap();
        fs::create_dir_all(sys.join("bin")).unwrap();
        fs::write(farm.join(REAL_DIR_FILE), "/real/bin\n").unwrap();
        symlink("/proc/self/exe", farm.join("bin/nix")).unwrap();
        symlink(farm.join("bin/nix"), sys.join("bin/nix")).unwrap();
        assert_eq!(
            farm_real_dir(&sys.join("bin/nix")),
            Some(PathBuf::from("/real/bin"))
        );
        assert_eq!(farm_real_dir(&sys.join("bin/other")), None);
    }

    /// Two builds of iets on `PATH`, both shadowing `nix`: neither is the
    /// real nix, whichever one is running.
    #[test]
    fn every_iets_build_is_skipped() {
        let tmp = tempdir().unwrap();
        let (x, y, real) = (
            tmp.path().join("x"),
            tmp.path().join("y"),
            tmp.path().join("real"),
        );
        let exe = |p: &Path| {
            fs::write(p, "#!/bin/sh\nexit 0\n").unwrap();
            set_executable(p, true).unwrap();
        };
        for d in [&x, &y, &real] {
            fs::create_dir_all(d).unwrap();
        }
        for d in [&x, &y] {
            exe(&d.join("iets"));
            symlink(d.join("iets"), d.join("nix")).unwrap();
        }
        let nix = real.join("nix");
        exe(&nix);

        let dirs = [x.clone(), y.clone(), real.clone()];
        assert_eq!(
            on_path("nix", dirs.iter().cloned(), None),
            Some(nix.clone())
        );
        assert_eq!(on_path("nix", dirs[..2].iter().cloned(), None), None);
        let resolved = fs::canonicalize(&nix).unwrap();
        assert_eq!(on_path("nix", dirs.iter().cloned(), Some(&resolved)), None);

        // what `nix-bin-dir` points at goes through the same test, so a
        // directory of iets's own classic mode commands cannot exec iets again
        assert_eq!(candidate(&x, "nix", None), None);
        assert_eq!(candidate(&real, "nix", None), Some(nix));
        assert_eq!(candidate(&real, "nix-build", None), None);
    }
}
