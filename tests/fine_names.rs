//! The fine trace, the region differ and the checker must agree on byte names.
#![cfg(unix)]

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(cmd: &mut Command) -> String {
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Fixture {
    dir: tempfile::TempDir,
    defs: PathBuf,
    dag: PathBuf,
}

impl Fixture {
    fn new(bin: &Iets, jobs: impl AsRef<[u8]>, defs: impl AsRef<[u8]>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let source = path.join("fine names\t");
        fs::create_dir(&source).unwrap();
        let root = source.join("jobs.nix");
        let file = source.join("defs.nix");
        let dag = path.join("dag");
        fs::write(&root, jobs).unwrap();
        fs::write(&file, defs).unwrap();
        run(bin
            .cmd()
            .env("IETS_FINE_TRACE", "")
            .env("IETS_FINE_DAG", &dag)
            .args(["eval", "--strict", "--no-cache", "--workers", "1"])
            .arg(&root));
        fs::write(
            path.join("old"),
            run(bin.cmd().arg("fine-regions").arg(&file)),
        )
        .unwrap();
        Self {
            dir,
            defs: file,
            dag,
        }
    }

    fn check(&self, bin: &Iets, defs: impl AsRef<[u8]>) -> Vec<usize> {
        fs::write(&self.defs, defs).unwrap();
        let path = self.dir.path();
        fs::write(
            path.join("new"),
            run(bin.cmd().arg("fine-regions").arg(&self.defs)),
        )
        .unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/incremental/fcmp.awk");
        let delta = run(Command::new("awk")
            .current_dir(path)
            .arg("-f")
            .arg(script)
            .args(["old", "new"]));
        let mut changes = String::from("1\tV\t3\n");
        for line in delta.lines() {
            changes.push_str(&format!("1\t{line}\n"));
        }
        let file = path.join("changes");
        fs::write(&file, changes).unwrap();
        run(bin.cmd().arg("fine-check").arg(&file).arg(&self.dag))
            .lines()
            .map(|s| s.split('\t').nth(1).unwrap().parse().unwrap())
            .collect()
    }
}

const PADDING: &str = "q0=0; q1=1; q2=2; q3=3; q4=4; q5=5; q6=6; q7=7;";

#[rstest]
fn non_utf8_attribute_bytes_are_distinct(bin: Iets) {
    let f = Fixture::new(
        &bin,
        b"let p=import ./defs.nix; in [ (p ? \"\xff\") (p ? \"\xfe\") ]",
        format!("{{ {PADDING} }}"),
    );
    for (job, byte) in [0xff, 0xfe].into_iter().enumerate() {
        let mut defs = format!("{{ {PADDING} \"").into_bytes();
        defs.push(byte);
        defs.extend_from_slice(b"\"=0; }");
        assert_eq!(f.check(&bin, defs), [job]);
    }
}

#[rstest]
fn changed_literal_keys_reach_only_their_lookups(bin: Iets) {
    let jobs = r#"let p = import ./defs.nix; in [
        (p ? "a b") (p ? "a\tb") (p ? "a\nb") (p ? "") (p ? "*")
        (p ? "~") (p ? "a") (p ? "b") (p ? "a\\nb") ]"#;
    let f = Fixture::new(&bin, jobs, format!("{{ {PADDING} }}"));
    for (job, name) in [
        r#""a b""#,
        r#""a\tb""#,
        r#""a\nb""#,
        r#""""#,
        r#""*""#,
        r#""~""#,
        r#""a""#,
        r#""b""#,
        r#""a\\nb""#,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            f.check(&bin, format!("{{ {PADDING} {name}=0; }}")),
            [job],
            "{name}"
        );
    }
}

#[rstest]
fn nested_and_slash_containing_regions_are_distinct(bin: Iets) {
    let source =
        |nested, flat| format!("{{ a={{x={nested}; {PADDING}}}; \"a/0.x\"={flat}; {PADDING} }}");
    let f = Fixture::new(
        &bin,
        r#"let p = import ./defs.nix; in [ (p."a/0.x" + 0) (p.a.x + 0) ]"#,
        source(1, 2),
    );
    assert_eq!(f.check(&bin, source(3, 2)), [1]);
    assert_eq!(f.check(&bin, source(1, 4)), [0]);

    let changes = f.dir.path().join("changes");
    fs::write(&changes, "1\tCF\tdefs.nix\n").unwrap();
    let check = || {
        bin.cmd()
            .arg("fine-check")
            .arg(&changes)
            .arg(&f.dag)
            .output()
            .unwrap()
    };
    assert!(
        !check().status.success(),
        "legacy changes must be regenerated"
    );
    fs::write(&changes, "1\tV\t3\n").unwrap();
    let node_file = f.dag.join("nodes");
    let mut data = fs::read(&node_file).unwrap();
    data[..4].fill(0);
    fs::write(node_file, data).unwrap();
    assert!(!check().status.success(), "legacy DAG must be regenerated");
}

#[rstest]
fn git_directory_changes_reach_negative_lookups(bin: Iets) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let repo = home.join("incr/nixpkgs");
    let base = home.join("incr/base");
    let traces = home.join("traces");
    let dag = home.join("dag");
    for root in [&repo, &base] {
        for name in ["removed", "replaced", "surviving"] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(root.join(name).join("child"), "").unwrap();
            fs::write(root.join(name).join("keep"), "").unwrap();
        }
    }
    fs::create_dir(&traces).unwrap();
    fs::write(
        base.join("jobs.nix"),
        "let r=builtins.readDir; a=r ./removed; b=r ./replaced; c=r ./surviving; in [
         (a ? child) (a ? absent) (builtins.attrNames a)
         (b ? child) (b ? absent) (builtins.attrNames b)
         (c ? child) (c ? absent) (builtins.attrNames c) ]",
    )
    .unwrap();
    run(bin
        .cmd()
        .env("IETS_FINE_TRACE", "")
        .env("IETS_FINE_JOBS", &traces)
        .env("IETS_FINE_DAG", &dag)
        .args(["eval", "--strict", "--no-cache", "--workers", "1"])
        .arg(base.join("jobs.nix")));
    let git = |args: &[&str]| {
        run(Command::new("git")
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args))
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    // The generator's historical range includes 101 first-parent revisions.
    for i in 0..101 {
        if i == 100 {
            fs::remove_dir_all(repo.join("removed")).unwrap();
            fs::remove_dir_all(repo.join("replaced")).unwrap();
            fs::write(repo.join("replaced"), "now a file").unwrap();
            fs::remove_file(repo.join("surviving/child")).unwrap();
            git(&["add", "-A"]);
        }
        git(&["commit", "-qm", "fixture", "--allow-empty"]);
        if i == 1 {
            git(&["tag", "9dbd131e6c65"]);
        }
    }
    git(&["tag", "4e63084a455b"]);
    let incremental = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/incremental");
    let changes_dir = home.join("incr/fchg");
    fs::create_dir(&changes_dir).unwrap();
    // The generator must replace pre-fix persisted changes.
    fs::write(changes_dir.join("100.txt"), "V\t2\n").unwrap();
    run(Command::new("bash")
        .env("HOME", home)
        .arg(incremental.join("fchg.sh")));
    let delta = fs::read_to_string(changes_dir.join("100.txt")).unwrap();
    assert!(delta.lines().any(|line| line == "V\t3"));
    let changes = home.join("changes");
    fs::write(
        &changes,
        delta
            .lines()
            .map(|s| format!("1\t{s}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let expected = [0, 1, 2, 3, 4, 5, 6, 8];
    let actual = run(bin.cmd().arg("fine-check").arg(&changes).arg(&dag));
    let mut jobs: Vec<usize> = actual
        .lines()
        .map(|s| s.split('\t').nth(1).unwrap().parse().unwrap())
        .collect();
    jobs.sort_unstable();
    assert_eq!(jobs, expected);
    let flat = home.join("flat");
    fs::write(
        &flat,
        (0..9)
            .map(|i| {
                format!(
                    "J\t{i}\n{}",
                    fs::read_to_string(traces.join(i.to_string())).unwrap()
                )
            })
            .collect::<String>(),
    )
    .unwrap();
    let awk = || {
        let mut cmd = Command::new("awk");
        cmd.arg("-f")
            .arg(incremental.join("fcheck.awk"))
            .arg(&changes)
            .arg(&flat);
        cmd
    };
    let actual = run(&mut awk());
    let mut jobs: Vec<usize> = actual
        .lines()
        .map(|s| s.split('\t').next().unwrap().parse().unwrap())
        .collect();
    jobs.sort_unstable();
    assert_eq!(jobs, expected);
    // Direct consumers must reject pre-fix evidence too, without regenerating it.
    fs::write(&changes, "1\tV\t2\n").unwrap();
    assert!(
        !bin.cmd()
            .arg("fine-check")
            .arg(&changes)
            .arg(&dag)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!awk().output().unwrap().status.success());
    // Exercise each replay's actual error-handling code with the real checker.
    fs::copy(&changes, home.join("step.chg")).unwrap();
    fs::write(home.join("owners"), format!("job\t{}\n", dag.display())).unwrap();
    let replay = fs::read_to_string(incremental.join("replay.sh")).unwrap();
    let check = replay
        .lines()
        .find(|line| line.trim_start().starts_with("$I fine-check "))
        .unwrap();
    let replay2 = fs::read_to_string(incremental.join("replay2.sh")).unwrap();
    let start = replay2.find("srv() {").unwrap();
    let end = start + replay2[start..].find("\n}\n").unwrap() + 3;
    let server = format!(
        "coproc SRV {{ exec \"$I\" fine-check --serve; }}\npid=$SRV_PID\ntrap 'kill $pid 2>/dev/null || :; wait $pid 2>/dev/null || :' EXIT\n{}\nsrv \"load $DAG 0\"\nsrv \"check $R/step.chg $R/chk\"",
        &replay2[start..end]
    );
    let marker = home.join("continued");
    for code in [check, &server] {
        let output = Command::new("bash")
            .env("I", env!("CARGO_BIN_EXE_iets"))
            .env("R", home)
            .env("DAG", &dag)
            .env("MARK", &marker)
            .args(["-uc", &format!("{code}\nprintf bad > \"$MARK\"")])
            .output()
            .unwrap();
        assert!(!output.status.success(), "replay accepted stale changes");
        assert!(!marker.exists(), "replay continued after a checker error");
    }
}
