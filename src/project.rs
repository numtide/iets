//! The project root: the directory a run's sources belong to.
//!
//! Three things need the same answer, so they ask the same question here:
//! pure evaluation reads below the root without a flake
//! (`builtins::store_hooks::allowed`), the `impure-path-in-drv` and
//! `absolute-path-in-drv` lints split on it (`lint::purity`), and the
//! project `iets.toml` walk stops at it (`lint::find_config_file`).
//!
//! A marker is `flake.nix`, `.git` or `iets.toml`. The first two say the
//! same thing Nix says; `iets.toml` lets a checkout that installs neither
//! name its own root. `docs/explanation/flakes-and-sources.md` says why the
//! root is an allow list and not a copy of a tree.

use std::path::{Path, PathBuf};

/// The marker names, in the order a directory is probed.
pub const MARKERS: [&str; 3] = ["flake.nix", ".git", "iets.toml"];

/// Whether `dir` is a project root. `.git` is a directory in a checkout and
/// a file in a worktree or a submodule, so its type is not tested; the two
/// settings files must be files. `lint::purity::project_root_of` repeats this
/// rule on the tracing cache's own `stat`.
pub fn is_root(dir: &Path) -> bool {
    MARKERS.iter().any(|m| {
        let p = dir.join(m);
        if *m == ".git" {
            p.exists()
        } else {
            p.is_file()
        }
    })
}

/// The nearest project root at or above `start`, `None` above every marker.
pub fn find(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if is_root(&dir) {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
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

    #[test]
    fn each_marker_is_a_root() {
        for m in MARKERS {
            let d = tmp(m.trim_start_matches('.'));
            let deep = d.join("a/b");
            std::fs::create_dir_all(&deep).unwrap();
            assert_eq!(find(&deep), None);
            if m == ".git" {
                std::fs::create_dir(d.join(m)).unwrap();
            } else {
                std::fs::write(d.join(m), "").unwrap();
            }
            assert_eq!(find(&deep).as_deref(), Some(d.as_path()));
            std::fs::remove_dir_all(&d).unwrap();
        }
    }

    /// The nearest root wins: a sub-flake is its own project.
    #[test]
    fn the_nearest_root_wins() {
        let d = tmp("nearest");
        let inner = d.join("inner");
        std::fs::create_dir_all(inner.join("deep")).unwrap();
        std::fs::create_dir(d.join(".git")).unwrap();
        std::fs::write(inner.join("flake.nix"), "").unwrap();
        assert_eq!(find(&inner.join("deep")).as_deref(), Some(inner.as_path()));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
