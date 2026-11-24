//! Both raw fine-change readers must reject a malformed delta before using it.
#![cfg(unix)]

mod hermetic;

use hermetic::{Iets, bin, ok, stderr, stdout};
use rstest::rstest;
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

#[rstest]
fn malformed_changes_cannot_report_a_clean_job(bin: Iets) {
    let dir = tempdir().unwrap();
    let dir = dir.path();
    let payload = dir.join("payload");
    fs::write(dir.join("root.nix"), "[ (import ./payload) ]").unwrap();
    fs::write(&payload, "17").unwrap();
    ok(bin
        .cmd()
        .current_dir(dir)
        .env("IETS_FINE_TRACE", "")
        .env("IETS_FINE_DAG", dir.join("dag"))
        .args([
            "eval",
            "--workers",
            "1",
            "--no-cache",
            "--strict",
            "--json",
            "-f",
            "root.nix",
        ]));
    fs::write(&payload, "18").unwrap();
    let path = payload.display();
    let change = format!("step\tV\t3\r\nstep\tCR\t{path}\t\r\n");
    fs::write(dir.join("good"), &change).unwrap();
    let check = |delta| {
        bin.cmd()
            .current_dir(dir)
            .args(["fine-check", delta, "dag"])
            .output()
            .unwrap()
    };
    assert_eq!(stdout(&check("good")).trim_end(), "dag\t0\tstep");

    let awk = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/incremental/fcheck.awk");
    fs::write(dir.join("empty"), "").unwrap();
    for bad in [
        change.replace("\tCR\t", "\tCRR\t"),
        format!("step\tCR\t{path}\n"),
        format!("step\tCM\t{path}\tunexpected\n"),
        format!("step\tCL\t{path}\t\tinvalid\tx\n"),
        format!("step\tCD\t{path}\t\n"),
        format!("step\tCX\t{path}\t4294967296\n"),
        "step\tCM\t\n".to_string(),
    ] {
        fs::write(dir.join("bad"), &bad).unwrap();
        let run = check("bad");
        assert!(!run.status.success(), "{bad:?}");
        assert!(
            stderr(&run).contains("malformed change record"),
            "{}",
            stderr(&run)
        );
        assert!(run.stdout.is_empty());
        let text = Command::new("awk")
            .current_dir(dir)
            .arg("-f")
            .arg(&awk)
            .args(["bad", "empty"])
            .output()
            .unwrap();
        assert!(!text.status.success() && stderr(&text).contains("malformed change record"));
    }

    // Ending mid-record can otherwise form a different valid path, e.g.
    // '/payload' truncated to '/p'. A complete valid prefix remains a matter
    // for the producer or an authenticated manifest.
    fs::write(dir.join("bad"), "step\tCM\t/p").unwrap();
    assert!(!check("bad").status.success());
    fs::write(
        dir.join("commands"),
        "load dag 0\ncheck bad partial\ncheck good complete\n",
    )
    .unwrap();
    let resident = ok(bin
        .cmd()
        .current_dir(dir)
        .args(["fine-check", "--serve"])
        .stdin(fs::File::open(dir.join("commands")).unwrap()));
    let replies = stdout(&resident);
    assert!(replies.lines().nth(1).unwrap().starts_with("error bad:"));
    assert_eq!(replies.lines().last(), Some("ok"));
    assert!(!dir.join("partial").exists());
    assert!(
        fs::read_to_string(dir.join("complete"))
            .unwrap()
            .contains("H\tdag\t0")
    );
}

#[test]
fn text_checker_routes_each_name_in_a_directory_change() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("changes");
    fs::write(&file, "step\tV\t3\r\nstep\tCD\t/repo\tfirst second\r\n").unwrap();
    let trace = dir.path().join("trace");
    fs::write(&trace, "J\tobserved\nV\t2\nV\tattr-order-1\nV\texternal-reads-v1\nV\tvolatile-v1\nV\tjob-output-v1\nV\tlist-control-v1\nP\t0\tdir\t/repo\nK\t0\tsecond\n").unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/incremental/fcheck.awk");
    let result = ok(Command::new("awk")
        .arg("-f")
        .arg(script)
        .arg(file)
        .arg(trace));
    assert_eq!(stdout(&result).trim_end(), "observed\tstep\tK second");
}
