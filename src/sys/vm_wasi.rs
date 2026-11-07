//! Linear memory, which only grows. A mapping is new pages at the end of it:
//! the engine gives them zeroed, and a browser commits them when they are
//! touched. An unmapped page stays in the instance, so a page cannot fault.

use std::arch::wasm32::memory_grow;
use std::fs::File;
use std::{io, ptr};

/// Bytes of `[base, base + len)` that are resident. Nothing tells them apart
/// from the rest here.
pub fn resident(_base: *mut u8, len: usize) -> usize {
    len
}

/// # Safety
/// The range is a mapping of the caller's and nothing in it is needed.
pub unsafe fn reset(p: *mut u8, len: usize) {
    unsafe { ptr::write_bytes(p, 0, len) };
}

/// There is no way to map a file: every [`FileMap::new`] is `None`, and the
/// caller reads the file instead.
#[derive(Debug)]
pub struct FileMap;

impl FileMap {
    /// # Safety
    /// No demand on the caller here: this never maps anything.
    pub unsafe fn new(_file: &File, _len: u64) -> Option<FileMap> {
        None
    }

    pub fn bytes(&self) -> &[u8] {
        &[]
    }
}

pub fn page_size() -> usize {
    1 << 16
}

/// `len` zeroed bytes, readable and writable, aligned to a page.
pub fn map(len: usize, _lazy: bool) -> io::Result<*mut u8> {
    let page = memory_grow(0, len.div_ceil(page_size()));
    if page == usize::MAX {
        return Err(io::ErrorKind::OutOfMemory.into());
    }
    Ok(ptr::without_provenance_mut(page * page_size()))
}

/// Fibers do not exist here, so nothing runs on this.
pub fn map_stack(len: usize) -> io::Result<*mut u8> {
    map(len, true)
}

pub const STACK_ZONE: usize = 0;

pub fn stack_limit(base: *mut u8, _len: usize) -> *mut u8 {
    base
}

pub fn fault_span() -> usize {
    page_size()
}

/// # Safety
/// No demand on the caller here: this changes nothing.
pub unsafe fn trim_stack(_base: *mut u8, _len: usize, _keep: usize) {}

/// # Safety
/// No demand on the caller here: this changes nothing.
pub unsafe fn unmap(_p: *mut u8, _len: usize) {}

/// # Safety
/// No demand on the caller here: this changes nothing.
pub unsafe fn fence(_p: *mut u8, _len: usize) {}

/// Always `false`: a page cannot be taken away.
///
/// # Safety
/// No demand on the caller here: this changes nothing.
pub unsafe fn revoke(_p: *mut u8, _len: usize) -> bool {
    false
}
