//! Shared symbol interner: sharded read-write locked hash maps for
//! lookup, a lock-free id -> name table for `name`. Cloning shares the
//! table.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::process;
use std::str::from_utf8;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use super::symbol::{FxHasher, Sym};
use crate::sys::heap::Heap;
use crate::sys::vm;

const SHARDS: usize = 64;

/// `FxHasher` with a final mix. hashbrown indexes a bucket with the low
/// bits of the hash, and FxHash leaves its entropy in the high ones: at
/// four million short names of one shape (`a0`, `a1`, …) the probe chains
/// were 51 % of a run's cycles, all of it comparing keys. The mix is the
/// 64-bit finaliser of splitmix64.
#[derive(Default)]
struct MixHasher(FxHasher);

impl Hasher for MixHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.0.write(bytes);
    }

    #[inline]
    fn finish(&self) -> u64 {
        mix(self.0.finish())
    }
}

/// One multiply carries the entropy up, one fold brings it back down into
/// the bits hashbrown indexes with. Three instructions: a full
/// finaliser measured 0.2 % more instructions on nixos-server.
#[inline]
fn mix(h: u64) -> u64 {
    let h = h.wrapping_mul(0x9e3779b97f4a7c15);
    h ^ (h >> 32)
}

type MixBuild = BuildHasherDefault<MixHasher>;

/// Symbols one run may hold. The id table is one `MAP_NORESERVE` mapping
/// of `MAX_SYMS` words, so a larger cap costs address space and nothing
/// else until the names exist. `Sym` is a `u32`, which is the real bound.
pub const MAX_SYMS: usize = 1 << 30;

/// Hash for the shard and for the per-handle cache, which read the high
/// bits FxHash fills well. No mix here: it would run on every intern, and
/// only the shared table needs the low bits.
#[inline]
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write(b);
    h.finish()
}

struct Inner {
    shards: Vec<RwLock<HashMap<&'static [u8], Sym, MixBuild>>>,
    /// `names[id]` points at a name in `heap`. Written once,
    /// before the id is published through a shard.
    names: *mut AtomicPtr<&'static [u8]>,
    /// `prefixes[id]`: [`prefix`] of the name, stored with it, so sorting
    /// by name compares integers in one dense table.
    prefixes: *mut AtomicU64,
    next: AtomicU32,
    /// The session memory the names live in: a `Sym`'s name outlives this
    /// table and lives as long as the session's values.
    heap: Arc<Heap>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        unmap_per_sym(self.names);
        unmap_per_sym(self.prefixes);
    }
}

// SAFETY: the name table is append-only; each entry is written once with
// Release before its id escapes and read with Acquire.
unsafe impl Sync for Inner {}
unsafe impl Send for Inner {}

/// Per-handle direct-mapped lookup cache in front of the shared table:
/// a hit costs one hash and one compare, no lock.
const CACHE: usize = 1024;

#[derive(Clone, Copy)]
struct CacheEntry {
    hash: u64,
    name: &'static [u8],
    sym: Sym,
}

pub struct Interner {
    shared: Arc<Inner>,
    cache: Box<[CacheEntry]>,
}

impl Clone for Interner {
    fn clone(&self) -> Interner {
        Interner {
            shared: self.shared.clone(),
            cache: empty_cache(),
        }
    }
}

fn empty_cache() -> Box<[CacheEntry]> {
    // The empty name hashes to 0, so a sentinel with hash 0 would answer
    // `intern(b"")`; hash 1 with an empty name matches no lookup.
    vec![
        CacheEntry {
            hash: 1,
            name: &[],
            sym: Sym(u32::MAX),
        };
        CACHE
    ]
    .into_boxed_slice()
}

/// The mapping of [`per_sym`].
pub fn unmap_per_sym<T>(p: *mut T) {
    // SAFETY: the owner's last handle is gone, so nothing reads the table.
    unsafe { vm::unmap(p.cast(), MAX_SYMS * size_of::<T>()) };
}

/// The first 8 bytes of `name`, big-endian and zero-padded: two names in
/// byte order have prefixes in the same order or equal ones.
fn prefix(name: &[u8]) -> u64 {
    let mut p = [0u8; 8];
    let n = name.len().min(8);
    p[..n].copy_from_slice(&name[..n]);
    u64::from_be_bytes(p)
}

/// `MAX_SYMS` zeroed `T`s, paged in on demand, unmapped by the owner's drop.
pub fn per_sym<T>() -> *mut T {
    vm::map(MAX_SYMS * size_of::<T>(), true)
        .expect("per-symbol table: mmap failed")
        .cast()
}

impl Interner {
    pub fn in_heap(heap: Arc<Heap>) -> Interner {
        let names = per_sym();
        Interner {
            shared: Arc::new(Inner {
                shards: (0..SHARDS)
                    .map(|_| RwLock::new(HashMap::with_hasher(MixBuild::default())))
                    .collect(),
                names,
                prefixes: per_sym(),
                next: AtomicU32::new(0),
                heap,
            }),
            cache: empty_cache(),
        }
    }

    #[inline]
    fn shard(&self, hash: u64) -> &RwLock<HashMap<&'static [u8], Sym, MixBuild>> {
        &self.shared.shards[(hash >> 58) as usize % SHARDS]
    }

    pub fn intern(&mut self, name: &[u8]) -> Sym {
        let hash = hash_bytes(name);
        let slot = (hash as usize) & (CACHE - 1);
        let e = &self.cache[slot];
        if e.hash == hash && e.name == name {
            return e.sym;
        }
        let (leaked, sym) = self.intern_shared(name, hash);
        self.cache[slot] = CacheEntry {
            hash,
            name: leaked,
            sym,
        };
        sym
    }

    fn intern_shared(&self, name: &[u8], hash: u64) -> (&'static [u8], Sym) {
        let shard = self.shard(hash);
        if let Some((&k, &s)) = shard.read().unwrap().get_key_value(name) {
            return (k, s);
        }
        let mut w = shard.write().unwrap();
        if let Some((&k, &s)) = w.get_key_value(name) {
            return (k, s);
        }
        let leaked = self.shared.heap.alloc_bytes(name);
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed);
        if (id as usize) >= MAX_SYMS {
            // no way back from here: the id is already handed out, and the
            // caller's signature cannot fail
            errln!("error: too many symbols (more than {MAX_SYMS})");
            process::exit(1);
        }
        let entry = self.shared.heap.alloc(size_of::<&[u8]>()) as *mut &'static [u8];
        // SAFETY: fresh, aligned and sized for one slice reference.
        unsafe { entry.write(leaked) };
        // SAFETY: id < MAX_SYMS indexes the mappings. The prefix is stored
        // first, so the release of the name publishes it too.
        unsafe {
            (*self.shared.prefixes.add(id as usize)).store(prefix(name), Ordering::Relaxed);
            (*self.shared.names.add(id as usize)).store(entry, Ordering::Release);
        }
        w.insert(leaked, Sym(id));
        (leaked, Sym(id))
    }
    /// A lookup that does not intern. Used by the interner's own tests.
    pub fn get(&self, name: &[u8]) -> Option<Sym> {
        self.shard(hash_bytes(name))
            .read()
            .unwrap()
            .get(name)
            .copied()
    }

    pub fn name(&self, s: Sym) -> &'static [u8] {
        assert!((s.0 as usize) < MAX_SYMS);
        // SAFETY: ids come from `intern`, whose entry store happened before
        // the id was published.
        unsafe {
            let p = (*self.shared.names.add(s.0 as usize)).load(Ordering::Acquire);
            assert!(!p.is_null(), "unknown symbol {}", s.0);
            *p
        }
    }

    /// The order of names by bytes, as far as their first 8 bytes decide
    /// it; on a tie, compare the names.
    #[inline]
    pub fn prefix(&self, s: Sym) -> u64 {
        assert!((s.0 as usize) < MAX_SYMS);
        // SAFETY: as in `name`; the prefix was stored before the id was
        // published.
        unsafe { (*self.shared.prefixes.add(s.0 as usize)).load(Ordering::Relaxed) }
    }

    pub fn name_str(&self, s: Sym) -> &'static str {
        from_utf8(self.name(s)).unwrap_or("<non-utf8>")
    }

    pub fn len(&self) -> usize {
        self.shared.next.load(Ordering::Relaxed) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Interner {
        Interner::in_heap(Arc::new(Heap::default()))
    }

    use std::thread;

    /// A prefix never contradicts byte order: ordering by prefix, then by
    /// name on a tie, is ordering by name.
    #[test]
    fn prefix_order_agrees_with_byte_order() {
        let mut i = fresh();
        let names: [&[u8]; 10] = [
            b"",
            b"a",
            b"a\0",
            b"a\0b",
            b"ab",
            b"abcdefgh",
            b"abcdefghi",
            b"abcdefgg",
            b"\xff",
            b"B",
        ];
        let syms: Vec<Sym> = names.iter().map(|n| i.intern(n)).collect();
        for (&x, &a) in names.iter().zip(&syms) {
            for (&y, &b) in names.iter().zip(&syms) {
                let by_prefix = i.prefix(a).cmp(&i.prefix(b)).then_with(|| x.cmp(y));
                assert_eq!(by_prefix, x.cmp(y), "{x:?} vs {y:?}");
            }
        }
    }

    #[test]
    fn intern_is_stable_and_shared() {
        let mut i = fresh();
        let a = i.intern(b"foo");
        let b = i.intern(b"bar");
        assert_ne!(a, b);
        assert_eq!(i.intern(b"foo"), a);
        assert_eq!(i.name(a), b"foo");
        assert_eq!(i.get(b"bar"), Some(b));
        assert_eq!(i.get(b"baz"), None);
        let mut j = i.clone();
        let c = j.intern(b"baz");
        assert_eq!(i.get(b"baz"), Some(c));
        assert_eq!(i.len(), 3);
    }

    #[test]
    fn empty_name_misses_the_cache_sentinel() {
        let mut i = fresh();
        let e = i.intern(b"");
        assert_eq!(i.name(e), b"");
        assert_eq!(i.intern(b""), e);
        assert_eq!(i.len(), 1);
    }

    #[test]
    fn concurrent_intern_agrees() {
        let i = fresh();
        let hs: Vec<_> = (0..8)
            .map(|t| {
                let mut i = i.clone();
                thread::spawn(move || {
                    let mut v = Vec::new();
                    for k in 0..5000u32 {
                        let name = format!("sym{}", (k * 7 + t) % 3000);
                        v.push((name.clone(), i.intern(name.as_bytes())));
                    }
                    v
                })
            })
            .collect();
        for h in hs {
            for (name, s) in h.join().unwrap() {
                assert_eq!(i.name(s), name.as_bytes());
                assert_eq!(i.get(name.as_bytes()), Some(s));
            }
        }
        assert_eq!(i.len(), 3000);
    }
}
