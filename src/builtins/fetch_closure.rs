//! `builtins.fetchClosure` (primops/fetchClosure.cc). Evaluation checks what
//! Nix checks from the narinfos alone: every path the store lacks must be
//! content-addressed or signed by a trusted key, and `fromPath` must be
//! addressed the way `inputAddressed` says. It downloads no NAR.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::paths::{coerce_to_path, store_path_string, to_store_path};
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::fetch::http::{self, Limits};
use crate::runtime::{Value, Word};
use crate::source::nix_conf;
use crate::store::narinfo::NarInfo;
use crate::store::path::StorePath;
use crate::trace;

/// How many narinfos are in flight at once.
const JOBS: usize = 8;

pub fn fetch_closure(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let attrs = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.fetchClosure",
    )?;
    let mut from_path = None;
    let mut to_path = false;
    let mut from_store = None;
    let mut input_addressed = false;
    for attr in attrs.iter() {
        let name = String::from_utf8_lossy(ev.interner.name(attr.sym)).into_owned();
        let hint =
            format!("while evaluating the attribute '{name}' passed to builtins.fetchClosure");
        let v = attr.cell.get();
        match name.as_str() {
            "fromPath" => from_path = Some(store_path_arg(ev, v, pos, &hint)?),
            "toPath" => {
                let v = ev.force(v)?;
                if !matches!(v.view(), Value::String(s) if s.bytes.is_empty()) {
                    store_path_arg(ev, v, pos, &hint)?;
                }
                to_path = true;
            }
            "fromStore" => {
                let s = ev.force_string_no_ctx(v, pos, &hint)?;
                from_store = Some(String::from_utf8_lossy(s).into_owned());
            }
            "inputAddressed" => input_addressed = ev.force_bool(v, pos, &hint)?,
            _ => bail!(
                ErrorKind::Eval,
                pos,
                "attribute '{name}' isn't supported in call to 'fetchClosure'"
            ),
        }
    }
    let Some(from_path) = from_path else {
        bail!(
            ErrorKind::Eval,
            pos,
            "attribute 'fromPath' is missing in call to 'fetchClosure'"
        );
    };
    if input_addressed && to_path {
        bail!(
            ErrorKind::Eval,
            pos,
            "attribute 'inputAddressed' is set to true, but 'toPath' is also set. Please remove one of them"
        );
    }
    let Some(from_store) = from_store else {
        bail!(
            ErrorKind::Eval,
            pos,
            "attribute 'fromStore' is missing in call to 'fetchClosure'"
        );
    };
    let in_test = ev.session().env.var("_NIX_IN_TEST").is_some();
    let scheme = from_store.split_once("://").map(|(s, _)| s);
    if !matches!(scheme, Some("http" | "https")) && !(in_test && scheme == Some("file")) {
        bail!(
            ErrorKind::Eval,
            pos,
            "'fetchClosure' only supports http:// and https:// stores"
        );
    }
    if from_store.contains('?') {
        bail!(
            ErrorKind::Eval,
            pos,
            "'fetchClosure' does not support URL query parameters (in '{from_store}')"
        );
    }
    if to_path {
        bail!(
            ErrorKind::Unsupported,
            pos,
            "'fetchClosure' with 'toPath' is not supported yet"
        );
    }

    let closure = walk(ev, from_store.trim_end_matches('/'), &from_path, pos)?;
    let root = &closure[&from_path];
    let shown = from_path.to_full_string();
    match (input_addressed, root.is_content_addressed()) {
        (false, false) => bail!(
            ErrorKind::Eval,
            pos,
            "The 'fromPath' value '{shown}' is input-addressed, but 'inputAddressed' is set to 'false' (default).\n\n\
             If you do intend to fetch an input-addressed store path, add\n\n    inputAddressed = true;\n\n\
             to the 'fetchClosure' arguments.\n\n\
             Note that to ensure authenticity input-addressed store paths, users must configure a trusted binary cache public key on their systems. This is not needed for content-addressed paths."
        ),
        (true, true) => bail!(
            ErrorKind::Eval,
            pos,
            "The store object referred to by 'fromPath' at '{shown}' is not input-addressed, but 'inputAddressed' is set to 'true'.\n\n\
             Remove the 'inputAddressed' attribute (it defaults to 'false') to expect 'fromPath' to be content-addressed"
        ),
        _ => {}
    }
    for p in closure.into_keys() {
        ev.known.add_on_disk(p);
    }
    Ok(store_path_string(ev, &from_path))
}

/// state.coerceToStorePath: a path that is exactly a store path.
fn store_path_arg(ev: &mut Evaluator, v: Word, pos: Pos, hint: &str) -> EvalResult<StorePath> {
    let mut ctx = Vec::new();
    let p = coerce_to_path(ev, v, &mut ctx, pos, hint)?;
    match to_store_path(&p) {
        Some((Ok(sp), [])) => Ok(sp),
        _ => {
            let mut e = Error::new(
                ErrorKind::Eval,
                format!(
                    "path '{}' is not in the Nix store",
                    String::from_utf8_lossy(&p)
                ),
                pos,
            );
            e.add_trace(pos, hint);
            Err(Box::new(e))
        }
    }
}

/// The narinfos of `root`'s closure, read level by level with [`JOBS`]
/// requests at a time. A path missing from disk must be trusted, as
/// local-store.cc:addToStore requires of a copy.
fn walk(
    ev: &mut Evaluator,
    store: &str,
    root: &StorePath,
    pos: Pos,
) -> EvalResult<BTreeMap<StorePath, NarInfo>> {
    let session = ev.session().clone();
    let cfg = nix_conf(&session);
    let fail = |msg: String| Box::new(Error::new(ErrorKind::Eval, msg, pos));
    let limits = Limits::new(cfg).map_err(|e| fail(e.to_string()))?;
    let keys = cfg.trusted_public_keys();
    let require_sigs = cfg
        .bool("require-sigs")
        .map_err(|e| fail(e.to_string()))?
        .unwrap_or(true);
    let mut seen = BTreeSet::from([root.clone()]);
    let mut frontier = vec![root.clone()];
    let mut closure = BTreeMap::new();
    while !frontier.is_empty() {
        let next = AtomicUsize::new(0);
        let got = Mutex::new(Vec::with_capacity(frontier.len()));
        thread::scope(|s| {
            for _ in 0..JOBS.min(frontier.len()) {
                s.spawn(|| {
                    let _entered = session.enter();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(p) = frontier.get(i) else { break };
                        let r = narinfo(store, p, limits);
                        got.lock().unwrap_or_else(|e| e.into_inner()).push(r);
                    }
                });
            }
        });
        frontier.clear();
        for r in got.into_inner().unwrap_or_else(|e| e.into_inner()) {
            let ni = r.map_err(fail)?;
            let present = ev
                .trace()
                .stat_now(ni.path.to_full_string().as_bytes(), true)
                != trace::Kind::Missing;
            if !present && !ni.trusted(&keys, require_sigs) {
                return Err(fail(format!(
                    "cannot add path '{}' because it lacks a signature by a trusted key",
                    ni.path.to_full_string()
                )));
            }
            for r in &ni.references {
                if seen.insert(r.clone()) {
                    frontier.push(r.clone());
                }
            }
            closure.insert(ni.path.clone(), ni);
        }
    }
    Ok(closure)
}

fn narinfo(store: &str, p: &StorePath, limits: Limits) -> Result<NarInfo, String> {
    let url = format!("{store}/{}.narinfo", p.hash_part());
    let mut body = Vec::new();
    http::get(&url, &[], limits, false, &mut body)
        .map_err(|e| format!("cannot fetch '{url}': {e}"))?;
    let ni = NarInfo::parse(&String::from_utf8_lossy(&body), &url).map_err(|e| e.0)?;
    if ni.path != *p {
        return Err(format!(
            "'{url}' describes '{}', not '{}'",
            ni.path.to_full_string(),
            p.to_full_string()
        ));
    }
    Ok(ni)
}
