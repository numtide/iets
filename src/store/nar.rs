use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::{Scope, ScopedJoinHandle};

use super::hash::{Hash, HashAlgo, Hasher};

/// The Nix type name passed to `builtins.filterSource` filters (primops.cc:fileTypeToString).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    Unknown,
}

impl FileType {
    pub fn nix_name(self) -> &'static str {
        match self {
            FileType::Regular => "regular",
            FileType::Directory => "directory",
            FileType::Symlink => "symlink",
            FileType::Unknown => "unknown",
        }
    }

    pub(crate) fn of(ft: fs::FileType) -> FileType {
        if ft.is_symlink() {
            FileType::Symlink
        } else if ft.is_dir() {
            FileType::Directory
        } else if ft.is_file() {
            FileType::Regular
        } else {
            FileType::Unknown
        }
    }
}

/// Filter called on each directory entry with its absolute path; `false` skips it. Mirrors
/// archive.hh:PathFilter as used by primops.cc:addPath.
pub type PathFilter<'a> = dyn FnMut(&Path, FileType) -> bool + 'a;

pub(crate) const NAR_MAX_DEPTH: usize = 64;

/// One buffer flush is one `write_all` on the sink, hence one hasher update:
/// long enough that SHA-256 runs over long runs, short enough that the buffer
/// and the file being read both stay in cache.
const BUF: usize = 1 << 20;
/// A content read never asks for less than this; the sink flushes first instead.
const MIN_READ: usize = 1 << 16;

/// The framed form of a run of constant tokens.
///
/// Most of a NAR is fixed keywords, and `write_str` of a literal is a
/// compile-time byte string, so a whole run of them is one memcpy rather than a
/// dozen bounds-checked appends. `N` is checked against the encoded length at
/// compile time.
pub const fn framed<const N: usize>(toks: &[&[u8]]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut o = 0;
    let mut t = 0;
    while t < toks.len() {
        let s = toks[t];
        let mut i = 0;
        while i < 8 {
            out[o + i] = (s.len() >> (8 * i)) as u8;
            i += 1;
        }
        o += 8;
        let mut j = 0;
        while j < s.len() {
            out[o + j] = s[j];
            j += 1;
        }
        o += s.len().next_multiple_of(8);
        t += 1;
    }
    assert!(o == N, "framed: wrong N");
    out
}

// The groups nix-community/go-nix PR #123 (pkg/narv2, edef) cuts the format
// into. They are chosen so that the reader can tell the four node shapes apart
// from a single byte: peek 32 bytes at a node's type token and byte 16 is the
// length prefix of whatever follows the type — `target` (6) after `symlink`,
// `contents` (8) after `regular`, `executable` (10) after an executable
// `regular` — except for `directory`, which is long enough that byte 16 is its
// own last letter, `y`.
const TOK_NAR: [u8; 56] = framed(&[b"nix-archive-1", b"(", b"type"]);
const TOK_REG: [u8; 32] = framed(&[b"regular", b"contents"]);
const TOK_EXE: [u8; 64] = framed(&[b"regular", b"executable", b"", b"contents"]);
const TOK_SYM: [u8; 32] = framed(&[b"symlink", b"target"]);
const TOK_DIR: [u8; 24] = framed(&[b"directory"]);
const TOK_ENT: [u8; 48] = framed(&[b"entry", b"(", b"name"]);
const TOK_NOD: [u8; 48] = framed(&[b"node", b"(", b"type"]);
const TOK_PAR: [u8; 16] = framed(&[b")"]);

/// What byte 16 of a peeked node header says the node is.
const TAG_SYM: u8 = 6;
const TAG_REG: u8 = 8;
const TAG_EXE: u8 = 10;
const TAG_DIR: u8 = b'y';

/// The longest entry name the reader accepts: `NAME_MAX`, so a name it accepts
/// is a name the file system can hold.
const MAX_NAME: usize = 255;
/// The longest symlink target: `PATH_MAX` less its NUL.
const MAX_TARGET: usize = 4095;

/// The staging buffer a NAR write funnels through.
///
/// NAR framing is 8-to-24-byte tokens, a dozen per file system object, and the
/// sink behind them is a `dyn Write` — usually a hasher, so every token would
/// otherwise cost an indirect call plus a SHA-256 block-buffer copy. Buffering
/// turns a whole subtree of framing into one `write_all`, and file contents are
/// read straight into the same buffer, so the bytes are still copied only once.
pub struct NarSink<'a> {
    out: &'a mut dyn Write,
    buf: Box<[u8]>,
    n: usize,
}

impl<'a> NarSink<'a> {
    /// A sink that starts with the small buffer and grows to `BUF` only if a
    /// file's contents ask for it. Most NARs an evaluation writes are a few
    /// kilobytes, and the sink is built once per NAR, so zeroing a megabyte up
    /// front costs more than the flushes it saves.
    pub fn new(out: &'a mut dyn Write) -> NarSink<'a> {
        NarSink::with_capacity(out, 0)
    }

    /// A sink whose buffer holds `cap` bytes, rounded up to fit one content read.
    pub fn with_capacity(out: &'a mut dyn Write, cap: usize) -> NarSink<'a> {
        NarSink {
            out,
            buf: vec![0u8; cap.max(2 * MIN_READ)].into_boxed_slice(),
            n: 0,
        }
    }

    /// Grows to `BUF` for a file that would not fit, which is where the larger
    /// buffer pays for itself in read syscalls.
    #[cold]
    fn grow(&mut self) -> io::Result<()> {
        self.flush()?;
        self.buf = vec![0u8; BUF].into_boxed_slice();
        Ok(())
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        if b.len() > self.buf.len() - self.n {
            self.flush()?;
            if b.len() > self.buf.len() {
                self.out.write_all(b)?;
                return Ok(());
            }
        }
        self.buf[self.n..self.n + b.len()].copy_from_slice(b);
        self.n += b.len();
        Ok(())
    }

    fn u64(&mut self, n: u64) -> io::Result<()> {
        self.put(&n.to_le_bytes())
    }

    fn padding(&mut self, len: u64) -> io::Result<()> {
        let rem = (len % 8) as usize;
        if rem != 0 {
            self.put(&[0u8; 8][..8 - rem])?;
        }
        Ok(())
    }

    /// A run of tokens already in their framed form, from [`framed`].
    /// `N` is a constant so the copy compiles to a few stores; through a plain
    /// slice it would be a `memmove` call, which is most of the cost at this size.
    #[inline]
    pub fn preframed<const N: usize>(&mut self, run: &[u8; N]) -> io::Result<()> {
        if N > self.buf.len() - self.n {
            return self.put(run);
        }
        let n = self.n;
        self.buf[n..n + N].copy_from_slice(run);
        self.n = n + N;
        Ok(())
    }

    /// One NAR token: length-prefixed, zero-padded to 8.
    #[inline]
    pub fn token(&mut self, s: &[u8]) -> io::Result<()> {
        let need = 8 + s.len().next_multiple_of(8);
        if need > self.buf.len() - self.n {
            return self.token_slow(s, need);
        }
        let n = self.n;
        self.buf[n..n + 8].copy_from_slice(&(s.len() as u64).to_le_bytes());
        self.buf[n + 8..n + 8 + s.len()].copy_from_slice(s);
        self.buf[n + 8 + s.len()..n + need].fill(0);
        self.n = n + need;
        Ok(())
    }

    #[cold]
    fn token_slow(&mut self, s: &[u8], need: usize) -> io::Result<()> {
        self.flush()?;
        if need <= self.buf.len() {
            return self.token(s);
        }
        self.u64(s.len() as u64)?;
        self.put(s)?;
        self.padding(s.len() as u64)
    }

    /// The `contents` field of a regular file: exactly `len` bytes read from
    /// `src`, padded to 8.
    pub fn contents(&mut self, src: &mut dyn Read, len: u64) -> io::Result<()> {
        self.u64(len)?;
        if len > self.buf.len() as u64 && self.buf.len() < BUF {
            self.grow()?;
        }
        let mut left = len;
        while left > 0 {
            if self.buf.len() - self.n < MIN_READ {
                self.flush()?;
            }
            let want = (self.buf.len() - self.n).min(left.min(BUF as u64) as usize);
            let got = self.out_read(src, want)?;
            left -= got as u64;
        }
        self.padding(len)
    }

    fn out_read(&mut self, src: &mut dyn Read, want: usize) -> io::Result<usize> {
        let got = src.read(&mut self.buf[self.n..self.n + want])?;
        if got == 0 {
            return Err(io::Error::other("file ended early while reading contents"));
        }
        self.n += got;
        Ok(got)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.n > 0 {
            self.out.write_all(&self.buf[..self.n])?;
            self.n = 0;
        }
        Ok(())
    }
}

/// The one NAR writer: every NAR byte iets produces is framed here.
///
/// A tree that is walked — a directory on disk, an indexed archive, a string —
/// drives it with the nodes it finds, in NAR order, and keeps its own
/// traversal. The framing, the buffering and the content copy are this type's.
/// Each call writes one whole thing: `regular` and `symlink` close their node,
/// `dir` opens one and `end_dir` closes it, and `entry`/`end_entry` wrap the
/// child between them.
pub struct TreeWriter<'a> {
    sink: NarSink<'a>,
}

impl<'a> TreeWriter<'a> {
    pub fn new(out: &'a mut dyn Write) -> TreeWriter<'a> {
        TreeWriter {
            sink: NarSink::new(out),
        }
    }

    /// `nix-archive-1`, and the head of the root node.
    pub fn header(&mut self) -> io::Result<()> {
        self.sink.preframed(&TOK_NAR)
    }

    /// A regular file: exactly `len` bytes read from `src`.
    pub fn regular(&mut self, executable: bool, len: u64, src: &mut dyn Read) -> io::Result<()> {
        if executable {
            self.sink.preframed(&TOK_EXE)?;
        } else {
            self.sink.preframed(&TOK_REG)?;
        }
        self.sink.contents(src, len)?;
        self.sink.preframed(&TOK_PAR)
    }

    pub fn symlink(&mut self, target: &[u8]) -> io::Result<()> {
        self.sink.preframed(&TOK_SYM)?;
        self.sink.token(target)?;
        self.sink.preframed(&TOK_PAR)
    }

    pub fn dir(&mut self) -> io::Result<()> {
        self.sink.preframed(&TOK_DIR)
    }

    /// The head of one directory entry; the child node follows.
    pub fn entry(&mut self, name: &[u8]) -> io::Result<()> {
        self.sink.preframed(&TOK_ENT)?;
        self.sink.token(name)?;
        self.sink.preframed(&TOK_NOD)
    }

    pub fn end_entry(&mut self) -> io::Result<()> {
        self.sink.preframed(&TOK_PAR)
    }

    pub fn end_dir(&mut self) -> io::Result<()> {
        self.sink.preframed(&TOK_PAR)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

/// Mirrors archive.cc:SourceAccessor::dumpPath; writes the NAR of `path` into `sink`.
pub fn dump_path(path: &Path, sink: &mut dyn Write, filter: &mut PathFilter<'_>) -> io::Result<()> {
    let ft = FileType::of(fs::symlink_metadata(path)?.file_type());
    let mut root = path.as_os_str().as_bytes().to_vec();
    // The walk appends "/name", so a trailing slash on the root would double it
    // and change the paths the filter sees.
    while root.len() > 1 && root.last() == Some(&b'/') {
        root.pop();
    }
    let mut w = Walk {
        w: TreeWriter::new(sink),
        path: root,
        names: Vec::with_capacity(4096),
        index: Vec::with_capacity(256),
    };
    w.w.header()?;
    w.node(ft, filter, 0)?;
    w.w.flush()
}

/// The scratch a dump reuses at every level: one path buffer, one arena of entry
/// names and one entry index, all kept to stack discipline, so walking 40k
/// entries allocates nothing past the first few levels.
struct Walk<'a> {
    w: TreeWriter<'a>,
    /// The absolute path of the node being written, as bytes. `PathBuf::pop`
    /// re-parses the whole path to find the last separator; truncation does not.
    path: Vec<u8>,
    names: Vec<u8>,
    /// `(offset into the level's slice of `names`, name length, type)`.
    index: Vec<(u32, u32, FileType)>,
}

impl Walk<'_> {
    fn cur(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.path))
    }

    /// Writes the node at `self.path`, whose type the parent's `readdir` — or the
    /// root's `lstat` — already told us.
    fn node(&mut self, ft: FileType, filter: &mut PathFilter<'_>, depth: usize) -> io::Result<()> {
        if depth >= NAR_MAX_DEPTH {
            return Err(io::Error::other(format!(
                "path '{}' exceeds maximum NAR directory depth of {NAR_MAX_DEPTH}",
                self.cur().display()
            )));
        }
        match ft {
            FileType::Regular => self.regular(),
            FileType::Directory => {
                self.w.dir()?;
                self.directory(filter, depth)?;
                self.w.end_dir()
            }
            FileType::Symlink => {
                let target = fs::read_link(self.cur())?;
                self.w.symlink(target.as_os_str().as_bytes())
            }
            FileType::Unknown => Err(io::Error::other(format!(
                "file '{}' has an unsupported type",
                self.cur().display()
            ))),
        }
    }

    fn regular(&mut self) -> io::Result<()> {
        let Walk { w, path, .. } = self;
        regular_file(w, Path::new(OsStr::from_bytes(path)))
    }

    fn directory(&mut self, filter: &mut PathFilter<'_>, depth: usize) -> io::Result<()> {
        let name_base = self.names.len();
        let idx_base = self.index.len();
        for e in fs::read_dir(self.cur())? {
            let e = e?;
            let off = self.names.len() - name_base;
            self.names.extend_from_slice(e.file_name().as_bytes());
            let len = self.names.len() - name_base - off;
            let ft = match e.file_type() {
                Ok(ft) => FileType::of(ft),
                // A file system whose `dirent` carries no `d_type`.
                Err(_) => FileType::of(fs::symlink_metadata(e.path())?.file_type()),
            };
            self.index.push((off as u32, len as u32, ft));
        }
        let idx_end = self.index.len();
        {
            let Walk { names, index, .. } = self;
            let arena = &names[name_base..];
            let key = |e: &(u32, u32, FileType)| &arena[e.0 as usize..(e.0 + e.1) as usize];
            index[idx_base..].sort_unstable_by(|a, b| key(a).cmp(key(b)));
        }
        let path_base = self.path.len();
        for i in idx_base..idx_end {
            let (off, len, ft) = self.index[i];
            let from = name_base + off as usize;
            let to = from + len as usize;
            self.path.push(b'/');
            self.path.extend_from_slice(&self.names[from..to]);
            if filter(self.cur(), ft) {
                {
                    let Walk { names, w, .. } = self;
                    w.entry(&names[from..to])?;
                }
                // A child directory leaves `names` and `index` exactly as it
                // found them, so the siblings still to come stay addressable.
                self.node(ft, filter, depth + 1)?;
                self.w.end_entry()?;
            }
            self.path.truncate(path_base);
        }
        self.index.truncate(idx_base);
        self.names.truncate(name_base);
        Ok(())
    }
}

/// Mirrors archive.cc:dumpString: the NAR of a regular, non-executable file with `contents`.
pub fn dump_string(contents: &[u8], sink: &mut dyn Write) -> io::Result<()> {
    let mut w = TreeWriter::new(sink);
    w.header()?;
    w.regular(false, contents.len() as u64, &mut &contents[..])?;
    w.flush()
}

/// `open` then `fstat`, not `lstat` then `open`: the same two syscalls, but
/// the kernel resolves the path once and the size and mode we announce
/// cannot disagree with the bytes that follow.
fn regular_file(w: &mut TreeWriter<'_>, path: &Path) -> io::Result<()> {
    let mut f = fs::File::open(path)?;
    let st = f.metadata()?;
    if !st.is_file() {
        return Err(changed_type(path));
    }
    w.regular(st.permissions().mode() & 0o100 != 0, st.len(), &mut f)
}

fn changed_type(path: &Path) -> io::Error {
    io::Error::other(format!(
        "file '{}' changed type while being read",
        path.display()
    ))
}

/// How a hashing dump reports each entry to its caller. `All` keeps every
/// entry and only observes; `Filter` decides. Only `All` may enumerate ahead
/// of the walk: a filter's answer is what says whether a directory may be
/// read at all, and a filter is called in NAR order on the calling thread,
/// so its answers cannot be known early.
pub enum Visit<'a> {
    All(&'a mut (dyn FnMut(&Path, FileType) + 'a)),
    Filter(&'a mut PathFilter<'a>),
}

/// The knobs of a hashing dump. The pool starts only once the walk has seen
/// `spawn_at` entries, so a small tree never pays for a thread.
#[derive(Clone, Copy)]
struct Knobs {
    readers: usize,
    spawn_at: usize,
    /// Entries the pool may hold in listings the walk has not reached.
    lookahead: usize,
    /// Directories the pool may hold waiting to be listed.
    queue_max: usize,
}

const KNOBS: Knobs = Knobs {
    readers: 8,
    spawn_at: 512,
    lookahead: 1 << 16,
    queue_max: 4096,
};
/// Items per batch, and the most contents bytes one batch holds: a file that
/// does not fit is opened by the writer instead.
const RUN_FILES: usize = 64;
const RUN_BYTES: usize = 1 << 20;
/// Batches in flight per pool thread.
const DEPTH: usize = 3;

/// Mirrors store-api.cc:hashPath for NixArchive ingestion, with a filter; returns (hash, NAR size).
pub fn nar_hash_filtered(
    path: &Path,
    algo: HashAlgo,
    filter: &mut PathFilter<'_>,
) -> io::Result<(Hash, u64)> {
    hash_tree(path, algo, Visit::Filter(filter), knobs())
}

/// `nar_hash_filtered` for a caller that keeps every entry: `see` is told
/// each one, in NAR order, and the enumeration runs ahead of the walk.
pub fn nar_hash_all(
    path: &Path,
    algo: HashAlgo,
    see: &mut dyn FnMut(&Path, FileType),
) -> io::Result<(Hash, u64)> {
    hash_tree(path, algo, Visit::All(see), knobs())
}

/// `nix hash path PATH`: sha256 of the NAR of `path`.
pub fn nar_hash(path: &Path) -> io::Result<Hash> {
    Ok(nar_hash_all(path, HashAlgo::Sha256, &mut |_, _| {})?.0)
}

/// `nix-store --dump PATH | wc -c`.
pub fn nar_size(path: &Path) -> io::Result<u64> {
    Ok(nar_hash_all(path, HashAlgo::Sha256, &mut |_, _| {})?.1)
}

fn knobs() -> Knobs {
    Knobs {
        readers: crate::session::Session::current().settings().nar_readers,
        ..KNOBS
    }
}

/// The NAR hash of `path` with the file system work off the hasher's thread.
///
/// The NAR is one serial byte stream, so the walk that orders it and the
/// hasher that consumes it are each one thread; what runs wide is the reading.
/// The walk (this thread, so the filter runs here) lists directories, calls
/// the filter and emits batches: the events the writer replays and the files
/// they name. A pool fills each batch with its files' bytes and, for `All`,
/// lists directories ahead of the walk. The writer thread replays the batches
/// in order into the hasher. Until the walk has seen `spawn_at` entries there
/// is no pool and no writer: each batch is replayed here as soon as it closes,
/// which is the serial dump with one indirection.
fn hash_tree(
    path: &Path,
    algo: HashAlgo,
    visit: Visit<'_>,
    knobs: Knobs,
) -> io::Result<(Hash, u64)> {
    let ft = FileType::of(fs::symlink_metadata(path)?.file_type());
    let mut root = path.as_os_str().as_bytes().to_vec();
    while root.len() > 1 && root.last() == Some(&b'/') {
        root.pop();
    }
    let h = Mutex::new(Hasher::new(algo));
    let mut lk = Locked(&h);
    let pool = Pool {
        st: Mutex::new(PoolState::default()),
        work: Condvar::new(),
        ready: Condvar::new(),
        filled: Condvar::new(),
        stop: AtomicBool::new(false),
        window: knobs.readers * DEPTH,
        knobs,
    };
    std::thread::scope(|s| {
        let mut p = Planner {
            s,
            pool: &pool,
            h: &h,
            w: TreeWriter::new(&mut lk),
            writer: None,
            visit,
            path: root,
            batch: Batch::default(),
            seen: 0,
            submitted: 0,
        };
        p.w.header()?;
        let walked = p.node(ft, 0);
        p.finish(walked)
    })?;
    let h = h.into_inner().unwrap();
    let size = h.bytes_written();
    Ok((h.finish(), size))
}

/// The hasher as the two writers of a dump see it: the walk's own until the
/// pool starts, the writer thread's after. A lock per flush is one per
/// buffer of the sink, not one per token.
struct Locked<'a>(&'a Mutex<Hasher>);

impl Write for Locked<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One directory's entries, sorted by name bytes, which is the order a NAR
/// demands (archive.cc:dumpPath sorts the same way).
struct Listing {
    names: Vec<u8>,
    /// `(offset into names, name length, type)`.
    ents: Vec<(u32, u32, FileType)>,
}

fn listing(dir: &Path) -> io::Result<Listing> {
    let mut l = Listing {
        names: Vec::new(),
        ents: Vec::new(),
    };
    for e in fs::read_dir(dir)? {
        let e = e?;
        let off = l.names.len();
        l.names.extend_from_slice(e.file_name().as_bytes());
        let ft = match e.file_type() {
            Ok(ft) => FileType::of(ft),
            // A file system whose `dirent` carries no `d_type`.
            Err(_) => FileType::of(fs::symlink_metadata(e.path())?.file_type()),
        };
        l.ents.push((off as u32, (l.names.len() - off) as u32, ft));
    }
    let Listing { names, ents } = &mut l;
    let key = |e: &(u32, u32, FileType)| &names[e.0 as usize..][..e.1 as usize];
    ents.sort_unstable_by(|a, b| key(a).cmp(key(b)));
    Ok(l)
}

enum Ev {
    Dir,
    EndDir,
    /// A name in the batch's bytes.
    Entry(u32, u32),
    EndEntry,
    /// The next of the batch's items.
    Item,
}

/// What the writer replays, and the files and symlinks the pool resolves for
/// it. Names and paths share one arena, so a batch is three allocations.
#[derive(Default)]
struct Batch {
    ev: Vec<Ev>,
    bytes: Vec<u8>,
    /// `(offset into bytes, path length, is a symlink)`.
    items: Vec<(u32, u32, bool)>,
}

/// A filled batch: one allocation for the contents, one slot per item.
struct Slab {
    bytes: Vec<u8>,
    got: Vec<Got>,
}

enum Got {
    /// `(offset, length, executable)` in the slab.
    Bytes(usize, usize, bool),
    Link(usize, usize),
    /// Too big for the slab; the writer opens it.
    Big,
}

fn fill(b: &Batch) -> io::Result<Slab> {
    let mut s = Slab {
        bytes: Vec::with_capacity(RUN_BYTES / 4),
        got: Vec::with_capacity(b.items.len()),
    };
    for &(o, n, link) in &b.items {
        let path = Path::new(OsStr::from_bytes(&b.bytes[o as usize..][..n as usize]));
        let off = s.bytes.len();
        if link {
            s.bytes
                .extend_from_slice(fs::read_link(path)?.as_os_str().as_bytes());
            s.got.push(Got::Link(off, s.bytes.len() - off));
            continue;
        }
        let f = fs::File::open(path)?;
        let st = f.metadata()?;
        if !st.is_file() {
            return Err(changed_type(path));
        }
        let len = st.len();
        if len > (RUN_BYTES - off) as u64 {
            s.got.push(Got::Big);
            continue;
        }
        s.bytes.reserve(len as usize);
        let got = (&f).take(len).read_to_end(&mut s.bytes)?;
        if got as u64 != len {
            return Err(io::Error::other("file ended early while reading contents"));
        }
        s.got
            .push(Got::Bytes(off, got, st.permissions().mode() & 0o100 != 0));
    }
    Ok(s)
}

/// Writes one batch. Without a slab every item is resolved here, which is
/// what the serial dump does.
fn replay(w: &mut TreeWriter<'_>, b: &Batch, slab: Option<&Slab>) -> io::Result<()> {
    let mut i = 0;
    for ev in &b.ev {
        match *ev {
            Ev::Dir => w.dir()?,
            Ev::EndDir => w.end_dir()?,
            Ev::Entry(o, n) => w.entry(&b.bytes[o as usize..][..n as usize])?,
            Ev::EndEntry => w.end_entry()?,
            Ev::Item => {
                let (o, n, link) = b.items[i];
                let path = Path::new(OsStr::from_bytes(&b.bytes[o as usize..][..n as usize]));
                match slab.map(|s| (&s.got[i], s)) {
                    Some((&Got::Bytes(off, len, exe), s)) => {
                        w.regular(exe, len as u64, &mut &s.bytes[off..off + len])?
                    }
                    Some((&Got::Link(off, len), s)) => w.symlink(&s.bytes[off..off + len])?,
                    _ if link => w.symlink(fs::read_link(path)?.as_os_str().as_bytes())?,
                    _ => regular_file(w, path)?,
                }
                i += 1;
            }
        }
    }
    Ok(())
}
/// What the pool threads, the walk and the writer share.
struct Pool {
    st: Mutex<PoolState>,
    /// Pool threads wait here for a batch or a directory.
    work: Condvar,
    /// The walk waits here for a listing or for the window.
    ready: Condvar,
    /// The writer waits here for its next batch.
    filled: Condvar,
    stop: AtomicBool,
    /// Batches the walk may run ahead of the writer.
    window: usize,
    knobs: Knobs,
}

#[derive(Default)]
struct PoolState {
    dirs: HashMap<Vec<u8>, Slot>,
    /// Directories to list ahead, last in first out with a listing's
    /// subdirectories pushed in reverse, so the pool reads in NAR order and
    /// what it holds is what the walk takes next.
    queue: Vec<(Vec<u8>, usize)>,
    /// Entries in `Done` listings.
    held: usize,
    /// Pool threads waiting on `work`: a wake-up is a syscall, so only as
    /// many are woken as there are new jobs.
    idle: usize,
    /// The walk is waiting on `ready` for a listing.
    wants_dir: bool,
    batches: VecDeque<(usize, Batch)>,
    filled: HashMap<usize, (Batch, io::Result<Slab>)>,
    /// The batch the writer wants next.
    next: usize,
    submitted: usize,
    done: bool,
}

enum Slot {
    Queued,
    Running,
    Done(io::Result<Listing>),
}

enum Job {
    Fill(usize, Batch),
    List(Vec<u8>, usize),
}

impl PoolState {
    /// Queues the subdirectories of a listing and returns how many.
    fn queue_subdirs(
        &mut self,
        dir: &[u8],
        l: &io::Result<Listing>,
        depth: usize,
        max: usize,
    ) -> usize {
        let Ok(l) = l else { return 0 };
        if depth + 1 >= NAR_MAX_DEPTH {
            return 0;
        }
        let room = max.saturating_sub(self.queue.len());
        let subs: Vec<&(u32, u32, FileType)> = l
            .ents
            .iter()
            .filter(|e| e.2 == FileType::Directory)
            .take(room)
            .collect();
        for &&(off, len, _) in subs.iter().rev() {
            let mut p = Vec::with_capacity(dir.len() + 1 + len as usize);
            p.extend_from_slice(dir);
            p.push(b'/');
            p.extend_from_slice(&l.names[off as usize..][..len as usize]);
            self.dirs.insert(p.clone(), Slot::Queued);
            self.queue.push((p, depth + 1));
        }
        subs.len()
    }
}

impl Pool {
    fn wake(&self, st: &PoolState, jobs: usize) {
        for _ in 0..jobs.min(st.idle) {
            self.work.notify_one();
        }
    }

    /// A pool thread. Filling comes before listing: the writer waits on a
    /// fill, and the walk never waits on a listing that has not started.
    fn serve(&self) {
        loop {
            let job = {
                let mut st = self.st.lock().unwrap();
                loop {
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some((k, b)) = st.batches.pop_front() {
                        break Job::Fill(k, b);
                    }
                    if st.held < self.knobs.lookahead
                        && let Some((dir, depth)) = st.queue.pop()
                    {
                        if let Some(slot @ Slot::Queued) = st.dirs.get_mut(&dir) {
                            *slot = Slot::Running;
                            break Job::List(dir, depth);
                        }
                        continue;
                    }
                    st.idle += 1;
                    st = self.work.wait(st).unwrap();
                    st.idle -= 1;
                }
            };
            match job {
                Job::Fill(k, b) => {
                    let slab = fill(&b);
                    self.st.lock().unwrap().filled.insert(k, (b, slab));
                    self.filled.notify_one();
                }
                Job::List(dir, depth) => {
                    let l = listing(Path::new(OsStr::from_bytes(&dir)));
                    let mut st = self.st.lock().unwrap();
                    st.held += l.as_ref().map_or(0, |l| l.ents.len());
                    let n = st.queue_subdirs(&dir, &l, depth, self.knobs.queue_max);
                    st.dirs.insert(dir, Slot::Done(l));
                    self.wake(&st, n);
                    if st.wants_dir {
                        self.ready.notify_all();
                    }
                }
            }
        }
    }

    /// The walk's own demand for a listing. It waits only on one a pool
    /// thread is in the middle of; anything the pool has not started it reads
    /// itself, so the lookahead cap can starve the pool but never the walk.
    fn take(&self, dir: &[u8], depth: usize) -> io::Result<Listing> {
        let mut st = self.st.lock().unwrap();
        loop {
            match st.dirs.get(dir) {
                Some(Slot::Done(_)) => {
                    let Some(Slot::Done(l)) = st.dirs.remove(dir) else {
                        unreachable!()
                    };
                    let starved = st.held >= self.knobs.lookahead;
                    st.held -= l.as_ref().map_or(0, |l| l.ents.len());
                    if starved {
                        self.wake(&st, 1);
                    }
                    return l;
                }
                Some(Slot::Running) => {
                    st.wants_dir = true;
                    st = self.ready.wait(st).unwrap();
                    st.wants_dir = false;
                }
                _ => {
                    st.dirs.remove(dir);
                    drop(st);
                    let l = listing(Path::new(OsStr::from_bytes(dir)));
                    let mut st = self.st.lock().unwrap();
                    let n = st.queue_subdirs(dir, &l, depth, self.knobs.queue_max);
                    self.wake(&st, n);
                    return l;
                }
            }
        }
    }

    fn submit(&self, k: usize, b: Batch) -> io::Result<()> {
        let mut st = self.st.lock().unwrap();
        while k >= st.next + self.window {
            if self.stop.load(Ordering::Relaxed) {
                return Err(io::Error::other("the NAR writer stopped"));
            }
            st = self.ready.wait(st).unwrap();
        }
        st.batches.push_back((k, b));
        st.submitted = k + 1;
        self.wake(&st, 1);
        Ok(())
    }

    /// The writer's next batch, or `None` once the walk is done and every
    /// batch is written, or on stop.
    fn next_filled(&self) -> Option<(Batch, io::Result<Slab>)> {
        let mut st = self.st.lock().unwrap();
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return None;
            }
            let k = st.next;
            if let Some(x) = st.filled.remove(&k) {
                st.next = k + 1;
                self.ready.notify_all();
                return Some(x);
            }
            if st.done && k >= st.submitted {
                return None;
            }
            st = self.filled.wait(st).unwrap();
        }
    }

    fn finish(&self) {
        self.st.lock().unwrap().done = true;
        self.filled.notify_all();
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.work.notify_all();
        self.ready.notify_all();
        self.filled.notify_all();
    }
}

/// The walk: NAR order, the filter, and the batches.
struct Planner<'scope, 'env, 'v> {
    s: &'scope Scope<'scope, 'env>,
    pool: &'env Pool,
    h: &'env Mutex<Hasher>,
    /// Used until the writer thread starts, then flushed and left.
    w: TreeWriter<'env>,
    writer: Option<ScopedJoinHandle<'scope, io::Result<()>>>,
    visit: Visit<'v>,
    /// See `Walk::path`.
    path: Vec<u8>,
    batch: Batch,
    seen: usize,
    submitted: usize,
}

impl Planner<'_, '_, '_> {
    fn node(&mut self, ft: FileType, depth: usize) -> io::Result<()> {
        if depth >= NAR_MAX_DEPTH {
            return Err(io::Error::other(format!(
                "path '{}' exceeds maximum NAR directory depth of {NAR_MAX_DEPTH}",
                Path::new(OsStr::from_bytes(&self.path)).display()
            )));
        }
        match ft {
            FileType::Regular => self.item(false),
            FileType::Symlink => self.item(true),
            FileType::Directory => self.directory(depth),
            FileType::Unknown => Err(io::Error::other(format!(
                "file '{}' has an unsupported type",
                Path::new(OsStr::from_bytes(&self.path)).display()
            ))),
        }
    }

    fn item(&mut self, link: bool) -> io::Result<()> {
        let off = self.batch.bytes.len();
        self.batch.bytes.extend_from_slice(&self.path);
        self.batch
            .items
            .push((off as u32, self.path.len() as u32, link));
        self.batch.ev.push(Ev::Item);
        if self.writer.is_none() {
            replay(&mut self.w, &self.batch, None)?;
            self.batch.ev.clear();
            self.batch.bytes.clear();
            self.batch.items.clear();
        } else if self.batch.items.len() >= RUN_FILES {
            self.submit()?;
        }
        Ok(())
    }

    fn submit(&mut self) -> io::Result<()> {
        let b = std::mem::take(&mut self.batch);
        self.pool.submit(self.submitted, b)?;
        self.submitted += 1;
        Ok(())
    }

    fn directory(&mut self, depth: usize) -> io::Result<()> {
        if self.pool.stop.load(Ordering::Relaxed) {
            return Err(io::Error::other("the NAR writer stopped"));
        }
        let l = match (&self.writer, &self.visit) {
            (Some(_), Visit::All(_)) => self.pool.take(&self.path, depth)?,
            _ => listing(Path::new(OsStr::from_bytes(&self.path)))?,
        };
        self.seen += l.ents.len();
        if self.writer.is_none()
            && self.pool.knobs.readers > 0
            && self.seen >= self.pool.knobs.spawn_at
        {
            self.spawn()?;
        }
        self.batch.ev.push(Ev::Dir);
        let base = self.path.len();
        for &(off, len, ft) in &l.ents {
            let name = &l.names[off as usize..][..len as usize];
            self.path.push(b'/');
            self.path.extend_from_slice(name);
            let cur = Path::new(OsStr::from_bytes(&self.path));
            let keep = match &mut self.visit {
                Visit::All(see) => {
                    see(cur, ft);
                    true
                }
                Visit::Filter(f) => f(cur, ft),
            };
            if keep {
                let o = self.batch.bytes.len();
                self.batch.bytes.extend_from_slice(name);
                self.batch.ev.push(Ev::Entry(o as u32, len));
                self.node(ft, depth + 1)?;
                self.batch.ev.push(Ev::EndEntry);
            }
            self.path.truncate(base);
        }
        self.batch.ev.push(Ev::EndDir);
        Ok(())
    }

    fn spawn(&mut self) -> io::Result<()> {
        self.w.flush()?;
        let pool = self.pool;
        for _ in 0..pool.knobs.readers {
            self.s.spawn(move || pool.serve());
        }
        let h = self.h;
        self.writer = Some(self.s.spawn(move || {
            let mut lk = Locked(h);
            let mut w = TreeWriter::new(&mut lk);
            let r = (|| {
                while let Some((b, slab)) = pool.next_filled() {
                    replay(&mut w, &b, Some(&slab?))?;
                }
                w.flush()
            })();
            if r.is_err() {
                pool.stop();
            }
            r
        }));
        Ok(())
    }

    /// Closes the last batch, lets the writer drain, and stops the pool. The
    /// writer's error wins: a stop it caused is what the walk saw.
    fn finish(mut self, walked: io::Result<()>) -> io::Result<()> {
        let Some(writer) = self.writer.take() else {
            walked?;
            replay(&mut self.w, &self.batch, None)?;
            return self.w.flush();
        };
        let r = walked.and_then(|()| match self.batch.ev.is_empty() {
            true => Ok(()),
            false => self.submit(),
        });
        if r.is_err() {
            self.pool.stop();
        }
        self.pool.finish();
        let wrote = writer
            .join()
            .map_err(|_| io::Error::other("the NAR writer panicked"))?;
        self.pool.stop();
        wrote.and(r)
    }
}

/// Mirrors store-api.cc:hashPath for Flat ingestion: hash of the file bytes.
pub fn flat_hash(path: &Path, algo: HashAlgo) -> io::Result<Hash> {
    let mut h = Hasher::new(algo);
    let mut f = fs::File::open(path)?;
    let cap = f.metadata().map_or(BUF as u64, |m| m.len());
    let mut buf = vec![0u8; (cap.max(MIN_READ as u64).min(BUF as u64)) as usize];
    loop {
        let got = f.read(&mut buf)?;
        if got == 0 {
            return Ok(h.finish());
        }
        h.update(&buf[..got]);
    }
}

fn bad_nar(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("bad NAR: {why}"))
}

/// A NAR stream read through one refillable window.
///
/// The shape is the one nix-community/go-nix PR #123 (pkg/narv2) settled on: a
/// synchronous state machine over a buffered reader with peek and discard, whole
/// token groups matched with one compare against a pre-framed constant, the node
/// shape picked from a single byte of a peeked header, contents streamed by their
/// declared length, and nothing allocated per token.
struct Reader<'a> {
    src: &'a mut dyn Read,
    buf: Box<[u8]>,
    /// The filled region of `buf`.
    start: usize,
    end: usize,
    eof: bool,
    /// How much of the stream has been consumed: the NAR's length once done.
    consumed: u64,
}

impl<'a> Reader<'a> {
    fn new(src: &'a mut dyn Read) -> Reader<'a> {
        Reader {
            src,
            buf: vec![0u8; BUF].into_boxed_slice(),
            start: 0,
            end: 0,
            eof: false,
            consumed: 0,
        }
    }

    /// Reads more of the source, sliding what is left of the window to the front.
    fn refill(&mut self) -> io::Result<()> {
        if self.start > 0 {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        if self.end == self.buf.len() {
            return Ok(());
        }
        let got = self.src.read(&mut self.buf[self.end..])?;
        self.eof = got == 0;
        self.end += got;
        Ok(())
    }

    /// Makes the window hold at least `n` bytes. `n` must fit the buffer.
    fn want(&mut self, n: usize) -> io::Result<()> {
        while self.end - self.start < n {
            self.refill()?;
            if self.eof && self.end - self.start < n {
                return Err(bad_nar("stream ended early"));
            }
        }
        Ok(())
    }

    /// Fills the window to `n` bytes if the source has them; returns how many it
    /// holds. Unlike `want` it does not fail at the end of the stream.
    fn avail(&mut self, n: usize) -> io::Result<usize> {
        while self.end - self.start < n {
            self.refill()?;
            if self.eof {
                break;
            }
        }
        Ok((self.end - self.start).min(n))
    }

    fn advance(&mut self, n: usize) {
        self.start += n;
        self.consumed += n as u64;
    }

    /// `n` bytes of the stream without consuming them.
    fn peek(&mut self, n: usize) -> io::Result<&[u8]> {
        self.want(n)?;
        Ok(&self.buf[self.start..self.start + n])
    }

    /// Consumes a run of constant tokens, which the stream must hold exactly.
    fn consume<const N: usize>(&mut self, run: &[u8; N], what: &str) -> io::Result<()> {
        if self.avail(N)? < N || self.buf[self.start..self.start + N] != *run {
            return Err(bad_nar(&format!("expected {what}")));
        }
        self.advance(N);
        Ok(())
    }

    fn u64(&mut self) -> io::Result<u64> {
        self.want(8)?;
        let n = u64::from_le_bytes(self.buf[self.start..self.start + 8].try_into().unwrap());
        self.advance(8);
        Ok(n)
    }

    /// Up to `n` bytes of contents, however many the window holds.
    fn some(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.start == self.end {
            self.refill()?;
            if self.eof {
                return Err(bad_nar("stream ended early"));
            }
        }
        let take = n.min(self.end - self.start);
        self.advance(take);
        Ok(&self.buf[self.start - take..self.start])
    }

    /// Throws away `n` bytes: what a parse that wants only the framing does with
    /// a file's body.
    fn skip(&mut self, n: u64) -> io::Result<()> {
        let mut left = n;
        while left > 0 {
            left -= self.some(left.min(BUF as u64) as usize)?.len() as u64;
        }
        Ok(())
    }

    fn padding(&mut self, len: u64) -> io::Result<()> {
        let rem = (len % 8) as usize;
        if rem != 0 {
            self.want(8 - rem)?;
            if self.buf[self.start..self.start + 8 - rem]
                .iter()
                .any(|&b| b != 0)
            {
                return Err(bad_nar("non-zero padding"));
            }
            self.advance(8 - rem);
        }
        Ok(())
    }

    /// Mirrors serialise.hh:readString with a size cap. The token is a slice of
    /// the window, so it lives only until the next read.
    fn token(&mut self, max: usize) -> io::Result<&[u8]> {
        let len = self.u64()?;
        if len > max as u64 {
            return Err(bad_nar("string too long"));
        }
        let len = len as usize;
        // Token and padding are checked together so the borrow of the window
        // outlives neither.
        let need = len.next_multiple_of(8);
        self.want(need)?;
        let s = self.start;
        if self.buf[s + len..s + need].iter().any(|&b| b != 0) {
            return Err(bad_nar("non-zero padding"));
        }
        self.advance(need);
        Ok(&self.buf[s..s + len])
    }

    /// Which of the four node shapes comes next, from one byte of its header.
    fn node_tag(&mut self) -> io::Result<u8> {
        let tag = self.peek(32)?[16];
        match tag {
            TAG_SYM | TAG_REG | TAG_EXE | TAG_DIR => Ok(tag),
            _ => Err(bad_nar("unknown node type")),
        }
    }

    /// Copies `len` bytes of contents into `out`, then the padding.
    fn contents_to(&mut self, out: &mut fs::File, len: u64) -> io::Result<()> {
        let mut left = len;
        while left > 0 {
            let chunk = self.some(left.min(BUF as u64) as usize)?;
            out.write_all(chunk)?;
            left -= chunk.len() as u64;
        }
        self.padding(len)
    }
}

/// Mirrors archive.cc:parseDump with RestoreSink: restores the NAR read from `source` at
/// `dest`, which must not exist. Byte-exact inverse of `dump_path`.
pub fn restore_path(source: &mut dyn Read, dest: &Path) -> io::Result<()> {
    let mut r = Restorer {
        r: Reader::new(source),
        path: dest.as_os_str().as_bytes().to_vec(),
        name: Vec::with_capacity(MAX_NAME),
        prev: Vec::new(),
    };
    r.r.consume(&TOK_NAR, "a NAR header")?;
    r.node(0)
}

/// The scratch a restore reuses: one path buffer, one entry-name buffer, and the
/// previous name at each depth, which the format requires to be increasing.
struct Restorer<'a> {
    r: Reader<'a>,
    /// See `Walk::path`: truncation instead of `PathBuf::pop`.
    path: Vec<u8>,
    name: Vec<u8>,
    prev: Vec<Vec<u8>>,
}

impl Restorer<'_> {
    fn cur(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.path))
    }

    fn node(&mut self, depth: usize) -> io::Result<()> {
        if depth >= NAR_MAX_DEPTH {
            return Err(bad_nar("directory nesting too deep"));
        }
        match self.r.node_tag()? {
            TAG_REG => {
                self.r.consume(&TOK_REG, "'regular' 'contents'")?;
                self.regular(false)?;
            }
            TAG_EXE => {
                self.r
                    .consume(&TOK_EXE, "'regular' 'executable' 'contents'")?;
                self.regular(true)?;
            }
            TAG_SYM => {
                self.r.consume(&TOK_SYM, "'symlink' 'target'")?;
                take(&mut self.r, MAX_TARGET, &mut self.name)?;
                std::os::unix::fs::symlink(OsStr::from_bytes(&self.name), self.cur())?;
            }
            // The `)` that ends a directory's entry list is the node's own, so
            // `directory` consumes it and this one must not look for a second.
            _ => {
                self.r.consume(&TOK_DIR, "'directory'")?;
                return self.directory(depth);
            }
        }
        self.r.consume(&TOK_PAR, "')'")
    }

    fn regular(&mut self, executable: bool) -> io::Result<()> {
        let len = self.r.u64()?;
        // exclusive: a symlink that is already there must never be written
        // through, and a NAR never names one entry twice
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.cur())?;
        self.r.contents_to(&mut f, len)?;
        if executable {
            f.set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    fn directory(&mut self, depth: usize) -> io::Result<()> {
        fs::create_dir(self.cur())?;
        if self.prev.len() <= depth {
            self.prev.resize(depth + 1, Vec::new());
        }
        self.prev[depth].clear();
        let base = self.path.len();
        loop {
            // 1 is the length prefix of ")", 5 that of "entry".
            match self.r.peek(16)?[0] {
                1 => {
                    self.r.consume(&TOK_PAR, "')'")?;
                    return Ok(());
                }
                5 => {}
                _ => return Err(bad_nar("expected 'entry' or ')'")),
            }
            self.r.consume(&TOK_ENT, "'entry' '(' 'name'")?;
            take(&mut self.r, MAX_NAME, &mut self.name)?;
            let name = &self.name;
            if name.is_empty()
                || name.as_slice() == b"."
                || name.as_slice() == b".."
                || name.contains(&b'/')
                || name.contains(&0)
            {
                return Err(bad_nar("bad entry name"));
            }
            if *name <= self.prev[depth] {
                return Err(bad_nar("directory entries out of order"));
            }
            self.prev[depth].clear();
            self.prev[depth].extend_from_slice(name);
            self.r.consume(&TOK_NOD, "'node' '(' 'type'")?;
            self.path.push(b'/');
            self.path.extend_from_slice(&self.prev[depth]);
            let res = self.node(depth + 1);
            self.path.truncate(base);
            res?;
            self.r.consume(&TOK_PAR, "')'")?;
        }
    }
}

/// Parses the framing of the NAR in `source`, throwing the file contents away,
/// and returns its length in bytes.
///
/// This is `restore_path` with no file system on the other end: it says whether
/// a NAR is well formed and how long it is without unpacking it, and it prices
/// the parser on its own.
pub fn parse_length(source: &mut dyn Read) -> io::Result<u64> {
    let mut r = Reader::new(source);
    r.consume(&TOK_NAR, "a NAR header")?;
    let mut name = Vec::with_capacity(MAX_NAME);
    parse_node(&mut r, &mut name, 0)?;
    Ok(r.consumed)
}

fn parse_node(r: &mut Reader<'_>, name: &mut Vec<u8>, depth: usize) -> io::Result<()> {
    if depth >= NAR_MAX_DEPTH {
        return Err(bad_nar("directory nesting too deep"));
    }
    match r.node_tag()? {
        tag @ (TAG_REG | TAG_EXE) => {
            if tag == TAG_EXE {
                r.consume(&TOK_EXE, "'regular' 'executable' 'contents'")?;
            } else {
                r.consume(&TOK_REG, "'regular' 'contents'")?;
            }
            let len = r.u64()?;
            r.skip(len)?;
            r.padding(len)?;
        }
        TAG_SYM => {
            r.consume(&TOK_SYM, "'symlink' 'target'")?;
            r.token(MAX_TARGET)?;
        }
        _ => {
            r.consume(&TOK_DIR, "'directory'")?;
            loop {
                match r.peek(16)?[0] {
                    1 => return r.consume(&TOK_PAR, "')'"),
                    5 => {}
                    _ => return Err(bad_nar("expected 'entry' or ')'")),
                }
                r.consume(&TOK_ENT, "'entry' '(' 'name'")?;
                take(r, MAX_NAME, name)?;
                r.consume(&TOK_NOD, "'node' '(' 'type'")?;
                // Nothing here keeps the name past this point, so one buffer
                // serves the whole walk.
                parse_node(r, name, depth + 1)?;
                r.consume(&TOK_PAR, "')'")?;
            }
        }
    }
    r.consume(&TOK_PAR, "')'")
}

/// Reads one token into `out`, which the caller owns past the next read.
fn take(r: &mut Reader<'_>, max: usize, out: &mut Vec<u8>) -> io::Result<()> {
    let t = r.token(max)?;
    out.clear();
    out.extend_from_slice(t);
    Ok(())
}

#[cfg(test)]
/// Hashes `contents` as a NAR regular file (what `nix hash path` gives for that file).
pub fn nar_hash_of_string(contents: &[u8]) -> Hash {
    let mut h = Hasher::new(HashAlgo::Sha256);
    dump_string(contents, &mut h).expect("hasher never fails");
    h.finish()
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    /// Builds the fixture tree used by the shell commands in the test comments:
    /// mkdir -p tree/sub; printf 'hello world\n' > tree/a.txt;
    /// printf '#!/bin/sh\necho hi\n' > tree/run.sh; chmod +x tree/run.sh;
    /// ln -s a.txt tree/link; : > tree/sub/empty; printf zzz > tree/sub/z
    pub(crate) fn fixture_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("iets-nar-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let tree = dir.join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("a.txt"), b"hello world\n").unwrap();
        fs::write(tree.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(tree.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("a.txt", tree.join("link")).unwrap();
        fs::write(tree.join("sub/empty"), b"").unwrap();
        fs::write(tree.join("sub/z"), b"zzz").unwrap();
        tree
    }

    // nix-store --dump tree | sha256sum ; nix-store --dump tree | wc -c
    #[test]
    fn directory_nar() {
        let tree = fixture_tree("dir");
        assert_eq!(
            nar_hash(&tree).unwrap().to_base16(),
            "9db4d4d873838f1fbb8f363841597d61244beebb19bd10b606ae2a6d4b940ede"
        );
        assert_eq!(nar_size(&tree).unwrap(), 1272);
    }

    // nix hash path tree ; nix hash path --base32 tree
    #[test]
    fn directory_nar_hash_formats() {
        let tree = fixture_tree("fmt");
        let h = nar_hash(&tree).unwrap();
        assert_eq!(
            h.to_sri(),
            "sha256-nbTU2HODjx+7jzY4QVl9YSRL7rsZvRC2Bq4qbUuUDt4="
        );
        assert_eq!(
            h.to_nix32(),
            "1phfji5nsamf0sv11g8rpgp4n931gmcl2f1niyxiz3w3fgcd9d4x"
        );
    }

    // nix-store --dump tree/a.txt | sha256sum
    #[test]
    fn regular_file_nar() {
        let tree = fixture_tree("file");
        let expected = "34ca3ac63094d1d5751f741101692a78f95eedf10744b088129fc324dfd0f603";
        assert_eq!(nar_hash(&tree.join("a.txt")).unwrap().to_base16(), expected);
        assert_eq!(nar_hash_of_string(b"hello world\n").to_base16(), expected);
    }

    // nix-store --dump tree/run.sh | sha256sum
    #[test]
    fn executable_file_nar() {
        let tree = fixture_tree("exec");
        assert_eq!(
            nar_hash(&tree.join("run.sh")).unwrap().to_base16(),
            "5e0accf02cedede5e4119ffa15e79e79a5fb1fb9bc43c3d434f33227a14477a0"
        );
    }

    // nix-store --dump tree/link | sha256sum
    #[test]
    fn symlink_nar() {
        let tree = fixture_tree("link");
        assert_eq!(
            nar_hash(&tree.join("link")).unwrap().to_base16(),
            "8d3c00cfa866e4d1b809772afeac240786246221eb2c574d69c4bba168834e81"
        );
    }

    // nix-store --dump tree | nix-store --restore out ; nix-store --dump out | sha256sum
    #[test]
    fn restore_roundtrip() {
        let tree = fixture_tree("restore");
        let mut nar = Vec::new();
        dump_path(&tree, &mut nar, &mut |_, _| true).unwrap();
        let out = tree.with_file_name("restored");
        restore_path(&mut nar.as_slice(), &out).unwrap();
        assert_eq!(nar_hash(&out).unwrap(), nar_hash(&tree).unwrap());
        assert_eq!(
            fs::read_link(out.join("link")).unwrap().as_os_str(),
            "a.txt"
        );
        assert_ne!(
            fs::metadata(out.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0
        );
        assert_eq!(
            fs::metadata(out.join("a.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0
        );
    }

    #[test]
    fn restore_rejects_garbage() {
        let out = std::env::temp_dir().join(format!("iets-nar-garbage-{}", std::process::id()));
        assert!(restore_path(&mut &b"nix-archive-2"[..], &out).is_err());
    }

    #[test]
    fn filter_sees_types_and_paths() {
        let tree = fixture_tree("filter");
        let mut seen = Vec::new();
        let (_, _) = nar_hash_filtered(&tree, HashAlgo::Sha256, &mut |p, t| {
            seen.push((p.strip_prefix(&tree).unwrap().to_path_buf(), t));
            true
        })
        .unwrap();
        let expect: Vec<(PathBuf, FileType)> = vec![
            ("a.txt".into(), FileType::Regular),
            ("link".into(), FileType::Symlink),
            ("run.sh".into(), FileType::Regular),
            ("sub".into(), FileType::Directory),
            ("sub/empty".into(), FileType::Regular),
            ("sub/z".into(), FileType::Regular),
        ];
        assert_eq!(seen, expect);
    }

    /// The serial walk, which every pipelined dump must reproduce byte for byte.
    fn serial(path: &Path, filter: &mut PathFilter<'_>) -> io::Result<(Hash, u64)> {
        let mut h = Hasher::new(HashAlgo::Sha256);
        dump_path(path, &mut h, filter)?;
        let size = h.bytes_written();
        Ok((h.finish(), size))
    }

    fn piped(readers: usize) -> Knobs {
        Knobs {
            readers,
            spawn_at: 0,
            ..KNOBS
        }
    }

    /// An empty directory, a zero-length file, an executable, a file too big
    /// for a slab, a dangling symlink, deep nesting, names that only sort right
    /// by bytes (`a` beside `a-b` and `a.txt`), more files than one batch holds
    /// and more directories than one lookahead cap in `lookahead_cap_cannot_deadlock`.
    fn awkward_tree(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("iets-nar-awkward-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let t = dir.join("tree");
        fs::create_dir_all(t.join("empty")).unwrap();
        fs::write(t.join("zero"), b"").unwrap();
        fs::write(t.join("exe"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(t.join("exe"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(t.join("big"), vec![7u8; RUN_BYTES + 1]).unwrap();
        symlink("nowhere", t.join("dangling")).unwrap();
        let mut deep = t.join("deep");
        for i in 0..30 {
            deep.push(format!("d{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("leaf"), b"leaf").unwrap();
        fs::create_dir(t.join("a")).unwrap();
        fs::write(t.join("a/x"), b"x").unwrap();
        fs::write(t.join("a.txt"), b"a").unwrap();
        fs::write(t.join("a-b"), b"ab").unwrap();
        fs::create_dir(t.join("many")).unwrap();
        for i in 0..150 {
            fs::write(t.join(format!("many/f{i:03}")), format!("file {i}")).unwrap();
        }
        fs::create_dir(t.join("dirs")).unwrap();
        for i in 0..300 {
            let d = t.join(format!("dirs/d{i:03}"));
            fs::create_dir(&d).unwrap();
            fs::write(d.join("f"), i.to_string()).unwrap();
            if i % 3 == 0 {
                fs::create_dir(d.join("sub")).unwrap();
                fs::write(d.join("sub/g"), b"g").unwrap();
                symlink("../f", d.join("sub/l")).unwrap();
            }
        }
        t
    }

    #[test]
    fn pipelined_hash_matches_serial_on_awkward_shapes() {
        let tree = awkward_tree("shapes");
        for root in [
            tree.clone(),
            tree.join("big"),
            tree.join("dangling"),
            tree.join("exe"),
            tree.join("empty"),
        ] {
            let want = serial(&root, &mut |_, _| true).unwrap();
            assert_eq!(
                hash_tree(
                    &root,
                    HashAlgo::Sha256,
                    Visit::All(&mut |_, _| {}),
                    piped(4)
                )
                .unwrap(),
                want,
                "{}",
                root.display()
            );
            assert_eq!(
                hash_tree(
                    &root,
                    HashAlgo::Sha256,
                    Visit::Filter(&mut |_, _| true),
                    piped(4)
                )
                .unwrap(),
                want,
                "{}",
                root.display()
            );
            assert_eq!(
                hash_tree(
                    &root,
                    HashAlgo::Sha256,
                    Visit::All(&mut |_, _| {}),
                    Knobs {
                        readers: 0,
                        ..KNOBS
                    }
                )
                .unwrap(),
                want,
                "{}",
                root.display()
            );
        }
    }

    #[test]
    fn pipelined_filter_sees_the_serial_sequence() {
        let tree = awkward_tree("sequence");
        let decide =
            |p: &Path| !p.to_string_lossy().ends_with('1') && p.file_name().unwrap() != "deep";
        let mut want = Vec::new();
        let (wh, ws) = serial(&tree, &mut |p, t| {
            let keep = decide(p);
            want.push((p.to_path_buf(), t, keep));
            keep
        })
        .unwrap();
        let mut got = Vec::new();
        let (gh, gs) = hash_tree(
            &tree,
            HashAlgo::Sha256,
            Visit::Filter(&mut |p, t| {
                let keep = decide(p);
                got.push((p.to_path_buf(), t, keep));
                keep
            }),
            piped(3),
        )
        .unwrap();
        assert_eq!(got, want);
        assert_eq!((gh, gs), (wh, ws));

        let mut want = Vec::new();
        serial(&tree, &mut |p, t| {
            want.push((p.to_path_buf(), t));
            true
        })
        .unwrap();
        let mut got = Vec::new();
        hash_tree(
            &tree,
            HashAlgo::Sha256,
            Visit::All(&mut |p, t| got.push((p.to_path_buf(), t))),
            piped(3),
        )
        .unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn lookahead_cap_cannot_deadlock() {
        let tree = awkward_tree("cap");
        let want = serial(&tree, &mut |_, _| true).unwrap();
        let tight = Knobs {
            readers: 2,
            spawn_at: 0,
            lookahead: 4,
            queue_max: 2,
        };
        assert_eq!(
            hash_tree(&tree, HashAlgo::Sha256, Visit::All(&mut |_, _| {}), tight).unwrap(),
            want
        );
    }

    #[test]
    fn rejected_directory_is_never_read() {
        let tree = awkward_tree("locked");
        let locked = tree.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("secret"), b"s").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&locked).is_ok() {
            return;
        }
        let mut reject = |p: &Path, _: FileType| p != locked;
        let want = serial(&tree, &mut reject).unwrap();
        let got = hash_tree(
            &tree,
            HashAlgo::Sha256,
            Visit::Filter(&mut reject),
            piped(4),
        );
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(got.unwrap(), want);
    }

    #[test]
    fn too_deep_fails_in_both_modes() {
        let mut deep = std::env::temp_dir().join(format!("iets-nar-deep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&deep);
        let root = deep.clone();
        for i in 0..NAR_MAX_DEPTH + 2 {
            deep.push(format!("d{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        let why = |r: io::Result<(Hash, u64)>| r.unwrap_err().to_string();
        let want = why(serial(&root, &mut |_, _| true));
        assert!(
            want.contains("exceeds maximum NAR directory depth"),
            "{want}"
        );
        let got = why(hash_tree(
            &root,
            HashAlgo::Sha256,
            Visit::All(&mut |_, _| {}),
            piped(4),
        ));
        assert_eq!(got, want);
    }
}
