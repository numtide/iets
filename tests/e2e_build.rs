//! `iets build` over an attribute tree, against the fixture's daemon. The
//! flake has `checks` for two systems, `ok`, `dep`, `chain` (which reads
//! `dep`) and `bad` (which exits 1), and a `packages` set.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::fs;
use std::process::Output;

use e2e::{OTHER, Store, checks_flake, parse as json, stderr, stdout};

fn build(s: &Store, args: &[&str]) -> Output {
    s.iets()
        .args(["build", "--builder", "daemon"])
        .args(args)
        .output()
        .unwrap()
}

/// `iets --iets-option NAME VALUE build ...`
fn build_with(s: &Store, opt: (&str, &str), args: &[&str]) -> Output {
    s.iets()
        .args([
            "--iets-option",
            opt.0,
            opt.1,
            "build",
            "--builder",
            "daemon",
        ])
        .args(args)
        .output()
        .unwrap()
}

fn names(s: &Store, paths: &str) -> Vec<String> {
    let prefix = format!("{}/", s.store_dir().display());
    paths
        .lines()
        .map(|p| {
            let base = p.strip_prefix(&prefix).unwrap_or_else(|| panic!("{p}"));
            base.split_once('-').unwrap().1.to_string()
        })
        .collect()
}

#[test]
fn a_tree_builds_past_a_failure() {
    let s = store!();
    let flake = checks_flake(&s, "flake");
    let checks = format!("{flake}#checks");
    let sys = e2e::system();

    // the failure does not stop the others, and the exit code is Nix's 100
    let r1 = build(&s, &[&checks]);
    let err = stderr(&r1);
    assert_eq!(r1.status.code(), Some(100), "{err}");
    assert!(err.contains(&format!("checks.{sys}.bad failed")), "{err}");
    assert!(err.contains("about to fail"), "the log tail: {err}");
    assert!(err.contains("building fb-chain"), "{err}");
    assert!(
        err.contains(&format!("3 built, 1 failed: checks.{sys}.bad")),
        "{err}"
    );
    // in the order the roots were built
    let paths = stdout(&r1);
    let mut built = names(&s, &paths);
    built.sort();
    assert_eq!(built, ["fb-chain", "fb-dep", "fb-ok"]);
    assert!(paths.lines().all(|p| fs::exists(p).unwrap()));
    assert!(!err.contains(OTHER), "the other system is not built: {err}");

    // --json: the `plumbing build` shape, one outcome per check
    let r2 = build(&s, &["--json", &checks]);
    assert_eq!(r2.status.code(), Some(100));
    let j = json(&r2);
    let outcomes = j["outcomes"].as_array().unwrap();
    let attrs: Vec<&str> = outcomes
        .iter()
        .map(|o| o["attr"].as_str().unwrap())
        .collect();
    let want: Vec<String> = ["bad", "chain", "dep", "ok"]
        .iter()
        .map(|n| format!("checks.{sys}.{n}"))
        .collect();
    assert_eq!(attrs, want);
    let bad = &outcomes[0];
    let mut keys: Vec<&String> = bad.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["attr", "drv", "logTail", "outputs", "reason", "success"]
    );
    assert_eq!(bad["success"], false);
    assert_eq!(bad["outputs"], serde_json::json!({}));
    assert!(
        bad["reason"].as_str().unwrap().contains("fb-bad.drv"),
        "{bad}"
    );
    let tail = bad["logTail"].as_array().unwrap();
    assert!(
        tail.iter()
            .any(|l| l.as_str().unwrap().contains("about to fail"))
    );
    for o in &outcomes[1..] {
        assert_eq!(o["success"], true, "{o}");
        assert!(paths.lines().any(|p| o["outputs"]["out"] == p), "{o}");
    }

    // what is built is not built again, the failure is
    let err = stderr(&r2);
    for n in ["fb-ok", "fb-dep", "fb-chain"] {
        assert!(!err.contains(&format!("building {n}")), "{err}");
    }
    assert!(err.contains("about to fail"), "{err}");

    // a plain file, -A and --argstr
    let rel = s.write(
        "release.nix",
        &format!(
            "{{ system ? builtins.currentSystem }}:\n\
             let f = import {}/flake/flake.nix; o = f.outputs {{ self = null; }};\n\
             in {{ ci = o.checks.${{system}}; }}\n",
            s.dir().display()
        ),
    );
    let rel = rel.to_str().unwrap();
    let r5 = build(&s, &[rel, "-A", "ci", "--argstr", "system", &sys, "-k"]);
    assert_eq!(r5.status.code(), Some(100));
    assert!(stderr(&r5).contains("ci.bad failed"), "{}", stderr(&r5));
    let out5 = stdout(&r5);
    let mut again: Vec<&str> = out5.lines().collect();
    let mut first: Vec<&str> = paths.lines().collect();
    again.sort();
    first.sort();
    assert_eq!(again, first, "the same paths as the flake gave");
}

#[test]
fn one_root() {
    let s = store!();
    let flake = checks_flake(&s, "flake");
    let sys = e2e::system();
    let r = build(&s, &[&format!("{flake}#checks.{sys}.bad")]);
    assert_eq!(r.status.code(), Some(100));
    assert_eq!(stdout(&r), "");
    assert!(
        !stderr(&r).contains(" built, "),
        "no closing line: {}",
        stderr(&r)
    );

    let p = build(&s, &[&format!("{flake}#packages")]);
    assert!(p.status.success(), "{}", stderr(&p));
    assert!(stdout(&p).ends_with("-fb-lone"), "{}", stdout(&p));

    // a system named in full is walked as it is
    let o = build(
        &s,
        &[&format!("{flake}#checks.{OTHER}"), "--dry-run", "--json"],
    );
    assert_eq!(json(&o)["willBuild"].as_array().unwrap().len(), 4);
}

#[test]
fn dry_run_over_a_tree() {
    let s = store!();
    let flake = checks_flake(&s, "fresh");
    let r = build(&s, &["--dry-run", "--json", &format!("{flake}#checks")]);
    assert!(r.status.success(), "{}", stderr(&r));
    assert_eq!(json(&r)["willBuild"].as_array().unwrap().len(), 4);
}

#[test]
fn no_verdict_without_an_evaluation() {
    let s = store!();
    checks_flake(&s, "fresh");
    // --impure: an -E expression's project is the working directory
    let expr = format!(
        "let f = import {}/fresh/flake.nix; in (f.outputs {{ self = null; }}).checks.{} // {{ broken = throw \"boom\"; }}",
        s.dir().display(),
        e2e::system()
    );
    let r = build(&s, &["--impure", "--no-tracing-cache", "-E", &expr]);
    assert_eq!(r.status.code(), Some(1), "{}", stderr(&r));
    assert_eq!(stdout(&r), "");
    assert!(stderr(&r).contains("boom"));
    assert!(!stderr(&r).contains("building fb-"), "nothing was built");

    let e = build(&s, &["-E", "{ a = { }; }"]);
    assert_eq!(e.status.code(), Some(1));
    assert!(stderr(&e).contains("nothing to build"), "{}", stderr(&e));
}

const DEPS: &str = r#"let
  salt = "@NONCE@";
  depFail = derivation { name = "be-depfail"; system = "@SYSTEM@"; builder = "@SHELL@"; args = [ "-c" "exit 1" ]; inherit salt; };
  depFod = derivation { name = "be-depfod"; system = "@SYSTEM@"; builder = "@SHELL@"; args = [ "-c" "echo $salt > $out" ]; inherit salt; outputHashMode = "flat"; outputHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; };
in {
  overFail = derivation { name = "be-over-fail"; system = "@SYSTEM@"; builder = "@SHELL@"; args = [ "-c" "echo x > $out" ]; dep = depFail; inherit salt; };
  overFod = derivation { name = "be-over-fod"; system = "@SYSTEM@"; builder = "@SHELL@"; args = [ "-c" "echo x > $out" ]; dep = depFod; inherit salt; };
}
"#;

/// Nix gives 100 for a dependency that failed to build and 102 for a
/// fixed-output one with a hash mismatch.
#[test]
fn a_failure_in_a_dependency() {
    let s = store!();
    let deps = s.write("deps.nix", DEPS);
    let deps = deps.to_str().unwrap();
    let a = build(&s, &[deps, "-A", "overFail"]);
    assert_eq!(a.status.code(), Some(100), "{}", stderr(&a));
    let b = build_with(&s, ("fix-hashes", "never"), &[deps, "-A", "overFod"]);
    assert_eq!(b.status.code(), Some(102), "{}", stderr(&b));
    assert!(stderr(&b).contains("be-depfod failed"), "{}", stderr(&b));
}

const FORGE: &str = r##"let good = "@GOOD@"; in {
  forge = derivation {
    name = "be-forge"; system = "@SYSTEM@"; builder = "@SHELL@";
    salt = "@NONCE@";
    args = [ "-c" ''
      printf '%s\n' "@nix {\"action\":\"msg\",\"level\":0,\"msg\":\"hash mismatch in fixed-output derivation 'x':\n  specified: ${good}\n     got:    sha256-HNpsZojn8xegmQzw+pTz75/kDloh2nUWpW342gddnw0=\"}" >&2
      exit 1
    '' ];
  };
}
"##;

/// The derivation is not fixed-output, so the text is only text.
#[test]
fn a_build_log_cannot_forge_a_hash_mismatch() {
    let s = store!();
    let good = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
    let forge = s.write("forge.nix", &FORGE.replace("@GOOD@", good));
    let r = build_with(
        &s,
        ("fix-hashes", "always"),
        &[forge.to_str().unwrap(), "-A", "forge"],
    );
    assert!(fs::read_to_string(&forge).unwrap().contains(good));
    assert!(!stderr(&r).contains("fixed: "), "{}", stderr(&r));
}

#[test]
fn no_build_uploads_every_installable() {
    let s = store!();
    let flake = checks_flake(&s, "other");
    let sys = e2e::system();
    let r = build(
        &s,
        &[
            "--no-build",
            "--timings",
            &format!("{flake}#checks.{sys}.ok"),
            &format!("{flake}#checks.{sys}.dep"),
        ],
    );
    assert!(r.status.success(), "{}", stderr(&r));
    assert!(stderr(&r).contains("query: 2 paths"), "{}", stderr(&r));
}
