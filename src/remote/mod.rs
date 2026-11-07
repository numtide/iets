//! Remote builds: the upload plan, the Nix worker-protocol client and the
//! `grpc://` client for nix-grpc-store.

pub mod closure;
pub mod copy;
pub mod event;
#[cfg_attr(target_os = "wasi", path = "grpc_none.rs")]
pub mod grpc;
pub mod plan;
pub mod worker;
