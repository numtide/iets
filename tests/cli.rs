//! The native command line: help and version on stdout with exit 0, a bad
//! command line on stderr with exit 2 and never the whole usage.

mod hermetic;

use rstest::rstest;
use std::path::Path;
use std::process::{Command, Output};
use std::{env, fs};
use tempfile::tempdir;

fn iets(args: &[&str]) -> Output {
    hermetic::iets().args(args).output().expect("run iets")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn run_complete(mut c: Command, words: &[&str]) -> String {
    let o = c.arg("__complete").args(words).output().expect("run iets");
    assert_eq!(o.status.code(), Some(0), "{words:?}: {}", text(&o.stderr));
    assert!(o.stderr.is_empty(), "{words:?}: {}", text(&o.stderr));
    text(&o.stdout)
}

#[test]
fn help_and_version_go_to_stdout() {
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
        let o = iets(args);
        assert_eq!(o.status.code(), Some(0), "{args:?}");
        assert!(o.stderr.is_empty(), "{args:?}: {}", text(&o.stderr));
        assert!(!o.stdout.is_empty(), "{args:?}");
    }
    let o = iets(&["--help"]);
    assert!(text(&o.stdout).contains("  eval-jobs "));
    let o = iets(&["eval", "--help"]);
    assert!(text(&o.stdout).starts_with("usage: iets eval "));
    assert!(text(&o.stdout).contains("--strict"));
    let v = text(&iets(&["--version"]).stdout);
    assert_eq!(v.trim_end(), format!("iets {}", env!("CARGO_PKG_VERSION")));
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
fn a_bad_command_line_names_the_problem(#[case] args: &[&str], #[case] want: &str) {
    let o = iets(args);
    let err = text(&o.stderr);
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
#[test]
fn dash_f_names_the_file_in_the_native_namespace() {
    for args in [
        &["eval", "-f", "/nonexistent/missing.nix"][..],
        &["eval", "--file", "/nonexistent/missing.nix"],
        &["build", "-f", "/nonexistent/missing.nix"],
        &["prefetch", "-f", "/nonexistent/missing.nix"],
        &["eval-jobs", "-f", "/nonexistent/missing.nix"],
    ] {
        let err = text(&iets(args).stderr);
        assert!(err.contains(NO_SUCH_FILE), "{args:?}: {err}");
    }
    let o = iets(&["nix-eval-jobs", "-f", "/nonexistent/missing.nix"]);
    let err = text(&o.stderr);
    assert!(err.contains("unknown flag '-f'"), "{err}");
    assert_eq!(o.status.code(), Some(1), "{err}");
}

#[test]
fn io_errors_read_like_strerror() {
    let o = iets(&["eval", "/nonexistent/missing.nix"]);
    let err = text(&o.stderr);
    assert!(err.contains(NO_SUCH_FILE), "{err}");
    assert!(!err.contains("os error"), "{err}");
}

#[test]
fn completion_asks_the_binary_and_ignores_settings() {
    let conf = tempdir().unwrap();
    fs::create_dir_all(conf.path().join("iets")).unwrap();
    fs::write(conf.path().join("iets/iets.toml"), "this is = not [valid\n").unwrap();
    let complete = |words: &[&str]| {
        let mut c = hermetic::iets();
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
        let o = iets(&["completion", shell]);
        assert_eq!(o.status.code(), Some(0), "{shell}");
        assert!(text(&o.stdout).contains("__complete"), "{shell}");
    }
    let o = iets(&["completion", "ksh"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("unknown subcommand \'ksh\'"));
}

#[test]
fn a_flake_fragment_completes_to_the_outputs_attribute_names() {
    let d = tempdir().unwrap();
    let sys = [
        "x86_64-linux",
        "aarch64-linux",
        "x86_64-darwin",
        "aarch64-darwin",
        "x86_64-windows",
        "aarch64-windows",
    ];
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
        let mut c = hermetic::iets();
        c.current_dir(d.path());
        run_complete(c, &["eval", word])
    };
    // the package prefix is implied, and no space follows: not the end
    assert_eq!(complete(".#he"), ".#hello\n.#help\n:6\n");
    // an explicit path is completed from the root, name by name
    let mut systems = sys.to_vec();
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

#[test]
fn dash_a_completes_the_attribute_names_under_its_root() {
    let d = tempdir().unwrap();
    fs::write(
        d.path().join("x.nix"),
        "{ lib = { alpha = 1; beta = 2; }; pkgs = { hello = 1; }; }",
    )
    .unwrap();
    let complete = |words: &[&str]| {
        let mut c = hermetic::iets();
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
fn build_unreachable(dir: &Path, cache: Option<&Path>, installables: &[&str]) -> String {
    let mut c = hermetic::iets();
    c.current_dir(dir)
        .args(["build", "--builder", "unix:///nonexistent", "--timings"]);
    match cache {
        Some(cache) => c.arg("--cache-dir").arg(cache),
        None => c.arg("--no-cache"),
    };
    text(&c.args(installables).output().expect("run iets").stderr)
}

const A: &str = ".#checks.x86_64-linux.a";
const B: &str = ".#checks.x86_64-linux.b";

/// The last installable used to replace the earlier ones.
#[test]
fn build_evaluates_every_installable() {
    let d = tempdir().unwrap();
    two_checks(d.path());
    let log = build_unreachable(d.path(), None, &[A, B]);
    assert!(log.contains("eval: 2 derivations"), "{log}");
}

/// Installables in another order are one job and one cache entry.
#[test]
fn build_cache_ignores_installable_order() {
    let d = tempdir().unwrap();
    two_checks(&d.path().join("flake"));
    let cache = d.path().join("cache");
    let first = build_unreachable(&d.path().join("flake"), Some(&cache), &[A, B]);
    let second = build_unreachable(&d.path().join("flake"), Some(&cache), &[B, A]);
    assert!(first.contains("cache: wrote"), "{first}");
    assert!(second.contains("eval: skipped"), "{second}");
}

#[test]
fn eval_json_forces_without_strict() {
    let o = iets(&["eval", "--json", "--expr", "{ a = [ (1 + 1) ]; }"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout).trim(), r#"{"a":[2]}"#);
}

/// Nix's currentSystem is the host's unless `system` says otherwise.
#[test]
fn current_system_is_the_hosts() {
    let os = match env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let want = format!("\"{}-{os}\"\n", env::consts::ARCH);
    let o = iets(&["eval", "--impure", "--expr", "builtins.currentSystem"]);
    assert_eq!(text(&o.stdout), want, "{}", text(&o.stderr));
}

#[cfg(target_os = "linux")]
#[rstest]
#[case::version(&["version"])]
#[case::parse(&["parse", "multi.nix"])]
#[case::help(&["--help"])]
#[case::lint_list(&["lint", "list"])]
fn a_full_stdout_is_an_error(#[case] args: &[&str]) {
    let d = tempdir().unwrap();
    fs::write(d.path().join("multi.nix"), "{ }").unwrap();
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let o = hermetic::iets()
        .current_dir(d.path())
        .args(args)
        .stdout(full)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(err.contains("No space left on device"), "{err}");
}

#[test]
fn a_bad_config_stops_only_what_reads_a_setting() {
    let conf = tempdir().unwrap();
    fs::create_dir_all(conf.path().join("iets")).unwrap();
    fs::write(conf.path().join("iets/iets.toml"), "bogus = 1\n").unwrap();
    let run = |a: &[&str]| {
        hermetic::iets()
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
        assert_eq!(o.status.code(), Some(want), "{a:?}: {}", text(&o.stderr));
    }
    // the log format is read first, so an early error is a record
    let o = run(&["eval", "--log-format", "internal-json", "-E", "1"]);
    assert!(text(&o.stderr).starts_with("@nix "), "{}", text(&o.stderr));
}

#[test]
fn eval_says_what_it_does_not_support() {
    let o = iets(&["eval", "--expr", "builtins.fetchMercurial ./."]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains(r#"builtins.fetchTree { type = "hg";"#),
        "{}",
        text(&o.stderr)
    );
}

#[rstest]
#[case::registry(
    "nixpkgs#lib.version",
    "flake registry references (nixpkgs#…) are not supported"
)]
#[case::github("github:NixOS/nixpkgs#lib.version", "only local paths are supported")]
fn eval_says_which_flake_refs_it_does_not_support(#[case] r: &str, #[case] want: &str) {
    let o = iets(&["eval", r]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains(want), "{}", text(&o.stderr));
}

/// Every `-A` prints, in order, from one evaluation of the input
/// (nix-instantiate --eval).
#[test]
fn eval_prints_every_attr() {
    let e = r#"builtins.trace "root" { a = 1; b = 2; c = throw "boom"; }"#;
    let o = iets(&["eval", "-E", e, "-A", "b", "-A", "a"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "2\n1\n");
    assert_eq!(text(&o.stderr).matches("trace: root").count(), 1);
    // what came before the failing one is out already
    let o = iets(&["eval", "-E", e, "-A", "a", "-A", "c", "-A", "b"]);
    assert!(!o.status.success());
    assert_eq!(text(&o.stdout), "1\n");
}
