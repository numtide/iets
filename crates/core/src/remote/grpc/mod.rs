//! The `grpc://` transport: nix-grpc-store (github.com/Mic92/nix-grpc-store).
//!
//! The evaluator stays synchronous, so one tokio runtime lives inside `Client`
//! and every call is a `block_on`. See `docs/reference/builders.md`.

pub mod backend;
pub mod build;
pub mod drvwire;
pub mod health;
pub mod sched;

#[allow(clippy::all, clippy::pedantic)]
pub mod proto {
    tonic::include_proto!("nix.remote");
}

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{error, fs, result, thread};

use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use crate::error::{Error, Result};
use crate::fetch::roots::roots;

use proto::nix_remote_client::NixRemoteClient;
use proto::scheduler_client::SchedulerClient;

use super::worker;

/// The `grpc://` query parameters, with the same names and defaults as
/// nix-grpc-store's `GrpcStoreConfig` settings (src/client/store.hh).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrpcConfig {
    /// `host:port`, the URL authority.
    pub authority: String,
    /// `insecure`: plaintext instead of TLS. Local testing only.
    pub insecure: bool,
    /// `connect-timeout`, seconds.
    pub connect_timeout: u64,
    /// `system`: the `x-nix-system` header on every call.
    pub system: String,
    /// `ca-cert`: PEM file with the CA that verifies the server.
    pub ca_cert: Option<PathBuf>,
    /// `client-cert` / `client-key`: the mTLS client identity.
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    /// `token-file`: an OIDC bearer token, re-read for every call.
    pub token_file: Option<PathBuf>,
    /// `reschedule-retries`: how often a build a worker bounced goes back to
    /// the scheduler before it fails.
    pub reschedule_retries: u32,
    /// `restart-grace`, seconds: how long to wait for a scheduler that went
    /// away, and for a worker that can take an `Unplaceable` derivation.
    pub restart_grace: u64,
    /// `max-builds`: concurrent `BuildDerivation` calls.
    pub max_builds: usize,
    /// `nar-connections`: parallel connections for NAR downloads. Accepted for
    /// URL compatibility; this client fetches over the one channel.
    pub nar_connections: u32,
}

/// store.hh: `GrpcStoreConfig::defaultConnectTimeout`.
const DEFAULT_CONNECT_TIMEOUT: u64 = 30;
/// store.hh: `GrpcStoreConfig::defaultRescheduleRetries`.
const DEFAULT_RESCHEDULE_RETRIES: u32 = 8;
/// store.hh: `GrpcStoreConfig::defaultRestartGrace`.
const DEFAULT_RESTART_GRACE: u64 = 120;
/// store.hh: `GrpcStoreConfig::defaultMaxBuilds`.
const DEFAULT_MAX_BUILDS: usize = 64;
/// store.hh: the `nar-connections` setting's default.
const DEFAULT_NAR_CONNECTIONS: u32 = 4;

impl GrpcConfig {
    /// `grpc://host:port?k=v&…`. `None` when the URL is not a `grpc://` one;
    /// `Some(Err)` when it is but a parameter is wrong.
    pub fn parse(url: &str, default_system: &str) -> Option<Result<GrpcConfig>> {
        let rest = url.strip_prefix("grpc://")?;
        Some(GrpcConfig::parse_rest(rest, default_system))
    }

    fn parse_rest(rest: &str, default_system: &str) -> Result<GrpcConfig> {
        let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
        let authority = authority.trim_end_matches('/');
        if authority.is_empty() {
            return Err(Error::Usage("grpc:// needs a host[:port]".into()));
        }
        let mut c = GrpcConfig {
            authority: authority.to_string(),
            insecure: false,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            system: default_system.to_string(),
            ca_cert: None,
            client_cert: None,
            client_key: None,
            token_file: None,
            reschedule_retries: DEFAULT_RESCHEDULE_RETRIES,
            restart_grace: DEFAULT_RESTART_GRACE,
            max_builds: DEFAULT_MAX_BUILDS,
            nar_connections: DEFAULT_NAR_CONNECTIONS,
        };
        for kv in query.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, "1"));
            let v = worker::percent_decode(v);
            let num = |what: &str| -> Result<u64> {
                v.parse::<u64>().map_err(|_| {
                    Error::Usage(format!("grpc://: '{what}' wants a number, got '{v}'"))
                })
            };
            // configuration.cc:310, BaseSetting<bool>::parse: exactly these spellings.
            let flag = |what: &str| -> Result<bool> {
                match v.as_str() {
                    "true" | "1" | "yes" => Ok(true),
                    "false" | "0" | "no" => Ok(false),
                    _ => Err(Error::Usage(format!(
                        "grpc://: '{what}' wants a boolean, got '{v}'"
                    ))),
                }
            };
            match k {
                "insecure" => c.insecure = flag("insecure")?,
                "connect-timeout" => c.connect_timeout = num("connect-timeout")?,
                "system" => c.system = v,
                "ca-cert" => c.ca_cert = Some(PathBuf::from(v)),
                "client-cert" => c.client_cert = Some(PathBuf::from(v)),
                "client-key" => c.client_key = Some(PathBuf::from(v)),
                "token-file" => c.token_file = Some(PathBuf::from(v)),
                "reschedule-retries" => c.reschedule_retries = num("reschedule-retries")? as u32,
                "restart-grace" => c.restart_grace = num("restart-grace")?,
                "max-builds" => c.max_builds = num("max-builds")?.max(1) as usize,
                "nar-connections" => c.nar_connections = num("nar-connections")? as u32,
                // store.hh has it, but it only turns on gRPC's own tracing.
                "debug" => {}
                _ => {
                    return Err(Error::Usage(format!("grpc://: unknown parameter '{k}'")));
                }
            }
        }
        if c.client_cert.is_some() != c.client_key.is_some() {
            return Err(Error::Usage(
                "grpc://: client-cert and client-key must be set together".into(),
            ));
        }
        if c.insecure && c.token_file.is_some() {
            return Err(Error::Usage("grpc://: token-file needs TLS".into()));
        }
        Ok(c)
    }

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

    /// The URL this config came from, for log lines.
    pub fn url(&self) -> String {
        format!("grpc://{}", self.authority)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> GrpcConfig {
        GrpcConfig::parse(url, "x86_64-linux").unwrap().unwrap()
    }

    #[test]
    fn defaults_match_the_cxx_settings() {
        let c = cfg("grpc://farm.numtide.com:50051");
        assert_eq!(c.authority, "farm.numtide.com:50051");
        assert!(!c.insecure);
        assert_eq!(c.connect_timeout, 30);
        assert_eq!(c.system, "x86_64-linux");
        assert_eq!(c.reschedule_retries, 8);
        assert_eq!(c.max_builds, 64);
        assert_eq!(c.nar_connections, 4);
        assert_eq!(c.ca_cert, None);
        assert_eq!(c.token_file, None);
    }

    #[test]
    fn every_parameter_parses() {
        let c = cfg(
            "grpc://farm:50051?system=aarch64-linux&token-file=/run/t&ca-cert=/ca.pem\
             &reschedule-retries=2&max-builds=3&nar-connections=8&connect-timeout=7",
        );
        assert_eq!(c.system, "aarch64-linux");
        assert_eq!(c.token_file, Some(PathBuf::from("/run/t")));
        assert_eq!(c.ca_cert, Some(PathBuf::from("/ca.pem")));
        assert_eq!(c.reschedule_retries, 2);
        assert_eq!(c.max_builds, 3);
        assert_eq!(c.nar_connections, 8);
        assert_eq!(c.connect_timeout, 7);
    }

    #[test]
    fn insecure_is_a_bare_flag_or_a_boolean() {
        assert!(cfg("grpc://127.0.0.1:50051?insecure=1").insecure);
        assert!(cfg("grpc://127.0.0.1:50051?insecure").insecure);
        assert!(cfg("grpc://127.0.0.1:50051?insecure=true").insecure);
        assert!(!cfg("grpc://127.0.0.1:50051?insecure=false").insecure);
    }

    #[test]
    fn mtls_needs_both_halves() {
        let c = cfg("grpc://farm:50051?client-cert=/c.crt&client-key=/c.key");
        assert_eq!(c.client_cert, Some(PathBuf::from("/c.crt")));
        assert!(
            GrpcConfig::parse("grpc://farm:50051?client-cert=/c.crt", "x86_64-linux")
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn a_token_needs_tls_and_unknown_parameters_are_errors() {
        assert!(
            GrpcConfig::parse("grpc://farm:50051?insecure=1&token-file=/t", "x86_64-linux")
                .unwrap()
                .is_err()
        );
        assert!(
            GrpcConfig::parse("grpc://farm:50051?nope=1", "x86_64-linux")
                .unwrap()
                .is_err()
        );
        assert!(GrpcConfig::parse("ssh-ng://host", "x86_64-linux").is_none());
    }

    #[test]
    fn percent_escapes_decode_in_values() {
        let c = cfg("grpc://farm:50051?token-file=%2Frun%2Fkeys%2Ftoken");
        assert_eq!(c.token_file, Some(PathBuf::from("/run/keys/token")));
    }
}
