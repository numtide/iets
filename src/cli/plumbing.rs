//! `iets plumbing OP`: the machine-first commands. One operation each,
//! JSON on stdout, a stable contract (docs/reference/plumbing.md).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use crate::api::build::Dispatch;
use crate::api::fetch::{drv_hash, url_hash};
use crate::api::installable::{self, Prefixes, Want, drv_roots};
use crate::api::render::{Fetched, Missing, Outcomes, Report};
use crate::api::{self, build as apibuild};
use crate::build::Target;
use crate::build::render::Style;
use crate::cli::help::{self, arg_error, each_arg};
use crate::cli::{self, EvalArgs, Input, evaluate, parse_eval_args, render, report, run_for};
use crate::error::{self, Error as CrateError};
use crate::eval::{Error, Evaluator};
use crate::jobs::cache::Resolver;
use crate::jobs::line::{Job, Line};
use crate::log;
use crate::provenance::Provenance;
use crate::remote::plan::Plan;
use crate::remote::worker::{self, Transport};
use crate::runtime::Word;
use crate::store::drv;
use crate::store::hash::HashAlgo;
use crate::store::path::StorePath;

pub const HELP: &str = "\
usage: iets plumbing OP [FLAGS] ...

The machine-first operations the user-facing commands are written against.
Each one writes JSON on stdout ('iets help reference/plumbing').

operations:
  resolve   an installable becomes derivation roots
  plan      derivation roots become an upload plan
  missing   a plan and a builder become what will be built and fetched
  build     derivation roots are built; outcomes come back
  fetch     a URL or a fixed-output derivation becomes a hash

Every op that takes roots names them as DRV or DRV!OUT[,OUT], or reads a
previous op's stdout with --roots -. They read the closure from /nix/store
unless --drvs DIR, or a \"drvDir\" on stdin, names a resolve --dump-drvs
directory; then the derivations come from there.

usage: iets plumbing resolve [FLAGS] (-E EXPR | FILE | FLAKEREF[#FRAGMENT])

Print {\"attrPaths\", \"roots\"}: the attribute paths that were tried, and
one object per derivation root with \"drv\", \"output\" and \"attr\".

  --prefixes WHICH      the attribute paths a flake fragment is tried at:
                        package (the default), dev-shell, package-set, or
                        none for the fragment itself
  --want first|all      stop at the first path that is there, or take all
  --walk HOW            how a value becomes roots: context (the default, the
                        derivations its string context and drvPath name),
                        get-drvs (get-drvs.cc, recursing with
                        recurseForDerivations), or to-install
                        (outputSpecified, else meta.outputsToInstall)
  --dump-drvs DIR       write the ATerm of every derivation this evaluation
                        made to DIR, and report DIR as \"drvDir\", so an op
                        downstream can plan or build a closure that is not
                        in /nix/store
  --no-drvs             report the attribute paths only

usage: iets plumbing plan [--drvs DIR] [--roots -] DRV...

Print {\"drvs\", \"sources\", \"storeDrvs\"}: the closure to upload, in
dependency order, references first.

usage: iets plumbing missing [FLAGS] [--roots -] DRV...

Print {\"willBuild\", \"willSubstitute\", \"unknown\", \"downloadSize\",
\"narSize\"}: what building the roots on the builder would take.

  --builder URL         where the builds go; the builder setting by default
  --check-cache-status  add \"roots\": one {\"drv\", \"cacheStatus\",
                        \"neededBuilds\", \"neededSubstitutes\"} per root

usage: iets plumbing build [FLAGS] [--roots -] DRV...

Build the roots and print {\"outcomes\"}: one {\"drv\", \"success\",
\"outputs\", \"reason\", \"logTail\"} each. Exit codes: 'iets help errors'.

  --builder URL         where the builds go; the builder setting by default
  --out NAME            an output to build besides the roots' own
  --source STOREPATH=LOCAL   upload STOREPATH from a local tree
  --fetch-to DIR        restore every output into DIR
  --gc-root DIR         register each output under DIR on the local daemon,
                        and report the links as \"gcRoots\"
  --no-build            register the closure and stop

usage: iets plumbing fetch [FLAGS] URL
       iets plumbing fetch [FLAGS] (-E EXPR | FILE | FLAKEREF[#FRAGMENT])

Print {\"hash\", \"url\"} for what URL serves, or {\"hash\", \"drvPath\"} for
the hash a fixed-output derivation produces. Nothing is added to a store.

  --unpack              hash the NAR of the unpacked tarball (fetchzip)
  --hash-type ALGO      md5, sha1, sha256 (default) or sha512
";

pub fn main(argv: &[String]) -> i32 {
    match argv.first().map(String::as_str) {
        Some("resolve") => resolve(&argv[1..]),
        Some("plan") => op_plan(&argv[1..]),
        Some("missing") => op_missing(&argv[1..]),
        Some("build") => op_build(&argv[1..]),
        Some("fetch") => op_fetch(&argv[1..]),
        w => help::unknown_subcommand(w, &["resolve", "plan", "missing", "build", "fetch"]),
    }
}

/// `--walk`: how a value becomes derivation roots.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Walk {
    /// The `drvPath` attribute, or the derivations in a string's context.
    Context,
    /// get-drvs.cc:getDerivations, what `nix-build` and `nix-shell` do.
    GetDrvs,
    /// get-drvs.cc:queryOutputs, what `nix build` installs.
    ToInstall,
}

struct Opts {
    /// `None` is `--prefixes none`: the fragment names the path itself.
    prefixes: Option<Prefixes>,
    want: Want,
    walk: Walk,
    drvs: bool,
}

fn resolve(argv: &[String]) -> i32 {
    let mut o = Opts {
        prefixes: Some(Prefixes::Package),
        want: Want::First,
        walk: Walk::Context,
        drvs: true,
    };
    let mut rest: Vec<String> = Vec::new();
    each_arg(argv, |s, next| match s {
        "--prefixes" => {
            let v = next();
            o.prefixes = match v.as_str() {
                "none" => None,
                _ => Some(Prefixes::parse(&v).unwrap_or_else(|| {
                    arg_error(format!(
                        "--prefixes wants none, package, dev-shell or package-set, got '{v}'"
                    ))
                })),
            };
        }
        "--want" => {
            let v = next();
            o.want = match v.as_str() {
                "first" => Want::First,
                "all" => Want::All,
                _ => arg_error(format!("--want wants first or all, got '{v}'")),
            };
        }
        "--walk" => {
            let v = next();
            o.walk = match v.as_str() {
                "context" => Walk::Context,
                "get-drvs" => Walk::GetDrvs,
                "to-install" => Walk::ToInstall,
                _ => arg_error(format!(
                    "--walk wants context, get-drvs or to-install, got '{v}'"
                )),
            };
        }
        "--no-drvs" => o.drvs = false,
        s => rest.push(s.to_string()),
    });
    let a = parse_eval_args(&rest);
    if a.input.is_none() {
        arg_error("nothing to resolve: give -E EXPR, FILE or FLAKEREF[#FRAGMENT]")
    }
    cli::run_parallel(run_for(&a), move |ev, _| match run(ev, a, o) {
        Ok(()) => 0,
        Err(e) => report(ev, &e),
    })
}

/// One entry of `resolve`'s `roots`. `output` is null when the value named the
/// derivation and not one output.
fn root_json(drv: &StorePath, output: Option<String>, attr: Option<String>) -> serde_json::Value {
    serde_json::json!({ "drv": drv.to_string(), "output": output, "attr": attr })
}

fn run(ev: &mut Evaluator, a: EvalArgs, o: Opts) -> Result<(), Box<Error>> {
    let dump = a.dump_drvs.clone();
    let what = match &a.input {
        Some(Input::Flake(f) | Input::File(f)) => f.clone(),
        _ => String::new(),
    };
    let is_flake = matches!(a.input, Some(Input::Flake(_)));
    let args = api::auto_args(ev, &a.args, &api::cwd())?;
    let flake = match (&a.input, o.prefixes) {
        (Some(Input::Flake(f)), Some(p)) => {
            let (loc, frag) = f.split_once('#').unwrap_or((f.as_str(), ""));
            Some((loc.to_string(), frag.to_string(), p))
        }
        _ => None,
    };
    let attr = a.attr.clone();
    let mut paths: Vec<String> = Vec::new();
    let mut found: Vec<(Option<String>, Word)> = Vec::new();
    match flake {
        None => found.push((attr, evaluate(ev, a)?)),
        Some((loc, frag, p)) => {
            paths = installable::attr_paths(p, &frag, ev.current_system());
            let outputs = evaluate(
                ev,
                EvalArgs {
                    input: Some(Input::Flake(loc.clone())),
                    attr: None,
                    ..a
                },
            )?;
            for f in installable::from_outputs(ev, outputs, &loc, p, &frag, o.want)? {
                found.push((Some(f.path.join(".")), f.value));
            }
        }
    }
    let mut roots = Vec::new();
    let mut seen = HashSet::new();
    for (attr, v) in found {
        if !o.drvs {
            break;
        }
        match o.walk {
            Walk::Context => {
                let mut rs = Vec::new();
                drv_roots(ev, v, &mut rs)?;
                for (drv, output) in rs {
                    roots.push(root_json(&drv, output, attr.clone()));
                }
            }
            Walk::GetDrvs => {
                let mut rs = Vec::new();
                let start = attr.clone().unwrap_or_default();
                installable::get_drvs_at(ev, v, args, &mut rs, &mut seen, &start)?;
                for r in rs {
                    // the walk records the path it reached each derivation by
                    let at = (ev.drv.drvs.get(&r.drv))
                        .and_then(|i| i.attr)
                        .or_else(|| attr.clone());
                    roots.push(root_json(&r.drv, Some(r.output), at));
                }
            }
            Walk::ToInstall => {
                let i = installable::to_install(ev, v, &what, is_flake)?;
                for out in i.outputs {
                    roots.push(root_json(&i.drv, Some(out), attr.clone()));
                }
            }
        }
    }
    let mut v = serde_json::json!({ "attrPaths": paths, "roots": roots });
    if let Some(dir) = dump {
        // an op downstream reads the closure from there instead of /nix/store
        v["drvDir"] = dir.into();
    }
    render::bytes(&render::json(&v, false));
    Ok(())
}

/// What an op works on: the roots, and the directory their derivations were
/// dumped to, if any.
struct Roots {
    targets: Vec<Target>,
    /// `resolve --dump-drvs DIR`: the closure is read from there, not from
    /// `/nix/store`.
    drv_dir: Option<PathBuf>,
}

/// The roots an op works on, one target per derivation. An argument is `DRV`
/// or `DRV!OUT[,OUT]`; `--roots -` reads a previous op's stdout instead, the
/// `roots` of a `resolve` or the `drvs` of a `plan`, and its `drvDir` when it
/// has one. No output name means every output, as `!*` does in the worker
/// protocol.
fn roots(args: &[String], stdin: bool, drvs: Option<PathBuf>) -> Roots {
    let mut drv_dir = drvs;
    let mut out: Vec<Target> = Vec::new();
    let mut add = |drv: &str, output: Option<&str>| {
        let drv = StorePath::parse(drv)
            .unwrap_or_else(|e| arg_error(format!("'{drv}' is not a store path: {}", e.0)));
        let wanted: Vec<String> = (output.into_iter())
            .flat_map(|o| o.split(',').filter(|o| !o.is_empty()).map(String::from))
            .collect();
        match out.iter_mut().find(|t| t.drv == drv) {
            Some(t) => {
                for o in wanted {
                    if !t.outputs.contains(&o) {
                        t.outputs.push(o);
                    }
                }
            }
            None => {
                let name = drv.name().to_string();
                out.push(Target::new(&name, drv, wanted));
            }
        }
    };
    if stdin {
        let mut text = String::new();
        io::Read::read_to_string(&mut io::stdin(), &mut text)
            .unwrap_or_else(|e| arg_error(format!("--roots -: cannot read stdin: {e}")));
        let v: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| arg_error(format!("--roots -: not JSON: {e}")));
        let bad = || arg_error("--roots -: no \"roots\" and no \"drvs\" on stdin");
        if let Some(d) = v.get("drvDir").and_then(serde_json::Value::as_str)
            && drv_dir.is_none()
        {
            drv_dir = Some(d.into());
        }
        if let Some(rs) = v.get("roots").and_then(|r| r.as_array()) {
            for r in rs {
                let Some(drv) = r.get("drv").and_then(serde_json::Value::as_str) else {
                    bad()
                };
                add(drv, r.get("output").and_then(serde_json::Value::as_str));
            }
        } else if let Some(ds) = v.get("drvs").and_then(|d| d.as_array()) {
            for d in ds {
                match d.as_str() {
                    Some(drv) => add(drv, None),
                    None => bad(),
                }
            }
        } else {
            bad()
        }
    }
    for a in args {
        let (drv, output) = match a.split_once('!') {
            Some((d, o)) => (d, Some(o)),
            None => (a.as_str(), None),
        };
        add(drv, output);
    }
    if out.is_empty() {
        arg_error("nothing to do: give DRV... or --roots -")
    }
    Roots {
        targets: out,
        drv_dir,
    }
}

fn paths_of(targets: &[Target]) -> Vec<StorePath> {
    targets.iter().map(|t| t.drv.clone()).collect()
}

/// The plan of the roots: from the derivations `--dump-drvs` wrote when there
/// are any, else from `/nix/store`. Sources come from the local store either
/// way; nothing here evaluated.
fn plan_of(r: &Roots) -> error::Result<Plan> {
    let drvs = match &r.drv_dir {
        Some(d) => apibuild::drvs_in_dir(d)?,
        None => HashMap::new(),
    };
    apibuild::plan_with(&paths_of(&r.targets), &drvs, &HashMap::new())
}

/// `--roots -`, `--drvs DIR` plus the flags `f` accepts; what is left are the
/// root words.
fn split_flags(
    argv: &[String],
    mut f: impl FnMut(&str, &mut dyn FnMut() -> String) -> bool,
) -> (Vec<String>, bool, Option<PathBuf>) {
    let mut rest: Vec<String> = Vec::new();
    let mut stdin = false;
    let mut drvs: Option<PathBuf> = None;
    each_arg(argv, |s, next| {
        if s == "--roots" {
            if next() != "-" {
                arg_error("--roots takes '-': the roots come from stdin")
            }
            stdin = true;
        } else if s == "--drvs" {
            drvs = Some(next().into());
        } else if !f(s, next) {
            if s.starts_with('-') {
                help::unknown_flag(s)
            }
            rest.push(s.to_string());
        }
    });
    (rest, stdin, drvs)
}

fn op_plan(argv: &[String]) -> i32 {
    let (args, stdin, drvs) = split_flags(argv, |_, _| false);
    let r = roots(&args, stdin, drvs);
    let plan = match plan_of(&r) {
        Ok(p) => p,
        Err(e) => return e.report(),
    };
    let drvs: Vec<String> = (plan.drvs.iter())
        .map(|o| o.store_path().to_string())
        .collect();
    let sources: Vec<String> = plan.sources.iter().map(ToString::to_string).collect();
    render::line(
        &serde_json::json!({
            "drvs": drvs, "sources": sources, "storeDrvs": plan.store_drvs,
        })
        .to_string(),
    );
    0
}

fn op_missing(argv: &[String]) -> i32 {
    let mut url: Option<String> = None;
    let mut status = false;
    let (args, stdin, drvs) = split_flags(argv, |s, next| match s {
        "--builder" => {
            url = Some(next());
            true
        }
        "--check-cache-status" => {
            status = true;
            true
        }
        _ => false,
    });
    let r = roots(&args, stdin, drvs);
    match missing(&r, url.as_deref(), status) {
        Ok(v) => {
            render::bytes(&render::json(&v, false));
            0
        }
        Err(e) => e.report(),
    }
}

fn missing(r: &Roots, url: Option<&str>, status: bool) -> error::Result<serde_json::Value> {
    let cfg = apibuild::config();
    let transport = apibuild::transport(url, &cfg)?;
    let opts = worker::Options::default();
    let plan = plan_of(r)?;
    let prov = Provenance::off();
    let mut d = Dispatch::open(&transport, &opts, plan, prov, Style::detect())?;
    let m = d.missing(&r.targets)?;
    let mut v = Missing::of(&m, d.plan()).json();
    if status {
        v["roots"] = serde_json::Value::Array(cache_status(&transport, &r.targets)?);
    }
    Ok(v)
}

/// `--check-cache-status`, per root: the verdict `eval-jobs` reports, from the
/// derivations in `/nix/store`.
fn cache_status(
    transport: &Transport,
    targets: &[Target],
) -> error::Result<Vec<serde_json::Value>> {
    let mut lines: Vec<Line> = Vec::new();
    for t in targets {
        let drv = drv::load_derivation_from_store(&t.drv).map_err(|e| CrateError::Store(e.0))?;
        let mut outputs = BTreeMap::new();
        for (name, o) in &drv.outputs {
            let p = o
                .path(&drv.name, name)
                .map_err(|e| CrateError::Store(e.0))?;
            outputs.insert(name.clone(), p);
        }
        lines.push(Line::Job(Job {
            attr: t.attr.clone(),
            attr_path: Vec::new(),
            drv_path: t.drv.clone(),
            outputs,
            constituents: Vec::new(),
            named_constituents: None,
            glob_constituents: false,
            extra: BTreeMap::new(),
        }));
    }
    Resolver::open(transport)?.resolve(&mut lines)?;
    let raw = |j: &Job, k: &str| {
        (j.extra.get(k))
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    Ok(lines
        .iter()
        .filter_map(|l| match l {
            Line::Job(j) => Some(serde_json::json!({
                "drv": j.drv_path.to_string(),
                "cacheStatus": raw(j, "cacheStatus"),
                "neededBuilds": raw(j, "neededBuilds"),
                "neededSubstitutes": raw(j, "neededSubstitutes"),
            })),
            Line::Error(_) => None,
        })
        .collect())
}

struct BuildOpts {
    url: Option<String>,
    outputs: Vec<String>,
    sources: HashMap<String, PathBuf>,
    fetch_to: Option<PathBuf>,
    gc_root: Option<PathBuf>,
    no_build: bool,
}

fn op_build(argv: &[String]) -> i32 {
    let mut o = BuildOpts {
        url: None,
        outputs: Vec::new(),
        sources: HashMap::new(),
        fetch_to: None,
        gc_root: None,
        no_build: false,
    };
    let (args, stdin, drvs) = split_flags(argv, |s, next| match s {
        "--builder" => {
            o.url = Some(next());
            true
        }
        "--out" => {
            o.outputs.push(next());
            true
        }
        "--fetch-to" => {
            o.fetch_to = Some(next().into());
            true
        }
        "--gc-root" => {
            o.gc_root = Some(next().into());
            true
        }
        "--no-build" => {
            o.no_build = true;
            true
        }
        "--source" => {
            let spec = next();
            let Some((store, local)) = spec.split_once('=') else {
                arg_error(format!("--source wants STOREPATH=LOCAL, got '{spec}'"))
            };
            o.sources.insert(store.to_string(), local.into());
            true
        }
        _ => false,
    });
    let mut r = roots(&args, stdin, drvs);
    for t in &mut r.targets {
        for name in &o.outputs {
            if !t.outputs.contains(name) {
                t.outputs.push(name.clone());
            }
        }
    }
    match build(&o, r) {
        Ok(code) => code,
        Err(e) => e.report(),
    }
}

fn build(o: &BuildOpts, r: Roots) -> error::Result<i32> {
    let cfg = apibuild::config();
    let transport = apibuild::transport(o.url.as_deref(), &cfg)?;
    let opts = worker::Options::default();
    let plan = plan_of(&r)?;
    let prov = Provenance::off();
    let mut d = Dispatch::open(&transport, &opts, plan, prov, Style::detect())?
        .overrides(o.sources.clone());
    if o.no_build {
        d.upload_only(&paths_of(&r.targets))?;
        render::line(&serde_json::json!({ "outcomes": [] }).to_string());
        return Ok(0);
    }
    let outcomes = d.submit(r.targets)?;
    if let Some(dir) = &o.fetch_to {
        for path in outcomes.iter().flat_map(|c| c.outputs.values()) {
            d.d.fetch(path, dir)?;
        }
    }
    let mut roots: Vec<String> = Vec::new();
    if let Some(dir) = &o.gc_root {
        // the links go on the local daemon whatever the builder is, which is
        // what brings the outputs back here (docs/reference/classic.md)
        let links: Vec<(String, Option<String>)> = (outcomes.iter())
            .flat_map(|c| c.outputs.values())
            .map(|p| {
                let link = dir.join(
                    StorePath::parse(p).map_or_else(|_| p.clone(), |sp| sp.base_name().to_string()),
                );
                let link = link.to_string_lossy().into_owned();
                roots.push(link.clone());
                (p.clone(), Some(link))
            })
            .collect();
        d.d.out_links(&links)?;
    }
    let mut v = Outcomes(&outcomes).json();
    if o.gc_root.is_some() {
        v["gcRoots"] = serde_json::json!(roots);
    }
    render::line(&v.to_string());
    Ok(d.d.exit)
}

/// A URL is a positional this op fetches itself; anything else is an
/// installable, which `parse_eval_args` reads.
pub fn is_url(s: &str) -> bool {
    // `file:/abs` is one slash and libcurl reads it, so `file:/` covers all three
    ["https://", "http://", "file:/"]
        .iter()
        .any(|p| s.starts_with(p))
}

/// One line on stdout: the hash and where it came from, as JSON or as the
/// hash alone.
pub fn hash_line(json: bool, hash: &str, key: &'static str, value: &str) -> i32 {
    let f = Fetched {
        hash: hash.to_string(),
        key,
        origin: value.to_string(),
    };
    render::emit(&f, json);
    0
}

/// The hash the one fixed-output derivation an installable names really
/// produces. `iets prefetch` is the other frontend.
pub fn hash_of_drv(ev: &mut Evaluator, a: EvalArgs, json: bool) -> i32 {
    let mut roots = Vec::new();
    if let Err(e) = evaluate(ev, a).and_then(|v| drv_roots(ev, v, &mut roots)) {
        return report(ev, &e);
    }
    let [(root, _)] = roots.as_slice() else {
        let msg = format!("fetch wants one derivation, got {}", roots.len());
        return log::Report::new("usage", msg).emit();
    };
    match drv_hash(ev, root) {
        Ok((got, p)) => hash_line(json, &got, "drvPath", &p.to_full_string()),
        Err(e) => e.report(),
    }
}

/// The flags `fetch` and `iets prefetch` share, and what is left of the argv.
pub fn fetch_flags(argv: &[String]) -> (bool, HashAlgo, Vec<String>) {
    let (mut unpack, mut algo) = (false, HashAlgo::Sha256);
    let mut rest: Vec<String> = Vec::new();
    each_arg(argv, |s, next| match s {
        "--unpack" => unpack = true,
        "--hash-type" => {
            let v = next();
            algo = HashAlgo::parse(&v)
                .unwrap_or_else(|| arg_error(format!("unknown hash algorithm '{v}'")));
        }
        s => rest.push(s.to_string()),
    });
    (unpack, algo, rest)
}

/// A URL or an installable becomes a hash: the whole of both frontends past
/// their own flags.
pub fn fetch(unpack: bool, algo: HashAlgo, rest: Vec<String>, json: bool) -> i32 {
    let Some(at) = rest.iter().position(|s| is_url(s)) else {
        if rest.is_empty() {
            arg_error("nothing to fetch: give a URL, -E EXPR, FILE or FLAKEREF#ATTR")
        }
        if unpack {
            arg_error("--unpack applies to a URL; a derivation says how it is hashed")
        }
        let a = parse_eval_args(&rest);
        return cli::run_parallel(run_for(&a), move |ev, _| hash_of_drv(ev, a, json));
    };
    let mut rest = rest;
    let url = rest.remove(at);
    // what is left are flags only (--log-format, --show-trace, …)
    parse_eval_args(&rest);
    match url_hash(&url, unpack, algo) {
        Ok(h) => hash_line(json, &h.to_sri(), "url", &url),
        Err(e) => e.report(),
    }
}

fn op_fetch(argv: &[String]) -> i32 {
    let (unpack, algo, rest) = fetch_flags(argv);
    fetch(unpack, algo, rest, true)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    /// libcurl reads `file:/abs`, and a lock file may hold that form.
    #[rstest]
    #[case("file:/a/b", true)]
    #[case("file:///a/b", true)]
    #[case("file://localhost/a/b", true)]
    #[case("http://x/y", true)]
    #[case("https://x/y", true)]
    #[case("/a/b", false)]
    #[case("./a", false)]
    #[case("a/b", false)]
    #[case("fil:/a", false)]
    fn a_file_url_needs_one_slash_not_three(#[case] s: &str, #[case] want: bool) {
        assert_eq!(super::is_url(s), want);
    }

    #[test]
    fn a_url_is_the_positional_fetch_takes_itself() {
        assert!(super::is_url("file:///tmp/a.tar.gz"));
        assert!(super::is_url("https://example.org/a.tar.gz"));
        assert!(!super::is_url("github:numtide/treefmt"));
        assert!(!super::is_url("./default.nix"));
    }
}
