//! What `bench/ci.nu` cannot do from nushell: measure a process tree,
//! disable huge pages or pin CPUs for one command, and drop files from the
//! page cache. One file, the standard
//! library only; ci.nu builds it with `rustc -O` on every run.
//!
//! ```text
//! helper run [--thp-off] [--cpus N] [--oom-first] [--stderr FILE] [--env-file FILE] -- CMD ARG...
//!     runs CMD and prints one JSON line: wall, user and sys seconds, the
//!     peak RSS summed over the process tree (sampled every 20 ms), the
//!     largest single process's peak RSS, a hash of its output, and the
//!     exit status. Wall time ends when CMD exits, as its caller sees it;
//!     on Linux the helper then also waits for what CMD left running (it
//!     is their subreaper), whose CPU time counts, and `teardown_s` says
//!     how long that took; what still runs after ten minutes is killed
//!     and counted in `orphans_killed`. --env-file replaces the
//!     environment with the NUL-separated NAME=VALUE entries of FILE;
//!     --oom-first makes CMD the OOM killer's first choice (Linux)
//! helper fragment MIB [PIDFILE]
//!     writes its pid to PIDFILE (a new file, which no symlink can
//!     redirect: it runs as root in the user's work directory), takes MIB
//!     page by page in two halves touched in turn, so their pages
//!     interleave in physical memory, locks one half and frees the
//!     other, prints `ready`, and holds the memory until killed (Linux).
//!     Locked pages cannot be compacted once `vm.compact_unevictable_allowed`
//!     is 0, which ci.nu --privileged sets; it runs this as root
//! helper guard PID [PATH VALUE]...
//!     waits until process PID is gone (finished, interrupted or killed),
//!     then kills every `fragment` process of this helper binary and
//!     writes each VALUE to its PATH (Linux, as root): settings and the
//!     fragmenter cannot outlive the suite. Everything it will do is on
//!     its command line, fixed when root starts it: no file the user can
//!     write decides what root writes or kills hours later
//! helper evict PATH...
//!     drops the pages of every file under PATH (symlinks to the roots
//!     followed) from the page cache, and prints the files and their bytes
//!     as JSON (Linux). Whether it took shows in the next run's disk reads
//!     (`inblock`): mincore and cachestat cannot tell for files the caller
//!     may not write, such as the store's
//! ```
//!
//! `--thp-off`, `--cpus` and `evict` are Linux only and exit 2
//! elsewhere, so ci.nu can tell "unsupported" from a failure.

use std::ffi::{c_int, c_long};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io;
#[cfg(target_os = "linux")]
use std::io::Write;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{self, Command, Stdio};
#[cfg(target_os = "linux")]
use std::ptr;
use std::sync::{Arc, mpsc};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Default)]
struct Timeval {
    sec: i64,
    #[cfg(target_os = "linux")]
    usec: i64,
    #[cfg(not(target_os = "linux"))]
    usec: i32,
}

#[repr(C)]
#[derive(Default)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    maxrss: c_long,
    rest: [c_long; 13],
}

const RUSAGE_CHILDREN: c_int = -1;

const RUSAGE_SELF: c_int = 0;
const WNOHANG: c_int = 1;

unsafe extern "C" {
    fn getrusage(who: c_int, usage: *mut Rusage) -> c_int;
    fn sysconf(name: c_int) -> c_long;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{c_int, c_void};

    pub const PR_SET_THP_DISABLE: c_int = 41;
    pub const PR_SET_CHILD_SUBREAPER: c_int = 36;
    pub const POSIX_FADV_DONTNEED: c_int = 4;
    pub const PROT_READ_WRITE: c_int = 1 | 2;
    pub const MAP_PRIVATE_ANON: c_int = 0x02 | 0x20;
    pub const MADV_NOHUGEPAGE: c_int = 15;
    pub const SC_PAGESIZE: c_int = 30;
    pub const SIGKILL: c_int = 9;

    unsafe extern "C" {
        pub fn prctl(option: c_int, ...) -> c_int;
        pub fn sched_setaffinity(pid: c_int, size: usize, mask: *const u64) -> c_int;
        pub fn sched_getaffinity(pid: c_int, size: usize, mask: *mut u64) -> c_int;
        pub fn posix_fadvise(fd: c_int, off: i64, len: i64, advice: c_int) -> c_int;
        pub fn mmap(a: *mut c_void, l: usize, p: c_int, f: c_int, fd: c_int, o: i64) -> *mut c_void;
        pub fn madvise(a: *mut c_void, l: usize, advice: c_int) -> c_int;
        pub fn mlock(a: *const c_void, l: usize) -> c_int;
        pub fn munmap(a: *mut c_void, l: usize) -> c_int;
        pub fn kill(pid: c_int, sig: c_int) -> c_int;
    }
}

#[cfg(target_os = "macos")]
const SC_PAGESIZE: c_int = 29;
#[cfg(target_os = "linux")]
const SC_PAGESIZE: c_int = linux::SC_PAGESIZE;

fn usage() -> ! {
    eprintln!("usage: helper run [--thp-off] [--cpus N] [--oom-first] [--stderr FILE] [--env-file FILE] -- CMD ARG...");
    eprintln!("       helper fragment MIB [PIDFILE]");
    eprintln!("       helper guard PID [PATH VALUE]...");
    eprintln!("       helper evict PATH...");
    process::exit(64)
}

fn unsupported(what: &str) -> ! {
    eprintln!("helper: {what} is Linux only");
    process::exit(2)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => run(&args[1..]),
        Some("fragment") => fragment(&args[1..]),
        Some("guard") => guard(&args[1..]),
        Some("evict") => evict(&args[1..]),
        _ => usage(),
    }
}

// ---- run ----------------------------------------------------------------

fn run(args: &[String]) {
    let mut thp_off = false;
    let mut cpus: Option<usize> = None;
    let mut stderr_to: Option<String> = None;
    let mut env_file: Option<String> = None;
    let mut oom_first = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--thp-off" => thp_off = true,
            "--oom-first" => oom_first = true,
            "--cpus" => {
                i += 1;
                cpus = args.get(i).and_then(|s| s.parse().ok()).or_else(|| usage());
            }
            "--stderr" => {
                i += 1;
                stderr_to = Some(args.get(i).cloned().unwrap_or_else(|| usage()));
            }
            "--env-file" => {
                i += 1;
                env_file = Some(args.get(i).cloned().unwrap_or_else(|| usage()));
            }
            "--" => {
                i += 1;
                break;
            }
            _ => usage(),
        }
        i += 1;
    }
    let cmd = &args[i..];
    if cmd.is_empty() {
        usage();
    }
    if (thp_off || cpus.is_some()) && !cfg!(target_os = "linux") {
        unsupported("--thp-off and --cpus");
    }
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]).stdin(Stdio::null()).stdout(Stdio::piped());
    // a file, not the command line, so no value shows in ps or sudo's log
    if let Some(f) = &env_file {
        let s = fs::read(f).unwrap_or_else(|e| {
            eprintln!("helper: {f}: {e}");
            process::exit(1)
        });
        c.env_clear();
        for kv in s.split(|b| *b == 0) {
            let kv = String::from_utf8_lossy(kv);
            if let Some((k, v)) = kv.split_once('=')
                && !k.is_empty()
            {
                c.env(k, v);
            }
        }
    }
    match &stderr_to {
        Some(f) => {
            let file = fs::File::create(f).unwrap_or_else(|e| {
                eprintln!("helper: {f}: {e}");
                process::exit(1)
            });
            c.stderr(file);
        }
        None => {
            c.stderr(Stdio::null());
        }
    }
    let mut chosen = String::new();
    #[cfg(target_os = "linux")]
    {
        // inherited by the command: under memory pressure it is killed
        // before anything else on the machine (raising needs no privilege)
        if oom_first {
            let _ = fs::write("/proc/self/oom_score_adj", "1000");
        }
        // SAFETY: a plain prctl; orphans of the command become our
        // children, so their CPU time is counted and the next run starts
        // after them
        unsafe { linux::prctl(linux::PR_SET_CHILD_SUBREAPER, 1u64, 0u64, 0u64, 0u64) };
        let mask = cpus.map(|n| {
            let (m, list) = pick_cpus(n);
            chosen = list;
            m
        });
        // SAFETY: only async-signal-safe syscalls between fork and exec.
        unsafe {
            c.pre_exec(move || {
                if thp_off && linux::prctl(linux::PR_SET_THP_DISABLE, 1u64, 0u64, 0u64, 0u64) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if let Some(m) = &mask
                    && linux::sched_setaffinity(0, size_of_val(m), m.as_ptr()) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    // the machine-wide snapshots before the child starts, so the scan does
    // not compete with its startup
    let procs0 = proc_cpu();
    let stat0 = machine_busy_ticks();
    let self0 = rusage(RUSAGE_SELF);
    let t0 = Instant::now();
    let mut child = c.spawn().unwrap_or_else(|e| {
        eprintln!("helper: {}: {e}", cmd[0]);
        process::exit(1)
    });
    let pid = child.id();
    // the output, as a hash of its lines that ignores their order (parallel
    // workers print in any order): did both binaries compute the same?
    let out = child.stdout.take().expect("piped");
    let (hash_tx, hash_rx) = mpsc::channel();
    thread::spawn(move || hash_tx.send(hash_lines(out)));
    let psi0 = psi();
    let peak = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, done) = (peak.clone(), done.clone());
        thread::spawn(move || {
            // per thread: the last on-CPU and run-queue nanoseconds seen
            let mut sched: HashMap<u32, (u64, u64)> = HashMap::new();
            let mut tree: HashSet<u32> = HashSet::new();
            while !done.load(Ordering::Relaxed) {
                let (rss, pids) = tree_rss(pid);
                peak.fetch_max(rss, Ordering::Relaxed);
                tree.extend(pids);
                tree_sched(pid, &mut sched);
                // darwin reads the tree with `ps`, a process per sample
                thread::sleep(Duration::from_millis(if cfg!(target_os = "linux") { 20 } else { 250 }));
            }
            (sched, tree)
        })
    };
    let status = child.wait().expect("wait");
    let wall = t0.elapsed().as_secs_f64();
    done.store(true, Ordering::Relaxed);
    let (sched, mut tree) = sampler.join().unwrap_or_default();
    // what the command left running (a process that exits early and frees
    // its heap in a child): waited for, at most ten minutes, then killed
    let (teardown, orphans_killed) = reap_orphans(Duration::from_secs(600));
    // the output ends when every process holding the pipe is gone, which
    // the reaping saw to; a process outside the tree that still holds it
    // (handed the descriptor) must not hang the suite: no hash then
    let hashed = hash_rx.recv_timeout(Duration::from_secs(10)).ok();
    let (out_hash, out_lines, out_bytes) = hashed.unwrap_or_default();
    let out_hash = if hashed.is_some() { format!("{out_hash:016x}") } else { "unknown".to_string() };
    let psi1 = psi();
    let procs1 = proc_cpu();
    let self1 = rusage(RUSAGE_SELF);
    let stat1 = machine_busy_ticks();
    tree.insert(pid);
    tree.insert(process::id());
    let ru = rusage(RUSAGE_CHILDREN);
    let secs = |t: &Timeval| t.sec as f64 + t.usec as f64 / 1e6;
    // Linux reports KiB, darwin bytes
    let maxchild = if cfg!(target_os = "linux") {
        ru.maxrss as u64 * 1024
    } else {
        ru.maxrss as u64
    };
    let peak = peak.load(Ordering::Relaxed).max(maxchild);
    let (code, signal) = match status.code() {
        Some(c) => (c, 0),
        None => (-1, status.signal().unwrap_or(0)),
    };
    // thread state: on CPU, and runnable but waiting for one (Linux)
    let on_cpu: u64 = sched.values().map(|v| v.0).sum();
    // what everything else on the machine used while this ran, user
    // processes (interference) apart from kernel threads (often I/O done
    // for this run), with the heaviest by name
    let mut foreign = foreign_cpu(&procs0, &procs1, &tree);
    // the machine's busy time from /proc/stat less this run's and the
    // processes seen in both snapshots: processes that started and ended
    // in between, which the per-process scan cannot see. /proc/stat counts
    // in sampled 10 ms ticks per CPU, rusage exactly: up to a tick per CPU
    // and snapshot, and a little of the run's own time, is rounding
    if let (Some((b0, _)), Some((b1, ncpu))) = (stat0, stat1) {
        let own = secs(&ru.utime) + secs(&ru.stime) + (secs(&self1.utime) + secs(&self1.stime)) - (secs(&self0.utime) + secs(&self0.stime));
        let seen: f64 = foreign_seen_s(&procs0, &procs1, &tree);
        let short = (b1.saturating_sub(b0) as f64 / 100.0 - own - seen).max(0.0);
        foreign.push_str(&format!(",\"foreign_shortlived_cpu_s\":{short:.2},\"tick_floor_cpu_s\":{:.2}", ncpu as f64 * 0.02 + own * 0.02));
    }
    let runq: u64 = sched.values().map(|v| v.1).sum();
    let psi: Vec<String> = psi0
        .iter()
        .zip(&psi1)
        .map(|((k, a), (_, b))| format!(",\"psi_{k}_s\":{:.6}", b.saturating_sub(*a) as f64 / 1e6))
        .collect();
    println!(
        "{{\"wall_s\":{wall:.6},\"teardown_s\":{teardown:.6},\"inblock\":{},\"orphans_killed\":{orphans_killed},\"out_hash\":\"{out_hash}\",\"out_lines\":{out_lines},\"out_bytes\":{out_bytes},\"cpus\":\"{chosen}\",\"user_s\":{:.6},\"sys_s\":{:.6},\"rss_peak_bytes\":{peak},\"rss_maxchild_bytes\":{maxchild},\"minflt\":{},\"majflt\":{},\"vcsw\":{},\"ivcsw\":{},\"threads\":{},\"oncpu_s\":{:.6},\"runq_s\":{:.6}{}{},\"exit\":{code},\"signal\":{signal}}}",
        ru.rest[6],
        secs(&ru.utime),
        secs(&ru.stime),
        ru.rest[3],
        ru.rest[4],
        ru.rest[11],
        ru.rest[12],
        sched.len(),
        on_cpu as f64 / 1e9,
        runq as f64 / 1e9,
        psi.concat(),
        foreign
    );
}

fn rusage(who: c_int) -> Rusage {
    let mut ru = Rusage::default();
    // SAFETY: fills the struct we own.
    unsafe { getrusage(who, &mut ru) };
    ru
}

/// Waits for every remaining child (orphans adopted as their subreaper)
/// until none is left; the seconds it took, and how many were still
/// running after `limit` and killed (with whatever they started), so the
/// next run starts on a quiet machine and the output pipe closes.
fn reap_orphans(limit: Duration) -> (f64, u32) {
    let t = Instant::now();
    let mut killed: HashSet<i32> = HashSet::new();
    loop {
        let mut st = 0;
        // SAFETY: waits for any child of ours.
        let r = unsafe { waitpid(-1, &mut st, WNOHANG) };
        if r < 0 {
            break; // no children left
        }
        if r == 0 {
            if t.elapsed() > limit {
                if killed.is_empty() {
                    eprintln!("helper: the command left processes running after {} s; killed", limit.as_secs());
                }
                killed.extend(kill_children());
                // a child killed now is reaped on a later pass; one the
                // kill cannot end yet (uninterruptible sleep) is waited
                // for another minute at most
                if t.elapsed() > limit + Duration::from_secs(60) {
                    eprintln!("helper: processes survived SIGKILL for 60 s; giving up");
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    (t.elapsed().as_secs_f64(), killed.len() as u32)
}

/// SIGKILL to every process whose parent is this one; their pids. Their
/// own children then come to this process as subreaper, for the next pass.
#[cfg(target_os = "linux")]
fn kill_children() -> Vec<i32> {
    let me = process::id().to_string();
    let Ok(rd) = fs::read_dir("/proc") else { return vec![] };
    let mut n = vec![];
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let Ok(s) = fs::read_to_string(e.path().join("stat")) else { continue };
        let f: Vec<&str> = s.rsplit(')').next().unwrap_or("").split_whitespace().collect();
        // a zombie is dead already, only not yet reaped
        if f.first() != Some(&"Z") && f.get(1) == Some(&me.as_str()) {
            // SAFETY: a signal to a child of ours.
            unsafe { linux::kill(pid, linux::SIGKILL) };
            n.push(pid);
        }
    }
    n
}

/// Without a subreaper nothing is adopted, so there is nothing to kill.
#[cfg(not(target_os = "linux"))]
fn kill_children() -> Vec<i32> {
    vec![]
}

/// An order-free hash of the lines read from `r`: the wrapping sum of a
/// mixed FNV-1a per line, with the line and byte counts.
fn hash_lines(mut r: impl io::Read) -> (u64, u64, u64) {
    let (mut sum, mut lines, mut bytes) = (0u64, 0u64, 0u64);
    let mut h = 0xcbf29ce484222325u64;
    let mut buf = vec![0u8; 1 << 16];
    let mut partial = false;
    let mix = |mut z: u64| {
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    };
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        bytes += n as u64;
        for &b in &buf[..n] {
            if b == b'\n' {
                sum = sum.wrapping_add(mix(h));
                lines += 1;
                h = 0xcbf29ce484222325;
                partial = false;
            } else {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
                partial = true;
            }
        }
    }
    if partial {
        sum = sum.wrapping_add(mix(h));
        lines += 1;
    }
    (sum, lines, bytes)
}

/// Busy ticks of the whole machine (user, nice, system, and steal, which
/// another guest took) and the CPU count, from /proc/stat. Interrupt time
/// belongs to no process, and partly to this run's I/O, so it is left
/// out. None off Linux.
fn machine_busy_ticks() -> Option<(u64, usize)> {
    let s = fs::read_to_string("/proc/stat").ok()?;
    let mut lines = s.lines();
    let f: Vec<u64> = lines.next()?.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    let busy = [0, 1, 2, 7].iter().filter_map(|i| f.get(*i)).sum();
    let ncpu = lines.filter(|l| l.starts_with("cpu")).count();
    Some((busy, ncpu))
}

/// One CPU per physical core first (SMT siblings may be numbered next to
/// each other), from the CPUs this process may use, so a container's
/// cpuset that leaves out CPU 0 still works; CPU 0's core last, as it
/// takes most interrupts and housekeeping; siblings only when there are
/// fewer cores than `n`. Exits 2 when fewer than `n` CPUs are allowed.
#[cfg(target_os = "linux")]
fn pick_cpus(n: usize) -> ([u64; 16], String) {
    let mut allowed = [0u64; 16];
    // SAFETY: fills the mask we own.
    unsafe { linux::sched_getaffinity(0, size_of_val(&allowed), allowed.as_mut_ptr()) };
    let cpus: Vec<usize> = (0..1024).filter(|c| allowed[c / 64] & (1 << (c % 64)) != 0).collect();
    if cpus.len() < n {
        eprintln!("helper: --cpus {n}, but only {} CPUs are allowed", cpus.len());
        process::exit(2);
    }
    let core = |c: usize| fs::read_to_string(format!("/sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list")).unwrap_or_else(|_| c.to_string());
    let core0 = core(0);
    let mut seen = HashSet::new();
    let firsts: Vec<usize> = cpus.iter().copied().filter(|c| seen.insert(core(*c))).collect();
    let (rest, zero): (Vec<usize>, Vec<usize>) = firsts.into_iter().partition(|c| core(*c) != core0);
    let mut pick: Vec<usize> = rest.into_iter().chain(zero).take(n).collect();
    for c in &cpus {
        if pick.len() >= n {
            break;
        }
        if !pick.contains(c) {
            pick.push(*c);
        }
    }
    let mut m = [0u64; 16];
    for c in &pick {
        m[c / 64] |= 1 << (c % 64);
    }
    (m, pick.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(","))
}

#[cfg(not(target_os = "linux"))]
fn pick_cpus(_n: usize) -> ([u64; 16], String) {
    unsupported("--cpus")
}

/// CPU ticks (1/100 s) of every process so far, with its name and whether
/// it is a kernel thread (a child of kthreadd), from `/proc/PID/stat`.
/// Empty off Linux.
fn proc_cpu() -> HashMap<u32, (String, u64, bool)> {
    let mut m = HashMap::new();
    let Ok(rd) = fs::read_dir("/proc") else { return m };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(s) = fs::read_to_string(e.path().join("stat")) else { continue };
        // comm is in parentheses and may hold spaces
        let (Some(a), Some(b)) = (s.find('('), s.rfind(')')) else { continue };
        let comm = s[a + 1..b].to_string();
        let f: Vec<&str> = s[b + 1..].split_whitespace().collect();
        let (Some(ppid), Some(ut), Some(st)) = (f.get(1), f.get(11), f.get(12)) else { continue };
        let ticks = ut.parse::<u64>().unwrap_or(0) + st.parse::<u64>().unwrap_or(0);
        m.insert(pid, (comm, ticks, *ppid == "2" || pid == 2));
    }
    m
}

/// The CPU seconds processes outside `tree` used between two snapshots, as
/// JSON fields: user processes, kernel threads, and the heaviest five.
fn foreign_cpu(a: &HashMap<u32, (String, u64, bool)>, b: &HashMap<u32, (String, u64, bool)>, tree: &HashSet<u32>) -> String {
    if b.is_empty() {
        return String::new();
    }
    let mut user = 0u64;
    let mut kernel = 0u64;
    let mut by: HashMap<String, u64> = HashMap::new();
    for (pid, (comm, t1, kt)) in b {
        if tree.contains(pid) {
            continue;
        }
        let d = t1.saturating_sub(a.get(pid).map_or(0, |x| x.1));
        if d == 0 {
            continue;
        }
        if *kt { kernel += d } else { user += d }
        // kernel worker names carry their queue after a slash or dash
        let name = if *kt { comm.split(['/', '-']).next().unwrap_or(comm).to_string() } else { comm.clone() };
        *by.entry(name).or_default() += d;
    }
    let mut top: Vec<_> = by.into_iter().collect();
    top.sort_by(|x, y| y.1.cmp(&x.1));
    let top: Vec<String> = top.iter().take(5).map(|(n, t)| format!("{}:{:.2}", n.replace(['"', '\\', ','], "_"), *t as f64 / 100.0)).collect();
    format!(
        ",\"foreign_user_cpu_s\":{:.2},\"foreign_kernel_cpu_s\":{:.2},\"foreign_top\":\"{}\"",
        user as f64 / 100.0,
        kernel as f64 / 100.0,
        top.join(",")
    )
}

/// The CPU seconds of processes outside `tree` seen in the second snapshot.
fn foreign_seen_s(a: &HashMap<u32, (String, u64, bool)>, b: &HashMap<u32, (String, u64, bool)>, tree: &HashSet<u32>) -> f64 {
    b.iter()
        .filter(|(pid, _)| !tree.contains(pid))
        .map(|(pid, (_, t1, _))| t1.saturating_sub(a.get(pid).map_or(0, |x| x.1)))
        .sum::<u64>() as f64
        / 100.0
}

/// Machine-wide pressure stall totals in microseconds (`/proc/pressure`,
/// Linux 4.20): time some task, or all (`full`), waited for CPU, memory or
/// I/O. Empty elsewhere.
fn psi() -> Vec<(String, u64)> {
    let mut v = Vec::new();
    for res in ["cpu", "memory", "io"] {
        let Ok(s) = fs::read_to_string(format!("/proc/pressure/{res}")) else { continue };
        for line in s.lines() {
            let kind = line.split_whitespace().next().unwrap_or("");
            if let Some(t) = line.rsplit("total=").next().and_then(|t| t.trim().parse().ok()) {
                v.push((format!("{res}_{kind}"), t));
            }
        }
    }
    v
}

/// On-CPU and run-queue time of every thread in the tree, from
/// `/proc/PID/task/TID/schedstat` (Linux): the thread state analysis of a
/// run, without root.
#[cfg(target_os = "linux")]
fn tree_sched(root: u32, seen: &mut HashMap<u32, (u64, u64)>) {
    let mut todo = vec![root];
    while let Some(p) = todo.pop() {
        let Ok(tasks) = fs::read_dir(format!("/proc/{p}/task")) else { continue };
        for t in tasks.flatten() {
            let path = t.path();
            if let (Some(tid), Ok(s)) = (
                path.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<u32>().ok()),
                fs::read_to_string(path.join("schedstat")),
            ) {
                let mut f = s.split_whitespace().filter_map(|x| x.parse::<u64>().ok());
                if let (Some(run), Some(wait)) = (f.next(), f.next()) {
                    seen.insert(tid, (run, wait));
                }
            }
            if let Ok(c) = fs::read_to_string(path.join("children")) {
                todo.extend(c.split_whitespace().filter_map(|x| x.parse::<u32>().ok()));
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn tree_sched(_root: u32, _seen: &mut HashMap<u32, (u64, u64)>) {}

fn page_size() -> u64 {
    // SAFETY: a plain query.
    let n = unsafe { sysconf(SC_PAGESIZE) };
    if n > 0 { n as u64 } else { 4096 }
}

/// Resident bytes of `root` and every descendant, and their pids.
#[cfg(target_os = "linux")]
fn tree_rss(root: u32) -> (u64, Vec<u32>) {
    let page = page_size();
    let mut todo = vec![root];
    let mut seen = Vec::new();
    let mut total = 0;
    while let Some(p) = todo.pop() {
        seen.push(p);
        if let Ok(s) = fs::read_to_string(format!("/proc/{p}/statm"))
            && let Some(r) = s.split_whitespace().nth(1).and_then(|r| r.parse::<u64>().ok())
        {
            total += r * page;
        }
        if let Ok(tasks) = fs::read_dir(format!("/proc/{p}/task")) {
            for t in tasks.flatten() {
                if let Ok(c) = fs::read_to_string(t.path().join("children")) {
                    todo.extend(c.split_whitespace().filter_map(|x| x.parse::<u32>().ok()));
                }
            }
        }
    }
    (total, seen)
}

/// Resident bytes of `root` and every descendant, from `ps`: darwin has no
/// /proc, and `ps` is the documented way to read another process's RSS.
#[cfg(not(target_os = "linux"))]
fn tree_rss(root: u32) -> (u64, Vec<u32>) {
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pid=,ppid=,rss="]).output() else {
        return (0, vec![root]);
    };
    let rows: Vec<(u32, u32, u64)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?.parse().ok()?))
        })
        .collect();
    let mut todo = vec![root];
    let mut seen = Vec::new();
    let mut total = 0;
    while let Some(p) = todo.pop() {
        seen.push(p);
        for (pid, ppid, rss) in &rows {
            if *pid == p {
                total += rss * 1024;
            }
            if *ppid == p {
                todo.push(*pid);
            }
        }
    }
    (total, seen)
}

// ---- fragment -----------------------------------------------------------

#[cfg(target_os = "linux")]
fn fragment(args: &[String]) {
    let mib: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
    // first, so whoever started it can always stop it
    if let Some(f) = args.get(1) {
        // a new file only: this runs as root in a directory the user can
        // write, and O_CREAT | O_EXCL never follows a symlink
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(f)
            .and_then(|mut h| h.write_all(process::id().to_string().as_bytes()))
            .unwrap_or_else(|e| {
                eprintln!("helper: {f}: {e}");
                process::exit(1)
            });
    }
    let page = page_size() as usize;
    let half = (mib << 20) / 2 / page * page;
    let map = || {
        // SAFETY: a fresh private anonymous mapping only this function uses.
        let p = unsafe { linux::mmap(ptr::null_mut(), half, linux::PROT_READ_WRITE, linux::MAP_PRIVATE_ANON, -1, 0) };
        if p as isize == -1 {
            eprintln!("helper: mmap {} MiB: {}", half >> 20, io::Error::last_os_error());
            process::exit(1);
        }
        // 4 KB pages, so a freed page leaves its 2 MB block broken
        unsafe { linux::madvise(p, half, linux::MADV_NOHUGEPAGE) };
        p as *mut u8
    };
    let (keep, free) = (map(), map());
    // touched in turn, the two halves take every other physical page
    for off in (0..half).step_by(page) {
        // SAFETY: both offsets are inside their mappings.
        unsafe {
            keep.add(off).write_volatile(1);
            free.add(off).write_volatile(1);
        }
    }
    // SAFETY: the mappings made above, whole: one lock, one unmap, so no
    // mapping is split into pieces.
    unsafe {
        if linux::mlock(keep as *const _, half) != 0 {
            eprintln!("helper: mlock {} MiB: {} (needs root or RLIMIT_MEMLOCK)", half >> 20, io::Error::last_os_error());
            process::exit(1);
        }
        linux::munmap(free as *mut _, half);
    }
    println!("ready {} MiB held", half >> 20);
    if let Err(e) = io::stdout().flush() {
        eprintln!("helper: stdout: {e}");
        process::exit(1);
    }
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(not(target_os = "linux"))]
fn fragment(_args: &[String]) {
    unsupported("fragment")
}

// ---- guard --------------------------------------------------------------

#[cfg(target_os = "linux")]
fn guard(args: &[String]) {
    let Some(pid) = args.first() else { usage() };
    // what to put back, read now, while root starts it: from the command
    // line, not from a file in the user's work directory, which anything
    // running as the user could rewrite before the guard acts on it
    let pairs = &args[1..];
    if pairs.len() % 2 != 0 {
        usage();
    }
    // the process's start time too, so a new process that gets its pid
    // does not keep the guard waiting; a zombie (exited, not yet reaped by
    // a slow parent) is gone
    let start = |p: &str| {
        let s = fs::read_to_string(format!("/proc/{p}/stat")).ok()?;
        let f: Vec<String> = s.rsplit(')').next()?.split_whitespace().map(String::from).collect();
        if matches!(f.first().map(String::as_str), Some("Z" | "X")) { None } else { f.get(19).cloned() }
    };
    let first = start(pid);
    while first.is_some() && start(pid) == first {
        thread::sleep(Duration::from_millis(500));
    }
    // the fragmenters: processes of this very binary (each run builds its
    // own, in its own work directory) started as `fragment`, found in
    // /proc rather than through a pid file the user could point anywhere
    let fragmenters = || -> Vec<i32> {
        let Ok(me) = fs::read_link("/proc/self/exe") else { return vec![] };
        let Ok(rd) = fs::read_dir("/proc") else { return vec![] };
        rd.flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
            .filter(|p| fs::read_link(format!("/proc/{p}/exe")).is_ok_and(|x| x == me))
            .filter(|p| fs::read(format!("/proc/{p}/cmdline")).is_ok_and(|c| c.split(|b| *b == 0).nth(1) == Some(b"fragment")))
            .collect()
    };
    for p in fragmenters() {
        // SAFETY: a signal to a process we just identified.
        unsafe { linux::kill(p, linux::SIGKILL) };
    }
    // gone before the settings return: it holds most of memory locked
    let t = Instant::now();
    while !fragmenters().is_empty() && t.elapsed() < Duration::from_secs(60) {
        thread::sleep(Duration::from_millis(100));
    }
    for kv in pairs.chunks(2) {
        let _ = fs::write(&kv[0], &kv[1]);
    }
}

#[cfg(not(target_os = "linux"))]
fn guard(_args: &[String]) {
    unsupported("guard")
}

// ---- evict --------------------------------------------------------------

#[cfg(target_os = "linux")]
fn evict(args: &[String]) {
    fn walk(p: &Path, t: &mut (u64, u64)) {
        let Ok(m) = fs::symlink_metadata(p) else { return };
        if m.is_dir() {
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    walk(&e.path(), t);
                }
            }
        } else if m.is_file()
            && let Ok(f) = fs::File::open(p)
        {
            // SAFETY: advice on a descriptor we hold.
            unsafe { linux::posix_fadvise(f.as_raw_fd(), 0, 0, linux::POSIX_FADV_DONTNEED) };
            t.0 += 1;
            t.1 += m.len();
        }
    }
    if args.is_empty() {
        usage();
    }
    let mut t = (0, 0);
    for a in args {
        // the root itself may be a symlink (a result link to the store)
        let root = fs::canonicalize(a).unwrap_or_else(|_| a.into());
        walk(&root, &mut t);
    }
    println!("{{\"files\":{},\"bytes\":{}}}", t.0, t.1);
}

#[cfg(not(target_os = "linux"))]
fn evict(_args: &[String]) {
    unsupported("evict")
}
