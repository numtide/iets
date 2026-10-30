//! Claim / park / publish protocol on a thunk's result `Cell`, and the
//! side table of parked fibers. See docs/explanation/evaluation.md "The claim
//! protocol".

use std::collections::HashMap;
use std::hint;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::fiber;
use super::sched::{Runtime, Slot};
use crate::runtime::{Cell, Word};

const SHARDS: usize = 64;
const SPIN: u32 = 256;
/// Set in a BLACKHOLE word when the waiter table may hold fibers for it.
pub const WAITERS_BIT: u64 = 0x80;

#[inline]
pub fn blackhole(id: u32) -> Word {
    Word::blackhole_claimed(id)
}

#[inline]
pub fn claimer(w: Word) -> u32 {
    w.blackhole_claimer()
}

#[inline]
fn atomic(cell: &Cell) -> &AtomicU64 {
    cell.atomic()
}

pub enum Claim {
    /// We own the thunk and must publish or reset it; carries the payload
    /// word the state cell held.
    Won(Word),
    /// The state word of a forced thunk.
    Value(Word),
    /// Claimed by this fiber already: infinite recursion.
    Mine,
    /// Claimed by another fiber; the word as observed.
    Busy(Word),
}

/// `unforced` says whether a state word is still the payload (thunk not
/// forced), as opposed to the forced state; see `Word::thunk_unforced`.
#[inline]
pub fn try_claim(cell: &Cell, me: u32, unforced: impl Fn(Word) -> bool) -> Claim {
    // Most forces hit a thunk another fiber already evaluated. A plain
    // load answers those without the CAS, which would pull the line into
    // exclusive state on every reader of a hot shared value.
    let mut cur = atomic(cell).load(Ordering::Acquire);
    loop {
        // SAFETY: `cur` is a word another fiber stored in this cell.
        let w = unsafe { Word::from_bits(cur) };
        if w.is_blackhole() {
            return if claimer(w) == me {
                Claim::Mine
            } else {
                Claim::Busy(w)
            };
        }
        if !unforced(w) {
            return Claim::Value(w);
        }
        match atomic(cell).compare_exchange(
            cur,
            blackhole(me).bits(),
            Ordering::Acquire,
            Ordering::Acquire,
        ) {
            Ok(_) => return Claim::Won(w),
            Err(now) => cur = now,
        }
    }
}

/// Store the new state word (the forced state, or the payload again to
/// reset) and wake every parked waiter.
pub fn publish(slot: &mut Slot, cell: &Cell, v: Word) {
    let old = atomic(cell).swap(v.bits(), Ordering::AcqRel);
    if old & WAITERS_BIT != 0 {
        let rt = slot.runtime();
        let list = rt.waiters.take(cell);
        let w = slot.worker();
        for f in list {
            // SAFETY: entries come from `wait`, whose slots live until the teardown;
            // the demand fiber, if it parked here, is the first of them.
            unsafe { rt.wake(w, f) };
        }
    }
}

/// The fiber was cancelled while parked (deadlock resolution).
#[derive(Debug)]
pub struct Cancelled;

/// Park until the cell leaves the claimed state `cur`.
pub fn wait(slot: &mut Slot, cell: &Cell, cur: Word) -> Result<(), Cancelled> {
    let base = cur.bits() & !WAITERS_BIT;
    for _ in 0..SPIN {
        if cell.get().bits() & !WAITERS_BIT != base {
            return Ok(());
        }
        hint::spin_loop();
    }
    let me = slot as *mut Slot;
    let rt: &Runtime = slot.runtime();
    let shard = rt.waiters.shard(cell);
    {
        let mut g = shard.lock().unwrap();
        let now = cell.get();
        if now.bits() & !WAITERS_BIT != base {
            return Ok(());
        }
        // PARKING before the entry is visible, so a waker that finds the
        // entry always sees PARKING or a later state.
        slot.fiber.state.store(fiber::PARKING, Ordering::Release);
        if atomic(cell)
            .compare_exchange(
                now.bits(),
                now.bits() | WAITERS_BIT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            slot.fiber.state.store(fiber::RUNNING, Ordering::Release);
            return Ok(());
        }
        // the demand fiber parks at the front: it decides the result, so
        // `publish` wakes it first and needs no scan of the list
        let list = g.entry(cell as *const Cell as usize).or_default();
        if slot.is_main {
            list.insert(0, me);
        } else {
            list.push(me);
        }
    }
    rt.stat_fiber_parks.fetch_add(1, Ordering::Relaxed);
    let t0 = slot.is_main.then(Instant::now);
    // SAFETY: we are on this slot's fiber stack.
    unsafe { slot.park() };
    if let Some(t0) = t0 {
        rt.stat_main_parks.fetch_add(1, Ordering::Relaxed);
        rt.stat_main_wait_ns
            .fetch_add(t0.elapsed().as_nanos() as usize, Ordering::Relaxed);
    }
    if slot.cancel.load(Ordering::Acquire) {
        return Err(Cancelled);
    }
    Ok(())
}

pub struct WaiterTable {
    shards: Vec<Mutex<HashMap<usize, Vec<*mut Slot>>>>,
}

impl Default for WaiterTable {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: slot pointers are opaque handles here; see `Runtime`.
unsafe impl Sync for WaiterTable {}
unsafe impl Send for WaiterTable {}

impl WaiterTable {
    pub fn new() -> WaiterTable {
        WaiterTable {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    #[inline]
    fn shard(&self, cell: &Cell) -> &Mutex<HashMap<usize, Vec<*mut Slot>>> {
        let a = cell as *const Cell as usize;
        &self.shards[(a >> 4) % SHARDS]
    }

    fn take(&self, cell: &Cell) -> Vec<*mut Slot> {
        self.shard(cell)
            .lock()
            .unwrap()
            .remove(&(cell as *const Cell as usize))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parallel::sched::{Driver, Task, Worker};
    use std::sync::atomic::AtomicUsize;
    use std::{ptr, thread};

    /// A synthetic thunk: value = 1 + sum of dependency values.
    struct Node {
        cell: Cell,
        deps: Vec<usize>,
    }

    struct Graph {
        nodes: Vec<Node>,
        done: AtomicUsize,
        parks: AtomicUsize,
        deadlocks: AtomicUsize,
        fibers: AtomicUsize,
    }

    // SAFETY: cells are atomic; deps are immutable.
    unsafe impl Sync for Graph {}

    struct GraphDriver(&'static Graph);

    fn force(g: &Graph, slot: &mut Slot, i: usize) -> i64 {
        let cell = &g.nodes[i].cell;
        loop {
            match try_claim(cell, slot.id, |w| w == Word::EMPTY) {
                Claim::Won(_) => {
                    // Some work per node so claims overlap and fibers park.
                    let mut x = i as u64;
                    for _ in 0..3000 {
                        x = x
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                    }
                    hint::black_box(x);
                    let mut v = 1;
                    for &d in &g.nodes[i].deps {
                        v += force(g, slot, d);
                    }
                    publish(slot, cell, Word::imm_int(v));
                    return v;
                }
                Claim::Value(w) => return w.imm_int_value(),
                Claim::Mine => panic!("cycle"),
                Claim::Busy(cur) => {
                    g.parks.fetch_add(1, Ordering::Relaxed);
                    wait(slot, cell, cur).expect("not cancelled");
                }
            }
        }
    }

    impl Driver for GraphDriver {
        fn new_user(&self, _rt: &Runtime) -> *mut () {
            ptr::null_mut()
        }
        fn run_task(&self, _rt: &Runtime, slot: &mut Slot) {
            let t = slot.task;
            for i in t.start..t.start + t.len {
                force(self.0, slot, i as usize);
            }
            self.0.done.fetch_add(1, Ordering::Release);
        }
        fn on_switch(&self, _slot: &mut Slot, _w: &mut Worker) {}
    }

    /// Two cells; task A claims cell 0 then waits on cell 1, task B the
    /// other way round. Both park; deadlock resolution cancels them.
    struct Pair {
        cells: [Cell; 2],
        cancelled: AtomicUsize,
        done: AtomicUsize,
        deadlocks: AtomicUsize,
    }

    // SAFETY: cells are atomic.
    unsafe impl Sync for Pair {}

    struct PairDriver(&'static Pair);

    impl Driver for PairDriver {
        fn new_user(&self, _rt: &Runtime) -> *mut () {
            ptr::null_mut()
        }
        fn run_task(&self, _rt: &Runtime, slot: &mut Slot) {
            let p = self.0;
            let mine = slot.task.start as usize;
            let other = 1 - mine;
            let Claim::Won(_) = try_claim(&p.cells[mine], slot.id, |w| w == Word::EMPTY) else {
                panic!("cell already claimed")
            };
            // Let the other task claim its cell before we look at it.
            let t0 = Instant::now();
            while p.cells[other].get() == Word::EMPTY {
                assert!(t0.elapsed().as_secs() < 10, "peer never claimed");
                thread::yield_now();
            }
            loop {
                match try_claim(&p.cells[other], slot.id, |w| w == Word::EMPTY) {
                    Claim::Busy(cur) => {
                        if wait(slot, &p.cells[other], cur).is_err() {
                            p.cancelled.fetch_add(1, Ordering::Relaxed);
                            publish(slot, &p.cells[mine], Word::EMPTY);
                            break;
                        }
                    }
                    Claim::Won(_) => {
                        publish(slot, &p.cells[other], Word::imm_int(1));
                        publish(slot, &p.cells[mine], Word::imm_int(1));
                        break;
                    }
                    Claim::Value(_) => {
                        publish(slot, &p.cells[mine], Word::imm_int(1));
                        break;
                    }
                    Claim::Mine => unreachable!(),
                }
            }
            p.done.fetch_add(1, Ordering::Release);
        }
        fn on_switch(&self, _slot: &mut Slot, _w: &mut Worker) {}
    }

    /// a hang guard, not a speed bound: it turns a deadlock into a failure
    /// and must not fire on a loaded machine.
    const HANG: u64 = 300;

    #[test]
    fn deadlocked_fibers_are_cancelled() {
        let p: &'static Pair = Box::leak(Box::new(Pair {
            cells: [Cell::new(Word::EMPTY), Cell::new(Word::EMPTY)],
            cancelled: AtomicUsize::new(0),
            deadlocks: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
        }));
        let rt = Runtime::start(3, Box::new(PairDriver(p)), 1 << 20).expect("runtime");
        rt.set_eager_wakes();
        let r = rt.run_main(
            ptr::null_mut(),
            Box::new(move |slot: &mut Slot| {
                let rt = slot.runtime();
                for i in 0..2 {
                    let t = Task {
                        kind: 1,
                        obj: Word::EMPTY,
                        start: i,
                        len: 0,
                    };
                    assert!(rt.submit_urgent(slot.worker(), t));
                }
                // Once both helpers hold their claims, main parks on cell 0
                // like a demand fiber would; the cancelled helper resets it
                // and main re-claims.
                let t0 = Instant::now();
                while p.cells.iter().any(|c| c.get() == Word::EMPTY) {
                    assert!(t0.elapsed().as_secs() < HANG, "timeout");
                    thread::yield_now();
                }
                loop {
                    assert!(t0.elapsed().as_secs() < HANG, "timeout");
                    match try_claim(&p.cells[0], slot.id, |w| w == Word::EMPTY) {
                        Claim::Won(_) => {
                            publish(slot, &p.cells[0], Word::imm_int(7));
                            break;
                        }
                        Claim::Value(_) => break,
                        Claim::Busy(cur) => {
                            wait(slot, &p.cells[0], cur).unwrap();
                        }
                        Claim::Mine => unreachable!(),
                    }
                }
                while p.done.load(Ordering::Acquire) < 2 {
                    assert!(t0.elapsed().as_secs() < HANG, "timeout");
                    thread::yield_now();
                }
                p.deadlocks
                    .store(rt.deadlocks.load(Ordering::Relaxed), Ordering::Relaxed);
            }),
        );
        r.expect("main fiber");
        assert!(
            p.deadlocks.load(Ordering::Relaxed) >= 1,
            "resolver did not run"
        );
        assert!(p.cancelled.load(Ordering::Relaxed) >= 1);
        assert!(!p.cells[0].get().is_blackhole());
        assert!(!p.cells[1].get().is_blackhole());
    }

    fn expected(g: &Graph, i: usize, memo: &mut Vec<Option<i64>>) -> i64 {
        if let Some(v) = memo[i] {
            return v;
        }
        let v = 1 + g.nodes[i]
            .deps
            .clone()
            .iter()
            .map(|&d| expected(g, d, memo))
            .sum::<i64>();
        memo[i] = Some(v);
        v
    }

    #[test]
    fn many_fibers_force_one_shared_graph() {
        for _ in 0..10 {
            shared_graph_round();
        }
    }

    fn shared_graph_round() {
        const N: usize = 4000;
        let mut nodes = Vec::with_capacity(N);
        let mut seed = 12345u64;
        for i in 0..N {
            let mut deps = Vec::new();
            // Dependencies point at lower indices: a DAG with heavy sharing
            // of the low nodes, so many fibers collide on the same thunks.
            for _ in 0..3 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                if i > 0 {
                    let d = if seed.is_multiple_of(4) {
                        (seed % 8) as usize % i
                    } else {
                        (seed as usize) % i
                    };
                    deps.push(d);
                }
            }
            nodes.push(Node {
                cell: Cell::new(Word::EMPTY),
                deps,
            });
        }
        let g: &'static Graph = Box::leak(Box::new(Graph {
            nodes,
            done: AtomicUsize::new(0),
            parks: AtomicUsize::new(0),
            deadlocks: AtomicUsize::new(0),
            fibers: AtomicUsize::new(0),
        }));
        let rt = Runtime::start(8, Box::new(GraphDriver(g)), 1 << 20).expect("runtime");
        rt.set_eager_wakes();
        let r = rt.run_main(
            ptr::null_mut(),
            Box::new(move |slot: &mut Slot| {
                let rt = slot.runtime();
                let mut ntasks = 0;
                // Every fiber forces a range of roots, from the top down so
                // the deep shared nodes are contended.
                for start in (0..N as u32).rev().step_by(16) {
                    let t = Task {
                        kind: 1,
                        obj: Word::EMPTY,
                        start: start.saturating_sub(15),
                        len: start - start.saturating_sub(15) + 1,
                    };
                    assert!(rt.submit_urgent(slot.worker(), t));
                    ntasks += 1;
                }
                let t0 = Instant::now();
                while g.done.load(Ordering::Acquire) < ntasks {
                    assert!(t0.elapsed().as_secs() < 30, "timeout");
                    // Main also forces roots, so it parks and migrates.
                    force(g, slot, N - 1);
                    thread::yield_now();
                }
                g.deadlocks
                    .store(rt.deadlocks.load(Ordering::Relaxed), Ordering::Relaxed);
                g.fibers.store(rt.stats_fibers(), Ordering::Relaxed);
            }),
        );
        r.expect("main fiber");
        let mut memo = vec![None; N];
        for i in 0..N {
            let want = expected(g, i, &mut memo);
            let got = g.nodes[i].cell.get();
            assert!(
                !got.is_blackhole() && got != Word::EMPTY,
                "node {i} unresolved"
            );
            assert_eq!(got.imm_int_value(), want, "node {i}");
        }
        assert_eq!(g.deadlocks.load(Ordering::Relaxed), 0);
        eprintln!(
            "parks={} fibers={}",
            g.parks.load(Ordering::Relaxed),
            g.fibers.load(Ordering::Relaxed)
        );
    }
}
