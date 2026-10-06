//! The project root: the directory a run's sources belong to.
//!
//! Three things need the same answer, so they ask the same question here:
//! pure evaluation reads below the root without a flake
//! (`builtins::store_hooks::allowed`), the `impure-path-in-drv` and
//! `absolute-path-in-drv` lints split on it (`lint::purity`), and the
//! project `iets.toml` walk stops at it (`lint::find_config_file`).
//!
//! **A working tree wins over a file marker, however deep the file marker
//! is.** A repository is the unit the code lives in: a subdirectory that
//! holds an `iets.toml` to set lint levels, or a `flake.nix` of its own,
//! must not stop a sibling directory from being read. Only outside a
//! working tree — an unpacked tarball, a scratch directory — does the
//! nearest `flake.nix` or `iets.toml` name the root.
//!
//! `docs/explanation/flakes-and-sources.md` says why the root is an allow
//! list and not a copy of a tree.

use std::path::{Path, PathBuf};

/// The markers of a directory that is no working tree, in probe order.
pub const FILES: [&str; 2] = ["flake.nix", "iets.toml"];

/// Every entry whose presence makes `dir` a project root, in probe order.
/// `lint::purity` probes these through the tracing cache.
pub fn markers(dir: &Path) -> impl Iterator<Item = PathBuf> + use<'_> {
    crate::source::scm::root_markers(dir).chain(FILES.iter().map(|f| dir.join(f)))
}

/// The nearest project root at or above `start`, `None` above every marker.
/// `present` decides whether a marker is there: the file system for a
/// frontend, the tracing cache's own `stat` for the lint.
///
/// The walk goes to the top whenever no working tree is found, so a
/// `.git` that appears above a file marker is still the answer — and, for
/// the lint, so every probe it depends on is an observation.
pub fn find_with(start: &Path, mut present: impl FnMut(&Path) -> bool) -> Option<PathBuf> {
    let mut file_marker = None;
    for dir in start.ancestors() {
        if crate::source::scm::root_markers(dir).any(|m| present(&m)) {
            return Some(dir.to_path_buf());
        }
        if file_marker.is_none() && FILES.iter().any(|f| present(&dir.join(f))) {
            file_marker = Some(dir.to_path_buf());
        }
    }
    file_marker
}

/// [`find_with`] on the file system.
pub fn find(start: &Path) -> Option<PathBuf> {
    find_with(start, |m| m.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("iets-project-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Each marker on its own makes a root, the SCM markers included: the
    /// test SCM's marker is in the list because detection knows it.
    #[test]
    fn each_marker_is_a_root() {
        let d = tmp("markers");
        let deep = d.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find(&deep), None);
        for m in markers(&d) {
            std::fs::write(&m, "").unwrap();
            assert_eq!(find(&deep).as_deref(), Some(d.as_path()), "{m:?}");
            std::fs::remove_file(&m).unwrap();
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// A working tree beats a file marker below it: an `iets.toml` that
    /// sets lint levels for a subdirectory must not shrink what a pure
    /// evaluation there may read.
    #[test]
    fn a_working_tree_wins_over_a_file_marker_below_it() {
        let d = tmp("tree");
        let sub = d.join("sub");
        std::fs::create_dir_all(sub.join("deep")).unwrap();
        std::fs::write(sub.join("iets.toml"), "").unwrap();
        std::fs::write(sub.join("flake.nix"), "").unwrap();
        assert_eq!(find(&sub.join("deep")).as_deref(), Some(sub.as_path()));
        let marker = crate::source::scm::root_markers(&d).next().unwrap();
        std::fs::write(&marker, "").unwrap();
        assert_eq!(find(&sub.join("deep")).as_deref(), Some(d.as_path()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Outside a working tree the nearest file marker wins, so a sub-flake
    /// of an unpacked tarball is its own project.
    #[test]
    fn the_nearest_file_marker_wins_without_a_working_tree() {
        let d = tmp("nearest");
        let inner = d.join("inner");
        std::fs::create_dir_all(inner.join("deep")).unwrap();
        std::fs::write(d.join("iets.toml"), "").unwrap();
        std::fs::write(inner.join("flake.nix"), "").unwrap();
        assert_eq!(find(&inner.join("deep")).as_deref(), Some(inner.as_path()));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
