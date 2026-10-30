use super::allow::bit;
use super::ast::{Ast, Expr, ExprId, FileId, Note, StrPart};
use super::dump::{dump, json, tree};
use super::interner::Interner;
use super::lints::LintId;
use super::{ParseError, line_col, parse};
use crate::sys::heap::Heap;
use rstest::rstest;
use serde_json::Value;
use std::sync::Arc;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread::Builder;

fn fresh() -> Interner {
    Interner::in_heap(Arc::new(Heap::default()))
}

fn dump_src(src: &str) -> String {
    let mut interner = fresh();
    let ast = parse(src.as_bytes(), FileId(0), &mut interner)
        .unwrap_or_else(|e| panic!("parse of {src:?} failed: {}", e.msg));
    dump(&ast, &interner)
}

fn parse_error(src: &str) -> ParseError {
    let mut interner = fresh();
    match parse(src.as_bytes(), FileId(0), &mut interner) {
        Ok(ast) => panic!(
            "expected parse error for {src:?}, got {}",
            dump(&ast, &interner)
        ),
        Err(e) => e,
    }
}

fn parse_err(src: &str) -> String {
    parse_error(src).msg
}

fn str_value(src: &str) -> String {
    let d = dump_src(src);
    let inner = d
        .strip_prefix("(str ")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("not a plain string dump: {d}"));
    inner.to_string()
}

/// Scope errors that Nix reports from its resolver: iets reports them in
/// `compile`.
const NOT_PARSER_ERRORS: &[&str] = &[
    "parse-fail-undef-var.nix",
    "parse-fail-regression-20060610.nix",
];

/// Nix rejects these in its parser. The corpus files them under eval-fail
/// because `nix-instantiate --eval` is what fails.
const PARSER_ERRORS: &[&str] = &[
    "eval-fail-dynamic-attrs-inherit.nix",
    "eval-fail-dynamic-attrs-inherit-2.nix",
    "eval-fail-dynamic-attrs-let.nix",
    "eval-fail-dynamic-attrs-let-2.nix",
    "eval-fail-dynamic-attrs-let-3.nix",
    "eval-fail-path-slash.nix",
    "eval-fail-string-nul-1.nix",
    "eval-fail-string-nul-2.nix",
];

// a new file shows up after `touch src/syntax/tests.rs`
#[rstest]
fn lang_corpus(#[files("tests/lang/*.nix")] path: PathBuf) {
    let name = path.file_name().unwrap().to_str().unwrap();
    let src = fs::read(&path).unwrap();
    let result = parse(&src, FileId(0), &mut fresh());
    let must_fail = (name.starts_with("parse-fail-") && !NOT_PARSER_ERRORS.contains(&name))
        || PARSER_ERRORS.contains(&name);
    match (result, must_fail) {
        (Ok(_), true) => panic!("{name}: parsed but should fail"),
        (Err(e), false) => {
            let (l, c) = line_col(&src, e.span.start);
            panic!("{name}:{l}:{c}: {}", e.msg);
        }
        _ => {}
    }
}

#[test]
fn indented_string_stripping() {
    assert_eq!(str_value("''\n  foo\n   bar\n  ''"), r#""foo\n bar\n""#);
    assert_eq!(str_value("''\n    a\n  $\n  b''"), r#""  a\n$\nb""#);
    assert_eq!(str_value("''\n  a'$''"), r#""a'$""#);
    assert_eq!(str_value("''   \n  x''"), r#""x""#);
    assert_eq!(str_value("''  a\n  '''\n  ''"), r#""a\n''\n""#);
    assert_eq!(str_value("''\n  foo''\\\n  bar''"), r#""foo\nbar""#);
    assert_eq!(str_value("''\n  a\n\n  b\n''"), r#""a\n\nb\n""#);
    assert_eq!(str_value("''''"), r#""""#);
    assert_eq!(str_value("''\n  ''"), r#""""#);
    assert_eq!(str_value("'' a$ ''"), r#""a$ ""#);
    assert_eq!(str_value("''$${x}''"), r#""$${x}""#);
    assert_eq!(str_value("''''\\n''"), r#""\n""#);
    assert_eq!(str_value("''  a\r\n  b''"), r#""a\r\nb""#);
    assert_eq!(
        dump_src("''  ${\"a\"}\n  x''"),
        r#"(str (interp (str "a")) "\nx")"#
    );
    assert_eq!(
        dump_src("''\n  x = ${x};\n    y\n''"),
        r#"(str "x = " (interp x) ";\n  y\n")"#
    );
}

#[test]
fn string_escapes_and_interpolation() {
    assert_eq!(
        str_value(r#""a\nb\tc\\d\$e\"f\qg""#),
        r#""a\nb\tc\\d$e\"fqg""#
    );
    assert_eq!(str_value("\"a\r\nb\rc\""), r#""a\nb\nc""#);
    assert_eq!(str_value(r#""$${x}""#), r#""$${x}""#);
    assert_eq!(str_value(r#""$$""#), r#""$$""#);
    assert_eq!(str_value(r#""$""#), r#""$""#);
    assert_eq!(str_value(r#""""#), r#""""#);
    assert_eq!(str_value(r#""$\n""#), "\"$\\n\"");
    assert_eq!(dump_src(r#""a${b}c""#), r#"(str "a" (interp b) "c")"#);
    assert_eq!(dump_src(r#""${b}""#), r#"(str (interp b))"#);
    assert_eq!(dump_src(r#""${"x"}""#), r#"(str (interp (str "x")))"#);
    assert_eq!(
        dump_src(r#""a${ "b${c}" }""#),
        r#"(str "a" (interp (str "b" (interp c))))"#
    );
    assert_eq!(dump_src("x:y"), r#"(str "x:y")"#);
    assert_eq!(
        dump_src("https://a.b/c?d=e"),
        r#"(str "https://a.b/c?d=e")"#
    );
    assert!(parse_err("\"abc").contains("end of file"));
    assert!(parse_err("a 1\"$").contains("end of file"));
}

#[test]
fn paths() {
    assert_eq!(dump_src("./foo"), r#"(path rel "./foo")"#);
    assert_eq!(dump_src("../foo/bar.nix"), r#"(path rel "../foo/bar.nix")"#);
    assert_eq!(dump_src("/foo"), r#"(path abs "/foo")"#);
    assert_eq!(dump_src("~/foo"), r#"(path home "/foo")"#);
    assert_eq!(dump_src("<nixpkgs/lib>"), r#"(path search "nixpkgs/lib")"#);
    assert_eq!(dump_src("./foo/${x}"), r#"(path rel "./foo/" (interp x))"#);
    assert_eq!(
        dump_src("./${x}/y${z}.nix"),
        r#"(path rel "./" (interp x) "/y" (interp z) ".nix")"#
    );
    assert_eq!(dump_src("~/${x}"), r#"(path home "/" (interp x))"#);
    assert_eq!(dump_src("/${x}"), r#"(path abs "/" (interp x))"#);
    assert_eq!(dump_src("x/${a}b"), r#"(path rel "x/" (interp a) "b")"#);
    assert_eq!(dump_src("2/3"), r#"(path rel "2/3")"#);
    assert_eq!(dump_src("a / b"), "(/ a b)");
    assert_eq!(dump_src("a // b"), "(// a b)");
    assert!(parse_err("a < b > c").contains("unexpected '>'"));
    assert_eq!(dump_src("a <b> c"), r#"(apply a (path search "b") c)"#);
    assert!(parse_err("./foo/").contains("trailing slash"));
    assert!(parse_err("/foo/${x}/").contains("trailing slash"));
}

#[test]
fn numbers() {
    assert_eq!(dump_src("007"), "7");
    assert_eq!(dump_src("1."), "1.0");
    assert_eq!(dump_src(".5"), "0.5");
    assert_eq!(dump_src("1.5e3"), "1500.0");
    assert_eq!(dump_src("0.5e-1"), "0.05");
    assert_eq!(dump_src("-1"), "-1");
    assert_eq!(dump_src("- 1"), "-1");
    assert_eq!(dump_src("- - 1"), "1");
    assert_eq!(dump_src("-1.5"), "-1.5");
    assert_eq!(dump_src("-0.0"), "0.0");
    assert_eq!(dump_src("-x"), "(neg x)");
    assert_eq!(dump_src("x.5"), "(apply x 0.5)");
    assert_eq!(dump_src("1e3"), "(apply 1 e3)");
    assert_eq!(dump_src("9223372036854775807"), "9223372036854775807");
    assert!(parse_err("9223372036854775808").contains("invalid integer"));
    assert!(parse_err("-9223372036854775808").contains("invalid integer"));
}

#[test]
fn operator_precedence() {
    assert_eq!(dump_src("a -> b -> c"), "(-> a (-> b c))");
    assert_eq!(dump_src("a || b && c"), "(|| a (&& b c))");
    assert_eq!(dump_src("a && b || c"), "(|| (&& a b) c)");
    assert_eq!(dump_src("a == b && c"), "(&& (== a b) c)");
    assert_eq!(dump_src("a < b == c"), "(== (< a b) c)");
    assert_eq!(dump_src("a // b // c"), "(// a (// b c))");
    assert_eq!(dump_src("a // b + c"), "(// a (+ b c))");
    assert_eq!(dump_src("!a + b"), "(! (+ a b))");
    assert_eq!(dump_src("!a // b"), "(// (! a) b)");
    assert_eq!(dump_src("!a == b"), "(== (! a) b)");
    assert_eq!(dump_src("-a + b"), "(+ (neg a) b)");
    assert_eq!(dump_src("-a * b"), "(* (neg a) b)");
    assert_eq!(dump_src("a - b - c"), "(- (- a b) c)");
    assert_eq!(dump_src("a + b * c"), "(+ a (* b c))");
    assert_eq!(dump_src("a * b ++ c"), "(* a (++ b c))");
    assert_eq!(dump_src("a ++ b ++ c"), "(++ a (++ b c))");
    assert_eq!(dump_src("a ++ b ? c"), "(++ a (? b (c)))");
    assert_eq!(dump_src("!a ? b"), "(! (? a (b)))");
    assert_eq!(dump_src("-a ? b"), "(? (neg a) (b))");
    assert_eq!(dump_src("a ? b.c"), "(? a (b c))");
    assert_eq!(dump_src("a ? b == c"), "(== (? a (b)) c)");
    assert_eq!(dump_src("f a b"), "(apply f a b)");
    assert_eq!(dump_src("(f a) b"), "(apply f a b)");
    assert_eq!(dump_src("f a.b c"), "(apply f (select a (b)) c)");
    assert_eq!(dump_src("-f a"), "(neg (apply f a))");
    assert_eq!(dump_src("f -a"), "(- f a)");
    assert_eq!(dump_src("a.b or c d"), "(apply (select a (b) (or c)) d)");
    assert_eq!(dump_src("f or x"), "(apply f or x)");
    assert_eq!(dump_src("a |> f |> g"), "(apply g (apply f a))");
    assert_eq!(dump_src("g <| f <| a"), "(apply g (apply f a))");
    assert_eq!(dump_src("a |> f x"), "(apply f x a)");
    assert_eq!(dump_src("if a then b else c: d"), "(if a b (lambda c d))");
    assert_eq!(dump_src("with a; b: c"), "(with a (lambda b c))");
    assert_eq!(dump_src("assert a; b"), "(assert a b)");
    assert!(parse_err("a == b == c").contains("unexpected '=='"));
    assert!(parse_err("a < b < c").contains("unexpected '<'"));
    assert_eq!(dump_src("a ? b ? c"), "(? (? a (b)) (c))");
    assert!(parse_err("a |> f <| b").contains("unexpected"));
    assert!(parse_err("[ -1 ]").contains("unexpected '-'"));
    assert!(parse_err("1 != != 2").contains("unexpected '!='"));
}

#[test]
fn attrs_and_bindings() {
    assert_eq!(dump_src("{ }"), "(attrs)");
    assert_eq!(dump_src("rec { a = 1; }"), "(rec-attrs (= (a) 1))");
    assert_eq!(
        dump_src("{ a.b.\"c\".${d} = 1; }"),
        "(attrs (= (a b c (dyn d)) 1))"
    );
    assert_eq!(dump_src("{ ${\"x\"} = 1; }"), "(attrs (= (x) 1))");
    assert_eq!(
        dump_src("{ \"a${b}\" = 1; }"),
        r#"(attrs (= ((dyn (str "a" (interp b)))) 1))"#
    );
    assert_eq!(
        dump_src("{ or = 1; }.or"),
        "(select (attrs (= (or) 1)) (or))"
    );
    assert_eq!(
        dump_src("{ inherit a b; inherit (c) d \"e\"; }"),
        "(attrs (inherit a b) (inherit (from c) d e))"
    );
    assert_eq!(dump_src("let a = 1; in a"), "(let ( (= (a) 1)) a)");
    assert_eq!(
        dump_src("let { body = 1; }"),
        "(select (rec-attrs (= (body) 1)) (body))"
    );
    assert_eq!(
        dump_src("let a.${b} = 1; in a"),
        "(let ( (= (a (dyn b)) 1)) a)"
    );
    assert_eq!(
        dump_src("{ a = 1; b = { c = 2; }; b.d = 3; }"),
        "(attrs (= (a) 1) (= (b) (attrs (= (c) 2))) (= (b d) 3))"
    );
    assert!(parse_err("let ${b} = 1; in a").contains("dynamic attributes not allowed in let"));
    assert!(parse_err("{ inherit ${b}; }").contains("dynamic attributes not allowed in inherit"));
    assert!(parse_err("{ a = 1; a = 2; }").contains("attribute 'a' already defined"));
    assert!(parse_err("{ a.b = 1; a.b = 2; }").contains("attribute 'a.b' already defined"));
    assert!(parse_err("{ a.b = 1; a = { b = 2; }; }").contains("attribute 'a.b' already defined"));
    assert!(parse_err("{ a = { b = 2; }; a.b = 1; }").contains("attribute 'a.b' already defined"));
    assert!(parse_err("{ inherit a; a = 1; }").contains("already defined"));
    assert!(parse_err("{ inherit a a; }").contains("already defined"));
    assert!(parse_err("{ a = 1; inherit (x) a; }").contains("already defined"));
    assert!(parse_err("{ a.b = 1; inherit a; }").contains("already defined"));
    assert!(parse_err("{ a = 1; }; }").contains("expecting end of file"));
    assert!(parse_err("{ a ... }").contains("expecting '.' or '='"));
    assert!(parse_err("{ 1 = x; }").contains("unexpected integer"));
}

#[test]
fn lambdas() {
    assert_eq!(dump_src("x: x"), "(lambda x x)");
    assert_eq!(dump_src("{ }: x"), "(lambda {} x)");
    assert_eq!(dump_src("{ ... }: x"), "(lambda {...} x)");
    assert_eq!(
        dump_src("{ a, b ? 1, ... }: a"),
        "(lambda {a, b ? 1, ...} a)"
    );
    assert_eq!(dump_src("{ a, }: a"), "(lambda {a} a)");
    assert_eq!(dump_src("x@{ a }: a"), "(lambda {a} @ x a)");
    assert_eq!(dump_src("{ a }@x: a"), "(lambda {a} @ x a)");
    assert_eq!(dump_src("x: y: z"), "(lambda x (lambda y z))");
    assert!(parse_err("{ ..., a }: a").contains("expecting '}'"));
    assert!(parse_err("{ a, a }: a").contains("duplicate formal function argument 'a'"));
    assert!(parse_err("a@{ a }: a").contains("duplicate formal function argument 'a'"));
    assert!(parse_err("{ a b }: a").contains("expecting '.' or '='"));
}

#[test]
fn comments_and_misc() {
    assert_eq!(dump_src("# c\n1 /* x */ # y\n"), "1");
    assert_eq!(dump_src("/** doc */ x: x"), "(lambda x x)");
    assert_eq!(dump_src("{ x = # c\ry; }"), "(attrs (= (x) y))");
    assert_eq!(
        dump_src("[ a b.c (d e) ]"),
        "(list a (select b (c)) (apply d e))"
    );
    assert_eq!(dump_src("__curPos"), "__curPos");
    assert!(parse_err("é").contains("invalid token"));
    assert!(parse_err("(").contains("end of file"));
    assert!(parse_err("a.'b").contains("invalid token"));
    assert!(parse_err("").contains("end of file"));
}

#[test]
fn line_col_counts_bytes() {
    let src = b"ab\ncd\xc3\xa9f\n";
    assert_eq!(line_col(src, 0), (1, 1));
    assert_eq!(line_col(src, 2), (1, 3));
    assert_eq!(line_col(src, 3), (2, 1));
    assert_eq!(line_col(src, 7), (2, 5));
    assert_eq!(line_col(src, 9), (3, 1));
}

#[test]
fn a_line_ends_at_cr_lf_and_lone_cr() {
    let src = b"a\rb\r\nc\n\rd";
    assert_eq!(line_col(src, 2), (2, 1));
    assert_eq!(line_col(src, 4), (2, 3));
    assert_eq!(line_col(src, 5), (3, 1));
    assert_eq!(line_col(src, 7), (4, 1));
    assert_eq!(line_col(src, 8), (5, 1));
    let e = parse_error("l\r =");
    assert_eq!(line_col(b"l\r =", e.span.start), (2, 2));
}

#[test]
fn allow_comments() {
    let parse_allows = |src: &str| {
        let mut interner = fresh();
        let ast = parse(src.as_bytes(), FileId(0), &mut interner).unwrap();
        (ast.allows.clone(), ast.notes.clone())
    };
    // own line: the next non-blank, non-comment line; trailing: its own line
    let src =
        "# iets: allow fod\n\n# plain\n  with a; [\n  (with b; x) # iets: allow fod, ifd\n]\n";
    let (a, notes) = parse_allows(src);
    assert!(notes.is_empty());
    let l = a.lines();
    assert_eq!(l.len(), 2);
    assert_eq!(
        (l[0].start, l[0].end, l[0].mask),
        (27, 38, bit(LintId::Fod))
    );
    assert_eq!((l[1].start, l[1].end), (39, 75));
    assert_eq!(l[1].mask, bit(LintId::Fod) | bit(LintId::Ifd));
    assert!(a.covers(30, LintId::Fod));
    assert!(!a.covers(38, LintId::Fod));
    assert!(!a.covers(30, LintId::Ifd));
    assert!(a.covers(45, LintId::Ifd));
    // a comment before the third token of lookahead is noted once
    let (a, _) = parse_allows("{\n# iets: allow fod\nb = with x; y; }");
    assert_eq!(a.lines().len(), 1);
    assert_eq!((a.lines()[0].start, a.lines()[0].end), (20, 36));
    // an own-line comment at the end of the file covers nothing
    let (a, _) = parse_allows("1\n# iets: allow fod\n");
    assert!(a.is_empty());
    // unknown names are notes; the known ones still count
    let (a, notes) = parse_allows("# iets: allow fdo, fod\n1");
    assert_eq!(a.lines()[0].mask, bit(LintId::Fod));
    assert!(matches!(notes[..], [Note::UnknownAllow { at: 14, .. }]));
    let (a, notes) = parse_allows("1 # iets: allo fod");
    assert!(a.is_empty());
    assert!(matches!(notes[..], [Note::UnknownAllow { at: 10, .. }]));
    // other comments are not parsed
    let (a, notes) = parse_allows("# iets allow fod\n#iets: allow fod\n1");
    assert!(a.is_empty() && notes.is_empty());
}

#[test]
fn spans_cover_tokens() {
    let src = "  f a.b  ";
    let mut interner = fresh();
    let ast = parse(src.as_bytes(), FileId(0), &mut interner).unwrap();
    let root = ast.span(ast.root);
    assert_eq!((root.start, root.end), (2, 7));
}

/// Value of a parsed string whose interpolations are all literal strings.
fn literal_value(ast: &Ast, id: ExprId) -> Option<Vec<u8>> {
    let Expr::Str(parts) = ast.expr(id) else {
        return None;
    };
    let mut out = Vec::new();
    for p in ast.parts(*parts) {
        match p {
            StrPart::Lit(l) => out.extend_from_slice(ast.bytes(*l)),
            StrPart::Interp(e) => out.extend_from_slice(&literal_value(ast, *e)?),
        }
    }
    Some(out)
}

fn nix_eval_raw(expr: &str) -> Option<Vec<u8>> {
    let out = Command::new("nix")
        .args([
            "eval",
            "--extra-experimental-features",
            "nix-command",
            "--raw",
            "--expr",
            expr,
        ])
        .output()
        .expect("nix on PATH");
    if out.status.success() {
        Some(out.stdout)
    } else {
        None
    }
}

/// Random string literals, compared byte for byte with the reference Nix.
/// Run with `cargo test -- --ignored`.
#[test]
#[ignore = "needs nix on PATH and takes a few seconds"]
fn differential_strings_against_nix() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut rng = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    let ind_alpha = [
        " ", "\n", "a", "$", "'", "\\", "{", "}", "n", "\r", "${\"X\"}", "  ", "''$", "'''",
        "''\\n", "\n  ", "\n ",
    ];
    let dq_alpha = [
        " ", "\n", "a", "$", "\\", "\"", "{", "}", "n", "\r", "${\"X\"}", "\\$", "\\n", "$$",
        "\\\\", "\r\n",
    ];
    let mut samples = Vec::new();
    for _ in 0..3000 {
        let len = 1 + rng(10);
        let mut s = String::from("''");
        for _ in 0..len {
            s.push_str(ind_alpha[rng(ind_alpha.len())]);
        }
        s.push_str("''");
        samples.push(s);
    }
    for _ in 0..3000 {
        let len = 1 + rng(10);
        let mut s = String::from("\"");
        for _ in 0..len {
            s.push_str(dq_alpha[rng(dq_alpha.len())]);
        }
        s.push('"');
        samples.push(s);
    }

    let mut accepted: Vec<(String, Vec<u8>)> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    for s in samples {
        let mut interner = fresh();
        match parse(s.as_bytes(), FileId(0), &mut interner) {
            Ok(ast) => {
                let Some(v) = literal_value(&ast, ast.root) else {
                    continue;
                };
                accepted.push((s, v));
            }
            Err(_) => rejected.push(s),
        }
    }

    let expr = format!(
        "builtins.concatStringsSep \"\\t\" [ {} ]",
        accepted
            .iter()
            .map(|(s, _)| format!("({s})"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut mismatches = Vec::new();
    match nix_eval_raw(&expr) {
        Some(out) => {
            let got: Vec<&[u8]> = out.split(|&b| b == b'\t').collect();
            assert_eq!(got.len(), accepted.len(), "separator count differs");
            for ((s, mine), theirs) in accepted.iter().zip(got) {
                if mine.as_slice() != theirs {
                    mismatches.push(format!(
                        "{s:?}: iets {:?} nix {:?}",
                        String::from_utf8_lossy(mine),
                        String::from_utf8_lossy(theirs)
                    ));
                }
            }
        }
        None => {
            for (s, _) in &accepted {
                if nix_eval_raw(s).is_none() {
                    mismatches.push(format!("{s:?}: iets accepts, nix rejects"));
                }
            }
        }
    }
    for s in rejected.iter().take(150) {
        if nix_eval_raw(s).is_some() {
            mismatches.push(format!("{s:?}: iets rejects, nix accepts"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} accepted, {} rejected, mismatches:\n{}",
        accepted.len(),
        rejected.len(),
        mismatches.join("\n")
    );
}

#[test]
fn nul_in_string_literals() {
    let cases = [
        ("\"foo\0bar\"", "foo␀bar", (1, 2)),
        ("''\n  foo\n  \0\n  bar\n''", "  foo\n  ␀\n  bar\n", (2, 1)),
        ("let x = \"q\"; in \"a${x}\0b\"", "␀b", (1, 23)),
        ("''x${\"y\"}\0z''", "␀z", (1, 10)),
        ("let y = \"a\0b\"; in 1", "a␀b", (1, 10)),
        ("{ \"a\0b\" = 1; }", "a␀b", (1, 4)),
        ("\"a\\n\0b\"", "a\n␀b", (1, 2)),
        ("\"a\r\n\0b\"", "a\n␀b", (1, 2)),
        ("\"$\0\"", "$␀", (1, 2)),
    ];
    for (src, shown, pos) in cases {
        let e = parse_error(src);
        assert_eq!(
            (e.msg, line_col(src.as_bytes(), e.span.start)),
            (
                format!(
                    "input string '{shown}' cannot be represented as Nix string because it contains null bytes"
                ),
                pos
            ),
            "{src:?}"
        );
    }
    // escaped NULs pass, as in Nix
    dump_src("\"a\\\0b\"");
    dump_src("''a''\\\0b''");
}

#[test]
fn end_of_file_inside_a_path_is_at_its_last_token() {
    let cases = [
        ("[ ./a", (1, 3)),
        ("[ ~/a", (1, 3)),
        ("[ ./a${b}", (1, 9)),
        ("[ ./a${b}c", (1, 10)),
        ("( ./a", (1, 3)),
        ("/a/", (1, 1)),
        ("/a/${b}/", (1, 8)),
        ("[ ./a/${b}/", (1, 11)),
        // a byte after the path is a token of its own, and the end is there
        ("[ ./a ", (1, 7)),
        ("./a/ ", (1, 5)),
    ];
    for (src, pos) in cases {
        let e = parse_error(src);
        assert_eq!(line_col(src.as_bytes(), e.span.start), pos, "{src:?}");
    }
}

#[test]
fn the_error_reported_is_the_one_nix_meets_first() {
    let cases = [
        ("/a//0", "unexpected end of path", (1, 1)),
        ("/a//0 ", "unexpected end of path", (1, 4)),
        ("x/a//0//1", "unexpected string", (1, 8)),
        ("{ a, a }: (", "unexpected end of file", (1, 12)),
        (
            "{ x } @ x: 1",
            "duplicate formal function argument 'x'",
            (1, 1),
        ),
        (
            "{ a = 1; a = 2; b = }",
            "attribute 'a' already defined at 1:3",
            (1, 10),
        ),
        (
            "let a.b = 1; a = 2; in",
            "attribute 'a' already defined at 1:5",
            (1, 14),
        ),
        (
            "let ${a} = 1; in 1",
            "dynamic attributes not allowed in let",
            (1, 1),
        ),
        ("let ${a} = 1; in (", "unexpected end of file", (1, 19)),
        (
            "{l,l}:v|>l<|",
            "duplicate formal function argument 'l'",
            (1, 4),
        ),
        ("(a <| b |> c)", "unexpected '|>', expecting ')'", (1, 9)),
        (
            "let x = a |> b <| c; in x",
            "unexpected '<|', expecting ';'",
            (1, 16),
        ),
    ];
    for (src, msg, pos) in cases {
        let e = parse_error(src);
        assert!(e.msg.contains(msg), "{src:?}: {}", e.msg);
        assert_eq!(line_col(src.as_bytes(), e.span.start), pos, "{src:?}");
    }
}

#[test]
fn an_interpolated_name_is_static_when_nix_has_one_string_node() {
    assert_eq!(dump_src("r.${''${\"t\"}''}"), "(select r (t))");
    assert_eq!(dump_src("r.${'' ${\"t\"}''}"), "(select r (t))");
    assert_eq!(
        dump_src("r.${'''''t''}"),
        r#"(select r ((dyn (str "''t"))))"#
    );
    assert_eq!(
        dump_src("rec { ${''${\"a\"}''} = 1; b = a; }"),
        "(rec-attrs (= (a) 1) (= (b) a))"
    );
}

#[test]
fn json_gives_every_node_its_kind_and_span() {
    let json_of = |src: &str| {
        let mut interner = fresh();
        let ast = parse(src.as_bytes(), FileId(0), &mut interner).unwrap();
        let mut out = String::new();
        json(&tree(&ast, &interner), &mut out);
        out
    };
    assert_eq!(
        json_of("a: a.b or 1"),
        r#"{"kind":"lambda","start":0,"end":11,"arg":"a","formals":null,"ellipsis":false,"body":{"kind":"select","start":3,"end":11,"subject":{"kind":"var","start":3,"end":4,"name":"a"},"path":["b"],"default":{"kind":"int","start":10,"end":11,"value":1}}}"#
    );
    fn walk(v: &Value, src: &str, nodes: &mut usize) {
        match v {
            Value::Object(o) => {
                if let Some(kind) = o.get("kind") {
                    let at = |k: &str| o[k].as_u64().expect("an offset") as usize;
                    let (start, end) = (at("start"), at("end"));
                    assert!(start < end && end <= src.len(), "{kind} {start}..{end}");
                    if kind == "var" || kind == "name" {
                        assert_eq!(o["name"], src[start..end]);
                    }
                    *nodes += 1;
                }
                o.values().for_each(|v| walk(v, src, nodes));
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, src, nodes)),
            _ => {}
        }
    }
    let src =
        "{ x ? 1.5, ... }@s: with s; [ ./p \"t${x}\" ] // { inherit (s) y; \"k\".${x} = !x; }";
    let mut nodes = 0;
    walk(
        &serde_json::from_str(&json_of(src)).unwrap(),
        src,
        &mut nodes,
    );
    assert_eq!(nodes, 18);
    assert_eq!(
        json_of("\"\\\"\u{e9}\""),
        r#"{"kind":"string","start":0,"end":6,"parts":["\"é"]}"#
    );
}

/// A deeply nested file must give an error, never a stack overflow. The
/// parse runs on a big stack like `main.rs` gives it.
#[test]
fn deep_nesting_ends_with_an_error() {
    Builder::new()
        .stack_size(1 << 30)
        .spawn(deep_nesting_cases)
        .unwrap()
        .join()
        .unwrap();
}

fn deep_nesting_cases() {
    for (open, mid, close) in [
        ("(", "1", ")"),
        ("[", "", "]"),
        ("{a=", "1", ";}"),
        ("x: ", "1", ""),
        ("-", "1", ""),
        ("!", "true", ""),
    ] {
        let n = 60_000;
        let mut src = String::with_capacity(n * 4);
        for _ in 0..n {
            src.push_str(open);
        }
        src.push_str(mid);
        for _ in 0..n {
            src.push_str(close);
        }
        assert_eq!(
            parse_err(&src),
            "stack overflow (possible infinite recursion)",
            "for {open:?}"
        );
    }
}
