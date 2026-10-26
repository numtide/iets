//! The lint cases of a flake with a `path:` input, built in a scratch
//! directory: the scope of an input, the copy lints and `tree-meta-in-drv`.

use crate::cli::run::{Host, Outcome, run};
use crate::sys::internal_string;
use rstest::{fixture, rstest};
use std::path::{Path, PathBuf};
use std::{env, fs};
use tempfile::{TempDir, tempdir};

struct Scratch {
    work: PathBuf,
    dir: String,
    _tmp: TempDir,
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            fs::copy(e.path(), dest).unwrap();
        }
    }
}

/// Some store path that exists, for the fixture that re-copies one.
#[cfg(unix)]
fn a_store_path() -> Option<String> {
    let path = env::var("PATH").unwrap_or_default();
    path.split(':').find_map(|p| {
        let rest = p.strip_prefix("/nix/store/")?;
        Some(format!("/nix/store/{}", rest.split('/').next()?))
    })
}

#[fixture]
fn scratch() -> Scratch {
    let tmp = tempdir().unwrap();
    let work = tmp.path().to_path_buf();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/lint/scope/flake");
    copy_tree(&src.join("root"), &work.join("root"));
    copy_tree(&src.join("dep"), &work.join("dep"));
    let dep = internal_string(&work.join("dep"));
    let expr = format!("(builtins.fetchTree {{ type = \"path\"; path = \"{dep}\"; }}).narHash");
    let r = run(
        &[
            "eval",
            "--raw",
            "--impure",
            "--no-tracing-cache",
            "-E",
            &expr,
        ],
        &Host::default(),
    );
    assert_eq!(r.rc, 0, "{}", r.stderr);
    let dir = work.join("root");
    for f in ["flake.nix", "flake.lock"] {
        let text = fs::read_to_string(dir.join(f)).unwrap();
        let text = text.replace("@DEP@", &dep).replace("@HASH@", &r.stdout);
        fs::write(dir.join(f), text).unwrap();
    }
    let dir = internal_string(&dir);
    Scratch {
        work,
        dir,
        _tmp: tmp,
    }
}

fn eval(s: &Scratch, attr: &str, flags: &[&str]) -> Outcome {
    eval_with(s, attr, flags, &Host::default())
}

fn eval_with(s: &Scratch, attr: &str, flags: &[&str], host: &Host) -> Outcome {
    let target = format!("{}#{attr}", s.dir);
    // the `path:` input lies beside the root, where a pure evaluation does
    // not fetch (docs/explanation/pure-eval-and-local-sources.md)
    let mut argv = vec!["eval", "--raw", "--impure", "--no-tracing-cache"];
    argv.extend_from_slice(flags);
    argv.push(&target);
    run(&argv, host)
}

const COPIES: [&str; 6] = [
    "--lint",
    "store-path-recopy=warn",
    "--lint",
    "path-copy=warn",
    "--lint",
    "dir-copy-flake=warn",
];

fn no_lint(r: &Outcome) -> bool {
    !r.stderr.contains("warning: [")
}

#[rstest]
fn an_input_is_silent_until_the_scope_or_the_inputs_table_opens_it(scratch: Scratch) {
    let root_site = format!("at {}/flake.nix:4:25", scratch.dir);
    let dep_site = format!("at {}/dep/flake.nix:1:33", internal_string(&scratch.work));
    let warn = ["--lint", "url-literal=warn"];
    let r = eval(&scratch, "value", &warn);
    assert_eq!(r.stdout, "http://dep.examplehttp://root.example");
    assert!(
        r.stderr.contains(&root_site) && !r.stderr.contains(&dep_site),
        "{}",
        r.stderr
    );
    let r = eval(
        &scratch,
        "value",
        &["--lint", "url-literal=warn", "--lint-scope", "all"],
    );
    assert!(
        r.stderr.contains(&root_site) && r.stderr.contains(&dep_site),
        "{}",
        r.stderr
    );
    fs::write(
        scratch.work.join("root/iets.toml"),
        "[lint.inputs.dep]\nurl-literal = \"warn\"\n",
    )
    .unwrap();
    let r = eval(&scratch, "value", &warn);
    assert!(
        r.stderr.contains(&root_site) && r.stderr.contains(&dep_site),
        "{}",
        r.stderr
    );
}

#[rstest]
fn a_copy_under_the_root_or_an_input_is_not_a_recopy(scratch: Scratch) {
    let r = eval(&scratch, "file", &COPIES);
    assert!(
        r.stdout.starts_with("/nix/store/") && no_lint(&r),
        "{}",
        r.stderr
    );
    let r = eval(
        &scratch,
        "inputFile",
        &[&["--lint-scope", "all"], &COPIES[..]].concat(),
    );
    assert!(
        r.stdout.starts_with("/nix/store/") && no_lint(&r),
        "{}",
        r.stderr
    );
}

#[rstest]
fn a_directory_under_the_root_is_dir_copy_flake_not_path_copy(scratch: Scratch) {
    let r = eval(&scratch, "dir", &COPIES);
    let msg = format!(
        "warning: [dir-copy-flake] directory \'{}/sub\' of the flake tree is copied to the store as \'/nix/store/",
        scratch.dir
    );
    assert!(r.stderr.contains(&msg), "{}", r.stderr);
    assert!(
        r.stderr.contains("-sub\' by this string coercion")
            && r.stderr.contains("the tracked files"),
        "{}",
        r.stderr
    );
    assert!(
        !r.stderr.contains("[path-copy]") && !r.stderr.contains("[store-path-recopy]"),
        "{}",
        r.stderr
    );
    let r = eval(&scratch, "dir", &["--lint", "dir-copy-flake=error"]);
    assert_eq!(r.rc, 1, "{}", r.stderr);
    assert!(
        r.stderr.contains(&msg.replace("warning:", "error:")),
        "{}",
        r.stderr
    );
}

/// Windows has no Nix store to re-copy from.
#[cfg(unix)]
#[test]
fn a_pre_existing_store_path_is_still_a_recopy() {
    let Some(store_path) = a_store_path() else {
        eprintln!("skipped: no Nix store on this machine");
        return;
    };
    let s = scratch();
    let f = s.work.join("root/flake.nix");
    let text = fs::read_to_string(&f).unwrap();
    fs::write(&f, text.replace("@STORE@", &store_path)).unwrap();
    let r = eval(&s, "store", &[&["--impure"], &COPIES[..]].concat());
    assert!(
        r.stderr.contains("warning: [store-path-recopy]"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("warning: [path-copy] path \'"),
        "{}",
        r.stderr
    );
}

#[rstest]
fn the_roots_metadata_in_a_derivation_is_reported_by_default(scratch: Scratch) {
    let r = eval(&scratch, "meta.drvPath", &[]);
    let meta = "warning: [tree-meta-in-drv] derivation \'meta\': attribute";
    assert!(r.stderr.contains(&format!("{meta} \'v\' holds \'self.lastModifiedDate\', a value that changes with every commit or edit")), "{}", r.stderr);
    assert!(
        r.stderr
            .contains(&format!("{meta} \'args\' holds \'self.narHash\'")),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains(&format!("at {}/flake.nix:9:69", scratch.dir)),
        "{}",
        r.stderr
    );
    let r = eval(&scratch, "wrapped.drvPath", &[]);
    assert!(
        r.stderr
            .contains("derivation \'wrapped\': attribute \'v\' holds \'self.narHash\'"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(&format!(
            "at {}/dep/flake.nix:2:79",
            internal_string(&scratch.work)
        )),
        "{}",
        r.stderr
    );
}

#[rstest]
fn the_level_of_tree_meta_comes_from_the_flag_the_env_and_the_user_file(scratch: Scratch) {
    let r = eval(
        &scratch,
        "meta.drvPath",
        &["--lint", "tree-meta-in-drv=error"],
    );
    assert!(
        r.rc == 1 && r.stderr.contains("error: [tree-meta-in-drv]"),
        "{}",
        r.stderr
    );
    let off = Host::default().env([("IETS_LINT", "family:purity=off")]);
    let r = eval_with(&scratch, "meta.drvPath", &[], &off);
    assert!(r.rc == 0 && no_lint(&r), "{}", r.stderr);
    let user = scratch.work.join("user/iets");
    fs::create_dir_all(&user).unwrap();
    fs::write(
        user.join("iets.toml"),
        "[lint]\ntree-meta-in-drv = \"error\"\n",
    )
    .unwrap();
    let host = Host::default().env([("XDG_CONFIG_HOME", scratch.work.join("user"))]);
    let r = eval_with(&scratch, "meta.drvPath", &[], &host);
    assert!(
        r.rc == 1 && r.stderr.contains("error: [tree-meta-in-drv]"),
        "{}",
        r.stderr
    );
}

#[rstest]
fn an_input_or_an_allowed_site_is_silent_and_the_scope_opens_the_input(scratch: Scratch) {
    for attr in ["allowed.drvPath", "depMeta.drvPath"] {
        let r = eval(&scratch, attr, &[]);
        assert!(r.rc == 0 && no_lint(&r), "{attr}: {}", r.stderr);
    }
    let r = eval(&scratch, "depMeta.drvPath", &["--lint-scope", "all"]);
    assert!(
        r.stderr.contains(
            "holds \'inputs.dep.narHash\', a value that changes with every update of the lock file"
        ),
        "{}",
        r.stderr
    );
}

#[rstest]
fn the_project_file_sets_tree_meta_per_tree(scratch: Scratch) {
    fs::write(
        scratch.work.join("root/iets.toml"),
        "[lint]\ntree-meta-in-drv = \"off\"\n[lint.inputs.dep]\ntree-meta-in-drv = \"warn\"\n",
    )
    .unwrap();
    let r = eval(&scratch, "depMeta.drvPath", &[]);
    assert!(
        r.stderr
            .contains("derivation \'dep-meta\': attribute \'v\' holds \'inputs.dep.narHash\'"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains(&format!("at {}/flake.nix:11:76", scratch.dir)),
        "{}",
        r.stderr
    );
    let r = eval(&scratch, "meta.drvPath", &[]);
    assert!(r.rc == 0 && no_lint(&r), "{}", r.stderr);
}

#[rstest]
fn a_tree_meta_finding_is_a_record_with_a_span_on_a_cache_hit_too(scratch: Scratch) {
    let cache = scratch.work.join("cache").display().to_string();
    let target = format!("{}#meta.drvPath", scratch.dir);
    let args = [
        "eval",
        "--raw",
        "--timings",
        "--cache-dir",
        &cache,
        "--log-format",
        "internal-json",
        &target,
    ];
    let (r1, r2) = (run(&args, &Host::default()), run(&args, &Host::default()));
    let has = |r: &Outcome| {
        r.stderr.contains("\"code\":\"lint\"")
            && r.stderr
                .contains("[tree-meta-in-drv] derivation \'meta\': attribute \'v\'")
            && r.stderr.contains("\"start\":")
    };
    assert!(has(&r1) && has(&r2), "{}\n{}", r1.stderr, r2.stderr);
    assert!(
        r2.stderr.contains("cache: hit") && r1.stdout == r2.stdout,
        "{}",
        r2.stderr
    );
}
