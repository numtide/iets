//! What a run owns. A library must not keep this in process-wide statics:
//! two evaluators in one process each want their own settings, their own
//! log destination and their own mismatch list. `Session` holds all of it.
//!
//! An `Evaluator` keeps a handle to the session it was made in, so the hot
//! paths read a field. Code with no evaluator at hand — the command line,
//! a fetch, a build — uses [`Session::current`], the session this thread
//! entered. A thread the library starts inherits it through
//! [`bind`]. docs/reference/library.md says what a host owns.

use std::cell::RefCell;
use std::panic;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::env::Env;
use crate::events::Events;
use crate::ignored::Ignored;
use crate::jobs::Run;
use crate::log::Log;
use crate::profile::Profile;
use crate::provenance::SourceMap;
use crate::settings::Settings;
use crate::stdout::Stdout;
use crate::syntax::interner::Interner;
use crate::sys::heap::Heap;
use crate::{config, error, treestore};

#[derive(Default)]
pub struct Session {
    pub log: Log,
    pub env: Env,
    pub command: Mutex<Option<&'static str>>,
    pub catch_exit: AtomicBool,
    pub out: Stdout,
    /// This run's sampled flame graph (`crate::profile`).
    pub profile: Profile,
    /// Where this run's instrumentation events go (`crate::events`).
    pub events: Events,
    settings: OnceLock<Settings>,
    /// What this run was asked for and does not do (`crate::ignored`).
    pub ignored: Ignored,
    /// The local tree store this run reads fetched trees from
    /// (`crate::treestore`).
    pub trees: treestore::Store,
    /// The `eval-jobs` walk of this run (`crate::jobs`).
    pub jobs: Run,
    /// nix.conf, read once per run: it cannot change under an evaluation.
    pub nix_conf: OnceLock<config::Config>,
    /// Store path to local tree, for the commands that build the map while
    /// the evaluation is live (`crate::provenance`).
    pub source_map: OnceLock<SourceMap>,
    /// The `parse-toml-timestamps` experimental feature.
    pub toml_timestamps: AtomicBool,
    /// Every arena chunk and symbol name of this session's evaluators.
    /// Released when the last of the session, its evaluators and its
    /// worker threads lets go (docs/reference/library.md).
    pub heap: Arc<Heap>,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Session>>> = const { RefCell::new(None) };
}

/// The session of a thread that entered none: everything the library does
/// before a host makes one of its own. It writes nowhere.
fn fallback() -> Arc<Session> {
    static D: OnceLock<Arc<Session>> = OnceLock::new();
    D.get_or_init(|| Arc::new(Session::default())).clone()
}

impl Session {
    pub fn new() -> Arc<Session> {
        crate::sys::on_fatal(|m| Session::current().log.errln(m));
        Arc::new(Session::default())
    }

    pub fn with_env(env: Env) -> Arc<Session> {
        Arc::new(Session {
            env,
            ..Session::default()
        })
    }

    /// The session this thread entered, else the fallback one.
    pub fn current() -> Arc<Session> {
        CURRENT
            .with(|c| c.borrow().clone())
            .unwrap_or_else(fallback)
    }

    /// Makes `self` this thread's session until the guard drops.
    pub fn enter(self: &Arc<Session>) -> Entered {
        let prev = CURRENT.with(|c| c.borrow_mut().replace(self.clone()));
        Entered(prev)
    }

    /// This session's settings: the environment knobs alone until
    /// [`Session::load_settings`] reads the files (docs/reference/settings.md).
    pub fn settings(&self) -> &Settings {
        self.settings.get_or_init(Settings::default)
    }

    /// The environment settings with `IETS_STATS` at `level`, for a test that
    /// reads the arena totals under its own session.
    #[cfg(any(test, feature = "testkit"))]
    pub fn set_stats_for_test(&self, level: u8) {
        let s = Settings {
            stats: level,
            ..Settings::default()
        };
        let _ = self.settings.set(s);
    }

    /// Reads the `iets.toml` settings files and applies the `--iets-option` pairs.
    /// Call it once, on an entered session, before anything reads a setting.
    pub fn load_settings(&self, options: &[(String, String)]) -> error::Result<()> {
        let s = Settings::load(options)?;
        let _ = self.settings.set(s);
        Ok(())
    }
}

pub struct Exit(pub i32);

/// Ends the process, or unwinds to `cli::run` when the host asked to catch it.
pub fn exit(code: i32) -> ! {
    if Session::current().catch_exit.load(Ordering::Relaxed) {
        panic::resume_unwind(Box::new(Exit(code)));
    }
    process::exit(code)
}

/// Puts back the session [`Session::enter`] replaced.
pub struct Entered(Option<Arc<Session>>);

impl Drop for Entered {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.0.take());
    }
}

/// `f` on a new thread, with this thread's session entered there.
pub fn bind<R>(f: impl FnOnce() -> R) -> impl FnOnce() -> R {
    let s = Session::current();
    move || {
        let _entered = s.enter();
        f()
    }
}

impl Session {
    /// An interner whose names live in this session's memory.
    pub fn interner(&self) -> Interner {
        Interner::in_heap(self.heap.clone())
    }
}
