//! `iets build` against a Nix daemon: `--builder daemon`, `unix://` and
//! `ssh-ng://`, a cold and a warm builder store, an output as an input
//! source, and substitution from a binary cache.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Output;

use e2e::{Store, ok, stderr, stdout};
use serde_json::Value;

/// The shared case, with an executable and a symlink in the tree too.
fn make_case(s: &Store, dir: &str, name: &str) -> String {
    let d = s.dir().join(dir);
    s.write(&format!("{dir}/src/run.sh"), "#!/bin/sh\necho hi\n");
    fs::set_permissions(d.join("src/run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("a", d.join("src/link")).unwrap();
    e2e::make_case(s, dir, name)
}

/// `{ca, narHash, narSize, references}` of `path`, from `nix path-info`.
fn path_info(s: &Store, path: &str, store: Option<&str>) -> Value {
    let mut c = s.nix("nix");
    c.arg("path-info");
    if let Some(st) = store {
        c.args(["--store", st]);
    }
    let v: Value = serde_json::from_slice(&ok(c.args(["--json", path])).stdout).unwrap();
    let info = match &v {
        Value::Array(a) => a[0].clone(),
        Value::Object(m) => m.values().next().unwrap().clone(),
        _ => panic!("{v}"),
    };
    let keys = ["ca", "narHash", "narSize", "references"];
    keys.iter()
        .map(|k| (k.to_string(), info[*k].clone()))
        .collect()
}

fn build(s: &Store, args: &[&str]) -> Output {
    let o = s
        .iets()
        .args(["build", "--timings", "--no-tracing-cache"])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?}: {}", stderr(&o));
    o
}

/// Instantiates `case`, records what Nix made, deletes it, builds through
/// `url` and checks that iets registered the same bytes.
fn run_custom(s: &Store, case: &str, url: &str, env: &[(&str, &str)]) {
    let drv = stdout(&ok(s.nix("nix-instantiate").arg(case)));
    let src = s.nix_store(&["-q", "--references", &drv]);
    let expected = s.nix_store(&["-q", "--outputs", &drv]);
    let drv_bytes = fs::read(&drv).unwrap();
    let hashes = s.nix_store(&["-q", "--hash", &drv, &src]);
    let show = stdout(&ok(s.nix("nix").args(["derivation", "show", &drv])));
    let src_info = path_info(s, &src, None);
    let drv_info = path_info(s, &drv, None);
    ok(s.nix("nix-store").args(["--delete", &drv, &src]));
    assert!(
        !s.nix("nix-store")
            .args(["--check-validity", &src])
            .status()
            .unwrap()
            .success()
    );

    let fetch = format!("{case}.fetch");
    let o = s
        .iets()
        .envs(env.iter().copied())
        .args([
            "build",
            "--builder",
            url,
            "--timings",
            "--no-tracing-cache",
            "--fetch-to",
            &fetch,
            case,
        ])
        .output()
        .unwrap();
    let err = stderr(&o);
    assert!(o.status.success(), "{err}");
    assert!(
        !err.contains("read from /nix/store"),
        "eval fell back to the store: {err}"
    );
    let out = stdout(&o);
    assert_eq!(out, expected);
    let dir = Path::new(case).parent().unwrap();
    let mut want = fs::read(dir.join("src/a")).unwrap();
    want.extend(fs::read(dir.join("src/sub/b")).unwrap());
    assert_eq!(fs::read(&out).unwrap(), want);
    ok(s.nix("nix-store").args(["--check-validity", &drv, &src]));
    assert_eq!(fs::read(&drv).unwrap(), drv_bytes, "the registered drv");
    assert_eq!(s.nix_store(&["-q", "--hash", &drv, &src]), hashes);
    assert_eq!(
        stdout(&ok(s.nix("nix").args(["derivation", "show", &drv]))),
        show
    );
    assert_eq!(path_info(s, &src, None), src_info);
    assert_eq!(path_info(s, &drv, None), drv_info);
    let run = fs::metadata(format!("{src}/run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert!(run & 0o111 != 0, "the executable bit");
    assert_eq!(
        fs::read_link(format!("{src}/link")).unwrap(),
        Path::new("a")
    );
    let base = out.rsplit('/').next().unwrap();
    assert_eq!(
        fs::read(format!("{fetch}/{base}")).unwrap(),
        fs::read(&out).unwrap()
    );
}

#[test]
fn a_source_tree_through_each_transport() {
    let s = store!();
    let c1 = make_case(&s, "c1", "iets-daemon-e2e");
    run_custom(&s, &c1, "daemon", &[]);
    let c5 = make_case(&s, "c5", "iets-daemon-e2e-unix");
    run_custom(&s, &c5, &format!("unix://{}", s.socket().display()), &[]);

    // an `ssh` that ignores the host and runs the daemon on stdio here
    let bin = s.dir().join("bin");
    s.write("bin/ssh", "#!/bin/sh\nexec nix-daemon --stdio\n");
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let c6 = make_case(&s, "c6", "iets-daemon-e2e-ssh");
    run_custom(&s, &c6, "ssh-ng://shim", &[("PATH", &path)]);
}

fn query(err: &str) -> (u64, u64) {
    let l = err
        .lines()
        .find(|l| l.starts_with("query: "))
        .unwrap_or_else(|| panic!("{err}"));
    let mut n = l
        .trim_start_matches("query: ")
        .split(|c: char| !c.is_ascii_digit())
        .filter(|w| !w.is_empty())
        .map(|w| w.parse().unwrap());
    (n.next().unwrap(), n.next().unwrap())
}

/// The chain's closure: built here first, then uploaded to a cold builder
/// in one AddMultipleToStore, then found there warm.
#[cfg(target_os = "linux")]
#[test]
fn a_closure_cold_and_warm() {
    let s = store!();
    let chain = s.pkgs().join("chain.nix");
    let chain = chain.to_str().unwrap();
    let drv = stdout(&ok(s.nix("nix-instantiate").arg(chain)));
    let out = s.nix_store(&["-r", &drv]);

    // the closure is valid here, so only QueryValidPaths and the realise run
    let o = build(&s, &["--builder", "daemon", chain]);
    assert_eq!(stdout(&o), out);
    assert!(!stderr(&o).contains("read from /nix/store"));

    let b = s.builder("b", &[]);
    let o = build(&s, &["--builder", &b.url(), "--no-build", chain]);
    let err = stderr(&o);
    let (paths, missing) = query(&err);
    assert!(
        paths > 0 && paths == missing,
        "a cold store misses every path: {err}"
    );
    let store = b.store();
    assert_eq!(
        stdout(&ok(s
            .nix("nix-store")
            .args(["--store", &store, "-q", "--hash", &drv]))),
        s.nix_store(&["-q", "--hash", &drv])
    );
    assert_eq!(fs::read(b.real(&drv)).unwrap(), fs::read(&drv).unwrap());
    let refs = s.nix_store(&["-q", "--references", &drv]);
    let sources: Vec<&str> = refs.lines().filter(|p| !p.ends_with(".drv")).collect();
    assert!(!sources.is_empty());
    for p in sources {
        assert_eq!(
            path_info(&s, p, Some(&store)),
            path_info(&s, p, None),
            "{p}"
        );
    }

    let o = build(&s, &["--builder", &b.url(), "--no-build", chain]);
    assert!(
        stderr(&o).contains("query: 1 paths, 0 missing"),
        "{}",
        stderr(&o)
    );
}

/// An input-addressed output as an input source (`builtins.storePath`).
#[cfg(target_os = "linux")]
#[test]
fn an_output_as_an_input_source() {
    let s = store!();
    let chain = s.pkgs().join("chain.nix");
    let dep = stdout(&ok(s.iets().arg("build").arg("-f").arg(&chain)));
    let case = s.write(
        "c7/case.nix",
        &r#"derivation {
  name = "iets-daemon-e2e-storepath";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "while read -r l; do echo \"$l\"; done < ${builtins.storePath "@DEP@"}/n > $out" ];
}
"#
        .replace("@DEP@", &dep),
    );
    let case = case.to_str().unwrap();
    let drv = stdout(&ok(s.nix("nix-instantiate").arg(case)));
    assert!(
        s.nix_store(&["-q", "--references", &drv])
            .lines()
            .any(|l| l == dep)
    );
    let expected = s.nix_store(&["-q", "--outputs", &drv]);
    let drv_bytes = fs::read(&drv).unwrap();
    ok(s.nix("nix-store").args(["--delete", &drv]));

    // the local daemon has the output and lacks the drv
    let o = build(
        &s,
        &[
            "--builder",
            "daemon",
            "--option",
            "pure-eval",
            "false",
            case,
        ],
    );
    let err = stderr(&o);
    assert!(err.contains("query: 2 paths, 1 missing"), "{err}");
    assert!(!err.contains("substitute:"), "{err}");
    assert_eq!(stdout(&o), expected);
    assert_eq!(fs::read_to_string(&expected).unwrap(), "3\n");
    assert_eq!(fs::read(&drv).unwrap(), drv_bytes);

    // a builder that lacks the output substitutes it from a file:// cache
    let cache = format!("file://{}/cache", s.dir().display());
    ok(s.nix("nix")
        .args(["copy", "--to", &format!("{cache}?compression=none"), &dep]));
    let b = s.builder("b2", &[("substituters", &cache), ("require-sigs", "false")]);
    let store = b.store();
    let valid = |p: &str| {
        s.nix("nix-store")
            .args(["--store", &store, "--check-validity", p])
            .status()
            .unwrap()
            .success()
    };
    assert!(!valid(&dep));
    let o = build(
        &s,
        &[
            "--builder",
            &b.url(),
            "--no-build",
            "--option",
            "pure-eval",
            "false",
            case,
        ],
    );
    let err = stderr(&o);
    assert!(err.contains("query: 2 paths, 2 missing"), "{err}");
    assert!(
        err.contains("substitute: 1 paths left to the daemon"),
        "{err}"
    );
    assert!(
        err.contains("add: 1 paths"),
        "only the drv is uploaded: {err}"
    );
    assert!(valid(&dep) && valid(&drv));
    assert_eq!(fs::read(b.real(&drv)).unwrap(), drv_bytes);

    let o = build(
        &s,
        &[
            "--builder",
            &b.url(),
            "--option",
            "pure-eval",
            "false",
            case,
        ],
    );
    assert!(
        stderr(&o).contains("query: 2 paths, 0 missing"),
        "{}",
        stderr(&o)
    );
    assert_eq!(stdout(&o), expected);
    assert_eq!(fs::read_to_string(b.real(&expected)).unwrap(), "3\n");
}

/// The stdout contract (docs/reference/commands.md): exit 0 put a path on
/// stdout, and a run that puts none exits non-zero with an error.
/// A `fetchClosure` path a builder lacks goes there with the cache's
/// signature, and a builder that does not trust the key refuses it.
#[cfg(target_os = "linux")]
#[test]
fn a_fetched_closure_goes_to_the_builder() {
    let Some(s) = Store::start_with(&[]) else {
        return;
    };
    // a chain whose large middle starts before its references, and before
    // the root that waits on it alone
    let dep_case = s.write(
        "fc-dep/default.nix",
        r#"let
  mk = name: script: derivation { inherit name; system = "@SYSTEM@"; builder = "@SHELL@"; args = [ "-c" script ]; };
  a = mk "fc-a" "echo a > $out";
  b = mk "fc-b" "echo ${a} > $out";
  big = mk "fc-big" "{ echo ${b}; i=0; while [ $i -lt 20000 ]; do echo $i; i=$((i+1)); done; } > $out";
in
mk "fc-dep" "echo ${big} > $out"
"#,
    );
    let dep = stdout(&ok(s
        .nix("nix-build")
        .args(["--no-out-link".as_ref(), dep_case.as_os_str()])));
    let (sk, pk) = (s.dir().join("sk"), s.dir().join("pk"));
    ok(s.nix("nix-store").arg("--generate-binary-cache-key").args([
        "test-1".as_ref(),
        sk.as_os_str(),
        pk.as_os_str(),
    ]));
    let pk = fs::read_to_string(&pk).unwrap();
    let cache = format!("file://{}/fc-cache", s.dir().display());
    let to = format!("{cache}?compression=zstd&secret-key={}", sk.display());
    ok(s.nix("nix").args(["copy", "--to", &to, &dep]));
    let case = s.write(
        "fc/case.nix",
        &r#"let
  dep = builtins.fetchClosure { fromStore = "@CACHE@"; fromPath = @DEP@; inputAddressed = true; };
in
derivation {
  name = "fc-use";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "while read -r l; do echo \"$l\"; done < ${dep} > $out" ];
}
"#
        .replace("@CACHE@", &cache)
        .replace("@DEP@", &dep),
    );
    let case = case.to_str().unwrap();
    let run_with = |url: &str, flags: &[&str]| {
        s.iets()
            .env("_NIX_IN_TEST", "1")
            .args(["build", "--timings", "--builder", url])
            .args(flags)
            .args(["--option", "trusted-public-keys", &pk, case])
            .output()
            .unwrap()
    };
    let run = |url: &str| run_with(url, &["--no-tracing-cache"]);

    let b = s.builder("b-trusts", &[("trusted-public-keys", &pk)]);
    let o = run(&b.url());
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert_eq!(
        fs::read_to_string(b.real(&out)).unwrap(),
        fs::read_to_string(&dep).unwrap()
    );
    assert_eq!(
        path_info(&s, &dep, Some(&b.store())),
        path_info(&s, &dep, None)
    );

    // a cache hit knows the closure without evaluating
    for name in ["b-miss", "b-hit"] {
        let b = s.builder(name, &[("trusted-public-keys", &pk)]);
        let o = run_with(&b.url(), &[]);
        assert!(o.status.success(), "{name}: {}", stderr(&o));
        assert_eq!(stdout(&o), out);
        if name == "b-hit" {
            assert!(stderr(&o).contains("eval: skipped"), "{}", stderr(&o));
        }
    }

    let b = s.builder("b-distrusts", &[]);
    let o = run(&b.url());
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("lacks a signature by a trusted key"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_path_on_stdout_or_a_failure() {
    let s = store!();
    let case = make_case(&s, "c9", "iets-contract-ok");
    let run = |args: &[&str]| {
        s.iets()
            .args(["build", "--builder", "daemon", "--no-tracing-cache"])
            .args(args)
            .output()
            .unwrap()
    };
    let o = run(&[&case]);
    assert!(
        o.status.success() && !stdout(&o).is_empty(),
        "{}",
        stderr(&o)
    );

    let fail = s.write(
        "c9/fail.nix",
        "derivation { name = \"iets-contract-fail\"; system = \"@SYSTEM@\"; builder = \"@SHELL@\"; args = [ \"-c\" \"exit 3\" ]; }\n",
    );
    let bad = run(&[fail.to_str().unwrap()]);
    assert!(!bad.status.success() && stdout(&bad).is_empty());
    let empty = run(&["-E", "[]"]);
    assert!(!empty.status.success() && stdout(&empty).is_empty());
    assert!(
        stderr(&empty).contains("nothing to build"),
        "{}",
        stderr(&empty)
    );
    assert_eq!(run(&["--no-build", "-E", "[]"]).status.code(), Some(1));
    let wrong = run(&["--out", "bogus", &case]);
    assert!(!wrong.status.success() && stdout(&wrong).is_empty());
}
