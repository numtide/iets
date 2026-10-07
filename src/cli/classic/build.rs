//! `nix-build` (nix-build.cc). Evaluates, uploads the closure, builds every
//! root in one `BuildPathsWithResults`, makes the `result` symlinks and prints
//! the output paths.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::{Argv, Common, common_flag, fallback};
use crate::api::build::{self as apibuild, Dispatch, Root};
use crate::api::installable;
use crate::build::render::Style;
use crate::cli::{Outcome, abs_path, auto_args, cwd};
use crate::eval::Evaluator;
use crate::provenance::Provenance;
use crate::remote::plan::Plan;
use crate::remote::worker::{Options, Transport};
use std::io::Write;

const NAME: &str = "nix-build";

#[derive(Default)]
struct Build {
    c: Common,
    out_link: String,
    no_out_link: bool,
}

fn parse(argv: &[String]) -> Result<Build, String> {
    let mut b = Build {
        out_link: "./result".into(),
        ..Build::default()
    };
    let mut a = Argv { all: argv, i: 0 };
    while a.i < argv.len() {
        let s = argv[a.i].clone();
        match s.as_str() {
            "-o" | "--out-link" => b.out_link = a.value("--out-link")?,
            "--no-out-link" | "--no-link" => b.no_out_link = true,
            // nix-build.cc: "obsolete", a no-op in Nix too
            "--add-drv-link" => {}
            "-Q" | "--no-build-output" => b.c.quiet = true,
            "-" => return Err("reading the expression from stdin is not implemented".into()),
            "--dry-run" | "--drv-link" | "--run-env" | "--command" | "--run" | "--repair"
            | "--check" | "--pure" | "--exclude" | "--keep" | "-p" | "--packages" => {
                return Err(format!("'{s}' is not implemented"));
            }
            _ => {
                if !common_flag(&mut b.c, &mut a, &s)? {
                    if s.starts_with('-') {
                        return Err(format!("'{s}' is not implemented"));
                    }
                    b.c.files.push(s);
                }
            }
        }
        a.i += 1;
    }
    if b.c.files.is_empty() && !b.c.from_args {
        b.c.files.push(".".into());
    }
    if b.c.flags.attrs.is_empty() {
        b.c.flags.attrs.push(String::new());
    }
    Ok(b)
}

enum Built {
    Ready(Vec<Root>, Box<Plan>, Box<Provenance>),
    Empty,
}

pub fn main(argv: &[String]) -> i32 {
    let b = match parse(argv) {
        Ok(b) => b,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let (cfg, transport, options) = match super::settings(&b.c) {
        Ok(t) => t,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let ec = super::eval_config(&b.c, &cfg);
    let b = Arc::new(b);
    let build = Arc::clone(&b);
    match crate::cli::run_reported(crate::cli::Run::on(ec), move |ev, _| run(ev, &build)) {
        Outcome::Failed(code) => code,
        Outcome::Unsupported(reason) => fallback(NAME, argv, &reason),
        Outcome::Ok(Built::Empty) => 0,
        Outcome::Ok(Built::Ready(roots, plan, prov)) => {
            realise(&b, &transport, &options, &roots, *plan, &prov)
        }
    }
}

fn run(ev: &mut Evaluator, b: &Build) -> crate::error::Result<Built> {
    let cwd = cwd();
    let mut roots: Vec<Root> = Vec::new();
    let mut seen = HashSet::new();
    let args = auto_args(ev, &b.c.flags.args, &cwd)?;
    for file in &b.c.files {
        let root = super::eval_input(ev, &b.c, file, &cwd)?;
        for attr in &b.c.flags.attrs {
            let v = crate::api::installable::find_along_attr_path(ev, root, attr, args)?;
            installable::get_drvs_at(ev, v, args, &mut roots, &mut seen, attr)?;
        }
    }
    if roots.is_empty() {
        return Ok(Built::Empty);
    }
    let prov = Provenance::snapshot(ev);
    let plan = apibuild::plan(ev, &Root::paths(&roots))?;
    Ok(Built::Ready(roots, Box::new(plan), Box::new(prov)))
}

/// `--no-out-link` asks for no symlink, and in nix that means the output is
/// collectable the moment the run ends: the opt-out protects nothing. Here it
/// keeps a root in iets's roots store instead — the workflow the flag is for,
/// without a `result` in the tree — keyed on the working directory, the files
/// and attributes asked for, and the name the symlink would have had, so a
/// second build replaces its own roots in place. It is a divergence from Nix,
/// invisible on stdout and stderr (`docs/reference/divergences.md`), and
/// `build-gc-root = false` turns it off. The links go on the local daemon,
/// which is where `out_links` brought the outputs.
fn keep_outputs(b: &Build, kept: &[(String, String)]) {
    let mut key: Vec<Vec<u8>> = vec![b"nix-build".to_vec(), cwd()];
    key.extend(b.c.files.iter().map(|f| f.as_bytes().to_vec()));
    key.extend(b.c.flags.attrs.iter().map(|a| a.as_bytes().to_vec()));
    crate::cli::devenv::keep_outputs(&super::local(), &key, kept);
}

/// Uploads the closure, builds every root in one call, links and prints.
fn realise(
    b: &Build,
    transport: &Transport,
    options: &Options,
    roots: &[Root],
    plan: Plan,
    prov: &Provenance,
) -> i32 {
    let style = Style {
        quiet: b.c.quiet,
        ..Style::detect()
    };
    let mut d = match Dispatch::open(transport, options, plan, prov.clone(), style) {
        Ok(d) => d,
        Err(e) => return e.report(),
    };
    // nix-build asks for every root in one op, one output per root. The bare
    // attribute names each one: the dispatcher adds the position, once.
    let name = |p: &crate::store::path::StorePath| {
        prov.attr_of(&p.to_string())
            .unwrap_or_else(|| crate::provenance::drv_name(&p.to_string()))
    };
    let built = match d.realise(roots, &name) {
        Ok(b) => b,
        Err(e) => return e.report(),
    };
    // a daemon client nix exits 1 on a failed build, whatever the status
    if d.d.exit != 0 {
        return 1;
    }
    let paths = match built.paths(roots) {
        Ok(p) => p,
        Err(e) => return e.report(),
    };

    // nix-build.cc: the n-th distinct drv gets the suffix `-n` from 2 on; an
    // output other than `out` adds `-<output>`.
    let mut counter: HashMap<String, usize> = HashMap::new();
    for r in roots {
        let n = counter.len();
        counter.entry(r.drv.to_string()).or_insert(n);
    }
    let cwd = cwd();
    let mut out = String::new();
    let mut links: Vec<(String, Option<String>)> = Vec::new();
    let mut kept: Vec<(String, String)> = Vec::new();
    for (r, path) in roots.iter().zip(&paths) {
        let drv = r.drv.to_string();
        if b.no_out_link {
            if !super::store_given(&b.c) {
                links.push((path.clone(), None));
                kept.push((format!("{}!{}", counter[&drv], r.output), path.clone()));
            }
        } else {
            let mut link = b.out_link.clone();
            let c = counter[&drv];
            if c > 0 {
                link.push_str(&format!("-{}", c + 1));
            }
            if r.output != "out" {
                link.push('-');
                link.push_str(&r.output);
            }
            let link = String::from_utf8_lossy(&abs_path(&link, &cwd)).into_owned();
            links.push((path.clone(), Some(link)));
        }
        out.push_str(path);
        out.push('\n');
    }
    if let Err(e) = d.d.out_links(&links) {
        return e.report();
    }
    keep_outputs(b, &kept);
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
    0
}
