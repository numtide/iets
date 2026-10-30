//! List primops.

use super::Registry;
use crate::eval::{ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::parallel;
use crate::runtime::arena::Stat;
use crate::runtime::{Arena, Value, Word};

pub fn register(r: &mut Registry) {
    r.op("__head", 1, head);
    r.op("__tail", 1, tail);
    r.op("__length", 1, length);
    r.op("__elem", 2, elem);
    r.op("__elemAt", 2, elem_at);
    r.op("map", 2, map);
    r.op("__filter", 2, filter);
    r.op("__foldl'", 3, foldl_strict);
    r.op("__concatLists", 1, concat_lists);
    r.op("__concatMap", 2, concat_map);
    r.op("__genList", 2, gen_list);
    r.op("__any", 2, any);
    r.op("__all", 2, all);
    r.op("__sort", 2, sort);
    r.op("__partition", 2, partition);
}

fn head(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.head",
    )?;
    if l.len == 0 {
        bail!(
            ErrorKind::Eval,
            pos,
            "'builtins.head' called on an empty list"
        );
    }
    ev.force_cell(&l.cells()[0])
}

fn tail(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.tail",
    )?;
    if l.len == 0 {
        bail!(
            ErrorKind::Eval,
            pos,
            "'builtins.tail' called on an empty list"
        );
    }
    let items: Vec<Word> = l.cells()[1..].iter().map(|c| c.get()).collect();
    Ok(ev.mk_list(&items))
}

fn length(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.length",
    )?;
    Ok(Word::imm_int(l.len as i64))
}

fn elem(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.elem",
    )?;
    parallel::fanout_list(ev, l);
    for c in l.cells() {
        let x = ev.force(args[0])?;
        let v = c.get();
        let fv = ev.force_cell(c)?;
        if ev.eq_values(args[0], x, v, fv, pos)? {
            return Ok(Word::TRUE);
        }
    }
    Ok(Word::FALSE)
}

fn elem_at(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.elemAt",
    )?;
    let n = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.elemAt",
    )?;
    if n < 0 || n as usize >= l.len as usize {
        bail!(
            ErrorKind::Eval,
            pos,
            "'builtins.elemAt' called with index {} on a list of size {}",
            n,
            l.len
        );
    }
    ev.force_cell(&l.cells()[n as usize])
}

fn map(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.map",
    )?;
    if l.len == 0 {
        return Ok(Word::from_ref(l));
    }
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.map",
    )?;
    let out = ev.arena.alloc_list_as(l.len as usize, Stat::ListMap);
    for (o, c) in out.cells().iter().zip(l.cells()) {
        let t = ev.arena.mk_thunk_app(f, c.get());
        o.set(t);
    }
    Ok(Word::from_ref(out))
}

fn filter(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.filter",
    )?;
    parallel::fanout_list(ev, l);
    if l.len == 0 {
        return Ok(Word::from_ref(l));
    }
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.filter",
    )?;
    let mut out = Vec::with_capacity(l.len as usize);
    let mut same = true;
    for c in l.cells() {
        let v = c.get();
        let r = ev.call1(f, v, pos)?;
        if ev.force_bool(
            r,
            pos,
            "while evaluating the return value of the filtering function passed to builtins.filter",
        )? {
            out.push(v);
        } else {
            same = false;
        }
    }
    if same {
        return Ok(Word::from_ref(l));
    }
    Ok(ev.mk_list(&out))
}

fn foldl_strict(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.foldlStrict",
    )?;
    let l = ev.force_list(
        args[2],
        pos,
        "while evaluating the third argument passed to builtins.foldlStrict",
    )?;
    parallel::fanout_list(ev, l);
    let mut acc = args[1];
    for c in l.cells() {
        acc = ev.call2(f, acc, c.get(), pos)?;
    }
    ev.force(acc)
}

fn concat_lists(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.concatLists",
    )?;
    parallel::fanout_list(ev, l);
    let mut lists = Vec::with_capacity(l.len as usize);
    for c in l.cells() {
        let v = c.get();
        lists.push(ev.force_list(
            v,
            pos,
            "while evaluating a value of the list passed to builtins.concatLists",
        )?);
    }
    Ok(ev.concat_lists(&lists))
}

fn concat_map(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.concatMap",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.concatMap",
    )?;
    parallel::fanout_list(ev, l);
    let mut lists = Vec::with_capacity(l.len as usize);
    for c in l.cells() {
        let r = ev.call1(f, c.get(), pos)?;
        lists.push(ev.force_list(
            r,
            pos,
            "while evaluating the return value of the function passed to builtins.concatMap",
        )?);
    }
    Ok(ev.concat_lists(&lists))
}

fn gen_list(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let n = ev.force_int(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.genList",
    )?;
    if n < 0 {
        bail!(ErrorKind::Eval, pos, "cannot create list of size {}", n);
    }
    if Arena::list_bytes(n as usize).is_none() {
        bail!(ErrorKind::Eval, pos, "out of memory");
    }
    let f = ev.force_function(
        args[0],
        Pos::NONE,
        "while evaluating the first argument passed to builtins.genList",
    )?;
    let out = ev.arena.alloc_list_as(n as usize, Stat::ListGen);
    for (i, c) in out.cells().iter().enumerate() {
        let t = ev.arena.mk_thunk_app(f, Word::imm_int(i as i64));
        c.set(t);
    }
    Ok(Word::from_ref(out))
}

fn any(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.any",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.any",
    )?;
    parallel::fanout_list(ev, l);
    for c in l.cells() {
        let r = ev.call1(f, c.get(), pos)?;
        if ev.force_bool(
            r,
            pos,
            "while evaluating the return value of the function passed to builtins.any",
        )? {
            return Ok(Word::TRUE);
        }
    }
    Ok(Word::FALSE)
}

fn all(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.all",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.all",
    )?;
    parallel::fanout_list(ev, l);
    for c in l.cells() {
        let r = ev.call1(f, c.get(), pos)?;
        if !ev.force_bool(
            r,
            pos,
            "while evaluating the return value of the function passed to builtins.all",
        )? {
            return Ok(Word::FALSE);
        }
    }
    Ok(Word::TRUE)
}

/// Nix sorts with peeksort (`src/libutil/include/nix/util/sort.hh`), so the
/// order of the comparator's calls is part of the observable behaviour: a
/// comparator that is no strict weak order, or that fails on one pair, must
/// give the same answer here.
fn sort(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.sort",
    )?;
    parallel::fanout_list(ev, l);
    if l.len == 0 {
        return Ok(Word::from_ref(l));
    }
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.sort",
    )?;
    let mut items: Vec<Word> = Vec::with_capacity(l.len as usize);
    for c in l.cells() {
        items.push(ev.force_cell(c)?);
    }
    let is_less_than = matches!(f.view(), Value::PrimOp(p) if p.name == "__lessThan");
    let mut less = |ev: &mut Evaluator, a: Word, b: Word| -> EvalResult<bool> {
        if is_less_than {
            return ev.less_than(a, b, pos);
        }
        let r = ev.call2(f, a, b, pos)?;
        ev.force_bool(
            r,
            pos,
            "while evaluating the return value of the sorting function passed to builtins.sort",
        )
    };
    let mut buf: Vec<Word> = Vec::new();
    peeksort(ev, &mut items, &mut buf, &mut less)?;
    Ok(ev.mk_list(&items))
}

type Less<'a> = &'a mut dyn FnMut(&mut Evaluator, Word, Word) -> EvalResult<bool>;

/// libstdc++'s cut-off, which Nix's peeksort copies.
const INSERTION_SORT_MAX: usize = 16;

fn insertion_sort(ev: &mut Evaluator, v: &mut [Word], less: Less<'_>) -> EvalResult<()> {
    for current in 1..v.len() {
        let mut k = current;
        while k > 0 && less(ev, v[k], v[k - 1])? {
            v.swap(k, k - 1);
            k -= 1;
        }
    }
    Ok(())
}

/// `mergeSortedRunsInPlace` on `v[begin..end]`, split at `middle`.
fn merge_runs(
    ev: &mut Evaluator,
    v: &mut [Word],
    buf: &mut Vec<Word>,
    less: Less<'_>,
    begin: usize,
    middle: usize,
    end: usize,
) -> EvalResult<()> {
    buf.clear();
    buf.extend_from_slice(&v[begin..end]);
    let (split, n) = (middle - begin, end - begin);
    let (mut l, mut r, mut o) = (0, split, begin);
    while l < split && r < n {
        // not-less keeps the sort stable: a tie takes the left element
        if less(ev, buf[r], buf[l])? {
            v[o] = buf[r];
            r += 1;
        } else {
            v[o] = buf[l];
            l += 1;
        }
        o += 1;
    }
    v[o..o + (split - l)].copy_from_slice(&buf[l..split]);
    v[o..o + (n - r)].copy_from_slice(&buf[r..n]);
    Ok(())
}

/// Maximal `i` with `v[begin..i]` strictly decreasing; `rev` negates the
/// comparator, which gives `weaklyIncreasingPrefix`.
fn run_prefix(
    ev: &mut Evaluator,
    v: &[Word],
    less: Less<'_>,
    mut begin: usize,
    end: usize,
    rev: bool,
) -> EvalResult<usize> {
    if begin == end {
        return Ok(begin);
    }
    while begin + 1 != end && less(ev, v[begin + 1], v[begin])? != rev {
        begin += 1;
    }
    Ok(begin + 1)
}

/// Minimal `i` with `v[i..end]` strictly decreasing; `rev` as in `run_prefix`.
fn run_suffix(
    ev: &mut Evaluator,
    v: &[Word],
    less: Less<'_>,
    begin: usize,
    mut end: usize,
    rev: bool,
) -> EvalResult<usize> {
    if begin == end {
        return Ok(end);
    }
    while end - 1 > begin && less(ev, v[end - 1], v[end - 2])? != rev {
        end -= 1;
    }
    Ok(end - 1)
}

fn peeksort(
    ev: &mut Evaluator,
    v: &mut [Word],
    buf: &mut Vec<Word>,
    less: Less<'_>,
) -> EvalResult<()> {
    let n = v.len();
    match n {
        0 | 1 => return Ok(()),
        2 => {
            if less(ev, v[1], v[0])? {
                v.swap(0, 1);
            }
            return Ok(());
        }
        _ => {}
    }
    buf.reserve(n);
    peeksort_impl(ev, v, buf, less, 0, n, 0, n)
}

/// Sorts `v[begin..end]`, given that `v[begin..left_run_end]` and
/// `v[right_run_begin..end]` are sorted.
#[allow(clippy::too_many_arguments)]
fn peeksort_impl(
    ev: &mut Evaluator,
    v: &mut [Word],
    buf: &mut Vec<Word>,
    less: Less<'_>,
    begin: usize,
    end: usize,
    left_run_end: usize,
    right_run_begin: usize,
) -> EvalResult<()> {
    if left_run_end == end || right_run_begin == begin {
        return Ok(());
    }
    let length = end - begin;
    if length <= INSERTION_SORT_MAX {
        return insertion_sort(ev, &mut v[begin..end], less);
    }
    let middle = begin + length / 2;
    if middle <= left_run_end {
        peeksort_impl(
            ev,
            v,
            buf,
            less,
            left_run_end,
            end,
            left_run_end + 1,
            right_run_begin,
        )?;
        return merge_runs(ev, v, buf, less, begin, left_run_end, end);
    }
    if middle >= right_run_begin {
        peeksort_impl(
            ev,
            v,
            buf,
            less,
            begin,
            right_run_begin,
            left_run_end,
            right_run_begin - 1,
        )?;
        return merge_runs(ev, v, buf, less, begin, right_run_begin, end);
    }
    // the run that holds middle - 1
    let up = !less(ev, v[middle], v[middle - 1])?;
    let i = run_suffix(ev, v, less, left_run_end, middle, up)?;
    let j = run_prefix(ev, v, less, middle - 1, right_run_begin, up)?;
    if !up {
        v[i..j].reverse();
    }
    if i == begin && j == end {
        return Ok(());
    }
    if middle - i < j - middle {
        peeksort_impl(ev, v, buf, less, begin, i, left_run_end, i.wrapping_sub(1))?;
        peeksort_impl(ev, v, buf, less, i, end, j, right_run_begin)?;
        merge_runs(ev, v, buf, less, begin, i, end)
    } else {
        peeksort_impl(ev, v, buf, less, begin, j, left_run_end, i)?;
        peeksort_impl(ev, v, buf, less, j, end, j + 1, right_run_begin)?;
        merge_runs(ev, v, buf, less, begin, j, end)
    }
}

fn partition(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.partition",
    )?;
    let l = ev.force_list(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.partition",
    )?;
    parallel::fanout_list(ev, l);
    let mut right = Vec::new();
    let mut wrong = Vec::new();
    for c in l.cells() {
        let v = ev.force_cell(c)?;
        let r = ev.call1(f, v, pos)?;
        if ev.force_bool(r, pos, "while evaluating the return value of the partition function passed to builtins.partition")? {
            right.push(v);
        } else {
            wrong.push(v);
        }
    }
    let r = ev.mk_list(&right);
    let w = ev.mk_list(&wrong);
    let mut entries = vec![(ev.s.right, Pos::NONE, r), (ev.s.wrong, Pos::NONE, w)];
    ev.mk_attrs(&mut entries, pos)
}
