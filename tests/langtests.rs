//! tests/lang/eval-okay-*.nix and eval-fail-*.nix, run from tests/ like Nix's
//! own harness. `--impure` because the corpus is Nix's: cases read
//! `/proc/version`, use `storePath` and the search path.

mod hermetic;

use iets::sys::path_to_internal;
use iets::sys::process::signal;
use rstest::rstest;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
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
    let pwd = String::from_utf8_lossy(&path_to_internal(&tests_dir())).into_owned();
    let got = String::from_utf8_lossy(&out.stdout).replace(&pwd, "/pwd");
    let (got, want) = (chomp(&got), chomp(&want));
    (got != want).then(|| {
        let at = got.lines().zip(want.lines()).find(|(g, w)| g != w);
        match at {
            Some((g, w)) => format!("output differs: got {g:?}, want {w:?}"),
            None => format!(
                "output differs in length: got {} lines, want {}; stderr: {}",
                got.lines().count(),
                want.lines().count(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    })
}

// a new case shows up after `touch tests/langtests.rs`
#[rstest]
fn lang(
    #[files("tests/lang/eval-okay-*.nix")]
    #[files("tests/lang/eval-fail-*.nix")]
    path: PathBuf,
) {
    let stem = path.file_stem().unwrap().to_str().unwrap();
    if let Some(why) = check(stem) {
        panic!("{stem}: {why}");
    }
}
