//! The evaluator and everything it needs below the command line: runtime,
//! compiler, builtins, lints, the tracing cache, the fetchers, the stores
//! and the build dispatcher. `api` and `cli` sit above, in `iets`.

pub use iets_store as store;
pub use iets_syntax as syntax;
pub use iets_sys as sys;
pub use iets_sys::{argv, bytescan, display, env, events, sink, stdout};

/// One line to this thread's session log, in place of `eprintln!`: a
/// library writes to no descriptor of its own (docs/reference/library.md).
#[macro_export]
macro_rules! errln {
    ($($t:tt)*) => {
        $crate::session::Session::current().log.errln(&format!($($t)*))
    };
}

pub mod build;
pub mod builtins;
pub mod cachefs;
pub mod compile;
pub mod config;
pub mod error;
pub mod eval;
pub mod fetch;
pub mod flake;
pub mod ignored;
pub mod jobs;
pub mod lint;
pub mod locate;
pub mod log;
pub mod nixbin;
pub mod parallel;
pub mod print;
pub mod profile;
pub mod progress;
pub mod project;
pub mod provenance;
pub mod remote;
pub mod runtime;
pub mod session;
pub mod settings;
pub mod source;
pub mod srcpath;
pub mod trace;
pub mod treestore;
