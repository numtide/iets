//! The Windows AArch64 context switch.
//!
//! The callee-saved set is the Linux one. x18 holds the TEB and belongs to
//! the thread, so it is neither saved nor restored, and a fiber that moves
//! threads sees the new thread's TEB. The stack bounds live in the TEB
//! (`[x18, #8]` is the base, `[x18, #0x10]` the limit and `[x18, #0x1478]`
//! the deallocation stack), and the probes and unwinder read them, so every
//! switch swaps them.

use core::arch::{asm, naked_asm};

use super::Entry;
use std::ptr;

/// Saves x19-x30, d8-d15, fpcr and the TEB stack bounds, swaps sp. Untested
/// on hardware.
///
/// # Safety
/// `*save` is where this stack's pointer goes, and `load` is a pointer this
/// function or `seed_stack` produced for a stack nobody is running on.
#[unsafe(naked)]
pub unsafe extern "C" fn switch(save: *mut *mut u8, load: *mut u8) {
    naked_asm!(
        "sub sp, sp, #192",
        "stp x19, x20, [sp, #0]",
        "stp x21, x22, [sp, #16]",
        "stp x23, x24, [sp, #32]",
        "stp x25, x26, [sp, #48]",
        "stp x27, x28, [sp, #64]",
        "stp x29, x30, [sp, #80]",
        "stp d8, d9, [sp, #96]",
        "stp d10, d11, [sp, #112]",
        "stp d12, d13, [sp, #128]",
        "stp d14, d15, [sp, #144]",
        "mrs x9, fpcr",
        "str x9, [sp, #160]",
        "ldr x9, [x18, #8]",
        "ldr x10, [x18, #0x10]",
        "ldr x11, [x18, #0x1478]",
        "str x9, [sp, #168]",
        "str x10, [sp, #176]",
        "str x11, [sp, #184]",
        "mov x9, sp",
        "str x9, [x0]",
        "mov sp, x1",
        "ldr x9, [sp, #160]",
        "msr fpcr, x9",
        "ldr x9, [sp, #168]",
        "ldr x10, [sp, #176]",
        "ldr x11, [sp, #184]",
        "str x9, [x18, #8]",
        "str x10, [x18, #0x10]",
        "str x11, [x18, #0x1478]",
        "ldp d14, d15, [sp, #144]",
        "ldp d12, d13, [sp, #128]",
        "ldp d10, d11, [sp, #112]",
        "ldp d8, d9, [sp, #96]",
        "ldp x29, x30, [sp, #80]",
        "ldp x27, x28, [sp, #64]",
        "ldp x25, x26, [sp, #48]",
        "ldp x23, x24, [sp, #32]",
        "ldp x21, x22, [sp, #16]",
        "ldp x19, x20, [sp, #0]",
        "add sp, sp, #192",
        "ret",
    )
}

#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    naked_asm!("mov x0, x19", "blr x20", "brk #1",)
}

/// # Safety
/// `low..top` is a writable mapping with room for the frame.
pub unsafe fn seed_stack(low: *mut u8, top: *mut u8, entry: Entry, arg: *mut ()) -> *mut u8 {
    // SAFETY: writes 192 bytes below `top`, inside the mapping.
    unsafe {
        let sp = top.sub(192);
        ptr::write_bytes(sp, 0, 192);
        let t = sp as *mut u64;
        *t = arg as u64; // x19
        *t.add(1) = entry as usize as u64; // x20
        *t.add(11) = trampoline as *const () as usize as u64; // x30
        *t.add(21) = top as u64; // stack base
        *t.add(22) = low as u64; // stack limit
        *t.add(23) = low as u64; // deallocation stack
        sp
    }
}

#[inline(always)]
pub fn sp() -> usize {
    let sp: usize;
    // SAFETY: reads a register into a local; touches no memory and
    // leaves the stack and the flags alone.
    unsafe {
        asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    sp
}
