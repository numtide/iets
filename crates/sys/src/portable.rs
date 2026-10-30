use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};

use super::vm;

/// One lock and one condition for every word. The scheduler sleeps on a
/// single epoch, so there is nothing to tell apart.
static SLEEP: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

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

/// No prefault and no huge pages here: the pages come in on first touch.
///
/// # Safety
/// The range is an anonymous mapping of the caller's.
pub unsafe fn prefault(_p: *mut u8, _len: usize) {}

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
