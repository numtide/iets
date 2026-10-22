//! The event stream: the groups a run names, one JSON object per store add,
//! and silence when nobody asked for it.

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;
use std::path::Path;
use std::process::Output;
use std::{env, fs};
use tempfile::tempdir;

/// `groups` is `IETS_EVENTS`; the sink stays stderr, which is its default.
fn eval(bin: &Iets, expr: &str, groups: Option<&str>) -> Output {
    let mut c = bin.cmd();
    c.args(["eval", "--no-cache", "--raw", "-E", expr])
        .env_remove("IETS_EVENTS_TO");
    match groups {
        Some(v) => c.env("IETS_EVENTS", v),
        None => c.env_remove("IETS_EVENTS"),
    };
    c.output().expect("run iets")
}

fn stderr(bin: &Iets, expr: &str, groups: Option<&str>) -> String {
    let o = eval(bin, expr, groups);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const TO_FILE: &str = r#""${builtins.toFile "t" "hi"}""#;

#[rstest]
fn a_text_add_names_its_size_and_its_position(bin: Iets) {
    let err = stderr(&bin, TO_FILE, Some("store"));
    let line = err.lines().next().unwrap_or_default();
    assert!(line.contains(r#""event":"store-add""#), "{err}");
    assert!(line.contains(r#""kind":"text""#), "{err}");
    assert!(line.contains(r#""size":2"#), "{err}");
    assert!(line.contains(r#""from":"text""#), "{err}");
    assert!(line.contains("-t\""), "{err}");
    assert!(line.contains(r#""at":"«string»:1:4""#), "{err}");
}

#[rstest]
fn a_tree_add_names_the_local_path_it_copied(bin: Iets) {
    // A Nix path has forward slashes, and a drive is its first component
    let native = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml").replace('\\', "/");
    let src = if native.starts_with('/') {
        native
    } else {
        format!("/{native}")
    };
    // A path literal cannot hold the colon of a drive
    let err = stderr(&bin, &format!(r#""${{/. + "{src}"}}""#), Some("store"));
    assert!(err.contains(r#""kind":"tree""#), "{err}");
    // `from` is where the file is on disk, written as a JSON string
    let on_disk = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let on_disk = on_disk.display().to_string().replace('\\', "\\\\");
    assert!(err.contains(&format!(r#""from":"{on_disk}""#)), "{err}");
}

#[rstest]
fn all_takes_the_cold_groups(bin: Iets) {
    assert!(stderr(&bin, TO_FILE, Some("all")).contains("store-add"));
    assert!(stderr(&bin, TO_FILE, Some("store,fetch")).contains("store-add"));
}

#[rstest]
fn a_group_nobody_named_says_nothing(bin: Iets) {
    assert_eq!(stderr(&bin, TO_FILE, None), "");
    assert_eq!(stderr(&bin, TO_FILE, Some("fetch")), "");
    assert_eq!(stderr(&bin, TO_FILE, Some("")), "");
}

#[rstest]
fn a_group_that_does_not_exist_is_a_settings_error(bin: Iets) {
    let o = eval(&bin, TO_FILE, Some("store,stor"));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("IETS_EVENTS: 'stor' is not an event group"),
        "{err}"
    );
    assert!(err.contains("all, syntax"), "{err}");
    assert_ne!(o.status.code(), Some(0), "{err}");
}

#[rstest]
fn the_sink_is_its_own_knob(bin: Iets) {
    let dir = tempdir().unwrap();
    let file = dir.path().join("e.jsonl");
    let o = bin
        .cmd()
        .args(["eval", "--no-cache", "--raw", "-E", TO_FILE])
        .env("IETS_EVENTS", "store")
        .env("IETS_EVENTS_TO", &file)
        .output()
        .expect("run iets");
    assert_eq!(o.status.code(), Some(0));
    assert!(
        o.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let written = fs::read_to_string(&file).expect("the events file");
    assert!(written.contains(r#""event":"store-add""#), "{written}");
}
