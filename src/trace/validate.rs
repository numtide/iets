//! Checks a recorded read set against the world, on a few threads. Any
//! mismatch is a miss. Stat-only checks come first; a file whose stat
//! changed is rehashed and accepted when its content did not.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{DirBuf, Kind, Obs, dir_hash, mtime_ns, stat_kind};
#[cfg(test)]
use super::{dir_listing_hash, read_dir_kinds};
use crate::store::hash::HashAlgo;
use crate::store::hash::Hasher;

#[derive(Debug, Default)]
pub struct Report {
    pub entries: usize,
    /// Files and trees whose contents were rehashed because their stat changed.
    pub rehashed: usize,
    /// Entries whose stat fields were refreshed; the read set should be rewritten.
    pub refreshed: usize,
}

/// The lowest-indexed observation that no longer holds, and why. The read
/// set has one order, so one change always names the same observation,
/// whichever thread saw it (docs/reference/errors.md, `cache-invalidated`).
#[derive(Debug)]
pub struct Mismatch(pub String);

enum Outcome {
    Same,
    /// Content still matches; the entry's stat fields are stale.
    Refresh(Obs),
    Changed(String),
}

fn os(p: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(p))
}

fn lossy(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

/// Why a path no longer reads: it is not there, or it is there and the read
/// failed. The message says which, because the two need different fixes.
fn unreadable(path: &[u8], e: &std::io::Error) -> Outcome {
    let what = if e.kind() == std::io::ErrorKind::NotFound {
        "is gone"
    } else {
        "cannot be read"
    };
    Outcome::Changed(format!("{} {what}", lossy(path)))
}

/// A kind as the invalidation message names it.
fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Missing => "missing",
        Kind::Regular => "a regular file",
        Kind::Directory => "a directory",
        Kind::Symlink => "a symbolic link",
        Kind::Other => "another kind of file",
    }
}

/// sha256 of a file's bytes without holding them: a rehash pass over a big `nixpkgs` file
/// would otherwise allocate the whole file to read it once.
fn file_sha256(p: &Path) -> std::io::Result<super::Digest32> {
    let mut h = Hasher::new(HashAlgo::Sha256);
    std::io::copy(&mut fs::File::open(p)?, &mut h)?;
    Ok(super::hash_to_digest(&h.finish()))
}

fn check(o: &Obs, rehashed: &AtomicUsize, dirs: &mut DirBuf) -> Outcome {
    match o {
        Obs::File {
            path,
            size,
            mtime_ns: mt,
            ctime_ns: ct,
            dev,
            ino,
            hash,
        } => {
            let m = match fs::metadata(os(path)) {
                Ok(m) => m,
                Err(e) => return unreadable(path, &e),
            };
            if !m.is_file() {
                return Outcome::Changed(format!(
                    "{} is now {}, not a regular file",
                    lossy(path),
                    kind_name(Kind::of(&Ok(m)))
                ));
            }
            if m.len() == *size
                && mtime_ns(&m) == *mt
                && super::ctime_ns(&m) == *ct
                && m.dev() == *dev
                && m.ino() == *ino
            {
                return Outcome::Same;
            }
            rehashed.fetch_add(1, Ordering::Relaxed);
            let now = match file_sha256(os(path)) {
                Ok(h) => h,
                Err(e) => return unreadable(path, &e),
            };
            if now != *hash {
                return Outcome::Changed(format!("{} changed", lossy(path)));
            }
            Outcome::Refresh(super::file_obs(path, &m, *hash))
        }
        Obs::Dir { path, hash } => match dir_hash(os(path), dirs) {
            Ok(now) => {
                if now == *hash {
                    Outcome::Same
                } else {
                    Outcome::Changed(format!("directory {} has different entries", lossy(path)))
                }
            }
            Err(e) => unreadable(path, &e),
        },
        Obs::Stat { path, follow, kind } => {
            let now = stat_kind(os(path), *follow);
            match (now, *kind) {
                (now, was) if now == was => Outcome::Same,
                (Kind::Missing, _) => Outcome::Changed(format!("{} is gone", lossy(path))),
                (_, Kind::Missing) => Outcome::Changed(format!("{} now exists", lossy(path))),
                (now, was) => Outcome::Changed(format!(
                    "{} is now {}, not {}",
                    lossy(path),
                    kind_name(now),
                    kind_name(was)
                )),
            }
        }
        Obs::Tree {
            root,
            nar_hash,
            stat_hash,
            filtered,
            newest,
        } => {
            let accepted: Option<HashSet<PathBuf>> = filtered.as_ref().map(|f| {
                f.accepted
                    .iter()
                    .map(|p| PathBuf::from(OsStr::from_bytes(p)))
                    .collect()
            });
            let root_p = os(root);
            match super::tree_fingerprint(root_p, accepted.as_ref()) {
                Ok((sh, _)) if sh == *stat_hash => return Outcome::Same,
                Ok(_) => {}
                Err(e) => return unreadable(root, &e),
            }
            rehashed.fetch_add(1, Ordering::Relaxed);
            let (nar, seen, sh, now) = match super::tree_rehash(root_p, accepted.as_ref()) {
                Ok(t) => t,
                Err(e) => return unreadable(root, &e),
            };
            if nar != *nar_hash
                || filtered
                    .as_ref()
                    .is_some_and(|f| f.seen_hash.is_some_and(|s| s != seen))
            {
                return Outcome::Changed(format!("source tree {} changed", lossy(root)));
            }
            if newest.is_some_and(|n| n != now) {
                return Outcome::Changed(format!(
                    "source tree {} has another modification time",
                    lossy(root)
                ));
            }
            Outcome::Refresh(Obs::Tree {
                root: root.clone(),
                nar_hash: *nar_hash,
                stat_hash: sh,
                filtered: filtered.clone(),
                newest: *newest,
            })
        }
        Obs::Canon { path, target } => {
            let now = fs::canonicalize(os(path))
                .ok()
                .map(|p| p.as_os_str().as_bytes().to_vec());
            if now == *target {
                Outcome::Same
            } else {
                Outcome::Changed(format!("{} resolves to a different path", lossy(path)))
            }
        }
        Obs::Env { name, value } => {
            let now = super::env_hash(name);
            let what = match (&now, value) {
                _ if now == *value => return Outcome::Same,
                (Some(_), None) => "is now set",
                (None, Some(_)) => "is no longer set",
                _ => "changed",
            };
            Outcome::Changed(format!("environment variable ${} {what}", lossy(name)))
        }
        Obs::Cmd { dir, query, hash } => match super::run_query(os(dir), *query) {
            Ok((status, out)) if super::cmd_hash(status, &out) == *hash => Outcome::Same,
            _ => Outcome::Changed(format!(
                "the output of `{}` in {} changed",
                query.argv().join(" "),
                lossy(dir)
            )),
        },
        // One `stat` per entry, no bytes: the name is the content address, so an
        // entry that is still there still holds what the run read.
        Obs::Input { entry, version } => {
            if crate::treestore::has(entry, *version) {
                Outcome::Same
            } else {
                Outcome::Changed(format!("input store entry {} is gone", lossy(entry)))
            }
        }
    }
}

/// Validates every observation on `threads` threads. On success the
/// refreshed entries are written back into `obs`.
///
/// The [`Mismatch`] names the lowest-indexed observation that failed, not the
/// first one a thread reported, so one change always names the same thing.
/// The threads still stop at the first failure: every index below it was
/// already handed out, and a thread finishes the check it started before it
/// breaks, so the lowest failing index is always among the ones seen.
pub fn validate(obs: &mut [Obs], threads: usize) -> Result<Report, Mismatch> {
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let rehashed = AtomicUsize::new(0);
    let first: Mutex<Option<(usize, String)>> = Mutex::new(None);
    let refreshes: Mutex<Vec<(usize, Obs)>> = Mutex::new(Vec::new());
    let threads = threads.clamp(1, obs.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(crate::session::bind(|| {
                let mut mine = Vec::new();
                let mut dirs = DirBuf::default();
                loop {
                    if failed.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= obs.len() {
                        break;
                    }
                    match check(&obs[i], &rehashed, &mut dirs) {
                        Outcome::Same => {}
                        Outcome::Refresh(o) => mine.push((i, o)),
                        Outcome::Changed(why) => {
                            let why = crate::source::scm::changed(&obs[i]).unwrap_or(why);
                            failed.store(true, Ordering::Relaxed);
                            let mut lowest = first.lock().unwrap();
                            if lowest.as_ref().is_none_or(|(j, _)| i < *j) {
                                *lowest = Some((i, why));
                            }
                            drop(lowest);
                            break;
                        }
                    }
                }
                refreshes.lock().unwrap().extend(mine);
            }));
        }
    });
    if failed.load(Ordering::Relaxed) {
        let why = first.into_inner().unwrap().map(|(_, w)| w);
        return Err(Mismatch(why.unwrap_or_default()));
    }
    let refreshes = refreshes.into_inner().unwrap();
    let refreshed = refreshes.len();
    for (i, o) in refreshes {
        obs[i] = o;
    }
    Ok(Report {
        entries: obs.len(),
        rehashed: rehashed.into_inner(),
        refreshed,
    })
}

#[cfg(test)]
mod tests {
    use super::super::sha256;
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("iets-trace-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn file_touch_refreshes_edit_misses() {
        let d = tmp("file");
        let f = d.join("a.nix");
        fs::write(&f, b"1").unwrap();
        let m = fs::metadata(&f).unwrap();
        let p = f.as_os_str().as_bytes().to_vec();
        let mut obs = vec![super::super::file_obs(&p, &m, sha256(b"1"))];
        let r = validate(&mut obs, 2).unwrap();
        assert_eq!((r.rehashed, r.refreshed), (0, 0));

        // same content, new mtime: rehash once, refresh
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        fs::File::open(&f).unwrap().set_modified(later).unwrap();
        let r = validate(&mut obs, 2).unwrap();
        assert_eq!((r.rehashed, r.refreshed), (1, 1));
        let r = validate(&mut obs, 2).unwrap();
        assert_eq!((r.rehashed, r.refreshed), (0, 0));

        fs::write(&f, b"2").unwrap();
        assert!(validate(&mut obs, 2).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    /// `cp -p` over a file of the same length keeps the size, the mtime and
    /// the inode, and only `ctime` says the bytes are new.
    #[test]
    fn a_copy_in_place_that_keeps_the_stat_still_misses() {
        let d = tmp("cp-p");
        let f = d.join("a.nix");
        let other = d.join("b.nix");
        fs::write(&f, b"AAAA").unwrap();
        fs::write(&other, b"BBBB").unwrap();
        let fixed = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        for p in [&f, &other] {
            fs::File::open(p).unwrap().set_modified(fixed).unwrap();
        }
        let m = fs::metadata(&f).unwrap();
        let p = f.as_os_str().as_bytes().to_vec();
        let mut obs = vec![super::super::file_obs(&p, &m, sha256(b"AAAA"))];
        validate(&mut obs, 1).unwrap();

        // the kernel stamps ctime from a clock that ticks every few
        // milliseconds, so wait until a write can get a new one
        while {
            fs::File::open(&other).unwrap().set_modified(fixed).unwrap();
            super::super::ctime_ns(&fs::metadata(&other).unwrap()) == super::super::ctime_ns(&m)
        } {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut bytes = fs::File::create(&f).unwrap();
        std::io::Write::write_all(&mut bytes, b"BBBB").unwrap();
        drop(bytes);
        fs::File::open(&f).unwrap().set_modified(fixed).unwrap();
        let now = fs::metadata(&f).unwrap();
        assert_eq!(
            (now.len(), mtime_ns(&now), now.ino()),
            (m.len(), mtime_ns(&m), m.ino()),
            "size, mtime and inode are the ones the entry holds"
        );
        assert!(validate(&mut obs, 1).is_err(), "and the content changed");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn dir_stat_env_checks() {
        let d = tmp("dir");
        fs::write(d.join("x"), b"").unwrap();
        let p = d.as_os_str().as_bytes().to_vec();
        let mut entries = read_dir_kinds(&d).unwrap();
        let mut obs = vec![
            Obs::Dir {
                path: p.clone(),
                hash: dir_listing_hash(&mut entries),
            },
            Obs::Stat {
                path: p.clone(),
                follow: true,
                kind: Kind::Directory,
            },
            Obs::Env {
                name: b"IETS_TRACE_TEST_UNSET".to_vec(),
                value: None,
            },
        ];
        validate(&mut obs, 3).unwrap();
        fs::write(d.join("y"), b"").unwrap();
        assert!(validate(&mut obs, 3).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_lowest_failing_observation_is_the_one_reported() {
        let gone = |n: usize| Obs::Stat {
            path: format!("/nonexistent/iets-validate-{n}").into_bytes(),
            follow: true,
            kind: Kind::Regular,
        };
        let here = Obs::Env {
            name: b"IETS_TRACE_TEST_UNSET".to_vec(),
            value: None,
        };
        let mut obs: Vec<Obs> = (0..64)
            .map(|n| if n % 7 == 3 { gone(n) } else { here.clone() })
            .collect();
        for _ in 0..50 {
            let e = validate(&mut obs, 4).expect_err("the missing paths fail");
            assert_eq!(e.0, "/nonexistent/iets-validate-3 is gone");
        }
    }
}

#[cfg(test)]
mod dir_hash_tests {
    use super::super::{DirBuf, dir_hash, dir_listing_hash, read_dir_kinds};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    /// The buffered hash must answer exactly what the allocating one does, or
    /// a cache entry written by one generation is unreadable by the next.
    #[test]
    fn dir_hash_agrees_with_dir_listing_hash() {
        let d = std::env::temp_dir().join(format!("iets-dirhash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        // names that exercise the ordering: sorts differ for case, digits,
        // length-prefix collisions and a non-UTF-8 byte
        for n in [
            "b",
            "a",
            "B",
            "A",
            "a-long-one",
            "a0",
            "_0",
            "zz",
            "10",
            "2",
        ] {
            fs::write(d.join(n), b"x").unwrap();
        }
        fs::create_dir(d.join("adir")).unwrap();
        std::os::unix::fs::symlink("a", d.join("alink")).unwrap();
        fs::write(d.join(std::ffi::OsStr::from_bytes(b"non\xffutf8")), b"x").unwrap();

        let mut entries = read_dir_kinds(&d).unwrap();
        let want = dir_listing_hash(&mut entries);
        let mut buf = DirBuf::default();
        assert_eq!(dir_hash(&d, &mut buf).unwrap(), want);
        // the buffer is reused, so a second directory must not see the first
        let e = std::env::temp_dir().join(format!("iets-dirhash-e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&e);
        fs::create_dir_all(&e).unwrap();
        let mut none = read_dir_kinds(&e).unwrap();
        assert_eq!(dir_hash(&e, &mut buf).unwrap(), dir_listing_hash(&mut none));
        assert_eq!(dir_hash(&d, &mut buf).unwrap(), want);

        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&e);
    }
}
