//! The `iets` command line: argument parsing shared by the subcommands and
//! one module per subcommand. Everything that is not about parsing a command
//! line lives in `crate::api`.

pub mod build;
pub mod cache_dump;
pub mod cache_gc;
pub mod cache_paths;
pub mod classic;
pub mod complete;
pub mod devenv;
pub mod diff;
pub mod docs;
pub mod eval;
pub mod help;
pub mod jobs;
pub mod parse;
pub mod plumbing;
pub mod prefetch;
pub mod render;
pub mod run;
pub mod search;

use help::{arg_error, each_arg};

use crate::api::{self, flake};
use crate::eval::{Error, Evaluator};
use crate::lint::{self, Level, Scope};
use crate::log::{self, Activity};
use crate::parallel::fiber::StackError;
use crate::runtime::Word;
use crate::session::{self, Session};
use crate::{config, error, ignored};

pub use crate::api::run::{Outcome, Run};
pub use crate::api::{
    ArgKind, EvalArgs, Input, abs_path, auto_args, build_targets, cwd, flake_ref, full_includes,
    parse_include, report,
};
use crate::trace::run::Cache;

/// The evaluation input flags every command line shares (common-eval-args.cc).
#[derive(Default)]
pub struct EvalFlags {
    pub includes: Vec<(Vec<u8>, Vec<u8>)>,
    pub args: Vec<(String, ArgKind)>,
    /// `-A` in order: classic commands select each one, the others the last.
    pub attrs: Vec<String>,
    /// `--option NAME VALUE` but `pure-eval`, and the settings a command's
    /// flags alias, in flag order.
    pub overrides: Vec<(String, String)>,
    /// The last of `--pure-eval`, `--no-pure-eval`, `--impure` and `--option
    /// pure-eval`; `None` when none was given (docs/reference/settings.md).
    pub pure: Option<bool>,
}

/// `Ok(true)`: `s` is one of the `EvalFlags` flags or a logging flag of
/// common-args.cc, and `next` gave its values.
/// `Err`: a value is missing or wrong; each family reports that its own way.
pub fn eval_flag(
    f: &mut EvalFlags,
    s: &str,
    mut next: impl FnMut() -> Result<String, String>,
) -> Result<bool, String> {
    match s {
        "-I" | "--include" => f.includes.push(parse_include(&next()?)),
        // `-IP=PATH` is iets's own spelling: Nix's short-flag clustering
        // reads it as `-I -f -o -o …` and refuses it, so classic mode
        // must too (docs/reference/commands.md)
        _ if s.starts_with("-I") && s.len() > 2 && help::command().is_some() => {
            f.includes.push(parse_include(&s[2..]))
        }
        "-A" | "--attr" => f.attrs.push(next()?),
        "--arg" => f.args.push((next()?, ArgKind::Expr(next()?))),
        "--argstr" => f.args.push((next()?, ArgKind::Str(next()?))),
        "--option" => {
            let (name, v) = (next()?, next()?);
            if name == "pure-eval" {
                let b = config::parse_bool(&name, &v).map_err(|e| e.to_string())?;
                f.pure = Some(b);
            } else if name == "json-log-path" {
                log::set_json_log_path(v);
            } else {
                f.overrides.push((name, v));
            }
        }
        "--log-format" => log::set_format(&next()?)?,
        "--show-trace" => log::set_show_trace(),
        "--json-log-path" => log::set_json_log_path(next()?),
        "--pure-eval" => f.pure = Some(true),
        "--no-pure-eval" | "--impure" => f.pure = Some(false),
        // args.cc reads flags in order, so a `--help` a flag before it asked
        // for is that flag's value, not a request for the help
        "-h" | "--help" if help::command().is_some() => help::help(help::command()),
        _ => return Ok(false),
    }
    Ok(true)
}

/// The cache and worker flags every evaluating command shares. `true`: `s`
/// was one of them and `next` gave its value.
pub fn cache_flag(a: &mut EvalArgs, s: &str, mut next: impl FnMut() -> String) -> bool {
    match s {
        "--no-tracing-cache" => a.no_tracing_cache = true,
        "--no-ir-cache" => a.no_ir_cache = true,
        "--no-cache" => {
            a.no_tracing_cache = true;
            a.no_ir_cache = true;
        }
        "--workers" => {
            let v = next();
            let n: usize =
                v.parse().ok().filter(|n| *n >= 1).unwrap_or_else(|| {
                    arg_error(format!("--workers wants a number >= 1, got '{v}'"))
                });
            // the settings path pins this too; here is where a flag would
            // override it (docs/reference/profile.md)
            if n != 1 && Session::current().settings().profile.is_some() {
                arg_error(format!(
                    "a profile needs one worker, so --workers must be 1, not {n}"
                ))
            }
            a.workers = n;
        }
        "--cache-dir" => a.cache_dir = Some(next()),
        _ => return false,
    }
    true
}

pub fn parse_eval_args(argv: &[String]) -> EvalArgs {
    let mut a = EvalArgs::new();
    let mut f = EvalFlags::default();
    each_arg(argv, |s, next| {
        match s {
            _ if eval_flag(&mut f, s, || Ok(next())).unwrap_or_else(|e| arg_error(e)) => {}
            "--strict" => a.strict = true,
            "--json" => a.json = true,
            "--raw" => a.raw = true,
            _ if cache_flag(&mut a, s, &mut *next) => {}
            "--timings" => a.timings = true,
            "-E" | "--expr" => {
                let e = next();
                set_input(&mut a, Input::Expr(e));
            }
            "--dump-drvs" => a.dump_drvs = Some(next()),
            "--reference-lock-file" => a.reference_lock_file = Some(next()),
            "--lint" => a.lints.push(parse_lint_flag(&next())),
            "--lint-root" => a.lint_roots.push(next()),
            "--lint-scope" => a.lint_scope = Some(parse_lint_scope(&next())),
            "--lint-absolute-path-literals"
            | "--lint-short-path-literals"
            | "--lint-url-literals" => {
                let v = next();
                let Some(level) = Level::parse(&v) else {
                    arg_error(format!("invalid lint mode '{v}'"))
                };
                a.lints.push((s[2..].to_string(), level));
            }
            // `eval` and `plumbing` write to no store, so it holds for them
            "--readonly-mode" => {
                if !matches!(help::command(), Some("eval" | "plumbing" | "prefetch")) {
                    ignored::flag(
                        s,
                        "this command writes to the store; 'eval' and 'eval-jobs --no-instantiate' do not",
                    )
                }
            }
            "--extra-experimental-features" | "--experimental-features" => {
                let v = next();
                a.xp_features
                    .extend(v.split_whitespace().map(str::to_owned));
                // as a setting too, so `config::xp_on` sees it
                f.overrides.push((s.trim_start_matches('-').to_string(), v));
            }
            // a synonym for the positional FILE, in the native namespace
            // only: classic mode's argv stays Nix's
            "-f" | "--file" if help::command().is_some() => {
                let f = next();
                set_positional(&mut a, &f)
            }
            _ if s.starts_with('-') && s != "-" => help::unknown_flag(s),
            _ => set_positional(&mut a, s),
        }
    });
    // for its warnings, and for the session's copy that the fetcher reads
    if let Err(e) = config::Config::load_native(&f.overrides) {
        session::exit(e.report())
    }
    ignored::flush();
    (a.includes, a.args, a.pure) = (f.includes, f.args, f.pure);
    a.attrs = f.attrs;
    a.attr = a.attrs.last().cloned();
    a
}

/// The input a positional argument (or `-f`) names: a flake reference when
/// it looks like one, else a file.
fn set_positional(a: &mut EvalArgs, s: &str) {
    if flake::is_flake_ref(s) {
        set_input(a, Input::Flake(s.to_string()))
    } else {
        set_input(a, Input::File(s.to_string()))
    }
}

/// Earlier files and flakes go to `more`. `-E` and FILE exclude each other.
fn set_input(a: &mut EvalArgs, input: Input) {
    if let Some(old) = a.input.take() {
        if matches!(old, Input::Expr(_)) != matches!(input, Input::Expr(_)) {
            arg_error("-E and FILE cannot be combined")
        }
        if !matches!(old, Input::Expr(_)) {
            a.more.push(old);
        }
    }
    a.input = Some(input);
}

/// `--lint SELECTOR=LEVEL`: the selector is checked against the registry
/// now, so a typo is a usage error before anything runs.
pub fn parse_lint_flag(v: &str) -> (String, Level) {
    let mut probe = lint::Config::default();
    if let Err(m) = probe.set_pair(v) {
        arg_error(format!("--lint {v}: {m}"))
    }
    let (name, level) = v.split_once('=').expect("set_pair accepted the pair");
    (
        name.trim().to_string(),
        Level::parse(level.trim()).expect("set_pair accepted the level"),
    )
}

/// `--lint-scope root|all`.
pub fn parse_lint_scope(v: &str) -> Scope {
    Scope::parse(v).unwrap_or_else(|| arg_error(format!("--lint-scope {v}: use root or all")))
}

/// `iets lint list [--lint SELECTOR=LEVEL]... [--lint-root DIR]...
/// [--lint-scope root|all] [PATH]`: the registry with the levels a run
/// from `PATH` (default: cwd) would use.
pub fn cmd_lint(argv: &[String]) -> i32 {
    match argv.first().map(String::as_str) {
        Some("list") => {}
        w => help::unknown_subcommand(w, &["list"]),
    }
    let mut a = EvalArgs::new();
    each_arg(&argv[1..], |s, next| match s {
        "--lint" => a.lints.push(parse_lint_flag(&next())),
        "--lint-root" => a.lint_roots.push(next()),
        "--lint-scope" => a.lint_scope = Some(parse_lint_scope(&next())),
        s if s.starts_with('-') => help::unknown_flag(s),
        s => a.input = Some(Input::File(s.to_string())),
    });
    match api::lint_config(&a) {
        Ok(c) => {
            render::bytes(lint::list(&c).as_bytes());
            0
        }
        Err(e) => error::Error::Eval(e).report(),
    }
}

/// `api::Run::new` for a command line: a lint configuration that does not
/// resolve is a startup failure, reported with its own exit code.
pub fn run_for(a: &EvalArgs) -> Run {
    match Run::new(a) {
        Ok(r) => r,
        Err(code) => session::exit(code),
    }
}

/// `api::Run::go` for a command line: the fiber stacks are a startup
/// failure, so it reports them and exits 1. The library returns the error.
pub fn run_parallel<R: Send + 'static>(
    run: Run,
    f: impl FnOnce(&mut Evaluator, Option<Cache>) -> R + Send + 'static,
) -> R {
    run.go(f).unwrap_or_else(stacks)
}

/// The same for `api::Run::eval`.
pub fn run_reported<T: Send + 'static>(
    run: Run,
    f: impl FnOnce(&mut Evaluator, Option<Cache>) -> error::Result<T> + Send + 'static,
) -> Outcome<T> {
    run.eval(f).unwrap_or_else(stacks)
}

fn stacks<R>(e: StackError) -> R {
    log::line(&format!("iets: {e}"));
    session::exit(1)
}

/// `api::evaluate`, with a missing input reported as a usage error.
pub fn evaluate(ev: &mut Evaluator, a: EvalArgs) -> Result<Word, Box<Error>> {
    need_input(&a);
    api::evaluate(ev, a)
}

/// `api::evaluate_each`, with a missing input reported as a usage error.
pub fn evaluate_each(
    ev: &mut Evaluator,
    a: &EvalArgs,
    each: impl FnMut(&mut Evaluator, Word) -> Result<(), Box<Error>>,
) -> Result<(), Box<Error>> {
    need_input(a);
    api::evaluate_each(ev, a, each)
}

fn need_input(a: &EvalArgs) {
    if a.input.is_none() {
        arg_error("nothing to evaluate: give -E EXPR or FILE")
    }
}

/// Nix has no activity for evaluation, so this is `actUnknown`.
pub fn evaluating(a: &EvalArgs) -> Activity {
    let what = match &a.input {
        Some(Input::Flake(f) | Input::File(f)) => format!("'{f}'"),
        Some(Input::Expr(_)) => "the expression".into(),
        None => String::new(),
    };
    let attr = a
        .attr
        .as_ref()
        .map(|p| format!(" -A {p}"))
        .unwrap_or_default();
    Activity::new(
        log::LVL_TALKATIVE,
        log::ACT_UNKNOWN,
        &format!("evaluating {what}{attr}"),
        vec![],
    )
}
