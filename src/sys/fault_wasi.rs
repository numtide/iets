//! Faults trap the WebAssembly instance, and nothing in it can catch that.

pub use super::Fault;

pub fn install(_classify: fn(usize) -> Option<Fault>) {}

pub fn alt_stack() {}
