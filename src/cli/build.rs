//! `iets build`: evaluate to derivations and build them through a daemon.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::slice;
use std::time::{Duration, Instant};

use crate::api;
use crate::api::Loaded;
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
    self, EvalArgs, Input, Run, build_targets, devenv, parse_eval_args, report, run_for,
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
use crate::syntax::symbol::FxHashMap;
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
    b.root_key = root_key(&installables(&a));
    (b, a)
}

/// One `EvalArgs` per installable, sorted and without repeats, so every order
/// of the same set is one job and one cache entry.
fn installables(a: &EvalArgs) -> Vec<EvalArgs> {
    let mut all: Vec<EvalArgs> = Vec::new();
    for input in a.more.iter().cloned().chain(a.input.clone()) {
        let mut one = a.clone();
        one.more = Vec::new();
        one.input = Some(input);
        all.push(one);
    }
    if all.is_empty() {
        all.push(a.clone());
    }
    all.sort_by_cached_key(label);
    all.dedup_by_key(|a| label(a));
    all
}

fn label(a: &EvalArgs) -> Vec<u8> {
    let mut l = match &a.input {
        Some(Input::Flake(f) | Input::File(f) | Input::Expr(f)) => f.as_bytes().to_vec(),
        None => Vec::new(),
    };
    l.push(0);
    l.extend(a.attr.clone().unwrap_or_default().into_bytes());
    l
}

/// What names this run's roots: the tag, the working directory and the
/// installables, so two projects keep two sets of roots and the same command
/// replaces its own in place. A drv path would not: every edit would leave the
/// previous root behind for the TTL to sweep.
fn root_key(all: &[EvalArgs]) -> Vec<Vec<u8>> {
    let mut key = vec![b"build".to_vec(), crate::cli::cwd()];
    key.extend(all.iter().map(label));
    key
}

/// The local files the evaluation of `argv` parses, from a fresh
/// evaluation: a cache hit does not know them.
fn local_files(argv: &[String]) -> Vec<PathBuf> {
    let (_, a) = parse(argv);
    let all: Vec<EvalArgs> = installables(&a)
        .into_iter()
        .map(|a| EvalArgs {
            no_tracing_cache: true,
            ..a
        })
        .collect();
    let Ok(run) = Run::new(&all[0]) else {
        return Vec::new();
    };
    cli::run_parallel(run, move |ev, _| {
        let _ = resolve_all(ev, all);
        fixhash::local_files(ev)
    })
}

fn build_once(argv: &[String]) -> i32 {
    let (b, a) = parse(argv);
    if a.more.is_empty()
        && let Some(Input::File(f)) = &a.input
        && f.ends_with(".drv")
    {
        return build_drv_file(&b, f);
    }
    let mut all = installables(&a);

    let t0 = Instant::now();
    for one in &mut all {
        one.timings |= b.timings;
    }
    let first = &all[0];
    let run = run_for(first);
    // only flakes can be named in the key by their directory
    let run = if all.len() == 1 {
        run.cached(first, "build")
    } else if all.iter().all(|a| matches!(a.input, Some(Input::Flake(_)))) {
        let more: Vec<String> = all[1..]
            .iter()
            .filter_map(|a| match &a.input {
                Some(Input::Flake(f)) => Some(f.clone()),
                _ => None,
            })
            .collect();
        run.cached_more(first, "build", &more)
    } else {
        run
    };
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
    cli::run_parallel(run, move |ev, cache| build_eval(ev, &b, all, t0, cache))
}

type Roots = Vec<(StorePath, Option<String>)>;

/// The roots of every installable, loading each flake once.
fn resolve_all(ev: &mut Evaluator, all: Vec<EvalArgs>) -> Result<Roots, Box<eval::Error>> {
    let mut loaded = Loaded::default();
    let mut roots = Vec::new();
    for a in all {
        let evaluating = cli::evaluating(&a);
        roots.extend(resolve(ev, a, &mut loaded)?);
        drop(evaluating);
    }
    Ok(roots)
}

/// The roots of the installable, each named by the attribute it was reached
/// by: the walk names a tree's members, the installable names the rest.
fn resolve(
    ev: &mut Evaluator,
    a: EvalArgs,
    loaded: &mut Loaded,
) -> Result<Roots, Box<eval::Error>> {
    let target = a.attr.clone().or_else(|| match &a.input {
        Some(Input::Flake(f)) => f.split_once('#').map(|(_, frag)| frag.to_string()),
        _ => None,
    });
    let target = target.unwrap_or_default();
    let args = a.args.clone();
    if a.input.is_none() {
        arg_error("nothing to evaluate: give -E EXPR or FILE")
    }
    let v = api::evaluate_with(ev, a, loaded)?;
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
    all: Vec<EvalArgs>,
    t0: Instant,
    cache: Option<Cache>,
) -> i32 {
    let roots = match resolve_all(ev, all) {
        Ok(r) => r,
        Err(e) => return report(ev, &e),
    };
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
    let drvs = FxHashMap::from_iter([(root.clone(), info)]);
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
        if let Err(e) = d.upload_only(roots) {
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
    // the paths go to stdout as each root is built, so a caller can start on
    // the first while the rest still build
    let stream = !b.json && b.fetch_to.is_none() && b.out_link.is_none();
    let submitted = match stream {
        true => d.submit_each(targets, &mut |o| {
            devenv::keep_outputs(&transport, &b.root_key, &kept(slice::from_ref(o)));
            render::emit(&Outcomes(slice::from_ref(o)), false);
        }),
        false => d.submit(targets),
    };
    let outcomes = match submitted {
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
    } else if let Some(prefix) = &b.out_link {
        if let Err(e) = d.d.out_links(&out_links(prefix, &outcomes)) {
            return e.report();
        }
    } else if !stream {
        devenv::keep_outputs(&transport, &b.root_key, &kept(&outcomes));
    }
    timings(b, &d, plan_time);
    let report = Outcomes(&outcomes);
    if outcomes.len() > 1 {
        text::built(&report);
    }
    if !stream {
        render::emit(&report, b.json);
    }
    match silence(&outcomes, outputs) {
        Some(why) => log::combine_exit(d.d.exit, no_output(&why)),
        None => d.d.exit,
    }
}

/// The outputs a caller reads off stdout and uses afterwards, so a collection
/// in between must not take them. `--fetch-to` already has the bytes outside
/// the store and keeps no root here.
fn kept(outcomes: &[Outcome]) -> Vec<(String, String)> {
    outcomes
        .iter()
        .flat_map(|o| {
            o.outputs
                .iter()
                .map(|(name, path)| (format!("{}!{name}", o.attr), path.clone()))
        })
        .collect()
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
    use crate::sys;
    use rstest::rstest;
    use std::collections::BTreeMap;

    #[test]
    fn installables_are_sorted_without_repeats() {
        let flake = |f: &str| Input::Flake(f.into());
        let a = EvalArgs {
            more: vec![flake(".#b"), flake(".#a"), flake(".#b")],
            input: Some(flake(".#c")),
            ..EvalArgs::new()
        };
        let names: Vec<_> = installables(&a).iter().map(label).collect();
        assert_eq!(names, [b".#a\0", b".#b\0", b".#c\0"]);
    }

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

    /// Why a build printed nothing on stdout, if that needs saying.
    #[rstest]
    #[case::a_path_on_stdout_is_silent(true, &["out"], &[], None)]
    #[case::a_failed_build_reports_itself(false, &[], &[], None)]
    #[case::a_success_with_no_output_path_is_named(true, &[], &[], Some("no output path"))]
    #[case::an_out_flag_that_matches_nothing_is_named(true, &[], &["dev"], Some("no output dev"))]
    fn silence_reason(
        #[case] ok: bool,
        #[case] outputs: &[&str],
        #[case] out_flags: &[&str],
        #[case] want: Option<&str>,
    ) {
        let flags: Vec<String> = out_flags.iter().map(|s| s.to_string()).collect();
        let why = silence(&[outcome(ok, false, outputs)], &flags);
        match (why, want) {
            (None, None) => {}
            (Some(why), Some(want)) => assert!(why.contains(want), "{why}"),
            (why, _) => panic!("got {why:?}, want {want:?}"),
        }
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
    fn out_links_are_named_as_nix_build_names_them() {
        let roots = [
            outcome(true, false, &["out"]),
            outcome(true, false, &["dev", "out"]),
        ];
        let names: Vec<String> = out_links("/r/result", &roots)
            .into_iter()
            .map(|(_, l)| l.unwrap())
            .collect();
        let native = |p: &str| sys::path_from_internal(p.as_bytes()).display().to_string();
        assert_eq!(
            names,
            [
                native("/r/result"),
                native("/r/result-1-dev"),
                native("/r/result-1")
            ]
        );
    }
}
