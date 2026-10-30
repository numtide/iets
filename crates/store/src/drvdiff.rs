//! What differs between two derivations, the way nix-diff explains a
//! rebuild: the local differences of a pair, then the input derivations
//! that differ, paired by name and recursed into (docs/reference/commands.md).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::drv::{Derivation, DerivationOutput, DrvError, DrvLoader, InputNode};
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
    let echoes_name =
        |d: &Derivation| (d.env.iter()).any(|(k, v)| k == "name" && *v == d.name.as_bytes());
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
