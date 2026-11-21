mod hermetic;

use hermetic::{Iets, bin, ok, stderr, stdout};
use rstest::rstest;
use std::fs;
use std::process::Stdio;
use tempfile::tempdir;

#[rstest]
#[case("1")]
#[case("4")]
fn records_both_jobs_reading_a_shared_thunk(bin: Iets, #[case] workers: &str) {
    let dir = tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file = |name| root.join(name);
    fs::write(file("payload"), "old").unwrap();
    fs::write(
        file("expr.nix"),
        "let shared = builtins.readFile ./payload; in [ shared shared ]",
    )
    .unwrap();
    let eval = || {
        let mut cmd = bin.cmd();
        cmd.args(["eval", "--json", "--no-cache", "--workers", workers])
            .arg(file("expr.nix"));
        cmd
    };
    fs::create_dir(file("jobs")).unwrap();
    let captured = ok(eval()
        .env("IETS_FINE_TRACE", "")
        .env("IETS_FINE_DAG", file("dag"))
        .env("IETS_FINE_JOBS", file("jobs")));
    assert_eq!(stdout(&captured), r#"["old","old"]"#);
    assert_eq!(fs::read_dir(file("jobs")).unwrap().count(), 2);
    fs::write(file("payload"), "new").unwrap();
    assert_eq!(stdout(&ok(&mut eval())), r#"["new","new"]"#);
    let mut delta = b"s\tV\t2\ns\tCM\t".to_vec();
    delta.extend(iets::sys::path_to_internal(&file("payload")));
    delta.push(b'\n');
    fs::write(file("delta"), delta).unwrap();
    let checked = stdout(&ok(bin
        .cmd()
        .arg("fine-check")
        .arg(file("delta"))
        .arg(file("dag"))));
    let dirty: Vec<_> = checked
        .lines()
        .map(|line| line.split('\t').nth(1).unwrap())
        .collect();
    assert_eq!(dirty, ["0", "1"]);

    // Old graphs used zero in the reserved word, regardless of worker count.
    let nodes = file("dag/nodes");
    let mut legacy = fs::read(&nodes).unwrap();
    legacy[4..8].fill(0);
    fs::write(nodes, legacy).unwrap();
    let result = bin
        .cmd()
        .arg("fine-check")
        .arg(file("delta"))
        .arg(file("dag"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(stderr(&result).contains("regenerate the fine graph"));
}

#[rstest]
#[case("IETS_FINE_DAG")]
#[case("IETS_FINE_JOBS")]
fn populated_capture_is_preserved_on_failure(bin: Iets, #[case] channel: &str) {
    let dir = tempdir().unwrap();
    let dest = dir.path().join("recording");
    let capture = |expr: &str| {
        bin.cmd()
            .env("IETS_FINE_TRACE", "")
            .env(channel, &dest)
            .args(["eval", "--no-cache", "--json", "--workers", "4", "-E", expr])
            .output()
            .unwrap()
    };
    let contents = || {
        let mut files: Vec<_> = fs::read_dir(&dest)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect();
        files.sort();
        files
    };
    assert!(capture("[ 1 2 ]").status.success());
    let before = contents();
    let result = capture("[ 3 ]");
    assert_eq!(result.status.code(), Some(1));
    assert!(stderr(&result).contains("use a new or empty directory"));
    assert_eq!(contents(), before);
}

#[rstest]
#[case(false)]
#[case(true)]
fn empty_job_batch_is_a_complete_snapshot(bin: Iets, #[case] precreate: bool) {
    let dir = tempdir().unwrap();
    let dest = dir.path().join("jobs");
    if precreate {
        fs::create_dir(&dest).unwrap();
    }
    let capture = |expr: &str| {
        bin.cmd()
            .env("IETS_FINE_TRACE", "")
            .env("IETS_FINE_JOBS", &dest)
            .args(["eval", "--no-cache", "--json", "--workers", "4", "-E", expr])
            .output()
            .unwrap()
    };
    let first = capture("[]");
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(stdout(&first), "[]");
    let entries: Vec<_> = fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, [".empty"]);
    assert!(fs::read(dest.join(".empty")).unwrap().is_empty());
    for expr in ["[]", "[ 1 ]"] {
        let result = capture(expr);
        assert_eq!(result.status.code(), Some(1));
        assert!(stderr(&result).contains("use a new or empty directory"));
        assert!(!dest.join("0").exists());
        assert!(dest.join(".empty").is_file());
    }
    assert_eq!(fs::read_dir(&dest).unwrap().count(), 1);
}

#[rstest]
#[case("[]")]
#[case("[ 1 ]")]
fn concurrent_empty_job_batch_has_one_publisher(bin: Iets, #[case] other: &str) {
    let dir = tempdir().unwrap();
    let dest = dir.path().join("jobs");
    fs::create_dir(&dest).unwrap();
    let mut children = Vec::new();
    for expr in ["[]", other] {
        children.push(
            bin.cmd()
                .env("IETS_FINE_TRACE", "")
                .env("IETS_FINE_JOBS", &dest)
                .args(["eval", "--no-cache", "--json", "--workers", "4", "-E", expr])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let results: Vec<_> = children
        .into_iter()
        .map(|c| c.wait_with_output().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|r| r.status.success()).count(), 1);
    let winner = results.iter().find(|r| r.status.success()).unwrap();
    if stdout(winner) == "[]" {
        assert!(dest.join(".empty").is_file());
        assert!(!dest.join("0").exists());
    } else {
        assert_eq!(stdout(winner), "[1]");
        assert!(!dest.join(".empty").exists());
        assert!(
            fs::read_to_string(dest.join("0"))
                .unwrap()
                .ends_with("V\tend-serial-volatile-v1\n")
        );
    }
    let loser = results.iter().find(|r| !r.status.success()).unwrap();
    assert_eq!(loser.status.code(), Some(1));
}
