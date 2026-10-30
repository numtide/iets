//! The System V x86_64 context switch: Linux and macOS.

use core::arch::{asm, naked_asm};

use super::Entry;

/// Saves rbx rbp r12-r15 mxcsr x87cw, stores rsp into `*save`, loads
/// `load` as rsp, restores, returns into the target's frame.
///
/// # Safety
/// `*save` is where this stack's pointer goes, and `load` is a pointer this
/// function or `seed_stack` produced for a stack nobody is running on.
#[unsafe(naked)]
pub unsafe extern "C" fn switch(save: *mut *mut u8, load: *mut u8) {
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "sub rsp, 8",
        "stmxcsr [rsp]",
        "fnstcw [rsp + 4]",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "ldmxcsr [rsp]",
        "fldcw [rsp + 4]",
        "add rsp, 8",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// Entered by the first `ret` of a fresh fiber with the fiber pointer
/// in r12, the entry in r13 and rsp 16-byte aligned.
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    naked_asm!("mov rdi, r12", "call r13", "ud2",)
}

/// Lay out the frame `switch` will pop: fp control word, r15..rbp, then
/// the return address (the trampoline).
///
/// # Safety
/// `top` is the end of a writable mapping with room for the frame.
pub unsafe fn seed_stack(_low: *mut u8, top: *mut u8, entry: Entry, arg: *mut ()) -> *mut u8 {
    // SAFETY: writes the 64 bytes below `top`, inside the mapping.
    unsafe {
        let t = top as *mut u64;
        *t.sub(1) = trampoline as *const () as usize as u64; // ret
        *t.sub(2) = 0; // rbp
        *t.sub(3) = 0; // rbx
        *t.sub(4) = arg as u64; // r12
        *t.sub(5) = entry as usize as u64; // r13
        *t.sub(6) = 0; // r14
        *t.sub(7) = 0; // r15
        *t.sub(8) = 0x1F80 | (0x037F << 32); // mxcsr, x87 cw
        t.sub(8) as *mut u8
    }
}

#[inline(always)]
pub fn sp() -> usize {
    let sp: usize;
    // SAFETY: reads a register into a local; touches no memory and
    // leaves the stack and the flags alone.
    unsafe {
        asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    sp
}
