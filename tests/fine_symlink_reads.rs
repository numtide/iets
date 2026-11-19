//! Git names changes by repository path. The kernel follows symbolic links
//! while resolving many of the paths a Nix job reads.
#![cfg(unix)]

mod hermetic;

use hermetic::{Iets, bin, ok, stdout};
use serde_json::{Value, from_str};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output};

fn evaluate(runner: &Iets, root: &Path, trace: bool) -> Value {
    let mut cmd = runner.cmd();
    cmd.args([
        "eval",
        "--strict",
        "--impure",
        "--json",
        "--no-cache",
        "--workers",
        "1",
    ])
    .arg(root.join("jobs.nix"));
    if trace {
        let jobs = root.join("jobs");
        fs::create_dir_all(&jobs).unwrap();
        cmd.env("IETS_FINE_TRACE", "")
            .env("IETS_FINE_DAG", root.join("graph"))
            .env("IETS_FINE_JOBS", jobs);
    }
    from_str(&stdout(&ok(&mut cmd))).unwrap()
}

fn check(runner: &Iets, root: &Path) -> Output {
    runner
        .cmd()
        .arg("fine-check")
        .arg(root.join("delta"))
        .arg(root.join("graph"))
        .output()
        .unwrap()
}

fn dirty(out: &Output, job_column: usize) -> Vec<usize> {
    assert!(out.status.success(), "{}", hermetic::stderr(out));
    stdout(out)
        .lines()
        .map(|row| row.split('\t').nth(job_column).unwrap().parse().unwrap())
        .collect()
}

#[test]
fn symlink_reads_invalidate_only_their_consumers() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let real = root.join("real");
    fs::create_dir(&real).unwrap();
    let target = real.join("target");
    fs::write(&target, "old").unwrap();
    fs::write(root.join("untouched"), "stable").unwrap();
    symlink("real/target", root.join("link")).unwrap();
    symlink("link", root.join("chain")).unwrap();
    symlink("real", root.join("dir-link")).unwrap();
    fs::write(
        root.join("jobs.nix"),
        r#"[
        (builtins.readFile ./link)
        (builtins.readFile ./chain)
        (builtins.readFile ./dir-link/target)
        ((builtins.readDir ./dir-link) ? added)
        (builtins.pathExists ./dir-link/added)
        (builtins.readFileType ./link)
        (builtins.readFile ./untouched)
    ]"#,
    )
    .unwrap();
    let runner = bin();
    assert_eq!(
        evaluate(&runner, &root, true),
        serde_json::json!(["old", "old", "old", false, false, "symlink", "stable"])
    );
    fs::write(&target, "new").unwrap();
    fs::write(real.join("added"), "yes").unwrap();
    assert_eq!(
        evaluate(&runner, &root, false),
        serde_json::json!(["new", "new", "new", true, true, "symlink", "stable"])
    );
    let delta = root.join("delta");
    fs::write(
        &delta,
        format!(
            "1\tV\t2\n1\tCM\t{}\n1\tCM\t{}\n1\tCS\t{}\n1\tCD\t{}\tadded\n1\tCT\t{}\n",
            target.display(),
            real.join("added").display(),
            real.join("added").display(),
            real.display(),
            real.display()
        ),
    )
    .unwrap();
    assert_eq!(dirty(&check(&runner, &root), 1), [0, 1, 2, 3, 4]);

    let flat = root.join("flat");
    let mut text = String::new();
    for i in 0..7 {
        let job = fs::read_to_string(root.join("jobs").join(i.to_string())).unwrap();
        text.push_str(&format!("J\t{i}\n{job}"));
    }
    fs::write(&flat, &text).unwrap();
    let awk = || {
        Command::new("awk")
            .arg("-f")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/incremental/fcheck.awk"))
            .arg(&delta)
            .arg(&flat)
            .output()
            .unwrap()
    };
    assert_eq!(dirty(&awk(), 0), [0, 1, 2, 3, 4]);

    // Older recorders cannot establish which spellings the kernel followed.
    let obs = root.join("graph/obs");
    let evidence = "V\tsymlink-paths-v1\n";
    fs::write(
        &obs,
        fs::read_to_string(&obs).unwrap().replace(evidence, ""),
    )
    .unwrap();
    assert!(!check(&runner, &root).status.success());
    fs::write(&flat, text.replace(evidence, "")).unwrap();
    assert!(!awk().status.success());
}

#[test]
fn replacing_an_ordinary_ancestor_wakes_a_previously_missing_lookup() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    fs::create_dir_all(root.join("dir/sub")).unwrap();
    fs::create_dir_all(root.join("other/sub")).unwrap();
    fs::write(root.join("dir/sub/module.nix"), "1").unwrap();
    fs::write(root.join("other/sub/module.nix"), "2").unwrap();
    fs::write(root.join("other/sub/child"), "hi").unwrap();
    fs::write(
        root.join("jobs.nix"),
        r#"[
        (builtins.pathExists ./dir/sub/child)
        ((builtins.readDir ./dir/sub) ? child)
        (import ./dir/sub/module.nix)
        (builtins.add 1 1)
    ]"#,
    )
    .unwrap();
    let runner = bin();
    assert_eq!(
        evaluate(&runner, &root, true),
        serde_json::json!([false, false, 1, 2])
    );
    fs::rename(root.join("dir"), root.join("old")).unwrap();
    symlink("other", root.join("dir")).unwrap();
    assert_eq!(
        evaluate(&runner, &root, false),
        serde_json::json!([true, true, 2, 2])
    );
    fs::write(
        root.join("delta"),
        format!("1\tV\t2\n1\tCM\t{}\n", root.join("dir").display()),
    )
    .unwrap();
    assert_eq!(dirty(&check(&runner, &root), 1), [0, 1, 2]);
}

#[test]
fn an_intermediate_retarget_changes_the_import_and_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    for (directory, source) in [("one", "1"), ("two", "2")] {
        fs::create_dir(root.join(directory)).unwrap();
        fs::write(root.join(directory).join("module.nix"), source).unwrap();
    }
    symlink("one", root.join("inner")).unwrap();
    symlink("inner", root.join("outer")).unwrap();
    fs::write(
        root.join("jobs.nix"),
        r#"[ (import ./outer/module.nix)
        (builtins.path { path = ./outer; name = "copied"; })
        (builtins.readFileType ./outer) ]"#,
    )
    .unwrap();
    let runner = bin();
    let old = evaluate(&runner, &root, true);
    fs::remove_file(root.join("inner")).unwrap();
    symlink("two", root.join("inner")).unwrap();
    let fresh = evaluate(&runner, &root, false);
    assert_eq!(old[0], 1);
    assert_eq!(fresh[0], 2);
    assert_ne!(old[1], fresh[1]);
    assert_eq!(old[2], fresh[2]);
    fs::write(
        root.join("delta"),
        format!("1\tV\t2\n1\tCM\t{}\n", root.join("inner").display()),
    )
    .unwrap();
    assert_eq!(dirty(&check(&runner, &root), 1), [0, 1]);
}
