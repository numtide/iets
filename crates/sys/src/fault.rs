//! Turning a fault on a known address into a message and an exit.

use std::cell::RefCell;
use std::sync::{Once, OnceLock};
use std::{mem, ptr};

pub use super::Fault;

static CLASSIFY: OnceLock<fn(usize) -> Option<Fault>> = OnceLock::new();

/// Replaces the runtime's fault handler. `classify` gets the faulting
/// address and runs inside the handler, so it must not lock or allocate.
/// A fault it declines gets the default disposition.
pub fn install(classify: fn(usize) -> Option<Fault>) {
    static ONCE: Once = Once::new();
    let _ = CLASSIFY.set(classify);
    ONCE.call_once(set_handler);
}

fn set_handler() {
    // SAFETY: the documented use of `sigaction`, with a handler that only
    // writes to fd 2 and ends the process.
    unsafe {
        for sig in [libc::SIGSEGV, libc::SIGBUS] {
            let mut sa: libc::sigaction = mem::zeroed();
            sa.sa_sigaction = on_fault as *const () as usize;
            sa.sa_flags = libc::SA_ONSTACK | libc::SA_SIGINFO;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, ptr::null_mut());
        }
    }
}

extern "C" fn on_fault(sig: i32, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo for SIGSEGV/SIGBUS.
    let addr = unsafe { (*info).si_addr() } as usize;
    if let Some(f) = CLASSIFY.get().and_then(|c| c(addr)) {
        // SAFETY: write, _exit and abort are async-signal-safe.
        unsafe {
            libc::write(2, f.message.as_ptr() as *const _, f.message.len());
            if f.abort {
                libc::abort();
            }
            libc::_exit(1);
        }
    }
    // not ours: let the default disposition produce the usual crash
    // SAFETY: restoring the default handler and re-raising is the standard
    // way to report a fault we do not own.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
    }
}

/// This thread's alternate signal stack. The destructor turns `sigaltstack`
/// off before the memory goes.
struct AltStack {
    _mem: Box<[u8]>,
}

impl Drop for AltStack {
    fn drop(&mut self) {
        let ss = libc::stack_t {
            ss_sp: ptr::null_mut(),
            ss_flags: libc::SS_DISABLE,
            ss_size: 0,
        };
        // SAFETY: disables this thread's alternate stack; no handler can
        // run on it after this returns.
        unsafe { libc::sigaltstack(&ss, ptr::null_mut()) };
    }
}

thread_local! {
    static ALT_STACK: RefCell<Option<AltStack>> = const { RefCell::new(None) };
}

/// Gives this thread a stack to run the handler on, so a fault caused by an
/// exhausted stack can still be reported. It is separate from every fiber
/// stack, so it stays valid across a fiber switch.
pub fn alt_stack() {
    const ALT: usize = 64 << 10;
    ALT_STACK.with(|a| {
        let mut a = a.borrow_mut();
        if a.is_some() {
            return;
        }
        let mut mem = vec![0u8; ALT].into_boxed_slice();
        let ss = libc::stack_t {
            ss_sp: mem.as_mut_ptr() as *mut _,
            ss_flags: 0,
            ss_size: ALT,
        };
        // SAFETY: the alternate stack lives in this thread's `ALT_STACK`
        // until the thread exits.
        unsafe { libc::sigaltstack(&ss, ptr::null_mut()) };
        *a = Some(AltStack { _mem: mem });
    });
}
