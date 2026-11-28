//! Evaluator-side use of the pure `store` module: copying source paths to
//! the (virtual) store, and reading a virtual store path back from the
//! local tree it was copied from, from the spool of a fetched input, or from
//! memory.
//!
//! This is the one place that maps a store path to its bytes. Everything the
//! evaluator reads for a store path goes through [`locate`]: `import`,
//! `readFile`, `readDir`, `pathExists`, `readFileType`, `hashFile`,
//! `findFile`, `storePath`, `fetchTree`, `builtins.path`, `filterSource` and
//! string coercion. A read of a file goes through [`open`], which applies
//! the allow list of pure evaluation first. The answer is a [`SrcPath`],
//! one of:
//!
//! - the real file system;
//! - a local tree that a virtual copy stands for;
//! - a member of a fetched flake input, served out of its spool by offset
//!   (`crate::treestore`);
//! - a `toFile` text or a `.drv` that this evaluation holds in memory.
//!
//! The readers below record what they saw in the read set of the tracing
//! cache.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

use crate::parallel::force::require_demand;
use crate::sys::errno;
use crate::sys::{self, OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::str::from_utf8;
use std::sync::Arc;

use crate::compile::canon_path;
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::lint::{LintId, purity};
use crate::source::scm;
use crate::store::hash::{HashAlgo, Hasher};
use crate::store::nar::{FileType, PathFilter, dump_path, dump_string, nar_hash_filtered};
use crate::store::path::{StorePath, split_store_path};
use crate::store::source::{SourceOrigin, Src, SrcPath};
use crate::trace::{self, Kind, Obs};
use crate::treestore::{self, StoredTree};

/// Nix reads a copied tree from the store; iets never writes the store, so
/// a path under a store path this evaluation produced from a local tree
/// (`"${./x}"`, `builtins.path`, the flake root, a `path:` input) reads the
/// local tree instead, and a path under a fetched input reads that input's
/// spool. The content is the same by construction: the store path is the NAR
/// hash of exactly those bytes. `None` when `path` is under no such store
/// path.
pub fn locate(ev: &Evaluator, path: &[u8]) -> Option<SrcPath> {
    let (base, sub) = split_store_path(path)?;
    let sub = sub.strip_prefix(b"/").unwrap_or(sub);
    let base = from_utf8(base).ok()?;
    let sp = StorePath::from_base_name(base).ok()?;
    let at = match ev.known.origin(&sp) {
        Some(o) => o.at,
        None if base.ends_with(".drv") => SrcPath {
            src: Src::Mem(ev.drv.aterm(&sp)?),
            rel: Vec::new(),
        },
        None => return None,
    };
    // a vanished entry falls back to the store, which a build has usually filled by then
    if let Src::Spool { name, .. } = &at.src
        && treestore::get(name).is_none()
    {
        return None;
    }
    Some(at.join(sub))
}

/// [`locate`], else `path` itself on the real file system.
pub fn resolve(ev: &Evaluator, path: &[u8]) -> SrcPath {
    locate(ev, path).unwrap_or_else(|| SrcPath::fs(sys::path_from_internal(path), None))
}

/// Whether a pure evaluation may read `path` (eval.cc: rootFS is the store
/// behind an allow list): under a store path this evaluation added or a fetch
/// found on disk, or inside this run's project. Always true when impure.
/// The project is an allow list, not a copy: a path keeps naming the checkout,
/// so positions name the file the user edits. Only a path that reaches a
/// derivation depends on where the checkout is, and `impure-path-in-drv`
/// reports it (docs/explanation/flakes-and-sources.md).
pub fn allowed(ev: &Evaluator, path: &[u8]) -> bool {
    !ev.pure() || ev.known.under(path).is_some() || ev.in_project(path)
}

/// [`allowed`] for the path a read of `path` reaches: a path in the project
/// by its text is followed through its links and `..` unless the kernel shows
/// it stays there. `at` is where [`located`] put it.
pub fn admits(
    ev: &Evaluator,
    at: Option<&SrcPath>,
    path: &[u8],
    mode: Follow,
    pos: Pos,
) -> EvalResult<bool> {
    if !allowed(ev, path) {
        return Ok(false);
    }
    if at.is_some()
        || !ev.pure()
        || !ev.in_project(path)
        || ev.beneath_project(path, mode == Follow::Full)
    {
        return Ok(true);
    }
    Ok(allowed(ev, &resolve_links(ev, path, mode, pos)?))
}

/// Which components a resolution follows (source-accessor.hh:SymlinkResolution).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    /// Every component but the last: `pathExists`.
    Ancestors,
    /// Every component: `readFile`, `readDir`, `builtins.path`.
    Full,
}

/// Mirrors source-accessor.cc:resolveSymlinks. A store path this evaluation
/// made holds no bytes, so the kernel would resolve a link below it against the
/// local tree it stands for. Walking the text of the path instead puts every
/// target where the store copy would, and the tree's filter and [`allowed`]
/// judge the result. A link the filter hides, or one a pure evaluation may not
/// read, is left alone, as `maybeLstat` leaves it (filtering-source-accessor.cc).
pub fn resolve_links(ev: &Evaluator, path: &[u8], mode: Follow, pos: Pos) -> EvalResult<Vec<u8>> {
    let split = |p: &[u8], out: &mut Vec<Vec<u8>>| {
        out.extend(
            p.split(|&b| b == b'/')
                .filter(|c| !c.is_empty())
                .rev()
                .map(<[u8]>::to_vec),
        );
    };
    let mut todo = Vec::new();
    split(path, &mut todo);
    let mut res: Vec<u8> = Vec::with_capacity(path.len());
    let mut hops = 0u32;
    while let Some(c) = todo.pop() {
        if c == b"." {
            continue;
        }
        if c == b".." {
            res.truncate(res.iter().rposition(|&b| b == b'/').unwrap_or(0));
            continue;
        }
        res.push(b'/');
        res.extend_from_slice(&c);
        if mode == Follow::Ancestors && todo.is_empty() {
            break;
        }
        let Some(target) = resolve(ev, &res).read_link() else {
            continue;
        };
        if !allowed(ev, &res) {
            continue;
        }
        hops += 1;
        if hops > 1024 {
            bail!(
                ErrorKind::Eval,
                pos,
                "infinite symlink recursion in path '{}'",
                String::from_utf8_lossy(path)
            );
        }
        if target.starts_with(b"/") {
            res.clear();
        } else {
            res.truncate(res.iter().rposition(|&b| b == b'/').unwrap_or(0));
        }
        split(&target, &mut todo);
    }
    if res.is_empty() {
        res.push(b'/');
    }
    Ok(res)
}

/// Where a read of `path` goes once the symlinks `mode` names are resolved,
/// and its place in the (virtual) store, `None` for one the real file system
/// answers. [`resolve_links`] runs under a store path copied from a local
/// tree, and in a pure evaluation under the project, where the kernel would
/// follow a link out of an allow list judged on the text of the path.
/// Elsewhere the kernel or the spool resolves as the accessor would.
pub fn located(
    ev: &Evaluator,
    path: Vec<u8>,
    mode: Follow,
    pos: Pos,
) -> EvalResult<(Vec<u8>, Option<SrcPath>)> {
    let at = locate(ev, &path);
    let walk = match &at {
        Some(at) => matches!(at.src, Src::Fs { .. }),
        None => ev.pure() && ev.in_project(&path),
    };
    if !walk {
        return Ok((path, at));
    }
    let res = resolve_links(ev, &path, mode, pos)?;
    if res == path {
        return Ok((path, at));
    }
    if at.is_none() {
        // no tree observation covers a link of the project
        let full = sys::fs::canonicalize(&os(&path));
        ev.trace().canon(&path, full.ok().as_deref());
    }
    let at = locate(ev, &res);
    Ok((res, at))
}

/// `import` and a copy keep the path they were given, as
/// eval.cc:resolveExprPath does, and the kernel follows the links on the way.
/// In a pure evaluation the place the links `mode` names lead to must be one
/// the evaluation may read.
pub fn confine(ev: &Evaluator, path: &[u8], mode: Follow, pos: Pos) -> EvalResult<()> {
    if !ev.pure() {
        return Ok(());
    }
    let (res, at) = located(ev, path.to_vec(), mode, pos)?;
    if res != path {
        check(ev, at, &res, mode, pos)?;
    }
    Ok(())
}

/// `path` as a path under the root tree, when it is at or below the local
/// directory the root flake was copied from. The tree's filter then hides
/// what the copy does not hold, as it does for a read.
fn in_root_tree(ev: &Evaluator, path: &[u8]) -> Option<Vec<u8>> {
    let (sp, dir) = ev.known.root_tree()?;
    let dir = sys::path_to_internal(&dir);
    let dir: &[u8] = dir.strip_suffix(b"/").unwrap_or(&dir);
    let rest = path.strip_prefix(dir)?;
    if !rest.is_empty() && rest[0] != b'/' {
        return None;
    }
    let mut out = sp.to_full_string().into_bytes();
    out.extend_from_slice(rest);
    Some(out)
}

/// The source of a fetch that is on this machine: a `file:` URL, a `path`
/// input, a local repository. In a pure evaluation it is a read like any
/// other, so it goes through the same accessor: a path below the root
/// flake's directory reads through the root tree and its filter, every link
/// is resolved on the text of the path, and the allow list judges where that
/// ends. The path it ends at, and what holds its bytes
/// (docs/explanation/pure-eval-and-local-sources.md).
pub fn local_source(ev: &Evaluator, path: &[u8], pos: Pos) -> EvalResult<(Vec<u8>, SrcPath)> {
    if !ev.pure() {
        return Ok((path.to_vec(), fs_path(path)));
    }
    let path = canon_path(path);
    let path = in_root_tree(ev, &path).unwrap_or(path);
    let (path, at) = located(ev, path, Follow::Full, pos)?;
    let at = check(ev, at, &path, Follow::Full, pos)?;
    Ok((path, at))
}

/// [`local_source`] for a directory that a tool opens in place, such as a
/// repository. No copy can stand for it, so it must be a directory on disk
/// that the root tree or the project holds.
pub fn local_dir(ev: &Evaluator, path: &[u8], pos: Pos) -> EvalResult<PathBuf> {
    if !ev.pure() {
        return Ok(os(path));
    }
    let (res, at) = local_source(ev, path, pos)?;
    let on_disk = matches!(at.src, Src::Fs { .. }) && !at.hidden();
    match at.local() {
        Some(dir) if on_disk && (ev.in_project(&res) || in_root_copy(ev, &res)) => Ok(dir),
        _ => Err(forbidden(path, pos)),
    }
}

/// Whether `path` is the root tree's store path or lies below it.
fn in_root_copy(ev: &Evaluator, path: &[u8]) -> bool {
    let Some((sp, _)) = ev.known.root_tree() else {
        return false;
    };
    let root = sp.to_full_string();
    path.strip_prefix(root.as_bytes())
        .is_some_and(|rest| rest.is_empty() || rest[0] == b'/')
}

fn forbidden(path: &[u8], pos: Pos) -> Box<Error> {
    Error::boxed(
        ErrorKind::Eval,
        format!(
            "access to absolute path '{}' is forbidden in pure evaluation mode (use '--impure' to override)",
            String::from_utf8_lossy(path)
        ),
        pos,
    )
}

/// `path` on the real file system, for a path [`locate`] does not answer.
pub fn fs_path(path: &[u8]) -> SrcPath {
    SrcPath::fs(sys::path_from_internal(path), None)
}

/// A located path behind the pure-eval allow list, with the error Nix's
/// `AllowListSourceAccessor` throws: what every read of a file goes through.
/// `pathExists` (false, not an error) and a `.drv` import (read through the
/// store, as Nix does) are the readers that resolve the path themselves.
pub fn check(
    ev: &Evaluator,
    at: Option<SrcPath>,
    path: &[u8],
    mode: Follow,
    pos: Pos,
) -> EvalResult<SrcPath> {
    if admits(ev, at.as_ref(), path, mode, pos)? {
        let at = at.unwrap_or_else(|| fs_path(path));
        if at.hidden()
            && let Some(e) = untracked(&at, pos)
        {
            return Err(e);
        }
        return Ok(at);
    }
    Err(forbidden(path, pos))
}

/// [`check`] of `path` located afresh.
pub fn open(ev: &Evaluator, path: &[u8], pos: Pos) -> EvalResult<SrcPath> {
    check(ev, locate(ev, path), path, Follow::Full, pos)
}

/// git.cc:makeNotAllowedError, in Nix's words: a path on disk that a
/// working tree leaves out because its SCM does not track it.
fn untracked(at: &SrcPath, pos: Pos) -> Option<Box<Error>> {
    let Src::Fs { root, .. } = &at.src else {
        return None;
    };
    let rel = OsStr::from_bytes(&at.rel);
    let scm = scm::at(root)?;
    if fs::symlink_metadata(root.join(rel)).is_err() {
        return None;
    }
    let add = scm.untracked(root, rel)?;
    let (rel, root) = (String::from_utf8_lossy(&at.rel), root.display());
    let msg = format!(
        "Path '{rel}' in the repository \"{root}\" is not tracked by {}.\n\n\
         To make it visible to Nix, run:\n\n{add}",
        scm.title()
    );
    let mut e = Error::new(ErrorKind::Io, msg, pos);
    e.hint = Some(format!("run '{add}'"));
    Some(Box::new(e))
}

/// Copying a [`Src::Mem`] path again would need a source origin that
/// uploads a NAR from memory. It is refused instead, so classic mode
/// fall back to nix.
pub fn recopy_unsupported(path: &[u8], pos: Pos) -> Box<Error> {
    Error::boxed(
        ErrorKind::Unsupported,
        format!(
            "copying '{}' to the store again is not supported",
            String::from_utf8_lossy(path)
        ),
        pos,
    )
}

/// [`SrcPath::stat`] of `path`, `Missing` for any error.
pub fn stat(ev: &Evaluator, path: &[u8], follow: bool) -> Kind {
    resolve(ev, path).stat(ev, follow).unwrap_or(Kind::Missing)
}

/// What a [`SrcPath`] opens.
pub enum At<'a> {
    Disk(Vec<u8>),
    Spool(Arc<StoredTree>, &'a str),
    Mem(&'a [u8]),
}

/// A failed read: the verb Nix names the failed step with, and the error.
pub type Failed = (&'static str, io::Error);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    FileBuiltin,
    Import,
    Transport,
}

const STAT: &str = "getting status of";
const READ: &str = "reading file";

fn os(p: &[u8]) -> PathBuf {
    sys::path_from_internal(p)
}

fn read_disk(file: impl Read, size: u64, sized: bool) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(if sized { size } else { u64::MAX })
        .read_to_end(&mut bytes)?;
    if sized && (bytes.len() as u64) < size {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(bytes)
}

fn read_failure(error: io::Error, file: &fs::File, name: &[u8]) -> Failed {
    if error.kind() != io::ErrorKind::UnexpectedEof {
        return (READ, error);
    }
    #[cfg(target_os = "linux")]
    let path = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()));
    #[cfg(not(target_os = "linux"))]
    let path = {
        let _ = file;
        sys::fs::canonicalize(&os(name))
    };
    let path = path.unwrap_or_else(|_| os(name));
    let message = format!("unexpected end-of-file reading from {path:?}");
    (READ, io::Error::new(error.kind(), message))
}

fn enoent() -> io::Error {
    errno::not_found()
}

/// `strerror(ENOENT)` alone, the text `import`'s stat has always carried (no
/// Rust " (os error 2)" suffix).
fn missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "No such file or directory")
}

fn below(root: &Path, rel: &[u8]) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(OsStr::from_bytes(rel))
    }
}

impl SrcPath {
    /// The path on disk, for [`Src::Fs`].
    pub fn local(&self) -> Option<PathBuf> {
        let Src::Fs { root, .. } = &self.src else {
            return None;
        };
        Some(below(root, &self.rel))
    }

    fn filter(&self) -> Option<&HashSet<PathBuf>> {
        match &self.src {
            Src::Fs { accepted, .. } | Src::Spool { accepted, .. } => accepted.as_deref(),
            Src::Mem(_) => None,
        }
    }

    /// Whether the tree's filter kept `p`: a path on disk, or a member's
    /// path in its spool.
    pub fn accepts(&self, p: &Path) -> bool {
        self.filter().is_none_or(|accepted| accepted.contains(p))
    }

    /// Where a spool dump names this path: `/` plus its path in the entry.
    pub fn in_spool(&self) -> PathBuf {
        below(Path::new("/"), &self.rel)
    }

    /// `None` when the store copy has no such entry: the tree's filter left
    /// it out, or it lies below a file in memory.
    pub fn at(&self) -> Option<At<'_>> {
        match &self.src {
            Src::Fs { .. } => {
                let p = self.local()?;
                (self.rel.is_empty() || self.accepts(&p))
                    .then(|| At::Disk(sys::path_to_internal(&p)))
            }
            Src::Spool { name, accepted, .. } => {
                if matches!(accepted, Some(paths) if !paths.contains(&self.in_spool())) {
                    return None;
                }
                Some(At::Spool(treestore::get(name)?, name))
            }
            Src::Mem(b) => self.rel.is_empty().then_some(At::Mem(b)),
        }
    }

    pub fn hidden(&self) -> bool {
        self.at().is_none()
    }

    /// A symlink's target, `None` for anything else and for an entry the
    /// tree's filter hides. The tree's own observation covers every target in
    /// it, so nothing is recorded. A spool member is never one: the entry
    /// resolves its own links ([`crate::fetch::spool`]).
    pub fn read_link(&self) -> Option<Vec<u8>> {
        match self.at()? {
            At::Disk(p) => Some(sys::path_to_internal(&fs::read_link(os(&p)).ok()?)),
            At::Spool(..) | At::Mem(_) => None,
        }
    }

    /// `stat`, or `lstat` unless `follow`, recorded in the read set.
    pub fn stat(&self, ev: &Evaluator, follow: bool) -> io::Result<Kind> {
        match self.at().ok_or_else(missing)? {
            At::Disk(p) => {
                let r = trace::stat_metadata(&os(&p), follow);
                let kind = ev.trace().record_stat(&p, follow, &r);
                r.map(|_| kind)
            }
            At::Spool(tree, name) => {
                treestore::record(ev, &tree, name);
                let kind = tree.stat(&self.rel, follow).ok_or_else(missing)?;
                Ok(trace::nar_kind(kind))
            }
            At::Mem(_) => Ok(Kind::Regular),
        }
    }

    /// `import`'s stat: whether `/default.nix` is appended. A regular file on
    /// disk records nothing: its `File` observation checks the type.
    pub fn is_dir(&self, ev: &Evaluator) -> io::Result<bool> {
        if let Some(At::Disk(p)) = self.at() {
            let dir = fs::metadata(os(&p))?.is_dir();
            if dir {
                ev.trace().stat(&p, true, Kind::Directory);
            }
            return Ok(dir);
        }
        Ok(self.stat(ev, true)? == Kind::Directory)
    }

    /// A sized Nix source read requires exactly `st_size` bytes. A raw
    /// transport read streams to the end.
    pub fn read(&self, ev: &Evaluator, sized: bool) -> Result<Vec<u8>, Failed> {
        self.read_with(
            ev,
            if sized {
                ReadMode::FileBuiltin
            } else {
                ReadMode::Transport
            },
        )
    }

    pub(crate) fn read_import(&self, ev: &Evaluator) -> Result<Vec<u8>, Failed> {
        self.read_with(ev, ReadMode::Import)
    }

    fn read_with(&self, ev: &Evaluator, mode: ReadMode) -> Result<Vec<u8>, Failed> {
        let sized = mode == ReadMode::FileBuiltin;
        match self.at().ok_or((STAT, enoent()))? {
            At::Disk(p) => {
                let (meta, f) = if sized {
                    let meta = fs::metadata(os(&p)).map_err(|e| (STAT, e))?;
                    if !meta.is_file() {
                        return Err((STAT, errno::is_a_directory()));
                    }
                    (meta, fs::File::open(os(&p)).map_err(|e| (READ, e))?)
                } else {
                    let f = fs::File::open(os(&p)).map_err(|e| (READ, e))?;
                    (f.metadata().map_err(|e| (STAT, e))?, f)
                };
                // Imports can also read streams, including completion's FIFO.
                let sized = sized || mode == ReadMode::Import && meta.is_file();
                let bytes =
                    read_disk(&f, meta.len(), sized).map_err(|e| read_failure(e, &f, &p))?;
                ev.trace().file(&p, &meta, &bytes);
                Ok(bytes)
            }
            At::Spool(tree, name) => match tree.read(&self.rel) {
                // the type of the member is what the stat would have said
                Err(e) if e.kind() == io::ErrorKind::NotFound || errno::is_directory(&e) => {
                    Err((STAT, e))
                }
                r => {
                    treestore::record(ev, &tree, name);
                    r.map_err(|e| (READ, e))
                }
            },
            At::Mem(b) => Ok(b.to_vec()),
        }
    }

    /// A directory's entries, recorded in the read set; `ENOTDIR` for
    /// anything else. The listing hides entries the tree's filter left out
    /// (untracked files of a git tree); the `Dir` observation keeps the full
    /// listing, the one validation can re-read.
    pub fn read_dir(&self, ev: &Evaluator) -> Result<Vec<(Vec<u8>, FileType)>, Failed> {
        let filter = self.filter();
        let notdir = || (STAT, errno::not_a_directory());
        match self.at().ok_or((STAT, enoent()))? {
            At::Disk(p) => {
                let rd = fs::read_dir(os(&p)).map_err(|e| ("opening directory", e))?;
                let mut out = Vec::new();
                let mut listing = Vec::new();
                for e in rd {
                    let e = e.map_err(|e| ("reading directory", e))?;
                    let t = FileType::of(e.file_type().map_err(|e| (STAT, e))?);
                    let name = e.file_name().into_vec();
                    if ev.trace().is_on() {
                        listing.push((name.clone(), trace::nar_kind(t)));
                    }
                    if filter.is_none_or(|accepted| accepted.contains(&e.path())) {
                        out.push((name, t));
                    }
                }
                if ev.trace().is_on() {
                    ev.trace().record(Obs::Dir {
                        path: p,
                        hash: trace::dir_listing_hash(&mut listing),
                    });
                }
                Ok(out)
            }
            At::Spool(tree, name) => {
                let entries = tree.list(&self.rel).map_err(|e| (STAT, e))?;
                treestore::record(ev, &tree, name);
                let Some(accepted) = filter else {
                    return Ok(entries);
                };
                let here = self.in_spool();
                Ok(entries
                    .into_iter()
                    .filter(|(n, _)| accepted.contains(&here.join(OsStr::from_bytes(n))))
                    .collect())
            }
            At::Mem(_) => Err(notdir()),
        }
    }

    /// The NAR of this path, `filter` asked about every entry below it: on
    /// disk under its path on disk, in a spool under `/`. The tree's own
    /// filter is the caller's to apply ([`SrcPath::accepts`]), because a dump
    /// that records the tree must see what the filter hides.
    pub fn dump(&self, sink: &mut dyn io::Write, filter: &mut PathFilter<'_>) -> io::Result<()> {
        match &self.src {
            Src::Fs { root, .. } => dump_path(&below(root, &self.rel), sink, filter),
            Src::Spool { name, .. } => treestore::get(name)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "the input store no longer holds '{name}'; evaluate again to fetch it"
                    ))
                })?
                .dump(&self.rel, &self.in_spool(), sink, filter),
            Src::Mem(b) => dump_string(b, sink),
        }
    }

    /// The NAR of the store copy: under the tree's own filter.
    pub fn nar(&self, sink: &mut dyn io::Write) -> io::Result<()> {
        self.dump(sink, &mut |p, _| self.accepts(p))
    }
}

/// Store path a source path would get when copied (`"${./foo}"`). Nothing
/// is written; the NAR hash is what `fetchToStore` would compute.
pub fn copy_path_to_store(
    ev: &mut Evaluator,
    path: &'static [u8],
    pos: Pos,
) -> EvalResult<&'static [u8]> {
    if let Some(sp) = ev.src_to_store.get(path) {
        return Ok(sp);
    }
    // Only the demand path can publish a new origin. Already published
    // entries above remain available to helpers.
    require_demand(ev)?;
    // the entry is copied as it is, a link included, from where the links
    // above it lead, under a copied tree too
    confine(ev, path, Follow::Ancestors, pos)?;
    let at = check(ev, locate(ev, path), path, Follow::Ancestors, pos)?;
    let copy_err = |e: io::Error| {
        Error::boxed(
            ErrorKind::Io,
            format!(
                "copying '{}' to the store: {}",
                String::from_utf8_lossy(path),
                e
            ),
            pos,
        )
    };
    let name = Path::new(OsStr::from_bytes(path))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "source".into());
    let fixed = |hash| {
        treestore::store_path(&name, hash)
            .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))
    };
    let (sp, origin, hash, nar_size) = match at.at() {
        None => return Err(copy_err(enoent())),
        Some(At::Mem(_)) => return Err(recopy_unsupported(path, pos)),
        // the top is a store path, which fetchToStore copies again under its
        // base name, as it does once the tree is on disk
        Some(At::Spool(tree, base))
            if at.rel.is_empty() && matches!(at.src, Src::Spool { accepted: None, .. }) =>
        {
            treestore::record(ev, &tree, base);
            let hash = tree.nar_hash();
            (fixed(&hash)?, at.clone(), hash, tree.nar_size())
        }
        Some(At::Spool(tree, base)) => {
            treestore::record(ev, &tree, base);
            let mut h = Hasher::new(HashAlgo::Sha256);
            at.nar(&mut h).map_err(copy_err)?;
            let size = h.bytes_written();
            let hash = h.finish();
            (fixed(&hash)?, at.clone(), hash, size)
        }
        Some(At::Disk(local)) => {
            let local = os(&local);
            let accepted = match &at.src {
                Src::Fs { accepted, .. } => accepted.clone(),
                _ => None,
            };
            // a plain local directory, not a tree the flake machinery filtered
            if ev.lint_on(LintId::DirCopyUnfiltered)
                && accepted.is_none()
                && !ev.is_store_path_text(path)
                && ev.trace().stat_now(local.as_os_str().as_bytes(), true) == Kind::Directory
            {
                purity::dir_copy_unfiltered(ev, path, pos)?;
            }
            let git_lint = ev.lint_on(LintId::GitInSource);
            let mut saw_git = false;
            let mut fp = ev.trace().fp(&local);
            let mut filter = |p: &Path, t: FileType| {
                let keep = at.accepts(p);
                if let Some(fp) = &mut fp {
                    fp.entry(p, t, keep);
                }
                if keep && git_lint && is_git_entry(&local, p) {
                    saw_git = true;
                }
                keep
            };
            let (hash, nar_size) =
                nar_hash_filtered(&local, HashAlgo::Sha256, &mut filter).map_err(copy_err)?;
            ev.trace().tree(
                &local,
                trace::hash_to_digest(&hash),
                accepted.as_deref(),
                fp,
            );
            if saw_git {
                purity::git_in_source(ev, path, pos)?;
            }
            purity::large_source_copy(ev, path, nar_size, pos)?;
            (fixed(&hash)?, SrcPath::fs(local, accepted), hash, nar_size)
        }
    };
    let full = ev.arena.alloc_bytes(sp.to_full_string().as_bytes());
    ev.note_store_add(&sp, "tree", nar_size, &origin, pos);
    let origin_on_disk = origin.local().is_some();
    let o = SourceOrigin::tree(origin, hash, nar_size);
    if origin_on_disk {
        ev.known.add_copy(sp, o);
    } else {
        ev.known.add(sp, o);
    }
    // A cache hit can immediately read through the virtual store path.
    ev.src_to_store.insert(path.to_vec(), full);
    Ok(full)
}

/// `p` is the `.git` entry at the top of the tree rooted at `root`.
pub fn is_git_entry(root: &Path, p: &Path) -> bool {
    p.file_name().is_some_and(|n| n == ".git") && p.parent() == Some(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::eval::EvalConfig;
    use crate::store::hash::Hash;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn filtered_directory_observes_the_full_listing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("keep"), b"a").unwrap();
        fs::write(dir.path().join("hide"), b"b").unwrap();
        let ev = EvalConfig::new().trace(true).build();
        let source = SrcPath::fs(
            dir.path(),
            Some(Arc::new(HashSet::from([dir.path().join("keep")]))),
        );
        assert_eq!(
            source.read_dir(&ev).unwrap(),
            vec![(b"keep".to_vec(), FileType::Regular)]
        );
        assert_eq!(
            ev.trace().take(),
            vec![Obs::Dir {
                path: sys::path_to_internal(dir.path()),
                hash: trace::dir_hash(dir.path(), &mut trace::DirBuf::default()).unwrap(),
            }]
        );
    }

    #[test]
    fn virtual_tree_reads_go_to_the_local_files() {
        let ev = Evaluator::new();
        let root = PathBuf::from("/r");
        let sp = StorePath::parse("/nix/store/z4wkdlk69gck09s64a8ghpn6h7bxhkyy-source").unwrap();
        let accepted: HashSet<PathBuf> = ["/r/flake.nix", "/r/a", "/r/a/b.nix"]
            .map(PathBuf::from)
            .into();
        ev.known.add(
            sp.clone(),
            SourceOrigin::tree(
                SrcPath::fs(root, Some(Arc::new(accepted))),
                Hash::from_bytes(HashAlgo::Sha256, &[0u8; 32]).unwrap(),
                0,
            ),
        );
        let on_disk = |p: &[u8]| match resolve(&ev, p).at() {
            Some(At::Disk(p)) => Some(p),
            _ => None,
        };
        let full = sp.to_full_string();
        assert_eq!(on_disk(full.as_bytes()).unwrap(), b"/r");
        assert_eq!(
            on_disk(format!("{full}/a/b.nix").as_bytes()).unwrap(),
            b"/r/a/b.nix"
        );
        assert!(on_disk(format!("{full}/untracked").as_bytes()).is_none());
        assert!(on_disk(format!("{full}/.git/HEAD").as_bytes()).is_none());
        let other = b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-other/x";
        assert_eq!(on_disk(other).unwrap(), other);
        assert_eq!(on_disk(b"/etc/passwd").unwrap(), b"/etc/passwd");
        assert!(locate(&ev, b"/nix/store/not-a-store-path").is_none());
    }

    #[test]
    fn a_copy_cache_hit_requires_a_registered_origin() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("source");
        fs::write(&file, b"copied bytes").unwrap();
        let mut writer = Evaluator::new();
        let path = writer.arena.alloc_bytes(&sys::path_to_internal(&file));
        let mut reader = writer.clone();
        let full = copy_path_to_store(&mut writer, path, Pos::NONE).unwrap();
        assert_eq!(
            copy_path_to_store(&mut reader, path, Pos::NONE).unwrap(),
            full
        );
        assert_eq!(reader.nr_store_adds, 0);
        assert_eq!(
            resolve(&reader, full).read(&reader, true).unwrap(),
            b"copied bytes"
        );

        // Make registration fail at its lock on a fresh pair of evaluators.
        let mut writer = Evaluator::new();
        let mut reader = writer.clone();
        let known = writer.known.clone();
        catch_unwind(AssertUnwindSafe(|| {
            let _guard = known.lock();
            panic!("poison the origin index");
        }))
        .unwrap_err();
        assert!(
            catch_unwind(AssertUnwindSafe(|| copy_path_to_store(
                &mut writer,
                path,
                Pos::NONE
            )))
            .is_err()
        );
        assert_eq!(writer.nr_store_adds, 1, "the copy reached registration");
        // A miss must now report the missing source. An early cache publication
        // would let the cloned reader return the store path despite the failure.
        fs::remove_file(file).unwrap();
        assert!(copy_path_to_store(&mut reader, path, Pos::NONE).is_err());
    }

    #[test]
    fn a_relative_path_inside_an_entry_joins_once() {
        let rel = |rel: &[u8], sub: &[u8]| SrcPath::spool("x", rel).join(sub).rel;
        assert_eq!(rel(b"", b""), b"");
        assert_eq!(rel(b"lib", b""), b"lib");
        assert_eq!(rel(b"", b"lib"), b"lib");
        assert_eq!(rel(b"lib", b"x.nix"), b"lib/x.nix");
    }

    #[test]
    fn sized_source_reads_require_the_reported_byte_count() {
        let src = b"42".as_slice();
        let read = |size| read_disk(src, size, true);
        assert_eq!(read(3).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read(2).unwrap(), src);
        assert_eq!(read(1).unwrap(), b"4");
        assert!(read(0).unwrap().is_empty());
        assert_eq!(read_disk(src, 0, false).unwrap(), src);
    }
}
