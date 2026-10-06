//! Purity and reproducibility: impure builtins, unhashed fetches, import
//! from derivation, fixed-output derivations, large or git-bearing source
//! copies. Hook: the evaluator at the call; `currentTime` is a thunk the
//! globals hold, with no position of its own, so the compiler reports
//! the reference.

use super::{Level, LintId, Where};
use crate::eval::{EvalResult, Evaluator, Pos};
use crate::runtime::{Cell, Ctx, Kind, Taint, Value, Word};

/// The message for a reference to the global `name`, when it is `currentTime`.
pub fn current_time(name: &[u8]) -> Option<String> {
    (name.strip_prefix(b"__").unwrap_or(name) == b"currentTime")
        .then(|| "`currentTime` is impure: the result changes with every run".into())
}

pub fn get_env(ev: &mut Evaluator, name: &[u8], pos: Pos) -> EvalResult<()> {
    ev.lint(LintId::ImpureBuiltin, pos, || {
        format!(
            "`getEnv \"{}\"` is impure: the result depends on the environment",
            String::from_utf8_lossy(name)
        )
    })
}

const HASH_ATTRS: &[&[u8]] = &[b"sha256", b"hash", b"narHash", b"rev", b"outputHash"];

/// A fetcher call: `arg` is a URL string or an attrset; either way a hash
/// (or a `rev`) must pin the result.
pub fn unhashed_fetch(ev: &mut Evaluator, builtin: &str, arg: Word, pos: Pos) -> EvalResult<()> {
    if !ev.lint_on(LintId::UnhashedFetch) {
        return Ok(());
    }
    let w = ev.force(arg)?;
    let pinned = match w.view() {
        Value::Attrs(a) => a.iter().any(|attr| {
            let n = ev.interner.name(attr.sym);
            HASH_ATTRS.contains(&n)
        }),
        Value::String(_) => false,
        _ => return Ok(()),
    };
    if pinned {
        return Ok(());
    }
    ev.lint(LintId::UnhashedFetch, pos, || {
        format!(
            "`{builtin}` is called without a hash, narHash or rev: the result is not reproducible"
        )
    })
}

/// `import` of a string whose context names a derivation.
pub fn ifd(ev: &mut Evaluator, ctx: &[Ctx], pos: Pos) -> EvalResult<()> {
    let drv = ctx.iter().find_map(|c| match c {
        Ctx::DrvDeep(d) => Some(*d),
        // the `.drv` at the bottom of the chain is the one to build first
        Ctx::Built { drv: d, .. } => Some(crate::runtime::split_built(d).1),
        Ctx::Opaque(_) | Ctx::Impure(_) => None,
    });
    let Some(drv) = drv else {
        return Ok(());
    };
    ev.lint(LintId::Ifd, pos, || {
        format!(
            "import from derivation: this import needs '{}' built first",
            String::from_utf8_lossy(drv)
        )
    })
}

pub fn fod(ev: &mut Evaluator, name: &str, mode: &str, algo: &str, pos: Pos) -> EvalResult<()> {
    ev.lint(LintId::Fod, pos, || {
        format!("derivation '{name}' is fixed-output ({mode}, {algo})")
    })
}

/// A recursive copy of `path` whose NAR has `size` bytes.
pub fn large_source_copy(ev: &mut Evaluator, path: &[u8], size: u64, pos: Pos) -> EvalResult<()> {
    let max = ev.lint_config().max_bytes;
    if size <= max {
        return Ok(());
    }
    let shown = ev.lint_shown_path(path);
    ev.lint(LintId::LargeSourceCopy, pos, || {
        format!(
            "copying '{}' to the store takes {} bytes of NAR, over the {} byte limit; filter the source",
            shown,
            size,
            max
        )
    })
}

/// A string coercion copied the directory `path` whole: no filter, and not
/// a tree the flake machinery already turned into a tracked file set.
pub fn dir_copy_unfiltered(ev: &mut Evaluator, path: &[u8], pos: Pos) -> EvalResult<()> {
    ev.lint(LintId::DirCopyUnfiltered, pos, || {
        format!(
            "directory '{}' is copied to the store whole, untracked and ignored files included; use `builtins.path {{ path = ...; filter = ...; }}`",
            String::from_utf8_lossy(path)
        )
    })
}

/// The nearest project root at or above `path` (`crate::project`). Every
/// probe is a `Stat` observation, so a new marker is a cache miss.
fn project_root_of(ev: &Evaluator, path: &[u8]) -> Option<Vec<u8>> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let dir = crate::compile::canon_path(path);
    let at = std::path::Path::new(std::ffi::OsStr::from_bytes(&dir));
    let root = crate::project::find_with(at, |m| {
        ev.trace().stat_now(m.as_os_str().as_bytes(), true) != crate::trace::Kind::Missing
    })?;
    Some(root.into_os_string().into_vec())
}

/// Derivation `drv_name` received attribute `key` whose value carries the
/// text of the non-store path `path`: `absolute-path-in-drv` under a
/// project root, `impure-path-in-drv` elsewhere.
fn path_in_drv(
    ev: &mut Evaluator,
    drv_name: &str,
    key: &str,
    path: &[u8],
    attr_pos: Pos,
) -> EvalResult<()> {
    match project_root_of(ev, path) {
        // the root is not in the message: it is the working tree, which the
        // reader is in, and naming it makes the text differ per checkout
        Some(_) => ev.lint(LintId::AbsolutePathInDrv, attr_pos, || {
            format!(
                "derivation '{drv_name}': attribute '{key}' holds the absolute path '{}' of this project; interpolate the path to copy it, or pass a relative name",
                String::from_utf8_lossy(path)
            )
        }),
        None => ev.lint(LintId::ImpurePathInDrv, attr_pos, || {
            format!(
                "derivation '{drv_name}': attribute '{key}' holds the checkout path '{}', so the .drv depends on where the repository lives; interpolate the path to copy it to the store",
                String::from_utf8_lossy(path)
            )
        }),
    }
}

/// The level of `id`, a lint on the metadata of a tree, for the tree
/// `source`: `self`, or `inputs.<node>` by lock node name. The tree a value
/// comes from decides, not the site: a locked input changes at a lock update
/// only, so it follows the scope rule like a site outside the roots.
pub fn tree_level(ev: &Evaluator, id: LintId, source: &str) -> Level {
    let cfg = ev.lint_config();
    let w = match source.strip_prefix("inputs.") {
        None => Where::Root,
        Some(node) => cfg.input_index(node).map_or(Where::Elsewhere, Where::Input),
    };
    cfg.site_level(id, w)
}

/// The evaluation reads `revCount` of the tree `source`. `pos` is the
/// selection, or `Pos::NONE` when the read forced the lazy value (`toJSON`,
/// `attrValues`, a computed name): native code made that value and the
/// evaluator keeps no call trace, so that finding has no site and the
/// message alone names the tree. The tree gives the level, as for
/// `marks_in_drv`, so a site outside the roots reports too.
pub fn rev_count(ev: &mut Evaluator, source: &str, pos: Pos) -> EvalResult<()> {
    let id = LintId::RevCount;
    let level = tree_level(ev, id, source);
    ev.lint_site(id, pos, Some(level), Some(source), || {
        format!(
            "'{source}.revCount' is deprecated: the count of all revisions is expensive to compute and absent on a shallow clone, and iets plans to remove it; use 'lastModifiedDate' and 'shortRev' for a version"
        )
    })
}

/// A selection at `pos` reads `cell` under the name `revCount`. When the
/// cell holds the lazy count of a tree, the finding goes to `pos` and the
/// count is computed here. The thunk stays unforced on purpose: it names
/// its tree for the next selection, which a plain integer cannot, and a
/// read that is not a selection still reports when it forces the thunk.
pub fn count_read(ev: &mut Evaluator, cell: &Cell, pos: Pos) -> EvalResult<Word> {
    let w = cell.get();
    if w.is_thunk() && w.thunk_kind() == Kind::ThunkApp2 {
        let t = w.as_thunk_app2();
        // `code` before `state`: a force takes the state first and puts the
        // value in `code` after, so an unforced state proves the function
        let (f, of) = (t.code.get(), t.state.get());
        if w.thunk_unforced(of)
            && crate::flake::fetch::is_count(f)
            && let Some(tree) = t.arg1.as_string()
        {
            rev_count(ev, &String::from_utf8_lossy(tree.bytes), pos)?;
            return ev.apply(f, &[Word::NULL, of], pos);
        }
    }
    ev.force_cell(cell)
}

/// The marks in `ctx`, the context that the value of attribute `key` gave
/// derivation `drv_name`. The tree metadata (`self.rev`) is one finding
/// that names them all, at the strictest of their levels. `site` is
/// `Pos::NONE` when the derivation has no position the scope sees: the
/// level of this lint comes from the tree, so the finding then goes to
/// `attr_pos`, out of scope or not.
// `derivationStrict` tests each attribute of each derivation: the report
// stays out of that loop
#[cold]
pub fn marks_in_drv(
    ev: &mut Evaluator,
    drv_name: &str,
    key: &str,
    ctx: &[Ctx],
    site: Pos,
    attr_pos: Pos,
) -> EvalResult<()> {
    let paths = ev.lint_on(LintId::ImpurePathInDrv) || ev.lint_on(LintId::AbsolutePathInDrv);
    let mut metas = Vec::new();
    for c in ctx {
        match *c {
            Ctx::Impure(Taint::Path(p)) if paths => path_in_drv(ev, drv_name, key, p, site)?,
            Ctx::Impure(Taint::Meta(m)) => metas.push(String::from_utf8_lossy(m)),
            _ => {}
        }
    }
    metas.sort();
    metas.dedup();
    let id = LintId::TreeMetaInDrv;
    let source =
        |m: &std::borrow::Cow<str>| tree_level(ev, id, m.rsplit_once('.').map_or(m, |(s, _)| s));
    let levels = metas.iter().map(source);
    let Some(level) = levels.max() else {
        return Ok(());
    };
    let site = if site.is_none() { attr_pos } else { site };
    ev.lint_site(id, site, Some(level), None, || {
        let when = match metas.iter().any(|m| m.starts_with("self.")) {
            true => "commit or edit",
            false => "update of the lock file",
        };
        format!(
            "derivation '{drv_name}': attribute '{key}' holds '{}', a value that changes with every {when}, so this derivation and all that depend on it are built again each time; keep the value out of the build, or read it at run time",
            metas.join("', '")
        )
    })
}

/// A recursive copy of `path` whose top level holds a `.git` entry.
pub fn git_in_source(ev: &mut Evaluator, path: &[u8], pos: Pos) -> EvalResult<()> {
    ev.lint(LintId::GitInSource, pos, || {
        format!(
            "copying '{}' to the store includes its `.git` entry; filter the source or use a git fetcher",
            String::from_utf8_lossy(path)
        )
    })
}
