//! Classic mode commands that build, against the real Nix: out-links, roots, the
//! log records of a build and its failures.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use e2e::classic::{Both, Checks, Farm, link, online, records, show, strip_ansi};
use e2e::{Store, stderr, stdout};
use serde_json::Value;

fn case(s: &Store) -> (String, String) {
    s.write("case/src/a", "hello @NONCE@\n");
    let d = s.dir().join("case");
    let def = s.write(
        "case/default.nix",
        &r#"derivation {
  name = "classic-e2e";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "while read -r l; do echo \"$l\"; done < ${@DIR@/src}/a > $out" ];
}
"#
        .replace("@DIR@", d.to_str().unwrap()),
    );
    let multi = s.write(
        "case/multi.nix",
        r#"derivation {
  name = "classic-e2e-multi-@NONCE@";
  system = "@SYSTEM@";
  outputs = [ "out" "dev" ];
  builder = "@SHELL@";
  args = [ "-c" "echo a > $out; echo b > $dev" ];
}
"#,
    );
    (def.to_str().unwrap().into(), multi.to_str().unwrap().into())
}

/// Whether the daemon names `link` as a root of `path`.
fn rooted(s: &Store, path: &str, link: &Path) -> bool {
    let o = s
        .nix("nix-store")
        .args(["--query", "--roots", path])
        .output()
        .unwrap();
    stdout(&o).contains(link.to_str().unwrap())
}

#[test]
fn out_links_and_roots() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let (def, multi) = case(&s);

    // --add-root: the printed path is the link, and the daemon knows the root
    for (who, mut cmd) in [
        ("real", f.real("nix-instantiate")),
        ("ours", f.ours("nix-instantiate")),
    ] {
        let l = s.dir().join(format!("gcroot-{who}"));
        let o = cmd.arg("--add-root").arg(&l).arg(&def).output().unwrap();
        c.check(
            &format!("--add-root {who} prints the link"),
            stdout(&o) == l.to_str().unwrap(),
            stdout(&o),
        );
        c.check(
            &format!("--add-root {who} is a gc root"),
            rooted(&s, &link(&l), &l),
            link(&l),
        );
    }

    // an out-link replaces only a symlink into the store
    let other = s.write("other-target", "other\n");
    for kind in ["regular", "dir", "other-link", "dangling"] {
        let mut got = vec![];
        for (who, mut cmd) in [("real", f.real("nix-build")), ("ours", f.ours("nix-build"))] {
            let wd = s.dir().join(format!("keep-{kind}-{who}"));
            fs::create_dir(&wd).unwrap();
            let t = wd.join("t");
            match kind {
                "regular" => fs::write(&t, "PRECIOUS\n").unwrap(),
                "dir" => fs::create_dir(&t).unwrap(),
                "other-link" => symlink(&other, &t).unwrap(),
                _ => symlink("/nonexistent-for-e2e", &t).unwrap(),
            }
            let o = cmd
                .current_dir(&wd)
                .args(["-o", "t", &def])
                .output()
                .unwrap();
            got.push((o.status.code(), link(&t), t.is_dir()));
        }
        c.check(
            &format!("-o over a {kind}"),
            got[0] == got[1],
            format!("real {:?}, ours {:?}", got[0], got[1]),
        );
    }

    // nix-build: the default `result`, -o, multi-output naming
    let two_outs = format!("let m = import {multi}; in [ m.dev m.out ]");
    let two_drvs = format!("[ (import {def}) (import {multi}) ]");
    let a_b = format!("{{ a = import {def}; b = import {multi}; }}");
    for (label, args, links) in [
        (
            "a custom drv, default result",
            vec![def.as_str()],
            vec!["result"],
        ),
        ("-o link", vec!["-o", "link", &def], vec!["link"]),
        ("--no-out-link", vec!["--no-out-link", &def], vec![]),
        (
            "two outputs of one drv",
            vec!["-E", &two_outs],
            vec!["result", "result-dev"],
        ),
        (
            "two drvs",
            vec!["-E", &two_drvs],
            vec!["result", "result-2"],
        ),
        (
            "a repeated -A",
            vec!["-E", &a_b, "-A", "a", "-A", "a", "-A", "b"],
            vec!["result", "result-2"],
        ),
    ] {
        let mut got = vec![];
        for (who, mut cmd) in [("real", f.real("nix-build")), ("ours", f.ours("nix-build"))] {
            let wd = s
                .dir()
                .join(format!("build-{who}-{}", label.replace(' ', "_")));
            fs::create_dir(&wd).unwrap();
            let o = cmd.current_dir(&wd).args(&args).output().unwrap();
            let ls: Vec<String> = links.iter().map(|l| link(&wd.join(l))).collect();
            got.push((o, ls));
        }
        let (r, o) = (&got[0], &got[1]);
        c.check(
            &format!("nix-build {label}"),
            o.0.stdout == r.0.stdout && o.0.status.code() == r.0.status.code() && o.1 == r.1,
            format!(
                "links ours {:?} real {:?}\n{}",
                o.1,
                r.1,
                show(&Both {
                    ours: o.0.clone(),
                    real: r.0.clone()
                })
            ),
        );
    }

    // nix build: the out-link prefix, the printed paths and the link target
    s.write(
        "flake/flake.nix",
        r#"{ outputs = { self }: { packages.@SYSTEM@.pkg = derivation { name = "classic-e2e-flake-@NONCE@"; system = "@SYSTEM@"; builder = "@SHELL@"; args = [ "-c" "echo flake > $out" ]; }; }; }"#,
    );
    s.write(
        "flake/flake.lock",
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    );
    let pkg = format!("path:{}#pkg", s.dir().join("flake").display());
    let mut got = vec![];
    for (who, mut cmd) in [("real", f.real("nix")), ("ours", f.ours("nix"))] {
        let wd = s.dir().join(format!("nixbuild-{who}"));
        fs::create_dir(&wd).unwrap();
        let o = cmd
            .current_dir(&wd)
            .args(["build", &pkg, "--print-out-paths", "-o", "result-x"])
            .output()
            .unwrap();
        got.push((o, link(&wd.join("result-x"))));
    }
    c.check(
        "nix build .#pkg --print-out-paths -o result-x",
        got[0].0.stdout == got[1].0.stdout
            && got[0].0.status.code() == got[1].0.status.code()
            && got[0].1 == got[1].1
            && !got[0].1.is_empty(),
        show(&Both {
            ours: got[1].0.clone(),
            real: got[0].0.clone(),
        }),
    );
    // `nix print-dev-env` is `iets print-dev-env`
    let shell = format!("path:{}", s.pkgs().display());
    let a = f
        .ours("nix")
        .args(["print-dev-env", &shell])
        .output()
        .unwrap();
    let b = s.iets().args(["print-dev-env", &shell]).output().unwrap();
    c.check(
        "nix print-dev-env is iets print-dev-env",
        a.stdout == b.stdout && a.status.code() == b.status.code() && a.status.success(),
        stderr(&a),
    );
}

/// The build runs on another daemon, and the outputs come back so `result`
/// names a path the local store has.
#[cfg(target_os = "linux")]
#[test]
fn the_builder_setting() {
    let s = store!();
    let f = Farm::new(&s);
    let b = s.builder("b", &[]);
    let (def, _) = case(&s);
    let wd = s.dir().join("bb");
    fs::create_dir(&wd).unwrap();
    let o = f
        .ours("nix-build")
        .env("IETS_CONFIG", format!("builder = \"{}\"", b.url()))
        .current_dir(&wd)
        .arg(&def)
        .output()
        .unwrap();
    let out = stdout(&o);
    let valid = s
        .nix("nix-store")
        .args(["--check-validity", &out])
        .status()
        .unwrap()
        .success();
    let res = wd.join("result");
    assert!(
        o.status.success()
            && link(&res) == out
            && b.real(&out).exists()
            && valid
            && rooted(&s, &out, &res),
        "{}",
        stderr(&o)
    );
}

fn drv(name: &str, extra: &str) -> String {
    format!(
        r#"derivation {{ name = "{name}"; system = "@SYSTEM@"; builder = "@SHELL@"; salt = "@NONCE@"; {extra} }}"#
    )
}

/// `text` with the store's templates filled in, through a file.
fn expr(s: &Store, name: &str, text: &str) -> String {
    fs::read_to_string(s.write(name, text)).unwrap()
}

#[test]
fn failures() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let j = ["--log-format", "internal-json"];
    // a daemon client nix exits 1 on any failed build, and so do the
    // classic mode. `iets build` exits with the build status.
    let fail = expr(
        &s,
        "fail.nix",
        &drv(
            "classic-e2e-fail",
            r#"args = [ "-c" "echo oops; exit 3" ];"#,
        ),
    );
    c.same_record(
        "internal-json: a failed build",
        "build-failed",
        &f.both(
            "nix-build",
            &[&j[..], &["--no-out-link", "-E", &fail]].concat(),
        ),
    );
    let fod_attrs = r#"args = [ "-c" "echo hi > $out" ]; outputHashMode = "flat"; outputHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";"#;
    let fod = expr(&s, "fod.nix", &drv("classic-e2e-fod", fod_attrs));
    c.same_record(
        "internal-json: a hash mismatch",
        "hash-mismatch",
        &f.both(
            "nix-build",
            &[&j[..], &["--no-out-link", "-E", &fod]].concat(),
        ),
    );
    let bf = s.iets().args(["build", "-E", &fail]).output().unwrap();
    c.check(
        "iets build: a failed build exits 100",
        bf.status.code() == Some(100),
        stderr(&bf),
    );

    // a successful build's block ends with its ✔
    let okd = expr(
        &s,
        "order.nix",
        &drv(
            "classic-e2e-order",
            r#"args = [ "-c" "echo from-the-builder; echo x > $out" ];"#,
        ),
    );
    let ob = s.iets().args(["build", "-E", &okd]).output().unwrap();
    let lines: Vec<String> = strip_ansi(&stderr(&ob)).lines().map(String::from).collect();
    let at = |p: &str| lines.iter().position(|l| l.contains(p));
    c.check(
        "iets build: the ✔ is the last line of a successful block",
        ob.status.success()
            && matches!((at("from-the-builder"), at("✔")), (Some(a), Some(b)) if a < b),
        stderr(&ob),
    );
    let hm = s
        .iets()
        .args(["build"])
        .args(j)
        .args(["-E", &fod])
        .output()
        .unwrap();
    c.check(
        "iets build: a hash mismatch exits 102 and names the hash",
        hm.status.code() == Some(102)
            && stderr(&hm).contains(r#""hint":"set the expected hash to "#),
        stderr(&hm),
    );
    // every record of the run, activities too, has the one sid
    let recs = records(&stderr(&hm));
    let mut sids: Vec<&Value> = recs.iter().map(|r| &r["sid"]).collect();
    sids.dedup();
    c.check(
        "iets build: every record has one sid",
        recs.iter().any(|r| r["action"] == "start") && sids.len() == 1 && !sids[0].is_null(),
        format!("{sids:?}"),
    );

    // fix-hashes rewrites the fake hash and the rebuild succeeds
    let fix = s.dir().join("fix");
    let fixed = |label: &str, c: &mut Checks, text: &str| {
        s.write("fix/default.nix", &format!("{{ pkg = {text}; }}\n"));
        let o = s
            .iets()
            .current_dir(&fix)
            .args([
                "--iets-option",
                "fix-hashes",
                "always",
                "build",
                "./default.nix",
                "-A",
                "pkg",
            ])
            .output()
            .unwrap();
        let file = fs::read_to_string(fix.join("default.nix")).unwrap();
        c.check(
            label,
            o.status.success() && !file.contains("AAAAAAAA") && stderr(&o).contains("fixed: "),
            stderr(&o),
        );
    };
    fixed(
        "fix-hashes always: rewrites the hash and builds",
        &mut c,
        &fod,
    );
    // the default asks, and with no terminal nobody answers
    s.write("fix/default.nix", &format!("{{ pkg = {fod}; }}\n"));
    let fa = s
        .iets()
        .current_dir(&fix)
        .args(["build", "./default.nix", "-A", "pkg"])
        .output()
        .unwrap();
    c.check(
        "without a terminal fix-hashes asks nobody, the file stays",
        fa.status.code() == Some(102)
            && fs::read_to_string(fix.join("default.nix"))
                .unwrap()
                .contains("AAAAAAAA"),
        stderr(&fa),
    );
    // a fixed-output drv with an input drv is built as a resolved copy,
    // whose mismatch the daemon reports as a line
    let dep = drv(
        "classic-e2e-fod-dep",
        r#"args = [ "-c" "echo dep > $out" ];"#,
    );
    let fodin = expr(
        &s,
        "fodin.nix",
        &drv("classic-e2e-fod-in", &format!("dep = {dep}; {fod_attrs}")),
    );
    fixed(
        "fix-hashes: a resolved drv's mismatch is fixed too",
        &mut c,
        &fodin,
    );
}

/// A drv with a dependency, fresh for each call.
fn act(s: &Store, n: usize) -> String {
    let dep = drv(
        &format!("classic-e2e-act-dep-{n}"),
        r#"args = [ "-c" "echo dep; echo x > $out" ];"#,
    );
    expr(
        s,
        &format!("act-{n}.nix"),
        &drv(
            &format!("classic-e2e-act-{n}"),
            &format!(r#"args = [ "-c" "echo top; echo $0 > $out" ({dep}) ];"#),
        ),
    )
}

/// The starts and stops of the realise, builds, copy-paths and build
/// activities in order, and the result types each type got.
fn act_shape(stderr: &str) -> (Vec<String>, Vec<String>) {
    let recs = records(stderr);
    let mut ty = BTreeMap::new();
    for r in &recs {
        let t = r["type"].as_i64().unwrap_or(0);
        if r["action"] == "start" && (102..=105).contains(&t) && (t == 105 || r["text"] == "") {
            ty.insert(r["id"].to_string(), t);
        }
    }
    let mine: Vec<&Value> = recs
        .iter()
        .filter(|r| ty.contains_key(&r["id"].to_string()))
        .collect();
    let order = mine
        .iter()
        .filter(|r| r["action"] == "start" || r["action"] == "stop")
        .map(|r| {
            format!(
                "{} {}",
                r["action"].as_str().unwrap(),
                ty[&r["id"].to_string()]
            )
        })
        .collect();
    let mut results: Vec<String> = mine
        .iter()
        .filter(|r| r["action"] == "result")
        .map(|r| format!("{}/{}", ty[&r["id"].to_string()], r["type"]))
        .collect();
    results.sort();
    results.dedup();
    (order, results)
}

/// A build's record stream has the shape of Nix's.
#[test]
fn activity_records() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    for (i, (name, flags)) in [
        ("nix-build", &["--no-out-link", "-E"][..]),
        ("nix", &["build", "--no-link", "--expr"]),
    ]
    .into_iter()
    .enumerate()
    {
        let run = |mut cmd: Command, n| {
            let e = act(&s, n);
            cmd.args(&flags[..flags.len() - 1])
                .args(["--log-format", "internal-json", flags[flags.len() - 1], &e])
                .output()
                .unwrap()
        };
        let o = run(f.ours(name), 2 * i);
        let r = run(f.real(name), 2 * i + 1);
        let (a, b) = (act_shape(&stderr(&r)), act_shape(&stderr(&o)));
        c.check(
            &format!("{name}: activity records match Nix's"),
            o.status.success() && r.status.success() && a == b && !a.0.is_empty(),
            format!("real {a:?}\n  ours {b:?}"),
        );
    }
}

/// `--no-out-link` and `--no-link` ask for no symlink. Nix then lets the
/// output go, and iets keeps a root in its roots store instead.
#[test]
fn the_roots_a_build_keeps() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let e = expr(
        &s,
        "noout.nix",
        &drv("classic-e2e-noout", r#"args = [ "-c" "echo x > $out" ];"#),
    );
    let wd = s.dir().join("noout");
    fs::create_dir(&wd).unwrap();
    let roots = |cache: &Path| -> Vec<PathBuf> {
        let mut v: Vec<_> = fs::read_dir(cache.join("roots"))
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        v.sort();
        v
    };
    for (label, name, a) in [
        (
            "nix-build --no-out-link -E",
            "nix-build",
            &["--no-out-link", "-E"][..],
        ),
        (
            "nix build --no-link --print-out-paths",
            "nix",
            &["build", "--no-link", "--print-out-paths", "--expr"],
        ),
    ] {
        let cache = s.dir().join(format!("broots-{name}"));
        let b = f.both_with(name, &[a, &[&e]].concat(), |c| {
            c.env("IETS_CACHE_DIR", &cache).current_dir(&wd);
        });
        c.same(label, &b);
        let ls = roots(&cache);
        let ok = ls.len() == 1
            && link(&ls[0]) == stdout(&b.ours)
            && rooted(&s, &stdout(&b.ours), &ls[0]);
        c.check(
            &format!("{label} keeps a root"),
            ok && !wd.join("result").exists(),
            format!("{ls:?}"),
        );
    }
    let off = s.dir().join("broots-off");
    let o = f
        .ours("nix-build")
        .env("IETS_CACHE_DIR", &off)
        .args([
            "--iets-option",
            "build-gc-root",
            "false",
            "--no-out-link",
            "-E",
            &e,
        ])
        .output()
        .unwrap();
    c.check(
        "build-gc-root = false keeps no root",
        o.status.success() && !off.join("roots").exists(),
        stderr(&o),
    );
    let nv = s.dir().join("broots-native");
    let o = s
        .iets()
        .env("IETS_CACHE_DIR", &nv)
        .args(["build", "-E", &e])
        .output()
        .unwrap();
    let ls = roots(&nv);
    c.check(
        "iets build roots the path it printed",
        o.status.success() && ls.len() == 1 && link(&ls[0]) == stdout(&o),
        format!("{ls:?}: {}", stderr(&o)),
    );
}

#[test]
fn dry_run_and_json_log_path() {
    let s = store!();
    let mut c = Checks::default();
    let dry = act(&s, 0);
    let dr = online(
        s.nix("nix")
            .args(["build", "--dry-run", "--expr", &dry])
            .output()
            .unwrap(),
    );
    let dj = s
        .iets()
        .args(["build", "--dry-run", "--json", "-E", &dry])
        .output()
        .unwrap();
    let plan: Value = serde_json::from_slice(&dj.stdout).unwrap_or(Value::Null);
    let will: Vec<&str> = plan["willBuild"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    c.check(
        "iets build --dry-run: nix's text, and the plan as JSON",
        dj.status.success()
            && stderr(&dj) == stderr(&dr)
            && will.len() == 2
            && stderr(&dr).contains(&will.join("\n  ")),
        format!(
            "real {}\n  ours {}\n  json {}",
            stderr(&dr),
            stderr(&dj),
            stdout(&dj)
        ),
    );
    let jl = s.dir().join("jlog-build");
    let jb = s
        .iets()
        .arg("build")
        .arg("--json-log-path")
        .arg(&jl)
        .args(["-E", &act(&s, 1)])
        .output()
        .unwrap();
    let n = fs::read_to_string(&jl)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["action"] == "start" && r["type"] == 105)
        .count();
    c.check(
        "json-log-path gets a build's activity records, stderr stays text",
        jb.status.success() && n == 2 && !stderr(&jb).contains("@nix"),
        stderr(&jb),
    );
}

/// The daemon gets the store settings of the command line, as from Nix.
#[test]
fn settings_reach_the_daemon() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let slow = s.write(
        "slow.nix",
        &drv("slow", r#"args = [ "-c" "while :; do :; done" ];"#),
    );
    let to = f.both(
        "nix-build",
        &[
            slow.to_str().unwrap(),
            "--no-out-link",
            "--option",
            "timeout",
            "2",
        ],
    );
    c.same("nix-build --option timeout 2", &to);
    c.check(
        "--option timeout reaches the daemon",
        to.ours.status.code() == Some(1) && stderr(&to.ours).contains("timed out after 2 seconds"),
        stderr(&to.ours),
    );
    // nix develop sends them too: the shell's input times out on both sides
    let input = drv("slow-input", r#"args = [ "-c" "while :; do :; done" ];"#);
    s.write(
        "slow-shell/flake.nix",
        &format!(
            r#"{{ outputs = {{ self }}: {{ devShells.@SYSTEM@.default = derivation {{ name = "slow-shell"; system = "@SYSTEM@"; builder = "@SHELL@"; slow = {input}; }}; }}; }}"#
        ),
    );
    s.write(
        "slow-shell/flake.lock",
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    );
    let r = format!("path:{}", s.dir().join("slow-shell").display());
    let sd = f.both(
        "nix",
        &[
            "develop", "--impure", &r, "--option", "timeout", "2", "-c", "true",
        ],
    );
    c.same("nix develop --option timeout 2", &sd);
    c.check(
        "nix develop sends its build settings to the daemon",
        sd.ours.status.code() == Some(1)
            && strip_ansi(&stderr(&sd.ours)).contains("timed out after 2 seconds"),
        stderr(&sd.ours),
    );
}
