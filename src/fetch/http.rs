//! The one HTTP client iets has: a streaming GET, for the fetch builtins
//! (`builtins::fetch`). hyper over tokio-rustls, the stack tonic already
//! brings; no libcurl. Redirects are followed like
//! filetransfer.cc (`CURLOPT_FOLLOWLOCATION`, `CURLOPT_MAXREDIRS` 10) and
//! `file://` reads the local file, as libcurl does for the `tarball` inputs
//! `nix flake lock` writes with that scheme. Credentials follow a redirect only
//! as libcurl lets them: see `redirect`.
//!
//! The body is never held whole: hyper runs on a thread of its own and hands
//! its frames to a bounded queue, the caller's thread reads that queue through
//! the decompressor, and both ends stop when the other does.

use std::cell::Cell;
use std::error::Error as StdError;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::future::Future;
#[cfg(test)]
use std::io::Cursor;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::{mem, result, thread};

use crate::config;
use crate::error::{Error, Result};
use crate::fetch::roots;
use crate::log::{self, ACT_FILE_TRANSFER, Activity, LVL_TALKATIVE, RES_PROGRESS};
use crate::remote::worker;
use crate::session::Session;
use crate::store::hash;
use crate::sys;
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

/// `fut`, or `why` once `limit` has passed.
async fn within<T>(
    limit: Option<Duration>,
    why: impl FnOnce() -> Error,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    match limit {
        None => fut.await,
        Some(d) => tokio::time::timeout(d, fut).await.map_err(|_| why())?,
    }
}

/// The socket under TLS and hyper, with `stalled-download-timeout`: a read
/// that has waited `limit` fails with [`Stalled`] only if the kernel holds no
/// byte for it either. Like curl, which tries the socket before
/// `Curl_speedcheck`, a read is tried before the clock, so the time the
/// process was stopped or not scheduled is no stall.
struct Watched {
    tcp: tokio::net::TcpStream,
    limit: Option<Duration>,
    wait: Option<Pin<Box<tokio::time::Sleep>>>,
}

/// The error under hyper's when [`Watched`] gives up.
#[derive(Debug)]
struct Stalled;

impl Display for Stalled {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("no byte arrived within stalled-download-timeout")
    }
}

impl StdError for Stalled {}

/// Whether `e` or one of its causes is [`Stalled`].
fn is_stall(e: &(dyn StdError + 'static)) -> bool {
    let mut at = Some(e);
    while let Some(e) = at {
        let ours = e.downcast_ref::<io::Error>().and_then(|e| e.get_ref());
        if ours.is_some_and(|e| e.is::<Stalled>()) {
            return true;
        }
        at = e.source();
    }
    false
}

/// tokio drops the readiness of an `epoll_wait` that SIGCONT interrupts and
/// still fires the timers that the stop overran, so the kernel decides. No
/// `MSG_DONTWAIT`, which Windows lacks: tokio sockets are nonblocking.
fn arrived(tcp: &tokio::net::TcpStream) -> bool {
    let peek = rustix::net::RecvFlags::PEEK;
    rustix::net::recv(tcp, &mut [0u8; 1][..], peek) != Err(rustix::io::Errno::WOULDBLOCK)
}

impl tokio::io::AsyncRead for Watched {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let read = Pin::new(&mut this.tcp).poll_read(cx, buf);
        let Some(limit) = this.limit.filter(|_| read.is_pending()) else {
            this.wait = None;
            return read;
        };
        let wait = (this.wait).get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        loop {
            if tokio::time::Instant::now() >= wait.deadline() {
                if !arrived(&this.tcp) {
                    return Poll::Ready(Err(io::Error::new(ErrorKind::TimedOut, Stalled)));
                }
                wait.as_mut().reset(tokio::time::Instant::now() + limit);
            }
            if wait.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
        }
    }
}

impl tokio::io::AsyncWrite for Watched {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().tcp).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().tcp).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().tcp).poll_shutdown(cx)
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
            Some((u, h)) => (Some(worker::percent_decode(u)), h),
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
        let b64 = hash::base64_encode(u.as_bytes());
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
fn redirect(first: &Url, from: &Url, to: &Url) -> result::Result<bool, &'static str> {
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

/// Process-wide on purpose: it is immutable, costly to build, and holds no
/// run state, so two sessions may share it.
fn tls() -> Result<Arc<rustls::ClientConfig>> {
    static TLS: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    if let Some(cfg) = TLS.get() {
        return Ok(cfg.clone());
    }
    let roots = rustls::RootCertStore::from_iter(roots::roots()?.iter().cloned());
    let mut cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TLS.get_or_init(|| Arc::new(cfg)).clone())
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
    if let Some(path) = local_path(url) {
        let file = File::open(sys::path_from_internal(path.as_bytes()))
            .map_err(|e| Error::io(format!("cannot read '{path}'"), e))?;
        let raw = Counted(Box::new(file), read_in.clone());
        return drain(url, undo(Box::new(raw))?, &read_in, limits, sink);
    }
    let (tx, rx) = tokio::sync::mpsc::channel(QUEUE);
    let target = url.to_string();
    let sent = headers.to_vec();
    let download = thread::spawn(move || -> Result<()> {
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
    rx: tokio::sync::mpsc::Receiver<result::Result<Vec<u8>, String>>,
    at: Vec<u8>,
    pos: usize,
}

impl Read for Queued {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.at.len() {
            match self.rx.blocking_recv() {
                None => return Ok(0),
                Some(Err(e)) => return Err(io::Error::other(e)),
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

/// Follows redirects and pushes the frames of the 200 into `tx`.
async fn stream(
    url: &str,
    headers: &[(String, String)],
    limits: Limits,
    tx: tokio::sync::mpsc::Sender<result::Result<Vec<u8>, String>>,
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
                    let next = body.frame().await.transpose().map_err(|e| {
                        if is_stall(&e) {
                            bad(&current, "the download stalled (stalled-download-timeout)")
                        } else {
                            bad(&current, &format!("reading the response failed: {e}"))
                        }
                    });
                    let frame = match next {
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
                log::line(&format!(
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
    tokio::task::JoinHandle<result::Result<(), hyper::Error>>,
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
        let stream = Watched {
            tcp: stream,
            limit: limits.stall,
            wait: None,
        };
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
    let res = sender.send_request(req).await.map_err(|e| {
        if is_stall(&e) {
            bad(url, "no response (stalled-download-timeout)")
        } else {
            bad(url, &format!("request failed: {e}"))
        }
    })?;
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
    use std::future::poll_fn;
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::task::Waker;
    use tokio::io::AsyncRead;

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

    /// The runtime thread blocks past the limit, as SIGSTOP or a busy host
    /// would hold it, with no reactor turn in between: bytes waiting in the
    /// kernel are no stall, and an empty socket is one.
    #[test]
    fn time_the_reader_did_not_run_is_no_stall() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(l.local_addr().unwrap()).unwrap();
            let (mut server, _) = l.accept().unwrap();
            client.set_nonblocking(true).unwrap();
            let limit = Duration::from_millis(1);
            let mut w = Watched {
                tcp: tokio::net::TcpStream::from_std(client).unwrap(),
                limit: Some(limit),
                wait: None,
            };
            let read = |w: &mut Watched, cx: &mut Context<'_>| {
                let mut buf = [0u8; 8];
                let mut rb = tokio::io::ReadBuf::new(&mut buf);
                Pin::new(w)
                    .poll_read(cx, &mut rb)
                    .map_ok(|()| rb.filled().to_vec())
            };
            let poll = |w: &mut Watched| read(w, &mut Context::from_waker(Waker::noop()));

            server.write_all(b"data").unwrap();
            assert!(poll(&mut w).is_pending());
            thread::sleep(limit * 2);
            assert!(poll(&mut w).is_pending());
            let got = poll_fn(|cx| read(&mut w, cx)).await.unwrap();
            assert_eq!(got, b"data");

            let e = loop {
                thread::sleep(limit * 2);
                if let Poll::Ready(r) = poll(&mut w) {
                    break r.unwrap_err();
                }
            };
            assert!(is_stall(&e), "{e}");
        });
    }
}
