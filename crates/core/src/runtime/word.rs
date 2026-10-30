//! `Word` is the one-machine-word value representation.
//!
//! Low three bits are the tag:
//!
//! | tag | meaning |
//! |-----|---------|
//! | 000 | pointer to a heap object (see `value.rs`); the word is never 0 |
//! | 001 | 61-bit immediate integer |
//! | 010 | pointer to a thunk object, not yet evaluated |
//! | 011 | immediates: null, true, false, blackhole |
//!
//! `Word(0)` is EMPTY: no value. Only thunk result cells hold it.
//!
//! # Why the bits are private
//!
//! `view` and the getters over it dereference a pointer-shaped word. They
//! are sound only for a word this crate built, so the bit pattern is
//! private. The operations are `pub` for `api` and `cli` in the crate
//! above; to a frontend a `Word` is an opaque token: it takes one from
//! `api::evaluate`, passes it back to the library, and reads it through
//! `api::value` (docs/reference/library.md). Inside the evaluator a
//! pointer-shaped word comes from a `&'static` arena object, so no safe
//! path invents an address. Only two entries take raw bits: `from_bits`,
//! which is `unsafe`, and `imm_from_bits`, which accepts immediates only.
//!
//! Outside the crate the bits cannot be invented:
//!
//! ```compile_fail,E0423
//! let w = iets::runtime::Word(8);
//! ```
//!
//! and the word cannot be dereferenced:
//!
//! ```compile_fail,E0624
//! fn f(w: iets::runtime::Word) -> iets::runtime::Value {
//!     w.view()
//! }
//! ```
//!
//! Reading one through `api::value`:
//!
//! ```ignore
//! let mut ev = iets::eval::Evaluator::new();
//! let mut e = iets::api::value::Eval::new(&mut ev);
//! let v = e.eval_expr("1 + 1", b"/").unwrap();
//! assert_eq!(e.force(v).unwrap().as_int(), Some(2));
//! ```

use std::fmt::{self, Debug, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};

use super::value::{LambdaObj, MapDesc, ThunkApp2Obj, ThunkAppObj, ThunkObj};

/// One evaluator value, tagged as above.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Word(u64);

/// An arena object a `TAG_PTR` word may point at.
///
/// # Safety
///
/// An implementor is allocated in the arena, is 8-byte aligned so the tag
/// bits are free, and starts with the `Kind` byte `Word::view` reads.
pub unsafe trait Obj {}

pub const TAG_MASK: u64 = 0b111;
pub const TAG_PTR: u64 = 0b000;
pub const TAG_INT: u64 = 0b001;
pub const TAG_THUNK: u64 = 0b010;
pub const TAG_THUNK_APP: u64 = 0b110;
pub const TAG_THUNK_APP2: u64 = 0b100;
pub const TAG_LAMBDA: u64 = 0b101;
pub const TAG_IMM: u64 = 0b011;
/// `mapAttrs` entry: bits 3..48 point at a `MapDesc`, bits 48.. hold the
/// entry index; the entry's own cell in the result set is the thunk.
pub const TAG_THUNK_MAP: u64 = 0b111;
/// One bit per tag value: 010, 100, 110, 111 are thunks.
const THUNK_TAGS: u64 =
    (1 << TAG_THUNK) | (1 << TAG_THUNK_APP2) | (1 << TAG_THUNK_APP) | (1 << TAG_THUNK_MAP);

const IMM_NULL: u64 = TAG_IMM;
const IMM_TRUE: u64 = TAG_IMM | (1 << 3);
const IMM_FALSE: u64 = TAG_IMM | (2 << 3);
const IMM_BLACKHOLE: u64 = TAG_IMM | (3 << 3);
/// Forced state written by a helper fiber. Distinct from `EMPTY` so that a
/// printer can tell a thunk the demand path forced from one only a helper
/// forced (docs/explanation/evaluation.md, "Why the answer is identical").
/// One bit above `EMPTY`, so "forced" is one mask on the force path; it is
/// no payload, because a payload is a pointer or a tagged immediate.
const IMM_SPECULATED: u64 = 0x40;
/// `SPECULATED`, with effects parked on the thunk (`parallel::force::defer`):
/// a reader that sees the plain mark need not look them up.
const IMM_SPECULATED_FX: u64 = 0xc0;
/// Bits a forced state word may hold. `state & !FORCED_BITS == 0` says
/// the thunk is forced, by the demand path or by a helper.
pub const FORCED_BITS: u64 = IMM_SPECULATED_FX;

pub const INT_MIN: i64 = -(1 << 60);
pub const INT_MAX: i64 = (1 << 60) - 1;

impl Word {
    pub const EMPTY: Word = Word(0);
    pub const NULL: Word = Word(IMM_NULL);
    pub const TRUE: Word = Word(IMM_TRUE);
    pub const FALSE: Word = Word(IMM_FALSE);
    pub const BLACKHOLE: Word = Word(IMM_BLACKHOLE);
    pub const SPECULATED: Word = Word(IMM_SPECULATED);
    pub const SPECULATED_FX: Word = Word(IMM_SPECULATED_FX);

    /// The bit pattern, for a hash key, an atomic slot or the cache.
    #[inline]
    pub fn bits(self) -> u64 {
        self.0
    }

    /// The word those bits stand for.
    ///
    /// # Safety
    /// `bits` must be the `bits()` of a word this evaluator built: either
    /// an immediate, or a tagged pointer to an arena object of that tag's
    /// kind. Bits from anywhere else make `view` dereference an address of
    /// the caller's choosing.
    #[inline]
    pub const unsafe fn from_bits(bits: u64) -> Word {
        Word(bits)
    }

    /// The word those bits stand for when they are an immediate, else
    /// `None`. The checked entry for bits that were stored outside the
    /// process: no pointer can come out of it.
    #[inline]
    pub fn imm_from_bits(bits: u64) -> Option<Word> {
        match bits & TAG_MASK {
            TAG_INT => Some(Word(bits)),
            TAG_IMM => matches!(bits, IMM_NULL | IMM_TRUE | IMM_FALSE).then_some(Word(bits)),
            _ => None,
        }
    }

    #[inline]
    pub fn tag(self) -> u64 {
        self.0 & TAG_MASK
    }

    #[inline]
    pub fn from_bool(b: bool) -> Word {
        if b { Word::TRUE } else { Word::FALSE }
    }

    /// Immediate integer. Caller checks the range with `fits_imm`.
    #[inline]
    pub fn imm_int(v: i64) -> Word {
        debug_assert!(Self::fits_imm(v));
        Word(((v << 3) as u64) | TAG_INT)
    }

    #[inline]
    pub fn fits_imm(v: i64) -> bool {
        (INT_MIN..=INT_MAX).contains(&v)
    }

    #[inline]
    pub fn is_imm_int(self) -> bool {
        self.tag() == TAG_INT
    }

    #[inline]
    pub fn imm_int_value(self) -> i64 {
        (self.0 as i64) >> 3
    }

    #[inline]
    pub fn is_thunk(self) -> bool {
        (THUNK_TAGS >> self.tag()) & 1 != 0
    }

    #[inline]
    pub fn is_ptr(self) -> bool {
        self.tag() == TAG_PTR && self.0 != 0
    }

    /// Any BLACKHOLE, including the claimer-tagged parallel forms (bit 7
    /// is the has-waiters flag, bits 8.. the claimer id).
    #[inline]
    pub fn is_blackhole(self) -> bool {
        self.0 & 0x7F == IMM_BLACKHOLE
    }

    /// The BLACKHOLE a fiber writes while it forces a thunk.
    #[inline]
    pub fn blackhole_claimed(id: u32) -> Word {
        Word(IMM_BLACKHOLE | ((id as u64) << 8))
    }

    /// The claimer id of a `blackhole_claimed` word.
    #[inline]
    pub fn blackhole_claimer(self) -> u32 {
        debug_assert!(self.is_blackhole());
        (self.0 >> 8) as u32
    }

    #[inline]
    pub fn ptr<T>(self) -> *const T {
        (self.0 & !TAG_MASK) as *const T
    }

    /// A value word over an arena object.
    #[inline]
    pub fn from_ref<T: Obj>(r: &'static T) -> Word {
        let p = r as *const T as u64;
        debug_assert!(p & TAG_MASK == 0);
        Word(p)
    }

    /// A pointer-shaped word over an arena object that is *not* a value:
    /// the IR and the environment a thunk's cells hold until it is forced.
    /// `view` never sees one.
    #[inline]
    pub fn ptr_to<T>(r: &'static T) -> Word {
        const {
            assert!(
                align_of::<T>() >= 8,
                "a tagged pointer needs 8-byte alignment"
            )
        };
        Word(r as *const T as u64)
    }

    #[inline]
    pub fn thunk_from_ref(t: &'static ThunkObj) -> Word {
        Word(t as *const ThunkObj as u64 | TAG_THUNK)
    }

    #[inline]
    pub fn thunk_app_from_ref(t: &'static ThunkAppObj) -> Word {
        Word(t as *const ThunkAppObj as u64 | TAG_THUNK_APP)
    }

    #[inline]
    pub fn thunk_app2_from_ref(t: &'static ThunkApp2Obj) -> Word {
        Word(t as *const ThunkApp2Obj as u64 | TAG_THUNK_APP2)
    }

    #[inline]
    pub fn lambda_from_ref(l: &'static LambdaObj) -> Word {
        Word(l as *const LambdaObj as u64 | TAG_LAMBDA)
    }

    /// Largest entry index a map thunk word can carry.
    pub const MAP_MAX_IDX: usize = 0xFFFF;

    #[inline]
    pub fn thunk_map_from_ref(d: &'static MapDesc, idx: usize) -> Word {
        let p = d as *const MapDesc as u64;
        debug_assert!(p & TAG_MASK == 0 && p >> 48 == 0);
        debug_assert!(idx <= Self::MAP_MAX_IDX);
        Word(p | ((idx as u64) << 48) | TAG_THUNK_MAP)
    }

    /// The descriptor pointer and entry index of a map thunk word.
    #[inline]
    pub fn map_parts<T>(self) -> (*const T, usize) {
        debug_assert!(self.tag() == TAG_THUNK_MAP);
        (
            (self.0 & 0x0000_FFFF_FFFF_FFF8) as *const T,
            (self.0 >> 48) as usize,
        )
    }
}

/// A mutable slot holding a `Word`. Env slots, list elements, attr values and
/// thunk results are cells.
///
/// Loads are `Acquire` and stores `Release` so that an object built on one
/// thread is fully visible to another thread that reads the word pointing
/// at it. Both are plain moves on x86-64. The parallel claim protocol on
/// thunk results lives in `parallel::thunk`.
#[repr(transparent)]
pub struct Cell(AtomicU64);

impl Cell {
    #[inline]
    pub const fn new(w: Word) -> Cell {
        Cell(AtomicU64::new(w.0))
    }

    #[inline]
    pub fn get(&self) -> Word {
        Word(self.0.load(Ordering::Acquire))
    }

    #[inline]
    pub fn set(&self, w: Word) {
        self.0.store(w.0, Ordering::Release)
    }

    /// The slot as the atomic it is, so `parallel::thunk` can CAS the claim
    /// protocol on it. Whatever is stored through this comes back out of
    /// `get` as a `Word`, so only `Word::bits()` may be written; that
    /// obligation is the reason the accessor is crate-private and has one
    /// caller.
    #[inline]
    pub fn atomic(&self) -> &AtomicU64 {
        &self.0
    }
}

impl Debug for Word {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.tag() {
            TAG_INT => write!(f, "Int({})", self.imm_int_value()),
            TAG_THUNK => write!(f, "Thunk({:#x})", self.0),
            TAG_THUNK_APP => write!(f, "ThunkApp({:#x})", self.0),
            TAG_THUNK_APP2 => write!(f, "ThunkApp2({:#x})", self.0),
            TAG_THUNK_MAP => write!(f, "ThunkMap({:#x})", self.0),
            TAG_LAMBDA => write!(f, "Lambda({:#x})", self.0),
            TAG_IMM => match *self {
                Word::NULL => write!(f, "null"),
                Word::TRUE => write!(f, "true"),
                Word::FALSE => write!(f, "false"),
                w if w.is_blackhole() => write!(f, "blackhole({})", self.0 >> 8),
                _ => write!(f, "Imm({:#x})", self.0),
            },
            _ => {
                if self.0 == 0 {
                    write!(f, "EMPTY")
                } else {
                    write!(f, "Ptr({:#x})", self.0)
                }
            }
        }
    }
}
