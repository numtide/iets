//! Attribute set primops.

use std::collections::HashMap;

use super::Registry;
use crate::eval::{ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::lint::purity;
use crate::runtime::Word;
use crate::runtime::arena::Stat;
use crate::syntax::symbol::Sym;
use crate::{parallel, print};

pub fn register(r: &mut Registry) {
    r.op("__attrNames", 1, attr_names);
    r.op("__attrValues", 1, attr_values);
    r.op("__getAttr", 2, get_attr);
    r.op("__hasAttr", 2, has_attr);
    r.op("removeAttrs", 2, remove_attrs);
    r.op("__intersectAttrs", 2, intersect_attrs);
    r.op("__mapAttrs", 2, map_attrs);
    r.op("__listToAttrs", 1, list_to_attrs);
    r.op("__zipAttrsWith", 2, zip_attrs_with);
    r.op("__catAttrs", 2, cat_attrs);
    r.op("__groupBy", 2, group_by);
}

fn attr_names(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.attrNames",
    )?;
    let sorted = print::lexicographic_order(ev, a);
    let mut items = Vec::with_capacity(sorted.len());
    for attr in sorted {
        items.push(ev.sym_string(attr.sym));
    }
    Ok(ev.mk_list(&items))
}

fn attr_values(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.attrValues",
    )?;
    let sorted = print::lexicographic_order(ev, a);
    let items: Vec<Word> = sorted.iter().map(|a| a.cell.get()).collect();
    Ok(ev.mk_list(&items))
}

fn get_attr(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.getAttr",
    )?;
    let a = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.getAttr",
    )?;
    let sym = ev.sym(name);
    match a.get(sym) {
        Some(attr) if sym == ev.s.rev_count => purity::count_read(ev, &attr.cell, pos),
        Some(attr) => ev.force_cell(&attr.cell),
        None => bail!(
            ErrorKind::MissingAttr,
            pos,
            "attribute '{}' missing",
            String::from_utf8_lossy(name)
        ),
    }
}

fn has_attr(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hasAttr",
    )?;
    let a = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.hasAttr",
    )?;
    let sym = ev.sym(name);
    Ok(Word::from_bool(a.get(sym).is_some()))
}

fn remove_attrs(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.removeAttrs",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.removeAttrs",
    )?;
    let mut names: Vec<Sym> = Vec::with_capacity(l.len as usize);
    for c in l.cells() {
        let w = c.get();
        let n = ev.force_string_no_ctx(
            w,
            pos,
            "while evaluating the values of the second argument passed to builtins.removeAttrs",
        )?;
        names.push(ev.sym(n));
    }
    names.sort();
    names.dedup();
    let out: Vec<(Sym, Pos, Word)> = a
        .iter()
        .filter(|at| names.binary_search(&at.sym).is_err())
        .map(|at| (at.sym, at.pos, at.cell.get()))
        .collect();
    Ok(ev.arena.mk_attrs_sorted_as(&out, Stat::AttrsRemove))
}

fn intersect_attrs(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let left = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.intersectAttrs",
    )?;
    let right = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.intersectAttrs",
    )?;
    let mut out: Vec<(Sym, Pos, Word)> = Vec::with_capacity(left.len().min(right.len()));
    if left.len() < right.len() {
        for l in left.iter() {
            if let Some(r) = right.get(l.sym) {
                out.push((r.sym, r.pos, r.cell.get()));
            }
        }
    } else {
        for r in right.iter() {
            if left.get(r.sym).is_some() {
                out.push((r.sym, r.pos, r.cell.get()));
            }
        }
    }
    Ok(ev.arena.mk_attrs_sorted_as(&out, Stat::AttrsInter))
}

fn map_attrs(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.mapAttrs",
    )?;
    let f = args[0];
    let a = ev.arena.flat(a);
    if a.len() <= Word::MAP_MAX_IDX + 1 {
        // one map word per entry, no thunk object (docs/explanation/evaluation.md)
        return Ok(ev.arena.mk_map_attrs(f, a));
    }
    let mut out: Vec<(Sym, Pos, Word)> = Vec::with_capacity(a.len());
    for attr in a.iter() {
        let name = ev.sym_string(attr.sym);
        let v = ev.arena.mk_thunk_app2(f, name, attr.cell.get());
        out.push((attr.sym, attr.pos, v));
    }
    Ok(ev.arena.mk_attrs_sorted_as(&out, Stat::AttrsMap))
}

fn list_to_attrs(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.listToAttrs",
    )?;
    parallel::fanout_list(ev, l); // parallel
    let mut entries: Vec<(Sym, Pos, Word)> = Vec::with_capacity(l.len as usize);
    let mut seen: HashMap<Sym, ()> = HashMap::new();
    for c in l.cells() {
        let w = c.get();
        let a = ev.force_attrs(
            w,
            pos,
            "while evaluating an element of the list passed to builtins.listToAttrs",
        )?;
        let Some(n) = a.get(ev.s.name) else {
            bail!(
                ErrorKind::Type,
                pos,
                "attribute 'name' missing in a {{name=...; value=...;}} pair"
            );
        };
        let nv = n.cell.get();
        let name = ev.force_string_no_ctx(
            nv,
            n.pos,
            "while evaluating the `name` attribute of an element of the list passed to builtins.listToAttrs",
        )?;
        let sym = ev.sym(name);
        if seen.insert(sym, ()).is_some() {
            continue;
        }
        let Some(v) = a.get(ev.s.value) else {
            bail!(
                ErrorKind::Type,
                pos,
                "attribute 'value' missing in a {{name=...; value=...;}} pair"
            );
        };
        entries.push((sym, v.pos, v.cell.get()));
    }
    entries.sort_by_key(|e| e.0);
    Ok(ev.arena.mk_attrs_sorted_as(&entries, Stat::AttrsListTo))
}

fn zip_attrs_with(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.zipAttrsWith",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.zipAttrsWith",
    )?;
    parallel::fanout_list(ev, l); // parallel
    let mut groups: HashMap<Sym, Vec<Word>> = HashMap::new();
    let mut order: Vec<Sym> = Vec::new();
    for c in l.cells() {
        let w = c.get();
        let a = ev.force_attrs(
            w,
            Pos::NONE,
            "while evaluating a value of the list passed as second argument to builtins.zipAttrsWith",
        )?;
        for attr in a.iter() {
            let e = groups.entry(attr.sym).or_insert_with(|| {
                order.push(attr.sym);
                Vec::new()
            });
            e.push(attr.cell.get());
        }
    }
    let mut out: Vec<(Sym, Pos, Word)> = Vec::with_capacity(groups.len());
    for sym in order {
        let vals = &groups[&sym];
        let name = ev.sym_string(sym);
        let list = ev.mk_list(vals);
        let v = ev.arena.mk_thunk_app2(f, name, list);
        out.push((sym, Pos::NONE, v));
    }
    out.sort_by_key(|e| e.0);
    Ok(ev.arena.mk_attrs_sorted_as(&out, Stat::AttrsZip))
}

fn cat_attrs(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.catAttrs",
    )?;
    let sym = ev.sym(name);
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.catAttrs",
    )?;
    parallel::fanout_list(ev, l); // parallel
    let mut out = Vec::new();
    for c in l.cells() {
        let w = c.get();
        let a = ev.force_attrs(
            w,
            pos,
            "while evaluating an element in the list passed as second argument to builtins.catAttrs",
        )?;
        if let Some(attr) = a.get(sym) {
            out.push(attr.cell.get());
        }
    }
    Ok(ev.mk_list(&out))
}

fn group_by(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.groupBy",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.groupBy",
    )?;
    parallel::fanout_list(ev, l); // parallel
    let mut groups: HashMap<Sym, Vec<Word>> = HashMap::new();
    for c in l.cells() {
        let elem = c.get();
        let r = ev.call1(f, elem, pos)?;
        let name = ev.force_string_no_ctx(
            r,
            pos,
            "while evaluating the return value of the grouping function passed to builtins.groupBy",
        )?;
        let sym = ev.sym(name);
        groups.entry(sym).or_default().push(elem);
    }
    let mut out: Vec<(Sym, Pos, Word)> = Vec::with_capacity(groups.len());
    for (sym, vals) in groups {
        let list = ev.mk_list(&vals);
        out.push((sym, Pos::NONE, list));
    }
    out.sort_by_key(|e| e.0);
    Ok(ev.arena.mk_attrs_sorted(&out))
}
