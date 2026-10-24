//! nix-shell, nix develop and nix fmt in classic mode, against the
//! real Nix, on the fake stdenv of `tests/e2e/pkgs/shell.nix`.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use e2e::classic::{Both, Checks, Farm, env_lines, link, show};
use e2e::{Store, ok, resolved, stderr, stdout};

/// Both sides start this bash: nix-shell would evaluate `<nixpkgs>` for one.
fn bash() -> PathBuf {
    resolved("bash")
}

fn same_env(c: &mut Checks, label: &str, b: &Both) {
    let (a, o) = (env_lines(&b.real.stdout), env_lines(&b.ours.stdout));
    let only = |x: &[String], y: &[String]| {
        x.iter()
            .filter(|l| !y.contains(l))
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
    };
    c.check(
        &format!("{label} ({} variables)", a.len()),
        a == o && !a.is_empty() && b.ours.status.code() == b.real.status.code(),
        format!(
            "only real {:?}\n  only ours {:?}\n  ours stderr {}\n  real stderr {}",
            only(&a, &o),
            only(&o, &a),
            stderr(&b.ours),
            stderr(&b.real)
        ),
    );
}

/// The links in `cache`'s roots store, the target of the first, and whether
/// `store` names that link a root of it.
fn rooted(s: &Store, cache: &Path, store: &[&str]) -> (Vec<String>, String, bool) {
    let ls = e2e::ls(&cache.join("roots"));
    let first = ls.first().map(|l| cache.join("roots").join(l));
    let target = first.as_deref().map(link).unwrap_or_default();
    let held = first.is_some_and(|l| {
        stdout(&ok(s
            .nix("nix-store")
            .args(store)
            .args(["--query", "--roots", &target])))
        .contains(l.to_str().unwrap())
    });
    (ls, target, held)
}

#[test]
fn nix_shell() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let shell = s.pkgs().join("shell.nix");
    let shell = shell.to_str().unwrap();
    let both = |a: &[&str], g: &dyn Fn(&mut Command)| {
        f.both_with("nix-shell", a, |c| {
            c.env("NIX_BUILD_SHELL", bash());
            g(c);
        })
    };
    c.same(
        "nix-shell --run 'echo $name'",
        &both(&[shell, "--run", "echo $name"], &|_| {}),
    );
    same_env(
        &mut c,
        "nix-shell --run env",
        &both(&[shell, "--run", "env"], &|_| {}),
    );
    same_env(
        &mut c,
        "nix-shell --pure --run env",
        &both(&[shell, "--pure", "--run", "env"], &|_| {}),
    );
    c.same(
        "nix-shell --pure --keep FOO",
        &both(
            &[
                shell,
                "--pure",
                "--keep",
                "FOO",
                "--run",
                r#"echo "[$FOO]""#,
            ],
            &|c| {
                c.env("FOO", "bar");
            },
        ),
    );
    // --command is interactive unless stdin is not a tty
    c.same(
        "nix-shell --command, stdin not a tty",
        &both(&[shell, "--command", "echo $name"], &|c| {
            c.stdin(Stdio::null());
        }),
    );

    // shell.nix wins over default.nix, and passAsFile
    let t = r#"derivation {
  name = "classic-e2e-shell";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo hi > $out" ];
  MARK = "from-@WHICH@";
  passAsFile = "BIG";
  BIG = "big-value";
}
"#;
    s.write("shellnix/shell.nix", &t.replace("@WHICH@", "shell-nix"));
    s.write("shellnix/default.nix", &t.replace("@WHICH@", "default-nix"));
    let sh = s.dir().join("shellnix");
    let here = |c: &mut Command| {
        c.current_dir(&sh);
    };
    c.same(
        "nix-shell with no argument prefers shell.nix",
        &both(&["--run", "echo $MARK; cat $BIGPath"], &here),
    );
    c.same(
        "nix-shell --run 'exit 3' keeps the exit code",
        &both(&["--run", "exit 3"], &here),
    );
    let empty = s.dir().join("empty");
    fs::create_dir(&empty).unwrap();
    c.same(
        "nix-shell with no shell.nix or default.nix",
        &both(&["--run", "true"], &|c| {
            c.current_dir(&empty);
        }),
    );
}

/// nix-shell keeps what it realised under GC roots of its own. The
/// collection runs on a builder store.
#[cfg(target_os = "linux")]
#[test]
fn nix_shell_roots() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    s.write(
        "shellroot/shell.nix",
        r#"let
  dep = derivation {
    name = "shellroot-dep";
    system = "@SYSTEM@";
    builder = "@SHELL@";
    args = [ "-c" "echo building the dep >&2; echo @NONCE@ > $out" ];
  };
in derivation {
  name = "shellroot-shell";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "echo hi > $out" ];
  DEP = dep;
}
"#,
    );
    let b = s.builder("sr", &[("substituters", ""), ("builders", "")]);
    let wd = s.dir().join("shellroot");
    let enter = |cache: &Path, extra: &[&str]| {
        f.ours("nix-shell")
            .env("IETS_CACHE_DIR", cache)
            .env("NIX_BUILD_SHELL", bash())
            .current_dir(&wd)
            .args(extra)
            .args(["--option", "store", &b.url(), "--run", "true"])
            .output()
            .unwrap()
    };
    let gc = || ok_status(s.nix("nix-store").args(["--store", &b.store(), "--gc"]));
    let cache = s.dir().join("shellroots");
    let s1 = enter(&cache, &[]);
    let (ls, target, held) = rooted(&s, &cache, &["--store", &b.store()]);
    c.check(
        "nix-shell roots the input it realised",
        s1.status.success()
            && stderr(&s1).contains("building the dep")
            && ls.len() == 1
            && target.ends_with("-shellroot-dep")
            && held,
        format!("{ls:?} -> {target}: {}", stderr(&s1)),
    );
    // the collection keeps the rooted output, so the next entry builds nothing
    let collected = gc();
    let kept = b.real(&target).exists();
    let s2 = enter(&cache, &[]);
    c.check(
        "a collection leaves nix-shell's root, and the next entry builds nothing",
        collected && kept && s2.status.success() && !stderr(&s2).contains("building the dep"),
        stderr(&s2),
    );
    let cg = s
        .iets()
        .args(["cache", "gc", "--ttl", "0", "--cache-dir"])
        .arg(&cache)
        .output()
        .unwrap();
    c.check(
        "iets cache gc sweeps nix-shell's roots",
        cg.status.success()
            && stdout(&cg).contains(&format!("{}/roots: 1 of 1 entries", cache.display()))
            && !cache.join("roots").join(&ls[0]).exists(),
        stdout(&cg),
    );
    // with shell-gc-root off, the same collection takes the output
    let off = s.dir().join("shellroots-off");
    let flag = ["--iets-option", "shell-gc-root", "false"];
    let s3 = enter(&off, &flag);
    let no_roots = !off.join("roots").exists();
    let collected = gc();
    let s4 = enter(&off, &flag);
    c.check(
        "shell-gc-root = false keeps no root, and the collection takes the output",
        s3.status.success() && no_roots && collected && stderr(&s4).contains("building the dep"),
        stderr(&s4),
    );
}

fn ok_status(c: &mut Command) -> bool {
    c.output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn nix_develop() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let r = format!("path:{}", s.pkgs().display());
    let bash = bash();
    let bash = bash.to_str().unwrap();
    same_env(
        &mut c,
        "nix develop -c env",
        &f.both("nix", &["develop", &r, "-c", "env"]),
    );
    c.same(
        "nix develop -c bash -c 'echo $name'",
        &f.both("nix", &["develop", &r, "-c", bash, "-c", "echo $name"]),
    );
    same_env(
        &mut c,
        "nix develop --ignore-environment --keep HOME --keep PATH",
        // without PATH the real nix finds no bash in the sandbox
        &f.both(
            "nix",
            &["develop", &r, "-i", "-k", "HOME", "-k", "PATH", "-c", "env"],
        ),
    );
    c.same(
        "nix develop -u HOME",
        &f.both(
            "nix",
            &[
                "develop",
                &r,
                "-u",
                "HOME",
                "-c",
                bash,
                "-c",
                r#"echo "[$HOME]""#,
            ],
        ),
    );
    // one link in the roots store, which a second entry replaces in place
    let cache = s.dir().join("devcache");
    let run = |cache: &Path, extra: &[&str]| -> Output {
        f.ours("nix")
            .env("IETS_CACHE_DIR", cache)
            .arg("develop")
            .args(extra)
            .args([&r, "-c", "true"])
            .output()
            .unwrap()
    };
    let (a, b) = (run(&cache, &[]), run(&cache, &[]));
    let (ls, target, held) = rooted(&s, &cache, &[]);
    c.check(
        "nix develop keeps the shell's environment under a GC root",
        a.status.success()
            && b.status.success()
            && ls.len() == 1
            && target.ends_with("-env")
            && held,
        format!("{ls:?} -> {target}: {}", stderr(&a)),
    );
    let off = s.dir().join("devcache-off");
    let o = run(&off, &["--iets-option", "shell-gc-root", "false"]);
    c.check(
        "shell-gc-root = false keeps no root for nix develop",
        o.status.success() && !off.join("roots").exists(),
        stderr(&o),
    );
}

/// The flake's formatter.<system>, built once and kept alive by an out-link
/// that is also a GC root.
#[test]
fn nix_fmt() {
    let s = store!();
    let f = Farm::new(&s);
    let mut c = Checks::default();
    let dir = s.dir().join("fmtflake");
    fs::create_dir(&dir).unwrap();
    for n in ["config.nix", "env.nix"] {
        fs::copy(s.pkgs().join(n), dir.join(n)).unwrap();
    }
    // a versioned name and no meta.mainProgram: the program is `fmt-e2e`
    s.write(
        "fmtflake/flake.nix",
        r#"{
  outputs = { self }:
    let inherit (import ./config.nix) mkDerivation system shell; in {
      formatter.${system} = mkDerivation {
        name = "fmt-e2e-1.0";
        buildCommand = ''
          mkdir -p $out/bin
          printf '#!%s\necho "PRJ_ROOT=$PRJ_ROOT"\necho "args:$@"\n' ${shell} > $out/bin/fmt-e2e
          chmod +x $out/bin/fmt-e2e
        '';
      };
    };
}
"#,
    );
    s.write(
        "fmtflake/flake.lock",
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    );
    let cache = s.dir().join("fmtcache");
    let both = |a: &[&str]| {
        f.both_with("nix", a, |c| {
            c.env("IETS_CACHE_DIR", &cache).current_dir(&dir);
        })
    };
    let first = both(&["fmt"]);
    c.same("nix fmt with no arguments", &first);
    c.check(
        "nix fmt runs the formatter",
        stdout(&first.ours).contains("args:"),
        show(&first),
    );
    c.same("nix fmt with paths", &both(&["fmt", "a.nix", "sub/b.nix"]));
    c.same(
        "nix fmt -- --write x.nix",
        &both(&["fmt", "--", "--write", "x.nix"]),
    );
    // an edit invalidates silently, and json-log-path gets the record
    let text = fs::read_to_string(dir.join("flake.nix")).unwrap();
    fs::write(dir.join("flake.nix"), text + "\n# edited\n").unwrap();
    let jl = s.dir().join("fmt-jlog");
    let inv = f
        .ours("nix")
        .env("IETS_CACHE_DIR", &cache)
        .current_dir(&dir)
        .args(["fmt", "--option", "json-log-path", jl.to_str().unwrap()])
        .output()
        .unwrap();
    let n = fs::read_to_string(&jl)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(r#""code":"cache-invalidated""#))
        .count();
    c.check(
        "nix fmt: an invalidation is silent on stderr and one record",
        inv.status.success() && !stderr(&inv).contains("re-evaluating") && n == 1,
        format!("{n} records: {}", stderr(&inv)),
    );
    let (ls, target, held) = rooted(&s, &cache, &[]);
    c.check(
        "nix fmt keeps the formatter under a GC root",
        ls.len() == 1 && target.starts_with(s.store_dir().to_str().unwrap()) && held,
        format!("{ls:?} -> {target}"),
    );
    let gc = s
        .iets()
        .args(["cache", "gc", "--ttl", "0", "--cache-dir"])
        .arg(&cache)
        .output()
        .unwrap();
    c.check(
        "iets cache gc sweeps the roots store",
        gc.status.success()
            && stdout(&gc).contains(&format!("{}/roots: 1 of 1 entries", cache.display()))
            && ls
                .first()
                .is_some_and(|l| !cache.join("roots").join(l).exists()),
        stdout(&gc),
    );
    // a flake with no formatter: nix's own message, and no fallback
    let nofmt = f.both_with("nix", &["fmt"], |c| {
        c.current_dir(s.pkgs());
    });
    c.same("nix fmt without a formatter output", &nofmt);
    let want = format!("does not provide attribute 'formatter.{}'", e2e::system());
    c.check(
        "nix fmt names the missing attribute itself",
        stderr(&nofmt.ours).contains(&want)
            && stderr(&nofmt.real).contains(&want)
            && !stderr(&nofmt.ours).contains("falling back"),
        show(&nofmt),
    );
}
