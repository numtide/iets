//! The paths a watcher has to watch for a tracing-cache entry's answer to go
//! stale. `iets cache paths` is the command over this.

use std::iter;
use std::path::Path;

use crate::error;
use crate::store::path;
use crate::trace::store::{Entry, Part};
pub use crate::trace::watch::{MAX, Watched};
use crate::trace::{watch, wire};

/// The watchable paths of the entry file at `entry`, in the order the run
/// read them, capped at [`MAX`].
///
/// The stores to exclude come from `entry` itself: an entry lives at
/// `<cache>/trace/<key>/entry`, so its cache root is three levels up, and a
/// caller that used `--cache-dir` therefore passes nothing extra.
pub fn watch_paths(entry: &Path) -> error::Result<Vec<Watched>> {
    let obs = Entry::open(entry)
        .and_then(|e| e.part(Part::Readset))
        .and_then(|b| wire::decode_readset(&b));
    let Some(obs) = obs else {
        return Err(error::Error::Cache(format!(
            "{} is not a cache entry",
            entry.display()
        )));
    };
    let own = entry
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent());
    let store = Path::new(path::STORE_DIR);
    let skip: Vec<&Path> = iter::once(store).chain(own).collect();
    let mut w = watch::watched(&obs, &skip);
    w.truncate(MAX);
    Ok(w)
}
