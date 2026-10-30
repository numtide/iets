//! Nix's settings files, read the way Nix reads them.
//!
//! Discovery follows libstore/globals.cc:loadConfFile; the line syntax follows
//! libutil/configuration.cc:parseConfigFiles. Every value stays a raw string:
//! the typed getters below apply the same parse rules Nix's `BaseSetting`
//! specialisations do. Settings this crate does not know are kept, not dropped
//! (Nix warns about them and keeps them in `unknownSettings`).

use std::collections::{BTreeMap, BTreeSet};
use std::env::split_paths;
use std::path::{Path, PathBuf};
use std::{fs, thread};

use crate::env::Env;
use crate::error::{Error, Result};
use crate::session::Session;
use crate::{ignored, log};

/// Settings whose values are whitespace-separated lists, so `extra-<name>`
/// appends instead of replacing (configuration.cc:Config::set, `isAppendable`).
const APPENDABLE: &[&str] = &[
    "access-tokens",
    "allowed-uris",
    "allowed-users",
    "build-hook",
    "experimental-features",
    "extra-platforms",
    "hashed-mirrors",
    "ignored-acls",
    "impure-env",
    "nix-path",
    "sandbox-paths",
    "secret-key-files",
    "substituters",
    "system-features",
    "trusted-public-keys",
    "trusted-substituters",
    "trusted-users",
];

/// Every setting of Nix 2.34.8 (`nix config show`); `ALIASES` has the old
/// names. A name in neither list is `warning: unknown setting 'NAME'`, the
/// text Nix gives (configuration.cc:warnUnknownSettings).
const KNOWN: &str = "\
    abort-on-warn accept-flake-config access-tokens allow-dirty \
    allow-dirty-locks allow-import-from-derivation allow-new-privileges \
    allow-symlinked-store allow-unsafe-native-code-during-evaluation \
    allowed-impure-host-deps allowed-uris allowed-users \
    always-allow-substitutes auto-allocate-uids auto-optimise-store \
    bash-prompt bash-prompt-prefix bash-prompt-suffix build-dir build-hook \
    build-poll-interval build-users-group builders builders-use-substitutes \
    commit-lock-file-summary compress-build-log connect-timeout cores \
    debugger-on-trace debugger-on-warn diff-hook download-attempts \
    download-buffer-size download-speed \
    eval-attrset-update-layer-rhs-threshold eval-cache eval-profile-file \
    eval-profiler eval-profiler-frequency eval-system experimental-features \
    external-builders extra-platforms fallback filter-syscalls \
    flake-registry fsync-metadata fsync-store-paths gc-reserved-space \
    hashed-mirrors http-connections http2 id-count ignore-try ignored-acls \
    impersonate-linux-26 impure-env json-log-path keep-build-log \
    keep-derivations keep-env-derivations keep-failed keep-going \
    keep-outputs lint-absolute-path-literals lint-short-path-literals \
    lint-url-literals log-lines max-build-log-size max-call-depth max-free \
    max-jobs max-silent-time max-substitution-jobs min-free \
    min-free-check-interval nar-buffer-size narinfo-cache-meta-ttl \
    narinfo-cache-negative-ttl narinfo-cache-positive-ttl netrc-file \
    nix-path nix-shell-always-looks-for-shell-nix \
    nix-shell-shebang-arguments-relative-to-script plugin-files \
    post-build-hook pre-build-hook preallocate-contents print-missing \
    pure-eval require-drop-supplementary-groups require-sigs restrict-eval \
    run-diff-hook sandbox sandbox-build-dir sandbox-dev-shm-size \
    sandbox-fallback sandbox-paths secret-key-files show-trace \
    ssl-cert-file stalled-download-timeout start-id store substitute \
    substituters sync-before-registering system system-features tarball-ttl \
    timeout trace-function-calls trace-import-from-derivation trace-verbose \
    trust-tarballs-from-git-forges trusted-public-keys trusted-substituters \
    trusted-users upgrade-nix-store-path-url use-case-hack use-cgroups \
    use-registries use-sqlite-wal use-xdg-base-directories \
    user-agent-suffix warn-dirty warn-large-path-threshold \
    warn-short-path-literals";

/// The Boolean settings of Nix 2.34.8 (`nix config show`). Nix parses a value
/// the moment it is set, so a spelling outside `parse_bool` stops the run.
const BOOLS: &str = "\
    abort-on-warn accept-flake-config allow-dirty allow-dirty-locks \
    allow-import-from-derivation allow-new-privileges allow-symlinked-store \
    allow-unsafe-native-code-during-evaluation always-allow-substitutes \
    auto-allocate-uids auto-optimise-store builders-use-substitutes \
    compress-build-log debugger-on-trace debugger-on-warn eval-cache fallback \
    filter-syscalls fsync-metadata fsync-store-paths http2 ignore-try \
    impersonate-linux-26 keep-build-log keep-derivations keep-env-derivations \
    keep-failed keep-going keep-outputs nix-shell-always-looks-for-shell-nix \
    nix-shell-shebang-arguments-relative-to-script preallocate-contents \
    print-missing pure-eval require-drop-supplementary-groups require-sigs \
    restrict-eval run-diff-hook sandbox-fallback show-trace substitute \
    sync-before-registering trace-function-calls \
    trace-import-from-derivation trace-verbose trust-tarballs-from-git-forges \
    use-case-hack use-cgroups use-registries use-sqlite-wal \
    use-xdg-base-directories warn-dirty warn-short-path-literals";

/// The integer settings. `sandbox` and `max-jobs` take a word beside their
/// type and are checked on their own.
const INTS: &str = "\
    build-poll-interval connect-timeout cores download-attempts \
    download-buffer-size download-speed \
    eval-attrset-update-layer-rhs-threshold eval-profiler-frequency \
    gc-reserved-space http-connections id-count log-lines max-build-log-size \
    max-call-depth max-free max-silent-time max-substitution-jobs min-free \
    min-free-check-interval nar-buffer-size narinfo-cache-meta-ttl \
    narinfo-cache-negative-ttl narinfo-cache-positive-ttl \
    stalled-download-timeout start-id tarball-ttl timeout \
    warn-large-path-threshold";

/// The integer settings Nix keeps in a 32-bit field, so a larger value is an
/// error there. `max-jobs` is one of them.
const NARROW: &str = "\
    build-poll-interval cores download-attempts download-speed \
    eval-attrset-update-layer-rhs-threshold eval-profiler-frequency id-count \
    max-call-depth max-jobs max-substitution-jobs narinfo-cache-meta-ttl \
    narinfo-cache-negative-ttl narinfo-cache-positive-ttl start-id tarball-ttl";

const ALIASES: &[(&str, &str)] = &[
    ("binary-cache-public-keys", "trusted-public-keys"),
    ("binary-caches", "substituters"),
    ("binary-caches-parallel-connections", "http-connections"),
    ("build-chroot-dirs", "sandbox-paths"),
    ("build-compress-log", "compress-build-log"),
    ("build-cores", "cores"),
    ("build-fallback", "fallback"),
    ("build-impersonate-linux-26", "impersonate-linux-26"),
    ("build-keep-log", "keep-build-log"),
    ("build-max-jobs", "max-jobs"),
    ("build-max-log-size", "max-build-log-size"),
    ("build-max-silent-time", "max-silent-time"),
    ("build-sandbox-paths", "sandbox-paths"),
    ("build-timeout", "timeout"),
    ("build-use-chroot", "sandbox"),
    ("build-use-sandbox", "sandbox"),
    ("build-use-substitutes", "substitute"),
    ("commit-lockfile-summary", "commit-lock-file-summary"),
    ("env-keep-derivations", "keep-env-derivations"),
    ("gc-keep-derivations", "keep-derivations"),
    ("gc-keep-outputs", "keep-outputs"),
    ("substitution-max-jobs", "max-substitution-jobs"),
    ("trusted-binary-caches", "trusted-substituters"),
];

const NO_DEBUGGER: &str = "iets has no debugger";
const NO_RESTRICTED_MODE: &str =
    "iets has no restricted mode, so every path and URI stays readable";
const NO_LINT_SETTINGS: &str = "iets does not read it; 'iets eval' has the '--lint' flags";

/// The settings that act in the Nix client and that iets has nothing for:
/// the name, Nix's default, and why. With any other value, whatever set it,
/// classic mode falls back to the real nix (`Config::left_to_nix`), and the
/// commands that cannot give one `ignored-setting` warning. The other client
/// settings are applied or hold by construction (`CLIENT`).
const UNIMPLEMENTED: &[(&str, &str, &str)] = &[
    (
        "abort-on-warn",
        "false",
        "builtins.warn never stops an evaluation",
    ),
    ("accept-flake-config", "false", ignored::NO_NIX_CONFIG),
    (
        "allow-dirty",
        "true",
        "iets evaluates a dirty Git tree as it is",
    ),
    (
        "allow-unsafe-native-code-during-evaluation",
        "false",
        "iets has no builtins.exec and no builtins.importNative",
    ),
    ("allowed-uris", "", NO_RESTRICTED_MODE),
    ("debugger-on-trace", "false", NO_DEBUGGER),
    ("debugger-on-warn", "false", NO_DEBUGGER),
    (
        "eval-profiler",
        "disabled",
        "iets profiles an evaluation with IETS_PROFILE, not with this setting (docs/reference/profile.md)",
    ),
    (
        "eval-system",
        "",
        "iets does not read it; builtins.currentSystem is 'system', or the system iets was built for",
    ),
    ("ignore-try", "false", NO_DEBUGGER),
    (
        "impure-env",
        "",
        "builtins.getEnv reads the process environment alone",
    ),
    ("lint-absolute-path-literals", "ignore", NO_LINT_SETTINGS),
    ("lint-short-path-literals", "ignore", NO_LINT_SETTINGS),
    ("lint-url-literals", "ignore", NO_LINT_SETTINGS),
    (
        "max-call-depth",
        "10000",
        "the call depth limit is fixed at 10000",
    ),
    (
        "log-lines",
        "25",
        "iets prints the last 200 lines of a failed build",
    ),
    ("plugin-files", "", "iets loads no plugins"),
    ("restrict-eval", "false", NO_RESTRICTED_MODE),
    ("tarball-ttl", "3600", "iets's fetcher has no such switch"),
    (
        "trace-function-calls",
        "false",
        "iets has no function-call trace",
    ),
    (
        "trace-verbose",
        "false",
        "builtins.traceVerbose never prints",
    ),
    (
        "trust-tarballs-from-git-forges",
        "true",
        "iets's fetcher has no such switch",
    ),
    ("warn-short-path-literals", "false", NO_LINT_SETTINGS),
];

/// The `UNIMPLEMENTED` settings that are no reason to fall back: a flake
/// seldom has a `nixConfig`, and `log-lines` changes no result.
const WARN_ONLY: &[&str] = &["accept-flake-config", "log-lines"];

/// The other settings that act in the Nix client. iets applies them, or
/// they hold by construction; docs/reference/settings.md says which and why.
/// A known setting outside this list and `UNIMPLEMENTED` acts in the store,
/// and goes to the daemon (`Config::forwarded`).
const CLIENT: &[&str] = &[
    "access-tokens",
    "allow-dirty-locks",
    "allow-import-from-derivation",
    "bash-prompt",
    "bash-prompt-prefix",
    "bash-prompt-suffix",
    "commit-lock-file-summary",
    "eval-attrset-update-layer-rhs-threshold",
    "eval-cache",
    "eval-profile-file",
    "eval-profiler-frequency",
    "experimental-features",
    "flake-registry",
    "json-log-path",
    "nix-path",
    "nix-shell-always-looks-for-shell-nix",
    "nix-shell-shebang-arguments-relative-to-script",
    "preallocate-contents",
    "pure-eval",
    "show-trace",
    "trace-import-from-derivation",
    "use-case-hack",
    "use-registries",
    "warn-dirty",
];
fn env(name: &str) -> Option<String> {
    Session::current().env.non_empty(name)
}

/// `$XDG_CONFIG_HOME` or `~/.config` (unix/xdg-dirs.cc:getConfigHome).
pub fn config_home() -> PathBuf {
    config_home_in(&Session::current().env)
}

pub fn config_home_in(e: &Env) -> PathBuf {
    let home = e
        .non_empty("HOME")
        .map_or_else(|| PathBuf::from("/"), PathBuf::from);
    e.non_empty("XDG_CONFIG_HOME")
        .map_or_else(|| home.join(".config"), PathBuf::from)
}

/// `$XDG_CONFIG_DIRS` or `/etc/xdg` (unix/xdg-dirs.cc:getConfigDirs).
fn config_dirs() -> Vec<PathBuf> {
    env("XDG_CONFIG_DIRS")
        .unwrap_or_else(|| "/etc/xdg".into())
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// `$NIX_CONF_DIR` or the compiled-in `/etc/nix` (globals.cc:nixConfDir).
pub fn conf_dir() -> PathBuf {
    env("NIX_CONF_DIR").map_or_else(|| PathBuf::from("/etc/nix"), PathBuf::from)
}

/// globals.cc:nixUserConfFiles. `$NIX_USER_CONF_FILES` replaces the XDG list
/// wholesale. The list is in precedence order: the *first* entry wins, because
/// loadConfFile applies it last (globals.cc:143 iterates `rbegin()..rend()`).
/// A set but empty variable is an empty list, so no user file is read at all:
/// globals.cc:167 tests `has_value()`, not the text.
pub fn user_conf_files() -> Vec<PathBuf> {
    if let Some(files) = Session::current().env.var_os("NIX_USER_CONF_FILES") {
        return split_paths(&files)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
    }
    let mut files = vec![
        env("NIX_CONFIG_HOME")
            .map_or_else(|| config_home().join("nix"), PathBuf::from)
            .join("nix.conf"),
    ];
    files.extend(config_dirs().into_iter().map(|d| d.join("nix/nix.conf")));
    files
}

/// Whether an experimental feature is on for this run: `experimental-features`
/// of nix.conf, plus what the command line added. Only classic mode asks,
/// for the builtins Nix gates on one; iets's own commands have every feature
/// (docs/reference/divergences.md). Before the settings are read nothing is on,
/// which is Nix's own default.
pub fn xp_on(feature: &str) -> bool {
    Session::current()
        .nix_conf
        .get()
        .is_some_and(|c| (c.list("experimental-features").iter()).any(|f| f == feature))
}

/// Whether an evaluation may read a derivation output that is already in the
/// store. Nix refuses that read when `allow-import-from-derivation` is off and
/// warns with `trace-import-from-derivation`, so both leave it to the real nix.
pub fn reads_built_outputs(s: &Session) -> bool {
    s.nix_conf.get().is_none_or(|c| {
        c.bool("allow-import-from-derivation").ok().flatten() != Some(false)
            && c.bool("trace-import-from-derivation").ok().flatten() != Some(true)
    })
}

/// `configuration.cc:BaseSetting<bool>::parse`.
pub fn parse_bool(name: &str, s: &str) -> Result<bool> {
    match s {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(Error::Config(format!(
            "Boolean setting '{name}' has invalid value '{s}'"
        ))),
    }
}

/// util.hh:string2IntWithUnitPrefix. A trailing K, M, G or T, in either case,
/// multiplies by 2^10, 2^20, 2^30 or 2^40. `narrow` is a 32-bit field.
fn parse_int(s: &str, narrow: bool) -> Option<u64> {
    let (digits, mul) = match s.chars().next_back()?.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        'M' => (&s[..s.len() - 1], 1 << 20),
        'G' => (&s[..s.len() - 1], 1 << 30),
        'T' => (&s[..s.len() - 1], 1 << 40),
        c if c.is_ascii_alphabetic() => return None,
        _ => (s, 1),
    };
    let n = digits.trim().parse::<u64>().ok()?.checked_mul(mul)?;
    (!narrow || n <= u64::from(u32::MAX)).then_some(n)
}

fn in_list(list: &str, name: &str) -> bool {
    list.split_ascii_whitespace().any(|k| k == name)
}

/// The value check Nix does when a setting is set. `name` is canonical.
pub fn check_value(name: &str, v: &str) -> Result<()> {
    let bad = |text: String| Err(Error::Config(text));
    if in_list(BOOLS, name) {
        return parse_bool(name, v).map(|_| ());
    }
    match name {
        // globals.cc:319: the Booleans plus `relaxed`
        "sandbox" if v != "relaxed" && parse_bool("", v).is_err() => {
            bad(format!("option 'sandbox' has invalid value '{v}'"))
        }
        "max-jobs" if v != "auto" && parse_int(v, true).is_none() => {
            bad("configuration setting 'max-jobs' should be 'auto' or an integer".into())
        }
        n if in_list(INTS, n) && parse_int(v, in_list(NARROW, n)).is_none() => {
            bad(format!("setting '{name}' has invalid value '{v}'"))
        }
        _ => Ok(()),
    }
}

/// The Nix settings that apply to this process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    settings: BTreeMap<String, String>,
    /// Set by anything but `$NIX_CONF_DIR/nix.conf`: what a Nix client sends
    /// to the daemon (globals.cc:loadConfFile, `resetOverridden`).
    overridden: BTreeSet<String>,
    /// List settings that only ever got `extra-` values.
    extra_only: BTreeSet<String>,
}

fn is_known(name: &str) -> bool {
    KNOWN.split_ascii_whitespace().any(|k| k == name)
}

/// `name` as the setting it sets: no `extra-`, no alias.
fn canonical(name: &str) -> &str {
    let alias = |n| ALIASES.iter().find(|(a, _)| *a == n).map(|(_, to)| *to);
    let bare = name.strip_prefix("extra-").map(|b| alias(b).unwrap_or(b));
    match bare {
        Some(b) if APPENDABLE.contains(&b) => b,
        _ => alias(name).unwrap_or(name),
    }
}

/// A setting of Nix that acts in the store, not in the client.
fn acts_in_store(name: &str) -> bool {
    is_known(name) && !CLIENT.contains(&name) && unimplemented(name).is_none()
}

/// One `ignored-setting` warning for each `--option` that acts in the store
/// and is not in `applied`, for a command line whose store takes none.
pub fn note_unsent(overrides: &[(String, String)], applied: &[&str], why: &str) {
    for (name, _) in overrides {
        let name = canonical(name);
        if acts_in_store(name) && !applied.contains(&name) {
            let raw = format!("setting '{name}' is ignored: {why}");
            ignored::note("ignored-setting", name, raw);
        }
    }
}

/// `note_unsent` for a `grpc://` builder, which has no `SetOptions`.
pub fn note_grpc(overrides: &[(String, String)]) {
    let why = "a grpc:// builder takes no settings; its URL has 'max-builds'";
    note_unsent(overrides, &["store", "system"], why);
    ignored::flush();
}

fn unimplemented(name: &str) -> Option<(&'static str, &'static str)> {
    let mut all = UNIMPLEMENTED.iter();
    all.find(|(n, _, _)| *n == name)
        .map(|(_, d, why)| (*d, *why))
}

/// `a` and `b` are one value: the same Boolean, else the same words.
fn same_value(a: &str, b: &str) -> bool {
    match (parse_bool("", a), parse_bool("", b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a.split_whitespace().eq(b.split_whitespace()),
    }
}

impl Config {
    pub fn empty() -> Config {
        Config::default()
    }

    /// The whole discovery: `$NIX_CONF_DIR/nix.conf`, then the user files from
    /// lowest to highest precedence, then `$NIX_CONFIG`, then `--option`
    /// overrides last (globals.cc:loadConfFile, common-args.cc:42). It warns
    /// about each setting that has no effect (`crate::ignored`), and the
    /// session keeps the result, so the fetcher sees the `--option`s too.
    pub fn load(overrides: &[(String, String)]) -> Result<Config> {
        let c = Config::read(overrides)?;
        c.announce();
        Ok(c)
    }

    /// What `load` does once the settings are read.
    pub fn announce(&self) {
        self.note_ignored();
        if self.bool("show-trace").ok().flatten() == Some(true) {
            log::set_show_trace();
        }
        let _ = Session::current().nix_conf.set(self.clone());
        ignored::flush();
    }

    /// The first `UNIMPLEMENTED` setting with a value of its own that is a
    /// reason for classic mode to fall back: the real nix applies it.
    pub fn left_to_nix(&self) -> Option<&'static str> {
        let set = |(name, default, _): &&(&'static str, &str, &str)| {
            let differs = |v: &str| !same_value(v, default);
            !WARN_ONLY.contains(name) && self.str(name).is_some_and(differs)
        };
        UNIMPLEMENTED.iter().find(set).map(|(name, _, _)| *name)
    }

    /// One note for each setting that has no effect: Nix's own text for a
    /// name it does not know either, the reason for the rest.
    fn note_ignored(&self) {
        for (name, value) in &self.settings {
            if !is_known(name) {
                let raw = format!("unknown setting '{name}'");
                ignored::note("unknown-setting", name, raw);
            } else if let Some((default, why)) = unimplemented(name)
                && !same_value(value, default)
            {
                let raw = format!("setting '{name}' is ignored: {why}");
                if name == "accept-flake-config" {
                    ignored::note_for_nix_config("ignored-setting", name, raw);
                } else {
                    ignored::note("ignored-setting", name, raw);
                }
            }
        }
    }

    /// `load` for iets's own commands, which send a daemon no settings.
    pub fn load_native(overrides: &[(String, String)]) -> Result<Config> {
        let why = "iets's own commands send a daemon no settings; classic mode does";
        note_unsent(overrides, &[], why);
        Config::load(overrides)
    }

    /// `load` without the warnings, for a reader inside iets: the log sink,
    /// the fetcher.
    pub fn read(overrides: &[(String, String)]) -> Result<Config> {
        let mut c = Config::empty();
        c.apply_file_if_present(&conf_dir().join("nix.conf"))?;
        c.overridden.clear();
        for f in user_conf_files().iter().rev() {
            c.apply_file_if_present(f)?;
        }
        if let Some(text) = env("NIX_CONFIG") {
            c.apply(&text, Path::new("NIX_CONFIG"))?;
        }
        for (name, value) in overrides {
            check_value(canonical(name), value)?;
            c.set(name, value);
        }
        Ok(c)
    }

    /// An unreadable file is skipped, as loadConfFile's `catch (SystemError &)` does.
    fn apply_file_if_present(&mut self, path: &Path) -> Result<()> {
        match fs::read_to_string(path) {
            Ok(text) => self.apply(&text, path),
            Err(_) => Ok(()),
        }
    }

    pub fn apply(&mut self, text: &str, origin: &Path) -> Result<()> {
        for e in parse(text, origin)? {
            // NIX_PATH, even an empty one, must override the setting
            // (configuration.cc:192)
            let set = Session::current().env.var_os("NIX_PATH").is_some();
            if (e.name == "nix-path" || e.name == "extra-nix-path") && set {
                continue;
            }
            check_value(canonical(&e.name), &e.value)?;
            self.set(&e.name, &e.value);
        }
        Ok(())
    }

    /// configuration.cc:Config::set. An alias is its setting. `extra-<name>`
    /// appends when `<name>` is a list setting; otherwise the pair is kept
    /// under its literal name.
    pub fn set(&mut self, name: &str, value: &str) {
        let to = canonical(name);
        self.overridden.insert(to.to_string());
        if name.starts_with("extra-") && APPENDABLE.contains(&to) {
            if !self.settings.contains_key(to) {
                self.extra_only.insert(to.to_string());
            }
            let slot = self.settings.entry(to.to_string()).or_default();
            if !slot.is_empty() && !value.is_empty() {
                slot.push(' ');
            }
            slot.push_str(value);
            return;
        }
        self.extra_only.remove(to);
        self.settings.insert(to.to_string(), value.to_string());
    }

    /// The override map of `SetOptions` (remote-store.cc:setOptions): every
    /// store setting the user set outside the system nix.conf, but the ones
    /// `worker::Options` sends as fields. A list that only got `extra-`
    /// values goes as `extra-<name>`, which the daemon appends to its own.
    pub fn forwarded(&self) -> Vec<(String, String)> {
        // `store` names the daemon this goes to; it is not a setting for it
        const FIELDS: &[&str] = &[
            "store",
            "keep-failed",
            "keep-going",
            "fallback",
            "max-jobs",
            "max-silent-time",
            "cores",
            "substitute",
        ];
        let sent = |n: &&String| acts_in_store(n) && !FIELDS.contains(&n.as_str());
        let pair = |n: &String| {
            let name = match self.extra_only.contains(n) {
                true => format!("extra-{n}"),
                false => n.clone(),
            };
            (name, self.settings[n].clone())
        };
        self.overridden.iter().filter(sent).map(pair).collect()
    }

    /// `name` was set outside `$NIX_CONF_DIR/nix.conf`: a user file,
    /// `$NIX_CONFIG` or `--option`. That is Nix's `overridden` bit, which
    /// `loadConfFile` clears after the system file.
    pub fn overridden(&self, name: &str) -> bool {
        self.overridden.contains(name)
    }

    pub fn str(&self, name: &str) -> Option<&str> {
        self.settings.get(name).map(String::as_str)
    }

    pub fn int(&self, name: &str) -> Result<Option<u64>> {
        match self.str(name) {
            None => Ok(None),
            Some(s) => parse_int(s, in_list(NARROW, name))
                .map(Some)
                .ok_or_else(|| Error::Config(format!("setting '{name}' has invalid value '{s}'"))),
        }
    }

    pub fn bool(&self, name: &str) -> Result<Option<bool>> {
        self.str(name).map(|s| parse_bool(name, s)).transpose()
    }

    /// A whitespace-separated list setting. Unset is the empty list.
    pub fn list(&self, name: &str) -> Vec<String> {
        self.str(name)
            .into_iter()
            .flat_map(str::split_whitespace)
            .map(str::to_string)
            .collect()
    }

    /// The `nix-path` entries, and whether they replace `NIX_PATH`: a file
    /// never sets them beside it, so `--option nix-path` did, and that wins
    /// in Nix; `--option extra-nix-path` adds to it.
    pub fn nix_path(&self) -> (bool, Vec<String>) {
        let replaces =
            self.settings.contains_key("nix-path") && !self.extra_only.contains("nix-path");
        (replaces, self.list("nix-path"))
    }

    /// `store`, default `auto` (globals.cc:storeUri).
    pub fn store(&self) -> String {
        self.str("store").unwrap_or("auto").to_string()
    }

    /// `system`, default the build target.
    pub fn system(&self) -> String {
        self.str("system").unwrap_or(DEFAULT_SYSTEM).to_string()
    }

    /// `max-jobs`, with `auto` resolving to the CPU count. The default is the
    /// CPU count too: that is what this client has always sent.
    pub fn max_jobs(&self) -> Result<u64> {
        match self.str("max-jobs") {
            None | Some("auto") => Ok(cpus()),
            Some(_) => Ok(self.int("max-jobs")?.unwrap_or_else(cpus)),
        }
    }

    /// `cores`, where 0 means "all of them" and is the default this client sends.
    pub fn cores(&self) -> Result<u64> {
        Ok(self.int("cores")?.unwrap_or(0))
    }

    pub fn keep_going(&self) -> Result<bool> {
        Ok(self.bool("keep-going")?.unwrap_or(false))
    }

    pub fn keep_failed(&self) -> Result<bool> {
        Ok(self.bool("keep-failed")?.unwrap_or(false))
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub const DEFAULT_SYSTEM: &str = "x86_64-linux";
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
pub const DEFAULT_SYSTEM: &str = "aarch64-linux";
#[cfg(all(target_arch = "riscv64", target_os = "linux"))]
pub const DEFAULT_SYSTEM: &str = "riscv64-linux";
#[cfg(all(target_arch = "x86_64", target_os = "macos"))]
pub const DEFAULT_SYSTEM: &str = "x86_64-darwin";
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub const DEFAULT_SYSTEM: &str = "aarch64-darwin";
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
pub const DEFAULT_SYSTEM: &str = "x86_64-windows";
#[cfg(all(target_arch = "aarch64", target_os = "windows"))]
pub const DEFAULT_SYSTEM: &str = "aarch64-windows";

fn cpus() -> u64 {
    thread::available_parallelism().map_or(1, |n| n.get() as u64)
}

/// One `name = value` line, with the file and line it came from so a reader
/// that rejects unknown names can say where the name is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub value: String,
    pub file: PathBuf,
    pub line: usize,
}

/// configuration.cc:parseConfigFiles. Returns the entries in file order,
/// includes expanded in place.
pub fn parse(text: &str, origin: &Path) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    parse_into(text, origin, &mut out, 0)?;
    Ok(out)
}

const MAX_INCLUDE_DEPTH: usize = 32;

fn parse_into(text: &str, origin: &Path, out: &mut Vec<Entry>, depth: usize) -> Result<()> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(Error::Config(format!(
            "too many nested includes in {}",
            origin.display()
        )));
    }
    for (nr, raw) in text.split('\n').enumerate() {
        let line = match raw.find('#') {
            Some(i) => &raw[..i],
            None => raw,
        };
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        if tokens.len() < 2 {
            return Err(Error::Config(format!(
                "syntax error in configuration line '{line}' in {}",
                origin.display()
            )));
        }
        let ignore_missing = match tokens[0] {
            "include" => Some(false),
            "!include" => Some(true),
            _ => None,
        };
        if let Some(ignore_missing) = ignore_missing {
            if tokens.len() != 2 {
                return Err(Error::Config(format!(
                    "syntax error in configuration line '{line}' in {}",
                    origin.display()
                )));
            }
            let path = abs_path(tokens[1], origin.parent().unwrap_or(Path::new("")));
            if path.exists() {
                // An existing but unreadable include is ignored, as Nix's
                // `catch (SystemError &)` around the read does.
                if let Ok(included) = fs::read_to_string(&path) {
                    parse_into(&included, &path, out, depth + 1)?;
                }
            } else if !ignore_missing {
                return Err(Error::Config(format!(
                    "file '{}' included from '{}' not found",
                    path.display(),
                    origin.display()
                )));
            }
            continue;
        }
        if tokens[1] != "=" {
            return Err(Error::Config(format!(
                "syntax error in configuration line '{line}' in {}",
                origin.display()
            )));
        }
        out.push(Entry {
            name: tokens[0].to_string(),
            value: tokens[2..].join(" "),
            file: origin.to_path_buf(),
            line: nr + 1,
        });
    }
    Ok(())
}

fn abs_path(path: &str, base: &Path) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/conf")
    }

    fn from_str(text: &str, origin: impl AsRef<Path>) -> Result<Config> {
        let mut c = Config::empty();
        c.apply(text, origin.as_ref())?;
        Ok(c)
    }

    fn load(name: &str) -> Config {
        let path = fixtures().join(name);
        let text = fs::read_to_string(&path).unwrap();
        from_str(&text, &path).unwrap()
    }

    #[test]
    fn basic_syntax() {
        let c = load("basic.conf");
        assert_eq!(c.str("cores"), Some("4"));
        assert_eq!(c.cores().unwrap(), 4);
        assert!(c.keep_going().unwrap());
        assert!(!c.keep_failed().unwrap());
        assert_eq!(c.str("with-comment"), Some("value"));
        assert_eq!(c.str("spaced"), Some("a b c"));
        assert_eq!(c.str("empty-value"), Some(""));
    }

    #[test]
    fn unknown_settings_are_kept() {
        let c = load("basic.conf");
        assert_eq!(c.str("not-a-real-setting"), Some("kept"));
    }

    #[test]
    fn the_setting_tables_name_nix_settings() {
        let aliased = ALIASES.iter().map(|(_, to)| *to);
        let ignored = UNIMPLEMENTED.iter().map(|(n, _, _)| *n);
        let all = aliased.chain(ignored).chain(CLIENT.iter().copied());
        for name in all.chain(APPENDABLE.iter().copied()) {
            assert!(is_known(name), "{name}");
        }
        assert!(ALIASES.iter().all(|(a, _)| !is_known(a)));
        assert!(CLIENT.iter().all(|n| unimplemented(n).is_none()));
        let typed = BOOLS
            .split_ascii_whitespace()
            .chain(INTS.split_ascii_whitespace());
        for name in typed.chain(NARROW.split_ascii_whitespace()) {
            assert!(is_known(name), "{name}");
            assert!(!APPENDABLE.contains(&name), "{name}");
        }
        for name in BOOLS.split_ascii_whitespace() {
            assert!(!in_list(INTS, name), "{name}");
        }
        for name in NARROW.split_ascii_whitespace() {
            assert!(in_list(INTS, name) || name == "max-jobs", "{name}");
        }
    }

    #[test]
    fn an_alias_is_its_setting() {
        let c = from_str("build-cores = 3\nextra-binary-caches = https://a\n", "t").unwrap();
        assert_eq!(c.cores().unwrap(), 3);
        assert_eq!(c.list("substituters"), ["https://a"]);
        assert_eq!(canonical("extra-substituters"), "substituters");
        assert_eq!(canonical("extra-cores"), "extra-cores");
        assert_eq!(canonical("extra-platforms"), "extra-platforms");
    }

    #[test]
    fn the_daemon_gets_the_store_settings_set_outside_the_system_file() {
        let mut c = from_str("substituters = https://sys\nsandbox = true\n", "t").unwrap();
        c.overridden.clear();
        assert!(c.forwarded().is_empty());
        for (name, value) in [
            ("extra-substituters", "https://me"),
            ("extra-trusted-public-keys", "k:1"),
            ("extra-trusted-public-keys", "k:2"),
            ("build-timeout", "9"),
            ("max-jobs", "1"),
            ("store", "ssh-ng://b"),
            ("restrict-eval", "true"),
            ("pure-eval", "true"),
            ("lazy-trees", "true"),
            ("bogus", "1"),
        ] {
            c.set(name, value);
        }
        let pair = |n: &str, v: &str| (n.to_string(), v.to_string());
        assert_eq!(
            c.forwarded(),
            [
                pair("substituters", "https://sys https://me"),
                pair("timeout", "9"),
                pair("extra-trusted-public-keys", "k:1 k:2"),
            ]
        );
    }

    #[test]
    fn a_setting_with_no_effect_is_noted() {
        let text = "no-such-setting-a = 1\nextra-no-such-b = 1\ntrace-verbose = true\n\
                    max-call-depth = 10000\nallow-dirty = yes\nsandbox = false\neval-cache = false\n";
        from_str(text, "t").unwrap().note_ignored();
        let unknown = ignored::noted("unknown-setting");
        assert!(unknown.iter().any(|n| n == "no-such-setting-a"));
        assert!(unknown.iter().any(|n| n == "extra-no-such-b"));
        let ignored = ignored::noted("ignored-setting");
        assert!(ignored.iter().any(|n| n == "trace-verbose"));
        for quiet in ["max-call-depth", "allow-dirty", "sandbox", "eval-cache"] {
            assert!(!ignored.iter().any(|n| n == quiet), "{quiet}");
        }
        let o = |n: &str| (n.to_string(), String::new());
        note_unsent(
            &[o("build-use-sandbox"), o("keep-outputs"), o("system")],
            &["system"],
            "why",
        );
        note_unsent(&[o("nix-path"), o("no-such-setting-c")], &[], "why");
        let ignored = ignored::noted("ignored-setting");
        assert!(ignored.iter().any(|n| n == "sandbox"));
        assert!(ignored.iter().any(|n| n == "keep-outputs"));
        for quiet in ["system", "nix-path", "no-such-setting-c"] {
            assert!(!ignored.iter().any(|n| n == quiet), "{quiet}");
        }
    }

    #[test]
    fn a_classic_name_leaves_an_unimplemented_setting_to_nix() {
        let c = |text: &str| from_str(text, "t").unwrap().left_to_nix();
        assert_eq!(c("restrict-eval = true\n"), Some("restrict-eval"));
        assert_eq!(c("max-call-depth = 5\n"), Some("max-call-depth"));
        assert_eq!(c("restrict-eval = false\nmax-call-depth = 10000\n"), None);
        assert_eq!(c("accept-flake-config = true\nlog-lines = 5\n"), None);
        assert_eq!(c("sandbox = false\neval-cache = false\n"), None);
    }

    #[test]
    fn option_nix_path_replaces_nix_path_and_extra_adds() {
        let mut c = Config::empty();
        assert_eq!(c.nix_path(), (false, vec![]));
        c.set("extra-nix-path", "a=/a");
        assert_eq!(c.nix_path(), (false, vec!["a=/a".to_string()]));
        c.set("nix-path", "b=/b");
        assert_eq!(c.nix_path(), (true, vec!["b=/b".to_string()]));
    }

    #[test]
    fn include_chain() {
        let c = load("outer.conf");
        assert_eq!(c.str("from-outer"), Some("1"));
        assert_eq!(c.str("from-middle"), Some("2"));
        assert_eq!(c.str("from-inner"), Some("3"));
        // The inner file is read where the include sits, so a later line wins.
        assert_eq!(c.str("overridden"), Some("outer-wins"));
    }

    #[test]
    fn missing_include_errors_and_bang_include_does_not() {
        let dir = fixtures();
        let err = from_str("include nope.conf\n", dir.join("x.conf")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("included from"), "{msg}");
        assert!(msg.contains("not found"), "{msg}");
        let c = from_str("!include nope.conf\ncores = 1\n", dir.join("x.conf")).unwrap();
        assert_eq!(c.cores().unwrap(), 1);
    }

    #[test]
    fn syntax_errors() {
        let e = from_str("cores\n", "t").unwrap_err().to_string();
        assert!(e.contains("syntax error in configuration line"), "{e}");
        let e = from_str("cores 4\n", "t").unwrap_err().to_string();
        assert!(e.contains("syntax error in configuration line"), "{e}");
        let e = from_str("include a b\n", "t").unwrap_err().to_string();
        assert!(e.contains("syntax error in configuration line"), "{e}");
    }

    #[test]
    fn extra_appends_to_list_settings_only() {
        let c = load("extra.conf");
        assert_eq!(
            c.list("substituters"),
            vec![
                "https://cache.nixos.org/".to_string(),
                "https://one.example".to_string(),
                "https://two.example".to_string(),
            ]
        );
        assert_eq!(
            c.list("experimental-features"),
            vec!["nix-command".to_string(), "flakes".to_string()]
        );
        // `cores` is not a list, so `extra-cores` stays a setting of its own.
        assert_eq!(c.cores().unwrap(), 2);
        assert_eq!(c.str("extra-cores"), Some("8"));
    }

    #[test]
    fn typed_getters_and_defaults() {
        let c = Config::empty();
        assert_eq!(c.store(), "auto");
        assert_eq!(c.cores().unwrap(), 0);
        assert!(!c.keep_going().unwrap());
        assert!(!c.keep_failed().unwrap());
        assert_eq!(c.max_jobs().unwrap(), cpus());
        assert!(c.list("substituters").is_empty());
        assert!(
            ["-linux", "-darwin", "-windows"]
                .iter()
                .any(|os| c.system().ends_with(os))
        );

        let c = load("typed.conf");
        assert_eq!(c.store(), "daemon");
        assert_eq!(c.system(), "aarch64-linux");
        assert_eq!(c.max_jobs().unwrap(), 3);
        assert_eq!(c.cores().unwrap(), 7);
        assert_eq!(
            c.list("trusted-public-keys"),
            vec!["k1:AAAA=".to_string(), "k2:BBBB=".to_string()]
        );
    }

    #[test]
    fn max_jobs_auto_is_the_cpu_count() {
        let c = from_str("max-jobs = auto\n", "t").unwrap();
        assert_eq!(c.max_jobs().unwrap(), cpus());
    }

    /// Nix parses a value where it is set, so a bad one stops the run there
    /// (config-impl.hh:151, configuration.cc:304, globals.cc:319).
    #[test]
    fn bad_typed_values_error_where_they_are_set() {
        let err = |text: &str| from_str(text, "t").map(|_| ()).unwrap_err().to_string();
        assert_eq!(
            err("keep-going = maybe\n"),
            "Boolean setting 'keep-going' has invalid value 'maybe'"
        );
        assert_eq!(
            err("cores = many\n"),
            "setting 'cores' has invalid value 'many'"
        );
        assert_eq!(
            err("cores = 4294967296\n"),
            "setting 'cores' has invalid value '4294967296'"
        );
        assert_eq!(
            err("build-cores = -1\n"),
            "setting 'cores' has invalid value '-1'"
        );
        assert_eq!(
            err("sandbox = maybe\n"),
            "option 'sandbox' has invalid value 'maybe'"
        );
        assert_eq!(
            err("max-jobs = 1h\n"),
            "configuration setting 'max-jobs' should be 'auto' or an integer"
        );
        // the unit prefix of string2IntWithUnitPrefix, and the words that
        // sandbox and max-jobs take beside their type
        let c = from_str(
            "cores = 1K\nmin-free = 2g\nmax-jobs = auto\nsandbox = relaxed\n",
            "t",
        )
        .unwrap();
        assert_eq!(c.cores().unwrap(), 1024);
        assert_eq!(c.int("min-free").unwrap(), Some(2 << 30));
        assert_eq!(c.max_jobs().unwrap(), cpus());
    }

    #[rstest]
    #[case("true", true)]
    #[case("yes", true)]
    #[case("1", true)]
    #[case("false", false)]
    #[case("no", false)]
    #[case("0", false)]
    fn bool_spellings(#[case] text: &str, #[case] want: bool) {
        let c = from_str(&format!("keep-failed = {text}\n"), "t").unwrap();
        assert_eq!(c.keep_failed().unwrap(), want);
    }

    #[test]
    fn overrides_win_over_files() {
        let mut c = load("basic.conf");
        c.set("cores", "16");
        assert_eq!(c.cores().unwrap(), 16);
    }

    #[test]
    fn user_conf_files_are_in_precedence_order() {
        let files = user_conf_files();
        assert!(!files.is_empty());
        assert!(files[0].ends_with("nix/nix.conf"), "{files:?}");
    }
}
