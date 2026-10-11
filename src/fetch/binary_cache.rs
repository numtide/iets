//! Reading an HTTP binary cache, as binary-cache-store.cc does for
//! `builtins.fetchClosure`: `nix-cache-info`, a path's `.narinfo`, and its NAR
//! undone from the compression the `.narinfo` names. Nothing is written to
//! the cache, and nothing here checks a signature: the store that receives
//! the paths does (`crate::builtins::fetch_closure`).

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};

use crate::error::{Error, Result};
use crate::fetch::{http, spool};
use crate::remote::grpc::drvwire::PathInfo;
use crate::remote::worker::wire;
use crate::store::hash::{Hash, HashAlgo, Hasher};
use crate::store::path::{STORE_DIR, StorePath};

/// A path's `.narinfo`: its path info as the worker protocol carries it, and
/// where its NAR is.
#[derive(Clone, Debug)]
pub struct NarInfo {
    pub info: PathInfo,
    pub url: String,
    pub compression: String,
}

pub struct BinaryCache {
    url: String,
    limits: http::Limits,
    opened: bool,
}

impl BinaryCache {
    /// `url` is `http://`, `https://` or `file://`, without query parameters.
    pub fn new(url: &str, limits: http::Limits) -> BinaryCache {
        BinaryCache {
            url: url.trim_end_matches('/').to_string(),
            limits,
            opened: false,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn file(&self, rel: &str, sink: &mut dyn Write) -> Result<bool> {
        let url = format!("{}/{rel}", self.url);
        Ok(http::get_if_found(&url, &[], self.limits, sink)?.is_some())
    }

    /// binary-cache-store.cc:init and http-binary-cache-store.cc:init: the
    /// cache must say it is one, for this store directory.
    fn open(&mut self) -> Result<()> {
        if self.opened {
            return Ok(());
        }
        let mut info = Vec::new();
        if !self.file("nix-cache-info", &mut info)? {
            return Err(Error::Store(format!(
                "'{}' does not appear to be a binary cache",
                self.url
            )));
        }
        for line in String::from_utf8_lossy(&info).lines() {
            if let Some((name, value)) = line.split_once(':')
                && name == "StoreDir"
                && value.trim() != STORE_DIR
            {
                return Err(Error::Store(format!(
                    "binary cache '{}' is for Nix stores with prefix '{}', not '{STORE_DIR}'",
                    self.url,
                    value.trim()
                )));
            }
        }
        self.opened = true;
        Ok(())
    }

    /// The `.narinfo` of `sp`, or `None` when the cache has not got it
    /// (store-api.cc:queryPathInfo, which also refuses one for another path).
    pub fn nar_info(&mut self, sp: &StorePath) -> Result<Option<NarInfo>> {
        self.open()?;
        let whence = format!("{}.narinfo", sp.hash_part());
        let mut text = Vec::new();
        if !self.file(&whence, &mut text)? {
            return Ok(None);
        }
        let ni = parse(&String::from_utf8_lossy(&text), &whence)?;
        Ok((ni.info.path == *sp).then_some(ni))
    }

    /// The NAR of `ni`, decompressed into a scratch file and checked against
    /// the `NarHash` and `NarSize` it promised, rewound.
    pub fn nar(&mut self, ni: &NarInfo) -> Result<File> {
        let path = &ni.info.path;
        let mut raw = spool::scratch("nar")?;
        if !self.file(&ni.url, &mut raw)? {
            return Err(Error::Store(format!(
                "file '{}' does not exist in binary cache '{}'",
                ni.url, self.url
            )));
        }
        let cannot = |e: io::Error| Error::io(format!("cannot read the NAR of '{path}'"), e);
        raw.seek(SeekFrom::Start(0)).map_err(cannot)?;
        // one byte past the promise is enough to refuse it, so a bomb stops there
        let mut from = decoder(&ni.compression, Box::new(BufReader::new(raw)), path)?
            .take(ni.info.nar_size + 1);
        let mut out = spool::scratch("nar")?;
        let mut hasher = Hasher::new(HashAlgo::Sha256);
        let mut buf = vec![0; 64 << 10];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(cannot(e)),
            };
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(cannot)?;
        }
        let size = hasher.bytes_written();
        let got = hasher.finish();
        if size != ni.info.nar_size || got.to_base16() != ni.info.nar_hash {
            return Err(Error::Store(format!(
                "the NAR of '{path}' in binary cache '{}' does not match its narinfo: \
                 it is {size} bytes with hash 'sha256:{}', not {} bytes with hash 'sha256:{}'",
                self.url,
                got.to_nix32(),
                ni.info.nar_size,
                Hash::from_base16(HashAlgo::Sha256, &ni.info.nar_hash)
                    .map(|h| h.to_nix32())
                    .unwrap_or_default(),
            )));
        }
        out.seek(SeekFrom::Start(0)).map_err(cannot)?;
        Ok(out)
    }
}

/// compression.cc:makeDecompressionSink for the methods iets has.
fn decoder(method: &str, r: Box<dyn Read>, path: &StorePath) -> Result<Box<dyn Read>> {
    Ok(match method {
        "none" | "" => r,
        "xz" => Box::new(lzma_rust2::XzReader::new(r, true)),
        "bzip2" => Box::new(bzip2::read::MultiBzDecoder::new(r)),
        "gzip" => Box::new(flate2::read::MultiGzDecoder::new(r)),
        "zstd" => Box::new(
            zstd::stream::read::Decoder::new(r)
                .map_err(|e| Error::io("cannot start the zstd decoder", e))?,
        ),
        other => {
            return Err(Error::Unsupported(format!(
                "iets: the NAR of '{path}' is compressed with '{other}', which iets cannot undo"
            )));
        }
    })
}

/// nar-info.cc:NarInfo::NarInfo. `whence` names the file in errors.
pub fn parse(s: &str, whence: &str) -> Result<NarInfo> {
    let mut line = 1;
    let corrupt = |reason: &str, line: usize| {
        let at = if line > 0 {
            format!(" at line {line}")
        } else {
            String::new()
        };
        Error::Store(format!("NAR info file '{whence}' is corrupt: {reason}{at}"))
    };
    let hash =
        |v: &str, line: usize| Hash::parse_any(v, None).map_err(|_| corrupt("bad hash", line));
    let store_path =
        |v: &str, line: usize| StorePath::from_base_name(v).map_err(|e| corrupt(&e.0, line));
    let mut path = None;
    let mut nar_hash = None;
    let mut url = String::new();
    let mut compression = String::new();
    let mut nar_size = 0;
    let mut references: Vec<String> = Vec::new();
    let mut deriver = String::new();
    let mut sigs = Vec::new();
    let mut ca: Option<String> = None;
    let mut pos = 0;
    while pos < s.len() {
        let colon = s[pos..]
            .find(':')
            .map(|i| pos + i)
            .ok_or_else(|| corrupt("expecting ':'", line))?;
        let name = &s[pos..colon];
        let eol = s
            .get(colon + 2..)
            .and_then(|r| r.find('\n'))
            .map(|i| colon + 2 + i)
            .ok_or_else(|| corrupt("expecting '\\n'", line))?;
        let value = &s[colon + 2..eol];
        match name {
            "StorePath" => {
                let p = StorePath::parse(value).map_err(|e| corrupt(&e.0, line))?;
                path = Some(p);
            }
            "URL" => url = value.to_string(),
            "Compression" => compression = value.to_string(),
            "FileHash" => {
                hash(value, line)?;
            }
            "FileSize" => {
                value
                    .parse::<u64>()
                    .map_err(|_| corrupt("invalid FileSize", line))?;
            }
            "NarHash" => nar_hash = Some(hash(value, line)?),
            "NarSize" => {
                nar_size = value
                    .parse::<u64>()
                    .map_err(|_| corrupt("invalid NarSize", line))?
            }
            "References" => {
                if !references.is_empty() {
                    return Err(corrupt("extra References", line));
                }
                for r in value.split(' ').filter(|r| !r.is_empty()) {
                    references.push(store_path(r, line)?.to_string());
                }
            }
            "Deriver" if value != "unknown-deriver" => {
                deriver = store_path(value, line)?.to_string();
            }
            "Sig" => sigs.push(wire::signature(value).map_err(|e| corrupt(&e.to_string(), line))?),
            "CA" => {
                if ca.is_some() {
                    return Err(corrupt("extra CA", line));
                }
                ca = Some(wire::content_address(value).map_err(|e| corrupt(&e.to_string(), line))?);
            }
            _ => {}
        }
        pos = eol + 1;
        line += 1;
    }
    if compression.is_empty() {
        compression = "bzip2".into();
    }
    let missing = if path.is_none() {
        Some("StorePath missing")
    } else if nar_hash.is_none() {
        Some("NarHash missing")
    } else if url.is_empty() {
        Some("URL missing")
    } else if nar_size == 0 {
        Some("NarSize missing or zero")
    } else {
        None
    };
    if let Some(why) = missing {
        return Err(corrupt(why, 0));
    }
    let (Some(path), Some(nar_hash)) = (path, nar_hash) else {
        unreachable!("checked above")
    };
    references.sort();
    sigs.sort();
    sigs.dedup();
    Ok(NarInfo {
        info: PathInfo {
            path,
            deriver,
            nar_hash: nar_hash.to_base16(),
            references,
            registration_time: 0,
            nar_size,
            ultimate: false,
            sigs,
            ca: ca.unwrap_or_default(),
        },
        url,
        compression,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &str = "StorePath: /nix/store/lcxc2z1h2g8wq8dxfpizy1di85kyxnqx-hello-2.12.3
URL: nar/174fba8vinrah6iyhp92kmz7r5kx6habnc4qb1rs0657sx224zvn.nar.xz
Compression: xz
FileHash: sha256:174fba8vinrah6iyhp92kmz7r5kx6habnc4qb1rs0657sx224zvn
FileSize: 58484
NarHash: sha256:1n71v0lypd23hmqq1zbhzcd9v49r8g6n2f0hsyfv99pkgmajraw2
NarSize: 279624
References: 0d8g8n0a11v6f5m2h416ajyxmnkwc3md-glibc-2.42-67 lcxc2z1h2g8wq8dxfpizy1di85kyxnqx-hello-2.12.3
Deriver: a41gq8gxdhd2haqk5ij6ak4ybwmignrp-hello-2.12.3.drv
Sig: niks3.numtide.com-1:lIxEzaoWG6hMHP458gMAK85dtGdz+lwsN850uEvAnQ+IzdZX3IpQzeTP1+GseoFQiR+W60vMl8InhaBPHehcAA==
";

    #[test]
    fn a_narinfo_becomes_a_path_info() {
        let ni = parse(HELLO, "x.narinfo").unwrap();
        assert_eq!(
            ni.info.path.to_string(),
            "/nix/store/lcxc2z1h2g8wq8dxfpizy1di85kyxnqx-hello-2.12.3"
        );
        assert_eq!(ni.compression, "xz");
        assert_eq!(ni.info.nar_size, 279624);
        assert_eq!(
            ni.info.references,
            [
                "/nix/store/0d8g8n0a11v6f5m2h416ajyxmnkwc3md-glibc-2.42-67",
                "/nix/store/lcxc2z1h2g8wq8dxfpizy1di85kyxnqx-hello-2.12.3"
            ]
        );
        assert_eq!(
            ni.info.deriver,
            "/nix/store/a41gq8gxdhd2haqk5ij6ak4ybwmignrp-hello-2.12.3.drv"
        );
        assert_eq!(ni.info.sigs.len(), 1);
        assert_eq!(ni.info.ca, "");
        assert_eq!(ni.info.nar_hash.len(), 64);
    }

    #[test]
    fn a_missing_field_is_corrupt() {
        let e = parse(
            "StorePath: /nix/store/lcxc2z1h2g8wq8dxfpizy1di85kyxnqx-hello\n",
            "h.narinfo",
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "NAR info file 'h.narinfo' is corrupt: NarHash missing"
        );
        let e = parse("StorePath /nix/store/x\n", "h.narinfo").unwrap_err();
        assert!(e.to_string().ends_with("expecting ':' at line 1"), "{e}");
    }

    #[test]
    fn compression_defaults_to_bzip2() {
        let s = HELLO.replace("Compression: xz\n", "");
        assert_eq!(parse(&s, "x").unwrap().compression, "bzip2");
    }

    #[test]
    fn a_ca_field_is_normalised() {
        let s = format!(
            "{HELLO}CA: fixed:r:sha256:1n71v0lypd23hmqq1zbhzcd9v49r8g6n2f0hsyfv99pkgmajraw2\n"
        );
        assert_eq!(
            parse(&s, "x").unwrap().info.ca,
            "fixed:r:sha256:1n71v0lypd23hmqq1zbhzcd9v49r8g6n2f0hsyfv99pkgmajraw2"
        );
    }
}
