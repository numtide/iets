//! `builtins.fetchTree` for locked inputs (fetchTree.cc, fetchers.cc): the
//! attrs the evaluator passes become a `source::SourceRef`, which resolves
//! to a tree, and the result attrset mirrors `emitTreeAttrs`. A `path` input
//! is a virtual copy of the local directory, like the root tree; everything
//! else is locked, and `crate::source` fetches it.

use std::path::Path;

use crate::builtins::{paths, store_hooks};
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::lint::purity::{self, tree_level};
use crate::lint::{Level, LintId};
use crate::runtime::{Ctx, Taint, Value, Word};
use crate::source::{
    self, AttrVal, Attrs, SourceRef, Tree, bool_attr, int_attr, last_modified_date, parse_nar_hash,
    scm, str_attr, to_url,
};
use crate::store::hash::{Hash, HashAlgo};
use crate::store::path::IngestionMethod;
use crate::syntax::symbol::Sym;
use crate::{provenance, treestore};

use super::flakeref;

/// The attrs `fetchTree` accepts: strings and paths, booleans, non-negative
/// integers (fetchTree.cc).
fn collect_attrs(ev: &mut Evaluator, w: Word, pos: Pos) -> EvalResult<Attrs> {
    let w = ev.force(w)?;
    let Some(a) = w.as_attrs() else {
        bail!(
            ErrorKind::Unsupported,
            pos,
            "iets: 'fetchTree' with a URL argument is not supported; pass the locked attribute set"
        );
    };
    let mut out = Attrs::new();
    for attr in a.iter() {
        let name = String::from_utf8_lossy(ev.interner.name(attr.sym)).into_owned();
        if name == "__final" {
            continue;
        }
        let v = ev.force(attr.cell.get())?;
        let val = match v.view() {
            Value::String(s) => AttrVal::Str(String::from_utf8_lossy(s.bytes).into_owned()),
            Value::Path(p) => AttrVal::Str(String::from_utf8_lossy(p.path).into_owned()),
            Value::Bool(b) => AttrVal::Bool(b),
            Value::Int(n) if n >= 0 => AttrVal::Int(n as u64),
            Value::Int(n) => bail!(
                ErrorKind::Eval,
                pos,
                "negative value given for 'fetchTree' argument '{name}': {n}"
            ),
            _ => bail!(
                ErrorKind::Type,
                pos,
                "argument '{name}' to 'fetchTree' is {} while a string, Boolean or integer is expected",
                v.value_type().type_of()
            ),
        };
        out.insert(name, val);
    }
    // fetchTree.cc:158 (`allowNameArgument` is false for fetchTree), before the
    // attrs reach `Input::fromAttrs`, which is where the missing `type` is seen
    if out.contains_key("name") {
        bail!(
            ErrorKind::Eval,
            pos,
            "argument 'name' isn\u{2019}t supported in call to 'fetchTree'"
        );
    }
    if !out.contains_key("type") {
        bail!(
            ErrorKind::Eval,
            pos,
            "argument 'type' is missing in call to 'fetchTree'"
        );
    }
    Ok(out)
}

/// fetchTree.cc:fetchTree: a pure evaluation refuses an input that is not
/// locked (a `narHash` for `path` and `tarball`, a `rev` for the rest). The
/// message names the input the way `Input::to_string` does
/// (`flakeref::attrs_to_string`); for `git` (fetchTree adds `?shallow=1`) and
/// what iets cannot show, it is unsupported and classic mode falls back.
fn check_locked(attrs: &Attrs, pos: Pos) -> EvalResult<()> {
    let ty = str_attr(attrs, "type").unwrap_or("");
    let locked = match ty {
        "path" | "tarball" => attrs.contains_key("narHash"),
        _ => attrs.contains_key("rev"),
    };
    if locked {
        return Ok(());
    }
    // fetchTree.cc only warns on a `narHash`
    let shown = match ty {
        _ if attrs.contains_key("narHash") => None,
        "path" | "tarball" | "github" | "gitlab" => flakeref::attrs_to_string(attrs.clone()).ok(),
        _ => None,
    };
    match shown {
        Some(s) => bail!(
            ErrorKind::Eval,
            pos,
            "in pure evaluation mode, 'fetchTree' doesn't fetch unlocked input '{s}'"
        ),
        None => bail!(
            ErrorKind::Unsupported,
            pos,
            "iets: an unlocked '{ty}' input in pure evaluation mode is not supported"
        ),
    }
}

pub fn fetch_tree(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    purity::unhashed_fetch(ev, "fetchTree", args[0], pos)?;
    let mut attrs = collect_attrs(ev, args[0], pos)?;
    if ev.pure() {
        check_locked(&attrs, pos)?;
    }
    let ty = str_attr(&attrs, "type").unwrap_or("").to_string();
    let tree = match ty.as_str() {
        "path" => {
            let Some(path) = str_attr(&attrs, "path") else {
                bail!(
                    ErrorKind::Eval,
                    pos,
                    "fetchTree: 'path' attribute is missing"
                );
            };
            if !path.starts_with('/') {
                bail!(
                    ErrorKind::Eval,
                    pos,
                    "cannot fetch input '{}' because it uses a relative path",
                    to_url(&attrs)
                );
            }
            let (hash, mtime) = match store_hooks::locate(ev, path.as_bytes()) {
                None => source::path_tree(ev, Path::new(path), pos)?,
                // a store tree's mtimes are all 1, so that is its lastModified
                Some(_) => {
                    let sp = paths::add_path(
                        ev,
                        pos,
                        b"source",
                        path.as_bytes(),
                        None,
                        IngestionMethod::Recursive,
                        None,
                        None,
                    )?;
                    let nar = ev.known.origin(&sp).and_then(|o| o.nar);
                    (nar.expect("add_path records a recursive copy").0, 1)
                }
            };
            if let Some(expected) = str_attr(&attrs, "narHash") {
                let exp = parse_nar_hash(expected)
                    .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
                if exp != hash {
                    let msg = format!(
                        "NAR hash mismatch in input '{}', expected '{}' but got '{}'",
                        to_url(&attrs),
                        exp.to_sri(),
                        hash.to_sri()
                    );
                    let sym = ev.sym(b"narHash");
                    let at = ev
                        .force(args[0])?
                        .as_attrs()
                        .and_then(|a| a.get(sym))
                        .map(|a| a.pos);
                    let at = at.map(|p| {
                        provenance::describe_pos(ev, p).unwrap_or_else(|| ev.describe_pos(p))
                    });
                    return Err(Error::hash_mismatch(msg, &hash, at, pos));
                }
            }
            attrs.insert("narHash".into(), AttrVal::Str(hash.to_sri()));
            attrs
                .entry("lastModified".into())
                .or_insert(AttrVal::Int(mtime.max(0) as u64));
            let path = treestore::store_path("source", &hash)
                .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
            Tree {
                path,
                attrs,
                subdir: String::new(),
                counter: None,
            }
        }
        "github" | "gitlab" | "sourcehut" | "git" | "tarball" | "hg" => {
            source::tree(ev, &SourceRef::Locked(attrs), pos)?
        }
        other => bail!(
            ErrorKind::Unsupported,
            pos,
            "iets: fetchTree input type '{other}' is not supported"
        ),
    };
    let name = str_attr(&tree.attrs, "narHash").and_then(|h| ev.input_names.get(h));
    if let Some(name) = &name {
        ev.known.name_flake(&tree.path, name.clone());
    }
    let source = name.map(|n| format!("inputs.{n}"));
    emit_tree_attrs(ev, &tree, source.as_deref(), pos)
}

/// fetchTree.cc:fetch for `builtins.fetchTarball`: with a `sha256` the store
/// path is known before the download, so it is the same fetch as a locked
/// `tarball` input. Without one it is not, and the call is unsupported.
pub fn fetch_tarball_builtin(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    purity::unhashed_fetch(ev, "fetchTarball", args[0], pos)?;
    let w = ev.force(args[0])?;
    let (mut url, mut sha256, mut name) = (None, None, "source".to_string());
    if let Some(a) = w.as_attrs() {
        for attr in a.iter() {
            let n = String::from_utf8_lossy(ev.interner.name(attr.sym)).into_owned();
            let what = match n.as_str() {
                "url" => "while evaluating the url we should fetch",
                "sha256" => "while evaluating the sha256 of the content we should fetch",
                "name" => "while evaluating the name of the content we should fetch",
                _ => bail!(
                    ErrorKind::Eval,
                    pos,
                    "unsupported argument '{n}' to 'fetchTarball'"
                ),
            };
            let v = ev.force_string_no_ctx(attr.cell.get(), pos, what)?;
            let v = String::from_utf8_lossy(v).into_owned();
            match n.as_str() {
                "url" => url = Some(v),
                "sha256" => sha256 = Some(v),
                _ => name = v,
            }
        }
    } else {
        let v = ev.force_string_no_ctx(w, pos, "while evaluating the url we should fetch")?;
        url = Some(String::from_utf8_lossy(v).into_owned());
    }
    let Some(url) = url else {
        bail!(ErrorKind::Eval, pos, "'url' argument required");
    };
    let url = match url.strip_prefix("channel:") {
        Some(c) => format!("https://channels.nixos.org/{c}/nixexprs.tar.xz"),
        None => url,
    };
    if ev.pure() && sha256.is_none() {
        bail!(
            ErrorKind::Eval,
            pos,
            "in pure evaluation mode, 'fetchTarball' requires a 'sha256' argument"
        );
    }
    let Some(hash) = sha256
        .filter(|h| !h.is_empty())
        .and_then(|h| Hash::parse_any(&h, Some(HashAlgo::Sha256)).ok())
    else {
        ev.trace().mark_uncacheable();
        bail!(
            ErrorKind::Unsupported,
            pos,
            "iets: builtins.fetchTarball without a sha256 is not supported"
        );
    };
    // not `source::tree`: `fetchTarball` names the entry itself, and the
    // input store's entry names are what an upload looks up
    let sp = treestore::store_path(&name, &hash)
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
    let attrs = Attrs::from([
        ("type".into(), AttrVal::Str("tarball".into())),
        ("url".into(), AttrVal::Str(url)),
        ("narHash".into(), AttrVal::Str(hash.to_sri())),
    ]);
    source::ensure_readable(ev, &sp, &attrs, pos)?;
    let full = ev.arena.alloc_bytes(sp.to_full_string().as_bytes());
    let ctx = ev.arena.alloc_slice(&[Ctx::Opaque(full)]);
    Ok(ev.arena.mk_string_static(full, ctx))
}

const COUNT: &str = "revCountRaw";

/// `f` is the function of the thunk that `emit_tree_attrs` makes for
/// `revCount`. No builtin has the name, so no expression can make one.
pub fn is_count(f: Word) -> bool {
    !f.is_thunk() && matches!(f.view(), Value::PrimOp(op) if op.name == COUNT)
}

/// `revCount` is a thunk over this, so the SCM counts and `rev-count` reports
/// only when the evaluation reads the value, as in Nix 2.35
/// (fetchTree.cc:emitLazyAttrThunk).
/// `args[0]` is the name of the tree when the lint is on for it. `args[1]` is
/// the count when the lock holds it, else the name of the SCM, the root and
/// the revision.
fn rev_count(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    if let Some(tree) = args[0].as_string() {
        purity::rev_count(ev, &String::from_utf8_lossy(tree.bytes), pos)?;
    }
    if args[1].as_int().is_some() {
        return Ok(args[1]);
    }
    let mut of = Vec::with_capacity(3);
    for c in ev.force_list(args[1], pos, "")?.cells() {
        let s = ev.force_string(c.get(), pos, "")?;
        of.push(String::from_utf8_lossy(s.bytes).into_owned());
    }
    let scm = of.first().and_then(|n| scm::named(n));
    let (Some(scm), [_, root, rev]) = (scm, of.as_slice()) else {
        bail!(ErrorKind::Eval, pos, "iets: revCount of an unknown tree");
    };
    let n = scm::rev_count(ev.trace(), scm, Path::new(root), rev)
        .map_err(|e| Error::boxed(ErrorKind::Io, e, pos))?;
    Ok(ev.mk_int(n as i64))
}

/// fetchTree.cc:emitTreeAttrs with `emptyRevFallback = false`; `forceDirty`
/// is expressed by the caller leaving out `rev`/`revCount`. `source` names
/// the tree (`self`, `inputs.<node>`). Each string that changes with the
/// tree gets the mark `tree-meta-in-drv` reads, and `revCount` is a thunk
/// that reports `rev-count`, when the lint is on for the tree.
pub fn emit_tree_attrs(
    ev: &mut Evaluator,
    tree: &Tree,
    source: Option<&str>,
    pos: Pos,
) -> EvalResult<Word> {
    let (sp, a) = (&tree.path, &tree.attrs);
    let mut entries: Vec<(Sym, Pos, Word)> = Vec::with_capacity(10);
    let full = ev.arena.alloc_bytes(sp.to_full_string().as_bytes());
    let ctx = ev.arena.alloc_slice(&[Ctx::Opaque(full)]);
    let out_path = ev.arena.mk_string_static(full, ctx);
    entries.push((ev.s.out_path, pos, out_path));
    let counted = source.filter(|s| tree_level(ev, LintId::RevCount, s) != Level::Off);
    let source = source.filter(|s| tree_level(ev, LintId::TreeMetaInDrv, s) != Level::Off);
    let mut push_str = |ev: &mut Evaluator, name: &[u8], v: &str| {
        let w = match source {
            Some(s) => {
                let m = ev.arena.alloc_concat(&[s.as_bytes(), b".", name]);
                ev.arena
                    .mk_string_ctx(v.as_bytes(), &[Ctx::Impure(Taint::Meta(m))])
            }
            None => ev.mk_string(v.as_bytes()),
        };
        entries.push((ev.sym(name), pos, w));
    };
    if let Some(h) = str_attr(a, "narHash") {
        push_str(ev, b"narHash", h);
    }
    if let Some(rev) = str_attr(a, "rev") {
        push_str(ev, b"rev", rev);
        push_str(ev, b"shortRev", &rev[..rev.len().min(7)]);
    }
    if let Some(d) = str_attr(a, "dirtyRev") {
        push_str(ev, b"dirtyRev", d);
        push_str(
            ev,
            b"dirtyShortRev",
            str_attr(a, "dirtyShortRev").unwrap_or(""),
        );
    }
    if let Some(t) = int_attr(a, "lastModified") {
        push_str(ev, b"lastModifiedDate", &last_modified_date(t as i64));
        let w = ev.mk_int(t as i64);
        entries.push((ev.sym(b"lastModified"), pos, w));
    }
    let of = match (&tree.counter, tree.rev()) {
        (Some((scm, root)), Some(rev)) => {
            let root = root.to_string_lossy();
            let of = [scm.name(), &root, rev].map(|s| ev.mk_string(s.as_bytes()));
            Some(ev.mk_list(&of))
        }
        _ => int_attr(a, "revCount").map(|n| ev.mk_int(n as i64)),
    };
    if let Some(mut w) = of {
        if tree.counter.is_some() || counted.is_some() {
            let name = counted.map_or(Word::NULL, |s| ev.mk_string(s.as_bytes()));
            let count_sym = ev.sym(COUNT.as_bytes());
            let op = ev.arena.mk_primop(COUNT, 2, rev_count, count_sym);
            w = ev.arena.mk_thunk_app2(op, name, w);
        }
        entries.push((ev.sym(b"revCount"), pos, w));
    }
    if str_attr(a, "type") == Some("git") {
        let w = Word::from_bool(bool_attr(a, "submodules").unwrap_or(false));
        entries.push((ev.sym(b"submodules"), pos, w));
    }
    ev.mk_attrs(&mut entries, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::cache::{deserialize, serialize};
    use crate::eval::EvalConfig;
    use crate::lint::Config;
    use crate::lint::allow::Allows;
    use crate::session::Session;
    use crate::store::path::StorePath;

    fn tree(ev: &mut Evaluator, source: &str) -> Word {
        let path = StorePath::parse("/nix/store/00000000000000000000000000000000-source").unwrap();
        let attrs = Attrs::from([
            ("narHash".into(), AttrVal::Str("sha256-x".into())),
            ("rev".into(), AttrVal::Str("a".repeat(40))),
            ("lastModified".into(), AttrVal::Int(86400)),
            ("revCount".into(), AttrVal::Int(7)),
        ]);
        let tree = Tree {
            path,
            attrs,
            subdir: String::new(),
            counter: None,
        };
        emit_tree_attrs(ev, &tree, Some(source), Pos::NONE).unwrap()
    }

    /// `f tree` as a string: its bytes and the names of its marks.
    fn marks(ev: &mut Evaluator, tree: Word, f: &str) -> (String, Vec<String>) {
        let f = ev.eval_string(f, b"/").unwrap();
        let v = ev.apply(f, &[tree], Pos::NONE).unwrap();
        let s = ev.force_string(v, Pos::NONE, "").unwrap();
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let names = s.ctx.iter().map(|c| match c {
            Ctx::Impure(Taint::Meta(m)) => text(m),
            other => panic!("{f:?}: {other:?}"),
        });
        (text(s.bytes), names.collect())
    }

    #[test]
    fn the_mark_follows_the_string_like_store_context() {
        let mut ev = Evaluator::new();
        let t = tree(&mut ev, "self");
        for (f, want) in [
            ("t: t.rev", "self.rev"),
            ("t: t.narHash", "self.narHash"),
            (
                "t: builtins.substring 0 8 t.lastModifiedDate",
                "self.lastModifiedDate",
            ),
            ("t: \"v\" + t.shortRev", "self.shortRev"),
            ("t: \"1.0-${t.shortRev}\"", "self.shortRev"),
            (
                "t: builtins.concatStringsSep \".\" [ \"1\" t.shortRev ]",
                "self.shortRev",
            ),
            ("t: toString t.rev", "self.rev"),
            ("t: toString [ 1 t.rev ]", "self.rev"),
            (
                "t: builtins.replaceStrings [ \"a\" ] [ \"b\" ] t.rev",
                "self.rev",
            ),
            ("t: builtins.toJSON { r = t.rev; }", "self.rev"),
        ] {
            assert_eq!(marks(&mut ev, t, f).1, [want], "{f}");
        }
        let both = marks(&mut ev, t, "t: t.shortRev + t.narHash").1;
        assert_eq!(both, ["self.narHash", "self.shortRev"]);
        let none: [&str; 0] = [];
        for f in [
            "t: builtins.unsafeDiscardStringContext t.rev",
            // an integer holds no context: docs/reference/lints.md, Limits
            "t: toString t.lastModified",
            "t: toString t.revCount",
        ] {
            assert_eq!(marks(&mut ev, t, f).1, none, "{f}");
        }
    }

    #[test]
    fn nix_sees_no_mark() {
        let mut ev = Evaluator::new();
        let t = tree(&mut ev, "self");
        let f = "t: builtins.toJSON [
          (builtins.getContext t.rev) (builtins.hasContext t.rev)
          (t.rev == builtins.unsafeDiscardStringContext t.rev)
          (builtins.toJSON t.shortRev) { ${t.shortRev} = 1; }
          (builtins.match \"(a+)\" t.rev != null) (builtins.stringLength t.rev)
          t.lastModifiedDate
        ]";
        assert_eq!(
            marks(&mut ev, t, f).0,
            r#"[{},false,true,"\"aaaaaaa\"",{"aaaaaaa":1},true,40,"19700102000000"]"#
        );
    }

    #[test]
    fn the_tree_decides_the_level() {
        let marked = |cfg: Config, source: &str| {
            let mut ev = EvalConfig::new().lints(cfg).build();
            let t = tree(&mut ev, source);
            !marks(&mut ev, t, "t: t.rev").1.is_empty()
        };
        assert!(marked(Config::default(), "self"));
        assert!(!marked(Config::default(), "inputs.nixpkgs"));
        assert!(!marked(
            toml("[lint]\ntree-meta-in-drv = \"off\"\n"),
            "self"
        ));
        let input = "[lint]\ntree-meta-in-drv = \"off\"\n[lint.inputs.nixpkgs]\ntree-meta-in-drv = \"warn\"\n";
        assert!(marked(toml(input), "inputs.nixpkgs"));
        assert!(!marked(toml(input), "inputs.other"));
        assert!(!marked(toml(input), "self"));
        assert!(marked(toml("[lint]\nscope = \"all\"\n"), "inputs.nixpkgs"));
        let family = "[lint.inputs.nixpkgs]\n\"family:purity\" = \"error\"\n";
        assert!(marked(toml(family), "inputs.nixpkgs"));
    }

    fn toml(text: &str) -> Config {
        let mut c = Config::default();
        c.apply_toml(text, Path::new("/r/iets.toml")).unwrap();
        c
    }

    /// The findings of `rev-count`: the tree, then the line and the column
    /// of the site when the finding has one.
    fn found(ev: &Evaluator) -> Vec<String> {
        let text = |r: String| {
            let r = serde_json::from_str::<serde_json::Value>(&r).unwrap();
            let tree = r["raw_msg"].as_str()?.split(".revCount' is").next()?;
            let tree = tree.strip_prefix("[rev-count] '")?;
            Some(match r["line"].as_u64() {
                Some(line) => format!("{tree} {line}:{}", r["column"]),
                None => tree.to_string(),
            })
        };
        ev.lints.records().into_iter().filter_map(text).collect()
    }

    /// `found` after `f` read each tree of `sources`.
    fn reads(cfg: Config, sources: &[&str], f: &str) -> Vec<String> {
        let session = Session::new();
        let _entered = session.enter();
        let mut ev = EvalConfig::new().lints(cfg).build();
        for s in sources {
            let t = tree(&mut ev, s);
            let f = ev.eval_string(f, b"/").unwrap();
            let v = ev.apply(f, &[t], Pos::NONE).unwrap();
            assert_eq!(ev.force(v).unwrap().as_int(), Some(14), "{s}: {f:?}");
        }
        found(&ev)
    }

    #[test]
    fn a_read_of_the_count_is_one_finding_for_each_tree() {
        let twice = "t: t.revCount + t.revCount";
        let names = "t: if t ? revCount && builtins.elem \"revCount\" (builtins.attrNames t) then 14 else 0";
        let unused = "t: let n = t.revCount; in { inherit (t) revCount; v = 14; }.v";
        let trees = ["self", "inputs.a", "inputs.b"];
        assert_eq!(reads(Config::default(), &trees, twice), ["self 1:4"]);
        let again = ["self", "self"];
        assert_eq!(reads(Config::default(), &again, twice), ["self 1:4"]);
        assert!(reads(Config::default(), &trees, names).is_empty());
        assert!(reads(Config::default(), &trees, unused).is_empty());
        let off =
            toml("[lint]\nrev-count = \"off\"\n[lint.inputs.b]\n\"family:purity\" = \"warn\"\n");
        assert_eq!(reads(off, &trees, twice), ["inputs.b 1:4"]);
        let all = toml("[lint]\nscope = \"all\"\n");
        assert_eq!(
            reads(all, &trees, twice),
            ["self 1:4", "inputs.a 1:4", "inputs.b 1:4"]
        );

        let mut ev = EvalConfig::new()
            .lints(toml("[lint]\nrev-count = \"error\"\n"))
            .build();
        let t = tree(&mut ev, "self");
        let f = "t: [ (builtins.tryEval t.revCount).value (builtins.tryEval (with t; revCount)).value ]";
        let f = ev.eval_string(f, b"/").unwrap();
        let l = ev.apply(f, &[t], Pos::NONE).unwrap();
        for c in ev.force_list(l, Pos::NONE, "").unwrap().cells() {
            let e = ev.force(c.get()).err().unwrap();
            assert_eq!(e.kind, ErrorKind::Lint);
            assert!(
                e.msg
                    .starts_with("[rev-count] 'self.revCount' is deprecated")
            );
        }
    }

    #[test]
    fn a_selection_of_the_count_is_the_site() {
        for (f, site) in [
            ("t: t.revCount * 2", "1:4"),
            ("t: (t.revCount or 0) * 2", "1:5"),
            ("t: { inherit (t) revCount; }.revCount * 2", "1:18"),
            ("t: (let inherit (t) revCount; in revCount) * 2", "1:21"),
            (
                "t: (rec { inherit (t) revCount; n = revCount * 2; }).n",
                "1:23",
            ),
            ("t: { sourceInfo = t; }.sourceInfo.revCount * 2", "1:4"),
            ("t: t.\"revCount\" * 2", "1:4"),
            ("t: t.${\"revCount\"} * 2", "1:4"),
            ("t: (let s = t; in s.revCount) * 2", "1:19"),
            (
                "t: ({ self, ... }: self.revCount) { self = t; } * 2",
                "1:20",
            ),
            ("t: ({ a = 1; } // t).revCount * 2", "1:4"),
            ("t: builtins.getAttr \"revCount\" t * 2", "1:4"),
            ("t: builtins.getAttr (\"rev\" + \"Count\") t * 2", "1:4"),
            ("t:\n  let n = t.revCount; in n + n", "2:11"),
        ] {
            let found = reads(Config::default(), &["self"], f);
            assert_eq!(found, [format!("self {site}")], "{f}");
        }
    }

    #[test]
    fn a_read_that_is_not_a_selection_has_no_site() {
        for f in [
            "t: t.${\"rev\" + \"Count\"} * 2",
            "t: (with t; revCount) * 2",
            "t: ({ revCount, ... }: revCount * 2) t",
            "t: builtins.elemAt (builtins.attrValues (builtins.intersectAttrs { revCount = 1; } t)) 0 * 2",
            "t: (builtins.fromJSON (builtins.toJSON (builtins.intersectAttrs { revCount = 1; } t))).revCount * 2",
            "t: (builtins.mapAttrs (n: v: v) t).revCount * 2",
        ] {
            assert_eq!(reads(Config::default(), &["self"], f), ["self"], "{f}");
        }
    }

    #[test]
    fn the_first_read_of_a_tree_gives_its_finding() {
        let one = |f: &str| reads(Config::default(), &["self"], f);
        let site_first = "t: t.revCount + (with t; revCount) + t.revCount - 7";
        assert_eq!(one(site_first), ["self 1:4"]);
        assert_eq!(one("t: (with t; revCount) + t.revCount"), ["self"]);
        let allowed = "t: t.revCount + t.revCount # iets: allow rev-count";
        assert!(one(allowed).is_empty());
        let next = "t: t.revCount # iets: allow rev-count\n  + t.revCount";
        assert_eq!(one(next), ["self 2:5"]);
        let force = "t: t.revCount # iets: allow rev-count\n  + (with t; revCount)";
        assert_eq!(one(force), ["self"]);
    }

    #[test]
    fn an_entry_of_the_compiled_file_cache_keeps_the_site() {
        let session = Session::new();
        let _entered = session.enter();
        let mut ev = Evaluator::new();
        let src = "t:\n  { s = t; }.s.revCount + { inherit (t) revCount; }.revCount";
        let file = ev.add_file(b"/t/f.nix", src.as_bytes());
        let base = ev.files[file.0 as usize].base;
        let ir = ev.compile_file(b"/t/f.nix", src.into(), b"/t").unwrap();
        let allows = Allows::default();
        let entry = serialize(&ev, ir, base, &[], &allows).unwrap();
        let (ir, _, _) = deserialize(&mut ev, &entry, base, src.len() as u32, false).unwrap();
        assert_eq!(serialize(&ev, ir, base, &[], &allows), Some(entry));
        let (f, t) = (ev.eval(ev.root_env, ir).unwrap(), tree(&mut ev, "self"));
        let v = ev.apply(f, &[t], Pos::NONE).unwrap();
        assert_eq!(ev.force(v).unwrap().as_int(), Some(14));
        assert_eq!(found(&ev), ["self 2:3"]);
    }

    #[test]
    fn the_name_on_a_value_that_is_not_a_tree_is_silent() {
        for f in [
            "t: { revCount = 14; }.revCount",
            "t: { revCount = t.lastModified / 86400 * 14; }.revCount",
            "t: (builtins.zipAttrsWith (n: vs: 14) [ { revCount = 1; } ]).revCount",
            "t: (builtins.mapAttrs (n: v: v * 2) { revCount = 7; }).revCount",
            "t: builtins.getAttr \"revCount\" { revCount = 14; }",
            "t: { revCount = 14; }.revCount or 0",
            "t: { }.revCount or 14",
            "t: { }.a.revCount or 14",
        ] {
            let all = toml("[lint]\nscope = \"all\"\n");
            assert!(reads(all, &["self", "inputs.a"], f).is_empty(), "{f}");
        }
    }
}
