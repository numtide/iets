//! Fetching: an HTTP client, a spooled archive read by offset, an archive
//! unpacker, and Git and Git LFS. It knows nothing about flakes beyond the
//! URL helpers in `crate::flake::flakeref`. Callers include a locked flake
//! input (`crate::source`) and `iets prefetch`
//! (docs/explanation/plumbing-and-porcelain.md).

pub mod git;
pub mod http;
pub mod lfs;
pub mod roots;
pub mod spool;
pub mod unpack;
