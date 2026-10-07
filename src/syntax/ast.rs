//! Shared AST. The parser produces it, `compile` consumes it. The shape is
//! deliberately raw: attrpaths are not merged, scopes are not resolved,
//! `inherit` is kept as written. Those transforms live in `compile`.
//!
//! Every variable-length payload (attrpaths, string parts, bindings,
//! arguments, formals, literal bytes) lives in one side table of the
//! `Ast` and a node holds a `Range` into it. A file parses into nine
//! vectors, not one heap block per node, so building and dropping the
//! tree costs a handful of allocations.

use super::symbol::Sym;

use std::ops;

use crate::lint::allow::Allows;

/// Index into `Ast::exprs`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ExprId(pub u32);

/// Index into the evaluator's file table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct FileId(pub u32);

/// Byte range in the file. Line and column are derived from a line table.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

/// Contiguous run in one of the `Ast` side tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Range {
    pub start: u32,
    pub len: u32,
}

impl Range {
    #[inline]
    pub fn len(self) -> usize {
        self.len as usize
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn bounds(self) -> ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

/// One parsed file. `exprs[i]` and `spans[i]` describe the same node.
#[derive(Default)]
pub struct Ast {
    pub file: FileId,
    pub exprs: Vec<Expr>,
    pub spans: Vec<Span>,
    pub root: ExprId,
    /// List items and application arguments.
    pub ids: Vec<ExprId>,
    /// Attribute paths.
    pub elems: Vec<AttrElem>,
    /// String and path parts.
    pub parts: Vec<StrPart>,
    /// Decoded text of every `StrPart::Lit`.
    pub bytes: Vec<u8>,
    pub binds: Vec<Bind>,
    /// `inherit` names.
    pub names: Vec<(Sym, Span)>,
    pub formals: Vec<Formal>,
    /// Constructs the parser accepts but a lint may report (docs/reference/lints.md).
    pub notes: Vec<Note>,
    /// The lines `# iets: allow` comments cover (docs/reference/lints.md).
    pub allows: Allows,
}

/// A construct the parser saw and a lint may report: a `url-literal`
/// node, and a `# iets: allow` word the registry does
/// not know (`unknown-allow`), with its byte offset and its message in
/// `bytes`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Note {
    Url(ExprId),
    UnknownAllow { at: u32, msg: Range },
}

impl Ast {
    #[inline]
    pub fn expr(&self, id: ExprId) -> &Expr {
        &self.exprs[id.0 as usize]
    }
    #[inline]
    pub fn span(&self, id: ExprId) -> Span {
        self.spans[id.0 as usize]
    }
    #[inline]
    pub fn ids(&self, r: Range) -> &[ExprId] {
        &self.ids[r.bounds()]
    }
    #[inline]
    pub fn elems(&self, r: Range) -> &[AttrElem] {
        &self.elems[r.bounds()]
    }
    #[inline]
    pub fn parts(&self, r: Range) -> &[StrPart] {
        &self.parts[r.bounds()]
    }
    #[inline]
    pub fn bytes(&self, r: Range) -> &[u8] {
        &self.bytes[r.bounds()]
    }
    #[inline]
    pub fn binds(&self, r: Range) -> &[Bind] {
        &self.binds[r.bounds()]
    }
    #[inline]
    pub fn names(&self, r: Range) -> &[(Sym, Span)] {
        &self.names[r.bounds()]
    }
    #[inline]
    pub fn formals(&self, r: Range) -> &[Formal] {
        &self.formals[r.bounds()]
    }

    /// The text of a string that is exactly one literal part.
    pub fn single_lit(&self, parts: Range) -> Option<&[u8]> {
        match self.parts(parts) {
            [StrPart::Lit(l)] => Some(self.bytes(*l)),
            _ => None,
        }
    }

    /// Forget the contents but keep the buffers for the next file.
    pub fn clear(&mut self) {
        self.exprs.clear();
        self.spans.clear();
        self.ids.clear();
        self.elems.clear();
        self.parts.clear();
        self.bytes.clear();
        self.binds.clear();
        self.names.clear();
        self.formals.clear();
        self.notes.clear();
        self.allows.clear();
        self.root = ExprId(0);
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Expr {
    Int(i64),
    Float(f64),
    /// `"..."` and `''...''`. Indentation of `''` strings is already
    /// stripped; escapes are already decoded. A plain string is one `Lit`.
    Str(Range),
    /// `./foo`, `/abs`, `~/x`, `<nixpkgs>`, with optional interpolation.
    Path(PathExpr),
    Var(Sym),
    /// `e.a.b or d`
    Select {
        subject: ExprId,
        path: Range,
        default: Option<ExprId>,
    },
    /// `e ? a.b`
    HasAttr {
        subject: ExprId,
        path: Range,
    },
    Attrs {
        rec: bool,
        binds: Range,
    },
    List(Range),
    Lambda {
        param: Param,
        body: ExprId,
    },
    /// `f a b c` is one node with `args = [a, b, c]`.
    Apply {
        func: ExprId,
        args: Range,
    },
    Let {
        binds: Range,
        body: ExprId,
    },
    With {
        scope: ExprId,
        body: ExprId,
    },
    If {
        cond: ExprId,
        then: ExprId,
        else_: ExprId,
    },
    Assert {
        cond: ExprId,
        body: ExprId,
    },
    Unary {
        op: UnOp,
        expr: ExprId,
    },
    Binary {
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum StrPart {
    /// Range in `Ast::bytes`.
    Lit(Range),
    Interp(ExprId),
}

#[derive(Clone, Copy, Debug)]
pub struct PathExpr {
    pub kind: PathKind,
    /// The literal text after the kind prefix, plus interpolations.
    /// For `<nixpkgs/lib>` this is the text between the angle brackets.
    pub parts: Range,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathKind {
    /// `./x` or `../x`; parts start with `./` or `../` as written.
    Relative,
    /// `/x`; parts start with `/`.
    Absolute,
    /// `~/x`; parts start with `/` (the `~` is dropped).
    Home,
    /// `<x>`.
    Search,
}

/// One element of an attribute path: `a`, `"a"`, `${e}`, `"a${e}"`.
#[derive(Clone, Copy, Debug)]
pub enum AttrElem {
    Static(Sym),
    /// Interpolated or `${}`; a string whose value must coerce to a name.
    Dynamic(ExprId),
}

#[derive(Clone, Copy, Debug)]
pub struct Bind {
    pub span: Span,
    pub kind: BindKind,
}

#[derive(Clone, Copy, Debug)]
pub enum BindKind {
    /// `a.b.c = e;`
    Value { path: Range, value: ExprId },
    /// `inherit a b;` or `inherit (e) a b;`
    Inherit { from: Option<ExprId>, names: Range },
}

#[derive(Clone, Copy, Debug)]
pub enum Param {
    Ident(Sym),
    Formals {
        /// `name @ { ... }` or `{ ... } @ name`.
        bind: Option<Sym>,
        formals: Range,
        ellipsis: bool,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Formal {
    pub name: Sym,
    pub default: Option<ExprId>,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    Impl,
    Update,
    Concat,
    Add,
    Sub,
    Mul,
    Div,
}
