//! `iets diff` on two small derivations that differ in an env var, in an
//! input derivation and in an input source: the text, the JSON, the exit
//! codes and the depth limit.

mod hermetic;

use hermetic::{Iets, bin};

use rstest::rstest;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::{TempDir, tempdir};

struct Run {
    rc: i32,
    out: String,
    err: String,
}

fn demo() -> TempDir {
    let work = tempdir().unwrap();
    fs::create_dir_all(work.path().join("data")).expect("create the work directory");
    fs::write(work.path().join("data/one.txt"), "one\n").expect("write one.txt");
    fs::write(work.path().join("data/two.txt"), "two\n").expect("write two.txt");
    fs::write(
        work.path().join("demo.nix"),
        r#"let
  dep = msg: derivation {
    name = "diffdep";
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [ "-c" "echo ${msg} > $out" ];
  };
  top = { msg, greeting, src }: derivation {
    name = "diffdemo";
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [ "-c" "cat ${dep msg} ${src} > $out" ];
    GREETING = greeting;
  };
in {
  a = top { msg = "one"; greeting = "hello"; src = ./data/one.txt; };
  b = top { msg = "two"; greeting = "goodbye"; src = ./data/two.txt; };
  c = top { msg = "one"; greeting = "hello"; src = ./data/one.txt; };
}
"#,
    )
    .expect("write demo.nix");
    work
}

fn diff(bin: &Iets, work: &Path, args: &[&str]) -> Run {
    let o = bin
        .cmd()
        .current_dir(work)
        .arg("diff")
        .args(args)
        .output()
        .expect("run iets");
    Run {
        rc: o.status.code().expect("iets exits"),
        out: String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
        err: String::from_utf8_lossy(&o.stderr).trim_end().to_string(),
    }
}

#[rstest]
fn the_text_names_the_env_the_source_and_the_input_drv(bin: Iets) {
    let w = demo();
    let r = diff(&bin, w.path(), &["-A", "a", "-A", "b", "demo.nix"]);
    assert_eq!(r.rc, 1, "{}", r.out);
    for want in [
        "env GREETING:",
        "- hello",
        "+ goodbye",
        "input source removed:",
        "input source added:",
        "input drv changed: diffdep",
        "\"echo one > $out\"",
    ] {
        assert!(r.out.contains(want), "missing {want:?}:\n{}", r.out);
    }
}

#[rstest]
fn the_json_object_names_the_same_differences(bin: Iets) {
    let w = demo();
    let r = diff(
        &bin,
        w.path(),
        &["-A", "a", "-A", "b", "--json", "demo.nix"],
    );
    assert_eq!(r.rc, 1, "{}", r.out);
    let j: Value = serde_json::from_str(&r.out).expect("JSON on stdout");
    assert_eq!(j["equal"], false);
    let changes = j["changes"].as_array().expect("changes");
    assert!(
        changes
            .iter()
            .any(|c| c["what"] == "env" && c["key"] == "GREETING" && c["b"] == "goodbye"),
        "{}",
        r.out
    );
    let sources: Vec<&Value> = changes
        .iter()
        .filter(|c| c["what"] == "input source")
        .collect();
    assert_eq!(
        sources.iter().filter(|c| c["b"].is_null()).count(),
        1,
        "{}",
        r.out
    );
    assert_eq!(
        sources.iter().filter(|c| c["a"].is_null()).count(),
        1,
        "{}",
        r.out
    );
    let inputs = j["inputDrvs"].as_array().expect("inputDrvs");
    assert_eq!(inputs.len(), 1, "{}", r.out);
    let inner = inputs[0]["changes"].as_array().expect("inner changes");
    assert!(
        inner.iter().any(|c| c["what"] == "args"
            && c["b"].as_str().is_some_and(|b| b.contains("echo two"))),
        "{}",
        r.out
    );
}

/// The exit code, and a line of stdout or (for a usage error) stderr.
#[rstest]
#[case::one_side_is_a_usage_error(&["-A", "a"], 2, "error:")]
#[case::an_unknown_flag_is_a_usage_error(&["-A", "a", "-A", "b", "--bogus"], 2, "unknown flag")]
#[case::equal_derivations_exit_0(&["-A", "a", "-A", "c"], 0, "are the same derivation")]
#[case::depth_zero_stops_at_the_input_derivation(&["-A", "a", "-A", "b", "--depth", "0"], 1, "stopped: depth")]
fn exit_code(bin: Iets, #[case] args: &[&str], #[case] rc: i32, #[case] want: &str) {
    let w = demo();
    let r = diff(&bin, w.path(), &[args, &["demo.nix"]].concat());
    let text = if rc == 2 { &r.err } else { &r.out };
    assert_eq!(r.rc, rc, "{}{}", r.out, r.err);
    assert!(text.contains(want), "{text}");
}

#[rstest]
fn two_installables_are_one_evaluation_each(bin: Iets) {
    let w = demo();
    fs::write(w.path().join("a.nix"), "(import ./demo.nix).a\n").expect("write a.nix");
    fs::write(w.path().join("b.nix"), "(import ./demo.nix).b\n").expect("write b.nix");
    let r = diff(&bin, w.path(), &["a.nix", "b.nix"]);
    assert_eq!(r.rc, 1, "{}", r.out);
    assert!(r.out.contains("input drv changed: diffdep"), "{}", r.out);
}

#[rstest]
fn an_env_name_that_repeats_the_derivation_name_is_left_out(bin: Iets) {
    let w = demo();
    fs::write(
        w.path().join("named.nix"),
        r#"let mk = n: derivation {
    name = n;
    system = "x86_64-linux";
    builder = "/bin/sh";
    args = [ "-c" "echo hi > $out" ];
  };
in { one = mk "ux-one"; two = mk "ux-two"; three = mk "ux-one" // { }; }
"#,
    )
    .expect("write named.nix");
    let r = diff(&bin, w.path(), &["-A", "one", "-A", "two", "named.nix"]);
    assert_eq!(r.rc, 1, "{}", r.out);
    assert!(r.out.contains("name: ux-one -> ux-two"), "{}", r.out);
    assert!(!r.out.contains("env name"), "{}", r.out);
}

#[rstest]
fn dash_f_is_the_positional_file(bin: Iets) {
    let w = demo();
    let want = diff(&bin, w.path(), &["-A", "a", "-A", "b", "demo.nix"]);
    for flag in ["-f", "--file"] {
        let r = diff(&bin, w.path(), &["-A", "a", "-A", "b", flag, "demo.nix"]);
        assert_eq!((r.rc, &r.out), (want.rc, &want.out), "{flag}");
    }
}
