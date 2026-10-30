//! A local tree. The working tree of an SCM is copied the way
//! git.cc:getAccessorFromWorkdir does it: the files the SCM tracks
//! (`scm`), with the working tree contents and modes; nothing untracked.
//! A plain directory is copied whole (path.cc). The copy is virtual: the
//! store path is computed from the NAR hash and registered as a
//! `SourceOrigin`, so reads under it go to the working tree
//! (`store_hooks::locate`); nothing is fetched and the store is never
//! consulted. The SCM records its own observations and the copied file set
//! is a `Tree` observation, so the tracing cache follows the tree.

use crate::sys::fs::mtime_secs;
use crate::sys::{self, OsStrExt};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use super::scm::Scm;
use super::{AttrVal, Attrs, Tree, store_commit};
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos};
use crate::srcpath::{SourceOrigin, SrcPath};
use crate::store::hash::{Hash, HashAlgo};
use crate::store::nar;
use crate::store::path::StorePath;
use crate::trace::{self, TreeFp};
use crate::treestore;

/// A directory copied whole, as a `path:` input.
pub fn path_source(ev: &mut Evaluator, dir: &Path, pos: Pos) -> EvalResult<Tree> {
    let (hash, mtime) = path_tree(ev, dir, None, pos)?.expect("no hash to match");
    let mut attrs = Attrs::new();
    attrs.insert("type".into(), AttrVal::Str("path".into()));
    attrs.insert(
        "path".into(),
        AttrVal::Str(dir.to_string_lossy().into_owned()),
    );
    attrs.insert("narHash".into(), AttrVal::Str(hash.to_sri()));
    attrs.insert("lastModified".into(), AttrVal::Int(mtime.max(0) as u64));
    let path = treestore::store_path("source", &hash)
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
    Ok(Tree {
        path,
        attrs,
        subdir: String::new(),
        counter: None,
    })
}

/// NAR hash of a whole directory and the newest `lstat` mtime in it. A tree
/// that is not `expect` is not made readable, and `Err` has its hash.
pub fn path_tree(
    ev: &mut Evaluator,
    dir: &Path,
    expect: Option<&Hash>,
    pos: Pos,
) -> EvalResult<Result<(Hash, i64), Hash>> {
    let io = |e: io::Error| {
        Error::boxed(
            ErrorKind::Io,
            format!("copying '{}' to the store: {e}", dir.display()),
            pos,
        )
    };
    let mut fp = ev.trace().fp(dir);
    let (hash, nar_size, mtime) = dump_and_max_mtime(dir, &mut fp).map_err(io)?;
    if expect.is_some_and(|e| *e != hash) {
        return Ok(Err(hash));
    }
    if names_its_content(dir, &hash) {
        ev.trace()
            .stat(dir.as_os_str().as_bytes(), false, trace::Kind::Directory);
    } else {
        ev.trace()
            .tree_dated(dir, trace::hash_to_digest(&hash), mtime, fp);
    }
    let sp = treestore::store_path("source", &hash)
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
    ev.known.add(
        sp,
        SourceOrigin::tree(SrcPath::fs(dir, None), hash, nar_size),
    );
    Ok(Ok((hash, mtime)))
}

/// A store path that is the fixed-output path of the tree just hashed: a
/// locked input that is already in `/nix/store`. Its name is its NAR hash, so
/// the tracing cache checks that it is still there and walks none of it, as
/// it does for an input store entry (docs/explanation/caching.md). Any other
/// directory, in the store or not, keeps its `Tree` observation.
fn names_its_content(dir: &Path, hash: &Hash) -> bool {
    let Some(p) = dir.to_str().and_then(|s| StorePath::parse(s).ok()) else {
        return false;
    };
    treestore::store_path(p.name(), hash).is_ok_and(|q| q == p)
}

/// One walk for both answers, like `dumpPathAndGetMtime`: the dump already reaches every
/// entry in the tree, so the newest `lstat` mtime comes from its filter callback instead of a
/// second recursive `read_dir`.
fn dump_and_max_mtime(dir: &Path, fp: &mut Option<TreeFp>) -> io::Result<(Hash, u64, i64)> {
    let mut mtime = mtime_secs(&fs::symlink_metadata(dir)?);
    let mut filter = |p: &Path, t: nar::FileType| {
        if let Ok(st) = fs::symlink_metadata(p) {
            mtime = mtime.max(mtime_secs(&st));
        }
        if let Some(fp) = fp.as_mut() {
            fp.entry(p, t, true);
        }
        true
    };
    let (hash, nar_size) = nar::nar_hash_filtered(dir, HashAlgo::Sha256, &mut filter)?;
    Ok((hash, nar_size, mtime))
}

/// The filter set for the NAR dump: every tracked file and the directories
/// above it (CanonPath::isAllowed on the allow-list accessor).
pub fn accepted_set(root: &Path, files: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    let mut acc = HashSet::with_capacity(files.len() * 2);
    for f in files {
        let mut p = f.as_path();
        while p != root && acc.insert(p.to_path_buf()) {
            match p.parent() {
                Some(q) => p = q,
                None => break,
            }
        }
    }
    acc
}

/// The working tree of an SCM, with the flake's `subdir` carried through.
pub fn tracked_source(
    ev: &mut Evaluator,
    scm: &'static dyn Scm,
    root: &Path,
    subdir: &str,
    pos: Pos,
) -> EvalResult<Tree> {
    let (mut tree, _) = workdir(ev, scm, root, "source", false, pos)?;
    tree.subdir = subdir.to_string();
    Ok(tree)
}

/// The tree of `rev` in the repository at `root`
/// (git.cc:getAccessorFromCommit), under the store `name`, without what
/// `Scm::export_ignored` drops when `export` is set. `None` when the
/// repository has not got the revision.
pub fn commit_tree(
    ev: &mut Evaluator,
    scm: &'static dyn Scm,
    root: &Path,
    rev: &str,
    name: &str,
    export: bool,
    pos: Pos,
) -> EvalResult<Option<Tree>> {
    let commit = scm
        .tree_at(ev.trace(), root, rev, export)
        .map_err(|e| Error::boxed(ErrorKind::Io, e, pos))?;
    let Some(commit) = commit else {
        return Ok(None);
    };
    let last_modified = commit.last_modified;
    let (path, hash) = store_commit(ev, name, commit, pos)?;
    Ok(Some(Tree {
        path,
        attrs: Attrs::from([
            ("type".into(), AttrVal::Str(scm.name().into())),
            (
                "url".into(),
                AttrVal::Str(format!(
                    "file://{}",
                    String::from_utf8_lossy(&sys::path_to_internal(root))
                )),
            ),
            ("narHash".into(), AttrVal::Str(hash.to_sri())),
            ("rev".into(), AttrVal::Str(rev.to_string())),
            ("lastModified".into(), AttrVal::Int(last_modified)),
            ("submodules".into(), AttrVal::Bool(false)),
        ]),
        subdir: String::new(),
        counter: Some((scm, root.to_path_buf())),
    }))
}

/// [`commit_tree`] for a flake root, with its `subdir`. The revision has to
/// be there: a root flake is never fetched.
pub fn commit_source(
    ev: &mut Evaluator,
    scm: &'static dyn Scm,
    root: &Path,
    rev: &str,
    subdir: &str,
    pos: Pos,
) -> EvalResult<Tree> {
    let Some(mut tree) = commit_tree(ev, scm, root, rev, "source", false, pos)? else {
        return Err(Error::boxed(
            ErrorKind::Io,
            format!(
                "iets: the {} repository at '{}' has no revision {rev}; \
                 check it out and name the directory instead",
                scm.title(),
                root.display()
            ),
            pos,
        ));
    };
    tree.subdir = subdir.to_string();
    Ok(tree)
}

/// The working tree of an SCM under the store name `name`, without what
/// `Scm::export_ignored` drops when `export` is set, and whether it is dirty.
pub fn workdir(
    ev: &mut Evaluator,
    scm: &'static dyn Scm,
    root: &Path,
    name: &str,
    export: bool,
    pos: Pos,
) -> EvalResult<(Tree, bool)> {
    let at = scm
        .snapshot(ev.trace(), root)
        .map_err(|e| Error::boxed(ErrorKind::Io, e, pos))?;
    let mut accepted = accepted_set(root, &at.files);
    if export {
        let dropped = scm
            .export_ignored(ev.trace(), root, &accepted)
            .map_err(|e| Error::boxed(ErrorKind::Io, e, pos))?;
        accepted.retain(|p| {
            !p.ancestors()
                .take_while(|a| *a != root)
                .any(|a| dropped.contains(a))
        });
    }
    let mut fp = ev.trace().fp(root);
    let mut filter = |p: &Path, t: nar::FileType| {
        let keep = accepted.contains(p);
        if let Some(fp) = &mut fp {
            fp.entry(p, t, keep);
        }
        keep
    };
    let (hash, nar_size) =
        nar::nar_hash_filtered(root, HashAlgo::Sha256, &mut filter).map_err(|e| {
            Error::boxed(
                ErrorKind::Io,
                format!("copying '{}' to the store: {e}", root.display()),
                pos,
            )
        })?;
    ev.trace()
        .tree_external(root, trace::hash_to_digest(&hash), &accepted, fp);
    let path = treestore::store_path(name, &hash)
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
    ev.known.add(
        path.clone(),
        SourceOrigin::tree(SrcPath::fs(root, Some(Arc::new(accepted))), hash, nar_size),
    );
    let mut attrs = Attrs::new();
    attrs.insert("type".into(), AttrVal::Str(scm.name().into()));
    attrs.insert(
        "url".into(),
        AttrVal::Str(format!(
            "file://{}",
            String::from_utf8_lossy(&sys::path_to_internal(root))
        )),
    );
    attrs.insert("narHash".into(), AttrVal::Str(hash.to_sri()));
    attrs.insert("lastModified".into(), AttrVal::Int(at.last_modified));
    // fetchTree.cc:emitTreeAttrs: a dirty tree shows no count
    let counts = at.countable && !at.dirty && at.rev.is_some();
    match at.rev {
        Some(h) if at.dirty => {
            attrs.insert(
                "dirtyShortRev".into(),
                AttrVal::Str(format!("{}-dirty", &h[..h.len().min(7)])),
            );
            attrs.insert("dirtyRev".into(), AttrVal::Str(h + "-dirty"));
        }
        Some(h) => {
            attrs.insert("rev".into(), AttrVal::Str(h));
        }
        None => {}
    }
    let tree = Tree {
        path,
        attrs,
        subdir: String::new(),
        counter: counts.then(|| (scm, root.to_path_buf())),
    };
    Ok((tree, at.dirty))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, SystemTime};

    use crate::store::hash;
    use crate::store::nar::tests::{Tree, tree};
    use rstest::rstest;

    /// Only the fixed-output path of the hash itself skips the walk: another
    /// hash under the same name, another name, a path below it and a
    /// directory outside the store all keep their `Tree` observation.
    #[test]
    fn only_a_content_addressed_store_path_names_its_content() {
        let hash = hash::hash_bytes(HashAlgo::Sha256, b"a tree");
        let other = hash::hash_bytes(HashAlgo::Sha256, b"another tree");
        let p = treestore::store_path("source", &hash).unwrap().to_string();
        assert!(names_its_content(Path::new(&p), &hash));
        assert!(!names_its_content(Path::new(&p), &other));
        assert!(!names_its_content(Path::new(&format!("{p}x")), &hash));
        assert!(!names_its_content(Path::new(&format!("{p}/sub")), &hash));
        assert!(!names_its_content(Path::new("/home/u/source"), &hash));
    }

    #[test]
    fn accepted_set_adds_parent_directories() {
        let root = PathBuf::from("/r");
        let files: HashSet<PathBuf> = ["/r/a/b/c.nix", "/r/d.nix"].map(PathBuf::from).into();
        let acc = accepted_set(&root, &files);
        let mut v: Vec<&str> = acc.iter().map(|p| p.to_str().unwrap()).collect();
        v.sort();
        assert_eq!(v, ["/r/a", "/r/a/b", "/r/a/b/c.nix", "/r/d.nix"]);
    }

    #[rstest]
    fn max_mtime_covers_root_and_children(#[from(tree)] d: Tree) {
        let f = d.join("newest");
        fs::write(&f, b"x").unwrap();
        let later = SystemTime::now() + Duration::from_secs(1000);
        fs::OpenOptions::new()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let want = mtime_secs(&fs::metadata(&f).unwrap());
        let (hash, size, mtime) = dump_and_max_mtime(&d, &mut None).unwrap();
        assert_eq!(mtime, want);
        assert_eq!(hash, nar::nar_hash(&d).unwrap());
        assert_eq!(size, nar::nar_size(&d).unwrap());
    }
}
