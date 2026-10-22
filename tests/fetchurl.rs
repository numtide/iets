//! `builtins.fetchurl` of a `file://` URL, against the store paths and the
//! messages of `nix eval` (Nix 2.34.8) for the same file.

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use tempfile::{TempDir, tempdir};

const BYTES: &[u8] = b"hello fetchurl\n";
const SHA256: &str = "113a52l57f2jv9fh9igj2xh2mi0is40flxbkqi69wbh7nlybkcbm";

fn run(bin: &Iets, cache: &Path, offline: bool, args: &[&str]) -> Output {
    let mut c = bin.cmd();
    c.args(args)
        .env("IETS_CACHE_DIR", cache)
        .env("IETS_CACHE", "0");
    if offline {
        c.env("IETS_OFFLINE", "1");
    }
    c.output().expect("run iets")
}

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

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A scratch directory holding `hello.txt`, and a cache directory beside it.
fn scratch() -> (TempDir, PathBuf) {
    let d = tempdir().unwrap();
    fs::write(d.path().join("hello.txt"), BYTES).unwrap();
    let cache = d.path().join("cache");
    (d, cache)
}

fn eval(bin: &Iets, cache: &Path, offline: bool, flags: &[&str], expr: &str) -> Output {
    let mut args = vec!["eval", "--raw"];
    args.extend_from_slice(flags);
    args.extend_from_slice(&["--expr", expr]);
    run(bin, cache, offline, &args)
}

#[rstest]
fn a_hashed_fetch_has_the_store_path_nix_gives(bin: Iets) {
    let (d, cache) = scratch();
    let url = format!("{}/hello.txt", file_url(d.path()));
    let call = format!(r#"builtins.fetchurl {{ url = "{url}"; sha256 = "{SHA256}"; }}"#);

    let o = eval(&bin, &cache, false, &[], &call);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout),
        "/nix/store/3ay25cl2lpykmx07i7m22qxz7zwzqmvy-hello.txt"
    );

    // the input store holds it now: no download, and the bytes read back
    fs::remove_file(d.path().join("hello.txt")).unwrap();
    let o = eval(
        &bin,
        &cache,
        true,
        &[],
        &format!("builtins.readFile ({call})"),
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(o.stdout, BYTES);

    let named =
        format!(r#"builtins.fetchurl {{ url = "{url}"; sha256 = "{SHA256}"; name = "x"; }}"#);
    fs::write(d.path().join("hello.txt"), BYTES).unwrap();
    let o = eval(&bin, &cache, false, &[], &named);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout),
        "/nix/store/wi98cklj3li90ii41aggl9mf0w3nqp5y-x"
    );
}

#[rstest]
fn a_wrong_hash_is_refused_with_the_hash_the_file_has(bin: Iets) {
    let (d, cache) = scratch();
    let url = format!("{}/hello.txt", file_url(d.path()));
    let zero = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let o = eval(
        &bin,
        &cache,
        false,
        &[],
        &format!(r#"builtins.fetchurl {{ url = "{url}"; sha256 = "{zero}"; name = "y"; }}"#),
    );
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stdout));
    let want = format!(
        "error: hash mismatch in file downloaded from '{url}':\n  \
         specified: sha256:0000000000000000000000000000000000000000000000000000\n  \
         got:       sha256:{SHA256}\n"
    );
    assert!(text(&o.stderr).starts_with(&want), "{}", text(&o.stderr));
}

#[rstest]
fn without_a_hash_no_evaluation_fetches_and_prefetch_gives_the_hash(bin: Iets) {
    let (d, cache) = scratch();
    let url = format!("{}/hello.txt", file_url(d.path()));
    let call = format!(r#"builtins.fetchurl "{url}""#);
    let want = format!(
        "error: 'fetchurl' requires a 'sha256' argument: run 'iets prefetch {url}' and pass the hash as 'sha256'"
    );
    for flags in [&[][..], &["--impure"][..]] {
        let o = eval(&bin, &cache, false, flags, &call);
        assert_eq!(o.status.code(), Some(1), "{flags:?}: {}", text(&o.stdout));
        assert!(text(&o.stderr).starts_with(&want), "{}", text(&o.stderr));
    }

    let o = run(&bin, &cache, false, &["prefetch", &url]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let sri = text(&o.stdout).trim().to_string();
    let o = eval(
        &bin,
        &cache,
        false,
        &[],
        &format!(r#"builtins.fetchurl {{ url = "{url}"; sha256 = "{sri}"; }}"#),
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout),
        "/nix/store/3ay25cl2lpykmx07i7m22qxz7zwzqmvy-hello.txt"
    );
}
