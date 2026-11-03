//! Anonymous memory: mapping, giving back and fencing off address space.

use std::fs::File;
use std::{fmt, io, ptr, slice};

use rustix::mm::{MapFlags, MprotectFlags, ProtFlags, mmap_anonymous, munmap};

use crate::sys::discard;

/// Bytes of `[base, base + len)` that are resident, in whole pages.
pub fn resident(base: *mut u8, len: usize) -> usize {
    let page = page_size();
    let mut v = vec![0u8; len / page];
    // SAFETY: one byte per page of the range, which the caller maps. The
    // vector's element type is `c_char` or `u8` depending on the platform.
    unsafe { libc::mincore(base.cast(), len, v.as_mut_ptr().cast()) };
    v.iter().filter(|b| **b & 1 != 0).count() * page
}

/// Hands the pages back and leaves fresh zero pages in their place.
///
/// # Safety
/// The range is a mapping of the caller's and nothing in it is needed.
pub unsafe fn reset(p: *mut u8, len: usize) {
    let _ = unsafe {
        mmap_anonymous(
            p.cast(),
            len,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::PRIVATE | MapFlags::FIXED,
        )
    };
}

/// A file mapped read-only, unmapped when dropped: a spooled tarball, which
/// every pass over its members reads in place of a `pread` per member.
pub struct FileMap {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: a read-only mapping; nothing writes through it.
unsafe impl Send for FileMap {}
// SAFETY: as above.
unsafe impl Sync for FileMap {}

impl FileMap {
    /// The first `len` bytes of `file`, or `None` when they cannot be mapped
    /// (and the caller reads the file instead).
    ///
    /// # Safety
    /// Nothing truncates `file` while the map lives: a read past its end
    /// would be `SIGBUS`.
    pub unsafe fn new(file: &File, len: u64) -> Option<FileMap> {
        if file.metadata().ok()?.len() < len {
            return None;
        }
        let len = usize::try_from(len).ok().filter(|&n| n > 0)?;
        // SAFETY: a fresh read-only mapping of a file the caller owns and does
        // not truncate while the mapping lives.
        let p = unsafe {
            rustix::mm::mmap(
                ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::SHARED,
                file,
                0,
            )
        }
        .ok()?;
        // the index reads it front to back, the passes after it in tree order
        // SAFETY: the range is the mapping just made.
        let _ = unsafe { rustix::mm::madvise(p, len, rustix::mm::Advice::WillNeed) };
        Some(FileMap { ptr: p.cast(), len })
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `len` mapped, readable bytes, alive as long as `self`.
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for FileMap {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made; nothing borrows it past `self`.
        let _ = unsafe { munmap(self.ptr.cast(), self.len) };
    }
}

impl fmt::Debug for FileMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileMap({} bytes)", self.len)
    }
}

pub fn page_size() -> usize {
    rustix::param::page_size()
}

/// `len` zeroed bytes, readable and writable. A `lazy` mapping reserves
/// address space and charges nothing until the pages are touched.
pub fn map(len: usize, lazy: bool) -> io::Result<*mut u8> {
    let flags = if lazy {
        MapFlags::PRIVATE | MapFlags::NORESERVE
    } else {
        MapFlags::PRIVATE
    };
    // SAFETY: an anonymous mapping aliases nothing.
    let p = unsafe {
        mmap_anonymous(
            ptr::null_mut(),
            len,
            ProtFlags::READ | ProtFlags::WRITE,
            flags,
        )
    };
    Ok(p?.cast())
}

/// `len` bytes for a stack. Unix reserves it lazily and the pages come in as
/// the stack grows.
pub fn map_stack(len: usize) -> io::Result<*mut u8> {
    map(len, true)
}

pub const STACK_ZONE: usize = 0;

/// Windows' TEB stack limit; nothing reads it here.
pub fn stack_limit(base: *mut u8, _len: usize) -> *mut u8 {
    base
}

/// Bytes at the low end of a stack whose faults mean an overflow.
pub fn fault_span() -> usize {
    page_size()
}

/// Returns the pages of an idle stack, except the top `keep` bytes.
///
/// # Safety
/// `[base, base + len)` is a stack from `map_stack` that nothing runs on.
pub unsafe fn trim_stack(base: *mut u8, len: usize, keep: usize) {
    // SAFETY: the range is inside the mapping and above the guard page.
    unsafe { discard(base.add(page_size()), len - page_size() - keep) };
}

/// # Safety
/// `[p, p + len)` is a mapping of the caller's that nothing uses any more.
pub unsafe fn unmap(p: *mut u8, len: usize) {
    let _ = unsafe { munmap(p.cast(), len) };
}

/// Makes the low `len` bytes of a mapping fault when touched.
///
/// # Safety
/// The range is inside a mapping of the caller's and nothing needs it.
pub unsafe fn fence(p: *mut u8, len: usize) {
    let _ = unsafe { rustix::mm::mprotect(p.cast(), len, MprotectFlags::empty()) };
}

/// Replaces the range with address space that faults and costs nothing.
/// `false` leaves the pages as they were.
///
/// # Safety
/// The range is a mapping of the caller's and nothing may touch it again.
pub unsafe fn revoke(p: *mut u8, len: usize) -> bool {
    unsafe {
        mmap_anonymous(
            p.cast(),
            len,
            ProtFlags::empty(),
            MapFlags::PRIVATE | MapFlags::NORESERVE | MapFlags::FIXED,
        )
    }
    .is_ok()
}
