//! The one HTTP client iets has: a streaming GET, for the fetch builtins
//! (`builtins::fetch`). `file://` reads the local file, as libcurl does for the
//! `tarball` inputs `nix flake lock` writes with that scheme; the rest goes to
//! `client`, the network half, which a platform without sockets replaces
//! with a stub. Here the bytes get decompressed and limited, the same for both.

use std::cell::Cell;
use std::fs::File;
#[cfg(test)]
use std::io::Cursor;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::mem;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::config;
use crate::error::{Error, Result};
use crate::log::{ACT_FILE_TRANSFER, Activity, LVL_TALKATIVE, RES_PROGRESS};
use crate::remote::worker;
use crate::session::Session;
use crate::sys;

#[cfg_attr(target_os = "wasi", path = "http/client_none.rs")]
mod client;

pub use client::Kept;

/// The zip-bomb guard: past `RATIO_AFTER` bytes out, a stream that gives more
/// than `MAX_RATIO` bytes out per byte in is refused. The narHash is checked
/// only at the end, so it bounds nothing on the way.
const MAX_RATIO: u64 = 1000;
const RATIO_AFTER: u64 = 16 << 20;

/// What one download may cost. filetransfer.cc sets `connect-timeout` as
/// `CURLOPT_CONNECTTIMEOUT` and `stalled-download-timeout` as
/// `CURLOPT_LOW_SPEED_TIME` (at 1 byte/s); 0 is no limit for both.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub connect: Option<Duration>,
    pub stall: Option<Duration>,
    /// the `fetch-max-size` setting: decompressed bytes.
    pub max_size: u64,
}

impl Limits {
    /// The nix.conf timeouts with filetransfer.hh's defaults, 15 s and 300 s.
    pub fn new(cfg: &config::Config) -> Result<Limits> {
        let secs = |name: &str, default: u64| -> Result<Option<Duration>> {
            let s = cfg.int(name)?.unwrap_or(default);
            Ok((s > 0).then(|| Duration::from_secs(s)))
        };
        Ok(Limits {
            connect: secs("connect-timeout", 15)?,
            stall: secs("stalled-download-timeout", 300)?,
            max_size: Session::current().settings().fetch_max_size,
        })
    }
}

/// A URL as a message may show it: the password of a `user:password@host`
/// authority is never printed, here or anywhere else.
pub fn shown(url: &str) -> String {
    let Some((head, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..end]
        .rsplit_once('@')
        .and_then(|(u, _)| u.split_once(':'))
    {
        Some((user, pass)) => format!(
            "{head}://{user}:***@{}",
            &rest[user.len() + pass.len() + 2..]
        ),
        None => url.to_string(),
    }
}

fn bad(url: &str, why: &str) -> Error {
    Error::Usage(format!("cannot fetch '{}': {why}", shown(url)))
}

/// The local path a `file:` URL names: `file:/p`, `file:///p` and
/// `file://localhost/p` all name `/p`. libcurl reads an empty host and
/// `localhost`; any other host is not a local file, so it is not one here.
fn file_path(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("file:")?;
    let Some(after) = rest.strip_prefix("//") else {
        return Some(rest);
    };
    let (host, path) = after.split_at(after.find('/')?);
    (host.is_empty() || host.eq_ignore_ascii_case("localhost")).then_some(path)
}

/// `GET url` with `headers`, following redirects, with the body decompressed
/// into `sink` as it arrives; the number of bytes written. A `file://` URL is
/// read from the file system instead. Nothing buffers the whole body.
/// `headers` are credentials: they go only where `redirect` lets them.
/// `IETS_OFFLINE=1` makes any download an error that names the URL.
pub fn get_decompressed(
    url: &str,
    headers: &[(String, String)],
    limits: Limits,
    sink: &mut dyn Write,
) -> Result<u64> {
    get(url, headers, limits, true, sink)
}

/// The local path a `file:` URL names, decoded and without its query.
pub fn local_path(url: &str) -> Option<String> {
    let path = file_path(url)?;
    let path = path.split(['?', '#']).next().unwrap_or(path);
    Some(worker::percent_decode(path))
}

/// [`get`] of `url` for bytes the caller read itself: a pure evaluation reads
/// a `file:` URL through its own accessor
/// (`builtins::store_hooks::local_source`), not through the kernel.
pub fn put(
    url: &str,
    bytes: Vec<u8>,
    limits: Limits,
    decompress: bool,
    sink: &mut dyn Write,
) -> Result<u64> {
    let read_in = Rc::new(Cell::new(0));
    let raw: Box<dyn Read> = Box::new(Counted(Box::new(io::Cursor::new(bytes)), read_in.clone()));
    let read = if decompress {
        decompress_reader(raw, layers_budget(limits.max_size))?
    } else {
        raw
    };
    drain(url, read, &read_in, limits, sink)
}

/// [`get_decompressed`], with the decompression optional: `iets prefetch`
/// hashes the bytes the URL serves, as `nix store prefetch-file` does.
pub fn get(
    url: &str,
    headers: &[(String, String)],
    limits: Limits,
    decompress: bool,
    sink: &mut dyn Write,
) -> Result<u64> {
    request(url, headers, None, limits, decompress, sink)
}

/// [`get`] with a POST of `body`, for an API such as Git LFS's batch API.
pub fn post(
    url: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    limits: Limits,
    sink: &mut dyn Write,
) -> Result<u64> {
    request(url, headers, Some(body), limits, false, sink)
}

fn request(
    url: &str,
    headers: &[(String, String)],
    body: Option<Vec<u8>>,
    limits: Limits,
    decompress: bool,
    sink: &mut dyn Write,
) -> Result<u64> {
    let undo = |r: Box<dyn Read>| {
        if decompress {
            decompress_reader(r, layers_budget(limits.max_size))
        } else {
            Ok(r)
        }
    };
    if Session::current().settings().offline {
        return Err(Error::Usage(format!(
            "cannot fetch '{url}': IETS_OFFLINE is set"
        )));
    }
    let read_in = Rc::new(Cell::new(0));
    if let Some(path) = local_path(url).filter(|_| body.is_none()) {
        let file = File::open(sys::path_from_internal(path.as_bytes()))
            .map_err(|e| Error::io(format!("cannot read '{path}'"), e))?;
        let raw = Counted(Box::new(file), read_in.clone());
        return drain(url, undo(Box::new(raw))?, &read_in, limits, sink);
    }
    client::fetch(url, headers, body, limits, |queued| {
        undo(Box::new(Counted(queued, read_in.clone())))
            .and_then(|r| drain(url, r, &read_in, limits, sink))
    })
}

/// A reader that adds what it reads to a counter: the compressed side, for the
/// ratio [`drain`] checks.
struct Counted(Box<dyn Read>, Rc<Cell<u64>>);

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        self.1.set(self.1.get() + n as u64);
        Ok(n)
    }
}

/// Copies the decompressed stream into `sink` within `limits.max_size` and the
/// [`MAX_RATIO`] to the `read_in` compressed bytes.
fn drain(
    url: &str,
    mut read: Box<dyn Read>,
    read_in: &Cell<u64>,
    limits: Limits,
    sink: &mut dyn Write,
) -> Result<u64> {
    let shown = shown(url);
    // filetransfer.cc:TransferItem::act
    let act = Activity::new(
        LVL_TALKATIVE,
        ACT_FILE_TRANSFER,
        &format!("downloading '{shown}'"),
        vec![shown.as_str().into()],
    );
    let mut told = Instant::now() - Duration::from_millis(100);
    let mut buf = vec![0; 64 << 10];
    let mut out = 0u64;
    loop {
        if told.elapsed() >= Duration::from_millis(100) {
            told = Instant::now();
            act.result(
                RES_PROGRESS,
                vec![read_in.get().into(), 0.into(), 0.into(), 0.into()],
            );
        }
        let n = match read.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::io("cannot read the download", e)),
        };
        out += n as u64;
        if out > limits.max_size {
            let why = format!("over the fetch-max-size of {} bytes", limits.max_size);
            return Err(bad(url, &why));
        }
        if out > RATIO_AFTER && out / read_in.get().max(1) > MAX_RATIO {
            let why = format!("it decompresses over {MAX_RATIO} times its size");
            return Err(bad(url, &why));
        }
        sink.write_all(&buf[..n])
            .map_err(|e| Error::io("cannot write the download", e))?;
    }
}

/// The magic bytes libarchive's `archive_read_support_filter_all` recognises,
/// for the filters iets implements (tarfile.cc:TarArchive).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Compression {
    None,
    Gzip,
    Xz,
    Zstd,
    Bzip2,
}

impl Compression {
    pub fn sniff(bytes: &[u8]) -> Compression {
        const XZ: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];
        const ZSTD: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
        if bytes.starts_with(&[0x1f, 0x8b]) {
            Compression::Gzip
        } else if bytes.starts_with(XZ) {
            Compression::Xz
        } else if bytes.starts_with(ZSTD) {
            Compression::Zstd
        } else if bytes.starts_with(b"BZh") {
            Compression::Bzip2
        } else {
            Compression::None
        }
    }
}

/// The magic bytes read ahead of the stream they came from.
struct Sniffed {
    head: Vec<u8>,
    pos: usize,
    rest: Box<dyn Read>,
}

impl Read for Sniffed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.head.len() {
            let n = buf.len().min(self.head.len() - self.pos);
            buf[..n].copy_from_slice(&self.head[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.rest.read(buf)
    }
}

/// Gzip members one after another, and the stream over as soon as what follows
/// the last one is not a gzip header. `MultiGzDecoder` makes that an error;
/// libarchive's filter bids on the bytes and, finding no filter, hands the rest
/// to the tar reader, which stops at its own end-of-archive marker.
struct MultiGz {
    at: flate2::bufread::GzDecoder<BufReader<Box<dyn Read>>>,
}

impl Read for MultiGz {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.at.read(buf)? {
                0 => {}
                n => return Ok(n),
            }
            let rest = self.at.get_mut();
            if !BufRead::fill_buf(rest)?.starts_with(&[0x1f, 0x8b]) {
                return Ok(0);
            }
            let inner = mem::replace(rest, BufReader::new(Box::new(&[][..])));
            self.at = flate2::bufread::GzDecoder::new(inner);
        }
    }
}

/// How many filters `archive_read_support_filter_all` may stack before iets
/// calls the stream a bomb: a doubly compressed tarball is one Nix accepts.
const MAX_FILTERS: usize = 8;

/// What the layers of an archive within `max_size` may read: every layer
/// about that much, with room for the framing that grows incompressible
/// data. `drain` still holds the output to `max_size`.
fn layers_budget(max_size: u64) -> u64 {
    max_size.saturating_mul(2 * (MAX_FILTERS as u64 + 1))
}

/// The bytes every filter layer reads, charged against one budget, so the
/// work of a layer that outputs little, such as a run of empty gzip members,
/// is bounded too.
struct Charged {
    inner: Box<dyn Read>,
    left: Rc<Cell<u64>>,
}

impl Read for Charged {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let left = self.left.get().checked_sub(n as u64).ok_or_else(|| {
            io::Error::other("the archive's layers of compression decompress to too much")
        })?;
        self.left.set(left);
        Ok(n)
    }
}

/// `read` with the compression its first bytes announce undone, as
/// `archive_read_support_filter_all` does it, without holding the stream. A
/// filter's own output is sniffed again, so a doubly compressed archive
/// unpacks. What the layers read together may not exceed `budget`.
pub fn decompress_reader(read: Box<dyn Read>, budget: u64) -> Result<Box<dyn Read>> {
    let left = Rc::new(Cell::new(budget));
    let mut read = read;
    for _ in 0..MAX_FILTERS {
        read = Box::new(Charged {
            inner: read,
            left: left.clone(),
        });
        let mut head = [0u8; 6];
        let mut n = 0;
        while n < head.len() {
            match read.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::io("cannot read the archive", e)),
            }
        }
        let kind = Compression::sniff(&head[..n]);
        let src = Box::new(Sniffed {
            head: head[..n].to_vec(),
            pos: 0,
            rest: read,
        }) as Box<dyn Read>;
        read = match kind {
            Compression::None => return Ok(src),
            Compression::Gzip => Box::new(MultiGz {
                at: flate2::bufread::GzDecoder::new(BufReader::new(src)),
            }),
            Compression::Xz => Box::new(lzma_rust2::XzReader::new(src, true)),
            Compression::Zstd => Box::new(
                zstd::stream::read::Decoder::new(src)
                    .map_err(|e| Error::io("cannot start the zstd decoder", e))?,
            ),
            Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(src)),
        };
    }
    Err(Error::Usage(format!(
        "the archive has over {MAX_FILTERS} layers of compression"
    )))
}

#[cfg(test)]
/// `bytes` with the compression it announces undone.
pub fn decompress(bytes: Vec<u8>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decompress_reader(Box::new(Cursor::new(bytes)), u64::MAX)?
        .read_to_end(&mut out)
        .map_err(|e| Error::io("cannot decompress the stream", e))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    use std::fs;
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    /// libcurl reads `file:/p`, `file:///p` and `file://localhost/p` alike.
    #[test]
    fn file_urls_name_a_local_path() {
        assert_eq!(file_path("file:///abs/p"), Some("/abs/p"));
        assert_eq!(file_path("file://localhost/abs/p"), Some("/abs/p"));
        assert_eq!(file_path("file:/abs/p"), Some("/abs/p"));
        assert_eq!(file_path("file://elsewhere/abs/p"), None);
        assert_eq!(file_path("https://x.example/p"), None);
    }

    /// libarchive ignores what follows the last gzip member and bids its
    /// filters on a filter's own output, so both of these unpack.
    #[test]
    fn trailing_bytes_and_a_second_layer() {
        let gz = |bytes: &[u8]| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(bytes).unwrap();
            e.finish().unwrap()
        };
        let one = gz(b"payload");
        for extra in [&b""[..], &[0u8; 8][..], b"GARBAGE"] {
            let mut bytes = one.clone();
            bytes.extend_from_slice(extra);
            assert_eq!(decompress(bytes).unwrap(), b"payload", "{extra:?}");
        }
        let mut two = one.clone();
        two.extend_from_slice(&one);
        assert_eq!(decompress(two).unwrap(), b"payloadpayload");
        assert_eq!(decompress(gz(&one)).unwrap(), b"payload");

        let mut onion = b"deep".to_vec();
        for _ in 0..MAX_FILTERS + 1 {
            onion = gz(&onion);
        }
        assert!(
            decompress(onion)
                .unwrap_err()
                .to_string()
                .contains("layers")
        );

        // empty members that output nothing still cost what they are read as
        let empty = gz(b"");
        let members = empty.repeat(1 << 14);
        let r = decompress_reader(Box::new(Cursor::new(gz(&members))), 1 << 16);
        let mut sink = Vec::new();
        assert!(
            r.and_then(|mut r| r.read_to_end(&mut sink).map_err(|e| Error::io("x", e)))
                .is_err()
        );

        // incompressible bytes compressed twice still fit the budget of
        // their size
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let noise: Vec<u8> = (0..800 << 10)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let mut r = decompress_reader(
            Box::new(Cursor::new(gz(&gz(&noise)))),
            layers_budget(1 << 20),
        )
        .unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, noise);
    }

    #[test]
    fn magic_bytes() {
        assert_eq!(Compression::sniff(&[0x1f, 0x8b, 8, 0]), Compression::Gzip);
        assert_eq!(Compression::sniff(b"BZh9"), Compression::Bzip2);
        assert_eq!(
            Compression::sniff(&[0xfd, b'7', b'z', b'X', b'Z', 0]),
            Compression::Xz
        );
        assert_eq!(
            Compression::sniff(&[0x28, 0xb5, 0x2f, 0xfd]),
            Compression::Zstd
        );
        assert_eq!(Compression::sniff(b"./file\0\0"), Compression::None);
    }

    /// A server on 127.0.0.1 for `n` connections, one after the other: `reply`
    /// answers each; the lowercased request heads come out of the receiver.
    fn serve(
        n: usize,
        reply: impl Fn(&str, &mut TcpStream) + Send + 'static,
    ) -> (u16, Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for c in l.incoming().take(n) {
                let mut c = c.unwrap();
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && c.read(&mut b).unwrap_or(0) == 1 {
                    head.push(b[0]);
                }
                let head = String::from_utf8_lossy(&head).to_lowercase();
                reply(&head, &mut c);
                let _ = tx.send(head);
            }
        });
        (port, rx)
    }

    fn respond(c: &mut TcpStream, status: &str, extra: &str, body: &[u8]) {
        let len = body.len();
        let head = format!("HTTP/1.1 {status}\r\n{extra}content-length: {len}\r\n\r\n");
        let _ = c.write_all(head.as_bytes());
        let _ = c.write_all(body);
    }

    /// No wall-clock limit: the servers run in this process, so any limit
    /// fires when the host does not schedule the process for that long.
    fn limits() -> Limits {
        Limits {
            connect: None,
            stall: None,
            max_size: 1 << 30,
        }
    }

    #[test]
    fn credentials_stay_on_the_first_origin() {
        let (other, seen_other) = serve(1, |_, c| respond(c, "200 OK", "", b"hello"));
        let (first, seen_first) = serve(2, move |head, c| {
            let to = if head.starts_with("get / ") {
                "/again".to_string()
            } else {
                format!("http://127.0.0.1:{other}/x")
            };
            respond(c, "302 Found", &format!("location: {to}\r\n"), b"");
        });
        let auth = [("Authorization".to_string(), "token s3cret".to_string())];
        let mut out = Vec::new();
        let url = format!("http://127.0.0.1:{first}/");
        get_decompressed(&url, &auth, limits(), &mut out).unwrap();
        assert_eq!(out, b"hello");
        for _ in 0..2 {
            assert!(
                seen_first
                    .recv()
                    .unwrap()
                    .contains("authorization: token s3cret")
            );
        }
        assert!(!seen_other.recv().unwrap().contains("authorization"));
    }

    /// A download against a server that sends `sent` and then holds the
    /// connection open until the call has returned: only the stall limit can
    /// end the call, however long the host takes to schedule either side.
    fn stalled(sent: &'static [u8]) -> String {
        let (hold, held) = mpsc::channel::<()>();
        let (port, _) = serve(1, move |_, c| {
            let _ = c.write_all(sent);
            let _ = held.recv();
        });
        let l = Limits {
            stall: Some(Duration::from_millis(50)),
            ..limits()
        };
        let url = format!("http://127.0.0.1:{port}/");
        let e = get_decompressed(&url, &[], l, &mut Vec::new()).unwrap_err();
        drop(hold);
        e.to_string()
    }

    #[test]
    fn a_server_that_never_answers_is_a_stall() {
        let e = stalled(b"");
        assert!(e.contains("no response (stalled-download-timeout)"), "{e}");
    }

    #[test]
    fn a_stalled_download_fails() {
        // a server the host does not schedule within the limit ends in the
        // wait for the status line instead, which is the same limit
        let e = stalled(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nhalf");
        assert!(e.contains("(stalled-download-timeout)"), "{e}");
    }

    fn file_url(d: &Path, name: &str, bytes: &[u8]) -> String {
        let f = d.join(name);
        fs::write(&f, bytes).unwrap();
        let internal = String::from_utf8(sys::path_to_internal(&f)).expect("utf-8 path");
        format!("file://{internal}")
    }

    #[test]
    fn downloads_stop_at_the_size_limit() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&vec![0; 4 << 20]).unwrap();
        let d = tempdir().unwrap();
        let url = file_url(d.path(), "size.gz", &gz.finish().unwrap());
        let small = Limits {
            max_size: 1 << 20,
            ..limits()
        };
        let e = get_decompressed(&url, &[], small, &mut Vec::new()).unwrap_err();
        assert!(e.to_string().contains("fetch-max-size"), "{e}");
        assert_eq!(
            get_decompressed(&url, &[], limits(), &mut std::io::sink()).unwrap(),
            4 << 20
        );
    }

    /// libcurl sends `user:password@host` as Basic credentials, and no message
    /// ever prints the password.
    #[test]
    fn credentials_in_the_url_are_sent_and_never_printed() {
        // `serve` hands the request head over in lower case
        let want = format!(
            "authorization: basic {}",
            crate::store::hash::base64_encode(b"user:s3cret").to_lowercase()
        );
        let sent = want.clone();
        let (port, seen) = serve(1, move |head, c| {
            let ok = head.contains(&sent);
            let status = if ok { "200 OK" } else { "401 Unauthorized" };
            respond(c, status, "", if ok { b"hello" } else { b"" });
        });
        let mut out = Vec::new();
        let url = format!("http://user:s3cret@127.0.0.1:{port}/x");
        get_decompressed(&url, &[], limits(), &mut out).unwrap();
        assert_eq!(out, b"hello");
        assert!(seen.recv().unwrap().contains(&want));

        let (port, _) = serve(1, |_, c| respond(c, "403 Forbidden", "", b""));
        let url = format!("http://user:s3cret@127.0.0.1:{port}/x");
        let e = get_decompressed(&url, &[], limits(), &mut Vec::new())
            .unwrap_err()
            .to_string();
        assert!(!e.contains("s3cret"), "{e}");
        assert!(e.contains("user:***@"), "{e}");
        assert_eq!(shown("https://x.example/a?u=1"), "https://x.example/a?u=1");
    }

    /// filetransfer.cc retries a 5xx and gives up on a 404.
    #[test]
    fn a_transient_status_is_retried() {
        let n = AtomicUsize::new(0);
        let (port, _) = serve(3, move |_, c| {
            let i = n.fetch_add(1, Ordering::Relaxed);
            if i < 2 {
                respond(c, "503 Service Unavailable", "", b"");
            } else {
                respond(c, "200 OK", "", b"late");
            }
        });
        let mut out = Vec::new();
        let url = format!("http://127.0.0.1:{port}/");
        get_decompressed(&url, &[], limits(), &mut out).unwrap();
        assert_eq!(out, b"late");

        let (port, _) = serve(1, |_, c| respond(c, "404 Not Found", "", b""));
        let url = format!("http://127.0.0.1:{port}/");
        let e = get_decompressed(&url, &[], limits(), &mut Vec::new()).unwrap_err();
        assert!(e.to_string().contains("HTTP status 404"), "{e}");
    }

    /// deflate tops out near 1032:1, so the bomb is zstd.
    #[test]
    fn a_decompression_bomb_is_refused() {
        let bomb = zstd::encode_all(&vec![0u8; 64 << 20][..], 19).unwrap();
        let d = tempdir().unwrap();
        let url = file_url(d.path(), "bomb.zst", &bomb);
        let e = get_decompressed(&url, &[], limits(), &mut io::sink()).unwrap_err();
        assert!(e.to_string().contains("times its size"), "{e}");
    }
}
