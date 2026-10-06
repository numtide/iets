//! The project root: the directory a run's sources belong to.
//!
//! Three things need the same answer, so they ask the same question here:
//! pure evaluation reads below the root without a flake
//! (`builtins::store_hooks::allowed`), the `impure-path-in-drv` and
//! `absolute-path-in-drv` lints split on it (`lint::purity`), and the
//! project `iets.toml` walk stops at it (`lint::find_config_file`).
//!
//! A marker is a `flake.nix`, the root of a working tree of an SCM iets
//! knows (`source::scm`), or an `iets.toml`. The first two say what Nix
//! says; `iets.toml` lets a checkout that installs neither name its own
//! root. `docs/explanation/flakes-and-sources.md` says why the root is an
//! allow list and not a copy of a tree.

use std::path::{Path, PathBuf};

/// Every entry whose presence makes `dir` a project root, in probe order.
pub fn markers(dir: &Path) -> impl Iterator<Item = PathBuf> + use<'_> {
    std::iter::once(dir.join("flake.nix"))
        .chain(crate::source::scm::root_markers(dir))
        .chain(std::iter::once(dir.join("iets.toml")))
}

/// Whether `dir` is a project root. A marker's presence decides, not its
/// type: `.git` is a directory in a checkout and a file in a worktree.
pub fn is_root(dir: &Path) -> bool {
    markers(dir).any(|m| m.exists())
}

/// The nearest project root at or above `start`, `None` above every marker.
pub fn find(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| is_root(d))
        .map(Path::to_path_buf)
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

    /// The nearest root wins: a sub-flake is its own project.
    #[test]
    fn the_nearest_root_wins() {
        let d = tmp("nearest");
        let inner = d.join("inner");
        std::fs::create_dir_all(inner.join("deep")).unwrap();
        std::fs::write(d.join("iets.toml"), "").unwrap();
        std::fs::write(inner.join("flake.nix"), "").unwrap();
        assert_eq!(find(&inner.join("deep")).as_deref(), Some(inner.as_path()));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
