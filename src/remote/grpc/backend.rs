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

use crate::build::backend::{Backend, Batch, Phase, Status, local};
use crate::build::fanout;
use crate::error::{Error, Result};
use crate::log::{
    ACT_BUILD, ACT_BUILDS, ACT_COPY_PATH, ACT_COPY_PATHS, ACT_REALISE, ACT_UNKNOWN, Activity,
    LVL_ERROR, LVL_INFO, LVL_TALKATIVE, RES_PROGRESS,
};
use crate::remote::event::{Event, Started};
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
                trusted: c.info.trusted,
                calls: 1,
                ..Timings::default()
            },
            c,
            objects: Objects::default(),
            drvs: BTreeMap::new(),
        })
    }
}

/// A `Want` and a `BuildDerivation` per derivation of the batch's closure, at most
/// `max-builds` in flight, a derivation only once every input
/// derivation of it is done.
///
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
    let mut out_names: Vec<String> = Vec::new();
    let mut out_of: Vec<&StorePath> = Vec::new();
    for (drv_path, drv) in drvs {
        for (name, o) in &drv.outputs {
            let Some(p) = o.path(&drv.name, name)? else {
                return Err(Error::Plan(format!(
                    "'{drv_path}': output '{name}' has no statically known path; the build \
                     farm needs statically known input and output paths"
                )));
            };
            out_names.push(p.base_name().to_string());
            out_of.push(drv_path);
        }
    }
    let valid = build::query_valid_paths(c, &out_names, t)?;
    let mut needs: BTreeSet<&StorePath> = BTreeSet::new();
    for (name, drv_path) in out_names.iter().zip(&out_of) {
        if !valid.contains(name) {
            needs.insert(drv_path);
        }
    }

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
        let _ = tx.send(Event::Started(Started { drv_path: drv }));
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
                crate::log::build_exit(res.status, &res.error_msg),
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
        self.t.query_paths += names.len();
        let t0 = Instant::now();
        let valid = build::query_valid_paths(&mut self.c, &names, &mut self.t)?;
        self.t.query += t0.elapsed();
        Ok(valid)
    }

    fn substitute(&mut self, paths: &[StorePath], on_event: &mut dyn FnMut(Event)) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.t.substitute_paths += paths.len();
        let t0 = Instant::now();
        let url = self.c.cfg.url();
        let n = paths.len();
        let _ = on_event;
        let what = format!("substituting {n} paths on '{url}'");
        let _act = Activity::new(LVL_TALKATIVE, ACT_UNKNOWN, &what, vec![]);
        let r = build::substitute(&mut self.c, paths, &mut self.t);
        self.t.substitute += t0.elapsed();
        r
    }

    fn add_objects(&mut self, objects: &[&Object], on_event: &mut dyn FnMut(Event)) -> Result<u64> {
        let _ = on_event;
        if objects.is_empty() {
            return Ok(0);
        }
        let t0 = Instant::now();
        let _copying = crate::remote::copy::Copying::new(objects.len(), "local", &self.c.cfg.url());
        let bytes = build::add_multiple(&mut self.c, objects, &mut self.t)?;
        self.t.add += t0.elapsed();
        self.t.add_paths += objects.len();
        self.t.add_bytes += bytes;
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
        self.t.build += t0.elapsed();
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
        self.t.outputs += t0.elapsed();
        Ok(out)
    }

    fn fetch(&mut self, store_path: &str, dir: &Path) -> Result<()> {
        let t0 = Instant::now();
        let what = format!("copying path '{store_path}' from '{}'", self.c.cfg.url());
        let _act = Activity::new(LVL_INFO, ACT_COPY_PATH, &what, vec![store_path.into()]);
        let paths = [StorePath::parse(store_path)?];
        build::fetch_into_dir(&mut self.c, &paths, dir, &mut self.t)?;
        self.t.fetch += t0.elapsed();
        self.t.fetched += 1;
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
        let mut paths: Vec<StorePath> = Vec::with_capacity(links.len());
        for (p, _) in links {
            paths.push(StorePath::parse(p)?);
        }
        let mut conn = Connection::open(&local(), &mut |_| {})?;
        build::register(&mut self.c, &paths, &mut conn, &mut |_| {})?;
        for (path, link) in links {
            if let Some(link) = link {
                conn.add_perm_root(path, Path::new(link))?;
            }
        }
        Ok(())
    }

    fn note_connect(&mut self, d: Duration) {
        self.t.connect = d;
    }

    fn note_upload(&mut self, p: Phase) {
        self.t.missing += p.missing;
        self.t.resolve += p.resolve;
        self.t.resolve_paths += p.resolve_paths;
        self.t.store_sources += p.store_sources;
    }

    fn report(&self, plan: &Plan, plan_time: Duration) -> String {
        self.t.report(plan, plan_time)
    }
}
