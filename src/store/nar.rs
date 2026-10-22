use crate::sys::OsStrExt;
use crate::sys::fs::{is_executable, make_executable, symlink};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

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
/// A tree that is walked — a directory on disk, an indexed archive, a string —
/// drives it with the nodes it finds, in NAR order, and keeps its own
/// traversal. The framing, the buffering and the content copy are this type's.
/// `regular` and `symlink` close their node, `dir` opens one and `end_dir`
/// closes it, and `entry`/`end_entry` wrap the child between them.
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

    /// `open` then `fstat`, not `lstat` then `open`: the same two syscalls, but
    /// the kernel resolves the path once and the size and mode we announce
    /// cannot disagree with the bytes that follow.
    fn regular(&mut self) -> io::Result<()> {
        let mut f = fs::File::open(self.cur())?;
        let st = f.metadata()?;
        if !st.is_file() {
            return Err(io::Error::other(format!(
                "file '{}' changed type while being read",
                self.cur().display()
            )));
        }
        let executable = is_executable(&st);
        let len = st.len();
        self.w.regular(executable, len, &mut f)
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

/// Mirrors store-api.cc:hashPath for NixArchive ingestion, with a filter; returns (hash, NAR size).
pub fn nar_hash_filtered(
    path: &Path,
    algo: HashAlgo,
    filter: &mut PathFilter<'_>,
) -> io::Result<(Hash, u64)> {
    let mut h = Hasher::new(algo);
    dump_path(path, &mut h, filter)?;
    let size = h.bytes_written();
    Ok((h.finish(), size))
}

/// `nix hash path PATH`: sha256 of the NAR of `path`.
pub fn nar_hash(path: &Path) -> io::Result<Hash> {
    Ok(nar_hash_filtered(path, HashAlgo::Sha256, &mut |_, _| true)?.0)
}

/// `nix-store --dump PATH | wc -c`.
pub fn nar_size(path: &Path) -> io::Result<u64> {
    Ok(nar_hash_filtered(path, HashAlgo::Sha256, &mut |_, _| true)?.1)
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
                check_target(&self.name)?;
                symlink(OsStr::from_bytes(&self.name), self.cur())?;
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
            make_executable(&f)?;
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
            next_name(&self.name, &mut self.prev[depth])?;
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
    parse_node(&mut r, &mut name, &mut Vec::new(), 0)?;
    Ok(r.consumed)
}

/// `prev[d]` is the last entry name read at depth `d`.
fn parse_node(
    r: &mut Reader<'_>,
    name: &mut Vec<u8>,
    prev: &mut Vec<Vec<u8>>,
    depth: usize,
) -> io::Result<()> {
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
            check_target(r.token(MAX_TARGET)?)?;
        }
        _ => {
            r.consume(&TOK_DIR, "'directory'")?;
            if prev.len() <= depth {
                prev.resize(depth + 1, Vec::new());
            }
            prev[depth].clear();
            loop {
                match r.peek(16)?[0] {
                    1 => return r.consume(&TOK_PAR, "')'"),
                    5 => {}
                    _ => return Err(bad_nar("expected 'entry' or ')'")),
                }
                r.consume(&TOK_ENT, "'entry' '(' 'name'")?;
                take(r, MAX_NAME, name)?;
                next_name(name, &mut prev[depth])?;
                r.consume(&TOK_NOD, "'node' '(' 'type'")?;
                // Nothing here keeps the name past this point, so one buffer
                // serves the whole walk.
                parse_node(r, name, prev, depth + 1)?;
                r.consume(&TOK_PAR, "')'")?;
            }
        }
    }
    r.consume(&TOK_PAR, "')'")
}

/// archive.cc:parse's checks of an entry name: well formed, and after the
/// previous name of its directory, which it then replaces.
fn next_name(name: &[u8], prev: &mut Vec<u8>) -> io::Result<()> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(bad_nar("bad entry name"));
    }
    if name <= prev.as_slice() {
        return Err(bad_nar("directory entries out of order"));
    }
    prev.clear();
    prev.extend_from_slice(name);
    Ok(())
}

/// archive.cc:parse's check of a symlink target.
fn check_target(target: &[u8]) -> io::Result<()> {
    if target.is_empty() || target.contains(&0) {
        return Err(bad_nar("bad symlink target"));
    }
    Ok(())
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
    use crate::sys;
    use crate::sys::fs::set_executable;
    use rstest::rstest;
    use std::env;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// Builds the fixture tree used by the shell commands in the test comments:
    /// mkdir -p tree/sub; printf 'hello world\n' > tree/a.txt;
    /// printf '#!/bin/sh\necho hi\n' > tree/run.sh; chmod +x tree/run.sh;
    /// ln -s a.txt tree/link; : > tree/sub/empty; printf zzz > tree/sub/z
    pub(crate) fn fixture_tree(tag: &str) -> PathBuf {
        fixture_tree_with(tag, true)
    }

    /// `exec` is whether `run.sh` has the executable bit, which Windows cannot set.
    fn fixture_tree_with(tag: &str, exec: bool) -> PathBuf {
        let dir = sys::temp_dir().join(format!("iets-nar-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let tree = dir.join("tree");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("a.txt"), b"hello world\n").unwrap();
        fs::write(tree.join("run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
        set_executable(&tree.join("run.sh"), exec).unwrap();
        symlink("a.txt", tree.join("link")).unwrap();
        fs::write(tree.join("sub/empty"), b"").unwrap();
        fs::write(tree.join("sub/z"), b"zzz").unwrap();
        tree
    }

    // nix-store --dump tree | sha256sum ; nix-store --dump tree | wc -c
    #[test]
    fn directory_nar() {
        let tree = fixture_tree_with("dir", false);
        assert_eq!(
            nar_hash(&tree).unwrap().to_base16(),
            "5bd341428326a5608117fb128d3114fc3fb6c76de99cd19c10397bdad728beae"
        );
        assert_eq!(nar_size(&tree).unwrap(), 1240);
    }

    // the same with `chmod +x tree/run.sh`
    #[cfg(unix)]
    #[test]
    fn directory_nar_with_an_executable() {
        let tree = fixture_tree("dir-x");
        assert_eq!(
            nar_hash(&tree).unwrap().to_base16(),
            "9db4d4d873838f1fbb8f363841597d61244beebb19bd10b606ae2a6d4b940ede"
        );
        assert_eq!(nar_size(&tree).unwrap(), 1272);
    }

    // nix hash path tree ; nix hash path --base32 tree
    #[test]
    fn directory_nar_hash_formats() {
        let tree = fixture_tree_with("fmt", false);
        let h = nar_hash(&tree).unwrap();
        assert_eq!(
            h.to_sri(),
            "sha256-W9NBQoMmpWCBF/sSjTEU/D+2x23pnNGcEDl72tcovq4="
        );
        assert_eq!(
            h.to_nix32(),
            "1bmy53bxlyrr22fd3779dp3vcgzw2hqqs4pv2y0n1996hd143lsv"
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
    fn script_nar() {
        let tree = fixture_tree_with("script", false);
        assert_eq!(
            nar_hash(&tree.join("run.sh")).unwrap().to_base16(),
            "e519505edb9f77f7f02efefd3c9b29766fdd0f449e9bf932313d39ce249982e9"
        );
    }

    // the same after `chmod +x tree/run.sh`
    #[cfg(unix)]
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

    /// A NAR of one directory holding `names`, each a one-byte file, or of a
    /// symlink to `target` when `names` is empty.
    fn crafted(names: &[&[u8]], target: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = TreeWriter::new(&mut out);
        w.header().unwrap();
        if names.is_empty() {
            w.symlink(target).unwrap();
        } else {
            w.dir().unwrap();
            for n in names {
                w.entry(n).unwrap();
                w.regular(false, 1, &mut &b"x"[..]).unwrap();
                w.end_entry().unwrap();
            }
            w.end_dir().unwrap();
        }
        w.flush().unwrap();
        drop(w);
        out
    }

    // archive.cc:parse refuses each of these; found by fuzz/ (nar-read)
    #[rstest]
    #[case::unsorted(crafted(&[b"b", b"a"], b""))]
    #[case::duplicate(crafted(&[b"a", b"a"], b""))]
    #[case::dot_dot(crafted(&[b".."], b""))]
    #[case::slash(crafted(&[b"a/b"], b""))]
    #[case::empty_name(crafted(&[], b""))]
    #[case::nul(crafted(&[], b"a\0b"))]
    fn readers_refuse_what_nix_refuses(#[case] nar: Vec<u8>) {
        let d = tempdir().unwrap();
        assert!(parse_length(&mut nar.as_slice()).is_err());
        assert!(restore_path(&mut nar.as_slice(), &d.path().join("out")).is_err());
    }

    #[test]
    fn readers_take_a_sorted_directory() {
        assert!(parse_length(&mut crafted(&[b"a", b"b"], b"").as_slice()).is_ok());
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
        // Windows has no executable bit to restore
        assert_eq!(
            is_executable(&fs::metadata(out.join("run.sh")).unwrap()),
            cfg!(unix)
        );
        assert!(!is_executable(&fs::metadata(out.join("a.txt")).unwrap()));
    }

    #[test]
    fn restore_rejects_garbage() {
        let out = env::temp_dir().join(format!("iets-nar-garbage-{}", std::process::id()));
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
}
