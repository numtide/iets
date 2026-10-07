//! Bump allocator. Memory is never freed: the evaluator's whole heap lives
//! until process exit, so every allocation hands out a `'static` reference.
//! That is what makes graph sharing between cells trivially sound.

use std::time::{Duration, Instant};
use std::{process, ptr, slice};

#[cfg(feature = "stats2")]
use super::census::Census;
use super::value::Ctx;
use super::word::Word;
use crate::parallel::shared::ArenaUse;
use crate::syntax::symbol::{FxHashMap, FxHashSet};

const CHUNK: usize = 32 << 20;
/// A 32 MiB chunk populates in ~1 ms with huge pages and ~16 ms with 4 KiB
/// pages; anything slower than this is a compaction stall.
const THP_STALL: Duration = Duration::from_millis(6);

pub struct Arena {
    ptr: *mut u8,
    end: *mut u8,
    pub(crate) bytes_allocated: usize,
    /// Total chunk bytes mapped (and populated) so far.
    pub(crate) mapped: usize,
    pub(crate) stats: Stats,
    /// Boxed: `Arena` is swapped on every fiber switch, so the marks must
    /// not add their 448 bytes to that move.
    folded: Box<Folded>,
    /// Advise huge pages until a chunk populate stalls in compaction.
    thp: bool,
    /// String bytes by content, and the context-free string object over
    /// them once one was made (EMPTY until then). nixpkgs builds the same
    /// text millions of times (docs/explanation/evaluation.md).
    pub(crate) strings: FxHashMap<u64, StrEntry>,
    /// Context slices by content.
    pub(crate) ctxs: FxHashSet<&'static [Ctx]>,
}

/// Position of the bump pointer, to give back the last allocation.
#[derive(Clone, Copy)]
pub struct Mark {
    ptr: *mut u8,
    end: *mut u8,
    bytes: usize,
}

impl Default for Arena {
    fn default() -> Self {
        Self::new()
    }
}

impl Arena {
    pub(crate) fn new() -> Self {
        Arena {
            strings: FxHashMap::with_capacity_and_hasher(1 << 16, Default::default()),
            ..Arena::placeholder()
        }
    }

    /// An arena that reserves nothing, for a slot that a real arena is
    /// swapped into before anything allocates from it.
    pub(crate) fn placeholder() -> Self {
        Arena {
            ptr: ptr::null_mut(),
            end: ptr::null_mut(),
            bytes_allocated: 0,
            mapped: 0,
            stats: Stats::default(),
            folded: Box::default(),
            thp: true,
            strings: FxHashMap::default(),
            ctxs: FxHashSet::default(),
        }
    }

    /// Whether a fold would carry anything. The placeholder arena never
    /// allocates, so every switch into a fiber answers false here.
    #[inline]
    pub(crate) fn allocated_since_fold(&self) -> bool {
        self.bytes_allocated != self.folded.bytes
    }

    /// The bytes this arena allocated since the last sample, marked taken.
    /// `release` and `shrink_last` give bytes back, so a total can fall
    /// below its own mark; the sample saturates at nothing there.
    pub(crate) fn bytes_since_sample(&mut self) -> u64 {
        let d = self.bytes_allocated.saturating_sub(self.folded.sample);
        self.folded.sample = self.bytes_allocated;
        d as u64
    }

    /// The arena use not yet folded into the run's totals, marked folded.
    ///
    /// `release` and `shrink_last` give bytes back, so a total can fall below
    /// its own mark. The fold saturates there and keeps whatever it published,
    /// which overstates by the bytes given back since the last fold.
    pub(crate) fn take_unfolded(&mut self) -> ArenaUse {
        let mut u = ArenaUse {
            bytes: self.bytes_allocated.saturating_sub(self.folded.bytes) as u64,
            mapped: self.mapped.saturating_sub(self.folded.mapped) as u64,
            ..Default::default()
        };
        self.folded.bytes = self.bytes_allocated;
        self.folded.mapped = self.mapped;
        for i in 0..NSTATS {
            u.counts[i] = self.stats.counts[i].saturating_sub(self.folded.counts[i]);
            u.kind_bytes[i] = self.stats.bytes[i].saturating_sub(self.folded.kind_bytes[i]);
        }
        self.folded.counts = self.stats.counts;
        self.folded.kind_bytes = self.stats.bytes;
        u
    }

    #[inline]
    pub(crate) fn mark(&self) -> Mark {
        Mark {
            ptr: self.ptr,
            end: self.end,
            bytes: self.bytes_allocated,
        }
    }

    /// Give back everything allocated since `m`, when it is still the
    /// tail of the current chunk.
    ///
    /// # Safety
    ///
    /// Every reference and `Word` over memory allocated since `m` must be
    /// dead. The next allocation hands the same bytes out again, as a
    /// different type; `alloc` would then return a second `&'static mut`
    /// to memory a live reference still names.
    #[inline]
    pub(crate) unsafe fn release(&mut self, m: Mark) {
        if self.end == m.end {
            self.ptr = m.ptr;
            self.bytes_allocated = m.bytes;
        }
    }

    /// Gives the unused tail of the allocation that is still at the bump
    /// pointer back, so a copy sized at an upper bound costs what it used.
    ///
    /// # Safety
    ///
    /// The caller's allocation must be the last one and its last `tail`
    /// bytes must never be read or written.
    #[inline]
    pub(crate) unsafe fn shrink_last(&mut self, tail: usize, bucket: Stat) {
        self.ptr = unsafe { self.ptr.sub(tail) };
        self.bytes_allocated -= tail;
        self.stats.unnote(bucket, tail);
    }

    #[inline]
    pub(crate) fn alloc_raw(&mut self, size: usize, align: usize) -> *mut u8 {
        debug_assert!(align <= 16);
        let start = (self.ptr as usize + align - 1) & !(align - 1);
        let new_end = start + size;
        if new_end > self.end as usize || self.ptr.is_null() {
            return self.alloc_slow(size, align);
        }
        self.ptr = new_end as *mut u8;
        self.bytes_allocated += size;
        start as *mut u8
    }

    #[cold]
    fn alloc_slow(&mut self, size: usize, align: usize) -> *mut u8 {
        let chunk = CHUNK.max((size + align + CHUNK - 1) & !(CHUNK - 1));
        // Anonymous mmap, prefaulted in one syscall. Transparent huge pages
        // were tried first: on a fragmented box every huge fault that misses
        // a free 2 MiB block stalls in direct compaction and then falls back
        // to 4 KiB pages anyway, which cost ~1 s of system time per GiB.
        // MADV_POPULATE_WRITE zeroes the chunk inside the kernel without one
        // trap per page. The chunk is intentionally leaked.
        let base = unsafe {
            let p = libc::mmap(
                ptr::null_mut(),
                chunk,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if p == libc::MAP_FAILED {
                // no unwinding from here: other fibers hold arena pointers,
                // and Nix ends the same way on a heap it cannot grow
                errln!("error: out of memory");
                process::exit(1);
            }
            let t0 = Instant::now();
            if self.thp {
                libc::madvise(p, chunk, libc::MADV_HUGEPAGE);
            }
            libc::madvise(p, chunk, libc::MADV_POPULATE_WRITE);
            if self.thp && t0.elapsed() > THP_STALL {
                self.thp = false;
            }
            p as *mut u8
        };
        self.mapped += chunk;
        self.ptr = base;
        self.end = unsafe { base.add(chunk) };
        let start = (self.ptr as usize + align - 1) & !(align - 1);
        self.ptr = (start + size) as *mut u8;
        self.bytes_allocated += size;
        start as *mut u8
    }

    #[inline]
    pub(crate) fn alloc<T>(&mut self, v: T) -> &'static mut T {
        let p = self.alloc_raw(size_of::<T>(), align_of::<T>()) as *mut T;
        // SAFETY: p is fresh, aligned, and sized for T.
        unsafe {
            p.write(v);
            &mut *p
        }
    }

    #[inline]
    pub(crate) fn alloc_slice<T: Copy>(&mut self, items: &[T]) -> &'static mut [T] {
        if items.is_empty() {
            return &mut [];
        }
        let p = self.alloc_raw(size_of_val(items), align_of::<T>()) as *mut T;
        // SAFETY: p is fresh and sized for items.len() elements of T.
        unsafe {
            ptr::copy_nonoverlapping(items.as_ptr(), p, items.len());
            slice::from_raw_parts_mut(p, items.len())
        }
    }

    /// Move the elements of `v` into one arena slice.
    pub(crate) fn alloc_vec<T>(&mut self, v: Vec<T>) -> &'static [T] {
        let n = v.len();
        if n == 0 {
            return &[];
        }
        let p = self.alloc_raw(n * size_of::<T>(), align_of::<T>()) as *mut T;
        // SAFETY: fresh allocation of n elements; values moved in.
        unsafe {
            for (i, e) in v.into_iter().enumerate() {
                p.add(i).write(e);
            }
            slice::from_raw_parts(p, n)
        }
    }

    #[inline]
    pub(crate) fn alloc_bytes(&mut self, b: &[u8]) -> &'static [u8] {
        self.alloc_slice(b)
    }

    /// Concatenate byte slices into one arena allocation.
    pub(crate) fn alloc_concat(&mut self, parts: &[&[u8]]) -> &'static [u8] {
        let len: usize = parts.iter().map(|p| p.len()).sum();
        if len == 0 {
            return &[];
        }
        let p = self.alloc_raw(len, 1);
        let mut off = 0;
        for part in parts {
            // SAFETY: total length was computed from the same parts.
            unsafe { ptr::copy_nonoverlapping(part.as_ptr(), p.add(off), part.len()) };
            off += part.len();
        }
        // SAFETY: fully initialised above.
        unsafe { slice::from_raw_parts(p, len) }
    }
}

/// Allocation kind for `IETS_STATS`.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Stat {
    Thunk,
    ThunkApp,
    ThunkApp2,
    Env,
    ListLit,
    ListMap,
    ListGen,
    ListConcat,
    List,
    Attrs,
    AttrsLit,
    AttrsUpd,
    AttrsMap,
    AttrsListTo,
    AttrsRemove,
    AttrsZip,
    AttrsInter,
    StrObj,
    Bytes,
    Ctx,
    Path,
    Float,
    BigInt,
    Lambda,
    PrimOpApp,
    ThunkMap,
    Ir,
}

pub const NSTATS: usize = 27;

pub(crate) const STAT_NAMES: [&str; NSTATS] = [
    "thunk",
    "thunk_app",
    "thunk_app2",
    "env",
    "list_lit",
    "list_map",
    "list_gen",
    "list_concat",
    "list_other",
    "attrs_other",
    "attrs_lit",
    "attrs_upd",
    "attrs_map",
    "attrs_listto",
    "attrs_remove",
    "attrs_zip",
    "attrs_inter",
    "str_obj",
    "str_bytes",
    "ctx",
    "path",
    "float",
    "bigint",
    "lambda",
    "primop_app",
    "thunk_map",
    "ir",
];

impl Stat {
    /// The census origin byte of an attrset producer.
    pub(crate) fn attrs_origin(self) -> u8 {
        match self {
            Stat::AttrsLit => 1,
            Stat::AttrsUpd => 2,
            Stat::AttrsMap => 3,
            Stat::AttrsListTo => 4,
            Stat::AttrsRemove => 5,
            Stat::AttrsZip => 6,
            Stat::AttrsInter => 7,
            _ => 0,
        }
    }
}

/// The marks of the last fold, so a switch-out adds a delta and never the
/// running total again.
#[derive(Default)]
struct Folded {
    /// Mark for the allocation profile, which weighs a sample by the bytes
    /// since the one before it. In this box because the box is already paid
    /// for by the fiber switch.
    sample: usize,
    bytes: usize,
    mapped: usize,
    counts: [u64; NSTATS],
    kind_bytes: [u64; NSTATS],
}

#[derive(Default)]
pub struct Stats {
    pub counts: [u64; NSTATS],
    pub bytes: [u64; NSTATS],
    #[cfg(feature = "stats2")]
    pub census: Census,
}

impl Stats {
    #[inline]
    pub(crate) fn note(&mut self, s: Stat, bytes: usize) {
        self.counts[s as usize] += 1;
        self.bytes[s as usize] += bytes as u64;
    }

    /// Bytes an allocation asked for and gave back; the count stands.
    pub(crate) fn unnote(&mut self, s: Stat, bytes: usize) {
        self.bytes[s as usize] -= bytes as u64;
    }
}

/// One distinct string content: its arena bytes and the context-free
/// string object over them once one was made (EMPTY until then).
#[derive(Clone, Copy)]
pub struct StrEntry {
    pub bytes: &'static [u8],
    pub word: Word,
}

/// What `Arena::intern_bytes` hands back: the shared bytes, the shared
/// context-free object if any, and the table key (0 when the content is
/// not in the table because of a hash collision).
#[derive(Clone, Copy)]
pub struct Interned {
    pub bytes: &'static [u8],
    pub word: Word,
    pub hash: u64,
}
