use std::collections::HashSet;

use super::{EvalConfig, Evaluator};

use crate::print;

fn eval_str(src: &str) -> String {
    let mut ev = Evaluator::new();
    let v = ev.eval_string(src, b"/").expect("eval");
    let v = ev.force_deep(v).expect("force");
    let mut out = Vec::new();
    print::print_ambiguous(&mut ev, v, 1, &mut out, &mut HashSet::new(), 0).unwrap();
    String::from_utf8(out).unwrap()
}

fn eval_err(src: &str) -> String {
    let mut ev = Evaluator::new();
    match ev.eval_string(src, b"/").and_then(|v| ev.force_deep(v)) {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.msg,
    }
}

#[test]
fn arithmetic_and_strings() {
    assert_eq!(eval_str("1 + 2 * 3 - 4 / 2"), "5");
    assert_eq!(eval_str("1.5 + 1"), "2.5");
    assert_eq!(eval_str(r#""a" + "b" + toString 3"#), r#""ab3""#);
    assert_eq!(eval_str("builtins.stringLength \"héllo\""), "6");
    assert_eq!(eval_str("-(3 - 5)"), "2");
}

#[test]
fn laziness_and_recursion() {
    assert_eq!(eval_str("let x = throw \"no\"; in 1"), "1");
    assert_eq!(
        eval_str("let fib = n: if n < 2 then n else fib (n - 1) + fib (n - 2); in fib 15"),
        "610"
    );
    assert_eq!(eval_str("rec { a = b + 1; b = 1; }.a"), "2");
    assert_eq!(
        eval_err("let x = y; y = x; in x"),
        "infinite recursion encountered"
    );
}

#[test]
fn scoping() {
    assert_eq!(eval_str("let x = 1; in with { x = 2; }; x"), "1");
    assert_eq!(eval_str("with { x = 2; }; with { x = 3; }; x"), "3");
    assert_eq!(eval_str("with { map = 1; }; map"), "<PRIMOP>");
    assert_eq!(eval_str("with { true = 1; }; true"), "true");
    assert_eq!(
        eval_str("let f = { a, b ? a + 1, ... } @ args: a + b + args.c; in f { a = 1; c = 10; }"),
        "13"
    );
    assert_eq!(
        eval_str("let inherit (builtins) length; in length [ 1 2 ]"),
        "2"
    );
    assert_eq!(
        eval_str("{ a.b = 1; a.c = 2; a = { d = 3; }; }"),
        "{ a = { b = 1; c = 2; d = 3; }; }"
    );
    assert!(eval_err("{ a = 1; a = 2; }").starts_with("attribute 'a' already defined"));
}

#[test]
fn attrs_and_lists() {
    assert_eq!(
        eval_str("{ b = 2; a = 1; } // { a = 3; }"),
        "{ a = 3; b = 2; }"
    );
    assert_eq!(eval_str("[ 1 ] ++ [ 2 3 ]"), "[ 1 2 3 ]");
    assert_eq!(eval_str("{ a = 1; } ? a"), "true");
    assert_eq!(eval_str("{ a = 1; }.b or 7"), "7");
    assert_eq!(
        eval_str("let s = \"x\"; in { ${s} = 1; \"${s}y\" = 2; }"),
        "{ x = 1; xy = 2; }"
    );
    assert_eq!(
        eval_str("builtins.sort builtins.lessThan [ 3 1 2 ]"),
        "[ 1 2 3 ]"
    );
    assert_eq!(eval_str("map (x: x * 2) [ 1 2 ]"), "[ 2 4 ]");
    assert_eq!(eval_str("builtins.foldl' (a: b: a + b) 0 [ 1 2 3 ]"), "6");
}

#[test]
fn equality() {
    assert_eq!(eval_str("let f = x: x; in f == f"), "false");
    assert_eq!(eval_str("let f = x: x; in builtins.elem f [ f ]"), "true");
    assert_eq!(eval_str("1 == 1.0"), "true");
    assert_eq!(
        eval_str("[ 1 { a = \"x\"; } ] == [ 1 { a = \"x\"; } ]"),
        "true"
    );
    assert_eq!(eval_str("\"a\" < \"b\" && [ 1 2 ] < [ 1 3 ]"), "true");
}

#[test]
fn try_eval_and_errors() {
    assert_eq!(
        eval_str("(builtins.tryEval (throw \"x\")).success"),
        "false"
    );
    assert_eq!(
        eval_str("(builtins.tryEval (assert false; 1)).success"),
        "false"
    );
    assert_eq!(
        eval_err("builtins.tryEval (abort \"x\")"),
        "evaluation aborted with the following error message: 'x'"
    );
    assert_eq!(eval_err("{ a = 1; }.b"), "attribute 'b' missing");
}

#[test]
fn printing() {
    assert_eq!(
        eval_str("[ (x: x) builtins.map (builtins.map (x: x)) ]"),
        "[ <LAMBDA> <PRIMOP> <PRIMOP-APP> ]"
    );
    assert_eq!(eval_str("1.0e10"), "1e+10");
    assert_eq!(eval_str("0.1 + 0.2"), "0.3");
    assert_eq!(
        eval_str("\"a\\n\\\"b\\\" \\${x}\""),
        "\"a\\n\\\"b\\\" \\${x}\""
    );
    assert_eq!(eval_str("{ \"a b\" = 1; }"), "{ \"a b\" = 1; }");
    assert_eq!(
        eval_str("{ \"1\" = 1; \"if\" = 2; a-b = 3; }"),
        "{ \"1\" = 1; a-b = 3; \"if\" = 2; }"
    );
}

#[test]
fn derivation_wrapper_exists() {
    assert_eq!(eval_str("builtins.isFunction derivation"), "true");
    assert_eq!(
        eval_str("builtins.typeOf builtins.derivation"),
        "\"lambda\""
    );
}

#[test]
fn reading_a_derivation_output_is_unsupported() {
    let drv = r#"(derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; })"#;
    for f in [
        "import",
        "scopedImport {}",
        "builtins.readFile",
        "builtins.readDir",
        "builtins.pathExists",
        "builtins.readFileType",
        r#"builtins.hashFile "sha256""#,
    ] {
        let mut ev = Evaluator::new();
        let e = ev
            .eval_string(&format!("{f} {drv}"), b"/")
            .and_then(|v| ev.force_deep(v))
            .unwrap_err();
        assert_eq!(
            e.kind,
            crate::eval::ErrorKind::Unsupported,
            "{f}: {}",
            e.msg
        );
    }
}

fn eval_pure(src: &str) -> Result<String, String> {
    let mut ev = EvalConfig::new().pure(true).build();
    let v = ev
        .eval_string(src, b"/")
        .and_then(|v| ev.force_deep(v))
        .map_err(|e| e.msg)?;
    let mut out = Vec::new();
    print::print_ambiguous(&mut ev, v, 1, &mut out, &mut HashSet::new(), 0).unwrap();
    Ok(String::from_utf8(out).unwrap())
}

/// The answers of `nix-instantiate --pure-eval --read-write-mode` (Nix 2.34.8).
#[test]
fn pure_eval_answers_like_nix() {
    let ok = |src: &str, want: &str| assert_eq!(eval_pure(src), Ok(want.to_string()), "{src}");
    let err = |src: &str, want: &str| assert_eq!(eval_pure(src), Err(want.to_string()), "{src}");
    ok("builtins ? currentSystem", "false");
    ok("builtins ? currentTime", "false");
    ok(r#"builtins.getEnv "HOME""#, r#""""#);
    ok("builtins.nixPath", "[ ]");
    ok("builtins.pathExists /etc/hostname", "false");
    ok(r#"builtins.readFile (builtins.toFile "a" "b")"#, r#""b""#);
    ok(r#"import (builtins.toFile "a.nix" "1")"#, "1");
    let drv = r#"(derivation { name = "p"; builder = "/bin/sh"; system = "x"; })"#;
    ok(&format!("(import {drv}.drvPath).name"), r#""p""#);
    err(
        "builtins.currentSystem",
        "attribute 'currentSystem' missing",
    );
    err("__currentSystem", "undefined variable '__currentSystem'");
    err(
        "<nixpkgs>",
        "cannot look up '<nixpkgs>' in pure evaluation mode (use '--impure' to override)",
    );
    err(
        "builtins.readFile /etc/hostname",
        "access to absolute path '/etc/hostname' is forbidden in pure evaluation mode (use '--impure' to override)",
    );
    err(
        &format!("builtins.readFile {drv}.drvPath"),
        "access to absolute path '/nix/store/z4c1kxpa2xx2wq9gyqmmmxfvxl4fcjax-p.drv' is forbidden in pure evaluation mode (use '--impure' to override)",
    );
    err(
        "builtins.storePath /nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source",
        "'builtins.storePath' is not allowed in pure evaluation mode",
    );
    err(
        r#"builtins.fetchTarball "https://example.com/x.tar.gz""#,
        "in pure evaluation mode, 'fetchTarball' requires a 'sha256' argument",
    );
    err(
        r#"builtins.fetchTree { type = "tarball"; url = "https://example.com/x.tar.gz"; }"#,
        "in pure evaluation mode, 'fetchTree' doesn't fetch unlocked input 'https://example.com/x.tar.gz'",
    );
    err(
        r#"builtins.fetchTree { type = "github"; owner = "o"; repo = "r"; ref = "main"; }"#,
        "in pure evaluation mode, 'fetchTree' doesn't fetch unlocked input 'github:o/r/main'",
    );
    err(
        r#"builtins.fetchTree { type = "path"; path = "/a b"; lastModified = 3; }"#,
        "in pure evaluation mode, 'fetchTree' doesn't fetch unlocked input 'path:/a%20b?lastModified=3'",
    );
    err(
        r#"builtins.fetchTree { type = "gitlab"; owner = "o"; repo = "r"; host = "x.org"; }"#,
        "in pure evaluation mode, 'fetchTree' doesn't fetch unlocked input 'gitlab:o/r?host=x.org'",
    );
    err(
        r#"builtins.getFlake "/tmp""#,
        "cannot call 'getFlake' on unlocked flake reference '/tmp', at «none»:0 (use --impure to override)",
    );
}

#[test]
fn from_json_and_toml_reject_null_bytes() {
    let msg = |s: &str| {
        format!(
            "input string '{s}' cannot be represented as Nix string because it contains null bytes"
        )
    };
    assert_eq!(eval_err(r#"builtins.fromJSON "\"a\\u0000b\"""#), msg("a␀b"));
    assert_eq!(
        eval_err(r#"builtins.fromJSON "{\"a\\u0000\": 1}""#),
        msg("a␀")
    );
    assert_eq!(
        eval_err(r#"builtins.fromTOML "a = \"x\\u0000y\"""#),
        format!("while parsing TOML: error: {}", msg("x␀y"))
    );
    assert_eq!(
        eval_err(r#"builtins.fromTOML "\"k\\u0000\" = 1""#),
        format!("while parsing TOML: error: {}", msg("k␀"))
    );
}

/// The `.drvPath` answers of `nix-instantiate --eval --strict` (Nix 2.34.8). A
/// Nix string is bytes, and the `.drv` path hashes them as they are.
#[test]
fn derivation_keeps_env_and_args_that_are_not_utf8() {
    let byte = r#"(builtins.substring 0 1 "é")"#;
    for (attrs, want) in [
        (
            format!("v = {byte};"),
            "vhkdfhp7f0qfk8j0kx9b54hpn6cyg8m1-x.drv",
        ),
        (
            format!("args = [ {byte} ];"),
            "qsjvddzs1qqxaf6lm7w8pf66lhmkzb5l-x.drv",
        ),
        (
            format!(r#"v = {byte}; __structuredAttrs = false; passAsFile = [ "v" ];"#),
            "qxxd6h5v28msmzrnzf9mc1jxfk2d645i-x.drv",
        ),
    ] {
        let src = format!(
            r#"(derivation {{ name = "x"; builder = "/bin/sh"; system = "x86_64-linux"; {attrs} }}).drvPath"#
        );
        assert_eq!(eval_str(&src), format!(r#""/nix/store/{want}""#), "{attrs}");
    }
}
