//! A binary cache's `.narinfo`: what a path is, what it refers to, where its
//! NAR is and who signed it.

use std::collections::BTreeSet;
use std::fmt;

use ring::signature::{ED25519, UnparsedPublicKey};

use crate::store::hash::{Hash, HashAlgo, HashFormat, base64_decode};
use crate::store::path::{IngestionMethod, StorePath, make_fixed_output_path_self, store_dir};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NarInfo {
    pub path: StorePath,
    pub url: String,
    pub compression: String,
    pub file_size: u64,
    pub nar_hash: Hash,
    pub nar_size: u64,
    /// Without the path itself, which [`NarInfo::self_ref`] records.
    pub references: BTreeSet<StorePath>,
    pub self_ref: bool,
    pub deriver: Option<StorePath>,
    pub sigs: Vec<String>,
    pub ca: Option<String>,
}

/// A path `builtins.fetchClosure` checked, and the cache that has its NAR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    pub store: String,
    pub info: NarInfo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NarInfoError(pub String);

impl fmt::Display for NarInfoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl NarInfo {
    /// Parses `text`, served at `whence`, as nar-info.cc does.
    pub fn parse(text: &str, whence: &str) -> Result<NarInfo, NarInfoError> {
        let corrupt =
            |why: &str| NarInfoError(format!("NAR info file '{whence}' is corrupt: {why}"));
        let path_of = |s: &str| {
            StorePath::parse(s)
                .or_else(|_| StorePath::from_base_name(s))
                .map_err(|e| corrupt(&e.0))
        };
        let mut path = None;
        let mut url = None;
        let mut compression = None;
        let mut file_size = 0;
        let mut nar_hash = None;
        let mut nar_size = None;
        let mut refs = Vec::new();
        let mut deriver = None;
        let mut sigs = Vec::new();
        let mut ca = None;
        for line in text.lines().filter(|l| !l.is_empty()) {
            let (key, value) = line
                .split_once(": ")
                .ok_or_else(|| corrupt(&format!("line '{line}' has no ': '")))?;
            let num = || {
                value
                    .parse::<u64>()
                    .map_err(|_| corrupt(&format!("bad {key}")))
            };
            match key {
                "StorePath" => path = Some(path_of(value)?),
                "URL" => url = Some(value.to_string()),
                "Compression" => compression = Some(value.to_string()),
                "FileSize" => file_size = num()?,
                "NarHash" => {
                    let h = Hash::parse_any(value, None).map_err(|e| corrupt(&e.0))?;
                    nar_hash = Some(h);
                }
                "NarSize" => nar_size = Some(num()?),
                "References" => {
                    refs = value
                        .split_whitespace()
                        .map(path_of)
                        .collect::<Result<_, _>>()?;
                }
                "Deriver" if value != "unknown-deriver" => deriver = Some(path_of(value)?),
                "Sig" => sigs.push(value.to_string()),
                "CA" => ca = Some(value.to_string()),
                _ => {}
            }
        }
        let path = path.ok_or_else(|| corrupt("no StorePath"))?;
        let mut references = BTreeSet::new();
        let mut self_ref = false;
        for r in refs {
            if r == path {
                self_ref = true;
            } else {
                references.insert(r);
            }
        }
        Ok(NarInfo {
            url: url.ok_or_else(|| corrupt("no URL"))?,
            compression: compression.unwrap_or_else(|| "bzip2".into()),
            file_size,
            nar_hash: nar_hash.ok_or_else(|| corrupt("no NarHash"))?,
            nar_size: nar_size
                .filter(|&n| n != 0)
                .ok_or_else(|| corrupt("no NarSize"))?,
            references,
            self_ref,
            deriver,
            sigs,
            ca,
            path,
        })
    }

    /// The text [`NarInfo::parse`] reads back to `self`.
    pub fn to_text(&self) -> String {
        let all = self.all_references();
        let refs: Vec<&str> = all.iter().map(|r| r.base_name()).collect();
        let mut s = format!(
            "StorePath: {}/{}\nURL: {}\nCompression: {}\nFileSize: {}\nNarHash: {}\nNarSize: {}\nReferences: {}\n",
            store_dir(),
            self.path.base_name(),
            self.url,
            self.compression,
            self.file_size,
            self.nar_hash.to_string_with(HashFormat::Nix32, true),
            self.nar_size,
            refs.join(" ")
        );
        if let Some(d) = &self.deriver {
            s.push_str(&format!("Deriver: {}\n", d.base_name()));
        }
        for sig in &self.sigs {
            s.push_str(&format!("Sig: {sig}\n"));
        }
        if let Some(ca) = &self.ca {
            s.push_str(&format!("CA: {ca}\n"));
        }
        s
    }

    /// Every reference, the path itself included when it refers to itself.
    pub fn all_references(&self) -> BTreeSet<StorePath> {
        let mut all = self.references.clone();
        if self.self_ref {
            all.insert(self.path.clone());
        }
        all
    }

    /// What a signature covers (path-info.cc:fingerprint).
    pub fn fingerprint(&self) -> String {
        let refs: Vec<String> = self
            .all_references()
            .iter()
            .map(|r| format!("{}/{}", store_dir(), r.base_name()))
            .collect();
        format!(
            "1;{}/{};{};{};{}",
            store_dir(),
            self.path.base_name(),
            self.nar_hash.to_string_with(HashFormat::Nix32, true),
            self.nar_size,
            refs.join(",")
        )
    }

    /// Whether `CA` and the references rebuild the path
    /// (path-info.cc:isContentAddressed).
    pub fn is_content_addressed(&self) -> bool {
        let Some(ca) = &self.ca else {
            return false;
        };
        let (method, rest) = match ca.split_once(':') {
            Some(("text", rest)) => (IngestionMethod::Text, rest),
            Some(("fixed", rest)) => match rest.strip_prefix("r:") {
                Some(rest) => (IngestionMethod::Recursive, rest),
                None => (IngestionMethod::Flat, rest),
            },
            _ => return false,
        };
        let Some((algo, hash)) = rest.split_once(':') else {
            return false;
        };
        let Some(hash) =
            HashAlgo::parse(algo).and_then(|algo| Hash::parse_non_sri_unprefixed(hash, algo).ok())
        else {
            return false;
        };
        make_fixed_output_path_self(
            method,
            &hash,
            self.path.name(),
            &self.references,
            self.self_ref,
        )
        .is_ok_and(|p| p == self.path)
    }

    /// How many of `sigs` a key in `keys` made (path-info.cc:checkSignatures,
    /// without the content-addressed shortcut).
    pub fn good_signatures(&self, keys: &[String]) -> usize {
        let fp = self.fingerprint();
        self.sigs
            .iter()
            .filter(|s| verify(fp.as_bytes(), s, keys))
            .count()
    }

    /// Whether a store with `require-sigs` and `keys` would take this path
    /// (local-store.cc:pathInfoIsUntrusted).
    pub fn trusted(&self, keys: &[String], require_sigs: bool) -> bool {
        !require_sigs || self.is_content_addressed() || self.good_signatures(keys) > 0
    }
}

/// signature.cc:verifyDetached: `sig` is `name:base64`, checked with the key
/// of that name.
fn verify(data: &[u8], sig: &str, keys: &[String]) -> bool {
    let Some((name, sig)) = sig.split_once(':') else {
        return false;
    };
    let Ok(sig) = base64_decode(sig) else {
        return false;
    };
    keys.iter()
        .filter_map(|k| k.split_once(':'))
        .filter(|(n, _)| *n == name)
        .filter_map(|(_, k)| base64_decode(k).ok())
        .any(|k| {
            UnparsedPublicKey::new(&ED25519, k)
                .verify(data, &sig)
                .is_ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=";

    const GIT: &str = "StorePath: /nix/store/xgw813vgzl2q0696sq45adpj0xd690jq-git-2.56.0
URL: nar/18gxlgm87gp2rh37x8gdsrfyq2lnwqj7q8bb6z6rks2yja0wpkx9.nar.zst
Compression: zstd
FileHash: sha256:1qy42794qbiv9cp9c2pzfy7zc2v6jisrkach2m3qfpyhsxgml4ys
FileSize: 26467918
NarHash: sha256:18gxlgm87gp2rh37x8gdsrfyq2lnwqj7q8bb6z6rks2yja0wpkx9
NarSize: 62716888
References: 0f9na660n2568sn5drb8r3p5aknxy5r8-perl5.42.3-libwww-perl-6.83 42in9mqs4kcmha55xd0i3ahbnibbzwkb-zlib-ng-2.3.3 7a2cjhcgzyym5zaz6m4fksw47qvm150w-gcc-16.2.0-lib 7rx5im6jbgyfhrl0lc0kyxd9yf14rdxc-expat-2.9.0 86c5q81pmzq11lriiry3pazlb01gggww-perl5.42.3-Authen-SASL-2.2100 8qdw9shgr7dd3315jim8sfva2dnhq86c-gettext-1.0 9drfa3mrps11k2169s62h354mpj86q7l-perl5.42.3-FCGI-0.82 9wc73n5izv0m7ppjbibi4hbr68rba1lj-gnused-4.10 c3f45pxlb81gzz4w1nym7lpznfypva7s-gnugrep-3.12 cb987zkxvsv0490753ilrvn5yk7ylgxi-coreutils-9.12 fq0lrn9lsg7qadvbc3hgclx6ijqrkprf-perl5.42.3-URI-5.36 jsvghrcvyikk422yvr17g52qxscn5qg5-perl5.42.3-IO-Socket-SSL-2.083 k1xcmhfmkpzcxlimqd8fpbi0xqxaxm2j-perl5.42.3-Net-SMTP-SSL-1.04 lazac5vw20j0pyahzbaxz8z8hyf7m9zf-openssl-3.5.9 m7677iiw5w36mqw5d7kzws7067qi6ry8-perl5.42.3-TermReadKey-2.38 mb5kkl4yar0i9m20f6bglah9l7a28fcs-git-2.56.0-doc ncpfrkxdi790dnky93p1dpvn50m7icaj-perl5.42.3-FCGI-ProcManager-0.28 nnm9gdr70ihc6zc9298mzz8jpcgaciql-bash-5.3p20 p5dwcn4wpyjmhnnzj6iwwwjynyc6yif0-pcre2-10.49 p9i1b7jgfj4fl5bx557jnwbdsafl434i-curl-8.22.0 plix5s7ffq1fp9p8yhxj3adl0ah4hhqz-perl5.42.3-libnet-3.15 pqyiidz5mi73wv5f91d48s9plcqyj5bz-glibc-2.44-57 rab3bl91k7xksrdpdw69pcpkj6p1z78q-perl5.42.3-Net-SSLeay-1.92 rpswfv0ff4wap9anx7k1224x2m94pfcc-gzip-1.15 sixp6a2yhv1rqyjvi4q0vkz85z8r0rnr-perl5.42.3-HTML-Parser-3.85 spr79kzwpcsmfbvppf7br727bvn4gsdm-perl5.42.3-HTML-TagCloud-0.38 x5a9anjlhrsp9rmr1j2rdf1czlxlzxnk-perl5.42.3-Digest-HMAC-1.05 xahxrjs30p3mw0a42bqg9ikgkikskw8m-python3-3.14.8 xgw813vgzl2q0696sq45adpj0xd690jq-git-2.56.0 y1gmkixqpkfwayj9iqxn3c8brm6zkn1m-perl-5.42.3 y2fv5szkflkflgj2hws2mhlhr5bz7c7b-perl5.42.3-CGI-Fast-2.16 z2pfix6anjs8j8nplw18zlhqm2g9jygj-perl5.42.3-CGI-4.59
Deriver: lsklxr7k7vs2qja22qr2gp4z2h97hblx-git-2.56.0.drv
Sig: cache.nixos.org-1:JniGFZ0Ht8g2IeWKG3nf0tWw53rxjGBe1l0ukIGTBjSuncED6hM7mD1WV5Lw07OJ5vVQlMECvRaod5oPU31IBg==
";

    #[test]
    fn parses_and_verifies_a_cache_nixos_org_narinfo() {
        let ni = NarInfo::parse(GIT, "git.narinfo").unwrap();
        assert_eq!(ni.path.name(), "git-2.56.0");
        assert_eq!(ni.compression, "zstd");
        assert_eq!(ni.nar_size, 62716888);
        assert!(ni.self_ref);
        assert_eq!(ni.references.len(), 31);
        assert!(!ni.is_content_addressed());
        assert_eq!(ni.good_signatures(&[KEY.into()]), 1);
        assert!(ni.trusted(&[KEY.into()], true));
        assert!(!ni.trusted(
            &["other:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=".into()],
            true
        ));
        assert!(ni.trusted(&[], false));
        assert_eq!(NarInfo::parse(&ni.to_text(), "again").unwrap(), ni);
    }

    #[test]
    fn a_changed_fingerprint_breaks_the_signature() {
        let tampered = GIT.replace("NarSize: 62716888", "NarSize: 62716889");
        let ni = NarInfo::parse(&tampered, "git.narinfo").unwrap();
        assert!(!ni.trusted(&[KEY.into()], true));
    }

    #[test]
    fn a_content_address_must_rebuild_the_path() {
        let h = Hash::parse_any(
            "sha256:18gxlgm87gp2rh37x8gdsrfyq2lnwqj7q8bb6z6rks2yja0wpkx9",
            None,
        )
        .unwrap();
        let refs = BTreeSet::new();
        let path = make_fixed_output_path_self(IngestionMethod::Recursive, &h, "src", &refs, true)
            .unwrap();
        let text = format!(
            "StorePath: /nix/store/{}\nURL: nar/x.nar\nNarHash: {}\nNarSize: 8\nReferences: {}\nCA: fixed:r:sha256:{}\n",
            path.base_name(),
            h.to_string_with(HashFormat::Nix32, true),
            path.base_name(),
            h.to_nix32(),
        );
        let ni = NarInfo::parse(&text, "ca.narinfo").unwrap();
        assert_eq!(ni.compression, "bzip2");
        assert!(ni.is_content_addressed());
        assert!(ni.trusted(&[], true));
        let lying = text.replace(&format!("References: {}", path.base_name()), "References: ");
        assert!(
            !NarInfo::parse(&lying, "ca.narinfo")
                .unwrap()
                .is_content_addressed()
        );
    }

    #[test]
    fn missing_fields_are_corrupt() {
        let e = NarInfo::parse("URL: x\n", "a.narinfo").unwrap_err();
        assert_eq!(e.0, "NAR info file 'a.narinfo' is corrupt: no StorePath");
    }
}
