//! `iets` as a function, for `main` and for in-process tests.

use std::ffi::{OsStr, OsString};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::thread::Builder;

use crate::api::MAIN_STACK;
use crate::cli::{self, help};
use crate::env::Env;
use crate::session::{self, Exit, Session, exit};
use crate::{log, profile, settings};

pub fn classic_name(argv0: &OsStr) -> Option<String> {
    let base = Path::new(argv0).file_name()?.to_str()?.to_string();
    cli::classic::NAMES.contains(&base.as_str()).then_some(base)
}

/// An `exit` on the evaluator thread has to end the command here too.
fn on_big_stack(f: impl FnOnce() -> i32 + Send + 'static) -> i32 {
    let joined = Builder::new()
        .stack_size(MAIN_STACK)
        .spawn(session::bind(f))
        .expect("spawn evaluator thread")
        .join();
    match joined {
        Ok(code) => code,
        Err(p) => match p.downcast::<Exit>() {
            Ok(e) => exit(e.0),
            Err(_) => 1,
        },
    }
}

pub fn dispatch(session: &Session, argv: Vec<String>, classic: Option<String>) -> i32 {
    // a classic fallback execs the real nix with what is left
    let (argv, options) = match settings::take_options(&argv) {
        Ok(a) => a,
        Err(e) => {
            e.report();
            return 2;
        }
    };
    // read before the first thing that can fail: it shapes an error's record
    if let Some(i) = argv.iter().position(|a| a == "--log-format") {
        let _ = log::set_format(argv.get(i + 1).map_or("", String::as_str));
    }
    log::show_progress();
    // these run before the settings file loads, so a broken file stays harmless
    if classic.is_none() {
        match argv.first().map(String::as_str) {
            Some("--version" | "version") => {
                cli::render::bytes(concat!(env!("IETS_VERSION"), "\n").as_bytes());
                return 0;
            }
            Some("--help" | "-h" | "help") => {
                help::set_command("iets");
                help::help(argv.get(1).map(String::as_str))
            }
            // called on every tab, so a broken settings file would flood the shell
            Some("__complete") => {
                let words = argv[1..].to_vec();
                return on_big_stack(move || cli::complete::cmd_complete(&words));
            }
            _ => {}
        }
    }
    if let Err(e) = session.load_settings(&options) {
        return e.report();
    }
    session
        .log
        .progress
        .set_hidden(!session.settings().progress);
    session.events.open(session.settings());
    profile::open(&Session::current());
    let code = command(argv, classic);
    session.profile.write();
    code
}

fn command(argv: Vec<String>, classic: Option<String>) -> i32 {
    if let Some(name) = classic {
        return on_big_stack(move || cli::classic::dispatch(&name, &argv));
    }
    let Some(cmd) = argv.first() else {
        Session::current().log.err(help::OVERVIEW.as_bytes());
        return 2;
    };
    if cli::classic::NAMES.contains(&cmd.as_str()) {
        let name = cmd.clone();
        let rest: Vec<String> = argv[1..].to_vec();
        return on_big_stack(move || cli::classic::dispatch(&name, &rest));
    }
    let mut rest: Vec<String> = argv[1..].to_vec();
    if let Some(code) = crate::api::run::fine_command(cmd, &rest) {
        return code;
    }
    help::set_command("iets");
    let cmd = match cmd.as_str() {
        "cache-dump" => {
            rest.insert(0, "dump".into());
            "cache"
        }
        c => c,
    };
    let Some(name) = help::COMMANDS.split(' ').find(|c| *c == cmd) else {
        if cmd.starts_with('-') {
            help::unknown_flag(cmd)
        }
        help::unknown_command(cmd)
    };
    help::set_command(name);
    // these read flags in order, so `--help` may be a flag's value
    if !matches!(name, "eval" | "diff" | "eval-jobs")
        && rest.iter().any(|a| a == "--help" || a == "-h")
    {
        help::help(Some(name))
    }
    match name {
        "completion" => cli::complete::cmd_completion(&rest),
        "cache" => match rest.first().map(String::as_str) {
            Some("gc") => cli::cache_gc::cmd_cache_gc(&rest[1..]),
            Some("dump") => match &rest[1..] {
                [file] => cli::cache_dump::cmd_cache_dump(file),
                [] => help::arg_error("cache dump needs an ENTRY file"),
                [_, extra, ..] => help::arg_error(format!("unexpected argument '{extra}'")),
            },
            Some("paths") => match &rest[1..] {
                [file] => cli::cache_paths::cmd_cache_paths(file, false),
                [file, j] if j == "--json" => cli::cache_paths::cmd_cache_paths(file, true),
                [] => help::arg_error("cache paths needs an ENTRY file"),
                [_, extra, ..] => help::arg_error(format!("unexpected argument '{extra}'")),
            },
            w => help::unknown_subcommand(w, &["gc", "dump", "paths"]),
        },
        _ => on_big_stack(move || match name {
            "build" => cli::build::cmd_build(&rest),
            "diff" => cli::diff::cmd_diff(&rest),
            "eval-jobs" => cli::jobs::cmd_eval_jobs(&rest),
            "print-dev-env" => cli::devenv::cmd_print_dev_env(&rest),
            "plumbing" => cli::plumbing::main(&rest),
            "prefetch" => cli::prefetch::cmd_prefetch(&rest),
            "search" => cli::search::cmd_search(&rest),
            "lint" => cli::cmd_lint(&rest),
            "parse" => cli::parse::main(&rest),
            _ => cli::eval::run_eval(cli::parse_eval_args(&rest)),
        }),
    }
}

pub struct Host {
    env: Env,
}

impl Default for Host {
    fn default() -> Host {
        Host {
            env: Env::isolated(),
        }
    }
}

impl Host {
    pub fn env<K: Into<String>, V: Into<OsString>>(
        mut self,
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Host {
        self.env = self.env.with(pairs);
        self
    }
}

pub struct Outcome {
    pub rc: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `iets ARGV...` in a session of its own, writing to buffers.
pub fn run(argv: &[&str], host: &Host) -> Outcome {
    let session = Session::with_env(host.env.clone());
    session.catch_exit.store(true, Ordering::Relaxed);
    let (out, err) = (Sink::default(), Sink::default());
    session.out.write_to(Box::new(out.clone()));
    session.log.write_to(Box::new(err.clone()));
    let _entered = session.enter();
    let argv: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
    let rc = match panic::catch_unwind(AssertUnwindSafe(|| dispatch(&session, argv, None))) {
        Ok(rc) => rc,
        Err(p) => match p.downcast::<Exit>() {
            Ok(e) => e.0,
            Err(p) => panic::resume_unwind(p),
        },
    };
    Outcome {
        rc,
        stdout: out.text(),
        stderr: err.text(),
    }
}

pub use crate::sink::Sink;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_prints_its_value() {
        let r = run(&["eval", "-E", "1 + 2"], &Host::default());
        assert_eq!((r.rc, r.stdout.as_str()), (0, "3\n"), "{}", r.stderr);
    }

    #[test]
    fn a_usage_error_is_an_exit_code_and_a_message() {
        let r = run(&["eval", "--nope"], &Host::default());
        assert_eq!(r.rc, 2);
        assert!(r.stderr.contains("unknown flag '--nope'"), "{}", r.stderr);
        assert_eq!(r.stdout, "");
    }

    #[test]
    fn the_hosts_environment_reaches_the_lint_configuration() {
        let host = Host::default().env([("IETS_LINT", "url-literal=error")]);
        let r = run(&["eval", "-E", "http://example.com"], &host);
        assert_eq!(r.rc, 1, "{}", r.stderr);
        assert!(r.stderr.contains("error: [url-literal]"), "{}", r.stderr);
        let r = run(&["eval", "-E", "http://example.com"], &Host::default());
        assert_eq!(r.rc, 0, "{}", r.stderr);
    }
}
