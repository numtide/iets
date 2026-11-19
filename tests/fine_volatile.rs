//! Inputs without a replayable validator must reach each result that forced them.
#![cfg(unix)]

mod hermetic;
use hermetic::{bin, ok, stdout};
use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn uncacheable_forces_reach_job_graphs_and_flat_traces() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let entry = root.join("job.nix");
    let graph = root.join("graph");
    let jobs = root.join("jobs");
    fs::create_dir(&jobs).unwrap();
    let delta = root.join("changes");
    fs::write(&delta, "1\tV\t2\n1\tCM\t/unrelated\n").unwrap();
    let runner = bin();
    fn suspects(output: &str) -> Vec<usize> {
        output
            .lines()
            .map(|row| row.split('\t').nth(1).unwrap().parse().unwrap())
            .collect()
    }
    let capture = |src: &str, setting: &str| {
        fs::write(&entry, src).unwrap();
        stdout(&ok(runner
            .cmd()
            .args([
                "eval",
                setting,
                "--strict",
                "--json",
                "--no-tracing-cache",
                "--workers",
                "1",
            ])
            .env("IETS_FINE_TRACE", "")
            .env("IETS_FINE_DAG", &graph)
            .env("IETS_FINE_JOBS", &jobs)
            .arg(&entry)))
    };
    let check = || {
        suspects(&stdout(&ok(runner
            .cmd()
            .arg("fine-check")
            .arg(&delta)
            .arg(&graph))))
    };
    let text_check = |input: &Path| {
        Command::new("awk")
            .arg("-f")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/incremental/fcheck.awk"))
            .arg(&delta)
            .arg(input)
            .output()
            .unwrap()
    };
    // The second job reaches the same already-computed currentTime thunk through
    // a conditional. The third creates a lazy volatile binding without forcing it.
    let answer = capture(
        "[ (builtins.currentTime + 1) (if builtins.currentTime > 0 then 8 else 9) (let idle = builtins.currentTime; in 7) ]",
        "--impure",
    );
    assert!(answer.ends_with(",8,7]"));
    assert_eq!(check(), [0, 1]);
    let mut flat = String::new();
    for i in 0..3 {
        let job = fs::read_to_string(jobs.join(i.to_string())).unwrap();
        assert_eq!(job.lines().any(|row| row == "R\t?\tvolatile\t"), i != 2);
        flat.push_str(&format!("J\t{i}\n{job}"));
    }
    let flat_file = root.join("flat");
    fs::write(&flat_file, &flat).unwrap();
    let output = text_check(&flat_file);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output)
            .lines()
            .map(|row| row.split('\t').next().unwrap())
            .collect::<Vec<_>>(),
        ["0", "1"]
    );
    fs::write(&flat_file, flat.replace("V\tvolatile-v1\n", "")).unwrap();
    assert!(!text_check(&flat_file).status.success());
    let obs = graph.join("obs");
    let recorded = fs::read_to_string(&obs).unwrap();
    fs::write(&obs, recorded.replace("V\tvolatile-v1\n", "")).unwrap();
    assert!(
        !runner
            .cmd()
            .arg("fine-check")
            .arg(&delta)
            .arg(&graph)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !runner
            .cmd()
            .args(["fine-check", "--job", "0"])
            .arg(&graph)
            .output()
            .unwrap()
            .status
            .success()
    );

    assert_eq!(
        capture("[ (builtins ? currentTime) 7 ]", "--pure-eval"),
        "[false,7]"
    );
    assert!(check().is_empty());
    let defs = root.join("defs.nix");
    fs::write(&defs, "toString 123").unwrap();
    let scoped = r#"let a=builtins.scopedImport { toString = x: "shadow"; } ./defs.nix; in [ (a + "output") 7 ]"#;
    assert_eq!(capture(scoped, "--impure"), r#"["shadowoutput",7]"#);
    assert_eq!(check(), [0]);
    fs::write(&defs, "builtins.toString 123").unwrap();
    let fresh = stdout(&ok(runner
        .cmd()
        .args(["eval", "--impure", "--strict", "--json", "--no-cache"])
        .arg(&entry)));
    assert_eq!(fresh, r#"["123output",7]"#);
}
