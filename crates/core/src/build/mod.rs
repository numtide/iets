//! The one build path. Every iets command that realises a derivation goes
//! through `Dispatcher`: one connection carries the uploads and the builds of
//! a batch of ready derivations, and every build reports through one
//! structured event stream with provenance attached.
//!
//! Three axes, kept apart:
//!
//! - **What to feed it.** `iets build`, `nix-build` and `nix build` hand over
//!   a fixed set of roots (`upload::PlanUpload`). `eval-jobs` instantiation
//!   hands over batches as the walk finishes attributes
//!   (`upload::StreamUpload`). See docs/reference/commands.md.
//! - **How to reach the store.** `backend::Backend`: the worker protocol
//!   sends one `BuildPathsWithResults` per ready set (docs/explanation/history.md: one
//!   op per ready set beats one op per derivation by 7-10x, and the protocol
//!   is serial per connection); `grpc://` fans the set out instead.
//! - **What to say about it.** `render`, from one event stream.

pub mod backend;
pub mod event;
pub mod executor;
pub mod fanout;
pub mod fixhash;
pub mod render;
pub mod upload;

use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, Builder, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::log::{self, SessionOut};
use crate::provenance::Provenance;
use crate::remote::event::Event;
use crate::remote::plan::Plan;
use crate::remote::worker::{Missing, Options, Transport};
use crate::session::{self, Session};
use crate::store::path::StorePath;

use crate::store::drv::{Derivation, DerivationOutput};
use crate::store::hash::{Hash, HashFormat};
use backend::Backend;
use event::{BuildEvent, Kind};
use render::{Renderer, Style};
use upload::Uploader;

/// One derivation the caller asked for, under the name it wants to see.
#[derive(Debug, Clone)]
pub struct Target {
    /// Attribute path, or the derivation name when the caller has none.
    pub attr: String,
    pub drv: StorePath,
    /// Wanted output names; empty means every output (`!*`).
    pub outputs: Vec<String>,
}

impl From<&Target> for backend::Job {
    fn from(t: &Target) -> backend::Job {
        backend::Job {
            drv: t.drv.clone(),
            outputs: t.outputs.clone(),
        }
    }
}

impl Target {
    pub fn new(attr: &str, drv: StorePath, outputs: Vec<String>) -> Target {
        Target {
            attr: attr.to_string(),
            drv,
            outputs,
        }
    }
}

/// What became of one target.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub attr: String,
    pub drv: StorePath,
    pub success: bool,
    pub skipped: bool,
    pub reason: Option<String>,
    pub log_tail: Vec<String>,
    /// Output name to store path, for the targets that built.
    pub outputs: BTreeMap<String, String>,
    /// `log::build_exit` of a failure, 0 otherwise.
    pub exit: i32,
}

/// The heartbeat thread: pokes the renderer so a CI log shows liveness and
/// a stalled build gets escalated even while the client sits in a read.
struct Heartbeat {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Heartbeat {
    fn start(render: Arc<Mutex<Renderer>>) -> Heartbeat {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = Builder::new()
            .name("build-heartbeat".into())
            .spawn(session::bind(move || {
                // parked, not asleep: `drop` unparks it, so a build that
                // took 10 ms does not wait a tick for this thread to exit
                let mut last = Instant::now();
                while !flag.load(Ordering::Relaxed) {
                    let every = render.lock().unwrap().interval();
                    thread::park_timeout(every.saturating_sub(last.elapsed()));
                    if !flag.load(Ordering::Relaxed) && last.elapsed() >= every {
                        last = Instant::now();
                        render.lock().unwrap().heartbeat();
                    }
                }
            }))
            .ok();
        Heartbeat { stop, handle }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.thread().unpark();
            let _ = h.join();
        }
    }
}

/// Where the events go: the renderer.
pub struct Reporter {
    render: Arc<Mutex<Renderer>>,
    /// Whose log shows the live region, so the region comes down with us.
    live: Option<Arc<Session>>,
    /// Per drv: when the build started and how long it ran.
    timing: HashMap<String, (Instant, Option<Duration>)>,
}

impl Reporter {
    pub fn new(style: Style) -> Reporter {
        let mut render = Renderer::new(style, Box::new(SessionOut::default()));
        let mut live = None;
        // under internal-json the activity records carry the progress
        if style.mode == render::Mode::Tty && !style.quiet && !log::json() {
            let s = Session::current();
            s.log.progress.set_live(true);
            let sink = s.clone();
            render = render.with_live(Box::new(move |lines| sink.log.set_region(lines)));
            live = Some(s);
        }
        Reporter {
            render: Arc::new(Mutex::new(render)),
            live,
            timing: HashMap::new(),
        }
    }

    /// The same, writing the progress somewhere else than stderr.
    pub fn with_output(style: Style, out: Box<dyn Write + Send>) -> Reporter {
        Reporter {
            render: Arc::new(Mutex::new(Renderer::new(style, out))),
            live: None,
            timing: HashMap::new(),
        }
    }

    /// The derivations the build op now running reports verdicts for.
    fn owns(&mut self, drvs: impl IntoIterator<Item = String>) {
        self.render.lock().unwrap().owns(drvs);
    }

    fn release(&mut self) {
        self.render.lock().unwrap().release();
    }

    fn emit(&mut self, e: &BuildEvent) {
        self.render.lock().unwrap().event(e);
    }

    fn duration(&self, drv: &str) -> Duration {
        match self.timing.get(drv) {
            Some((_, Some(d))) => *d,
            Some((t, None)) => t.elapsed(),
            None => Duration::ZERO,
        }
    }

    fn tail(&self, drv: &str) -> Vec<String> {
        let r = self.render.lock().unwrap();
        let live = r.live_tail(drv);
        if live.is_empty() {
            r.retained_tail(drv)
        } else {
            live
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.render
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish();
        if let Some(s) = self.live.take() {
            s.log.progress.set_live(false);
        }
    }
}

/// The dispatcher: one backend, one event stream, batched build ops.
pub struct Dispatcher {
    backend: Box<dyn Backend>,
    pub reporter: Reporter,
    prov: Provenance,
    seen: HashSet<StorePath>,
    /// Nix's failingExitStatus of the failed builds, 0 while none failed.
    pub exit: i32,
    pub upload: upload::Stats,
    /// Stops when the dispatcher drops.
    _heartbeat: Heartbeat,
}

impl Dispatcher {
    pub fn open(
        transport: &Transport,
        options: &Options,
        prov: Provenance,
        style: Style,
    ) -> Result<Dispatcher> {
        let reporter = Reporter::new(style);
        let render = reporter.render.clone();
        let t0 = Instant::now();
        let mut backend = {
            let mut first = |e: Event| {
                if let Event::Log(l) = e {
                    render
                        .lock()
                        .unwrap()
                        .note(&String::from_utf8_lossy(&l.line));
                }
            };
            backend::open(transport, options, &mut first)?
        };
        backend.note_connect(t0.elapsed());
        Ok(Dispatcher::with_backend(backend, prov, reporter))
    }

    /// Wraps an already-open backend.
    pub fn with_backend(
        backend: Box<dyn Backend>,
        prov: Provenance,
        reporter: Reporter,
    ) -> Dispatcher {
        let heartbeat = Heartbeat::start(reporter.render.clone());
        Dispatcher {
            backend,
            reporter,
            prov,
            seen: HashSet::new(),
            exit: 0,
            upload: upload::Stats::default(),
            _heartbeat: heartbeat,
        }
    }

    /// Uploads what the batch needs and builds it in one op. Targets whose
    /// drv was already dispatched are dropped.
    pub fn submit(&mut self, up: &mut dyn Uploader, batch: Vec<Target>) -> Result<Vec<Outcome>> {
        let batch: Vec<Target> = batch
            .into_iter()
            .filter(|t| self.seen.insert(t.drv.clone()))
            .collect();
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let out = self.submit_once(up, &batch)?;
        self.exit = (out.iter()).fold(self.exit, |e, o| log::combine_exit(e, o.exit));
        Ok(out)
    }

    fn submit_once(&mut self, up: &mut dyn Uploader, batch: &[Target]) -> Result<Vec<Outcome>> {
        let by_drv: HashMap<String, String> = (batch.iter())
            .map(|t| (t.drv.to_string(), t.attr.clone()))
            .collect();
        let roots: Vec<StorePath> = batch.iter().map(|t| t.drv.clone()).collect();
        // Until this op reports its verdicts, the end of a build's activity
        // decides nothing: with a build hook the daemon runs two activities
        // for one derivation, and neither says whether it succeeded.
        self.reporter.owns(by_drv.keys().cloned());

        let built = static_outputs(up, batch, &roots);
        let held = match &built {
            Some(o) => {
                let mut on_event = event_sink(&mut self.reporter, &self.prov, &by_drv);
                let (held, valid) = already_built(&mut *self.backend, &roots, o, &mut on_event)?;
                up.roots_answered(&roots, &valid);
                held
            }
            None => false,
        };
        let status = match held {
            true => (by_drv.keys())
                .map(|d| (d.clone(), backend::Status::ok()))
                .collect(),
            false => {
                let stats = {
                    let mut on_event = event_sink(&mut self.reporter, &self.prov, &by_drv);
                    up.upload(&mut *self.backend, &roots, &mut on_event)?
                };
                self.upload.merge(stats);

                let jobs: Vec<backend::Job> = batch.iter().map(backend::Job::from).collect();
                let drvs = |deep: bool| up.drvs(&roots, deep);
                let b = backend::Batch {
                    jobs: &jobs,
                    drvs: &drvs,
                };
                let mut on_event = event_sink(&mut self.reporter, &self.prov, &by_drv);
                self.backend.submit(&b, &mut on_event)?
            }
        };

        let names = Names {
            prov: &self.prov,
            by_drv: &by_drv,
        };
        let earlier_failed = status.values().any(|s| !s.ok);
        let mut out = Vec::with_capacity(batch.len());
        for t in batch {
            let drv = t.drv.to_string();
            let elapsed = self.reporter.duration(&drv);
            match status.get(&drv) {
                Some(s) if s.ok => {
                    let map = match built.as_ref().and_then(|b| b.get(&drv)) {
                        Some(m) => m.clone(),
                        None => {
                            let mut on_event = event_sink(&mut self.reporter, &self.prov, &by_drv);
                            self.backend.outputs(&drv, &mut on_event)?
                        }
                    };
                    let outputs: BTreeMap<String, String> = map
                        .into_iter()
                        .filter_map(|(n, p)| p.map(|p| (n, p)))
                        .filter(|(n, _)| t.outputs.is_empty() || t.outputs.contains(n))
                        .collect();
                    self.reporter.emit(&names.event(
                        &drv,
                        Kind::Built {
                            outputs: outputs.clone(),
                        },
                        elapsed,
                    ));
                    out.push(Outcome {
                        attr: t.attr.clone(),
                        drv: t.drv.clone(),
                        success: true,
                        skipped: false,
                        reason: None,
                        log_tail: Vec::new(),
                        outputs,
                        exit: 0,
                    });
                }
                Some(s) => {
                    let tail = self.reporter.tail(&drv);
                    // The daemon's message is several coloured lines; the
                    // records want one plain one, the log tail has the rest.
                    let reason = log::strip_ansi(&self.prov.rewrite_str(&s.reason));
                    let reason = log::reason_line(&reason);
                    self.reporter.emit(&names.event(
                        &drv,
                        Kind::Failed {
                            reason: reason.clone(),
                            log_tail: tail.clone(),
                        },
                        elapsed,
                    ));
                    let full = self.prov.rewrite_str(&s.reason);
                    log::build_failed(names.pos(&drv), &full, s.exit, None);
                    out.push(Outcome {
                        attr: t.attr.clone(),
                        drv: t.drv.clone(),
                        success: false,
                        skipped: false,
                        reason: Some(reason),
                        log_tail: tail,
                        outputs: BTreeMap::new(),
                        exit: s.exit,
                    });
                }
                // No verdict for this target. Either the store stopped before
                // it reached it (`keep-going` off, something earlier failed),
                // or the op came back without it, which is a broken builder.
                // Both leave the target unbuilt, so both exit non-zero.
                None => {
                    let why = if earlier_failed {
                        "an earlier build in the batch failed".to_string()
                    } else {
                        format!("the builder returned no build result for '{drv}'")
                    };
                    self.reporter.emit(&names.event(
                        &drv,
                        Kind::Skipped { why: why.clone() },
                        elapsed,
                    ));
                    out.push(Outcome {
                        attr: t.attr.clone(),
                        drv: t.drv.clone(),
                        success: false,
                        skipped: true,
                        reason: Some(why),
                        log_tail: Vec::new(),
                        outputs: BTreeMap::new(),
                        exit: 1,
                    });
                }
            }
        }
        // A mismatch is a fact about a fixed-output derivation of this batch's
        // closure, from the daemon; anything else that prints the words is
        // dropped. Nix's worker sets the hash-mismatch flag for the whole run,
        // so a root whose dependency mismatched exits 102 as well.
        let planned = OnceCell::new();
        let closure = &*up;
        let kept = log::resolve_mismatches(&|specified| {
            (planned.get_or_init(|| closure.drvs(&roots, true).unwrap_or_default()))
                .iter()
                .any(|d| has_output_hash(d, specified))
        });
        if kept > 0 {
            self.exit = log::combine_exit(self.exit, 102);
        }
        // a batch that built in full built every dependency too
        if earlier_failed || status.len() < batch.len() {
            let held = self.reporter.render.lock().unwrap().held_successes();
            let unbuilt = {
                let mut on_event = event_sink(&mut self.reporter, &self.prov, &by_drv);
                unbuilt(&mut *self.backend, &held, &mut on_event)
            };
            self.reporter.render.lock().unwrap().unfinished(unbuilt);
        }
        self.reporter.release();
        Ok(out)
    }

    /// `--no-build`: register the closure and stop.
    pub fn upload_only(&mut self, up: &mut dyn Uploader, roots: &[StorePath]) -> Result<()> {
        let by_drv = HashMap::new();
        let stats = {
            let mut on_event = event_sink(&mut self.reporter, &self.prov, &by_drv);
            up.upload(&mut *self.backend, roots, &mut on_event)?
        };
        self.upload.merge(stats);
        Ok(())
    }

    /// `--dry-run`: register the closure, then ask what building `targets`
    /// takes.
    pub fn missing(&mut self, up: &mut dyn Uploader, targets: &[Target]) -> Result<Missing> {
        let roots: Vec<StorePath> = targets.iter().map(|t| t.drv.clone()).collect();
        self.upload_only(up, &roots)?;
        let jobs: Vec<backend::Job> = targets.iter().map(backend::Job::from).collect();
        let by_drv = HashMap::new();
        let mut on_event = event_sink(&mut self.reporter, &self.prov, &by_drv);
        self.backend.missing(&jobs, &mut on_event)
    }

    /// An indirect GC root at `link` for `store_path`.
    pub fn out_link(&mut self, store_path: &str, link: &Path) -> Result<()> {
        self.backend.add_root(store_path, link)
    }

    /// `nix build`'s outputs and out-links, on the local daemon whatever the
    /// builder is.
    pub fn out_links(&mut self, links: &[(String, Option<String>)]) -> Result<()> {
        self.backend.out_links(links)
    }

    /// Restores one output of a remote store into `dir`.
    pub fn fetch(&mut self, store_path: &str, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)
            .map_err(|e| Error::io(format!("cannot create {}", dir.display()), e))?;
        let sp = StorePath::parse(store_path)?;
        // symlink_metadata: a dangling symlink is a path that is there, and
        // the restore must not write through it
        if fs::symlink_metadata(dir.join(sp.base_name())).is_ok() {
            return Err(Error::Usage(format!(
                "fetch: '{}' already exists",
                dir.join(sp.base_name()).display()
            )));
        }
        self.backend.fetch(store_path, dir)
    }

    /// `--timings`: the backend's own per-phase report.
    pub fn report(&self, plan: &Plan, plan_time: Duration) -> String {
        self.backend.report(plan, plan_time)
    }
}

/// The wanted output paths of every target, when each one is known without
/// building: input-addressed and fixed outputs. `None` when any is not, or a
/// drv cannot be read; the batch then asks the store as usual.
fn static_outputs(
    up: &dyn Uploader,
    batch: &[Target],
    roots: &[StorePath],
) -> Option<HashMap<String, BTreeMap<String, Option<String>>>> {
    let drvs = up.drvs(roots, false).ok()?;
    let mut all = HashMap::new();
    for t in batch {
        let d = drvs.iter().find(|d| d.path == t.drv)?;
        let parsed = Derivation::parse(&d.aterm, Derivation::name_from_path(&t.drv).ok()?).ok()?;
        if t.outputs.iter().any(|n| !parsed.outputs.contains_key(n)) {
            return None;
        }
        let mut outs = BTreeMap::new();
        for (n, o) in &parsed.outputs {
            if t.outputs.is_empty() || t.outputs.contains(n) {
                let p = o.path(&parsed.name, n).ok()??;
                outs.insert(n.clone(), Some(p.to_string()));
            }
        }
        all.insert(t.drv.to_string(), outs);
    }
    Some(all)
}

/// Whether the store holds every root and every wanted output, so nothing
/// needs to go up or build, and which roots it holds. The daemon would
/// answer the same, but nixbuild.net takes a quarter of a second to. The
/// roots are asked first: a missing drv costs it 7 ms, a missing output
/// 170 ms.
fn already_built(
    b: &mut dyn Backend,
    roots: &[StorePath],
    outputs: &HashMap<String, BTreeMap<String, Option<String>>>,
    on_event: &mut dyn FnMut(Event),
) -> Result<(bool, HashSet<String>)> {
    let refs: Vec<&StorePath> = roots.iter().collect();
    let roots_valid = b.query_valid(&refs, on_event)?;
    if !roots.iter().all(|r| roots_valid.contains(r.base_name())) {
        return Ok((false, roots_valid));
    }
    let Some(paths) = (outputs.values().flat_map(BTreeMap::values).flatten())
        .map(|p| StorePath::parse(p).ok())
        .collect::<Option<Vec<_>>>()
    else {
        return Ok((false, roots_valid));
    };
    let refs: Vec<&StorePath> = paths.iter().collect();
    let valid = b.query_valid(&refs, on_event)?;
    Ok((
        paths.iter().all(|p| valid.contains(p.base_name())),
        roots_valid,
    ))
}

/// The derivations of `drvs` with an output the store does not hold. A
/// derivation the store cannot answer for is left out: its verdict stays
/// what the daemon's messages say.
fn unbuilt(b: &mut dyn Backend, drvs: &[String], on_event: &mut dyn FnMut(Event)) -> Vec<String> {
    let mut unbuilt = Vec::new();
    let mut outputs = Vec::new();
    for d in drvs {
        let Ok(map) = b.outputs(d, on_event) else {
            continue;
        };
        let paths: Option<Vec<StorePath>> = (map.into_values())
            .map(|p| StorePath::parse(&p?).ok())
            .collect();
        match paths {
            Some(p) => outputs.push((d, p)),
            None => unbuilt.push(d.clone()),
        }
    }
    let refs: Vec<&StorePath> = outputs.iter().flat_map(|(_, p)| p).collect();
    let Ok(valid) = b.query_valid(&refs, on_event) else {
        return unbuilt;
    };
    for (d, p) in outputs {
        if !p.iter().all(|p| valid.contains(p.base_name())) {
            unbuilt.push(d.clone());
        }
    }
    unbuilt
}

/// What one derivation is called and where it was defined: the attribute the
/// caller gave for it, else what provenance knows, else the derivation's own
/// name. The position is added in one place, here.
struct Names<'a> {
    prov: &'a Provenance,
    by_drv: &'a HashMap<String, String>,
}

impl Names<'_> {
    fn pos(&self, drv: &str) -> Option<String> {
        self.prov.site(drv).and_then(|s| s.pos.clone())
    }

    fn attr(&self, drv: &str) -> String {
        if let Some(attr) = self.by_drv.get(drv) {
            return attr.clone();
        }
        self.prov
            .attr_of(drv)
            .unwrap_or_else(|| strip_hash(drv).to_string())
    }

    /// `<attr> (<file>:<line>:<col>)` when the position is known and names
    /// this derivation alone.
    fn label(&self, drv: &str) -> String {
        let attr = self.attr(drv);
        match self.prov.site(drv).and_then(|s| s.own_pos()) {
            Some(pos) => format!("{attr} ({pos})"),
            None => attr,
        }
    }

    fn event(&self, drv: &str, kind: Kind, elapsed: Duration) -> BuildEvent {
        BuildEvent::new(&self.label(drv), drv, kind, elapsed).at(self.pos(drv))
    }
}

/// The daemon's stream mapped onto `BuildEvent`s. A free function so the
/// caller keeps the backend borrowed at the same time.
fn event_sink<'a>(
    reporter: &'a mut Reporter,
    prov: &'a Provenance,
    by_drv: &'a HashMap<String, String>,
) -> impl FnMut(Event) + 'a {
    let names = Names { prov, by_drv };
    move |e: Event| match e {
        Event::Started(s) => {
            reporter
                .timing
                .entry(s.drv_path.clone())
                .or_insert((Instant::now(), None));
            reporter.emit(&names.event(&s.drv_path, Kind::Started, Duration::ZERO));
        }
        Event::Phase(p) => {
            let d = reporter.duration(&p.drv_path);
            reporter.emit(&names.event(&p.drv_path, Kind::Phase(p.phase), d));
        }
        Event::Log(l) => {
            let line = prov.rewrite(&l.line);
            let d = reporter.duration(&l.drv_path);
            reporter.emit(&names.event(&l.drv_path, Kind::Log(line), d));
        }
        Event::Stopped(s) => {
            if let Some((t, slot)) = reporter.timing.get_mut(&s.drv_path)
                && slot.is_none()
            {
                *slot = Some(t.elapsed());
            }
            reporter
                .render
                .lock()
                .unwrap()
                .stopped(&s.drv_path, s.built);
        }
        Event::Failed(f) => {
            let d = reporter.duration(&f.drv_path);
            let tail = reporter.tail(&f.drv_path);
            reporter.emit(&names.event(
                &f.drv_path,
                Kind::Failed {
                    reason: prov.rewrite_str(&f.reason),
                    log_tail: tail,
                },
                d,
            ));
        }
    }
}

/// Whether `d` is fixed-output with `specified` as its output hash. The ATerm
/// holds the hash in base 16, so the scan is over bytes and only a derivation
/// that could match is parsed.
fn has_output_hash(d: &backend::Drv, specified: &str) -> bool {
    let Ok(want) = Hash::parse_any(specified, None) else {
        return false;
    };
    let hex = want.to_string_with(HashFormat::Base16, false);
    if !d.aterm.windows(hex.len()).any(|w| w == hex.as_bytes()) {
        return false;
    }
    let Ok(name) = Derivation::name_from_path(&d.path) else {
        return false;
    };
    let Ok(drv) = Derivation::parse(&d.aterm, name) else {
        return false;
    };
    (drv.outputs.values())
        .any(|o| matches!(o, DerivationOutput::Fixed { hash, .. } if *hash == want))
}

/// `<hash>-<name>.drv` as just `<name>`, the way a build log names a
/// derivation that no attribute points at.
fn strip_hash(drv_path: &str) -> &str {
    let base = drv_path.rsplit('/').next().unwrap_or(drv_path);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    match base.split_once('-') {
        Some((hash, rest)) if hash.len() == 32 => rest,
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::render::tests::Sink;
    use super::*;
    use crate::remote::event::{Failed, Log, Started, Stopped};
    use crate::remote::plan::Object;

    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::Sender;

    pub(super) fn drv(name: &str) -> StorePath {
        StorePath::parse(&format!(
            "/nix/store/00000000000000000000000000000000-{name}.drv"
        ))
        .unwrap()
    }

    #[test]
    fn targets_become_jobs() {
        let t = Target::new("a", drv("x"), vec!["out".into(), "dev".into()]);
        let j = backend::Job::from(&t);
        assert_eq!(j.drv, t.drv);
        assert_eq!(j.spec(), "dev,out");
    }

    #[test]
    fn hash_stripping() {
        assert_eq!(strip_hash(&drv("hello").to_string()), "hello");
        assert_eq!(strip_hash("plain"), "plain");
    }

    /// A backend that builds every job of a batch concurrently, the way the
    /// `grpc://` farm does, and tags every line it streams with its drv.
    struct Fan {
        limit: usize,
        peak: Arc<AtomicBool>,
        /// `false`: the op comes back without a verdict for any job, the way
        /// a store that stopped early leaves its targets.
        verdicts: bool,
    }

    impl Backend for Fan {
        fn query_valid(
            &mut self,
            _paths: &[&StorePath],
            _on_event: &mut dyn FnMut(Event),
        ) -> Result<HashSet<String>> {
            Ok(HashSet::new())
        }

        fn substitute(
            &mut self,
            _paths: &[StorePath],
            _on_event: &mut dyn FnMut(Event),
        ) -> Result<()> {
            Ok(())
        }

        fn add_objects(
            &mut self,
            _objects: &[&Object],
            _on_event: &mut dyn FnMut(Event),
        ) -> Result<u64> {
            Ok(0)
        }

        fn submit(
            &mut self,
            batch: &backend::Batch<'_>,
            on_event: &mut dyn FnMut(Event),
        ) -> Result<HashMap<String, backend::Status>> {
            let live = AtomicUsize::new(0);
            let over = self.peak.clone();
            let limit = self.limit;
            let jobs = batch.jobs;
            let build = |i: usize, tx: &Sender<Event>| -> bool {
                let drv = jobs[i].drv.to_string();
                if live.fetch_add(1, Ordering::SeqCst) + 1 > limit {
                    over.store(true, Ordering::SeqCst);
                }
                let _ = tx.send(Event::Started(Started {
                    drv_path: drv.clone(),
                }));
                for n in 0..4 {
                    let _ = tx.send(Event::Log(Log {
                        drv_path: drv.clone(),
                        line: format!("{} line {n}", jobs[i].drv.name()).into_bytes(),
                    }));
                    thread::sleep(Duration::from_millis(2));
                }
                live.fetch_sub(1, Ordering::SeqCst);
                true
            };
            let work = fanout::Work {
                build: &build,
                ok: &|ok: &bool| *ok,
                blocked: &|_, _| false,
            };
            let graph: Vec<fanout::Job> =
                jobs.iter().map(|_| fanout::Job { deps: vec![] }).collect();
            fanout::run(&graph, self.limit, &work, on_event);
            if !self.verdicts {
                return Ok(HashMap::new());
            }
            Ok(jobs
                .iter()
                .map(|j| (j.drv.to_string(), backend::Status::ok()))
                .collect())
        }

        fn outputs(
            &mut self,
            drv: &str,
            _on_event: &mut dyn FnMut(Event),
        ) -> Result<BTreeMap<String, Option<String>>> {
            let p = StorePath::parse(drv)?;
            Ok(BTreeMap::from([(
                "out".to_string(),
                Some(format!("/nix/store/{}", p.base_name().replace(".drv", ""))),
            )]))
        }

        fn fetch(&mut self, _store_path: &str, _dir: &Path) -> Result<()> {
            Ok(())
        }

        fn add_root(&mut self, _store_path: &str, _link: &Path) -> Result<()> {
            Ok(())
        }

        fn out_links(&mut self, _links: &[(String, Option<String>)]) -> Result<()> {
            Ok(())
        }

        fn note_connect(&mut self, _d: Duration) {}

        fn note_upload(&mut self, _p: backend::Phase) {}

        fn report(&self, _plan: &Plan, _plan_time: Duration) -> String {
            String::new()
        }
    }

    struct NoUpload;

    impl Uploader for NoUpload {
        fn upload(
            &mut self,
            _b: &mut dyn Backend,
            _roots: &[StorePath],
            _on_event: &mut dyn FnMut(Event),
        ) -> Result<upload::Stats> {
            Ok(upload::Stats::default())
        }

        fn drvs(&self, _roots: &[StorePath], _deep: bool) -> Result<Vec<backend::Drv>> {
            Ok(Vec::new())
        }

        fn roots_answered(&mut self, _roots: &[StorePath], _valid: &HashSet<String>) {}
    }

    /// Concurrent builds keep the cap, and every build's log comes out as one
    /// block: the events carry their drv, so the renderer's buffers key on it.
    #[test]
    fn a_fan_out_backend_keeps_the_cap_and_the_log_blocks() {
        let sink = Sink::default();
        let over = Arc::new(AtomicBool::new(false));
        let style = Style {
            mode: render::Mode::Ci,
            color: false,
            fold: false,
            quiet: false,
        };
        let mut d = Dispatcher::with_backend(
            Box::new(Fan {
                limit: 2,
                peak: over.clone(),
                verdicts: true,
            }),
            Provenance::off(),
            Reporter::with_output(style, Box::new(sink.clone())),
        );
        let names = ["alpha", "beta", "gamma", "delta"];
        let targets: Vec<Target> = names
            .iter()
            .map(|n| Target::new(n, drv(n), vec!["out".into()]))
            .collect();
        let outcomes = d.submit(&mut NoUpload, targets).unwrap();
        assert_eq!(outcomes.len(), 4);
        assert!(outcomes.iter().all(|o| o.success));
        assert!(!over.load(Ordering::SeqCst), "more than 2 builds in flight");

        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        for n in names {
            let lines: Vec<usize> = out
                .lines()
                .enumerate()
                .filter(|(_, l)| l.contains(&format!("{n}.drv line ")))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(lines.len(), 4, "{n}: {out}");
            assert_eq!(
                lines.last().unwrap() - lines[0],
                3,
                "{n}'s log block is interleaved: {out}"
            );
        }
    }

    /// Builds the dependencies `done`, `cut` and `bad` of one target. `bad`
    /// fails, so the store stops `cut` and names only `bad`.
    struct Stops;

    impl Backend for Stops {
        fn query_valid(
            &mut self,
            paths: &[&StorePath],
            _on_event: &mut dyn FnMut(Event),
        ) -> Result<HashSet<String>> {
            Ok((paths.iter())
                .map(|p| p.base_name().to_string())
                .filter(|p| p.ends_with("-done"))
                .collect())
        }

        fn substitute(
            &mut self,
            _paths: &[StorePath],
            _on_event: &mut dyn FnMut(Event),
        ) -> Result<()> {
            Ok(())
        }

        fn add_objects(
            &mut self,
            _objects: &[&Object],
            _on_event: &mut dyn FnMut(Event),
        ) -> Result<u64> {
            Ok(0)
        }

        fn submit(
            &mut self,
            batch: &backend::Batch<'_>,
            on_event: &mut dyn FnMut(Event),
        ) -> Result<HashMap<String, backend::Status>> {
            for n in ["done", "cut", "bad"] {
                on_event(Event::Started(Started {
                    drv_path: drv(n).to_string(),
                }));
            }
            on_event(Event::Failed(Failed {
                drv_path: drv("bad").to_string(),
                reason: "hash mismatch".into(),
            }));
            for n in ["done", "cut"] {
                on_event(Event::Stopped(Stopped {
                    drv_path: drv(n).to_string(),
                    built: false,
                }));
            }
            let why = format!("1 dependencies of derivation '{}' failed", drv("pkg"));
            Ok((batch.jobs.iter())
                .map(|j| (j.drv.to_string(), backend::Status::failed(why.clone(), 1)))
                .collect())
        }

        fn outputs(
            &mut self,
            drv: &str,
            on_event: &mut dyn FnMut(Event),
        ) -> Result<BTreeMap<String, Option<String>>> {
            Fan {
                limit: 1,
                peak: Arc::new(AtomicBool::new(false)),
                verdicts: true,
            }
            .outputs(drv, on_event)
        }

        fn fetch(&mut self, _store_path: &str, _dir: &Path) -> Result<()> {
            Ok(())
        }

        fn add_root(&mut self, _store_path: &str, _link: &Path) -> Result<()> {
            Ok(())
        }

        fn out_links(&mut self, _links: &[(String, Option<String>)]) -> Result<()> {
            Ok(())
        }

        fn note_connect(&mut self, _d: Duration) {}

        fn note_upload(&mut self, _p: backend::Phase) {}

        fn report(&self, _plan: &Plan, _plan_time: Duration) -> String {
            String::new()
        }
    }

    #[test]
    fn a_dependency_the_store_stopped_is_not_a_success() {
        let sink = Sink::default();
        let style = Style {
            mode: render::Mode::Tty,
            color: false,
            fold: false,
            quiet: false,
        };
        let mut d = Dispatcher::with_backend(
            Box::new(Stops),
            Provenance::off(),
            Reporter::with_output(style, Box::new(sink.clone())),
        );
        let outcomes = d
            .submit(&mut NoUpload, vec![Target::new("pkg", drv("pkg"), vec![])])
            .unwrap();
        assert!(!outcomes[0].success);
        d.reporter.render.lock().unwrap().finish();
        let out = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("✔  done"), "{out}");
        assert!(!out.contains("✔  cut"), "{out}");
        assert!(out.contains("·  cut stopped after"), "{out}");
        assert!(
            out.contains("✔ 1 succeeded · ✘ 2 failed · 1 stopped"),
            "{out}"
        );
    }

    /// A target the build op came back without leaves no output path, so the
    /// dispatcher must exit non-zero.
    #[test]
    fn a_target_with_no_verdict_fails() {
        let style = Style {
            mode: render::Mode::Ci,
            color: false,
            fold: false,
            quiet: false,
        };
        let mut d = Dispatcher::with_backend(
            Box::new(Fan {
                limit: 1,
                peak: Arc::new(AtomicBool::new(false)),
                verdicts: false,
            }),
            Provenance::off(),
            Reporter::with_output(style, Box::new(Sink::default())),
        );
        let outcomes = d
            .submit(&mut NoUpload, vec![Target::new("a", drv("a"), vec![])])
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(!outcomes[0].success);
        assert!(outcomes[0].skipped);
        assert!(outcomes[0].outputs.is_empty());
        assert_eq!(outcomes[0].exit, 1);
        assert_eq!(d.exit, 1);
        let why = outcomes[0].reason.clone().unwrap_or_default();
        assert!(why.contains("no build result"), "{why}");
    }
}
