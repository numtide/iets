//! `derivationStrict` and the derivation path pipeline (primops.cc:
//! derivationStrictInternal): attrs -> `Derivation` -> output paths ->
//! drvPath. Also `placeholder` and `import` of a `.drv` store path.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use super::Registry;
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::print::{json_escape, lexicographic_order, print_json};
use crate::runtime::*;
use crate::store::drv::{
    Derivation, DerivationOutput, DrvError, DrvHashCache, InputNode,
    finalize_input_addressed_outputs, hash_derivation_modulo, load_derivation_from_store,
};
use crate::store::hash::{Hash, HashAlgo, hash_bytes};
use crate::store::path::{
    DRV_EXTENSION, IngestionMethod, StorePath, check_name, hash_placeholder, output_path_name,
};
use crate::syntax::symbol::Sym;

pub fn register(r: &mut Registry) {
    r.op("derivationStrict", 1, derivation_strict);
    r.op("placeholder", 1, placeholder);
    r.op("outputOf", 2, output_of);
}

/// What later derivations need about one we created: its output names and
/// the references of its `.drv` file (for `=drv` context closures). The
/// `hashDerivationModulo` result lives in `DrvState::hash_cache`, so the full
/// `Derivation` is never needed again.
#[derive(Clone, Debug)]
pub struct DrvInfo {
    pub outputs: BTreeSet<String>,
    pub references: BTreeSet<StorePath>,
    /// The ATerm this evaluation produced; `None` when the drv was read from `/nix/store`.
    pub aterm: Option<Arc<Vec<u8>>>,
    /// Where the derivation was defined. `derivation` is a wrapper around
    /// `derivationStrict`, so the primop's call position is inside
    /// `«derivation-internal»` for every normal call; the position of the
    /// `name` attribute's value stands in, which is the same position Nix
    /// names in "whose name attribute is located at". A direct
    /// `builtins.derivationStrict` call keeps its own call position.
    pub pos: Option<Pos>,
    /// The attribute path a cli walk reached this derivation by.
    pub attr: Option<String>,
}

/// Per-evaluator derivation state (derivations.cc:drvHashes plus the
/// derivations this evaluation created).
#[derive(Clone)]
pub struct DrvState {
    pub hash_cache: Arc<std::sync::Mutex<DrvHashCache>>,
    pub drvs: crate::parallel::shared::SharedMap<StorePath, DrvInfo>,
    dump_dir: Option<PathBuf>,
    pub nr_derivations: u64,
    keep_aterm: bool,
    keep_site: bool,
    syms: Option<DrvSyms>,
    imported_drv_fn: Option<Word>,
}

impl DrvState {
    /// Records the attribute path a cli walk reached `p` by. The first walk
    /// to name a drv wins, as `getDerivations` reports the first path.
    pub fn set_attr(&self, p: &StorePath, attr: &str) {
        if attr.is_empty() {
            return;
        }
        let mut g = self.drvs.lock();
        if let Some(i) = g.get_mut(p)
            && i.attr.is_none()
        {
            i.attr = Some(attr.to_string());
        }
    }

    /// The three settings of `eval::EvalConfig`.
    pub(crate) fn new(keep_aterm: bool, keep_site: bool, dump_dir: Option<PathBuf>) -> DrvState {
        DrvState {
            hash_cache: Default::default(),
            drvs: Default::default(),
            dump_dir,
            nr_derivations: 0,
            keep_aterm,
            keep_site,
            syms: None,
            imported_drv_fn: None,
        }
    }
}

#[derive(Clone, Copy)]
struct DrvSyms {
    args: Sym,
    builder: Sym,
    system: Sym,
    output_hash: Sym,
    output_hash_algo: Sym,
    output_hash_mode: Sym,
    outputs: Sym,
    structured_attrs: Sym,
    ignore_nulls: Sym,
    content_addressed: Sym,
    impure: Sym,
    json: Sym,
    pname: Sym,
}

fn syms(ev: &mut Evaluator) -> DrvSyms {
    if let Some(s) = ev.drv.syms {
        return s;
    }
    let s = DrvSyms {
        args: ev.sym(b"args"),
        builder: ev.sym(b"builder"),
        system: ev.sym(b"system"),
        output_hash: ev.sym(b"outputHash"),
        output_hash_algo: ev.sym(b"outputHashAlgo"),
        output_hash_mode: ev.sym(b"outputHashMode"),
        outputs: ev.sym(b"outputs"),
        structured_attrs: ev.sym(b"__structuredAttrs"),
        ignore_nulls: ev.sym(b"__ignoreNulls"),
        content_addressed: ev.sym(b"__contentAddressed"),
        impure: ev.sym(b"__impure"),
        json: ev.sym(b"__json"),
        pname: ev.sym(b"pname"),
    };
    ev.drv.syms = Some(s);
    s
}

fn drv_err(e: DrvError, pos: Pos) -> Box<Error> {
    Error::boxed(ErrorKind::Eval, e.0, pos)
}

fn utf8(bytes: &[u8], pos: Pos) -> EvalResult<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| Error::boxed(ErrorKind::Eval, "string is not valid UTF-8", pos))
}

fn parse_store_path(p: &[u8], pos: Pos) -> EvalResult<StorePath> {
    let s = std::str::from_utf8(p)
        .map_err(|_| Error::boxed(ErrorKind::Eval, "store path is not valid UTF-8", pos))?;
    StorePath::parse(s).map_err(|e| Error::boxed(ErrorKind::Eval, e.0, pos))
}

/// Mirrors primops.cc:prim_derivationStrict.
fn derivation_strict(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let attrs = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.derivationStrict",
    )?;
    crate::parallel::fanout_attrs_coerce(ev, attrs); // parallel
    let Some(name_attr) = attrs.get(ev.s.name) else {
        let mut e = Error::new(
            ErrorKind::MissingAttr,
            "attribute 'name' missing".into(),
            pos,
        );
        e.add_trace(
            pos,
            "in the attrset passed as argument to builtins.derivationStrict",
        );
        return Err(Box::new(e));
    };
    let name_w = name_attr.cell.get();
    let drv_name = ev
        .force_string_no_ctx(
            name_w,
            pos,
            "while evaluating the `name` attribute passed to builtins.derivationStrict",
        )
        .map_err(|mut e| {
            e.add_trace(
                name_attr.pos,
                "while evaluating the derivation attribute 'name'",
            );
            e
        })?;
    let drv_name = utf8(drv_name, pos)?;
    let s = syms(ev);
    derivation_strict_internal(ev, &drv_name, attrs, pos, name_attr.pos, &s).map_err(|mut e| {
        e.add_trace(
            Pos::NONE,
            format!(
                "while evaluating derivation '{drv_name}'\n  whose name attribute is located at {}",
                ev.describe_pos(name_attr.pos)
            ),
        );
        e
    })
}

struct Builder<'a> {
    s: &'a DrvSyms,
    drv: Derivation,
    context: Vec<Ctx>,
    ignore_nulls: bool,
    content_addressed: bool,
    output_hash: Option<String>,
    output_hash_algo: Option<HashAlgo>,
    ingestion_method: Option<IngestionMethod>,
    outputs: BTreeSet<String>,
    /// The structured-attrs JSON object under construction, when
    /// `__structuredAttrs = true`.
    json: Option<Vec<u8>>,
    /// The derivation's own site for `drv-bool-null` and `drv-list`
    /// (`lint::coerce::drv_owner`); `None` keeps both silent.
    lint_owner: Option<Pos>,
}

impl Builder<'_> {
    /// Mirrors the `handleHashMode` lambda.
    fn handle_hash_mode(&mut self, s: &str, pos: Pos) -> EvalResult<()> {
        match IngestionMethod::parse(s) {
            Some(m) => {
                self.ingestion_method = Some(m);
                Ok(())
            }
            None if s == "git" => bail!(
                ErrorKind::Eval,
                pos,
                "outputHashMode '{s}' is not supported (experimental feature)"
            ),
            None => bail!(
                ErrorKind::Eval,
                pos,
                "invalid value '{s}' for 'outputHashMode' attribute"
            ),
        }
    }

    /// Mirrors the `handleOutputs` lambda.
    fn handle_outputs<'b>(
        &mut self,
        names: impl Iterator<Item = &'b str>,
        pos: Pos,
    ) -> EvalResult<()> {
        self.outputs.clear();
        for j in names {
            if self.outputs.contains(j) {
                bail!(ErrorKind::Eval, pos, "duplicate derivation output '{j}'");
            }
            if j == "drvPath" {
                bail!(
                    ErrorKind::Eval,
                    pos,
                    "invalid derivation output name 'drvPath'"
                );
            }
            self.outputs.insert(j.to_string());
        }
        if self.outputs.is_empty() {
            bail!(
                ErrorKind::Eval,
                pos,
                "derivation cannot have an empty set of outputs"
            );
        }
        Ok(())
    }
}

/// One iteration of the attribute loop in derivationStrictInternal. `key`
/// is the attribute name; attributes arrive in lexicographic order, so
/// pushing onto `drv.env` keeps it sorted.
fn process_attr(
    ev: &mut Evaluator,
    b: &mut Builder<'_>,
    sym: Sym,
    key: &str,
    w: Word,
    pos: Pos,
    attr_pos: Pos,
) -> EvalResult<()> {
    let s = *b.s;
    if b.ignore_nulls && ev.force(w)? == Word::NULL {
        return Ok(());
    }
    if sym == s.content_addressed {
        if ev.force_bool(w, pos, "")? {
            b.content_addressed = true;
        }
        return Ok(());
    }
    if sym == s.impure {
        if ev.force_bool(w, pos, "")? {
            bail!(
                ErrorKind::Eval,
                pos,
                "impure derivations are not supported (experimental feature)"
            );
        }
        return Ok(());
    }
    if sym == s.args {
        let l = ev.force_list(w, pos, "")?;
        for c in l.cells() {
            let st = ev.coerce_to_string(
                c.get(),
                &mut b.context,
                attr_pos,
                "while evaluating an element of the argument list",
                true,
                true,
                true,
            )?;
            b.drv.args.push(st.into_owned());
        }
        return Ok(());
    }
    if let Some(mut json) = b.json.take() {
        if sym == s.structured_attrs {
            b.json = Some(json);
            return Ok(());
        }
        if json.len() > 1 {
            json.push(b',');
        }
        json_escape(&mut json, key.as_bytes())?;
        json.push(b':');
        let r = print_json(ev, w, &mut json, &mut b.context, true, pos);
        b.json = Some(json);
        r?;
        if sym == s.builder {
            let st = ev.force_string(w, pos, "")?;
            b.context.extend_from_slice(st.ctx);
            b.drv.builder = utf8(st.bytes, pos)?;
        } else if sym == s.system {
            let st = ev.force_string_no_ctx(w, pos, "")?;
            b.drv.platform = utf8(st, pos)?;
        } else if sym == s.output_hash {
            let st = ev.force_string_no_ctx(w, pos, "")?;
            b.output_hash = Some(utf8(st, pos)?);
        } else if sym == s.output_hash_algo {
            let st = ev.force_string_no_ctx(w, pos, "")?;
            b.output_hash_algo = HashAlgo::parse(&utf8(st, pos)?);
        } else if sym == s.output_hash_mode {
            let st = ev.force_string_no_ctx(w, pos, "")?;
            let st = utf8(st, pos)?;
            b.handle_hash_mode(&st, pos)?;
        } else if sym == s.outputs {
            let l = ev.force_list(w, pos, "")?;
            let mut names = Vec::with_capacity(l.len as usize);
            for c in l.cells() {
                let st = ev.force_string_no_ctx(c.get(), pos, "")?;
                names.push(utf8(st, pos)?);
            }
            b.handle_outputs(names.iter().map(String::as_str), pos)?;
        }
        return Ok(());
    }
    if let Some(owner) = b.lint_owner {
        let forced = ev.force(w)?;
        let site = crate::lint::coerce::drv_attr_pos(ev, attr_pos, owner);
        crate::lint::coerce::drv_value(ev, key, forced, site)?;
    }
    // the attribute's own position, so a coercion lint names the binding
    let raw = ev.coerce_to_string(w, &mut b.context, attr_pos, "", true, true, true)?;
    let is_text = [
        s.json,
        s.builder,
        s.system,
        s.output_hash,
        s.output_hash_algo,
        s.output_hash_mode,
        s.outputs,
    ]
    .contains(&sym);
    if !is_text {
        b.drv.env.push((key.to_string(), raw.into_owned()));
        return Ok(());
    }
    let st = utf8(&raw, pos)?;
    if sym == s.json {
        crate::log::line(&format!(
            "warning: In derivation '{}': setting structured attributes via '__json' is deprecated, and may be disallowed in future versions of Nix. Set '__structuredAttrs = true' instead.",
            b.drv.name
        ));
        let text = ev.arena.alloc_bytes(st.as_bytes());
        b.drv.structured_attrs = Some(super::text::json::canonical_structured_attrs(
            ev, text, pos,
        )?);
        return Ok(());
    }
    if sym == s.builder {
        b.drv.builder = st.clone();
    } else if sym == s.system {
        b.drv.platform = st.clone();
    } else if sym == s.output_hash {
        b.output_hash = Some(st.clone());
    } else if sym == s.output_hash_algo {
        b.output_hash_algo = HashAlgo::parse(&st);
    } else if sym == s.output_hash_mode {
        b.handle_hash_mode(&st, pos)?;
    } else if sym == s.outputs {
        b.handle_outputs(
            st.split([' ', '\t', '\n', '\r']).filter(|t| !t.is_empty()),
            pos,
        )?;
    }
    b.drv.env.push((key.to_string(), st.into_bytes()));
    Ok(())
}

/// Mirrors primops.cc:derivationStrictInternal.
fn derivation_strict_internal(
    ev: &mut Evaluator,
    drv_name: &str,
    attrs: &'static AttrsObj,
    pos: Pos,
    name_pos: Pos,
    s: &DrvSyms,
) -> EvalResult<Word> {
    if let Err(e) = check_name(drv_name) {
        bail!(
            ErrorKind::Eval,
            pos,
            "invalid derivation name: {}. Please pass a different 'name'.",
            e.0
        );
    }
    let json_mode = match attrs.get(s.structured_attrs) {
        Some(a) => ev.force_bool(
            a.cell.get(),
            pos,
            "while evaluating the `__structuredAttrs` attribute passed to builtins.derivationStrict",
        )?,
        None => false,
    };
    let ignore_nulls = match attrs.get(s.ignore_nulls) {
        Some(a) => ev.force_bool(
            a.cell.get(),
            pos,
            "while evaluating the `__ignoreNulls` attribute passed to builtins.derivationStrict",
        )?,
        None => false,
    };

    // `lint::coerce::drv_fallback`, found when the first finding needs it
    let mut lint_fallback: Option<Pos> = None;
    let mut fallback = |ev: &mut Evaluator| {
        *lint_fallback
            .get_or_insert_with(|| crate::lint::coerce::drv_fallback(ev, attrs, pos, name_pos))
    };
    let lint_owner = if ev.lint_on(crate::lint::LintId::DrvBoolNull)
        || ev.lint_on(crate::lint::LintId::DrvList)
    {
        let builder_pos = attrs.get(s.builder).map_or(Pos::NONE, |a| a.pos);
        crate::lint::coerce::drv_owner(ev, pos, builder_pos)
    } else {
        None
    };
    let mut b = Builder {
        s,
        drv: Derivation {
            name: drv_name.to_string(),
            ..Default::default()
        },
        context: Vec::new(),
        ignore_nulls,
        content_addressed: false,
        output_hash: None,
        output_hash_algo: None,
        ingestion_method: None,
        outputs: ["out".to_string()].into_iter().collect(),
        json: json_mode.then(|| vec![b'{']),
        lint_owner,
    };

    for attr in lexicographic_order(ev, attrs) {
        if attr.sym == s.ignore_nulls {
            continue;
        }
        let key = utf8(ev.interner.name(attr.sym), pos)?;
        let before = b.context.len();
        process_attr(ev, &mut b, attr.sym, &key, attr.cell.get(), pos, attr.pos).map_err(
            |mut e| {
                e.add_trace(
                    attr.pos,
                    format!("while evaluating attribute '{key}' of derivation '{drv_name}'"),
                );
                e
            },
        )?;
        let ctx = &b.context[before..];
        if ctx.iter().any(|c| !c.is_store()) {
            let fallback = fallback(ev);
            let site = crate::lint::coerce::drv_attr_pos(ev, attr.pos, fallback);
            crate::lint::purity::marks_in_drv(ev, drv_name, &key, ctx, site, attr.pos)?;
        }
    }

    let Builder {
        mut drv,
        mut context,
        content_addressed,
        output_hash,
        output_hash_algo,
        ingestion_method,
        outputs,
        json,
        ..
    } = b;

    if let Some(mut json) = json {
        json.push(b'}');
        drv.structured_attrs = Some(utf8(&json, pos)?);
    }

    context.sort();
    context.dedup();
    for c in &context {
        match c {
            Ctx::Impure(_) => {}
            Ctx::Opaque(p) => {
                drv.input_srcs.insert(parse_store_path(p, pos)?);
            }
            Ctx::Built { drv: d, output } => {
                // derived-path-map.hh:DerivedPathMap::ensureSlot: one child per
                // layer, from the `.drv` at the bottom outwards, and the output
                // is taken of the node the last layer reaches
                let (layers, root) = split_built(d);
                let mut node = drv
                    .input_drvs
                    .entry(parse_store_path(root, pos)?)
                    .or_default();
                for name in layers.iter().rev() {
                    node = node.children.entry(utf8(name, pos)?).or_default();
                }
                node.outputs.insert(utf8(output, pos)?);
            }
            Ctx::DrvDeep(d) => {
                let d = parse_store_path(d, pos)?;
                for (j, outputs) in drv_closure(ev, &d, pos)? {
                    drv.input_srcs.insert(j.clone());
                    if let Some(outputs) = outputs {
                        drv.input_drvs.insert(j, InputNode::of(outputs));
                    }
                }
            }
        }
    }

    if drv.builder.is_empty() {
        bail!(ErrorKind::Eval, pos, "required attribute 'builder' missing");
    }
    if drv.platform.is_empty() {
        bail!(ErrorKind::Eval, pos, "required attribute 'system' missing");
    }
    // a text-addressed single `out` is a derivation that produces a `.drv`
    if drv_name.ends_with(DRV_EXTENSION)
        && !(ingestion_method == Some(IngestionMethod::Text)
            && outputs.len() == 1
            && outputs.contains("out"))
    {
        bail!(
            ErrorKind::Eval,
            pos,
            "derivation names are allowed to end in '{DRV_EXTENSION}' only if they produce a single derivation file"
        );
    }

    let trace = ev.trace().clone();
    let mut loader = move |p: &StorePath| load_drv_traced(&trace, p);
    if let Some(h) = output_hash {
        if outputs.len() != 1 || !outputs.contains("out") {
            bail!(
                ErrorKind::Eval,
                pos,
                "multiple outputs are not supported in fixed-output derivations"
            );
        }
        let hash = if h.is_empty() {
            let Some(algo) = output_hash_algo else {
                bail!(
                    ErrorKind::Eval,
                    pos,
                    "empty hash requires explicit hash algorithm"
                );
            };
            let zero = Hash::from_bytes(algo, &vec![0u8; algo.size()])
                .map_err(|e| Error::boxed(ErrorKind::Eval, e.0, pos))?;
            crate::log::line(&format!(
                "warning: found empty hash, assuming '{}'",
                zero.to_sri()
            ));
            zero
        } else {
            Hash::parse_any(&h, output_hash_algo)
                .map_err(|e| Error::boxed(ErrorKind::Eval, e.0, pos))?
        };
        let method = ingestion_method.unwrap_or(IngestionMethod::Flat);
        if ev.lint_on(crate::lint::LintId::Fod) {
            let sri = hash.to_sri();
            let algo = sri.split('-').next().unwrap_or("");
            let mode = match method {
                IngestionMethod::Flat => "flat",
                IngestionMethod::Recursive => "recursive",
                IngestionMethod::Text => "text",
            };
            let fallback = fallback(ev);
            let site = crate::lint::coerce::drv_attr_pos(ev, name_pos, fallback);
            crate::lint::purity::fod(ev, drv_name, mode, algo, site)?;
        }
        let out = DerivationOutput::Fixed { method, hash };
        let path = out
            .fixed_path(drv_name, "out")
            .map_err(|e| drv_err(e, pos))?
            .expect("fixed output has a path");
        drv.env_set("out", path.to_string().into_bytes());
        drv.outputs.insert("out".into(), out);
    } else if content_addressed {
        let algo = output_hash_algo.unwrap_or(HashAlgo::Sha256);
        let method = ingestion_method.unwrap_or(IngestionMethod::Recursive);
        for o in &outputs {
            drv.env_set(o, hash_placeholder(o).into_bytes());
            drv.outputs
                .insert(o.clone(), DerivationOutput::CaFloating { method, algo });
        }
    } else {
        for o in &outputs {
            drv.env_set(o, Vec::new());
            drv.outputs.insert(o.clone(), DerivationOutput::Deferred);
        }
        finalize_input_addressed_outputs(
            &mut drv,
            &mut ev.drv.hash_cache.lock().unwrap(),
            &mut loader,
        )
        .map_err(|e| drv_err(e, pos))?;
    }

    // A text-addressed output is a `.drv`, so `builtins.outputOf` may read
    // this derivation back. Nix reads it from the store, which it has written
    // by then; iets writes nothing during an evaluation, so the ATerm of a
    // producer is kept whatever the flags say. There are as many of these as
    // there are dynamic derivations, which is none in nixpkgs.
    let produces_drv = ingestion_method == Some(IngestionMethod::Text);
    // the ATerm is only kept when something asks for it; the path it is
    // hashed into is all a plain evaluation wants
    let keep_text = ev.drv.keep_aterm || ev.drv.dump_dir.is_some() || produces_drv;
    let (drv_path, text) = drv
        .with_aterm(|t| {
            (
                drv.drv_path_for(t),
                if keep_text { t.to_vec() } else { Vec::new() },
            )
        })
        .map_err(|e| drv_err(e, pos))?;
    let drv_path = drv_path.map_err(|e| drv_err(e, pos))?;
    let h = hash_derivation_modulo(
        &drv,
        false,
        &mut ev.drv.hash_cache.lock().unwrap(),
        &mut loader,
    )
    .map_err(|e| drv_err(e, pos))?;
    ev.drv
        .hash_cache
        .lock()
        .unwrap()
        .insert(drv_path.clone(), h);
    ev.drv.nr_derivations += 1;

    // not held back like a warning: the value this derivation is part of
    // escapes into its cell whether or not the demand path takes the thunk
    // the helper was forcing, so a dump parked on that thunk is dropped
    // while the drvPath that names it is returned
    if let Some(dir) = &ev.drv.dump_dir {
        let file = dir.join(drv_path.base_name());
        std::fs::write(&file, &text).map_err(|e| {
            Error::boxed(
                ErrorKind::Io,
                format!("writing '{}': {e}", file.display()),
                pos,
            )
        })?;
    }

    let mut references: BTreeSet<StorePath> = drv.input_srcs.clone();
    references.extend(drv.input_drvs.keys().cloned());
    // stdenv.mkDerivation computes `name` in make-derivation.nix but passes
    // the package's own `pname` through, as nixpkgs' meta.position does. A
    // name-only builder (runCommand) loses the caller's `name` binding the
    // same way, so the first attribute still bound in a root tree is next.
    let pname_pos = attrs.get(s.pname).map(|a| a.pos);
    let site = if ev.drv.keep_site {
        crate::provenance::real_pos(ev, pos)
            .or_else(|| pname_pos.and_then(|p| crate::provenance::real_pos(ev, p)))
            .or_else(|| {
                let p = crate::lint::coerce::drv_fallback(ev, attrs, pos, name_pos);
                crate::provenance::real_pos(ev, p)
            })
            .or_else(|| crate::provenance::real_pos(ev, name_pos))
    } else {
        None
    };
    ev.drv.drvs.insert(
        drv_path.clone(),
        DrvInfo {
            outputs: drv.outputs.keys().cloned().collect(),
            references,
            aterm: (ev.drv.keep_aterm || produces_drv).then(|| Arc::new(text)),
            pos: site,
            attr: None,
        },
    );

    let drv_path_s = drv_path.to_string();
    let drv_path_bytes = ev.arena.alloc_bytes(drv_path_s.as_bytes());
    let mut entries = Vec::with_capacity(1 + drv.outputs.len());
    let dp = ev
        .arena
        .mk_string_ctx(drv_path_bytes, &[Ctx::DrvDeep(drv_path_bytes)]);
    entries.push((ev.s.drv_path, Pos::NONE, dp));
    for (name, out) in &drv.outputs {
        entries.push((
            ev.sym(name.as_bytes()),
            Pos::NONE,
            output_string(ev, &drv_path, drv_path_bytes, drv_name, name, out, pos)?,
        ));
    }
    ev.mk_attrs(&mut entries, pos)
}

/// Mirrors eval.cc:EvalState::mkOutputString: the output path, or a
/// downstream placeholder when it is not statically known, with `!out!drv`
/// context.
fn output_string(
    ev: &mut Evaluator,
    drv_path: &StorePath,
    drv_path_bytes: &'static [u8],
    drv_name: &str,
    output: &str,
    out: &DerivationOutput,
    pos: Pos,
) -> EvalResult<Word> {
    let s = match out.path(drv_name, output).map_err(|e| drv_err(e, pos))? {
        Some(p) => p.to_string(),
        None => downstream_placeholder(drv_path, drv_name, output),
    };
    let output = ev.arena.alloc_bytes(output.as_bytes());
    let bytes = ev.arena.alloc_bytes(s.as_bytes());
    Ok(ev.arena.mk_string_ctx(
        bytes,
        &[Ctx::Built {
            drv: drv_path_bytes,
            output,
        }],
    ))
}

/// Mirrors downstream-placeholder.cc:DownstreamPlaceholder::unknownCaOutput.
fn unknown_ca_output(drv_path: &StorePath, drv_name: &str, output: &str) -> Hash {
    let clear = format!(
        "nix-upstream-output:{}:{}",
        drv_path.hash_part(),
        output_path_name(drv_name, output)
    );
    hash_bytes(HashAlgo::Sha256, clear.as_bytes())
}

/// Mirrors downstream-placeholder.cc:DownstreamPlaceholder::unknownDerivation:
/// an output of a derivation that is itself the output of one.
fn unknown_derivation(parent: &Hash, output: &str) -> Hash {
    let clear = format!(
        "nix-computed-output:{}:{}",
        crate::store::hash::compress_hash(parent, 20).to_nix32(),
        output
    );
    hash_bytes(HashAlgo::Sha256, clear.as_bytes())
}

/// Mirrors downstream-placeholder.cc:DownstreamPlaceholder::render.
fn render_placeholder(h: &Hash) -> String {
    format!("/{}", h.to_nix32())
}

fn downstream_placeholder(drv_path: &StorePath, drv_name: &str, output: &str) -> String {
    render_placeholder(&unknown_ca_output(drv_path, drv_name, output))
}

/// Mirrors eval.cc:mkSingleDerivedPathStringRaw of a `SingleDerivedPath::Built`,
/// spelled as this evaluation holds it: the `.drv` at the bottom, the output
/// names of the layers above it outermost first, and the output being taken.
/// The path when the run can know it, the downstream placeholder otherwise.
fn render_built(
    ev: &mut Evaluator,
    root: &StorePath,
    layers: &[&[u8]],
    output: &str,
    pos: Pos,
) -> EvalResult<String> {
    let drv_name = Derivation::name_from_path(root).map_err(|e| drv_err(e, pos))?;
    let Some((innermost, above)) = layers.split_last() else {
        // the derivation is a real `.drv`, so the output may have a path
        let drv = load_drv(ev, root).map_err(|e| drv_err(e, pos))?;
        let Some(out) = drv.outputs.get(output) else {
            return Err(Error::boxed(
                ErrorKind::Eval,
                format!("derivation '{root}' does not have output '{output}'"),
                pos,
            ));
        };
        return Ok(
            match out.path(drv_name, output).map_err(|e| drv_err(e, pos))? {
                Some(p) => p.to_string(),
                None => downstream_placeholder(root, drv_name, output),
            },
        );
    };
    // downstream-placeholder.cc:fromSingleDerivedPathBuilt, from the bottom up
    let mut h = unknown_ca_output(root, drv_name, &utf8(innermost, pos)?);
    for name in above.iter().rev() {
        h = unknown_derivation(&h, &utf8(name, pos)?);
    }
    Ok(render_placeholder(&unknown_derivation(&h, output)))
}

/// primops.cc:prim_outputOf. The first argument is a string whose one context
/// element names a derivation; the second an output of it. The result names
/// that output, and carries it as context, so a derivation that uses the
/// string depends on it.
fn output_of(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    const CTX: &str = "while evaluating the first argument to builtins.outputOf";
    let s = ev.force_string(args[0], pos, CTX)?;
    let name = ev.force_string_no_ctx(
        args[1],
        pos,
        "while evaluating the second argument to builtins.outputOf",
    )?;
    let name = utf8(name, pos)?;
    // eval.cc:coerceToSingleDerivedPathUnchecked. `Impure` is iets's own mark
    // and is no part of what Nix counts.
    let elems: Vec<&Ctx> = (s.ctx.iter())
        .filter(|c| !matches!(c, Ctx::Impure(_)))
        .collect();
    let text = String::from_utf8_lossy(s.bytes).into_owned();
    let [elem] = elems[..] else {
        return Err(Error::boxed(
            ErrorKind::Eval,
            format!(
                "string '{text}' has {} entries in its context. It should only have exactly one entry",
                elems.len()
            ),
            pos,
        ));
    };
    // the derivation the result's output comes from, as a `Ctx::Built.drv`
    let (drv, expected) = match elem {
        Ctx::Opaque(p) => (*p, String::from_utf8_lossy(p).into_owned()),
        Ctx::DrvDeep(_) => {
            return Err(Error::boxed(
                ErrorKind::Eval,
                format!(
                    "string '{text}' has a context which refers to a complete source and binary closure. This is not supported at this time"
                ),
                pos,
            ));
        }
        Ctx::Built { drv, output } => {
            let (layers, root) = split_built(drv);
            let root = parse_store_path(root, pos)?;
            let out = utf8(output, pos)?;
            let expected = render_built(ev, &root, &layers, &out, pos)?;
            let mut tail = Vec::with_capacity(output.len() + 1 + drv.len());
            tail.extend_from_slice(output);
            tail.push(b'!');
            tail.extend_from_slice(drv);
            (ev.arena.alloc_bytes(&tail), expected)
        }
        Ctx::Impure(_) => unreachable!("filtered out above"),
    };
    if text != expected {
        return Err(Error::boxed(
            ErrorKind::Eval,
            format!(
                "string '{text}' has context with a derivation output, but the string is not the right placeholder for it. It should be '{expected}'"
            ),
            pos,
        ));
    }
    let (layers, root) = split_built(drv);
    let root = parse_store_path(root, pos)?;
    let rendered = render_built(ev, &root, &layers, &name, pos)?;
    let bytes = ev.arena.alloc_bytes(rendered.as_bytes());
    let output = ev.arena.alloc_bytes(name.as_bytes());
    Ok(ev.arena.mk_string_ctx(bytes, &[Ctx::Built { drv, output }]))
}

/// `load_derivation_from_store` plus a `File` observation of the `.drv`.
pub fn load_drv_traced(trace: &crate::trace::Trace, p: &StorePath) -> Result<Derivation, DrvError> {
    if !trace.is_on() {
        return load_derivation_from_store(p);
    }
    let full = p.to_string();
    let read = || -> std::io::Result<(std::fs::Metadata, Vec<u8>)> {
        let mut f = std::fs::File::open(&full)?;
        let meta = f.metadata()?;
        let mut d = Vec::with_capacity(meta.len() as usize);
        std::io::Read::read_to_end(&mut f, &mut d)?;
        Ok((meta, d))
    };
    let (meta, bytes) =
        read().map_err(|e| DrvError(format!("cannot read derivation '{full}': {e}")))?;
    trace.file(full.as_bytes(), &meta, &bytes);
    Derivation::parse(&bytes, Derivation::name_from_path(p)?)
}

/// The derivation as this evaluation wrote it, else from the store.
pub fn load_drv(ev: &Evaluator, p: &StorePath) -> Result<Derivation, DrvError> {
    match ev.drv.drvs.get(p).and_then(|i| i.aterm) {
        Some(aterm) => Derivation::parse(&aterm, Derivation::name_from_path(p)?),
        None => load_drv_traced(ev.trace(), p),
    }
}

/// Output names and `.drv` references of `p`: from this evaluation's table,
/// else parsed from `/nix/store`.
fn drv_info(ev: &mut Evaluator, p: &StorePath, pos: Pos) -> EvalResult<DrvInfo> {
    if let Some(i) = ev.drv.drvs.get(p) {
        return Ok(i);
    }
    let drv = load_drv_traced(ev.trace(), p).map_err(|e| drv_err(e, pos))?;
    let mut references: BTreeSet<StorePath> = drv.input_srcs.clone();
    references.extend(drv.input_drvs.keys().cloned());
    let info = DrvInfo {
        outputs: drv.outputs.keys().cloned().collect(),
        references,
        aterm: None,
        pos: None,
        attr: None,
    };
    ev.drv.drvs.insert(p.clone(), info.clone());
    Ok(info)
}

/// Mirrors `computeFSClosure` on a `.drv` path as used for `DrvDeep` context:
/// the drv, its inputs, and their closures. A source this evaluation added
/// with `builtins.toFile` carries its own references; a store path that was
/// already on disk is a leaf, because eval does not ask the daemon.
fn drv_closure(
    ev: &mut Evaluator,
    start: &StorePath,
    pos: Pos,
) -> EvalResult<Vec<(StorePath, Option<BTreeSet<String>>)>> {
    let mut seen: BTreeSet<StorePath> = BTreeSet::new();
    let mut todo = vec![start.clone()];
    let mut out = Vec::new();
    while let Some(p) = todo.pop() {
        if !seen.insert(p.clone()) {
            continue;
        }
        if p.is_derivation() {
            let info = drv_info(ev, &p, pos)?;
            todo.extend(info.references);
            out.push((p, Some(info.outputs)));
        } else {
            if let Some(o) = ev.known.origin(&p)
                && let Some(refs) = o.text_refs()
            {
                todo.extend(refs.iter().cloned());
            }
            out.push((p, None));
        }
    }
    Ok(out)
}

/// Mirrors primops.cc:prim_placeholder.
fn placeholder(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.placeholder",
    )?;
    let s = utf8(s, pos)?;
    Ok(ev.mk_string(hash_placeholder(&s).as_bytes()))
}

/// Mirrors primops.cc:derivationToValue for `import /nix/store/...drv`:
/// returns `None` when `path` is not an existing `.drv` store path.
pub fn import_derivation(ev: &mut Evaluator, path: &[u8], pos: Pos) -> EvalResult<Option<Word>> {
    if !path.ends_with(DRV_EXTENSION.as_bytes()) {
        return Ok(None);
    }
    let Ok(sp) = std::str::from_utf8(path).map(StorePath::parse) else {
        return Ok(None);
    };
    let Ok(sp) = sp else {
        return Ok(None);
    };
    if super::store_hooks::stat(ev, path, true) != crate::trace::Kind::Regular {
        return Ok(None);
    }
    let drv = load_drv(ev, &sp).map_err(|e| drv_err(e, pos))?;
    let drv_name = Derivation::name_from_path(&sp)
        .map_err(|e| drv_err(e, pos))?
        .to_string();
    let path_bytes = ev.arena.alloc_bytes(path);
    let mut entries = Vec::with_capacity(3 + drv.outputs.len());
    let dp = ev
        .arena
        .mk_string_ctx(path_bytes, &[Ctx::DrvDeep(path_bytes)]);
    entries.push((ev.s.drv_path, Pos::NONE, dp));
    let name = ev.mk_string(drv.env_get("name").unwrap_or(b""));
    entries.push((ev.s.name, Pos::NONE, name));
    let mut names = Vec::with_capacity(drv.outputs.len());
    for (name, out) in &drv.outputs {
        let w = output_string(ev, &sp, path_bytes, &drv_name, name, out, pos)?;
        entries.push((ev.sym(name.as_bytes()), Pos::NONE, w));
        names.push(ev.mk_string(name.as_bytes()));
    }
    let outputs_sym = syms(ev).outputs;
    let list = ev.mk_list(&names);
    entries.push((outputs_sym, Pos::NONE, list));
    let attrs = ev.mk_attrs(&mut entries, pos)?;
    let f = match ev.drv.imported_drv_fn {
        Some(f) => f,
        None => {
            let ir = ev.compile_internal(
                b"\xc2\xabimported-drv-to-derivation\xc2\xbb",
                IMPORTED_DRV_TO_DERIVATION,
            )?;
            let root = ev.root_env;
            let f = ev.eval(root, ir)?;
            ev.drv.imported_drv_fn = Some(f);
            f
        }
    };
    Ok(Some(ev.call1(f, attrs, pos)?))
}

/// Nix 2.34.8 src/libexpr/imported-drv-to-derivation.nix.
const IMPORTED_DRV_TO_DERIVATION: &[u8] = b"attrs@{ drvPath, outputs, name, ... }:
let
  commonAttrs = (builtins.listToAttrs outputsList) // {
    all = map (x: x.value) outputsList;
    inherit drvPath name;
    type = \"derivation\";
  };
  outputToAttrListElement = outputName: {
    name = outputName;
    value = commonAttrs // {
      outPath = builtins.getAttr outputName attrs;
      inherit outputName;
    };
  };
  outputsList = map outputToAttrListElement outputs;
in
(builtins.head outputsList).value
";

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference values come from nix 2.35pre, `--extra-experimental-features
    /// 'ca-derivations dynamic-derivations'`, over
    ///   producer = name: derivation { inherit name; system = "x86_64-linux";
    ///     builder = "/bin/sh"; args = [ "-c" "echo done > $out" ];
    ///     __contentAddressed = true; outputHashMode = "text";
    ///     outputHashAlgo = "sha256"; };
    ///   one = builtins.outputOf (producer "p.drv").outPath "out";
    ///   two = builtins.outputOf (builtins.outputOf (producer "q.drv.drv").outPath "out") "out";
    #[test]
    fn a_dynamic_output_renders_the_placeholder_nix_renders() {
        let p = StorePath::parse("/nix/store/y86qixskwxs9z8i9gbd9x06h3q0hy1ji-p.drv.drv").unwrap();
        let h = unknown_derivation(&unknown_ca_output(&p, "p.drv", "out"), "out");
        assert_eq!(
            render_placeholder(&h),
            "/1k5662fdf252qa1rsrq0s5xc0w9a9wb1qsgzfk2i9zhx4as0qf76"
        );

        // one more layer: the output of the derivation that output is
        let q =
            StorePath::parse("/nix/store/czdvshq1r2b3acsxv0r6jmrr3jg302mq-q.drv.drv.drv").unwrap();
        let mut h = unknown_ca_output(&q, "q.drv.drv", "out");
        for _ in 0..2 {
            h = unknown_derivation(&h, "out");
        }
        assert_eq!(
            render_placeholder(&h),
            "/16v25j0s9i1rfcgz7lcr8hlsh14m83b35f0c63lkgcpwj9nj4c9s"
        );
    }

    /// `hashPlaceholder` is the env value of a floating output and is not the
    /// same function; a mix-up would move every CA drvPath.
    #[test]
    fn the_two_placeholders_are_not_the_same() {
        let p = StorePath::parse("/nix/store/y86qixskwxs9z8i9gbd9x06h3q0hy1ji-p.drv.drv").unwrap();
        assert_ne!(
            downstream_placeholder(&p, "p.drv", "out"),
            hash_placeholder("out")
        );
    }
}
