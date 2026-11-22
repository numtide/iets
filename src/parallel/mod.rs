//! Parallel evaluation: fibers, work-stealing scheduler, thunk claim
//! protocol. Design: docs/explanation/evaluation.md.

pub mod deque;
pub mod fiber;
pub mod force;
pub mod interner;
pub mod sched;
pub mod shared;
pub mod thunk;

use std::sync::{Arc, Mutex};
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
    /// How many thunks this helper is forcing. Inside one, an effect waits
    /// on the demand of the innermost (`force::defer`).
    pub frames: u32,
    /// The effects of the thunks this helper is forcing, innermost last.
    pub fx: force::Log,
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
            frames: 0,
            fx: force::Log::default(),
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

/// Workers one evaluation gains from at most. A NixOS system is mostly one
/// chain of dependencies: nixos-plasma was fastest at 8 workers on 8, 16
/// and 32 CPUs, and 16 workers took 22 % longer than 8 on 16 cores.
const EVAL_WORKERS_MAX: usize = 8;

/// The CPUs this process may run on: affinity and cgroup limits count.
pub fn visible_cpus() -> usize {
    thread::available_parallelism().map_or(1, |n| n.get())
}

/// Number of workers for one evaluation: `--workers`, `IETS_WORKERS`, or
/// the visible CPUs, at most `EVAL_WORKERS_MAX` and as many as the fiber
/// stacks fit.
pub fn default_workers() -> usize {
    Session::current()
        .settings()
        .workers
        .unwrap_or_else(|| fitting(visible_cpus().min(EVAL_WORKERS_MAX)))
}

/// Number of workers for many independent jobs (`eval-jobs` in one
/// process): every visible CPU, which was fastest on 4, 8 and 16 CPUs with
/// SMT or without, and level with half of them on 32; as many as the
/// fiber stacks fit.
pub fn default_jobs_workers() -> usize {
    Session::current()
        .settings()
        .workers
        .unwrap_or_else(|| fitting(visible_cpus()))
}

/// `n` workers, or fewer when their stacks would pass the reservation cap:
/// a default must start where a given `--workers` is refused.
fn fitting(n: usize) -> usize {
    n.min(fiber::max_workers(stack_size()))
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
///
/// When it returns, `ev` is dropped and so is everything the run started:
/// the helper threads have exited, and the fibers, their stacks and their
/// evaluators are freed. The heap the evaluation built stays.
pub fn try_run<R: Send + 'static>(
    workers: usize,
    mut ev: Evaluator,
    f: impl FnOnce(&mut Evaluator) -> R + Send + 'static,
) -> Result<R, fiber::StackError> {
    // the input store reads the current session, and the workers inherit it
    // so their arenas map into its heap
    let _entered = ev.session().clone().enter();
    if workers <= 1 {
        let r = f(&mut ev);
        ev.lints.flush();
        return Ok(r);
    }
    // from here this evaluator runs on the main fiber, whose switches start
    // and pause its clock
    ev.clock.pause(&ev.arena);
    let mut template = ev.clone();
    // every fiber's evaluator borrows its worker's interner cache
    template.interner.drop_cache();
    let template = force::SyncEv(template);
    let rt = sched::Runtime::start(workers, Box::new(template), stack_size())?;
    let user = Box::into_raw(Box::new(ev)) as *mut ();
    let out = Arc::new(Mutex::new(None));
    let put = out.clone();
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
            *put.lock().unwrap() = Some(r);
        }),
    )?;
    Ok(out
        .lock()
        .unwrap()
        .take()
        .expect("main fiber returned a result"))
}
