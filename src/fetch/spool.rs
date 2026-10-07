//! The NAR of a downloaded archive, without unpacking it.
//!
//! A tar is in archive order; a NAR needs every directory's entries sorted by
//! name and nested inside it. `fetch::unpack` gets there by writing a tree and
//! walking it, which costs a second copy of every byte on disk. This module
//! spools the decompressed archive into one file as it arrives, reads it once to
//! record where each member's bytes are, and then writes the NAR out of that
//! index, driving `nar::TreeWriter` from it and reading the bytes back by
//! offset. Peak memory is the index plus one copy buffer; nothing holds a
//! member whole. The framing is not this module's: one writer serves the
//! archive, a directory on disk and a string.
//!
//! The rules are `fetch::unpack`'s, which are tarfile.cc's and
//! git-utils.cc:GitFileSystemObjectSinkImpl's. `spool_matches_the_tree` in the
//! tests holds the two *walks* — the index and the unpacked tree — to the same
//! NAR bytes.
//!
//! The same index also answers evaluation. `crate::treestore` keeps it, with
//! every file named by the id of its bytes in the input store's packs instead
//! of by an offset in the spool, and every read of a fetched flake input,
//! `import` included, comes back out of those packs instead of out of
//! `/nix/store` (docs/reference/flakes.md).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{env, result, str};

use crate::error::{Error, Result};
use crate::store::hash::{Hash, HashAlgo};
use crate::store::nar::{FileType, NAR_MAX_DEPTH, PathFilter, TreeWriter};
use crate::treestore::pack::{Id, Packs};

use super::unpack;

/// One directory's entries, in the byte order a NAR wants them. A sorted vector,
/// not a map: a full nixpkgs holds 38 000 directories of two or three entries
/// each, and a `BTreeMap` pays for eleven of them per directory it has.
pub type Dir = Vec<(Vec<u8>, Node)>;

/// A member path split into its components.
type Parts = Vec<Vec<u8>>;

/// Bytes moved per `pread` when a member is copied out of the spool.
const CHUNK: usize = 128 * 1024;

#[derive(Debug)]
pub enum Node {
    Dir(Dir),
    File { at: Blob, exec: bool },
    Symlink(Vec<u8>),
}

/// Where a file's bytes are: in the spooled archive, in the side file that
/// holds the expanded GNU sparse members, or, once the tree is stored, in the
/// input store's packs under the id of the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Blob {
    Spool {
        offset: u64,
        size: u64,
        sparse: bool,
    },
    Object {
        id: Id,
        size: u64,
    },
}

impl Blob {
    pub fn size(&self) -> u64 {
        match self {
            Blob::Spool { size, .. } | Blob::Object { size, .. } => *size,
        }
    }
}

impl Node {
    pub fn file_type(&self) -> FileType {
        match self {
            Node::Dir(_) => FileType::Directory,
            Node::File { .. } => FileType::Regular,
            Node::Symlink(_) => FileType::Symlink,
        }
    }

    /// What a hard link to this member becomes: the target's blob and mode
    /// (git-utils.cc:1348-1364). A directory has none.
    fn copy(&self) -> Option<Node> {
        match self {
            Node::Dir(_) => None,
            Node::File { at, exec } => Some(Node::File {
                at: *at,
                exec: *exec,
            }),
            Node::Symlink(t) => Some(Node::Symlink(t.clone())),
        }
    }
}

/// An index of members, and where their bytes are.
#[derive(Debug)]
pub struct Tree {
    src: Source,
    root: Node,
}

#[derive(Debug)]
enum Source {
    Spool { spool: File, sparse: Option<File> },
    Packs(Arc<Packs>),
}

/// Version byte of the serialized index. A file that starts with anything else
/// is not read; the entry holding it is wiped (`crate::treestore`).
pub const INDEX_VERSION: u8 = 3;

/// Bytes of the checksum the index ends with.
const SUM: usize = 8;

/// The index's own checksum. It catches a changed byte before anything reads
/// what the index says, so no reader ever has to trust the file's shape.
fn checksum(body: &[u8]) -> [u8; SUM] {
    let h = blake3::hash(body);
    h.as_bytes()[..SUM].try_into().expect("eight bytes")
}

fn bad(why: String) -> Error {
    Error::Store(why)
}

fn show(parts: &[Vec<u8>]) -> String {
    parts
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// A file in TMPDIR, unlinked as soon as it is open: the bytes live exactly as
/// long as the handle, whatever happens to the process. The name is random and
/// the open is exclusive, so a name another user can guess cannot make this
/// truncate and write a file of theirs.
pub fn scratch(tag: &str) -> Result<File> {
    let dir = env::temp_dir();
    let mut last = None;
    for _ in 0..8 {
        let path = dir.join(format!("iets-spool-{tag}-{:016x}", random_u64()));
        match File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => {
                fs::remove_file(&path)
                    .map_err(|e| Error::io(format!("cannot unlink {}", path.display()), e))?;
                return Ok(file);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some((path, e)),
            Err(e) => return Err(Error::io(format!("cannot create {}", path.display()), e)),
        }
    }
    let (path, e) = last.expect("a failed attempt");
    Err(Error::io(format!("cannot create {}", path.display()), e))
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    // SAFETY: getrandom(2) writes at most `len` bytes into the buffer.
    unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
    u64::from_ne_bytes(b)
}

fn parts_of(rel: &Path) -> Parts {
    rel.components()
        .map(|c| c.as_os_str().as_bytes().to_vec())
        .collect()
}

fn find(d: &Dir, name: &[u8]) -> result::Result<usize, usize> {
    d.binary_search_by(|(k, _)| k.as_slice().cmp(name))
}

/// The last copy of a member wins (git-utils.cc:1213), except that a directory
/// listed again keeps what is already under it. `replace` is false for a hard
/// link, which carries no `id` and so never takes a name a member claimed.
fn put(d: &mut Dir, name: &[u8], node: Node, replace: bool) {
    match find(d, name) {
        Ok(i) => {
            if replace && !(matches!(node, Node::Dir(_)) && matches!(d[i].1, Node::Dir(_))) {
                d[i].1 = node;
            }
        }
        Err(i) => d.insert(i, (name.to_vec(), node)),
    }
}

/// Puts `node` at `parts`, with the missing parents created. A parent that is
/// not a directory is replaced, a symlink included, as `make_parents` replaces
/// it (git-utils.cc:addNode emplaces a directory at every parent name).
fn insert(at: &mut Dir, parts: &[Vec<u8>], node: Node, replace: bool) {
    let (name, rest) = parts.split_first().expect("a non-empty member path");
    if rest.is_empty() {
        put(at, name, node, replace);
        return;
    }
    let i = match find(at, name) {
        Ok(i) => i,
        Err(i) => {
            at.insert(i, (name.clone(), Node::Dir(Dir::new())));
            i
        }
    };
    if !matches!(at[i].1, Node::Dir(_)) {
        at[i].1 = Node::Dir(Dir::new());
    }
    let Node::Dir(d) = &mut at[i].1 else {
        unreachable!("a directory")
    };
    insert(d, rest, node, replace)
}

fn lookup<'a>(root: &'a Dir, parts: &[Vec<u8>]) -> Option<&'a Node> {
    let (last, parents) = parts.split_last()?;
    let mut at = root;
    for p in parents {
        match find(at, p).ok().map(|i| &at[i].1) {
            Some(Node::Dir(d)) => at = d,
            _ => return None,
        }
    }
    find(at, last).ok().map(|i| &at[i].1)
}

impl Tree {
    /// Indexes the decompressed tar in `spool`, which is read from the start and
    /// kept for `dump`. `max_size` is `fetch-max-size`: the spool and everything
    /// a sparse member expands to must fit in it together.
    pub fn index(spool: File, max_size: u64) -> Result<Tree> {
        let mut root = Dir::new();
        let mut links: BTreeMap<Parts, Parts> = Default::default();
        let mut sparse: Option<File> = None;
        let mut sparse_at = 0u64;
        let mut room = max_size.saturating_sub(
            spool
                .metadata()
                .map_err(|e| Error::io("cannot read the spooled tarball", e))?
                .len(),
        );
        {
            let mut src = &spool;
            src.rewind()
                .map_err(|e| Error::io("cannot read the spooled tarball", e))?;
            let mut archive = tar::Archive::new(src);
            let entries = archive
                .entries_with_seek()
                .map_err(|e| Error::io("cannot read the tarball", e))?;
            for entry in entries {
                let mut entry = entry.map_err(|e| Error::io("cannot read the tarball", e))?;
                let kind = entry.header().entry_type();
                // libarchive reads the pax global header as archive metadata,
                // not as a member; GitHub's tarballs start with one.
                if kind.is_pax_global_extensions() {
                    continue;
                }
                let pax = pax_sparse(&mut entry)?;
                let name = member_name(&pax, &entry)?;
                let parts = parts_of(&unpack::member_path(&name));
                if parts.is_empty() {
                    continue;
                }
                if kind.is_character_special() || kind.is_block_special() || kind.is_fifo() {
                    return Err(bad(format!(
                        "file '{}' in tarball has unsupported file type {kind:?}",
                        name.display()
                    )));
                }
                if kind.is_dir() {
                    insert(&mut root, &parts, Node::Dir(Dir::new()), true);
                } else if kind.is_symlink() {
                    let target = link_target(&mut entry, &parts, "symlink")?;
                    insert(&mut root, &parts, Node::Symlink(target), true);
                } else if kind.is_hard_link() {
                    let target = link_target(&mut entry, &parts, "link")?;
                    let target = unpack::member_path(Path::new(OsStr::from_bytes(&target)));
                    links.insert(parts, parts_of(&target));
                } else {
                    // libarchive gives every other type flag AE_IFREG (tarfile.cc:224)
                    let exec = entry.header().mode().unwrap_or(0o644) & 0o100 != 0;
                    // A sparse member's holes are not in the archive, so it is
                    // the one member whose bytes must be written out to be
                    // indexed, and the one whose size the spool does not bound.
                    let expanded = match &pax {
                        Some(s) => Some(s.size),
                        None if kind.is_gnu_sparse() => Some(entry.size()),
                        None => None,
                    };
                    let at = match expanded {
                        None => Blob::Spool {
                            offset: entry.raw_file_position(),
                            size: entry.size(),
                            sparse: false,
                        },
                        Some(size) => {
                            room = room.checked_sub(size).ok_or_else(|| {
                                bad(format!(
                                    "the tarball expands to over the fetch-max-size of {max_size} bytes"
                                ))
                            })?;
                            let side = match &mut sparse {
                                Some(f) => f,
                                None => sparse.insert(scratch("sparse")?),
                            };
                            expand(&mut entry, pax.as_ref(), side, sparse_at, size).map_err(
                                |e| {
                                    Error::io(
                                        format!("cannot read '{}' from the tarball", show(&parts)),
                                        e,
                                    )
                                },
                            )?;
                            let at = Blob::Spool {
                                offset: sparse_at,
                                size,
                                sparse: true,
                            };
                            sparse_at += size;
                            at
                        }
                    };
                    insert(&mut root, &parts, Node::File { at, exec }, true);
                }
            }
        }
        // git-utils.cc:1352: the links are resolved last, in the order of their
        // own names, and each one only takes a name no member claimed. So a
        // link may name a link whose own name sorts before it.
        for (parts, target) in links {
            let copy = match lookup(&root, &target) {
                Some(node) => node.copy().ok_or_else(|| {
                    bad(format!(
                        "tarball member '{}' is a hard link to the directory '{}'",
                        show(&parts),
                        show(&target)
                    ))
                })?,
                None => {
                    return Err(bad(format!(
                        "tarball member '{}' is a hard link to '{}', which the archive does not hold",
                        show(&parts),
                        show(&target)
                    )));
                }
            };
            insert(&mut root, &parts, copy, false);
        }
        Ok(Tree {
            src: Source::Spool { spool, sparse },
            root: Node::Dir(dereference(root)),
        })
    }

    pub fn stored(root: Node, packs: Arc<Packs>) -> Tree {
        Tree {
            src: Source::Packs(packs),
            root,
        }
    }

    /// Every file's blob, once: a hard link shares its target's.
    pub fn files(&self) -> Vec<Blob> {
        fn walk(n: &Node, seen: &mut HashSet<Blob>, out: &mut Vec<Blob>) {
            match n {
                Node::File { at, .. } => {
                    if seen.insert(*at) {
                        out.push(*at);
                    }
                }
                Node::Dir(d) => d.iter().for_each(|(_, c)| walk(c, seen, out)),
                Node::Symlink(_) => {}
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut Default::default(), &mut out);
        out
    }

    /// The same tree with every file named by the id of its bytes, which `ids`
    /// holds for every blob [`Tree::files`] gave.
    pub fn with_ids(&self, ids: &HashMap<Blob, Id>) -> Node {
        fn walk(n: &Node, ids: &HashMap<Blob, Id>) -> Node {
            match n {
                Node::File { at, exec } => Node::File {
                    at: Blob::Object {
                        id: ids[at],
                        size: at.size(),
                    },
                    exec: *exec,
                },
                Node::Symlink(t) => Node::Symlink(t.clone()),
                Node::Dir(d) => Node::Dir(
                    d.iter()
                        .map(|(name, c)| (name.clone(), walk(c, ids)))
                        .collect(),
                ),
            }
        }
        walk(&self.root, ids)
    }

    /// A spool member's file: no seek, so two threads may read one tree at
    /// once.
    fn spool_source(&self, sparse: bool) -> &File {
        match &self.src {
            Source::Spool {
                sparse: Some(f), ..
            } if sparse => f,
            Source::Spool { spool, .. } => spool,
            Source::Packs(_) => unreachable!("a spool blob in a stored tree"),
        }
    }

    fn object(&self, id: &Id, size: u64) -> io::Result<Vec<u8>> {
        let Source::Packs(packs) = &self.src else {
            unreachable!("an object blob in a spool")
        };
        let bytes = packs.read(id)?;
        if bytes.len() as u64 != size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "an object in the input store is not the size its index says",
            ));
        }
        Ok(bytes)
    }

    /// A member's bytes as a reader, for the NAR writer to pull through its
    /// own buffer. A spool member is read a chunk at a time; an object is one
    /// payload, so it is read whole.
    fn blob(&self, at: &Blob) -> io::Result<Box<dyn Read + '_>> {
        Ok(match *at {
            Blob::Spool {
                offset,
                size,
                sparse,
            } => Box::new(Bytes {
                src: self.spool_source(sparse),
                offset,
                left: size,
            }),
            Blob::Object { id, size } => Box::new(io::Cursor::new(self.object(&id, size)?)),
        })
    }

    /// The member at the resolved components `at`, without following a symlink.
    fn at(&self, at: &[Vec<u8>]) -> Option<&Node> {
        let mut n = &self.root;
        for name in at {
            let Node::Dir(d) = n else { return None };
            n = find(d, name).ok().map(|i| &d[i].1)?;
        }
        Some(n)
    }

    /// The member at `rel` (`b""` is the tree itself), with symlinks followed
    /// the way the kernel follows them for a path under `/nix/store`;
    /// `follow_last` is false for an `lstat`. A target that leaves the tree is
    /// not resolved: the spool holds no bytes for it.
    pub fn resolve(&self, rel: &[u8], follow_last: bool) -> Option<&Node> {
        let split = |p: &[u8]| -> Vec<Vec<u8>> {
            p.split(|&b| b == b'/')
                .filter(|c| !c.is_empty() && c != b".")
                .map(<[u8]>::to_vec)
                .collect()
        };
        let mut todo = split(rel);
        todo.reverse();
        let mut at: Vec<Vec<u8>> = Vec::new();
        let mut hops = 0;
        while let Some(c) = todo.pop() {
            if c == b".." {
                at.pop()?;
                continue;
            }
            at.push(c);
            if let Some(Node::Symlink(target)) = self.at(&at) {
                if todo.is_empty() && !follow_last {
                    break;
                }
                hops += 1;
                if hops > 32 || target.starts_with(b"/") {
                    return None;
                }
                at.pop();
                let mut parts = split(target);
                parts.reverse();
                todo.extend(parts);
            }
        }
        self.at(&at)
    }

    /// A regular member's whole contents, in one `pread` into one buffer: no
    /// `open`, no copy through a staging buffer, so a read of a member costs
    /// less than a read of the same file under `/nix/store`.
    pub fn read(&self, at: &Blob) -> io::Result<Vec<u8>> {
        let (offset, size, sparse) = match *at {
            Blob::Spool {
                offset,
                size,
                sparse,
            } => (offset, size, sparse),
            Blob::Object { id, size } => return self.object(&id, size),
        };
        let mut out = vec![0u8; size as usize];
        self.spool_source(sparse)
            .read_exact_at(&mut out, offset)
            .map_err(|_| io::Error::other("the spooled tarball is truncated"))?;
        Ok(out)
    }

    /// The NAR of the whole tree. `shown` is where a filtered dump names the
    /// nodes; with no filter nothing builds those paths.
    pub fn dump(&self, sink: &mut dyn Write) -> io::Result<()> {
        self.write(&self.root, None, sink, &mut |_, _| true)
    }

    /// The NAR of `node`, with `filter` asked about every entry below it under
    /// the path Nix would show (`shown`), as `nar::dump_path` asks.
    pub fn dump_filtered(
        &self,
        node: &Node,
        shown: &Path,
        sink: &mut dyn Write,
        filter: &mut PathFilter<'_>,
    ) -> io::Result<()> {
        self.write(node, Some(shown), sink, filter)
    }

    fn write(
        &self,
        node: &Node,
        shown: Option<&Path>,
        sink: &mut dyn Write,
        filter: &mut PathFilter<'_>,
    ) -> io::Result<()> {
        let mut w = TreeWriter::new(sink);
        w.header()?;
        self.node(node, shown, &mut w, filter, 0)?;
        w.flush()
    }

    /// One node into the writer, in NAR order. The index already holds every
    /// directory's entries sorted by name, so this walk only follows them.
    fn node(
        &self,
        n: &Node,
        shown: Option<&Path>,
        w: &mut TreeWriter<'_>,
        filter: &mut PathFilter<'_>,
        depth: usize,
    ) -> io::Result<()> {
        if depth >= NAR_MAX_DEPTH {
            return Err(io::Error::other(format!(
                "the tarball exceeds the maximum NAR directory depth of {NAR_MAX_DEPTH}"
            )));
        }
        match n {
            Node::File { at, exec } => w.regular(*exec, at.size(), &mut self.blob(at)?),
            Node::Symlink(target) => w.symlink(target),
            Node::Dir(d) => {
                w.dir()?;
                for (name, child) in d {
                    let below = match shown {
                        None => None,
                        Some(p) => {
                            let at = p.join(OsStr::from_bytes(name));
                            if !filter(&at, child.file_type()) {
                                continue;
                            }
                            Some(at)
                        }
                    };
                    w.entry(name)?;
                    self.node(child, below.as_deref(), w, filter, depth + 1)?;
                    w.end_entry()?;
                }
                w.end_dir()
            }
        }
    }
}

/// Serializes a stored index: the NAR hash and size the fetch verified against
/// the lock, so no reader ever hashes the tree again, then the tree with every
/// file named by its object id. Ends with its own checksum.
pub fn encode_index(root: &Node, nar: (&Hash, u64)) -> Vec<u8> {
    let mut e = Vec::with_capacity(1 << 16);
    e.push(INDEX_VERSION);
    put_bytes(&mut e, nar.0.algo.name().as_bytes());
    put_bytes(&mut e, nar.0.bytes());
    put_u64(&mut e, nar.1);
    put_node(&mut e, root);
    e.extend_from_slice(&checksum(&e));
    e
}

/// The tree, NAR hash and NAR size a stored index holds. `None` on anything
/// unexpected, so the caller wipes the entry rather than repairing it.
pub fn decode_index(bytes: &[u8]) -> Option<(Node, Hash, u64)> {
    let body = bytes.len().checked_sub(SUM)?;
    if checksum(&bytes[..body]) != bytes[body..] {
        return None;
    }
    let mut d = Idx {
        b: &bytes[..body],
        i: 0,
    };
    if d.u8()? != INDEX_VERSION {
        return None;
    }
    let algo = HashAlgo::parse(str::from_utf8(&d.bytes()?).ok()?)?;
    let nar_hash = Hash::from_bytes(algo, &d.bytes()?).ok()?;
    let nar_size = d.u64()?;
    let root = get_node(&mut d, 0)?;
    (d.i == body).then_some((root, nar_hash, nar_size))
}

pub fn object_ids(n: &Node, out: &mut impl Extend<Id>) {
    match n {
        Node::File {
            at: Blob::Object { id, .. },
            ..
        } => out.extend([*id]),
        Node::Dir(d) => d.iter().for_each(|(_, c)| object_ids(c, out)),
        Node::File { .. } | Node::Symlink(_) => {}
    }
}

/// The file the download is spooled into, in a staged directory of the store.
pub const SPOOL: &str = "spool";

/// The spool of a staged download, created for writing and read back to store it.
pub fn spool_file(dir: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.join(SPOOL))
}

fn put_u64(e: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            e.push(b);
            return;
        }
        e.push(b | 0x80);
    }
}

fn put_bytes(e: &mut Vec<u8>, b: &[u8]) {
    put_u64(e, b.len() as u64);
    e.extend_from_slice(b);
}

fn put_node(e: &mut Vec<u8>, n: &Node) {
    match n {
        Node::Dir(d) => {
            e.push(0);
            put_u64(e, d.len() as u64);
            for (name, child) in d {
                put_bytes(e, name);
                put_node(e, child);
            }
        }
        Node::File { at, exec } => {
            let Blob::Object { id, size } = at else {
                unreachable!("an index is stored once its files are objects")
            };
            e.push(1);
            e.push(u8::from(*exec));
            e.extend_from_slice(id);
            put_u64(e, *size);
        }
        Node::Symlink(t) => {
            e.push(2);
            put_bytes(e, t);
        }
    }
}

struct Idx<'a> {
    b: &'a [u8],
    i: usize,
}

impl Idx<'_> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.b.get(self.i)?;
        self.i += 1;
        Some(b)
    }

    fn u64(&mut self) -> Option<u64> {
        let mut n = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            if shift >= 64 {
                return None;
            }
            n |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(n);
            }
            shift += 7;
        }
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u64()? as usize;
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s.to_vec())
    }
}

fn get_node(d: &mut Idx<'_>, depth: usize) -> Option<Node> {
    if depth > NAR_MAX_DEPTH {
        return None;
    }
    Some(match d.u8()? {
        0 => {
            let n = d.u64()? as usize;
            if n > d.b.len().saturating_sub(d.i) {
                return None;
            }
            let mut dir = Dir::with_capacity(n);
            for _ in 0..n {
                let name = d.bytes()?;
                dir.push((name, get_node(d, depth + 1)?));
            }
            if dir.windows(2).any(|w| w[0].0 >= w[1].0) {
                return None;
            }
            Node::Dir(dir)
        }
        1 => {
            let flags = d.u8()?;
            if flags > 1 {
                return None;
            }
            let id: Id = d.b.get(d.i..d.i + 32)?.try_into().ok()?;
            d.i += 32;
            Node::File {
                at: Blob::Object { id, size: d.u64()? },
                exec: flags == 1,
            }
        }
        2 => Node::Symlink(d.bytes()?),
        _ => return None,
    })
}

/// What the `GNU.sparse.*` pax records of a member say: the name the member
/// really has, the size it expands to, and its data blocks. `map` is `None` for
/// format 1.0, which holds the map at the head of the member's data instead.
pub(super) struct Sparse {
    pub(super) name: Option<Vec<u8>>,
    pub(super) size: u64,
    map: Option<Vec<(u64, u64)>>,
}

fn number(v: &[u8]) -> Option<u64> {
    str::from_utf8(v).ok()?.parse().ok()
}

/// The name the pax sparse records give the member, else the header's.
pub(super) fn member_name(
    pax: &Option<Sparse>,
    entry: &tar::Entry<'_, impl Read>,
) -> Result<PathBuf> {
    match pax.as_ref().and_then(|s| s.name.clone()) {
        Some(n) => Ok(OsString::from_vec(n).into()),
        None => Ok(entry
            .path()
            .map_err(|e| Error::io("cannot read a tarball member name", e))?
            .into_owned()),
    }
}

/// The three GNU pax sparse formats (GNU tar, `Sparse Formats`): 0.0 repeats
/// `offset`/`numbytes`, 0.1 holds one `map`, and 1.0 (`major` 1) puts the map in
/// the data. Without these records the member is an ordinary file.
pub(super) fn pax_sparse(entry: &mut tar::Entry<'_, impl Read>) -> Result<Option<Sparse>> {
    let ext = entry
        .pax_extensions()
        .map_err(|e| Error::io("cannot read the pax records of a tarball member", e))?;
    let Some(ext) = ext else { return Ok(None) };
    let (mut name, mut size, mut real, mut major) = (None, None, None, 0);
    let (mut map, mut blocks, mut offset) = (None, Vec::new(), None);
    for rec in ext {
        let rec = rec.map_err(|e| Error::io("cannot read a pax record", e))?;
        let v = rec.value_bytes();
        match rec.key_bytes() {
            b"GNU.sparse.name" => name = Some(v.to_vec()),
            b"GNU.sparse.size" => size = number(v),
            b"GNU.sparse.realsize" => real = number(v),
            b"GNU.sparse.major" => major = number(v).unwrap_or(0),
            b"GNU.sparse.offset" => offset = number(v),
            b"GNU.sparse.numbytes" => match (offset.take(), number(v)) {
                (Some(o), Some(n)) => blocks.push((o, n)),
                _ => return Err(bad("a tarball member has a bad sparse map".into())),
            },
            b"GNU.sparse.map" => {
                let mut it = v.split(|&b| b == b',').map(number);
                let mut got = Vec::new();
                while let Some(o) = it.next() {
                    match (o, it.next().flatten()) {
                        (Some(o), Some(n)) => got.push((o, n)),
                        _ => return Err(bad("a tarball member has a bad sparse map".into())),
                    }
                }
                map = Some(got);
            }
            _ => {}
        }
    }
    if major >= 1 {
        let Some(size) = real else {
            return Err(bad("a sparse tarball member has no 'realsize'".into()));
        };
        return Ok(Some(Sparse {
            name,
            size,
            map: None,
        }));
    }
    let map = match map {
        Some(m) => m,
        None if !blocks.is_empty() => blocks,
        None => return Ok(None),
    };
    let Some(size) = size else {
        return Err(bad("a sparse tarball member has no 'size'".into()));
    };
    Ok(Some(Sparse {
        name,
        size,
        map: Some(map),
    }))
}

/// A sparse member written out whole at `base` in the side file: its data
/// blocks where they belong, and holes for the rest.
pub(super) fn expand(
    entry: &mut tar::Entry<'_, impl Read>,
    pax: Option<&Sparse>,
    side: &File,
    base: u64,
    size: u64,
) -> io::Result<()> {
    let Some(pax) = pax else {
        // an old-style GNU sparse member: the tar reader expands it itself
        let mut side = side;
        side.seek(SeekFrom::Start(base))?;
        io::copy(entry, &mut side)?;
        return Ok(());
    };
    let map = match &pax.map {
        Some(m) => m.clone(),
        None => read_map(entry)?,
    };
    let mut buf = vec![0u8; CHUNK];
    let mut end = 0u64;
    for (at, len) in map {
        if at < end || at.checked_add(len).is_none_or(|e| e > size) {
            return Err(io::Error::other("a sparse map that does not fit the file"));
        }
        end = at + len;
        let mut left = len;
        let mut to = base + at;
        while left > 0 {
            let n = left.min(buf.len() as u64) as usize;
            entry.read_exact(&mut buf[..n])?;
            side.write_all_at(&buf[..n], to)?;
            to += n as u64;
            left -= n as u64;
        }
    }
    side.set_len(base + size)
}

/// The map a format 1.0 sparse member holds at the head of its data: decimal
/// numbers on their own lines, padded out to whole 512-byte blocks.
fn read_map(entry: &mut impl Read) -> io::Result<Vec<(u64, u64)>> {
    let mut at = Decimals {
        src: entry,
        buf: [0; 512],
        pos: 512,
    };
    let count = at.next()?;
    let mut map = Vec::new();
    for _ in 0..count {
        map.push((at.next()?, at.next()?));
    }
    Ok(map)
}

struct Decimals<'a, R: Read> {
    src: &'a mut R,
    buf: [u8; 512],
    pos: usize,
}

impl<R: Read> Decimals<'_, R> {
    fn next(&mut self) -> io::Result<u64> {
        let (mut n, mut digits) = (0u64, 0);
        loop {
            if self.pos == self.buf.len() {
                self.src.read_exact(&mut self.buf)?;
                self.pos = 0;
            }
            let b = self.buf[self.pos];
            self.pos += 1;
            if b == b'\n' && digits > 0 {
                return Ok(n);
            }
            let d = u64::from(b.wrapping_sub(b'0'));
            n = match (d < 10)
                .then(|| n.checked_mul(10)?.checked_add(d))
                .flatten()
            {
                Some(n) => n,
                None => return Err(io::Error::other("a bad sparse map in a tarball member")),
            };
            digits += 1;
        }
    }
}

fn link_target(
    entry: &mut tar::Entry<'_, impl Read>,
    parts: &[Vec<u8>],
    what: &str,
) -> Result<Vec<u8>> {
    let target = entry
        .link_name()
        .map_err(|e| Error::io(format!("cannot read a {what} target"), e))?
        .ok_or_else(|| bad(format!("tarball {what} '{}' has no target", show(parts))))?;
    Ok(target.as_os_str().as_bytes().to_vec())
}

/// git-utils.cc:dereferenceSingletonDirectory: one entry, and a directory.
fn dereference(mut root: Dir) -> Dir {
    if root.len() == 1 && matches!(root[0].1, Node::Dir(_)) {
        match root.pop() {
            Some((_, Node::Dir(d))) => d,
            _ => unreachable!("a directory"),
        }
    } else {
        root
    }
}

/// A member's bytes, read by `pread` from where they lie: the spooled archive,
/// or the side file that holds the expanded sparse members. The NAR writer
/// pulls through its own buffer, so no chunk of a member is ever held twice.
struct Bytes<'a> {
    src: &'a File,
    offset: u64,
    left: u64,
}

impl Read for Bytes<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (self.left.min(buf.len() as u64) as usize).min(CHUNK);
        if n == 0 {
            return Ok(0);
        }
        self.src
            .read_exact_at(&mut buf[..n], self.offset)
            .map_err(|_| io::Error::other("the spooled tarball is truncated"))?;
        self.offset += n as u64;
        self.left -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::symlink;

    use crate::fetch::http;
    use crate::store::hash::Hasher;
    use crate::store::nar;
    use crate::treestore::pack::{self, PackWriter};

    const DIR: tar::EntryType = tar::EntryType::Directory;
    const REG: tar::EntryType = tar::EntryType::Regular;
    const SYM: tar::EntryType = tar::EntryType::Symlink;
    const HARD: tar::EntryType = tar::EntryType::Link;

    fn tar_gz(entries: &[(&str, tar::EntryType, u32, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, kind, mode, data) in entries {
            let carries = !matches!(*kind, DIR | SYM | HARD);
            let mut h = tar::Header::new_ustar();
            h.set_entry_type(*kind);
            h.set_mode(*mode);
            h.set_size(if carries { data.len() as u64 } else { 0 });
            if *kind == SYM || *kind == HARD {
                h.set_link_name(str::from_utf8(data).unwrap()).unwrap();
            }
            // `set_path` refuses a `..`, which is a member name a tarball may
            // hold; the field is 100 bytes and these names are short
            let raw = &mut h.as_ustar_mut().unwrap().name;
            raw[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            let body: &[u8] = if carries { data } else { &[] };
            b.append(&h, body).unwrap();
        }
        let tar = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }

    /// The NAR the spool writes for `bytes`, and the NAR the tree path writes
    /// for the same archive.
    fn both(tag: &str, bytes: Vec<u8>) -> (Result<Vec<u8>>, Result<Vec<u8>>) {
        let plain = http::decompress(bytes.clone()).unwrap();
        let mut spool = scratch(tag).unwrap();
        spool.write_all(&plain).unwrap();
        let streamed = Tree::index(spool, u64::MAX).and_then(|t| {
            let mut nar = Vec::new();
            t.dump(&mut nar)
                .map_err(|e| Error::io("cannot dump the spool", e))?;
            Ok(nar)
        });

        let dest = env::temp_dir().join(format!("iets-spool-tree-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        let tree = unpack::unpack(plain, &dest).and_then(|root| {
            let mut nar = Vec::new();
            nar::dump_path(&root, &mut nar, &mut |_, _| true)
                .map_err(|e| Error::io("cannot dump the tree", e))?;
            Ok(nar)
        });
        let _ = fs::remove_dir_all(&dest);
        (streamed, tree)
    }

    fn same(tag: &str, entries: &[(&str, tar::EntryType, u32, &[u8])]) -> Vec<u8> {
        let (streamed, tree) = both(tag, tar_gz(entries));
        let streamed = streamed.unwrap_or_else(|e| panic!("{tag}: the spool path failed: {e}"));
        let tree = tree.unwrap_or_else(|e| panic!("{tag}: the tree path failed: {e}"));
        assert_eq!(streamed, tree, "{tag}: the two NARs differ");
        streamed
    }

    /// Every rule the git sink applies, both ways, byte for byte.
    #[test]
    fn spool_matches_the_tree() {
        let big = vec![b'z'; 70 * 1024];
        same(
            "rules",
            &[
                (
                    "pax_global_header",
                    tar::EntryType::XGlobalHeader,
                    0o644,
                    b"",
                ),
                ("repo-abc/", DIR, 0o755, b""),
                ("repo-abc/flake.nix", REG, 0o644, b"{}\n"),
                ("repo-abc/run", REG, 0o755, b"#!/bin/sh\n"),
                ("repo-abc/empty/", DIR, 0o700, b""),
                ("repo-abc/link", SYM, 0o777, b"flake.nix"),
                ("repo-abc/dup", REG, 0o644, b"first"),
                ("repo-abc/dup", REG, 0o755, b"second"),
                ("repo-abc/same", HARD, 0o644, b"repo-abc/run"),
                ("repo-abc/deep/a/b/c", REG, 0o644, b"nested\n"),
                ("repo-abc/big", REG, 0o644, &big),
            ],
        );
    }

    /// One leading directory is stripped; two top-level entries are not.
    #[test]
    fn spool_matches_the_tree_on_the_top_level() {
        let one = same(
            "single",
            &[("t/", DIR, 0o755, b""), ("t/f", REG, 0o644, b"x")],
        );
        let two = same(
            "double",
            &[
                ("t/", DIR, 0o755, b""),
                ("t/f", REG, 0o644, b"x"),
                ("g", REG, 0o644, b"y"),
            ],
        );
        assert_ne!(one, two);
        same("file-at-top", &[("only", REG, 0o644, b"x")]);
    }

    /// An empty archive is the NAR of an empty directory.
    #[test]
    fn an_empty_archive_is_an_empty_directory() {
        let nar = same("empty", &[]);
        let dest = env::temp_dir().join(format!("iets-spool-bare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        fs::create_dir_all(&dest).unwrap();
        let mut want = Vec::new();
        nar::dump_path(&dest, &mut want, &mut |_, _| true).unwrap();
        assert_eq!(nar, want);
        fs::remove_dir_all(&dest).unwrap();
    }

    /// The same errors as the tree path, for the same archives.
    #[test]
    fn the_errors_match() {
        for (tag, entries, why) in [
            (
                "fifo",
                vec![("dev", tar::EntryType::Fifo, 0o644u32, &b""[..])],
                "unsupported file type",
            ),
            (
                "dangling-link",
                vec![("l", HARD, 0o644u32, &b"missing"[..])],
                "hard link",
            ),
            (
                "link-to-dir",
                vec![
                    ("d/", DIR, 0o755u32, &b""[..]),
                    ("l", HARD, 0o644, &b"d"[..]),
                    ("k", REG, 0o644, &b"x"[..]),
                ],
                "hard link",
            ),
        ] {
            let (streamed, tree) = both(tag, tar_gz(&entries));
            let e = streamed.unwrap_err().to_string();
            assert!(e.contains(why), "{tag}: {e}");
            assert!(tree.is_err(), "{tag}: the tree path accepted it");
        }
    }

    /// The archives the git sink takes and libarchive's extract flags would
    /// not: a `..` that `CanonPath` applies, a member below a symlink, and a
    /// type flag libarchive gives `AE_IFREG`.
    #[test]
    fn archives_the_git_sink_accepts() {
        let escaped = same(
            "dotdot",
            &[
                ("t/", DIR, 0o755, b""),
                ("t/a", REG, 0o644, b"in"),
                ("t/../../esc", REG, 0o644, b"out"),
            ],
        );
        let want = same(
            "dotdot-want",
            &[
                ("t/", DIR, 0o755, b""),
                ("t/a", REG, 0o644, b"in"),
                ("esc", REG, 0o644, b"out"),
            ],
        );
        assert_eq!(escaped, want, "'..' is applied, not refused");

        let under = same(
            "under-a-symlink",
            &[
                ("t/", DIR, 0o755, b""),
                ("t/s", SYM, 0o777, b"/etc"),
                ("t/s/f", REG, 0o644, b"x"),
            ],
        );
        let want = same(
            "under-a-symlink-want",
            &[("t/", DIR, 0o755, b""), ("t/s/f", REG, 0o644, b"x")],
        );
        assert_eq!(under, want, "the symlink becomes the directory");

        let unknown = same(
            "unknown-type-flag",
            &[
                ("t/", DIR, 0o755, b""),
                ("t/f", tar::EntryType::new(b'Z'), 0o644, b"hello"),
            ],
        );
        let want = same(
            "unknown-type-flag-want",
            &[("t/", DIR, 0o755, b""), ("t/f", REG, 0o644, b"hello")],
        );
        assert_eq!(unknown, want, "an unknown type flag is a regular file");
    }

    /// The tree as the store keeps it: its files as objects in a pack under
    /// `dir`, and the index naming them.
    fn stored(dir: &Path, tree: &Tree) -> (Tree, Vec<u8>) {
        let mut w = PackWriter::new(dir).unwrap();
        let mut ids = HashMap::new();
        for at in tree.files() {
            let bytes = tree.read(&at).unwrap();
            let id = pack::id_of(&bytes);
            w.add(id, &bytes).unwrap();
            ids.insert(at, id);
        }
        let root = tree.with_ids(&ids);
        w.finish().unwrap();
        let mut h = Hasher::new(HashAlgo::Sha256);
        tree.dump(&mut h).unwrap();
        let (size, hash) = (h.bytes_written(), h.finish());
        let index = encode_index(&root, (&hash, size));
        let packs = Arc::new(Packs::open(dir));
        (Tree::stored(root, packs), index)
    }

    fn spooled(entries: &[(&str, tar::EntryType, u32, &[u8])]) -> Tree {
        let plain = http::decompress(tar_gz(entries)).unwrap();
        let mut spool = scratch("test").unwrap();
        spool.write_all(&plain).unwrap();
        Tree::index(spool, u64::MAX).unwrap()
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("iets-spool-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stored tree is the tree that was fetched: the same NAR, member for
    /// member, and the same answers to a read. Two members with one content
    /// are one object.
    #[test]
    fn a_stored_tree_dumps_the_spools_nar() {
        let dir = tmp_dir("stored");
        let long = vec![b'z'; 10_000];
        let tree = spooled(&[
            ("t/", DIR, 0o755, b""),
            ("t/a", REG, 0o644, b"same"),
            ("t/sub/", DIR, 0o755, b""),
            ("t/sub/b", REG, 0o755, b"same"),
            ("t/sub/long", REG, 0o644, &long),
            ("t/empty", REG, 0o644, b""),
            ("t/l", SYM, 0o777, b"sub/b"),
        ]);
        let (back, index) = stored(&dir, &tree);
        let (mut want, mut got) = (Vec::new(), Vec::new());
        tree.dump(&mut want).unwrap();
        back.dump(&mut got).unwrap();
        assert_eq!(got, want);
        let Some(Node::File { at, .. }) = back.resolve(b"l", true) else {
            panic!("the symlink resolves to a file")
        };
        assert_eq!(back.read(at).unwrap(), b"same");
        let mut ids = HashSet::new();
        let (root, _, _) = decode_index(&index).expect("the index decodes");
        object_ids(&root, &mut ids);
        assert_eq!(
            ids.len(),
            3,
            "\"same\" twice, the long file and the empty one"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// No byte of a stored index is believed on its own: one flipped byte is a
    /// miss, never a crash and never a tree that is not the one stored.
    #[test]
    fn a_changed_index_byte_is_a_miss() {
        let dir = tmp_dir("index");
        let tree = spooled(&[
            ("t/", DIR, 0o755, b""),
            ("t/f", REG, 0o644, b"x"),
            ("t/l", SYM, 0o777, b"f"),
        ]);
        let (_, good) = stored(&dir, &tree);
        assert!(decode_index(&good).is_some());
        for i in 0..good.len() {
            let mut broken = good.clone();
            broken[i] ^= 0xff;
            assert!(decode_index(&broken).is_none(), "byte {i} was believed");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A hard link fills a name no member claimed, and it may name a link whose
    /// own name sorts before it (git-utils.cc:1352).
    #[test]
    fn a_hard_link_never_replaces_a_member() {
        let later = same(
            "link-then-member",
            &[
                ("t/", DIR, 0o755, b""),
                ("t/f", REG, 0o644, b"first"),
                ("t/h", HARD, 0o644, b"t/f"),
                ("t/h", REG, 0o644, b"second"),
            ],
        );
        let want = same(
            "link-then-member-want",
            &[
                ("t/", DIR, 0o755, b""),
                ("t/f", REG, 0o644, b"first"),
                ("t/h", REG, 0o644, b"second"),
            ],
        );
        assert_eq!(later, want, "the member after the link must win");

        let chain = same(
            "link-to-a-later-link",
            &[
                ("t/", DIR, 0o755, b""),
                ("t/f", REG, 0o644, b"x"),
                ("t/h2", HARD, 0o644, b"t/h1"),
                ("t/h1", HARD, 0o644, b"t/f"),
            ],
        );
        let want = same(
            "link-to-a-later-link-want",
            &[
                ("t/", DIR, 0o755, b""),
                ("t/f", REG, 0o644, b"x"),
                ("t/h1", REG, 0o644, b"x"),
                ("t/h2", REG, 0o644, b"x"),
            ],
        );
        assert_eq!(chain, want, "a link to a link that comes later");
    }

    /// A symlink at the guessable name `iets-spool-<pid>-<tag>`: the open must
    /// not follow it.
    #[test]
    fn the_scratch_file_never_follows_a_symlink() {
        let dir = env::temp_dir();
        let victim = dir.join(format!("iets-spool-victim-{}", std::process::id()));
        fs::write(&victim, b"precious").unwrap();
        let bait = dir.join(format!("iets-spool-{}-bait", std::process::id()));
        let _ = fs::remove_file(&bait);
        symlink(&victim, &bait).unwrap();
        let mut file = scratch("bait").unwrap();
        file.write_all(b"the archive").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        fs::remove_file(&bait).unwrap();
        fs::remove_file(&victim).unwrap();
    }

    /// One pax local extended header and the member it belongs to.
    fn pax_tar(records: &[(&str, String)], name: &str, data: &[u8]) -> Vec<u8> {
        let one = |key: &str, value: &str| {
            let body = format!(" {key}={value}\n");
            let mut len = body.len() + 1;
            loop {
                let rec = format!("{len}{body}");
                if rec.len() == len {
                    return rec;
                }
                len = rec.len();
            }
        };
        let recs: String = records.iter().map(|(k, v)| one(k, v)).collect();
        let mut b = tar::Builder::new(Vec::new());
        for (kind, name, body) in [
            (tar::EntryType::XHeader, "PaxHeaders/x", recs.as_bytes()),
            (REG, name, data),
        ] {
            let mut h = tar::Header::new_ustar();
            h.set_entry_type(kind);
            h.set_mode(0o644);
            h.set_size(body.len() as u64);
            b.append_data(&mut h, name, body).unwrap();
        }
        b.into_inner().unwrap()
    }

    /// 4096 zeros, `abcd`, 4092 zeros: one hole, one block and one hole.
    fn holey() -> Vec<u8> {
        let mut want = vec![0u8; 8192];
        want[4096..4100].copy_from_slice(b"abcd");
        want
    }

    /// GNU tar's three pax sparse formats give the file the archive describes,
    /// under the name its records name, and not the condensed bytes.
    #[test]
    fn pax_sparse_members_are_expanded() {
        fn n(k: &str, v: u64) -> (&str, String) {
            (k, v.to_string())
        }
        let mut one_zero = b"1\n4096\n4\n".to_vec();
        one_zero.resize(512, 0);
        one_zero.extend_from_slice(b"abcd");
        for (tag, records, name, data) in [
            (
                "v0.0",
                vec![
                    n("GNU.sparse.size", 8192),
                    n("GNU.sparse.numblocks", 1),
                    n("GNU.sparse.offset", 4096),
                    n("GNU.sparse.numbytes", 4),
                ],
                "top/sparse",
                b"abcd".to_vec(),
            ),
            (
                "v0.1",
                vec![
                    n("GNU.sparse.size", 8192),
                    n("GNU.sparse.numblocks", 1),
                    ("GNU.sparse.name", "top/sparse".into()),
                    ("GNU.sparse.map", "4096,4".into()),
                ],
                "top/GNUSparseFile.1/sparse",
                b"abcd".to_vec(),
            ),
            (
                "v1.0",
                vec![
                    n("GNU.sparse.major", 1),
                    n("GNU.sparse.minor", 0),
                    ("GNU.sparse.name", "top/sparse".into()),
                    n("GNU.sparse.realsize", 8192),
                ],
                "top/GNUSparseFile.1/sparse",
                one_zero.clone(),
            ),
        ] {
            let (streamed, tree) = both(tag, pax_tar(&records, name, &data));
            let streamed = streamed.unwrap_or_else(|e| panic!("{tag}: {e}"));
            assert_eq!(streamed, tree.unwrap(), "{tag}: the two NARs differ");
            let want = same(
                &format!("{tag}-plain"),
                &[
                    ("top/", DIR, 0o755, b""),
                    ("top/sparse", REG, 0o644, &holey()),
                ],
            );
            assert_eq!(streamed, want, "{tag}: not the file the records describe");
        }
    }

    /// A member that expands past `fetch-max-size` is refused before it is
    /// written: the archive is small, the file it claims is not.
    #[test]
    fn a_sparse_member_counts_against_the_size_limit() {
        let bomb = pax_tar(
            &[
                ("GNU.sparse.major", "1".into()),
                ("GNU.sparse.minor", "0".into()),
                ("GNU.sparse.name", "top/hole".into()),
                ("GNU.sparse.realsize", (1u64 << 40).to_string()),
            ],
            "top/GNUSparseFile.1/hole",
            &{
                let mut d = b"0\n".to_vec();
                d.resize(512, 0);
                d
            },
        );
        assert!(bomb.len() < 4096, "the archive is small: {}", bomb.len());
        let mut spool = scratch("bomb").unwrap();
        spool.write_all(&bomb).unwrap();
        let e = Tree::index(spool, 1 << 20).unwrap_err().to_string();
        assert!(e.contains("fetch-max-size"), "{e}");
    }

    /// The fixture whose `narHash` a lock file records, hashed out of the spool.
    #[test]
    fn nar_hash_of_the_fixture() {
        let bytes =
            fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/tarball/tree.tar.gz"))
                .unwrap();
        let (streamed, tree) = both("golden", bytes);
        let streamed = streamed.unwrap();
        assert_eq!(streamed, tree.unwrap());
        let want = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/tarball/narHash"),
        )
        .unwrap();
        let mut h = Hasher::new(HashAlgo::Sha256);
        h.write_all(&streamed).unwrap();
        assert_eq!(h.finish().to_sri(), want.trim());
    }
}
