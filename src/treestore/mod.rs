//! The local content-addressed tree store: where a fetched tree lives instead
//! of `/nix/store`.
//!
//! Two different things are called a store in iets. `crate::remote` is a Nix
//! store reached over a protocol, with its trust model. This is the other one:
//! content we own on local disk, under `crate::cachefs`'s rules — a version
//! file, one writer at a time, an atomic rename into place, a TTL sweep
//! (docs/explanation/caching.md) — like the IR cache and the tracing store. `cachefs` does
//! not know about this module; this module is one of its users.
//!
//! Put a tree in, get a store path. Read bytes back for evaluation. Stream a
//! NAR out for an upload. Nothing else.
//!
//! **The store is fixed-output only, and that is what makes it safe in user
//! space.** [`put`] takes the hash the caller already knows — a lock entry's
//! `narHash` — and has no form without it, so the content verifies itself:
//! no signature, no trusted builder, no daemon. An entry's name *is* its
//! content address, which is why a reader never re-hashes and why two
//! processes can share one entry.
//!
//! The only user is `crate::source`, which fetches a
//! `github`/`gitlab`/`sourcehut`/`tarball` input as one decompressed tar and
//! indexes it (`crate::fetch::spool`). The store keeps the index as
//! `$XDG_CACHE_HOME/iets/inputs/<store path base name>`, with every file named
//! by the blake3 of its bytes, and the bytes themselves in `inputs/packs/`
//! (`pack`), where a file that two inputs share is stored once. Evaluation
//! reads the input straight out of the packs: `import`, `readFile`,
//! `readDir`, `pathExists`, `readFileType`, `hashFile`, `builtins.path`,
//! `filterSource` and string coercion all answer from them. No daemon is
//! needed to evaluate a flake with fetched inputs, and none is written during
//! evaluation; a build uploads the tree from the same packs when its target
//! store lacks it (`remote::plan`).

pub mod pack;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::cachefs;
use crate::error::Result;
use crate::eval::Evaluator;
use crate::fetch::spool::{self, Blob, Node, Tree};
use crate::store::hash::Hash;
use crate::store::nar::{FileType, PathFilter};
use crate::store::path::{IngestionMethod, StorePath, make_fixed_output_path};
use std::sync::atomic::Ordering::Relaxed;

/// The store path a tree with this NAR hash has. The one naming site for a
/// tree the store holds: `fetchTree`, `fetchTarball` and a copied source all
/// ask here, so a stored entry's name and an evaluated store path cannot
/// disagree.
pub fn store_path(name: &str, nar_hash: &Hash) -> Result<StorePath> {
    make_fixed_output_path(
        IngestionMethod::Recursive,
        nar_hash,
        name,
        &std::collections::BTreeSet::new(),
    )
    .map_err(Into::into)
}

/// One tree the store holds: its content, and the NAR hash and size that were
/// verified against the caller's expectation before it was named.
#[derive(Debug)]
pub struct StoredTree {
    tree: Tree,
    nar_hash: Hash,
    nar_size: u64,
    /// Whether this run's read set already names the entry. One observation
    /// covers every read of it, so the flag turns a nixpkgs-sized input's
    /// thousands of reads into a single record and one atomic per read.
    recorded: std::sync::atomic::AtomicBool,
}

impl StoredTree {
    fn new(tree: Tree, nar_hash: Hash, nar_size: u64) -> StoredTree {
        StoredTree {
            tree,
            nar_hash,
            nar_size,
            recorded: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn nar_hash(&self) -> Hash {
        self.nar_hash
    }

    pub fn nar_size(&self) -> u64 {
        self.nar_size
    }

    /// The type of the member at `rel` (`b""` is the tree itself), with
    /// symlinks followed as the kernel follows them under `/nix/store`;
    /// `follow` is false for an `lstat`.
    pub fn stat(&self, rel: &[u8], follow: bool) -> Option<FileType> {
        self.tree.resolve(rel, follow).map(Node::file_type)
    }

    /// A directory's entries, in NAR order. `ENOTDIR` for anything else.
    pub fn list(&self, rel: &[u8]) -> io::Result<Vec<(Vec<u8>, FileType)>> {
        match self.tree.resolve(rel, true) {
            Some(Node::Dir(d)) => Ok(d
                .iter()
                .map(|(n, child)| (n.clone(), child.file_type()))
                .collect()),
            Some(_) => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
            None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }
    }

    /// A regular member's whole contents, in one `pread` into one buffer: no
    /// `open`, no copy through a staging buffer, so a read of a member costs
    /// less than a read of the same file under `/nix/store`. `EISDIR` for a
    /// member that is not a regular file.
    pub fn read(&self, rel: &[u8]) -> io::Result<Vec<u8>> {
        match self.tree.resolve(rel, true) {
            Some(Node::File { at, .. }) => self.tree.read(at),
            Some(_) => Err(io::Error::from_raw_os_error(libc::EISDIR)),
            None => Err(io::Error::from_raw_os_error(libc::ENOENT)),
        }
    }

    /// The NAR of the member at `rel`, with `filter` asked about every entry
    /// below it under the path Nix would show (`shown`), as
    /// `nar::dump_path` asks. This is what an upload streams.
    pub fn dump(
        &self,
        rel: &[u8],
        shown: &Path,
        sink: &mut dyn io::Write,
        filter: &mut PathFilter<'_>,
    ) -> io::Result<()> {
        let node = (self.tree.resolve(rel, true))
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        self.tree.dump_filtered(node, shown, sink, filter)
    }
}

/// `--cache-dir DIR`, else `IETS_CACHE_DIR`, else `$XDG_CACHE_HOME/iets`,
/// else `~/.cache/iets`; the entries live in its `inputs/` subdirectory.
/// `None` when no home is known.
pub fn locate(override_dir: Option<&str>) -> Option<PathBuf> {
    Some(crate::cachefs::root(override_dir)?.join("inputs"))
}

/// The store of one run: where it lives, the trees it opened, and what
/// opening them cost. The session owns it (`crate::session`).
#[derive(Default)]
pub struct Store {
    root: OnceLock<Option<PathBuf>>,
    packs: OnceLock<Option<Arc<pack::Packs>>>,
    /// Trees opened, by store path base name. An entry is
    /// content-addressed, so one open serves every read of it; a stored
    /// `None` records a name the store does not hold, so a miss is asked
    /// once.
    opened: Mutex<HashMap<String, Option<Arc<StoredTree>>>>,
    entries: std::sync::atomic::AtomicUsize,
    nanos: std::sync::atomic::AtomicU64,
}

fn session() -> Arc<crate::session::Session> {
    crate::session::Session::current()
}

/// The store this run uses, prepared on first use.
pub fn root() -> Option<PathBuf> {
    session()
        .trees
        .root
        .get_or_init(|| {
            let root = locate(None)?;
            cachefs::prepare(&root, cachefs::Shape::StoreEntries, false).then_some(root)
        })
        .clone()
}

fn dir_of(name: &str) -> Option<PathBuf> {
    Some(root()?.join(name))
}

fn packs() -> Option<Arc<pack::Packs>> {
    session()
        .trees
        .packs
        .get_or_init(|| Some(Arc::new(pack::Packs::open(&root()?.join(pack::PACKS)))))
        .clone()
}

/// What opening the trees of this run cost, for `--timings`: reading each
/// index and decoding it is the one fixed cost the tree store adds to an
/// evaluation that `/nix/store` does not pay.
pub fn report_timings() {
    let s = session();
    let n = s.trees.entries.load(Relaxed);
    if n > 0 {
        crate::errln!(
            "inputs: {n} entries opened, {:.3}s",
            s.trees.nanos.load(Relaxed) as f64 / 1e9
        );
    }
}

fn read_entry(path: &Path) -> Option<(Node, Hash, u64)> {
    let (f, _) = cachefs::open_entry(path)?;
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut &f, &mut bytes).ok()?;
    spool::decode_index(&bytes)
}

/// The entry `name` of the store at `root`. One whose index does not decode is
/// wiped: the next evaluation fetches it again.
fn open(root: &Path, packs: Arc<pack::Packs>, name: &str) -> Option<StoredTree> {
    let path = root.join(name);
    match read_entry(&path) {
        Some((tree, nar_hash, nar_size)) => {
            cachefs::touch(&path);
            Some(StoredTree::new(
                Tree::stored(tree, packs),
                nar_hash,
                nar_size,
            ))
        }
        None => {
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
            None
        }
    }
}

/// The tree named `name`, opened once per run.
pub fn get(name: &str) -> Option<Arc<StoredTree>> {
    let s = session();
    if let Some(hit) = s.trees.opened.lock().unwrap().get(name) {
        return hit.clone();
    }
    let t0 = std::time::Instant::now();
    let found = root()
        .zip(packs())
        .and_then(|(root, packs)| open(&root, packs, name))
        .map(Arc::new);
    if found.is_some() {
        s.trees.entries.fetch_add(1, Relaxed);
        s.trees
            .nanos
            .fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
    }
    s.trees
        .opened
        .lock()
        .unwrap()
        .entry(name.to_string())
        .or_insert(found)
        .clone()
}

/// Whether the store holds `name`, for the tracing cache: an entry's presence
/// is the whole observation, because its name is its content address.
pub fn has(name: &[u8], version: u8) -> bool {
    if version != spool::INDEX_VERSION {
        return false;
    }
    let Some(path) = std::str::from_utf8(name).ok().and_then(dir_of) else {
        return false;
    };
    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    // a tracing-cache hit reads nothing out of the entry, so this is the only
    // thing that keeps the TTL sweep from removing an input that is in use.
    // `touch` compares the mtime first, so a hit inside the interval writes
    // nothing (docs/explanation/caching.md)
    cachefs::touch(&path);
    true
}

/// A place to download into, under the store root. The staged directory is
/// named like a `cachefs` temporary, so a killed writer's leftovers are swept.
pub struct Staged {
    dir: PathBuf,
}

impl Staged {
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.dir.exists() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

pub fn stage(name: &str) -> Option<Staged> {
    let dir = root()?.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        cachefs::tag()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    cachefs::create_dir(&dir).ok()?;
    Some(Staged { dir })
}

/// One download per tree: every process that is about to fetch `name` takes
/// this lock first, and one that had to wait finds the tree its sibling
/// stored (`reopen`). The lock file is named like a temporary, so the sweep
/// removes it once it is a day old and unused.
pub fn fetch_lock(name: &str, on_wait: impl FnOnce()) -> Option<cachefs::WriteLock> {
    cachefs::wait_lock(&root()?.join(format!(".{name}.fetching.tmp")), on_wait)
}

/// [`get`], past a miss this process recorded earlier.
pub fn reopen(name: &str) -> Option<Arc<StoredTree>> {
    session().trees.opened.lock().unwrap().remove(name);
    get(name)
}

/// Why a tree was not stored. A mismatch carries the hash the content really
/// has, so the caller can say what it expected instead.
#[derive(Debug)]
pub enum Rejected {
    Mismatch(Hash),
    Io(io::Error),
    /// No cache directory, or the rename lost to something that is not a
    /// sibling's entry: the tree stays unstored, which costs a fetch on the
    /// next run and nothing else.
    Unstorable,
}

/// Puts a tree in, under the store path `name` and `expect` name together.
///
/// `expect` is the hash the caller already knows — a lock entry's `narHash`,
/// a derivation's `outputHash` — and there is no form of this call without
/// it. The tree is hashed here and refused unless it is the one that was
/// asked for, so an entry's name is always its content address and nothing
/// that lacks a prior hash can be admitted. That is the whole of the store's
/// trust model: no signature, no trusted builder, no daemon.
///
/// The bytes go into a new pack, except those some pack already holds; the
/// pack and then the index are renamed into place, so a reader sees either no
/// entry or a whole one, and a sibling that got there first wins.
pub fn put(
    name: &str,
    expect: &Hash,
    staged: Staged,
    tree: Tree,
) -> std::result::Result<Arc<StoredTree>, Rejected> {
    let root = root().ok_or(Rejected::Unstorable)?;
    let packs = packs().ok_or(Rejected::Unstorable)?;
    let (name, stored) = store(&root, packs, name, expect, tree)?;
    drop(staged);
    cachefs::maybe_sweep(&root, cachefs::Shape::StoreEntries, false);
    let stored = Arc::new(stored);
    session()
        .trees
        .opened
        .lock()
        .unwrap()
        .insert(name, Some(stored.clone()));
    Ok(stored)
}

/// Reads, hashes and, unless some pack already holds it, compresses every
/// distinct file of `tree` on several threads, a batch at a time so that one
/// batch bounds the memory, and appends the new ones to `writer` in the tree's
/// order. Gives each file's id, and the files left to the packs that hold them.
fn objects(
    tree: &Tree,
    packs: &pack::Packs,
    writer: &mut pack::PackWriter,
) -> io::Result<(HashMap<Blob, pack::Id>, HashMap<pack::Id, Blob>)> {
    const BATCH: u64 = 32 << 20;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
    let files = tree.files();
    let mut ids = HashMap::with_capacity(files.len());
    let mut elsewhere = HashMap::new();
    let mut start = 0;
    while start < files.len() {
        let (mut end, mut size) = (start, 0);
        while end < files.len() && (end == start || size < BATCH) {
            size += files[end].size();
            end += 1;
        }
        let batch = &files[start..end];
        let parts: Vec<io::Result<Vec<_>>> = std::thread::scope(|s| {
            let workers: Vec<_> = batch
                .chunks(batch.len().div_ceil(threads))
                .map(|part| {
                    s.spawn(move || {
                        part.iter()
                            .map(|at| {
                                let bytes = tree.read(at)?;
                                let id = pack::id_of(&bytes);
                                let new = !packs.contains(&id);
                                Ok((*at, id, new.then(|| pack::prepare(bytes))))
                            })
                            .collect()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().expect("an object worker panicked"))
                .collect()
        });
        for part in parts {
            for (at, id, prepared) in part? {
                ids.insert(at, id);
                match prepared {
                    Some(p) => writer.add_prepared(id, p)?,
                    None => {
                        elsewhere.entry(id).or_insert(at);
                    }
                }
            }
        }
        start = end;
    }
    Ok((ids, elsewhere))
}

/// [`put`] into the store at `root`; the name of the entry, and the tree read
/// back out of the packs.
fn store(
    root: &Path,
    packs: Arc<pack::Packs>,
    name: &str,
    expect: &Hash,
    tree: Tree,
) -> std::result::Result<(String, StoredTree), Rejected> {
    let mut writer = pack::PackWriter::new(packs.dir()).map_err(Rejected::Io)?;
    // the objects go into a pack that is still a temporary, so they can be
    // made while the NAR is hashed: a mismatch drops the pack unnamed
    let (nar, objects) = std::thread::scope(|s| {
        let nar = s.spawn(|| -> io::Result<(u64, Hash)> {
            let mut h = crate::store::hash::Hasher::new(expect.algo);
            tree.dump(&mut h)?;
            Ok((h.bytes_written(), h.finish()))
        });
        let objects = objects(&tree, &packs, &mut writer);
        (nar.join().expect("the NAR hasher panicked"), objects)
    });
    let (nar_size, nar_hash) = nar.map_err(Rejected::Io)?;
    if nar_hash != *expect {
        return Err(Rejected::Mismatch(nar_hash));
    }
    let (ids, elsewhere) = objects.map_err(Rejected::Io)?;
    let sp = store_path(name, &nar_hash).map_err(|_| Rejected::Unstorable)?;
    let name = sp.base_name();
    let stored_root = tree.with_ids(&ids);
    let index = spool::encode_index(&stored_root, (&nar_hash, nar_size));
    {
        // `collect` holds the lock for as long as a compaction takes, so this
        // waits rather than drop a whole download; under it, a pack this tree
        // relied on may have gone, and its objects come from the spool instead
        let _lock = cachefs::store_lock(root).ok_or(Rejected::Unstorable)?;
        let now = packs.refresh();
        for (id, at) in &elsewhere {
            if now.find(id).is_none() {
                writer
                    .add(*id, &tree.read(at).map_err(Rejected::Io)?)
                    .map_err(Rejected::Io)?;
            }
        }
        writer.finish().map_err(Rejected::Io)?;
        let path = root.join(name);
        if !path.exists() {
            cachefs::write_atomic(root, name, cachefs::tag(), &[&index]).map_err(Rejected::Io)?;
        }
    }
    packs.refresh();
    let tree = Tree::stored(stored_root, packs);
    Ok((name.to_string(), StoredTree::new(tree, nar_hash, nar_size)))
}

/// Drops the objects no entry names any more: a pack none of them uses goes,
/// and one mostly unused is rewritten (`pack::collect`). An entry whose index
/// does not decode goes too. Runs under the store's lock, so no `put` publishes
/// an entry between the marking and the sweeping.
pub fn collect(root: &Path, dry_run: bool) -> pack::Collected {
    let Some(_lock) = cachefs::store_lock(root) else {
        return pack::Collected::default();
    };
    let mut live = HashSet::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return pack::Collected::default();
    };
    for e in rd.flatten() {
        let path = e.path();
        let is_entry = e
            .file_name()
            .to_str()
            .is_some_and(|n| StorePath::from_base_name(n).is_ok());
        if !is_entry || !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        match read_entry(&path) {
            Some((tree, _, _)) => spool::object_ids(&tree, &mut live),
            None if !dry_run => {
                let _ = std::fs::remove_file(&path);
            }
            None => {}
        }
    }
    pack::collect(&root.join(pack::PACKS), &live, dry_run).unwrap_or_default()
}

/// The one observation a read of the tree store leaves in the read set. The
/// entry's name is the NAR hash of its tree, so its presence, plus the index
/// version that says the entry can still be read, is the whole check: there is
/// nothing per file to stat and nothing to rehash (docs/explanation/caching.md).
pub fn record(ev: &Evaluator, tree: &StoredTree, name: &str) {
    if ev.trace().is_on()
        && !tree
            .recorded
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        ev.trace().record(crate::trace::Obs::Input {
            entry: name.as_bytes().to_vec(),
            version: spool::INDEX_VERSION,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_staged_directory_goes_when_it_is_dropped() {
        let d = std::env::temp_dir().join(format!("iets-inputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        let staged = Staged { dir: d.join("x") };
        std::fs::create_dir_all(staged.dir()).expect("staged");
        let at = staged.dir().to_path_buf();
        drop(staged);
        assert!(!at.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The name an entry gets is the store path the lock's `narHash` implies.
    #[test]
    fn a_tree_is_named_by_its_nar_hash() {
        let h =
            Hash::parse_any("sha256-9i/VTdusq/+NM/tz+J1Re+ojkMB8MBf0QshnYfzHz30=", None).unwrap();
        assert_eq!(
            store_path("source", &h).unwrap().to_full_string(),
            "/nix/store/z4wkdlk69gck09s64a8ghpn6h7bxhkyy-source"
        );
    }

    fn store_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("iets-inputs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A spooled tarball holding `files` under one top-level directory.
    fn tree_of(files: &[(&str, &[u8])]) -> (Tree, Hash) {
        let mut b = tar::Builder::new(Vec::new());
        for (name, bytes) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, format!("top/{name}"), *bytes)
                .unwrap();
        }
        let mut spool = spool::scratch("test").unwrap();
        io::Write::write_all(&mut spool, &b.into_inner().unwrap()).unwrap();
        let tree = Tree::index(spool, u64::MAX).unwrap();
        let mut h = crate::store::hash::Hasher::new(crate::store::hash::HashAlgo::Sha256);
        tree.dump(&mut h).unwrap();
        (tree, h.finish())
    }

    fn put_in(root: &Path, files: &[(&str, &[u8])]) -> String {
        let (tree, hash) = tree_of(files);
        let packs = Arc::new(pack::Packs::open(&root.join(pack::PACKS)));
        store(root, packs, "source", &hash, tree).unwrap().0
    }

    fn read(root: &Path, name: &str, file: &str) -> io::Result<Vec<u8>> {
        let packs = Arc::new(pack::Packs::open(&root.join(pack::PACKS)));
        open(root, packs, name)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?
            .read(file.as_bytes())
    }

    fn objects(root: &Path) -> usize {
        pack::PackSet::load(&root.join(pack::PACKS))
            .packs()
            .iter()
            .map(|p| p.ids().count())
            .sum()
    }

    #[test]
    fn a_stored_input_reads_back_and_a_wrong_hash_is_refused() {
        let root = store_dir("put");
        let name = put_in(&root, &[("a", b"alpha"), ("b", b"beta")]);
        assert_eq!(read(&root, &name, "a").unwrap(), b"alpha");
        assert_eq!(read(&root, &name, "b").unwrap(), b"beta");

        let (tree, _) = tree_of(&[("a", b"alpha")]);
        let (_, other) = tree_of(&[("a", b"other")]);
        let packs = Arc::new(pack::Packs::open(&root.join(pack::PACKS)));
        assert!(matches!(
            store(&root, packs, "source", &other, tree),
            Err(Rejected::Mismatch(_))
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn two_inputs_share_their_common_files() {
        let root = store_dir("dedup");
        let shared = vec![b's'; 5000];
        let a = put_in(&root, &[("shared", &shared), ("only-a", b"a")]);
        let b = put_in(&root, &[("shared", &shared), ("only-b", b"b")]);
        assert_ne!(a, b);
        assert_eq!(objects(&root), 3);
        assert_eq!(read(&root, &b, "shared").unwrap(), shared);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An input whose entry is gone loses the objects only it named; another
    /// input keeps the ones it shares, and a reader that opened the gone input
    /// before the collection still reads it.
    #[test]
    fn collect_keeps_what_an_entry_names() {
        let root = store_dir("collect");
        let big = vec![b'x'; 50_000];
        let a = put_in(&root, &[("shared", b"shared bytes"), ("big", &big)]);
        let b = put_in(&root, &[("shared", b"shared bytes"), ("small", b"y")]);
        let packs = Arc::new(pack::Packs::open(&root.join(pack::PACKS)));
        let early = open(&root, packs, &a).unwrap();

        std::fs::remove_file(root.join(&a)).unwrap();
        let c = collect(&root, false);
        assert!(c.rewritten + c.removed >= 1, "{c:?}");
        assert_eq!(objects(&root), 2, "the shared file and the small one");
        assert_eq!(read(&root, &b, "shared").unwrap(), b"shared bytes");
        assert_eq!(
            early.read(b"big").unwrap(),
            big,
            "an open pack stays readable"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A writer that relied on a pack a collection then removed stores those
    /// objects itself, from the spool it still holds.
    #[test]
    fn a_put_after_a_collection_brings_its_own_objects() {
        let root = store_dir("race");
        let shared = vec![b'r'; 5000];
        let a = put_in(&root, &[("shared", &shared)]);
        let stale = Arc::new(pack::Packs::open(&root.join(pack::PACKS)));
        std::fs::remove_file(root.join(&a)).unwrap();
        collect(&root, false);
        assert_eq!(objects(&root), 0);

        let (tree, hash) = tree_of(&[("shared", &shared), ("new", b"n")]);
        let (b, _) = store(&root, stale, "source", &hash, tree).unwrap();
        assert_eq!(read(&root, &b, "shared").unwrap(), shared);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
