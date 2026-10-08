//! The event stream: the groups a run names, one JSON object per store add,
//! and silence when nobody asked for it.

use std::process::{Command, Output};
use std::{env, fs};

/// `groups` is `IETS_EVENTS`; the sink stays stderr, which is its default.
fn eval(expr: &str, groups: Option<&str>) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_iets"));
    c.args(["eval", "--no-cache", "--raw", "-E", expr])
        .env("XDG_CONFIG_HOME", "/nonexistent")
        .env_remove("IETS_EVENTS_TO");
    match groups {
        Some(v) => c.env("IETS_EVENTS", v),
        None => c.env_remove("IETS_EVENTS"),
    };
    c.output().expect("run iets")
}

fn stderr(expr: &str, groups: Option<&str>) -> String {
    let o = eval(expr, groups);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const TO_FILE: &str = r#""${builtins.toFile "t" "hi"}""#;

#[test]
fn a_text_add_names_its_size_and_its_position() {
    let err = stderr(TO_FILE, Some("store"));
    let line = err.lines().next().unwrap_or_default();
    assert!(line.contains(r#""event":"store-add""#), "{err}");
    assert!(line.contains(r#""kind":"text""#), "{err}");
    assert!(line.contains(r#""size":2"#), "{err}");
    assert!(line.contains(r#""from":"text""#), "{err}");
    assert!(line.contains("-t\""), "{err}");
    assert!(line.contains(r#""at":"«string»:1:4""#), "{err}");
}

#[test]
fn a_tree_add_names_the_local_path_it_copied() {
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let err = stderr(&format!(r#""${{{src}}}""#), Some("store"));
    assert!(err.contains(r#""kind":"tree""#), "{err}");
    assert!(err.contains(&format!(r#""from":"{src}""#)), "{err}");
}

#[test]
fn all_takes_the_cold_groups() {
    assert!(stderr(TO_FILE, Some("all")).contains("store-add"));
    assert!(stderr(TO_FILE, Some("store,fetch")).contains("store-add"));
}

#[test]
fn a_group_nobody_named_says_nothing() {
    assert_eq!(stderr(TO_FILE, None), "");
    assert_eq!(stderr(TO_FILE, Some("fetch")), "");
    assert_eq!(stderr(TO_FILE, Some("")), "");
}

#[test]
fn a_group_that_does_not_exist_is_a_settings_error() {
    let o = eval(TO_FILE, Some("store,stor"));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("IETS_EVENTS: 'stor' is not an event group"),
        "{err}"
    );
    assert!(err.contains("all, syntax"), "{err}");
    assert_ne!(o.status.code(), Some(0), "{err}");
}

#[test]
fn the_sink_is_its_own_knob() {
    let dir = env::temp_dir().join(format!("iets-events-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("e.jsonl");
    let o = Command::new(env!("CARGO_BIN_EXE_iets"))
        .args(["eval", "--no-cache", "--raw", "-E", TO_FILE])
        .env("XDG_CONFIG_HOME", "/nonexistent")
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
    let _ = fs::remove_dir_all(&dir);
}
