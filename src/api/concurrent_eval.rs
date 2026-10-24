//! Differential test of the parallel runtime: every case is evaluated with
//! 1, 4 and 8 workers. Stdout, exit code and the effect lines on stderr,
//! in their order, must match.

use crate::api::run::Run;
use crate::api::testkit::eval;
use crate::api::{self, EvalArgs, Input};
use crate::lint::Level;
use crate::parallel::fiber::MAIN_STACK;
use crate::session::Session;
use rstest::rstest;
use std::path::Path;
use std::{env, fs, process, thread};

const SLOW: &str = "slow = n: builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) n);";
const WORKERS: [usize; 3] = [1, 4, 8];

struct Case {
    name: String,
    strict: bool,
    lint: Option<&'static str>,
    drvs: Option<usize>,
    expr: String,
}

fn case(name: &str, strict: bool, expr: &str) -> Case {
    Case {
        name: name.into(),
        strict,
        lint: None,
        drvs: None,
        expr: expr
            .replace("SLOW", SLOW)
            .replace("ROOT", env!("CARGO_MANIFEST_DIR")),
    }
}

/// A lint finding or a dumped `.drv` is work beyond what the program asked for.
/// The lint cases hold it to demand. `drvs` is how many derivations the
/// program demands, and the dump directory must hold at least that many.
fn effect(name: &str, lint: Option<&'static str>, drvs: Option<usize>, expr: &str) -> Case {
    let stops = r#"let SLOW d = i: (derivation { name = "d${toString i}"; builder = "/bin/sh"; system = "x86_64-linux"; }).drvPath;"#;
    Case {
        lint,
        drvs,
        ..case(name, true, &expr.replace("STOPS", stops))
    }
}

fn plain_cases() -> Vec<Case> {
    vec![
        case(
            "shared-graph",
            true,
            r#"let
  n = 400;
  m = 1000003;
  g = builtins.genList (i: if i < 2 then i else (builtins.elemAt g (i - 1) + builtins.elemAt g (i - 2) + (if i > 100 then builtins.elemAt g (i - 100) else 0)) - (builtins.elemAt g (i - 1) + builtins.elemAt g (i - 2) + (if i > 100 then builtins.elemAt g (i - 100) else 0)) / m * m) n;
  rows = builtins.genList (r: map (i: builtins.elemAt g ((i * 7 + r) - (i * 7 + r) / n * n)) (builtins.genList (x: x) 64)) 64;
in builtins.foldl' (a: b: a + builtins.foldl' (x: y: x + y) 0 b) 0 rows"#,
        ),
        case(
            "deep-attrs",
            true,
            r#"let mk = d: if d == 0 then { v = 1; } else builtins.listToAttrs (map (i: { name = "k${toString i}"; value = mk (d - 1); }) [ 0 1 2 3 4 ]); in mk 5"#,
        ),
        case("cycle", true, "let x = y; y = x; in [ x y x y x y ]"),
        case(
            "cycle-two-ranges",
            true,
            "let x = y + 1; y = x + 1; in [ x 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 y 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 x ]",
        ),
        case(
            "cycle-deep",
            true,
            "let a = { p = b.q; }; b = { q = a.p; }; in [ a.p b.q 1 2 ]",
        ),
        case(
            "error-elem",
            true,
            r#"map (i: if i == 3 then throw "boom" else i) [ 1 2 3 4 5 6 ]"#,
        ),
        case(
            "try-eval",
            true,
            r#"map (i: (builtins.tryEval (if i == 3 then throw "boom" else i)).value) [ 1 2 3 4 5 6 ]"#,
        ),
        case(
            "trace-once",
            true,
            r#"map (i: builtins.trace "t${toString i}" (i * 2)) [ 1 2 3 4 5 6 7 8 9 10 ]"#,
        ),
        case(
            "warn-once",
            true,
            r#"builtins.concatStringsSep "," (map (i: builtins.warn "w${toString i}" (toString i)) [ 1 2 3 4 5 6 ])"#,
        ),
        case(
            "import-shared",
            true,
            "let f = import ROOT/tests/lang/eval-okay-arithmetic.nix; in map (i: f) [ 1 2 3 4 5 6 7 8 ]",
        ),
        case(
            "drv",
            true,
            r#"let d = derivation { name = "x"; builder = "/bin/sh"; system = "x86_64-linux"; args = map (i: toString i) [ 1 2 3 4 5 ]; }; in map (i: d.drvPath) [ 1 2 3 4 5 6 ]"#,
        ),
        case(
            "json",
            true,
            "builtins.toJSON (builtins.genList (i: { a = i; b = builtins.genList (j: i * j) 8; }) 40)",
        ),
        // the identity of a function is the word before forcing
        case(
            "fn-identity",
            true,
            "let SLOW t = (x: y: x) 1; A = [ t t t t t t t t ]; B = [ t t t t t t t t ]; in builtins.seq (builtins.any (x: false) A) (builtins.seq (slow 300000) (A == B))",
        ),
        // a thunk only a helper forced still prints as <CODE>
        case(
            "print-code",
            false,
            "let SLOW l = map (x: x + slow 20000) [ 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 ]; in builtins.seq (builtins.any (x: x > 0) l) l",
        ),
        // one thunk in one cell is still read by the printer (docs/explanation/evaluation.md, "One cell is not enough")
        case(
            "print-code-single",
            false,
            "let SLOW l = builtins.genList (i: slow 20000 * 0 + i + 1) 20; in builtins.seq (builtins.any (x: x > 0) l) l",
        ),
        case(
            "nan",
            true,
            "let inf = 1.0e308 * 10.0; nan = inf - inf; in [ (nan == nan) nan (0.0 - nan) inf (-inf) ]",
        ),
        // deeper than a helper stack: a helper that runs out drops its task
        case(
            "deep-chain",
            true,
            "let xs = builtins.genList (i: i) 200000; r = builtins.foldl' (acc: x: { v = acc; }) null xs; in builtins.deepSeq r 1",
        ),
        // one failure stops the speculation, so no task restarts it
        case(
            "failing-shared",
            true,
            r#"let SLOW bad = builtins.seq (slow 100000) (throw "bad"); l = builtins.genList (i: bad + i) 20000; in builtins.foldl' (a: b: a + b) 0 l"#,
        ),
    ]
}

fn effect_cases() -> Vec<Case> {
    let lint = Some("eq-type-mismatch");
    vec![
        effect(
            "lint-speculated",
            lint,
            None,
            r#"let SLOW in builtins.any (x: x) [ (slow 300000 > 0) (1 == "a") (2 == "b") (3 == "c") (4 == "d") ]"#,
        ),
        effect(
            "lint-demanded",
            lint,
            None,
            r#"let SLOW in builtins.all (x: x == false) [ (slow 300000 > 0 && false) (1 == "a") (2 == "b") (3 == "c") (4 == "d") ]"#,
        ),
        // a helper is still on the second element when the demand path
        // finishes the first, so the demand path walks the rest ahead of it
        effect(
            "lint-ahead",
            lint,
            None,
            r#"let SLOW in [ (slow 300000) (builtins.seq (slow 600000) (1 == "a")) (2 == "b") 3 4 ]"#,
        ),
        effect(
            "trace-ahead",
            None,
            None,
            r#"let SLOW in [ (slow 300000) (builtins.seq (slow 600000) (builtins.trace "a" 1)) (builtins.trace "b" 2) 3 4 ]"#,
        ),
        effect(
            "warn-ahead",
            lint,
            None,
            r#"let SLOW in [ (slow 300000) (builtins.seq (slow 600000) (builtins.warn "a" 1)) (2 == "b") 3 4 ]"#,
        ),
        // `x` is shared: the walk forces it ahead, under the third element,
        // and the sequential run under the second
        effect(
            "lint-shared-ahead",
            lint,
            None,
            r#"let SLOW x = 0 == "x"; in [ (slow 300000) (builtins.seq (slow 600000) (builtins.seq x (1 == "a"))) (builtins.seq x 2) 3 4 ]"#,
        ),
        effect(
            "trace-shared-ahead",
            None,
            None,
            r#"let SLOW x = builtins.trace "x" 0; in [ (slow 300000) (builtins.seq (slow 600000) (builtins.seq x (builtins.trace "a" 1))) (builtins.seq x 2) 3 4 ]"#,
        ),
        // a helper forces `x` under the second element, and the demand path
        // reads it under the first
        effect(
            "lint-shared-helper",
            lint,
            None,
            r#"let SLOW x = 0 == "x"; in [ (builtins.seq (slow 600000) (builtins.seq x (1 == "a"))) (builtins.seq x 2) 3 4 5 ]"#,
        ),
        // an outer entry forces an inner entry twice: the second force must
        // still read `x`, or the inner value lands in its set without it
        effect(
            "lint-map-twice",
            lint,
            None,
            r#"let SLOW x = builtins.seq (0 == "x") 1; inner = builtins.mapAttrs (n: v: v) { a = x; }; outer = builtins.mapAttrs (n: v: builtins.seq v v) inner; in [ (slow 600000) outer 3 4 5 ]"#,
        ),
        // a helper's `x` fails after its finding, and `tryEval` keeps the
        // value of the thunk that forced it
        effect(
            "lint-try-helper",
            lint,
            None,
            r#"let SLOW x = builtins.seq (0 == "x") (throw "no"); in [ (slow 300000) (builtins.tryEval x).success 3 4 5 ]"#,
        ),
        // the same through a `mapAttrs` entry, whose value lands in the set
        effect(
            "lint-shared-map",
            lint,
            None,
            r#"let SLOW m = builtins.mapAttrs (n: v: builtins.seq (0 == "x") v) { a = 1; }; in [ (builtins.seq (slow 600000) (builtins.seq m.a (1 == "a"))) m.a 3 4 5 ]"#,
        ),
        // `elem` returns at the first element, so nothing below it is demanded
        effect(
            "drv-speculated",
            None,
            Some(0),
            "STOPS in builtins.elem 1 [ (slow 300000 * 0 + 1) (builtins.stringLength (d 1)) (builtins.stringLength (d 2)) (builtins.stringLength (d 3)) ]",
        ),
        effect(
            "drv-demanded",
            None,
            Some(8),
            "STOPS in map (i: builtins.stringLength (d i)) [ 1 2 3 4 5 6 7 8 ]",
        ),
    ]
}

const SET8: &str = "{ a = t; b = t; c = t; d = t; e = t; f = t; g = t; h = t; }";

/// Every route by which one thunk's word reaches a second cell, as `pre`,
/// `a` and `b`: `any` fans the first list out so helpers force `t`, and `==`
/// then reads both cells. A route nothing marks is a silent return of the
/// bug, so each route has its own case.
fn route_cases() -> Vec<Case> {
    let eight = "[ t t t t t t t t ]";
    let same =
        |name: &str, pre: &str, ab: &str| (name.to_string(), pre.to_string(), ab.to_string());
    let set = format!("s = {SET8};");
    let routes = vec![
        same("local", "", eight),
        same(
            "select",
            "s = { inherit t; };",
            "[ s.t s.t s.t s.t s.t s.t s.t s.t ]",
        ),
        same("apply", "f = x: [ x x x x x x x x ];", "f t"),
        same("concat", "L = [ t t t t ]; M = [ t t t t ];", "L ++ M"),
        same("tail", "L = [ 0 t t t t t t t t ];", "builtins.tail L"),
        same(
            "filter",
            "L = [ t 1 t 2 t 3 t 4 t 5 t 6 t 7 t 8 ];",
            "builtins.filter (x: !(builtins.isInt x)) L",
        ),
        same("attr-values", &set, "builtins.attrValues s"),
        same(
            "cat-attrs",
            "l = builtins.genList (i: { k = t; }) 8;",
            "builtins.catAttrs \"k\" l",
        ),
        same(
            "list-to-attrs",
            "l = builtins.genList (i: { name = \"k${toString i}\"; value = t; }) 8;",
            "builtins.attrValues (builtins.listToAttrs l)",
        ),
        same("update", &set, "builtins.attrValues (s // { z = 1; })"),
        same(
            "inherit-from",
            &set,
            "builtins.attrValues { inherit (s) a b c d e f g h; }",
        ),
        same(
            "rec-attrs",
            "",
            &format!("builtins.attrValues (rec {SET8})"),
        ),
        same(
            "map-arg",
            "L = [ t t t t t t t t ];",
            "builtins.concatLists (builtins.map (x: [ x ]) L)",
        ),
        // one `mapAttrs` result, read twice: a second call would make fresh
        // map words and the two sides would differ whatever the workers do
        same(
            "map-attrs",
            &format!("s = {SET8}; m = builtins.attrValues (builtins.mapAttrs (n: v: v) s);"),
            "builtins.tail ([ 0 ] ++ m)",
        ),
        same(
            "zip-attrs",
            "s = { a = t; };",
            "(builtins.zipAttrsWith (n: vs: vs) [ s s s s s s s s ]).a",
        ),
        same(
            "with-scope",
            "s = { a = t; b = t; c = t; d = t; };",
            "with s; [ a b c d a b c d ]",
        ),
        same(
            "remove-attrs",
            "s = { a = t; b = t; c = t; d = t; e = t; f = t; g = t; h = t; z = 1; };",
            "builtins.attrValues (builtins.removeAttrs s [ \"z\" ])",
        ),
        same(
            "intersect-attrs",
            &set,
            "builtins.attrValues (builtins.intersectAttrs s s)",
        ),
        same(
            "foldl",
            "L = [ t t t t t t t t ];",
            "builtins.foldl' (acc: x: acc ++ [ x ]) [] L",
        ),
    ];
    // 300000 steals and parks as often as 3000000 did, at a tenth of the
    // time in the unoptimized test build; 30000 starts fewer tasks
    let wrap = |name: String, pre: &str, a: &str, b: &str| {
        case(
            &name,
            true,
            &format!(
                "let SLOW t = (x: y: x) 1; {pre} A = {a}; B = {b}; in builtins.seq (builtins.any (x: false) A) (builtins.seq (slow 300000) (A == B))"
            ),
        )
    };
    let mut cases: Vec<Case> = routes
        .iter()
        .map(|(n, pre, ab)| wrap(format!("route-{n}"), pre, ab, ab))
        .collect();
    // one thunk shared through many routes at once
    let stress = "L = [ t t t t ]; s = { a = t; b = t; c = t; d = t; }; W = (builtins.tail ([ 0 ] ++ L)) ++ (builtins.attrValues s) ++ (builtins.attrValues { inherit (s) a b c d; }) ++ (builtins.concatLists (builtins.map (x: [ x ]) L)) ++ (builtins.attrValues (builtins.mapAttrs (n: v: v) s)) ++ (builtins.filter (x: !(builtins.isInt x)) [ t 1 t 2 ]) ++ (with s; [ a b c d ]) ++ (builtins.attrValues (s // { z = 1; })) ++ (builtins.attrValues (rec { e = t; f = t; })) ++ [ s.a s.b ];";
    let side = "builtins.tail ([ 0 ] ++ W)";
    cases.push(wrap("route-stress".into(), stress, side, side));
    cases
}

#[derive(PartialEq, Debug)]
struct Outcome {
    out: String,
    rc: i32,
    eff: Vec<String>,
}

fn args(c: &Case, workers: usize, dump: Option<&Path>) -> EvalArgs {
    let mut a = EvalArgs::new();
    a.input = Some(Input::Expr(c.expr.clone()));
    a.strict = c.strict;
    a.workers = workers;
    a.no_tracing_cache = true;
    a.no_ir_cache = true;
    a.lints = c
        .lint
        .map(|n| (n.to_string(), Level::Warn))
        .into_iter()
        .collect();
    a.dump_drvs = dump.map(|d| d.display().to_string());
    a
}

fn outcome(c: &Case, workers: usize, dump: Option<&Path>) -> Outcome {
    let (out, rc, err) = eval(args(c, workers, dump));
    let eff: Vec<String> = err
        .lines()
        .filter(|l| {
            ["trace", "evaluation warning", "warning: ["]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .map(String::from)
        .collect();
    Outcome { out, rc, eff }
}

/// What is wrong with `c`, if anything.
fn check(c: &Case) -> Option<String> {
    let mut runs = Vec::new();
    for w in WORKERS {
        let dir = c.drvs.map(|_| {
            let d = env::temp_dir().join(format!(
                "iets-concurrent-eval-{}-{}-{w}",
                process::id(),
                c.name
            ));
            fs::create_dir_all(&d).expect("create the dump directory");
            d
        });
        let o = outcome(c, w, dir.as_deref());
        let dumped = dir.as_ref().map(|d| {
            let n = fs::read_dir(d).expect("read the dump directory").count();
            let _ = fs::remove_dir_all(d);
            n
        });
        runs.push((w, o, dumped));
    }
    let (_, base, _) = &runs[0];
    for (w, o, _) in &runs[1..] {
        if o != base {
            return Some(format!("1 vs {w} workers:\n{base:?}\n{o:?}"));
        }
    }
    for (w, _, dumped) in &runs {
        if let (Some(want), Some(got)) = (c.drvs, dumped)
            && *got < want
        {
            return Some(format!(
                "{w} workers: {got} derivations dumped, {want} demanded"
            ));
        }
    }
    None
}

fn all_cases() -> Vec<Case> {
    plain_cases()
        .into_iter()
        .chain(effect_cases())
        .chain(route_cases())
        .collect()
}

#[rstest]
#[case::shared_graph("shared-graph")]
#[case::deep_attrs("deep-attrs")]
#[case::cycle("cycle")]
#[case::cycle_two_ranges("cycle-two-ranges")]
#[case::cycle_deep("cycle-deep")]
#[case::error_elem("error-elem")]
#[case::try_eval("try-eval")]
#[case::trace_once("trace-once")]
#[case::warn_once("warn-once")]
#[case::import_shared("import-shared")]
#[case::drv("drv")]
#[case::json("json")]
#[case::fn_identity("fn-identity")]
#[case::print_code("print-code")]
#[case::print_code_single("print-code-single")]
#[case::nan("nan")]
#[case::deep_chain("deep-chain")]
#[case::failing_shared("failing-shared")]
#[case::lint_speculated("lint-speculated")]
#[case::lint_demanded("lint-demanded")]
#[case::lint_ahead("lint-ahead")]
#[case::trace_ahead("trace-ahead")]
#[case::warn_ahead("warn-ahead")]
#[case::lint_shared_ahead("lint-shared-ahead")]
#[case::trace_shared_ahead("trace-shared-ahead")]
#[case::lint_shared_helper("lint-shared-helper")]
#[case::lint_map_twice("lint-map-twice")]
#[case::lint_try_helper("lint-try-helper")]
#[case::lint_shared_map("lint-shared-map")]
#[case::drv_speculated("drv-speculated")]
#[case::drv_demanded("drv-demanded")]
#[case::route_local("route-local")]
#[case::route_select("route-select")]
#[case::route_apply("route-apply")]
#[case::route_concat("route-concat")]
#[case::route_tail("route-tail")]
#[case::route_filter("route-filter")]
#[case::route_attr_values("route-attr-values")]
#[case::route_cat_attrs("route-cat-attrs")]
#[case::route_list_to_attrs("route-list-to-attrs")]
#[case::route_update("route-update")]
#[case::route_inherit_from("route-inherit-from")]
#[case::route_rec_attrs("route-rec-attrs")]
#[case::route_map_arg("route-map-arg")]
#[case::route_map_attrs("route-map-attrs")]
#[case::route_zip_attrs("route-zip-attrs")]
#[case::route_with_scope("route-with-scope")]
#[case::route_remove_attrs("route-remove-attrs")]
#[case::route_intersect_attrs("route-intersect-attrs")]
#[case::route_foldl("route-foldl")]
#[case::route_stress("route-stress")]
fn concurrent_eval_matches_sequential_eval(#[case] name: &str) {
    let c = all_cases()
        .into_iter()
        .find(|c| c.name == name)
        .expect("a case of that name");
    // the stack `main` gives the evaluator: deep-chain recurses deeply
    let failed = thread::Builder::new()
        .stack_size(MAIN_STACK)
        .spawn(move || check(&c))
        .expect("spawn the evaluator thread")
        .join()
        .expect("the case finishes");
    assert!(failed.is_none(), "{}", failed.unwrap_or_default());
}

#[test]
fn every_case_has_a_test() {
    let names: Vec<String> = all_cases().into_iter().map(|c| c.name).collect();
    let src = include_str!("concurrent_eval.rs");
    for n in names {
        assert!(src.contains(&format!("(\"{n}\")]")), "no #[case] for {n}");
    }
}

const ALLOC: &str = "let xs = builtins.genList (i: builtins.genList (j: { a = j; b = toString (i + j); }) 3000) 300; in builtins.length (builtins.deepSeq xs xs)";

fn arena_bytes(workers: usize) -> f64 {
    let mut a = EvalArgs::new();
    a.input = Some(Input::Expr(ALLOC.into()));
    a.strict = true;
    a.workers = workers;
    a.no_tracing_cache = true;
    a.no_ir_cache = true;
    // the arenas fold into the total only when a stats line will read it
    let session = Session::new();
    session.set_stats_for_test(1);
    let _entered = session.enter();
    let run = Run::new(&a).expect("the arguments configure");
    run.go(move |ev, _| {
        let v = api::evaluate(ev, a).expect("evaluates");
        ev.force_deep(v).expect("forces");
        ev.arena_use().bytes as f64
    })
    .expect("the fiber stacks")
}

#[test]
fn concurrent_eval_sums_arena_use_over_workers() {
    let (one, four) = thread::Builder::new()
        .stack_size(MAIN_STACK)
        .spawn(|| (arena_bytes(1), arena_bytes(4)))
        .expect("spawn the evaluator thread")
        .join()
        .expect("the evaluations finish");
    let ratio = four / one;
    assert!(
        (0.9..=1.5).contains(&ratio),
        "arena total follows the worker count: {one} at 1, {four} at 4"
    );
}
