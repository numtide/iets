//! `nix fmt` (src/nix/formatter.cc): the `formatter.<system>` of the flake in
//! the working directory, built once and kept alive by a link in iets's roots
//! store, then exec'd with the arguments the user gave. See
//! `docs/reference/classic.md`.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str;

use super::{Argv, Common, eval_config, fallback, local, nix};
use crate::api::build::{self, Dispatch, Options, Plan, Root, Style, Transport};
use crate::api::flake::{self, FlakeRef};
use crate::api::installable::{self, Prefixes, Want};
use crate::cli::{self, EvalArgs, Input, Outcome, Run, cwd, devenv};
use crate::eval::Evaluator;
use crate::provenance::Provenance;
use crate::{argv, cachefs, error, log};

const NAME: &str = "nix";

#[derive(Default)]
struct Fmt {
    c: Common,
    /// What is forwarded to the formatter; the first `--` is dropped.
    args: Vec<String>,
}

/// What a run remembers: the output that holds the formatter and the program
/// inside it. Neither can hold a newline.
struct Built {
    out_path: String,
    program: String,
}

impl Built {
    fn decode(bytes: &[u8]) -> Option<Built> {
        let text = str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        Some(Built {
            out_path: lines.next()?.to_string(),
            program: lines.next()?.to_string(),
        })
    }
}

/// What the run remembers, for [`Built::decode`] next time.
fn remembered(app: &installable::App) -> Vec<u8> {
    format!("{}\n{}\n", app.out_path, app.program).into_bytes()
}

/// args.cc:Args::parse: the first `--` is consumed and everything after it is
/// an argument, flags included.
fn parse(argv: &[String]) -> Result<Fmt, String> {
    let mut f = Fmt::default();
    let mut a = Argv { all: argv, i: 0 };
    let mut dash_dash = false;
    while a.i < argv.len() {
        let s = argv[a.i].clone();
        if !dash_dash && s == "--" {
            dash_dash = true;
        } else if !dash_dash && s.starts_with('-') {
            if !nix::global_flag(&mut f.c, &mut a, &s)? {
                return Err(format!("'{s}' is not implemented"));
            }
        } else {
            f.args.push(s);
        }
        a.i += 1;
    }
    Ok(f)
}

/// `nix fmt [ARGS...]`. `rest` is the arguments after the subcommand.
pub fn main(argv: &[String], rest: &[String]) -> i32 {
    let mut f = match parse(rest) {
        Ok(f) => f,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    // the builder builds it and the out-link brings the closure back, so the
    // formatter runs from the local store (docs/reference/classic.md)
    let (cfg, transport, options) = match nix::nix_settings(&mut f.c, false, true) {
        Ok(t) => t,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let cwd = cwd();
    // formatter.cc: the installable is always `.`, never an argument
    let fr = match cli::flake_ref(".", &cwd) {
        Ok(fr) => fr,
        Err(e) => fallback(NAME, argv, &e.to_string()),
    };
    let attr = format!("formatter.{}", cfg.system());
    let link = out_link(&fr.dir, &attr);

    let mut a = EvalArgs::new();
    a.input = Some(Input::Flake(".".into()));
    a.attr = Some(attr.clone());
    a.includes = f.c.flags.includes.clone();
    a.pure = f.c.flags.pure;
    let ec = eval_config(&f.c, &cfg);
    let run = Run::on(ec).cached(&a, "fmt");

    if let Some(hit) = run.hit_result().as_deref().and_then(Built::decode)
        && usable(&hit)
    {
        keep(link.as_deref(), &hit.out_path);
        return exec(&hit.program, &f.args, &fr.dir);
    }

    let flake = fr.clone();
    let r = match cli::run_reported(run, move |ev, cache| {
        let r = resolve(ev, &flake, &attr)?;
        if let Some(c) = cache {
            c.write(ev, &remembered(&r.app), false);
        }
        Ok(r)
    }) {
        Outcome::Failed(code) => return code,
        Outcome::Unsupported(reason) => fallback(NAME, argv, &reason),
        Outcome::Ok(r) => r,
    };
    let Resolved { app, plan } = r;
    match realise(&app, *plan, &transport, &options) {
        Ok(0) => {
            keep(link.as_deref(), &app.out_path);
            exec(&app.program, &f.args, &fr.dir)
        }
        Ok(code) => code,
        Err(e) => e.report(),
    }
}

/// Where the root of this flake's formatter lives: one link in iets's roots
/// store, named by the flake directory and the attribute it holds, so two
/// flakes on one machine keep two roots. `None` when no home is known, which
/// costs the root and nothing else.
fn out_link(dir: &Path, attr: &str) -> Option<PathBuf> {
    cachefs::root_link(None, &[b"fmt", dir.as_os_str().as_bytes(), attr.as_bytes()])
}

/// A cache hit is usable while the program is still there. A cache directory
/// is writable by whoever runs iets, so the entry may only ever name a
/// program inside the output it remembered, and that output must be a path of
/// the store directory: a planted entry can then point `nix fmt` at no file of
/// its choosing. What the store holds under that name is the daemon's to say.
fn usable(hit: &Built) -> bool {
    cachefs::program_of_output(&hit.out_path, &hit.program) && Path::new(&hit.program).exists()
}

/// Links `out_path` in the roots store and registers the link with the local
/// daemon, whatever built it, then marks it used so the TTL sweep keeps it.
/// A root is a claim on the local store, not part of the answer: when the
/// daemon will not take one, the formatter still runs, so this warns.
fn keep(link: Option<&Path>, out_path: &str) {
    let Some(l) = link else { return };
    let done = link_dir(l).and_then(|()| devenv::ensure_gc_root(&local(), l, out_path, None));
    match done {
        Ok(()) => cachefs::touch_link(l),
        Err(e) => log::line(&format!(
            "warning: no GC root on the formatter ({}): {e}",
            l.display()
        )),
    }
}

fn link_dir(link: &Path) -> error::Result<()> {
    let Some(dir) = link.parent() else {
        return Ok(());
    };
    cachefs::create_dir(dir)
        .map_err(|e| error::Error::io(format!("cannot create {}", dir.display()), e))
}

/// What the evaluation produced: the app and the plan its derivation needs.
struct Resolved {
    app: installable::App,
    plan: Box<Plan>,
}

/// Evaluates `formatter.<system>` and turns it into an app and a build plan.
fn resolve(ev: &mut Evaluator, fr: &FlakeRef, attr: &str) -> error::Result<Resolved> {
    let outputs = flake::outputs(ev, fr)?;
    let mut found = installable::from_outputs(
        ev,
        outputs,
        &flake::url(fr),
        Prefixes::Package,
        &format!(".{attr}"),
        Want::First,
    )?;
    let app = installable::to_app(ev, found.remove(0).value, attr)?;
    let roots = vec![Root {
        drv: app.drv.clone(),
        output: app.output.clone(),
    }];
    let plan = build::plan(ev, &Root::paths(&roots))?;
    Ok(Resolved {
        app,
        plan: Box::new(plan),
    })
}

/// Builds the formatter and brings its closure to the local store, because the
/// formatter runs here whatever the builder is (docs/reference/classic.md).
/// `keep` roots what this leaves behind.
fn realise(
    app: &installable::App,
    plan: Plan,
    transport: &Transport,
    options: &Options,
) -> error::Result<i32> {
    let roots = vec![Root {
        drv: app.drv.clone(),
        output: app.output.clone(),
    }];
    let style = Style {
        quiet: true,
        ..Style::detect()
    };
    let mut d = Dispatch::open(transport, options, plan, Provenance::off(), style)?;
    d.realise(&roots, &|p| p.name().to_string())?;
    if d.d.exit != 0 {
        return Ok(1);
    }
    d.d.out_links(&[(app.out_path.clone(), None)])?;
    Ok(0)
}

/// formatter.cc: the program with the user's arguments after it, `PRJ_ROOT`
/// set to the flake directory, and this process replaced by it.
fn exec(program: &str, args: &[String], flake_dir: &Path) -> i32 {
    let e = Command::new(program)
        .arg0(program)
        .args(args.iter().map(|a| argv::os(a)))
        .env("PRJ_ROOT", flake_dir)
        .exec();
    log::line(&format!("error: unable to execute '{program}': {e}"));
    1
}
