//! The dispatcher's transport half. `Dispatcher` owns the batching, the
//! provenance, the event stream and the records; a `Backend` owns the wire:
//! what "ask whether these paths are valid", "register these objects" and
//! "build this ready set" mean on one transport.
//!
//! Two of them. The worker backend holds one `Connection` and sends one
//! `BuildPathsWithResults` per ready set, because the protocol is serial per
//! connection and a per-derivation op idles the daemon between derivations
//! (docs/reference/builders.md: 7-10x slower). The `grpc://` backend fans the ready
//! set out instead: one `BuildDerivation` per derivation, placed by the
//! store's scheduler, with up to `max-builds` in flight, which the gRPC API
//! tags with its derivation, so the events key correctly.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::log::{self, Activity};
use crate::remote::copy::{self, Copying};
use crate::remote::event::Event;
use crate::remote::grpc::backend::Grpc;
use crate::remote::plan::{self, Object, Plan};
use crate::remote::worker::wire::{self, write_object};
use crate::remote::worker::{Connection, DEFAULT_SOCKET, Missing, Options, Stats, Transport};
use crate::store::nar;
use crate::store::path::StorePath;

/// One derivation of a ready set, and the outputs the caller wants.
#[derive(Debug, Clone)]
pub struct Job {
    pub drv: StorePath,
    /// Empty means every output.
    pub outputs: Vec<String>,
}

impl Job {
    /// `<out1,out2>` or `*`, the output half of a derived path.
    pub fn spec(&self) -> String {
        if self.outputs.is_empty() {
            return "*".to_string();
        }
        let names: BTreeSet<&str> = self.outputs.iter().map(String::as_str).collect();
        names.into_iter().collect::<Vec<_>>().join(",")
    }
}

/// What became of one derivation of a ready set.
#[derive(Debug, Clone)]
pub struct Status {
    pub ok: bool,
    /// The one-line reason, when it failed.
    pub reason: String,
    /// `log::build_exit` of the failure, 0 for a success.
    pub exit: i32,
}

impl Status {
    pub fn ok() -> Status {
        Status {
            ok: true,
            reason: String::new(),
            exit: 0,
        }
    }

    pub fn failed(reason: String, exit: i32) -> Status {
        Status {
            ok: false,
            reason,
            exit,
        }
    }
}

/// One derivation of a batch's closure: its path and the ATerm that hashes to
/// it. A backend that schedules the builds itself needs the whole closure, not
/// just the ready set.
pub struct Drv {
    pub path: StorePath,
    pub aterm: Arc<Vec<u8>>,
}

/// One ready set, plus the derivations behind it.
pub struct Batch<'a> {
    pub jobs: &'a [Job],
    /// The jobs' own derivations, or with `true` everything they need too,
    /// references before referrers. Walked only when the backend asks, which
    /// the worker backend never does: the daemon resolves the closure.
    pub drvs: &'a dyn Fn(bool) -> Result<Vec<Drv>>,
}

/// What the client side of an upload measured, which no backend can see:
/// `--timings` wants it in the same report.
#[derive(Debug, Default, Clone, Copy)]
pub struct Phase {
    /// Planned paths the store lacked.
    pub missing: usize,
    pub resolve: Duration,
    /// Missing sources the client located and hashed.
    pub resolve_paths: usize,
    /// Of those, the ones read from the local store.
    pub store_sources: usize,
}

/// One transport, as the dispatcher uses it.
pub trait Backend {
    /// The subset of `paths` the store already has, by store path base name.
    fn query_valid(
        &mut self,
        paths: &[&StorePath],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<HashSet<String>>;

    /// Realises the sources the client cannot produce (an input-addressed
    /// build output pinned with `builtins.storePath`): the store substitutes
    /// them, and must hold them before the drvs that name them.
    fn substitute(&mut self, paths: &[StorePath], on_event: &mut dyn FnMut(Event)) -> Result<()>;

    /// Registers `objects` on the store; returns the bytes it cost.
    fn add_objects(&mut self, objects: &[&Object], on_event: &mut dyn FnMut(Event)) -> Result<u64>;

    /// Builds a ready set. Every job gets a verdict, keyed by its derivation
    /// path; a job the store never reached is absent from the map.
    fn submit(
        &mut self,
        batch: &Batch<'_>,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<HashMap<String, Status>>;

    /// What building `jobs` would build and fetch.
    fn missing(&mut self, jobs: &[Job], on_event: &mut dyn FnMut(Event)) -> Result<Missing> {
        let _ = (jobs, on_event);
        Err(Error::Usage(
            "--dry-run needs a worker-protocol builder".into(),
        ))
    }

    /// Output name to store path for a derivation that built.
    fn outputs(
        &mut self,
        drv: &str,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<BTreeMap<String, Option<String>>>;

    /// Restores one output into `dir`, whose existence the caller checked.
    fn fetch(&mut self, store_path: &str, dir: &Path) -> Result<()>;

    /// An indirect GC root at `link` for `store_path`.
    fn add_root(&mut self, store_path: &str, link: &Path) -> Result<()>;

    /// `nix build`'s outputs on the local daemon, an output the builder kept
    /// registered there first, and a symlink with its GC root where a link is
    /// named. `--no-link` names none, yet the path stays valid, as the build
    /// hook leaves it.
    fn out_links(&mut self, links: &[(String, Option<String>)]) -> Result<()>;

    fn note_connect(&mut self, d: Duration);

    fn note_upload(&mut self, p: Phase);

    /// `--timings`: one line per phase, in the flow's order.
    fn report(&self, plan: &Plan, plan_time: Duration) -> String;
}

/// The local daemon, where out-links and GC roots go whatever the builder is
/// (docs/reference/classic.md).
pub fn local() -> Transport {
    Transport::Unix(PathBuf::from(DEFAULT_SOCKET))
}

/// The store paths `out_links` was given.
pub fn link_targets(links: &[(String, Option<String>)]) -> Result<Vec<StorePath>> {
    links
        .iter()
        .map(|(p, _)| Ok(StorePath::parse(p)?))
        .collect()
}

/// Makes each symlink that was asked for, as a GC root of `conn`'s store.
pub fn add_roots(conn: &mut Connection, links: &[(String, Option<String>)]) -> Result<()> {
    for (path, link) in links {
        if let Some(link) = link {
            conn.add_perm_root(path, Path::new(link))?;
        }
    }
    Ok(())
}

/// Connects the transport `url` names.
pub fn open(
    transport: &Transport,
    options: &Options,
    on_event: &mut dyn FnMut(Event),
) -> Result<Box<dyn Backend>> {
    if let Transport::Grpc(cfg) = transport {
        return Ok(Box::new(Grpc::open(cfg)?));
    }
    Ok(Box::new(Worker::open(transport, options, on_event)?))
}

/// What either backend did, in the shape `--timings` prints.
#[derive(Debug, Default, Clone)]
pub struct Timings {
    pub connect: Duration,
    pub query: Duration,
    pub resolve: Duration,
    pub substitute: Duration,
    pub add: Duration,
    pub build: Duration,
    pub outputs: Duration,
    pub fetch: Duration,
    pub trusted: Option<bool>,
    pub query_paths: usize,
    pub missing: usize,
    /// Missing sources the client located and hashed.
    pub resolve_paths: usize,
    /// Of those, the ones with no eval-time origin, found through the local store.
    pub store_sources: usize,
    /// Missing sources left to the store's substituters.
    pub substitute_paths: usize,
    pub add_paths: usize,
    pub add_bytes: u64,
    pub fetched: usize,
}

impl Timings {
    pub fn note_upload(&mut self, p: Phase) {
        self.missing += p.missing;
        self.resolve += p.resolve;
        self.resolve_paths += p.resolve_paths;
        self.store_sources += p.store_sources;
    }

    /// `protocol`, `store` and `built` are the words that differ by transport;
    /// the caller appends its own last line.
    pub fn report(
        &self,
        plan: &Plan,
        plan_time: Duration,
        protocol: &str,
        store: &str,
        built: &str,
    ) -> String {
        let trusted = match self.trusted {
            Some(true) => "trusted",
            Some(false) => "untrusted",
            None => "trust unknown",
        };
        let mut s = format!(
            "plan: {} sources, {} drvs, {:.3}s\nconnect: {protocol}, {trusted}, {:.3}s\nquery: {} paths, {} missing, {:.3}s\nresolve: {} sources",
            plan.sources.len(),
            plan.drvs.len(),
            plan_time.as_secs_f64(),
            self.connect.as_secs_f64(),
            self.query_paths,
            self.missing,
            self.query.as_secs_f64(),
            self.resolve_paths,
        );
        if self.store_sources > 0 {
            s.push_str(&format!(
                ", {} not from this eval, read from /nix/store",
                self.store_sources
            ));
        }
        s.push_str(&format!(", {:.3}s", self.resolve.as_secs_f64()));
        if self.substitute_paths > 0 {
            s.push_str(&format!(
                "\nsubstitute: {} paths left to the {store}, {:.3}s",
                self.substitute_paths,
                self.substitute.as_secs_f64()
            ));
        }
        s.push_str(&format!(
            "\nadd: {} paths, {} bytes, {:.3}s\nbuild: {built}{:.3}s\noutputs: {:.3}s",
            self.add_paths,
            self.add_bytes,
            self.add.as_secs_f64(),
            self.build.as_secs_f64(),
            self.outputs.as_secs_f64(),
        ));
        if self.fetched > 0 {
            s.push_str(&format!(
                "\nfetch: {} outputs, {:.3}s",
                self.fetched,
                self.fetch.as_secs_f64()
            ));
        }
        s
    }
}

/// The worker protocol: one serial connection, one op per ready set.
pub struct Worker {
    pub conn: Connection,
    transport: Transport,
    t: Timings,
}

impl Worker {
    pub fn open(
        transport: &Transport,
        options: &Options,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Worker> {
        let conn = Connection::open_with(transport, options, on_event)?;
        Ok(Worker {
            t: Timings {
                trusted: conn.trusted,
                ..Timings::default()
            },
            conn,
            transport: transport.clone(),
        })
    }
}

impl Backend for Worker {
    fn query_valid(
        &mut self,
        paths: &[&StorePath],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<HashSet<String>> {
        if paths.is_empty() {
            return Ok(HashSet::new());
        }
        let full: Vec<String> = paths.iter().map(ToString::to_string).collect();
        self.t.query_paths += full.len();
        let t0 = Instant::now();
        let valid = self.conn.query_valid_paths(&full, on_event)?;
        self.t.query += t0.elapsed();
        Ok(valid.into_iter().map(base_name).collect())
    }

    fn substitute(&mut self, paths: &[StorePath], on_event: &mut dyn FnMut(Event)) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.t.substitute_paths += paths.len();
        let t0 = Instant::now();
        let targets: Vec<String> = paths.iter().map(ToString::to_string).collect();
        let results = self.conn.build_paths_with_results(&targets, on_event)?;
        self.t.substitute += t0.elapsed();
        for r in results {
            if !r.success() {
                return Err(Error::Store(format!(
                    "cannot substitute '{}' on the builder: status {}: {}",
                    r.path, r.status, r.msg
                )));
            }
        }
        Ok(())
    }

    fn add_objects(&mut self, objects: &[&Object], on_event: &mut dyn FnMut(Event)) -> Result<u64> {
        if objects.is_empty() {
            return Ok(0);
        }
        let t0 = Instant::now();
        plan::prehash_drvs(objects);
        let before = self.conn.bytes_sent();
        let mut copying = Copying::new(objects.len(), "local", &self.transport.uri());
        let mut body = |w: &mut dyn Write| -> io::Result<()> {
            wire::write_u64(w, objects.len() as u64)?;
            for o in objects {
                let _act = copying.path(&o.store_path().to_string());
                write_object(w, o)?;
            }
            Ok(())
        };
        self.conn
            .add_multiple_to_store(false, &mut body, on_event)?;
        let bytes = self.conn.bytes_sent() - before;
        self.t.add += t0.elapsed();
        self.t.add_paths += objects.len();
        self.t.add_bytes += bytes;
        Ok(bytes)
    }

    fn submit(
        &mut self,
        batch: &Batch<'_>,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<HashMap<String, Status>> {
        let targets: Vec<String> = batch
            .jobs
            .iter()
            .map(|j| format!("{}!{}", j.drv, j.spec()))
            .collect();
        let t0 = Instant::now();
        let results = self.conn.build_paths_with_results(&targets, on_event)?;
        self.t.build += t0.elapsed();
        let mut out: HashMap<String, Status> = HashMap::new();
        for r in &results {
            let drv = r.path.split('!').next().unwrap_or(&r.path).to_string();
            let e = out.entry(drv).or_insert_with(Status::ok);
            if !r.success() {
                let exit = log::build_exit(r.status, &r.msg);
                *e = Status::failed(format!("status {}: {}", r.status, r.msg), exit);
            }
        }
        Ok(out)
    }

    fn missing(&mut self, jobs: &[Job], on_event: &mut dyn FnMut(Event)) -> Result<Missing> {
        let paths: Vec<String> = jobs
            .iter()
            .map(|j| format!("{}!{}", j.drv, j.spec()))
            .collect();
        self.conn.query_missing(&paths, on_event)
    }

    fn outputs(
        &mut self,
        drv: &str,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<BTreeMap<String, Option<String>>> {
        let t0 = Instant::now();
        let map = self.conn.query_derivation_output_map(drv, on_event)?;
        self.t.outputs += t0.elapsed();
        Ok(map)
    }

    fn fetch(&mut self, store_path: &str, dir: &Path) -> Result<()> {
        let sp = StorePath::parse(store_path)?;
        let dest = dir.join(sp.base_name());
        let t0 = Instant::now();
        let what = format!(
            "copying path '{store_path}' from '{}'",
            self.transport.uri()
        );
        let _act = Activity::new(
            log::LVL_INFO,
            log::ACT_COPY_PATH,
            &what,
            vec![store_path.into()],
        );
        self.conn.nar_from_path(store_path, &mut |_| {}, &mut |r| {
            nar::restore_path(r, &dest)
        })?;
        self.t.fetch += t0.elapsed();
        self.t.fetched += 1;
        Ok(())
    }

    fn add_root(&mut self, store_path: &str, link: &Path) -> Result<()> {
        self.conn.add_perm_root(store_path, link)
    }

    fn out_links(&mut self, links: &[(String, Option<String>)]) -> Result<()> {
        if links.is_empty() {
            return Ok(());
        }
        if self.transport == local() {
            return add_roots(&mut self.conn, links);
        }
        // A symlink and its GC root are worth nothing without the path they
        // name, so the closure comes back to the local daemon first.
        let paths = link_targets(links)?;
        let mut conn = Connection::open(&local(), &mut |_| {})?;
        let from = self.transport.uri();
        copy::closure(&mut self.conn, &from, &mut conn, &paths, &mut |_| {})?;
        add_roots(&mut conn, links)
    }

    fn note_connect(&mut self, d: Duration) {
        self.t.connect = d;
    }

    fn note_upload(&mut self, p: Phase) {
        self.t.note_upload(p);
    }

    fn report(&self, plan: &Plan, plan_time: Duration) -> String {
        worker_report(&self.t, plan, plan_time, self.conn.minor, self.conn.stats)
    }
}

fn worker_report(t: &Timings, plan: &Plan, plan_time: Duration, minor: u64, s: Stats) -> String {
    format!(
        "{}\nworker: {} ops, {} round trips, {} bytes sent",
        t.report(
            plan,
            plan_time,
            &format!("protocol 1.{minor}"),
            "daemon",
            ""
        ),
        s.ops,
        s.round_trips,
        s.bytes_sent
    )
}

/// `/nix/store/<hash>-<name>` as `<hash>-<name>`.
fn base_name(path: String) -> String {
    match path.rsplit_once('/') {
        Some((_, base)) => base.to_string(),
        None => path,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::tests::drv;
    use super::*;

    /// Every field set, so that `report` leaves no line out.
    pub(crate) fn every_timing() -> Timings {
        Timings {
            connect: Duration::from_millis(1),
            query: Duration::from_millis(2),
            resolve: Duration::from_millis(3),
            substitute: Duration::from_millis(4),
            add: Duration::from_millis(5),
            build: Duration::from_millis(6),
            outputs: Duration::from_millis(7),
            fetch: Duration::from_millis(8),
            trusted: Some(true),
            query_paths: 9,
            missing: 8,
            resolve_paths: 7,
            store_sources: 6,
            substitute_paths: 5,
            add_paths: 4,
            add_bytes: 3,
            fetched: 2,
        }
    }

    #[test]
    fn the_timings_report_with_every_line() {
        let t = every_timing();
        let stats = Stats {
            ops: 1,
            round_trips: 2,
            bytes_sent: 3,
        };
        assert_eq!(
            worker_report(&t, &Plan::default(), Duration::from_millis(9), 38, stats),
            "plan: 0 sources, 0 drvs, 0.009s\n\
             connect: protocol 1.38, trusted, 0.001s\n\
             query: 9 paths, 8 missing, 0.002s\n\
             resolve: 7 sources, 6 not from this eval, read from /nix/store, 0.003s\n\
             substitute: 5 paths left to the daemon, 0.004s\n\
             add: 4 paths, 3 bytes, 0.005s\n\
             build: 0.006s\n\
             outputs: 0.007s\n\
             fetch: 2 outputs, 0.008s\n\
             worker: 1 ops, 2 round trips, 3 bytes sent"
        );
    }

    #[test]
    fn output_specs() {
        let j = Job {
            drv: drv("x"),
            outputs: vec![],
        };
        assert_eq!(j.spec(), "*");
        let j = Job {
            drv: drv("x"),
            outputs: vec!["dev".into(), "out".into(), "out".into()],
        };
        assert_eq!(j.spec(), "dev,out");
    }

    #[test]
    fn base_names_drop_the_store_dir() {
        assert_eq!(base_name("/nix/store/aaa-x".into()), "aaa-x");
        assert_eq!(base_name("aaa-x".into()), "aaa-x");
    }
}
