//! Getting what a batch of derivations needs onto the builder, over the
//! backend the builds themselves use.
//!
//! Two shapes. `PlanUpload` ships a `remote::plan::Plan` computed once, for
//! the commands that evaluate to a fixed set of roots before they build
//! anything (`iets build`, `nix-build`, `nix build`). `StreamUpload` is
//! incremental, for the eval-jobs walk that hands over jobs while it runs:
//! per batch it walks only the part of the closure it has not registered
//! yet, one `QueryValidPaths` and one `AddMultipleToStore`. The walk
//! duplicates `remote::plan::plan` on purpose: `plan` has no way to prune
//! at a drv already known valid, and re-planning a whole closure per batch
//! is quadratic on a big `checks` set.
//!
//! Neither shape knows the transport: both speak `build::backend::Backend`,
//! so the same upload runs over the worker protocol and over `grpc://`.

use std::collections::{HashMap, HashSet};
use std::slice;
use std::time::{Duration, Instant};

use crate::builtins::derivation::DrvInfo;
use crate::error::{Error, Result};
use crate::eval::Known;
use crate::locate;
use crate::parallel::shared::SharedMap;
use crate::remote::event::Event;
use crate::remote::plan::{self, Locator, Node, Object, Plan};
use crate::srcpath::SourceOrigin;
use crate::store::path::StorePath;

use super::backend::{Backend, Drv, Phase};

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub batches: usize,
    pub queried: usize,
    pub added: usize,
    pub bytes: u64,
    pub substituted: usize,
    pub time: Duration,
}

impl Stats {
    pub fn merge(&mut self, o: Stats) {
        self.batches += o.batches;
        self.queried += o.queried;
        self.added += o.added;
        self.bytes += o.bytes;
        self.substituted += o.substituted;
        self.time += o.time;
    }
}

/// Registers everything `roots` need that the builder lacks.
pub trait Uploader {
    fn upload(
        &mut self,
        b: &mut dyn Backend,
        roots: &[StorePath],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Stats>;

    /// The derivations `roots` name, and with `deep` everything they need as
    /// well, references before referrers: what a backend that reports outputs
    /// itself, or schedules the builds itself, drives. The worker backend never
    /// asks, so neither walk costs anything there.
    fn drvs(&self, roots: &[StorePath], deep: bool) -> Result<Vec<Drv>>;

    /// The store was asked about `roots` already and holds `valid` of them.
    fn roots_answered(&mut self, roots: &[StorePath], valid: &HashSet<String>);
}

/// A closure planned once, shipped on the first call.
pub struct PlanUpload<'a> {
    pub plan: &'a Plan,
    pub locate: &'a Locator<'a>,
    done: bool,
    answered: Option<HashSet<String>>,
}

impl<'a> PlanUpload<'a> {
    pub fn new(plan: &'a Plan, locate: &'a Locator<'a>) -> PlanUpload<'a> {
        PlanUpload {
            plan,
            locate,
            done: false,
            answered: None,
        }
    }
}

impl Uploader for PlanUpload<'_> {
    fn upload(
        &mut self,
        b: &mut dyn Backend,
        roots: &[StorePath],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Stats> {
        if self.done || roots.is_empty() {
            return Ok(Stats::default());
        }
        self.done = true;
        let t0 = Instant::now();
        let plan = self.plan;
        let mut queried = 0;
        let missing = plan.query_missing(roots, self.answered.as_ref(), &mut |paths| {
            queried += paths.len();
            b.query_valid(&paths.iter().collect::<Vec<_>>(), on_event)
        })?;
        let missing_sources: Vec<StorePath> = plan
            .sources
            .iter()
            .filter(|p| missing.contains(p))
            .cloned()
            .collect();
        let missing_drvs: Vec<&Object> = plan
            .drvs
            .iter()
            .filter(|o| missing.contains(o.store_path()))
            .collect();

        let t1 = Instant::now();
        let resolved = plan.resolve(&missing_sources, self.locate)?;
        b.note_upload(Phase {
            missing: missing_sources.len() + missing_drvs.len(),
            resolve: t1.elapsed(),
            resolve_paths: resolved.objects.len(),
            store_sources: resolved.store_sources,
        });

        b.substitute(&resolved.substitute, on_event)?;

        let missing: Vec<&Object> = resolved.objects.iter().chain(missing_drvs).collect();
        let bytes = b.add_objects(&missing, on_event)?;
        Ok(Stats {
            batches: 1,
            queried,
            added: missing.len(),
            bytes,
            substituted: resolved.substitute.len(),
            time: t0.elapsed(),
        })
    }

    fn roots_answered(&mut self, _roots: &[StorePath], valid: &HashSet<String>) {
        self.answered = Some(valid.clone());
    }

    /// The plan is the closure of every root, in post-order already.
    fn drvs(&self, roots: &[StorePath], deep: bool) -> Result<Vec<Drv>> {
        let wanted: HashSet<&StorePath> = roots.iter().collect();
        Ok(self
            .plan
            .drvs
            .iter()
            .filter_map(|o| match o {
                Object::Drv { path, aterm, .. } if deep || wanted.contains(path) => Some(Drv {
                    path: path.clone(),
                    aterm: aterm.clone(),
                }),
                _ => None,
            })
            .collect())
    }
}

/// The incremental one: the evaluation's tables plus what the builder is
/// already known to hold.
pub struct StreamUpload {
    drvs: SharedMap<StorePath, DrvInfo>,
    sources: SharedMap<StorePath, Known>,
    valid: HashSet<StorePath>,
}

impl StreamUpload {
    pub fn new(
        drvs: SharedMap<StorePath, DrvInfo>,
        sources: SharedMap<StorePath, Known>,
    ) -> StreamUpload {
        StreamUpload {
            drvs,
            sources,
            valid: HashSet::new(),
        }
    }

    /// The drvs `roots` need and the sources they name, pruned at `known`
    /// (`plan::walk`).
    fn walk(
        &self,
        roots: &[StorePath],
        known: &HashSet<StorePath>,
    ) -> Result<(Vec<Node>, Vec<StorePath>)> {
        let (drvs, srcs, _) = plan::walk(roots, known, &|p| self.drvs.get(p), &|p| {
            self.sources.origin(p)?.text_refs().cloned()
        })?;
        Ok((drvs, srcs))
    }

    /// Where to read a source and how it was addressed: from the evaluation,
    /// else from the local store, else `None` and the builder substitutes it.
    fn source_info(&self, p: &StorePath) -> Result<Option<SourceOrigin>> {
        if let Some(origin) = self.sources.origin(p) {
            return Ok(Some(origin));
        }
        let found = locate::locate_in_store(slice::from_ref(p), &HashMap::new())?;
        Ok(found.into_iter().next().flatten())
    }
}

impl Uploader for StreamUpload {
    fn upload(
        &mut self,
        b: &mut dyn Backend,
        roots: &[StorePath],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Stats> {
        let t0 = Instant::now();
        let mut st = Stats {
            batches: 1,
            ..Stats::default()
        };
        let (drvs, srcs) = self.walk(roots, &self.valid)?;
        let paths: Vec<&StorePath> = srcs.iter().chain(drvs.iter().map(|d| &d.0)).collect();
        if paths.is_empty() {
            st.time = t0.elapsed();
            return Ok(st);
        }
        st.queried = paths.len();
        let valid = b.query_valid(&paths, on_event)?;

        let t1 = Instant::now();
        let mut objects: Vec<Object> = Vec::new();
        let mut substitute: Vec<StorePath> = Vec::new();
        for p in &srcs {
            if valid.contains(p.base_name()) {
                continue;
            }
            match self.source_info(p)? {
                Some(info) => objects.push(
                    Object::source(p.clone(), info)
                        .map_err(|e| Error::io(format!("cannot dump '{p}'"), e))?,
                ),
                None => substitute.push(p.clone()),
            }
        }
        let missing_drvs = drvs
            .iter()
            .filter(|(p, _, _)| !valid.contains(p.base_name()))
            .count();
        b.note_upload(Phase {
            missing: objects.len() + substitute.len() + missing_drvs,
            resolve: t1.elapsed(),
            resolve_paths: objects.len(),
            // A streamed batch never prints `--timings`.
            store_sources: 0,
        });

        // A path registers only once its references are valid, so the
        // substituted sources must land before the drvs that name them.
        st.substituted = substitute.len();
        b.substitute(&substitute, on_event)?;

        for (path, aterm, references) in &drvs {
            if valid.contains(path.base_name()) {
                continue;
            }
            objects.push(Object::drv(path.clone(), aterm.clone(), references.clone()));
        }
        let refs: Vec<&Object> = objects.iter().collect();
        st.bytes = b.add_objects(&refs, on_event)?;
        st.added = objects.len();
        self.valid.extend(srcs);
        self.valid.extend(drvs.into_iter().map(|d| d.0));
        st.time = t0.elapsed();
        Ok(st)
    }

    /// A held root prunes the walk at it, as a registered one does.
    fn roots_answered(&mut self, roots: &[StorePath], valid: &HashSet<String>) {
        let held = roots.iter().filter(|r| valid.contains(r.base_name()));
        self.valid.extend(held.cloned());
    }

    fn drvs(&self, roots: &[StorePath], deep: bool) -> Result<Vec<Drv>> {
        if !deep {
            return roots
                .iter()
                .map(|p| {
                    plan::load_drv(p, &|p| self.drvs.get(p)).map(|(aterm, ..)| Drv {
                        path: p.clone(),
                        aterm,
                    })
                })
                .collect();
        }
        // No pruning: a derivation whose `.drv` the store already has still
        // needs building when its outputs are missing.
        let (drvs, _) = self.walk(roots, &HashSet::new())?;
        Ok(drvs
            .into_iter()
            .map(|(path, aterm, _)| Drv { path, aterm })
            .collect())
    }
}
