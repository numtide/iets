//! Unpacking a downloaded tarball the way Nix does, so the NAR of the result
//! has the `narHash` the lock file names.
//!
//! tarfile.cc:unpackTarfileToSink walks the archive and feeds each member to a
//! sink; git-utils.cc:GitFileSystemObjectSinkImpl builds a git tree from them,
//! and tarball.cc:downloadTarball_ then calls
//! `dereferenceSingletonDirectory` before `treeHashToNarHash` hashes the tree.
//! What survives into the hash is therefore: directories (empty ones included,
//! since the sink writes an empty tree object for them), regular files with one
//! bit of mode (`archive_entry_mode() & S_IXUSR`, tarfile.cc:226), symlink
//! targets, and hard links resolved to a second entry with the target's blob
//! and mode (git-utils.cc:1348-1364). Everything else is an error
//! (tarfile.cc:253). A member listed twice keeps the last copy
//! (git-utils.cc:1213, "only import the latest version of a path").
//!
//! A fetch writes no tree: `fetch::spool` indexes the archive and shares
//! `member_path` with this module. `unpack` is built for the tests alone, as
//! the oracle the spool's NAR is held to (`spool_matches_the_tree`).

#[cfg(test)]
use crate::sys::fs::{is_executable, set_executable, symlink};
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::fs::{self, File};
#[cfg(test)]
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};

#[cfg(test)]
use super::spool;
#[cfg(test)]
use crate::error::{Error, Result};

#[cfg(test)]
fn bad(why: String) -> Error {
    Error::Store(why)
}

/// `CanonPath{name}` (tarfile.cc:204): the member path with any root stripped,
/// `.` dropped and `..` applied. A `..` at the top has nothing to remove, so no
/// member ever names anything outside the tree.
pub fn member_path(name: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in name.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                out.pop();
            }
        }
    }
    out
}

#[cfg(test)]
/// `root` joined with a member path, with the missing parents created. A parent
/// that exists and is not a directory is replaced, a symlink included: the sink
/// never follows one out of the tree, it overwrites it (git-utils.cc:addNode
/// emplaces a directory at every parent name).
fn make_parents(root: &Path, rel: &Path) -> Result<PathBuf> {
    let mut at = root.to_path_buf();
    let n = rel.components().count();
    for (i, c) in rel.components().enumerate() {
        at.push(c);
        if i + 1 == n {
            break;
        }
        match fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                fs::remove_file(&at)
                    .map_err(|e| Error::io(format!("cannot replace {}", at.display()), e))?;
                mkdir(&at)?;
            }
            Err(_) => mkdir(&at)?,
        }
    }
    Ok(at)
}

#[cfg(test)]
fn mkdir(at: &Path) -> Result<()> {
    fs::create_dir(at).map_err(|e| Error::io(format!("cannot create {}", at.display()), e))
}

#[cfg(test)]
/// Whatever is at `at` goes away, unless it is a directory.
fn clear(at: &Path) -> Result<()> {
    match fs::symlink_metadata(at) {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => fs::remove_file(at)
            .map_err(|e| Error::io(format!("cannot replace {}", at.display()), e)),
    }
}

#[cfg(test)]
/// Unpacks the tar stream in `bytes` under `dest` and answers the directory to
/// hash: `dest`, or its single entry when that is the only one and is a
/// directory (git-utils.cc:dereferenceSingletonDirectory).
pub fn unpack(bytes: Vec<u8>, dest: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dest)
        .map_err(|e| Error::io(format!("cannot create {}", dest.display()), e))?;
    let mut links: BTreeMap<PathBuf, PathBuf> = Default::default();
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let entries = archive
        .entries()
        .map_err(|e| Error::io("cannot read the tarball", e))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| Error::io("cannot read the tarball", e))?;
        let kind = entry.header().entry_type();
        // libarchive reads the pax global header as archive metadata, not as a
        // member; GitHub's tarballs start with one.
        if kind.is_pax_global_extensions() {
            continue;
        }
        let pax = spool::pax_sparse(&mut entry)?;
        let name = spool::member_name(&pax, &entry)?;
        let rel = member_path(&name);
        if rel.as_os_str().is_empty() {
            continue;
        }
        if kind.is_character_special() || kind.is_block_special() || kind.is_fifo() {
            return Err(bad(format!(
                "file '{}' in tarball has unsupported file type {kind:?}",
                name.display()
            )));
        }
        let at = make_parents(dest, &rel)?;
        if kind.is_dir() {
            if fs::symlink_metadata(&at).is_ok_and(|m| !m.is_dir()) {
                fs::remove_file(&at)
                    .map_err(|e| Error::io(format!("cannot replace {}", at.display()), e))?;
            }
            if !at.is_dir() {
                mkdir(&at)?;
            }
        } else if kind.is_symlink() {
            let target = entry
                .link_name()
                .map_err(|e| Error::io("cannot read a symlink target", e))?
                .ok_or_else(|| bad(format!("tarball symlink '{}' has no target", rel.display())))?
                .into_owned();
            clear(&at)?;
            symlink(&target, &at)
                .map_err(|e| Error::io(format!("cannot create {}", at.display()), e))?;
        } else if kind.is_hard_link() {
            let target = entry
                .link_name()
                .map_err(|e| Error::io("cannot read a hard link target", e))?
                .ok_or_else(|| bad(format!("tarball link '{}' has no target", rel.display())))?
                .into_owned();
            links.insert(rel, member_path(&target));
        } else {
            // libarchive gives every other type flag AE_IFREG (tarfile.cc:224)
            let executable = entry.header().mode().unwrap_or(0o644) & 0o100 != 0;
            clear(&at)?;
            let out = File::create(&at)
                .map_err(|e| Error::io(format!("cannot write {}", at.display()), e))?;
            let size = pax.as_ref().map_or_else(|| entry.size(), |s| s.size);
            spool::expand(&mut entry, pax.as_ref(), &out, 0, size).map_err(|e| {
                Error::io(
                    format!("cannot read '{}' from the tarball", rel.display()),
                    e,
                )
            })?;
            set_executable(&at, executable)
                .map_err(|e| Error::io(format!("cannot set the mode of {}", at.display()), e))?;
        }
    }
    for (rel, target) in links {
        let at = make_parents(dest, &rel)?;
        let from = dest.join(&target);
        if from.is_dir() {
            return Err(bad(format!(
                "tarball member '{}' is a hard link to the directory '{}'",
                rel.display(),
                target.display()
            )));
        }
        // a link carries no `id`, so it never takes a name a member claimed
        if fs::symlink_metadata(&at).is_ok() {
            continue;
        }
        fs::copy(&from, &at).map_err(|e| {
            Error::io(
                format!("cannot link {} to {}", at.display(), from.display()),
                e,
            )
        })?;
    }
    Ok(singleton(dest))
}

#[cfg(test)]
/// git-utils.cc:dereferenceSingletonDirectory: one entry, and a directory.
fn singleton(dest: &Path) -> PathBuf {
    let mut entries = match fs::read_dir(dest) {
        Ok(e) => e,
        Err(_) => return dest.to_path_buf(),
    };
    let Some(Ok(first)) = entries.next() else {
        return dest.to_path_buf();
    };
    if entries.next().is_some() {
        return dest.to_path_buf();
    }
    match first.file_type() {
        Ok(t) if t.is_dir() => first.path(),
        _ => dest.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::spool::tests::tar_gz;
    use rstest::rstest;
    use std::collections::BTreeMap;
    use std::{env, str};
    use tempfile::tempdir;

    use crate::fetch::http;

    /// The unpacked tree, flattened: a directory is `dir`, a symlink `-> target`,
    /// a file its contents with a leading `x` when it is executable. Only what
    /// the NAR of the tree holds.
    fn tree(dest: &Path) -> BTreeMap<String, String> {
        fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
            let mut names: Vec<PathBuf> = fs::read_dir(at)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            names.sort();
            for p in names {
                let rel = p.strip_prefix(root).unwrap().display().to_string();
                let m = fs::symlink_metadata(&p).unwrap();
                if m.is_symlink() {
                    out.insert(
                        rel,
                        format!("-> {}", std::fs::read_link(&p).unwrap().display()),
                    );
                } else if m.is_dir() {
                    out.insert(rel, "dir".into());
                    walk(root, &p, out);
                } else {
                    let x = if is_executable(&m) { "x" } else { "" };
                    out.insert(
                        rel,
                        format!(
                            "{x}{}",
                            String::from_utf8_lossy(&std::fs::read(&p).unwrap())
                        ),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dest, dest, &mut out);
        out
    }

    const DIR: tar::EntryType = tar::EntryType::Directory;
    const REG: tar::EntryType = tar::EntryType::Regular;

    /// One leading directory and nothing beside it: the hash is of its inside.
    #[test]
    fn strips_one_leading_component() {
        let w = tempdir().unwrap();
        let bytes = tar_gz(&[
            ("repo-abc/", DIR, 0o755, b""),
            ("repo-abc/flake.nix", REG, 0o644, b"{}\n"),
            ("repo-abc/sub/", DIR, 0o755, b""),
        ]);
        let root = unpack(http::decompress(bytes).unwrap(), &w.path().join("x")).unwrap();
        assert_eq!(root.file_name().unwrap(), "repo-abc");
        assert_eq!(
            tree(&root),
            BTreeMap::from([
                ("flake.nix".to_string(), "{}\n".to_string()),
                ("sub".to_string(), "dir".to_string()),
            ])
        );
        fs::remove_dir_all(w.path()).unwrap();
    }

    /// Two top-level entries, or one that is a file: nothing is stripped.
    #[test]
    fn keeps_the_root_when_it_is_not_a_single_directory() {
        let w = tempdir().unwrap();
        let two = tar_gz(&[("a/", DIR, 0o755, b""), ("b", REG, 0o644, b"x")]);
        let root = unpack(http::decompress(two).unwrap(), &w.path().join("two")).unwrap();
        assert_eq!(root, w.path().join("two"));
        assert_eq!(
            tree(&root),
            BTreeMap::from([
                ("a".to_string(), "dir".to_string()),
                ("b".to_string(), "x".to_string()),
            ])
        );

        let one = tar_gz(&[("only", REG, 0o644, b"x")]);
        let root = unpack(http::decompress(one).unwrap(), &w.path().join("one")).unwrap();
        assert_eq!(root, w.path().join("one"));
        fs::remove_dir_all(w.path()).unwrap();
    }

    // TODO: Windows has no executable bit, so the fixture archives differently. Port with a fixture that has none.

    #[cfg(unix)]
    #[test]
    fn modes_symlinks_and_empty_dirs() {
        let w = tempdir().unwrap();
        let bytes = tar_gz(&[
            ("t/", DIR, 0o755, b""),
            ("t/run", REG, 0o755, b"#!/bin/sh\n"),
            ("t/plain", REG, 0o644, b"p"),
            ("t/empty/", DIR, 0o700, b""),
            ("t/link", tar::EntryType::Symlink, 0o777, b"plain"),
            ("t/same", tar::EntryType::Link, 0o644, b"t/plain"),
        ]);
        let root = unpack(http::decompress(bytes).unwrap(), &w.path().join("x")).unwrap();
        assert_eq!(
            tree(&root),
            BTreeMap::from([
                ("empty".to_string(), "dir".to_string()),
                ("link".to_string(), "-> plain".to_string()),
                ("plain".to_string(), "p".to_string()),
                ("run".to_string(), "x#!/bin/sh\n".to_string()),
                ("same".to_string(), "p".to_string()),
            ])
        );
        fs::remove_dir_all(w.path()).unwrap();
    }

    /// The last copy of a member wins, as the git sink's `id` ordering does.
    // TODO: Windows has no executable bit, so the fixture archives differently. Port with a fixture that has none.
    #[cfg(unix)]
    #[test]
    fn a_repeated_member_keeps_the_last_copy() {
        let w = tempdir().unwrap();
        let bytes = tar_gz(&[
            ("t/", DIR, 0o755, b""),
            ("t/f", REG, 0o644, b"first"),
            ("t/f", REG, 0o755, b"second"),
        ]);
        let root = unpack(http::decompress(bytes).unwrap(), &w.path().join("x")).unwrap();
        assert_eq!(
            tree(&root),
            BTreeMap::from([("f".to_string(), "xsecond".to_string())])
        );
        fs::remove_dir_all(w.path()).unwrap();
    }

    /// A device, socket or fifo member is an error, as tarfile.cc:253 is.
    #[test]
    fn an_unsupported_member_type_is_an_error() {
        let w = tempdir().unwrap();
        let bytes = tar_gz(&[("dev", tar::EntryType::Fifo, 0o644, b"")]);
        let e = unpack(http::decompress(bytes).unwrap(), &w.path().join("y"));
        assert!(e.unwrap_err().to_string().contains("unsupported file type"));
        fs::remove_dir_all(w.path()).unwrap();
    }

    /// An absolute member name loses its root and `..` is applied, as
    /// `CanonPath` applies it; a `..` at the top removes nothing.
    #[rstest]
    #[case("/a/b", "a/b")]
    #[case("./a", "a")]
    #[case("a/./b", "a/b")]
    #[case("a/../b", "b")]
    #[case("../outside", "outside")]
    #[case("t/../../esc", "esc")]
    fn member_names_are_made_relative_and_cannot_escape(#[case] name: &str, #[case] want: &str) {
        assert_eq!(member_path(Path::new(name)), Path::new(want));
    }

    /// The NAR hash of an unpacked tree is what `nix hash path` prints for it;
    /// the expected value comes from a tarball whose `narHash` a lock file
    /// records (`tests/data/tarball`).
    // TODO: Windows has no executable bit, so the fixture archives differently. Port with a fixture that has none.
    #[cfg(unix)]
    #[test]
    fn nar_hash_of_the_fixture() {
        let w = tempdir().unwrap();
        let bytes = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data/tarball/tree.tar.gz"),
        )
        .unwrap();
        let root = unpack(http::decompress(bytes).unwrap(), &w.path().join("x")).unwrap();
        let want = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data/tarball/narHash"),
        )
        .unwrap();
        assert_eq!(
            crate::store::nar::nar_hash(&root).unwrap().to_sri(),
            want.trim()
        );
        fs::remove_dir_all(w.path()).unwrap();
    }
}
