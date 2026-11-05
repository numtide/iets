use std::fs::File;
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};

use super::vm;

/// One lock and one condition for every word. The scheduler sleeps on a
/// single epoch, so there is nothing to tell apart.
static SLEEP: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// No asynchronous writeback here (Linux's `sync_file_range`): macOS,
/// the BSDs and Windows only have blocking syncs.
pub fn start_writeback(_f: &File, _from: u64, _len: u64) {}

/// Sleeps while `*word == expected`. May return early, so the caller looks
/// again.
pub fn futex_wait(word: &AtomicU32, expected: u32) {
    let g = SLEEP.0.lock().unwrap_or_else(|e| e.into_inner());
    if word.load(Ordering::Acquire) == expected {
        drop(SLEEP.1.wait(g));
    }
}

/// Wakes the sleepers. The lock orders this after a waiter's check, so a
/// wake between the check and the sleep is not lost.
pub fn futex_wake(_word: &AtomicU32, n: u32) {
    let _g = SLEEP.0.lock().unwrap_or_else(|e| e.into_inner());
    if n == 1 {
        SLEEP.1.notify_one();
    } else {
        SLEEP.1.notify_all();
    }
}

/// No huge pages here.
///
/// # Safety
/// The range is an anonymous mapping of the caller's.
pub unsafe fn advise_huge(_p: *mut u8, _len: usize) {}

/// Linux only: the mapping of the allocator's first arena.
pub fn note_allocator_arena(_p: *const u8) {}

/// Gives the pages back and leaves fresh zero pages in their place.
///
/// # Safety
/// The range is an anonymous mapping of the caller's, and nothing in it is
/// needed any more.
pub unsafe fn discard(p: *mut u8, len: usize) {
    unsafe { vm::reset(p, len) };
}

/// Never proven here, so callers walk the path themselves.
pub fn beneath(_root: &File, _rel: &Path, _follow: bool) -> Option<bool> {
    None
}

/// No fork here: the exit stays in the foreground (`linux.rs`).
pub struct ExitNote;

pub fn detach_exit() -> Option<ExitNote> {
    None
}

impl ExitNote {
    pub fn exit(self, code: i32) -> ! {
        process::exit(code)
    }
}
