mod hermetic;

use hermetic::{Iets, bin, ok, stderr, stdout};
use rstest::rstest;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};
use tempfile::tempdir;

fn eval(bin: &Iets, dir: &Path, expr: &str) -> Command {
    let mut cmd = bin.cmd();
    cmd.current_dir(dir).args([
        "eval",
        "--impure",
        "--strict",
        "--no-cache",
        "--workers",
        "1",
        "-E",
        expr,
    ]);
    cmd
}

fn capture(cmd: &mut Command, dir: &Path) -> String {
    fs::create_dir(dir.join("jobs")).unwrap();
    stdout(&ok(cmd
        .env("IETS_FINE_TRACE", "")
        .env("IETS_FINE_DAG", "dag")
        .env("IETS_FINE_JOBS", "jobs")))
}

fn changed(bin: &Iets, dir: &Path) -> String {
    // A validation step with no changed source files.
    fs::write(dir.join("delta"), "r\tV\t2\n").unwrap();
    stdout(&ok(bin.cmd().current_dir(dir).args([
        "fine-check",
        "delta",
        "dag",
    ])))
}

#[rstest]
fn changed_environment_and_checkout_time_invalidate_only_their_readers(bin: Iets) {
    let dir = tempdir().unwrap();
    let dir = dir.path();
    let tree = dir.join("tree");
    fs::create_dir(&tree).unwrap();
    let file = tree.join("file");
    fs::write(&file, "same bytes").unwrap();
    let child_time = |s| {
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(s))
            .unwrap()
    };
    let before = fs::metadata(&tree)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    let after = before + 10;
    child_time(before);
    let expr = r#"[ (builtins.getEnv "PROBE") (builtins.fetchTree { type="path"; path=./tree; }).lastModified 7 ]"#;
    let old = capture(eval(&bin, dir, expr).env("PROBE", "old"), dir);
    assert_eq!(old, format!("[ \"old\" {before} 7 ]"));
    child_time(after);
    let new = stdout(&ok(eval(&bin, dir, expr).env("PROBE", "new")));
    assert_eq!(new, format!("[ \"new\" {after} 7 ]"));
    assert_eq!(changed(&bin, dir), "dag\t0\tr\ndag\t1\tr");
    fs::write(dir.join("commands"), "load dag 0\ncheck delta result\n").unwrap();
    let served = ok(bin
        .cmd()
        .current_dir(dir)
        .args(["fine-check", "--serve"])
        .stdin(fs::File::open(dir.join("commands")).unwrap()));
    assert!(!stdout(&served).contains("error"), "{}", stdout(&served));
    let result = fs::read_to_string(dir.join("result")).unwrap();
    assert!(
        result.contains("H\tdag\t0") && result.contains("H\tdag\t1"),
        "{result}"
    );
    assert!(!result.contains("H\tdag\t2"), "{result}");

    let obs = dir.join("dag/obs");
    let old = fs::read_to_string(&obs)
        .unwrap()
        .replace("V\texternal-reads-v1\n", "");
    fs::write(obs, old).unwrap();
    let incompatible = bin
        .cmd()
        .current_dir(dir)
        .args(["fine-check", "delta", "dag"])
        .output()
        .unwrap();
    assert!(!incompatible.status.success());
    assert!(stderr(&incompatible).contains("external read evidence"));
}

#[cfg(unix)]
#[rstest]
fn scm_results_and_legacy_text_jobs_are_revalidated(bin: Iets) {
    let dir = tempdir().unwrap();
    let dir = dir.path();
    let repo = dir.join("repo");
    fs::create_dir(&repo).unwrap();
    ok(Command::new("git").current_dir(&repo).args(["init", "-q"]));
    let commit = |name| {
        ok(Command::new("git").current_dir(&repo).args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "-qm",
            name,
        ]));
    };
    commit("before");
    let expr = format!(
        "[ (builtins.fetchGit {{url=\"{}\";}}).rev 7 ]",
        repo.display()
    );
    let old = capture(&mut eval(&bin, dir, &expr), dir);
    commit("after");
    assert_ne!(old, stdout(&ok(&mut eval(&bin, dir, &expr))));
    assert_eq!(changed(&bin, dir), "dag\t0\tr");
    let observed = fs::read_to_string(dir.join("jobs/0")).unwrap();
    assert!(observed.lines().any(|l| l.starts_with("Q\t")));
    let clean = fs::read_to_string(dir.join("jobs/1")).unwrap();
    let legacy = clean.replace("V\texternal-reads-v1\n", "");
    fs::write(
        dir.join("traces"),
        format!("J\treader\n{observed}J\tclean\n{clean}J\tlegacy\n{legacy}"),
    )
    .unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/incremental/fcheck.awk");
    let result = stdout(&ok(Command::new("awk")
        .current_dir(dir)
        .arg("-f")
        .arg(script)
        .args(["delta", "traces"])));
    assert!(
        result.contains("reader\tr\t") && result.contains("legacy\tr\t"),
        "{result}"
    );
    assert!(!result.contains("clean\tr\t"), "{result}");
}

#[cfg(unix)]
#[rstest]
fn each_reader_of_an_open_stored_input_records_it(bin: Iets) {
    let dir = tempdir().unwrap();
    let dir = dir.path();
    let archive = dir.join("source.tar");
    let mut tar = tar::Builder::new(fs::File::create(&archive).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o644);
    header.set_size(1);
    tar.append_data(&mut header, "src/file", &b"x"[..]).unwrap();
    tar.finish().unwrap();
    let url = format!("file://{}", archive.display());
    let hash = stdout(&ok(bin.cmd().args(["prefetch", "--unpack", &url])));
    let fetch = format!(r#"builtins.fetchTarball {{ url="{url}"; sha256="{hash}"; }}"#);
    let store_path = stdout(&ok(eval(&bin, dir, &fetch).arg("--raw")));
    // First register the fetched input in this evaluator. The second job
    // reaches the same tree by a literal, without forcing the first thunk.
    let jobs =
        format!(r#"[ (builtins.seq ({fetch}) 3) (builtins.readFile "{store_path}/file") 7 ]"#);
    assert_eq!(capture(&mut eval(&bin, dir, &jobs), dir), "[ 3 \"x\" 7 ]");
    for id in ["0", "1"] {
        let reads = stdout(&ok(bin.cmd().current_dir(dir).args([
            "fine-check",
            "--job",
            id,
            "dag",
        ])));
        assert!(reads.lines().any(|l| l.starts_with("I\t")), "{id}: {reads}");
    }
    assert_eq!(changed(&bin, dir), "dag\t0\tr\ndag\t1\tr");
}
