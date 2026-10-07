//! Evaluate an expression or a file and print it as strict JSON.
//!
//! `cargo run --example eval -- [-I prefix=path]... [-A attr] (-E EXPR | FILE)`
//!
//! Uses the library only (`iets::api`), never `iets::cli`.

use iets::api::{self, EvalArgs, Input};
use iets::eval::Pos;
use std::io::Write;

fn main() -> std::process::ExitCode {
    // a frontend owns the run state and says where messages go
    let session = iets::session::Session::new();
    session.log.write_to_stderr();
    let _entered = session.enter();
    let mut a = EvalArgs {
        strict: true,
        json: true,
        ..EvalArgs::new()
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut next = || {
            argv.next()
                .unwrap_or_else(|| die(&format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "-I" | "--include" => a.includes.push(api::parse_include(&next())),
            "-E" | "--expr" => a.input = Some(Input::Expr(next())),
            "-A" | "--attr" => a.attr = Some(next()),
            "--workers" => a.workers = next().parse().unwrap_or_else(|_| die("--workers N")),
            f if f.starts_with('-') && f != "-" => die(&format!("unknown flag {f}")),
            f => a.input = Some(Input::File(f.to_string())),
        }
    }
    if a.input.is_none() {
        die("give -E EXPR or a FILE");
    }

    // a setting that does not resolve is reported already
    let Ok(run) = iets::api::run::Run::new(&a) else {
        return std::process::ExitCode::from(1);
    };
    // The evaluator runs as the main fiber of the parallel runtime; with
    // one worker `go` is a plain call.
    let code = run.go(move |ev, _| {
        let v = match api::evaluate(ev, a) {
            Ok(v) => v,
            Err(e) => {
                api::report(ev, &e);
                return 1;
            }
        };
        let mut out = Vec::new();
        let mut ctx = Vec::new();
        match iets::print::print_json(ev, v, &mut out, &mut ctx, true, Pos::NONE) {
            Ok(()) => {
                out.push(b'\n');
                std::io::stdout().write_all(&out).expect("write stdout");
                0
            }
            Err(e) => {
                api::report(ev, &e);
                1
            }
        }
    });
    match code {
        Ok(c) => std::process::ExitCode::from(c),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

fn die(msg: &str) -> ! {
    eprintln!("usage: eval [-I prefix=path]... [-A attr] [--workers N] (-E EXPR | FILE): {msg}");
    std::process::exit(2)
}
