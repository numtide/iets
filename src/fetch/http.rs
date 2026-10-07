//! The one HTTP client iets has: a streaming GET, for flake inputs
//! (`flake::fetch`). hyper over tokio-rustls, the stack tonic already
//! brings; no libcurl. Redirects are followed like
//! filetransfer.cc (`CURLOPT_FOLLOWLOCATION`, `CURLOPT_MAXREDIRS` 10) and
//! `file://` reads the local file, as libcurl does for the `tarball` inputs
//! `nix flake lock` writes with that scheme. Credentials follow a redirect only
//! as libcurl lets them: see [`redirect`].
//!
//! The body is never held whole: hyper runs on a thread of its own and hands
//! its frames to a bounded queue, the caller's thread reads that queue through
//! the decompressor, and both ends stop when the other does.

use std::cell::Cell;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::log::ACT_FILE_TRANSFER;
use crate::log::Activity;
use crate::log::LVL_TALKATIVE;
use crate::log::RES_PROGRESS;
use http_body_util::BodyExt;

/// filetransfer.cc:CURLOPT_MAXREDIRS.
const MAX_REDIRECTS: usize = 10;

/// filetransfer.hh:`download-attempts`. The setting itself is the daemon's
/// (`docs/reference/settings.md`), so this is its default, fixed.
const TRIES: u32 = 5;

/// Body frames in flight between hyper and the reader. hyper's frames are tens
/// of kilobytes, so this is the download's whole memory cost.
const QUEUE: usize = 16;

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
    pub fn new(cfg: &crate::config::Config) -> Result<Limits> {
        let secs = |name: &str, default: u64| -> Result<Option<Duration>> {
            let s = cfg.int(name)?.unwrap_or(default);
            Ok((s > 0).then(|| Duration::from_secs(s)))
        };
        Ok(Limits {
            connect: secs("connect-timeout", 15)?,
            stall: secs("stalled-download-timeout", 300)?,
            max_size: crate::session::Session::current().settings().fetch_max_size,
        })
    }
}

/// `fut`, or `why` once `limit` has passed.
async fn within<T>(
    limit: Option<Duration>,
    why: impl FnOnce() -> Error,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match limit {
        None => fut.await,
        Some(d) => tokio::time::timeout(d, fut).await.map_err(|_| why())?,
    }
}

/// What a URL was split into; `target` is the request line's path and query.
struct Url {
    https: bool,
    host: String,
    port: u16,
    target: String,
    /// `user:password` before the host, percent-decoded.
    userinfo: Option<String>,
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

impl Url {
    fn parse(url: &str) -> Result<Url> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| bad(url, "no scheme"))?;
        let https = match scheme {
            "https" => true,
            "http" => false,
            other => return Err(bad(url, &format!("unsupported scheme '{other}'"))),
        };
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let (userinfo, authority) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(crate::remote::worker::percent_decode(u)), h),
            None => (None, authority),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h, p.parse::<u16>().map_err(|_| bad(url, "bad port"))?)
            }
            _ => (authority, if https { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(bad(url, "no host"));
        }
        Ok(Url {
            https,
            host: host.to_string(),
            port,
            target: if path.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
            userinfo,
        })
    }

    /// libcurl sends the URL's `user:password` as Basic credentials on the
    /// first request, without waiting for a challenge (`CURLAUTH_BASIC` is its
    /// default `CURLOPT_HTTPAUTH`).
    fn basic(&self) -> Option<(String, String)> {
        let u = self.userinfo.as_ref()?;
        let b64 = crate::store::hash::base64_encode(u.as_bytes());
        Some(("Authorization".into(), format!("Basic {b64}")))
    }

    fn authority(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// A redirect from `from` to `to` in a transfer that began at `first`: whether
/// the credentials go along. libcurl sends a custom `Authorization` only to the
/// first scheme, host and port (http.c:Curl_allow_auth_to_host, the
/// CVE-2022-27776 fix), and filetransfer.cc never sets
/// `CURLOPT_UNRESTRICTED_AUTH`. iets's headers are all credentials, so the rule
/// covers `Private-token` too. An https-to-http redirect is refused: libcurl
/// allows it, but it drops TLS from a download nothing has verified yet.
fn redirect(first: &Url, from: &Url, to: &Url) -> std::result::Result<bool, &'static str> {
    if from.https && !to.https {
        return Err("refusing a redirect from https to http");
    }
    Ok(first.https == to.https
        && first.host.eq_ignore_ascii_case(&to.host)
        && first.port == to.port)
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

/// Whether `location` starts with a scheme, and so is absolute. What comes
/// before `://` must be a scheme (RFC 3986): a `://` in a query is not one.
fn has_scheme(location: &str) -> bool {
    let Some((s, _)) = location.split_once("://") else {
        return false;
    };
    s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
}

/// `.` and `..` segments removed, as a URL path has them removed
/// (RFC 3986:5.2.4). A `..` at the top removes nothing.
fn normalise(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let slash = path.ends_with(['/', '.']) && !out.is_empty();
    format!("/{}{}", out.join("/"), if slash { "/" } else { "" })
}

/// `Location` against the URL it came from: absolute, host-relative or
/// path-relative (libcurl resolves all three).
fn resolve(base: &str, location: &str) -> String {
    if has_scheme(location) {
        return location.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let joined = if location.starts_with('/') {
        location.to_string()
    } else {
        let path = &rest[authority.len()..];
        let path = path.split(['?', '#']).next().unwrap_or(path);
        format!(
            "{}/{location}",
            path.rsplit_once('/').map_or("", |(d, _)| d)
        )
    };
    let (path, tail) = joined.split_at(joined.find(['?', '#']).unwrap_or(joined.len()));
    format!("{scheme}://{authority}{}{tail}", normalise(path))
}

/// One rustls client config for the process: the platform's CA store, which is
/// what curl uses too. Process-wide on purpose: it is immutable, costly to
/// build, and holds no run state, so two sessions may share it.
fn tls() -> Result<Arc<rustls::ClientConfig>> {
    static TLS: OnceLock<std::result::Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    TLS.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        let found = rustls_native_certs::load_native_certs();
        if found.certs.is_empty() {
            return Err(format!(
                "no trusted CA certificates found: {}",
                found
                    .errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        for c in found.certs {
            let _ = roots.add(c);
        }
        let mut cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    })
    .clone()
    .map_err(Error::Config)
}

/// `GET url` with `headers`, following redirects, with the body decompressed
/// into `sink` as it arrives; the number of bytes written. A `file://` URL is
/// read from the file system instead. Nothing buffers the whole body.
/// `headers` are credentials: they go only where [`redirect`] lets them.
///
/// `IETS_OFFLINE=1` makes any download an error. A run that has every input in
/// the input store then costs nothing and needs nothing; a run that is missing
/// one says which URL it wanted.
pub fn get_decompressed(
    url: &str,
    headers: &[(String, String)],
    limits: Limits,
    sink: &mut dyn Write,
) -> Result<u64> {
    get(url, headers, limits, true, sink)
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
    let undo = |r: Box<dyn Read>| {
        if decompress {
            decompress_reader(r)
        } else {
            Ok(r)
        }
    };
    if crate::session::Session::current().settings().offline {
        return Err(Error::Usage(format!(
            "cannot fetch '{url}': IETS_OFFLINE is set"
        )));
    }
    let read_in = Rc::new(Cell::new(0));
    if let Some(path) = file_path(url) {
        let path =
            crate::remote::worker::percent_decode(path.split(['?', '#']).next().unwrap_or(path));
        let file = std::fs::File::open(&path)
            .map_err(|e| Error::io(format!("cannot read '{path}'"), e))?;
        let raw = Counted(Box::new(file), read_in.clone());
        return drain(url, undo(Box::new(raw))?, &read_in, limits, sink);
    }
    let (tx, rx) = tokio::sync::mpsc::channel(QUEUE);
    let target = url.to_string();
    let sent = headers.to_vec();
    let download = std::thread::spawn(move || -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::io("cannot start a tokio runtime", e))?;
        rt.block_on(stream(&target, &sent, limits, tx))
    });
    let queued = Queued {
        rx,
        at: Vec::new(),
        pos: 0,
    };
    let read = undo(Box::new(Counted(Box::new(queued), read_in.clone())))
        .and_then(|r| drain(url, r, &read_in, limits, sink));
    download
        .join()
        .map_err(|_| Error::Store("the download thread panicked".into()))??;
    read
}

/// The reading half of the queue: hyper's frames, one after the other.
struct Queued {
    rx: tokio::sync::mpsc::Receiver<std::result::Result<Vec<u8>, String>>,
    at: Vec<u8>,
    pos: usize,
}

impl Read for Queued {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos == self.at.len() {
            match self.rx.blocking_recv() {
                None => return Ok(0),
                Some(Err(e)) => return Err(std::io::Error::other(e)),
                Some(Ok(chunk)) => {
                    self.at = chunk;
                    self.pos = 0;
                }
            }
        }
        let n = buf.len().min(self.at.len() - self.pos);
        buf[..n].copy_from_slice(&self.at[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// A reader that adds what it reads to a counter: the compressed side, for the
/// ratio [`drain`] checks.
struct Counted(Box<dyn Read>, Rc<Cell<u64>>);

impl Read for Counted {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
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
    let mut told = std::time::Instant::now() - Duration::from_millis(100);
    let mut buf = vec![0; 64 << 10];
    let mut out = 0u64;
    loop {
        if told.elapsed() >= Duration::from_millis(100) {
            told = std::time::Instant::now();
            act.result(
                RES_PROGRESS,
                vec![read_in.get().into(), 0.into(), 0.into(), 0.into()],
            );
        }
        let n = match read.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
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

/// Follows redirects and pushes the frames of the 200 into `tx`.
async fn stream(
    url: &str,
    headers: &[(String, String)],
    limits: Limits,
    tx: tokio::sync::mpsc::Sender<std::result::Result<Vec<u8>, String>>,
) -> Result<()> {
    let first = Url::parse(url)?;
    let mut sent = headers.to_vec();
    sent.extend(first.basic());
    let headers = &sent[..];
    let mut current = url.to_string();
    let mut credentials = true;
    let (mut redirects, mut attempt) = (0usize, 0u32);
    loop {
        let creds = if credentials { headers } else { &[] };
        let (status, location, mut body, driver) = once(&current, creds, limits).await?;
        match status {
            200 => {
                let mut sent = Ok(());
                loop {
                    let stalled =
                        || bad(&current, "the download stalled (stalled-download-timeout)");
                    let next = within(limits.stall, stalled, async {
                        body.frame().await.transpose().map_err(|e| {
                            bad(&current, &format!("reading the response failed: {e}"))
                        })
                    });
                    let frame = match next.await {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string())).await;
                            sent = Err(e);
                            break;
                        }
                    };
                    if let Ok(data) = frame.into_data()
                        && tx.send(Ok(data.to_vec())).await.is_err()
                    {
                        break;
                    }
                }
                driver.abort();
                return sent;
            }
            301 | 302 | 303 | 307 | 308 => {
                driver.abort();
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(bad(url, &format!("more than {MAX_REDIRECTS} redirects")));
                }
                let Some(to) = location else {
                    return Err(bad(&current, &format!("HTTP {status} with no Location")));
                };
                let next = resolve(&current, &to);
                credentials = redirect(&first, &Url::parse(&current)?, &Url::parse(&next)?)
                    .map_err(|why| bad(&next, why))?;
                current = next;
            }
            other => {
                driver.abort();
                attempt += 1;
                let Some(base) = transient(other).filter(|_| attempt < TRIES) else {
                    return Err(bad(&current, &format!("HTTP status {other}")));
                };
                let ms = base << (attempt - 1);
                crate::log::line(&format!(
                    "warning: unable to download '{}': HTTP error {other}; \
                     retrying in {ms} ms (attempt {attempt}/{TRIES})",
                    shown(&current)
                ));
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        }
    }
}

/// filetransfer.cc:finish: the statuses Nix treats as transient, and the delay
/// each one starts from (`RETRY_TIME_MS_DEFAULT`, and a minute for 429). 404,
/// 410 and the authorisation failures are final, as are 501, 505 and 511.
fn transient(status: u16) -> Option<u64> {
    match status {
        429 => Some(60_000),
        408 => Some(250),
        501 | 505 | 511 => None,
        _ if (500..600).contains(&status) => Some(250),
        _ => None,
    }
}

type Started = (
    u16,
    Option<String>,
    hyper::body::Incoming,
    tokio::task::JoinHandle<std::result::Result<(), hyper::Error>>,
);

/// One request: the status, the `Location` header, the body not yet read, and
/// the connection task that must live as long as the body. `connect-timeout`
/// covers TCP and TLS, as `CURLOPT_CONNECTTIMEOUT` does; the wait for the
/// status line counts as a stall.
async fn once(url: &str, headers: &[(String, String)], limits: Limits) -> Result<Started> {
    let u = Url::parse(url)?;
    let timed_out = || bad(url, "the connection timed out (connect-timeout)");
    let io = within(limits.connect, timed_out, async {
        let stream = tokio::net::TcpStream::connect((u.host.as_str(), u.port))
            .await
            .map_err(|e| Error::io(format!("cannot connect to '{}'", u.authority()), e))?;
        if !u.https {
            return Ok(Box::new(stream) as Box<dyn Stream>);
        }
        let name = rustls::pki_types::ServerName::try_from(u.host.clone())
            .map_err(|_| bad(url, "the host is not a valid TLS name"))?;
        let tls = tokio_rustls::TlsConnector::from(tls()?);
        let tls = tls
            .connect(name, stream)
            .await
            .map_err(|e| Error::io(format!("TLS handshake with '{}'", u.host), e))?;
        Ok(Box::new(tls) as Box<dyn Stream>)
    })
    .await?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
            .await
            .map_err(|e| bad(url, &format!("HTTP handshake failed: {e}")))?;
    let driver = tokio::spawn(conn);

    let mut req = hyper::Request::builder()
        .method("GET")
        .uri(&u.target)
        .header("host", u.authority())
        .header("user-agent", concat!("iets/", env!("CARGO_PKG_VERSION")));
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let req = req
        .body(String::new())
        .map_err(|e| bad(url, &format!("bad request: {e}")))?;
    let stalled = || bad(url, "no response (stalled-download-timeout)");
    let res = within(limits.stall, stalled, async {
        sender
            .send_request(req)
            .await
            .map_err(|e| bad(url, &format!("request failed: {e}")))
    })
    .await?;
    let status = res.status().as_u16();
    let location = res
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok((status, location, res.into_body(), driver))
}

trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> Stream for T {}

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
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
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
    at: flate2::bufread::GzDecoder<std::io::BufReader<Box<dyn Read>>>,
}

impl Read for MultiGz {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.at.read(buf)? {
                0 => {}
                n => return Ok(n),
            }
            let rest = self.at.get_mut();
            if !std::io::BufRead::fill_buf(rest)?.starts_with(&[0x1f, 0x8b]) {
                return Ok(0);
            }
            let inner = std::mem::replace(rest, std::io::BufReader::new(Box::new(&[][..])));
            self.at = flate2::bufread::GzDecoder::new(inner);
        }
    }
}

/// How many filters `archive_read_support_filter_all` may stack before iets
/// calls the stream a bomb: a doubly compressed tarball is one Nix accepts.
const MAX_FILTERS: usize = 8;

/// `read` with the compression its first bytes announce undone, as
/// `archive_read_support_filter_all` does it, without holding the stream. A
/// filter's own output is sniffed again, so a doubly compressed archive
/// unpacks.
pub fn decompress_reader(read: Box<dyn Read>) -> Result<Box<dyn Read>> {
    let mut read = read;
    for _ in 0..MAX_FILTERS {
        let mut head = [0u8; 6];
        let mut n = 0;
        while n < head.len() {
            match read.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
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
                at: flate2::bufread::GzDecoder::new(std::io::BufReader::new(src)),
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
    decompress_reader(Box::new(std::io::Cursor::new(bytes)))?
        .read_to_end(&mut out)
        .map_err(|e| Error::io("cannot decompress the stream", e))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split() {
        let u = Url::parse("https://api.github.com/repos/a/b/tarball/c").unwrap();
        assert!(u.https);
        assert_eq!((u.host.as_str(), u.port), ("api.github.com", 443));
        assert_eq!(u.target, "/repos/a/b/tarball/c");
        assert_eq!(u.authority(), "api.github.com");

        let u = Url::parse("http://localhost:8080").unwrap();
        assert!(!u.https);
        assert_eq!(
            (u.host.as_str(), u.port, u.target.as_str()),
            ("localhost", 8080, "/")
        );
        assert_eq!(u.authority(), "localhost:8080");

        let u =
            Url::parse("https://gitlab.com/api/v4/projects/a%2Fb/repository/archive.tar.gz?sha=d")
                .unwrap();
        assert_eq!(
            u.target,
            "/api/v4/projects/a%2Fb/repository/archive.tar.gz?sha=d"
        );

        assert!(Url::parse("ftp://x/y").is_err());
        assert!(Url::parse("nonsense").is_err());
    }

    #[test]
    fn redirects_resolve() {
        assert_eq!(
            resolve(
                "https://api.github.com/repos/a/b/tarball/c",
                "https://codeload.github.com/a/b/tar.gz/c"
            ),
            "https://codeload.github.com/a/b/tar.gz/c"
        );
        assert_eq!(
            resolve("https://x.example/a/b?q=1", "/c/d"),
            "https://x.example/c/d"
        );
        assert_eq!(
            resolve("https://x.example/a/b", "c"),
            "https://x.example/a/c"
        );
        // a `://` in a query is not a scheme, and `.` and `..` are applied
        assert_eq!(
            resolve("https://x.example/a/b", "/ok.tar?next=http://y.invalid/x"),
            "https://x.example/ok.tar?next=http://y.invalid/x"
        );
        assert_eq!(
            resolve("https://x.example/a/b", "../ok.tar"),
            "https://x.example/ok.tar"
        );
        assert_eq!(
            resolve("https://x.example/a/b", "./d/"),
            "https://x.example/a/d/"
        );
    }

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
        reply: impl Fn(&str, &mut std::net::TcpStream) + Send + 'static,
    ) -> (u16, std::sync::mpsc::Receiver<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
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

    fn respond(c: &mut std::net::TcpStream, status: &str, extra: &str, body: &[u8]) {
        let len = body.len();
        let head = format!("HTTP/1.1 {status}\r\n{extra}content-length: {len}\r\n\r\n");
        let _ = c.write_all(head.as_bytes());
        let _ = c.write_all(body);
    }

    fn limits() -> Limits {
        Limits {
            connect: Some(Duration::from_secs(5)),
            stall: Some(Duration::from_secs(5)),
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

    #[test]
    fn redirects_keep_credentials_only_on_the_same_origin() {
        let u = |s: &str| Url::parse(s).unwrap();
        let first = u("https://api.github.com/a");
        assert_eq!(
            redirect(&first, &first, &u("https://API.github.com/b")),
            Ok(true)
        );
        let other = u("https://codeload.github.com/b");
        assert_eq!(redirect(&first, &first, &other), Ok(false));
        assert_eq!(
            redirect(&first, &other, &u("https://api.github.com/c")),
            Ok(true)
        );
        assert_eq!(
            redirect(&first, &first, &u("https://api.github.com:8443/")),
            Ok(false)
        );
        assert!(redirect(&first, &first, &u("http://api.github.com/b")).is_err());
        let plain = u("http://x.example/a");
        assert_eq!(
            redirect(&plain, &plain, &u("https://x.example/a")),
            Ok(false)
        );
    }

    #[test]
    fn a_stalled_download_fails() {
        // the server holds the connection far past any test run, so only the
        // stall limit can end the call; a wall-clock bound here fails on a
        // loaded machine and a short hold lets the server's close win the race
        let (port, _) = serve(1, |_, c| {
            let _ = c.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nhalf");
            std::thread::sleep(Duration::from_secs(3600));
        });
        let l = Limits {
            stall: Some(Duration::from_secs(1)),
            ..limits()
        };
        let url = format!("http://127.0.0.1:{port}/");
        let e = get_decompressed(&url, &[], l, &mut Vec::new()).unwrap_err();
        assert!(e.to_string().contains("stalled"), "{e}");
    }

    fn file_url(name: &str, bytes: &[u8]) -> String {
        let f = std::env::temp_dir().join(format!("iets-http-{}-{name}", std::process::id()));
        std::fs::write(&f, bytes).unwrap();
        format!("file://{}", f.display())
    }

    #[test]
    fn downloads_stop_at_the_size_limit() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&vec![0; 4 << 20]).unwrap();
        let url = file_url("size.gz", &gz.finish().unwrap());
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
        let n = std::sync::atomic::AtomicUsize::new(0);
        let (port, _) = serve(3, move |_, c| {
            let i = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        let url = file_url("bomb.zst", &bomb);
        let e = get_decompressed(&url, &[], limits(), &mut std::io::sink()).unwrap_err();
        assert!(e.to_string().contains("times its size"), "{e}");
    }
}
