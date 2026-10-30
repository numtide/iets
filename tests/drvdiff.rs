use std::sync::Arc;

use iets::api::render::Report;
use iets::cli::render::Text;
use iets::store::drv::Derivation;
use iets::store::drv::{DerivationOutput, DrvError, InputNode};
use iets::store::drvdiff::*;
use iets::store::hash::{HashAlgo, hash_bytes};
use iets::store::path::IngestionMethod;
use iets::store::path::StorePath;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// The loader a test uses: the derivations it made, by path.
#[derive(Default)]
struct Store(BTreeMap<StorePath, Arc<Derivation>>);

impl Store {
    fn add(&mut self, d: Derivation) -> StorePath {
        let p = d.drv_path().expect("drv path");
        self.0.insert(p.clone(), Arc::new(d));
        p
    }

    fn diff(&self, a: &StorePath, b: &StorePath, depth: usize) -> Diff {
        let mut load = |p: &StorePath| -> Result<Derivation, DrvError> { Ok((*self.0[p]).clone()) };
        diff(&mut load, a, b, depth).expect("diff")
    }
}

fn drv(name: &str, args: &[&str], env: &[(&str, &str)]) -> Derivation {
    let mut d = Derivation {
        name: name.to_string(),
        platform: "x86_64-linux".into(),
        builder: "/bin/sh".into(),
        args: args.iter().map(|s| s.as_bytes().to_vec()).collect(),
        ..Derivation::default()
    };
    d.outputs.insert("out".into(), DerivationOutput::Deferred);
    for (k, v) in env {
        d.env_set(k, v.as_bytes().to_vec());
    }
    d
}

/// The items of `d` as `what key a b`, for a readable assertion.
fn items(d: &Diff) -> Vec<String> {
    (d.items.iter())
        .map(|i| {
            let s = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
            format!("{}|{}|{}|{}", i.what, i.key, s(&i.a), s(&i.b))
        })
        .collect()
}

#[test]
fn the_same_derivation_is_empty() {
    let mut s = Store::default();
    let p = s.add(drv("foo", &["-c", "true"], &[("x", "1")]));
    let d = s.diff(&p, &p, 2);
    assert!(d.is_empty());
    assert_eq!(d.text(), format!("{p} and {p} are the same derivation\n"));
}

#[test]
fn the_local_fields_the_env_and_an_absent_value() {
    let mut s = Store::default();
    let a = s.add(drv("foo", &["-c", "true"], &[("x", "1"), ("gone", "")]));
    let mut second = drv("foo", &["-c", "false"], &[("x", "2"), ("new", "")]);
    second.platform = "aarch64-linux".into();
    let b = s.add(second);
    let d = s.diff(&a, &b, 2);
    assert_eq!(
        items(&d),
        [
            "platform||x86_64-linux|aarch64-linux",
            "args||\"-c\"\n\"true\"\n|\"-c\"\n\"false\"\n",
            "env|gone||-",
            "env|new|-|",
            "env|x|1|2",
        ]
    );
    // an empty value is a value, not an absent entry
    assert!(d.text().contains("env removed: gone=\n"));
    assert!(d.text().contains("env added: new=\n"));
    assert!(d.changed.is_empty());
}

/// Nix sets `name` in the environment as well, so `env name` repeats the
/// `name` item; it shows only when it says something else.
#[test]
fn env_name_shows_only_when_it_is_not_the_derivation_name() {
    let mut s = Store::default();
    let a = s.add(drv("ux-one", &[], &[("name", "ux-one")]));
    let b = s.add(drv("ux-two", &[], &[("name", "ux-two")]));
    assert_eq!(items(&s.diff(&a, &b, 2)), ["name||ux-one|ux-two"]);

    let a = s.add(drv("ux-one", &[], &[("name", "other")]));
    let b = s.add(drv("ux-two", &[], &[("name", "ux-two")]));
    assert_eq!(
        items(&s.diff(&a, &b, 2)),
        ["name||ux-one|ux-two", "env|name|other|ux-two"]
    );

    let a = s.add(drv("ux-one", &[], &[("name", "ux-one")]));
    let b = s.add(drv("ux-two", &[], &[]));
    assert_eq!(
        items(&s.diff(&a, &b, 2)),
        ["name||ux-one|ux-two", "env|name|ux-one|-"]
    );
}

#[test]
fn a_changed_input_drv_is_paired_by_name_and_recursed_into() {
    let mut s = Store::default();
    let (pa, pb) = (
        s.add(drv("dep", &["-c", "a"], &[])),
        s.add(drv("dep", &["-c", "b"], &[])),
    );
    let out = InputNode::of(BTreeSet::from(["out".to_string()]));
    let (mut ta, mut tb) = (drv("top", &[], &[]), drv("top", &[], &[]));
    ta.input_drvs.insert(pa, out.clone());
    tb.input_drvs.insert(pb, out);
    let (x, y) = (s.add(ta), s.add(tb));
    let d = s.diff(&x, &y, 2);
    assert!(d.items.is_empty(), "paired, not added or removed");
    assert_eq!(d.changed.len(), 1);
    assert_eq!(
        items(&d.changed[0]),
        ["args||\"-c\"\n\"a\"\n|\"-c\"\n\"b\"\n"]
    );
    let t = d.text();
    assert!(t.contains("input drv changed: dep"), "{t}");
    assert!(t.contains("+ \"b\""), "{t}");
}

#[test]
fn the_depth_limit_names_the_input_without_walking_it() {
    let mut s = Store::default();
    let out = InputNode::of(BTreeSet::from(["out".to_string()]));
    let (mut ta, mut tb) = (drv("top", &[], &[]), drv("top", &[], &[]));
    ta.input_drvs
        .insert(s.add(drv("dep", &["a"], &[])), out.clone());
    tb.input_drvs.insert(s.add(drv("dep", &["b"], &[])), out);
    let (x, y) = (s.add(ta), s.add(tb));
    let d = s.diff(&x, &y, 0);
    assert_eq!(d.changed.len(), 1);
    assert_eq!(d.changed[0].cut, Some("depth"));
    assert!(d.changed[0].items.is_empty());
    assert!(d.text().contains("(stopped: depth; use --all)"));
}

#[test]
fn an_input_source_an_added_input_drv_and_a_fixed_output_show_up() {
    let mut s = Store::default();
    let dep = s.add(drv("dep", &[], &[]));
    let (mut a, mut b) = (drv("top", &[], &[]), drv("top", &[], &[]));
    let src = |h: &str| StorePath::from_base_name(h).expect("store path");
    a.input_srcs
        .insert(src("00000000000000000000000000000000-a.txt"));
    b.input_srcs
        .insert(src("11111111111111111111111111111111-b.txt"));
    b.input_drvs
        .insert(dep, InputNode::of(BTreeSet::from(["out".to_string()])));
    let fix = |bytes: &[u8]| DerivationOutput::Fixed {
        method: IngestionMethod::Flat,
        hash: hash_bytes(HashAlgo::Sha256, bytes),
    };
    a.outputs.insert("out".into(), fix(b"one"));
    b.outputs.insert("out".into(), fix(b"two"));
    let (x, y) = (s.add(a), s.add(b));
    let d = s.diff(&x, &y, 2);
    let kinds: Vec<&str> = d.items.iter().map(|i| i.what).collect();
    assert_eq!(
        kinds,
        ["output", "input source", "input source", "input drv"]
    );
    assert_eq!(d.items[0].key, "out");
    assert!(d.items[1].b.is_none() && d.items[2].a.is_none());
    assert!(d.items[3].a.is_none(), "the input drv is added");
}

#[test]
fn json_names_what_differs() {
    let mut s = Store::default();
    let a = s.add(drv("foo", &[], &[("x", "1")]));
    let b = s.add(drv("foo", &[], &[("x", "2")]));
    let v = s.diff(&a, &b, 2).json();
    assert_eq!(
        v["changes"],
        serde_json::json!([{"what": "env", "key": "x", "a": "1", "b": "2"}])
    );
    assert!(v.get("inputDrvs").is_none() && v.get("cut").is_none());
}
