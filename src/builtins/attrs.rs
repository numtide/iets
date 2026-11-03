//! Attribute set primops.

use super::Registry;
use super::bits::Bits;
use crate::eval::{ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::lint::purity;
use crate::runtime::Word;
use crate::runtime::arena::Stat;
use crate::runtime::value::{Attr, for_each_leaf};
use crate::syntax::symbol::{FxHashMap, FxHashSet, Sym};
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
    let items: Vec<Word> = sorted.map(|a| a.cell.get()).collect();
    Ok(ev.mk_list(&items))
}

fn get_attr(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let sym = ev.force_sym(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.getAttr",
    )?;
    let a = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.getAttr",
    )?;
    match a.get(sym) {
        Some(attr) if sym == ev.s.rev_count => purity::count_read(ev, &attr.cell, pos),
        Some(attr) => ev.force_cell(&attr.cell),
        None => bail!(
            ErrorKind::MissingAttr,
            pos,
            "attribute '{}' missing",
            String::from_utf8_lossy(ev.interner.name(sym))
        ),
    }
}

fn has_attr(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let sym = ev.force_sym(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.hasAttr",
    )?;
    let a = ev.force_attrs(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.hasAttr",
    )?;
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
    if l.len == 0 {
        return Ok(Word::from_ref(a));
    }
    // Each name is looked up in the set and its entry marked: sorting the
    // names took half the builtin on long lists.
    let flat = ev.arena.flat(a);
    let src = flat.attrs_raw();
    let mut marks = Bits::new(src.len());
    let mut removed = 0;
    for c in l.cells() {
        let name = ev.force_sym(
            c.get(),
            pos,
            "while evaluating the values of the second argument passed to builtins.removeAttrs",
        )?;
        let Some(at) = flat.get(name) else {
            continue;
        };
        // SAFETY: `get` answers an entry of `src`.
        let i = unsafe { (at as *const Attr).offset_from(src.as_ptr()) } as usize;
        if marks.set(i) {
            removed += 1;
        }
    }
    // nothing removed: the same entries, so the same set
    if removed == 0 {
        return Ok(Word::from_ref(a));
    }
    if removed == src.len() {
        return Ok(ev.arena.mk_attrs_sorted_as(&[], Stat::AttrsRemove));
    }
    let out = ev
        .arena
        .alloc_attrs_as(src.len() - removed, Stat::AttrsRemove);
    let kept = src.iter().enumerate().filter(|(i, _)| !marks.get(*i));
    for (n, (_, at)) in kept.enumerate() {
        out.init(n, at.sym, at.pos, at.cell.get());
    }
    Ok(Word::from_ref(out))
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
    // `len` of a union iterates it, so compare bounds first
    let (l, r) = (left.len_bounds(), right.len_bounds());
    let left_small = if l.1 < r.0 {
        true
    } else if r.1 <= l.0 {
        false
    } else {
        left.len() < right.len()
    };
    let (small, big) = if left_small {
        (left, right)
    } else {
        (right, left)
    };
    let mut out: Vec<(Sym, Pos, Word)> = Vec::with_capacity(small.len_bounds().1);
    if small.len_bounds().1 < 32 {
        for k in small.iter() {
            if let Some(b) = big.get(k.sym) {
                let r = if left_small { b } else { k };
                out.push((r.sym, r.pos, r.cell.get()));
            }
        }
        return Ok(ev.arena.mk_attrs_sorted_as(&out, Stat::AttrsInter));
    }
    // One ordered walk per leaf of the big side instead of a binary search
    // per key and leaf: nixpkgs intersects thousands of names with `pkgs`.
    let keys: Vec<&Attr> = small.iter().collect();
    let mut hit = vec![false; keys.len()];
    for_each_leaf(big, |leaf| {
        let mut rest = leaf.attrs_raw();
        for (k, h) in keys.iter().zip(hit.iter_mut()) {
            rest = &rest[gallop(rest, k.sym)..];
            match rest.first() {
                None => break,
                Some(a) if a.sym == k.sym => *h = true,
                Some(_) => {}
            }
        }
    });
    for (k, _) in keys.iter().zip(&hit).filter(|(_, h)| **h) {
        // the value is the right side's, and a union's top leaf wins
        let r = if left_small {
            right.get(k.sym).expect("found in a leaf")
        } else {
            k
        };
        out.push((r.sym, r.pos, r.cell.get()));
    }
    Ok(ev.arena.mk_attrs_sorted_as(&out, Stat::AttrsInter))
}

/// Index of the first entry of the sorted `attrs` not below `sym`:
/// exponential then binary search, so a walk over many keys stays linear in
/// the larger side and a few keys stay logarithmic.
fn gallop(attrs: &[Attr], sym: Sym) -> usize {
    let mut hi = 1;
    while hi < attrs.len() && attrs[hi - 1].sym < sym {
        hi *= 2;
    }
    let lo = hi / 2;
    let hi = hi.min(attrs.len());
    lo + attrs[lo..hi].partition_point(|a| a.sym < sym)
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
    parallel::fanout_list(ev, l);
    let mut entries: Vec<(Sym, Pos, Word)> = Vec::with_capacity(l.len as usize);
    let mut seen: FxHashSet<Sym> = FxHashSet::default();
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
        let sym = ev.force_sym(
            nv,
            n.pos,
            "while evaluating the `name` attribute of an element of the list passed to builtins.listToAttrs",
        )?;
        if !seen.insert(sym) {
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
    parallel::fanout_list(ev, l);
    // Every set is sorted by symbol, so a stable sort of all the pairs
    // groups them with each group in list order: one table, where a hash
    // map and a Vec per group were the builtin's allocations.
    let mut pairs: Vec<(Sym, Word)> = Vec::new();
    for c in l.cells() {
        let w = c.get();
        let a = ev.force_attrs(
            w,
            Pos::NONE,
            "while evaluating a value of the list passed as second argument to builtins.zipAttrsWith",
        )?;
        pairs.reserve(a.len_bounds().1);
        pairs.extend(a.iter().map(|attr| (attr.sym, attr.cell.get())));
    }
    pairs.sort_by_key(|p| p.0);
    let mut out: Vec<(Sym, Pos, Word)> = Vec::new();
    let mut vals: Vec<Word> = Vec::new();
    for group in pairs.chunk_by(|x, y| x.0 == y.0) {
        let sym = group[0].0;
        vals.clear();
        vals.extend(group.iter().map(|p| p.1));
        let name = ev.sym_string(sym);
        let list = ev.mk_list(&vals);
        let v = ev.arena.mk_thunk_app2(f, name, list);
        out.push((sym, Pos::NONE, v));
    }
    Ok(ev.arena.mk_attrs_sorted_as(&out, Stat::AttrsZip))
}

fn cat_attrs(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let sym = ev.force_sym(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.catAttrs",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.catAttrs",
    )?;
    parallel::fanout_list(ev, l);
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
    parallel::fanout_list(ev, l);
    let mut groups: FxHashMap<Sym, Vec<Word>> = FxHashMap::default();
    for c in l.cells() {
        let elem = c.get();
        let r = ev.call1(f, elem, pos)?;
        let sym = ev.force_sym(
            r,
            pos,
            "while evaluating the return value of the grouping function passed to builtins.groupBy",
        )?;
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
