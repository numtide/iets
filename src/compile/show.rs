//! `Expr::show` (nixexpr.cc), with which eval.cc's `ExprAssert::eval` names
//! a failed condition: `assertion '(x != null)' failed`. Nix prints the tree
//! its parser made, every compound in parentheses. iets's tree is the
//! source as written, so this prints what parser.y would have made of it:
//! `a - b` is `(__sub a b)`, `a > b` is `(__lessThan b a)`, `a >= b` is
//! `(! (__lessThan a b))`, `-a` is `(__sub 0 a)`, `<p>` is
//! `(__findFile __nixPath "p")`, a call applied again is one call
//! (`makeCall`), and `a.b = 1; a.c = 2;` is one nested set.

use std::collections::BTreeMap;
use std::slice;

use super::Compiler;
use crate::print::{format_g, print_literal_string};
use crate::syntax::ast::{
    AttrElem, BinOp, Bind, BindKind, Expr, ExprId, Param, PathExpr, PathKind, StrPart, UnOp,
};
use crate::syntax::symbol::Sym;

/// A call's function or argument once parser.y's rewrites are applied:
/// an expression, or text the rewrite put there.
enum Arg {
    Expr(ExprId),
    Text(Vec<u8>),
}

/// One binding of a set or a `let`, after parser.y's `addAttr`.
enum Entry {
    Value(ExprId),
    Nested(Binds),
    Inherit,
    /// `inherit (e)`, by the index of its clause.
    InheritFrom(usize),
}

/// ExprAttrs's bindings, which `showBindings` prints.
#[derive(Default)]
struct Binds {
    named: BTreeMap<Vec<u8>, Entry>,
    dynamic: Vec<(ExprId, Entry)>,
    froms: Vec<ExprId>,
}

/// print.cc `printIdentifier`: bare when it is an identifier, a keyword in
/// quotes, anything else as a string literal.
fn print_identifier(out: &mut Vec<u8>, s: &[u8]) {
    const KEYWORDS: [&[u8]; 9] = [
        b"if", b"then", b"else", b"assert", b"with", b"let", b"in", b"rec", b"inherit",
    ];
    if s.is_empty() {
        out.extend_from_slice(b"\"\"");
    } else if KEYWORDS.contains(&s) {
        out.push(b'"');
        out.extend_from_slice(s);
        out.push(b'"');
    } else if (s[0].is_ascii_alphabetic() || s[0] == b'_')
        && s.iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'\'' | b'-'))
    {
        out.extend_from_slice(s);
    } else {
        print_literal_string(out, s);
    }
}

impl Compiler<'_> {
    /// `Expr::show` of `id`.
    pub(super) fn show(&self, id: ExprId) -> Vec<u8> {
        let mut out = Vec::new();
        self.show_to(id, &mut out);
        out
    }

    fn show_name(&self, s: Sym, out: &mut Vec<u8>) {
        print_identifier(out, self.interner.name(s));
    }

    fn show_to(&self, id: ExprId, out: &mut Vec<u8>) {
        let ast = self.ast;
        if let Some((head, args)) = self.call_of(id) {
            out.push(b'(');
            self.show_arg(&head, out);
            for a in &args {
                out.push(b' ');
                self.show_arg(a, out);
            }
            out.push(b')');
            return;
        }
        match ast.expr(id) {
            Expr::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
            Expr::Float(f) => out.extend_from_slice(format_g(*f).as_bytes()),
            Expr::Str(parts) => self.show_string(ast.parts(*parts), out),
            Expr::Path(p) => self.show_path(p, out),
            Expr::Var(s) => {
                if *s == self.sym_cur_pos {
                    out.extend_from_slice(b"__curPos");
                } else {
                    self.show_name(*s, out);
                }
            }
            Expr::Select {
                subject,
                path,
                default,
            } => {
                out.push(b'(');
                self.show_to(*subject, out);
                out.extend_from_slice(b").");
                self.show_attr_path(ast.elems(*path), out);
                if let Some(d) = default {
                    out.extend_from_slice(b" or (");
                    self.show_to(*d, out);
                    out.push(b')');
                }
            }
            Expr::HasAttr { subject, path } => {
                out.extend_from_slice(b"((");
                self.show_to(*subject, out);
                out.extend_from_slice(b") ? ");
                self.show_attr_path(ast.elems(*path), out);
                out.push(b')');
            }
            Expr::Attrs { rec, binds } => {
                if *rec {
                    out.extend_from_slice(b"rec ");
                }
                out.extend_from_slice(b"{ ");
                self.show_binds(&self.merge(ast.binds(*binds)), out);
                out.push(b'}');
            }
            Expr::List(items) => {
                out.extend_from_slice(b"[ ");
                for e in ast.ids(*items) {
                    out.push(b'(');
                    self.show_to(*e, out);
                    out.extend_from_slice(b") ");
                }
                out.push(b']');
            }
            Expr::Lambda { param, body } => {
                out.push(b'(');
                match param {
                    Param::Ident(s) => self.show_name(*s, out),
                    Param::Formals {
                        bind,
                        formals,
                        ellipsis,
                    } => {
                        out.extend_from_slice(b"{ ");
                        let mut fs = ast.formals(*formals).to_vec();
                        fs.sort_by(|a, b| {
                            self.interner.name(a.name).cmp(self.interner.name(b.name))
                        });
                        for (i, f) in fs.iter().enumerate() {
                            if i > 0 {
                                out.extend_from_slice(b", ");
                            }
                            self.show_name(f.name, out);
                            if let Some(d) = f.default {
                                out.extend_from_slice(b" ? ");
                                self.show_to(d, out);
                            }
                        }
                        if *ellipsis {
                            if !fs.is_empty() {
                                out.extend_from_slice(b", ");
                            }
                            out.extend_from_slice(b"...");
                        }
                        out.extend_from_slice(b" }");
                        if let Some(b) = bind {
                            out.extend_from_slice(b" @ ");
                            self.show_name(*b, out);
                        }
                    }
                }
                out.extend_from_slice(b": ");
                self.show_to(*body, out);
                out.push(b')');
            }
            Expr::Let { binds, body } => {
                out.extend_from_slice(b"(let ");
                self.show_binds(&self.merge(ast.binds(*binds)), out);
                out.extend_from_slice(b"in ");
                self.show_to(*body, out);
                out.push(b')');
            }
            Expr::With { scope, body } => {
                out.extend_from_slice(b"(with ");
                self.show_to(*scope, out);
                out.extend_from_slice(b"; ");
                self.show_to(*body, out);
                out.push(b')');
            }
            Expr::If { cond, then, else_ } => {
                out.extend_from_slice(b"(if ");
                self.show_to(*cond, out);
                out.extend_from_slice(b" then ");
                self.show_to(*then, out);
                out.extend_from_slice(b" else ");
                self.show_to(*else_, out);
                out.push(b')');
            }
            Expr::Assert { cond, body } => {
                out.extend_from_slice(b"assert ");
                self.show_to(*cond, out);
                out.extend_from_slice(b"; ");
                self.show_to(*body, out);
            }
            Expr::Unary {
                op: UnOp::Not,
                expr,
            } => {
                out.extend_from_slice(b"(! ");
                self.show_to(*expr, out);
                out.push(b')');
            }
            Expr::Binary { op, lhs, rhs } => {
                let infix: &[u8] = match op {
                    BinOp::Eq => b" == ",
                    BinOp::Ne => b" != ",
                    BinOp::And => b" && ",
                    BinOp::Or => b" || ",
                    BinOp::Impl => b" -> ",
                    BinOp::Update => b" // ",
                    BinOp::Concat => b" ++ ",
                    // ExprConcatStrings of the two
                    BinOp::Add => b" + ",
                    // `!(__lessThan b a)` and `!(__lessThan a b)`
                    BinOp::Le | BinOp::Ge => {
                        let (a, b) = if *op == BinOp::Le {
                            (rhs, lhs)
                        } else {
                            (lhs, rhs)
                        };
                        out.extend_from_slice(b"(! (__lessThan ");
                        self.show_to(*a, out);
                        out.push(b' ');
                        self.show_to(*b, out);
                        out.extend_from_slice(b"))");
                        return;
                    }
                    // calls, which `call_of` took
                    BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Lt | BinOp::Gt => {
                        unreachable!("a call")
                    }
                };
                out.push(b'(');
                self.show_to(*lhs, out);
                out.extend_from_slice(infix);
                self.show_to(*rhs, out);
                out.push(b')');
            }
            Expr::Unary { op: UnOp::Neg, .. } | Expr::Apply { .. } => unreachable!("a call"),
        }
    }

    fn show_arg(&self, a: &Arg, out: &mut Vec<u8>) {
        match a {
            Arg::Expr(e) => self.show_to(*e, out),
            Arg::Text(t) => out.extend_from_slice(t),
        }
    }

    /// The function and arguments of `id` when parser.y made it an
    /// ExprCall, applications merged as `makeCall` merges them.
    fn call_of(&self, id: ExprId) -> Option<(Arg, Vec<Arg>)> {
        let text = |s: &str| Arg::Text(s.as_bytes().to_vec());
        Some(match self.ast.expr(id) {
            Expr::Apply { func, args } => {
                let (head, mut all) = self
                    .call_of(*func)
                    .unwrap_or_else(|| (Arg::Expr(*func), Vec::new()));
                all.extend(self.ast.ids(*args).iter().map(|a| Arg::Expr(*a)));
                (head, all)
            }
            Expr::Binary { op, lhs, rhs } => {
                let (f, a, b) = match op {
                    BinOp::Sub => ("__sub", lhs, rhs),
                    BinOp::Mul => ("__mul", lhs, rhs),
                    BinOp::Div => ("__div", lhs, rhs),
                    BinOp::Lt => ("__lessThan", lhs, rhs),
                    BinOp::Gt => ("__lessThan", rhs, lhs),
                    _ => return None,
                };
                (text(f), vec![Arg::Expr(*a), Arg::Expr(*b)])
            }
            Expr::Unary {
                op: UnOp::Neg,
                expr,
            } => (text("__sub"), vec![text("0"), Arg::Expr(*expr)]),
            Expr::Path(PathExpr {
                kind: PathKind::Search,
                parts,
            }) => {
                let mut name = Vec::new();
                for p in self.ast.parts(*parts) {
                    if let StrPart::Lit(l) = p {
                        name.extend_from_slice(self.ast.bytes(*l));
                    }
                }
                let mut lit = Vec::new();
                print_literal_string(&mut lit, &name);
                (text("__findFile"), vec![text("__nixPath"), Arg::Text(lit)])
            }
            _ => return None,
        })
    }

    /// ExprString, or ExprConcatStrings of the parts.
    fn show_string(&self, parts: &[StrPart], out: &mut Vec<u8>) {
        match parts {
            [] => out.extend_from_slice(b"\"\""),
            [StrPart::Lit(l)] => print_literal_string(out, self.ast.bytes(*l)),
            _ => {
                out.push(b'(');
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        out.extend_from_slice(b" + ");
                    }
                    match p {
                        StrPart::Lit(l) => print_literal_string(out, self.ast.bytes(*l)),
                        StrPart::Interp(e) => self.show_to(*e, out),
                    }
                }
                out.push(b')');
            }
        }
    }

    /// ExprPath, resolved as `path` resolves it, or ExprConcatStrings of it
    /// and the parts after it.
    fn show_path(&self, p: &PathExpr, out: &mut Vec<u8>) {
        let parts = self.ast.parts(p.parts);
        let all_lit = parts.iter().all(|p| matches!(p, StrPart::Lit(_)));
        let first: Vec<u8> = match parts.first() {
            Some(StrPart::Lit(l)) => self.ast.bytes(*l).to_vec(),
            _ => Vec::new(),
        };
        let mut resolved = self.resolve_path_literal(&p.kind, &first);
        if first.ends_with(b"/") && !all_lit && !resolved.ends_with(b"/") {
            resolved.push(b'/');
        }
        if all_lit {
            out.extend_from_slice(&resolved);
            return;
        }
        out.push(b'(');
        out.extend_from_slice(&resolved);
        let rest = if matches!(parts.first(), Some(StrPart::Lit(_))) {
            &parts[1..]
        } else {
            parts
        };
        for part in rest {
            out.extend_from_slice(b" + ");
            match part {
                StrPart::Lit(l) => print_literal_string(out, self.ast.bytes(*l)),
                StrPart::Interp(e) => self.show_to(*e, out),
            }
        }
        out.push(b')');
    }

    /// The literal text of an attribute path element that parser.y keeps
    /// as a name: a static one, or `${"x"}`, an ExprString.
    fn static_elem(&self, e: &AttrElem) -> Option<Vec<u8>> {
        match e {
            AttrElem::Static(s) => Some(self.interner.name(*s).to_vec()),
            AttrElem::Dynamic(d) => match self.ast.expr(*d) {
                Expr::Str(parts) => match self.ast.parts(*parts) {
                    [] => Some(Vec::new()),
                    [StrPart::Lit(l)] => Some(self.ast.bytes(*l).to_vec()),
                    _ => None,
                },
                _ => None,
            },
        }
    }

    /// `showAttrSelectionPath`.
    fn show_attr_path(&self, elems: &[AttrElem], out: &mut Vec<u8>) {
        for (i, e) in elems.iter().enumerate() {
            if i > 0 {
                out.push(b'.');
            }
            match (self.static_elem(e), e) {
                (Some(name), _) => print_identifier(out, &name),
                (None, AttrElem::Dynamic(d)) => {
                    out.extend_from_slice(b"\"${");
                    self.show_to(*d, out);
                    out.extend_from_slice(b"}\"");
                }
                (None, AttrElem::Static(_)) => unreachable!("a static element has a name"),
            }
        }
    }

    /// parser.y's `addAttr` over `binds`: paths become nested sets, and a
    /// set literal at a name another path enters is merged into it.
    fn merge(&self, binds: &[Bind]) -> Binds {
        let mut b = Binds::default();
        for bind in binds {
            match bind.kind {
                BindKind::Inherit { from: None, names } => {
                    for (s, _) in self.ast.names(names) {
                        b.named
                            .insert(self.interner.name(*s).to_vec(), Entry::Inherit);
                    }
                }
                BindKind::Inherit {
                    from: Some(e),
                    names,
                } => {
                    let g = b.froms.len();
                    b.froms.push(e);
                    for (s, _) in self.ast.names(names) {
                        b.named
                            .insert(self.interner.name(*s).to_vec(), Entry::InheritFrom(g));
                    }
                }
                BindKind::Value { path, value } => {
                    self.insert(&mut b, self.ast.elems(path), value);
                }
            }
        }
        b
    }

    fn insert(&self, b: &mut Binds, elems: &[AttrElem], value: ExprId) {
        let Some((first, rest)) = elems.split_first() else {
            return;
        };
        let Some(name) = self.static_elem(first) else {
            let AttrElem::Dynamic(key) = first else {
                unreachable!("a static element has a name")
            };
            let entry = if rest.is_empty() {
                Entry::Value(value)
            } else {
                let mut nested = Binds::default();
                self.insert(&mut nested, rest, value);
                Entry::Nested(nested)
            };
            b.dynamic.push((*key, entry));
            return;
        };
        if rest.is_empty() {
            match (b.named.get_mut(&name), self.literal_binds(value)) {
                (Some(Entry::Nested(nested)), Some(binds)) => {
                    for bind in binds {
                        self.insert_bind(nested, bind);
                    }
                }
                _ => {
                    b.named.insert(name, Entry::Value(value));
                }
            }
            return;
        }
        let entry = b
            .named
            .entry(name)
            .or_insert_with(|| Entry::Nested(Binds::default()));
        if let Entry::Value(v) = entry
            && let Some(binds) = self.literal_binds(*v)
        {
            let mut nested = Binds::default();
            for bind in binds {
                self.insert_bind(&mut nested, bind);
            }
            *entry = Entry::Nested(nested);
        }
        if let Entry::Nested(nested) = entry {
            self.insert(nested, rest, value);
        }
    }

    fn insert_bind(&self, b: &mut Binds, bind: &Bind) {
        match bind.kind {
            BindKind::Value { path, value } => self.insert(b, self.ast.elems(path), value),
            BindKind::Inherit { .. } => {
                // the clause's own `inherit (e)` group, after the groups
                // `b` has
                let m = self.merge(slice::from_ref(bind));
                let base = b.froms.len();
                for (n, e) in m.named {
                    let e = match e {
                        Entry::InheritFrom(g) => Entry::InheritFrom(base + g),
                        e => e,
                    };
                    b.named.insert(n, e);
                }
                b.froms.extend(m.froms);
            }
        }
    }

    /// The bindings of a non-`rec` set literal, which parser.y merges.
    fn literal_binds(&self, e: ExprId) -> Option<&[Bind]> {
        match self.ast.expr(e) {
            Expr::Attrs { rec: false, binds } => Some(self.ast.binds(*binds)),
            _ => None,
        }
    }

    /// `ExprAttrs::showBindings`.
    fn show_binds(&self, b: &Binds, out: &mut Vec<u8>) {
        let inherits: Vec<&Vec<u8>> = b
            .named
            .iter()
            .filter(|(_, e)| matches!(e, Entry::Inherit))
            .map(|(n, _)| n)
            .collect();
        if !inherits.is_empty() {
            out.extend_from_slice(b"inherit");
            for n in inherits {
                out.push(b' ');
                print_identifier(out, n);
            }
            out.extend_from_slice(b"; ");
        }
        for (g, from) in b.froms.iter().enumerate() {
            let names: Vec<&Vec<u8>> = b
                .named
                .iter()
                .filter(|(_, e)| matches!(e, Entry::InheritFrom(x) if *x == g))
                .map(|(n, _)| n)
                .collect();
            if names.is_empty() {
                continue;
            }
            out.extend_from_slice(b"inherit (");
            self.show_to(*from, out);
            out.push(b')');
            for n in names {
                out.push(b' ');
                print_identifier(out, n);
            }
            out.extend_from_slice(b"; ");
        }
        for (n, e) in &b.named {
            if matches!(e, Entry::Value(_) | Entry::Nested(_)) {
                print_identifier(out, n);
                out.extend_from_slice(b" = ");
                self.show_entry(e, out);
                out.extend_from_slice(b"; ");
            }
        }
        for (key, e) in &b.dynamic {
            out.extend_from_slice(b"\"${");
            self.show_to(*key, out);
            out.extend_from_slice(b"}\" = ");
            self.show_entry(e, out);
            out.extend_from_slice(b"; ");
        }
    }

    fn show_entry(&self, e: &Entry, out: &mut Vec<u8>) {
        match e {
            Entry::Value(v) => self.show_to(*v, out),
            Entry::Nested(b) => {
                out.extend_from_slice(b"{ ");
                self.show_binds(b, out);
                out.push(b'}');
            }
            Entry::Inherit | Entry::InheritFrom(_) => unreachable!("printed with the inherits"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::eval::Evaluator;

    /// A failed assert names its condition as Nix 2.34.9 does: each case's
    /// message and assertion text are what `nix-instantiate --eval --expr`
    /// printed for it, with `/` as the base directory. A failed `==` says
    /// how the sides differ and names the assertion in a trace.
    #[test]
    fn a_failed_assert_reads_as_in_nix() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "let x = null; in assert x != null; 1",
                "assertion '(x != null)' failed",
                "(x != null)",
            ),
            ("assert false; 1", "assertion 'false' failed", "false"),
            ("assert !true; 1", "assertion '(! true)' failed", "(! true)"),
            (
                "assert 1 > 2; 1",
                "assertion '(__lessThan 2 1)' failed",
                "(__lessThan 2 1)",
            ),
            (
                "assert 1 >= 2; 1",
                "assertion '(! (__lessThan 1 2))' failed",
                "(! (__lessThan 1 2))",
            ),
            (
                "assert 2 <= 1; 1",
                "assertion '(! (__lessThan 1 2))' failed",
                "(! (__lessThan 1 2))",
            ),
            (
                "assert 2 < 1; 1",
                "assertion '(__lessThan 2 1)' failed",
                "(__lessThan 2 1)",
            ),
            (
                "assert 1 - 1 == 1; 1",
                "an integer with value '0' is not equal to an integer with value '1'",
                "((__sub 1 1) == 1)",
            ),
            (
                "assert -1 > 0; 1",
                "assertion '(__lessThan 0 (__sub 0 1))' failed",
                "(__lessThan 0 (__sub 0 1))",
            ),
            (
                "assert 2 * 3 / 6 > 5; 1",
                "assertion '(__lessThan 5 (__div (__mul 2 3) 6))' failed",
                "(__lessThan 5 (__div (__mul 2 3) 6))",
            ),
            (
                "let lib = { versionAtLeast = a: b: false; }; v = \"1\"; in assert lib.versionAtLeast v \"2\"; 1",
                "assertion '((lib).versionAtLeast v \"2\")' failed",
                "((lib).versionAtLeast v \"2\")",
            ),
            (
                "let a = { b.c = false; }; in assert a.b.c; 1",
                "assertion '(a).b.c' failed",
                "(a).b.c",
            ),
            (
                "let a = {}; in assert a.b or false; 1",
                "assertion '(a).b or (false)' failed",
                "(a).b or (false)",
            ),
            (
                "let a = {}; in assert a ? b; 1",
                "assertion '((a) ? b)' failed",
                "((a) ? b)",
            ),
            (
                "let a = {}; in assert a ? \"b c\".d; 1",
                "assertion '((a) ? \"b c\".d)' failed",
                "((a) ? \"b c\".d)",
            ),
            (
                "assert [ 1 2 ] == [ 1 ]; 1",
                "list of size '2' is not equal to list of size '1', left hand side is '[ 1 2 ]', right hand side is '[ 1 ]'",
                "([ (1) (2) ] == [ (1) ])",
            ),
            (
                "assert \"a\" == \"b\"; 1",
                "string '\"a\"' is not equal to string '\"b\"'",
                "(\"a\" == \"b\")",
            ),
            (
                "assert { a = 1; } == { a = 2; }; 1",
                "an integer with value '1' is not equal to an integer with value '2'",
                "({ a = 1; } == { a = 2; })",
            ),
            (
                "assert { a = 1; } == { b = 1; }; 1",
                "attribute name 'a' is contained in '{ a = 1; }', but not in '{ b = 1; }'",
                "({ a = 1; } == { b = 1; })",
            ),
            (
                "assert true == 1; 1",
                "a Boolean of value 'true' is not equal to an integer of value '1'",
                "(true == 1)",
            ),
            (
                "assert (x: x) == (x: x); 1",
                "distinct functions and immediate comparisons of identical functions compare as unequal",
                "((x: x) == (x: x))",
            ),
            (
                "assert (x: false) 1; 1",
                "assertion '((x: false) 1)' failed",
                "((x: false) 1)",
            ),
            (
                "assert ({ a, b ? 2, ... } @ args: false) { a = 1; }; 1",
                "assertion '(({ a, b ? 2, ... } @ args: false) { a = 1; })' failed",
                "(({ a, b ? 2, ... } @ args: false) { a = 1; })",
            ),
            (
                "assert ({ b, a }: false) { a = 1; b = 2; }; 1",
                "assertion '(({ a, b }: false) { a = 1; b = 2; })' failed",
                "(({ a, b }: false) { a = 1; b = 2; })",
            ),
            (
                "assert (let a = 1; b.c = 2; inherit (builtins) toString; in false); 1",
                "assertion '(let inherit (builtins) toString; a = 1; b = { c = 2; }; in false)' failed",
                "(let inherit (builtins) toString; a = 1; b = { c = 2; }; in false)",
            ),
            (
                "assert (with builtins; false); 1",
                "assertion '(with builtins; false)' failed",
                "(with builtins; false)",
            ),
            (
                "assert (if true then false else true); 1",
                "assertion '(if true then false else true)' failed",
                "(if true then false else true)",
            ),
            (
                "assert \"a${toString 1}\" == \"b\"; 1",
                "string '\"a1\"' is not equal to string '\"b\"'",
                "((\"a\" + (toString 1)) == \"b\")",
            ),
            (
                "assert ./foo == ./bar; 1",
                "path '/foo' is not equal to path '/bar'",
                "(/foo == /bar)",
            ),
            (
                "assert 1.5 > 2.0; 1",
                "assertion '(__lessThan 2 1.5)' failed",
                "(__lessThan 2 1.5)",
            ),
            (
                "assert [ (1) \"a\" ] == []; 1",
                "list of size '2' is not equal to list of size '0', left hand side is '[ 1 \"a\" ]', right hand side is '[ ]'",
                "([ (1) (\"a\") ] == [ ])",
            ),
            (
                "assert { \"a b\" = 1; inherit ({x=1;}) x; } == {}; 1",
                "attribute names of attribute set '{ \"a b\" = 1; x = «thunk»; }' differs from attribute set '{ }'",
                "({ inherit ({ x = 1; }) x; \"a b\" = 1; } == { })",
            ),
            (
                "assert rec { a = 1; } == {}; 1",
                "attribute names of attribute set '{ a = 1; }' differs from attribute set '{ }'",
                "(rec { a = 1; } == { })",
            ),
            (
                "assert { a = 1; } // { b = 2; } == {}; 1",
                "attribute names of attribute set '{ a = 1; b = 2; }' differs from attribute set '{ }'",
                "(({ a = 1; } // { b = 2; }) == { })",
            ),
            (
                "assert [ 1 ] ++ [ 2 ] == []; 1",
                "list of size '2' is not equal to list of size '0', left hand side is '[ 1 2 ]', right hand side is '[ ]'",
                "(([ (1) ] ++ [ (2) ]) == [ ])",
            ),
            (
                "assert true -> false; 1",
                "assertion '(true -> false)' failed",
                "(true -> false)",
            ),
            (
                "assert false || false && true; 1",
                "assertion '(false || (false && true))' failed",
                "(false || (false && true))",
            ),
            (
                "assert builtins.isString 1; 1",
                "assertion '((builtins).isString 1)' failed",
                "((builtins).isString 1)",
            ),
            (
                "let f = x: y: false; in assert f 1 2; 1",
                "assertion '(f 1 2)' failed",
                "(f 1 2)",
            ),
            (
                "let f = x: y: false; in assert (f 1) 2; 1",
                "assertion '(f 1 2)' failed",
                "(f 1 2)",
            ),
            (
                "assert \"\\${x}\" == \"y\"; 1",
                "string '\"\\${x}\"' is not equal to string '\"y\"'",
                "(\"\\${x}\" == \"y\")",
            ),
            (
                "assert ''a${toString 1}b'' == \"\"; 1",
                "string '\"a1b\"' is not equal to string '\"\"'",
                "((\"a\" + (toString 1) + \"b\") == \"\")",
            ),
            (
                "assert { a.b = 1; a.c = 2; } == {}; 1",
                "attribute names of attribute set '{ a = «thunk»; }' differs from attribute set '{ }'",
                "({ a = { b = 1; c = 2; }; } == { })",
            ),
            (
                "assert { a = { b = 1; }; a.c = 2; } == {}; 1",
                "attribute names of attribute set '{ a = «thunk»; }' differs from attribute set '{ }'",
                "({ a = { b = 1; c = 2; }; } == { })",
            ),
            (
                "assert [ 1 2 3 ] == [ 1 2 4 ]; 1",
                "an integer with value '3' is not equal to an integer with value '4'",
                "([ (1) (2) (3) ] == [ (1) (2) (4) ])",
            ),
            (
                "assert { a.b = 1; } == { a.b = 2; }; 1",
                "an integer with value '1' is not equal to an integer with value '2'",
                "({ a = { b = 1; }; } == { a = { b = 2; }; })",
            ),
            (
                "let x = 1; in assert x == 2.5; 1",
                "an integer with value '1' is not equal to a float with value '2.5'",
                "(x == 2.5)",
            ),
            (
                "assert null == false; 1",
                "null of value 'null' is not equal to a Boolean of value 'false'",
                "(null == false)",
            ),
            (
                "let f = x: x; in assert [ f 1 ] == [ f 2 ]; 1",
                "an integer with value '1' is not equal to an integer with value '2'",
                "([ (f) (1) ] == [ (f) (2) ])",
            ),
            (
                "let f = x: x; in assert { a = f; b = 1; } == { a = f; b = 2; }; 1",
                "an integer with value '1' is not equal to an integer with value '2'",
                "({ a = f; b = 1; } == { a = f; b = 2; })",
            ),
            (
                "let f = x: x; in assert [ (f f) ] == [ (f f) ]; 1",
                "distinct functions and immediate comparisons of identical functions compare as unequal",
                "([ ((f f)) ] == [ ((f f)) ])",
            ),
            (
                "assert ({ }: false) {}; 1",
                "assertion '(({  }: false) { })' failed",
                "(({  }: false) { })",
            ),
            (
                "assert ({ ... }: false) {}; 1",
                "assertion '(({ ... }: false) { })' failed",
                "(({ ... }: false) { })",
            ),
        ];
        let mut failed = Vec::new();
        for (expr, msg, text) in cases {
            let mut ev = Evaluator::new();
            let e = ev.eval_string(expr, b"/").expect_err(expr);
            let in_trace = format!("while evaluating the condition of the assertion '{text}'");
            let named = e.msg == format!("assertion '{text}' failed")
                || e.trace.iter().any(|(_, t)| *t == in_trace);
            if e.msg != *msg || !named {
                failed.push(format!(
                    "{expr}\n  nix:  {msg} [{text}]\n  iets: {} {:?}",
                    e.msg, e.trace
                ));
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }
}
