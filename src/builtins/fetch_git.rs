//! `builtins.fetchGit` (fetchTree.cc with `isFetchGit`, git.cc). A local
//! working tree without a `ref` or a `rev` is copied the way the flake's own
//! tree is (git.cc:getAccessorFromWorkdir), so the tracing cache follows its
//! files. Any other fetch reads a commit (git.cc:getAccessorFromCommit), of
//! a local repository where it is, else of a clone (`crate::fetch::git`).
//! Only `lfs` and `verifyCommit` still go to Nix's own `fetchGit`.

use crate::sys::OsStrExt;
use std::path::Path;

use super::fetch::{emit_tree_attrs, input_attrs};
use super::store_hooks;
use crate::error;
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::fetch;
use crate::flake::{eval_error, flakeref};
use crate::lint::purity;
use crate::log;
use crate::runtime::Word;
use crate::source::{self, AttrVal, Attrs, Tree, bool_attr, scm, str_attr};

const NULL_REV: &str = "0000000000000000000000000000000000000000";

/// git.cc:GitInputScheme::allowedAttrs.
const ALLOWED: &[&str] = &[
    "url",
    "ref",
    "rev",
    "shallow",
    "submodules",
    "lfs",
    "exportIgnore",
    "lastModified",
    "revCount",
    "narHash",
    "allRefs",
    "name",
    "dirtyRev",
    "dirtyShortRev",
    "verifyCommit",
    "keytype",
    "publicKey",
    "publicKeys",
];

const FLAGS: &[&str] = &[
    "shallow",
    "submodules",
    "lfs",
    "exportIgnore",
    "allRefs",
    "verifyCommit",
];

pub fn fetch_git(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    purity::unhashed_fetch(ev, "fetchGit", args[0], pos)?;
    let mut args = call_attrs(ev, args[0], pos)?;
    let Some(url) = str_attr(&args, "url").map(String::from) else {
        bail!(ErrorKind::Eval, pos, "input attribute 'url' is missing");
    };
    let fixed = match flakeref::fix_git_url(&url) {
        Ok(u) => Some(u),
        Err(e @ error::Error::Unsupported(_)) if ev.pure() => return Err(eval_error(e, pos)),
        Err(error::Error::Unsupported(_)) => None,
        Err(e) => return Err(eval_error(e, pos)),
    };
    let submodules = bool_attr(&args, "submodules").unwrap_or(false);
    if !submodules {
        args.entry("exportIgnore".into())
            .or_insert(AttrVal::Bool(true));
    }
    if ev.pure() {
        check_locked(&args, fixed.as_deref().unwrap_or(&url), pos)?;
    }
    let root = fixed.as_deref().and_then(workdir_root);
    // a local repository is a source on this machine: a pure evaluation
    // reads one only where it may read
    // (docs/explanation/pure-eval-and-local-sources.md)
    let root = match root {
        Some(r) if ev.pure() => {
            let dir = store_hooks::local_dir(ev, r.as_bytes(), pos)?;
            Some(dir.to_string_lossy().into_owned())
        }
        root => root,
    };
    let shown = input_string(&args, fixed.as_deref().unwrap_or(&url)).unwrap_or(url.clone());
    if submodules && bool_attr(&args, "exportIgnore") == Some(true) {
        bail!(
            ErrorKind::Unsupported,
            pos,
            "exportIgnore and submodules are not supported together yet"
        );
    }
    let Some(url) = fixed.filter(|_| !needs_nix(&args)) else {
        return from_nix(ev, &args, &shown, pos);
    };
    let unpinned = !args.contains_key("rev") && !args.contains_key("ref");
    let workdir = root
        .as_deref()
        .map(Path::new)
        .filter(|r| r.join(".git").exists());
    let mut tree = match workdir {
        Some(root) if unpinned => from_workdir(ev, &args, root, pos)?,
        _ => from_commit(ev, &args, &url, workdir, pos)?,
    };
    // fetchTree.cc:emitTreeAttrs with `emptyRevFallback`
    if tree.counter.is_none() {
        tree.attrs
            .entry("revCount".into())
            .or_insert(AttrVal::Int(0));
    }
    check_nar_hash(&args, &tree, &shown, pos)?;
    emit_tree_attrs(ev, &tree, None, pos)
}

/// What only Nix fetches yet: Git LFS files and signed commits.
pub(crate) fn needs_nix(args: &Attrs) -> bool {
    let set = |k| bool_attr(args, k).unwrap_or(false);
    set("lfs")
        || set("verifyCommit")
        || ["publicKey", "publicKeys", "keytype"]
            .iter()
            .any(|k| args.contains_key(*k))
}

/// fetchers.cc:Input::checkLocks for a `narHash` the call gives.
fn check_nar_hash(args: &Attrs, tree: &Tree, shown: &str, pos: Pos) -> EvalResult<()> {
    let (Some(want), Some(got)) = (str_attr(args, "narHash"), str_attr(&tree.attrs, "narHash"))
    else {
        return Ok(());
    };
    let want = source::parse_nar_hash(want)
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
    if want.to_sri() != got {
        bail!(
            ErrorKind::HashMismatch,
            pos,
            "NAR hash mismatch in input '{shown}', expected '{}' but got '{got}'",
            want.to_sri()
        );
    }
    Ok(())
}

/// fetchTree.cc:fetchTree with `isFetchGit`: a string is the URL, and an
/// attribute set has no `type`.
fn call_attrs(ev: &mut Evaluator, arg: Word, pos: Pos) -> EvalResult<Attrs> {
    let w = ev.force(arg)?;
    let Some(a) = w.as_attrs() else {
        let mut ctx = Vec::new();
        let what = "while evaluating the first argument passed to 'fetchGit'";
        let url = ev.coerce_to_string(w, &mut ctx, pos, what, false, false, true)?;
        let url = String::from_utf8_lossy(&url).into_owned();
        return Ok(Attrs::from([("url".into(), AttrVal::Str(url))]));
    };
    if a.get(ev.sym(b"type")).is_some() {
        bail!(ErrorKind::Eval, pos, "unexpected argument 'type'");
    }
    let attrs = input_attrs(ev, a, "fetchGit", pos)?;
    // fetchers.cc:Input::fromAttrs, then git.cc:inputFromAttrs
    if let Some(k) = attrs.keys().find(|k| !ALLOWED.contains(&k.as_str())) {
        bail!(
            ErrorKind::Eval,
            pos,
            "input attribute '{k}' not supported by scheme 'git'"
        );
    }
    if let Some(k) = FLAGS.iter().find(|k| {
        attrs
            .get(**k)
            .is_some_and(|v| !matches!(v, AttrVal::Bool(_)))
    }) {
        bail!(
            ErrorKind::Eval,
            pos,
            "input attribute '{k}' is not a Boolean"
        );
    }
    Ok(attrs)
}

/// fetchers.cc:Input::to_string of the input `args` make.
fn input_string(args: &Attrs, url: &str) -> Option<String> {
    let mut input = args.clone();
    input.insert("type".into(), AttrVal::Str("git".into()));
    input.insert("url".into(), AttrVal::Str(url.into()));
    flakeref::attrs_to_string(input).ok()
}

/// fetchTree.cc:fetchTree: a pure evaluation fetches a `rev` that is not
/// the null revision, or warns and fetches what a `narHash` checks.
fn check_locked(args: &Attrs, url: &str, pos: Pos) -> EvalResult<()> {
    if str_attr(args, "rev").is_some_and(|r| r != NULL_REV) {
        return Ok(());
    }
    let shown = match input_string(args, url) {
        Some(s) => s,
        None => bail!(
            ErrorKind::Unsupported,
            pos,
            "an unlocked Git input '{url}' in pure evaluation mode is not supported"
        ),
    };
    if args.contains_key("narHash") {
        log::line(&format!(
            "warning: Input '{shown}' is unlocked (e.g. lacks a Git revision) but is checked by NAR hash. \
             This is not reproducible and will break after garbage collection or when shared."
        ));
        return Ok(());
    }
    bail!(
        ErrorKind::Eval,
        pos,
        "in pure evaluation mode, 'fetchGit' doesn't fetch unlocked input '{shown}'"
    )
}

/// git.cc:getRepoInfo: the path of a `file:` URL, when it is absolute. It
/// is a working tree when it has a `.git`; any other is a repository.
fn workdir_root(url: &str) -> Option<String> {
    flakeref::file_url_path(url).filter(|p| p.starts_with('/'))
}

/// git.cc:getAccessorFromWorkdir, with fetchTree.cc:emitTreeAttrs'
/// `emptyRevFallback`.
fn from_workdir(ev: &mut Evaluator, args: &Attrs, root: &Path, pos: Pos) -> EvalResult<Tree> {
    let git = scm::named("git").expect("iets knows git");
    let marker = git.root_marker(root);
    ev.trace().stat_now(marker.as_os_str().as_bytes(), true);
    let submodules = bool_attr(args, "submodules").unwrap_or(false);
    let export = bool_attr(args, "exportIgnore").unwrap_or(false);
    let name = str_attr(args, "name").unwrap_or("source");
    let (mut tree, dirty) = source::workdir(ev, git, root, name, export, submodules, pos)?;
    let shallow = bool_attr(args, "shallow").unwrap_or(false);
    // a shallow clone has no count, which Nix reports when it is read
    if !dirty && !shallow && tree.counter.is_none() {
        tree.counter = Some((git, root.to_path_buf()));
    }
    if dirty {
        log::line(&format!("warning: Git tree '{}' is dirty", root.display()));
    }
    if shallow {
        tree.counter = None;
    }
    let a = &mut tree.attrs;
    a.insert("submodules".into(), AttrVal::Bool(submodules));
    a.entry("rev".into())
        .or_insert_with(|| AttrVal::Str(NULL_REV.into()));
    Ok(tree)
}

/// git.cc:getAccessorFromCommit: the tree of `rev`, or of the commit `ref`
/// names, else of the default branch. A local working tree is read where it
/// is, its refs observed. Any other repository is fetched into its clone
/// (`fetch::git`), and without a `rev` the run cannot be cached.
pub(crate) fn from_commit(
    ev: &mut Evaluator,
    args: &Attrs,
    url: &str,
    workdir: Option<&Path>,
    pos: Pos,
) -> EvalResult<Tree> {
    let git = scm::named("git").expect("iets knows git");
    let io = |e: String| Error::boxed(ErrorKind::Io, e, pos);
    let rev = str_attr(args, "rev");
    let r#ref = str_attr(args, "ref");
    if let Some(r) = rev.filter(|r| !is_rev(r)) {
        bail!(ErrorKind::Eval, pos, "invalid Git revision '{r}'");
    }
    if let Some(r) = r#ref.filter(|r| !is_ref_name(r)) {
        bail!(ErrorKind::Eval, pos, "invalid Git branch/tag name '{r}'");
    }
    let (repo, rev, r#ref) = match workdir {
        Some(root) => {
            let r#ref = r#ref.unwrap_or("HEAD");
            let rev = match rev {
                Some(rev) => rev.to_string(),
                None => git
                    .resolve(ev.trace(), root, r#ref)
                    .map_err(io)?
                    .ok_or_else(|| {
                        io(format!(
                            "resolving Git reference '{ref}' in '{}'",
                            root.display()
                        ))
                    })?,
            };
            (root.to_path_buf(), rev, r#ref.to_string())
        }
        None => {
            if rev.is_none() {
                ev.trace().mark_uncacheable();
            }
            let want = fetch::git::Want {
                url,
                rev,
                r#ref,
                all_refs: bool_attr(args, "allRefs").unwrap_or(false),
                local_ok: true,
            };
            let got = fetch::git::open(&want).map_err(io)?;
            (got.repo, got.rev, got.r#ref)
        }
    };
    let export = bool_attr(args, "exportIgnore").unwrap_or(false);
    let submodules = bool_attr(args, "submodules").unwrap_or(false);
    let name = str_attr(args, "name").unwrap_or("source");
    let r = submodules.then_some(r#ref.as_str());
    // a pure evaluation reads a local repository only where `local_dir`
    // allows, which a submodule's URL would get around
    let local_ok = fetch::git::is_local(url) && !ev.pure();
    let tree = source::commit_tree(ev, git, &repo, &rev, name, export, r, local_ok, pos)?;
    let Some(mut tree) = tree else {
        bail!(
            ErrorKind::Io,
            pos,
            "Cannot find Git revision '{rev}' in ref '{ref}' of repository '{url}'!"
        );
    };
    tree.attrs.insert("url".into(), AttrVal::Str(url.into()));
    if bool_attr(args, "shallow").unwrap_or(false) {
        tree.counter = None;
    }
    for k in ["lastModified", "revCount"] {
        if let Some(v) = args.get(k) {
            tree.attrs.insert(k.into(), v.clone());
            tree.counter = tree.counter.filter(|_| k != "revCount");
        }
    }
    Ok(tree)
}

/// A full revision that is not the null one.
fn is_rev(r: &str) -> bool {
    r.len() == 40 && r != NULL_REV && r.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A plain branch or tag name. It cannot start with `-`, so git never reads
/// it as an option.
fn is_ref_name(r: &str) -> bool {
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'+');
    !r.is_empty()
        && !r.starts_with(['-', '/', '.'])
        && !r.ends_with(['/', '.'])
        && !r.contains("..")
        && !r.contains("//")
        && r.bytes().all(plain)
}

/// Nix's `fetchGit` with the call's own attributes. A result with a `rev`
/// asked for follows from it, so only the store path is an observation;
/// any other depends on the repository now, and the run is uncacheable.
fn from_nix(ev: &mut Evaluator, args: &Attrs, shown: &str, pos: Pos) -> EvalResult<Word> {
    let locked = str_attr(args, "rev").is_some_and(|r| r != NULL_REV);
    if !locked {
        ev.trace().mark_uncacheable();
    }
    let (path, nar_hash, attrs) = source::fetch_git_with_nix(args, shown, pos)?;
    let mut input = Attrs::new();
    for k in ["type", "rev", "narHash"] {
        input.extend(attrs.get(k).map(|v| (k.to_string(), v.clone())));
    }
    input.extend(args.get("url").map(|v| ("url".to_string(), v.clone())));
    source::ensure_readable(ev, &path, &nar_hash, &input, pos)?;
    let tree = Tree {
        path,
        attrs,
        subdir: String::new(),
        counter: None,
    };
    emit_tree_attrs(ev, &tree, None, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn only_a_full_lowercase_rev_is_a_rev() {
        let rev = "61c2d57b4c40383bc42bf989900543aaba01d24f";
        assert!(is_rev(rev));
        assert!(!is_rev(NULL_REV));
        assert!(!is_rev(&rev.to_uppercase()));
        assert!(!is_rev(&rev[..7]));
    }

    #[rstest]
    fn a_plain_ref_name_is_read_by_iets(
        #[values("main", "HEAD", "v1.0", "refs/tags/v1", "a_b+c")] r: &str,
    ) {
        assert!(is_ref_name(r));
    }

    #[rstest]
    fn a_ref_name_git_would_refuse_is_not(
        #[values("", "-x", "/a", "a/", "a..b", "a//b", "a b", "a~1", "a^", "x.", ".x")] r: &str,
    ) {
        assert!(!is_ref_name(r));
    }

    #[rstest]
    #[case("/r/a b", Some("/r/a b"))]
    #[case("file:///r/x", Some("/r/x"))]
    #[case("git+file:///r/x", Some("/r/x"))]
    #[case("rel/x", None)]
    #[case("https://x.org/a.git", None)]
    fn a_working_tree_is_a_file_url_of_an_absolute_path(
        #[case] url: &str,
        #[case] root: Option<&str>,
    ) {
        let fixed = flakeref::fix_git_url(url).unwrap();
        assert_eq!(workdir_root(&fixed).as_deref(), root);
    }
}
