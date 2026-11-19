//! Attribute equality forces pairs in interning order. An earlier import can
//! change the order even when both operands have identical source across runs.
mod hermetic;

use hermetic::{bin, ok, stdout};
use rstest::rstest;
use std::fs;

#[rstest]
#[case(false, "")]
#[case(false, "c=0; d=0; e=0; f=0; g=0; h=0;")]
#[case(true, "")]
fn attr_order_invalidates_the_comparison(#[case] earlier_import: bool, #[case] extra: &str) {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path();
    let iets = bin();
    let root = dir.join("root.nix");
    let left = dir.join("left.nix");
    let seed = dir.join("seed.nix");
    fs::write(
        &root,
        r#"[ (builtins.seq (import ./seed.nix)
        (builtins.tryEval ((import ./left.nix) == (import ./right.nix))).success)
        ({ safe = 1; } == { safe = 1; }) ]"#,
    )
    .unwrap();
    fs::write(
        dir.join("right.nix"),
        format!("{{ aaaxyz=2; bbbxyz=0; {extra} }}"),
    )
    .unwrap();
    let edit = |reverse| {
        let binds = if reverse {
            r#"bbbxyz=throw "oops"; aaaxyz=1;"#
        } else {
            r#"aaaxyz=1; bbbxyz=throw "oops";"#
        };
        if earlier_import {
            fs::write(&seed, format!("let unused = {{ {binds} }}; in 0")).unwrap();
            fs::write(&left, r#"{ aaaxyz=1; bbbxyz=throw "oops"; }"#).unwrap();
        } else {
            fs::write(&seed, "0").unwrap();
            fs::write(&left, format!("{{ {binds} {extra} }}")).unwrap();
        }
    };
    let dag = dir.join("dag");
    let eval = |trace| {
        let mut cmd = iets.cmd();
        cmd.args(["eval", "--strict", "--no-cache", "--workers", "1"])
            .arg(&root);
        if trace {
            cmd.env("IETS_FINE_TRACE", "").env("IETS_FINE_DAG", &dag);
        }
        stdout(&ok(&mut cmd))
    };
    let file = if earlier_import { &seed } else { &left };
    let regions = || stdout(&ok(iets.cmd().arg("fine-regions").arg(file)));
    let changes = dir.join("changes");
    // A reorder leaves the region and key tables alone; all that the normal
    // diff reports is a changed file, tree and position.
    fs::write(
        &changes,
        format!(
            "1\tV\t2\n1\tCM\t{}\n1\tCT\t{}\n1\tCX\t{}\t0\n",
            file.display(),
            file.display(),
            file.display()
        ),
    )
    .unwrap();
    for reverse in [false, true] {
        edit(reverse);
        let old = regions();
        let result = eval(true);
        edit(!reverse);
        assert_eq!(old, regions());
        let fresh = eval(false);
        let (a, b) = if reverse {
            ("false", "true")
        } else {
            ("true", "false")
        };
        assert_eq!(result, format!("[ {a} true ]"));
        assert_eq!(fresh, format!("[ {b} true ]"));
        assert_eq!(
            stdout(&ok(iets.cmd().arg("fine-check").arg(&changes).arg(&dag))),
            format!("{}\t0\t1", dag.display())
        );
    }
}

#[test]
fn subset_replay_reports_order_dependence() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path();
    let iets = bin();
    let dag = dir.join("dag");
    let root = dir.join("jobs.nix");
    let run = |jobs| {
        fs::write(&root, jobs).unwrap();
        stdout(&ok(iets
            .cmd()
            .env("IETS_FINE_TRACE", "")
            .env("IETS_FINE_DAG", &dag)
            .args(["eval", "--strict", "--no-cache", "--workers", "1"])
            .arg(&root)))
    };
    let check = || {
        iets.cmd()
            .args(["fine-check", "--require-stable-attr-order"])
            .arg(&dag)
            .output()
            .unwrap()
    };

    fs::write(
        dir.join("seed.nix"),
        "let unused = { bbbxyz=0; aaaxyz=0; }; in 0",
    )
    .unwrap();
    fs::write(dir.join("compare.nix"),
        r#"(builtins.tryEval ({ aaaxyz=1; bbbxyz=throw "oops"; } == { aaaxyz=2; bbbxyz=0; })).success"#).unwrap();
    assert_eq!(
        run("[ (import ./seed.nix) (import ./compare.nix) ]"),
        "[ 0 false ]"
    );
    assert!(!check().status.success());
    assert_eq!(run("[ (import ./compare.nix) ]"), "[ true ]");
    let rejected = check();
    assert!(!rejected.status.success());
    assert!(hermetic::stderr(&rejected).contains("evaluate the original batch in order"));

    // The built-in symbol 'name' always precedes the two source-defined
    // names. A mismatch there cannot force either tail in any batch.
    assert_eq!(
        run(r#"[ ({ name=1; aaaxyz=throw "oops"; bbbxyz=0; } ==
                        { name=2; aaaxyz=0; bbbxyz=0; })
                        ({ aaaxyz=1; bbbxyz=2; } == { bbbxyz=1; aaaxyz=2; }) ]"#),
        "[ false false ]"
    );
    assert!(check().status.success());

    let path = dag.join("nodes");
    let mut bytes = fs::read(&path).unwrap();
    bytes[4..8].fill(0); // unversioned trace from before the order observation
    fs::write(path, bytes).unwrap();
    let legacy = check();
    assert!(!legacy.status.success());
    assert!(hermetic::stderr(&legacy).contains("record the graph again"));
}
