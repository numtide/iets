use std::fs::{self, File};
use std::path::Path;
use std::sync::LazyLock;
use std::sync::atomic::AtomicU32;

use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
use rustix::io::Errno;
use rustix::mm::{Advice, madvise};
use rustix::thread::futex;

/// Sleeps while `*word == expected`. Returns early when the word moved or a
/// signal arrived. The caller looks again.
pub fn futex_wait(word: &AtomicU32, expected: u32) {
    let _ = futex::wait(word, futex::Flags::PRIVATE, expected, None);
}

/// Wakes up to `n` threads sleeping on `word`.
pub fn futex_wake(word: &AtomicU32, n: u32) {
    let _ = futex::wake(word, futex::Flags::PRIVATE, n);
}

/// Free memory below which huge pages are not asked for. A huge page the
/// kernel has no free block for is made by direct compaction in the
/// faulting thread, and workers doing that at once queue on the zone lock:
/// on a fragmented box eight workers were slower than four.
const HUGE_FREE_MIN: u64 = 1 << 30;

/// Whether the kernel has free blocks for huge pages, read once per run.
static HUGE: LazyLock<bool> = LazyLock::new(|| {
    let page = rustix::param::page_size() as u64;
    fs::read_to_string("/proc/buddyinfo").is_ok_and(|s| huge_free(&s, page) >= HUGE_FREE_MIN)
});

/// Bytes free in blocks of order 9 and up, the size of a huge page.
fn huge_free(buddyinfo: &str, page: u64) -> u64 {
    let mut n = 0;
    for line in buddyinfo.lines() {
        let counts = line.split_whitespace().skip(4);
        for (order, c) in counts.enumerate().skip(9) {
            n += c.parse::<u64>().unwrap_or(0) * (page << order);
        }
    }
    n
}

/// Populates `[p, p + len)` now, with huge pages where the kernel has free
/// blocks for them, so the first touch does not fault.
///
/// # Safety
/// The range is an anonymous mapping of the caller's.
pub unsafe fn prefault(p: *mut u8, len: usize) {
    unsafe {
        if *HUGE {
            let _ = madvise(p.cast(), len, Advice::LinuxHugepage);
        }
        let _ = madvise(p.cast(), len, Advice::LinuxPopulateWrite);
    }
}

/// Gives the pages back. The range stays mapped and reads as zeros.
///
/// # Safety
/// The range is an anonymous mapping of the caller's, and nothing in it is
/// needed any more.
pub unsafe fn discard(p: *mut u8, len: usize) {
    let _ = unsafe { madvise(p.cast(), len, Advice::LinuxDontNeed) };
}

/// Whether resolving `rel` in `root` provably stays below it: `Some(true)`
/// when it opened there, `Some(false)` when it stopped at a missing entry,
/// which no read gets past either.
pub fn beneath(root: &File, rel: &Path, follow: bool) -> Option<bool> {
    let mut flags = OFlags::PATH | OFlags::CLOEXEC;
    if !follow {
        flags |= OFlags::NOFOLLOW;
    }
    let rel = if rel.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rel
    };
    let how = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
    match openat2(root, rel, flags, Mode::empty(), how) {
        Ok(_) => Some(true),
        Err(Errno::NOENT | Errno::NOTDIR) => Some(false),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::huge_free;

    #[test]
    fn huge_free_counts_orders_nine_and_up() {
        let info = "\
Node 0, zone      DMA      0      0      0      0      0      0      0      0      1      1      2
Node 0, zone   Normal      1   3134  91049 331647  69416     90    165    182      0      0      0
Node 1, zone   Normal  87007 104189 106281 116353 105443  88545  78588  71283  79096    711     39
";
        let page = 4096;
        assert_eq!(
            huge_free(info, page),
            (1 + 711) * (2 << 20) + (2 + 39) * (4 << 20)
        );
    }
}
