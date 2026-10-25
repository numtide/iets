//! The fine trace: what an evaluation read, at the grain of a binding. A
//! prototype for incremental evaluation, on only when `IETS_FINE_TRACE`
//! names the file to write, and only at one worker.
//!
//! A set or `let` of `FINE_MIN` bindings or more makes each binding a region
//! of its own: the compiler wraps its value in `Ir::Region`, so an edit to
//! one binding changes that region only. A region's hash leaves out the
//! names of such a set. A lookup records the name it asked for (`K`) and an
//! iteration the whole set (`A`), so a new package changes neither the
//! region around `all-packages.nix`'s set nor the readers of other names.
//!
//! `//`, `mapAttrs`, `removeAttrs` and the flat copy of a union keep the
//! provenance of their operands, so a lookup in `pkgs` is a lookup in the
//! sets and directory listings it was made from. Any other way to make a set
//! reads its operands through `get` and `iter`, which record as they read.
//!
//! Each force of a thunk is a frame. What the frame observes, and an edge
//! to the trace of each forced thunk it reads, become the thunk's trace,
//! which its state word holds (`Word::traced`). So one evaluation of many
//! jobs that share thunks still gives each job all that it depends on.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::panic::Location;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, Ordering};

use crate::compile::ir::{
    AttrElemIr, AttrsIr, BindIr, BindScope, Ir, IrRef, LambdaIr, LetIr, NO_LIT, ParamIr, SelectIr,
};
use crate::eval::Evaluator;
use crate::runtime::Word;
use crate::runtime::value::{Attr, AttrsObj, Value};
use crate::runtime::word::{TAG_IMM, TAG_INT};
use crate::syntax::symbol::{FxHashMap, FxHashSet, Interner, Sym};
use crate::trace::{Kind, Obs};

pub const FINE_MIN: usize = 8;
/// `AttrsObj::flags` bit: the set has a provenance here.
pub(crate) const FLAG: u8 = 0x40;
/// `AttrsObj::flags` bit: the set is the value at a query path (`State::tag`).
pub(crate) const TAGGED: u8 = 0x20;
/// With `TAGGED`: every cell of the set has its path.
const FILLED: u8 = 0x10;
/// An entry of a frame that is an edge to a trace, not an observation.
const EDGE: u32 = 1 << 31;
/// With `EDGE`: the read of a query, whose path follows in the next word.
const QUERY: u32 = 1 << 30;
/// The parent of a path of the root set.
const NO_PATH: u32 = u32::MAX;
const STALE: u32 = u32::MAX;

static ON: AtomicBool = AtomicBool::new(false);
/// An `import` is reading its source: the regions stand for the bytes.
static IMPORTING: AtomicBool = AtomicBool::new(false);
static NEXT_REGION: AtomicU32 = AtomicU32::new(0);
static NEXT_LIT: AtomicU32 = AtomicU32::new(0);
/// Where the builtin that runs now was called.
static AT: AtomicU32 = AtomicU32::new(0);
/// The Nix call stack of that builtin, which is alive while it runs.
static STACK: AtomicPtr<crate::profile::Stack> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct State {
    out: Option<PathBuf>,
    regions: FxHashMap<u32, Region>,
    lits: FxHashMap<u32, Lit>,
    provs: Vec<Prov>,
    interned: FxHashMap<Prov, u32>,
    prov_of: FxHashMap<usize, u32>,
    /// The reader in iets, the builtin call in Nix and its stack, of the
    /// first whole read of each set.
    sites: FxHashMap<u32, (&'static Location<'static>, u32, Vec<u64>)>,
    /// Constants the compiler folded, by bits: their global or builtin name.
    rev: Option<FxHashMap<u64, Sym>>,
    obs: Vec<O>,
    obs_ids: FxHashMap<O, u32>,
    /// The frame that last recorded each observation and each trace.
    obs_stamp: Vec<u32>,
    /// Traces: a stamp, a length, then the entries; a trace id is the
    /// offset of its stamp, and 0 is no trace.
    nodes: Vec<u32>,
    /// The entries of the open frames, outermost first.
    log: Vec<u32>,
    /// Each open frame: where its entries start in `log`, and its stamp.
    frames: Vec<(usize, u32)>,
    /// The trace of each path copied to the store, for the cache hits.
    copies: FxHashMap<Vec<u8>, u32>,
    serial: u32,
    /// Query paths: the parent path (`NO_PATH` under the root set) and the
    /// name.
    paths: Vec<(u32, Sym)>,
    path_ids: FxHashMap<(u32, Sym), u32>,
    /// By path: how its reads are recorded (`Kind`), the fingerprint of
    /// its value, the trace of its force, and the frame that last read it.
    kind: Vec<u8>,
    fps: Vec<Option<[u8; 16]>>,
    qtrace: Vec<u32>,
    path_stamp: Vec<u32>,
    whole_stamp: Vec<u32>,
    /// By ring head: how many paths joined it. By path: that count plus one
    /// when `select` last took the path to each path of its parent's ring.
    grown: Vec<u32>,
    fanned: Vec<u32>,
    /// By ring head: the trace `above` made, which holds the edges, or
    /// `STALE`. By path: whether it has a path below it.
    above_node: Vec<u32>,
    parent: Vec<bool>,
    /// The paths that reach one object, a ring by path, and the first of
    /// them, which reads record and which holds the kind, fingerprint and
    /// trace.
    ring: Vec<u32>,
    head: Vec<u32>,
    /// The length of `nodes` when each path was made.
    born: Vec<u32>,
    /// The thunks of the cells of the root set and of every set a query
    /// read, by bits: their first path.
    cells: FxHashMap<u64, u32>,
    /// Those sets, by address: their first path.
    tags: FxHashMap<usize, (u32, u8)>,
    /// Functions such a set holds or such a thunk forced to, by bits: their
    /// first path, and a ring of their paths by path.
    funcs: FxHashMap<u64, u32>,
    fring: Vec<u32>,
}

/// How the reads of a path are recorded.
mod kind {
    pub const UNREAD: u8 = 0;
    /// A query, by fingerprint.
    pub const QUERY: u8 = 1;
    /// A function: an edge, and the paths above it when it is called.
    pub const FUNC: u8 = 2;
    /// An edge and the paths above it: no fingerprint, or a set another
    /// path reached first.
    pub const PLAIN: u8 = 3;
    /// A set its parent holds as a value: no force of its own.
    pub const EAGER: u8 = 4;
}

/// One observation.
#[derive(Clone, PartialEq, Eq, Hash)]
enum O {
    Region(u32),
    Key(u32, Sym),
    All(u32),
    /// A position at a byte offset of a file reached a value.
    Pos(Arc<[u8]>, u32),
    /// What the tracing cache records, as a line of the output.
    Out(String),
}

struct Region {
    file: Arc<[u8]>,
    path: String,
    hash: [u8; 16],
}

struct Lit {
    file: Arc<[u8]>,
    region: String,
    ordinal: u32,
    keys: Vec<Sym>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Prov {
    Lit(u32),
    Dir(Vec<u8>),
    /// `b` over `a`.
    Union(u32, u32),
    Remove(u32, Vec<Sym>),
}

/// Turns the trace on when `IETS_FINE_TRACE` is set; empty, the run writes no
/// whole trace of its own.
pub fn init() {
    if let Some(p) = std::env::var_os("IETS_FINE_TRACE") {
        start((!p.is_empty()).then(|| PathBuf::from(p)));
    }
}

fn start(out: Option<PathBuf>) {
    let mut s = State {
        out,
        nodes: vec![0, 0],
        serial: 1,
        ..State::default()
    };
    s.frames.push((0, 1));
    STATE.with(|c| *c.borrow_mut() = Some(s));
    ON.store(true, Ordering::Relaxed);
}

#[inline]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

#[inline]
pub fn at(pos: u32, stack: &crate::profile::Stack) {
    if on() {
        AT.store(pos, Ordering::Relaxed);
        STACK.store(
            stack as *const crate::profile::Stack as *mut _,
            Ordering::Relaxed,
        );
    }
}

#[inline]
pub fn importing() -> bool {
    IMPORTING.load(Ordering::Relaxed)
}

pub fn set_importing(b: bool) {
    IMPORTING.store(b, Ordering::Relaxed);
}

pub fn new_region() -> u32 {
    NEXT_REGION.fetch_add(1, Ordering::Relaxed)
}

pub fn new_lit() -> u32 {
    NEXT_LIT.fetch_add(1, Ordering::Relaxed)
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|c| f(c.borrow_mut().as_mut().expect("the fine trace is on")))
}

impl State {
    fn stamp(&self) -> u32 {
        self.frames.last().map_or(0, |f| f.1)
    }

    fn record(&mut self, o: O) {
        let id = match self.obs_ids.get(&o) {
            Some(id) => *id,
            None => {
                let id = self.obs.len() as u32;
                self.obs.push(o.clone());
                self.obs_ids.insert(o, id);
                self.obs_stamp.push(0);
                id
            }
        };
        let st = self.stamp();
        if self.obs_stamp[id as usize] != st {
            self.obs_stamp[id as usize] = st;
            self.log.push(id);
        }
    }

    fn edge(&mut self, t: u32) {
        let st = self.stamp();
        if self.nodes[t as usize] != st {
            self.nodes[t as usize] = st;
            self.log.push(EDGE | t);
        }
    }
}

/// Opens the frame of a force; false when the trace is off.
#[inline]
pub fn push() -> bool {
    if !on() {
        return false;
    }
    with(|s| {
        s.serial += 1;
        let st = s.serial;
        s.frames.push((s.log.len(), st));
    });
    true
}

impl State {
    /// Closes the innermost frame: its trace, or 0 for none.
    fn close(&mut self) -> u32 {
        let (start, _) = self.frames.pop().expect("an open frame");
        let n = self.log.len() - start;
        let t = match n {
            0 => 0,
            1 if self.log[start] & (EDGE | QUERY) == EDGE => self.log[start] & !EDGE,
            _ => {
                let t = self.nodes.len() as u32;
                self.nodes.push(0);
                self.nodes.push(n as u32);
                self.nodes.extend_from_slice(&self.log[start..]);
                t
            }
        };
        self.log.truncate(start);
        t
    }

    fn path(&mut self, parent: u32, sym: Sym) -> u32 {
        if let Some(p) = self.path_ids.get(&(parent, sym)) {
            return *p;
        }
        let p = self.paths.len() as u32;
        self.paths.push((parent, sym));
        self.path_ids.insert((parent, sym), p);
        self.kind.push(kind::UNREAD);
        self.fps.push(None);
        self.qtrace.push(0);
        self.path_stamp.push(0);
        self.whole_stamp.push(0);
        self.grown.push(0);
        self.fanned.push(0);
        self.above_node.push(STALE);
        self.parent.push(false);
        if parent != NO_PATH {
            self.parent[parent as usize] = true;
        }
        self.ring.push(p);
        self.head.push(p);
        self.fring.push(p);
        self.born.push(self.nodes.len() as u32);
        p
    }

    /// Adds the new path `c` to the ring of `h`: they reach one object.
    fn link(&mut self, h: u32, c: u32) {
        let (h, c) = (self.head[h as usize], c as usize);
        if h as usize == c || self.ring[c] != c as u32 {
            return;
        }
        self.head[c] = h;
        self.grown[h as usize] += 1;
        self.above_node[h as usize] = STALE;
        self.ring[c] = self.ring[h as usize];
        self.ring[h as usize] = c as u32;
    }

    /// A read of query `p`, whose force left trace `t`.
    fn query(&mut self, p: u32, t: u32) {
        let st = self.stamp();
        if self.path_stamp[p as usize] != st {
            self.path_stamp[p as usize] = st;
            self.log.push(EDGE | QUERY | t);
            self.log.push(p);
        }
    }

    /// Edges to the traces of the queries above each path of the ring of
    /// `p`: what made the sets that hold the object, and so its code. The
    /// readers of one ring share them as one trace.
    fn above(&mut self, p: u32) {
        let h = self.head[p as usize] as usize;
        if self.above_node[h] == STALE {
            let mut ts = Vec::new();
            let mut m = p;
            loop {
                let mut q = self.paths[m as usize].0;
                while q != NO_PATH {
                    let t = self.qtrace[q as usize];
                    if t != 0 {
                        ts.push(EDGE | t);
                    }
                    q = self.paths[q as usize].0;
                }
                m = self.ring[m as usize];
                if m == p {
                    break;
                }
            }
            ts.sort_unstable();
            ts.dedup();
            self.above_node[h] = match ts[..] {
                [] => 0,
                [t] => t & !EDGE,
                _ => {
                    let t = self.nodes.len() as u32;
                    self.nodes.push(0);
                    self.nodes.push(ts.len() as u32);
                    self.nodes.extend_from_slice(&ts);
                    t
                }
            };
        }
        let t = self.above_node[h];
        if t != 0 {
            self.edge(t);
        }
    }

    fn cell(&mut self, w: Word, c: u32) {
        match self.cells.get(&w.bits()) {
            Some(&h) => self.link(h, c),
            None => {
                self.cells.insert(w.bits(), c);
                let st = w.thunk_state_cell();
                let sw = st.get();
                if sw.is_traced() {
                    st.set(Word::traced_cell(sw.traced_id()));
                } else if !w.thunk_unforced(sw) && !sw.is_blackhole() {
                    st.set(Word::traced_cell(0));
                }
            }
        }
    }

    /// Marks function `w` as the value at path `c`: a cell that holds it,
    /// or a thunk that forced to it.
    fn func(&mut self, w: Word, c: u32) {
        match self.funcs.get(&w.bits()) {
            Some(&h) if h != c && self.fring[c as usize] == c => {
                self.fring[c as usize] = self.fring[h as usize];
                self.fring[h as usize] = c;
            }
            Some(_) => {}
            None => {
                self.funcs.insert(w.bits(), c);
            }
        }
    }

    /// Records set `a` as the value at path `p`, `depth` sets below the
    /// value of a force: a cell of it becomes a query when a lookup or an
    /// iteration reaches it. False when a force reached `a` and another path
    /// reached it first, which keeps it: a read through `p` is then no query,
    /// and its reader depends on all that made `p` lead to `a`, such as
    /// `foo = foo_2`.
    fn tag(&mut self, a: &'static AttrsObj, p: u32, depth: u32) -> bool {
        match self.tags.get(&addr(a)).copied() {
            Some((q, _)) if q != NO_PATH && self.head[q as usize] == self.head[p as usize] => {
                return true;
            }
            Some((q, _)) if depth == 0 || q == NO_PATH => return false,
            Some((q, d)) => {
                self.kind[p as usize] = kind::EAGER;
                self.link(q, p);
                if a.flags & FILLED != 0 {
                    for at in a.iter_raw() {
                        let c = self.path(p, at.sym);
                        self.mark(at, c, d);
                    }
                }
                return true;
            }
            None => {}
        }
        if depth > 0 {
            self.kind[p as usize] = kind::EAGER;
        }
        self.tags.insert(addr(a), (p, depth as u8));
        a.mark(TAGGED);
        if a.is_union()
            && let Some(f) = a.flattened()
        {
            self.tags.insert(addr(f), (p, depth as u8));
            f.mark(TAGGED);
        }
        true
    }

    /// Marks the object in cell `at` as the value at path `c`.
    fn mark(&mut self, at: &Attr, c: u32, depth: u8) {
        let w = at.cell.get();
        if w.is_thunk() {
            self.cell(w, c);
            return;
        }
        match w.view() {
            Value::Lambda(_) | Value::PrimOpApp(_) => self.func(w, c),
            Value::Attrs(b) if depth < 4 => {
                self.tag(b, c, depth as u32 + 1);
            }
            _ => {}
        }
    }

    /// A lookup in the tagged set `a` reached cell `at`: it is the value at
    /// that name below each path of `a`.
    fn select(&mut self, a: &AttrsObj, at: &Attr) {
        let Some(&(p, depth)) = self.tags.get(&addr(a)) else {
            return;
        };
        let c = self.path(p, at.sym);
        let n = self.grown[self.head[p as usize] as usize] + 1;
        if std::mem::replace(&mut self.fanned[c as usize], n) == n {
            return;
        }
        let mut r = p;
        loop {
            let c = self.path(r, at.sym);
            self.mark(at, c, depth);
            r = self.ring[r as usize];
            if r == p {
                break;
            }
        }
    }

    /// An iteration or a copy reached every cell of the tagged set `a`.
    fn fill(&mut self, a: &AttrsObj) {
        if a.flags & FILLED != 0 {
            return;
        }
        a.mark(FILLED);
        for at in a.iter_raw() {
            self.select(a, at);
        }
    }

    /// A read of the forced thunk `w` in state `st`, with value `v`.
    fn read(&mut self, names: &Interner, w: Word, st: Word, v: Word) {
        let t = st.traced_id();
        let p = match st.is_query_cell() {
            true => self.cells.get(&w.bits()).copied().unwrap_or(NO_PATH),
            false => NO_PATH,
        };
        if p == NO_PATH {
            if t != 0 {
                self.edge(t);
            }
            return;
        }
        let i = p as usize;
        if self.kind[i] == kind::UNREAD {
            self.qtrace[i] = t;
            // the traces `above` made for the paths below lack this one
            if t != 0 && self.parent[i] {
                self.above_node.fill(STALE);
            }
            self.kind[i] = match v.view() {
                Value::Attrs(a) if !self.tag(a, p, 0) => kind::PLAIN,
                Value::Lambda(_) | Value::PrimOpApp(_) => {
                    self.func(v, p);
                    kind::FUNC
                }
                _ => match fingerprint(names, v) {
                    Some(h) => {
                        self.fps[i] = Some(h);
                        kind::QUERY
                    }
                    None => kind::PLAIN,
                },
            };
        }
        match self.kind[i] {
            kind::QUERY => self.query(p, t),
            k => {
                if t != 0 {
                    self.edge(t);
                }
                if k == kind::PLAIN {
                    self.above(p);
                }
            }
        }
    }
}

/// The fingerprint of a forced value a query read: scalars and lists of them
/// by value, a set by its names and the values its cells hold that are no
/// thunk. `None` for what has none: a function, a list of anything else, a
/// set that holds such a list.
fn fingerprint(names: &Interner, v: Word) -> Option<[u8; 16]> {
    let mut h = Hasher::new();
    put(names, &mut h, v, 0)?;
    Some(h.finalize().as_bytes()[..16].try_into().unwrap())
}

fn put(names: &Interner, h: &mut Hasher, v: Word, depth: u32) -> Option<()> {
    if depth > 4 {
        return None;
    }
    match v.view() {
        Value::Int(i) => {
            tag(h, 1);
            h.update(&i.to_le_bytes());
        }
        Value::Float(f) => {
            tag(h, 2);
            h.update(&f.to_bits().to_le_bytes());
        }
        Value::Bool(b) => tag(h, 3 + u8::from(b)),
        Value::Null => tag(h, 5),
        Value::String(s) => {
            tag(h, 6);
            bytes(h, s.bytes);
            bytes(h, format!("{:?}", s.ctx).as_bytes());
        }
        Value::Path(p) => {
            tag(h, 7);
            bytes(h, p.path);
        }
        Value::List(l) => {
            tag(h, 8);
            num(h, l.len as usize);
            for c in l.cells() {
                put(names, h, forced(c.get())?, depth + 1)?;
            }
        }
        Value::Attrs(a) => {
            tag(h, 9);
            let mut v: Vec<_> = a.iter_raw().collect();
            v.sort_by(|x, y| names.name(x.sym).cmp(names.name(y.sym)));
            num(h, v.len());
            for at in v {
                bytes(h, names.name(at.sym));
                let w = at.cell.get();
                if w.is_thunk() {
                    tag(h, 10);
                } else if matches!(
                    w.view(),
                    Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_)
                ) {
                    tag(h, 11);
                } else {
                    put(names, h, w, depth + 1)?;
                }
            }
        }
        Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_) => return None,
    }
    Some(())
}

/// The value of `w` when it holds one now, without forcing anything.
fn forced(w: Word) -> Option<Word> {
    if !w.is_thunk() {
        return Some(w);
    }
    let st = w.thunk_state_cell().get();
    (!w.thunk_unforced(st) && !st.is_blackhole()).then(|| w.thunk_value(st))
}

/// Closes the frame of a force that returned, and has the parent read it.
/// `done` is the state the thunk gets without the trace; the result is the
/// state to publish.
pub fn finish(names: &Interner, w: Word, v: Word, done: Word) -> Word {
    with(|s| {
        let t = s.close();
        let st = if s.cells.contains_key(&w.bits()) {
            Word::traced_cell(t)
        } else if t != 0 {
            Word::traced(t)
        } else {
            return done;
        };
        s.read(names, w, st, v);
        st
    })
}

/// A read of the forced thunk `w`, whose state `st` holds a trace.
#[inline]
pub fn read(names: &Interner, w: Word, st: Word, v: Word) {
    if !st.is_query_cell() {
        edge(st.traced_id());
        return;
    }
    with(|s| s.read(names, w, st, v));
}

/// A call of `f`: when a set a query read holds it, its code is the
/// computation of that set, which the caller now depends on in full.
#[inline]
pub fn apply(f: Word) {
    if on() {
        with(|s| {
            if let Some(&h) = s.funcs.get(&f.bits()) {
                let mut p = h;
                loop {
                    s.whole(p);
                    p = s.fring[p as usize];
                    if p == h {
                        break;
                    }
                }
            }
        });
    }
}

/// `==` on a set a query read: pointer equality inside it depends on how it
/// was built, so the caller depends on that in full.
pub fn compared(a: &AttrsObj) {
    if on() {
        with(|s| {
            if let Some(&(p, _)) = s.tags.get(&addr(a))
                && p != NO_PATH
            {
                s.whole(p);
            }
        });
    }
}

impl State {
    fn whole(&mut self, p: u32) {
        let st = self.stamp();
        if std::mem::replace(&mut self.whole_stamp[p as usize], st) == st {
            return;
        }
        let t = self.qtrace[self.head[p as usize] as usize];
        if t != 0 {
            self.edge(t);
        }
        self.above(p);
    }
}

/// `builtins.fineRoot set`: the cells of `set` are queries, named by their
/// attribute path.
pub fn root(a: &'static AttrsObj) {
    if on() {
        with(|s| {
            // the root set reached again is no query: its reader depends on the root in full
            s.tags.insert(addr(a), (NO_PATH, 0));
            for at in a.iter_raw() {
                let c = s.path(NO_PATH, at.sym);
                s.mark(at, c, 0);
            }
        });
    }
}

pub fn select(a: &AttrsObj, at: &Attr) {
    if on() {
        with(|s| s.select(a, at));
    }
}

/// An iteration or a copy of `a` reaches all its cells: those of the
/// tagged sets among `a` and the sides of a union it holds become queries.
#[inline]
pub fn spread(a: &AttrsObj) {
    if !on() {
        return;
    }
    let leaves = a.is_union() && a.flattened().is_none();
    if a.flags & TAGGED == 0 && !(leaves && a.leaf_sets().iter().any(|l| l.flags & TAGGED != 0)) {
        return;
    }
    with(|s| {
        if a.flags & TAGGED != 0 {
            s.fill(a);
        }
        if leaves {
            for l in a.leaf_sets() {
                if l.flags & TAGGED != 0 {
                    s.fill(l);
                }
            }
        }
    });
}

/// `out` is the flat copy of the union `a`: the same set at the same paths.
pub fn flattened(out: &AttrsObj, a: &AttrsObj) {
    if a.flags & TAGGED != 0 {
        with(|s| {
            if let Some(&t) = s.tags.get(&addr(a)) {
                s.tags.insert(addr(out), t);
                out.mark(TAGGED | (a.flags & FILLED));
            }
        });
    }
}

/// The fingerprint of `v` as a query records it, in hex; `-` for none.
pub fn fingerprint_hex(names: &Interner, v: Word) -> String {
    fingerprint(names, v).map_or("-".into(), |h| hex(&h))
}

/// Closes the frame of a force outside the thunk protocol (a copy to the
/// store), which the parent then reads; its trace, or 0 for none.
pub fn pop() -> u32 {
    with(|s| {
        let t = s.close();
        if t != 0 {
            s.edge(t);
        }
        t
    })
}

/// Closes the frame of a force that failed: what it read stays with the
/// parent, which saw the failure.
pub fn pop_failed() {
    with(|s| {
        s.frames.pop();
    });
}

/// A read of a forced thunk whose force left trace `t`.
#[inline]
pub fn edge(t: u32) {
    if t != 0 {
        with(|s| s.edge(t));
    }
}

/// The trace of the first copy of `path` to the store.
pub fn copy_trace(path: &[u8]) -> u32 {
    with(|s| s.copies.get(path).copied().unwrap_or(0))
}

pub fn set_copy_trace(path: &[u8], t: u32) {
    with(|s| s.copies.insert(path.to_vec(), t));
}

fn addr(a: &AttrsObj) -> usize {
    a as *const AttrsObj as usize
}

fn prov(s: &State, a: &AttrsObj) -> Option<u32> {
    if a.flags & FLAG == 0 {
        return None;
    }
    s.prov_of.get(&addr(a)).copied()
}

fn intern(s: &mut State, p: Prov) -> u32 {
    if let Some(id) = s.interned.get(&p) {
        return *id;
    }
    let id = s.provs.len() as u32;
    s.provs.push(p.clone());
    s.interned.insert(p, id);
    id
}

fn bind(s: &mut State, out: &AttrsObj, id: u32) {
    s.prov_of.insert(addr(out), id);
    // SAFETY: `flags` is a byte of a live arena object; the trace runs on one
    // thread, and `stats2` (the other writer) is not built with it.
    unsafe { (*(&raw const out.flags as *const AtomicU8)).fetch_or(FLAG, Ordering::Relaxed) };
}

fn set(s: &mut State, out: &AttrsObj, p: Prov) {
    let id = intern(s, p);
    bind(s, out, id);
}

pub fn enter(id: u32) {
    with(|s| s.record(O::Region(id)));
}

pub fn lit(out: &AttrsObj, lit: u32) {
    with(|s| set(s, out, Prov::Lit(lit)));
}

pub fn dir(out: &AttrsObj, path: &[u8]) {
    with(|s| set(s, out, Prov::Dir(path.to_vec())));
}

/// `b` over `a`. A side with no provenance adds nothing a lookup could
/// see change, so the union is then the other side.
#[inline]
pub fn union(out: &AttrsObj, a: &AttrsObj, b: &AttrsObj) {
    if (a.flags | b.flags) & FLAG != 0 {
        union_slow(out, a, b);
    }
}

fn union_slow(out: &AttrsObj, a: &AttrsObj, b: &AttrsObj) {
    with(|s| match (prov(s, a), prov(s, b)) {
        (Some(x), Some(y)) => set(s, out, Prov::Union(x, y)),
        (Some(x), None) | (None, Some(x)) => bind(s, out, x),
        (None, None) => {}
    });
}

/// `out` has the names of `src`.
#[inline]
pub fn same(out: &AttrsObj, src: &AttrsObj) {
    if src.flags & FLAG != 0 && addr(out) != addr(src) {
        with(|s| {
            if let Some(p) = prov(s, src) {
                bind(s, out, p)
            }
        });
    }
}

/// `out` is `src` without `names`, which are sorted.
pub fn removed(out: &AttrsObj, src: &AttrsObj, names: &[Sym]) {
    if src.flags & FLAG == 0 {
        return;
    }
    with(|s| {
        if let Some(p) = prov(s, src) {
            set(s, out, Prov::Remove(p, names.to_vec()))
        }
    });
}

#[cold]
pub fn get(a: &AttrsObj, sym: Sym) {
    with(|s| {
        if let Some(p) = prov(s, a) {
            s.record(O::Key(p, sym));
        }
    });
}

#[cold]
pub fn all(a: &AttrsObj, at: &'static Location<'static>) {
    with(|s| {
        if let Some(p) = prov(s, a) {
            s.sites.entry(p).or_insert_with(|| {
                let st = STACK.load(Ordering::Relaxed);
                // SAFETY: `at` stored the stack of the evaluator that is
                // running the builtin, which outlives the builtin's call.
                let frames = if st.is_null() {
                    Vec::new()
                } else {
                    unsafe { (*st).frames().to_vec() }
                };
                (at, AT.load(Ordering::Relaxed), frames)
            });
            s.record(O::All(p));
        }
    });
}

/// A position at byte `off` of `path` reached a value.
pub fn position(path: &[u8], off: u32) {
    with(|s| s.record(O::Pos(path.into(), off)));
}

/// What the tracing cache records.
pub fn outside(o: &Obs) {
    let line = match o {
        Obs::File { path, hash, .. } => format!("F\t{}\t{}", lossy(path), hex(hash)),
        Obs::Dir { path, hash } => format!("D\t{}\t{}", lossy(path), hex(hash)),
        Obs::Stat { path, kind, .. } => format!("S\t{}\t{}", lossy(path), kind_name(*kind)),
        Obs::Tree { root, nar_hash, .. } => format!("T\t{}\t{}", lossy(root), hex(nar_hash)),
        Obs::Canon { path, .. } => format!("C\t{}", lossy(path)),
        Obs::Env { name, .. } => format!("E\t{}", lossy(name)),
        Obs::Cmd { dir, .. } => format!("Q\t{}", lossy(dir)),
        Obs::Input { entry, .. } => format!("I\t{}", lossy(entry)),
    };
    with(|s| s.record(O::Out(line)));
}

/// After the compile of `path` with `root` as its IR: the regions and the
/// literals of the file.
pub fn compiled(ev: &Evaluator, path: &[u8], root: IrRef, base: u32) {
    let Ir::Region { id, body } = root else {
        return;
    };
    let rev = with(|s| s.rev.take()).unwrap_or_else(|| {
        let mut rev = FxHashMap::default();
        if let Some(b) = ev.globals.builtins {
            for a in b.iter_raw() {
                rev.insert(a.cell.get().bits(), a.sym);
            }
        }
        for (sym, w) in &ev.globals.table {
            rev.insert(w.bits(), *sym);
        }
        rev
    });
    let mut w = Walk {
        names: &ev.interner,
        rev: &rev,
        base,
        regions: Vec::new(),
        lits: Vec::new(),
        envs: Vec::new(),
    };
    w.region(*id, String::new(), 0, body);
    let (regions, lits) = (w.regions, w.lits);
    let file: Arc<[u8]> = path.into();
    with(|s| {
        for (id, path, hash) in regions {
            let file = file.clone();
            s.regions.insert(id, Region { file, path, hash });
        }
        for (id, region, ordinal, keys) in lits {
            let file = file.clone();
            s.lits.insert(
                id,
                Lit {
                    file,
                    region,
                    ordinal,
                    keys,
                },
            );
        }
        s.rev = Some(rev);
    });
}

type Hasher = blake3::Hasher;

struct Walk<'a> {
    names: &'a Interner,
    rev: &'a FxHashMap<u64, Sym>,
    base: u32,
    regions: Vec<(u32, String, [u8; 16])>,
    lits: Vec<(u32, String, u32, Vec<Sym>)>,
    /// The slots of each env the walk is in, innermost last; `None` for one
    /// with no name.
    envs: Vec<Vec<Option<Sym>>>,
}

fn tag(h: &mut Hasher, t: u8) {
    h.update(&[t]);
}

fn num(h: &mut Hasher, n: usize) {
    h.update(&(n as u64).to_le_bytes());
}

fn bytes(h: &mut Hasher, b: &[u8]) {
    num(h, b.len());
    h.update(b);
}

fn scope_tag(s: BindScope) -> u8 {
    match s {
        BindScope::Own => 0,
        BindScope::Outer => 1,
        BindScope::From => 2,
    }
}

impl Walk<'_> {
    fn region(&mut self, id: u32, path: String, scope: u8, body: IrRef) {
        let mut h = Hasher::new();
        tag(&mut h, scope);
        let mut ord = 0;
        self.node(&mut h, &path, &mut ord, body);
        let hash = h.finalize().as_bytes()[..16].try_into().unwrap();
        self.regions.push((id, path, hash));
    }

    fn sym(&self, h: &mut Hasher, s: Sym) {
        bytes(h, self.names.name(s));
    }

    fn opt(&mut self, h: &mut Hasher, path: &str, ord: &mut u32, e: Option<IrRef>) {
        match e {
            None => tag(h, 0),
            Some(e) => {
                tag(h, 1);
                self.node(h, path, ord, e);
            }
        }
    }

    fn nodes(&mut self, h: &mut Hasher, path: &str, ord: &mut u32, es: &[IrRef]) {
        num(h, es.len());
        for e in es {
            self.node(h, path, ord, e);
        }
    }

    fn elems(&mut self, h: &mut Hasher, path: &str, ord: &mut u32, elems: &[AttrElemIr]) {
        num(h, elems.len());
        for e in elems {
            match e {
                AttrElemIr::Static(s) => {
                    tag(h, 0);
                    self.sym(h, *s);
                }
                AttrElemIr::Dynamic(e) => {
                    tag(h, 1);
                    self.node(h, path, ord, e);
                }
            }
        }
    }

    fn select(&mut self, h: &mut Hasher, path: &str, ord: &mut u32, s: &SelectIr) {
        self.node(h, path, ord, s.subject);
        self.elems(h, path, ord, s.path);
        self.opt(h, path, ord, s.default);
    }

    fn konst(&self, h: &mut Hasher, w: Word) {
        if w.tag() == TAG_INT || w.tag() == TAG_IMM {
            tag(h, 0);
            num(h, w.bits() as usize);
            return;
        }
        if let Some(s) = self.rev.get(&w.bits()) {
            tag(h, 1);
            self.sym(h, *s);
            return;
        }
        match w.view() {
            Value::String(s) => {
                tag(h, 2);
                bytes(h, s.bytes);
                num(h, s.ctx.len());
            }
            Value::Path(p) => {
                tag(h, 3);
                bytes(h, p.path);
            }
            Value::Float(f) => {
                tag(h, 4);
                h.update(&f.to_bits().to_le_bytes());
            }
            Value::Int(i) => {
                tag(h, 5);
                h.update(&i.to_le_bytes());
            }
            _ => panic!("fine trace: a constant of no stable name"),
        }
    }

    /// Symbol ids follow the order of interning, which differs between
    /// runs, so a hash takes names in name order and never a slot index
    /// that a sort by id chose.
    fn by_name<'b, T>(&self, items: &'b [T], sym: impl Fn(&T) -> Sym) -> Vec<&'b T> {
        let mut v: Vec<&T> = items.iter().collect();
        v.sort_by(|a, b| self.names.name(sym(a)).cmp(self.names.name(sym(b))));
        v
    }

    fn lambda(&mut self, h: &mut Hasher, path: &str, ord: &mut u32, l: &LambdaIr) {
        let mut slots = Vec::new();
        match &l.param {
            ParamIr::Ident => {
                tag(h, 0);
                slots.push(l.arg);
            }
            ParamIr::Formals {
                has_bind, formals, ..
            } => {
                if *has_bind {
                    slots.push(l.arg);
                }
                slots.extend(formals.iter().map(|f| Some(f.sym)));
            }
        }
        match l.arg {
            None => tag(h, 0),
            Some(s) => {
                tag(h, 1);
                self.sym(h, s);
            }
        }
        self.envs.push(slots);
        if let ParamIr::Formals {
            has_bind,
            formals,
            ellipsis,
        } = &l.param
        {
            tag(h, 1);
            tag(h, u8::from(*has_bind) | (u8::from(*ellipsis) << 1));
            num(h, formals.len());
            for f in self.by_name(formals, |f| f.sym) {
                self.sym(h, f.sym);
                self.opt(h, path, ord, f.default);
            }
        }
        self.node(h, path, ord, l.body);
        self.envs.pop();
    }

    /// `f` in the env a bind's value is evaluated in: the one below a rec
    /// set's or a `let`'s own env for `inherit x`, the env of the
    /// `inherit (e)` sources for a plain set's `inherit (e) x`.
    fn in_bind_env(&mut self, b: &BindIr, rec: bool, from: usize, f: impl FnOnce(&mut Self)) {
        match (rec, b.scope) {
            (true, BindScope::Outer) => {
                let top = self.envs.pop();
                f(self);
                self.envs.extend(top);
            }
            (false, BindScope::From) => {
                self.envs.push(vec![None; from]);
                f(self);
                self.envs.pop();
            }
            _ => f(self),
        }
    }

    fn binds(
        &mut self,
        h: &mut Hasher,
        path: &str,
        ord: &mut u32,
        binds: &[BindIr],
        rec: bool,
        from: usize,
    ) {
        num(h, binds.len());
        for b in self.by_name(binds, |b| b.sym) {
            self.sym(h, b.sym);
            tag(h, scope_tag(b.scope));
            self.in_bind_env(b, rec, from, |w| w.node(h, path, ord, b.value));
        }
    }

    /// The binds of a split set or `let`, each a region named by its place
    /// in `path` and its name.
    fn split(&mut self, path: &str, ord: u32, binds: &[BindIr], rec: bool, from: usize) {
        for b in binds {
            let Ir::Region { id, body } = b.value else {
                unreachable!("a split bind is a region");
            };
            let sub = format!(
                "{path}/{ord}.{}",
                String::from_utf8_lossy(self.names.name(b.sym))
            );
            self.in_bind_env(b, rec, from, |w| {
                w.region(*id, sub, scope_tag(b.scope), body)
            });
        }
    }

    fn rec_env(binds: &[BindIr], from: &[IrRef]) -> Vec<Option<Sym>> {
        binds
            .iter()
            .map(|b| Some(b.sym))
            .chain(from.iter().map(|_| None))
            .collect()
    }

    fn attrs(&mut self, h: &mut Hasher, path: &str, ord: &mut u32, a: &AttrsIr) {
        let n = a.from.len();
        if a.rec {
            self.envs.push(Self::rec_env(a.binds, a.from));
        }
        if a.lit == NO_LIT {
            tag(h, 10);
            tag(h, u8::from(a.rec) | (u8::from(a.overrides.is_some()) << 1));
            self.binds(h, path, ord, a.binds, a.rec, n);
            self.nodes(h, path, ord, a.from);
            num(h, a.dynamic.len());
            for d in a.dynamic {
                self.node(h, path, ord, d.name);
                self.node(h, path, ord, d.value);
            }
        } else {
            tag(h, 41);
            tag(h, u8::from(a.rec));
            self.nodes(h, path, ord, a.from);
            let o = *ord;
            *ord += 1;
            let keys = a.binds.iter().map(|b| b.sym).collect();
            self.lits.push((a.lit, path.to_string(), o, keys));
            self.split(path, o, a.binds, a.rec, n);
        }
        if a.rec {
            self.envs.pop();
        }
    }

    fn let_(&mut self, h: &mut Hasher, path: &str, ord: &mut u32, l: &LetIr) {
        let n = l.from.len();
        self.envs.push(Self::rec_env(l.binds, l.from));
        if !l
            .binds
            .first()
            .is_some_and(|b| matches!(b.value, Ir::Region { .. }))
        {
            tag(h, 14);
            self.binds(h, path, ord, l.binds, true, n);
            self.nodes(h, path, ord, l.from);
        } else {
            tag(h, 42);
            self.nodes(h, path, ord, l.from);
            let o = *ord;
            *ord += 1;
            self.split(path, o, l.binds, true, n);
        }
        self.node(h, path, ord, l.body);
        self.envs.pop();
    }

    fn node(&mut self, h: &mut Hasher, path: &str, ord: &mut u32, ir: IrRef) {
        let bin = |w: &mut Self, h: &mut Hasher, ord: &mut u32, t: u8, l: IrRef, r: IrRef| {
            tag(h, t);
            w.node(h, path, ord, l);
            w.node(h, path, ord, r);
        };
        match ir {
            Ir::Const(w) => {
                tag(h, 0);
                self.konst(h, *w);
            }
            Ir::Local { depth, idx } => {
                tag(h, 1);
                num(h, *depth as usize);
                let name = (self.envs.len().checked_sub(1 + *depth as usize))
                    .and_then(|e| self.envs[e].get(*idx as usize).copied().flatten());
                match name {
                    Some(s) => {
                        tag(h, 0);
                        self.sym(h, s);
                    }
                    None => {
                        tag(h, 1);
                        num(h, *idx as usize);
                    }
                }
            }
            Ir::WithVar { sym, withs, .. } => {
                tag(h, 2);
                self.sym(h, *sym);
                num(h, withs.len());
                for d in withs.iter() {
                    num(h, *d as usize);
                }
            }
            Ir::CurPos(pos) => {
                tag(h, 3);
                num(h, pos.0.wrapping_sub(self.base) as usize);
            }
            Ir::Concat {
                force_string,
                parts,
                ..
            } => {
                tag(h, 4);
                tag(h, u8::from(*force_string));
                self.nodes(h, path, ord, parts);
            }
            Ir::SearchPath { name, .. } => {
                tag(h, 5);
                self.node(h, path, ord, name);
            }
            Ir::Select(s) => {
                tag(h, 6);
                self.select(h, path, ord, s);
            }
            Ir::SelectCount(s) => {
                tag(h, 7);
                self.select(h, path, ord, s);
            }
            Ir::Select1 {
                subject,
                sym,
                default,
                ..
            } => {
                tag(h, 8);
                self.node(h, path, ord, subject);
                self.sym(h, *sym);
                self.opt(h, path, ord, *default);
            }
            Ir::HasAttr { subject, path: p } => {
                tag(h, 9);
                self.node(h, path, ord, subject);
                self.elems(h, path, ord, p);
            }
            Ir::Attrs(a) => self.attrs(h, path, ord, a),
            Ir::List(items) => {
                tag(h, 11);
                self.nodes(h, path, ord, items);
            }
            Ir::Lambda(l) => {
                tag(h, 12);
                self.lambda(h, path, ord, l);
            }
            Ir::Apply { func, args, .. } => {
                tag(h, 13);
                self.node(h, path, ord, func);
                self.nodes(h, path, ord, args);
            }
            Ir::Let(l) => self.let_(h, path, ord, l),
            Ir::With { scope, body, .. } => {
                tag(h, 15);
                self.node(h, path, ord, scope);
                self.envs.push(vec![None]);
                self.node(h, path, ord, body);
                self.envs.pop();
            }
            Ir::If {
                cond, then, else_, ..
            } => {
                tag(h, 16);
                self.node(h, path, ord, cond);
                self.node(h, path, ord, then);
                self.node(h, path, ord, else_);
            }
            Ir::Assert { cond, body, .. } => bin(self, h, ord, 17, cond, body),
            Ir::Not(e, _) => {
                tag(h, 18);
                self.node(h, path, ord, e);
            }
            Ir::Neg(e, _) => {
                tag(h, 19);
                self.node(h, path, ord, e);
            }
            Ir::Eq(l, r, _) => bin(self, h, ord, 20, l, r),
            Ir::Ne(l, r, _) => bin(self, h, ord, 21, l, r),
            Ir::Lt(l, r, _) => bin(self, h, ord, 22, l, r),
            Ir::Gt(l, r, _) => bin(self, h, ord, 23, l, r),
            Ir::Le(l, r, _) => bin(self, h, ord, 24, l, r),
            Ir::Ge(l, r, _) => bin(self, h, ord, 25, l, r),
            Ir::And(l, r, _) => bin(self, h, ord, 26, l, r),
            Ir::Or(l, r, _) => bin(self, h, ord, 27, l, r),
            Ir::Impl(l, r, _) => bin(self, h, ord, 28, l, r),
            Ir::Update(l, r, _) => bin(self, h, ord, 29, l, r),
            Ir::ConcatLists(l, r, _) => bin(self, h, ord, 30, l, r),
            Ir::Sub(l, r, _) => bin(self, h, ord, 31, l, r),
            Ir::Mul(l, r, _) => bin(self, h, ord, 32, l, r),
            Ir::Div(l, r, _) => bin(self, h, ord, 33, l, r),
            Ir::Region { id, body } => {
                tag(h, 40);
                let sub = format!("{path}/r{ord}");
                *ord += 1;
                self.region(*id, sub, 0, body);
            }
        }
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

fn lossy(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

impl State {
    /// The observations of `entries` and of the traces they read, each once.
    fn flatten(&self, entries: &[u32]) -> Vec<u32> {
        let mut nodes = FxHashSet::default();
        let mut obs = FxHashSet::default();
        let mut stack = vec![];
        let mut visit = |es: &[u32], stack: &mut Vec<u32>| {
            scan(es, |e| match e {
                Entry::Obs(o) => {
                    obs.insert(o);
                }
                Entry::Edge(t) => stack.push(t),
            })
        };
        visit(entries, &mut stack);
        while let Some(t) = stack.pop() {
            let t = t as usize;
            if t == 0 || !nodes.insert(t) {
                continue;
            }
            let n = self.nodes[t + 1] as usize;
            visit(&self.nodes[t + 2..t + 2 + n], &mut stack);
        }
        let mut v: Vec<u32> = obs.into_iter().collect();
        v.sort_unstable();
        v
    }

    fn region_line(&self, id: u32) -> String {
        match self.regions.get(&id) {
            Some(r) => format!("R\t{}\t{}\t{}", lossy(&r.file), r.path, hex(&r.hash)),
            None => format!("R\t?\t{id}\t"),
        }
    }

    fn prov_line(&self, ev: &Evaluator, i: usize) -> String {
        match &self.provs[i] {
            Prov::Lit(l) => match self.lits.get(l) {
                Some(l) => format!(
                    "P\t{i}\tlit\t{}\t{}\t{}",
                    lossy(&l.file),
                    l.region,
                    l.ordinal
                ),
                None => format!("P\t{i}\tlit?\t{l}"),
            },
            Prov::Dir(d) => format!("P\t{i}\tdir\t{}", lossy(d)),
            Prov::Union(a, b) => format!("P\t{i}\tu\t{a}\t{b}"),
            Prov::Remove(a, names) => {
                let names: Vec<_> = names.iter().map(|n| ev.interner.name_str(*n)).collect();
                format!("P\t{i}\tx\t{a}\t{}", names.join(" "))
            }
        }
    }

    /// The whole read of `p`, with where it happened.
    fn all_line(&self, ev: &Evaluator, p: u32) -> String {
        let Some((at, nix, frames)) = self.sites.get(&p) else {
            return format!("A\t{p}\t?\t?\t");
        };
        let nix = ev.describe_pos(crate::eval::Pos(*nix));
        let mut st = String::new();
        for f in frames.iter().rev().take(12) {
            let _ = write!(
                st,
                " < {}",
                ev.describe_pos(crate::eval::Pos((f >> 32) as u32))
            );
        }
        format!("A\t{p}\t{}:{}\t{nix}\t{st}", at.file(), at.line())
    }

    fn obs_line(&self, ev: &Evaluator, id: u32) -> String {
        match &self.obs[id as usize] {
            O::Region(r) => self.region_line(*r),
            O::Key(p, k) => format!("K\t{p}\t{}", ev.interner.name_str(*k)),
            O::All(p) => self.all_line(ev, *p),
            O::Pos(f, off) => format!("X\t{}\t{off}", lossy(f)),
            O::Out(l) => l.clone(),
        }
    }

    /// `obs` as lines: `R` regions, `P` the provenances the `K` lookups and
    /// `A` whole reads name, `X` the last position per file, then what the
    /// tracing cache records.
    fn text(&self, ev: &Evaluator, obs: &[u32]) -> String {
        let mut out = String::new();
        let mut regions = Vec::new();
        let mut keys = Vec::new();
        let mut alls = Vec::new();
        let mut pos: FxHashMap<&Arc<[u8]>, u32> = FxHashMap::default();
        let mut outs = Vec::new();
        for id in obs {
            match &self.obs[*id as usize] {
                O::Region(r) => regions.push(*r),
                O::Key(p, k) => keys.push((*p, *k)),
                O::All(p) => alls.push(*p),
                O::Pos(f, off) => {
                    let o = pos.entry(f).or_insert(*off);
                    *o = (*o).max(*off);
                }
                O::Out(l) => outs.push(l),
            }
        }
        regions.sort_unstable();
        for id in regions {
            out.push_str(&self.region_line(id));
            out.push('\n');
        }
        let mut used = vec![false; self.provs.len()];
        let mut stack: Vec<u32> = keys
            .iter()
            .map(|k| k.0)
            .chain(alls.iter().copied())
            .collect();
        while let Some(p) = stack.pop() {
            if std::mem::replace(&mut used[p as usize], true) {
                continue;
            }
            match &self.provs[p as usize] {
                Prov::Union(a, b) => stack.extend([*a, *b]),
                Prov::Remove(a, _) => stack.push(*a),
                Prov::Lit(_) | Prov::Dir(_) => {}
            }
        }
        for (i, u) in used.iter().enumerate() {
            if *u {
                out.push_str(&self.prov_line(ev, i));
                out.push('\n');
            }
        }
        for (p, k) in keys {
            let _ = writeln!(out, "K\t{p}\t{}", ev.interner.name_str(k));
        }
        for p in alls {
            out.push_str(&self.all_line(ev, p));
            out.push('\n');
        }
        for (f, off) in pos {
            let _ = writeln!(out, "X\t{}\t{off}", lossy(f));
        }
        for l in outs {
            out.push_str(l);
            out.push('\n');
        }
        out
    }

    /// The trace graph of the run in `dir`: `obs` (a line per observation),
    /// `provs`, `nodes` (the traces as little-endian `u32`, see `nodes`) and
    /// `roots` (the trace of each job, a line each).
    fn write_dag(
        &self,
        ev: &Evaluator,
        dir: &std::path::Path,
        roots: &[u32],
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut o = String::new();
        for id in 0..self.obs.len() as u32 {
            // a name may hold a newline, and a line is an id
            o.push_str(&self.obs_line(ev, id).replace('\n', "\\n"));
            o.push('\n');
        }
        std::fs::write(dir.join("obs"), o)?;
        let mut p = String::new();
        for i in 0..self.provs.len() {
            p.push_str(&self.prov_line(ev, i).replace('\n', "\\n"));
            p.push('\n');
        }
        std::fs::write(dir.join("provs"), p)?;
        let mut w = std::io::BufWriter::new(std::fs::File::create(dir.join("nodes"))?);
        for x in &self.nodes {
            std::io::Write::write_all(&mut w, &x.to_le_bytes())?;
        }
        std::io::Write::flush(&mut w)?;
        let mut q = String::new();
        for (i, (parent, sym)) in self.paths.iter().enumerate() {
            let parent = if *parent == NO_PATH {
                "-".to_string()
            } else {
                parent.to_string()
            };
            let fp = self.fps[i].map_or("-".to_string(), |h| hex(&h));
            let name = ev.interner.name_str(*sym).replace(['\t', '\n'], " ");
            let (h, k, b) = (self.head[i], self.kind[i], self.born[i]);
            let _ = writeln!(
                q,
                "{i}\t{parent}\t{name}\t{fp}\t{}\t{h}\t{k}\t{b}",
                self.qtrace[i]
            );
        }
        std::fs::write(dir.join("queries"), q)?;
        let r: String = roots.iter().map(|t| format!("{t}\n")).collect();
        std::fs::write(dir.join("roots"), r)
    }
}

/// The trace of each element of `v` when it is a list, 0 for none.
fn roots(v: Word) -> Vec<u32> {
    let mut v = v;
    if v.is_thunk() {
        let st = v.thunk_state_cell().get();
        if v.thunk_unforced(st) || st.is_blackhole() {
            return Vec::new();
        }
        v = v.thunk_value(st);
    }
    let Some(l) = v.as_list() else {
        return Vec::new();
    };
    l.cells()
        .iter()
        .map(|c| {
            let w = c.get();
            let st = if w.is_thunk() {
                w.thunk_state_cell().get()
            } else {
                Word::EMPTY
            };
            if st.is_traced() { st.traced_id() } else { 0 }
        })
        .collect()
}

/// Writes the trace of the whole run to the file `IETS_FINE_TRACE` named.
/// When `v` is a list, writes the trace of each element to `DIR/INDEX` for
/// `IETS_FINE_JOBS=DIR`, and the trace graph to `IETS_FINE_DAG=DIR`.
pub fn dump(ev: &Evaluator, v: Option<Word>) {
    with(|s| {
        if let Some(p) = &s.out {
            let all = s.flatten(&s.log[s.frames[0].0..]);
            if let Err(e) = std::fs::write(p, s.text(ev, &all)) {
                errln!("fine trace: writing {}: {e}", p.display());
            }
        }
        let roots = v.map(roots).unwrap_or_default();
        if let Some(dir) = std::env::var_os("IETS_FINE_JOBS") {
            for (i, t) in roots.iter().enumerate() {
                let obs = if *t == 0 {
                    Vec::new()
                } else {
                    s.flatten(&[EDGE | t])
                };
                let p = std::path::Path::new(&dir).join(i.to_string());
                if let Err(e) = std::fs::write(&p, s.text(ev, &obs)) {
                    errln!("fine trace: writing {}: {e}", p.display());
                }
            }
        }
        if let Some(dir) = std::env::var_os("IETS_FINE_DAG")
            && let Err(e) = s.write_dag(ev, std::path::Path::new(&dir), &roots)
        {
            errln!("fine trace: writing {}: {e}", dir.display());
        }
        errln!(
            "fine trace: {} frames, {} words of traces, {} observations, {} provenances",
            s.serial,
            s.nodes.len(),
            s.obs.len(),
            s.provs.len()
        );
    });
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Missing => "missing",
        Kind::Regular => "regular",
        Kind::Directory => "directory",
        Kind::Symlink => "symlink",
        Kind::Other => "other",
    }
}

/// `iets fine-regions FILE[=AS]...`: the regions and literals of each file,
/// compiled as if it were at `AS`, for the validation of a trace against
/// another revision.
pub fn regions_main(argv: &[String]) -> i32 {
    start(None);
    let mut ev = Evaluator::new();
    for arg in argv {
        let (f, at) = arg.split_once('=').unwrap_or((arg, arg));
        let path = crate::compile::canon_path(at.as_bytes());
        let src = match std::fs::read(f) {
            Ok(s) => s,
            Err(e) => {
                errln!("{f}: {e}");
                continue;
            }
        };
        let dir = crate::eval::dir_of(&path).to_vec();
        if let Err(e) = ev.compile_file(&path, src, &dir) {
            println!("E\t{at}\t{}", e.msg);
            continue;
        }
        with(|s| {
            let mut rs: Vec<_> = s.regions.values().filter(|r| *r.file == *path).collect();
            rs.sort_by(|a, b| a.path.cmp(&b.path));
            for r in rs {
                println!("R\t{at}\t{}\t{}", r.path, hex(&r.hash));
            }
            let mut ls: Vec<_> = s.lits.values().filter(|l| *l.file == *path).collect();
            ls.sort_by(|a, b| (&a.region, a.ordinal).cmp(&(&b.region, b.ordinal)));
            for l in ls {
                let mut keys: Vec<_> = l.keys.iter().map(|k| ev.interner.name_str(*k)).collect();
                keys.sort();
                println!("L\t{at}\t{}\t{}\t{}", l.region, l.ordinal, keys.join(" "));
            }
            s.regions.retain(|_, r| *r.file != *path);
            s.lits.retain(|_, l| *l.file != *path);
        });
    }
    0
}

/// One change of a step to a set's names: the names, or `None` for all.
type NameChange = (u8, Option<Arc<FxHashSet<String>>>);

/// What the steps of a change file changed, in the terms of the trace.
#[derive(Default)]
struct Changes {
    steps: Vec<String>,
    /// By "file\tregion".
    regions: FxHashMap<String, u128>,
    files: FxHashMap<String, u128>,
    /// By "file\tregion\tordinal".
    lits: FxHashMap<String, Vec<NameChange>>,
    dirs: FxHashMap<String, Vec<NameChange>>,
    modified: FxHashMap<String, u128>,
    trees: FxHashMap<String, u128>,
    stats: FxHashMap<String, u128>,
    /// The first changed byte of each file a step edited.
    first: FxHashMap<String, Vec<(u8, u32)>>,
}

fn bit(s: u8) -> u128 {
    1 << s
}

impl Changes {
    /// `step\tCHANGE...` lines, as `fchg.sh` writes them.
    fn read(text: &str) -> Changes {
        let mut c = Changes::default();
        let mut idx: FxHashMap<String, u8> = FxHashMap::default();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 3 {
                continue;
            }
            let s = *idx.entry(f[0].to_string()).or_insert_with(|| {
                c.steps.push(f[0].to_string());
                assert!(c.steps.len() <= 128, "fine-check: at most 128 steps");
                (c.steps.len() - 1) as u8
            });
            let names = |n: &str| {
                if n == "*" {
                    None
                } else {
                    Some(Arc::new(
                        n.split(' ')
                            .filter(|x| !x.is_empty())
                            .map(String::from)
                            .collect(),
                    ))
                }
            };
            let or = |m: &mut FxHashMap<String, u128>, k: &str| {
                *m.entry(k.to_string()).or_insert(0) |= bit(s)
            };
            match (f[1], f.len()) {
                ("CR", 4..) => *c.regions.entry(f[2..4].join("\t")).or_insert(0) |= bit(s),
                ("CF", _) => or(&mut c.files, f[2]),
                ("CL", 6..) => c
                    .lits
                    .entry(f[2..5].join("\t"))
                    .or_default()
                    .push((s, names(f[5]))),
                ("CD", 4..) => c
                    .dirs
                    .entry(f[2].into())
                    .or_default()
                    .push((s, names(f[3]))),
                ("CM", _) => or(&mut c.modified, f[2]),
                ("CT", _) => or(&mut c.trees, f[2]),
                ("CS", _) => or(&mut c.stats, f[2]),
                ("CX", 4..) => c
                    .first
                    .entry(f[2].into())
                    .or_default()
                    .push((s, f[3].parse().unwrap_or(0))),
                _ => {}
            }
        }
        c
    }
}

/// The changes each provenance's sets and listings went through, from the
/// `provs` of a graph.
fn leaves(c: &Changes, provs: &str) -> Vec<Vec<NameChange>> {
    let mut leaves: Vec<Vec<NameChange>> = Vec::new();
    for line in provs.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let e = match f.get(2).copied() {
            Some("lit") => c.lits.get(&f[3..6].join("\t")).cloned().unwrap_or_default(),
            Some("dir") => c.dirs.get(f[3]).cloned().unwrap_or_default(),
            Some("u") => {
                let (a, b): (usize, usize) = (f[3].parse().unwrap(), f[4].parse().unwrap());
                leaves[a].iter().chain(leaves[b].iter()).cloned().collect()
            }
            Some("x") => leaves[f[3].parse::<usize>().unwrap()].clone(),
            _ => (0..c.steps.len() as u8).map(|s| (s, None)).collect(),
        };
        leaves.push(e);
    }
    leaves
}

fn read_words(path: &std::path::Path) -> std::io::Result<Vec<u32>> {
    Ok(std::fs::read(path)?
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect())
}

/// The steps each job of the trace graph in `dir` must be evaluated again
/// for, with every query read as the trace of its force: `(job, mask)`.
fn check_dag(
    c: &Changes,
    dir: &std::path::Path,
    skip: Option<&str>,
) -> std::io::Result<Vec<(usize, u128)>> {
    let every = if c.steps.is_empty() {
        0
    } else {
        u128::MAX >> (128 - c.steps.len())
    };
    let leaves = leaves(c, &std::fs::read_to_string(dir.join("provs"))?);
    let obs = ObsIndex::new(&std::fs::read_to_string(dir.join("obs"))?, skip).changed(c, &leaves);
    let mut words = read_words(&dir.join("nodes"))?;
    // a trace comes after the traces it reads, so one pass in order does;
    // the stamp word of each trace, of no use here, takes the index of its
    // mask in `masks` plus one, or 0 for none
    let mut masks: Vec<u128> = Vec::new();
    let mut at = 2;
    while at < words.len() {
        let n = words[at + 1] as usize;
        let mut m = 0;
        scan(&words[at + 2..at + 2 + n], |e| {
            m |= match e {
                Entry::Obs(o) => obs[o as usize],
                Entry::Edge(t) => match words[t as usize] {
                    0 => 0,
                    k => masks[k as usize - 1],
                },
            }
        });
        words[at] = if m == 0 {
            0
        } else {
            masks.push(m);
            masks.len() as u32
        };
        at += 2 + n;
    }
    let mut out = Vec::new();
    for (i, line) in std::fs::read_to_string(dir.join("roots"))?
        .lines()
        .enumerate()
    {
        let t: usize = line.parse().unwrap_or(0);
        let m = match t {
            0 => 0,
            t if t >= words.len() => every,
            t => match words[t] {
                0 => 0,
                k => masks[k as usize - 1],
            },
        };
        if m != 0 {
            out.push((i, m));
        }
    }
    Ok(out)
}

/// A path of a trace graph, with what the first path of its ring holds.
struct GPath {
    key: u32,
    /// The index of the parent, or `NO_PATH`.
    parent: u32,
    head: u32,
    fp: String,
    trace: u32,
    kind: u8,
    /// Where the traces stood when the path was made: a trace that closed
    /// before knew no route through it.
    born: u32,
}

/// The traces of a graph are in blocks of this many words (a power of
/// two), by where they start: a check reads only the blocks that what
/// changed can reach.
const BLOCK: u32 = 12;
/// A block marks its readers by parts of this many words that have a trace
/// that is not clean: a smaller part wakes fewer blocks for nothing.
const PART: u32 = 8;

/// By key, a list: its values, in the order they came.
#[derive(Default)]
struct Lists {
    at: Vec<u32>,
    to: Vec<u32>,
}

impl Lists {
    /// From `(key, value)` pairs of keys below `n`.
    fn new(n: usize, pairs: &[(u32, u32)]) -> Lists {
        let mut at = vec![0u32; n + 1];
        for (k, _) in pairs {
            at[*k as usize + 1] += 1;
        }
        for i in 0..n {
            at[i + 1] += at[i];
        }
        let mut fill = at.clone();
        let mut to = vec![0u32; pairs.len()];
        for (k, b) in pairs {
            to[fill[*k as usize] as usize] = *b;
            fill[*k as usize] += 1;
        }
        Lists { at, to }
    }

    fn get(&self, k: u32) -> &[u32] {
        &self.to[self.at[k as usize] as usize..self.at[k as usize + 1] as usize]
    }
}

fn mark(visit: &mut [u64], blocks: &[u32]) {
    for b in blocks {
        visit[*b as usize >> 6] |= 1 << (b & 63);
    }
}

/// A trace graph a server keeps for many checks.
struct Graph {
    dir: String,
    /// The step that wrote it: a query the graphs of the newest step read
    /// is checked against their traces.
    step: u32,
    provs: String,
    index: ObsIndex,
    words: Vec<u32>,
    roots: Vec<u32>,
    paths: Vec<GPath>,
    /// By key, its path, or `NO_PATH`.
    by_key: Vec<u32>,
    /// The other paths of each ring of more than one, by its first.
    rings: Lists,
    /// The paths below each path, from `below[i]` to `below[i + 1]`.
    below: Vec<u32>,
    children: Vec<u32>,
    /// The paths a query read whose force left a trace, as `(trace, path)`
    /// in order.
    forced: Vec<(u32, u32)>,
    /// By path, after a refresh: `LOCAL` when the newest step that read its
    /// key is this one, `OWNER` when a check takes the trace of the key here.
    mine: Vec<u8>,
    /// By block, its first trace, or `NO_PATH`.
    starts: Vec<u32>,
    /// The blocks that hold each observation, the later blocks that read a
    /// trace of each part, and the blocks with a read that the status of
    /// each first path decides.
    by_obs: Lists,
    readers: Lists,
    by_head: Lists,
    /// After a check: by trace, 0 for clean, else the index of its status
    /// in `stat` plus one; the suspects of a status are in `sets`. `dirty`
    /// has the traces that are not clean.
    stat: Vec<(bool, u32)>,
    sets: Sets,
    dirty: Vec<u32>,
}

/// The observations of a graph, by what can change them.
#[derive(Default)]
struct ObsIndex {
    n: usize,
    every: Vec<u32>,
    regions: FxHashMap<String, Vec<u32>>,
    region_files: FxHashMap<String, Vec<u32>>,
    /// Name lookups by name, with their provenance, and by provenance.
    names: FxHashMap<String, Vec<(u32, u32)>>,
    keys_of: FxHashMap<u32, Vec<u32>>,
    wholes: FxHashMap<u32, Vec<u32>>,
    positions: FxHashMap<String, Vec<(u32, u32)>>,
    files: FxHashMap<String, Vec<u32>>,
    trees: FxHashMap<String, Vec<u32>>,
    stats: FxHashMap<String, Vec<u32>>,
    dirs: FxHashMap<String, Vec<u32>>,
}

impl ObsIndex {
    /// The index of the observations in `text`, without the whole reads
    /// whose place holds `skip`.
    fn new(text: &str, skip: Option<&str>) -> ObsIndex {
        let mut x = ObsIndex::default();
        let push = |m: &mut FxHashMap<String, Vec<u32>>, k: &str, i: u32| {
            m.entry(k.to_string()).or_default().push(i)
        };
        for (i, line) in text.lines().enumerate() {
            let i = i as u32;
            x.n += 1;
            let mut f = line.splitn(4, '\t');
            let (kind, a, b) = (
                f.next().unwrap_or(""),
                f.next().unwrap_or(""),
                f.next().unwrap_or(""),
            );
            match kind {
                "R" if a == "?" => x.every.push(i),
                "R" => {
                    // the file and the region path are next to each other in the line
                    push(&mut x.regions, &line[2..2 + a.len() + 1 + b.len()], i);
                    push(&mut x.region_files, a, i);
                }
                "K" => {
                    let p = a.parse().unwrap_or(0);
                    x.names.entry(b.to_string()).or_default().push((i, p));
                    x.keys_of.entry(p).or_default().push(i);
                }
                "A" if skip.is_some_and(|t| line.contains(t)) => {}
                "A" => x.wholes.entry(a.parse().unwrap_or(0)).or_default().push(i),
                "X" => x
                    .positions
                    .entry(a.to_string())
                    .or_default()
                    .push((i, b.parse().unwrap_or(0))),
                "F" | "C" => push(&mut x.files, a, i),
                "T" => push(&mut x.trees, a, i),
                "S" => push(&mut x.stats, a, i),
                "D" => push(&mut x.dirs, a, i),
                _ => {}
            }
        }
        x
    }

    /// The steps of `c` that changed each observation.
    fn changed(&self, c: &Changes, leaves: &[Vec<NameChange>]) -> Vec<u128> {
        let mut d = vec![0u128; self.n];
        self.each_changed(c, leaves, &mut |i, m| d[i as usize] |= m);
        d
    }

    /// Gives `put` the observations that steps of `c` changed, with the
    /// steps; one observation can come many times.
    fn each_changed(
        &self,
        c: &Changes,
        leaves: &[Vec<NameChange>],
        put: &mut dyn FnMut(u32, u128),
    ) {
        fn or(put: &mut dyn FnMut(u32, u128), ids: Option<&Vec<u32>>, m: u128) {
            for i in ids.into_iter().flatten() {
                put(*i, m);
            }
        }
        let steps = |v: &[NameChange]| v.iter().fold(0, |m, (s, _)| m | bit(*s));
        if !c.steps.is_empty() {
            or(put, Some(&self.every), u128::MAX >> (128 - c.steps.len()));
        }
        for (k, m) in &c.regions {
            or(put, self.regions.get(k), *m);
        }
        for (maps, idx) in [
            (&c.files, &self.region_files),
            (&c.modified, &self.files),
            (&c.trees, &self.trees),
            (&c.stats, &self.stats),
        ] {
            for (k, m) in maps {
                or(put, idx.get(k), *m);
            }
        }
        for (k, v) in &c.dirs {
            or(put, self.dirs.get(k), steps(v));
        }
        for (f, v) in &c.first {
            for (i, off) in self.positions.get(f).into_iter().flatten() {
                let m = v
                    .iter()
                    .filter(|(_, o)| o <= off)
                    .fold(0, |m, (s, _)| m | bit(*s));
                put(*i, m);
            }
        }
        let mut names: FxHashSet<&str> = FxHashSet::default();
        for (p, l) in leaves.iter().enumerate() {
            let p = p as u32;
            or(put, self.wholes.get(&p), steps(l));
            for (s, n) in l {
                match n {
                    None => or(put, self.keys_of.get(&p), bit(*s)),
                    Some(n) => names.extend(n.iter().map(String::as_str)),
                }
            }
        }
        for n in names {
            for (i, p) in self.names.get(n).into_iter().flatten() {
                let l = &leaves[*p as usize];
                let m = l
                    .iter()
                    .filter(|(_, s)| s.as_ref().is_some_and(|s| s.contains(n)))
                    .fold(0, |m, (s, _)| m | bit(*s));
                put(*i, m);
            }
        }
    }
}

/// The keys of the queries every graph names: a path in the root set.
#[derive(Default)]
struct Keys {
    ids: FxHashMap<(u32, String), u32>,
    rows: Vec<(u32, String)>,
}

impl Keys {
    fn id(&mut self, parent: u32, name: &str) -> u32 {
        if let Some(k) = self.ids.get(&(parent, name.to_string())) {
            return *k;
        }
        let k = self.rows.len() as u32;
        self.rows.push((parent, name.to_string()));
        self.ids.insert((parent, name.to_string()), k);
        k
    }

    fn names(&self, mut k: u32) -> Vec<&str> {
        let mut v = Vec::new();
        while k != NO_PATH {
            v.push(self.rows[k as usize].1.as_str());
            k = self.rows[k as usize].0;
        }
        v.reverse();
        v
    }
}

impl Graph {
    /// The paths of the graph in `dir`.
    fn paths(dir: &str, keys: &mut Keys) -> Vec<GPath> {
        let d = std::path::Path::new(dir);
        let mut paths: Vec<GPath> = Vec::new();
        if let Ok(t) = std::fs::read_to_string(d.join("queries")) {
            for line in t.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                let num = |s: &str| s.parse::<u32>().unwrap_or(0);
                let parent = if f[1] == "-" { NO_PATH } else { num(f[1]) };
                let pk = if parent == NO_PATH {
                    NO_PATH
                } else {
                    paths[parent as usize].key
                };
                let head = num(f[5]);
                let i = paths.len() as u32;
                let (fp, trace, kind) = if head == i {
                    (f[3].to_string(), num(f[4]), num(f[6]) as u8)
                } else {
                    let h = &paths[head as usize];
                    (h.fp.clone(), h.trace, h.kind)
                };
                paths.push(GPath {
                    key: keys.id(pk, f[2]),
                    parent,
                    head,
                    fp,
                    trace,
                    kind,
                    born: num(f[7]),
                });
            }
        }
        paths
    }

    fn load(dir: String, step: u32, paths: Vec<GPath>) -> std::io::Result<Graph> {
        let d = std::path::Path::new(&dir);
        let mut rings: Vec<(u32, u32)> = Vec::new();
        let mut forced: Vec<(u32, u32)> = Vec::new();
        let mut below = vec![0u32; paths.len() + 1];
        for (i, p) in paths.iter().enumerate() {
            if p.head != i as u32 {
                rings.push((p.head, i as u32));
            }
            if p.trace != 0 && p.kind != kind::UNREAD {
                forced.push((p.trace, i as u32));
            }
            if p.parent != NO_PATH {
                below[p.parent as usize + 1] += 1;
            }
        }
        for i in 0..paths.len() {
            below[i + 1] += below[i];
        }
        let mut fill = below.clone();
        let mut children = vec![0u32; paths.len()];
        for (i, p) in paths.iter().enumerate() {
            if p.parent != NO_PATH {
                children[fill[p.parent as usize] as usize] = i as u32;
                fill[p.parent as usize] += 1;
            }
        }
        children.truncate(*below.last().unwrap_or(&0) as usize);
        forced.sort_unstable();
        let rings = Lists::new(paths.len(), &rings);
        let mut by_key = vec![NO_PATH; paths.iter().map(|p| p.key as usize + 1).max().unwrap_or(0)];
        for (i, p) in paths.iter().enumerate() {
            by_key[p.key as usize] = i as u32;
        }
        let (provs, index, words, roots) = (
            std::fs::read_to_string(d.join("provs"))?,
            ObsIndex::new(&std::fs::read_to_string(d.join("obs"))?, None),
            read_words(&d.join("nodes"))?,
            std::fs::read_to_string(d.join("roots"))?
                .lines()
                .map(|l| l.parse().unwrap_or(0))
                .collect(),
        );
        let mut g = Graph {
            dir,
            step,
            provs,
            index,
            words,
            roots,
            by_key,
            paths,
            rings,
            below,
            children,
            forced,
            mine: Vec::new(),
            starts: Vec::new(),
            by_obs: Lists::default(),
            readers: Lists::default(),
            by_head: Lists::default(),
            stat: Vec::new(),
            sets: Sets::default(),
            dirty: Vec::new(),
        };
        g.reach();
        Ok(g)
    }

    /// Makes the blocks and what reaches them, and every trace clean.
    fn reach(&mut self) {
        /// `(key, block)` pairs, each once: `last` has the last block of a key, plus one.
        struct Pairs(Vec<u32>, Vec<(u32, u32)>);
        impl Pairs {
            fn add(&mut self, k: u32, b: u32) -> bool {
                let new = std::mem::replace(&mut self.0[k as usize], b + 1) != b + 1;
                if new {
                    self.1.push((k, b));
                }
                new
            }
        }
        let mut words = std::mem::take(&mut self.words);
        let nb = (words.len() >> BLOCK) + 1;
        self.starts = vec![NO_PATH; nb];
        let mut obs = Pairs(vec![0; self.index.n], Vec::new());
        let parts = (words.len() >> PART) + 1;
        let mut edges = Pairs(vec![0; parts], Vec::new());
        let mut read = Pairs(vec![0; self.paths.len()], Vec::new());
        let mut heads = Pairs(vec![0; self.paths.len()], Vec::new());
        let mut at = 2;
        while at < words.len() {
            let b = (at >> BLOCK) as u32;
            if self.starts[b as usize] == NO_PATH {
                self.starts[b as usize] = at as u32;
            }
            words[at] = 0;
            let n = words[at + 1] as usize;
            let mut k = at + 2;
            while k < at + 2 + n {
                let w = words[k];
                k += 1;
                if w & EDGE == 0 {
                    obs.add(w, b);
                    continue;
                }
                if w & QUERY != 0 {
                    let h = self.paths[words[k] as usize].head;
                    k += 1;
                    if read.add(h, b) {
                        heads.add(h, b);
                        for m in self.ring(h) {
                            let p = self.paths[m as usize].parent;
                            if p != NO_PATH {
                                heads.add(self.paths[p as usize].head, b);
                            }
                        }
                    }
                }
                let t = w & !(EDGE | QUERY);
                if t != 0 && t >> BLOCK != b {
                    edges.add(t >> PART, b);
                }
            }
            at += 2 + n;
        }
        read.1.clear();
        self.by_obs = Lists::new(self.index.n, &obs.1);
        self.readers = Lists::new(parts, &edges.1);
        self.by_head = Lists::new(self.paths.len(), &heads.1);
        self.words = words;
    }

    fn path_of(&self, key: u32) -> Option<u32> {
        self.by_key
            .get(key as usize)
            .copied()
            .filter(|p| *p != NO_PATH)
    }

    fn status(&self, t: u32) -> Option<(bool, u32)> {
        match self.words[t as usize] {
            0 => None,
            k => Some(self.stat[k as usize - 1]),
        }
    }

    fn ring(&self, h: u32) -> impl Iterator<Item = u32> + '_ {
        std::iter::once(h).chain(self.rings.get(h).iter().copied())
    }

    fn read(&self, i: u32) -> bool {
        self.paths[i as usize].kind != kind::UNREAD
    }
}

/// Sets of keys, each kept once.
#[derive(Default)]
struct Sets {
    sets: Vec<Vec<u32>>,
    ids: FxHashMap<Vec<u32>, u32>,
}

impl Sets {
    fn id(&mut self, v: Vec<u32>) -> u32 {
        if v.is_empty() {
            return 0;
        }
        if let Some(i) = self.ids.get(&v) {
            return *i;
        }
        let i = self.sets.len() as u32 + 1;
        self.sets.push(v.clone());
        self.ids.insert(v, i);
        i
    }

    fn get(&self, i: u32) -> &[u32] {
        if i == 0 {
            &[]
        } else {
            &self.sets[i as usize - 1]
        }
    }

    /// What `a` and `b` together require.
    fn join(&mut self, a: St, b: St) -> St {
        if a.0 || b.0 {
            return HARD;
        }
        if b.1 == 0 || a.1 == b.1 {
            return a;
        }
        if a.1 == 0 {
            return b;
        }
        let mut v: Vec<u32> = self.get(a.1).iter().chain(self.get(b.1)).copied().collect();
        v.sort_unstable();
        v.dedup();
        self.status(v)
    }

    fn status(&mut self, v: Vec<u32>) -> St {
        match v.len() {
            0 => CLEAN,
            n if n > MAX_SUSPECTS => HARD,
            _ => (false, self.id(v)),
        }
    }
}

/// A status: changed, or the suspects (an id of `Sets`) whose fingerprints
/// must hold.
type St = (bool, u32);
const CLEAN: St = (false, 0);
const HARD: St = (true, 0);
/// Past this many suspects a status is hard: it bounds the work of a check.
const MAX_SUSPECTS: usize = 256;

/// The resident checker: graphs, newest step last.
#[derive(Default)]
struct Server {
    graphs: Vec<Graph>,
    keys: Keys,
    /// The graphs that `load` started to read.
    loading: Vec<std::thread::JoinHandle<std::io::Result<Graph>>>,
    /// By key, made again after a load or a drop: the graph of the newest
    /// step that read it, plus one, where a check takes its trace; and how
    /// many graphs read it.
    owner: Vec<u32>,
    readers: Vec<u32>,
    fresh: bool,
}

/// What the objects of a graph that a route from the root set reaches
/// require by now: those whose force or whose holder, by any path made by
/// now, changed or is a suspect.
struct Moved {
    on: Vec<St>,
    /// The paths made by now are those before it.
    now: u32,
    /// The first paths whose status `set` changed.
    touched: Vec<u32>,
}

impl Moved {
    fn set(&mut self, g: &Graph, sets: &mut Sets, h: u32, st: St) {
        let mut stack = vec![(h, st)];
        while let Some((h, st)) = stack.pop() {
            let old = self.on[h as usize];
            let new = sets.join(old, st);
            if new == old {
                continue;
            }
            self.on[h as usize] = new;
            self.touched.push(h);
            for r in g.ring(h).filter(|r| *r < self.now) {
                let (a, b) = (g.below[r as usize], g.below[r as usize + 1]);
                for c in &g.children[a as usize..b as usize] {
                    if *c < self.now {
                        stack.push((g.paths[*c as usize].head, new));
                    }
                }
            }
        }
    }

    /// Makes the paths made before trace offset `at`.
    fn advance(&mut self, g: &Graph, sets: &mut Sets, at: u32) {
        while (self.now as usize) < g.paths.len() && g.paths[self.now as usize].born <= at {
            let c = self.now;
            self.now += 1;
            let p = g.paths[c as usize].parent;
            if p != NO_PATH {
                let st = self.on[g.paths[p as usize].head as usize];
                if st != CLEAN {
                    self.set(g, sets, g.paths[c as usize].head, st);
                }
            }
        }
    }

    /// What the holders of the paths of the ring of `h` made by now require.
    fn above(&self, g: &Graph, sets: &mut Sets, h: u32) -> St {
        let mut st = CLEAN;
        for m in g.ring(h).filter(|m| *m < self.now) {
            let p = g.paths[m as usize].parent;
            if p != NO_PATH {
                st = sets.join(st, self.on[g.paths[p as usize].head as usize]);
            }
        }
        st
    }
}

impl Server {
    fn refresh(&mut self) {
        if self.fresh {
            return;
        }
        let n = self.keys.rows.len();
        self.owner.clear();
        self.owner.resize(n, 0);
        self.readers.clear();
        self.readers.resize(n, 0);
        let mut order: Vec<usize> = (0..self.graphs.len()).collect();
        order.sort_by_key(|i| std::cmp::Reverse(self.graphs[*i].step));
        for i in order {
            let g = &self.graphs[i];
            for (j, p) in g.paths.iter().enumerate() {
                if g.read(j as u32) {
                    let k = p.key as usize;
                    if self.owner[k] == 0 {
                        self.owner[k] = i as u32 + 1;
                    }
                    self.readers[k] += 1;
                }
            }
        }
        for i in 0..self.graphs.len() {
            let g = &self.graphs[i];
            let mine = (0..g.paths.len() as u32)
                .map(|j| match g.read(j) {
                    true => {
                        let o = self.owner[g.paths[j as usize].key as usize] as usize - 1;
                        ((self.graphs[o].step == g.step) as u8 * LOCAL) | ((o == i) as u8 * OWNER)
                    }
                    false => 0,
                })
                .collect();
            self.graphs[i].mine = mine;
        }
        self.fresh = true;
    }

    /// Takes the graphs that `load` started.
    fn settle(&mut self) -> std::io::Result<()> {
        for h in std::mem::take(&mut self.loading) {
            self.graphs.push(h.join().expect("a loader thread")?);
            self.fresh = false;
        }
        Ok(())
    }

    /// Drops those of `dirs`, graphs that own no job, whose traces no other
    /// graph needs: none is the newest trace of a query another graph read.
    fn gc(&mut self, dirs: &FxHashSet<&str>) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            self.refresh();
            let needed = |g: &Graph| {
                g.paths
                    .iter()
                    .enumerate()
                    .any(|(j, p)| g.mine[j] & OWNER != 0 && self.readers[p.key as usize] > 1)
            };
            let Some(i) = (0..self.graphs.len()).find(|i| {
                let g = &self.graphs[*i];
                dirs.contains(g.dir.as_str()) && !needed(g)
            }) else {
                return out;
            };
            out.push(self.graphs.remove(i).dir);
            self.fresh = false;
        }
    }

    /// Checks every graph against one step of changes. A query read is a
    /// cutoff when only the force of its object changed: the reader is then
    /// a suspect, clean when the query keeps its fingerprint. When a route
    /// to the object may have moved, the reader changed. Returns, by graph,
    /// the status of each job: `None` clean, `Some((hard, suspects))`.
    fn check(&mut self, c: &Changes) -> Vec<Vec<Option<(bool, u32)>>> {
        self.refresh();
        let mut steps: Vec<u32> = self.graphs.iter().map(|g| g.step).collect();
        steps.sort_unstable();
        steps.dedup();
        // by key: the graph whose trace of it, the newest, is not clean
        let mut stale: Stale = FxHashMap::default();
        // a step's graphs read only the statuses of newer steps: in parallel
        for step in steps.into_iter().rev() {
            let idx: Vec<usize> = (0..self.graphs.len())
                .filter(|i| self.graphs[*i].step == step)
                .collect();
            let mut taken: Vec<(usize, Vec<u32>)> = idx
                .into_iter()
                .map(|i| (i, std::mem::take(&mut self.graphs[i].words)))
                .collect();
            let out: Vec<Pass> = std::thread::scope(|sc| {
                let (graphs, stale) = (&self.graphs, &stale);
                let hs: Vec<_> = taken
                    .iter_mut()
                    .map(|(i, w)| {
                        let i = *i;
                        sc.spawn(move || pass(graphs, i, w, c, stale))
                    })
                    .collect();
                hs.into_iter()
                    .map(|h| h.join().expect("a checker thread"))
                    .collect()
            });
            for ((i, w), p) in taken.into_iter().zip(out) {
                let g = &mut self.graphs[i];
                g.words = w;
                g.stat = p.stat;
                g.sets = p.sets;
                g.dirty = p.dirty;
                stale.extend(p.keys.into_iter().map(|(k, st)| (k, (i, st))));
            }
        }
        self.graphs
            .iter()
            .map(|g| {
                g.roots
                    .iter()
                    .map(|t| if *t == 0 { None } else { g.status(*t) })
                    .collect()
            })
            .collect()
    }
}

const LOCAL: u8 = 1;
const OWNER: u8 = 2;
type Stale = FxHashMap<u32, (usize, St)>;

/// What a pass leaves: the statuses, the traces that have one, and the keys
/// whose newest trace, which is of this graph, is not clean.
struct Pass {
    stat: Vec<St>,
    sets: Sets,
    dirty: Vec<u32>,
    keys: Vec<(u32, St)>,
}

/// The forward pass over graph `i`, whose trace words are `words`; graphs of
/// newer steps are done. A hard status keeps no suspects. It reads a block
/// when a trace of it can be other than clean: the block has an observation
/// that changed, reads a block or a path that is not clean, makes a path
/// below one.
fn pass(graphs: &[Graph], i: usize, words: &mut [u32], c: &Changes, stale_keys: &Stale) -> Pass {
    let g = &graphs[i];
    let leaves = leaves(c, &g.provs);
    let mut visit = vec![0u64; (g.starts.len() >> 6) + 1];
    for at in &g.dirty {
        words[*at as usize] = 0;
    }
    let mut obs = vec![false; g.index.n];
    g.index.each_changed(c, &leaves, &mut |o, m| {
        if m != 0 && !std::mem::replace(&mut obs[o as usize], true) {
            mark(&mut visit, g.by_obs.get(o));
        }
    });
    let local = |h: u32| g.mine[h as usize] & LOCAL != 0;
    let mut sets = Sets::default();
    // the forces a newer step read again: what their newest trace requires
    let mut moved = Moved {
        on: vec![CLEAN; g.paths.len()],
        now: 0,
        touched: Vec::new(),
    };
    let mut stale = vec![CLEAN; g.paths.len()];
    for (key, (j, st)) in stale_keys {
        if let Some(h) = g.path_of(*key)
            && g.paths[h as usize].head == h
            && g.read(h)
            && g.paths[h as usize].kind != kind::EAGER
            && !local(h)
        {
            let st = match *st {
                (true, _) => HARD,
                (false, x) => sets.status(graphs[*j].sets.get(x).to_vec()),
            };
            stale[h as usize] = st;
            moved.on[h as usize] = st;
            mark(&mut visit, g.by_head.get(h));
        }
    }
    let mut out = Pass {
        stat: Vec::new(),
        sets: Sets::default(),
        dirty: Vec::new(),
        keys: Vec::new(),
    };
    let stat = &mut out.stat;
    let mut sus: Vec<u32> = Vec::new();
    let mut next = 0;
    for b in 0..g.starts.len() {
        if visit[b >> 6] & (1 << (b & 63)) == 0 {
            // a path made below one that is not clean changes what reads see
            let mut n = moved.now as usize;
            let mut wake = false;
            while !wake && n < g.paths.len() && g.paths[n].born as usize >> BLOCK <= b {
                let p = g.paths[n].parent;
                wake = p != NO_PATH && moved.on[g.paths[p as usize].head as usize] != CLEAN;
                n += 1;
            }
            if !wake {
                moved.now = n as u32;
                continue;
            }
        }
        let mut at = g.starts[b] as usize;
        let mut part = usize::MAX;
        while at < words.len() && at >> BLOCK == b {
            moved.advance(g, &mut sets, at as u32);
            let n = words[at + 1] as usize;
            let mut hard = false;
            sus.clear();
            let mut k = at + 2;
            while k < at + 2 + n && !hard {
                let e = words[k];
                k += 1;
                if e & EDGE == 0 {
                    hard = obs[e as usize];
                } else if e & QUERY != 0 {
                    let (t, p) = (e & !(EDGE | QUERY), words[k]);
                    k += 1;
                    let h = g.paths[p as usize].head;
                    let key = g.paths[h as usize].key;
                    let up = moved.above(g, &mut sets, h);
                    let own = if !local(h) {
                        stale[h as usize]
                    } else if t == 0 || words[t as usize] == 0 {
                        CLEAN
                    } else {
                        stat[words[t as usize] as usize - 1]
                    };
                    // a force that changed is verified by its path; one whose
                    // queries may have, by those
                    hard = up.0;
                    sus.extend_from_slice(sets.get(up.1));
                    match own {
                        (true, _) => sus.push(key),
                        (false, x) => sus.extend_from_slice(sets.get(x)),
                    }
                } else {
                    let t = (e & !EDGE) as usize;
                    if t != 0
                        && let x @ 1.. = words[t]
                    {
                        let (h, s) = stat[x as usize - 1];
                        hard = h;
                        sus.extend_from_slice(sets.get(s));
                    }
                }
            }
            let st = if hard {
                HARD
            } else {
                sus.sort_unstable();
                sus.dedup();
                sets.status(sus.clone())
            };
            words[at] = if st == CLEAN {
                0
            } else {
                stat.push(st);
                stat.len() as u32
            };
            if st != CLEAN {
                if std::mem::replace(&mut part, at >> PART) != at >> PART {
                    mark(&mut visit, g.readers.get(part as u32));
                }
                out.dirty.push(at as u32);
                while next < g.forced.len() && g.forced[next].0 < at as u32 {
                    next += 1;
                }
                let own = g.forced[next..].iter().take_while(|f| f.0 == at as u32);
                for (_, h) in own {
                    let p = &g.paths[*h as usize];
                    if g.mine[*h as usize] & OWNER != 0 {
                        out.keys.push((p.key, st));
                    }
                    if p.head == *h && p.kind != kind::EAGER && local(*h) {
                        moved.set(g, &mut sets, *h, st);
                    }
                }
            }
            at += 2 + n;
        }
        for h in moved.touched.drain(..) {
            mark(&mut visit, g.by_head.get(h));
        }
    }
    out.sets = sets;
    out
}

/// `iets fine-check --serve`: reads commands, one a line, and answers each:
///
/// - `load DIR STEP`: starts to read the graph in DIR, written at STEP, and
///   keeps it; `ok`. When it cannot be read, the next command that is no
///   `load` fails.
/// - `drop DIR`: forgets it; `ok`.
/// - `gc FILE OUT`: of the graphs in FILE, a line each, which own no job,
///   forgets those whose traces no other graph needs, and writes them to
///   OUT; `ok`.
/// - `check FILE OUT`: checks every graph against the one step of changes
///   in FILE, and writes to OUT a line per job that is not clean,
///   `H\tDIR\tINDEX` when its own trace changed and `S\tDIR\tINDEX\tKEY...`
///   when only queries it read may have, then `Q\tKEY\tFINGERPRINT\tNAME...`
///   per suspect query, with the fingerprint the newest graph that read it
///   recorded and the names of its path from the root set; `ok`.
fn serve() -> i32 {
    use std::io::BufRead;
    let mut s = Server::default();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let f: Vec<&str> = line.split(' ').collect();
        // a graph that did not load fails the next command that needs the graphs
        let failed = match f[0] {
            "load" => None,
            _ => s.settle().err(),
        };
        let reply = match f.as_slice() {
            _ if failed.is_some() => format!("error {}\n", failed.unwrap()),
            ["load", dir, step] => {
                let (dir, step) = (dir.to_string(), step.parse().unwrap_or(0));
                let paths = Graph::paths(&dir, &mut s.keys);
                let d = dir.clone();
                s.loading.push(std::thread::spawn(move || {
                    Graph::load(dir, step, paths)
                        .map_err(|e| std::io::Error::other(format!("{d}: {e}")))
                }));
                "ok\n".to_string()
            }
            ["drop", dir] => {
                s.graphs.retain(|g| g.dir != *dir);
                s.fresh = false;
                "ok\n".to_string()
            }
            ["gc", file, dest] => match std::fs::read_to_string(file) {
                Ok(t) => {
                    let dirs: FxHashSet<&str> = t.lines().collect();
                    let gone: String = s.gc(&dirs).into_iter().map(|d| d + "\n").collect();
                    match std::fs::write(dest, gone) {
                        Ok(()) => "ok\n".to_string(),
                        Err(e) => format!("error {dest}: {e}\n"),
                    }
                }
                Err(e) => format!("error {file}: {e}\n"),
            },
            ["check", file, dest] => match std::fs::read_to_string(file) {
                Ok(t) => {
                    let c = Changes::read(&t);
                    let res = s.check(&c);
                    let mut o = String::new();
                    let mut suspects: FxHashSet<u32> = FxHashSet::default();
                    for (g, jobs) in s.graphs.iter().zip(&res) {
                        for (j, st) in jobs.iter().enumerate() {
                            match st {
                                None => {}
                                Some((true, _)) => {
                                    let _ = writeln!(o, "H\t{}\t{j}", g.dir);
                                }
                                Some((false, set)) => {
                                    let ks = g.sets.get(*set);
                                    suspects.extend(ks);
                                    let ks: Vec<String> =
                                        ks.iter().map(|k| k.to_string()).collect();
                                    let _ = writeln!(o, "S\t{}\t{j}\t{}", g.dir, ks.join(" "));
                                }
                            }
                        }
                    }
                    let mut ks: Vec<u32> = suspects.into_iter().collect();
                    ks.sort_unstable();
                    for k in ks {
                        let fp = s
                            .owner
                            .get(k as usize)
                            .filter(|j| **j != 0)
                            .map_or("-", |j| {
                                let g = &s.graphs[*j as usize - 1];
                                g.path_of(k)
                                    .map_or("-", |p| g.paths[p as usize].fp.as_str())
                            });
                        let names = s.keys.names(k).join("\t");
                        let _ = writeln!(o, "Q\t{k}\t{fp}\t{names}");
                    }
                    match std::fs::write(dest, o) {
                        Ok(()) => "ok\n".to_string(),
                        Err(e) => format!("error {dest}: {e}\n"),
                    }
                }
                Err(e) => format!("error {file}: {e}\n"),
            },
            _ => format!("error unknown command: {line}\n"),
        };
        if std::io::Write::write_all(&mut out, reply.as_bytes()).is_err()
            || std::io::Write::flush(&mut out).is_err()
        {
            break;
        }
    }
    0
}

/// `iets fine-check CHANGES [--skip TEXT] DAG...`: for each job of each
/// trace graph that a step of CHANGES invalidates, `DAG\tJOB\tSTEP`. A
/// whole read whose place holds TEXT is left out, to price a fix there.
/// `--serve` keeps graphs for many checks (`serve`), `--job N DAG` prints
/// what job N of a graph observed.
pub fn check_main(argv: &[String]) -> i32 {
    if let [flag] = argv
        && flag == "--serve"
    {
        return serve();
    }
    if let [flag, job, dir] = argv
        && flag == "--job"
    {
        return match show_job(std::path::Path::new(dir), job.parse().unwrap_or(0)) {
            Ok(()) => 0,
            Err(e) => {
                errln!("{dir}: {e}");
                1
            }
        };
    }
    let Some(changes) = argv.first() else {
        errln!("usage: iets fine-check CHANGES [--skip TEXT] DAG...");
        return 2;
    };
    let c = match std::fs::read_to_string(changes) {
        Ok(t) => Changes::read(&t),
        Err(e) => {
            errln!("{changes}: {e}");
            return 1;
        }
    };
    let mut skip = None;
    let mut dags = &argv[1..];
    if dags.first().map(String::as_str) == Some("--skip") && dags.len() > 1 {
        skip = Some(dags[1].as_str());
        dags = &dags[2..];
    }
    let mut out = String::new();
    let results: Vec<_> = std::thread::scope(|sc| {
        let hs: Vec<_> = dags
            .iter()
            .map(|d| sc.spawn(|| check_dag(&c, std::path::Path::new(d), skip)))
            .collect();
        hs.into_iter()
            .map(|h| h.join().expect("a checker thread"))
            .collect()
    });
    for (d, r) in dags.iter().zip(results) {
        match r {
            Ok(jobs) => {
                for (i, m) in jobs {
                    for (s, name) in c.steps.iter().enumerate() {
                        if m & bit(s as u8) != 0 {
                            let _ = writeln!(out, "{d}\t{i}\t{name}");
                        }
                    }
                }
            }
            Err(e) => {
                errln!("{d}: {e}");
                return 1;
            }
        }
    }
    print!("{out}");
    0
}

/// `iets fine-check --job N DAG`: the observations of job N of a graph.
fn show_job(dir: &std::path::Path, job: usize) -> std::io::Result<()> {
    let obs: Vec<String> = std::fs::read_to_string(dir.join("obs"))?
        .lines()
        .map(String::from)
        .collect();
    let words = read_words(&dir.join("nodes"))?;
    let roots = std::fs::read_to_string(dir.join("roots"))?;
    let t: u32 = roots
        .lines()
        .nth(job)
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    let mut seen = FxHashSet::default();
    let mut ids = FxHashSet::default();
    let mut stack = vec![t];
    while let Some(t) = stack.pop() {
        let t = t as usize;
        if t == 0 || !seen.insert(t) {
            continue;
        }
        let n = words[t + 1] as usize;
        scan(&words[t + 2..t + 2 + n], |e| match e {
            Entry::Obs(o) => {
                ids.insert(o);
            }
            Entry::Edge(c) => stack.push(c),
        });
    }
    let mut ids: Vec<u32> = ids.into_iter().collect();
    ids.sort_unstable();
    for i in ids {
        println!("{}", obs[i as usize]);
    }
    Ok(())
}

/// One entry of a frame or a trace; the read of a query is an edge to the
/// trace of its force.
enum Entry {
    Obs(u32),
    Edge(u32),
}

fn scan(es: &[u32], mut f: impl FnMut(Entry)) {
    let mut i = 0;
    while i < es.len() {
        let e = es[i];
        if e & EDGE == 0 {
            f(Entry::Obs(e));
            i += 1;
        } else if e & QUERY != 0 {
            f(Entry::Edge(e & !(EDGE | QUERY)));
            i += 2;
        } else {
            f(Entry::Edge(e & !EDGE));
            i += 1;
        }
    }
}
