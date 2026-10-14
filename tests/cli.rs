//! The native command line: help and version on stdout with exit 0, a bad
//! command line on stderr with exit 2 and never the whole usage.

mod hermetic;

use std::path::Path;
use std::process::{self, Command, Output};
use std::{env, fs};

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

#[test]
fn a_bad_command_line_names_the_problem() {
    for (args, want) in [
        (&["bogus"][..], "error: unknown command 'bogus'\n"),
        (
            &["buidl"],
            "error: unknown command 'buidl'\ndid you mean 'build'?\n",
        ),
        (&["serach", "^"], "did you mean 'search'?"),
        (
            &["nix-instantate", "--eval", "-E", "1"],
            "did you mean 'nix-instantiate'?",
        ),
        (
            &["eval", "--stirct", "-E", "1"],
            "error: unknown flag '--stirct'\ndid you mean '--strict'?\nrun 'iets eval --help' for the flags\n",
        ),
        (
            &["eval", "--workers", "0", "-E", "1"],
            "--workers wants a number >= 1, got '0'",
        ),
        (&["eval", "-E"], "error: -E needs an expression\n"),
        (
            &["eval", "-E", "1", "x.nix"],
            "-E and FILE cannot be combined",
        ),
        (&["build"], "nothing to evaluate"),
        (&["build", "--what"], "unknown flag '--what'"),
        (&["cache", "dunp"], "did you mean 'dump'?"),
    ] {
        let o = iets(args);
        let err = text(&o.stderr);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {err}");
        assert!(o.stdout.is_empty(), "{args:?}");
        assert!(err.contains(want), "{args:?}: {err}");
        assert!(
            !err.contains("usage:") && !err.contains("falling back"),
            "{args:?}: {err}"
        );
    }
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
    let conf = env::temp_dir().join(format!("iets-complete-{}", process::id()));
    fs::create_dir_all(conf.join("iets")).unwrap();
    fs::write(conf.join("iets/iets.toml"), "this is = not [valid\n").unwrap();
    let complete = |words: &[&str]| {
        let mut c = hermetic::iets();
        c.env("XDG_CONFIG_HOME", &conf);
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
    let _ = fs::remove_dir_all(&conf);
}

#[test]
fn a_flake_fragment_completes_to_the_outputs_attribute_names() {
    let d = env::temp_dir().join(format!("iets-complete-flake-{}", process::id()));
    fs::create_dir_all(&d).unwrap();
    let sys = [
        "x86_64-linux",
        "aarch64-linux",
        "x86_64-darwin",
        "aarch64-darwin",
    ];
    let pkgs: String = sys
        .iter()
        .map(|s| format!("{s} = {{ hello = 1; help = 2; other = 3; \"a.b\" = 4; }}; "))
        .collect();
    fs::write(
        d.join("flake.nix"),
        format!("{{ outputs = _: {{ packages = {{ {pkgs} }}; lib.answer = 42; }}; }}"),
    )
    .unwrap();
    let complete = |word: &str| {
        let mut c = hermetic::iets();
        c.current_dir(&d);
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
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn dash_a_completes_the_attribute_names_under_its_root() {
    let d = env::temp_dir().join(format!("iets-complete-attr-{}", process::id()));
    fs::create_dir_all(&d).unwrap();
    fs::write(
        d.join("x.nix"),
        "{ lib = { alpha = 1; beta = 2; }; pkgs = { hello = 1; }; }",
    )
    .unwrap();
    let complete = |words: &[&str]| {
        let mut c = hermetic::iets();
        c.current_dir(&d);
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
    let _ = fs::remove_dir_all(&d);
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
    let d = env::temp_dir().join(format!("iets-build-several-{}", process::id()));
    two_checks(&d);
    let log = build_unreachable(&d, None, &[A, B]);
    let _ = fs::remove_dir_all(&d);
    assert!(log.contains("eval: 2 derivations"), "{log}");
}

/// Installables in another order are one job and one cache entry.
#[test]
fn build_cache_ignores_installable_order() {
    let d = env::temp_dir().join(format!("iets-build-order-{}", process::id()));
    two_checks(&d.join("flake"));
    let cache = d.join("cache");
    let first = build_unreachable(&d.join("flake"), Some(&cache), &[A, B]);
    let second = build_unreachable(&d.join("flake"), Some(&cache), &[B, A]);
    let _ = fs::remove_dir_all(&d);
    assert!(first.contains("cache: wrote"), "{first}");
    assert!(second.contains("eval: skipped"), "{second}");
}
