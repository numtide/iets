//! `nix develop` (src/nix/develop.cc): the `print-dev-env` environment, then
//! bash. The rc script is `makeRcScript` plus the tail `CmdDevelop::run` adds.
//! See `docs/reference/classic.md`.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use super::{Argv, Common, fallback};
use crate::cli::{Input, parse_eval_args};
use crate::devenv::escape_shell_arg_always as esc;
use std::os::unix::fs::DirBuilderExt;

const NAME: &str = "nix";

#[derive(Default)]
struct Develop {
    c: Common,
    installable: Option<String>,
    /// `-c`/`--command`: the command and its arguments, `exec`'d by the rc script.
    command: Vec<String>,
    ignore_env: bool,
    keep: Vec<String>,
    unset: Vec<String>,
}

fn parse(argv: &[String]) -> Result<Develop, String> {
    let mut d = Develop::default();
    let mut a = Argv { all: argv, i: 0 };
    while a.i < argv.len() {
        let s = argv[a.i].clone();
        match s.as_str() {
            // develop.cc: the handler takes a vector, so `-c` eats the rest.
            "-c" | "--command" => {
                d.command = argv[a.i + 1..].to_vec();
                if d.command.is_empty() {
                    return Err("--command requires at least one argument".into());
                }
                a.i = argv.len();
            }
            "-i" | "--ignore-env" | "--ignore-environment" => d.ignore_env = true,
            "-k" | "--keep" | "--keep-env-var" => {
                let v = a.value(&s)?;
                d.keep.push(v);
            }
            "-u" | "--unset" | "--unset-env-var" => {
                let v = a.value(&s)?;
                d.unset.push(v);
            }
            _ => {
                if !super::nix::global_flag(&mut d.c, &mut a, &s)? {
                    if s.starts_with('-') {
                        return Err(format!("'{s}' is not implemented"));
                    }
                    if d.installable.replace(s).is_some() {
                        return Err("'nix develop' takes one installable".into());
                    }
                }
            }
        }
        a.i += 1;
    }
    if d.ignore_env && !d.unset.is_empty() {
        return Err("--unset-env-var does not make sense with --ignore-env".into());
    }
    if !d.ignore_env && !d.keep.is_empty() {
        return Err("--keep-env-var does not make sense without --ignore-env".into());
    }
    Ok(d)
}

/// `nix develop [INSTALLABLE]`. `rest` is the arguments after the subcommand.
pub fn main(argv: &[String], rest: &[String]) -> i32 {
    let mut d = match parse(rest) {
        Ok(d) => d,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let (cfg, transport, options) = match super::nix::nix_settings(&mut d.c, false, false) {
        Ok(t) => t,
        Err(reason) => fallback(NAME, argv, &reason),
    };
    let ec = super::eval_config(&d.c, &cfg);
    let mut a = parse_eval_args(&[]);
    a.includes = d.c.flags.includes.clone();
    a.pure = d.c.flags.pure;
    let installable = d.installable.clone().unwrap_or_else(|| ".".into());
    let root = crate::cli::devenv::flake_shell_root(&installable, &transport);
    a.input = Some(Input::Flake(installable));
    let env = match crate::cli::devenv::dev_env(ec, a, transport, options, root) {
        Ok(e) => e,
        Err(code) => return code,
    };
    match run(&d, &cfg, &env) {
        Ok(()) => unreachable!("exec replaces the process"),
        Err(e) => e.report(),
    }
}

/// Writes the rc file and execs bash. Returns only when something failed.
fn run(
    d: &Develop,
    cfg: &crate::config::Config,
    dev: &crate::cli::devenv::DevEnv,
) -> crate::error::Result<()> {
    // a private directory, so no other user can put a symlink where the rc
    // file and the attribute files go
    let dir = private_dir()?;
    let attrs_dir = dir.join("attrs");
    let outputs_dir = Path::new(
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&crate::cli::cwd()),
    )
    .join("outputs");
    let rc_path = dir.join("rc");
    let mut script = crate::devenv::make_rc_script(&dev.env, &attrs_dir, &outputs_dir)?.script;
    script.push_str(&format!("command rm -f '{}'\n", rc_path.display()));

    if !d.command.is_empty() {
        let args: Vec<String> = d.command.iter().map(|s| esc(s)).collect();
        script.push_str(&format!("exec {}\n", args.join(" ")));
    } else {
        script = format!(
            "[ -n \"$PS1\" ] && [ -e ~/.bashrc ] && source ~/.bashrc;\nshopt -u expand_aliases\n{script}\nshopt -s expand_aliases\n"
        );
        for (setting, form) in [
            ("bash-prompt", "PS1={v};"),
            ("bash-prompt-prefix", "PS1={v}\"$PS1\";"),
            ("bash-prompt-suffix", "PS1+={v};"),
        ] {
            match cfg.str(setting) {
                Some("") | None => {}
                Some(v) => script.push_str(&format!(
                    "[ -n \"$PS1\" ] && {}\n",
                    form.replace("{v}", &esc(v))
                )),
            }
        }
    }

    // develop.cc takes bash from `nixpkgs#bashInteractive`; iets cannot fetch,
    // so the shell derivation's own builder is the bash (docs/reference/classic.md).
    let store_bash = Path::new(&dev.builder).is_file();
    let shell = if store_bash {
        dev.builder.clone()
    } else {
        "bash".to_string()
    };
    script.push_str(&format!("SHELL=\"{shell}\"\n"));
    if store_bash {
        let dir = Path::new(&shell)
            .parent()
            .unwrap_or(Path::new("/"))
            .display();
        script.push_str(&format!("PATH=\"{dir}${{PATH:+:$PATH}}\"\n"));
    }
    std::fs::write(&rc_path, &script)
        .map_err(|e| crate::error::Error::io(format!("cannot write '{}'", rc_path.display()), e))?;

    let mut cmd = Command::new(&shell);
    cmd.arg0("bash");
    if d.command.is_empty() {
        cmd.arg("--rcfile");
    }
    cmd.arg(&rc_path);
    cmd.env_clear().envs(environment(d, &dev.out_path, &shell));
    let e = cmd.exec();
    Err(crate::error::Error::io(
        format!("cannot execute '{shell}'"),
        e,
    ))
}

/// A fresh directory in `$TMPDIR` that only this user can enter, made with
/// exclusive creation so it is never something that was already there.
fn private_dir() -> crate::error::Result<std::path::PathBuf> {
    let tmp = std::env::temp_dir();
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = tmp.join(format!("iets-develop-{}-{nanos}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(crate::error::Error::io(
                    format!("cannot create '{}'", dir.display()),
                    e,
                ));
            }
        }
    }
    Err(crate::error::Error::Usage(format!(
        "cannot make a private directory in '{}'",
        tmp.display()
    )))
}

/// command.cc:MixEnvironment::setEnviron, then develop.cc's `NIX_GCROOT` and `SHELL`.
fn environment(d: &Develop, gc_root: &str, shell: &str) -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    if d.ignore_env {
        env.retain(|k, _| d.keep.iter().any(|n| OsStr::new(n) == k));
    }
    for n in &d.unset {
        env.remove(OsStr::new(n));
    }
    env.insert("NIX_GCROOT".into(), gc_root.into());
    env.insert("SHELL".into(), shell.into());
    env
}
