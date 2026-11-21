//! The serial producer's boundaries must survive concatenation into flat jobs.
#![cfg(unix)]

mod hermetic;
use hermetic::{bin, ok, stdout};
use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn serial_flat_frames_reject_torn_and_repeated_prologues() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("a"), "a").unwrap();
    fs::write(root.join("b"), "b").unwrap();
    fs::write(
        root.join("root.nix"),
        "[ (builtins.readFile ./a) (builtins.readFile ./b) 7 ]",
    )
    .unwrap();
    let jobs = root.join("jobs");
    let whole = root.join("whole");
    let captured = ok(bin()
        .cmd()
        .args(["eval", "--json", "--no-cache", "--workers", "4"])
        .env("IETS_FINE_TRACE", &whole)
        .env("IETS_FINE_JOBS", &jobs)
        .arg(root.join("root.nix")));
    assert_eq!(stdout(&captured), r#"["a","b",7]"#);
    let records: Vec<_> = (0..3)
        .map(|i| fs::read_to_string(jobs.join(i.to_string())).unwrap())
        .collect();
    let start = "V\tserial-v2\n";
    let end = "V\tend-serial-external-v1\n";
    assert!(
        records
            .iter()
            .all(|s| s.starts_with(start) && s.ends_with(end))
    );
    let intact = format!(
        "J\t0\n{}J\t1\n{}J\t2\n{}",
        records[0], records[1], records[2]
    );
    let checker = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/incremental/fcheck.awk");
    let check = |text: &str, changes: &str| {
        fs::write(root.join("flat"), text).unwrap();
        fs::write(root.join("changes"), changes).unwrap();
        Command::new("awk")
            .arg("-f")
            .arg(&checker)
            .arg(root.join("changes"))
            .arg(root.join("flat"))
            .output()
            .unwrap()
    };
    let changes = format!("1\tV\t2\n1\tCM\t{}\n", root.join("b").display());
    let result = check(&intact, &changes);
    assert!(result.status.success());
    assert_eq!(
        stdout(&result)
            .lines()
            .map(|s| s.split('\t').next().unwrap())
            .collect::<Vec<_>>(),
        ["1"]
    );
    assert!(check(&intact, "").status.success());
    for prefix in [
        "",
        "R",
        "R\t?\tvolatile",
        "R\tregion",
        "F\t/path",
        "P\t0\tlit\t",
    ] {
        // The second producer's prologue must not reset a still-open first job,
        // including when the torn observation swallowed its J separator.
        let torn = format!("J\t0\n{start}{prefix}J\t1\n{}", records[1]);
        for changes in [&*changes, ""] {
            assert!(!check(&torn, changes).status.success(), "accepted {torn:?}");
        }
    }
    for bad in [
        intact.replacen(start, "", 1),
        intact.replacen(start, &format!("{start}{start}"), 1),
        intact.replacen(end, "", 1),
        intact.replacen("J\t1\n", "J\t1\textra\n", 1),
        format!("J\t0\n{}", fs::read_to_string(whole).unwrap()),
        format!("J\t0\n{}{end}F\tignored\n", records[0]),
    ] {
        assert!(!check(&bad, "").status.success());
    }
    // Empty observation sets still require both producer-written boundaries.
    assert!(check(&format!("J\t0\n{start}{end}"), "").status.success());
    // Structural framing cannot detect omission of an entire, complete job.
    assert!(check(&format!("J\t1\n{}", records[1]), "").status.success());
}
