//! The `narHash` and `lastModified` of archives fetched by `rev` alone, so
//! each is downloaded once. One file per archive under `<cache root>/revs/`,
//! named by the sha256 of its URL.
//!
//! Entries are not verified: anyone who can write the cache directory can
//! change which tree a revision maps to. Nix's fetcher cache is the same.

use std::fs::{File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};

use super::parse_nar_hash;
use crate::cachefs;
use crate::store::hash::{Hash, base16_encode_into};
use crate::trace::{Digest32, Obs, Trace, sha256};

pub fn dir(cache_dir: Option<&str>) -> Option<PathBuf> {
    Some(cachefs::root(cache_dir)?.join("revs"))
}

/// The file name for `key`.
pub fn file_name(key: &str) -> String {
    name(&sha256(key.as_bytes()))
}

fn name(key: &Digest32) -> String {
    let mut s = String::with_capacity(64);
    base16_encode_into(&mut s, key);
    s
}

fn read(dir: &Path, key: &Digest32, size: Option<u64>) -> Option<(File, Metadata, Vec<u8>)> {
    let (f, md) = cachefs::open_entry(&dir.join(name(key))).file()?;
    if size.is_some_and(|n| md.len() != n) {
        return None;
    }
    let mut bytes = Vec::new();
    (&f).take(md.len() + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 == md.len()).then_some((f, md, bytes))
}

pub fn get(dir: &Path, key: &str, trace: &Trace) -> Option<(Hash, u64)> {
    let id = sha256(key.as_bytes());
    let (f, md, bytes) = read(dir, &id, None)?;
    let n = bytes.len().checked_sub(cachefs::SUM)?;
    if bytes[n..] != cachefs::checksum(key, &[&bytes[..n]]) {
        return None;
    }
    let text = str::from_utf8(&bytes[..n]).ok()?;
    let (hash, last_modified) = text.split_once(' ')?;
    let found = (parse_nar_hash(hash).ok()?, last_modified.parse().ok()?);
    if trace.is_on() {
        trace.record(Obs::RevMapping {
            key: id,
            size: md.len(),
            hash: sha256(&bytes),
        });
    }
    cachefs::touch_open(&f, md.modified().ok());
    Some(found)
}

/// The exact record that passed the URL checksum and supplied the hash and date.
pub fn matching(
    dir: &Path,
    key: &Digest32,
    size: u64,
    hash: &Digest32,
) -> Option<(File, Metadata)> {
    let (f, md, bytes) = read(dir, key, Some(size))?;
    (sha256(&bytes) == *hash).then_some((f, md))
}

/// Errors are ignored: a missing entry only means another download.
pub fn put(dir: &Path, key: &str, hash: &Hash, last_modified: u64) {
    let text = format!("{} {last_modified}", hash.to_sri());
    let sum = cachefs::checksum(key, &[text.as_bytes()]);
    if cachefs::create_dir(dir).is_ok() {
        let _ = cachefs::write_atomic(dir, &file_name(key), &[text.as_bytes(), &sum]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    const HASH: &str = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    #[test]
    fn an_entry_reads_back_under_its_own_key_only() {
        let d = tempdir().unwrap();
        let dir = d.path().join("revs");
        let hash = parse_nar_hash(HASH).unwrap();
        assert_eq!(get(&dir, "https://x.org/a", &Trace::default()), None);
        put(&dir, "https://x.org/a", &hash, 7);
        assert_eq!(
            get(&dir, "https://x.org/a", &Trace::default()),
            Some((hash, 7))
        );
        assert_eq!(get(&dir, "https://x.org/b", &Trace::default()), None);
    }

    #[test]
    fn a_damaged_entry_is_a_miss() {
        let d = tempdir().unwrap();
        put(d.path(), "k", &parse_nar_hash(HASH).unwrap(), 7);
        let path = d.path().join(file_name("k"));
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert_eq!(get(d.path(), "k", &Trace::default()), None);
    }

    #[test]
    fn a_receipt_tracks_the_selected_tree_and_archive_date() {
        let d = tempdir().unwrap();
        let key = "https://example.org/archive";
        let first = parse_nar_hash(HASH).unwrap();
        let trace = Trace::enabled();
        put(d.path(), key, &first, 7);
        assert_eq!(get(d.path(), key, &trace), Some((first, 7)));
        let obs = trace.take();
        let [
            Obs::RevMapping {
                key: id,
                size,
                hash,
            },
        ] = obs.as_slice()
        else {
            panic!("missing revision observation")
        };
        let valid = || matching(d.path(), id, *size, hash).is_some();
        assert!(valid());
        put(d.path(), key, &first, 9);
        assert!(!valid());
        let other = crate::store::hash::hash_bytes(crate::store::hash::HashAlgo::Sha256, b"other");
        put(d.path(), key, &other, 7);
        assert!(!valid());
        let path = d.path().join(file_name(key));
        put(d.path(), key, &first, 7);
        assert!(valid());
        let mut corrupt = fs::read(&path).unwrap();
        *corrupt.last_mut().unwrap() ^= 1;
        fs::write(&path, corrupt).unwrap();
        assert!(!valid());
        fs::remove_file(path).unwrap();
        assert!(!valid());
    }

    #[test]
    fn only_a_valid_readset_renews_the_selected_revision() {
        use crate::session::Session;
        use crate::trace::{Kind, validate, wire};
        use std::time::SystemTime;

        let d = tempdir().unwrap();
        let session = Session::new();
        let _entered = session.enter();
        session.set_cache_dir(d.path().to_str().unwrap());
        let dir = dir(None).unwrap();
        let key = "https://example.org/archive";
        let hash = parse_nar_hash(HASH).unwrap();
        put(&dir, key, &hash, 7);
        let trace = Trace::enabled();
        get(&dir, key, &trace).unwrap();
        let mut obs = trace.take();
        assert_eq!(
            wire::decode_readset(&wire::encode_readset(&obs)),
            Some(obs.clone())
        );

        let path = dir.join(file_name(key));
        let old = SystemTime::UNIX_EPOCH;
        let age = || {
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap()
        };
        let mtime = || fs::metadata(&path).unwrap().modified().unwrap();
        age();
        obs.push(Obs::Stat {
            path: crate::sys::path_to_internal(&d.path().join("missing")),
            follow: true,
            kind: Kind::Regular,
        });
        assert!(validate::validate(&mut obs, 1).is_err());
        assert_eq!(mtime(), old);
        obs.pop();
        assert!(validate::validate(&mut obs, 1).is_ok());
        assert!(mtime() > old);

        // A concurrent rebind leaves the validator holding the old descriptor.
        let Obs::RevMapping {
            key: id,
            size,
            hash: digest,
        } = &obs[0]
        else {
            unreachable!()
        };
        let (old_entry, _) = matching(&dir, id, *size, digest).unwrap();
        put(&dir, key, &hash, 8);
        age();
        cachefs::touch_open(&old_entry, None);
        assert_eq!(mtime(), old);
    }
}
