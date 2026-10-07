//! git. The tree is what git.cc:getAccessorFromWorkdir copies: the index
//! entries that still exist, no `.git`, no submodules. Each command that reads
//! the working tree is a `Cmd` observation. The date and the count follow from
//! the revision, which is one, so they are none (git.cc:getLastModified and
//! getRevCount keep them by revision).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Scm, Snapshot};
use crate::trace::{Kind, Obs, Query, Trace};

const NULL_REV: &str = "0000000000000000000000000000000000000000";

pub struct Git;

fn run(trace: &Trace, root: &Path, args: &[&str]) -> Result<(i32, Vec<u8>), String> {
    let mut argv: Vec<Vec<u8>> = vec![b"git".to_vec()];
    argv.extend(args.iter().map(|a| a.as_bytes().to_vec()));
    trace
        .cmd(root, &argv)
        .map_err(|e| format!("iets: cannot run git: {e}"))
}

/// A traced query. Its argv comes from `trace::Query`, so the command and the
/// observation that records it can never drift apart.
fn query(trace: &Trace, root: &Path, q: Query) -> Result<Vec<u8>, String> {
    git(trace, root, &q.argv()[1..])
}

fn git(trace: &Trace, root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let (status, out) = run(trace, root, args)?;
    if status != 0 {
        return Err(format!(
            "iets: `git {}` failed in '{}' (exit {status})",
            args.join(" "),
            root.display()
        ));
    }
    Ok(out)
}

/// A number that follows from a revision. The revision is an observation,
/// so this command is none.
fn of_rev(root: &Path, args: &[&str]) -> Result<u64, String> {
    let out = git(&Trace::default(), root, args)?;
    Ok(String::from_utf8_lossy(&out).trim().parse().unwrap_or(0))
}

impl Scm for Git {
    fn name(&self) -> &'static str {
        "git"
    }

    fn title(&self) -> &'static str {
        "Git"
    }

    fn root_marker(&self, dir: &Path) -> PathBuf {
        dir.join(".git")
    }

    /// git-utils.cc:getWorkdirInfo: index entries minus deleted ones; dirty
    /// when any tracked file differs from HEAD or the index.
    fn snapshot(&self, trace: &Trace, root: &Path) -> Result<Snapshot, String> {
        // `-v` tags an entry that git checks against the working tree no more
        // (`S` for skip-worktree, lower case for assume-unchanged): `git
        // status` never reports such an entry, while libgit2's status, which
        // getWorkdirInfo reads, still calls it deleted when it is not on disk
        let ls = query(trace, root, Query::GitLsFiles)?;
        let mut files: HashSet<PathBuf> = HashSet::new();
        let mut dirty = false;
        for entry in ls.split(|&b| b == 0).filter(|e| !e.is_empty()) {
            let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let mut fields = entry[..tab].split(|&b| b == b' ');
            let tag = fields.next().unwrap_or(b"");
            if fields.next().unwrap_or(b"") == b"160000" {
                continue;
            }
            let file = root.join(OsStr::from_bytes(&entry[tab + 1..]));
            if tag != b"H" && trace.stat_now(file.as_os_str().as_bytes(), false) == Kind::Missing {
                dirty = true;
                continue;
            }
            files.insert(file);
        }
        let status = query(trace, root, Query::GitStatus)?;
        for entry in status.split(|&b| b == 0).filter(|e| e.len() > 3) {
            dirty = true;
            if entry[0] == b'D' || entry[1] == b'D' {
                files.remove(&root.join(OsStr::from_bytes(&entry[3..])));
            }
        }
        let head = run(trace, root, &Query::GitHead.argv()[1..])?;
        let head = (head.0 == 0).then(|| String::from_utf8_lossy(&head.1).trim().to_string());
        let last_modified = match &head {
            Some(h) => of_rev(root, &["log", "-1", "--format=%ct", h])?,
            None => 0,
        };
        let shallow = root.join(".git/shallow");
        let shallow = trace.stat_now(shallow.as_os_str().as_bytes(), true) != Kind::Missing;
        Ok(Snapshot {
            files,
            dirty,
            // git.cc gives a clean tree without a commit the null revision
            rev: head.or_else(|| (!dirty).then(|| NULL_REV.to_string())),
            last_modified,
            countable: !shallow,
        })
    }

    fn rev_count(&self, root: &Path, rev: &str) -> Result<u64, String> {
        // git.cc gives the null revision the count 0
        match rev {
            NULL_REV => Ok(0),
            _ => of_rev(root, &["rev-list", "--count", rev]),
        }
    }

    fn untracked(&self, root: &Path, rel: &OsStr) -> Option<String> {
        let tracked = Command::new("git")
            .arg("--no-optional-locks")
            .arg("-C")
            .arg(root)
            .args(["ls-files", "--error-unmatch", "--"])
            .arg(rel)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        (!tracked.success()).then(|| {
            format!(
                "git -C \"{}\" add \"{}\"",
                root.display(),
                rel.to_string_lossy()
            )
        })
    }

    fn tree_of<'a>(&self, obs: &'a Obs) -> Option<&'a [u8]> {
        match obs {
            Obs::Cmd { dir, query, .. } if query.scm() == self.name() => Some(dir),
            _ => None,
        }
    }

    /// `git ls-files`, `git status` and the `HEAD` queries all change with
    /// the index: a commit, a checkout and `git add` rewrite it. A linked
    /// work tree has a `.git` file that names its git directory.
    fn watch(&self, root: &Path) -> Option<PathBuf> {
        let dot = root.join(".git");
        if dot.is_dir() {
            return Some(dot.join("index"));
        }
        let text = fs::read_to_string(&dot).ok()?;
        Some(
            root.join(text.strip_prefix("gitdir: ")?.trim_end())
                .join("index"),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::flake::parse_flake_ref;
    use crate::source::SourceRef;
    use crate::store::nar::tests::fixture_tree;

    use std::fs;

    #[test]
    fn a_directory_in_a_repository_is_found() {
        let d = fixture_tree("git-root");
        fs::write(d.join("sub/flake.nix"), b"{ outputs = _: {}; }").unwrap();
        fs::create_dir_all(d.join(".git")).unwrap();
        let r = parse_flake_ref(&format!("{}/sub#x", d.display()), b"/", None).unwrap();
        let url = format!("git+file://{}?dir=sub", d.display());
        assert_eq!(crate::api::flake::url(&r), url);
        let r = parse_flake_ref(&format!("path:{}/sub", d.display()), b"/", None).unwrap();
        assert_eq!(r.source, SourceRef::Path(d.join("sub")));
    }
}
