//! `iets --help`, `iets COMMAND --help`, `--version`, and the one way a
//! native command reports a bad command line.

use crate::cli::{classic, docs, plumbing, prefetch, render};
use crate::log;
use crate::session::{Session, exit};

pub const OVERVIEW: &str = "\
usage: iets COMMAND [ARGS...]

A Nix evaluator, and a client that builds what it evaluates.

commands:
  eval           evaluate an expression or a file and print the value
  build          evaluate to derivations, one or a set of them, and build them
  search         find packages by regex, like 'nix search'
  diff           print what differs between two derivations
  eval-jobs      print a jobset's derivations as JSON lines (nix-eval-jobs)
  print-dev-env  print the environment of a flake's devShell, for direnv
  prefetch       print the hash of a URL or of a fixed-output derivation
  lint           list the lints and the levels a run would use
  parse          parse Nix files and print or check the syntax tree
  cache          sweep old cache entries, or dump a cache entry's read set
  completion     print the tab-completion script of a shell
  help, version

classic: 'iets NAME ARGS...' with NAME one of nix-instantiate, nix-build,
  nix-shell, nix-eval-jobs or nix acts like that Nix command,
  and falls back to the real one for the rest ('iets help classic').

Scripting: JSON, stable exit codes, plumbing ops: 'iets help plumbing'.

Every command also takes --iets-option NAME VALUE.
Run 'iets COMMAND --help' for the flags of a command.
";

const INPUT: &str = "
common:
  -E, --expr EXPR       evaluate EXPR instead of FILE
  -f, --file FILE       the same as the positional FILE
  -A, --attr PATH       select the attribute PATH; a top-level function is
                        called with the --args at every step of it
  --arg N EXPR, --argstr N STR  call a top-level function with N = EXPR/\"STR\"
  -I, --include P=PATH  make <P> resolve to PATH
  --impure, --pure-eval  turn pure evaluation off or on
  --workers N           evaluator threads (default: the cores, at most 4)
  --timings             report each phase and the cache on stderr
  --no-tracing-cache    do not use the tracing cache
  --no-ir-cache         do not use the compiled-file cache
  --no-cache            do not use either of them
  --cache-dir DIR       keep the three cache stores in DIR
  --log-format F        internal-json: stderr as Nix JSON records
  --show-trace          put the trace in the JSON error records too

see: 'iets help commands' for every flag, 'iets help errors'
  for the codes and the records.
";

const EVAL: &str = "\
usage: iets eval [FLAGS] (-E EXPR | FILE | FLAKEREF#attr)

Evaluate an expression, a file or a locked flake and print the value.

  --strict              evaluate the whole value, not only the top level
  --json                print the value as JSON
  --raw                 print a string as it is, without quotes or newline
  --dump-drvs DIR       write the ATerm of each derivation to DIR
  --lint NAME=LEVEL     set a lint to off, warn or error
  --lint-root DIR       report lint sites under DIR too
  --lint-scope root|all  report sites in the lint roots only, or everywhere

'iets help lints' is the lint registry.
";

const BUILD: &str = "\
usage: iets build [FLAGS] (-E EXPR | FILE | FLAKEREF[#ATTR])
       iets build [FLAGS] [--source STOREPATH=LOCAL]... DRV

Evaluate to derivations, send the closure the builder lacks from memory,
build, and print the output paths ('iets help build-remotely'). A set of
derivations is built whole, each member to its own verdict: 'iets build
.#checks' builds the checks of this system.

  --builder URL         daemon, unix:///path, ssh-ng://[user@]host or
                        grpc://host:port (default: the builder setting)
  --no-build            upload the closure, do not build
  --dry-run             upload the closure, print what would be built and
                        fetched, do not build; --json: as JSON on stdout
  --json                print {\"outcomes\"} on stdout, not the paths
  -k, --keep-going      one root: build what a failure does not block
  --out NAME            build the output NAME only (repeatable)
  --fetch-to DIR        copy the outputs from the builder to DIR
  -o, --out-link PATH   bring the outputs into the local store and link
                        them at PATH, PATH-1, ... (GC roots)
  --source SP=LOCAL     with DRV: the local tree of the input source SP
";

const SEARCH: &str = "\
usage: iets search [INSTALLABLE] [FLAGS] REGEX...

Print the packages whose attribute path, name or description match every
REGEX ('iets help commands'). INSTALLABLE is a flake reference, a path or
<name>; the default is <nixpkgs>. '^' matches every package.

  -e, --exclude REGEX   hide the packages this matches
  --json                print one object per package, keyed by attribute
  --pretty, --no-pretty  indent the JSON, or do not (default: a terminal)
  -f, --file FILE, --expr EXPR  search a file or an expression instead
  --arg N EXPR, --argstr N STR  call a top-level function with N = EXPR/\"STR\"
  -I, --include P=PATH  make <P> resolve to PATH
  --impure, --pure-eval  turn pure evaluation off or on
  --workers N           evaluator threads (default: the cores, at most 4)
  --timings             report the index cache on stderr
  --no-tracing-cache    do not use the index, which the tracing cache holds
  --no-ir-cache         do not use the compiled-file cache
  --no-cache            do not use either of them
  --cache-dir DIR       keep the index in DIR
  --log-format F        internal-json: stderr as Nix JSON records
  --show-trace          put the trace in the JSON error records too
";

const DIFF: &str = "\
usage: iets diff [FLAGS] A B
       iets diff [FLAGS] -A X -A Y (-E EXPR | FILE | FLAKEREF)

Print what differs between two derivations, in place of nix-diff: the local
differences first, then the input derivations that differ. A and B are
.drv store paths, flake references or files ('iets help commands').
Exit: 0 they are equal, 1 they differ, 2 trouble.

  --depth N             levels of input derivations to walk (default 2)
  --all                 walk every level
  --json                print the difference as one JSON object
  --closure             compare the output closures by package and version,
                        as nvd does; the outputs must be in the store
  --builder URL         the store --closure asks (default: daemon)
  -f, --file FILE       the same as the positional FILE
  -I P=PATH, --arg N E, --argstr N S, --option N V, --impure  as for eval
  --log-format F, --show-trace  as for 'iets eval'
";

const EVAL_JOBS: &str = "\
usage: iets eval-jobs [FLAGS] (-E EXPR | FILE | --flake FLAKEREF[#attr])

Walk a jobset and print one JSON line per derivation, like nix-eval-jobs
('iets help commands').

  -E, --expr EXPR       evaluate EXPR instead of FILE
  --flake               the argument is a flake reference
  --arg N EXPR, --argstr N STR  call the top level with N = EXPR/\"STR\"
  --arg-from-file N F, --arg-from-stdin N  --argstr N from F or stdin
  -I, --include P=PATH  make <P> resolve to PATH
  --impure              turn off the pure evaluation --flake implies
  --workers N           worker processes (default: the cores, at most 4)
  --max-memory-size MB  restart a worker above MB (4096; 0: one process)
  --force-recurse       walk every attribute set, not only marked ones
  --meta, --show-input-drvs  add each job's meta, inputDrvs
  --provenance          add each derivation's source position as pos
  --no-instantiate      do not write the .drv files
  --eval-store URL      the daemon that gets the .drv files (default daemon)
  --gc-roots-dir DIR    add a GC root for each job in DIR
  --reference-lock-file PATH  read PATH instead of the flake's flake.lock
  --check-cache-status  add whether each job is built or substitutable
  --constituents        resolve Hydra aggregate constituents
  --apply EXPR          add the result of EXPR applied to each job
  --select EXPR         walk EXPR applied to the root instead
  --chunks M, --chunk N  evaluate part N (0 to M-1) of M parts of the tree
  --chunk-depth D       split the tree at depth D (default 2)
  --quiet               no summary line on stderr
  --log-format F, --show-trace  as for 'iets eval'
";

const PRINT_DEV_ENV: &str = "\
usage: iets print-dev-env [FLAGS] [FLAKEREF[#attr]]

Print the bash that 'nix print-dev-env' prints for a flake's devShell
(default: .), for direnv ('iets help use-with-direnv').

  --builder URL         the Nix daemon: daemon (default), unix:///path or
                        ssh-ng://[user@]host
  --gc-root PATH        link PATH to the environment and keep it as a GC root
  --json                print what 'nix print-dev-env --json' prints
  --entry-to FILE       write the path of this run's cache entry to FILE,
                        for 'iets cache paths' (see use-with-direnv)
  --workers N, --timings, --cache-dir DIR  as for 'iets eval'
  --no-tracing-cache, --no-ir-cache, --no-cache  as for 'iets eval'
  --log-format F, --show-trace  as for 'iets eval'
";

const LINT: &str = "\
usage: iets lint list [FLAGS] [PATH]

Print the lint registry with the levels a run from PATH (default: the
current directory) uses, then the scope and its roots ('iets help lints').

  --lint NAME=LEVEL     set a lint to off, warn or error
  --lint-root DIR       add a lint root
  --lint-scope root|all  report sites in the lint roots only, or everywhere
";

const PARSE: &str = "\
usage: iets parse [-q | --json] [--log-format F] FILE...

Parse each FILE and print its syntax tree; FILE - reads the list from stdin.

  -q                    print only the parse errors and the totals
  --json                print each tree as one line of JSON
  --log-format F        internal-json: stderr as Nix JSON records

'iets help syntax-tree' is the schema of the JSON.
";

const CACHE: &str = "\
usage: iets cache gc [--ttl DAYS] [--dry-run] [--cache-dir DIR]
       iets cache dump ENTRY
       iets cache paths ENTRY [--json]

gc removes the cache entries nobody read for DAYS days, and the spark
profile ('sparks' in the cache directory) when it went unread that long;
dump prints the
read set of a tracing-cache entry ('iets help caching').

paths prints the paths a watcher has to watch for that entry's answer to
go stale, NUL-separated, or --json with the kind of each. A command names
its own entry with --entry-to.

  --ttl DAYS            the age limit in days
  -n, --dry-run         count what gc would remove, remove nothing
  --cache-dir DIR       sweep DIR instead of the default cache
  --json                one object per path, with its kind
";

const COMPLETION: &str = "\
usage: iets completion bash|zsh|fish|nushell|elvish|powershell

Print the script that completes 'iets' in that shell, for
'source <(iets completion bash)' in a startup file, or for a file the shell
loads by itself (bash-completion, zsh's fpath, fish's completions). Each
script says in its first lines how its shell loads it.

The script asks 'iets __complete WORD...' for the candidates, which
prints one 'CANDIDATE<TAB>DESCRIPTION' per line and then ':N', where N
says what the shell adds: 0 files too, 4 nothing.
";

pub const COMMANDS: &str = "eval build search diff eval-jobs print-dev-env prefetch plumbing lint parse \
     cache completion";

/// The native command being parsed, "iets" before the command word; unset
/// under a classic name.
pub fn set_command(cmd: &'static str) {
    *Session::current().command.lock().unwrap() = Some(cmd);
}

pub fn command() -> Option<&'static str> {
    *Session::current().command.lock().unwrap()
}

/// The help of `cmd` in two parts, `None` for the overview.
fn parts(cmd: Option<&str>) -> Option<[&'static str; 2]> {
    Some(match cmd? {
        "eval" => [EVAL, INPUT],
        "build" => [BUILD, INPUT],
        "search" => [SEARCH, ""],
        "diff" => [DIFF, ""],
        "eval-jobs" => [EVAL_JOBS, ""],
        "print-dev-env" => [PRINT_DEV_ENV, ""],
        "prefetch" => [prefetch::HELP, INPUT],
        "plumbing" => [plumbing::HELP, INPUT],
        "lint" => [LINT, ""],
        "parse" => [PARSE, ""],
        "cache" | "cache-dump" => [CACHE, ""],
        "completion" => [COMPLETION, ""],
        _ => return None,
    })
}

/// Help text on stdout, through the one writer that reports a write error.
pub fn out(text: &str) {
    render::bytes(text.as_bytes());
}

/// `iets help [COMMAND | TOPIC]`, and `iets COMMAND --help`. A name that
/// is both a command and a topic is the command, and the help says where
/// the page is.
pub fn help(cmd: Option<&str>) -> ! {
    match (cmd, parts(cmd)) {
        (None, _) => out(&format!("{OVERVIEW}{}", docs::list())),
        (Some(c), Some([own, input])) => {
            let page = docs::find(c).map(|p| {
                format!(
                    "\nThe page of the same name: 'iets help {}/{c}'.\n",
                    p.quadrant
                )
            });
            out(&format!("{own}{input}{}", page.unwrap_or_default()));
        }
        (Some(c), None) => match docs::find(c) {
            Some(p) => docs::show(p),
            None => {
                let names: Vec<&str> = COMMANDS.split(' ').chain(docs::names()).collect();
                fail(
                    &format!("unknown command or topic '{c}'"),
                    suggest(c, names),
                )
            }
        },
    }
    exit(0)
}

/// A flag the help of a command lists.
#[derive(Debug, PartialEq)]
pub struct Flag {
    pub name: &'static str,
    pub about: &'static str,
    /// The placeholders of the values it takes: `["N", "EXPR"]` for `--arg`.
    pub values: Vec<&'static str>,
}

/// The flags the help of `cmd` lists: the words before the description. A
/// short flag takes the values of the long one it is listed with, as in
/// `-f, --file FILE`.
pub fn flag_table(cmd: &str) -> Vec<Flag> {
    let Some(parts) = parts(Some(cmd)) else {
        let about = "";
        let bare = |name| Flag {
            name,
            about,
            values: vec![],
        };
        return vec![bare("--help"), bare("-h"), bare("--version")];
    };
    let mut table = Vec::new();
    for l in parts.into_iter().flat_map(str::lines) {
        if !l.starts_with("  -") {
            continue;
        }
        let (head, about) = match l[2..].split_once("  ") {
            Some((h, a)) => (h, a.trim()),
            None => (&l[2..], ""),
        };
        // the placeholders follow the flag word of its comma-separated
        // alternative; a bare one takes those of the alternative after it
        let mut alts: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
        for alt in head.split(',').map(str::trim) {
            let mut words = alt.split(' ');
            let Some(name) = words.next().filter(|w| w.starts_with('-')) else {
                continue;
            };
            alts.push((name, words.take_while(|w| !w.starts_with('-')).collect()));
        }
        for i in 0..alts.len() {
            let (name, mut values) = alts[i].clone();
            if values.is_empty() && name.len() == 2 {
                values = alts.get(i + 1).map(|a| a.1.clone()).unwrap_or_default();
            }
            table.push(Flag {
                name,
                about,
                values,
            });
        }
    }
    table
}

fn flags(cmd: &str) -> Vec<&'static str> {
    flag_table(cmd).into_iter().map(|f| f.name).collect()
}

/// The commands of the overview with their one-line descriptions.
pub fn command_summaries() -> Vec<(&'static str, &'static str)> {
    OVERVIEW
        .lines()
        .skip_while(|l| *l != "commands:")
        .skip(1)
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.trim_start().split_once("  "))
        .map(|(n, a)| (n, a.trim()))
        .collect()
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diag = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = diag + usize::from(ca != *cb);
            diag = row[j + 1];
            row[j + 1] = sub.min(row[j] + 1).min(diag + 1);
        }
    }
    row[b.len()]
}

/// The closest candidate, when it is close enough to be a typo: a third of
/// the candidate, or two edits once the candidate is long enough for two to
/// be a slip rather than another word ('buidl' for 'build').
pub fn suggest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let close = candidates.into_iter().map(|c| (distance(word, c), c));
    close
        .filter(|(d, c)| d * 3 <= c.len() || (*d <= 2 && c.len() >= 5))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Every word `iets WORD` takes: the native commands, the classic names it
/// answers to, and the two that are not commands.
pub fn command_words() -> Vec<&'static str> {
    (COMMANDS.split(' '))
        .chain(classic::NAMES.iter().copied())
        .chain(["help", "version"])
        .collect()
}

/// The report of a bad command line under `cmd` ("iets" before the command
/// word, `None` under a classic name).
pub fn usage(cmd: Option<&str>, msg: &str, maybe: Option<&str>) -> log::Report {
    let maybe = maybe.map(|m| format!("did you mean '{m}'?"));
    let help = match cmd {
        Some("iets") => Some("run 'iets --help' for the commands".to_string()),
        Some(c) => Some(format!("run 'iets {c} --help' for the flags")),
        None => None,
    };
    let hint: Vec<String> = maybe.into_iter().chain(help).collect();
    log::Report {
        hint: (!hint.is_empty()).then(|| hint.join("\n")),
        ..log::Report::new("usage", msg)
    }
}

fn fail(msg: &str, maybe: Option<&str>) -> ! {
    exit(usage(command(), msg, maybe).emit())
}

pub fn arg_error(msg: impl AsRef<str>) -> ! {
    fail(msg.as_ref(), None)
}

pub fn unknown_flag(flag: &str) -> ! {
    let maybe = command().and_then(|c| suggest(flag, flags(c)));
    fail(&format!("unknown flag '{flag}'"), maybe)
}

pub fn unknown_command(word: &str) -> ! {
    fail(
        &format!("unknown command '{word}'"),
        suggest(word, command_words()),
    )
}

/// `word` is not one of `subs`, the subcommands of this command.
pub fn unknown_subcommand(word: Option<&str>, subs: &[&'static str]) -> ! {
    match word {
        None => arg_error(format!("missing subcommand: {}", subs.join(" or "))),
        Some(w) => fail(
            &format!("unknown subcommand '{w}'"),
            suggest(w, subs.iter().copied()),
        ),
    }
}

/// `argv[i]`, the value of `flag`.
pub fn value<'a>(argv: &'a [String], i: usize, flag: &str) -> &'a String {
    let expr = matches!(flag, "-E" | "--expr");
    let wants = if expr { "an expression" } else { "a value" };
    argv.get(i)
        .unwrap_or_else(|| arg_error(format!("{flag} needs {wants}")))
}

/// Calls `f` with each argument; `next` takes the one after it as its value.
pub fn each_arg(argv: &[String], mut f: impl FnMut(&str, &mut dyn FnMut() -> String)) {
    let mut i = 0;
    while i < argv.len() {
        let s = argv[i].as_str();
        f(s, &mut || {
            i += 1;
            value(argv, i, s).clone()
        });
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn help_fits_80_columns() {
        let all = COMMANDS.split(' ').filter_map(|c| parts(Some(c))).flatten();
        for l in all.chain([OVERVIEW]).flat_map(str::lines) {
            assert!(l.chars().count() <= 80, "{l}");
        }
    }

    #[rstest]
    #[case("eval", "--stirct", Some("--strict"))]
    #[case("build", "--no-ir-cach", Some("--no-ir-cache"))]
    #[case("eval-jobs", "--chunk-dept", Some("--chunk-depth"))]
    #[case("build", "--keep-goin", Some("--keep-going"))]
    #[case("build", "\"A", None)]
    #[case("eval", "-x", None)]
    fn suggestions_come_from_the_help(
        #[case] cmd: &str,
        #[case] typo: &str,
        #[case] want: Option<&str>,
    ) {
        assert_eq!(suggest(typo, flags(cmd)), want);
    }

    #[rstest]
    #[case("buidl", Some("build"))]
    #[case("evl", Some("eval"))]
    #[case("serach", Some("search"))]
    #[case("verison", Some("version"))]
    #[case("nix-buidl", Some("nix-build"))]
    #[case("nix-instantate", Some("nix-instantiate"))]
    #[case("eval-jbos", Some("eval-jobs"))]
    #[case("bogus", None)]
    #[case("x", None)]
    fn a_mistyped_command_is_suggested(#[case] typo: &str, #[case] want: Option<&str>) {
        assert_eq!(suggest(typo, command_words()), want);
    }

    #[test]
    fn messages() {
        let t = usage(Some("eval"), "unknown flag '--stirct'", Some("--strict")).text();
        let want = "did you mean '--strict'?\nrun 'iets eval --help' for the flags";
        assert_eq!(t, format!("error: unknown flag '--stirct'\n{want}"));
        assert_eq!(usage(None, "x", None).text(), "error: x");
    }
}
