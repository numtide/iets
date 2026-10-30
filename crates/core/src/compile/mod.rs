//! AST -> IR: scope resolution, attrpath merging, inherit expansion,
//! literal allocation, and folding of `builtins.x` to constants. The
//! compile-time lints (docs/reference/lints.md) hook the same walk.

pub mod cache;
pub mod ir;

use crate::eval::{Error, ErrorKind, Pos};
use crate::lint::{self, Level, LintId};
use crate::runtime::{Arena, AttrsObj, Word};
use crate::syntax::ast::{
    Ast, AttrElem, BinOp, Bind, BindKind, Expr, ExprId, Note, Param, PathKind, Range, StrPart, UnOp,
};
use crate::syntax::symbol::{FxHashMap, Interner, Sym};
use crate::sys;
use ir::{
    AttrElemIr, AttrsIr, BindIr, BindScope, DynBindIr, FormalIr, Ir, IrRef, LambdaIr, LetIr,
    ParamIr, SelectIr,
};

use std::cell::Cell;
use std::{env, mem};

/// Names visible at the root of every file.
#[derive(Clone)]
pub struct Globals {
    pub table: FxHashMap<Sym, Word>,
    /// The `builtins` attrset, for `builtins.x` folding.
    pub builtins: Option<&'static AttrsObj>,
}

enum Scope {
    /// Names and their slots; `shares` when the slots extend the frame of
    /// the scope below instead of opening one (a lambda's body lambda or
    /// `let`).
    Lexical {
        names: FxHashMap<Sym, u32>,
        shares: bool,
    },
    With,
}

impl Scope {
    fn frame(names: FxHashMap<Sym, u32>) -> Scope {
        Scope::Lexical {
            names,
            shares: false,
        }
    }
}

pub struct Compiler<'a> {
    ast: &'a Ast,
    arena: &'a mut Arena,
    interner: &'a mut Interner,
    globals: &'a Globals,
    scopes: Vec<Scope>,
    /// Open lambdas, innermost last: the index in `scopes` of the param
    /// frame and whether the body has read a variable from outside it.
    lambdas: Vec<(usize, Cell<bool>)>,
    /// Sorted start offsets of each symbol's `Var`s and `inherit` names,
    /// built on the file's first `x: e`.
    var_uses: Option<FxHashMap<Sym, Vec<u32>>>,
    /// Directory of the file being compiled, for relative paths.
    base_dir: Vec<u8>,
    /// Global offset of this file's first byte.
    base: u32,
    home: Option<Vec<u8>>,
    /// Set when a `~/` path literal was compiled; the evaluator then records `$HOME`.
    pub used_home: Cell<bool>,
    /// Lint levels; `None` for an embedded source, which is never linted.
    lints: Option<&'a lint::Config>,
    /// The warnings the compile produced; the evaluator reports them.
    pub diags: Vec<lint::Diag>,
    sym_builtins: Sym,
    sym_rev_count: Sym,
    sym_cur_pos: Sym,
    sym_overrides: Sym,
    sym_body: Sym,
}

type CResult<T> = Result<T, Box<Error>>;

fn err<T>(kind: ErrorKind, msg: String, pos: Pos) -> CResult<T> {
    Err(Box::new(Error::new(kind, msg, pos)))
}

/// Every variable reference is a `Var` or an `inherit` name.
fn var_uses(ast: &Ast) -> FxHashMap<Sym, Vec<u32>> {
    let vars = ast
        .exprs
        .iter()
        .zip(&ast.spans)
        .filter_map(|(e, s)| match e {
            Expr::Var(v) => Some((*v, s)),
            _ => None,
        });
    let inherits = ast.names.iter().map(|(v, s)| (*v, s));
    let mut m: FxHashMap<Sym, Vec<u32>> = FxHashMap::default();
    for (v, s) in vars.chain(inherits) {
        m.entry(v).or_default().push(s.start);
    }
    for v in m.values_mut() {
        v.sort_unstable();
    }
    m
}

/// Intermediate attrpath-merged binding tree, mirroring Nix's `addAttr`.
struct Merged {
    rec: bool,
    statics: Vec<(Sym, Def, Pos)>,
    index: FxHashMap<Sym, usize>,
    dynamics: Vec<(ExprId, Def, Pos)>,
    from_exprs: Vec<ExprId>,
    pos: Pos,
}

enum Def {
    Expr(ExprId),
    Nested(Box<Merged>),
    Inherit(Sym),
    InheritFrom(usize, Sym),
}

impl Merged {
    fn new(rec: bool, pos: Pos) -> Self {
        Merged {
            rec,
            statics: Vec::new(),
            index: FxHashMap::default(),
            dynamics: Vec::new(),
            from_exprs: Vec::new(),
            pos,
        }
    }

    /// The nested node at `statics[idx]`, which the caller has just made a
    /// `Def::Nested` (a fresh push, or `open_nested`).
    fn nested_at(&mut self, idx: usize) -> &mut Merged {
        match &mut self.statics[idx].1 {
            Def::Nested(n) => n,
            _ => unreachable!("statics[{idx}] was just set to Def::Nested"),
        }
    }
}

/// The name of an attrpath element: a symbol when it is known at compile
/// time (a literal, or a string with no interpolation), otherwise the
/// expression that computes it.
enum Name {
    Static(Sym),
    Dynamic(ExprId),
}

impl<'a> Compiler<'a> {
    pub fn new(
        ast: &'a Ast,
        arena: &'a mut Arena,
        interner: &'a mut Interner,
        globals: &'a Globals,
        base: u32,
        base_dir: &[u8],
        lints: Option<&'a lint::Config>,
    ) -> Self {
        let sym_builtins = interner.intern(b"builtins");
        let sym_rev_count = interner.intern(b"revCount");
        let sym_cur_pos = interner.intern(b"__curPos");
        let sym_overrides = interner.intern(b"__overrides");
        let sym_body = interner.intern(b"body");
        Compiler {
            ast,
            arena,
            interner,
            globals,
            scopes: Vec::new(),
            lambdas: Vec::new(),
            var_uses: None,
            base_dir: base_dir.to_vec(),
            base,
            home: env::var_os("HOME").map(|h| h.into_encoded_bytes()),
            used_home: Cell::new(false),
            lints,
            diags: Vec::new(),
            sym_builtins,
            sym_rev_count,
            sym_cur_pos,
            sym_overrides,
            sym_body,
        }
    }

    /// Compile a whole file. `extra_scope`, when given, is a lexical frame
    /// (for `scopedImport`) that the runtime supplies as the env above root.
    pub fn compile_root(&mut self, extra_scope: Option<&[Sym]>) -> CResult<IrRef> {
        if let Some(names) = extra_scope {
            let mut m = FxHashMap::default();
            for (i, s) in names.iter().enumerate() {
                m.insert(*s, i as u32);
            }
            self.scopes.push(Scope::frame(m));
        }
        self.parser_notes()?;
        let root = self.ast.root;
        let ir = self.expr(root)?;
        self.scopes.clear();
        Ok(ir)
    }

    #[inline]
    fn lint_level(&self, id: LintId) -> Level {
        match self.lints {
            Some(c) => c.level(id),
            None => Level::Off,
        }
    }

    #[inline]
    fn lint_on(&self, id: LintId) -> bool {
        self.lint_level(id) != Level::Off
    }

    /// A lint site: nothing at `off` or on a line a `# iets: allow`
    /// comment covers, a diagnostic for the evaluator to report at `warn`,
    /// a compile error at `error`.
    fn lint(&mut self, id: LintId, pos: Pos, msg: impl FnOnce() -> String) -> CResult<()> {
        let level = self.lint_level(id);
        if level != Level::Off && !pos.is_none() && self.ast.allows.covers(pos.0 - self.base, id) {
            return Ok(());
        }
        match level {
            Level::Off => Ok(()),
            Level::Warn => {
                self.diags.push(lint::Diag {
                    id,
                    pos,
                    msg: msg(),
                });
                Ok(())
            }
            Level::Error => Err(lint::error(id, pos, &msg())),
        }
    }

    /// The URL literals and unknown allow names the parser noted.
    fn parser_notes(&mut self) -> CResult<()> {
        if !self.lint_on(LintId::UrlLiteral) && !self.lint_on(LintId::UnknownAllow) {
            return Ok(());
        }
        let ast: &'a Ast = self.ast;
        for note in &ast.notes {
            match *note {
                Note::Url(id) => {
                    let text = match ast.expr(id) {
                        Expr::Str(parts) => ast.single_lit(*parts).unwrap_or(b""),
                        _ => b"",
                    };
                    let error = self.lint_level(LintId::UrlLiteral) == Level::Error;
                    self.lint(LintId::UrlLiteral, self.pos(id), || {
                        lint::legacy::url_literal(text, error)
                    })?;
                }
                Note::UnknownAllow { at, msg } => {
                    self.lint(LintId::UnknownAllow, Pos(self.base + at), || {
                        String::from_utf8_lossy(ast.bytes(msg)).into_owned()
                    })?;
                }
            }
        }
        Ok(())
    }

    /// A reference to the global `name` (a bare name, or the `x` of
    /// `builtins.x`): the `currentTime` half of `impure-builtin`.
    fn global_ref(&mut self, name: &[u8], pos: Pos) -> CResult<()> {
        if self.lint_on(LintId::ImpureBuiltin)
            && let Some(msg) = lint::purity::current_time(name)
        {
            self.lint(LintId::ImpureBuiltin, pos, || msg)?;
        }
        Ok(())
    }

    fn pos(&self, id: ExprId) -> Pos {
        Pos(self.base + self.ast.span(id).start)
    }

    fn alloc<T>(&mut self, v: T) -> &'static T {
        self.arena.alloc(v)
    }

    fn alloc_refs(&mut self, v: Vec<IrRef>) -> &'static [IrRef] {
        self.arena.alloc_slice(&v)
    }

    /// A variable read from scope index `s` captures every open lambda
    /// whose frame sits above `s`.
    fn note_capture(&self, s: usize) {
        for (frame, captures) in self.lambdas.iter().rev() {
            if *frame > s {
                captures.set(true);
            } else {
                break;
            }
        }
    }

    fn resolve(&mut self, sym: Sym) -> Option<Ir> {
        let mut withs: Vec<u32> = Vec::new();
        let mut outermost_with = None;
        let mut depth = 0;
        let n = self.scopes.len();
        for (d, scope) in self.scopes.iter().rev().enumerate() {
            match scope {
                Scope::Lexical { names, shares } => {
                    if let Some(&idx) = names.get(&sym) {
                        self.note_capture(n - 1 - d);
                        return Some(Ir::Local { depth, idx });
                    }
                    if !shares {
                        depth += 1;
                    }
                }
                Scope::With => {
                    withs.push(depth);
                    outermost_with = Some(n - 1 - d);
                    depth += 1;
                }
            }
        }
        if let Some(s) = outermost_with {
            self.note_capture(s);
        }
        // Globals live in Nix's static base env, a lexical scope, so they
        // beat any enclosing `with` (`with { true = 1; }; true` is `true`).
        if let Some(w) = self.globals.table.get(&sym) {
            return Some(Ir::Const(*w));
        }
        if withs.is_empty() {
            return None;
        }
        Some(
            Ir::WithVar {
                sym,
                withs: &[],
                pos: Pos::NONE,
            }
            .with_withs(self.arena.alloc_slice(&withs)),
        )
    }

    fn expr(&mut self, id: ExprId) -> CResult<IrRef> {
        let ir = self.expr_inner(id)?;
        Ok(self.alloc(ir))
    }

    fn expr_inner(&mut self, id: ExprId) -> CResult<Ir> {
        let pos = self.pos(id);
        let ast: &'a Ast = self.ast;
        Ok(match ast.expr(id) {
            Expr::Int(i) => Ir::Const(self.arena.mk_int(*i)),
            Expr::Float(f) => Ir::Const(self.arena.mk_float(*f)),
            Expr::Str(parts) => self.string(ast.parts(*parts), pos)?,
            Expr::Path(p) => self.path(p.kind, ast.parts(p.parts), pos)?,
            Expr::Var(sym) => {
                let sym = *sym;
                // parser.y makes every `__curPos` an ExprPos, so a binding
                // of that name has no effect
                if sym == self.sym_cur_pos {
                    return Ok(Ir::CurPos(pos));
                }
                match self.resolve(sym) {
                    Some(Ir::WithVar { sym, withs, .. }) => Ir::WithVar { sym, withs, pos },
                    Some(ir) => {
                        if matches!(ir, Ir::Const(_)) {
                            self.global_ref(self.interner.name(sym), pos)?;
                        }
                        ir
                    }
                    None => {
                        return err(
                            ErrorKind::UndefinedVar,
                            format!("undefined variable '{}'", self.interner.name_str(sym)),
                            pos,
                        );
                    }
                }
            }
            Expr::Select {
                subject,
                path,
                default,
            } => {
                let path = ast.elems(*path);
                if let Expr::Var(s) = ast.expr(*subject)
                    && *s == self.sym_builtins
                    && self.lexical_lookup(*s).is_none()
                    && let Some(AttrElem::Static(first)) = path.first()
                {
                    self.global_ref(self.interner.name(*first), pos)?;
                }
                if let Some(folded) = self.fold_builtins_select(*subject, path, default.is_none()) {
                    return Ok(folded);
                }
                let subject = self.expr(*subject)?;
                let path = self.attr_path(path)?;
                let default = match default {
                    Some(d) => Some(self.expr(*d)?),
                    None => None,
                };
                self.mk_select(subject, path, default, pos)
            }
            Expr::HasAttr { subject, path } => {
                let subject = self.expr(*subject)?;
                let path = self.attr_path(ast.elems(*path))?;
                Ir::HasAttr { subject, path }
            }
            Expr::Attrs { rec, binds } => {
                let merged = self.merge_binds(*rec, ast.binds(*binds), pos)?;
                // `{ }` too is a value already
                if merged.statics.is_empty() && merged.dynamics.is_empty() {
                    return Ok(Ir::Const(self.arena.mk_attrs_sorted(&[])));
                }
                let a = self.lower_attrs(merged)?;
                Ir::Attrs(a)
            }
            Expr::List(items) => {
                let items = ast.ids(*items);
                // `[ ]` is a value already: no thunk and no list per evaluation
                if items.is_empty() {
                    return Ok(Ir::Const(self.arena.mk_list(&[])));
                }
                let mut v = Vec::with_capacity(items.len());
                for i in items {
                    v.push(self.expr(*i)?);
                }
                Ir::List(self.alloc_refs(v))
            }
            Expr::Lambda { param, body } => {
                Ir::Lambda(self.lambda(*param, *body, pos, None, None)?)
            }
            Expr::Apply { func, args } => {
                let func = self.expr(*func)?;
                let args = ast.ids(*args);
                let mut v = Vec::with_capacity(args.len());
                for a in args {
                    v.push(self.expr(*a)?);
                }
                Ir::Apply {
                    func,
                    args: self.alloc_refs(v),
                    pos,
                }
            }
            Expr::Let { binds, body } => self.let_in(*binds, *body, pos, None)?,
            Expr::With { scope, body } => {
                let scope = self.expr(*scope)?;
                self.scopes.push(Scope::With);
                let body = self.expr(*body)?;
                self.scopes.pop();
                Ir::With { scope, body, pos }
            }
            Expr::If { cond, then, else_ } => Ir::If {
                cond: self.expr(*cond)?,
                then: self.expr(*then)?,
                else_: self.expr(*else_)?,
                pos: self.pos(*cond),
            },
            Expr::Assert { cond, body } => Ir::Assert {
                cond: self.expr(*cond)?,
                body: self.expr(*body)?,
                pos,
            },
            Expr::Unary { op, expr } => {
                let e = self.expr(*expr)?;
                match op {
                    UnOp::Not => Ir::Not(e, pos),
                    UnOp::Neg => Ir::Neg(e, pos),
                }
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.expr(*lhs)?;
                let r = self.expr(*rhs)?;
                match op {
                    BinOp::Eq => Ir::Eq(l, r, pos),
                    BinOp::Ne => Ir::Ne(l, r, pos),
                    BinOp::Lt => Ir::Lt(l, r, pos),
                    // parser.y desugars `a > b` to `__lessThan b a` and
                    // `a <= b` to `!(__lessThan b a)`, so the right operand
                    // is evaluated first
                    BinOp::Gt => Ir::Lt(r, l, pos),
                    BinOp::Le => Ir::Ge(r, l, pos),
                    BinOp::Ge => Ir::Ge(l, r, pos),
                    BinOp::And => Ir::And(l, r, pos),
                    BinOp::Or => Ir::Or(l, r, pos),
                    BinOp::Impl => Ir::Impl(l, r, pos),
                    BinOp::Update => Ir::Update(l, r, pos),
                    BinOp::Concat => Ir::ConcatLists(l, r, pos),
                    BinOp::Add => Ir::Concat {
                        force_string: false,
                        parts: self.alloc_refs(vec![l, r]),
                        pos,
                    },
                    BinOp::Sub => Ir::Sub(l, r, pos),
                    BinOp::Mul => Ir::Mul(l, r, pos),
                    BinOp::Div => Ir::Div(l, r, pos),
                }
            }
        })
    }

    fn lexical_lookup(&self, sym: Sym) -> Option<(u32, u32)> {
        for (d, scope) in self.scopes.iter().rev().enumerate() {
            if let Scope::Lexical { names, .. } = scope
                && let Some(&idx) = names.get(&sym)
            {
                return Some((d as u32, idx));
            }
        }
        None
    }

    /// `builtins.x` where `builtins` is the global: fold to the constant.
    fn fold_builtins_select(
        &mut self,
        subject: ExprId,
        path: &[AttrElem],
        strict: bool,
    ) -> Option<Ir> {
        let Expr::Var(s) = self.ast.expr(subject) else {
            return None;
        };
        if *s != self.sym_builtins || self.lexical_lookup(*s).is_some() {
            return None;
        }
        if self.scopes.iter().any(|s| matches!(s, Scope::With)) {
            return None;
        }
        let b = self.globals.builtins?;
        let AttrElem::Static(first) = path.first()? else {
            return None;
        };
        let attr = b.get(*first)?;
        let w = attr.cell.get();
        if w.is_thunk() {
            return None;
        }
        if path.len() == 1 {
            return Some(Ir::Const(w));
        }
        if !strict {
            return None;
        }
        let rest = self.attr_path(&path[1..]).ok()?;
        let subject = self.alloc(Ir::Const(w));
        let pos = self.pos(subject_pos_hack(path));
        Some(self.mk_select(subject, rest, None, pos))
    }

    /// One static element stays inline as `Select1`; anything else is boxed.
    /// A path that names `revCount` is a `SelectCount`.
    fn mk_select(
        &mut self,
        subject: IrRef,
        path: &'static [AttrElemIr],
        default: Option<IrRef>,
        pos: Pos,
    ) -> Ir {
        let count = self.sym_rev_count;
        let counts = |e: &AttrElemIr| matches!(e, AttrElemIr::Static(s) if *s == count);
        if let [AttrElemIr::Static(sym)] = path
            && *sym != count
        {
            return Ir::Select1 {
                subject,
                sym: *sym,
                default,
                pos,
            };
        }
        let select = self.alloc(SelectIr {
            subject,
            path,
            default,
            pos,
        });
        match path.iter().any(counts) {
            true => Ir::SelectCount(select),
            false => Ir::Select(select),
        }
    }

    /// `inherit (subject) sym`.
    fn inherit_from(&mut self, subject: IrRef, sym: Sym, pos: Pos) -> IrRef {
        let ir = if sym == self.sym_rev_count {
            let path = self.alloc_elems(vec![AttrElemIr::Static(sym)]);
            self.mk_select(subject, path, None, pos)
        } else {
            Ir::Select1 {
                subject,
                sym,
                default: None,
                pos,
            }
        };
        self.alloc(ir)
    }

    fn attr_path(&mut self, path: &[AttrElem]) -> CResult<&'static [AttrElemIr]> {
        let mut v = Vec::with_capacity(path.len());
        for e in path {
            v.push(match e {
                AttrElem::Static(s) => AttrElemIr::Static(*s),
                AttrElem::Dynamic(id) => {
                    // A dynamic element that is a constant string is static.
                    let ast: &'a Ast = self.ast;
                    if let Expr::Str(parts) = ast.expr(*id)
                        && let Some(lit) = ast.single_lit(*parts)
                    {
                        AttrElemIr::Static(self.interner.intern(lit))
                    } else {
                        AttrElemIr::Dynamic(self.expr(*id)?)
                    }
                }
            });
        }
        let refs: Vec<&'static AttrElemIr> = Vec::new();
        drop(refs);
        Ok(self.alloc_elems(v))
    }

    fn alloc_elems(&mut self, v: Vec<AttrElemIr>) -> &'static [AttrElemIr] {
        self.arena.alloc_vec(v)
    }

    fn alloc_binds(&mut self, v: Vec<BindIr>) -> &'static [BindIr] {
        self.arena.alloc_vec(v)
    }

    fn alloc_dyn(&mut self, v: Vec<DynBindIr>) -> &'static [DynBindIr] {
        self.arena.alloc_vec(v)
    }

    fn alloc_formals(&mut self, v: Vec<FormalIr>) -> &'static [FormalIr] {
        self.arena.alloc_vec(v)
    }

    fn str_part(&mut self, p: &StrPart) -> CResult<&'static Ir> {
        Ok(match p {
            StrPart::Lit(l) => {
                let w = self.arena.mk_string(self.ast.bytes(*l));
                self.alloc(Ir::Const(w))
            }
            StrPart::Interp(e) => self.expr(*e)?,
        })
    }

    fn string(&mut self, parts: &[StrPart], pos: Pos) -> CResult<Ir> {
        let ast: &'a Ast = self.ast;
        if let [StrPart::Lit(l)] = parts {
            return Ok(Ir::Const(self.arena.mk_string(ast.bytes(*l))));
        }
        if parts.iter().all(|p| matches!(p, StrPart::Lit(_))) {
            let mut bytes = Vec::new();
            for p in parts {
                if let StrPart::Lit(l) = p {
                    bytes.extend_from_slice(ast.bytes(*l));
                }
            }
            return Ok(Ir::Const(self.arena.mk_string(&bytes)));
        }
        let mut v = Vec::with_capacity(parts.len());
        for p in parts {
            v.push(self.str_part(p)?);
        }
        Ok(Ir::Concat {
            force_string: true,
            parts: self.alloc_refs(v),
            pos,
        })
    }

    fn resolve_path_literal(&self, kind: &PathKind, text: &[u8]) -> Vec<u8> {
        match kind {
            PathKind::Absolute => canon_path(text),
            PathKind::Relative => {
                let mut p = self.base_dir.clone();
                p.push(b'/');
                p.extend_from_slice(text);
                canon_path(&p)
            }
            // parser.y's HPATH rule joins `$HOME` and the text and does not
            // call absPath, so `~/a/../b` keeps its `..`
            PathKind::Home => {
                self.used_home.set(true);
                let mut p = self.home.clone().unwrap_or_else(|| b"/".to_vec());
                p.extend_from_slice(text);
                p
            }
            // `<x>` is a lookup, not a literal: `path` returns before this.
            PathKind::Search => unreachable!("a search path has no literal to resolve"),
        }
    }

    /// parser.y `path_start`: the `lint-absolute-path-literals` and
    /// `lint-short-path-literals` diagnostics.
    fn lint_path_literal(&mut self, kind: &PathKind, lit: &[u8], pos: Pos) -> CResult<()> {
        if !self.lint_on(LintId::AbsolutePathLiteral) && !self.lint_on(LintId::ShortPathLiteral) {
            return Ok(());
        }
        match lint::path::path_literal(*kind, lit) {
            Some((id, msg)) => self.lint(id, pos, || msg),
            None => Ok(()),
        }
    }

    fn path(&mut self, kind: PathKind, parts: &[StrPart], pos: Pos) -> CResult<Ir> {
        let ast: &'a Ast = self.ast;
        let kind = &kind;
        if *kind == PathKind::Search {
            let name = self.string(parts, pos)?;
            let name = self.alloc(name);
            // `<x>` is `__findFile __nixPath "x"`, so a local `__nixPath`
            // or `__findFile` takes part like in Nix
            let ff = self.interner.intern(b"__findFile");
            let np = self.interner.intern(b"__nixPath");
            if self.lexical_lookup(ff).is_some()
                || self.lexical_lookup(np).is_some()
                || self.scopes.iter().any(|s| matches!(s, Scope::With))
            {
                let func = self.var_ir(ff, pos)?;
                let path = self.var_ir(np, pos)?;
                return Ok(Ir::Apply {
                    func,
                    args: self.alloc_refs(vec![path, name]),
                    pos,
                });
            }
            return Ok(Ir::SearchPath { name, pos });
        }
        let all_lit = parts.iter().all(|p| matches!(p, StrPart::Lit(_)));
        let first_lit: Vec<u8> = match parts.first() {
            Some(StrPart::Lit(l)) => ast.bytes(*l).to_vec(),
            _ => Vec::new(),
        };
        self.lint_path_literal(kind, &first_lit, pos)?;
        let trailing_slash = first_lit.ends_with(b"/") && !all_lit;
        let mut resolved = self.resolve_path_literal(kind, &first_lit);
        if trailing_slash && !resolved.ends_with(b"/") {
            resolved.push(b'/');
        }
        let first = self.arena.mk_path(&resolved);
        if all_lit {
            return Ok(Ir::Const(first));
        }
        let mut v = Vec::with_capacity(parts.len());
        v.push(self.alloc(Ir::Const(first)));
        let rest = if matches!(parts.first(), Some(StrPart::Lit(_))) {
            &parts[1..]
        } else {
            parts
        };
        for p in rest {
            v.push(self.str_part(p)?);
        }
        Ok(Ir::Concat {
            force_string: false,
            parts: self.alloc_refs(v),
            pos,
        })
    }

    fn merge_binds(&mut self, rec: bool, binds: &[Bind], pos: Pos) -> CResult<Merged> {
        let mut m = Merged::new(rec, pos);
        for b in binds {
            self.add_bind(&mut m, b)?;
        }
        Ok(m)
    }

    fn bind_pos(&self, b: &Bind) -> Pos {
        Pos(self.base + b.span.start)
    }

    fn add_bind(&mut self, m: &mut Merged, b: &Bind) -> CResult<()> {
        let ast: &'a Ast = self.ast;
        let pos = self.bind_pos(b);
        match &b.kind {
            BindKind::Value { path, value } => {
                self.add_path(m, ast.elems(*path), Def::Expr(*value), pos)
            }
            BindKind::Inherit { from: None, names } => {
                for (name, span) in ast.names(*names) {
                    let p = Pos(self.base + span.start);
                    self.add_static(m, *name, Def::Inherit(*name), p)?;
                }
                Ok(())
            }
            BindKind::Inherit {
                from: Some(e),
                names,
            } => {
                let idx = m.from_exprs.len();
                m.from_exprs.push(*e);
                for (name, span) in ast.names(*names) {
                    let p = Pos(self.base + span.start);
                    self.add_static(m, *name, Def::InheritFrom(idx, *name), p)?;
                }
                Ok(())
            }
        }
    }

    fn add_path(&mut self, m: &mut Merged, path: &[AttrElem], def: Def, pos: Pos) -> CResult<()> {
        let (last, prefix) = path.split_last().expect("attrpath has an element");
        let mut cur = m;
        for elem in prefix {
            match self.attr_name(elem) {
                Name::Static(sym) => {
                    let idx = match cur.index.get(&sym) {
                        Some(&i) => {
                            self.open_nested(cur, i, pos)?;
                            i
                        }
                        None => {
                            let i = cur.statics.len();
                            cur.statics.push((
                                sym,
                                Def::Nested(Box::new(Merged::new(false, pos))),
                                pos,
                            ));
                            cur.index.insert(sym, i);
                            i
                        }
                    };
                    cur = cur.nested_at(idx);
                }
                Name::Dynamic(e) => {
                    cur.dynamics
                        .push((e, Def::Nested(Box::new(Merged::new(false, pos))), pos));
                    let last = cur.dynamics.last_mut().expect("just pushed");
                    let Def::Nested(n) = &mut last.1 else {
                        unreachable!("the dynamic node just pushed is a Def::Nested")
                    };
                    cur = n;
                }
            }
        }
        match self.attr_name(last) {
            Name::Static(sym) => self.add_static(cur, sym, def, pos),
            Name::Dynamic(e) => {
                cur.dynamics.push((e, def, pos));
                Ok(())
            }
        }
    }

    fn attr_name(&mut self, e: &AttrElem) -> Name {
        let ast: &'a Ast = self.ast;
        match e {
            AttrElem::Static(s) => Name::Static(*s),
            AttrElem::Dynamic(id) => {
                if let Expr::Str(parts) = ast.expr(*id)
                    && let Some(lit) = ast.single_lit(*parts)
                {
                    Name::Static(self.interner.intern(lit))
                } else {
                    Name::Dynamic(*id)
                }
            }
        }
    }

    /// Turn an `a = { ... };` literal into a mergeable nested node.
    fn open_nested(&mut self, m: &mut Merged, idx: usize, pos: Pos) -> CResult<()> {
        let sym = m.statics[idx].0;
        match &m.statics[idx].1 {
            Def::Nested(_) => Ok(()),
            Def::Expr(e) => {
                let ast: &'a Ast = self.ast;
                if let Expr::Attrs { rec, binds } = ast.expr(*e) {
                    let nested = self.merge_binds(*rec, ast.binds(*binds), self.pos(*e))?;
                    m.statics[idx].1 = Def::Nested(Box::new(nested));
                    Ok(())
                } else {
                    self.dup_attr(sym, pos, m.statics[idx].2)
                }
            }
            _ => self.dup_attr(sym, pos, m.statics[idx].2),
        }
    }

    fn dup_attr<T>(&self, sym: Sym, pos: Pos, _prev: Pos) -> CResult<T> {
        err(
            ErrorKind::Eval,
            format!(
                "attribute '{}' already defined",
                self.interner.name_str(sym)
            ),
            pos,
        )
    }

    fn add_static(&mut self, m: &mut Merged, sym: Sym, def: Def, pos: Pos) -> CResult<()> {
        if let Some(&i) = m.index.get(&sym) {
            // Both sides attrs literals: merge (Nix discards the inner `rec`).
            let ast: &'a Ast = self.ast;
            let new_binds: Option<&'a [Bind]> = match &def {
                Def::Expr(e) => match ast.expr(*e) {
                    Expr::Attrs { binds, .. } => Some(ast.binds(*binds)),
                    _ => None,
                },
                Def::Nested(_) => Some(&[]),
                _ => None,
            };
            let Some(new_binds) = new_binds else {
                return self.dup_attr(sym, pos, m.statics[i].2);
            };
            self.open_nested(m, i, pos)?;
            let target = m.nested_at(i);
            if let Def::Nested(src) = def {
                let src = *src;
                for (s, d, p) in src.statics {
                    Self::add_static_def(self, target, s, d, p)?;
                }
                target.dynamics.extend(src.dynamics);
            } else {
                for b in new_binds {
                    self.add_bind(target, b)?;
                }
            }
            return Ok(());
        }
        let i = m.statics.len();
        m.statics.push((sym, def, pos));
        m.index.insert(sym, i);
        Ok(())
    }

    fn add_static_def(
        this: &mut Self,
        m: &mut Merged,
        sym: Sym,
        def: Def,
        pos: Pos,
    ) -> CResult<()> {
        this.add_static(m, sym, def, pos)
    }

    /// Plain (non-rec) attrs and nested attrpath sets.
    fn lower_attrs(&mut self, m: Merged) -> CResult<&'static AttrsIr> {
        if m.rec {
            let pos = m.pos;
            let mut m = m;
            let dynamics = mem::take(&mut m.dynamics);
            let (binds, from) = self.lower_rec_binds(m, None)?;
            // dynamics of rec sets are evaluated in the rec env: compile them
            // while its scope is still pushed
            let dynamic = self.lower_dynamics(dynamics)?;
            let overrides = binds
                .iter()
                .position(|b| b.sym == self.sym_overrides)
                .map(|i| i as u32);
            self.scopes.pop();
            return Ok(self.alloc(AttrsIr {
                rec: true,
                binds,
                from,
                dynamic,
                pos,
                overrides,
            }));
        }
        let Merged {
            statics,
            dynamics,
            from_exprs,
            pos,
            ..
        } = m;
        let mut binds: Vec<BindIr> = Vec::with_capacity(statics.len());
        let mut deferred_from: Vec<(usize, Sym, Pos, usize)> = Vec::new();
        for (sym, def, bpos) in statics {
            match def {
                Def::Expr(e) => {
                    let value = self.value_expr(e, Some(sym))?;
                    binds.push(BindIr {
                        sym,
                        value,
                        pos: bpos,
                        scope: BindScope::Own,
                    });
                }
                Def::Nested(n) => {
                    let a = self.lower_attrs(*n)?;
                    let value = self.alloc(Ir::Attrs(a));
                    binds.push(BindIr {
                        sym,
                        value,
                        pos: bpos,
                        scope: BindScope::Own,
                    });
                }
                Def::Inherit(var) => {
                    let value = self.var_ir(var, bpos)?;
                    binds.push(BindIr {
                        sym,
                        value,
                        pos: bpos,
                        scope: BindScope::Own,
                    });
                }
                Def::InheritFrom(fi, name) => {
                    deferred_from.push((fi, name, bpos, binds.len()));
                    // placeholder, patched below
                    binds.push(BindIr {
                        sym,
                        value: self.alloc(Ir::Const(Word::NULL)),
                        pos: bpos,
                        scope: BindScope::From,
                    });
                }
            }
        }
        let mut from: Vec<IrRef> = Vec::with_capacity(from_exprs.len());
        for e in &from_exprs {
            from.push(self.expr(*e)?);
        }
        if !deferred_from.is_empty() {
            self.scopes.push(Scope::frame(FxHashMap::default()));
            // the from-env has `from.len()` anonymous slots at depth 0
            for (fi, name, bpos, bi) in deferred_from {
                let subject = self.alloc(Ir::Local {
                    depth: 0,
                    idx: fi as u32,
                });
                binds[bi].value = self.inherit_from(subject, name, bpos);
            }
            self.scopes.pop();
        }
        let dynamic = self.lower_dynamics(dynamics)?;
        binds.sort_by_key(|b| b.sym);
        let binds = self.alloc_binds(binds);
        let from = self.alloc_refs(from);
        Ok(self.alloc(AttrsIr {
            rec: false,
            binds,
            from,
            dynamic,
            pos,
            overrides: None,
        }))
    }

    fn lower_dynamics(
        &mut self,
        dynamics: Vec<(ExprId, Def, Pos)>,
    ) -> CResult<&'static [DynBindIr]> {
        let mut dyns = Vec::with_capacity(dynamics.len());
        for (name_e, def, dpos) in dynamics {
            let name = self.expr(name_e)?;
            let value = match def {
                Def::Expr(e) => self.expr(e)?,
                Def::Nested(n) => {
                    let a = self.lower_attrs(*n)?;
                    self.alloc(Ir::Attrs(a))
                }
                _ => unreachable!("inherit is never dynamic"),
            };
            dyns.push(DynBindIr {
                name,
                value,
                pos: dpos,
            });
        }
        Ok(self.alloc_dyn(dyns))
    }

    /// Recursive bindings (`rec { }` and `let`). Pushes the new scope and
    /// leaves it on the stack so the caller can compile the body; the caller
    /// pops it.
    /// `let`; with `base`, its slots extend the frame of the lambda it is the
    /// body of from slot `base` on.
    fn let_in(&mut self, binds: Range, body: ExprId, pos: Pos, base: Option<u32>) -> CResult<Ir> {
        let ast: &'a Ast = self.ast;
        let merged = self.merge_binds(true, ast.binds(binds), pos)?;
        if !merged.dynamics.is_empty() {
            return err(
                ErrorKind::Eval,
                "dynamic attributes not allowed in let".into(),
                pos,
            );
        }
        let (binds, from) = self.lower_rec_binds(merged, base)?;
        let body = self.expr(body)?;
        self.scopes.pop();
        Ok(Ir::Let(self.alloc(LetIr {
            binds,
            from,
            body,
            base: base.unwrap_or(LetIr::OWN_ENV),
        })))
    }

    fn lower_rec_binds(
        &mut self,
        m: Merged,
        base: Option<u32>,
    ) -> CResult<(&'static [BindIr], &'static [IrRef])> {
        let Merged {
            mut statics,
            from_exprs,
            ..
        } = m;
        statics.sort_by_key(|s| s.0);
        // `inherit x` values resolve in the enclosing scope, so compile them
        // before pushing the new frame.
        let mut outer_vals: Vec<Option<IrRef>> = Vec::with_capacity(statics.len());
        for (_, def, bpos) in &statics {
            outer_vals.push(match def {
                Def::Inherit(var) => Some(self.var_ir(*var, *bpos)?),
                _ => None,
            });
        }
        let at = base.unwrap_or(0);
        let mut names = FxHashMap::with_capacity_and_hasher(statics.len(), Default::default());
        for (i, (sym, _, _)) in statics.iter().enumerate() {
            names.insert(*sym, at + i as u32);
        }
        self.scopes.push(Scope::Lexical {
            names,
            shares: base.is_some(),
        });
        let nbinds = statics.len();
        let mut binds = Vec::with_capacity(nbinds);
        for (i, (sym, def, bpos)) in statics.into_iter().enumerate() {
            let (value, scope) = match def {
                Def::Expr(e) => (self.value_expr(e, Some(sym))?, BindScope::Own),
                Def::Nested(n) => {
                    let a = self.lower_attrs(*n)?;
                    (self.alloc(Ir::Attrs(a)), BindScope::Own)
                }
                Def::Inherit(_) => (outer_vals[i].unwrap(), BindScope::Outer),
                Def::InheritFrom(fi, name) => {
                    let subject = self.alloc(Ir::Local {
                        depth: 0,
                        idx: at + (nbinds + fi) as u32,
                    });
                    (self.inherit_from(subject, name, bpos), BindScope::Own)
                }
            };
            binds.push(BindIr {
                sym,
                value,
                pos: bpos,
                scope,
            });
        }
        let mut from = Vec::with_capacity(from_exprs.len());
        for e in from_exprs {
            from.push(self.expr(e)?);
        }
        Ok((self.alloc_binds(binds), self.alloc_refs(from)))
    }

    fn var_ir(&mut self, var: Sym, pos: Pos) -> CResult<IrRef> {
        if var == self.sym_cur_pos {
            return Ok(self.alloc(Ir::CurPos(pos)));
        }
        match self.resolve(var) {
            Some(Ir::WithVar { sym, withs, .. }) => Ok(self.alloc(Ir::WithVar { sym, withs, pos })),
            Some(ir) => {
                if matches!(ir, Ir::Const(_)) {
                    self.global_ref(self.interner.name(var), pos)?;
                }
                Ok(self.alloc(ir))
            }
            None => err(
                ErrorKind::UndefinedVar,
                format!("undefined variable '{}'", self.interner.name_str(var)),
                pos,
            ),
        }
    }

    /// A binding's value; names lambdas after their binding.
    fn value_expr(&mut self, e: ExprId, name: Option<Sym>) -> CResult<IrRef> {
        let ast: &'a Ast = self.ast;
        if let Expr::Lambda { param, body } = ast.expr(e) {
            let pos = self.pos(e);
            let l = self.lambda(*param, *body, pos, name, None)?;
            return Ok(self.alloc(Ir::Lambda(l)));
        }
        self.expr(e)
    }

    /// Whether `body` may read the variable `sym`. Desugared syntax reads
    /// `__`-names without naming them (`<p>` reads `__findFile`), so those
    /// always count.
    fn named_in(&mut self, sym: Sym, body: ExprId) -> bool {
        if self.interner.name(sym).starts_with(b"__") {
            return true;
        }
        let ast: &'a Ast = self.ast;
        let span = ast.span(body);
        let uses = self.var_uses.get_or_insert_with(|| var_uses(ast));
        uses.get(&sym).is_some_and(|v| {
            let i = v.partition_point(|&p| p < span.start);
            v.get(i).is_some_and(|&p| p < span.end)
        })
    }

    fn lambda(
        &mut self,
        param: Param,
        body: ExprId,
        pos: Pos,
        name: Option<Sym>,
        base: Option<u32>,
    ) -> CResult<&'static LambdaIr> {
        let _ = self.sym_body;
        let ast: &'a Ast = self.ast;
        let at = base.unwrap_or(0);
        match param {
            Param::Ident(sym) => {
                let named = self.named_in(sym, body);
                self.lambdas.push((self.scopes.len(), Cell::new(false)));
                let body = if named {
                    let mut names = FxHashMap::default();
                    names.insert(sym, at);
                    self.scopes.push(Scope::Lexical {
                        names,
                        shares: base.is_some(),
                    });
                    let body = self.lambda_body(body, at + 1)?;
                    self.scopes.pop();
                    body
                } else {
                    self.expr(body)?
                };
                let closed = !self.lambdas.pop().unwrap().1.get();
                Ok(self.alloc(LambdaIr {
                    param: ParamIr::Ident,
                    body,
                    pos,
                    name,
                    arg: Some(sym),
                    closed,
                    no_env: !named,
                    base: if named { at } else { 0 },
                    frame: if named { frame_end(body, at + 1) } else { 0 },
                }))
            }
            Param::Formals {
                bind,
                formals,
                ellipsis,
            } => {
                let formals = ast.formals(formals);
                let mut sorted: Vec<_> = formals.iter().collect();
                sorted.sort_by_key(|f| f.name);
                for w in sorted.windows(2) {
                    if w[0].name == w[1].name {
                        return err(
                            ErrorKind::Eval,
                            format!(
                                "duplicate formal function argument '{}'",
                                self.interner.name_str(w[0].name)
                            ),
                            pos,
                        );
                    }
                }
                let mut m = FxHashMap::default();
                let mut idx = at;
                if let Some(b) = bind {
                    m.insert(b, idx);
                    idx += 1;
                }
                for f in &sorted {
                    if m.insert(f.name, idx).is_some() {
                        return err(
                            ErrorKind::Eval,
                            format!(
                                "duplicate formal function argument '{}'",
                                self.interner.name_str(f.name)
                            ),
                            pos,
                        );
                    }
                    idx += 1;
                }
                self.lambdas.push((self.scopes.len(), Cell::new(false)));
                self.scopes.push(Scope::Lexical {
                    names: m,
                    shares: base.is_some(),
                });
                let mut firs = Vec::with_capacity(sorted.len());
                for f in &sorted {
                    let default = match f.default {
                        Some(d) => Some(self.expr(d)?),
                        None => None,
                    };
                    firs.push(FormalIr {
                        sym: f.name,
                        default,
                        pos: Pos(self.base + f.span.start),
                    });
                }
                let body = self.lambda_body(body, idx)?;
                self.scopes.pop();
                let closed = !self.lambdas.pop().unwrap().1.get();
                let formals = self.alloc_formals(firs);
                Ok(self.alloc(LambdaIr {
                    param: ParamIr::Formals {
                        has_bind: bind.is_some(),
                        formals,
                        ellipsis,
                    },
                    body,
                    pos,
                    name,
                    arg: bind,
                    closed,
                    no_env: false,
                    base: at,
                    frame: frame_end(body, idx),
                }))
            }
        }
    }

    /// The body of a lambda whose frame has `end` slots: a lambda or `let`
    /// there extends that frame instead of opening its own. An empty frame
    /// (`{ }:`) is not extended: `base` 0 marks a lambda with its own.
    /// `cache::De` applies the same rule when it loads the IR.
    fn lambda_body(&mut self, body: ExprId, end: u32) -> CResult<IrRef> {
        if end == 0 {
            return self.expr(body);
        }
        let ast: &'a Ast = self.ast;
        let pos = self.pos(body);
        let ir = match ast.expr(body) {
            Expr::Lambda { param, body } => {
                Ir::Lambda(self.lambda(*param, *body, pos, None, Some(end))?)
            }
            Expr::Let { binds, body } => self.let_in(*binds, *body, pos, Some(end))?,
            _ => return self.expr(body),
        };
        Ok(self.alloc(ir))
    }
}

/// Slots of a lambda's env: `end` after its params, or past the bindings
/// of a `let` body that extends the frame.
fn frame_end(body: IrRef, end: u32) -> u32 {
    match body {
        Ir::Let(l) if l.base != LetIr::OWN_ENV => l.base + (l.binds.len() + l.from.len()) as u32,
        _ => end,
    }
}

fn subject_pos_hack(_path: &[AttrElem]) -> ExprId {
    ExprId(0)
}

impl Ir {
    fn with_withs(self, withs: &'static [u32]) -> Ir {
        match self {
            Ir::WithVar { sym, pos, .. } => Ir::WithVar { sym, withs, pos },
            other => other,
        }
    }
}

/// Lexical path canonicalisation like Nix's `CanonPath`: collapse `//`,
/// drop `.`, resolve `..`, strip a trailing slash (except root).
pub fn canon_path(p: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(p.len());
    let mut comps: Vec<&[u8]> = Vec::new();
    for c in p.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                // Windows keeps a drive or a share under `..`
                if comps.len() > sys::anchor_len(&comps) {
                    comps.pop();
                }
            }
            c => comps.push(c),
        }
    }
    if comps.is_empty() {
        out.push(b'/');
        return out;
    }
    for c in comps {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    out
}

#[cfg(all(test, windows))]
mod winpath_tests {
    use super::canon_path;

    #[test]
    fn canon_path_keeps_the_drive() {
        assert_eq!(canon_path(b"/C:/a/../../.."), b"/C:");
        assert_eq!(
            canon_path(b"/UNC/host/share/a/../../.."),
            b"/UNC/host/share"
        );
    }
}
