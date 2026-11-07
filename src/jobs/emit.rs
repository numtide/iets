//! The last stage: lines to stdout, with `--constituents` withholding the
//! aggregates until the walk is over (nix-eval-jobs.cc:emitResponse).

use std::io::{self, Write};
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
    /// Nothing more is written: the reader closed the pipe
    /// (`iets eval-jobs | head`), and the frontend stops with exit 0, or a
    /// write failed (`failed`).
    pub closed: bool,
    /// A write failed with something other than a closed pipe (a full disk):
    /// the output is incomplete, so the frontend stops with this error.
    pub failed: Option<io::Error>,
}

fn write_line(out: &mut dyn Write, line: &Line) -> io::Result<()> {
    out.write_all(&line.render())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Writes `line` unless writing has stopped, and stops it at the first error.
fn put(out: &mut dyn Write, closed: &mut bool, failed: &mut Option<io::Error>, line: &Line) {
    if *closed {
        return;
    }
    if let Err(e) = write_line(out, line) {
        *closed = true;
        if e.kind() != io::ErrorKind::BrokenPipe {
            *failed = Some(e);
        }
    }
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
            failed: None,
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
            put(self.out.as_mut(), &mut self.closed, &mut self.failed, &line);
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
                put(self.out.as_mut(), &mut self.closed, &mut self.failed, &line);
                self.jobs.insert(attr, entry);
            }
        }
    }

    /// `--constituents`: resolve and print the withheld aggregates.
    pub fn finish(&mut self, conn: &mut Connection, gc_roots_dir: Option<&Path>) -> Result<()> {
        if !self.constituents {
            return Ok(());
        }
        let (out, closed, failed) = (&mut self.out, &mut self.closed, &mut self.failed);
        constituents::rewrite(&mut self.jobs, conn, gc_roots_dir, &mut |l| {
            put(out.as_mut(), closed, failed, &l);
        })
    }
}
