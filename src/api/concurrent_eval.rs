//! Differential test of the parallel runtime: every case is evaluated with
//! 1, 2, 4 and 8 workers. Stdout, exit code and the effect lines on stderr
//! must match.

use crate::api::run::Run;
use crate::api::{self, EvalArgs, Input};
use crate::lint::Level;
use crate::parallel::fiber::MAIN_STACK;
use crate::print;
use crate::session::Session;
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::{env, fs, process, thread};

const SLOW: &str = "slow = n: builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) n);";
const WORKERS: [usize; 4] = [1, 2, 4, 8];

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
            "let SLOW t = (x: y: x) 1; A = [ t t t t t t t t ]; B = [ t t t t t t t t ]; in builtins.seq (builtins.any (x: false) A) (builtins.seq (slow 3000000) (A == B))",
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
            r#"let SLOW in builtins.any (x: x) [ (slow 3000000 > 0) (1 == "a") (2 == "b") (3 == "c") (4 == "d") ]"#,
        ),
        effect(
            "lint-demanded",
            lint,
            None,
            r#"let SLOW in builtins.all (x: x == false) [ (slow 2000000 > 0 && false) (1 == "a") (2 == "b") (3 == "c") (4 == "d") ]"#,
        ),
        // `elem` returns at the first element, so nothing below it is demanded
        effect(
            "drv-speculated",
            None,
            Some(0),
            "STOPS in builtins.elem 1 [ (slow 3000000 * 0 + 1) (builtins.stringLength (d 1)) (builtins.stringLength (d 2)) (builtins.stringLength (d 3)) ]",
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
    let wrap = |name: String, pre: &str, a: &str, b: &str| {
        case(
            &name,
            true,
            &format!(
                "let SLOW t = (x: y: x) 1; {pre} A = {a}; B = {b}; in builtins.seq (builtins.any (x: false) A) (builtins.seq (slow 3000000) (A == B))"
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

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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

/// One evaluation as `iets eval` does it: stdout, exit code and stderr.
fn eval(a: EvalArgs) -> (String, i32, String) {
    let session = Session::new();
    let log = Sink::default();
    session.log.write_to(Box::new(log.clone()));
    let _entered = session.enter();
    let strict = a.strict;
    let mut run = Run::new(&a).expect("the arguments configure");
    if a.dump_drvs.is_some() {
        run.cfg = run.cfg.keep_aterm(true).keep_site(false);
    }
    let (out, rc) = run
        .go(move |ev, _| {
            let r = api::evaluate(ev, a).and_then(|v| {
                let v = if strict {
                    ev.force_deep(v)?
                } else {
                    ev.force(v)?
                };
                let mut out = Vec::new();
                print::print_ambiguous(ev, v, 1, &mut out, &mut HashSet::new(), 0)?;
                Ok(out)
            });
            match r {
                Ok(out) => {
                    ev.lints.flush();
                    (out, 0)
                }
                Err(e) => (Vec::new(), api::report(ev, &e)),
            }
        })
        .expect("the fiber stacks");
    let err = String::from_utf8_lossy(&log.0.lock().unwrap()).into_owned();
    (String::from_utf8_lossy(&out).into_owned(), rc, err)
}

fn outcome(c: &Case, workers: usize, dump: Option<&Path>) -> Outcome {
    let (out, rc, err) = eval(args(c, workers, dump));
    let mut eff: Vec<String> = err
        .lines()
        .filter(|l| {
            ["trace", "evaluation warning", "warning: ["]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .map(String::from)
        .collect();
    eff.sort();
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

fn failures(cases: Vec<Case>) -> Vec<String> {
    let chunk = cases.len().div_ceil(4).max(1);
    thread::scope(|s| {
        let handles: Vec<_> = cases
            .chunks(chunk)
            .map(|part| {
                // the stack `main` gives the evaluator: deep-chain recurses deeply
                thread::Builder::new()
                    .stack_size(MAIN_STACK)
                    .spawn_scoped(s, move || {
                        part.iter()
                            .filter_map(|c| Some(format!("{}: {}", c.name, check(c)?)))
                            .collect::<Vec<_>>()
                    })
                    .expect("spawn a case thread")
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("a worker panicked"))
            .collect()
    })
}

#[test]
fn concurrent_eval_matches_sequential_eval() {
    let cases = plain_cases()
        .into_iter()
        .chain(effect_cases())
        .chain(route_cases())
        .collect();
    let failed = failures(cases);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
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
