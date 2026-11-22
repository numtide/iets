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

use std::collections::{BTreeMap, HashMap, HashSet};
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
use crate::remote::plan::{self, Object, Plan};
use crate::remote::worker::{Connection, failed_event};
use crate::store::drv::Derivation;
use crate::store::path::StorePath;

use super::build::{self, Limits, Objects, Timings};
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
    limits: Limits,
}

impl Grpc {
    pub fn open(cfg: &GrpcConfig, limits: Limits) -> Result<Grpc> {
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
            limits,
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
    limits: Limits,
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
                .map(|(k, _)| (k, j.outputs.as_slice()))
                .ok_or_else(|| Error::Plan(format!("'{}' is not in the batch's closure", j.drv)))
        })
        .collect::<Result<Vec<_>>>()?;
    let needs = missing_drvs(drvs, &roots, &mut |names| {
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
        jobs.push(build::Job::new(drv_path.clone(), drv.clone(), srcs, limits));
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

/// The derivations of the roots' closure the store must build: a root for
/// its `outputs` (empty: all), an input for the outputs a missing referrer
/// uses (nix's `missingForBuild`).
fn missing_drvs<'a>(
    drvs: &'a BTreeMap<StorePath, Derivation>,
    roots: &[(&'a StorePath, &[String])],
    query: &mut dyn FnMut(&[String]) -> Result<HashSet<String>>,
) -> Result<HashSet<&'a StorePath>> {
    let want = |p: &'a StorePath, names: &mut dyn Iterator<Item = &String>| {
        let drv = &drvs[p];
        let paths = names
            .map(|name| match drv.outputs.get(name) {
                Some(o) => o.path(&drv.name, name)?.ok_or_else(|| {
                    Error::Plan(format!(
                        "'{p}': output '{name}' has no statically known path; the build \
                         farm needs statically known input and output paths"
                    ))
                }),
                None => Err(Error::Plan(format!("'{p}' has no output '{name}'"))),
            })
            .collect::<Result<_>>()?;
        Ok((p, paths))
    };
    let roots = roots
        .iter()
        .map(|&(p, outputs)| match outputs {
            [] => want(p, &mut drvs[p].outputs.keys()),
            _ => want(p, &mut outputs.iter()),
        })
        .collect::<Result<_>>()?;
    plan::missing_top_down(
        roots,
        &|p| {
            drvs[p]
                .input_drvs
                .iter()
                .filter_map(|(d, node)| {
                    let (d, _) = drvs.get_key_value(d)?;
                    Some(want(
                        d,
                        &mut node.outputs.iter().chain(node.children.keys()),
                    ))
                })
                .collect()
        },
        None,
        &mut |paths| {
            query(
                &paths
                    .iter()
                    .map(|p| p.base_name().to_string())
                    .collect::<Vec<_>>(),
            )
        },
    )
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
            self.limits,
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

    /// `name.drv` with outputs `name-<output>`, using `inputs` as `(drv, outputs)`.
    fn drv(name: &str, outputs: &[&str], inputs: &[(&str, &[&str])]) -> (StorePath, Derivation) {
        let drv = Derivation {
            name: name.to_string(),
            outputs: outputs
                .iter()
                .map(|o| {
                    let path = sp(&format!("{name}-{o}"));
                    (o.to_string(), DerivationOutput::InputAddressed(path))
                })
                .collect(),
            input_drvs: inputs
                .iter()
                .map(|(i, outs)| {
                    let outs = outs.iter().map(ToString::to_string).collect();
                    (sp(&format!("{i}.drv")), InputNode::of(outs))
                })
                .collect(),
            ..Derivation::default()
        };
        (sp(&format!("{name}.drv")), drv)
    }

    /// The derivations to build and the output names asked, sorted per call.
    fn run(
        drvs: &BTreeMap<StorePath, Derivation>,
        root: &[String],
        valid: &[&str],
    ) -> (Vec<String>, Vec<Vec<String>>) {
        let root_path = sp("root.drv");
        let valid: HashSet<String> = valid
            .iter()
            .map(|v| sp(v).base_name().to_string())
            .collect();
        let mut calls = Vec::new();
        let needs = missing_drvs(drvs, &[(&root_path, root)], &mut |names| {
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

    fn chain() -> BTreeMap<StorePath, Derivation> {
        [
            drv("root", &["out"], &[("a", &["out"])]),
            drv("a", &["out"], &[("b", &["out"])]),
            drv("b", &["out"], &[("c", &["out"])]),
            drv("c", &["out"], &[("d", &["out"])]),
            drv("d", &["out"], &[]),
        ]
        .into()
    }

    /// `root` uses `dep^out` only.
    fn split() -> BTreeMap<StorePath, Derivation> {
        [
            drv("root", &["out", "dev"], &[("dep", &["out"])]),
            drv("dep", &["out", "dev"], &[]),
        ]
        .into()
    }

    #[test]
    fn a_built_root_asks_nothing_below_it() {
        let held = ["root-out", "a-out", "b-out", "c-out", "d-out"];
        let (needs, calls) = run(&chain(), &[], &held);
        assert!(needs.is_empty());
        assert_eq!(calls, [["root-out"]]);
    }

    #[test]
    fn a_built_input_stops_the_walk() {
        let (needs, calls) = run(&chain(), &[], &["a-out", "b-out", "c-out", "d-out"]);
        assert_eq!(needs, ["root"]);
        assert_eq!(calls, [["root-out"], ["a-out"]]);
    }

    #[test]
    fn a_cold_store_is_asked_the_rest_in_one_call() {
        let (needs, calls) = run(&chain(), &[], &[]);
        assert_eq!(needs, ["a", "b", "c", "d", "root"]);
        assert_eq!(
            calls,
            vec![
                vec!["root-out"],
                vec!["a-out"],
                vec!["b-out", "c-out", "d-out"]
            ]
        );
    }

    #[test]
    fn an_input_below_a_built_one_is_not_built() {
        let (needs, calls) = run(&chain(), &[], &["c-out"]);
        assert_eq!(needs, ["a", "b", "root"]);
        assert_eq!(
            calls,
            vec![
                vec!["root-out"],
                vec!["a-out"],
                vec!["b-out", "c-out", "d-out"]
            ]
        );
    }

    #[test]
    fn an_input_output_nobody_uses_is_not_wanted() {
        let (needs, calls) = run(&split(), &[], &["dep-out"]);
        assert_eq!(needs, ["root"]);
        assert_eq!(calls, [vec!["root-dev", "root-out"], vec!["dep-out"]]);
    }

    #[test]
    fn a_root_is_wanted_for_the_requested_outputs_only() {
        let (needs, calls) = run(&split(), &["out".into()], &["root-out"]);
        assert!(needs.is_empty());
        assert_eq!(calls, [["root-out"]]);
    }

    /// `dep^dev` is wanted only through `mid`.
    fn diamond() -> BTreeMap<StorePath, Derivation> {
        [
            drv("root", &["out"], &[("mid", &["out"]), ("dep", &["out"])]),
            drv("mid", &["out"], &[("dep", &["dev"])]),
            drv("dep", &["out", "dev"], &[]),
        ]
        .into()
    }

    #[test]
    fn a_held_drv_wants_nothing_of_its_inputs() {
        let (needs, calls) = run(&diamond(), &[], &["mid-out", "dep-out"]);
        assert_eq!(needs, ["root"]);
        assert_eq!(calls, [vec!["root-out"], vec!["dep-out", "mid-out"]]);
    }

    #[test]
    fn a_later_want_can_make_a_held_input_missing() {
        let (needs, calls) = run(&diamond(), &[], &["dep-out"]);
        assert_eq!(needs, ["dep", "mid", "root"]);
        assert_eq!(
            calls,
            [
                vec!["root-out"],
                vec!["dep-out", "mid-out"],
                vec!["dep-dev"]
            ]
        );
    }

    #[test]
    fn an_unknown_output_is_an_error() {
        let drvs = [drv("root", &["out"], &[])].into();
        let root = sp("root.drv");
        let e = missing_drvs(&drvs, &[(&root, &["bin".into()])], &mut |_| {
            Ok(HashSet::new())
        })
        .unwrap_err();
        assert!(e.to_string().contains("has no output 'bin'"), "{e}");
    }
}
