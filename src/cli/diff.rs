//! `iets diff A B`: what differs between two derivations (docs/reference/commands.md).

use std::collections::{BTreeMap, BTreeSet};

use crate::api::installable::drv_roots;
use crate::cli::help::{arg_error, unknown_flag, value};
use crate::cli::{EvalArgs, EvalFlags, Input, Run, eval_flag, evaluate, report};
use crate::error::{Error, Result};
use crate::store::drv::{Derivation, DrvError, load_derivation_from_store};
use crate::store::path::StorePath;

struct Opts {
    json: bool,
    depth: usize,
    closure: bool,
    builder: Option<String>,
}

/// One of the two things being compared: a `.drv` in the store, or an
/// evaluation that must give exactly one derivation.
enum Side {
    Drv(StorePath),
    Eval(Box<EvalArgs>),
}

/// 0 when the two are equal, 1 when they differ, 2 for a usage error or any
/// other failure (docs/reference/errors.md).
pub fn cmd_diff(argv: &[String]) -> i32 {
    let (o, sides) = parse(argv);
    let (aterms, roots) = match resolve(sides) {
        Ok(r) => r,
        Err(code) => return code.max(2),
    };
    // a derivation this run evaluated, else the one in /nix/store
    let mut load = |p: &StorePath| -> std::result::Result<Derivation, DrvError> {
        match aterms.get(p) {
            Some(t) => Derivation::parse(t, Derivation::name_from_path(p)?),
            None => load_derivation_from_store(p),
        }
    };
    match run(&o, &mut load, &roots) {
        Ok(code) => code,
        Err(e) => e.report().max(2),
    }
}

fn parse(argv: &[String]) -> (Opts, Vec<Side>) {
    let mut o = Opts {
        json: false,
        depth: 2,
        closure: false,
        builder: None,
    };
    let mut f = EvalFlags::default();
    let mut pos: Vec<String> = Vec::new();
    let mut expr: Option<String> = None;
    let mut i = 0;
    while i < argv.len() {
        let s = argv[i].as_str();
        let mut next = || {
            i += 1;
            value(argv, i, s).clone()
        };
        match s {
            _ if eval_flag(&mut f, s, || Ok(next())).unwrap_or_else(|e| arg_error(e)) => {}
            "--json" => o.json = true,
            "--all" => o.depth = usize::MAX,
            "--closure" => o.closure = true,
            "--builder" => o.builder = Some(next()),
            "--depth" => {
                let v = next();
                o.depth = v
                    .parse()
                    .unwrap_or_else(|_| arg_error(format!("--depth wants a number, got '{v}'")));
            }
            "-E" | "--expr" => expr = Some(next()),
            // a synonym for the positional side, as for 'iets eval'
            "-f" | "--file" => pos.push(next()),
            _ if s.starts_with('-') && s != "-" => unknown_flag(s),
            _ => pos.push(s.to_string()),
        }
        i += 1;
    }
    let _ = crate::config::Config::load_native(&f.overrides);
    // one side: the positional `p` (a .drv in the store, a flake reference
    // or a file), or `-E EXPR`, with the attribute `attr` selected
    let side = |p: Option<&String>, attr: Option<String>| match (p, &expr) {
        (Some(p), _) if p.ends_with(".drv") => match StorePath::from_base_name(base_name(p)) {
            Ok(sp) if sp.is_derivation() => Side::Drv(sp),
            Ok(sp) => arg_error(format!("'{sp}' is not a derivation path")),
            Err(e) => arg_error(e.0),
        },
        (Some(_), Some(_)) => arg_error("-E and FILE cannot be combined"),
        (p, e) => Side::Eval(Box::new(EvalArgs {
            input: match (p, e) {
                (Some(p), _) if crate::api::flake::is_flake_ref(p) => Some(Input::Flake(p.clone())),
                (Some(p), _) => Some(Input::File(p.clone())),
                (None, Some(e)) => Some(Input::Expr(e.clone())),
                (None, None) => None,
            },
            attr,
            includes: f.includes.clone(),
            args: f.args.clone(),
            pure: f.pure,
            ..EvalArgs::new()
        })),
    };
    let sides = match (f.attrs.len(), pos.len()) {
        (2, 0..=1) => (f.attrs.iter())
            .map(|at| side(pos.first(), Some(at.clone())))
            .collect(),
        (0, 2) => vec![side(Some(&pos[0]), None), side(Some(&pos[1]), None)],
        _ => {
            arg_error("give two derivations (A B), or one expression, file or flake with -A X -A Y")
        }
    };
    (o, sides)
}

/// `/nix/store/<hash>-<name>.drv`, `./<hash>-<name>.drv` or the base name.
fn base_name(arg: &str) -> &str {
    arg.rsplit('/').next().unwrap_or(arg)
}

fn run(o: &Opts, load: &mut crate::store::drv::DrvLoader<'_>, roots: &[StorePath]) -> Result<i32> {
    let (a, b) = (&roots[0], &roots[1]);
    if o.closure {
        return closure_diff(o, load, a, b);
    }
    let d = crate::store::drvdiff::diff(load, a, b, o.depth)?;
    crate::cli::render::emit(&d, o.json);
    Ok(i32::from(!d.is_empty()))
}

type Aterms = std::collections::HashMap<StorePath, std::sync::Arc<Vec<u8>>>;

/// The two root derivations, and the ATerms an evaluation produced for the
/// derivations under them; the rest are read from `/nix/store`.
fn resolve(sides: Vec<Side>) -> std::result::Result<(Aterms, Vec<StorePath>), i32> {
    // the sides share their flags in practice, so the first side's settings
    // are the run's
    let first = sides.iter().find_map(|s| match s {
        Side::Eval(a) => Some(Run::new(a)),
        Side::Drv(_) => None,
    });
    let Some(run) = first else {
        let paths = sides
            .into_iter()
            .map(|s| match s {
                Side::Drv(p) => p,
                Side::Eval(_) => unreachable!("no evaluated side"),
            })
            .collect();
        return Ok((Aterms::new(), paths));
    };
    crate::cli::run_parallel(run?, move |ev, _| {
        let mut roots = Vec::new();
        for s in sides {
            let a = match s {
                Side::Drv(p) => {
                    roots.push(p);
                    continue;
                }
                Side::Eval(a) => *a,
            };
            let mut found = Vec::new();
            let r = evaluate(ev, a).and_then(|v| drv_roots(ev, v, &mut found));
            if let Err(e) = r {
                return Err(report(ev, &e));
            }
            let [(p, _)] = &found[..] else {
                let n = found.len();
                return Err(Error::Usage(format!(
                    "each side must be one derivation, this one is {n}"
                ))
                .report());
            };
            roots.push(p.clone());
        }
        let aterms = (ev.drv.drvs.lock().iter())
            .filter_map(|(p, i)| i.aterm.clone().map(|t| (p.clone(), t)))
            .collect();
        Ok((aterms, roots))
    })
}

/// `--closure`: the output closures compared by package name and version,
/// as nvd does. The daemon answers what each path refers to.
fn closure_diff(
    o: &Opts,
    load: &mut crate::store::drv::DrvLoader<'_>,
    a: &StorePath,
    b: &StorePath,
) -> Result<i32> {
    use crate::remote::worker::{Connection, Transport};
    // the closure is read where the outputs are, so this is the local daemon
    // and not the `builder` setting
    let url = o.builder.as_deref().unwrap_or("daemon");
    let Some(t) = Transport::parse(url) else {
        return Err(Error::Usage(format!(
            "unsupported store '{url}': expected daemon, unix:///path or ssh-ng://[user@]host"
        )));
    };
    let mut conn = Connection::open(&t, &mut |_| {})?;
    let (ca, cb) = (closure(&mut conn, load, a)?, closure(&mut conn, load, b)?);
    let c = crate::api::render::Closure::of(&a.to_string(), &b.to_string(), &ca, &cb);
    crate::cli::render::emit(&c, o.json);
    Ok(i32::from(!c.is_empty()))
}

/// The versions of each package name in the closure of every output of the
/// derivation `p`.
fn closure(
    conn: &mut crate::remote::worker::Connection,
    load: &mut crate::store::drv::DrvLoader<'_>,
    p: &StorePath,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let drv = load(p)?;
    let outs = (drv.outputs.iter()).map(|(name, out)| out.path(&drv.name, name));
    let mut todo: Vec<String> = outs
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .map(|sp| sp.to_string())
        .collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    while let Some(path) = todo.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some(info) = conn.query_path_info(&path, &mut |_| {})? else {
            let e = Error::Store(format!("'{path}' is not in the store"));
            return Err(Error::Hint(
                e.into(),
                format!("build it first: iets build {p}"),
            ));
        };
        let (n, v) = name_version(base_name(&path));
        by_name.entry(n.into()).or_default().insert(v.into());
        todo.extend(info.references.into_iter().filter(|r| !seen.contains(r)));
    }
    Ok(by_name)
}

/// `<hash>-NAME-VERSION`: the name ends at the first dash before a digit,
/// as Nix's `parseDrvName` and nvd split it.
fn name_version(base: &str) -> (&str, &str) {
    let n = base
        .get(crate::store::path::HASH_PART_LEN + 1..)
        .unwrap_or(base);
    let dash = |i: usize| n.as_bytes()[i] == b'-' && n.as_bytes()[i + 1].is_ascii_digit();
    match (0..n.len().saturating_sub(1)).find(|i| dash(*i)) {
        Some(i) => (&n[..i], &n[i + 1..]),
        None => (n, ""),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_name_ends_at_the_first_dash_before_a_digit() {
        let cases = [
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.1",
                ("hello", "2.12.1"),
            ),
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bash-interactive-5.2",
                ("bash-interactive", "5.2"),
            ),
            ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source", ("source", "")),
        ];
        for (base, want) in cases {
            assert_eq!(super::name_version(base), want, "{base}");
        }
    }
}
