//! The derivation targets.
//!
//! `drv-read`: the input is a `.drv` file. Both sides read it as the
//! derivation `x` and must agree on accept or refuse. An accepted one is
//! written again by each side, and the ATerm and the drvPath must be the
//! same.
//!
//! `drv-gen`: the input chooses a derivation part by part (`generate`),
//! which then goes through `drv-read`.
//!
//! Oracle frames (`drv-oracle` of nix-conformance): request u32 length,
//! the ATerm. Reply u8 kind (0 done, 1 error), u32 length, then the
//! drvPath, a NUL and the ATerm written again, or the message.

use crate::oracle::{self, Oracle};
use crate::{Divergence, Target, Verdict, classify, known, no_panic};
use iets::store::drv::Derivation;
use libfuzzer_sys::arbitrary::Unstructured;
use std::collections::HashSet;
use std::{env, fs};

pub const KINDS: &[&str] = &[
    // Nix accepts the input, iets refuses it
    "nix-only",
    // iets accepts the input, Nix refuses it
    "iets-only",
    // both accept, and the ATerms written again differ: in this field, read
    // back from both ATerms, or `aterm` when that does not tell
    "aterm-outputs",
    "aterm-inputs",
    "aterm-sources",
    "aterm-system",
    "aterm-builder",
    "aterm-args",
    "aterm-env",
    "aterm-json",
    "aterm",
    // both accept, and the drvPaths differ
    "path",
    "iets-panic",
    // the oracle died: Nix itself crashes on the input
    "nix-crash",
];

#[derive(Clone, Copy)]
pub enum Mode {
    Read,
    Gen,
}

pub struct Drv {
    oracle: Oracle,
    known: Vec<known::Entry>,
    mode: Mode,
}

/// The drvPath and the ATerm written again, or the error.
type Answer = Result<(String, Vec<u8>), String>;

impl Drv {
    pub fn new(known: Vec<known::Entry>, mode: Mode) -> Drv {
        Drv {
            oracle: Oracle::from_env("IETS_DRV_ORACLE", "drv-oracle"),
            known,
            mode,
        }
    }

    fn nix(&mut self, aterm: &[u8]) -> Result<Answer, String> {
        let mut frame = (aterm.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(aterm);
        let (kind, body) = self.oracle.ask(&frame, |out| {
            let kind = oracle::bytes(out, 1)?[0];
            let n = oracle::u32_le(out)? as usize;
            Ok((kind, oracle::bytes(out, n)?))
        })?;
        Ok(match kind {
            0 => {
                let nul = body.iter().position(|&b| b == 0).unwrap_or(body.len());
                let path = String::from_utf8_lossy(&body[..nul]).into_owned();
                Ok((path, body.get(nul + 1..).unwrap_or_default().to_vec()))
            }
            _ => Err(String::from_utf8_lossy(&body).into_owned()),
        })
    }

    fn read(&mut self, aterm: &[u8]) -> Option<Divergence> {
        let nix = match self.nix(aterm) {
            Ok(n) => n,
            Err(why) => return diverge("nix-crash", why, String::new()),
        };
        let iets: Answer = match no_panic(|| {
            let drv = Derivation::parse(aterm, "x").map_err(|e| e.to_string())?;
            let out = drv.unparse().map_err(|e| e.to_string())?;
            let path = drv.drv_path_for(&out).map_err(|e| e.to_string())?;
            Ok((path.to_string(), out))
        }) {
            Ok(a) => a,
            Err(msg) => return diverge("iets-panic", show(&nix), msg),
        };
        match (&nix, &iets) {
            (Err(_), Err(_)) => None,
            (Ok(_), Err(e)) => diverge("nix-only", show(&nix), e.clone()),
            (Err(_), Ok(_)) => diverge("iets-only", show(&nix), show(&iets)),
            (Ok((np, na)), Ok((ip, ia))) => {
                if na != ia {
                    let at = na.iter().zip(ia).take_while(|(a, b)| a == b).count();
                    Some(Divergence {
                        kind: field(na, ia),
                        nix: excerpt(na, at),
                        iets: excerpt(ia, at),
                        at: format!("byte {at}"),
                    })
                } else if np != ip {
                    diverge("path", np.clone(), ip.clone())
                } else {
                    None
                }
            }
        }
    }
}

impl Target for Drv {
    fn check(&mut self, input: &[u8]) -> Verdict {
        let d = match self.mode {
            Mode::Read => self.read(input),
            Mode::Gen => {
                let aterm = generate(&mut Unstructured::new(input));
                if let Some(f) = env::var_os("IETS_DRV_WRITE") {
                    let _ = fs::write(f, &aterm);
                }
                self.read(&aterm)
            }
        };
        classify(&self.known, d, input)
    }
}

fn diverge(kind: &'static str, nix: String, iets: String) -> Option<Divergence> {
    Some(Divergence {
        kind,
        nix,
        iets,
        at: String::new(),
    })
}

fn show(a: &Answer) -> String {
    match a {
        Ok((p, t)) => format!("{p} {}", excerpt(t, 0)),
        Err(e) => format!("error: {e}"),
    }
}

/// Up to 80 bytes of `b` from a little before `at`.
fn excerpt(b: &[u8], at: usize) -> String {
    let from = at.saturating_sub(16).min(b.len());
    String::from_utf8_lossy(&b[from..b.len().min(from + 80)]).into_owned()
}

const PATHS: &[&str] = &[
    "/nix/store/0c6ld7f4z8xbwgaz1mqk5c5bkwlhypzb-hello-2.12.3",
    "/nix/store/1x2kbk4wpqvw3gwhxc5fy0xyhrbqk46n-glibc-2.42",
    "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x",
];

const DRVS: &[&str] = &[
    "/nix/store/7q6r5bxprr5bzxqyhjmzjvcg9c0sfbyk-a.drv",
    "/nix/store/lrrxdsf8pd52ahlfwpx9mrg5lxx5fsgw-u1.drv",
];

const NAMES: &[&str] = &["out", "dev", "lib", "bin", "doc", "man", "a.drv", ""];

const ALGOS: &[&str] = &[
    "sha256",
    "r:sha256",
    "text:sha256",
    "sha1",
    "r:sha1",
    "md5",
    "sha512",
    "r:sha512",
    "git:sha1",
    "git:sha256",
    "blake3",
    "r:",
    "",
];

const HASHES: &[&str] = &[
    "4ab29c33e1fd9f3e3e1ebf6e0da0e8f8c3a5e3a73c92c4e9d6e8c04b1a5f2b3d",
    "0g9bbwd4ph78svlw94iwlzisbhzqx2h0svmz3qz3x7zxw4rrrcja",
    "a9993e364706816aba3e25717850c26c9cd0d89d",
    "900150983cd24fb0d6963f7d28e17f72",
    "impure",
    "",
];

fn r<T>(v: libfuzzer_sys::arbitrary::Result<T>, d: T) -> T {
    v.unwrap_or(d)
}

fn pick<'a>(u: &mut Unstructured, from: &[&'a str]) -> &'a str {
    from[r(u.choose_index(from.len()), 0)]
}

/// A string as an ATerm writes it. Now and then a byte goes in unescaped.
fn string(out: &mut Vec<u8>, u: &mut Unstructured, s: &[u8]) {
    out.push(b'"');
    let raw = r(u.ratio(1u8, 32), false);
    for &c in s {
        match c {
            b'"' | b'\\' if !raw => out.extend_from_slice(&[b'\\', c]),
            b'\n' if !raw => out.extend_from_slice(b"\\n"),
            b'\r' if !raw => out.extend_from_slice(b"\\r"),
            b'\t' if !raw => out.extend_from_slice(b"\\t"),
            _ => out.push(c),
        }
    }
    out.push(b'"');
}

/// A string from `from`, or arbitrary bytes now and then.
fn some(u: &mut Unstructured, from: &[&str]) -> Vec<u8> {
    if r(u.ratio(1u8, 8), false) {
        r(u.arbitrary::<&[u8]>(), b"").to_vec()
    } else {
        pick(u, from).as_bytes().to_vec()
    }
}

/// Mostly sorted and without duplicates, as Nix writes them.
fn keys(u: &mut Unstructured, max: u8, from: &[&str]) -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = (0..r(u.int_in_range(0..=max), 0))
        .map(|_| some(u, from))
        .collect();
    if !r(u.ratio(1u8, 16), false) {
        v.sort();
        v.dedup();
    }
    v
}

fn strings(out: &mut Vec<u8>, u: &mut Unstructured, v: &[Vec<u8>]) {
    out.push(b'[');
    for (i, s) in v.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        string(out, u, s);
    }
    out.push(b']');
}

/// `(outputs,children)` of a dynamic input, or the plain output list.
fn input_node(out: &mut Vec<u8>, u: &mut Unstructured, dynamic: bool, depth: u8) {
    let outs = keys(u, 3, NAMES);
    if !dynamic {
        strings(out, u, &outs);
        return;
    }
    out.push(b'(');
    strings(out, u, &outs);
    out.push(b',');
    let children = if depth < 3 {
        keys(u, 2, NAMES)
    } else {
        Vec::new()
    };
    out.push(b'[');
    for (i, c) in children.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.push(b'(');
        string(out, u, c);
        out.push(b',');
        input_node(out, u, true, depth + 1);
        out.push(b')');
    }
    out.push(b']');
    out.push(b')');
}

/// A `.drv` file: mostly well formed, with wrong paths, algorithms, hashes
/// and orders now and then, odd bytes in strings, and sometimes cut short
/// or with one byte changed.
pub fn generate(u: &mut Unstructured) -> Vec<u8> {
    let mut out = Vec::new();
    let dynamic = r(u.ratio(1u8, 8), false);
    if dynamic {
        out.extend_from_slice(b"DrvWithVersion(");
        let v: &[u8] = if r(u.ratio(1u8, 16), false) {
            b"xp-dyn-drv2"
        } else {
            b"xp-dyn-drv"
        };
        string(&mut out, u, v);
        out.extend_from_slice(b",[");
    } else {
        out.extend_from_slice(b"Derive([");
    }
    // one name per output: iets keeps the last of two and Nix the first, an
    // iets bug that would otherwise fill every campaign (divergences.md,
    // "Reading a .drv file")
    let mut names = keys(u, 3, NAMES);
    let mut seen = HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.push(b'(');
        string(&mut out, u, name);
        let (path, algo, hash): (Vec<u8>, Vec<u8>, Vec<u8>) = match r(u.int_in_range(0u8..=4), 0) {
            // input-addressed
            0 | 1 => (some(u, PATHS), Vec::new(), Vec::new()),
            // fixed
            2 => (some(u, PATHS), some(u, ALGOS), some(u, HASHES)),
            // floating, or impure with the hash "impure"
            3 => (Vec::new(), some(u, ALGOS), some(u, &["", "impure"])),
            // deferred
            _ => (Vec::new(), Vec::new(), Vec::new()),
        };
        for s in [path, algo, hash] {
            out.push(b',');
            string(&mut out, u, &s);
        }
        out.push(b')');
    }
    out.extend_from_slice(b"],");
    let drvs = keys(u, 3, DRVS);
    out.push(b'[');
    for (i, d) in drvs.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.push(b'(');
        string(&mut out, u, d);
        out.push(b',');
        input_node(&mut out, u, dynamic, 0);
        out.push(b')');
    }
    out.extend_from_slice(b"],");
    let srcs = keys(u, 3, PATHS);
    strings(&mut out, u, &srcs);
    out.push(b',');
    let system = some(u, &["x86_64-linux", "aarch64-darwin", "builtin", ""]);
    string(&mut out, u, &system);
    out.push(b',');
    let builder = some(u, &["/bin/sh", "builtin:fetchurl", "builtin:buildenv", ""]);
    string(&mut out, u, &builder);
    out.push(b',');
    let args: Vec<Vec<u8>> = (0..r(u.int_in_range(0u8..=3), 0))
        .map(|_| some(u, &["-e", "echo \"$out\"\n", "\\"]))
        .collect();
    strings(&mut out, u, &args);
    out.push(b',');
    let env = keys(
        u,
        4,
        &[
            "out",
            "name",
            "system",
            "__json",
            "__structuredAttrs",
            "PATH",
        ],
    );
    let env: Vec<(Vec<u8>, Vec<u8>)> = env
        .into_iter()
        .map(|k| {
            let v = some(
                u,
                &[
                    "",
                    "x",
                    "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x",
                    "{\"a\":1}",
                ],
            );
            (k, v)
        })
        .collect();
    out.push(b'[');
    for (i, (k, v)) in env.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.push(b'(');
        string(&mut out, u, k);
        out.push(b',');
        string(&mut out, u, v);
        out.push(b')');
    }
    out.extend_from_slice(b"])");
    if r(u.ratio(1u8, 16), false) {
        let at = r(u.choose_index(out.len()), 0);
        out[at] = r(u.arbitrary::<u8>(), 0);
    }
    if r(u.ratio(1u8, 16), false) {
        let at = r(u.choose_index(out.len()), 0);
        out.truncate(at);
    }
    out
}

/// The kind for two ATerms that differ: the first field in which the
/// derivations read back from them differ. `__json` is compared as bytes,
/// because Nix writes an input's output name without escapes
/// (nix-conformance `nix-bugs/drv-round-trip`), and such an ATerm does not
/// read back.
fn field(nix: &[u8], iets: &[u8]) -> &'static str {
    if without_json(nix).is_some_and(|n| Some(n) == without_json(iets)) {
        return "aterm-json";
    }
    let (Ok(n), Ok(i)) = (Derivation::parse(nix, "x"), Derivation::parse(iets, "x")) else {
        return "aterm";
    };
    if n.outputs != i.outputs {
        "aterm-outputs"
    } else if n.input_drvs != i.input_drvs {
        "aterm-inputs"
    } else if n.input_srcs != i.input_srcs {
        "aterm-sources"
    } else if n.platform != i.platform {
        "aterm-system"
    } else if n.builder != i.builder {
        "aterm-builder"
    } else if n.args != i.args {
        "aterm-args"
    } else if n.env != i.env {
        "aterm-env"
    } else {
        "aterm"
    }
}

/// The ATerm with the value of `__json` cut out.
fn without_json(aterm: &[u8]) -> Option<Vec<u8>> {
    const KEY: &[u8] = br#"("__json",""#;
    let start = aterm.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    let mut rest = aterm[start..].iter().enumerate();
    while let Some((k, &c)) = rest.next() {
        match c {
            b'\\' => _ = rest.next(),
            b'"' => return Some([&aterm[..start], &aterm[start + k..]].concat()),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::field;

    /// An ATerm whose input output name holds a raw `"`, as Nix writes it,
    /// so it does not read back, with `json` as the value of `__json`.
    fn unreadable(json: &str) -> Vec<u8> {
        format!(
            r#"Derive([("out","","","")],[("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv",["a"b"])],[],"s","/b",[],[("__json","{json}"),("out","")])"#
        )
        .into_bytes()
    }

    #[test]
    fn only_a_json_difference_is_json() {
        let nix = unreadable(r#"{\"k\":2}"#);
        let iets = unreadable(r#"{\"k\":1,\"k\":2}"#);
        assert_eq!(field(&nix, &iets), "aterm-json");
        let other = String::from_utf8(iets).unwrap().replace("/b", "/c");
        assert_eq!(field(&nix, other.as_bytes()), "aterm");
    }
}
