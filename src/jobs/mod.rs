//! `iets eval-jobs`: the nix-eval-jobs 2.35 walk over an attribute tree
//! and its JSON lines. The main fiber walks the tree in lexicographic
//! depth-first order and hands every job or error to a sink; helpers run
//! `T_JOB` range tasks that force what a job needs (`type`, `name`,
//! `drvPath`, output paths, `meta`) ahead of the walk. See docs/reference/commands.md.

pub mod cache;
pub mod collector;
pub mod constituents;
pub mod emit;
pub mod instantiate;
pub mod line;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos};
use crate::parallel::force::{CellState, T_JOB, bail_requested, fanout_range, take_cell};
use crate::print::{json_escape, lexicographic_order, print_json};
use crate::runtime::*;
use crate::store::drv::{Derivation, DerivationOutput, load_derivation_from_store};
use crate::store::path::{STORE_DIR, StorePath};
use crate::syntax::symbol::Sym;

pub use line::{ErrorLine, Job, Line};

#[derive(Clone, Copy, Default)]
pub struct Opts {
    pub force_recurse: bool,
    pub meta: bool,
    pub show_input_drvs: bool,
    /// `--no-instantiate`: nix-eval-jobs cannot read the `.drv` back, so
    /// `system` comes from the attribute and `requiredSystemFeatures` and
    /// `inputDrvs` are absent.
    pub no_instantiate: bool,
    /// `--constituents`: read `_hydraAggregate` jobs' `constituents`.
    pub constituents: bool,
    /// `--chunks M --chunk N`: only the units whose path hash is N mod M.
    pub chunks: u32,
    pub chunk: u32,
    /// `--chunk-depth D`: a path of length D is a unit, whatever it holds;
    /// a derivation above that depth is a unit of its own.
    pub chunk_depth: u32,
    /// `--provenance`: add a `pos` key, the source position of each job's
    /// derivation. nix-eval-jobs 2.35 has no such key, so this is off by
    /// default and the output stays flag-identical (docs/explanation/provenance.md).
    pub provenance: bool,
}

#[derive(Clone, Copy)]
struct Syms {
    type_: Sym,
    meta: Sym,
    outputs: Sym,
    recurse: Sym,
    system: Sym,
    hydra_aggregate: Sym,
    constituents: Sym,
    glob_constituents: Sym,
}

#[derive(Clone, Copy)]
struct Config {
    opts: Opts,
    syms: Syms,
    /// The `--arg`/`--argstr` attrset, auto-called on every function met.
    /// A `Word` into the arena, which is never freed.
    auto_args: Word,
    /// The `--apply` function, evaluated once.
    apply: Option<Word>,
}

/// The `eval-jobs` walk of one run. The session owns it
/// (`crate::session`), so two walks in one process do not share it.
#[derive(Default)]
pub struct Run {
    config: OnceLock<Config>,
    /// Path hash and depth of every set the walk fanned out, by address, so
    /// a helper handed a range of its cells can apply the chunk rule.
    sets: Mutex<Option<HashMap<u64, (u64, u32)>>>,
    /// Worker processes spawned so far: `--workers` plus one per restart.
    pub spawned: std::sync::atomic::AtomicUsize,
}

fn remember_set(a: &'static AttrsObj, hash: u64, depth: u32) {
    crate::session::Session::current()
        .jobs
        .sets
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(a as *const AttrsObj as u64, (hash, depth));
}

fn recall_set(a: &'static AttrsObj) -> Option<(u64, u32)> {
    crate::session::Session::current()
        .jobs
        .sets
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(&(a as *const AttrsObj as u64)).copied())
}

/// A copy, not a reference: the callers hold it across a `&mut Evaluator`.
fn config() -> Config {
    *crate::session::Session::current()
        .jobs
        .config
        .get()
        .expect("eval-jobs config")
}

/// Cells per helper task. A leaf costs a whole build graph, so batches
/// are small.
const JOB_BATCH: u32 = 4;
/// Children the main walk fans out ahead of itself in one go; the urgent
/// deque is bounded, so a huge set is fed in windows.
const WINDOW: usize = 1024;

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub jobs: u64,
    pub errors: u64,
}

/// Install the options for every fiber. `auto_args` is the `--arg`/`--argstr`
/// set applied to every function met on the way; `apply` the `--apply`
/// function.
pub fn configure(ev: &mut Evaluator, opts: Opts, auto_args: Word, apply: Option<Word>) {
    let syms = Syms {
        type_: ev.sym(b"type"),
        meta: ev.sym(b"meta"),
        outputs: ev.sym(b"outputs"),
        recurse: ev.sym(b"recurseForDerivations"),
        system: ev.sym(b"system"),
        hydra_aggregate: ev.sym(b"_hydraAggregate"),
        constituents: ev.sym(b"constituents"),
        glob_constituents: ev.sym(b"_hydraGlobConstituents"),
    };
    let _ = crate::session::Session::current().jobs.config.set(Config {
        opts,
        syms,
        auto_args,
        apply,
    });
}

/// Walk `root` and hand every job or error line to `sink`, in
/// lexicographic depth-first order.
pub fn run(ev: &mut Evaluator, root: Word, sink: &mut dyn FnMut(Line)) {
    let mut path = Vec::new();
    walk(ev, root, &mut path, FNV_BASIS, sink);
}

pub enum Node {
    Job(Job),
    Recurse(&'static AttrsObj),
    Skip,
}

fn walk(
    ev: &mut Evaluator,
    w: Word,
    path: &mut Vec<String>,
    hash: u64,
    sink: &mut dyn FnMut(Line),
) {
    let depth = path.len() as u32;
    if depth == config().opts.chunk_depth && !admit(hash, depth, false) {
        return;
    }
    match node(ev, w, path, hash) {
        Ok(Node::Job(job)) => sink(Line::Job(job)),
        Ok(Node::Recurse(attrs)) => {
            remember_set(attrs, hash, depth);
            let ordered = lexicographic_order(ev, attrs);
            let cells = attrs.attrs();
            let index: Vec<usize> = ordered
                .iter()
                .map(|a| (*a as *const Attr as usize - cells.as_ptr() as usize) / size_of::<Attr>())
                .collect();
            for (n, attr) in ordered.iter().enumerate() {
                if n % WINDOW == 0 {
                    // the cell range is in sym order, not walk order, but it
                    // covers the whole window before the walk reaches it
                    let lo = index[n..(n + WINDOW).min(ordered.len())]
                        .iter()
                        .copied()
                        .min()
                        .unwrap_or(0);
                    let hi = index[n..(n + WINDOW).min(ordered.len())]
                        .iter()
                        .copied()
                        .max()
                        .map_or(0, |i| i + 1);
                    fanout_range(ev, T_JOB, Word::from_ref(attrs), lo, hi, JOB_BATCH);
                }
                let name = ev.interner.name_str(attr.sym).to_string();
                let h = fnv_step(hash, name.as_bytes());
                path.push(name);
                walk(ev, attr.cell.get(), path, h, sink);
                path.pop();
            }
        }
        Ok(Node::Skip) => {}
        // An attr that fails above the chunk depth is a unit of its own,
        // like a derivation there, so one chunk reports it.
        Err(_) if !admit(hash, depth, true) => {}
        Err(e) => sink(Line::Error(error_line(ev, path, &e))),
    }
}

/// The error line for `path`, also printed to stderr as nix-eval-jobs does.
pub fn error_line(ev: &Evaluator, path: &[String], e: &Error) -> ErrorLine {
    let r = error_report(ev, e);
    r.emit();
    ErrorLine {
        attr: attr_join(path),
        attr_path: path.to_vec(),
        error: r.text(),
        fatal: e.kind == ErrorKind::StackOverflow,
    }
}

/// worker.cc:attrPathJoin: elements containing a dot are quoted.
pub fn attr_join(path: &[String]) -> String {
    let mut s = String::new();
    for (i, p) in path.iter().enumerate() {
        if i > 0 {
            s.push('.');
        }
        if p.contains('.') {
            s.push('"');
            s.push_str(p);
            s.push('"');
        } else {
            s.push_str(p);
        }
    }
    s
}

/// `api::report`'s report without the provenance rewrite.
pub fn error_report(ev: &Evaluator, e: &Error) -> crate::log::Report {
    crate::log::Report::eval(e, &e.msg, &|p| ev.describe_pos(p), &|m| m.to_string())
}

pub const FNV_BASIS: u64 = 0xcbf29ce484222325;

/// FNV-1a of one more path element (NUL-terminated) after `h`.
pub fn fnv_step(h: u64, name: &[u8]) -> u64 {
    let mut h = h;
    for b in name.iter().copied().chain(std::iter::once(0)) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The chunk rule for a node at `depth` with path hash `hash`. Units are
/// the paths of length `chunk_depth` and the derivations above it; every
/// deeper node belongs to its unit.
pub fn admit(hash: u64, depth: u32, leaf: bool) -> bool {
    let o = config().opts;
    if o.chunks <= 1 || depth > o.chunk_depth || (depth < o.chunk_depth && !leaf) {
        return true;
    }
    hash % o.chunks as u64 == o.chunk as u64
}

/// attr-path.cc: an all-digit component is a list index. Under an attrset it
/// names an attribute instead, where nix-eval-jobs fails: a `listToAttrs`
/// jobset keyed by numbers must build.
fn is_index(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) && name.parse::<u32>().is_ok()
}

/// eval.cc:isDerivation: an attrset whose `type` is the string "derivation".
fn is_derivation(ev: &mut Evaluator, a: &'static AttrsObj) -> EvalResult<bool> {
    let Some(t) = a.get(config().syms.type_) else {
        return Ok(false);
    };
    let v = ev.force_cell(&t.cell)?;
    Ok(v.as_string().is_some_and(|s| s.bytes == b"derivation"))
}

/// worker.cc:processJobRequest for one attribute path: what to print or
/// where to recurse.
pub fn node(ev: &mut Evaluator, w: Word, path: &[String], hash: u64) -> EvalResult<Node> {
    let cfg = config();
    let v = ev.auto_call(w, cfg.auto_args)?;
    let v = ev.force(v)?;
    let Some(attrs) = v.as_attrs() else {
        return Ok(Node::Skip);
    };
    if is_derivation(ev, attrs)? {
        if !admit(hash, path.len() as u32, true) {
            return Ok(Node::Skip);
        }
        return job(ev, attrs, path).map(Node::Job);
    }
    let mut recurse = cfg.opts.force_recurse || path.is_empty();
    if !cfg.opts.force_recurse
        && let Some(r) = attrs.get(cfg.syms.recurse)
    {
        recurse = ev.force_bool(
            r.cell.get(),
            r.pos,
            "while evaluating recurseForDerivations",
        )?;
    }
    Ok(if recurse {
        // the walk and the prefetch address children by index
        Node::Recurse(ev.arena.flat(attrs))
    } else {
        Node::Skip
    })
}

/// attr-path.cc:findAlongAttrPath with nix-eval-jobs's auto-call at every
/// step: the value at `path` under `root`.
pub fn find_along_attr_path(ev: &mut Evaluator, root: Word, path: &[String]) -> EvalResult<Word> {
    let cfg = config();
    let mut v = root;
    for (i, name) in path.iter().enumerate() {
        v = ev.auto_call(v, cfg.auto_args)?;
        v = ev.force(v)?;
        let selection = attr_join(&path[..=i]);
        if is_index(name) && v.as_attrs().is_none() {
            let Some(l) = v.as_list() else {
                return Err(Error::boxed(
                    ErrorKind::Type,
                    format!(
                        "the expression selected by the selection path '{selection}' should be a list but is {}",
                        v.value_type().type_of()
                    ),
                    Pos::NONE,
                ));
            };
            let n: usize = name.parse().unwrap_or(usize::MAX);
            let Some(c) = l.cells().get(n) else {
                return Err(Error::boxed(
                    ErrorKind::Eval,
                    format!("list index {n} in selection path '{selection}' is out of range"),
                    Pos::NONE,
                ));
            };
            v = c.get();
            continue;
        }
        let Some(attrs) = v.as_attrs() else {
            return Err(Error::boxed(
                ErrorKind::Type,
                format!(
                    "the expression selected by the selection path '{selection}' should be a set but is {}",
                    v.value_type().type_of()
                ),
                Pos::NONE,
            ));
        };
        let sym = ev.sym(name.as_bytes());
        let Some(a) = attrs.get(sym) else {
            return Err(Error::boxed(
                ErrorKind::MissingAttr,
                format!("attribute '{name}' in selection path '{selection}' not found"),
                Pos::NONE,
            ));
        };
        v = a.cell.get();
    }
    Ok(v)
}

fn coerce_store_path(ev: &mut Evaluator, w: Word, pos: Pos, what: &str) -> EvalResult<StorePath> {
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(w, &mut ctx, pos, what, false, false, true)?;
    let s = String::from_utf8_lossy(&s).into_owned();
    if !s.starts_with(STORE_DIR) {
        return Err(Error::boxed(
            ErrorKind::Eval,
            format!("path '{s}' is not in the Nix store"),
            pos,
        ));
    }
    StorePath::parse(&s).map_err(|e| Error::boxed(ErrorKind::Eval, e.0, pos))
}

/// get-drvs.cc:queryOutputs. Without `with_paths` the names are read and
/// every path is left unknown, which is how a floating content-addressed
/// output reaches the JSON as a `null`.
fn outputs(
    ev: &mut Evaluator,
    a: &'static AttrsObj,
    with_paths: bool,
) -> EvalResult<BTreeMap<String, Option<StorePath>>> {
    let mut out = BTreeMap::new();
    match a.get(config().syms.outputs) {
        Some(o) => {
            let l = ev.force_list(
                o.cell.get(),
                o.pos,
                "while evaluating the 'outputs' attribute of a derivation",
            )?;
            for c in l.cells() {
                let name = ev.force_string_no_ctx(
                    c.get(),
                    o.pos,
                    "while evaluating the name of an output of a derivation",
                )?;
                let name = String::from_utf8_lossy(name).into_owned();
                if !with_paths {
                    out.insert(name, None);
                    continue;
                }
                let sym = ev.sym(name.as_bytes());
                let Some(v) = a.get(sym) else { continue };
                let va = ev.force_attrs(
                    v.cell.get(),
                    o.pos,
                    "while evaluating an output of a derivation",
                )?;
                let Some(p) = va.get(ev.s.out_path) else {
                    continue;
                };
                let sp = coerce_store_path(
                    ev,
                    p.cell.get(),
                    p.pos,
                    "while evaluating an output path of a derivation",
                )?;
                out.insert(name, Some(sp));
            }
        }
        None if !with_paths => {
            out.insert("out".into(), None);
        }
        None => {
            let Some(p) = a.get(ev.s.out_path) else {
                return Err(Error::boxed(
                    ErrorKind::Eval,
                    "derivation has no 'outPath' attribute",
                    Pos::NONE,
                ));
            };
            let sp = coerce_store_path(
                ev,
                p.cell.get(),
                p.pos,
                "while evaluating the output path of a derivation",
            )?;
            out.insert("out".into(), Some(sp));
        }
    }
    Ok(out)
}

/// get-drvs.cc:PackageInfo::checkMeta: values JSON can carry, no
/// derivations, no functions, no null, no paths.
fn check_meta(ev: &mut Evaluator, w: Word) -> EvalResult<bool> {
    let v = ev.force(w)?;
    Ok(match v.view() {
        Value::List(l) => {
            for c in l.cells() {
                if !check_meta(ev, c.get())? {
                    return Ok(false);
                }
            }
            true
        }
        Value::Attrs(a) => {
            if a.get(ev.s.out_path).is_some() {
                return Ok(false);
            }
            for attr in a.iter() {
                if !check_meta(ev, attr.cell.get())? {
                    return Ok(false);
                }
            }
            true
        }
        Value::Int(_) | Value::Bool(_) | Value::String(_) | Value::Float(_) => true,
        _ => false,
    })
}

/// drv.cc:queryMeta: `null` without usable entries, else an object.
fn meta_json(ev: &mut Evaluator, a: &'static AttrsObj, out: &mut Vec<u8>) -> EvalResult<()> {
    let Some(m) = a.get(config().syms.meta) else {
        out.extend_from_slice(b"null");
        return Ok(());
    };
    let meta = ev.force_attrs(
        m.cell.get(),
        m.pos,
        "while evaluating the 'meta' attribute of a derivation",
    )?;
    let mut first = true;
    for attr in lexicographic_order(ev, meta) {
        if !check_meta(ev, attr.cell.get())? {
            continue;
        }
        if first {
            out.push(b'{');
        } else {
            out.push(b',');
        }
        first = false;
        json_escape(out, ev.interner.name(attr.sym))?;
        out.push(b':');
        let mut ctx = Vec::new();
        print_json(ev, attr.cell.get(), out, &mut ctx, true, attr.pos)?;
    }
    if first {
        out.extend_from_slice(b"null");
    } else {
        out.push(b'}');
    }
    Ok(())
}

/// The `.drv` nix-eval-jobs reads back from the store: from this
/// evaluation's table, else from `/nix/store`.
fn read_drv(ev: &Evaluator, p: &StorePath) -> EvalResult<Derivation> {
    let err = |m: String| Error::boxed(ErrorKind::Eval, m, Pos::NONE);
    let name = Derivation::name_from_path(p).map_err(|e| err(e.0))?;
    match ev.drv.drvs.get(p).and_then(|i| i.aterm) {
        Some(aterm) => Derivation::parse(&aterm, name).map_err(|e| err(e.0)),
        None => load_derivation_from_store(p).map_err(|e| err(e.0)),
    }
}

/// derivation-options.cc:getRequiredSystemFeatures.
fn required_system_features(drv: &Derivation) -> BTreeSet<String> {
    let mut set: BTreeSet<String> = match &drv.structured_attrs {
        Some(json) => serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|v| v.get("requiredSystemFeatures").cloned())
            .and_then(|v| v.as_array().cloned())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        None => drv
            .env_str("requiredSystemFeatures")
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
    };
    let known = drv.outputs.values().all(|o| {
        matches!(
            o,
            DerivationOutput::InputAddressed(_) | DerivationOutput::Fixed { .. }
        )
    });
    if !known {
        set.insert("ca-derivations".into());
    }
    set
}

/// worker.cc:extractConstituents: the `.drv`s in the string context of
/// `constituents`, the job names in it, and `_hydraGlobConstituents`.
fn extract_constituents(
    ev: &mut Evaluator,
    a: &'static AttrsObj,
) -> EvalResult<(Vec<StorePath>, Vec<String>, bool)> {
    let syms = &config().syms;
    let Some(agg) = a.get(syms.hydra_aggregate) else {
        return Ok((Vec::new(), Vec::new(), false));
    };
    if !ev.force_bool(
        agg.cell.get(),
        agg.pos,
        "while evaluating the `_hydraAggregate` attribute",
    )? {
        return Ok((Vec::new(), Vec::new(), false));
    }
    let Some(c) = a.get(syms.constituents) else {
        return Err(Error::boxed(
            ErrorKind::Eval,
            "derivation must have a 'constituents' attribute",
            Pos::NONE,
        ));
    };
    let mut ctx = Vec::new();
    ev.coerce_to_string(
        c.cell.get(),
        &mut ctx,
        c.pos,
        "while evaluating the `constituents` attribute",
        true,
        false,
        true,
    )?;
    ctx.sort();
    ctx.dedup();
    let mut drvs = Vec::new();
    for e in ctx {
        if let Ctx::Built { drv, .. } = e {
            // the `.drv` at the bottom of the chain: what a build starts from
            let (_, root) = crate::runtime::split_built(drv);
            let p = StorePath::parse(&String::from_utf8_lossy(root))
                .map_err(|e| Error::boxed(ErrorKind::Eval, e.0, c.pos))?;
            if drvs.last() != Some(&p) {
                drvs.push(p);
            }
        }
    }
    let l = ev.force_list(
        c.cell.get(),
        c.pos,
        "while evaluating the `constituents` attribute",
    )?;
    let mut named = Vec::new();
    for cell in l.cells() {
        let v = ev.force(cell.get())?;
        if let Some(s) = v.as_string() {
            named.push(String::from_utf8_lossy(s.bytes).into_owned());
        }
    }
    let glob = match a.get(syms.glob_constituents) {
        Some(g) => ev.force_bool(
            g.cell.get(),
            g.pos,
            "while evaluating the `_hydraGlobConstituents` attribute",
        )?,
        None => false,
    };
    Ok((drvs, named, glob))
}

/// worker.cc:applyExprToValue: `--apply` on the job, printed as strict JSON.
fn apply_json(ev: &mut Evaluator, f: Word, a: &'static AttrsObj) -> EvalResult<Vec<u8>> {
    let res = ev.apply(f, &[Word::from_ref(a)], Pos::NONE)?;
    ev.force_attrs(res, Pos::NONE, "apply needs to evaluate to an attrset")?;
    let mut out = Vec::new();
    let mut ctx = Vec::new();
    print_json(ev, res, &mut out, &mut ctx, true, Pos::NONE)?;
    Ok(out)
}

/// drv.cc:Drv::fromPackageInfo, in the order nix-eval-jobs forces things:
/// constituents, `--apply`, name, drvPath, outputs, the `.drv`, meta.
fn job(ev: &mut Evaluator, a: &'static AttrsObj, path: &[String]) -> EvalResult<Job> {
    #[cfg(feature = "stats2")]
    let scope = ev.arena.stats.census.scope_enter(ev.arena.bytes_allocated);
    let r = job_inner(ev, a, path);
    #[cfg(feature = "stats2")]
    if scope {
        let (bytes, escapes) = ev.arena.stats.census.scope_leave(ev.arena.bytes_allocated);
        let j = &mut ev.arena.stats.census.job_scopes;
        j[0] += 1;
        j[1] += bytes;
        if escapes == 0 {
            j[2] += 1;
            j[3] += bytes;
        }
    }
    r
}

fn job_inner(ev: &mut Evaluator, a: &'static AttrsObj, path: &[String]) -> EvalResult<Job> {
    let cfg = config();
    let (constituents, named, glob) = if cfg.opts.constituents {
        extract_constituents(ev, a)?
    } else {
        (Vec::new(), Vec::new(), false)
    };
    let mut extra: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    if let Some(f) = cfg.apply {
        extra.insert("extraValue".into(), apply_json(ev, f, a)?);
    }
    let name = match a.get(ev.s.name) {
        Some(n) => ev
            .force_string_no_ctx(
                n.cell.get(),
                n.pos,
                "while evaluating the 'name' attribute of a derivation",
            )?
            .to_vec(),
        None => {
            return Err(Error::boxed(
                ErrorKind::Eval,
                "derivation name missing",
                Pos::NONE,
            ));
        }
    };
    let drv_path = match a.get(ev.s.drv_path) {
        Some(d) => {
            let p = coerce_store_path(
                ev,
                d.cell.get(),
                d.pos,
                "while evaluating the 'drvPath' attribute of a derivation",
            )?;
            if !p.is_derivation() {
                return Err(Error::boxed(
                    ErrorKind::Eval,
                    format!("store path '{p}' is not a derivation"),
                    d.pos,
                ));
            }
            p
        }
        None => {
            return Err(Error::boxed(
                ErrorKind::Eval,
                "derivation does not contain a 'drvPath' attribute",
                Pos::NONE,
            ));
        }
    };
    // drv.cc:queryOutputs: an output path the evaluation cannot know yet is
    // a `null`, not an error, so a failure to resolve them is retried with
    // the names alone. nix-eval-jobs makes that retry conditional on
    // `ca-derivations`; iets never asks for the feature to evaluate such a
    // derivation, so it does not ask here either.
    let outs = match outputs(ev, a, true) {
        Ok(o) => o,
        Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
        Err(_) => outputs(ev, a, false).map_err(|mut e| {
            e.msg = format!(
                "derivation '{}' does not have valid outputs: {}",
                attr_join(path),
                e.msg
            );
            e
        })?,
    };
    let system = if cfg.opts.no_instantiate {
        // get-drvs.cc:PackageInfo::querySystem.
        match a.get(cfg.syms.system) {
            Some(s) => ev
                .force_string_no_ctx(
                    s.cell.get(),
                    s.pos,
                    "while evaluating the 'system' attribute of a derivation",
                )?
                .to_vec(),
            None => b"unknown".to_vec(),
        }
    } else {
        let drv = read_drv(ev, &drv_path)?;
        if cfg.opts.show_input_drvs {
            let mut v = vec![b'{'];
            for (i, (p, names)) in drv.input_drvs.iter().enumerate() {
                if i > 0 {
                    v.push(b',');
                }
                v.extend(line::json_str(&p.to_string()));
                v.push(b':');
                v.extend(line::json_str_list(names.outputs.iter()));
            }
            v.push(b'}');
            extra.insert("inputDrvs".into(), v);
        }
        extra.insert(
            "requiredSystemFeatures".into(),
            line::json_str_list(required_system_features(&drv).iter()),
        );
        drv.platform.into_bytes()
    };
    if cfg.opts.meta {
        let mut m = Vec::new();
        meta_json(ev, a, &mut m)?;
        extra.insert("meta".into(), m);
    }
    if cfg.opts.provenance
        && let Some(pos) = ev.drv.drvs.get(&drv_path).and_then(|i| i.pos)
        && let Some(s) = crate::provenance::describe_pos(ev, pos)
    {
        extra.insert("pos".into(), line::json_str(&s));
    }
    let mut js = Vec::new();
    json_escape(&mut js, &name)?;
    extra.insert("name".into(), js);
    extra.insert("storeDir".into(), b"\"/nix/store\"".to_vec());
    let mut js = Vec::new();
    json_escape(&mut js, &system)?;
    extra.insert("system".into(), js);
    Ok(Job {
        attr: attr_join(path),
        attr_path: path.to_vec(),
        drv_path,
        outputs: outs,
        constituents,
        named_constituents: Some(named),
        glob_constituents: glob,
        extra,
    })
}

/// A `T_JOB` range task: force what the main walk will ask of each child
/// in `start..end` of `attrs`. Never parks on a cell another fiber holds;
/// an evaluation error in one child does not stop the others.
pub fn prefetch_range(ev: &mut Evaluator, attrs: &'static AttrsObj, start: usize, end: usize) {
    // A set the walk never registered is treated as below the chunk depth.
    let (hash, depth) = recall_set(attrs).unwrap_or((FNV_BASIS, u32::MAX - 1));
    for attr in &attrs.attrs()[start..end] {
        if prefetch_child(ev, attr, hash, depth).is_err() {
            return;
        }
    }
}

/// Force one child of a registered set and what a job needs of it. `Err`
/// only when the fiber must stop.
fn prefetch_child(
    ev: &mut Evaluator,
    attr: &'static Attr,
    hash: u64,
    depth: u32,
) -> Result<(), ()> {
    if bail_requested(ev) || ev.par.cancelled {
        return Err(());
    }
    let cfg = config();
    let h = fnv_step(hash, ev.interner.name(attr.sym));
    let d = depth + 1;
    if d == cfg.opts.chunk_depth && !admit(h, d, false) {
        return Ok(());
    }
    // only the demand path may unbox a cell and take the effects a helper
    // held on its thunk (docs/explanation/evaluation.md); a prefetch task
    // runs on a helper, so it reads through the same call as a walk
    let v = match take_cell(ev, &attr.cell) {
        CellState::Busy => return Ok(()),
        CellState::Ready(v) => v,
        CellState::Unclaimed => match ev.force_cell(&attr.cell) {
            Ok(v) => v,
            Err(e) if e.kind == ErrorKind::Cancelled => return Err(()),
            Err(_) => return Ok(()),
        },
    };
    match prefetch_value(ev, v, h, d) {
        Err(e) if e.kind == ErrorKind::Cancelled => Err(()),
        _ => Ok(()),
    }
}

fn prefetch_value(ev: &mut Evaluator, v: Word, hash: u64, depth: u32) -> EvalResult<()> {
    if depth > 64 {
        return Ok(());
    }
    let cfg = config();
    let v = ev.auto_call(v, cfg.auto_args)?;
    let Some(a) = v.as_attrs() else {
        return Ok(());
    };
    if is_derivation(ev, a)? {
        if admit(hash, depth, true) {
            prefetch_job(ev, a)?;
        }
        return Ok(());
    }
    let mut recurse = cfg.opts.force_recurse;
    if !recurse && let Some(r) = a.get(cfg.syms.recurse) {
        recurse = ev.force_bool(r.cell.get(), r.pos, "")?;
    }
    if !recurse {
        return Ok(());
    }
    let a = ev.arena.flat(a);
    remember_set(a, hash, depth);
    fanout_range(ev, T_JOB, Word::from_ref(a), 0, a.len(), JOB_BATCH);
    for attr in a.attrs() {
        if prefetch_child(ev, attr, hash, depth).is_err() {
            return Err(Error::boxed(ErrorKind::Cancelled, "cancelled", Pos::NONE));
        }
    }
    Ok(())
}

fn prefetch_job(ev: &mut Evaluator, a: &'static AttrsObj) -> EvalResult<()> {
    let cfg = config();
    if let Some(n) = a.get(ev.s.name) {
        ev.force(n.cell.get())?;
    }
    if let Some(d) = a.get(ev.s.drv_path) {
        let mut ctx = Vec::new();
        ev.coerce_to_string(d.cell.get(), &mut ctx, d.pos, "", false, false, true)?;
    }
    outputs(ev, a, true)?;
    if cfg.opts.meta
        && let Some(m) = a.get(cfg.syms.meta)
    {
        let meta = ev.force_attrs(m.cell.get(), m.pos, "")?;
        for attr in meta.iter() {
            if check_meta(ev, attr.cell.get())? {
                ev.force_deep(attr.cell.get())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_numeric_name_selects_an_attribute_under_a_set() {
        let mut ev = Evaluator::new();
        let args = ev.eval_string("{ }", b"/").unwrap();
        configure(&mut ev, Opts::default(), args, None);
        let root = ev
            .eval_string(r#"{ "9" = { a = 1; }; l = [ 7 ]; }"#, b"/")
            .unwrap();
        let mut at = |p: &[&str]| {
            let p: Vec<String> = p.iter().map(|s| s.to_string()).collect();
            let v = find_along_attr_path(&mut ev, root, &p).unwrap();
            assert!(node(&mut ev, v, &p, 0).is_ok());
            ev.force(v).unwrap().as_int()
        };
        assert_eq!(at(&["9", "a"]), Some(1));
        assert_eq!(at(&["9"]), None);
        assert_eq!(at(&["l", "0"]), Some(7));
    }
}
