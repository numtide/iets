//! Where an evaluation's time went, as a sampled flame graph.
//!
//! A tick thread asks for a sample and calls and returns test one relaxed
//! load, so no clock is read per call. A sample is taken before a push or
//! pop, so it goes to the stack that ran. Each fiber samples its own shadow
//! stack on the worker that owns it, and never reads another fiber's. The
//! output is
//! the folded form `frame;frame <count>` that `flamegraph.pl` and every
//! compatible tool read (docs/reference/profile.md).
//!
//! The shadow stack is two stores per call. A `hello` evaluation with the
//! sampler compiled in and no profile asked for is inside the run-to-run
//! spread of one without it, so there is no feature gate: `IETS_PROFILE` is
//! the only switch.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, Builder};
use std::time::Duration;

use crate::compile::ir::{AttrElemIr, BindIr, Ir, IrRef, ParamIr};
use crate::eval::error::Pos;
use crate::eval::ir_pos;
use crate::log;
use crate::parallel::interner::Interner;
use crate::parallel::shared::Files;
use crate::session::Session;
use crate::syntax::symbol::Sym;
use crate::sys::process::thread_cpu_time;

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

/// Set on a frame that is a thunk. The rest of the word is the address of
/// the thunk's IR, which `Profile::write` turns into the binding the thunk
/// came from.
const THUNK: u64 = 0xFFFE << 48;

/// A thunk with no binding, already resolved to the position of its IR at
/// sample time, because the arena the IR lives in is gone when the profile is
/// written.
const THUNK_AT: u64 = 0xFFFD << 48;

/// A `mapAttrs` entry, named by its attribute. The payload is the symbol.
const THUNK_ATTR: u64 = 0xFFFC << 48;

/// A forced `mapAttrs` entry.
#[inline]
pub fn attr_frame(name: Sym) -> u64 {
    THUNK_ATTR | name.0 as u64
}

/// The address or position part of a thunk frame, below the tag.
const PAYLOAD: u64 = (1 << 48) - 1;

#[inline]
fn is_thunk(f: u64) -> bool {
    f >> 48 == 0xFFFE
}

#[inline]
fn is_attr(f: u64) -> bool {
    f >> 48 == 0xFFFC
}

#[inline]
fn is_thunk_at(f: u64) -> bool {
    f >> 48 == 0xFFFD
}

/// A forced thunk, named by the IR it runs.
#[inline]
pub fn thunk_frame(ir: *const Ir) -> u64 {
    THUNK | (ir as u64 & PAYLOAD)
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
    /// Avoids a load through the session when the run does not profile.
    on: bool,
}

/// A fiber made from another starts its own stack: `Evaluator` is `Clone`
/// and the clone runs elsewhere, so copying the frames would blame the new
/// fiber for its maker's call chain.
impl Clone for Stack {
    fn clone(&self) -> Stack {
        Stack {
            on: self.on,
            ..Stack::default()
        }
    }
}

impl Default for Stack {
    fn default() -> Stack {
        Stack {
            frames: Vec::with_capacity(1024),
            on: false,
        }
    }
}

impl Stack {
    /// Created on the evaluating thread, so CPU time before the run is not
    /// counted.
    pub fn sampling() -> Stack {
        cpu_us_since_sample();
        Stack {
            on: true,
            ..Stack::default()
        }
    }

    #[inline]
    pub fn on(&self) -> bool {
        self.on
    }

    /// Enters `f`. The whole cost of a call that is not sampled.
    #[inline]
    pub fn enter(&mut self, f: u64) {
        self.frames.push(f);
    }

    /// Whether a tick passed since the run's last sample.
    #[inline]
    pub fn due(&mut self, p: &Profile) -> bool {
        self.on && p.tick.load(Ordering::Relaxed) != p.sampled.load(Ordering::Relaxed)
    }

    /// Credits this stack with `bytes`, or with `cpu_us` when the profile
    /// weighs time.
    pub fn take(&mut self, p: &Profile, bytes: u64, cpu_us: u64) {
        p.sampled
            .store(p.tick.load(Ordering::Relaxed), Ordering::Relaxed);
        let w = if p.by_bytes.load(Ordering::Relaxed) {
            bytes
        } else {
            cpu_us
        };
        let n = self.frames.len().min(MAX_DEPTH);
        if self.frames[..n].iter().any(|&f| is_thunk(f)) {
            let resolved: Vec<u64> = self.frames[..n].iter().map(|&f| p.resolve(f)).collect();
            p.sample(&resolved, w);
        } else {
            p.sample(&self.frames[..n], w);
        }
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
    /// The tick of the last sample. Per run, not per fiber, so a fiber
    /// switched in on a sampled tick does not take it again.
    sampled: AtomicU64,
    stop: AtomicBool,
    folded: Mutex<HashMap<Box<[u64]>, u64>>,
    /// Set by the first evaluator that profiles, for `write` to symbolise
    /// with after the evaluation is gone.
    names: OnceLock<(Files, Interner)>,
    /// Samples a second the tick thread asks for.
    hz: AtomicU64,
    /// Where the folded lines go. `-` is stderr.
    to: OnceLock<String>,
    /// IR address of a binding's value to `(position, name)`, from the
    /// compiler, so a thunk frame reads as the binding it evaluates.
    binds: Mutex<HashMap<usize, (Pos, Sym)>>,
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

    /// Records the bindings in one file's IR. The IR is the one thing a
    /// compile and a cache load both end with, so walking it covers both.
    pub fn note_ir(&self, root: IrRef) {
        let mut found: Vec<(usize, Pos, Sym)> = Vec::new();
        let mut todo: Vec<IrRef> = vec![root];
        // The IR is a tree, but a cache entry may share a node.
        let mut seen: HashSet<usize> = HashSet::new();
        while let Some(ir) = todo.pop() {
            if !seen.insert(ir as *const Ir as usize) {
                continue;
            }
            let mut bind = |bs: &'static [BindIr]| {
                for b in bs {
                    found.push((b.value as *const Ir as usize, b.pos, b.sym));
                    todo.push(b.value);
                }
            };
            match ir {
                Ir::Attrs(a) => {
                    bind(a.binds);
                    todo.extend(a.from);
                    for d in a.dynamic {
                        todo.push(d.name);
                        todo.push(d.value);
                    }
                }
                Ir::Let(l) => {
                    bind(l.binds);
                    todo.extend(l.from);
                    todo.push(l.body);
                }
                Ir::Lambda(l) => {
                    if let ParamIr::Formals { formals, .. } = &l.param {
                        todo.extend(formals.iter().filter_map(|f| f.default));
                    }
                    todo.push(l.body);
                }
                Ir::Concat { parts, .. } => todo.extend(*parts),
                Ir::SearchPath { name, .. } => todo.push(name),
                Ir::Select(s) | Ir::SelectCount(s) => {
                    todo.push(s.subject);
                    todo.extend(s.default);
                    for e in s.path {
                        if let AttrElemIr::Dynamic(d) = e {
                            todo.push(d);
                        }
                    }
                }
                Ir::Select1 {
                    subject, default, ..
                } => {
                    todo.push(subject);
                    todo.extend(*default);
                }
                Ir::HasAttr { subject, path } => {
                    todo.push(subject);
                    for e in *path {
                        if let AttrElemIr::Dynamic(d) = e {
                            todo.push(d);
                        }
                    }
                }
                Ir::List(xs) => todo.extend(*xs),
                Ir::Apply { func, args, .. } => {
                    todo.push(func);
                    todo.extend(*args);
                }
                Ir::With { scope, body, .. } => {
                    todo.push(scope);
                    todo.push(body);
                }
                Ir::If {
                    cond, then, else_, ..
                } => todo.extend([*cond, *then, *else_]),
                Ir::Assert { cond, body, .. } => todo.extend([*cond, *body]),
                Ir::Not(a, _) | Ir::Neg(a, _) => todo.push(a),
                Ir::Eq(a, b, _)
                | Ir::Ne(a, b, _)
                | Ir::Lt(a, b, _)
                | Ir::Gt(a, b, _)
                | Ir::Le(a, b, _)
                | Ir::Ge(a, b, _)
                | Ir::And(a, b, _)
                | Ir::Or(a, b, _)
                | Ir::Impl(a, b, _)
                | Ir::Update(a, b, _)
                | Ir::ConcatLists(a, b, _)
                | Ir::Sub(a, b, _)
                | Ir::Mul(a, b, _)
                | Ir::Div(a, b, _) => todo.extend([*a, *b]),
                Ir::Const(_) | Ir::Local { .. } | Ir::WithVar { .. } | Ir::CurPos(_) => {}
            }
        }
        if let Ok(mut g) = self.binds.lock() {
            g.extend(found.into_iter().map(|(a, p, s)| (a, (p, s))));
        }
    }

    /// Replaces a thunk frame that no binding names by its IR's position.
    fn resolve(&self, f: u64) -> u64 {
        if !is_thunk(f) {
            return f;
        }
        let addr = (f & PAYLOAD) as usize;
        if self.binds.lock().is_ok_and(|g| g.contains_key(&addr)) {
            return f;
        }
        // SAFETY: the evaluator that pushed this frame is running, so the
        // arena holding the IR is alive.
        let pos = ir_pos(unsafe { &*(addr as *const Ir) });
        THUNK_AT | pos.0 as u64
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
            if stack.is_empty() {
                out.extend_from_slice("«toplevel»".as_bytes());
            }
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
        if is_attr(f) {
            let Some((_, interner)) = self.names.get() else {
                return "«thunk»".to_string();
            };
            let s = format!("{}«attr»", interner.name_str(Sym((f & PAYLOAD) as u32)));
            return s.replace(';', ",").replace(' ', "_");
        }
        if is_thunk_at(f) {
            return self.describe_thunk(Pos((f & PAYLOAD) as u32), None);
        }
        if is_thunk(f) {
            let known = self
                .binds
                .lock()
                .ok()
                .and_then(|g| g.get(&((f & PAYLOAD) as usize)).copied());
            return match known {
                Some((p, s)) => self.describe_thunk(p, Some(s)),
                None => "«thunk»".to_string(),
            };
        }
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

    /// `file:line:col:name«thunk»` for a thunk whose IR sits in a binding,
    /// and the IR's own position with no name for any other.
    fn describe_thunk(&self, pos: Pos, name: Option<Sym>) -> String {
        let Some((files, interner)) = self.names.get() else {
            return "«thunk»".to_string();
        };
        let at = match files.file_of(pos) {
            Some((file, off)) => {
                let (l, c) = file.line_col(off);
                format!("{}:{l}:{c}", String::from_utf8_lossy(&file.path))
            }
            None => "«none»".to_string(),
        };
        let s = match name {
            Some(n) => format!("{at}:{}«thunk»", interner.name_str(n)),
            None => format!("{at}:«thunk»"),
        };
        s.replace(';', ",").replace(' ', "_")
    }
}

/// This thread's CPU time since its last call, in microseconds. CPU and
/// not wall time, so a wait (a fetch, a subprocess) does not count.
pub fn cpu_us_since_sample() -> u64 {
    thread_local!(static LAST: Cell<u64> = const { Cell::new(0) });
    let now = thread_cpu_time().as_micros() as u64;
    now - LAST.replace(now)
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

    /// A call the way the evaluator enters one: sample, then push.
    fn call(s: &mut Stack, p: &Profile, frame: u64, bytes: u64) {
        if s.due(p) {
            s.take(p, bytes, 7);
        }
        s.enter(frame);
    }

    fn folded(p: &Profile) -> Vec<(Vec<u64>, u64)> {
        let g = p.folded.lock().unwrap();
        let mut v: Vec<_> = g.iter().map(|(k, c)| (k.to_vec(), *c)).collect();
        v.sort();
        v
    }

    use crate::compile::ir::{AttrsIr, BindScope, FormalIr, LambdaIr, LetIr};
    use crate::runtime::Word;

    fn leak<T>(v: T) -> &'static T {
        Box::leak(Box::new(v))
    }

    fn konst() -> IrRef {
        leak(Ir::Const(Word::NULL))
    }

    fn bind(sym: u32, value: IrRef, pos: u32) -> BindIr {
        BindIr {
            sym: Sym(sym),
            value,
            pos: Pos(pos),
            scope: BindScope::Own,
        }
    }

    fn bound(p: &Profile, ir: IrRef) -> Option<(Pos, Sym)> {
        p.binds
            .lock()
            .unwrap()
            .get(&(ir as *const Ir as usize))
            .copied()
    }

    #[test]
    fn no_frame_reads_as_another_kind() {
        // `Pos::NONE` puts all ones in the top half of a call frame
        let none = frame(Pos::NONE, None);
        let prim = primop_frame(Pos::NONE, Sym(1));
        let lam = frame(Pos(0x7FFF_FFFE), Some(Sym(1)));
        for f in [none, prim, lam, frame(Pos(1), None)] {
            assert!(!is_thunk(f) && !is_thunk_at(f) && !is_attr(f), "{f:#x}");
        }
        let ir = konst();
        let t = thunk_frame(ir);
        assert!(is_thunk(t) && !is_thunk_at(t) && !is_attr(t));
        assert_eq!((t & PAYLOAD) as usize, ir as *const Ir as usize);
        let a = attr_frame(Sym(u32::MAX));
        assert!(is_attr(a) && !is_thunk(a));
        assert_eq!((a & PAYLOAD) as u32, u32::MAX);
        assert!(is_thunk_at(THUNK_AT | 5) && !is_thunk(THUNK_AT | 5));
    }

    #[test]
    fn the_ir_walk_finds_the_bindings_of_lets_sets_and_lambdas() {
        let p = Profile::default();
        let (a, b, c, d) = (konst(), konst(), konst(), konst());
        let set = leak(Ir::Attrs(leak(AttrsIr {
            rec: false,
            binds: leak([bind(2, b, 20)]),
            from: &[],
            dynamic: &[],
            pos: Pos(0),
            overrides: None,
        })));
        let lam = leak(Ir::Lambda(leak(LambdaIr {
            param: ParamIr::Formals {
                has_bind: false,
                formals: leak([FormalIr {
                    sym: Sym(9),
                    default: Some(c),
                    pos: Pos(0),
                }]),
                ellipsis: false,
            },
            body: d,
            pos: Pos(0),
            name: None,
            arg: None,
            closed: true,
            no_env: false,
            base: 0,
            frame: 1,
        })));
        let root = leak(Ir::Let(leak(LetIr {
            binds: leak([bind(1, a, 10), bind(3, set, 30), bind(4, lam, 40)]),
            from: &[],
            body: konst(),
            base: LetIr::OWN_ENV,
        })));
        p.note_ir(root);
        assert_eq!(bound(&p, a), Some((Pos(10), Sym(1))));
        // a set under a binding, and its own bindings
        assert_eq!(bound(&p, set), Some((Pos(30), Sym(3))));
        assert_eq!(bound(&p, b), Some((Pos(20), Sym(2))));
        assert_eq!(bound(&p, lam), Some((Pos(40), Sym(4))));
        // a formal's default is not a binding
        assert_eq!(bound(&p, c), None);
        assert_eq!(bound(&p, d), None);
    }

    #[test]
    fn a_node_two_parents_share_is_walked_once() {
        let p = Profile::default();
        let shared = konst();
        let root = leak(Ir::List(leak([shared, shared])));
        p.note_ir(root);
        assert!(p.binds.lock().unwrap().is_empty());
    }

    #[test]
    fn an_unnamed_thunk_is_resolved_to_its_position() {
        let p = Profile::default();
        let named = konst();
        p.note_ir(leak(Ir::Let(leak(LetIr {
            binds: leak([bind(1, named, 10)]),
            from: &[],
            body: konst(),
            base: LetIr::OWN_ENV,
        }))));
        // a binding's thunk keeps its frame, and anything else gets a position
        assert_eq!(p.resolve(thunk_frame(named)), thunk_frame(named));
        let anon = leak(Ir::CurPos(Pos(77)));
        let r = p.resolve(thunk_frame(anon));
        assert!(is_thunk_at(r));
        assert_eq!(r & PAYLOAD, 77);
        assert_eq!(p.resolve(frame(Pos(5), None)), frame(Pos(5), None));
    }

    #[test]
    fn a_frame_keeps_its_call_site_and_its_name() {
        let f = frame(Pos(7), Some(Sym(3)));
        assert_eq!(f >> 32, 7);
        assert_eq!(f as u32, 3);
        assert_eq!(frame(Pos(7), None) as u32, ANON);
    }

    #[test]
    fn a_tick_is_sampled_once() {
        let p = Profile::default();
        let (mut s, mut other) = (Stack::sampling(), Stack::sampling());
        call(&mut s, &p, 10, 0);
        p.tick.store(3, Ordering::Relaxed);
        call(&mut s, &p, 20, 0);
        call(&mut s, &p, 30, 0);
        // another fiber, switched in on the same tick, takes nothing
        call(&mut other, &p, 50, 0);
        assert_eq!(folded(&p), vec![(vec![10], 7)]);
        p.tick.store(4, Ordering::Relaxed);
        call(&mut s, &p, 40, 0);
        assert_eq!(folded(&p), vec![(vec![10], 7), (vec![10, 20, 30], 7)]);
    }

    #[test]
    fn an_unwind_returns_to_a_depth() {
        let p = Profile::default();
        let mut s = Stack::sampling();
        let d = s.depth();
        call(&mut s, &p, 10, 0);
        call(&mut s, &p, 20, 0);
        s.unwind(d);
        assert_eq!(s.depth(), 0);
        call(&mut s, &p, 30, 0);
        p.tick.store(1, Ordering::Relaxed);
        call(&mut s, &p, 40, 0);
        assert_eq!(folded(&p), vec![(vec![30], 7)]);
    }

    #[test]
    fn a_wait_is_not_cpu_time() {
        cpu_us_since_sample();
        thread::sleep(Duration::from_millis(50));
        assert!(cpu_us_since_sample() < 10_000);
        let start = thread_cpu_time();
        while thread_cpu_time() - start < Duration::from_millis(20) {}
        assert!(cpu_us_since_sample() >= 20_000);
    }

    #[test]
    fn a_deep_stack_is_capped() {
        let p = Profile::default();
        let mut s = Stack::sampling();
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
        let mut s = Stack::sampling();
        call(&mut s, &p, 10, 0);
        p.tick.store(1, Ordering::Relaxed);
        call(&mut s, &p, 20, 4096);
        assert_eq!(folded(&p), vec![(vec![10], 4096)]);
    }
}
