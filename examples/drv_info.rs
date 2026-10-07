//! List the derivations an evaluation created, with the attribute path and
//! the source position each came from (docs/explanation/provenance.md).
//!
//! `cargo run --example drv_info -- [-I prefix=path]... [-A attr] (-E EXPR | FILE)`
//!
//! Uses the library only (`iets::api`), never `iets::cli`.

use iets::api::{self, EvalArgs, Input};

use std::env;
use std::process::{self, ExitCode};

fn main() -> ExitCode {
    // a frontend owns the run state and says where messages go
    let session = iets::session::Session::new();
    session.log.write_to_stderr();
    let _entered = session.enter();
    let mut a = EvalArgs::new();
    let mut argv = env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut next = || {
            argv.next()
                .unwrap_or_else(|| die(&format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "-I" | "--include" => a.includes.push(api::parse_include(&next())),
            "-E" | "--expr" => a.input = Some(Input::Expr(next())),
            "-A" | "--attr" => a.attr = Some(next()),
            f if f.starts_with('-') && f != "-" => die(&format!("unknown flag {f}")),
            f => a.input = Some(Input::File(f.to_string())),
        }
    }
    if a.input.is_none() {
        die("give -E EXPR or a FILE");
    }

    // a setting that does not resolve is reported already
    let Ok(run) = iets::api::run::Run::new(&a) else {
        return ExitCode::from(1);
    };
    let r = run.go(move |ev, _| {
        let target = a.attr.clone();
        // A derivation exists once its attrset is forced, so walk deeply.
        let forced = api::evaluate(ev, a).and_then(|v| ev.force_deep(v));
        let v = match forced {
            Ok(v) => v,
            Err(e) => {
                api::report(ev, &e);
                return 1;
            }
        };
        // The attribute path a frontend reached a drv by is provenance the
        // evaluator cannot know: the walk records it.
        if let Some(t) = &target {
            let mut roots = Vec::new();
            if iets::api::installable::drv_roots(ev, v, &mut roots).is_ok() {
                for (p, _) in &roots {
                    ev.drv.set_attr(p, t);
                }
            }
        }
        let drvs = ev.drv.drvs.lock();
        let mut rows: Vec<String> = drvs
            .iter()
            .map(|(path, info)| {
                let attr = info.attr.as_deref().unwrap_or("-");
                let pos = info
                    .pos
                    .map(|p| {
                        iets::provenance::describe_pos(ev, p).unwrap_or_else(|| ev.describe_pos(p))
                    })
                    .unwrap_or_else(|| "-".to_string());
                let outs: Vec<&str> = info.outputs.iter().map(String::as_str).collect();
                format!("{path}\t{attr}\t{pos}\t{}", outs.join(","))
            })
            .collect();
        rows.sort();
        println!("drv\tattr\tposition\toutputs");
        for row in rows {
            println!("{row}");
        }
        eprintln!("{} derivations", drvs.len());
        0
    });
    match r {
        Ok(c) => ExitCode::from(c),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn die(msg: &str) -> ! {
    eprintln!("usage: drv_info [-I prefix=path]... [-A attr] (-E EXPR | FILE): {msg}");
    process::exit(2)
}
