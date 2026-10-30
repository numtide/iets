//! File system primops (primops.cc): readFile, readDir, readFileType,
//! pathExists, toPath, storePath, filterSource, path, toFile. The network
//! fetchers and flake primops are registered, so `builtins ? fetchMercurial`
//! holds, but a call fails. Only these work: `fetchurl`, `fetchTree` on a
//! locked input, `fetchTarball` with a `sha256`, `fetchGit`, `getFlake`,
//! `parseFlakeRef` and `flakeRefToString` (`flake`).

use crate::sys::errno;
use crate::sys::{self, OsStrExt};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::from_utf8;
use std::sync::Arc;
use std::{fs, io};

use super::Registry;
use super::store_hooks::{self, At, Follow};
use super::{fetch, fetch_git};
use crate::compile::canon_path;
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::flake::{self, flakeref};
use crate::lint::{LintId, coerce, purity};
use crate::locate;
use crate::runtime::{self, Ctx, Value, Word, ctx_to_arena};
use crate::source;
use crate::store::drv::Derivation;
use crate::store::hash::{Hash, HashAlgo, Hasher, hash_bytes};
use crate::store::nar::FileType;
use crate::store::path::{
    HASH_PART_LEN, IngestionMethod, StorePath, StorePathError, make_fixed_output_path,
    make_text_path, strip_store_dir,
};
use crate::store::source::{SourceOrigin, Src, SrcPath};
use crate::trace::{self, Kind};
use crate::{config, log, treestore};

pub fn register(r: &mut Registry) {
    r.op("__readFile", 1, read_file);
    r.op("__readDir", 1, read_dir);
    r.op("__readFileType", 1, read_file_type);
    r.op("__pathExists", 1, path_exists);
    r.op("__toPath", 1, to_path);
    r.op("__storePath", 1, store_path);
    r.op("__filterSource", 2, filter_source);
    r.op("__path", 1, path);
    r.op("__toFile", 2, to_file);
    r.op("__fetchurl", 1, fetch::fetchurl_builtin);
    r.op("fetchTarball", 1, fetch::fetch_tarball_builtin);
    r.op("fetchGit", 1, fetch_git::fetch_git);
    r.op("fetchTree", 1, fetch::fetch_tree);
    r.op("fetchMercurial", 1, fetch_mercurial);
    r.op("__fetchClosure", 1, fetch_closure);
    r.op("__getFlake", 1, flake::get_flake);
    r.op("__parseFlakeRef", 1, flakeref::parse_flake_ref);
    r.op("__flakeRefToString", 1, flakeref::flake_ref_to_string);
}

macro_rules! unsupported {
    ($fn:ident, $msg:expr, hint: $hint:expr) => {
        fn $fn(ev: &mut Evaluator, _args: &[Word], pos: Pos) -> EvalResult<Word> {
            ev.trace().mark_uncacheable();
            let mut e = Error::new(ErrorKind::Unsupported, $msg.into(), pos);
            e.hint = Some($hint.into());
            Err(Box::new(e))
        }
    };
    ($fn:ident, $msg:expr) => {
        fn $fn(ev: &mut Evaluator, _args: &[Word], pos: Pos) -> EvalResult<Word> {
            ev.trace().mark_uncacheable();
            bail!(ErrorKind::Unsupported, pos, "{}", $msg)
        }
    };
    ($fn:ident, $name:literal, $msg:expr) => {
        fn $fn(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
            ev.trace().mark_uncacheable();
            crate::lint::purity::unhashed_fetch(ev, $name, args[0], pos)?;
            bail!(ErrorKind::Unsupported, pos, "{}", $msg)
        }
    };
}

unsupported!(
    fetch_mercurial,
    "builtins.fetchMercurial is not supported",
    hint: "use `builtins.fetchTree { type = \"hg\"; url = \"…\"; rev = \"…\"; narHash = \"…\"; }` \
     or an hg flake input, or run it as nix-instantiate, nix-build or nix-shell, \
     which fall back to Nix"
);
unsupported!(
    fetch_closure,
    "iets: network fetchers are not supported (builtins.fetchClosure)"
);

fn lossy(b: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(b)
}

fn os_path(p: &[u8]) -> PathBuf {
    sys::path_from_internal(p)
}

fn io_error(e: io::Error, verb: &str, p: &[u8], pos: Pos) -> Box<Error> {
    let msg = if e.kind() == io::ErrorKind::NotFound {
        format!("path '{}' does not exist", lossy(p))
    } else {
        format!("{verb} '{}': {}", lossy(p), crate::error::io_text(&e))
    };
    Error::boxed(ErrorKind::Io, msg, pos)
}

fn utf8(b: &[u8], pos: Pos) -> EvalResult<&str> {
    from_utf8(b).map_err(|_| {
        Error::boxed(
            ErrorKind::Eval,
            format!("'{}' is not valid UTF-8", lossy(b)),
            pos,
        )
    })
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Symlink => FileType::Symlink.nix_name(),
        Kind::Directory => FileType::Directory.nix_name(),
        Kind::Regular => FileType::Regular.nix_name(),
        _ => FileType::Unknown.nix_name(),
    }
}

/// A failed [`SrcPath::read`] of a file, `readFile`'s way.
fn read_error((verb, e): (&str, io::Error), p: &[u8], pos: Pos) -> Box<Error> {
    if errno::is_directory(&e) {
        Error::boxed(
            ErrorKind::Io,
            format!("file '{}' is not a regular file", lossy(p)),
            pos,
        )
    } else {
        io_error(e, verb, p, pos)
    }
}

/// Nix's `coerceToPath`: path values pass through, `__toString` sets are
/// called, everything else is coerced to a string that must be absolute.
pub fn coerce_to_path(
    ev: &mut Evaluator,
    w: Word,
    ctx: &mut Vec<Ctx>,
    pos: Pos,
    err_ctx: &str,
) -> EvalResult<Vec<u8>> {
    let w = ev.force(w).map_err(|mut e| {
        e.add_trace(pos, err_ctx);
        e
    })?;
    match w.view() {
        Value::Path(p) => return Ok(p.path.to_vec()),
        Value::Attrs(a) => {
            if let Some(f) = a.get(ev.s.to_string) {
                let f = ev.force_cell(&f.cell)?;
                let r = ev.call1(f, w, pos)?;
                return coerce_to_path(ev, r, ctx, pos, err_ctx);
            }
        }
        _ => {}
    }
    let s = ev.coerce_to_string(w, ctx, pos, err_ctx, false, false, true)?;
    if s.first() != Some(&b'/') {
        let mut e = Error::new(
            ErrorKind::Eval,
            format!("string '{}' doesn't represent an absolute path", lossy(&s)),
            pos,
        );
        e.add_trace(pos, err_ctx);
        return Err(Box::new(e));
    }
    Ok(canon_path(&s))
}

/// Nix's `realisePath` without building: a path with a derivation output in
/// its context cannot be realised here, and an opaque store path or `.drv`
/// (primops.cc:realiseContext treats `DrvDeep` as opaque) must exist or be
/// one this evaluation made. The result is the path as Nix names it;
/// `store_hooks::locate` gives the file to read.
pub fn realise_path(ev: &mut Evaluator, w: Word, pos: Pos) -> EvalResult<Vec<u8>> {
    let mut ctx = Vec::new();
    let p = coerce_to_path(
        ev,
        w,
        &mut ctx,
        Pos::NONE,
        "while realising the context of a path",
    )?;
    for c in &ctx {
        let mut kind = ErrorKind::Eval;
        let msg = match c {
            Ctx::Impure(_) => continue,
            Ctx::Opaque(sp) | Ctx::DrvDeep(sp) => {
                if store_hooks::locate(ev, sp).is_some()
                    || ev.trace().stat_now(sp, true) != trace::Kind::Missing
                {
                    continue;
                }
                if let Ok(Ok(d)) = from_utf8(sp).map(StorePath::parse)
                    && ev.drv.drvs.lock().contains_key(&d)
                {
                    kind = ErrorKind::Unsupported;
                    format!(
                        "'{}' is not in the store and this run did not keep its text (--no-tracing-cache)",
                        lossy(sp)
                    )
                } else {
                    format!("path '{}' is not valid", lossy(sp))
                }
            }
            Ctx::Built { drv: d, output } => {
                if let Some(out) = built_output_present(ev, d, output) {
                    purity::ifd_local_output(ev, &out, pos)?;
                    ev.known.add_on_disk(out);
                    continue;
                }
                kind = ErrorKind::Unsupported;
                format!(
                    "import from derivation is not supported: '{}' is not built",
                    lossy(d)
                )
            }
        };
        let mut e = Error::new(kind, msg, pos);
        e.add_trace(
            pos,
            format!("while realising the context of path '{}'", lossy(&p)),
        );
        return Err(Box::new(e));
    }
    Ok(p)
}

/// The path of output `output` of `drv` when it is already in the store, so
/// a read needs no build. Only a path known without a build counts:
/// input-addressed or fixed, not a dynamic derivation's, a floating or a
/// deferred one. `stat_now` records the kind, so a tracing-cache hit checks
/// the path again.
fn built_output_present(ev: &Evaluator, drv: &[u8], output: &[u8]) -> Option<StorePath> {
    if !config::reads_built_outputs(ev.session()) {
        return None;
    }
    let drv = from_utf8(drv).ok()?;
    if drv.contains('!') {
        return None;
    }
    let path = StorePath::parse(drv).ok()?;
    let aterm = match ev.drv.drvs.lock().get(&path).and_then(|i| i.aterm.clone()) {
        Some(a) => a.to_vec(),
        None => fs::read(path.to_string()).ok()?,
    };
    let parsed = Derivation::parse(&aterm, Derivation::name_from_path(&path).ok()?).ok()?;
    let output = from_utf8(output).ok()?;
    let out = parsed
        .outputs
        .get(output)?
        .path(&parsed.name, output)
        .ok()??;
    (ev.trace().stat_now(out.to_string().as_bytes(), true) != Kind::Missing).then_some(out)
}

/// What a realised path reads, the path its messages name (the file on
/// disk, else the path itself), and the path after `follow`. An entry a copied tree's filter left out is `path '...' does not
/// exist`. `follow` is the resolution primops.cc asks `realisePath` for;
/// `readFileType` asks for none.
fn realise_existing(
    ev: &mut Evaluator,
    w: Word,
    follow: Option<Follow>,
    pos: Pos,
) -> EvalResult<(SrcPath, Vec<u8>, Vec<u8>)> {
    let p = realise_path(ev, w, pos)?;
    let (p, at) = match follow {
        Some(m) => store_hooks::located(ev, p, m, pos)?,
        None => {
            let at = store_hooks::locate(ev, &p);
            (p, at)
        }
    };
    let at = store_hooks::check(ev, at, &p, follow.unwrap_or(Follow::Ancestors), pos)?;
    if at.hidden() {
        return Err(io_error(errno::not_found(), "getting status of", &p, pos));
    }
    coerce::read_through_copy(ev, &p, pos)?;
    let shown = at
        .local()
        .map_or_else(|| p.clone(), |l| sys::path_to_internal(&l));
    Ok((at, shown, p))
}

/// Reads `st_size` bytes like `PosixSourceAccessor::readFile`, so a procfs
/// file reads as empty.
fn read_file(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let (at, p, named) = realise_existing(ev, args[0], Some(Follow::Full), pos)?;
    let bytes = at.read(ev, true).map_err(|e| read_error(e, &p, pos))?;
    if bytes.contains(&0) {
        bail!(
            ErrorKind::Eval,
            pos,
            "the contents of the file '{}' cannot be represented as a Nix string",
            lossy(&p)
        );
    }
    let refs = file_references(ev, &named, &bytes);
    if refs.is_empty() {
        return Ok(ev.mk_string(&bytes));
    }
    let ctx = refs
        .iter()
        .map(|r| Ctx::Opaque(ev.arena.alloc_bytes(r.to_full_string().as_bytes())))
        .collect();
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(&bytes, ctx))
}

/// primops.cc:prim_readFile's context: the references of the file's store
/// path whose hash part occurs in `bytes`.
fn file_references(ev: &Evaluator, named: &[u8], bytes: &[u8]) -> BTreeSet<StorePath> {
    // nothing for the scan to keep, so skip the store query
    if !has_hash_run(bytes) {
        return BTreeSet::new();
    }
    // realisePath resolves links before the isInStore check
    let resolved;
    let path = if store_hooks::locate(ev, named).is_none() {
        let r = sys::fs::canonicalize(&os_path(named));
        ev.trace().canon(named, r.as_deref().ok());
        let Ok(r) = r else {
            return BTreeSet::new();
        };
        resolved = sys::path_to_internal(&r);
        &resolved[..]
    } else {
        named
    };
    if !ev.is_store_path_text(path) {
        return BTreeSet::new();
    }
    let Some((Ok(sp), _)) = to_store_path(path) else {
        return BTreeSet::new();
    };
    scan_references(bytes, references(ev, Some(sp)))
}

fn is_nix32(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'd' | b'f'..=b'n' | b'p'..=b's' | b'v'..=b'z')
}

/// Whether `bytes` has a run of [`HASH_PART_LEN`] nix32 characters.
fn has_hash_run(bytes: &[u8]) -> bool {
    let mut run = 0;
    for &b in bytes {
        run = if is_nix32(b) { run + 1 } else { 0 };
        if run == HASH_PART_LEN {
            return true;
        }
    }
    false
}

/// references.cc:RefScanSink: the `refs` whose hash part occurs in `bytes`.
fn scan_references(bytes: &[u8], refs: BTreeSet<StorePath>) -> BTreeSet<StorePath> {
    let mut by_hash: HashMap<Vec<u8>, StorePath> = refs
        .into_iter()
        .map(|r| (r.hash_part().as_bytes().to_vec(), r))
        .collect();
    let mut found = BTreeSet::new();
    let mut i = 0;
    while i + HASH_PART_LEN <= bytes.len() && !by_hash.is_empty() {
        let window = &bytes[i..i + HASH_PART_LEN];
        if let Some(j) = window.iter().rposition(|&b| !is_nix32(b)) {
            i += j + 1;
            continue;
        }
        if let Some(r) = by_hash.remove(window) {
            found.insert(r);
        }
        i += 1;
    }
    found
}

fn read_dir(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let (at, p, _) = realise_existing(ev, args[0], Some(Follow::Full), pos)?;
    let listing = at.read_dir(ev).map_err(|(verb, e)| {
        if errno::is_not_directory(&e) {
            Error::boxed(
                ErrorKind::Io,
                format!("'{}' is not a directory", lossy(&p)),
                pos,
            )
        } else {
            io_error(e, verb, &p, pos)
        }
    })?;
    let names = [
        ev.mk_string(FileType::Regular.nix_name().as_bytes()),
        ev.mk_string(FileType::Directory.nix_name().as_bytes()),
        ev.mk_string(FileType::Symlink.nix_name().as_bytes()),
        ev.mk_string(FileType::Unknown.nix_name().as_bytes()),
    ];
    let mut entries = Vec::with_capacity(listing.len());
    for (name, t) in listing {
        entries.push((ev.sym(&name), Pos::NONE, names[t as usize]));
    }
    ev.mk_attrs(&mut entries, pos)
}

fn read_file_type(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let (at, p, _) = realise_existing(ev, args[0], None, pos)?;
    let kind = at
        .stat(ev, false)
        .map_err(|e| io_error(e, "getting status of", &p, pos))?;
    Ok(ev.mk_string(kind_name(kind).as_bytes()))
}

fn path_exists(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let w = ev.force(args[0])?;
    let must_be_dir = matches!(
        w.view(),
        Value::String(s) if s.bytes.ends_with(b"/") || s.bytes.ends_with(b"/.")
    );
    let p = realise_path(ev, w, pos)?;
    let follow = if must_be_dir {
        Follow::Full
    } else {
        Follow::Ancestors
    };
    let (p, at) = store_hooks::located(ev, p, follow, pos)?;
    if !store_hooks::admits(ev, at.as_ref(), &p, follow, pos)? {
        return Ok(Word::FALSE);
    }
    coerce::read_through_copy(ev, &p, pos)?;
    let at = at.unwrap_or_else(|| store_hooks::fs_path(&p));
    match at.stat(ev, must_be_dir) {
        Ok(k) => Ok(Word::from_bool(k == Kind::Directory || !must_be_dir)),
        Err(e) if e.kind() == io::ErrorKind::NotFound || errno::is_not_directory(&e) => {
            Ok(Word::FALSE)
        }
        Err(e) => {
            let shown = at.local().map_or(p, |l| sys::path_to_internal(&l));
            Err(io_error(e, "getting status of", &shown, pos))
        }
    }
}

fn to_path(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut ctx = Vec::new();
    let p = coerce_to_path(
        ev,
        args[0],
        &mut ctx,
        pos,
        "while evaluating the first argument passed to builtins.toPath",
    )?;
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(&p, ctx))
}

/// `/nix/store/<hash>-<name>` with nothing after it.
fn is_store_path(p: &[u8]) -> bool {
    to_store_path(p).is_some_and(|(sp, rest)| rest.is_empty() && sp.is_ok())
}

/// Split `/nix/store/<base>/rest` into the store path and the rest.
fn to_store_path(p: &[u8]) -> Option<(Result<StorePath, ()>, &[u8])> {
    let rest = strip_store_dir(p)?;
    let end = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
    let base = from_utf8(&rest[..end]).ok()?;
    Some((
        StorePath::from_base_name(base).map_err(|_| ()),
        &rest[end..],
    ))
}

fn store_path(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    if ev.pure() {
        bail!(
            ErrorKind::Eval,
            pos,
            "'builtins.storePath' is not allowed in pure evaluation mode"
        );
    }
    let mut ctx = Vec::new();
    let mut p = coerce_to_path(
        ev,
        args[0],
        &mut ctx,
        pos,
        "while evaluating the first argument passed to 'builtins.storePath'",
    )?;
    if !is_store_path(&p) {
        p = store_hooks::resolve_links(ev, &p, Follow::Full, pos)?;
        if store_hooks::locate(ev, &p).is_none_or(|at| at.hidden()) {
            let resolved = sys::fs::canonicalize(&os_path(&p));
            ev.trace().canon(&p, resolved.as_deref().ok());
            let resolved = resolved.map_err(|e| io_error(e, "getting status of", &p, pos))?;
            p = sys::path_to_internal(&resolved);
        }
    }
    let sp = match to_store_path(&p) {
        Some((Ok(sp), _)) => sp,
        _ => bail!(
            ErrorKind::Eval,
            pos,
            "path '{}' is not in the Nix store",
            lossy(&p)
        ),
    };
    let full = ev.arena.alloc_bytes(sp.to_full_string().as_bytes());
    ctx.push(Ctx::Opaque(full));
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(&p, ctx))
}

/// The base name of a canonical path; `source` for the root, like
/// `SourcePath::baseName`.
fn base_name(p: &[u8]) -> &[u8] {
    match p.iter().rposition(|&b| b == b'/') {
        Some(i) if i + 1 < p.len() => &p[i + 1..],
        _ => b"source",
    }
}

/// A store path as a string whose context is that path.
fn store_path_string(ev: &mut Evaluator, sp: &StorePath) -> Word {
    let full = ev.arena.alloc_bytes(sp.to_full_string().as_bytes());
    let ctx = ev.arena.alloc_slice(&[Ctx::Opaque(full)]);
    ev.arena.mk_string_static(full, ctx)
}

fn call_filter(
    ev: &mut Evaluator,
    filter: Word,
    p: &Path,
    t: FileType,
    pos: Pos,
) -> EvalResult<bool> {
    let a = ev.mk_string(p.as_os_str().as_bytes());
    let b = ev.mk_string(t.nix_name().as_bytes());
    let r = ev.call2(filter, a, b, pos)?;
    ev.force_bool(
        r,
        pos,
        "while evaluating the return value of the path filter function",
    )
}

/// Mirrors primops.cc:addPath. `in_store` is a store path the caller was
/// given with a context, whose references belong to the copy.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_path(
    ev: &mut Evaluator,
    pos: Pos,
    name: &[u8],
    path: &[u8],
    filter: Option<Word>,
    method: IngestionMethod,
    expected: Option<Hash>,
    in_store: Option<StorePath>,
) -> EvalResult<StorePath> {
    add_path_inner(ev, pos, name, path, filter, method, expected, in_store).map_err(|mut e| {
        e.add_trace(pos, format!("while adding path '{}'", lossy(path)));
        e
    })
}

/// The store path `path` lies under when the value that named it carried a
/// context, which is when primops.cc:addPath reads its references.
fn in_store(ev: &Evaluator, path: &[u8], ctx: &[Ctx]) -> Option<StorePath> {
    if ctx.is_empty() || !ev.is_store_path_text(path) {
        return None;
    }
    to_store_path(path)?.0.ok()
}

/// The references of `sp`: a `toFile`'s own, a `.drv`'s inputs, none for a
/// tree, else what the local store holds. iets writes no `.drv` to the
/// store, so the store's answer for one would depend on earlier builds. A
/// flake input or the flake root is a tree, so it skips the store query,
/// which costs a `nix` process. Nix swallows the store's error, and so does
/// this.
fn references(ev: &Evaluator, sp: Option<StorePath>) -> BTreeSet<StorePath> {
    let Some(sp) = sp else {
        return BTreeSet::new();
    };
    if let Some(d) = ev.drv.drvs.get(&sp) {
        return d.references;
    }
    match ev.known.get(&sp) {
        Some(k) if k.origin.is_some() => k
            .origin
            .and_then(|o| o.text_refs().cloned())
            .unwrap_or_default(),
        Some(k) if k.flake.is_some() => BTreeSet::new(),
        _ => locate::references_of(&sp),
    }
}

#[allow(clippy::too_many_arguments)]
fn add_path_inner(
    ev: &mut Evaluator,
    pos: Pos,
    name: &[u8],
    path: &[u8],
    filter: Option<Word>,
    method: IngestionMethod,
    expected: Option<Hash>,
    in_store: Option<StorePath>,
) -> EvalResult<StorePath> {
    let name = utf8(name, pos)?;
    let refs = references(ev, in_store);
    coerce::store_path_recopy(ev, path, pos)?;
    coerce::read_through_copy(ev, path, pos)?;
    let store_err = |e: StorePathError| Error::boxed(ErrorKind::Eval, e.0, pos);
    let expected_sp = expected
        .map(|h| make_fixed_output_path(method, &h, name, &refs))
        .transpose()
        .map_err(store_err)?;
    if let (Some(sp), Some(h)) = (&expected_sp, &expected)
        && source::valid_in_store(ev, sp, method, h)
    {
        ev.known.lock().entry(sp.clone()).or_default();
        return Ok(sp.clone());
    }
    // primops.cc:addPath dumps `path.resolveSymlinks()`, so the copy is of
    // what the links point at and a dangling one is an error.
    let (target, located) = store_hooks::located(ev, path.to_vec(), Follow::Full, pos)?;
    let path = &target[..];
    let virtual_tree = located.is_some();
    // A path under a tree this evaluation copied reads the local tree, with
    // that tree's filter applied before this call's `filter`, or the input's
    // spool; the filter function still sees the store path Nix would show it.
    let at = store_hooks::check(ev, located, path, Follow::Full, pos)?;
    let missing = |p: &[u8]| io_error(errno::not_found(), "getting status of", p, pos);
    let at = if virtual_tree {
        at
    } else {
        let Ok(resolved) = sys::fs::canonicalize(&os_path(path)) else {
            // a link with no target, or a missing component: the walk names
            // the path that does not exist, as `resolveSymlinks` names it
            let end = store_hooks::resolve_links(ev, path, Follow::Full, pos)?;
            return Err(missing(&end));
        };
        if sys::path_to_internal(&resolved) != path {
            ev.trace().canon(path, Some(&resolved));
        }
        SrcPath::fs(resolved, None)
    };
    let on_disk = match at.at() {
        None => return Err(missing(path)),
        Some(At::Mem(_)) => return Err(store_hooks::recopy_unsupported(path, pos)),
        Some(At::Spool(entry, base)) => {
            treestore::record(ev, &entry, base);
            false
        }
        Some(At::Disk(_)) => true,
    };
    if virtual_tree && at.stat(ev, false).is_err() {
        return Err(missing(path));
    }
    let resolved = at.local().unwrap_or_else(|| at.in_spool());
    let parent = match &at.src {
        Src::Fs { accepted, .. } | Src::Spool { accepted, .. } => accepted.clone(),
        Src::Mem(_) => None,
    };
    // a copy of a virtual store path is a copy of a store path to Nix, whose
    // filter sees the entry under that path and never the local tree
    let shown = |p: &Path| -> PathBuf {
        match p.strip_prefix(&resolved) {
            Ok(rest) if virtual_tree => os_path(path).join(rest),
            _ => p.to_path_buf(),
        }
    };
    let mut accepted: Option<HashSet<PathBuf>> = filter.map(|_| HashSet::new());
    let nar_size: u64;
    let hash = match method {
        // only `outputHashMode` reaches it, never a path added to the store
        IngestionMethod::Text => unreachable!("a source tree is not text addressed"),
        IngestionMethod::Flat => {
            let bytes = at.read(ev, true).map_err(|e| read_error(e, path, pos))?;
            nar_size = bytes.len() as u64;
            hash_bytes(HashAlgo::Sha256, &bytes)
        }
        IngestionMethod::Recursive => {
            // The observation records `accepted`, which is the dump's own decisions unless
            // an enclosing tree's `parent` set is the only thing narrowing them: then the
            // recorded filter is accept-all and only a separate walk matches it.
            let mut fp = (on_disk && (filter.is_some() || parent.is_none()))
                .then(|| ev.trace().fp(&resolved))
                .flatten();
            let mut err: Option<Box<Error>> = None;
            let git_lint = on_disk && ev.lint_on(LintId::GitInSource);
            let mut saw_git = false;
            let mut f = |p: &Path, t: FileType| -> bool {
                let keep = if !at.accepts(p) {
                    false
                } else if let Some(filter) = filter {
                    if err.is_some() {
                        false
                    } else {
                        match call_filter(ev, filter, &shown(p), t, pos) {
                            Ok(b) => {
                                if b && let Some(a) = &mut accepted {
                                    a.insert(p.to_path_buf());
                                }
                                b
                            }
                            Err(e) => {
                                err = Some(e);
                                false
                            }
                        }
                    }
                } else {
                    true
                };
                if let Some(fp) = &mut fp {
                    fp.entry(p, t, keep);
                }
                if keep && git_lint && store_hooks::is_git_entry(&resolved, p) {
                    saw_git = true;
                }
                keep
            };
            let mut h = Hasher::new(HashAlgo::Sha256);
            let r = at.dump(&mut h, &mut f);
            if let Some(e) = err {
                return Err(e);
            }
            r.map_err(|e| io_error(e, "copying", path, pos))?;
            nar_size = h.bytes_written();
            let h = h.finish();
            if on_disk {
                ev.trace()
                    .tree(&resolved, trace::hash_to_digest(&h), accepted.as_ref(), fp);
            }
            if saw_git {
                purity::git_in_source(ev, path, pos)?;
            }
            if on_disk {
                purity::large_source_copy(ev, path, nar_size, pos)?;
            }
            h
        }
    };
    let sp = make_fixed_output_path(method, &hash, name, &refs).map_err(store_err)?;
    let kind = if method == IngestionMethod::Recursive {
        "tree"
    } else {
        "flat"
    };
    ev.note_store_add(&sp, kind, nar_size, &at, pos);
    if method == IngestionMethod::Recursive {
        if on_disk {
            let at = SrcPath::fs(resolved, accepted.map(Arc::new).or(parent));
            ev.known
                .add_copy(sp.clone(), SourceOrigin::tree(at, hash, nar_size));
        } else {
            // a spool origin's root is itself a member, which no filter was asked about
            if let Some(a) = &mut accepted {
                a.insert(resolved);
            }
            let at = at.filtered(accepted.map(Arc::new).or(parent));
            ev.known
                .add(sp.clone(), SourceOrigin::tree(at, hash, nar_size));
        }
    }
    if let Some(exp) = expected_sp
        && exp != sp
    {
        bail!(
            ErrorKind::Eval,
            pos,
            "store path mismatch in (possibly filtered) path added from '{}'",
            lossy(path)
        );
    }
    Ok(sp)
}

fn filter_source(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut ctx = Vec::new();
    let p = coerce_to_path(
        ev,
        args[1],
        &mut ctx,
        pos,
        "while evaluating the second argument (the path to filter) passed to 'builtins.filterSource'",
    )?;
    let f = ev.force_function(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.filterSource",
    )?;
    let name = base_name(&p).to_vec();
    let sp = add_path(
        ev,
        pos,
        &name,
        &p,
        Some(f),
        IngestionMethod::Recursive,
        None,
        in_store(ev, &p, &ctx),
    )?;
    Ok(store_path_string(ev, &sp))
}

/// Mirrors `newHashAllowEmpty`: an empty hash string means all zeros.
pub(crate) fn parse_sha256_allow_empty(s: &[u8], pos: Pos) -> EvalResult<Hash> {
    let s = utf8(s, pos)?;
    if s.is_empty() {
        let zero = Hash::from_bytes(HashAlgo::Sha256, &[0u8; 32]).expect("32 bytes");
        log::line(&format!(
            "warning: found empty hash, assuming '{}'",
            zero.to_sri()
        ));
        return Ok(zero);
    }
    Hash::parse_any(s, Some(HashAlgo::Sha256)).map_err(|e| Error::boxed(ErrorKind::Eval, e.0, pos))
}

fn path(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let a = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to 'builtins.path'",
    )?;
    let mut ctx = Vec::new();
    let mut p: Option<Vec<u8>> = None;
    let mut name: Vec<u8> = Vec::new();
    let mut filter: Option<Word> = None;
    let mut method = IngestionMethod::Recursive;
    let mut expected: Option<Hash> = None;
    for attr in a.iter() {
        let n: Vec<u8> = ev.interner.name(attr.sym).to_vec();
        let v = attr.cell.get();
        match n.as_slice() {
            b"path" => {
                p = Some(coerce_to_path(
                    ev,
                    v,
                    &mut ctx,
                    attr.pos,
                    "while evaluating the 'path' attribute passed to 'builtins.path'",
                )?)
            }
            b"name" => {
                name = ev
                    .force_string_no_ctx(
                        v,
                        attr.pos,
                        "while evaluating the `name` attribute passed to builtins.path",
                    )?
                    .to_vec()
            }
            b"filter" => {
                filter = Some(ev.force_function(
                    v,
                    attr.pos,
                    "while evaluating the `filter` parameter passed to builtins.path",
                )?)
            }
            b"recursive" => {
                method = if ev.force_bool(
                    v,
                    attr.pos,
                    "while evaluating the `recursive` attribute passed to builtins.path",
                )? {
                    IngestionMethod::Recursive
                } else {
                    IngestionMethod::Flat
                }
            }
            b"sha256" => {
                let s = ev.force_string_no_ctx(
                    v,
                    attr.pos,
                    "while evaluating the `sha256` attribute passed to builtins.path",
                )?;
                expected = Some(parse_sha256_allow_empty(s, attr.pos)?);
            }
            _ => bail!(
                ErrorKind::Eval,
                attr.pos,
                "unsupported argument '{}' to 'builtins.path'",
                lossy(&n)
            ),
        }
    }
    let Some(p) = p else {
        bail!(
            ErrorKind::Eval,
            pos,
            "missing required 'path' attribute in the first argument to 'builtins.path'"
        );
    };
    if name.is_empty() {
        name = base_name(&p).to_vec();
    }
    let in_store = in_store(ev, &p, &ctx);
    let sp = add_path(ev, pos, &name, &p, filter, method, expected, in_store)?;
    Ok(store_path_string(ev, &sp))
}

/// Nix's internal context form: base names, `=` and `!out!` prefixes.
fn ctx_internal(c: &Ctx) -> String {
    let base = |p: &[u8]| lossy(base_name(p)).into_owned();
    match c {
        Ctx::Opaque(p) => base(p),
        Ctx::DrvDeep(d) => format!("={}", base(d)),
        // value/context.cc: one `<output>!` per layer, then the `.drv`'s name
        Ctx::Built { drv, output } => {
            let (layers, root) = runtime::split_built(drv);
            let mut s = format!("!{}!", lossy(output));
            for l in layers {
                s.push_str(&lossy(l));
                s.push('!');
            }
            s.push_str(&base(root));
            s
        }
        Ctx::Impure(m) => lossy(m.bytes()).into_owned(),
    }
}

fn to_file(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let name = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.toFile",
    )?;
    let contents = ev.force_string(
        args[1],
        pos,
        "while evaluating the second argument passed to builtins.toFile",
    )?;
    let mut refs = BTreeSet::new();
    for c in contents.ctx {
        match c {
            Ctx::Impure(_) => {}
            Ctx::Opaque(p) => {
                let sp = StorePath::parse(utf8(p, pos)?)
                    .map_err(|e| Error::boxed(ErrorKind::Eval, e.0, pos))?;
                refs.insert(sp);
            }
            other => bail!(
                ErrorKind::Eval,
                pos,
                "files created by builtins.toFile may not reference derivations, but {} references {}",
                lossy(name),
                ctx_internal(other)
            ),
        }
    }
    let h = hash_bytes(HashAlgo::Sha256, contents.bytes);
    let sp = make_text_path(utf8(name, pos)?, &h, &refs)
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.0, pos))?;
    let o = SourceOrigin::text(contents.bytes.to_vec(), refs);
    ev.note_store_add(&sp, "text", contents.bytes.len() as u64, &o.at, pos);
    ev.known.add(sp.clone(), o);
    Ok(store_path_string(ev, &sp))
}
