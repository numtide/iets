//! The `grpc://` protocol calls, the counterpart of the worker backend
//! (`build::backend::Worker`): one `QueryValidPaths`, one
//! `AddMultipleToStore` of what the store lacks, the builds, and `FetchNars`
//! for the outputs. Each build is a `Want` on the `Schedule` stream, then
//! `BuildDerivation` on the worker that the scheduler assigned. `backend.rs`
//! drives these calls as the dispatcher's backend.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{self, Seek, Write};

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use std::{mem, result, thread};
use zstd::stream::raw::{InBuffer, Operation, OutBuffer};

use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::build::backend::{self, Drv};
use crate::error::{Error, Result};
use crate::log::{Activity, RES_BUILD_LOG_LINE, RES_SET_PHASE};
use crate::remote::plan;
use crate::session;
use crate::store::drv::Derivation;
use crate::store::nar::restore_path;
use crate::store::path::StorePath;

use super::super::copy::topo_sort;
use super::super::event::{Event, Log, Phase};
use super::super::plan::{Object, Plan};
use super::super::worker::Connection;
use super::super::worker::wire::{write_object, write_u64};
use super::drvwire::{BUILD_PROTOCOL, BuildResult, PathInfo, write_basic_derivation};
use super::sched::{Answer, Sched};
use super::{Client, Headers, c_request, first_line, gone_away, proto};

/// The shared timings, and what only this transport counts.
#[derive(Debug, Default, Clone)]
pub struct Timings {
    pub common: backend::Timings,
    pub drvs: usize,
    pub drvs_cached: usize,
    pub calls: usize,
}

impl Timings {
    pub fn report(&self, plan: &Plan, plan_time: Duration) -> String {
        let built = format!("{} drvs, {} already valid, ", self.drvs, self.drvs_cached);
        format!(
            "{}\ngrpc: {} calls",
            self.common.report(plan, plan_time, "gRPC", "store", &built),
            self.calls
        )
    }
}

/// The sources the client cannot produce (an input-addressed build output
/// pinned with `builtins.storePath`): the store fetches them instead. A worker
/// substitutes an input when the build asks for it (daemon/main.cc:
/// `gatherInputs`), so here we only check that some substituter has them,
/// which is what `QueryValidPaths` with `substitute` answers.
pub fn substitute(c: &mut Client, paths: &[StorePath], t: &mut Timings) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = paths.iter().map(|p| p.base_name().to_string()).collect();
    let known = query(c, &names, true, t)?;
    for p in paths {
        if !known.contains(p.base_name()) {
            return Err(Error::Store(format!(
                "'{p}' is not in the local store and no substituter of '{}' has it",
                c.cfg.authority
            )));
        }
    }
    Ok(())
}

/// Derivations parsed from their ATerms: every derivation of the batch's
/// closure, since the client walks it.
pub fn drv_table(drvs: &[Drv]) -> Result<BTreeMap<StorePath, Derivation>> {
    let mut out = BTreeMap::new();
    for d in drvs {
        let drv = Derivation::parse(&d.aterm, Derivation::name_from_path(&d.path)?)?;
        out.insert(d.path.clone(), drv);
    }
    Ok(out)
}

/// The store path of `drv_path!output`, statically.
pub fn output_path(
    drvs: &BTreeMap<StorePath, Derivation>,
    drv_path: &StorePath,
    output: &str,
) -> Option<StorePath> {
    let drv = drvs.get(drv_path)?;
    drv.outputs
        .get(output)?
        .path(&drv.name, output)
        .ok()
        .flatten()
}

/// `QueryValidPaths` on store path base names, in one call.
pub fn query_valid_paths(
    c: &mut Client,
    names: &[String],
    t: &mut Timings,
) -> Result<HashSet<String>> {
    if names.is_empty() {
        return Ok(HashSet::new());
    }
    query(c, names, false, t)
}

fn query(
    c: &mut Client,
    names: &[String],
    substitute: bool,
    t: &mut Timings,
) -> Result<HashSet<String>> {
    t.calls += 1;
    let req = proto::QueryValidPathsRequest {
        paths: names.to_vec(),
        substitute,
    };
    let reply = c.call("QueryValidPaths", |mut stub, h| {
        let req = req.clone();
        async move { stub.query_valid_paths(c_request(req, &h)).await }
    })?;
    Ok(reply.paths.into_iter().collect())
}

/// `QueryPathInfos`: the 1.16 `UnkeyedValidPathInfo` of each path the store
/// has, signatures included. The worker substitutes a path it
/// lacks from the cache first, so this also answers for cached outputs.
fn query_path_infos(
    c: &mut Client,
    paths: &[StorePath],
    t: &mut Timings,
) -> Result<HashMap<StorePath, PathInfo>> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    t.calls += 1;
    let req = proto::QueryPathInfosRequest {
        paths: paths.iter().map(|p| p.base_name().to_string()).collect(),
    };
    let reply = c.call("QueryPathInfos", |mut stub, h| {
        let req = req.clone();
        async move { stub.query_path_infos(c_request(req, &h)).await }
    })?;
    let mut out = HashMap::new();
    for entry in reply.infos {
        let info = PathInfo::decode(&entry.path, &entry.info)?;
        out.insert(info.path.clone(), info);
    }
    Ok(out)
}

/// Ships `AddMultipleChunk`s to the channel, one message per `CHUNK` of
/// compressed bytes (pump.hh:`ZstdWriterSink`).
struct ChunkShip {
    tx: tokio::sync::mpsc::Sender<proto::AddMultipleChunk>,
    buf: Vec<u8>,
    sent: Arc<AtomicU64>,
}

/// pump.hh:`kChunkSize`.
const CHUNK: usize = 256 << 10;

impl ChunkShip {
    fn ship(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let data = mem::take(&mut self.buf);
        self.sent.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.tx
            .blocking_send(proto::AddMultipleChunk {
                repair: false,
                check_sigs: false,
                data,
            })
            .map_err(|_| closed())
    }
}

impl Write for ChunkShip {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.ship()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ship()
    }
}

/// `AddMultipleToStore`: one zstd stream of the worker-protocol framing (a
/// count, then each `ValidPathInfo` at 1.16 followed by its NAR), from the same
/// `write_object` the worker transport uses. Like the C++ sink, the stream is
/// flushed once, at the end; the reader decodes a partial frame fine.
/// `check_sigs` is false because every path sent is content-addressed, so the
/// daemon recomputes its address. For a `write` token the server forces
/// `CheckSigs` on anyway (daemon/main.cc), which those paths still pass.
pub fn add_multiple(c: &mut Client, objects: &[&Object], t: &mut Timings) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(c.cfg.connect_timeout);
    let mut pause = Duration::from_millis(500);
    loop {
        t.calls += 1;
        let headers = c.headers()?;
        match upload(c, objects, &headers) {
            Ok(n) => return Ok(n),
            Err(s) => {
                if !gone_away(&s) || Instant::now() + pause > deadline {
                    return Err(c.status_error(&s, "AddMultipleToStore"));
                }
                errln!("{}: {}, retrying", c.cfg.authority, first_line(s.message()));
                thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_secs(4));
            }
        }
    }
}

/// One `AddMultipleToStore` attempt. The NAR dump is synchronous and borrows
/// the plan, so it runs on a scoped thread while this one drives the stream.
fn upload(
    c: &Client,
    objects: &[&Object],
    headers: &Headers,
) -> result::Result<u64, tonic::Status> {
    let (tx, rx) = tokio::sync::mpsc::channel::<proto::AddMultipleChunk>(4);
    let mut stub = c.stub.clone();
    let rt = &c.rt;
    let n = objects.len();
    plan::prehash_drvs(objects);
    let sent = Arc::new(AtomicU64::new(0));
    thread::scope(|s| {
        let producer = {
            let tx = tx.clone();
            let sent = Arc::clone(&sent);
            s.spawn(session::bind(move || -> io::Result<()> {
                // store.cc sends the flags alone first; the zstd stream
                // follows in its own messages.
                tx.blocking_send(proto::AddMultipleChunk {
                    repair: false,
                    check_sigs: false,
                    data: Vec::new(),
                })
                .map_err(|_| closed())?;
                let ship = ChunkShip {
                    tx,
                    buf: Vec::with_capacity(CHUNK + (1 << 16)),
                    sent,
                };
                let mut enc = zstd::stream::write::Encoder::new(ship, 1)?;
                write_u64(&mut enc, n as u64)?;
                for o in objects {
                    write_object(&mut enc, o)?;
                }
                enc.finish()?.flush()
            }))
        };
        drop(tx);
        let reply = rt.block_on(async {
            stub.add_multiple_to_store(c_request(ReceiverStream::new(rx), headers))
                .await
        });
        let written = producer.join().map_err(|_| {
            tonic::Status::internal("the AddMultipleToStore producer thread panicked")
        })?;
        upload_outcome(reply, written).map(|()| sent.load(Ordering::Relaxed))
    })
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "gRPC stream closed by peer")
}

/// The server status explains a stream the server broke better than the local
/// write error does (transfer.cc:addMultipleToStoreOnce). A stream this side
/// broke, a source that no longer dumps as evaluated, reaches the server only
/// as its end ("gRPC stream ended"), so the local error wins then.
fn upload_outcome<T>(
    reply: result::Result<T, tonic::Status>,
    written: io::Result<()>,
) -> result::Result<(), tonic::Status> {
    match (reply, written) {
        (_, Err(e)) if e.kind() != io::ErrorKind::BrokenPipe => {
            Err(tonic::Status::internal(e.to_string()))
        }
        (Err(status), _) => Err(status),
        (Ok(_), w) => w.map_err(|e| tonic::Status::internal(e.to_string())),
    }
}

/// One derivation the farm must build, in the shape `BuildDerivation` wants.
pub struct Job {
    pub drv_path: StorePath,
    pub drv: Derivation,
    pub input_srcs: BTreeSet<StorePath>,
    pub features: Vec<String>,
}

impl Job {
    /// The features the balancer routes on.
    pub fn new(drv_path: StorePath, drv: Derivation, input_srcs: BTreeSet<StorePath>) -> Job {
        let features = required_system_features(&drv);
        Job {
            drv_path,
            drv,
            input_srcs,
            features,
        }
    }

    /// build.cc:`fillWant`. `drv_path` is the base name, as C++
    /// `StorePath::to_string` prints it.
    fn want(&self) -> proto::Want {
        proto::Want {
            drv_path: self.drv_path.base_name().to_string(),
            out_keys: self
                .drv
                .outputs
                .iter()
                .filter_map(|(name, o)| o.path(&self.drv.name, name).ok().flatten())
                .map(|p| format!("{}.narinfo", p.hash_part()))
                .collect(),
            inputs: self.input_srcs.iter().map(ToString::to_string).collect(),
            cp_hint_ms: 0,
            build_mode: 0,
            required_features: self.features.clone(),
            system: self.drv.platform.clone(),
        }
    }
}

/// nix-compat.hh:`requiredSystemFeatures`, which the balancer routes on: the
/// `requiredSystemFeatures` environment variable, space separated, or the same
/// key of `__json` when the derivation has structured attributes.
fn required_system_features(drv: &Derivation) -> Vec<String> {
    if let Some(json) = &drv.structured_attrs {
        let mut out: BTreeSet<String> = BTreeSet::new();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json)
            && let Some(list) = v.get("requiredSystemFeatures").and_then(|l| l.as_array())
        {
            for f in list {
                if let Some(s) = f.as_str() {
                    out.insert(s.to_string());
                }
            }
        }
        return out.into_iter().collect();
    }
    drv.env_str("requiredSystemFeatures")
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_string)
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

/// One derivation through the scheduler (build.cc:`Run::resultFor`): a `Want`,
/// then `BuildDerivation` on the worker it names. A worker that bounces the
/// build sends it back to the scheduler, up to `reschedule-retries` times.
/// Returns the result and the RPCs it cost.
pub fn build(
    c: &Client,
    sched: &Sched,
    job: &Job,
    objects: &Objects,
    tx: &Sender<Event>,
    act: &Activity,
) -> Result<(BuildResult, usize)> {
    let mut calls = 0usize;
    let mut bounces = 0u32;
    // A refusal costs no call, so spending `reschedule-retries` on it would
    // fail the derivation with nothing attempted. Bounded like `Unplaceable`.
    let refuse_by = Instant::now() + sched.grace();
    loop {
        let (worker, assign_id) = match sched.place(job.want())? {
            // 2 = AlreadyValid.
            Answer::Cached => {
                return Ok((
                    BuildResult {
                        status: 2,
                        ..BuildResult::default()
                    },
                    calls,
                ));
            }
            Answer::Assigned { worker, assign_id } => (worker, assign_id),
        };
        if let Some(wait) = sched.health.refusal(&worker) {
            let left = refuse_by.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Store(format!(
                    "'{}': waited {}s, and every worker the scheduler assigned turns builds away (last '{worker}')",
                    job.drv_path,
                    sched.grace().as_secs()
                )));
            }
            thread::sleep(wait.min(left));
            continue;
        }
        match build_derivation(c, job, &worker, assign_id, objects, tx, act, &mut calls)? {
            Some(res) => {
                sched.health.worked(&worker);
                return Ok((res, calls));
            }
            None => {
                sched.health.bounced(&worker);
                bounces += 1;
                if bounces > c.cfg.reschedule_retries {
                    return Err(Error::Store(format!(
                        "'{}': gave up after {} reschedules",
                        job.drv_path, c.cfg.reschedule_retries
                    )));
                }
                thread::sleep(reschedule_pause(bounces));
            }
        }
    }
}

/// After the `n`th bounce: 1 s doubling to 30 s, 121 s over the default 8
/// reschedules. build.cc asks again at once, but a worker that crashed stays
/// placeable until the scheduler notices (42 s seen), and every bounce off it
/// is instant, so back-to-back asks spent the budget first. This is one
/// derivation's own backstop; `health` is what stops the whole batch walking
/// this ladder against the same address.
fn reschedule_pause(n: u32) -> Duration {
    Duration::from_secs((1u64 << n.saturating_sub(1).min(5)).min(30))
}

/// build.cc:`buildAssigned`: `NOT_FOUND` means the worker lacks the `.drv`
/// closure, so upload it and ask once more. `None` when the worker bounced
/// the build and the scheduler must place it again.
#[allow(clippy::too_many_arguments)]
fn build_derivation(
    c: &Client,
    job: &Job,
    worker: &str,
    assign_id: u64,
    objects: &Objects,
    tx: &Sender<Event>,
    act: &Activity,
    calls: &mut usize,
) -> Result<Option<BuildResult>> {
    let mut drv_bytes = Vec::new();
    write_basic_derivation(&mut drv_bytes, &job.drv, &job.input_srcs)
        .map_err(|e| Error::io("serialising a derivation", e))?;
    let req = proto::BuildDerivationRequest {
        // `StorePath::to_string()` on the C++ side: the base name.
        drv_path: job.drv_path.base_name().to_string(),
        drv: drv_bytes,
        build_mode: 0,
        protocol: BUILD_PROTOCOL,
        assign_id,
    };
    // build.cc:routingFor.
    let mut headers = c.headers()?;
    headers.retain(|(k, _)| *k != "x-nix-system");
    headers.push(("x-nix-system", job.drv.platform.clone()));
    headers.push(("x-nix-features", job.features.join(",")));
    headers.push(("x-nix-worker", worker.to_string()));

    *calls += 1;
    let mut r = try_build_derivation(c, &req, &headers, &job.drv_path, tx, act);
    if matches!(&r, Err(s) if s.code() == tonic::Code::NotFound) {
        *calls += upload_closure(c, objects, &job.drv_path, &headers)? + 1;
        r = try_build_derivation(c, &req, &headers, &job.drv_path, tx, act);
    }
    match r {
        Ok(res) => Ok(Some(res)),
        Err(s) => match s.code() {
            // Superseded, not expected there (any more), worker draining or
            // gone, or died mid-stream.
            tonic::Code::Aborted
            | tonic::Code::FailedPrecondition
            | tonic::Code::Unavailable
            | tonic::Code::Unknown
            | tonic::Code::Internal => {
                errln!(
                    "{}: {}, asking the scheduler again",
                    job.drv_path,
                    first_line(s.message())
                );
                Ok(None)
            }
            _ => Err(c.status_error(&s, "BuildDerivation")),
        },
    }
}

fn try_build_derivation(
    c: &Client,
    req: &proto::BuildDerivationRequest,
    headers: &Headers,
    drv_path: &StorePath,
    tx: &Sender<Event>,
    act: &Activity,
) -> result::Result<BuildResult, tonic::Status> {
    let mut stub = c.stub.clone();
    let req = c_request(req.clone(), headers);
    let mut stream =
        c.rt.block_on(async { stub.build_derivation(req).await })?
            .into_inner();
    let drv = drv_path.to_string();
    let mut res: Option<BuildResult> = None;
    while let Some(msg) = c.rt.block_on(stream.next()).transpose()? {
        match msg.event {
            Some(proto::build_derivation_chunk::Event::LogLine(line)) => {
                let l = String::from_utf8_lossy(&line);
                act.result(RES_BUILD_LOG_LINE, vec![l.trim_end_matches('\n').into()]);
                let _ = tx.send(Event::Log(Log {
                    drv_path: drv.clone(),
                    line,
                }));
            }
            // The call knows its derivation, so the phase is attributed the
            // way the daemon's `setPhase` result is.
            Some(proto::build_derivation_chunk::Event::Phase(phase)) => {
                act.result(RES_SET_PHASE, vec![phase.as_str().into()]);
                let _ = tx.send(Event::Phase(Phase {
                    drv_path: drv.clone(),
                    phase,
                }));
            }
            Some(proto::build_derivation_chunk::Event::Done(done)) => {
                res = Some(
                    BuildResult::read(&mut done.result.as_slice())
                        .map_err(|e| tonic::Status::internal(e.to_string()))?,
                );
            }
            None => {}
        }
    }
    res.ok_or_else(|| tonic::Status::internal("gRPC BuildDerivation stream ended without a result"))
}

/// What this run can re-send, by store path base name, so a `NOT_FOUND` retry
/// re-sends just the closure of one derivation (store.cc:`uploadDrvClosure`).
/// Everything this run registered is here; a path the store already had is not
/// ours to send again, and neither is a substituted source.
#[derive(Default)]
pub struct Objects {
    by_name: HashMap<String, Object>,
}

impl Objects {
    /// Remembers what an `AddMultipleToStore` carried.
    pub fn extend(&mut self, objects: &[&Object]) {
        for o in objects {
            self.by_name
                .insert(o.store_path().base_name().to_string(), (*o).clone());
        }
    }

    /// `drv_path` and everything it references, references before referrers.
    fn closure(&self, drv_path: &StorePath) -> Vec<&Object> {
        let mut out = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack = vec![(drv_path.base_name().to_string(), false)];
        while let Some((name, expanded)) = stack.pop() {
            let Some(o) = self.by_name.get(name.as_str()) else {
                continue;
            };
            if expanded {
                out.push(o);
                continue;
            }
            if !seen.insert(o.store_path().base_name()) {
                continue;
            }
            stack.push((name, true));
            if let Object::Drv { references, .. } = o {
                for r in references {
                    stack.push((r.base_name().to_string(), false));
                }
            }
        }
        out
    }
}

/// store.cc:`uploadDrvClosure`: `QueryValidPaths` on the closure with the same
/// routing headers, then `AddMultipleToStore` of what the worker lacks.
fn upload_closure(
    c: &Client,
    objects: &Objects,
    drv_path: &StorePath,
    headers: &Headers,
) -> Result<usize> {
    let closure = objects.closure(drv_path);
    if closure.is_empty() {
        return Ok(0);
    }
    let mut stub = c.stub.clone();
    let req = proto::QueryValidPathsRequest {
        paths: closure
            .iter()
            .map(|o| o.store_path().base_name().to_string())
            .collect(),
        substitute: false,
    };
    let valid: HashSet<String> =
        c.rt.block_on(async { stub.query_valid_paths(c_request(req, headers)).await })
            .map_err(|s| c.status_error(&s, "QueryValidPaths"))?
            .into_inner()
            .paths
            .into_iter()
            .collect();
    let missing: Vec<&Object> = closure
        .into_iter()
        .filter(|o| !valid.contains(o.store_path().base_name()))
        .collect();
    if missing.is_empty() {
        return Ok(1);
    }
    upload(c, &missing, headers)
        .map(|_| 2)
        .map_err(|s| c.status_error(&s, "AddMultipleToStore"))
}

/// A `FetchNars` stream: one zstd stream across every path, flushed at each
/// frame, so a frame's decompressed bytes belong to `paths[path_index]`.
struct NarDecoder {
    dec: zstd::stream::raw::Decoder<'static>,
    buf: Vec<u8>,
}

impl NarDecoder {
    fn new() -> io::Result<NarDecoder> {
        Ok(NarDecoder {
            dec: zstd::stream::raw::Decoder::new()?,
            buf: vec![0; 1 << 20],
        })
    }

    /// Hands the decompressed bytes of one frame to `out`, a buffer at a
    /// time, so a frame that expands a lot is never held whole.
    fn feed(
        &mut self,
        data: &[u8],
        out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut input = InBuffer::around(data);
        loop {
            let mut o = OutBuffer::around(&mut self.buf[..]);
            self.dec.run(&mut input, &mut o)?;
            let n = o.pos();
            if n > 0 {
                out(&self.buf[..n])?;
            }
            if input.pos() == data.len() && n < self.buf.len() {
                return Ok(());
            }
        }
    }
}

/// What `fetch_nars` hands the frames to, a piece at a time: the path index,
/// decompressed bytes, and whether the path's last frame ended.
type OnFrame<'a> = &'a mut dyn FnMut(usize, &[u8], bool) -> io::Result<()>;

/// `FetchNars(paths)`, frame by frame.
fn fetch_nars(c: &Client, paths: &[StorePath], on_frame: OnFrame<'_>) -> Result<()> {
    let headers = c.headers()?;
    let mut stub = c.stub.clone();
    let req = proto::FetchNarsRequest {
        paths: paths.iter().map(|p| p.base_name().to_string()).collect(),
    };
    let mut stream =
        c.rt.block_on(async { stub.fetch_nars(c_request(req, &headers)).await })
            .map_err(|s| c.status_error(&s, "FetchNars"))?
            .into_inner();
    let mut dec = NarDecoder::new().map_err(|e| Error::io("zstd", e))?;
    while let Some(frame) =
        c.rt.block_on(stream.next())
            .transpose()
            .map_err(|s| c.status_error(&s, "FetchNars"))?
    {
        let i = frame.path_index as usize;
        if i >= paths.len() {
            return Err(Error::Protocol(format!(
                "FetchNars sent a frame for path {i}, only {} were asked for",
                paths.len()
            )));
        }
        dec.feed(&frame.data, &mut |b| on_frame(i, b, false))
            .map_err(|e| Error::io("restoring a NAR", e))?;
        on_frame(i, &[], frame.eof).map_err(|e| Error::io("restoring a NAR", e))?;
    }
    Ok(())
}

/// `--fetch-to DIR`: the farm never copies outputs into a local store, so the
/// NARs come back over `FetchNars`. Each is spooled to an unnamed file beside
/// its destination, so the restore streams from disk instead of holding the
/// whole NAR in memory.
pub fn fetch_into_dir(
    c: &mut Client,
    paths: &[StorePath],
    dir: &Path,
    t: &mut Timings,
) -> Result<()> {
    t.calls += 1;
    let mut open: Option<(usize, File)> = None;
    fetch_nars(c, paths, &mut |i, bytes, eof| {
        if open.as_ref().is_none_or(|(j, _)| *j != i) {
            open = Some((i, tempfile::tempfile_in(dir)?));
        }
        open.as_mut().expect("opened").1.write_all(bytes)?;
        if eof {
            let (_, mut f) = open.take().expect("opened");
            f.rewind()?;
            return restore_path(&mut f, &dir.join(paths[i].base_name()));
        }
        Ok(())
    })
}

/// Registers `paths` and their closure in the local nix-daemon so an out-link
/// points at a real local store path. The local daemon substitutes first: a
/// farm publishes outputs under the cache key, while a worker's path infos carry
/// only the worker's key. The rest comes from the remote: `QueryPathInfos` for
/// the closure, then `FetchNars` for what the local store lacks, written into
/// one `AddMultipleToStore` with each path's `ValidPathInfo`, signatures
/// included, so the daemon checks them as `nix copy --from` does.
pub fn register(
    c: &mut Client,
    paths: &[StorePath],
    conn: &mut Connection,
    on_event: &mut dyn FnMut(Event),
) -> Result<usize> {
    let names: Vec<String> = paths.iter().map(StorePath::to_string).collect();
    let local: HashSet<String> = conn
        .substitute_paths(&names, on_event)?
        .into_iter()
        .collect();
    let paths: Vec<StorePath> = paths
        .iter()
        .filter(|p| !local.contains(&p.to_string()))
        .cloned()
        .collect();
    if paths.is_empty() {
        return Ok(0);
    }
    let mut t = Timings::default();
    let infos = remote_closure(c, &paths, &mut t)?;
    let all: Vec<String> = infos.keys().map(StorePath::to_string).collect();
    let valid: HashSet<String> = conn
        .query_valid_paths(&all, on_event)?
        .into_iter()
        .collect();
    let ordered: Vec<StorePath> = topo_sort(&infos)
        .into_iter()
        .filter(|p| !valid.contains(&p.to_string()))
        .collect();
    if ordered.is_empty() {
        return Ok(0);
    }
    let n = ordered.len();
    let cref = &c;
    let infos_ref = &infos;
    let order = &ordered;
    let mut body = |w: &mut dyn Write| -> io::Result<()> {
        write_u64(w, n as u64)?;
        let mut started = vec![false; n];
        fetch_nars(cref, order, &mut |i, bytes, _eof| {
            if !started[i] {
                started[i] = true;
                infos_ref[&order[i]].write(w)?;
            }
            w.write_all(bytes)
        })
        .map_err(io::Error::other)
    };
    conn.add_multiple_to_store(false, &mut body, on_event)
        .map_err(|e| {
            Error::Protocol(format!(
                "{e}\nhint: '{}' publishes its outputs to a binary cache. The local daemon \
             checks their signatures, as `nix copy --from` does, so add the cache's public \
             key to `trusted-public-keys` in nix.conf, or use --no-link.",
                c.cfg.url()
            ))
        })?;
    Ok(n)
}

/// The closure of `paths` on the remote store, as path infos.
fn remote_closure(
    c: &mut Client,
    paths: &[StorePath],
    t: &mut Timings,
) -> Result<HashMap<StorePath, PathInfo>> {
    let mut infos: HashMap<StorePath, PathInfo> = HashMap::new();
    let mut queue: Vec<StorePath> = paths.to_vec();
    while !queue.is_empty() {
        let batch: Vec<StorePath> = mem::take(&mut queue)
            .into_iter()
            .filter(|p| !infos.contains_key(p))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if batch.is_empty() {
            break;
        }
        let got = query_path_infos(c, &batch, t)?;
        for p in &batch {
            let Some(info) = got.get(p) else {
                return Err(Error::Protocol(format!(
                    "'{p}' has no path info on '{}'",
                    c.cfg.authority
                )));
            };
            for r in &info.references {
                let rp = StorePath::parse(r)?;
                if !infos.contains_key(&rp) {
                    queue.push(rp);
                }
            }
            infos.insert(p.clone(), info.clone());
        }
    }
    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::build::backend::tests::every_timing;

    #[test]
    fn the_timings_report_with_every_line() {
        let t = Timings {
            common: every_timing(),
            drvs: 11,
            drvs_cached: 10,
            calls: 12,
        };
        assert_eq!(
            t.report(&Plan::default(), Duration::from_millis(9)),
            "plan: 0 sources, 0 drvs, 0.009s\n\
             connect: gRPC, trusted, 0.001s\n\
             query: 9 paths, 8 missing, 0.002s\n\
             resolve: 7 sources, 6 not from this eval, read from /nix/store, 0.003s\n\
             substitute: 5 paths left to the store, 0.004s\n\
             add: 4 paths, 3 bytes, 0.005s\n\
             build: 11 drvs, 10 already valid, 0.006s\n\
             outputs: 0.007s\n\
             fetch: 2 outputs, 0.008s\n\
             link: 1 out-links, 3 paths copied, 0.010s\n\
             grpc: 12 calls"
        );
    }

    #[test]
    fn a_local_upload_failure_beats_the_stream_ended_status() {
        let ended = || Err::<(), _>(tonic::Status::unavailable("gRPC stream ended"));
        let local = io::Error::other("source 'x' changed since evaluation");
        let e = upload_outcome(ended(), Err(local)).unwrap_err();
        assert!(e.message().contains("changed since evaluation"), "{e}");
        let e = upload_outcome(ended(), Err(closed())).unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unavailable);
        assert!(upload_outcome(Ok(()), Ok(())).is_ok());
    }

    #[test]
    fn reschedules_back_off_past_a_dead_worker() {
        let p: Vec<u64> = (1..=8).map(|n| reschedule_pause(n).as_secs()).collect();
        assert_eq!(p, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(reschedule_pause(u32::MAX).as_secs(), 30);
    }

    fn info(name: &str, refs: &[&str]) -> PathInfo {
        PathInfo {
            path: StorePath::from_base_name(name).unwrap(),
            deriver: String::new(),
            nar_hash: "0".repeat(64),
            references: refs.iter().map(|r| format!("/nix/store/{r}")).collect(),
            registration_time: 0,
            nar_size: 0,
            ultimate: false,
            sigs: vec![],
            ca: String::new(),
        }
    }

    #[test]
    fn topo_sort_puts_references_first() {
        let a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
        let b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b";
        let c = "cccccccccccccccccccccccccccccccc-c";
        let mut infos = HashMap::new();
        for i in [info(a, &[b]), info(b, &[c]), info(c, &[])] {
            infos.insert(i.path.clone(), i);
        }
        let order: Vec<String> = topo_sort(&infos)
            .into_iter()
            .map(|p| p.base_name().to_string())
            .collect();
        assert_eq!(order, vec![c.to_string(), b.to_string(), a.to_string()]);
    }

    #[test]
    fn a_self_reference_does_not_loop() {
        let a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
        let mut infos = HashMap::new();
        let i = info(a, &[a]);
        infos.insert(i.path.clone(), i);
        assert_eq!(topo_sort(&infos).len(), 1);
    }

    #[test]
    fn required_features_come_from_the_env_or_structured_attrs() {
        let mut drv = Derivation {
            name: "x".into(),
            platform: "x86_64-linux".into(),
            ..Derivation::default()
        };
        drv.env_set("requiredSystemFeatures", "kvm big-parallel".into());
        assert_eq!(
            required_system_features(&drv),
            vec!["big-parallel".to_string(), "kvm".to_string()]
        );
        drv.structured_attrs = Some(r#"{"requiredSystemFeatures":["kvm"]}"#.into());
        assert_eq!(required_system_features(&drv), vec!["kvm".to_string()]);
    }
}
