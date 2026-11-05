//! On-disk hygiene shared by the compiled-IR cache (`src/compile/cache.rs`),
//! the tracing run cache (`src/trace/`) and the input store
//! (`crate::treestore`): a format version file, a single-writer lock, mtime
//! touches, and TTL sweeps. The roots store (`roots/`,
//! `docs/explanation/caching.md`) takes the TTL sweep and nothing else: a
//! symlink has no format to version and no torn state to wipe.
//!
//! Many `iets` processes share one cache directory (eval-jobs workers,
//! direnv, the classic command names). The invariant they hold together:
//! **an entry is one file, written to a temporary name and renamed into
//! place, so one rename publishes the whole entry, a reader sees either the
//! whole previous entry or the whole new one and never locks; a writer holds
//! the store's `flock` for exactly one such file, and a writer that cannot
//! take it within `LOCK_TIMEOUT` drops the write.** A
//! dropped write costs a cache miss later, which is why the caches never wait
//! on a sibling worker or fail a run. The input store waits instead
//! (`wait_lock`), because a dropped write there costs a download.

use crate::sys::OsStrExt;
use crate::sys::fs as sysfs;
use crate::sys::tmp;
use io::Write;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::str::from_utf8;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, io, thread};

use crate::compile::cache::IrCache;
use crate::session::Session;
use crate::store::hash::is_nix32_char;
use crate::store::path::StorePath;
use crate::sys;
use crate::trace::{self, store};
use crate::treestore::{self, pack};

/// The store layout. A mismatch wipes the store; bump it when an entry
/// encoding changes in a way the keys do not already cover.
pub const FORMAT: &[u8] = b"3";

/// The layout of a store of `shape`. The input store has its own, so a change
/// to its entries wipes it alone.
fn format(shape: Shape) -> &'static [u8] {
    match shape {
        Shape::StoreEntries => b"2",
        Shape::Dirs | Shape::ShardedFiles | Shape::Links => FORMAT,
        Shape::Clones | Shape::Objects => b"1",
    }
}

const DAY: u64 = 24 * 60 * 60;
/// Entries unread for this long are swept.
pub const TTL_DAYS: u64 = 30;
/// A hit inside this window writes nothing.
const TOUCH_INTERVAL: Duration = Duration::from_secs(DAY);
/// A sweep runs at most this often per store.
const GC_INTERVAL: Duration = Duration::from_secs(DAY);
/// Leftover temporary files from a killed writer go after this long.
const TMP_TTL: Duration = Duration::from_secs(DAY);
const LOCK_TIMEOUT: Duration = Duration::from_millis(50);
const LOCK_POLL: Duration = Duration::from_millis(2);

const VERSION: &str = "version";
const LOCK: &str = "lock";
const STAMP: &str = "gc-stamp";
const ENV_KEY: &str = "env-key";
const PACKS: &str = pack::PACKS;

/// A cache entry holds what an evaluation read — the bytes of every file and
/// the answer of every `getEnv` — so it is the user's, not the machine's.
/// Directories and files carry these modes whatever the umask.
const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// `create_dir_all` with `DIR_MODE` on every component it makes.
pub fn create_dir(path: &Path) -> io::Result<()> {
    sysfs::dir_mode(fs::DirBuilder::new().recursive(true), DIR_MODE).create(path)
}

/// Narrows `path` to [`DIR_MODE`] when it is wider. A cache root a run of an
/// older iets made follows the umask, and the user cannot be asked to fix
/// it: tightening is the whole repair, and it costs the run nothing, where
/// refusing the directory would cost every entry in it.
fn tighten(path: &Path) {
    sysfs::restrict_to_owner(path);
}

/// The directory every cache store lives under: `--cache-dir DIR`, else
/// `IETS_CACHE_DIR`, else `$XDG_CACHE_HOME/iets`, else `~/.cache/iets`.
/// Each store takes a subdirectory of it — `trace/`, `ir/`, `inputs/`,
/// `roots/` — so the directory the user names is never a removal target.
/// `None` when no home is known.
pub fn root(override_dir: Option<&str>) -> Option<PathBuf> {
    if let Some(d) = override_dir {
        return Some(PathBuf::from(d));
    }
    let session = Session::current();
    if let Some(d) = session.cache_dir() {
        return Some(d.clone());
    }
    if let Some(d) = &session.settings().cache_dir {
        return Some(d.clone());
    }
    let base = match env::var_os("XDG_CACHE_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(env::var_os("HOME")?).join(".cache"),
    };
    Some(base.join("iets"))
}

/// The GC roots iets owns, under `<cache root>/roots`. A root is not a cache
/// entry: it holds nothing that can be recomputed, it is a claim on the
/// daemon's store. The TTL sweep still fits — a root nobody has used in
/// `TTL_DAYS` days is one nobody wants kept — so `iets cache gc` walks it,
/// but the version file, the writer lock and poison-and-wipe do not, which is
/// why it is not `prepare`d.
pub fn roots(override_dir: Option<&str>) -> Option<PathBuf> {
    Some(root(override_dir)?.join("roots"))
}

/// The link in the roots store that `parts` name — what the root is for, then
/// whatever tells two of them apart. They are length-prefixed into one 64-hex
/// key, so no part can spell another one's boundary.
pub fn root_link(override_dir: Option<&str>, parts: &[&[u8]]) -> Option<PathBuf> {
    let mut bytes = Vec::new();
    for p in parts {
        bytes.extend_from_slice(&(p.len() as u64).to_le_bytes());
        bytes.extend_from_slice(p);
    }
    let name: String = trace::sha256(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(roots(override_dir)?.join(name))
}

/// Whether a cache entry may send a run to `program`: it has to be a file
/// inside `out_path`, and `out_path` a path of the store directory. A cache
/// directory is writable by whoever runs iets, so an entry that named a
/// program freely would be a way to start one; what the store holds under
/// that name is the daemon's to say. `nix fmt` is the one command that runs
/// what an entry remembered.
pub fn program_of_output(out_path: &str, program: &str) -> bool {
    let Ok(out) = StorePath::parse(out_path) else {
        return false;
    };
    let program = Path::new(program);
    program.starts_with(out.to_full_string())
        && !program.components().any(|c| c == Component::ParentDir)
}

/// How a store names its entries under its root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    /// One directory per entry, named by the 64-hex run key (tracing cache).
    Dirs,
    /// One file per entry, named by its key past the two-character shard
    /// directory the key's first two characters make (IR cache).
    ShardedFiles,
    /// One file per entry, named by a store path's base name, and the packs
    /// in `packs/` (the input store, `crate::treestore`).
    StoreEntries,
    /// One symlink per entry, named by a 64-hex key (the roots store). The
    /// link points into `/nix/store` and is what the daemon's
    /// `gcroots/auto/<hash>` entry names, so removing it releases the root.
    Links,
    /// One bare Git repository per entry, named by the nix32 sha256 of its
    /// URL, with an optional `-history` twin (`crate::fetch::git`). Each has
    /// a `<name>.lock` that every fetch takes and touches, so the lock's
    /// mtime is when the clone was last used.
    Clones,
    /// One file per entry, named by its 64-hex sha256 (Git LFS objects).
    Objects,
}

/// A sweep only ever removes something named like a key, so the store-level
/// files, the `ir/` store under a shared `--cache-dir`, and anything else a
/// user put in the directory all survive.
fn is_key(name: &OsStr, shape: Shape) -> bool {
    let b = name.as_bytes();
    match shape {
        Shape::StoreEntries => from_utf8(b).is_ok_and(|n| StorePath::from_base_name(n).is_ok()),
        Shape::Dirs | Shape::Links | Shape::Objects => {
            b.len() == 64 && b.iter().all(u8::is_ascii_hexdigit)
        }
        Shape::Clones => {
            let b = b.strip_suffix(b"-history").unwrap_or(b);
            b.len() == 52 && b.iter().all(|&c| is_nix32_char(c))
        }
        Shape::ShardedFiles => b.len() == 62 && b.iter().all(u8::is_ascii_hexdigit),
    }
}

fn is_shard(name: &OsStr) -> bool {
    let b = name.as_bytes();
    b.len() == 2 && b.iter().all(u8::is_ascii_hexdigit)
}

fn is_tmp(name: &OsStr) -> bool {
    let b = name.as_bytes();
    b.starts_with(b".") && b.ends_with(b".tmp")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn age(path: &Path) -> Option<Duration> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()?
        .elapsed()
        .ok()
}

/// The age of an entry of `shape`: a symlink entry is aged by the link, not by
/// what it points at, whose mtime is the store's epoch.
fn entry_age(path: &Path, shape: Shape) -> Option<Duration> {
    match shape {
        Shape::Links => fs::symlink_metadata(path)
            .and_then(|m| m.modified())
            .ok()?
            .elapsed()
            .ok(),
        Shape::Clones => age(&clone_lock(path)).or_else(|| age(path)),
        _ => age(path),
    }
}

/// Marks a link in the roots store as used now, so the TTL sweep keeps the
/// roots of the flakes somebody still formats. The stamps go on the link itself.
pub fn touch_link(path: &Path) {
    sysfs::touch(path, false);
}

fn stamp_age(bytes: &[u8]) -> Option<Duration> {
    let secs: u64 = from_utf8(bytes.trim_ascii()).ok()?.parse().ok()?;
    // a stamp in the future is no reason to wipe a store, but a sweep must
    // not wait for the clock to catch up with it
    Some(match now_secs().checked_sub(secs) {
        Some(d) => Duration::from_secs(d),
        None => Duration::MAX,
    })
}

/// Writes `chunks`, one after the other, to `dir/name` through a temporary
/// file, so a reader sees either the old contents or the new ones. Several
/// chunks make one file, which is how an entry of many parts is published in
/// one rename without first joining it into one buffer.
pub fn write_atomic(dir: &Path, name: &str, chunks: &[&[u8]]) -> io::Result<()> {
    tmp::replace(&dir.join(name), |f| {
        for c in chunks {
            f.write_all(c)?;
        }
        Ok(())
    })
}

/// Bytes of the integrity check an entry of a hashed store carries.
pub const SUM: usize = 8;

/// The largest entry a hashed store will read. An entry is the compiled form
/// of one source file, or the record of one run; the largest of either in
/// nixpkgs are tens of megabytes, so this leaves two orders of magnitude of
/// headroom and still refuses a crafted length — a sparse file of a terabyte —
/// before anything is allocated.
pub const MAX_ENTRY: u64 = 512 << 20;

pub fn checksum(key: &str, parts: &[&[u8]]) -> [u8; SUM] {
    let mut h = crc32fast::Hasher::new();
    h.update(&(key.len() as u64).to_le_bytes());
    h.update(key.as_bytes());
    let mut len = 0usize;
    for p in parts {
        h.update(p);
        len += p.len();
    }
    let mut out = [0u8; SUM];
    out[..4].copy_from_slice(&h.finalize().to_le_bytes());
    out[4..].copy_from_slice(&(len as u32).to_le_bytes());
    out
}

/// What the name of an entry holds, as [`open_entry`] found it.
pub enum Found {
    /// A regular file of a length an entry can have, open for reading.
    File(fs::File, fs::Metadata),
    /// Nothing this run can read now: no name, or a regular file that does
    /// not open. Windows refuses an open while a sibling renames a new entry
    /// over the name (`ERROR_ACCESS_DENIED`, `ERROR_SHARING_VIOLATION`), so a
    /// file that is there and does not open is a miss, never poison.
    Missing,
    /// A name that is no entry: a link, a directory, a FIFO, or a length no
    /// entry has. The caller wipes it.
    Foreign,
}

impl Found {
    pub fn file(self) -> Option<(fs::File, fs::Metadata)> {
        match self {
            Found::File(f, md) => Some((f, md)),
            _ => None,
        }
    }
}

/// Opens an entry for reading under the rules a cache directory needs: it is
/// writable by whoever runs iets, so the name must not be a symbolic link,
/// what it names must be a regular file, and its length must be one an entry
/// can have. `O_NONBLOCK` keeps the open of a FIFO from waiting for a writer.
pub fn open_entry(path: &Path) -> Found {
    let Ok(f) = sysfs::no_follow_no_block(fs::OpenOptions::new().read(true)).open(path) else {
        return match path.symlink_metadata() {
            Ok(md) if !md.is_file() => Found::Foreign,
            _ => Found::Missing,
        };
    };
    match f.metadata() {
        Ok(md) if md.is_file() && md.len() <= MAX_ENTRY => Found::File(f, md),
        Ok(_) => Found::Foreign,
        Err(_) => Found::Missing,
    }
}

/// Whose directory a store root is, before iets writes to it.
enum Owner {
    /// iets's, and usable as it stands.
    Ready,
    /// iets's, but of another format or with a broken store-level file.
    Stale(&'static str),
    /// Not iets's: it holds a name that no store of this shape can have.
    Foreign,
    /// A link, or another user's: whoever controls it could plant entries.
    Untrusted(&'static str),
}

/// A name a store of `shape` puts in its own root: a store-level file, a
/// temporary file, an entry, or an IR shard.
fn belongs(name: &OsStr, shape: Shape) -> bool {
    matches!(
        name.as_bytes(),
        b"version" | b"lock" | b"gc-stamp" | b"env-key"
    ) || is_tmp(name)
        || is_key(name, shape)
        || (matches!(shape, Shape::ShardedFiles) && is_shard(name))
        || (matches!(shape, Shape::StoreEntries) && name.as_bytes() == PACKS.as_bytes())
        || (matches!(shape, Shape::Clones)
            && name
                .as_bytes()
                .strip_suffix(b".lock")
                .is_some_and(|k| is_key(OsStr::from_bytes(k), shape)))
}

/// True when every name in `root` is one this store's own layout makes. An
/// unreadable directory or entry counts as foreign: iets removes only what
/// it can prove it put there.
fn all_ours(root: &Path, shape: Shape) -> bool {
    let Ok(rd) = fs::read_dir(root) else {
        return false;
    };
    for e in rd {
        match e {
            Ok(e) if belongs(&e.file_name(), shape) => {}
            _ => return false,
        }
    }
    true
}

fn owner(root: &Path, shape: Shape) -> Owner {
    match fs::symlink_metadata(root) {
        Err(_) => return Owner::Ready,
        Ok(m) if m.is_symlink() => return Owner::Untrusted("is a symbolic link"),
        Ok(m) if !sysfs::owned_by_me(&m) => return Owner::Untrusted("belongs to another user"),
        Ok(_) => {}
    }
    // before anything in it is read or wiped: whoever else could write it
    // may have planted what is there
    tighten(root);
    if !fs::symlink_metadata(root).is_ok_and(|m| sysfs::private_to_me(&m)) {
        return Owner::Untrusted("is open to other users");
    }
    match fs::read(root.join(VERSION)) {
        Ok(v) if v.trim_ascii() == format(shape) => {}
        Ok(_) => return Owner::Stale("format version mismatch"),
        Err(_) if empty(root) => {}
        Err(_) if all_ours(root, shape) => return Owner::Stale("no format version"),
        Err(_) => return Owner::Foreign,
    }
    if root.join(LOCK).is_dir() {
        return Owner::Stale("lock is not a file");
    }
    match fs::read(root.join(STAMP)) {
        Ok(s) if stamp_age(&s).is_none() => Owner::Stale("unreadable gc stamp"),
        _ => Owner::Ready,
    }
}

/// Empties a store iets owns, touching only the names its own layout makes:
/// the store-level files, the temporary files, the entries and the IR
/// shards. Anything else in the directory stays.
fn wipe(root: &Path, shape: Shape) {
    for name in [VERSION, LOCK, STAMP, ENV_KEY] {
        let p = root.join(name);
        let _ = fs::remove_file(&p).or_else(|_| fs::remove_dir_all(&p));
    }
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let path = e.path();
        if is_tmp(&name) || is_key(&name, shape) {
            // an entry of another format may not have this one's shape
            let _ = fs::remove_file(&path).or_else(|_| fs::remove_dir_all(&path));
        } else if matches!(shape, Shape::ShardedFiles)
            && is_shard(&name)
            && e.file_type().is_ok_and(|t| t.is_dir())
        {
            wipe_shard(&path);
        } else if matches!(shape, Shape::StoreEntries) && name.as_bytes() == PACKS.as_bytes() {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn wipe_shard(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        if is_tmp(&name) || is_key(&name, Shape::ShardedFiles) {
            let _ = fs::remove_file(e.path());
        }
    }
    let _ = fs::remove_dir(dir);
}

/// Makes `root` usable, and says whether it is. A version mismatch or an
/// unreadable store-level file wipes the store and starts it empty: a cache
/// is never repaired in place. A directory that is not iets's — it exists,
/// holds something, and carries no store of this shape — is refused instead,
/// because `--cache-dir` points this at a directory the user names.
#[must_use]
pub fn prepare(root: &Path, shape: Shape, timings: bool) -> bool {
    match owner(root, shape) {
        Owner::Ready => {}
        Owner::Stale(why) => {
            if timings {
                errln!("cache: wiping {} ({why})", root.display());
            }
            wipe(root, shape);
        }
        Owner::Foreign => {
            errln!(
                "error: {} is not an iets cache and is not empty, so iets will not \
                 use it; point --cache-dir (or IETS_CACHE_DIR) at a new or empty \
                 directory, or empty this one",
                root.display()
            );
            return false;
        }
        Owner::Untrusted(why) => {
            errln!(
                "error: the cache {} {why}, so iets will not use it",
                root.display()
            );
            return false;
        }
    }
    if create_dir(root).is_ok() && fs::read(root.join(VERSION)).is_err() {
        let _ = write_atomic(root, VERSION, &[format(shape)]);
    }
    tighten(root);
    // after tightening, so only a root iets cannot make private is refused
    if !fs::symlink_metadata(root).is_ok_and(|m| m.is_dir() && sysfs::private_to_me(&m)) {
        errln!(
            "error: the cache {} is not a directory only this user can open, so iets will not use it",
            root.display()
        );
        return false;
    }
    true
}

/// The key of this process's `getEnv` hashes, from `<root>/env-key`. An
/// observation of an environment variable needs only to know whether the
/// value changed, so an entry holds a keyed hash and never the value; the key
/// is what keeps that hash from being a guessing game for anyone who reads
/// the file. One process uses one cache, so the first store to be prepared
/// sets it; `env_key` without one hashes with zeros, and nothing that run
/// records reaches disk.
pub fn use_env_key(root: &Path) {
    ENV_SECRET.get_or_init(|| {
        let read = || {
            fs::read(root.join(ENV_KEY))
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(&b[..]).ok())
        };
        if let Some(k) = read() {
            return k;
        }
        // two cold processes must not each publish a key: the loser's entries
        // would then hash with one the winner's run cannot reproduce
        let _lock = write_lock(root);
        if let Some(k) = read() {
            return k;
        }
        let mut k = [0u8; 32];
        sys::random(&mut k);
        let _ = write_atomic(root, ENV_KEY, &[&k]);
        k
    });
}

pub fn env_key() -> &'static [u8; 32] {
    ENV_SECRET.get_or_init(|| [0u8; 32])
}

static ENV_SECRET: OnceLock<[u8; 32]> = OnceLock::new();

fn empty(root: &Path) -> bool {
    fs::read_dir(root).is_ok_and(|mut d| d.next().is_none())
}

/// The store's single-writer lock, released when this value drops.
pub struct WriteLock(fs::File);

impl Drop for WriteLock {
    fn drop(&mut self) {
        sysfs::unlock(&self.0);
    }
}

/// Takes the store's writer lock for one file write. `None` when a sibling
/// holds it longer than `LOCK_TIMEOUT`: the caller then skips the write,
/// which is a later miss and never a stall.
pub fn write_lock(root: &Path) -> Option<WriteLock> {
    try_write_lock(root).ok().flatten()
}

fn open_lock(path: &Path) -> io::Result<fs::File> {
    sysfs::create_mode(
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false),
        FILE_MODE,
    )
    .open(path)
}

/// [`write_lock`] with the two failures apart: `Err` when the lock file
/// cannot be opened at all, which is a store this run cannot write, and
/// `Ok(None)` when a sibling held the lock past `LOCK_TIMEOUT`, which is a
/// skipped write. The caller says which, and they read differently.
pub fn try_write_lock(root: &Path) -> io::Result<Option<WriteLock>> {
    let f = open_lock(&root.join(LOCK))?;
    let mut waited = Duration::ZERO;
    loop {
        if sysfs::try_lock(&f) {
            return Ok(Some(WriteLock(f)));
        }
        if waited >= LOCK_TIMEOUT {
            return Ok(None);
        }
        thread::sleep(LOCK_POLL);
        waited += LOCK_POLL;
    }
}

/// The store's writer lock, waited for: for a writer that must not drop its
/// write, and for work that holds the lock longer than `LOCK_TIMEOUT`.
pub fn store_lock(root: &Path) -> Option<WriteLock> {
    wait_lock(&root.join(LOCK), || {})
}

/// An exclusive `flock` on the file `path`, taken without a timeout: the one
/// wait in the caches, for work that costs far more than waiting (a
/// download). `on_wait` runs once when a sibling holds it. The file is
/// touched, so a sweep never takes a lock file that is in use. A sweep may
/// remove the file while a waiter queues on it, and the waiter then holds a
/// lock nobody else can see, so it locks the file at `path` again.
pub fn wait_lock(path: &Path, on_wait: impl FnOnce()) -> Option<WriteLock> {
    let mut on_wait = Some(on_wait);
    loop {
        let f = open_lock(path).ok()?;
        if !sysfs::try_lock(&f) {
            if let Some(w) = on_wait.take() {
                w();
            }
            if !sysfs::lock(&f) {
                return None;
            }
        }
        if sysfs::same_file(&f, path) {
            sysfs::touch_open(&f);
            return Some(WriteLock(f));
        }
    }
}

/// Marks `path` as used, at most once per `TOUCH_INTERVAL`: a hot cache on a
/// busy box does not churn the disk.
pub fn touch(path: &Path) {
    if age(path).is_some_and(|a| a < TOUCH_INTERVAL) {
        return;
    }
    sysfs::touch(path, true);
}

/// `touch` for a file already open, with `modified` from its own `fstat`.
pub fn touch_open(f: &fs::File, modified: Option<SystemTime>) {
    if modified
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|a| a < TOUCH_INTERVAL)
    {
        return;
    }
    sysfs::touch_open(f);
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Swept {
    pub scanned: usize,
    pub removed: usize,
    pub bytes: u64,
}

fn entry_size(path: &Path, shape: Shape) -> u64 {
    match shape {
        Shape::ShardedFiles | Shape::StoreEntries | Shape::Objects => {
            fs::metadata(path).map_or(0, |m| m.len())
        }
        Shape::Clones => tree_size(path),
        // a root holds no bytes of its own; what it frees is the daemon's to say
        Shape::Links => 0,
        Shape::Dirs => fs::read_dir(path).map_or(0, |d| {
            d.flatten()
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        }),
    }
}

fn drop_entry(path: &Path, shape: Shape, ttl: Duration) -> bool {
    match shape {
        Shape::Links => sysfs::remove_link(path).is_ok(),
        Shape::ShardedFiles | Shape::StoreEntries | Shape::Objects => fs::remove_file(path).is_ok(),
        Shape::Dirs => fs::remove_dir_all(path).is_ok(),
        Shape::Clones => drop_clone(path, ttl),
    }
}

/// The lock every user of the clone at `path` takes.
pub fn clone_lock(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".lock");
    path.with_file_name(name)
}

/// Whether a user holds the entry, so a sweep keeps it whatever its age.
fn in_use(path: &Path, shape: Shape) -> bool {
    shape == Shape::Clones && fs::File::open(clone_lock(path)).is_ok_and(|f| !sysfs::try_lock(&f))
}

/// A fetch returns the clone and reads it after the lock is gone, so the
/// age is checked again under the lock.
fn drop_clone(path: &Path, ttl: Duration) -> bool {
    let lock = clone_lock(path);
    let Ok(f) = open_lock(&lock) else {
        return false;
    };
    if !sysfs::try_lock(&f) || entry_age(path, Shape::Clones).is_some_and(|a| a < ttl) {
        return false;
    }
    if fs::remove_dir_all(path).is_err() {
        return false;
    }
    // under the lock, so a waiter sees the file gone and locks a new one
    let _ = fs::remove_file(&lock);
    true
}

fn tree_size(path: &Path) -> u64 {
    let Ok(m) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    fs::read_dir(path).map_or(0, |d| d.flatten().map(|e| tree_size(&e.path())).sum())
}

fn sweep_dir(dir: &Path, shape: Shape, ttl: Duration, dry_run: bool, out: &mut Swept) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let path = e.path();
        if is_tmp(&name) {
            if !dry_run && age(&path).is_some_and(|a| a >= TMP_TTL) {
                // A killed writer leaves a file in the hashed stores and a
                // whole staged directory in the input store.
                let _ = fs::remove_file(&path).or_else(|_| fs::remove_dir_all(&path));
            }
            continue;
        }
        if !is_key(&name, shape) {
            continue;
        }
        out.scanned += 1;
        let Some(a) = entry_age(&path, shape) else {
            continue;
        };
        if a < ttl {
            continue;
        }
        let bytes = entry_size(&path, shape);
        if dry_run && !in_use(&path, shape) || !dry_run && drop_entry(&path, shape, ttl) {
            out.removed += 1;
            out.bytes += bytes;
        }
    }
}

/// Removes every entry unread for longer than `ttl`, plus the temporary files
/// a killed writer left behind. `dry_run` counts and changes nothing.
pub fn sweep(root: &Path, shape: Shape, ttl: Duration, dry_run: bool) -> Swept {
    let mut out = Swept::default();
    match shape {
        Shape::Dirs | Shape::Links | Shape::Clones | Shape::Objects => {
            sweep_dir(root, shape, ttl, dry_run, &mut out);
        }
        Shape::StoreEntries => {
            sweep_dir(root, shape, ttl, dry_run, &mut out);
            // the entries name the objects; the packs hold them
            out.bytes += treestore::collect(root, dry_run).freed;
        }
        Shape::ShardedFiles => {
            let Ok(rd) = fs::read_dir(root) else {
                return out;
            };
            for shard in rd.flatten() {
                if !is_shard(&shard.file_name()) {
                    continue;
                }
                let dir = shard.path();
                if dir.is_dir() {
                    sweep_dir(&dir, shape, ttl, dry_run, &mut out);
                    if !dry_run {
                        let _ = fs::remove_dir(&dir);
                    }
                }
            }
        }
    }
    out
}

/// A sweep on a write path, at most once a day per store. The stamp is
/// claimed under the writer lock and the walk runs unlocked, so a sweep of a
/// large store never holds a sibling's write back.
pub fn maybe_sweep(root: &Path, shape: Shape, timings: bool) {
    let due = |root: &Path| {
        fs::read(root.join(STAMP))
            .ok()
            .and_then(|s| stamp_age(&s))
            .is_none_or(|a| a >= GC_INTERVAL)
    };
    if !due(root) {
        return;
    }
    {
        let Some(_lock) = write_lock(root) else {
            return;
        };
        if !due(root) {
            return;
        }
        if write_atomic(root, STAMP, &[now_secs().to_string().as_bytes()]).is_err() {
            return;
        }
    }
    let s = sweep(root, shape, Duration::from_secs(TTL_DAYS * DAY), false);
    if timings && s.scanned > 0 {
        errln!(
            "cache: swept {} of {} entries ({} KiB) from {}",
            s.removed,
            s.scanned,
            s.bytes / 1024,
            root.display()
        );
    }
}

/// The stores a `cache gc` run walks.
pub fn stores(cache_dir: Option<&str>) -> Vec<(PathBuf, Shape)> {
    let mut v = Vec::new();
    if let Some(s) = store::Store::locate(cache_dir) {
        v.push((s.root().to_path_buf(), Shape::Dirs));
    }
    if let Some(c) = IrCache::locate(cache_dir, false) {
        v.push((c.root().to_path_buf(), Shape::ShardedFiles));
    }
    if let Some(r) = treestore::locate(cache_dir) {
        v.push((r, Shape::StoreEntries));
    }
    if let Some(r) = roots(cache_dir) {
        v.push((r, Shape::Links));
    }
    if let Some(r) = root(cache_dir) {
        v.push((r.join("git"), Shape::Clones));
        v.push((r.join("lfs"), Shape::Objects));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use tempfile::tempdir;

    use crate::sys::fs::symlink;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use crate::fetch::spool;
    use crate::store::hash::{HashAlgo, hash_bytes};

    fn back_date(path: &Path, secs: u64) {
        sysfs::set_times(path, (now_secs() - secs) as i64, true).expect("utimensat");
    }

    fn back_date_link(path: &Path, secs: u64) {
        sysfs::set_times(path, (now_secs() - secs) as i64, false).expect("utimensat");
    }

    fn ready(d: &Path) {
        assert!(prepare(d, Shape::Dirs, false), "the store is iets's");
    }

    /// An entry of one shape, and how its users mark it as used.
    struct Fixture {
        shape: Shape,
        /// Creates entry `i` under the root and returns its path.
        make: fn(&Path, u8) -> PathBuf,
        age: fn(&Path, u64),
        use_: fn(&Path),
        /// Where a killed writer leaves its temporary files.
        litter: fn(&Path) -> PathBuf,
    }

    const PAYLOAD: &[u8] = b"payload";

    fn hex(i: u8, n: usize) -> String {
        char::from(b"0123456789abcdef"[usize::from(i)])
            .to_string()
            .repeat(n)
    }

    fn nix32(i: u8, n: usize) -> String {
        char::from(b"0123456789abcdfg"[usize::from(i)])
            .to_string()
            .repeat(n)
    }

    fn file(p: PathBuf) -> PathBuf {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, PAYLOAD).unwrap();
        p
    }

    /// An entry the input store can read, which a sweep keeps when young.
    fn store_entry() -> Vec<u8> {
        let nar = hash_bytes(HashAlgo::Sha256, b"");
        spool::encode_index(&spool::Node::Symlink(b"x".to_vec()), (&nar, 1))
    }

    fn fixtures() -> [Fixture; 6] {
        // exhaustive, so a new shape needs a fixture here
        let _ = |s: Shape| match s {
            Shape::Dirs
            | Shape::ShardedFiles
            | Shape::StoreEntries
            | Shape::Links
            | Shape::Clones
            | Shape::Objects => {}
        };
        let root = |r: &Path| r.to_path_buf();
        [
            Fixture {
                shape: Shape::Dirs,
                make: |r, i| {
                    let e = r.join(hex(i, 64));
                    file(e.join("readset"));
                    e
                },
                age: back_date,
                use_: touch,
                litter: root,
            },
            Fixture {
                shape: Shape::ShardedFiles,
                make: |r, i| file(r.join("ab").join(hex(i, 62))),
                age: back_date,
                use_: touch,
                litter: |r| r.join("ab"),
            },
            Fixture {
                shape: Shape::StoreEntries,
                make: |r, i| {
                    let e = r.join(format!("{}-source", nix32(i, 32)));
                    fs::write(&e, store_entry()).unwrap();
                    e
                },
                age: back_date,
                use_: touch,
                litter: root,
            },
            Fixture {
                shape: Shape::Links,
                make: |r, i| {
                    // the target carries the store's epoch, as a store path does
                    let target = file(r.join("target"));
                    back_date(&target, 400 * DAY);
                    let l = r.join(hex(i, 64));
                    symlink(&target, &l).unwrap();
                    l
                },
                age: back_date_link,
                use_: touch_link,
                litter: root,
            },
            Fixture {
                shape: Shape::Clones,
                make: |r, i| {
                    let c = r.join(nix32(i, 52));
                    fs::create_dir_all(c.join("objects")).unwrap();
                    file(c.join("HEAD"));
                    fs::write(clone_lock(&c), b"").unwrap();
                    c
                },
                age: |c, secs| back_date(&clone_lock(c), secs),
                use_: |c| drop(wait_lock(&clone_lock(c), || {})),
                litter: root,
            },
            Fixture {
                shape: Shape::Objects,
                make: |r, i| file(r.join(hex(i, 64))),
                age: back_date,
                use_: touch,
                litter: root,
            },
        ]
    }

    /// Every shape: an entry unused for the TTL goes with exactly its bytes,
    /// and nothing else does. A dry run only counts.
    #[test]
    fn a_sweep_removes_only_what_the_ttl_says() {
        let ttl = Duration::from_secs(TTL_DAYS * DAY);
        for f in fixtures() {
            let d = tempdir().unwrap();
            let r = d.path();
            assert!(prepare(r, f.shape, false), "{:?}", f.shape);
            let [old, young, used, held] = [1, 2, 3, 4].map(|i| (f.make)(r, i));
            for e in [&old, &used, &held] {
                (f.age)(e, 40 * DAY);
            }
            (f.age)(&young, 60);
            (f.use_)(&used);
            // a sibling fetching the clone right now
            let _held = (f.shape == Shape::Clones).then(|| {
                let l = wait_lock(&clone_lock(&held), || {}).unwrap();
                back_date(&clone_lock(&held), 40 * DAY);
                l
            });
            let foreign = r.join("notes.txt");
            fs::write(&foreign, b"PRECIOUS").unwrap();
            back_date(&foreign, 400 * DAY);
            let litter = (f.litter)(r).join(".x.tmp");
            fs::write(&litter, b"").unwrap();
            back_date(&litter, 3 * DAY);

            let held_stays = f.shape == Shape::Clones;
            let n_old = if held_stays { 1 } else { 2 };
            let bytes = match f.shape {
                Shape::Links => 0,
                Shape::StoreEntries => store_entry().len() as u64,
                _ => PAYLOAD.len() as u64,
            };
            let want = Swept {
                scanned: 4,
                removed: n_old,
                bytes: bytes * n_old as u64,
            };
            assert_eq!(sweep(r, f.shape, ttl, true), want, "{:?} dry", f.shape);
            assert!(
                old.exists() && litter.exists(),
                "{:?}: a dry run removes nothing",
                f.shape
            );

            assert_eq!(sweep(r, f.shape, ttl, false), want, "{:?}", f.shape);
            assert!(!old.exists(), "{:?}: unused for the TTL", f.shape);
            assert!(young.exists(), "{:?}: young", f.shape);
            assert!(used.exists(), "{:?}: use makes an entry young", f.shape);
            assert_eq!(held.exists(), held_stays, "{:?}: held", f.shape);
            assert!(foreign.exists(), "{:?}: not iets's", f.shape);
            assert!(!litter.exists(), "{:?}: a killed writer's file", f.shape);
            match f.shape {
                Shape::Clones => {
                    assert!(!clone_lock(&old).exists(), "the lock goes with its clone")
                }
                Shape::Links => assert!(r.join("target").exists(), "what a root named stays"),
                _ => {}
            }
            assert_eq!(
                sweep(r, f.shape, ttl, false).removed,
                0,
                "{:?}: settled",
                f.shape
            );
            assert!(prepare(r, f.shape, false), "{:?}: still iets's", f.shape);
        }
    }

    /// Every shape: a store of another format starts empty, and keeps what
    /// iets did not create.
    #[test]
    fn a_stale_store_is_wiped_to_its_own_files() {
        // another version, a garbled stamp
        let stale: [(&str, &[u8]); 2] = [(VERSION, b"99"), (STAMP, b"\x00not a time")];
        for f in fixtures() {
            for (why, bytes) in stale {
                let d = tempdir().unwrap();
                let r = d.path();
                assert!(prepare(r, f.shape, false));
                let e = (f.make)(r, 1);
                let foreign = file((f.litter)(r).join("keep.txt"));
                fs::write(r.join(why), bytes).unwrap();
                assert!(prepare(r, f.shape, false), "{:?}, {why}", f.shape);
                assert!(!e.exists(), "{:?}, {why}: the entry goes", f.shape);
                assert!(
                    foreign.exists(),
                    "{:?}, {why}: a foreign file stays",
                    f.shape
                );
                assert_eq!(fs::read(r.join(VERSION)).unwrap(), format(f.shape));
            }
        }
    }

    /// A waiter that queued on a lock file the sweep then removed must not
    /// take it alongside one that locked the new file at the same path.
    #[test]
    fn a_clone_lock_excludes_across_a_sweep() {
        let d = tempdir().unwrap();
        let r = d.path().to_path_buf();
        assert!(prepare(&r, Shape::Clones, false));
        let clone = r.join(nix32(1, 52));
        let inside = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let fetchers: Vec<_> = (0..4)
            .map(|_| {
                let (clone, inside) = (clone.clone(), inside.clone());
                thread::spawn(move || {
                    for _ in 0..100 {
                        let _l = wait_lock(&clone_lock(&clone), || {}).unwrap();
                        assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0, "two holders");
                        let _ = fs::create_dir_all(clone.join("objects"));
                        // old enough for the sweep to want it the moment it is free
                        back_date(&clone_lock(&clone), 40 * DAY);
                        thread::yield_now();
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        let sweeper = {
            let (r, stop) = (r.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    sweep(&r, Shape::Clones, Duration::from_secs(DAY), false);
                }
            })
        };
        let failed = fetchers
            .into_iter()
            .map(|t| t.join().is_err())
            .fold(false, |a, b| a | b);
        stop.store(true, Ordering::SeqCst);
        sweeper.join().unwrap();
        assert!(!failed, "a lock holder shared the clone");
    }

    #[test]
    fn a_wait_lock_waits_for_its_holder() {
        let d = tempdir().unwrap();
        ready(d.path());
        let f = d.path().join(".x.fetching.tmp");
        let held = wait_lock(&f, || panic!("nobody holds it")).expect("first lock");
        let waited = Arc::new(AtomicBool::new(false));
        let (w, f2) = (waited.clone(), f.clone());
        let t = thread::spawn(move || wait_lock(&f2, || w.store(true, Ordering::SeqCst)).is_some());
        while !waited.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }
        drop(held);
        assert!(t.join().expect("waiter"));
    }

    /// The input store of format 1 held a directory per entry; format 2 holds
    /// a file, so the version check is what removes the old ones.
    #[test]
    fn an_input_store_of_the_spool_layout_is_wiped() {
        let d = tempdir().unwrap();
        let entry = d.path().join("z4wkdlk69gck09s64a8ghpn6h7bxhkyy-source");
        fs::create_dir_all(&entry).expect("entry");
        fs::write(entry.join("spool"), b"").expect("spool");
        fs::write(d.path().join(VERSION), FORMAT).expect("version");
        assert!(prepare(d.path(), Shape::StoreEntries, false));
        assert!(!entry.exists());
        assert_eq!(fs::read(d.path().join(VERSION)).expect("version"), b"2");
    }

    #[test]
    fn a_store_without_a_version_file_is_wiped_once() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join(OLD)).expect("entry");
        ready(d.path());
        assert!(!d.path().join(OLD).exists());
        ready(d.path());
        assert_eq!(fs::read(d.path().join(VERSION)).expect("version"), FORMAT);
    }

    #[test]
    fn a_directory_that_is_not_ours_is_refused_untouched() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("unrelated.txt"), b"PRECIOUS").expect("file");
        fs::create_dir_all(d.path().join("notes")).expect("dir");
        assert!(
            !prepare(d.path(), Shape::Dirs, false),
            "a foreign directory"
        );
        assert_eq!(
            fs::read(d.path().join("unrelated.txt")).expect("file"),
            b"PRECIOUS"
        );
        assert!(d.path().join("notes").exists());
        assert!(
            !d.path().join(VERSION).exists(),
            "and nothing is written into it"
        );

        let link = d.path().join("link");
        let empty = d.path().join("empty");
        fs::create_dir(&empty).expect("dir");
        symlink(&empty, &link).expect("link");
        assert!(!prepare(&link, Shape::Dirs, false), "a link to a store");
        assert!(!empty.join(VERSION).exists());
    }

    /// A stale store open to the group is closed before it is wiped, and a
    /// shard that is a link is not followed.
    #[cfg(unix)]
    #[test]
    fn a_wipe_never_follows_a_planted_shard_link() {
        let d = tempdir().unwrap();
        let root = d.path().join("ir");
        let victim = d.path().join("victim");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&victim).expect("victim");
        let key = "0".repeat(62);
        fs::write(victim.join(&key), "keep").expect("victim file");
        fs::write(root.join(VERSION), "old").expect("version");
        symlink(&victim, root.join("ab")).expect("shard link");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o770)).expect("chmod");
        assert!(prepare(&root, Shape::ShardedFiles, false));
        assert!(victim.join(&key).exists(), "the link target is untouched");
        let mode = fs::metadata(&root).expect("root").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// A root a umask left unusable is made usable, not refused.
    #[cfg(unix)]
    #[test]
    fn a_root_without_owner_bits_is_repaired() {
        let d = tempdir().unwrap();
        let root = d.path().join("trace");
        fs::create_dir_all(&root).expect("root");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).expect("chmod");
        assert!(prepare(&root, Shape::Dirs, false));
        assert!(root.join(VERSION).exists());
    }

    #[test]
    fn an_empty_directory_is_ours_to_take() {
        let d = tempdir().unwrap();
        assert!(prepare(d.path(), Shape::Dirs, false));
        assert_eq!(fs::read(d.path().join(VERSION)).expect("version"), FORMAT);
    }

    #[test]
    fn the_writer_lock_excludes_one_holder_at_a_time() {
        let d = tempdir().unwrap();
        ready(d.path());
        let held = write_lock(d.path()).expect("first lock");
        assert!(write_lock(d.path()).is_none());
        drop(held);
        assert!(write_lock(d.path()).is_some());
    }

    #[test]
    fn a_touch_inside_the_interval_writes_nothing() {
        let d = tempdir().unwrap();
        ready(d.path());
        let f = d.path().join("entry");
        fs::write(&f, b"x").expect("entry");
        back_date(&f, 60);
        let before = fs::metadata(&f).expect("stat").modified().expect("mtime");
        touch(&f);
        assert_eq!(
            fs::metadata(&f).expect("stat").modified().expect("mtime"),
            before
        );
        back_date(&f, 3 * DAY);
        touch(&f);
        assert!(
            fs::metadata(&f).expect("stat").modified().expect("mtime") > before,
            "an entry older than the interval is touched"
        );
    }

    const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn a_sweep_runs_once_per_interval() {
        let d = tempdir().unwrap();
        ready(d.path());
        maybe_sweep(d.path(), Shape::Dirs, false);
        let stamp = fs::read(d.path().join(STAMP)).expect("stamp");
        let old = d.path().join(OLD);
        fs::create_dir_all(&old).expect("entry");
        back_date(&old, 40 * DAY);
        maybe_sweep(d.path(), Shape::Dirs, false);
        assert!(old.exists(), "the second sweep is inside the interval");
        assert_eq!(fs::read(d.path().join(STAMP)).expect("stamp"), stamp);
        fs::write(d.path().join(STAMP), (now_secs() - 2 * DAY).to_string()).expect("stamp");
        maybe_sweep(d.path(), Shape::Dirs, false);
        assert!(!old.exists());
    }

    /// A stamp ahead of the clock must not hold every sweep off until the
    /// clock catches up, and is no reason to wipe the store.
    #[test]
    fn a_stamp_in_the_future_is_due() {
        let d = tempdir().unwrap();
        ready(d.path());
        let old = d.path().join(OLD);
        fs::create_dir_all(&old).expect("entry");
        back_date(&old, 40 * DAY);
        fs::write(d.path().join(STAMP), (now_secs() + 900 * DAY).to_string()).expect("stamp");
        maybe_sweep(d.path(), Shape::Dirs, false);
        assert!(!old.exists(), "the sweep ran");
        assert!(
            d.path().join(VERSION).exists(),
            "and the store was not wiped"
        );
    }

    /// A store whose lock cannot be opened at all is not a store whose lock a
    /// sibling holds, and the two read differently to whoever sees the line.
    #[test]
    fn a_lock_that_cannot_be_opened_is_not_a_busy_lock() {
        let d = tempdir().unwrap();
        ready(d.path());
        let held = try_write_lock(d.path()).expect("open").expect("first lock");
        assert!(
            try_write_lock(d.path()).expect("open").is_none(),
            "a sibling holds it"
        );
        drop(held);
        let lock = d.path().join(LOCK);
        let mut p = fs::metadata(&lock).unwrap().permissions();
        p.set_readonly(true);
        fs::set_permissions(&lock, p).expect("read-only");
        assert!(
            try_write_lock(d.path()).is_err(),
            "a lock this run cannot write"
        );
    }
}
