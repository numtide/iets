//! `iets help TOPIC` from outside the repository: the binary carries the
//! reference pages, so no `docs/` directory has to be reachable.

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;

use std::process::Output;

/// The binary, run from `/`, where there is no `docs/`.
fn run(bin: &Iets, args: &[&str]) -> Output {
    bin.cmd()
        .args(args)
        .current_dir("/")
        .output()
        .expect("iets runs")
}

fn stdout(bin: &Iets, args: &[&str]) -> String {
    let o = run(bin, args);
    assert!(o.status.success(), "{args:?}: {:?}", o.status);
    String::from_utf8(o.stdout).expect("utf-8")
}

#[rstest]
fn help_lists_the_topics(bin: Iets) {
    let s = stdout(&bin, &["help"]);
    assert!(s.contains("usage: iets COMMAND"));
    assert!(s.contains("topics:"));
    for t in ["settings", "errors", "lints", "divergences", "caching"] {
        assert!(s.contains(t), "{t} is not listed");
    }
}

#[rstest]
fn a_topic_prints_its_page(bin: Iets) {
    let s = stdout(&bin, &["help", "settings"]);
    assert!(s.starts_with("# Settings"), "{}", &s[..40.min(s.len())]);
    assert!(s.contains("IETS_WORKERS"));

    let s = stdout(&bin, &["help", "errors"]);
    assert!(s.contains("| `build-failed` |"));

    let s = stdout(&bin, &["help", "divergences"]);
    assert!(s.contains("builtins.fromTOML"));
}

#[rstest]
fn a_command_wins_over_a_topic_of_the_same_name(bin: Iets) {
    let s = stdout(&bin, &["help", "plumbing"]);
    assert!(s.contains("usage: iets plumbing OP"));
    assert!(s.contains("'iets help reference/plumbing'"));

    let s = stdout(&bin, &["help", "reference/plumbing"]);
    assert!(s.starts_with("# Plumbing operations"));
}

#[rstest]
fn a_page_the_binary_does_not_carry_names_its_path(bin: Iets) {
    let s = stdout(&bin, &["help", "caching"]);
    assert!(s.contains("docs/explanation/caching.md"));
}

#[rstest]
fn an_unknown_topic_is_a_usage_error_with_a_suggestion(bin: Iets) {
    let o = run(&bin, &["help", "settigns"]);
    assert_eq!(o.status.code(), Some(2));
    let e = String::from_utf8(o.stderr).expect("utf-8");
    assert!(e.contains("unknown command or topic 'settigns'"), "{e}");
    assert!(e.contains("did you mean 'settings'?"), "{e}");
}
