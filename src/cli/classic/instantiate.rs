//! `nix-instantiate` (nix-instantiate.cc). Evaluates, registers the derivation
//! closure through the daemon and prints the drv paths, or evaluates and prints
//! the value with `--eval`.

use std::collections::HashSet;
use std::path::Path;
use std::slice;
use std::sync::Arc;

use super::{
    Argv, Common, common_flag, eval_config, eval_input, fallback, local, shell_settings,
    store_given,
};
use crate::api::build::{Dispatch, Root};
use crate::api::{build as apibuild, installable};
use crate::build::render::Style;
use crate::cli::{self, Outcome, Run, abs_path, auto_args, cwd, render};
use crate::eval::{Error, Evaluator, Pos};
use crate::provenance::Provenance;
use crate::remote::plan::Plan;
use crate::remote::worker::{Options, Transport};
use crate::runtime::Word;
use crate::{error, log, print};

const NAME: &str = "nix-instantiate";

/// shared.cc:printGCWarning, which fires at the first derivation.
const GC_WARNING: &str = "warning: you did not specify '--add-root'; the result might be removed by the garbage collector";

#[derive(Default, PartialEq, Eq, Clone, Copy)]
enum Output {
    #[default]
    Plain,
    Raw,
    Json,
}

#[derive(Default)]
struct Inst {
    c: Common,
    eval_only: bool,
    strict: bool,
    output: Output,
    add_root: Option<String>,
    read_write: bool,
    readonly: bool,
    gc_warning: bool,
}

fn parse(argv: &[String]) -> Result<Inst, String> {
    let mut i = Inst {
        gc_warning: true,
        ..Inst::default()
    };
    let mut a = Argv { all: argv, i: 0 };
    while a.i < argv.len() {
        let s = argv[a.i].clone();
        match s.as_str() {
            "--eval" | "--eval-only" => i.eval_only = true,
            "--read-write-mode" => i.read_write = true,
            "--readonly-mode" | "--dry-run" => i.readonly = true,
            "--strict" => i.strict = true,
            "--json" => i.output = Output::Json,
            "--raw" => i.output = Output::Raw,
            // nix-instantiate.cc: an empty `gcRoot` is "no root", warning and all
            "--add-root" => i.add_root = Some(a.value("--add-root")?).filter(|s| !s.is_empty()),
            "--no-gc-warning" => i.gc_warning = false,
            "-Q" | "--no-build-output" => i.c.quiet = true,
            // it changes `--xml` output only, and `--xml` falls back
            "--no-location" => {}
            "-" => return Err("reading the expression from stdin is not implemented".into()),
            "--parse" | "--parse-only" | "--xml" | "--find-file" => {
                return Err(format!("'{s}' is not implemented"));
            }
            _ => {
                if !common_flag(&mut i.c, &mut a, &s)? {
                    if s.starts_with('-') {
                        return Err(format!("'{s}' is not implemented"));
                    }
                    i.c.files.push(s);
                }
            }
        }
        a.i += 1;
    }
    if i.eval_only && !i.read_write {
        i.readonly = true;
    }
    if i.c.files.is_empty() && !i.c.from_args {
        i.c.files.push("./default.nix".into());
    }
    if i.c.flags.attrs.is_empty() {
        i.c.flags.attrs.push(String::new());
    }
    if i.add_root.is_some() && i.readonly {
        return Err("'--add-root' with a read-only store is not implemented".into());
    }
    Ok(i)
}

enum Made {
    /// `--eval`: each value went to stdout as it was made.
    Values,
    /// The default mode: the roots to register and print, and their closure.
    Drvs(Vec<Root>, Option<Box<Plan>>),
}

pub fn main(argv: &[String]) -> i32 {
    let i = match parse(argv) {
        Ok(i) => i,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    // `nix-instantiate` builds nothing: the drvs, their sources and the
    // `--add-root` links belong to the local store whatever `builder` says.
    let (cfg, store, options) = match shell_settings(&i.c) {
        Ok(t) => t,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let transport = local();
    if !i.readonly && store_given(&i.c) && store != transport {
        fallback(NAME, argv, "'--store' is not the local daemon");
    }
    let ec = eval_config(&i.c, &cfg);
    let i = Arc::new(i);
    let inst = Arc::clone(&i);
    match cli::run_reported(Run::on(ec), move |ev, _| run(ev, &inst)) {
        Outcome::Failed(code) => code,
        Outcome::Unsupported(reason) => fallback(NAME, argv, &reason),
        Outcome::Ok(Made::Values) => 0,
        Outcome::Ok(Made::Drvs(roots, plan)) => {
            finish(&i, &transport, &options, &roots, plan.map(|p| *p))
        }
    }
}

fn run(ev: &mut Evaluator, i: &Inst) -> error::Result<Made> {
    let cwd = cwd();
    let mut roots: Vec<Root> = Vec::new();
    let args = auto_args(ev, &i.c.flags.args, &cwd)?;
    let has_args = !i.c.flags.args.is_empty();
    let mut warned = false;
    for file in &i.c.files {
        let root = eval_input(ev, &i.c, file, &cwd)?;
        for attr in &i.c.flags.attrs {
            let v = installable::find_along_attr_path(ev, root, attr, args)?;
            if i.eval_only {
                let v = if has_args { ev.auto_call(v, args)? } else { v };
                // nix-instantiate.cc writes each value as it has it, so what
                // ran before a failing expression is on stdout already
                let mut text: Vec<u8> = Vec::new();
                print_value(ev, v, i, &mut text)?;
                render::bytes_lossy(&text);
            } else {
                // nix-instantiate.cc: one `getDerivations` per attribute path,
                // so two of them that name one derivation give two roots
                let mut seen = HashSet::new();
                installable::get_drvs_at(ev, v, args, &mut roots, &mut seen, attr)?;
                if !warned && !roots.is_empty() && i.add_root.is_none() && i.gc_warning {
                    warned = true;
                    log::line(GC_WARNING);
                }
            }
        }
    }
    if i.eval_only {
        return Ok(Made::Values);
    }
    if i.readonly || roots.is_empty() {
        return Ok(Made::Drvs(roots, None));
    }
    let plan = apibuild::plan(ev, &Root::paths(&roots))?;
    Ok(Made::Drvs(roots, Some(Box::new(plan))))
}

fn print_value(ev: &mut Evaluator, v: Word, i: &Inst, out: &mut Vec<u8>) -> Result<(), Box<Error>> {
    match i.output {
        Output::Raw => {
            let mut ctx = Vec::new();
            let s = ev.coerce_to_string(
                v,
                &mut ctx,
                Pos::NONE,
                "while generating the nix-instantiate output",
                false,
                true,
                true,
            )?;
            out.extend_from_slice(&s);
        }
        Output::Json => {
            let mut ctx = Vec::new();
            print::print_json(ev, v, out, &mut ctx, i.strict, Pos::NONE)?;
            out.push(b'\n');
        }
        Output::Plain => {
            let v = if i.strict {
                ev.force_deep(v)?
            } else {
                ev.force(v)?
            };
            let mut seen = HashSet::new();
            print::print_ambiguous(ev, v, 1, out, &mut seen, 0)?;
            out.push(b'\n');
        }
    }
    Ok(())
}

/// Registers the closure, makes the `--add-root` links, prints the drv paths.
fn finish(
    i: &Inst,
    transport: &Transport,
    options: &Options,
    roots: &[Root],
    plan: Option<Plan>,
) -> i32 {
    let mut up = None;
    if let Some(plan) = plan {
        // no build, so nothing to render: the daemon's log lines go out as they are
        let style = Style {
            quiet: true,
            ..Style::detect()
        };
        let r =
            Dispatch::open(transport, options, plan, Provenance::off(), style).and_then(|mut d| {
                d.upload_only(slice::from_ref(&roots[0].drv))?;
                Ok(d)
            });
        match r {
            Ok(d) => up = Some(d),
            Err(e) => return e.report(),
        }
    }
    let cwd = cwd();
    let mut out = String::new();
    let mut root_nr = 0;
    for r in roots {
        let shown = match (&i.add_root, up.as_mut()) {
            (Some(g), Some(d)) => {
                root_nr += 1;
                let mut name = String::from_utf8_lossy(&abs_path(g, &cwd)).into_owned();
                if root_nr > 1 {
                    name.push_str(&format!("-{root_nr}"));
                }
                if let Err(e) = d.d.out_link(&r.drv.to_string(), Path::new(&name)) {
                    return e.report();
                }
                name
            }
            _ => r.drv.to_string(),
        };
        if r.output == "out" {
            out.push_str(&shown);
        } else {
            out.push_str(&format!("{shown}!{}", r.output));
        }
        out.push('\n');
    }
    render::bytes_lossy(out.as_bytes());
    0
}
