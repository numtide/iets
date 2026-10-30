//! Worker threads, per-worker deques, parking, and the fiber wake
//! handshake. Evaluator-agnostic: the `Driver` supplies the per-fiber
//! payload and runs tasks.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};
use std::thread::{self, Builder, JoinHandle};
use std::time::{Duration, Instant};
use std::{hint, process, ptr};

use super::deque::Deque;
use super::fiber::{self, Fiber};
use super::force;
use super::thunk::WaiterTable;
use crate::runtime::{Arena, Word};
use crate::session::{self, Session};
use crate::sys;

pub const READY_CAP: usize = 4096;
pub const TASK_CAP: usize = 4096;
const PREWARM: usize = 4;
/// Pool fibers beyond this many are trimmed when the worker parks.
const TRIM_ABOVE: usize = 16;
const WARM_STACK: usize = 64 << 10;
/// Direct-mapped table of the thunks that failed last. A miss costs one
/// load, so a failure that falls out of it only loses the saving.
const FAILED_SLOTS: usize = 256;
/// True once any thunk failed. A run that fails nothing — nearly every
/// run — never reads the table, and the force path never chases its slot
/// to the runtime to reach it.
static ANY_FAILED: AtomicBool = AtomicBool::new(false);

/// Whether `Runtime::is_failed` can answer anything but false.
#[inline]
pub fn any_failed() -> bool {
    ANY_FAILED.load(Ordering::Relaxed)
}

/// Cells one range task may cover, from the packed `Task`.
const LEN_MAX: u32 = u16::MAX as u32;

/// The three scheduler tunables, read once on the starting thread. A
/// fiber migrates between threads, so the scheduler reads no thread-local
/// (ARCHITECTURE.md, "Changing the evaluator").
struct Tuning {
    /// Scans of 64 polls an idle searcher makes before it parks (~2 ms). A
    /// shorter spin (64 scans) saved no user time once searchers were capped,
    /// and cost 3-5 % wall in futex round trips. `IETS_SPIN` overrides.
    spin: u32,
    /// Queued tasks per awake worker that justify waking one more sleeper.
    /// A fan-out burst of a few tiny tasks is cleared faster by the workers
    /// already awake than by a futex round trip; a deep queue recruits
    /// sleepers one handoff at a time (`IETS_HANDOFF` overrides).
    handoff: usize,
    /// Idle workers allowed to search at once (`IETS_SPINNERS` overrides).
    spinner_cap: usize,
}

impl Tuning {
    fn read(n: usize) -> Tuning {
        let s = Session::current();
        let s = s.settings();
        Tuning {
            spin: s.spin,
            handoff: s.handoff,
            spinner_cap: s.spinners.unwrap_or((n / 4).max(1)),
        }
    }
}

/// Per-worker counters on their own cache line; summed for the stats.
#[repr(align(128))]
#[derive(Default)]
pub struct WorkerStat {
    /// 1 while the worker is inside a fiber.
    pub running: AtomicUsize,
    pub busy_ns: AtomicUsize,
    pub tasks: AtomicUsize,
}

/// A unit of not-yet-started work. `obj` is a `Word` or anything else the
/// driver packs; `start`/`len` address a range inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Task {
    pub kind: u8,
    pub obj: Word,
    pub start: u32,
    pub len: u32,
}

impl Task {
    /// 8 bits of kind, the whole 32 of start, 16 of len. A container holds
    /// at most `u32::MAX` cells and a range is at most one batch, so
    /// nothing is dropped.
    #[inline]
    fn pack(self) -> (u64, u64) {
        debug_assert!(self.len <= LEN_MAX);
        (
            self.obj.bits(),
            self.kind as u64
                | ((self.start as u64) << 8)
                | ((self.len as u64 & LEN_MAX as u64) << 40),
        )
    }

    #[inline]
    fn unpack(a: u64, b: u64) -> Task {
        Task {
            kind: b as u8,
            // SAFETY: `a` is the `bits()` `pack` wrote for this task.
            obj: unsafe { Word::from_bits(a) },
            start: (b >> 8) as u32,
            len: ((b >> 40) & LEN_MAX as u64) as u32,
        }
    }
}

pub trait Driver: Sync {
    /// Payload for a new fiber slot (an `Evaluator`).
    fn new_user(&self, rt: &Runtime) -> *mut ();
    /// Frees a payload `new_user` made, at teardown. Its fiber never runs
    /// again, but may be parked in the middle of a task.
    fn drop_user(&self, _user: *mut ()) {}
    /// Run `slot.task`. Called on the fiber's own stack.
    fn run_task(&self, rt: &Runtime, slot: &mut Slot);
    /// Called on the worker's stack right before switching into `slot`
    /// and right after it switched out (arena swap).
    fn on_switch(&self, slot: &mut Slot, worker: &mut Worker);
}

pub type MainEntry = Box<dyn FnOnce(&mut Slot)>;

pub struct Slot {
    pub fiber: Fiber,
    /// Claimer id: nonzero, unique per slot, stable across migration.
    pub id: u32,
    pub cancel: AtomicBool,
    pub is_main: bool,
    pub user: *mut (),
    pub task: Task,
    main_entry: Option<MainEntry>,
    /// Worker currently running this fiber. Valid while RUNNING.
    pub worker: *mut Worker,
    pub rt: *const Runtime,
    /// Stack pages beyond the warm top were returned to the OS.
    trimmed: bool,
}

pub struct Worker {
    pub idx: usize,
    pub arena: Arena,
    pool: Vec<*mut Slot>,
    spinning: bool,
    rng: u64,
    pub tasks_run: u64,
    pub steals: u64,
    pub parks: u64,
    pub fibers_created: u64,
}

struct Lanes {
    ready: Deque,
    urgent: Deque,
}

#[repr(align(128))]
struct Padded<T>(T);

pub struct Runtime {
    pub n: usize,
    lanes: Vec<Lanes>,
    overflow: Mutex<Vec<*mut Slot>>,
    overflow_len: Padded<AtomicUsize>,
    /// Summaries of queued work so idle workers poll two words instead of
    /// touching every lane's deque lines.
    pending_tasks: Padded<AtomicUsize>,
    pending_ready: Padded<AtomicUsize>,
    /// Per-worker running flag, busy time and task count.
    wstat: Vec<WorkerStat>,
    /// Idle workers searching for work. A push wakes a sleeper only when
    /// this is zero: a searcher will find the work, and when it takes it,
    /// it wakes one more sleeper if work remains (one wake per handoff,
    /// never one per push).
    spinners: Padded<AtomicUsize>,
    sleepers: Padded<AtomicUsize>,
    /// Wake a sleeper for every queued task. For drivers whose fibers
    /// busy-wait on a queued peer instead of parking (tests).
    eager_wakes: AtomicBool,
    epoch: Padded<AtomicU32>,
    pub shutdown: AtomicBool,
    /// Set once a fiber stack could not be reserved (reported once).
    stack_failed: AtomicBool,
    pub main_done: AtomicBool,
    /// Set once the main result exists: helpers stop starting new work.
    pub suppress: AtomicBool,
    next_id: AtomicU32,
    all: Mutex<Vec<*mut Slot>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// Effects held back by `force::defer`, by thunk address. Per runtime,
    /// so a run that ends drops the effects nothing took.
    pub(crate) pending: Mutex<HashMap<u64, Vec<force::Effect>>>,
    driver: Box<dyn Driver>,
    stack_size: usize,
    tune: Tuning,
    pub waiters: WaiterTable,
    /// Thunks whose evaluation failed, for helpers to skip. A failure is
    /// not memoised (Nix evaluates a failed thunk again at every force),
    /// so this table only stops the speculation: every cell of a fan-out
    /// would otherwise start the same failing computation again.
    failed: [AtomicU64; FAILED_SLOTS],
    pub deadlocks: AtomicUsize,
    pub stat_steals: AtomicUsize,
    pub stat_parks: AtomicUsize,
    pub stat_main_parks: AtomicUsize,
    /// Nanoseconds the main fiber spent parked.
    pub stat_main_wait_ns: AtomicUsize,
    pub stat_fiber_parks: AtomicUsize,
}

// SAFETY: raw slot pointers are only dereferenced under the ownership
// rules in docs/explanation/evaluation.md (a fiber is resumed by one thread at a time;
// slot fields other than atomics are touched only by its current owner).
unsafe impl Sync for Runtime {}
unsafe impl Send for Runtime {}
unsafe impl Send for Worker {}

impl Runtime {
    /// Create the runtime and spawn workers `1..n`. The caller's thread is
    /// worker 0 and must call `run_main`. Fails when the fiber stacks of
    /// `n` workers would reserve more address space than the cap allows.
    pub fn start(
        n: usize,
        driver: Box<dyn Driver>,
        stack_size: usize,
    ) -> Result<Handle, fiber::StackError> {
        assert!(n >= 1);
        fiber::check_reservation(n, stack_size)?;
        let rt: &'static Runtime = Box::leak(Box::new(Runtime {
            n,
            lanes: (0..n)
                .map(|_| Lanes {
                    ready: Deque::new(READY_CAP),
                    urgent: Deque::new(TASK_CAP),
                })
                .collect(),
            overflow: Mutex::new(Vec::new()),
            overflow_len: Padded(AtomicUsize::new(0)),
            pending_tasks: Padded(AtomicUsize::new(0)),
            pending_ready: Padded(AtomicUsize::new(0)),
            wstat: (0..n).map(|_| WorkerStat::default()).collect(),
            spinners: Padded(AtomicUsize::new(0)),
            sleepers: Padded(AtomicUsize::new(0)),
            eager_wakes: AtomicBool::new(false),
            epoch: Padded(AtomicU32::new(0)),
            shutdown: AtomicBool::new(false),
            stack_failed: AtomicBool::new(false),
            main_done: AtomicBool::new(false),
            suppress: AtomicBool::new(false),
            next_id: AtomicU32::new(1),
            all: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
            pending: Mutex::new(HashMap::new()),
            driver,
            stack_size,
            tune: Tuning::read(n),
            waiters: WaiterTable::new(),
            failed: [const { AtomicU64::new(0) }; FAILED_SLOTS],
            deadlocks: AtomicUsize::new(0),
            stat_steals: AtomicUsize::new(0),
            stat_parks: AtomicUsize::new(0),
            stat_main_parks: AtomicUsize::new(0),
            stat_main_wait_ns: AtomicUsize::new(0),
            stat_fiber_parks: AtomicUsize::new(0),
        }));
        let mut threads = rt.threads.lock().unwrap();
        for i in 1..n {
            let t = Builder::new()
                .name(format!("iets-w{i}"))
                .stack_size(4 << 20)
                .spawn(session::bind(move || {
                    sys::guard::install_overflow_handler();
                    let mut w = Worker::new(i);
                    rt.prewarm(&mut w);
                    rt.drain_loop(&mut w, || rt.shutdown.load(Ordering::Acquire));
                }))
                .expect("spawn worker");
            threads.push(t);
        }
        drop(threads);
        Ok(Handle(rt))
    }

    /// Run `entry` in the main fiber on this thread (worker 0), draining
    /// other work while it is parked. Returns when `entry` has returned.
    /// Fails only when the main fiber's own stack cannot be reserved.
    fn run_main(&'static self, user: *mut (), entry: MainEntry) -> Result<(), fiber::StackError> {
        if Session::current().settings().par_stats >= 2 {
            force::SAMPLE_POS.store(true, Ordering::Relaxed);
            let _ = force::START.set(Instant::now());
            self.spawn_timeline();
        }
        sys::guard::install_overflow_handler();
        let mut w = Worker::new(0);
        // the demand path keeps the stack it has at one worker
        let slot = match self.new_slot_sized(&mut w, user, fiber::MAIN_STACK) {
            Ok(s) => s,
            Err(e) => {
                self.driver.drop_user(user);
                return Err(e);
            }
        };
        // SAFETY: fresh slot, owned by this thread.
        unsafe {
            (*slot).is_main = true;
            (*slot).main_entry = Some(entry);
            (*slot).fiber.reset(slot_entry, slot as *mut ());
        }
        self.run_slot(&mut w, slot);
        self.drain_loop(&mut w, || self.main_done.load(Ordering::Acquire));
        Ok(())
    }

    /// Stops the helpers and frees every slot: its fiber stack and its
    /// payload. Returns the number of fibers that were parked in the middle
    /// of a task; whatever their frames own on the Rust heap is lost.
    ///
    /// # Safety
    /// The main fiber has returned, or never started, and nothing holds a
    /// slot pointer or a reference to this runtime past this call.
    unsafe fn teardown(&'static self) -> usize {
        for &s in self.all.lock().unwrap().iter() {
            // SAFETY: slots live until the loop below; atomics only.
            unsafe { (*s).cancel.store(true, Ordering::Release) };
        }
        self.shutdown.store(true, Ordering::Release);
        self.wake_all();
        for t in self.threads.lock().unwrap().drain(..) {
            let _ = t.join();
        }
        let mut parked = 0;
        for s in self.all.lock().unwrap().drain(..) {
            // SAFETY: every thread that ran a fiber has been joined, so
            // this thread owns every slot, and no fiber resumes again.
            let slot = unsafe { Box::from_raw(s) };
            if !matches!(
                slot.fiber.state.load(Ordering::Acquire),
                fiber::NEW | fiber::FINISHED
            ) {
                parked += 1;
            }
            if !slot.user.is_null() {
                self.driver.drop_user(slot.user);
            }
        }
        self.pending.lock().unwrap().clear();
        parked
    }

    fn drain_loop(&self, w: &mut Worker, done: impl Fn() -> bool) {
        let mut idle: u32 = 0;
        let scans = self.tune.spin;
        loop {
            if done() {
                return;
            }
            if let Some(s) = self.take_ready(w) {
                idle = 0;
                self.found_work(w);
                self.run_slot(w, s);
                continue;
            }
            if let Some(t) = self.take_task(w) {
                idle = 0;
                self.found_work(w);
                self.run_task(w, t);
                continue;
            }
            idle += 1;
            if idle < scans && self.may_spin(w) {
                // Poll the two summary words until something appears.
                for _ in 0..64 {
                    if self.has_work() {
                        break;
                    }
                    hint::spin_loop();
                }
                continue;
            }
            self.stop_spinning(w);
            idle = 0;
            self.park(w, &done);
        }
    }

    /// Register as a searcher. Worker 0 (it carries the demand fiber) is
    /// never refused; helpers only while fewer than the cap are searching.
    #[inline]
    fn may_spin(&self, w: &mut Worker) -> bool {
        if w.spinning {
            return true;
        }
        if w.idx == 0 {
            self.spinners.0.fetch_add(1, Ordering::SeqCst);
            w.spinning = true;
            return true;
        }
        let cap = self.tune.spinner_cap;
        let cur = self.spinners.0.load(Ordering::Relaxed);
        if cur < cap
            && self
                .spinners
                .0
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
        {
            w.spinning = true;
            return true;
        }
        false
    }

    /// Leave the searcher set without a handoff (nothing was found).
    #[inline]
    fn stop_spinning(&self, w: &mut Worker) {
        if w.spinning {
            w.spinning = false;
            self.spinners.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// A searcher took work. If it was the last searcher and work remains,
    /// wake one sleeper to keep exactly one searcher alive.
    #[inline]
    fn found_work(&self, w: &mut Worker) {
        if w.spinning {
            w.spinning = false;
            if self.spinners.0.fetch_sub(1, Ordering::SeqCst) == 1
                && self.worth_a_wake()
                && self.sleepers.0.load(Ordering::SeqCst) > 0
            {
                self.wake_one();
            }
        }
    }

    fn new_slot(&self, w: &mut Worker, user: *mut ()) -> Result<*mut Slot, fiber::StackError> {
        self.new_slot_sized(w, user, self.stack_size)
    }

    fn new_slot_sized(
        &self,
        w: &mut Worker,
        user: *mut (),
        stack: usize,
    ) -> Result<*mut Slot, fiber::StackError> {
        w.fibers_created += 1;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let slot = Box::into_raw(Box::new(Slot {
            fiber: Fiber::new(stack, slot_entry, ptr::null_mut())?,
            id,
            cancel: AtomicBool::new(false),
            is_main: false,
            user,
            task: Task {
                kind: 0,
                obj: Word::EMPTY,
                start: 0,
                len: 0,
            },
            main_entry: None,
            worker: ptr::null_mut(),
            rt: self,
            trimmed: false,
        }));
        self.all.lock().unwrap().push(slot);
        Ok(slot)
    }

    /// Report the first stack failure. Tasks are re-derivable work, so a
    /// run that cannot grow its fiber pool finishes on the fibers it has.
    fn report_no_stack(&self, e: &fiber::StackError) {
        if !self.stack_failed.swap(true, Ordering::Relaxed) {
            errln!("iets: {e}; continuing without new fibers");
        }
    }

    fn prewarm(&self, w: &mut Worker) {
        for _ in 0..PREWARM {
            let user = self.driver.new_user(self);
            match self.new_slot(w, user) {
                Ok(s) => w.pool.push(s),
                Err(e) => return self.report_no_stack(&e),
            }
        }
    }

    fn run_task(&self, w: &mut Worker, t: Task) {
        let slot = match w.pool.pop() {
            Some(s) => s,
            None => {
                if self.stack_failed.load(Ordering::Relaxed) {
                    return;
                }
                let user = self.driver.new_user(self);
                match self.new_slot(w, user) {
                    Ok(s) => s,
                    Err(e) => return self.report_no_stack(&e),
                }
            }
        };
        w.tasks_run += 1;
        self.wstat[w.idx].tasks.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the slot is idle (in the pool) and owned by this thread.
        unsafe {
            (*slot).task = t;
            (*slot).cancel.store(false, Ordering::Relaxed);
            (*slot).fiber.reset(slot_entry, slot as *mut ());
        }
        self.run_slot(w, slot);
    }

    /// Resume `slot` on this worker. Requires ownership: the slot is NEW,
    /// or this thread took its single ready entry.
    fn run_slot(&self, w: &mut Worker, slot: *mut Slot) {
        // SAFETY: ownership per the wake handshake (docs/explanation/evaluation.md).
        unsafe {
            let s = &mut *slot;
            s.fiber.state.store(fiber::RUNNING, Ordering::Relaxed);
            s.worker = w;
            self.driver.on_switch(s, w);
            let t0 = Instant::now();
            let st = &self.wstat[w.idx];
            st.running.store(1, Ordering::Relaxed);
            s.fiber.resume();
            st.running.store(0, Ordering::Relaxed);
            st.busy_ns
                .fetch_add(t0.elapsed().as_nanos() as usize, Ordering::Relaxed);
            self.driver.on_switch(s, w);
            match s.fiber.state.load(Ordering::Acquire) {
                fiber::FINISHED => {
                    s.worker = ptr::null_mut();
                    w.pool.push(slot);
                }
                // A waker may already have moved PARKING to WOKEN.
                fiber::PARKING | fiber::WOKEN => {
                    if s.fiber.state.swap(fiber::SUSPENDED, Ordering::AcqRel) == fiber::WOKEN {
                        self.push_ready(w, slot);
                    }
                }
                st => panic!("fiber switched out in state {st}"),
            }
        }
    }

    /// Make a parked fiber runnable. Callable from any thread at any time;
    /// a spurious wake just makes the fiber re-check its wait.
    ///
    /// # Safety
    /// `slot` must point at a live slot (slots live until the teardown).
    pub unsafe fn wake(&self, w: &mut Worker, slot: *mut Slot) {
        // SAFETY: per the contract; only the atomic state is touched.
        let st = unsafe { &(*slot).fiber.state };
        loop {
            match st.load(Ordering::Acquire) {
                fiber::SUSPENDED => {
                    if st
                        .compare_exchange(
                            fiber::SUSPENDED,
                            fiber::RUNNABLE,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        self.push_ready(w, slot);
                        return;
                    }
                }
                fiber::PARKING => {
                    if st
                        .compare_exchange(
                            fiber::PARKING,
                            fiber::WOKEN,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    fn push_ready(&self, w: &mut Worker, slot: *mut Slot) {
        self.pending_ready.0.fetch_add(1, Ordering::SeqCst);
        if !self.lanes[w.idx].ready.push(slot as u64, 0) {
            self.overflow.lock().unwrap().push(slot);
            self.overflow_len.0.fetch_add(1, Ordering::Release);
        }
        self.notify();
    }

    fn take_ready(&self, w: &mut Worker) -> Option<*mut Slot> {
        if self.pending_ready.0.load(Ordering::Acquire) == 0 {
            return None;
        }
        let got = self.take_ready_scan(w);
        if got.is_some() {
            self.pending_ready.0.fetch_sub(1, Ordering::SeqCst);
        }
        got
    }

    fn take_ready_scan(&self, w: &mut Worker) -> Option<*mut Slot> {
        if let Some((a, _)) = self.lanes[w.idx].ready.pop() {
            return Some(a as *mut Slot);
        }
        if self.overflow_len.0.load(Ordering::Acquire) > 0 {
            let mut o = self.overflow.lock().unwrap();
            if let Some(s) = o.pop() {
                self.overflow_len.0.fetch_sub(1, Ordering::Release);
                return Some(s);
            }
        }
        let start = w.next_victim();
        for k in 0..self.n {
            let v = (start + k) % self.n;
            if v == w.idx {
                continue;
            }
            if let Some((a, _)) = self.lanes[v].ready.steal() {
                w.steals += 1;
                self.stat_steals.fetch_add(1, Ordering::Relaxed);
                return Some(a as *mut Slot);
            }
        }
        None
    }

    /// Owner push from the worker currently running the caller.
    pub fn submit_urgent(&self, w: &Worker, t: Task) -> bool {
        let (a, b) = t.pack();
        if self.lanes[w.idx].urgent.push(a, b) {
            self.pending_tasks.0.fetch_add(1, Ordering::SeqCst);
            self.notify();
            true
        } else {
            false
        }
    }

    fn take_task(&self, w: &mut Worker) -> Option<Task> {
        if self.suppress.load(Ordering::Relaxed)
            || self.pending_tasks.0.load(Ordering::Acquire) == 0
        {
            return None;
        }
        let got = self.take_task_scan(w);
        if got.is_some() {
            self.pending_tasks.0.fetch_sub(1, Ordering::SeqCst);
        }
        got
    }

    fn take_task_scan(&self, w: &mut Worker) -> Option<Task> {
        if let Some((a, b)) = self.lanes[w.idx].urgent.pop() {
            return Some(Task::unpack(a, b));
        }
        let start = w.next_victim();
        for k in 0..self.n {
            let v = (start + k) % self.n;
            if v == w.idx {
                continue;
            }
            if let Some((a, b)) = self.lanes[v].urgent.steal() {
                w.steals += 1;
                return Some(Task::unpack(a, b));
            }
        }
        None
    }

    #[inline]
    fn has_work(&self) -> bool {
        self.pending_ready.0.load(Ordering::Acquire) > 0
            || (!self.suppress.load(Ordering::Relaxed)
                && self.pending_tasks.0.load(Ordering::Acquire) > 0)
    }

    /// Called after work was published. Wakes one sleeper only when no
    /// searcher is awake to find it; the searcher hands off (`found_work`).
    fn notify(&self) {
        fence(Ordering::SeqCst);
        if self.sleepers.0.load(Ordering::Relaxed) > 0
            && self.spinners.0.load(Ordering::Relaxed) == 0
            && self.worth_a_wake()
        {
            self.wake_one();
        }
    }

    /// A runnable fiber always is. Queued tasks are when the queue is
    /// deeper than `handoff` per awake worker, or when the caller
    /// is the only worker awake (its own fiber may run for a long time).
    #[inline]
    fn worth_a_wake(&self) -> bool {
        if self.pending_ready.0.load(Ordering::Acquire) > 0 {
            return true;
        }
        if self.suppress.load(Ordering::Relaxed) {
            return false;
        }
        let tasks = self.pending_tasks.0.load(Ordering::Acquire);
        if tasks == 0 {
            return false;
        }
        if self.eager_wakes.load(Ordering::Relaxed) {
            return true;
        }
        let awake = self.n - self.sleepers.0.load(Ordering::Relaxed);
        awake <= 1 || tasks > self.tune.handoff * awake
    }

    /// Wakes a sleeper on every push, for tests that need the wake to happen.
    pub fn set_eager_wakes(&self) {
        self.eager_wakes.store(true, Ordering::Relaxed);
    }

    fn wake_one(&self) {
        self.epoch.0.fetch_add(1, Ordering::Release);
        sys::futex_wake(&self.epoch.0, 1);
    }

    pub fn wake_all(&self) {
        self.epoch.0.fetch_add(1, Ordering::Release);
        sys::futex_wake(&self.epoch.0, i32::MAX as u32);
    }

    fn park(&self, w: &mut Worker, done: &impl Fn() -> bool) {
        w.parks += 1;
        self.stat_parks.fetch_add(1, Ordering::Relaxed);
        self.trim_pool(w);
        let s = self.sleepers.0.fetch_add(1, Ordering::SeqCst) + 1;
        let e = self.epoch.0.load(Ordering::Acquire);
        if done() || self.has_work() {
            self.sleepers.0.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        if s == self.n && !self.main_done.load(Ordering::Acquire) {
            // Everyone is asleep and main is not finished: every fiber is
            // parked on another parked fiber. Break the cycle.
            self.resolve_deadlock(w);
            self.sleepers.0.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        sys::futex_wait(&self.epoch.0, e);
        self.sleepers.0.fetch_sub(1, Ordering::SeqCst);
    }

    fn resolve_deadlock(&self, w: &mut Worker) {
        // before the wake-ups: a woken fiber can finish main first
        self.deadlocks.fetch_add(1, Ordering::Relaxed);
        let all = self.all.lock().unwrap();
        let mut woken = 0;
        for &s in all.iter() {
            // SAFETY: slots live until the teardown; only atomics are read here.
            let slot = unsafe { &*s };
            if slot.is_main || slot.fiber.state.load(Ordering::Acquire) != fiber::SUSPENDED {
                continue;
            }
            slot.cancel.store(true, Ordering::Release);
            // SAFETY: `s` comes from `all`.
            unsafe { self.wake(w, s) };
            woken += 1;
        }
        drop(all);
        if woken == 0 {
            if self.has_work() {
                return;
            }
            errln!("iets: internal error: all workers idle with the main fiber parked");
            process::abort();
        }
    }

    /// Give the stack pages of idle pool fibers beyond the prewarm count
    /// back to the OS. Runs only when the worker is about to park.
    fn trim_pool(&self, w: &mut Worker) {
        if !w.parks.is_multiple_of(64) {
            return;
        }
        for &s in w.pool.iter().skip(TRIM_ABOVE) {
            // SAFETY: pool fibers are idle and owned by this worker.
            let slot = unsafe { &mut *s };
            if !slot.trimmed {
                slot.fiber.stack.release(WARM_STACK);
                slot.trimmed = true;
            }
        }
    }

    #[inline]
    pub fn note_failed(&self, key: u64) {
        ANY_FAILED.store(true, Ordering::Relaxed);
        self.failed[(key >> 4) as usize % FAILED_SLOTS].store(key, Ordering::Relaxed);
    }

    /// True while `key` is one of the thunks that failed last.
    #[inline]
    pub fn is_failed(&self, key: u64) -> bool {
        self.failed[(key >> 4) as usize % FAILED_SLOTS].load(Ordering::Relaxed) == key
    }

    pub fn stats_fibers(&self) -> usize {
        self.all.lock().unwrap().len()
    }

    pub fn stat_tasks(&self) -> usize {
        self.wstat
            .iter()
            .map(|s| s.tasks.load(Ordering::Relaxed))
            .sum()
    }

    pub fn stat_busy_ns(&self) -> usize {
        self.wstat
            .iter()
            .map(|s| s.busy_ns.load(Ordering::Relaxed))
            .sum()
    }

    fn stat_running(&self) -> usize {
        self.wstat
            .iter()
            .map(|s| s.running.load(Ordering::Relaxed))
            .sum()
    }

    /// Print, every 20 ms, how many workers are inside fibers, how much
    /// work is queued and whether main is parked.
    fn spawn_timeline(&'static self) {
        let t = thread::spawn(session::bind(move || {
            let t0 = Instant::now();
            let mut last_busy = 0;
            let mut last_tasks = 0;
            loop {
                thread::sleep(Duration::from_millis(20));
                if self.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                let busy = self.stat_busy_ns();
                let tasks = self.stat_tasks();
                let pos = force::MAIN_POS.load(Ordering::Relaxed);
                errln!(
                    "t={:4}ms running={:2} busy-workers={:.1} tasks/20ms={:5} queued={:5} sleepers={:2} main-at=pos{}",
                    t0.elapsed().as_millis(),
                    self.stat_running(),
                    (busy - last_busy) as f64 / 20e6,
                    tasks - last_tasks,
                    self.pending_tasks.0.load(Ordering::Relaxed),
                    self.sleepers.0.load(Ordering::Relaxed),
                    pos,
                );
                last_busy = busy;
                last_tasks = tasks;
            }
        }));
        self.threads.lock().unwrap().push(t);
    }
}

/// The runtime `Runtime::start` made. `run_main` consumes it and frees it.
pub struct Handle(&'static Runtime);

impl Deref for Handle {
    type Target = Runtime;
    fn deref(&self) -> &Runtime {
        self.0
    }
}

impl Handle {
    /// Runs the main fiber, then the teardown: when this returns, every
    /// helper thread has exited and the runtime, its fibers and their
    /// payloads are freed.
    pub fn run_main(self, user: *mut (), entry: MainEntry) -> Result<(), fiber::StackError> {
        let rt = self.0;
        let r = rt.run_main(user, entry);
        // SAFETY: the main fiber returned or never started; `Handle` was the
        // last reference outside the runtime's own threads and slots.
        let parked = unsafe { rt.teardown() };
        if Session::current().settings().par_stats >= 1 && parked > 0 {
            errln!("parallel: teardown dropped {parked} parked fibers");
        }
        // SAFETY: from `Box::leak` in `start`; the teardown joined every
        // thread that held `rt` and freed every slot that pointed at it.
        drop(unsafe { Box::from_raw(rt as *const Runtime as *mut Runtime) });
        r
    }
}

unsafe fn slot_entry(p: *mut ()) {
    // SAFETY: `p` is the slot whose fiber stack we run on.
    let slot = unsafe { &mut *(p as *mut Slot) };
    let rt = unsafe { &*slot.rt };
    if let Some(entry) = slot.main_entry.take() {
        entry(slot);
        rt.main_done.store(true, Ordering::Release);
        rt.suppress.store(true, Ordering::Release);
        rt.shutdown.store(true, Ordering::Release);
        rt.wake_all();
    } else {
        // SAFETY: `trimmed` only describes idle pages; running clears it.
        slot.trimmed = false;
        rt.driver.run_task(rt, slot);
    }
}

impl Slot {
    /// Park the current fiber. The caller has already stored PARKING (or a
    /// waker may have moved it to WOKEN). Returns when resumed.
    ///
    /// # Safety
    /// Must be called on this slot's own fiber stack.
    #[inline(never)]
    pub unsafe fn park(&mut self) {
        debug_assert!(matches!(
            self.fiber.state.load(Ordering::Relaxed),
            fiber::PARKING | fiber::WOKEN
        ));
        unsafe { self.fiber.yield_() };
    }

    /// The worker currently running this fiber.
    #[inline]
    pub fn worker(&mut self) -> &mut Worker {
        // SAFETY: valid while the fiber runs; set by `run_slot`. Exclusive
        // through `&mut self`: the fiber is the worker's only running code.
        unsafe { &mut *self.worker }
    }

    #[inline]
    pub fn runtime(&self) -> &'static Runtime {
        // SAFETY: the teardown frees the runtime after every slot.
        unsafe { &*self.rt }
    }
}

impl Worker {
    fn new(idx: usize) -> Worker {
        Worker {
            idx,
            arena: Arena::new(),
            pool: Vec::new(),
            spinning: false,
            rng: 0x9E37_79B9_7F4A_7C15 ^ (idx as u64 + 1),
            tasks_run: 0,
            steals: 0,
            parks: 0,
            fibers_created: 0,
        }
    }

    #[inline]
    fn next_victim(&mut self) -> usize {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 32) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::sync::atomic::{AtomicPtr, AtomicU64};

    #[rstest]
    #[case(7, 0, 0)]
    #[case(1, 16_777_216, 16)]
    #[case(8, u32::MAX, LEN_MAX)]
    fn a_task_survives_packing_past_the_old_24_bit_start(
        #[case] kind: u8,
        #[case] start: u32,
        #[case] len: u32,
    ) {
        let t = Task {
            kind,
            obj: Word::EMPTY,
            start,
            len,
        };
        let (a, b) = t.pack();
        assert_eq!(Task::unpack(a, b), t);
    }

    struct Counter {
        sum: AtomicU64,
        done: AtomicU64,
        parked: AtomicPtr<Slot>,
    }

    struct TestDriver(&'static Counter);

    const ADD: u8 = 1;
    const SPAWN: u8 = 2;
    const PARK: u8 = 3;
    const WAKE: u8 = 4;

    impl Driver for TestDriver {
        fn new_user(&self, _rt: &Runtime) -> *mut () {
            ptr::null_mut()
        }
        fn run_task(&self, rt: &Runtime, slot: &mut Slot) {
            let c = self.0;
            let t = slot.task;
            match t.kind {
                ADD => {
                    for i in t.start..t.start + t.len {
                        c.sum.fetch_add(i as u64, Ordering::Relaxed);
                    }
                    c.done.fetch_add(1, Ordering::Release);
                }
                SPAWN => {
                    for i in 0..t.len {
                        let sub = Task {
                            kind: ADD,
                            obj: Word::EMPTY,
                            start: i * 16,
                            len: 16,
                        };
                        let w = slot.worker();
                        if !rt.submit_urgent(w, sub) {
                            self.run_task_inline(c, sub);
                        }
                    }
                    c.done.fetch_add(1, Ordering::Release);
                }
                PARK => {
                    slot.fiber.state.store(fiber::PARKING, Ordering::Release);
                    c.parked.store(slot, Ordering::Release);
                    unsafe { slot.park() };
                    assert!(!slot.cancel.load(Ordering::Relaxed));
                    c.sum.fetch_add(1000, Ordering::Relaxed);
                    c.done.fetch_add(1, Ordering::Release);
                }
                WAKE => loop {
                    let p = c.parked.load(Ordering::Acquire);
                    if !p.is_null() {
                        // SAFETY: `p` is a leaked slot.
                        unsafe { rt.wake(slot.worker(), p) };
                        c.done.fetch_add(1, Ordering::Release);
                        return;
                    }
                    hint::spin_loop();
                },
                _ => unreachable!(),
            }
        }
        fn on_switch(&self, _slot: &mut Slot, _w: &mut Worker) {}
    }

    impl TestDriver {
        fn run_task_inline(&self, c: &Counter, t: Task) {
            for i in t.start..t.start + t.len {
                c.sum.fetch_add(i as u64, Ordering::Relaxed);
            }
            c.done.fetch_add(1, Ordering::Release);
        }
    }

    fn counter() -> &'static Counter {
        Box::leak(Box::new(Counter {
            sum: AtomicU64::new(0),
            done: AtomicU64::new(0),
            parked: AtomicPtr::new(ptr::null_mut()),
        }))
    }

    fn wait_done(c: &Counter, n: u64) {
        let t0 = Instant::now();
        while c.done.load(Ordering::Acquire) < n {
            // a hang guard, not a speed bound
            assert!(t0.elapsed().as_secs() < 300, "timeout");
            thread::yield_now();
        }
    }

    #[test]
    fn fan_out_from_main() {
        let c = counter();
        let rt = Runtime::start(4, Box::new(TestDriver(c)), 1 << 20).expect("runtime");
        rt.set_eager_wakes();
        let r = rt.run_main(
            ptr::null_mut(),
            Box::new(move |slot: &mut Slot| {
                let rt = slot.runtime();
                let n = 64;
                for i in 0..n {
                    let t = Task {
                        kind: SPAWN,
                        obj: Word::EMPTY,
                        start: 0,
                        len: 8,
                    };
                    let _ = i;
                    assert!(rt.submit_urgent(slot.worker(), t));
                }
                // 64 spawners, 8 adds each.
                wait_done(c, 64 * 9);
            }),
        );
        r.expect("main fiber");
        let expect: u64 = 64 * (0..128u64).sum::<u64>();
        assert_eq!(c.sum.load(Ordering::Relaxed), expect);
    }

    #[test]
    fn park_and_wake_across_workers() {
        let c = counter();
        let rt = Runtime::start(3, Box::new(TestDriver(c)), 1 << 20).expect("runtime");
        rt.set_eager_wakes();
        let r = rt.run_main(
            ptr::null_mut(),
            Box::new(move |slot: &mut Slot| {
                let rt = slot.runtime();
                let park = Task {
                    kind: PARK,
                    obj: Word::EMPTY,
                    start: 0,
                    len: 0,
                };
                let wake = Task {
                    kind: WAKE,
                    obj: Word::EMPTY,
                    start: 0,
                    len: 0,
                };
                assert!(rt.submit_urgent(slot.worker(), park));
                assert!(rt.submit_urgent(slot.worker(), wake));
                wait_done(c, 2);
            }),
        );
        r.expect("main fiber");
        assert_eq!(c.sum.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn main_parks_and_is_woken_by_helper() {
        let c = counter();
        let rt = Runtime::start(2, Box::new(TestDriver(c)), 1 << 20).expect("runtime");
        rt.set_eager_wakes();
        let r = rt.run_main(
            ptr::null_mut(),
            Box::new(move |slot: &mut Slot| {
                let rt = slot.runtime();
                let wake = Task {
                    kind: WAKE,
                    obj: Word::EMPTY,
                    start: 0,
                    len: 0,
                };
                assert!(rt.submit_urgent(slot.worker(), wake));
                slot.fiber.state.store(fiber::PARKING, Ordering::Release);
                c.parked.store(slot, Ordering::Release);
                unsafe { slot.park() };
                c.sum.fetch_add(7, Ordering::Relaxed);
                wait_done(c, 1);
            }),
        );
        r.expect("main fiber");
        assert_eq!(c.sum.load(Ordering::Relaxed), 7);
    }
}
