//! What an operation produced, and the JSON that is its contract: the same
//! bytes `iets plumbing OP` writes and a porcelain `--json` writes. A value
//! two frontends can emit belongs here; how a terminal shows it belongs in
//! `crate::cli::render` (docs/explanation/plumbing-and-porcelain.md).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Value, json};

use crate::build::Outcome;
use crate::remote::plan::Plan;
use crate::remote::worker;
use crate::store::drvdiff::Diff;

/// A value an operation produced. `json` is the contract in docs/reference/plumbing.md.
pub trait Report {
    fn json(&self) -> Value;
}

/// What a build will build and what it will fetch: `plumbing missing`, and
/// `iets build --dry-run --json`.
pub struct Missing {
    pub will_build: Vec<String>,
    pub will_substitute: Vec<String>,
    pub unknown: Vec<String>,
    pub download_size: u64,
    pub nar_size: u64,
}

impl Missing {
    /// The dispatcher's answer ordered: `willBuild` in the plan's order,
    /// references first, as Nix's reversed topoSortPaths gives it, and
    /// `willSubstitute` by the name part of the store path.
    pub fn of(m: &worker::Missing, plan: &Plan) -> Missing {
        let at: HashMap<String, usize> = (plan.drvs.iter().enumerate())
            .map(|(i, o)| (o.store_path().to_string(), i))
            .collect();
        let mut will_build = m.will_build.clone();
        will_build.sort_by_key(|p| at.get(p).copied().unwrap_or(usize::MAX));
        let mut will_substitute = m.will_substitute.clone();
        will_substitute.sort_by_key(|p| (p.get(44..).unwrap_or(p).to_string(), p.clone()));
        Missing {
            will_build,
            will_substitute,
            unknown: m.unknown.clone(),
            download_size: m.download_size,
            nar_size: m.nar_size,
        }
    }
}

impl Report for Missing {
    fn json(&self) -> Value {
        json!({
            "willBuild": self.will_build,
            "willSubstitute": self.will_substitute,
            "unknown": self.unknown,
            "downloadSize": self.download_size,
            "narSize": self.nar_size,
        })
    }
}

/// What became of every root: `plumbing build`, and `iets build --json`.
pub struct Outcomes<'a>(pub &'a [Outcome]);

impl Report for Outcomes<'_> {
    fn json(&self) -> Value {
        let o: Vec<Value> = (self.0.iter())
            .map(|o| {
                json!({
                    "attr": o.attr,
                    "drv": o.drv.to_string(),
                    "success": o.success,
                    "outputs": o.outputs,
                    "reason": o.reason,
                    "logTail": o.log_tail,
                })
            })
            .collect();
        json!({ "outcomes": o })
    }
}

/// One package name whose versions differ between two closures.
pub struct Change {
    pub change: &'static str,
    pub name: String,
    pub a: BTreeSet<String>,
    pub b: BTreeSet<String>,
}

/// Two output closures compared by package name and version, as nvd does:
/// `plumbing closure-diff`, and `iets diff --closure --json`.
pub struct Closure {
    pub a: String,
    pub b: String,
    pub packages: Vec<Change>,
}

type Versions = BTreeMap<String, BTreeSet<String>>;

impl Closure {
    /// The names either closure has, with the versions each side holds,
    /// keeping the ones that differ.
    pub fn of(a: &str, b: &str, ca: &Versions, cb: &Versions) -> Closure {
        let mut names: BTreeSet<&String> = ca.keys().collect();
        names.extend(cb.keys());
        let none = BTreeSet::new();
        let packages = (names.into_iter())
            .map(|n| (n, ca.get(n).unwrap_or(&none), cb.get(n).unwrap_or(&none)))
            .filter(|(_, x, y)| x != y)
            .map(|(n, x, y)| Change {
                change: match (x.is_empty(), y.is_empty()) {
                    (true, _) => "added",
                    (_, true) => "removed",
                    _ => "changed",
                },
                name: n.clone(),
                a: x.clone(),
                b: y.clone(),
            })
            .collect();
        Closure {
            a: a.to_string(),
            b: b.to_string(),
            packages,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }
}

impl Report for Closure {
    fn json(&self) -> Value {
        let p: Vec<Value> = (self.packages.iter())
            .map(|c| json!({"change": c.change, "name": c.name, "a": c.a, "b": c.b}))
            .collect();
        json!({"a": self.a, "b": self.b, "equal": self.is_empty(), "packages": p})
    }
}

/// A hash and what it was taken of: `plumbing fetch`, and
/// `iets prefetch --json`.
pub struct Fetched {
    pub hash: String,
    /// `"url"` or `"drvPath"`, and what it names.
    pub key: &'static str,
    pub origin: String,
}

impl Report for Fetched {
    fn json(&self) -> Value {
        json!({"hash": self.hash, self.key: self.origin})
    }
}

/// `plumbing drv-diff`, and `iets diff --json`. Only the root carries
/// `equal`; an input derivation under it is [`node`]'s shape.
impl Report for Diff {
    fn json(&self) -> Value {
        let mut v = node(self);
        v["equal"] = self.is_empty().into();
        v
    }
}

/// One node of the walk: what differs here, then the input derivations that
/// differ, and why the walk stopped.
fn node(d: &Diff) -> Value {
    let items: Vec<Value> = (d.items.iter())
        .map(|i| json!({"what": i.what, "key": i.key, "a": i.a, "b": i.b}))
        .collect();
    let mut o = json!({"a": d.a, "b": d.b, "changes": items});
    if !d.changed.is_empty() {
        o["inputDrvs"] = d.changed.iter().map(node).collect();
    }
    if let Some(c) = d.cut {
        o["cut"] = c.into();
    }
    o
}
