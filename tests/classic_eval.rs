//! Classic mode commands that evaluate, against the real Nix: stdout bytes and exit
//! codes are Nix's.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::process::{Command, Output};

use e2e::classic::{Checks, Farm, err_record, first_error, records, show};
use e2e::{Store, stderr, stdout};
use serde_json::Value;

/// `{d}/default.nix`, `multi.nix`, `textrefs.nix`, `floats.nix`,
/// `dyndrv.nix` and `dyndeferred.nix`.
fn drvs(s: &Store) -> String {
    let w = |n: &str, t: &str| s.write(&format!("case/{n}"), t);
    s.write("case/src/a", "hello @NONCE@\n");
    let d = s.dir().join("case").to_str().unwrap().to_string();
    w(
        "default.nix",
        &r#"derivation {
  name = "classic-e2e";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "while read -r l; do echo \"$l\"; done < ${@DIR@/src}/a > $out" ];
}
"#
        .replace("@DIR@", &d),
    );
    w(
        "multi.nix",
        r#"derivation {
  name = "classic-e2e-multi";
  system = "@SYSTEM@";
  outputs = [ "out" "dev" ];
  builder = "@SHELL@";
  args = [ "-c" "echo a > $out; echo b > $dev" ];
}
"#,
    );
    // a `=drv` context closure over a toFile text that names another toFile
    w(
        "textrefs.nix",
        r#"let
  a = builtins.toFile "classic-e2e-tf-a" "x";
  b = builtins.toFile "classic-e2e-tf-b" "ref ${a}";
  c = builtins.toFile "classic-e2e-tf-c" "ref ${b}";
  dep = name: src: derivation { inherit name src; system = "@SYSTEM@"; builder = "@SHELL@"; };
in [
  (derivation { name = "classic-e2e-tf-1"; system = "@SYSTEM@"; builder = "@SHELL@"; x = (dep "classic-e2e-tf-in1" b).drvPath; })
  (derivation { name = "classic-e2e-tf-2"; system = "@SYSTEM@"; builder = "@SHELL@"; x = (dep "classic-e2e-tf-in2" c).drvPath; })
]
"#,
    );
    // nlohmann writes a structuredAttrs float in its own shortest form
    w(
        "floats.nix",
        r#"derivation {
  name = "classic-e2e-floats";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  __structuredAttrs = true;
  a = 0.00001;
  b = 1.0e15;
  c = 1.234e-05;
  d = 1.0e-7;
  e = 2.5;
  f = 1.0;
  g = [ 0.0001 { h = 1.0e20; } ];
}
"#,
    );
    // outputOf over a text-addressed producer, once and nested: the
    // consumer's inputDrvs are a tree and its ATerm is the versioned form
    w(
        "dyndrv.nix",
        r#"let
  producer = name: derivation {
    inherit name;
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "echo done > $out" ];
    __contentAddressed = true;
    outputHashMode = "text";
    outputHashAlgo = "sha256";
  };
  one = builtins.outputOf (producer "classic-e2e-p.drv").outPath "out";
  two = builtins.outputOf (builtins.outputOf (producer "classic-e2e-q.drv.drv").outPath "out") "out";
in derivation {
  name = "classic-e2e-dyn";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo ${one} ${two} > $out" ];
}
"#,
    );
    // two consumers that differ only in the dynamic output they take: the
    // placeholder in the env keeps their modulo hashes apart
    w(
        "dyndeferred.nix",
        r#"let
  producer = derivation {
    name = "classic-e2e-h.drv";
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "echo done > $out" ];
    __contentAddressed = true;
    outputHashMode = "text";
    outputHashAlgo = "sha256";
  };
  cons = out: derivation {
    name = "classic-e2e-h-cons";
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "true" ];
    dep = builtins.outputOf producer.outPath out;
  };
  a = cons "outA";
  b = cons "outB";
in { aDrv = a.drvPath; bDrv = b.drvPath; aOut = a.outPath; bOut = b.outPath; }
"#,
    );
    d
}

/// A locked flake with no inputs, as `path:`.
fn flake(s: &Store, dir: &str) -> String {
    s.write(
        &format!("{dir}/flake.nix"),
        r#"{
  outputs = { self }: {
    x = "hello";
    n = 1.5;
    l = [ 1 2 { a = null; } ];
    s = { a = 1; b = "two"; nested = { c = [ ]; }; };
    packages.@SYSTEM@.pkg = derivation {
      name = "classic-e2e-flake";
      system = "@SYSTEM@";
      builder = "@SHELL@";
      args = [ "-c" "echo flake > $out" ];
    };
    packages.@SYSTEM@.default = self.packages.@SYSTEM@.pkg;
  };
}
"#,
    );
    s.write(
        &format!("{dir}/flake.lock"),
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    );
    format!("path:{}", s.dir().join(dir).display())
}

#[test]
fn the_builtins_names() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    // the builtins Nix gates on a feature are gated in classic mode too
    for mode in ["--impure", "--pure-eval"] {
        for feats in [
            "",
            "dynamic-derivations",
            "dynamic-derivations fetch-closure fetch-tree",
        ] {
            let fs = format!("nix-command flakes {feats}");
            let b = f.both(
                "nix-instantiate",
                &[
                    "--option",
                    "experimental-features",
                    &fs,
                    "--eval",
                    "--json",
                    "-E",
                    "builtins.attrNames builtins",
                    mode,
                ],
            );
            let names = |o: &Output| -> Vec<String> {
                serde_json::from_slice(&o.stdout).unwrap_or_default()
            };
            let (ours, real) = (names(&b.ours), names(&b.real));
            let missing: Vec<_> = real.iter().filter(|n| !ours.contains(n)).collect();
            let extra: Vec<_> = ours.iter().filter(|n| !real.contains(n)).collect();
            c.check(
                &format!("builtins {mode} [{feats}]"),
                !real.is_empty() && missing.is_empty() && extra.is_empty(),
                format!("missing {missing:?}, extra {extra:?}"),
            );
        }
    }
}

#[test]
fn nix_instantiate() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let d = drvs(&s);
    if let Ok(np) = env::var("IETS_TEST_NIXPKGS") {
        let i = format!("nixpkgs={np}");
        for (label, attrs) in [
            ("hello", &["-A", "hello"][..]),
            ("glibc, multi-output", &["-A", "glibc"]),
            ("glibc.dev, an output suffix", &["-A", "glibc.dev"]),
            ("two -A in one run", &["-A", "hello", "-A", "glibc.dev"]),
            // one getDerivations per attribute path: printed twice
            (
                "two -A that name one derivation",
                &["-A", "hello", "-A", "hello"],
            ),
        ] {
            let mut a = vec!["-I", &i, "<nixpkgs>"];
            a.extend(attrs);
            c.same(
                &format!("instantiate <nixpkgs> {label}"),
                &f.both("nix-instantiate", &a),
            );
        }
        c.same(
            "instantiate --add-root ''",
            &f.both(
                "nix-instantiate",
                &["--add-root", "", "-I", &i, "<nixpkgs>", "-A", "hello"],
            ),
        );
    }
    s.write("raw/p.txt", "raw-path\n");
    let rawp = s.dir().join("raw/p.txt");
    let rawp = rawp.to_str().unwrap();
    let outpath = format!("({{ outPath = {rawp}; }}).outPath");
    let evals: &[(&str, &[&str])] = &[
        (
            "--eval --strict --json",
            &["--eval", "--strict", "--json", "-E", "{ a = 1; }"],
        ),
        ("--eval", &["--eval", "-E", "{ a = 1; b = [ 1 2 ]; }"]),
        ("--eval --raw", &["--eval", "--raw", "-E", "\"x\" + \"y\""]),
        // a path value is copied to the store first
        ("--eval --raw a path", &["--eval", "--raw", "-E", rawp]),
        (
            "--eval --raw an outPath",
            &["--eval", "--raw", "-E", &outpath],
        ),
        // autoArgs is a map: the last value of a name wins
        (
            "one --arg name twice",
            &[
                "--eval", "--arg", "a", "1", "--arg", "a", "2", "-E", "{ a }: a",
            ],
        ),
        (
            "--arg then --argstr of one name",
            &[
                "--eval",
                "--arg",
                "a",
                "1",
                "--arg",
                "b",
                "9",
                "--argstr",
                "a",
                "x",
                "-E",
                "{ a, b }: [ a b ]",
            ],
        ),
        // each value is written as it is made
        (
            "a later expression throws",
            &["--eval", "-E", "1", "throw \"x\""],
        ),
        (
            "--argstr",
            &[
                "--eval",
                "--strict",
                "--json",
                "--argstr",
                "who",
                "world",
                "-E",
                "{ who }: { inherit who; }",
            ],
        ),
    ];
    for (label, a) in evals {
        c.same(
            &format!("instantiate {label}"),
            &f.both("nix-instantiate", a),
        );
    }
    let dyn_feats = [
        "--extra-experimental-features",
        "ca-derivations dynamic-derivations",
    ];
    let file = |n: &str| format!("{d}/{n}");
    let dyn_eval = format!("let f = import {d}/dyndrv.nix; in f.drvPath");
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("a custom drv", vec![]),
        ("a toFile's references", vec![]),
        ("builtins.outputOf, once and nested", dyn_feats.to_vec()),
        (
            "outputOf placeholders",
            [
                &dyn_feats[..],
                &[
                    "--read-write-mode",
                    "--eval",
                    "--strict",
                    "--json",
                    "-E",
                    &dyn_eval,
                ],
            ]
            .concat(),
        ),
        (
            "two consumers of one dynamic producer",
            [
                &dyn_feats[..],
                &["--read-write-mode", "--eval", "--strict", "--json"],
            ]
            .concat(),
        ),
        ("--readonly-mode", vec!["--readonly-mode"]),
    ];
    let files = [
        Some(file("default.nix")),
        Some(file("textrefs.nix")),
        Some(file("dyndrv.nix")),
        None,
        Some(file("dyndeferred.nix")),
        Some(file("default.nix")),
    ];
    for ((label, mut a), fl) in cases.into_iter().zip(&files) {
        if let Some(fl) = fl {
            a.push(fl);
        }
        c.same(
            &format!("instantiate {label}"),
            &f.both("nix-instantiate", &a),
        );
    }
    // a name that is not UTF-8 reaches the evaluator and the fallback intact
    let ff = s.dir().join(OsStr::from_bytes(b"\xff.nix"));
    fs::write(&ff, "{ a = 1; }\n").unwrap();
    let real = f
        .real("nix-instantiate")
        .arg("--eval")
        .arg(&ff)
        .output()
        .unwrap();
    let ours = f
        .ours("nix-instantiate")
        .arg("--eval")
        .arg(&ff)
        .output()
        .unwrap();
    let own = s.iets().arg("eval").arg(&ff).output().unwrap();
    c.same(
        "--eval a non-UTF-8 file name",
        &e2e::classic::Both {
            ours,
            real: real.clone(),
        },
    );
    c.same(
        "iets eval a non-UTF-8 file name",
        &e2e::classic::Both { ours: own, real },
    );
    let parse = |mut c: Command| c.arg("--parse").arg(&ff).output().unwrap();
    c.same(
        "a non-UTF-8 file name reaches the fallback",
        &e2e::classic::Both {
            ours: parse(f.ours("nix-instantiate")),
            real: parse(f.real("nix-instantiate")),
        },
    );
}

#[test]
fn nix_eval_of_a_flake() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let r = flake(&s, "flake");
    for (label, a) in [
        ("--json .#x", vec!["--json", "#x"]),
        ("--raw .#x", vec!["--raw", "#x"]),
        (".#s", vec!["#s"]),
        (".#l", vec!["#l"]),
        (".#pkg", vec!["#pkg"]),
        (". (packages.<system>.default)", vec![""]),
    ] {
        let a: Vec<String> = a
            .iter()
            .map(|x| {
                if x.starts_with('#') || x.is_empty() {
                    format!("{r}{x}")
                } else {
                    x.to_string()
                }
            })
            .collect();
        let a: Vec<&str> = [
            &["eval"][..],
            &a.iter().map(String::as_str).collect::<Vec<_>>(),
        ]
        .concat();
        c.same(&format!("nix eval {label}"), &f.both("nix", &a));
    }
    let pf = f.both("nix", &["flake", "prefetch-inputs", &r]);
    c.same("nix flake prefetch-inputs, no inputs", &pf);
    c.check(
        "prefetch-inputs is iets's own",
        !stderr(&pf.ours).contains("falling back"),
        stderr(&pf.ours),
    );

    // without flake.lock nix locks in memory and writes nothing
    let nl = flake(&s, "nolock");
    fs::remove_file(s.dir().join("nolock/flake.lock")).unwrap();
    let pkg = format!("{nl}#packages.{}.pkg.drvPath", e2e::system());
    for (label, a) in [
        (".#x", vec!["eval", "--json"]),
        (".#pkg.drvPath", vec!["eval", "--raw"]),
        (
            "build .#pkg",
            vec!["build", "--no-link", "--print-out-paths"],
        ),
    ] {
        let target = match label {
            ".#x" => format!("{nl}#x"),
            ".#pkg.drvPath" => pkg.clone(),
            _ => format!("{nl}#pkg"),
        };
        let mut a = a;
        a.push(&target);
        let b = f.both("nix", &a);
        c.same(&format!("nix {label}, no flake.lock"), &b);
        c.check(
            &format!("{label}, no flake.lock, is iets's own"),
            !stderr(&b.ours).contains("falling back"),
            stderr(&b.ours),
        );
    }
    c.check(
        "no flake.lock: neither side writes one",
        !s.dir().join("nolock/flake.lock").exists(),
        "",
    );

    // the farm first on PATH: copying a fresh tree spawns the real nix
    let fresh = flake(&s, "fresh");
    // other bytes than `flake`, which is in the store already
    let nix = s.dir().join("fresh/flake.nix");
    fs::write(&nix, fs::read_to_string(&nix).unwrap() + "# fresh\n").unwrap();
    let x = format!("{fresh}#x");
    let path = format!("{}:{}", f.bin.display(), std::env::var("PATH").unwrap());
    c.same(
        "nix eval, farm first on PATH, fresh tree",
        &f.both_with("nix", &["eval", "--json", &x], |c| {
            c.env("PATH", &path);
        }),
    );
    c.same(
        "nix eval --expr",
        &f.both(
            "nix",
            &[
                "eval",
                "--expr",
                "{ a = 1; b = [ 1 2 ]; c = \"x\"; d = 123456789.5; }",
            ],
        ),
    );
    for (label, e) in [
        ("a nested error", "{ a = throw \"boom\"; b = 1; }"),
        (
            "repeated and empty values",
            "let a = { x = 1; }; in [ a a {} {} [] ]",
        ),
        (
            "functions",
            "{ f = x: x; p = builtins.add; q = builtins.add 1; }",
        ),
    ] {
        c.same(
            &format!("nix eval --expr, {label}"),
            &f.both("nix", &["eval", "--expr", e]),
        );
    }
    let d = drvs(&s);
    let multi = format!("{d}/multi.nix");
    c.same(
        "nix eval -f FILE ATTR",
        &f.both("nix", &["eval", "-f", &multi, "outputs"]),
    );
    c.same(
        "nix eval --json -f FILE ATTR",
        &f.both("nix", &["eval", "--json", "-f", &multi, "outputs"]),
    );
}

/// `nix` is pure unless told otherwise, `--file` is impure, and the legacy
/// names are impure.
#[test]
fn pure_eval_and_nix_conf() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let r = flake(&s, "flake");
    let d = drvs(&s);
    let env_expr = r#"builtins.getEnv "HOME""#;
    let envf = s.write("env.nix", env_expr);
    let envf = envf.to_str().unwrap();
    let local = s.write("local.txt", "local\n");
    let read_local = format!("builtins.readFile {}", local.display());
    let probe = r#"[ (builtins ? currentSystem) (builtins.getEnv "HOME") builtins.nixPath ]"#;
    let x = format!("{r}#x");
    for (label, name, a) in [
        (
            "nix eval --expr is pure",
            "nix",
            vec!["eval", "--expr", env_expr],
        ),
        (
            "nix eval --impure --expr",
            "nix",
            vec!["eval", "--impure", "--expr", env_expr],
        ),
        (
            "nix --option pure-eval false",
            "nix",
            vec!["--option", "pure-eval", "false", "eval", "--expr", env_expr],
        ),
        ("nix eval -f is impure", "nix", vec!["eval", "-f", envf]),
        ("nix eval .#x is pure", "nix", vec!["eval", &x]),
        (
            "nix-instantiate is impure",
            "nix-instantiate",
            vec!["--eval", "-E", env_expr],
        ),
        (
            "nix-instantiate --pure-eval",
            "nix-instantiate",
            vec!["--pure-eval", "--eval", "--strict", "-E", probe],
        ),
        (
            "nix-instantiate --option pure-eval true",
            "nix-instantiate",
            vec![
                "--option",
                "pure-eval",
                "true",
                "--eval",
                "--strict",
                "-E",
                probe,
            ],
        ),
        (
            "--pure-eval refuses a local file",
            "nix-instantiate",
            vec!["--pure-eval", "--eval", "-E", &read_local],
        ),
    ] {
        c.same(label, &f.both(name, &a));
    }
    let cur = ["--eval", "-E", "builtins.currentSystem"];
    c.same(
        "nix.conf system reaches builtins.currentSystem",
        &f.both_with("nix-instantiate", &cur, |c| {
            c.env("NIX_CONFIG", "system = aarch64-linux");
        }),
    );
    // NIX_USER_CONF_FILES set and empty means no user file
    let cdir = s.dir().join("confdir");
    s.write("confdir/nix.conf", "system = from-system\n");
    s.write("xdg/nix/nix.conf", "system = from-user\n");
    for (label, v) in [("empty", Some("")), ("unset", None)] {
        c.same(
            &format!("NIX_USER_CONF_FILES {label}"),
            &f.both_with("nix-instantiate", &cur, |c| {
                c.env("NIX_CONF_DIR", &cdir);
                match v {
                    Some(v) => c.env("NIX_USER_CONF_FILES", v),
                    None => c.env_remove("NIX_USER_CONF_FILES"),
                };
            }),
        );
    }
    fs::remove_file(s.dir().join("xdg/nix/nix.conf")).unwrap();
    // installables.cc refuses `nix eval -f` when pure-eval was overridden,
    // but not from the system nix.conf
    s.write(
        "confdir/nix.conf",
        "experimental-features = nix-command flakes\npure-eval = true\n",
    );
    let user = s.write("confdir/user.conf", "pure-eval = true\n");
    let multi = format!("{d}/multi.nix");
    type Env<'a> = Vec<(&'a str, &'a OsStr)>;
    let cases: Vec<(&str, Env, Vec<&str>)> = vec![
        (
            "the system nix.conf",
            vec![("NIX_CONF_DIR", cdir.as_os_str())],
            vec![],
        ),
        (
            "a user nix.conf",
            vec![("NIX_USER_CONF_FILES", user.as_os_str())],
            vec![],
        ),
        (
            "NIX_CONFIG",
            vec![("NIX_CONFIG", "pure-eval = true".as_ref())],
            vec![],
        ),
        ("--option", vec![], vec!["--option", "pure-eval", "true"]),
        ("--impure", vec![], vec!["--impure"]),
    ];
    for (label, env, flags) in cases {
        let a = [&["eval"][..], &flags, &["-f", &multi, "outputs"]].concat();
        c.same(
            &format!("nix eval -f with pure-eval from {label}"),
            &f.both_with("nix", &a, |c| {
                c.envs(env.iter().copied());
            }),
        );
    }
    // -j and --cores are settings; the clustered -IP=PATH is iets's own
    let flags: [&[&str]; 6] = [
        &["-j", "4294967296"],
        &["--cores", "4294967296"],
        &["-j", "auto"],
        &["--cores", "1K"],
        &["-j", "2"],
        &["-Ifoo=/etc"],
    ];
    for a in flags {
        let all = [a, &["--eval", "-E", "1"]].concat();
        c.same(
            &format!("instantiate {}", a.join(" ")),
            &f.both("nix-instantiate", &all),
        );
    }
    // a value Nix refuses stops the run with Nix's own text
    for v in [
        "pure-eval = maybe",
        "show-trace = maybe",
        "sandbox = maybe",
        "eval-cache = 2",
        "connect-timeout = abc",
        "cores = -1",
        "cores = 4294967296",
        "max-jobs = abc",
        "cores = 1K",
        "min-free = 2G",
        "max-jobs = auto",
        "sandbox = relaxed",
    ] {
        let b = f.both_with("nix-instantiate", &["--eval", "-E", "1"], |c| {
            c.env("NIX_CONFIG", v);
        });
        let (eo, er) = (first_error(&stderr(&b.ours)), first_error(&stderr(&b.real)));
        c.check(
            &format!("nix.conf '{v}'"),
            b.ours.status.code() == b.real.status.code()
                && b.ours.stdout == b.real.stdout
                && eo == er,
            format!("ours '{eo}', real '{er}'"),
        );
    }
    // the nix-path setting, which NIX_PATH replaces, even an empty one
    s.write("nixpath/x.nix", "\"from nix-path\"\n");
    let np = format!("nix-path = np={}", s.dir().join("nixpath").display());
    for (label, empty) in [
        ("the nix-path setting", false),
        ("an empty NIX_PATH replaces nix-path", true),
    ] {
        c.same(
            label,
            &f.both_with(
                "nix-instantiate",
                &["--eval", "-E", "import <np/x.nix>"],
                |c| {
                    c.env("NIX_CONFIG", &np);
                    if empty {
                        c.env("NIX_PATH", "");
                    }
                },
            ),
        );
    }
}

#[test]
fn eval_jobs_summary() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    // nix-eval-jobs says nothing on stderr, iets eval-jobs keeps its summary
    let o = f
        .ours("nix-eval-jobs")
        .args(["--no-instantiate", "-E", "{ }"])
        .output()
        .unwrap();
    c.check(
        "nix-eval-jobs prints no summary",
        stderr(&o).is_empty(),
        stderr(&o),
    );
    let o = s
        .iets()
        .args(["eval-jobs", "--no-instantiate", "-E", "{ }"])
        .output()
        .unwrap();
    c.check(
        "iets eval-jobs keeps its summary",
        stderr(&o).contains("eval-jobs: "),
        stderr(&o),
    );
}

/// Nix reads a path below a store path out of the store copy. iets reads the
/// local tree, so it resolves links on the text of the path.
#[test]
fn symlinks_below_a_copied_path() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    s.write("sl/a.txt", "hello\n");
    s.write("sl/sub/inner", "inner\n");
    let sl = s.dir().join("sl");
    symlink("a.txt", sl.join("link-file")).unwrap();
    symlink("sub", sl.join("link-dir")).unwrap();
    symlink("nope", sl.join("dangling")).unwrap();
    let withref = stdout(
        &f.real("nix")
            .args([
                "eval",
                "--impure",
                "--raw",
                "--expr",
                r#"builtins.toFile "b" "ref ${builtins.toFile "a" "aaa"}""#,
            ])
            .output()
            .unwrap(),
    );
    let store_re = format!("{}/.*", s.store_dir().display());
    for (label, e) in [
        (
            "readFile through a copy of the link itself",
            r#"builtins.readFile "${@SL@/link-file}""#,
        ),
        (
            "readFile of a target the filter left out",
            r#"builtins.readFile "${builtins.filterSource (p: _: baseNameOf p != "a.txt") @SL@}/link-file""#,
        ),
        (
            "readDir through a copy of the link itself",
            r#"builtins.readDir "${@SL@/link-dir}""#,
        ),
        (
            "hashFile through a copy of the link itself",
            r#"builtins.hashFile "sha256" "${@SL@/link-file}""#,
        ),
        (
            "readFile below a link in a copied tree",
            r#"builtins.readFile "${@SL@}/link-dir/inner""#,
        ),
        (
            "readDir of a link in a copied tree",
            r#"builtins.readDir "${@SL@}/link-dir""#,
        ),
        (
            "pathExists below a link in a copied tree",
            r#"builtins.pathExists "${@SL@}/link-dir/inner""#,
        ),
        (
            "path of a link in a copied tree",
            r#"builtins.path { path = "${@SL@}/link-dir"; }"#,
        ),
        (
            "path of a link, local and copied agree",
            r#"builtins.path { path = @SL@/link-dir; }"#,
        ),
        (
            "path of a dangling link",
            r#"builtins.path { path = @SL@/dangling; }"#,
        ),
        (
            "path of a dangling link in a copied tree",
            r#"builtins.path { path = "${@SL@}/dangling"; }"#,
        ),
        (
            "storePath below a link in a copied tree",
            r#"builtins.storePath "${@SL@}/link-dir/inner""#,
        ),
        (
            "filterSource sees the store path of an unfiltered copy",
            r#"builtins.filterSource (p: _: builtins.match "@STORE@" p != null) "${@SL@/sub}""#,
        ),
        (
            "path keeps the references of a store path",
            r#"builtins.path { path = "${builtins.storePath @REF@}"; }"#,
        ),
        (
            "path flat refuses a store path with references",
            r#"builtins.path { path = "${builtins.storePath @REF@}"; recursive = false; }"#,
        ),
    ] {
        let e = e
            .replace("@SL@", sl.to_str().unwrap())
            .replace("@REF@", &withref)
            .replace("@STORE@", &store_re);
        c.same(
            &format!("symlink: {label}"),
            &f.both("nix", &["eval", "--impure", "--expr", &e]),
        );
    }
}

/// A file whose top level is a function: the attribute path auto-calls it
/// at every step, in the native names too.
#[test]
fn auto_call_a_function_file() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let fun = s.write(
        "fun.nix",
        r#"{ a ? 1, b ? 2 }: { sum = a + b; name = "n-${toString a}"; }"#,
    );
    let ell = s.write(
        "ellipsis.nix",
        "{ a ? 1, ... }@args: { sum = a; name = builtins.toJSON (builtins.attrNames args); }",
    );
    let (fun, ell) = (fun.to_str().unwrap(), ell.to_str().unwrap());
    let both = |ours: Output, real: Output| e2e::classic::Both { ours, real };
    for (label, args, attr) in [
        ("defaults", &[][..], "sum"),
        ("--arg", &["--arg", "a", "5"], "sum"),
        ("--argstr", &["--argstr", "a", "x"], "name"),
    ] {
        for (file, extra, what) in [
            (fun, &[][..], "a function"),
            (ell, &["--arg", "extra", "9"][..], "a '...' function"),
        ] {
            let a = [args, extra, &[file, "-A", attr]].concat();
            c.same(
                &format!("auto-call {what} file, {label}: nix-instantiate"),
                &f.both("nix-instantiate", &[&["--eval"][..], &a].concat()),
            );
            let ours = s.iets().arg("eval").args(&a).output().unwrap();
            let real = f
                .real("nix")
                .arg("eval")
                .args(args)
                .args(extra)
                .args(["-f", file, attr])
                .output()
                .unwrap();
            c.same(
                &format!("auto-call {what} file, {label}: iets eval"),
                &both(ours, real),
            );
        }
    }
    // a required formal nobody supplied: the same first error line
    let na = s.write("needs-arg.nix", "{ a, b ? 2 }: { sum = a + b; }");
    let na = na.to_str().unwrap();
    let outs = [
        f.real("nix")
            .args(["eval", "-f", na, "sum"])
            .output()
            .unwrap(),
        s.iets().args(["eval", na, "-A", "sum"]).output().unwrap(),
        f.ours("nix-instantiate")
            .args(["--eval", na, "-A", "sum"])
            .output()
            .unwrap(),
    ];
    let lines: Vec<String> = outs.iter().map(|o| first_error(&stderr(o))).collect();
    c.check(
        "a missing required --arg: the same first error everywhere",
        outs.iter().all(|o| o.status.code() == Some(1))
            && !lines[0].is_empty()
            && lines.iter().all(|l| *l == lines[0]),
        format!("{lines:?}"),
    );
}

const PKGS: &str = r#"let
  mk = { pname, version ? "", description ? null }:
    derivation {
      name = if version == "" then pname else "${pname}-${version}";
      system = builtins.currentSystem;
      builder = "@SHELL@";
      args = [ "-c" "echo hi > $out" ];
    } // (if description == null then { } else { meta = { inherit description; }; });
in
{
  legacyPackages.${builtins.currentSystem} = {
    hello = mk { pname = "hello"; version = "2.12"; description = "A program that produces a familiar, friendly greeting"; };
    goodbye = mk { pname = "goodbye"; version = "1.0"; description = "Says goodbye\nacross two lines"; };
    plain = mk { pname = "plain"; };
    broken = throw "this attribute is broken";
    brokenSet = { recurseForDerivations = true; inner = throw "deep throw"; ok = mk { pname = "survivor"; version = "1"; description = "Found after a throw"; }; };
    notASet = 42;
    badType = { type = "derivation"; name = 7; };
    nested = {
      recurseForDerivations = true;
      tool = mk { pname = "nested-tool"; version = "0.1"; description = "A nested tool"; };
      deeper = { recurseForDerivations = true; deepest = mk { pname = "deepest"; version = "3"; description = "Deep package"; }; };
      unmarked = { hidden = mk { pname = "hidden"; version = "9"; description = "Not reachable"; }; };
    };
    metaNotASet = mk { pname = "metaless"; version = "2"; } // { meta = 5; };
    quotes = mk { pname = "quotes"; version = "1"; description = "Uses \"quotes\", a tab\tand a backslash \\"; };
    unicode = mk { pname = "unicode"; version = "1"; description = "Ünïcödé — em dash and ▲"; };
  };
  packages.${builtins.currentSystem}.direct = mk { pname = "direct"; version = "4"; description = "Under packages"; };
}
"#;

/// Nested recurseForDerivations sets, attributes that throw, a bad `type`
/// and a description with a newline.
#[test]
fn nix_search() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let pkgs = s.write("pkgs-search.nix", PKGS);
    let pkgs = pkgs.to_str().unwrap();
    for (label, a) in [
        ("every package", &["^"][..]),
        ("--json", &["^", "--json"]),
        ("--json --pretty", &["^", "--json", "--pretty"]),
        ("two regexes are ANDed", &["deep", "est"]),
        ("--exclude", &["e", "-e", "good"]),
        ("is case insensitive", &["HELLO"]),
        ("a bracket expression", &["[Ff]riendly"]),
        ("an alternation", &["hell|good"]),
        ("no results", &["zzz"]),
        ("--json, no results", &["zzz", "--json"]),
    ] {
        let all = [&["search", "-f", pkgs, ""][..], a].concat();
        c.same(&format!("nix search {label}"), &f.both("nix", &all));
    }
}

/// The walk over nixpkgs that costs, under legacyPackages so both sides skip
/// the attributes that throw.
#[test]
fn nix_search_nixpkgs() {
    let Ok(np) = env::var("IETS_TEST_NIXPKGS") else {
        eprintln!("skipped: IETS_TEST_NIXPKGS unset");
        return;
    };
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let e = format!("{{ legacyPackages.${{builtins.currentSystem}} = import {np} {{}}; }}");
    c.same(
        "nix search nixpkgs --json ^hello$",
        &f.both(
            "nix",
            &["search", "--impure", "--expr", &e, "", "^hello$", "--json"],
        ),
    );
}

#[test]
fn internal_json_errors() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let undef = s.write("undef.nix", "let\n  x = 1;\nin\n  y + x\n");
    let j = ["--log-format", "internal-json"];
    let undef = undef.to_str().unwrap();
    for (label, code, name, a) in [
        (
            "undefined variable in a file",
            "undefined-var",
            "nix-instantiate",
            vec!["--eval", undef],
        ),
        (
            "missing attribute",
            "missing-attr",
            "nix-instantiate",
            vec!["--eval", "-E", "{ a = 1; }.b"],
        ),
        (
            "syntax error",
            "parse",
            "nix-instantiate",
            vec!["--eval", "-E", "1 +"],
        ),
        (
            "nix eval, missing attribute",
            "missing-attr",
            "nix",
            vec!["eval", "--expr", "{ a = 1; }.b"],
        ),
    ] {
        let a = if name == "nix" {
            [&a[..1], &j, &a[1..]].concat()
        } else {
            [&j[..], &a].concat()
        };
        c.same_record(&format!("internal-json: {label}"), code, &f.both(name, &a));
    }
    // json-log-path: stderr stays text, the file gets the records unprefixed
    let run = |mut cmd: Command, log: &str| {
        let p = s.dir().join(log);
        let o = cmd
            .args([
                "eval",
                "--option",
                "json-log-path",
                p.to_str().unwrap(),
                "--expr",
                "{ a = 1; }.b",
            ])
            .output()
            .unwrap();
        let text: String = fs::read_to_string(&p)
            .unwrap_or_default()
            .lines()
            .map(|l| format!("@nix {l}\n"))
            .collect();
        (o, text)
    };
    let (jo, jot) = run(f.ours("nix"), "jlog-ours");
    let (jr, jrt) = run(f.real("nix"), "jlog-real");
    let (a, b) = (err_record(&jrt).0, err_record(&jot));
    c.check(
        "json-log-path holds the error record",
        a == b.0 && b.1 == "missing-attr" && jo.status.code() == jr.status.code(),
        format!("real {a}, ours {}", b.0),
    );
    c.check(
        "json-log-path: stderr stays text",
        stderr(&jo).starts_with("error: attribute 'b' missing"),
        stderr(&jo),
    );
}

/// A flag or a setting with no effect: one warning that gives the reason,
/// where Nix prints nothing. stdout and the exit code stay Nix's.
#[test]
fn ignored_flags_and_settings() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let once = |e: &str, t: &str| e.lines().filter(|l| l.contains(t)).count() == 1;
    let coded = |e: &str, code: &str| -> Vec<Value> {
        records(e)
            .into_iter()
            .filter(|r| r["code"] == code)
            .collect()
    };
    let vb = f.both(
        "nix-instantiate",
        &["--eval", "-v", "--verbose", "-E", "1 + 1"],
    );
    c.same("nix-instantiate -v --verbose", &vb);
    c.check(
        "-v is one warning with the reason",
        once(
            &stderr(&vb.ours),
            "warning: '--verbose' is ignored: iets has one log level",
        ),
        stderr(&vb.ours),
    );
    s.write(
        "nc/flake.nix",
        "{\n  nixConfig.extra-substituters = [ \"https://cache.invalid\" ];\n  outputs = { self }: { x = 1; };\n}\n",
    );
    s.write(
        "nc/flake.lock",
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    );
    let x = format!("path:{}#x", s.dir().join("nc").display());
    let ac = f.both(
        "nix",
        &[
            "eval",
            "--accept-flake-config",
            "--eval-store",
            "auto",
            "--eval-store",
            "auto",
            &x,
        ],
    );
    c.same(
        "--accept-flake-config --eval-store auto, a flake with a nixConfig",
        &ac,
    );
    let e = stderr(&ac.ours);
    c.check(
        "--accept-flake-config and --eval-store are one warning each",
        once(
            &e,
            "'--accept-flake-config' is ignored: iets never applies a flake's nixConfig",
        ) && once(&e, "'--eval-store' is ignored: iets evaluates in memory")
            && !e.contains("falling back"),
        &e,
    );
    let aj = f
        .ours("nix")
        .args([
            "eval",
            "--log-format",
            "internal-json",
            "--accept-flake-config",
            "--expr",
            "1",
        ])
        .output()
        .unwrap();
    let r = coded(&stderr(&aj), "ignored-flag");
    c.check(
        "an ignored flag is a level-1 ignored-flag record",
        stdout(&aj) == "1"
            && r.len() == 1
            && r[0]["level"] == 1
            && r[0]["raw_msg"].as_str().unwrap_or("").contains("nixConfig"),
        stderr(&aj),
    );
    let jlf = s.dir().join("ignored.jsonl");
    f.ours("nix")
        .args([
            "eval",
            "--option",
            "json-log-path",
            jlf.to_str().unwrap(),
            "--eval-store",
            "auto",
            "--expr",
            "1",
        ])
        .output()
        .unwrap();
    let jl = fs::read_to_string(&jlf).unwrap_or_default();
    let n = jl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["code"] == "ignored-flag")
        .count();
    c.check("json-log-path gets the ignored-flag record", n == 1, &jl);
    // a nix.conf written for a fork of Nix is a file of unknown settings
    let fconf = s.write("fork.conf", "lazy-trees = true\neval-cores = 0\n");
    let fk = f.both_with("nix-instantiate", &["--eval", "-E", "1 + 1"], |c| {
        c.env("NIX_USER_CONF_FILES", &fconf);
    });
    c.same("a nix.conf with a fork's settings changes nothing", &fk);
    c.check(
        "each is Nix's own unknown-setting warning, once",
        stderr(&fk.ours) == stderr(&fk.real)
            && once(&stderr(&fk.real), "unknown setting 'lazy-trees'"),
        show(&fk),
    );
    let fj = s
        .iets()
        .env("NIX_USER_CONF_FILES", &fconf)
        .args([
            "eval",
            "--log-format",
            "internal-json",
            "--option",
            "eval-cores",
            "4",
            "-E",
            "1",
        ])
        .output()
        .unwrap();
    let r = coded(&stderr(&fj), "unknown-setting");
    let msgs: Vec<&str> = r.iter().filter_map(|r| r["raw_msg"].as_str()).collect();
    c.check(
        "an unknown setting is a level-1 record, once per name",
        stdout(&fj) == "1"
            && msgs
                == [
                    "unknown setting 'eval-cores'",
                    "unknown setting 'lazy-trees'",
                ]
            && r.iter().all(|r| r["level"] == 1),
        stderr(&fj),
    );
    let uk = f.both(
        "nix-instantiate",
        &[
            "--eval",
            "-E",
            "1",
            "--option",
            "no-such-setting",
            "1",
            "--option",
            "extra-no-such",
            "2",
        ],
    );
    c.same("an unknown setting", &uk);
    c.check(
        "an unknown setting gets Nix's own warning",
        stderr(&uk.ours) == stderr(&uk.real)
            && stderr(&uk.real).contains("unknown setting 'no-such-setting'"),
        show(&uk),
    );
    let afc = f.both_with("nix", &["eval", "--expr", "1"], |c| {
        c.env(
            "NIX_CONFIG",
            "accept-flake-config = true\nlog-lines = 7\nwarn-dirty = false\nmax-call-depth = 10000",
        );
    });
    c.same("accept-flake-config = true in NIX_CONFIG", &afc);
    let e = stderr(&afc.ours);
    c.check(
        "a client setting iets has nothing for is one warning, a default none",
        once(&e, "setting 'accept-flake-config' is ignored:")
            && once(&e, "setting 'log-lines' is ignored:")
            && e.lines().count() == 2,
        &e,
    );
    let local = s.write("local.txt", "local\n");
    let read = format!("builtins.readFile {}", local.display());
    let re = ["--eval", "-E", &read, "--option", "restrict-eval", "true"];
    c.same(
        "restrict-eval is left to the real nix",
        &f.both("nix-instantiate", &re),
    );
    let why = f.reason("nix-instantiate", &re);
    c.check(
        "restrict-eval falls back",
        why.contains("the setting 'restrict-eval' is not implemented"),
        &why,
    );
    let ns = s
        .iets()
        .args([
            "eval",
            "--log-format",
            "internal-json",
            "--option",
            "build-use-sandbox",
            "false",
            "--option",
            "eval-cache",
            "false",
            "-E",
            "1",
        ])
        .output()
        .unwrap();
    let r = coded(&stderr(&ns), "ignored-setting");
    c.check(
        "iets eval --option sandbox: one ignored-setting record",
        stdout(&ns) == "1"
            && r.len() == 1
            && r[0]["raw_msg"].as_str().unwrap_or("").starts_with(
                "setting 'sandbox' is ignored: iets's own commands send a daemon no settings",
            ),
        stderr(&ns),
    );
}

#[test]
fn nix_derivation_show() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let d = drvs(&s);
    let r = flake(&s, "flake");
    let fod = s.write(
        "show-fod.nix",
        r#"derivation {
  name = "classic-e2e-show-fod";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo x > $out" ];
  outputHashMode = "recursive";
  outputHashAlgo = "sha1";
  outputHash = "0000000000000000000000000000000000000000";
}
"#,
    );
    let dep = s.write(
        "show-dep.nix",
        r#"let
  dep = derivation {
    name = "classic-e2e-show-dep";
    system = "@SYSTEM@";
    outputs = [ "out" "dev" ];
    builder = "@SHELL@";
    args = [ "-c" "echo a > $out; echo b > $dev" ];
  };
in {
  top = derivation {
    name = "classic-e2e-show-top";
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "true" ];
    d = "${dep.dev}";
    t = builtins.toFile "note" "hello";
    __structuredAttrs = true;
  };
  inherit dep;
}
"#,
    );
    let (fod, dep) = (fod.to_str().unwrap(), dep.to_str().unwrap());
    let (def, multi, floats) = (
        format!("{d}/default.nix"),
        format!("{d}/multi.nix"),
        format!("{d}/floats.nix"),
    );
    let pkg = format!("{r}#pkg");
    let mut cases: Vec<(&str, Vec<&str>)> = vec![
        ("a custom drv", vec!["-f", &def]),
        ("a multi-output drv", vec!["-f", &multi]),
        ("a fixed-output drv", vec!["-f", fod]),
        ("structured attrs and an input drv", vec!["-f", dep, "top"]),
        ("two installables", vec!["-f", dep, "top", "dep"]),
        ("--pretty", vec!["--pretty", "-f", &multi]),
        ("--no-pretty", vec!["--no-pretty", "-f", &multi]),
        (".#pkg", vec![&pkg]),
        ("-r over a closure", vec!["-r", "-f", dep, "top"]),
        ("structuredAttrs floats", vec!["-f", &floats]),
        (
            "structuredAttrs floats --pretty",
            vec!["--pretty", "-f", &floats],
        ),
    ];
    let np = env::var("IETS_TEST_NIXPKGS").ok();
    let i = np.as_ref().map(|np| format!("nixpkgs={np}"));
    if let (Some(np), Some(i)) = (&np, &i) {
        cases.push((
            "-r over nixpkgs hello.src",
            vec!["-r", "-I", i, "-f", np, "hello.src"],
        ));
    }
    for (label, a) in cases {
        let all = [&["derivation", "show"][..], &a].concat();
        c.same(&format!("derivation show {label}"), &f.both("nix", &all));
    }
    let miss = f.both("nix", &["derivation", "show", "-f", dep, "nope"]);
    c.same("derivation show an installable that does not exist", &miss);
    // only the first line: nix adds a "Did you mean" line
    let first = |o: &Output| stderr(o).lines().next().unwrap_or_default().to_string();
    c.check(
        "the same error line",
        first(&miss.ours) == first(&miss.real),
        show(&miss),
    );
    // iets answers from the evaluation, so the store gains no `.drv`
    let salt = s.write(
        "show-salt.nix",
        r#"derivation { name = "classic-e2e-show-salt"; system = "@SYSTEM@"; builder = "@SHELL@"; args = [ "-c" "true" ]; salt = "@NONCE@"; }"#,
    );
    let so = f
        .ours("nix")
        .args(["derivation", "show", "-f"])
        .arg(&salt)
        .output()
        .unwrap();
    let j: Value = serde_json::from_slice(&so.stdout).unwrap_or(Value::Null);
    let name = j["derivations"]
        .as_object()
        .and_then(|m| m.keys().next().cloned())
        .unwrap_or_default();
    let sp = s.store_dir().join(&name);
    let before = sp.exists();
    let sr = f
        .real("nix")
        .args(["derivation", "show", "-f"])
        .arg(&salt)
        .output()
        .unwrap();
    c.check(
        "derivation show does not instantiate, where the real nix does",
        !name.is_empty() && so.stdout == sr.stdout && !before && sp.exists(),
        format!("{}: before {before}", sp.display()),
    );
    let fake = format!(
        "{}/00000000000000000000000000000000-x.drv",
        s.store_dir().display()
    );
    for a in [&["--stdin"][..], &[&fake]] {
        let why = f.reason("nix", &[&["derivation", "show"][..], a].concat());
        c.check(
            &format!("derivation show {a:?} falls back"),
            why.contains("iets: falling back to nix:"),
            &why,
        );
    }
}
