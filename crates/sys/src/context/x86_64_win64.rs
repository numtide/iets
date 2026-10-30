//! The Win64 x86_64 context switch.
//!
//! Arguments come in rcx and rdx. rdi, rsi and xmm6 to xmm15 are callee-saved
//! here. Windows also keeps the stack bounds in the TEB (`gs:[8]` is the
//! base, `gs:[0x10]` the limit and `gs:[0x1478]` the deallocation stack), and
//! its stack probes and unwinder read them, so every switch swaps them.

use core::arch::{asm, naked_asm};
use std::ptr;

use super::Entry;

/// Saves the callee-saved registers, the TEB stack bounds, mxcsr and the x87
/// control word, stores rsp into `*save`, loads `load` as rsp, restores and
/// returns into the target's frame.
///
/// # Safety
/// `*save` is where this stack's pointer goes, and `load` is a pointer this
/// function or `seed_stack` produced for a stack nobody is running on.
#[unsafe(naked)]
pub unsafe extern "C" fn switch(save: *mut *mut u8, load: *mut u8) {
    naked_asm!(
        "push rbp",
        "push rbx",
        "push rdi",
        "push rsi",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "push qword ptr gs:[0x08]",
        "push qword ptr gs:[0x10]",
        "push qword ptr gs:[0x1478]",
        "sub rsp, 168",
        "stmxcsr [rsp + 160]",
        "fnstcw [rsp + 164]",
        "movups [rsp], xmm6",
        "movups [rsp + 16], xmm7",
        "movups [rsp + 32], xmm8",
        "movups [rsp + 48], xmm9",
        "movups [rsp + 64], xmm10",
        "movups [rsp + 80], xmm11",
        "movups [rsp + 96], xmm12",
        "movups [rsp + 112], xmm13",
        "movups [rsp + 128], xmm14",
        "movups [rsp + 144], xmm15",
        "mov [rcx], rsp",
        "mov rsp, rdx",
        "ldmxcsr [rsp + 160]",
        "fldcw [rsp + 164]",
        "movups xmm6, [rsp]",
        "movups xmm7, [rsp + 16]",
        "movups xmm8, [rsp + 32]",
        "movups xmm9, [rsp + 48]",
        "movups xmm10, [rsp + 64]",
        "movups xmm11, [rsp + 80]",
        "movups xmm12, [rsp + 96]",
        "movups xmm13, [rsp + 112]",
        "movups xmm14, [rsp + 128]",
        "movups xmm15, [rsp + 144]",
        "add rsp, 168",
        "pop qword ptr gs:[0x1478]",
        "pop qword ptr gs:[0x10]",
        "pop qword ptr gs:[0x08]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rsi",
        "pop rdi",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// Entered by the first `ret` of a fresh fiber with the argument in r12, the
/// entry in r13 and rsp 16-byte aligned. The call needs 32 bytes of shadow
/// space above its return address.
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    naked_asm!("mov rcx, r12", "sub rsp, 32", "call r13", "ud2")
}

/// Lays out the 264 bytes `switch` will pop, from the bottom: xmm6 to xmm15,
/// mxcsr and the x87 control word, the three TEB words, r15 to rbp, then the
/// return address (the trampoline).
///
/// # Safety
/// `low..top` is a writable mapping with room for the frame.
pub unsafe fn seed_stack(low: *mut u8, top: *mut u8, entry: Entry, arg: *mut ()) -> *mut u8 {
    // SAFETY: writes the 264 bytes below `top`, inside the mapping.
    unsafe {
        let sp = top.sub(264);
        ptr::write_bytes(sp, 0, 264);
        let t = sp as *mut u64;
        *t.add(20) = 0x1F80 | (0x037F << 32); // mxcsr, x87 cw
        *t.add(21) = low as u64; // deallocation stack
        *t.add(22) = low as u64; // stack limit
        *t.add(23) = top as u64; // stack base
        *t.add(26) = entry as usize as u64; // r13
        *t.add(27) = arg as u64; // r12
        *t.add(32) = trampoline as *const () as usize as u64; // ret
        sp
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
