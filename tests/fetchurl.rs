//! `builtins.fetchurl` of a `file://` URL, against the store paths and the
//! messages of `nix eval` (Nix 2.34.8) for the same file.

mod hermetic;

use hermetic::{Iets, bin, stderr, stdout};
use rstest::{fixture, rstest};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::{TempDir, tempdir};

const BYTES: &[u8] = b"hello fetchurl\n";
const SHA256: &str = "113a52l57f2jv9fh9igj2xh2mi0is40flxbkqi69wbh7nlybkcbm";

/// The `file://` URL of a path. Windows drives come after a third slash and
/// backslashes would read as escapes in the Nix string that carries it.
fn file_url(p: &Path) -> String {
    let p = p.display().to_string().replace('\\', "/");
    if p.starts_with('/') {
        format!("file://{p}")
    } else {
        format!("file:///{p}")
    }
}

/// `hello.txt` in a directory of its own, its URL, and iets with a cache
/// beside it.
struct Scratch {
    bin: Iets,
    dir: TempDir,
    url: String,
}

#[fixture]
fn scratch(bin: Iets) -> Scratch {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("hello.txt"), BYTES).unwrap();
    let url = format!("{}/hello.txt", file_url(dir.path()));
    Scratch { bin, dir, url }
}

impl Scratch {
    fn hello(&self) -> PathBuf {
        self.dir.path().join("hello.txt")
    }

    fn run(&self, offline: bool, args: &[&str]) -> Output {
        let mut c = self.bin.cmd();
        c.args(args)
            .env("IETS_CACHE_DIR", self.dir.path().join("cache"))
            .env("IETS_CACHE", "0");
        if offline {
            c.env("IETS_OFFLINE", "1");
        }
        c.output().expect("run iets")
    }

    fn eval(&self, offline: bool, flags: &[&str], expr: &str) -> Output {
        let mut args = vec!["eval", "--raw"];
        args.extend_from_slice(flags);
        args.extend_from_slice(&["--expr", expr]);
        self.run(offline, &args)
    }
}

#[rstest]
fn a_hashed_fetch_has_the_store_path_nix_gives(scratch: Scratch) {
    let url = &scratch.url;
    let call = format!(r#"builtins.fetchurl {{ url = "{url}"; sha256 = "{SHA256}"; }}"#);

    let o = scratch.eval(false, &[], &call);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        "/nix/store/3ay25cl2lpykmx07i7m22qxz7zwzqmvy-hello.txt"
    );

    // the input store holds it now: no download, and the bytes read back
    fs::remove_file(scratch.hello()).unwrap();
    let o = scratch.eval(true, &[], &format!("builtins.readFile ({call})"));
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(o.stdout, BYTES);

    let named =
        format!(r#"builtins.fetchurl {{ url = "{url}"; sha256 = "{SHA256}"; name = "x"; }}"#);
    fs::write(scratch.hello(), BYTES).unwrap();
    let o = scratch.eval(false, &[], &named);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "/nix/store/wi98cklj3li90ii41aggl9mf0w3nqp5y-x");
}

#[rstest]
fn a_wrong_hash_is_refused_with_the_hash_the_file_has(scratch: Scratch) {
    let url = &scratch.url;
    let zero = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let o = scratch.eval(
        false,
        &[],
        &format!(r#"builtins.fetchurl {{ url = "{url}"; sha256 = "{zero}"; name = "y"; }}"#),
    );
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let want = format!(
        "error: hash mismatch in file downloaded from '{url}':\n  \
         specified: sha256:0000000000000000000000000000000000000000000000000000\n  \
         got:       sha256:{SHA256}\n"
    );
    assert!(stderr(&o).starts_with(&want), "{}", stderr(&o));
}

#[rstest]
fn without_a_hash_no_evaluation_fetches_and_prefetch_gives_the_hash(scratch: Scratch) {
    let url = &scratch.url;
    let call = format!(r#"builtins.fetchurl "{url}""#);
    let want = format!(
        "error: 'fetchurl' requires a 'sha256' argument: run 'iets prefetch {url}' and pass the hash as 'sha256'"
    );
    for (flags, code) in [(&[][..], 1), (&["--impure"][..], 3)] {
        let o = scratch.eval(false, flags, &call);
        assert_eq!(o.status.code(), Some(code), "{flags:?}: {}", stdout(&o));
        assert!(stderr(&o).starts_with(&want), "{}", stderr(&o));
    }

    let o = scratch.run(false, &["prefetch", url]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let sri = stdout(&o);
    let o = scratch.eval(
        false,
        &[],
        &format!(r#"builtins.fetchurl {{ url = "{url}"; sha256 = "{sri}"; }}"#),
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        "/nix/store/3ay25cl2lpykmx07i7m22qxz7zwzqmvy-hello.txt"
    );
}
