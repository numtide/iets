//! Heap objects. Every object starts with a one-byte `Kind` so a `Word` with
//! the pointer tag can be classified with one load. Variable-length objects
//! (lists, attrs, envs) keep their elements inline after a fixed header.

use super::arena::{Arena, Interned, Stat, StrEntry};
#[cfg(feature = "stats2")]
use super::census;
use super::word::{
    Cell, FORCED_BITS, Obj, TAG_IMM, TAG_INT, TAG_LAMBDA, TAG_THUNK, TAG_THUNK_APP, TAG_THUNK_APP2,
    TAG_THUNK_MAP, Word,
};
use crate::compile::ir::{Ir, LambdaIr};
use crate::eval::{self, Evaluator, Pos};
use crate::syntax::symbol::{self, Sym};
#[cfg(feature = "stats2")]
use std::sync::atomic::{AtomicU8, AtomicUsize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{ptr, slice};

/// Most contents the `strings` table records: the capacity of 2^20 buckets,
/// about 34 MiB. A NixOS eval records about 240,000.
const STRINGS_CAP: usize = (1 << 20) / 8 * 7;

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Int = 1,
    Float,
    String,
    Path,
    List,
    Attrs,
    Lambda,
    PrimOp,
    PrimOpApp,
    AttrsUnion,
    Thunk,
    ThunkApp,
    ThunkApp2,
    ThunkMap,
}

/// Type of a forced value, as Nix sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueType {
    Int,
    Float,
    Bool,
    Null,
    String,
    Path,
    List,
    Attrs,
    Function,
}

impl ValueType {
    pub fn name(self) -> &'static str {
        match self {
            ValueType::Int => "an integer",
            ValueType::Float => "a float",
            ValueType::Bool => "a Boolean",
            ValueType::Null => "null",
            ValueType::String => "a string",
            ValueType::Path => "a path",
            ValueType::List => "a list",
            ValueType::Attrs => "a set",
            ValueType::Function => "a function",
        }
    }

    pub fn type_of(self) -> &'static str {
        match self {
            ValueType::Int => "int",
            ValueType::Float => "float",
            ValueType::Bool => "bool",
            ValueType::Null => "null",
            ValueType::String => "string",
            ValueType::Path => "path",
            ValueType::List => "list",
            ValueType::Attrs => "set",
            ValueType::Function => "lambda",
        }
    }
}

#[repr(C)]
pub struct IntObj {
    pub kind: Kind,
    pub val: i64,
}

#[repr(C)]
pub struct FloatObj {
    pub kind: Kind,
    pub val: f64,
}

/// String context element. Ordered so contexts can be kept as sorted,
/// deduplicated slices and merged linearly.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Ctx {
    /// A store path referenced as-is.
    Opaque(&'static [u8]),
    /// `=drv`: the whole derivation closure.
    DrvDeep(&'static [u8]),
    /// `!out!drv`: one output of a derivation. `drv` is the derivation that
    /// output comes from, written as value/context.cc writes it: the `.drv`
    /// path, or one `<output>!` per layer of dynamic derivation above it, so
    /// `out!/nix/store/x.drv` is the output of the derivation that is the
    /// `out` of `x.drv`. `split_built` takes it apart.
    Built {
        drv: &'static [u8],
        output: &'static [u8],
    },
    /// A mark Nix does not have. Invisible to `getContext` and
    /// `hasContext`, never in an ATerm; the lints on derivation attributes
    /// read it. Last so the store elements sort first.
    Impure(Taint),
}

/// Takes a `Ctx::Built.drv` apart: the output names of the dynamic derivations
/// above the `.drv` at the bottom, outermost first, and that `.drv` path.
/// Mirrors the `parseRest` of value/context.cc:NixStringContextElem::parse.
/// A plain built path has no names.
pub fn split_built(drv: &[u8]) -> (Vec<&[u8]>, &[u8]) {
    let mut names = Vec::new();
    let mut rest = drv;
    while let Some(i) = rest.iter().position(|&b| b == b'!') {
        names.push(&rest[..i]);
        rest = &rest[i + 1..];
    }
    (names, rest)
}

/// What makes a string impure.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Taint {
    /// An absolute path outside the store that a coercion kept as text
    /// (`toString ./x`): `impure-path-in-drv`.
    Path(&'static [u8]),
    /// Metadata of a source tree, by name (`self.rev`,
    /// `inputs.nixpkgs.shortRev`): `tree-meta-in-drv`.
    Meta(&'static [u8]),
}

impl Taint {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Taint::Path(b) | Taint::Meta(b) => b,
        }
    }
}

const _: () = assert!(size_of::<Ctx>() == 40);

impl Ctx {
    /// A store reference, as Nix's context holds: not an `Impure` marker.
    #[inline]
    pub fn is_store(&self) -> bool {
        !matches!(self, Ctx::Impure(_))
    }
}

#[repr(C)]
pub struct StringObj {
    pub kind: Kind,
    /// Census only (`stats2`): set once the string was viewed.
    pub flags: u8,
    /// The symbol of `bytes` plus one, 0 until first asked for
    /// (`Evaluator::force_sym`); it fills padding, the object stays 5 words.
    pub sym: AtomicU32,
    pub bytes: &'static [u8],
    pub ctx: &'static [Ctx],
}

const _: () = assert!(size_of::<StringObj>() == 40);

impl StringObj {
    #[inline]
    pub fn cached_sym(&self) -> Option<Sym> {
        self.sym.load(Ordering::Relaxed).checked_sub(1).map(Sym)
    }

    /// Every worker that asks computes the same symbol for the same
    /// content, so a racing store is harmless.
    #[inline]
    pub fn cache_sym(&self, sym: Sym) {
        self.sym.store(sym.0 + 1, Ordering::Relaxed);
    }
}

#[repr(C)]
pub struct PathObj {
    pub kind: Kind,
    pub path: &'static [u8],
}

#[repr(C)]
pub struct ListObj {
    pub kind: Kind,
    /// Census only (`stats2`): set once the cells were read.
    pub flags: u8,
    pub len: u32,
    // followed by `len` Cells
}

#[repr(C)]
pub struct Attr {
    pub sym: Sym,
    pub pos: Pos,
    pub cell: Cell,
}

#[repr(C)]
pub struct AttrsObj {
    /// `Kind::Attrs`: `len` entries follow. `Kind::AttrsUnion`: a
    /// `UnionTail` follows and the entries are those of `b` over `a`.
    pub kind: Kind,
    /// Producer, see `census::ORIGINS`; written by every build.
    pub origin: u8,
    /// Census only (`stats2`): selections (low 7 bits, saturating) and
    /// bit 7 once the entries were iterated.
    pub flags: u8,
    /// Flat slices an iteration merges: 1 for a flat set.
    pub leaves: u8,
    /// Entry count; `u32::MAX` on a union whose count was not needed yet.
    pub len: AtomicU32,
    // followed by `len` Attrs sorted by sym, or by a `UnionTail`
}

/// `a // b` without the copy: the flat leaves of both sides, lowest
/// priority first. Lookups probe the leaves from the last; an iteration
/// merges them; `flat` caches the copy once someone needs the entries as
/// one slice.
#[repr(C)]
pub struct UnionTail {
    pub flat: Cell,
    // followed by `leaves` pointers to flat sets
}

/// Leaves a union may hold before `//` flattens its left side first:
/// bounds a lookup miss to this many probes.
pub const MAX_LEAVES: u8 = 8;
/// Left operands with fewer entries are merged flat: the copy is cheaper
/// than the union plus an extra probe.
pub const UNION_MIN: usize = 8;

/// Flat leaves a set contributes to a union: itself, its cached flat
/// copy, or its own leaves.
#[inline]
pub fn for_each_leaf(a: &'static AttrsObj, mut f: impl FnMut(&'static AttrsObj)) {
    if a.kind == Kind::Attrs {
        f(a)
    } else if let Some(flat) = a.flattened() {
        f(flat)
    } else {
        for leaf in a.leaf_sets() {
            f(leaf)
        }
    }
}

#[inline]
pub fn leaf_count(a: &'static AttrsObj) -> usize {
    if a.kind == Kind::Attrs || a.flattened().is_some() {
        1
    } else {
        a.leaves as usize
    }
}

/// Envs are reached only through `up`, lambdas and thunks, never through a
/// `Word`, so they carry no `Kind`; slot indices come from the compiler, so
/// they carry no length either. Header is one word.
#[repr(C)]
pub struct EnvObj {
    pub up: *const EnvObj,
    // followed by the slots
}

/// Closures are the third most common object; `TAG_LAMBDA` on the word
/// replaces the Kind byte so the object is two words.
#[repr(C)]
pub struct LambdaObj {
    pub env: &'static EnvObj,
    pub lam: &'static LambdaIr,
}

pub type PrimFn = fn(&mut Evaluator, &[Word], Pos) -> Result<Word, Box<eval::Error>>;

#[repr(C)]
pub struct PrimOpObj {
    pub kind: Kind,
    pub arity: u8,
    /// The interned `name`, for a profile frame. It sits in the padding
    /// after `arity`, so the object is the same size with it.
    pub sym: Sym,
    pub name: &'static str,
    pub f: PrimFn,
}

#[repr(C)]
pub struct PrimOpAppObj {
    pub kind: Kind,
    pub op: &'static PrimOpObj,
    pub args: &'static [Word],
}

// SAFETY: each of these is allocated only by the `mk_*`/`alloc_*` methods
// below, which align it to 8, and each starts with the `Kind` byte `view`
// reads. `Word::from_ref` is safe because of this list.
unsafe impl Obj for IntObj {}
unsafe impl Obj for FloatObj {}
unsafe impl Obj for StringObj {}
unsafe impl Obj for PathObj {}
unsafe impl Obj for ListObj {}
unsafe impl Obj for AttrsObj {}
unsafe impl Obj for PrimOpObj {}
unsafe impl Obj for PrimOpAppObj {}

/// A suspended `ir` in `env`. A force overwrites it in place. Thunk
/// objects carry no `Kind` byte: the word tag tells the layouts apart.
/// Every layout has one `state` cell and one `code` word
/// (docs/explanation/evaluation.md). `state` holds the payload while
/// unforced (never EMPTY, never a BLACKHOLE), a BLACKHOLE during the
/// force, and EMPTY after it. `code` holds the IR or the function until
/// the force, and the forced value after it.
#[repr(C)]
pub struct ThunkObj {
    /// `&Ir`, then the value.
    pub code: Cell,
    /// `&EnvObj`, BLACKHOLE, EMPTY.
    pub state: Cell,
}

/// A suspended application `func arg`, as created by `map`, `genList`, etc.
#[repr(C)]
pub struct ThunkAppObj {
    /// `func`, then the value.
    pub code: Cell,
    /// `arg`, BLACKHOLE, EMPTY.
    pub state: Cell,
}

/// A suspended two-argument application `func a b`, as created by
/// `zipAttrsWith` and by `mapAttrs` on sets too large for a map word.
#[repr(C)]
pub struct ThunkApp2Obj {
    /// `func`, then the value.
    pub code: Cell,
    pub arg1: Word,
    /// `arg2`, BLACKHOLE, EMPTY.
    pub state: Cell,
}

/// One per `mapAttrs` call. Entry `i` of `out` starts as the map word
/// `(desc, i)`: a thunk with no object of its own. Its `code` word is the
/// cell of that entry (the map word, then the value). Its state is
/// `states[i]` (NULL, BLACKHOLE, EMPTY). A force applies `f` to the name
/// and to entry `i` of `src`. Attr cells never hold a BLACKHOLE, so
/// readers of `attr.cell` need no special case.
#[repr(C)]
pub struct MapDesc {
    pub f: Word,
    /// Flat, same syms as `out`.
    pub src: &'static AttrsObj,
    pub out: &'static AttrsObj,
    // followed by `out.len()` state cells
}

impl MapDesc {
    #[inline]
    pub fn state(&self, i: usize) -> &'static Cell {
        debug_assert!(i < self.out.len());
        // SAFETY: `mk_map_attrs` placed one cell per entry after the header.
        unsafe { &*((self as *const MapDesc).add(1) as *const Cell).add(i) }
    }
}

const _: () = assert!(size_of::<ListObj>() == 8);
const _: () = assert!(size_of::<AttrsObj>() == 8);
const _: () = assert!(size_of::<EnvObj>() == 8);
const _: () = assert!(size_of::<Attr>() == 16);
const _: () = assert!(size_of::<ThunkObj>() == 16);
const _: () = assert!(size_of::<ThunkAppObj>() == 16);
const _: () = assert!(size_of::<ThunkApp2Obj>() == 24);
const _: () = assert!(size_of::<MapDesc>() == 24);
const _: () = assert!(size_of::<LambdaObj>() == 16);

impl ThunkObj {
    /// The IR, valid while `state` is not EMPTY.
    #[inline]
    pub fn ir(&self) -> &'static Ir {
        // SAFETY: `mk_thunk` stored an arena IR pointer in `code`; it is
        // overwritten only when `state` becomes EMPTY.
        unsafe { &*self.code.get().ptr::<Ir>() }
    }

    /// The env a state word holds while unforced.
    #[inline]
    pub fn env_of(state: Word) -> &'static EnvObj {
        // SAFETY: `mk_thunk` stored an arena env pointer as the state.
        unsafe { &*state.ptr::<EnvObj>() }
    }
}

/// Census marks (`stats2`): a byte in header padding is written through a
/// raw pointer. Single-worker measurement runs only.
#[cfg(feature = "stats2")]
#[inline]
fn mark_byte(p: *const u8, f: impl Fn(u8) -> u8) {
    // SAFETY: `p` is a flags byte inside a live arena object, viewed as an
    // atomic byte.
    let a = unsafe { &*(p as *const AtomicU8) };
    let _ = a.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(f(v)));
}

impl ListObj {
    #[inline]
    pub fn cells(&self) -> &'static [Cell] {
        #[cfg(feature = "stats2")]
        mark_byte(&self.flags, |_| 1);
        // SAFETY: the allocator placed `len` cells directly after the header.
        unsafe {
            let p = (self as *const ListObj).add(1) as *const Cell;
            slice::from_raw_parts(p, self.len as usize)
        }
    }
}

impl AttrsObj {
    /// The entries as one sorted slice. A union must have been flattened
    /// (`Arena::flat`) by the caller; iteration without a copy is `iter()`.
    #[inline]
    pub fn attrs(&self) -> &'static [Attr] {
        #[cfg(feature = "stats2")]
        mark_byte(&self.flags, |f| f | 0x80);
        self.attrs_raw()
    }

    #[inline]
    pub fn attrs_raw(&self) -> &'static [Attr] {
        if self.kind == Kind::AttrsUnion {
            return self
                .flattened()
                .expect("attrs() on a union: flatten first")
                .attrs_raw();
        }
        // SAFETY: the allocator placed `len` attrs directly after the header.
        unsafe {
            let p = (self as *const AttrsObj).add(1) as *const Attr;
            slice::from_raw_parts(p, self.len_raw() as usize)
        }
    }

    #[inline]
    fn len_raw(&self) -> u32 {
        self.len.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn is_union(&self) -> bool {
        self.kind == Kind::AttrsUnion
    }

    /// The `UnionTail` of a union.
    #[inline]
    pub fn union(&self) -> &'static UnionTail {
        debug_assert!(self.is_union());
        // SAFETY: `alloc_union` placed the tail directly after the header.
        unsafe { &*((self as *const AttrsObj).add(1) as *const UnionTail) }
    }

    /// The flat sets a union merges, lowest priority first.
    #[inline]
    pub fn leaf_sets(&self) -> &'static [&'static AttrsObj] {
        debug_assert!(self.is_union());
        // SAFETY: `alloc_union` placed `leaves` pointers after the tail.
        unsafe {
            let p = (self.union() as *const UnionTail).add(1) as *const &'static AttrsObj;
            slice::from_raw_parts(p, self.leaves as usize)
        }
    }

    /// The cached flat copy of a union, once made.
    #[inline]
    pub fn flattened(&self) -> Option<&'static AttrsObj> {
        let w = self.union().flat.get();
        if w == Word::EMPTY {
            return None;
        }
        // SAFETY: `flat` holds a pointer to a flat arena set or EMPTY.
        Some(unsafe { &*w.ptr::<AttrsObj>() })
    }

    #[inline]
    pub fn len(&self) -> usize {
        let n = self.len_raw();
        if n != u32::MAX {
            return n as usize;
        }
        let n = self.iter().count();
        self.len.store(n as u32, Ordering::Relaxed);
        n
    }

    /// A lower and an upper bound of `len`. Unlike `len`, it does not
    /// iterate a union: a union has at least as many attributes as its
    /// largest leaf and at most as many as all its leaves together.
    #[inline]
    pub fn len_bounds(&self) -> (usize, usize) {
        let n = self.len_raw();
        if n != u32::MAX {
            return (n as usize, n as usize);
        }
        if let Some(flat) = self.flattened() {
            return flat.len_bounds();
        }
        let leaves = self.leaf_sets().iter().map(|l| l.len_raw() as usize);
        leaves.fold((0, 0), |(lo, hi), n| (lo.max(n), hi + n))
    }

    /// `len` for the census, which must not count as a use.
    #[cfg(feature = "stats2")]
    pub fn len_unmarked(&self) -> usize {
        let f = self.flags;
        let n = self.len();
        self.set_flags(f);
        n
    }

    #[cfg(feature = "stats2")]
    pub fn set_flags(&self, f: u8) {
        mark_byte(&self.flags, |_| f);
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        // a union never holds two empty sides
        self.kind == Kind::Attrs && self.len_raw() == 0
    }

    /// Binary search over symbol ids; linear for small sets. On a union
    /// the leaves are probed from the last (the right side of the last
    /// `//`) without a copy.
    #[inline]
    pub fn get(&self, sym: Sym) -> Option<&'static Attr> {
        #[cfg(feature = "stats2")]
        mark_byte(&self.flags, |f| if f & 0x7f < 0x7f { f + 1 } else { f });
        if self.kind == Kind::Attrs {
            #[cfg(feature = "stats2")]
            census::note_get(0, 0);
            return self.get_flat(sym);
        }
        if let Some(flat) = self.flattened() {
            #[cfg(feature = "stats2")]
            census::note_get(1, 0);
            return flat.get_flat(sym);
        }
        #[cfg(feature = "stats2")]
        {
            let (probes, hit) = self.probes(sym);
            census::note_get(if hit { 2 } else { 4 }, probes);
        }
        for leaf in self.leaf_sets().iter().rev() {
            if let Some(a) = leaf.get_flat(sym) {
                return Some(a);
            }
        }
        None
    }

    /// Leaves a lookup of `sym` on this union probes, and whether it hits.
    #[cfg(feature = "stats2")]
    fn probes(&self, sym: Sym) -> (u64, bool) {
        let leaves = self.leaf_sets().iter().rev();
        match leaves.clone().position(|l| l.get_flat(sym).is_some()) {
            Some(i) => (i as u64 + 1, true),
            None => (leaves.len() as u64, false),
        }
    }

    #[inline]
    fn get_flat(&self, sym: Sym) -> Option<&'static Attr> {
        let attrs = self.attrs_raw();
        if attrs.len() <= 8 {
            attrs.iter().find(|a| a.sym == sym)
        } else {
            attrs
                .binary_search_by(|a| a.sym.cmp(&sym))
                .ok()
                .map(|i| &attrs[i])
        }
    }

    /// The entries in sym order, merging a union's leaves on the fly.
    #[inline]
    pub fn iter(&self) -> AttrIter {
        #[cfg(feature = "stats2")]
        mark_byte(&self.flags, |f| f | 0x80);
        let mut it = AttrIter {
            leaves: [&[]; SENTINEL + 1],
            keys: [u64::MAX; SENTINEL + 1],
            tree: [0; MAX_LEAVES as usize],
            n: 1,
        };
        if self.kind == Kind::Attrs {
            it.leaves[0] = self.attrs_raw();
        } else if let Some(flat) = self.flattened() {
            it.leaves[0] = flat.attrs_raw();
        } else {
            let sets = self.leaf_sets();
            for (i, s) in sets.iter().enumerate() {
                it.leaves[i] = s.attrs_raw();
            }
            it.n = sets.len();
            it.build();
        }
        it
    }
}

/// The leaf slot that stands for an exhausted stream while the tree is built.
const SENTINEL: usize = MAX_LEAVES as usize;

/// Merging iterator over the flat leaves of a set, lowest priority first; on
/// equal syms the last leaf wins (`b` over `a`). More than one leaf runs a loser
/// tree: `tree[0]` is the winner and `tree[i]` the loser of the match under node
/// `i`, the parent of leaf `s` being `(s + n) / 2`, so an advance costs one root
/// path instead of a rescan of every head. Leaf `i` keys as
/// `(sym + 1) << 3 | (7 - i)`, which puts the last leaf first on equal syms. Key
/// 0 is the sentinel and `u64::MAX` an exhausted leaf, so neither ties with a head.
pub struct AttrIter {
    leaves: [&'static [Attr]; SENTINEL + 1],
    keys: [u64; SENTINEL + 1],
    tree: [u8; MAX_LEAVES as usize],
    n: usize,
}

impl AttrIter {
    #[inline]
    fn set_key(&mut self, i: usize) {
        self.keys[i] = match self.leaves[i].first() {
            Some(a) => ((a.sym.0 as u64 + 1) << 3) | (MAX_LEAVES as u64 - 1 - i as u64),
            None => u64::MAX,
        };
    }

    /// Replays leaf `s` from its parent to the root, restoring the loser at
    /// every node on the way and leaving the winner in `tree[0]`.
    #[inline]
    fn adjust(&mut self, s: usize) {
        let mut s = s;
        let mut t = (s + self.n) / 2;
        while t > 0 {
            let o = self.tree[t] as usize;
            if self.keys[s] > self.keys[o] {
                self.tree[t] = s as u8;
                s = o;
            }
            t /= 2;
        }
        self.tree[0] = s as u8;
    }

    /// Every match starts against the sentinel, which is the smallest key
    /// while the tree fills so that each real leaf is pushed into the tree
    /// and the sentinel is displaced out of `tree[0]` by the last replay.
    fn build(&mut self) {
        self.tree = [SENTINEL as u8; MAX_LEAVES as usize];
        self.keys[SENTINEL] = 0;
        for i in 0..self.n {
            self.set_key(i);
        }
        for s in (0..self.n).rev() {
            self.adjust(s);
        }
        self.keys[SENTINEL] = u64::MAX;
    }
}

impl Iterator for AttrIter {
    type Item = &'static Attr;

    #[inline]
    fn next(&mut self) -> Option<&'static Attr> {
        if self.n == 1 {
            let (first, rest) = self.leaves[0].split_first()?;
            self.leaves[0] = rest;
            return Some(first);
        }
        let w = self.tree[0] as usize;
        let (item, rest) = self.leaves[w].split_first()?;
        self.leaves[w] = rest;
        self.set_key(w);
        self.adjust(w);
        let sym = (item.sym.0 as u64 + 1) << 3;
        loop {
            let v = self.tree[0] as usize;
            if self.keys[v] & !7 != sym {
                break;
            }
            self.leaves[v] = &self.leaves[v][1..];
            self.set_key(v);
            self.adjust(v);
        }
        Some(item)
    }
}

impl EnvObj {
    /// Slot `i`. The compiler resolved `i` against this env's binder, so it
    /// is in range by construction.
    #[inline]
    pub fn slot(&self, i: usize) -> &'static Cell {
        // SAFETY: `alloc_env` placed the slots directly after the header and
        // every index the evaluator uses was resolved at compile time.
        unsafe { &*((self as *const EnvObj).add(1) as *const Cell).add(i) }
    }

    /// The first `n` slots, for filling a fresh env of `n` slots.
    #[inline]
    pub fn slots_n(&self, n: usize) -> &'static [Cell] {
        // SAFETY: as for `slot`; the caller passes the `n` it allocated.
        unsafe {
            let p = (self as *const EnvObj).add(1) as *const Cell;
            slice::from_raw_parts(p, n)
        }
    }

    #[inline]
    pub fn up(&self) -> Option<&'static EnvObj> {
        #[cfg(feature = "stats2")]
        let up = ((self.up as usize) & !7) as *const EnvObj;
        #[cfg(not(feature = "stats2"))]
        let up = self.up;
        // SAFETY: `up` is either null or an arena env that lives forever.
        unsafe { up.as_ref() }
    }

    /// Census (`stats2`): a slot of this env was read.
    #[inline]
    pub fn mark_read(&self) {
        #[cfg(feature = "stats2")]
        self.mark_bit(1);
    }

    /// Census (`stats2`): a thunk, closure or child env holds this env.
    #[inline]
    pub fn mark_captured(&self) {
        #[cfg(feature = "stats2")]
        self.mark_bit(2);
    }

    #[cfg(feature = "stats2")]
    #[inline]
    fn mark_bit(&self, bit: usize) {
        // SAFETY: `up` is a pointer-sized, pointer-aligned word viewed as an
        // atomic; the low bits are free (8-byte aligned) and masked by `up()`.
        unsafe {
            let a = &*(&self.up as *const *const EnvObj as *const AtomicUsize);
            a.fetch_or(bit, Ordering::Relaxed);
        }
    }
}

/// Forced-value view of a `Word`.
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    String(&'static StringObj),
    Path(&'static PathObj),
    List(&'static ListObj),
    Attrs(&'static AttrsObj),
    Lambda(&'static LambdaObj),
    PrimOp(&'static PrimOpObj),
    PrimOpApp(&'static PrimOpAppObj),
}

impl Word {
    /// Classify a forced word. Panics on thunks and EMPTY: callers force first.
    #[inline]
    pub fn view(self) -> Value {
        match self.tag() {
            TAG_INT => Value::Int(self.imm_int_value()),
            TAG_IMM => match self {
                Word::NULL => Value::Null,
                Word::TRUE => Value::Bool(true),
                Word::FALSE => Value::Bool(false),
                _ => panic!("view: blackhole"),
            },
            TAG_THUNK | TAG_THUNK_APP | TAG_THUNK_APP2 | TAG_THUNK_MAP => {
                panic!("view: unforced thunk")
            }
            // SAFETY: lambda-tagged words point at an arena LambdaObj.
            TAG_LAMBDA => Value::Lambda(unsafe { &*self.ptr::<LambdaObj>() }),
            _ => {
                debug_assert!(self.bits() != 0, "view: empty word");
                // SAFETY: pointer-tagged words always point at an arena
                // object that starts with a Kind byte.
                unsafe {
                    let p: *const u8 = self.ptr();
                    match *(p as *const Kind) {
                        Kind::Int => Value::Int((*(p as *const IntObj)).val),
                        Kind::Float => Value::Float((*(p as *const FloatObj)).val),
                        Kind::String => {
                            #[cfg(feature = "stats2")]
                            mark_byte(&(*(p as *const StringObj)).flags, |_| 1);
                            Value::String(&*(p as *const StringObj))
                        }
                        Kind::Path => Value::Path(&*(p as *const PathObj)),
                        Kind::List => Value::List(&*(p as *const ListObj)),
                        Kind::Attrs | Kind::AttrsUnion => Value::Attrs(&*(p as *const AttrsObj)),
                        Kind::PrimOp => Value::PrimOp(&*(p as *const PrimOpObj)),
                        Kind::PrimOpApp => Value::PrimOpApp(&*(p as *const PrimOpAppObj)),
                        k => panic!("view: not a value object: {k:?}"),
                    }
                }
            }
        }
    }

    #[inline]
    pub fn value_type(self) -> ValueType {
        match self.view() {
            Value::Int(_) => ValueType::Int,
            Value::Float(_) => ValueType::Float,
            Value::Bool(_) => ValueType::Bool,
            Value::Null => ValueType::Null,
            Value::String(_) => ValueType::String,
            Value::Path(_) => ValueType::Path,
            Value::List(_) => ValueType::List,
            Value::Attrs(_) => ValueType::Attrs,
            Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_) => ValueType::Function,
        }
    }

    #[inline]
    pub fn heap_kind(self) -> Option<Kind> {
        if self.tag() == TAG_LAMBDA {
            return Some(Kind::Lambda);
        }
        if self.is_ptr() {
            // SAFETY: see `view`.
            Some(unsafe { *(self.ptr::<u8>() as *const Kind) })
        } else {
            None
        }
    }

    #[inline]
    pub fn as_attrs(self) -> Option<&'static AttrsObj> {
        match self.view() {
            Value::Attrs(a) => Some(a),
            _ => None,
        }
    }

    #[inline]
    pub fn as_list(self) -> Option<&'static ListObj> {
        match self.view() {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    #[inline]
    pub fn as_string(self) -> Option<&'static StringObj> {
        match self.view() {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    #[inline]
    pub fn as_int(self) -> Option<i64> {
        match self.view() {
            Value::Int(i) => Some(i),
            _ => None,
        }
    }

    /// Which thunk layout a THUNK-tagged word points at.
    #[inline]
    pub fn thunk_kind(self) -> Kind {
        debug_assert!(self.is_thunk());
        match self.tag() {
            TAG_THUNK_APP => Kind::ThunkApp,
            TAG_THUNK_APP2 => Kind::ThunkApp2,
            TAG_THUNK_MAP => Kind::ThunkMap,
            _ => Kind::Thunk,
        }
    }

    /// The cell whose word is this thunk's state: the payload while
    /// unforced, a BLACKHOLE while claimed, then EMPTY (value in `code`).
    #[inline]
    pub fn thunk_state_cell(self) -> &'static Cell {
        // SAFETY: the tag names the layout; every layout keeps its state
        // cell where the match says, and a map word's cell is entry `i`
        // of the result set it names.
        unsafe {
            match self.tag() {
                TAG_THUNK_APP2 => &(*self.ptr::<ThunkApp2Obj>()).state,
                TAG_THUNK_MAP => {
                    let (d, i) = self.map_desc();
                    d.state(i)
                }
                _ => &(*self.ptr::<ThunkObj>()).state,
            }
        }
    }

    /// The cell that holds the IR or function until forced and the value
    /// after: the object's first word, or the map entry's own cell.
    #[inline]
    fn thunk_code_cell(self) -> &'static Cell {
        if self.tag() == TAG_THUNK_MAP {
            let (d, i) = self.map_desc();
            &d.out.attrs_raw()[i].cell
        } else {
            // SAFETY: every object layout starts with `code`.
            unsafe { &(*self.ptr::<ThunkObj>()).code }
        }
    }

    /// True when `state` (read from `thunk_state_cell`) still holds the
    /// payload: the thunk was not forced and is not being forced.
    #[inline]
    pub fn thunk_unforced(self, state: Word) -> bool {
        state.bits() & !FORCED_BITS != 0 && !state.is_blackhole()
    }

    /// True when the forced state came from a helper fiber and the demand
    /// path has not asked for this thunk yet.
    #[inline]
    pub fn thunk_speculated(self, state: Word) -> bool {
        state == Word::SPECULATED || state == Word::SPECULATED_FX
    }

    /// The forced value once the state is EMPTY.
    #[inline]
    pub fn thunk_value(self, _state: Word) -> Word {
        self.thunk_code_cell().get()
    }

    /// Store the forced value; returns the state word to publish. A helper
    /// publishes `SPECULATED` so its work stays invisible until demanded.
    #[inline]
    pub fn thunk_finish(self, v: Word, demanded: bool) -> Word {
        self.thunk_code_cell().set(v);
        if demanded {
            Word::EMPTY
        } else {
            Word::SPECULATED
        }
    }

    /// Descriptor and entry index of a map word.
    #[inline]
    pub fn map_desc(self) -> (&'static MapDesc, usize) {
        let (p, i) = self.map_parts::<MapDesc>();
        // SAFETY: map words are built by `mapAttrs` over an arena `MapDesc`.
        (unsafe { &*p }, i)
    }

    #[inline]
    pub fn as_thunk(self) -> &'static ThunkObj {
        // SAFETY: caller checked `thunk_kind() == Kind::Thunk`.
        unsafe { &*self.ptr::<ThunkObj>() }
    }

    #[inline]
    pub fn as_thunk_app(self) -> &'static ThunkAppObj {
        // SAFETY: caller checked `thunk_kind() == Kind::ThunkApp`.
        unsafe { &*self.ptr::<ThunkAppObj>() }
    }

    #[inline]
    pub fn as_thunk_app2(self) -> &'static ThunkApp2Obj {
        // SAFETY: caller checked `thunk_kind() == Kind::ThunkApp2`.
        unsafe { &*self.ptr::<ThunkApp2Obj>() }
    }
}

impl Arena {
    #[inline]
    pub fn mk_int(&mut self, v: i64) -> Word {
        if Word::fits_imm(v) {
            Word::imm_int(v)
        } else {
            self.stats.note(Stat::BigInt, size_of::<IntObj>());
            Word::from_ref(self.alloc(IntObj {
                kind: Kind::Int,
                val: v,
            }))
        }
    }

    #[inline]
    pub fn mk_float(&mut self, v: f64) -> Word {
        self.stats.note(Stat::Float, size_of::<FloatObj>());
        Word::from_ref(self.alloc(FloatObj {
            kind: Kind::Float,
            val: v,
        }))
    }

    pub fn mk_string_static(&mut self, bytes: &'static [u8], ctx: &'static [Ctx]) -> Word {
        self.stats.note(Stat::StrObj, size_of::<StringObj>());
        let s = self.alloc(StringObj {
            kind: Kind::String,
            flags: 0,
            sym: AtomicU32::new(0),
            bytes,
            ctx,
        });
        #[cfg(feature = "stats2")]
        {
            self.stats
                .census
                .note_string(s as *const StringObj, bytes, size_of_val(ctx));
            self.stats.census.note_ctx(ctx);
        }
        Word::from_ref(s)
    }

    /// A context-free string, shared by content with every earlier one.
    pub fn mk_string(&mut self, bytes: &[u8]) -> Word {
        let i = self.intern_bytes(bytes);
        self.mk_string_over(i, &[])
    }

    pub fn mk_string_ctx(&mut self, bytes: &[u8], ctx: &[Ctx]) -> Word {
        let i = self.intern_bytes(bytes);
        let c = self.intern_ctx(ctx);
        self.mk_string_over(i, c)
    }

    /// Content hash of string bytes for the `strings` table. The table is
    /// keyed by this 64-bit value so a probe compares integers and the
    /// bytes are read only on a hit; a full collision of distinct contents
    /// (about 1 in 2^64 per pair) just skips the sharing.
    #[inline]
    fn str_hash(bytes: &[u8]) -> u64 {
        symbol::fx_bytes(bytes)
    }

    /// The recorded content equal to `bytes`, if any.
    #[inline]
    fn str_lookup(&self, hash: u64, bytes: &[u8]) -> Option<Interned> {
        self.strings
            .get(&hash)
            .filter(|e| e.bytes == bytes)
            .map(|e| Interned {
                bytes: e.bytes,
                word: e.word,
                hash,
            })
    }

    /// Record `b` under `hash`; the entry it collides with, if any, keeps
    /// its own content. A full table records nothing more, so a fold that
    /// makes a million unique strings cannot grow it without bound.
    #[inline]
    fn str_record(&mut self, hash: u64, b: &'static [u8]) -> Interned {
        if self.strings.len() >= STRINGS_CAP {
            return Interned {
                bytes: b,
                word: Word::EMPTY,
                hash: 0,
            };
        }
        let e = self.strings.entry(hash).or_insert(StrEntry {
            bytes: b,
            word: Word::EMPTY,
        });
        Interned {
            bytes: b,
            word: Word::EMPTY,
            hash: if e.bytes.as_ptr() == b.as_ptr() {
                hash
            } else {
                0
            },
        }
    }

    /// The arena copy of `bytes`, made once per distinct content, and the
    /// context-free string object over it if one exists.
    #[inline]
    pub fn intern_bytes(&mut self, bytes: &[u8]) -> Interned {
        let hash = Self::str_hash(bytes);
        if let Some(i) = self.str_lookup(hash, bytes) {
            return i;
        }
        self.stats.note(Stat::Bytes, bytes.len());
        let b = self.alloc_bytes(bytes);
        self.str_record(hash, b)
    }

    /// Like `intern_bytes` for bytes that live as long as the session
    /// already, such as a slice of another string: recorded without a copy.
    pub fn intern_static(&mut self, bytes: &'static [u8]) -> Interned {
        let hash = Self::str_hash(bytes);
        if let Some(i) = self.str_lookup(hash, bytes) {
            return i;
        }
        self.str_record(hash, bytes)
    }

    /// Like `intern_bytes` for the concatenation of `parts`: the pieces are
    /// joined in the arena and given back when the content is known.
    pub fn intern_concat(&mut self, parts: &[&[u8]]) -> Interned {
        let m = self.mark();
        let b = self.alloc_concat(parts);
        let hash = Self::str_hash(b);
        if let Some(i) = self.str_lookup(hash, b) {
            // SAFETY: the content was already interned, so `b` is dead from
            // here on and nothing else was allocated since `m`.
            unsafe { self.release(m) };
            return i;
        }
        self.stats.note(Stat::Bytes, b.len());
        self.str_record(hash, b)
    }

    /// The string object over interned bytes: for an empty context the
    /// shared object (made and recorded on first use), else a fresh object
    /// sharing the bytes.
    #[inline]
    pub fn mk_string_over(&mut self, i: Interned, ctx: &'static [Ctx]) -> Word {
        if !ctx.is_empty() {
            return self.mk_string_static(i.bytes, ctx);
        }
        if i.word != Word::EMPTY {
            return i.word;
        }
        let w = self.mk_string_static(i.bytes, &[]);
        if i.hash != 0
            && let Some(e) = self.strings.get_mut(&i.hash)
        {
            e.word = w;
        }
        w
    }

    /// Bytes for a string object whose bytes were already copied (from
    /// a `Vec` built by a builtin): interned by content.
    pub fn mk_string_shared(&mut self, bytes: &[u8], ctx: &'static [Ctx]) -> Word {
        let i = self.intern_bytes(bytes);
        self.mk_string_over(i, ctx)
    }

    /// A sorted, deduplicated context slice, shared by content.
    pub fn intern_ctx(&mut self, ctx: &[Ctx]) -> &'static [Ctx] {
        if ctx.is_empty() {
            return &[];
        }
        if let Some(&c) = self.ctxs.get(ctx) {
            return c;
        }
        self.stats.note(Stat::Ctx, size_of_val(ctx));
        let c: &'static [Ctx] = self.alloc_slice(ctx);
        self.ctxs.insert(c);
        c
    }

    pub fn mk_path(&mut self, path: &[u8]) -> Word {
        self.stats
            .note(Stat::Path, size_of::<PathObj>() + path.len());
        let p = self.alloc_bytes(path);
        Word::from_ref(self.alloc(PathObj {
            kind: Kind::Path,
            path: p,
        }))
    }

    /// Allocate a list of `n` cells, all EMPTY. Fill through `cells()`.
    pub fn alloc_list(&mut self, n: usize) -> &'static ListObj {
        self.alloc_list_as(n, Stat::List)
    }

    /// Bytes a list of `n` cells needs, or `None` when `n` is past what a
    /// `ListObj` can address or the count overflows. A builtin that takes
    /// the length from the program checks this and reports out of memory.
    pub fn list_bytes(n: usize) -> Option<usize> {
        if n > u32::MAX as usize {
            return None;
        }
        n.checked_mul(size_of::<Cell>())?
            .checked_add(size_of::<ListObj>())
    }

    /// `alloc_list` with a stats bucket naming the producer.
    pub fn alloc_list_as(&mut self, n: usize, bucket: Stat) -> &'static ListObj {
        if n == 0
            && let Some(l) = self.empty_list
        {
            return l;
        }
        let bytes = Self::list_bytes(n).expect("list size checked by the caller");
        self.stats.note(bucket, bytes);
        let p = self.alloc_raw(bytes, 8) as *mut ListObj;
        // SAFETY: fresh allocation sized for header plus n cells.
        unsafe {
            p.write(ListObj {
                kind: Kind::List,
                flags: 0,
                len: n as u32,
            });
            let cells = p.add(1) as *mut u64;
            ptr::write_bytes(cells, 0, n);
            #[cfg(feature = "stats2")]
            self.stats.census.note_list(p);
            if n == 0 {
                self.empty_list = Some(&*p);
            }
            &*p
        }
    }

    pub fn mk_list(&mut self, items: &[Word]) -> Word {
        self.mk_list_as(items, Stat::List)
    }

    pub fn mk_list_as(&mut self, items: &[Word], bucket: Stat) -> Word {
        let l = self.alloc_list_as(items.len(), bucket);
        // SAFETY: `alloc_list_as` placed `len` cells after the header; this
        // is the fill, not a read, so the census read mark stays clear.
        let cells = unsafe {
            slice::from_raw_parts((l as *const ListObj).add(1) as *const Cell, items.len())
        };
        for (c, w) in cells.iter().zip(items) {
            c.set(*w);
        }
        Word::from_ref(l)
    }

    /// `alloc_attrs` with a stats bucket naming the producer.
    pub fn alloc_attrs_as(&mut self, n: usize, bucket: Stat) -> &'static mut AttrsObj {
        self.stats
            .note(bucket, size_of::<AttrsObj>() + n * size_of::<Attr>());
        let p = self.alloc_raw(size_of::<AttrsObj>() + n * size_of::<Attr>(), 8) as *mut AttrsObj;
        // SAFETY: fresh allocation sized for header plus n attrs.
        unsafe {
            p.write(AttrsObj {
                kind: Kind::Attrs,
                origin: bucket.attrs_origin(),
                flags: 0,
                leaves: 1,
                len: AtomicU32::new(n as u32),
            });
            #[cfg(feature = "stats2")]
            self.stats.census.note_attrs(p, bucket.attrs_origin(), n);
            &mut *p
        }
    }

    /// `a // b` as a union over the flat leaves of both sides; the caller
    /// checked the leaf budget (`leaf_count`).
    pub fn alloc_union(&mut self, a: &'static AttrsObj, b: &'static AttrsObj) -> Word {
        let n = leaf_count(a) + leaf_count(b);
        debug_assert!(n <= MAX_LEAVES as usize);
        let size = size_of::<AttrsObj>() + size_of::<UnionTail>() + n * size_of::<&AttrsObj>();
        self.stats.note(Stat::AttrsUpd, size);
        let p = self.alloc_raw(size, 8) as *mut AttrsObj;
        // SAFETY: fresh allocation sized for header, tail and n pointers.
        unsafe {
            p.write(AttrsObj {
                kind: Kind::AttrsUnion,
                origin: Stat::AttrsUpd.attrs_origin(),
                flags: 0,
                leaves: n as u8,
                len: AtomicU32::new(u32::MAX),
            });
            let tail = p.add(1) as *mut UnionTail;
            tail.write(UnionTail {
                flat: Cell::new(Word::EMPTY),
            });
            let mut leaves = tail.add(1) as *mut &'static AttrsObj;
            for side in [a, b] {
                for_each_leaf(side, |leaf| {
                    leaves.write(leaf);
                    leaves = leaves.add(1);
                });
            }
            #[cfg(feature = "stats2")]
            self.stats.census.note_attrs(
                p,
                Stat::AttrsUpd.attrs_origin(),
                a.len_unmarked() + b.len_unmarked(),
            );
            Word::from_ref(&*p)
        }
    }

    /// The set as one flat slice: itself for a flat set, else the cached
    /// or freshly merged copy of the union.
    pub fn flat(&mut self, a: &'static AttrsObj) -> &'static AttrsObj {
        if a.kind == Kind::Attrs {
            return a;
        }
        let u = a.union();
        if let Some(f) = a.flattened() {
            return f;
        }
        // one merge pass, not two: the entry count is only known after the
        // merge, so the copy is sized at the upper bound (no leaf entry is
        // dropped unless a later leaf repeats its sym) and the tail the
        // duplicates left over is handed back to the bump pointer.
        let cap = a.leaf_sets().iter().map(|l| l.len_raw() as usize).sum();
        let out = self.alloc_attrs_as(cap, Stat::AttrsUpd);
        let mut n = 0;
        for e in a.iter() {
            out.init_from(n, e);
            n += 1;
        }
        out.len.store(n as u32, Ordering::Relaxed);
        // SAFETY: nothing was allocated since `out`, and `out` never had
        // more than `n` of its `cap` entries written.
        unsafe { self.shrink_last((cap - n) * size_of::<Attr>(), Stat::AttrsUpd) };
        let out: &'static AttrsObj = out;
        a.len.store(n as u32, Ordering::Relaxed);
        u.flat.set(Word::from_ref(out));
        out
    }

    /// Build attrs from `(sym, pos, word)` triples; sorts and rejects duplicates
    /// (returns the duplicate sym).
    pub fn mk_attrs(&mut self, entries: &mut [(Sym, Pos, Word)]) -> Result<Word, Sym> {
        entries.sort_by_key(|e| e.0);
        for w in entries.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(w[0].0);
            }
        }
        Ok(self.mk_attrs_sorted(entries))
    }

    /// Entries must already be sorted by sym and unique.
    pub fn mk_attrs_sorted(&mut self, entries: &[(Sym, Pos, Word)]) -> Word {
        self.mk_attrs_sorted_as(entries, Stat::Attrs)
    }

    pub fn mk_attrs_sorted_as(&mut self, entries: &[(Sym, Pos, Word)], bucket: Stat) -> Word {
        if entries.is_empty() {
            if let Some(w) = self.empty_attrs {
                return w;
            }
            let w = Word::from_ref(self.alloc_attrs_as(0, bucket));
            self.empty_attrs = Some(w);
            return w;
        }
        let a = self.alloc_attrs_as(entries.len(), bucket);
        let slots = a.attrs_mut();
        for (slot, (sym, pos, w)) in slots.iter_mut().zip(entries) {
            // SAFETY: writing into uninitialised arena memory of the right type.
            unsafe {
                ptr::write(
                    slot,
                    Attr {
                        sym: *sym,
                        pos: *pos,
                        cell: Cell::new(*w),
                    },
                )
            };
        }
        Word::from_ref(a)
    }

    pub fn alloc_env(&mut self, up: Option<&'static EnvObj>, n: usize) -> &'static EnvObj {
        self.stats
            .note(Stat::Env, size_of::<EnvObj>() + n * size_of::<Cell>());
        #[cfg(feature = "stats2")]
        if let Some(u) = up {
            u.mark_captured();
        }
        let p = self.alloc_raw(size_of::<EnvObj>() + n * size_of::<Cell>(), 8) as *mut EnvObj;
        // SAFETY: fresh allocation sized for header plus n cells.
        unsafe {
            p.write(EnvObj {
                up: up.map_or(ptr::null(), |e| e as *const EnvObj),
            });
            let cells = p.add(1) as *mut u64;
            ptr::write_bytes(cells, 0, n);
            #[cfg(feature = "stats2")]
            self.stats.census.note_env(p, n);
            &*p
        }
    }

    #[inline]
    pub fn mk_thunk(&mut self, env: &'static EnvObj, ir: &'static Ir) -> Word {
        self.stats.note(Stat::Thunk, size_of::<ThunkObj>());
        env.mark_captured();
        let t = self.alloc(ThunkObj {
            code: Cell::new(Word::ptr_to(ir)),
            state: Cell::new(Word::ptr_to(env)),
        });
        let w = Word::thunk_from_ref(t);
        #[cfg(feature = "stats2")]
        self.stats.census.note_thunk_created(w.bits());
        w
    }

    #[inline]
    pub fn mk_thunk_app(&mut self, func: Word, arg: Word) -> Word {
        debug_assert!(arg != Word::EMPTY && !arg.is_blackhole());
        self.stats.note(Stat::ThunkApp, size_of::<ThunkAppObj>());
        let t = self.alloc(ThunkAppObj {
            code: Cell::new(func),
            state: Cell::new(arg),
        });
        let w = Word::thunk_app_from_ref(t);
        #[cfg(feature = "stats2")]
        self.stats.census.note_thunk_created(w.bits());
        w
    }

    #[inline]
    pub fn mk_thunk_app2(&mut self, func: Word, arg1: Word, arg2: Word) -> Word {
        debug_assert!(arg2 != Word::EMPTY && !arg2.is_blackhole());
        self.stats.note(Stat::ThunkApp2, size_of::<ThunkApp2Obj>());
        let t = self.alloc(ThunkApp2Obj {
            code: Cell::new(func),
            arg1,
            state: Cell::new(arg2),
        });
        let w = Word::thunk_app2_from_ref(t);
        #[cfg(feature = "stats2")]
        self.stats.census.note_thunk_created(w.bits());
        w
    }

    /// `mapAttrs f src` for a flat `src` of at most `MAP_MAX_IDX + 1`
    /// entries: the result's cells are the thunks, no object per entry.
    pub fn mk_map_attrs(&mut self, f: Word, src: &'static AttrsObj) -> Word {
        debug_assert!(!src.is_union() && src.len() <= Word::MAP_MAX_IDX + 1);
        let n = src.len();
        let out: *mut AttrsObj = self.alloc_attrs_as(n, Stat::AttrsMap);
        let size = size_of::<MapDesc>() + n * size_of::<Cell>();
        self.stats.note(Stat::ThunkMap, size);
        // SAFETY: `out` is a fresh arena set that lives forever; the
        // descriptor only reads it after the fill below.
        let out_ref: &'static AttrsObj = unsafe { &*out };
        let desc = self.alloc_raw(size, 8) as *mut MapDesc;
        // SAFETY: fresh allocation sized for the header plus n state cells.
        unsafe {
            desc.write(MapDesc {
                f,
                src,
                out: out_ref,
            });
            let states = desc.add(1) as *mut Cell;
            for i in 0..n {
                states.add(i).write(Cell::new(Word::NULL));
            }
        }
        for (i, a) in src.attrs_raw().iter().enumerate() {
            // SAFETY: exclusive access to the fresh set during the fill.
            unsafe { (*out).init(i, a.sym, a.pos, Word::thunk_map_from_ref(&*desc, i)) };
        }
        Word::from_ref(out_ref)
    }

    pub fn mk_lambda(&mut self, env: &'static EnvObj, lam: &'static LambdaIr) -> Word {
        self.stats.note(Stat::Lambda, size_of::<LambdaObj>());
        env.mark_captured();
        #[cfg(feature = "stats2")]
        {
            self.stats.census.lambda_closed += lam.closed as u64;
        }
        Word::lambda_from_ref(self.alloc(LambdaObj { env, lam }))
    }

    pub fn mk_primop(&mut self, name: &'static str, arity: u8, f: PrimFn, sym: Sym) -> Word {
        Word::from_ref(self.alloc(PrimOpObj {
            kind: Kind::PrimOp,
            arity,
            name,
            sym,
            f,
        }))
    }

    pub fn mk_primop_app(&mut self, op: &'static PrimOpObj, args: &[Word]) -> Word {
        self.stats.note(
            Stat::PrimOpApp,
            size_of::<PrimOpAppObj>() + size_of_val(args),
        );
        let args = self.alloc_slice(args);
        Word::from_ref(self.alloc(PrimOpAppObj {
            kind: Kind::PrimOpApp,
            op,
            args,
        }))
    }
}

impl AttrsObj {
    #[inline]
    pub fn attrs_mut(&mut self) -> &'static mut [Attr] {
        // SAFETY: see `attrs`; exclusive access through `&mut self`.
        unsafe {
            let p = (self as *mut AttrsObj).add(1) as *mut Attr;
            slice::from_raw_parts_mut(p, self.len_raw() as usize)
        }
    }
}

/// Sort and dedup a context vector, then move it to the arena.
pub fn ctx_to_arena(arena: &mut Arena, mut v: Vec<Ctx>) -> &'static [Ctx] {
    if v.is_empty() {
        return &[];
    }
    v.sort();
    v.dedup();
    arena.intern_ctx(&v)
}

impl AttrsObj {
    /// Write entry `i` of a set from `alloc_attrs`. Callers fill every entry
    /// in sorted order before handing the set out.
    #[inline]
    pub fn init(&mut self, i: usize, sym: Sym, pos: Pos, w: Word) {
        debug_assert!(i < self.len_raw() as usize);
        // SAFETY: `alloc_attrs` reserved `len` entries after the header;
        // `write` does not read the uninitialised slot.
        unsafe {
            let p = (self as *mut AttrsObj).add(1) as *mut Attr;
            ptr::write(
                p.add(i),
                Attr {
                    sym,
                    pos,
                    cell: Cell::new(w),
                },
            )
        }
    }

    #[inline]
    pub fn init_from(&mut self, i: usize, a: &Attr) {
        self.init(i, a.sym, a.pos, a.cell.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    /// The leaves of `sets`, folded left to right: the last set holding a sym
    /// wins, as `//` does.
    fn reference(sets: &[Vec<(u32, i64)>]) -> Vec<(u32, i64)> {
        let mut m: BTreeMap<u32, i64> = BTreeMap::new();
        for s in sets {
            for (sym, v) in s {
                m.insert(*sym, *v);
            }
        }
        m.into_iter().collect()
    }

    fn build(arena: &mut Arena, sets: &[Vec<(u32, i64)>]) -> &'static AttrsObj {
        let mut acc: Option<&'static AttrsObj> = None;
        for s in sets {
            let entries: Vec<(Sym, Pos, Word)> = s
                .iter()
                .map(|(sym, v)| (Sym(*sym), Pos::NONE, arena.mk_int(*v)))
                .collect();
            let w = arena.mk_attrs_sorted(&entries);
            // SAFETY: `mk_attrs_sorted` returns a pointer to an arena set.
            let set = unsafe { &*w.ptr::<AttrsObj>() };
            acc = Some(match acc {
                None => set,
                // SAFETY: as above, for the union `alloc_union` returns.
                Some(a) => unsafe { &*arena.alloc_union(a, set).ptr::<AttrsObj>() },
            });
        }
        acc.unwrap()
    }

    fn observed(a: &'static AttrsObj) -> Vec<(u32, i64)> {
        a.iter()
            .map(|e| (e.sym.0, e.cell.get().imm_int_value()))
            .collect()
    }

    /// Every leaf count the budget allows, over syms that overlap between
    /// leaves, against a fold of the same sets.
    #[test]
    fn union_iteration_merges_like_a_fold() {
        let mut seed = 0x2545f4914f6cdd1du64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for n in 1..=MAX_LEAVES as usize {
            for round in 0..40 {
                let mut arena = Arena::new();
                let mut sets: Vec<Vec<(u32, i64)>> = Vec::new();
                for leaf in 0..n {
                    let len = 1 + (rng() % 12) as usize;
                    let mut syms: Vec<u32> = (0..len).map(|_| (rng() % 20) as u32).collect();
                    syms.sort_unstable();
                    syms.dedup();
                    sets.push(
                        syms.into_iter()
                            .map(|s| (s, (leaf * 1000 + s as usize) as i64))
                            .collect(),
                    );
                }
                let a = build(&mut arena, &sets);
                let want = reference(&sets);
                assert_eq!(observed(a), want, "n={n} round={round} sets={sets:?}");
                assert_eq!(a.len(), want.len(), "len n={n} round={round}");
                let flat = arena.flat(a);
                assert_eq!(observed(flat), want, "flat n={n} round={round}");
                assert_eq!(flat.len_raw() as usize, want.len());
            }
        }
    }

    /// A lookup must see the same winner an iteration emits.
    #[test]
    fn union_lookup_agrees_with_iteration() {
        let mut arena = Arena::new();
        let sets: Vec<Vec<(u32, i64)>> = (0..MAX_LEAVES as usize)
            .map(|leaf| {
                (0u32..10)
                    .filter(|s| !(s + leaf as u32).is_multiple_of(3))
                    .map(|s| (s, (leaf * 100 + s as usize) as i64))
                    .collect()
            })
            .collect();
        let a = build(&mut arena, &sets);
        for (sym, want) in reference(&sets) {
            let got = a.get(Sym(sym)).expect("present");
            assert_eq!(got.cell.get().imm_int_value(), want);
        }
        assert!(a.get(Sym(999)).is_none());
    }
}

#[cfg(test)]
mod alloc_tests {
    use super::*;

    #[test]
    fn a_list_too_long_to_address_has_no_size() {
        assert_eq!(Arena::list_bytes(0), Some(size_of::<ListObj>()));
        assert_eq!(
            Arena::list_bytes(3),
            Some(size_of::<ListObj>() + 3 * size_of::<Cell>())
        );
        assert_eq!(Arena::list_bytes(u32::MAX as usize + 1), None);
        assert_eq!(Arena::list_bytes(i64::MAX as usize), None);
    }

    #[test]
    fn a_full_string_table_records_no_more_but_still_shares() {
        let mut arena = Arena::new();
        let first = arena.mk_string(b"s0");
        for i in 1..STRINGS_CAP + 10 {
            arena.mk_string(format!("s{i}").as_bytes());
        }
        assert_eq!(arena.strings.len(), STRINGS_CAP);
        assert_eq!(arena.mk_string(b"s0"), first);
        let late = format!("s{}", STRINGS_CAP + 5);
        let w = arena.mk_string(late.as_bytes());
        assert_eq!(w.as_string().unwrap().bytes, late.as_bytes());
    }

    #[test]
    fn a_static_slice_is_interned_without_a_copy() {
        let mut arena = Arena::new();
        let whole = arena.intern_bytes(b"hello world").bytes;
        let i = arena.intern_static(&whole[6..]);
        assert_eq!(i.bytes.as_ptr(), whole[6..].as_ptr());
        assert_eq!(
            arena.intern_bytes(b"world").bytes.as_ptr(),
            i.bytes.as_ptr()
        );
    }

    #[test]
    fn a_speculated_thunk_is_forced_but_not_demanded() {
        let mut arena = Arena::new();
        let t = arena.mk_thunk_app(Word::NULL, Word::imm_int(1));
        let st = t.thunk_state_cell();
        assert!(t.thunk_unforced(st.get()));
        assert!(!t.thunk_speculated(st.get()));
        let done = t.thunk_finish(Word::imm_int(7), false);
        assert!(!t.thunk_unforced(done));
        assert!(t.thunk_speculated(done));
        assert_eq!(t.thunk_value(done).imm_int_value(), 7);
        let done = t.thunk_finish(Word::imm_int(7), true);
        assert!(!t.thunk_unforced(done));
        assert!(!t.thunk_speculated(done));
    }
}

#[cfg(test)]
mod layout {
    use super::*;

    /// `sym`, for a profile frame, must stay free: it sits in the padding
    /// after `arity`, and every primop is one of these.
    #[test]
    fn a_primop_pays_nothing_for_its_symbol() {
        assert_eq!(size_of::<PrimOpObj>(), 32);
    }
}
