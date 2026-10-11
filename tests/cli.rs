//! The native command line: help and version on stdout with exit 0, a bad
//! command line on stderr with exit 2 and never the whole usage.

mod hermetic;

use std::process::Output;

fn iets(args: &[&str]) -> Output {
    hermetic::iets().args(args).output().expect("run iets")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
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
        (
            &["build", "--store", "ssh-ng://h", "x.nix"],
            "error: unknown flag '--store'\ndid you mean '--builder'?\n",
        ),
        (
            &["print-dev-env", "--store", "ssh-ng://h", "."],
            "did you mean '--builder'?",
        ),
        (
            &["build", "--option", "store", "ssh-ng://h", "x.nix"],
            "error: iets build does not read the setting 'store'\ndid you mean '--builder'?\n",
        ),
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
        assert!(err.contains("No such file or directory"), "{args:?}: {err}");
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
    assert!(err.contains("No such file or directory"), "{err}");
    assert!(!err.contains("os error"), "{err}");
}
