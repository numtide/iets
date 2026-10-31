//! The tracing cache under the `nix` name (docs/reference/classic.md). The
//! flake's value reads a file, a tarball input and, when impure, a variable,
//! behind a `builtins.trace`. An evaluation prints the trace and a cache hit
//! does not.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use e2e::classic::{Both, Checks, Farm, link};
use e2e::{Store, ok, stderr, stdout};
use serde_json::Value;

const FLAKE: &str = r#"{
  nixConfig.extra-substituters = [ "https://cache.invalid" ];
  inputs.tb = { url = "file://@CC@/flake/tbA.tar.gz"; flake = false; };
  outputs = { self, tb }:
    let
      inherit (import ./config.nix) system;
      val = builtins.trace "EVALUATED"
        (import ./val.nix + "-" + builtins.readFile "${tb}/a" + "-" + builtins.getEnv "CC_VAR");
    in {
      inherit val;
      packages.${system}.pkg = derivation {
        name = "cc-pkg";
        inherit system val;
        stamp = self.narHash;
        builder = "@SHELL@";
        args = [ "-c" "echo $val > $out" ];
      };
      devShells.${system}.default = derivation ((import ./shell.nix).drvAttrs // {
        inherit val;
        stamp = self.narHash;
      });
    };
}
"#;

struct Cmd {
    name: &'static str,
    args: &'static [&'static str],
    attr: &'static str,
    tail: Vec<String>,
    value: fn(&Output) -> String,
}

fn ran(o: &Output) -> bool {
    stderr(o).contains("trace: EVALUATED")
}

fn fixture(s: &Store) -> (String, PathBuf) {
    let cc = s.dir().join("cc");
    s.write("cc/tbA/a", "A");
    s.write("cc/tbB/a", "B");
    // the archives lie in the flake: a pure evaluation fetches no file
    // outside its root (docs/explanation/pure-eval-and-local-sources.md)
    fs::create_dir_all(cc.join("flake")).unwrap();
    for t in ["tbA", "tbB"] {
        ok(Command::new("tar")
            .current_dir(&cc)
            .args(["-czf", &format!("flake/{t}.tar.gz"), t]));
    }
    s.write(
        "cc/flake/flake.nix",
        &FLAKE.replace("@CC@", cc.to_str().unwrap()),
    );
    for n in ["config.nix", "env.nix", "shell.nix"] {
        fs::copy(s.pkgs().join(n), cc.join("flake").join(n)).unwrap();
    }
    s.write("cc/flake/val.nix", "\"one\"\n");
    (format!("path:{}", cc.join("flake").display()), cc)
}

#[test]
fn the_tracing_cache() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let (r, cc) = fixture(&s);
    let lock_a = cc.join("flake/flake.lock");
    let locked = s
        .nix("nix")
        .current_dir(cc.join("flake"))
        .args(["flake", "lock"])
        .output()
        .unwrap();
    assert!(locked.status.success(), "{}", stderr(&locked));
    let a_text = fs::read_to_string(&lock_a).unwrap();
    // the lock `nix flake update tb` would write after the tarball moved on
    let hash_b = stdout(&ok(s
        .nix("nix")
        .args(["hash", "path", "--type", "sha256", "--sri"])
        .arg(cc.join("tbB"))));
    let mut lb: Value = serde_json::from_str(&a_text).unwrap();
    lb["nodes"]["tb"]["locked"]["url"] = format!("file://{}/flake/tbB.tar.gz", cc.display()).into();
    lb["nodes"]["tb"]["locked"]["narHash"] = hash_b.into();
    let b_text = lb.to_string();

    let shell = e2e::resolved("bash").to_str().unwrap().to_string();
    let cmds = [
        Cmd {
            name: "nix eval",
            args: &["eval", "--raw"],
            attr: "#val",
            tail: vec![],
            value: |o| stdout(o),
        },
        Cmd {
            name: "nix build",
            args: &["build", "--no-link", "--print-out-paths"],
            attr: "#pkg",
            tail: vec![],
            value: |o| fs::read_to_string(stdout(o)).unwrap_or_default(),
        },
        Cmd {
            name: "nix derivation show",
            args: &["derivation", "show"],
            attr: "#pkg",
            tail: vec![],
            value: |o| {
                let j: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
                j["derivations"]
                    .as_object()
                    .and_then(|m| m.values().next())
                    .and_then(|d| d["env"]["val"].as_str())
                    .unwrap_or_default()
                    .to_string()
            },
        },
        Cmd {
            name: "nix develop",
            args: &["develop"],
            attr: "",
            tail: vec!["-c".into(), shell, "-c".into(), r#"echo "$val""#.into()],
            value: |o| stdout(o),
        },
    ];
    let cache = cc.join("cache");
    let entries = || e2e::ls(&cache.join("trace")).len();
    for cmd in &cmds {
        let target = format!("{r}{}", cmd.attr);
        let one = |flags: &[&str], env: &[(&str, &str)]| {
            f.ours("nix")
                .env("IETS_CACHE_DIR", &cache)
                .envs(env.iter().copied())
                .args(cmd.args)
                .args(flags)
                .arg(&target)
                .args(&cmd.tail)
                .output()
                .unwrap()
        };
        let label = |l: &str| format!("{}: {l}", cmd.name);
        let value = cmd.value;
        s.write("cc/flake/val.nix", "\"one\"\n");
        fs::write(&lock_a, &a_text).unwrap();

        let cold = one(&[], &[]);
        let real = f
            .real("nix")
            .args(cmd.args)
            .arg(&target)
            .args(&cmd.tail)
            .output()
            .unwrap();
        c.same(
            &label("a cold run prints what nix prints"),
            &Both {
                ours: cold.clone(),
                real: real.clone(),
            },
        );
        c.check(
            &label("a cold run evaluates"),
            ran(&cold) && value(&cold).contains("one-A-"),
            stderr(&cold),
        );
        let hit = one(&[], &[]);
        c.same(
            &label("a hit prints what nix prints"),
            &Both {
                ours: hit.clone(),
                real,
            },
        );
        c.check(
            &label("the second run is a hit with the same bytes"),
            !ran(&hit) && hit.stdout == cold.stdout,
            stderr(&hit),
        );
        // `stamp` is what `tree-meta-in-drv` reports under the iets names
        c.check(
            &label("no lint in classic mode"),
            !(stderr(&cold) + &stderr(&hit)).contains("warning: ["),
            stderr(&cold),
        );
        // the warnings come from the command line, so a hit gives them too;
        // accept-flake-config's because the entry kept the flake's nixConfig
        let noisy = || {
            one(
                &["--accept-flake-config", "--eval-store", "auto"],
                &[(
                    "NIX_CONFIG",
                    "accept-flake-config = true\nno-such-setting = 1",
                )],
            )
        };
        noisy();
        let w = noisy();
        let each_once = [
            "'--accept-flake-config' is ignored:",
            "'--eval-store' is ignored:",
            "setting 'accept-flake-config' is ignored:",
            "unknown setting 'no-such-setting'",
        ]
        .iter()
        .all(|t| {
            stderr(&w)
                .lines()
                .filter(|l| l.starts_with(&format!("warning: {t}")))
                .count()
                == 1
        });
        c.check(
            &label("a hit gives every warning once"),
            !ran(&w) && each_once && w.status.success(),
            stderr(&w),
        );

        s.write("cc/flake/val.nix", "\"two\"\n");
        let jlog = cc.join("jlog");
        let _ = fs::remove_file(&jlog);
        let edit = one(&["--option", "json-log-path", jlog.to_str().unwrap()], &[]);
        let recs = fs::read_to_string(&jlog)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(r#""code":"cache-invalidated""#))
            .count();
        c.check(
            &label("an edit of a file the flake reads evaluates again, and then hits"),
            ran(&edit) && value(&edit).contains("two-A-") && !ran(&one(&[], &[])),
            format!("value {}: {}", value(&edit), stderr(&edit)),
        );
        c.check(
            &label("the invalidation is silent on stderr and one json-log-path record"),
            !stderr(&edit).contains("re-evaluating") && recs == 1,
            format!("{recs} records: {}", stderr(&edit)),
        );

        fs::write(&lock_a, &b_text).unwrap();
        let relock = one(&[], &[]);
        c.check(
            &label("another flake.lock evaluates again, and then hits"),
            ran(&relock) && value(&relock).contains("two-B-") && !ran(&one(&[], &[])),
            format!("value {}: {}", value(&relock), stderr(&relock)),
        );
        fs::write(&lock_a, &a_text).unwrap();

        let ia = one(&["--impure"], &[("CC_VAR", "a")]);
        let ihit = one(&["--impure"], &[("CC_VAR", "a")]);
        let ib = one(&["--impure"], &[("CC_VAR", "b")]);
        c.check(
            &label("--impure: its own entry, and a changed variable evaluates again"),
            ran(&ia)
                && value(&ia).contains("two-A-a")
                && !ran(&ihit)
                && ran(&ib)
                && value(&ib).contains("two-A-b"),
            format!("a {}, b {}, hit {}", value(&ia), value(&ib), stderr(&ihit)),
        );

        let before = entries();
        let tbb = format!("path:{}/tbB", cc.display());
        let over: Vec<&str> = [cmd.args, &["--override-input", "tb", &tbb, &target]].concat();
        let why = f.reason(
            "nix",
            &[
                &over[..],
                &cmd.tail.iter().map(String::as_str).collect::<Vec<_>>(),
            ]
            .concat(),
        );
        c.check(
            &label("--override-input is nix's: no entry is read or written"),
            why.contains("'--override-input' is not implemented") && entries() == before,
            format!("{why}, {before} -> {} entries", entries()),
        );
    }

    // a hit skips the evaluation only: the out-link and the roots of a
    // `--no-link` build and of a shell are made again when they are gone
    let roots = || e2e::ls(&cache.join("roots")).len();
    let nix = |a: &[&str], cwd: &Path| {
        f.ours("nix")
            .env("IETS_CACHE_DIR", &cache)
            .current_dir(cwd)
            .args(a)
            .output()
            .unwrap()
    };
    let pkg = format!("{r}#pkg");
    nix(&["build", "--no-link", &pkg], s.dir());
    nix(&["develop", &r, "-c", "true"], s.dir());
    fs::remove_dir_all(cache.join("roots")).unwrap();
    let nolink = nix(&["build", "--no-link", "--print-out-paths", &pkg], s.dir());
    c.check(
        "nix build --no-link: a hit keeps the root",
        !ran(&nolink) && roots() == 1,
        stderr(&nolink),
    );
    let res = cc.join("res");
    let linked = nix(&["build", "-o", res.to_str().unwrap(), &pkg], s.dir());
    c.check(
        "nix build -o: a hit makes the out-link",
        !ran(&linked) && link(&res) == stdout(&nolink) && res.exists(),
        stderr(&linked),
    );
    fs::remove_dir_all(cache.join("roots")).unwrap();
    let sh = nix(&["develop", &r, "-c", "true"], s.dir());
    c.check(
        "nix develop: a hit keeps the root",
        !ran(&sh) && sh.status.success() && roots() == 1,
        format!("{} roots: {}", roots(), stderr(&sh)),
    );

    // nix-build: a hit prints and links what the run that wrote it did
    let nb = cc.join("nb");
    s.write("cc/nb/default.nix", NB);
    s.write("cc/nb/val.nix", "\"one\"\n");
    let nbv = |o: &Output| fs::read_to_string(stdout(o)).unwrap_or_default();
    let nb_one = |flags: &[&str]| {
        f.ours("nix-build")
            .env("IETS_CACHE_DIR", &cache)
            .arg("--no-out-link")
            .args(flags)
            .arg(&nb)
            .output()
            .unwrap()
    };
    let nb_cold = nb_one(&[]);
    let nb_real = f
        .real("nix-build")
        .arg("--no-out-link")
        .arg(&nb)
        .output()
        .unwrap();
    c.same(
        "nix-build: a cold run prints what nix prints",
        &Both {
            ours: nb_cold.clone(),
            real: nb_real,
        },
    );
    c.check(
        "nix-build: a cold run evaluates",
        ran(&nb_cold) && nbv(&nb_cold).contains("one"),
        stderr(&nb_cold),
    );
    let nb_hit = nb_one(&[]);
    c.check(
        "nix-build: the second run is a hit with the same bytes",
        !ran(&nb_hit) && nb_hit.status.success() && nb_hit.stdout == nb_cold.stdout,
        stderr(&nb_hit),
    );
    s.write("cc/nb/val.nix", "\"two\"\n");
    let nb_edit = nb_one(&[]);
    let nb_arg = nb_one(&["--argstr", "v", "x"]);
    c.check(
        "nix-build: an edit and another --argstr evaluate again",
        ran(&nb_edit)
            && nbv(&nb_edit).contains("two")
            && ran(&nb_arg)
            && nbv(&nb_arg).contains("twox"),
        format!("edit {}, arg {}", nbv(&nb_edit), nbv(&nb_arg)),
    );
    let nb_linked = f
        .ours("nix-build")
        .env("IETS_CACHE_DIR", &cache)
        .current_dir(&nb)
        .arg(&nb)
        .output()
        .unwrap();
    let nb_target = link(&nb.join("result"));
    c.check(
        "nix-build: a hit makes the result link",
        !ran(&nb_linked) && nb_target == stdout(&nb_edit),
        format!("link -> {nb_target}: {}", stderr(&nb_linked)),
    );
}

const NB: &str = r#"{ v ? "" }: derivation {
  name = "cc-nb";
  system = "@SYSTEM@";
  val = builtins.trace "EVALUATED" (import ./val.nix + v);
  builder = "@SHELL@";
  args = [ "-c" "echo $val > $out" ];
}
"#;
