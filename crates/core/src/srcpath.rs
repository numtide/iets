//! Where the bytes of a path are, and where an input source came from during
//! evaluation. The remote client ships sources from these records, so it
//! never needs a local store.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::store::hash::Hash;
use crate::store::path::StorePath;
use crate::sys::OsStrExt;

use crate::events::{self, Field};

/// The entries a tree filter kept, `None` for no filter.
pub type Accepted = Option<Arc<HashSet<PathBuf>>>;

/// What holds the bytes (source-accessor.hh:SourceAccessor). The readers are
/// in `builtins::store_hooks`.
#[derive(Clone, Debug)]
pub enum Src {
    /// A directory on disk, or any path of the real file system as its own
    /// root. `accepted` holds every entry a tree filter kept (the tracked
    /// files of a git tree): an entry it left out does not exist
    /// (filtering-source-accessor.hh).
    Fs { root: PathBuf, accepted: Accepted },
    /// An entry of the user-level input store (`crate::treestore`), by store
    /// path base name. Opened on first read, so a tracing-cache hit that only
    /// uploads needs no evaluation. `accepted` is as for `Fs`, with each
    /// member named by its path in the entry under `/`.
    Spool { name: String, accepted: Accepted },
    /// One regular file: a `toFile` text or a `.drv` this evaluation holds.
    Mem(Arc<Vec<u8>>),
}

/// A path inside a [`Src`] (source-path.hh:SourcePath). `rel` has no leading
/// `/` and is empty for the root.
#[derive(Clone, Debug)]
pub struct SrcPath {
    pub src: Src,
    pub rel: Vec<u8>,
}

impl SrcPath {
    pub fn fs(root: impl Into<PathBuf>, accepted: Accepted) -> SrcPath {
        SrcPath {
            src: Src::Fs {
                root: root.into(),
                accepted,
            },
            rel: Vec::new(),
        }
    }

    pub fn spool(name: &str, rel: &[u8]) -> SrcPath {
        SrcPath {
            src: Src::Spool {
                name: name.to_string(),
                accepted: None,
            },
            rel: rel.to_vec(),
        }
    }

    /// This path under the filter `accepted` instead of its own.
    pub fn filtered(mut self, accepted: Accepted) -> SrcPath {
        if let Src::Fs { accepted: a, .. } | Src::Spool { accepted: a, .. } = &mut self.src {
            *a = accepted;
        }
        self
    }

    /// Where the bytes are, for a message: the local root, the input store
    /// entry it is served out of, or the text itself. `(filtered)` when a
    /// filter keeps part of the tree out of the copy. Appends, because the
    /// event stream builds a record without allocating.
    pub fn describe_into(&self, out: &mut Vec<u8>) {
        let accepted = match &self.src {
            Src::Fs { root, accepted } => {
                out.extend_from_slice(root.as_os_str().as_bytes());
                accepted
            }
            Src::Spool { name, accepted } => {
                out.extend_from_slice(b"input-store:");
                out.extend_from_slice(name.as_bytes());
                accepted
            }
            Src::Mem(_) => {
                out.extend_from_slice(b"text");
                return;
            }
        };
        if !self.rel.is_empty() {
            out.push(b'/');
            out.extend_from_slice(&self.rel);
        }
        if accepted.is_some() {
            out.extend_from_slice(b" (filtered)");
        }
    }

    /// `sub` below this path.
    pub fn join(mut self, sub: &[u8]) -> SrcPath {
        if !sub.is_empty() {
            if !self.rel.is_empty() {
                self.rel.push(b'/');
            }
            self.rel.extend_from_slice(sub);
        }
        self
    }
}

/// How an input source was content-addressed (store-api.hh:ContentAddress, the three kinds
/// eval produces).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Recursive sha256 of the NAR, no references: `./path`, `fetchTarball`.
    Source,
    /// Flat sha256 of one regular file plus references: `builtins.toFile`.
    Text { references: BTreeSet<StorePath> },
    /// Flat hash of one regular file, no references: `builtins.fetchurl`.
    Flat { hash: Hash },
}

/// Where an input source's bytes are and how it was addressed. Reads under its
/// store path go to `at` (`builtins::store_hooks::locate`), so the store copy
/// need not exist.
#[derive(Clone, Debug)]
pub struct SourceOrigin {
    pub at: SrcPath,
    pub kind: SourceKind,
    /// The NAR hash and size the dump under the recorded filter produced, the exact pair
    /// `AddMultipleToStore` needs, so no upload path hashes the tree a second time. `None`
    /// for a text, and for a store path the plan must hash itself.
    pub nar: Option<(Hash, u64)>,
}

impl Field for &SrcPath {
    fn write(self, out: &mut Vec<u8>) {
        let start = out.len();
        self.describe_into(out);
        events::quote_in_place(out, start);
    }
}

impl SourceOrigin {
    pub fn tree(at: SrcPath, nar_hash: Hash, nar_size: u64) -> SourceOrigin {
        SourceOrigin {
            at,
            kind: SourceKind::Source,
            nar: Some((nar_hash, nar_size)),
        }
    }

    /// A flat fixed output named by `hash`, the hash of the one regular file at
    /// `at`, with the NAR hash and size of that file.
    pub fn flat(at: SrcPath, hash: Hash, nar_hash: Hash, nar_size: u64) -> SourceOrigin {
        SourceOrigin {
            at,
            kind: SourceKind::Flat { hash },
            nar: Some((nar_hash, nar_size)),
        }
    }

    pub fn text(contents: Vec<u8>, references: BTreeSet<StorePath>) -> SourceOrigin {
        SourceOrigin {
            at: SrcPath {
                src: Src::Mem(Arc::new(contents)),
                rel: Vec::new(),
            },
            kind: SourceKind::Text { references },
            nar: None,
        }
    }

    /// The references of a `toFile` text.
    pub fn text_refs(&self) -> Option<&BTreeSet<StorePath>> {
        match &self.kind {
            SourceKind::Text { references } => Some(references),
            SourceKind::Source | SourceKind::Flat { .. } => None,
        }
    }
}
