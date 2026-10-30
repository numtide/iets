//! `--check-cache-status`: cache-status-resolver.cc. A job is `local` when
//! every output is valid, `cached` when the missing outputs are all
//! substitutable, else `notBuilt` with the input drvs that will build
//! (post-order) and the outputs a substituter has. Validity and
//! substituter probes are batched: one `QueryValidPaths` and one
//! `QuerySubstitutablePathInfos` round trip per pass over the pending jobs.

use std::collections::{BTreeSet, HashMap};
use std::mem;

use super::line::{Job, Line, json_str, json_str_list};
use crate::error::{Error, Result};
use crate::remote::worker::{Connection, Transport};
use crate::store::drv::{Derivation, DerivationOutput, load_derivation_from_store};
use crate::store::path::StorePath;

pub struct Resolver {
    conn: Connection,
    valid: HashMap<StorePath, bool>,
    probe: HashMap<StorePath, bool>,
    drvs: HashMap<StorePath, Derivation>,
    wanted_valid: BTreeSet<StorePath>,
    wanted: BTreeSet<StorePath>,
    attempt_complete: bool,
    pub round_trips: usize,
}

#[derive(Default)]
struct Traversal {
    visited: BTreeSet<StorePath>,
    needed_builds: Vec<StorePath>,
    substitute_paths: BTreeSet<StorePath>,
}

struct MissingOutputs {
    known: bool,
    complete: bool,
    missing: Vec<StorePath>,
}

/// Deterministic output order: by name, then full path.
fn sort_paths(paths: &mut [StorePath]) {
    paths.sort_by(|a, b| {
        a.name()
            .cmp(b.name())
            .then_with(|| a.to_string().cmp(&b.to_string()))
    });
}

impl Resolver {
    pub fn open(transport: &Transport) -> Result<Resolver> {
        let conn = Connection::open(transport, &mut |_| {})?;
        Ok(Resolver::new(conn))
    }

    pub fn new(conn: Connection) -> Resolver {
        Resolver {
            conn,
            valid: HashMap::new(),
            probe: HashMap::new(),
            drvs: HashMap::new(),
            wanted_valid: BTreeSet::new(),
            wanted: BTreeSet::new(),
            attempt_complete: true,
            round_trips: 0,
        }
    }

    /// Fills the cache-status fields of every job in `lines`.
    pub fn resolve(&mut self, lines: &mut [Line]) -> Result<()> {
        let mut left: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| matches!(l, Line::Job(_)))
            .map(|(i, _)| i)
            .collect();
        while !left.is_empty() {
            let mut still = Vec::new();
            for i in left {
                let Line::Job(job) = &mut lines[i] else {
                    continue;
                };
                if !self.try_resolve(job)? {
                    still.push(i);
                }
            }
            left = still;
            if left.is_empty() {
                break;
            }
            if self.wanted_valid.is_empty() && self.wanted.is_empty() {
                return Err(Error::Cache(
                    "cache status resolver made no progress".into(),
                ));
            }
            self.resolve_wanted()?;
        }
        Ok(())
    }

    /// One `QueryValidPaths` and one `QuerySubstitutablePathInfos` for
    /// everything the last pass asked for.
    fn resolve_wanted(&mut self) -> Result<()> {
        if !self.wanted_valid.is_empty() {
            let batch = mem::take(&mut self.wanted_valid);
            let paths: Vec<String> = batch.iter().map(|p| p.to_string()).collect();
            let valid: BTreeSet<String> = self
                .conn
                .query_valid_paths(&paths, &mut |_| {})?
                .into_iter()
                .collect();
            self.round_trips += 1;
            for p in batch {
                let is = valid.contains(&p.to_string());
                self.valid.insert(p, is);
            }
        }
        if self.wanted.is_empty() {
            return Ok(());
        }
        let batch = mem::take(&mut self.wanted);
        let paths: Vec<String> = batch.iter().map(|p| p.to_string()).collect();
        // Unreachable substituters count as misses.
        let infos: BTreeSet<String> = self
            .conn
            .query_substitutable_path_infos(&paths, &mut |_| {})
            .unwrap_or_default()
            .into_iter()
            .collect();
        self.round_trips += 1;
        for p in batch {
            let has = infos.contains(&p.to_string());
            self.probe.insert(p, has);
        }
        Ok(())
    }

    fn all_substitutable(&mut self, paths: &[StorePath]) -> Option<bool> {
        let mut all = true;
        let mut complete = true;
        for p in paths {
            match self.probe.get(p) {
                Some(has) => all = all && *has,
                None => {
                    self.wanted.insert(p.clone());
                    self.attempt_complete = false;
                    complete = false;
                }
            }
        }
        if !complete {
            return None;
        }
        Some(all)
    }

    fn probe_validity(&mut self, p: &StorePath) -> Option<bool> {
        if let Some(v) = self.valid.get(p) {
            return Some(*v);
        }
        self.wanted_valid.insert(p.clone());
        self.attempt_complete = false;
        None
    }

    fn read_derivation(&mut self, p: &StorePath) -> Result<&Derivation> {
        if !self.drvs.contains_key(p) {
            let drv = load_derivation_from_store(p)?;
            self.drvs.insert(p.clone(), drv);
        }
        Ok(&self.drvs[p])
    }

    /// The wanted (or all) output paths of `drv` that are not valid.
    fn missing_outputs(
        &mut self,
        drv: &Derivation,
        wanted: &BTreeSet<String>,
    ) -> Result<MissingOutputs> {
        let mut r = MissingOutputs {
            known: true,
            complete: true,
            missing: Vec::new(),
        };
        for (name, out) in &drv.outputs {
            if !wanted.is_empty() && !wanted.contains(name) {
                continue;
            }
            let path = match out {
                DerivationOutput::InputAddressed(p) => p.clone(),
                DerivationOutput::Fixed { .. } => drv.fixed_output_path(name)?,
                _ => {
                    r.known = false;
                    return Ok(r);
                }
            };
            match self.probe_validity(&path) {
                None => r.complete = false,
                Some(false) => r.missing.push(path),
                Some(true) => {}
            }
        }
        Ok(r)
    }

    fn visit_drv(
        &mut self,
        t: &mut Traversal,
        drv_path: &StorePath,
        wanted: &BTreeSet<String>,
    ) -> Result<()> {
        if !t.visited.insert(drv_path.clone()) {
            return Ok(());
        }
        let drv = self.read_derivation(drv_path)?.clone();
        let outputs = self.missing_outputs(&drv, wanted)?;
        if !outputs.known {
            return Ok(());
        }
        // Validity probes are cheap: recurse anyway so one round covers
        // the closure.
        if !outputs.complete {
            for (p, names) in &drv.input_drvs {
                self.visit_drv(t, p, &names.outputs)?;
            }
            return Ok(());
        }
        if outputs.missing.is_empty() {
            return Ok(());
        }
        // Substituter probes are per-path requests: a substitutable
        // derivation's inputs are never probed.
        let Some(substitutable) = self.all_substitutable(&outputs.missing) else {
            return Ok(());
        };
        if substitutable {
            t.substitute_paths.extend(outputs.missing);
            return Ok(());
        }
        for (p, names) in &drv.input_drvs {
            self.visit_drv(t, p, &names.outputs)?;
        }
        t.needed_builds.push(drv_path.clone());
        Ok(())
    }

    /// One attempt with the probes cached so far; `false` when a probe is
    /// still pending (the job is retried after `resolve_wanted`).
    fn try_resolve(&mut self, job: &mut Job) -> Result<bool> {
        self.attempt_complete = true;
        let mut missing: Vec<StorePath> = Vec::new();
        let mut known = true;
        let mut complete = true;
        for path in job.outputs.values() {
            let Some(path) = path else {
                known = false;
                break;
            };
            match self.probe_validity(path) {
                None => complete = false,
                Some(false) => missing.push(path.clone()),
                Some(true) => {}
            }
        }
        if known {
            if !complete {
                return Ok(false);
            }
            let Some(all) = self.all_substitutable(&missing) else {
                return Ok(false);
            };
            if all {
                sort_paths(&mut missing);
                let status = if missing.is_empty() {
                    "local"
                } else {
                    "cached"
                };
                set_status(job, status, &[], &missing);
                return Ok(true);
            }
        }
        let mut t = Traversal::default();
        self.visit_drv(&mut t, &job.drv_path.clone(), &BTreeSet::new())?;
        if !self.attempt_complete {
            return Ok(false);
        }
        let mut substitutes: Vec<StorePath> = t.substitute_paths.into_iter().collect();
        sort_paths(&mut substitutes);
        set_status(job, "notBuilt", &t.needed_builds, &substitutes);
        Ok(true)
    }
}

fn set_status(job: &mut Job, status: &str, builds: &[StorePath], substitutes: &[StorePath]) {
    let cached = status != "notBuilt";
    job.extra.insert(
        "isCached".into(),
        if cached {
            b"true".to_vec()
        } else {
            b"false".to_vec()
        },
    );
    job.extra.insert("cacheStatus".into(), json_str(status));
    job.extra.insert(
        "neededBuilds".into(),
        json_str_list(builds.iter().map(|p| p.to_string())),
    );
    job.extra.insert(
        "neededSubstitutes".into(),
        json_str_list(substitutes.iter().map(|p| p.to_string())),
    );
}
