//! The paths a watcher has to watch for a tracing-cache entry's answer to go
//! stale. `iets cache paths` is the command over this.

use std::path::Path;

pub use crate::trace::watch::{MAX, Watched};

/// The watchable paths of the entry file at `entry`, in the order the run
/// read them, capped at [`MAX`].
///
/// The stores to exclude come from `entry` itself: an entry lives at
/// `<cache>/trace/<key>/entry`, so its cache root is three levels up, and a
/// caller that used `--cache-dir` therefore passes nothing extra.
pub fn watch_paths(entry: &Path) -> crate::error::Result<Vec<Watched>> {
    use crate::trace::store::{Entry, Part};
    let obs = Entry::open(entry)
        .and_then(|e| e.part(Part::Readset))
        .and_then(|b| crate::trace::wire::decode_readset(&b));
    let Some(obs) = obs else {
        return Err(crate::error::Error::Cache(format!(
            "{} is not a cache entry",
            entry.display()
        )));
    };
    let own = entry
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent());
    let store = Path::new(crate::store::path::STORE_DIR);
    let skip: Vec<&Path> = std::iter::once(store).chain(own).collect();
    let mut w = crate::trace::watch::watched(&obs, &skip);
    w.truncate(MAX);
    Ok(w)
}
