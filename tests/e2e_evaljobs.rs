//! nix-eval-jobs and `iets eval-jobs` with every supported flag combination
//! on the jobsets in tests/evaljobs. The outputs are compared per line, with
//! keys sorted, error text replaced by `true` and timings dropped.
//!
//! Needs `IETS_TEST_NIX_EVAL_JOBS`. The cases that import nixpkgs also need
//! `IETS_TEST_NIXPKGS`.
#![cfg(unix)]

mod e2e;
mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;

use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use e2e::{Store, ok, stderr};
use serde_json::Value;

struct Ctx {
    s: Store,
    nej: String,
    dir: PathBuf,
}

/// The fixtures, copied into a git repo: the flake imports `../ci.nix`.
fn setup() -> Option<Ctx> {
    let Ok(nej) = env::var("IETS_TEST_NIX_EVAL_JOBS") else {
        eprintln!("skipped: IETS_TEST_NIX_EVAL_JOBS unset");
        return None;
    };
    let s = Store::start_with(&[])?;
    let dir = s.dir().join("evaljobs");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/evaljobs"),
        &dir,
    );
    for args in [&["init", "-q"][..], &["add", "."]] {
        ok(Command::new("git").args(args).current_dir(&dir));
    }
    Some(Ctx { s, nej, dir })
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        match e.file_type().unwrap().is_dir() {
            true => copy_dir(&e.path(), &to.join(e.file_name())),
            false => drop(fs::copy(e.path(), to.join(e.file_name())).unwrap()),
        }
    }
}

impl Ctx {
    fn nej(&self, args: &[String]) -> Command {
        let mut c = self.s.nix(&self.nej);
        c.args(["--extra-experimental-features", "nix-command flakes"])
            .args(args)
            .current_dir(&self.dir);
        c
    }

    /// iets evaluates a file purely and nix-eval-jobs follows nix.conf, so a
    /// file case gets `--impure` on the iets side. `--flake` is pure in both.
    fn iets(&self, args: &[String]) -> Command {
        let mut c = self.s.iets();
        c.arg("eval-jobs");
        if !args.iter().any(|a| a == "--flake" || a == "--impure") {
            c.arg("--impure");
        }
        c.args(args).current_dir(&self.dir);
        c
    }
}

fn run(mut c: Command, stdin: Option<&Path>) -> Output {
    c.stdin(match stdin {
        Some(p) => Stdio::from(File::open(p).unwrap()),
        None => Stdio::null(),
    });
    c.output().unwrap()
}

fn norm(o: &Output) -> Vec<String> {
    let mut lines: Vec<String> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| {
            let mut v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}"));
            let m = v.as_object_mut().unwrap();
            m.remove("stats");
            if m.contains_key("error") {
                m.insert("error".into(), Value::Bool(true));
            }
            v.to_string()
        })
        .collect();
    lines.sort();
    lines
}

/// Both tools get `args`. `GCROOTS` becomes a dir per tool, whose entries
/// are compared too.
fn check(c: &Ctx, name: &str, args: &[String], stdin: Option<&str>) -> Result<(), String> {
    let gc = |tool: &str| {
        let d = c.s.dir().join(format!("{name}.{tool}-gc"));
        fs::create_dir_all(&d).unwrap();
        d
    };
    let (ngc, igc) = (gc("nej"), gc("iets"));
    let sub = |d: &Path| -> Vec<String> {
        args.iter()
            .map(|a| a.replace("GCROOTS", d.to_str().unwrap()))
            .collect()
    };
    let stdin = stdin.map(|f| c.dir.join(f));
    let n = run(c.nej(&sub(&ngc)), stdin.as_deref());
    let i = run(c.iets(&sub(&igc)), stdin.as_deref());
    let (nn, ii) = (norm(&n), norm(&i));
    // the whole flake has no derivation at its top level
    let empty_ok = name == "flake-whole";
    if (empty_ok || !nn.is_empty())
        && n.status.code() == i.status.code()
        && nn == ii
        && e2e::ls(&ngc) == e2e::ls(&igc)
    {
        return Ok(());
    }
    Err(format!(
        "{name}: exit nej={:?} iets={:?}\nnej:\n{}\niets:\n{}\nroots {:?} {:?}\n{}",
        n.status.code(),
        i.status.code(),
        nn.join("\n"),
        ii.join("\n"),
        e2e::ls(&ngc),
        e2e::ls(&igc),
        stderr(&i)
    ))
}

/// iets in two chunks against one nix-eval-jobs run.
fn chunks(c: &Ctx, name: &str, args: &[String]) -> Result<(), String> {
    let n = run(c.nej(args), None);
    let mut ii = Vec::new();
    let mut code = None;
    for chunk in ["0", "1"] {
        let mut a: Vec<String> = ["--chunks", "2", "--chunk", chunk]
            .map(String::from)
            .to_vec();
        a.extend_from_slice(args);
        let i = run(c.iets(&a), None);
        ii.extend(norm(&i));
        code = i.status.code();
        if code != Some(0) {
            break;
        }
    }
    ii.sort();
    let nn = norm(&n);
    match n.status.code() == code && nn == ii {
        true => Ok(()),
        false => Err(format!(
            "{name}:\nnej:\n{}\niets:\n{}",
            nn.join("\n"),
            ii.join("\n")
        )),
    }
}

fn expect_fail(c: &Ctx, name: &str, args: &[String]) -> Result<(), String> {
    let n = run(c.nej(args), None).status;
    let i = run(c.iets(args), None).status;
    match !n.success() && !i.success() {
        true => Ok(()),
        false => Err(format!("{name}: exit nej={n} iets={i}, both should fail")),
    }
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn flags_match_nix_eval_jobs() {
    let Some(c) = setup() else { return };
    let alt_lock = c.s.dir().join("alt.lock");
    fs::copy(c.dir.join("flake/flake.lock"), &alt_lock).unwrap();
    let alt_lock = alt_lock.to_str().unwrap();
    let apply = r#"drv: { the-name = drv.name; version = drv.version or null; outs = drv.outputs or [ "out" ]; }"#;
    let impure = r#"{ x = derivation { name = "x-${builtins.currentSystem}"; system = builtins.currentSystem; builder = ":"; }; }"#;
    let checks: &[(&str, &[&str], Option<&str>)] = &[
        ("basic", &["ci.nix"], None),
        ("meta", &["--meta", "ci.nix"], None),
        (
            "force-recurse",
            &["--meta", "--force-recurse", "ci.nix"],
            None,
        ),
        (
            "no-instantiate",
            &[
                "--meta",
                "--no-instantiate",
                "--gc-roots-dir",
                "GCROOTS",
                "ci.nix",
            ],
            None,
        ),
        ("input-drvs", &["--show-input-drvs", "ci.nix"], None),
        (
            "gc-roots",
            &["--gc-roots-dir", "GCROOTS", "--meta", "ci.nix"],
            None,
        ),
        ("apply", &["--apply", apply, "ci.nix"], None),
        ("apply-error", &["--apply", "drv: drv.name", "ci.nix"], None),
        (
            "select",
            &[
                "--select",
                "jobs: { inherit (jobs) builtJob recurse; }",
                "--meta",
                "ci.nix",
            ],
            None,
        ),
        (
            "expr",
            &[
                "--expr",
                r#"import ./ci.nix { system = "aarch64-linux"; }"#,
                "--meta",
            ],
            None,
        ),
        (
            "expr-E",
            &["-E", "(import ./ci.nix { }).recurse", "--meta"],
            None,
        ),
        (
            "argstr",
            &["--argstr", "system", "aarch64-linux", "ci.nix"],
            None,
        ),
        (
            "arg",
            &["--arg", "system", r#""i686-linux""#, "ci.nix"],
            None,
        ),
        (
            "arg-from-file",
            &["--arg-from-file", "system", "system.txt", "ci.nix"],
            None,
        ),
        (
            "arg-from-stdin",
            &["--arg-from-stdin", "system", "ci.nix"],
            Some("system.txt"),
        ),
        (
            "option",
            &[
                "--option",
                "allow-import-from-derivation",
                "false",
                "--show-trace",
                "--quiet",
                "--max-jobs",
                "1",
                "--log-format",
                "raw",
                "ci.nix",
            ],
            None,
        ),
        ("verbose", &["--verbose", "--debug", "ci.nix"], None),
        (
            "log-format-json",
            &["--log-format", "internal-json", "--show-trace", "ci.nix"],
            None,
        ),
        ("impure", &["--impure", "--expr", impure], None),
        (
            "flake",
            &[
                "--flake",
                "./flake#hydraJobs",
                "--meta",
                "--gc-roots-dir",
                "GCROOTS",
            ],
            None,
        ),
        ("flake-whole", &["--flake", "./flake"], None),
        (
            "flake-select",
            &[
                "--flake",
                "./flake",
                "--select",
                "flake: flake.outputs.hydraJobs",
                "--meta",
            ],
            None,
        ),
        (
            "flake-select-fragment",
            &[
                "--flake",
                "./flake#hydraJobs",
                "--select",
                "outputs: { inherit (outputs) builtJob; }",
            ],
            None,
        ),
        (
            "flake-legacy",
            &[
                "--flake",
                "./flake#legacyPackages.x86_64-linux.success",
                "--meta",
            ],
            None,
        ),
        (
            "flake-reference-lock",
            &[
                "--flake",
                "./flake#hydraJobs",
                "--reference-lock-file",
                alt_lock,
                "--meta",
            ],
            None,
        ),
        (
            "constituents",
            &[
                "--constituents",
                "--gc-roots-dir",
                "GCROOTS",
                "--meta",
                "aggregates.nix",
            ],
            None,
        ),
        (
            "constituents-flake",
            &[
                "--constituents",
                "--flake",
                "./flake#legacyPackages.x86_64-linux",
                "--meta",
            ],
            None,
        ),
        (
            "constituents-select",
            &[
                "--constituents",
                "--select",
                "j: { inherit (j) success; }",
                "aggregates.nix",
            ],
            None,
        ),
        (
            "ca",
            &[
                "--extra-experimental-features",
                "ca-derivations",
                "--no-instantiate",
                "--meta",
                "ca.nix",
            ],
            None,
        ),
        (
            "ca-workers",
            &[
                "--extra-experimental-features",
                "ca-derivations",
                "--no-instantiate",
                "--workers",
                "3",
                "ca.nix",
            ],
            None,
        ),
        (
            "cache-status-constituents",
            &[
                "--check-cache-status",
                "--constituents",
                "--gc-roots-dir",
                "GCROOTS",
                "aggregates.nix",
            ],
            None,
        ),
    ];
    let mut failed = Vec::new();
    for (name, a, stdin) in checks {
        failed.extend(check(&c, name, &args(a), *stdin).err());
    }
    if let Ok(np) = env::var("IETS_TEST_NIXPKGS") {
        let inc = format!("nixpkgs={np}");
        let hello = "{ inherit (import <nixpkgs> { }) hello; }";
        let a = args(&["--include", &inc, "--expr", hello, "--no-instantiate"]);
        failed.extend(check(&c, "include", &a, None).err());
        let a = args(&["--check-cache-status", "--meta", "-I", &inc, "cache.nix"]);
        failed.extend(check(&c, "cache-status", &a, None).err());
    }
    failed.extend(chunks(&c, "chunks", &args(&["--meta", "ci.nix"])).err());
    let fails: &[(&str, &[&str])] = &[
        (
            "no-instantiate-constituents",
            &["--no-instantiate", "--constituents", "aggregates.nix"],
        ),
        (
            "no-instantiate-cache",
            &["--no-instantiate", "--check-cache-status", "ci.nix"],
        ),
        (
            "no-instantiate-input-drvs",
            &["--no-instantiate", "--show-input-drvs", "ci.nix"],
        ),
        ("select-not-attrs", &["--select", r#"x: "str""#, "ci.nix"]),
        ("no-expr", &[]),
    ];
    for (name, a) in fails {
        failed.extend(expect_fail(&c, name, &args(a)).err());
    }
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}

/// Every internal-json record of one run carries one `sid`, and
/// IETS_SESSION_ID sets it (docs/reference/errors.md).
#[rstest]
fn one_session_id_per_run(bin: Iets) {
    let e = r#"{ a = builtins.trace "a" (derivation { name = "a"; system = "x"; builder = ":"; }); b = builtins.trace "b" (derivation { name = "b"; system = "x"; builder = ":"; }); }"#;
    let sids = |id: Option<&str>| -> Vec<String> {
        let mut c = bin.cmd();
        if let Some(id) = id {
            c.env("IETS_SESSION_ID", id);
        }
        c.args(["eval-jobs", "--workers", "2", "--no-instantiate"])
            .args(["--log-format", "internal-json", "--expr", e]);
        let o = c.output().unwrap();
        stderr(&o)
            .lines()
            .filter_map(|l| l.strip_prefix("@nix "))
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                v["sid"].as_str().unwrap_or_default().to_string()
            })
            .collect()
    };
    let own = sids(None);
    assert_eq!(own.len(), 3, "{own:?}");
    assert!(own.iter().all(|s| *s == own[0]), "{own:?}");
    assert_eq!(own[0].as_bytes()[14], b'7', "a UUIDv7: {}", own[0]);
    assert!(sids(Some("run-42")).iter().all(|s| s == "run-42"));
}

fn ci(bin: &Iets) -> Command {
    let mut c = bin.cmd();
    c.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/evaljobs"))
        .args(["eval-jobs", "--workers", "2", "--no-instantiate"]);
    c
}

/// Every line from a worker carries the `stats` nixbot reads.
#[rstest]
fn every_line_has_stats(bin: Iets) {
    let o = ok(ci(&bin).arg("ci.nix"));
    let lines: Vec<Value> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(!lines.is_empty());
    for l in &lines {
        assert!(
            l["stats"]["wallMs"].is_u64() && l["stats"]["allocBytes"].is_u64(),
            "{l}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|l| l["stats"]["allocBytes"].as_u64() > Some(0))
    );
}

/// `--verbose` and `--debug` are one warning each, from the main process;
/// `--max-jobs` and `--quiet` hold (docs/reference/divergences.md).
#[rstest]
fn ignored_flags_warn_once(bin: Iets) {
    let o = ci(&bin)
        .args([
            "--verbose",
            "--debug",
            "--verbose",
            "--max-jobs",
            "1",
            "--quiet",
            "ci.nix",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    let warned: Vec<String> = stderr(&o)
        .lines()
        .filter(|l| l.starts_with("warning: "))
        .map(String::from)
        .collect();
    let want: Vec<String> = ["--verbose", "--debug"]
        .iter()
        .map(|f| {
            format!("warning: '{f}' is ignored: iets has one log level, the one Nix starts at")
        })
        .collect();
    assert_eq!(warned, want);
}

/// `--iets-option` leaves the argv before any command sees it, so the
/// collector puts it back for every worker. Only a worker that read
/// `cache-dir` makes the directory.
#[rstest]
fn iets_options_reach_the_workers(bin: Iets) {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("opt-cache");
    let mut c = bin.cmd();
    c.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/evaljobs"))
        .args(["eval-jobs", "--iets-option", "cache-dir"])
        .arg(&cache)
        .args(["--workers", "2", "--no-instantiate", "ci.nix"]);
    ok(&mut c);
    assert!(cache.exists());
}
