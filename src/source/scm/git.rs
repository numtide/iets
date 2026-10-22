//! git. The tree is what git.cc:getAccessorFromWorkdir copies: the index
//! entries that still exist, no `.git`, no submodules. Each command that reads
//! the working tree is a `Cmd` observation. The date and the count follow from
//! the revision, which is one, so they are none (git.cc:getLastModified and
//! getRevCount keep them by revision). The tree of a revision follows from
//! it too; the name of a branch or a tag is a `Cmd` observation of the refs.

use crate::sys::OsStrExt;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{fs, iter, str, thread};

use super::{Commit, Entry, Scm, Snapshot};
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

/// `git ARGS` in `root` with `stdin` as its input; the exit status and the
/// output. No observation.
fn piped(root: &Path, args: &[&str], stdin: Vec<u8>) -> Result<(i32, Vec<u8>), String> {
    let fail = |e: String| {
        format!(
            "iets: `git {}` failed in '{}': {e}",
            args[0],
            root.display()
        )
    };
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| fail(e.to_string()))?;
    let mut input = child.stdin.take().expect("piped");
    let writer = thread::spawn(move || input.write_all(&stdin));
    let out = child.wait_with_output().map_err(|e| fail(e.to_string()))?;
    let _ = writer.join();
    Ok((out.status.code().unwrap_or(-1), out.stdout))
}

/// A file that changes an answer and that git tracks nowhere: its bytes, or
/// its absence, become an observation.
fn observe(trace: &Trace, p: &Path) -> Option<Vec<u8>> {
    let path = p.as_os_str().as_bytes();
    match (fs::metadata(p), fs::read(p)) {
        (Ok(meta), Ok(bytes)) if meta.is_file() => {
            trace.file(path, &meta, &bytes);
            Some(bytes)
        }
        _ => {
            trace.stat_now(path, true);
            None
        }
    }
}

/// The attribute files that libgit2 reads beside the `.gitattributes` of
/// the tree, as Nix opens a repository: `$GIT_DIR/info/attributes` and the
/// file `core.attributesFile` names. libgit2 has no default for the second,
/// where git reads `$XDG_CONFIG_HOME/git/attributes`, so the `-c` this
/// gives makes git read the same files. Both files, and the setting, are
/// observed.
fn attr_files(trace: &Trace, root: &Path) -> Result<String, String> {
    let info = git(
        &Trace::default(),
        root,
        &["rev-parse", "--git-path", "info/attributes"],
    )?;
    let info = String::from_utf8_lossy(&info).trim_end().to_string();
    observe(trace, &root.join(info));
    let (status, global) = run(trace, root, &Query::GitAttributesFile.argv()[1..])?;
    let global = String::from_utf8_lossy(&global).trim_end().to_string();
    if status != 0 || global.is_empty() {
        return Ok("core.attributesFile=/dev/null".into());
    }
    observe(trace, &root.join(&global));
    Ok(format!("core.attributesFile={global}"))
}

/// The paths of `paths`, relative to the root, that `export-ignore` drops.
/// `source` is `--cached` for the index or `--source=REV` for a revision.
fn check_export(
    trace: &Trace,
    root: &Path,
    source: &str,
    paths: impl Iterator<Item = Vec<u8>>,
) -> Result<Option<HashSet<Vec<u8>>>, String> {
    let config = attr_files(trace, root)?;
    let mut stdin = Vec::new();
    for p in paths {
        stdin.extend_from_slice(&p);
        stdin.push(0);
    }
    let args = [
        "-c",
        &config,
        "check-attr",
        source,
        "-z",
        "--stdin",
        "export-ignore",
    ];
    let (status, out) = piped(root, &args, stdin)?;
    if status != 0 {
        return Ok(None);
    }
    let fields: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    Ok(Some(
        fields
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|[_, _, info]| *info == b"set")
            .map(|[path, _, _]| path.to_vec())
            .collect(),
    ))
}

/// The entries of `paths` that `export-ignore` drops, after the
/// `.gitattributes` in the index (git-utils.cc:GitExportIgnoreSourceAccessor
/// on a working tree). The index is what `ls-files` observes, so this query
/// is none.
fn export_ignored(
    trace: &Trace,
    root: &Path,
    paths: &HashSet<PathBuf>,
) -> Result<HashSet<PathBuf>, String> {
    let rel = paths
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok())
        .map(|p| p.as_os_str().as_bytes().to_vec());
    let dropped = check_export(trace, root, "--cached", rel)?
        .ok_or_else(|| format!("iets: `git check-attr` failed in '{}'", root.display()))?;
    Ok(dropped
        .iter()
        .map(|p| root.join(OsStr::from_bytes(p)))
        .collect())
}

/// The directories that hold `path`, `""` for the root first.
fn parents(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    let ends = path.iter().enumerate().filter(|(_, b)| **b == b'/');
    iter::once(&path[..0]).chain(ends.map(|(i, _)| &path[..i]))
}

fn attributes_in(dir: &[u8]) -> Vec<u8> {
    match dir {
        [] => b".gitattributes".to_vec(),
        d => [d, b"/.gitattributes"].concat(),
    }
}

/// git-utils.cc:GitExportIgnoreSourceAccessor at a revision asks libgit2
/// with `GIT_ATTR_CHECK_INCLUDE_COMMIT`, which reads the `.gitattributes`
/// of the working tree and of the index as well as the commit's. `git
/// check-attr --source` reads the commit's alone, so it answers the same
/// only when the other two hold the commit's files. The working tree's are
/// observed, and the index through `ls-files`.
fn same_attributes(
    trace: &Trace,
    root: &Path,
    dirs: &HashSet<Vec<u8>>,
    oids: &HashMap<Vec<u8>, Vec<u8>>,
    blobs: &HashMap<Vec<u8>, Vec<u8>>,
) -> Result<bool, String> {
    let ls = query(trace, root, Query::GitLsFiles)?;
    for entry in ls.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let path = &entry[tab + 1..];
        let dir = path.strip_suffix(b".gitattributes");
        let dir = dir.and_then(|d| match d {
            [] => Some(&d[..0]),
            d => d.strip_suffix(b"/"),
        });
        if dir.is_some_and(|d| dirs.contains(d)) {
            let oid = entry[..tab].split(|&b| b == b' ').nth(2);
            if oid != oids.get(path).map(Vec::as_slice) {
                return Ok(false);
            }
        }
    }
    for dir in dirs {
        let rel = attributes_in(dir);
        let on_disk = observe(trace, &root.join(OsStr::from_bytes(&rel)));
        if on_disk.as_ref() != blobs.get(&rel) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The tree of `rev`, as git.cc:getAccessorFromCommit reads a local
/// repository: every blob as it is stored, a submodule as an empty
/// directory, and with `export`, no entry that `export-ignore` drops.
fn tree_at(trace: &Trace, root: &Path, rev: &str, export: bool) -> Result<Option<Commit>, String> {
    let quiet = Trace::default();
    let commit = format!("{rev}^{{commit}}");
    if run(&quiet, root, &["cat-file", "-e", &commit])?.0 != 0 {
        return Ok(None);
    }
    let last_modified = of_rev(root, &["log", "-1", "--format=%ct", rev])?;
    let ls = git(&quiet, root, &["ls-tree", "-r", "-z", "--full-tree", rev])?;
    let mut listed = Vec::new();
    for entry in ls.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
            return Ok(None);
        };
        let mut fields = entry[..tab].split(|&b| b == b' ');
        let (Some(mode), Some(_), Some(oid)) = (fields.next(), fields.next(), fields.next()) else {
            return Ok(None);
        };
        listed.push((entry[tab + 1..].to_vec(), mode.to_vec(), oid.to_vec()));
    }
    let mut stdin = Vec::new();
    for (_, mode, oid) in &listed {
        if mode != b"160000" {
            stdin.extend_from_slice(oid);
            stdin.push(b'\n');
        }
    }
    let (status, out) = piped(root, &["cat-file", "--batch"], stdin)?;
    if status != 0 {
        return Ok(None);
    }
    let mut out = &out[..];
    let mut entries = Vec::with_capacity(listed.len());
    let mut oids = HashMap::new();
    let mut blobs = HashMap::new();
    for (path, mode, oid) in listed {
        if mode == b"160000" {
            entries.push((path, Entry::Empty));
            continue;
        }
        let Some(nl) = out.iter().position(|&b| b == b'\n') else {
            return Ok(None);
        };
        let size = out[..nl].rsplit(|&b| b == b' ').next();
        let size = size.and_then(|s| str::from_utf8(s).ok()?.parse::<usize>().ok());
        let Some(size) = size.filter(|s| nl + 1 + s < out.len()) else {
            return Ok(None);
        };
        let bytes = out[nl + 1..nl + 1 + size].to_vec();
        out = &out[nl + 2 + size..];
        if path.ends_with(b".gitattributes") {
            oids.insert(path.clone(), oid);
            blobs.insert(path.clone(), bytes.clone());
        }
        let e = match &mode[..] {
            b"100644" => Entry::File { bytes, exec: false },
            b"100755" => Entry::File { bytes, exec: true },
            b"120000" => Entry::Symlink(bytes),
            _ => return Ok(None),
        };
        entries.push((path, e));
    }
    let dirs: HashSet<Vec<u8>> = entries
        .iter()
        .flat_map(|(p, _)| parents(p))
        .map(<[u8]>::to_vec)
        .collect();
    if export {
        if !same_attributes(trace, root, &dirs, &oids, &blobs)? {
            return Ok(None);
        }
        let asked = entries.iter().map(|(p, _)| p.clone());
        let asked = asked.chain(dirs.iter().filter(|d| !d.is_empty()).cloned());
        let Some(dropped) = check_export(trace, root, &format!("--source={rev}"), asked)? else {
            return Ok(None);
        };
        let kept = |p: &[u8]| !dropped.contains(p) && !parents(p).any(|d| dropped.contains(d));
        entries.retain(|(p, _)| kept(p));
        // an export keeps a directory whose entries it all drops
        for d in dirs.iter().filter(|d| !d.is_empty() && kept(d)) {
            entries.push((d.clone(), Entry::Empty));
        }
    }
    Ok(Some(Commit {
        rev: rev.to_string(),
        last_modified,
        entries,
    }))
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

    fn export_ignored(
        &self,
        trace: &Trace,
        root: &Path,
        paths: &HashSet<PathBuf>,
    ) -> Result<HashSet<PathBuf>, String> {
        export_ignored(trace, root, paths)
    }

    /// git-utils.cc:resolveRef, which is `git_revparse_single`: git's own
    /// lookup of a name, peeled to the commit. The refs are observed whole.
    fn resolve(&self, trace: &Trace, root: &Path, name: &str) -> Result<Option<String>, String> {
        run(trace, root, &Query::GitRefs.argv()[1..])?;
        let commit = format!("{name}^{{commit}}");
        let args = ["rev-parse", "--verify", "-q", "--end-of-options", &commit];
        let (status, out) = run(&Trace::default(), root, &args)?;
        let rev = String::from_utf8_lossy(&out).trim().to_string();
        Ok((status == 0 && !rev.is_empty()).then_some(rev))
    }

    fn tree_at(
        &self,
        trace: &Trace,
        root: &Path,
        rev: &str,
        export: bool,
    ) -> Result<Option<Commit>, String> {
        tree_at(trace, root, rev, export)
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
    use crate::store::nar::tests::{Tree, tree};
    use crate::sys;
    use rstest::rstest;

    use std::path::Path;

    /// A path as the evaluator and a flake URL name it: `/C:/x` on Windows.
    fn internal(p: &Path) -> String {
        String::from_utf8(sys::path_to_internal(p)).expect("utf-8 path")
    }

    use std::fs;

    #[rstest]
    fn a_directory_in_a_repository_is_found(#[from(tree)] d: Tree) {
        fs::write(d.join("sub/flake.nix"), b"{ outputs = _: {}; }").unwrap();
        fs::create_dir_all(d.join(".git")).unwrap();
        let r = parse_flake_ref(&format!("{}/sub#x", internal(&d)), b"/", None).unwrap();
        let url = format!("git+file://{}?dir=sub", internal(&d));
        assert_eq!(crate::api::flake::url(&r), url);
        let r = parse_flake_ref(&format!("path:{}/sub", internal(&d)), b"/", None).unwrap();
        assert_eq!(r.source, SourceRef::Path(d.join("sub")));
    }
}
