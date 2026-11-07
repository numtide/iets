//! The `grpc://` transport: nix-grpc-store (github.com/Mic92/nix-grpc-store).
//!
//! The evaluator stays synchronous, so one tokio runtime lives inside `Client`
//! and every call is a `block_on`. See `docs/reference/builders.md`.

pub mod backend;
pub mod build;
pub mod config;
pub mod drvwire;
pub mod health;
pub mod sched;

#[allow(clippy::all, clippy::pedantic)]
pub mod proto {
    tonic::include_proto!("nix.remote");
}

use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};
use std::{error, fs, result, thread};

use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use crate::error::{Error, Result};
use crate::fetch::roots::roots;

use proto::nix_remote_client::NixRemoteClient;
use proto::scheduler_client::SchedulerClient;

pub use config::GrpcConfig;

use crate::build::backend::Backend;

/// The build backend of `cfg`'s store.
pub fn open(cfg: &GrpcConfig) -> Result<Box<dyn Backend>> {
    Ok(Box::new(backend::Grpc::open(cfg)?))
}

impl GrpcConfig {
    /// store.cc:statusError, with the same hints.
    pub fn status_error(&self, s: &tonic::Status, what: &str) -> Error {
        let mut hint = String::new();
        if !self.insecure
            && matches!(
                s.code(),
                tonic::Code::Unauthenticated | tonic::Code::Unavailable
            )
        {
            hint = connect_hint(self, s.message());
        }
        Error::Protocol(format!(
            "gRPC {what} on '{}' failed ({:?}): {}{hint}",
            self.authority,
            s.code(),
            s.message()
        ))
    }

    fn endpoint(&self) -> Result<Endpoint> {
        let scheme = if self.insecure { "http" } else { "https" };
        let mut ep = Endpoint::from_shared(format!("{scheme}://{}", self.authority))
            .map_err(|e| Error::Usage(format!("grpc://{}: {e}", self.authority)))?
            .connect_timeout(Duration::from_secs(self.connect_timeout))
            .tcp_nodelay(true)
            .initial_stream_window_size(8 << 20)
            .initial_connection_window_size(32 << 20);
        if !self.insecure {
            // store.cc:sslOptions. The balancer's own certificate is public, so
            // the shared roots verify it; `ca-cert` adds another CA.
            let mut tls = ClientTlsConfig::new().trust_anchors(roots()?.iter().cloned());
            if let Some(ca) = &self.ca_cert {
                tls = tls.ca_certificate(Certificate::from_pem(read(ca)?));
            }
            if let (Some(cert), Some(key)) = (&self.client_cert, &self.client_key) {
                tls = tls.identity(Identity::from_pem(read(cert)?, read(key)?));
            }
            ep = ep
                .tls_config(tls)
                .map_err(|e| Error::Protocol(format!("grpc://{}: TLS: {e}", self.authority)))?;
        }
        Ok(ep)
    }
}

/// `tonic::transport::Error` prints only "transport error"; the reason (a TLS
/// alert, a refused connection) is in its source chain.
fn causes(e: &dyn error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(e) = src {
        let next = e.to_string();
        if !s.contains(&next) {
            s.push_str(": ");
            s.push_str(&next);
        }
        src = e.source();
    }
    s
}

fn read(p: &Path) -> Result<Vec<u8>> {
    fs::read(p).map_err(|e| Error::io(format!("cannot read '{}'", p.display()), e))
}

/// What `StoreInfo` reported.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreInfo {
    pub trusted: Option<bool>,
}

/// A connected `grpc://` store: one channel, one runtime, the headers every
/// call carries.
pub struct Client {
    pub cfg: GrpcConfig,
    pub rt: tokio::runtime::Runtime,
    pub stub: NixRemoteClient<Channel>,
    pub sched: SchedulerClient<Channel>,
    pub info: StoreInfo,
}

/// tonic caps decoded messages at 4 MiB by default; NAR frames are larger.
const MAX_MESSAGE: usize = 256 << 20;

impl Client {
    /// Connects and runs `StoreInfo`, which is what `GrpcStore::connect` does.
    pub fn open(cfg: &GrpcConfig) -> Result<Client> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| Error::io("cannot start a tokio runtime", e))?;
        let ep = cfg.endpoint()?;
        let channel = rt
            .block_on(ep.connect())
            .map_err(|e| connect_error(cfg, &causes(&e)))?;
        let mut c = Client {
            cfg: cfg.clone(),
            rt,
            sched: SchedulerClient::new(channel.clone()),
            stub: NixRemoteClient::new(channel)
                .max_decoding_message_size(MAX_MESSAGE)
                .max_encoding_message_size(MAX_MESSAGE),
            info: StoreInfo::default(),
        };
        let reply = c.call("StoreInfo", |mut stub, h| async move {
            stub.store_info(c_request(proto::StoreInfoRequest {}, &h))
                .await
        })?;
        c.info = StoreInfo {
            trusted: reply.trusted,
        };
        Ok(c)
    }

    /// One unary call with the store's fixed headers and the `UNAVAILABLE`
    /// retry loop of `GrpcStore::retrying`: the first call waits
    /// `connect-timeout` for the service, later ones get `RESTART_GRACE`.
    pub fn call<T, F, Fut>(&mut self, what: &str, mut attempt: F) -> Result<T>
    where
        F: FnMut(NixRemoteClient<Channel>, Headers) -> Fut,
        Fut: Future<Output = result::Result<tonic::Response<T>, tonic::Status>>,
    {
        let deadline = Instant::now() + Duration::from_secs(self.cfg.connect_timeout);
        let mut pause = Duration::from_millis(500);
        loop {
            let stub = self.stub.clone();
            let headers = self.headers()?;
            let r = self.rt.block_on(attempt(stub, headers));
            match r {
                Ok(resp) => return Ok(resp.into_inner()),
                Err(s) => {
                    if !gone_away(&s) || Instant::now() + pause > deadline {
                        return Err(self.status_error(&s, what));
                    }
                    errln!(
                        "{}: {}, retrying",
                        self.cfg.authority,
                        first_line(s.message())
                    );
                    thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_secs(4));
                }
            }
        }
    }

    pub fn status_error(&self, s: &tonic::Status, what: &str) -> Error {
        self.cfg.status_error(s, what)
    }

    /// The headers `StaticHeaders` stamps on every call plus the per-call
    /// bearer token, which the C++ `TokenFileCredentials` re-reads each time.
    pub fn headers(&self) -> Result<Headers> {
        let mut h = Vec::new();
        if !self.cfg.system.is_empty() {
            h.push(("x-nix-system", self.cfg.system.clone()));
        }
        if let Some(path) = &self.cfg.token_file {
            let token = String::from_utf8_lossy(&read(path)?).trim().to_string();
            if token.is_empty() || token.contains(['\r', '\n']) {
                return Err(Error::Usage(format!(
                    "token file '{}' is empty or multi-line",
                    path.display()
                )));
            }
            h.push(("authorization", format!("Bearer {token}")));
        }
        Ok(h)
    }
}

/// Metadata this client sets: `x-nix-system` and `authorization` on every
/// call, plus `x-nix-features` and `x-nix-worker` on a build.
pub type Headers = Vec<(&'static str, String)>;

/// A request with `headers` in its initial metadata.
pub fn c_request<T>(body: T, headers: &[(&'static str, String)]) -> tonic::Request<T> {
    let mut req = tonic::Request::new(body);
    put_headers(req.metadata_mut(), headers);
    req
}

fn put_headers(md: &mut MetadataMap, headers: &[(&'static str, String)]) {
    for (k, v) in headers {
        if let Ok(v) = MetadataValue::try_from(v.as_str()) {
            md.insert(*k, v);
        }
    }
}

/// store.cc:transportError: gRPC folds every connect failure into
/// `UNAVAILABLE`, only the text tells TCP from TLS apart.
fn transport_error(msg: &str) -> bool {
    [
        "Connection refused",
        "Connection reset",
        "No route",
        "unreachable",
        "timed out",
        "DNS",
        "GOAWAY",
        "error trying to connect",
        "tcp connect error",
    ]
    .iter()
    .any(|n| msg.contains(n))
}

/// store.cc:goneAway: a worker or the balancer restarting under us.
pub fn gone_away(s: &tonic::Status) -> bool {
    match s.code() {
        tonic::Code::Unavailable => {
            transport_error(s.message())
                || s.message().contains("upstream")
                || s.message().contains("shutting down")
        }
        tonic::Code::Cancelled => s.message().contains("Cancelling all calls"),
        _ => false,
    }
}

pub fn first_line(msg: &str) -> &str {
    msg.split('\n').next().unwrap_or(msg)
}

fn connect_hint(cfg: &GrpcConfig, msg: &str) -> String {
    if transport_error(msg) {
        return "\nhint: could not reach the server (TCP/DNS), not a certificate problem."
            .to_string();
    }
    if [
        "handshake",
        "Handshake",
        "SSL",
        "TLS",
        "certificate",
        "Socket closed",
    ]
    .iter()
    .any(|n| msg.contains(n))
    {
        if cfg.client_cert.is_none() && cfg.token_file.is_none() {
            return "\nhint: TLS handshake failed and no client certificate was presented. \
                    Set 'client-cert'/'client-key' or 'token-file'."
                .to_string();
        }
        return "\nhint: TLS handshake failed: the server certificate is not trusted by our CA \
                bundle, the server rejected our client certificate, or the peer is not \
                speaking TLS."
            .to_string();
    }
    String::new()
}

fn connect_error(cfg: &GrpcConfig, msg: &str) -> Error {
    Error::Protocol(format!(
        "cannot connect to '{}': {msg}{}",
        cfg.url(),
        connect_hint(cfg, msg)
    ))
}
