//! The byte span of a finding, and the edit when the lint has one
//! (docs/reference/errors.md). A site is the offset where a node starts,
//! and the IR keeps nothing more. Nodes share a start when one is the first
//! operand of the next (`a`, `a.b`, `a.b c`, `a.b c == d`), so the span is
//! that of the innermost node there of a kind the lint reports at.

use super::LintId;
use crate::syntax::ast::{Ast, BinOp, Expr, Span, StrPart};

type Kind<'a> = &'a dyn Fn(&Expr) -> bool;

/// The span of the site of `id` at offset `off` of `src`, which parsed
/// into `ast`, and the text that replaces it.
pub fn finding(src: &[u8], ast: &Ast, id: LintId, off: u32) -> Option<(Span, Option<String>)> {
    let interp = |e: &Expr| {
        let has = |p: &StrPart| matches!(p, StrPart::Interp(_));
        matches!(e, Expr::Str(p) if ast.parts(*p).iter().any(has))
    };
    let add = |e: &Expr| matches!(e, Expr::Binary { op: BinOp::Add, .. });
    let apply = |e: &Expr| matches!(e, Expr::Apply { .. });
    let coercion = |e: &Expr| interp(e) || add(e) || apply(e);
    let path = |e: &Expr| matches!(e, Expr::Path(_));
    let string = |e: &Expr| matches!(e, Expr::Str(_));
    let eq = |e: &Expr| {
        matches!(
            e,
            Expr::Binary {
                op: BinOp::Eq | BinOp::Ne,
                ..
            }
        )
    };
    let or = |e: &Expr| {
        matches!(
            e,
            Expr::Select {
                default: Some(_),
                ..
            }
        )
    };
    let select = |e: &Expr| matches!(e, Expr::Select { .. });
    let var = |e: &Expr| matches!(e, Expr::Var(_));
    // first choice first; `true` when a binding at the site goes before them
    let (binds, kinds): (bool, &[Kind]) = match id {
        LintId::AbsolutePathLiteral | LintId::ShortPathLiteral => (false, &[&path]),
        LintId::UrlLiteral => (false, &[&string]),
        LintId::EqTypeMismatch | LintId::EqFunction => (false, &[&eq]),
        LintId::OrNonAttrs => (false, &[&or]),
        LintId::ImpureBuiltin => (false, &[&apply, &select, &var]),
        LintId::UnhashedFetch | LintId::Ifd | LintId::IfdLocalOutput | LintId::ReadThroughCopy => {
            (false, &[&apply])
        }
        // `inherit (self) revCount`, then `getAttr "revCount" self`: the call
        // starts where its `builtins.getAttr` starts
        LintId::RevCount => (true, &[&apply, &select]),
        LintId::UnknownAllow => {
            let word = src.get(off as usize..)?;
            let len = word.iter().take_while(|b| !b" \t,\r\n".contains(b)).count();
            let end = off + len as u32;
            return (len > 0).then_some((Span { start: off, end }, None));
        }
        _ => (true, &[&coercion]),
    };
    let at = |s: &&Span| s.start == off;
    let binding = || {
        let names = ast.names.iter().map(|(_, s)| s);
        names.chain(ast.binds.iter().map(|b| &b.span)).find(at)
    };
    let node = |kind: &Kind| {
        let nodes = ast.exprs.iter().zip(&ast.spans);
        let here = nodes.filter(|(e, s)| at(s) && kind(e)).map(|(_, s)| s);
        here.min_by_key(|s| s.end)
    };
    let span = binds.then(binding).flatten();
    let span = *span.or_else(|| kinds.iter().find_map(node))?;
    let text = String::from_utf8_lossy(src.get(span.start as usize..span.end as usize)?);
    let edit = match id {
        LintId::UrlLiteral if !text.starts_with(['"', '\'']) => Some(format!("\"{text}\"")),
        LintId::ShortPathLiteral => Some(format!("./{text}")),
        _ => None,
    };
    Some((span, edit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax;
    use crate::syntax::ast::FileId;

    fn at(src: &str, id: LintId, site: &str) -> Option<(String, Option<String>)> {
        let ast = syntax::parse(
            src.as_bytes(),
            FileId(0),
            &mut crate::session::Session::current().interner(),
        )
        .unwrap();
        let off = src.find(site).unwrap() as u32;
        let (s, edit) = finding(src.as_bytes(), &ast, id, off)?;
        Some((src[s.start as usize..s.end as usize].to_string(), edit))
    }

    #[test]
    fn the_span_is_the_innermost_node_of_the_kind_of_the_lint() {
        let span = |src, id, site| at(src, id, site).map(|(s, _)| s);
        let src = "a.b == 1 && c";
        assert_eq!(
            span(src, LintId::EqTypeMismatch, "a"),
            Some("a.b == 1".into())
        );
        let src = "x: x.a or 1 + 2";
        assert_eq!(
            span(src, LintId::OrNonAttrs, "x.a"),
            Some("x.a or 1".into())
        );
        let src = "builtins.getEnv \"A\" == builtins.currentTime";
        let id = LintId::ImpureBuiltin;
        assert_eq!(
            span(src, id, "builtins"),
            Some("builtins.getEnv \"A\"".into())
        );
        assert_eq!(
            span(src, id, "builtins.c"),
            Some("builtins.currentTime".into())
        );
        let src = "{ src = \"a\" + ./b + \"c\"; inherit d; e = \"${./f}\" + \"g\"; }";
        let id = LintId::PathCopy;
        assert_eq!(span(src, id, "\"a\""), Some("\"a\" + ./b".into()));
        assert_eq!(span(src, id, "\"${"), Some("\"${./f}\"".into()));
        assert_eq!(
            span(src, id, "src"),
            Some("src = \"a\" + ./b + \"c\";".into())
        );
        assert_eq!(span(src, LintId::DrvList, "d;"), Some("d".into()));
        assert_eq!(span(src, id, "= \"a\""), None);
        let src = "# iets: allow nope, fod\n1";
        assert_eq!(span(src, LintId::UnknownAllow, "nope"), Some("nope".into()));
    }

    #[test]
    fn a_literal_lint_of_nix_has_an_edit() {
        let url = "[ http://a.b/c?d=1 \"http://e.f\" ]";
        let edit = Some("\"http://a.b/c?d=1\"".to_string());
        assert_eq!(
            at(url, LintId::UrlLiteral, "http"),
            Some(("http://a.b/c?d=1".into(), edit))
        );
        assert_eq!(
            at(url, LintId::UrlLiteral, "\"http"),
            Some(("\"http://e.f\"".into(), None))
        );
        let path = "a/b + c/${d}";
        assert_eq!(
            at(path, LintId::ShortPathLiteral, "a/b"),
            Some(("a/b".into(), Some("./a/b".into())))
        );
        assert_eq!(
            at(path, LintId::ShortPathLiteral, "c/"),
            Some(("c/${d}".into(), Some("./c/${d}".into())))
        );
        assert_eq!(
            at("/a", LintId::AbsolutePathLiteral, "/a"),
            Some(("/a".into(), None))
        );
    }
}
