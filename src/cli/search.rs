//! `nix search` and `iets search`: the package walk of Nix's
//! `src/nix/search.cc` over an index the tracing cache keeps between runs
//! (docs/reference/commands.md).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::api::run::{Outcome, Run};
use crate::api::{EvalArgs, Input, cwd, installable, report};
use crate::builtins::text::regex::{self, Prog};
use crate::cli::classic::nix::{global_flag, nix_settings};
use crate::cli::classic::{Argv, Common, eval_config, fallback, lookup_file_arg};
use crate::cli::help;
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos};
use crate::parallel::force::{T_ATTRS_WHNF, fanout};
use crate::runtime::{AttrsObj, Value, Word};
use crate::trace::run::Cache;

const NAME: &str = "nix";
const BOLD: &[u8] = b"\x1b[0;1m";
const GREEN: &[u8] = b"\x1b[32;1m";
const NORMAL: &[u8] = b"\x1b[0m";

/// One package: what the index keeps, what the output prints, and what the
/// regexes match on.
struct Entry {
    path: Vec<u8>,
    pname: Vec<u8>,
    version: Vec<u8>,
    desc: Vec<u8>,
}

struct Search {
    c: Common,
    json: bool,
    /// `--pretty`/`--no-pretty`; the default is a terminal on stdout.
    pretty: Option<bool>,
    expr: Option<String>,
    file: Option<String>,
    /// The attribute path inside `--expr`/`--file`, else the flake reference.
    what: Option<String>,
    regexes: Vec<String>,
    exclude: Vec<String>,
    native: bool,
    timings: bool,
    /// `--no-tracing-cache`, `--no-ir-cache`, `--no-cache`, `--cache-dir` and
    /// `--workers`, the block every evaluating command shares; `key_args`
    /// carries them on.
    cache: EvalArgs,
}

impl Default for Search {
    fn default() -> Search {
        Search {
            c: Common::default(),
            json: false,
            pretty: None,
            expr: None,
            file: None,
            what: None,
            regexes: Vec::new(),
            exclude: Vec::new(),
            native: false,
            timings: false,
            cache: EvalArgs::new(),
        }
    }
}

/// `nix search INSTALLABLE REGEX...`; `argv` is the whole command line, for
/// the fallback to the real nix.
pub fn classic_main(argv: &[String], rest: &[String]) -> i32 {
    let s = match parse(rest, false) {
        Ok(s) => s,
        Err(why) => fallback(NAME, argv, &why),
    };
    run(s, Some(argv))
}

/// `iets search [INSTALLABLE] REGEX...` (docs/reference/commands.md).
pub fn cmd_search(argv: &[String]) -> i32 {
    let mut s = parse(argv, true).unwrap_or_else(|e| help::arg_error(e));
    // The default installable is `<nixpkgs>`: iets has no flake registry, so
    // `nix search nixpkgs` has nothing to resolve.
    match s.what.take() {
        Some(w) if crate::api::flake::is_flake_ref(&w) => s.what = Some(w),
        Some(w) => s.file = Some(w),
        None if s.expr.is_none() && s.file.is_none() => s.file = Some("<nixpkgs>".into()),
        None => {}
    }
    run(s, None)
}

fn parse(argv: &[String], native: bool) -> Result<Search, String> {
    let mut s = Search {
        native,
        ..Search::default()
    };
    let mut pos: Vec<String> = Vec::new();
    let mut a = Argv { all: argv, i: 0 };
    while a.i < argv.len() {
        let t = argv[a.i].clone();
        match t.as_str() {
            "--json" => s.json = true,
            "--pretty" => s.pretty = Some(true),
            "--no-pretty" => s.pretty = Some(false),
            "-e" | "--exclude" => s.exclude.push(a.value(&t)?),
            "--expr" => s.expr = Some(a.value(&t)?),
            "-f" | "--file" => s.file = Some(a.value(&t)?),
            "--timings" if native => s.timings = true,
            // the cache flags are iets's own, so a classic `nix search`
            // keeps rejecting them and falls back
            _ if native
                && crate::cli::cache_flag(&mut s.cache, &t, || {
                    a.value(&t).unwrap_or_else(|e| help::arg_error(e))
                }) => {}
            _ => {
                if !global_flag(&mut s.c, &mut a, &t)? {
                    if t.starts_with('-') && t != "-" {
                        return Err(format!("'{t}' is not implemented"));
                    }
                    pos.push(t);
                }
            }
        }
        a.i += 1;
    }
    // The first argument is the installable; natively only when it names one,
    // so that `iets search hello` searches nixpkgs.
    let takes = !pos.is_empty()
        && (!native || (s.expr.is_none() && s.file.is_none() && looks_installable(&pos[0])));
    if takes {
        s.what = Some(pos.remove(0));
    }
    s.regexes = pos;
    Ok(s)
}

/// A first argument that names an installable rather than a regex: a flake
/// reference, `<name>`, or a path that is there.
fn looks_installable(w: &str) -> bool {
    crate::api::flake::is_flake_ref(w)
        || (w.starts_with('<') && w.ends_with('>'))
        || std::path::Path::new(w).exists()
}

fn run(mut s: Search, classic: Option<&[String]>) -> i32 {
    if s.regexes.is_empty() {
        help::arg_error(
            "Must provide at least one regex! To match all packages, use 'nix search <installable> ^'.",
        );
    }
    let (res, exclude) =
        match compile_all(&s.regexes).and_then(|r| Ok((r, compile_all(&s.exclude)?))) {
            Ok(r) => r,
            Err(e) => help::arg_error(e),
        };
    let bail = |why: String| -> i32 {
        match classic {
            Some(argv) => fallback(NAME, argv, &why),
            None => crate::error::Error::Unsupported(why).report(),
        }
    };
    let (cfg, _, _) = match nix_settings(&mut s.c, s.file.is_some(), true) {
        Ok(t) => t,
        Err(why) => return bail(why),
    };
    let ec = eval_config(&s.c, &cfg).ir_cache(s.cache.ir_cache());
    // The cache key names the file, so `<nixpkgs>` and a directory become the
    // file an import would read. The lookup has an evaluator of its own: the
    // run builds its evaluator in `go`.
    if let Some(f) = &s.file {
        let mut ev = ec.clone().build();
        match lookup_file_arg(&mut ev, f, &cwd()) {
            Ok(p) => s.file = Some(entry_file(p)),
            Err(e) => return report(&ev, &e),
        }
    }
    let workers = s.cache.workers;
    let s = Arc::new(s);
    let r = Run::on(ec).workers(workers).cached(&key_args(&s), "search");
    let index = match r.hit(|c| {
        c.replay_warnings();
        decode(&c.part(crate::trace::store::Part::Result)?)
    }) {
        Some(index) => index,
        None => {
            let run = Arc::clone(&s);
            match crate::cli::run_reported(r, move |ev, cache| build(ev, &run, cache)) {
                Outcome::Ok(index) => index,
                Outcome::Failed(code) => return code,
                Outcome::Unsupported(why) => return bail(why),
            }
        }
    };
    let (out, results) = match show_results(&index, &res, &exclude, &s) {
        Ok(r) => r,
        Err(e) => return crate::error::Error::Eval(e).report(),
    };
    crate::cli::render::bytes(&out);
    if results == 0 && !s.json {
        return crate::log::Report {
            hint: Some("use fewer or shorter regexes".into()),
            ..crate::log::Report::new("no-results", "no results for the given search term(s)!")
        }
        .emit();
    }
    0
}

fn build(ev: &mut Evaluator, s: &Search, cache: Option<Cache>) -> crate::error::Result<Vec<Entry>> {
    let mut index = Vec::new();
    for (v, mut path) in cursors(ev, s, &cwd())? {
        visit(ev, v, &mut path, true, &mut index, s.native)?;
    }
    if let Some(c) = &cache {
        c.write(ev, &encode(&index), false);
    }
    Ok(index)
}

/// The values the walk starts from, with their attribute paths
/// (`InstallableValue::getCursors`). A flake takes `packages.<system>` and
/// `legacyPackages.<system>`, or the fragment under the same prefixes; an
/// expression or a file takes the value at the attribute path.
fn cursors(ev: &mut Evaluator, s: &Search, cwd: &[u8]) -> EvalResult<Vec<(Word, Vec<Vec<u8>>)>> {
    let attr = s.what.as_deref().unwrap_or("");
    let args = crate::api::auto_args(ev, &s.c.flags.args, cwd)?;
    if s.expr.is_some() || s.file.is_some() {
        let root = match &s.expr {
            Some(e) => ev.eval_string(e, cwd)?,
            None => ev.eval_file(
                &crate::argv::bytes(s.file.as_deref().unwrap_or("")),
                Pos::NONE,
            )?,
        };
        // Natively the root is auto-called, so a function like nixpkgs'
        // `default.nix` is a package set (docs/reference/commands.md).
        let root = if s.native {
            ev.auto_call(root, args)?
        } else {
            root
        };
        return Ok(vec![(
            installable::find_along_attr_path(ev, root, attr, args)?,
            Vec::new(),
        )]);
    }
    let (loc, fragment) = attr.split_once('#').unwrap_or((attr, ""));
    let fr = crate::api::flake_ref(loc, cwd)
        .map_err(|e| Error::boxed(ErrorKind::Unsupported, e.to_string(), Pos::NONE))?;
    let outputs = crate::api::flake::outputs(ev, &fr)?;
    let found = installable::from_outputs(
        ev,
        outputs,
        loc,
        installable::Prefixes::PackageSet,
        fragment,
        installable::Want::All,
    )?;
    Ok((found.into_iter())
        .map(|f| {
            (
                f.value,
                f.path.iter().map(|p| p.as_bytes().to_vec()).collect(),
            )
        })
        .collect())
}

/// search.cc:visit for one attribute path. `initial` marks a value the
/// installable named. An evaluation error under `legacyPackages` is skipped,
/// as in Nix; anywhere else it stops the search. `lenient` (the native
/// command) treats every path as `legacyPackages` does, so that a package set
/// that is not a flake output is searched whole (docs/reference/commands.md).
fn visit(
    ev: &mut Evaluator,
    w: Word,
    path: &mut Vec<Vec<u8>>,
    initial: bool,
    out: &mut Vec<Entry>,
    lenient: bool,
) -> EvalResult<()> {
    let skips_errors = lenient || path.first().is_some_and(|p| p == b"legacyPackages");
    let r = (|| -> EvalResult<()> {
        let attrs = ev.force(w)?.as_attrs();
        if let Some(a) = attrs {
            let drv = match a.get(ev.s.type_) {
                Some(t) => string_of(ev, t.cell.get(), path, "type")? == b"derivation",
                None => false,
            };
            if drv {
                out.push(entry(ev, a, path)?);
                return Ok(());
            }
        }
        if !recurses(ev, path, initial, attrs, lenient)? {
            return Ok(());
        }
        let Some(a) = attrs else {
            return Err(Error::boxed(
                ErrorKind::Type,
                format!("'{}' is not an attribute set", show(path)),
                Pos::NONE,
            ));
        };
        // parallel: the helpers force the children while the walk reads them
        let a = ev.arena.flat(a);
        fanout(ev, T_ATTRS_WHNF, Word::from_ref(a), a.len());
        for attr in crate::print::lexicographic_order(ev, a) {
            path.push(ev.interner.name(attr.sym).to_vec());
            let r = visit(ev, attr.cell.get(), path, false, out, lenient);
            path.pop();
            r?;
        }
        Ok(())
    })();
    match r {
        Err(e) if skips_errors && swallowed(&e) => Ok(()),
        r => r,
    }
}

/// eval-error.hh: the errors `catch (EvalError &)` in search.cc swallows.
fn swallowed(e: &Error) -> bool {
    use ErrorKind as K;
    matches!(
        e.kind,
        K::Eval
            | K::Type
            | K::Throw
            | K::Assert
            | K::Abort
            | K::UndefinedVar
            | K::InfiniteRecursion
            | K::MissingAttr
    )
}

/// Whether the walk descends into an attribute set: everything the
/// installable named, the two levels under `packages` and `legacyPackages`,
/// and under `legacyPackages` (or anywhere, when `lenient`) a set with
/// `recurseForDerivations = true`.
fn recurses(
    ev: &mut Evaluator,
    path: &[Vec<u8>],
    initial: bool,
    attrs: Option<&'static AttrsObj>,
    lenient: bool,
) -> EvalResult<bool> {
    let head = |n: &[u8]| path.first().is_some_and(|p| p == n);
    let two = head(b"legacyPackages") || head(b"packages");
    if path.is_empty() || initial || (two && path.len() <= 2) {
        return Ok(true);
    }
    if (lenient || head(b"legacyPackages"))
        && let Some(a) = attrs
        && let Some(r) = a.get(ev.sym(b"recurseForDerivations"))
    {
        return ev.force_bool(
            r.cell.get(),
            r.pos,
            "while evaluating recurseForDerivations",
        );
    }
    Ok(false)
}

/// One derivation's index entry: `name` split into pname and version, and
/// `meta.description` with its newlines turned into spaces.
fn entry(ev: &mut Evaluator, a: &'static AttrsObj, path: &[Vec<u8>]) -> EvalResult<Entry> {
    let Some(n) = a.get(ev.s.name) else {
        return Err(Error::boxed(
            ErrorKind::MissingAttr,
            format!("attribute '{}.name' does not exist", show(path)),
            Pos::NONE,
        ));
    };
    let name = string_of(ev, n.cell.get(), path, "name")?;
    let mut desc = Vec::new();
    if let Some(m) = a.get(ev.sym(b"meta")) {
        let meta = ev.force(m.cell.get())?;
        if let Some(ma) = meta.as_attrs()
            && let Some(d) = ma.get(ev.sym(b"description"))
        {
            desc = string_of(ev, d.cell.get(), path, "meta.description")?;
        }
    }
    desc.iter_mut()
        .filter(|b| **b == b'\n')
        .for_each(|b| *b = b' ');
    let (pname, version) = drv_name(&name);
    Ok(Entry {
        path: path.join(&b'.'),
        pname,
        version,
        desc,
    })
}

/// eval-cache.cc:getString: a string or a path, anything else is an error.
fn string_of(ev: &mut Evaluator, w: Word, path: &[Vec<u8>], what: &str) -> EvalResult<Vec<u8>> {
    let v = ev.force(w)?;
    match v.view() {
        Value::String(s) => Ok(s.bytes.to_vec()),
        Value::Path(p) => Ok(p.path.to_vec()),
        _ => Err(Error::boxed(
            ErrorKind::Type,
            format!(
                "'{}.{what}' is not a string but {}",
                show(path),
                v.value_type().name()
            ),
            Pos::NONE,
        )),
    }
}

/// names.cc:DrvName: the name runs up to the first `-` that is not followed
/// by a letter, the version is the rest.
fn drv_name(s: &[u8]) -> (Vec<u8>, Vec<u8>) {
    match (0..s.len())
        .find(|&i| s[i] == b'-' && s.get(i + 1).is_some_and(|c| !c.is_ascii_alphabetic()))
    {
        Some(i) => (s[..i].to_vec(), s[i + 1..].to_vec()),
        None => (s.to_vec(), Vec::new()),
    }
}

fn show(path: &[Vec<u8>]) -> String {
    String::from_utf8_lossy(&path.join(&b'.')).into_owned()
}

/// The file `import` would read: a directory resolves to its `default.nix`.
fn entry_file(p: Vec<u8>) -> String {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut p = p;
    if std::path::Path::new(std::ffi::OsStr::from_bytes(&p)).is_dir() {
        p.extend_from_slice(b"/default.nix");
    }
    crate::argv::encode(std::ffi::OsString::from_vec(p))
}

fn compile_all(res: &[String]) -> Result<Vec<Prog>, String> {
    res.iter()
        .map(|r| {
            regex::compile_opt(&crate::argv::bytes(r), true)
                .map_err(|_| format!("invalid regular expression '{r}'"))
        })
        .collect()
}

type Spans = Vec<(usize, usize)>;

/// search.cc's match: every regex must match the attribute path, the pname or
/// the description; an exclude regex that matches any of the three hides the
/// package. The spans are what the text output highlights.
fn matched(e: &Entry, res: &[Prog], exclude: &[Prog]) -> Option<(Spans, Spans)> {
    let hit = |p: &Prog, s: &[u8]| regex::matches(p, s).next().is_some();
    if exclude
        .iter()
        .any(|p| hit(p, &e.path) || hit(p, &e.pname) || hit(p, &e.desc))
    {
        return None;
    }
    let (mut paths, mut descs) = (Vec::new(), Vec::new());
    for p in res {
        let before = (paths.len(), descs.len());
        paths.extend(regex::matches(p, &e.path).map(|m| (m.start, m.end)));
        descs.extend(regex::matches(p, &e.desc).map(|m| (m.start, m.end)));
        if (paths.len(), descs.len()) == before && !hit(p, &e.pname) {
            return None;
        }
    }
    Some((paths, descs))
}

/// hilite.cc:hiliteMatches: every match wrapped, continuous ones merged.
fn hilite(s: &[u8], mut spans: Spans, prefix: &[u8], postfix: &[u8]) -> Vec<u8> {
    if spans.is_empty() {
        return s.to_vec();
    }
    spans.sort_by_key(|m| m.0);
    let (mut out, mut last, mut i) = (Vec::new(), 0, 0);
    while i < spans.len() {
        let (start, mut end) = spans[i];
        out.extend_from_slice(&s[last..start]);
        i += 1;
        while i < spans.len() && spans[i].0 <= end {
            end = end.max(spans[i].1);
            i += 1;
        }
        out.extend_from_slice(prefix);
        out.extend_from_slice(&s[start..end]);
        out.extend_from_slice(postfix);
        last = end;
    }
    out.extend_from_slice(&s[last..]);
    out
}

/// The whole output and the number of results.
fn show_results(
    index: &[Entry],
    res: &[Prog],
    exclude: &[Prog],
    s: &Search,
) -> Result<(Vec<u8>, u64), Box<Error>> {
    let mut out = Vec::new();
    let mut json: BTreeMap<&[u8], &Entry> = BTreeMap::new();
    let mut results = 0;
    for e in index {
        let Some((paths, descs)) = matched(e, res, exclude) else {
            continue;
        };
        results += 1;
        if s.json {
            json.insert(&e.path, e);
            continue;
        }
        if results > 1 {
            out.push(b'\n');
        }
        out.extend_from_slice(b"* ");
        out.extend_from_slice(BOLD);
        out.extend_from_slice(&hilite(&e.path, paths, GREEN, BOLD));
        out.extend_from_slice(NORMAL);
        if !e.version.is_empty() {
            out.extend_from_slice(b" (");
            out.extend_from_slice(&e.version);
            out.push(b')');
        }
        out.push(b'\n');
        if !e.desc.is_empty() {
            out.extend_from_slice(b"  ");
            out.extend_from_slice(&hilite(&e.desc, descs, GREEN, NORMAL));
            out.push(b'\n');
        }
    }
    if s.json {
        let pretty = s.pretty.unwrap_or_else(crate::cli::render::pretty);
        out = crate::cli::render::json(&json_out(&json)?, pretty);
    }
    Ok((out, results))
}

/// The object `--json` prints: one entry per package, keyed by attribute
/// path. A string that is not UTF-8 is an error, as `nlohmann::json` makes
/// it.
fn json_out(m: &BTreeMap<&[u8], &Entry>) -> Result<serde_json::Value, Box<Error>> {
    fn text(b: &[u8]) -> Result<&str, Box<Error>> {
        std::str::from_utf8(b).map_err(|_| {
            Error::boxed(
                ErrorKind::Eval,
                "JSON serialization error: invalid UTF-8 byte in string",
                Pos::NONE,
            )
        })
    }
    let mut obj = serde_json::Map::new();
    for (path, e) in m {
        obj.insert(
            text(path)?.to_string(),
            serde_json::json!({
                "description": text(&e.desc)?,
                "pname": text(&e.pname)?,
                "version": text(&e.version)?,
            }),
        );
    }
    Ok(serde_json::Value::Object(obj))
}

/// The arguments the cache key is made of: the installable, the `--arg`s, the
/// search path and pure evaluation. The regexes are not in it, so a second
/// search over the same package set answers from the index.
fn key_args(s: &Search) -> EvalArgs {
    let mut a = EvalArgs::new();
    a.input = match (&s.expr, &s.file) {
        (Some(e), _) => Some(Input::Expr(e.clone())),
        (_, Some(f)) => Some(Input::File(f.clone())),
        _ => s.what.clone().map(Input::Flake),
    };
    if s.expr.is_some() || s.file.is_some() {
        a.attr = s.what.clone();
    }
    // the native walk recurses and skips errors everywhere, so its index is
    // another index; `strict` is the key field that is free here
    a.strict = s.native;
    a.args =
        s.c.flags
            .args
            .iter()
            .map(|(n, k)| (n.clone(), k.clone()))
            .collect();
    a.includes = s.c.flags.includes.clone();
    a.pure = s.c.flags.pure;
    a.timings = s.timings;
    a.no_tracing_cache = s.cache.no_tracing_cache;
    a.no_ir_cache = s.cache.no_ir_cache;
    a.cache_dir = s.cache.cache_dir.clone();
    a.workers = s.cache.workers;
    a
}

fn encode(index: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in index {
        for f in [&e.path, &e.pname, &e.version, &e.desc] {
            out.extend_from_slice(&(f.len() as u32).to_le_bytes());
            out.extend_from_slice(f);
        }
    }
    out
}

fn decode(b: &[u8]) -> Option<Vec<Entry>> {
    let mut index = Vec::new();
    let mut i = 0;
    let field = |i: &mut usize| -> Option<Vec<u8>> {
        let n = u32::from_le_bytes(b.get(*i..*i + 4)?.try_into().ok()?) as usize;
        let v = b.get(*i + 4..*i + 4 + n)?.to_vec();
        *i += 4 + n;
        Some(v)
    };
    while i < b.len() {
        index.push(Entry {
            path: field(&mut i)?,
            pname: field(&mut i)?,
            version: field(&mut i)?,
            desc: field(&mut i)?,
        });
    }
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drv_names() {
        let split = |s: &str| {
            let (p, v) = drv_name(s.as_bytes());
            (String::from_utf8(p).unwrap(), String::from_utf8(v).unwrap())
        };
        assert_eq!(split("hello-2.12"), ("hello".into(), "2.12".into()));
        assert_eq!(
            split("apache-httpd-2.0.48"),
            ("apache-httpd".into(), "2.0.48".into())
        );
        assert_eq!(split("gtk-doc"), ("gtk-doc".into(), "".into()));
        assert_eq!(split("hello"), ("hello".into(), "".into()));
    }

    #[test]
    fn merged_highlights() {
        let out = hilite(b"goodbye", vec![(0, 4), (4, 7)], b"<", b">");
        assert_eq!(out, b"<goodbye>");
        let out = hilite(b"abcd", vec![(2, 3), (0, 1)], b"<", b">");
        assert_eq!(out, b"<a>b<c>d");
    }

    #[test]
    fn index_round_trip() {
        let index = vec![
            Entry {
                path: b"a.b".to_vec(),
                pname: b"b".to_vec(),
                version: Vec::new(),
                desc: b"x y".to_vec(),
            },
            Entry {
                path: b"c".to_vec(),
                pname: b"c".to_vec(),
                version: b"1".to_vec(),
                desc: Vec::new(),
            },
        ];
        let back = decode(&encode(&index)).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].path, b"c");
        assert_eq!(back[0].desc, b"x y");
        assert!(decode(&encode(&index)[..7]).is_none());
    }
}
