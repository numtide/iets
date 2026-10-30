//! Switching between stacks: what a fiber needs from the machine.
//!
//! One file per calling convention. Each has `switch`, which saves the
//! callee-saved state and swaps stack pointers. It has `seed_stack`, which
//! lays out a fresh stack spanning `low..top` so the first `switch` into it
//! calls `entry(arg)`. It has `sp`, which reads the stack pointer.

/// The first function on a fresh stack. It must never return.
pub type Entry = unsafe extern "C" fn(*mut ()) -> !;

#[cfg_attr(all(target_arch = "x86_64", not(windows)), path = "x86_64_sysv.rs")]
#[cfg_attr(all(target_arch = "x86_64", windows), path = "x86_64_win64.rs")]
#[cfg_attr(all(target_arch = "aarch64", not(windows)), path = "aarch64.rs")]
#[cfg_attr(all(target_arch = "aarch64", windows), path = "aarch64_windows.rs")]
#[cfg_attr(target_arch = "riscv64", path = "riscv64.rs")]
mod imp;

pub use imp::{seed_stack, sp, switch};
