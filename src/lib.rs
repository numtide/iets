//! The library facade. Frontends use `api`; the modules below it are the
//! `iets-core` crate and its layers, re-exported here under their old
//! names (docs/reference/library.md).

pub use iets_core::errln;
pub use iets_core::*;

pub mod api;
pub mod cli;
pub mod devenv;
