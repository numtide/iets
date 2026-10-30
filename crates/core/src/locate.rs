//! Where the local store keeps an input source.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::error::{self, Error as CrateError};
use crate::nixbin;
use crate::session::Session;
use crate::srcpath::{SourceKind, SourceOrigin, SrcPath};
use crate::store::path::{StorePath, store_dir};

#[derive(Default)]
struct Slot {
    prefetched: OnceLock<Option<BTreeSet<StorePath>>>,
    answer: OnceLock<BTreeSet<StorePath>>,
}

/// What the store answered for a path, kept for the life of the process: the
/// references of a valid store path never change.
static REFERENCES: Mutex<Option<HashMap<String, Arc<Slot>>>> = Mutex::new(None);

fn slot(full: &str) -> (Arc<Slot>, bool) {
    let mut kept = REFERENCES.lock().expect("references cache");
    let table = kept.get_or_insert_with(HashMap::new);
    match table.entry(full.to_string()) {
        Entry::Occupied(e) => (e.get().clone(), false),
        Entry::Vacant(e) => (e.insert(Arc::default()).clone(), true),
    }
}

/// The references the local store holds for `path`, empty when it cannot
/// answer. primops.cc:addPath needs them for a store path it is given with a
/// context, and swallows `queryPathInfo`'s error the same way.
pub fn references_of(path: &StorePath) -> BTreeSet<StorePath> {
    let full = path.to_string();
    let (s, _) = slot(&full);
    // a failed prefetch queries again
    s.answer
        .get_or_init(|| {
            (s.prefetched.get_or_init(|| None).clone())
                .or_else(|| query_references(&full))
                .unwrap_or_default()
        })
        .clone()
}

const MAX_PREFETCH: usize = 8;

/// Starts `references_of(path)` on a thread.
pub fn prefetch_references(path: StorePath, session: Arc<Session>) {
    static STARTED: AtomicUsize = AtomicUsize::new(0);
    if STARTED.load(Ordering::Relaxed) >= MAX_PREFETCH {
        return;
    }
    let full = path.to_string();
    let (s, new) = slot(&full);
    if new && STARTED.fetch_add(1, Ordering::Relaxed) < MAX_PREFETCH {
        thread::spawn(move || {
            let _entered = session.enter();
            s.prefetched.get_or_init(|| query_references(&full));
        });
    }
}

fn query_references(full: &str) -> Option<BTreeSet<StorePath>> {
    let out = Command::new(nixbin::nix())
        .args([
            "--extra-experimental-features",
            "nix-command",
            "path-info",
            "--json",
            full,
        ])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let info: HashMap<String, serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    info.get(full)?["references"]
        .as_array()?
        .iter()
        .map(|r| StorePath::parse(r.as_str()?).ok())
        .collect()
}

/// Answers the plan's locator from the local Nix store: one `nix path-info --json` call
/// tells how each input source was content-addressed and what it references. Paths named
/// by `--source` are read from the given local tree instead and are plain sources. A path
/// that is not valid locally, or whose content address v1 cannot send (an input-addressed
/// output pinned with `builtins.storePath`), is `None`: the daemon substitutes it.
pub fn locate_in_store(
    paths: &[StorePath],
    overrides: &HashMap<String, PathBuf>,
) -> error::Result<Vec<Option<SourceOrigin>>> {
    let ask: Vec<String> = paths
        .iter()
        .map(|p| p.to_string())
        .filter(|p| !overrides.contains_key(p))
        .collect();
    let mut info: HashMap<String, serde_json::Value> = HashMap::new();
    if !ask.is_empty() {
        let out = Command::new(nixbin::nix())
            .args([
                "--extra-experimental-features",
                "nix-command",
                "path-info",
                "--json",
            ])
            .args(&ask)
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| CrateError::io("nix path-info", e))?;
        if !out.status.success() {
            return Err(CrateError::Protocol("nix path-info failed".into()));
        }
        info = serde_json::from_slice(&out.stdout)
            .map_err(|e| CrateError::Protocol(format!("nix path-info: {e}")))?;
    }
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
            // `null` is what `nix path-info` prints for a path that is not valid.
            let v = match info.get(&full) {
                Some(v) if !v.is_null() => v,
                _ => return Ok(None),
            };
            let ca = v["ca"].as_str().unwrap_or("");
            let kind = if ca.starts_with("fixed:r:sha256:") {
                SourceKind::Source
            } else if ca.starts_with("text:sha256:") {
                let references = v["references"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|r| r.as_str()).collect::<Vec<_>>())
                    .unwrap_or_default()
                    .into_iter()
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
