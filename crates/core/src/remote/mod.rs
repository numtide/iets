//! Remote builds: the upload plan, the Nix worker-protocol client and the
//! `grpc://` client for nix-grpc-store.

pub mod copy;
pub mod event;
pub mod grpc;
pub mod plan;
pub mod worker;
