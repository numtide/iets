//! Tree-walking evaluator over the resolved IR.

pub mod coerce;
pub mod error;
pub mod search_path;

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::from_utf8;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use std::{fs, mem, ptr};

pub use error::{Error, ErrorKind, EvalResult, Pos};

use crate::builtins::derivation::DrvState;
use crate::builtins::{self, store_hooks};
use crate::compile::cache::{self, IrCache};
use crate::compile::ir::{
    AttrElemIr, AttrsIr, BindIr, BindScope, Ir, IrRef, LambdaIr, LetIr, ParamIr, SelectIr,
};
use crate::compile::{self, Compiler, Globals};
use crate::events::{self, Field, Group};
use crate::lint::allow::Allows;
use crate::lint::coerce::{or_non_attrs, read_through_copy};
use crate::lint::{self, Diag, Lints, purity};
use crate::parallel::shared::{ArenaUse, Counts, Files, SharedMap, SymWords, Totals};
use crate::profile::{self, Stack};
use crate::runtime::arena::{self, STAT_NAMES, Stat};
#[cfg(feature = "stats2")]
use crate::runtime::census;
use crate::runtime::{
    Arena, AttrsObj, Cell, Ctx, EnvObj, Kind, ListObj, MAX_LEAVES, StringObj, ThunkObj, UNION_MIN,
    Value, ValueType, Word, ctx_to_arena, leaf_count,
};
use crate::session::Session;
use crate::source::LazyStore;
use crate::store::locate;
use crate::store::path::{StorePath, store_dir, strip_store_dir};
use crate::store::source::{SourceOrigin, SrcPath};
use crate::syntax::ast::{Ast, FileId};
use crate::syntax::symbol::{FxHashMap, FxHashSet, Interner, Sym};
use crate::trace::{self, Trace, store};
use crate::{argv, config, parallel, syntax};

pub const MAX_CALL_DEPTH: u32 = 10000;

/// Virtual path prefix for the embedded corepkgs (`<nix/fetchurl.nix>`).
pub struct CorePkgs;
impl CorePkgs {
    pub const PREFIX: &[u8] = b"/__corepkgs__/";
}

/// Where a compiled text comes from, which decides its `Files` entry.
#[derive(Clone, Copy, PartialEq)]
enum Origin {
    /// A file: one entry per path.
    File,
    /// A text a pseudo-path names (`«string»`, `«arg»`): one entry per text.
    Text,
    /// An embedded text: one entry per text, never linted.
    Internal,
}

pub struct FileInfo {
    pub path: Vec<u8>,
    pub base: u32,
    pub line_starts: Vec<u32>,
    /// The `# iets: allow` lines, set once the file is parsed or its IR
    /// cache entry loaded, before any of its IR runs.
    allows: OnceLock<Allows>,
    /// The source and its tree, read and parsed again for the first finding
    /// in the file (`lint::site`).
    parsed: OnceLock<Option<(Vec<u8>, Ast)>>,
}

impl FileInfo {
    pub(crate) fn new(path: Vec<u8>, src: &[u8], base: u32) -> FileInfo {
        let line_starts = if src.contains(&b'\r') {
            syntax::line_starts(src)
        } else {
            newline_starts(src)
        };
        FileInfo {
            path,
            base,
            line_starts,
            allows: OnceLock::new(),
            parsed: OnceLock::new(),
        }
    }

    /// Every compile of one path sees the same source, so a second set
    /// (a helper fiber compiling the file again) carries the same table.
    pub fn set_allows(&self, allows: Allows) {
        let _ = self.allows.set(allows);
    }

    #[inline]
    pub fn allows(&self) -> &Allows {
        self.allows.get().unwrap_or(Allows::EMPTY)
    }

    /// The source `read` gives and its tree; `None` when there is no source
    /// to read or it does not parse.
    pub fn parsed(&self, read: impl FnOnce() -> Option<Vec<u8>>) -> Option<&(Vec<u8>, Ast)> {
        let parse = || {
            let src = read()?;
            let ast = syntax::parse(&src, FileId(0), &mut Interner::new()).ok()?;
            Some((src, ast))
        };
        self.parsed.get_or_init(parse).as_ref()
    }

    /// 1-based line and column of a byte offset.
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        (line as u32 + 1, offset - self.line_starts[line] + 1)
    }
}

/// `syntax::line_starts` for a source with no `\r`, at memchr speed.
fn newline_starts(src: &[u8]) -> Vec<u32> {
    let mut line_starts = vec![0u32];
    let mut rest = src;
    let mut off = 0usize;
    // SAFETY: memchr reads at most `rest.len()` bytes of `rest`.
    while let Some(i) = memchr::memchr(b'\n', rest) {
        off += i + 1;
        line_starts.push(off as u32);
        rest = &rest[i + 1..];
    }
    line_starts
}

/// The symbols Nix 2.34.8 creates before any other, in its order
/// (eval.hh, `StaticEvalSymbols::preallocate`). A set's attributes are in
/// symbol order, and `==` on two sets compares them in that order, so
/// which of a differing and a throwing attribute it meets first depends on
/// it: `{ a = 1; meta = throw "t"; } == { a = 2; meta = 1; }` throws in
/// Nix. The names must stay first and in this order.
const NIX_STATIC_SYMBOLS: [&[u8]; 52] = [
    b"<with>",
    b"outPath",
    b"drvPath",
    b"type",
    b"meta",
    b"name",
    b"value",
    b"system",
    b"__overrides",
    b"outputs",
    b"outputName",
    b"__ignoreNulls",
    b"file",
    b"line",
    b"column",
    b"__functor",
    b"__toString",
    b"right",
    b"wrong",
    b"__structuredAttrs",
    b"__json",
    b"allowedReferences",
    b"allowedRequisites",
    b"disallowedReferences",
    b"disallowedRequisites",
    b"maxSize",
    b"maxClosureSize",
    b"builder",
    b"args",
    b"__contentAddressed",
    b"__impure",
    b"outputHash",
    b"outputHashAlgo",
    b"outputHashMode",
    b"recurseForDerivations",
    b"description",
    b"self",
    b"",
    b"startSet",
    b"operator",
    b"key",
    b"path",
    b"prefix",
    b"outputSpecified",
    b"__sub",
    b"__lessThan",
    b"__mul",
    b"__div",
    b"or",
    b"__findFile",
    b"__nixPath",
    b"body",
];

/// Interned symbols the evaluator needs by name.
#[derive(Clone)]
pub struct WellKnown {
    pub out_path: Sym,
    pub rev_count: Sym,
    pub drv_path: Sym,
    pub type_: Sym,
    pub functor: Sym,
    pub to_string: Sym,
    pub name: Sym,
    pub value: Sym,
    pub file: Sym,
    pub line: Sym,
    pub column: Sym,
    pub success: Sym,
    pub right: Sym,
    pub wrong: Sym,
    pub version: Sym,
    pub path: Sym,
    pub prefix: Sym,
}

/// Clone gives a fiber its own evaluator: shared tables by handle, a
/// fresh arena, private counters (see docs/explanation/evaluation.md).
#[derive(Clone)]
pub struct Evaluator {
    pub(crate) arena: Arena,
    pub interner: Interner,
    pub files: Files,
    pub globals: Globals,
    pub root_env: &'static EnvObj,
    /// Resolved path -> lazy import thunk (`parallel::import_thunk`).
    import_cache: SharedMap<Vec<u8>, Word>,
    /// `(prefix, path)` entries, `-I` first then `NIX_PATH`.
    search_path: Vec<(Vec<u8>, Vec<u8>)>,
    pub call_depth: u32,
    pub try_level: u32,
    pub s: WellKnown,
    pub builtins_word: Word,
    /// Store path prefix, for `builtins.storeDir` and store path checks.
    pub store_dir: Vec<u8>,
    current_system: &'static str,
    pub nr_thunks: u64,
    pub nr_function_calls: u64,
    pub nr_store_adds: u64,
    pub t_compile: Duration,
    /// Where those counters and `drv.nr_derivations` go when this fiber
    /// finishes a task. Shared, so a stats line covers every fiber.
    totals: Arc<Totals>,
    /// `IETS_STATS`: a stats line will read the arena totals, so a switch
    /// folds into them. Nothing else does, so a normal run skips the fold.
    fold_arenas: bool,
    /// One shared context-free string per symbol, made on first use.
    sym_strings: SymWords,
    /// Source path -> virtual store path, like Nix's `srcToStore`.
    pub src_to_store: SharedMap<Vec<u8>, &'static [u8]>,
    /// Every store path this evaluation added or a fetch found on disk: the
    /// paths a pure evaluation may read (eval.cc:allowPath).
    pub known: SharedMap<StorePath, Known>,
    /// Locked `narHash` -> flake input name, from the lock file, for messages
    /// about an input that must be fetched.
    pub input_names: SharedMap<String, String>,
    /// Project marker path -> present, for `lint::purity`: every path a
    /// derivation holds walks its ancestors, which repeat across derivations.
    pub project_markers: SharedMap<Vec<u8>, bool>,
    /// Lint levels (docs/reference/lints.md).
    lint_config: lint::Config,
    /// The warnings emitted so far.
    pub lints: Lints,
    pub drv: DrvState,
    pub par: parallel::Ctx,
    /// This fiber's shadow stack for the sampled profile.
    prof: Stack,
    /// Read-set recorder for the tracing cache; off unless the run has an entry to write.
    trace: Trace,
    /// Compiled-IR cache for imported files; `None` when disabled.
    ir_cache: Option<Arc<IrCache>>,
    /// The eval store, opened on the first flake input that is missing from it
    /// (`crate::source`).
    pub store: LazyStore,
    /// `==` by Nix's exact rules, not by the shortcut that two equal forced
    /// values are equal. Only the eval fuzz target turns it on, to tell that
    /// known divergence from others (docs/reference/divergences.md).
    #[doc(hidden)]
    pub nix_eq: bool,
    /// `pure-eval` (docs/reference/settings.md).
    pure: bool,
    /// The project this run's sources belong to (`crate::project`). A pure
    /// evaluation reads below it as well as below the store paths it allowed,
    /// so a checkout with no flake evaluates purely. `None` under a classic
    /// name, which answers like nix
    /// (docs/explanation/flakes-and-sources.md).
    project_root: Option<Vec<u8>>,
    project_dir: Arc<OnceLock<Option<fs::File>>>,
    /// Paths [`Evaluator::beneath_project`] proved, true when with the last
    /// link followed.
    beneath: SharedMap<Vec<u8>, bool>,
    /// The session this evaluator was made in: its settings and its log.
    /// A field, not a thread-local lookup, because the fan-out checks read
    /// it on the hot path (docs/reference/library.md).
    session: Arc<Session>,
}

/// A position in a record, `file:line:column`, the way an error names one.
pub enum At {
    Known(&'static [u8], u32, u32),
    None,
}

impl Field for At {
    fn write(self, out: &mut Vec<u8>) {
        let start = out.len();
        match self {
            At::Known(file, line, col) => {
                out.extend_from_slice(file);
                for n in [line, col] {
                    out.push(b':');
                    events::push_u64(out, n as u64);
                }
            }
            At::None => out.extend_from_slice("«none»".as_bytes()),
        }
        events::quote_in_place(out, start);
    }
}

/// What an evaluation knows about one store path.
#[derive(Clone, Default)]
pub struct Known {
    /// Where the content lives when this evaluation added the path; `None`
    /// for a path that is simply on disk.
    pub origin: Option<SourceOrigin>,
    /// The lock node name when the path is the flake root or a locked input:
    /// a tree whose files are the user's or an input's (lint scope) and whose
    /// paths are not re-copies.
    pub flake: Option<String>,
    /// Copied from a local path by this evaluation (`read-through-copy`).
    pub copy: bool,
    /// The tree of the flake on the command line. A fetch from the local
    /// file system reads below its directory through this copy
    /// (`builtins::store_hooks::local_source`).
    pub root: bool,
}

impl SharedMap<StorePath, Known> {
    pub fn origin(&self, p: &StorePath) -> Option<SourceOrigin> {
        self.lock().get(p)?.origin.clone()
    }

    pub fn add(&self, p: StorePath, o: SourceOrigin) {
        self.lock().entry(p).or_default().origin = Some(o);
    }

    /// A path that is simply on disk, such as a realised derivation output:
    /// pure evaluation may read it (primops.cc:realiseContext's `allowPath`).
    pub fn add_on_disk(&self, p: StorePath) {
        self.lock().entry(p).or_default();
    }

    pub fn add_copy(&self, p: StorePath, o: SourceOrigin) {
        let mut t = self.lock();
        let k = t.entry(p).or_default();
        k.origin = Some(o);
        k.copy = true;
    }

    /// The length of the `/nix/store/<base>` prefix of `path` and what the
    /// table has for that store path.
    pub fn under(&self, path: &[u8]) -> Option<(usize, Known)> {
        let rest = strip_store_dir(path)?;
        let end = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
        let sp = StorePath::from_base_name(from_utf8(&rest[..end]).ok()?).ok()?;
        Some((path.len() - rest.len() + end, self.get(&sp)?))
    }

    /// The root is named before any input is fetched and keeps its name.
    pub fn name_flake(&self, p: &StorePath, name: String) {
        if let Some(k) = self.lock().get_mut(p) {
            k.flake.get_or_insert(name);
        }
    }

    pub fn mark_root(&self, p: &StorePath) {
        if let Some(k) = self.lock().get_mut(p) {
            k.root = true;
        }
    }

    /// The root tree and the local directory it stands for.
    pub fn root_tree(&self) -> Option<(StorePath, PathBuf)> {
        let t = self.lock();
        let (p, k) = t.iter().find(|(_, k)| k.root)?;
        let dir = k.origin.as_ref()?.at.local()?;
        Some((p.clone(), dir))
    }

    /// The locked trees no local directory stands for, by lock node name.
    pub fn inputs(&self) -> Vec<(StorePath, String)> {
        self.lock()
            .iter()
            .filter(|(_, k)| k.origin.as_ref().and_then(|o| o.at.local()).is_none())
            .filter_map(|(p, k)| Some((p.clone(), k.flake.clone()?)))
            .collect()
    }

    /// Every source this evaluation added to the (virtual) store and where its content is.
    pub fn origins(&self) -> HashMap<StorePath, SourceOrigin> {
        self.lock()
            .iter()
            .filter_map(|(p, k)| Some((p.clone(), k.origin.clone()?)))
            .collect()
    }
}

macro_rules! bail {
    ($kind:expr, $pos:expr, $($arg:tt)*) => {
        return Err($crate::eval::Error::boxed_fmt($kind, format_args!($($arg)*), $pos))
    };
}
use crate::sys;
pub(crate) use bail;

/// Everything fixed before evaluation starts. A field set after `try_run` clones
/// the evaluator is one fiber's alone, so there is no setter (docs/explanation/evaluation.md).
///
/// ```compile_fail
/// let mut ev = iets::eval::Evaluator::new();
/// ev.pure = true;
/// ```
#[derive(Clone)]
pub struct EvalConfig {
    system: &'static str,
    pure: bool,
    project_root: Option<Vec<u8>>,
    session: Arc<Session>,
    search_path: Vec<(Vec<u8>, Vec<u8>)>,
    lints: lint::Config,
    ir_cache: Option<Arc<IrCache>>,
    keep_aterm: bool,
    keep_site: bool,
    dump_dir: Option<PathBuf>,
    trace: bool,
    trace_store: Option<store::Store>,
}

impl Default for EvalConfig {
    /// In the session this thread entered (docs/reference/library.md).
    fn default() -> EvalConfig {
        EvalConfig {
            system: config::DEFAULT_SYSTEM,
            pure: false,
            project_root: None,
            session: Session::current(),
            search_path: Vec::new(),
            lints: Default::default(),
            ir_cache: None,
            keep_aterm: true,
            keep_site: true,
            dump_dir: None,
            trace: false,
            trace_store: None,
        }
    }
}

fn prefetch_store_entries(entries: &[(Vec<u8>, Vec<u8>)], session: &Arc<Session>) {
    for (_, p) in entries {
        let Some(rest) = strip_store_dir(p) else {
            continue;
        };
        let base = rest.split(|&b| b == b'/').next().unwrap_or_default();
        if let Ok(s) = str::from_utf8(base)
            && let Ok(sp) = StorePath::from_base_name(s)
        {
            locate::prefetch_references(sp, session.clone());
        }
    }
}

impl EvalConfig {
    pub fn new() -> EvalConfig {
        EvalConfig::default()
    }

    pub fn system(mut self, system: &str) -> EvalConfig {
        if system != self.system {
            self.system = Box::leak(system.into());
        }
        self
    }

    /// `pure-eval` (docs/reference/settings.md).
    pub fn pure(mut self, pure: bool) -> EvalConfig {
        self.pure = pure;
        self
    }

    /// The value [`EvalConfig::pure`] settled on. The tracing-cache key needs
    /// it, because the flag of a frontend is not the whole answer.
    pub fn is_pure(&self) -> bool {
        self.pure
    }

    pub fn configured_project_root(&self) -> Option<&[u8]> {
        self.project_root.as_deref()
    }

    /// The project a pure evaluation may read below, with no flake
    /// (`crate::project`). The path must be absolute and hold no trailing
    /// slash, as `crate::project::find` returns it.
    pub fn project_root(mut self, root: Option<Vec<u8>>) -> EvalConfig {
        self.project_root = root;
        self
    }

    /// A host that keeps one session per evaluator needs no thread-local
    /// at all (docs/reference/library.md).
    pub fn session(mut self, session: Arc<Session>) -> EvalConfig {
        self.session = session;
        self
    }

    /// `(prefix, path)` entries, `-I` first then `NIX_PATH`.
    pub fn search_path(mut self, entries: Vec<(Vec<u8>, Vec<u8>)>) -> EvalConfig {
        self.search_path = entries;
        self
    }

    pub fn lints(mut self, config: lint::Config) -> EvalConfig {
        self.lints = config;
        self
    }

    pub fn lint_root(mut self, dir: &[u8]) -> EvalConfig {
        self.lints.add_root(dir);
        self
    }

    /// Compiled-IR cache for imported files; `None` when disabled.
    pub fn ir_cache(mut self, cache: Option<Arc<IrCache>>) -> EvalConfig {
        self.ir_cache = cache;
        self
    }

    /// Keep the ATerm text of every derivation for instantiation, the
    /// cache, `--dump-drvs` and `build`. Off for runs that only print
    /// paths: 331 MB on the outpaths chunk (docs/explanation/evaluation.md).
    pub fn keep_aterm(mut self, keep: bool) -> EvalConfig {
        self.keep_aterm = keep;
        self
    }

    /// Find each derivation's source position (`DrvInfo::pos`), which only
    /// build progress lines and `eval-jobs --provenance` show; the search
    /// walks the attributes of every derivation.
    pub fn keep_site(mut self, keep: bool) -> EvalConfig {
        self.keep_site = keep;
        self
    }

    /// `iets eval --dump-drvs DIR`: every created derivation is written as
    /// `DIR/<drv basename>` in ATerm form.
    pub fn dump_dir(mut self, dir: PathBuf) -> EvalConfig {
        self.dump_dir = Some(dir);
        self
    }

    /// Record the read set, for the tracing cache.
    pub fn trace(mut self, on: bool) -> EvalConfig {
        self.trace = on;
        self
    }

    /// `trace` for a run that has an entry in `store`. The store also
    /// keeps what follows from a revision (`source::scm::rev_count`).
    pub fn trace_in(mut self, store: Option<&store::Store>) -> EvalConfig {
        self.trace = store.is_some();
        self.trace_store = store.cloned();
        self
    }

    #[inline]
    pub fn current_system(&self) -> &'static str {
        self.system
    }

    /// `system` and `pure-eval` shape the builtins, so they are fixed
    /// before `builtins::install` (docs/reference/settings.md).
    pub fn build(self) -> Evaluator {
        if !self.pure {
            prefetch_store_entries(&self.search_path, &self.session);
        }
        let mut arena = Arena::in_heap(self.session.heap.clone());
        let mut interner = Interner::in_heap(self.session.heap.clone());
        for name in NIX_STATIC_SYMBOLS {
            interner.intern(name);
        }
        let root_env = arena.alloc_env(None, 0);
        let s = WellKnown {
            out_path: interner.intern(b"outPath"),
            rev_count: interner.intern(b"revCount"),
            drv_path: interner.intern(b"drvPath"),
            type_: interner.intern(b"type"),
            functor: interner.intern(b"__functor"),
            to_string: interner.intern(b"__toString"),
            name: interner.intern(b"name"),
            value: interner.intern(b"value"),
            file: interner.intern(b"file"),
            line: interner.intern(b"line"),
            column: interner.intern(b"column"),
            success: interner.intern(b"success"),
            right: interner.intern(b"right"),
            wrong: interner.intern(b"wrong"),
            version: interner.intern(b"version"),
            path: interner.intern(b"path"),
            prefix: interner.intern(b"prefix"),
        };
        let mut ev = Evaluator {
            arena,
            interner,
            files: Files::in_heap(self.session.heap.clone()),
            globals: Globals {
                table: FxHashMap::default(),
                builtins: None,
            },
            root_env,
            import_cache: Default::default(),
            // a pure evaluation keeps the empty one (eval.cc:EvalState)
            search_path: if self.pure {
                Vec::new()
            } else {
                self.search_path
            },
            call_depth: 0,
            try_level: 0,
            s,
            builtins_word: Word::NULL,
            store_dir: store_dir().as_bytes().to_vec(),
            current_system: self.system,
            nr_thunks: 0,
            nr_function_calls: 0,
            nr_store_adds: 0,
            t_compile: Duration::ZERO,
            totals: Default::default(),
            fold_arenas: Session::current().settings().stats >= 1,
            sym_strings: SymWords::new(),
            src_to_store: Default::default(),
            known: Default::default(),
            input_names: Default::default(),
            project_markers: Default::default(),
            lint_config: self.lints,
            lints: Default::default(),
            drv: DrvState::new(self.keep_aterm, self.keep_site, self.dump_dir),
            par: Default::default(),
            prof: Default::default(),
            trace: if self.trace {
                Trace::enabled_in(self.trace_store)
            } else {
                Default::default()
            },
            ir_cache: self.ir_cache,
            store: Default::default(),
            nix_eq: false,
            pure: self.pure,
            project_root: self.project_root,
            project_dir: Default::default(),
            beneath: Default::default(),
            session: self.session,
        };
        builtins::install(&mut ev);
        if ev.session.profile.on() {
            ev.session.profile.names(&ev.files, &ev.interner);
        }
        ev
    }
}

impl Evaluator {
    pub fn new() -> Evaluator {
        EvalConfig::new().build()
    }

    #[inline]
    pub fn pure(&self) -> bool {
        self.pure
    }

    /// Whether `path` is this run's project root or lies below it. A pure
    /// evaluation reads there (`builtins::store_hooks::allowed`).
    pub fn in_project(&self, path: &[u8]) -> bool {
        let Some(root) = &self.project_root else {
            return false;
        };
        // a root of "/" becomes "", so that every absolute path is below it
        let root: &[u8] = root.strip_suffix(b"/").unwrap_or(root);
        path.starts_with(root) && (path.len() == root.len() || path[root.len()] == b'/')
    }

    /// Whether the kernel shows that `path`, in the project by its text,
    /// resolves inside it too (`sys::beneath`).
    pub fn beneath_project(&self, path: &[u8], follow: bool) -> bool {
        let Some(root) = &self.project_root else {
            return false;
        };
        if self.beneath.get(path).is_some_and(|full| full || !follow) {
            return true;
        }
        let dir = self
            .project_dir
            .get_or_init(|| fs::File::open(sys::path_from_internal(root)).ok());
        let root: &[u8] = root.strip_suffix(b"/").unwrap_or(root);
        let rel = &path[root.len()..];
        let rel = rel.strip_prefix(b"/").unwrap_or(rel);
        let found = dir
            .as_ref()
            .and_then(|d| sys::beneath(d, &sys::path_from_internal(rel), follow));
        // a missing entry may yet appear as a link
        if found == Some(true) {
            self.beneath.insert(path.to_vec(), follow);
        }
        found.is_some()
    }

    #[inline]
    pub fn current_system(&self) -> &'static str {
        self.current_system
    }

    #[inline]
    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    #[inline]
    pub fn search_path(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.search_path
    }

    #[inline]
    pub fn lint_config(&self) -> &lint::Config {
        &self.lint_config
    }

    #[inline]
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    #[inline]
    pub fn ir_cache(&self) -> Option<&Arc<IrCache>> {
        self.ir_cache.as_ref()
    }

    /// Register a source file: only its path and line table are kept, so
    /// 40 MB of nixpkgs text does not stay resident.
    pub fn add_file(&mut self, path: &[u8], src: &[u8]) -> FileId {
        self.files.add(path, src)
    }

    /// The file holding `pos` and the offset inside it.
    pub fn file_of(&self, pos: Pos) -> Option<(&'static FileInfo, u32)> {
        self.files.file_of(pos)
    }

    pub fn describe_pos(&self, pos: Pos) -> String {
        match self.file_of(pos) {
            Some((f, off)) => {
                let (l, c) = f.line_col(off);
                format!("{}:{}:{}", String::from_utf8_lossy(&f.path), l, c)
            }
            _ => "«none»".to_string(),
        }
    }

    /// Parse and compile a source text. `path` is the file name used for
    /// positions; `base_dir` resolves relative path literals.
    pub fn compile_source(
        &mut self,
        path: &[u8],
        src: Vec<u8>,
        base_dir: &[u8],
        extra_scope: Option<&[Sym]>,
    ) -> EvalResult<IrRef> {
        self.compile_source_inner(path, src, base_dir, extra_scope, Origin::Text)
            .map(|(ir, _, _)| ir)
    }

    /// `compile_source` for the file at `path`, without the IR cache.
    fn compile_path(
        &mut self,
        path: &[u8],
        src: Vec<u8>,
        base_dir: &[u8],
        extra_scope: Option<&[Sym]>,
    ) -> EvalResult<IrRef> {
        self.compile_source_inner(path, src, base_dir, extra_scope, Origin::File)
            .map(|(ir, _, _)| ir)
    }

    /// `compile_source` for an embedded source (corepkgs, `call-flake.nix`):
    /// never linted.
    pub fn compile_internal(&mut self, path: &[u8], src: &[u8]) -> EvalResult<IrRef> {
        self.compile_source_inner(path, src.to_vec(), b"/", None, Origin::Internal)
            .map(|(ir, _, _)| ir)
    }

    /// `compile_source` plus whether a `~/` path literal was compiled and
    /// the lint diagnostics, already reported.
    fn compile_source_inner(
        &mut self,
        path: &[u8],
        src: Vec<u8>,
        base_dir: &[u8],
        extra_scope: Option<&[Sym]>,
        origin: Origin,
    ) -> EvalResult<(IrRef, bool, Vec<Diag>)> {
        let file = match origin {
            Origin::File => self.add_file(path, &src),
            Origin::Text | Origin::Internal => self.files.add_text(path, &src),
        };
        let base = self.files[file.0 as usize].base;
        let ast = syntax::parse(&src, file, &mut self.interner)
            .map_err(|e| Error::boxed(ErrorKind::Parse, e.msg, Pos(base + e.span.start)))?;
        self.files[file.0 as usize].set_allows(ast.allows.clone());
        let t0 = Instant::now();
        let before = self.arena.bytes_allocated;
        let lints = (origin != Origin::Internal).then(|| self.config_for_file(path));
        let mut c = Compiler::new(
            &ast,
            &mut self.arena,
            &mut self.interner,
            &self.globals,
            base,
            base_dir,
            lints.as_ref(),
        );
        let r = c.compile_root(extra_scope);
        let used_home = c.used_home.get();
        let diags = mem::take(&mut c.diags);
        self.arena
            .stats
            .note(Stat::Ir, self.arena.bytes_allocated - before);
        #[cfg(feature = "stats2")]
        self.arena
            .stats
            .census
            .ir_by_file
            .push((file.0, (self.arena.bytes_allocated - before) as u64));
        self.t_compile += sys::elapsed(t0);
        if used_home {
            self.trace.env(b"HOME");
        }
        let ir = r?;
        self.report_diags(&diags);
        Ok((ir, used_home, diags))
    }

    /// `compile_source` for an imported file, through the IR cache when it is on.
    /// An entry is keyed on the source and the compile-time lint levels the file
    /// gets after the scope rule, so a store file under `scope = "root"` keys like
    /// an unlinted one. It carries the compile's lint warnings, which a load
    /// reports again, and the file's `# iets: allow` table for the runtime lints.
    /// Files with `~/` paths bypass it. Embedded corepkgs are never linted.
    pub fn compile_file(
        &mut self,
        path: &[u8],
        src: Vec<u8>,
        base_dir: &[u8],
    ) -> EvalResult<IrRef> {
        if path.starts_with(CorePkgs::PREFIX) {
            return self.compile_internal(path, &src);
        }
        let Some(cache) = self.ir_cache.clone() else {
            return self.compile_path(path, src, base_dir, None);
        };
        if src.len() < cache.min_size {
            return self.compile_path(path, src, base_dir, None);
        }
        let t0 = Instant::now();
        let lint_key = self.config_for_file(path).compile_key();
        let key = cache.key(
            path,
            &src,
            self.current_system,
            &self.store_dir,
            &lint_key,
            self.pure(),
        );
        let file = self.add_file(path, &src);
        let base = self.files[file.0 as usize].base;
        if let Some(bytes) = cache.read(&key) {
            let before = self.arena.bytes_allocated;
            let span = u32::try_from(src.len()).unwrap_or(u32::MAX);
            let loaded = cache::deserialize(self, &bytes, base, span, cache.stats);
            self.arena
                .stats
                .note(Stat::Ir, self.arena.bytes_allocated - before);
            if loaded.is_none() {
                cache.discard(&key);
            }
            if let Some((ir, diags, allows)) = loaded {
                self.files[file.0 as usize].set_allows(allows);
                self.report_diags(&diags);
                if cache.stats {
                    errln!(
                        "ir-cache load {} {} {}us",
                        String::from_utf8_lossy(path),
                        src.len(),
                        t0.elapsed().as_micros()
                    );
                }
                return Ok(ir);
            }
        }
        let (ir, used_home, diags) =
            self.compile_source_inner(path, src, base_dir, None, Origin::File)?;
        let t1 = Instant::now();
        let allows = self.files[file.0 as usize].allows();
        if !used_home && let Some(bytes) = cache::serialize(self, ir, base, &diags, allows) {
            cache.write(&key, &bytes);
        }
        if cache.stats {
            errln!(
                "ir-cache store {} {}us (write {}us)",
                String::from_utf8_lossy(path),
                (t1 - t0).as_micros(),
                t1.elapsed().as_micros()
            );
        }
        Ok(ir)
    }

    /// Evaluate a string expression (`-E`), in `crate::argv`'s encoding.
    pub fn eval_string(&mut self, src: &str, base_dir: &[u8]) -> EvalResult<Word> {
        let ir =
            self.compile_source(b"\xc2\xabstring\xc2\xbb", argv::bytes(src), base_dir, None)?;
        let root = self.root_env;
        self.eval(root, ir)
    }

    /// `import`: parse, compile, evaluate and cache a file.
    pub fn eval_file(&mut self, path: &[u8], pos: Pos) -> EvalResult<Word> {
        let resolved = self.resolve_import_path(path, pos)?;
        // parallel: one lazy thunk per file; forcing it does the work once
        let w = match self.import_cache.get(&resolved) {
            Some(w) => w,
            None => {
                let t = parallel::import_thunk(self, &resolved, pos);
                self.import_cache.get_or_insert(resolved, t)
            }
        };
        self.force(w)
    }

    /// Like `eval_file` but with `scope` attrs bound above the globals; not
    /// cached.
    pub fn scoped_import(
        &mut self,
        scope: &'static AttrsObj,
        path: &[u8],
        pos: Pos,
    ) -> EvalResult<Word> {
        let resolved = self.resolve_import_path(path, pos)?;
        let src = self.read_source(&resolved, pos)?;
        let names: Vec<Sym> = scope.iter().map(|a| a.sym).collect();
        let dir = dir_of(&resolved).to_vec();
        let ir = self.compile_path(&resolved, src, &dir, Some(&names))?;
        let env = self.arena.alloc_env(Some(self.root_env), names.len());
        for (slot, a) in env.slots_n(names.len()).iter().zip(scope.iter()) {
            slot.set(a.cell.get());
        }
        self.eval(env, ir)
    }

    fn resolve_import_path(&mut self, path: &[u8], pos: Pos) -> EvalResult<Vec<u8>> {
        let mut p = path.to_vec();
        if p.starts_with(CorePkgs::PREFIX) {
            return Ok(p);
        }
        store_hooks::confine(self, &p, store_hooks::Follow::Full, pos)?;
        let mut at = store_hooks::open(self, &p, pos)?;
        read_through_copy(self, &p, pos)?;
        // eval.cc:resolveExprPath follows a symlink on the last component,
        // so a relative path in the file resolves against the directory of
        // the target. The `lstat` is what it costs; a path inside a copied
        // tree keeps `store_hooks`' own resolution.
        if let Some(local) = at.local()
            && fs::symlink_metadata(&local).is_ok_and(|m| m.is_symlink())
            && store_hooks::locate(self, &p).is_none()
        {
            for _ in 0..1024 {
                let Ok(target) = fs::read_link(sys::path_from_internal(&p)) else {
                    break;
                };
                let t = sys::path_to_internal(&target);
                let joined = if t.starts_with(b"/") {
                    t
                } else {
                    let mut d = dir_of(&p).to_vec();
                    d.push(b'/');
                    d.extend_from_slice(&t);
                    d
                };
                p = compile::canon_path(&joined);
            }
            let full = sys::fs::canonicalize(&local);
            self.trace()
                .canon(&sys::path_to_internal(&local), full.ok().as_deref());
            at = store_hooks::open(self, &p, pos)?;
        }
        match at.is_dir(self) {
            Ok(true) => p.extend_from_slice(b"/default.nix"),
            Ok(false) => {}
            Err(e) => bail!(
                ErrorKind::Io,
                pos,
                "getting status of '{}': {}",
                String::from_utf8_lossy(&p),
                crate::error::io_text(&e)
            ),
        }
        Ok(p)
    }

    /// `<name>` lookup.
    pub fn find_file(
        &mut self,
        entries: &[(Vec<u8>, Vec<u8>)],
        name: &[u8],
        pos: Pos,
    ) -> EvalResult<Vec<u8>> {
        for (prefix, path) in entries {
            // search-path.cc:Prefix::suffixIfPotentialMatch: the whole prefix
            // must match, so a prefix may hold a slash
            let n = prefix.len();
            let sep = n > 0 && n < name.len();
            if (sep && name[n] != b'/') || !name.starts_with(prefix) {
                continue;
            }
            let suffix = &name[if sep { n + 1 } else { n }..];
            if search_path::is_url(path) {
                bail!(
                    ErrorKind::Unsupported,
                    pos,
                    "the search path entry '{}' is a URL, which iets does not fetch",
                    String::from_utf8_lossy(path)
                );
            }
            store_hooks::open(self, path, pos)?;
            let mut candidate = path.clone();
            if !suffix.is_empty() {
                candidate.push(b'/');
                candidate.extend_from_slice(suffix);
            }
            let cp = compile::canon_path(&candidate);
            let kind = store_hooks::stat(self, &cp, true);
            if kind != trace::Kind::Missing {
                return Ok(cp);
            }
        }
        if let Some(rest) = name.strip_prefix(b"nix/".as_slice())
            && builtins::corepkgs(rest).is_some()
        {
            let mut p = CorePkgs::PREFIX.to_vec();
            p.extend_from_slice(rest);
            return Ok(p);
        }
        if self.pure {
            bail!(
                ErrorKind::Throw,
                pos,
                "cannot look up '<{}>' in pure evaluation mode (use '--impure' to override)",
                String::from_utf8_lossy(name)
            )
        }
        bail!(
            ErrorKind::Throw,
            pos,
            "file '{}' was not found in the Nix search path (add it using $NIX_PATH or -I)",
            String::from_utf8_lossy(name)
        )
    }

    pub(crate) fn read_source(&self, resolved: &[u8], pos: Pos) -> EvalResult<Vec<u8>> {
        if let Some(rest) = resolved.strip_prefix(CorePkgs::PREFIX)
            && let Some(src) = builtins::corepkgs(rest)
        {
            return Ok(src.to_vec());
        }
        match store_hooks::resolve(self, resolved).read(self, false) {
            Ok(s) => Ok(s),
            Err((_, e)) => bail!(
                ErrorKind::Io,
                pos,
                "opening file '{}': {}",
                String::from_utf8_lossy(resolved),
                e
            ),
        }
    }

    /// Force a word to weak head normal form.
    #[inline]
    pub fn force(&mut self, w: Word) -> EvalResult<Word> {
        if w.is_thunk() {
            self.force_thunk(w)
        } else {
            Ok(w)
        }
    }

    /// Force the word in a cell and write the result back. Only the demand
    /// path writes: a cell that holds a value is a cell the program asked
    /// for, which `==` and the printers read.
    #[inline]
    pub fn force_cell(&mut self, cell: &Cell) -> EvalResult<Word> {
        let w = cell.get();
        if !w.is_thunk() {
            return Ok(w);
        }
        let v = self.force_thunk(w)?;
        if self.par.is_main {
            cell.set(v);
        }
        Ok(v)
    }

    fn force_thunk(&mut self, w: Word) -> EvalResult<Word> {
        if self.par.on {
            return parallel::force_thunk(self, w); // parallel: claim protocol
        }
        let state = w.thunk_state_cell();
        let st = state.get();
        if st.is_blackhole() {
            bail!(
                ErrorKind::InfiniteRecursion,
                self.thunk_pos(w),
                "infinite recursion encountered"
            );
        }
        if !w.thunk_unforced(st) {
            return Ok(w.thunk_value(st));
        }
        state.set(Word::BLACKHOLE);
        #[cfg(feature = "stats2")]
        {
            self.arena.stats.census.forced[match w.thunk_kind() {
                Kind::Thunk => 0,
                Kind::ThunkApp => 1,
                Kind::ThunkApp2 => 2,
                _ => 3,
            }] += 1;
            self.arena.stats.census.note_thunk_forced(w.bits());
        }
        let v = self.run_thunk(w, st);
        match v {
            Ok(v) => {
                debug_assert!(!v.is_thunk());
                let done = w.thunk_finish(v, true);
                state.set(done);
                Ok(v)
            }
            Err(e) => {
                state.set(st);
                Err(e)
            }
        }
    }

    /// Evaluate a claimed thunk from its payload word `st`.
    #[inline]
    pub fn run_thunk(&mut self, w: Word, st: Word) -> EvalResult<Word> {
        match w.thunk_kind() {
            Kind::Thunk => {
                let t = w.as_thunk();
                self.eval(ThunkObj::env_of(st), t.ir())
            }
            Kind::ThunkApp => {
                let f = w.as_thunk_app().code.get();
                self.apply(f, &[st], Pos::NONE)
            }
            Kind::ThunkApp2 => {
                let t = w.as_thunk_app2();
                self.apply(t.code.get(), &[t.arg1, st], Pos::NONE)
            }
            _ => {
                let (d, i) = w.map_desc();
                let name = self.sym_string(d.out.attrs_raw()[i].sym);
                let v = d.src.attrs_raw()[i].cell.get();
                self.apply(d.f, &[name, v], Pos::NONE)
            }
        }
    }

    /// Position of a claimed or unforced thunk, for the recursion error.
    pub fn thunk_pos(&self, w: Word) -> Pos {
        match w.thunk_kind() {
            Kind::Thunk => ir_pos(w.as_thunk().ir()),
            _ => Pos::NONE,
        }
    }

    /// Wrap `ir` for lazy evaluation in `env`, sharing existing values and
    /// thunks for cheap expressions.
    #[inline]
    pub fn maybe_thunk(&mut self, env: &'static EnvObj, ir: IrRef) -> Word {
        match ir {
            Ir::Const(w) => *w,
            Ir::Local { depth, idx } => {
                let mut e = env;
                for _ in 0..*depth {
                    e = e.up().expect("compile-time depth is valid");
                }
                e.mark_read();
                let w = e.slot(*idx as usize).get();
                // a slot of the env under construction (`let x = x;`): thunk it
                // so forcing reports infinite recursion instead of reading EMPTY
                if w == Word::EMPTY {
                    self.nr_thunks += 1;
                    return self.arena.mk_thunk(env, ir);
                }
                w
            }
            Ir::Lambda(l) => self.arena.mk_lambda(env, l),
            // `n - 1` on an already-forced int cannot fail except on
            // overflow, so compute it now instead of allocating a thunk
            Ir::Sub(l, r, _) => match (self.forced_int(env, l), self.forced_int(env, r)) {
                (Some(a), Some(b)) => match a.checked_sub(b) {
                    Some(v) if Word::fits_imm(v) => Word::imm_int(v),
                    _ => self.mk_thunk_counted(env, ir),
                },
                _ => self.mk_thunk_counted(env, ir),
            },
            Ir::Concat {
                force_string: false,
                parts,
                ..
            } if parts.len() == 2 => match (
                self.forced_int(env, parts[0]),
                self.forced_int(env, parts[1]),
            ) {
                (Some(a), Some(b)) => match a.checked_add(b) {
                    Some(v) if Word::fits_imm(v) => Word::imm_int(v),
                    _ => self.mk_thunk_counted(env, ir),
                },
                _ => self.mk_thunk_counted(env, ir),
            },
            // `x.y` on an attrset that is already forced: the attr's cell
            // holds the value that a fresh thunk would only look up, so
            // share it. Missing attrs and unforced subjects keep the lazy
            // path and its error position.
            Ir::Select(s) => match self.shared_select(env, s.subject, s.path, s.default) {
                Some(w) => w,
                None => self.mk_thunk_counted(env, ir),
            },
            Ir::Select1 {
                subject,
                sym,
                default,
                ..
            } => match self.shared_select(env, subject, &[AttrElemIr::Static(*sym)], *default) {
                Some(w) => w,
                None => self.mk_thunk_counted(env, ir),
            },
            _ => self.mk_thunk_counted(env, ir),
        }
    }

    #[inline]
    fn mk_thunk_counted(&mut self, env: &'static EnvObj, ir: IrRef) -> Word {
        self.nr_thunks += 1;
        self.arena.mk_thunk(env, ir)
    }

    fn shared_select(
        &mut self,
        env: &'static EnvObj,
        subject: IrRef,
        path: &[AttrElemIr],
        default: Option<IrRef>,
    ) -> Option<Word> {
        let mut w = Self::peek(env, subject)?;
        for elem in path {
            let AttrElemIr::Static(sym) = elem else {
                return None;
            };
            if !matches!(w.heap_kind(), Some(Kind::Attrs | Kind::AttrsUnion)) {
                return None;
            }
            match w.as_attrs()?.get(*sym) {
                Some(a) => {
                    w = a.cell.get();
                    if w == Word::EMPTY {
                        return None;
                    }
                }
                None => {
                    w = self.maybe_thunk(env, default?);
                    break;
                }
            }
        }
        // the subject may be forced or not here depending on which fiber got
        // to it first, so share only what `==` cannot tell from a fresh thunk
        (!w.is_thunk() && w.value_type() != ValueType::Function).then_some(w)
    }

    /// What a constant or a local holds, without evaluating anything.
    #[inline]
    fn peek(env: &'static EnvObj, ir: IrRef) -> Option<Word> {
        match ir {
            Ir::Const(w) => Some(*w),
            Ir::Local { depth, idx } => {
                let mut e = env;
                for _ in 0..*depth {
                    e = e.up()?;
                }
                e.mark_read();
                Some(e.slot(*idx as usize).get())
            }
            _ => None,
        }
    }

    /// The value of a cheap operand if it is already a forced integer.
    #[inline]
    fn forced_int(&self, env: &'static EnvObj, ir: IrRef) -> Option<i64> {
        let w = Self::peek(env, ir)?;
        if w.is_imm_int() {
            Some(w.imm_int_value())
        } else {
            None
        }
    }

    /// Recursively force lists and attrs (`--strict`).
    pub fn force_deep(&mut self, w: Word) -> EvalResult<Word> {
        if self.par.on {
            return parallel::force_deep(self, w); // parallel: fan-out walk
        }
        let mut seen = FxHashSet::default();
        let w = self.force(w)?;
        self.force_deep_inner(w, &mut seen)?;
        Ok(w)
    }

    fn force_deep_inner(&mut self, w: Word, seen: &mut FxHashSet<u64>) -> EvalResult<()> {
        match w.view() {
            Value::Attrs(a) => {
                if !seen.insert(w.bits()) {
                    return Ok(());
                }
                for attr in a.iter() {
                    let v = self.force_cell(&attr.cell).map_err(|mut e| {
                        e.add_trace(
                            attr.pos,
                            format!(
                                "while evaluating the attribute '{}'",
                                self.interner.name_str(attr.sym)
                            ),
                        );
                        e
                    })?;
                    self.force_deep_inner(v, seen)?;
                }
            }
            Value::List(l) => {
                if !seen.insert(w.bits()) {
                    return Ok(());
                }
                for c in l.cells() {
                    let v = self.force_cell(c)?;
                    self.force_deep_inner(v, seen)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn type_error<T>(&self, expected: &str, got: Word, pos: Pos, ctx: &str) -> EvalResult<T> {
        let mut e = Error::new(
            ErrorKind::Type,
            format!(
                "expected {} but found {}: {}",
                expected,
                got.value_type().name(),
                crate::print::show_value_short(self, got)
            ),
            pos,
        );
        if !ctx.is_empty() {
            e.add_trace(pos, ctx);
        }
        Err(Box::new(e))
    }

    #[inline]
    pub fn force_attrs(&mut self, w: Word, pos: Pos, ctx: &str) -> EvalResult<&'static AttrsObj> {
        let w = self.force(w)?;
        match w.view() {
            Value::Attrs(a) => Ok(a),
            _ => self.type_error("a set", w, pos, ctx),
        }
    }

    #[inline]
    pub fn force_list(&mut self, w: Word, pos: Pos, ctx: &str) -> EvalResult<&'static ListObj> {
        let w = self.force(w)?;
        match w.view() {
            Value::List(l) => Ok(l),
            _ => self.type_error("a list", w, pos, ctx),
        }
    }

    #[inline]
    pub fn force_int(&mut self, w: Word, pos: Pos, ctx: &str) -> EvalResult<i64> {
        let w = self.force(w)?;
        match w.view() {
            Value::Int(i) => Ok(i),
            _ => self.type_error("an integer", w, pos, ctx),
        }
    }

    pub fn force_float(&mut self, w: Word, pos: Pos, ctx: &str) -> EvalResult<f64> {
        let w = self.force(w)?;
        match w.view() {
            Value::Int(i) => Ok(i as f64),
            Value::Float(f) => Ok(f),
            _ => self.type_error("a float", w, pos, ctx),
        }
    }

    #[inline]
    pub fn force_bool(&mut self, w: Word, pos: Pos, ctx: &str) -> EvalResult<bool> {
        let w = self.force(w)?;
        match w.view() {
            Value::Bool(b) => Ok(b),
            _ => self.type_error("a Boolean", w, pos, ctx),
        }
    }

    /// Force a string, returning the object (with context).
    #[inline]
    pub fn force_string(&mut self, w: Word, pos: Pos, ctx: &str) -> EvalResult<&'static StringObj> {
        let w = self.force(w)?;
        match w.view() {
            Value::String(s) => Ok(s),
            _ => self.type_error("a string", w, pos, ctx),
        }
    }

    /// Force a string that must not carry context.
    pub fn force_string_no_ctx(
        &mut self,
        w: Word,
        pos: Pos,
        ctx: &str,
    ) -> EvalResult<&'static [u8]> {
        Ok(self.force_string_obj_no_ctx(w, pos, ctx)?.bytes)
    }

    fn force_string_obj_no_ctx(
        &mut self,
        w: Word,
        pos: Pos,
        ctx: &str,
    ) -> EvalResult<&'static StringObj> {
        let s = self.force_string(w, pos, ctx)?;
        if let Some(first) = s.ctx.iter().find(|c| c.is_store()) {
            let mut e = Error::new(
                ErrorKind::Eval,
                format!(
                    "the string '{}' is not allowed to refer to a store path (such as '{}')",
                    String::from_utf8_lossy(s.bytes),
                    crate::print::ctx_display(first)
                ),
                pos,
            );
            if !ctx.is_empty() {
                e.add_trace(pos, ctx);
            }
            return Err(Box::new(e));
        }
        Ok(s)
    }

    pub fn force_function(&mut self, w: Word, pos: Pos, ctx: &str) -> EvalResult<Word> {
        let w = self.force(w)?;
        match w.view() {
            Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_) => Ok(w),
            Value::Attrs(a) if a.get(self.s.functor).is_some() => Ok(w),
            _ => self.type_error("a function", w, pos, ctx),
        }
    }

    /// Mirrors eval.cc:EvalState::isDerivation, which lets an error of the
    /// `type` attribute reach the caller.
    pub fn is_derivation(&mut self, w: Word) -> EvalResult<bool> {
        match w.view() {
            Value::Attrs(a) => match a.get(self.s.type_) {
                Some(attr) => {
                    let t = self.force_cell(&attr.cell)?;
                    Ok(t.as_string().is_some_and(|s| s.bytes == b"derivation"))
                }
                None => Ok(false),
            },
            _ => Ok(false),
        }
    }

    pub fn mk_string(&mut self, bytes: &[u8]) -> Word {
        self.arena.mk_string(bytes)
    }

    pub fn mk_int(&mut self, i: i64) -> Word {
        self.arena.mk_int(i)
    }

    pub fn mk_float(&mut self, f: f64) -> Word {
        self.arena.mk_float(f)
    }

    pub fn mk_list(&mut self, items: &[Word]) -> Word {
        self.arena.mk_list(items)
    }

    pub fn mk_attrs(&mut self, entries: &mut [(Sym, Pos, Word)], pos: Pos) -> EvalResult<Word> {
        match self.arena.mk_attrs(entries) {
            Ok(w) => Ok(w),
            Err(dup) => bail!(
                ErrorKind::Eval,
                pos,
                "attribute '{}' already defined",
                self.interner.name_str(dup)
            ),
        }
    }

    pub fn empty_attrs(&mut self) -> Word {
        self.arena.mk_attrs_sorted(&[])
    }

    pub fn sym(&mut self, name: &[u8]) -> Sym {
        self.interner.intern(name)
    }

    /// `force_string_no_ctx` as a symbol. The symbol is kept in the string
    /// object: names passed again and again, such as the literals of a
    /// `removeAttrs` call, are hashed once.
    pub fn force_sym(&mut self, w: Word, pos: Pos, ctx: &str) -> EvalResult<Sym> {
        let s = self.force_string_obj_no_ctx(w, pos, ctx)?;
        if let Some(sym) = s.cached_sym() {
            return Ok(sym);
        }
        let sym = self.interner.intern(s.bytes);
        s.cache_sym(sym);
        Ok(sym)
    }

    /// The string value of a symbol's name. attrNames and mapAttrs hand
    /// out the same object for the same name instead of copying it.
    pub fn sym_string(&mut self, sym: Sym) -> Word {
        let w = self.sym_strings.get(sym);
        if w != Word::EMPTY {
            return w;
        }
        let w = self.arena.mk_string(self.interner.name(sym));
        if let Value::String(s) = w.view() {
            s.cache_sym(sym);
        }
        self.sym_strings.set(sym, w);
        w
    }

    /// `{ file, line, column }` for `__curPos` and `unsafeGetAttrPos`.
    pub fn mk_pos_attrs(&mut self, pos: Pos) -> Word {
        let Some((f, off)) = self.file_of(pos) else {
            return Word::NULL;
        };
        if f.path.starts_with(b"\xc2\xab") {
            return Word::NULL;
        }
        let (line, col) = f.line_col(off);
        let path = f.path.clone();
        let file = self.mk_string(&path);
        let line = self.mk_int(line as i64);
        let col = self.mk_int(col as i64);
        let mut entries = vec![
            (self.s.file, Pos::NONE, file),
            (self.s.line, Pos::NONE, line),
            (self.s.column, Pos::NONE, col),
        ];
        self.arena.mk_attrs(&mut entries).expect("distinct keys")
    }

    #[inline]
    pub(crate) fn enter_call(&mut self, pos: Pos) -> EvalResult<()> {
        self.call_depth += 1;
        if self.call_depth > MAX_CALL_DEPTH {
            self.call_depth -= 1;
            return Err(Error::stack_overflow(pos));
        }
        Ok(())
    }

    /// Apply `func` to `args` left to right. Returns a forced value.
    pub fn apply(&mut self, func: Word, args: &[Word], pos: Pos) -> EvalResult<Word> {
        self.enter_call(pos)?;
        let depth = self.prof.depth();
        let r = self.apply_inner(func, args, pos);
        self.prof.unwind(depth);
        self.call_depth -= 1;
        r
    }

    /// Takes a sample when the tick thread asked for one. The arena is
    /// touched only here, so a call that is not sampled does not read it.
    #[inline]
    fn prof_take(&mut self) {
        if self.prof.due(&self.session.profile) {
            let bytes = self.arena.bytes_since_sample();
            self.prof.take(&self.session.profile, bytes);
        }
    }

    /// Enters a frame for a call of `name` at `pos`, and answers the depth
    /// the caller returns to.
    #[inline]
    fn prof_enter(&mut self, pos: Pos, name: Option<Sym>) -> usize {
        let d = self.prof.depth();
        self.prof.enter(profile::frame(pos, name));
        self.prof_take();
        d
    }

    /// Enters a frame for a call of the builtin `sym` at `pos`.
    #[inline]
    fn prof_enter_op(&mut self, pos: Pos, sym: Sym) -> usize {
        let d = self.prof.depth();
        self.prof.enter(profile::primop_frame(pos, sym));
        self.prof_take();
        d
    }

    fn apply_inner(&mut self, func: Word, args: &[Word], pos: Pos) -> EvalResult<Word> {
        let mut cur = self.force(func)?;
        let mut args = args;
        let mut functors = 0u32;
        while !args.is_empty() {
            match cur.view() {
                Value::Lambda(l) => {
                    let (mut fenv, mut lam) = (l.env, l.lam);
                    loop {
                        // `lam` and the lambdas in its body that extend its
                        // frame (`a: b: e`) take their args in one env
                        let mut last = lam;
                        let mut n = 1;
                        while n < args.len()
                            && let Ir::Lambda(inner) = last.body
                            && inner.base > 0
                        {
                            last = inner;
                            n += 1;
                        }
                        let env2 = if lam.no_env {
                            fenv
                        } else {
                            self.frame_env(fenv, lam, last.frame)
                        };
                        let mut cur_lam = lam;
                        let mut d = 0;
                        for (i, a) in args[..n].iter().enumerate() {
                            if i > 0 {
                                let Ir::Lambda(inner) = cur_lam.body else {
                                    unreachable!("the walk above found a lambda");
                                };
                                cur_lam = inner;
                                self.prof.unwind(d);
                            }
                            self.nr_function_calls += 1;
                            d = self.prof_enter(pos, cur_lam.name);
                            if !cur_lam.no_env {
                                self.bind_params(env2, cur_lam, *a, pos)?;
                            }
                        }
                        args = &args[n..];
                        // `a: b: e` applied to two arguments with `b` in an
                        // env of its own: bind `b` without the closure
                        if !args.is_empty()
                            && let Ir::Lambda(inner) = last.body
                        {
                            self.prof.unwind(d);
                            (fenv, lam) = (env2, inner);
                            continue;
                        }
                        cur = self.eval(env2, last.body).map_err(|mut e| {
                            e.add_trace(
                                last.pos,
                                format!("while calling {}", self.lambda_desc(last)),
                            );
                            if !pos.is_none() {
                                e.add_trace(pos, "from call site");
                            }
                            e
                        })?;
                        self.prof.unwind(d);
                        break;
                    }
                }
                Value::PrimOp(op) => {
                    let arity = op.arity as usize;
                    if args.len() < arity {
                        return Ok(self.arena.mk_primop_app(op, args));
                    }
                    let d = self.prof_enter_op(pos, op.sym);
                    cur = (op.f)(self, &args[..arity], pos).map_err(|mut e| {
                        e.add_trace(pos, format!("while calling the '{}' builtin", op.name));
                        e
                    })?;
                    self.prof.unwind(d);
                    args = &args[arity..];
                }
                Value::PrimOpApp(app) => {
                    let op = app.op;
                    let arity = op.arity as usize;
                    let need = arity - app.args.len();
                    if args.len() < need {
                        let mut all = app.args.to_vec();
                        all.extend_from_slice(args);
                        return Ok(self.arena.mk_primop_app(op, &all));
                    }
                    let mut all: [Word; 8] = [Word::EMPTY; 8];
                    all[..app.args.len()].copy_from_slice(app.args);
                    all[app.args.len()..arity].copy_from_slice(&args[..need]);
                    let d = self.prof_enter_op(pos, op.sym);
                    cur = (op.f)(self, &all[..arity], pos).map_err(|mut e| {
                        e.add_trace(pos, format!("while calling the '{}' builtin", op.name));
                        e
                    })?;
                    self.prof.unwind(d);
                    args = &args[need..];
                }
                Value::Attrs(a) => {
                    let Some(functor) = a.get(self.s.functor) else {
                        bail!(
                            ErrorKind::Type,
                            pos,
                            "attempt to call something which is not a function but a set: {}",
                            crate::print::show_value_short(self, cur)
                        );
                    };
                    // eval.cc:callFunction recurses through the functor, so
                    // its call depth bounds the chain; this loop counts the
                    // chain itself, against the same limit, and a functor
                    // that gives back a functor ends there
                    functors += 1;
                    if functors > MAX_CALL_DEPTH {
                        return Err(Error::stack_overflow(pos));
                    }
                    let f = self.force_cell(&functor.cell)?;
                    cur = self.apply(f, &[cur], pos)?;
                }
                _ => bail!(
                    ErrorKind::Type,
                    pos,
                    "attempt to call something which is not a function but {}: {}",
                    cur.value_type().name(),
                    crate::print::show_value_short(self, cur)
                ),
            }
        }
        Ok(cur)
    }

    /// An env of `size` slots for a call of `lam` in the closure env
    /// `fenv`. A lambda that extends its parent's frame starts from a copy
    /// of the parent's slots, above the parent's env.
    fn frame_env(
        &mut self,
        fenv: &'static EnvObj,
        lam: &'static LambdaIr,
        size: u32,
    ) -> &'static EnvObj {
        if lam.base == 0 {
            return self.arena.alloc_env(Some(fenv), size as usize);
        }
        let n = lam.base as usize;
        let env2 = self.arena.alloc_env(fenv.up(), size as usize);
        for (to, from) in env2.slots_n(n).iter().zip(fenv.slots_n(n)) {
            to.set(from.get());
        }
        env2
    }

    /// `lam`'s params from `arg`, in `env2` from slot `lam.base` on.
    fn bind_params(
        &mut self,
        env2: &'static EnvObj,
        lam: &'static LambdaIr,
        arg: Word,
        pos: Pos,
    ) -> EvalResult<()> {
        let base = lam.base as usize;
        let slots = &env2.slots_n(base + lam.nslots())[base..];
        match &lam.param {
            ParamIr::Ident => slots[0].set(arg),
            ParamIr::Formals {
                has_bind,
                formals,
                ellipsis,
            } => {
                let attrs = self
                    .force_attrs(
                        arg,
                        lam.pos,
                        "while evaluating the value passed for the lambda argument",
                    )
                    .map_err(|mut e| {
                        if !pos.is_none() {
                            e.add_trace(pos, "from call site");
                        }
                        e
                    })?;
                let mut i = 0;
                if *has_bind {
                    slots[0].set(arg);
                    i = 1;
                }
                let mut used = 0;
                for f in formals.iter() {
                    match attrs.get(f.sym) {
                        Some(a) => {
                            used += 1;
                            slots[i].set(a.cell.get());
                        }
                        None => match f.default {
                            Some(d) => {
                                let w = self.maybe_thunk(env2, d);
                                slots[i].set(w);
                            }
                            None => {
                                let mut e = Error::new(
                                    ErrorKind::Type,
                                    format!(
                                        "function '{}' called without required argument '{}'",
                                        self.lambda_name(lam),
                                        self.interner.name_str(f.sym)
                                    ),
                                    lam.pos,
                                );
                                e.add_trace(pos, "from call site");
                                return Err(Box::new(e));
                            }
                        },
                    }
                    i += 1;
                }
                if !*ellipsis && used != attrs.len() {
                    for a in attrs.iter() {
                        if formals.binary_search_by(|f| f.sym.cmp(&a.sym)).is_err() {
                            let mut e = Error::new(
                                ErrorKind::Type,
                                format!(
                                    "function '{}' called with unexpected argument '{}'",
                                    self.lambda_name(lam),
                                    self.interner.name_str(a.sym)
                                ),
                                lam.pos,
                            );
                            e.add_trace(pos, "from call site");
                            return Err(Box::new(e));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn lambda_name(&self, lam: &LambdaIr) -> String {
        match lam.name {
            Some(n) => self.interner.name_str(n).to_string(),
            None => "anonymous lambda".to_string(),
        }
    }

    fn lambda_desc(&self, lam: &LambdaIr) -> String {
        match lam.name {
            Some(n) => format!("'{}'", self.interner.name_str(n)),
            None => "anonymous lambda".to_string(),
        }
    }

    #[inline]
    pub fn call1(&mut self, f: Word, a: Word, pos: Pos) -> EvalResult<Word> {
        self.apply(f, &[a], pos)
    }

    #[inline]
    pub fn call2(&mut self, f: Word, a: Word, b: Word, pos: Pos) -> EvalResult<Word> {
        self.apply(f, &[a, b], pos)
    }

    /// Evaluate `ir` in `env` to weak head normal form.
    pub fn eval(&mut self, env: &'static EnvObj, ir: IrRef) -> EvalResult<Word> {
        match ir {
            Ir::Const(w) => Ok(*w),
            Ir::Local { depth, idx } => {
                let mut e = env;
                for _ in 0..*depth {
                    e = e.up().expect("compile-time depth is valid");
                }
                e.mark_read();
                self.force_cell(e.slot(*idx as usize))
            }
            Ir::WithVar { sym, withs, pos } => self.eval_with_var(env, *sym, withs, *pos),
            Ir::CurPos(pos) => Ok(self.mk_pos_attrs(*pos)),
            Ir::Concat {
                force_string,
                parts,
                pos,
            } => self.eval_concat(env, *force_string, parts, *pos),
            Ir::SearchPath { name, pos } => {
                let n = self.eval(env, name)?;
                let n = self.force_string_no_ctx(n, *pos, "while evaluating a path segment")?;
                let entries = self.search_path.clone();
                let p = self.find_file(&entries, n, *pos)?;
                Ok(self.arena.mk_path(&p))
            }
            Ir::Select(s) => self.eval_select(env, s.subject, s.path, s.default, s.pos),
            Ir::SelectCount(s) => self.eval_select_count(env, s),
            Ir::Select1 {
                subject,
                sym,
                default,
                pos,
            } => self.eval_select1(env, subject, *sym, *default, *pos),
            Ir::HasAttr { subject, path } => {
                let mut v = self.eval(env, subject)?;
                for elem in path.iter() {
                    let name = self.attr_name(env, elem)?;
                    let attrs = self.force(v)?;
                    match attrs.as_attrs().and_then(|a| a.get(name)) {
                        Some(a) => v = a.cell.get(),
                        None => return Ok(Word::FALSE),
                    }
                }
                Ok(Word::TRUE)
            }
            Ir::Attrs(a) => self.eval_attrs(env, a),
            Ir::List(items) => {
                let l = self.arena.alloc_list_as(items.len(), Stat::ListLit);
                for (c, item) in l.cells().iter().zip(items.iter()) {
                    let w = self.maybe_thunk(env, item);
                    c.set(w);
                }
                Ok(Word::from_ref(l))
            }
            Ir::Lambda(l) => Ok(self.arena.mk_lambda(env, l)),
            Ir::Apply { func, args, pos } => {
                let f = self.eval(env, func)?;
                let mut buf: [Word; 8] = [Word::EMPTY; 8];
                if args.len() <= 8 {
                    for (i, a) in args.iter().enumerate() {
                        buf[i] = self.maybe_thunk(env, a);
                    }
                    self.apply(f, &buf[..args.len()], *pos)
                } else {
                    let v: Vec<Word> = args.iter().map(|a| self.maybe_thunk(env, a)).collect();
                    self.apply(f, &v, *pos)
                }
            }
            Ir::Let(l) => {
                let env2 = if l.base == LetIr::OWN_ENV {
                    self.alloc_rec_env(env, l.binds, l.from, false)
                } else {
                    // the lambda's call made the slots; `inherit x` reads
                    // its params, which live in the same env
                    self.fill_rec_slots(env, env, l.base as usize, l.binds, l.from, false);
                    env
                };
                self.eval(env2, l.body)
            }
            Ir::With { scope, body, .. } => {
                let env2 = self.arena.alloc_env(Some(env), 1);
                let w = self.maybe_thunk(env, scope);
                env2.slot(0).set(w);
                self.eval(env2, body)
            }
            Ir::If {
                cond,
                then,
                else_,
                pos,
            } => {
                let c = self.eval_bool(env, cond, *pos, "while evaluating a branch condition")?;
                self.eval(env, if c { then } else { else_ })
            }
            Ir::Assert { cond, body, pos } => {
                if !self.eval_bool(env, cond, *pos, "in the condition of the assert statement")? {
                    bail!(ErrorKind::Assert, *pos, "assertion failed");
                }
                self.eval(env, body)
            }
            Ir::Not(e, pos) => {
                let b = self.eval_bool(env, e, *pos, "in the argument of the not operator")?;
                Ok(Word::from_bool(!b))
            }
            Ir::Neg(e, pos) => {
                let v = self.eval(env, e)?;
                let zero = Word::imm_int(0);
                self.arith(zero, v, ArithOp::Sub, *pos)
            }
            Ir::Eq(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                Ok(Word::from_bool(self.eq_values_top(a, b, *pos)?))
            }
            Ir::Ne(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                Ok(Word::from_bool(!self.eq_values_top(a, b, *pos)?))
            }
            Ir::Lt(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                Ok(Word::from_bool(self.less_than(a, b, *pos)?))
            }
            Ir::Gt(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                Ok(Word::from_bool(self.less_than(b, a, *pos)?))
            }
            Ir::Le(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                Ok(Word::from_bool(!self.less_than(b, a, *pos)?))
            }
            Ir::Ge(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                Ok(Word::from_bool(!self.less_than(a, b, *pos)?))
            }
            Ir::And(l, r, pos) => {
                if !self.eval_bool(env, l, *pos, "in the left operand of the AND (&&) operator")? {
                    return Ok(Word::FALSE);
                }
                let b = self.eval_bool(
                    env,
                    r,
                    *pos,
                    "in the right operand of the AND (&&) operator",
                )?;
                Ok(Word::from_bool(b))
            }
            Ir::Or(l, r, pos) => {
                if self.eval_bool(env, l, *pos, "in the left operand of the OR (||) operator")? {
                    return Ok(Word::TRUE);
                }
                let b =
                    self.eval_bool(env, r, *pos, "in the right operand of the OR (||) operator")?;
                Ok(Word::from_bool(b))
            }
            Ir::Impl(l, r, pos) => {
                if !self.eval_bool(
                    env,
                    l,
                    *pos,
                    "in the left operand of the IMPL (->) operator",
                )? {
                    return Ok(Word::TRUE);
                }
                let b = self.eval_bool(
                    env,
                    r,
                    *pos,
                    "in the right operand of the IMPL (->) operator",
                )?;
                Ok(Word::from_bool(b))
            }
            Ir::Update(l, r, pos) => {
                // the right operand first, as ExprOpUpdate::evalForUpdate
                // (eval.cc): `tryEval (1 // throw "x")` catches the throw
                let b = self.eval(env, r)?;
                let b =
                    self.force_attrs(b, *pos, "in the right operand of the update (//) operator")?;
                let a = self.eval(env, l)?;
                let a =
                    self.force_attrs(a, *pos, "in the left operand of the update (//) operator")?;
                #[cfg(feature = "stats2")]
                let before = (
                    census::upd_path(a, b),
                    a.flags,
                    b.flags,
                    self.arena.bytes_allocated,
                    matches!(r, Ir::Update(..)),
                );
                let out = self.update_attrs(a, b);
                #[cfg(feature = "stats2")]
                self.note_upd(a, b, out, *pos, before);
                Ok(out)
            }
            Ir::ConcatLists(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                let a = self.force_list(
                    a,
                    *pos,
                    "in the left operand of the list concatenation operator",
                )?;
                let b = self.force_list(
                    b,
                    *pos,
                    "in the right operand of the list concatenation operator",
                )?;
                Ok(self.concat_lists(&[a, b]))
            }
            Ir::Sub(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                self.arith(a, b, ArithOp::Sub, *pos)
            }
            Ir::Mul(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                self.arith(a, b, ArithOp::Mul, *pos)
            }
            Ir::Div(l, r, pos) => {
                let a = self.eval(env, l)?;
                let b = self.eval(env, r)?;
                self.arith(a, b, ArithOp::Div, *pos)
            }
        }
    }

    #[inline]
    fn eval_bool(
        &mut self,
        env: &'static EnvObj,
        ir: IrRef,
        pos: Pos,
        ctx: &str,
    ) -> EvalResult<bool> {
        let v = self.eval(env, ir)?;
        match v {
            Word::TRUE => Ok(true),
            Word::FALSE => Ok(false),
            _ => self.type_error("a Boolean", v, pos, ctx),
        }
    }

    fn eval_with_var(
        &mut self,
        env: &'static EnvObj,
        sym: Sym,
        withs: &[u32],
        pos: Pos,
    ) -> EvalResult<Word> {
        let mut e = env;
        let mut walked = 0u32;
        for &d in withs {
            for _ in walked..d {
                e = e.up().expect("compile-time depth is valid");
            }
            walked = d;
            e.mark_read();
            let attrs = self.force_attrs(
                e.slot(0).get(),
                pos,
                "while evaluating the first subexpression of a with expression",
            )?;
            e.slot(0).set(Word::from_ref(attrs));
            if let Some(a) = attrs.get(sym) {
                return self.force_cell(&a.cell);
            }
        }
        if let Some(w) = self.globals.table.get(&sym) {
            return Ok(*w);
        }
        bail!(
            ErrorKind::UndefinedVar,
            pos,
            "undefined variable '{}'",
            self.interner.name_str(sym)
        )
    }

    fn attr_name(&mut self, env: &'static EnvObj, elem: &AttrElemIr) -> EvalResult<Sym> {
        match elem {
            AttrElemIr::Static(s) => Ok(*s),
            AttrElemIr::Dynamic(e) => {
                let pos = ir_pos(e);
                let v = self.eval(env, e)?;
                self.force_sym(v, pos, "while evaluating an attribute name")
            }
        }
    }

    fn eval_select(
        &mut self,
        env: &'static EnvObj,
        subject: IrRef,
        path: &[AttrElemIr],
        default: Option<IrRef>,
        pos: Pos,
    ) -> EvalResult<Word> {
        let mut v = self.eval(env, subject)?;
        for elem in path {
            let name = self.attr_name(env, elem)?;
            let attr = match default {
                Some(d) => match v.as_attrs().and_then(|a| a.get(name)) {
                    Some(a) => a,
                    None => {
                        if v.as_attrs().is_none() {
                            or_non_attrs(self, v, pos)?;
                        }
                        return self.eval(env, d);
                    }
                },
                None => {
                    let attrs = self.force_attrs(v, pos, "while selecting an attribute")?;
                    match attrs.get(name) {
                        Some(a) => a,
                        None => bail!(
                            ErrorKind::MissingAttr,
                            pos,
                            "attribute '{}' missing",
                            self.interner.name_str(name)
                        ),
                    }
                }
            };
            v = self.force_cell(&attr.cell).map_err(|mut e| {
                e.add_trace(
                    attr.pos,
                    format!(
                        "while evaluating the attribute '{}'",
                        self.show_attr_path(env, path)
                    ),
                );
                e
            })?;
        }
        Ok(v)
    }

    /// `eval_select` for `Ir::SelectCount`. It is a function of its own
    /// because one body for both, with the test of the name compiled out of
    /// `eval_select`, cost 0.16 % of the instructions on nixos-server.
    #[cold]
    #[inline(never)]
    fn eval_select_count(&mut self, env: &'static EnvObj, s: &SelectIr) -> EvalResult<Word> {
        let mut v = self.eval(env, s.subject)?;
        for elem in s.path {
            let name = self.attr_name(env, elem)?;
            let Some(a) = v.as_attrs().and_then(|a| a.get(name)) else {
                let Some(d) = s.default else {
                    self.force_attrs(v, s.pos, "while selecting an attribute")?;
                    let name = self.interner.name_str(name);
                    bail!(ErrorKind::MissingAttr, s.pos, "attribute '{name}' missing");
                };
                if v.as_attrs().is_none() {
                    or_non_attrs(self, v, s.pos)?;
                }
                return self.eval(env, d);
            };
            let read = match name == self.s.rev_count {
                true => purity::count_read(self, &a.cell, s.pos),
                false => self.force_cell(&a.cell),
            };
            v = read.map_err(|mut e| {
                let path = self.show_attr_path(env, s.path);
                e.add_trace(a.pos, format!("while evaluating the attribute '{path}'"));
                e
            })?;
        }
        Ok(v)
    }

    /// `eval_select` for the one-static-element case, without the path loop.
    fn eval_select1(
        &mut self,
        env: &'static EnvObj,
        subject: IrRef,
        sym: Sym,
        default: Option<IrRef>,
        pos: Pos,
    ) -> EvalResult<Word> {
        let v = self.eval(env, subject)?;
        let attr = match default {
            Some(d) => match v.as_attrs().and_then(|a| a.get(sym)) {
                Some(a) => a,
                None => {
                    if v.as_attrs().is_none() {
                        or_non_attrs(self, v, pos)?;
                    }
                    return self.eval(env, d);
                }
            },
            None => {
                let attrs = self.force_attrs(v, pos, "while selecting an attribute")?;
                match attrs.get(sym) {
                    Some(a) => a,
                    None => bail!(
                        ErrorKind::MissingAttr,
                        pos,
                        "attribute '{}' missing",
                        self.interner.name_str(sym)
                    ),
                }
            }
        };
        self.force_cell(&attr.cell).map_err(|mut e| {
            e.add_trace(
                attr.pos,
                format!(
                    "while evaluating the attribute '{}'",
                    self.interner.name_str(sym)
                ),
            );
            e
        })
    }

    fn show_attr_path(&mut self, env: &'static EnvObj, path: &[AttrElemIr]) -> String {
        let mut out = String::new();
        for (i, e) in path.iter().enumerate() {
            if i > 0 {
                out.push('.');
            }
            match e {
                AttrElemIr::Static(s) => out.push_str(self.interner.name_str(*s)),
                AttrElemIr::Dynamic(d) => match self.eval(env, d) {
                    Ok(v) if v.as_string().is_some() => {
                        out.push_str(&String::from_utf8_lossy(v.as_string().unwrap().bytes))
                    }
                    _ => out.push_str("\"${...}\""),
                },
            }
        }
        out
    }

    /// Env for `let` and `rec { }`: slots for binds then `from` thunks.
    fn alloc_rec_env(
        &mut self,
        env: &'static EnvObj,
        binds: &'static [BindIr],
        from: &'static [IrRef],
        overrides: bool,
    ) -> &'static EnvObj {
        let env2 = self.arena.alloc_env(Some(env), binds.len() + from.len());
        self.fill_rec_slots(env, env2, 0, binds, from, overrides);
        env2
    }

    /// The binds and `from` thunks of `env2` from slot `base` on; `inherit x`
    /// reads `env`.
    fn fill_rec_slots(
        &mut self,
        env: &'static EnvObj,
        env2: &'static EnvObj,
        base: usize,
        binds: &'static [BindIr],
        from: &'static [IrRef],
        overrides: bool,
    ) {
        let slots = &env2.slots_n(base + binds.len() + from.len())[base..];
        // eval.cc:ExprAttrs::eval takes a real thunk for every binding that
        // is not inherited when `__overrides` is there, so the override
        // reaches the other bindings. The test is out of the loop: the
        // usual set has no `__overrides` and must not pay for one.
        if overrides {
            for (i, b) in binds.iter().enumerate() {
                let w = match b.scope {
                    BindScope::Outer => self.maybe_thunk(env, b.value),
                    _ => self.mk_thunk_counted(env2, b.value),
                };
                slots[i].set(w);
            }
        } else {
            for (i, b) in binds.iter().enumerate() {
                let w = match b.scope {
                    BindScope::Outer => self.maybe_thunk(env, b.value),
                    _ => self.maybe_thunk(env2, b.value),
                };
                slots[i].set(w);
            }
        }
        for (j, f) in from.iter().enumerate() {
            let w = self.maybe_thunk(env2, f);
            slots[binds.len() + j].set(w);
        }
    }

    fn eval_attrs(&mut self, env: &'static EnvObj, a: &'static AttrsIr) -> EvalResult<Word> {
        if a.dynamic.is_empty() && a.overrides.is_none() {
            return Ok(self.eval_static_attrs(env, a));
        }
        let mut entries: Vec<(Sym, Pos, Word)> =
            Vec::with_capacity(a.binds.len() + a.dynamic.len());
        let dyn_env: &'static EnvObj;
        let mut needs_sort = false;
        if a.rec {
            let env2 = self.alloc_rec_env(env, a.binds, a.from, a.overrides.is_some());
            dyn_env = env2;
            for (i, b) in a.binds.iter().enumerate() {
                entries.push((b.sym, b.pos, env2.slot(i).get()));
            }
            if let Some(oi) = a.overrides {
                let ow = env2.slot(oi as usize).get();
                let overrides =
                    self.force_attrs(ow, a.pos, "while evaluating the `__overrides` attribute")?;
                for o in overrides.iter() {
                    match a.binds.binary_search_by(|b| b.sym.cmp(&o.sym)) {
                        Ok(j) => {
                            entries[j].2 = o.cell.get();
                            env2.slot(j).set(o.cell.get());
                        }
                        Err(_) => entries.push((o.sym, o.pos, o.cell.get())),
                    }
                }
                needs_sort = true;
            }
        } else {
            dyn_env = env;
            let inherit_env = self.inherit_env(env, a.from);
            for b in a.binds.iter() {
                let w = match b.scope {
                    BindScope::From => self.maybe_thunk(inherit_env.unwrap(), b.value),
                    _ => self.maybe_thunk(env, b.value),
                };
                entries.push((b.sym, b.pos, w));
            }
        }
        for d in a.dynamic.iter() {
            let nv = self.eval(dyn_env, d.name)?;
            if nv == Word::NULL {
                continue;
            }
            let name = self.force_string_no_ctx(
                nv,
                d.pos,
                "while evaluating the name of a dynamic attribute",
            )?;
            let sym = self.interner.intern(name);
            if entries.iter().any(|e| e.0 == sym) {
                bail!(
                    ErrorKind::Eval,
                    d.pos,
                    "dynamic attribute '{}' already defined",
                    self.interner.name_str(sym)
                );
            }
            let w = self.maybe_thunk(dyn_env, d.value);
            entries.push((sym, d.pos, w));
            needs_sort = true;
        }
        if needs_sort {
            self.mk_attrs(&mut entries, a.pos)
        } else {
            Ok(self.arena.mk_attrs_sorted(&entries))
        }
    }

    /// The common case: static names only, so the binds are already sorted
    /// and the set is filled in place without a temporary vector.
    fn eval_static_attrs(&mut self, env: &'static EnvObj, a: &'static AttrsIr) -> Word {
        let out = self.arena.alloc_attrs_as(a.binds.len(), Stat::AttrsLit);
        if a.rec {
            let env2 = self.alloc_rec_env(env, a.binds, a.from, false);
            for (i, b) in a.binds.iter().enumerate() {
                out.init(i, b.sym, b.pos, env2.slot(i).get());
            }
        } else {
            let inherit_env = self.inherit_env(env, a.from);
            for (i, b) in a.binds.iter().enumerate() {
                let w = match b.scope {
                    BindScope::From => self.maybe_thunk(inherit_env.unwrap(), b.value),
                    _ => self.maybe_thunk(env, b.value),
                };
                out.init(i, b.sym, b.pos, w);
            }
        }
        Word::from_ref(out)
    }

    /// Env holding the `inherit (e)` source thunks of a non-rec set.
    fn inherit_env(
        &mut self,
        env: &'static EnvObj,
        from: &'static [IrRef],
    ) -> Option<&'static EnvObj> {
        if from.is_empty() {
            return None;
        }
        let ie = self.arena.alloc_env(Some(env), from.len());
        for (j, f) in from.iter().enumerate() {
            let w = self.maybe_thunk(env, f);
            ie.slot(j).set(w);
        }
        Some(ie)
    }

    /// Kept out of `eval` so its buffers do not widen every eval frame.
    #[inline(never)]
    fn eval_concat(
        &mut self,
        env: &'static EnvObj,
        force_string: bool,
        parts: &[IrRef],
        pos: Pos,
    ) -> EvalResult<Word> {
        let mut first = !force_string;
        let mut first_type = ValueType::String;
        let mut n: i64 = 0;
        let mut nf: f64 = 0.0;
        // Interpolations have few parts and coerce to borrowed slices: keep
        // them on the stack and spill to a Vec only past the buffer.
        let mut buf: [&'static [u8]; 16] = [&[]; 16];
        let mut nbuf = 0usize;
        let mut spill: Vec<Cow<'static, [u8]>> = Vec::new();
        let mut ctx: Vec<Ctx> = Vec::new();
        for part in parts {
            let v = self.eval(env, part)?;
            let ppos = ir_pos(part);
            // a literal part has no position of its own: the whole concat's stands in
            let ppos = if ppos.is_none() { pos } else { ppos };
            if first {
                first_type = v.value_type();
            }
            match first_type {
                ValueType::Int => match v.view() {
                    Value::Int(i) => match n.checked_add(i) {
                        Some(x) => n = x,
                        None => bail!(
                            ErrorKind::Eval,
                            ppos,
                            "integer overflow in adding {} + {}",
                            n,
                            i
                        ),
                    },
                    Value::Float(f) => {
                        first_type = ValueType::Float;
                        nf = n as f64 + f;
                    }
                    _ => bail!(
                        ErrorKind::Eval,
                        ppos,
                        "cannot add {} to an integer",
                        v.value_type().name()
                    ),
                },
                ValueType::Float => match v.view() {
                    Value::Int(i) => nf += i as f64,
                    Value::Float(f) => nf += f,
                    _ => bail!(
                        ErrorKind::Eval,
                        ppos,
                        "cannot add {} to a float",
                        v.value_type().name()
                    ),
                },
                _ => {
                    let s = self.coerce_to_string(
                        v,
                        &mut ctx,
                        ppos,
                        "while evaluating a path segment",
                        false,
                        first_type == ValueType::String,
                        !first,
                    )?;
                    match s {
                        Cow::Borrowed(b) if spill.is_empty() && nbuf < buf.len() => {
                            buf[nbuf] = b;
                            nbuf += 1;
                        }
                        s => {
                            if spill.is_empty() {
                                spill.extend(buf[..nbuf].iter().map(|b| Cow::Borrowed(*b)));
                                nbuf = 0;
                            }
                            spill.push(s);
                        }
                    }
                }
            }
            first = false;
        }
        let spilled: Vec<&[u8]> = spill.iter().map(|s| s.as_ref()).collect();
        let pieces: &[&[u8]] = if spill.is_empty() {
            &buf[..nbuf]
        } else {
            &spilled
        };
        match first_type {
            ValueType::Int => Ok(self.mk_int(n)),
            ValueType::Float => Ok(self.mk_float(nf)),
            ValueType::Path => {
                if ctx.iter().any(Ctx::is_store) {
                    bail!(
                        ErrorKind::Eval,
                        pos,
                        "a string that refers to a store path cannot be appended to a path"
                    );
                }
                let joined = pieces.concat();
                Ok(self.arena.mk_path(&compile::canon_path(&joined)))
            }
            _ => {
                let i = self.arena.intern_concat(pieces);
                let ctx = ctx_to_arena(&mut self.arena, ctx);
                Ok(self.arena.mk_string_over(i, ctx))
            }
        }
    }

    /// `a // b`. A large left side is not copied: the result is a union
    /// that probes `b` then `a` (docs/explanation/evaluation.md). Small left sides and
    /// unions past the leaf budget are merged flat in two linear passes:
    /// one to size the result, one to fill it.
    pub fn update_attrs(&mut self, a: &'static AttrsObj, b: &'static AttrsObj) -> Word {
        if a.is_empty() {
            return Word::from_ref(b);
        }
        if b.is_empty() {
            return Word::from_ref(a);
        }
        let a_small = a.kind == Kind::Attrs && a.len() < UNION_MIN;
        if !a_small {
            let a = if leaf_count(a) + leaf_count(b) > MAX_LEAVES as usize {
                self.arena.flat(a)
            } else {
                a
            };
            if leaf_count(a) + leaf_count(b) <= MAX_LEAVES as usize {
                return self.arena.alloc_union(a, b);
            }
        }
        let (aa, bb) = (
            self.arena.flat(a).attrs_raw(),
            self.arena.flat(b).attrs_raw(),
        );
        let (mut i, mut j, mut common) = (0, 0, 0);
        while i < aa.len() && j < bb.len() {
            match aa[i].sym.cmp(&bb[j].sym) {
                Ordering::Less => i += 1,
                Ordering::Greater => j += 1,
                Ordering::Equal => {
                    common += 1;
                    i += 1;
                    j += 1;
                }
            }
        }
        let out = self
            .arena
            .alloc_attrs_as(aa.len() + bb.len() - common, Stat::AttrsUpd);
        let (mut i, mut j, mut k) = (0, 0, 0);
        while i < aa.len() && j < bb.len() {
            match aa[i].sym.cmp(&bb[j].sym) {
                Ordering::Less => {
                    out.init_from(k, &aa[i]);
                    i += 1;
                }
                Ordering::Greater => {
                    out.init_from(k, &bb[j]);
                    j += 1;
                }
                Ordering::Equal => {
                    out.init_from(k, &bb[j]);
                    i += 1;
                    j += 1;
                }
            }
            k += 1;
        }
        for x in &aa[i..] {
            out.init_from(k, x);
            k += 1;
        }
        for x in &bb[j..] {
            out.init_from(k, x);
            k += 1;
        }
        Word::from_ref(out)
    }

    /// Census of one `//`. The update flattens union operands, which marks
    /// them iterated; their marks are put back so a use counts once, as
    /// an operand.
    #[cfg(feature = "stats2")]
    fn note_upd(
        &mut self,
        a: &'static AttrsObj,
        b: &'static AttrsObj,
        out: Word,
        pos: Pos,
        (path, fa, fb, bytes, nested): (usize, u8, u8, usize, bool),
    ) {
        a.set_flags(fa);
        b.set_flags(fb);
        let file = self
            .file_of(pos)
            .map(|(f, _)| (f as *const FileInfo as usize, &f.path[..]));
        let bytes = (self.arena.bytes_allocated - bytes) as u64;
        let census = &mut self.arena.stats.census;
        census.upd.nested_right += nested as u64;
        census.note_upd(path, a, b, out, bytes, file);
    }

    pub fn concat_lists(&mut self, lists: &[&'static ListObj]) -> Word {
        let total: usize = lists.iter().map(|l| l.len as usize).sum();
        if let Some(l) = lists.iter().find(|l| l.len as usize == total) {
            return Word::from_ref(*l);
        }
        let out = self.arena.alloc_list_as(total, Stat::ListConcat);
        let cells = out.cells();
        let mut k = 0;
        for l in lists {
            for c in l.cells() {
                cells[k].set(c.get());
                k += 1;
            }
        }
        Word::from_ref(out)
    }

    pub fn arith(&mut self, a: Word, b: Word, op: ArithOp, pos: Pos) -> EvalResult<Word> {
        let a = self.force(a)?;
        let b = self.force(b)?;
        let is_float = matches!(a.view(), Value::Float(_)) || matches!(b.view(), Value::Float(_));
        if is_float {
            let x = self.force_float(a, pos, op.ctx(true))?;
            let y = self.force_float(b, pos, op.ctx(false))?;
            if op == ArithOp::Div && y == 0.0 {
                bail!(ErrorKind::Eval, pos, "division by zero");
            }
            return Ok(self.mk_float(match op {
                ArithOp::Add => x + y,
                ArithOp::Sub => x - y,
                ArithOp::Mul => x * y,
                ArithOp::Div => x / y,
            }));
        }
        let x = self.force_int(a, pos, op.ctx(true))?;
        let y = self.force_int(b, pos, op.ctx(false))?;
        let r = match op {
            ArithOp::Add => x.checked_add(y),
            ArithOp::Sub => x.checked_sub(y),
            ArithOp::Mul => x.checked_mul(y),
            ArithOp::Div => {
                if y == 0 {
                    bail!(ErrorKind::Eval, pos, "division by zero");
                }
                x.checked_div(y)
            }
        };
        match r {
            Some(v) => Ok(self.mk_int(v)),
            None => bail!(
                ErrorKind::Eval,
                pos,
                "integer overflow in {} {} {} {}",
                op.verb(),
                x,
                op.symbol(),
                y
            ),
        }
    }
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl ArithOp {
    fn verb(self) -> &'static str {
        match self {
            ArithOp::Add => "adding",
            ArithOp::Sub => "subtracting",
            ArithOp::Mul => "multiplying",
            ArithOp::Div => "dividing",
        }
    }
    fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
        }
    }
    fn ctx(self, first: bool) -> &'static str {
        match (self, first) {
            (ArithOp::Add, true) => "while evaluating the first argument of the addition",
            (ArithOp::Add, false) => "while evaluating the second argument of the addition",
            (ArithOp::Sub, true) => "while evaluating the first argument of the subtraction",
            (ArithOp::Sub, false) => "while evaluating the second argument of the subtraction",
            (ArithOp::Mul, true) => "while evaluating the first of the multiplication",
            (ArithOp::Mul, false) => "while evaluating the second argument of the multiplication",
            (ArithOp::Div, true) => "while evaluating the first operand of the division",
            (ArithOp::Div, false) => "while evaluating the second operand of the division",
        }
    }
}

/// Best-effort position of an IR node, for error messages.
pub fn ir_pos(ir: &Ir) -> Pos {
    match ir {
        Ir::WithVar { pos, .. }
        | Ir::CurPos(pos)
        | Ir::Concat { pos, .. }
        | Ir::SearchPath { pos, .. }
        | Ir::Select1 { pos, .. }
        | Ir::Apply { pos, .. }
        | Ir::With { pos, .. }
        | Ir::If { pos, .. }
        | Ir::Assert { pos, .. }
        | Ir::Not(_, pos)
        | Ir::Neg(_, pos)
        | Ir::Eq(_, _, pos)
        | Ir::Ne(_, _, pos)
        | Ir::Lt(_, _, pos)
        | Ir::Gt(_, _, pos)
        | Ir::Le(_, _, pos)
        | Ir::Ge(_, _, pos)
        | Ir::And(_, _, pos)
        | Ir::Or(_, _, pos)
        | Ir::Impl(_, _, pos)
        | Ir::Update(_, _, pos)
        | Ir::ConcatLists(_, _, pos)
        | Ir::Sub(_, _, pos)
        | Ir::Mul(_, _, pos)
        | Ir::Div(_, _, pos) => *pos,
        Ir::Attrs(a) => a.pos,
        Ir::Lambda(l) => l.pos,
        Ir::Select(s) | Ir::SelectCount(s) => s.pos,
        Ir::Let(l) => ir_pos(l.body),
        Ir::Const(_) | Ir::Local { .. } | Ir::HasAttr { .. } | Ir::List(_) => Pos::NONE,
    }
}

pub fn dir_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => b"/",
        Some(i) => &path[..i],
        None => b".",
    }
}

impl Evaluator {
    /// `--arg`/`--argstr` handling: call a formals lambda with the matching
    /// subset of `args`; anything else is returned unchanged.
    pub fn auto_call(&mut self, fun: Word, args: Word) -> EvalResult<Word> {
        let fun = self.force(fun)?;
        let Value::Lambda(l) = fun.view() else {
            return Ok(fun);
        };
        let ParamIr::Formals {
            formals, ellipsis, ..
        } = &l.lam.param
        else {
            return Ok(fun);
        };
        let all = self.force(args)?;
        let args = self.force_attrs(all, Pos::NONE, "")?;
        // eval.cc:autoCallFunction: formals that take extra arguments get
        // every one of them, not only the ones they name
        if *ellipsis {
            return self.apply(fun, &[all], Pos::NONE);
        }
        let mut entries: Vec<(Sym, Pos, Word)> = Vec::new();
        for f in formals.iter() {
            match args.get(f.sym) {
                Some(a) => entries.push((f.sym, a.pos, a.cell.get())),
                None if f.default.is_some() => {}
                None => bail!(
                    ErrorKind::Eval,
                    l.lam.pos,
                    "cannot evaluate a function that has an argument without a value ('{}')",
                    self.interner.name_str(f.sym)
                ),
            }
        }
        let a = self.arena.mk_attrs_sorted(&entries);
        self.apply(fun, &[a], Pos::NONE)
    }
}

impl Evaluator {
    /// Thunks, calls, derivations and compile time of the whole run: this
    /// fiber's own counters plus every fiber that folded into the totals.
    /// Read it, not the fields, to report a number.
    pub fn counts(&self) -> Counts {
        let t = self.totals.get();
        Counts {
            thunks: t.thunks + self.nr_thunks,
            calls: t.calls + self.nr_function_calls,
            derivations: t.derivations + self.drv.nr_derivations,
            store_adds: t.store_adds + self.nr_store_adds,
            compile: t.compile + self.t_compile,
        }
    }

    /// Count a store path this evaluation made, and name it on the event
    /// stream: what Nix would `addToStore` or `addTextToStore`. `size` is
    /// the NAR size of a tree, the byte count of a flat add or a text.
    pub fn note_store_add(
        &mut self,
        sp: &StorePath,
        kind: &'static str,
        size: u64,
        from: &SrcPath,
        pos: Pos,
    ) {
        self.nr_store_adds += 1;
        let Some(mut r) = self.session.events.rec(Group::Store, "store-add") else {
            return;
        };
        r.set("path", sp)
            .set("kind", kind)
            .set("size", size)
            .set("from", from)
            .set("at", self.at(pos));
    }

    /// `pos` as a record field, without the `String` [`Self::describe_pos`]
    /// builds.
    pub fn at(&self, pos: Pos) -> At {
        match self.file_of(pos) {
            Some((f, off)) => {
                let (line, col) = f.line_col(off);
                At::Known(&f.path, line, col)
            }
            None => At::None,
        }
    }

    /// Arena use of the whole run: every arena that has folded, plus what
    /// the one this fiber holds has allocated since. Read it, not
    /// `self.arena`, which is this worker's share alone.
    pub fn arena_use(&mut self) -> ArenaUse {
        let mut u = self.totals.arena();
        let mine = self.arena.take_unfolded();
        self.totals.add_arena(&mine);
        u.bytes += mine.bytes;
        u.mapped += mine.mapped;
        for i in 0..arena::NSTATS {
            u.counts[i] += mine.counts[i];
            u.kind_bytes[i] += mine.kind_bytes[i];
        }
        u
    }

    /// Move an arena's new allocation into the run's totals. Only a stats
    /// run reads them, and the scheduler calls this on every fiber switch,
    /// so the flag is the whole reason it costs a run nothing.
    #[inline]
    pub fn fold_arena(&self, arena: &mut arena::Arena) {
        if !self.fold_arenas || !arena.allocated_since_fold() {
            return;
        }
        let u = arena.take_unfolded();
        self.totals.add_arena(&u);
    }

    /// Move this fiber's counters into the run's totals. The scheduler
    /// calls it when a fiber finishes a task, which is why the counters
    /// themselves need no atomics.
    pub fn fold_counters(&mut self) {
        self.totals.add(Counts {
            thunks: self.nr_thunks,
            calls: self.nr_function_calls,
            derivations: self.drv.nr_derivations,
            store_adds: self.nr_store_adds,
            compile: self.t_compile,
        });
        self.nr_thunks = 0;
        self.nr_function_calls = 0;
        self.nr_store_adds = 0;
        self.drv.nr_derivations = 0;
        self.t_compile = Duration::ZERO;
    }

    /// `IETS_STATS=1`: allocation and phase counters on stderr. The arena
    /// figures are the run's, summed over every worker's arena; a fiber
    /// running elsewhere right now has not folded its last task yet.
    pub fn print_stats(&mut self) {
        let a = self.arena_use();
        let c = self.counts();
        errln!(
            "files={} thunks={} calls={} drvs={} store_adds={} arena_bytes={} arena_mapped={} rss_kb={} compile={:?} ir_size={}",
            self.files.len(),
            c.thunks,
            c.calls,
            c.derivations,
            c.store_adds,
            a.bytes,
            a.mapped,
            std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map_or(0, |pages| pages * 4),
            c.compile,
            size_of::<Ir>()
        );
        for (i, name) in STAT_NAMES.iter().enumerate() {
            errln!(
                "{name:>12} count={:>9} bytes={:>11} ({:.1} MiB)",
                a.counts[i],
                a.kind_bytes[i],
                a.kind_bytes[i] as f64 / (1 << 20) as f64
            );
        }
        let mut line_table = 0usize;
        let mut file_paths = 0usize;
        for i in 0..self.files.len() {
            let f = &self.files[i];
            line_table += f.line_starts.capacity() * 4;
            file_paths += f.path.capacity();
        }
        let mut sym_bytes = 0usize;
        for i in 0..self.interner.len() {
            sym_bytes += self.interner.name(Sym(i as u32)).len();
        }
        let (mut drvs, mut aterm_bytes) = (0usize, 0usize);
        for (_, d) in self.drv.drvs.iter() {
            drvs += 1;
            aterm_bytes += d.aterm.as_ref().map_or(0, |a| a.len());
        }
        errln!(
            "non-arena: line_tables={line_table} file_paths={file_paths} symbols={} (name bytes {sym_bytes}, map entries ~{}) import_cache={} drvs={drvs} (aterm bytes {aterm_bytes}) regex_cache={}",
            self.interner.len(),
            self.interner.len() * 32,
            self.import_cache.len(),
            crate::builtins::text::regex_cache_len()
        );
        // SAFETY: a null argument prints to stderr.
        unsafe { libmimalloc_sys::mi_stats_print(ptr::null_mut()) };
        if self.session.settings().stats >= 2 {
            #[cfg(feature = "stats2")]
            {
                // the census is about duplicates inside one arena, so unlike
                // the totals above it stays this worker's own
                let mut out = String::new();
                self.arena.stats.census.report(&mut out);
                self.session.log.err(out.as_bytes());
            }
            #[cfg(not(feature = "stats2"))]
            errln!("census: build with --features stats2");
        }
    }
}
