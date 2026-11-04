//! `builtins.fetchClosure` from a `file://` binary cache made here, with the
//! messages of Nix 2.34.8.

mod hermetic;

use hermetic::{Iets, bin, stderr, stdout};
use iets::store::hash::{HashAlgo, base64_encode, hash_bytes};
use iets::store::narinfo::NarInfo;
use iets::store::path::{IngestionMethod, make_fixed_output_path};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use rstest::rstest;
use std::collections::BTreeSet;
use std::fs;
use std::process::Output;
use tempfile::{TempDir, tempdir};

const ROOT: &str = "00000000000000000000000000000000-ia-root";
const UNSIGNED: &str = "11111111111111111111111111111111-ia-unsigned";

/// A cache with an input-addressed root signed by `test-1`, which refers to
/// a content-addressed path, and an unsigned input-addressed path.
struct Cache {
    bin: Iets,
    dir: TempDir,
    key: String,
    ca: String,
}

fn narinfo(base: &str, refs: &[&str], ca: Option<&str>, signer: Option<&Ed25519KeyPair>) -> String {
    let h = hash_bytes(HashAlgo::Sha256, base.as_bytes());
    let mut text = format!(
        "StorePath: /nix/store/{base}\nURL: nar/{}.nar\nCompression: none\nNarHash: sha256:{}\nNarSize: 8\nReferences: {}\n",
        h.to_nix32(),
        h.to_nix32(),
        refs.join(" ")
    );
    if let Some(ca) = ca {
        text.push_str(&format!("CA: {ca}\n"));
    }
    if let Some(k) = signer {
        let fp = NarInfo::parse(&text, base).unwrap().fingerprint();
        let sig = base64_encode(k.sign(fp.as_bytes()).as_ref());
        text.push_str(&format!("Sig: test-1:{sig}\n"));
    }
    text
}

fn cache() -> Cache {
    let dir = tempdir().unwrap();
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let key = format!("test-1:{}", base64_encode(pair.public_key().as_ref()));

    let h = hash_bytes(HashAlgo::Sha256, b"ca-dep");
    let ca =
        make_fixed_output_path(IngestionMethod::Recursive, &h, "ca-dep", &BTreeSet::new()).unwrap();
    let ca_info = format!(
        "StorePath: {}\nURL: nar/x.nar\nNarHash: sha256:{}\nNarSize: 8\nReferences: \nCA: fixed:r:sha256:{}\n",
        ca.to_full_string(),
        h.to_nix32(),
        h.to_nix32()
    );
    let write = |base: &str, text: &str| {
        fs::write(dir.path().join(format!("{}.narinfo", &base[..32])), text).unwrap();
    };
    write(ca.base_name(), &ca_info);
    write(
        ROOT,
        &narinfo(ROOT, &[ROOT, ca.base_name()], None, Some(&pair)),
    );
    write(UNSIGNED, &narinfo(UNSIGNED, &[ca.base_name()], None, None));
    Cache {
        bin: bin(),
        dir,
        key,
        ca: ca.to_full_string(),
    }
}

impl Cache {
    fn eval(&self, keys: &str, in_test: bool, args: &str) -> Output {
        let url = format!("file://{}", self.dir.path().display());
        let expr = format!(r#"builtins.fetchClosure {{ fromStore = "{url}"; {args} }}"#);
        let mut c = self.bin.cmd();
        c.args(["eval", "--raw", "--option", "trusted-public-keys", keys])
            .args(["--expr", &expr])
            .env("IETS_CACHE", "0");
        if in_test {
            c.env("_NIX_IN_TEST", "1");
        }
        c.output().expect("run iets")
    }
}

#[rstest]
#[cfg_attr(windows, ignore = "the store paths are Unix paths")]
fn a_signed_input_addressed_closure_is_fetched() {
    let c = cache();
    let args = format!("fromPath = /nix/store/{ROOT}; inputAddressed = true;");
    let o = c.eval(&c.key, true, &args);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("/nix/store/{ROOT}"));

    let o = c.eval(
        "other-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=",
        true,
        &args,
    );
    assert!(
        stderr(&o).contains(&format!(
            "cannot add path '/nix/store/{ROOT}' because it lacks a signature by a trusted key"
        )),
        "{}",
        stderr(&o)
    );

    let o = c.eval(&c.key, true, &format!("fromPath = /nix/store/{ROOT};"));
    assert!(
        stderr(&o).contains(&format!(
            "The 'fromPath' value '/nix/store/{ROOT}' is input-addressed, but 'inputAddressed' is set to 'false' (default)."
        )),
        "{}",
        stderr(&o)
    );

    let o = c.eval(&c.key, false, &args);
    assert!(
        stderr(&o).contains("'fetchClosure' only supports http:// and https:// stores"),
        "{}",
        stderr(&o)
    );
}

#[rstest]
#[cfg_attr(windows, ignore = "the store paths are Unix paths")]
fn a_content_addressed_path_needs_no_signature() {
    let c = cache();
    let o = c.eval("", true, &format!("fromPath = {};", c.ca));
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), c.ca);

    let o = c.eval(
        "",
        true,
        &format!("fromPath = {}; inputAddressed = true;", c.ca),
    );
    assert!(
        stderr(&o).contains("is not input-addressed, but 'inputAddressed' is set to 'true'"),
        "{}",
        stderr(&o)
    );

    let args = format!("fromPath = /nix/store/{UNSIGNED}; inputAddressed = true;");
    let o = c.eval(&c.key, true, &args);
    assert!(
        stderr(&o).contains("lacks a signature by a trusted key"),
        "{}",
        stderr(&o)
    );
}
