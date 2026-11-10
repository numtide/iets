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
    use std::io::{BufRead, BufReader, Read};
    use std::mem::MaybeUninit;
    use std::path::{Path, PathBuf};
    use std::process::{self, Command, ExitCode, Stdio};
    use std::ptr;
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
    fn hash_output(r: impl Read) -> [u8; 32] {
        let mut lines: Vec<[u8; 32]> = BufReader::new(r)
            .split(b'\n')
            .map_while(Result::ok)
            .map(|l| Sha256::digest(mask_stats(&l)).into())
            .collect();
        lines.sort_unstable();
        let mut h = Sha256::new();
        for l in &lines {
            h.update(l);
        }
        h.finalize().into()
    }

    /// How long processes a run left behind may go on before they are killed.
    const LEFTOVER_LIMIT: Duration = Duration::from_secs(60);

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
    fn kill_leftovers() {
        let me = process::id();
        let kids = fs::read_to_string(format!("/proc/{me}/task/{me}/children")).unwrap_or_default();
        for pid in kids
            .split_whitespace()
            .filter_map(|p| p.parse::<libc::pid_t>().ok())
        {
            // SAFETY: a signal to a process this one adopted.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn kill_leftovers() {}

    /// Processes the run left behind, which are ours as the subreaper. False
    /// when they had to be killed.
    fn reap_leftovers(limit: Duration) -> bool {
        let until = Instant::now() + limit;
        loop {
            // SAFETY: waitpid with a null status pointer is allowed.
            match unsafe { libc::waitpid(-1, ptr::null_mut(), libc::WNOHANG) } {
                0 if Instant::now() < until => thread::sleep(Duration::from_millis(10)),
                0 => {
                    // a killed process's own children come to us in turn
                    loop {
                        kill_leftovers();
                        // SAFETY: as above.
                        match unsafe { libc::waitpid(-1, ptr::null_mut(), libc::WNOHANG) } {
                            0 => thread::sleep(Duration::from_millis(10)),
                            p if p > 0 => {}
                            _ => return false,
                        }
                    }
                }
                p if p > 0 => {}
                _ => return true,
            }
        }
    }

    fn run_once(bin: &Path, args: &[String], cache: &Path, stderr: &Path) -> Result<Run, String> {
        // a thread of its own, so the counters it opens cover only the run
        thread::scope(|s| {
            s.spawn(|| run(bin, args, cache, stderr))
                .join()
                .expect("the run thread")
        })
    }

    fn run(bin: &Path, args: &[String], cache: &Path, stderr: &Path) -> Result<Run, String> {
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
        let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", bin.display()))?;
        let pid = child.id() as libc::pid_t;
        let out = child.stdout.take().expect("piped");
        let hasher = thread::spawn(move || hash_output(out));
        let mut status = 0;
        let mut ru = MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: the pid is our child, and wait4 fills the struct it is given.
        let ru = unsafe {
            libc::wait4(pid, &mut status, 0, ru.as_mut_ptr());
            ru.assume_init()
        };
        let wall = t0.elapsed().as_secs_f64();
        let clean = reap_leftovers(LEFTOVER_LIMIT);
        let out = hasher.join().expect("the hash thread");
        let used: Vec<f64> = children().iter().zip(before).map(|(a, b)| a - b).collect();
        #[cfg(target_os = "linux")]
        let counts = counters.as_deref_mut().and_then(counters::read);
        #[cfg(not(target_os = "linux"))]
        let counts = None;
        if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
            let err = fs::read_to_string(stderr).unwrap_or_default();
            let tail: Vec<&str> = err.lines().rev().take(5).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            return Err(format!("exit status {status}:\n{}", tail.join("\n")));
        }
        if !clean {
            return Err(format!(
                "processes still ran {}s after it exited, and were killed",
                LEFTOVER_LIMIT.as_secs()
            ));
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

    fn measure(c: &Ctx, sc: &Scenario, o: &Opts) -> Row {
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
        let result = (|| -> Result<(), String> {
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
                    let r = run_once(bins[i], &sc.args, cache, &err)
                        .map_err(|e| format!("{}: {e}", bins[i].display()))?;
                    row.runs[i].push(r);
                }
            }
            Ok(())
        })();
        if sc.cache == Cache::Edit {
            let _ = fs::write(&edit_at, &original);
        }
        row.status = match result {
            Err(e) => format!("FAILED: {e}"),
            Ok(()) => check_output(sc, &row.runs),
        };
        row
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
        // SAFETY: a prctl that takes an integer.
        unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
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
            rows.push(measure(&c, sc, &o));
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
        use super::*;

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
        #[test]
        fn a_leftover_process_is_killed_at_the_limit() {
            // SAFETY: a prctl that takes an integer.
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) };
            let t = Instant::now();
            Command::new("sh")
                // a leftover with a child of its own, which comes to us once it is killed
                .args(["-c", "(sleep 30 & wait) &"])
                .status()
                .unwrap();
            assert!(!reap_leftovers(Duration::from_secs(1)));
            assert!(t.elapsed() < Duration::from_secs(10));
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
