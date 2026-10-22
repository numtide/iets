//! A nix-daemon on a private store dir, for tests that build. Each `Store`
//! has its own daemon, so tests run in parallel without root.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::fmt::Display;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs, thread};

use tempfile::TempDir;

use crate::hermetic::{self, Iets};

pub mod classic;

const CONF: &str = "\
experimental-features = nix-command flakes
substituters =
trusted-users = *
build-users-group =
sandbox = false
";

pub struct Store {
    dir: TempDir,
    daemon: Child,
    shell: PathBuf,
    nonce: String,
    bin: Iets,
}

pub fn which(prog: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
}

/// `prog` on `PATH` in the real store, out of any profile.
pub fn resolved(prog: &str) -> PathBuf {
    let p = which(prog).unwrap_or_else(|| panic!("{prog} on PATH"));
    fs::canonicalize(p).unwrap()
}

impl Store {
    /// `None` without `nix-daemon` on `PATH`, unless `IETS_TEST_E2E` is set.
    /// `opts` go into nix.conf.
    pub fn start_with(opts: &[(&str, &str)]) -> Option<Store> {
        let Some(daemon) = which("nix-daemon") else {
            assert!(
                env::var_os("IETS_TEST_E2E").is_none(),
                "IETS_TEST_E2E is set, and no nix-daemon is on PATH"
            );
            return None;
        };
        // a socket path has to fit in sun_path, and darwin's TMPDIR is long
        let dir = tempfile::Builder::new()
            .prefix("iets-e2e.")
            .tempdir_in("/tmp")
            .expect("tempdir");
        let d = dir.path();
        for sub in ["store", "var", "log", "conf"] {
            fs::create_dir(d.join(sub)).unwrap();
        }
        // an empty registry: nix develop's nixpkgs#bashInteractive fails fast
        // instead of downloading the global one
        fs::write(d.join("conf/registry.json"), r#"{"flakes":[],"version":2}"#).unwrap();
        let mut conf = format!(
            "{CONF}flake-registry = {}\n",
            d.join("conf/registry.json").display()
        );
        for (k, v) in opts {
            conf.push_str(&format!("{k} = {v}\n"));
        }
        fs::write(d.join("conf/nix.conf"), conf).unwrap();
        let mut cmd = Command::new(daemon);
        Store::env(d, &mut cmd);
        let log = fs::File::create(d.join("daemon.log")).unwrap();
        cmd.args(["--store", "local"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            // the per-connection children outlive the parent, so `Drop`
            // kills the whole group
            .process_group(0);
        let daemon = cmd.spawn().expect("spawn nix-daemon");
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string();
        let mut store = Store {
            dir,
            daemon,
            shell: resolved("bash"),
            nonce,
            bin: hermetic::bin(),
        };
        store.write_pkgs();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !store.socket().exists() {
            let exited = store.daemon.try_wait().unwrap().is_some();
            assert!(
                !exited && Instant::now() < deadline,
                "no daemon socket: {}",
                store.daemon_log()
            );
            thread::sleep(Duration::from_millis(20));
        }
        Some(store)
    }

    fn env(d: &Path, c: &mut Command) {
        hermetic::isolate(c);
        c.env("NIX_STORE_DIR", d.join("store"))
            .env("NIX_STATE_DIR", d.join("var"))
            .env("NIX_LOG_DIR", d.join("log"))
            .env("NIX_DAEMON_SOCKET_PATH", d.join("socket"))
            .env("NIX_CONF_DIR", d.join("conf"))
            .env("NIX_USER_CONF_FILES", "")
            .env("XDG_CONFIG_HOME", d.join("xdg"))
            .env("XDG_CONFIG_DIRS", d.join("xdg"))
            .env("XDG_CACHE_HOME", d.join("cache"))
            .env("HOME", d);
    }

    /// `tests/e2e/pkgs`, with an `env.nix` that names this machine's bash and
    /// coreutils and a nonce, so every `Store` builds fresh paths.
    fn write_pkgs(&self) {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e/pkgs");
        let dst = self.pkgs();
        fs::create_dir(&dst).unwrap();
        for e in fs::read_dir(src).unwrap() {
            let e = e.unwrap();
            fs::copy(e.path(), dst.join(e.file_name())).unwrap();
        }
        let coreutils = resolved("ls");
        let env =
            r#"{ shell = "@SHELL@"; path = "@PATH@"; system = "@SYSTEM@"; nonce = "@NONCE@"; }"#;
        let path = coreutils.parent().unwrap().to_str().unwrap();
        self.write("pkgs/env.nix", &env.replace("@PATH@", path));
    }

    /// The stdout of `nix-store args`, which must succeed.
    pub fn nix_store(&self, args: &[&str]) -> String {
        stdout(&ok(self.nix("nix-store").args(args)))
    }

    /// An empty dir for PATH and IETS_NIX_BIN_DIR: no nix to fall back on.
    pub fn no_nix(&self) -> PathBuf {
        let d = self.dir().join("nobin");
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Writes `template` to `name` in the store's dir, with `@SYSTEM@`,
    /// `@SHELL@` (an absolute bash) and `@NONCE@` filled in.
    pub fn write(&self, name: &str, template: &str) -> PathBuf {
        let text = template
            .replace("@SYSTEM@", &system())
            .replace("@SHELL@", self.shell.to_str().unwrap())
            .replace("@NONCE@", &self.nonce);
        let p = self.dir().join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub fn store_dir(&self) -> PathBuf {
        self.dir().join("store")
    }

    pub fn socket(&self) -> PathBuf {
        self.dir().join("socket")
    }

    /// The copy of `tests/e2e/pkgs` this store builds from.
    pub fn pkgs(&self) -> PathBuf {
        self.dir().join("pkgs")
    }

    pub fn daemon_pid(&self) -> u32 {
        self.daemon.id()
    }

    pub fn daemon_log(&self) -> String {
        fs::read_to_string(self.dir().join("daemon.log")).unwrap_or_default()
    }

    /// iets, talking to this daemon only.
    pub fn iets(&self) -> Command {
        self.iets_as(env!("CARGO_BIN_EXE_iets"))
    }

    /// iets started as `prog`, a link to it under another name.
    pub fn iets_as(&self, prog: impl AsRef<OsStr>) -> Command {
        let mut c = self.bin.cmd_as(prog);
        Store::env(self.dir(), &mut c);
        c.current_dir(self.dir());
        c
    }

    /// A Nix tool, talking to this daemon only.
    pub fn nix(&self, prog: &str) -> Command {
        let mut c = Command::new(prog);
        Store::env(self.dir(), &mut c);
        // with a writable state dir Nix would open the store itself
        c.env("NIX_REMOTE", "daemon").current_dir(self.dir());
        c
    }
}

/// A second daemon on the same store dir, with its files under `root`, for
/// the `--builder` of a test. Its store is relocated, so Nix builds in the
/// sandbox there, with the host's `/nix/store` for bash and coreutils.
/// Chroot stores are Linux only.
pub struct Builder {
    root: PathBuf,
    store: String,
    daemon: Child,
}

/// A chroot store takes `/nix/store` unless `store=` names the store dir.
fn builder_store(root: &Path, store_dir: &Path) -> String {
    format!(
        "local?root={}&store={}",
        root.display(),
        store_dir.display()
    )
}

impl Builder {
    pub fn socket(&self) -> PathBuf {
        self.root.join("socket")
    }

    pub fn url(&self) -> String {
        format!("unix://{}", self.socket().display())
    }

    /// `local?root=…`, for `--store` on the Nix side.
    pub fn store(&self) -> String {
        self.store.clone()
    }

    /// Where store path `path` lies on disk: under `root/nix/store`, the
    /// default of Nix's `real` setting for a chroot store.
    pub fn real(&self, path: &str) -> PathBuf {
        self.root
            .join("nix/store")
            .join(path.rsplit('/').next().unwrap())
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        // SAFETY: kill only sends a signal
        unsafe { libc::kill(-(self.daemon.id() as i32), libc::SIGKILL) };
        let _ = self.daemon.wait();
    }
}

impl Store {
    /// A [`Builder`] named `name`, with `opts` as `--option`s.
    pub fn builder(&self, name: &str, opts: &[(&str, &str)]) -> Builder {
        let root = self.dir().join(name);
        fs::create_dir(&root).unwrap();
        let mut cmd = Command::new(which("nix-daemon").unwrap());
        Store::env(self.dir(), &mut cmd);
        let log = fs::File::create(root.join("daemon.log")).unwrap();
        cmd.env("NIX_DAEMON_SOCKET_PATH", root.join("socket"))
            .arg("--store")
            .arg(builder_store(&root, &self.store_dir()))
            .args(["--option", "sandbox-paths", "/nix/store"]);
        for (k, v) in opts {
            cmd.args(["--option", k, v]);
        }
        cmd.stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .process_group(0);
        let mut b = Builder {
            daemon: cmd.spawn().expect("spawn nix-daemon"),
            store: builder_store(&root, &self.store_dir()),
            root,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !b.socket().exists() {
            let exited = b.daemon.try_wait().unwrap().is_some();
            assert!(
                !exited && Instant::now() < deadline,
                "no builder socket: {}",
                fs::read_to_string(b.root.join("daemon.log")).unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(20));
        }
        b
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // SAFETY: kill only sends a signal
        unsafe { libc::kill(-(self.daemon.id() as i32), libc::SIGKILL) };
        let _ = self.daemon.wait();
        // store paths are read-only, and TempDir would leave them behind
        writable(self.dir());
    }
}

fn writable(dir: &Path) {
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            writable(&e.path());
        }
    }
}

/// The system the tree also names, which nothing builds.
pub const OTHER: &str = "riscv64-none";

const CHECKS_FLAKE: &str = r#"{
  outputs = { self }:
    let
      systems = [ "@SYSTEM@" "@OTHER@" ];
      forAll = f: builtins.listToAttrs (map (s: { name = s; value = f s; }) systems);
      drv = system: name: args: derivation ({
        inherit name system;
        builder = "@SHELL@";
      } // args);
      nonce = "@NONCE@";
    in {
      checks = forAll (system: rec {
        ok = drv system "fb-ok" {
          args = [ "-c" "echo building fb-ok ${nonce}; echo done > $out" ];
        };
        dep = drv system "fb-dep" {
          args = [ "-c" "echo building fb-dep ${nonce}; echo dep > $out" ];
        };
        chain = drv system "fb-chain" {
          args = [ "-c" "echo building fb-chain ${nonce}; while read -r l; do echo \"$l\"; done < ${dep} > $out" ];
        };
        bad = drv system "fb-bad" {
          args = [ "-c" "echo about to fail ${nonce}; echo line2; exit 1" ];
        };
      });
      packages = forAll (system: {
        lone = drv system "fb-lone" {
          args = [ "-c" "echo building fb-lone ${nonce}; echo lone > $out" ];
        };
      });
    };
}
"#;

/// The checks flake in `dir`: `ok`, `dep`, `chain` (which reads `dep`) and
/// `bad` (which exits 1) for two systems, and a `packages` set. It has its
/// own nonce, and a lock written here: iets only evaluates locked flakes,
/// and `nix flake lock` writes nothing for one without inputs.
pub fn checks_flake(s: &Store, dir: &str) -> String {
    let text = CHECKS_FLAKE
        .replace("@OTHER@", OTHER)
        .replace("@NONCE@", &format!("@NONCE@-{dir}"));
    s.write(&format!("{dir}/flake.nix"), &text);
    s.write(
        &format!("{dir}/flake.lock"),
        r#"{"nodes":{"root":{}},"root":"root","version":7}"#,
    );
    format!("path:{}", s.dir().join(dir).display())
}

/// Nix's `system` for this host.
pub fn system() -> String {
    let os = match env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{}-{os}", env::consts::ARCH)
}

/// Runs `c`, and panics with its stderr unless it succeeds.
pub fn ok(c: &mut Command) -> Output {
    let o = c.output().expect("spawn");
    assert!(
        o.status.success(),
        "{c:?} failed with {}:\n{}",
        o.status,
        String::from_utf8_lossy(&o.stderr)
    );
    o
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The `--log-format internal-json` records in `stderr`.
pub fn records(stderr: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter_map(|l| l.strip_prefix("@nix "))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// git in `repo`, with no config of the machine's.
pub fn git(repo: &Path, args: &[&str]) -> Output {
    let mut c = Command::new("git");
    c.args(["-c", "user.name=iets", "-c", "user.email=iets@example.com"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    ok(&mut c)
}

/// A commit of what is staged in `repo`, made at `date`.
pub fn git_commit(repo: &Path, msg: &str, date: &str) {
    let mut c = Command::new("git");
    c.args(["-c", "user.name=iets", "-c", "user.email=iets@example.com"])
        .args(["-c", "commit.gpgsign=false", "-C"])
        .arg(repo)
        .args(["commit", "-q", "-m", msg])
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    ok(&mut c);
}

/// The sorted names in `dir`, none if it is missing.
pub fn ls(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Collects failed verdicts, so one run reports every failing case.
#[derive(Default)]
pub struct Checks(Vec<String>);

impl Checks {
    pub fn check(&mut self, label: &str, ok: bool, detail: impl Display) -> bool {
        if !ok {
            self.0.push(format!("{label}: {detail}"));
        }
        ok
    }

    pub fn fail(&mut self, what: String) {
        self.0.push(what);
    }
}

impl Drop for Checks {
    fn drop(&mut self) {
        if !thread::panicking() && !self.0.is_empty() {
            panic!("{} failed:\n{}", self.0.len(), self.0.join("\n\n"));
        }
    }
}

/// A derivation that copies `DIR/src/a` and `DIR/src/sub/b`, at
/// `DIR/case.nix`.
pub fn make_case(s: &Store, dir: &str, name: &str) -> String {
    s.write(&format!("{dir}/src/a"), &format!("hello @NONCE@ {dir}\n"));
    s.write(&format!("{dir}/src/sub/b"), "zzz\n");
    let src = s.dir().join(dir).join("src");
    let text = r#"derivation {
  name = "@NAME@";
  system = "@SYSTEM@";
  builder = "@SHELL@";
  args = [ "-c" "while read -r l; do echo \"$l\"; done < ${@SRC@}/a > $out; while read -r l; do echo \"$l\"; done < ${@SRC@}/sub/b >> $out" ];
}
"#
    .replace("@NAME@", name)
    .replace("@SRC@", src.to_str().unwrap());
    s.write(&format!("{dir}/case.nix"), &text)
        .to_str()
        .unwrap()
        .to_string()
}

/// The stdout of `o` as JSON.
pub fn parse(o: &Output) -> serde_json::Value {
    serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}\n{}", stdout(o), stderr(o)))
}

/// The stdout of `c` as JSON. `c` must succeed.
pub fn json(c: &mut Command) -> serde_json::Value {
    parse(&ok(c))
}

/// The exit code of `c`.
pub fn code(c: &mut Command) -> i32 {
    c.output().expect("spawn").status.code().unwrap_or(-1)
}

/// `a | b`: `b` reads the stdout of `a`, which must succeed.
pub fn pipe(a: &mut Command, b: &mut Command) -> Output {
    let first = ok(a);
    let mut child = b
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&first.stdout)
        .unwrap();
    child.wait_with_output().unwrap()
}

/// A Store, or an early return from a test that has no `nix-daemon`.
#[macro_export]
macro_rules! store {
    ($($opt:expr),* $(,)?) => {
        match e2e::Store::start_with(&[$($opt),*]) {
            Some(s) => s,
            None => {
                eprintln!("skipped: no nix-daemon on PATH");
                return;
            }
        }
    };
}
