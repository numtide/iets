use crate::session::*;

use crate::api::{self, EvalArgs, Input};
use crate::eval::{EvalConfig, Evaluator, Pos};
use crate::runtime::Word;
use crate::sys::heap::is_released;
use crate::{parallel, print};
use std::env;
use std::process::{self, Command};
use std::sync::Arc;

use crate::log;

#[test]
fn two_sessions_share_no_state() {
    let line = format!(
        "{}:\n  specified: sha256-A=\n     got:    sha256-B=",
        crate::log::MISMATCH
    );
    let (a, b) = (Session::new(), Session::new());
    {
        let _entered = a.enter();
        log::note_mismatch(&line);
        log::resolve_mismatches(&|_| true);
    }
    {
        let _entered = b.enter();
        assert!(crate::log::take_mismatches().is_empty());
    }
    let _entered = a.enter();
    assert_eq!(crate::log::take_mismatches().len(), 1);
}

fn eval_in(s: &Arc<Session>, expr: &str) -> (Evaluator, Word) {
    let mut ev = EvalConfig::new().session(s.clone()).build();
    let a = EvalArgs {
        input: Some(Input::Expr(expr.into())),
        no_ir_cache: true,
        no_tracing_cache: true,
        workers: 1,
        ..EvalArgs::new()
    };
    let w = api::evaluate(&mut ev, a).expect("evaluates");
    (ev, w)
}

fn json(ev: &mut Evaluator, w: Word) -> String {
    let mut out = Vec::new();
    print::print_json(ev, w, &mut out, &mut Vec::new(), true, Pos::NONE).expect("prints");
    String::from_utf8(out).unwrap()
}

const EXPR: &str = r#"builtins.genList (i: { n = i; s = "x${toString i}"; }) 2000"#;

#[test]
fn a_released_session_leaves_the_other_alone() {
    let (a, b) = (Session::new(), Session::new());
    let (ev_a, wa) = eval_in(&a, EXPR);
    let (mut ev_b, wb) = eval_in(&b, EXPR);
    let at = wa.bits() as usize & 0xffff_ffff_fff8;
    assert!(a.heap.mapped() > 0);
    drop(ev_a);
    drop(a);
    assert!(is_released(at));
    let (mut ev_c, wc) = eval_in(&Session::new(), EXPR);
    assert_eq!(json(&mut ev_b, wb), json(&mut ev_c, wc));
}

#[test]
fn a_parallel_run_releases_its_session() {
    let s = Session::new();
    let ev = EvalConfig::new().session(s.clone()).build();
    let at = parallel::try_run(4, ev, |ev| {
        let a = EvalArgs {
            input: Some(Input::Expr(EXPR.into())),
            no_ir_cache: true,
            no_tracing_cache: true,
            ..EvalArgs::new()
        };
        let w = api::evaluate(ev, a).expect("evaluates");
        json(ev, w);
        w.bits() as usize & 0xffff_ffff_fff8
    })
    .expect("runs");
    assert!(!is_released(at));
    drop(s);
    assert!(is_released(at));
}

/// The child process uses a value of a released session; the parent
/// checks that it died with the message and did not read freed memory.
#[test]
fn a_value_used_after_release_is_loud() {
    if env::var_os("IETS_TEST_STALE_CHILD").is_some() {
        let a = Session::new();
        let (ev_a, wa) = eval_in(&a, EXPR);
        drop(ev_a);
        drop(a);
        let (mut ev_b, _) = eval_in(&Session::new(), "1");
        let _ = json(&mut ev_b, wa);
        process::exit(0);
    }
    let out = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "api::session_tests::a_value_used_after_release_is_loud",
        ])
        .env("IETS_TEST_STALE_CHILD", "1")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(
        err.contains("error: a value was used after its session was released"),
        "{err}"
    );
}
