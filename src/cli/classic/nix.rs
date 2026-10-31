//! `nix` (src/nix): the subcommands iets implements — `eval`, `build`,
//! `derivation show`, `print-dev-env`, `develop`, `search`, `fmt` and `flake
//! prefetch-inputs` — with the same exec fallback as the rest of classic mode
//! for everything else.
//! See `docs/reference/classic.md`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use super::{
    Argv, Common, common_flag, develop, drvshow, eval_config, fallback, fmt, local,
    lookup_file_arg, settings, shell_settings, store_given,
};
use crate::api::build::{self, Dispatch, Root};
use crate::api::{self, flake, installable};
use crate::build::render::Style;
use crate::cli::{
    self, EvalArgs, Input, Outcome, Run, abs_path, auto_args, cwd, devenv, render, report, search,
};
use crate::eval::{self, Error, ErrorKind, EvalConfig, EvalResult, Evaluator, Pos};
use crate::provenance::Provenance;
use crate::remote::plan::Plan;
use crate::remote::worker::{Options, Transport};
use crate::runtime::{Value, Word};
use crate::store::path::StorePath;
use crate::syntax::symbol::FxHashSet;
use crate::{config, error, ignored, print, session};

const NAME: &str = "nix";
/// filetransfer.hh: the default of `http-connections`.
const HTTP_CONNECTIONS: usize = 25;
const TOML_TIMESTAMPS: &str = "parse-toml-timestamps";

/// The flags accepted before the subcommand, and how many values each takes.
/// `global_flag` applies them; `find_sub` only needs the arity.
const GLOBALS: &[(&str, usize)] = &[
    ("--impure", 0),
    ("--pure-eval", 0),
    ("--no-pure-eval", 0),
    ("--accept-flake-config", 0),
    ("--show-trace", 0),
    ("--log-format", 1),
    ("--experimental-features", 1),
    ("--extra-experimental-features", 1),
    ("--option", 2),
    ("-I", 1),
    ("--include", 1),
    ("--arg", 2),
    ("--argstr", 2),
    ("--store", 1),
    ("--flake-registry", 1),
    // The `nix-farm` wrapper passes `--eval-store auto` because the farm
    // refuses the worker-protocol tunnel; iets always evaluates locally.
    ("--eval-store", 1),
    ("-j", 1),
    ("--max-jobs", 1),
    ("--cores", 1),
    // `nixos-rebuild --verbose` passes this to `nix build`; Nix takes it as
    // a global flag that raises the log level.
    ("--verbose", 0),
    ("-v", 0),
    ("--quiet", 0),
    ("-k", 0),
    ("--keep-going", 0),
];

#[derive(Default)]
pub(super) struct Nix {
    pub c: Common,
    json: bool,
    raw: bool,
    pub expr: Option<String>,
    pub file: Option<String>,
    pub installables: Vec<String>,
    /// The `-o`/`--out-link` prefix; `--no-link` clears it.
    out_link: String,
    no_link: bool,
    print_out_paths: bool,
    print_build_logs: bool,
    /// `derivation show -r`.
    pub recursive: bool,
    /// `--pretty`/`--no-pretty`; `None` leaves the terminal default.
    pub pretty: Option<bool>,
}

/// Runs `nix <subcommand>`. `argv` is everything after `argv[0]`.
pub fn main(argv: &[String]) -> i32 {
    let k = match find_sub(argv) {
        Ok(k) => k,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let mut rest = argv.to_vec();
    let sub = rest.remove(k);
    match sub.as_str() {
        "eval" => eval_main(argv, &rest),
        "build" => build_main(argv, &rest),
        "print-dev-env" => dev_env_main(argv, &rest),
        "develop" => develop::main(argv, &rest),
        "fmt" => fmt::main(argv, &rest),
        "search" => search::classic_main(argv, &rest),
        "derivation" if rest.get(k).is_some_and(|s| s == "show") => {
            rest.remove(k);
            drvshow::main(argv, &rest)
        }
        "flake" if rest.get(k).is_some_and(|s| s == "prefetch-inputs") => {
            rest.remove(k);
            prefetch_inputs_main(argv, &rest)
        }
        s => fallback(NAME, argv, &format!("subcommand '{s}'")),
    }
}

/// The index of the subcommand word: the first argument that is not one of
/// the global flags iets accepts.
fn find_sub(argv: &[String]) -> Result<usize, String> {
    let mut i = 0;
    while i < argv.len() {
        let s = &argv[i];
        if !s.starts_with('-') {
            return Ok(i);
        }
        let Some((_, arity)) = GLOBALS.iter().find(|(f, _)| *f == s.as_str()) else {
            return Err(format!("'{s}' is not implemented"));
        };
        i += 1 + arity;
    }
    Err("no subcommand".into())
}

/// `Ok(true)`: consumed. `Ok(false)`: not one of ours. `Err`: fall back.
pub fn global_flag(c: &mut Common, a: &mut Argv, s: &str) -> Result<bool, String> {
    if !GLOBALS.iter().any(|(f, _)| *f == s) {
        return Ok(false);
    }
    match s {
        "--accept-flake-config" => ignored::flag_for_nix_config(s),
        // iets has every feature but this one, which changes what `fromTOML`
        // returns (docs/reference/divergences.md). In classic mode the
        // value is still a setting, because the builtins Nix gates on a
        // feature are gated here too (`builtins::install`).
        "--experimental-features" | "--extra-experimental-features" => {
            let v = a.value(s)?;
            if v.split_whitespace().any(|f| f == TOML_TIMESTAMPS) {
                return Err(format!("'{s} {TOML_TIMESTAMPS}' is not implemented"));
            }
            c.flags
                .overrides
                .push((s.trim_start_matches('-').to_string(), v));
        }
        "--eval-store" => match a.value(s)?.as_str() {
            "auto" | "daemon" | "" => {
                ignored::flag(s, "iets evaluates in memory and needs no evaluation store")
            }
            v => return Err(format!("'--eval-store {v}' is not implemented")),
        },
        _ => return common_flag(c, a, s),
    }
    Ok(true)
}

fn parse_eval(argv: &[String]) -> Result<Nix, String> {
    let mut n = Nix::default();
    n.parse(argv, |n, _, s| {
        match s {
            "--json" => n.json = true,
            "--raw" => n.raw = true,
            // an evaluation that would build falls back, so there is no log
            "-L" | "--print-build-logs" => {}
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    if n.raw && n.json {
        return Err("--raw and --json are mutually exclusive".into());
    }
    if n.installables.len() > 1 {
        return Err("'nix eval' takes one installable".into());
    }
    n.check_args_with_flake()?;
    Ok(n)
}

fn parse_build(argv: &[String]) -> Result<Nix, String> {
    let mut n = Nix {
        out_link: "result".into(),
        ..Nix::default()
    };
    n.parse(argv, |n, a, s| {
        match s {
            "-o" | "--out-link" => n.out_link = a.value(s)?,
            "--no-link" => n.no_link = true,
            "--print-out-paths" => n.print_out_paths = true,
            "-L" | "--print-build-logs" => n.print_build_logs = true,
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    if n.installables.is_empty() {
        n.installables.push(".".into());
    }
    n.check_args_with_flake()?;
    Ok(n)
}

impl Nix {
    /// The command line of a subcommand that takes installables. `own` gets
    /// each argument first and says whether it was one of its flags.
    pub(super) fn parse(
        &mut self,
        argv: &[String],
        mut own: impl FnMut(&mut Nix, &mut Argv, &str) -> Result<bool, String>,
    ) -> Result<(), String> {
        let mut a = Argv { all: argv, i: 0 };
        while a.i < argv.len() {
            let s = argv[a.i].clone();
            if !own(self, &mut a, &s)? {
                match s.as_str() {
                    "--expr" => self.expr = Some(a.value(&s)?),
                    "-f" | "--file" => self.file = Some(a.value(&s)?),
                    _ if global_flag(&mut self.c, &mut a, &s)? => {}
                    _ if s.starts_with('-') => return Err(format!("'{s}' is not implemented")),
                    _ => self.installables.push(s),
                }
            }
            a.i += 1;
        }
        if self.expr.is_some() && self.file.is_some() {
            return Err("'--file' and '--expr' are exclusive".into());
        }
        Ok(())
    }

    /// installable-flake.cc: a flake installable refuses `--arg`/`--argstr`, so
    /// the real nix reports that better than iets would.
    pub(super) fn check_args_with_flake(&self) -> Result<(), String> {
        if !self.c.flags.args.is_empty() && self.expr.is_none() && self.file.is_none() {
            return Err("'--arg' and '--argstr' are incompatible with flakes".into());
        }
        Ok(())
    }

    /// `ec` as one run of `subcommand` with its tracing cache. The key is
    /// what changes the answer: the installables, `--expr` or `--file`, the
    /// `--arg`s, the search path, `pure-eval`, `--json` and `--raw`
    /// (`api::setup_cache_more` adds the system and the working directory).
    /// `strict` and `json` are the key fields a command without them is free
    /// to use. No other flag the parsers accept reaches the evaluator.
    pub(super) fn run(&self, ec: EvalConfig, subcommand: &str, strict: bool, json: bool) -> Run {
        let mut a = EvalArgs::new();
        let (first, more) = match self.installables.split_first() {
            Some((f, m)) => (f.clone(), m),
            None => (".".to_string(), &[][..]),
        };
        a.input = Some(match (&self.expr, &self.file) {
            (Some(e), _) => Input::Expr(e.clone()),
            (_, Some(f)) => Input::File(f.clone()),
            _ => Input::Flake(first.clone()),
        });
        if self.expr.is_some() || self.file.is_some() {
            a.attr = Some(first);
        }
        a.args = self.c.flags.args.clone();
        a.includes = self.c.flags.includes.clone();
        a.pure = self.c.flags.pure;
        a.strict = strict;
        a.json = json;
        a.raw = self.raw;
        Run::on(ec).cached_more(&a, subcommand, more)
    }
}

/// `settings` (`shell_settings` without `builder`) plus the `nix` command's
/// `pure-eval`. main.cc: pure unless a flag says otherwise; nix.conf's
/// setting does not change that, it only counts as set. installables.cc:
/// `--file` is impure, and refuses a `pure-eval` set anywhere.
pub fn nix_settings(
    c: &mut Common,
    file: bool,
    builder: bool,
) -> Result<(config::Config, Transport, Options), String> {
    let s = if builder {
        settings(c)
    } else {
        shell_settings(c)
    }?;
    let set = c.flags.pure.is_some() || s.0.overridden("pure-eval");
    let pure = c.flags.pure.unwrap_or(true);
    if file && pure && set {
        return Err("'--file' is not compatible with '--pure-eval'".into());
    }
    c.flags.pure = Some(pure && !file);
    Ok(s)
}

fn eval_main(argv: &[String], rest: &[String]) -> i32 {
    let mut n = match parse_eval(rest) {
        Ok(n) => n,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let (cfg, _, _) = match nix_settings(&mut n.c, n.file.is_some(), true) {
        Ok(t) => t,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let ec = eval_config(&n.c, &cfg);
    let run = n.run(ec, "nix eval", false, n.json);
    print_cached(argv, run, move |ev| eval_run(ev, &n))
}

/// Prints the stdout `f` makes, from the tracing cache when `run` hits it.
pub(super) fn print_cached(
    argv: &[String],
    run: Run,
    f: impl FnOnce(&mut Evaluator) -> error::Result<Vec<u8>> + Send + 'static,
) -> i32 {
    if let Some(text) = run.hit_result() {
        render::bytes(&text);
        return 0;
    }
    match cli::run_reported(run, move |ev, cache| {
        let text = f(ev)?;
        if let Some(c) = cache {
            c.write(ev, &text, false);
        }
        Ok(text)
    }) {
        Outcome::Failed(code) => code,
        Outcome::Unsupported(reason) => fallback(NAME, argv, &reason),
        Outcome::Ok(text) => {
            render::bytes(&text);
            0
        }
    }
}

/// `nix eval`: the whole stdout, already formatted.
fn eval_run(ev: &mut Evaluator, n: &Nix) -> error::Result<Vec<u8>> {
    let cwd = cwd();
    let mut out: Vec<u8> = Vec::new();
    let what = n.installables.first().map_or(".", String::as_str);
    let v = resolve(ev, n, what, &cwd)?;
    // installables.cc: the installable's value is forced before printing,
    // so an error at the top level is reported instead of printed.
    let v = ev.force(v)?;
    if n.raw {
        let mut ctx = Vec::new();
        let s = ev.coerce_to_string(
            v,
            &mut ctx,
            Pos::NONE,
            "while generating the eval command output",
            false,
            true,
            true,
        )?;
        out.extend_from_slice(&s);
    } else if n.json {
        let mut ctx = Vec::new();
        print::print_json(ev, v, &mut out, &mut ctx, true, Pos::NONE)?;
        out.push(b'\n');
    } else {
        let mut seen = HashSet::default();
        print_eval(ev, v, &mut out, &mut seen, 0)?;
        out.push(b'\n');
    }
    Ok(out)
}

fn build_main(argv: &[String], rest: &[String]) -> i32 {
    let mut n = match parse_build(rest) {
        Ok(n) => n,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let (cfg, transport, options) = match nix_settings(&mut n.c, n.file.is_some(), true) {
        Ok(t) => t,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let ec = eval_config(&n.c, &cfg);
    let run = n.run(ec, "nix build", false, false);
    // a hit skips the evaluation and nothing else: the upload, the build,
    // the out-links and the roots all happen in `realise`
    if let Some((roots, plan)) = run.hit_plan() {
        return realise(&n, &transport, &options, &remembered(roots), plan);
    }
    let n = Arc::new(n);
    let m = Arc::clone(&n);
    match cli::run_reported(run, move |ev, cache| {
        let r = build_run(ev, &m)?;
        if let Some(c) = cache {
            c.write_build(ev, &remember(&r.0));
        }
        Ok(r)
    }) {
        Outcome::Failed(code) => code,
        Outcome::Unsupported(reason) => fallback(NAME, argv, &reason),
        Outcome::Ok((targets, plan)) => realise(&n, &transport, &options, &targets, *plan),
    }
}

/// `nix build`: what to build, and the closure to upload.
fn build_run(ev: &mut Evaluator, n: &Nix) -> error::Result<(Vec<installable::Install>, Box<Plan>)> {
    let cwd = cwd();
    let mut targets: Vec<installable::Install> = Vec::new();
    for what in &n.installables {
        let v = resolve(ev, n, what, &cwd)?;
        targets.push(installable::to_install(
            ev,
            v,
            what,
            n.expr.is_none() && n.file.is_none(),
        )?);
    }
    let roots = flat_roots(&targets);
    let plan = build::plan(ev, &Root::paths(&roots))?;
    Ok((targets, Box::new(plan)))
}

/// flake-prefetch-inputs.cc: every locked input the lock reaches, resolved in
/// parallel the way an evaluation resolves it (`source::tree`), on as many
/// threads as `http-connections`' default. A lock that is missing or
/// incomplete needs locking, which the real nix does.
fn prefetch_inputs_main(argv: &[String], rest: &[String]) -> i32 {
    let mut c = Common::default();
    let mut refs = Vec::new();
    let mut a = Argv { all: rest, i: 0 };
    while a.i < rest.len() {
        let s = rest[a.i].clone();
        match global_flag(&mut c, &mut a, &s) {
            Ok(true) => {}
            Ok(false) if !s.starts_with('-') => refs.push(s),
            Ok(false) => fallback(NAME, argv, &format!("'{s}' is not implemented")),
            Err(why) => fallback(NAME, argv, &why),
        }
        a.i += 1;
    }
    if refs.len() > 1 {
        fallback(NAME, argv, "'nix flake prefetch-inputs' takes one flake");
    }
    let fr = api::flake_ref(refs.first().map_or(".", String::as_str), &cwd())
        .unwrap_or_else(|e| fallback(NAME, argv, &e.to_string()));
    let todo = flake::info(&fr)
        .and_then(|i| i.inputs())
        .unwrap_or_else(|e| fallback(NAME, argv, &e.to_string()));
    let system = config::Config::load(&c.flags.overrides)
        .unwrap_or_else(|e| fallback(NAME, argv, &e.to_string()))
        .system();
    let threads = todo.len().min(HTTP_CONNECTIONS);
    let queue = Mutex::new(todo.into_iter());
    let failed = AtomicBool::new(false);
    thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(session::bind(|| {
                while let Some((name, r)) = queue.lock().unwrap().next() {
                    let mut ev = EvalConfig::new().system(&system).pure(true).build();
                    if let Some(h) = r.nar_hash() {
                        ev.input_names.insert(h.to_string(), name);
                    }
                    if let Err(e) = flake::tree(&mut ev, &r) {
                        report(&ev, &e);
                        failed.store(true, Ordering::Relaxed);
                    }
                }
            }));
        }
    });
    i32::from(failed.into_inner())
}

fn dev_env_main(argv: &[String], rest: &[String]) -> i32 {
    let mut pass: Vec<String> = Vec::new();
    let mut flakes = 0;
    for s in rest {
        if s.starts_with('-') && s != "--json" {
            fallback(NAME, argv, &format!("'{s}' is not implemented"));
        }
        if !s.starts_with('-') {
            flakes += 1;
        }
        pass.push(s.clone());
    }
    if flakes > 1 {
        fallback(NAME, argv, "'nix print-dev-env' takes one installable");
    }
    // no flag reached here, so main.cc's default stands
    pass.push("--pure-eval".into());
    devenv::cmd_print_dev_env(&pass)
}

/// The value one installable names: an attribute of the `--expr`/`--file`
/// value, or a flake output attribute.
pub(super) fn resolve(ev: &mut Evaluator, n: &Nix, what: &str, cwd: &[u8]) -> EvalResult<Word> {
    let args = auto_args(ev, &n.c.flags.args, cwd)?;
    // installable-attr-path.cc:parse: `.` is the whole value.
    let attr = if what == "." { "" } else { what };
    if let Some(e) = &n.expr {
        let root = ev.eval_string(e, cwd)?;
        return installable::find_along_attr_path(ev, root, attr, args);
    }
    if let Some(f) = &n.file {
        let p = lookup_file_arg(ev, f, cwd)?;
        let root = ev.eval_file(&p, Pos::NONE)?;
        return installable::find_along_attr_path(ev, root, attr, args);
    }
    if what.contains('^') || what.as_bytes().starts_with(&ev.store_dir) {
        return Err(Error::boxed(
            ErrorKind::Unsupported,
            format!("the installable '{what}' is a store path"),
            Pos::NONE,
        ));
    }
    let (loc, fragment) = what.split_once('#').unwrap_or((what, ""));
    let fr = cli::flake_ref(loc, cwd)
        .map_err(|e| Error::boxed(ErrorKind::Unsupported, e.to_string(), Pos::NONE))?;
    let outputs = flake::outputs(ev, &fr)?;
    let mut found = installable::from_outputs(
        ev,
        outputs,
        loc,
        installable::Prefixes::Package,
        fragment,
        installable::Want::First,
    )?;
    Ok(found.remove(0).value)
}

/// The targets as a cache entry's roots, one per installable. The outputs
/// are joined with a comma, which no output name can hold (store path names
/// are `[A-Za-z0-9+._?=-]`), so the installables stay apart: the n-th one
/// names its own out-link.
fn remember(targets: &[installable::Install]) -> Vec<(StorePath, Option<String>)> {
    (targets.iter())
        .map(|t| (t.drv.clone(), Some(t.outputs.join(","))))
        .collect()
}

fn remembered(roots: Vec<(StorePath, Option<String>)>) -> Vec<installable::Install> {
    (roots.into_iter())
        .map(|(drv, outputs)| installable::Install {
            drv,
            outputs: (outputs.iter())
                .flat_map(|o| o.split(','))
                .filter(|o| !o.is_empty())
                .map(str::to_string)
                .collect(),
        })
        .collect()
}

/// The `Root`s the plan and `BuildPathsWithResults` need, in target order.
fn flat_roots(targets: &[installable::Install]) -> Vec<Root> {
    (targets.iter())
        .flat_map(|t| {
            (t.outputs.iter()).map(|o| Root {
                drv: t.drv.clone(),
                output: o.clone(),
            })
        })
        .collect()
}

/// Uploads the closure, builds every target in one call, makes the symlinks
/// (command.cc:createOutLinks) and prints with `--print-out-paths`.
fn realise(
    n: &Nix,
    transport: &Transport,
    options: &Options,
    targets: &[installable::Install],
    plan: Plan,
) -> i32 {
    let roots = flat_roots(targets);
    let style = Style {
        quiet: !n.print_build_logs,
        ..Style::detect()
    };
    let mut d = match Dispatch::open(transport, options, plan, Provenance::off(), style) {
        Ok(d) => d,
        Err(e) => return e.report(),
    };
    let built = match d.realise(&roots, &|p: &StorePath| p.name().to_string()) {
        Ok(b) => b,
        Err(e) => return e.report(),
    };
    // a daemon client nix exits 1 on a failed build, whatever the status
    if d.d.exit != 0 {
        return 1;
    }
    let paths = match built.paths(&roots) {
        Ok(p) => p,
        Err(e) => return e.report(),
    };

    let cwd = cwd();
    let mut out = String::new();
    let mut links: Vec<(String, Option<String>)> = Vec::new();
    let mut kept: Vec<(String, String)> = Vec::new();
    let mut at = 0;
    for (i, t) in targets.iter().enumerate() {
        for output in &t.outputs {
            let path = &paths[at];
            at += 1;
            if n.no_link {
                if !store_given(&n.c) {
                    links.push((path.clone(), None));
                    kept.push((format!("{i}!{output}"), path.clone()));
                }
            } else {
                // build.cc: the n-th installable adds `-n` from 1 on, an
                // output other than `out` adds `-<output>`.
                let mut link = n.out_link.clone();
                if i > 0 {
                    link.push_str(&format!("-{i}"));
                }
                if output != "out" {
                    link.push('-');
                    link.push_str(output);
                }
                let link = String::from_utf8_lossy(&abs_path(&link, &cwd)).into_owned();
                links.push((path.clone(), Some(link)));
            }
            if n.print_out_paths {
                out.push_str(path);
                out.push('\n');
            }
        }
    }
    if let Err(e) = d.d.out_links(&links) {
        return e.report();
    }
    // `--no-link` keeps a root instead of nothing, as `nix-build --no-out-link`
    // does (src/cli/classic/build.rs, docs/reference/divergences.md)
    let mut key: Vec<Vec<u8>> = vec![b"nix build".to_vec(), cwd];
    key.extend(n.installables.iter().map(|i| i.as_bytes().to_vec()));
    key.extend(
        n.file
            .iter()
            .chain(n.expr.iter())
            .map(|s| s.as_bytes().to_vec()),
    );
    devenv::keep_outputs(&local(), &key, &kept);
    render::bytes(out.as_bytes());
    0
}

/// An error the printer shows in place of the value. `Unsupported` must
/// escape so the caller can fall back to the real nix instead.
fn printable(e: &Error) -> bool {
    !matches!(e.kind, ErrorKind::Unsupported | ErrorKind::StackOverflow)
}

/// `nix eval`'s printer (print.cc:Printer with `force`, `derivationPaths`
/// and `trackRepeated`): forces as it goes, prints a derivation as its drv
/// path and a failing value as `«error: …»`.
fn print_eval(
    ev: &mut Evaluator,
    w: Word,
    out: &mut Vec<u8>,
    seen: &mut FxHashSet<u64>,
    depth: usize,
) -> EvalResult<()> {
    if depth > eval::MAX_CALL_DEPTH as usize {
        return Err(Error::stack_overflow(Pos::NONE));
    }
    let w = match ev.force(w) {
        Ok(w) => w,
        Err(e) if printable(&e) => {
            // docs/explanation/caching.md: errors are never cached. A read
            // that failed leaves no observation of the absence, so an entry
            // holding this text would replay it after the path appeared.
            ev.trace().mark_uncacheable();
            out.extend_from_slice("«error: ".as_bytes());
            out.extend_from_slice(e.msg.as_bytes());
            out.extend_from_slice("»".as_bytes());
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    match w.view() {
        Value::Attrs(a) => {
            // Nix keys the seen set on the `Bindings` and shares one empty
            // `Bindings`, so the second empty attrset is «repeated».
            let key = if a.is_empty() { 0 } else { w.bits() };
            if !seen.insert(key) {
                out.extend_from_slice("«repeated»".as_bytes());
                return Ok(());
            }
            if let Some(drv) = derivation_path(ev, w)? {
                out.extend_from_slice("«derivation".as_bytes());
                if !drv.is_empty() {
                    out.push(b' ');
                    out.extend_from_slice(&drv);
                }
                out.extend_from_slice("»".as_bytes());
                return Ok(());
            }
            out.extend_from_slice(b"{ ");
            for attr in print::lexicographic_order(ev, a) {
                print::print_attr_name(out, ev.interner.name(attr.sym));
                out.extend_from_slice(b" = ");
                print_eval(ev, attr.cell.get(), out, seen, depth + 1)?;
                out.extend_from_slice(b"; ");
            }
            out.push(b'}');
        }
        Value::List(l) => {
            if l.len > 0 && !seen.insert(w.bits()) {
                out.extend_from_slice("«repeated»".as_bytes());
                return Ok(());
            }
            out.extend_from_slice(b"[ ");
            for c in l.cells() {
                print_eval(ev, c.get(), out, seen, depth + 1)?;
                out.push(b' ');
            }
            out.push(b']');
        }
        Value::PrimOp(p) => {
            out.extend_from_slice("«primop ".as_bytes());
            out.extend_from_slice(primop_name(p.name).as_bytes());
            out.extend_from_slice("»".as_bytes());
        }
        Value::PrimOpApp(p) => {
            out.extend_from_slice("«partially applied primop ".as_bytes());
            out.extend_from_slice(primop_name(p.op.name).as_bytes());
            out.extend_from_slice("»".as_bytes());
        }
        Value::Lambda(_) => {
            out.extend_from_slice(print::show_value_short(ev, w).as_bytes());
        }
        _ => {
            let mut one = HashSet::default();
            print::print_ambiguous(ev, w, 1, out, &mut one, depth)?;
        }
    }
    Ok(())
}

/// eval.cc:addPrimOp drops a leading `__` from a primop's name, so that is
/// the name print.cc:printFunction shows.
fn primop_name(name: &str) -> &str {
    name.strip_prefix("__").unwrap_or(name)
}

/// print.cc:printDerivation: `Some(drvPath)` when the attrset is a
/// derivation, which the printer shows instead of the attributes.
fn derivation_path(ev: &mut Evaluator, w: Word) -> EvalResult<Option<Vec<u8>>> {
    let Some(attrs) = w.as_attrs() else {
        return Ok(None);
    };
    let Some(t) = attrs.get(ev.s.type_) else {
        return Ok(None);
    };
    let t = ev.force_string(
        t.cell.get(),
        Pos::NONE,
        "while evaluating the 'type' attribute",
    )?;
    if t.bytes != b"derivation" {
        return Ok(None);
    }
    let Some(dp) = attrs.get(ev.s.drv_path) else {
        return Ok(Some(Vec::new()));
    };
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        dp.cell.get(),
        &mut ctx,
        dp.pos,
        "while evaluating the drvPath of a derivation",
        false,
        false,
        true,
    )?;
    Ok(Some(s.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn eval_takes_print_build_logs() {
        assert!(parse_eval(&args(&["-L", "--expr", "1"])).is_ok());
        assert!(parse_eval(&args(&["--print-build-logs", "--expr", "1"])).is_ok());
    }

    #[test]
    fn verbose_is_a_global_flag() {
        // `nixos-rebuild --verbose` builds with `nix build --verbose`.
        assert!(parse_build(&args(&["--verbose", ".#a"])).is_ok());
        assert!(parse_eval(&args(&["--verbose", "--expr", "1"])).is_ok());
        assert!(parse_build(&args(&["-v", ".#a"])).is_ok());
        assert!(parse_build(&args(&["--quiet", ".#a"])).is_ok());
        let before = args(&["--verbose", "build", ".#a"]);
        assert_eq!(find_sub(&before), Ok(1));
    }

    #[test]
    fn flake_registry_is_the_setting() {
        let n = parse_build(&args(&["--flake-registry", "", ".#a"])).unwrap();
        let set = ("flake-registry".to_string(), String::new());
        assert!(n.c.flags.overrides.contains(&set));
        let n = parse_eval(&args(&["--flake-registry", "/r.json", "--expr", "1"])).unwrap();
        let set = ("flake-registry".to_string(), "/r.json".to_string());
        assert!(n.c.flags.overrides.contains(&set));
        let before = args(&["--flake-registry", "", "eval", "--expr", "1"]);
        assert_eq!(find_sub(&before), Ok(2));
    }
}
