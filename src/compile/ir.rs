//! Resolved intermediate representation. A variable is a `(depth, index)`
//! pair into the runtime env chain. The compiler merges attrpaths, expands
//! `inherit` and folds `builtins.x` on the unshadowed global to a constant.
//! IR nodes live forever (`&'static`) so thunks can point at them.

use crate::eval::Pos;
use crate::runtime::Word;
use crate::syntax::symbol::Sym;

pub type IrRef = &'static Ir;

pub enum Ir {
    /// Literal, or pre-resolved builtin.
    Const(Word),
    /// Lexically bound variable: walk `depth` envs up, take slot `idx`.
    Local {
        depth: u32,
        idx: u32,
    },
    /// Variable bound by `with`. `withs` lists, innermost first, the depth of
    /// each enclosing with-env relative to the current env.
    WithVar {
        sym: Sym,
        withs: &'static [u32],
        pos: Pos,
    },
    /// `__curPos`.
    CurPos(Pos),
    /// `+` chains and interpolated strings/paths. When `force_string` the
    /// result is a string even for a single path part; when the first
    /// part is a path the result is a path; ints and floats add.
    Concat {
        force_string: bool,
        parts: &'static [IrRef],
        pos: Pos,
    },
    /// `<name>`: search path lookup of the string produced by `name`.
    SearchPath {
        name: IrRef,
        pos: Pos,
    },
    /// `e.a.b or d` with a dynamic or multi-element path; boxed so the
    /// node stays 32 bytes.
    Select(&'static SelectIr),
    /// A `Select` whose path has the static name `revCount`, where `rev-count`
    /// looks at the attribute before the read. No other name pays for that
    /// test.
    SelectCount(&'static SelectIr),
    /// `e.name`, the common case, inline.
    Select1 {
        subject: IrRef,
        sym: Sym,
        default: Option<IrRef>,
        pos: Pos,
    },
    HasAttr {
        subject: IrRef,
        path: &'static [AttrElemIr],
    },
    Attrs(&'static AttrsIr),
    List(&'static [IrRef]),
    Lambda(&'static LambdaIr),
    Apply {
        func: IrRef,
        args: &'static [IrRef],
        pos: Pos,
    },
    /// `let`: new env with `binds.len() + from.len()` slots.
    Let(&'static LetIr),
    /// New env with one slot holding the (thunked) scope attrs.
    With {
        scope: IrRef,
        body: IrRef,
        pos: Pos,
    },
    If {
        cond: IrRef,
        then: IrRef,
        else_: IrRef,
        pos: Pos,
    },
    Assert {
        cond: IrRef,
        body: IrRef,
        pos: Pos,
    },
    Not(IrRef, Pos),
    Neg(IrRef, Pos),
    Eq(IrRef, IrRef, Pos),
    Ne(IrRef, IrRef, Pos),
    Lt(IrRef, IrRef, Pos),
    Gt(IrRef, IrRef, Pos),
    Le(IrRef, IrRef, Pos),
    Ge(IrRef, IrRef, Pos),
    And(IrRef, IrRef, Pos),
    Or(IrRef, IrRef, Pos),
    Impl(IrRef, IrRef, Pos),
    Update(IrRef, IrRef, Pos),
    ConcatLists(IrRef, IrRef, Pos),
    Sub(IrRef, IrRef, Pos),
    Mul(IrRef, IrRef, Pos),
    Div(IrRef, IrRef, Pos),
    /// A binding of a large set or `let`, or a file's root, compiled for the
    /// fine trace only (`trace::fine`): evaluating it records region `id`.
    Region {
        id: u32,
        body: IrRef,
    },
}

pub enum AttrElemIr {
    Static(Sym),
    Dynamic(IrRef),
}

/// Where a binding's value expression is evaluated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindScope {
    /// In the attrs' own env (rec attrs, let) or the enclosing env (plain attrs).
    Own,
    /// `inherit x` in a rec set or let: the enclosing env.
    Outer,
    /// `inherit (e) x`: the env holding the `from` thunks.
    From,
}

pub struct BindIr {
    pub sym: Sym,
    pub value: IrRef,
    pub pos: Pos,
    pub scope: BindScope,
}

pub struct AttrsIr {
    pub rec: bool,
    /// Sorted by sym. For rec sets, slot `i` of the new env holds bind `i`.
    pub binds: &'static [BindIr],
    /// `inherit (e)` sources; occupy slots after the binds (rec) or their own
    /// small env (plain attrs).
    pub from: &'static [IrRef],
    pub dynamic: &'static [DynBindIr],
    pub pos: Pos,
    /// Index of `__overrides` in `binds`, rec sets only.
    pub overrides: Option<u32>,
    /// The fine trace's literal when every bind is a region, else `NO_LIT`.
    pub lit: u32,
}

pub const NO_LIT: u32 = u32::MAX;

pub struct DynBindIr {
    pub name: IrRef,
    pub value: IrRef,
    pub pos: Pos,
}

pub struct LambdaIr {
    pub param: ParamIr,
    pub body: IrRef,
    pub pos: Pos,
    pub name: Option<Sym>,
    /// The `x` in `x: ...` or the `args` in `{ ... }@args: ...`; `toXML` prints it.
    pub arg: Option<Sym>,
    /// The body reads no variable of an enclosing scope (no `Local` above
    /// its own frame, no `with` variable), so every closure of it is alike.
    pub closed: bool,
}

pub enum ParamIr {
    /// One slot.
    Ident,
    /// Slot 0 is the `@` binding when `has_bind`; formals follow in sorted
    /// order.
    Formals {
        has_bind: bool,
        formals: &'static [FormalIr],
        ellipsis: bool,
    },
}

pub struct FormalIr {
    pub sym: Sym,
    pub default: Option<IrRef>,
    pub pos: Pos,
}

impl LambdaIr {
    pub fn nslots(&self) -> usize {
        match &self.param {
            ParamIr::Ident => 1,
            ParamIr::Formals {
                has_bind, formals, ..
            } => formals.len() + usize::from(*has_bind),
        }
    }
}

pub struct SelectIr {
    pub subject: IrRef,
    pub path: &'static [AttrElemIr],
    pub default: Option<IrRef>,
    pub pos: Pos,
}

pub struct LetIr {
    pub binds: &'static [BindIr],
    pub from: &'static [IrRef],
    pub body: IrRef,
}

// Every evaluated file keeps its IR for the whole run: 101 MB on nixos-server
// at 32 bytes per node, 122 MB when one variant made it 48, so the node size
// is load-bearing.
const _: () = assert!(size_of::<Ir>() == 32);
