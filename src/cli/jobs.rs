//! `iets eval-jobs`: flags, the three process roles, and the summary.

use std::process::exit;

use crate::cli::help::{self, arg_error, value};
use crate::cli::{
    self, EvalArgs, Input, abs_path, auto_args, cwd, evaluate, parse_eval_args, render, report,
    run_for,
};
use crate::error::{self, Error as CrateError};
use crate::eval::{Error, Evaluator, Pos};
use crate::jobs::cache::Resolver;
use crate::jobs::collector::{Out, Sink, run_collector, run_worker, write_out};
use crate::jobs::emit::Emitter;
use crate::jobs::instantiate::{self, Instantiator, Store};
use crate::jobs::{self, Line, Opts};
use crate::remote::worker::{Connection, Transport};
use crate::runtime::Word;
use crate::session::{self, Session};
use crate::{config, ignored, log};
use std::fs;
use std::io::{self, BufWriter};
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread::Builder;
use std::time::Instant;

/// nix-eval-jobs's `MyArgs::DEFAULT_MAX_MEMORY_SIZE` (eval-args.hh:18): an
/// arena without a GC cannot shed a walked subtree, so the default needs
/// the same process boundary nix-eval-jobs has.
const DEFAULT_MAX_MEMORY_MB: u64 = 4096;

/// `iets eval-jobs`'s own flags; everything else goes to `parse_eval_args`.
struct JobsArgs {
    opts: Opts,
    check_cache_status: bool,
    gc_roots_dir: Option<PathBuf>,
    eval_store: String,
    apply: Option<String>,
    select: Option<String>,
    /// `--max-memory-size MB`: worker processes restarted above it;
    /// `0` is one process with no ceiling.
    max_memory: u64,
    /// `--arg-from-stdin NAME`: what was read, for the worker argv.
    stdin_args: Vec<(String, String)>,
    worker: bool,
    flake: bool,
    /// Its own flag, not `EvalArgs::pure`: nix-eval-jobs.cc lets it beat `--flake`.
    impure: bool,
    quiet: bool,
}

fn parse_jobs_args(argv: &[String]) -> (JobsArgs, Vec<String>) {
    let mut j = JobsArgs {
        opts: Opts {
            force_recurse: false,
            meta: false,
            show_input_drvs: false,
            no_instantiate: false,
            constituents: false,
            chunks: 1,
            chunk: 0,
            chunk_depth: 2,
            provenance: false,
        },
        check_cache_status: false,
        gc_roots_dir: None,
        eval_store: "daemon".into(),
        apply: None,
        select: None,
        max_memory: DEFAULT_MAX_MEMORY_MB,
        stdin_args: Vec::new(),
        worker: false,
        flake: false,
        impure: false,
        quiet: false,
    };
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    let arg = |i: &mut usize| -> String {
        *i += 1;
        value(argv, *i, &argv[*i - 1]).clone()
    };
    let num = |i: &mut usize| -> u64 {
        let v = arg(i);
        v.parse()
            .unwrap_or_else(|_| arg_error(format!("{} wants a number, got '{v}'", argv[*i - 1])))
    };
    let unsupported = |flag: &str, why: &str| -> ! {
        exit(
            CrateError::Unsupported(format!(
                "{flag} is not supported by iets: {why} ('iets help commands')"
            ))
            .report(),
        )
    };
    while i < argv.len() {
        let s = argv[i].as_str();
        match s {
            "--force-recurse" => j.opts.force_recurse = true,
            "--meta" => j.opts.meta = true,
            "--show-input-drvs" => j.opts.show_input_drvs = true,
            "--no-instantiate" => j.opts.no_instantiate = true,
            "--constituents" => j.opts.constituents = true,
            "--provenance" => j.opts.provenance = true,
            "--check-cache-status" => j.check_cache_status = true,
            "--chunks" => j.opts.chunks = (num(&mut i) as u32).max(1),
            "--chunk" => j.opts.chunk = num(&mut i) as u32,
            "--chunk-depth" => j.opts.chunk_depth = num(&mut i) as u32,
            "--max-memory-size" => j.max_memory = num(&mut i),
            "--apply" => j.apply = Some(arg(&mut i)),
            "--select" => j.select = Some(arg(&mut i)),
            "--gc-roots-dir" => {
                let d = arg(&mut i);
                let cwd = cwd();
                j.gc_roots_dir = Some(PathBuf::from(
                    <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&abs_path(
                        &d, &cwd,
                    )),
                ));
            }
            "--eval-store" => j.eval_store = arg(&mut i),
            // holds by construction: it caps the builds of an import from
            // derivation, and iets builds nothing while it evaluates
            "--max-jobs" | "-j" => {
                let _ = arg(&mut i);
            }
            "--flake" => j.flake = true,
            "--impure" => j.impure = true,
            "--worker" => j.worker = true,
            "--quiet" => j.quiet = true,
            "--verbose" | "--debug" => ignored::flag(s, ignored::ONE_LOG_LEVEL),
            "--override-input" | "--override-flake" => unsupported(
                s,
                "iets evaluates locked flakes only; an override makes the lock file incomplete",
            ),
            "--repair" => unsupported(s, "iets does not write or repair store paths"),
            "--arg-from-file" => {
                let name = arg(&mut i);
                let path = arg(&mut i);
                let contents = fs::read_to_string(&path).unwrap_or_else(|e| {
                    exit(CrateError::io(format!("cannot read '{path}'"), e).report())
                });
                rest.extend(["--argstr".to_string(), name, contents]);
            }
            "--arg-from-stdin" => {
                let name = arg(&mut i);
                let contents = io::read_to_string(io::stdin())
                    .unwrap_or_else(|e| exit(CrateError::io("cannot read stdin", e).report()));
                rest.extend(["--argstr".to_string(), name.clone(), contents.clone()]);
                j.stdin_args.push((name, contents));
            }
            "--include" => {
                rest.push("-I".to_string());
                rest.push(arg(&mut i));
            }
            _ => rest.push(s.to_string()),
        }
        i += 1;
    }
    (j, rest)
}

/// A worker process's arguments: `argv` with every `--arg-from-stdin`
/// replaced by the `--argstr` the main process read for it. A worker's
/// stdin is the collector's pipe, so it cannot read the value itself.
fn worker_argv(argv: &[String], j: &JobsArgs) -> Vec<String> {
    // `--iets-option` left `argv` before this command saw it, and a worker
    // reads its own settings, so the triples go back on here
    let mut out: Vec<String> = Session::current().settings().option_argv();
    let mut read = j.stdin_args.iter();
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--arg-from-stdin" {
            if let Some((name, contents)) = read.next() {
                out.extend(["--argstr".to_string(), name.clone(), contents.clone()]);
            }
            i += 2;
            continue;
        }
        out.push(argv[i].clone());
        i += 1;
    }
    out
}

/// nix-eval-jobs.cc:validateIncompatibleFlags.
fn check_jobs_args(j: &JobsArgs) -> error::Result<Transport> {
    if j.opts.chunk >= j.opts.chunks {
        return Err(CrateError::Usage("--chunk must be below --chunks".into()));
    }
    if j.opts.no_instantiate {
        let mut bad = Vec::new();
        if j.opts.show_input_drvs {
            bad.push("--show-input-drvs");
        }
        if j.check_cache_status {
            bad.push("--check-cache-status");
        }
        if j.opts.constituents {
            bad.push("--constituents");
        }
        if !bad.is_empty() {
            return Err(CrateError::Usage(format!(
                "--no-instantiate is incompatible with: {}. These features require instantiated derivations.",
                bad.join(", ")
            )));
        }
    }
    Transport::parse(&j.eval_store).ok_or_else(|| {
        CrateError::Usage(format!(
            "--eval-store '{}': eval-jobs needs a Nix daemon: daemon, unix:///path or ssh-ng://host",
            j.eval_store
        ))
    })
}

/// `iets eval-jobs`: nix-eval-jobs's JSON lines. Worker processes
/// restarted above `--max-memory-size` (4096 MB, like nix-eval-jobs);
/// `--max-memory-size 0` is one process with no ceiling.
/// See docs/reference/commands.md.
pub fn cmd_eval_jobs(argv: &[String]) -> i32 {
    let (j, rest) = parse_jobs_args(argv);
    if j.worker {
        ignored::hush();
    }
    let mut a = parse_eval_args(&rest);
    a.no_tracing_cache = true;
    a.flake_whole = true;
    // nix-eval-jobs takes the argument as a file unless --flake says otherwise.
    a.input = match a.input.take() {
        Some(Input::Flake(s)) | Some(Input::File(s)) => Some(if j.flake {
            Input::Flake(s)
        } else {
            Input::File(s)
        }),
        other => other,
    };
    if a.input.is_none() {
        return CrateError::Usage("no expression specified".into()).report();
    }
    // nix-eval-jobs.cc: `--impure` wins, then `--flake` makes it pure, then
    // `--pure-eval`, `--option pure-eval` and nix.conf decide.
    if j.impure {
        a.pure = Some(false);
    } else if j.flake {
        a.pure = Some(true);
    } else if a.pure.is_none() {
        a.pure = config::Config::load(&[])
            .ok()
            .and_then(|c| c.bool("pure-eval").ok().flatten());
    }
    let transport = match check_jobs_args(&j) {
        Ok(t) => t,
        Err(e) => return e.report(),
    };
    let t0 = Instant::now();
    // the process model is this command's own: the collector evaluates
    // nothing itself, and a worker is one evaluator thread with no cache
    if !j.worker && j.max_memory > 0 {
        return jobs_collector(&worker_argv(argv, &j), a.workers, &j, &transport, t0);
    }
    let mut run = run_for(&a);
    run.cfg = run.cfg.keep_aterm(!j.opts.no_instantiate);
    run.cfg = run.cfg.keep_site(j.opts.provenance);
    if j.worker {
        let run = run.workers(1);
        return cli::run_parallel(run, move |ev, _| jobs_worker(ev, a, &j, &transport));
    }
    cli::run_parallel(run, move |ev, _| jobs_single(ev, a, &j, &transport, t0))
}

/// worker.cc:initializeRootValue: the root, auto-called with the
/// `--arg`s, then `--select`ed; `jobs::configure` done on the way.
fn jobs_root(ev: &mut Evaluator, a: EvalArgs, j: &JobsArgs) -> Result<Word, Box<Error>> {
    let cwd = cwd();
    let args = auto_args(ev, &a.args, &cwd)?;
    let apply = match &j.apply {
        Some(e) => Some(ev.eval_string(e, &cwd)?),
        None => None,
    };
    jobs::configure(ev, j.opts, args, apply);
    let root = evaluate(
        ev,
        EvalArgs {
            args: Vec::new(),
            ..a
        },
    )?;
    let root = ev.auto_call(root, args)?;
    let Some(select) = &j.select else {
        return Ok(root);
    };
    let f = ev.eval_string(select, &cwd)?;
    let v = ev.apply(f, &[root], Pos::NONE)?;
    ev.force_attrs(
        v,
        Pos::NONE,
        "'--select' must evaluate to an attrset (the traversal root)",
    )?;
    Ok(v)
}

fn jobs_summary(j: &JobsArgs, stats: jobs::Stats, inst: Option<instantiate::Stats>, t0: Instant) {
    // the real nix-eval-jobs says nothing, so the classic name says nothing
    if j.quiet || help::command().is_none() {
        return;
    }
    let mut s = format!(
        "eval-jobs: {} jobs, {} errors, {:.3}s, {}",
        stats.jobs,
        stats.errors,
        t0.elapsed().as_secs_f64(),
        if j.max_memory > 0 {
            format!(
                "{} worker processes spawned",
                crate::session::Session::current()
                    .jobs
                    .spawned
                    .load(std::sync::atomic::Ordering::Relaxed)
            )
        } else {
            format!("peak RSS {} MB", crate::jobs::collector::peak_rss_mb())
        }
    );
    if stats.jobs + stats.errors == 0 && j.opts.chunks == 1 {
        s.push_str("\nhint: the root has no derivations");
        if !j.opts.force_recurse {
            s.push_str(
                "; a nested set counts with recurseForDerivations = true or --force-recurse",
            );
        }
    }
    if let Some(i) = inst {
        s.push_str(&format!(
            "\ninstantiate: {} batches, {} paths queried, {} added ({} MB), {} roots, {:.3}s",
            i.upload.batches,
            i.upload.queried,
            i.upload.added,
            i.upload.bytes >> 20,
            i.roots,
            i.upload.time.as_secs_f64()
        ));
    }
    render::status(&s);
}

/// The tail every role shares: the emitter's `finish` when a connection is
/// there for it, the summary line, and the exit code.
fn jobs_finish(
    emitter: &Mutex<Emitter>,
    j: &JobsArgs,
    conn: Option<&mut Connection>,
    inst: Option<instantiate::Stats>,
    t0: Instant,
) -> i32 {
    if let Some(conn) = conn
        && let Err(e) = emitter
            .lock()
            .unwrap()
            .finish(conn, j.gc_roots_dir.as_deref())
    {
        return e.report();
    }
    let em = emitter.lock().unwrap();
    // `| head`: the reader is gone, so the summary has nowhere to go either
    if em.closed {
        return 0;
    }
    jobs_summary(j, em.stats, inst, t0);
    if em.fatal { 1 } else { 0 }
}

/// One process: the walk on the main fiber, instantiation and cache
/// status on a thread beside it, constituents at the end.
fn jobs_single(
    ev: &mut Evaluator,
    a: EvalArgs,
    j: &JobsArgs,
    transport: &Transport,
    t0: Instant,
) -> i32 {
    let root = match jobs_root(ev, a, j) {
        Ok(r) => r,
        Err(e) => return report(ev, &e),
    };
    let stdout = Box::new(BufWriter::new(io::stdout()));
    let emitter = Arc::new(Mutex::new(Emitter::new(stdout, j.opts.constituents)));
    let mut inst_stats = None;
    let mut conn = None;
    if j.opts.no_instantiate {
        let em = emitter.clone();
        jobs::run(ev, root, &mut |line| em.lock().unwrap().emit(line));
        if ev.session().settings().stats >= 1 {
            ev.print_stats();
        }
    } else {
        let setup = Store::open(
            transport,
            j.gc_roots_dir.clone(),
            ev.drv.drvs.clone(),
            ev.known.clone(),
        )
        .and_then(|store| {
            let resolver = if j.check_cache_status {
                Some(Resolver::open(transport)?)
            } else {
                None
            };
            Ok((store, resolver))
        });
        let (store, mut resolver) = match setup {
            Ok(s) => s,
            Err(e) => return e.report(),
        };
        let em = emitter.clone();
        let inst = Instantiator::start(
            store,
            Box::new(move |mut lines| {
                if let Some(r) = &mut resolver {
                    r.resolve(&mut lines)?;
                }
                let mut e = em.lock().unwrap();
                for l in lines {
                    e.emit(l);
                }
                Ok(())
            }),
        );
        jobs::run(ev, root, &mut |line| inst.push(line));
        let store = match inst.finish() {
            Ok(r) => r,
            Err(e) => return e.report(),
        };
        inst_stats = Some(store.stats);
        conn = Some(store.into_connection());
    }
    jobs_finish(&emitter, j, conn.as_mut(), inst_stats, t0)
}

/// A worker process under `--max-memory-size`: the root once, then
/// `do` requests from the collector until `exit` or the limit.
fn jobs_worker(ev: &mut Evaluator, a: EvalArgs, j: &JobsArgs, transport: &Transport) -> i32 {
    let out: Out = Arc::new(Mutex::new(Box::new(io::stdout())));
    let fail = |out: &Out, r: log::Report| -> i32 {
        let (msg, code) = (r.text(), r.emit());
        write_out(
            out,
            &format!(
                "{{\"error\":{}}}",
                String::from_utf8_lossy(&crate::jobs::line::json_str(&msg))
            )
            .into_bytes(),
        );
        code
    };
    let root = match jobs_root(ev, a, j) {
        Ok(r) => r,
        Err(e) => return fail(&out, jobs::error_report(ev, &e)),
    };
    let inst = if j.opts.no_instantiate {
        None
    } else {
        match Store::open(
            transport,
            j.gc_roots_dir.clone(),
            ev.drv.drvs.clone(),
            ev.known.clone(),
        ) {
            Ok(store) => {
                let o = out.clone();
                Some(Instantiator::start(
                    store,
                    Box::new(move |lines| {
                        for l in lines {
                            write_out(&o, &l.render());
                        }
                        Ok(())
                    }),
                ))
            }
            Err(e) => return fail(&out, e.to_report()),
        }
    };
    match run_worker(ev, root, inst, j.max_memory, &out) {
        Ok(()) => 0,
        Err(e) => fail(&out, e.to_report()),
    }
}

/// The main process under `--max-memory-size`: collector threads drive
/// the worker processes; cache status and constituents run here.
fn jobs_collector(
    argv: &[String],
    workers: usize,
    j: &JobsArgs,
    transport: &Transport,
    t0: Instant,
) -> i32 {
    let stdout = Box::new(BufWriter::new(io::stdout()));
    let emitter = Arc::new(Mutex::new(Emitter::new(stdout, j.opts.constituents)));
    let (tx, rx) = channel::<Line>();
    let resolver_thread = if j.check_cache_status {
        let mut resolver = match Resolver::open(transport) {
            Ok(r) => r,
            Err(e) => return e.report(),
        };
        let em = emitter.clone();
        Some(
            Builder::new()
                .name("cache-status".into())
                .stack_size(64 << 20)
                .spawn(session::bind(move || -> error::Result<()> {
                    while let Ok(first) = rx.recv() {
                        let mut batch = vec![first];
                        while let Ok(l) = rx.try_recv() {
                            batch.push(l);
                        }
                        resolver.resolve(&mut batch)?;
                        let mut e = em.lock().unwrap();
                        for l in batch {
                            e.emit(l);
                        }
                    }
                    Ok(())
                }))
                .expect("spawn cache-status thread"),
        )
    } else {
        drop(rx);
        None
    };
    let sink: Sink = {
        let em = emitter.clone();
        let tx = resolver_thread.is_some().then_some(tx);
        Arc::new(Mutex::new(Box::new(move |line: Line| match &tx {
            Some(tx) => {
                let _ = tx.send(line);
            }
            None => em.lock().unwrap().emit(line),
        })))
    };
    let r = run_collector(argv, workers, sink.clone());
    drop(sink);
    let r = r.and_then(|_| match resolver_thread {
        Some(t) => t
            .join()
            .map_err(|_| CrateError::Protocol("cache-status thread panicked".into()))?,
        None => Ok(()),
    });
    if let Err(e) = r {
        return e.report();
    }
    let mut conn = None;
    if j.opts.constituents {
        match Connection::open(transport, &mut |_| {}) {
            Ok(c) => conn = Some(c),
            Err(e) => return e.report(),
        }
    }
    jobs_finish(&emitter, j, conn.as_mut(), None, t0)
}
