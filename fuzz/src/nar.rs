//! The NAR targets.
//!
//! `nar-read`: the input is a NAR. Both sides read it and must agree on
//! accept or refuse and on the bytes consumed; iets's `parse_length` must
//! agree too. An accepted NAR is written out again by each side, from
//! Nix's memory tree and from the directory `restore_path` made, and the
//! two must be the same bytes. Nix also dumps that directory, and iets's
//! `nar_hash` of it must be the SHA-256 of those bytes.
//!
//! `nar-gen`: the input chooses a NAR token by token (`generate`), which
//! then goes through `nar-read`.
//!
//! `nar-tree`: the input chooses a file tree, which is made on disk. Both
//! sides dump it; the NAR must be the same bytes, or both must refuse. The
//! NAR then goes through `nar-read`.
//!
//! Oracle frames (`nar-oracle`): request u8 op, u32 length, payload. Op 0
//! reads the payload as a NAR, op 1 dumps the path it names. Reply u8 kind
//! (0 done, 1 error), u32 length, then for op 0 the bytes consumed (u64)
//! and the tree as a NAR, for op 1 the NAR, for an error the message.

use crate::oracle::{self, Oracle};
use crate::{Divergence, Target, Verdict, classify, known, no_panic};
use iets::store::nar;
use iets::sys::tmp;
use libfuzzer_sys::arbitrary::Unstructured;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::{fs, io};

pub const KINDS: &[&str] = &[
    // Nix accepts the input, iets refuses it
    "nix-only",
    // iets accepts the input, Nix refuses it
    "iets-only",
    // parse_length and Nix disagree on accept or refuse
    "parse-length",
    // both accept, after a different number of bytes
    "consumed",
    // both accept, and the trees written out again differ
    "tree",
    // the two dumps of one directory differ, or only one fails
    "dump",
    // nar_hash or nar_size does not describe the dump
    "hash",
    "iets-panic",
    // the oracle died: Nix itself crashes on the input
    "nix-crash",
];

pub struct Nar {
    oracle: Oracle,
    known: Vec<known::Entry>,
    /// Where trees are made and restored.
    dir: PathBuf,
    mode: Mode,
    seq: u64,
}

type Answer = Result<Vec<u8>, String>;

#[derive(Clone, Copy)]
pub enum Mode {
    /// `nar-read`: the input is the NAR.
    Read,
    /// `nar-gen`: the input chooses the NAR (`generate`).
    Gen,
    /// `nar-tree`: the input chooses a tree on disk.
    Tree,
}

impl Nar {
    /// The target `mode` names.
    pub fn new(known: Vec<known::Entry>, mode: Mode) -> Nar {
        let dir =
            tmp::private_dir("iets-fuzz-nar-").expect("a scratch directory for the nar targets");
        Nar {
            oracle: Oracle::from_env("IETS_NAR_ORACLE", "nar-oracle"),
            known,
            dir,
            mode,
            seq: 0,
        }
    }

    /// Nix's answer, or the oracle's death.
    fn nix(&mut self, op: u8, payload: &[u8]) -> Result<Answer, String> {
        let mut frame = vec![op];
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        let (kind, body) = self.oracle.ask(&frame, |out| {
            let kind = oracle::bytes(out, 1)?[0];
            let n = oracle::u32_le(out)? as usize;
            Ok((kind, oracle::bytes(out, n)?))
        })?;
        Ok(match kind {
            0 => Ok(body),
            _ => Err(String::from_utf8_lossy(&body).into_owned()),
        })
    }

    /// A path not used before: the oracle caches `lstat` by path for the
    /// life of the process (posix-source-accessor.cc).
    fn scratch(&mut self) -> PathBuf {
        clear(&self.dir.join(self.seq.to_string()));
        self.seq += 1;
        self.dir.join(self.seq.to_string())
    }

    fn read(&mut self, input: &[u8]) -> Option<Divergence> {
        let nix = match self.nix(0, input) {
            Ok(n) => n,
            Err(why) => return diverge("nix-crash", why, String::new()),
        };
        let dest = self.scratch();
        let iets = no_panic(|| {
            let len = nar::parse_length(&mut &input[..]).map_err(|e| e.to_string());
            let restored = nar::restore_path(&mut &input[..], &dest).map_err(|e| e.to_string());
            (len, restored)
        });
        let (len, restored) = match iets {
            Ok(r) => r,
            Err(msg) => return diverge("iets-panic", show(&nix), msg),
        };
        let (nix_len, nix_tree) = match (&nix, &restored) {
            (Err(_), Err(_)) => {
                return len
                    .is_ok()
                    .then(|| diverge("parse-length", show(&nix), format!("{len:?}")))
                    .flatten();
            }
            (Ok(_), Err(e)) => return diverge("nix-only", show(&nix), e.clone()),
            (Err(_), Ok(())) => return diverge("iets-only", show(&nix), "restored".into()),
            (Ok(n), Ok(())) => {
                let (head, tree) = n.split_at(8);
                (u64::from_le_bytes(head.try_into().unwrap()), tree.to_vec())
            }
        };
        match len {
            Err(e) => return diverge("parse-length", show(&nix), e),
            Ok(l) if l != nix_len => {
                return diverge("consumed", nix_len.to_string(), l.to_string());
            }
            Ok(_) => {}
        }
        self.dumps(&dest, Some(&nix_tree))
    }

    /// Both dumps of `path`, and `want` (Nix's tree of the same NAR) when
    /// there is one.
    fn dumps(&mut self, path: &Path, want: Option<&[u8]>) -> Option<Divergence> {
        let nix = match self.nix(1, path.as_os_str().as_bytes()) {
            Ok(n) => n,
            Err(why) => return diverge("nix-crash", why, String::new()),
        };
        let iets = no_panic(|| {
            let mut out = Vec::new();
            nar::dump_path(path, &mut out, &mut |_, _| true)
                .map(|()| out)
                .map_err(|e| e.to_string())
        });
        let iets = match iets {
            Ok(r) => r,
            Err(msg) => return diverge("iets-panic", show(&nix), msg),
        };
        let mine = match (&nix, &iets) {
            (Err(_), Err(_)) => return None,
            (Ok(n), Ok(i)) if n == i => i,
            _ => return diverge("dump", show(&nix), show(&iets)),
        };
        if let Some(want) = want
            && want != mine.as_slice()
        {
            return diverge("tree", hex_head(want), hex_head(mine));
        }
        match nar::nar_hash(path) {
            Ok(h) if h.bytes() == Sha256::digest(mine).as_slice() => {}
            other => {
                return diverge(
                    "hash",
                    hex(&Sha256::digest(mine)),
                    format!("{:?}", other.map(|h| hex(h.bytes()))),
                );
            }
        }
        match nar::nar_size(path) {
            Ok(n) if n == mine.len() as u64 => {}
            other => return diverge("hash", mine.len().to_string(), format!("{other:?}")),
        }
        let file = fs::symlink_metadata(path)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o100 == 0);
        if file {
            let mut one = Vec::new();
            let contents = fs::read(path).unwrap_or_default();
            if nar::dump_string(&contents, &mut one).is_err() || one != *mine {
                return diverge(
                    "dump",
                    hex_head(mine),
                    format!("dump_string: {}", hex_head(&one)),
                );
            }
        }
        None
    }

    fn tree(&mut self, input: &[u8]) -> Option<Divergence> {
        let root = self.scratch();
        let mut u = Unstructured::new(input);
        if build(&mut u, &root, 0).is_err() {
            return None;
        }
        if let Some(d) = self.dumps(&root, None) {
            return Some(d);
        }
        let mut nar = Vec::new();
        nar::dump_path(&root, &mut nar, &mut |_, _| true).ok()?;
        self.read(&nar)
    }
}

impl Target for Nar {
    fn check(&mut self, input: &[u8]) -> Verdict {
        let d = match self.mode {
            Mode::Read => self.read(input),
            Mode::Gen => {
                let nar = generate(&mut Unstructured::new(input));
                self.read(&nar)
            }
            Mode::Tree => self.tree(input),
        };
        classify(&self.known, d, input)
    }
}

impl Drop for Nar {
    fn drop(&mut self) {
        clear(&self.dir);
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
        Ok(b) => hex_head(b),
        Err(e) => format!("error: {e}"),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A NAR is long; its length and head are where a human starts.
fn hex_head(b: &[u8]) -> String {
    format!("{} bytes: {}", b.len(), hex(&b[..b.len().min(64)]))
}

/// Removes `p`, giving back the permissions a generated tree took away.
fn clear(p: &Path) {
    let Ok(m) = fs::symlink_metadata(p) else {
        return;
    };
    if m.is_dir() {
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
        if let Ok(rd) = fs::read_dir(p) {
            for e in rd.flatten() {
                clear(&e.path());
            }
        }
        let _ = fs::remove_dir(p);
    } else {
        let _ = fs::remove_file(p);
    }
}

/// Makes at `path` the tree the bytes choose: files with any mode the owner
/// can read, large files past the writer's 64 KiB buffer, symlinks to any
/// target, FIFOs, and directories of names in any bytes a name may hold.
fn build(u: &mut Unstructured, path: &Path, depth: usize) -> io::Result<()> {
    let bad = |_| io::Error::other("input exhausted");
    // a chain across the depth limit both dumps share (archive.cc: 64)
    if depth == 0 && u.ratio(1u8, 32).map_err(bad)? {
        let mut p = path.to_path_buf();
        for _ in 0..u.int_in_range(60u8..=66).map_err(bad)? {
            fs::create_dir(&p)?;
            p.push("d");
        }
        return fs::write(p, b"");
    }
    match u.int_in_range(0u8..=9).map_err(bad)? {
        0..=3 => {
            let contents = if u.ratio(1u8, 8).map_err(bad)? {
                let n = u.int_in_range(0usize..=(1 << 20)).map_err(bad)?;
                vec![u.arbitrary::<u8>().map_err(bad)?; n]
            } else {
                u.arbitrary::<&[u8]>().map_err(bad)?.to_vec()
            };
            fs::write(path, contents)?;
            let mode = u32::from(u.arbitrary::<u16>().map_err(bad)? & 0o777) | 0o400;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
        }
        4 | 5 => {
            let target: &[u8] = u.arbitrary().map_err(bad)?;
            symlink(OsStr::from_bytes(target), path).or_else(|_| symlink("x", path))
        }
        6 => rustix::fs::mkfifoat(
            rustix::fs::CWD,
            path,
            rustix::fs::Mode::from_raw_mode(0o644),
        )
        .map_err(io::Error::from),
        _ => {
            fs::create_dir(path)?;
            if depth < 8 {
                for _ in 0..u.int_in_range(0u8..=6).map_err(bad)? {
                    let name: &[u8] = u.arbitrary().map_err(bad)?;
                    if name.is_empty()
                        || name.len() > 255
                        || name == b"."
                        || name == b".."
                        || name.contains(&b'/')
                        || name.contains(&0)
                    {
                        continue;
                    }
                    let child = path.join(OsStr::from_bytes(name));
                    if fs::symlink_metadata(&child).is_ok() {
                        continue;
                    }
                    build(u, &child, depth + 1)?;
                }
            }
            let mode = u32::from(u.arbitrary::<u16>().map_err(bad)? & 0o777) | 0o700;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
        }
    }
}

/// The NAR the bytes choose, token by token, for the `nar-gen` target:
/// mostly well formed, with names of any bytes, entries in any order,
/// symlink targets of any bytes, a wrong tag now and then, and a cut at
/// any point.
pub fn generate(u: &mut Unstructured) -> Vec<u8> {
    let mut out = Vec::new();
    tok(&mut out, b"nix-archive-1");
    gen_node(u, &mut out, 0);
    if u.ratio(1u8, 16).unwrap_or(false) {
        let cut = u.int_in_range(0..=out.len()).unwrap_or(out.len());
        out.truncate(cut);
    }
    out
}

fn tok(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s);
    out.resize(out.len().next_multiple_of(8), 0);
}

/// `want`, or rarely a wrong tag in its place.
fn tag(u: &mut Unstructured, out: &mut Vec<u8>, want: &[u8]) {
    const TAGS: [&[u8]; 12] = [
        b"(",
        b")",
        b"type",
        b"regular",
        b"directory",
        b"symlink",
        b"executable",
        b"contents",
        b"entry",
        b"name",
        b"node",
        b"target",
    ];
    if u.ratio(1u8, 64).unwrap_or(false) {
        let i = u.choose_index(TAGS.len()).unwrap_or(0);
        tok(out, TAGS[i]);
    } else {
        tok(out, want);
    }
}

fn gen_node(u: &mut Unstructured, out: &mut Vec<u8>, depth: usize) {
    // a chain across the depth limit both sides share (archive.cc: 64)
    if depth == 0 && u.ratio(1u8, 32).unwrap_or(false) {
        let n = u.int_in_range(60u8..=66).unwrap_or(64);
        for _ in 0..n {
            for t in [
                &b"("[..],
                b"type",
                b"directory",
                b"entry",
                b"(",
                b"name",
                b"d",
                b"node",
            ] {
                tok(out, t);
            }
        }
        for t in [&b"("[..], b"type", b"regular", b"contents", b"", b")"] {
            tok(out, t);
        }
        for _ in 0..n {
            for t in [&b")"[..], b")"] {
                tok(out, t);
            }
        }
        return;
    }
    tag(u, out, b"(");
    tag(u, out, b"type");
    match u.int_in_range(0u8..=3).unwrap_or(0) {
        0 | 1 => {
            tag(u, out, b"regular");
            if u.ratio(1u8, 3).unwrap_or(false) {
                tag(u, out, b"executable");
                let v: &[u8] = if u.ratio(1u8, 32).unwrap_or(false) {
                    b"x"
                } else {
                    b""
                };
                tok(out, v);
            }
            tag(u, out, b"contents");
            tok(out, u.arbitrary::<&[u8]>().unwrap_or(&[]));
        }
        2 => {
            tag(u, out, b"symlink");
            tag(u, out, b"target");
            tok(out, u.arbitrary::<&[u8]>().unwrap_or(b"x"));
        }
        _ => {
            tag(u, out, b"directory");
            let n = if depth < 6 {
                u.int_in_range(0u8..=5).unwrap_or(0)
            } else {
                0
            };
            let mut names: Vec<Vec<u8>> = (0..n)
                .map(|_| u.arbitrary::<&[u8]>().unwrap_or(b"a").to_vec())
                .collect();
            if !u.ratio(1u8, 16).unwrap_or(false) {
                names.sort();
                names.dedup();
            }
            for name in names {
                tag(u, out, b"entry");
                tag(u, out, b"(");
                tag(u, out, b"name");
                tok(out, &name);
                tag(u, out, b"node");
                gen_node(u, out, depth + 1);
                tag(u, out, b")");
            }
        }
    }
    tag(u, out, b")");
}
