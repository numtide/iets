//! The `grpc://` store URL: what `build` and `remote` need to know of a gRPC
//! store on every platform, also where the client itself does not exist.

use std::path::PathBuf;

use super::super::worker;
use crate::error::{Error, Result};

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

    /// The URL this config came from, for log lines.
    pub fn url(&self) -> String {
        format!("grpc://{}", self.authority)
    }
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
