//! The native command line: help and version on stdout with exit 0, a bad
//! command line on stderr with exit 2 and never the whole usage.

mod hermetic;

use hermetic::{Iets, bin, stderr, stdout};

use rstest::rstest;
use std::path::Path;
use std::process::{Command, Output};
use std::{env, fs};
use tempfile::tempdir;

fn run(bin: &Iets, args: &[&str]) -> Output {
    bin.cmd().args(args).output().expect("run iets")
}

fn run_complete(mut c: Command, words: &[&str]) -> String {
    let o = c.arg("__complete").args(words).output().expect("run iets");
    assert_eq!(o.status.code(), Some(0), "{words:?}: {}", stderr(&o));
    assert!(o.stderr.is_empty(), "{words:?}: {}", stderr(&o));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[rstest]
fn help_and_version_go_to_stdout(bin: Iets) {
    for args in [
        &["--help"][..],
        &["-h"],
        &["help"],
        &["help", "eval"],
        &["eval", "--help"],
        &["build", "-h"],
        &["cache", "gc", "--help"],
        &["lint", "--help"],
        &["version"],
        &["--version"],
    ] {
        let o = run(&bin, args);
        assert_eq!(o.status.code(), Some(0), "{args:?}");
        assert!(o.stderr.is_empty(), "{args:?}: {}", stderr(&o));
        assert!(!o.stdout.is_empty(), "{args:?}");
    }
    let o = run(&bin, &["--help"]);
    assert!(stdout(&o).contains("  eval-jobs "));
    let o = run(&bin, &["eval", "--help"]);
    assert!(stdout(&o).starts_with("usage: iets eval "));
    assert!(stdout(&o).contains("--strict"));
    let v = stdout(&run(&bin, &["--version"]));
    assert_eq!(v.trim_end(), format!("iets {}", env!("CARGO_PKG_VERSION")));
}

#[rstest]
#[case::unsupported(
    "import (derivation { name = \"d\"; system = \"x\"; builder = \"b\"; })",
    3
)]
#[case::other("throw \"x\"", 1)]
fn an_unsupported_construct_has_its_own_exit_code(
    bin: Iets,
    #[case] expr: &str,
    #[case] code: i32,
) {
    let o = run(&bin, &["eval", "-E", expr]);
    assert_eq!(o.status.code(), Some(code), "{}", stderr(&o));
}

#[rstest]
#[case::unknown(&["bogus"], "error: unknown command 'bogus'\n")]
#[case::typo(&["buidl"], "error: unknown command 'buidl'\ndid you mean 'build'?\n")]
#[case::typo_with_args(&["serach", "^"], "did you mean 'search'?")]
#[case::classic_typo(&["nix-instantate", "--eval", "-E", "1"], "did you mean 'nix-instantiate'?")]
#[case::flag_typo(
    &["eval", "--stirct", "-E", "1"],
    "error: unknown flag '--stirct'\ndid you mean '--strict'?\nrun 'iets eval --help' for the flags\n"
)]
#[case::zero_workers(&["eval", "--workers", "0", "-E", "1"], "--workers wants a number >= 1, got '0'")]
#[case::no_expr(&["eval", "-E"], "error: -E needs an expression\n")]
#[case::expr_and_file(&["eval", "-E", "1", "x.nix"], "-E and FILE cannot be combined")]
#[case::build_nothing(&["build"], "nothing to evaluate")]
#[case::build_flag(&["build", "--what"], "unknown flag '--what'")]
#[case::subcommand_typo(&["cache", "dunp"], "did you mean 'dump'?")]
fn a_bad_command_line_names_the_problem(bin: Iets, #[case] args: &[&str], #[case] want: &str) {
    let o = run(&bin, args);
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(o.stdout.is_empty());
    assert!(err.contains(want), "{err}");
    assert!(
        !err.contains("usage:") && !err.contains("falling back"),
        "{err}"
    );
}

/// What the system says for a path that is not there.
const NO_SUCH_FILE: &str = if cfg!(windows) {
    "The system cannot find the path specified."
} else {
    "No such file or directory"
};

/// `-f FILE` is the positional FILE in the native namespace; under a classic
/// name the argv stays Nix's, where `nix-eval-jobs` has no `-f`.
#[rstest]
fn dash_f_names_the_file_in_the_native_namespace(bin: Iets) {
    for args in [
        &["eval", "-f", "/nonexistent/missing.nix"][..],
        &["eval", "--file", "/nonexistent/missing.nix"],
        &["build", "-f", "/nonexistent/missing.nix"],
        &["prefetch", "-f", "/nonexistent/missing.nix"],
        &["eval-jobs", "-f", "/nonexistent/missing.nix"],
    ] {
        let err = stderr(&run(&bin, args));
        assert!(err.contains(NO_SUCH_FILE), "{args:?}: {err}");
    }
    let o = run(&bin, &["nix-eval-jobs", "-f", "/nonexistent/missing.nix"]);
    let err = stderr(&o);
    assert!(err.contains("unknown flag '-f'"), "{err}");
    assert_eq!(o.status.code(), Some(1), "{err}");
}

#[rstest]
fn io_errors_read_like_strerror(bin: Iets) {
    let o = run(&bin, &["eval", "/nonexistent/missing.nix"]);
    let err = stderr(&o);
    assert!(err.contains(NO_SUCH_FILE), "{err}");
    assert!(!err.contains("os error"), "{err}");
}

#[rstest]
fn completion_asks_the_binary_and_ignores_settings(bin: Iets) {
    let conf = tempdir().unwrap();
    fs::create_dir_all(conf.path().join("iets")).unwrap();
    fs::write(conf.path().join("iets/iets.toml"), "this is = not [valid\n").unwrap();
    let complete = |words: &[&str]| {
        let mut c = bin.cmd();
        c.env("XDG_CONFIG_HOME", conf.path());
        run_complete(c, words)
    };
    let out = complete(&["bu"]);
    assert!(out.starts_with("build\t"), "{out}");
    assert!(out.ends_with("\n:4\n"), "{out}");
    assert_eq!(complete(&["eval", "--arg", "n", ""]), ":4\n");
    assert_eq!(complete(&["eval", "-f", ""]), ":0\n");
    assert!(complete(&["cache", ""]).starts_with("gc\t"));

    for shell in ["bash", "zsh", "fish", "nushell", "elvish", "powershell"] {
        let o = run(&bin, &["completion", shell]);
        assert_eq!(o.status.code(), Some(0), "{shell}");
        assert!(stdout(&o).contains("__complete"), "{shell}");
    }
    let o = run(&bin, &["completion", "ksh"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("unknown subcommand \'ksh\'"));
}

#[rstest]
fn a_flake_fragment_completes_to_the_outputs_attribute_names(bin: Iets) {
    let d = tempdir().unwrap();
    // a bare word completes under `packages.<this build's system>`
    let mut sys: Vec<&str> = vec![
        "x86_64-linux",
        "aarch64-linux",
        "x86_64-darwin",
        "aarch64-darwin",
        "x86_64-windows",
        "aarch64-windows",
    ];
    let host = iets::config::DEFAULT_SYSTEM;
    if !sys.contains(&host) {
        sys.push(host);
    }
    let pkgs: String = sys
        .iter()
        .map(|s| format!("{s} = {{ hello = 1; help = 2; other = 3; \"a.b\" = 4; }}; "))
        .collect();
    fs::write(
        d.path().join("flake.nix"),
        format!("{{ outputs = _: {{ packages = {{ {pkgs} }}; lib.answer = 42; }}; }}"),
    )
    .unwrap();
    let complete = |word: &str| {
        let mut c = bin.cmd();
        c.current_dir(d.path());
        run_complete(c, &["eval", word])
    };
    // the package prefix is implied, and no space follows: not the end
    assert_eq!(complete(".#he"), ".#hello\n.#help\n:6\n");
    // an explicit path is completed from the root, name by name
    let mut systems = sys.clone();
    systems.sort();
    let want: String = systems
        .iter()
        .map(|s| format!(".#packages.{s}\n"))
        .collect();
    assert_eq!(complete(".#packages."), format!("{want}:6\n"));
    assert_eq!(complete(".#.lib.an"), ".#.lib.answer\n:6\n");
    assert_eq!(complete(".#pack"), ".#packages\n:6\n");
    // nothing matches, or the flake is not there: no candidates, no noise
    assert_eq!(complete(".#zzz"), ":6\n");
    assert_eq!(complete("/nonexistent#a"), ":6\n");
}

#[rstest]
fn dash_a_completes_the_attribute_names_under_its_root(bin: Iets) {
    let d = tempdir().unwrap();
    fs::write(
        d.path().join("x.nix"),
        "{ lib = { alpha = 1; beta = 2; }; pkgs = { hello = 1; }; }",
    )
    .unwrap();
    let complete = |words: &[&str]| {
        let mut c = bin.cmd();
        c.current_dir(d.path());
        run_complete(c, words)
    };
    assert_eq!(
        complete(&["eval", "-f", "x.nix", "-A", ""]),
        "lib\npkgs\n:6\n"
    );
    assert_eq!(
        complete(&["eval", "x.nix", "-A", "lib.b"]),
        "lib.beta\n:6\n"
    );
    assert_eq!(
        complete(&["eval", "-E", "{ a = 1; ab = 2; }", "--attr", "a"]),
        "a\nab\n:6\n"
    );
    // no root to select from, and a root that is not there: nothing, not files
    assert_eq!(complete(&["eval", "-A", "a"]), ":4\n");
    assert_eq!(complete(&["eval", "-f", "nope.nix", "-A", "a"]), ":6\n");
}

// the hermetic env waits for every completion, so only this tests the budget
#[cfg(unix)]
mod budget {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::thread;
    use std::time::{Duration, Instant};

    /// A FIFO that nothing writes to: only the budget can end the evaluation.
    #[rstest]
    fn a_completion_that_evaluates_stops_at_its_budget(bin: Iets) {
        let d = tempdir().unwrap();
        let fifo = d.path().join("x.nix");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: c is a valid C string
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "fifo");
        let complete = |ms: &str| {
            let mut c = bin.cmd();
            c.current_dir(d.path()).env("IETS_COMPLETE_MS", ms);
            run_complete(c, &["eval", "-f", "x.nix", "-A", ""])
        };

        let start = Instant::now();
        assert_eq!(complete("100"), ":6\n");
        assert!(start.elapsed() >= Duration::from_millis(100));

        // with a writer and no budget, the file is read
        let feed = thread::spawn(move || fs::write(fifo, "{ lib = 1; }").expect("feed"));
        assert_eq!(complete("0"), "lib\n:6\n");
        feed.join().unwrap();
    }
}

/// A flake with checks `a` and `b` under `dir`.
fn two_checks(dir: &Path) {
    fs::create_dir_all(dir).expect("create the flake directory");
    fs::write(
        dir.join("flake.nix"),
        r#"{ outputs = _: let
  mk = n: derivation {
    name = n;
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [ "-c" "echo ${n} > $out" ];
  };
in { checks.x86_64-linux = { a = mk "a"; b = mk "b"; }; }; }"#,
    )
    .expect("write flake.nix");
    fs::write(
        dir.join("flake.lock"),
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    )
    .expect("write flake.lock");
}

/// `iets build` with a dead builder. The evaluation comes first, so
/// `--timings` still says what it found.
fn build_unreachable(
    bin: &Iets,
    dir: &Path,
    cache: Option<&Path>,
    installables: &[&str],
) -> String {
    let mut c = bin.cmd();
    c.current_dir(dir)
        .args(["build", "--builder", "unix:///nonexistent", "--timings"]);
    match cache {
        Some(cache) => c.arg("--cache-dir").arg(cache),
        None => c.arg("--no-cache"),
    };
    stderr(&c.args(installables).output().expect("run iets"))
}

const A: &str = ".#checks.x86_64-linux.a";
const B: &str = ".#checks.x86_64-linux.b";

#[rstest]
fn build_evaluates_every_installable(bin: Iets) {
    let d = tempdir().unwrap();
    two_checks(d.path());
    let log = build_unreachable(&bin, d.path(), None, &[A, B]);
    assert!(log.contains("eval: 2 derivations"), "{log}");
}

/// Installables in another order are one job and one cache entry.
#[rstest]
fn build_cache_ignores_installable_order(bin: Iets) {
    let d = tempdir().unwrap();
    two_checks(&d.path().join("flake"));
    let cache = d.path().join("cache");
    let first = build_unreachable(&bin, &d.path().join("flake"), Some(&cache), &[A, B]);
    let second = build_unreachable(&bin, &d.path().join("flake"), Some(&cache), &[B, A]);
    assert!(first.contains("cache: wrote"), "{first}");
    assert!(second.contains("eval: skipped"), "{second}");
}

#[rstest]
fn eval_json_forces_without_strict(bin: Iets) {
    let o = run(&bin, &["eval", "--json", "--expr", "{ a = [ (1 + 1) ]; }"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), r#"{"a":[2]}"#);
}

/// Nix's currentSystem is the host's unless `system` says otherwise.
#[rstest]
fn current_system_is_the_hosts(bin: Iets) {
    let os = match env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let want = format!("\"{}-{os}\"\n", env::consts::ARCH);
    let o = run(
        &bin,
        &["eval", "--impure", "--expr", "builtins.currentSystem"],
    );
    assert_eq!(o.stdout, want.as_bytes(), "{}", stderr(&o));
}

#[cfg(target_os = "linux")]
#[rstest]
#[case::version(&["version"])]
#[case::parse(&["parse", "multi.nix"])]
#[case::help(&["--help"])]
#[case::lint_list(&["lint", "list"])]
fn a_full_stdout_is_an_error(bin: Iets, #[case] args: &[&str]) {
    let d = tempdir().unwrap();
    fs::write(d.path().join("multi.nix"), "{ }").unwrap();
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let o = bin
        .cmd()
        .current_dir(d.path())
        .args(args)
        .stdout(full)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let err = stderr(&o);
    assert!(err.contains("No space left on device"), "{err}");
}

#[rstest]
fn a_bad_config_stops_only_what_reads_a_setting(bin: Iets) {
    let conf = tempdir().unwrap();
    fs::create_dir_all(conf.path().join("iets")).unwrap();
    fs::write(conf.path().join("iets/iets.toml"), "bogus = 1\n").unwrap();
    let run = |a: &[&str]| {
        bin.cmd()
            .env("XDG_CONFIG_HOME", conf.path())
            .args(a)
            .output()
            .unwrap()
    };
    for (a, want) in [
        (&["version"][..], 0),
        (&["--help"], 0),
        (&["eval", "-E", "1"], 1),
    ] {
        let o = run(a);
        assert_eq!(o.status.code(), Some(want), "{a:?}: {}", stderr(&o));
    }
    // the log format is read first, so an early error is a record
    let o = run(&["eval", "--log-format", "internal-json", "-E", "1"]);
    assert!(stderr(&o).starts_with("@nix "), "{}", stderr(&o));
}

#[rstest]
fn eval_says_what_it_does_not_support(bin: Iets) {
    let o = run(&bin, &["eval", "--expr", "builtins.fetchMercurial ./."]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        stderr(&o).contains(r#"builtins.fetchTree { type = "hg";"#),
        "{}",
        stderr(&o)
    );
}

#[rstest]
#[case::registry(
    "nixpkgs#lib.version",
    "flake registry references (nixpkgs#…) are not supported"
)]
#[case::github("github:NixOS/nixpkgs#lib.version", "only local paths are supported")]
fn eval_says_which_flake_refs_it_does_not_support(bin: Iets, #[case] r: &str, #[case] want: &str) {
    let o = run(&bin, &["eval", r]);
    assert_eq!(o.status.code(), Some(3));
    assert!(stderr(&o).contains(want), "{}", stderr(&o));
}

/// Every `-A` prints, in order, from one evaluation of the input
/// (nix-instantiate --eval).
#[rstest]
fn eval_prints_every_attr(bin: Iets) {
    let e = r#"builtins.trace "root" { a = 1; b = 2; c = throw "boom"; }"#;
    let o = run(&bin, &["eval", "-E", e, "-A", "b", "-A", "a"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(o.stdout, "2\n1\n".as_bytes());
    assert_eq!(stderr(&o).matches("trace: root").count(), 1);
    // what came before the failing one is out already
    let o = run(&bin, &["eval", "-E", e, "-A", "a", "-A", "c", "-A", "b"]);
    assert!(!o.status.success());
    assert_eq!(o.stdout, "1\n".as_bytes());
}
