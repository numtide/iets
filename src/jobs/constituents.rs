//! `--constituents`: constituents.cc. After the walk, every
//! `_hydraAggregate` job with named constituents is resolved against the
//! job table, its `.drv` rewritten with the constituents as `inputDrvs`
//! (output paths recomputed, written through the daemon, rooted) and
//! printed; a missing or failed constituent makes the aggregate an error
//! line that keeps its job fields.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::{fs, result};

use super::line::{Job, Line, json_str};
use crate::error::{Error, Result};
use crate::log;
use crate::remote::plan::Object;
use crate::remote::worker::Connection;
use crate::remote::worker::wire::{self, write_object};
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

struct Aggregate {
    dependencies: BTreeSet<String>,
    broken: BTreeMap<String, String>,
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
            Some(b'\\') => {
                if pattern.get(p + 1) == Some(&name[n]) {
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
                log::line(&format!(
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
                    log::line(&format!(
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
                        log::line(&format!(
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
                dependencies: results,
                broken,
            },
        );
    }
    aggregates
}

type NamedAggregate<'a> = (&'a str, &'a Aggregate);

/// constituents.cc:topoSort: dependencies first; report the first back edge.
fn topo_sort(
    dict: &BTreeMap<String, Aggregate>,
) -> result::Result<Vec<NamedAggregate<'_>>, (&str, &str)> {
    fn visit<'a>(
        dict: &'a BTreeMap<String, Aggregate>,
        visited: &mut BTreeSet<&'a str>,
        parents: &mut BTreeSet<&'a str>,
        sorted: &mut Vec<NamedAggregate<'a>>,
        (name, aggregate): NamedAggregate<'a>,
    ) -> result::Result<(), (&'a str, &'a str)> {
        if !visited.insert(name) {
            return Ok(());
        }
        parents.insert(name);
        for dep in &aggregate.dependencies {
            if dep != name
                && let Some(child) = dict.get(dep)
            {
                if parents.contains(dep.as_str()) {
                    return Err((dep, name));
                }
                visit(dict, visited, parents, sorted, (dep, child))?;
            }
        }
        sorted.push((name, aggregate));
        parents.remove(name);
        Ok(())
    }
    let mut sorted = Vec::new();
    let mut visited = BTreeSet::new();
    let mut parents = BTreeSet::new();
    for (name, aggregate) in dict {
        visit(
            dict,
            &mut visited,
            &mut parents,
            &mut sorted,
            (name, aggregate),
        )?;
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
    let sorted = match topo_sort(&aggregates) {
        Ok(s) => s,
        Err((a, b)) => {
            let msg = format!("Dependency cycle: {a} <-> {b}");
            log::line(&format!(
                "error: Found dependency cycle between jobs '{a}' and '{b}'"
            ));
            for name in [a, b] {
                if let Some(mut job) = jobs.get_mut(name).and_then(|e| e.held.take()) {
                    set_error(&mut job, &msg);
                    out(Line::Job(job));
                }
            }
            // The cycle endpoints have already been taken from the held jobs.
            for name in aggregates.keys() {
                if let Some(mut job) = jobs.get_mut(name).and_then(|e| e.held.take()) {
                    set_error(&mut job, "Skipping aggregate because of a dependency cycle");
                    out(Line::Job(job));
                }
            }
            return Ok(());
        }
    };
    let mut hash_cache = DrvHashCache::new();
    for (name, agg) in sorted {
        let Some(mut job) = jobs.get_mut(name).and_then(|e| e.held.take()) else {
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
                    let aterm = Arc::new(text);
                    let object = Object::drv(new_path.clone(), aterm, references);
                    let mut body = |w: &mut dyn Write| -> io::Result<()> {
                        wire::write_u64(w, 1)?;
                        write_object(w, &object)
                    };
                    conn.add_multiple_to_store(false, &mut body, &mut |_| {})?;
                }
                if let Some(dir) = gc_roots_dir {
                    let root = dir.join(new_path.base_name());
                    if fs::symlink_metadata(&root).is_err() {
                        conn.add_perm_root(&new_path.to_string(), &root)?;
                    }
                }
                job.drv_path = new_path.clone();
                for name in &names {
                    let p = drv.env_str(name).and_then(|s| StorePath::parse(s).ok());
                    job.outputs.insert(name.clone(), p);
                }
                if let Some(e) = jobs.get_mut(name) {
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
    use rstest::rstest;

    #[rstest]
    #[case(b"*", b"anything.here", true)]
    #[case(b"packages.*", b"packages.constituentA", true)]
    #[case(b"tests.*", b"packages.constituentA", false)]
    #[case(b"a?c", b"abc", true)]
    #[case(b"a?c", b"abbc", false)]
    #[case(b"a[b-d]c", b"acc", true)]
    #[case(b"a[!b-d]c", b"acc", false)]
    #[case(b"a\\*c", b"a*c", true)]
    #[case(b"a\\*c", b"abc", false)]
    #[case(b"*.x86_64-linux", b"nixos.tests.login.x86_64-linux", true)]
    #[case(b"abc", b"abcd", false)]
    #[case(b"a*b*c", b"axxbyyc", true)]
    #[case(br"a*\", br"abc\", false)]
    #[case(br"a*\\", br"abc\", true)]
    fn fnmatch_basics(#[case] pattern: &[u8], #[case] name: &[u8], #[case] want: bool) {
        assert_eq!(fnmatch(pattern, name), want);
    }

    #[test]
    fn topo_sort_orders_and_detects_cycles() {
        let agg = |name: &str, deps: &[&str]| {
            (
                name.to_owned(),
                Aggregate {
                    dependencies: deps.iter().map(|s| s.to_string()).collect(),
                    broken: BTreeMap::new(),
                },
            )
        };
        let dict = BTreeMap::from([
            agg("indirect", &["direct"]),
            agg("direct", &["direct", "leaf"]),
        ]);
        let names: Vec<_> = topo_sort(&dict)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["direct", "indirect"]);
        let dict = BTreeMap::from([
            agg("aggregate0", &["aggregate1"]),
            agg("aggregate1", &["aggregate0"]),
            agg("other", &[]),
        ]);
        assert_eq!(topo_sort(&dict).err(), Some(("aggregate0", "aggregate1")));
    }
}
