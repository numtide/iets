//! Compiled-IR cache on disk. A file's IR is written after its first
//! compile and loaded instead of parsing and compiling on later runs
//! (docs/explanation/caching.md, "IR cache").
//!
//! The IR is process-specific in three ways, and the entry undoes each:
//! `Sym` ids (a per-entry name table is re-interned on load), `Pos` values
//! (stored relative to the file, rebased on load), and constant `Word`s
//! (literals are stored by content, globals and `builtins.x` by name and
//! looked up in the live tables, so `__nixPath` follows the run). Binding
//! frames (`let`, `rec`, formals) are slot-ordered by `Sym` id, so the
//! loader re-sorts them and rewrites every `Local` index through a
//! permutation stack that mirrors the compiler's scope pushes.

use std::path::{Path, PathBuf};

use super::ir::{
    AttrElemIr, AttrsIr, BindIr, BindScope, DynBindIr, FormalIr, Ir, IrRef, LambdaIr, LetIr,
    ParamIr, SelectIr,
};
use crate::cachefs::{self, Shape};
use crate::eval::{Evaluator, Pos};
use crate::lint::allow::{Allows, Line};
use crate::lint::{Diag, LintId};
use crate::runtime::word::{TAG_IMM, TAG_INT};
use crate::runtime::{self, Kind, Word};
use crate::session::Session;
use crate::syntax::symbol::{FxHashMap, Sym};
use crate::trace::key::BUILD_ID;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use std::{fs, slice};

const MAGIC: &[u8; 8] = b"IETS_IR2";

pub struct IrCache {
    root: PathBuf,
    /// Sources below this size compile faster than their entry loads.
    pub min_size: usize,
    /// `IETS_IR_STATS=1`: one line per file on stderr.
    pub stats: bool,
    /// The store is swept at most once per process, on the first write.
    swept: AtomicBool,
    /// Entries read, over every fiber: the cache is one handle shared by all
    /// of them, so this counts the whole run.
    hits: AtomicU64,
}

impl IrCache {
    /// `--cache-dir DIR`, else `IETS_CACHE_DIR`, else `$XDG_CACHE_HOME/iets`,
    /// else `~/.cache/iets`; the entries live in its `ir/` subdirectory, so
    /// the directory the user names is never a removal target. `None` when
    /// that subdirectory is not iets's to write in.
    pub fn locate(override_dir: Option<&str>, timings: bool) -> Option<IrCache> {
        let session = Session::current();
        let set = session.settings();
        let root = cachefs::root(override_dir)?.join("ir");
        if !cachefs::prepare(&root, Shape::ShardedFiles, timings) {
            return None;
        }
        Some(IrCache {
            root,
            min_size: set.ir_min,
            stats: set.ir_stats,
            swept: AtomicBool::new(false),
            hits: AtomicU64::new(0),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Everything the compiler folds out of the evaluator's configuration is
    /// here, or the entry of one configuration answers another one. The folds
    /// store a constant by content, so `system`, `store_dir` and `pure` (which
    /// removes the `impureOnly` names) are in the key; `nixVersion` and
    /// `langVersion` follow the build id. Every other constant is looked up by name
    /// on load, and a `~/` literal is never cached. `lint_key` is
    /// `lint::Config::compile_key`, whose warnings the entry carries.
    pub fn key(
        &self,
        path: &[u8],
        src: &[u8],
        system: &str,
        store_dir: &[u8],
        lint_key: &str,
        pure: bool,
    ) -> String {
        let mut h = blake3::Hasher::new();
        for part in [
            BUILD_ID.as_bytes(),
            path,
            system.as_bytes(),
            store_dir,
            lint_key.as_bytes(),
            &[u8::from(pure)][..],
        ] {
            h.update(part);
            h.update(b"\0");
        }
        h.update(src);
        h.finalize().to_hex().to_string()
    }

    fn entry(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2]).join(&key[2..])
    }

    /// A hit marks the entry as used from the same `fstat` that sizes the
    /// read, and only when the mark is due (`cachefs::touch_open`). The
    /// integrity check is verified before the caller decodes anything, and an
    /// entry that fails it is poisoned like one that does not decode.
    pub fn read(&self, key: &str) -> Option<Vec<u8>> {
        let (f, md) = cachefs::open_entry(&self.entry(key))?;
        let mut bytes = Vec::with_capacity(md.len() as usize + 1);
        let mut r = &f;
        if r.read_to_end(&mut bytes).is_err() {
            self.discard(key);
            return None;
        }
        let Some(n) = bytes.len().checked_sub(cachefs::SUM) else {
            self.discard(key);
            return None;
        };
        if bytes[n..] != cachefs::checksum(key, &[&bytes[..n]]) {
            self.discard(key);
            return None;
        }
        bytes.truncate(n);
        cachefs::touch_open(&f, md.modified().ok());
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(bytes)
    }

    /// How many entries this run read, the `ir-cache load` lines of
    /// `IETS_IR_STATS=1` less the ones that failed to decode.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Skips the write when a sibling process holds the store lock: the file
    /// is compiled again next run, which is cheaper than waiting here.
    pub fn write(&self, key: &str, bytes: &[u8]) {
        let path = self.entry(key);
        let dir = path.parent().expect("entry has a parent");
        let _ = cachefs::create_dir(dir);
        let name = &key[2..];
        if let Some(_lock) = cachefs::write_lock(&self.root) {
            let sum = cachefs::checksum(key, &[bytes]);
            let _ = cachefs::write_atomic(dir, name, cachefs::tag(), &[bytes, &sum]);
        } else if self.stats {
            errln!("ir-cache skip {name} (a sibling holds the store lock)");
        }
        if !self.swept.swap(true, Ordering::Relaxed) {
            cachefs::maybe_sweep(&self.root, Shape::ShardedFiles, self.stats);
        }
    }

    /// Poison and wipe, never repair: an entry that does not decode goes.
    pub fn discard(&self, key: &str) {
        let _ = fs::remove_file(self.entry(key));
    }
}

fn put_u32(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_u64(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

struct Ser {
    out: Vec<u8>,
    syms: FxHashMap<Sym, u32>,
    names: Vec<Sym>,
    base: u32,
    /// Heap constant word -> (is a global, its name); built from the live tables.
    rev: FxHashMap<u64, (bool, Sym)>,
    ok: bool,
}

/// The entry for `ir`, the lint warnings its compile produced and the
/// file's `# iets: allow` table, or `None` when a constant cannot be named.
pub fn serialize(
    ev: &Evaluator,
    ir: IrRef,
    base: u32,
    diags: &[Diag],
    allows: &Allows,
) -> Option<Vec<u8>> {
    let mut rev = FxHashMap::default();
    if let Some(b) = ev.globals.builtins {
        for a in b.iter() {
            rev.insert(a.cell.get().bits(), (false, a.sym));
        }
    }
    for (sym, w) in &ev.globals.table {
        rev.insert(w.bits(), (true, *sym));
    }
    let mut s = Ser {
        out: Vec::with_capacity(1 << 16),
        syms: FxHashMap::default(),
        names: Vec::new(),
        base,
        rev,
        ok: true,
    };
    s.node(ir);
    if !s.ok {
        return None;
    }
    put_u32(&mut s.out, diags.len() as u32);
    for d in diags {
        s.out.push(d.id as u8);
        s.pos(d.pos);
        put_bytes(&mut s.out, d.msg.as_bytes());
    }
    put_u32(&mut s.out, allows.lines().len() as u32);
    for l in allows.lines() {
        put_u32(&mut s.out, l.start);
        put_u32(&mut s.out, l.end);
        put_u64(&mut s.out, l.mask);
    }
    let mut file = Vec::with_capacity(s.out.len() + 16 * s.names.len() + 16);
    file.extend_from_slice(MAGIC);
    put_u32(&mut file, s.names.len() as u32);
    for sym in &s.names {
        put_bytes(&mut file, ev.interner.name(*sym));
    }
    file.extend_from_slice(&s.out);
    Some(file)
}

impl Ser {
    fn sym(&mut self, s: Sym) {
        let n = self.names.len() as u32;
        let id = *self.syms.entry(s).or_insert_with(|| {
            self.names.push(s);
            n
        });
        put_u32(&mut self.out, id);
    }

    fn pos(&mut self, p: Pos) {
        let v = if p.is_none() {
            u32::MAX
        } else {
            debug_assert!(p.0 >= self.base);
            p.0 - self.base
        };
        put_u32(&mut self.out, v);
    }

    fn opt_sym(&mut self, s: Option<Sym>) {
        match s {
            None => self.out.push(0),
            Some(s) => {
                self.out.push(1);
                self.sym(s);
            }
        }
    }

    fn opt_node(&mut self, ir: Option<IrRef>) {
        match ir {
            None => self.out.push(0),
            Some(ir) => {
                self.out.push(1);
                self.node(ir);
            }
        }
    }

    fn nodes(&mut self, irs: &[IrRef]) {
        put_u32(&mut self.out, irs.len() as u32);
        for ir in irs {
            self.node(ir);
        }
    }

    fn elems(&mut self, path: &[AttrElemIr]) {
        put_u32(&mut self.out, path.len() as u32);
        for e in path {
            match e {
                AttrElemIr::Static(s) => {
                    self.out.push(0);
                    self.sym(*s);
                }
                AttrElemIr::Dynamic(ir) => {
                    self.out.push(1);
                    self.node(ir);
                }
            }
        }
    }

    fn word(&mut self, w: Word) {
        if w.is_ptr() {
            match w.heap_kind() {
                Some(Kind::String) => {
                    let s = w.as_string().expect("string kind");
                    if !s.ctx.is_empty() {
                        self.ok = false;
                        return;
                    }
                    self.out.push(1);
                    put_bytes(&mut self.out, s.bytes);
                    return;
                }
                Some(Kind::Path) => {
                    let p = match w.view() {
                        runtime::Value::Path(p) => p,
                        _ => unreachable!(),
                    };
                    self.out.push(2);
                    put_bytes(&mut self.out, p.path);
                    return;
                }
                Some(Kind::Float) => {
                    let f = match w.view() {
                        runtime::Value::Float(f) => f,
                        _ => unreachable!(),
                    };
                    self.out.push(3);
                    self.out.extend_from_slice(&f.to_bits().to_le_bytes());
                    return;
                }
                Some(Kind::Int) => {
                    let i = w.as_int().expect("int kind");
                    self.out.push(4);
                    self.out.extend_from_slice(&i.to_le_bytes());
                    return;
                }
                _ => {}
            }
        } else if w.tag() == TAG_INT || w.tag() == TAG_IMM {
            self.out.push(0);
            put_u64(&mut self.out, w.bits());
            return;
        }
        match self.rev.get(&w.bits()) {
            Some(&(global, sym)) => {
                self.out.push(if global { 5 } else { 6 });
                self.sym(sym);
            }
            None => self.ok = false,
        }
    }

    /// A binding frame: the headers, the `from` count, then the values and
    /// the `from` expressions. The loader needs both counts before it reads
    /// a value, so it can size the slot permutation.
    fn binds(&mut self, binds: &[BindIr], from: &[IrRef]) {
        put_u32(&mut self.out, binds.len() as u32);
        for b in binds {
            self.sym(b.sym);
            self.pos(b.pos);
            self.out.push(match b.scope {
                BindScope::Own => 0,
                BindScope::Outer => 1,
                BindScope::From => 2,
            });
        }
        put_u32(&mut self.out, from.len() as u32);
        for b in binds {
            self.node(b.value);
        }
        for f in from {
            self.node(f);
        }
    }

    fn node(&mut self, ir: IrRef) {
        if !self.ok {
            return;
        }
        match ir {
            Ir::Const(w) => {
                self.out.push(0);
                self.word(*w);
            }
            Ir::Local { depth, idx } => {
                self.out.push(1);
                put_u32(&mut self.out, *depth);
                put_u32(&mut self.out, *idx);
            }
            Ir::WithVar { sym, withs, pos } => {
                self.out.push(2);
                self.sym(*sym);
                put_u32(&mut self.out, withs.len() as u32);
                for w in withs.iter() {
                    put_u32(&mut self.out, *w);
                }
                self.pos(*pos);
            }
            Ir::CurPos(pos) => {
                self.out.push(3);
                self.pos(*pos);
            }
            Ir::Concat {
                force_string,
                parts,
                pos,
            } => {
                self.out.push(4);
                self.out.push(*force_string as u8);
                self.nodes(parts);
                self.pos(*pos);
            }
            Ir::SearchPath { name, pos } => {
                self.out.push(5);
                self.node(name);
                self.pos(*pos);
            }
            Ir::Select(s) | Ir::SelectCount(s) => {
                self.out
                    .push(if matches!(ir, Ir::Select(_)) { 6 } else { 33 });
                self.node(s.subject);
                self.elems(s.path);
                self.opt_node(s.default);
                self.pos(s.pos);
            }
            Ir::Select1 {
                subject,
                sym,
                default,
                pos,
            } => {
                self.out.push(7);
                self.node(subject);
                self.sym(*sym);
                self.opt_node(*default);
                self.pos(*pos);
            }
            Ir::HasAttr { subject, path } => {
                self.out.push(8);
                self.node(subject);
                self.elems(path);
            }
            Ir::Attrs(a) => {
                self.out.push(9);
                self.out.push(a.rec as u8);
                self.pos(a.pos);
                self.binds(a.binds, a.from);
                put_u32(&mut self.out, a.dynamic.len() as u32);
                for d in a.dynamic.iter() {
                    self.node(d.name);
                    self.node(d.value);
                    self.pos(d.pos);
                }
            }
            Ir::List(items) => {
                self.out.push(10);
                self.nodes(items);
            }
            Ir::Lambda(l) => {
                self.out.push(11);
                match &l.param {
                    ParamIr::Ident => self.out.push(0),
                    ParamIr::Formals {
                        has_bind,
                        formals,
                        ellipsis,
                    } => {
                        self.out.push(1);
                        self.out.push(*has_bind as u8);
                        self.out.push(*ellipsis as u8);
                        put_u32(&mut self.out, formals.len() as u32);
                        for f in formals.iter() {
                            self.sym(f.sym);
                            self.pos(f.pos);
                        }
                        for f in formals.iter() {
                            self.opt_node(f.default);
                        }
                    }
                }
                self.node(l.body);
                self.pos(l.pos);
                self.opt_sym(l.name);
                self.opt_sym(l.arg);
                self.out.push(l.closed as u8);
            }
            Ir::Apply { func, args, pos } => {
                self.out.push(12);
                self.node(func);
                self.nodes(args);
                self.pos(*pos);
            }
            Ir::Let(l) => {
                self.out.push(13);
                self.binds(l.binds, l.from);
                self.node(l.body);
            }
            Ir::With { scope, body, pos } => {
                self.out.push(14);
                self.node(scope);
                self.node(body);
                self.pos(*pos);
            }
            Ir::If {
                cond,
                then,
                else_,
                pos,
            } => {
                self.out.push(15);
                self.node(cond);
                self.node(then);
                self.node(else_);
                self.pos(*pos);
            }
            Ir::Assert { cond, body, pos } => {
                self.out.push(16);
                self.node(cond);
                self.node(body);
                self.pos(*pos);
            }
            Ir::Not(e, pos) => self.unary(17, e, *pos),
            Ir::Neg(e, pos) => self.unary(18, e, *pos),
            Ir::Eq(l, r, pos) => self.binary(19, l, r, *pos),
            Ir::Ne(l, r, pos) => self.binary(20, l, r, *pos),
            Ir::Lt(l, r, pos) => self.binary(21, l, r, *pos),
            Ir::Gt(l, r, pos) => self.binary(22, l, r, *pos),
            Ir::Le(l, r, pos) => self.binary(23, l, r, *pos),
            Ir::Ge(l, r, pos) => self.binary(24, l, r, *pos),
            Ir::And(l, r, pos) => self.binary(25, l, r, *pos),
            Ir::Or(l, r, pos) => self.binary(26, l, r, *pos),
            Ir::Impl(l, r, pos) => self.binary(27, l, r, *pos),
            Ir::Update(l, r, pos) => self.binary(28, l, r, *pos),
            Ir::ConcatLists(l, r, pos) => self.binary(29, l, r, *pos),
            Ir::Sub(l, r, pos) => self.binary(30, l, r, *pos),
            Ir::Mul(l, r, pos) => self.binary(31, l, r, *pos),
            Ir::Div(l, r, pos) => self.binary(32, l, r, *pos),
        }
    }

    fn unary(&mut self, tag: u8, e: IrRef, pos: Pos) {
        self.out.push(tag);
        self.node(e);
        self.pos(pos);
    }

    fn binary(&mut self, tag: u8, l: IrRef, r: IrRef, pos: Pos) {
        self.out.push(tag);
        self.node(l);
        self.node(r);
        self.pos(pos);
    }
}

/// One scope the compiler pushed. The decoder mirrors the compiler's scope
/// stack so it can check every stored index against the env that will hold
/// it: a `Local` past the end of its frame would be an out-of-bounds slot
/// read at evaluation time.
struct Frame {
    /// Slots the env at this depth has.
    slots: usize,
    /// A `with` scope. Only these carry a `WithVar` depth, and only these
    /// hold the set in slot 0.
    with: bool,
    /// The slot permutation of a re-sorted frame, `None` for an unchanged one.
    perm: Option<Vec<u32>>,
}

/// Open `node` calls. Real IR nests far below this; a crafted entry that
/// nests deeper would exhaust the stack.
const MAX_NEST: u32 = 10_000;

struct De<'a, 'e> {
    ev: &'e mut Evaluator,
    data: &'a [u8],
    at: usize,
    syms: Vec<Sym>,
    base: u32,
    /// Byte length of the file this entry belongs to: every stored position
    /// is an offset inside it.
    span: u32,
    /// Items the counted runs may still ask for. Every item of a valid
    /// entry costs at least one byte, so the entry's own length is the
    /// bound, and a crafted count cannot make the arena grow without one.
    budget: usize,
    /// One entry per scope the compiler pushed, innermost last.
    frames: Vec<Frame>,
    nest: u32,
    ok: bool,
}

/// The IR of an entry, allocated in the evaluator's arena with positions
/// rebased at `base`, plus its lint warnings and the file's allow table;
/// `None` on a malformed entry, which the caller then discards. `span` is
/// the length of the source the entry was compiled from.
pub fn deserialize(
    ev: &mut Evaluator,
    data: &[u8],
    base: u32,
    span: u32,
    stats: bool,
) -> Option<(IrRef, Vec<Diag>, Allows)> {
    let t0 = Instant::now();
    if data.len() < MAGIC.len() || &data[..MAGIC.len()] != MAGIC {
        return None;
    }
    let mut d = De {
        ev,
        data,
        at: MAGIC.len(),
        syms: Vec::new(),
        base,
        span,
        budget: data.len(),
        frames: Vec::new(),
        nest: 0,
        ok: true,
    };
    let n = d.count();
    d.syms.reserve(n);
    for _ in 0..n {
        let len = d.u32() as usize;
        let end = d.at.saturating_add(len);
        if end > data.len() {
            return None;
        }
        let s = d.ev.interner.intern(&data[d.at..end]);
        d.at = end;
        d.syms.push(s);
    }
    let t1 = t0.elapsed();
    let ir = d.node();
    let ndiags = d.count();
    let mut diags = Vec::with_capacity(ndiags.min(1024));
    for _ in 0..ndiags {
        if !d.ok {
            break;
        }
        let id = LintId::from_u8(d.u8());
        let pos = d.pos();
        let msg = String::from_utf8_lossy(d.bytes()).into_owned();
        match id {
            Some(id) => diags.push(Diag { id, pos, msg }),
            None => d.ok = false,
        }
    }
    let nallows = d.count();
    let mut lines = Vec::with_capacity(nallows.min(1024));
    for _ in 0..nallows {
        if !d.ok {
            break;
        }
        let start = d.u32();
        let end = d.u32();
        let mask = d.u64();
        lines.push(Line { start, end, mask });
    }
    let allows = match Allows::from_sorted(lines) {
        Some(a) => a,
        None => {
            d.ok = false;
            Allows::default()
        }
    };
    if stats {
        errln!(
            "ir-cache   syms={} {}us nodes {}us",
            n,
            t1.as_micros(),
            (t0.elapsed() - t1).as_micros()
        );
    }
    if d.ok && d.at == data.len() {
        Some((ir, diags, allows))
    } else {
        None
    }
}

impl<'a> De<'a, '_> {
    fn u8(&mut self) -> u8 {
        match self.data.get(self.at) {
            Some(b) => {
                self.at += 1;
                *b
            }
            None => {
                self.ok = false;
                0
            }
        }
    }

    fn u64(&mut self) -> u64 {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8();
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 || !self.ok || shift >= 63 {
                return v;
            }
            shift += 7;
        }
    }

    fn u32(&mut self) -> u32 {
        self.u64() as u32
    }

    fn bytes(&mut self) -> &[u8] {
        let n = self.u32() as usize;
        let end = self.at.saturating_add(n);
        if end > self.data.len() {
            self.ok = false;
            return &[];
        }
        let b = &self.data[self.at..end];
        self.at = end;
        b
    }

    fn sym(&mut self) -> Sym {
        let i = self.u32() as usize;
        match self.syms.get(i) {
            Some(s) => *s,
            None => {
                self.ok = false;
                Sym(0)
            }
        }
    }

    fn pos(&mut self) -> Pos {
        let v = self.u32();
        if v == u32::MAX {
            Pos::NONE
        } else if v > self.span {
            self.ok = false;
            Pos::NONE
        } else {
            Pos(self.base + v)
        }
    }

    fn opt_sym(&mut self) -> Option<Sym> {
        if self.u8() == 0 {
            None
        } else {
            Some(self.sym())
        }
    }

    fn opt_node(&mut self) -> Option<IrRef> {
        if self.u8() == 0 {
            None
        } else {
            Some(self.node())
        }
    }

    /// A count that a run of items follows, against the entry's own item
    /// budget, so a corrupt count cannot reserve gigabytes and a chain of
    /// them cannot do it one level at a time.
    fn count(&mut self) -> usize {
        let n = (self.u32() as usize).min(self.data.len() - self.at.min(self.data.len()));
        if n > self.budget {
            self.ok = false;
            return 0;
        }
        self.budget -= n;
        n
    }

    /// The arena slice is reserved first and filled as the items decode,
    /// so a list costs no scratch vector.
    fn fill<T>(&mut self, n: usize, mut item: impl FnMut(&mut Self) -> T) -> &'static [T] {
        if n == 0 {
            return &[];
        }
        let p = self.ev.arena.alloc_raw(n * size_of::<T>(), align_of::<T>()) as *mut T;
        // SAFETY: fresh allocation of n elements; every slot is written
        // below before the slice is made (a failed decode still writes a
        // value; the entry is dropped by the caller).
        unsafe {
            for i in 0..n {
                p.add(i).write(item(self));
            }
            slice::from_raw_parts(p, n)
        }
    }

    fn nodes(&mut self) -> &'static [IrRef] {
        let n = self.count();
        self.fill(n, |d| d.node())
    }

    fn elems(&mut self) -> &'static [AttrElemIr] {
        let n = self.count();
        self.fill(n, |d| {
            if d.u8() == 0 {
                AttrElemIr::Static(d.sym())
            } else {
                AttrElemIr::Dynamic(d.node())
            }
        })
    }

    /// The bytes of a stored string or path, borrowed from the entry rather
    /// than from `self`, so the arena can be written while they are held.
    fn stored_bytes(&mut self) -> Option<&'a [u8]> {
        let data = self.data;
        let n = self.u32() as usize;
        let end = self.at.saturating_add(n);
        if end > data.len() {
            self.ok = false;
            return None;
        }
        let b = &data[self.at..end];
        self.at = end;
        Some(b)
    }

    fn word(&mut self) -> Word {
        match self.u8() {
            0 => match Word::imm_from_bits(self.u64()) {
                Some(w) => w,
                None => {
                    self.ok = false;
                    Word::NULL
                }
            },
            1 => match self.stored_bytes() {
                Some(b) => self.ev.arena.mk_string(b),
                None => Word::NULL,
            },
            2 => match self.stored_bytes() {
                Some(b) => self.ev.arena.mk_path(b),
                None => Word::NULL,
            },
            3 => {
                let mut raw = [0u8; 8];
                for r in raw.iter_mut() {
                    *r = self.u8();
                }
                self.ev
                    .arena
                    .mk_float(f64::from_bits(u64::from_le_bytes(raw)))
            }
            4 => {
                let mut raw = [0u8; 8];
                for r in raw.iter_mut() {
                    *r = self.u8();
                }
                self.ev.arena.mk_int(i64::from_le_bytes(raw))
            }
            5 => {
                let s = self.sym();
                match self.ev.globals.table.get(&s) {
                    Some(w) => *w,
                    None => {
                        self.ok = false;
                        Word::NULL
                    }
                }
            }
            6 => {
                let s = self.sym();
                match self.ev.globals.builtins.and_then(|b| b.get(s)) {
                    Some(a) => a.cell.get(),
                    None => {
                        self.ok = false;
                        Word::NULL
                    }
                }
            }
            _ => {
                self.ok = false;
                Word::NULL
            }
        }
    }

    /// A frame pushed with no permutation and no slots of its own to name.
    fn plain(slots: usize, with: bool) -> Frame {
        Frame {
            slots,
            with,
            perm: None,
        }
    }

    /// The permutation that moves entries from their stored order to
    /// `Sym` order: `perm[old] = new`. `extra` identity slots follow.
    fn perm(syms: &[Sym], extra: usize) -> Option<Vec<u32>> {
        if syms.windows(2).all(|w| w[0] < w[1]) {
            return None;
        }
        let mut order: Vec<u32> = (0..syms.len() as u32).collect();
        order.sort_by_key(|&i| syms[i as usize]);
        let mut perm = vec![0u32; syms.len() + extra];
        for (new, old) in order.iter().enumerate() {
            perm[*old as usize] = new as u32;
        }
        for (i, p) in perm.iter_mut().enumerate().skip(syms.len()) {
            *p = i as u32;
        }
        Some(perm)
    }

    fn frame_of(&self, depth: u32) -> Option<&Frame> {
        let n = self.frames.len();
        let i = n.checked_sub(depth as usize + 1)?;
        self.frames.get(i)
    }

    /// A binding frame: `(binds, from)` with the binds in `Sym` order.
    /// `sorted` frames (`let`, `rec`) are pushed while their values and
    /// `from` expressions load; a plain set's `inherit (e)` selects load
    /// under the anonymous from-env instead.
    fn binds(&mut self, sorted: bool) -> (&'static [BindIr], &'static [IrRef]) {
        let n = self.count();
        let mut syms = Vec::with_capacity(n);
        let mut poss = Vec::with_capacity(n);
        let mut scopes = Vec::with_capacity(n);
        for _ in 0..n {
            syms.push(self.sym());
            poss.push(self.pos());
            scopes.push(match self.u8() {
                0 => BindScope::Own,
                1 => BindScope::Outer,
                2 => BindScope::From,
                _ => {
                    self.ok = false;
                    BindScope::Own
                }
            });
        }
        let nfrom = self.count();
        // a `From` bind is evaluated in the env of the `from` thunks, which
        // `eval::inherit_env` does not make for an empty `from` list
        if nfrom == 0 && scopes.contains(&BindScope::From) {
            self.ok = false;
        }
        let perm = Self::perm(&syms, if sorted { nfrom } else { 0 });
        if sorted {
            self.frames.push(Frame {
                slots: n + nfrom,
                with: false,
                perm: perm.clone(),
            });
        }
        let out = self.fill(n, |_| BindIr {
            sym: Sym(0),
            value: &Ir::Const(Word::NULL),
            pos: Pos::NONE,
            scope: BindScope::Own,
        });
        // SAFETY: `fill` handed out a fresh arena slice that nothing else
        // holds yet; the entries are rewritten in place below.
        let out_mut = unsafe { slice::from_raw_parts_mut(out.as_ptr() as *mut BindIr, n) };
        for i in 0..n {
            if !self.ok {
                break;
            }
            let value = match scopes[i] {
                BindScope::Outer if sorted => {
                    let f = self.frames.pop();
                    let v = self.node();
                    self.frames.extend(f);
                    v
                }
                BindScope::From if !sorted => {
                    self.frames.push(Self::plain(nfrom, false));
                    let v = self.node();
                    self.frames.pop();
                    v
                }
                _ => self.node(),
            };
            let new = perm.as_ref().map_or(i, |p| p[i] as usize);
            out_mut[new] = BindIr {
                sym: syms[i],
                value,
                pos: poss[i],
                scope: scopes[i],
            };
        }
        let from = self.fill(nfrom, |d| d.node());
        (out, from)
    }

    fn node(&mut self) -> IrRef {
        if !self.ok || self.nest >= MAX_NEST {
            self.ok &= self.nest < MAX_NEST;
            return &Ir::Const(Word::NULL);
        }
        self.nest += 1;
        let ir = match self.u8() {
            0 => Ir::Const(self.word()),
            1 => {
                let depth = self.u32();
                let idx = self.u32();
                // the slot this names is read without a bounds check at
                // evaluation time, so the frame it lands in decides it here
                let slot = match self.frame_of(depth) {
                    Some(f) => match &f.perm {
                        Some(p) => p.get(idx as usize).copied(),
                        None => Some(idx),
                    }
                    .filter(|i| (*i as usize) < f.slots),
                    None => None,
                };
                match slot {
                    Some(idx) => Ir::Local { depth, idx },
                    None => {
                        self.ok = false;
                        Ir::Const(Word::NULL)
                    }
                }
            }
            2 => {
                let sym = self.sym();
                let n = self.count();
                let withs = self.fill(n, |d| d.u32());
                // each names a `with` env, whose slot 0 the lookup reads.
                // `eval_with_var` walks the list as a rising sequence of
                // depths, so a list that does not rise would walk past the
                // outermost env it checked here.
                if !withs.windows(2).all(|w| w[0] < w[1])
                    || !withs
                        .iter()
                        .all(|w| self.frame_of(*w).is_some_and(|f| f.with))
                {
                    self.ok = false;
                }
                let pos = self.pos();
                Ir::WithVar { sym, withs, pos }
            }
            3 => Ir::CurPos(self.pos()),
            4 => {
                let force_string = self.u8() != 0;
                let parts = self.nodes();
                let pos = self.pos();
                Ir::Concat {
                    force_string,
                    parts,
                    pos,
                }
            }
            5 => {
                let name = self.node();
                let pos = self.pos();
                Ir::SearchPath { name, pos }
            }
            t @ (6 | 33) => {
                let subject = self.node();
                let path = self.elems();
                let default = self.opt_node();
                let pos = self.pos();
                let select = self.ev.arena.alloc(SelectIr {
                    subject,
                    path,
                    default,
                    pos,
                });
                match t {
                    6 => Ir::Select(select),
                    _ => Ir::SelectCount(select),
                }
            }
            7 => {
                let subject = self.node();
                let sym = self.sym();
                let default = self.opt_node();
                let pos = self.pos();
                Ir::Select1 {
                    subject,
                    sym,
                    default,
                    pos,
                }
            }
            8 => {
                let subject = self.node();
                let path = self.elems();
                Ir::HasAttr { subject, path }
            }
            9 => {
                let rec = self.u8() != 0;
                let pos = self.pos();
                let (binds, from) = self.binds(rec);
                let ndyn = self.count();
                let dynamic = self.fill(ndyn, |d| {
                    let name = d.node();
                    let value = d.node();
                    let pos = d.pos();
                    DynBindIr { name, value, pos }
                });
                if rec {
                    self.frames.pop();
                }
                let overrides = if rec {
                    let ov = self.ev.interner.intern(b"__overrides");
                    binds.iter().position(|b| b.sym == ov).map(|i| i as u32)
                } else {
                    None
                };
                Ir::Attrs(self.ev.arena.alloc(AttrsIr {
                    rec,
                    binds,
                    from,
                    dynamic,
                    pos,
                    overrides,
                }))
            }
            10 => Ir::List(self.nodes()),
            11 => {
                let param = if self.u8() == 0 {
                    self.frames.push(Self::plain(1, false));
                    ParamIr::Ident
                } else {
                    let has_bind = self.u8() != 0;
                    let ellipsis = self.u8() != 0;
                    let n = self.count();
                    let mut syms = Vec::with_capacity(n);
                    let mut poss = Vec::with_capacity(n);
                    for _ in 0..n {
                        syms.push(self.sym());
                        poss.push(self.pos());
                    }
                    let fperm = Self::perm(&syms, 0);
                    let off = u32::from(has_bind);
                    self.frames.push(Frame {
                        slots: n + off as usize,
                        with: false,
                        perm: fperm.as_ref().map(|fp| {
                            let mut perm = Vec::with_capacity(n + off as usize);
                            if has_bind {
                                perm.push(0);
                            }
                            perm.extend(fp.iter().map(|p| p + off));
                            perm
                        }),
                    });
                    let formals = self.fill(n, |_| FormalIr {
                        sym: Sym(0),
                        default: None,
                        pos: Pos::NONE,
                    });
                    // SAFETY: as in `binds`: a fresh slice, rewritten in place.
                    let fm =
                        unsafe { slice::from_raw_parts_mut(formals.as_ptr() as *mut FormalIr, n) };
                    for i in 0..n {
                        let default = self.opt_node();
                        let new = fperm.as_ref().map_or(i, |p| p[i] as usize);
                        fm[new] = FormalIr {
                            sym: syms[i],
                            default,
                            pos: poss[i],
                        };
                    }
                    ParamIr::Formals {
                        has_bind,
                        formals,
                        ellipsis,
                    }
                };
                let body = self.node();
                self.frames.pop();
                let pos = self.pos();
                let name = self.opt_sym();
                let arg = self.opt_sym();
                let closed = self.u8() != 0;
                Ir::Lambda(self.ev.arena.alloc(LambdaIr {
                    param,
                    body,
                    pos,
                    name,
                    arg,
                    closed,
                }))
            }
            12 => {
                let func = self.node();
                let args = self.nodes();
                let pos = self.pos();
                Ir::Apply { func, args, pos }
            }
            13 => {
                let (binds, from) = self.binds(true);
                let body = self.node();
                self.frames.pop();
                Ir::Let(self.ev.arena.alloc(LetIr { binds, from, body }))
            }
            14 => {
                let scope = self.node();
                self.frames.push(Self::plain(1, true));
                let body = self.node();
                self.frames.pop();
                let pos = self.pos();
                Ir::With { scope, body, pos }
            }
            15 => {
                let cond = self.node();
                let then = self.node();
                let else_ = self.node();
                let pos = self.pos();
                Ir::If {
                    cond,
                    then,
                    else_,
                    pos,
                }
            }
            16 => {
                let cond = self.node();
                let body = self.node();
                let pos = self.pos();
                Ir::Assert { cond, body, pos }
            }
            17 => {
                let e = self.node();
                Ir::Not(e, self.pos())
            }
            18 => {
                let e = self.node();
                Ir::Neg(e, self.pos())
            }
            t @ 19..=32 => {
                let l = self.node();
                let r = self.node();
                let pos = self.pos();
                match t {
                    19 => Ir::Eq(l, r, pos),
                    20 => Ir::Ne(l, r, pos),
                    21 => Ir::Lt(l, r, pos),
                    22 => Ir::Gt(l, r, pos),
                    23 => Ir::Le(l, r, pos),
                    24 => Ir::Ge(l, r, pos),
                    25 => Ir::And(l, r, pos),
                    26 => Ir::Or(l, r, pos),
                    27 => Ir::Impl(l, r, pos),
                    28 => Ir::Update(l, r, pos),
                    29 => Ir::ConcatLists(l, r, pos),
                    30 => Ir::Sub(l, r, pos),
                    31 => Ir::Mul(l, r, pos),
                    _ => Ir::Div(l, r, pos),
                }
            }
            _ => {
                self.ok = false;
                Ir::Const(Word::NULL)
            }
        };
        self.nest -= 1;
        self.ev.arena.alloc(ir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;
    use std::thread::Builder;
    use std::{env, iter};

    use crate::print;

    const SRC: &str = "let f = a: with { x = a; }; [ x 1 true null \"s\" ./p 2.5 ]; in f 3\n";
    const PATH: &[u8] = b"/t/test.nix";

    /// A valid entry for `SRC`, with the evaluator that made it.
    fn entry() -> (Evaluator, Vec<u8>, u32, u32) {
        let mut ev = Evaluator::new();
        let file = ev.add_file(PATH, SRC.as_bytes());
        let base = ev.files[file.0 as usize].base;
        let ir = ev
            .compile_source(PATH, SRC.as_bytes().to_vec(), b"/t", None)
            .expect("compile");
        let bytes = serialize(&ev, ir, base, &[], &Allows::default()).expect("entry");
        (ev, bytes, base, SRC.len() as u32)
    }

    /// What `ir` evaluates to, deeply, or the error it gives.
    fn strict_print(ev: &mut Evaluator, ir: IrRef) -> String {
        let root = ev.root_env;
        let r = ev.eval(root, ir).and_then(|w| {
            let w = ev.force_deep(w)?;
            let mut out = Vec::new();
            print::print_ambiguous(ev, w, 1, &mut out, &mut HashSet::new(), 0)?;
            Ok(String::from_utf8_lossy(&out).into_owned())
        });
        match r {
            Ok(s) => s,
            Err(e) => format!("error: {}", e.msg),
        }
    }

    /// Every case runs against one evaluator: building one and compiling
    /// `SRC` costs far more than a decode.
    #[test]
    fn a_malformed_entry_is_a_miss_and_never_a_crash() {
        let (mut ev, bytes, base, span) = entry();
        let mut loads = |b: &[u8]| deserialize(&mut ev, b, base, span, false).is_some();
        assert!(loads(&bytes), "the entry it made loads");

        for n in 0..bytes.len() {
            assert!(!loads(&bytes[..n]), "{n} bytes of {} load", bytes.len());
        }

        // a constant word of 8 under tag 0 must not reach the evaluator as
        // a pointer
        assert!(!loads(b"IETS_IR2\x00\x00\x00\x08\x00\x00"));
        for w in 0u8..64 {
            // names=0, node tag 0 (a constant), word tag 0, the word, then
            // no warnings and no allow lines
            let b = [
                b'I', b'E', b'T', b'S', b'_', b'I', b'R', b'2', 0, 0, 0, w, 0, 0,
            ];
            let ok = loads(&b);
            let imm = Word::imm_from_bits(u64::from(w)).is_some();
            assert_eq!(ok, imm, "tag 0 word {w}");
        }

        let mut corrupted = 0;
        for i in MAGIC.len()..bytes.len() {
            for b in [0x7f_u8, 0xff] {
                let mut bad = bytes.clone();
                bad[i] = b;
                if bad != bytes && !loads(&bad) {
                    corrupted += 1;
                }
            }
        }
        assert!(corrupted > 0, "a corrupt byte is a miss");

        // a `Local` slot no frame has: the evaluator reads that slot with
        // no bounds check of its own
        let mut local = bytes.clone();
        let at = local
            .windows(3)
            .position(|w| w[0] == 1 && w[1] == 0)
            .expect("a Local node");
        local[at + 2] = 0x7f;
        assert!(!loads(&local), "a slot past the frame misses");

        let mut huge = Vec::from(MAGIC.as_slice());
        huge.push(0); // no names
        huge.push(10); // a list
        huge.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x0f]); // of u32::MAX items
        assert!(!loads(&huge), "a count past the entry misses");
    }

    /// Every changed byte of a real entry, over every source shape the decoder
    /// has an invariant for. What it accepts is evaluated deeply: no entry may
    /// make the evaluator read a slot, a `with` env or an inherit env that is not
    /// there, and those faults are a panic or a SIGSEGV, not a wrong value. A byte
    /// that names another symbol is a well-formed entry for another program, which
    /// the store's integrity check stops
    /// (`a_changed_byte_of_a_stored_entry_is_a_miss`).
    #[test]
    fn a_changed_byte_never_makes_the_evaluator_read_past_its_env() {
        // the `with` depths, an `inherit (e)` frame, nested scopes, formals
        // with defaults, a rec set and constants of every stored kind
        const CASES: &[&str] = &[
            "with { a = 1; }; with { b = 2; }; with { c = 3; }; [ a b c ]\n",
            "let s = { a = 1; b = 2; }; in (let inherit (s) a b; in a + b)\n",
            "let f = { x, y ? x + 1, ... }@all: [ x y all ]; in f { x = 4; z = 9; }\n",
            "rec { a = 1; b = a + 1; c = [ b \"s\" ./p 2.5 true null ]; }\n",
            "let g = a: with { x = a; }; { inherit x; y = builtins.add x 1; }; in g 7\n",
        ];
        for (n, src) in CASES.iter().enumerate() {
            let path = format!("/t/fuzz{n}.nix").into_bytes();
            let mut ev = Evaluator::new();
            let file = ev.add_file(&path, src.as_bytes());
            let base = ev.files[file.0 as usize].base;
            let span = src.len() as u32;
            let ir = ev
                .compile_source(&path, src.as_bytes().to_vec(), b"/t", None)
                .expect("compile");
            let bytes = serialize(&ev, ir, base, &[], &Allows::default()).expect("entry");
            let want = strict_print(&mut ev, ir);
            assert!(!want.is_empty(), "case {n} evaluates");
            let mut loaded = 0;
            for i in MAGIC.len()..bytes.len() {
                for b in [0x00_u8, 0x01, 0x02, 0x7f, 0x80, 0xff] {
                    if bytes[i] == b {
                        continue;
                    }
                    let mut bad = bytes.clone();
                    bad[i] = b;
                    let Some((ir, _, _)) = deserialize(&mut ev, &bad, base, span, false) else {
                        continue;
                    };
                    loaded += 1;
                    // the assertion is that this returns at all
                    let _ = strict_print(&mut ev, ir);
                }
            }
            // the cases are chosen so that some changed bytes still decode
            // (a position, a name-table index); if none did, the fuzz proved
            // nothing about the evaluator
            assert!(loaded > 0, "case {n} accepted no changed byte");
        }
    }

    /// The same flips through the store, where the integrity check makes each
    /// one a miss before anything is decoded, and the entry is poisoned.
    #[test]
    fn a_changed_byte_of_a_stored_entry_is_a_miss() {
        let dir = env::temp_dir().join(format!("iets-ir-fuzz-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = IrCache::locate(Some(dir.to_str().expect("utf-8 path")), false).expect("cache");
        let key = "ab".to_string() + &"c".repeat(60);
        let other = "ab".to_string() + &"d".repeat(60);
        let (_, bytes, _, _) = entry();
        cache.write(&key, &bytes);
        let file = dir.join("ir").join("ab").join(&key[2..]);
        let stored = fs::read(&file).expect("stored entry");
        assert_eq!(cache.read(&key).as_deref(), Some(bytes.as_slice()));

        for i in 0..stored.len() {
            for b in [0x00_u8, 0x01, 0xff] {
                if stored[i] == b {
                    continue;
                }
                let mut bad = stored.clone();
                bad[i] = b;
                fs::write(&file, &bad).expect("entry");
                assert!(cache.read(&key).is_none(), "byte {i} set to {b:#04x}");
                assert!(!file.exists(), "and the entry is poisoned");
                fs::write(&file, &stored).expect("entry");
            }
        }

        // the check covers the key, so an entry copied to another name is not
        // an entry there
        cache.write(&other, &stored);
        fs::copy(&file, dir.join("ir").join("ab").join(&other[2..])).expect("copy");
        assert!(cache.read(&other).is_none(), "an entry under another key");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The nesting cap is sized for the evaluator's stack, not a test
    /// thread's, so this case gets a stack of its own.
    #[test]
    fn nesting_past_the_cap_misses() {
        let mut deep = Vec::from(MAGIC.as_slice());
        deep.push(0); // no names
        deep.extend(iter::repeat_n(17u8, MAX_NEST as usize + 16)); // Not(Not(…
        let t = Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                let (mut ev, _, base, span) = entry();
                deserialize(&mut ev, &deep, base, span, false).is_some()
            })
            .expect("spawn");
        assert!(!t.join().expect("decode"));
    }
}
