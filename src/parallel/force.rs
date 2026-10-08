//! Evaluator side of the parallel runtime: the driver that gives every
//! fiber an `Evaluator`, the parallel `force_thunk`, strict-demand fan-out,
//! and the range tasks helpers run.

use std::collections::{HashMap, HashSet};
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::sched::{self, Driver, Runtime, Slot, Task, Worker};
use super::thunk::{self, Claim};
use super::{Ctx, fiber};
use crate::eval::{self, Error, ErrorKind, EvalResult, Evaluator, Pos};
use crate::jobs;
use crate::runtime::{AttrsObj, Cell, Kind, ListObj, Value, Word};

pub const MIN_FANOUT: usize = 4;
pub const BATCH: u32 = 16;

/// `IETS_FANOUT` bitmask for experiments: 1 deep, 2 coerce, 4 list builtins.
#[inline]
fn fanout_mask(ev: &Evaluator) -> u32 {
    ev.session().settings().fanout
}

pub const T_LIST_WHNF: u8 = 1;
pub const T_ATTRS_WHNF: u8 = 2;
pub const T_LIST_DEEP: u8 = 3;
pub const T_ATTRS_DEEP: u8 = 4;
/// Coercion-shaped walk: lists element-wise, derivations to `outPath`,
/// other attrsets attribute-wise. What `derivationStrict`, `toJSON` and
/// `concatStringsSep` demand.
pub const T_LIST_COERCE: u8 = 6;
pub const T_ATTRS_COERCE: u8 = 7;
/// `iets eval-jobs` prefetch of a range of attrs (`crate::jobs`).
pub const T_JOB: u8 = 8;
const COERCE_BATCH: u32 = 4;

/// The root evaluator, cloned for every fiber. The `Sync`/`Send` impls
/// below are why it is a type of its own.
pub struct SyncEv(pub Evaluator);
// SAFETY: the template is only read through `Clone`, which copies scalars
// and bumps reference counts on the shared handles.
unsafe impl Sync for SyncEv {}
unsafe impl Send for SyncEv {}

impl Driver for SyncEv {
    fn new_user(&self, _rt: &Runtime) -> *mut () {
        Box::into_raw(Box::new(self.0.clone())) as *mut ()
    }

    fn run_task(&self, _rt: &Runtime, slot: &mut Slot) {
        // Everything below reaches the slot through this pointer, so no
        // reference to it is live across the task (see `slot_mut`).
        let slot: *mut Slot = slot;
        // SAFETY: the fiber about to run owns the slot and its evaluator:
        // the scheduler resumes a slot on one worker at a time, and no
        // other reference to either is live here.
        unsafe {
            let ev = ev_of(slot);
            init_ctx(ev, slot);
            let t = (*slot).task;
            run_task(ev, t);
            ev.fold_counters();
        }
    }

    fn on_switch(&self, slot: &mut Slot, w: &mut Worker) {
        // SAFETY: the fiber is switched out and this worker holds the slot,
        // so nothing else references its evaluator.
        let ev = unsafe { ev_of(slot) };
        mem::swap(&mut ev.arena, &mut w.arena);
        // a fiber parked in the middle of a task keeps its counters until it
        // is resumed, which a stats line printed meanwhile would not see
        ev.fold_counters();
        // the arena stays with the worker, so the run's memory is the sum over
        // workers; on a switch-in `w.arena` is the placeholder and folds zero
        ev.fold_arena(&mut w.arena);
    }
}

/// The evaluator of `slot`, for the fiber that owns it.
///
/// # Safety
/// The caller must own the slot — it runs on that fiber, or the fiber is
/// switched out and this worker holds it — and must hold no other
/// reference to that evaluator.
#[inline]
pub unsafe fn ev_of(slot: *mut Slot) -> &'static mut Evaluator {
    // SAFETY: `new_user` leaked a `Box<Evaluator>` into `user`; the
    // reference is derived from that raw pointer, and the caller
    // guarantees it is the only live one.
    unsafe { &mut *((*slot).user as *mut Evaluator) }
}

/// Bind a slot's evaluator to the fiber that is about to run.
///
/// # Safety
/// `slot` must be the live slot of the fiber that will run `ev`.
pub unsafe fn init_ctx(ev: &mut Evaluator, slot: *mut Slot) {
    // SAFETY: per the contract; only the slot's own fields are read, and
    // the pointer is stored for `slot_mut` to derive borrows from.
    let (id, is_main, stack_lo) =
        unsafe { ((*slot).id, (*slot).is_main, (*slot).fiber.stack.limit()) };
    ev.par = Ctx {
        on: true,
        id,
        is_main,
        cancelled: false,
        stack_lo,
        root: 0,
        slot,
    };
    ev.call_depth = 0;
    ev.try_level = 0;
}

/// Exclusive borrow of the slot this evaluator's fiber runs on.
///
/// # Safety
/// The borrow must not outlive the call it is passed to, and no other borrow of
/// the slot may be live across it. One fiber owns a slot at a time: a single
/// worker resumes it, and `init_ctx` binds it to the evaluator that fiber runs
/// (docs/explanation/evaluation.md).
#[inline]
pub unsafe fn ev_slot(ev: &Evaluator) -> &'static mut Slot {
    // SAFETY: derived from the raw pointer `init_ctx` stored, never from a
    // reference to the slot; exclusivity is the caller's contract.
    unsafe { &mut *ev.par.slot }
}

#[inline]
unsafe fn slot_mut(ev: &Evaluator) -> &'static mut Slot {
    // SAFETY: same contract as `ev_slot`, which the callers below keep by
    // passing the borrow straight into one call.
    unsafe { ev_slot(ev) }
}

/// Shared view of the slot: its atomics and its runtime handle.
#[inline]
fn slot_ref(ev: &Evaluator) -> &'static Slot {
    // SAFETY: the pointer `init_ctx` stored; slots are never freed, and no
    // exclusive borrow of the slot is live inside the callers below.
    unsafe { &*ev.par.slot }
}

fn cancelled() -> Box<Error> {
    Error::boxed(ErrorKind::Cancelled, "cancelled", Pos::NONE)
}

/// The fiber is about to go below the margin above its guard page. A
/// helper drops its task, so the demand path — which has the same stack as
/// the sequential run — decides the result; the demand path reports what
/// Nix reports.
#[cold]
fn stack_exhausted(ev: &mut Evaluator, pos: Pos) -> Box<Error> {
    if ev.par.is_main {
        Error::boxed(
            ErrorKind::StackOverflow,
            "stack overflow (possible infinite recursion)",
            pos,
        )
    } else {
        ev.par.cancelled = true;
        cancelled()
    }
}

/// True when this fiber has less than the margin of stack left.
#[inline]
pub fn stack_low(ev: &Evaluator) -> bool {
    ev.par.stack_lo != 0 && fiber::sp() < ev.par.stack_lo
}

/// True once this helper fiber must stop: cancelled by deadlock resolution.
#[inline]
pub fn bail_requested(ev: &Evaluator) -> bool {
    if ev.par.is_main {
        return false;
    }
    if ev.par.cancelled {
        return true;
    }
    slot_ref(ev).cancel.load(Ordering::Relaxed)
}

/// Position of the thunk the main fiber forced last (diagnostics: the
/// `IETS_PAR_STATS=2` timeline samples it). These three and `EFFECT_BAILS`
/// stay process-wide: they are sampled on the force path, where a session
/// lookup would cost more than the diagnostic is worth, and two sessions
/// would only blur one debug line.
pub static MAIN_POS: AtomicU64 = AtomicU64::new(0);
pub static SAMPLE_POS: AtomicBool = AtomicBool::new(false);
pub static START: OnceLock<Instant> = OnceLock::new();

/// Parallel `Evaluator::force_thunk`: claim, evaluate, publish. A thunk
/// another fiber already forced is answered here and not in the claim
/// protocol, the way the sequential path answers it. A helper leaves the
/// cell boxed, so this is the whole cost of reading a value a helper
/// computed, and every later reader of that cell pays it again.
#[inline]
pub fn force_thunk(ev: &mut Evaluator, w: Word) -> EvalResult<Word> {
    let cell = w.thunk_state_cell();
    // Acquire pairs with the publish that released the value cell.
    // SAFETY: the word a fiber stored in this state cell.
    let st = unsafe { Word::from_bits(cell.atomic().load(Ordering::Acquire)) };
    if !st.is_blackhole() && !w.thunk_unforced(st) {
        if ev.par.is_main && w.thunk_speculated(st) {
            take_speculated(ev, cell);
        }
        return Ok(w.thunk_value(st));
    }
    claim_and_run(ev, w, cell)
}

/// The demand path took a value a helper computed: the mark is upgraded so
/// the printers show the value, and the effects parked on the thunk become
/// the program's.
#[inline]
fn take_speculated(ev: &mut Evaluator, cell: &Cell) {
    cell.set(Word::EMPTY);
    if NPENDING.load(Ordering::Relaxed) != 0 {
        release(ev, cell as *const Cell as u64);
    }
}

#[inline(never)]
fn claim_and_run(ev: &mut Evaluator, w: Word, cell: &'static Cell) -> EvalResult<Word> {
    let me = ev.par.id;
    let key = cell as *const Cell as u64;
    // SAFETY (every `slot_mut` below): this fiber owns its slot, and each
    // borrow goes straight into one call, so no two are ever live at once.
    //
    // `payload` is the state word the cell held (env, arg, or the map
    // word); a reset stores it back.
    let payload = loop {
        // a thunk that already failed for another helper is not worth
        // starting again: the demand path evaluates it and reports. The
        // check is inside the loop so a waiter that the failure woke stops
        // instead of claiming the reset thunk.
        if !ev.par.is_main && sched::any_failed() && slot_ref(ev).runtime().is_failed(key) {
            ev.par.cancelled = true;
            return Err(cancelled());
        }
        match thunk::try_claim(cell, me, |st| w.thunk_unforced(st)) {
            Claim::Won(p) => break p,
            Claim::Value(st) => {
                if ev.par.is_main && w.thunk_speculated(st) {
                    take_speculated(ev, cell);
                }
                return Ok(w.thunk_value(st));
            }
            Claim::Mine => {
                return Err(Error::boxed(
                    ErrorKind::InfiniteRecursion,
                    "infinite recursion encountered",
                    ev.thunk_pos(w),
                ));
            }
            Claim::Busy(cur) => {
                if thunk::wait(unsafe { slot_mut(ev) }, cell, cur).is_err() {
                    ev.par.cancelled = true;
                    return Err(cancelled());
                }
            }
        }
    };
    if SAMPLE_POS.load(Ordering::Relaxed) && ev.par.is_main && w.thunk_kind() == Kind::Thunk {
        let p = eval::ir_pos(w.as_thunk().ir());
        MAIN_POS.store(p.0 as u64, Ordering::Relaxed);
    }
    if bail_requested(ev) {
        ev.par.cancelled = true;
        thunk::publish(unsafe { slot_mut(ev) }, cell, payload);
        return Err(cancelled());
    }
    if stack_low(ev) {
        let e = stack_exhausted(ev, ev.thunk_pos(w));
        thunk::publish(unsafe { slot_mut(ev) }, cell, payload);
        return Err(e);
    }
    // effects below this point belong to the first thunk this helper
    // claimed: the demand path that takes its value takes them too
    let outer = ev.par.root == 0 && !ev.par.is_main;
    if outer {
        ev.par.root = key;
    }
    let r = ev.run_thunk(w, payload);
    if outer {
        ev.par.root = 0;
    }
    match r {
        Ok(v) => {
            debug_assert!(!v.is_thunk());
            if ev.par.cancelled {
                // Bailed (or hit an effect) somewhere below: transient.
                discard(outer, key);
                thunk::publish(unsafe { slot_mut(ev) }, cell, payload);
                Err(cancelled())
            } else {
                let done = w.thunk_finish(v, ev.par.is_main);
                thunk::publish(unsafe { slot_mut(ev) }, cell, done);
                Ok(v)
            }
        }
        Err(e) => {
            if !ev.par.is_main && e.kind != ErrorKind::Cancelled {
                slot_ref(ev).runtime().note_failed(key);
            }
            discard(outer, key);
            thunk::publish(unsafe { slot_mut(ev) }, cell, payload);
            Err(e)
        }
    }
}

/// `trace`/`warn` in a helper fiber: the helper abandons its task instead
/// of printing. Cancel is sticky, so every thunk still open in this fiber
/// is reset and no value built after the effect escapes into a cell; the
/// demand path re-evaluates and prints. Returns false when the caller
/// should print.
#[inline]
pub fn journal(ev: &mut Evaluator, _msg: String) -> bool {
    if ev.par.on && !ev.par.is_main {
        ev.par.cancelled = true;
        EFFECT_BAILS.fetch_add(1, Ordering::Relaxed);
        true
    } else {
        false
    }
}

pub static EFFECT_BAILS: AtomicUsize = AtomicUsize::new(0);

type Deferred = Box<dyn FnOnce(&mut Evaluator) + Send>;

/// Effects a helper produced, by the thunk whose value carries them. The
/// demand path runs them when it takes that value and drops them
/// otherwise, so a warning from work the program never asks for is never
/// seen. Only for an effect the program may lose without losing anything
/// it returns. The counter keeps the demand path off the lock while the
/// map is empty, which is every run without lints.
static PENDING: Mutex<Option<HashMap<u64, Vec<Deferred>>>> = Mutex::new(None);
static NPENDING: AtomicUsize = AtomicUsize::new(0);

/// Run an effect, or hold it back when a helper produced it. The demand
/// path runs it at once.
pub fn defer(ev: &mut Evaluator, f: impl FnOnce(&mut Evaluator) + Send + 'static) {
    if !ev.par.on || ev.par.is_main || ev.par.root == 0 {
        f(ev);
        return;
    }
    PENDING
        .lock()
        .unwrap()
        .get_or_insert_with(Default::default)
        .entry(ev.par.root)
        .or_default()
        .push(Box::new(f));
    NPENDING.fetch_add(1, Ordering::Relaxed);
}

/// The thunk was reset, so nothing carries its effects.
#[inline]
fn discard(outer: bool, key: u64) {
    if outer && NPENDING.load(Ordering::Relaxed) != 0 {
        let held = PENDING
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|m| m.remove(&key));
        NPENDING.fetch_sub(held.map_or(0, |v| v.len()), Ordering::Relaxed);
    }
}

/// The demand path took the value of the thunk at `key`: its effects are
/// now the program's.
#[cold]
fn release(ev: &mut Evaluator, key: u64) {
    let held = {
        let mut g = PENDING.lock().unwrap();
        g.as_mut().and_then(|m| m.remove(&key))
    };
    for f in held.into_iter().flatten() {
        NPENDING.fetch_sub(1, Ordering::Relaxed);
        f(ev);
    }
}

pub enum CellState {
    Ready(Word),
    Unclaimed,
    Busy,
}

/// What a walk found in a cell, with the demand path's unboxing done in
/// the same read of the thunk. A helper leaves the cell alone: its work
/// must stay invisible until the demand path asks for the value. This is
/// the one path that takes a value without `force_thunk`, so the demand
/// path takes the effects held on a helper's thunk here too.
#[inline]
pub fn take_cell(ev: &mut Evaluator, c: &Cell) -> CellState {
    let w = c.get();
    if !w.is_thunk() {
        return CellState::Ready(w);
    }
    let st = w.thunk_state_cell();
    let s = st.get();
    if s.is_blackhole() {
        return CellState::Busy;
    }
    if w.thunk_unforced(s) {
        return CellState::Unclaimed;
    }
    let v = w.thunk_value(s);
    if ev.par.is_main {
        if w.thunk_speculated(s) {
            take_speculated(ev, st);
        }
        c.set(v);
    }
    CellState::Ready(v)
}

#[inline]
fn cell_state(cell: &Cell) -> CellState {
    let w = cell.get();
    if !w.is_thunk() {
        return CellState::Ready(w);
    }
    let st = w.thunk_state_cell().get();
    if st.is_blackhole() {
        CellState::Busy
    } else if w.thunk_unforced(st) {
        CellState::Unclaimed
    } else {
        CellState::Ready(w.thunk_value(st))
    }
}

/// Submit `n` cells of `obj` as range tasks on the urgent lane.
pub fn fanout(ev: &Evaluator, kind: u8, obj: Word, n: usize) {
    fanout_batched(ev, kind, obj, n, BATCH)
}

fn fanout_batched(ev: &Evaluator, kind: u8, obj: Word, n: usize, batch: u32) {
    fanout_range(ev, kind, obj, 0, n, batch)
}

/// Submit cells `lo..hi` of `obj` as range tasks of `batch` cells.
pub fn fanout_range(ev: &Evaluator, kind: u8, obj: Word, lo: usize, hi: usize, batch: u32) {
    if !ev.par.on || hi - lo < MIN_FANOUT {
        return;
    }
    // SAFETY: this fiber owns its slot, and the borrow only serves to reach
    // the worker it runs on; nothing else borrows the slot below.
    let slot = unsafe { slot_mut(ev) };
    let rt = slot.runtime();
    let w = slot.worker();
    let cell = |i: usize| -> &'static Cell {
        match kind {
            T_LIST_WHNF | T_LIST_DEEP | T_LIST_COERCE => &obj.as_list().unwrap().cells()[i],
            _ => &obj.as_attrs().unwrap().attrs()[i].cell,
        }
    };
    let n = hi;
    let mut start = lo as u32;
    while (start as usize) < n {
        let len = batch.min(n as u32 - start);
        // Only ranges with an unclaimed thunk are worth a task.
        let worth = (start..start + len)
            .any(|i| matches!(cell_state(cell(i as usize)), CellState::Unclaimed));
        if worth {
            let t = Task {
                kind,
                obj,
                start,
                len,
            };
            if !rt.submit_urgent(w, t) {
                return;
            }
        }
        start += len;
    }
}

/// Fan out a list a strict builtin is about to walk (elements to WHNF).
#[inline]
pub fn fanout_list(ev: &Evaluator, l: &'static ListObj) {
    if ev.par.on && fanout_mask(ev) & 4 != 0 {
        fanout(ev, T_LIST_WHNF, Word::from_ref(l), l.len as usize);
    }
}

/// A list whose elements are about to be coerced to strings.
#[inline]
pub fn fanout_list_coerce(ev: &Evaluator, l: &'static ListObj) {
    if ev.par.on && fanout_mask(ev) & 2 != 0 {
        fanout_batched(
            ev,
            T_LIST_COERCE,
            Word::from_ref(l),
            l.len as usize,
            COERCE_BATCH,
        );
    }
}

/// Attrs whose values are about to be coerced (`derivationStrict`, JSON).
#[inline]
pub fn fanout_attrs_coerce(ev: &mut Evaluator, a: &'static AttrsObj) {
    if ev.par.on && fanout_mask(ev) & 2 != 0 {
        let a = ev.arena.flat(a);
        fanout_batched(ev, T_ATTRS_COERCE, Word::from_ref(a), a.len(), COERCE_BATCH);
    }
}

/// Helper-side coercion walk of a forced value. Never parks on busy
/// cells; returns false when the task should stop.
fn coerce_walk(ev: &mut Evaluator, v: Word, depth: u32) -> bool {
    if depth > 64 || bail_requested(ev) {
        return depth <= 64;
    }
    match v.view() {
        Value::List(l) => {
            fanout_batched(ev, T_LIST_COERCE, v, l.len as usize, COERCE_BATCH);
            for c in l.cells() {
                if !coerce_step(ev, c, depth) {
                    return false;
                }
            }
            true
        }
        Value::Attrs(a) => {
            if a.get(ev.s.to_string).is_some() {
                return true;
            }
            if let Some(o) = a.get(ev.s.out_path) {
                return coerce_step(ev, &o.cell, depth);
            }
            let a = ev.arena.flat(a);
            fanout_batched(ev, T_ATTRS_COERCE, Word::from_ref(a), a.len(), COERCE_BATCH);
            for attr in a.attrs() {
                if !coerce_step(ev, &attr.cell, depth) {
                    return false;
                }
            }
            true
        }
        _ => true,
    }
}

fn coerce_step(ev: &mut Evaluator, c: &Cell, depth: u32) -> bool {
    let v = match take_cell(ev, c) {
        CellState::Busy => return true,
        CellState::Ready(v) => v,
        CellState::Unclaimed => match ev.force_cell(c) {
            Ok(v) => v,
            Err(_) => return false,
        },
    };
    coerce_walk(ev, v, depth + 1)
}

/// Parallel `Evaluator::force_deep`.
pub fn force_deep(ev: &mut Evaluator, w: Word) -> EvalResult<Word> {
    let mut seen = HashSet::new();
    let w = ev.force(w)?;
    deep_inner(ev, w, &mut seen, true)?;
    Ok(w)
}

/// Walk `w` deeply. `demand`: park on busy cells (main-style walk) after
/// first doing every unclaimed cell; otherwise skip busy cells.
fn deep_inner(
    ev: &mut Evaluator,
    w: Word,
    seen: &mut HashSet<u64>,
    demand: bool,
) -> EvalResult<()> {
    if stack_low(ev) {
        return Err(stack_exhausted(ev, Pos::NONE));
    }
    match w.view() {
        Value::Attrs(a) => {
            if !seen.insert(w.bits()) {
                return Ok(());
            }
            let a = ev.arena.flat(a);
            if fanout_mask(ev) & 1 != 0 {
                fanout(ev, T_ATTRS_DEEP, Word::from_ref(a), a.len());
            }
            let attrs = a.attrs();
            walk(
                ev,
                attrs.len(),
                seen,
                demand,
                |i| &attrs[i].cell,
                |ev, i, e| {
                    let mut e = e;
                    e.add_trace(
                        attrs[i].pos,
                        format!(
                            "while evaluating the attribute '{}'",
                            ev.interner.name_str(attrs[i].sym)
                        ),
                    );
                    e
                },
            )
        }
        Value::List(l) => {
            if !seen.insert(w.bits()) {
                return Ok(());
            }
            if fanout_mask(ev) & 1 != 0 {
                fanout(ev, T_LIST_DEEP, w, l.len as usize);
            }
            let cells = l.cells();
            walk(ev, cells.len(), seen, demand, |i| &cells[i], |_, _, e| e)
        }
        _ => Ok(()),
    }
}

/// Two-pass walk over `n` cells: unclaimed cells first, then (demand
/// only) the cells another fiber was busy with, in index order so that
/// the first error in sequential order is the one reported.
fn walk(
    ev: &mut Evaluator,
    n: usize,
    seen: &mut HashSet<u64>,
    demand: bool,
    cell: impl Fn(usize) -> &'static Cell,
    decorate: impl Fn(&mut Evaluator, usize, Box<Error>) -> Box<Error>,
) -> EvalResult<()> {
    let mut deferred: Vec<usize> = Vec::new();
    let mut first_err: Option<(usize, Box<Error>)> = None;
    for i in 0..n {
        let c = cell(i);
        // a ready cell already gave up its value: forcing it again would
        // walk the claim protocol of a thunk a helper left boxed
        let ready = match take_cell(ev, c) {
            CellState::Busy => {
                if demand {
                    deferred.push(i);
                }
                continue;
            }
            CellState::Ready(v) => Some(v),
            CellState::Unclaimed => None,
        };
        if !demand && bail_requested(ev) {
            return Err(cancelled());
        }
        let r = match ready {
            Some(v) => deep_inner(ev, v, seen, demand),
            None => ev
                .force_cell(c)
                .and_then(|v| deep_inner(ev, v, seen, demand)),
        };
        if let Err(e) = r {
            if demand {
                first_err = Some((i, decorate(ev, i, e)));
                break;
            }
            return Err(e);
        }
    }
    let limit = first_err.as_ref().map_or(n, |(i, _)| *i);
    for i in deferred {
        if i >= limit {
            break;
        }
        let c = cell(i);
        let r = ev
            .force_cell(c)
            .and_then(|v| deep_inner(ev, v, seen, demand));
        if let Err(e) = r {
            return Err(decorate(ev, i, e));
        }
    }
    match first_err {
        Some((_, e)) => Err(e),
        None => Ok(()),
    }
}

fn run_task(ev: &mut Evaluator, t: Task) {
    let obj = t.obj;
    let (start, end) = (t.start as usize, (t.start + t.len) as usize);
    match t.kind {
        T_LIST_WHNF | T_LIST_DEEP => {
            let l = obj.as_list().expect("list task");
            let deep = t.kind == T_LIST_DEEP;
            let mut seen = HashSet::new();
            for c in &l.cells()[start..end] {
                if !range_step(ev, c, deep, &mut seen) {
                    return;
                }
            }
        }
        T_ATTRS_WHNF | T_ATTRS_DEEP => {
            let a = obj.as_attrs().expect("attrs task");
            let deep = t.kind == T_ATTRS_DEEP;
            let mut seen = HashSet::new();
            for attr in &a.attrs()[start..end] {
                if !range_step(ev, &attr.cell, deep, &mut seen) {
                    return;
                }
            }
        }
        T_LIST_COERCE => {
            let l = obj.as_list().expect("list task");
            for c in &l.cells()[start..end] {
                if !coerce_step(ev, c, 0) {
                    return;
                }
            }
        }
        T_ATTRS_COERCE => {
            let a = obj.as_attrs().expect("attrs task");
            for attr in &a.attrs()[start..end] {
                if !coerce_step(ev, &attr.cell, 0) {
                    return;
                }
            }
        }
        T_JOB => {
            let a = obj.as_attrs().expect("attrs task");
            jobs::prefetch_range(ev, a, start, end);
        }
        k => panic!("unknown task kind {k}"),
    }
}

/// Force one cell of a range task. Skips cells someone else claimed.
/// Returns false when the task should stop.
fn range_step(ev: &mut Evaluator, c: &Cell, deep: bool, seen: &mut HashSet<u64>) -> bool {
    if bail_requested(ev) {
        return false;
    }
    match take_cell(ev, c) {
        CellState::Unclaimed => {}
        CellState::Ready(v) if deep => {
            return deep_inner(ev, v, seen, false).is_ok();
        }
        _ => return true,
    }
    match ev.force_cell(c) {
        Ok(v) => {
            if deep {
                deep_inner(ev, v, seen, false).is_ok()
            } else {
                true
            }
        }
        Err(_) => false,
    }
}

/// Lazy import: the cache maps a resolved path to this thunk so that the
/// claim protocol serialises concurrent importers of one file.
pub fn import_thunk(ev: &mut Evaluator, resolved: &[u8], pos: Pos) -> Word {
    let path = ev.mk_string(resolved);
    let posw = Word::imm_int(pos.0 as i64);
    let s = ev.sym(b"importRaw");
    let op = ev.arena.mk_primop("importRaw", 2, import_raw, s);
    let Value::PrimOp(p) = op.view() else {
        unreachable!("mk_primop makes a primop")
    };
    let app = ev.arena.mk_primop_app(p, &[path]);
    ev.arena.mk_thunk_app(app, posw)
}

fn import_raw(ev: &mut Evaluator, args: &[Word], _pos: Pos) -> EvalResult<Word> {
    let resolved = args[0].as_string().expect("path string").bytes;
    let pos = Pos(args[1].imm_int_value() as u32);
    let t0 = SAMPLE_POS.load(Ordering::Relaxed).then(Instant::now);
    let src = ev.read_source(resolved, pos)?;
    let dir = eval::dir_of(resolved).to_vec();
    let ir = ev.compile_file(resolved, src, &dir)?;
    if let Some(t0) = t0 {
        // SAFETY: this fiber owns its slot; the borrow ends with the
        // statement and nothing else borrows the slot here.
        let w = unsafe { slot_mut(ev) }.worker().idx;
        errln!(
            "import t={}ms {}us main={} w{} {}",
            START.get().map_or(0, |s| s.elapsed().as_millis()),
            t0.elapsed().as_micros(),
            ev.par.is_main,
            w,
            String::from_utf8_lossy(resolved)
        );
    }
    let root = ev.root_env;
    ev.eval(root, ir)
}
