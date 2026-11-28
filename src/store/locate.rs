//! Where the local store keeps an input source.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::error::{self, Error as CrateError};
use crate::session::Session;
use crate::source;
use crate::store::path::{StorePath, store_dir};
use crate::store::source::{SourceKind, SourceOrigin, SrcPath};

#[derive(Default)]
struct Slot {
    loading: Mutex<()>,
    answer: OnceLock<BTreeSet<StorePath>>,
}

impl Slot {
    fn get_or_query(
        &self,
        query: impl FnOnce() -> Option<BTreeSet<StorePath>>,
    ) -> Option<&BTreeSet<StorePath>> {
        if let Some(refs) = self.answer.get() {
            return Some(refs);
        }
        // A background prefetch and the first foreground reader share the
        // lookup. A failed or missing answer leaves the slot available to retry.
        let _guard = self.loading.lock().expect("references lookup");
        if self.answer.get().is_none() {
            self.answer.set(query()?).expect("locked empty slot");
        }
        self.answer.get()
    }
}

#[derive(Default)]
pub(crate) struct References(Mutex<HashMap<String, Arc<Slot>>>);

impl References {
    fn slot(&self, full: &str) -> (Arc<Slot>, bool) {
        let mut table = self.0.lock().expect("references cache");
        match table.entry(full.to_string()) {
            Entry::Occupied(e) => (e.get().clone(), false),
            Entry::Vacant(e) => (e.insert(Arc::default()).clone(), true),
        }
    }
}

/// The references the local store holds for `path`, empty when it cannot
/// answer. primops.cc:addPath needs them for a store path it is given with a
/// context, and swallows `queryPathInfo`'s error the same way.
pub fn references_of(path: &StorePath) -> BTreeSet<StorePath> {
    references_in(path, &Session::current()).unwrap_or_default()
}

pub(crate) fn references_in(
    path: &StorePath,
    session: &Arc<Session>,
) -> Option<BTreeSet<StorePath>> {
    let full = path.to_string();
    let (s, _) = session.references.slot(&full);
    s.get_or_query(|| query_references(&full, session)).cloned()
}

const MAX_PREFETCH: usize = 8;

/// Starts `references_of(path)` on a thread.
pub fn prefetch_references(path: StorePath, session: Arc<Session>) {
    static STARTED: AtomicUsize = AtomicUsize::new(0);
    if STARTED.load(Ordering::Relaxed) >= MAX_PREFETCH {
        return;
    }
    let full = path.to_string();
    let (s, new) = session.references.slot(&full);
    if new && STARTED.fetch_add(1, Ordering::Relaxed) < MAX_PREFETCH {
        thread::spawn(move || {
            s.get_or_query(|| query_references(&full, &session));
        });
    }
}

fn query_references(full: &str, session: &Arc<Session>) -> Option<BTreeSet<StorePath>> {
    // The worker protocol sends daemon activities to the thread's log.
    let _entered = session.enter();
    let info = source::local_daemon(session)?
        .query_path_info(full, &mut |_| {})
        .ok()??;
    info.references
        .iter()
        .map(|r| StorePath::parse(r).ok())
        .collect()
}

/// Answers the plan's locator from the local daemon: how each input source was
/// content-addressed and what it references. Paths named by `--source` are read from the given local tree instead and are plain sources. A path
/// that is not valid locally, or whose content address v1 cannot send (an input-addressed
/// output pinned with `builtins.storePath`), is `None`: the daemon substitutes it.
pub fn locate_in_store(
    paths: &[StorePath],
    overrides: &HashMap<String, PathBuf>,
) -> error::Result<Vec<Option<SourceOrigin>>> {
    let mut daemon = paths
        .iter()
        .any(|p| !overrides.contains_key(&p.to_string()))
        .then(|| source::local_daemon(&Session::current()))
        .flatten();
    paths
        .iter()
        .map(|p| {
            let full = p.to_string();
            let unhashed = |at, kind| SourceOrigin {
                at,
                kind,
                nar: None,
            };
            if let Some(local) = overrides.get(&full) {
                return Ok(Some(unhashed(
                    SrcPath::fs(local.clone(), None),
                    SourceKind::Source,
                )));
            }
            let Some(d) = daemon.as_mut() else {
                return Ok(None);
            };
            let Some(info) = d.query_path_info(&full, &mut |_| {})? else {
                return Ok(None);
            };
            let kind = if info.ca.starts_with("fixed:r:sha256:") {
                SourceKind::Source
            } else if info.ca.starts_with("text:sha256:") {
                let references = info
                    .references
                    .iter()
                    .map(|r| StorePath::parse(r).map_err(CrateError::from))
                    .collect::<error::Result<_>>()?;
                SourceKind::Text { references }
            } else {
                return Ok(None);
            };
            Ok(Some(unhashed(
                SrcPath::fs(format!("{}/{}", store_dir(), p.base_name()), None),
                kind,
            )))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_reference_query_is_retried_without_losing_valid_empty_answers() {
        let reference =
            StorePath::from_base_name("00000000000000000000000000000000-reference").unwrap();
        for registered in [BTreeSet::from([reference]), BTreeSet::new()] {
            let slot = Slot::default();
            assert!(slot.get_or_query(|| None).is_none());
            assert_eq!(
                slot.get_or_query(|| Some(registered.clone())),
                Some(&registered)
            );
            assert_eq!(
                slot.get_or_query(|| panic!("registered answer should be reused")),
                Some(&registered)
            );
        }
    }
}
