//! Where an evaluation's time went, as a sampled flame graph.
//!
//! A tick thread asks for a sample and the call path tests one relaxed load,
//! so no clock is read per call. Each fiber samples its own shadow stack on
//! the worker that owns it, and never reads another fiber's. The output is
//! the folded form `frame;frame <count>` that `flamegraph.pl` and every
//! compatible tool read (docs/reference/profile.md).
//!
//! The shadow stack is two stores per call. A `hello` evaluation with the
//! sampler compiled in and no profile asked for is inside the run-to-run
//! spread of one without it, so there is no feature gate: `IETS_PROFILE` is
//! the only switch.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, Builder};
use std::time::Duration;

use crate::eval::error::Pos;
use crate::log;
use crate::parallel::interner::Interner;
use crate::parallel::shared::Files;
use crate::session::Session;
use crate::syntax::symbol::Sym;

/// Frames below this are left out of a sample, so hashing one stays bounded.
/// A nixpkgs attribute reaches about 200.
pub const MAX_DEPTH: usize = 512;

/// What a sample stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One sample, so a stack's count is the time it was on CPU.
    Time,
    /// The arena bytes allocated since the last sample, so a stack's count
    /// is the memory it built.
    Bytes,
}

impl Mode {
    /// `IETS_PROFILE`, or `None` when the word names no profile.
    pub fn parse(name: &str) -> Option<Mode> {
        match name {
            "eval" => Some(Mode::Time),
            "alloc" => Some(Mode::Bytes),
            _ => None,
        }
    }
}

/// Set on the low word of a frame whose callee is a builtin, so a primop
/// name and a lambda name are never read for each other.
const PRIMOP: u32 = 1 << 31;

/// Low word of a frame whose lambda has no name.
const ANON: u32 = !PRIMOP;

/// A call site and the lambda it entered, in one word: both halves are
/// already interned, and `Profile::write` turns them into text at the end
/// instead of while the samples are collected.
#[inline]
pub fn frame(pos: Pos, name: Option<Sym>) -> u64 {
    ((pos.0 as u64) << 32) | name.map_or(ANON, |s| s.0) as u64
}

/// A call site and the builtin it entered.
#[inline]
pub fn primop_frame(pos: Pos, sym: Sym) -> u64 {
    ((pos.0 as u64) << 32) | (sym.0 | PRIMOP) as u64
}

/// One fiber's shadow stack. Pushed and popped by the call path, read by
/// nothing else.
pub struct Stack {
    frames: Vec<u64>,
    last_tick: u64,
}

/// A fiber made from another starts its own stack: `Evaluator` is `Clone`
/// and the clone runs elsewhere, so copying the frames would blame the new
/// fiber for its maker's call chain.
impl Clone for Stack {
    fn clone(&self) -> Stack {
        Stack::default()
    }
}

impl Default for Stack {
    fn default() -> Stack {
        Stack {
            frames: Vec::with_capacity(1024),
            last_tick: 0,
        }
    }
}

impl Stack {
    /// Enters `f`. The whole cost of a call that is not sampled.
    #[inline]
    pub fn enter(&mut self, f: u64) {
        self.frames.push(f);
    }

    /// Whether the tick thread has asked for a sample since this fiber's
    /// last. One relaxed load and a compare, which is what a call pays.
    #[inline]
    pub fn due(&mut self, p: &Profile) -> bool {
        p.tick.load(Ordering::Relaxed) != self.last_tick
    }

    /// Credits this fiber's stack with `bytes`, or with one sample when the
    /// profile weighs time. Cold: 99 times a second per fiber on CPU.
    pub fn take(&mut self, p: &Profile, bytes: u64) {
        self.last_tick = p.tick.load(Ordering::Relaxed);
        let w = if p.by_bytes.load(Ordering::Relaxed) {
            bytes
        } else {
            1
        };
        let n = self.frames.len().min(MAX_DEPTH);
        p.sample(&self.frames[..n], w);
    }

    pub fn frames(&self) -> &[u64] {
        &self.frames
    }

    #[inline]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Returns to `depth`. A call that failed left its frames behind, and
    /// `tryEval` goes on evaluating, so the unwind is by depth and not by a
    /// pop per frame.
    #[inline]
    pub fn unwind(&mut self, depth: usize) {
        self.frames.truncate(depth);
    }
}

/// One run's samples. Lives on the session, so two evaluators in one process
/// each profile their own run.
#[derive(Default)]
pub struct Profile {
    /// Weighs a sample by the arena bytes since the one before it, instead
    /// of by one for the time it stood for.
    by_bytes: AtomicBool,
    /// Bumped by the tick thread; compared by the call path.
    tick: AtomicU64,
    stop: AtomicBool,
    folded: Mutex<HashMap<Box<[u64]>, u64>>,
    /// Set by the first evaluator that profiles, for `write` to symbolise
    /// with after the evaluation is gone.
    names: OnceLock<(Files, Interner)>,
    /// Samples a second the tick thread asks for.
    hz: AtomicU64,
    /// Where the folded lines go. `-` is stderr.
    to: OnceLock<String>,
}

impl Profile {
    /// Whether this run profiles. A sample is never taken when it does not,
    /// because the tick stays at zero.
    pub fn on(&self) -> bool {
        self.to.get().is_some()
    }

    /// What `write` needs to turn a frame into text.
    pub fn names(&self, files: &Files, interner: &Interner) {
        let _ = self.names.set((files.clone(), interner.clone()));
    }

    fn sample(&self, frames: &[u64], weight: u64) {
        let Ok(mut g) = self.folded.lock() else {
            return;
        };
        *g.entry(frames.into()).or_insert(0) += weight;
    }

    /// Writes the folded lines, and stops the tick thread. Called once, when
    /// the run has finished.
    pub fn write(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(to) = self.to.get() else { return };
        let Ok(g) = self.folded.lock() else { return };
        if g.is_empty() {
            errln!(
                "profile: no samples at {} Hz, so the evaluation was shorter than one tick",
                self.hz.load(Ordering::Relaxed)
            );
            return;
        }
        let mut out = Vec::new();
        for (stack, count) in g.iter() {
            let mut first = true;
            for f in stack.iter() {
                if !first {
                    out.push(b';');
                }
                first = false;
                out.extend_from_slice(self.describe(*f).as_bytes());
            }
            let _ = writeln!(out, " {count}");
        }
        if to == "-" {
            let _ = io::stderr().write_all(&out);
            return;
        }
        match log::open_sink(Path::new(to)) {
            Ok(mut w) => {
                let _ = w.write_all(&out);
            }
            Err(e) => errln!(
                "error (ignored): opening profile file \"{to}\": {}",
                crate::error::io_text(&e)
            ),
        }
    }

    /// `file:line:col:name`, and no name for an anonymous lambda. A frame a
    /// flame graph tool reads must hold no `;` and no space; a Nix path can
    /// hold either, so both are replaced.
    fn describe(&self, f: u64) -> String {
        let pos = Pos((f >> 32) as u32);
        let low = f as u32;
        let Some((files, interner)) = self.names.get() else {
            return format!("pos{}", pos.0);
        };
        let at = match files.file_of(pos) {
            Some((file, off)) => {
                let (l, c) = file.line_col(off);
                format!("{}:{l}:{c}", String::from_utf8_lossy(&file.path))
            }
            None => "«none»".to_string(),
        };
        let mut s = match low {
            l if l & PRIMOP != 0 => {
                format!("{at}:«{}»", interner.name_str(Sym(l & !PRIMOP)))
            }
            ANON => at,
            sym => format!("{at}:{}", interner.name_str(Sym(sym))),
        };
        if s.contains(';') || s.contains(' ') {
            s = s.replace(';', ",").replace(' ', "_");
        }
        s
    }
}

/// Starts profiling this session, when it asked for it. The tick thread
/// holds the session: it ends on the first tick after [`Profile::write`].
pub fn open(session: &Arc<Session>) {
    let s = session.settings();
    let Some(mode) = s.profile else {
        return;
    };
    let hz = s.profile_hz.max(1);
    session.profile.hz.store(hz as u64, Ordering::Relaxed);
    session
        .profile
        .by_bytes
        .store(mode == Mode::Bytes, Ordering::Relaxed);
    let _ = session
        .profile
        .to
        .set(s.profile_to.clone().unwrap_or_else(|| "-".to_string()));
    let own = session.clone();
    let _ = Builder::new().name("iets-profile".into()).spawn(move || {
        let period = Duration::from_micros(1_000_000 / hz as u64);
        while !own.profile.stop.load(Ordering::Relaxed) {
            thread::sleep(period);
            own.profile.tick.fetch_add(1, Ordering::Relaxed);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call the way the evaluator enters one: push, then sample if a tick passed.
    fn call(s: &mut Stack, p: &Profile, frame: u64, bytes: u64) {
        s.enter(frame);
        if s.due(p) {
            s.take(p, bytes);
        }
    }

    fn folded(p: &Profile) -> Vec<(Vec<u64>, u64)> {
        let g = p.folded.lock().unwrap();
        let mut v: Vec<_> = g.iter().map(|(k, c)| (k.to_vec(), *c)).collect();
        v.sort();
        v
    }

    #[test]
    fn a_frame_keeps_its_call_site_and_its_name() {
        let f = frame(Pos(7), Some(Sym(3)));
        assert_eq!(f >> 32, 7);
        assert_eq!(f as u32, 3);
        assert_eq!(frame(Pos(7), None) as u32, ANON);
    }

    #[test]
    fn a_sample_is_taken_once_per_tick() {
        let p = Profile::default();
        let mut s = Stack::default();
        p.tick.store(1, Ordering::Relaxed);
        call(&mut s, &p, 10, 0);
        call(&mut s, &p, 20, 0);
        assert_eq!(folded(&p), vec![(vec![10], 1)]);
        p.tick.store(2, Ordering::Relaxed);
        call(&mut s, &p, 30, 0);
        assert_eq!(folded(&p), vec![(vec![10], 1), (vec![10, 20, 30], 1)]);
    }

    #[test]
    fn an_unwind_returns_to_a_depth() {
        let p = Profile::default();
        let mut s = Stack::default();
        let d = s.depth();
        call(&mut s, &p, 10, 0);
        call(&mut s, &p, 20, 0);
        s.unwind(d);
        assert_eq!(s.depth(), 0);
        p.tick.store(1, Ordering::Relaxed);
        call(&mut s, &p, 30, 0);
        assert_eq!(folded(&p), vec![(vec![30], 1)]);
    }

    #[test]
    fn a_deep_stack_is_capped() {
        let p = Profile::default();
        let mut s = Stack::default();
        for i in 0..(MAX_DEPTH as u64 + 10) {
            call(&mut s, &p, i, 0);
            p.tick.store(i + 1, Ordering::Relaxed);
        }
        assert!(folded(&p).iter().all(|(k, _)| k.len() <= MAX_DEPTH));
    }

    #[test]
    fn a_weight_is_bytes_when_the_profile_asks_for_them() {
        let p = Profile::default();
        p.by_bytes.store(true, Ordering::Relaxed);
        let mut s = Stack::default();
        p.tick.store(1, Ordering::Relaxed);
        call(&mut s, &p, 10, 4096);
        assert_eq!(folded(&p), vec![(vec![10], 4096)]);
    }
}
