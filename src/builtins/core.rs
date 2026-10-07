//! Import, types, control flow, errors, tracing, and introspection.

use super::Registry;
use crate::compile::ir::ParamIr;
use crate::eval::{ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::runtime::{Ctx, Value, Word, ctx_to_arena};

pub fn register(r: &mut Registry) {
    r.op("import", 1, import);
    r.op("scopedImport", 2, scoped_import);
    r.op("toString", 1, to_string);
    r.op("__typeOf", 1, type_of);
    r.op("isNull", 1, is_null);
    r.op("__isFunction", 1, is_function);
    r.op("__isInt", 1, is_int);
    r.op("__isFloat", 1, is_float);
    r.op("__isString", 1, is_string);
    r.op("__isBool", 1, is_bool);
    r.op("__isPath", 1, is_path);
    r.op("__isList", 1, is_list);
    r.op("__isAttrs", 1, is_attrs);
    r.op("__seq", 2, seq);
    r.op("__deepSeq", 2, deep_seq);
    r.op("abort", 1, abort);
    r.op("throw", 1, throw);
    r.op("__trace", 2, trace);
    r.op("__traceVerbose", 2, second);
    r.op("__warn", 2, warn);
    r.op("__addErrorContext", 2, add_error_context);
    r.op("__tryEval", 1, try_eval);
    r.op("__functionArgs", 1, function_args);
    r.op("__unsafeGetAttrPos", 2, unsafe_get_attr_pos);
    r.op("__hasContext", 1, has_context);
    r.op(
        "__unsafeDiscardStringContext",
        1,
        unsafe_discard_string_context,
    );
    r.op(
        "__unsafeDiscardOutputDependency",
        1,
        unsafe_discard_output_dependency,
    );
    r.op("__findFile", 2, find_file);
}

fn import_path(ev: &mut Evaluator, w: Word, pos: Pos) -> EvalResult<Vec<u8>> {
    let w = ev.force(w)?;
    match w.view() {
        Value::Path(p) => Ok(p.path.to_vec()),
        Value::String(s) => {
            crate::lint::purity::ifd(ev, s.ctx, pos)?;
            super::paths::realise_path(ev, w, pos)
        }
        // eval.cc:coerceToString tries `tryAttrsToString` before `outPath`
        Value::Attrs(a) => {
            let next = match a.get(ev.s.to_string) {
                Some(f) => {
                    let f = ev.force_cell(&f.cell)?;
                    ev.call1(f, w, pos)?
                }
                None => match a.get(ev.s.out_path) {
                    Some(o) => o.cell.get(),
                    None => {
                        return ev.type_error(
                            "a path",
                            w,
                            pos,
                            "while evaluating the argument passed to import",
                        );
                    }
                },
            };
            import_path(ev, next, pos)
        }
        _ => ev.type_error(
            "a path",
            w,
            pos,
            "while evaluating the argument passed to import",
        ),
    }
}

fn import(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let p = import_path(ev, args[0], pos)?;
    if let Some(w) = super::derivation::import_derivation(ev, &p, pos)? {
        return Ok(w);
    }
    ev.eval_file(&p, pos)
}

fn scoped_import(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let scope = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.scopedImport",
    )?;
    let p = import_path(ev, args[1], pos)?;
    ev.scoped_import(scope, &p, pos)
}

fn to_string(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the first argument passed to builtins.toString",
        true,
        false,
        true,
    )?;
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(&s, ctx))
}

fn type_of(ev: &mut Evaluator, args: &[Word], _pos: Pos) -> EvalResult<Word> {
    let w = ev.force(args[0])?;
    Ok(ev.mk_string(w.value_type().type_of().as_bytes()))
}

macro_rules! is_type {
    ($name:ident, $pat:pat) => {
        fn $name(ev: &mut Evaluator, args: &[Word], _pos: Pos) -> EvalResult<Word> {
            let w = ev.force(args[0])?;
            Ok(Word::from_bool(matches!(w.view(), $pat)))
        }
    };
}

is_type!(is_null, Value::Null);
is_type!(
    is_function,
    Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_)
);
is_type!(is_int, Value::Int(_));
is_type!(is_float, Value::Float(_));
is_type!(is_string, Value::String(_));
is_type!(is_bool, Value::Bool(_));
is_type!(is_path, Value::Path(_));
is_type!(is_list, Value::List(_));
is_type!(is_attrs, Value::Attrs(_));

fn seq(ev: &mut Evaluator, args: &[Word], _pos: Pos) -> EvalResult<Word> {
    ev.force(args[0])?;
    ev.force(args[1])
}

fn deep_seq(ev: &mut Evaluator, args: &[Word], _pos: Pos) -> EvalResult<Word> {
    ev.force_deep(args[0])?;
    ev.force(args[1])
}

fn second(ev: &mut Evaluator, args: &[Word], _pos: Pos) -> EvalResult<Word> {
    ev.force(args[1])
}

fn abort(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the error message passed to builtins.abort",
        false,
        true,
        true,
    )?;
    bail!(
        ErrorKind::Abort,
        pos,
        "evaluation aborted with the following error message: '{}'",
        String::from_utf8_lossy(&s)
    )
}

fn throw(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the error message passed to builtin.throw",
        false,
        true,
        true,
    )?;
    bail!(ErrorKind::Throw, pos, "{}", String::from_utf8_lossy(&s))
}

fn say(ev: &mut Evaluator, line: String) {
    // parallel: helpers journal, the demand path prints
    if !crate::parallel::journal(ev, line.clone()) {
        ev.lints.flush();
        crate::log::line(&line);
    }
}

fn trace(ev: &mut Evaluator, args: &[Word], _pos: Pos) -> EvalResult<Word> {
    let w = ev.force(args[0])?;
    let msg = match w.view() {
        Value::String(s) => String::from_utf8_lossy(s.bytes).into_owned(),
        _ => crate::print::show_value_short(ev, w),
    };
    say(ev, format!("trace: {msg}"));
    ev.force(args[1])
}

fn warn(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string(
        args[0],
        pos,
        "while evaluating the first argument; the message passed to builtins.warn",
    )?;
    say(
        ev,
        format!("evaluation warning: {}", String::from_utf8_lossy(s.bytes)),
    );
    ev.force(args[1])
}

fn add_error_context(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    match ev.force(args[1]) {
        Ok(v) => Ok(v),
        Err(mut e) => {
            let mut ctx = Vec::new();
            let msg = ev.coerce_to_string(
                args[0],
                &mut ctx,
                pos,
                "while evaluating the error message passed to builtins.addErrorContext",
                false,
                false,
                true,
            )?;
            e.add_trace(Pos::NONE, String::from_utf8_lossy(&msg).into_owned());
            Err(e)
        }
    }
}

fn try_eval(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    ev.try_level += 1;
    #[cfg(feature = "stats2")]
    let scope = ev.arena.stats.census.scope_enter(ev.arena.bytes_allocated);
    let r = ev.force(args[0]);
    ev.try_level -= 1;
    #[cfg(feature = "stats2")]
    if scope {
        let (bytes, escapes) = ev.arena.stats.census.scope_leave(ev.arena.bytes_allocated);
        let t = &mut ev.arena.stats.census.try_scopes;
        t[0] += 1;
        if matches!(&r, Err(e) if e.is_catchable()) {
            t[1] += 1;
            t[2] += bytes;
            if escapes == 0 {
                t[3] += 1;
                t[4] += bytes;
            }
        }
    }
    let (success, value) = match r {
        Ok(v) => (Word::TRUE, v),
        Err(e) if e.is_catchable() => (Word::FALSE, Word::FALSE),
        Err(e) => return Err(e),
    };
    let mut entries = vec![
        (ev.s.success, Pos::NONE, success),
        (ev.s.value, Pos::NONE, value),
    ];
    ev.mk_attrs(&mut entries, pos)
}

fn function_args(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let w = ev.force(args[0])?;
    match w.view() {
        Value::PrimOp(_) | Value::PrimOpApp(_) => Ok(ev.empty_attrs()),
        Value::Lambda(l) => match &l.lam.param {
            ParamIr::Formals { formals, .. } => {
                let entries: Vec<(_, _, _)> = formals
                    .iter()
                    .map(|f| (f.sym, f.pos, Word::from_bool(f.default.is_some())))
                    .collect();
                Ok(ev.arena.mk_attrs_sorted(&entries))
            }
            ParamIr::Ident => Ok(ev.empty_attrs()),
        },
        _ => bail!(ErrorKind::Type, pos, "'functionArgs' requires a function"),
    }
}

fn unsafe_get_attr_pos(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.unsafeGetAttrPos",
    )?;
    let attrs = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.unsafeGetAttrPos",
    )?;
    let sym = ev.sym(name);
    #[cfg(feature = "stats2")]
    {
        ev.arena.stats.census.get_attr_pos_calls += 1;
    }
    match attrs.get(sym) {
        None => Ok(Word::NULL),
        Some(a) => Ok(ev.mk_pos_attrs(a.pos)),
    }
}

fn has_context(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.hasContext",
    )?;
    Ok(Word::from_bool(s.ctx.iter().any(Ctx::is_store)))
}

fn unsafe_discard_string_context(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut ctx = Vec::new();
    let s = ev.coerce_to_string(
        args[0],
        &mut ctx,
        pos,
        "while evaluating the argument passed to builtins.unsafeDiscardStringContext",
        false,
        true,
        true,
    )?;
    if ctx.is_empty()
        && let Some(so) = ev.force(args[0])?.as_string()
    {
        return Ok(Word::from_ref(so));
    }
    Ok(ev.mk_string(&s))
}

fn unsafe_discard_output_dependency(
    ev: &mut Evaluator,
    args: &[Word],
    pos: Pos,
) -> EvalResult<Word> {
    let mut ctx0 = Vec::new();
    let bytes = ev.coerce_to_string(
        args[0],
        &mut ctx0,
        pos,
        "while evaluating the argument passed to builtins.unsafeDiscardOutputDependency",
        false,
        true,
        true,
    )?;
    let ctx: Vec<Ctx> = ctx0
        .iter()
        .map(|c| match c {
            Ctx::DrvDeep(d) => Ctx::Opaque(d),
            other => *other,
        })
        .collect();
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(&bytes, ctx))
}

fn find_file(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.findFile",
    )?;
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(l.len as usize);
    let prefix_sym = ev.s.prefix;
    let path_sym = ev.s.path;
    for c in l.cells() {
        let w = c.get();
        let a = ev.force_attrs(
            w,
            pos,
            "while evaluating an element of the list passed to builtins.findFile",
        )?;
        let prefix = match a.get(prefix_sym) {
            Some(p) => {
                let pv = p.cell.get();
                ev.force_string_no_ctx(pv, pos, "while evaluating the `prefix` attribute of an element of the list passed to builtins.findFile")?
                    .to_vec()
            }
            None => Vec::new(),
        };
        let Some(pa) = a.get(path_sym) else {
            bail!(
                ErrorKind::Type,
                pos,
                "attribute 'path' missing in an element of the list passed to builtins.findFile"
            );
        };
        let pv = pa.cell.get();
        let mut ctx = Vec::new();
        let path = ev.coerce_to_string(
            pv,
            &mut ctx,
            pos,
            "while evaluating the `path` attribute of an element of the list passed to builtins.findFile",
            false,
            false,
            true,
        )?;
        entries.push((prefix, path.into_owned()));
    }
    let name = ev.force_string_no_ctx(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.findFile",
    )?;
    let p = ev.find_file(&entries, name, pos)?;
    Ok(ev.arena.mk_path(&p))
}
