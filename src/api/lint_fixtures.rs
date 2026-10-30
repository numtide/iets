//! One fixture per lint in `tests/lint/<name>.nix`, evaluated in this
//! process. At the default level a fixture is quiet, at `warn` its stderr
//! is `<name>.warn.exp`, and at `error` it fails with `error: [<name>]`.

use crate::api::testkit::eval;
use crate::api::{EvalArgs, Input};
use crate::lint::*;
use crate::sys;
use std::fs;
use std::path::{Path, PathBuf};

fn lint_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/lint")
}

/// The fixture's own `--lint` and `--impure` flags, then `extra`.
fn args(name: &str, extra: Option<(&str, Level)>) -> EvalArgs {
    let dir = lint_dir();
    let mut a = EvalArgs::new();
    a.input = Some(Input::File(
        dir.join(format!("{name}.nix")).display().to_string(),
    ));
    a.strict = true;
    a.no_tracing_cache = true;
    a.no_ir_cache = true;
    if let Ok(flags) = fs::read_to_string(dir.join(format!("{name}.flags"))) {
        let mut it = flags.split_whitespace();
        while let Some(f) = it.next() {
            match f {
                "--impure" => a.pure = Some(false),
                "--lint" => {
                    let spec = it.next().expect("--lint takes NAME=LEVEL");
                    let (n, l) = spec.split_once('=').expect("NAME=LEVEL");
                    a.lints.push((n.into(), Level::parse(l).expect("a level")));
                }
                _ => panic!("{name}.flags: unknown flag {f}"),
            }
        }
    }
    a.lints.extend(extra.map(|(n, l)| (n.to_string(), l)));
    a
}

fn fixtures() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(lint_dir())
        .unwrap()
        .filter_map(|e| {
            let f = e.ok()?.file_name().into_string().ok()?;
            f.strip_suffix(".nix").map(String::from)
        })
        .collect();
    names.sort();
    names
}

/// What is wrong with one fixture, if anything.
fn check(name: &str) -> Option<String> {
    let dir = lint_dir();
    let (_, _, quiet) = eval(args(name, None));
    let own = format!("warning: [{name}]");
    if quiet
        .lines()
        .any(|l| l.starts_with("warning: [") && !l.starts_with(&own))
    {
        return Some("fires at the default level".into());
    }
    let (_, _, warn) = eval(args(name, Some((name, Level::Warn))));
    if !warn.contains(&own) {
        return Some("does not fire at warn".into());
    }
    let exp = fs::read_to_string(dir.join(format!("{name}.warn.exp"))).unwrap_or_default();
    let pwd = String::from_utf8_lossy(&sys::path_to_internal(&dir)).into_owned();
    let exp = exp.replace("/pwd/lint", &pwd);
    if warn.trim_end() != exp.trim_end() {
        return Some(format!("stderr differs from {name}.warn.exp:\n{warn}"));
    }
    let (_, rc, err) = eval(args(name, Some((name, Level::Error))));
    let first = err.lines().next().unwrap_or_default();
    if rc != 1 || !first.starts_with(&format!("error: [{name}]")) {
        return Some(format!("error run: rc={rc}, first line {first:?}"));
    }
    None
}

#[test]
fn every_lint_fires_on_its_fixture_only_when_asked() {
    // git refuses to track a `.git` entry, so the fixture gets one at run time
    let git = lint_dir().join("git-in-source.d/.git");
    fs::create_dir_all(&git).unwrap();
    let bad: Vec<String> = fixtures()
        .iter()
        .filter_map(|n| Some(format!("{n}: {}", check(n)?)))
        .collect();
    let _ = fs::remove_dir_all(&git);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
