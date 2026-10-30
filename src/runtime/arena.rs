//! Bump allocator. Memory is never freed while its session lives. Every
//! allocation gives a `'static` reference, and the chunks go only when the
//! `Heap` of the session drops. Thus it is sound for cells to share any
//! part of the graph with no further check.

use std::sync::Arc;
use std::{process, ptr, slice};

#[cfg(feature = "stats2")]
use super::census::Census;
use super::value::{Ctx, ListObj};
use super::word::Word;
use crate::parallel::shared::ArenaUse;
use crate::session::Session;
use crate::syntax::symbol::{FxHashMap, FxHashSet};
use crate::sys::heap::Heap;
use crate::sys::{self, vm};

const CHUNK: usize = 32 << 20;

// Call it on the thread that fills the chunk: zeroing leaves the chunk in
// that core's L3. On a helper thread on another CCD, one worker ran 20 % slower.
fn map_chunk(chunk: usize) -> usize {
    // SAFETY: prefaulting only changes how the chunk's own pages are backed.
    unsafe {
        let Ok(p) = vm::map(chunk, false) else {
            // no unwinding from here: other fibers hold arena pointers,
            // and Nix ends the same way on a heap it cannot grow
            errln!("error: out of memory");
            process::exit(1);
        };
        sys::prefault(p.cast(), chunk);
        p as usize
    }
}

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
    /// String bytes by content, and the context-free string object over
    /// them once one was made (EMPTY until then). nixpkgs builds the same
    /// text millions of times (docs/explanation/evaluation.md).
    pub(crate) strings: FxHashMap<u64, StrEntry>,
    /// Context slices by content.
    pub(crate) ctxs: FxHashSet<&'static [Ctx]>,
    /// The session memory the chunks belong to; `None` for a placeholder
    /// until it first allocates.
    heap: Option<Arc<Heap>>,
    /// The one `[ ]` and the one `{ }`, made on first use. Nothing writes
    /// to an object without entries, so every empty one can be this one.
    pub(crate) empty_list: Option<&'static ListObj>,
    pub(crate) empty_attrs: Option<Word>,
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
    /// An arena in the memory of the session this thread entered.
    pub(crate) fn new() -> Self {
        Arena::in_heap(Session::current().heap.clone())
    }

    pub(crate) fn in_heap(heap: Arc<Heap>) -> Self {
        let mut a = Arena::placeholder();
        a.strings = FxHashMap::with_capacity_and_hasher(1 << 16, Default::default());
        a.heap = Some(heap);
        a
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
            strings: FxHashMap::default(),
            ctxs: FxHashSet::default(),
            heap: None,
            empty_list: None,
            empty_attrs: None,
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
    /// Every reference and `Word` over memory allocated since `m` must be dead:
    /// the next allocation hands the same bytes out again, as a different type.
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
        // Anonymous mmap, prefaulted in one syscall: MADV_POPULATE_WRITE
        // zeroes the chunk inside the kernel without one trap per page. The
        // chunk goes with the session's heap.
        let base = map_chunk(chunk) as *mut u8;
        self.heap
            .get_or_insert_with(|| Session::current().heap.clone())
            .adopt(base as usize, chunk);
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
