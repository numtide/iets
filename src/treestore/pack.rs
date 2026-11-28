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

use crate::sys::{self, fs as sysfs};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as Atomic};
use std::sync::{Arc, OnceLock, RwLock};

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

/// Packs a process keeps open, well under the usual descriptor limits (1024,
/// macOS 256). Further packs are opened per read and can be gone by then.
const OPEN_PACKS: usize = 128;
static OPEN: AtomicUsize = AtomicUsize::new(0);

/// The tests of one binary share `OPEN`: a test that fills it writes here,
/// and a test that needs a pack held open reads.
#[cfg(test)]
pub(super) static OPEN_IN_TESTS: RwLock<()> = RwLock::new(());

/// `FAN_IN` packs of one size class (a factor of four) merge into one of the
/// next class. A pack of `SMALL` or more stays as it is.
const SMALL: u64 = 64 << 20;
const FAN_IN: usize = 4;

/// How often a read lists the packs again: a merge can remove a pack during
/// a listing, or between the listing and the read of a pack not held open.
const LISTINGS: usize = 3;

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
    file: OnceLock<File>,
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
        let pack = Pack::parse(&file, path, md.len())?;
        if let Some(p) = &pack {
            let _ = p.retain(file);
        }
        Ok(pack)
    }

    /// Keeps `file` open for the reads to come, when the process may hold one
    /// more; hands it back when it may not.
    fn retain(&self, file: File) -> Result<&File, File> {
        if OPEN.fetch_add(1, Atomic::Relaxed) >= OPEN_PACKS {
            OPEN.fetch_sub(1, Atomic::Relaxed);
            return Err(file);
        }
        if self.file.set(file).is_err() {
            OPEN.fetch_sub(1, Atomic::Relaxed);
        }
        Ok(self.file.get().expect("set above, or by a sibling"))
    }

    /// The name of a pack is the checksum of its index, so the file at its
    /// path is the pack that was parsed, or is not there.
    fn with_file<T>(&self, f: impl FnOnce(&File) -> io::Result<T>) -> io::Result<T> {
        if let Some(file) = self.file.get() {
            return f(file);
        }
        let file = sysfs::no_follow_no_block(File::options().read(true)).open(&self.path)?;
        let md = file.metadata()?;
        if !md.is_file() || md.len() != self.len {
            return Err(corrupt("changed since it was listed"));
        }
        match self.retain(file) {
            Ok(file) => f(file),
            Err(file) => f(&file),
        }
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.with_file(|file| {
            sysfs::read_exact_at(file, buf, offset).map_err(|_| corrupt("truncated"))
        })
    }

    fn parse(file: &File, path: &Path, len: u64) -> io::Result<Option<Pack>> {
        if len < (MAGIC.len() + TAIL) as u64 {
            return Ok(None);
        }
        let mut magic = [0u8; 8];
        sysfs::read_exact_at(file, &mut magic, 0)?;
        if &magic != MAGIC {
            return Ok(None);
        }
        let mut tail = [0u8; TAIL];
        sysfs::read_exact_at(file, &mut tail, len - TAIL as u64)?;
        if &tail[TAIL - 8..] != TRAILER {
            return Ok(None);
        }
        let word = |at: usize| u64::from_le_bytes(tail[at..at + 8].try_into().unwrap());
        let count = word(1024);
        let start = word(1032);
        let Some(index_len) = count.checked_mul(ENTRY as u64) else {
            return Ok(None);
        };
        if start < MAGIC.len() as u64 || start.checked_add(index_len) != Some(len - TAIL as u64) {
            return Ok(None);
        }
        let mut index = vec![0u8; index_len as usize];
        sysfs::read_exact_at(file, &mut index, start)?;
        let mut h = blake3::Hasher::new();
        h.update(&index);
        h.update(&tail[..1040]);
        if h.finalize().as_bytes()[..16] != tail[1040..1056] {
            return Ok(None);
        }
        let mut fanout = [0u32; 256];
        for (i, f) in fanout.iter_mut().enumerate() {
            *f = u32::from_le_bytes(tail[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let pack = Pack {
            file: OnceLock::new(),
            path: path.to_path_buf(),
            index,
            fanout,
            len,
        };
        Ok(pack.well_formed(start).then_some(pack))
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
                // LZ4 expands a byte at most 255 times
                LZ4 => loc.stored < loc.raw && loc.raw / 255 <= loc.stored,
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
        self.read_at(&mut out, loc.offset)?;
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

impl Drop for Pack {
    fn drop(&mut self) {
        if self.file.get().is_some() {
            OPEN.fetch_sub(1, Atomic::Relaxed);
        }
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
    /// The bytes the disk was asked to start writing (`start_writeback`).
    written_back: u64,
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
            written_back: 0,
        })
    }

    /// Asks the disk to start writing what the pack holds so far, so the
    /// sync in `finish` waits for the last of it rather than all of it.
    pub fn start_writeback(&mut self) {
        let handed = self.at - self.file.buffer().len() as u64;
        if handed > self.written_back {
            sys::start_writeback(
                self.file.get_ref(),
                self.written_back,
                handed - self.written_back,
            );
            self.written_back = handed;
        }
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
            from.read_at(&mut buf[..n], loc.offset + done)?;
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
        // The writer's open descriptor works even if umask removed owner read.
        // A replacement must be readable before collection unlinks its source.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            self.file
                .get_ref()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        // a reader checks the index, not the objects, so the bytes go
        // down before the name
        sysfs::sync_ordered(self.file.get_ref())?;
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
    /// Packs that do not parse, with their size, for [`collect`] to remove.
    broken: Vec<(PathBuf, u64)>,
}

impl PackSet {
    /// Every whole pack in `dir`. Only regular files count, so a link never
    /// leads a later removal out of the store.
    pub fn load(dir: &Path) -> PackSet {
        Self::list(dir, |_, _| {})
    }

    fn list(dir: &Path, mut on_error: impl FnMut(&Path, io::Error)) -> PackSet {
        let mut set = PackSet::default();
        let rd = match fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    on_error(dir, e);
                }
                return set;
            }
        };
        for entry in rd {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    on_error(dir, e);
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().is_some_and(|x| x == "pack")
                && let Err(e) = set.add_entry(&path, &entry)
                && e.kind() != io::ErrorKind::NotFound
            {
                on_error(&path, e);
            }
        }
        set
    }

    fn add_entry(&mut self, path: &Path, entry: &fs::DirEntry) -> io::Result<()> {
        if entry.file_type()?.is_file() {
            match Pack::open(path)? {
                Some(p) => self.packs.push(Arc::new(p)),
                None => self
                    .broken
                    .push((path.to_owned(), entry.metadata().map_or(0, |m| m.len()))),
            }
        }
        Ok(())
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
/// an object is not in it, or its pack is gone, because a compaction since may
/// have moved it. A pack this process has open stays readable after a sweep
/// unlinks it.
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
        self.refresh_with(|_, _| {})
    }

    fn refresh_with(&self, on_error: impl FnMut(&Path, io::Error)) -> Arc<PackSet> {
        let fresh = Arc::new(PackSet::list(&self.dir, on_error));
        *self.set.write().unwrap() = fresh.clone();
        fresh
    }

    pub fn contains(&self, id: &Id) -> bool {
        self.snapshot().find(id).is_some()
    }

    /// Lists the packs again and merges those of each full size class, until
    /// no class is full: a `put` runs it after its own pack, so the store
    /// holds a few packs per class and not one per put. The caller holds the
    /// store's lock. It does not know what is live, and copies every object;
    /// `collect` drops the dead ones. An error leaves the listing current.
    pub fn merge(&self) -> io::Result<()> {
        while self.merge_full(&self.refresh())? {}
        Ok(())
    }

    /// Merges each full class of `set`. `false` when no class is full any
    /// more, or a pack stayed: it is in its class for the next put.
    fn merge_full(&self, set: &PackSet) -> io::Result<bool> {
        let mut classes: BTreeMap<u32, Vec<&Arc<Pack>>> = BTreeMap::new();
        for p in set.packs().iter().filter(|p| p.file_len() < SMALL) {
            classes
                .entry(p.file_len().max(1).ilog2() / 2)
                .or_default()
                .push(p);
        }
        classes.retain(|_, c| c.len() >= FAN_IN);
        if classes.is_empty() {
            return Ok(false);
        }
        let all = classes.into_values().try_fold(true, |all, class| {
            io::Result::Ok(all & merge_class(&self.dir, &class)?)
        });
        if !matches!(all, Ok(true)) {
            // the classes before this one are merged, and their packs gone
            self.refresh();
        }
        all
    }

    /// An object's bytes. `NotFound` when no pack holds it even after fresh
    /// listings. A miss after an incomplete listing reports its I/O error.
    pub fn read(&self, id: &Id) -> io::Result<Vec<u8>> {
        let mut listing_error = None;
        let result = self.read_listing(id, || {
            listing_error = None;
            self.refresh_with(|_, e| {
                listing_error.get_or_insert(e);
            })
        });
        match result {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(listing_error.unwrap_or(e)),
            other => other,
        }
    }

    fn read_listing(&self, id: &Id, mut list: impl FnMut() -> Arc<PackSet>) -> io::Result<Vec<u8>> {
        let mut set = self.snapshot();
        for _ in 0..LISTINGS {
            match set.find(id).map(|(p, loc)| p.read(loc)) {
                Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => {}
                Some(r) => return r,
                None => {}
            }
            set = list();
        }
        match set.find(id) {
            Some((p, loc)) => p.read(loc),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "an object of this input is missing from the input store",
            )),
        }
    }
}

/// One pack of all that the packs of `class` hold. `false` when one of them
/// could not be removed.
fn merge_class(dir: &Path, class: &[&Arc<Pack>]) -> io::Result<bool> {
    let mut w = PackWriter::new(dir)?;
    for pack in class {
        for (id, loc) in pack.ids() {
            w.copy(id, pack, loc)?;
        }
    }
    let new = w.finish()?;
    Ok(class
        .iter()
        .filter(|p| new.as_deref() != Some(p.path()))
        .all(|p| remove(p.path(), false)))
}

/// What a collection did, or would do on a dry run.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Collected {
    pub removed: usize,
    pub rewritten: usize,
    pub freed: u64,
}

/// Whether `path` went, or would go on a dry run. A pack another process
/// holds open without delete sharing stays, on Windows, for the next
/// collection; it is not counted.
fn remove(path: &Path, dry_run: bool) -> bool {
    dry_run || fs::remove_file(path).is_ok()
}

/// Removes every pack that holds no object in `live`, and rewrites a pack whose
/// dead bytes outweigh its live ones into a new pack of the live ones only. The
/// caller holds the store's lock, so no writer publishes an entry meanwhile;
/// a reader that has a pack open keeps reading it after the unlink.
pub fn collect(dir: &Path, live: &HashSet<Id>, dry_run: bool) -> io::Result<Collected> {
    let mut out = Collected::default();
    let set = PackSet::list(dir, |path, e| {
        errln!("cache: skipping input pack {}: {e}", path.display());
    });
    // a pack that does not parse is a cache file whose objects come back
    // with the inputs that need them
    for (path, len) in &set.broken {
        if remove(path, dry_run) {
            out.removed += 1;
            out.freed += len;
        }
    }
    for pack in set.packs() {
        let (mut keep, mut dead) = (0u64, 0u64);
        for (id, loc) in pack.ids() {
            if live.contains(&id) {
                keep += loc.stored;
            } else {
                dead += loc.stored;
            }
        }
        if keep == 0 {
            if remove(pack.path(), dry_run) {
                out.removed += 1;
                out.freed += pack.file_len();
            }
        } else if dead > keep {
            if !dry_run {
                let mut w = PackWriter::new(dir)?;
                for (id, loc) in pack.ids().filter(|(id, _)| live.contains(id)) {
                    w.copy(id, pack, loc)?;
                }
                let new = w.finish()?;
                if new.as_deref() != Some(pack.path()) && !remove(pack.path(), false) {
                    continue;
                }
            }
            out.rewritten += 1;
            out.freed += dead;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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
        // Model a temporary created under umask 0400 without changing the
        // process-wide umask of parallel tests.
        #[cfg(unix)]
        w.file
            .get_ref()
            .set_permissions(fs::Permissions::from_mode(0o200))
            .unwrap();
        let path = w.finish().unwrap().expect("a pack");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
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
    fn a_set_finds_objects_across_packs_and_only_collect_drops_a_broken_one() {
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
        let err = packs.read(&id_of(b"three")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let junk = d.path().join("junk.pack");
        let live: HashSet<Id> = [id_of(b"one"), id_of(b"two")].into();
        let dry = collect(d.path(), &live, true).unwrap();
        assert!(junk.exists(), "a dry run removes nothing");
        assert_eq!((dry.removed, dry.freed), (1, 10));
        assert_eq!(collect(d.path(), &live, false).unwrap(), dry);
        assert!(!junk.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_pack_that_cannot_be_removed_stays_uncounted_and_collection_goes_on() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("junk.pack"), b"not a pack").unwrap();
        let mut w = PackWriter::new(d.path()).unwrap();
        w.add(id_of(b"dead"), b"dead").unwrap();
        w.finish().unwrap();
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let held = collect(d.path(), &HashSet::new(), false).unwrap();
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(held, Collected::default());
        assert_eq!(PackSet::load(d.path()).packs().len(), 1, "still there");
        assert_eq!(
            collect(d.path(), &HashSet::new(), false).unwrap().removed,
            2
        );
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

    fn one_pack(dir: &Path, bytes: &[u8]) -> PathBuf {
        let mut w = PackWriter::new(dir).unwrap();
        w.add(id_of(bytes), bytes).unwrap();
        w.finish().unwrap().unwrap()
    }

    #[test]
    fn a_process_keeps_a_bounded_number_of_packs_open() {
        let _filled = OPEN_IN_TESTS.write().unwrap();
        let d = tempdir().unwrap();
        let n = OPEN_PACKS + 8;
        for i in 0..n {
            one_pack(d.path(), format!("object {i}").as_bytes());
        }
        let packs = Packs::open(d.path());
        let set = packs.snapshot();
        assert_eq!(set.packs().len(), n);
        for i in 0..n {
            let bytes = format!("object {i}").into_bytes();
            assert_eq!(packs.read(&id_of(&bytes)).unwrap(), bytes);
        }
        let open = set.packs().iter().filter(|p| p.file.get().is_some());
        assert!(open.count() <= OPEN_PACKS);
    }

    #[test]
    fn a_miss_reports_why_the_directory_could_not_be_listed() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("file");
        File::create(&file).unwrap();
        let expected = fs::read_dir(&file).unwrap_err();
        let err = Packs::open(&file).read(&id_of(b"object")).unwrap_err();
        assert_eq!(err.raw_os_error(), expected.raw_os_error());
    }

    /// `p` as a process past its bound lists it: not held open.
    fn closed(p: &Pack, path: &Path, len: u64) -> Arc<Pack> {
        Arc::new(Pack {
            file: OnceLock::new(),
            path: path.to_path_buf(),
            index: p.index.clone(),
            fanout: p.fanout,
            len,
        })
    }

    fn set_of(packs: impl Iterator<Item = Arc<Pack>>) -> Arc<PackSet> {
        Arc::new(PackSet {
            packs: packs.collect(),
            broken: Vec::new(),
        })
    }

    #[test]
    fn a_read_follows_a_second_merge() {
        let d = tempdir().unwrap();
        let real = Pack::open(&one_pack(d.path(), b"object")).unwrap().unwrap();
        // two listings, each naming a pack that a merge removed before the read
        let gone = |name: &str| set_of([closed(&real, &d.path().join(name), real.len)].into_iter());
        let reader = Packs {
            dir: d.path().to_path_buf(),
            set: RwLock::new(gone("first.pack")),
        };
        let mut stale = Some(gone("second.pack"));
        let read = reader.read_listing(&id_of(b"object"), || {
            stale.take().unwrap_or_else(|| reader.refresh())
        });
        assert_eq!(read.unwrap(), b"object");
    }

    #[test]
    fn a_read_follows_a_merge_its_listing_ran_into() {
        let d = tempdir().unwrap();
        one_pack(d.path(), b"object");
        // a listing that saw the old packs go and not yet the new one
        let empty = || set_of([].into_iter());
        let reader = Packs {
            dir: d.path().to_path_buf(),
            set: RwLock::new(empty()),
        };
        let mut stale = Some(empty());
        let read = reader.read_listing(&id_of(b"object"), || {
            stale.take().unwrap_or_else(|| reader.refresh())
        });
        assert_eq!(read.unwrap(), b"object");
    }

    #[test]
    fn a_merge_that_fails_part_way_lists_the_packs_again() {
        let d = tempdir().unwrap();
        let old: Vec<PathBuf> = (0..FAN_IN)
            .map(|i| one_pack(d.path(), format!("object {i}").as_bytes()))
            .collect();
        let packs = Packs::open(d.path());
        // a full class above, whose packs went before the copy
        let listed = packs.snapshot();
        let real = &listed.packs()[0];
        let gone = (0..FAN_IN).map(|i| {
            closed(
                real,
                &d.path().join(format!("gone {i}.pack")),
                real.len << 4,
            )
        });
        let set = set_of(listed.packs().iter().cloned().chain(gone));

        assert!(packs.merge_full(&set).is_err());
        assert!(old.iter().all(|p| !p.exists()));
        let now = packs.snapshot();
        assert_eq!(now.packs().len(), 1);
        assert!(now.packs().iter().all(|p| p.path().exists()));
    }

    #[test]
    fn a_reader_without_the_packs_open_follows_a_merge() {
        let d = tempdir().unwrap();
        let objects: Vec<Vec<u8>> = (0..FAN_IN)
            .map(|i| format!("object {i}").into_bytes())
            .collect();
        let old: Vec<PathBuf> = objects.iter().map(|o| one_pack(d.path(), o)).collect();

        // a reader past its bound: no pack is held open
        let listed = PackSet::load(d.path());
        let closed = listed.packs().iter().map(|p| closed(p, &p.path, p.len));
        let reader = Packs {
            dir: d.path().to_path_buf(),
            set: RwLock::new(set_of(closed)),
        };

        Packs::open(d.path()).merge().unwrap();
        assert!(old.iter().all(|p| !p.exists()));
        assert_eq!(PackSet::load(d.path()).packs().len(), 1);
        for o in &objects {
            assert_eq!(&reader.read(&id_of(o)).unwrap(), o);
        }
    }

    #[test]
    fn a_merge_after_each_pack_keeps_the_packs_few() {
        let d = tempdir().unwrap();
        let reader = Packs::open(d.path());
        let objects: Vec<Vec<u8>> = (0..200)
            .map(|i| format!("object {i} ").repeat(1 + i % 7).into_bytes())
            .collect();
        for (i, o) in objects.iter().enumerate() {
            one_pack(d.path(), o);
            reader.merge().unwrap();
            if i == FAN_IN - 2 {
                assert_eq!(PackSet::load(d.path()).packs().len(), FAN_IN - 1);
            }
        }
        let left = PackSet::load(d.path()).packs().len();
        assert!(left < 4 * FAN_IN, "{left} packs for 200 puts");
        for o in &objects {
            assert_eq!(&reader.read(&id_of(o)).unwrap(), o);
        }
    }
}
