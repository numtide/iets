//! The sampled profile: what a run says when it cannot collect one, and the
//! folded lines when it can.

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;

use std::fs;
use std::process::Output;
use tempfile::tempdir;

fn eval(bin: &Iets, expr: &str, env: &[(&str, &str)]) -> Output {
    let mut c = bin.cmd();
    c.args(["eval", "--no-cache", "--workers", "1", "-E", expr])
        .env_remove("IETS_PROFILE")
        .env_remove("IETS_PROFILE_TO")
        .env_remove("IETS_PROFILE_HZ");
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().expect("run iets")
}

/// The folded lines of a `kind` profile of `expr`, and the run.
fn profile(bin: &Iets, kind: &str, expr: &str) -> (String, Output) {
    let dir = tempdir().unwrap();
    let out = dir.path().join("p.folded");
    let o = eval(
        bin,
        expr,
        &[
            ("IETS_PROFILE", kind),
            ("IETS_PROFILE_TO", out.to_str().unwrap()),
            ("IETS_PROFILE_HZ", "1000"),
            ("IETS_STATS", "1"),
        ],
    );
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let folded = fs::read_to_string(&out).expect("the profile file");
    (folded, o)
}

#[rstest]
fn a_profile_that_is_not_eval_is_a_settings_error(bin: Iets) {
    let o = eval(&bin, "1", &[("IETS_PROFILE", "cpu")]);
    assert_ne!(o.status.code(), Some(0));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("IETS_PROFILE: 'cpu' is not 'eval'"), "{err}");
}

#[rstest]
fn an_unprofiled_run_says_nothing_about_a_profile(bin: Iets) {
    let o = eval(&bin, "1", &[]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!err.contains("profile"), "{err}");
}

/// A run with no call in it takes no sample, and an empty profile is said
/// out loud rather than written as an empty file.
#[rstest]
fn a_run_shorter_than_a_tick_says_it_sampled_nothing(bin: Iets) {
    let o = eval(&bin, "1 + 1", &[("IETS_PROFILE", "eval")]);
    assert_eq!(o.status.code(), Some(0));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("profile: no samples"), "{err}");
}

#[rstest]
fn a_folded_line_is_a_stack_and_a_count(bin: Iets) {
    // mutual recursion, so a sample sees a stack and not one frame
    let expr = "let f = n: if n == 0 then 0 else g (n - 1); g = n: f n; \
                in builtins.foldl' (a: _: a + f 400) 0 (builtins.genList (x: x) 4000)";
    let (folded, _) = profile(&bin, "eval", expr);
    let mut deepest = 0;
    for line in folded.lines() {
        let (stack, count) = line.rsplit_once(' ').expect("a stack and a count");
        count.parse::<u64>().expect("a count");
        for frame in stack.split(';') {
            assert!(!frame.is_empty(), "{line}");
        }
        deepest = deepest.max(stack.split(';').count());
    }
    assert!(deepest > 2, "every stack is shallow:\n{folded}");
}

/// A builtin is a frame too, so time inside one is not blamed on the lambda
/// that called it.
#[rstest]
fn a_builtin_names_itself_in_a_frame(bin: Iets) {
    let expr = "builtins.length (builtins.foldl' (a: _: a ++ builtins.genList (x: x) 50) [] \
                (builtins.genList (x: x) 2000))";
    let (folded, _) = profile(&bin, "eval", expr);
    assert!(folded.contains(":«"), "no builtin frame:\n{folded}");
}

/// `alloc` weighs a sample by the arena bytes since the one before it, so
/// the folded counts add up to what the arena allocated. Every byte is
/// taken by the next sample, so only what came after the last one is
/// missing.
#[rstest]
fn an_allocation_profile_accounts_for_the_arena(bin: Iets) {
    let expr = "builtins.length (builtins.foldl' (a: _: a ++ builtins.genList toString 200) [] \
                (builtins.genList (x: x) 2000))";
    let (folded, o) = profile(&bin, "alloc", expr);
    let weighed: u64 = folded
        .lines()
        .map(|l| {
            l.rsplit_once(' ')
                .expect("a stack and a count")
                .1
                .parse::<u64>()
                .expect("a count")
        })
        .sum();
    let err = String::from_utf8_lossy(&o.stderr);
    let total: u64 = err
        .split_whitespace()
        .find_map(|w| w.strip_prefix("arena_bytes=")?.parse().ok())
        .expect("arena_bytes in the stats line");
    assert!(
        weighed * 100 > total * 90 && weighed <= total,
        "weighed {weighed} of {total} arena bytes"
    );
}

/// A profile is read per fiber, so more than one worker gives stacks with no
/// shape. Asking for both is refused rather than answered misleadingly.
#[rstest]
fn a_profile_refuses_more_than_one_worker(bin: Iets) {
    let mut c = bin.cmd();
    let o = c
        .args(["eval", "--no-cache", "--workers", "4", "-E", "1"])
        .env("IETS_PROFILE", "eval")
        .output()
        .expect("run iets");
    assert_ne!(o.status.code(), Some(0));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("--workers must be 1, not 4"), "{err}");

    // the same through the setting rather than the flag
    let mut c = bin.cmd();
    let o = c
        .args(["eval", "--no-cache", "-E", "1"])
        .env("IETS_PROFILE", "eval")
        .env("IETS_WORKERS", "4")
        .output()
        .expect("run iets");
    assert_ne!(o.status.code(), Some(0));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("workers must be 1, not 4"), "{err}");
}
