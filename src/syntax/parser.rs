//! Recursive-descent parser with precedence climbing for operators. It
//! follows the productions of Nix's `parser.y` one to one, including its
//! oddities: `or` as an application argument, `let { body = ...; }`,
//! duplicate-attribute detection at parse time, and nonassociative
//! comparison operators.
//!
//! Variable-length children are built on per-table scratch stacks and
//! moved into the `Ast` side tables in one block when their parent node
//! is complete, so a nested construct never splits its parent's run.

use super::ast::{
    Ast, AttrElem, BinOp, Bind, BindKind, Expr, ExprId, FileId, Formal, Note, Param, PathExpr,
    PathKind, Range, Span, StrPart, UnOp,
};
use super::lexer::{IndPart, Lexer, PathPart, StrEnd, Tok, Token};
use super::symbol::{FxHashMap, Interner, Sym};
use super::{ParseError, line_col};

use std::str;

use crate::lint::allow;

type R<T> = Result<T, ParseError>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Assoc {
    Left,
    Right,
    None,
}

const BP_NOT: u8 = 7;
const BP_NEG: u8 = 12;

fn binop(kind: Tok) -> Option<(u8, Assoc, Option<BinOp>)> {
    Some(match kind {
        Tok::Impl => (1, Assoc::Right, Some(BinOp::Impl)),
        Tok::OrOr => (2, Assoc::Left, Some(BinOp::Or)),
        Tok::AndAnd => (3, Assoc::Left, Some(BinOp::And)),
        Tok::Eq => (4, Assoc::None, Some(BinOp::Eq)),
        Tok::Ne => (4, Assoc::None, Some(BinOp::Ne)),
        Tok::Lt => (5, Assoc::None, Some(BinOp::Lt)),
        Tok::Gt => (5, Assoc::None, Some(BinOp::Gt)),
        Tok::Le => (5, Assoc::None, Some(BinOp::Le)),
        Tok::Ge => (5, Assoc::None, Some(BinOp::Ge)),
        Tok::Update => (6, Assoc::Right, Some(BinOp::Update)),
        Tok::Plus => (8, Assoc::Left, Some(BinOp::Add)),
        Tok::Minus => (8, Assoc::Left, Some(BinOp::Sub)),
        Tok::Star => (9, Assoc::Left, Some(BinOp::Mul)),
        Tok::Slash => (9, Assoc::Left, Some(BinOp::Div)),
        Tok::Concat => (10, Assoc::Right, Some(BinOp::Concat)),
        Tok::Question => (11, Assoc::None, None),
        _ => return None,
    })
}

fn starts_simple(kind: Tok) -> bool {
    matches!(
        kind,
        Tok::Id
            | Tok::Int
            | Tok::Float
            | Tok::DQuote
            | Tok::IndOpen
            | Tok::Path
            | Tok::HPath
            | Tok::SPath
            | Tok::Uri
            | Tok::LParen
            | Tok::Let
            | Tok::Rec
            | Tok::LBrace
            | Tok::LBracket
    )
}

/// Move `stack[mark..]` to the end of `table` and describe where it went.
fn commit<T: Copy>(table: &mut Vec<T>, stack: &mut Vec<T>, mark: usize) -> Range {
    let start = table.len() as u32;
    table.extend_from_slice(&stack[mark..]);
    stack.truncate(mark);
    Range {
        start,
        len: table.len() as u32 - start,
    }
}

pub struct Parser<'a> {
    src: &'a [u8],
    lx: Lexer<'a>,
    tok: Option<Token>,
    next: Option<Token>,
    last_end: u32,
    interner: &'a mut Interner,
    ast: Ast,
    sc_ids: Vec<ExprId>,
    sc_elems: Vec<AttrElem>,
    sc_parts: Vec<StrPart>,
    sc_binds: Vec<Bind>,
    sc_names: Vec<(Sym, Span)>,
    sc_formals: Vec<Formal>,
    sym_or: Sym,
    sym_body: Sym,
    depth: u32,
}

/// Nix has no limit and reports a stack overflow from its SIGSEGV handler.
/// This parser recurses, so a limit takes the place of the crash. The value
/// is a third of the deepest nesting the 4 GiB evaluator stack survived
/// (150,000 nested attribute sets), so the parse ends before any stage of
/// the pipeline runs out of stack.
const MAX_PARSE_DEPTH: u32 = 50_000;

impl<'a> Parser<'a> {
    pub fn new(src: &'a [u8], interner: &'a mut Interner) -> Self {
        Self::with_ast(src, interner, Ast::default())
    }

    /// Reuse the buffers of a previous file's `Ast` (contents discarded).
    pub fn with_ast(src: &'a [u8], interner: &'a mut Interner, mut ast: Ast) -> Self {
        let sym_or = interner.intern(b"or");
        let sym_body = interner.intern(b"body");
        ast.clear();
        let cap = src.len() / 12 + 8;
        if ast.exprs.capacity() < cap {
            ast.exprs.reserve(cap);
            ast.spans.reserve(cap);
        }
        Parser {
            src,
            lx: Lexer::new(src),
            tok: None,
            next: None,
            last_end: 0,
            interner,
            ast,
            sc_ids: Vec::new(),
            sc_elems: Vec::new(),
            sc_parts: Vec::new(),
            sc_binds: Vec::new(),
            sc_names: Vec::new(),
            sc_formals: Vec::new(),
            sym_or,
            sym_body,
            depth: 0,
        }
    }

    /// One more level of nesting, or Nix's stack-overflow error.
    #[inline]
    fn deeper(&mut self) -> R<()> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            let t = self.peek();
            return Err(self.error_at(
                t.start,
                t.end,
                "stack overflow (possible infinite recursion)".into(),
            ));
        }
        Ok(())
    }

    pub fn parse_file(mut self, file: FileId) -> R<Ast> {
        let root = self.parse_expr()?;
        let t = self.peek();
        if t.kind != Tok::Eof {
            return Err(self.unexpected(t, Some("expecting end of file")));
        }
        self.allows();
        self.ast.file = file;
        self.ast.root = root;
        Ok(self.ast)
    }

    /// The `# iets:` comments the lexer noted: their names go into the
    /// allow table, the words it cannot resolve become `unknown-allow` notes.
    fn allows(&mut self) {
        for a in self.lx.take_allows() {
            let text = &self.src[a.text.start as usize..a.text.end as usize];
            let (mask, bad) = allow::parse_comment(text);
            self.ast.allows.add(a.line.start, a.line.end, mask);
            for (off, msg) in bad {
                let start = self.ast.bytes.len() as u32;
                self.ast.bytes.extend_from_slice(msg.as_bytes());
                let msg = Range {
                    start,
                    len: msg.len() as u32,
                };
                self.ast.notes.push(Note::UnknownAllow {
                    at: a.text.start + off,
                    msg,
                });
            }
        }
    }

    #[inline]
    fn peek(&mut self) -> Token {
        match self.tok {
            Some(t) => t,
            None => {
                let t = match self.next.take() {
                    Some(t) => t,
                    None => self.lx.next_token(),
                };
                self.tok = Some(t);
                t
            }
        }
    }

    fn peek2(&mut self) -> Token {
        self.peek();
        match self.next {
            Some(t) => t,
            None => {
                let t = self.lx.next_token();
                self.next = Some(t);
                t
            }
        }
    }

    /// Third token of lookahead. Only valid right after `peek2`.
    fn peek3(&mut self) -> Token {
        let save = self.lx.pos();
        let t = self.lx.next_token();
        self.lx.set_pos(save);
        t
    }

    #[inline]
    fn bump(&mut self) -> Token {
        let t = self.peek();
        self.tok = None;
        self.last_end = t.end;
        t
    }

    fn expect(&mut self, kind: Tok, expecting: &str) -> R<Token> {
        let t = self.peek();
        if t.kind == kind {
            Ok(self.bump())
        } else {
            Err(self.unexpected(t, Some(expecting)))
        }
    }

    fn unexpected(&self, t: Token, expecting: Option<&str>) -> ParseError {
        let what = t.kind.describe();
        let msg = match expecting {
            Some(e) => format!("syntax error, unexpected {what}, {e}"),
            None => format!("syntax error, unexpected {what}"),
        };
        ParseError {
            span: t.span(),
            msg,
        }
    }

    fn error_at(&self, start: u32, end: u32, msg: String) -> ParseError {
        ParseError {
            span: Span { start, end },
            msg,
        }
    }

    #[inline]
    fn add(&mut self, e: Expr, start: u32, end: u32) -> ExprId {
        let id = ExprId(self.ast.exprs.len() as u32);
        self.ast.exprs.push(e);
        self.ast.spans.push(Span { start, end });
        id
    }

    fn text(&self, t: Token) -> &'a [u8] {
        &self.src[t.start as usize..t.end as usize]
    }

    /// Store `b` in the bytes table.
    fn lit(&mut self, b: &[u8]) -> StrPart {
        let start = self.ast.bytes.len() as u32;
        self.ast.bytes.extend_from_slice(b);
        StrPart::Lit(Range {
            start,
            len: b.len() as u32,
        })
    }

    /// One-part string over `b`.
    fn single_str(&mut self, b: &[u8]) -> Range {
        let mark = self.sc_parts.len();
        let p = self.lit(b);
        self.sc_parts.push(p);
        commit(&mut self.ast.parts, &mut self.sc_parts, mark)
    }

    pub fn parse_expr(&mut self) -> R<ExprId> {
        self.parse_function()
    }

    fn parse_function(&mut self) -> R<ExprId> {
        self.deeper()?;
        let r = self.parse_function_inner();
        self.depth -= 1;
        r
    }

    fn parse_function_inner(&mut self) -> R<ExprId> {
        let t = self.peek();
        match t.kind {
            Tok::Id => {
                let n = self.peek2();
                match n.kind {
                    Tok::Colon => {
                        self.bump();
                        self.bump();
                        let name = self.interner.intern(self.text(t));
                        let body = self.parse_function()?;
                        Ok(self.add(
                            Expr::Lambda {
                                param: Param::Ident(name),
                                body,
                            },
                            t.start,
                            self.last_end,
                        ))
                    }
                    Tok::At => {
                        self.bump();
                        self.bump();
                        let name = self.interner.intern(self.text(t));
                        let (formals, ellipsis) = self.parse_formals()?;
                        self.validate_formals(formals, Some((name, t.span())))?;
                        self.expect(Tok::Colon, "expecting ':'")?;
                        let body = self.parse_function()?;
                        Ok(self.add(
                            Expr::Lambda {
                                param: Param::Formals {
                                    bind: Some(name),
                                    formals,
                                    ellipsis,
                                },
                                body,
                            },
                            t.start,
                            self.last_end,
                        ))
                    }
                    _ => self.parse_if(),
                }
            }
            Tok::LBrace if self.is_formals() => {
                let (formals, ellipsis) = self.parse_formals()?;
                let mut bind = None;
                if self.peek().kind == Tok::At {
                    self.bump();
                    let n = self.expect(Tok::Id, "expecting identifier")?;
                    bind = Some((self.interner.intern(self.text(n)), n.span()));
                }
                self.validate_formals(formals, bind)?;
                self.expect(Tok::Colon, "expecting ':'")?;
                let body = self.parse_function()?;
                Ok(self.add(
                    Expr::Lambda {
                        param: Param::Formals {
                            bind: bind.map(|b| b.0),
                            formals,
                            ellipsis,
                        },
                        body,
                    },
                    t.start,
                    self.last_end,
                ))
            }
            Tok::Assert => {
                self.bump();
                let cond = self.parse_expr()?;
                self.expect(Tok::Semi, "expecting ';'")?;
                let body = self.parse_function()?;
                Ok(self.add(Expr::Assert { cond, body }, t.start, self.last_end))
            }
            Tok::With => {
                self.bump();
                let scope = self.parse_expr()?;
                self.expect(Tok::Semi, "expecting ';'")?;
                let body = self.parse_function()?;
                Ok(self.add(Expr::With { scope, body }, t.start, self.last_end))
            }
            Tok::Let if self.peek2().kind != Tok::LBrace => {
                self.bump();
                let binds = self.parse_binds(Tok::In)?;
                self.expect(Tok::In, "expecting 'in'")?;
                for b in self.ast.binds(binds) {
                    if let BindKind::Value { path, .. } = b.kind
                        && let Some(AttrElem::Dynamic(_)) = self.ast.elems(path).first()
                    {
                        return Err(self.error_at(
                            b.span.start,
                            b.span.end,
                            "dynamic attributes not allowed in let".into(),
                        ));
                    }
                }
                let body = self.parse_function()?;
                Ok(self.add(Expr::Let { binds, body }, t.start, self.last_end))
            }
            _ => self.parse_if(),
        }
    }

    /// Decide between a formals pattern and an attrset after `{`.
    fn is_formals(&mut self) -> bool {
        let n = self.peek2();
        match n.kind {
            Tok::Ellipsis => true,
            Tok::RBrace => matches!(self.peek3().kind, Tok::Colon | Tok::At),
            Tok::Id => matches!(self.peek3().kind, Tok::Comma | Tok::Question | Tok::RBrace),
            _ => false,
        }
    }

    fn parse_formals(&mut self) -> R<(Range, bool)> {
        self.expect(Tok::LBrace, "expecting '{'")?;
        let mark = self.sc_formals.len();
        let mut ellipsis = false;
        loop {
            let t = self.peek();
            match t.kind {
                Tok::RBrace => {
                    self.bump();
                    break;
                }
                Tok::Ellipsis => {
                    self.bump();
                    ellipsis = true;
                    self.expect(Tok::RBrace, "expecting '}'")?;
                    break;
                }
                Tok::Id => {
                    self.bump();
                    let name = self.interner.intern(self.text(t));
                    let mut default = None;
                    if self.peek().kind == Tok::Question {
                        self.bump();
                        default = Some(self.parse_expr()?);
                    }
                    self.sc_formals.push(Formal {
                        name,
                        default,
                        span: Span {
                            start: t.start,
                            end: self.last_end,
                        },
                    });
                    let n = self.peek();
                    match n.kind {
                        Tok::Comma => {
                            self.bump();
                        }
                        Tok::RBrace => {
                            self.bump();
                            break;
                        }
                        _ => return Err(self.unexpected(n, Some("expecting '}' or ','"))),
                    }
                }
                _ => return Err(self.unexpected(t, Some("expecting identifier or '...' or '}'"))),
            }
        }
        let r = commit(&mut self.ast.formals, &mut self.sc_formals, mark);
        Ok((r, ellipsis))
    }

    fn validate_formals(&self, formals: Range, bind: Option<(Sym, Span)>) -> R<()> {
        let formals = self.ast.formals(formals);
        if formals.len() > 1 {
            let mut seen: Vec<(Sym, u32)> =
                formals.iter().map(|f| (f.name, f.span.start)).collect();
            seen.sort_unstable();
            for w in seen.windows(2) {
                if w[0].0 == w[1].0 {
                    let f = formals.iter().find(|f| f.span.start == w[1].1).unwrap();
                    return Err(self.error_at(
                        f.span.start,
                        f.span.end,
                        format!(
                            "duplicate formal function argument '{}'",
                            self.interner.name_str(f.name)
                        ),
                    ));
                }
            }
        }
        if let Some((b, span)) = bind
            && formals.iter().any(|f| f.name == b)
        {
            return Err(self.error_at(
                span.start,
                span.end,
                format!(
                    "duplicate formal function argument '{}'",
                    self.interner.name_str(b)
                ),
            ));
        }
        Ok(())
    }

    fn parse_if(&mut self) -> R<ExprId> {
        let t = self.peek();
        if t.kind == Tok::If {
            self.bump();
            let cond = self.parse_expr()?;
            self.expect(Tok::Then, "expecting 'then'")?;
            let then = self.parse_expr()?;
            self.expect(Tok::Else, "expecting 'else'")?;
            let else_ = self.parse_expr()?;
            return Ok(self.add(Expr::If { cond, then, else_ }, t.start, self.last_end));
        }
        self.parse_pipe()
    }

    fn parse_pipe(&mut self) -> R<ExprId> {
        let lhs = self.parse_op(0)?;
        match self.peek().kind {
            Tok::PipeInto => {
                let mut acc = lhs;
                while self.peek().kind == Tok::PipeInto {
                    self.bump();
                    let f = self.parse_op(0)?;
                    acc = self.make_call(f, acc);
                }
                let t = self.peek();
                if t.kind == Tok::PipeFrom {
                    return Err(self.unexpected(t, None));
                }
                Ok(acc)
            }
            Tok::PipeFrom => {
                let mut chain = vec![lhs];
                while self.peek().kind == Tok::PipeFrom {
                    self.bump();
                    chain.push(self.parse_op(0)?);
                }
                let t = self.peek();
                if t.kind == Tok::PipeInto {
                    return Err(self.unexpected(t, None));
                }
                let mut acc = chain.pop().unwrap();
                while let Some(f) = chain.pop() {
                    acc = self.make_call(f, acc);
                }
                Ok(acc)
            }
            _ => Ok(lhs),
        }
    }

    /// `f a` becomes one `Apply`; applying an `Apply` again extends it,
    /// exactly like Nix's `makeCall`. The extended argument run is copied
    /// to the end of the table; the old run is left behind.
    fn make_call(&mut self, func: ExprId, arg: ExprId) -> ExprId {
        let arg_span = self.ast.spans[arg.0 as usize];
        if let Expr::Apply { args, .. } = self.ast.exprs[func.0 as usize] {
            let mark = self.sc_ids.len();
            self.sc_ids.extend_from_slice(self.ast.ids(args));
            self.sc_ids.push(arg);
            let args = commit(&mut self.ast.ids, &mut self.sc_ids, mark);
            let Expr::Apply { args: a, .. } = &mut self.ast.exprs[func.0 as usize] else {
                unreachable!()
            };
            *a = args;
            let s = &mut self.ast.spans[func.0 as usize];
            s.start = s.start.min(arg_span.start);
            s.end = s.end.max(arg_span.end);
            return func;
        }
        let fs = self.ast.spans[func.0 as usize];
        let mark = self.sc_ids.len();
        self.sc_ids.push(arg);
        let args = commit(&mut self.ast.ids, &mut self.sc_ids, mark);
        self.add(
            Expr::Apply { func, args },
            fs.start.min(arg_span.start),
            fs.end.max(arg_span.end),
        )
    }

    fn parse_op(&mut self, min_bp: u8) -> R<ExprId> {
        let t = self.peek();
        let mut lhs = match t.kind {
            Tok::Bang => {
                self.bump();
                self.deeper()?;
                let e = self.parse_op(BP_NOT + 1)?;
                self.depth -= 1;
                self.add(
                    Expr::Unary {
                        op: UnOp::Not,
                        expr: e,
                    },
                    t.start,
                    self.last_end,
                )
            }
            Tok::Minus => {
                self.bump();
                self.deeper()?;
                let e = self.parse_op(BP_NEG + 1)?;
                self.depth -= 1;
                match self.ast.exprs[e.0 as usize] {
                    Expr::Int(i) => {
                        self.ast.exprs[e.0 as usize] = Expr::Int(0i64.wrapping_sub(i));
                        self.ast.spans[e.0 as usize].start = t.start;
                        e
                    }
                    Expr::Float(f) => {
                        self.ast.exprs[e.0 as usize] = Expr::Float(0.0 - f);
                        self.ast.spans[e.0 as usize].start = t.start;
                        e
                    }
                    _ => self.add(
                        Expr::Unary {
                            op: UnOp::Neg,
                            expr: e,
                        },
                        t.start,
                        self.last_end,
                    ),
                }
            }
            _ => self.parse_app()?,
        };
        loop {
            let op = self.peek();
            let Some((bp, assoc, bin)) = binop(op.kind) else {
                break;
            };
            if bp < min_bp {
                break;
            }
            self.bump();
            let start = self.ast.spans[lhs.0 as usize].start;
            lhs = match bin {
                None => {
                    let path = self.parse_attrpath()?;
                    self.add(Expr::HasAttr { subject: lhs, path }, start, self.last_end)
                }
                Some(op) => {
                    let rhs = self.parse_op(if assoc == Assoc::Right { bp } else { bp + 1 })?;
                    self.add(Expr::Binary { op, lhs, rhs }, start, self.last_end)
                }
            };
            if assoc == Assoc::None {
                let n = self.peek();
                if let Some((bp2, _, _)) = binop(n.kind)
                    && bp2 == bp
                {
                    return Err(self.unexpected(n, None));
                }
            }
        }
        Ok(lhs)
    }

    /// `f a b c`: the arguments are collected first so they land in one
    /// run; a `func` that already is an `Apply` (the `or` argument hack)
    /// is extended like `make_call` does.
    fn parse_app(&mut self) -> R<ExprId> {
        let f = self.parse_select()?;
        if !starts_simple(self.peek().kind) {
            return Ok(f);
        }
        let mark = self.sc_ids.len();
        let existing = match self.ast.exprs[f.0 as usize] {
            Expr::Apply { args, .. } => {
                self.sc_ids.extend_from_slice(self.ast.ids(args));
                true
            }
            _ => false,
        };
        while starts_simple(self.peek().kind) {
            let a = self.parse_select()?;
            self.sc_ids.push(a);
        }
        let args = commit(&mut self.ast.ids, &mut self.sc_ids, mark);
        let last = self.ast.ids(args).last().copied().unwrap();
        let end = self.ast.spans[last.0 as usize].end;
        if existing {
            let Expr::Apply { args: a, .. } = &mut self.ast.exprs[f.0 as usize] else {
                unreachable!()
            };
            *a = args;
            let s = &mut self.ast.spans[f.0 as usize];
            s.end = s.end.max(end);
            return Ok(f);
        }
        let fs = self.ast.spans[f.0 as usize];
        Ok(self.add(Expr::Apply { func: f, args }, fs.start, fs.end.max(end)))
    }

    fn parse_select(&mut self) -> R<ExprId> {
        let start = self.peek().start;
        let subject = self.parse_simple()?;
        match self.peek().kind {
            Tok::Dot => {
                self.bump();
                let path = self.parse_attrpath()?;
                let mut default = None;
                if self.peek().kind == Tok::Or {
                    self.bump();
                    default = Some(self.parse_select()?);
                }
                Ok(self.add(
                    Expr::Select {
                        subject,
                        path,
                        default,
                    },
                    start,
                    self.last_end,
                ))
            }
            Tok::Or => {
                let t = self.bump();
                let or = self.add(Expr::Var(self.sym_or), t.start, t.end);
                let mark = self.sc_ids.len();
                self.sc_ids.push(or);
                let args = commit(&mut self.ast.ids, &mut self.sc_ids, mark);
                Ok(self.add(
                    Expr::Apply {
                        func: subject,
                        args,
                    },
                    start,
                    t.end,
                ))
            }
            _ => Ok(subject),
        }
    }

    fn parse_simple(&mut self) -> R<ExprId> {
        self.deeper()?;
        let r = self.parse_simple_inner();
        self.depth -= 1;
        r
    }

    fn parse_simple_inner(&mut self) -> R<ExprId> {
        let t = self.peek();
        match t.kind {
            Tok::Id => {
                self.bump();
                let s = self.interner.intern(self.text(t));
                Ok(self.add(Expr::Var(s), t.start, t.end))
            }
            Tok::Int => {
                self.bump();
                let text = self.text(t);
                let Some(v) = parse_int(text) else {
                    return Err(self.error_at(
                        t.start,
                        t.end,
                        format!("invalid integer '{}'", String::from_utf8_lossy(text)),
                    ));
                };
                Ok(self.add(Expr::Int(v), t.start, t.end))
            }
            Tok::Float => {
                self.bump();
                let text = str::from_utf8(self.text(t)).unwrap();
                let v: f64 = text.parse().unwrap_or(f64::INFINITY);
                // lexer.l reports every strtod ERANGE, so an overflow, a
                // subnormal and an underflow to zero are all invalid
                let range = !v.is_finite()
                    || (v != 0.0 && v.abs() < f64::MIN_POSITIVE)
                    || (v == 0.0
                        && text
                            .split(['e', 'E'])
                            .next()
                            .is_some_and(|m| m.bytes().any(|b| b.is_ascii_digit() && b != b'0')));
                if range {
                    return Err(self.error_at(t.start, t.end, format!("invalid float '{text}'")));
                }
                Ok(self.add(Expr::Float(v), t.start, t.end))
            }
            Tok::DQuote => {
                self.bump();
                let parts = self.parse_string_parts()?;
                Ok(self.add(Expr::Str(parts), t.start, self.last_end))
            }
            Tok::IndOpen => {
                self.bump();
                let parts = self.parse_ind_string()?;
                Ok(self.add(Expr::Str(parts), t.start, self.last_end))
            }
            Tok::Path | Tok::HPath => {
                self.bump();
                self.parse_path(t)
            }
            Tok::SPath => {
                self.bump();
                let text = self.text(t);
                let parts = self.single_str(&text[1..text.len() - 1]);
                Ok(self.add(
                    Expr::Path(PathExpr {
                        kind: PathKind::Search,
                        parts,
                    }),
                    t.start,
                    t.end,
                ))
            }
            Tok::Uri => {
                self.bump();
                let text = self.text(t);
                let parts = self.single_str(text);
                let id = self.add(Expr::Str(parts), t.start, t.end);
                self.ast.notes.push(Note::Url(id));
                Ok(id)
            }
            Tok::LParen => {
                self.bump();
                let e = self.parse_expr()?;
                self.expect(Tok::RParen, "expecting ')'")?;
                Ok(e)
            }
            Tok::Let => {
                self.bump();
                self.expect(Tok::LBrace, "expecting '{'")?;
                let binds = self.parse_binds(Tok::RBrace)?;
                self.expect(Tok::RBrace, "expecting '}'")?;
                let attrs = self.add(Expr::Attrs { rec: true, binds }, t.start, self.last_end);
                let mark = self.sc_elems.len();
                self.sc_elems.push(AttrElem::Static(self.sym_body));
                let path = commit(&mut self.ast.elems, &mut self.sc_elems, mark);
                Ok(self.add(
                    Expr::Select {
                        subject: attrs,
                        path,
                        default: None,
                    },
                    t.start,
                    self.last_end,
                ))
            }
            Tok::Rec => {
                self.bump();
                self.expect(Tok::LBrace, "expecting '{'")?;
                let binds = self.parse_binds(Tok::RBrace)?;
                self.expect(Tok::RBrace, "expecting '}'")?;
                Ok(self.add(Expr::Attrs { rec: true, binds }, t.start, self.last_end))
            }
            Tok::LBrace => {
                self.bump();
                let binds = self.parse_binds(Tok::RBrace)?;
                self.expect(Tok::RBrace, "expecting '}'")?;
                Ok(self.add(Expr::Attrs { rec: false, binds }, t.start, self.last_end))
            }
            Tok::LBracket => {
                self.bump();
                let mark = self.sc_ids.len();
                loop {
                    let n = self.peek();
                    if n.kind == Tok::RBracket {
                        self.bump();
                        break;
                    }
                    if !starts_simple(n.kind) {
                        return Err(self.unexpected(n, None));
                    }
                    let e = self.parse_select()?;
                    self.sc_ids.push(e);
                }
                let items = commit(&mut self.ast.ids, &mut self.sc_ids, mark);
                Ok(self.add(Expr::List(items), t.start, self.last_end))
            }
            _ => Err(self.unexpected(t, None)),
        }
    }

    /// Body of a `"..."` string; the opening quote is already consumed.
    /// Literal chunks are decoded straight into the bytes table.
    fn parse_string_parts(&mut self) -> R<Range> {
        debug_assert!(self.tok.is_none() && self.next.is_none());
        let mark = self.sc_parts.len();
        loop {
            let start = self.ast.bytes.len();
            let end = self.lx.lex_str_chunk(&mut self.ast.bytes)?;
            let lit = StrPart::Lit(Range {
                start: start as u32,
                len: (self.ast.bytes.len() - start) as u32,
            });
            let empty = self.ast.bytes.len() == start;
            match end {
                StrEnd::Close => {
                    if !empty || self.sc_parts.len() == mark {
                        self.sc_parts.push(lit);
                    }
                    self.last_end = self.lx.pos() as u32;
                    return Ok(commit(&mut self.ast.parts, &mut self.sc_parts, mark));
                }
                StrEnd::Interp => {
                    if !empty {
                        self.sc_parts.push(lit);
                    }
                    let e = self.parse_expr()?;
                    self.expect(Tok::RBrace, "expecting '}'")?;
                    self.sc_parts.push(StrPart::Interp(e));
                }
            }
        }
    }

    fn parse_ind_string(&mut self) -> R<Range> {
        debug_assert!(self.tok.is_none() && self.next.is_none());
        let mut parts = Vec::new();
        let mut interps = Vec::new();
        loop {
            match self.lx.lex_ind_chunk(&mut parts)? {
                StrEnd::Close => break,
                StrEnd::Interp => {
                    let e = self.parse_expr()?;
                    self.expect(Tok::RBrace, "expecting '}'")?;
                    parts.push(IndPart::Interp);
                    interps.push(e);
                }
            }
        }
        self.last_end = self.lx.pos() as u32;
        let mark = self.sc_parts.len();
        strip_indentation(
            self.src,
            &parts,
            &interps,
            &mut self.ast.bytes,
            &mut self.sc_parts,
        );
        Ok(commit(&mut self.ast.parts, &mut self.sc_parts, mark))
    }

    fn parse_path(&mut self, t: Token) -> R<ExprId> {
        let text = self.text(t);
        let (kind, first) = match t.kind {
            Tok::HPath => (PathKind::Home, &text[1..]),
            _ if text[0] == b'/' => (PathKind::Absolute, text),
            _ => (PathKind::Relative, text),
        };
        let mark = self.sc_parts.len();
        let p = self.lit(first);
        self.sc_parts.push(p);
        let mut after_slash = *text.last().unwrap() == b'/';
        // parser.y: `string_parts_interpolated` begins with `${` or with one
        // STR that `${` follows, so a piece of text before the first
        // interpolation cannot end the path
        let mut need_interp = false;
        let mut seen_interp = false;
        loop {
            let part = self.lx.lex_path_part(after_slash)?;
            if need_interp && !matches!(part, PathPart::Interp) {
                let what = match part {
                    PathPart::End => "end of path",
                    _ => "string",
                };
                let t = self.lx.pos() as u32;
                return Err(self.error_at(
                    t,
                    t,
                    format!("syntax error, unexpected {what}, expecting '${{'"),
                ));
            }
            match part {
                PathPart::End => break,
                PathPart::Str(s, e) => {
                    need_interp = !seen_interp;
                    let bytes = &self.src[s as usize..e as usize];
                    after_slash = bytes[bytes.len() - 1] == b'/';
                    // Consecutive literal pieces sit at the end of the
                    // bytes table (no interpolation was parsed between
                    // them), so the last part just grows.
                    match self.sc_parts.last_mut() {
                        Some(StrPart::Lit(l))
                            if (l.start + l.len) as usize == self.ast.bytes.len() =>
                        {
                            self.ast.bytes.extend_from_slice(bytes);
                            l.len += bytes.len() as u32;
                        }
                        _ => {
                            let p = self.lit(bytes);
                            self.sc_parts.push(p);
                        }
                    }
                    self.last_end = e;
                }
                PathPart::Interp => {
                    need_interp = false;
                    seen_interp = true;
                    let e = self.parse_expr()?;
                    self.expect(Tok::RBrace, "expecting '}'")?;
                    self.sc_parts.push(StrPart::Interp(e));
                    after_slash = false;
                }
            }
        }
        let parts = commit(&mut self.ast.parts, &mut self.sc_parts, mark);
        Ok(self.add(Expr::Path(PathExpr { kind, parts }), t.start, self.last_end))
    }

    fn parse_attrpath(&mut self) -> R<Range> {
        let mark = self.sc_elems.len();
        loop {
            let e = self.parse_attr_elem()?;
            self.sc_elems.push(e);
            if self.peek().kind == Tok::Dot {
                self.bump();
            } else {
                return Ok(commit(&mut self.ast.elems, &mut self.sc_elems, mark));
            }
        }
    }

    fn parse_attr_elem(&mut self) -> R<AttrElem> {
        let t = self.peek();
        match t.kind {
            Tok::Id => {
                self.bump();
                Ok(AttrElem::Static(self.interner.intern(self.text(t))))
            }
            Tok::Or => {
                self.bump();
                Ok(AttrElem::Static(self.sym_or))
            }
            Tok::DQuote => {
                self.bump();
                let parts = self.parse_string_parts()?;
                Ok(self.str_to_attr_elem(parts, t.start))
            }
            Tok::DollarCurly => {
                self.bump();
                let e = self.parse_expr()?;
                self.expect(Tok::RBrace, "expecting '}'")?;
                Ok(self.expr_to_attr_elem(e))
            }
            _ => Err(self.unexpected(t, Some("expecting identifier or 'or' or '${' or '\"'"))),
        }
    }

    fn str_to_attr_elem(&mut self, parts: Range, start: u32) -> AttrElem {
        match self.ast.parts(parts) {
            [] => AttrElem::Static(self.interner.intern(b"")),
            [StrPart::Lit(l)] => AttrElem::Static(self.interner.intern(self.ast.bytes(*l))),
            _ => {
                let e = self.add(Expr::Str(parts), start, self.last_end);
                AttrElem::Dynamic(e)
            }
        }
    }

    /// `${"literal"}` counts as a static attribute, like in Nix.
    fn expr_to_attr_elem(&mut self, e: ExprId) -> AttrElem {
        if let Expr::Str(parts) = self.ast.exprs[e.0 as usize] {
            match self.ast.parts(parts) {
                [] => return AttrElem::Static(self.interner.intern(b"")),
                [StrPart::Lit(l)] => {
                    return AttrElem::Static(self.interner.intern(self.ast.bytes(*l)));
                }
                _ => {}
            }
        }
        AttrElem::Dynamic(e)
    }

    fn parse_binds(&mut self, closer: Tok) -> R<Range> {
        let mark = self.sc_binds.len();
        loop {
            let t = self.peek();
            if t.kind == closer {
                break;
            }
            if t.kind == Tok::Inherit {
                self.bump();
                let mut from = None;
                if self.peek().kind == Tok::LParen {
                    self.bump();
                    from = Some(self.parse_expr()?);
                    self.expect(Tok::RParen, "expecting ')'")?;
                }
                let nmark = self.sc_names.len();
                loop {
                    let n = self.peek();
                    match n.kind {
                        Tok::Id => {
                            self.bump();
                            let s = self.interner.intern(self.text(n));
                            self.sc_names.push((s, n.span()));
                        }
                        Tok::Or => {
                            self.bump();
                            self.sc_names.push((self.sym_or, n.span()));
                        }
                        Tok::DQuote => {
                            self.bump();
                            let parts = self.parse_string_parts()?;
                            let elem = self.str_to_attr_elem(parts, n.start);
                            let s = self.static_inherit_name(elem, n)?;
                            self.sc_names.push((s, n.span()));
                        }
                        Tok::DollarCurly => {
                            self.bump();
                            let e = self.parse_expr()?;
                            self.expect(Tok::RBrace, "expecting '}'")?;
                            let elem = self.expr_to_attr_elem(e);
                            let s = self.static_inherit_name(elem, n)?;
                            self.sc_names.push((s, n.span()));
                        }
                        Tok::Semi => {
                            self.bump();
                            break;
                        }
                        _ => return Err(self.unexpected(n, None)),
                    }
                }
                let names = commit(&mut self.ast.names, &mut self.sc_names, nmark);
                self.sc_binds.push(Bind {
                    span: Span {
                        start: t.start,
                        end: self.last_end,
                    },
                    kind: BindKind::Inherit { from, names },
                });
                continue;
            }
            let path = self.parse_attrpath()?;
            self.expect(Tok::Assign, "expecting '.' or '='")?;
            let value = self.parse_expr()?;
            self.expect(Tok::Semi, "expecting ';'")?;
            self.sc_binds.push(Bind {
                span: Span {
                    start: t.start,
                    end: self.last_end,
                },
                kind: BindKind::Value { path, value },
            });
        }
        let binds = commit(&mut self.ast.binds, &mut self.sc_binds, mark);
        check_dup_attrs(self.src, &self.ast, self.interner, binds)?;
        Ok(binds)
    }

    fn static_inherit_name(&self, elem: AttrElem, t: Token) -> R<Sym> {
        match elem {
            AttrElem::Static(s) => Ok(s),
            AttrElem::Dynamic(_) => Err(self.error_at(
                t.start,
                self.last_end,
                "dynamic attributes not allowed in inherit".into(),
            )),
        }
    }
}

fn parse_int(text: &[u8]) -> Option<i64> {
    let mut v: u64 = 0;
    for &b in text {
        v = v.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    i64::try_from(v).ok()
}

/// Port of `ParserState::stripIndentation` from Nix. Token boundaries
/// matter: only the last token loses a trailing whitespace-only line.
/// Output text goes to `bytes`, the parts to `out`.
fn strip_indentation(
    src: &[u8],
    parts: &[IndPart],
    interps: &[ExprId],
    bytes: &mut Vec<u8>,
    out: &mut Vec<StrPart>,
) {
    let out_mark = out.len();
    if parts.is_empty() {
        out.push(StrPart::Lit(Range {
            start: bytes.len() as u32,
            len: 0,
        }));
        return;
    }
    let mut at_start = true;
    let mut min_indent = usize::MAX;
    let mut cur_indent = 0usize;
    for p in parts {
        match p {
            IndPart::Src(s, e) => {
                for &b in &src[*s as usize..*e as usize] {
                    if at_start {
                        if b == b' ' {
                            cur_indent += 1;
                        } else if b == b'\n' {
                            cur_indent = 0;
                        } else {
                            at_start = false;
                            min_indent = min_indent.min(cur_indent);
                        }
                    } else if b == b'\n' {
                        at_start = true;
                        cur_indent = 0;
                    }
                }
            }
            IndPart::Lit(..) | IndPart::Interp => {
                if at_start {
                    at_start = false;
                    min_indent = min_indent.min(cur_indent);
                }
            }
        }
    }

    // `pending` is bytes[pending_start..]: the literal run being built.
    let mut pending_start = bytes.len();
    let mut interp_i = 0;
    at_start = true;
    let mut cur_dropped = 0usize;
    for (idx, p) in parts.iter().enumerate() {
        let last = idx + 1 == parts.len();
        let text: &[u8] = match p {
            IndPart::Interp => {
                at_start = false;
                cur_dropped = 0;
                if bytes.len() > pending_start {
                    out.push(StrPart::Lit(Range {
                        start: pending_start as u32,
                        len: (bytes.len() - pending_start) as u32,
                    }));
                    pending_start = bytes.len();
                }
                out.push(StrPart::Interp(interps[interp_i]));
                interp_i += 1;
                continue;
            }
            IndPart::Src(s, e) => &src[*s as usize..*e as usize],
            IndPart::Lit(a, b, n) => {
                let tmp: [u8; 2] = [*a, *b];
                let n = *n as usize;
                let before = bytes.len();
                trim_into(
                    bytes,
                    &tmp[..n],
                    min_indent,
                    &mut at_start,
                    &mut cur_dropped,
                );
                if last {
                    drop_trailing_blank_line(bytes, before);
                }
                continue;
            }
        };
        let before = bytes.len();
        trim_into(bytes, text, min_indent, &mut at_start, &mut cur_dropped);
        if last {
            drop_trailing_blank_line(bytes, before);
        }
    }
    if bytes.len() > pending_start {
        out.push(StrPart::Lit(Range {
            start: pending_start as u32,
            len: (bytes.len() - pending_start) as u32,
        }));
    }
    if out.len() == out_mark {
        out.push(StrPart::Lit(Range {
            start: bytes.len() as u32,
            len: 0,
        }));
    }
}

fn trim_into(
    out: &mut Vec<u8>,
    bytes: &[u8],
    min_indent: usize,
    at_start: &mut bool,
    cur_dropped: &mut usize,
) {
    for &b in bytes {
        if *at_start {
            if b == b' ' {
                if *cur_dropped >= min_indent {
                    out.push(b);
                }
                *cur_dropped += 1;
            } else if b == b'\n' {
                *cur_dropped = 0;
                out.push(b);
            } else {
                *at_start = false;
                *cur_dropped = 0;
                out.push(b);
            }
        } else {
            out.push(b);
            if b == b'\n' {
                *at_start = true;
            }
        }
    }
}

/// If the text produced by the last token ends in a line of only spaces,
/// remove that line (keep the newline before it).
fn drop_trailing_blank_line(pending: &mut Vec<u8>, token_start: usize) {
    let tail = &pending[token_start..];
    if let Some(p) = tail.iter().rposition(|&b| b == b'\n')
        && tail[p + 1..].iter().all(|&b| b == b' ')
    {
        pending.truncate(token_start + p + 1);
    }
}

// duplicate attribute detection, mirrors ParserState::addAttr

enum NodeKind {
    Leaf,
    /// A literal attrset value not yet expanded into a map.
    Lazy(ExprId),
    Map(SymMap),
}

struct Node {
    sym: Sym,
    pos: u32,
    kind: NodeKind,
}

#[derive(Default)]
struct SymMap {
    nodes: Vec<Node>,
    index: Option<FxHashMap<Sym, u32>>,
}

const INDEX_THRESHOLD: usize = 16;

impl SymMap {
    fn find(&self, s: Sym) -> Option<usize> {
        match &self.index {
            Some(ix) => ix.get(&s).map(|&i| i as usize),
            None => self.nodes.iter().position(|n| n.sym == s),
        }
    }

    fn insert(&mut self, node: Node) -> usize {
        let i = self.nodes.len();
        if let Some(ix) = &mut self.index {
            ix.insert(node.sym, i as u32);
        } else if i >= INDEX_THRESHOLD {
            let mut ix = FxHashMap::with_capacity_and_hasher(i * 2, Default::default());
            for (j, n) in self.nodes.iter().enumerate() {
                ix.insert(n.sym, j as u32);
            }
            ix.insert(node.sym, i as u32);
            self.index = Some(ix);
        }
        self.nodes.push(node);
        i
    }
}

struct DupCx<'a> {
    src: &'a [u8],
    ast: &'a Ast,
    interner: &'a Interner,
}

impl<'a> DupCx<'a> {
    fn binds_of(&self, e: ExprId) -> &'a [Bind] {
        match self.ast.expr(e) {
            Expr::Attrs { binds, .. } => self.ast.binds(*binds),
            _ => unreachable!("Lazy node always holds an Attrs expr"),
        }
    }

    fn is_attrs(&self, e: ExprId) -> bool {
        matches!(self.ast.expr(e), Expr::Attrs { .. })
    }

    fn dup(&self, prefix: &[Sym], pos: u32, prev: u32) -> ParseError {
        let name = prefix
            .iter()
            .map(|s| self.interner.name_str(*s).to_string())
            .collect::<Vec<_>>()
            .join(".");
        let (l, c) = line_col(self.src, prev);
        ParseError {
            span: Span {
                start: pos,
                end: pos,
            },
            msg: format!("attribute '{name}' already defined at {l}:{c}"),
        }
    }

    fn expand(&self, node: &mut Node, prefix: &mut Vec<Sym>) -> R<()> {
        if let NodeKind::Lazy(e) = node.kind {
            let mut m = SymMap::default();
            self.add_binds(&mut m, self.binds_of(e), prefix)?;
            node.kind = NodeKind::Map(m);
        }
        Ok(())
    }

    fn add_binds(&self, map: &mut SymMap, binds: &[Bind], prefix: &mut Vec<Sym>) -> R<()> {
        let depth = prefix.len();
        for b in binds {
            match b.kind {
                BindKind::Value { path, value } => {
                    self.add_path(map, self.ast.elems(path), value, b.span.start, prefix)?;
                }
                BindKind::Inherit { names, .. } => {
                    for (s, sp) in self.ast.names(names) {
                        self.add_leaf(map, *s, None, sp.start, prefix)?;
                        prefix.truncate(depth);
                    }
                }
            }
            prefix.truncate(depth);
        }
        Ok(())
    }

    fn add_path(
        &self,
        mut map: &mut SymMap,
        path: &[AttrElem],
        value: ExprId,
        pos: u32,
        prefix: &mut Vec<Sym>,
    ) -> R<()> {
        let (last, init) = path.split_last().unwrap();
        for elem in init {
            let AttrElem::Static(s) = elem else {
                return Ok(());
            };
            prefix.push(*s);
            let idx = match map.find(*s) {
                Some(i) => i,
                None => map.insert(Node {
                    sym: *s,
                    pos,
                    kind: NodeKind::Map(SymMap::default()),
                }),
            };
            let node = &mut map.nodes[idx];
            match node.kind {
                NodeKind::Leaf => return Err(self.dup(prefix, pos, node.pos)),
                NodeKind::Lazy(_) => self.expand(node, prefix)?,
                NodeKind::Map(_) => {}
            }
            map = match &mut map.nodes[idx].kind {
                NodeKind::Map(m) => m,
                _ => unreachable!(),
            };
        }
        match last {
            AttrElem::Static(s) => self.add_leaf(map, *s, Some(value), pos, prefix),
            AttrElem::Dynamic(_) => Ok(()),
        }
    }

    fn add_leaf(
        &self,
        map: &mut SymMap,
        s: Sym,
        value: Option<ExprId>,
        pos: u32,
        prefix: &mut Vec<Sym>,
    ) -> R<()> {
        prefix.push(s);
        let attrs_val = value.filter(|e| self.is_attrs(*e));
        match map.find(s) {
            None => {
                map.insert(Node {
                    sym: s,
                    pos,
                    kind: match attrs_val {
                        Some(e) => NodeKind::Lazy(e),
                        None => NodeKind::Leaf,
                    },
                });
                Ok(())
            }
            Some(i) => {
                let node = &mut map.nodes[i];
                let Some(e) = attrs_val else {
                    return Err(self.dup(prefix, pos, node.pos));
                };
                if let NodeKind::Leaf = node.kind {
                    return Err(self.dup(prefix, pos, node.pos));
                }
                self.expand(node, prefix)?;
                let NodeKind::Map(m) = &mut node.kind else {
                    unreachable!()
                };
                self.add_binds(m, self.binds_of(e), prefix)
            }
        }
    }
}

fn check_dup_attrs(src: &[u8], ast: &Ast, interner: &Interner, binds: Range) -> R<()> {
    let binds = ast.binds(binds);
    let needs_check = match binds {
        [] => false,
        [b] => matches!(b.kind, BindKind::Inherit { names, .. } if names.len() > 1),
        _ => true,
    };
    if !needs_check {
        return Ok(());
    }
    let cx = DupCx { src, ast, interner };
    let mut map = SymMap::default();
    let mut prefix = Vec::new();
    cx.add_binds(&mut map, binds, &mut prefix)
}
