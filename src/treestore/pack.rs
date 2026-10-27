//! Where the input store keeps file contents: immutable pack files of objects
//! named by the blake3 of their bytes, so a file that two inputs share — most
//! of nixpkgs between two revisions — is stored once. The design is
//! nix-tarmac's (github.com/Mic92/nix-tarmac), with a pack that indexes itself
//! in place of its LMDB index: a pack is written once under a temporary name
//! and renamed into place, and nothing in it changes after, which is the
//! `cachefs` discipline.
//!
//! A pack is the header, the payloads one after the other, the index — one
//! fixed-width record per object, sorted by id — and a tail: a fanout table on
//! the first byte of the id, the count, where the index starts, a checksum over
//! the index and the tail, and a trailer. A payload is LZ4 when that makes it
//! smaller. The checksum covers the index; a payload is trusted because the
//! tree it belongs to was hashed against the lock before anything named it.

use crate::sys::fs as sysfs;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::cachefs;

pub const PACKS: &str = "packs";

pub type Id = [u8; 32];

pub fn id_of(bytes: &[u8]) -> Id {
    *blake3::hash(bytes).as_bytes()
}

/// An object at least this big is streamed in, uncompressed, and never held
/// in memory whole.
pub const STREAM: u64 = 64 << 20;

pub fn id_of_reader(r: &mut dyn Read) -> io::Result<Id> {
    let mut h = blake3::Hasher::new();
    io::copy(r, &mut h)?;
    Ok(*h.finalize().as_bytes())
}

const MAGIC: &[u8; 8] = b"ietspack";
const TRAILER: &[u8; 8] = b"ietsidx1";
const ENTRY: usize = 32 + 8 + 8 + 8 + 1;
const TAIL: usize = 256 * 4 + 8 + 8 + 16 + 8;
const LZ4: u8 = 1;
const MIN_COMPRESS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loc {
    offset: u64,
    stored: u64,
    raw: u64,
    flags: u8,
}

fn corrupt(why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("input store pack: {why}"),
    )
}

#[derive(Debug)]
pub struct Pack {
    file: File,
    path: PathBuf,
    index: Vec<u8>,
    fanout: [u32; 256],
    len: u64,
}

impl Pack {
    /// `Ok(None)` when the file is not a whole, well-formed pack: the caller
    /// removes it rather than reading anything it says.
    pub fn open(path: &Path) -> io::Result<Option<Pack>> {
        let file = sysfs::no_follow_no_block(File::options().read(true)).open(path)?;
        let md = file.metadata()?;
        if !md.is_file() {
            return Ok(None);
        }
        Ok(Pack::parse(file, path, md.len()))
    }

    fn parse(file: File, path: &Path, len: u64) -> Option<Pack> {
        if len < (MAGIC.len() + TAIL) as u64 {
            return None;
        }
        let mut magic = [0u8; 8];
        sysfs::read_exact_at(&file, &mut magic, 0).ok()?;
        if &magic != MAGIC {
            return None;
        }
        let mut tail = [0u8; TAIL];
        sysfs::read_exact_at(&file, &mut tail, len - TAIL as u64).ok()?;
        if &tail[TAIL - 8..] != TRAILER {
            return None;
        }
        let word = |at: usize| u64::from_le_bytes(tail[at..at + 8].try_into().unwrap());
        let count = word(1024);
        let start = word(1032);
        let index_len = count.checked_mul(ENTRY as u64)?;
        if start < MAGIC.len() as u64 || start.checked_add(index_len)? + TAIL as u64 != len {
            return None;
        }
        let mut index = vec![0u8; index_len as usize];
        sysfs::read_exact_at(&file, &mut index, start).ok()?;
        let mut h = blake3::Hasher::new();
        h.update(&index);
        h.update(&tail[..1040]);
        if h.finalize().as_bytes()[..16] != tail[1040..1056] {
            return None;
        }
        let mut fanout = [0u32; 256];
        for (i, f) in fanout.iter_mut().enumerate() {
            *f = u32::from_le_bytes(tail[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let pack = Pack {
            file,
            path: path.to_path_buf(),
            index,
            fanout,
            len,
        };
        pack.well_formed(start).then_some(pack)
    }

    /// The index is sorted, the fanout agrees with it, and every payload lies
    /// between the header and the index: a reader can then trust what an entry
    /// says without checking it again.
    fn well_formed(&self, start: u64) -> bool {
        let mut counts = [0u32; 256];
        let mut prev: Option<&[u8]> = None;
        for i in 0..self.count() {
            let (id, loc) = self.entry(i);
            if prev.is_some_and(|p| p >= &id[..]) {
                return false;
            }
            prev = Some(&self.index[i * ENTRY..i * ENTRY + 32]);
            counts[id[0] as usize] += 1;
            let fits = loc.offset >= MAGIC.len() as u64
                && loc
                    .offset
                    .checked_add(loc.stored)
                    .is_some_and(|e| e <= start);
            let shape = match loc.flags {
                0 => loc.stored == loc.raw,
                LZ4 => loc.stored < loc.raw,
                _ => false,
            };
            if !fits || !shape {
                return false;
            }
        }
        let mut sum = 0u32;
        for (b, c) in counts.iter().enumerate() {
            sum += c;
            if self.fanout[b] != sum {
                return false;
            }
        }
        true
    }

    fn count(&self) -> usize {
        self.index.len() / ENTRY
    }

    fn entry(&self, i: usize) -> (Id, Loc) {
        let e = &self.index[i * ENTRY..(i + 1) * ENTRY];
        let word = |at: usize| u64::from_le_bytes(e[at..at + 8].try_into().unwrap());
        (
            e[..32].try_into().unwrap(),
            Loc {
                offset: word(32),
                stored: word(40),
                raw: word(48),
                flags: e[56],
            },
        )
    }

    pub fn find(&self, id: &Id) -> Option<Loc> {
        let b = id[0] as usize;
        let lo = if b == 0 {
            0
        } else {
            self.fanout[b - 1] as usize
        };
        let hi = self.fanout[b] as usize;
        let ids = |i: usize| &self.index[i * ENTRY..i * ENTRY + 32];
        let (mut lo, mut hi) = (lo, hi);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match ids(mid).cmp(&id[..]) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(self.entry(mid).1),
            }
        }
        None
    }

    fn stored_bytes(&self, loc: Loc) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; loc.stored as usize];
        sysfs::read_exact_at(&self.file, &mut out, loc.offset).map_err(|_| corrupt("truncated"))?;
        Ok(out)
    }

    pub fn read(&self, loc: Loc) -> io::Result<Vec<u8>> {
        let stored = self.stored_bytes(loc)?;
        if loc.flags & LZ4 == 0 {
            return Ok(stored);
        }
        lz4_flex::block::decompress(&stored, loc.raw as usize)
            .ok()
            .filter(|out| out.len() as u64 == loc.raw)
            .ok_or_else(|| corrupt("a payload does not decompress"))
    }

    pub fn ids(&self) -> impl Iterator<Item = (Id, Loc)> + '_ {
        (0..self.count()).map(|i| self.entry(i))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_len(&self) -> u64 {
        self.len
    }
}

/// An object's payload as a pack stores it, made apart from the writer so that
/// several threads can compress while one appends.
pub struct Prepared {
    payload: Vec<u8>,
    raw: u64,
    flags: u8,
}

pub fn prepare(bytes: Vec<u8>) -> Prepared {
    let raw = bytes.len() as u64;
    let packed = (bytes.len() >= MIN_COMPRESS)
        .then(|| lz4_flex::block::compress(&bytes))
        .filter(|c| c.len() < bytes.len());
    match packed {
        Some(payload) => Prepared {
            payload,
            raw,
            flags: LZ4,
        },
        None => Prepared {
            payload: bytes,
            raw,
            flags: 0,
        },
    }
}

/// A pack being written, under a temporary name in the packs directory, so a
/// killed writer leaves a `.tmp` the sweep removes.
pub struct PackWriter {
    dir: PathBuf,
    tmp: PathBuf,
    file: io::BufWriter<File>,
    at: u64,
    entries: Vec<(Id, Loc)>,
    ids: HashSet<Id>,
}

impl PackWriter {
    pub fn new(dir: &Path) -> io::Result<PackWriter> {
        cachefs::create_dir(dir)?;
        let (f, tmp) = tempfile::Builder::new()
            .prefix(".pack.")
            .suffix(".tmp")
            .tempfile_in(dir)?
            .keep()
            .map_err(|e| e.error)?;
        let mut file = io::BufWriter::with_capacity(1 << 20, f);
        file.write_all(MAGIC)?;
        Ok(PackWriter {
            dir: dir.to_path_buf(),
            tmp,
            file,
            at: MAGIC.len() as u64,
            entries: Vec::new(),
            ids: HashSet::new(),
        })
    }

    pub fn add(&mut self, id: Id, bytes: &[u8]) -> io::Result<()> {
        self.add_prepared(id, prepare(bytes.to_vec()))
    }

    pub fn add_prepared(&mut self, id: Id, p: Prepared) -> io::Result<()> {
        if !self.ids.insert(id) {
            return Ok(());
        }
        self.push(id, &p.payload, p.raw, p.flags)
    }

    /// An object of `raw` bytes copied from `r` as it is, for one of at least
    /// [`STREAM`] bytes.
    pub fn add_stream(&mut self, id: Id, r: &mut dyn Read, raw: u64) -> io::Result<()> {
        if !self.ids.insert(id) {
            return Ok(());
        }
        if io::copy(r, &mut self.file)? != raw {
            return Err(io::Error::other(
                "an object changed size while it was stored",
            ));
        }
        self.entries.push((
            id,
            Loc {
                offset: self.at,
                stored: raw,
                raw,
                flags: 0,
            },
        ));
        self.at += raw;
        Ok(())
    }

    /// An object copied from another pack as it is stored, for a compaction:
    /// nothing is decompressed or compressed again. It goes through in
    /// chunks, as a streamed object may be any size.
    pub fn copy(&mut self, id: Id, from: &Pack, loc: Loc) -> io::Result<()> {
        if !self.ids.insert(id) {
            return Ok(());
        }
        let mut buf = vec![0u8; loc.stored.min(1 << 20) as usize];
        let mut done = 0;
        while done < loc.stored {
            let n = (loc.stored - done).min(buf.len() as u64) as usize;
            sysfs::read_exact_at(&from.file, &mut buf[..n], loc.offset + done)
                .map_err(|_| corrupt("truncated"))?;
            self.file.write_all(&buf[..n])?;
            done += n as u64;
        }
        self.entries.push((
            id,
            Loc {
                offset: self.at,
                ..loc
            },
        ));
        self.at += loc.stored;
        Ok(())
    }

    fn push(&mut self, id: Id, payload: &[u8], raw: u64, flags: u8) -> io::Result<()> {
        self.file.write_all(payload)?;
        self.entries.push((
            id,
            Loc {
                offset: self.at,
                stored: payload.len() as u64,
                raw,
                flags,
            },
        ));
        self.at += payload.len() as u64;
        Ok(())
    }

    /// Writes the index, syncs and renames the pack into place, named by the
    /// checksum of its index. `None` when it holds nothing: no file is left.
    pub fn finish(mut self) -> io::Result<Option<PathBuf>> {
        if self.entries.is_empty() {
            let _ = fs::remove_file(&self.tmp);
            return Ok(None);
        }
        self.entries.sort_unstable_by_key(|e| e.0);
        let mut index = Vec::with_capacity(self.entries.len() * ENTRY);
        let mut fanout = [0u32; 256];
        for (id, loc) in &self.entries {
            index.extend_from_slice(id);
            index.extend_from_slice(&loc.offset.to_le_bytes());
            index.extend_from_slice(&loc.stored.to_le_bytes());
            index.extend_from_slice(&loc.raw.to_le_bytes());
            index.push(loc.flags);
            fanout[id[0] as usize] += 1;
        }
        let mut tail = Vec::with_capacity(TAIL);
        let mut sum = 0u32;
        for c in fanout {
            sum += c;
            tail.extend_from_slice(&sum.to_le_bytes());
        }
        tail.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        tail.extend_from_slice(&self.at.to_le_bytes());
        let mut h = blake3::Hasher::new();
        h.update(&index);
        h.update(&tail);
        let digest = h.finalize();
        tail.extend_from_slice(&digest.as_bytes()[..16]);
        tail.extend_from_slice(TRAILER);
        self.file.write_all(&index)?;
        self.file.write_all(&tail)?;
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        let name: String = digest.as_bytes()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let path = self.dir.join(format!("{name}.pack"));
        fs::rename(&self.tmp, &path)?;
        Ok(Some(path))
    }
}

impl Drop for PackWriter {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.tmp);
    }
}

#[derive(Default, Debug)]
pub struct PackSet {
    packs: Vec<Arc<Pack>>,
}

impl PackSet {
    /// Every whole pack in `dir`. A pack that does not parse is removed: it is
    /// a cache file, and its objects come back with the inputs that need them.
    pub fn load(dir: &Path) -> PackSet {
        let mut packs = Vec::new();
        let Ok(rd) = fs::read_dir(dir) else {
            return PackSet::default();
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "pack") {
                continue;
            }
            match Pack::open(&path) {
                Ok(Some(p)) => packs.push(Arc::new(p)),
                Ok(None) => {
                    let _ = fs::remove_file(&path);
                }
                Err(_) => {}
            }
        }
        PackSet { packs }
    }

    pub fn find(&self, id: &Id) -> Option<(&Arc<Pack>, Loc)> {
        self.packs
            .iter()
            .find_map(|p| p.find(id).map(|loc| (p, loc)))
    }

    pub fn packs(&self) -> &[Arc<Pack>] {
        &self.packs
    }
}

/// The packs of a store as a process sees them: one listing, taken again when
/// an object is not in it, because a compaction since may have moved it. A
/// pack this process has open stays readable after a sweep unlinks it.
#[derive(Debug)]
pub struct Packs {
    dir: PathBuf,
    set: RwLock<Arc<PackSet>>,
}

impl Packs {
    pub fn open(dir: &Path) -> Packs {
        Packs {
            dir: dir.to_path_buf(),
            set: RwLock::new(Arc::new(PackSet::load(dir))),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn snapshot(&self) -> Arc<PackSet> {
        self.set.read().unwrap().clone()
    }

    pub fn refresh(&self) -> Arc<PackSet> {
        let fresh = Arc::new(PackSet::load(&self.dir));
        *self.set.write().unwrap() = fresh.clone();
        fresh
    }

    pub fn contains(&self, id: &Id) -> bool {
        self.snapshot().find(id).is_some()
    }

    /// An object's bytes. `NotFound` when no pack holds it even after a fresh
    /// listing: the entry that named it is then not whole.
    pub fn read(&self, id: &Id) -> io::Result<Vec<u8>> {
        let set = self.snapshot();
        if let Some((p, loc)) = set.find(id) {
            return p.read(loc);
        }
        let set = self.refresh();
        match set.find(id) {
            Some((p, loc)) => p.read(loc),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "an object of this input is missing from the input store",
            )),
        }
    }
}

/// What a collection did, or would do on a dry run.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Collected {
    pub removed: usize,
    pub rewritten: usize,
    pub freed: u64,
}

/// Removes every pack that holds no object in `live`, and rewrites a pack whose
/// dead bytes outweigh its live ones into a new pack of the live ones only. The
/// caller holds the store's lock, so no writer publishes an entry meanwhile;
/// a reader that has a pack open keeps reading it after the unlink.
pub fn collect(dir: &Path, live: &HashSet<Id>, dry_run: bool) -> io::Result<Collected> {
    let mut out = Collected::default();
    for pack in PackSet::load(dir).packs() {
        let (mut keep, mut dead) = (0u64, 0u64);
        for (id, loc) in pack.ids() {
            if live.contains(&id) {
                keep += loc.stored;
            } else {
                dead += loc.stored;
            }
        }
        if keep == 0 {
            out.removed += 1;
            out.freed += pack.file_len();
            if !dry_run {
                fs::remove_file(pack.path())?;
            }
        } else if dead > keep {
            out.rewritten += 1;
            out.freed += dead;
            if !dry_run {
                let mut w = PackWriter::new(dir)?;
                for (id, loc) in pack.ids().filter(|(id, _)| live.contains(id)) {
                    w.copy(id, pack, loc)?;
                }
                let new = w.finish()?;
                if new.as_deref() != Some(pack.path()) {
                    fs::remove_file(pack.path())?;
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn objects() -> Vec<Vec<u8>> {
        vec![
            b"".to_vec(),
            b"short".to_vec(),
            vec![b'a'; 4096],
            (0..5000u32).flat_map(|i| i.to_le_bytes()).collect(),
        ]
    }

    #[test]
    fn a_streamed_object_reads_back() {
        let d = tempdir().unwrap();
        let o = vec![7u8; 4096];
        let id = id_of_reader(&mut &o[..]).unwrap();
        assert_eq!(id, id_of(&o));
        let mut w = PackWriter::new(d.path()).unwrap();
        w.add_stream(id, &mut &o[..], o.len() as u64).unwrap();
        let p = Pack::open(&w.finish().unwrap().unwrap()).unwrap().unwrap();
        assert_eq!(p.read(p.find(&id).unwrap()).unwrap(), o);
    }

    #[test]
    fn a_pack_reads_back_what_was_written() {
        let d = tempdir().unwrap();
        let mut w = PackWriter::new(d.path()).unwrap();
        for o in objects() {
            w.add(id_of(&o), &o).unwrap();
        }
        let path = w.finish().unwrap().expect("a pack");
        let p = Pack::open(&path).unwrap().expect("well formed");
        for o in objects() {
            let loc = p.find(&id_of(&o)).expect("indexed");
            assert_eq!(p.read(loc).unwrap(), o);
        }
        assert!(p.find(&id_of(b"absent")).is_none());
        let a = p.find(&id_of(&vec![b'a'; 4096])).unwrap();
        assert!(a.stored < a.raw, "a repetitive payload is compressed");
    }

    #[test]
    fn an_empty_writer_leaves_no_file() {
        let d = tempdir().unwrap();
        assert!(
            PackWriter::new(d.path())
                .unwrap()
                .finish()
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_changed_index_byte_is_not_a_pack() {
        let d = tempdir().unwrap();
        let mut w = PackWriter::new(d.path()).unwrap();
        for o in objects() {
            w.add(id_of(&o), &o).unwrap();
        }
        let path = w.finish().unwrap().unwrap();
        let good = fs::read(&path).unwrap();
        let start = good.len() - TAIL - objects().len() * ENTRY;
        for i in (start..good.len()).step_by(7) {
            let mut bad = good.clone();
            bad[i] ^= 1;
            fs::write(&path, &bad).unwrap();
            assert!(
                Pack::open(&path).unwrap().is_none(),
                "byte {i} was believed"
            );
        }
        fs::write(&path, &good[..good.len() - 1]).unwrap();
        assert!(Pack::open(&path).unwrap().is_none(), "a truncated pack");
    }

    #[test]
    fn a_set_finds_objects_across_packs_and_drops_a_broken_one() {
        let d = tempdir().unwrap();
        let mut a = PackWriter::new(d.path()).unwrap();
        a.add(id_of(b"one"), b"one").unwrap();
        a.finish().unwrap();
        let mut b = PackWriter::new(d.path()).unwrap();
        b.add(id_of(b"two"), b"two").unwrap();
        b.finish().unwrap();
        fs::write(d.path().join("junk.pack"), b"not a pack").unwrap();
        let packs = Packs::open(d.path());
        assert_eq!(packs.read(&id_of(b"one")).unwrap(), b"one");
        assert_eq!(packs.read(&id_of(b"two")).unwrap(), b"two");
        assert!(!d.path().join("junk.pack").exists());
        let err = packs.read(&id_of(b"three")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_reader_sees_a_pack_written_after_it_listed() {
        let d = tempdir().unwrap();
        let packs = Packs::open(d.path());
        let mut w = PackWriter::new(d.path()).unwrap();
        w.add(id_of(b"late"), b"late").unwrap();
        w.finish().unwrap();
        assert_eq!(packs.read(&id_of(b"late")).unwrap(), b"late");
    }

    #[test]
    fn collect_removes_dead_packs_and_compacts_mostly_dead_ones() {
        let d = tempdir().unwrap();
        let big = |c: u8| {
            vec![c; 10_000]
                .into_iter()
                .chain([c ^ 0x55; 3])
                .collect::<Vec<u8>>()
        };
        let (keep, drop, gone) = (b"keep, small".to_vec(), big(1), big(2));
        let mut w = PackWriter::new(d.path()).unwrap();
        w.add(id_of(&keep), &keep).unwrap();
        w.add(id_of(&drop), &drop).unwrap();
        let mixed = w.finish().unwrap().unwrap();
        let mut w = PackWriter::new(d.path()).unwrap();
        w.add(id_of(&gone), &gone).unwrap();
        let dead = w.finish().unwrap().unwrap();
        let live: HashSet<Id> = [id_of(&keep)].into();

        let dry = collect(d.path(), &live, true).unwrap();
        assert_eq!((dry.removed, dry.rewritten), (1, 1));
        assert!(mixed.exists() && dead.exists(), "a dry run changes nothing");

        let reader = Packs::open(d.path());
        collect(d.path(), &live, false).unwrap();
        assert!(!dead.exists() && !mixed.exists());
        assert_eq!(
            reader.read(&id_of(&keep)).unwrap(),
            keep,
            "from the unlinked pack"
        );
        assert_eq!(reader.refresh().packs().len(), 1);
        assert_eq!(
            reader.read(&id_of(&keep)).unwrap(),
            keep,
            "from the new pack"
        );
        assert!(reader.read(&id_of(&drop)).is_err());
    }
}
