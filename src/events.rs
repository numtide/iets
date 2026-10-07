//! Instrumentation: one JSON object per line, naming what an evaluation did.
//! The `IETS_STATS` counters say how much; these say which path, from where,
//! and how big. `IETS_EVENTS` names the groups a run watches and
//! `IETS_EVENTS_TO` where they go; a host calls [`Events::watch`] and
//! [`Events::write_to`] instead (docs/reference/events.md).
//!
//! The stream is for a program to read: every record has an `event` name and
//! keeps the fields it was published with. Adding a field is not a change a
//! reader must follow.

use std::cell::RefCell;
use std::io::{self, Write};
use std::path::Path;
use std::str::from_utf8;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::{mem, result};

use crate::settings::Settings;
use crate::{log, print};

thread_local! {
    /// The line a record is built in. A thread keeps it, so a record in a
    /// watched run allocates nothing.
    static LINE: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// A part of the evaluator, and the unit a run turns on. A group costs what
/// its rate class costs, so `all` takes the cold ones and a warm one is
/// named or left out (docs/reference/events.md).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Syntax,
    Ir,
    Import,
    Store,
    Drv,
    Fetch,
    Flake,
    Cache,
    Io,
    Build,
    Lint,
    Error,
}

impl Group {
    /// Every group, in the order `iets help events` lists them.
    pub const ALL: [Group; 12] = [
        Group::Syntax,
        Group::Ir,
        Group::Import,
        Group::Store,
        Group::Drv,
        Group::Fetch,
        Group::Flake,
        Group::Cache,
        Group::Io,
        Group::Build,
        Group::Lint,
        Group::Error,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Group::Syntax => "syntax",
            Group::Ir => "ir",
            Group::Import => "import",
            Group::Store => "store",
            Group::Drv => "drv",
            Group::Fetch => "fetch",
            Group::Flake => "flake",
            Group::Cache => "cache",
            Group::Io => "io",
            Group::Build => "build",
            Group::Lint => "lint",
            Group::Error => "error",
        }
    }

    /// A group whose rate makes it worth naming: `all` leaves it out.
    pub fn warm(self) -> bool {
        matches!(self, Group::Drv | Group::Io)
    }

    fn bit(self) -> u32 {
        1 << self as u32
    }

    /// The mask `names` asks for: a comma-separated list, or `all` for every
    /// cold group. The error names what was not understood, because a
    /// mistyped group would otherwise watch nothing in silence.
    pub fn mask(names: &str) -> result::Result<u32, String> {
        let mut mask = 0;
        for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if name == "all" {
                mask |= Group::ALL
                    .iter()
                    .filter(|g| !g.warm())
                    .fold(0, |m, g| m | g.bit());
                continue;
            }
            match Group::ALL.iter().find(|g| g.name() == name) {
                Some(g) => mask |= g.bit(),
                None => {
                    let all: Vec<&str> = Group::ALL.iter().map(|g| g.name()).collect();
                    return Err(format!(
                        "'{name}' is not an event group (all, {})",
                        all.join(", ")
                    ));
                }
            }
        }
        Ok(mask)
    }
}

/// Where one session's events go, and which groups it wants. No group is
/// the normal case: nothing asked for them, so no hook builds a record.
#[derive(Default)]
pub struct Events {
    groups: AtomicU32,
    sink: OnceLock<Option<Mutex<Box<dyn Write + Send>>>>,
}

impl Events {
    /// Watches the groups in `mask` ([`Group::mask`] parses the names).
    pub fn watch(&self, mask: u32) {
        self.groups.store(mask, Ordering::Relaxed);
    }

    /// Sends every event to `w`.
    pub fn write_to(&self, w: Box<dyn Write + Send>) {
        let _ = self.sink.set(Some(Mutex::new(w)));
    }

    /// Watches what `IETS_EVENTS` names, where `IETS_EVENTS_TO` says. A path
    /// that does not open is reported once and the stream stays off, as a
    /// `json-log-path` that does not open is.
    pub fn open(&self, settings: &Settings) {
        if settings.events == 0 {
            return;
        }
        self.watch(settings.events);
        let path = settings.events_to.as_deref().unwrap_or("-");
        if path == "-" {
            self.write_to(Box::new(io::stderr()));
            return;
        }
        match log::open_sink(Path::new(path)) {
            Ok(w) => self.write_to(w),
            Err(e) => errln!(
                "error (ignored): opening events file \"{path}\": {}",
                crate::error::io_text(&e)
            ),
        }
    }

    /// Whether `group` is watched. A site that must gather something before
    /// it can fill a record asks this first.
    pub fn on(&self, group: Group) -> bool {
        self.groups.load(Ordering::Relaxed) & group.bit() != 0
    }

    /// A builder for one `{"event": NAME, …}` line, and `None` when the
    /// group is not watched: a call site asks first and builds nothing for a
    /// run that does not want it. [`Rec`] writes the line when it drops.
    pub fn rec<'a>(&'a self, group: Group, event: &str) -> Option<Rec<'a>> {
        if !self.on(group) {
            return None;
        }
        let Some(Some(sink)) = self.sink.get() else {
            return None;
        };
        let mut line = LINE.with(|b| mem::take(&mut *b.borrow_mut()));
        line.clear();
        line.extend_from_slice(b"{\"event\":");
        push_str(&mut line, event.as_bytes());
        Some(Rec { sink, line })
    }
}

/// `s` as a JSON string. Invalid UTF-8 is replaced, because a path is bytes
/// and a record is never worth failing a run over.
fn push_str(out: &mut Vec<u8>, s: &[u8]) {
    let lossy = String::from_utf8_lossy(s);
    // the only error is invalid UTF-8, which `lossy` has already replaced
    let _ = print::json_escape(out, lossy.as_bytes());
}

/// How one value appears in a record: it writes a whole JSON value, and
/// nothing else about the record is its business. The types of the domain
/// implement this next to themselves, so this module knows nothing about
/// store paths, source trees or positions.
pub trait Field {
    fn write(self, out: &mut Vec<u8>);
}

impl Field for u64 {
    fn write(self, out: &mut Vec<u8>) {
        push_u64(out, self);
    }
}

impl Field for bool {
    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(if self { b"true" } else { b"false" });
    }
}

impl Field for &str {
    fn write(self, out: &mut Vec<u8>) {
        push_str(out, self.as_bytes());
    }
}

impl Field for &[u8] {
    fn write(self, out: &mut Vec<u8>) {
        push_str(out, self);
    }
}

/// Quotes what a [`Field`] appended from `start`, so a value built out of
/// several pieces needs no buffer of its own. A value with nothing to
/// escape — nearly every path and name — copies nothing and inserts two
/// bytes; one that needs escaping pays a copy.
pub fn quote_in_place(out: &mut Vec<u8>, start: usize) {
    let plain = out[start..]
        .iter()
        .all(|&b| b >= 0x20 && b != b'"' && b != b'\\')
        && from_utf8(&out[start..]).is_ok();
    if plain {
        out.insert(start, b'"');
        out.push(b'"');
        return;
    }
    let raw = out.split_off(start);
    push_str(out, &raw);
}

/// One record under construction. Each field appends to a line the thread
/// owns; the line reaches the sink in one write when this drops, because a
/// write per fragment cost 6% of a nixpkgs evaluation's instructions.
pub struct Rec<'a> {
    sink: &'a Mutex<Box<dyn Write + Send>>,
    line: Vec<u8>,
}

impl Rec<'_> {
    /// One field. Chains, so a call site reads as the record it writes.
    pub fn set<F: Field>(&mut self, key: &str, value: F) -> &mut Self {
        self.line.push(b',');
        push_str(&mut self.line, key.as_bytes());
        self.line.push(b':');
        value.write(&mut self.line);
        self
    }
}

pub fn push_u64(out: &mut Vec<u8>, mut n: u64) {
    let mut d = [0u8; 20];
    let mut i = d.len();
    loop {
        i -= 1;
        d[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&d[i..]);
}

impl Drop for Rec<'_> {
    fn drop(&mut self) {
        self.line.extend_from_slice(b"}\n");
        let mut w = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.write_all(&self.line);
        drop(w);
        let line = mem::take(&mut self.line);
        LINE.with(|b| *b.borrow_mut() = line);
    }
}
