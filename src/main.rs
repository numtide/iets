use std::env;
use std::path::Path;
use std::process::exit;
use std::thread::Builder;

use iets::cli::{self, help};

/// The demand path has this stack whether it runs here or on a fiber.
const STACK_SIZE: usize = iets::parallel::fiber::MAIN_STACK;

/// Rust masks SIGPIPE, so a closed stdout (`iets eval-jobs | head`) surfaces
/// as an EPIPE panic. The default disposition makes the process die quietly.
fn restore_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// The classic Nix name this process was invoked under, if any.
fn classic_name() -> Option<String> {
    let argv0 = env::args_os().next()?;
    let base = Path::new(&argv0).file_name()?.to_str()?.to_string();
    cli::classic::NAMES.contains(&base.as_str()).then_some(base)
}

/// Classic commands need the evaluator's stack, like the subcommands below.
fn on_big_stack(f: impl FnOnce() -> i32 + Send + 'static) -> i32 {
    Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(iets::session::bind(f))
        .expect("spawn evaluator thread")
        .join()
        .unwrap_or(1)
}

fn main() {
    restore_sigpipe();
    let session = iets::session::Session::new();
    // a command line says what it has to say on stderr; a library says it
    // to whatever sink its host set, or to none (docs/reference/library.md)
    session.log.write_to_stderr();
    let _entered = session.enter();
    let argv: Vec<String> = env::args_os().skip(1).map(iets::argv::encode).collect();
    // `--iets-option` is ours, so it leaves the argv here: a classic fallback
    // execs the real nix with what is left (docs/reference/settings.md).
    let (argv, options) = match iets::settings::take_options(&argv) {
        Ok(a) => a,
        Err(e) => {
            e.report();
            exit(2)
        }
    };
    // the log format shapes every record, an error's included, so it is read
    // before the first thing that can fail
    if let Some(i) = argv.iter().position(|a| a == "--log-format") {
        let _ = iets::log::set_format(argv.get(i + 1).map_or("", String::as_str));
    }
    iets::log::show_progress();
    // `version` and `help` read no setting, so a settings file with a
    // mistake in it does not stop them
    if classic_name().is_none() {
        match argv.first().map(String::as_str) {
            Some("--version" | "version") => {
                cli::render::bytes(concat!(env!("IETS_VERSION"), "\n").as_bytes());
                exit(0)
            }
            Some("--help" | "-h" | "help") => {
                help::set_command("iets");
                help::help(argv.get(1).map(String::as_str))
            }
            _ => {}
        }
    }
    if let Err(e) = session.load_settings(&options) {
        exit(e.report())
    }
    session.events.open(session.settings());
    iets::profile::open(&session);
    if let Some(name) = classic_name() {
        let code = on_big_stack(move || cli::classic::dispatch(&name, &argv));
        session.profile.write();
        exit(code)
    }
    let Some(cmd) = argv.first() else {
        eprint!("{}", help::OVERVIEW);
        exit(2)
    };
    if cli::classic::NAMES.contains(&cmd.as_str()) {
        let name = cmd.clone();
        let rest: Vec<String> = argv[1..].to_vec();
        let code = on_big_stack(move || cli::classic::dispatch(&name, &rest));
        session.profile.write();
        exit(code)
    }
    let mut rest: Vec<String> = argv[1..].to_vec();
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
    // these read their flags in order, so `--help` reaches `eval_flag` where
    // it stands and a flag before it may take it as a value
    if !matches!(name, "eval" | "diff" | "eval-jobs")
        && rest.iter().any(|a| a == "--help" || a == "-h")
    {
        help::help(Some(name))
    }
    let code = match name {
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
    };
    session.profile.write();
    exit(code)
}
