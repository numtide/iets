//! `iets print-dev-env`: the shell environment of a flake's devShell.

use std::sync::Arc;

use crate::api::installable::{self, drv_roots};
use crate::api::{self, build};
use crate::build::backend::{Backend, Batch, Job, Worker};
use crate::build::upload::{PlanUpload, Uploader};
use crate::builtins::derivation::{DrvInfo, load_drv_traced};
use crate::cli::help::{arg_error, each_arg};
use crate::cli::{
    self, EvalArgs, Input, Run, abs_path, cwd, evaluate, parse_eval_args, render, report, run_for,
};
use crate::devenv::{self, BuildEnvironment, CachedResult};
use crate::error::{self, Error as CrateError};
use crate::eval::{Error, ErrorKind, EvalConfig, Evaluator, Pos};
use crate::provenance::Provenance;
use crate::remote::event::Event;
use crate::remote::worker::{self, Connection, Transport, failed_event};
use crate::session::Session;
use crate::store::drv::Derivation;
use crate::store::nar;
use crate::store::path::{StorePath, store_dir};
use crate::store::source::SourceOrigin;
use crate::sys::{OsStrExt, tmp};
use crate::trace::run::Cache;
use crate::trace::store::Part;
use crate::{cachefs, config, log};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;
use std::{fs, slice, str};

struct DevEnvArgs {
    transport: Transport,
    /// `nix develop`'s `-j`, `--cores` and the rest; the defaults natively.
    options: worker::Options,
    gc_root: GcRoot,
}

/// Where the root of a shell's environment goes. A caller that named a path
/// owns it: the path is a contract and a failure is the command's. Where
/// nothing was named, `nix develop` takes a link in iets's roots store, and
/// that one is best effort — see `keep_root`.
pub enum GcRoot {
    None,
    /// `print-dev-env --gc-root PATH`, absolute.
    Given(PathBuf),
    /// A link in iets's roots store, from `shell_root`.
    Store(PathBuf),
}

impl GcRoot {
    /// The path the caller named, which also holds the `-attrs` directory the
    /// rc script points at. A link in the roots store is not one.
    fn given(&self) -> Option<&Path> {
        match self {
            GcRoot::Given(p) => Some(p),
            _ => None,
        }
    }

    /// Points this run's root at `out_path`.
    fn register(
        &self,
        transport: &Transport,
        out_path: &str,
        conn: Option<&mut Connection>,
    ) -> error::Result<()> {
        match self {
            GcRoot::None => Ok(()),
            GcRoot::Given(p) => ensure_gc_root(transport, p, out_path, conn),
            GcRoot::Store(p) => {
                keep_root(transport, p, out_path, conn);
                Ok(())
            }
        }
    }
}

/// The root `nix develop` keeps for the flake installable `f`. It has no flag
/// for one, so it keeps one of its own: the `-env` output references the whole
/// shell, so a single link holds it, keyed on the flake directory and the
/// attribute path — two flakes on one machine keep two roots, two shells of one
/// flake keep two, and one shell replaces its own in place
/// (`docs/explanation/caching.md`). `NIX_GCROOT` is not this root: that one
/// holds the closure only while the shell runs, which is what nix does with it.
pub fn flake_shell_root(f: &str, transport: &Transport) -> GcRoot {
    let Ok(fr) = api::flake_ref(f, &cwd()) else {
        return GcRoot::None;
    };
    let attr = fr.attr.join(".");
    let parts: [&[u8]; 3] = [b"develop", fr.dir.as_os_str().as_bytes(), attr.as_bytes()];
    shell_root(transport, &parts).map_or(GcRoot::None, GcRoot::Store)
}

/// The link iets keeps for a shell of its own accord, named by `parts` as
/// `nix fmt`'s is (`src/cli/classic/fmt.rs`). `None` when the `shell-gc-root`
/// setting is off, when the daemon is not local — only a local daemon takes a
/// root — or when no cache root is known.
pub(crate) fn shell_root(transport: &Transport, parts: &[&[u8]]) -> Option<PathBuf> {
    if !shell_roots(transport) {
        return None;
    }
    cachefs::root_link(None, parts)
}

/// Whether a shell keeps a root of its own: the `shell-gc-root` setting
/// (`docs/reference/settings.md`), and a local daemon, the only kind that takes
/// one. Ask before doing the work a root needs.
pub(crate) fn shell_roots(transport: &Transport) -> bool {
    own_root(Session::current().settings().shell_gc_root, transport)
}

/// The same question for a build's outputs, under `build-gc-root`.
fn build_roots(transport: &Transport) -> bool {
    own_root(Session::current().settings().build_gc_root, transport)
}

/// `on` is the setting that allows the root; only a local daemon takes one.
fn own_root(on: bool, transport: &Transport) -> bool {
    on && matches!(transport, crate::remote::worker::Transport::Unix(_))
}

/// Keeps the outputs a build printed alive under roots of iets's own: one
/// link in the roots store per output, named by `key` — where the command ran
/// and what it was asked to build — and by the name that tells two outputs of
/// one run apart, so a second run of the same command replaces its own roots in
/// place. A caller that named a path of its own does not come here: one root
/// per thing (`docs/explanation/caching.md`).
pub(crate) fn keep_outputs(transport: &Transport, key: &[Vec<u8>], outputs: &[(String, String)]) {
    if outputs.is_empty() || !build_roots(transport) {
        return;
    }
    let mut conn = Connection::open(transport, &mut |_| {}).ok();
    let mut parts: Vec<&[u8]> = key.iter().map(|p| p.as_slice()).collect();
    parts.push(b"");
    for (name, path) in outputs {
        let last = parts.len() - 1;
        parts[last] = name.as_bytes();
        let Some(link) = cachefs::root_link(None, &parts) else {
            return;
        };
        keep_root(transport, &link, path, conn.as_mut());
    }
}

/// Links `out_path` at `link` in the roots store, registers the link with the
/// daemon and marks it used, so the TTL sweep keeps the roots of the shells
/// somebody still enters. Best effort and silent: classic mode prints
/// what nix prints, byte for byte, so a root a daemon will not take costs the
/// root and nothing else.
pub(crate) fn keep_root(
    transport: &Transport,
    link: &Path,
    out_path: &str,
    conn: Option<&mut Connection>,
) {
    if let Some(dir) = link.parent()
        && fs::create_dir_all(dir).is_err()
    {
        return;
    }
    if ensure_gc_root(transport, link, out_path, conn).is_ok() {
        cachefs::touch_link(link);
    }
}

/// Build events on the daemon connection: log lines are printed live with `--timings`,
/// else kept and printed only when the build fails.
struct EventSink {
    live: bool,
    log: Vec<String>,
    failed: Option<String>,
    prov: Provenance,
}

impl EventSink {
    fn event(&mut self, e: Event) {
        match e {
            Event::Log(l) => {
                let line = self.prov.rewrite_str(&String::from_utf8_lossy(&l.line));
                if self.live {
                    log::line(&format!("iets: {line}"));
                } else {
                    self.log.push(line);
                }
            }
            Event::Started(s) if self.live => match self.prov.started(&s.drv_path) {
                Some(l) => log::line(&format!("iets: {l}")),
                None => log::line(&format!("iets: building {}", s.drv_path)),
            },
            Event::Failed(f) => {
                self.failed = Some(match self.prov.failed(&f.drv_path, &f.reason) {
                    Some(l) => l.trim_start_matches("error: ").to_string(),
                    None => format!(
                        "build of {} failed: {}",
                        f.drv_path,
                        self.prov.rewrite_str(&f.reason)
                    ),
                });
            }
            _ => {}
        }
    }

    fn dump(&self) {
        for l in &self.log {
            log::line(&format!("iets: {l}"));
        }
    }
}

/// `iets print-dev-env`: the bash `nix print-dev-env` prints for a flake's shell, from the
/// in-memory shell derivation through a Nix daemon. See docs/reference/commands.md.
pub fn cmd_print_dev_env(argv: &[String]) -> i32 {
    let mut url: Option<String> = None;
    let mut gc_root: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut flake: Option<String> = None;
    let mut entry_to: Option<String> = None;
    each_arg(argv, |s, next| match s {
        "--builder" => url = Some(next()),
        "--gc-root" => gc_root = Some(next()),
        "--entry-to" => entry_to = Some(next()),
        "--workers" | "--cache-dir" | "-I" | "--include" | "--log-format" => {
            let v = next();
            rest.extend([s.to_string(), v]);
        }
        s if s.starts_with('-') => rest.push(s.to_string()),
        s => {
            if let Some(old) = flake.replace(s.to_string()) {
                arg_error(format!("one FLAKEREF only, got '{old}' and '{s}'"))
            }
        }
    });
    let mut a = parse_eval_args(&rest);
    a.input = Some(Input::Flake(flake.unwrap_or_else(|| ".".into())));
    let cfg = config::Config::load(&[]).unwrap_or_else(|_| config::Config::empty());
    // the environment runs here, so its inputs belong in the local store:
    // the `builder` setting does not apply
    let url = url.unwrap_or_else(|| cfg.store());
    let Some(transport) = Transport::parse(&url) else {
        return CrateError::Usage(
            "print-dev-env needs a Nix daemon: --builder daemon, unix:///path or ssh-ng://host"
                .into(),
        )
        .report();
    };
    let cwd = cwd();
    let gc_root = match gc_root {
        Some(r) => GcRoot::Given(PathBuf::from(
            <std::ffi::OsStr as crate::sys::OsStrExt>::from_bytes(&abs_path(&r, &cwd)),
        )),
        None => GcRoot::None,
    };
    if matches!(transport, crate::remote::worker::Transport::Grpc(_)) {
        return CrateError::Usage("print-dev-env needs a Nix daemon; grpc:// only builds".into())
            .report();
    }
    if gc_root.given().is_some() && !matches!(transport, crate::remote::worker::Transport::Unix(_))
    {
        return CrateError::Usage("--gc-root needs a local daemon (daemon or unix://)".into())
            .report();
    }
    let options = worker::Options::default();
    let d = DevEnvArgs {
        transport,
        options,
        gc_root,
    };

    let t0 = Instant::now();
    let run = run_for(&a).cached(&a, "print-dev-env");
    // the caller asked where this run's entry lives, so it can hand that to
    // `iets cache paths` instead of deriving the key a second time
    if let Some(f) = &entry_to {
        let path = run.cache.as_ref().map(|c| c.entry_path());
        let Some(path) = path else {
            return error::Error::Cache(
                "--entry-to needs the tracing cache, and this run has none".into(),
            )
            .report();
        };
        if let Err(e) = fs::write(f, path.as_os_str().as_encoded_bytes()) {
            return error::Error::Io {
                context: format!("--entry-to '{f}'"),
                source: e,
            }
            .report();
        }
    }
    if let Some(code) = run.hit(|c| {
        let bytes = c.part(Part::Result)?;
        let hit = CachedResult::decode(&bytes)?;
        match dev_env_hit(&d, &hit) {
            Ok(true) => {
                c.replay_warnings();
                render::bytes_lossy(&hit.bytes);
                if a.timings {
                    eprintln!("total: {:.3}s", t0.elapsed().as_secs_f64());
                }
                Some(0)
            }
            Ok(false) => {
                if a.timings {
                    eprintln!("cache: env output '{}' is gone, rebuilding", hit.out_path);
                }
                None
            }
            Err(e) => Some(e.report()),
        }
    }) {
        return code;
    }
    // parallel: eval and the daemon flow run as the main fiber
    cli::run_parallel(run, move |ev, cache| dev_env_full(ev, a, &d, cache, t0))
}

/// A cache hit is usable when the env output (and the files the script names) still exist;
/// the GC root is refreshed when it does not point at the output.
fn dev_env_hit(d: &DevEnvArgs, hit: &CachedResult) -> error::Result<bool> {
    let present = match &d.transport {
        Transport::Unix(_) => Path::new(&hit.out_path).exists(),
        Transport::Ssh { .. } | Transport::Grpc(_) => {
            let mut conn = Connection::open(&d.transport, &mut |_| {})?;
            !conn
                .query_valid_paths(slice::from_ref(&hit.out_path), &mut |_| {})?
                .is_empty()
        }
    };
    if !present || !hit.files.iter().all(|f| f.exists()) {
        return Ok(false);
    }
    d.gc_root.register(&d.transport, &hit.out_path, None)?;
    Ok(true)
}

/// indirect-root-store.cc:addPermRoot: a symlink at `root` to `out_path`, then
/// `AddIndirectRoot`. Nothing happens when the link already points there.
pub(crate) fn ensure_gc_root(
    transport: &Transport,
    root: &Path,
    out_path: &str,
    conn: Option<&mut Connection>,
) -> error::Result<()> {
    if fs::read_link(root).ok().as_deref() == Some(Path::new(out_path)) {
        return Ok(());
    }
    if root.starts_with(store_dir()) {
        return Err(CrateError::Usage(format!(
            "creating a garbage collector root ({}) in the Nix store is forbidden",
            root.display()
        )));
    }
    if let Ok(m) = fs::symlink_metadata(root)
        && (!m.file_type().is_symlink()
            || !fs::read_link(root)
                .map(|t| t.starts_with(store_dir()))
                .unwrap_or(false))
    {
        return Err(CrateError::Usage(format!(
            "cannot create symlink {}; already exists",
            root.display()
        )));
    }
    worker::replace_symlink(out_path, root)
        .map_err(|e| CrateError::io(format!("cannot create {}", root.display()), e))?;
    let root_s = root
        .to_str()
        .ok_or_else(|| CrateError::Usage("gc root path is not UTF-8".into()))?;
    let mut opened;
    let conn = match conn {
        Some(c) => c,
        None => {
            opened = Connection::open(transport, &mut |_| {})?;
            &mut opened
        }
    };
    conn.add_indirect_root(root_s, &mut |_| {})
}

/// What the `-env` build produced: the environment `get-env.sh` dumped, the output that
/// holds it (`nix develop` makes it `NIX_GCROOT`), and the shell derivation's builder.
pub struct DevEnv {
    pub env: BuildEnvironment,
    pub out_path: String,
    /// `<store path>/bin/bash`: the bash the shell was built with.
    pub builder: String,
    /// When the daemon phase started, for the caller's `--timings` line.
    t1: Instant,
}

/// `print-dev-env` without the printing: the build environment of the flake installable
/// `a.input` names, on `base`. `nix develop` (src/cli/classic/develop.rs) uses this.
pub fn dev_env(
    base: EvalConfig,
    a: EvalArgs,
    transport: Transport,
    options: worker::Options,
    gc_root: GcRoot,
) -> Result<DevEnv, i32> {
    let d = DevEnvArgs {
        transport,
        options,
        gc_root,
    };
    let mut run = Run::on(base).workers(a.workers);
    // a hit reads the environment out of the local store, so only a local
    // daemon gets an entry
    if matches!(d.transport, crate::remote::worker::Transport::Unix(_)) {
        run = run.cached(&a, "develop");
    }
    if let Some(env) = run.hit(|c| {
        let env = remembered_env(&c.part(Part::Result)?)?;
        c.replay_warnings();
        Some(env)
    }) {
        return match d.gc_root.register(&d.transport, &env.out_path, None) {
            Ok(()) => Ok(env),
            Err(e) => Err(e.report()),
        };
    }
    cli::run_parallel(run, move |ev, cache| {
        let env = dev_env_build(ev, a, &d)?;
        if let Some(c) = cache {
            c.write(
                ev,
                format!("{}\n{}", env.out_path, env.builder).as_bytes(),
                false,
            );
        }
        Ok(env)
    })
}

/// What a `develop` entry holds is the `-env` output and the shell's builder,
/// neither of which can hold a newline. The environment itself stays in the
/// output: an output that is gone is a miss, and the run builds it again.
fn remembered_env(result: &[u8]) -> Option<DevEnv> {
    let text = str::from_utf8(result).ok()?;
    let (out_path, builder) = text.split_once('\n')?;
    let env = BuildEnvironment::parse(&fs::read(out_path).ok()?).ok()?;
    Some(DevEnv {
        env,
        out_path: out_path.to_string(),
        builder: builder.to_string(),
        t1: Instant::now(),
    })
}

/// The cache-miss path: evaluate the shell, derive and build its `-env` derivation through
/// the daemon, read the environment, print it, and remember the bytes.
fn dev_env_full(
    ev: &mut Evaluator,
    a: EvalArgs,
    d: &DevEnvArgs,
    cache: Option<Cache>,
    t0: Instant,
) -> i32 {
    let (timings, json) = (a.timings, a.json);
    let cwd = cwd();
    let built = match dev_env_build(ev, a, d) {
        Ok(b) => b,
        Err(code) => return code,
    };
    let r = (|| -> error::Result<Vec<u8>> {
        let (mut output, files) = if json {
            (
                serde_json::to_vec(&built.env.to_json())
                    .map_err(|e| CrateError::Protocol(e.to_string()))?,
                Vec::new(),
            )
        } else {
            let attrs_dir = match d.gc_root.given() {
                Some(r) => PathBuf::from(format!("{}-attrs", r.display())),
                // only structured attributes are written there
                None if built.env.structured_attrs.is_none() => PathBuf::new(),
                None => tmp::private_dir("iets-dev-env-")?,
            };
            let outputs_dir =
                Path::new(<std::ffi::OsStr as crate::sys::OsStrExt>::from_bytes(&cwd))
                    .join("outputs");
            let rc = devenv::make_rc_script(&built.env, &attrs_dir, &outputs_dir)?;
            (rc.script.into_bytes(), rc.files)
        };
        output.push(b'\n');
        if let Some(c) = &cache {
            c.write(
                ev,
                &CachedResult {
                    out_path: built.out_path.clone(),
                    files,
                    bytes: output.clone(),
                }
                .encode(),
                false,
            );
        }
        if timings {
            eprintln!("daemon: {:.3}s", built.t1.elapsed().as_secs_f64());
        }
        Ok(output)
    })();
    match r {
        Ok(out) => {
            render::bytes_lossy(&out);
            if timings {
                eprintln!("total: {:.3}s", t0.elapsed().as_secs_f64());
            }
            0
        }
        Err(e) => e.report(),
    }
}

/// Evaluates the shell, derives and builds its `-env` derivation through the daemon and
/// reads the environment back. The error is already reported; `Err` is the exit code.
fn dev_env_build(ev: &mut Evaluator, a: EvalArgs, d: &DevEnvArgs) -> Result<DevEnv, i32> {
    let t0 = Instant::now();
    let timings = a.timings;
    let f = match &a.input {
        Some(Input::Flake(f)) => f.clone(),
        _ => arg_error("print-dev-env needs a FLAKEREF"),
    };
    let (loc, frag) = f.split_once('#').unwrap_or((f.as_str(), ""));
    let mut roots = Vec::new();
    let r = (|| {
        let outputs = evaluate(
            ev,
            EvalArgs {
                input: Some(Input::Flake(loc.to_string())),
                ..a
            },
        )?;
        let mut found = installable::from_outputs(
            ev,
            outputs,
            loc,
            installable::Prefixes::DevShell,
            frag,
            installable::Want::First,
        )?;
        drv_roots(ev, found.remove(0).value, &mut roots)
    })();
    if let Err(e) = r {
        return Err(report(ev, &e));
    }
    if roots.len() != 1 {
        let msg = format!(
            "'{f}' needs to evaluate to a single derivation, but it evaluated to {} derivations",
            roots.len()
        );
        return Err(CrateError::Eval(Error::boxed(ErrorKind::Eval, msg, Pos::NONE)).report());
    }
    let shell_path = roots[0].0.clone();
    let eval_time = t0.elapsed();

    let r = (|| -> error::Result<DevEnv> {
        let shell = match ev.drv.drvs.get(&shell_path).and_then(|i| i.aterm) {
            Some(aterm) => Derivation::parse(&aterm, Derivation::name_from_path(&shell_path)?),
            None => load_drv_traced(ev.trace(), &shell_path),
        }?;
        let trace = ev.trace().clone();
        let mut loader = move |p: &StorePath| load_drv_traced(&trace, p);
        let env_drv =
            devenv::env_derivation(&shell, &mut ev.drv.hash_cache.lock().unwrap(), &mut loader)?;
        let text = env_drv.unparse()?;
        let env_path = env_drv.drv_path_for(&text)?;
        let mut references = env_drv.input_srcs.clone();
        references.extend(env_drv.input_drvs.keys().cloned());
        ev.drv.drvs.insert(
            env_path.clone(),
            DrvInfo {
                outputs: env_drv.outputs.keys().cloned().collect(),
                references,
                aterm: Some(Arc::new(text)),
                pos: None,
                attr: None,
            },
        );
        ev.known.add(
            devenv::get_env_sh_path(),
            SourceOrigin::text(devenv::GET_ENV_SH.as_bytes().to_vec(), BTreeSet::new()),
        );
        let mut out_paths = Vec::new();
        for (name, out) in &env_drv.outputs {
            match out.path(&env_drv.name, name)? {
                Some(p) => out_paths.push(p.to_string()),
                None => {
                    return Err(CrateError::Store(
                        "content-addressed shells are not supported".into(),
                    ));
                }
            }
        }
        if timings {
            eprintln!(
                "eval: {} derivations, {:.3}s\nenv drv: {env_path}",
                ev.counts().derivations,
                eval_time.as_secs_f64()
            );
        }

        let t1 = Instant::now();
        let mut sink = EventSink {
            live: timings,
            log: Vec::new(),
            failed: None,
            prov: Provenance::snapshot(ev),
        };
        let mut w = Worker::open(&d.transport, &d.options, &mut |e| sink.event(e))?;
        let valid = w
            .conn
            .query_valid_paths(&out_paths, &mut |e| sink.event(e))?;
        if valid.len() != out_paths.len() {
            let t2 = Instant::now();
            let plan = build::plan(ev, slice::from_ref(&env_path))?;
            let plan_time = t2.elapsed();
            PlanUpload::new(&plan, &build::locate_store).upload(
                &mut w,
                slice::from_ref(&env_path),
                &mut |e| sink.event(e),
            )?;
            let job = Job {
                drv: env_path.clone(),
                outputs: Vec::new(),
            };
            let batch = Batch {
                jobs: slice::from_ref(&job),
                drvs: &|_| Ok(Vec::new()),
            };
            for s in w.submit(&batch, &mut |e| sink.event(e))?.values() {
                if !s.ok {
                    sink.event(failed_event(&env_path.to_string(), &s.reason));
                }
            }
            if let Some(why) = &sink.failed {
                sink.dump();
                return Err(CrateError::Protocol(why.clone()));
            }
            if timings {
                eprintln!("{}", w.report(&plan, plan_time));
            }
        } else if timings {
            eprintln!(
                "daemon: env output already valid, {:.3}s",
                t1.elapsed().as_secs_f64()
            );
        }

        let (out_path, bytes) = read_env_output(&d.transport, &mut w.conn, &out_paths, &mut sink)?;
        let env = devenv::BuildEnvironment::parse(&bytes)?;
        d.gc_root
            .register(&d.transport, &out_path, Some(&mut w.conn))?;
        drop(w);
        Ok(DevEnv {
            env,
            out_path,
            builder: shell.builder.clone(),
            t1,
        })
    })();
    r.map_err(|e| e.report())
}

/// develop.cc:getDerivationEnvironment, after the build: the first output (by name) whose
/// file is not empty holds the JSON. A local daemon's output is read from disk; a remote one
/// arrives as a NAR.
fn read_env_output(
    transport: &Transport,
    conn: &mut Connection,
    out_paths: &[String],
    sink: &mut EventSink,
) -> error::Result<(String, Vec<u8>)> {
    for p in out_paths {
        let bytes = match transport {
            Transport::Unix(_) => {
                fs::read(p).map_err(|e| CrateError::io(format!("cannot read '{p}'"), e))?
            }
            Transport::Ssh { .. } | Transport::Grpc(_) => {
                let dir = tmp::private_dir("iets-dev-env-")?;
                let dest = dir.join(StorePath::parse(p)?.base_name());
                conn.nar_from_path(p, &mut |e| sink.event(e), &mut |r| {
                    nar::restore_path(r, &dest)
                })?;
                let bytes = fs::read(&dest)?;
                let _ = fs::remove_dir_all(&dir);
                bytes
            }
        };
        if !bytes.is_empty() {
            return Ok((p.clone(), bytes));
        }
    }
    Err(CrateError::Protocol(
        "get-env.sh failed to produce an environment".into(),
    ))
}
