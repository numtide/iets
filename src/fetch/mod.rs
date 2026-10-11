//! Fetching: an HTTP client, a binary cache reader, a spooled archive read by
//! offset, and an archive unpacker. None of it knows about flakes; a locked
//! flake input is one caller (`crate::source`), `iets prefetch` is another
//! (docs/explanation/plumbing-and-porcelain.md).

pub mod binary_cache;
pub mod http;
pub mod roots;
pub mod spool;
pub mod unpack;
