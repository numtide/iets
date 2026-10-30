//! The syntax tree as data. `tree` walks an `Ast` once; `json` prints the
//! machine form of `iets parse --json` (docs/reference/syntax-tree.md) and
//! `sexp` the S-expressions of `iets parse` and of the tests.

use super::ast::{
    Ast, AttrElem, BinOp, Bind, BindKind, Expr, ExprId, Formal, Param, PathKind, Range, Span,
    StrPart, UnOp,
};
use super::symbol::{Interner, Sym};

/// A node with its kind, its span and its fields in order, or a plain
/// field of its parent. The field names and the kinds are the JSON schema.
pub enum Tree<'a> {
    Node(&'static str, Span, Vec<(&'static str, Tree<'a>)>),
    List(Vec<Tree<'a>>),
    Text(&'a [u8]),
    Name(&'static str),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

pub fn dump(ast: &Ast, interner: &Interner) -> String {
    let mut out = String::new();
    sexp(&tree(ast, interner), &mut out);
    out
}

pub fn tree<'a>(ast: &'a Ast, interner: &'a Interner) -> Tree<'a> {
    Walker { ast, interner }.expr(ast.root)
}

struct Walker<'a> {
    ast: &'a Ast,
    interner: &'a Interner,
}

impl<'a> Walker<'a> {
    fn name(&self, s: Sym) -> Tree<'a> {
        Tree::Name(self.interner.name_str(s))
    }

    fn opt(&self, e: Option<ExprId>) -> Tree<'a> {
        e.map_or(Tree::Null, |e| self.expr(e))
    }

    fn list(&self, ids: Range) -> Tree<'a> {
        Tree::List(self.ast.ids(ids).iter().map(|e| self.expr(*e)).collect())
    }

    fn parts(&self, parts: Range) -> Tree<'a> {
        let part = |p: &StrPart| match p {
            StrPart::Lit(l) => Tree::Text(self.ast.bytes(*l)),
            StrPart::Interp(e) => self.expr(*e),
        };
        Tree::List(self.ast.parts(parts).iter().map(part).collect())
    }

    fn attrpath(&self, path: Range) -> Tree<'a> {
        let elem = |a: &AttrElem| match a {
            AttrElem::Static(s) => self.name(*s),
            AttrElem::Dynamic(e) => self.expr(*e),
        };
        Tree::List(self.ast.elems(path).iter().map(elem).collect())
    }

    fn binds(&self, binds: Range) -> Tree<'a> {
        let bind = |b: &Bind| match b.kind {
            BindKind::Value { path, value } => Tree::Node(
                "bind",
                b.span,
                vec![("path", self.attrpath(path)), ("value", self.expr(value))],
            ),
            BindKind::Inherit { from, names } => {
                let name = |(s, span): &(Sym, Span)| {
                    Tree::Node("name", *span, vec![("name", self.name(*s))])
                };
                let names = self.ast.names(names).iter().map(name).collect();
                Tree::Node(
                    "inherit",
                    b.span,
                    vec![("from", self.opt(from)), ("names", Tree::List(names))],
                )
            }
        };
        Tree::List(self.ast.binds(binds).iter().map(bind).collect())
    }

    fn lambda(&self, param: Param, body: ExprId) -> Vec<(&'static str, Tree<'a>)> {
        let (arg, formals, ellipsis) = match param {
            Param::Ident(s) => (self.name(s), Tree::Null, false),
            Param::Formals {
                bind,
                formals,
                ellipsis,
            } => {
                let formal = |f: &Formal| {
                    let fields = vec![
                        ("name", self.name(f.name)),
                        ("default", self.opt(f.default)),
                    ];
                    Tree::Node("formal", f.span, fields)
                };
                let formals = self.ast.formals(formals).iter().map(formal).collect();
                let arg = bind.map_or(Tree::Null, |b| self.name(b));
                (arg, Tree::List(formals), ellipsis)
            }
        };
        vec![
            ("arg", arg),
            ("formals", formals),
            ("ellipsis", Tree::Bool(ellipsis)),
            ("body", self.expr(body)),
        ]
    }

    fn expr(&self, id: ExprId) -> Tree<'a> {
        let (kind, fields) = match *self.ast.expr(id) {
            Expr::Int(i) => ("int", vec![("value", Tree::Int(i))]),
            Expr::Float(f) => ("float", vec![("value", Tree::Float(f))]),
            Expr::Str(parts) => ("string", vec![("parts", self.parts(parts))]),
            Expr::Path(p) => {
                let form = match p.kind {
                    PathKind::Relative => "relative",
                    PathKind::Absolute => "absolute",
                    PathKind::Home => "home",
                    PathKind::Search => "search",
                };
                let fields = vec![("form", Tree::Name(form)), ("parts", self.parts(p.parts))];
                ("path", fields)
            }
            Expr::Var(s) => ("var", vec![("name", self.name(s))]),
            Expr::Select {
                subject,
                path,
                default,
            } => (
                "select",
                vec![
                    ("subject", self.expr(subject)),
                    ("path", self.attrpath(path)),
                    ("default", self.opt(default)),
                ],
            ),
            Expr::HasAttr { subject, path } => (
                "has-attr",
                vec![
                    ("subject", self.expr(subject)),
                    ("path", self.attrpath(path)),
                ],
            ),
            Expr::Attrs { rec, binds } => (
                "attrs",
                vec![("rec", Tree::Bool(rec)), ("binds", self.binds(binds))],
            ),
            Expr::List(items) => ("list", vec![("items", self.list(items))]),
            Expr::Lambda { param, body } => ("lambda", self.lambda(param, body)),
            Expr::Apply { func, args } => (
                "apply",
                vec![("func", self.expr(func)), ("args", self.list(args))],
            ),
            Expr::Let { binds, body } => (
                "let",
                vec![("binds", self.binds(binds)), ("body", self.expr(body))],
            ),
            Expr::With { scope, body } => (
                "with",
                vec![("scope", self.expr(scope)), ("body", self.expr(body))],
            ),
            Expr::If { cond, then, else_ } => (
                "if",
                vec![
                    ("cond", self.expr(cond)),
                    ("then", self.expr(then)),
                    ("else", self.expr(else_)),
                ],
            ),
            Expr::Assert { cond, body } => (
                "assert",
                vec![("cond", self.expr(cond)), ("body", self.expr(body))],
            ),
            Expr::Unary { op, expr } => {
                let op = match op {
                    UnOp::Not => "!",
                    UnOp::Neg => "-",
                };
                (
                    "unary",
                    vec![("op", Tree::Name(op)), ("expr", self.expr(expr))],
                )
            }
            Expr::Binary { op, lhs, rhs } => {
                let op = match op {
                    BinOp::Eq => "==",
                    BinOp::Ne => "!=",
                    BinOp::Lt => "<",
                    BinOp::Gt => ">",
                    BinOp::Le => "<=",
                    BinOp::Ge => ">=",
                    BinOp::And => "&&",
                    BinOp::Or => "||",
                    BinOp::Impl => "->",
                    BinOp::Update => "//",
                    BinOp::Concat => "++",
                    BinOp::Add => "+",
                    BinOp::Sub => "-",
                    BinOp::Mul => "*",
                    BinOp::Div => "/",
                };
                (
                    "binary",
                    vec![
                        ("op", Tree::Name(op)),
                        ("lhs", self.expr(lhs)),
                        ("rhs", self.expr(rhs)),
                    ],
                )
            }
        };
        Tree::Node(kind, self.ast.span(id), fields)
    }
}

/// One JSON value, a node as `{"kind", "start", "end", fields...}`. Text
/// that is not UTF-8 is converted with replacement characters.
pub fn json(t: &Tree, out: &mut String) {
    let string = |s: &str| serde_json::Value::from(s).to_string();
    match t {
        Tree::Node(kind, span, fields) => {
            let (start, end) = (span.start, span.end);
            out.push_str(&format!(
                "{{\"kind\":\"{kind}\",\"start\":{start},\"end\":{end}"
            ));
            for (name, value) in fields {
                out.push_str(&format!(",\"{name}\":"));
                json(value, out);
            }
            out.push('}');
        }
        Tree::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json(item, out);
            }
            out.push(']');
        }
        Tree::Text(b) => out.push_str(&string(&String::from_utf8_lossy(b))),
        Tree::Name(s) => out.push_str(&string(s)),
        Tree::Int(i) => out.push_str(&i.to_string()),
        Tree::Float(f) => out.push_str(&serde_json::Value::from(*f).to_string()),
        Tree::Bool(b) => out.push_str(&b.to_string()),
        Tree::Null => out.push_str("null"),
    }
}

fn quote(bytes: &[u8], out: &mut String) {
    out.push('"');
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push('"');
}

/// `items`, each after `sep` but the first; a node among plain fields is
/// wrapped as `(WRAP node)`.
fn seq(items: &Tree, first: &str, sep: &str, wrap: &str, out: &mut String) {
    let Tree::List(items) = items else { return };
    for (i, item) in items.iter().enumerate() {
        out.push_str(if i == 0 { first } else { sep });
        match item {
            Tree::Node(..) if !wrap.is_empty() => {
                out.push_str(&format!("({wrap} "));
                sexp(item, out);
                out.push(')');
            }
            _ => sexp(item, out),
        }
    }
}

fn sexp(t: &Tree, out: &mut String) {
    let (kind, fields) = match t {
        Tree::Node(kind, _, fields) => (*kind, fields.as_slice()),
        Tree::Text(b) => return quote(b, out),
        Tree::Name(s) => return out.push_str(s),
        Tree::Int(i) => return out.push_str(&i.to_string()),
        Tree::Float(f) => return out.push_str(&format!("{f:?}")),
        Tree::List(_) | Tree::Bool(_) | Tree::Null => return,
    };
    let open = |head: &str, out: &mut String| {
        out.push('(');
        out.push_str(head);
    };
    // field `i` is an attribute path
    let attrpath = |i: usize, out: &mut String| {
        out.push('(');
        seq(&fields[i].1, "", " ", "dyn", out);
        out.push(')');
    };
    match (kind, fields) {
        ("int" | "float" | "var" | "name", [(_, v)]) => return sexp(v, out),
        ("string", [(_, parts)]) => {
            open("str", out);
            seq(parts, " ", " ", "interp", out);
        }
        ("path", [(_, Tree::Name(form)), (_, parts)]) => {
            open("path ", out);
            out.push_str(match *form {
                "relative" => "rel",
                "absolute" => "abs",
                form => form,
            });
            seq(parts, " ", " ", "interp", out);
        }
        ("select", [(_, subject), _, (_, default)]) => {
            open("select ", out);
            sexp(subject, out);
            out.push(' ');
            attrpath(1, out);
            if !matches!(default, Tree::Null) {
                out.push_str(" (or ");
                sexp(default, out);
                out.push(')');
            }
        }
        ("has-attr", [(_, subject), _]) => {
            open("? ", out);
            sexp(subject, out);
            out.push(' ');
            attrpath(1, out);
        }
        ("attrs", [(_, Tree::Bool(rec)), (_, binds)]) => {
            open(if *rec { "rec-attrs" } else { "attrs" }, out);
            seq(binds, " ", " ", "", out);
        }
        ("bind", [_, (_, value)]) => {
            open("= ", out);
            attrpath(0, out);
            out.push(' ');
            sexp(value, out);
        }
        ("inherit", [(_, from), (_, names)]) => {
            open("inherit", out);
            if !matches!(from, Tree::Null) {
                out.push_str(" (from ");
                sexp(from, out);
                out.push(')');
            }
            seq(names, " ", " ", "", out);
        }
        ("list", [(_, items)]) => {
            open("list", out);
            seq(items, " ", " ", "", out);
        }
        ("lambda", [(_, arg), (_, formals), (_, Tree::Bool(ellipsis)), (_, body)]) => {
            open("lambda ", out);
            if let Tree::List(list) = formals {
                out.push('{');
                seq(formals, "", ", ", "", out);
                if *ellipsis {
                    out.push_str(if list.is_empty() { "..." } else { ", ..." });
                }
                out.push('}');
                if !matches!(arg, Tree::Null) {
                    out.push_str(" @ ");
                }
            }
            sexp(arg, out);
            out.push(' ');
            sexp(body, out);
        }
        ("formal", [(_, name), (_, default)]) => {
            sexp(name, out);
            if !matches!(default, Tree::Null) {
                out.push_str(" ? ");
                sexp(default, out);
            }
            return;
        }
        ("apply", [(_, func), (_, args)]) => {
            open("apply ", out);
            sexp(func, out);
            seq(args, " ", " ", "", out);
        }
        ("let", [(_, binds), (_, body)]) => {
            open("let (", out);
            seq(binds, " ", " ", "", out);
            out.push_str(") ");
            sexp(body, out);
        }
        ("unary", [(_, Tree::Name(op)), (_, expr)]) => {
            open(if *op == "-" { "neg " } else { "! " }, out);
            sexp(expr, out);
        }
        ("binary", [(_, op), (_, lhs), (_, rhs)]) => {
            out.push('(');
            for (i, part) in [op, lhs, rhs].into_iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                sexp(part, out);
            }
        }
        // `with`, `if` and `assert`: the kind, then each field
        (_, fields) => {
            open(kind, out);
            for (_, field) in fields {
                out.push(' ');
                sexp(field, out);
            }
        }
    }
    out.push(')');
}
