//! Parallel evaluation: fibers, work-stealing scheduler, thunk claim
//! protocol. Design: docs/explanation/evaluation.md.

pub mod deque;
pub mod fiber;
pub mod force;
pub mod interner;
pub mod sched;
pub mod shared;
pub mod thunk;

use std::sync::Mutex;
use std::{ptr, thread};

use crate::eval::Evaluator;
use crate::runtime::Arena;
use crate::session::Session;

pub use force::{
    defer, fanout_attrs_coerce, fanout_list, fanout_list_coerce, force_deep, force_thunk,
    import_thunk, journal,
};
use std::sync::atomic::Ordering::Relaxed;

/// Per-fiber evaluator state. `on` is false on the sequential path.
#[derive(Clone)]
pub struct Ctx {
    pub on: bool,
    /// Claimer id of the fiber running this evaluator.
    pub id: u32,
    pub is_main: bool,
    pub cancelled: bool,
    /// Lowest stack address this fiber may use. Zero on the sequential
    /// path, which runs on the thread stack and never checks.
    pub stack_lo: usize,
    /// The outermost thunk this helper claimed in its task. Effects it
    /// produces wait on that thunk's demand.
    pub root: u64,
    pub slot: *mut sched::Slot,
}

impl Default for Ctx {
    fn default() -> Ctx {
        Ctx {
            on: false,
            id: 0,
            is_main: true,
            cancelled: false,
            stack_lo: 0,
            root: 0,
            slot: ptr::null_mut(),
        }
    }
}

/// A cloned evaluator starts with a placeholder arena; the worker swaps
/// its own in on every resume, so this one is never allocated from. It
/// reserves nothing: a prewarmed string table here cost 210 KB for every
/// parked fiber.
impl Clone for Arena {
    fn clone(&self) -> Arena {
        Arena::placeholder()
    }
}

/// Number of workers from `--workers`, `IETS_WORKERS`, or the machine.
pub fn default_workers() -> usize {
    if let Some(n) = Session::current().settings().workers {
        return n;
    }
    // the critical chain, not core count, sets the floor; past 4 workers the
    // idle spin costs more wall time than the extra stealing gains
    thread::available_parallelism().map_or(1, |n| n.get().min(4))
}

fn stack_size() -> usize {
    Session::current()
        .settings()
        .fiber_stack_mb
        .map_or(fiber::DEFAULT_STACK, |mb| mb << 20)
}

/// Run `f` with `ev` as the main fiber on `workers` threads. With one
/// worker this is a plain call. The error is the fiber stacks: the
/// reservation is over the cap, or the kernel refused the mapping.
pub fn try_run<R: Send + 'static>(
    workers: usize,
    mut ev: Evaluator,
    f: impl FnOnce(&mut Evaluator) -> R + Send + 'static,
) -> Result<R, fiber::StackError> {
    if workers <= 1 {
        let r = f(&mut ev);
        ev.lints.flush();
        return Ok(r);
    }
    let template = force::SyncEv(ev.clone());
    let rt = sched::Runtime::start(workers, Box::new(template), stack_size())?;
    let user = Box::into_raw(Box::new(ev)) as *mut ();
    let out: &'static Mutex<Option<R>> = Box::leak(Box::new(Mutex::new(None)));
    rt.run_main(
        user,
        Box::new(move |slot: &mut sched::Slot| {
            // The main fiber reaches its slot through this pointer only, so
            // no reference to the slot is live while it evaluates.
            let slot: *mut sched::Slot = slot;
            // SAFETY: the main fiber owns this slot for the whole entry, and
            // holds no other reference to it or to its evaluator.
            let ev = unsafe { force::ev_of(slot) };
            // SAFETY: as above.
            unsafe { force::init_ctx(ev, slot) };
            let r = f(ev);
            ev.lints.flush();
            if ev.session().settings().par_stats >= 2 {
                for i in 0..ev.files.len() {
                    errln!(
                        "file{i} base={} = {}",
                        ev.files[i].base,
                        String::from_utf8_lossy(&ev.files[i].path)
                    );
                }
            }
            if ev.session().settings().par_stats >= 1 {
                // SAFETY: the main fiber still owns the slot; evaluation is
                // over, so nothing else reads it.
                let slot = unsafe { &*slot };
                let rt = slot.runtime();
                errln!(
                    "parallel: fibers={} tasks={} effect-bails={} steals={} parks={} fiber-parks={} main-parks={} main-wait={}ms busy={}ms deadlocks={} main-stack-resident={}K",
                    rt.stats_fibers(),
                    rt.stat_tasks(),
                    force::EFFECT_BAILS.load(Relaxed),
                    rt.stat_steals.load(Relaxed),
                    rt.stat_parks.load(Relaxed),
                    rt.stat_fiber_parks.load(Relaxed),
                    rt.stat_main_parks.load(Relaxed),
                    rt.stat_main_wait_ns.load(Relaxed) / 1_000_000,
                    rt.stat_busy_ns() / 1_000_000,
                    rt.deadlocks.load(Relaxed),
                    slot.fiber.stack.resident() >> 10
                );
            }
            *out.lock().unwrap() = Some(r);
        }),
    )?;
    Ok(out
        .lock()
        .unwrap()
        .take()
        .expect("main fiber returned a result"))
}
