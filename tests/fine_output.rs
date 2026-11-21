mod hermetic;

use hermetic::{Iets, bin, ok, stderr, stdout};
use rstest::rstest;
use std::fs;
use tempfile::tempdir;

#[rstest]
#[case("IETS_FINE_TRACE")]
#[case("IETS_FINE_JOBS")]
#[case("IETS_FINE_DAG")]
fn failed_output_fails_eval(bin: Iets, #[case] output: &str) {
    let dir = tempdir().unwrap();
    let previous = dir.path().join("previous");
    fs::write(&previous, "previous capture").unwrap();
    let dest = previous.join("requested-output");
    let result = bin
        .cmd()
        .env("IETS_FINE_TRACE", "")
        .env(output, &dest)
        .args([
            "eval",
            "--no-cache",
            "--strict",
            "--workers",
            "1",
            "-E",
            "[ 1 ]",
        ])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(stdout(&result), "[ 1 ]");
    assert!(stderr(&result).contains(&format!("fine trace: writing {}", dest.display())));
    assert_eq!(fs::read_to_string(previous).unwrap(), "previous capture");
}

#[rstest]
fn warm_eval_still_writes_a_requested_graph(bin: Iets) {
    let dir = tempdir().unwrap();
    let dag = dir.path().join("graph");
    fs::create_dir(&dag).unwrap();
    let eval = || {
        let mut c = bin.cmd();
        c.args([
            "eval",
            "--strict",
            "--timings",
            "--workers",
            "1",
            "-E",
            "[ (builtins ? fineRoot) ]",
        ]);
        c
    };
    assert_eq!(stdout(&ok(&mut eval())), "[ false ]");
    assert!(stderr(&ok(&mut eval())).contains("cache: hit"));
    let traced = ok(eval().env("IETS_FINE_TRACE", "").env("IETS_FINE_DAG", &dag));
    assert_eq!(stdout(&traced), "[ true ]");
    assert!(dag.join("roots").is_file());
    assert!(dag.join("nodes").is_file());
    assert_eq!(stdout(&ok(&mut eval())), "[ false ]");
}

#[rstest]
fn failed_trace_output_preserves_the_evaluation_error(bin: Iets) {
    let dir = tempdir().unwrap();
    // Writing the whole-run trace onto a directory fails even after eval fails.
    let result = bin
        .cmd()
        .env("IETS_FINE_TRACE", dir.path())
        .args([
            "eval",
            "--no-cache",
            "--workers",
            "1",
            "-E",
            "throw \"evaluation failed\"",
        ])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    let err = stderr(&result);
    assert!(err.contains("evaluation failed"), "{err}");
    assert!(err.contains("fine trace: writing"), "{err}");
}
