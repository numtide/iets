//! Stackful coroutines with mmap'd stacks. A fiber is resumed by a worker
//! thread and yields back to it; a suspended fiber can be resumed from any
//! thread because its whole state lives on its own stack plus `sp`.
//!
//! The switch saves the callee-saved registers on the current stack and
//! swaps stack pointers. Nothing else is saved: the compiler treats the
//! switch as an ordinary call, so caller-saved registers are already dead.

use std::fmt::{self, Display, Formatter};
use std::sync::atomic::{AtomicU8, Ordering};
use std::{error, io, ptr};

use crate::sys::context;
use crate::sys::guard;
use crate::sys::{self, vm};

pub const DEFAULT_STACK: usize = 256 << 20;
/// Stack of the demand fiber and of the process main thread. The demand
/// path must have the same room at every worker count, or a program that
/// works with one worker crashes with four.
pub const MAIN_STACK: usize = 4 << 30;
/// The inaccessible page at the low end of a stack: one page, whatever the
/// platform's page size is.
fn guard() -> usize {
    vm::page_size()
}
/// Bytes a fiber keeps in hand above its guard page. The force path
/// refuses to go below it, so an overflow is reported instead of taken.
pub const MARGIN: usize = 1 << 20;

/// Address space the fiber stacks of one runtime may reserve. The mappings
/// are `MAP_NORESERVE`, so they cost address space and page tables, not
/// memory; the cap turns an absurd `--workers` or `IETS_FIBER_STACK_MB`
/// into an error at startup instead of a failed mmap in the middle of a run.
pub const MAX_RESERVE: usize = 64 << 30;

/// A fiber stack that could not be reserved.
#[derive(Debug)]
pub struct StackError {
    /// Bytes the reservation asked for.
    pub bytes: usize,
    pub why: Why,
}

#[derive(Debug)]
pub enum Why {
    /// More than `MAX_RESERVE`, before anything was mapped.
    OverLimit,
    Mmap(io::Error),
}

impl Display for StackError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot reserve {} MiB of fiber stacks: ",
            self.bytes >> 20
        )?;
        match &self.why {
            Why::OverLimit => write!(
                f,
                "over the {} MiB limit, lower --workers or IETS_FIBER_STACK_MB",
                MAX_RESERVE >> 20
            ),
            Why::Mmap(e) => write!(f, "{e}"),
        }
    }
}

impl error::Error for StackError {}

/// Check the reservation the demand fiber and `workers - 1` helper fibers
/// of `stack_size` bytes make.
pub fn check_reservation(workers: usize, stack_size: usize) -> Result<(), StackError> {
    let bytes = stack_size
        .saturating_mul(workers.saturating_sub(1))
        .saturating_add(MAIN_STACK);
    if bytes > MAX_RESERVE {
        return Err(StackError {
            bytes,
            why: Why::OverLimit,
        });
    }
    Ok(())
}

/// Fiber lifecycle. Transitions are documented in docs/explanation/evaluation.md.
pub const NEW: u8 = 0;
pub const RUNNING: u8 = 1;
pub const PARKING: u8 = 2;
pub const WOKEN: u8 = 3;
pub const SUSPENDED: u8 = 4;
pub const RUNNABLE: u8 = 5;
pub const FINISHED: u8 = 6;

pub struct Stack {
    base: *mut u8,
    size: usize,
}

impl Stack {
    pub fn new(size: usize) -> Result<Stack, StackError> {
        // The low page is made inaccessible as a guard.
        let base = vm::map_stack(size).map_err(|e| StackError {
            bytes: size,
            why: Why::Mmap(e),
        })?;
        // SAFETY: the page is the bottom of the mapping just made.
        unsafe { vm::fence(base, guard()) };
        guard::register_guard(base as usize);
        Ok(Stack { base, size })
    }

    /// Lowest address the fiber may use before it must stop.
    #[inline]
    pub fn limit(&self) -> usize {
        self.base as usize + guard() + MARGIN
    }

    #[inline]
    pub fn top(&self) -> *mut u8 {
        // SAFETY: base + size is one past the mapping, 16-byte aligned.
        unsafe { self.base.add(self.size) }
    }

    /// Return every page except the top `keep` bytes to the OS. Only valid
    /// while the fiber is not running (its frames are garbage).
    pub fn release(&self, keep: usize) {
        // SAFETY: the fiber is idle and the range is a stack from `map_stack`.
        unsafe { vm::trim_stack(self.base, self.size, keep) };
    }

    /// Number of resident bytes, for diagnostics.
    pub fn resident(&self) -> usize {
        sys::resident(self.base, self.size)
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        guard::unregister_guard(self.base as usize);
        // SAFETY: the whole mapping `new` made; the fiber on it never runs
        // again.
        unsafe { vm::unmap(self.base, self.size) };
    }
}

pub type Entry = unsafe fn(*mut ());

pub struct Fiber {
    /// Saved stack pointer of this fiber while suspended.
    sp: *mut u8,
    /// Saved stack pointer of the resumer while this fiber runs.
    caller_sp: *mut u8,
    pub stack: Stack,
    pub state: AtomicU8,
    entry: Entry,
    arg: *mut (),
}

impl Fiber {
    pub fn new(stack_size: usize, entry: Entry, arg: *mut ()) -> Result<Fiber, StackError> {
        Ok(Fiber {
            sp: ptr::null_mut(),
            caller_sp: ptr::null_mut(),
            stack: Stack::new(stack_size)?,
            state: AtomicU8::new(NEW),
            entry,
            arg,
        })
    }

    /// Rewind a NEW or FINISHED fiber so the next resume starts `entry`.
    /// The stack is seeded at resume time, so the fiber may still move
    /// between `reset` and its first `resume`.
    pub fn reset(&mut self, entry: Entry, arg: *mut ()) {
        debug_assert!(matches!(self.state.load(Ordering::Relaxed), NEW | FINISHED));
        self.entry = entry;
        self.arg = arg;
        self.sp = ptr::null_mut();
        self.state.store(NEW, Ordering::Relaxed);
    }

    /// Switch onto the fiber. Returns when it yields or finishes. The
    /// caller must own the fiber (state NEW or RUNNABLE) and must set
    /// `state` to RUNNING first.
    ///
    /// # Safety
    /// Exactly one thread resumes a given fiber at a time.
    #[inline(never)]
    pub unsafe fn resume(&mut self) {
        if self.sp.is_null() {
            // SAFETY: the stack mapping has room for the frame.
            self.sp = unsafe {
                context::seed_stack(
                    self.stack.base.add(vm::STACK_ZONE),
                    self.stack.top(),
                    fiber_main,
                    self as *mut Fiber as *mut (),
                )
            };
        }
        unsafe { context::switch(&mut self.caller_sp, self.sp) };
    }

    /// Switch back to the resumer. Called on the fiber's own stack.
    ///
    /// # Safety
    /// Must be called from code running on this fiber's stack.
    #[inline(never)]
    pub unsafe fn yield_(&mut self) {
        unsafe { context::switch(&mut self.sp, self.caller_sp) };
    }
}

/// First frame of every fiber: runs the entry, marks FINISHED, switches
/// back for good. Never returns.
unsafe extern "C" fn fiber_main(f: *mut ()) -> ! {
    // SAFETY: `f` is the fiber whose stack we are on; it outlives us.
    unsafe {
        let fiber = &mut *f.cast::<Fiber>();
        (fiber.entry)(fiber.arg);
        fiber.state.store(FINISHED, Ordering::Release);
        fiber.yield_();
    }
    unreachable!("finished fiber resumed")
}

/// Stack pointer of the caller's frame, the force path's bound on how far
/// the running fiber has gone. One instruction, inlined: a helper that
/// returns the address of its own local costs a call and a frame slot on
/// every force.
#[inline(always)]
pub fn sp() -> usize {
    context::sp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::guard::{MAX_GUARDS, is_guard};
    use std::sync::atomic::AtomicUsize;
    use std::{hint, slice, thread};

    struct Counter {
        fiber: *mut Fiber,
        n: AtomicUsize,
    }

    unsafe fn count_entry(p: *mut ()) {
        let c = unsafe { &*(p as *const Counter) };
        for _ in 0..3 {
            c.n.fetch_add(1, Ordering::Relaxed);
            unsafe { (*c.fiber).yield_() };
        }
    }

    fn counting(stack: usize, entry: Entry) -> (&'static mut Counter, &'static mut Fiber) {
        let c = Box::leak(Box::new(Counter {
            fiber: ptr::null_mut(),
            n: AtomicUsize::new(0),
        }));
        let f = Box::leak(Box::new(
            Fiber::new(stack, entry, c as *mut Counter as *mut ()).expect("fiber stack"),
        ));
        c.fiber = f;
        (c, f)
    }

    #[test]
    fn resume_yield_finish() {
        let (c, f) = counting(1 << 20, count_entry);
        for i in 1..=3 {
            f.state.store(RUNNING, Ordering::Relaxed);
            unsafe { f.resume() };
            assert_eq!(c.n.load(Ordering::Relaxed), i);
            assert_eq!(f.state.load(Ordering::Relaxed), RUNNING);
        }
        unsafe { f.resume() };
        assert_eq!(f.state.load(Ordering::Relaxed), FINISHED);
        f.reset(count_entry, c as *mut Counter as *mut ());
        f.state.store(RUNNING, Ordering::Relaxed);
        unsafe { f.resume() };
        assert_eq!(c.n.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn migrates_between_threads() {
        let (c, f) = counting(1 << 20, count_entry);
        let fp = f as *mut Fiber as usize;
        for i in 1..=3 {
            thread::spawn(move || {
                let f = unsafe { &mut *(fp as *mut Fiber) };
                f.state.store(RUNNING, Ordering::Relaxed);
                unsafe { f.resume() };
            })
            .join()
            .unwrap();
            assert_eq!(c.n.load(Ordering::Relaxed), i);
        }
    }

    #[test]
    fn reservation_is_capped_and_mmap_failure_is_an_error() {
        check_reservation(4, DEFAULT_STACK).expect("the default fits");
        let e = check_reservation(1024, DEFAULT_STACK).expect_err("over the cap");
        assert_eq!(
            e.to_string(),
            format!(
                "cannot reserve {} MiB of fiber stacks: over the {} MiB limit, \
                 lower --workers or IETS_FIBER_STACK_MB",
                (DEFAULT_STACK * 1023 + MAIN_STACK) >> 20,
                MAX_RESERVE >> 20
            )
        );
        let Err(e) = Stack::new(usize::MAX - 4095) else {
            panic!("the whole address space was mapped");
        };
        assert!(
            matches!(e.why, Why::Mmap(_)) && e.to_string().starts_with("cannot reserve"),
            "unexpected message: {e}"
        );
    }

    #[inline(never)]
    fn deep(n: u64, low: &AtomicUsize) -> u64 {
        let buf = [n; 64];
        let p = hint::black_box(&buf);
        if n == 0 {
            low.store(sp(), Ordering::Relaxed);
            p[3]
        } else {
            deep(n - 1, low) + hint::black_box(p)[5] - n
        }
    }

    unsafe fn deep_entry(p: *mut ()) {
        let c = unsafe { &*(p as *const Counter) };
        hint::black_box(deep(20000, &c.n));
    }

    // Residency is only an upper bound: under memory pressure the kernel
    // may swap the used pages out before they are counted.
    #[test]
    fn large_stack_use_and_release() {
        let keep = 64 << 10;
        let (c, f) = counting(64 << 20, deep_entry);
        f.state.store(RUNNING, Ordering::Relaxed);
        unsafe { f.resume() };
        assert_eq!(f.state.load(Ordering::Relaxed), FINISHED);
        let top = f.stack.top() as usize;
        let low = c.n.load(Ordering::Relaxed) & !(vm::page_size() - 1);
        assert!(top - low > 20000 * 512);
        // SAFETY: the range is in the idle fiber's mapping, above the guard.
        let used = unsafe { slice::from_raw_parts(low as *const u64, (top - keep - low) / 8) };
        assert!(used.iter().any(|w| *w != 0));
        f.stack.release(keep);
        assert!(f.stack.resident() <= keep);
        // Windows decommits the range, so a read there faults.
        #[cfg(unix)]
        assert!(used.iter().all(|w| *w == 0));
    }

    #[test]
    fn a_freed_stack_frees_its_guard_entry() {
        for _ in 0..MAX_GUARDS + 16 {
            drop(Stack::new(1 << 20).expect("stack"));
        }
        let s = Stack::new(1 << 20).expect("stack");
        assert!(is_guard(s.base as usize));
    }
}
