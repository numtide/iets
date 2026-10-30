use std::io::{self, Write};
use std::{error, fmt};

use md5::Digest as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HashAlgo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
    Blake3,
}

impl HashAlgo {
    /// Mirrors hash.hh:regularHashSize.
    pub fn size(self) -> usize {
        match self {
            HashAlgo::Md5 => 16,
            HashAlgo::Sha1 => 20,
            HashAlgo::Sha256 => 32,
            HashAlgo::Sha512 => 64,
            HashAlgo::Blake3 => 32,
        }
    }

    /// Mirrors hash.cc:printHashAlgo.
    pub fn name(self) -> &'static str {
        match self {
            HashAlgo::Md5 => "md5",
            HashAlgo::Sha1 => "sha1",
            HashAlgo::Sha256 => "sha256",
            HashAlgo::Sha512 => "sha512",
            HashAlgo::Blake3 => "blake3",
        }
    }

    /// Mirrors hash.cc:parseHashAlgoOpt.
    pub fn parse(s: &str) -> Option<HashAlgo> {
        match s {
            "md5" => Some(HashAlgo::Md5),
            "sha1" => Some(HashAlgo::Sha1),
            "sha256" => Some(HashAlgo::Sha256),
            "sha512" => Some(HashAlgo::Sha512),
            "blake3" => Some(HashAlgo::Blake3),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashFormat {
    Base16,
    Nix32,
    Base64,
    Sri,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashError(pub String);

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl error::Error for HashError {}

const MAX_HASH_SIZE: usize = 64;

/// A hash digest. `len` may be smaller than `algo.size()` after `compress_hash`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash {
    pub algo: HashAlgo,
    len: u8,
    buf: [u8; MAX_HASH_SIZE],
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_string_with(HashFormat::Base16, true))
    }
}

impl Hash {
    /// Mirrors hash.hh:Hash::Hash(HashAlgorithm) plus a byte copy.
    pub fn from_bytes(algo: HashAlgo, bytes: &[u8]) -> Result<Hash, HashError> {
        if bytes.len() != algo.size() {
            return Err(HashError(format!(
                "hash has wrong length {} for algorithm '{}'",
                bytes.len(),
                algo.name()
            )));
        }
        Ok(Self::raw(algo, bytes))
    }

    fn raw(algo: HashAlgo, bytes: &[u8]) -> Hash {
        let mut buf = [0u8; MAX_HASH_SIZE];
        buf[..bytes.len()].copy_from_slice(bytes);
        Hash {
            algo,
            len: bytes.len() as u8,
            buf,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    /// Mirrors hash.cc:Hash::to_string.
    pub fn to_string_with(&self, format: HashFormat, include_algo: bool) -> String {
        let mut s = String::new();
        self.write_string_with(&mut s, format, include_algo);
        s
    }

    /// `to_string_with` appended to a buffer the caller keeps.
    pub fn write_string_with(&self, s: &mut String, format: HashFormat, include_algo: bool) {
        if format == HashFormat::Sri || include_algo {
            s.push_str(self.algo.name());
            s.push(if format == HashFormat::Sri { '-' } else { ':' });
        }
        match format {
            HashFormat::Base16 => base16_encode_into(s, self.bytes()),
            HashFormat::Nix32 => nix32_encode_into(s, self.bytes()),
            HashFormat::Base64 | HashFormat::Sri => s.push_str(&base64_encode(self.bytes())),
        }
    }

    pub fn to_base16(&self) -> String {
        self.to_string_with(HashFormat::Base16, false)
    }

    pub fn to_nix32(&self) -> String {
        self.to_string_with(HashFormat::Nix32, false)
    }

    pub fn to_base64(&self) -> String {
        self.to_string_with(HashFormat::Base64, false)
    }

    pub fn to_sri(&self) -> String {
        self.to_string_with(HashFormat::Sri, false)
    }

    /// Mirrors hash.cc:Hash::parseAny: `<algo>:<hash>`, `<algo>-<base64>` (SRI), or a bare
    /// hash whose encoding is picked by its length.
    pub fn parse_any(s: &str, algo: Option<HashAlgo>) -> Result<Hash, HashError> {
        let (prefix_algo, rest, is_sri) = match s.split_once(':') {
            Some((a, rest)) => (Some(a), rest, false),
            None => match s.split_once('-') {
                Some((a, rest)) => (Some(a), rest, true),
                None => (None, s, false),
            },
        };
        let parsed_algo = match prefix_algo {
            Some(a) => Some(
                HashAlgo::parse(a)
                    .ok_or_else(|| HashError(format!("unknown hash algorithm '{a}'")))?,
            ),
            None => None,
        };
        let algo = match (parsed_algo, algo) {
            (None, None) => {
                return Err(HashError(format!(
                    "hash '{s}' does not include a type, nor is the type otherwise known from context"
                )));
            }
            (Some(p), Some(a)) if p != a => {
                return Err(HashError(format!(
                    "hash '{s}' should have type '{}'",
                    a.name()
                )));
            }
            (Some(p), _) => p,
            (None, Some(a)) => a,
        };
        if is_sri {
            return Self::from_base64(algo, rest);
        }
        Self::parse_non_sri_unprefixed(rest, algo)
    }

    /// Mirrors hash.cc:Hash::parseNonSRIUnprefixed (format chosen by length).
    pub fn parse_non_sri_unprefixed(s: &str, algo: HashAlgo) -> Result<Hash, HashError> {
        let n = algo.size();
        if s.len() == n * 2 {
            Self::from_base16(algo, s)
        } else if s.len() == nix32_encoded_len(n) {
            Self::from_nix32(algo, s)
        } else if s.len() == base64_encoded_len(n) {
            Self::from_base64(algo, s)
        } else {
            Err(HashError(format!(
                "hash '{s}' has wrong length for hash algorithm '{}'",
                algo.name()
            )))
        }
    }

    /// Mirrors hash.cc:Hash::parseSRI.
    pub fn parse_sri(s: &str) -> Result<Hash, HashError> {
        let (a, rest) = s
            .split_once('-')
            .ok_or_else(|| HashError(format!("hash '{s}' is not SRI")))?;
        let algo =
            HashAlgo::parse(a).ok_or_else(|| HashError(format!("unknown hash algorithm '{a}'")))?;
        Self::from_base64(algo, rest)
    }

    pub fn from_base16(algo: HashAlgo, s: &str) -> Result<Hash, HashError> {
        Self::from_decoded(algo, base16_decode(s)?, "base16", s)
    }

    pub fn from_nix32(algo: HashAlgo, s: &str) -> Result<Hash, HashError> {
        Self::from_decoded(algo, nix32_decode(s)?, "nix32", s)
    }

    pub fn from_base64(algo: HashAlgo, s: &str) -> Result<Hash, HashError> {
        Self::from_decoded(algo, base64_decode(s)?, "Base64", s)
    }

    fn from_decoded(algo: HashAlgo, d: Vec<u8>, enc: &str, s: &str) -> Result<Hash, HashError> {
        if d.len() != algo.size() {
            return Err(HashError(format!(
                "invalid {enc} hash '{s}', length {} != expected length {}",
                d.len(),
                algo.size()
            )));
        }
        Ok(Self::raw(algo, &d))
    }
}

/// Mirrors hash.cc:hashString.
pub fn hash_bytes(algo: HashAlgo, data: &[u8]) -> Hash {
    let mut h = Hasher::new(algo);
    h.update(data);
    h.finish()
}

/// Mirrors hash.cc:compressHash.
pub fn compress_hash(hash: &Hash, new_size: usize) -> Hash {
    let mut out = vec![0u8; new_size];
    for (i, b) in hash.bytes().iter().enumerate() {
        out[i % new_size] ^= b;
    }
    Hash::raw(hash.algo, &out)
}

enum State {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Blake3(Box<blake3::Hasher>),
}

/// Mirrors hash.cc:HashSink: an incremental hasher that also counts bytes.
pub struct Hasher {
    state: State,
    bytes: u64,
}

impl Hasher {
    pub fn new(algo: HashAlgo) -> Hasher {
        let state = match algo {
            HashAlgo::Md5 => State::Md5(md5::Md5::new()),
            HashAlgo::Sha1 => State::Sha1(sha1::Sha1::new()),
            HashAlgo::Sha256 => State::Sha256(sha2::Sha256::new()),
            HashAlgo::Sha512 => State::Sha512(sha2::Sha512::new()),
            HashAlgo::Blake3 => State::Blake3(Box::new(blake3::Hasher::new())),
        };
        Hasher { state, bytes: 0 }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.bytes += data.len() as u64;
        match &mut self.state {
            State::Md5(h) => h.update(data),
            State::Sha1(h) => h.update(data),
            State::Sha256(h) => h.update(data),
            State::Sha512(h) => h.update(data),
            State::Blake3(h) => {
                h.update(data);
            }
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    pub fn finish(self) -> Hash {
        match self.state {
            State::Md5(h) => Hash::raw(HashAlgo::Md5, &h.finalize()),
            State::Sha1(h) => Hash::raw(HashAlgo::Sha1, &h.finalize()),
            State::Sha256(h) => Hash::raw(HashAlgo::Sha256, &h.finalize()),
            State::Sha512(h) => Hash::raw(HashAlgo::Sha512, &h.finalize()),
            State::Blake3(h) => Hash::raw(HashAlgo::Blake3, h.finalize().as_bytes()),
        }
    }
}

impl Write for Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const NIX32_CHARS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

const NIX32_DIGITS: [u8; 256] = {
    let mut t = [0xff; 256];
    let mut i = 0;
    while i < NIX32_CHARS.len() {
        t[NIX32_CHARS[i] as usize] = i as u8;
        i += 1;
    }
    t
};

/// Whether `c` is one of `NIX32_CHARS`, which is what nix tests a store
/// path's hash part against (path.cc:StorePath::StorePath).
pub fn is_nix32_char(c: u8) -> bool {
    NIX32_DIGITS[c as usize] != 0xff
}

/// Mirrors base-nix-32.hh:BaseNix32::encodedLength.
pub fn nix32_encoded_len(n: usize) -> usize {
    (n * 8 - 1) / 5 + 1
}

#[cfg(test)]
/// Mirrors base-nix-32.cc:BaseNix32::encode (Nix's reversed-bit-order base32).
pub fn nix32_encode(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let mut s = String::with_capacity(nix32_encoded_len(bytes.len()));
    nix32_encode_into(&mut s, bytes);
    s
}

/// `nix32_encode` appended to a buffer the caller keeps.
pub fn nix32_encode_into(s: &mut String, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let len = nix32_encoded_len(bytes.len());
    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let lo = bytes[i] >> j;
        let hi = if i + 1 < bytes.len() {
            bytes[i + 1].checked_shl((8 - j) as u32).unwrap_or(0)
        } else {
            0
        };
        s.push(NIX32_CHARS[((lo | hi) & 0x1f) as usize] as char);
    }
}

/// Mirrors base-nix-32.cc:BaseNix32::decode.
pub fn nix32_decode(s: &str) -> Result<Vec<u8>, HashError> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut res: Vec<u8> = vec![0; (bytes.len() * 5 - 5) / 8 + 1];
    for n in 0..bytes.len() {
        let c = bytes[bytes.len() - n - 1];
        let digit = NIX32_DIGITS[c as usize];
        if digit == 0xff {
            return Err(HashError(format!(
                "invalid character in Nix32 (Nix's Base32 variation) string: '{}'",
                c as char
            )));
        }
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        res[i] |= digit << j;
        let carry = digit.checked_shr((8 - j) as u32).unwrap_or(0);
        if carry != 0 {
            if res.len() < i + 2 {
                res.resize(i + 2, 0);
            }
            res[i + 1] |= carry;
        }
    }
    Ok(res)
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Mirrors base-n.cc:base16::encode, appended to a buffer the caller keeps.
pub fn base16_encode_into(s: &mut String, bytes: &[u8]) {
    let mut buf = [0u8; 128];
    for chunk in bytes.chunks(buf.len() / 2) {
        for (i, b) in chunk.iter().enumerate() {
            buf[2 * i] = HEX[(b >> 4) as usize];
            buf[2 * i + 1] = HEX[(b & 0xf) as usize];
        }
        s.push_str(str::from_utf8(&buf[..2 * chunk.len()]).expect("hex digits are ASCII"));
    }
}

/// `base16_encode_into` for a byte buffer.
pub fn base16_encode_into_bytes(s: &mut Vec<u8>, bytes: &[u8]) {
    for b in bytes {
        s.push(HEX[(b >> 4) as usize]);
        s.push(HEX[(b & 0xf) as usize]);
    }
}

/// Mirrors base-n.cc:base16::decode.
pub fn base16_decode(s: &str) -> Result<Vec<u8>, HashError> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(HashError(format!(
            "invalid base16 string '{s}': odd length"
        )));
    }
    let nibble = |c: u8| -> Result<u8, HashError> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(HashError(format!(
                "invalid character in base16 string: '{}'",
                c as char
            ))),
        }
    };
    bytes
        .chunks(2)
        .map(|p| Ok(nibble(p[0])? << 4 | nibble(p[1])?))
        .collect()
}

const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Mirrors base-n.hh:base64::encodedLength.
pub fn base64_encoded_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// Mirrors base-n.cc:base64::encode.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(base64_encoded_len(bytes.len()));
    for chunk in bytes.chunks(3) {
        let mut n: u32 = 0;
        for (k, b) in chunk.iter().enumerate() {
            n |= (*b as u32) << (16 - 8 * k);
        }
        for k in 0..4 {
            if k <= chunk.len() {
                s.push(BASE64_CHARS[((n >> (18 - 6 * k)) & 0x3f) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Mirrors base-n.cc:base64::decode (stops at '=', skips '\n').
pub fn base64_decode(s: &str) -> Result<Vec<u8>, HashError> {
    let mut res = Vec::with_capacity((s.len() + 2) / 4 * 3);
    let mut d: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        if c == b'\n' {
            continue;
        }
        let digit = BASE64_CHARS.iter().position(|&x| x == c).ok_or_else(|| {
            HashError(format!(
                "invalid character in Base64 string: '{}'",
                c as char
            ))
        })? as u32;
        bits += 6;
        d = d << 6 | digit;
        if bits >= 8 {
            res.push((d >> (bits - 8) & 0xff) as u8);
            bits -= 8;
        }
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const ABC_SHA256_HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    // nix hash convert --hash-algo ALGO --to nix32 $(printf abc | ALGOsum | cut -d' ' -f1)
    #[rstest]
    #[case::sha256(
        HashAlgo::Sha256,
        "1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s"
    )]
    #[case::sha1(HashAlgo::Sha1, "kpcd173cq987hw957sx6m0868wv3x6d9")]
    #[case::md5(HashAlgo::Md5, "3jgzhjhz9zjvbb0kyj7jc500ch")]
    fn nix32(#[case] algo: HashAlgo, #[case] want: &str) {
        assert_eq!(hash_bytes(algo, b"abc").to_nix32(), want);
    }

    // printf abc | ALGOsum, printf '' | sha256sum
    #[rstest]
    #[case::sha256(HashAlgo::Sha256, b"abc", ABC_SHA256_HEX)]
    #[case::sha1(HashAlgo::Sha1, b"abc", "a9993e364706816aba3e25717850c26c9cd0d89d")]
    #[case::empty_sha256(
        HashAlgo::Sha256,
        b"",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    )]
    fn base16(#[case] algo: HashAlgo, #[case] input: &[u8], #[case] want: &str) {
        assert_eq!(hash_bytes(algo, input).to_base16(), want);
    }

    // nix hash convert --hash-algo sha256 --to sri <hex>
    #[test]
    fn sha256_sri() {
        let h = hash_bytes(HashAlgo::Sha256, b"abc");
        assert_eq!(
            h.to_sri(),
            "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
        );
        assert_eq!(
            h.to_string_with(HashFormat::Base16, true),
            format!("sha256:{ABC_SHA256_HEX}")
        );
    }

    // nix hash convert --hash-algo sha512 --to nix32 / --to base64 $(printf abc | sha512sum | cut -d' ' -f1)
    #[test]
    fn sha512_nix32_base64() {
        let h = hash_bytes(HashAlgo::Sha512, b"abc");
        assert_eq!(
            h.to_nix32(),
            "2gs8k559z4rlahfx0y688s49m2vvszylcikrfinm30ly9rak69236nkam5ydvly1ai7xac99vxfc4ii84hawjbk876blyk1jfhkbbyx"
        );
        assert_eq!(
            h.to_base64(),
            "3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw=="
        );
    }

    #[test]
    fn streaming_matches_oneshot() {
        let mut w = Hasher::new(HashAlgo::Sha256);
        w.write_all(b"a").unwrap();
        w.write_all(b"bc").unwrap();
        assert_eq!(w.bytes_written(), 3);
        assert_eq!(w.finish(), hash_bytes(HashAlgo::Sha256, b"abc"));
    }

    #[test]
    fn parse_any_all_formats() {
        let h = hash_bytes(HashAlgo::Sha256, b"abc");
        for s in [
            ABC_SHA256_HEX.to_string(),
            format!("sha256:{ABC_SHA256_HEX}"),
            h.to_nix32(),
            format!("sha256:{}", h.to_nix32()),
            h.to_base64(),
            h.to_sri(),
        ] {
            assert_eq!(
                Hash::parse_any(&s, Some(HashAlgo::Sha256)).unwrap(),
                h,
                "{s}"
            );
        }
        assert_eq!(Hash::parse_any(&h.to_sri(), None).unwrap(), h);
        assert!(Hash::parse_any(ABC_SHA256_HEX, None).is_err());
        assert!(
            Hash::parse_any(&format!("sha1:{ABC_SHA256_HEX}"), Some(HashAlgo::Sha256)).is_err()
        );
        assert!(Hash::parse_any("sha256:abc", None).is_err());
        assert_eq!(Hash::parse_sri(&h.to_sri()).unwrap(), h);
    }

    #[test]
    fn nix32_roundtrip_and_reject() {
        for n in 1..70 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(nix32_decode(&nix32_encode(&data)).unwrap(), data);
        }
        assert!(nix32_decode("e").is_err());
        assert_eq!(nix32_decode(&"z".repeat(32)).unwrap().len(), 20);
        assert!(Hash::from_nix32(HashAlgo::Sha256, &"z".repeat(52)).is_err());
    }

    #[test]
    fn base64_roundtrip() {
        for n in 0..70 {
            let data: Vec<u8> = (0..n).map(|i| (i * 53 + 7) as u8).collect();
            let e = base64_encode(&data);
            assert_eq!(e.len(), base64_encoded_len(n));
            assert_eq!(base64_decode(&e).unwrap(), data);
        }
        assert!(base64_decode("ab$c").is_err());
    }

    // compressHash is what makeStorePath applies; checked end-to-end in path.rs.
    #[test]
    fn compress_hash_xors() {
        let h = Hash::raw(HashAlgo::Sha256, &[1u8, 2, 3, 4, 5, 6]);
        let c = compress_hash(&h, 4);
        assert_eq!(c.bytes(), &[1 ^ 5, 2 ^ 6, 3, 4]);
        assert_eq!(c.algo, HashAlgo::Sha256);
    }
}
