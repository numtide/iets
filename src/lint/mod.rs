//! Named lints for language corner cases, each `off`, `warn` or `error`.
//! The registry lives here, one line per lint; each family file holds
//! the trigger logic its hooks call (`allow.rs` for `meta`). Levels
//! resolve from the built-in default, then `[lint]` in each `iets.toml`
//! (machine, user, project), then `IETS_LINT`, then `--lint`; inside one
//! source a lint name beats `family:NAME`, which beats `all`. A site is
//! reported only inside the lint roots unless the scope is `all` or the
//! site's locked input has its own levels. See docs/reference/lints.md.

pub mod allow;
pub mod coerce;
pub mod legacy;
pub mod path;
pub mod purity;
pub mod site;

use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{env, fs};

use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, FileInfo, Pos};
use crate::log::{self, Site};
use crate::session::Session;
use crate::syntax::symbol::{FxHashMap, FxHasher};
use crate::{compile, ignored, parallel, project, provenance, settings};
use std::os::unix::ffi::OsStrExt;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum Level {
    #[default]
    Off,
    Warn,
    Error,
}

impl Level {
    /// `off`, `warn`, `error`, plus Nix's `ignore` and `fatal`.
    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "off" | "ignore" => Some(Level::Off),
            "warn" => Some(Level::Warn),
            "error" | "fatal" => Some(Level::Error),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Off => "off",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Path,
    Coerce,
    Legacy,
    Purity,
    Meta,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::Path => "path",
            Family::Coerce => "coerce",
            Family::Legacy => "legacy",
            Family::Purity => "purity",
            Family::Meta => "meta",
        }
    }

    pub fn parse(s: &str) -> Option<Family> {
        [
            Family::Path,
            Family::Coerce,
            Family::Legacy,
            Family::Purity,
            Family::Meta,
        ]
        .into_iter()
        .find(|f| f.name() == s)
    }
}

/// Where a lint fires. Parser and compiler lints run once per file and
/// travel with the IR cache entry; evaluator lints fire on demand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hook {
    Parser,
    Compiler,
    Evaluator,
}

impl Hook {
    pub fn name(self) -> &'static str {
        match self {
            Hook::Parser => "parser",
            Hook::Compiler => "compiler",
            Hook::Evaluator => "evaluator",
        }
    }

    /// The lint runs while a file is compiled, so its level is part of the
    /// IR cache key and its warnings are stored in the entry.
    pub fn is_compile_time(self) -> bool {
        self != Hook::Evaluator
    }
}

pub struct Def {
    pub id: LintId,
    pub name: &'static str,
    pub family: Family,
    pub hook: Hook,
    pub default: Level,
    pub doc: &'static str,
}

macro_rules! registry {
    ($($id:ident: $name:literal, $family:ident, $hook:ident, $default:ident, $doc:literal;)*) => {
        #[repr(u8)]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum LintId { $($id),* }

        pub const LINTS: &[Def] = &[$(Def {
            id: LintId::$id,
            name: $name,
            family: Family::$family,
            hook: Hook::$hook,
            default: Level::$default,
            doc: $doc,
        }),*];
    };
}

registry! {
    AbsolutePathLiteral: "absolute-path-literal", Path, Compiler, Off, "a path literal that starts with `/` or `~/` (Nix: lint-absolute-path-literals)";
    ShortPathLiteral: "short-path-literal", Path, Compiler, Off, "a relative path literal without a leading `./` (Nix: lint-short-path-literals)";
    UrlLiteral: "url-literal", Legacy, Parser, Off, "an unquoted URL (Nix: lint-url-literals)";
    PathCopy: "path-copy", Coerce, Evaluator, Off, "a path coerced to a string with a copy to the store (interpolation, `+`, derivation attrs); not under a flake tree";
    DirCopyFlake: "dir-copy-flake", Coerce, Evaluator, Off, "a directory of the flake tree (its tracked files) coerced to a string is copied to the store whole; narrow it with `lib.fileset` or `builtins.path` with a filter only when needed";
    StorePathRecopy: "store-path-recopy", Coerce, Evaluator, Off, "a path already under the store copied again, which doubles the hash in its name";
    ReadThroughCopy: "read-through-copy", Coerce, Evaluator, Off, "a read (readFile, readDir, import, ...) of a store copy this evaluation made, when the original path would do";
    DrvBoolNull: "drv-bool-null", Coerce, Evaluator, Off, "a Boolean or null coerced into a derivation attribute (`\"1\"`, `\"\"`) by a `derivation` call in scope; Nix's own Boolean attributes excepted";
    DrvList: "drv-list", Coerce, Evaluator, Off, "a list space-joined into a derivation attribute by a `derivation` call in scope (`outputs` and `args` excepted)";
    EqTypeMismatch: "eq-type-mismatch", Coerce, Evaluator, Off, "`==` or `!=` on values of different types (int and float excepted, and null on either side)";
    EqFunction: "eq-function", Coerce, Evaluator, Off, "`==` or `!=` where a side is a function, which is never equal";
    OrNonAttrs: "or-non-attrs", Coerce, Evaluator, Off, "`x.a or d` where the subject of some element is not an attrset";
    ImpureBuiltin: "impure-builtin", Purity, Evaluator, Off, "a `getEnv` call, or a reference to `currentTime`";
    UnhashedFetch: "unhashed-fetch", Purity, Evaluator, Off, "fetchurl, fetchTarball, fetchGit or fetchTree called without a hash, narHash or rev";
    Ifd: "ifd", Purity, Evaluator, Off, "import of a string that carries derivation context";
    Fod: "fod", Purity, Evaluator, Off, "a fixed-output derivation; the message names the derivation and its hash mode";
    LargeSourceCopy: "large-source-copy", Purity, Evaluator, Off, "a path copy whose NAR exceeds `max-bytes` (default 8 MiB)";
    GitInSource: "git-in-source", Purity, Evaluator, Warn, "a path copy of a directory that contains a `.git` entry";
    DirCopyUnfiltered: "dir-copy-unfiltered", Purity, Evaluator, Warn, "a directory coerced to a string is copied whole, untracked and ignored files included; use `builtins.path` with a filter";
    ImpurePathInDrv: "impure-path-in-drv", Purity, Evaluator, Error, "a derivation attribute holds the text of a path outside the store and outside any project root (`toString ./x`), so the .drv depends on the checkout location";
    AbsolutePathInDrv: "absolute-path-in-drv", Purity, Evaluator, Error, "a derivation attribute holds the text of a path under a project root, an absolute path a flake evaluation would have copied";
    TreeMetaInDrv: "tree-meta-in-drv", Purity, Evaluator, Warn, "a derivation attribute holds metadata of the flake's source tree (`self.rev`, `self.lastModifiedDate`, ...), so each commit gives a new .drv and a rebuild; a locked input is silent without `[lint.inputs.<node>]`";
    RevCount: "rev-count", Purity, Evaluator, Warn, "the evaluation reads `revCount` of the flake's source tree, which is deprecated: the count is expensive, a shallow clone has none, and each commit changes it; use `lastModifiedDate` and `shortRev`; a locked input is silent without `[lint.inputs.<node>]`";
    UnknownAllow: "unknown-allow", Meta, Parser, Warn, "a `# iets: allow` comment that names a lint the registry does not know, so a typo does not silence nothing";
}

pub const N: usize = LINTS.len();

/// Nix's names for the lints that predate the registry.
const ALIASES: &[(&str, LintId)] = &[
    ("lint-absolute-path-literals", LintId::AbsolutePathLiteral),
    ("lint-short-path-literals", LintId::ShortPathLiteral),
    ("lint-url-literals", LintId::UrlLiteral),
];

pub const DEFAULT_MAX_BYTES: u64 = 8 << 20;

impl LintId {
    #[inline]
    pub fn def(self) -> &'static Def {
        &LINTS[self as usize]
    }

    #[inline]
    pub fn name(self) -> &'static str {
        self.def().name
    }

    pub fn from_u8(b: u8) -> Option<LintId> {
        LINTS.get(b as usize).map(|d| d.id)
    }
}

pub fn by_name(name: &str) -> Option<LintId> {
    LINTS
        .iter()
        .find(|d| d.name == name)
        .map(|d| d.id)
        .or_else(|| ALIASES.iter().find(|(a, _)| *a == name).map(|(_, id)| *id))
}

/// Which files a lint site is reported in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Scope {
    /// Sites inside a lint root only; a locked input's own levels
    /// (`[lint.inputs.<node>]`) open that input's tree.
    #[default]
    Root,
    /// Every site, the store included.
    All,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "root" => Some(Scope::Root),
            "all" => Some(Scope::All),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scope::Root => "root",
            Scope::All => "all",
        }
    }
}

/// Where a lint site lies, for the scope rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Where {
    /// Under a lint root, through the virtual copies.
    Root,
    /// Inside the tree of the locked input at this index of `Config::inputs`.
    Input(usize),
    /// The store, an input without its own levels, a pseudo-file.
    Elsewhere,
}

/// The effective level of every lint plus the lint knobs and the scope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Config {
    pub levels: [Level; N],
    /// `large-source-copy`: the NAR size a copy may reach.
    pub max_bytes: u64,
    pub scope: Scope,
    /// Canonical absolute directories: the entry directory, then `roots`
    /// of `iets.toml`, then `--lint-root`.
    pub roots: Vec<Vec<u8>>,
    /// `[lint.inputs.<node>]`: the levels that hold inside that locked
    /// input's tree, by lock node name; `None` leaves the scope rule.
    pub inputs: Vec<(String, [Option<Level>; N])>,
}

impl Default for Config {
    fn default() -> Config {
        let mut levels = [Level::Off; N];
        for d in LINTS {
            levels[d.id as usize] = d.default;
        }
        Config {
            levels,
            max_bytes: DEFAULT_MAX_BYTES,
            scope: Scope::Root,
            roots: Vec::new(),
            inputs: Vec::new(),
        }
    }
}

/// How narrowly a selector names its lints: `all`, `family:NAME`, a name.
/// Inside one source the narrower selector wins, whatever the order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Specificity {
    All,
    Family,
    Name,
}

/// `all`, `family:NAME`, a lint name, or a Nix alias, with its specificity.
pub fn select(selector: &str) -> Result<(Specificity, Vec<LintId>), String> {
    if selector == "all" {
        return Ok((Specificity::All, LINTS.iter().map(|d| d.id).collect()));
    }
    if let Some(f) = selector.strip_prefix("family:") {
        let Some(f) = Family::parse(f) else {
            return Err(format!("unknown lint family '{f}'"));
        };
        let ids = LINTS
            .iter()
            .filter(|d| d.family == f)
            .map(|d| d.id)
            .collect();
        return Ok((Specificity::Family, ids));
    }
    match by_name(selector) {
        Some(id) => Ok((Specificity::Name, vec![id])),
        None => Err(format!("unknown lint '{selector}'")),
    }
}

/// The settings of one source (a `[lint]` table, `IETS_LINT`, the `--lint`
/// flags) before they reach the levels. A TOML table has no order, so
/// "later wins" cannot hold inside a source: a name beats `family:NAME`,
/// which beats `all`; among settings of one specificity the later one
/// wins. `apply` writes the result over the levels of the earlier sources.
#[derive(Clone, Debug)]
pub struct Layer {
    levels: [Option<(Specificity, Level)>; N],
}

impl Default for Layer {
    fn default() -> Layer {
        Layer { levels: [None; N] }
    }
}

impl Layer {
    /// `all`, `family:NAME`, a lint name, or a Nix alias.
    pub fn set(&mut self, selector: &str, level: Level) -> Result<(), String> {
        let (spec, ids) = select(selector)?;
        for id in ids {
            let slot = &mut self.levels[id as usize];
            if slot.is_none_or(|(s, _)| spec >= s) {
                *slot = Some((spec, level));
            }
        }
        Ok(())
    }

    /// `set` for a file or `IETS_LINT`: a selector the registry does not
    /// know is one `unknown-lint` warning, and the run goes on. An error
    /// there stops every command in the repository, the one that installs
    /// the fix included.
    fn set_from(&mut self, selector: &str, level: Level, origin: &str) {
        if self.set(selector, level).is_err() {
            let raw = format!(
                "{origin}: iets does not know the lint '{selector}' and ignores it; 'iets help lints' has the list (docs/reference/lints.md)"
            );
            ignored::note("unknown-lint", &format!("{origin} {selector}"), raw);
        }
    }

    /// `NAME=LEVEL`, as `--lint` takes it.
    pub fn set_pair(&mut self, pair: &str) -> Result<(), String> {
        let (name, level) = split_pair(pair)?;
        self.set(name, level)
    }

    pub fn get(&self, id: LintId) -> Option<Level> {
        self.levels[id as usize].map(|(_, l)| l)
    }

    /// Writes every level this layer sets over `levels`.
    pub fn apply(&self, levels: &mut [Level; N]) {
        for (slot, level) in levels.iter_mut().zip(self.levels) {
            if let Some((_, l)) = level {
                *slot = l;
            }
        }
    }
}

fn parse_level(level: &str, name: &str) -> Result<Level, String> {
    Level::parse(level)
        .ok_or_else(|| format!("invalid lint level '{level}' for '{name}': use off, warn or error"))
}

fn split_pair(pair: &str) -> Result<(&str, Level), String> {
    let Some((name, level)) = pair.split_once('=') else {
        return Err(format!("lint setting '{pair}' is not NAME=LEVEL"));
    };
    Ok((name.trim(), parse_level(level.trim(), name)?))
}

fn under(path: &[u8], dir: &[u8]) -> bool {
    path.starts_with(dir) && (path.len() == dir.len() || path[dir.len()] == b'/')
}

impl Config {
    #[inline]
    pub fn level(&self, id: LintId) -> Level {
        self.levels[id as usize]
    }

    /// One selector as a source of its own: `all`, `family:NAME`, a lint
    /// name, or a Nix alias.
    pub fn set(&mut self, selector: &str, level: Level) -> Result<(), String> {
        let mut layer = Layer::default();
        layer.set(selector, level)?;
        layer.apply(&mut self.levels);
        Ok(())
    }

    /// Every lint off: a classic name prints what Nix prints.
    pub fn off() -> Config {
        Config {
            levels: [Level::Off; N],
            ..Config::default()
        }
    }

    /// Adds a lint root; `dir` is made canonical.
    pub fn add_root(&mut self, dir: &[u8]) {
        let dir = compile::canon_path(dir);
        if !self.roots.contains(&dir) {
            self.roots.push(dir);
        }
    }

    pub fn input_index(&self, name: &str) -> Option<usize> {
        self.inputs.iter().position(|(n, _)| n == name)
    }

    /// Whether any site can be silenced by where it lies: false with
    /// `scope = "all"` and no input levels, the fast path.
    #[inline]
    pub fn scoped(&self) -> bool {
        self.scope == Scope::Root || !self.inputs.is_empty()
    }

    /// The level of `id` at a site that lies at `w`.
    pub fn site_level(&self, id: LintId, w: Where) -> Level {
        let global = self.level(id);
        let outside = match self.scope {
            Scope::All => global,
            Scope::Root => Level::Off,
        };
        match w {
            Where::Root => global,
            Where::Input(i) => self.inputs[i].1[id as usize].unwrap_or(outside),
            Where::Elsewhere => outside,
        }
    }

    /// Whether a lint root contains `path` (a local, canonical path).
    pub fn in_root(&self, path: &[u8]) -> bool {
        self.roots.iter().any(|r| under(path, r))
    }

    /// The configuration a file at `w` is compiled under: every level
    /// replaced by its `site_level`.
    pub fn for_where(&self, w: Where) -> Config {
        let mut c = self.clone();
        if w != Where::Root && self.scoped() {
            for d in LINTS {
                c.levels[d.id as usize] = self.site_level(d.id, w);
            }
        }
        c
    }

    fn apply_inputs(&mut self, t: &toml::Table, origin: &str) -> Result<(), String> {
        for (input, levels) in t {
            let Some(levels) = levels.as_table() else {
                return Err(format!("'inputs.{input}' is not a table"));
            };
            let mut layer = Layer::default();
            for (selector, level) in levels {
                let Some(level) = level.as_str() else {
                    return Err(format!(
                        "lint '{selector}' of input '{input}' must be a level string"
                    ));
                };
                layer.set_from(selector, parse_level(level, selector)?, origin);
            }
            let mut arr = [None; N];
            for d in LINTS {
                arr[d.id as usize] = layer.get(d.id);
            }
            match self.input_index(input) {
                Some(i) => self.inputs[i].1 = arr,
                None => self.inputs.push((input.clone(), arr)),
            }
        }
        Ok(())
    }

    /// `NAME=LEVEL` as a source of its own, as `--lint` takes it.
    pub fn set_pair(&mut self, pair: &str) -> Result<(), String> {
        let mut layer = Layer::default();
        layer.set_pair(pair)?;
        layer.apply(&mut self.levels);
        Ok(())
    }

    /// `IETS_LINT="name=level,name=level"`: one source.
    pub fn apply_env(&mut self, s: &str) -> Result<(), String> {
        let mut layer = Layer::default();
        for pair in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (name, level) = split_pair(pair)?;
            layer.set_from(name, level, "IETS_LINT");
        }
        layer.apply(&mut self.levels);
        Ok(())
    }

    /// The `[lint]` table of an `iets.toml`: `name = "level"`, or
    /// `name = { level = "warn", max-bytes = 1000 }` for a lint with knobs,
    /// plus `scope`, `roots` (relative to the file) and `inputs.<node>`.
    pub fn apply_toml(&mut self, text: &str, origin: &Path) -> Result<(), String> {
        let from = origin.display().to_string();
        let at = |m: String| format!("{from}: {m}");
        let table: toml::Table = text.parse().map_err(|e| at(format!("{e}")))?;
        let Some(lint) = table.get("lint") else {
            return Ok(());
        };
        let Some(lint) = lint.as_table() else {
            return Err(at("'lint' is not a table".into()));
        };
        let mut layer = Layer::default();
        for (name, value) in lint {
            match (name.as_str(), value) {
                ("scope", toml::Value::String(s)) => {
                    let Some(s) = Scope::parse(s) else {
                        return Err(at(format!("invalid lint scope '{s}': use root or all")));
                    };
                    self.scope = s;
                }
                ("scope", _) => return Err(at("'scope' must be \"root\" or \"all\"".into())),
                ("roots", toml::Value::Array(dirs)) => {
                    let base = origin.parent().unwrap_or(Path::new("/"));
                    for d in dirs {
                        let Some(d) = d.as_str() else {
                            return Err(at("'roots' must be a list of strings".into()));
                        };
                        self.add_root(base.join(d).as_os_str().as_bytes());
                    }
                }
                ("roots", _) => return Err(at("'roots' must be a list of strings".into())),
                ("inputs", toml::Value::Table(t)) => self.apply_inputs(t, &from).map_err(at)?,
                ("inputs", _) => return Err(at("'inputs' must be a table".into())),
                (_, toml::Value::String(level)) => {
                    let level = parse_level(level, name).map_err(at)?;
                    layer.set_from(name, level, &from);
                }
                (_, toml::Value::Table(_)) if by_name(name).is_none() => {
                    layer.set_from(name, Level::Off, &from);
                }
                (_, toml::Value::Table(t)) => {
                    for (k, v) in t {
                        match (name.as_str(), k.as_str(), v) {
                            (_, "level", toml::Value::String(level)) => {
                                let level = parse_level(level, name).map_err(at)?;
                                layer.set_from(name, level, &from);
                            }
                            ("large-source-copy", "max-bytes", toml::Value::Integer(n)) => {
                                self.max_bytes = (*n).max(0) as u64;
                            }
                            _ => {
                                return Err(at(format!("unknown key '{k}' for lint '{name}'")));
                            }
                        }
                    }
                }
                _ => {
                    return Err(at(format!(
                        "lint '{name}' must be a level string or a table"
                    )));
                }
            }
        }
        layer.apply(&mut self.levels);
        Ok(())
    }

    /// Default, then the settings files, then the project `iets.toml` found
    /// by walking up from `start`, then `IETS_LINT`, then `flags` as one
    /// source. `start` is the first lint root; `roots` (`--lint-root`) follow and
    /// `scope` (`--lint-scope`) wins over the file's.
    pub fn resolve(
        start: Option<&Path>,
        flags: &[(String, Level)],
        roots: &[Vec<u8>],
        scope: Option<Scope>,
    ) -> Result<Config, String> {
        let mut c = Config::default();
        if let Some(dir) = start {
            c.add_root(dir.as_os_str().as_bytes());
        }
        let read = |f: &Path| fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()));
        let settings = settings::conf_file();
        if settings.is_file() {
            c.apply_toml(&read(&settings)?, &settings)?;
        }
        // a walk that starts below the settings directory ends on the file read above
        let is_settings = |f: &PathBuf| {
            let f = f.canonicalize().ok();
            f.is_some() && settings.canonicalize().ok() == f
        };
        if let Some(file) = start.and_then(find_config_file).filter(|f| !is_settings(f)) {
            let text = read(&file)?;
            settings::check_project(&text, &file)?;
            c.apply_toml(&text, &file)?;
        }
        if let Ok(env) = env::var("IETS_LINT") {
            c.apply_env(&env).map_err(|m| format!("IETS_LINT: {m}"))?;
        }
        let mut layer = Layer::default();
        for (selector, level) in flags {
            layer.set(selector, *level)?;
        }
        layer.apply(&mut c.levels);
        for r in roots {
            c.add_root(r);
        }
        if let Some(s) = scope {
            c.scope = s;
        }
        ignored::flush();
        Ok(c)
    }

    /// One byte per lint plus the knobs and the scope: the cache key field.
    pub fn key(&self) -> String {
        let mut s: String = self.levels.iter().map(|l| level_char(*l)).collect();
        s.push(':');
        s.push_str(&self.max_bytes.to_string());
        s.push(':');
        s.push_str(self.scope.name());
        for r in &self.roots {
            s.push(':');
            s.push_str(&String::from_utf8_lossy(r));
        }
        for (name, levels) in &self.inputs {
            s.push(':');
            s.push_str(name);
            s.push('=');
            s.extend(levels.iter().map(|l| l.map_or('-', level_char)));
        }
        s
    }

    /// The compile-time lints only: the IR cache key field. Empty when they
    /// are all off, so entries written before the registry existed stay valid.
    pub fn compile_key(&self) -> String {
        let on = LINTS
            .iter()
            .any(|d| d.hook.is_compile_time() && self.level(d.id) != Level::Off);
        if !on {
            return String::new();
        }
        LINTS
            .iter()
            .map(|d| {
                if d.hook.is_compile_time() {
                    level_char(self.level(d.id))
                } else {
                    '-'
                }
            })
            .collect()
    }
}

fn level_char(l: Level) -> char {
    match l {
        Level::Off => 'o',
        Level::Warn => 'w',
        Level::Error => 'e',
    }
}

/// The nearest project `iets.toml` at or above `start`, stopping at the
/// project root (`crate::project`). The walk must not leave the project: a
/// world-writable `/tmp/iets.toml` would otherwise set a lint level for
/// every evaluation below it, and the file is untrusted input
/// (`settings::check_project`).
pub fn find_config_file(start: &Path) -> Option<PathBuf> {
    let root = project::find(start)?;
    let mut dir = start.to_path_buf();
    loop {
        let f = dir.join("iets.toml");
        if f.is_file() {
            return Some(f);
        }
        if dir == root || !dir.pop() {
            return None;
        }
    }
}

/// `iets lint list`: every lint with its effective level.
pub fn list(c: &Config) -> String {
    let mut out = String::new();
    for d in LINTS {
        let level = c.level(d.id);
        let dflt = if level == d.default {
            String::new()
        } else {
            format!(" (default {})", d.default.name())
        };
        out.push_str(&format!(
            "{:<24} {:<5}{:<16} {:<8} {:<9} {}\n",
            d.name,
            level.name(),
            dflt,
            d.family.name(),
            d.hook.name(),
            d.doc
        ));
    }
    out.push_str(&format!("scope: {}", c.scope.name()));
    if c.scope == Scope::Root {
        let roots: Vec<_> = c
            .roots
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        out.push_str(&format!(" (roots: {})", roots.join(", ")));
    }
    out.push('\n');
    for (name, levels) in &c.inputs {
        let on: Vec<String> = LINTS
            .iter()
            .filter_map(|d| levels[d.id as usize].map(|l| format!("{}={}", d.name, l.name())))
            .collect();
        out.push_str(&format!("input {name}: {}\n", on.join(", ")));
    }
    out
}

/// Warnings wait in `pending` until it holds this much, then go to stderr
/// in one write: 13k lines on nixos-desktop cost 7 % of the wall at 4
/// workers as one syscall each.
const FLUSH_AT: usize = 16 << 10;

type SiteSet = HashSet<(LintId, Pos), BuildHasherDefault<FxHasher>>;

struct Emitted {
    seen: SiteSet,
    /// One record for each warning, as a cache entry keeps them.
    records: Vec<String>,
    /// The trees that have their finding of `rev-count`.
    counted: Vec<String>,
    pending: Vec<u8>,
}

impl Emitted {
    fn flush(&mut self) {
        if !self.pending.is_empty() {
            Session::current().log.err(&self.pending);
            self.pending.clear();
        }
    }
}

impl Drop for Emitted {
    fn drop(&mut self) {
        self.flush();
    }
}

/// The warnings a run emitted. Clones share the emitted set, so every fiber
/// deduplicates against the same sites.
#[derive(Clone)]
pub struct Lints {
    shared: Arc<Mutex<Emitted>>,
    /// Sites this fiber saw reported or silenced: a hit skips the shared
    /// lock, and a firing site is evaluated many times more than it is
    /// reported.
    local_seen: SiteSet,
    /// `Where` of every file this fiber classified, by the file's base.
    where_cache: FxHashMap<u32, Where>,
}

impl Default for Lints {
    fn default() -> Lints {
        Lints {
            shared: Arc::new(Mutex::new(Emitted {
                seen: SiteSet::default(),
                records: Vec::new(),
                counted: Vec::new(),
                pending: Vec::new(),
            })),
            local_seen: SiteSet::default(),
            where_cache: FxHashMap::default(),
        }
    }
}

impl Lints {
    /// The record of every warning emitted so far, in order.
    pub fn records(&self) -> Vec<String> {
        self.shared.lock().unwrap().records.clone()
    }

    /// Writes the buffered warnings to stderr. `parallel::try_run` flushes at
    /// the end of every command, `report` before an error, `trace` and
    /// `warn` before their own line, and the last clone on drop.
    pub fn flush(&self) {
        self.shared.lock().unwrap().flush();
    }

    fn seen(&mut self, id: LintId, pos: Pos) -> bool {
        if self.local_seen.contains(&(id, pos)) {
            return true;
        }
        if self.shared.lock().unwrap().seen.contains(&(id, pos)) {
            self.local_seen.insert((id, pos));
            return true;
        }
        false
    }

    /// A site outside the scope: seen, so no fiber classifies it again.
    fn silence(&mut self, id: LintId, pos: Pos) {
        self.local_seen.insert((id, pos));
        self.shared.lock().unwrap().seen.insert((id, pos));
    }

    /// Records the finding and queues its line; false when another fiber
    /// recorded it first. A finding of `tree` is the one of that tree in the
    /// run, with a site or without; every other finding is the one of its
    /// site.
    fn record(
        &mut self,
        id: LintId,
        pos: Pos,
        line: &str,
        site: Option<Site>,
        tree: Option<&str>,
    ) -> bool {
        let mut g = self.shared.lock().unwrap();
        let first = match tree {
            Some(t) => !g.counted.iter().any(|c| c == t),
            None => {
                self.local_seen.insert((id, pos));
                g.seen.insert((id, pos))
            }
        };
        if !first {
            return false;
        }
        g.counted.extend(tree.map(String::from));
        let (shown, record) = log::lint(line, site);
        g.pending.extend_from_slice(shown.as_bytes());
        g.pending.push(b'\n');
        g.records.push(record);
        if g.pending.len() >= FLUSH_AT {
            g.flush();
        }
        true
    }
}

/// A compile-time diagnostic, carried from the compiler (or an IR cache
/// entry) to the evaluator that reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diag {
    pub id: LintId,
    pub pos: Pos,
    pub msg: String,
}

pub fn error(id: LintId, pos: Pos, msg: &str) -> Box<Error> {
    Error::boxed(ErrorKind::Lint, format!("[{}] {msg}", id.name()), pos)
}

impl Evaluator {
    #[inline]
    pub fn lint_on(&self, id: LintId) -> bool {
        self.lint_config().level(id) != Level::Off
    }

    /// Reports a lint site: nothing at `off`, outside the scope, or on a
    /// line a `# iets: allow` comment covers (`allow`); one warning per
    /// (lint, pos) at `warn`, an error `tryEval` does not catch at `error`.
    #[inline]
    pub fn lint(&mut self, id: LintId, pos: Pos, msg: impl FnOnce() -> String) -> EvalResult<()> {
        if self.lint_config().level(id) == Level::Off {
            return Ok(());
        }
        self.lint_site(id, pos, None, None, msg)
    }

    /// Any fiber reports: the site set keeps a warning to one line, and bailing
    /// like `trace` (docs/explanation/evaluation.md, "Speculation is invisible")
    /// cost 15 % of the wall on nixos-desktop at 4 workers. A site the scope
    /// silences enters the set too; an `error` site never does, so every fiber
    /// that reaches it fails. A `level` is that of the tree a value comes from,
    /// not of the site. A finding of `tree` is one for the tree, so the site set
    /// does not see it.
    #[inline(never)]
    fn lint_site(
        &mut self,
        id: LintId,
        pos: Pos,
        level: Option<Level>,
        tree: Option<&str>,
        msg: impl FnOnce() -> String,
    ) -> EvalResult<()> {
        if tree.is_none() && self.lints.seen(id, pos) {
            return Ok(());
        }
        let file = self.file_of(pos);
        if let Some((f, off)) = file
            && f.allows().covers(off, id)
        {
            self.lints.silence(id, pos);
            return Ok(());
        }
        let level = if let Some(level) = level {
            level
        } else if self.lint_config().scoped() {
            let w = self.where_of_file(file);
            self.lint_config().site_level(id, w)
        } else {
            self.lint_config().level(id)
        };
        match level {
            Level::Off => {
                self.lints.silence(id, pos);
                Ok(())
            }
            Level::Warn => {
                self.lint_print(id, pos, msg(), tree);
                Ok(())
            }
            Level::Error => Err(error(id, pos, &msg())),
        }
    }

    /// Where the file of `pos` lies, classified once per file and fiber.
    pub fn where_of(&mut self, pos: Pos) -> Where {
        let file = self.file_of(pos);
        self.where_of_file(file)
    }

    fn where_of_file(&mut self, file: Option<(&'static FileInfo, u32)>) -> Where {
        let Some((f, _)) = file else {
            return Where::Elsewhere;
        };
        if let Some(w) = self.lints.where_cache.get(&f.base) {
            return *w;
        }
        let w = self.where_of_path(&f.path);
        self.lints.where_cache.insert(f.base, w);
        w
    }

    /// Where a file lies. The entry expression (`«string»`, `«arg»`) is
    /// in root, the other pseudo-files are not. A store path resolves
    /// through the virtual copies to the local tree it stands for, so a
    /// flake root's files land in the root directory; the tree of a
    /// locked input the configuration names is `Input`.
    pub fn where_of_path(&self, path: &[u8]) -> Where {
        if path.starts_with("\u{ab}".as_bytes()) {
            let entry =
                path == "\u{ab}string\u{bb}".as_bytes() || path == "\u{ab}arg\u{bb}".as_bytes();
            return if entry { Where::Root } else { Where::Elsewhere };
        }
        let mut input = None;
        let p = self.local_path(path, |flake| {
            if input.is_none() {
                input = self.lint_config().input_index(flake);
            }
        });
        if self.lint_config().in_root(&p) {
            return Where::Root;
        }
        match input {
            Some(i) => Where::Input(i),
            None => Where::Elsewhere,
        }
    }

    /// The lint configuration the file `path` is compiled under.
    pub fn config_for_file(&self, path: &[u8]) -> Config {
        if !self.lint_config().scoped() {
            return self.lint_config().clone();
        }
        let w = self.where_of_path(path);
        self.lint_config().for_where(w)
    }

    /// The lock node name of the flake root or locked input whose tree
    /// `path` lies in, when the entry is a flake.
    pub fn flake_tree_of(&self, path: &[u8]) -> Option<String> {
        self.known.under(path)?.1.flake
    }

    /// `file:line:col` of a lint site, the local path for a file inside a
    /// store copy this evaluation made. `provenance::describe_pos` freezes
    /// its map on first use and an error report needs the whole run's map,
    /// so this resolves the copy chain through the source table directly.
    fn lint_pos(&self, pos: Pos) -> String {
        let Some((f, off)) = self.file_of(pos) else {
            return self.describe_pos(pos);
        };
        let (l, c) = f.line_col(off);
        let path = if provenance::enabled() {
            self.local_path(&f.path, |_| {})
        } else {
            f.path.clone()
        };
        format!("{}:{l}:{c}", String::from_utf8_lossy(&path))
    }

    /// A source path as a lint message shows it: the local path a store
    /// copy stands for, like a site's position. The message resolves its
    /// own paths so `api::report` can leave a lint error's text alone,
    /// where `provenance::rewrite_msg` would turn the store path a copy
    /// produced into the directory it was copied from.
    pub fn lint_shown_path(&self, path: &[u8]) -> String {
        let path = if provenance::enabled() {
            self.local_path(path, |_| {})
        } else {
            path.to_vec()
        };
        String::from_utf8_lossy(&path).into_owned()
    }

    /// The local path a store copy stands for, through the copy chain and
    /// then `source_map`. `flake` sees the lock node name of each flake tree
    /// on the way, outermost copy first.
    fn local_path(&self, path: &[u8], mut flake: impl FnMut(&str)) -> Vec<u8> {
        let mut p = path.to_vec();
        for _ in 0..8 {
            let Some((root, known)) = self.known.under(&p) else {
                break;
            };
            if let Some(name) = &known.flake {
                flake(name);
            }
            let Some(local) = known.origin.and_then(|o| o.at.local()) else {
                break;
            };
            let mut next = local.as_os_str().as_encoded_bytes().to_vec();
            next.extend_from_slice(&p[root..]);
            p = next;
        }
        for (store, local) in &self.session().settings().source_map {
            if let Some(rest) = p.strip_prefix(store.as_bytes())
                && (rest.is_empty() || rest[0] == b'/')
            {
                let mut next = local.as_bytes().to_vec();
                next.extend_from_slice(rest);
                return next;
            }
        }
        p
    }

    fn lint_print(&mut self, id: LintId, pos: Pos, msg: String, tree: Option<&str>) {
        let line = if pos.is_none() {
            format!("warning: [{}] {msg}", id.name())
        } else {
            format!(
                "warning: [{}] {msg}\n  at {}",
                id.name(),
                self.lint_pos(pos)
            )
        };
        let site = self.lint_site_of(id, pos);
        let tree = tree.map(String::from);
        // a helper may reach a site the program never asks for; the
        // finding then waits for the thunk whose value carries it
        parallel::defer(self, move |ev| {
            ev.lints.record(id, pos, &line, site, tree.as_deref());
        });
    }

    /// The byte span of the site and its edit (`site::finding`), from a
    /// second parse of the file: the IR keeps where a node starts, not where
    /// it ends. `None` for a source that is not a file, such as `-E`.
    pub fn lint_site_of(&self, id: LintId, pos: Pos) -> Option<Site> {
        let (f, off) = self.file_of(pos)?;
        let (src, ast) = f.parsed(|| self.read_source(&f.path, pos).ok())?;
        let (span, replacement) = site::finding(src, ast, id, off)?;
        Some(Site {
            start: span.start.into(),
            end: span.end.into(),
            replacement,
        })
    }

    /// `lint_site_of` for a lint at `error`: its message starts with `[NAME]`.
    pub fn lint_site_of_error(&self, e: &Error) -> Option<Site> {
        let name = e.msg.strip_prefix('[')?.split_once(']')?.0;
        self.lint_site_of(by_name(name)?, e.pos)
    }

    /// Compile-time diagnostics, at the level the compiler saw. A file
    /// compiles the same in every fiber, so a helper prints them itself
    /// instead of bailing; the site set keeps them to one line each.
    pub fn report_diags(&mut self, diags: &[Diag]) {
        for d in diags {
            if !self.lints.seen(d.id, d.pos) {
                self.lint_print(d.id, d.pos, d.msg.clone(), None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A `/tmp/iets.toml` must not set a lint level for a project below it:
    /// the walk stops at the root, and the file is untrusted input.
    #[test]
    fn the_project_file_walk_stops_at_the_project_root() {
        let d = env::temp_dir().join(format!("iets-walk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let deep = d.join("repo/sub");
        fs::create_dir_all(&deep).unwrap();
        fs::write(d.join("iets.toml"), "[lint]\n").unwrap();
        assert_eq!(find_config_file(&deep), Some(d.join("iets.toml")));
        fs::create_dir_all(d.join("repo/.git")).unwrap();
        assert_eq!(find_config_file(&deep), None, "the walk left the project");
        let own = d.join("repo/iets.toml");
        fs::write(&own, "[lint]\n").unwrap();
        assert_eq!(find_config_file(&deep), Some(own));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn names_are_unique_kebab_case() {
        let mut seen = HashSet::new();
        for d in LINTS {
            assert!(seen.insert(d.name), "duplicate lint {}", d.name);
            assert!(
                d.name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit()),
                "{} is not kebab-case",
                d.name
            );
            assert_eq!(by_name(d.name), Some(d.id));
            assert_eq!(LintId::from_u8(d.id as u8), Some(d.id));
        }
    }

    #[test]
    fn selectors_and_order() {
        let mut c = Config::default();
        c.set("all", Level::Warn).unwrap();
        assert!(c.levels.iter().all(|l| *l == Level::Warn));
        c.set("family:path", Level::Error).unwrap();
        assert_eq!(c.level(LintId::AbsolutePathLiteral), Level::Error);
        assert_eq!(c.level(LintId::PathCopy), Level::Warn);
        c.set_pair("absolute-path-literal=off").unwrap();
        assert_eq!(c.level(LintId::AbsolutePathLiteral), Level::Off);
        c.set_pair("lint-url-literals=fatal").unwrap();
        assert_eq!(c.level(LintId::UrlLiteral), Level::Error);
        assert!(c.set("nope", Level::Warn).is_err());
        assert!(c.set("family:nope", Level::Warn).is_err());
        assert!(c.set_pair("absolute-path-literal=loud").is_err());
        c.apply_env("absolute-path-literal=warn, ifd=error")
            .unwrap();
        assert_eq!(c.level(LintId::AbsolutePathLiteral), Level::Warn);
        assert_eq!(c.level(LintId::Ifd), Level::Error);
    }

    #[test]
    fn specificity_inside_a_source_and_order_between_sources() {
        // one table: a name beats its family, which beats `all`, whatever
        // the order TOML hands the keys in
        let mut c = Config::default();
        c.apply_toml(
            "[lint]\nabsolute-path-literal = \"warn\"\n\"family:path\" = \"error\"\nall = \"off\"\ndir-copy-flake = \"warn\"\n\"family:coerce\" = \"error\"\n",
            Path::new("t"),
        )
        .unwrap();
        assert_eq!(c.level(LintId::AbsolutePathLiteral), Level::Warn);
        assert_eq!(c.level(LintId::ShortPathLiteral), Level::Error);
        assert_eq!(c.level(LintId::DirCopyFlake), Level::Warn);
        assert_eq!(c.level(LintId::PathCopy), Level::Error);
        assert_eq!(c.level(LintId::Ifd), Level::Off);
        // a Nix alias is a name
        let mut c = Config::default();
        c.apply_toml(
            "[lint]\n\"family:path\" = \"error\"\nlint-short-path-literals = \"warn\"\n",
            Path::new("t"),
        )
        .unwrap();
        assert_eq!(c.level(LintId::ShortPathLiteral), Level::Warn);
        assert_eq!(c.level(LintId::AbsolutePathLiteral), Level::Error);
        // an input table follows the same rule
        let mut c = Config::default();
        c.apply_toml(
            "[lint.inputs.dep]\nfod = \"warn\"\nall = \"error\"\n",
            Path::new("t"),
        )
        .unwrap();
        let i = c.input_index("dep").unwrap();
        assert_eq!(c.site_level(LintId::Fod, Where::Input(i)), Level::Warn);
        assert_eq!(c.site_level(LintId::Ifd, Where::Input(i)), Level::Error);
        // between sources the later one wins, whatever its specificity
        let mut c = Config::default();
        c.apply_toml("[lint]\nabsolute-path-literal = \"warn\"\n", Path::new("t"))
            .unwrap();
        c.apply_env("family:path=error").unwrap();
        assert_eq!(c.level(LintId::AbsolutePathLiteral), Level::Error);
        c.set("all", Level::Off).unwrap();
        assert_eq!(c.level(LintId::AbsolutePathLiteral), Level::Off);
        // the flags are one source: a name given first still beats a
        // family or `all` given later; equals keep the later one
        let flags = |s: &[&str]| {
            let mut l = Layer::default();
            for p in s {
                l.set_pair(p).unwrap();
            }
            let mut c = Config::default();
            l.apply(&mut c.levels);
            c
        };
        assert_eq!(
            flags(&["absolute-path-literal=off", "all=error"]).level(LintId::AbsolutePathLiteral),
            Level::Off
        );
        assert_eq!(
            flags(&["all=error", "absolute-path-literal=off"]).level(LintId::AbsolutePathLiteral),
            Level::Off
        );
        assert_eq!(
            flags(&["all=error", "absolute-path-literal=off"]).level(LintId::Ifd),
            Level::Error
        );
        assert_eq!(
            flags(&["absolute-path-literal=off", "family:path=error"])
                .level(LintId::AbsolutePathLiteral),
            Level::Off
        );
        assert_eq!(
            flags(&["absolute-path-literal=off", "absolute-path-literal=warn"])
                .level(LintId::AbsolutePathLiteral),
            Level::Warn
        );
        assert_eq!(
            flags(&["all=off", "all=warn"]).level(LintId::AbsolutePathLiteral),
            Level::Warn
        );
        let mut c = Config::default();
        c.apply_env("family:path=error, absolute-path-literal=off")
            .unwrap();
        assert_eq!(c.level(LintId::AbsolutePathLiteral), Level::Off);
        assert_eq!(c.level(LintId::ShortPathLiteral), Level::Error);
    }

    #[test]
    fn toml_table() {
        let mut c = Config::default();
        c.apply_toml(
            "[other]\nx = 1\n[lint]\nabsolute-path-literal = \"warn\"\nlarge-source-copy = { level = \"error\", max-bytes = 64 }\n",
            Path::new("t"),
        )
        .unwrap();
        assert_eq!(c.level(LintId::AbsolutePathLiteral), Level::Warn);
        assert_eq!(c.level(LintId::LargeSourceCopy), Level::Error);
        assert_eq!(c.max_bytes, 64);
        assert!(
            c.apply_toml("[lint]\nabsolute-path-literal = 3\n", Path::new("t"))
                .is_err()
        );
        assert!(
            c.apply_toml(
                "[lint]\nabsolute-path-literal = { max-bytes = 3 }\n",
                Path::new("t")
            )
            .is_err()
        );
    }

    #[test]
    fn keys() {
        let mut c = Config::default();
        c.set("all", Level::Off).unwrap();
        assert_eq!(c.compile_key(), "");
        c.set("path-copy", Level::Warn).unwrap();
        assert_eq!(c.compile_key(), "");
        let off = c.clone();
        c.set("absolute-path-literal", Level::Warn).unwrap();
        assert!(c.compile_key().contains('w'));
        assert_ne!(c.key(), off.key());
        assert_eq!(off.key(), off.key());
        let mut all = c.clone();
        all.scope = Scope::All;
        assert_ne!(all.key(), c.key());
        let mut rooted = c.clone();
        rooted.add_root(b"/r");
        assert_ne!(rooted.key(), c.key());
    }

    #[test]
    fn scope_and_inputs() {
        let mut c = Config::default();
        c.apply_toml(
            "[lint]\nabsolute-path-literal = \"warn\"\nroots = [\"lib\", \"/abs/../x\"]\n[lint.inputs.nixpkgs]\nfod = \"warn\"\n\"family:path\" = \"off\"\n",
            Path::new("/repo/iets.toml"),
        )
        .unwrap();
        assert_eq!(c.scope, Scope::Root);
        assert_eq!(c.roots, vec![b"/repo/lib".to_vec(), b"/x".to_vec()]);
        assert!(c.in_root(b"/repo/lib/a.nix"));
        assert!(!c.in_root(b"/repo/library/a.nix"));
        let i = c.input_index("nixpkgs").unwrap();
        assert_eq!(
            c.site_level(LintId::AbsolutePathLiteral, Where::Root),
            Level::Warn
        );
        assert_eq!(
            c.site_level(LintId::AbsolutePathLiteral, Where::Elsewhere),
            Level::Off
        );
        assert_eq!(
            c.site_level(LintId::AbsolutePathLiteral, Where::Input(i)),
            Level::Off
        );
        assert_eq!(c.site_level(LintId::Fod, Where::Input(i)), Level::Warn);
        assert_eq!(c.for_where(Where::Input(i)).level(LintId::Fod), Level::Warn);
        assert_eq!(c.for_where(Where::Elsewhere).compile_key(), "");
        c.scope = Scope::All;
        assert_eq!(
            c.site_level(LintId::AbsolutePathLiteral, Where::Elsewhere),
            Level::Warn
        );
        assert_eq!(
            c.site_level(LintId::AbsolutePathLiteral, Where::Input(i)),
            Level::Off
        );
        assert!(
            c.apply_toml("[lint]\nscope = \"some\"\n", Path::new("t"))
                .is_err()
        );
        assert!(
            c.apply_toml("[lint]\nroots = \"lib\"\n", Path::new("t"))
                .is_err()
        );
        let mut d = Config::default();
        d.apply_toml("[lint]\nscope = \"all\"\n", Path::new("t"))
            .unwrap();
        assert_eq!(d.scope, Scope::All);
        assert!(!d.scoped());
    }

    #[test]
    fn an_unknown_name_is_a_warning_in_a_file_and_an_error_in_a_flag() {
        let s = Session::new();
        let _entered = s.enter();
        let mut c = Config::default();
        c.apply_toml(
            "[lint]\nnope = \"error\"\n\"family:nope\" = \"warn\"\ngone = { level = \"warn\" }\nfod = \"warn\"\n[lint.inputs.x]\nnope = \"warn\"\n",
            Path::new("/r/iets.toml"),
        )
        .unwrap();
        c.apply_env("nope=warn, ifd=error").unwrap();
        assert_eq!(c.level(LintId::Fod), Level::Warn);
        assert_eq!(c.level(LintId::Ifd), Level::Error);
        let mut noted = ignored::noted("unknown-lint");
        noted.sort();
        assert_eq!(
            noted,
            [
                "/r/iets.toml family:nope",
                "/r/iets.toml gone",
                "/r/iets.toml nope",
                "IETS_LINT nope"
            ]
        );
        assert!(c.apply_env("nope=loud").is_err());
        assert!(c.set_pair("nope=warn").is_err());
    }
}
