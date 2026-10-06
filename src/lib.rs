/// One line to this thread's session log, in place of `eprintln!`: a
/// library writes to no descriptor of its own (docs/reference/library.md).
#[macro_export]
macro_rules! errln {
    ($($t:tt)*) => {
        $crate::session::Session::current().log.errln(&format!($($t)*))
    };
}

pub mod api;
pub mod argv;
pub mod build;
pub mod builtins;
pub mod cachefs;
pub mod cli;
pub mod compile;
pub mod config;
pub mod devenv;
pub mod error;
pub mod eval;
pub mod events;
pub mod fetch;
pub(crate) mod flake;
pub mod ignored;
pub mod jobs;
pub mod lint;
pub mod log;
pub mod nixbin;
pub mod parallel;
pub mod print;
pub mod profile;
pub mod project;
pub mod provenance;
pub mod remote;
pub mod runtime;
pub mod session;
pub mod settings;
pub mod source;
pub mod store;
pub mod syntax;
pub mod trace;
pub mod treestore;
