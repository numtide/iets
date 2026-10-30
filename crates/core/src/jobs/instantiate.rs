//! The instantiation nix-eval-jobs does inside `derivationStrict`: every
//! job's `.drv` and its input closure registered through the daemon. Here
//! it happens in batches on a thread beside the walk: one
//! `build::upload::StreamUpload` per batch over the worker backend, then
//! `--gc-roots-dir` roots (worker.cc:registerGCRoot) and the batch's lines
//! go on to the sink.

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{Builder, JoinHandle};
use std::time::Instant;

use super::line::{Job, Line};
use crate::build::backend::{Backend, Worker};
use crate::build::upload::{self, StreamUpload, Uploader};
use crate::builtins::derivation::DrvInfo;
use crate::error::{Error, Result};
use crate::eval::Known;
use crate::parallel::shared::SharedMap;
use crate::remote::worker::{Connection, Options, Transport};
use crate::session;
use crate::store::path::StorePath;

/// The most lines per daemon round trip.
pub const BATCH: usize = 256;

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub upload: upload::Stats,
    pub roots: usize,
}

/// The daemon side: one connection and the upload that remembers what it
/// holds.
pub struct Store {
    worker: Worker,
    up: StreamUpload,
    gc_roots_dir: Option<PathBuf>,
    pub stats: Stats,
}

impl Store {
    pub fn open(
        transport: &Transport,
        gc_roots_dir: Option<PathBuf>,
        drvs: SharedMap<StorePath, DrvInfo>,
        sources: SharedMap<StorePath, Known>,
    ) -> Result<Store> {
        let worker = Worker::open(transport, &Options::default(), &mut |_| {})?;
        if let Some(dir) = &gc_roots_dir {
            fs::create_dir_all(dir)
                .map_err(|e| Error::io(format!("cannot create '{}'", dir.display()), e))?;
        }
        Ok(Store {
            worker,
            up: StreamUpload::new(drvs, sources),
            gc_roots_dir,
            stats: Stats::default(),
        })
    }

    /// Registers the closures of `jobs`' drvs that the daemon lacks, then
    /// roots each job's `.drv`.
    pub fn instantiate(&mut self, jobs: &[&Job]) -> Result<()> {
        let t0 = Instant::now();
        let roots: Vec<StorePath> = jobs.iter().map(|j| j.drv_path.clone()).collect();
        let mut st = self.up.upload(&mut self.worker, &roots, &mut |_| {})?;
        if let Some(dir) = &self.gc_roots_dir {
            for j in jobs {
                let root = dir.join(j.drv_path.base_name());
                if fs::symlink_metadata(&root).is_ok() {
                    continue;
                }
                self.worker.add_root(&j.drv_path.to_string(), &root)?;
                self.stats.roots += 1;
            }
        }
        st.time = t0.elapsed();
        self.stats.upload.merge(st);
        Ok(())
    }

    pub fn into_connection(self) -> Connection {
        self.worker.conn
    }
}
/// The first line and what queued behind it, up to `max`.
fn next_batch<T>(rx: &Receiver<T>, max: usize) -> Option<Vec<T>> {
    let mut batch = vec![rx.recv().ok()?];
    while batch.len() < max
        && let Ok(x) = rx.try_recv()
    {
        batch.push(x);
    }
    Some(batch)
}

/// The thread: lines in, instantiated batches out to `sink`, in order.
pub struct Instantiator {
    tx: Option<Sender<Line>>,
    thread: Option<JoinHandle<Result<Store>>>,
}

impl Instantiator {
    pub fn start(
        mut store: Store,
        mut sink: Box<dyn FnMut(Vec<Line>) -> Result<()> + Send>,
    ) -> Instantiator {
        let (tx, rx) = channel::<Line>();
        let thread = Builder::new()
            .name("instantiate".into())
            .stack_size(64 << 20)
            .spawn(session::bind(move || {
                while let Some(lines) = next_batch(&rx, BATCH) {
                    let jobs: Vec<&Job> = lines
                        .iter()
                        .filter_map(|l| match l {
                            Line::Job(j) => Some(j),
                            Line::Error(_) => None,
                        })
                        .collect();
                    if !jobs.is_empty() {
                        store.instantiate(&jobs)?;
                    }
                    sink(lines)?;
                }
                Ok(store)
            }))
            .expect("spawn instantiate thread");
        Instantiator {
            tx: Some(tx),
            thread: Some(thread),
        }
    }

    pub fn push(&self, line: Line) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(line);
        }
    }

    /// Flushes the last batch and returns the store for later use.
    pub fn finish(mut self) -> Result<Store> {
        drop(self.tx.take());
        self.thread
            .take()
            .expect("instantiate thread")
            .join()
            .map_err(|_| Error::Protocol("instantiate thread panicked".into()))?
    }
}
