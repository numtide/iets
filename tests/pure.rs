//! Pure evaluation is the native default, and a checkout with no flake is
//! what a pure run may read (docs/explanation/flakes-and-sources.md).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn iets(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_iets"))
        .args(args)
        .current_dir(dir)
        .env("XDG_CONFIG_HOME", "/nonexistent")
        .env("IETS_CACHE", "0")
        .output()
        .expect("run iets")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A project holding `repo/read.nix`, which reads `repo/in.txt`, and
/// `outside.txt` beside the project. The marker is an `iets.toml`: a working
/// tree would need a real SCM here, and `project::tests` covers the markers
/// that `source::scm` supplies.
fn checkout(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("iets-pure-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let repo = d.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("iets.toml"), "").unwrap();
    std::fs::write(d.join("outside.txt"), "out").unwrap();
    std::fs::write(repo.join("in.txt"), "in").unwrap();
    std::fs::write(repo.join("read.nix"), "builtins.readFile ./in.txt").unwrap();
    std::fs::write(repo.join("up.nix"), "builtins.readFile ../outside.txt").unwrap();
    std::fs::write(repo.join("env.nix"), "builtins.getEnv \"IETS_TEST_VAR\"").unwrap();
    d
}

#[test]
fn a_read_in_the_project_is_pure_and_one_above_it_is_not() {
    let d = checkout("root");
    let repo = d.join("repo");

    let o = iets(&repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "in");

    let o = iets(&repo, &["eval", "--raw", "-f", "up.nix"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    let e = text(&o.stderr);
    assert!(
        e.contains("outside.txt' is forbidden in pure evaluation mode"),
        "{e}"
    );

    let o = iets(&repo, &["eval", "--raw", "--impure", "-f", "up.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "out");

    let _ = std::fs::remove_dir_all(&d);
}

/// No marker above the entry file: its own directory is the project, so a
/// one-off expression in a scratch directory still evaluates.
#[test]
fn a_directory_with_no_marker_is_its_own_project() {
    let d = checkout("marker");
    let repo = d.join("repo");
    std::fs::remove_file(repo.join("iets.toml")).unwrap();
    let o = iets(&repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "in");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_other_purity_rules_hold_without_a_flag() {
    let d = checkout("env");
    let repo = d.join("repo");

    let o = iets(&repo, &["eval", "--raw", "-f", "env.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "");

    let o = iets(&repo, &["eval", "-E", "builtins ? currentSystem"]);
    assert_eq!(text(&o.stdout).trim(), "false");

    let _ = std::fs::remove_dir_all(&d);
}

/// The user's file turns it off for every run; a project's file may not.
#[test]
fn the_pure_setting_is_the_users_alone() {
    let d = checkout("setting");
    let repo = d.join("repo");
    let conf = d.join("config/iets");
    std::fs::create_dir_all(&conf).unwrap();
    std::fs::write(conf.join("iets.toml"), "pure = false\n").unwrap();

    let o = Command::new(env!("CARGO_BIN_EXE_iets"))
        .args(["eval", "--raw", "-f", "up.nix"])
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", d.join("config"))
        .env("IETS_CACHE", "0")
        .output()
        .expect("run iets");
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "out");

    std::fs::write(repo.join("iets.toml"), "pure = false\n").unwrap();
    let o = iets(&repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    let e = text(&o.stderr);
    assert!(e.contains("'pure' may not be set in a project file"), "{e}");

    let _ = std::fs::remove_dir_all(&d);
}
