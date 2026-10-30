//! The CA roots of both TLS clients: `NIX_SSL_CERT_FILE`, else `SSL_CERT_FILE`
//! (Nix 2.34.8, libstore/filetransfer.cc), else the platform store, else the
//! Mozilla set compiled in.

use std::env;
use std::ffi::OsString;
use std::fmt::Display;
use std::path::Path;
use std::result;
use std::sync::OnceLock;

use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, TrustAnchor};

use crate::error::{Error, Result};

pub fn roots() -> Result<&'static [TrustAnchor<'static>]> {
    static ROOTS: OnceLock<result::Result<Vec<TrustAnchor<'static>>, String>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            choose(
                |v| env::var_os(v),
                || rustls_native_certs::load_native_certs().certs,
            )
        })
        .as_deref()
        .map_err(|e| Error::Config(e.clone()))
}

/// The first variable that is set decides, even when empty: an empty
/// `NIX_SSL_CERT_FILE` hides `SSL_CERT_FILE`, as in Nix. A named file that
/// cannot be read is an error, never a fallback.
fn choose(
    var: impl Fn(&str) -> Option<OsString>,
    platform: impl FnOnce() -> Vec<CertificateDer<'static>>,
) -> result::Result<Vec<TrustAnchor<'static>>, String> {
    let named = ["NIX_SSL_CERT_FILE", "SSL_CERT_FILE"]
        .into_iter()
        .find_map(|v| var(v).map(|p| (v, p)));
    if let Some((v, path)) = named.filter(|(_, p)| !p.is_empty()) {
        let path = Path::new(&path);
        let at = |e: &dyn Display| format!("{v}={}: {e}", path.display());
        let anchors = anchors(read(path).map_err(|e| at(&e))?);
        if anchors.is_empty() {
            return Err(at(&"no CA certificate in the file"));
        }
        return Ok(anchors);
    }
    let found = anchors(platform());
    if found.is_empty() {
        return Ok(webpki_roots::TLS_SERVER_ROOTS.to_vec());
    }
    Ok(found)
}

fn read(path: &Path) -> result::Result<Vec<CertificateDer<'static>>, String> {
    CertificateDer::pem_file_iter(path)
        .map_err(|e| e.to_string())?
        .collect::<result::Result<_, _>>()
        .map_err(|e| e.to_string())
}

fn anchors(certs: Vec<CertificateDer<'static>>) -> Vec<TrustAnchor<'static>> {
    let mut store = RootCertStore::empty();
    store.add_parsable_certificates(certs);
    store.roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    use std::fs;
    use std::path::PathBuf;

    const CA: &str = "-----BEGIN CERTIFICATE-----
MIIBiTCCAS+gAwIBAgIUYqdid7LEUqasdYlUjSL8+CUCJwQwCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwOaWV0cyB0ZXN0IHJvb3QwIBcNMjYxMDA5MTYyNDU2WhgPMjEy
NjA5MTUxNjI0NTZaMBkxFzAVBgNVBAMMDmlldHMgdGVzdCByb290MFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAEngrQS1SJb7q35MOe2l5vde337hrZfbAvFJxNXpNn
I77sXiamR0/gx0GP+PqfgF6RvwpwoDvWZZ1PbwlJeAoQr6NTMFEwHQYDVR0OBBYE
FD5ctw4e54co56kh/n93SxbYz2xYMB8GA1UdIwQYMBaAFD5ctw4e54co56kh/n93
SxbYz2xYMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIgPNEswBcf
yBSVTKb+pHkI+2FnxMJ3rAqDQY8+ZxT68DUCIQD4BHWTRVy6D9ngnltoK5cfyk3R
xaH3eJUFJGliZRO5NA==
-----END CERTIFICATE-----
";

    fn file(d: &Path, name: &str, text: &str) -> PathBuf {
        let f = d.join(name);
        fs::write(&f, text).unwrap();
        f
    }

    fn vars(set: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> {
        let set: Vec<(String, OsString)> = set
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_os_str().to_owned()))
            .collect();
        move |k| set.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    fn ca_der() -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(CA.as_bytes()).unwrap()
    }

    #[test]
    fn no_bundle_anywhere_gives_the_compiled_in_set() {
        let got = choose(vars(&[]), Vec::new).unwrap();
        assert_eq!(got.len(), webpki_roots::TLS_SERVER_ROOTS.len());
    }

    #[test]
    fn the_platform_store_wins_over_the_compiled_in_set() {
        let got = choose(vars(&[]), || vec![ca_der()]).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn nix_ssl_cert_file_wins_over_ssl_cert_file_and_the_platform() {
        let d = tempdir().unwrap();
        let ca = file(d.path(), "ca.pem", CA);
        let none = file(d.path(), "none.pem", "");
        let got = choose(
            vars(&[("NIX_SSL_CERT_FILE", &ca), ("SSL_CERT_FILE", &none)]),
            Vec::new,
        )
        .unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn an_empty_nix_ssl_cert_file_hides_ssl_cert_file() {
        let d = tempdir().unwrap();
        let none = file(d.path(), "hidden.pem", "");
        let got = choose(
            vars(&[
                ("NIX_SSL_CERT_FILE", Path::new("")),
                ("SSL_CERT_FILE", &none),
            ]),
            || vec![ca_der()],
        )
        .unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn a_named_file_that_is_missing_or_empty_is_an_error() {
        let missing = choose(
            vars(&[("SSL_CERT_FILE", Path::new("/nonexistent/ca.pem"))]),
            Vec::new,
        );
        assert!(
            missing
                .unwrap_err()
                .starts_with("SSL_CERT_FILE=/nonexistent/ca.pem: ")
        );
        let d = tempdir().unwrap();
        let empty = file(d.path(), "empty.pem", "");
        let got = choose(vars(&[("NIX_SSL_CERT_FILE", &empty)]), Vec::new);
        assert!(got.unwrap_err().ends_with("no CA certificate in the file"));
    }
}
