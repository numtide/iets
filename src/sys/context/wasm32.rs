//! WebAssembly has no stack to switch: the engine keeps the call stack out of
//! reach. Evaluation stays on one fiber there, so nothing here runs.

use super::Entry;

/// # Safety
/// Never sound to call: see the module.
pub unsafe extern "C" fn switch(_save: *mut *mut u8, _load: *mut u8) {
    unreachable!("WebAssembly cannot switch stacks")
}

/// # Safety
/// Never sound to call: see the module.
pub unsafe fn seed_stack(
    _low: *mut u8,
    _limit: *mut u8,
    _top: *mut u8,
    _entry: Entry,
    _arg: *mut (),
) -> *mut u8 {
    unreachable!("WebAssembly cannot switch stacks")
}

/// There is no stack pointer to read. Zero is below every limit, and the one
/// reader tests a limit that is itself zero here.
#[inline(always)]
pub fn sp() -> usize {
    0
}
