//! The lint cases of `iets eval` and `iets lint` that need the command
//! line: usage errors, `lint list`, finding records and cache replay.

use crate::cli::run::{Host, run};
use crate::sys;
use std::path::{Path, PathBuf};
use std::{env, fs};
use tempfile::{TempDir, tempdir};

const URL_WARN: &str = "warning: [url-literal] URL literals are discouraged. Consider using a string literal \"http://example.com\" instead";

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/lint")
}

fn file(f: &str) -> String {
    dir().join(f).display().to_string()
}

/// A cache directory, removed with the `TempDir`.
fn cache() -> (TempDir, String) {
    let d = tempdir().unwrap();
    let path = d.path().display().to_string();
    (d, path)
}

#[test]
fn lint_list_shows_the_effective_levels_and_the_scope() {
    let r = run(
        &["lint", "list", "--lint", "url-literal=error"],
        &Host::default(),
    );
    let line = r
        .stdout
        .lines()
        .find(|l| l.starts_with("url-literal "))
        .unwrap_or_default();
    assert!(line.contains("error"), "{}", r.stdout);
    let r = run(
        &[
            "lint",
            "list",
            "--lint-scope",
            "all",
            &file("scope/root/main.nix"),
        ],
        &Host::default(),
    );
    assert!(r.stdout.contains("scope: all"), "{}", r.stdout);
}

#[test]
fn an_unknown_lint_name_in_a_flag_is_a_usage_error() {
    let r = run(
        &[
            "eval",
            "--no-tracing-cache",
            "--lint",
            "nope=warn",
            &file("url-literal.nix"),
        ],
        &Host::default(),
    );
    assert_eq!(r.rc, 2, "{}", r.stderr);
    assert!(r.stderr.contains("unknown lint 'nope'"), "{}", r.stderr);
}

#[test]
fn an_unknown_name_in_iets_lint_is_a_record() {
    let host = Host::default().env([("IETS_LINT", "nope=warn,url-literal=warn")]);
    let r = run(
        &[
            "eval",
            "--no-tracing-cache",
            "--log-format",
            "internal-json",
            &file("url-literal.nix"),
        ],
        &host,
    );
    assert_eq!(r.rc, 0, "{}", r.stderr);
    assert!(
        r.stderr.contains("\"code\":\"unknown-lint\""),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("IETS_LINT: iets does not know the lint 'nope'"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_warning_is_replayed_on_a_cache_hit() {
    let (_c, c) = cache();
    let args = [
        "eval",
        "--strict",
        "--timings",
        "--cache-dir",
        &c,
        "--lint",
        "url-literal=warn",
        &file("url-literal.nix"),
    ];
    let (r1, r2) = (run(&args, &Host::default()), run(&args, &Host::default()));
    let warns = |e: &str| {
        e.lines()
            .filter(|l| l.starts_with("warning:"))
            .map(String::from)
            .collect::<Vec<_>>()
    };
    assert!(r2.stderr.contains("cache: hit"), "{}", r2.stderr);
    assert!(r1.stderr.contains(URL_WARN), "{}", r1.stderr);
    assert_eq!(warns(&r1.stderr), warns(&r2.stderr));
}

#[test]
fn a_finding_record_has_the_span_and_the_edit_on_a_cache_hit_too() {
    let (_c, c) = cache();
    let args = [
        "eval",
        "--strict",
        "--timings",
        "--cache-dir",
        &c,
        "--log-format",
        "internal-json",
        "--lint",
        "url-literal=warn",
        &file("url-literal.nix"),
    ];
    let (r1, r2) = (run(&args, &Host::default()), run(&args, &Host::default()));
    let has = |e: &str| {
        e.contains("\"end\":18,")
            && e.contains("\"replacement\":\"\\\"http://example.com\\\"\",\"sid\":")
            && e.contains("\"start\":0}")
    };
    assert!(has(&r1.stderr), "{}", r1.stderr);
    assert!(
        has(&r2.stderr) && r2.stderr.contains("cache: hit"),
        "{}",
        r2.stderr
    );
}

#[test]
fn a_finding_at_error_has_the_span_and_no_edit() {
    let r = run(
        &[
            "eval",
            "--no-tracing-cache",
            "--log-format",
            "internal-json",
            "--lint",
            "eq-type-mismatch=error",
            &file("cases/tryeval.nix"),
        ],
        &Host::default(),
    );
    assert_eq!(r.rc, 1, "{}", r.stderr);
    assert!(
        r.stderr.contains("\"end\":27,") && r.stderr.contains("\"start\":19}"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("replacement"), "{}", r.stderr);
}

#[test]
fn the_scope_is_part_of_the_cache_key() {
    let (_c, c) = cache();
    let f = file("scope/root/main.nix");
    let r1 = run(
        &[
            "eval",
            "--timings",
            "--cache-dir",
            &c,
            "--lint",
            "url-literal=warn",
            &f,
        ],
        &Host::default(),
    );
    let r2 = run(
        &[
            "eval",
            "--timings",
            "--cache-dir",
            &c,
            "--lint",
            "url-literal=warn",
            "--lint-scope",
            "all",
            &f,
        ],
        &Host::default(),
    );
    assert!(!r1.stderr.contains("warning: ["), "{}", r1.stderr);
    assert!(!r2.stderr.contains("cache: hit"), "{}", r2.stderr);
    assert!(r2.stderr.contains("scope/lib/url.nix:1:1"), "{}", r2.stderr);
}

#[test]
fn an_allow_comment_is_part_of_the_cache_key_and_a_hit_replays_nothing() {
    let (_c, c) = cache();
    let (_work, work) = cache();
    let work = PathBuf::from(work);
    fs::create_dir_all(&work).unwrap();
    let main = work.join("main.nix").display().to_string();
    fs::copy(file("allow/own-line.nix"), &main).unwrap();
    let go = || {
        run(
            &[
                "eval",
                "--strict",
                "--timings",
                "--cache-dir",
                &c,
                "--lint",
                "url-literal=warn",
                &main,
            ],
            &Host::default(),
        )
    };
    let (r1, r2) = (go(), go());
    fs::write(&main, "http://example.com\n").unwrap();
    let r3 = go();
    assert!(r2.stderr.contains("cache: hit"), "{}", r2.stderr);
    assert!(!r1.stderr.contains("warning: [") && !r2.stderr.contains("warning: ["));
    assert!(
        !r3.stderr.contains("cache: hit") && r3.stderr.contains(URL_WARN),
        "{}",
        r3.stderr
    );
}

#[test]
fn an_allow_table_travels_in_the_ir_cache_entry_and_a_load_stays_silent() {
    for f in ["own-line", "runtime"] {
        let (_c, c) = cache();
        let host = Host::default().env([("IETS_IR_MIN", "0"), ("IETS_IR_STATS", "1")]);
        let path = file(&format!("allow/{f}.nix"));
        let shown = String::from_utf8_lossy(&sys::path_to_internal(Path::new(&path))).into_owned();
        let go = || {
            run(
                &[
                    "eval",
                    "--strict",
                    "--no-tracing-cache",
                    "--cache-dir",
                    &c,
                    "--lint",
                    "url-literal=warn",
                    "--lint",
                    "eq-type-mismatch=warn",
                    &path,
                ],
                &host,
            )
        };
        let (r1, r2) = (go(), go());
        assert!(
            r1.stderr.contains(&format!("ir-cache store {shown}")),
            "{f}: {}",
            r1.stderr
        );
        assert!(
            r2.stderr.contains(&format!("ir-cache load {shown}")),
            "{f}: {}",
            r2.stderr
        );
        assert!(
            !r1.stderr.contains("warning: [") && !r2.stderr.contains("warning: ["),
            "{f}: {}",
            r2.stderr
        );
    }
}
