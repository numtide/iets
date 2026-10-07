//! What differs between two derivations, the way nix-diff explains a
//! rebuild: the local differences of a pair, then the input derivations
//! that differ, paired by name and recursed into (docs/reference/commands.md).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::drv::{DerivationOutput, DrvError, DrvLoader, InputNode};
use super::path::StorePath;

/// One local difference. `what` names the part, `key` the entry inside it
/// (empty for a part that has one value), and `a`/`b` the two sides;
/// `None` is a side that has not got it.
#[derive(Debug, PartialEq)]
pub struct Item {
    pub what: &'static str,
    pub key: String,
    pub a: Option<String>,
    pub b: Option<String>,
}

/// The difference between two derivations: what differs here, then the
/// input derivations that differ, paired by name.
#[derive(Default, Debug, PartialEq)]
pub struct Diff {
    pub a: String,
    pub b: String,
    pub items: Vec<Item>,
    pub changed: Vec<Diff>,
    /// Why the walk stopped here: "depth" or "shown above".
    pub cut: Option<&'static str>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.changed.is_empty()
    }
}

/// The difference between the derivations `a` and `b`, walking `depth`
/// levels of input derivations (`usize::MAX`: every level). `load` reads a
/// derivation: from the evaluation that made it, else from `/nix/store`.
pub fn diff(
    load: &mut DrvLoader<'_>,
    a: &StorePath,
    b: &StorePath,
    depth: usize,
) -> Result<Diff, DrvError> {
    walk(load, a, b, depth, &mut HashSet::new())
}

fn walk(
    load: &mut DrvLoader<'_>,
    pa: &StorePath,
    pb: &StorePath,
    depth: usize,
    seen: &mut HashSet<(StorePath, StorePath)>,
) -> Result<Diff, DrvError> {
    let mut d = node(pa, pb, None);
    if pa == pb {
        return Ok(d);
    }
    if !seen.insert((pa.clone(), pb.clone())) {
        return Ok(node(pa, pb, Some("shown above")));
    }
    let (a, b) = (load(pa)?, load(pb)?);
    let v = &mut d.items;
    scalar(v, "name", &a.name, &b.name);
    scalar(v, "platform", &a.platform, &b.platform);
    scalar(v, "builder", &a.builder, &b.builder);
    scalar(v, "args", &args(&a.args), &args(&b.args));
    let (sa, sb) = (a.structured_attrs.clone(), b.structured_attrs.clone());
    entry(v, "structured attrs", "", sa, sb);
    for n in union(a.outputs.keys(), b.outputs.keys()) {
        match (a.outputs.get(n), b.outputs.get(n)) {
            // an input-addressed output path differs whenever anything else
            // does, so only a fixed or content-addressed one is compared
            (Some(x), Some(y)) => entry(v, "output", n, fixed(x), fixed(y)),
            (x, y) => {
                let side = |o: Option<&DerivationOutput>| o.map(|_| n.clone());
                entry(v, "output", "", side(x), side(y));
            }
        }
    }
    // the env entry named after an output holds that output's path, which
    // differs whenever anything else does (nix-diff leaves it out too), and
    // `env name` is the derivation's own name, which the `name` item said
    let echoes_name = |d: &super::drv::Derivation| {
        (d.env.iter()).any(|(k, v)| k == "name" && *v == d.name.as_bytes())
    };
    let hide_name = echoes_name(&a) && echoes_name(&b);
    let keep = |(k, _): &&(String, Vec<u8>)| {
        (!a.outputs.contains_key(k) || !b.outputs.contains_key(k)) && !(hide_name && k == "name")
    };
    let (ea, eb): (BTreeMap<_, _>, BTreeMap<_, _>) = (
        a.env.iter().filter(keep).map(|(k, x)| (k, x)).collect(),
        b.env.iter().filter(keep).map(|(k, x)| (k, x)).collect(),
    );
    for k in union(ea.keys().copied(), eb.keys().copied()) {
        let side = |e: &BTreeMap<&String, &Vec<u8>>| {
            e.get(k).map(|x| String::from_utf8_lossy(x).into_owned())
        };
        entry(v, "env", k, side(&ea), side(&eb));
    }
    let srcs = &a.input_srcs;
    one_sided(v, "input source", srcs.difference(&b.input_srcs), true);
    one_sided(v, "input source", b.input_srcs.difference(srcs), false);
    let (pairs, added, removed) = pair_inputs(&a.input_drvs, &b.input_drvs);
    one_sided(v, "input drv", removed.iter(), true);
    one_sided(v, "input drv", added.iter(), false);
    let want = |n: &InputNode| Some(n.outputs.iter().cloned().collect::<Vec<_>>().join(","));
    for (x, y) in pairs {
        let mut c = match depth {
            0 => node(&x, &y, Some("depth")),
            _ => walk(load, &x, &y, depth - 1, seen)?,
        };
        let (wa, wb) = (want(&a.input_drvs[&x]), want(&b.input_drvs[&y]));
        entry(&mut c.items, "wanted", "", wa, wb);
        if !c.is_empty() || c.cut.is_some() {
            d.changed.push(c);
        }
    }
    Ok(d)
}

/// An empty node for the pair, with the reason the walk stopped, if any.
fn node(a: &StorePath, b: &StorePath, cut: Option<&'static str>) -> Diff {
    Diff {
        a: a.to_string(),
        b: b.to_string(),
        cut,
        ..Diff::default()
    }
}

/// The keys of both sides, sorted, without duplicates.
fn union<'a>(
    a: impl Iterator<Item = &'a String>,
    b: impl Iterator<Item = &'a String>,
) -> BTreeSet<&'a String> {
    a.chain(b).collect()
}

fn scalar(v: &mut Vec<Item>, what: &'static str, a: &str, b: &str) {
    entry(v, what, "", Some(a.to_string()), Some(b.to_string()));
}

/// The paths only one side has; `left` says which side.
fn one_sided<'p>(
    v: &mut Vec<Item>,
    what: &'static str,
    paths: impl Iterator<Item = &'p StorePath>,
    left: bool,
) {
    for p in paths.map(|p| Some(p.to_string())) {
        let (a, b) = if left { (p, None) } else { (None, p) };
        entry(v, what, "", a, b);
    }
}

fn entry(v: &mut Vec<Item>, what: &'static str, key: &str, a: Option<String>, b: Option<String>) {
    let key = key.to_string();
    if a != b {
        v.push(Item { what, key, a, b });
    }
}

/// The builder's args, one quoted arg per line, so a long command line
/// diffs line by line.
fn args(v: &[Vec<u8>]) -> String {
    v.iter()
        .map(|s| format!("{:?}\n", String::from_utf8_lossy(s)))
        .collect()
}

/// A fixed or content-addressed output, as text; `None` for an
/// input-addressed or deferred one.
fn fixed(o: &DerivationOutput) -> Option<String> {
    match o {
        DerivationOutput::Fixed { method, hash } => {
            Some(format!("{}{}", method.prefix(), hash.to_base16()))
        }
        DerivationOutput::CaFloating { method, algo } => {
            Some(format!("{}{}", method.prefix(), algo.name()))
        }
        _ => None,
    }
}

/// The input derivations that are not on both sides, paired by derivation
/// name; the rest are added or removed.
fn pair_inputs(
    a: &BTreeMap<StorePath, InputNode>,
    b: &BTreeMap<StorePath, InputNode>,
) -> (Vec<(StorePath, StorePath)>, Vec<StorePath>, Vec<StorePath>) {
    let mut pairs: Vec<(StorePath, StorePath)> = Vec::new();
    let mut by_name: [BTreeMap<&str, Vec<&StorePath>>; 2] = Default::default();
    for (i, (x, y)) in [(a, b), (b, a)].into_iter().enumerate() {
        for p in x.keys().filter(|p| !y.contains_key(*p)) {
            by_name[i].entry(p.name()).or_default().push(p);
        }
    }
    // the same path on both sides, with different wanted outputs
    pairs.extend(
        (a.iter().filter(|(p, o)| b.get(*p).is_some_and(|q| q != *o)))
            .map(|(p, _)| (p.clone(), p.clone())),
    );
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    let [left, right] = &by_name;
    let names: BTreeSet<&str> = left.keys().chain(right.keys()).copied().collect();
    let none = Vec::new();
    for name in names {
        let (xs, ys) = (
            left.get(name).unwrap_or(&none),
            right.get(name).unwrap_or(&none),
        );
        for i in 0..xs.len().max(ys.len()) {
            match (xs.get(i), ys.get(i)) {
                (Some(x), Some(y)) => pairs.push(((**x).clone(), (**y).clone())),
                (Some(x), None) => removed.push((**x).clone()),
                (None, Some(y)) => added.push((**y).clone()),
                (None, None) => {}
            }
        }
    }
    (pairs, added, removed)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cli::render::Text;
    use crate::store::drv::Derivation;

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
            let mut load =
                |p: &StorePath| -> Result<Derivation, DrvError> { Ok((*self.0[p]).clone()) };
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
        use crate::store::hash::{HashAlgo, hash_bytes};
        use crate::store::path::IngestionMethod;
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
        use crate::api::render::Report;
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
}
