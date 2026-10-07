//! `nix derivation show` (src/nix/derivation-show.cc, Nix 2.34.8): the JSON of
//! the store derivations the installables evaluate to, and with `-r` of their
//! whole input-derivation closure. The document is
//! `crate::api::drvshow`'s; this file is the command line and the printing.
//! See `docs/reference/classic.md`.

use super::nix::{Nix, nix_settings, print_cached, resolve};
use super::{eval_config, fallback};
use crate::api::{drvshow, installable};
use crate::cli::{cwd, render};

const NAME: &str = "nix";

fn parse(argv: &[String]) -> Result<Nix, String> {
    let mut n = Nix::default();
    n.parse(argv, |n, _, s| {
        match s {
            "-r" | "--recursive" => n.recursive = true,
            "--pretty" => n.pretty = Some(true),
            "--no-pretty" => n.pretty = Some(false),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    // installables.cc: an InstallablesCommand with no argument takes `.`.
    if n.installables.is_empty() {
        n.installables.push(".".into());
    }
    n.check_args_with_flake()?;
    Ok(n)
}

/// Runs `nix derivation show`. `argv` is everything after argv[0], for the
/// fallback; `rest` is everything after the subcommand.
pub fn main(argv: &[String], rest: &[String]) -> i32 {
    let mut n = match parse(rest) {
        Ok(n) => n,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let (cfg, _, _) = match nix_settings(&mut n.c, n.file.is_some(), true) {
        Ok(t) => t,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let pretty = n.pretty.unwrap_or_else(render::pretty);
    let ec = eval_config(&n.c, &cfg);
    // `-r` and the indentation make another document, so both are in the key
    let run = n.run(ec, "nix derivation show", n.recursive, pretty);
    print_cached(argv, run, move |ev| {
        let cwd = cwd();
        let mut roots = Vec::new();
        for what in &n.installables {
            let v = resolve(ev, &n, what, &cwd)?;
            roots.push(installable::to_derivation(ev, v, what)?);
        }
        Ok(drvshow::json_text(
            &drvshow::show(ev, &roots, n.recursive)?,
            pretty,
        ))
    })
}
