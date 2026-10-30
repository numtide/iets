//! What a fault on a known address means: a fiber ran into its guard page,
//! or a value of a released session was used. The fiber stacks register
//! their guards here; the heap registers its tombstones in `heap`.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::fault::{self, Fault};
use super::{heap, vm};

/// Base addresses of the fiber stacks, for the SIGSEGV handler. A fixed
/// table because the handler may not lock; more live fibers than this lose
/// only the message, not the exit code. A freed stack frees its entry.
pub const MAX_GUARDS: usize = 8192;
static GUARDS: [AtomicUsize; MAX_GUARDS] = [const { AtomicUsize::new(0) }; MAX_GUARDS];
static NGUARDS: AtomicUsize = AtomicUsize::new(0);

pub fn register_guard(base: usize) {
    let n = NGUARDS.load(Ordering::Acquire).min(MAX_GUARDS);
    for g in &GUARDS[..n] {
        if g.compare_exchange(0, base, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
    }
    let i = NGUARDS.fetch_add(1, Ordering::AcqRel);
    if i < MAX_GUARDS {
        GUARDS[i].store(base, Ordering::Release);
    }
}

pub fn unregister_guard(base: usize) {
    let n = NGUARDS.load(Ordering::Acquire).min(MAX_GUARDS);
    if let Some(g) = GUARDS[..n]
        .iter()
        .find(|g| g.load(Ordering::Acquire) == base)
    {
        g.store(0, Ordering::Release);
    }
}

pub fn is_guard(addr: usize) -> bool {
    let n = NGUARDS.load(Ordering::Acquire).min(MAX_GUARDS);
    (0..n).any(|i| {
        let b = GUARDS[i].load(Ordering::Acquire);
        b != 0 && addr >= b && addr < b + vm::fault_span()
    })
}

const OVERFLOW_MSG: &[u8] = b"error: stack overflow (possible infinite recursion)\n";
const RELEASED_MSG: &[u8] = b"error: a value was used after its session was released\n";

/// The fault handler without an alternate stack, for a thread that runs no
/// fiber: a released session's memory faults on any thread. It replaces
/// the handler Rust's runtime installs, as `install_overflow_handler` does.
pub fn install_fault_handler() {
    fault::install(classify);
}

fn classify(addr: usize) -> Option<Fault> {
    if is_guard(addr) {
        return Some(Fault {
            message: OVERFLOW_MSG,
            abort: false,
        });
    }
    heap::is_released(addr).then_some(Fault {
        message: RELEASED_MSG,
        abort: true,
    })
}

/// Install the guard-page handler on this thread, which also needs the
/// alternate stack: `sigaltstack` is per thread.
pub fn install_overflow_handler() {
    fault::alt_stack();
    install_fault_handler();
}
