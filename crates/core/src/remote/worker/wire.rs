//! Framing of the Nix worker protocol (libutil/serialise.hh): every integer is a u64 in
//! little-endian; a string is its length, its bytes, and zero padding to a multiple of 8.

use std::io::{self, Read, Write};

use crate::remote::plan::{DrvHashes, Object};
use crate::srcpath::{SourceKind, Src};
use crate::store::hash::{Hash, HashAlgo, base64_decode, base64_encode, hash_bytes};
use crate::store::nar::{dump_string, flat_hash};
use crate::store::path::{StorePath, check_name};

pub fn write_u64(w: &mut dyn Write, n: u64) -> io::Result<()> {
    w.write_all(&n.to_le_bytes())
}

pub fn write_str(w: &mut dyn Write, s: &[u8]) -> io::Result<()> {
    write_u64(w, s.len() as u64)?;
    w.write_all(s)?;
    let rem = s.len() % 8;
    if rem != 0 {
        w.write_all(&[0u8; 8][..8 - rem])?;
    }
    Ok(())
}

/// Mirrors serialise.cc:writeStrings: a count followed by each string.
pub fn write_strs<S: AsRef<[u8]>>(w: &mut dyn Write, items: &[S]) -> io::Result<()> {
    write_u64(w, items.len() as u64)?;
    for s in items {
        write_str(w, s.as_ref())?;
    }
    Ok(())
}

pub fn read_u64(r: &mut dyn Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn bad(why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("worker protocol: {why}"),
    )
}

/// Mirrors serialise.cc:readString with a size cap.
pub fn read_bytes(r: &mut dyn Read, max: u64) -> io::Result<Vec<u8>> {
    let len = read_u64(r)?;
    if len > max {
        return Err(bad("string too long"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    let rem = (len % 8) as usize;
    if rem != 0 {
        let mut pad = [0u8; 8];
        r.read_exact(&mut pad[..8 - rem])?;
        if pad.iter().any(|&b| b != 0) {
            return Err(bad("non-zero padding"));
        }
    }
    Ok(buf)
}

pub const MAX_STRING: u64 = 64 << 20;

pub fn read_str(r: &mut dyn Read) -> io::Result<String> {
    Ok(String::from_utf8_lossy(&read_bytes(r, MAX_STRING)?).into_owned())
}

pub fn read_strs(r: &mut dyn Read) -> io::Result<Vec<String>> {
    let n = read_u64(r)?;
    if n > 1 << 24 {
        return Err(bad("string list too long"));
    }
    (0..n).map(|_| read_str(r)).collect()
}

/// `serialise.hh:readNum<bool>`: 0 or 1, nothing else.
pub fn read_bool(r: &mut dyn Read) -> io::Result<bool> {
    match read_u64(r)? {
        0 => Ok(false),
        1 => Ok(true),
        n => Err(bad(&format!(
            "serialised integer {n} is too large for a boolean"
        ))),
    }
}

/// serialise.hh:readNum<time_t>: a signed 64-bit integer.
pub fn read_time(r: &mut dyn Read) -> io::Result<u64> {
    let n = read_u64(r)?;
    if n > i64::MAX as u64 {
        return Err(bad(&format!(
            "serialised integer {n} is too large for a time"
        )));
    }
    Ok(n)
}

/// `worker-protocol.cc:Serialise<StorePath>`: the daemon names a store path, and
/// what is not one is an error, not a path for this client to act on.
pub fn read_path(r: &mut dyn Read) -> io::Result<String> {
    let s = read_str(r)?;
    StorePath::parse(&s).map_err(|e| bad(&e.to_string()))?;
    Ok(s)
}

/// `Serialise<std::optional<StorePath>>`: the empty string is none.
pub fn read_opt_path(r: &mut dyn Read) -> io::Result<String> {
    let s = read_str(r)?;
    if !s.is_empty() {
        StorePath::parse(&s).map_err(|e| bad(&e.to_string()))?;
    }
    Ok(s)
}

/// `Serialise<StorePathSet>`.
pub fn read_paths(r: &mut dyn Read) -> io::Result<Vec<String>> {
    let v = read_strs(r)?;
    for s in &v {
        StorePath::parse(s).map_err(|e| bad(&e.to_string()))?;
    }
    Ok(v)
}

/// `Serialise<UnkeyedValidPathInfo>`'s NAR hash: `Hash::parseAny` for SHA-256,
/// in any of its forms, held as the base16 the protocol writes.
pub fn nar_hash(s: &str) -> io::Result<String> {
    Hash::parse_any(s, Some(HashAlgo::Sha256))
        .map(|h| h.to_base16())
        .map_err(|e| bad(&e.to_string()))
}

/// local-keys.cc:parseColonBase64: a key name, a colon and non-empty base64,
/// held as Signature::to_string writes it: the bytes encoded again, so a
/// newline or what follows `=` in the reply is gone.
pub fn signature(s: &str) -> io::Result<String> {
    match s.split_once(':') {
        Some((name, data)) if !name.is_empty() => match base64_decode(data) {
            Ok(d) if !d.is_empty() => Ok(format!("{name}:{}", base64_encode(&d))),
            _ => Err(bad("signature is corrupt")),
        },
        _ => Err(bad("signature is corrupt")),
    }
}

/// content-address.cc:ContentAddress::parseOpt: empty, `text:ALGO:HASH`, or
/// `fixed:[r:]ALGO:HASH`, held as renderContentAddress writes it (the hash
/// in nix32). `fixed:git:` needs the git-hashing experimental feature, which
/// a client has not got by default.
pub fn content_address(s: &str) -> io::Result<String> {
    if s.is_empty() {
        return Ok(String::new());
    }
    let (method, rest) = match s.split_once(':') {
        Some(("text", rest)) => ("text:", rest),
        Some(("fixed", rest)) => match rest.strip_prefix("r:") {
            Some(rest) => ("fixed:r:", rest),
            None => ("fixed:", rest),
        },
        _ => return Err(bad(&format!("'{s}' is not a content address"))),
    };
    let (algo, hash) = rest
        .split_once(':')
        .ok_or_else(|| bad(&format!("'{s}' is not a content address")))?;
    let algo =
        HashAlgo::parse(algo).ok_or_else(|| bad(&format!("unknown hash algorithm '{algo}'")))?;
    let h = Hash::parse_non_sri_unprefixed(hash, algo).map_err(|e| bad(&e.to_string()))?;
    Ok(format!("{method}{}:{}", algo.name(), h.to_nix32()))
}

/// `serialise.hh:readNum<T>`: a number no larger than `T` holds.
pub fn read_num(r: &mut dyn Read, max: u64) -> io::Result<u64> {
    let n = read_u64(r)?;
    if n > max {
        return Err(bad(&format!("serialised integer {n} is too large")));
    }
    Ok(n)
}

/// derived-path.cc:DerivedPath::parseLegacy without dynamic derivations:
/// `PATH`, `PATH!*` or `PATH!NAME,...`, held as to_string_legacy writes it,
/// with the names sorted and each once.
pub fn derived_path(s: &str) -> io::Result<String> {
    let Some((drv, outputs)) = s.rsplit_once('!') else {
        StorePath::parse(s).map_err(|e| bad(&e.to_string()))?;
        return Ok(s.to_string());
    };
    if drv.contains('!') {
        return Err(bad(&format!(
            "'{s}' builds an output of a derivation output, which needs dynamic-derivations"
        )));
    }
    StorePath::parse(drv).map_err(|e| bad(&e.to_string()))?;
    if outputs == "*" {
        return Ok(s.to_string());
    }
    let mut names: Vec<&str> = outputs.split(',').collect();
    for n in &names {
        check_name(n).map_err(|e| bad(&e.to_string()))?;
    }
    names.sort_unstable();
    names.dedup();
    Ok(format!("{drv}!{}", names.join(",")))
}

/// realisation.cc:DrvOutput::parse: a prefixed hash, `!`, an output name
/// that is not checked.
pub fn drv_output(s: &str) -> io::Result<()> {
    let (hash, _) = s
        .split_once('!')
        .ok_or_else(|| bad(&format!("invalid derivation output id {s}")))?;
    Hash::parse_any(hash, None).map_err(|e| bad(&e.to_string()))?;
    Ok(())
}

/// `common-protocol.cc:Serialise<Realisation>`: the JSON of realisation.cc,
/// `outPath` a store path's base name, `signatures` if there, and `id`.
pub fn realisation(raw: &[u8]) -> io::Result<()> {
    let bad_json = |why: &str| bad(&format!("while parsing a realisation object: {why}"));
    let v: serde_json::Value = serde_json::from_slice(raw).map_err(|e| bad_json(&e.to_string()))?;
    let o = v.as_object().ok_or_else(|| bad_json("not an object"))?;
    let string = |key: &str| {
        o.get(key)
            .ok_or_else(|| bad_json(&format!("no key '{key}'")))?
            .as_str()
            .ok_or_else(|| bad_json(&format!("'{key}' is not a string")))
    };
    StorePath::from_base_name(string("outPath")?).map_err(|e| bad(&e.to_string()))?;
    if let Some(sigs) = o.get("signatures") {
        for s in sigs
            .as_array()
            .ok_or_else(|| bad_json("'signatures' is not a list"))?
        {
            signature(
                s.as_str()
                    .ok_or_else(|| bad_json("a signature is not a string"))?,
            )?;
        }
    }
    drv_output(string("id")?)
}

/// Mirrors serialise.hh:FramedSink: the payload of `AddMultipleToStore` goes out as
/// length-prefixed frames (no padding) closed by a zero-length frame, so the daemon can find
/// the end without parsing the content.
pub struct FramedSink<'a> {
    to: &'a mut dyn Write,
    buf: Vec<u8>,
}

impl<'a> FramedSink<'a> {
    pub const FRAME: usize = 1 << 20;

    pub fn new(to: &'a mut dyn Write) -> Self {
        FramedSink {
            to,
            buf: Vec::with_capacity(Self::FRAME),
        }
    }

    fn send(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            write_u64(self.to, self.buf.len() as u64)?;
            self.to.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Sends the last frame and the terminator.
    pub fn finish(mut self) -> io::Result<()> {
        self.send()?;
        write_u64(self.to, 0)?;
        self.to.flush()
    }
}

impl Write for FramedSink<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len() + data.len() > Self::FRAME {
            self.send()?;
        }
        if data.len() >= Self::FRAME {
            write_u64(self.to, data.len() as u64)?;
            self.to.write_all(data)?;
        } else {
            self.buf.extend_from_slice(data);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send()?;
        self.to.flush()
    }
}

/// Counts the bytes that pass through, for `--timings`.
pub struct Counting<W: Write> {
    pub inner: W,
    pub bytes: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(data)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// One `ValidPathInfo` (worker-protocol.cc, the 1.16 layout `addMultipleToStore` fixes) and
/// the NAR after it. Registration time 0 and no signatures: the daemon recomputes the content
/// address and accepts the path on that alone.
pub fn write_object(w: &mut dyn Write, o: &Object) -> io::Result<()> {
    match o {
        Object::Source {
            path,
            at,
            kind,
            nar_hash,
            nar_size,
            ..
        } => {
            let (ca, references): (String, Vec<String>) = match kind {
                SourceKind::Source => (format!("fixed:r:sha256:{}", nar_hash.to_nix32()), vec![]),
                SourceKind::Flat { hash } => (
                    format!("fixed:{}:{}", hash.algo.name(), hash.to_nix32()),
                    vec![],
                ),
                SourceKind::Text { references } => {
                    let flat = match (&at.src, at.local()) {
                        (Src::Mem(contents), _) => hash_bytes(HashAlgo::Sha256, contents),
                        (_, Some(path)) => flat_hash(&path, HashAlgo::Sha256)?,
                        _ => return Err(io::Error::other("a text path cannot come from a NAR")),
                    };
                    (
                        format!("text:sha256:{}", flat.to_nix32()),
                        references.iter().map(|r| r.to_string()).collect(),
                    )
                }
            };
            write_info(
                w,
                &path.to_string(),
                &nar_hash.to_base16(),
                &references,
                *nar_size,
                &ca,
            )?;
            // The framed body declares `nar_size` before the bytes, so a tree that changed
            // after evaluation must fail here rather than desynchronise the stream.
            let mut counted = Counting { inner: w, bytes: 0 };
            at.nar(&mut counted)?;
            if counted.bytes != *nar_size {
                return Err(io::Error::other(format!(
                    "source '{path}' changed since evaluation: its NAR is {} bytes, not {nar_size}",
                    counted.bytes
                )));
            }
            Ok(())
        }
        Object::Drv {
            path,
            aterm,
            references,
            hashes,
        } => {
            let h = hashes.get_or_init(|| DrvHashes::of(aterm));
            let references: Vec<String> = references.iter().map(|r| r.to_string()).collect();
            write_info(
                w,
                &path.to_string(),
                &h.nar_hash.to_base16(),
                &references,
                h.nar_size,
                &format!("text:sha256:{}", h.text.to_nix32()),
            )?;
            dump_string(aterm, w)
        }
    }
}

fn write_info(
    w: &mut dyn Write,
    path: &str,
    nar_hash_base16: &str,
    references: &[String],
    nar_size: u64,
    ca: &str,
) -> io::Result<()> {
    write_str(w, path.as_bytes())?;
    write_str(w, b"")?;
    write_str(w, nar_hash_base16.as_bytes())?;
    write_strs(w, references)?;
    write_u64(w, 0)?;
    write_u64(w, nar_size)?;
    write_u64(w, 0)?;
    write_strs::<&str>(w, &[])?;
    write_str(w, ca.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_padded() {
        let mut out = Vec::new();
        write_str(&mut out, b"hello").unwrap();
        assert_eq!(out.len(), 16);
        assert_eq!(&out[..8], &5u64.to_le_bytes());
        assert_eq!(&out[13..], &[0, 0, 0]);
        let mut r = out.as_slice();
        assert_eq!(read_str(&mut r).unwrap(), "hello");
        assert!(r.is_empty());
    }

    #[test]
    fn framed_sink_frames_and_terminates() {
        let mut out = Vec::new();
        {
            let mut s = FramedSink::new(&mut out);
            s.write_all(b"abc").unwrap();
            s.write_all(b"de").unwrap();
            s.finish().unwrap();
        }
        let mut expect = Vec::new();
        write_u64(&mut expect, 5).unwrap();
        expect.extend_from_slice(b"abcde");
        write_u64(&mut expect, 0).unwrap();
        assert_eq!(out, expect);
    }

    // what Nix 2.34.8's client refuses in a reply, found by fuzz/ (wire)
    #[test]
    fn replies_are_read_as_nix_reads_them() {
        let enc = |s: &str| {
            let mut v = Vec::new();
            write_str(&mut v, s.as_bytes()).unwrap();
            v
        };
        let num = |n: u64| n.to_le_bytes().to_vec();
        assert!(read_path(&mut &enc("/etc/passwd")[..]).is_err());
        assert!(read_path(&mut &enc("/nix/store/not-a-store-path")[..]).is_err());
        assert!(read_opt_path(&mut &enc("")[..]).is_ok());
        assert!(read_bool(&mut &num(2)[..]).is_err());
        assert!(read_time(&mut &num(u64::MAX)[..]).is_err());
        let hex = "4ab29c33e1fd9f3e3e1ebf6e0da0e8f8c3a5e3a73c92c4e9d6e8c04b1a5f2b3d";
        assert_eq!(nar_hash(&format!("sha256:{hex}")).unwrap(), hex);
        assert!(nar_hash("abc").is_err());
        assert_eq!(
            signature("cache.nixos.org-1:dGVz\ndA==junk").unwrap(),
            "cache.nixos.org-1:dGVzdA=="
        );
        assert!(signature("no-colon").is_err());
        assert!(signature(":dGVzdA==").is_err());
        assert_eq!(
            content_address(&format!("text:sha256:{hex}")).unwrap(),
            "text:sha256:0g9bbwd4ph78svlw94iwlzisbhzqx2h0svmz3qz3x7zxw4rrrcja"
        );
        assert!(content_address(&format!("fixed:r:sha256:{hex}")).is_ok());
        assert!(content_address(&format!("fixed:git:sha1:{hex}")).is_err());
        assert!(content_address("nonsense").is_err());
    }

    #[test]
    fn build_results_are_read_as_nix_reads_them() {
        let d = "/nix/store/7q6r5bxprr5bzxqyhjmzjvcg9c0sfbyk-a.drv";
        assert_eq!(
            derived_path(&format!("{d}!out,dev,out")).unwrap(),
            format!("{d}!dev,out")
        );
        assert_eq!(derived_path(&format!("{d}!*")).unwrap(), format!("{d}!*"));
        assert!(derived_path(d).is_ok());
        assert!(derived_path(&format!("{d}!out!bin")).is_err());
        assert!(derived_path(&format!("{d}!")).is_err());
        assert!(derived_path("/etc/passwd!out").is_err());
        let hex = "4ab29c33e1fd9f3e3e1ebf6e0da0e8f8c3a5e3a73c92c4e9d6e8c04b1a5f2b3d";
        assert!(drv_output(&format!("sha256:{hex}!out")).is_ok());
        assert!(drv_output(&format!("{hex}!out")).is_err());
        let out = "0c6ld7f4z8xbwgaz1mqk5c5bkwlhypzb-hello-2.12.3";
        let id = format!("sha256:{hex}!out");
        let ok = format!(r#"{{"id":"{id}","outPath":"{out}","signatures":["k:dGVzdA=="]}}"#);
        assert!(realisation(ok.as_bytes()).is_ok());
        let null_sigs = format!(r#"{{"id":"{id}","outPath":"{out}","signatures":null}}"#);
        assert!(realisation(null_sigs.as_bytes()).is_err());
        let full = format!(r#"{{"id":"{id}","outPath":"/nix/store/{out}"}}"#);
        assert!(realisation(full.as_bytes()).is_err());
        assert!(realisation(b"[]").is_err());
        assert!(read_num(&mut &256u64.to_le_bytes()[..], u8::MAX.into()).is_err());
    }
}
