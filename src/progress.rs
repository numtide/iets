//! The activities running now, from iets, the daemon or a child nix. A
//! terminal gets a status line (progress-bar.cc), a pipe gets a `⏵` line
//! after [`FIRST`] and every [`HEARTBEAT`]. Under internal-json the records
//! carry it and nothing is drawn.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::{self, Builder};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::build::event::fmt_duration;
use crate::build::render;
use crate::log::{
    self, ACT_BUILD, ACT_BUILDS, ACT_COPY_PATH, ACT_COPY_PATHS, ACT_FETCH_TREE, ACT_FILE_TRANSFER,
    ACT_QUERY_PATH_INFO, ACT_REALISE, ACT_SUBSTITUTE, LVL_INFO,
};
use crate::session::{self, Session};

pub const FIRST: Duration = Duration::from_secs(5);
pub const HEARTBEAT: Duration = render::HEARTBEAT;
const STALL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Off,
    Bar,
    Lines,
}

struct Task {
    ty: u64,
    text: String,
    parent: u64,
    started: Instant,
    done: u64,
    expected: u64,
    /// Only a task that reports progress can be called stalled.
    measured: bool,
    moved: Instant,
    /// What the task's own tool prints, such as git's `Receiving objects`.
    detail: String,
    told: bool,
}

#[derive(Default)]
struct Tasks {
    /// By id, which is start order: ids only grow.
    all: BTreeMap<u64, Task>,
    ticking: bool,
    /// A child that may prompt on the terminal runs (processes.cc:338).
    suspended: usize,
    beat: Option<Instant>,
}

#[derive(Default)]
pub struct Board {
    mode: AtomicU8,
    tasks: Mutex<Tasks>,
}

impl Board {
    pub fn set_mode(&self, m: Mode) {
        self.mode.store(m as u8, Ordering::Relaxed);
    }

    pub fn mode(&self) -> Mode {
        match self.mode.load(Ordering::Relaxed) {
            1 => Mode::Bar,
            2 => Mode::Lines,
            _ => Mode::Off,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Tasks> {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The line logging.cc:SimpleLogger::startActivity prints, for a pipe
    /// under a Nix name. The build renderer reports builds.
    pub fn start(&self, id: u64, lvl: u64, ty: u64, text: &str, parent: u64) -> Option<String> {
        let now = Instant::now();
        let mut t = self.lock();
        t.all.insert(
            id,
            Task {
                ty,
                text: text.to_string(),
                parent,
                started: now,
                done: 0,
                expected: 0,
                moved: now,
                measured: false,
                detail: String::new(),
                told: false,
            },
        );
        if !t.ticking {
            t.ticking = true;
            spawn_ticker();
        }
        let classic = self.mode() == Mode::Lines && log::classic();
        (classic && lvl <= LVL_INFO && !text.is_empty() && !is_build(ty))
            .then(|| format!("{text}..."))
    }

    /// logging.hh:resProgress: done, expected, running, failed.
    pub fn result(&self, id: u64, ty: u64, f: &[Value]) {
        if ty != log::RES_PROGRESS {
            return;
        }
        let n = |i: usize| f.get(i).and_then(Value::as_u64).unwrap_or(0);
        if let Some(task) = self.lock().all.get_mut(&id) {
            if n(0) != task.done {
                task.moved = Instant::now();
            }
            task.measured = true;
            task.done = n(0);
            task.expected = n(1);
        }
    }

    pub fn detail(&self, id: u64, text: &str) {
        if let Some(task) = self.lock().all.get_mut(&id)
            && task.detail != text
        {
            task.moved = Instant::now();
            task.detail = text.to_string();
        }
    }

    pub fn suspend(&self) {
        self.lock().suspended += 1;
    }

    pub fn resume(&self) {
        let mut t = self.lock();
        t.suspended = t.suspended.saturating_sub(1);
    }

    /// Whether that was the last task: the status line must come down now,
    /// not a tick later, or the next output puts it back.
    pub fn stop(&self, id: u64) -> bool {
        let mut t = self.lock();
        t.all.remove(&id);
        t.all.is_empty()
    }

    /// `None` once nothing runs, which ends the ticker.
    fn tick(&self, now: Instant) -> Option<Tick> {
        let mut t = self.lock();
        if t.all.is_empty() {
            t.ticking = false;
            return None;
        }
        match self.mode() {
            Mode::Bar if t.suspended > 0 => Some(Tick::Bar(String::new())),
            Mode::Bar => Some(Tick::Bar(summary(&t.all, now, true).unwrap_or_default())),
            Mode::Lines if log::classic() => Some(Tick::Quiet),
            Mode::Lines => {
                let due = t.beat.is_none_or(|b| now - b >= HEARTBEAT);
                let new = t
                    .all
                    .values()
                    .any(|k| !k.told && !is_build(k.ty) && now - k.started >= FIRST);
                let old = t
                    .all
                    .values()
                    .any(|k| !is_build(k.ty) && now - k.started >= FIRST);
                if !(new || (due && old)) {
                    return Some(Tick::Quiet);
                }
                let Some(line) = summary(&t.all, now, false) else {
                    return Some(Tick::Quiet);
                };
                t.beat = Some(now);
                for k in t.all.values_mut() {
                    k.told = true;
                }
                Some(Tick::Line(line))
            }
            Mode::Off => Some(Tick::Quiet),
        }
    }
}

enum Tick {
    Bar(String),
    Line(String),
    Quiet,
}

fn spawn_ticker() {
    let run = session::bind(|| {
        loop {
            let s = Session::current();
            let pause = match s.log.progress.mode() {
                Mode::Bar => Duration::from_millis(250),
                _ => Duration::from_secs(1),
            };
            thread::sleep(pause);
            match s.log.progress.tick(Instant::now()) {
                None => {
                    s.log.set_status(String::new());
                    return;
                }
                Some(Tick::Bar(line)) => s.log.set_status(line),
                Some(Tick::Line(line)) => s.log.errln(&line),
                Some(Tick::Quiet) => {}
            }
        }
    });
    let _ = Builder::new().name("progress".into()).spawn(run);
}

/// The build renderer reports these in a log.
fn is_build(ty: u64) -> bool {
    matches!(ty, ACT_BUILD | ACT_BUILDS | ACT_REALISE)
}

fn counts_bytes(ty: u64) -> bool {
    matches!(ty, ACT_COPY_PATH | ACT_FILE_TRANSFER)
}

fn verb(ty: u64) -> &'static str {
    match ty {
        ACT_BUILD => "building",
        ACT_COPY_PATH | ACT_COPY_PATHS => "copying",
        ACT_FILE_TRANSFER => "downloading",
        ACT_FETCH_TREE => "fetching",
        ACT_SUBSTITUTE => "substituting",
        ACT_QUERY_PATH_INFO => "querying",
        _ => "running",
    }
}

/// A task under another task with a text is folded into it, as a download
/// into the copy that asked for it.
fn shown(all: &BTreeMap<u64, Task>, k: &Task) -> bool {
    if k.text.is_empty() {
        return false;
    }
    let mut up = k.parent;
    for _ in 0..16 {
        match all.get(&up) {
            Some(p) if !p.text.is_empty() => return false,
            Some(p) => up = p.parent,
            None => return true,
        }
    }
    true
}

/// One group per type: a single task by its text, several by their count.
fn summary(all: &BTreeMap<u64, Task>, now: Instant, builds: bool) -> Option<String> {
    let mut groups: Vec<(u64, Vec<&Task>)> = Vec::new();
    for k in all.values().filter(|k| shown(all, k)) {
        if !builds && is_build(k.ty) {
            continue;
        }
        match groups.iter_mut().find(|(ty, _)| *ty == k.ty) {
            Some((_, g)) => g.push(k),
            None => groups.push((k.ty, vec![k])),
        }
    }
    if groups.is_empty() {
        return None;
    }
    let parts: Vec<String> = groups
        .iter()
        .map(|(ty, g)| match g.as_slice() {
            [k] => one(k, now),
            many => {
                let mut s = format!("{} {}", many.len(), verb(*ty));
                if counts_bytes(*ty) {
                    s.push_str(&format!(
                        ", {}",
                        crate::cli::render::size(many.iter().map(|k| k.done).sum())
                    ));
                }
                s
            }
        })
        .collect();
    Some(format!("⏵ {}", parts.join(" · ")))
}

fn one(k: &Task, now: Instant) -> String {
    let mut s = short(&k.text);
    if k.expected > 0 && counts_bytes(k.ty) {
        s.push_str(&format!(
            ", {} of {}",
            crate::cli::render::size(k.done),
            crate::cli::render::size(k.expected)
        ));
    } else if counts_bytes(k.ty) && k.done > 0 {
        s.push_str(&format!(", {}", crate::cli::render::size(k.done)));
    } else if k.expected > 0 {
        s.push_str(&format!(", {}/{}", k.done, k.expected));
    }
    if !k.detail.is_empty() {
        s.push_str(&format!(", {}", k.detail));
    }
    let age = now - k.started;
    let quiet = now - k.moved;
    let watched = k.measured || !k.detail.is_empty();
    match (age >= Duration::from_secs(1), watched && quiet >= STALL) {
        (_, true) => s.push_str(&format!(
            " ({}, no data for {})",
            fmt_duration(age),
            fmt_duration(quiet)
        )),
        (true, false) => s.push_str(&format!(" ({})", fmt_duration(age))),
        (false, false) => {}
    }
    s
}

/// Longer quoted names keep only their ends, so the numbers after them
/// still fit on the line.
const QUOTED: usize = 40;

fn short(text: &str) -> String {
    let text = unhashed(text);
    let mut out = String::with_capacity(text.len());
    for (i, part) in text.split('\'').enumerate() {
        if i > 0 {
            out.push('\'');
        }
        if i % 2 == 0 || part.chars().count() <= QUOTED {
            out.push_str(part);
            continue;
        }
        let part = part.split_once("://").map_or(part, |(_, p)| p);
        let n = part.chars().count();
        out.extend(part.chars().take(20));
        out.push('…');
        out.extend(part.chars().skip(n.saturating_sub(QUOTED - 21)));
    }
    out
}

fn unhashed(text: &str) -> String {
    const STORE: &str = "/nix/store/";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(STORE) {
        out.push_str(&rest[..i]);
        let after = &rest[i + STORE.len()..];
        let hashed = after.len() > 33
            && after.as_bytes()[32] == b'-'
            && after[..32].bytes().all(|b| b.is_ascii_alphanumeric());
        match hashed {
            true => rest = &after[33..],
            false => {
                out.push_str(STORE);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(ty: u64, text: &str, parent: u64, age: u64) -> Task {
        let at = Instant::now() - Duration::from_secs(age);
        Task {
            ty,
            text: text.into(),
            parent,
            started: at,
            done: 0,
            expected: 0,
            moved: at,
            measured: false,
            detail: String::new(),
            told: false,
        }
    }

    #[test]
    fn store_hashes_are_left_out() {
        assert_eq!(
            short(
                "copying path '/nix/store/0123456789abcdfghijklmnpqrsvwxyz-hello-2.12' from 'https://cache.nixos.org'"
            ),
            "copying path 'hello-2.12' from 'https://cache.nixos.org'"
        );
        assert_eq!(short("/nix/store/x"), "/nix/store/x");
        assert_eq!(
            short(
                "downloading 'https://github.com/NixOS/nixpkgs/archive/66bbe65cae88eb900d5b3236bc1b07fa47a09c12.tar.gz'"
            ),
            "downloading 'github.com/NixOS/nix…07fa47a09c12.tar.gz'"
        );
    }

    #[test]
    fn a_download_inside_a_copy_is_part_of_the_copy() {
        let mut all = BTreeMap::new();
        all.insert(1, task(ACT_SUBSTITUTE, "", 0, 3));
        all.insert(2, task(ACT_COPY_PATH, "copying path 'a'", 1, 3));
        all.insert(3, task(ACT_FILE_TRANSFER, "downloading 'u'", 2, 3));
        let s = summary(&all, Instant::now(), true).unwrap();
        assert_eq!(s, "⏵ copying path 'a' (3s)");
    }

    #[test]
    fn several_of_a_kind_are_counted() {
        let mut all = BTreeMap::new();
        let mut a = task(ACT_FILE_TRANSFER, "downloading 'a'", 0, 0);
        a.done = 1 << 20;
        a.moved = Instant::now();
        let mut b = task(ACT_FILE_TRANSFER, "downloading 'b'", 0, 0);
        b.done = 1 << 20;
        b.moved = Instant::now();
        all.insert(1, a);
        all.insert(2, b);
        all.insert(3, task(ACT_BUILD, "building 'x'", 0, 0));
        assert_eq!(
            summary(&all, Instant::now(), true).unwrap(),
            "⏵ 2 downloading, 2.0 MiB · building 'x'"
        );
        assert_eq!(
            summary(&all, Instant::now(), false).unwrap(),
            "⏵ 2 downloading, 2.0 MiB"
        );
    }

    #[test]
    fn a_transfer_that_does_not_move_says_so() {
        let mut all = BTreeMap::new();
        let mut k = task(ACT_FILE_TRANSFER, "downloading 'u'", 0, 45);
        k.done = 2048;
        k.measured = true;
        all.insert(1, k);
        assert_eq!(
            summary(&all, Instant::now(), false).unwrap(),
            "⏵ downloading 'u', 2.0 KiB (45s, no data for 45s)"
        );
    }

    #[test]
    fn a_log_hears_of_a_task_after_first_and_then_every_heartbeat() {
        let b = Board::default();
        b.set_mode(Mode::Lines);
        b.lock()
            .all
            .insert(1, task(ACT_FETCH_TREE, "fetching 'x'", 0, 1));
        b.lock().ticking = true;
        let now = Instant::now();
        assert!(matches!(b.tick(now), Some(Tick::Quiet)));
        let later = now + FIRST;
        let mut young = task(ACT_FILE_TRANSFER, "downloading 'y'", 0, 0);
        young.started = later - Duration::from_secs(4);
        b.lock().all.insert(2, young);
        let first = "⏵ fetching 'x' (6s) · downloading 'y' (4s)";
        assert!(matches!(b.tick(later), Some(Tick::Line(l)) if l == first));
        assert!(matches!(
            b.tick(later + Duration::from_secs(1)),
            Some(Tick::Quiet)
        ));
        assert!(matches!(b.tick(later + HEARTBEAT), Some(Tick::Line(_))));
        b.stop(1);
        b.stop(2);
        assert!(b.tick(later + HEARTBEAT).is_none());
    }
}
