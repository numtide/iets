//! Pure evaluation is the native default, and a checkout with no flake is
//! what a pure run may read (docs/explanation/flakes-and-sources.md).

mod hermetic;

use hermetic::{Iets, bin, stderr, stdout};
use rstest::rstest;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::{TempDir, tempdir};

/// `p` resolved. A project root is the cwd's text as given, so a test runs
/// iets here and names files below it: `/var` is a link on macOS, `TEMP` a
/// short name on Windows runners.
fn resolved(p: &Path) -> PathBuf {
    iets::sys::fs::canonicalize(p).unwrap()
}

/// The evaluator's form of `p`: `/C:/...` on Windows, which keeps
/// backslashes out of the string.
fn internal(p: &Path) -> String {
    String::from_utf8(iets::sys::path_to_internal(p)).unwrap()
}

fn run(bin: &Iets, dir: &Path, args: &[&str]) -> Output {
    bin.cmd()
        .args(args)
        .current_dir(dir)
        .env("IETS_CACHE", "0")
        .output()
        .expect("run iets")
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
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "in");

    let o = run(&bin, &repo, &["eval", "--raw", "-f", "up.nix"]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(
        e.contains("outside.txt' is forbidden in pure evaluation mode"),
        "{e}"
    );

    let o = run(&bin, &repo, &["eval", "--raw", "--impure", "-f", "up.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "out");
}

/// A link or a `..` that leads out of the project is read as what it reaches.
#[cfg(unix)]
#[rstest]
#[case::file_link("builtins.readFile ./file-link")]
#[case::dir_link("builtins.readFile ./dir-link/outside.txt")]
#[case::import_link("import ./nix-link")]
#[case::home_dots("builtins.readFile ~/repo/../outside.txt")]
#[case::path_exists("if builtins.pathExists ./dir-link/outside.txt then \"\" else 0")]
fn a_link_out_of_the_project_is_forbidden(bin: Iets, #[case] expr: &str) {
    let d = checkout();
    let repo = d.path().join("repo");
    fs::write(d.path().join("outside.nix"), "1").unwrap();
    symlink("../outside.txt", repo.join("file-link")).unwrap();
    symlink("..", repo.join("dir-link")).unwrap();
    symlink("../outside.nix", repo.join("nix-link")).unwrap();
    fs::write(repo.join("e.nix"), expr).unwrap();
    let o = bin
        .cmd()
        .args(["eval", "--raw", "-f", "e.nix"])
        .current_dir(&repo)
        .env("HOME", d.path())
        .env("IETS_CACHE", "0")
        .output()
        .expect("run iets");
    if expr.contains("pathExists") {
        assert!(
            stderr(&o).contains("cannot coerce an integer"),
            "{}",
            stderr(&o)
        );
        return;
    }
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(e.contains("is forbidden in pure evaluation mode"), "{e}");
}

/// No marker above the entry file: its own directory is the project, so a
/// one-off expression in a scratch directory still evaluates.
#[rstest]
fn a_directory_with_no_marker_is_its_own_project(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");
    fs::remove_file(repo.join("iets.toml")).unwrap();
    let o = run(&bin, &repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "in");
}

#[rstest]
fn the_other_purity_rules_hold_without_a_flag(bin: Iets) {
    let d = checkout();
    let repo = d.path().join("repo");

    let o = run(&bin, &repo, &["eval", "--raw", "-f", "env.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "");

    let o = run(&bin, &repo, &["eval", "-E", "builtins ? currentSystem"]);
    assert_eq!(stdout(&o), "false");
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
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "out");

    fs::write(repo.join("iets.toml"), "pure = false\n").unwrap();
    let o = run(&bin, &repo, &["eval", "--raw", "-f", "read.nix"]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(e.contains("'pure' may not be set in a project file"), "{e}");
}

/// A `narHash` locks a `path:` reference, so a pure evaluation takes it when
/// the tree matches, refuses a tree that does not, and refuses no hash at all.
#[rstest]
fn get_flake_takes_a_path_its_nar_hash_locks(bin: Iets) {
    let d = tempdir().unwrap();
    let dir = resolved(d.path());
    fs::write(dir.join("flake.nix"), "{ outputs = _: { x = 42; }; }").unwrap();
    fs::write(
        dir.join("flake.lock"),
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    )
    .unwrap();
    let at = internal(&dir);

    let tree = format!(r#"(builtins.fetchTree {{ type = "path"; path = "{at}"; }}).narHash"#);
    let o = run(&bin, &dir, &["eval", "--impure", "--raw", "--expr", &tree]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let hash = stdout(&o);

    let get = |query: &str| {
        let e = format!(r#"(builtins.getFlake "path:{at}{query}").x"#);
        run(&bin, &dir, &["eval", "--expr", &e])
    };
    let right = get(&format!("?narHash={hash}"));
    let wrong = get("?narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    let unpinned = get("");

    assert_eq!(right.status.code(), Some(0), "{}", stderr(&right));
    assert_eq!(right.stdout, "42\n".as_bytes());
    assert_eq!(wrong.status.code(), Some(1), "{}", stdout(&wrong));
    let e = stderr(&wrong);
    assert!(e.contains("NAR hash mismatch"), "{e}");
    let e = stderr(&unpinned);
    assert!(e.contains("unlocked flake reference"), "{e}");

    let tree = format!(r#"builtins.fetchTree {{ type = "path"; path = "{at}"; narHash = true; }}"#);
    let o = run(&bin, &dir, &["eval", "--expr", &tree]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(
        e.contains("input attribute 'narHash' is not a string"),
        "{e}"
    );
}

const ZERO: &str = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const FORBIDDEN: &str = "is forbidden in pure evaluation mode";

/// [`checkout`] with the links a change from a stranger could carry:
/// `out.txt` and `up` lead out of the project, `in-link.txt` and `sub-link`
/// stay in it.
#[cfg(unix)]
fn linked() -> TempDir {
    let d = checkout();
    let repo = d.path().join("repo");
    fs::create_dir(repo.join("sub")).unwrap();
    fs::write(repo.join("sub/in.txt"), "in").unwrap();
    fs::write(repo.join("sub/one.nix"), "1").unwrap();
    fs::write(d.path().join("two.nix"), "2").unwrap();
    symlink("../outside.txt", repo.join("out.txt")).unwrap();
    symlink(d.path().join("two.nix"), repo.join("two.nix")).unwrap();
    symlink("..", repo.join("up")).unwrap();
    symlink("sub/in.txt", repo.join("in-link.txt")).unwrap();
    symlink("sub", repo.join("sub-link")).unwrap();
    d
}

/// A project is an allow list judged on the text of a path, so a link below
/// it is resolved on the text too: the kernel would follow it out
/// (docs/explanation/pure-eval-and-local-sources.md).
#[cfg(unix)]
#[rstest]
#[case::read_file("builtins.readFile ./out.txt")]
#[case::below_a_link("builtins.readFile ./up/outside.txt")]
#[case::read_dir("builtins.readDir ./up")]
#[case::file_type("builtins.readFileType ./up/outside.txt")]
#[case::hash_file(r#"builtins.hashFile "sha256" ./out.txt"#)]
#[case::import("import ./two.nix")]
#[case::import_below_a_link("import ./up/two.nix")]
#[case::copy_below_a_link(r#"builtins.readFile "${./up/outside.txt}""#)]
#[case::copy_below_a_link_of_a_tree(
    r#""${/. + builtins.unsafeDiscardStringContext (builtins.path { path = ./.; }) + "/up/outside.txt"}""#
)]
#[case::path(r#"builtins.readFile (builtins.path { path = ./out.txt; })"#)]
fn a_link_out_of_the_project_is_not_followed(bin: Iets, #[case] expr: &str) {
    let d = linked();
    let repo = d.path().join("repo");
    fs::write(repo.join("e.nix"), expr).unwrap();
    let o = run(&bin, &repo, &["eval", "--json", "-f", "e.nix"]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(e.contains(FORBIDDEN), "{e}");

    let o = run(&bin, &repo, &["eval", "--json", "--impure", "-f", "e.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
}

#[cfg(unix)]
#[rstest]
#[case::read_file("builtins.readFile ./in-link.txt", r#""in""#)]
#[case::below_a_link("builtins.readFile ./sub-link/in.txt", r#""in""#)]
#[case::import("import ./sub-link/one.nix", "1")]
#[case::exists_inside("builtins.pathExists ./sub-link/in.txt", "true")]
#[case::exists_outside("builtins.pathExists ./up/outside.txt", "false")]
#[case::a_link_is_copied_as_a_link(r#"builtins.readFileType "${./up}""#, r#""symlink""#)]
fn a_link_inside_the_project_is_followed(bin: Iets, #[case] expr: &str, #[case] want: &str) {
    let d = linked();
    let repo = d.path().join("repo");
    fs::write(repo.join("e.nix"), expr).unwrap();
    let o = run(&bin, &repo, &["eval", "--json", "-f", "e.nix"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), want);
}

/// A fetch from the local file system is a read: a hash says what the bytes
/// are, not where a pure evaluation may take them from. `@UP@` is the
/// directory above the project and `@REPO@` the project.
#[rstest]
#[case::fetchurl(r#"builtins.fetchurl { url = "file://@UP@/outside.txt"; sha256 = ""; }"#)]
#[case::fetchurl_of_a_missing_file(
    r#"builtins.fetchurl { url = "file://@UP@/nothing"; sha256 = ""; }"#
)]
#[case::fetch_tarball(
    r#"builtins.fetchTarball { url = "file://@UP@/outside.txt"; sha256 = "@Z@"; }"#
)]
#[case::tarball_input(
    r#"builtins.fetchTree { type = "tarball"; url = "file://@UP@/outside.txt"; narHash = "@Z@"; }"#
)]
#[case::path_input(r#"builtins.fetchTree { type = "path"; path = "@UP@"; narHash = "@Z@"; }"#)]
#[case::get_flake(r#"builtins.getFlake "path:@UP@?narHash=@Z@""#)]
#[case::fetch_git(
    r#"builtins.fetchGit { url = "file://@UP@"; rev = "0000000000000000000000000000000000000001"; }"#
)]
#[case::git_input(
    r#"builtins.fetchTree { type = "git"; url = "file://@UP@"; rev = "0000000000000000000000000000000000000001"; narHash = "@Z@"; }"#
)]
#[cfg_attr(
    unix,
    case::fetchurl_through_a_link(
        r#"builtins.fetchurl { url = "file://@REPO@/out.txt"; sha256 = ""; }"#
    )
)]
#[cfg_attr(
    unix,
    case::path_input_through_a_link(
        r#"builtins.fetchTree { type = "path"; path = "@REPO@/up"; narHash = "@Z@"; }"#
    )
)]
fn a_local_fetch_out_of_the_project_is_refused(bin: Iets, #[case] expr: &str) {
    #[cfg(unix)]
    let d = linked();
    #[cfg(not(unix))]
    let d = checkout();
    let repo = d.path().join("repo");
    let internal = |p: &Path| String::from_utf8(iets::sys::path_to_internal(p)).unwrap();
    let expr = expr
        .replace("@UP@", &internal(d.path()))
        .replace("@REPO@", &internal(&repo))
        .replace("@Z@", ZERO);
    let o = bin
        .cmd()
        .args(["eval", "--expr", &expr])
        .current_dir(&repo)
        .env("IETS_CACHE", "0")
        .env("IETS_CACHE_DIR", d.path().join("cache"))
        .output()
        .expect("run iets");
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(e.contains(FORBIDDEN), "{e}");
    // neither the hash of the file nor whether it is there
    assert!(!e.contains("got"), "{e}");
    assert!(!e.contains("No such file"), "{e}");
}

/// Inside the project the same fetch is a read the evaluation may do, so it
/// goes on to the hash.
#[rstest]
fn a_local_fetch_in_the_project_reads_the_file(bin: Iets) {
    let d = checkout();
    let repo = resolved(d.path()).join("repo");
    let at = internal(&repo);
    let eval = |expr: &str| {
        bin.cmd()
            .args(["eval", "--raw", "--expr", expr])
            .current_dir(&repo)
            .env("IETS_CACHE", "0")
            .env("IETS_CACHE_DIR", d.path().join("cache"))
            .output()
            .expect("run iets")
    };
    let o = eval(&format!(
        r#"builtins.fetchurl {{ url = "file://{at}/in.txt"; sha256 = ""; }}"#
    ));
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    // sha256 of "in"
    let hash = "sha256-WClnU00PkJ0Za5f55pITQnd66oe0b6Ut8WU4nbH7jM8=";
    assert!(
        e.contains("got:       sha256:1kwczfqrsf35y4nsavxlhzm7lxs22f9fdycpdccrv40g9m9nfaaq"),
        "{e}"
    );

    let o = eval(&format!(
        r#"builtins.readFile (builtins.fetchurl {{ url = "file://{at}/in.txt"; sha256 = "{hash}"; }})"#
    ));
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "in");

    // the input store holds the file now, and the check still comes first:
    // a run with another project gets no bytes for knowing the hash
    let elsewhere = d.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    let o = bin
        .cmd()
        .args(["eval", "--raw", "--expr"])
        .arg(format!(
            r#"builtins.fetchurl {{ url = "file://{at}/in.txt"; sha256 = "{hash}"; }}"#
        ))
        .current_dir(&elsewhere)
        .env("IETS_CACHE", "0")
        .env("IETS_CACHE_DIR", d.path().join("cache"))
        .output()
        .expect("run iets");
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let e = stderr(&o);
    assert!(e.contains(FORBIDDEN), "{e}");
}

/// Under a flake the root is the flake's tree, and a fetch below its
/// directory reads through that tree.
#[rstest]
fn a_local_fetch_under_a_flake_stays_in_its_tree(bin: Iets) {
    let d = tempdir().unwrap();
    let dir = resolved(d.path());
    let flake = dir.join("flake");
    fs::create_dir(&flake).unwrap();
    fs::write(dir.join("outside.txt"), "out").unwrap();
    fs::write(flake.join("in.txt"), "in").unwrap();
    let at = internal(&dir);
    let hash = "sha256-WClnU00PkJ0Za5f55pITQnd66oe0b6Ut8WU4nbH7jM8=";
    fs::write(
        flake.join("flake.nix"),
        format!(
            r#"{{ outputs = _: {{
  inside = builtins.readFile (builtins.fetchurl {{ url = "file://{at}/flake/in.txt"; sha256 = "{hash}"; }});
  outside = builtins.fetchurl {{ url = "file://{at}/outside.txt"; sha256 = ""; }};
  tree = builtins.fetchTree {{ type = "path"; path = "{at}"; narHash = "{ZERO}"; }};
}}; }}"#
        ),
    )
    .unwrap();
    let eval = |attr: &str| {
        bin.cmd()
            .args(["eval", "--raw", &format!(".#{attr}")])
            .current_dir(&flake)
            .env("IETS_CACHE", "0")
            .env("IETS_CACHE_DIR", d.path().join("cache"))
            .output()
            .expect("run iets")
    };
    let o = eval("inside");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "in");
    for attr in ["outside", "tree"] {
        let o = eval(attr);
        assert_eq!(o.status.code(), Some(1), "{attr}: {}", stdout(&o));
        let e = stderr(&o);
        assert!(e.contains(FORBIDDEN), "{attr}: {e}");
        assert!(!e.contains("got"), "{attr}: {e}");
    }
}
