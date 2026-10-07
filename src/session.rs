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
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use crate::events::Events;
use crate::ignored::Ignored;
use crate::jobs::Run;
use crate::log::Log;
use crate::profile::Profile;
use crate::provenance::SourceMap;
use crate::settings::Settings;
use crate::{config, error, treestore};

#[derive(Default)]
pub struct Session {
    pub log: Log,
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
        Arc::new(Session::default())
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

    /// Reads the `iets.toml` settings files and applies the `--iets-option` pairs.
    /// Call it once, on an entered session, before anything reads a setting.
    pub fn load_settings(&self, options: &[(String, String)]) -> error::Result<()> {
        let s = Settings::load(options)?;
        let _ = self.settings.set(s);
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::log;

    #[test]
    fn two_sessions_share_no_state() {
        let line = format!(
            "{}:\n  specified: sha256-A=\n     got:    sha256-B=",
            crate::log::MISMATCH
        );
        let (a, b) = (Session::new(), Session::new());
        {
            let _entered = a.enter();
            log::note_mismatch(&line);
            log::resolve_mismatches(&|_| true);
        }
        {
            let _entered = b.enter();
            assert!(crate::log::take_mismatches().is_empty());
        }
        let _entered = a.enter();
        assert_eq!(crate::log::take_mismatches().len(), 1);
    }
}
