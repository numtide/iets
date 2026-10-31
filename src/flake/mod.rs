//! Locked flakes: `PATH#attr.path` on the command line and
//! `builtins.getFlake`, evaluated through
//! Nix 2.34.8's `call-flake.nix` (embedded verbatim) with `fetchTree` for
//! the locked inputs and the root tree copied like Nix copies it. Only a
//! complete `flake.lock` is accepted, or none for a flake without inputs;
//! nothing is locked or looked up in a registry. See
//! docs/reference/flakes.md.

pub mod flakeref;

use std::path::{Path, PathBuf};
use std::{fs, io, mem};

use crate::builtins::fetch::{self, fetch_tree};
use crate::builtins::store_hooks::locate;
use crate::compile;
use crate::compile::ir::ParamIr;
use crate::error::{Error, Result};
use crate::eval::{Error as EvalError, ErrorKind, EvalResult, Evaluator, Pos};
use crate::ignored;
use crate::runtime::{AttrsObj, Value, Word};
use crate::source::lock::Declared;
use crate::source::{self, AttrVal, SourceRef, lock};
use crate::store::path::StorePath;
use crate::sys;

pub const CALL_FLAKE: &str = include_str!("call-flake.nix");
pub const LOCK_ERROR: &str = "flake.lock missing or incomplete, run nix flake lock";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlakeRef {
    /// The directory holding `flake.nix` and `flake.lock`.
    pub dir: PathBuf,
    /// Where its tree comes from (flakeref.cc: a directory inside a
    /// working tree is `git+file://`, everything else is `path:`).
    pub source: SourceRef,
    /// The attribute path after `#`, selected from the flake's outputs.
    pub attr: Vec<String>,
    /// `source` came from a probe for a working tree, not from an explicit
    /// `path:`, so the evaluation records what the probe saw.
    pub probed: bool,
    /// `--reference-lock-file PATH`, read in place of `dir/flake.lock`.
    pub lock: Option<PathBuf>,
}

/// A command-line input that names a flake rather than a `.nix` file: it
/// has a `#`, a `path:` prefix, or is a directory with `flake.nix` and no
/// `default.nix`.
pub fn is_flake_ref(arg: &str) -> bool {
    if arg.contains('#') || arg.starts_with("path:") {
        return true;
    }
    let p = Path::new(arg);
    p.join("flake.nix").is_file() && !p.join("default.nix").exists()
}

/// flakeref.cc:parseFlakeIdRef: `ID[/REF-OR-REV]`, with ID
/// `[a-zA-Z][a-zA-Z0-9_-]*`, is a registry lookup even where a directory of
/// that name exists; a path needs `./`.
fn is_indirect(loc: &str) -> bool {
    let loc = loc.strip_prefix("flake:").unwrap_or(loc);
    let (id, rest) = loc
        .split_once('/')
        .map_or((loc, None), |(i, r)| (i, Some(r)));
    let mut id = id.chars();
    id.next().is_some_and(|c| c.is_ascii_alphabetic())
        && id.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && rest.is_none_or(|r| {
            r.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '@')
                && r.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_./@+-".contains(c))
        })
}

/// `PATH[#attr.path]`, parsed by `flakeref::parse` with cwd as the base
/// directory. Only a local path is taken: `.`, `./x`, `/abs`, `~/x` or
/// `path:…`.
pub fn parse_flake_ref(arg: &str, cwd: &[u8], home: Option<&[u8]>) -> Result<FlakeRef> {
    // `C:\x#a` is a path on Windows, and `flakeref` would read `C` as a scheme
    let (head, fragment) = arg
        .split_once('#')
        .map_or((arg, None), |(h, f)| (h, Some(f)));
    let native = String::from_utf8_lossy(&sys::from_native(head.as_bytes())).into_owned();
    let arg = &match fragment {
        Some(f) => format!("{native}#{f}"),
        None => native,
    };
    let p = flakeref::parse(arg)?;
    let loc = arg.split('#').next().unwrap_or(arg);
    let ty = source::str_attr(&p.input, "type");
    // A checkout an SCM names: CI pins the tree it evaluates as
    // `git+file:///path?ref=HEAD&rev=SHA`, which this process reads out of
    // the repository beside it (docs/reference/flakes.md).
    if let Some(scm) = ty.filter(|_| !p.bare).and_then(source::scm::named) {
        return scm_flake(&p, scm, loc, parse_attr_path(&p.fragment)?);
    }
    let why = match ty {
        Some("path") => None,
        Some("indirect") => Some(format!(
            "flake registry references ({loc}#…) are not supported; use a path or a locked input"
        )),
        ty => Some(format!(
            "flake reference '{loc}' is a '{}' input; only local paths are supported (iets cannot fetch a root flake)",
            ty.unwrap_or("")
        )),
    };
    if let Some(why) = why {
        return Err(Error::Unsupported(why));
    }
    let submodules =
        p.bare && p.input.len() == 3 && source::str_attr(&p.input, "submodules") == Some("1");
    if (p.input.len() > 2 && !submodules) || !p.dir.is_empty() {
        return Err(Error::Unsupported(format!(
            "query parameters in flake reference '{arg}' are not supported"
        )));
    }
    let loc = source::str_attr(&p.input, "path").unwrap_or("");
    let full = match loc.strip_prefix('~') {
        Some(rest) if p.bare && (rest.is_empty() || rest.starts_with('/')) => {
            let Some(h) = home else {
                return Err(Error::Flake("iets: $HOME is not set".to_string()));
            };
            [h, rest.as_bytes()].concat()
        }
        _ if loc.starts_with('/') => loc.as_bytes().to_vec(),
        _ => [cwd, b"/", loc.as_bytes()].concat(),
    };
    let mut fr = local_flake(&full, !p.bare, parse_attr_path(&p.fragment)?)?;
    if submodules {
        fr.source = match &fr.source {
            SourceRef::Tracked { scm, root, subdir } if scm.name() == "git" => {
                SourceRef::Submodules {
                    root: root.clone(),
                    subdir: subdir.clone(),
                }
            }
            _ => {
                return Err(Error::Unsupported(
                    "'submodules=1' needs a local Git checkout".into(),
                ));
            }
        };
    }
    Ok(fr)
}

/// `git+file:///path[?ref=…&rev=…][&dir=…]`: the flake in `dir` of that
/// tree. With a `rev` the source is that revision, read out of the
/// repository; without one it is the working tree, as Nix reads it. A `ref`
/// beside a `rev` is advisory, as it is for Nix: the rev names the tree. The
/// `flake.lock` is the checked-out one either way
/// (docs/reference/divergences.md).
fn scm_flake(
    p: &flakeref::Parsed,
    scm: &'static dyn source::scm::Scm,
    loc: &str,
    attr: Vec<String>,
) -> Result<FlakeRef> {
    const KNOWN: [&str; 4] = ["type", "url", "ref", "rev"];
    if let Some(k) = p.input.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(Error::Unsupported(format!(
            "'{k}' in flake reference '{loc}' is not supported"
        )));
    }
    let url = source::str_attr(&p.input, "url").unwrap_or("");
    let Some(root) = flakeref::file_url_path(url).filter(|r| r.starts_with('/')) else {
        return Err(Error::Unsupported(format!(
            "flake reference '{loc}' is a '{}' input; only local paths are supported (iets cannot fetch a root flake)",
            scm.name()
        )));
    };
    let root = sys::path_from_internal(root.as_bytes());
    let dir = root.join(&p.dir);
    let mut fr = local_flake(&sys::path_to_internal(&dir), false, attr)?;
    let Some(rev) = source::str_attr(&p.input, "rev") else {
        if p.input.contains_key("ref") {
            return Err(Error::Unsupported(format!(
                "flake reference '{loc}' names a '{}' but no 'rev'; iets resolves no reference of a root flake, so name the rev or the directory",
                scm.name()
            )));
        }
        return Ok(fr);
    };
    if !flakeref::is_rev(rev) {
        return Err(Error::Flake(format!(
            "flake reference '{loc}': 'rev={rev}' is not a revision; name the 40 hex digits of a commit"
        )));
    }
    fr.source = source::SourceRef::Commit {
        scm,
        root,
        rev: rev.to_string(),
        subdir: p.dir.clone(),
    };
    fr.probed = false;
    Ok(fr)
}

/// The flake in directory `full`: a `path:` input, or unless `explicit_path`
/// the input of the working tree holding it (`git+file:`).
fn local_flake(full: &[u8], explicit_path: bool, attr: Vec<String>) -> Result<FlakeRef> {
    let full = compile::canon_path(full);
    let dir = sys::path_from_internal(&full);
    if !dir.is_dir() {
        return Err(Error::Flake(format!(
            "path '{}' is not a flake (because it's not a directory)",
            dir.display()
        )));
    }
    if !dir.join("flake.nix").is_file() {
        return Err(Error::Flake(format!(
            "path '{}' is not a flake (because it doesn't contain a 'flake.nix' file)",
            dir.display()
        )));
    }
    let source = if explicit_path {
        SourceRef::Path(dir.clone())
    } else {
        // git.cc opens the repository, which resolves the directory: a link to
        // a working tree is that tree, not a copy of the link object
        let real = sys::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        source::scm::detect(&real).map_or_else(
            || SourceRef::Path(dir.clone()),
            |(scm, root)| SourceRef::Tracked {
                scm,
                subdir: real
                    .strip_prefix(&root)
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                root,
            },
        )
    };
    Ok(FlakeRef {
        dir,
        source,
        attr,
        probed: !explicit_path,
        lock: None,
    })
}

/// attr-path.cc:parseAttrPath: `.` separates, `"..."` quotes.
pub fn parse_attr_path(s: &str) -> Result<Vec<String>> {
    let mut res = Vec::new();
    let mut cur = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '.' => res.push(mem::take(&mut cur)),
            '"' => loop {
                match it.next() {
                    None => {
                        return Err(Error::Flake(format!(
                            "missing closing quote in selection path '{s}'"
                        )));
                    }
                    Some('"') => break,
                    Some(c) => cur.push(c),
                }
            },
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        res.push(cur);
    }
    Ok(res)
}

fn read_traced(ev: &Evaluator, p: &Path) -> io::Result<Vec<u8>> {
    let meta = fs::metadata(p)?;
    let bytes = fs::read(p)?;
    ev.trace().file(
        <std::ffi::OsStr as crate::sys::OsStrExt>::as_bytes(p.as_os_str()),
        &meta,
        &bytes,
    );
    Ok(bytes)
}

/// Evaluates the top-level flake: the `callFlake` result (`_type`, `inputs`,
/// `outputs`, `sourceInfo` and its fields), as `nix-eval-jobs --flake DIR`
/// walks it.
pub fn eval_flake_value(ev: &mut Evaluator, fr: &FlakeRef) -> EvalResult<Word> {
    load_flake(ev, fr, true)
}

/// `top`: the flake a command names, whose `nixConfig` Nix would apply;
/// `builtins.getFlake` loads one that is not.
fn load_flake(ev: &mut Evaluator, fr: &FlakeRef, top: bool) -> EvalResult<Word> {
    let pos = Pos::NONE;
    let lock_path = fr.lock.clone().unwrap_or_else(|| fr.dir.join("flake.lock"));
    let lock_bytes = match read_traced(ev, &lock_path) {
        Ok(b) => b,
        // flake.cc locks a flake without inputs to an empty lock and writes
        // no file; one that declares inputs fails check_declared_inputs
        Err(e) if e.kind() == io::ErrorKind::NotFound && fr.lock.is_none() => {
            ev.trace()
                .stat_now(lock_path.as_os_str().as_encoded_bytes(), true);
            lock::EMPTY.as_bytes().to_vec()
        }
        Err(_) => {
            let msg = match &fr.lock {
                Some(p) => format!("cannot read the lock file '{}'", p.display()),
                None => LOCK_ERROR.to_string(),
            };
            return Err(EvalError::boxed(ErrorKind::Unsupported, msg, pos));
        }
    };
    let lock = lock::check(&lock_bytes).map_err(|why| {
        let mut e = EvalError::boxed(ErrorKind::Unsupported, format!("{LOCK_ERROR} ({why})"), pos);
        e.hint = why.to_report().hint;
        e
    })?;
    for (hash, name) in lock.nar_hash_names() {
        ev.input_names.insert(hash.to_string(), name.to_string());
    }

    if fr.probed {
        let found = match &fr.source {
            SourceRef::Tracked { root, .. } | SourceRef::Submodules { root, .. } => {
                Some(root.as_path())
            }
            _ => None,
        };
        source::scm::record_detect(ev.trace(), &fr.dir, found);
    }
    let root = source::tree(ev, &fr.source, pos)?;
    let source_info = fetch::emit_tree_attrs(ev, &root, Some("self"), pos)?;
    let subdir = root.subdir;
    let out_path = {
        let ctx = format!(
            "while evaluating the sourceInfo of flake '{}'",
            fr.dir.display()
        );
        let a = ev.force_attrs(source_info, pos, &ctx)?;
        let Some(attr) = a.get(ev.s.out_path) else {
            return Err(EvalError::boxed(
                ErrorKind::MissingAttr,
                format!(
                    "flake '{}': sourceInfo has no attribute 'outPath'",
                    fr.dir.display()
                ),
                pos,
            ));
        };
        ev.force_string(attr.cell.get(), pos, "while evaluating the flake's outPath")?
            .bytes
            .to_vec()
    };
    if let Ok(sp) = StorePath::parse(&String::from_utf8_lossy(&out_path)) {
        ev.known.name_flake(&sp, lock.root.clone());
    }
    let mut flake_dir = out_path.clone();
    if !subdir.is_empty() {
        flake_dir.push(b'/');
        flake_dir.extend_from_slice(subdir.as_bytes());
    }
    let has_nix_config = check_declared_inputs(ev, &flake_dir, &lock, pos)?;
    if top && has_nix_config {
        ignored::nix_config_seen();
    }

    let ir = ev.compile_internal(b"\xc2\xabcall-flake\xc2\xbb", CALL_FLAKE.as_bytes())?;
    let root_env = ev.root_env;
    let call_flake = ev.eval(root_env, ir)?;
    let lock_str = ev.mk_string(&lock_bytes);
    let dir_w = ev.mk_string(subdir.as_bytes());
    let mut ov = vec![
        (ev.sym(b"sourceInfo"), pos, source_info),
        (ev.sym(b"dir"), pos, dir_w),
    ];
    let ov = ev.mk_attrs(&mut ov, pos)?;
    let mut overrides = vec![(ev.sym(lock.root.as_bytes()), pos, ov)];
    let overrides = ev.mk_attrs(&mut overrides, pos)?;
    // its own primop, not the one in `builtins`: in classic mode that
    // one is there only with `fetch-tree` or `flakes` (`builtins::exposed`),
    // and `call-flake.nix` needs it either way
    let fetch_sym = ev.sym(b"fetchTree");
    let fetch_tree = ev.arena.mk_primop("fetchTree", 1, fetch_tree, fetch_sym);
    ev.apply(call_flake, &[lock_str, overrides, fetch_tree], pos)
}

/// flake-primops.cc:prim_getFlake on a local path, parsed by
/// `flakeref::parse_absolute`. Impure, or pure on a `path:` reference with a
/// `narHash` the tree matches; a pure evaluation refuses every other one.
/// Nothing is locked here. A directory is always a `path:` input, even
/// inside a git repository: flakeref.cc looks for `.git` only when it has a
/// base directory, and getFlake passes none. A virtual copy stands for its
/// local tree.
pub fn get_flake(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.getFlake",
    )?;
    let s = String::from_utf8_lossy(s).into_owned();
    let p = flakeref::parse_absolute(&s).map_err(|e| eval_error(e, pos))?;
    let unsupported = |why: &str| {
        let msg = format!("builtins.getFlake on '{s}' {why} is not supported");
        EvalError::boxed(ErrorKind::Unsupported, msg, pos)
    };
    let pinned = pinned_path(&p);
    if let Some((path, expected)) = pinned {
        let expected = source::parse_nar_hash(expected).map_err(|e| eval_error(e, pos))?;
        if let Err(got) = fetch::path_nar_hash(ev, path, Some(&expected), pos)? {
            let msg = format!(
                "NAR hash mismatch in input '{s}', expected '{}' but got '{}'",
                expected.to_sri(),
                got.to_sri()
            );
            return Err(EvalError::boxed(ErrorKind::Eval, msg, pos));
        }
    } else if ev.pure() {
        // a rev may lock it; the real nix decides that
        if p.input.contains_key("rev") || p.input.contains_key("narHash") {
            return Err(unsupported("in pure evaluation mode"));
        }
        // flake-primops.cc prints the primop's position, which it never has
        return Err(EvalError::boxed(
            ErrorKind::Eval,
            format!(
                "cannot call 'getFlake' on unlocked flake reference '{s}', at «none»:0 (use --impure to override)"
            ),
            pos,
        ));
    }
    let local = (p.input.len() == 2 || pinned.is_some()) && p.dir.is_empty();
    let Some(path) = source::str_attr(&p.input, "path").filter(|_| local) else {
        return Err(unsupported("(only a local path)"));
    };
    let full = match locate(ev, path.as_bytes()) {
        Some(at) if !at.hidden() => match at.local() {
            Some(local) => sys::path_to_internal(&local),
            None => return Err(unsupported("(a fetched input)")),
        },
        _ => path.as_bytes().to_vec(),
    };
    let fr = local_flake(&full, true, Vec::new()).map_err(|e| eval_error(e, pos))?;
    load_flake(ev, &fr, false)
}

/// `path:DIR?narHash=H` and nothing else: a reference its NAR hash locks, so
/// a pure evaluation may take it once the tree matches.
fn pinned_path(p: &flakeref::Parsed) -> Option<(&str, &str)> {
    let only = p.input.len() == 3 && p.dir.is_empty();
    let path = source::str_attr(&p.input, "path").filter(|_| only)?;
    Some((path, source::str_attr(&p.input, "narHash")?))
}

/// An error outside the evaluator as an evaluation error at `pos`.
pub(crate) fn eval_error(e: Error, pos: Pos) -> Box<EvalError> {
    let kind = match e {
        Error::Unsupported(_) => ErrorKind::Unsupported,
        _ => ErrorKind::Eval,
    };
    EvalError::boxed(kind, e.to_string(), pos)
}

/// Evaluates the flake and selects `attr` from its outputs (or returns the
/// outputs attrset for an empty path).
pub fn eval_flake(ev: &mut Evaluator, fr: &FlakeRef) -> EvalResult<Word> {
    let result = eval_flake_value(ev, fr)?;
    select_output(ev, fr, result)
}

/// `fr.attr` of the outputs in `result`, the value [`eval_flake_value`] gave.
pub fn select_output(ev: &mut Evaluator, fr: &FlakeRef, result: Word) -> EvalResult<Word> {
    let pos = Pos::NONE;
    let attrs = ev.force_attrs(result, pos, "while evaluating the flake")?;
    let outputs_sym = ev.sym(b"outputs");
    let Some(outputs) = attrs.get(outputs_sym) else {
        return Err(EvalError::boxed(
            ErrorKind::MissingAttr,
            format!("flake '{}' has no attribute 'outputs'", fr.dir.display()),
            pos,
        ));
    };
    let outputs = outputs.cell.get();
    let candidates = output_candidates(&fr.attr, ev.current_system());
    for path in &candidates {
        if let Some(v) = select(ev, outputs, path)? {
            return ev.force(v);
        }
    }
    let names: Vec<String> = candidates
        .iter()
        .map(|p| format!("'{}'", p.join(".")))
        .collect();
    let shown = match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => names.concat(),
    };
    Err(EvalError::boxed(
        ErrorKind::MissingAttr,
        format!(
            "flake '{}' does not provide attribute {shown}",
            fr.dir.display()
        ),
        pos,
    ))
}

/// installable-flake.cc:getActualAttrPaths: `packages.<system>.`, then
/// `legacyPackages.<system>.`, then the path itself; a leading `.` names
/// the path from the root only. The empty path is the outputs.
fn output_candidates(attr: &[String], system: &str) -> Vec<Vec<String>> {
    match attr.split_first() {
        None => vec![Vec::new()],
        Some((first, rest)) if first.is_empty() => vec![rest.to_vec()],
        Some(_) => ["packages", "legacyPackages"]
            .iter()
            .map(|set| {
                let mut p = vec![set.to_string(), system.to_string()];
                p.extend_from_slice(attr);
                p
            })
            .chain([attr.to_vec()])
            .collect(),
    }
}

/// eval-cache.cc:AttrCursor::findAlongAttrPath: walks `path` from `v`, forcing each
/// step; `None` when a step is missing or the value at a step is not an attrset.
pub fn select(ev: &mut Evaluator, mut v: Word, path: &[String]) -> EvalResult<Option<Word>> {
    for name in path {
        let forced = ev.force(v)?;
        let Some(attrs) = forced.as_attrs() else {
            return Ok(None);
        };
        let sym = ev.sym(name.as_bytes());
        let Some(a) = attrs.get(sym) else {
            return Ok(None);
        };
        v = a.cell.get();
    }
    Ok(Some(v))
}

/// Every input `flake.nix` declares must have a lock entry; Nix would
/// otherwise fetch and lock it. flake.cc:getFlake declares one for each
/// name in `inputs`, and one for each formal of `outputs` but `self`.
/// True when `flake.nix` has a `nixConfig`.
fn check_declared_inputs(
    ev: &mut Evaluator,
    flake_dir: &[u8],
    lock: &lock::Lock,
    pos: Pos,
) -> EvalResult<bool> {
    let mut p = flake_dir.to_vec();
    p.extend_from_slice(b"/flake.nix");
    let v = ev.eval_file(&p, pos)?;
    let attrs = ev.force_attrs(v, pos, "while evaluating flake.nix")?;
    let mut declared = Vec::new();
    let mut inputs = None;
    if let Some(a) = attrs.get(ev.sym(b"inputs")) {
        let a = ev.force_attrs(
            a.cell.get(),
            pos,
            "while evaluating the 'inputs' attribute of flake.nix",
        )?;
        declared.extend(a.iter().map(|a| (a.sym, "")));
        inputs = Some(a);
    }
    if let Some(outputs) = attrs.get(ev.sym(b"outputs")) {
        let outputs = ev.force(outputs.cell.get())?;
        if let Value::Lambda(l) = outputs.view()
            && let ParamIr::Formals { formals, .. } = &l.lam.param
        {
            let self_sym = ev.sym(b"self");
            let implicit = ", an argument of 'outputs' that Nix looks up in the flake registry,";
            declared.extend(
                formals
                    .iter()
                    .filter(|f| f.sym != self_sym)
                    .map(|f| (f.sym, implicit)),
            );
        }
    }
    for (sym, how) in declared {
        let name = ev.interner.name(sym).to_vec();
        if !lock.root_inputs().any(|k| k.as_bytes() == name) {
            return Err(EvalError::boxed(
                ErrorKind::Unsupported,
                format!(
                    "{LOCK_ERROR} (input '{}'{how} has no lock entry)",
                    String::from_utf8_lossy(&name)
                ),
                pos,
            ));
        }
    }
    if let Some(inputs) = inputs {
        check_inputs(ev, lock, &lock.root, "", inputs, pos)?;
    }
    Ok(attrs.get(ev.sym(b"nixConfig")).is_some())
}

/// flake.cc:computeLocks keeps a lock entry only while `flake.nix`, with
/// the overrides in it, still asks for it. Only the root's `flake.nix` is
/// read, so a stale `follows` that an input's own flake declares passes.
fn check_inputs(
    ev: &mut Evaluator,
    lock: &lock::Lock,
    node: &str,
    path: &str,
    inputs: &'static AttrsObj,
    pos: Pos,
) -> EvalResult<()> {
    for a in inputs.iter() {
        let name = String::from_utf8_lossy(ev.interner.name(a.sym)).into_owned();
        let at = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}/{name}")
        };
        let decl = ev.force_attrs(a.cell.get(), pos, "while evaluating a flake input")?;
        let mut get = |k: &[u8]| decl.get(ev.sym(k)).map(|x| x.cell.get());
        let (url, follows, sub) = (get(b"url"), get(b"follows"), get(b"inputs"));
        let is_ref = url.is_some() || get(b"type").is_some();
        let want = match (follows, url) {
            (Some(_), _) if is_ref => {
                let m = format!(
                    "flake input '{at}' has both a flake reference and a follows attribute"
                );
                return Err(EvalError::boxed(ErrorKind::Eval, m, pos));
            }
            (Some(f), _) => {
                let f = ev.force_string_no_ctx(f, pos, "while evaluating a follows attribute")?;
                let f = String::from_utf8_lossy(f);
                Declared::Follows(
                    f.split('/')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                )
            }
            (None, Some(u)) => declared_ref(ev, u)?,
            (None, None) => Declared::Unknown,
        };
        if let Some(why) = lock.disagrees(node, &name, &want) {
            let m = format!("{LOCK_ERROR} (input '{at}' {why}, not in flake.lock)");
            return Err(EvalError::boxed(ErrorKind::Unsupported, m, pos));
        }
        if let Some(sub) = sub
            && let Some(child) = lock.child(node, &name)
        {
            let sub = ev.force_attrs(sub, pos, "while evaluating the 'inputs' of a flake input")?;
            check_inputs(ev, lock, child, &at, sub, pos)?;
        }
    }
    Ok(())
}

/// A url string as the lock's `original` holds it; a path value or a url
/// iets cannot parse is not compared.
fn declared_ref(ev: &mut Evaluator, url: Word) -> EvalResult<Declared> {
    let Value::String(s) = ev.force(url)?.view() else {
        return Ok(Declared::Unknown);
    };
    let url = String::from_utf8_lossy(s.bytes).into_owned();
    let Ok(mut p) = flakeref::parse(&url) else {
        return Ok(Declared::Unknown);
    };
    if !p.dir.is_empty() {
        p.input
            .insert("dir".into(), AttrVal::Str(mem::take(&mut p.dir)));
    }
    let original = p
        .input
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                AttrVal::Str(s) => serde_json::Value::String(s),
                AttrVal::Bool(b) => serde_json::Value::Bool(b),
                AttrVal::Int(n) => serde_json::Value::from(n),
            };
            (k, v)
        })
        .collect();
    Ok(Declared::Ref(url, original))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::internal_string;

    use crate::source::scm;
    use crate::store::nar::tests::{Tree, tree};
    use rstest::rstest;

    #[test]
    fn attr_paths() {
        assert_eq!(
            parse_attr_path("devShells.x86_64-linux.default.drvPath").unwrap(),
            ["devShells", "x86_64-linux", "default", "drvPath"]
        );
        assert_eq!(
            parse_attr_path("packages.\"x86_64-linux\".\"a.b\"").unwrap(),
            ["packages", "x86_64-linux", "a.b"]
        );
        assert_eq!(parse_attr_path("").unwrap(), Vec::<String>::new());
        assert!(parse_attr_path("a.\"b").is_err());
    }

    #[rstest]
    fn refs_resolve_against_cwd_and_home(#[from(tree)] d: Tree) {
        fs::write(d.join("flake.nix"), b"{ outputs = _: {}; }").unwrap();
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/flake.nix"), b"{ outputs = _: {}; }").unwrap();
        let cwd = &sys::path_to_internal(&d);
        let r = parse_flake_ref(".#a.b", cwd, None).unwrap();
        assert_eq!(r.dir, *d);
        assert_eq!(r.source, SourceRef::Path(d.clone()));
        assert_eq!(r.attr, ["a", "b"]);
        let r = parse_flake_ref("./sub", cwd, None).unwrap();
        assert_eq!(r.dir, d.join("sub"));
        assert!(r.attr.is_empty());
        let r = parse_flake_ref("~/sub#", b"/", Some(cwd)).unwrap();
        assert_eq!(r.dir, d.join("sub"));
        assert!(parse_flake_ref("./missing", cwd, None).is_err());
        let unsupported = |r: &str| match parse_flake_ref(r, cwd, None) {
            Err(Error::Unsupported(m)) => m,
            other => panic!("{r}: {other:?}"),
        };
        for r in [
            "github:a/b#x",
            "git+https://x.org/a",
            "https://x.org/a.tar.gz#x",
        ] {
            assert!(unsupported(r).contains("only local paths"), "{r}");
        }
        assert!(unsupported(".?dir=sub").contains("query parameters"));
        assert!(unsupported("hg+https://x.org/a#x").contains("not supported"));
        assert!(matches!(
            parse_flake_ref("./x#%zz", cwd, None),
            Err(Error::Flake(_))
        ));
        assert_eq!(
            parse_flake_ref("path:./sub", cwd, None).unwrap().dir,
            d.join("sub")
        );
        for r in [
            "nixpkgs#hello",
            "nixpkgs/nixos-24.05#x",
            "flake:nixpkgs",
            "sub#x",
        ] {
            assert!(unsupported(r).contains("flake registry references"), "{r}");
        }

        assert!(is_flake_ref(&format!("{}/sub", d.display())));
        assert!(!is_flake_ref("x.nix"));
    }

    #[rstest]
    fn a_local_git_checkout_can_request_submodules(#[from(tree)] d: Tree) {
        fs::write(d.join("flake.nix"), b"{ outputs = _: {}; }").unwrap();
        fs::create_dir_all(d.join(".git")).unwrap();
        let cwd = sys::path_to_internal(&d);
        let r = parse_flake_ref(".?submodules=1#value", &cwd, None).unwrap();
        assert_eq!(
            r.source,
            SourceRef::Submodules {
                root: d.clone(),
                subdir: String::new(),
            }
        );
        assert_eq!(r.attr, ["value"]);
        assert!(r.probed);
        assert!(matches!(
            parse_flake_ref("path:./?submodules=1", &cwd, None),
            Err(Error::Unsupported(_))
        ));
    }

    #[rstest]
    fn a_pinned_checkout_is_its_revision(#[from(tree)] d: Tree) {
        fs::write(d.join("flake.nix"), b"{ outputs = _: {}; }").unwrap();
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("sub/flake.nix"), b"{ outputs = _: {}; }").unwrap();
        fs::create_dir_all(d.join(".git")).unwrap();
        let rev = "0".repeat(39) + "1";
        let git = scm::named("git").unwrap();
        let url = format!("git+file://{}", internal_string(&d));

        let r = parse_flake_ref(&format!("{url}?ref=HEAD&rev={rev}#a"), b"/", None).unwrap();
        assert_eq!(r.dir, *d);
        assert_eq!(r.attr, ["a"]);
        assert_eq!(
            r.source,
            SourceRef::Commit {
                scm: git,
                root: d.clone(),
                rev: rev.clone(),
                subdir: String::new(),
            }
        );

        // `dir` is the flake inside the tree, and the tree stays the root
        let r = parse_flake_ref(&format!("{url}?rev={rev}&dir=sub"), b"/", None).unwrap();
        assert_eq!(r.dir, d.join("sub"));
        assert_eq!(
            r.source,
            SourceRef::Commit {
                scm: git,
                root: d.clone(),
                rev,
                subdir: "sub".to_string(),
            }
        );

        // no rev: the working tree, as a plain path resolves to
        let r = parse_flake_ref(&url, b"/", None).unwrap();
        assert_eq!(
            r.source,
            SourceRef::Tracked {
                scm: git,
                root: d.clone(),
                subdir: String::new(),
            }
        );

        let unsupported = |r: &str| match parse_flake_ref(r, b"/", None) {
            Err(Error::Unsupported(m)) => m,
            other => panic!("{r}: {other:?}"),
        };
        assert!(unsupported(&format!("{url}?ref=main")).contains("no 'rev'"));
        assert!(unsupported(&format!("{url}?submodules=1")).contains("'submodules'"));
        assert!(unsupported("git+https://x.org/a").contains("only local paths"));

        // a rev that is not one says so, instead of the repository reporting
        // that it has no revision ''
        let bad = |r: &str| match parse_flake_ref(r, b"/", None) {
            Err(Error::Flake(m)) => m,
            other => panic!("{r}: {other:?}"),
        };
        for r in ["", "HEAD", "0123", &"z".repeat(40)] {
            let m = bad(&format!("{url}?rev={r}"));
            assert!(m.contains("is not a revision"), "{r}: {m}");
        }
    }
}
