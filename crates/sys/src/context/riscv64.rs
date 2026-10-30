//! The RISC-V 64 context switch.

use core::arch::{asm, naked_asm};

use super::Entry;
use std::ptr;

/// Saves ra, s0-s11, fs0-fs11 and fcsr; swaps sp. Untested on hardware.
///
/// # Safety
/// `*save` is where this stack's pointer goes, and `load` is a pointer this
/// function or `seed_stack` produced for a stack nobody is running on.
#[unsafe(naked)]
pub unsafe extern "C" fn switch(save: *mut *mut u8, load: *mut u8) {
    naked_asm!(
        ".option push",
        ".option arch, +d",
        "addi sp, sp, -208",
        "sd ra, 0(sp)",
        "sd s0, 8(sp)",
        "sd s1, 16(sp)",
        "sd s2, 24(sp)",
        "sd s3, 32(sp)",
        "sd s4, 40(sp)",
        "sd s5, 48(sp)",
        "sd s6, 56(sp)",
        "sd s7, 64(sp)",
        "sd s8, 72(sp)",
        "sd s9, 80(sp)",
        "sd s10, 88(sp)",
        "sd s11, 96(sp)",
        "fsd fs0, 104(sp)",
        "fsd fs1, 112(sp)",
        "fsd fs2, 120(sp)",
        "fsd fs3, 128(sp)",
        "fsd fs4, 136(sp)",
        "fsd fs5, 144(sp)",
        "fsd fs6, 152(sp)",
        "fsd fs7, 160(sp)",
        "fsd fs8, 168(sp)",
        "fsd fs9, 176(sp)",
        "fsd fs10, 184(sp)",
        "fsd fs11, 192(sp)",
        "frcsr t0",
        "sd t0, 200(sp)",
        "sd sp, 0(a0)",
        "mv sp, a1",
        "ld t0, 200(sp)",
        "fscsr t0",
        "fld fs0, 104(sp)",
        "fld fs1, 112(sp)",
        "fld fs2, 120(sp)",
        "fld fs3, 128(sp)",
        "fld fs4, 136(sp)",
        "fld fs5, 144(sp)",
        "fld fs6, 152(sp)",
        "fld fs7, 160(sp)",
        "fld fs8, 168(sp)",
        "fld fs9, 176(sp)",
        "fld fs10, 184(sp)",
        "fld fs11, 192(sp)",
        "ld ra, 0(sp)",
        "ld s0, 8(sp)",
        "ld s1, 16(sp)",
        "ld s2, 24(sp)",
        "ld s3, 32(sp)",
        "ld s4, 40(sp)",
        "ld s5, 48(sp)",
        "ld s6, 56(sp)",
        "ld s7, 64(sp)",
        "ld s8, 72(sp)",
        "ld s9, 80(sp)",
        "ld s10, 88(sp)",
        "ld s11, 96(sp)",
        "addi sp, sp, 208",
        "ret",
        ".option pop",
    )
}

/// Entered by the first `ret` of a fresh fiber with the fiber pointer
/// in s0, the entry in s1 and sp 16-byte aligned.
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    naked_asm!("mv a0, s0", "jalr s1", "ebreak",)
}

/// # Safety
/// `top` is the end of a writable mapping with room for the frame.
pub unsafe fn seed_stack(_low: *mut u8, top: *mut u8, entry: Entry, arg: *mut ()) -> *mut u8 {
    // SAFETY: writes 208 bytes below `top`, inside the mapping.
    unsafe {
        let sp = top.sub(208);
        ptr::write_bytes(sp, 0, 208);
        let t = sp as *mut u64;
        *t = trampoline as *const () as usize as u64; // ra
        *t.add(1) = arg as u64; // s0
        *t.add(2) = entry as usize as u64; // s1
        sp
    }
}

#[inline(always)]
pub fn sp() -> usize {
    let sp: usize;
    // SAFETY: reads a register into a local; touches no memory and
    // leaves the stack alone.
    unsafe {
        asm!("mv {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    sp
}
