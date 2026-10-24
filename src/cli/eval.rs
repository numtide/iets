//! `iets eval`: evaluate an expression and print it.

use std::collections::HashSet;
use std::time::Instant;

use crate::cli::{self, EvalArgs, render, report, run_for};
use crate::eval::{Evaluator, Pos};
use crate::trace::run::Cache;
use crate::trace::store::Part;
use crate::{print, treestore};

pub fn run_eval(a: EvalArgs) -> i32 {
    let mut run = run_for(&a).cached(&a, "eval");
    if let Some(out) = run.hit(|c| {
        let out = c.part(Part::Result)?;
        c.replay_warnings();
        Some(out)
    }) {
        render::bytes(&out);
        return 0;
    }
    // only the cache and --dump-drvs read the ATerm text after evaluation
    let keep = run.cache.is_some() || a.dump_drvs.is_some();
    run.cfg = run.cfg.keep_aterm(keep).keep_site(false);
    // parallel: the rest runs as the main fiber
    cli::run_parallel(run, move |ev, cache| eval_and_print(ev, a, cache))
}

fn eval_and_print(ev: &mut Evaluator, a: EvalArgs, cache: Option<Cache>) -> i32 {
    let strict = a.strict;
    let json = a.json;
    let raw = a.raw;
    let t0 = Instant::now();
    let timings = a.timings;
    let evaluating = cli::evaluating(&a);
    let mut out: Vec<u8> = Vec::new();
    let mut last = None;
    let r = cli::evaluate_each(ev, &a, |ev, v| {
        last = Some(v);
        let start = out.len();
        if raw {
            // nix eval --raw: coerce without copying to the store, no newline
            let mut ctx = Vec::new();
            let s = ev.coerce_to_string(
                v,
                &mut ctx,
                Pos::NONE,
                "while generating the eval command output",
                false,
                false,
                true,
            )?;
            out.extend_from_slice(&s);
        } else if json {
            // nix eval --json forces the whole value, --strict or not (eval.cc)
            let mut ctx = Vec::new();
            print::print_json(ev, v, &mut out, &mut ctx, true, Pos::NONE)?;
        } else {
            let v = if strict {
                ev.force_deep(v)?
            } else {
                ev.force(v)?
            };
            let mut seen = HashSet::default();
            print::print_ambiguous(ev, v, 1, &mut out, &mut seen, 0)?;
        }
        if !raw {
            out.push(b'\n');
        }
        // each value goes out as it is made, as in nix-instantiate --eval
        render::bytes(&out[start..]);
        Ok(())
    });
    drop(evaluating);
    if ev.session().settings().stats >= 1 {
        ev.print_stats();
    }
    if crate::trace::fine::on() {
        crate::trace::fine::dump(ev, last);
    }
    if let Err(e) = r {
        return report(ev, &e);
    }
    if timings {
        eprintln!("eval: {:.3}s", t0.elapsed().as_secs_f64());
        treestore::report_timings();
    }
    if let Some(c) = &cache {
        c.write(ev, &out, false);
    }
    0
}
