//! The lint registry: one line per lint, with its family, hook and default
//! level. It sits under the parser because a `# iets: allow` comment names
//! lints by name (`allow.rs`); the levels and the reporting are in `lint`.

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
    IfdLocalOutput: "ifd-local-output", Purity, Evaluator, Warn, "a read (import, readFile, readDir, ...) of a derivation output that this machine's store already holds, so a machine without it builds it or fails";
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
