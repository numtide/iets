//! Classic mode: iets installed under Nix's own command names, with an exec
//! fallback to the real binary for everything it does not implement.
//! See `docs/reference/classic.md`.

pub mod build;
pub mod develop;
pub mod drvshow;
pub mod fmt;
pub mod instantiate;
pub mod nix;
pub mod shell;

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, exit};

use crate::cli::{EvalArgs, EvalFlags, abs_path, eval_flag, full_includes, jobs};
use crate::config::{self, Config};
use crate::eval::{self, EvalConfig, EvalResult, Evaluator, Pos};
use crate::remote::worker::{DEFAULT_SOCKET, Options, Transport};
use crate::runtime::Word;
use crate::session::Session;
use crate::settings::builders;
use crate::{api, argv, ignored, lint, log, nixbin};

/// The command names iets answers to when it is installed as classic Nix.
pub const NAMES: &[&str] = &[
    "nix-instantiate",
    "nix-build",
    "nix-eval-jobs",
    "nix-shell",
    "nix",
];

/// Runs the classic command `name`. `argv` is everything after argv[0].
pub fn dispatch(name: &str, argv: &[String]) -> i32 {
    log::set_classic();
    match name {
        "nix-instantiate" => instantiate::main(argv),
        "nix-build" => build::main(argv),
        "nix-shell" => shell::main(argv),
        "nix-eval-jobs" => jobs::cmd_eval_jobs(argv),
        _ => nix::main(argv),
    }
}

/// Execs the real `name` with the same arguments. `IETS_NO_FALLBACK=1` makes
/// this an error instead, so a test can see which cases fall back.
pub fn fallback(name: &str, argv: &[String], reason: &str) -> ! {
    log::line(&format!("iets: falling back to {name}: {reason}"));
    if Session::current().settings().no_fallback {
        log::line(&format!("iets: no-fallback is set, not running {name}"));
        exit(1)
    }
    let Some(real) = nixbin::real(name) else {
        eprintln!("iets: cannot fall back to {name}: not found on PATH");
        exit(1)
    };
    let e = Command::new(&real)
        .arg0(name)
        .args(argv.iter().map(|a| argv::os(a)))
        .exec();
    eprintln!("iets: cannot exec '{}': {e}", real.display());
    exit(1)
}

/// The flags `nix-instantiate` and `nix-build` share (`MixEvalArgs` plus the
/// settings aliases of `LegacyArgs`).
#[derive(Default)]
pub struct Common {
    pub flags: EvalFlags,
    pub files: Vec<String>,
    /// `-E`: the positional arguments are expressions, not paths.
    pub from_args: bool,
    pub quiet: bool,
}

/// A cursor over the command line; a flag without its value falls back, because
/// the real nix reports that better than a second usage message would.
pub struct Argv<'a> {
    pub all: &'a [String],
    pub i: usize,
}

impl Argv<'_> {
    pub fn value(&mut self, flag: &str) -> Result<String, String> {
        self.i += 1;
        self.all
            .get(self.i)
            .cloned()
            .ok_or_else(|| format!("'{flag}' requires an argument"))
    }

    /// `-j` and `--cores` set a setting, so the value is the setting's: an
    /// integer in a 32-bit field, and `auto` for `max-jobs`.
    fn number(&mut self, flag: &str, setting: &str) -> Result<String, String> {
        let v = self.value(flag)?;
        config::check_value(setting, &v)
            .map(|()| v.clone())
            .map_err(|e| e.to_string())
    }
}

/// `Ok(true)`: consumed. `Ok(false)`: not one of ours. `Err`: fall back.
pub fn common_flag(c: &mut Common, a: &mut Argv, s: &str) -> Result<bool, String> {
    if eval_flag(&mut c.flags, s, || a.value(s))? {
        return Ok(true);
    }
    let set = &mut c.flags.overrides;
    match s {
        "-E" | "--expr" => c.from_args = true,
        "--store" => set.push(("store".into(), a.value(s)?)),
        "-j" | "--max-jobs" => set.push(("max-jobs".into(), a.number(s, "max-jobs")?)),
        "--cores" => set.push(("cores".into(), a.number(s, "cores")?)),
        "-k" | "--keep-going" => set.push(("keep-going".into(), "true".into())),
        "-K" | "--keep-failed" => set.push(("keep-failed".into(), "true".into())),
        "--quiet" => c.quiet = true,
        "-v" | "--verbose" => ignored::flag("--verbose", ignored::ONE_LOG_LEVEL),
        // `--indirect` does nothing in Nix either (nix-build.cc: "obsolete").
        // Only nix-instantiate has the GC warning, and it takes that flag itself.
        "--no-gc-warning" | "--indirect" => {}
        _ => return Ok(false),
    }
    Ok(true)
}

/// Where the builds go, from the `store` setting. `system` is the `system=`
/// default a `grpc://` URL takes.
pub fn transport_of(store: &str, system: &str) -> Result<Transport, String> {
    match store {
        "auto" | "" | "daemon" => Ok(Transport::Unix(PathBuf::from(DEFAULT_SOCKET))),
        s if s.starts_with("unix://") || s.starts_with("ssh-ng://") || s.starts_with("grpc://") => {
            Transport::parse_checked(s, system)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("cannot parse the store URL '{s}'"))
        }
        s => Err(format!(
            "store '{s}' is not supported; iets speaks the worker protocol and grpc:// only"
        )),
    }
}

/// `Config::load` plus the transport and `SetOptions` values it implies. The
/// build target is `--store` or `--option store` when the command line gave
/// one, else the `builder` setting, else nix.conf's `store` (docs/reference/settings.md).
pub fn settings(c: &Common) -> Result<(Config, Transport, Options), String> {
    settings_with(c, true)
}

/// `settings` for a command that builds nothing on a builder: a shell, whose
/// inputs belong in the local store where it runs, and `nix-instantiate`.
/// The `builder` setting is skipped; `--store` still wins.
pub fn shell_settings(c: &Common) -> Result<(Config, Transport, Options), String> {
    settings_with(c, false)
}

fn settings_with(c: &Common, use_builder: bool) -> Result<(Config, Transport, Options), String> {
    // a nix.conf Nix itself refuses is no reason to hand it to Nix: it stops
    // at the same value with the same text (config-impl.hh:151)
    let cfg = Config::read(&c.flags.overrides).unwrap_or_else(|e| exit(e.report()));
    if let Some(name) = cfg.left_to_nix() {
        return Err(format!("the setting '{name}' is not implemented"));
    }
    cfg.announce();
    let url = if use_builder {
        let flag = store_given(c).then(|| cfg.store());
        // A classic name addresses one store, as the tool it stands in for
        // does. Placing across several is `iets build`'s (docs/reference/classic.md).
        let specs = builders(flag.as_deref(), &cfg);
        if let [first, rest @ ..] = specs.as_slice()
            && !rest.is_empty()
        {
            let names: Vec<&str> = rest.iter().map(|s| s.url.as_str()).collect();
            log::line(&format!(
                "warning: a classic name builds on one place: using '{}', ignoring {}",
                first.url,
                names.join(", ")
            ));
        }
        specs
            .first()
            .map(|s| s.url.clone())
            .unwrap_or_else(|| cfg.store())
    } else {
        cfg.store()
    };
    let transport = transport_of(&url, &cfg.system())?;
    if matches!(transport, Transport::Grpc(_)) {
        config::note_grpc(&c.flags.overrides);
    }
    let options = Options::try_from(&cfg).map_err(|e| e.to_string())?;
    Ok((cfg, transport, options))
}

/// `--store` names the store the outputs belong to. Without it the builder
/// stands in for the local store, as a build hook does, so outputs come back
/// even under `--no-link`.
pub fn store_given(c: &Common) -> bool {
    c.flags.overrides.iter().any(|(k, _)| k == "store")
}

/// The local daemon, where GC roots, out-links and `--add-root` registrations
/// go whatever `builder` says (docs/reference/classic.md).
pub fn local() -> Transport {
    Transport::Unix(PathBuf::from(DEFAULT_SOCKET))
}

/// The evaluator configuration: the search path, the `system` setting and
/// `pure-eval`.
pub fn eval_config(c: &Common, cfg: &Config) -> EvalConfig {
    let pure = c
        .flags
        .pure
        .unwrap_or_else(|| cfg.bool("pure-eval").ok().flatten().unwrap_or(false));
    let cwd = api::cwd();
    let entry = match (c.from_args, c.files.first()) {
        (false, Some(f)) => eval::dir_of(&abs_path(f, &cwd)).to_vec(),
        _ => cwd,
    };
    // the root alone feeds the provenance fallback
    EvalConfig::new()
        .system(&cfg.system())
        .pure(pure)
        .search_path(full_includes(&c.flags.includes))
        .ir_cache(EvalArgs::new().ir_cache())
        .lints(lint::Config::off())
        .lint_root(&entry)
}

/// `lookupFileArg`: `<name>` goes through the search path, anything else is a
/// path relative to the working directory.
pub fn lookup_file_arg(ev: &mut Evaluator, f: &str, cwd: &[u8]) -> EvalResult<Vec<u8>> {
    if f.len() > 2 && f.starts_with('<') && f.ends_with('>') {
        let entries = ev.search_path().to_vec();
        return ev.find_file(&entries, &f.as_bytes()[1..f.len() - 1], Pos::NONE);
    }
    Ok(abs_path(f, cwd))
}

/// One of the positional arguments, evaluated: an expression under `-E`,
/// otherwise a file (a directory resolves to its `default.nix`).
pub fn eval_input(ev: &mut Evaluator, c: &Common, file: &str, cwd: &[u8]) -> EvalResult<Word> {
    if c.from_args {
        ev.eval_string(file, cwd)
    } else {
        let p = lookup_file_arg(ev, file, cwd)?;
        ev.eval_file(&p, Pos::NONE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(line: &[&str]) -> Result<bool, String> {
        let all: Vec<String> = line.iter().map(|s| s.to_string()).collect();
        let mut a = Argv { all: &all, i: 0 };
        nix::global_flag(&mut Common::default(), &mut a, &all[0])
    }

    #[test]
    fn a_flag_with_no_effect_is_noted_and_one_that_holds_is_not() {
        assert_eq!(flag(&["--accept-flake-config"]), Ok(true));
        assert_eq!(flag(&["--eval-store", "auto"]), Ok(true));
        assert!(flag(&["--eval-store", "ssh-ng://b"]).is_err());
        assert_eq!(
            flag(&["--extra-experimental-features", "nix-command flakes"]),
            Ok(true)
        );
        assert!(flag(&["--experimental-features", "flakes parse-toml-timestamps"]).is_err());
        let all = vec!["-v".to_string()];
        let mut a = Argv { all: &all, i: 0 };
        for f in ["-v", "--verbose", "--no-gc-warning", "--indirect"] {
            assert_eq!(common_flag(&mut Common::default(), &mut a, f), Ok(true));
        }
        let noted = ignored::noted("ignored-flag");
        for f in ["--accept-flake-config", "--eval-store", "--verbose"] {
            assert_eq!(noted.iter().filter(|n| *n == f).count(), 1, "{f}");
        }
        for f in [
            "-v",
            "--no-gc-warning",
            "--indirect",
            "--extra-experimental-features",
        ] {
            assert!(!noted.iter().any(|n| n == f), "{f}");
        }
    }
}
