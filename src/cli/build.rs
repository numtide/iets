//! `iets build`: evaluate to derivations and build them through a daemon.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::api;
use crate::api::build as apibuild;
use crate::api::build::Dispatch;
use crate::api::installable::roots_of;
use crate::api::render::{Outcomes, Report};
use crate::build::fixhash::{self, Answer};
use crate::build::render::Style;
use crate::build::{Outcome, Target};
use crate::cli::help::{arg_error, each_arg};
use crate::cli::render::{self, text};
use crate::cli::{
    self, EvalArgs, Input, Run, build_targets, devenv, evaluate, parse_eval_args, report, run_for,
};
use crate::error::Error as CrateError;
use crate::eval::{self, Evaluator};
use crate::log;
use crate::provenance::{self, Provenance, SourceMap};
use crate::remote::plan::{Inputs, Plan};
use crate::remote::worker::{self, Missing};
use crate::session::Session;
use crate::settings::FixHashes;
use crate::store::path::StorePath;
use crate::trace::run::Cache;
use crate::trace::wire;

struct BuildArgs {
    url: Option<String>,
    timings: bool,
    no_build: bool,
    dry_run: bool,
    json: bool,
    keep_going: bool,
    fetch_to: Option<PathBuf>,
    out_link: Option<String>,
    outputs: Vec<String>,
    sources: HashMap<String, PathBuf>,
    /// What names this run's roots in the roots store ([`keep_outputs`]).
    root_key: Vec<Vec<u8>>,
}

/// `iets build`: evaluate to derivations and ship their closure from memory, or (legacy)
/// take a `.drv` path and read its closure from `/nix/store`. A fixed-output
/// build that fails on its hash gets it fixed per the `fix-hashes` setting.
pub fn cmd_build(argv: &[String]) -> i32 {
    let mut ask = match Session::current().settings().fix_hashes {
        FixHashes::Never => return build_once(argv),
        FixHashes::Ask if !fixhash::interactive() => return build_once(argv),
        FixHashes::Ask => true,
        FixHashes::Always => false,
    };
    let mut wrote: Vec<String> = Vec::new();
    loop {
        let code = build_once(argv);
        let mismatches = log::take_mismatches();
        if code == 0 || mismatches.is_empty() {
            return code;
        }
        let files = local_files(argv);
        let mut fixed = false;
        for (specified, got) in mismatches {
            if wrote.contains(&specified) {
                log::line(&format!(
                    "warning: fix-hashes: the hash changed again, from '{specified}' to '{got}': the output is not reproducible"
                ));
                return code;
            }
            let fix = match fixhash::locate(&files, &specified, &got) {
                Ok(f) => f,
                Err(why) => {
                    log::line(&format!("warning: fix-hashes: {why}"));
                    continue;
                }
            };
            if ask {
                match fixhash::prompt(&fix) {
                    Answer::No => continue,
                    Answer::All => ask = false,
                    Answer::Yes => {}
                }
            }
            if let Err(e) = fix.write() {
                log::line(&format!("warning: fix-hashes: {e}"));
                continue;
            }
            log::fixed(&fix.pos(), fix.at, &fix.old, &fix.new);
            wrote.push(got);
            fixed = true;
        }
        if !fixed {
            return code;
        }
    }
}

fn parse(argv: &[String]) -> (BuildArgs, EvalArgs) {
    let mut b = BuildArgs {
        url: None,
        timings: false,
        no_build: false,
        dry_run: false,
        json: false,
        keep_going: false,
        fetch_to: None,
        out_link: None,
        outputs: Vec::new(),
        sources: HashMap::new(),
        root_key: Vec::new(),
    };
    let mut rest: Vec<String> = Vec::new();
    each_arg(argv, |s, next| match s {
        "--builder" => b.url = Some(next()),
        "--timings" => b.timings = true,
        "--no-build" => b.no_build = true,
        "--dry-run" => b.dry_run = true,
        "--json" => b.json = true,
        "-k" | "--keep-going" => b.keep_going = true,
        "--fetch-to" => b.fetch_to = Some(next().into()),
        "-o" | "--out-link" => b.out_link = Some(next()),
        "--out" => b.outputs.push(next()),
        "--source" => {
            let spec = next();
            let Some((store, local)) = spec.split_once('=') else {
                arg_error(format!("--source wants STOREPATH=LOCAL, got '{spec}'"))
            };
            b.sources.insert(store.to_string(), local.into());
        }
        s => rest.push(s.to_string()),
    });
    let a = parse_eval_args(&rest);
    b.root_key = root_key(&a);
    (b, a)
}

/// What names this run's roots: the tag, the working directory and the
/// installable, so two projects keep two sets of roots and the same command
/// replaces its own in place. A drv path would not: every edit would leave the
/// previous root behind for the TTL to sweep.
fn root_key(a: &EvalArgs) -> Vec<Vec<u8>> {
    let input = match &a.input {
        Some(Input::Flake(f) | Input::File(f) | Input::Expr(f)) => f.as_bytes().to_vec(),
        None => Vec::new(),
    };
    vec![
        b"build".to_vec(),
        crate::cli::cwd(),
        input,
        a.attr.clone().unwrap_or_default().into_bytes(),
    ]
}

/// The local files the evaluation of `argv` parses, from a fresh
/// evaluation: a cache hit does not know them.
fn local_files(argv: &[String]) -> Vec<PathBuf> {
    let (_, a) = parse(argv);
    let a = EvalArgs {
        no_tracing_cache: true,
        ..a
    };
    let Ok(run) = Run::new(&a) else {
        return Vec::new();
    };
    cli::run_parallel(run, move |ev, _| {
        let _ = resolve(ev, a);
        fixhash::local_files(ev)
    })
}

fn build_once(argv: &[String]) -> i32 {
    let (b, a) = parse(argv);
    if let Some(Input::File(f)) = &a.input
        && f.ends_with(".drv")
    {
        return build_drv_file(&b, f);
    }

    let t0 = Instant::now();
    let a = EvalArgs {
        timings: a.timings || b.timings,
        ..a
    };
    let run = run_for(&a).cached(&a, "build");
    if let Some(code) = run.hit(|c| {
        let (roots, drvs, sources) = c.read_build()?;
        c.replay_warnings();
        if b.timings {
            eprintln!(
                "eval: skipped, {} derivations and {} sources from the cache, {:.3}s",
                drvs.len(),
                sources.len(),
                t0.elapsed().as_secs_f64()
            );
        }
        let (root_paths, outputs) = build_targets(&b.outputs, &roots);
        let prov = Provenance::from_map(SourceMap::from_sources(&sources));
        let inputs = Inputs {
            drvs: &drvs,
            sources: &sources,
        };
        Some(build_roots(&b, &root_paths, &outputs, inputs, &prov))
    }) {
        return code;
    }
    // parallel: eval and build run as the main fiber
    cli::run_parallel(run, move |ev, cache| build_eval(ev, &b, a, t0, cache))
}

type Roots = Vec<(StorePath, Option<String>)>;

/// The roots of the installable, each named by the attribute it was reached
/// by: the walk names a tree's members, the installable names the rest.
fn resolve(ev: &mut Evaluator, a: EvalArgs) -> Result<Roots, Box<eval::Error>> {
    let target = a.attr.clone().or_else(|| match &a.input {
        Some(Input::Flake(f)) => f.split_once('#').map(|(_, frag)| frag.to_string()),
        _ => None,
    });
    let target = target.unwrap_or_default();
    let args = a.args.clone();
    let v = evaluate(ev, a)?;
    let args = cli::auto_args(ev, &args, &cli::cwd())?;
    let mut roots = Vec::new();
    roots_of(ev, v, args, &target, &mut roots)?;
    for (p, _) in &roots {
        ev.drv.set_attr(p, &target);
    }
    Ok(roots)
}

fn build_eval(
    ev: &mut Evaluator,
    b: &BuildArgs,
    a: EvalArgs,
    t0: Instant,
    cache: Option<Cache>,
) -> i32 {
    let evaluating = cli::evaluating(&a);
    let roots = match resolve(ev, a) {
        Ok(r) => r,
        Err(e) => return report(ev, &e),
    };
    drop(evaluating);
    let eval_time = t0.elapsed();
    if b.timings {
        eprintln!(
            "eval: {} derivations, {} sources, {:.3}s",
            ev.counts().derivations,
            ev.known.origins().len(),
            eval_time.as_secs_f64()
        );
    }
    if let Some(c) = &cache {
        c.write(ev, &wire::encode_roots(&roots), true);
    }
    let (root_paths, outputs) = build_targets(&b.outputs, &roots);
    let prov = Provenance::snapshot(ev);
    let drvs = ev.drv.drvs.lock();
    let sources = ev.known.origins();
    let inputs = Inputs {
        drvs: &drvs,
        sources: &sources,
    };
    build_roots(b, &root_paths, &outputs, inputs, &prov)
}

/// Legacy mode: `DRV` is a `.drv` store path, or a copy of the file named
/// `<hash>-<name>.drv`; its closure is read from `/nix/store`.
fn build_drv_file(b: &BuildArgs, root_arg: &str) -> i32 {
    let base = Path::new(root_arg)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let root = match StorePath::from_base_name(&base) {
        Ok(p) if p.is_derivation() => p,
        Ok(p) => {
            return CrateError::Store(format!("'{p}' is not a derivation")).report();
        }
        Err(e) => return CrateError::from(e).report(),
    };
    // The root may be a copy outside the store; its inputs are read from `/nix/store`.
    let info = fs::read(root_arg)
        .map_err(CrateError::from)
        .and_then(|text| apibuild::drv_info(&root, text));
    let info = match info {
        Ok(i) => i,
        Err(e) => {
            let msg = format!("cannot read derivation '{root_arg}': {e}");
            return log::Report::new(e.code(), msg).emit();
        }
    };
    let drvs = HashMap::from([(root.clone(), info)]);
    let sources = HashMap::new();
    let prov = Provenance::from_map(SourceMap::from_sources(&sources));
    build_roots(
        b,
        &[root],
        &b.outputs,
        Inputs {
            drvs: &drvs,
            sources: &sources,
        },
        &prov,
    )
}

fn build_roots(
    b: &BuildArgs,
    roots: &[StorePath],
    outputs: &[String],
    inputs: Inputs<'_>,
    prov: &Provenance,
) -> i32 {
    if roots.is_empty() {
        return no_output("nothing to build: the expression has no derivation in it");
    }
    let cfg = apibuild::config();
    let transport = match apibuild::transport(b.url.as_deref(), &cfg) {
        Ok(t) => t,
        Err(e) => return e.report(),
    };
    let t0 = Instant::now();
    let plan = match apibuild::plan_with(roots, inputs.drvs, inputs.sources) {
        Ok(p) => p,
        Err(e) => return e.report(),
    };
    let plan_time = t0.elapsed();
    if b.timings && plan.store_drvs > 0 {
        eprintln!(
            "plan: {} drvs not from this eval, read from /nix/store",
            plan.store_drvs
        );
    }
    let opts = worker::Options {
        // one root is one question, and its first failure answers it; several
        // roots are several questions, and each one gets its verdict
        keep_going: b.keep_going || roots.len() > 1,
        ..Default::default()
    };
    let mut d = match Dispatch::open(&transport, &opts, plan, prov.clone(), Style::detect()) {
        Ok(d) => d.overrides(b.sources.clone()),
        Err(e) => return e.report(),
    };

    if b.no_build {
        if let Err(e) = d.upload_only(&roots[..1]) {
            return e.report();
        }
        timings(b, &d, plan_time);
        return 0;
    }

    let targets: Vec<Target> = roots
        .iter()
        .map(|r| {
            // The bare attribute: the dispatcher adds the position, once.
            let name = (inputs.drvs.get(r))
                .and_then(|i| i.attr.clone())
                .unwrap_or_else(|| provenance::drv_name(&r.to_string()));
            Target::new(&name, r.clone(), outputs.to_vec())
        })
        .collect();
    if b.dry_run {
        return match d.missing(&targets) {
            Ok(m) => print_missing(&m, d.plan(), b.json),
            Err(e) => e.report(),
        };
    }
    let outcomes = match d.submit(targets) {
        Ok(o) => o,
        Err(e) => return e.report(),
    };
    if let Some(dir) = &b.fetch_to {
        for o in &outcomes {
            for path in o.outputs.values() {
                if let Err(e) = d.d.fetch(path, dir) {
                    return e.report();
                }
            }
        }
    }
    if let Some(prefix) = &b.out_link {
        if let Err(e) = d.d.out_links(&out_links(prefix, &outcomes)) {
            return e.report();
        }
    } else if b.fetch_to.is_none() {
        // the caller reads these paths off stdout and uses them afterwards, so a
        // collection in between must not take them. `--fetch-to` already has
        // the bytes outside the store and keeps no root here.
        let outputs: Vec<(String, String)> = outcomes
            .iter()
            .flat_map(|o| {
                o.outputs
                    .iter()
                    .map(|(name, path)| (format!("{}!{name}", o.attr), path.clone()))
            })
            .collect();
        devenv::keep_outputs(&transport, &b.root_key, &outputs);
    }
    timings(b, &d, plan_time);
    let report = Outcomes(&outcomes);
    if outcomes.len() > 1 {
        text::built(&report);
    }
    render::emit(&report, b.json);
    match silence(&outcomes, outputs) {
        Some(why) => log::combine_exit(d.d.exit, no_output(&why)),
        None => d.d.exit,
    }
}

/// `nix build`'s names (`createOutLinks`): `PREFIX`, `PREFIX-1`, ... per
/// root, and `-OUTPUT` after it for an output other than `out`.
fn out_links(prefix: &str, outcomes: &[Outcome]) -> Vec<(String, Option<String>)> {
    let cwd = api::cwd();
    let mut links = Vec::new();
    for (i, o) in outcomes.iter().enumerate() {
        for (name, path) in &o.outputs {
            let mut link = prefix.to_string();
            if i > 0 {
                link.push_str(&format!("-{i}"));
            }
            if name != "out" {
                link.push_str(&format!("-{name}"));
            }
            let link = api::abs_pathbuf(&link, &cwd);
            links.push((path.clone(), Some(link.display().to_string())));
        }
    }
    links
}

/// A run that printed no store path for a target it was asked to build.
/// `iets build` is read by capturing its stdout, so the caller gets a path
/// or a non-zero exit, never silence and success (docs/reference/errors.md).
const NO_OUTPUT: &str = "no-output";

fn no_output(why: &str) -> i32 {
    log::Report::new(NO_OUTPUT, why).emit()
}

/// The stdout contract, checked: why a target put no store path on stdout, or
/// `None` when every target did. A build that failed reported itself already
/// and its exit code carries it, so it is not named again here.
fn silence(outcomes: &[Outcome], wanted: &[String]) -> Option<String> {
    if outcomes.is_empty() {
        return Some("the builder returned no build result".to_string());
    }
    for o in outcomes.iter().filter(|o| o.outputs.is_empty()) {
        if o.skipped {
            let why = o.reason.as_deref().unwrap_or("the builder gave no reason");
            return Some(format!("'{}' was not built: {why}", o.attr));
        }
        if !o.success {
            continue;
        }
        return Some(match wanted.is_empty() {
            true => format!(
                "'{}' built, but the builder has no output path for '{}'",
                o.attr, o.drv
            ),
            false => format!(
                "'{}' built, but the builder has no output {} of '{}'",
                o.attr,
                wanted.join(", "),
                o.drv
            ),
        });
    }
    None
}

/// `--dry-run`: the sets as log records, as shared.cc:printMissing writes
/// them, and with `--json` the contract shape beside them on stdout.
fn print_missing(m: &Missing, plan: &Plan, json: bool) -> i32 {
    let m = api::render::Missing::of(m, plan);
    text::missing(&m);
    if json {
        render::bytes(&render::json(&m.json(), false));
    }
    0
}

/// `--timings`: one line per phase of the flow, as the backend measured it.
fn timings(b: &BuildArgs, d: &Dispatch, plan_time: Duration) {
    if b.timings {
        eprintln!("{}", d.d.report(d.plan(), plan_time));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::Outcome;
    use crate::store::path::StorePath;
    use std::collections::BTreeMap;

    fn outcome(success: bool, skipped: bool, outputs: &[&str]) -> Outcome {
        Outcome {
            attr: "top".into(),
            drv: StorePath::parse("/nix/store/00000000000000000000000000000000-top.drv").unwrap(),
            success,
            skipped,
            reason: skipped.then(|| "the builder returned no build result".to_string()),
            log_tail: Vec::new(),
            outputs: outputs
                .iter()
                .map(|n| ((*n).to_string(), format!("/nix/store/aaa-top-{n}")))
                .collect::<BTreeMap<_, _>>(),
            exit: if success { 0 } else { 1 },
        }
    }

    #[test]
    fn a_path_on_stdout_is_silent() {
        assert_eq!(silence(&[outcome(true, false, &["out"])], &[]), None);
    }

    #[test]
    fn a_failed_build_reports_itself() {
        assert_eq!(silence(&[outcome(false, false, &[])], &[]), None);
    }

    #[test]
    fn no_build_result_is_named() {
        let why = silence(&[outcome(false, true, &[])], &[]).unwrap();
        assert!(why.contains("was not built"), "{why}");
        assert!(why.contains("no build result"), "{why}");
    }

    #[test]
    fn no_outcome_at_all_is_named() {
        assert!(silence(&[], &[]).unwrap().contains("no build result"));
    }

    #[test]
    fn a_success_with_no_output_path_is_named() {
        let why = silence(&[outcome(true, false, &[])], &[]).unwrap();
        assert!(why.contains("no output path"), "{why}");
    }

    #[test]
    fn an_out_flag_that_matches_nothing_is_named() {
        let why = silence(&[outcome(true, false, &[])], &["dev".to_string()]).unwrap();
        assert!(why.contains("no output dev"), "{why}");
    }

    #[test]
    fn out_links_are_named_as_nix_build_names_them() {
        let roots = [
            outcome(true, false, &["out"]),
            outcome(true, false, &["dev", "out"]),
        ];
        let names: Vec<String> = out_links("/r/result", &roots)
            .into_iter()
            .map(|(_, l)| l.unwrap())
            .collect();
        assert_eq!(names, ["/r/result", "/r/result-1-dev", "/r/result-1"]);
    }
}
