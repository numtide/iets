//! The wire target: iets's daemon client against Nix's, both talking to one
//! scripted daemon.
//!
//! The input chooses a script: the daemon's side of the handshake at any
//! protocol version, stderr frames (log lines, activities, results, errors)
//! after `SetOptions` and after the operation, then the operation's reply,
//! with wrong store paths, hashes, signatures and content addresses now and
//! then. A child process (`DAEMON`) serves the script on a Unix socket to
//! each connection in turn and reads whatever the client sends. iets's
//! `Connection` and Nix's `RemoteStore` (`wire-oracle`) each connect and run
//! the same operation; the two answers must be the same line
//! (nix-conformance's SPEC.md).
//!
//! Oracle frames (`wire-oracle`): request u32 length and the line
//! `SOCKET OP ARG...`, reply u32 length and the answer.

use crate::oracle::{self, Oracle};
use crate::{Divergence, Target, Verdict, classify, known, no_panic};
use iets::remote::worker::{Connection, Options, Transport};
use iets::store::hash::{HashAlgo, hash_bytes};
use iets::store::nar;
use iets::sys::tmp;
use libfuzzer_sys::arbitrary::Unstructured;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::{env, fs, iter, process};

pub const KINDS: &[&str] = &[
    // Nix's client accepts the reply, iets's refuses it
    "nix-only",
    // iets's client accepts the reply, Nix's refuses it
    "iets-only",
    // both accept it, with different answers
    "answer",
    "iets-panic",
    // the oracle died: Nix itself crashes on the reply
    "nix-crash",
];

const MAGIC_2: u64 = 0x6478_696f;
const NEXT: u64 = 0x6f6c_6d67;
const READ: u64 = 0x6461_7461;
const LAST: u64 = 0x616c_7473;
const ERROR: u64 = 0x6378_7470;
const START: u64 = 0x5354_5254;
const STOP: u64 = 0x5354_4f50;
const RESULT: u64 = 0x5253_4c54;

/// Valid store paths for the arguments and the replies.
const PATHS: [&str; 4] = [
    "/nix/store/0c6ld7f4z8xbwgaz1mqk5c5bkwlhypzb-hello-2.12.3",
    "/nix/store/1x2kbk4wpqvw3gwhxc5fy0xyhrbqk46n-glibc-2.42",
    "/nix/store/7q6r5bxprr5bzxqyhjmzjvcg9c0sfbyk-a.drv",
    "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x",
];
const HASH: &str = "4ab29c33e1fd9f3e3e1ebf6e0da0e8f8c3a5e3a73c92c4e9d6e8c04b1a5f2b3d";

pub struct Wire {
    oracle: Oracle,
    daemon: Oracle,
    known: Vec<known::Entry>,
    dir: PathBuf,
    seq: u64,
}

/// One operation and its arguments, as both clients run it.
struct Op {
    name: &'static str,
    args: Vec<String>,
}

impl Wire {
    pub fn new(known: Vec<known::Entry>) -> Wire {
        let dir =
            tmp::private_dir("iets-fuzz-wire-").expect("a scratch directory for the wire target");
        Wire {
            oracle: Oracle::from_env("IETS_WIRE_ORACLE", "wire-oracle"),
            daemon: Oracle::with_env(
                env::current_exe()
                    .expect("the path of this fuzzer")
                    .to_string_lossy()
                    .into_owned(),
                (DAEMON, "1"),
            ),
            known,
            dir,
            seq: 0,
        }
    }

    fn compare(&mut self, input: &[u8]) -> Option<Divergence> {
        let (op, script, split) = generate(&mut Unstructured::new(input));
        // the daemon's bytes, to replay a case against a client by hand
        if let Ok(f) = env::var("IETS_WIRE_SCRIPT") {
            let _ = fs::write(f, &script);
        }
        self.seq += 1;
        let sock = self.dir.join(format!("{}.sock", self.seq));
        let mut start = (split as u32).to_le_bytes().to_vec();
        start.extend_from_slice(&(sock.as_os_str().len() as u32).to_le_bytes());
        start.extend_from_slice(sock.as_os_str().as_bytes());
        start.extend_from_slice(&script);
        self.daemon
            .ask(&framed(&start), |out| oracle::bytes(out, 1))
            .expect("the scripted daemon listens");
        let iets = no_panic(|| run_iets(&sock, op.name, &op.args));
        let mut line = format!("{} {}", sock.display(), op.name);
        for a in &op.args {
            line.push(' ');
            line.push_str(a);
        }
        let mut frame = (line.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(line.as_bytes());
        let nix = self.oracle.ask(&frame, |out| {
            let n = oracle::u32_le(out)? as usize;
            Ok(String::from_utf8_lossy(&oracle::bytes(out, n)?).into_owned())
        });
        // the stop frame first, then a connection to wake the daemon's accept
        self.daemon
            .ask(&framed(&[]), |out| {
                let _ = UnixStream::connect(&sock);
                oracle::bytes(out, 1)
            })
            .expect("the scripted daemon stops");
        let _ = fs::remove_file(&sock);
        let d = |kind, nix: String, iets: String| {
            Some(Divergence {
                kind,
                nix,
                iets,
                at: iter::once(op.name.to_string())
                    .chain(op.args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" "),
            })
        };
        let nix = match nix {
            Ok(n) => n,
            Err(why) => return d("nix-crash", why, String::new()),
        };
        let (iets, why) = match iets {
            Ok(Ok(i)) => (i, String::new()),
            Ok(Err(why)) => ("error".to_string(), why),
            Err(msg) => return d("iets-panic", nix, msg),
        };
        match (nix == "error", iets == "error") {
            _ if nix == iets => None,
            // iets's reason, for divergences.toml to tell the cases apart
            (false, true) => d("nix-only", nix, format!("error: {why}")),
            (true, false) => d("iets-only", nix, iets),
            _ => d("answer", nix, iets),
        }
    }
}

impl Target for Wire {
    fn check(&mut self, input: &[u8]) -> Verdict {
        let d = self.compare(input);
        classify(&self.known, d, input)
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The end of the one upload both clients make: a frame of the count 0 of
/// AddMultipleToStore, then the empty frame.
const UPLOAD_END: [u8; 24] = {
    let mut b = [0; 24];
    b[0] = 8;
    b
};

/// Set in the environment of the scripted daemon: this fuzzer, run again.
/// In a thread of the fuzzer, the daemon's code would add to the coverage
/// of each input, more or less with timing, and libFuzzer would keep other
/// inputs on each run.
const DAEMON: &str = "IETS_WIRE_DAEMON";

/// A request to the daemon: u32 length, then the body.
fn framed(body: &[u8]) -> Vec<u8> {
    let mut f = (body.len() as u32).to_le_bytes().to_vec();
    f.extend_from_slice(body);
    f
}

/// Runs the scripted daemon instead of the fuzzer when `DAEMON` is set.
pub fn daemon_if_asked() {
    if env::var_os(DAEMON).is_some() {
        daemon();
        process::exit(0);
    }
}

/// Serves one script per request on stdin, until stdin ends. A request is
/// u32 split, u32 length and the socket path, then the script; the empty
/// request stops it. Each gets the reply byte 0 once done.
fn daemon() {
    let mut input = io::stdin().lock();
    let mut reply = io::stdout().lock();
    while let Some(start) = read_frame(&mut input) {
        let split = u32::from_le_bytes(start[..4].try_into().unwrap()) as usize;
        let n = u32::from_le_bytes(start[4..8].try_into().unwrap()) as usize;
        let sock = Path::new(OsStr::from_bytes(&start[8..8 + n]));
        let script = &start[8 + n..];
        let listener = UnixListener::bind(sock).expect("a socket for the scripted daemon");
        reply.write_all(&[0]).and_then(|()| reply.flush()).unwrap();
        loop {
            let (s, _) = listener.accept().expect("a connection");
            // the stop request comes before the connection that wakes this
            let mut fds = [PollFd::new(&input, PollFlags::IN)];
            if poll(&mut fds, Some(&Timespec::default())).unwrap_or(0) > 0 {
                read_frame(&mut input);
                break;
            }
            serve(s, script, split);
        }
        reply.write_all(&[0]).and_then(|()| reply.flush()).unwrap();
    }
}

/// Serves `script` to one connection: write it, close the write side, read
/// until the client goes. Bytes from `split` on wait for the end of an
/// upload, as a daemon reads it all before it answers; Nix's client asserts
/// when the answer comes first (nix-conformance nix-bugs/).
fn serve(mut s: UnixStream, script: &[u8], split: usize) {
    let split = split.min(script.len());
    let _ = s.write_all(&script[..split]);
    let mut got = Vec::new();
    let mut sink = [0u8; 4096];
    if split < script.len() {
        while !got.ends_with(&UPLOAD_END) {
            match s.read(&mut sink) {
                Ok(n) if n > 0 => got.extend_from_slice(&sink[..n]),
                _ => break,
            }
        }
        let _ = s.write_all(&script[split..]);
    }
    let _ = s.shutdown(Shutdown::Write);
    while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
}

fn read_frame(input: &mut impl Read) -> Option<Vec<u8>> {
    let mut n = [0u8; 4];
    input.read_exact(&mut n).ok()?;
    let mut b = vec![0u8; u32::from_le_bytes(n) as usize];
    input.read_exact(&mut b).ok()?;
    Some(b)
}

/// The answer line of iets's client to `name args` against the daemon at
/// `sock` (nix-conformance SPEC.md), or why iets refused the reply.
pub fn run_iets(sock: &Path, name: &str, args: &[String]) -> Result<String, String> {
    let answer = || -> iets::error::Result<String> {
        let mut quiet = |_| {};
        let mut c = Connection::open_with(
            &Transport::Unix(sock.to_path_buf()),
            &Options::default(),
            &mut quiet,
        )?;
        Ok(match name {
            "connect" => format!(
                "ok minor={} version={} trusted={}",
                c.minor,
                c.daemon_nix_version,
                match c.trusted {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                }
            ),
            "path-info" => match c.query_path_info(&args[0], &mut quiet)? {
                None => "none".into(),
                Some(i) => format!(
                    "info deriver={} nar={} refs={} time={} size={} ultimate={} sigs={} ca={}",
                    i.deriver,
                    i.nar_hash,
                    set(i.references),
                    i.registration_time,
                    i.nar_size,
                    u8::from(i.ultimate),
                    set(i.sigs),
                    i.ca
                ),
            },
            "valid-paths" => format!("paths {}", set(c.query_valid_paths(args, &mut quiet)?)),
            "missing" => {
                let m = c.query_missing(args, &mut quiet)?;
                format!(
                    "missing build={} subst={} unknown={} download={} nar={}",
                    set(m.will_build),
                    set(m.will_substitute),
                    set(m.unknown),
                    m.download_size,
                    m.nar_size
                )
            }
            "output-map" => {
                let m = c.query_derivation_output_map(&args[0], &mut quiet)?;
                let v = m
                    .into_iter()
                    .map(|(k, p)| format!("{k}={}", p.unwrap_or_else(|| "-".into())))
                    .collect();
                format!("outputs {}", set(v))
            }
            "substitutable" => format!(
                "subst {}",
                set(c.query_substitutable_path_infos(args, &mut quiet)?)
            ),
            "substitute" => format!("paths {}", set(c.substitute_paths(args, &mut quiet)?)),
            "build" => {
                let mut o = String::from("built");
                for b in c.build_paths_with_results(args, &mut quiet)? {
                    o += &format!("\n{} status={}", b.path, b.status);
                    // Nix keeps the message of a failure only
                    if !b.success() {
                        o += &format!(" msg={}", b.msg);
                    }
                }
                o
            }
            "indirect-root" => {
                c.add_indirect_root(&args[0], &mut quiet)?;
                "ok".into()
            }
            "nar" => {
                let mut got = String::new();
                c.nar_from_path(&args[0], &mut quiet, &mut |r| {
                    let mut tee = Tee(r, Vec::new());
                    let n = nar::parse_length(&mut tee)?;
                    let h = hash_bytes(HashAlgo::Sha256, &tee.1[..n as usize]);
                    got = format!("nar {n} {}", h.to_base16());
                    Ok(())
                })?;
                got
            }
            "add-multiple" => {
                c.add_multiple_to_store(false, &mut |w| w.write_all(&[0; 8]), &mut quiet)?;
                "ok".into()
            }
            _ => "unsupported".into(),
        })
    };
    answer().map_err(|e| e.to_string())
}

/// What a NAR reader took from the connection.
struct Tee<'a>(&'a mut dyn Read, Vec<u8>);

impl Read for Tee<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        self.1.extend_from_slice(&buf[..n]);
        Ok(n)
    }
}

/// A set on Nix's side: sorted and without duplicates.
fn set(mut v: Vec<String>) -> String {
    v.sort();
    v.dedup();
    v.join(",")
}

struct W(Vec<u8>);

impl W {
    fn u64(&mut self, n: u64) {
        self.0.extend_from_slice(&n.to_le_bytes());
    }
    fn str(&mut self, s: &[u8]) {
        self.u64(s.len() as u64);
        self.0.extend_from_slice(s);
        self.0.resize(self.0.len().next_multiple_of(8), 0);
    }
    fn strs(&mut self, v: &[Vec<u8>]) {
        self.u64(v.len() as u64);
        for s in v {
            self.str(s);
        }
    }
}

fn r<T>(x: Result<T, libfuzzer_sys::arbitrary::Error>, d: T) -> T {
    x.unwrap_or(d)
}

/// The operation, the daemon's whole side of the conversation, and where
/// the daemon waits for an upload (`serve`).
fn generate(u: &mut Unstructured) -> (Op, Vec<u8>, usize) {
    let pick = |u: &mut Unstructured| PATHS[r(u.choose_index(PATHS.len()), 0)].to_string();
    let drv = |u: &mut Unstructured| {
        r(
            u.choose(&[
                format!("{}!out", PATHS[2]),
                format!("{}!*", PATHS[2]),
                PATHS[0].to_string(),
            ]),
            &String::new(),
        )
        .clone()
    };
    let op = match r(u.int_in_range(0u8..=10), 0) {
        0 => Op {
            name: "connect",
            args: vec![],
        },
        1 => Op {
            name: "path-info",
            args: vec![pick(u)],
        },
        2 => Op {
            name: "valid-paths",
            args: vec![pick(u), pick(u)],
        },
        3 => Op {
            name: "missing",
            args: vec![format!("{}!out", PATHS[2])],
        },
        4 => Op {
            name: "output-map",
            args: vec![PATHS[2].to_string()],
        },
        5 => Op {
            name: "substitutable",
            args: vec![pick(u)],
        },
        6 => Op {
            name: "substitute",
            args: vec![pick(u), pick(u)],
        },
        7 => Op {
            name: "build",
            args: vec![drv(u)],
        },
        8 => Op {
            name: "indirect-root",
            args: vec!["/tmp/iets-fuzz-wire-root".into()],
        },
        9 => Op {
            name: "nar",
            args: vec![pick(u)],
        },
        _ => Op {
            name: "add-multiple",
            args: vec![],
        },
    };
    let mut w = W(Vec::new());
    w.u64(if r(u.ratio(1u8, 64), false) {
        0
    } else {
        MAGIC_2
    });
    let major = if r(u.ratio(1u8, 64), false) { 2 } else { 1 };
    // iets speaks 1.35 and newer only, on purpose (docs/reference/divergences.md)
    let minor: u64 = if r(u.ratio(1u8, 4), false) {
        r(u.int_in_range(35..=40), 38)
    } else {
        38
    };
    w.u64((major << 8) | minor);
    let proto = minor.min(38);
    if proto >= 38 {
        let n = r(u.int_in_range(0u8..=2), 0);
        let feats: Vec<Vec<u8>> = (0..n).map(|_| bytes(u, b"f")).collect();
        w.strs(&feats);
    }
    if proto >= 33 {
        w.str(&bytes(u, b"2.34.8"));
    }
    if proto >= 35 {
        w.u64(r(u.choose(&[0, 1, 2, 3]), &0).to_owned());
    }
    // after the handshake (remote-store.cc:initConnection), after SetOptions,
    // then after the operation
    stderr(u, &mut w, proto);
    stderr(u, &mut w, proto);
    let split = if op.name == "add-multiple" {
        w.0.len()
    } else {
        usize::MAX
    };
    if op.name != "connect" {
        stderr(u, &mut w, proto);
        reply(u, &mut w, &op, proto);
    }
    if r(u.ratio(1u8, 16), false) {
        let cut = r(u.int_in_range(0..=w.0.len()), w.0.len());
        w.0.truncate(cut);
    }
    (op, w.0, split)
}

fn bytes(u: &mut Unstructured, default: &[u8]) -> Vec<u8> {
    if r(u.ratio(3u8, 4), true) {
        default.to_vec()
    } else {
        r(u.arbitrary::<&[u8]>(), default).to_vec()
    }
}

/// A store path, now and then a wrong one.
fn path(u: &mut Unstructured) -> Vec<u8> {
    match r(u.int_in_range(0u8..=15), 0) {
        0 => b"/nix/store/not-a-store-path".to_vec(),
        1 => b"/etc/passwd".to_vec(),
        // never empty: an empty store path makes Nix's canonPath assert (nix-conformance
        // nix-bugs/); an empty deriver is "none" and is made by the caller
        2 => match r(u.arbitrary::<&[u8]>(), b"") {
            b"" => b"x".to_vec(),
            b => b.to_vec(),
        },
        _ => PATHS[r(u.choose_index(PATHS.len()), 0)].as_bytes().to_vec(),
    }
}

fn paths(u: &mut Unstructured) -> Vec<Vec<u8>> {
    (0..r(u.int_in_range(0u8..=3), 0))
        .map(|_| path(u))
        .collect()
}

/// Log lines, activities and results, ended by `LAST` or by an error.
fn stderr(u: &mut Unstructured, w: &mut W, proto: u64) {
    for _ in 0..r(u.int_in_range(0u8..=4), 0) {
        match r(u.int_in_range(0u8..=5), 0) {
            0 => {
                w.u64(NEXT);
                w.str(&bytes(u, b"building\n"));
            }
            1 => {
                w.u64(START);
                w.u64(r(u.int_in_range(1u64..=3), 1));
                w.u64(3);
                w.u64(r(u.choose(&[100, 105, 0, 999]), &105).to_owned());
                w.str(&bytes(u, b"building '/nix/store/x.drv'"));
                fields(u, w, 0);
                w.u64(r(u.int_in_range(0u64..=2), 0));
            }
            2 => {
                w.u64(STOP);
                w.u64(r(u.int_in_range(1u64..=3), 1));
            }
            3 => {
                w.u64(RESULT);
                w.u64(r(u.int_in_range(1u64..=3), 1));
                let ty = r(u.choose(&[101, 104, 107, 0]), &101).to_owned();
                w.u64(ty);
                // Nix's SimpleLogger takes the first field of a log line
                // unchecked and crashes without one (nix-conformance nix-bugs/)
                fields(u, w, if matches!(ty, 101 | 107) { 1 } else { 0 });
            }
            4 => {
                w.u64(READ);
                w.u64(8);
            }
            _ => w.u64(r(u.arbitrary::<u64>(), 0)),
        }
    }
    if r(u.ratio(1u8, 8), false) {
        w.u64(ERROR);
        if proto >= 26 {
            // any other type makes Nix's readError assert (nix-conformance nix-bugs/)
            w.str(b"Error");
            w.u64(0);
            w.str(b"Error");
            w.str(&bytes(u, b"path '/nix/store/x' is not valid"));
            w.u64(0);
            let n = r(u.int_in_range(0u8..=2), 0);
            w.u64(n.into());
            for _ in 0..n {
                w.u64(0);
                w.str(&bytes(u, b"while doing it"));
            }
        } else {
            w.str(&bytes(u, b"failed"));
            w.u64(1);
        }
    } else {
        w.u64(LAST);
    }
}

fn fields(u: &mut Unstructured, w: &mut W, min: u8) {
    let n = r(u.int_in_range(min..=3), min);
    w.u64(n.into());
    for _ in 0..n {
        match r(u.int_in_range(0u8..=8), 0) {
            0 => w.u64(7),
            1..=4 => {
                w.u64(0);
                w.u64(r(u.arbitrary::<u64>(), 0));
            }
            _ => {
                w.u64(1);
                w.str(&bytes(u, b"/nix/store/x.drv"));
            }
        }
    }
}

fn reply(u: &mut Unstructured, w: &mut W, op: &Op, proto: u64) {
    match op.name {
        "path-info" => {
            w.u64(r(u.choose(&[1, 1, 1, 0, 2]), &1).to_owned());
            w.str(&if r(u.ratio(1u8, 3), false) {
                path(u)
            } else {
                Vec::new()
            });
            let nar: Vec<u8> = match r(u.int_in_range(0u8..=7), 0) {
                0 => format!("sha256:{HASH}").into_bytes(),
                1 => b"1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s".to_vec(),
                2 => r(u.arbitrary::<&[u8]>(), b"").to_vec(),
                _ => HASH.as_bytes().to_vec(),
            };
            w.str(&nar);
            w.strs(&paths(u));
            w.u64(r(u.arbitrary::<u64>(), 0));
            w.u64(r(u.arbitrary::<u64>(), 0));
            w.u64(r(u.choose(&[0, 1, 2]), &0).to_owned());
            let sigs: Vec<Vec<u8>> = (0..r(u.int_in_range(0u8..=2), 0))
                .map(|_| match r(u.int_in_range(0u8..=3), 0) {
                    0 => b"no-colon".to_vec(),
                    1 => r(u.arbitrary::<&[u8]>(), b"").to_vec(),
                    _ => b"cache.nixos.org-1:dGVzdA==".to_vec(),
                })
                .collect();
            w.strs(&sigs);
            let ca: Vec<u8> = match r(u.int_in_range(0u8..=5), 0) {
                0 => format!("fixed:r:sha256:{HASH}").into_bytes(),
                1 => format!("text:sha256:{HASH}").into_bytes(),
                2 => r(u.arbitrary::<&[u8]>(), b"").to_vec(),
                _ => Vec::new(),
            };
            w.str(&ca);
        }
        "valid-paths" | "substitute" => w.strs(&paths(u)),
        "build" => {
            let n = r(u.int_in_range(0u8..=2), 1);
            w.u64(n.into());
            for _ in 0..n {
                build_result(u, w, proto);
            }
        }
        "indirect-root" => w.u64(r(u.choose(&[1, 1, 0, 1 << 32, u64::MAX]), &1).to_owned()),
        "nar" => {
            let mut nar = Vec::new();
            nar::dump_string(&bytes(u, b"hello"), &mut nar).expect("a NAR in memory");
            if r(u.ratio(1u8, 8), false) && !nar.is_empty() {
                let i = r(u.choose_index(nar.len()), 0);
                nar[i] ^= r(u.arbitrary::<u8>(), 1) | 1;
            }
            if r(u.ratio(1u8, 8), false) {
                nar.extend_from_slice(r(u.arbitrary::<&[u8]>(), b"x"));
            }
            w.0.extend_from_slice(&nar);
        }
        "missing" => {
            for _ in 0..3 {
                w.strs(&paths(u));
            }
            w.u64(r(u.arbitrary::<u64>(), 0));
            w.u64(r(u.arbitrary::<u64>(), 0));
        }
        "output-map" => {
            let n = r(u.int_in_range(0u8..=3), 0);
            w.u64(n.into());
            for _ in 0..n {
                w.str(&bytes(u, b"out"));
                w.str(&if r(u.ratio(1u8, 4), false) {
                    Vec::new()
                } else {
                    path(u)
                });
            }
        }
        "substitutable" => {
            let n = r(u.int_in_range(0u8..=2), 0);
            w.u64(n.into());
            for _ in 0..n {
                w.str(&path(u));
                w.str(&if r(u.ratio(1u8, 2), false) {
                    path(u)
                } else {
                    Vec::new()
                });
                w.strs(&paths(u));
                w.u64(r(u.arbitrary::<u64>(), 0));
                w.u64(r(u.arbitrary::<u64>(), 0));
            }
        }
        _ => {}
    }
}

/// worker-protocol.cc:Serialise<KeyedBuildResult>, now and then wrong.
fn build_result(u: &mut Unstructured, w: &mut W, proto: u64) {
    let d = PATHS[2];
    let mut path: Vec<u8> = match r(u.int_in_range(0u8..=11), 0) {
        0 => format!("{d}!out,dev,out").into_bytes(),
        1 => format!("{d}!*").into_bytes(),
        2 => PATHS[0].as_bytes().to_vec(),
        3 => format!("{d}!out!bin").into_bytes(),
        4 => format!("{d}!").into_bytes(),
        5 => format!("{d}!.").into_bytes(),
        6 => path(u),
        7 => r(u.arbitrary::<&[u8]>(), b"x").to_vec(),
        _ => format!("{d}!out").into_bytes(),
    };
    // Nix parses what comes before the first `!` as a store path, and an
    // empty one makes canonPath assert (nix-conformance nix-bugs/)
    if path.first().is_none_or(|&c| c == b'!') {
        path.insert(0, b'x');
    }
    w.str(&path);
    w.u64(match r(u.int_in_range(0u8..=7), 0) {
        0 => r(u.int_in_range(0..=14), 0),
        1 => r(u.choose(&[15, 255, 256]), &15).to_owned(),
        2 => r(u.arbitrary::<u64>(), 0),
        _ => r(u.choose(&[0, 1, 2, 3, 9, 13]), &0).to_owned(),
    });
    w.str(&bytes(u, b"builder for '/nix/store/x.drv' failed"));
    w.u64(r(u.choose(&[1, 1, 0, 1 << 32]), &1).to_owned());
    w.u64(r(u.choose(&[0, 0, 1, 2]), &0).to_owned());
    for _ in 0..2 {
        w.u64(num(u));
    }
    if proto >= 37 {
        for _ in 0..2 {
            let tag = r(u.choose(&[0, 1, 1, 2, 256]), &0).to_owned();
            w.u64(tag);
            if tag == 1 {
                w.u64(num(u));
            }
        }
    }
    let n = r(u.int_in_range(0u8..=2), 0);
    w.u64(n.into());
    for _ in 0..n {
        w.str(&match r(u.int_in_range(0u8..=5), 0) {
            0 => format!("{HASH}!out").into_bytes(),
            1 => b"no-separator".to_vec(),
            2 => r(u.arbitrary::<&[u8]>(), b"").to_vec(),
            _ => format!("sha256:{HASH}!out").into_bytes(),
        });
        w.str(&realisation(u));
    }
}

/// A time or a count: small, now and then above i64::MAX.
fn num(u: &mut Unstructured) -> u64 {
    match r(u.int_in_range(0u8..=7), 0) {
        0 => r(u.arbitrary::<u64>(), 0),
        1 => 1 << 63,
        _ => r(u.int_in_range(0..=1_800_000_000), 0),
    }
}

/// The JSON of realisation.cc, now and then wrong.
fn realisation(u: &mut Unstructured) -> Vec<u8> {
    let out = &PATHS[0]["/nix/store/".len()..];
    let id = format!("sha256:{HASH}!out");
    let sig = "cache.nixos.org-1:dGVzdA==";
    match r(u.int_in_range(0u8..=11), 0) {
        0 => format!(r#"{{"id":"{id}","outPath":"{out}"}}"#),
        1 => format!(r#"{{"id":"{id}","outPath":"{out}","signatures":null}}"#),
        2 => format!(r#"{{"id":"{id}","outPath":"{}"}}"#, PATHS[0]),
        3 => format!(r#"{{"outPath":"{out}"}}"#),
        4 => "[]".into(),
        5 => format!(r#"{{"id":"{id}","outPath":"{out}","signatures":["no-colon"]}}"#),
        6 => format!(r#"{{"id":"{HASH}!out","outPath":"{out}"}}"#),
        7 => return r(u.arbitrary::<&[u8]>(), b"").to_vec(),
        _ => format!(
            r#"{{"id":"{id}","outPath":"{out}","signatures":["{sig}"],"dependentRealisations":{{}}}}"#
        ),
    }
    .into_bytes()
}
