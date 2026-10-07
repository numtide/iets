//! String context introspection (primops/context.cc): getContext,
//! appendContext, addDrvOutputDependencies. hasContext and the two
//! `unsafeDiscard*` primops live in `core`.

use std::collections::BTreeMap;
use std::str;

use super::Registry;
use crate::eval::{ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::runtime::{Ctx, Word, ctx_to_arena, split_built};
use crate::store::path::StorePath;

pub fn register(r: &mut Registry) {
    r.op("__getContext", 1, get_context);
    r.op("__appendContext", 2, append_context);
    r.op("__addDrvOutputDependencies", 1, add_drv_output_dependencies);
}

#[derive(Default)]
struct Info {
    path: bool,
    all_outputs: bool,
    outputs: Vec<&'static [u8]>,
}

fn get_context(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.getContext",
    )?;
    let mut infos: BTreeMap<&'static [u8], Info> = BTreeMap::new();
    for c in s.ctx {
        match c {
            Ctx::Opaque(p) => infos.entry(p).or_default().path = true,
            Ctx::DrvDeep(d) => infos.entry(d).or_default().all_outputs = true,
            // primops/context.cc resolves the derived path first, which builds
            // the producer; an evaluation does not build
            Ctx::Built { drv, output } if !split_built(drv).0.is_empty() => {
                bail!(
                    ErrorKind::Unsupported,
                    pos,
                    "iets: builtins.getContext cannot handle the output '{}' of a dynamic derivation",
                    String::from_utf8_lossy(output)
                )
            }
            Ctx::Built { drv, output } => infos.entry(drv).or_default().outputs.push(output),
            Ctx::Impure(_) => {}
        }
    }
    let s_path = ev.s.path;
    let s_all = ev.sym(b"allOutputs");
    let s_outputs = ev.sym(b"outputs");
    let mut entries = Vec::with_capacity(infos.len());
    for (p, info) in infos {
        let mut inner = Vec::new();
        if info.path {
            inner.push((s_path, Pos::NONE, Word::TRUE));
        }
        if info.all_outputs {
            inner.push((s_all, Pos::NONE, Word::TRUE));
        }
        if !info.outputs.is_empty() {
            let items: Vec<Word> = info
                .outputs
                .iter()
                .map(|o| ev.arena.mk_string_static(o, &[]))
                .collect();
            let l = ev.mk_list(&items);
            inner.push((s_outputs, Pos::NONE, l));
        }
        let v = ev.mk_attrs(&mut inner, pos)?;
        let sym = ev.sym(p);
        entries.push((sym, Pos::NONE, v));
    }
    ev.mk_attrs(&mut entries, pos)
}

fn append_context(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let orig = ev.force_string(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.appendContext",
    )?;
    let attrs = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.appendContext",
    )?;
    let s_path = ev.s.path;
    let s_all = ev.sym(b"allOutputs");
    let s_outputs = ev.sym(b"outputs");
    let mut ctx: Vec<Ctx> = orig.ctx.to_vec();
    for a in attrs.iter() {
        let name: Vec<u8> = ev.interner.name(a.sym).to_vec();
        let sp = str::from_utf8(&name)
            .ok()
            .and_then(|n| StorePath::parse(n).ok());
        let Some(sp) = sp else {
            bail!(
                ErrorKind::Eval,
                a.pos,
                "context key '\"{}\"' is not a store path",
                String::from_utf8_lossy(&name)
            );
        };
        let full = ev.arena.alloc_bytes(&name);
        let v = ev.force_attrs(
            a.cell.get(),
            a.pos,
            "while evaluating the value of a string context",
        )?;
        if let Some(pa) = v.get(s_path)
            && ev.force_bool(
                pa.cell.get(),
                pa.pos,
                "while evaluating the `path` attribute of a string context",
            )?
        {
            ctx.push(Ctx::Opaque(full));
        }
        if let Some(aa) = v.get(s_all)
            && ev.force_bool(
                aa.cell.get(),
                aa.pos,
                "while evaluating the `allOutputs` attribute of a string context",
            )?
        {
            if !sp.is_derivation() {
                bail!(
                    ErrorKind::Eval,
                    a.pos,
                    "tried to add all-outputs context of \"{}\", which is not a derivation, to a string",
                    String::from_utf8_lossy(&name)
                );
            }
            ctx.push(Ctx::DrvDeep(full));
        }
        if let Some(oa) = v.get(s_outputs) {
            let l = ev.force_list(
                oa.cell.get(),
                oa.pos,
                "while evaluating the `outputs` attribute of a string context",
            )?;
            if l.len > 0 && !sp.is_derivation() {
                bail!(
                    ErrorKind::Eval,
                    a.pos,
                    "tried to add derivation output context of \"{}\", which is not a derivation, to a string",
                    String::from_utf8_lossy(&name)
                );
            }
            for c in l.cells() {
                let output = ev.force_string_no_ctx(
                    c.get(),
                    oa.pos,
                    "while evaluating an output name within a string context",
                )?;
                ctx.push(Ctx::Built { drv: full, output });
            }
        }
    }
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_static(orig.bytes, ctx))
}

fn add_drv_output_dependencies(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the argument passed to builtins.addDrvOutputDependencies",
        false,
        false,
        true,
    )?;
    ctx.retain(Ctx::is_store);
    ctx.sort();
    ctx.dedup();
    if ctx.len() != 1 {
        bail!(
            ErrorKind::Eval,
            pos,
            "context of string '{}' must have exactly one element, but has {}",
            String::from_utf8_lossy(&s),
            ctx.len()
        );
    }
    let c = match ctx[0] {
        Ctx::Opaque(p) => {
            if !p.ends_with(b".drv") {
                bail!(
                    ErrorKind::Eval,
                    pos,
                    "path '{}' is not a derivation",
                    String::from_utf8_lossy(p)
                );
            }
            Ctx::DrvDeep(p)
        }
        Ctx::Built { output, .. } => bail!(
            ErrorKind::Eval,
            pos,
            "`addDrvOutputDependencies` can only act on derivations, not on a derivation output such as '{}'",
            String::from_utf8_lossy(output)
        ),
        d @ Ctx::DrvDeep(_) => d,
        Ctx::Impure(_) => unreachable!("impure markers were dropped above"),
    };
    let bytes = ev.arena.alloc_bytes(&s);
    let ctx = ev.arena.alloc_slice(&[c]);
    Ok(ev.arena.mk_string_static(bytes, ctx))
}
