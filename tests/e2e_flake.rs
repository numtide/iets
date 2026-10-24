//! Flake evaluation end to end: `iets eval --raw FLAKE#attr` prints what
//! `nix eval --raw` prints, on flakes with locked inputs of every offline
//! type, git working trees in every state, and inputs that iets fetches into
//! its own input store (docs/reference/flakes.md).
//!
//! The cases that download from the network run with `IETS_TEST_ONLINE=1`.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use e2e::{Checks, Store, git, ok, records, stderr, stdout};
use serde_json::{Value, json};

const LOCK: &str = r#"{"nodes":{"root":{}},"root":"root","version":7}"#;

struct F {
    s: Store,
    c: Checks,
}

fn last(s: &str, n: usize) -> String {
    let l: Vec<&str> = s.lines().collect();
    l[l.len().saturating_sub(n)..].join(" | ")
}

fn mkdir(p: &Path) {
    fs::create_dir_all(p).unwrap();
}

fn write(p: &Path, text: &str) {
    mkdir(p.parent().unwrap());
    fs::write(p, text).unwrap();
}

fn tar(dir: &Path, out: &str, member: &str) {
    ok(Command::new("tar")
        .arg("-C")
        .arg(dir)
        .args(["-czf", out, member]));
}

/// A lock with one tarball input `name`, which is not a flake.
fn tarball_lock(inputs: &[(&str, &str, &str)]) -> String {
    let mut nodes = serde_json::Map::new();
    let mut root = serde_json::Map::new();
    for (name, url, hash) in inputs {
        nodes.insert(
            name.to_string(),
            json!({
                "flake": false,
                "locked": { "type": "tarball", "url": url, "narHash": hash },
                "original": { "type": "tarball", "url": url },
            }),
        );
        root.insert(name.to_string(), json!(name));
    }
    nodes.insert("root".into(), json!({ "inputs": root }));
    json!({ "nodes": nodes, "root": "root", "version": 7 }).to_string()
}

impl F {
    fn new() -> Option<F> {
        Some(F {
            s: Store::start_with(&[])?,
            c: Checks::default(),
        })
    }

    fn work(&self) -> PathBuf {
        self.s.dir().to_path_buf()
    }

    fn w(&self, rel: &str) -> String {
        format!("{}/{rel}", self.s.dir().display())
    }

    fn iets(&self) -> Command {
        let mut c = self.s.iets();
        c.env("IETS_CACHE_DIR", self.w("cache"));
        c
    }

    fn nix(&self, args: &[&str]) -> Output {
        self.s.nix("nix").args(args).output().unwrap()
    }

    fn nix_eval(&self, r: &str) -> Output {
        self.nix(&["eval", "--raw", "--option", "eval-cache", "false", r])
    }

    fn iets_eval(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut c = self.iets();
        c.envs(env.iter().copied()).arg("eval").args(args);
        c.output().unwrap()
    }

    /// iets with an empty dir as PATH and IETS_NIX_BIN_DIR: no nix to fall
    /// back on.
    fn no_nix(&self) -> Vec<(&'static str, String)> {
        let nobin = self.s.no_nix().to_str().unwrap().to_string();
        vec![("IETS_NIX_BIN_DIR", nobin.clone()), ("PATH", nobin)]
    }

    fn verdict(&mut self, ok: bool, what: impl FnOnce() -> String) {
        if !ok {
            self.c.fail(what());
        }
    }

    /// Both evaluators agree and succeed.
    fn check(&mut self, name: &str, r: &str, env: &[(&str, &str)]) {
        let n = self.nix_eval(r);
        let i = self.iets_eval(env, &["--raw", "--no-tracing-cache", r]);
        let (exp, got) = (stdout(&n), stdout(&i));
        self.verdict(!exp.is_empty() && exp == got, || {
            format!(
                "{name}\n  nix:  {exp} {}\n  iets: {got} {}",
                last(&stderr(&n), 3),
                last(&stderr(&i), 3)
            )
        });
    }

    /// `check` on a tree Nix never copied: iets runs first, with the cache
    /// off, and fetches nothing.
    fn check_fresh(&mut self, name: &str, r: &str) {
        let i = self.iets_eval(&[("IETS_CACHE", "0")], &["--raw", "--no-cache", r]);
        let n = self.nix_eval(r);
        let (exp, got) = (stdout(&n), stdout(&i));
        let fetched = stderr(&i).contains("fetching");
        self.verdict(!exp.is_empty() && exp == got && !fetched, || {
            format!(
                "{name}\n  nix:  {exp}\n  iets: {got} {}",
                last(&stderr(&i), 3)
            )
        });
    }

    /// Both refuse, with the same `error:` line. The provenance rewrite is
    /// off, so the message names the store path as Nix does.
    fn check_error(&mut self, name: &str, r: &str) {
        let first = |o: &Output| {
            stderr(o)
                .lines()
                .map(str::trim)
                .find(|l| l.starts_with("error: "))
                .unwrap_or_default()
                .to_string()
        };
        let n = self.nix_eval(r);
        let i = self.iets_eval(
            &[("IETS_PROVENANCE", "0")],
            &["--raw", "--no-tracing-cache", "--pure-eval", r],
        );
        let (exp, got) = (first(&n), first(&i));
        self.verdict(
            !exp.is_empty() && exp == got && n.status.code() == i.status.code(),
            || {
                format!(
                    "{name}\n  nix:  {exp} ({})\n  iets: {got} ({})",
                    n.status, i.status
                )
            },
        );
    }

    /// iets alone fetches the input and prints `expected`.
    fn fetch_only(&mut self, name: &str, r: &str, expected: &str) {
        let env = self.no_nix();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let i = self.iets_eval(&env, &["--raw", "--no-tracing-cache", r]);
        let got = stdout(&i);
        self.verdict(
            !expected.is_empty() && got == expected && stderr(&i).contains("fetching it"),
            || {
                format!(
                    "{name}\n  want: {expected}\n  iets: {got} {}",
                    last(&stderr(&i), 3)
                )
            },
        );
    }

    /// Nix locks the flake at `dir` and evaluates `dir#v` in a store of its
    /// own, named `store`; returns that value and the store path of `input`.
    /// The daemon's store never has the input, so nothing must be deleted
    /// there: a daemon worker that served a Nix call can outlive the client,
    /// and its temp root then keeps the path.
    fn lock_apart(&self, dir: &str, input: &str, store: &str) -> (String, String) {
        let store = self.w(store);
        let o = self
            .s
            .nix("nix")
            .args(["flake", "lock", "--store", &store])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(o.status.success(), "lock {dir}: {}", last(&stderr(&o), 3));
        let r = format!("{dir}#v");
        let exp = stdout(&self.nix(&[
            "eval",
            "--store",
            &store,
            "--raw",
            "--option",
            "eval-cache",
            "false",
            &r,
        ]));
        let e = format!("(builtins.getFlake \"{dir}\").inputs.{input}.outPath");
        let path =
            stdout(&self.nix(&["eval", "--store", &store, "--raw", "--impure", "--expr", &e]));
        (exp, path)
    }

    fn lock(&self, dir: &str) -> Output {
        self.s
            .nix("nix")
            .args(["flake", "lock"])
            .current_dir(dir)
            .output()
            .unwrap()
    }

    fn cache_lines(&self, r: &str) -> String {
        let o = self.iets_eval(&[], &["--raw", "--timings", r]);
        let lines: Vec<String> = stderr(&o)
            .lines()
            .filter(|l| l.starts_with("cache:"))
            .map(String::from)
            .collect();
        lines.join("\n")
    }

    fn nar_hash(&self, dir: &str) -> String {
        stdout(&self.nix(&["hash", "path", "--type", "sha256", "--sri", dir]))
    }
}

/// `lib`: a git flake with a greeting.
fn lib_repo(f: &F) -> String {
    let lib = f.work().join("lib");
    write(
        &lib.join("flake.nix"),
        "{ outputs = { self }: { greeting = \"hello-\" + self.shortRev; }; }\n",
    );
    git(&lib, &["init", "-q"]);
    git(&lib, &["add", "-A"]);
    git(&lib, &["commit", "-q", "-m", "lib"]);
    lib.to_str().unwrap().to_string()
}

/// `sub`: a path flake with a value.
fn sub_flake(f: &F) {
    write(
        &f.work().join("sub/flake.nix"),
        "{ outputs = { self }: { value = \"sub-\" + self.narHash; }; }\n",
    );
}

/// Locked inputs of every offline type: the test packages (`path:`), a git
/// repo, a path flake whose `lib` follows ours, and a `file://` tarball.
#[test]
fn inputs_of_every_type() {
    let Some(mut f) = F::new() else { return };
    let lib = lib_repo(&f);
    let work = f.w("");
    write(
        &f.work().join("util/flake.nix"),
        &format!(
            "{{\n  inputs.lib.url = \"git+file://{lib}\";\n  outputs = {{ self, lib }}: {{ twice = lib.greeting + lib.greeting; inherit lib; }};\n}}\n"
        ),
    );
    write(
        &f.work().join("tbsrc/flake.nix"),
        &format!(
            "{{ outputs = {{ self }}: {{ value = \"tarball-\" + self.narHash; }}; }}\n# {work}\n"
        ),
    );
    tar(&f.work(), &f.w("tb.tar.gz"), "tbsrc");
    let big = f.w("big");
    let pkgs = f.s.pkgs();
    write(
        &f.work().join("big/flake.nix"),
        &format!(
            r#"{{
  inputs.pkgs.url = "path:{pkgs}";
  inputs.lib.url = "git+file://{lib}";
  inputs.util.url = "path:{work}util";
  inputs.util.inputs.lib.follows = "lib";
  inputs.tb.url = "file://{work}tb.tar.gz";
  outputs = {{ self, pkgs, lib, util, tb }}:
    let
      system = "x86_64-linux";
      chain = builtins.head (builtins.attrValues pkgs.packages);
    in {{
      devShells.${{system}}.default = derivation {{
        name = "shell"; inherit system; builder = "/bin/sh"; deps = [ chain.chain ];
        GREETING = util.twice + " " + tb.value + " " + lib.greeting;
      }};
    }};
}}
"#,
            pkgs = pkgs.display()
        ),
    );
    let lk = f.lock(&big);
    f.verdict(lk.status.success(), || {
        format!("nix flake lock: {}", stderr(&lk))
    });
    let shell = format!("{big}#devShells.x86_64-linux.default.drvPath");
    f.check("big: devShell", &shell, &[]);
    f.check(
        "big: devShell, IETS_WORKERS=8",
        &shell,
        &[("IETS_WORKERS", "8")],
    );

    // an input's metadata is silent until the input has a level of its own;
    // the site is the binding in the root flake
    let quiet = stderr(&f.iets_eval(&[], &["--raw", "--no-tracing-cache", &shell]));
    write(
        &f.work().join("big/iets.toml"),
        "[lint.inputs.lib]\ntree-meta-in-drv = \"warn\"\n",
    );
    let loud = stderr(&f.iets_eval(&[], &["--raw", "--no-tracing-cache", &shell]));
    let text = fs::read_to_string(f.work().join("big/flake.nix")).unwrap();
    let line = text.lines().position(|l| l.contains("GREETING")).unwrap() + 1;
    let ok = !quiet.contains("tree-meta-in-drv")
        && loud.contains("attribute 'GREETING' holds 'inputs.lib.shortRev', a value that changes with every update of the lock file")
        && loud.contains(&format!("at {big}/flake.nix:{line}:"));
    f.verdict(ok, || format!("tree-meta, input: {quiet} | {loud}"));
}

const SCRATCH: &str = r#"{
  inputs.sub.url = "path:@WORK@/sub";
  inputs.dep.url = "path:@WORK@/dep";
  inputs.dep.inputs.sub.follows = "sub";
  inputs.rel.url = "path:./rel";
  outputs = { self, dep, sub, rel }: {
    packages.x86_64-linux.default = derivation {
      name = "scratch";
      system = "x86_64-linux";
      builder = "/bin/sh";
      args = [ "-c" "echo ${self.narHash} ${self.rev or self.dirtyRev or "norev"} ${self.shortRev or self.dirtyShortRev or "norev"} ${toString self.lastModified} ${self.lastModifiedDate} ${toString (self.revCount or 0)} ${dep.value} ${dep.sub.value} ${dep.outPath} ${sub.outPath} ${rel.value} ${rel.outPath} ${self}/file ${builtins.readFile (self + "/file")} ${toString (builtins.attrNames (builtins.readDir self))} ${toString (builtins.pathExists (self + "/untracked"))} ${import ./nested/lib.nix} ${./nested} > $out" ];
    };
    visible = builtins.toJSON {
      context = builtins.getContext "${self.narHash} ${self.lastModifiedDate}";
      has = builtins.hasContext (builtins.substring 0 8 self.lastModifiedDate);
      equal = self.narHash == builtins.unsafeDiscardStringContext self.narHash;
      json = builtins.toJSON { inherit (self) narHash lastModifiedDate; };
      name = { ${self.lastModifiedDate} = builtins.replaceStrings [ "-" ] [ "+" ] self.narHash; };
      file = builtins.toFile "meta" self.narHash;
    };
  };
}
"#;

/// A git flake with a `path:` input, a relative `path:` input (no narHash in
/// the lock) and a `follows`, in every state a working tree can be in.
#[test]
fn a_git_working_tree() {
    let Some(mut f) = F::new() else { return };
    let w = f.work();
    let work = f.w("");
    let work = work.trim_end_matches('/');
    sub_flake(&f);
    write(
        &w.join("dep/flake.nix"),
        &format!(
            "{{\n  inputs.sub.url = \"path:{work}/sub\";\n  outputs = {{ self, sub }}: {{ value = \"dep-\" + sub.value; inherit sub; }};\n}}\n"
        ),
    );
    let repo = w.join("repo");
    let r = repo.to_str().unwrap().to_string();
    write(&repo.join("flake.nix"), &SCRATCH.replace("@WORK@", work));
    write(&repo.join("file"), "one\n");
    write(&repo.join("nested/tracked"), "two\n");
    write(&repo.join("nested/lib.nix"), "\"lib\"\n");
    write(
        &repo.join("rel/flake.nix"),
        "{ outputs = { self }: { value = \"rel-\" + builtins.readFile (self + \"/data\"); }; }\n",
    );
    write(&repo.join("rel/data"), "reldata");
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    f.lock(&r);
    git(&repo, &["add", "flake.lock"]);
    git(&repo, &["commit", "-q", "-m", "lock"]);
    let scratch = format!("{r}#packages.x86_64-linux.default");
    let drv = format!("{scratch}.drvPath");

    // Nix writes a relative path node without a narHash
    let lock: Value =
        serde_json::from_str(&fs::read_to_string(repo.join("flake.lock")).unwrap()).unwrap();
    let rel = lock["nodes"]["rel"]["locked"].clone();
    f.verdict(rel == json!({ "path": "./rel", "type": "path" }), || {
        format!("lock: rel is {rel}")
    });

    // a worktree at a path nix never copied: the root tree is never fetched
    let fresh = f.w("fresh");
    git(&repo, &["worktree", "add", "-q", &fresh, "HEAD"]);
    write(Path::new(&format!("{fresh}/untracked")), "untracked\n");
    let fresh_drv = format!("{fresh}#packages.x86_64-linux.default.drvPath");
    f.check_fresh("worktree: clean tree nix never saw", &fresh_drv);
    // pinned by rev, as CI names it: the source is the commit
    let rev = stdout(&git(Path::new(&fresh), &["rev-parse", "HEAD"]));
    let pinned =
        format!("git+file://{fresh}?ref=HEAD&rev={rev}#packages.x86_64-linux.default.drvPath");
    f.check("worktree: pinned by rev", &pinned, &[]);
    write(Path::new(&format!("{fresh}/file")), "changed\n");
    f.check_fresh("worktree: dirty tree nix never saw", &fresh_drv);
    f.check(
        "worktree: dirty tree, pinned rev is still the commit",
        &pinned,
        &[],
    );
    git(&repo, &["worktree", "remove", "--force", &fresh]);

    f.check("scratch: clean tree", &drv, &[]);
    f.check(
        "scratch: tree metadata strings have no context",
        &format!("{r}#visible"),
        &[],
    );
    f.check(
        "scratch: --raw on a derivation prints its outPath",
        &scratch,
        &[],
    );
    write(&repo.join("untracked"), "untracked\n");
    f.check("scratch: an untracked file is ignored", &drv, &[]);

    // reading an untracked file is Nix's error, and the hint is the git add
    let ut = w.join("untracked");
    write(
        &ut.join("flake.nix"),
        "{ outputs = _: { v = builtins.readFile ./extra; }; }",
    );
    write(&ut.join("flake.lock"), LOCK);
    write(&ut.join("extra"), "x\n");
    git(&ut, &["init", "-q"]);
    git(&ut, &["add", "flake.nix", "flake.lock"]);
    let u = ut.to_str().unwrap();
    let line = format!("Path 'extra' in the repository \"{u}\" is not tracked by Git.");
    let n = f.nix(&["eval", "--raw", &format!("{u}#v")]);
    let i = f.iets_eval(
        &[],
        &[
            "--raw",
            "--no-tracing-cache",
            "--log-format",
            "internal-json",
            &format!("{u}#v"),
        ],
    );
    let rec = records(&stderr(&i))
        .into_iter()
        .find(|r| r["level"] == 0)
        .unwrap_or_default();
    let ok = n.status.code() == Some(1)
        && stderr(&n).contains(&line)
        && i.status.code() == Some(1)
        && rec["raw_msg"]
            .as_str()
            .is_some_and(|m| m.starts_with(&line))
        && rec["hint"] == format!("run 'git -C \"{u}\" add \"extra\"'");
    f.verdict(ok, || format!("untracked read: {rec}"));

    write(&repo.join("file"), "three\n");
    f.check("scratch: dirty tree (modified tracked file)", &drv, &[]);
    let held = stderr(&f.iets_eval(&[], &["--raw", "--no-tracing-cache", &drv]));
    f.verdict(
        held.contains(
            "holds 'self.dirtyRev', 'self.dirtyShortRev', 'self.lastModifiedDate', 'self.narHash'",
        ),
        || format!("tree-meta, dirty: {held}"),
    );
    fs::remove_file(repo.join("nested/tracked")).unwrap();
    f.check("scratch: dirty tree (deleted tracked file)", &drv, &[]);
    write(&repo.join("added"), "four\n");
    git(&repo, &["add", "added"]);
    f.check("scratch: dirty tree (staged new file)", &drv, &[]);
    git(&repo, &["commit", "-q", "-am", "next"]);
    f.check("scratch: clean again after commit", &drv, &[]);
    let held = stderr(&f.iets_eval(&[], &["--raw", "--no-tracing-cache", &drv]));
    f.verdict(
        held.contains("[tree-meta-in-drv] derivation 'scratch': attribute 'args' holds 'self.lastModifiedDate', 'self.narHash', 'self.rev', 'self.shortRev'"),
        || format!("tree-meta, clean: {held}"),
    );
    f.check(
        "scratch: subdirectory ref",
        &format!("{r}/.#packages.x86_64-linux.default.drvPath"),
        &[],
    );
    f.check(
        "scratch: path: copies the whole tree",
        &format!("path:{r}#packages.x86_64-linux.default.drvPath"),
        &[],
    );

    let exp = f.nix(&["eval", "--json", "--option", "eval-cache", "false", &drv]);
    let got = f.iets_eval(&[], &["--json", "--no-tracing-cache", &drv]);
    f.verdict(!exp.stdout.is_empty() && exp.stdout == got.stdout, || {
        "scratch: --json".into()
    });

    // tracing cache: a hit after a miss, a miss after the tree changes
    f.iets_eval(&[], &["--raw", &drv]);
    let st = f.cache_lines(&drv);
    f.verdict(st.contains("cache: hit"), || {
        format!("cache: second run ({st})")
    });
    // the date and the count follow from the revision: a validation starts
    // no `git log` and no `git rev-list`
    let mut cmds = Vec::new();
    for e in fs::read_dir(w.join("cache/trace")).unwrap() {
        let entry = e.unwrap().path().join("entry");
        if !entry.exists() {
            continue;
        }
        let dump = e2e::ok(f.iets().args(["cache", "dump"]).arg(&entry));
        for l in stdout(&dump).lines() {
            if let Some(rest) = l.strip_prefix(&format!("cmd {r} ")) {
                let sub = rest.split(' ').skip(1).find(|w| !w.starts_with('-'));
                cmds.extend(sub.map(String::from));
            }
        }
    }
    cmds.sort();
    cmds.dedup();
    f.verdict(cmds == ["ls-files", "rev-parse", "status"], || {
        format!("git observations: {cmds:?}")
    });
    write(&repo.join("file"), "five\n");
    let st = f.cache_lines(&drv);
    f.verdict(st.contains("miss ("), || {
        format!("cache: edit misses ({st})")
    });
    git(&repo, &["commit", "-q", "-am", "five"]);
    let st = f.cache_lines(&drv);
    f.verdict(st.contains("miss (") && st.contains("git"), || {
        format!("cache: a commit misses on git ({st})")
    });

    fs::remove_file(repo.join("flake.lock")).unwrap();
    let err = stderr(&f.iets_eval(&[], &["--raw", "--no-tracing-cache", &drv]));
    f.verdict(
        err.contains("flake.lock missing or incomplete, run nix flake lock"),
        || format!("missing flake.lock: {err}"),
    );
}

const COUNT: &str = r#"{
  inputs.reader.url = "path:@WORK@/reader";
  outputs = { self, reader }:
    let
      s = self;
      f = { self, ... }: self.revCount;
      info = removeAttrs self.sourceInfo [ "outPath" "submodules" ];
      name = "rev" + "Count";
    in {
    meta = builtins.toJSON {
      count = self.revCount or null;
      has = self ? revCount;
      names = builtins.attrNames self.sourceInfo;
      date = self.lastModified;
    };
    names = builtins.toJSON [ (self ? revCount) (builtins.attrNames self) (let n = self.revCount; in 1) ];
    again = toString self.revCount;
    twice = toString (self.revCount + self.sourceInfo.revCount);
    site = {
      select = toString self.revCount;
      default = toString (self.revCount or 0);
      inherited = toString { inherit (self) revCount; }.revCount;
      info = toString self.sourceInfo.revCount;
      bound = toString s.revCount;
      argument = toString (f { inherit self; });
      builtin = toString (builtins.getAttr "revCount" self);
      input = toString (reader.count self);
    };
    forced = {
      json = builtins.toJSON info;
      values = toString (builtins.attrValues info);
      computed = toString self.${name};
      mapped = toString (builtins.mapAttrs (n: v: v) info).revCount;
      scope = with self; toString revCount;
    };
    silent = {
      allowed = toString self.revCount; # iets: allow rev-count
      other = toString { revCount = 2; }.revCount;
      whole = builtins.toJSON self;
    };
    all = builtins.toJSON [ self.site self.forced self.silent ];
  };
}
"#;

/// revCount is there when Nix has it, and counted only when it is read.
#[test]
fn rev_count() {
    let Some(mut f) = F::new() else { return };
    let w = f.work();
    let work = f.w("");
    let work = work.trim_end_matches('/');
    write(
        &w.join("reader/flake.nix"),
        "{ outputs = { self }: { count = t: t.revCount; }; }\n",
    );
    let cnt = w.join("count");
    let c = cnt.to_str().unwrap().to_string();
    write(&cnt.join("flake.nix"), &COUNT.replace("@WORK@", work));
    f.lock(&c);
    git(&cnt, &["init", "-q"]);
    git(&cnt, &["add", "-A"]);
    git(&cnt, &["commit", "-q", "-m", "one"]);
    git(&cnt, &["commit", "-q", "--allow-empty", "-m", "two"]);
    f.check("revCount: clean tree", &format!("{c}#meta"), &[]);
    let shallow = f.w("count-shallow");
    git(
        &w,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{c}"),
            &shallow,
        ],
    );
    f.check("revCount: shallow clone", &format!("{shallow}#meta"), &[]);

    // `git rev-list` fails here: a run that does not read the count starts
    // none, and one that reads it takes the count the store kept
    let nolist = w.join("nolist");
    let real = e2e::which("git").unwrap();
    write(
        &nolist.join("git"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in rev-list) exit 9 ;; esac\nexec {} \"$@\"\n",
            real.display()
        ),
    );
    fs::set_permissions(nolist.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", nolist.display(), std::env::var("PATH").unwrap());
    let no_list = [("PATH", path.as_str())];
    let lazy = f.iets_eval(
        &no_list,
        &["--raw", "--no-tracing-cache", &format!("{c}#names")],
    );
    let none = f.iets_eval(
        &no_list,
        &["--raw", "--no-tracing-cache", &format!("{c}#again")],
    );
    f.iets_eval(&[], &["--raw", &format!("{c}#meta")]);
    let kept = f.iets_eval(&no_list, &["--raw", &format!("{c}#again")]);
    let ok = lazy.status.success()
        && stdout(&lazy).starts_with("[true,")
        && none.status.code() == Some(1)
        && stderr(&none).contains("`git rev-list --count")
        && kept.status.success()
        && stdout(&kept) == "2";
    f.verdict(ok, || {
        format!(
            "revCount lazy: {} / {} / {}",
            stdout(&lazy),
            last(&stderr(&none), 2),
            last(&stderr(&kept), 2)
        )
    });

    // one finding per tree, with its site, from a cache hit too
    let finding = "[rev-count] 'self.revCount' is deprecated: the count of all revisions is expensive to compute";
    let lints = |o: &Output| -> Vec<Value> {
        records(&stderr(o))
            .into_iter()
            .filter(|r| {
                r["code"] == "lint"
                    && r["raw_msg"]
                        .as_str()
                        .is_some_and(|m| m.starts_with(finding))
            })
            .collect()
    };
    let cache = f.w("count-cache");
    let args = [
        "--raw",
        "--timings",
        "--log-format",
        "internal-json",
        "--cache-dir",
        &cache,
        &format!("{c}#twice"),
    ];
    let r1 = f.iets_eval(&[], &args);
    let r2 = f.iets_eval(&[], &args);
    let (l1, l2) = (lints(&r1), lints(&r2));
    let ok = stdout(&r1) == "4"
        && stdout(&r2) == "4"
        && l1.len() == 1
        && l2.len() == 1
        && !l1[0]["start"].is_null()
        && l1[0]["start"] == l2[0]["start"]
        && stderr(&r2).contains("cache: hit")
        && !stderr(&lazy).contains("rev-count");
    f.verdict(ok, || format!("rev-count findings: {l1:?} | {l2:?}"));
    let r = f.iets_eval(
        &[],
        &[
            "--raw",
            "--no-tracing-cache",
            "--lint",
            "rev-count=error",
            &format!("{c}#again"),
        ],
    );
    f.verdict(
        r.status.code() == Some(1) && stderr(&r).starts_with(&format!("error: {finding}")),
        || format!("rev-count=error: {}", stderr(&r)),
    );

    // a selection of the count is the site, in an input's code too
    let covered = |attr: &str| -> String {
        let o = f.iets_eval(
            &[],
            &[
                "--raw",
                "--no-tracing-cache",
                "--log-format",
                "internal-json",
                &format!("{c}#{attr}"),
            ],
        );
        let recs = lints(&o);
        if !o.status.success() || recs.len() != 1 {
            return format!(
                "exit {}, {} findings",
                o.status.code().unwrap_or(-1),
                recs.len()
            );
        }
        let r = &recs[0];
        let (Some(start), Some(end)) = (r["start"].as_u64(), r["end"].as_u64()) else {
            return "no site".into();
        };
        if r["line"].is_null() {
            return "no site".into();
        }
        let file = r["file"].as_str().unwrap();
        let file = file.rsplitn(3, ':').last().unwrap();
        let bytes = fs::read(file).unwrap();
        String::from_utf8(bytes[start as usize..end as usize].to_vec()).unwrap()
    };
    let want = [
        ("site.select", "self.revCount"),
        ("site.default", "self.revCount or 0"),
        ("site.inherited", "revCount"),
        ("site.info", "self.sourceInfo.revCount"),
        ("site.bound", "s.revCount"),
        ("site.argument", "self.revCount"),
        ("site.builtin", "builtins.getAttr \"revCount\" self"),
        ("site.input", "t.revCount"),
        ("forced.json", "no site"),
        ("forced.values", "no site"),
        ("forced.computed", "no site"),
        ("forced.mapped", "no site"),
        ("forced.scope", "no site"),
        ("silent.allowed", "exit 0, 0 findings"),
        ("silent.other", "exit 0, 0 findings"),
        ("silent.whole", "exit 0, 0 findings"),
    ];
    let bad: Vec<String> = want
        .iter()
        .filter_map(|(attr, text)| {
            let got = covered(attr);
            (got != *text).then(|| format!("{attr}: {got}"))
        })
        .collect();
    f.verdict(bad.is_empty(), || {
        format!("rev-count sites: {}", bad.join(" | "))
    });
    f.check(
        "revCount: each spelling gives Nix's value",
        &format!("{c}#all"),
        &[],
    );

    // the count of a locked input comes from the lock, silent without a level
    let lib = lib_repo(&f);
    let cin = f.w("count-in");
    write(
        Path::new(&format!("{cin}/flake.nix")),
        &format!(
            "{{ inputs.lib.url = \"git+file://{lib}\"; outputs = {{ self, lib }}: {{ n = toString lib.revCount; }}; }}\n"
        ),
    );
    f.lock(&cin);
    f.check("revCount: locked input", &format!("{cin}#n"), &[]);
    let quiet = stderr(&f.iets_eval(&[], &["--raw", "--no-tracing-cache", &format!("{cin}#n")]));
    write(
        Path::new(&format!("{cin}/iets.toml")),
        "[lint.inputs.lib]\nrev-count = \"warn\"\n",
    );
    let loud = stderr(&f.iets_eval(&[], &["--raw", "--no-tracing-cache", &format!("{cin}#n")]));
    let ok = !quiet.contains("rev-count")
        && loud.contains("warning: [rev-count] 'inputs.lib.revCount' is deprecated")
        && loud.contains(&format!("\n  at {cin}/flake.nix:1:"));
    f.verdict(ok, || format!("rev-count, input: {quiet} | {loud}"));

    let mut text = fs::read_to_string(cnt.join("flake.nix")).unwrap();
    text.push('\n');
    fs::write(cnt.join("flake.nix"), text).unwrap();
    f.check("revCount: dirty tree", &format!("{c}#meta"), &[]);
}

/// A tarball input: Nix locks it in a store of its own, and iets
/// downloads, unpacks, hashes and keeps the tree in its input store.
#[test]
fn iets_fetches_a_tarball_input() {
    let Some(mut f) = F::new() else { return };
    let w = f.work();
    let work = f.w("");
    let work = work.trim_end_matches('/').to_string();
    let src = w.join("tb2src");
    write(&src.join("a"), &format!("hello {work}\n"));
    write(&src.join("run"), "#!/bin/sh\n");
    fs::set_permissions(src.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
    write(&src.join("sub/b"), "x\n");
    symlink("a", src.join("link")).unwrap();
    tar(&w, &format!("{work}/tb2.tar.gz"), "tb2src");
    let tbf = f.w("tbflake");
    write(
        Path::new(&format!("{tbf}/flake.nix")),
        &format!(
            r#"{{
  inputs.tb.url = "file://{work}/tb2.tar.gz";
  inputs.tb.flake = false;
  outputs = {{ self, tb }}: {{ v = toString (builtins.attrNames (builtins.readDir tb)) + " " + builtins.readFile (tb + "/a"); }};
}}
"#
        ),
    );
    let (exp, tb_path) = f.lock_apart(&tbf, "tb", "tbstore");
    f.fetch_only(
        "tarball: into iets's input store",
        &format!("{tbf}#v"),
        &exp,
    );
    // nothing was registered: the tree lives in the input store
    let base = tb_path.rsplit('/').next().unwrap();
    let entry = w.join("cache/inputs").join(base);
    let packs = fs::read_dir(w.join("cache/inputs/packs"))
        .map(|d| {
            d.filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "pack")
            })
            .count()
        })
        .unwrap_or(0);
    f.verdict(
        !Path::new(&tb_path).exists() && entry.is_file() && packs > 0,
        || format!("input store entry {}", entry.display()),
    );

    // `build --no-build` sends the input's NAR out of the input store. The
    // lock is written by hand, so Nix never fetches this input.
    write(&w.join("tb3src/a"), &format!("hello3 {work}\n"));
    write(&w.join("tb3src/sub/b"), "y\n");
    tar(&w, &format!("{work}/tb3.tar.gz"), "tb3src");
    let tb3 = format!("file://{work}/tb3.tar.gz");
    // dereferenceSingletonDirectory strips the one top-level entry
    let h3 = f.nar_hash(&format!("{work}/tb3src"));
    let upf = f.w("upflake");
    write(
        Path::new(&format!("{upf}/flake.nix")),
        r#"{
  inputs.tb.url = "@URL@";
  inputs.tb.flake = false;
  outputs = { self, tb }: {
    packages.x86_64-linux.default = derivation {
      name = "usestb";
      system = "x86_64-linux";
      builder = "/bin/sh";
      args = [ "-c" "cp ${tb}/a $out" ];
    };
  };
}
"#
        .replace("@URL@", &tb3)
        .as_str(),
    );
    write(
        Path::new(&format!("{upf}/flake.lock")),
        &tarball_lock(&[("tb", &tb3, &h3)]),
    );
    let ub = f
        .iets()
        .args([
            "build",
            "--no-build",
            "--timings",
            "--iets-option",
            "builder",
            "daemon",
        ])
        .arg(format!("{upf}#packages.x86_64-linux.default"))
        .output()
        .unwrap();
    let expr = format!(
        "(builtins.fetchTree {{ type = \"tarball\"; url = \"{tb3}\"; narHash = \"{h3}\"; }}).outPath"
    );
    let tb3_path = stdout(&f.iets_eval(&[], &["--raw", "--no-tracing-cache", "-E", &expr]));
    let valid = f.nix(&["path-info", &tb3_path]).status.success();
    let err = stderr(&ub);
    let resolve = err
        .lines()
        .find(|l| l.starts_with("resolve:"))
        .unwrap_or_default();
    let add = err
        .lines()
        .find(|l| l.starts_with("add:"))
        .unwrap_or_default();
    let bytes: u64 = add
        .split(", ")
        .nth(1)
        .and_then(|b| b.strip_suffix(" bytes"))
        .and_then(|b| b.parse().ok())
        .unwrap_or(0);
    let store = f.s.store_dir();
    let ok = ub.status.success()
        && err.contains("fetching it")
        && resolve.starts_with("resolve: 1 sources")
        && !resolve.contains(store.to_str().unwrap())
        && bytes > 200
        && valid;
    f.verdict(ok, || {
        format!("build --no-build: {resolve} | {add} | {tb3_path} valid={valid}")
    });

    // a filtered `builtins.path` of an input member uploads the NAR that
    // evaluation hashed, which the daemon checks
    write(&w.join("tb4src/sub/b"), "b\n");
    write(&w.join("tb4src/sub/c"), "c\n");
    tar(&w, &format!("{work}/tb4.tar.gz"), "tb4src");
    let tb4 = format!("file://{work}/tb4.tar.gz");
    let h4 = f.nar_hash(&format!("{work}/tb4src"));
    let upg = f.w("upflake2");
    write(
        Path::new(&format!("{upg}/flake.nix")),
        r#"{
  inputs.tb.url = "@URL@";
  inputs.tb.flake = false;
  outputs = { self, tb }: {
    packages.x86_64-linux.default = derivation {
      name = "usestbsub";
      system = "x86_64-linux";
      builder = "/bin/sh";
      args = [ "-c" "cat ${builtins.path { path = "${tb}/sub"; name = "tbsub"; filter = p: t: baseNameOf p != "c"; }}/b > $out" ];
    };
  };
}
"#
        .replace("@URL@", &tb4)
        .as_str(),
    );
    write(
        Path::new(&format!("{upg}/flake.lock")),
        &tarball_lock(&[("tb", &tb4, &h4)]),
    );
    let fb = f
        .iets()
        .args(["build", "--no-build", "--iets-option", "builder", "daemon"])
        .arg(format!("{upg}#packages.x86_64-linux.default"))
        .output()
        .unwrap();
    f.verdict(
        fb.status.success() && stderr(&fb).contains("fetching it"),
        || format!("filtered upload: {}", last(&stderr(&fb), 1)),
    );

    // `nix flake prefetch-inputs`: both inputs land in the input store, and an
    // evaluation then downloads nothing
    write(&w.join("tb5src/a"), &format!("hello5 {work}\n"));
    tar(&w, &format!("{work}/tb5.tar.gz"), "tb5src");
    write(&w.join("tb6src/a"), &format!("hello6 {work}\n"));
    tar(&w, &format!("{work}/tb6.tar.gz"), "tb6src");
    let (tb5, tb6) = (
        format!("file://{work}/tb5.tar.gz"),
        format!("file://{work}/tb6.tar.gz"),
    );
    let (h5, h6) = (
        f.nar_hash(&format!("{work}/tb5src")),
        f.nar_hash(&format!("{work}/tb6src")),
    );
    let pf = f.w("prefetchflake");
    write(
        Path::new(&format!("{pf}/flake.nix")),
        &format!(
            "{{ inputs.a = {{ url = \"{tb5}\"; flake = false; }}; inputs.b = {{ url = \"{tb6}\"; flake = false; }}; outputs = {{ self, a, b }}: {{ v = builtins.readFile (a + \"/a\") + builtins.readFile (b + \"/a\"); }}; }}"
        ),
    );
    write(
        Path::new(&format!("{pf}/flake.lock")),
        &tarball_lock(&[("a", &tb5, &h5), ("b", &tb6, &h6)]),
    );
    let pcache = f.w("prefetchcache");
    let nobin = f.no_nix()[0].1.clone();
    let vars = [
        ("IETS_NIX_BIN_DIR", nobin.as_str()),
        ("PATH", nobin.as_str()),
        ("NIX_DAEMON_SOCKET_PATH", "/nonexistent/iets-no-daemon"),
        ("IETS_CACHE_DIR", pcache.as_str()),
    ];
    let p = f
        .iets()
        .envs(vars)
        .args(["nix", "flake", "prefetch-inputs", &pf])
        .output()
        .unwrap();
    let pe = f.iets_eval(
        &[vars[0], vars[1], vars[2], vars[3], ("IETS_OFFLINE", "1")],
        &["--raw", "--no-tracing-cache", &format!("{pf}#v")],
    );
    let fetched = stderr(&p)
        .lines()
        .filter(|l| l.contains("fetching it"))
        .count();
    let ok = p.status.success()
        && p.stdout.is_empty()
        && fetched == 2
        && pe.status.success()
        && String::from_utf8_lossy(&pe.stdout) == format!("hello5 {work}\nhello6 {work}\n")
        && !stderr(&pe).contains("fetching it");
    f.verdict(ok, || {
        format!(
            "prefetch-inputs: {} / {}",
            last(&stderr(&p), 2),
            last(&stderr(&pe), 2)
        )
    });

    // the lock's narHash is the contract: a changed tarball is refused, and
    // nothing is registered. The input store entry goes first, or the fetch
    // never happens.
    fs::remove_file(&entry).unwrap();
    write(&src.join("a"), "changed\n");
    tar(&w, &format!("{work}/tb2.tar.gz"), "tb2src");
    let env = f.no_nix();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mm = f.iets_eval(&env, &["--raw", "--no-tracing-cache", &format!("{tbf}#v")]);
    f.verdict(
        !mm.status.success()
            && mm.stdout.is_empty()
            && stderr(&mm).contains("NAR hash mismatch")
            && !Path::new(&tb_path).exists(),
        || format!("tarball mismatch: {}", last(&stderr(&mm), 2)),
    );
}

/// One download per tree: a sibling holds the tree's fetch lock and stores
/// the entry before it lets go, so iets waits and reads that entry.
#[cfg(target_os = "linux")]
#[test]
fn a_second_process_waits_for_the_download() {
    let Some(mut f) = F::new() else { return };
    let w = f.work();
    let work = f.w("");
    let work = work.trim_end_matches('/').to_string();
    write(&w.join("tb5src/a"), &format!("hello5 {work}\n"));
    tar(&w, &format!("{work}/tb5.tar.gz"), "tb5src");
    let url = format!("file://{work}/tb5.tar.gz");
    let hash = f.nar_hash(&format!("{work}/tb5src"));
    let wf = f.w("waitflake");
    write(
        Path::new(&format!("{wf}/flake.nix")),
        &format!(
            "{{ inputs.tb = {{ url = \"{url}\"; flake = false; }}; outputs = {{ self, tb }}: {{ v = builtins.readFile (tb + \"/a\"); }}; }}"
        ),
    );
    write(
        Path::new(&format!("{wf}/flake.lock")),
        &tarball_lock(&[("tb", &url, &hash)]),
    );
    let nobin = f.no_nix()[0].1.clone();
    let no_daemon = [
        ("IETS_NIX_BIN_DIR", nobin.as_str()),
        ("NIX_DAEMON_SOCKET_PATH", "/nonexistent/iets-no-daemon"),
    ];
    let src = f.w("waitsrc");
    let first = f.iets_eval(
        &[no_daemon[0], no_daemon[1], ("IETS_CACHE_DIR", &src)],
        &["--raw", "--no-tracing-cache", &format!("{wf}#v")],
    );
    let entry = fs::read_dir(format!("{src}/inputs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.is_file()
                && p.file_name().unwrap().to_str().unwrap().as_bytes().get(32) == Some(&b'-')
        })
        .unwrap();
    let wc = f.w("waitcache");
    mkdir(Path::new(&format!("{wc}/inputs")));
    fs::copy(
        format!("{src}/inputs/version"),
        format!("{wc}/inputs/version"),
    )
    .unwrap();
    let base = entry.file_name().unwrap().to_str().unwrap();
    let lockf = format!("{wc}/inputs/.{base}.fetching.tmp");
    // a handshake, not a sleep: under load, iets could start after the
    // sibling stored the entry and never wait
    let script = r#"
    flock '@LOCK@' sh -c 'touch @WC@/held; for _ in $(seq 600); do grep -qs "waiting for another" @WC@/w.err && break; sleep 0.05; done; cp -r @SRC@/inputs/packs @WC@/inputs/; cp @ENTRY@ @WC@/inputs/' &
    until [ -e @WC@/held ]; do sleep 0.05; done
    '@IETS@' eval --raw --no-tracing-cache '@WF@#v' 2> @WC@/w.err; s=$?
    cat @WC@/w.err >&2; wait; exit $s
    "#
    .replace("@LOCK@", &lockf)
    .replace("@WC@", &wc)
    .replace("@SRC@", &src)
    .replace("@ENTRY@", entry.to_str().unwrap())
    .replace("@IETS@", env!("CARGO_BIN_EXE_iets"))
    .replace("@WF@", &wf);
    let mut c = f.iets();
    let c = c
        .envs(no_daemon)
        .env("IETS_CACHE_DIR", &wc)
        .env("IETS_OFFLINE", "1");
    let mut sh = Command::new("bash");
    for (k, v) in c.get_envs() {
        match v {
            Some(v) => sh.env(k, v),
            None => sh.env_remove(k),
        };
    }
    let o = sh.args(["-c", &script]).output().unwrap();
    let ok = first.status.success()
        && o.status.success()
        && String::from_utf8_lossy(&o.stdout) == format!("hello5 {work}\n")
        && stderr(&o).contains("waiting for another process to fetch input 'tb'");
    f.verdict(ok, || {
        format!("fetch lock: {} {}", stdout(&o), last(&stderr(&o), 2))
    });
}

/// github.com and cache.nixos.org.
#[test]
fn online_inputs() {
    if env::var("IETS_TEST_ONLINE").as_deref() != Ok("1") {
        eprintln!("skipped: IETS_TEST_ONLINE unset");
        return;
    }
    let Some(mut f) = F::new() else { return };
    // a github input over https: one redirect to codeload, gzip
    let ghf = f.w("ghflake");
    write(
        Path::new(&format!("{ghf}/flake.nix")),
        r#"{
  inputs.tiny.url = "github:numtide/flake-utils/471aed544aef9c610ea465cddf4a39c358ac5aa8";
  inputs.tiny.flake = false;
  outputs = { self, tiny }: { v = toString (builtins.attrNames (builtins.readDir tiny)); };
}
"#,
    );
    let r = format!("{ghf}#v");
    let (exp, gh_path) = f.lock_apart(&ghf, "tiny", "ghstore");
    f.verdict(!Path::new(&gh_path).exists(), || {
        format!("github: {gh_path} is in the store already")
    });
    f.fetch_only("github: fetched over https by iets", &r, &exp);

    // no nix, no daemon, a fresh cache: a fetch, then an offline hit
    let nobin = f.no_nix()[0].1.clone();
    let cache = f.w("alone");
    let iets_cache = format!("{cache}/iets");
    let vars = [
        ("IETS_NIX_BIN_DIR", nobin.as_str()),
        ("PATH", nobin.as_str()),
        ("NIX_DAEMON_SOCKET_PATH", "/nonexistent/iets-no-daemon"),
        ("XDG_CACHE_HOME", cache.as_str()),
        ("IETS_CACHE_DIR", iets_cache.as_str()),
    ];
    let one = f.iets_eval(&vars, &["--raw", "--no-tracing-cache", &r]);
    let mut offline = vars.to_vec();
    offline.push(("IETS_OFFLINE", "1"));
    let two = f.iets_eval(&offline, &["--raw", "--no-tracing-cache", &r]);
    let entries = fs::read_dir(format!("{iets_cache}/inputs"))
        .map(|d| d.filter(|e| e.as_ref().unwrap().path().is_dir()).count())
        .unwrap_or(0);
    let ok = !exp.is_empty()
        && stdout(&one) == exp
        && stdout(&two) == exp
        && stderr(&one).contains("fetching it")
        && !stderr(&two).contains("fetching it")
        && entries > 0;
    f.verdict(ok, || {
        format!(
            "standalone: {} / {}",
            last(&stderr(&one), 3),
            last(&stderr(&two), 3)
        )
    });
}

/// iets has no git fetcher, so a missing git input goes to Nix, and without
/// Nix it is an error that names the input. Nix locks in a store of its own,
/// so the daemon's store has the input only once iets's Nix fetches it: the
/// case without Nix runs first.
#[test]
fn a_git_input_goes_through_nix() {
    let Some(mut f) = F::new() else { return };
    let lib = lib_repo(&f);
    let gitf = f.w("gitflake");
    write(
        Path::new(&format!("{gitf}/flake.nix")),
        &format!(
            "{{\n  inputs.lib.url = \"git+file://{lib}\";\n  outputs = {{ self, lib }}: {{ v = lib.greeting; }};\n}}\n"
        ),
    );
    let r = format!("{gitf}#v");
    let (exp, path) = f.lock_apart(&gitf, "lib", "gitstore");
    let env = f.no_nix();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let gn = f.iets_eval(&env, &["--raw", "--no-tracing-cache", &r]);
    f.verdict(
        !gn.status.success()
            && stderr(&gn).contains("nix is not on PATH")
            && stderr(&gn).contains("input 'lib'"),
        || format!("git input, no nix: {}", last(&stderr(&gn), 2)),
    );
    let gf = f.iets_eval(&[], &["--raw", "--no-tracing-cache", &r]);
    f.verdict(
        !exp.is_empty() && stdout(&gf) == exp && Path::new(&path).exists(),
        || format!("git input: {} vs {exp}", stdout(&gf)),
    );

    // a tree that is not the one the lock names: the hint names the causes
    let gm = f.w("gitmismatch");
    fs::create_dir(&gm).unwrap();
    fs::copy(format!("{gitf}/flake.nix"), format!("{gm}/flake.nix")).unwrap();
    let mut lock: Value =
        serde_json::from_str(&fs::read_to_string(format!("{gitf}/flake.lock")).unwrap()).unwrap();
    lock["nodes"]["lib"]["locked"]["narHash"] =
        json!("sha256-aWV0cy1naXQtbWlzbWF0Y2gtbmV2ZXItc3RvcmVkISE=");
    write(Path::new(&format!("{gm}/flake.lock")), &lock.to_string());
    let o = f.iets_eval(
        &[],
        &[
            "--raw",
            "--no-tracing-cache",
            "--log-format",
            "internal-json",
            &format!("{gm}#v"),
        ],
    );
    let rec = records(&stderr(&o))
        .into_iter()
        .find(|r| r["code"] == "hash-mismatch")
        .unwrap_or_default();
    f.verdict(
        o.status.code() == Some(1)
            && rec["hint"]
                .as_str()
                .is_some_and(|h| h.contains("export-subst")),
        || format!("git mismatch: {rec}"),
    );
}

#[test]
fn without_a_lock() {
    let Some(mut f) = F::new() else { return };
    // without inputs, Nix locks to an empty lock in memory and writes no file
    let nl = f.w("nolock");
    write(
        Path::new(&format!("{nl}/flake.nix")),
        r#"{
  outputs = { self }: {
    v = builtins.toJSON { inherit (self) narHash lastModified lastModifiedDate outPath; };
    d = (derivation { name = "nolock"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo ${self} > $out" ]; }).drvPath;
  };
}
"#,
    );
    f.check("no lock: sourceInfo", &format!("path:{nl}#v"), &[]);
    f.check("no lock: drvPath", &format!("path:{nl}#d"), &[]);
    f.verdict(!Path::new(&format!("{nl}/flake.lock")).exists(), || {
        "a flake.lock appeared".into()
    });
    let d = format!("path:{nl}#d");
    f.iets_eval(&[], &["--raw", &d]);
    let st = f.cache_lines(&d);
    f.verdict(st.contains("cache: hit"), || {
        format!("no lock, second run ({st})")
    });
    write(Path::new(&format!("{nl}/flake.lock")), "{\n");
    let bad = f.iets_eval(&[], &["--raw", &d]);
    f.verdict(
        !bad.status.success() && stderr(&bad).contains("flake.lock missing or incomplete"),
        || format!("a lock that appears: {}", last(&stderr(&bad), 2)),
    );

    // a declared input needs a lock entry, which `nix flake lock` writes
    sub_flake(&f);
    let dl = f.w("declared");
    write(
        Path::new(&format!("{dl}/flake.nix")),
        &format!(
            "{{\n  inputs.sub.url = \"path:{}\";\n  outputs = {{ self, sub }}: {{ v = sub.value; }};\n}}\n",
            f.w("sub")
        ),
    );
    let de = f.iets_eval(
        &[],
        &["--raw", "--no-tracing-cache", &format!("path:{dl}#v")],
    );
    f.verdict(
        !de.status.success()
            && stderr(&de).contains("flake.lock missing or incomplete, run nix flake lock (input 'sub' has no lock entry)"),
        || format!("declared: {}", last(&stderr(&de), 2)),
    );
    let lk = f.lock(&dl);
    f.verdict(lk.status.success(), || {
        format!("nix flake lock: {}", stderr(&lk))
    });
    f.check(
        "no lock: evaluates after nix flake lock",
        &format!("path:{dl}#v"),
        &[],
    );

    // a formal of `outputs` that `inputs` does not name is a registry input
    let il = f.w("implicit");
    write(
        Path::new(&format!("{il}/flake.nix")),
        "{ outputs = { self, nixpkgs }: { v = \"x\"; }; }\n",
    );
    let ie = f.iets_eval(
        &[],
        &["--raw", "--no-tracing-cache", &format!("path:{il}#v")],
    );
    f.verdict(
        !ie.status.success()
            && stderr(&ie).contains("(input 'nixpkgs', an argument of 'outputs' that Nix looks up in the flake registry, has no lock entry)"),
        || format!("implicit: {}", last(&stderr(&ie), 2)),
    );
}

/// A locked remote input that no substituter has and no forge serves.
#[test]
fn an_unfetchable_input() {
    let Some(mut f) = F::new() else { return };
    let ghost = f.w("ghost");
    write(
        Path::new(&format!("{ghost}/flake.nix")),
        "{ inputs.ghost.url = \"github:NixOS/ghost\"; outputs = { self, ghost }: { v = ghost.outPath; }; }\n",
    );
    let mut lock = json!({
        "nodes": {
            "ghost": {
                "locked": { "type": "github", "owner": "NixOS", "repo": "ghost", "rev": "0000000000000000000000000000000000000000", "narHash": "sha256-Z2hvc3QtaW5wdXQtbmV2ZXItaW4tdGhlLXN0b3JlISE=" },
                "original": { "type": "github", "owner": "NixOS", "repo": "ghost" },
            },
            "root": { "inputs": { "ghost": "ghost" } },
        },
        "root": "root",
        "version": 7,
    });
    write(Path::new(&format!("{ghost}/flake.lock")), &lock.to_string());
    let env = f.no_nix();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let g = f.iets_eval(
        &env,
        &["--raw", "--no-tracing-cache", &format!("{ghost}#v")],
    );
    let err = stderr(&g);
    let named = err.lines().filter(|l| l.contains("input 'ghost'")).count();
    f.verdict(
        !g.status.success()
            && g.stdout.is_empty()
            && named == 2
            && err.contains("cannot fetch input 'ghost'"),
        || format!("unfetchable: {}", last(&err, 3)),
    );

    // a lock entry without a narHash: the hint names the input and the command
    let unl = f.w("unlocked");
    fs::create_dir(&unl).unwrap();
    fs::copy(format!("{ghost}/flake.nix"), format!("{unl}/flake.nix")).unwrap();
    lock["nodes"]["ghost"]["locked"]
        .as_object_mut()
        .unwrap()
        .remove("narHash");
    write(Path::new(&format!("{unl}/flake.lock")), &lock.to_string());
    let u = f.iets_eval(
        &[],
        &[
            "--raw",
            "--no-tracing-cache",
            "--log-format",
            "internal-json",
            &format!("{unl}#v"),
        ],
    );
    let rec = records(&stderr(&u))
        .into_iter()
        .find(|r| r["level"] == 0)
        .unwrap_or_default();
    f.verdict(
        u.status.code() == Some(3)
            && rec["hint"]
                .as_str()
                .is_some_and(|h| h.contains("run 'nix flake update ghost'")),
        || format!("unlocked entry: {rec}"),
    );

    let p = f
        .iets()
        .envs(env.iter().copied())
        .args(["nix", "flake", "prefetch-inputs", &ghost])
        .output()
        .unwrap();
    f.verdict(
        p.status.code() == Some(1) && stderr(&p).contains("cannot fetch input 'ghost'"),
        || format!("prefetch-inputs ghost: {}", last(&stderr(&p), 2)),
    );
}

/// iets reads the root tree from the working tree, so a link must resolve
/// where the store copy puts it.
#[test]
fn symlinks_in_a_working_tree() {
    let Some(mut f) = F::new() else { return };
    let w = f.work();
    write(&w.join("outside.txt"), "outside\n");
    let sym = w.join("sym");
    write(
        &sym.join("flake.nix"),
        r#"{
  outputs = { self }: {
    esc = builtins.readFile (self + "/esc");
    abs = builtins.readFile (self + "/abs");
    unt = builtins.readFile (self + "/unt");
    ok = builtins.readFile (self + "/okdir/t.txt");
    okdir = builtins.toJSON (builtins.readDir (self + "/okdir"));
    okex = builtins.toJSON (builtins.pathExists (self + "/okdir"));
    nar = self.narHash;
  };
}
"#,
    );
    write(&sym.join("flake.lock"), LOCK);
    write(&sym.join("d/t.txt"), "tracked\n");
    write(&sym.join("untracked.txt"), "untracked\n");
    symlink("../outside.txt", sym.join("esc")).unwrap();
    symlink("/nonexistent/iets-symlink-test", sym.join("abs")).unwrap();
    symlink("untracked.txt", sym.join("unt")).unwrap();
    symlink("d", sym.join("okdir")).unwrap();
    git(&sym, &["init", "-q"]);
    git(
        &sym,
        &[
            "add",
            "flake.nix",
            "flake.lock",
            "d/t.txt",
            "esc",
            "abs",
            "unt",
            "okdir",
        ],
    );
    git(&sym, &["commit", "-q", "-m", "sym"]);
    let s = sym.to_str().unwrap();
    f.check_error(
        "sym: a link out of the tree is refused",
        &format!("{s}#esc"),
    );
    f.check_error("sym: an absolute link is refused", &format!("{s}#abs"));
    f.check_error(
        "sym: a link to an untracked file is Nix's git error",
        &format!("{s}#unt"),
    );
    f.check(
        "sym: readFile through a tracked link",
        &format!("{s}#ok"),
        &[],
    );
    f.check("sym: readDir of a tracked link", &format!("{s}#okdir"), &[]);
    f.check(
        "sym: pathExists of a tracked link",
        &format!("{s}#okex"),
        &[],
    );
    // a flake directory that is a link is the working tree it points to
    symlink("sym", w.join("sym-link")).unwrap();
    f.check(
        "sym: a flake directory that is a link",
        &format!("{}#nar", f.w("sym-link")),
        &[],
    );
}

/// libgit2's status calls an index entry that is not on disk deleted, so a
/// sparse checkout is a dirty tree: no rev and no revCount.
#[test]
fn a_sparse_checkout_is_dirty() {
    let Some(mut f) = F::new() else { return };
    let sparse = f.work().join("sparse");
    write(
        &sparse.join("flake.nix"),
        "{ outputs = { self }: { v = builtins.toJSON [ (self.rev or \"none\") (self.dirtyRev or \"none\") self.narHash (self.revCount or (-1)) ]; }; }\n",
    );
    write(&sparse.join("flake.lock"), LOCK);
    write(&sparse.join("d/f.txt"), "x\n");
    write(&sparse.join("top.txt"), "y\n");
    git(&sparse, &["init", "-q"]);
    git(&sparse, &["add", "-A"]);
    git(&sparse, &["commit", "-q", "-m", "init"]);
    let r = format!("{}#v", sparse.display());
    f.check("sparse: the whole tree is clean", &r, &[]);
    git(
        &sparse,
        &["sparse-checkout", "set", "--no-cone", "/*", "!/d/"],
    );
    f.check("sparse: an entry not on disk makes it dirty", &r, &[]);
}

#[test]
fn a_git_working_tree_caches() {
    let Some(mut f) = F::new() else { return };
    // every traced git query must be in `trace::Query`, or the run is
    // uncacheable
    let c = f.work().join("cacheable");
    write(
        &c.join("flake.nix"),
        "{ outputs = { self }: { v = \"c\"; }; }\n",
    );
    write(&c.join("flake.lock"), LOCK);
    git(&c, &["init", "-q"]);
    git(&c, &["add", "-A"]);
    let r = format!("{}#v", c.display());
    let cold = f.iets_eval(&[], &["--timings", "--raw", &r]);
    let warm = f.iets_eval(&[], &["--timings", "--raw", &r]);
    f.verdict(
        stderr(&cold).contains("cache: wrote") && stderr(&warm).contains("cache: hit"),
        || {
            format!(
                "cold {} warm {}",
                last(&stderr(&cold), 2),
                last(&stderr(&warm), 2)
            )
        },
    );

    // the ancestor probes are observations: a cached result must not stay a
    // path flake after `git init` above it
    let above = f.work().join("above");
    write(
        &above.join("fl/flake.nix"),
        "{ outputs = { self }: { v = builtins.toJSON [ (self ? rev) (builtins.pathExists (self + \"/b\")) ]; }; }\n",
    );
    write(&above.join("fl/flake.lock"), LOCK);
    write(&above.join("fl/b"), "b\n");
    let r = format!("{}/fl#v", above.display());
    let before = f.iets_eval(&[], &["--raw", &r]);
    git(&above, &["init", "-q"]);
    let after = f.iets_eval(&[], &["--raw", &r]);
    let real = f.nix_eval(&r);
    let ok = before.status.success()
        && stdout(&before) == "[false,true]"
        && after.status.code() == real.status.code()
        && after.status.code() == Some(1)
        && stderr(&after).contains("is not tracked by Git");
    f.verdict(ok, || {
        format!(
            "above: {} then {}",
            stdout(&before),
            last(&stderr(&after), 2)
        )
    });
}
