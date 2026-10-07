//! A made-up SCM that only the tests know. Its shape is not the shape of
//! git, so a part of the interface that only git can fill fails here: the
//! marker is the directory `.ledger`, `tracked` lists the files as text,
//! `head` holds `r<count> <date>` and is absent before the first revision,
//! the entry `dirty` is the dirty flag, a revision always has a count, and
//! no answer comes from a command.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::{Scm, Snapshot};
use crate::trace::{Kind, Obs, Trace};

pub struct Ledger;

const ENTRIES: [&str; 3] = ["/.ledger/tracked", "/.ledger/head", "/.ledger/dirty"];

fn read(trace: &Trace, p: &Path) -> Result<String, String> {
    let err = |e: std::io::Error| format!("ledger: {}: {e}", p.display());
    let meta = fs::metadata(p).map_err(err)?;
    let bytes = fs::read(p).map_err(err)?;
    trace.file(p.as_os_str().as_bytes(), &meta, &bytes);
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn there(trace: &Trace, p: &Path) -> bool {
    trace.stat_now(p.as_os_str().as_bytes(), false) != Kind::Missing
}

impl Scm for Ledger {
    fn name(&self) -> &'static str {
        "ledger"
    }

    fn title(&self) -> &'static str {
        "Ledger"
    }
    fn root_marker(&self, dir: &Path) -> PathBuf {
        dir.join(".ledger")
    }

    fn snapshot(&self, trace: &Trace, root: &Path) -> Result<Snapshot, String> {
        let dot = root.join(".ledger");
        let tracked = read(trace, &dot.join("tracked"))?;
        let head = dot.join("head");
        let head = match there(trace, &head) {
            true => Some(read(trace, &head)?),
            false => None,
        };
        let (rev, date) = head
            .as_deref()
            .and_then(|h| h.trim().split_once(' '))
            .unzip();
        Ok(Snapshot {
            files: tracked.lines().map(|l| root.join(l)).collect(),
            dirty: there(trace, &dot.join("dirty")),
            rev: rev.map(str::to_string),
            last_modified: date.and_then(|d| d.parse().ok()).unwrap_or(0),
            countable: true,
        })
    }

    fn rev_count(&self, _root: &Path, rev: &str) -> Result<u64, String> {
        let count = rev.strip_prefix('r').and_then(|n| n.parse().ok());
        count.ok_or_else(|| format!("ledger: '{rev}' is no revision"))
    }

    fn untracked(&self, root: &Path, rel: &OsStr) -> Option<String> {
        let tracked = fs::read_to_string(root.join(".ledger/tracked")).ok()?;
        (!tracked.lines().any(|l| OsStr::new(l) == rel))
            .then(|| format!("ledger track \"{}\"", root.join(rel).display()))
    }

    fn tree_of<'a>(&self, obs: &'a Obs) -> Option<&'a [u8]> {
        let (Obs::File { path, .. } | Obs::Stat { path, .. }) = obs else {
            return None;
        };
        ENTRIES.iter().find_map(|e| path.strip_suffix(e.as_bytes()))
    }

    fn watch(&self, _root: &Path) -> Option<PathBuf> {
        None
    }
}

mod tests {
    use super::*;
    use crate::builtins::store_hooks::open;
    use crate::eval::{EvalConfig, Evaluator, Pos};
    use crate::flake::{fetch::emit_tree_attrs, parse_flake_ref};
    use crate::runtime::{Ctx, Taint, Word};
    use crate::source::scm::{count_key, rev_count};
    use crate::source::{AttrVal, Attrs, SourceRef, Tree, tree};
    use crate::trace::validate::validate;

    const FLAKE: &str = "{ outputs = _: { }; }\n";

    /// A working tree with three tracked files, one untracked file and the
    /// revision `r12`; `plain` holds the tracked files and nothing else.
    fn fixture(tag: &str) -> (PathBuf, PathBuf) {
        let d = std::env::temp_dir().join(format!("iets-ledger-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let (root, plain) = (d.join("tree"), d.join("plain"));
        for top in [&root, &plain] {
            fs::create_dir_all(top.join("sub")).unwrap();
            for f in ["flake.nix", "sub/flake.nix", "sub/lib.nix"] {
                fs::write(top.join(f), FLAKE).unwrap();
            }
        }
        fs::write(root.join("extra"), "not tracked\n").unwrap();
        fs::create_dir_all(root.join(".ledger")).unwrap();
        let tracked = "flake.nix\nsub/flake.nix\nsub/lib.nix\n";
        fs::write(root.join(".ledger/tracked"), tracked).unwrap();
        fs::write(root.join(".ledger/head"), "r12 1700000000\n").unwrap();
        (root, plain)
    }

    fn source(root: &Path) -> SourceRef {
        let arg = format!("{}/sub#x", root.display());
        parse_flake_ref(&arg, b"/", None).unwrap().source
    }

    fn read_tree(ev: &mut Evaluator, root: &Path) -> Tree {
        tree(ev, &source(root), Pos::NONE).unwrap()
    }

    fn attrs(pairs: &[(&str, AttrVal)], root: &Path, nar_hash: String) -> Attrs {
        let url = format!("file://{}", root.display());
        let mut a: Attrs = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        a.insert("type".into(), AttrVal::Str("ledger".into()));
        a.insert("url".into(), AttrVal::Str(url));
        a.insert("narHash".into(), AttrVal::Str(nar_hash));
        a
    }

    #[test]
    fn a_directory_in_a_working_tree_is_found() {
        let (root, _) = fixture("detect");
        let r = parse_flake_ref(&format!("{}/sub#x", root.display()), b"/", None).unwrap();
        let tracked = SourceRef::Tracked {
            scm: &Ledger,
            root: root.clone(),
            subdir: "sub".into(),
        };
        assert_eq!(r.source, tracked);
        let url = format!("ledger+file://{}?dir=sub", root.display());
        assert_eq!(crate::api::flake::url(&r), url);
        let r = parse_flake_ref(&root.display().to_string(), b"/", None).unwrap();
        let url = format!("ledger+file://{}", root.display());
        assert_eq!(crate::api::flake::url(&r), url);
        let r = parse_flake_ref(&format!("path:{}/sub", root.display()), b"/", None).unwrap();
        assert_eq!(r.source, SourceRef::Path(root.join("sub")));
    }

    #[test]
    fn the_tracked_files_and_the_metadata_are_the_tree() {
        let (root, plain) = fixture("tree");
        let hash = |d: &Path| crate::store::nar::nar_hash(d).unwrap().to_sri();
        let t = read_tree(&mut Evaluator::new(), &root);
        assert_eq!(t.subdir, "sub");
        let clean = [
            ("rev", AttrVal::Str("r12".into())),
            ("lastModified", AttrVal::Int(1700000000)),
        ];
        assert_eq!(t.attrs, attrs(&clean, &root, hash(&plain)));
        assert!(t.counter == Some((&Ledger, root.clone())));

        fs::write(root.join(".ledger/dirty"), "").unwrap();
        for top in [&root, &plain] {
            fs::write(top.join("sub/lib.nix"), "1\n").unwrap();
        }
        let t = read_tree(&mut Evaluator::new(), &root);
        let dirty = [
            ("dirtyRev", AttrVal::Str("r12-dirty".into())),
            ("dirtyShortRev", AttrVal::Str("r12-dirty".into())),
            ("lastModified", AttrVal::Int(1700000000)),
        ];
        assert_eq!(t.attrs, attrs(&dirty, &root, hash(&plain)));
        assert!(t.counter.is_none());

        fs::remove_file(root.join(".ledger/dirty")).unwrap();
        fs::remove_file(root.join(".ledger/head")).unwrap();
        let t = read_tree(&mut Evaluator::new(), &root);
        let new = [("lastModified", AttrVal::Int(0))];
        assert_eq!(t.attrs, attrs(&new, &root, hash(&plain)));
        assert!(t.counter.is_none());
    }

    #[test]
    fn the_metadata_has_the_mark_of_its_tree() {
        let (root, _) = fixture("mark");
        let mut ev = Evaluator::new();
        let t = read_tree(&mut ev, &root);
        let info = emit_tree_attrs(&mut ev, &t, Some("self"), Pos::NONE).unwrap();
        for (f, text, mark) in [
            ("t: t.rev", "r12", "self.rev"),
            ("t: t.shortRev", "r12", "self.shortRev"),
            (
                "t: t.lastModifiedDate",
                "20231114221320",
                "self.lastModifiedDate",
            ),
        ] {
            let f = ev.eval_string(f, b"/").unwrap();
            let v = ev.apply(f, &[info], Pos::NONE).unwrap();
            let s = ev.force_string(v, Pos::NONE, "").unwrap();
            assert_eq!(s.bytes, text.as_bytes());
            assert!(matches!(s.ctx, [Ctx::Impure(Taint::Meta(m))] if *m == mark.as_bytes()));
        }
        let f = ev
            .eval_string("t: t.revCount + t.lastModified", b"/")
            .unwrap();
        let v = ev.apply(f, &[info], Pos::NONE).unwrap();
        assert_eq!(ev.force(v).unwrap().as_int(), Some(1700000012));
    }

    /// `x12` is a revision that the ledger cannot count, so a count of it
    /// shows where the answer comes from.
    #[test]
    fn the_count_is_lazy_and_kept_by_revision() {
        let (root, _) = fixture("count");
        fs::write(root.join(".ledger/head"), "x12 1700000000\n").unwrap();
        let store = crate::trace::store::Store::locate(root.parent().unwrap().to_str()).unwrap();
        assert!(crate::cachefs::prepare(
            store.root(),
            crate::cachefs::Shape::Dirs,
            false
        ));
        let read = |ev: &mut Evaluator, f: &str| {
            let t = read_tree(ev, &root);
            let info = emit_tree_attrs(ev, &t, Some("self"), Pos::NONE).unwrap();
            let f = ev.eval_string(f, b"/").unwrap();
            let v = ev.apply(f, &[info], Pos::NONE)?;
            ev.force(v)
        };
        let ev = &mut EvalConfig::new().trace_in(Some(&store)).build();
        let there = "t: t ? revCount && builtins.elem \"revCount\" (builtins.attrNames t)";
        assert_eq!(read(ev, there).unwrap(), Word::TRUE);
        let e = read(ev, "t: t.revCount").err().unwrap();
        assert_eq!(e.msg, "ledger: 'x12' is no revision");
        assert_eq!(ev.trace().take().len(), 5);

        ev.trace().keep(&count_key(&Ledger, "x12"), b"7");
        assert_eq!(read(ev, "t: t.revCount").unwrap().as_int(), Some(7));
        assert_eq!(rev_count(ev.trace(), &Ledger, &root, "r12"), Ok(12));
        let kept = ev.trace().kept(&count_key(&Ledger, "r12"));
        assert_eq!(kept.as_deref(), Some(b"12".as_slice()));
        assert_eq!(rev_count(&Trace::default(), &Ledger, &root, "r13"), Ok(13));
    }

    #[test]
    fn an_untracked_file_is_the_error_of_nix() {
        let (root, _) = fixture("untracked");
        let mut ev = Evaluator::new();
        let store = read_tree(&mut ev, &root).path.to_full_string();
        assert!(open(&ev, format!("{store}/sub/lib.nix").as_bytes(), Pos::NONE).is_ok());
        let e = open(&ev, format!("{store}/extra").as_bytes(), Pos::NONE)
            .err()
            .unwrap();
        let (root, add) = (root.display(), root.join("extra"));
        let add = format!("ledger track \"{}\"", add.display());
        let msg = format!(
            "Path 'extra' in the repository \"{root}\" is not tracked by Ledger.\n\n\
             To make it visible to Nix, run:\n\n{add}"
        );
        assert_eq!((e.msg, e.hint), (msg, Some(format!("run '{add}'"))));
    }

    #[test]
    fn a_change_of_the_ledger_is_a_change_of_the_working_tree() {
        let (root, _) = fixture("trace");
        let mut ev = EvalConfig::new().trace(true).build();
        read_tree(&mut ev, &root);
        let mut obs = ev.trace().take();
        let kinds: Vec<&str> = obs
            .iter()
            .map(|o| match o {
                Obs::File { .. } => "file",
                Obs::Stat { .. } => "stat",
                Obs::Tree { .. } => "tree",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["file", "stat", "file", "stat", "tree"]);
        assert_eq!(validate(&mut obs, 2).unwrap().entries, 5);
        let watched: Vec<String> = crate::trace::watch::watched(&obs, &[])
            .into_iter()
            .map(|w| w.path)
            .collect();
        let want = ENTRIES.map(|e| format!("{}{e}", root.display()));
        assert_eq!(watched, want);

        fs::write(root.join(".ledger/head"), "r13 1700000001\n").unwrap();
        let why = validate(&mut obs, 2).unwrap_err().0;
        let said = format!("the ledger working tree in {} changed", root.display());
        assert_eq!(why, said);
    }
}
