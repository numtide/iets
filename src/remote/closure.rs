//! Sending what `builtins.fetchClosure` checked to a store that lacks it, with
//! the cache's signatures, which the store checks again.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::{self, Read};
use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use tempfile::{NamedTempFile, TempPath};

use crate::error::{Error, Result};
use crate::fetch::http::{self, Limits};
use crate::remote::plan::Object;
use crate::session::{self, Session};
use crate::source::nix_conf;
use crate::store::narinfo::Fetched;
use crate::store::path::StorePath;

/// More daemon connections than this contend on the store's database.
const MAX_JOBS: usize = 8;

/// What the store lacks of the closures of `roots`, references first.
pub fn missing(
    roots: &[StorePath],
    lookup: &dyn Fn(&StorePath) -> Option<Arc<Fetched>>,
    query_valid: &mut dyn FnMut(&[&StorePath]) -> Result<HashSet<String>>,
) -> Result<Vec<Arc<Fetched>>> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let order = post_order(roots, lookup)?;
    let paths: Vec<&StorePath> = order.iter().map(|f| &f.info.path).collect();
    let valid = query_valid(&paths)?;
    Ok(order
        .into_iter()
        .filter(|f| !valid.contains(f.info.path.base_name()))
        .collect())
}

/// `missing` with every NAR downloaded first, for a store that takes one
/// stream of objects.
pub fn objects(missing: &[Arc<Fetched>]) -> Result<Vec<Object>> {
    let nars = download(missing)?;
    Ok(missing
        .iter()
        .cloned()
        .zip(nars)
        .map(|(fetched, nar)| Object::Closure {
            fetched,
            nar: Arc::new(nar),
        })
        .collect())
}

/// Writes one path to a connection, calling the hook before its last byte.
pub type Add<C> = dyn Fn(&mut C, &Fetched, &mut dyn Read, &mut dyn FnMut() -> io::Result<()>) -> Result<u64>
    + Sync;

struct Slot {
    started: bool,
    /// References not started yet
    unstarted: usize,
    /// References not on the store yet
    undone: usize,
    dependents: Vec<usize>,
    /// NAR size plus the longest chain of dependents
    tail: u64,
}

struct Sched {
    slots: Vec<Slot>,
    startable: BinaryHeap<(u64, usize)>,
    left: usize,
    sent: u64,
    failed: Option<Error>,
}

impl Sched {
    fn start(&mut self, i: usize) {
        self.slots[i].started = true;
        for k in 0..self.slots[i].dependents.len() {
            let d = self.slots[i].dependents[k];
            self.slots[d].unstarted -= 1;
            if self.slots[d].unstarted == 0 && !self.slots[d].started {
                self.startable.push((self.slots[d].tail, d));
            }
        }
    }

    fn done(&mut self, i: usize, sent: u64) {
        self.left -= 1;
        self.sent += sent;
        for k in 0..self.slots[i].dependents.len() {
            let d = self.slots[i].dependents[k];
            self.slots[d].undone -= 1;
        }
    }
}

/// Sends `missing` over up to `MAX_JOBS` connections, since one daemon
/// process hashes and writes serially. The store checks references only when
/// it registers a path after its whole NAR, so a path streams before its
/// references are there and holds back its last byte until they are.
/// Starting paths in topological order keeps that from deadlocking. A NAR
/// above its fair share starts at once on a connection of its own.
pub fn send<C>(
    missing: &[Arc<Fetched>],
    open: &(dyn Fn() -> Result<C> + Sync),
    add: &Add<C>,
) -> Result<u64> {
    if missing.is_empty() {
        return Ok(0);
    }
    let limits = Limits::new(nix_conf(&Session::current()))?;
    let index: HashMap<&StorePath, usize> = missing
        .iter()
        .enumerate()
        .map(|(i, f)| (&f.info.path, i))
        .collect();
    let mut slots: Vec<Slot> = missing
        .iter()
        .map(|f| Slot {
            started: false,
            unstarted: 0,
            undone: 0,
            dependents: Vec::new(),
            tail: f.info.nar_size,
        })
        .collect();
    for (i, f) in missing.iter().enumerate() {
        for r in &f.info.references {
            if let Some(&j) = index.get(r)
                && j != i
            {
                slots[i].unstarted += 1;
                slots[i].undone += 1;
                slots[j].dependents.push(i);
            }
        }
    }
    for i in (0..slots.len()).rev() {
        let after = slots[i].dependents.iter().map(|&d| slots[d].tail).max();
        slots[i].tail += after.unwrap_or(0);
    }
    let jobs = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(MAX_JOBS)
        .min(missing.len());
    let total: u64 = missing.iter().map(|f| f.info.nar_size).sum();
    let mut s = Sched {
        slots,
        startable: BinaryHeap::new(),
        left: missing.len(),
        sent: 0,
        failed: None,
    };
    let mut early: Vec<usize> = (0..missing.len())
        .filter(|&i| missing[i].info.nar_size > total / jobs as u64)
        .collect();
    early.truncate(jobs);
    for &i in &early {
        s.start(i);
    }
    s.startable.clear();
    for i in 0..missing.len() {
        if s.slots[i].unstarted == 0 && !s.slots[i].started {
            s.startable.push((s.slots[i].tail, i));
        }
    }

    let sched = Mutex::new(s);
    let changed = Condvar::new();
    let lock = || sched.lock().unwrap_or_else(|e| e.into_inner());
    let wait = |s| changed.wait(s).unwrap_or_else(|e| e.into_inner());
    let path = |conn: &mut C, i: usize| -> Result<()> {
        let mut registrable = || -> io::Result<()> {
            let mut s = lock();
            while s.slots[i].undone > 0 && s.failed.is_none() {
                s = wait(s);
            }
            match s.failed {
                Some(_) => Err(io::Error::other("another path failed")),
                None => Ok(()),
            }
        };
        let f = &missing[i];
        let n = streamed(f, limits, |r| add(conn, f, r, &mut registrable))?;
        lock().done(i, n);
        changed.notify_all();
        Ok(())
    };
    let topological = || -> Result<()> {
        let mut conn = open()?;
        loop {
            let i = {
                let mut s = lock();
                loop {
                    if s.failed.is_some() || s.left == 0 {
                        return Ok(());
                    }
                    if let Some((_, i)) = s.startable.pop() {
                        s.start(i);
                        break i;
                    }
                    s = wait(s);
                }
            };
            changed.notify_all();
            path(&mut conn, i)?;
        }
    };
    let fail = |r: Result<()>| {
        if let Err(e) = r {
            lock().failed.get_or_insert(e);
            changed.notify_all();
        }
    };
    thread::scope(|scope| {
        let (path, fail) = (&path, &fail);
        for &i in &early {
            scope.spawn(session::bind(move || {
                fail(open().and_then(|mut c| path(&mut c, i)))
            }));
        }
        for _ in 0..jobs {
            scope.spawn(session::bind(|| fail(topological())));
        }
    });
    let s = sched.into_inner().unwrap_or_else(|e| e.into_inner());
    match s.failed {
        Some(e) => Err(e),
        None => Ok(s.sent),
    }
}

/// `f`'s NAR as the cache sends it, decompressed, into `use_`.
fn streamed(
    f: &Fetched,
    limits: Limits,
    use_: impl FnOnce(&mut dyn Read) -> Result<u64>,
) -> Result<u64> {
    let url = format!("{}/{}", f.store, f.info.url);
    let (r, mut w) = io::pipe().map_err(|e| Error::io("cannot create a pipe", e))?;
    thread::scope(|scope| {
        let got = scope.spawn(session::bind(move || {
            http::get(&url, &[], limits, false, &mut w)
        }));
        let used = http::decompress_reader(Box::new(r), u64::MAX).and_then(|mut r| use_(&mut r));
        let got = got.join().unwrap_or_else(|p| panic::resume_unwind(p));
        got?;
        used
    })
}

/// The closure of `roots`, each path after its references.
fn post_order(
    roots: &[StorePath],
    lookup: &dyn Fn(&StorePath) -> Option<Arc<Fetched>>,
) -> Result<Vec<Arc<Fetched>>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<(StorePath, bool)> = roots.iter().map(|r| (r.clone(), false)).collect();
    while let Some((p, expanded)) = stack.pop() {
        let f = lookup(&p).ok_or_else(|| {
            Error::Plan(format!(
                "'{p}' is in a fetched closure, but its narinfo is not"
            ))
        })?;
        if expanded {
            out.push(f);
            continue;
        }
        if !seen.insert(p.clone()) {
            continue;
        }
        stack.push((p, true));
        for r in &f.info.references {
            if !seen.contains(r) {
                stack.push((r.clone(), false));
            }
        }
    }
    Ok(out)
}

/// Every NAR into a temporary file, in the order of `paths`.
fn download(paths: &[Arc<Fetched>]) -> Result<Vec<TempPath>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let limits = Limits::new(nix_conf(&Session::current()))?;
    let mut by_size: Vec<usize> = (0..paths.len()).collect();
    by_size.sort_by_key(|&i| Reverse(paths[i].info.file_size));
    let jobs = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(MAX_JOBS)
        .min(paths.len());
    let next = AtomicUsize::new(0);
    let done: Vec<Mutex<Option<Result<TempPath>>>> =
        paths.iter().map(|_| Mutex::new(None)).collect();
    thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(session::bind(|| {
                while let Some(&i) = by_size.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let r = one(&paths[i], limits);
                    let failed = r.is_err();
                    *done[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                    if failed {
                        next.store(by_size.len(), Ordering::Relaxed);
                    }
                }
            }));
        }
    });
    // a failure cancels the downloads not yet started, so only a failure
    // leaves a slot empty
    let mut out = Vec::with_capacity(paths.len());
    for m in done {
        if let Some(r) = m.into_inner().unwrap_or_else(|e| e.into_inner()) {
            out.push(r?);
        }
    }
    Ok(out)
}

fn one(f: &Fetched, limits: Limits) -> Result<TempPath> {
    let url = format!("{}/{}", f.store, f.info.url);
    let mut file =
        NamedTempFile::new().map_err(|e| Error::io("cannot create a temporary file", e))?;
    http::get(&url, &[], limits, false, file.as_file_mut())?;
    Ok(file.into_temp_path())
}
