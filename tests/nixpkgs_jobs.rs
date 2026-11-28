//! The job set nixpkgs's CI evaluates (`ci/eval/outpaths.nix`, about 82,000
//! jobs on x86_64-linux) through `iets eval-jobs`, against nix-eval-jobs and
//! against itself. `tests/nixpkgs_eval.rs` compares one drv path per
//! top-level package; these compare what the documentation promises of the
//! output (docs/reference/commands.md, docs/explanation/evaluation.md,
//! docs/explanation/provenance.md):
//!
//! - every line as nix-eval-jobs 2.35.4 writes it, every key, `stats` by its
//!   keys and not its numbers, an error by its message (the trace around it
//!   may differ, docs/reference/divergences.md), `warnings` and `traces`, and
//!   the exit code; the `trace:` and `evaluation warning:` lines of stderr
//!   as a set, since worker processes print a shared one once each;
//! - the same lines from worker processes and from one process, error text
//!   and all: evaluation is deterministic by design;
//! - with instantiation, into a scratch store with no substituter, the same
//!   store paths and the same bytes in every `.drv`. Nothing is built or
//!   fetched.
//!
//! On demand: each test is ignored, evaluates nixpkgs at least twice and
//! takes minutes. Needs `IETS_TEST_NIXPKGS` (a nixpkgs source) and
//! `IETS_TEST_NIX_EVAL_JOBS` (the binary); the instantiation test also a
//! `nix-daemon` on PATH. `IETS_TEST_NIXPKGS_WORKERS` sets `--workers` for
//! every run (default 4), `IETS_TEST_NIXPKGS_SELECT` a `--select` that
//! narrows the job set, for both tools.

mod e2e;
mod hermetic;

use hermetic::{Iets, bin, isolate};
use rstest::rstest;

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::hash::Hasher;
use std::path::Path;
use std::process::Command;

use e2e::Store;
use serde_json::Value;
use tempfile::TempDir;

/// One run's lines by attribute path, the `trace:` and `evaluation
/// warning:` lines of its stderr, and its exit code.
struct Run {
    lines: BTreeMap<Vec<String>, Value>,
    logs: BTreeSet<String>,
    code: Option<i32>,
}

fn run(c: &mut Command) -> Run {
    let o = c.output().expect("spawn");
    let mut lines = BTreeMap::new();
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        let v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}"));
        let path: Vec<String> =
            serde_json::from_value(v["attrPath"].clone()).unwrap_or_else(|e| panic!("{e}: {l}"));
        assert!(lines.insert(path, v).is_none(), "an attribute twice: {l}");
    }
    let logs = String::from_utf8_lossy(&o.stderr)
        .lines()
        .map(strip_ansi)
        .filter(|l| l.starts_with("trace: ") || l.starts_with("evaluation warning: "))
        .collect();
    Run {
        lines,
        logs,
        code: o.status.code(),
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: parameters up to a final byte in @..~
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The message of an error text: its last `error: ...` line. Nix puts the
/// trace first and the message last, iets the message first; the message
/// is the part both must agree on.
fn message(text: &str) -> String {
    let plain = strip_ansi(text);
    plain
        .lines()
        .map(str::trim)
        .rfind(|l| l.starts_with("error:") && *l != "error:")
        .unwrap_or_else(|| plain.lines().next().unwrap_or(""))
        .to_string()
}

/// A line as it must agree with nix-eval-jobs's: `stats` by its keys, an
/// error by its message.
fn against_nix(v: &Value) -> Value {
    let mut m = v.as_object().expect("a JSON object").clone();
    if let Some(s) = m.remove("stats") {
        let keys: Vec<&String> = s
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        m.insert("stats".into(), serde_json::json!(keys));
    }
    if let Some(Value::String(e)) = m.get("error") {
        let msg = message(e);
        m.insert("error".into(), Value::String(msg));
    }
    Value::Object(m)
}

/// A line as it must agree between iets's two process models: everything
/// but `stats`, which one process does not write
/// (docs/reference/commands.md).
fn between_modes(v: &Value) -> Value {
    let mut m = v.as_object().expect("a JSON object").clone();
    m.remove("stats");
    Value::Object(m)
}

/// Every difference between `a` and `b` after `view`, by key, the first few
/// of each spelled out; empty when they agree.
fn differences(names: (&str, &str), a: &Run, b: &Run, view: fn(&Value) -> Value) -> Vec<String> {
    let mut out = Vec::new();
    if a.code != b.code {
        out.push(format!(
            "exit: {} {:?}, {} {:?}",
            names.0, a.code, names.1, b.code
        ));
    }
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in a
        .lines
        .keys()
        .chain(b.lines.keys())
        .collect::<BTreeSet<_>>()
    {
        let shown = path.join(".");
        let (Some(x), Some(y)) = (a.lines.get(path), b.lines.get(path)) else {
            let only = if a.lines.contains_key(path) {
                names.0
            } else {
                names.1
            };
            by_key
                .entry("(the line)".into())
                .or_default()
                .push(format!("{shown}: only {only}"));
            continue;
        };
        let (x, y) = (view(x), view(y));
        let (x, y) = (x.as_object().unwrap(), y.as_object().unwrap());
        for k in x.keys().chain(y.keys()).collect::<BTreeSet<_>>() {
            if x.get(k) != y.get(k) {
                let show = |v: Option<&Value>| v.map_or("(none)".into(), |v| v.to_string());
                by_key.entry(k.clone()).or_default().push(format!(
                    "{shown}\n      {}: {}\n      {}: {}",
                    names.0,
                    show(x.get(k)),
                    names.1,
                    show(y.get(k))
                ));
            }
        }
    }
    for (k, v) in &by_key {
        out.push(format!("`{k}` differs on {} lines:", v.len()));
        out.extend(v.iter().take(5).map(|d| format!("    {d}")));
    }
    for (name, x, y) in [(names.0, &a.logs, &b.logs), (names.1, &b.logs, &a.logs)] {
        let only: Vec<&String> = x.difference(y).take(5).collect();
        if !only.is_empty() {
            out.push(format!("stderr lines only from {name}: {only:?}"));
        }
    }
    out
}

struct Setup {
    nixpkgs: String,
    nej: String,
    workers: String,
    select: Option<String>,
}

fn setup() -> Option<Setup> {
    let (Ok(nixpkgs), Ok(nej)) = (
        env::var("IETS_TEST_NIXPKGS"),
        env::var("IETS_TEST_NIX_EVAL_JOBS"),
    ) else {
        eprintln!("skipped: IETS_TEST_NIXPKGS or IETS_TEST_NIX_EVAL_JOBS unset");
        return None;
    };
    Some(Setup {
        nixpkgs,
        nej,
        workers: env::var("IETS_TEST_NIXPKGS_WORKERS").unwrap_or("4".into()),
        select: env::var("IETS_TEST_NIXPKGS_SELECT").ok(),
    })
}

impl Setup {
    /// The arguments both tools get: the job set for this machine's system.
    fn args(&self, extra: &[&str]) -> Vec<String> {
        let np = &self.nixpkgs;
        let expr = format!(
            "import {np}/ci/eval/outpaths.nix {{ path = {np}; systems = [ builtins.currentSystem ]; }}"
        );
        let mut a: Vec<String> = ["--impure", "--workers", &self.workers, "--expr", &expr]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(s) = &self.select {
            a.extend(["--select".into(), s.clone()]);
        }
        a.extend(extra.iter().map(|s| s.to_string()));
        a
    }
}

fn assert_agree(what: &str, d: Vec<String>, a: &Run) {
    assert!(!a.lines.is_empty(), "{what}: no lines");
    assert!(
        d.is_empty(),
        "{what}, {} lines:\n{}",
        a.lines.len(),
        d.join("\n")
    );
}

#[rstest]
#[ignore = "evaluates the nixpkgs job set twice; set IETS_TEST_NIXPKGS"]
fn jobset_lines_match_nix_eval_jobs(bin: Iets) {
    let Some(s) = setup() else { return };
    let home = TempDir::new().unwrap();
    let mut nej = Command::new(&s.nej);
    isolate(&mut nej);
    // in memory: no daemon, nothing written
    nej.args(["--store", "dummy://?read-only=false", "--no-instantiate"])
        .args(s.args(&[]))
        .env("NIX_CONF_DIR", "/nonexistent")
        .env("NIX_USER_CONF_FILES", "")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join(".cache"));
    let theirs = run(&mut nej);
    let mut iets = bin.cmd();
    iets.arg("eval-jobs")
        .arg("--no-instantiate")
        .args(s.args(&[]))
        .env("NIX_DAEMON_SOCKET_PATH", "/nonexistent/socket");
    let ours = run(&mut iets);
    let d = differences(("nix-eval-jobs", "iets"), &theirs, &ours, against_nix);
    assert_agree("iets against nix-eval-jobs", d, &theirs);
}

#[rstest]
#[ignore = "evaluates the nixpkgs job set twice; set IETS_TEST_NIXPKGS"]
fn jobset_is_the_same_in_both_process_models(bin: Iets) {
    let Some(s) = setup() else { return };
    let jobs = |max_memory: &str| {
        let mut c = bin.cmd();
        c.arg("eval-jobs")
            .args(["--no-instantiate", "--max-memory-size", max_memory])
            .args(s.args(&[]))
            .env("NIX_DAEMON_SOCKET_PATH", "/nonexistent/socket");
        run(&mut c)
    };
    let processes = jobs("4096");
    let one = jobs("0");
    let d = differences(
        ("worker processes", "one process"),
        &processes,
        &one,
        between_modes,
    );
    assert_agree("worker processes against one process", d, &processes);
}

/// The store paths in `store` (`.links` and locks aside), each `.drv` with a
/// hash of its bytes.
fn snapshot(store: &Path) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for e in fs::read_dir(store).unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.ends_with(".lock") {
            continue;
        }
        let h = if name.ends_with(".drv") {
            let mut h = DefaultHasher::new();
            h.write(&fs::read(store.join(&name)).unwrap());
            h.finish()
        } else {
            0
        };
        out.insert(name, h);
    }
    out
}

/// The store paths the jobs' derivations need: their `.drv` files and
/// everything those reference, by base name.
fn closure(store: &Store, run: &Run) -> BTreeSet<String> {
    let drvs: Vec<&str> = run
        .lines
        .values()
        .filter_map(|v| v["drvPath"].as_str())
        .collect();
    let mut out = BTreeSet::new();
    // a command line holds a few thousand paths
    for chunk in drvs.chunks(2000) {
        let mut args = vec!["--query", "--requisites"];
        args.extend(chunk);
        for p in store.nix_store(&args).lines() {
            out.insert(p.rsplit('/').next().unwrap_or(p).to_string());
        }
    }
    out
}

#[rstest]
#[ignore = "instantiates the nixpkgs job set twice; set IETS_TEST_NIXPKGS"]
fn jobset_drvs_match_nix_eval_jobs() {
    let Some(s) = setup() else { return };
    // a private daemon with no substituter: instantiation writes `.drv`
    // files and copied sources, and nothing is built or fetched
    let Some(store) = Store::start_with(&[]) else {
        eprintln!("skipped: no nix-daemon on PATH");
        return;
    };
    let theirs = run(store.nix(&s.nej).args(s.args(&[])));
    let nix_paths = snapshot(&store.store_dir());
    let nix_closure = closure(&store, &theirs);
    // nothing roots what nix-eval-jobs wrote, so a collection empties the
    // store and iets writes every path again
    store.nix_store(&["--gc"]);
    let left = snapshot(&store.store_dir());
    assert!(
        !left.keys().any(|p| p.ends_with(".drv")),
        "the collection left {} paths",
        left.len()
    );
    let ours = run(store.iets().arg("eval-jobs").args(s.args(&[])));
    let iets_paths = snapshot(&store.store_dir());
    let iets_closure = closure(&store, &ours);

    let mut d = differences(("nix-eval-jobs", "iets"), &theirs, &ours, against_nix);
    let first = |a: &BTreeSet<String>, b: &BTreeSet<String>| -> Vec<String> {
        a.difference(b).take(5).cloned().collect()
    };
    if nix_closure != iets_closure {
        d.push(format!(
            "the jobs' closures differ: nix-eval-jobs {} paths, iets {}; only nix-eval-jobs: {:?}; only iets: {:?}",
            nix_closure.len(),
            iets_closure.len(),
            first(&nix_closure, &iets_closure),
            first(&iets_closure, &nix_closure),
        ));
    }
    let changed: Vec<&String> = nix_closure
        .iter()
        .filter(|p| nix_paths.get(*p) != iets_paths.get(*p))
        .take(5)
        .collect();
    if !changed.is_empty() {
        d.push(format!(".drv bytes differ: {changed:?}"));
    }
    // nix-eval-jobs writes the `.drv` of every derivation the evaluation
    // makes, a failed job's included; iets writes the jobs' closures
    // (docs/reference/commands.md, "Instantiation"). What lies outside them
    // is reported, and is no difference in what a job builds.
    let outside = |paths: &BTreeMap<String, u64>, c: &BTreeSet<String>| {
        paths.keys().filter(|p| !c.contains(*p)).count()
    };
    eprintln!(
        "outside the jobs' closures: nix-eval-jobs wrote {} paths, iets {}",
        outside(&nix_paths, &nix_closure),
        outside(&iets_paths, &iets_closure)
    );
    assert_agree("instantiation against nix-eval-jobs", d, &theirs);
}
