//! `--constituents`: constituents.cc. After the walk, every
//! `_hydraAggregate` job with named constituents is resolved against the
//! job table, its `.drv` rewritten with the constituents as `inputDrvs`
//! (output paths recomputed, written through the daemon, rooted) and
//! printed; a missing or failed constituent makes the aggregate an error
//! line that keeps its job fields.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use super::line::{Job, Line, json_str};
use crate::error::{Error, Result};
use crate::remote::plan::Object;
use crate::remote::worker::Connection;
use crate::remote::worker::wire::write_object;
use crate::store::drv::{
    DerivationOutput, DrvHashCache, InputNode, finalize_input_addressed_outputs,
    load_derivation_from_store,
};
use crate::store::path::StorePath;

/// What the collector keeps of every line: enough to resolve names.
pub struct Entry {
    pub drv_path: Option<StorePath>,
    pub error: Option<String>,
    /// An aggregate with named constituents, withheld until [`rewrite`].
    pub held: Option<Job>,
}

pub type Jobs = BTreeMap<String, Entry>;

#[derive(Clone)]
struct Aggregate {
    name: String,
    dependencies: BTreeSet<String>,
    broken: BTreeMap<String, String>,
}

struct Cycle {
    a: String,
    b: String,
    remaining: BTreeSet<String>,
}

/// fnmatch(3) with no flags: `*`, `?`, `[...]` (with `!`/`^` and ranges),
/// `\` escapes; `/` and a leading `.` are not special.
pub fn fnmatch(pattern: &[u8], name: &[u8]) -> bool {
    fn bracket(p: &[u8], c: u8) -> Option<(bool, usize)> {
        // p starts after '['; returns (matched, bytes consumed incl. ']').
        let mut i = 0;
        let negate = matches!(p.first(), Some(b'!') | Some(b'^'));
        if negate {
            i = 1;
        }
        let mut matched = false;
        let mut first = true;
        loop {
            let &b = p.get(i)?;
            if b == b']' && !first {
                return Some((matched != negate, i + 1));
            }
            first = false;
            let lo = if b == b'\\' {
                i += 1;
                *p.get(i)?
            } else {
                b
            };
            i += 1;
            if p.get(i) == Some(&b'-') && p.get(i + 1).is_some_and(|&n| n != b']') {
                let mut hi = p[i + 1];
                i += 2;
                if hi == b'\\' {
                    hi = *p.get(i)?;
                    i += 1;
                }
                if lo <= c && c <= hi {
                    matched = true;
                }
            } else if lo == c {
                matched = true;
            }
        }
    }
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
                continue;
            }
            Some(b'?') => {
                p += 1;
                n += 1;
                continue;
            }
            Some(b'[') => {
                if let Some((ok, used)) = bracket(&pattern[p + 1..], name[n]) {
                    if ok {
                        p += 1 + used;
                        n += 1;
                        continue;
                    }
                } else if name[n] == b'[' {
                    p += 1;
                    n += 1;
                    continue;
                }
            }
            Some(b'\\') if p + 1 < pattern.len() => {
                if pattern[p + 1] == name[n] {
                    p += 2;
                    n += 1;
                    continue;
                }
            }
            Some(&c) if c == name[n] => {
                p += 1;
                n += 1;
                continue;
            }
            _ => {}
        }
        match star {
            Some((sp, sn)) => {
                p = sp + 1;
                n = sn + 1;
                star = Some((sp, sn + 1));
            }
            None => return false,
        }
    }
    while pattern.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pattern.len()
}

/// constituents.cc:resolveNamedConstituents: which jobs each aggregate
/// depends on and which of its constituents are broken.
fn resolve_named(jobs: &Jobs) -> BTreeMap<String, Aggregate> {
    let mut aggregates = BTreeMap::new();
    for (name, entry) in jobs {
        let Some(job) = &entry.held else { continue };
        let Some(named) = &job.named_constituents else {
            continue;
        };
        if named.is_empty() {
            continue;
        }
        let mut broken: BTreeMap<String, String> = BTreeMap::new();
        let mut results = BTreeSet::new();
        let is_broken = |broken: &mut BTreeMap<String, String>, child: &str, e: &Entry| -> bool {
            if let Some(err) = &e.error {
                crate::log::line(&format!(
                    "error: aggregate job '{name}' references broken job '{child}': {err}"
                ));
                broken.insert(child.to_string(), err.clone());
                return true;
            }
            false
        };
        for child in named {
            match jobs.get(child) {
                Some(e) => {
                    if !is_broken(&mut broken, child, e) {
                        results.insert(child.clone());
                    }
                }
                None if !job.glob_constituents => {
                    crate::log::line(&format!(
                        "error: aggregate job '{name}' references non-existent job '{child}'"
                    ));
                    broken.insert(child.clone(), "does not exist".into());
                }
                None => {
                    let mut found = false;
                    for (other, e) in jobs {
                        if other == name {
                            continue;
                        }
                        if fnmatch(child.as_bytes(), other.as_bytes())
                            && !is_broken(&mut broken, other, e)
                        {
                            results.insert(other.clone());
                            found = true;
                        }
                    }
                    if !found {
                        crate::log::line(&format!(
                            "warning: aggregate job '{name}' references constituent glob pattern '{child}' with no matches"
                        ));
                        broken.insert(
                            child.clone(),
                            "constituent glob pattern had no matches".into(),
                        );
                    }
                }
            }
        }
        aggregates.insert(
            name.clone(),
            Aggregate {
                name: name.clone(),
                dependencies: results,
                broken,
            },
        );
    }
    aggregates
}

/// constituents.cc:topoSort: dependencies first; a cycle names the two
/// jobs and every aggregate not yet erased.
fn topo_sort(mut dict: BTreeMap<String, Aggregate>) -> std::result::Result<Vec<Aggregate>, Cycle> {
    fn visit(
        dict: &mut BTreeMap<String, Aggregate>,
        visited: &mut BTreeSet<String>,
        parents: &mut BTreeSet<String>,
        sorted: &mut Vec<Aggregate>,
        path: &str,
        parent: Option<&str>,
    ) -> std::result::Result<(), Cycle> {
        if parents.contains(path) {
            dict.remove(path);
            let parent = parent.unwrap_or_default();
            dict.remove(parent);
            return Err(Cycle {
                a: path.to_string(),
                b: parent.to_string(),
                remaining: dict.keys().cloned().collect(),
            });
        }
        if !visited.insert(path.to_string()) {
            return Ok(());
        }
        parents.insert(path.to_string());
        let refs = dict[path].dependencies.clone();
        for r in &refs {
            if r != path && dict.contains_key(r) {
                visit(dict, visited, parents, sorted, r, Some(path))?;
            }
        }
        sorted.push(dict[path].clone());
        parents.remove(path);
        Ok(())
    }
    let mut sorted = Vec::new();
    let mut visited = BTreeSet::new();
    let mut parents = BTreeSet::new();
    let keys: Vec<String> = dict.keys().cloned().collect();
    for k in keys {
        visit(&mut dict, &mut visited, &mut parents, &mut sorted, &k, None)?;
    }
    Ok(sorted)
}

fn set_error(job: &mut Job, msg: &str) {
    job.extra.insert("error".into(), json_str(msg));
}

/// Resolves and prints every withheld aggregate through `out`.
pub fn rewrite(
    jobs: &mut Jobs,
    conn: &mut Connection,
    gc_roots_dir: Option<&Path>,
    out: &mut dyn FnMut(Line),
) -> Result<()> {
    let aggregates = resolve_named(jobs);
    let sorted = match topo_sort(aggregates) {
        Ok(s) => s,
        Err(cycle) => {
            let msg = format!("Dependency cycle: {} <-> {}", cycle.a, cycle.b);
            crate::log::line(&format!(
                "error: Found dependency cycle between jobs '{}' and '{}'",
                cycle.a, cycle.b
            ));
            for name in [&cycle.a, &cycle.b] {
                if let Some(mut job) = jobs.get_mut(name).and_then(|e| e.held.take()) {
                    set_error(&mut job, &msg);
                    out(Line::Job(job));
                }
            }
            for name in &cycle.remaining {
                if let Some(mut job) = jobs.get_mut(name).and_then(|e| e.held.take()) {
                    set_error(&mut job, "Skipping aggregate because of a dependency cycle");
                    out(Line::Job(job));
                }
            }
            return Ok(());
        }
    };
    let mut hash_cache = DrvHashCache::new();
    for agg in sorted {
        let Some(mut job) = jobs.get_mut(&agg.name).and_then(|e| e.held.take()) else {
            continue;
        };
        if agg.broken.is_empty() {
            let mut drv = load_derivation_from_store(&job.drv_path)?;
            for dep in &agg.dependencies {
                let child = jobs[dep]
                    .drv_path
                    .clone()
                    .ok_or_else(|| Error::Store(format!("constituent '{dep}' has no drvPath")))?;
                let child_drv = load_derivation_from_store(&child)?;
                let first =
                    child_drv.outputs.keys().next().cloned().ok_or_else(|| {
                        Error::Store(format!("derivation '{child}' has no outputs"))
                    })?;
                job.constituents.push(child.clone());
                drv.input_drvs
                    .insert(child, InputNode::of(BTreeSet::from([first])));
            }
            // Deferred outputs make fillInOutputPaths recompute them with
            // the new inputDrvs.
            let names: Vec<String> = drv.outputs.keys().cloned().collect();
            for name in &names {
                if matches!(drv.outputs[name], DerivationOutput::InputAddressed(_)) {
                    drv.outputs.insert(name.clone(), DerivationOutput::Deferred);
                    drv.env_set(name, Vec::new());
                }
            }
            let mut loader = |p: &StorePath| load_derivation_from_store(p);
            finalize_input_addressed_outputs(&mut drv, &mut hash_cache, &mut loader)?;
            let text = drv.unparse()?;
            let new_path = drv.drv_path_for(&text)?;
            if new_path != job.drv_path {
                let valid = conn.query_valid_paths(&[new_path.to_string()], &mut |_| {})?;
                if valid.is_empty() {
                    let mut references = drv.input_srcs.clone();
                    references.extend(drv.input_drvs.keys().cloned());
                    let aterm = std::sync::Arc::new(text);
                    let object = Object::drv(new_path.clone(), aterm, references);
                    let mut body = |w: &mut dyn Write| -> std::io::Result<()> {
                        crate::remote::worker::wire::write_u64(w, 1)?;
                        write_object(w, &object)
                    };
                    conn.add_multiple_to_store(false, &mut body, &mut |_| {})?;
                }
                if let Some(dir) = gc_roots_dir {
                    let root = dir.join(new_path.base_name());
                    if std::fs::symlink_metadata(&root).is_err() {
                        conn.add_perm_root(&new_path.to_string(), &root)?;
                    }
                }
                job.drv_path = new_path.clone();
                for name in &names {
                    let p = drv.env_str(name).and_then(|s| StorePath::parse(s).ok());
                    job.outputs.insert(name.clone(), p);
                }
                if let Some(e) = jobs.get_mut(&agg.name) {
                    e.drv_path = Some(new_path);
                }
            }
        }
        job.named_constituents = None;
        if !agg.broken.is_empty() {
            let mut msg = String::new();
            for (name, err) in &agg.broken {
                msg.push_str(&format!("{name}: {err}\n"));
            }
            set_error(&mut job, &msg);
        }
        out(Line::Job(job));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_basics() {
        assert!(fnmatch(b"*", b"anything.here"));
        assert!(fnmatch(b"packages.*", b"packages.constituentA"));
        assert!(!fnmatch(b"tests.*", b"packages.constituentA"));
        assert!(fnmatch(b"a?c", b"abc"));
        assert!(!fnmatch(b"a?c", b"abbc"));
        assert!(fnmatch(b"a[b-d]c", b"acc"));
        assert!(!fnmatch(b"a[!b-d]c", b"acc"));
        assert!(fnmatch(b"a\\*c", b"a*c"));
        assert!(!fnmatch(b"a\\*c", b"abc"));
        assert!(fnmatch(
            b"*.x86_64-linux",
            b"nixos.tests.login.x86_64-linux"
        ));
        assert!(!fnmatch(b"abc", b"abcd"));
        assert!(fnmatch(b"a*b*c", b"axxbyyc"));
    }

    #[test]
    fn topo_sort_orders_and_detects_cycles() {
        let agg = |name: &str, deps: &[&str]| Aggregate {
            name: name.into(),
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
            broken: BTreeMap::new(),
        };
        let dict: BTreeMap<String, Aggregate> =
            [agg("indirect", &["direct"]), agg("direct", &["leaf"])]
                .into_iter()
                .map(|a| (a.name.clone(), a))
                .collect();
        let names: Vec<String> = topo_sort(dict)
            .ok()
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, ["direct", "indirect"]);
        let dict: BTreeMap<String, Aggregate> = [
            agg("aggregate0", &["aggregate1"]),
            agg("aggregate1", &["aggregate0"]),
            agg("other", &[]),
        ]
        .into_iter()
        .map(|a| (a.name.clone(), a))
        .collect();
        let Err(c) = topo_sort(dict) else { panic!() };
        assert_eq!((c.a.as_str(), c.b.as_str()), ("aggregate0", "aggregate1"));
        assert_eq!(c.remaining, BTreeSet::from(["other".to_string()]));
    }
}
