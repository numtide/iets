//! Which paths of a read set a watcher can use.
//!
//! Selected for a watcher that compares modification times, which is all a
//! file watcher has. A consumer that can do better picks by `kind`.

use super::{Filtered, Kind, Obs};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::source::scm;

/// A watcher stats every path it holds, and a caller that writes them into
/// an environment has a size limit, so the list has a bound.
pub const MAX: usize = 500;

pub struct Watched {
    pub path: String,
    /// `content` a file the run read, `listing` a directory it listed,
    /// `absent` a path it found missing, `scm` the file a source-control
    /// query depends on, `member` a file under a tree the run copied.
    pub kind: &'static str,
}

/// The watchable paths of `obs`, in the order the run read them. `skip`
/// names roots that never change under a watcher (the Nix store, iets's own
/// stores).
///
/// A tree that lies inside another tree is a path the expression copied, so
/// its members follow. An outermost tree is the flake's source, which every
/// edit changes and few shells depend on: only what the run read out of it
/// is watched. Environment variables and resolved links have no file.
pub fn watched(obs: &[Obs], skip: &[&Path]) -> Vec<Watched> {
    let kept = |b: &[u8]| {
        let p = PathBuf::from(OsStr::from_bytes(b));
        (!skip.iter().any(|s| p.starts_with(s))).then_some(p)
    };
    let mut out: Vec<(PathBuf, &'static str)> = Vec::new();
    let mut trees = Vec::new();
    for o in obs {
        match o {
            Obs::File { path: p, .. } => out.extend(kept(p).map(|p| (p, "content"))),
            Obs::Dir { path: p, .. } => out.extend(kept(p).map(|p| (p, "listing"))),
            // A `Stat` observed existence and kind, which an mtime does not
            // answer: a git index refresh moves `.git` while it is still a
            // directory. A `Missing` path has no mtime to churn, and its
            // creation is the change it looks like.
            Obs::Stat { path: p, kind, .. } => {
                if *kind == Kind::Missing {
                    out.extend(kept(p).map(|p| (p, "absent")))
                }
            }
            Obs::Tree { root, filtered, .. } => {
                trees.extend(kept(root).map(|r| (r, filtered.as_ref())))
            }
            o => {
                if let Some((scm, dir)) = scm::owner(o) {
                    out.extend(kept(dir).and_then(|d| scm.watch(&d)).map(|p| (p, "scm")))
                }
            }
        }
    }
    for (root, filtered) in &trees {
        if trees.iter().any(|(r, _)| root != r && root.starts_with(r)) {
            members(root, *filtered, &mut out);
        }
    }
    let mut seen = HashSet::new();
    out.retain(|(p, _)| seen.insert(p.clone()));
    out.into_iter()
        .filter_map(|(p, kind)| {
            p.into_os_string()
                .into_string()
                .ok()
                .map(|path| Watched { path, kind })
        })
        .collect()
}

/// `p` and what the copy took below it; the directories too, because an
/// entry that comes or goes changes the copy.
fn members(p: &Path, filtered: Option<&Filtered>, out: &mut Vec<(PathBuf, &'static str)>) {
    if out.len() > MAX {
        return;
    }
    out.push((p.to_path_buf(), "member"));
    if !p.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return;
    }
    let mut kids: Vec<PathBuf> = fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    kids.sort();
    for k in kids {
        let name = k.as_os_str().as_encoded_bytes();
        if filtered.is_none_or(|f| f.accepted.binary_search_by(|a| (**a).cmp(name)).is_ok()) {
            members(&k, filtered, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::Filtered;

    use std::env;

    #[test]
    fn only_the_paths_an_mtime_can_speak_for() {
        let d = env::temp_dir().join(format!("iets-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for sub in [".git", "src/keep", "src/drop"] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        for f in ["hook.sh", "src/keep/a", "src/drop/b"] {
            fs::write(d.join(f), "").unwrap();
        }
        let b = |p: &str| d.join(p).into_os_string().into_encoded_bytes();
        let file = |path: Vec<u8>| Obs::File {
            path,
            size: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            dev: 0,
            ino: 0,
            hash: [0; 32],
        };
        let tree = |p: &str, accepted: Option<Vec<Vec<u8>>>| Obs::Tree {
            root: b(p),
            nar_hash: [0; 32],
            stat_hash: [0; 32],
            newest: None,
            filtered: accepted.map(|accepted| Filtered {
                seen_hash: None,
                accepted,
            }),
        };
        let mut obs = vec![
            tree("", None),
            file(b("flake.nix")),
            file(b"/nix/store/aaa-source/default.nix".to_vec()),
            file(b("cache/inputs/x")),
            Obs::Cmd {
                dir: b(""),
                query: crate::trace::Query::GitStatus,
                hash: [0; 32],
            },
            Obs::Env {
                name: b"HOME".to_vec(),
                value: None,
            },
            Obs::Stat {
                path: b("local.nix"),
                follow: false,
                kind: crate::trace::Kind::Missing,
            },
            // a kind observation of a path that is there: not a watch, or a
            // lock file inside `.git` reloads the shell
            Obs::Stat {
                path: b(".git"),
                follow: true,
                kind: crate::trace::Kind::Directory,
            },
            Obs::Dir {
                path: b("conf.d"),
                hash: [0; 32],
            },
            file(b("flake.nix")),
            tree("hook.sh", None),
            tree("src", Some(vec![b("src/keep"), b("src/keep/a")])),
        ];
        // store paths do not count against the bound
        obs.extend((0..=MAX).map(|i| file(format!("/nix/store/aaa-source/{i}").into_bytes())));
        let cache = d.join("cache");
        let got = watched(&obs, &[Path::new("/nix/store"), &cache])
            .into_iter()
            .map(|w| w.path)
            .collect::<Vec<_>>();
        let want = [
            "flake.nix",
            ".git/index",
            "local.nix",
            "conf.d",
            "hook.sh",
            "src",
            "src/keep",
            "src/keep/a",
        ];
        assert_eq!(got, want.map(|p| d.join(p).to_str().unwrap().to_string()));
        let _ = fs::remove_dir_all(&d);
    }
}
