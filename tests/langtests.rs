//! tests/lang/eval-okay-*.nix and eval-fail-*.nix, run from tests/ like Nix's
//! own harness. `--impure` because the corpus is Nix's: cases read
//! `/proc/version`, use `storePath` and the search path.

mod hermetic;

use iets::sys::process::signal;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::{fs, thread};

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn expected_failures() -> Vec<String> {
    let text =
        fs::read_to_string(tests_dir().join("lang/expected-failures.txt")).unwrap_or_default();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn cases(prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(tests_dir().join("lang"))
        .expect("read tests/lang")
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".nix")?;
            stem.starts_with(prefix).then(|| stem.to_string())
        })
        .collect();
    names.sort();
    names
}

fn flags_of(stem: &str) -> Vec<String> {
    fs::read_to_string(tests_dir().join(format!("lang/{stem}.flags")))
        .map(|t| {
            let line = t.lines().next().unwrap_or("");
            line.split_whitespace().map(String::from).collect()
        })
        .unwrap_or_default()
}

fn eval(stem: &str) -> Output {
    hermetic::iets()
        .current_dir(tests_dir())
        .env("HOME", "/fake-home")
        .env("TEST_VAR", "foo")
        .env("NIX_PATH", "lang/dir3:lang/dir4")
        .args(["eval", "--impure", "--strict"])
        .args(flags_of(stem))
        .arg(format!("lang/{stem}.nix"))
        .output()
        .expect("run iets")
}

fn chomp(s: &str) -> &str {
    s.trim_end_matches('\n')
}

fn check(stem: &str) -> Option<String> {
    let out = eval(stem);
    if stem.starts_with("eval-fail-") {
        return match (out.status.code(), signal(&out.status)) {
            (Some(0), _) => Some("should fail".into()),
            (None, Some(sig)) => Some(format!("crashed, signal {sig}")),
            _ => None,
        };
    }
    let want = fs::read_to_string(tests_dir().join(format!("lang/{stem}.exp"))).ok()?;
    let pwd = tests_dir().display().to_string();
    let got = String::from_utf8_lossy(&out.stdout).replace(&pwd, "/pwd");
    (chomp(&got) != chomp(&want)).then(|| "output differs".into())
}

fn failures() -> Vec<(String, String)> {
    let stems: Vec<String> = [cases("eval-okay-"), cases("eval-fail-")].concat();
    let workers = thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let chunk = stems.len().div_ceil(workers).max(1);
    let mut failed: Vec<(String, String)> = thread::scope(|s| {
        let handles: Vec<_> = stems
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .filter_map(|n| Some((n.clone(), check(n)?)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("a worker panicked"))
            .collect()
    });
    failed.sort();
    failed
}

#[test]
fn the_language_corpus_passes_but_for_the_expected_failures() {
    let failed = failures();
    let expected = expected_failures();
    let new: Vec<String> = failed
        .iter()
        .filter(|(n, _)| !expected.contains(n))
        .map(|(n, why)| format!("{n} ({why})"))
        .collect();
    let fixed: Vec<&String> = expected
        .iter()
        .filter(|n| !failed.iter().any(|(f, _)| f == *n))
        .collect();
    assert!(new.is_empty(), "unexpected failures: {}", new.join(", "));
    assert!(
        fixed.is_empty(),
        "now passing, drop from expected-failures.txt: {fixed:?}"
    );
}
