//! Tracing evaluation cache: the read set an evaluation records, its
//! validation against the file system, and the on-disk entry. Design:
//! docs/explanation/caching.md.

pub mod key;
pub mod run;
pub mod store;
pub mod validate;
pub mod watch;
pub mod wire;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::env::var_os;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{fs, io, mem};

use crate::cachefs;
use crate::store::hash::{Hash, HashAlgo};
use crate::store::nar::{self, FileType};
use sha2::{Digest, Sha256};

pub type Digest32 = [u8; 32];

/// What a stat saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Missing,
    Regular,
    Directory,
    Symlink,
    Other,
}

impl Kind {
    /// The kind a `FileType` names. `of` takes the same decision from a
    /// `Metadata`; a directory entry already carries the type, so a listing
    /// needs no second `stat`.
    pub fn of_file_type(ft: &fs::FileType) -> Kind {
        if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_dir() {
            Kind::Directory
        } else if ft.is_file() {
            Kind::Regular
        } else {
            Kind::Other
        }
    }

    pub fn of(r: &io::Result<fs::Metadata>) -> Kind {
        match r {
            Ok(m) => {
                let ft = m.file_type();
                if ft.is_symlink() {
                    Kind::Symlink
                } else if ft.is_dir() {
                    Kind::Directory
                } else if ft.is_file() {
                    Kind::Regular
                } else {
                    Kind::Other
                }
            }
            Err(_) => Kind::Missing,
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(b: u8) -> Option<Kind> {
        Some(match b {
            0 => Kind::Missing,
            1 => Kind::Regular,
            2 => Kind::Directory,
            3 => Kind::Symlink,
            4 => Kind::Other,
            _ => return None,
        })
    }
}

/// The filter result of a `filterSource`/`builtins.path` copy: every entry
/// the filter was asked about (hashed) and the ones it accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filtered {
    /// `None` when the accepted set came from outside the tree (a git
    /// index), so entries the set does not name cannot change the result.
    pub seen_hash: Option<Digest32>,
    /// Absolute paths, sorted.
    pub accepted: Vec<Vec<u8>>,
}

/// One observation of the outside world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Obs {
    File {
        path: Vec<u8>,
        size: u64,
        mtime_ns: i64,
        /// Changes on every write, and no user program can set it: without
        /// it a `cp -p` in place over a file of the same length is a hit.
        ctime_ns: i64,
        dev: u64,
        ino: u64,
        hash: Digest32,
    },
    Dir {
        path: Vec<u8>,
        hash: Digest32,
    },
    Stat {
        path: Vec<u8>,
        follow: bool,
        kind: Kind,
    },
    Tree {
        root: Vec<u8>,
        nar_hash: Digest32,
        stat_hash: Digest32,
        filtered: Option<Filtered>,
        /// The newest `lstat` mtime in the tree, in seconds, when the
        /// evaluation saw it: a `path:` input's `lastModified`. A tree with
        /// the same bytes and another date is then another answer.
        newest: Option<i64>,
    },
    Canon {
        path: Vec<u8>,
        target: Option<Vec<u8>>,
    },
    /// `value` is a keyed hash, not the value: the observation needs only to
    /// know whether the value changed, and an entry that held the answer of
    /// every `getEnv` in clear would be a file of the user's secrets.
    Env {
        name: Vec<u8>,
        value: Option<Digest32>,
    },
    /// A query of an SCM asked in `dir`; `hash` covers its exit status and
    /// stdout. The entry names the query, never the command line it is.
    Cmd {
        dir: Vec<u8>,
        query: Query,
        hash: Digest32,
    },
    /// One entry of the input store read during the evaluation
    /// (`crate::treestore`), whatever was read out of it. The entry's name is the
    /// NAR hash of its tree, so nothing about the bytes can change under a
    /// name that is still there: the check is that the store still holds the
    /// entry and that its index is a version this binary reads.
    Input {
        entry: Vec<u8>,
        version: u8,
    },
}

impl Obs {
    /// Identity for deduplication: kind and subject, not the observed value.
    fn dedupe_key(&self) -> (u8, Cow<'_, [u8]>, u8) {
        match self {
            Obs::File { path, .. } => (0, Cow::Borrowed(path), 0),
            Obs::Dir { path, .. } => (1, Cow::Borrowed(path), 0),
            Obs::Stat { path, follow, .. } => (2, Cow::Borrowed(path), u8::from(*follow)),
            Obs::Tree { root, filtered, .. } => {
                (3, Cow::Borrowed(root), u8::from(filtered.is_some()))
            }
            Obs::Canon { path, .. } => (4, Cow::Borrowed(path), 0),
            Obs::Env { name, .. } => (5, Cow::Borrowed(name), 0),
            Obs::Cmd { dir, query, .. } => (6, Cow::Borrowed(dir), query.to_u8()),
            Obs::Input { entry, .. } => (7, Cow::Borrowed(entry), 0),
        }
    }
}

/// The queries of a working tree that a run may record. An entry holds the
/// query, and validation builds its command line from this table, so a cache
/// directory a stranger can write names no program of its own. `Scm`
/// implementations reach it through [`Trace::cmd`], which refuses a command
/// the table does not hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Query {
    GitLsFiles,
    GitStatus,
    GitHead,
}

impl Query {
    /// Every permitted query, in the order that numbers their tags.
    pub const ALL: [Query; 3] = [Query::GitLsFiles, Query::GitStatus, Query::GitHead];

    /// The `Scm::name` of the system this query belongs to.
    pub fn scm(self) -> &'static str {
        match self {
            Query::GitLsFiles | Query::GitStatus | Query::GitHead => "git",
        }
    }

    /// `--no-optional-locks` on every query: evaluation writes nothing to a
    /// repository. Without it `git status` persists its refreshed stat cache,
    /// which rewrites `.git/index` whenever any worktree file's stat moved —
    /// an untracked one is enough. The output is byte-identical either way,
    /// but direnv watches that file, so a build would reload the shell for an
    /// index whose content did not change.
    pub fn argv(self) -> &'static [&'static str] {
        match self {
            Query::GitLsFiles => &[
                "git",
                "--no-optional-locks",
                "ls-files",
                "-z",
                "--cached",
                "--stage",
                "-v",
            ],
            Query::GitStatus => &[
                "git",
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=no",
                "--no-renames",
                "--ignore-submodules=all",
            ],
            Query::GitHead => &[
                "git",
                "--no-optional-locks",
                "rev-parse",
                "--verify",
                "-q",
                "HEAD",
            ],
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Query::GitLsFiles => 0,
            Query::GitStatus => 1,
            Query::GitHead => 2,
        }
    }

    pub fn from_u8(b: u8) -> Option<Query> {
        Query::ALL.get(usize::from(b)).copied()
    }

    /// The query `argv` is, or `None` for a command line this table does not
    /// hold: its answer is then no observation and the run is uncacheable.
    fn of(argv: &[Vec<u8>]) -> Option<Query> {
        Query::ALL.into_iter().find(|q| {
            let want = q.argv();
            want.len() == argv.len() && want.iter().zip(argv).all(|(a, b)| a.as_bytes() == &b[..])
        })
    }
}

/// Runs `argv` in `dir` and returns its exit status and stdout; stderr is
/// inherited.
fn run_cmd(dir: &Path, argv: &[Vec<u8>]) -> io::Result<(i32, Vec<u8>)> {
    let out = Command::new(OsStr::from_bytes(&argv[0]))
        .args(argv[1..].iter().map(|a| OsStr::from_bytes(a)))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()?;
    Ok((out.status.code().unwrap_or(-1), out.stdout))
}

/// Asks `q` in `dir` again, for validation. The argv comes from the table,
/// never from the entry.
pub fn run_query(dir: &Path, q: Query) -> io::Result<(i32, Vec<u8>)> {
    let argv: Vec<Vec<u8>> = q.argv().iter().map(|a| a.as_bytes().to_vec()).collect();
    run_cmd(dir, &argv)
}

pub fn cmd_hash(status: i32, stdout: &[u8]) -> Digest32 {
    let mut h = Sha256::new();
    h.update(status.to_le_bytes());
    h.update(stdout);
    h.finalize().into()
}

struct Inner {
    obs: Mutex<Vec<Obs>>,
    uncacheable: AtomicBool,
    /// Where the run keeps its entry, and what follows from a revision.
    store: Option<store::Store>,
}

/// Recorder handle. Clones share; the default is off and records nothing.
#[derive(Clone, Default)]
pub struct Trace(Option<Arc<Inner>>);

impl Trace {
    pub fn enabled() -> Trace {
        Trace::enabled_in(None)
    }

    pub fn enabled_in(store: Option<store::Store>) -> Trace {
        Trace(Some(Arc::new(Inner {
            obs: Mutex::new(Vec::new()),
            uncacheable: AtomicBool::new(false),
            store,
        })))
    }

    /// The answer that the store of the run keeps for `key`. It follows
    /// from the key alone, so it is no observation.
    pub fn kept(&self, key: &str) -> Option<Vec<u8>> {
        self.0.as_ref()?.store.as_ref()?.kept(key)
    }

    pub fn keep(&self, key: &str, answer: &[u8]) {
        if let Some(s) = self.0.as_ref().and_then(|i| i.store.as_ref()) {
            s.keep(key, answer);
        }
    }

    #[inline]
    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    pub fn record(&self, o: Obs) {
        if let Some(i) = &self.0 {
            i.obs.lock().unwrap().push(o);
        }
    }

    pub fn mark_uncacheable(&self) {
        if let Some(i) = &self.0 {
            i.uncacheable.store(true, Ordering::Relaxed);
        }
    }

    pub fn is_cacheable(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|i| !i.uncacheable.load(Ordering::Relaxed))
    }

    /// Every observation, deduplicated on (kind, subject); two observations
    /// of one subject with different values are both kept.
    pub fn take(&self) -> Vec<Obs> {
        let Some(i) = &self.0 else {
            return Vec::new();
        };
        let all = mem::take(&mut *i.obs.lock().unwrap());
        // one pass to find the subjects that repeat, and a second that encodes
        // only those: comparing each repeat with every earlier observation was
        // quadratic, and 80,000 of one subject took 12 s
        let mut counts: HashMap<(u8, Cow<'_, [u8]>, u8), u32> = HashMap::with_capacity(all.len());
        for o in &all {
            *counts.entry(o.dedupe_key()).or_default() += 1;
        }
        let mut exact: HashSet<Vec<u8>> = HashSet::new();
        let keep: Vec<bool> = all
            .iter()
            .map(|o| counts[&o.dedupe_key()] == 1 || exact.insert(wire::encode_obs(o)))
            .collect();
        all.into_iter()
            .zip(keep)
            .filter_map(|(o, k)| k.then_some(o))
            .collect()
    }

    /// A regular file whose bytes are in memory.
    pub fn file(&self, path: &[u8], meta: &fs::Metadata, bytes: &[u8]) {
        if self.is_on() {
            self.record(file_obs(path, meta, sha256(bytes)));
        }
    }

    pub fn stat(&self, path: &[u8], follow: bool, kind: Kind) {
        if self.is_on() {
            self.record(Obs::Stat {
                path: path.to_vec(),
                follow,
                kind,
            });
        }
    }

    /// Stats `path` and records what it saw; returns the kind.
    pub fn stat_now(&self, path: &[u8], follow: bool) -> Kind {
        let kind = stat_kind(Path::new(OsStr::from_bytes(path)), follow);
        self.stat(path, follow, kind);
        kind
    }

    pub fn env(&self, name: &[u8]) {
        if self.is_on() {
            self.record(Obs::Env {
                name: name.to_vec(),
                value: env_hash(name),
            });
        }
    }

    /// Runs `argv` in `dir`, records what it printed, returns (status, stdout).
    /// A command line that [`Query`] does not hold still runs, but its answer
    /// is no observation: validation would have to trust the entry for the
    /// argv, so the run is uncacheable instead.
    pub fn cmd(&self, dir: &Path, argv: &[Vec<u8>]) -> io::Result<(i32, Vec<u8>)> {
        let (status, stdout) = run_cmd(dir, argv)?;
        if self.is_on() {
            match Query::of(argv) {
                Some(query) => self.record(Obs::Cmd {
                    dir: dir.as_os_str().as_bytes().to_vec(),
                    query,
                    hash: cmd_hash(status, &stdout),
                }),
                None => self.mark_uncacheable(),
            }
        }
        Ok((status, stdout))
    }

    pub fn canon(&self, path: &[u8], target: Option<&Path>) {
        if self.is_on() {
            self.record(Obs::Canon {
                path: path.to_vec(),
                target: target.map(|t| t.as_os_str().as_bytes().to_vec()),
            });
        }
    }

    /// A fingerprint accumulator for a tree about to be dumped, `None` when
    /// tracing is off so the dump's filter wrapper costs nothing.
    pub fn fp(&self, root: &Path) -> Option<TreeFp> {
        if !self.is_on() {
            return None;
        }
        TreeFp::new(root)
    }

    /// A tree copied to the store. `accepted` is the filter result, `None`
    /// for an unfiltered copy. `fp` is what the NAR dump's filter recorded;
    /// without it the tree is walked once more with `lstat` only.
    pub fn tree(
        &self,
        root: &Path,
        nar_hash: Digest32,
        accepted: Option<&HashSet<PathBuf>>,
        fp: Option<TreeFp>,
    ) {
        self.tree_with(root, nar_hash, accepted, true, None, fp);
    }

    /// An unfiltered tree whose newest mtime the evaluation can see.
    pub fn tree_dated(&self, root: &Path, nar_hash: Digest32, newest: i64, fp: Option<TreeFp>) {
        self.tree_with(root, nar_hash, None, true, Some(newest), fp);
    }

    /// A tree whose accepted set comes from outside (a git index): only the
    /// accepted entries matter, so no `seen_hash` is kept.
    pub fn tree_external(
        &self,
        root: &Path,
        nar_hash: Digest32,
        accepted: &HashSet<PathBuf>,
        fp: Option<TreeFp>,
    ) {
        self.tree_with(root, nar_hash, Some(accepted), false, None, fp);
    }

    fn tree_with(
        &self,
        root: &Path,
        nar_hash: Digest32,
        accepted: Option<&HashSet<PathBuf>>,
        keep_seen: bool,
        newest: Option<i64>,
        fp: Option<TreeFp>,
    ) {
        if !self.is_on() {
            return;
        }
        let Some((stat_hash, seen_hash)) = fp
            .and_then(TreeFp::finish)
            .or_else(|| tree_fingerprint(root, accepted).ok())
        else {
            self.mark_uncacheable();
            return;
        };
        let filtered = accepted.map(|a| {
            // Only the members under this root can decide this tree. The set
            // a caller passes is the enclosing filter's — for a flake input
            // that is every tracked file of the whole input — so a single
            // patch file copied out of nixpkgs recorded 54k paths it could
            // never read, 8.4 MB for one observation.
            let mut accepted: Vec<Vec<u8>> = a
                .iter()
                .filter(|p| p.starts_with(root))
                .map(|p| p.as_os_str().as_bytes().to_vec())
                .collect();
            accepted.sort();
            Filtered {
                seen_hash: keep_seen.then_some(seen_hash),
                accepted,
            }
        });
        self.record(Obs::Tree {
            root: root.as_os_str().as_bytes().to_vec(),
            nar_hash,
            stat_hash,
            filtered,
            newest,
        });
    }
}

pub fn sha256(bytes: &[u8]) -> Digest32 {
    Sha256::digest(bytes).into()
}

/// What an `Env` observation records: the keyed hash of the value of `name`,
/// or `None` when the variable is not set. The key is the cache's own
/// (`cachefs::env_key`), so the entry says whether the value changed and
/// nothing else about it.
pub fn env_hash(name: &[u8]) -> Option<Digest32> {
    let v = var_os(OsStr::from_bytes(name))?;
    let mut h = blake3::Hasher::new_keyed(cachefs::env_key());
    h.update(&(name.len() as u64).to_le_bytes());
    h.update(name);
    h.update(v.as_bytes());
    Some(*h.finalize().as_bytes())
}

pub fn mtime_ns(m: &fs::Metadata) -> i64 {
    m.mtime()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(m.mtime_nsec())
}

pub fn ctime_ns(m: &fs::Metadata) -> i64 {
    m.ctime()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(m.ctime_nsec())
}

pub fn file_obs(path: &[u8], meta: &fs::Metadata, hash: Digest32) -> Obs {
    Obs::File {
        path: path.to_vec(),
        size: meta.len(),
        mtime_ns: mtime_ns(meta),
        ctime_ns: ctime_ns(meta),
        dev: meta.dev(),
        ino: meta.ino(),
        hash,
    }
}

pub fn stat_kind(p: &Path, follow: bool) -> Kind {
    let r = if follow {
        fs::metadata(p)
    } else {
        fs::symlink_metadata(p)
    };
    Kind::of(&r)
}

/// sha256 of the sorted `(name, type)` listing of a directory, as `readDir`
/// reports it.
pub fn dir_listing_hash(entries: &mut [(Vec<u8>, Kind)]) -> Digest32 {
    entries.sort();
    let mut h = Sha256::new();
    for (name, kind) in entries.iter() {
        h.update((name.len() as u64).to_le_bytes());
        h.update(name);
        h.update([kind.to_u8()]);
    }
    h.finalize().into()
}

/// Scratch for [`dir_hash`], reused across the directories of one walk: every
/// entry name in one buffer, and a side table that is what gets sorted. The
/// listing of a validation is read once and thrown away, so holding 20k live
/// `Vec<u8>` allocations to sort pointers at them is the whole cost
/// (docs/explanation/caching.md).
#[derive(Default)]
pub struct DirBuf {
    names: Vec<u8>,
    ents: Vec<Ent>,
}

/// One entry: where its name is in `DirBuf::names`, and its kind.
struct Ent {
    at: u32,
    len: u32,
    kind: u8,
}

/// [`dir_listing_hash`] of `p`, without a `Vec` per entry. The digest is the
/// same: the same canonical order over the same fields, so a cache entry
/// written by either is read by both.
pub fn dir_hash(p: &Path, buf: &mut DirBuf) -> io::Result<Digest32> {
    buf.names.clear();
    buf.ents.clear();
    for e in fs::read_dir(p)? {
        let e = e?;
        let kind = Kind::of_file_type(&e.file_type()?);
        let name = e.file_name();
        let b = name.as_bytes();
        buf.ents.push(Ent {
            at: buf.names.len() as u32,
            len: b.len() as u32,
            kind: kind.to_u8(),
        });
        buf.names.extend_from_slice(b);
    }
    let DirBuf { names, ents } = buf;
    let name_of = |e: &Ent| &names[e.at as usize..(e.at + e.len) as usize];
    ents.sort_unstable_by(|a, b| name_of(a).cmp(name_of(b)).then(a.kind.cmp(&b.kind)));
    let mut h = Sha256::new();
    for e in ents.iter() {
        h.update((e.len as u64).to_le_bytes());
        h.update(name_of(e));
        h.update([e.kind]);
    }
    Ok(h.finalize().into())
}

pub fn read_dir_kinds(p: &Path) -> io::Result<Vec<(Vec<u8>, Kind)>> {
    let mut out = Vec::new();
    for e in fs::read_dir(p)? {
        let e = e?;
        let kind = Kind::of_file_type(&e.file_type()?);
        out.push((e.file_name().as_bytes().to_vec(), kind));
    }
    Ok(out)
}

/// The `accepted` filter of a recorded copy, or accept-all.
fn accepts(accepted: Option<&HashSet<PathBuf>>, p: &Path) -> bool {
    accepted.is_none_or(|a| a.contains(p))
}

/// The same two hashes `tree_fingerprint` computes, taken from the NAR dump
/// that is already walking the tree. The dump offers every directory entry to
/// its filter in NAR order, which is the order `tree_fingerprint` walks in, so
/// feeding each offer to `entry` reproduces both digests byte for byte and no
/// second `read_dir` walk is needed.
pub struct TreeFp {
    root_len: usize,
    stat_h: Sha256,
    seen_h: Sha256,
    /// The newest mtime among the root and the accepted entries, in seconds.
    newest: i64,
    ok: bool,
}

impl TreeFp {
    /// Seeds the fingerprint with the root's own `lstat`, the one node the
    /// dump never offers to the filter. `None` when the root cannot be read.
    pub fn new(root: &Path) -> Option<TreeFp> {
        let mut fp = TreeFp {
            root_len: root.as_os_str().as_bytes().len(),
            stat_h: Sha256::new(),
            seen_h: Sha256::new(),
            newest: i64::MIN,
            ok: true,
        };
        fp.stat(root, b"");
        fp.ok.then_some(fp)
    }

    /// One directory entry the dump offered to the filter, and what the filter
    /// answered. Rejected entries reach `seen_hash` only, as in `walk_fp`.
    pub fn entry(&mut self, p: &Path, t: FileType, accepted: bool) {
        let rel = &p.as_os_str().as_bytes()[self.root_len..];
        self.seen_h.update((rel.len() as u64).to_le_bytes());
        self.seen_h.update(rel);
        self.seen_h.update([nar_kind(t).to_u8()]);
        if accepted {
            let rel = rel.to_vec();
            self.stat(p, &rel);
        }
    }

    fn stat(&mut self, p: &Path, rel: &[u8]) {
        let Ok(st) = fs::symlink_metadata(p) else {
            self.ok = false;
            return;
        };
        let kind = Kind::of(&Ok(st.clone()));
        self.newest = self.newest.max(st.mtime());
        self.stat_h.update((rel.len() as u64).to_le_bytes());
        self.stat_h.update(rel);
        self.stat_h.update([kind.to_u8()]);
        self.stat_h.update(st.len().to_le_bytes());
        self.stat_h.update(mtime_ns(&st).to_le_bytes());
        self.stat_h.update(ctime_ns(&st).to_le_bytes());
        self.stat_h.update(st.dev().to_le_bytes());
        self.stat_h.update(st.ino().to_le_bytes());
        self.stat_h.update((st.mode() & 0o100).to_le_bytes());
        if kind == Kind::Symlink {
            match fs::read_link(p) {
                Ok(t) => self.stat_h.update(t.as_os_str().as_bytes()),
                Err(_) => self.ok = false,
            }
        }
    }

    /// `(stat_hash, seen_hash)`, or `None` when any `lstat` on the way failed
    /// and the fingerprint would not be the tree's.
    pub fn finish(self) -> Option<(Digest32, Digest32)> {
        self.ok
            .then(|| (self.stat_h.finalize().into(), self.seen_h.finalize().into()))
    }
}

/// Walks `root` like the NAR dump does (sorted entries, no descent into
/// rejected directories) and hashes what `lstat` reports for every accepted
/// entry (`stat_hash`) and the `(relative path, type)` of every entry the
/// filter saw (`seen_hash`). No file contents are read.
pub fn tree_fingerprint(
    root: &Path,
    accepted: Option<&HashSet<PathBuf>>,
) -> io::Result<(Digest32, Digest32)> {
    let mut stat_h = Sha256::new();
    let mut seen_h = Sha256::new();
    walk_fp(root, b"", accepted, &mut stat_h, &mut seen_h, 0)?;
    Ok((stat_h.finalize().into(), seen_h.finalize().into()))
}

fn walk_fp(
    path: &Path,
    rel: &[u8],
    accepted: Option<&HashSet<PathBuf>>,
    stat_h: &mut Sha256,
    seen_h: &mut Sha256,
    depth: usize,
) -> io::Result<()> {
    if depth > 64 {
        return Err(io::Error::other("tree too deep"));
    }
    let st = fs::symlink_metadata(path)?;
    let kind = Kind::of(&Ok(st.clone()));
    stat_h.update((rel.len() as u64).to_le_bytes());
    stat_h.update(rel);
    stat_h.update([kind.to_u8()]);
    stat_h.update(st.len().to_le_bytes());
    stat_h.update(mtime_ns(&st).to_le_bytes());
    stat_h.update(ctime_ns(&st).to_le_bytes());
    stat_h.update(st.dev().to_le_bytes());
    stat_h.update(st.ino().to_le_bytes());
    stat_h.update((st.mode() & 0o100).to_le_bytes());
    match kind {
        Kind::Symlink => {
            let t = fs::read_link(path)?;
            stat_h.update(t.as_os_str().as_bytes());
        }
        Kind::Directory => {
            let mut entries = read_dir_kinds(path)?;
            entries.sort();
            for (name, ekind) in entries {
                let child = path.join(OsStr::from_bytes(&name));
                let mut crel = rel.to_vec();
                crel.push(b'/');
                crel.extend_from_slice(&name);
                seen_h.update((crel.len() as u64).to_le_bytes());
                seen_h.update(&crel);
                seen_h.update([ekind.to_u8()]);
                if accepts(accepted, &child) {
                    walk_fp(&child, &crel, accepted, stat_h, seen_h, depth + 1)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// NAR hash of `root` under the recorded filter, plus the `seen_hash` and the
/// current `stat_hash`, for the slow path of `Tree` validation. The dump gives
/// all three, so the caller never needs `tree_fingerprint` again afterwards.
pub fn tree_rehash(
    root: &Path,
    accepted: Option<&HashSet<PathBuf>>,
) -> io::Result<(Digest32, Digest32, Digest32, i64)> {
    let mut fp = TreeFp::new(root).ok_or_else(|| io::Error::other("tree is gone"))?;
    let mut filter = |p: &Path, t: FileType| -> bool {
        let keep = accepts(accepted, p);
        fp.entry(p, t, keep);
        keep
    };
    let (h, _) = nar::nar_hash_filtered(root, HashAlgo::Sha256, &mut filter)?;
    let newest = fp.newest;
    let (stat_hash, seen_hash) = fp
        .finish()
        .ok_or_else(|| io::Error::other("tree changed while it was read"))?;
    Ok((hash_to_digest(&h), seen_hash, stat_hash, newest))
}

fn nar_kind(t: FileType) -> Kind {
    match t {
        FileType::Regular => Kind::Regular,
        FileType::Directory => Kind::Directory,
        FileType::Symlink => Kind::Symlink,
        FileType::Unknown => Kind::Other,
    }
}

pub fn hash_to_digest(h: &Hash) -> Digest32 {
    let mut d = [0u8; 32];
    d.copy_from_slice(h.bytes());
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    use crate::store::nar::tests::fixture_tree;

    #[test]
    fn dedupe_keeps_conflicting_values() {
        let t = Trace::enabled();
        let o1 = Obs::Env {
            name: b"A".to_vec(),
            value: Some([1; 32]),
        };
        let o2 = Obs::Env {
            name: b"A".to_vec(),
            value: Some([2; 32]),
        };
        t.record(o1.clone());
        t.record(o1.clone());
        t.record(o2.clone());
        t.record(o2.clone());
        let v = t.take();
        assert_eq!(v, vec![o1, o2]);
    }

    /// `pathExists` on the same path 80,000 times cost 12 s to deduplicate,
    /// because every repeat was compared with every earlier observation.
    #[test]
    fn dedupe_of_many_repeats_is_linear() {
        const N: usize = 80_000;
        let t = Trace::enabled();
        for i in 0..N {
            t.record(Obs::Stat {
                path: b"/a".to_vec(),
                follow: true,
                kind: if i % 2 == 0 {
                    Kind::Regular
                } else {
                    Kind::Missing
                },
            });
        }
        let t0 = Instant::now();
        let v = t.take();
        assert_eq!(v.len(), 2, "one per distinct value");
        assert!(t0.elapsed().as_secs() < 2, "{:?}", t0.elapsed());
    }

    /// A command line the table does not hold has no observation the
    /// validator could ask again, so the run is uncacheable instead.
    #[test]
    fn only_a_named_query_is_an_observation() {
        for q in Query::ALL {
            let argv: Vec<Vec<u8>> = q.argv().iter().map(|a| a.as_bytes().to_vec()).collect();
            assert_eq!(Query::of(&argv), Some(q));
            assert_eq!(Query::from_u8(q.to_u8()), Some(q));
        }
        assert_eq!(Query::of(&[b"sh".to_vec(), b"-c".to_vec()]), None);
        assert_eq!(Query::from_u8(Query::ALL.len() as u8), None);

        let t = Trace::enabled();
        assert!(t.is_cacheable());
        let _ = t.cmd(Path::new("/"), &[b"true".to_vec()]);
        assert!(!t.is_cacheable(), "a command outside the table");
        assert!(t.take().is_empty(), "and it leaves no observation");
    }

    /// The fused fingerprint must be byte-identical to the separate walk, or
    /// every cache entry a dump writes would miss on validation.
    #[test]
    fn dump_fingerprint_matches_the_separate_walk() {
        let dir = fixture_tree("trace-fused");
        for accepted in [
            None,
            Some(
                [dir.join("a.txt"), dir.join("sub")]
                    .into_iter()
                    .collect::<HashSet<PathBuf>>(),
            ),
        ] {
            let mut fp = TreeFp::new(&dir).unwrap();
            let mut filter = |p: &Path, t: FileType| {
                let keep = accepts(accepted.as_ref(), p);
                fp.entry(p, t, keep);
                keep
            };
            nar::nar_hash_filtered(&dir, HashAlgo::Sha256, &mut filter).unwrap();
            assert_eq!(
                fp.finish().unwrap(),
                tree_fingerprint(&dir, accepted.as_ref()).unwrap()
            );
        }
    }

    #[test]
    fn fingerprint_changes_with_content_size_and_seen_entries() {
        let dir = fixture_tree("trace-fp");
        let (a, sa) = tree_fingerprint(&dir, None).unwrap();
        let (b, sb) = tree_fingerprint(&dir, None).unwrap();
        assert_eq!((a, sa), (b, sb));
        fs::write(dir.join("new-file"), b"x").unwrap();
        let (c, sc) = tree_fingerprint(&dir, None).unwrap();
        assert_ne!(a, c);
        assert_ne!(sa, sc);
        let (nar, seen, stat, newest) = tree_rehash(&dir, None).unwrap();
        let written = fs::symlink_metadata(dir.join("new-file")).unwrap();
        assert_eq!(newest, written.mtime());
        assert_eq!(stat, c);
        assert_eq!(seen, sc);
        assert_eq!(
            nar,
            hash_to_digest(&crate::store::nar::nar_hash(&dir).unwrap())
        );
    }
}
