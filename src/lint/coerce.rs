//! Silent coercions: path copies, reads through a copy, derivation
//! attribute coercions, `==` across types or on functions, `or` on a
//! non-attrset. Hook: the evaluator, at the coercion site.

use super::{LintId, Where};
use crate::builtins::store_hooks;
use crate::eval::{EvalResult, Evaluator, Pos};
use crate::runtime::{AttrsObj, Value, ValueType, Word};
use crate::trace;

use std::borrow::Cow;

fn lossy(b: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(b)
}

/// After a path was copied to the store by a string coercion. Inside a
/// flake root or a locked input `"${./file}"` is the normal reference:
/// a file is silent, a directory is `dir-copy-flake`, not `path-copy`.
pub fn path_copy(ev: &mut Evaluator, path: &[u8], store_path: &[u8], pos: Pos) -> EvalResult<()> {
    if !ev.lint_on(LintId::PathCopy) && !ev.lint_on(LintId::DirCopyFlake) {
        return Ok(());
    }
    let shown = ev.lint_shown_path(path);
    if ev.flake_tree_of(path).is_some() {
        if !ev.lint_on(LintId::DirCopyFlake)
            || store_hooks::stat(ev, path, true) != trace::Kind::Directory
        {
            return Ok(());
        }
        return ev.lint(LintId::DirCopyFlake, pos, || {
            format!(
                "directory '{}' of the flake tree is copied to the store as '{}' by this string coercion: the tracked files, as the flake machinery kept them; use `lib.fileset.toSource` or `builtins.path {{ path = ...; filter = ...; }}` only to narrow it further",
                shown,
                lossy(store_path)
            )
        });
    }
    if !ev.lint_on(LintId::PathCopy) {
        return Ok(());
    }
    ev.lint(LintId::PathCopy, pos, || {
        format!(
            "path '{}' is copied to the store as '{}' by this string coercion; use `toString` for the name or `builtins.path` for an explicit copy",
            shown,
            lossy(store_path)
        )
    })
}

/// Before `path` is copied: a path that already lies under the store dir,
/// unless it is the flake root's or a locked input's virtual copy.
pub fn store_path_recopy(ev: &mut Evaluator, path: &[u8], pos: Pos) -> EvalResult<()> {
    if !ev.lint_on(LintId::StorePathRecopy) {
        return Ok(());
    }
    let mut prefix = ev.store_dir.clone();
    prefix.push(b'/');
    if !path.starts_with(&prefix) || ev.flake_tree_of(path).is_some() {
        return Ok(());
    }
    ev.lint(LintId::StorePathRecopy, pos, || {
        format!(
            "'{}' is already a store path and is copied again, which doubles the hash in its name; use `builtins.storePath` or keep it a string",
            lossy(path)
        )
    })
}

/// The store path `path` lies under, when this evaluation copied it.
fn copied_root(ev: &Evaluator, path: &[u8]) -> Option<Vec<u8>> {
    let (end, k) = ev.known.under(path)?;
    k.copy.then(|| path[..end].to_vec())
}

/// A read of `path` (readFile, readDir, import, ...) that goes through a
/// store copy this evaluation made.
pub fn read_through_copy(ev: &mut Evaluator, path: &[u8], pos: Pos) -> EvalResult<()> {
    if !ev.lint_on(LintId::ReadThroughCopy) {
        return Ok(());
    }
    let Some(root) = copied_root(ev, path) else {
        return Ok(());
    };
    let local = ev
        .src_to_store
        .iter()
        .find(|(_, sp)| *sp == root.as_slice())
        .map(|(l, _)| l);
    ev.lint(LintId::ReadThroughCopy, pos, || {
        let orig = local.map_or(String::new(), |l| format!(" (copied from '{}')", lossy(&l)));
        format!(
            "'{}' is read through the store copy '{}'{orig}; read the original path instead",
            lossy(path),
            lossy(&root)
        )
    })
}

/// The position the value lints on derivation attributes
/// (`impure-path-in-drv`, `absolute-path-in-drv`, `fod`) fall back to
/// when the attribute's own position is out of scope: nixpkgs's
/// `mkDerivation` and `fetchurl` bind the attributes, but the offending
/// value is the user's whatever the wrapper, so the user's derivation
/// must be found elsewhere. The `derivationStrict` call, the `name`
/// attribute, then the first attribute (in the set's order) whose
/// position lies in a lint root; `Pos::NONE` when none does. The
/// evaluator keeps no call trace, so a derivation whose attribute set
/// holds no position from the root (a nixpkgs fetcher call) stays silent.
pub fn drv_fallback(ev: &mut Evaluator, attrs: &'static AttrsObj, pos: Pos, name_pos: Pos) -> Pos {
    if !ev.lint_config().scoped() {
        return Pos::NONE;
    }
    for p in [pos, name_pos] {
        if ev.where_of(p) == Where::Root {
            return p;
        }
    }
    for a in attrs.iter() {
        if ev.where_of(a.pos) == Where::Root {
            return a.pos;
        }
    }
    Pos::NONE
}

/// The site of the derivation itself, for the encoding lints
/// (`drv-bool-null`, `drv-list`): the `derivationStrict` call when the
/// scope sees it (a direct call), else the `builder` attribute's
/// position when the scope sees that. `derivation` is a wrapper in
/// `«derivation-internal»`, so a direct `derivation { }` call is known
/// by its `builder` binding. `None` when neither is seen: the derivation
/// was made by a wrapper out of scope, such as nixpkgs's `mkDerivation`,
/// and a list or Boolean there is the wrapper's contract, not a coercion
/// the user chose.
pub fn drv_owner(ev: &mut Evaluator, pos: Pos, builder_pos: Pos) -> Option<Pos> {
    if !ev.lint_config().scoped() {
        return Some(pos);
    }
    [pos, builder_pos]
        .into_iter()
        .find(|&p| ev.where_of(p) != Where::Elsewhere)
}

/// The site a derivation-attribute lint reports at: the attribute's own
/// position when the scope keeps it, else `fallback`.
pub fn drv_attr_pos(ev: &mut Evaluator, attr_pos: Pos, fallback: Pos) -> Pos {
    if !ev.lint_config().scoped() || ev.where_of(attr_pos) != Where::Elsewhere {
        attr_pos
    } else {
        fallback
    }
}

/// Nix's own Boolean derivation attributes: their `"1"`/`""` encoding is
/// the protocol, so `drv-bool-null` never reports them.
const PROTOCOL_BOOLS: &[&str] = &[
    "__structuredAttrs",
    "__ignoreNulls",
    "__impureHostDeps",
    "__propagatedImpureHostDeps",
    "__sandboxProfile",
    "__noChroot",
    "__darwinAllowLocalNetworking",
    "allowSubstitutes",
    "preferLocalBuild",
];

/// A derivation attribute value, forced, about to be coerced into the
/// environment.
pub fn drv_value(ev: &mut Evaluator, key: &str, w: Word, attr_pos: Pos) -> EvalResult<()> {
    match w.view() {
        Value::Bool(_) | Value::Null if PROTOCOL_BOOLS.contains(&key) => Ok(()),
        Value::Bool(b) => ev.lint(LintId::DrvBoolNull, attr_pos, || {
            format!(
                "derivation attribute '{key}' is the Boolean {b}, which becomes \"{}\" in the environment",
                if b { "1" } else { "" }
            )
        }),
        Value::Null => ev.lint(LintId::DrvBoolNull, attr_pos, || {
            format!(
                "derivation attribute '{key}' is null, which becomes \"\" in the environment; set `__ignoreNulls` to drop it"
            )
        }),
        Value::List(_) if key != "outputs" => ev.lint(LintId::DrvList, attr_pos, || {
            format!(
                "derivation attribute '{key}' is a list, which is space-joined into one environment string"
            )
        }),
        _ => Ok(()),
    }
}

/// `==` or `!=` on the forced operands `a` and `b`. A `null` on either
/// side is the language's test for absence, so it is no mismatch.
pub fn eq(ev: &mut Evaluator, a: Word, b: Word, pos: Pos) -> EvalResult<()> {
    let (ta, tb) = (a.value_type(), b.value_type());
    if ta == ValueType::Function || tb == ValueType::Function {
        ev.lint(LintId::EqFunction, pos, || {
            "`==` on a function is never true: functions compare by identity of one temporary"
                .to_string()
        })?;
    }
    let numeric = |t| matches!(t, ValueType::Int | ValueType::Float);
    let null = ta == ValueType::Null || tb == ValueType::Null;
    if ta != tb && !null && !(numeric(ta) && numeric(tb)) {
        ev.lint(LintId::EqTypeMismatch, pos, || {
            format!(
                "`==` compares {} with {}: values of different types are never equal, so the result is constant",
                ta.name(),
                tb.name()
            )
        })?;
    }
    Ok(())
}

/// `x.a or d` reached a subject that is not an attrset.
pub fn or_non_attrs(ev: &mut Evaluator, v: Word, pos: Pos) -> EvalResult<()> {
    ev.lint(LintId::OrNonAttrs, pos, || {
        format!(
            "`or` falls back because the subject is {}, not an attrset",
            v.value_type().name()
        )
    })
}
