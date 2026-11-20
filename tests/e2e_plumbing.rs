//! `iets plumbing`, one test per op, against the JSON shape that
//! docs/reference/plumbing.md specifies.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::fs;

use e2e::{Store, code, json, ok, parse, pipe, stdout};
use serde_json::{Value, json};

const HELLO: &str = r#"derivation {
  name = "plumb-hello";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo @NONCE@ > $out" ];
}
"#;

const CHAIN: &str = r#"let
  dep = derivation {
    name = "plumb-hello";
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "echo @NONCE@ > $out" ];
  };
in derivation {
  name = "plumb-top";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo ${dep} > $out" ];
}
"#;

// one derivation at the top, one under a recursed set, one under a set that
// is not recursed, and one whose meta.outputsToInstall picks an output
const SET: &str = r#"let
  mk = n: outs: derivation {
    name = n;
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "echo @NONCE@ > $out" ];
    outputs = outs;
  };
in {
  a = mk "plumb-wa" [ "out" ];
  sub = { recurseForDerivations = true; b = mk "plumb-wb" [ "out" ]; };
  hidden = { c = mk "plumb-wc" [ "out" ]; };
  multi = (mk "plumb-wm" [ "out" "dev" ]) // { meta.outputsToInstall = [ "dev" ]; };
}
"#;

// one derivation whose `args` interpolate 32 others, so the parallel
// evaluator coerces them on helper fibers
const MANY: &str = r#"let
  dep = n: derivation {
    name = "plumb-dep-${toString n}";
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "echo @NONCE@ ${toString n} > $out" ];
  };
  deps = builtins.genList dep 32;
in derivation {
  name = "plumb-root";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo @NONCE@ > $out" ] ++ (map (d: "${d}") deps);
}
"#;

// A helper claims one group's thunk and makes nine derivations under it. The
// demand path reaches those values through the attribute, never through the
// thunk the helper claimed. Held back, the dump lost 126 to 142 of the 145.
const DEEP: &str = r#"let
  dep = n: derivation {
    name = "plumb-deep-dep-${toString n}";
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "echo @NONCE@ ${toString n} > $out" ];
  };
  group = g: {
    drv = derivation {
      name = "plumb-deep-group-${toString g}";
      system = "@SYSTEM@";
      builder = "@SHELL@";
      args = [ "-c" "echo @NONCE@ > $out" ] ++ map (n: "${dep (g * 8 + n)}") (builtins.genList (x: x) 8);
    };
  };
  groups = map (g: (group g).drv) (builtins.genList (x: x) 16);
in derivation {
  name = "plumb-deep-root";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo @NONCE@ > $out" ] ++ map (d: "${d}") groups;
}
"#;

const BAD: &str = r#"derivation {
  name = "plumb-bad";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo boom @NONCE@ >&2; exit 7" ];
}
"#;

/// The ops read the closure from the store, so a root is instantiated first.
fn instantiate(s: &Store, name: &str, text: &str) -> String {
    s.write(name, text);
    stdout(&ok(s.nix("nix-instantiate").arg(name)))
}

fn plumbing(s: &Store, args: &[&str]) -> Value {
    json(s.iets().arg("plumbing").args(args))
}

fn count(s: &Store, dir: &str) -> usize {
    fs::read_dir(s.dir().join(dir)).unwrap().count()
}

#[test]
fn resolve() {
    let s = store!();
    let hello = instantiate(&s, "hello.nix", HELLO);
    let r = plumbing(&s, &["resolve", "hello.nix"]);
    assert_eq!(r["roots"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["roots"][0]["drv"], hello.as_str());

    // --walk get-drvs is get-drvs.cc, which nix-instantiate walks
    let walked = instantiate(&s, "set.nix", SET);
    let w = plumbing(&s, &["resolve", "--walk", "get-drvs", "set.nix"]);
    let roots = w["roots"].as_array().unwrap();
    let drvs: Vec<&str> = roots.iter().map(|r| r["drv"].as_str().unwrap()).collect();
    assert_eq!(drvs, walked.lines().collect::<Vec<_>>());
    let attrs: Vec<&str> = roots.iter().map(|r| r["attr"].as_str().unwrap()).collect();
    assert_eq!(attrs, ["a", "multi", "sub.b"]);

    // --walk to-install is get-drvs.cc:queryOutputs, which nix build installs
    let t = plumbing(
        &s,
        &["resolve", "--walk", "to-install", "set.nix", "-A", "multi"],
    );
    assert_eq!(t["roots"][0]["output"], "dev", "{t}");
    let d = plumbing(&s, &["resolve", "set.nix", "-A", "multi"]);
    assert_eq!(d["roots"][0]["output"], Value::Null, "{d}");
}

#[test]
fn resolve_dumps_every_drv_whichever_fiber_made_it() {
    let s = store!();
    s.write("many.nix", MANY);
    let many = plumbing(&s, &["resolve", "--dump-drvs", "many", "many.nix"]);
    assert_eq!(many["roots"].as_array().unwrap().len(), 1, "{many}");
    assert_eq!(count(&s, "many"), 33);

    // a held-back dump loses a different subset each run
    s.write("deep.nix", DEEP);
    for i in 0..3 {
        let dir = format!("deep{i}");
        plumbing(&s, &["resolve", "--dump-drvs", &dir, "deep.nix"]);
        assert_eq!(count(&s, &dir), 145, "run {i}");
    }
}

#[test]
fn plan() {
    let s = store!();
    let hello = instantiate(&s, "hello.nix", HELLO);
    let p = plumbing(&s, &["plan", &hello]);
    assert_eq!(p["drvs"], json!([hello]));
    assert_eq!(p["storeDrvs"], 1);
    assert_eq!(p["sources"], json!([]));
}

#[test]
fn missing() {
    let s = store!();
    let hello = instantiate(&s, "hello.nix", HELLO);
    let top = instantiate(&s, "chain.nix", CHAIN);
    let m = plumbing(&s, &["missing", &hello]);
    assert_eq!(m["willBuild"], json!([hello]));
    let mut keys: Vec<&String> = m.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "downloadSize",
            "narSize",
            "unknown",
            "willBuild",
            "willSubstitute"
        ]
    );
    let c = plumbing(&s, &["missing", "--check-cache-status", &hello]);
    assert_eq!(c["roots"][0]["cacheStatus"], "notBuilt", "{c}");
    assert_eq!(c["roots"][0]["neededBuilds"], json!([hello]));

    // docs/reference/plumbing.md: byte for byte `iets build --dry-run --json`
    let op = ok(s.iets().args(["plumbing", "missing", &top])).stdout;
    let dry = ok(s.iets().args(["build", "--dry-run", "--json", "chain.nix"])).stdout;
    assert_eq!(
        String::from_utf8(op).unwrap(),
        String::from_utf8(dry).unwrap()
    );
}

#[test]
fn build() {
    let s = store!();
    let hello = instantiate(&s, "hello.nix", HELLO);
    let bad = instantiate(&s, "bad.nix", BAD);
    let b = plumbing(&s, &["build", &hello]);
    assert_eq!(b["outcomes"][0]["success"], true, "{b}");
    let out = b["outcomes"][0]["outputs"]["out"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(fs::exists(&out).unwrap(), "{out}");
    assert_eq!(code(s.iets().args(["plumbing", "build", &bad])), 100);

    let gc = s.dir().join("gc");
    fs::create_dir(&gc).unwrap();
    let g = plumbing(&s, &["build", "--gc-root", gc.to_str().unwrap(), &hello]);
    let link = gc.join(out.rsplit('/').next().unwrap());
    assert_eq!(g["gcRoots"], json!([link]));
    assert_eq!(fs::read_link(&link).unwrap().to_str().unwrap(), out);

    let n = plumbing(&s, &["build", "--no-build", &hello]);
    assert_eq!(n["outcomes"], json!([]));
}

#[test]
fn a_bare_root_keeps_every_output_in_a_resolve_pipeline() {
    let s = store!();
    s.write(
        "mixed.nix",
        r#"
        let a = derivation {
          name = "mixed"; system = "@SYSTEM@"; builder = "@SHELL@";
          outputs = [ "out" "dev" "doc" ];
          args = [ "-c" "echo @NONCE@ > $out; echo b > $dev; echo c > $doc" ];
        }; in [ a.dev.outPath a ]
    "#,
    );
    let outputs = |flags: &[&str]| {
        let result = pipe(
            s.iets()
                .args(["plumbing", "resolve", "--dump-drvs", "dump", "mixed.nix"]),
            s.iets()
                .args(["plumbing", "build", "--roots", "-"])
                .args(flags),
        );
        assert!(result.status.success(), "{}", e2e::stderr(&result));
        parse(&result)["outcomes"][0]["outputs"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(outputs(&[]), ["dev", "doc", "out"]);
    assert_eq!(outputs(&["--out", "out"]), ["dev", "out"]);
}

#[test]
fn composition() {
    let s = store!();
    let hello = instantiate(&s, "hello.nix", HELLO);
    let piped = pipe(
        s.iets().args(["plumbing", "plan", &hello]),
        s.iets().args(["plumbing", "build", "--roots", "-"]),
    );
    let piped = parse(&piped);
    let out = piped["outcomes"][0]["outputs"]["out"].as_str().unwrap();
    assert!(out.ends_with("-plumb-hello"), "{piped}");
    let via = pipe(
        s.iets().args(["plumbing", "resolve", "hello.nix"]),
        s.iets().args(["plumbing", "missing", "--roots", "-"]),
    );
    let via = parse(&via);
    assert_eq!(via["willBuild"], json!([]), "already built");

    // a closure in no store: only --dump-drvs carries it across the pipe
    s.write("fresh.nix", &CHAIN.replace("@NONCE@", "fresh-@NONCE@"));
    let bare = pipe(
        s.iets().args(["plumbing", "resolve", "fresh.nix"]),
        s.iets().args(["plumbing", "plan", "--roots", "-"]),
    );
    assert!(!bare.status.success(), "the drv is in no store");
    let resolve = || {
        let mut c = s.iets();
        c.args(["plumbing", "resolve", "--dump-drvs", "dump", "fresh.nix"]);
        c
    };
    let c = pipe(
        &mut resolve(),
        s.iets().args(["plumbing", "plan", "--roots", "-"]),
    );
    let c = parse(&c);
    assert_eq!(c["storeDrvs"], 0, "{c}");
    assert_eq!(c["drvs"].as_array().unwrap().len(), 2, "{c}");
    let cb = pipe(
        &mut resolve(),
        s.iets().args(["plumbing", "build", "--roots", "-"]),
    );
    let cb = parse(&cb);
    assert_eq!(cb["outcomes"][0]["success"], true, "{cb}");
}

#[test]
fn fetch() {
    let s = store!();
    let file = s.write("bytes.txt", "fetch me\n");
    let url = format!("file://{}", file.display());
    let f = plumbing(&s, &["fetch", &url]);
    let hashed = stdout(&ok(s.nix("nix").args(["hash", "file", "--sri"]).arg(&file)));
    assert_eq!(f, json!({ "hash": hashed, "url": url }));
    let sha512 = plumbing(&s, &["fetch", "--hash-type", "sha512", &url]);
    let want = ok(s
        .nix("nix")
        .args(["hash", "file", "--sri", "--type", "sha512"])
        .arg(&file));
    assert_eq!(sha512["hash"], stdout(&want));
}
