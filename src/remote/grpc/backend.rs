//! `grpc://` as one of the dispatcher's backends (`build::backend`).
//!
//! Where the worker protocol sends one `BuildPathsWithResults` per ready set,
//! this fans the set out. The store builds one derivation per
//! `BuildDerivation` call, so the client walks the batch's whole derivation
//! closure, dependencies first, up to `min(max-builds, max-jobs)` in flight.
//! Each derivation goes to the scheduler first (`sched.rs`), which names the
//! worker or answers that the cache has it. Every call knows its derivation,
//! so its `log_line` and `phase` events carry the drv path and the renderer
//! keys its per-build buffers on them.
//!
//! Outputs land in the farm's binary cache, not in a local store, so an
//! out-link needs them registered locally first (docs/reference/builders.md).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::build::backend::{self, Backend, Batch, Phase, Status, add_roots, link_targets, local};
use crate::build::fanout;
use crate::error::{Error, Result};
use crate::log::{
    self, ACT_BUILD, ACT_BUILDS, ACT_COPY_PATH, ACT_COPY_PATHS, ACT_REALISE, ACT_UNKNOWN, Activity,
    LVL_ERROR, LVL_INFO, LVL_TALKATIVE, RES_PROGRESS,
};
use crate::remote::copy::Copying;
use crate::remote::event::{Event, Started, Stopped};
use crate::remote::plan::{Object, Plan};
use crate::remote::worker::{Connection, failed_event};
use crate::store::drv::Derivation;
use crate::store::path::StorePath;

use super::build::{self, Objects, Timings};
use super::drvwire::{BuildResult, basic_input_srcs};
use super::sched::Sched;
use super::{Client, GrpcConfig};

pub struct Grpc {
    c: Client,
    t: Timings,
    /// Everything this run uploaded, so a `NOT_FOUND` retry can re-send one
    /// derivation's closure.
    objects: Objects,
    /// The derivations the submitted batches named, for their output paths.
    drvs: BTreeMap<StorePath, Derivation>,
}

impl Grpc {
    pub fn open(cfg: &GrpcConfig) -> Result<Grpc> {
        let c = Client::open(cfg)?;
        Ok(Grpc {
            t: Timings {
                common: backend::Timings {
                    trusted: c.info.trusted,
                    ..Default::default()
                },
                calls: 1,
                ..Timings::default()
            },
            c,
            objects: Objects::default(),
            drvs: BTreeMap::new(),
        })
    }
}

/// A `Want` and a `BuildDerivation` per derivation of the batch's closure, at
/// most `max-builds` in flight, a derivation only once its inputs are done.
/// Derivations whose outputs the store already reports valid are skipped. The
/// C++ plugin sends them anyway and lets the niks3 claim answer `AlreadyValid`,
/// which costs one RPC per derivation; one extra `QueryValidPaths` over the
/// output paths is cheaper and asks the same store.
fn build_batch(
    c: &mut Client,
    t: &mut Timings,
    objects: &Objects,
    drvs: &BTreeMap<StorePath, Derivation>,
    batch: &Batch<'_>,
    on_event: &mut dyn FnMut(Event),
) -> Result<HashMap<String, Status>> {
    let mut out: HashMap<String, Status> = batch
        .jobs
        .iter()
        .map(|j| (j.drv.to_string(), Status::ok()))
        .collect();
    let roots = batch
        .jobs
        .iter()
        .map(|j| {
            drvs.get_key_value(&j.drv)
                .map(|(k, _)| k)
                .ok_or_else(|| Error::Plan(format!("'{}' is not in the batch's closure", j.drv)))
        })
        .collect::<Result<Vec<_>>>()?;
    let needs = missing_drvs(drvs, roots, &mut |names| {
        build::query_valid_paths(c, names, t)
    })?;

    let mut jobs: Vec<build::Job> = Vec::new();
    let mut index: HashMap<&StorePath, usize> = HashMap::new();
    for (drv_path, drv) in drvs {
        if !needs.contains(drv_path) {
            continue;
        }
        let srcs = basic_input_srcs(drv_path, drv, &|dep, output| {
            build::output_path(drvs, dep, output)
        })?;
        index.insert(drv_path, jobs.len());
        jobs.push(build::Job::new(drv_path.clone(), drv.clone(), srcs));
    }
    t.drvs += jobs.len();
    t.drvs_cached += drvs.len() - jobs.len();
    if jobs.is_empty() {
        return Ok(out);
    }
    let graph: Vec<fanout::Job> = jobs
        .iter()
        .map(|j| fanout::Job {
            deps: j
                .drv
                .input_drvs
                .keys()
                .filter_map(|d| index.get(d).copied())
                .collect(),
        })
        .collect();

    let sched = Sched::open(c)?;
    let client: &Client = c;
    let jobs = &jobs;
    let calls = AtomicUsize::new(0);
    // no daemon to report, so the activities of build/worker.cc come from here
    let _realise = Activity::new(LVL_ERROR, ACT_REALISE, "", vec![]);
    let builds = Activity::new(LVL_ERROR, ACT_BUILDS, "", vec![]);
    let _substitutions = Activity::new(LVL_ERROR, ACT_COPY_PATHS, "", vec![]);
    let counts = Mutex::new([0u64, jobs.len() as u64, 0, 0]);
    let progress = |f: &dyn Fn(&mut [u64; 4])| {
        let mut n = counts.lock().unwrap();
        f(&mut n);
        builds.result(RES_PROGRESS, n.iter().map(|&v| v.into()).collect());
    };
    let url = client.cfg.url();
    let build = |i: usize, tx: &Sender<Event>| -> BuildResult {
        let job = &jobs[i];
        let drv = job.drv_path.to_string();
        let f = vec![drv.as_str().into(), url.as_str().into(), 1.into(), 1.into()];
        let act = Activity::new(
            LVL_INFO,
            ACT_BUILD,
            &format!("building '{drv}' on '{url}'"),
            f,
        );
        progress(&|n| n[2] += 1);
        let _ = tx.send(Event::Started(Started {
            drv_path: drv.clone(),
        }));
        let res = match build::build(client, &sched, job, objects, tx, &act) {
            Ok((res, n)) => {
                calls.fetch_add(n, Ordering::Relaxed);
                res
            }
            // 9 = MiscFailure. nix reports only the top level result, so name
            // the leaf that broke.
            Err(e) => BuildResult {
                status: 9,
                error_msg: e.to_string(),
                ..BuildResult::default()
            },
        };
        progress(&|n| {
            n[2] -= 1;
            n[if res.success() { 0 } else { 3 }] += 1;
        });
        let _ = tx.send(Event::Stopped(Stopped {
            drv_path: drv,
            built: res.success(),
        }));
        res
    };
    let work = fanout::Work {
        build: &build,
        ok: &BuildResult::success,
        // 10 = DependencyFailed.
        blocked: &|_, dep| BuildResult {
            status: 10,
            error_msg: format!("dependency '{}' failed", jobs[dep].drv_path),
            ..BuildResult::default()
        },
    };
    let results = fanout::run(&graph, client.cfg.max_builds, &work, on_event);
    t.calls += calls.load(Ordering::Relaxed);

    for (i, res) in results.iter().enumerate() {
        let drv = jobs[i].drv_path.to_string();
        let (reason, exit) = match res {
            Some(res) if res.success() => continue,
            Some(res) => (
                format!("status {}: {}", res.status, res.error_msg),
                log::build_exit(res.status, &res.error_msg),
            ),
            None => ("the farm never reached this derivation".to_string(), 1),
        };
        match out.get_mut(&drv) {
            Some(slot) => *slot = Status::failed(reason, exit),
            // A dependency, not something the caller asked for: the renderer
            // still wants its verdict.
            None => on_event(failed_event(&drv, &reason)),
        }
    }
    Ok(out)
}

/// What the walk may ask about: every output of a root, and below one the
/// outputs each input edge names. This is only the batching of the queries;
/// whether a derivation is missing depends on `needs` in `missing_drvs`.
fn askable<'a>(
    drvs: &'a BTreeMap<StorePath, Derivation>,
    roots: &[&'a StorePath],
) -> HashMap<&'a StorePath, BTreeSet<String>> {
    let mut ask: HashMap<&'a StorePath, BTreeSet<String>> = HashMap::new();
    let mut stack: Vec<&'a StorePath> = Vec::new();
    let grow = |ask: &mut HashMap<&'a StorePath, BTreeSet<String>>,
                    stack: &mut Vec<&'a StorePath>,
                    p: &'a StorePath,
                    names: &BTreeSet<String>| {
        let entry = ask.entry(p).or_default();
        let before = entry.len();
        entry.extend(names.iter().cloned());
        if entry.len() != before {
            stack.push(p);
        }
    };
    for root in roots {
        if let Some(drv) = drvs.get(*root) {
            let names: BTreeSet<String> = drv.outputs.keys().cloned().collect();
            grow(&mut ask, &mut stack, root, &names);
        }
    }
    while let Some(p) = stack.pop() {
        // only known derivations are ever pushed
        let drv = &drvs[p];
        for (dep, node) in &drv.input_drvs {
            if let Some((dep_key, _)) = drvs.get_key_value(dep) {
                // an edge naming no outputs is malformed; assume every
                // output, as the walk did before it knew used outputs
                let all: BTreeSet<String>;
                let names = if node.outputs.is_empty() {
                    all = drvs[dep_key].outputs.keys().cloned().collect();
                    &all
                } else {
                    &node.outputs
                };
                grow(&mut ask, &mut stack, dep_key, names);
            }
        }
    }
    ask
}

/// The derivations of the roots' closure the store must build: a root
/// whose wanted outputs it lacks, and below one only what a derivation it
/// must build needs. Needs travel down only through missing derivations:
/// a held derivation never builds, so what only it uses stays satisfied.
/// This is nix's `missingForBuild` (src/top-down.hh): a node counts as
/// needed only for the outputs a user that gets built asks of it, so an
/// output nobody building needs (say `dev`) that the store lacks marks
/// nothing missing. Nothing below a derivation the store has built is asked
/// about or built either.
///
/// The store is asked top-down: the roots, then what they need, then
/// everything the walk may still reach in one call. A held node answers
/// for its closure, so a warm store answers the first small calls.
fn missing_drvs<'a>(
    drvs: &'a BTreeMap<StorePath, Derivation>,
    roots: Vec<&'a StorePath>,
    query: &mut dyn FnMut(&[String]) -> Result<HashSet<String>>,
) -> Result<HashSet<&'a StorePath>> {
    // Output names each derivation is needed for. Roots need every output;
    // below one only the outputs an edge of a missing derivation names.
    let mut needs: HashMap<&'a StorePath, BTreeSet<String>> = HashMap::new();
    for root in &roots {
        if let Some(drv) = drvs.get(*root) {
            needs.insert(*root, drv.outputs.keys().cloned().collect());
        }
    }
    // What later rounds may still reach, for the one call that covers the
    // rest. Read-only batching only: the verdicts still come from `needs`.
    let ask = askable(drvs, &roots);
    // Base names the store reports valid, and output paths asked about.
    let mut valid: HashSet<String> = HashSet::new();
    let mut asked: HashSet<StorePath> = HashSet::new();
    // A round queries every newly needed output path in one call, marks the
    // derivations with a needed output missing, and propagates the needs of
    // those below them. Needs only grow, over finitely many outputs, so the
    // walk reaches a fixpoint; every round asks something new or ends it.
    // After two levels the rest goes in one call, as in `plan::query_missing`.
    let mut round = 0;
    loop {
        // The static path of every needed output, or an error when a needed
        // output has none. Outputs nobody needs never error here.
        let mut paths: Vec<StorePath> = Vec::new();
        let mut resolve = |names: &BTreeSet<String>, drv: &Derivation, p: &StorePath| -> Result<()> {
            for name in names {
                let out = drv
                    .outputs
                    .get(name)
                    .and_then(|o| o.path(&drv.name, name).ok())
                    .flatten();
                let Some(out) = out else {
                    return Err(Error::Plan(format!(
                        "'{p}': output '{name}' has no statically known path; the build \
                         farm needs statically known input and output paths"
                    )));
                };
                if asked.insert(out.clone()) {
                    paths.push(out);
                }
            }
            Ok(())
        };
        for (p, names) in &needs {
            resolve(names, &drvs[*p], p)?;
        }
        if round >= 2 {
            for (p, names) in &ask {
                // Best effort: what is never needed must not error here.
                let drv = &drvs[*p];
                for name in names {
                    if let Some(out) = drv
                        .outputs
                        .get(name)
                        .and_then(|o| o.path(&drv.name, name).ok())
                        .flatten()
                        && asked.insert(out.clone())
                    {
                        paths.push(out);
                    }
                }
            }
        }
        if !paths.is_empty() {
            let names: Vec<String> =
                paths.iter().map(|p| p.base_name().to_string()).collect();
            valid.extend(query(&names)?);
        }
        let mut missing: HashSet<&'a StorePath> = HashSet::new();
        for p in needs.keys() {
            let drv = &drvs[*p];
            let held = needs[*p].iter().all(|name| {
                drv.outputs.get(name).and_then(|o| o.path(&drv.name, name).ok()).flatten().is_some_and(
                    |out| valid.contains(out.base_name()),
                )
            });
            if !held {
                missing.insert(*p);
            }
        }
        let mut grew = false;
        for p in &missing {
            for (dep, node) in &drvs[*p].input_drvs {
                let Some((dep_key, _)) = drvs.get_key_value(dep) else {
                    continue;
                };
                // an edge naming no outputs is malformed; assume every
                // output, as the walk did before it knew used outputs
                let all: BTreeSet<String>;
                let names = if node.outputs.is_empty() {
                    all = drvs[dep_key].outputs.keys().cloned().collect();
                    &all
                } else {
                    &node.outputs
                };
                let entry = needs.entry(dep_key).or_default();
                let before = entry.len();
                entry.extend(names.iter().cloned());
                grew |= entry.len() != before;
            }
        }
        if !grew {
            return Ok(missing);
        }
        round += 1;
    }
}

impl Backend for Grpc {
    fn query_valid(
        &mut self,
        paths: &[&StorePath],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<HashSet<String>> {
        let _ = on_event;
        if paths.is_empty() {
            return Ok(HashSet::new());
        }
        let names: Vec<String> = paths.iter().map(|p| p.base_name().to_string()).collect();
        self.t.common.query_paths += names.len();
        let t0 = Instant::now();
        let valid = build::query_valid_paths(&mut self.c, &names, &mut self.t)?;
        self.t.common.query += t0.elapsed();
        Ok(valid)
    }

    fn substitute(&mut self, paths: &[StorePath], on_event: &mut dyn FnMut(Event)) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.t.common.substitute_paths += paths.len();
        let t0 = Instant::now();
        let url = self.c.cfg.url();
        let n = paths.len();
        let _ = on_event;
        let what = format!("substituting {n} paths on '{url}'");
        let _act = Activity::new(LVL_TALKATIVE, ACT_UNKNOWN, &what, vec![]);
        let r = build::substitute(&mut self.c, paths, &mut self.t);
        self.t.common.substitute += t0.elapsed();
        r
    }

    fn add_objects(&mut self, objects: &[&Object], on_event: &mut dyn FnMut(Event)) -> Result<u64> {
        let _ = on_event;
        if objects.is_empty() {
            return Ok(0);
        }
        let t0 = Instant::now();
        let _copying = Copying::new(objects.len(), "local", &self.c.cfg.url());
        let bytes = build::add_multiple(&mut self.c, objects, &mut self.t)?;
        self.t.common.add += t0.elapsed();
        self.t.common.add_paths += objects.len();
        self.t.common.add_bytes += bytes;
        self.objects.extend(objects);
        Ok(bytes)
    }

    fn submit(
        &mut self,
        batch: &Batch<'_>,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<HashMap<String, Status>> {
        // The client walks the closure, so it needs all of it.
        let drvs = (batch.drvs)(true)?;
        self.drvs.extend(build::drv_table(&drvs)?);

        let t0 = Instant::now();
        let r = build_batch(
            &mut self.c,
            &mut self.t,
            &self.objects,
            &self.drvs,
            batch,
            on_event,
        );
        self.t.common.build += t0.elapsed();
        r
    }

    fn outputs(
        &mut self,
        drv: &str,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<BTreeMap<String, Option<String>>> {
        let _ = on_event;
        let t0 = Instant::now();
        let path = StorePath::parse(drv)?;
        let d = self
            .drvs
            .get(&path)
            .ok_or_else(|| Error::Plan(format!("'{drv}' is not in the batch's closure")))?;
        let mut out = BTreeMap::new();
        for (name, o) in &d.outputs {
            let Some(p) = o.path(&d.name, name)? else {
                return Err(Error::Plan(format!(
                    "'{drv}': output '{name}' has no statically known path"
                )));
            };
            out.insert(name.clone(), Some(p.to_string()));
        }
        self.t.common.outputs += t0.elapsed();
        Ok(out)
    }

    fn fetch(&mut self, store_path: &str, dir: &Path) -> Result<()> {
        let t0 = Instant::now();
        let what = format!("copying path '{store_path}' from '{}'", self.c.cfg.url());
        let _act = Activity::new(LVL_INFO, ACT_COPY_PATH, &what, vec![store_path.into()]);
        let paths = [StorePath::parse(store_path)?];
        build::fetch_into_dir(&mut self.c, &paths, dir, &mut self.t)?;
        self.t.common.fetch += t0.elapsed();
        self.t.common.fetched += 1;
        Ok(())
    }

    fn add_root(&mut self, store_path: &str, link: &Path) -> Result<()> {
        self.out_links(&[(store_path.to_string(), Some(link.display().to_string()))])
    }

    /// The store published the outputs to its cache, so they are copied into
    /// the local store before the symlink that names them.
    fn out_links(&mut self, links: &[(String, Option<String>)]) -> Result<()> {
        if links.is_empty() {
            return Ok(());
        }
        let t0 = Instant::now();
        self.t.common.links += links.len();
        let paths = link_targets(links)?;
        let mut conn = Connection::open(&local(), &mut |_| {})?;
        self.t.common.link_copied += build::register(&mut self.c, &paths, &mut conn, &mut |_| {})?;
        let r = add_roots(&mut conn, links);
        self.t.common.link += t0.elapsed();
        r
    }

    fn note_connect(&mut self, d: Duration) {
        self.t.common.connect = d;
    }

    fn note_upload(&mut self, p: Phase) {
        self.t.common.note_upload(p);
    }

    fn report(&self, plan: &Plan, plan_time: Duration) -> String {
        self.t.report(plan, plan_time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::drv::{DerivationOutput, InputNode};

    fn sp(name: &str) -> StorePath {
        const NIX32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
        let hash: String = name
            .bytes()
            .cycle()
            .take(32)
            .map(|b| NIX32[b as usize % 32] as char)
            .collect();
        StorePath::parse(&format!("/nix/store/{hash}-{name}")).unwrap()
    }

    /// Each `(name, inputs)` is `name.drv` with one output `name-out`.
    fn graph(edges: &[(&str, &[&str])]) -> BTreeMap<StorePath, Derivation> {
        edges
            .iter()
            .map(|(name, inputs)| {
                let drv = Derivation {
                    name: name.to_string(),
                    outputs: [(
                        "out".to_string(),
                        DerivationOutput::InputAddressed(sp(&format!("{name}-out"))),
                    )]
                    .into(),
                    input_drvs: inputs
                        .iter()
                        .map(|i| (sp(&format!("{i}.drv")), InputNode::default()))
                        .collect(),
                    ..Derivation::default()
                };
                (sp(&format!("{name}.drv")), drv)
            })
            .collect()
    }

    fn run(
        drvs: &BTreeMap<StorePath, Derivation>,
        roots: &[&str],
        valid: &[&str],
    ) -> (Vec<String>, Vec<Vec<String>>) {
        run_raw(
            drvs,
            roots,
            &valid.iter().map(|v| format!("{v}-out")).collect::<Vec<_>>(),
        )
    }

    fn run_raw(
        drvs: &BTreeMap<StorePath, Derivation>,
        roots: &[&str],
        valid: &[String],
    ) -> (Vec<String>, Vec<Vec<String>>) {
        let roots: Vec<StorePath> = roots.iter().map(|r| sp(&format!("{r}.drv"))).collect();
        let valid: HashSet<String> = valid
            .iter()
            .map(|v| sp(v).base_name().to_string())
            .collect();
        let mut calls = Vec::new();
        let needs = missing_drvs(drvs, roots.iter().collect(), &mut |names| {
            let mut asked: Vec<String> = names.iter().map(|n| n[33..].to_string()).collect();
            asked.sort();
            calls.push(asked);
            Ok(names
                .iter()
                .filter(|n| valid.contains(*n))
                .cloned()
                .collect())
        })
        .unwrap();
        let mut needs: Vec<String> = needs.iter().map(|p| drvs[*p].name.clone()).collect();
        needs.sort();
        (needs, calls)
    }

    const CHAIN: &[(&str, &[&str])] = &[
        ("root", &["a"]),
        ("a", &["b"]),
        ("b", &["c"]),
        ("c", &["d"]),
        ("d", &[]),
    ];

    #[test]
    fn a_built_root_asks_nothing_below_it() {
        let (needs, calls) = run(&graph(CHAIN), &["root"], &["root", "a", "b", "c", "d"]);
        assert!(needs.is_empty());
        assert_eq!(calls, vec![vec!["root-out"]]);
    }

    #[test]
    fn a_built_input_stops_the_walk() {
        let (needs, calls) = run(&graph(CHAIN), &["root"], &["a", "b", "c", "d"]);
        assert_eq!(needs, vec!["root"]);
        assert_eq!(calls, vec![vec!["root-out"], vec!["a-out"]]);
    }

    #[test]
    fn a_cold_store_is_asked_the_rest_in_one_call() {
        let (needs, calls) = run(&graph(CHAIN), &["root"], &[]);
        assert_eq!(needs, vec!["a", "b", "c", "d", "root"]);
        assert_eq!(
            calls,
            vec![vec!["root-out"], vec!["a-out"], vec!["b-out", "c-out", "d-out"]]
        );
    }

    #[test]
    fn an_input_below_a_built_one_is_not_built() {
        let (needs, calls) = run(&graph(CHAIN), &["root"], &["c"]);
        assert_eq!(needs, vec!["a", "b", "root"]);
        assert_eq!(
            calls,
            vec![vec!["root-out"], vec!["a-out"], vec!["b-out", "c-out", "d-out"]]
        );
    }

    /// `mid` uses only `dep`'s `out`, and `mid` itself is held: `dep`'s
    /// missing `dev`, which only `mid` uses, must not mark `dep` missing.
    /// Needs travel down through missing derivations only.
    fn held_mid() -> BTreeMap<StorePath, Derivation> {
        let mut drvs = BTreeMap::new();
        let out =
            |n: &str, p: &str| (n.to_string(), DerivationOutput::InputAddressed(sp(p)));
        let dep = Derivation {
            name: "dep".into(),
            outputs: [out("out", "dep-out"), out("dev", "dep-dev")].into(),
            ..Derivation::default()
        };
        let mid = Derivation {
            name: "mid".into(),
            outputs: [out("out", "mid-out")].into(),
            input_drvs: [(
                sp("dep.drv"),
                InputNode::of(BTreeSet::from(["out".to_string(), "dev".to_string()])),
            )]
            .into(),
            ..Derivation::default()
        };
        let root = Derivation {
            name: "root".into(),
            outputs: [out("out", "root-out")].into(),
            input_drvs: [(
                sp("mid.drv"),
                InputNode::of(BTreeSet::from(["out".to_string()])),
            )]
            .into(),
            ..Derivation::default()
        };
        drvs.insert(sp("dep.drv"), dep);
        drvs.insert(sp("mid.drv"), mid);
        drvs.insert(sp("root.drv"), root);
        drvs
    }

    #[test]
    fn a_held_drv_does_not_pass_needs_below_it() {
        // `root-out` missing, `mid-out` valid: only `root` builds, and the
        // walk never reaches `dep`, however missing `dep-dev` is
        let (needs, calls) = run_raw(&held_mid(), &["root"], &["mid-out".into()]);
        assert_eq!(needs, vec!["root"]);
        assert_eq!(calls, vec![vec!["root-out"], vec!["mid-out"]]);
    }

    /// `dep` has `out` (used by `root`) and `dev` (used by nobody). `dev`
    /// missing from the store does not mark `dep` missing and does not pull
    /// in what only `dev` needs.
    fn two_outputs(used: &[&str]) -> BTreeMap<StorePath, Derivation> {
        let mut drvs = BTreeMap::new();
        let out =
            |n: &str| (n.to_string(), DerivationOutput::InputAddressed(sp(&format!("dep-{n}"))));
        let dep = Derivation {
            name: "dep".into(),
            outputs: [out("out"), out("dev")].into(),
            ..Derivation::default()
        };
        let root = Derivation {
            name: "root".into(),
            outputs: [("out".to_string(), DerivationOutput::InputAddressed(sp("root-out")))].into(),
            input_drvs: [(
                sp("dep.drv"),
                InputNode::of(used.iter().map(|s| s.to_string()).collect()),
            )]
            .into(),
            ..Derivation::default()
        };
        drvs.insert(sp("dep.drv"), dep);
        drvs.insert(sp("root.drv"), root);
        drvs
    }

    #[test]
    fn an_output_nobody_uses_does_not_mark_its_drv_missing() {
        // `dep-dev` missing, `dep-out` and `root-out` missing: only the used
        // outputs count, so both drvs are still missing
        let (needs, calls) = run_raw(&two_outputs(&["out"]), &["root"], &[]);
        assert_eq!(needs, vec!["dep", "root"]);
        assert_eq!(calls, vec![vec!["root-out"], vec!["dep-out"]]);
    }

    #[test]
    fn an_unused_missing_output_does_not_pull_in_its_drv() {
        // everything the build needs is valid; only `dep-dev`, which nothing
        // uses, is missing: nothing to build, and nothing below asked about
        let (needs, calls) = run_raw(
            &two_outputs(&["out"]),
            &["root"],
            &["root-out".into(), "dep-out".into()],
        );
        assert!(needs.is_empty());
        assert_eq!(calls, vec![vec!["root-out"]]);
    }

    #[test]
    fn a_used_missing_output_below_a_missing_root_is_built() {
        // `dep-out` is used and missing: `dep` joins `root` in the build set,
        // while the unused `dep-dev` is never even asked about
        let (needs, calls) = run_raw(
            &two_outputs(&["out"]),
            &["root"],
            &["dep-dev".into()],
        );
        assert_eq!(needs, vec!["dep", "root"]);
        assert_eq!(calls, vec![vec!["root-out"], vec!["dep-out"]]);
    }

    #[test]
    fn a_used_output_counts_when_the_edge_names_it() {
        // the edge uses both outputs and `dep-dev` is missing: `dep` is
        // missing too, and this time `dep-dev` is asked about
        let (needs, calls) = run_raw(&two_outputs(&["dev", "out"]), &["root"], &[]);
        assert_eq!(needs, vec!["dep", "root"]);
        assert_eq!(calls, vec![vec!["root-out"], vec!["dep-dev", "dep-out"]]);
    }

    #[test]
    fn a_held_root_is_not_reached_below() {
        // `dep-out` missing while `root-out` is valid: the root is held and
        // answers for its closure, so nothing below is asked about either
        let (needs, calls) = run_raw(&two_outputs(&["out"]), &["root"], &["root-out".into()]);
        assert!(needs.is_empty());
        assert_eq!(calls, vec![vec!["root-out"]]);
    }
}
