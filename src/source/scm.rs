//! The source control system (SCM) of a local working tree. An [`Scm`]
//! supplies what such a tree gives to an evaluation: its root, the files it
//! tracks, the dirty flag and the metadata of its revision. `KNOWN` is the
//! one list of the implementations, and the callers reach them through the
//! functions below it, never by name. A remote input (`git+https:`, `hg+`)
//! is a fetcher and is not here.

mod git;
#[cfg(test)]
mod ledger;

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::trace::{Obs, Trace};

/// A working tree as its SCM reports it.
pub struct Snapshot {
    /// The tracked files that are in the working tree, as absolute paths.
    /// The working tree supplies their contents and their modes.
    pub files: HashSet<PathBuf>,
    /// A tracked file differs from the revision.
    pub dirty: bool,
    /// The revision the tree is at, in ASCII, as the SCM spells it: `rev`
    /// on a clean tree, the start of `dirtyRev` on a dirty one. `None` when
    /// the SCM has no name for it.
    pub rev: Option<String>,
    /// The date of the revision in seconds since the epoch, 0 without one.
    pub last_modified: u64,
    /// The SCM has the whole history of the revision, so `Scm::rev_count`
    /// has an answer. A shallow clone has not.
    pub countable: bool,
}

pub trait Scm: Sync {
    /// The `type` of the tree as an input and the scheme of its URL: `git`
    /// in `git+file://`.
    fn name(&self) -> &'static str;

    /// The name as a message spells it: `Git`.
    fn title(&self) -> &'static str;

    /// The entry whose presence makes `dir` the root of a working tree of
    /// this SCM. Detection probes it, and records the probe, so a tree that
    /// appears above a directory invalidates a cached answer.
    fn root_marker(&self, dir: &Path) -> PathBuf;

    /// Reads the working tree at `root`. Each answer that does not come
    /// from a tracked file is recorded in `trace`, so the tracing cache
    /// asks again.
    fn snapshot(&self, trace: &Trace, root: &Path) -> Result<Snapshot, String>;

    /// The number of revisions up to `rev` in the working tree at `root`,
    /// for a tree that is `countable`. The answer follows from `rev`, so it
    /// is no observation, and the caller can keep it by `rev`.
    fn rev_count(&self, root: &Path, rev: &str) -> Result<u64, String>;

    /// For `rel`, a file on disk below `root` that the SCM does not track:
    /// the command that makes the SCM track it. `None` when the SCM tracks
    /// `rel` or gives no answer.
    fn untracked(&self, root: &Path, rel: &OsStr) -> Option<String>;

    /// The root of the working tree that `obs` is about, when `snapshot`
    /// of this SCM recorded `obs`.
    fn tree_of<'a>(&self, obs: &'a Obs) -> Option<&'a [u8]>;

    /// The file that changes when an answer of the SCM on the tree at
    /// `root` changes, for a program that watches files. `None` when the
    /// observations of the SCM are files and nothing else.
    fn watch(&self, root: &Path) -> Option<PathBuf>;
}

impl PartialEq for dyn Scm {
    fn eq(&self, other: &dyn Scm) -> bool {
        self.name() == other.name()
    }
}

impl Eq for dyn Scm {}

impl std::fmt::Debug for dyn Scm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Every SCM that iets knows, in the order detection asks them.
static KNOWN: &[&dyn Scm] = &[
    &git::Git,
    #[cfg(test)]
    &ledger::Ledger,
];

/// The SCM that has a working tree with its root at `root`.
pub fn at(root: &Path) -> Option<&'static dyn Scm> {
    KNOWN.iter().copied().find(|s| s.root_marker(root).exists())
}

/// The entry that would make `dir` the root of a working tree, one per SCM,
/// in the order detection asks them. `crate::project` probes these beside
/// its own markers, and the lint probes them through the tracing cache.
pub fn root_markers(dir: &Path) -> impl Iterator<Item = PathBuf> + use<'_> {
    KNOWN.iter().map(|s| s.root_marker(dir))
}

/// The working tree that holds `dir`: its SCM and its root, the nearest
/// ancestor (or `dir` itself) that an SCM knows as one.
pub fn detect(dir: &Path) -> Option<(&'static dyn Scm, PathBuf)> {
    dir.ancestors()
        .find_map(|d| Some((at(d)?, d.to_path_buf())))
}

/// Records the probes a [`detect`] of `dir` made, `found` being the root it
/// stopped at. Without them a cached result stays a path flake after a
/// working tree appears above `dir`: the answer of the run depends on every
/// marker that was absent, not only on the tree that was found.
pub fn record_detect(trace: &Trace, dir: &Path, found: Option<&Path>) {
    if !trace.is_on() {
        return;
    }
    for d in dir.ancestors() {
        for s in KNOWN {
            let marker = s.root_marker(d);
            trace.stat_now(marker.as_os_str().as_bytes(), true);
        }
        if Some(d) == found {
            return;
        }
    }
}

/// The SCM that recorded `obs`, and the root of the working tree that
/// `obs` is about.
pub fn owner(obs: &Obs) -> Option<(&'static dyn Scm, &[u8])> {
    KNOWN.iter().find_map(|s| Some((*s, s.tree_of(obs)?)))
}

/// Why a cached result is stale when `obs` no longer holds: the tree is
/// what the user changed, not the answer of one query.
pub fn changed(obs: &Obs) -> Option<String> {
    let (scm, root) = owner(obs)?;
    let root = String::from_utf8_lossy(root);
    Some(format!("the {} working tree in {root} changed", scm.name()))
}

pub fn named(name: &str) -> Option<&'static dyn Scm> {
    KNOWN.iter().copied().find(|s| s.name() == name)
}

fn count_key(scm: &dyn Scm, rev: &str) -> String {
    let mut key = crate::trace::key::Key::new("rev-count");
    key.field("scm", scm.name().as_bytes());
    key.field("rev", rev.as_bytes());
    key.finish()
}

/// `Scm::rev_count`, asked once for each revision: the store of the run
/// keeps the answer with the name of the SCM and the revision as the key,
/// as git.cc:getRevCount does in the fetcher cache of Nix.
pub fn rev_count(trace: &Trace, scm: &dyn Scm, root: &Path, rev: &str) -> Result<u64, String> {
    let key = count_key(scm, rev);
    let kept = |b: Vec<u8>| String::from_utf8(b).ok()?.parse().ok();
    if let Some(n) = trace.kept(&key).and_then(kept) {
        return Ok(n);
    }
    let n = scm.rev_count(root, rev)?;
    trace.keep(&key, n.to_string().as_bytes());
    Ok(n)
}
