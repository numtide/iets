//! The tracing evaluation cache (docs/explanation/caching.md): a hit prints
//! the same output, every kind of change misses, uncacheable and failed runs
//! write nothing, and each cache flag turns off the store it names.
#![cfg(unix)]

mod hermetic;

use hermetic::Iets;
use rstest::{fixture, rstest};

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

struct Run {
    rc: i32,
    out: String,
    /// the `cache:` lines, joined
    status: String,
    /// the `re-evaluating:` line of an invalidation
    why: String,
}

struct Work {
    dir: TempDir,
    bin: Iets,
}

#[fixture]
fn work() -> Work {
    Work {
        dir: tempfile::tempdir().unwrap(),
        bin: hermetic::bin(),
    }
}

impl Work {
    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn s(&self, rel: &str) -> String {
        self.path(rel).to_str().unwrap().to_string()
    }

    fn write(&self, rel: &str, text: &str) -> String {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p.to_str().unwrap().to_string()
    }

    fn iets(&self) -> Command {
        let mut c = self.bin.cmd();
        c.env("IETS_CACHE_DIR", self.path("cache"))
            .env("CACHETEST_VAR", "one")
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        c
    }

    /// `iets eval --impure --strict --timings ARGS`. The workload observes
    /// the environment and the search path, which the cache invalidates on.
    fn eval(&self, args: &[&str]) -> Run {
        self.eval_with(&[], args)
    }

    fn eval_with(&self, env: &[(&str, &str)], args: &[&str]) -> Run {
        let mut c = self.iets();
        c.envs(env.iter().copied())
            .args(["eval", "--impure", "--strict", "--timings"])
            .args(args);
        run(c)
    }
}

fn run(mut c: Command) -> Run {
    let o = c.output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    let lines = |p: &str| -> Vec<&str> { err.lines().filter(|l| l.starts_with(p)).collect() };
    Run {
        rc: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
        status: lines("cache:").join(" "),
        why: lines("re-evaluating:").join(" "),
    }
}

/// Sets the mtime of `files` to `secs` after the epoch. BSD touch has no
/// `-d` for an ISO date.
fn touch(secs: u64, files: &[&str]) {
    for f in files {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        // a read handle, because one of them is a directory
        fs::File::open(f).unwrap().set_modified(t).unwrap();
    }
}

/// name, mtime and size of every file under `dir`
fn fingerprint(dir: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            if m.is_dir() {
                stack.push(e.path());
            } else {
                out.push((e.path(), m.modified().unwrap(), m.len()));
            }
        }
    }
    out.sort();
    out
}

fn entries(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_type().unwrap().is_dir())
        .count()
}

const LOCK: &str = "{\"nodes\":{\"root\":{}},\"root\":\"root\",\"version\":7}\n";

/// The main workload: an import, a readDir, a getEnv and a search path.
fn main_nix(w: &Work) -> Vec<String> {
    w.write(
        "src/main.nix",
        "{ a = import ./lib.nix; b = builtins.attrNames (builtins.readDir ./dir); c = builtins.getEnv \"CACHETEST_VAR\"; d = import <inc/x.nix>; }\n",
    );
    w.write("src/lib.nix", "40 + 2\n");
    w.write("src/dir/one", "x\n");
    w.write("inc1/x.nix", "\"inc1\"\n");
    w.write("inc2/x.nix", "\"inc2\"\n");
    vec![
        "-I".into(),
        format!("inc={}", w.s("inc1")),
        w.s("src/main.nix"),
    ]
}

fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

#[rstest]
fn hits_and_misses(work: Work) {
    let main = main_nix(&work);
    let main = strs(&main);
    let lib = work.s("src/lib.nix");

    let r = work.eval(&main);
    assert!(
        r.rc == 0 && r.status.contains("miss (no entry)") && r.status.contains("wrote"),
        "{}",
        r.status
    );
    assert_eq!(r.why, "", "a cold run reports no invalidation");
    let first = r.out;
    let r = work.eval(&main);
    assert!(
        r.status.contains("cache: hit") && r.out == first,
        "{}",
        r.status
    );
    assert_eq!(r.why, "");

    work.write("src/lib.nix", "40 + 3\n");
    let r = work.eval(&main);
    assert!(
        r.status.contains("miss (") && r.status.contains("lib.nix changed"),
        "{}",
        r.status
    );
    assert!(r.out.contains("a = 43"));
    let changed = format!("re-evaluating: {lib} changed");
    assert_eq!(r.why, changed);
    // the same message whichever thread saw the change first
    for _ in 0..5 {
        work.write("src/lib.nix", "40 + 4\n");
        assert_eq!(work.eval(&main).why, changed);
        work.write("src/lib.nix", "40 + 3\n");
        assert_eq!(work.eval(&main).why, changed);
    }

    touch(946_684_800, &[&lib]);
    let r = work.eval(&main);
    assert!(
        r.status.contains("cache: hit") && r.status.contains(" 1 rehashed"),
        "{}",
        r.status
    );
    let r = work.eval(&main);
    assert!(
        r.status.contains("cache: hit") && r.status.contains(" 0 rehashed"),
        "{}",
        r.status
    );

    work.write("src/dir/two", "y\n");
    let r = work.eval(&main);
    assert!(
        r.status.contains("miss (") && r.status.contains("directory"),
        "{}",
        r.status
    );
    assert!(r.out.contains("\"two\""));
    let dir = work.s("src/dir");
    assert_eq!(
        r.why,
        format!("re-evaluating: directory {dir} has different entries")
    );

    let r = work.eval_with(&[("CACHETEST_VAR", "two")], &main);
    assert!(
        r.status
            .contains("miss (environment variable $CACHETEST_VAR changed)"),
        "{}",
        r.status
    );
    assert!(r.out.contains("c = \"two\""));
    assert_eq!(
        r.why,
        "re-evaluating: environment variable $CACHETEST_VAR changed"
    );
    let r = work.eval(&main);
    assert!(
        r.status.contains("CACHETEST_VAR"),
        "one entry per key: {}",
        r.status
    );

    let inc2 = format!("inc={}", work.s("inc2"));
    let r = work.eval(&["-I", &inc2, main[2]]);
    assert!(
        r.status.contains("miss (no entry)") && r.out.contains("d = \"inc2\""),
        "{}",
        r.status
    );

    // a failed evaluation writes nothing, and the fixed file is no stale hit
    work.write("src/lib.nix", "throw \"boom\"\n");
    let r = work.eval(&main);
    assert!(r.rc != 0 && !r.status.contains("wrote"), "{}", r.status);
    work.write("src/lib.nix", "1\n");
    let r = work.eval(&main);
    assert!(r.rc == 0 && r.status.contains("miss ("), "{}", r.status);
}

/// `cp -p` in place keeps the size, the mtime and the inode. Only ctime says
/// the bytes are new.
#[rstest]
fn a_copy_in_place_misses(work: Work) {
    let main = work.write("ctime/main.nix", "import ./lib.nix\n");
    let lib = work.write("ctime/lib.nix", "\"AAAA\"\n");
    let other = work.write("ctime/other.nix", "\"BBBB\"\n");
    touch(1_577_836_800, &[&lib, &other]);
    let cc = work.s("ctimecache");
    let c1 = work.eval(&["--cache-dir", &cc, &main]);
    assert!(
        Command::new("cp")
            .args(["-p", &other, &lib])
            .status()
            .unwrap()
            .success()
    );
    let c2 = work.eval(&["--cache-dir", &cc, &main]);
    assert_eq!(c1.out, "\"AAAA\"");
    assert_eq!(c2.out, "\"BBBB\"");
    assert!(c2.status.contains("miss ("), "{}", c2.status);
}

/// The compiled-file cache folds `builtins.currentSystem` into an entry, and
/// `--pure-eval` removes that name, so an impure run must not answer a pure one.
#[rstest]
fn a_pure_run_never_reads_an_impure_entry(work: Work) {
    let pad: String = (1..=30)
        .map(|i| format!("# comment line {i} padding padding padding padding\n"))
        .collect();
    work.write(
        "pure/lib.nix",
        &format!("{pad}{{ dflt = builtins.currentSystem or \"fallback\"; }}\n"),
    );
    work.write(
        "pure/flake.nix",
        "{ outputs = _: { dflt = (import ./lib.nix).dflt; }; }\n",
    );
    work.write("pure/flake.lock", LOCK);
    let pc = work.s("purecache");
    let flake = format!("path:{}#dflt", work.s("pure"));
    let ev = |mode: &str| {
        let mut c = work.iets();
        c.args(["eval", "--cache-dir", &pc, mode, &flake]);
        run(c).out
    };
    let p1 = ev("--pure-eval");
    fs::remove_dir_all(&pc).unwrap();
    let p2 = ev("--impure");
    let p3 = ev("--pure-eval");
    assert_eq!(p1, "\"fallback\"");
    assert_ne!(p2, "\"fallback\"");
    assert_eq!(p3, "\"fallback\"");
}

#[rstest]
fn uncacheable_runs_write_nothing(work: Work) {
    let r = work.eval(&["-E", "builtins.currentTime > 0"]);
    assert!(r.status.contains("not written"), "{}", r.status);
    let r = work.eval(&["-E", "builtins.currentTime > 0"]);
    assert!(r.status.contains("miss (no entry)"), "{}", r.status);

    let fetch = r#"(builtins.tryEval (builtins.fetchTarball "https://x/y.tar.gz")).success"#;
    let r = work.eval(&["-E", fetch]);
    assert!(r.rc != 0 && !r.status.contains("wrote"), "{}", r.status);

    // `nix eval` prints «error: …» for a read that failed, with exit 0, and
    // that read left no observation
    let e = work.write(
        "errs/e.nix",
        "{ b = import ./missing.nix; c = builtins.readDir ./missingdir; }\n",
    );
    let nix_eval = || {
        let mut c = work.iets();
        c.args(["nix", "eval", "--file", &e]);
        c.output().unwrap()
    };
    let e1 = nix_eval();
    work.write("errs/missing.nix", "\"B\"\n");
    fs::create_dir(work.path("errs/missingdir")).unwrap();
    let e2 = nix_eval();
    assert!(e1.status.success() && String::from_utf8_lossy(&e1.stdout).contains("«error:"));
    assert!(e2.status.success() && !String::from_utf8_lossy(&e2.stdout).contains("«error:"));
}

#[rstest]
fn flags_that_turn_the_cache_off(work: Work) {
    let main = main_nix(&work);
    let main = strs(&main);
    work.eval(&main);
    let cache = work.path("cache");
    let before = fingerprint(&cache);
    let mut args = vec!["--no-tracing-cache"];
    args.extend(&main);
    assert_eq!(work.eval(&args).status, "");
    work.eval(&[
        "--no-tracing-cache",
        "-E",
        "import /nonexistent/nocache-probe.nix",
    ]);
    assert_eq!(
        fingerprint(&cache),
        before,
        "--no-tracing-cache touched the store"
    );
    assert_eq!(work.eval_with(&[("IETS_CACHE", "0")], &main).status, "");

    // each flag turns off the store it names, and no other
    let cases: [(&[&str], bool, bool); 4] = [
        (&[], true, true),
        (&["--no-tracing-cache"], false, true),
        (&["--no-ir-cache"], true, false),
        (&["--no-cache"], false, false),
    ];
    for (i, (flags, trace, ir)) in cases.into_iter().enumerate() {
        let dir = work.s(&format!("stores{i}"));
        let mut args = vec!["--cache-dir", &dir];
        args.extend(flags);
        args.extend(&main);
        assert_eq!(work.eval(&args).rc, 0);
        let got = (
            work.path(&format!("stores{i}/trace")).exists(),
            work.path(&format!("stores{i}/ir")).exists(),
        );
        assert_eq!(got, (trace, ir), "{flags:?}");
    }
}

/// A lint warning is an effect: the entry records it and a hit prints it again.
#[rstest]
fn a_lint_warning_is_replayed_on_a_hit(work: Work) {
    let url = work.write("src/url.nix", "http://z.example\n");
    let w1 = work.eval(&["--lint", "url-literal=warn", &url]);
    let w2 = work.eval(&["--lint", "url-literal=warn", &url]);
    assert!(w1.status.contains("miss"), "{}", w1.status);
    assert!(w2.status.contains("cache: hit"), "{}", w2.status);
    let o = work
        .iets()
        .args([
            "eval",
            "--impure",
            "--strict",
            "--timings",
            "--lint",
            "url-literal=warn",
            &url,
        ])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(
        err.lines()
            .filter(|l| l.starts_with("warning: [url-literal]"))
            .count(),
        1
    );
    assert!(err.contains("cache: hit"));
}

/// The file on the command line is in the read set, not in the key.
#[rstest]
fn the_file_argument_keeps_one_entry(work: Work) {
    let kd = work.s("keycache");
    let top = work.write("key/top.nix", "import ./dep.nix\n");
    work.write("key/dep.nix", "41 + 1\n");
    let ev = || work.eval(&["--cache-dir", &kd, &top]);
    let k = ev();
    assert!(
        k.status.contains("miss (no entry)") && k.why.is_empty(),
        "{}",
        k.status
    );
    assert!(ev().status.contains("cache: hit"));
    work.write("key/top.nix", "import ./dep.nix # edited\n");
    assert_eq!(ev().why, format!("re-evaluating: {top} changed"));
    assert!(ev().status.contains("cache: hit"));
    assert_eq!(entries(&work.path("keycache/trace")), 1);
    fs::remove_file(&top).unwrap();
    let k = ev();
    assert!(k.rc != 0);
    assert_eq!(k.why, format!("re-evaluating: {top} is gone"));
}

/// A flake's directory is the key, and flake.nix and flake.lock are read like
/// any other file.
#[rstest]
fn a_flake_keeps_one_entry(work: Work) {
    let fd = work.s("flakecache");
    let fl = work.s("fl");
    work.write("fl/flake.nix", "{ outputs = _: { x = 41 + 1; }; }\n");
    work.write("fl/flake.lock", LOCK);
    let flake = format!("path:{fl}#x");
    let ev = || work.eval(&["--cache-dir", &fd, &flake]);
    let f = ev();
    assert!(
        f.rc == 0 && f.status.contains("miss (no entry)") && f.why.is_empty(),
        "{}",
        f.status
    );
    assert!(ev().status.contains("cache: hit"));
    work.write("fl/flake.nix", "{ outputs = _: { x = 40 + 2; }; }\n");
    let f = ev();
    assert_eq!(f.why, format!("re-evaluating: source tree {fl} changed"));
    assert_eq!(f.out, "42");
    work.write("fl/flake.lock", &LOCK.replace('\n', " \n"));
    assert_eq!(ev().why, format!("re-evaluating: {fl}/flake.lock changed"));
    assert_eq!(entries(&work.path("flakecache/trace")), 1);

    // a `path:` flake's lastModified is the newest mtime of its tree
    let lm = work.s("lm");
    let nix = work.write(
        "lm/flake.nix",
        "{ outputs = { self }: { x = self.lastModified; }; }\n",
    );
    let lock = work.write("lm/flake.lock", LOCK);
    touch(1_577_836_800, &[&nix, &lock, &lm]);
    let flake = format!("path:{lm}#x");
    let f = work.eval(&["--cache-dir", &fd, &flake]);
    touch(1_609_459_200, &[&nix]);
    let g = work.eval(&["--cache-dir", &fd, &flake]);
    assert_eq!(f.out, "1577836800");
    assert_eq!(g.out, "1609459200");
    assert_eq!(
        g.why,
        format!("re-evaluating: source tree {lm} has another modification time")
    );
}

/// iets owns only the store subdirectories it makes in `--cache-dir`.
#[rstest]
fn the_cache_dir_keeps_what_it_did_not_make(work: Work) {
    let unrelated = work.write("mine/unrelated.txt", "PRECIOUS\n");
    let mine = work.s("mine");
    assert_eq!(work.eval(&["--cache-dir", &mine, "-E", "2"]).rc, 0);
    assert_eq!(fs::read_to_string(&unrelated).unwrap(), "PRECIOUS\n");
    assert!(work.path("mine/trace").exists() && work.path("mine/ir").exists());

    // a directory that is not iets's is refused, not wiped
    let notes = work.write("theirs/trace/notes.txt", "PRECIOUS\n");
    let theirs = work.s("theirs");
    let o = work
        .iets()
        .args([
            "eval",
            "--strict",
            "--timings",
            "--cache-dir",
            &theirs,
            "-E",
            "2",
        ])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("is not an iets cache"));
    assert_eq!(fs::read_to_string(&notes).unwrap(), "PRECIOUS\n");

    // --ttl in days must fit in seconds
    let ttl = work.s("ttl");
    let g = work
        .iets()
        .args([
            "cache",
            "gc",
            "--cache-dir",
            &ttl,
            "--ttl",
            "213503982334602",
        ])
        .output()
        .unwrap();
    assert_eq!(g.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&g.stderr).contains("--ttl wants a number of days"));
}

/// A hit prints the same output for a lang test with paths and positions.
#[rstest]
fn a_hit_prints_paths_and_positions_the_same(work: Work) {
    let t = "tests/lang/eval-okay-getattrpos.nix";
    let o1 = work.iets().args(["eval", "--strict", t]).output().unwrap();
    let r2 = work
        .iets()
        .args(["eval", "--strict", "--timings", t])
        .output()
        .unwrap();
    assert_eq!(o1.stdout, r2.stdout);
    let err = String::from_utf8_lossy(&r2.stderr);
    assert_eq!(err.lines().filter(|l| l.contains("cache: hit")).count(), 1);
}

/// search takes the same cache and worker flags as eval.
#[rstest]
fn search_uses_the_cache(work: Work) {
    let pkgs = work.write(
        "src/pkgs.nix",
        "{ }: { hello = derivation { name = \"hello-1\"; system = builtins.currentSystem; builder = \"/bin/sh\"; args = [ ]; } // { meta.description = \"greets\"; }; }\n",
    );
    let search = |dir: &str, extra: &[&str]| {
        let mut c = work.iets();
        c.args(["search", "-f", &pkgs, "--cache-dir", dir, "--timings"])
            .args(extra)
            .arg("hello");
        run(c)
    };
    let sdir = work.s("searchcache");
    let s1 = search(&sdir, &[]);
    let s2 = search(&sdir, &[]);
    assert!(s1.rc == 0 && s1.status.contains("miss"), "{}", s1.status);
    assert!(
        s2.rc == 0 && s2.status.contains("cache: hit") && s2.out == s1.out,
        "{}",
        s2.status
    );
    let before = fingerprint(&work.path("searchcache"));
    let s3 = search(&sdir, &["--no-tracing-cache"]);
    assert!(s3.rc == 0 && s3.status.is_empty() && s3.out == s1.out);
    assert_eq!(fingerprint(&work.path("searchcache")), before);
    let idir = work.s("searchcache-noir");
    assert_eq!(search(&idir, &["--no-ir-cache"]).rc, 0);
    assert!(!work.path("searchcache-noir/ir").exists());
    let s5 = search(&sdir, &["--workers", "2"]);
    assert!(s5.rc == 0 && s5.out == s1.out);
    let o = work
        .iets()
        .args(["search", "-f", &pkgs, "--workers", "0", "hello"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("--workers wants a number >= 1"));
}
