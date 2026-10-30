//! The last stage: lines to stdout, with `--constituents` withholding the
//! aggregates until the walk is over (nix-eval-jobs.cc:emitResponse).

use std::io::Write;
use std::path::Path;

use super::Stats;
use super::constituents::{self, Entry, Jobs};
use super::line::Line;
use crate::error::Result;
use crate::remote::worker::Connection;

pub struct Emitter {
    out: Box<dyn Write + Send>,
    constituents: bool,
    jobs: Jobs,
    pub stats: Stats,
    /// A fatal error line was seen: nix-eval-jobs stops with exit 1.
    pub fatal: bool,
    /// The reader closed the pipe (`iets eval-jobs | head`). Nothing more
    /// is written; the frontend stops with exit 0.
    pub closed: bool,
}

/// `false` once the reader is gone; that ends the run, it is not an error.
fn write_line(out: &mut dyn Write, line: &Line) -> bool {
    let r = out
        .write_all(&line.render())
        .and_then(|_| out.write_all(b"\n"))
        .and_then(|_| out.flush());
    !matches!(r, Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe)
}

impl Emitter {
    pub fn new(out: Box<dyn Write + Send>, constituents: bool) -> Emitter {
        Emitter {
            out,
            constituents,
            jobs: Jobs::new(),
            stats: Stats::default(),
            fatal: false,
            closed: false,
        }
    }

    pub fn emit(&mut self, line: Line) {
        if self.closed {
            return;
        }
        match &line {
            Line::Job(_) => self.stats.jobs += 1,
            Line::Error(e) => {
                self.stats.errors += 1;
                self.fatal |= e.fatal;
            }
        }
        if !self.constituents {
            self.closed = !write_line(&mut self.out, &line);
            return;
        }
        let entry = match &line {
            Line::Job(j) => Entry {
                drv_path: Some(j.drv_path.clone()),
                error: None,
                held: None,
            },
            Line::Error(e) => Entry {
                drv_path: None,
                error: Some(e.error.clone()),
                held: None,
            },
        };
        let attr = line.attr().to_string();
        match line {
            Line::Job(j) if j.named_constituents.as_ref().is_some_and(|n| !n.is_empty()) => {
                self.jobs.insert(
                    attr,
                    Entry {
                        held: Some(j),
                        ..entry
                    },
                );
            }
            line => {
                self.closed = !write_line(&mut self.out, &line);
                self.jobs.insert(attr, entry);
            }
        }
    }

    /// `--constituents`: resolve and print the withheld aggregates.
    pub fn finish(&mut self, conn: &mut Connection, gc_roots_dir: Option<&Path>) -> Result<()> {
        if !self.constituents {
            return Ok(());
        }
        let (out, closed) = (&mut self.out, &mut self.closed);
        constituents::rewrite(&mut self.jobs, conn, gc_roots_dir, &mut |l| {
            *closed |= !write_line(out.as_mut(), &l);
        })
    }
}
