use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
use rustix::io::Errno;
use rustix::mm::{Advice, madvise};
use rustix::thread::futex;

/// Sleeps while `*word == expected`. Returns early when the word moved or a
/// signal arrived. The caller looks again.
pub fn futex_wait(word: &AtomicU32, expected: u32) {
    let _ = futex::wait(word, futex::Flags::PRIVATE, expected, None);
}

/// Starts writing `len` bytes of `f` from `from` to disk without waiting
/// for them, so a later sync waits for less. rustix has no wrapper.
pub fn start_writeback(f: &File, from: u64, len: u64) {
    // SAFETY: a plain syscall on a descriptor the caller owns, reading no memory.
    unsafe {
        libc::sync_file_range(
            f.as_raw_fd(),
            from as i64,
            len as i64,
            libc::SYNC_FILE_RANGE_WRITE,
        )
    };
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
    if !gate_closes(*HUGE_BASE.get_or_init(|| n), n) {
        return true;
    }
    if !HUGE_CLOSED.swap(true, Ordering::Relaxed) {
        unadvise_allocator();
    }
    false
}

/// A read older than the base, by a worker that lost the race to set it,
/// counts as no failures.
fn gate_closes(base: u64, n: u64) -> bool {
    n.saturating_sub(base) > HUGE_FALLBACKS_MAX
}

fn vmstat(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
}

/// The mapping of mimalloc's first arena, which it advises huge before
/// `main` runs. It keeps its huge pages until the gate closes.
static ALLOC_ARENA: OnceLock<(usize, usize)> = OnceLock::new();

/// Notes the mapping that holds `p`, an allocation made at start.
pub fn note_allocator_arena(p: *const u8) {
    if let Some(r) = fs::read_to_string("/proc/self/maps")
        .ok()
        .and_then(|m| mapping_of(&m, p as usize))
    {
        let _ = ALLOC_ARENA.set(r);
    }
}

fn mapping_of(maps: &str, p: usize) -> Option<(usize, usize)> {
    maps.lines().find_map(|l| {
        let (a, b) = l.split_once(' ')?.0.split_once('-')?;
        let r = (
            usize::from_str_radix(a, 16).ok()?,
            usize::from_str_radix(b, 16).ok()?,
        );
        (r.0 <= p && p < r.1).then_some(r)
    })
}

fn unadvise_allocator() {
    if let Some(&(a, b)) = ALLOC_ARENA.get() {
        // SAFETY: advice on the allocator's own mapping keeps its contents.
        let _ = unsafe { madvise(a as *mut _, b - a, Advice::LinuxNoHugepage) };
    }
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

/// The write end of the pipe a forked child sends its exit code through
/// (`detach_exit`).
pub struct ExitNote(libc::c_int);

/// Forks so the parent can exit with the child's code before the kernel has
/// freed the child's heap (about a second for 22 GB). Returns in the child,
/// or `None` when the fork failed; the parent never returns. The child dies
/// with the parent, and one that ends without a note (an error exit, a crash,
/// an exec) is waited for and passed on. Call it while one thread runs.
pub fn detach_exit() -> Option<ExitNote> {
    let mut fds = [0; 2];
    // SAFETY: a plain syscall writing two descriptors into `fds`.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    let parent = process::id() as libc::pid_t;
    // an ignored SIGCHLD (inherited across exec) has the kernel reap the
    // child, and the waitpid below would get ECHILD instead of its status
    // SAFETY: setting a signal's disposition.
    let chld = unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };
    // SAFETY: one thread runs (the caller's contract), so the child's copy of
    // the process is whole.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        // SAFETY: closing the descriptors pipe2 just made, and putting back
        // the disposition.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
            libc::signal(libc::SIGCHLD, chld);
        }
        return None;
    }
    if pid == 0 {
        // SAFETY: plain syscalls on the child's own state.
        unsafe {
            libc::signal(libc::SIGCHLD, chld);
            libc::close(fds[0]);
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            // the parent died before the prctl took
            if libc::getppid() != parent {
                libc::_exit(1);
            }
        }
        return Some(ExitNote(fds[1]));
    }
    // SAFETY: the parent's end of the pipe.
    unsafe { libc::close(fds[1]) };
    forward_signals(pid);
    let mut code = 0u8;
    let n = loop {
        // SAFETY: reads at most one byte into `code`.
        let n = unsafe { libc::read(fds[0], (&raw mut code).cast(), 1) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        break n;
    };
    if n == 1 {
        // SAFETY: exit without the destructors of a process that did nothing.
        unsafe { libc::_exit(code as libc::c_int) };
    }
    let mut status = 0;
    // SAFETY: waits for our own child.
    while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            // SAFETY: as above.
            unsafe { libc::_exit(1) };
        }
    }
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        // SAFETY: the default action of the signal the child died of, on us;
        // not dumpable, so a child's crash leaves its core dump alone and
        // not a second one of this process that did nothing.
        unsafe {
            libc::prctl(libc::PR_SET_DUMPABLE, 0);
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
    // SAFETY: as above.
    unsafe { libc::_exit(libc::WEXITSTATUS(status)) }
}

/// The child `detach_exit` waits for.
static DETACHED_CHILD: AtomicI32 = AtomicI32::new(0);

/// Passes the signals that end a command on to the child, and waits on: the
/// parent dies of the child's signal only once the child has, so a Ctrl-C
/// gives the shell its prompt after the child has restored the terminal
/// (`tty::guard_region`), and a signal sent to the parent alone reaches the
/// work. A signal ignored at start stays ignored, as in `guard_region`.
fn forward_signals(child: libc::pid_t) {
    extern "C" fn forward(sig: libc::c_int) {
        // SAFETY: kill(2) is async-signal-safe.
        unsafe { libc::kill(DETACHED_CHILD.load(Ordering::Relaxed), sig) };
    }
    DETACHED_CHILD.store(child, Ordering::Relaxed);
    for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only calls kill(2).
        unsafe {
            let old = libc::signal(sig, forward as *const () as libc::sighandler_t);
            if old != libc::SIG_DFL {
                libc::signal(sig, old);
            }
        }
    }
}

impl ExitNote {
    /// Flushes and closes stdout and stderr, so a reader sees the end of the
    /// output, sends `code` to the parent and exits. The parent exits with
    /// `code` at once; this process's teardown goes on without it.
    pub fn exit(self, code: i32) -> ! {
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        // SAFETY: plain syscalls; /dev/null takes the place of the closed
        // streams for anything the exit still writes.
        unsafe {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if null >= 0 {
                libc::dup2(null, 1);
                libc::dup2(null, 2);
            }
            let b = code as u8;
            libc::write(self.0, (&raw const b).cast(), 1);
        }
        process::exit(code)
    }
}

#[cfg(test)]
mod tests {
    use super::{HUGE_FALLBACKS_MAX, gate_closes, mapping_of, vmstat};

    #[test]
    fn vmstat_reads_a_counter_by_its_whole_name() {
        let text = "thp_fault_fallback_charge 7\nthp_fault_fallback 42\nnr_free_pages 1\n";
        assert_eq!(vmstat(text, "thp_fault_fallback"), Some(42));
        assert_eq!(vmstat(text, "thp_fault_fallback_charge"), Some(7));
        assert_eq!(vmstat(text, "compact_stall"), None);
    }

    #[test]
    fn an_older_counter_read_keeps_the_gate_open() {
        assert!(!gate_closes(100, 99));
        assert!(!gate_closes(100, 100 + HUGE_FALLBACKS_MAX));
        assert!(gate_closes(100, 101 + HUGE_FALLBACKS_MAX));
    }

    #[test]
    fn mapping_of_finds_the_range_holding_an_address() {
        let maps = "1000-2000 rw-p 0 00:00 0\n7f00-8f00 rw-p 0 00:00 0 [heap]\n";
        assert_eq!(mapping_of(maps, 0x7f10), Some((0x7f00, 0x8f00)));
        assert_eq!(mapping_of(maps, 0x2000), None);
    }
}
