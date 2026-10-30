//! The memory of one session. Every arena chunk and every interned symbol
//! name is mapped here, and all of it goes when the last handle drops
//! (docs/reference/library.md, "Sessions and memory").
//!
//! A released range is not given back to the address space: it becomes an
//! inaccessible reservation, so a `Word` kept past its session faults on
//! first use instead of reading whatever the next session put there.

use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::{mem, ptr, slice};

use super::guard;
use super::vm;

/// Name bytes come from blocks of this size; a longer name gets its own.
const NAME_BLOCK: usize = 1 << 20;

#[derive(Default)]
pub struct Heap {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    chunks: Vec<(usize, usize)>,
    /// Rust values handed out as `&'static`, dropped before the chunks go.
    kept: Vec<Box<dyn Send>>,
    /// Bump state of the current name block.
    next: usize,
    end: usize,
}

impl Heap {
    /// Takes ownership of a mapping: it is released with the heap.
    pub fn adopt(&self, base: usize, len: usize) {
        self.inner.lock().unwrap().chunks.push((base, len));
    }

    /// Bytes mapped for this session so far.
    #[cfg(any(test, feature = "testkit"))]
    pub fn mapped(&self) -> usize {
        self.inner.lock().unwrap().chunks.iter().map(|c| c.1).sum()
    }

    /// `len` bytes at alignment 8 that live as long as the session.
    pub fn alloc(&self, len: usize) -> *mut u8 {
        let len = len.max(1).next_multiple_of(8);
        let mut g = self.inner.lock().unwrap();
        if g.end - g.next < len {
            let size = NAME_BLOCK.max(len.next_multiple_of(vm::page_size()));
            let p = map(size);
            g.chunks.push((p, size));
            if size > NAME_BLOCK {
                return p as *mut u8;
            }
            g.next = p;
            g.end = p + size;
        }
        let p = g.next;
        g.next += len;
        p as *mut u8
    }

    /// `v`, owned by the session and dropped when it is released.
    pub fn keep<T: Send + Sync + 'static>(&self, v: T) -> &'static T {
        let b = Box::new(v);
        let r: *const T = &*b;
        self.inner.lock().unwrap().kept.push(b);
        // SAFETY: the box does not move its contents and lives until release.
        unsafe { &*r }
    }

    /// `b` copied into this session's memory.
    pub fn alloc_bytes(&self, b: &[u8]) -> &'static [u8] {
        let p = self.alloc(b.len());
        // SAFETY: fresh allocation of at least `b.len()` bytes, valid until
        // the heap drops, which outlives every reference by the session rule.
        unsafe {
            ptr::copy_nonoverlapping(b.as_ptr(), p, b.len());
            slice::from_raw_parts(p, b.len())
        }
    }
}

fn map(len: usize) -> usize {
    let Ok(p) = vm::map(len, false) else {
        crate::fatal("error: out of memory");
    };
    p as usize
}

impl Drop for Heap {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap();
        drop(mem::take(&mut inner.kept));
        let chunks = mem::take(&mut inner.chunks);
        if chunks.is_empty() {
            return;
        }
        guard::install_fault_handler();
        for (base, len) in chunks {
            tombstone(base, len);
        }
    }
}

/// Gives the pages of `[base, base + len)` back and keeps the range
/// reserved and inaccessible. NORESERVE and PROT_NONE cost no commit
/// charge, and adjacent tombstones merge into one mapping.
fn tombstone(base: usize, len: usize) {
    // SAFETY: the range is a mapping this heap owns; nothing may touch it
    // after release, and what does faults, which is the point.
    if !unsafe { vm::revoke(base as *mut u8, len) } {
        // the pages stay, the range stays ours: a leak, never a reuse
        return;
    }
    let n = Box::into_raw(Box::new(Released {
        base,
        len,
        next: ptr::null_mut(),
    }));
    let mut head = RELEASED.load(Ordering::Relaxed);
    loop {
        // SAFETY: `n` is ours until the exchange publishes it.
        unsafe { (*n).next = head };
        match RELEASED.compare_exchange_weak(head, n, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => break,
            Err(h) => head = h,
        }
    }
}

/// Append-only, so the fault handler walks it without a lock.
struct Released {
    base: usize,
    len: usize,
    next: *mut Released,
}

static RELEASED: AtomicPtr<Released> = AtomicPtr::new(ptr::null_mut());

/// Whether `addr` is in the memory of a released session. Async-signal-safe.
pub fn is_released(addr: usize) -> bool {
    let mut p = RELEASED.load(Ordering::Acquire);
    while !p.is_null() {
        // SAFETY: nodes are leaked and never change after publication.
        let r = unsafe { &*p };
        if addr >= r.base && addr - r.base < r.len {
            return true;
        }
        p = r.next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tombstones_every_chunk() {
        let h = Heap::default();
        let a = h.alloc_bytes(b"outPath");
        let big = h.alloc(3 << 20);
        let at = a.as_ptr() as usize;
        assert_eq!(a, b"outPath");
        assert_eq!(h.mapped(), NAME_BLOCK + (3 << 20));
        assert!(!is_released(at));
        drop(h);
        assert!(is_released(at));
        assert!(is_released(big as usize + (2 << 20)));
    }
}
