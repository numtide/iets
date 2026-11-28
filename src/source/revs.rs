//! The `narHash` and `lastModified` of archives fetched by `rev` alone, so
//! each is downloaded once. One file per archive under `<cache root>/revs/`,
//! named by the sha256 of its URL.
//!
//! Entries are not verified: anyone who can write the cache directory can
//! change which tree a revision maps to. Nix's fetcher cache is the same.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::parse_nar_hash;
use crate::cachefs;
use crate::store::hash::{Hash, HashAlgo, Hasher};

pub fn dir(cache_dir: Option<&str>) -> Option<PathBuf> {
    Some(cachefs::root(cache_dir)?.join("revs"))
}

/// The file name for `key`.
pub fn file_name(key: &str) -> String {
    let mut h = Hasher::new(HashAlgo::Sha256);
    h.write_all(key.as_bytes())
        .expect("a hasher takes any bytes");
    h.finish().to_base16()
}

pub fn get(dir: &Path, key: &str) -> Option<(Hash, u64)> {
    let path = dir.join(file_name(key));
    let (mut f, _) = cachefs::open_entry(&path).file()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    let n = bytes.len().checked_sub(cachefs::SUM)?;
    if bytes[n..] != cachefs::checksum(key, &[&bytes[..n]]) {
        return None;
    }
    let text = str::from_utf8(&bytes[..n]).ok()?;
    let (hash, last_modified) = text.split_once(' ')?;
    let found = (parse_nar_hash(hash).ok()?, last_modified.parse().ok()?);
    // the sweep goes by when an entry was last used
    cachefs::touch(&path);
    Some(found)
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
        assert_eq!(get(&dir, "https://x.org/a"), None);
        put(&dir, "https://x.org/a", &hash, 7);
        assert_eq!(get(&dir, "https://x.org/a"), Some((hash, 7)));
        assert_eq!(get(&dir, "https://x.org/b"), None);
    }

    #[test]
    fn a_damaged_entry_is_a_miss() {
        let d = tempdir().unwrap();
        put(d.path(), "k", &parse_nar_hash(HASH).unwrap(), 7);
        let path = d.path().join(file_name("k"));
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 1;
        fs::write(&path, bytes).unwrap();
        assert_eq!(get(d.path(), "k"), None);
    }
}
