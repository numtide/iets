//! The entry points a frontend needs: evaluation arguments, the evaluate
//! call, error reporting, the build target list and the cache key. The
//! `iets` command line is one frontend on top of this; docs/reference/library.md
//! lists the whole surface.

pub mod build;
pub mod drvshow;
pub mod fetch;
pub mod flake;
pub mod installable;
pub mod lock;
pub mod render;
pub mod run;
pub mod value;
pub mod watch;

use crate::builtins::text;
use crate::compile::cache::IrCache;
use crate::eval::{self, Error, ErrorKind, EvalConfig, Evaluator, Pos};
use crate::flake::{eval_flake, eval_flake_value, parse_flake_ref};
use crate::lint::{self, Level, Scope};
use crate::provenance::{describe_pos, rewrite_msg};
use crate::runtime::Word;
use crate::session::Session;
use crate::store::path::StorePath;
use crate::trace::key::{Key, has_relative_path};
use crate::trace::run::Cache;
use crate::{argv, compile, error, log, parallel, project};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::{env, fs};

/// Everything one evaluation needs. `EvalArgs::new()` holds the defaults;
/// `crate::cli::parse_eval_args` fills it from a command line.
pub struct EvalArgs {
    pub strict: bool,
    pub json: bool,
    pub raw: bool,
    pub includes: Vec<(Vec<u8>, Vec<u8>)>,
    pub args: Vec<(String, ArgKind)>,
    pub input: Option<Input>,
    pub dump_drvs: Option<String>,
    pub attr: Option<String>,
    /// `--lint SELECTOR=LEVEL` in order; applied after `iets.toml` and
    /// `IETS_LINT` (docs/reference/lints.md).
    pub lints: Vec<(String, Level)>,
    /// `--lint-root DIR`: lint roots after the entry directory.
    pub lint_roots: Vec<String>,
    /// `--lint-scope root|all`; `None` keeps `iets.toml`'s or the default.
    pub lint_scope: Option<Scope>,
    pub xp_features: Vec<String>,
    pub workers: usize,
    pub timings: bool,
    /// `--no-tracing-cache` or `IETS_CACHE=0`: evaluate, never replay a run.
    pub no_tracing_cache: bool,
    /// `--no-ir-cache` or `IETS_CACHE=0`: compile every import from source.
    pub no_ir_cache: bool,
    pub cache_dir: Option<String>,
    /// `eval-jobs --flake DIR` with no fragment: the whole `callFlake` result,
    /// not its `outputs`.
    pub flake_whole: bool,
    /// The last of `--pure-eval`, `--no-pure-eval`, `--impure` and `--option
    /// pure-eval`; `None` when none was given (docs/reference/settings.md).
    pub pure: Option<bool>,
    /// `--reference-lock-file PATH`: the root flake's lock file, in place of
    /// its own `flake.lock`. It must be complete all the same.
    pub reference_lock_file: Option<String>,
}

impl Default for EvalArgs {
    fn default() -> EvalArgs {
        let cache = Session::current().settings().cache;
        EvalArgs {
            strict: false,
            json: false,
            raw: false,
            includes: Vec::new(),
            args: Vec::new(),
            input: None,
            dump_drvs: None,
            attr: None,
            lints: Vec::new(),
            lint_roots: Vec::new(),
            lint_scope: None,
            xp_features: Vec::new(),
            workers: parallel::default_workers(),
            timings: false,
            no_tracing_cache: !cache,
            no_ir_cache: !cache,
            cache_dir: None,
            flake_whole: false,
            pure: None,
            reference_lock_file: None,
        }
    }
}

impl EvalArgs {
    pub fn new() -> EvalArgs {
        EvalArgs::default()
    }

    /// `base` plus what these arguments set: the lints, the IR cache, the
    /// search path and `--dump-drvs`. `base` holds the system and `pure-eval`.
    pub fn config(&self, base: EvalConfig) -> Result<EvalConfig, Box<Error>> {
        if self
            .xp_features
            .iter()
            .any(|f| f == "parse-toml-timestamps")
        {
            text::set_parse_toml_timestamps(true);
        }
        let c = base
            .lints(lint_config(self)?)
            .ir_cache(self.ir_cache())
            .search_path(full_includes(&self.includes));
        let Some(dir) = &self.dump_drvs else {
            return Ok(c);
        };
        fs::create_dir_all(dir).map_err(|e| {
            Error::boxed(
                ErrorKind::Io,
                format!("cannot create '{dir}': {e}"),
                Pos::NONE,
            )
        })?;
        Ok(c.dump_dir(dir.into()))
    }

    /// The compiled-file cache of `--no-ir-cache` and `--cache-dir`
    /// (docs/explanation/caching.md).
    pub fn ir_cache(&self) -> Option<Arc<IrCache>> {
        if self.no_ir_cache {
            return None;
        }
        IrCache::locate(self.cache_dir.as_deref(), self.timings).map(Arc::new)
    }
}

#[derive(Clone)]
pub enum ArgKind {
    Expr(String),
    Str(String),
}

#[derive(Clone)]
pub enum Input {
    Expr(String),
    File(String),
    /// `DIR#attr.path`, see `crate::api::flake`.
    Flake(String),
}

/// A `-I prefix=path` (or `-I path`) entry as an absolute pair.
pub fn parse_include(s: &str) -> (Vec<u8>, Vec<u8>) {
    let (prefix, path) = s.split_once('=').unwrap_or(("", s));
    (argv::bytes(prefix), abs_path(path, &cwd()))
}

fn home() -> Option<Vec<u8>> {
    env::var_os("HOME").map(|h| h.into_encoded_bytes())
}

pub fn flake_ref(f: &str, cwd: &[u8]) -> error::Result<flake::FlakeRef> {
    parse_flake_ref(f, cwd, home().as_deref())
}

/// An evaluation error as a value a host can render itself. Positions and
/// the store paths inside the messages name the local file the source was
/// copied from, when one is known (docs/explanation/provenance.md).
pub fn report_of(ev: &Evaluator, e: &Error) -> log::Report {
    let at = |pos: Pos| describe_pos(ev, pos).unwrap_or_else(|| ev.describe_pos(pos));
    ev.lints.flush();
    // a lint message resolves its own paths (lint::Evaluator::lint_shown_path)
    let msg = if e.kind == ErrorKind::Lint {
        e.msg.clone()
    } else {
        rewrite_msg(ev, &e.msg)
    };
    // "iets: " marks what classic mode falls back on; here it only doubles "error: "
    let msg = msg.strip_prefix("iets: ").unwrap_or(&msg);
    let mut r = log::Report::eval(e, msg, &at, &|m| rewrite_msg(ev, m));
    if e.kind == ErrorKind::Lint {
        r.site = ev.lint_site_of_error(e);
    }
    r
}

/// The same report, sent to the session's log, with the exit code.
pub fn report(ev: &Evaluator, e: &Error) -> i32 {
    report_of(ev, e).emit()
}

/// The directory the `iets.toml` walk starts from: the entry file's
/// directory, the flake directory, or cwd for an expression.
pub fn entry_dir(a: &EvalArgs) -> PathBuf {
    let cwd = cwd();
    let bytes = match &a.input {
        Some(Input::File(f)) => eval::dir_of(&abs_path(f, &cwd)).to_vec(),
        Some(Input::Flake(f)) => match flake_ref(f, &cwd) {
            Ok(fr) => return fr.dir,
            Err(_) => cwd,
        },
        _ => cwd,
    };
    PathBuf::from(OsStr::from_bytes(&bytes))
}

/// The project this run's sources belong to: the root `crate::project` finds
/// at or above [`entry_dir`], else that directory. A pure evaluation reads below
/// it. It is part of the tracing-cache key, so a marker that appears above the
/// entry asks for a new entry. A flake gets none: its root tree is already a
/// filtered store copy, and an unfiltered second root would let it read an
/// untracked file by its absolute path (docs/explanation/flakes-and-sources.md).
pub fn project_root(a: &EvalArgs) -> Option<Vec<u8>> {
    if matches!(a.input, Some(Input::Flake(_))) {
        return None;
    }
    let dir = entry_dir(a);
    let root = project::find(&dir).unwrap_or(dir);
    Some(root.into_os_string().into_vec())
}

/// The lint configuration of a run (docs/reference/lints.md).
pub fn lint_config(a: &EvalArgs) -> Result<lint::Config, Box<Error>> {
    let cwd = cwd();
    let roots: Vec<Vec<u8>> = a.lint_roots.iter().map(|r| abs_path(r, &cwd)).collect();
    lint::Config::resolve(Some(&entry_dir(a)), &a.lints, &roots, a.lint_scope)
        .map_err(|m| Error::boxed(ErrorKind::Eval, m, Pos::NONE))
}

/// Evaluates the input, `--arg`s and `-A` included. The settings of `a` are
/// on `ev` already (`EvalArgs::config`).
pub fn evaluate(ev: &mut Evaluator, a: EvalArgs) -> Result<Word, Box<Error>> {
    let cwd = cwd();

    let mut v = match a.input {
        Some(Input::Expr(e)) => ev.eval_string(&e, &cwd)?,
        Some(Input::File(f)) => ev.eval_file(&abs_path(&f, &cwd), Pos::NONE)?,
        Some(Input::Flake(f)) => {
            let mut fr = flake_ref(&f, &cwd).map_err(|m| {
                let kind = match m {
                    error::Error::Unsupported(_) => ErrorKind::Unsupported,
                    _ => ErrorKind::Eval,
                };
                Error::boxed(kind, m.to_string(), Pos::NONE)
            })?;
            fr.lock = a
                .reference_lock_file
                .as_deref()
                .map(|p| abs_pathbuf(p, &cwd));
            if a.flake_whole && fr.attr.is_empty() {
                eval_flake_value(ev, &fr)?
            } else {
                eval_flake(ev, &fr)?
            }
        }
        None => {
            return Err(Error::boxed(
                ErrorKind::Eval,
                "nothing to evaluate: no expression, file or flake",
                Pos::NONE,
            ));
        }
    };
    let args = auto_args(ev, &a.args, &cwd)?;
    match &a.attr {
        Some(attr) => v = installable::find_along_attr_path(ev, v, attr, args)?,
        // nix-instantiate.cc: no attribute path, so the top level is called
        // only when there is something to call it with
        None if !a.args.is_empty() => v = ev.auto_call(v, args)?,
        None => {}
    }
    Ok(v)
}

/// The `--arg`/`--argstr` attrset (`nix-instantiate`'s autoArgs).
pub fn auto_args(
    ev: &mut Evaluator,
    args: &[(String, ArgKind)],
    cwd: &[u8],
) -> Result<Word, Box<Error>> {
    let mut entries = Vec::new();
    // common-eval-args.cc keeps autoArgs in a map and insert_or_assign's into
    // it, so the last `--arg`/`--argstr` of a name is the one that is used
    let mut seen = HashSet::new();
    let mut last: Vec<&(String, ArgKind)> =
        args.iter().rev().filter(|(n, _)| seen.insert(n)).collect();
    last.reverse();
    for (name, kind) in last {
        let w = match kind {
            ArgKind::Expr(e) => {
                let ir = ev.compile_source(b"\xc2\xabarg\xc2\xbb", argv::bytes(e), cwd, None)?;
                let root = ev.root_env;
                ev.arena.mk_thunk(root, ir)
            }
            ArgKind::Str(s) => ev.mk_string(&argv::bytes(s)),
        };
        let sym = ev.sym(name.as_bytes());
        entries.push((sym, Pos::NONE, w));
    }
    ev.mk_attrs(&mut entries, Pos::NONE)
}

pub fn cwd() -> Vec<u8> {
    env::current_dir()
        .map(|p| p.into_os_string().into_encoded_bytes())
        .unwrap_or_else(|_| b"/".to_vec())
}

/// [`abs_path`] as a path.
pub fn abs_pathbuf(f: &str, cwd: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(&abs_path(f, cwd)))
}

/// `f` is in `crate::argv`'s encoding.
pub fn abs_path(f: &str, cwd: &[u8]) -> Vec<u8> {
    let p = argv::bytes(f);
    if p.starts_with(b"/") {
        return p;
    }
    let mut full = cwd.to_vec();
    full.push(b'/');
    full.extend_from_slice(&p);
    compile::canon_path(&full)
}

/// `-I` entries first, then `NIX_PATH`, then the `nix-path` setting of the
/// nix.conf this run loaded, as absolute `(prefix, path)` pairs. Nix's channel
/// defaults are not here.
pub fn full_includes(includes: &[(Vec<u8>, Vec<u8>)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let session = Session::current();
    let (replaces, setting) = session
        .nix_conf
        .get()
        .map_or((false, Vec::new()), |c| c.nix_path());
    let env = env::var("NIX_PATH").ok().filter(|_| !replaces);
    let env = env
        .iter()
        .flat_map(|np| np.split(':'))
        .filter(|s| !s.is_empty());
    let mut all = includes.to_vec();
    all.extend(
        env.chain(setting.iter().map(String::as_str))
            .map(parse_include),
    );
    all
}

/// The derivations to build and the outputs to print: `wanted` plus the
/// outputs the roots' string context named.
pub fn build_targets(
    wanted: &[String],
    roots: &[(StorePath, Option<String>)],
) -> (Vec<StorePath>, Vec<String>) {
    let mut outputs = wanted.to_vec();
    for (_, out) in roots {
        if let Some(o) = out
            && !outputs.contains(o)
        {
            outputs.push(o.clone());
        }
    }
    (roots.iter().map(|(p, _)| p.clone()).collect(), outputs)
}

/// The tracing cache for this run, keyed on the eval arguments. `None` when the
/// cache is off or the command cannot be keyed.
pub fn setup_cache(a: &EvalArgs, subcommand: &str, cfg: &EvalConfig) -> Option<Cache> {
    setup_cache_more(a, subcommand, cfg, &[])
}

/// [`setup_cache`] for a command that takes several installables. `more` is
/// every one after `a.input`: further flake references, or further attribute
/// paths into the one `--expr` or `--file` value.
pub fn setup_cache_more(
    a: &EvalArgs,
    subcommand: &str,
    cfg: &EvalConfig,
    more: &[String],
) -> Option<Cache> {
    if a.no_tracing_cache || a.dump_drvs.is_some() {
        return None;
    }
    let cwd = cwd();
    let mut k = Key::new(subcommand);
    if cfg.is_pure() {
        k.field("pure-eval", b"1");
        if let Some(root) = project_root(a) {
            k.field("project-root", &root);
        }
    }
    // the read set holds the file this names, but not the fact that another
    // run read another file under the same command
    if let Some(p) = &a.reference_lock_file {
        k.field("reference-lock-file", &abs_path(p, &cwd));
    }
    let mut needs_cwd = false;
    match a.input.as_ref()? {
        Input::Expr(e) => {
            k.field("expr", e.as_bytes());
            needs_cwd |= has_relative_path(e);
        }
        // The file and the flake are named by path, never by content: the run
        // reads them through the traced reader, so the read set holds them,
        // and editing one invalidates the entry it belongs to instead of
        // asking a new key. One command keeps one entry, and an edit says
        // what changed.
        Input::File(f) => {
            k.field("file", &abs_path(f, &cwd));
        }
        Input::Flake(f) => {
            let fr = flake_ref(f, &cwd).ok()?;
            k.field("flake", f.as_bytes());
            k.field("flake-dir", fr.dir.as_os_str().as_encoded_bytes());
            needs_cwd = true;
        }
    }
    for m in more {
        if matches!(a.input, Some(Input::Flake(_))) {
            let fr = flake_ref(m, &cwd).ok()?;
            k.field("flake", m.as_bytes());
            k.field("flake-dir", fr.dir.as_os_str().as_encoded_bytes());
        } else {
            k.field("more-attr", m.as_bytes());
        }
    }
    for (name, kind) in &a.args {
        match kind {
            ArgKind::Expr(e) => {
                k.field("arg", name.as_bytes());
                k.field("val", e.as_bytes());
                needs_cwd |= has_relative_path(e);
            }
            ArgKind::Str(s) => {
                k.field("argstr", name.as_bytes());
                k.field("val", s.as_bytes());
            }
        }
    }
    for (prefix, path) in full_includes(&a.includes) {
        k.field("include-prefix", &prefix);
        k.field("include-path", &path);
    }
    k.field("attr", a.attr.as_deref().unwrap_or("").as_bytes());
    k.flag("strict", a.strict);
    k.flag("json", a.json);
    k.flag("raw", a.raw);
    k.field("lints", lint_config(a).ok()?.key().as_bytes());
    for f in &a.xp_features {
        k.field("xp", f.as_bytes());
    }
    k.field("system", cfg.current_system().as_bytes());
    if needs_cwd {
        k.field("cwd", &cwd);
    }
    Cache::open(a.cache_dir.as_deref(), k, a.timings)
}
