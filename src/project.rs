//! The project root: the directory a run's sources belong to.
//!
//! Three things need the same answer, so they ask the same question here:
//! pure evaluation reads below the root without a flake
//! (`builtins::store_hooks::allowed`), the `impure-path-in-drv` and
//! `absolute-path-in-drv` lints split on it (`lint::purity`), and the
//! project `iets.toml` walk stops at it (`lint::find_config_file`).
//!
//! Three markers, and each says something different, so each is weighed
//! differently:
//!
//! 1. **A working tree** (`source::scm`) is the unit the code lives in, and
//!    the nearest one wins over everything below it. A subdirectory that
//!    holds a marker of its own must not stop a sibling directory from
//!    being read.
//! 2. **`flake.nix`** claims a boundary — Nix makes that directory a tree —
//!    so the nearest one wins when there is no working tree. An unpacked
//!    tarball and a store copy of a checkout land here.
//! 3. **`iets.toml`** claims nothing. It sets lint levels for what is below
//!    it, a project may hold several, and a deep one must not shrink the
//!    root. So the **outermost** one answers, and only when neither of the
//!    two above does.
//!
//! `docs/explanation/flakes-and-sources.md` says why the root is an allow
//! list and not a copy of a tree.

use std::path::{Path, PathBuf};

use crate::source::scm;

pub const FLAKE: &str = "flake.nix";
pub const CONFIG: &str = "iets.toml";

/// Every entry whose presence makes `dir` a project root, whatever the rule
/// that weighs them. `lint::purity` probes these through the tracing cache.
pub fn markers(dir: &Path) -> impl Iterator<Item = PathBuf> + use<'_> {
    scm::root_markers(dir).chain([dir.join(FLAKE), dir.join(CONFIG)])
}

/// The project root at or above `start`, `None` above every marker.
/// `present` decides whether a marker is there: the file system for a
/// frontend, the tracing cache's own `stat` for the lint.
///
/// The walk goes to the top whenever no working tree is found, so a marker
/// above a deeper one is still weighed — and, for the lint, so every probe
/// the answer depends on is an observation.
pub fn find_with(start: &Path, mut present: impl FnMut(&Path) -> bool) -> Option<PathBuf> {
    let mut flake = None;
    let mut config = None;
    for dir in start.ancestors() {
        if scm::root_markers(dir).any(|m| present(&m)) {
            return Some(dir.to_path_buf());
        }
        if flake.is_none() && present(&dir.join(FLAKE)) {
            flake = Some(dir.to_path_buf());
        }
        if present(&dir.join(CONFIG)) {
            config = Some(dir.to_path_buf());
        }
    }
    flake.or(config)
}

/// [`find_with`] on the file system.
pub fn find(start: &Path) -> Option<PathBuf> {
    find_with(start, |m| m.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{env, fs};

    fn tmp(name: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("iets-project-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Each marker on its own makes a root, the SCM markers included: the
    /// test SCM's marker is in the list because detection knows it.
    #[test]
    fn each_marker_is_a_root() {
        let d = tmp("markers");
        let deep = d.join("a/b");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(find(&deep), None);
        for m in markers(&d) {
            fs::write(&m, "").unwrap();
            assert_eq!(find(&deep).as_deref(), Some(d.as_path()), "{m:?}");
            fs::remove_file(&m).unwrap();
        }
        fs::remove_dir_all(&d).unwrap();
    }

    /// A working tree beats a file marker below it: an `iets.toml` that
    /// sets lint levels for a subdirectory must not shrink what a pure
    /// evaluation there may read.
    #[test]
    fn a_working_tree_wins_over_a_file_marker_below_it() {
        let d = tmp("tree");
        let sub = d.join("sub");
        fs::create_dir_all(sub.join("deep")).unwrap();
        fs::write(sub.join("iets.toml"), "").unwrap();
        fs::write(sub.join("flake.nix"), "").unwrap();
        assert_eq!(find(&sub.join("deep")).as_deref(), Some(sub.as_path()));
        let marker = scm::root_markers(&d).next().unwrap();
        fs::write(&marker, "").unwrap();
        assert_eq!(find(&sub.join("deep")).as_deref(), Some(d.as_path()));
        fs::remove_dir_all(&d).unwrap();
    }

    /// Outside a working tree the nearest `flake.nix` wins, so a sub-flake
    /// of an unpacked tarball is its own project.
    #[test]
    fn the_nearest_flake_wins_without_a_working_tree() {
        let d = tmp("nearest");
        let inner = d.join("inner");
        fs::create_dir_all(inner.join("deep")).unwrap();
        fs::write(d.join(FLAKE), "").unwrap();
        fs::write(inner.join(FLAKE), "").unwrap();
        assert_eq!(find(&inner.join("deep")).as_deref(), Some(inner.as_path()));
        fs::remove_dir_all(&d).unwrap();
    }

    /// An `iets.toml` claims no boundary: a deep one must not shrink the
    /// root, so a `flake.nix` above it wins and the outermost one answers
    /// when nothing else does. A store copy of a checkout is this case —
    /// no working tree, and `[lint]` tables in subdirectories.
    #[test]
    fn a_config_file_never_shrinks_the_root() {
        let d = tmp("config");
        let sub = d.join("sub");
        fs::create_dir_all(sub.join("deep")).unwrap();
        fs::write(sub.join(CONFIG), "").unwrap();
        assert_eq!(find(&sub.join("deep")).as_deref(), Some(sub.as_path()));
        fs::write(d.join(CONFIG), "").unwrap();
        assert_eq!(find(&sub.join("deep")).as_deref(), Some(d.as_path()));
        fs::write(d.join(FLAKE), "").unwrap();
        assert_eq!(find(&sub.join("deep")).as_deref(), Some(d.as_path()));
        fs::remove_dir_all(&d).unwrap();
    }
}
