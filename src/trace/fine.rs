//! The fine trace: what one evaluation read, at the grain of a binding. A
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

use std::fmt::Write as _;
use std::panic::Location;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

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

static ON: AtomicBool = AtomicBool::new(false);
/// An `import` is reading its source: the regions stand for the bytes.
static IMPORTING: AtomicBool = AtomicBool::new(false);
static NEXT_REGION: AtomicU32 = AtomicU32::new(0);
static NEXT_LIT: AtomicU32 = AtomicU32::new(0);
/// Where the builtin that runs now was called.
static AT: AtomicU32 = AtomicU32::new(0);
/// The Nix call stack of that builtin, which is alive while it runs.
static STACK: AtomicPtr<crate::profile::Stack> = AtomicPtr::new(std::ptr::null_mut());
static STATE: Mutex<Option<State>> = Mutex::new(None);

#[derive(Default)]
struct State {
    out: Option<PathBuf>,
    regions: FxHashMap<u32, Region>,
    lits: FxHashMap<u32, Lit>,
    seen: FxHashSet<u32>,
    provs: Vec<Prov>,
    interned: FxHashMap<Prov, u32>,
    prov_of: FxHashMap<usize, u32>,
    keys: FxHashSet<(u32, Sym)>,
    /// Whole reads of a set: the reader in iets and the builtin call in Nix.
    alls: FxHashMap<u32, (&'static Location<'static>, u32, Vec<u64>)>,
    /// Files a position of which reached a value, and the last such byte
    /// offset: an edit after it moves none of them.
    positions: FxHashMap<Vec<u8>, u32>,
    /// Constants the compiler folded, by bits: their global or builtin name.
    rev: Option<FxHashMap<u64, Sym>>,
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

/// Turns the trace on when `IETS_FINE_TRACE` is set.
pub fn init() {
    if let Some(p) = std::env::var_os("IETS_FINE_TRACE") {
        start(Some(PathBuf::from(p)));
    }
}

fn start(out: Option<PathBuf>) {
    *STATE.lock().unwrap() = Some(State {
        out,
        ..State::default()
    });
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
        STACK.store(stack as *const crate::profile::Stack as *mut _, Ordering::Relaxed);
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
    let mut g = STATE.lock().unwrap();
    f(g.as_mut().expect("the fine trace is on"))
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
    with(|s| s.seen.insert(id));
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
            s.keys.insert((p, sym));
        }
    });
}

#[cold]
pub fn all(a: &AttrsObj, at: &'static Location<'static>) {
    with(|s| {
        if let Some(p) = prov(s, a) {
            s.alls.entry(p).or_insert_with(|| {
                let st = STACK.load(Ordering::Relaxed);
                // SAFETY: `at` stored the stack of the evaluator that is
                // running the builtin, which outlives the builtin's call.
                let frames = if st.is_null() { Vec::new() } else { unsafe { (*st).frames().to_vec() } };
                (at, AT.load(Ordering::Relaxed), frames)
            });
        }
    });
}

/// A position at byte `off` of `path` reached a value.
pub fn position(path: &[u8], off: u32) {
    with(|s| {
        let o = s.positions.entry(path.to_vec()).or_insert(off);
        *o = (*o).max(off);
    });
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
            s.lits.insert(id, Lit { file, region, ordinal, keys });
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

    fn binds(&mut self, h: &mut Hasher, path: &str, ord: &mut u32, binds: &[BindIr], rec: bool, from: usize) {
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
            let sub = format!("{path}/{ord}.{}", String::from_utf8_lossy(self.names.name(b.sym)));
            self.in_bind_env(b, rec, from, |w| w.region(*id, sub, scope_tag(b.scope), body));
        }
    }

    fn rec_env(binds: &[BindIr], from: &[IrRef]) -> Vec<Option<Sym>> {
        binds.iter().map(|b| Some(b.sym)).chain(from.iter().map(|_| None)).collect()
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
        if !l.binds.first().is_some_and(|b| matches!(b.value, Ir::Region { .. })) {
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

/// Writes the trace of the run to the file `IETS_FINE_TRACE` named.
pub fn dump(ev: &Evaluator) {
    let obs = ev.trace().take();
    let mut out = String::new();
    let path = with(|s| {
        let mut ids: Vec<u32> = s.seen.iter().copied().collect();
        ids.sort();
        for id in ids {
            match s.regions.get(&id) {
                Some(r) => {
                    let _ = writeln!(out, "R\t{}\t{}\t{}", lossy(&r.file), r.path, hex(&r.hash));
                }
                None => {
                    let _ = writeln!(out, "R\t?\t{id}\t");
                }
            }
        }
        let mut used = vec![false; s.provs.len()];
        let mut stack: Vec<u32> = s.keys.iter().map(|k| k.0).chain(s.alls.keys().copied()).collect();
        while let Some(p) = stack.pop() {
            if std::mem::replace(&mut used[p as usize], true) {
                continue;
            }
            match &s.provs[p as usize] {
                Prov::Union(a, b) => stack.extend([*a, *b]),
                Prov::Remove(a, _) => stack.push(*a),
                Prov::Lit(_) | Prov::Dir(_) => {}
            }
        }
        for (i, p) in s.provs.iter().enumerate() {
            if !used[i] {
                continue;
            }
            let _ = match p {
                Prov::Lit(l) => match s.lits.get(l) {
                    Some(l) => writeln!(out, "P\t{i}\tlit\t{}\t{}\t{}", lossy(&l.file), l.region, l.ordinal),
                    None => writeln!(out, "P\t{i}\tlit?\t{l}"),
                },
                Prov::Dir(d) => writeln!(out, "P\t{i}\tdir\t{}", lossy(d)),
                Prov::Union(a, b) => writeln!(out, "P\t{i}\tu\t{a}\t{b}"),
                Prov::Remove(a, names) => {
                    let names: Vec<_> = names.iter().map(|n| ev.interner.name_str(*n)).collect();
                    writeln!(out, "P\t{i}\tx\t{a}\t{}", names.join(" "))
                }
            };
        }
        let mut keys: Vec<_> = s.keys.iter().collect();
        keys.sort();
        for (p, k) in keys {
            let _ = writeln!(out, "K\t{p}\t{}", ev.interner.name_str(*k));
        }
        for (p, (at, nix, frames)) in &s.alls {
            let nix = ev.describe_pos(crate::eval::Pos(*nix));
            let mut st = String::new();
            for f in frames.iter().rev().take(12) {
                let _ = write!(st, " < {}", ev.describe_pos(crate::eval::Pos((f >> 32) as u32)));
            }
            let _ = writeln!(out, "A\t{p}\t{}:{}\t{nix}\t{st}", at.file(), at.line());
        }
        for (f, off) in &s.positions {
            let _ = writeln!(out, "X\t{}\t{off}", lossy(f));
        }
        s.out.clone()
    });
    for o in obs {
        let _ = match o {
            Obs::File { path, hash, .. } => writeln!(out, "F\t{}\t{}", lossy(&path), hex(&hash)),
            Obs::Dir { path, hash } => writeln!(out, "D\t{}\t{}", lossy(&path), hex(&hash)),
            Obs::Stat { path, kind, .. } => writeln!(out, "S\t{}\t{}", lossy(&path), kind_name(kind)),
            Obs::Tree { root, nar_hash, .. } => writeln!(out, "T\t{}\t{}", lossy(&root), hex(&nar_hash)),
            Obs::Canon { path, .. } => writeln!(out, "C\t{}", lossy(&path)),
            Obs::Env { name, .. } => writeln!(out, "E\t{}", lossy(&name)),
            Obs::Cmd { dir, .. } => writeln!(out, "Q\t{}", lossy(&dir)),
            Obs::Input { entry, .. } => writeln!(out, "I\t{}", lossy(&entry)),
        };
    }
    if let Some(p) = path
        && let Err(e) = std::fs::write(&p, out)
    {
        errln!("fine trace: writing {}: {e}", p.display());
    }
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
