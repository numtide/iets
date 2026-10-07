//! The ATerm code against data it was not written from: Nix 2.34.8's own
//! libstore unit-test data (`src/libstore-tests/data/derivation`, copied to
//! `tests/fixtures/aterm` and `INVARIANTS` below), and the hand-written cases
//! harmonia (github.com/nix-community/harmonia, MIT) checks its ATerm code with.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use iets::store::drv::{
    Derivation, DerivationOutput, DrvError, DrvHashCache, DrvHashKind, InputNode,
    finalize_input_addressed_outputs, hash_derivation_modulo,
};
use iets::store::hash::{Hash, HashAlgo};
use iets::store::path::{IngestionMethod, StorePath};

fn fixture(file: &str) -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/aterm")
        .join(file);
    fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn sp(base: &str) -> StorePath {
    StorePath::from_base_name(base).unwrap()
}

fn path_of(drv: &Derivation) -> StorePath {
    drv.drv_path_for(&drv.unparse().unwrap()).unwrap()
}

fn no_loader(p: &StorePath) -> Result<Derivation, DrvError> {
    panic!("no input derivation to load, asked for {p}")
}

/// The name is what the `.json` next to each upstream `.drv` gives.
const UPSTREAM: &[(&str, &str)] = &[
    ("simple-derivation.drv", "simple-derivation"),
    ("dyn-dep-derivation.drv", "dyn-dep-derivation"),
    ("ca-advanced-attributes.drv", "advanced-attributes"),
    (
        "ca-advanced-attributes-defaults.drv",
        "advanced-attributes-defaults",
    ),
    (
        "ca-advanced-attributes-structured-attrs.drv",
        "advanced-attributes-structured-attrs",
    ),
    (
        "ca-advanced-attributes-structured-attrs-defaults.drv",
        "advanced-attributes-structured-attrs-defaults",
    ),
    ("ia-advanced-attributes.drv", "advanced-attributes"),
    (
        "ia-advanced-attributes-defaults.drv",
        "advanced-attributes-defaults",
    ),
    (
        "ia-advanced-attributes-structured-attrs.drv",
        "advanced-attributes-structured-attrs",
    ),
    (
        "ia-advanced-attributes-structured-attrs-defaults.drv",
        "advanced-attributes-structured-attrs-defaults",
    ),
];

#[test]
fn upstream_drvs_roundtrip_byte_exact() {
    for (file, name) in UPSTREAM {
        let text = fixture(file);
        let drv = Derivation::parse(&text, name).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(drv.unparse().unwrap(), text, "{file}");
    }
}

#[test]
fn upstream_bad_drvs_are_rejected() {
    for file in ["bad-version.drv", "bad-old-version-dyn-deps.drv"] {
        assert!(Derivation::parse(&fixture(file), "x").is_err(), "{file}");
    }
}

/// Nix's parser benchmark data. Its hashes were blanked to `aaaa…`, which left
/// the input derivations out of the order Nix prints, so it cannot roundtrip
/// byte for byte; printing what was parsed must still be a fixpoint.
#[test]
fn upstream_bench_drvs_parse_and_reprint_stably() {
    for (file, name) in [("hello.drv", "hello-2.12.2"), ("firefox.drv", "firefox")] {
        let drv = Derivation::parse(&fixture(file), name).unwrap_or_else(|e| panic!("{file}: {e}"));
        let printed = drv.unparse().unwrap();
        let again = Derivation::parse(&printed, name).unwrap();
        assert_eq!(again, drv, "{file}");
        assert_eq!(again.unparse().unwrap(), printed, "{file}");
    }
}

/// The input-addressed fixtures with no input derivations: their output paths
/// are recomputed from nothing and must be the ones Nix wrote.
#[test]
fn upstream_input_addressed_paths_recompute() {
    for (file, name) in [
        (
            "ia-advanced-attributes-defaults.drv",
            "advanced-attributes-defaults",
        ),
        (
            "ia-advanced-attributes-structured-attrs-defaults.drv",
            "advanced-attributes-structured-attrs-defaults",
        ),
    ] {
        let original = Derivation::parse(&fixture(file), name).unwrap();
        assert!(original.input_drvs.is_empty(), "{file}");
        let mut drv = original.clone();
        for (out, o) in drv.outputs.iter_mut() {
            *o = DerivationOutput::Deferred;
            if let Some(v) = drv.env.iter_mut().find(|(k, _)| k == out) {
                v.1.clear();
            }
        }
        finalize_input_addressed_outputs(&mut drv, &mut DrvHashCache::new(), &mut no_loader)
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(drv, original, "{file}");
    }
}

fn simple(name: &str, outputs: &[(&str, DerivationOutput)], env: &[(&str, &str)]) -> Derivation {
    let mut env: Vec<(String, Vec<u8>)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
        .collect();
    env.sort();
    Derivation {
        name: name.into(),
        outputs: outputs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        env,
        ..Default::default()
    }
}

/// libstore-tests/derivation/invariants.cc: `fillInOutputPaths` on a deferred
/// output, with the paths Nix computed in `data/derivation/invariants/*-post.json`.
#[test]
fn upstream_invariants_fill_in_deferred() {
    for (name, doc, env_out, want) in [
        (
            "filled-in-deferred-empty-env-var",
            "Fill in deferred output with empty env var",
            Some(""),
            "bilpz1nq8qi9r3bzsp72n34yjgqg43ws-filled-in-deferred-empty-env-var",
        ),
        (
            "filled-in-deferred-no-env-var",
            "Fill in deferred with missing env var",
            None,
            "wpk9qrgg77fyswhailap0gicgw98izx9-filled-in-deferred-no-env-var",
        ),
    ] {
        let mut env = vec![("__doc", doc)];
        env.extend(env_out.map(|v| ("out", v)));
        let mut drv = simple(name, &[("out", DerivationOutput::Deferred)], &env);
        finalize_input_addressed_outputs(&mut drv, &mut DrvHashCache::new(), &mut no_loader)
            .unwrap();
        assert_eq!(
            drv.outputs["out"],
            DerivationOutput::InputAddressed(sp(want)),
            "{name}"
        );
        assert_eq!(
            drv.env_str("out"),
            Some(format!("/nix/store/{want}").as_str()),
            "{name}"
        );
    }
}

#[test]
fn upstream_invariants_input_addressed_kept_or_rejected() {
    let ok = "w4bk7hpyxzgy2gx8fsa8f952435pll3i-filled-in-already";
    let mut drv = simple(
        "filled-in-already",
        &[("out", DerivationOutput::InputAddressed(sp(ok)))],
        &[
            ("__doc", "Correct path stays unchanged"),
            ("out", &format!("/nix/store/{ok}")),
        ],
    );
    let before = drv.clone();
    finalize_input_addressed_outputs(&mut drv, &mut DrvHashCache::new(), &mut no_loader).unwrap();
    assert_eq!(drv, before);

    let wrong = "c015dhfh5l0lp6wxyvdn7bmwhbbr6hr9-wrong-name";
    let mut drv = simple(
        "bad-path",
        &[("out", DerivationOutput::InputAddressed(sp(wrong)))],
        &[
            ("__doc", "Wrong InputAddressed path throws error"),
            ("out", &format!("/nix/store/{wrong}")),
        ],
    );
    assert!(
        finalize_input_addressed_outputs(&mut drv, &mut DrvHashCache::new(), &mut no_loader)
            .is_err()
    );
}

/// invariants.cc:`makeCAFloatingDependency("dependency")`, which Nix writes to
/// `lg4c4b8r9hlczwprl6kgnzfd9mc1xmkk-dependency.drv`.
fn ca_floating_dependency() -> Derivation {
    simple(
        "dependency",
        &[(
            "out",
            DerivationOutput::CaFloating {
                method: IngestionMethod::Recursive,
                algo: HashAlgo::Sha256,
            },
        )],
        &[("out", "")],
    )
}

#[test]
fn upstream_invariants_ca_floating_dependency() {
    let dep = ca_floating_dependency();
    let dep_path = sp("lg4c4b8r9hlczwprl6kgnzfd9mc1xmkk-dependency.drv");
    assert_eq!(path_of(&dep), dep_path);

    let mut loader = |p: &StorePath| {
        assert_eq!(*p, dep_path);
        Ok(ca_floating_dependency())
    };
    let with_dep = |out: DerivationOutput, doc: &str| {
        let mut drv = simple(
            "depends-on-drv",
            &[("out", out)],
            &[("__doc", doc), ("out", "")],
        );
        drv.input_drvs.insert(
            dep_path.clone(),
            InputNode::of(BTreeSet::from(["out".to_string()])),
        );
        drv
    };

    let mut drv = with_dep(
        DerivationOutput::Deferred,
        "Deferred stays deferred with CA dependencies",
    );
    let before = drv.clone();
    finalize_input_addressed_outputs(&mut drv, &mut DrvHashCache::new(), &mut loader).unwrap();
    assert_eq!(drv, before);

    let mut drv = with_dep(
        DerivationOutput::InputAddressed(sp("c015dhfh5l0lp6wxyvdn7bmwhbbr6hr9-wrong-name")),
        "InputAddressed throws when should be deferred",
    );
    assert!(
        finalize_input_addressed_outputs(&mut drv, &mut DrvHashCache::new(), &mut loader).is_err()
    );
}

/// harmonia-store-aterm/tests/roundtrip.rs.
#[test]
fn harmonia_cases_roundtrip() {
    for (input, name) in [
        (
            r#"Derive([("out","","","")],[],[],"x86_64-linux","/bin/sh",[],[("name","test")])"#,
            "test",
        ),
        (
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[("name","test")])"#,
            "test",
        ),
        (
            r#"Derive([("out","","","")],[],[],"x86_64-linux","/bin/sh",[],[("msg","hello\nworld\t\"test\"\\end"),("name","test")])"#,
            "test",
        ),
        (
            r#"Derive([("info","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gnused-4.9-info","",""),("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gnused-4.9","","")],[("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bootstrap-tools.drv",["out"]),("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-stdenv-linux.drv",["out"])],["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-default-builder.sh"],"x86_64-linux","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bootstrap-tools/bin/bash",["-e","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-stdenv.sh"],[("name","gnused-4.9"),("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gnused-4.9"),("system","x86_64-linux")])"#,
            "gnused-4.9",
        ),
        (
            r#"Derive([("out","/nix/store/6fr8dalasgpy0bpykhjq2b9q65lb4j8y-linux-6.16.tar.xz","sha256","1a4be2fe6b5246aa4ac8987a8a4af34c42a8dd7d08b46ab48516bcc1befbcd83")],[],[],"builtin","builtin:fetchurl",[],[("name","linux-6.16.tar.xz")])"#,
            "linux-6.16.tar.xz",
        ),
    ] {
        let drv =
            Derivation::parse(input.as_bytes(), name).unwrap_or_else(|e| panic!("{input}: {e}"));
        assert_eq!(drv.unparse().unwrap(), input.as_bytes());
    }
}

/// derivations.cc:parseDerivationOutput only checks that a fixed output's path
/// starts with `/`; unparse prints the path the content address gives.
#[test]
fn fixed_output_path_is_recomputed_not_trusted() {
    let wrong = r#"Derive([("out","/nix/store/00000000000000000000000000000000-linux-6.16.tar.xz","sha256","1a4be2fe6b5246aa4ac8987a8a4af34c42a8dd7d08b46ab48516bcc1befbcd83")],[],[],"builtin","builtin:fetchurl",[],[("name","linux-6.16.tar.xz")])"#;
    let drv = Derivation::parse(wrong.as_bytes(), "linux-6.16.tar.xz").unwrap();
    assert_eq!(
        drv.unparse().unwrap(),
        wrong
            .replace(
                "00000000000000000000000000000000",
                "6fr8dalasgpy0bpykhjq2b9q65lb4j8y"
            )
            .into_bytes()
    );
}

/// harmonia-store-aterm/src/parser.rs and raw_output.rs: errors, not panics.
#[test]
fn malformed_aterm_is_an_error() {
    for input in [
        "",
        "Derive([",
        "Derive(x",
        r#"Derive([("out","","r:sha256","abc")],[],[],"x86_64-linux","/bin/sh",[],[])"#,
        r#"Derive([("out","/nix/store/00000000000000000000000000000000-x","sha256","abc")],[],[],"x86_64-linux","/bin/sh",[],[])"#,
    ] {
        assert!(Derivation::parse(input.as_bytes(), "x").is_err(), "{input}");
    }
}

fn fetch(url: &str) -> Derivation {
    let hash = Hash::parse_non_sri_unprefixed(
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        HashAlgo::Sha256,
    )
    .unwrap();
    let mut drv = simple(
        "dep-1.0",
        &[(
            "out",
            DerivationOutput::Fixed {
                method: IngestionMethod::Flat,
                hash,
            },
        )],
        &[("url", url)],
    );
    drv.builder = "builtin:fetchurl".into();
    drv
}

fn on(deps: &[&StorePath]) -> Derivation {
    let mut drv = simple(
        "app-1.0",
        &[("out", DerivationOutput::Deferred)],
        &[("out", "")],
    );
    for d in deps {
        drv.input_drvs.insert(
            (*d).clone(),
            InputNode::of(BTreeSet::from(["out".to_string()])),
        );
    }
    drv
}

/// harmonia-store-aterm/src/hash_modulo.rs `fixed_output_provenance_hidden`:
/// the url of a fixed-output input does not reach the output path downstream.
/// Depending on both copies is the same as depending on one, because their
/// modulo hashes collide and hashDerivationModulo merges the output sets.
#[test]
fn fixed_output_inputs_hide_their_provenance() {
    let (a, b) = (fetch("https://a.example/t"), fetch("https://b.example/t"));
    let (pa, pb) = (path_of(&a), path_of(&b));
    assert_ne!(pa, pb);
    let drvs: BTreeMap<StorePath, Derivation> = [(pa.clone(), a), (pb.clone(), b)].into();
    let mut loader = |p: &StorePath| Ok(drvs[p].clone());
    let out_path =
        |mut drv: Derivation,
         loader: &mut dyn FnMut(&StorePath) -> Result<Derivation, DrvError>| {
            finalize_input_addressed_outputs(&mut drv, &mut DrvHashCache::new(), loader).unwrap();
            drv.outputs["out"].clone()
        };
    let via_a = out_path(on(&[&pa]), &mut loader);
    assert_eq!(out_path(on(&[&pb]), &mut loader), via_a);
    assert_eq!(out_path(on(&[&pa, &pb]), &mut loader), via_a);
}

/// harmonia-store-aterm/src/hash_modulo.rs `floating_ca_is_deferred` and
/// `deferred_input_defers`.
#[test]
fn floating_ca_defers_itself_and_downstream() {
    let dep = ca_floating_dependency();
    let h = hash_derivation_modulo(&dep, false, &mut DrvHashCache::new(), &mut no_loader).unwrap();
    assert_eq!(h.kind, DrvHashKind::Deferred);

    let dep_path = path_of(&dep);
    let mut loader = |_: &StorePath| Ok(ca_floating_dependency());
    let h = hash_derivation_modulo(
        &on(&[&dep_path]),
        true,
        &mut DrvHashCache::new(),
        &mut loader,
    )
    .unwrap();
    assert_eq!(h.kind, DrvHashKind::Deferred);
}

/// harmonia-store-aterm/src/hash_modulo.rs `identity_and_masked_hashes_differ`.
#[test]
fn masked_and_unmasked_hashes_differ_once_filled() {
    let mut drv = on(&[]);
    finalize_input_addressed_outputs(&mut drv, &mut DrvHashCache::new(), &mut no_loader).unwrap();
    let masked =
        hash_derivation_modulo(&drv, true, &mut DrvHashCache::new(), &mut no_loader).unwrap();
    let identity =
        hash_derivation_modulo(&drv, false, &mut DrvHashCache::new(), &mut no_loader).unwrap();
    assert_ne!(masked.hashes["out"], identity.hashes["out"]);
}

/// harmonia-store-aterm/tests/roundtrip.rs `roundtrip_non_utf8_bytes`: an ATerm
/// string is bytes, and an arg or an env value that is not UTF-8 survives.
#[test]
fn bytes_that_are_not_utf8_roundtrip() {
    let input: &[u8] = b"Derive([(\"out\",\"\",\"\",\"\")],[],[],\"x86_64-linux\",\"/bin/sh\",[\"a\xc8\x82b\"],[(\"name\",\"test\"),(\"v\",\"\xc8\")])";
    let drv = Derivation::parse(input, "test").unwrap();
    assert_eq!(drv.args, [b"a\xc8\x82b".to_vec()]);
    assert_eq!(drv.env_get("v"), Some(&b"\xc8"[..]));
    assert_eq!(drv.env_str("v"), None);
    assert_eq!(drv.unparse().unwrap(), input);
}
