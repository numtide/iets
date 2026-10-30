//! The lint configuration and scope, `# iets: allow`, and the `iets.toml`
//! files of `tests/lint/`, evaluated in this process.

use crate::api::testkit::eval;
use crate::api::{EvalArgs, Input};
use crate::env::Env;
use crate::lint::*;
use crate::sys;
use std::path::{Path, PathBuf};

const URL_WARN: &str = "warning: [url-literal] URL literals are discouraged. Consider using a string literal \"http://example.com\" instead";

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/lint")
}

struct Out {
    rc: i32,
    out: String,
    err: String,
}

/// `file` under `tests/lint`, lints given as `name=level` or `family:x=level`.
fn run(file: &str, strict: bool, flags: &[&str]) -> Out {
    run_with(file, strict, &Env::isolated(), flags)
}

fn run_in(file: &str, env: &Env, flags: &[&str]) -> Out {
    run_with(file, false, env, flags)
}

fn run_with(file: &str, strict: bool, env: &Env, flags: &[&str]) -> Out {
    let mut a = EvalArgs::new();
    a.env = env.clone();
    a.input = Some(Input::File(dir().join(file).display().to_string()));
    a.strict = strict;
    a.no_tracing_cache = true;
    a.no_ir_cache = true;
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        match *f {
            "--lint" => {
                let (n, l) = it.next().unwrap().split_once('=').unwrap();
                a.lints.push((n.into(), Level::parse(l).unwrap()));
            }
            "--lint-scope" => a.lint_scope = Some(Scope::parse(it.next().unwrap()).unwrap()),
            "--lint-root" => a
                .lint_roots
                .push(dir().join(it.next().unwrap()).display().to_string()),
            other => panic!("unknown flag {other}"),
        }
    }
    let (out, rc, err) = eval(a);
    let pwd = String::from_utf8_lossy(&sys::path_to_internal(&dir())).into_owned();
    let err = err.replace(&pwd, "/pwd/lint");
    Out { rc, out, err }
}

fn count(err: &str, prefix: &str) -> usize {
    err.lines().filter(|l| l.starts_with(prefix)).count()
}

#[test]
fn a_later_flag_wins_over_all_and_a_name_beats_a_later_all() {
    let r = run(
        "url-literal.nix",
        false,
        &["--lint", "all=warn", "--lint", "url-literal=off"],
    );
    assert!(!r.err.contains("[url-literal]"), "{}", r.err);
    let r = run(
        "url-literal.nix",
        false,
        &["--lint", "url-literal=off", "--lint", "all=warn"],
    );
    assert!(!r.err.contains("[url-literal]"), "{}", r.err);
}

#[test]
fn a_family_selector_selects_its_members() {
    let r = run("url-literal.nix", false, &["--lint", "family:legacy=warn"]);
    assert!(r.err.contains(URL_WARN), "{}", r.err);
}

#[test]
fn the_project_iets_toml_sets_a_level_and_a_name_beats_its_family() {
    let r = run("cases/toml/main.nix", false, &[]);
    assert!(r.err.contains(URL_WARN), "{}", r.err);
    let r = run("cases/family/main.nix", false, &[]);
    assert_eq!(r.rc, 0, "{}", r.err);
    assert!(
        r.err.contains("warning: [absolute-path-literal]"),
        "{}",
        r.err
    );
    let r = run("cases/family/member.nix", false, &[]);
    assert_eq!(r.rc, 1, "{}", r.err);
    assert!(r.err.contains("error: [short-path-literal]"), "{}", r.err);
}

#[test]
fn a_project_file_may_not_set_nix_bin_dir() {
    let r = run("cases/untrusted/main.nix", false, &[]);
    assert_ne!(r.rc, 0);
    assert!(
        r.err
            .contains("'nix-bin-dir' may not be set in a project file"),
        "{}",
        r.err
    );
}

#[test]
fn an_unknown_name_in_iets_toml_is_a_warning_and_the_rest_counts() {
    let r = run("cases/unknown/main.nix", false, &[]);
    assert_eq!(r.rc, 0, "{}", r.err);
    assert!(
        r.err
            .contains("iets.toml: iets does not know the lint 'with' and ignores it"),
        "{}",
        r.err
    );
    assert!(
        r.err.contains("does not know the lint 'family:scope'"),
        "{}",
        r.err
    );
    assert!(r.err.contains(URL_WARN), "{}", r.err);
}

#[test]
fn one_warning_per_site() {
    let r = run(
        "cases/dedup.nix",
        true,
        &["--lint", "eq-type-mismatch=warn"],
    );
    assert_eq!(count(&r.err, "warning: [eq-type-mismatch]"), 1, "{}", r.err);
}

#[test]
fn try_eval_does_not_catch_a_lint_error() {
    let r = run(
        "cases/tryeval.nix",
        false,
        &["--lint", "eq-type-mismatch=error"],
    );
    assert_eq!(r.rc, 1, "{}", r.err);
    assert!(r.err.contains("error: [eq-type-mismatch]"), "{}", r.err);
}

#[test]
fn scope_limits_a_lint_to_the_entry_directory_until_it_is_opened() {
    let outside = "at /pwd/lint/scope/lib/url.nix:1:1";
    let warn = ["--lint", "url-literal=warn"];
    let r = run("scope/root/main.nix", false, &warn);
    assert!(
        !r.err.contains("warning:"),
        "outside site reported: {}",
        r.err
    );
    let r = run(
        "scope/root/main.nix",
        false,
        &[warn[0], warn[1], "--lint-scope", "all"],
    );
    assert!(r.err.contains(outside), "--lint-scope all: {}", r.err);
    let r = run(
        "scope/root/main.nix",
        false,
        &[warn[0], warn[1], "--lint-root", "scope/lib"],
    );
    assert!(r.err.contains(outside), "--lint-root: {}", r.err);
    let r = run("scope/all/main.nix", false, &warn);
    assert!(r.err.contains(outside), "toml scope = all: {}", r.err);
    let r = run("scope/roots/main.nix", false, &warn);
    assert!(r.err.contains(outside), "toml roots: {}", r.err);
}

#[test]
fn scope_attributes_a_derivation_to_the_users_binding() {
    let r = run("scope/root/drv.nix", false, &["--lint", "drv-list=warn"]);
    assert!(
        !r.err.contains("warning:"),
        "drv-list through a wrapper: {}",
        r.err
    );
    let r = run("scope/root/fod.nix", false, &["--lint", "fod=warn"]);
    assert_eq!(
        r.err.trim_end(),
        "warning: [fod] derivation 'wrapped' is fixed-output (recursive, sha256)\n  at /pwd/lint/scope/root/fod.nix:1:25"
    );
    let r = run("scope/root/own.nix", false, &["--lint", "drv-list=warn"]);
    assert!(
        !r.err.contains("warning:"),
        "the wrapper's own derivation: {}",
        r.err
    );
}

#[test]
fn allow_comments_silence_the_lines_they_cover() {
    let both = [
        "--lint",
        "url-literal=warn",
        "--lint",
        "eq-type-mismatch=warn",
    ];
    for f in [
        "own-line",
        "trailing",
        "multiple",
        "family",
        "all",
        "blank-lines",
        "runtime",
    ] {
        let r = run(&format!("allow/{f}.nix"), true, &both);
        assert!(
            r.rc == 0 && !r.err.contains("warning:"),
            "{f}: rc={} {}",
            r.rc,
            r.err
        );
    }
    let r = run(
        "allow/next-line-only.nix",
        true,
        &["--lint", "url-literal=warn"],
    );
    assert!(
        r.err.contains("at /pwd/lint/allow/next-line-only.nix:3:3"),
        "{}",
        r.err
    );
    let r = run(
        "allow/error-level.nix",
        false,
        &["--lint", "url-literal=error"],
    );
    assert!(r.rc == 0 && r.out.trim() == "18", "rc={} {}", r.rc, r.err);
    let r = run(
        "allow/directive.nix",
        false,
        &["--lint", "url-literal=warn"],
    );
    assert!(
        r.err
            .contains("warning: [unknown-allow] unknown directive 'allo'"),
        "{}",
        r.err
    );
    assert!(r.err.contains("warning: [url-literal]"), "{}", r.err);
}

#[test]
fn iets_lint_sets_a_level_and_a_flag_beats_it() {
    let env = Env::isolated().with([("IETS_LINT", "url-literal=warn")]);
    let r = run_in("url-literal.nix", &env, &[]);
    assert!(r.err.contains(URL_WARN), "{}", r.err);
    let r = run_in("url-literal.nix", &env, &["--lint", "url-literal=off"]);
    assert!(!r.err.contains("warning:"), "{}", r.err);
    let env = Env::isolated().with([("IETS_LINT", "url-literal=off,family:legacy=warn")]);
    let r = run_in("url-literal.nix", &env, &[]);
    assert!(
        !r.err.contains("[url-literal]"),
        "a name beats a later family: {}",
        r.err
    );
}

#[test]
fn the_user_iets_toml_sets_a_level_and_the_project_file_wins_over_it() {
    let user = Env::isolated().with([("XDG_CONFIG_HOME", dir().join("user"))]);
    let r = run_in("url-literal.nix", &user, &[]);
    assert_eq!(r.rc, 1, "{}", r.err);
    assert!(r.err.contains("error: [url-literal]"), "{}", r.err);
    let r = run_in("cases/toml/main.nix", &user, &[]);
    assert_eq!(r.rc, 0, "{}", r.err);
    assert!(r.err.contains(URL_WARN), "user below project: {}", r.err);
}

#[test]
fn an_unknown_name_in_iets_lint_is_a_warning_and_the_rest_counts() {
    let env = Env::isolated().with([("IETS_LINT", "nope=warn,url-literal=warn")]);
    let r = run_in("url-literal.nix", &env, &[]);
    assert_eq!(r.rc, 0, "{}", r.err);
    assert!(
        r.err
            .contains("IETS_LINT: iets does not know the lint 'nope'"),
        "{}",
        r.err
    );
    assert!(r.err.contains("[url-literal]"), "{}", r.err);
}
