//! What iets leaves to the real Nix in classic mode, the fetchers,
//! and the edges of a run: a full stdout and a bad iets.toml.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use e2e::classic::{Both, Checks, Farm, NAMES, show};
use e2e::{Store, ok, resolved, stderr, stdout};

#[test]
fn fallbacks() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let parse = ["--parse", "-E", "1"];
    c.same(
        "--parse execs the real nix-instantiate",
        &f.both("nix-instantiate", &parse),
    );
    // --iets-option is ours: the fallback execs nix without it
    let ours = f
        .ours("nix-instantiate")
        .args(["--iets-option", "workers", "2"])
        .args(parse)
        .output()
        .unwrap();
    let real = f.real("nix-instantiate").args(parse).output().unwrap();
    c.same(
        "--iets-option never reaches the fallback nix",
        &Both {
            ours: ours.clone(),
            real,
        },
    );
    c.check(
        "the fallback prints one line on stderr",
        stderr(&ours).contains("iets: falling back to nix-instantiate:"),
        stderr(&ours),
    );
    let o = f
        .ours("nix-instantiate")
        .env("IETS_NIX_FALLBACK", "never")
        .args(parse)
        .output()
        .unwrap();
    c.check(
        "IETS_NIX_FALLBACK=never exits 1",
        o.status.code() == Some(1),
        stderr(&o),
    );
    // IETS_NIX_FALLBACK=always execs the real binary even for what iets handles
    let native = ["--eval", "-E", "1 + 1"];
    let forced = f
        .ours("nix-instantiate")
        .env("IETS_NIX_FALLBACK", "always")
        .args(native)
        .output()
        .unwrap();
    let real = f.real("nix-instantiate").args(native).output().unwrap();
    c.check(
        "IETS_NIX_FALLBACK=always runs the real nix-instantiate",
        forced.stdout == real.stdout
            && forced.status.code() == real.status.code()
            && stderr(&forced)
                .contains("iets: falling back to nix-instantiate: IETS_NIX_FALLBACK is always"),
        stderr(&forced),
    );
    // a broken setting is one reason to want the real nix: the switch is
    // read before the settings are
    let broken = f
        .ours("nix-instantiate")
        .env("IETS_NIX_FALLBACK", "always")
        .env("IETS_WORKERS", "many")
        .args(native)
        .output()
        .unwrap();
    c.check(
        "IETS_NIX_FALLBACK=always runs the real nix-instantiate with a broken setting",
        broken.stdout == real.stdout && broken.status.code() == real.status.code(),
        stderr(&broken),
    );
    // `iets nix-instantiate …` is the same command
    let sub = s
        .iets()
        .env("IETS_NIX_BIN_DIR", &f.real)
        .env("NIX_REMOTE", "daemon")
        .env("IETS_NIX_FALLBACK", "always")
        .env("IETS_WORKERS", "many")
        .arg("nix-instantiate")
        .args(native)
        .output()
        .unwrap();
    c.check(
        "IETS_NIX_FALLBACK=always runs the real nix-instantiate as an iets subcommand",
        sub.stdout == real.stdout && sub.status.code() == real.status.code(),
        stderr(&sub),
    );
    // with a broken setting the real nix still comes from IETS_NIX_BIN_DIR
    let fake = s.dir().join("fakebin");
    fs::create_dir(&fake).unwrap();
    fs::write(fake.join("nix-instantiate"), "#!/bin/sh\necho FAKE\n").unwrap();
    fs::set_permissions(
        fake.join("nix-instantiate"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let faked = f
        .ours("nix-instantiate")
        .env("IETS_NIX_BIN_DIR", &fake)
        .env("IETS_NIX_FALLBACK", "always")
        .env("IETS_WORKERS", "many")
        .args(native)
        .output()
        .unwrap();
    c.check(
        "a broken setting keeps IETS_NIX_BIN_DIR for the forced fallback",
        stdout(&faked).trim() == "FAKE",
        format!("{}{}", stdout(&faked), stderr(&faked)),
    );
    // the file sets it too, for an admin who turned the real nix off
    let toml = s.dir().join("xdg/iets/iets.toml");
    fs::create_dir_all(toml.parent().unwrap()).unwrap();
    fs::write(&toml, "nix-fallback = \"never\"\n").unwrap();
    let o = f
        .ours("nix-instantiate")
        .args(["--parse", "-E", "1"])
        .output()
        .unwrap();
    fs::remove_file(&toml).unwrap();
    c.check(
        "nix-fallback = never in iets.toml exits 1",
        o.status.code() == Some(1) && stderr(&o).contains("nix-fallback is never"),
        stderr(&o),
    );

    // a nix-bin-dir of iets's own names: the fallback stops instead of looping
    let lp = s.dir().join("loop");
    fs::create_dir(&lp).unwrap();
    for n in NAMES {
        symlink(env!("CARGO_BIN_EXE_iets"), lp.join(n)).unwrap();
    }
    let looped = timeout(
        f.ours("nix-instantiate")
            .env("IETS_NIX_BIN_DIR", &lp)
            .env("PATH", &lp)
            .args(parse),
    );
    c.check(
        "nix-bin-dir full of iets: the fallback stops",
        looped.as_ref().is_some_and(|o| {
            o.status.code() == Some(1)
                && stderr(o).contains("cannot fall back to nix-instantiate: not found on PATH")
        }),
        looped.as_ref().map(stderr).unwrap_or("timed out".into()),
    );

    // the farm shadows the real nix out of PATH and no IETS_NIX_BIN_DIR is
    // set: the fallback finds nix through the farm's marker
    let shadow = s.dir().join("shadow");
    fs::create_dir(&shadow).unwrap();
    symlink(resolved("bash"), shadow.join("bash")).unwrap();
    let ours = f
        .ours("nix-instantiate")
        .env("PATH", format!("{}:{}", f.bin.display(), shadow.display()))
        .env_remove("IETS_NIX_BIN_DIR")
        .args(["--parse", "-E", "1 + 1"])
        .output()
        .unwrap();
    let real = f
        .real("nix-instantiate")
        .args(["--parse", "-E", "1 + 1"])
        .output()
        .unwrap();
    c.same("the fallback through the farm marker", &Both { ours, real });

    let repl = f.ours("nix").args(["repl", "--version"]).output().unwrap();
    c.check(
        "the 'nix' name falls back",
        stderr(&repl).contains("iets: falling back to nix:"),
        stderr(&repl),
    );

    let why = f.reason(
        "nix-instantiate",
        &["--eval", "-E", "builtins.fetchMercurial ./."],
    );
    c.check(
        "fetchMercurial falls back with one 'iets:' prefix",
        why.contains(
            "iets: falling back to nix-instantiate: builtins.fetchMercurial is not supported\n",
        ),
        &why,
    );

    let ifd = r#"import (derivation { name = "x"; system = builtins.currentSystem; builder = "@SHELL@"; args = ["-c" "echo 42 > $out"]; })"#;
    let ifd = fs::read_to_string(s.write("ifd.nix", ifd)).unwrap();
    c.same(
        "import from derivation falls back",
        &f.both("nix-instantiate", &["--eval", "-E", &ifd]),
    );
    let o = s
        .iets()
        .args(["eval", "--impure", "-E", &ifd])
        .output()
        .unwrap();
    c.check(
        "iets eval refuses import from derivation",
        o.status.code() == Some(3)
            && stderr(&o).contains("import from derivation is not supported"),
        stderr(&o),
    );

    // priced with IETS_NIX_FALLBACK=never, so the real run never happens
    let script = s.write(
        "script.sh",
        "#! /usr/bin/env nix-shell\n#! nix-shell -p hello -i bash\necho hi\n",
    );
    for (name, args, want) in [
        ("nix", &["fmt", "--json"][..], "'--json' is not implemented"),
        (
            "nix-shell",
            &["-p", "hello", "--run", "true"],
            "'-p' is not implemented",
        ),
        (
            "nix",
            &["develop", "--phase", "build"],
            "'--phase' is not implemented",
        ),
        (
            "nix",
            &["develop", "--profile", "p"],
            "'--profile' is not implemented",
        ),
        (
            "nix",
            &["develop", "--redirect", "a", "b"],
            "'--redirect' is not implemented",
        ),
        ("nix-shell", &[script.to_str().unwrap()], "a '#!' script"),
        (
            "nix",
            &["eval", "--apply", "x: x", "--expr", "1"],
            "'--apply' is not implemented",
        ),
        (
            "nix",
            &["flake", "metadata", "."],
            "falling back to nix: subcommand 'flake'",
        ),
    ] {
        let why = f.reason(name, args);
        c.check(
            &format!("{name} {} falls back", args.join(" ")),
            why.contains(want),
            &why,
        );
    }
    for (r, want) in [
        (
            "nixpkgs#lib.version",
            "flake registry references (nixpkgs#…) are not supported",
        ),
        (
            "github:NixOS/nixpkgs#lib.version",
            "only local paths are supported",
        ),
    ] {
        let why = f.reason("nix", &["eval", r]);
        c.check(
            &format!("nix eval {r} falls back"),
            why.contains("iets: falling back to nix: ")
                && why.contains(want)
                && !why.contains("nix: iets: "),
            &why,
        );
    }
}

/// `c`'s output, or `None` and a kill after 20 seconds.
fn timeout(c: &mut Command) -> Option<Output> {
    let mut child = c
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Some(child.wait_with_output().unwrap())
}

/// The exit code of `c` with stdout on /dev/full.
#[cfg(target_os = "linux")]
fn to_full(c: &mut Command) -> Option<i32> {
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    c.stdout(full)
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .code()
}

#[cfg(target_os = "linux")]
#[test]
fn a_full_stdout() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    // `nix eval` reports a failed write and exits 1, the legacy names exit 0
    // as `std::cout` lets them, and nothing aborts
    for (name, args, want) in [
        ("nix", &["eval", "--expr", "1"][..], 1),
        ("nix-instantiate", &["--eval", "-E", "1"], 0),
    ] {
        let (o, r) = (
            to_full(f.ours(name).args(args)),
            to_full(f.real(name).args(args)),
        );
        c.check(
            &format!("{name} > /dev/full"),
            o == Some(want) && r == Some(want),
            format!("ours {o:?}, real {r:?}"),
        );
    }
}

/// Files where a store path would be, which the store never registered:
/// what an interrupted copy leaves.
fn leftover(s: &Store, args: &[&str]) -> String {
    stdout(&ok(s.nix("nix-store").arg("--print-fixed-path").args(args)))
}

#[test]
fn fetchers() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    s.write("tb/f", "hi\n");
    ok(Command::new("tar")
        .current_dir(s.dir())
        .args(["-czf", "x.tar.gz", "tb"]));
    let tgz = format!("file://{}/x.tar.gz", s.dir().display());
    let sha = "113a52l57f2jv9fh9igj2xh2mi0is40flxbkqi69wbh7nlybkcbm";
    let tb_hash = stdout(&ok(s
        .nix("nix-hash")
        .args(["--type", "sha256", "--base32"])
        .arg(s.dir().join("tb"))));
    for p in [
        leftover(&s, &["sha256", sha, "leftover"]),
        leftover(&s, &["sha256", &"0".repeat(52), "leftover"]),
    ] {
        fs::write(p, "stale\n").unwrap();
    }
    let t = leftover(&s, &["--recursive", "sha256", &tb_hash, "leftover-tree"]);
    fs::create_dir(&t).unwrap();
    fs::write(Path::new(&t).join("f"), "stale\n").unwrap();

    c.same(
        "fetchTarball falls back and prints the real result",
        &f.both(
            "nix-instantiate",
            &["--eval", "-E", &format!("builtins.fetchTarball \"{tgz}\"")],
        ),
    );
    // ours runs first, so the file is not in the store yet
    s.write("hello.txt", "hello fetchurl\n");
    let fu = format!("file://{}/hello.txt", s.dir().display());
    let cases = [
        (
            "fetchurl with a sha256 has nix's store path",
            "nix-instantiate",
            vec!["--eval", "-E"],
            format!(r#"builtins.fetchurl {{ url = "{fu}"; sha256 = "{sha}"; name = "fu"; }}"#),
        ),
        (
            "nix eval reads what fetchurl got",
            "nix",
            vec!["eval", "--impure", "--raw", "--expr"],
            format!(
                r#"builtins.readFile (builtins.fetchurl {{ url = "{fu}"; sha256 = "{sha}"; }})"#
            ),
        ),
        (
            "fetchurl without a hash falls back in an impure evaluation",
            "nix-instantiate",
            vec!["--eval", "-E"],
            format!(r#"builtins.fetchurl "{fu}""#),
        ),
        (
            "fetchurl without a hash in a pure evaluation",
            "nix",
            vec!["eval", "--raw", "--expr"],
            format!(r#"builtins.fetchurl "{fu}""#),
        ),
        (
            "fetchurl does not take an invalid path",
            "nix",
            vec!["eval", "--impure", "--raw", "--expr"],
            format!(
                r#"builtins.readFile (builtins.fetchurl {{ url = "{fu}"; sha256 = "{sha}"; name = "leftover"; }})"#
            ),
        ),
        (
            "fetchTarball does not take an invalid path",
            "nix",
            vec!["eval", "--impure", "--raw", "--expr"],
            format!(
                r#"builtins.readFile (builtins.fetchTarball {{ url = "{tgz}"; sha256 = "{tb_hash}"; name = "leftover-tree"; }} + "/f")"#
            ),
        ),
    ];
    for (label, name, mut a, e) in cases {
        a.push(&e);
        c.same(label, &f.both(name, &a));
    }
    // a mismatch exits 1, where nix exits 102 (docs/reference/builtins.md)
    let e = format!(
        r#"builtins.fetchurl {{ url = "{fu}"; sha256 = "{}"; name = "leftover"; }}"#,
        "0".repeat(52)
    );
    let b = f.both("nix", &["eval", "--impure", "--raw", "--expr", &e]);
    c.check(
        "fetchurl with a wrong hash does not take an invalid path",
        b.ours.status.code() == Some(1)
            && b.real.status.code() == Some(102)
            && b.ours.stdout.is_empty()
            && stderr(&b.ours).contains("hash mismatch"),
        show(&b),
    );
}

fn git(repo: &Path, args: &[&str]) -> String {
    stdout(&e2e::git(repo, args))
}

/// builtins.fetchGit of a working tree, clean and dirty, with
/// `export-ignore`, and of a commit by `rev` and by `ref`, which iets reads
/// itself. Then `lib.fileset.gitTracked`, which calls it.
#[test]
fn fetch_git() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let repo = s.dir().join("fetchgit repo");
    fs::create_dir_all(repo.join("sub")).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("a"), "a\n").unwrap();
    fs::write(repo.join("sub/b"), "b\n").unwrap();
    fs::write(repo.join("sub/ign"), "i\n").unwrap();
    fs::write(repo.join(".gitattributes"), "sub/ign export-ignore\n").unwrap();
    git(&repo, &["add", "-A"]);
    let commit = |m: &str, date: &str| e2e::git_commit(&repo, m, date);
    commit("one", "1700000000 +0000");
    let rev = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["tag", "light"]);
    git(&repo, &["tag", "-a", "-m", "ann", "ann"]);
    // a branch the working tree is not on
    git(&repo, &["checkout", "-q", "-b", "more"]);
    fs::write(repo.join("a"), "a2\n").unwrap();
    fs::write(repo.join("x"), "x\n").unwrap();
    fs::set_permissions(repo.join("x"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("a", repo.join("l")).unwrap();
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{rev},m"),
        ],
    );
    commit("two", "1700000100 +0000");
    git(&repo, &["checkout", "-q", "main"]);

    let show_expr = |arg: &str| {
        format!(
            r#"let r = builtins.fetchGit ({arg}); in builtins.removeAttrs r [ "outPath" ] // {{ out = r.outPath; ls = builtins.attrNames (builtins.readDir r); a = builtins.readFile (r + "/a"); }}"#
        )
    };
    let path = format!("\"{}\"", repo.display());
    let url = format!(
        "\"file://{}\"",
        repo.display().to_string().replace(' ', "%20")
    );
    let one = |c: &mut Checks, label: &str, arg: &str, mode: &str| {
        let args = ["eval", mode, "--json", "--expr", &show_expr(arg)];
        c.same(&format!("fetchGit {label}"), &native(&f, &s, &args));
    };
    for (label, arg, mode) in [
        ("of a clean working tree", path.clone(), "--impure"),
        ("of a file URL", url, "--impure"),
        (
            "with name, shallow and exportIgnore = false",
            format!(r#"{{ url = {path}; name = "x"; shallow = true; exportIgnore = false; }}"#),
            "--impure",
        ),
        (
            "of a rev",
            format!(r#"{{ url = {path}; rev = "{rev}"; }}"#),
            "--impure",
        ),
        (
            "of a ref",
            format!(r#"{{ url = {path}; ref = "main"; }}"#),
            "--impure",
        ),
        (
            "of an unlocked tree in pure mode",
            path.clone(),
            "--pure-eval",
        ),
        (
            "of a ref that is a tag",
            format!(r#"{{ url = {path}; ref = "light"; }}"#),
            "--impure",
        ),
        (
            "of an annotated tag",
            format!(r#"{{ url = {path}; ref = "refs/tags/ann"; }}"#),
            "--impure",
        ),
        (
            "of a branch with an executable, a symlink and a submodule",
            format!(r#"{{ url = {path}; ref = "more"; }}"#),
            "--impure",
        ),
        (
            "of a rev and a ref",
            format!(r#"{{ url = {path}; rev = "{rev}"; ref = "more"; }}"#),
            "--impure",
        ),
        (
            "of a rev with exportIgnore = false",
            format!(r#"{{ url = {path}; rev = "{rev}"; exportIgnore = false; }}"#),
            "--impure",
        ),
        (
            "of a ref that names nothing",
            format!(r#"{{ url = {path}; ref = "nope"; }}"#),
            "--impure",
        ),
    ] {
        one(&mut c, label, &arg, mode);
    }
    // a rev locks the result, not where a pure evaluation reads it from: the
    // repository is no part of the run, so iets refuses where nix reads it
    // (docs/reference/divergences.md)
    let pinned = show_expr(&format!(r#"{{ url = {path}; rev = "{rev}"; }}"#));
    let b = native(
        &f,
        &s,
        &["eval", "--pure-eval", "--json", "--expr", &pinned],
    );
    c.check(
        "fetchGit of a rev in pure mode is a read outside the run",
        b.ours.status.code() == Some(1)
            && b.real.status.code() == Some(0)
            && b.ours.stdout.is_empty()
            && stderr(&b.ours).contains("is forbidden in pure evaluation mode"),
        show(&b),
    );
    fs::write(repo.join("a"), "changed\n").unwrap();
    fs::write(repo.join("untracked"), "new\n").unwrap();
    fs::write(repo.join(".gitattributes"), "sub/b export-ignore\n").unwrap();
    git(&repo, &["add", ".gitattributes"]);
    one(&mut c, "of a dirty working tree", &path, "--impure");
    let o = f
        .ours("nix")
        .args([
            "eval",
            "--impure",
            "--raw",
            "--expr",
            &format!("(builtins.fetchGit {path}).outPath"),
        ])
        .output()
        .unwrap();
    let want = format!("warning: Git tree '{}' is dirty", repo.display());
    c.check(
        "fetchGit of a dirty tree warns as Nix does",
        stderr(&o).contains(&want),
        stderr(&o),
    );
    let Ok(np) = env::var("IETS_TEST_NIXPKGS") else {
        eprintln!("skipped gitTracked: IETS_TEST_NIXPKGS unset");
        return;
    };
    let root = format!("(/. + {path})");
    let tracked = format!(
        "let lib = import {np}/lib; fs = lib.fileset; s = fs.toSource {{ root = {root}; fileset = fs.gitTracked {root}; }}; in [ s.outPath (builtins.attrNames (builtins.readDir s)) ]"
    );
    let e = ["eval", "--impure", "--json", "--expr", &tracked];
    c.same(
        "lib.fileset.gitTracked of a dirty working tree",
        &f.both("nix", &e),
    );
    git(
        &repo,
        &["checkout", "-q", "HEAD", "--", "a", ".gitattributes"],
    );
    fs::remove_file(repo.join("untracked")).unwrap();
    c.same(
        "lib.fileset.gitTracked of a clean working tree",
        &f.both("nix", &e),
    );
}

/// fetchGit with `submodules = true` on a working tree, compared with Nix:
/// nested submodules, a gitlink missing from `.gitmodules`, and an edit
/// inside a nested submodule.
#[test]
fn fetch_git_submodules() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let init = |dir: &Path, file: &str| {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]);
        fs::write(dir.join(file), format!("{file}\n")).unwrap();
        git(dir, &["add", "-A"]);
        e2e::git_commit(dir, "init", "1700000000 +0000");
    };
    let add = |repo: &Path, url: &Path, at: &str| {
        let url = url.to_str().unwrap();
        let args = [
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            url,
            at,
        ];
        git(repo, &args);
        git(repo, &["add", "-A"]);
        e2e::git_commit(repo, "sub", "1700000100 +0000");
    };
    let leaf = s.dir().join("leaf");
    let mid = s.dir().join("mid");
    let repo = s.dir().join("super repo");
    init(&leaf, "c");
    init(&mid, "b");
    add(&mid, &leaf, "leaf");
    init(&repo, "a");
    add(&repo, &mid, "mid");
    let update = [
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "update",
        "--init",
        "--recursive",
    ];
    git(&repo, &update);
    let rev = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{rev},unnamed"),
        ],
    );
    e2e::git_commit(&repo, "gitlink", "1700000200 +0000");

    let arg = format!(r#"{{ url = "{}"; submodules = true; }}"#, repo.display());
    let expr = format!(
        r#"let r = builtins.fetchGit {arg}; in builtins.removeAttrs r [ "outPath" ] // {{ out = r.outPath; c = builtins.readFile (r + "/mid/leaf/c"); }}"#
    );
    let args = ["eval", "--impure", "--json", "--expr", &expr];
    c.same("fetchGit with submodules", &native(&f, &s, &args));
    fs::write(repo.join("mid/leaf/c"), "changed\n").unwrap();
    c.same(
        "fetchGit with a dirty nested submodule",
        &native(&f, &s, &args),
    );
}

/// fetchGit of a repository that is not a working tree, which iets clones
/// itself as Nix does: by `rev`, by `ref`, the default branch, `allRefs`,
/// `shallow`, submodules, and a `narHash` that is wrong.
#[test]
fn fetch_git_remote() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let leaf = s.dir().join("leaf");
    fs::create_dir_all(&leaf).unwrap();
    git(&leaf, &["init", "-q", "-b", "trunk"]);
    fs::write(leaf.join("c"), "c\n").unwrap();
    git(&leaf, &["add", "-A"]);
    e2e::git_commit(&leaf, "leaf", "1700000000 +0000");
    let work = s.dir().join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "trunk"]);
    fs::write(work.join("a"), "a\n").unwrap();
    git(&work, &["add", "-A"]);
    e2e::git_commit(&work, "one", "1700000100 +0000");
    let leaf_url = leaf.to_str().unwrap();
    git(
        &work,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            leaf_url,
            "leaf",
        ],
    );
    // Nix resolves a relative URL against `file:///tmp/…` to `file://tmp/…`,
    // which it then refuses
    let leaf_bare = format!("file://{}", s.dir().join("leaf.git").display());
    let set_url = [
        "config",
        "-f",
        ".gitmodules",
        "submodule.leaf.url",
        &leaf_bare,
    ];
    git(&work, &set_url);
    git(&work, &["add", "-A"]);
    e2e::git_commit(&work, "two", "1700000200 +0000");
    let main = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "-b", "feature"]);
    fs::write(work.join("b"), "b\n").unwrap();
    git(&work, &["add", "-A"]);
    e2e::git_commit(&work, "three", "1700000300 +0000");
    let feature = git(&work, &["rev-parse", "HEAD"]);
    for (from, to) in [(&leaf, "leaf.git"), (&work, "super.git")] {
        let to = s.dir().join(to);
        git(
            s.dir(),
            &[
                "clone",
                "-q",
                "--bare",
                from.to_str().unwrap(),
                to.to_str().unwrap(),
            ],
        );
    }
    let url = format!("file://{}", s.dir().join("super.git").display());
    let show = r#"r: builtins.removeAttrs r [ "outPath" ] // { out = r.outPath; n = r.revCount or null; }"#;
    for (label, arg) in [
        ("rev", format!(r#"{{ url = "{url}"; rev = "{main}"; }}"#)),
        ("default branch", format!(r#"{{ url = "{url}"; }}"#)),
        ("ref", format!(r#"{{ url = "{url}"; ref = "feature"; }}"#)),
        (
            "allRefs",
            format!(r#"{{ url = "{url}"; rev = "{feature}"; allRefs = true; }}"#),
        ),
        (
            "shallow",
            format!(r#"{{ url = "{url}"; rev = "{main}"; shallow = true; }}"#),
        ),
        (
            "submodules",
            format!(r#"{{ url = "{url}"; rev = "{main}"; submodules = true; }}"#),
        ),
    ] {
        let expr = format!("({show}) (builtins.fetchGit {arg})");
        let args = ["eval", "--impure", "--json", "--expr", &expr];
        c.same(&format!("fetchGit {label}"), &native(&f, &s, &args));
    }
    // offline, a cached clone still answers, and a new download is an error
    let offline = |expr: &str| {
        let mut o = f.ours("nix");
        o.env("IETS_OFFLINE", "1").env("IETS_CACHE", "0");
        o.args(["eval", "--impure", "--raw", "--expr", expr])
            .output()
            .unwrap()
    };
    let at = format!(r#"builtins.fetchGit {{ url = "{url}"; rev = "{main}"; }}"#);
    let o = offline(&format!("({at}).narHash"));
    c.check(
        "offline, a cached rev is read",
        o.status.success(),
        stderr(&o),
    );
    fs::write(work.join("c"), "c\n").unwrap();
    git(&work, &["add", "-A"]);
    e2e::git_commit(&work, "four", "1700000500 +0000");
    let new = git(&work, &["rev-parse", "HEAD"]);
    let bare = s.dir().join("super.git");
    git(&work, &["push", "-q", bare.to_str().unwrap(), "feature"]);
    let at = format!(r#"builtins.fetchGit {{ url = "{url}"; rev = "{new}"; allRefs = true; }}"#);
    let o = offline(&format!("({at}).narHash"));
    let refused = stderr(&o).contains("IETS_OFFLINE is set");
    c.check("offline, an uncached rev is refused", refused, stderr(&o));
    // `ext::` runs a command, and a submodule's URL skips fetchGit's URL
    // checks. iets refuses it even when the user's git config allows it.
    let marker = s.dir().join("pwned");
    let evil = s.dir().join("evil");
    fs::create_dir_all(&evil).unwrap();
    git(&evil, &["init", "-q", "-b", "trunk"]);
    let ext = format!("ext::sh -c touch% {}", marker.display());
    let modules = format!("[submodule \"x\"]\n\tpath = x\n\turl = {ext}\n");
    fs::write(evil.join(".gitmodules"), modules).unwrap();
    git(&evil, &["add", "-A"]);
    let gitlink = format!("160000,{main},x");
    git(&evil, &["update-index", "--add", "--cacheinfo", &gitlink]);
    e2e::git_commit(&evil, "evil", "1700000400 +0000");
    let evil_rev = git(&evil, &["rev-parse", "HEAD"]);
    let config = s.dir().join("gitconfig");
    fs::write(&config, "[protocol \"ext\"]\n\tallow = always\n").unwrap();
    let expr = format!(
        r#"(builtins.fetchGit {{ url = "file://{}"; rev = "{evil_rev}"; submodules = true; }}).narHash"#,
        evil.display()
    );
    let o = f
        .ours("nix")
        .env("IETS_NIX_BIN_DIR", s.failing_nix())
        .env("GIT_CONFIG_GLOBAL", &config)
        .args(["eval", "--impure", "--raw", "--expr", &expr])
        .output()
        .unwrap();
    c.check(
        "a submodule with an ext:: URL runs no command",
        !o.status.success() && !marker.exists(),
        stderr(&o),
    );
    let wrong = "sha256-aWV0cy1naXQtbWlzbWF0Y2gtbmV2ZXItc3RvcmVkISE=";
    let expr = format!(
        r#"(builtins.fetchGit {{ url = "{url}"; rev = "{main}"; narHash = "{wrong}"; }}).rev"#
    );
    let ours = f
        .ours("nix")
        .env("IETS_NIX_BIN_DIR", s.failing_nix())
        .args(["eval", "--impure", "--raw", "--expr", &expr])
        .output()
        .unwrap();
    c.check(
        "fetchGit with a wrong narHash",
        !ours.status.success() && stderr(&ours).contains("NAR hash mismatch in input"),
        stderr(&ours),
    );
}

/// fetchGit with `verifyCommit`, against a commit signed with an SSH key,
/// fetched from a remote repository and read from a working tree.
#[test]
fn fetch_git_verify_commit() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let keygen = |name: &str, kind: &str| {
        let key = s.dir().join(name);
        let out = Command::new("ssh-keygen")
            .args(["-q", "-t", kind, "-N", "", "-f"])
            .arg(&key)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        let public = fs::read_to_string(key.with_extension("pub")).unwrap();
        (key, public.split(' ').nth(1).unwrap().to_string())
    };
    let (signer, good) = keygen("good", "ed25519");
    let (_, bad) = keygen("bad", "ed25519");
    let work = s.dir().join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "trunk"]);
    fs::write(work.join("a"), "a\n").unwrap();
    git(&work, &["add", "-A"]);
    e2e::git_commit(&work, "unsigned", "1700000000 +0000");
    let unsigned = git(&work, &["rev-parse", "HEAD"]);
    fs::write(work.join("b"), "b\n").unwrap();
    git(&work, &["add", "-A"]);
    let key = format!("user.signingkey={}", signer.display());
    let sign = ["-c", "gpg.format=ssh", "-c", &key, "-c", "user.name=iets"];
    let sign = [&sign[..], &["-c", "user.email=iets@example.com"]].concat();
    git(
        &work,
        &[&sign[..], &["commit", "-q", "-S", "-m", "signed"]].concat(),
    );
    let signed = git(&work, &["rev-parse", "HEAD"]);
    let bare = s.dir().join("work.git");
    git(
        s.dir(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let url = format!("file://{}", bare.display());
    let list = format!(
        r#"[ {{ type = "ssh-ed25519"; key = "{bad}"; }} {{ type = "ssh-ed25519"; key = "{good}"; }} ]"#
    );
    for (label, arg) in [
        (
            "good key",
            format!(r#"url = "{url}"; rev = "{signed}"; publicKey = "{good}";"#),
        ),
        (
            "wrong key",
            format!(r#"url = "{url}"; rev = "{signed}"; publicKey = "{bad}";"#),
        ),
        (
            "key list",
            format!(r#"url = "{url}"; rev = "{signed}"; publicKeys = {list};"#),
        ),
        (
            "unsigned",
            format!(r#"url = "{url}"; rev = "{unsigned}"; publicKey = "{good}";"#),
        ),
        (
            "verifyCommit off",
            format!(
                r#"url = "{url}"; rev = "{unsigned}"; verifyCommit = false; publicKey = "{good}";"#
            ),
        ),
        (
            "working tree",
            format!(
                r#"url = "file://{}"; publicKey = "{good}";"#,
                work.display()
            ),
        ),
    ] {
        let expr = format!("(builtins.fetchGit {{ {arg} }}).narHash");
        let args = [
            "eval",
            "--impure",
            "--raw",
            "--extra-experimental-features",
            "verified-fetches",
            "--expr",
            &expr,
        ];
        c.same(&format!("fetchGit {label}"), &native(&f, &s, &args));
    }
    // Nix writes `ssh-ecdsa` into the allowed signers, which OpenSSH rejects
    let (ecdsa, public) = keygen("ecdsa", "ecdsa");
    let key = format!("user.signingkey={}", ecdsa.display());
    let sign = [&["-c", "gpg.format=ssh", "-c", &key][..], &sign[4..]].concat();
    git(
        &work,
        &[
            &sign[..],
            &["commit", "-q", "-S", "--allow-empty", "-m", "ecdsa"],
        ]
        .concat(),
    );
    let rev = git(&work, &["rev-parse", "HEAD"]);
    let expr = format!(
        r#"(builtins.fetchGit {{ url = "file://{}"; rev = "{rev}"; keytype = "ssh-ecdsa"; publicKey = "{public}"; }}).narHash"#,
        work.display()
    );
    let mut o = f.ours("nix");
    o.env("IETS_NIX_BIN_DIR", s.failing_nix());
    let o = o
        .args(["eval", "--impure", "--raw", "--expr", &expr])
        .output()
        .unwrap();
    c.check(
        "an ECDSA signature verifies",
        o.status.success(),
        stderr(&o),
    );
}

/// Both runs, with a failing `nix` for iets, so a match shows iets fetched
/// on its own.
fn native(f: &Farm, s: &Store, args: &[&str]) -> Both {
    let nonix = s.failing_nix();
    f.both_with("nix", args, |c| {
        c.env("IETS_NIX_BIN_DIR", &nonix);
    })
}
