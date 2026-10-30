use std::collections::HashSet;

use super::{ErrorKind, EvalConfig, Evaluator, Pos};
use rstest::rstest;

use crate::print;

fn eval_with(mut ev: Evaluator, src: &str) -> Result<String, String> {
    let v = ev
        .eval_string(src, b"/")
        .and_then(|v| ev.force_deep(v))
        .map_err(|e| e.msg)?;
    let mut out = Vec::new();
    print::print_ambiguous(&mut ev, v, 1, &mut out, &mut HashSet::default(), 0).unwrap();
    Ok(String::from_utf8(out).unwrap())
}

fn eval_str(src: &str) -> String {
    eval_with(Evaluator::new(), src).unwrap_or_else(|e| panic!("{src:?}: {e}"))
}

fn eval_err(src: &str) -> String {
    eval_with(Evaluator::new(), src).expect_err("expected an error")
}

#[rstest]
#[case::flat("a 0 30", "a 5 40")]
#[case::few_in_union("a 3 6", "a 0 20 // b 0 400 // a 10 30 // c 0 50")]
#[case::union_in_few("a 0 20 // b 0 400 // a 10 30 // c 0 50", "a 3 6 // b 7 9")]
#[case::both_unions("a 0 40 // b 0 40 // c 5 9", "b 20 60 // a 30 35 // c 0 300")]
#[case::disjoint("x 0 500", "a 0 20 // b 0 400 // c 0 50")]
fn intersect_attrs_matches_a_filter(#[case] l: &str, #[case] r: &str) {
    // "p i j" is the set p<k> = "<p>-<i>-<k>" for k in i..j, so `//` both
    // overlaps and shadows; sets of 8 or more stay unions
    let set = |spec: &str| {
        spec.split(" // ")
            .map(|s| {
                let v: Vec<&str> = s.split(' ').collect();
                format!(
                    "(builtins.listToAttrs (builtins.genList (k: {{ name = \"{p}${{toString (k + {i})}}\"; value = \"{p}-{i}-${{toString k}}\"; }}) ({j} - {i})))",
                    p = v[0],
                    i = v[1],
                    j = v[2]
                )
            })
            .collect::<Vec<_>>()
            .join(" // ")
    };
    let src = format!(
        "let l = {}; r = {}; want = builtins.listToAttrs (map (n: {{ name = n; value = r.${{n}}; }}) (builtins.filter (n: l ? ${{n}}) (builtins.attrNames r))); got = builtins.intersectAttrs l r; in [ (got == want) (builtins.length (builtins.attrNames got)) ]",
        set(l),
        set(r)
    );
    let out = eval_str(&src);
    assert!(out.starts_with("[ true "), "{out}");
}
#[rstest]
#[case::ignored(r#"(x: 1) (throw "never")"#, "1")]
#[case::curried("(a: _: b: [ a b ]) 1 2 3", "[ 1 3 ]")]
#[case::reads_outer("let f = a: _: a; in f 1 2", "1")]
#[case::let_inside("(_: let a = 1; b = a; in b) 0", "1")]
#[case::with_inside("with { y = 5; }; (x: y) 0", "5")]
#[case::inherit("(x: { inherit x; }) 1", "{ x = 1; }")]
#[case::interpolation(r#"(x: "${x}") "s""#, r#""s""#)]
#[case::find_file(r#"(__findFile: <p>) (_: _: "found")"#, r#""found""#)]
#[case::curried_full("(a: b: c: [ a b c ]) 1 2 3", "[ 1 2 3 ]")]
#[case::curried_partial(
    "let f = a: b: c: [ a b c ]; g = f 1; h = g 2; in [ (h 3) (h 4) (g 5 6) ]",
    "[ [ 1 2 3 ] [ 1 2 4 ] [ 1 5 6 ] ]"
)]
#[case::inner_shadows_outer("(a: a: a) 1 2", "2")]
#[case::let_shadows_param("(x: let x = 2; y = x; in y) 1", "2")]
#[case::let_reads_param("(x: let y = x + 1; in [ x y ]) 1", "[ 1 2 ]")]
#[case::let_inherits_param("(x: let inherit x; in x) 1", "1")]
#[case::let_inherit_from("(x: let inherit (x) a; in a) { a = 1; }", "1")]
#[case::default_skips_let("let y = 1; in ({ a ? y }: let y = 2; in a) { }", "1")]
#[case::formals_curried("({ a }: { b }: let c = a + b; in c) { a = 1; } { b = 2; }", "3")]
#[case::empty_formals("({ }: b: b) { } 1", "1")]
#[case::closure_in_merged("let f = a: b: (c: [ a b c ]); in (f 1 2) 3", "[ 1 2 3 ]")]
#[case::through_builtin("builtins.mapAttrs (n: v: n + v) { a = \"x\"; }", r#"{ a = "ax"; }"#)]
fn lambda_params_read_or_ignored(#[case] src: &str, #[case] want: &str) {
    assert_eq!(eval_str(src), want);
}

/// `[ ]` and `{ }` compile to one value per site, which every evaluation
/// and every caller shares.
#[rstest]
#[case::list_grows(
    "let f = { a ? [ ] }: a ++ [ 1 ]; in [ (f { }) (f { }) ]",
    "[ [ 1 ] [ 1 ] ]"
)]
#[case::attrs_grow(
    "let f = x: { } // x; in [ (f { a = 1; }) (f { b = 2; }) ]",
    "[ { a = 1; } { b = 2; } ]"
)]
#[case::equal("[ ([ ] == [ ]) ({ } == { }) (rec { } == { }) ]", "[ true true true ]")]
#[case::types("map builtins.typeOf [ [ ] { } ]", r#"[ "list" "set" ]"#)]
#[case::or_default("let s = { }; in (s.x or [ ]) ++ (s.y or [ 2 ])", "[ 2 ]")]
#[case::dynamic_is_not_empty(r#"let n = "a"; in { ${n} = 1; }"#, "{ a = 1; }")]
fn empty_literals(#[case] src: &str, #[case] want: &str) {
    assert_eq!(eval_str(src), want);
}

#[test]
fn every_empty_list_and_set_is_one_object() {
    let mut ev = Evaluator::new();
    let list = ev.mk_list(&[]);
    let attrs = ev.empty_attrs();
    let made = ev
        .eval_string(
            "[ [ ] { } (builtins.filter (x: false) [ 1 ]) (removeAttrs { a = 1; } [ \"a\" ]) ]",
            b"/",
        )
        .and_then(|v| ev.force_deep(v))
        .unwrap();
    let items: Vec<_> = ev
        .force_list(made, Pos::NONE, "")
        .unwrap()
        .cells()
        .iter()
        .map(|c| c.get())
        .collect();
    assert_eq!(list, ev.mk_list(&[]));
    assert_eq!(attrs, ev.empty_attrs());
    assert_eq!([items[0], items[2]], [list, list]);
    assert_eq!([items[1], items[3]], [attrs, attrs]);
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

#[rstest]
fn reading_a_derivation_output_is_unsupported(
    #[values(
        "import",
        "scopedImport {}",
        "builtins.readFile",
        "builtins.readDir",
        "builtins.pathExists",
        "builtins.readFileType",
        r#"builtins.hashFile "sha256""#
    )]
    f: &str,
) {
    let drv = r#"(derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; })"#;
    let mut ev = Evaluator::new();
    let e = ev
        .eval_string(&format!("{f} {drv}"), b"/")
        .and_then(|v| ev.force_deep(v))
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported, "{}", e.msg);
}

#[test]
fn fetch_mercurial_names_a_way_out() {
    let mut ev = Evaluator::new();
    let e = ev
        .eval_string("builtins.fetchMercurial /tmp", b"/")
        .unwrap_err();
    assert_eq!(e.kind, crate::eval::ErrorKind::Unsupported);
    assert_eq!(e.msg, "builtins.fetchMercurial is not supported");
    let hint = e.hint.unwrap_or_default();
    assert!(hint.contains(r#"type = "hg""#), "{hint}");
    assert!(hint.contains("nix-instantiate"), "{hint}");
}

/// With no frontend to name it, the system is this build's own: `print-dev-env`
/// on darwin asks for a darwin shell.
#[test]
fn eval_config_defaults_to_the_system_this_build_is_for() {
    let system = EvalConfig::new().build().current_system();
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    assert!(system.ends_with(os), "{system} is not a {os} system");
}

fn eval_pure(src: &str) -> Result<String, String> {
    eval_with(EvalConfig::new().pure(true).build(), src)
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
        r#"builtins.fetchTree { type = "file"; url = "https://example.com/x"; narHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; }"#,
        "an unlocked 'file' input 'https://example.com/x' in pure evaluation mode is not supported",
    );
    err(
        r#"builtins.fetchTree { type = "indirect"; id = "nixpkgs"; }"#,
        "an unlocked 'indirect' input 'flake:nixpkgs' in pure evaluation mode is not supported",
    );
    err(
        r#"builtins.fetchTree { type = "sourcehut"; owner = "o"; repo = "r"; }"#,
        "an unlocked 'sourcehut' input in pure evaluation mode is not supported",
    );
    for (src, want) in [
        (
            r#"builtins.fetchGit "/a b""#,
            "in pure evaluation mode, 'fetchGit' doesn't fetch unlocked input 'git+file:///a%20b?exportIgnore=1'",
        ),
        (
            r#"builtins.fetchGit { url = "https://x.org/a.git"; ref = "main"; }"#,
            "in pure evaluation mode, 'fetchGit' doesn't fetch unlocked input 'git+https://x.org/a.git?exportIgnore=1&ref=main'",
        ),
        (
            r#"builtins.fetchGit { url = "git+ssh://git@x.org/a"; submodules = true; shallow = true; }"#,
            "in pure evaluation mode, 'fetchGit' doesn't fetch unlocked input 'git+ssh://git@x.org/a?shallow=1&submodules=1'",
        ),
        (
            r#"builtins.fetchGit "rel/x""#,
            "in pure evaluation mode, 'fetchGit' doesn't fetch unlocked input 'git+file:rel/x?exportIgnore=1'",
        ),
        (
            r#"builtins.fetchGit "github.com:a/b""#,
            "SCP-like URL 'github.com:a/b' is not supported; use SSH URL syntax instead (ssh://...)",
        ),
        (
            r#"builtins.fetchGit { url = "/a"; type = "git"; }"#,
            "unexpected argument 'type'",
        ),
        (
            r#"builtins.fetchGit { url = "/a"; foo = 1; }"#,
            "input attribute 'foo' not supported by scheme 'git'",
        ),
        ("builtins.fetchGit { }", "input attribute 'url' is missing"),
        (
            r#"builtins.fetchGit { url = "/a"; shallow = 1; }"#,
            "input attribute 'shallow' is not a Boolean",
        ),
        (
            r#"builtins.fetchGit { url = "/a"; x = []; }"#,
            "argument 'x' to 'fetchGit' is a list while a string, Boolean or integer is expected",
        ),
        (
            "builtins.fetchGit 1",
            "cannot coerce an integer to a string: 1",
        ),
    ] {
        err(src, want);
    }
    err(
        r#"builtins.getFlake "/tmp""#,
        "cannot call 'getFlake' on unlocked flake reference '/tmp', at «none»:0 (use --impure to override)",
    );
}

/// Nix fetches without a `sha256` in impure evaluation; iets refuses in every
/// mode, as unsupported where Nix would fetch.
#[rstest]
fn fetchurl_needs_a_sha256_in_every_mode(
    #[values(true, false)] pure: bool,
    #[values(
        r#"builtins.fetchurl "https://example.com/x""#,
        r#"builtins.fetchurl { url = "https://example.com/x"; name = "y"; }"#
    )]
    src: &str,
) {
    let want = "'fetchurl' requires a 'sha256' argument: run 'iets prefetch https://example.com/x' and pass the hash as 'sha256'";
    let mut ev = EvalConfig::new().pure(pure).build();
    let e = ev.eval_string(src, b"/").unwrap_err();
    let kind = if pure {
        ErrorKind::Eval
    } else {
        ErrorKind::Unsupported
    };
    assert_eq!((e.kind, e.msg.as_str()), (kind, want));
}

/// The errors `nix-instantiate --eval` gives before `fetchurl` downloads
/// anything (Nix 2.34.8, primops/fetchTree.cc:fetch).
#[test]
fn fetchurl_checks_its_argument_like_nix() {
    let h = "113a52l57f2jv9fh9igj2xh2mi0is40flxbkqi69wbh7nlybkcbm";
    let name_err = |url: &str, fix: &str| {
        format!(
            "invalid store path name when fetching URL '{url}': name '~wiggle~' contains illegal character '~'. {fix}"
        )
    };
    assert_eq!(
        eval_err(r#"builtins.fetchurl "https://example.com/~wiggle~""#),
        name_err(
            "https://example.com/~wiggle~",
            "Please pass an attribute set with 'url' and 'name' attributes to 'fetchurl',  so that it can create a valid store path."
        )
    );
    assert_eq!(
        eval_err(r#"builtins.fetchurl { url = "https://example.com/~wiggle~/"; }"#),
        name_err(
            "https://example.com/~wiggle~/",
            "Please add a valid 'name' attribute to the argument for 'fetchurl', so that it can create a valid store path."
        )
    );
    assert_eq!(
        eval_err(r#"builtins.fetchurl { url = "https://example.com/x"; name = "~wiggle~"; }"#),
        name_err(
            "https://example.com/x",
            "Please change the value for the 'name' attribute passed to 'fetchurl', so that it can create a valid store path."
        )
    );
    assert_eq!(
        eval_err(&format!(r#"builtins.fetchurl {{ sha256 = "{h}"; }}"#)),
        "'url' argument required"
    );
    assert_eq!(
        eval_err(&format!(
            r#"builtins.fetchurl {{ url = "https://example.com/x"; hash = "{h}"; }}"#
        )),
        "unsupported argument 'hash' to 'fetchurl'"
    );
    assert_eq!(
        eval_err(
            r#"builtins.fetchurl { url = "https://example.com/x"; sha256 = "sha1-AAAAAAAAAAAAAAAAAAAAAAAAAAA="; }"#
        ),
        "hash 'sha1-AAAAAAAAAAAAAAAAAAAAAAAAAAA=' should have type 'sha256'"
    );
    assert_eq!(
        eval_err("builtins.fetchurl 1"),
        "expected a string but found an integer: 1"
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
/// `byte` is a string that is not UTF-8.
#[rstest]
#[case::env("v = byte;", "vhkdfhp7f0qfk8j0kx9b54hpn6cyg8m1-x.drv")]
#[case::args("args = [ byte ];", "qsjvddzs1qqxaf6lm7w8pf66lhmkzb5l-x.drv")]
#[case::pass_as_file(
    r#"v = byte; __structuredAttrs = false; passAsFile = [ "v" ];"#,
    "qxxd6h5v28msmzrnzf9mc1jxfk2d645i-x.drv"
)]
fn derivation_keeps_env_and_args_that_are_not_utf8(#[case] attrs: &str, #[case] want: &str) {
    let src = format!(
        r#"let byte = builtins.substring 0 1 "é"; in (derivation {{ name = "x"; builder = "/bin/sh"; system = "x86_64-linux"; {attrs} }}).drvPath"#
    );
    assert_eq!(eval_str(&src), format!(r#""/nix/store/{want}""#));
}

/// `nix-instantiate --eval --arg a $'\n\n1' --arg b '  throw "b"'` names
/// `«string»:1:3` for the throw (Nix 2.34.8).
#[test]
fn a_second_pseudo_file_keeps_its_own_positions() {
    let mut ev = Evaluator::new();
    ev.eval_string("\n\n1", b"/").expect("eval");
    let e = ev.eval_string("  throw \"b\"", b"/").unwrap_err();
    assert_eq!(ev.describe_pos(e.pos), "«string»:1:3");
}

#[test]
fn update_forces_the_right_operand_first() {
    assert_eq!(
        eval_str("builtins.tryEval (1 // throw \"x\")"),
        "{ success = false; value = false; }"
    );
    assert_eq!(
        eval_str("builtins.tryEval ({ } // { } // (assert false; { }))"),
        "{ success = false; value = false; }"
    );
    assert!(eval_err("builtins.tryEval (throw \"x\" // 1)").starts_with("expected a set"));
}

#[rstest]
fn an_invalid_regex_fails_before_the_string_is_forced(#[values("match", "split")] f: &str) {
    let e = eval_err(&format!(
        "builtins.tryEval (builtins.{f} \"(\" (throw \"x\"))"
    ));
    assert!(e.starts_with("invalid regular expression"), "{e}");
}

#[test]
fn nix_eq_forces_what_the_shortcut_skips() {
    let eq = |src: &str| {
        let mut ev = Evaluator::new();
        ev.nix_eq = true;
        eval_with(ev, src)
    };
    let x = "let x = { a = throw \"a\"; }; in";
    assert_eq!(eval_str(&format!("{x} x == x")), "true");
    assert_eq!(eq(&format!("{x} x == x")), Err("a".into()));
    // the attribute is one slot: forced, then equal without a look inside
    assert_eq!(
        eq("let x = { a = { b = throw \"b\"; }; }; in x == x"),
        Ok("true".into())
    );
    assert_eq!(eq("[ 1 2 ] == [ 1 2 ]"), Ok("true".into()));
}

#[test]
fn a_function_select_is_a_fresh_slot_when_its_subject_is_forced() {
    // `f` forces `foo` in its slot, as the main fiber does when a helper
    // still has the `==` element
    let src = "let g = x: x; foo = { a = x: x; b = g g; }; f = foo.a; in builtins.seq f \
        [ ([ foo.a ] == [ foo.a ]) ([ foo.b ] == [ foo.b ]) ([ (foo.c or g) ] == [ (foo.c or g) ]) ]";
    assert_eq!(eval_str(src), "[ false false false ]");
}
#[test]
fn sets_compare_in_the_symbol_order_of_nix() {
    let first = |n: &str| {
        eval_str(&format!(
            "builtins.tryEval ({{ a = 1; \"{n}\" = throw \"t\"; }} == {{ a = 2; \"{n}\" = 1; }})"
        ))
    };
    assert_eq!(first("meta"), "{ success = false; value = false; }");
    assert_eq!(first(""), "{ success = false; value = false; }");
    assert_eq!(first("y"), "{ success = true; value = false; }");
}

/// `builder-rpc-v0` builders register their outputs themselves, so the
/// environment holds no placeholder for them. The paths are Nix's.
#[rstest]
#[case::list(
    CA,
    "requiredSystemFeatures = [ \"builder-rpc-v0\" ];",
    "hfx0mg11cj7g5b1ldb78lhm3vlcrg9hc"
)]
#[case::string(
    CA,
    "requiredSystemFeatures = \"kvm builder-rpc-v0\";",
    "vsdi08jx2v1qg20bjx02gd4y6h85avrr"
)]
#[case::structured(
    CA,
    "__structuredAttrs = true; requiredSystemFeatures = [ \"builder-rpc-v0\" ];",
    "lj3fkgvh7jaqlyaq2bbs478s6qha1w0a"
)]
#[case::fixed_output(
    FOD,
    "requiredSystemFeatures = [ \"builder-rpc-v0\" ];",
    "kkx1jwyzrkygzcgj8gkbi56pwd5f60hx"
)]
fn builder_rpc_outputs_have_no_placeholder(
    #[case] how: &str,
    #[case] attrs: &str,
    #[case] hash: &str,
) {
    let src = format!(
        "(derivation {{ name = \"x\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; {how} {attrs} }}).drvPath"
    );
    assert_eq!(eval_str(&src), format!("\"/nix/store/{hash}-x.drv\""));
}

const CA: &str =
    "__contentAddressed = true; outputHashMode = \"recursive\"; outputHashAlgo = \"sha256\";";
const FOD: &str = "outputHashMode = \"flat\"; outputHash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\";";
