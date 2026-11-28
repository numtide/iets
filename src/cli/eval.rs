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
    if run.hit(|c| replay(c, a.attrs.len())).is_some() {
        return 0;
    }
    // only the cache and --dump-drvs read the ATerm text after evaluation
    let keep = run.cache.is_some() || a.dump_drvs.is_some();
    run.cfg = run.cfg.keep_aterm(keep).keep_site(false);
    let before = run.cfg.configured_session().log.diagnostic_lines();
    match cli::run_parallel(run, move |ev, cache| {
        let finish = cache.map(|c| c.finish_eval(ev, before));
        let out = eval_and_print(ev, a, finish.is_some())?;
        Ok((finish, out))
    }) {
        Ok((Some(write), (out, sizes))) => write(&out, &sizes),
        Ok((None, _)) => {}
        Err(code) => return code,
    }
    0
}

fn replay(c: &Cache, selections: usize) -> Option<()> {
    let out = c.part(Part::Result)?;
    let mut rest = out.as_slice();
    let mut writes = Vec::new();
    if selections > 1 {
        // Raw selections may be empty or contain newlines. Check every size
        // before the first write, so a malformed entry can fall back cleanly.
        let sizes = c.part(Part::OutputSizes)?;
        let (sizes, tail) = sizes.as_chunks::<8>();
        if !tail.is_empty() || sizes.len() != selections - 1 {
            return None;
        }
        for size in sizes {
            let n = usize::try_from(u64::from_le_bytes(*size)).ok()?;
            writes.push(rest.split_off(..n)?);
        }
    }
    c.replay_warnings();
    for value in writes.into_iter().chain([rest]) {
        render::bytes(value);
    }
    Some(())
}

fn eval_and_print(ev: &mut Evaluator, a: EvalArgs, cache: bool) -> Result<(Vec<u8>, Vec<u8>), i32> {
    let strict = a.strict;
    let json = a.json;
    let raw = a.raw;
    let t0 = Instant::now();
    let timings = a.timings;
    let evaluating = cli::evaluating(&a);
    let mut out: Vec<u8> = Vec::new();
    let mut sizes = Vec::new();
    let mut boundaries = a.attrs.len().saturating_sub(1);
    let r = cli::evaluate_each(ev, &a, |ev, v| {
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
        if !cache {
            out.clear();
        } else if boundaries > 0 {
            sizes.extend_from_slice(&((out.len() - start) as u64).to_le_bytes());
            boundaries -= 1;
        }
        Ok(())
    });
    drop(evaluating);
    if ev.session().settings().stats >= 1 {
        ev.print_stats();
    }
    if let Err(e) = r {
        return Err(report(ev, &e));
    }
    if timings {
        eprintln!("eval: {:.3}s", t0.elapsed().as_secs_f64());
        treestore::report_timings();
    }
    Ok((out, sizes))
}
