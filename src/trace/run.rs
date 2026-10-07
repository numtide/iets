//! The tracing cache for one command run: the key a subcommand's arguments
//! make, the lookup, and the entry it writes.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::eval::Evaluator;
use crate::store::path::StorePath;
use crate::trace::key::Key;
use crate::trace::store::{Entry, Opened, Part};
use crate::trace::wire;

/// The tracing cache for one run: where the entry lives and its key. See docs/explanation/caching.md.
pub struct Cache {
    pub store: crate::trace::store::Store,
    pub key: String,
    timings: bool,
    /// The generation of the entry this run reads, opened once. Everything a
    /// hit replays comes from it, so a sibling that publishes meanwhile
    /// cannot mix its parts into this run.
    entry: OnceLock<Opened>,
}

impl Cache {
    /// `None` when no cache store can be located, or when its directory is
    /// not iets's to write in.
    pub fn open(cache_dir: Option<&str>, key: Key, timings: bool) -> Option<Cache> {
        let store = crate::trace::store::Store::locate(cache_dir)?;
        if !crate::cachefs::prepare(store.root(), crate::cachefs::Shape::Dirs, timings) {
            return None;
        }
        // before any evaluation records an `Env` observation
        crate::cachefs::use_env_key(store.root());
        Some(Cache {
            store,
            key: key.finish(),
            timings,
            entry: OnceLock::new(),
        })
    }

    fn opened(&self) -> &Opened {
        self.entry.get_or_init(|| self.store.open(&self.key))
    }

    /// One part of the entry this run validated.
    pub fn part(&self, part: Part) -> Option<Vec<u8>> {
        match self.opened() {
            Opened::Entry(e) => e.part(part),
            _ => None,
        }
    }

    /// Where this run's entry lives, for a caller that hands the entry to
    /// `iets cache paths` instead of deriving the key again. The file need
    /// not exist yet: a first run names the entry it is about to write.
    pub fn entry_path(&self) -> std::path::PathBuf {
        self.store
            .entry_dir(&self.key)
            .join(crate::trace::store::ENTRY)
    }

    /// Validates the stored read set; true on a hit. A hit that refreshed
    /// stat fields publishes the entry again with the new read set. An entry
    /// that existed and no longer holds reports what changed; a run with no
    /// entry says nothing, because a first run invalidated nothing.
    ///
    /// The entry is opened once per [`Cache`], as one run looks its key up
    /// once: everything this run replays comes from that one generation.
    pub fn lookup(&self) -> bool {
        let t0 = std::time::Instant::now();
        let bad = || {
            self.store.discard(&self.key);
            if self.timings {
                crate::errln!("cache: miss (unreadable entry, discarded)");
            }
            false
        };
        let entry = match self.opened() {
            Opened::None => {
                if self.timings {
                    crate::errln!("cache: miss (no entry)");
                }
                return false;
            }
            Opened::Bad => return bad(),
            Opened::Entry(e) => e,
        };
        let Some(bytes) = entry.part(Part::Readset) else {
            return bad();
        };
        let Some(mut obs) = crate::trace::wire::decode_readset(&bytes) else {
            return bad();
        };
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
        let t_load = t0.elapsed();
        match crate::trace::validate::validate(&mut obs, threads) {
            Ok(r) => {
                if r.refreshed > 0 {
                    self.republish(entry, &crate::trace::wire::encode_readset(&obs));
                } else {
                    self.store.touch(&self.key);
                }
                if self.timings {
                    crate::errln!(
                        "cache: hit, {} entries validated, {} rehashed, load {:.3}s, validate {:.3}s",
                        r.entries,
                        r.rehashed,
                        t_load.as_secs_f64(),
                        (t0.elapsed() - t_load).as_secs_f64()
                    );
                }
                true
            }
            Err(m) => {
                crate::log::invalidated(m.0.clone());
                if self.timings {
                    crate::errln!(
                        "cache: miss ({}), {} entries, {:.3}s",
                        m.0,
                        obs.len(),
                        t0.elapsed().as_secs_f64()
                    );
                }
                false
            }
        }
    }

    /// A hit that refreshed stat fields publishes the whole entry again, with
    /// the parts it validated and the new read set. The other parts come from
    /// the open generation, so a sibling's publish in between is not mixed in.
    fn republish(&self, entry: &Entry, readset: &[u8]) {
        let kept: Vec<(Part, Vec<u8>)> = [Part::Warnings, Part::Drvs, Part::Sources, Part::Result]
            .into_iter()
            .filter_map(|p| Some((p, entry.part(p)?)))
            .collect();
        let mut parts: Vec<(Part, &[u8])> = kept.iter().map(|(p, b)| (*p, b.as_slice())).collect();
        parts.push((Part::Readset, readset));
        let _ = self.store.publish(&self.key, &parts);
    }

    /// Writes the entry after a successful run; a run that observed
    /// `currentTime` or called a fetcher writes nothing. `tables` adds the
    /// drv and source tables, which only a `build` hit reads back.
    pub fn write(&self, ev: &Evaluator, result: &[u8], tables: bool) {
        if !ev.trace().is_cacheable() {
            if self.timings {
                crate::errln!("cache: not written (uncacheable run)");
            }
            return;
        }
        let t0 = std::time::Instant::now();
        let obs = ev.trace().take();
        let mut warnings = Vec::new();
        for w in ev.lints.records() {
            warnings.extend_from_slice(w.as_bytes());
            warnings.push(b'\n');
        }
        let readset = wire::encode_readset(&obs);
        let (drvs, sources) = if tables {
            (
                wire::encode_drvs(&ev.drv.drvs.lock()),
                wire::encode_sources(&ev.known.origins()),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let mut parts: Vec<(Part, &[u8])> = Vec::with_capacity(5);
        if !warnings.is_empty() {
            parts.push((Part::Warnings, &warnings));
        }
        if tables {
            parts.push((Part::Drvs, &drvs));
            parts.push((Part::Sources, &sources));
        }
        parts.push((Part::Result, result));
        parts.push((Part::Readset, &readset));
        match self.store.publish(&self.key, &parts) {
            Ok(wrote) => {
                if self.timings {
                    if wrote && !obs.is_empty() {
                        crate::errln!(
                            "cache: wrote {} entries, {:.3}s",
                            obs.len(),
                            t0.elapsed().as_secs_f64()
                        );
                    } else if !wrote {
                        crate::errln!("cache: not written (a sibling holds the store lock)");
                    }
                }
            }
            Err(e) => crate::log::line(&format!(
                "warning: cannot write cache entry {}: {e}",
                self.store.entry_dir(&self.key).display()
            )),
        }
        crate::cachefs::maybe_sweep(self.store.root(), crate::cachefs::Shape::Dirs, self.timings);
    }

    /// [`Cache::write`] for a command that builds: `roots` is the result, and
    /// the tables go with it, for [`Cache::read_build`].
    pub fn write_build(&self, ev: &Evaluator, roots: &[(StorePath, Option<String>)]) {
        self.write(ev, &crate::trace::wire::encode_roots(roots), true);
    }

    /// On a hit: the lint warnings the recorded run printed, to stderr.
    pub fn replay_warnings(&self) {
        if let Some(bytes) = self.part(Part::Warnings) {
            crate::session::Session::current()
                .log
                .err(&crate::log::replay_lints(&bytes));
        }
    }

    /// The roots, drv table and source table of a `build` entry.
    #[allow(clippy::type_complexity)]
    pub fn read_build(
        &self,
    ) -> Option<(
        Vec<(crate::store::path::StorePath, Option<String>)>,
        HashMap<crate::store::path::StorePath, crate::builtins::derivation::DrvInfo>,
        HashMap<crate::store::path::StorePath, crate::store::source::SourceOrigin>,
    )> {
        let tables = (|| {
            let roots = wire::decode_roots(&self.part(Part::Result)?)?;
            let drvs = wire::decode_drvs(&self.part(Part::Drvs)?)?;
            let sources = wire::decode_sources(&self.part(Part::Sources)?)?;
            Some((roots, drvs, sources))
        })();
        if tables.is_none() {
            self.store.discard(&self.key);
            if self.timings {
                crate::errln!("cache: miss (unreadable tables, discarded)");
            }
        }
        tables
    }
}
