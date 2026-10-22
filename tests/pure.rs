//! Pure evaluation is the native default, and a checkout with no flake is
//! what a pure run may read (docs/explanation/flakes-and-sources.md).

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;

use std::fs;
use std::path::Path;
use std::process::Output;
use tempfile::{TempDir, tempdir};

fn run(bin: &Iets, dir: &Path, args: &[&str]) -> Output {
    bin.cmd()
        .args(args)
        .current_dir(dir)
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
fn checkout() -> TempDir {
    let d = tempdir().unwrap();
    let repo = d.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("iets.toml"), "").unwrap();
    fs::write(d.path().join("outside.txt"), "out").unwrap();
    fs::write(repo.join("in.txt"), "in").unwrap();
    fs::write(repo.join("read.nix"), "builtins.readFile ./in.txt").unwrap();
    fs::write(repo.join("up.nix"), "builtins.readFile ../outside.txt").unwrap();
    fs::write(repo.join("env.nix"), "builtins.getEnv \"IETS_TEST_VAR\"").unwrap();
    d
}

#[rstest]
fn a_read_in_the_project_is_pure_and_one_above_it_is_not(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");

    let o = run(&bin, &repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "in");

    let o = run(&bin, &repo, &["eval", "--raw", "-f", "up.nix"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    let e = text(&o.stderr);
    assert!(
        e.contains("outside.txt' is forbidden in pure evaluation mode"),
        "{e}"
    );

    let o = run(&bin, &repo, &["eval", "--raw", "--impure", "-f", "up.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "out");
}

/// No marker above the entry file: its own directory is the project, so a
/// one-off expression in a scratch directory still evaluates.
#[rstest]
fn a_directory_with_no_marker_is_its_own_project(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");
    fs::remove_file(repo.join("iets.toml")).unwrap();
    let o = run(&bin, &repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "in");
}

#[rstest]
fn the_other_purity_rules_hold_without_a_flag(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");

    let o = run(&bin, &repo, &["eval", "--raw", "-f", "env.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "");

    let o = run(&bin, &repo, &["eval", "-E", "builtins ? currentSystem"]);
    assert_eq!(text(&o.stdout).trim(), "false");
}

/// The user's file turns it off for every run; a project's file may not.
#[rstest]
fn the_pure_setting_is_the_users_alone(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");
    let conf = d.path().join("config/iets");
    fs::create_dir_all(&conf).unwrap();
    fs::write(conf.join("iets.toml"), "pure = false\n").unwrap();

    let o = bin
        .cmd()
        .args(["eval", "--raw", "-f", "up.nix"])
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", d.path().join("config"))
        .env("IETS_CACHE", "0")
        .output()
        .expect("run iets");
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "out");

    fs::write(repo.join("iets.toml"), "pure = false\n").unwrap();
    let o = run(&bin, &repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    let e = text(&o.stderr);
    assert!(e.contains("'pure' may not be set in a project file"), "{e}");
}

/// A `narHash` locks a `path:` reference, so a pure evaluation takes it when
/// the tree matches, refuses a tree that does not, and refuses no hash at all.
#[rstest]
fn get_flake_takes_a_path_its_nar_hash_locks(bin: Iets) {
    let d = tempdir().unwrap();
    fs::write(d.path().join("flake.nix"), "{ outputs = _: { x = 42; }; }").unwrap();
    fs::write(
        d.path().join("flake.lock"),
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    )
    .unwrap();
    // the evaluator's form, `/C:/...` on Windows, keeps backslashes out of the string
    let at = String::from_utf8(iets::sys::path_to_internal(d.path())).unwrap();

    let tree = format!(r#"(builtins.fetchTree {{ type = "path"; path = "{at}"; }}).narHash"#);
    let o = run(
        &bin,
        d.path(),
        &["eval", "--impure", "--raw", "--expr", &tree],
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let hash = text(&o.stdout);

    let get = |query: &str| {
        let e = format!(r#"(builtins.getFlake "path:{at}{query}").x"#);
        run(&bin, d.path(), &["eval", "--expr", &e])
    };
    let right = get(&format!("?narHash={hash}"));
    let wrong = get("?narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    let unpinned = get("");

    assert_eq!(right.status.code(), Some(0), "{}", text(&right.stderr));
    assert_eq!(text(&right.stdout), "42\n");
    assert_eq!(wrong.status.code(), Some(1), "{}", text(&wrong.stdout));
    let e = text(&wrong.stderr);
    assert!(e.contains("NAR hash mismatch"), "{e}");
    let e = text(&unpinned.stderr);
    assert!(e.contains("unlocked flake reference"), "{e}");
}
