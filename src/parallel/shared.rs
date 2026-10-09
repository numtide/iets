//! Shared-by-handle containers for the evaluator state that every fiber
//! must see: file table, import cache, source-to-store map, derivation
//! tables. Cloning shares.

use std::array;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::Index;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use std::vec::IntoIter;

use crate::eval::{FileInfo, Pos};
use crate::runtime::Word;
use crate::runtime::arena::NSTATS;
use crate::syntax::ast::FileId;
use crate::syntax::symbol::Sym;

use super::interner;

pub struct SharedMap<K, V>(Arc<Mutex<HashMap<K, V>>>);

impl<K, V> Clone for SharedMap<K, V> {
    fn clone(&self) -> Self {
        SharedMap(self.0.clone())
    }
}

impl<K, V> Default for SharedMap<K, V> {
    fn default() -> Self {
        SharedMap(Arc::new(Mutex::new(HashMap::new())))
    }
}

impl<K: Hash + Eq, V: Clone> SharedMap<K, V> {
    pub fn get<Q: ?Sized + Hash + Eq>(&self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.0.lock().unwrap().get(k).cloned()
    }

    pub fn insert(&self, k: K, v: V) -> Option<V> {
        self.0.lock().unwrap().insert(k, v)
    }

    /// Insert `v` unless `k` is present; return the value now stored.
    pub fn get_or_insert(&self, k: K, v: V) -> V {
        self.0.lock().unwrap().entry(k).or_insert(v).clone()
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The map itself, for callers that read it whole (`iets build`).
    pub fn lock(&self) -> MutexGuard<'_, HashMap<K, V>> {
        self.0.lock().unwrap()
    }

    /// Snapshot of all entries.
    pub fn iter(&self) -> IntoIter<(K, V)>
    where
        K: Clone,
    {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Vec<_>>()
            .into_iter()
    }
}

struct FilesInner {
    list: Vec<&'static FileInfo>,
    ids: HashMap<Vec<u8>, FileId>,
    /// Global source offset the next file starts at (`Pos` space).
    next_base: u32,
}

/// Append-only table of loaded files: path, global base offset and line
/// table only, never the source text. Entries are leaked so references
/// outlive every lock.
#[derive(Clone)]
pub struct Files(Arc<Mutex<FilesInner>>);

impl Default for Files {
    fn default() -> Self {
        Self::new()
    }
}

impl Files {
    pub fn new() -> Files {
        Files(Arc::new(Mutex::new(FilesInner {
            list: Vec::new(),
            ids: HashMap::new(),
            next_base: 0,
        })))
    }

    /// Register `src` under `path`, or return the existing id. A pseudo-file
    /// (`«string»`, `«arg»`) is new each time: two of them share a name, not
    /// a text, and positions in the second would be read against the first.
    pub fn add(&self, path: &[u8], src: &[u8]) -> FileId {
        let mut g = self.0.lock().unwrap();
        let pseudo = path.starts_with("\u{ab}".as_bytes());
        if !pseudo && let Some(id) = g.ids.get(path) {
            return *id;
        }
        let id = FileId(g.list.len() as u32);
        let base = g.next_base;
        g.next_base = base
            .checked_add(src.len() as u32 + 1)
            .expect("sources exceed 4 GiB");
        g.list
            .push(Box::leak(Box::new(FileInfo::new(path.to_vec(), src, base))));
        if !pseudo {
            g.ids.insert(path.to_vec(), id);
        }
        id
    }

    pub fn get(&self, i: usize) -> Option<&'static FileInfo> {
        self.0.lock().unwrap().list.get(i).copied()
    }

    /// The file whose base range holds `pos`, and the offset inside it.
    pub fn file_of(&self, pos: Pos) -> Option<(&'static FileInfo, u32)> {
        if pos.is_none() {
            return None;
        }
        let g = self.0.lock().unwrap();
        let i = g.list.partition_point(|f| f.base <= pos.0);
        let f = *g.list.get(i.checked_sub(1)?)?;
        Some((f, pos.0 - f.base))
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Index<usize> for Files {
    type Output = FileInfo;
    fn index(&self, i: usize) -> &FileInfo {
        self.get(i).expect("file id out of range")
    }
}

/// Lock-free `Sym -> Word` table for the string object of a symbol's name
/// (`Evaluator::sym_string`), shared by every fiber. A per-evaluator
/// `Vec` cost each new fiber a copy or a re-zeroing of one word per
/// symbol (~1 MB with nixpkgs loaded). Entries are written at most a few
/// times: two fibers racing on one symbol both store a valid string, and
/// either one serves.
#[derive(Clone)]
pub struct SymWords(Arc<SymWordsInner>);

struct SymWordsInner(*mut AtomicU64);

// SAFETY: the table holds atomics only; the mapping lives for the process.
unsafe impl Sync for SymWordsInner {}
unsafe impl Send for SymWordsInner {}

impl Default for SymWords {
    fn default() -> Self {
        Self::new()
    }
}

impl SymWords {
    pub fn new() -> SymWords {
        // zero is `Word::EMPTY`
        SymWords(Arc::new(SymWordsInner(interner::per_sym())))
    }

    #[inline]
    fn slot(&self, s: Sym) -> &AtomicU64 {
        assert!((s.0 as usize) < super::interner::MAX_SYMS);
        // SAFETY: in bounds of the mapping.
        unsafe { &*self.0.0.add(s.0 as usize) }
    }

    /// `Word::EMPTY` when no string was stored yet.
    #[inline]
    pub fn get(&self, s: Sym) -> Word {
        // SAFETY: the slot holds EMPTY or a word `set` stored.
        unsafe { Word::from_bits(self.slot(s).load(Ordering::Acquire)) }
    }

    #[inline]
    pub fn set(&self, s: Sym, w: Word) {
        self.slot(s).store(w.bits(), Ordering::Release);
    }
}

/// Run-wide evaluation counters. Every fiber counts into plain fields on
/// its own evaluator, which keeps the hot path free of atomics, and adds
/// them here when it finishes a task. A stats line reads the sum, so what
/// a helper fiber did is reported too (docs/reference/settings.md).
#[derive(Default)]
pub struct Totals {
    thunks: AtomicU64,
    calls: AtomicU64,
    derivations: AtomicU64,
    store_adds: AtomicU64,
    compile_ns: AtomicU64,
    arena_bytes: AtomicU64,
    arena_mapped: AtomicU64,
    kind_counts: Kinds,
    kind_bytes: Kinds,
}

/// One counter per object kind. Only a stats run folds anything into these,
/// so a run without `IETS_STATS` never touches them.
pub struct Kinds([AtomicU64; NSTATS]);

impl Default for Kinds {
    fn default() -> Kinds {
        Kinds(array::from_fn(|_| AtomicU64::new(0)))
    }
}

/// Counters of one fiber, or of a whole run.
#[derive(Default, Clone, Copy)]
pub struct Counts {
    pub thunks: u64,
    pub calls: u64,
    pub derivations: u64,
    /// Source trees and `toFile` texts turned into a store path: the
    /// `addToStore` and `addTextToStore` calls Nix would make. A `.drv` is
    /// `derivations`; a fetched input is already in the store and counts in
    /// neither.
    pub store_adds: u64,
    pub compile: Duration,
}

/// What arenas hold: one arena's unfolded delta, or every arena of a run.
/// Each worker allocates from its own arena, so only the sum is the run's
/// memory; one arena is a quarter of it at the default worker count.
#[derive(Default, Clone, Copy)]
pub struct ArenaUse {
    pub bytes: u64,
    pub mapped: u64,
    pub counts: [u64; NSTATS],
    pub kind_bytes: [u64; NSTATS],
}

impl Totals {
    pub fn add(&self, c: Counts) {
        self.thunks.fetch_add(c.thunks, Ordering::Relaxed);
        self.calls.fetch_add(c.calls, Ordering::Relaxed);
        self.derivations.fetch_add(c.derivations, Ordering::Relaxed);
        self.store_adds.fetch_add(c.store_adds, Ordering::Relaxed);
        self.compile_ns
            .fetch_add(c.compile.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn get(&self) -> Counts {
        Counts {
            thunks: self.thunks.load(Ordering::Relaxed),
            calls: self.calls.load(Ordering::Relaxed),
            derivations: self.derivations.load(Ordering::Relaxed),
            store_adds: self.store_adds.load(Ordering::Relaxed),
            compile: Duration::from_nanos(self.compile_ns.load(Ordering::Relaxed)),
        }
    }

    /// A zero add is skipped: these lines are shared between workers, and
    /// most kinds see nothing in any one task.
    pub fn add_arena(&self, u: &ArenaUse) {
        if u.bytes != 0 {
            self.arena_bytes.fetch_add(u.bytes, Ordering::Relaxed);
        }
        if u.mapped != 0 {
            self.arena_mapped.fetch_add(u.mapped, Ordering::Relaxed);
        }
        for i in 0..NSTATS {
            if u.counts[i] != 0 {
                self.kind_counts.0[i].fetch_add(u.counts[i], Ordering::Relaxed);
                self.kind_bytes.0[i].fetch_add(u.kind_bytes[i], Ordering::Relaxed);
            }
        }
    }

    pub fn arena(&self) -> ArenaUse {
        ArenaUse {
            bytes: self.arena_bytes.load(Ordering::Relaxed),
            mapped: self.arena_mapped.load(Ordering::Relaxed),
            counts: array::from_fn(|i| self.kind_counts.0[i].load(Ordering::Relaxed)),
            kind_bytes: array::from_fn(|i| self.kind_bytes.0[i].load(Ordering::Relaxed)),
        }
    }
}
