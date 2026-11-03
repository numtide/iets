use std::fs::{self, File};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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

/// Failed huge-page faults (`thp_fault_fallback`) after which chunks are no
/// longer advised. Once the free 2 MB blocks are used up, each huge fault
/// stalls in direct compaction and then fails, and workers doing that at
/// once queue on the zone lock. The counter is machine-wide, so another
/// process's failures close the gate too: they say the same about free
/// memory.
const HUGE_FALLBACKS_MAX: u64 = 64;

static HUGE_BASE: OnceLock<u64> = OnceLock::new();
static HUGE_CLOSED: AtomicBool = AtomicBool::new(false);

fn huge() -> bool {
    if HUGE_CLOSED.load(Ordering::Relaxed) {
        return false;
    }
    let Some(n) = fs::read_to_string("/proc/vmstat")
        .ok()
        .and_then(|s| vmstat(&s, "thp_fault_fallback"))
    else {
        return true;
    };
    if n.wrapping_sub(*HUGE_BASE.get_or_init(|| n)) <= HUGE_FALLBACKS_MAX {
        return true;
    }
    HUGE_CLOSED.store(true, Ordering::Relaxed);
    false
}

fn vmstat(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
}

/// Advises `[p, p + len)` to be huge pages while the kernel can fill them.
/// The pages fault in on first touch: populating the range up front
/// (`MADV_POPULATE_WRITE`) made the workers take turns in the kernel.
///
/// # Safety
/// The range is an anonymous mapping of the caller's.
pub unsafe fn advise_huge(p: *mut u8, len: usize) {
    if huge() {
        let _ = unsafe { madvise(p.cast(), len, Advice::LinuxHugepage) };
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
    use super::vmstat;

    #[test]
    fn vmstat_reads_a_counter_by_its_whole_name() {
        let text = "thp_fault_fallback_charge 7\nthp_fault_fallback 42\nnr_free_pages 1\n";
        assert_eq!(vmstat(text, "thp_fault_fallback"), Some(42));
        assert_eq!(vmstat(text, "thp_fault_fallback_charge"), Some(7));
        assert_eq!(vmstat(text, "compact_stall"), None);
    }
}
