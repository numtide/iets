//! One run of one command: the evaluator configuration, the tracing cache and
//! the parallel entry. A frontend that owns one of those parts — `eval-jobs`
//! its worker processes, a classic command its settings — starts from
//! [`Run::on`] and keeps only what it needs (docs/explanation/plumbing-and-porcelain.md).

use crate::eval::{ErrorKind, EvalConfig, Evaluator};
use crate::parallel;
use crate::parallel::fiber::StackError;
use crate::session::Session;
use crate::store::path::StorePath;
use crate::trace::run::Cache;
use crate::trace::store::Part;

use super::build::{self, Plan};
use super::{EvalArgs, project_root, report, setup_cache, setup_cache_more};
use crate::error::{self, Error};

/// What one run produced: what the caller asked for, an error that was
/// reported and its exit code, or something this frontend does not serve,
/// which classic mode answers by exec'ing the real Nix.
pub enum Outcome<T> {
    Ok(T),
    Failed(i32),
    Unsupported(String),
}

/// A build entry's result: each root derivation and the outputs wanted of it.
pub type Roots = Vec<(StorePath, Option<String>)>;

pub struct Run {
    /// The evaluator is built from it once, in `go`.
    pub cfg: EvalConfig,
    /// `None` when the cache is off, the arguments cannot be keyed, or the
    /// command asked for none.
    pub cache: Option<Cache>,
    pub workers: usize,
}

impl Run {
    /// The configuration `a` asks for. `Err` is the exit code of a setting
    /// that does not resolve, a lint selector or `--dump-drvs`.
    ///
    /// A native command evaluates purely unless a flag or the user's `pure`
    /// setting says otherwise. The classic names keep Nix's own defaults, so
    /// one command line answers the same whichever binary `PATH` found
    /// (`cli::classic`, docs/reference/settings.md).
    pub fn new(a: &EvalArgs) -> Result<Run, i32> {
        let pure = a.pure.unwrap_or_else(|| Session::current().settings().pure);
        Run::over(
            EvalConfig::new().pure(pure).project_root(project_root(a)),
            a,
        )
    }

    /// [`Run::new`] on a configuration that has the system and `pure-eval`
    /// of the frontend.
    pub fn over(base: EvalConfig, a: &EvalArgs) -> Result<Run, i32> {
        match a.config(base) {
            Ok(cfg) => Ok(Run::on(cfg).workers(a.workers)),
            Err(e) => Err(error::Error::Eval(e).report()),
        }
    }

    /// A configuration the frontend made itself; its lints stay as they are,
    /// which is how the classic commands keep them off.
    pub fn on(cfg: EvalConfig) -> Run {
        Run {
            cfg,
            cache: None,
            workers: parallel::default_workers(),
        }
    }

    /// Opens the tracing cache of `subcommand`, keyed on `a`.
    pub fn cached(mut self, a: &EvalArgs, subcommand: &str) -> Run {
        self.cache = setup_cache(a, subcommand, &self.cfg);
        self
    }

    /// [`Run::cached`] with the installables after the first in the key
    /// (`api::setup_cache_more`).
    pub fn cached_more(mut self, a: &EvalArgs, subcommand: &str, more: &[String]) -> Run {
        self.cache = setup_cache_more(a, subcommand, &self.cfg, more);
        self
    }

    pub fn workers(mut self, n: usize) -> Run {
        self.workers = n;
        self
    }

    /// The cache entry `f` accepts, when this run has one and it validated.
    /// `f` returning `None` is a miss, so the run goes on.
    pub fn hit<T>(&self, f: impl FnOnce(&Cache) -> Option<T>) -> Option<T> {
        let c = self.cache.as_ref()?;
        if !c.lookup() {
            return None;
        }
        f(c)
    }

    /// The result bytes of the entry this run validated: what the command
    /// wrote with `Cache::write` last time, when the read set still holds.
    pub fn hit_result(&self) -> Option<Vec<u8>> {
        self.hit(|c| c.part(Part::Result))
    }

    /// A `build` entry this run validated: the roots `Cache::write_build`
    /// stored and the upload plan its tables make. A plan that cannot be made
    /// is a miss, so the evaluation reports it.
    pub fn hit_plan(&self) -> Option<(Roots, Plan)> {
        self.hit(|c| {
            let (roots, drvs, sources) = c.read_build()?;
            let paths: Vec<StorePath> = roots.iter().map(|(p, _)| p.clone()).collect();
            let plan = build::plan_with(&paths, &drvs, &sources).ok()?;
            c.replay_warnings();
            Some((roots, plan))
        })
    }

    /// `f` as the main fiber of `workers` evaluator threads, with the trace
    /// on when there is a cache entry to write. The error is the fiber
    /// stacks: the reservation is over the cap, or the kernel refused it.
    pub fn go<R: Send + 'static>(
        self,
        f: impl FnOnce(&mut Evaluator, Option<Cache>) -> R + Send + 'static,
    ) -> Result<R, StackError> {
        let store = self.cache.as_ref().map(|c| &c.store);
        let ev = self.cfg.trace_in(store).build();
        let cache = self.cache;
        parallel::try_run(self.workers, ev, move |ev| f(ev, cache))
    }

    /// [`Run::go`] with the error classified: what this frontend does not
    /// serve becomes `Unsupported`, anything else is reported once.
    pub fn eval<T: Send + 'static>(
        self,
        f: impl FnOnce(&mut Evaluator, Option<Cache>) -> error::Result<T> + Send + 'static,
    ) -> Result<Outcome<T>, StackError> {
        self.go(move |ev, cache| match f(ev, cache) {
            Ok(t) => Outcome::Ok(t),
            Err(Error::Unsupported(m)) => Outcome::Unsupported(m),
            Err(Error::Eval(e)) if e.kind == ErrorKind::Unsupported => {
                Outcome::Unsupported(e.msg.clone())
            }
            Err(Error::Eval(e)) => Outcome::Failed(report(ev, &e)),
            Err(e) => Outcome::Failed(e.report()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{self, EvalArgs, Input};

    use std::path::Path;
    use std::time::{Duration, Instant};
    use std::{env, fs, thread};

    /// `n` files that each cost real parsing and compiling, and the
    /// expression that imports them all from one list. They are slow on
    /// purpose: a file the main fiber is still compiling is a file the
    /// helpers steal, which is what this has to observe.
    fn workload(dir: &Path, n: usize) -> String {
        fs::create_dir_all(dir).expect("workload directory");
        let mut elems = String::new();
        for i in 0..4000 {
            elems += &format!("{i} ");
        }
        let mut list = String::from("[");
        for i in 0..n {
            let f = dir.join(format!("m{i}.nix"));
            let src = format!("let xs = [ {elems}]; in builtins.length xs + {i}\n");
            fs::write(&f, src).expect("workload file");
            list += &format!(" (import {}) ", f.display());
        }
        list + "]"
    }

    /// Impure: the workloads below import absolute paths under a temporary
    /// directory, and a pure run reads neither those nor the search path.
    fn args(expr: &str, cache_dir: &Path, workers: usize) -> EvalArgs {
        EvalArgs {
            input: Some(Input::Expr(expr.to_string())),
            cache_dir: Some(cache_dir.to_string_lossy().into_owned()),
            no_tracing_cache: true,
            no_ir_cache: false,
            workers,
            pure: Some(false),
            ..EvalArgs::new()
        }
    }

    /// The run's IR cache, and the entries it read while it evaluated `a`
    /// deeply on `a.workers` fibers.
    fn hits(a: EvalArgs) -> u64 {
        Run::new(&a)
            .expect("the arguments configure")
            .go(move |ev, _| {
                let v = api::evaluate(ev, a).expect("the workload evaluates");
                parallel::force_deep(ev, v).expect("the workload forces");
                ev.ir_cache().expect("the run has an IR cache").hits()
            })
            .expect("the fiber stacks")
    }

    /// The fiber template is a clone of the evaluator `go` builds, so a
    /// helper fiber has every setting of the arguments.
    #[test]
    fn a_fiber_template_carries_the_configuration() {
        let d = env::temp_dir().join(format!("iets-run-cfg-{}", std::process::id()));
        let mut a = args("1", &d, 4);
        a.includes = vec![(b"nixpkgs".to_vec(), b"/nonexistent".to_vec())];
        a.lint_roots = vec!["/nonexistent".into()];
        let want = api::lint_config(&a).expect("the lints resolve");
        let run = Run::new(&a).expect("the arguments configure");
        let fiber = run.cfg.build().clone();
        assert!(fiber.ir_cache().is_some(), "a fiber without the IR cache");
        assert_eq!(fiber.search_path()[0], a.includes[0]);
        assert_eq!(*fiber.lint_config(), want);
        let _ = fs::remove_dir_all(&d);
    }

    /// Every fiber shares one IR cache handle, so a warm run reads one entry
    /// per imported file whatever the worker count. With the cache on the
    /// main fiber alone, the helpers compile their imports again and the
    /// four-worker count collapses.
    #[test]
    fn helper_fibers_read_the_ir_cache() {
        let d = env::temp_dir().join(format!("iets-run-ircache-{}", std::process::id()));
        let n = 400;
        let expr = workload(&d.join("src"), n);
        let cache = d.join("cache");
        hits(args(&expr, &cache, 1));
        let one = hits(args(&expr, &cache, 1));
        let four = hits(args(&expr, &cache, 4));
        let _ = fs::remove_dir_all(&d);
        assert_eq!(one, n as u64, "a warm run reads one entry per file");
        assert_eq!(four, one, "helper fibers read the IR cache too");
    }

    /// The derivations this run made, counted after a deep force of the
    /// workload on `a.workers` fibers. A fiber folds its counters when it
    /// parks or finishes its task, so a helper still inside one when the
    /// walk returns is a few counts behind: the read waits for `want`,
    /// yielding, and reports what it has when the deadline passes.
    fn derivations(a: EvalArgs, want: u64) -> u64 {
        Run::new(&a)
            .expect("the arguments configure")
            .go(move |ev, _| {
                let v = api::evaluate(ev, a).expect("the workload evaluates");
                parallel::force_deep(ev, v).expect("the workload forces");
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    let n = ev.counts().derivations;
                    if n >= want || Instant::now() > deadline {
                        return n;
                    }
                    thread::yield_now();
                }
            })
            .expect("the fiber stacks")
    }

    /// Each fiber counts into its own evaluator and folds those counters
    /// into the run's totals, so a stats line does not depend on the worker
    /// count. The derivation count is deterministic; with the main fiber's
    /// counters alone the four-worker number is a fraction of it.
    #[test]
    fn the_counters_cover_the_helper_fibers() {
        let d = env::temp_dir().join(format!("iets-run-counts-{}", std::process::id()));
        let n = 400;
        let expr = format!(
            "builtins.genList (i: derivation {{ name = \"c${{toString i}}\"; \
             system = \"x86_64-linux\"; builder = \"/bin/sh\"; }}) {n}"
        );
        let one = derivations(args(&expr, &d, 1), n as u64);
        let four = derivations(args(&expr, &d, 4), one);
        let _ = fs::remove_dir_all(&d);
        assert_eq!(one, n as u64, "one derivation per list element");
        assert_eq!(four, one, "the helper fibers' derivations are counted too");
    }
}
