//! iets's own settings: the `iets.toml` files, `--iets-option` and every
//! `IETS_*` environment knob, read once into one struct. Each key, its type,
//! its default and its effect: docs/reference/settings.md.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::error::{Error, Result};

/// Keys a settings file may set. Nothing else: the file is ours, so a name
/// that is not here is a mistake, not a setting for someone else.
const KEYS: &[&str] = &[
    "build-gc-root",
    "builder",
    "cache",
    "cache-dir",
    "fetch-max-size",
    "fiber-stack-mb",
    "fix-hashes",
    "nix-bin-dir",
    "no-fallback",
    "provenance",
    "shell-gc-root",
    "source-map",
    "workers",
];

/// The settings of this process: the files, then the environment for the knobs
/// that have one.
pub struct Settings {
    /// `builder`: where derivations are realised. `None` falls back to the
    /// nix.conf `store` setting; resolve it with [`builder`].
    pub builder: Option<String>,
    /// `source-map`: `STOREPATH=LOCALDIR` pairs (docs/explanation/provenance.md).
    pub source_map: Vec<(String, String)>,
    /// `workers`, `IETS_WORKERS`: worker threads. `None` asks the machine.
    pub workers: Option<usize>,
    /// `fiber-stack-mb`, `IETS_FIBER_STACK_MB`: fiber stack size in MiB.
    pub fiber_stack_mb: Option<usize>,
    /// `IETS_SPIN`: scans an idle searcher makes before it parks.
    pub spin: u32,
    /// `IETS_SPINNERS`: idle workers that may search at once. `None` is
    /// a quarter of the workers.
    pub spinners: Option<usize>,
    /// `IETS_HANDOFF`: queued tasks per awake worker that wake a sleeper.
    pub handoff: usize,
    /// `IETS_FANOUT` bitmask: 1 deep, 2 coerce, 4 list builtins.
    pub fanout: u32,
    /// `IETS_PAR_STATS`: 1 the summary line, 2 adds the timeline.
    pub par_stats: u8,
    /// `IETS_STATS`: 1 the evaluator counters, 2 adds the memory census.
    pub stats: u8,
    /// `IETS_EVENTS`: the event groups this run watches, as a bit per
    /// `events::Group`. 0 watches none, which is the normal case.
    pub events: u32,
    /// `IETS_EVENTS_TO`: where the events go. `None` is stderr.
    pub events_to: Option<String>,
    /// `IETS_PROFILE`: sample the call
    /// stack, weighed by time (`eval`) or by arena bytes (`alloc`).
    pub profile: Option<crate::profile::Mode>,
    /// `IETS_PROFILE_TO`: where the folded lines go.
    /// `None` is stderr.
    pub profile_to: Option<String>,
    /// `IETS_PROFILE_HZ`: samples a second. Nix's `eval-profiler*` settings are
    /// not read (src/config.rs).
    pub profile_hz: u32,
    /// `cache`, `IETS_CACHE`: the tracing and IR caches.
    pub cache: bool,
    /// `cache-dir`, `IETS_CACHE_DIR`: cache root, below `--cache-dir`.
    pub cache_dir: Option<PathBuf>,
    /// `IETS_IR_STATS`: one line per compiled file on stderr.
    pub ir_stats: bool,
    /// `IETS_IR_MIN`: sources below this size are not cached.
    pub ir_min: usize,
    /// `IETS_NAR_READERS`: threads that read a tree ahead of its NAR hash.
    /// 0 hashes on the calling thread alone.
    pub nar_readers: usize,
    /// `provenance`, `IETS_PROVENANCE`: `false` prints plain store paths.
    pub provenance: bool,
    /// `nix-bin-dir`, `IETS_NIX_BIN_DIR`: where the real Nix binaries live.
    pub nix_bin_dir: Option<PathBuf>,
    /// `no-fallback`, `IETS_NO_FALLBACK`: fail instead of running the real Nix.
    pub no_fallback: bool,
    /// `shell-gc-root`: `nix-shell` and `nix develop` keep the closure they
    /// entered alive with a link in iets's roots store
    /// (docs/explanation/caching.md). `false` keeps none, for a small disk.
    pub shell_gc_root: bool,
    /// `build-gc-root`: `iets build` keeps the outputs it printed alive, and
    /// `--no-out-link`/`--no-link` keeps a root instead of nothing
    /// (docs/reference/divergences.md). `false` is Nix's behaviour back.
    pub build_gc_root: bool,
    /// `IETS_FETCHING`: set on the Nix we call to fetch a flake input, so
    /// a recursive fetch is an error instead of a loop.
    pub fetching: bool,
    /// `IETS_OFFLINE`: any download is an error. A flake whose inputs the
    /// input store already holds evaluates; one that would fetch fails and says
    /// which URL it wanted.
    pub offline: bool,
    /// `fetch-max-size`: the most bytes one download may decompress to.
    pub fetch_max_size: u64,
    /// `fix-hashes`: what `iets build` does with a wrong fixed-output hash
    /// (docs/reference/settings.md).
    pub fix_hashes: FixHashes,
    /// The `--iets-option` pairs this run was given, in flag order. A
    /// command that starts a second iets passes them on: they left the argv
    /// in [`take_options`], so they are nowhere else.
    pub options: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixHashes {
    /// Asks on a terminal; elsewhere only the hint.
    Ask,
    Always,
    Never,
}

/// The flag every iets command and every classic name accepts. It is ours, so
/// it never reaches the real nix: [`take_options`] removes it from the argv a
/// fallback execs.
pub const FLAG: &str = "--iets-option";

/// What [`take_options`] splits a command line into.
pub type Taken = (Vec<String>, Vec<(String, String)>);

/// `argv` without its `--iets-option name value` triples, and the pairs
/// themselves for [`Settings::load`]. Call it before any setting is read.
pub fn take_options(argv: &[String]) -> Result<Taken> {
    let mut rest = Vec::with_capacity(argv.len());
    let mut opts = Vec::new();
    let mut i = 0;
    // `--` ends this command's flags: what follows belongs to whatever the
    // command runs, and that may be a second iets with options of its own
    let end = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    while i < argv.len() {
        if argv[i] != FLAG || i >= end {
            rest.push(argv[i].clone());
            i += 1;
            continue;
        }
        let (Some(name), Some(value)) = (argv.get(i + 1), argv.get(i + 2)) else {
            return Err(Error::Usage(format!("'{FLAG}' wants a name and a value")));
        };
        opts.push((name.clone(), value.clone()));
        i += 3;
    }
    Ok((rest, opts))
}

/// Where a command builds: its own flag, else the `builder` setting, else the
/// nix.conf `store` setting.
pub fn builder(flag: Option<&str>, cfg: &crate::config::Config) -> String {
    flag.map(str::to_string)
        .or_else(|| {
            crate::session::Session::current()
                .settings()
                .builder
                .clone()
        })
        .unwrap_or_else(|| cfg.store())
}

/// The user's settings file. It may set every key; a project's `iets.toml`
/// may not ([`check_project`]). There is no machine file: iets is a client,
/// and what it builds with is the user's (docs/reference/settings.md).
pub fn conf_file() -> PathBuf {
    crate::config::config_home().join("iets/iets.toml")
}

/// Refuses every key of a project's `iets.toml` but `lint`. The file comes
/// with whatever repository was cloned, so it may not decide what runs
/// (`nix-bin-dir`), where sources go (`builder`) or what gets rewritten
/// (`fix-hashes`): docs/reference/settings.md has the threat model.
pub fn check_project(text: &str, origin: &Path) -> std::result::Result<(), String> {
    // a file that does not parse is `apply_toml`'s to report
    let table = text.parse::<toml::Table>().unwrap_or_default();
    let Some(key) = table.keys().find(|k| *k != "lint") else {
        return Ok(());
    };
    let file = conf_file();
    let file = file.display();
    let why = match KEYS.contains(&key.as_str()) {
        true => format!("a repository controls this file; set it in {file}"),
        false => "it holds only [lint]".to_string(),
    };
    let at = origin.display();
    Err(format!(
        "{at}: '{key}' may not be set in a project file: {why}"
    ))
}

impl Settings {
    /// The file, then `$IETS_CONFIG`, then `overrides`, then the environment
    /// for the knobs that have a variable. Later wins.
    pub fn load(overrides: &[(String, String)]) -> Result<Settings> {
        Settings::load_from(&conf_file(), overrides, &Env(|n| std::env::var_os(n)))
    }

    fn load_from(file: &Path, overrides: &[(String, String)], env: &Env) -> Result<Settings> {
        let mut c = Conf::default();
        match std::fs::read_to_string(file) {
            Ok(text) => c.apply(&text, &file.display().to_string(), true)?,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(e) => return Err(Error::Config(format!("{}: {e}", file.display()))),
        }
        if let Some(text) = env.str("IETS_CONFIG") {
            c.apply(&text, "$IETS_CONFIG", false)?;
        }
        for (name, value) in overrides {
            c.set(name, flag_value(name, value), FLAG, false)?;
        }
        Ok(Settings {
            options: overrides.to_vec(),
            ..Settings::from_conf(&c, env)?
        })
    }

    /// The `--iets-option` triples this run was given, for the argv of an
    /// iets it starts.
    pub fn option_argv(&self) -> Vec<String> {
        let triple = |(n, v): &(String, String)| [FLAG.to_string(), n.clone(), v.clone()];
        self.options.iter().flat_map(triple).collect()
    }

    /// The settings `c` asks for, with the environment on top.
    fn from_conf(c: &Conf, env: &Env) -> Result<Settings> {
        let source_map: Vec<(String, String)> = c
            .get("source-map", "a list of \"STOREPATH=LOCALDIR\"", |v| {
                let pair = |e: &toml::Value| {
                    let (store, local) = e.as_str()?.split_once('=')?;
                    Some((store.to_string(), local.to_string()))
                };
                v.as_array()?.iter().map(pair).collect()
            })?
            .unwrap_or_default();
        for (_, local) in &source_map {
            if !Path::new(local).is_dir() {
                crate::log::line(&format!(
                    "warning: source-map: '{local}' is not a directory"
                ));
            }
        }
        let profile = match env.str("IETS_PROFILE").as_deref() {
            None => None,
            Some(w) => match crate::profile::Mode::parse(w) {
                Some(m) => Some(m),
                None => {
                    return Err(Error::Config(format!(
                        "IETS_PROFILE: '{w}' is not 'eval' or 'alloc'"
                    )));
                }
            },
        };
        let workers: Option<usize> = env.num("IETS_WORKERS")?.or(c.int("workers")?);
        if workers == Some(0) {
            return Err(Error::Config("workers must be 1 or more".into()));
        }
        // A profile is read per fiber, and a fiber's stack starts at the thunk
        // it was handed, so at more than one worker the graph is a pile of
        // two-frame stacks and no shape (docs/reference/profile.md). Profiling
        // therefore pins the worker count rather than quietly misleading.
        let workers = match (profile, workers) {
            (Some(_), None) => Some(1),
            (Some(_), Some(1)) => Some(1),
            (Some(_), Some(n)) => {
                return Err(Error::Config(format!(
                    "a profile needs one worker, so workers must be 1, not {n}"
                )));
            }
            (None, w) => w,
        };
        let fix_hashes = c.get(
            "fix-hashes",
            "\"ask\", \"always\" or \"never\"",
            |v| match v.as_str()? {
                "ask" => Some(FixHashes::Ask),
                "always" => Some(FixHashes::Always),
                "never" => Some(FixHashes::Never),
                _ => None,
            },
        )?;
        Ok(Settings {
            builder: c.str("builder")?,
            source_map,
            workers,
            fiber_stack_mb: env.num("IETS_FIBER_STACK_MB")?.or(c.int("fiber-stack-mb")?),
            spin: env.num("IETS_SPIN")?.unwrap_or(2000),
            spinners: env.num("IETS_SPINNERS")?,
            handoff: env.num("IETS_HANDOFF")?.unwrap_or(8),
            fanout: env.num("IETS_FANOUT")?.unwrap_or(7),
            par_stats: env.level("IETS_PAR_STATS"),
            stats: env.level("IETS_STATS"),
            events: match env.str("IETS_EVENTS") {
                Some(names) => crate::events::Group::mask(&names)
                    .map_err(|e| Error::Config(format!("IETS_EVENTS: {e}")))?,
                None => 0,
            },
            events_to: env.str("IETS_EVENTS_TO"),
            profile,
            profile_to: env.str("IETS_PROFILE_TO"),
            profile_hz: env.num("IETS_PROFILE_HZ")?.unwrap_or(99),
            cache: env.flag("IETS_CACHE", c.bool("cache")?, true)?,
            cache_dir: env.dir("IETS_CACHE_DIR").or(c.str("cache-dir")?),
            ir_stats: env.set("IETS_IR_STATS"),
            ir_min: env.num("IETS_IR_MIN")?.unwrap_or(1024),
            nar_readers: env.num("IETS_NAR_READERS")?.unwrap_or(8),
            provenance: env.flag("IETS_PROVENANCE", c.bool("provenance")?, true)?,
            nix_bin_dir: env.dir("IETS_NIX_BIN_DIR").or(c.str("nix-bin-dir")?),
            no_fallback: env.flag("IETS_NO_FALLBACK", c.bool("no-fallback")?, false)?,
            shell_gc_root: c.bool("shell-gc-root")?.unwrap_or(true),
            build_gc_root: c.bool("build-gc-root")?.unwrap_or(true),
            fetching: env.set("IETS_FETCHING"),
            offline: env.set("IETS_OFFLINE"),
            fetch_max_size: c.int("fetch-max-size")?.unwrap_or(16 << 30),
            fix_hashes: fix_hashes.unwrap_or(FixHashes::Ask),
            options: Vec::new(),
        })
    }
}

impl Default for Settings {
    /// The environment knobs alone, for a session that read no file. A knob
    /// that does not parse is [`Settings::load`]'s to report, so here the
    /// whole environment is dropped instead of failing a library's session.
    fn default() -> Settings {
        let empty = || Settings::from_conf(&Conf::default(), &Env(|_| None));
        Settings::from_conf(&Conf::default(), &Env(|n| std::env::var_os(n)))
            .or_else(|_| empty())
            .expect("no file and no environment cannot fail")
    }
}

/// The VALUE of `--iets-option NAME VALUE`: a TOML boolean, integer, string
/// or array when it reads as one, else the text itself, so `fix-hashes always`
/// needs no quotes. Bare text for `source-map` is a whitespace-separated list.
fn flag_value(name: &str, value: &str) -> toml::Value {
    use toml::Value::{Array, Boolean, Integer, String};
    let table = format!("v = {value}").parse::<toml::Table>();
    match table.ok().and_then(|mut t| t.remove("v")) {
        Some(v @ (Boolean(_) | Integer(_) | String(_) | Array(_))) => v,
        _ if name == "source-map" => {
            Array(value.split_whitespace().map(|e| String(e.into())).collect())
        }
        _ => String(value.into()),
    }
}

/// What the settings sources said: each key's TOML value and where it was
/// set, for an error message. A later source replaces a key, except
/// `source-map`, whose entries add up.
#[derive(Debug, Default)]
struct Conf {
    at: BTreeMap<String, (toml::Value, String)>,
}

impl Conf {
    fn apply(&mut self, text: &str, at: &str, file: bool) -> Result<()> {
        let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
            Error::Config(format!("{at}: {}", e.to_string().trim_end()))
        })?;
        for (name, value) in table {
            self.set(&name, value, at, file)?;
        }
        Ok(())
    }

    /// `file` is a settings file, where a `[lint]` table is `lint::Config`'s
    /// to read.
    fn set(&mut self, name: &str, value: toml::Value, at: &str, file: bool) -> Result<()> {
        if name == "lint" && file {
            return Ok(());
        }
        if !KEYS.contains(&name) {
            return Err(Error::Config(format!(
                "unknown iets setting '{name}' in {at}"
            )));
        }
        let mut at = at.to_string();
        let value = match (self.at.remove(name), value) {
            (Some((toml::Value::Array(mut old), before)), toml::Value::Array(new))
                if name == "source-map" =>
            {
                old.extend(new);
                at = format!("{before} and {at}");
                toml::Value::Array(old)
            }
            (_, value) => value,
        };
        self.at.insert(name.to_string(), (value, at));
        Ok(())
    }

    fn get<T>(
        &self,
        name: &str,
        want: &str,
        read: impl Fn(&toml::Value) -> Option<T>,
    ) -> Result<Option<T>> {
        let Some((v, at)) = self.at.get(name) else {
            return Ok(None);
        };
        read(v).map(Some).ok_or_else(|| {
            Error::Config(format!(
                "iets setting '{name}' wants {want}, got {v} in {at}"
            ))
        })
    }

    fn bool(&self, name: &str) -> Result<Option<bool>> {
        self.get(name, "true or false", toml::Value::as_bool)
    }

    fn int<T: TryFrom<i64>>(&self, name: &str) -> Result<Option<T>> {
        self.get(name, "an integer", |v| T::try_from(v.as_integer()?).ok())
    }

    fn str<T: From<String>>(&self, name: &str) -> Result<Option<T>> {
        self.get(name, "a string", |v| Some(v.as_str()?.to_string().into()))
    }
}

/// A Boolean `IETS_*` variable, parsed the way nix.conf parses a Boolean.
fn env_bool(name: &str, v: &std::ffi::OsStr) -> Result<bool> {
    let bad = || Error::Config(format!("{name}: '{}' is not true or false", v.display()));
    crate::config::parse_bool(name, v.to_str().ok_or_else(bad)?).map_err(|_| bad())
}

/// The environment a load reads: the process's, or a test's.
struct Env(fn(&str) -> Option<OsString>);

impl Env {
    fn str(&self, name: &str) -> Option<String> {
        let v = (self.0)(name)?.into_string().ok()?;
        (!v.is_empty()).then_some(v)
    }

    fn num<T: FromStr>(&self, name: &str) -> Result<Option<T>> {
        let Some(v) = (self.0)(name) else {
            return Ok(None);
        };
        v.to_str()
            .and_then(|s| s.parse().ok())
            .map(Some)
            .ok_or_else(|| Error::Config(format!("{name}: '{}' is not a number", v.display())))
    }

    /// A level knob: unset is 0, `2` is 2, any other value is 1, so a knob
    /// set as a plain on/off switch still turns on.
    fn level(&self, name: &str) -> u8 {
        match (self.0)(name) {
            None => 0,
            Some(v) if v == "2" => 2,
            Some(_) => 1,
        }
    }

    fn set(&self, name: &str) -> bool {
        (self.0)(name).is_some()
    }

    /// The environment wins over the file, which wins over `default`. The
    /// value is a nix.conf Boolean, so `IETS_CACHE=false` turns the cache
    /// off and `IETS_NO_FALLBACK=yes` turns the fallback off.
    fn flag(&self, name: &str, file: Option<bool>, default: bool) -> Result<bool> {
        match (self.0)(name) {
            Some(v) => env_bool(name, &v),
            None => Ok(file.unwrap_or(default)),
        }
    }

    fn dir(&self, name: &str) -> Option<PathBuf> {
        (self.0)(name).map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_ENV: Env = Env(|_| None);

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("iets-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("user")).unwrap();
        d.join("user/iets.toml")
    }

    fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    fn load(text: &str) -> Result<Settings> {
        let mut c = Conf::default();
        c.apply(text, "t.toml", true)?;
        Settings::from_conf(&c, &NO_ENV)
    }

    #[test]
    fn each_source_wins_over_the_one_before() {
        let file = scratch("order");
        std::fs::write(
            &file,
            "builder = \"user\"\nworkers = 2\nfiber-stack-mb = 2\ncache = false\nfetch-max-size = 1\n",
        )
        .unwrap();
        let env = Env(|n| match n {
            "IETS_CONFIG" => Some("builder = \"inline\"\nworkers = 3\n".into()),
            "IETS_WORKERS" => Some("5".into()),
            "IETS_CACHE" => Some("1".into()),
            _ => None,
        });
        let s = Settings::load_from(&file, &opts(&[("builder", "flag")]), &env).unwrap();
        assert_eq!(s.builder.as_deref(), Some("flag"));
        assert_eq!(s.workers, Some(5));
        assert_eq!(s.fiber_stack_mb, Some(2));
        assert_eq!(s.fetch_max_size, 1);
        assert!(s.cache, "IETS_CACHE=1 wins over cache = false");
        let s = Settings::load_from(&file, &[], &NO_ENV).unwrap();
        assert_eq!(s.builder.as_deref(), Some("user"));
        assert_eq!(s.workers, Some(2));
        assert!(!s.cache);
        let inline = Env(|n| (n == "IETS_CONFIG").then(|| "workers = 3".into()));
        let s = Settings::load_from(&file, &opts(&[("workers", "4")]), &inline).unwrap();
        assert_eq!(s.workers, Some(4));
        let s = Settings::load_from(&file, &[], &inline).unwrap();
        assert_eq!(s.workers, Some(3));
    }

    #[test]
    fn source_map_adds_up_and_every_other_key_replaces() {
        let file = scratch("lists");
        std::fs::write(&file, "source-map = [\"/nix/store/a=/a\"]\n").unwrap();
        let inline =
            Env(|n| (n == "IETS_CONFIG").then(|| "source-map = [\"/nix/store/b=/b\"]".into()));
        let o = opts(&[("source-map", "/nix/store/c=/c /nix/store/d=/d")]);
        let s = Settings::load_from(&file, &o, &inline).unwrap();
        let stores: Vec<&str> = s.source_map.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(
            stores,
            [
                "/nix/store/a",
                "/nix/store/b",
                "/nix/store/c",
                "/nix/store/d"
            ]
        );
        std::fs::write(&file, "source-map = [\"no-equals-sign\"]\n").unwrap();
        let e = Settings::load_from(&file, &[], &NO_ENV)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(e.contains("'source-map' wants a list"), "{e}");
        assert!(e.contains("user/iets.toml"), "{e}");
    }

    #[test]
    fn a_missing_file_is_skipped_and_the_defaults_hold() {
        let s = Settings::load_from(&scratch("none"), &[], &NO_ENV).unwrap();
        assert_eq!(s.builder, None);
        assert_eq!(s.workers, None);
        assert_eq!(s.fetch_max_size, 16 << 30);
        assert_eq!(s.fix_hashes, FixHashes::Ask);
        assert!(s.cache && s.provenance && s.shell_gc_root && s.build_gc_root);
        assert!(!s.no_fallback);
    }

    #[test]
    fn a_project_file_holds_lint_and_nothing_else() {
        let at = Path::new("/repo/iets.toml");
        assert_eq!(check_project("[lint]\npath-copy = \"warn\"\n", at), Ok(()));
        assert_eq!(check_project("", at), Ok(()));
        for key in KEYS {
            let e = check_project(&format!("{key} = true\n[lint]\n"), at).unwrap_err();
            assert!(
                e.starts_with(&format!(
                    "/repo/iets.toml: '{key}' may not be set in a project file"
                )),
                "{e}"
            );
            assert!(e.contains(&conf_file().display().to_string()), "{e}");
        }
        let e = check_project("nope = 1\n", at).unwrap_err();
        assert!(
            e.ends_with("'nope' may not be set in a project file: it holds only [lint]"),
            "{e}"
        );
        assert_eq!(
            check_project("lint = [\n", at),
            Ok(()),
            "apply_toml reports it"
        );
    }

    #[test]
    fn a_settings_file_may_hold_lint_and_the_other_sources_may_not() {
        assert!(load("[lint]\npath-copy = \"warn\"\n").is_ok());
        let mut c = Conf::default();
        let e = c.apply("[lint]\n", "$IETS_CONFIG", false).unwrap_err();
        assert!(
            e.to_string().contains("setting 'lint' in $IETS_CONFIG"),
            "{e}"
        );
    }

    #[test]
    fn unknown_key_names_the_file() {
        let e = load("builder = \"daemon\"\nnope = 1\n")
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert_eq!(e, "unknown iets setting 'nope' in t.toml");
        let e = load("extra-source-map = []\n").map(|_| ()).unwrap_err();
        assert!(e.to_string().contains("'extra-source-map'"), "{e}");
    }

    #[test]
    fn a_wrong_type_names_the_key_and_the_file() {
        for (text, want) in [
            (
                "workers = \"four\"",
                "'workers' wants an integer, got \"four\" in t.toml",
            ),
            ("workers = -1", "'workers' wants an integer, got -1"),
            ("cache = \"no\"", "'cache' wants true or false, got \"no\""),
            (
                "no-fallback = 1",
                "'no-fallback' wants true or false, got 1",
            ),
            ("builder = 4", "'builder' wants a string, got 4"),
            (
                "fix-hashes = \"yes\"",
                "'fix-hashes' wants \"ask\", \"always\" or \"never\"",
            ),
            (
                "source-map = \"/nix/store/a=/a\"",
                "'source-map' wants a list",
            ),
        ] {
            let e = load(text).map(|_| ()).unwrap_err().to_string();
            assert!(e.contains(want), "{text}: {e}");
            assert!(e.ends_with("in t.toml"), "{text}: {e}");
        }
        let e = load("workers = ").map(|_| ()).unwrap_err().to_string();
        assert!(e.starts_with("t.toml: "), "{e}");
    }

    #[test]
    fn the_flag_value_is_toml_when_it_reads_as_toml() {
        use toml::Value;
        assert_eq!(flag_value("workers", "4"), Value::Integer(4));
        assert_eq!(flag_value("cache", "false"), Value::Boolean(false));
        assert_eq!(
            flag_value("fix-hashes", "always"),
            Value::String("always".into())
        );
        assert_eq!(
            flag_value("fix-hashes", "\"always\""),
            Value::String("always".into())
        );
        assert_eq!(
            flag_value("builder", "ssh-ng://bld1"),
            Value::String("ssh-ng://bld1".into())
        );
        assert_eq!(flag_value("builder", "inf"), Value::String("inf".into()));
        let list = flag_value("source-map", "[\"/nix/store/a=/a\"]");
        assert_eq!(list, flag_value("source-map", "/nix/store/a=/a"));
        let e = Settings::load_from(
            Path::new("/nonexistent"),
            &opts(&[("workers", "four")]),
            &NO_ENV,
        )
        .map(|_| ())
        .unwrap_err()
        .to_string();
        assert!(e.contains("'workers' wants an integer"), "{e}");
        assert!(e.ends_with(&format!("in {FLAG}")), "{e}");
        let s = Settings::load_from(
            Path::new("/nonexistent"),
            &opts(&[("shell-gc-root", "false")]),
            &NO_ENV,
        )
        .unwrap();
        assert!(!s.shell_gc_root);
    }

    #[test]
    fn the_flag_leaves_the_argv() {
        let argv: Vec<String> = ["--parse", FLAG, "workers", "2", "-E", "1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (rest, opts) = take_options(&argv).unwrap();
        assert_eq!(rest, vec!["--parse", "-E", "1"]);
        assert_eq!(opts, vec![("workers".to_string(), "2".to_string())]);
        let bad: Vec<String> = [FLAG, "workers"].iter().map(|s| s.to_string()).collect();
        assert!(take_options(&bad).is_err());
    }

    /// A knob is a nix.conf Boolean, so every spelling of it acts, and one
    /// that is not a Boolean is an error that names the variable.
    #[test]
    fn the_environment_wins_over_the_file() {
        assert!(NO_ENV.flag("X", None, true).unwrap());
        assert!(!NO_ENV.flag("X", Some(false), true).unwrap());
        assert!(NO_ENV.flag("X", Some(true), false).unwrap());
        let zero = Env(|_| Some("0".into()));
        assert!(!zero.flag("X", Some(true), true).unwrap());
        for off in ["0", "no", "false"] {
            assert!(!env_bool("X", off.as_ref()).unwrap(), "{off}");
        }
        for on in ["1", "yes", "true"] {
            assert!(env_bool("X", on.as_ref()).unwrap(), "{on}");
        }
        for bad in ["", "maybe", "2"] {
            let e = env_bool("X", bad.as_ref()).map(|_| ()).unwrap_err();
            assert_eq!(e.to_string(), format!("X: '{bad}' is not true or false"));
        }
        let n = Env(|_| Some("abc".into())).num::<usize>("N");
        assert_eq!(
            n.map(|_| ()).unwrap_err().to_string(),
            "N: 'abc' is not a number"
        );
    }

    /// The pairs go on the argv of the worker processes eval-jobs starts.
    #[test]
    fn the_flag_pairs_are_kept_for_a_second_iets() {
        let o = opts(&[("workers", "2"), ("cache-dir", "/d")]);
        let s = Settings::load_from(Path::new("/nonexistent"), &o, &NO_ENV).unwrap();
        assert_eq!(
            s.option_argv(),
            [FLAG, "workers", "2", FLAG, "cache-dir", "/d"]
        );
        assert_eq!(
            Settings::from_conf(&Conf::default(), &NO_ENV)
                .unwrap()
                .options,
            []
        );
    }

    #[test]
    fn the_file_is_the_users() {
        assert!(conf_file().ends_with("iets/iets.toml"));
    }
}
