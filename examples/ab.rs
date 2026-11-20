//! A/B of two iets binaries over the work CI gives iets.
//!
//! `cargo run --release --example ab -- BEFORE AFTER [--full] [--only SUBSTR]
//! [--rounds N] [--max-rounds N] [--threshold PCT] [--json FILE]`
//!
//! Each scenario runs at least `--rounds` rounds (6), and more, up to
//! `--max-rounds` (20), until each interval is narrower than the threshold
//! (2 % without one).
//!
//! `NIXPKGS` names the nixpkgs tree, by default the flake's locked input.
//! Exits 1 when a run fails, the outputs differ, or with `--threshold` when
//! a slowdown or more memory has its whole interval past it.

use std::process::ExitCode;

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!("ab: unix only");
    ExitCode::from(2)
}

#[cfg(unix)]
fn main() -> ExitCode {
    ab::main()
}

#[cfg(unix)]
mod ab {
    use std::env;
    use std::fs::{self, File};
    use std::io::{self, BufRead, BufReader, Read};
    use std::mem::MaybeUninit;
    use std::path::{Path, PathBuf};
    use std::process::{self, Command, ExitCode, Stdio};
    use std::ptr;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::json;
    use sha2::{Digest, Sha256};

    const EDIT_FILE: &str = "nixos/modules/misc/version.nix";

    #[derive(Clone, Copy, PartialEq)]
    enum Cache {
        Cold,
        Warm,
        /// Warm, with a file every NixOS evaluation reads changed each round.
        Edit,
    }

    struct Scenario {
        id: &'static str,
        full: bool,
        cache: Cache,
        checkout: bool,
        args: Vec<String>,
    }

    struct Ctx {
        np: String,
        checkout: PathBuf,
        work: PathBuf,
    }

    fn scenarios(c: &Ctx) -> Vec<Scenario> {
        let table = [
            (
                "nixos/server-w1",
                false,
                Cache::Cold,
                "eval --impure --strict --no-cache --workers 1 -I nixpkgs={np} {wl}/nixos-server.nix",
            ),
            (
                "nixos/server-w4",
                false,
                Cache::Cold,
                "eval --impure --strict --no-cache --workers 4 -I nixpkgs={np} {wl}/nixos-server.nix",
            ),
            (
                "nixos/server-pure-w1",
                false,
                Cache::Cold,
                "eval --strict --no-cache --workers 1 --lint absolute-path-in-drv=off {ck}/bench.nix",
            ),
            (
                "nixos/minimal-w1",
                false,
                Cache::Cold,
                "eval --impure --strict --no-cache --workers 1 -I nixpkgs={np} {wl}/nixos-minimal.nix",
            ),
            (
                "nixos/server-warm-ir",
                false,
                Cache::Warm,
                "eval --impure --strict --no-tracing-cache --workers 1 -I nixpkgs={np} {wl}/nixos-server.nix",
            ),
            (
                "rerun/server-hit",
                false,
                Cache::Warm,
                "eval --impure --strict --workers 1 -I nixpkgs={np} {wl}/nixos-server.nix",
            ),
            (
                "rerun/server-edit",
                false,
                Cache::Edit,
                "eval --impure --strict --workers 1 -I nixpkgs={ck} {wl}/nixos-server.nix",
            ),
            (
                "jobset/release-small",
                true,
                Cache::Cold,
                "eval-jobs --impure --no-instantiate --no-cache --force-recurse --workers 4 -I nixpkgs={np} --arg nixpkgs {rs} {np}/nixos/release-small.nix",
            ),
            (
                "nixpkgs/outpaths-chunk-w1",
                true,
                Cache::Cold,
                "eval-jobs --impure --no-instantiate --no-cache --workers 1 --chunks 8 --chunk 0 -I nixpkgs={np} {work}/outpaths.nix",
            ),
            (
                "nixpkgs/outpaths-w8",
                true,
                Cache::Cold,
                "eval-jobs --impure --no-instantiate --no-cache --workers 8 -I nixpkgs={np} {work}/outpaths.nix",
            ),
        ];
        let np = &c.np;
        let fill = |a: &str| {
            a.replace("{np}", np)
                .replace("{ck}", &c.checkout.display().to_string())
                .replace(
                    "{wl}",
                    concat!(env!("CARGO_MANIFEST_DIR"), "/bench/workloads"),
                )
                .replace("{work}", &c.work.display().to_string())
                .replace(
                    "{rs}",
                    &format!(r#"{{ outPath = "{np}"; revCount = 1; shortRev = "abcdefg"; }}"#),
                )
        };
        table
            .into_iter()
            .map(|(id, full, cache, args)| Scenario {
                id,
                full,
                cache,
                checkout: args.contains("{ck}"),
                args: args.split(' ').map(fill).collect(),
            })
            .collect()
    }

    struct Run {
        wall: f64,
        user: f64,
        sys: f64,
        /// Peak RSS of the largest process, in bytes.
        rss: f64,
        blocks_read: f64,
        blocks_written: f64,
        counts: Option<Vec<f64>>,
        out: [u8; 32],
    }

    fn secs(t: libc::timeval) -> f64 {
        t.tv_sec as f64 + t.tv_usec as f64 / 1e6
    }

    /// User and system seconds, and blocks read and written, of every
    /// child reaped so far.
    fn children() -> [f64; 4] {
        let mut ru = MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: getrusage fills the struct it is given.
        let ru = unsafe {
            libc::getrusage(libc::RUSAGE_CHILDREN, ru.as_mut_ptr());
            ru.assume_init()
        };
        [
            secs(ru.ru_utime),
            secs(ru.ru_stime),
            ru.ru_inblock as f64,
            ru.ru_oublock as f64,
        ]
    }

    /// The numbers in an eval-jobs `"stats":{...}` are time and allocation,
    /// which a change may move on purpose.
    fn mask_stats(line: &[u8]) -> Vec<u8> {
        let Some(at) = memchr::memmem::find(line, b"\"stats\":{") else {
            return line.to_vec();
        };
        let mut out = line[..at].to_vec();
        let (mut i, mut depth, mut num) = (at, 0, false);
        while i < line.len() {
            let b = line[i];
            i += 1;
            if num && (b.is_ascii_digit() || b == b'.') {
                continue;
            }
            num = false;
            out.push(b);
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                b':' if depth > 0 => {
                    out.push(b'0');
                    num = true;
                }
                _ => {}
            }
        }
        out.extend_from_slice(&line[i..]);
        out
    }

    /// Independent of line order, which eval-jobs's workers do not keep.
    fn hash_output(r: impl Read, eof: mpsc::Sender<io::Result<()>>) -> Option<[u8; 32]> {
        let mut lines: Vec<[u8; 32]> = Vec::new();
        for line in BufReader::new(r).split(b'\n') {
            match line {
                Ok(line) => lines.push(Sha256::digest(mask_stats(&line)).into()),
                Err(e) => {
                    let _ = eof.send(Err(e));
                    return None;
                }
            }
        }
        let _ = eof.send(Ok(()));
        lines.sort_unstable();
        let mut h = Sha256::new();
        for l in &lines {
            h.update(l);
        }
        Some(h.finalize().into())
    }

    fn wait_for_stdout(
        eof: &mpsc::Receiver<io::Result<()>>,
        deadline: Instant,
    ) -> Result<(), RunError> {
        let mut final_poll = false;
        loop {
            // recv_timeout takes a relative duration. Recheck the absolute deadline
            // so a paused receiver cannot start a second full wait when it resumes.
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(REAP_INTERVAL);
            match eof.recv_timeout(wait) {
                Ok(result) => {
                    return result
                        .map_err(|e| RunError::Scenario(format!("read benchmark stdout: {e}")));
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => continue,
                // EOF can queue while a timed-out receiver is paused. Check it
                // once with a zero timeout before reporting the absolute limit.
                Err(mpsc::RecvTimeoutError::Timeout) if !final_poll => final_poll = true,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(RunError::Cleanup(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "benchmark stdout did not close after the run-on limit",
                    )));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RunError::Cleanup(io::Error::other(
                        "benchmark stdout reader disconnected",
                    )));
                }
            }
        }
    }

    /// How long processes a run left behind may go on before they are killed.
    const LEFTOVER_LIMIT: Duration = Duration::from_secs(60);

    const CLEANUP_GRACE: Duration = Duration::from_secs(2);
    const REAP_INTERVAL: Duration = Duration::from_millis(10);

    const COUNTERS: [&str; 5] = [
        "instructions",
        "cycles",
        "cache_references",
        "cache_misses",
        "branch_misses",
    ];

    /// [`COUNTERS`] in user space, of the process and everything it starts.
    #[cfg(target_os = "linux")]
    mod counters {
        use std::fs::File;
        use std::io::Read;
        use std::os::fd::FromRawFd;

        const CONFIG: [u64; 5] = [1, 0, 2, 3, 5];

        /// Counters of the calling thread that start at an exec: a child it
        /// spawns inherits them, and the thread itself never counts. None where
        /// the kernel refuses, as with perf_event_paranoid > 2.
        pub fn open() -> Option<Vec<File>> {
            CONFIG
                .iter()
                .map(|&config| {
                    // perf_event_attr, PERF_ATTR_SIZE_VER7
                    let mut attr = [0u64; 16];
                    attr[0] = 128 << 32; // type PERF_TYPE_HARDWARE, size
                    attr[1] = config;
                    // disabled, inherit, exclude_kernel, exclude_hv, enable_on_exec
                    attr[5] = 1 | 1 << 1 | 1 << 5 | 1 << 6 | 1 << 12;
                    // SAFETY: attr is a perf_event_attr of the size it names.
                    let fd = unsafe {
                        libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 8)
                    };
                    // SAFETY: a new descriptor that nothing else owns.
                    (fd >= 0).then(|| unsafe { File::from_raw_fd(fd as i32) })
                })
                .collect()
        }

        pub fn read(files: &mut [File]) -> Option<Vec<f64>> {
            files
                .iter_mut()
                .map(|f| {
                    let mut b = [0u8; 8];
                    f.read_exact(&mut b).ok()?;
                    Some(u64::from_ne_bytes(b) as f64)
                })
                .collect()
        }
    }

    #[cfg(target_os = "linux")]
    fn enable_subreaper() -> io::Result<()> {
        // SAFETY: PR_SET_CHILD_SUBREAPER takes an integer flag.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn kill_leftovers(deadline: Instant) -> io::Result<()> {
        kill_leftovers_in(Path::new("/proc"), deadline)
    }

    #[cfg(target_os = "linux")]
    fn kill_leftovers_in(procfs: &Path, deadline: Instant) -> io::Result<()> {
        let overdue = Instant::now() >= deadline;
        let own_pid = process::id().to_string();
        let status = fs::read(procfs.join("self/status"))?;
        // An ancestor namespace can assign us the same numeric PID. NStgid
        // lists one PID per namespace from the procfs mount down to our own.
        let same_namespace = match status
            .split(|&b| b == b'\n')
            .find_map(|s| s.strip_prefix(b"NStgid:"))
        {
            Some(ids) => ids.trim_ascii() == own_pid.as_bytes(),
            // Before Linux 4.1, compare our namespace to that of PID 1
            // in this mount. Without PID namespaces only the self link exists.
            None => match fs::read_link(procfs.join("self/ns/pid")) {
                Ok(ours) => ours == fs::read_link(procfs.join("1/ns/pid"))?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::read_link(procfs.join("self"))? == Path::new(&own_pid)
                }
                Err(e) => return Err(e),
            },
        };
        if !same_namespace {
            return Err(io::Error::other("/proc uses a different PID namespace"));
        }
        // Some kernels omit the per-thread children files, while hidepid can
        // hide a child from the top-level listing. Check both sources.
        let mut result = Ok(());
        let signal = |pid| {
            // Leave exits for the outer reaper, which tracks newly adopted children.
            if pid > 0 && children_remain_for(libc::P_PID, pid as libc::id_t)? {
                // SAFETY: this is our child; the sole reaper cannot recycle its PID.
                if unsafe { libc::kill(pid, libc::SIGKILL) } == -1 {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() != Some(libc::ESRCH) {
                        return Err(e);
                    }
                }
                return Ok(matches!(children_state(libc::P_ALL, 0), Ok(Some(true))));
            }
            Ok(false)
        };
        let signal_direct = |result: &mut io::Result<()>, waitable: &mut bool| {
            let mut complete = true;
            let mut found = false;
            // Allow a small direct-list pass if the first sweep starts overdue.
            // Count task entries too, since many threads may have no children.
            let mut remaining = 32usize;
            let mut allow_direct = || {
                if overdue {
                    if remaining == 0 {
                        return false;
                    }
                    remaining -= 1;
                    true
                } else {
                    Instant::now() < deadline
                }
            };
            if let Ok(threads) = fs::read_dir(procfs.join("self/task")) {
                let mut field = Vec::new();
                for task in threads {
                    if !allow_direct() {
                        return false;
                    }
                    let Ok(file) = task.and_then(|task| File::open(task.path().join("children")))
                    else {
                        complete = false;
                        continue;
                    };
                    found = true;
                    // procfs emits space-separated decimal PIDs, including a trailing space.
                    let mut children = BufReader::new(file);
                    loop {
                        if !allow_direct() {
                            return false;
                        }
                        field.clear();
                        match children.read_until(b' ', &mut field) {
                            Ok(0) => break,
                            Err(_) => {
                                complete = false;
                                break;
                            }
                            Ok(_) => {}
                        }
                        if let Some(pid) = str::from_utf8(field.trim_ascii())
                            .ok()
                            .and_then(|s| s.parse().ok())
                        {
                            match signal(pid) {
                                Ok(ready) => *waitable |= ready,
                                Err(e) => *result = Err(e),
                            }
                        }
                    }
                }
            }
            complete && found
        };
        let mut waitable = false;
        let mut direct = signal_direct(&mut result, &mut waitable);
        if Instant::now() >= deadline {
            return result;
        }
        // A parent we signaled may exit during a long scan of unrelated PIDs.
        // Revisit the direct lists during a slow process-table pass.
        let mut revisit = Instant::now() + REAP_INTERVAL;
        let mut restart = None;
        for pass in 0..2 {
            for entry in fs::read_dir(procfs)? {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                if !direct && waitable && pass == 0 && restart.is_none() {
                    restart = Some(now + deadline.saturating_duration_since(now) / 2);
                }
                if restart.is_some_and(|at| now >= at) {
                    break;
                }
                if now >= revisit {
                    direct &= signal_direct(&mut result, &mut waitable);
                    let state = children_state(libc::P_ALL, 0);
                    waitable |= matches!(&state, Ok(Some(true)));
                    if Instant::now() >= deadline || matches!(state, Ok(None)) {
                        break;
                    }
                    revisit = Instant::now() + REAP_INTERVAL;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        result = Err(e);
                        continue;
                    }
                };
                if let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) {
                    match signal(pid) {
                        Ok(ready) => waitable |= ready,
                        Err(e) => result = Err(e),
                    }
                }
            }
            if direct || !waitable || Instant::now() >= deadline {
                break;
            }
            // A slow table scan can pass a grandchild before its parent exits.
            // Reserve part of the same grace for one retry only after
            // an exit is waitable; a still-live child does not restart the scan.
            restart = None;
        }
        result
    }

    #[cfg(not(target_os = "linux"))]
    fn kill_leftovers(_: Instant) -> io::Result<()> {
        Ok(())
    }

    /// Check ownership without reaping another exit or extending grace.
    fn children_remain() -> io::Result<bool> {
        children_remain_for(libc::P_ALL, 0)
    }

    fn children_remain_for(kind: libc::idtype_t, id: libc::id_t) -> io::Result<bool> {
        children_state(kind, id).map(|state| state.is_some())
    }

    // None means no children; Some reports whether an exit is ready to reap.
    fn children_state(kind: libc::idtype_t, id: libc::id_t) -> io::Result<Option<bool>> {
        let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: info is writable and remains initialized when no exit is ready.
        if unsafe {
            libc::waitid(
                kind,
                id,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } == -1
        {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ECHILD) {
                return Ok(None);
            }
            return Err(e);
        }
        // SAFETY: waitid initializes the exit information on success; the zeroed
        // value supplies si_signo = 0 when WNOHANG finds no waitable child.
        Ok(Some(unsafe { info.assume_init() }.si_signo != 0))
    }

    /// Processes the run left behind, which are ours as the subreaper. False
    /// when cleanup required a sweep after the run-on limit.
    fn reap_leftovers(
        until: Instant,
        deadline: Instant,
        sweep: impl FnMut(Instant) -> io::Result<()>,
    ) -> io::Result<bool> {
        reap_with_final_polls(until, deadline, 256, sweep)
    }

    fn reap_with_final_polls(
        until: Instant,
        deadline: Instant,
        mut final_polls: usize,
        mut sweep: impl FnMut(Instant) -> io::Result<()>,
    ) -> io::Result<bool> {
        let mut clean = true;
        let mut sweep_error = None;
        let mut reaped_since_sweep = false;
        let mut final_sweep_done = false;
        // Drain exits already pending at the deadline, even if several arrived
        // between polls. Bound the work if descendants keep producing exits.
        loop {
            // A timeout decision needs an observation made after the deadline.
            let now = Instant::now();
            // SAFETY: waitpid with a null status pointer is allowed.
            let pid = unsafe { libc::waitpid(-1, ptr::null_mut(), libc::WNOHANG) };
            if pid == -1 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::ECHILD) => return Ok(clean),
                    Some(libc::EINTR) => {}
                    _ => return Err(e),
                }
            }
            reaped_since_sweep |= pid > 0;
            if now >= until {
                // Always attempt the first sweep, even if the deadline has passed.
                let first_sweep = pid == 0 && clean;
                if first_sweep {
                    clean = false;
                }
                if !first_sweep && now >= deadline {
                    if pid != 0 && final_polls > 0 {
                        final_polls -= 1;
                        continue;
                    }
                    if pid != 0 {
                        // An exit or EINTR leaves the remaining count unknown.
                        if !children_remain()? {
                            return Ok(clean);
                        }
                    }
                    // Reaping a parent can expose a child after the previous
                    // sweep. Give it the same bounded direct-list attempt.
                    if !final_sweep_done && (clean || reaped_since_sweep) {
                        if let Err(e) = sweep(deadline) {
                            sweep_error = Some(e);
                        }
                        clean = false;
                        final_sweep_done = true;
                        if !children_remain()? {
                            return Ok(false);
                        }
                        if final_polls > 0 {
                            final_polls -= 1;
                            continue;
                        }
                    }
                    return Err(sweep_error.unwrap_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            "children remain after the cleanup deadline",
                        )
                    }));
                }
                if pid == 0 {
                    // A killed process's children come to us in turn.
                    reaped_since_sweep = false;
                    if let Err(e) = sweep(deadline) {
                        sweep_error = Some(e);
                    }
                }
            }
            if pid == 0 {
                let next = if clean { until } else { deadline };
                thread::sleep(REAP_INTERVAL.min(next.saturating_duration_since(Instant::now())));
            }
        }
    }

    #[derive(Debug)]
    enum RunError {
        Scenario(String),
        Cleanup(io::Error),
    }

    impl From<String> for RunError {
        fn from(message: String) -> Self {
            Self::Scenario(message)
        }
    }

    fn run_once(bin: &Path, args: &[String], cache: &Path, stderr: &Path) -> Result<Run, RunError> {
        // a thread of its own, so the counters it opens cover only the run
        thread::scope(|s| {
            s.spawn(|| run(bin, args, cache, stderr))
                .join()
                .expect("the run thread")
        })
        .map_err(|error| match error {
            RunError::Scenario(message) => {
                RunError::Scenario(format!("{}: {message}", bin.display()))
            }
            RunError::Cleanup(error) => RunError::Cleanup(io::Error::new(
                error.kind(),
                format!("{}: {error}", bin.display()),
            )),
        })
    }

    fn run(bin: &Path, args: &[String], cache: &Path, stderr: &Path) -> Result<Run, RunError> {
        let mut cmd = Command::new(bin);
        cmd.arg(&args[0])
            .arg("--cache-dir")
            .arg(cache)
            .args(&args[1..]);
        for (k, _) in env::vars_os() {
            if k.to_string_lossy().starts_with("IETS_") {
                cmd.env_remove(k);
            }
        }
        cmd.env("IETS_PROGRESS", "false")
            .env("XDG_CACHE_HOME", cache)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(File::create(stderr).map_err(|e| e.to_string())?);
        let before = children();
        #[cfg(target_os = "linux")]
        let mut counters = counters::open();
        let t0 = Instant::now();
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let pid = child.id() as libc::pid_t;
        let out = child.stdout.take().expect("piped");
        let (notify_eof, eof) = mpsc::channel();
        let hasher = thread::spawn(move || hash_output(out, notify_eof));
        let mut status = 0;
        let mut ru = MaybeUninit::<libc::rusage>::uninit();
        let ru = loop {
            // SAFETY: pid is our child; a successful wait4 initializes status and ru.
            if unsafe { libc::wait4(pid, &mut status, 0, ru.as_mut_ptr()) } == pid {
                // SAFETY: wait4 filled ru for this child.
                break unsafe { ru.assume_init() };
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                // The measurement has failed, but adopted descendants can
                // still be running even when wait4 reports ECHILD.
                let now = Instant::now();
                if let Err(cleanup) = reap_leftovers(now, now + CLEANUP_GRACE, kill_leftovers) {
                    return Err(RunError::Cleanup(io::Error::new(
                        e.kind(),
                        format!("wait4: {e}; leftover cleanup: {cleanup}"),
                    )));
                }
                return Err(RunError::Cleanup(io::Error::new(
                    e.kind(),
                    format!("wait4: {e}"),
                )));
            }
        };
        let wall = t0.elapsed().as_secs_f64();
        let run_on_deadline = Instant::now() + LEFTOVER_LIMIT;
        let cleanup_deadline = run_on_deadline + CLEANUP_GRACE;
        // A surviving writer may keep the pipe open. On incomplete cleanup the
        // whole invocation exits; joining its stdout reader here could hang.
        let clean = reap_leftovers(run_on_deadline, cleanup_deadline, kill_leftovers)
            .map_err(RunError::Cleanup)?;
        // A process outside our child tree can still hold a pipe writer.
        // Bound the wait for a writer. A queued EOF is usable immediately,
        // and sorting the completed output may take longer.
        wait_for_stdout(&eof, cleanup_deadline)?;
        let out = hasher
            .join()
            .expect("the hash thread")
            .expect("stdout reached EOF");
        let used: Vec<f64> = children().iter().zip(before).map(|(a, b)| a - b).collect();
        #[cfg(target_os = "linux")]
        let counts = counters.as_deref_mut().and_then(counters::read);
        #[cfg(not(target_os = "linux"))]
        let counts = None;
        if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
            let err = fs::read_to_string(stderr).unwrap_or_default();
            let tail: Vec<&str> = err.lines().rev().take(5).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            return Err(format!("exit status {status}:\n{}", tail.join("\n")).into());
        }
        if !clean {
            return Err(format!(
                "child cleanup was required {}s after it exited",
                LEFTOVER_LIMIT.as_secs()
            )
            .into());
        }
        let kb = if cfg!(target_os = "macos") {
            1.0
        } else {
            1024.0
        };
        Ok(Run {
            wall,
            user: used[0],
            sys: used[1],
            blocks_read: used[2],
            blocks_written: used[3],
            rss: ru.ru_maxrss as f64 * kb,
            counts,
            out,
        })
    }

    fn median(xs: &[f64]) -> f64 {
        let mut s = xs.to_vec();
        s.sort_by(f64::total_cmp);
        let n = s.len();
        if n % 2 == 1 {
            s[n / 2]
        } else {
            (s[n / 2 - 1] + s[n / 2]) / 2.0
        }
    }

    /// The median and a distribution-free interval for it: the k-th
    /// smallest and largest value, with the largest k that covers 95 %.
    struct Interval {
        median: f64,
        lo: f64,
        hi: f64,
        coverage: f64,
    }

    fn interval(xs: &[f64]) -> Interval {
        let mut s = xs.to_vec();
        s.sort_by(f64::total_cmp);
        let n = s.len();
        let below = |k: usize| -> f64 {
            let mut c = 1.0;
            let mut sum = 0.0;
            for i in 0..k {
                sum += c;
                c = c * (n - i) as f64 / (i + 1) as f64;
            }
            sum / 2f64.powi(n as i32)
        };
        let k = (1..=n / 2)
            .rev()
            .find(|&k| 1.0 - 2.0 * below(k) >= 0.95)
            .unwrap_or(1)
            .min(n);
        Interval {
            median: median(&s),
            lo: s[k - 1],
            hi: s[n - k],
            coverage: if n < 2 { 0.0 } else { 1.0 - 2.0 * below(k) },
        }
    }

    struct Opts {
        before: PathBuf,
        after: PathBuf,
        full: bool,
        only: Option<String>,
        rounds: usize,
        max_rounds: usize,
        threshold: Option<f64>,
        json: Option<PathBuf>,
    }

    fn usage() -> ! {
        eprintln!(
            "usage: cargo run --release --example ab -- BEFORE AFTER [--full] [--only SUBSTR] \
             [--rounds N] [--max-rounds N] [--threshold PCT] [--json FILE]"
        );
        process::exit(2);
    }

    fn parse() -> Opts {
        let mut args = env::args().skip(1);
        let mut bins = Vec::new();
        let mut o = Opts {
            before: PathBuf::new(),
            after: PathBuf::new(),
            full: false,
            only: None,
            rounds: 6,
            max_rounds: 20,
            threshold: None,
            json: None,
        };
        while let Some(a) = args.next() {
            let mut val = || args.next().unwrap_or_else(|| usage());
            match a.as_str() {
                "--full" => o.full = true,
                "--only" => o.only = Some(val()),
                "--rounds" => o.rounds = val().parse().unwrap_or_else(|_| usage()),
                "--max-rounds" => o.max_rounds = val().parse().unwrap_or_else(|_| usage()),
                "--threshold" => o.threshold = Some(val().parse().unwrap_or_else(|_| usage())),
                "--json" => o.json = Some(val().into()),
                s if s.starts_with('-') => usage(),
                _ => bins.push(PathBuf::from(a)),
            }
        }
        let [before, after] = <[PathBuf; 2]>::try_from(bins).unwrap_or_else(|_| usage());
        o.max_rounds = o.max_rounds.max(o.rounds);
        if o.rounds < 2 {
            usage();
        }
        (o.before, o.after) = (before, after);
        o
    }

    fn nixpkgs() -> String {
        if let Ok(np) = env::var("NIXPKGS") {
            return np;
        }
        let expr = concat!(
            "(builtins.getFlake \"",
            env!("CARGO_MANIFEST_DIR"),
            "\").inputs.nixpkgs.outPath"
        );
        let out = Command::new("nix")
            .args(["eval", "--raw", "--impure", "--expr", expr])
            .output()
            .expect("run nix");
        assert!(out.status.success(), "set NIXPKGS: nix eval failed");
        String::from_utf8(out.stdout).expect("utf-8 path")
    }

    fn make_checkout(np: &str, at: &Path) -> Result<(), String> {
        let ok = |c: &mut Command| c.status().is_ok_and(|s| s.success());
        if !ok(Command::new("cp").arg("-r").arg(format!("{np}/.")).arg(at))
            || !ok(Command::new("chmod").arg("-R").arg("u+w").arg(at))
        {
            return Err(format!("cannot copy {np}"));
        }
        let server = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/bench/workloads/nixos-server.nix"
        );
        let server = fs::read_to_string(server).map_err(|e| format!("{server}: {e}"))?;
        let w = |name: &str, text: &str| fs::write(at.join(name), text).map_err(|e| e.to_string());
        // a project marker, so a pure evaluation may read the whole tree
        w("iets.toml", "")?;
        w("bench.nix", &server.replace("<nixpkgs>", "./."))
    }

    struct Row {
        id: &'static str,
        runs: [Vec<Run>; 2],
        status: String,
    }

    fn measure(c: &Ctx, sc: &Scenario, o: &Opts) -> io::Result<Row> {
        let bins = [&o.before, &o.after];
        let mut row = Row {
            id: sc.id,
            runs: [Vec::new(), Vec::new()],
            status: String::new(),
        };
        let err = c.work.join("stderr");
        let edit_at = c.checkout.join(EDIT_FILE);
        let original = fs::read(Path::new(&c.np).join(EDIT_FILE)).unwrap_or_default();
        let fresh = || tempfile::tempdir_in(&c.work).expect("a cache directory");
        let kept = [fresh(), fresh()];
        let result = (|| -> Result<(), RunError> {
            if sc.cache == Cache::Edit {
                fs::write(&edit_at, &original).map_err(|e| e.to_string())?;
            }
            // untimed: warms the page cache, and the kept caches
            for (b, k) in bins.iter().zip(&kept) {
                let d = fresh();
                let cache = if sc.cache == Cache::Cold {
                    d.path()
                } else {
                    k.path()
                };
                run_once(b, &sc.args, cache, &err)?;
            }
            // until the intervals are narrow enough to tell a change of the
            // threshold from none
            let precision = o.threshold.unwrap_or(2.0) / 100.0;
            for round in 0..o.max_rounds {
                if round >= o.rounds
                    && intervals(&row.runs)
                        .iter()
                        .flatten()
                        .all(|i| i.hi - i.lo <= precision)
                {
                    break;
                }
                if sc.cache == Cache::Edit {
                    let mut text = original.clone();
                    text.extend(format!("\n# ab edit {round}\n").bytes());
                    fs::write(&edit_at, text).map_err(|e| e.to_string())?;
                }
                let order = if round % 2 == 0 { [0, 1] } else { [1, 0] };
                for i in order {
                    let d = fresh();
                    let cache = if sc.cache == Cache::Cold {
                        d.path()
                    } else {
                        kept[i].path()
                    };
                    let r = run_once(bins[i], &sc.args, cache, &err)?;
                    row.runs[i].push(r);
                }
            }
            Ok(())
        })();
        if sc.cache == Cache::Edit {
            let _ = fs::write(&edit_at, &original);
        }
        row.status = match result {
            Err(RunError::Cleanup(e)) => return Err(e),
            Err(RunError::Scenario(e)) => format!("FAILED: {e}"),
            Ok(()) => check_output(sc, &row.runs),
        };
        Ok(row)
    }

    fn check_output(sc: &Scenario, runs: &[Vec<Run>; 2]) -> String {
        let [a, b] = runs;
        if sc.cache == Cache::Edit {
            let differs = a.iter().zip(b).any(|(x, y)| x.out != y.out);
            return if differs {
                "OUTPUT DIFFERS".into()
            } else {
                String::new()
            };
        }
        for (name, rs) in [("before", a), ("after", b)] {
            if rs.iter().any(|r| r.out != rs[0].out) {
                return format!("NONDETERMINISTIC ({name})");
            }
        }
        if a[0].out != b[0].out {
            return "OUTPUT DIFFERS".into();
        }
        String::new()
    }

    fn ratios(runs: &[Vec<Run>; 2], f: impl Fn(&Run) -> f64) -> Interval {
        let r: Vec<f64> = runs[0]
            .iter()
            .zip(&runs[1])
            .map(|(a, b)| f(b) / f(a))
            .collect();
        interval(&r)
    }

    /// Wall time, peak RSS and, where counted, instructions.
    fn intervals(runs: &[Vec<Run>; 2]) -> [Option<Interval>; 3] {
        let counted = runs.iter().flatten().all(|r| r.counts.is_some());
        [
            Some(ratios(runs, |r| r.wall)),
            Some(ratios(runs, |r| r.rss)),
            counted.then(|| ratios(runs, |r| r.counts.as_ref().map_or(0.0, |c| c[0]))),
        ]
    }

    fn pct(x: f64) -> String {
        format!("{:+.1}", (x - 1.0) * 100.0)
    }

    fn run_json(rs: &[Run]) -> Vec<serde_json::Value> {
        rs.iter()
            .map(|r| {
                let mut j = json!({
                    "wall": r.wall,
                    "user": r.user,
                    "sys": r.sys,
                    "rss": r.rss,
                    "blocks_read": r.blocks_read,
                    "blocks_written": r.blocks_written,
                });
                for (name, v) in COUNTERS.iter().zip(r.counts.iter().flatten()) {
                    j[*name] = json!(v);
                }
                j
            })
            .collect()
    }

    pub fn main() -> ExitCode {
        let o = parse();
        for b in [&o.before, &o.after] {
            if !b.is_file() {
                eprintln!("ab: {} does not exist", b.display());
                return ExitCode::from(2);
            }
        }
        #[cfg(target_os = "linux")]
        if let Err(e) = enable_subreaper() {
            eprintln!("ab: cannot track leftover processes: {e}");
            return ExitCode::FAILURE;
        }
        #[cfg(target_os = "linux")]
        if counters::open().is_none() {
            let level = fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
                .ok()
                .and_then(|l| l.trim().parse::<i32>().ok());
            match level {
                Some(l) if l > 2 => eprintln!(
                    "ab: the kernel does not allow counting instructions (perf_event_paranoid is {l}). \
                     To allow it until the next reboot:\n\n    sudo sysctl kernel.perf_event_paranoid=2\n"
                ),
                _ => eprintln!(
                    "ab: this machine has no hardware counters, so instructions are not counted"
                ),
            }
        }
        // not /tmp, which may be memory
        let home = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").expect("HOME")).join(".cache"))
            .join("iets-ab");
        fs::create_dir_all(&home).expect("create the work directory");
        let work = tempfile::tempdir_in(&home).expect("create the work directory");
        let c = Ctx {
            np: nixpkgs(),
            checkout: work.path().join("nixpkgs"),
            work: work.path().to_path_buf(),
        };
        let todo: Vec<Scenario> = scenarios(&c)
            .into_iter()
            .filter(|s| o.full || !s.full)
            .filter(|s| o.only.as_ref().is_none_or(|p| s.id.contains(p.as_str())))
            .collect();
        if todo.is_empty() {
            eprintln!("ab: no scenario matches (the nixpkgs scenarios need --full)");
            return ExitCode::from(2);
        }
        if todo.iter().any(|s| s.checkout)
            && let Err(e) = make_checkout(&c.np, &c.checkout)
        {
            eprintln!("ab: {e}");
            return ExitCode::from(2);
        }
        let outpaths = format!(
            "import {np}/ci/eval/outpaths.nix {{ path = {np}; systems = [ builtins.currentSystem ]; }}\n",
            np = c.np
        );
        fs::write(c.work.join("outpaths.nix"), outpaths).expect("write outpaths.nix");

        let mut rows = Vec::new();
        for sc in &todo {
            eprintln!("ab: {}", sc.id);
            match measure(&c, sc, &o) {
                Ok(row) => rows.push(row),
                Err(e) => {
                    eprintln!("ab: stopping benchmark: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }

        let mut failed = false;
        let mut table: Vec<[String; 5]> = Vec::new();
        let mut coverage: f64 = 1.0;
        let mut json_rows = Vec::new();
        for row in &mut rows {
            if row.status.starts_with("FAILED") {
                json_rows.push(json!({"id": row.id, "status": row.status}));
                table.push([
                    row.id.into(),
                    String::new(),
                    String::new(),
                    String::new(),
                    row.status.replace('\n', " "),
                ]);
                failed = true;
                continue;
            }
            let [Some(wall), Some(mem), instr] = intervals(&row.runs) else {
                unreachable!()
            };
            coverage = coverage.min(wall.coverage);
            if let Some(t) = o.threshold {
                let t = 1.0 + t / 100.0;
                let mut slow = Vec::new();
                if wall.lo > t {
                    slow.push("SLOWER");
                }
                if mem.lo > t {
                    slow.push("MORE MEMORY");
                }
                if instr.as_ref().is_some_and(|i| i.lo > t) {
                    slow.push("MORE INSTRUCTIONS");
                }
                if !slow.is_empty() {
                    row.status = [slow.join(", "), row.status.clone()]
                        .join(" ")
                        .trim()
                        .into();
                }
            }
            failed |= !row.status.is_empty();
            let after = |f: fn(&Run) -> f64| median(&row.runs[1].iter().map(f).collect::<Vec<_>>());
            let change =
                |i: &Interval| format!("{} % [{}, {}]", pct(i.median), pct(i.lo), pct(i.hi));
            let width = |i: &Interval| json!({"median": i.median, "lo": i.lo, "hi": i.hi});
            json_rows.push(json!({
                "id": row.id,
                "status": row.status,
                "rounds": row.runs[1].len(),
                "wall": width(&wall),
                "rss": width(&mem),
                "counters": COUNTERS
                    .iter()
                    .enumerate()
                    .filter(|_| instr.is_some())
                    .map(|(n, name)| {
                        let i = ratios(&row.runs, |r| r.counts.as_ref().map_or(0.0, |c| c[n]));
                        (name.to_string(), width(&i))
                    })
                    .collect::<serde_json::Map<_, _>>(),
                "before": run_json(&row.runs[0]),
                "after": run_json(&row.runs[1]),
            }));
            table.push([
                row.id.into(),
                format!("{:.2} s {}", after(|r| r.wall), change(&wall)),
                instr.as_ref().map(change).unwrap_or_default(),
                format!("{:.2} GB {}", after(|r| r.rss / 1e9), change(&mem)),
                row.status.clone(),
            ]);
        }
        // columns that are empty everywhere, as instructions without counters
        let header = ["scenario", "wall", "instructions", "RSS", "status"];
        let shown: Vec<usize> = (0..header.len())
            .filter(|&c| c < 2 || table.iter().any(|r| !r[c].is_empty()))
            .collect();
        let line = |cells: Vec<&str>| println!("| {} |", cells.join(" | "));
        line(shown.iter().map(|&c| header[c]).collect());
        line(shown.iter().map(|_| "---").collect());
        for r in &table {
            line(shown.iter().map(|&c| r[c].as_str()).collect());
        }
        println!(
            "\nIntervals cover the median change with at least {:.0} %.",
            coverage * 100.0
        );

        if let Some(path) = &o.json {
            let doc = json!(json_rows);
            let text = serde_json::to_string_pretty(&doc).expect("json");
            if let Err(e) = fs::write(path, text) {
                eprintln!("ab: {}: {e}", path.display());
                failed = true;
            }
        }
        if failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }

    #[cfg(test)]
    mod tests {
        #[cfg(target_os = "linux")]
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::os::unix::net::UnixStream;
        #[cfg(target_os = "linux")]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(target_os = "linux")]
        use std::path::absolute;
        #[cfg(target_os = "linux")]
        use std::str;

        use super::*;

        #[test]
        fn a_writer_can_outlive_the_child_tree() {
            let (notify, eof) = mpsc::channel();
            notify.send(Ok(())).unwrap();
            wait_for_stdout(&eof, Instant::now()).unwrap();

            let (reader, writer) = UnixStream::pair().unwrap();
            let (notify, eof) = mpsc::channel();
            let hasher = thread::spawn(move || hash_output(reader, notify));
            assert!(matches!(
                wait_for_stdout(&eof, Instant::now()),
                Err(RunError::Cleanup(e)) if e.kind() == io::ErrorKind::TimedOut
            ));
            drop(writer);
            wait_for_stdout(&eof, Instant::now() + Duration::from_secs(10)).unwrap();
            assert_eq!(
                hasher.join().unwrap(),
                Some(<[u8; 32]>::from(Sha256::digest([])))
            );
        }

        #[test]
        fn stdout_error_after_a_complete_line_is_reported() {
            struct Broken;
            impl Read for Broken {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    Err(io::Error::other("injected read failure"))
                }
            }

            let reader = b"complete line\n".as_slice().chain(Broken);
            let (notify, eof) = mpsc::channel();
            let digest = hash_output(reader, notify);
            assert!(matches!(
                wait_for_stdout(&eof, Instant::now()),
                Err(RunError::Scenario(e)) if e == "read benchmark stdout: injected read failure"
            ));
            assert!(digest.is_none());
        }

        #[test]
        fn stats_numbers_are_masked() {
            let a = mask_stats(br#"{"attr":"x","stats":{"wallMs":12,"allocBytes":3.5},"y":1}"#);
            let b = mask_stats(br#"{"attr":"x","stats":{"wallMs":99,"allocBytes":7},"y":1}"#);
            assert_eq!(a, b);
            assert_ne!(
                a,
                mask_stats(br#"{"attr":"x","stats":{"wallMs":99,"allocBytes":7},"y":2}"#)
            );
        }

        #[cfg(target_os = "linux")]
        fn reap_after(
            limit: Duration,
            grace: Duration,
            sweep: impl FnMut(Instant) -> io::Result<()>,
        ) -> io::Result<bool> {
            let until = Instant::now() + limit;
            reap_leftovers(until, until + grace, sweep)
        }

        #[cfg(target_os = "linux")]
        fn wait_until_exited(pid: u32) {
            let mut info = MaybeUninit::uninit();
            // SAFETY: info is writable; WNOWAIT leaves the exit for the reaper.
            let observed = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            assert_eq!(observed, 0);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn a_leftover_process_is_killed_at_the_limit() {
            enable_subreaper().unwrap();
            // The shared boundary identifies either binary once for execution
            // and spawn failures; keep child-spawning assertions in this test.
            {
                let root = tempfile::tempdir().unwrap();
                let false_bin = env::split_paths(&env::var_os("PATH").unwrap())
                    .map(|dir| dir.join("false"))
                    .find(|path| path.is_file())
                    .expect("the false utility");
                let false_bin = absolute(false_bin).unwrap();
                for name in ["before", "after", "missing"] {
                    let bin = root.path().join(name);
                    if name != "missing" {
                        symlink(&false_bin, &bin).unwrap();
                    }
                    let result = run_once(
                        &bin,
                        &["eval".into()],
                        root.path(),
                        &root.path().join("stderr"),
                    );
                    let Err(RunError::Scenario(message)) = result else {
                        panic!("execution failure lost its scenario classification");
                    };
                    let context = format!("{}: ", bin.display());
                    assert_eq!(message.matches(context.as_str()).count(), 1);
                    assert!(message.starts_with(&context));
                    if name != "missing" {
                        assert!(message.contains("exit status"));
                    }
                }
            }
            // The ownership probe must distinguish a live child from its pending exit.
            {
                let mut child = Command::new("sleep").arg("30").spawn().unwrap();
                let id = child.id() as libc::id_t;
                assert_eq!(children_state(libc::P_PID, id).unwrap(), Some(false));
                child.kill().unwrap();
                wait_until_exited(child.id());
                assert_eq!(children_state(libc::P_PID, id).unwrap(), Some(true));
                assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
                assert_eq!(children_state(libc::P_PID, id).unwrap(), None);
            }
            // Make two children waitable after grace expires, before the next poll.
            let mut last = [(); 2].map(|_| Command::new("sleep").arg("30").spawn().unwrap());
            let clean = reap_after(Duration::ZERO, Duration::from_millis(1), |_| {
                thread::sleep(Duration::from_millis(1));
                for child in &mut last {
                    child.kill().unwrap();
                    wait_until_exited(child.id());
                }
                Ok(())
            })
            .unwrap();
            assert!(!clean);
            for mut child in last {
                assert_eq!(child.wait().unwrap_err().raw_os_error(), Some(libc::ECHILD));
            }
            // Reaping an exit at the limit must leave the first sweep available
            // for a live sibling, even if grace has already elapsed.
            let mut ready = Command::new("true").spawn().unwrap();
            wait_until_exited(ready.id());
            let mut live = Command::new("sleep").arg("30").spawn().unwrap();
            assert!(
                !reap_after(Duration::ZERO, Duration::ZERO, |_| {
                    live.kill().unwrap();
                    wait_until_exited(live.id());
                    Ok(())
                })
                .unwrap()
            );
            for child in [&mut ready, &mut live] {
                assert_eq!(child.wait().unwrap_err().raw_os_error(), Some(libc::ECHILD));
            }

            // Two pending exits exhaust a reduced poll budget before a live sibling.
            let ready = [(); 2].map(|_| Command::new("true").spawn().unwrap());
            for child in &ready {
                wait_until_exited(child.id());
            }
            let mut live = Command::new("sleep").arg("30").spawn().unwrap();
            let expired = Instant::now();
            let mut sweeps = 0;
            let error = reap_with_final_polls(expired, expired, 1, |_| {
                sweeps += 1;
                live.kill()?;
                wait_until_exited(live.id());
                Ok(())
            })
            .unwrap_err();
            live.kill().unwrap();
            live.wait().unwrap();
            assert_eq!((error.kind(), sweeps), (io::ErrorKind::TimedOut, 1));

            // A final PID-specific sweep can reap the last waitable child.
            let mut ready = [(); 2].map(|_| Command::new("true").spawn().unwrap());
            for child in &ready {
                wait_until_exited(child.id());
            }
            let mut sweeps = 0;
            let complete = reap_with_final_polls(expired, expired, 0, |_| {
                sweeps += 1;
                for child in &mut ready {
                    let _ = child.try_wait();
                }
                Ok(())
            });
            assert_eq!(sweeps, 1);
            assert!(!complete.unwrap());

            // Model a procfs listing that omits a live child PID.
            let procfs = tempfile::tempdir().unwrap();
            let own_pid = process::id().to_string();
            symlink(&own_pid, procfs.path().join("self")).unwrap();
            let task = procfs.path().join(&own_pid).join("task/1");
            fs::create_dir_all(&task).unwrap();
            let status = procfs.path().join("self/status");
            fs::write(&status, format!("NStgid:\t{own_pid}\t{own_pid}\n")).unwrap();
            assert!(kill_leftovers_in(procfs.path(), Instant::now()).is_err());
            let mut same_namespace = b"Name:\tinvalid-\xff\nNStgid:\t".to_vec();
            same_namespace.extend_from_slice(own_pid.as_bytes());
            fs::write(status, same_namespace).unwrap();
            fs::set_permissions(procfs.path(), fs::Permissions::from_mode(0o111)).unwrap();
            let mut hidden = Command::new("sleep").arg("30").spawn().unwrap();
            fs::write(task.join("children"), hidden.id().to_string()).unwrap();
            assert!(
                !reap_after(Duration::ZERO, CLEANUP_GRACE, |deadline| kill_leftovers_in(
                    procfs.path(),
                    deadline
                ))
                .unwrap()
            );
            assert_eq!(
                hidden.wait().unwrap_err().raw_os_error(),
                Some(libc::ECHILD)
            );

            // With a genuinely unknown child, even a late first sweep cannot
            // claim success or grant itself another grace period.
            let mut unknown = Command::new("sleep").arg("30").spawn().unwrap();
            fs::write(task.join("children"), "").unwrap();
            let expired = Instant::now() - Duration::from_secs(1);
            let mut sweeps = 0;
            let error = reap_leftovers(expired, expired, |deadline| {
                sweeps += 1;
                kill_leftovers_in(procfs.path(), deadline)
            })
            .unwrap_err();
            assert_eq!((sweeps, error.kind()), (1, io::ErrorKind::TimedOut));
            unknown.kill().unwrap();
            unknown.wait().unwrap();
            fs::set_permissions(procfs.path(), fs::Permissions::from_mode(0o700)).unwrap();
            // A kernel without task children files still permits PID-table cleanup.
            fs::remove_file(task.join("children")).unwrap();
            let mut listed = Command::new("sleep").arg("30").spawn().unwrap();
            let listed_entry = procfs.path().join(listed.id().to_string());
            fs::create_dir(&listed_entry).unwrap();
            kill_leftovers_in(procfs.path(), Instant::now() + CLEANUP_GRACE).unwrap();
            assert_eq!(listed.wait().unwrap().signal(), Some(libc::SIGKILL));
            fs::remove_dir(listed_entry).unwrap();

            // A sweep must leave a parent's exit visible to the outer reaper,
            // so its newly adopted child gets the bounded final direct pass.
            for (polls, make_waitable, sweep_fails, complete) in [
                (1, true, false, false),
                (2, true, false, true),
                (2, false, false, false),
                (1, true, true, false),
                (2, true, true, true),
                (2, false, true, false),
            ] {
                let mut parent = Command::new("sh")
                    .args(["-c", "sleep 30 >&- 2>&- & echo $!; wait"])
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut child_pid = String::new();
                BufReader::new(parent.stdout.take().unwrap())
                    .read_line(&mut child_pid)
                    .unwrap();
                let grandchild: libc::pid_t = child_pid.trim().parse().unwrap();
                let expired = Instant::now();
                let mut sweeps = 0;
                let result = reap_with_final_polls(expired, expired, polls, |deadline| {
                    sweeps += 1;
                    let pid = if sweeps == 1 {
                        parent.kill().unwrap();
                        wait_until_exited(parent.id());
                        parent.id()
                    } else if make_waitable {
                        grandchild as u32
                    } else {
                        0
                    };
                    fs::write(task.join("children"), pid.to_string()).unwrap();
                    kill_leftovers_in(procfs.path(), deadline)?;
                    if sweeps == 2 && make_waitable {
                        wait_until_exited(grandchild as u32);
                    }
                    if sweeps == 2 && sweep_fails {
                        return Err(io::Error::from_raw_os_error(libc::EPERM));
                    }
                    Ok(())
                });
                let mut status = 0;
                // SAFETY: this PID names our adopted child; status is writable.
                let reaped = unsafe { libc::waitpid(grandchild, &mut status, libc::WNOHANG) };
                let error = io::Error::last_os_error();
                if reaped == 0 {
                    // SAFETY: clean up the live child before a failing assertion.
                    unsafe {
                        libc::kill(grandchild, libc::SIGKILL);
                        libc::waitpid(grandchild, ptr::null_mut(), 0);
                    }
                }
                assert_eq!(sweeps, 2);
                if complete {
                    assert!(
                        result.is_ok(),
                        "a final sweep error prevented ready-exit draining: {result:?}"
                    );
                    assert!(!result.unwrap());
                    assert_eq!(reaped, -1);
                    assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
                } else {
                    let expected = if sweep_fails {
                        io::ErrorKind::PermissionDenied
                    } else {
                        io::ErrorKind::TimedOut
                    };
                    assert_eq!(result.unwrap_err().kind(), expected);
                    assert_eq!(reaped, if make_waitable { grandchild } else { 0 });
                    if make_waitable {
                        assert!(libc::WIFSIGNALED(status));
                        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
                    }
                }
                assert!(!children_remain().unwrap());
            }

            // An overdue sweep signals known children but skips the PID-table scan.
            let mut known = Command::new("sleep").arg("30").spawn().unwrap();
            let mut fallback = Command::new("sleep").arg("30").spawn().unwrap();
            let mut distant = Command::new("sleep").arg("30").spawn().unwrap();
            fs::write(
                task.join("children"),
                format!("{} {}{}", known.id(), "0 ".repeat(1024), distant.id()),
            )
            .unwrap();
            fs::create_dir(procfs.path().join(fallback.id().to_string())).unwrap();
            kill_leftovers_in(procfs.path(), expired).unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = known.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= until {
                    known.kill().unwrap();
                    known.wait().unwrap();
                    panic!("overdue sweep left the directly listed child alive");
                }
                thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(status.signal(), Some(libc::SIGKILL));
            let until = Instant::now() + Duration::from_millis(100);
            let untouched = loop {
                let untouched =
                    [&mut distant, &mut fallback].map(|child| child.try_wait().unwrap().is_none());
                if untouched != [true, true] || Instant::now() >= until {
                    break untouched;
                }
                thread::sleep(REAP_INTERVAL);
            };
            for child in [&mut distant, &mut fallback] {
                child.kill().unwrap();
                child.wait().unwrap();
            }
            assert_eq!(untouched, [true, true]);
            let t = Instant::now();
            // This direct child belongs to the test thread. Orphans that sh
            // leaves behind can belong to the process leader instead.
            let mut direct = Command::new("sleep").arg("30").spawn().unwrap();
            let nested_pid = procfs.path().join("nested-pid");
            let shell = Command::new("sh")
                // a leftover with a child of its own, which comes to us once it is killed
                .args(["-c", r#"(sleep 30 >/dev/null 2>&1 & echo $! > "$1"; exec >/dev/null 2>&1; wait) & echo $!"#, "sh"])
                .arg(&nested_pid)
                .output()
                .unwrap();
            assert!(shell.status.success());
            let adopted: libc::pid_t = str::from_utf8(&shell.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let nested: libc::pid_t = fs::read_to_string(nested_pid)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(adopted > 0 && nested > 0 && nested != adopted);
            // SAFETY: signal zero checks that the reported nested process exists.
            assert_eq!(unsafe { libc::kill(nested, 0) }, 0);
            // SAFETY: a null status pointer is allowed. Zero proves this live PID is ours.
            assert_eq!(
                unsafe { libc::waitpid(adopted, ptr::null_mut(), libc::WNOHANG) },
                0
            );
            // A sweep may see no children, or fail to signal one it sees.
            // Neither case permits the benchmark to continue measuring.
            for error in [None, Some(io::ErrorKind::PermissionDenied)] {
                let failure = reap_after(Duration::ZERO, Duration::from_millis(30), |_| {
                    error.map_or(Ok(()), |e| Err(io::Error::from(e)))
                })
                .unwrap_err();
                assert_eq!(failure.kind(), error.unwrap_or(io::ErrorKind::TimedOut));
            }
            assert!(!reap_after(Duration::from_secs(1), CLEANUP_GRACE, kill_leftovers).unwrap());
            assert!(t.elapsed() < Duration::from_secs(10));
            assert_eq!(
                direct.wait().unwrap_err().raw_os_error(),
                Some(libc::ECHILD)
            );
            // The adopted shell and its nested sleep must also be gone, not
            // only the direct child. No live or waitable children may remain.
            // SAFETY: a null status pointer is allowed with nonblocking waitpid.
            assert_eq!(
                unsafe { libc::waitpid(-1, ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }

        #[test]
        fn the_interval_widens_until_it_covers() {
            let xs: Vec<f64> = (1..=6).map(f64::from).collect();
            let i = interval(&xs);
            assert_eq!((i.lo, i.hi), (1.0, 6.0));
            assert!((i.coverage - 0.96875).abs() < 1e-9);
            let xs: Vec<f64> = (1..=9).map(f64::from).collect();
            let i = interval(&xs);
            assert_eq!((i.lo, i.hi, i.median), (2.0, 8.0, 5.0));
        }
    }
}
