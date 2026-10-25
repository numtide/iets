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
use crate::runtime::value::{AttrsObj, Value};
use crate::runtime::word::{TAG_IMM, TAG_INT};
use crate::syntax::symbol::{FxHashMap, FxHashSet, Interner, Sym};
use crate::trace::{Kind, Obs};

pub const FINE_MIN: usize = 8;
/// `AttrsObj::flags` bit: the set has a provenance here.
pub(crate) const FLAG: u8 = 0x40;
/// An entry of a frame that is an edge to a trace, not an observation.
const EDGE: u32 = 1 << 31;

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

/// Closes the frame of a force that returned: its trace, which the parent
/// frame now reads, or 0 for none.
pub fn pop() -> u32 {
    with(|s| {
        let (start, _) = s.frames.pop().expect("an open frame");
        let n = s.log.len() - start;
        let t = match n {
            0 => 0,
            1 if s.log[start] & EDGE != 0 => s.log[start] & !EDGE,
            _ => {
                let t = s.nodes.len() as u32;
                s.nodes.push(0);
                s.nodes.push(n as u32);
                s.nodes.extend_from_slice(&s.log[start..]);
                t
            }
        };
        s.log.truncate(start);
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
            for a in b.iter_quiet() {
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
        let mut stack = entries.to_vec();
        while let Some(e) = stack.pop() {
            if e & EDGE == 0 {
                obs.insert(e);
                continue;
            }
            let t = (e & !EDGE) as usize;
            if !nodes.insert(t) {
                continue;
            }
            let n = self.nodes[t + 1] as usize;
            stack.extend_from_slice(&self.nodes[t + 2..t + 2 + n]);
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

/// The steps each job of the trace graph in `dir` must be evaluated again
/// for: `(job, mask)`.
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
    let mut leaves: Vec<Vec<NameChange>> = Vec::new();
    for line in std::fs::read_to_string(dir.join("provs"))?.lines() {
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
    let mut obs: Vec<u128> = Vec::new();
    let text = std::fs::read_to_string(dir.join("obs"))?;
    for line in text.lines() {
        let get = |m: &FxHashMap<String, u128>, k: &str| m.get(k).copied().unwrap_or(0);
        let mut f = line.splitn(4, '\t');
        let (kind, a, b) = (
            f.next().unwrap_or(""),
            f.next().unwrap_or(""),
            f.next().unwrap_or(""),
        );
        let m = match kind {
            "R" if a == "?" => every,
            // the file and the region path are next to each other in the line
            "R" => get(&c.regions, &line[2..2 + a.len() + 1 + b.len()]) | get(&c.files, a),
            "K" => {
                let l = &leaves[a.parse::<usize>().unwrap()];
                l.iter()
                    .filter(|(_, n)| n.as_ref().is_none_or(|n| n.contains(b)))
                    .fold(0, |m, (s, _)| m | bit(*s))
            }
            "A" if skip.is_some_and(|re| line.contains(re)) => 0,
            "A" => leaves[a.parse::<usize>().unwrap()]
                .iter()
                .fold(0, |m, (s, _)| m | bit(*s)),
            "X" => {
                let off: u32 = b.parse().unwrap_or(0);
                c.first.get(a).map_or(0, |v| {
                    v.iter()
                        .filter(|(_, d)| *d <= off)
                        .fold(0, |m, (s, _)| m | bit(*s))
                })
            }
            "F" | "C" => get(&c.modified, a),
            "T" => get(&c.trees, a),
            "S" => get(&c.stats, a),
            "D" => c
                .dirs
                .get(a)
                .map_or(0, |v| v.iter().fold(0, |m, (s, _)| m | bit(*s))),
            _ => 0,
        };
        obs.push(m);
    }
    let mut words: Vec<u32> = std::fs::read(dir.join("nodes"))?
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    // a trace comes after the traces it reads, so one pass in order does;
    // the stamp word of each trace, of no use here, takes the index of its
    // mask in `masks` plus one, or 0 for none
    let mut masks: Vec<u128> = Vec::new();
    let mut at = 2;
    while at < words.len() {
        let n = words[at + 1] as usize;
        let mut m = 0;
        for e in &words[at + 2..at + 2 + n] {
            m |= if e & EDGE == 0 {
                obs[*e as usize]
            } else {
                match words[(e & !EDGE) as usize] {
                    0 => 0,
                    k => masks[k as usize - 1],
                }
            };
        }
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

/// `iets fine-check CHANGES [--skip TEXT] DAG...`: for each job of each
/// trace graph that a step of CHANGES invalidates, `DAG\tJOB\tSTEP`. A
/// whole read whose place holds TEXT is left out, to price a fix there.
pub fn check_main(argv: &[String]) -> i32 {
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
    let words: Vec<u32> = std::fs::read(dir.join("nodes"))?
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let roots = std::fs::read_to_string(dir.join("roots"))?;
    let t: u32 = roots
        .lines()
        .nth(job)
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    let mut seen = FxHashSet::default();
    let mut ids = FxHashSet::default();
    let mut stack = vec![EDGE | t];
    while let Some(e) = stack.pop() {
        if e & EDGE == 0 {
            ids.insert(e);
            continue;
        }
        let t = (e & !EDGE) as usize;
        if t == 0 || !seen.insert(t) {
            continue;
        }
        let n = words[t + 1] as usize;
        stack.extend_from_slice(&words[t + 2..t + 2 + n]);
    }
    let mut ids: Vec<u32> = ids.into_iter().collect();
    ids.sort_unstable();
    for i in ids {
        println!("{}", obs[i as usize]);
    }
    Ok(())
}
