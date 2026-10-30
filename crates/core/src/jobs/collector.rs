//! `--max-memory-size`: nix-eval-jobs's worker restarts. The main process
//! runs one collector thread per `--workers`, each driving a worker
//! process (`iets eval-jobs --worker ...`) through a line protocol; a
//! worker whose peak RSS crosses the limit finishes its instantiation
//! batch, says `restart`, and the collector spawns a fresh one.
//!
//! Collector to worker: `do <JSON attr path>` or `exit`. Worker to
//! collector: `next` when ready for a path; after `do`: `{"attrs":[..]}`
//! for a set to recurse into, `ok` for a skipped node or a queued job, or
//! an error line; job lines at any time (the worker's instantiator prints
//! them once their batch is registered); `restart` last when it leaves.
//! `nix-config`, before the first `next`, when the root flake has a
//! `nixConfig`: a worker is hushed, so the collector gives the warnings
//! that wait for one.

use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;
use std::{env, fs, thread};

use super::instantiate::Instantiator;
use super::line::{Line, json_str, json_str_list, split_object, stats_json};
use super::{
    FNV_BASIS, Node, admit, attr_join, config, error_line, find_along_attr_path, fnv_step, node,
};
use crate::error::{Error, Result};
use crate::eval::Evaluator;
use crate::ignored;
use crate::log;
use crate::print::lexicographic_order;
use crate::runtime::Word;
use crate::session::{self, Session};
use crate::sys::process;

pub const MSG_NEXT: &str = "next";
pub const MSG_RESTART: &str = "restart";
pub const MSG_OK: &str = "ok";
pub const MSG_EXIT: &str = "exit";
pub const MSG_DO: &str = "do ";
pub const MSG_NIX_CONFIG: &str = "nix-config";

pub type Out = Arc<Mutex<Box<dyn Write + Send>>>;

pub fn write_out(out: &Out, line: &[u8]) {
    let mut o = out.lock().unwrap();
    let _ = o.write_all(line);
    let _ = o.write_all(b"\n");
    let _ = o.flush();
}

/// `VmHWM` of this process, in MB (Linux only; 0 elsewhere).
pub fn peak_rss_mb() -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<u64>().ok())
        })
        .map_or(0, |kb| kb / 1024)
}

/// The worker side (worker.cc:worker): serve `do` requests from stdin until
/// `exit` or the memory limit.
pub fn run_worker(
    ev: &mut Evaluator,
    root: Word,
    mut inst: Option<Instantiator>,
    max_memory_mb: u64,
    out: &Out,
) -> Result<()> {
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let chunk_depth = config().opts.chunk_depth;
    let flush = |inst: &mut Option<Instantiator>| -> Result<()> {
        if let Some(i) = inst.take() {
            i.finish()?;
        }
        Ok(())
    };
    loop {
        write_out(out, MSG_NEXT.as_bytes());
        let Some(Ok(cmd)) = lines.next() else {
            break;
        };
        if cmd == MSG_EXIT {
            break;
        }
        let Some(js) = cmd.strip_prefix(MSG_DO) else {
            return Err(Error::Protocol(format!(
                "worker error: received invalid command '{cmd}'"
            )));
        };
        let path: Vec<String> = serde_json::from_str(js)
            .map_err(|e| Error::Protocol(format!("worker error: bad path: {e}")))?;
        let hash = path
            .iter()
            .fold(FNV_BASIS, |h, n| fnv_step(h, n.as_bytes()));
        let depth = path.len() as u32;
        let response: Vec<u8> = if depth == chunk_depth && !admit(hash, depth, false) {
            MSG_OK.into()
        } else {
            let (t, alloc) = (Instant::now(), ev.arena.bytes_allocated);
            let r = find_along_attr_path(ev, root, &path).and_then(|v| node(ev, v, &path, hash));
            let stats = stats_json(
                t.elapsed().as_millis() as u64,
                ev.arena.bytes_allocated.saturating_sub(alloc) as u64,
            );
            match r {
                Ok(Node::Job(mut job)) => {
                    job.extra.insert("stats".into(), stats);
                    let line = Line::Job(job);
                    match &inst {
                        Some(i) => i.push(line),
                        None => write_out(out, &line.render()),
                    }
                    MSG_OK.into()
                }
                Ok(Node::Recurse(attrs)) => {
                    let names = lexicographic_order(ev, attrs);
                    let mut v = b"{\"attr\":".to_vec();
                    v.extend(json_str(&attr_join(&path)));
                    v.extend_from_slice(b",\"attrPath\":");
                    v.extend(json_str_list(path.iter()));
                    v.extend_from_slice(b",\"attrs\":");
                    v.extend(json_str_list(names.map(|a| ev.interner.name_str(a.sym))));
                    v.push(b'}');
                    v
                }
                Ok(Node::Skip) => MSG_OK.into(),
                Err(_) if !admit(hash, depth, true) => MSG_OK.into(),
                Err(e) => {
                    let mut line = error_line(ev, &path, &e);
                    line.stats = Some(stats);
                    line.render()
                }
            }
        };
        write_out(out, &response);
        if peak_rss_mb() > max_memory_mb {
            flush(&mut inst)?;
            write_out(out, MSG_RESTART.as_bytes());
            return Ok(());
        }
    }
    flush(&mut inst)
}

pub type Sink = Arc<Mutex<Box<dyn FnMut(Line) + Send>>>;

struct State {
    todo: BTreeSet<Vec<String>>,
    active: BTreeSet<Vec<String>>,
    failed: Option<String>,
}

type Shared = Arc<(Mutex<State>, Condvar)>;

enum Control {
    Next,
    Restart,
    Ok,
    Attrs(Vec<String>),
    Error(Line),
}

struct Proc {
    child: Child,
    to: ChildStdin,
    from: BufReader<ChildStdout>,
}

impl Proc {
    fn spawn(args: &[String]) -> Result<Proc> {
        Session::current()
            .jobs
            .spawned
            .fetch_add(1, Ordering::Relaxed);
        let exe = env::current_exe().map_err(|e| Error::io("current_exe", e))?;
        let mut child = Command::new(exe)
            .arg("eval-jobs")
            .arg("--worker")
            .env("IETS_SESSION_ID", log::session_id())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::io("spawning worker process", e))?;
        let to = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("worker process has no stdin pipe".into()))?;
        let out = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("worker process has no stdout pipe".into()))?;
        let from = BufReader::with_capacity(1 << 20, out);
        Ok(Proc { child, to, from })
    }

    fn send(&mut self, line: &str) -> Result<()> {
        self.to
            .write_all(line.as_bytes())
            .and_then(|_| self.to.write_all(b"\n"))
            .and_then(|_| self.to.flush())
            .map_err(|e| Error::io("writing to worker", e))
    }

    /// nix-eval-jobs.cc:handleBrokenWorkerPipe: what a dead worker means.
    fn broken(&mut self, what: &str) -> String {
        match self.child.wait() {
            Ok(st) if st.code() == Some(1) => format!(
                "while {what}, evaluation worker exited with exit code 1, (possible infinite recursion)"
            ),
            Ok(st) if process::signal(&st) == Some(9) => format!(
                "while {what}, evaluation worker got killed by SIGKILL, maybe memory limit reached?"
            ),
            Ok(st) => match process::signal(&st) {
                Some(s) => format!("while {what}, evaluation worker crashed with signal {s}"),
                None => format!(
                    "while {what}, evaluation worker exited with {}",
                    st.code().unwrap_or(-1)
                ),
            },
            Err(e) => format!("while {what}, waitpid for evaluation worker failed: {e}"),
        }
    }

    /// The next control line; job lines on the way go to `sink`.
    fn read_control(&mut self, sink: &Sink, what: &str) -> Result<Control> {
        let mut buf = Vec::new();
        loop {
            if !read_line(&mut self.from, &mut buf)? {
                return Err(Error::Protocol(self.broken(what)));
            }
            match buf.as_slice() {
                b"next" => return Ok(Control::Next),
                b"restart" => return Ok(Control::Restart),
                b"ok" => return Ok(Control::Ok),
                b"nix-config" => ignored::nix_config_seen(),
                s if s.first() == Some(&b'{') => {
                    let fields = split_object(s).map_err(|e| {
                        Error::Protocol(format!(
                            "Received invalid JSON from worker: {e}\n json: '{}'",
                            String::from_utf8_lossy(s)
                        ))
                    })?;
                    if let Some((_, raw)) = fields.iter().find(|(k, _)| k == "attrs") {
                        let names: Vec<String> = serde_json::from_slice(raw).map_err(|e| {
                            Error::Protocol(format!("Received invalid JSON from worker: {e}"))
                        })?;
                        return Ok(Control::Attrs(names));
                    }
                    if !fields.iter().any(|(k, _)| k == "attr") {
                        let msg = fields
                            .iter()
                            .find(|(k, _)| k == "error")
                            .and_then(|(_, raw)| serde_json::from_slice::<String>(raw).ok())
                            .unwrap_or_default();
                        return Err(Error::Protocol(format!("worker error: {msg}")));
                    }
                    let line = Line::parse(s)?;
                    match line {
                        Line::Error(_) => return Ok(Control::Error(line)),
                        Line::Job(_) => (sink.lock().unwrap())(line),
                    }
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "worker error: unexpected line '{}'",
                        String::from_utf8_lossy(other)
                    )));
                }
            }
        }
    }

    /// Reads the job lines a leaving worker still prints, then reaps it.
    fn drain(mut self, sink: &Sink) -> Result<()> {
        drop(self.to);
        let mut buf = Vec::new();
        while read_line(&mut self.from, &mut buf)? {
            if buf.first() == Some(&b'{') {
                (sink.lock().unwrap())(Line::parse(&buf)?);
            }
        }
        let _ = self.child.wait();
        Ok(())
    }
}

/// The next line without its newline; false at the end of the stream.
fn read_line(from: &mut impl BufRead, buf: &mut Vec<u8>) -> Result<bool> {
    buf.clear();
    let n = from
        .read_until(b'\n', buf)
        .map_err(|e| Error::io("reading from worker", e))?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    Ok(n > 0)
}

fn next_job(shared: &Shared) -> Option<Vec<String>> {
    let (lock, cv) = &**shared;
    let mut st = lock.lock().unwrap();
    loop {
        if (st.todo.is_empty() && st.active.is_empty()) || st.failed.is_some() {
            return None;
        }
        if let Some(p) = st.todo.iter().next().cloned() {
            st.todo.remove(&p);
            st.active.insert(p.clone());
            return Some(p);
        }
        st = cv.wait(st).unwrap();
    }
}

fn collector(shared: &Shared, args: &[String], sink: &Sink) -> Result<()> {
    let mut proc: Option<Proc> = None;
    loop {
        let Some(path) = next_job(shared) else {
            if let Some(mut p) = proc.take() {
                // A worker whose pending status is `restart` is leaving on its own.
                if !matches!(
                    p.read_control(sink, "checking worker process")?,
                    Control::Restart
                ) {
                    p.send(MSG_EXIT)?;
                }
                p.drain(sink)?;
            }
            return Ok(());
        };
        loop {
            let p = match proc {
                Some(ref mut p) => p,
                None => proc.insert(Proc::spawn(args)?),
            };
            match p.read_control(sink, "checking worker process")? {
                Control::Next => break,
                Control::Restart => {
                    if let Some(old) = proc.take() {
                        old.drain(sink)?;
                    }
                }
                _ => {
                    return Err(Error::Protocol(
                        "worker error: unexpected response while idle".into(),
                    ));
                }
            }
        }
        let Some(p) = proc.as_mut() else {
            return Err(Error::Protocol("worker error: no worker process".into()));
        };
        let joined = attr_join(&path);
        p.send(&format!(
            "{MSG_DO}{}",
            String::from_utf8_lossy(&json_str_list(path.iter()))
        ))?;
        let what = format!("reading result for attrPath '{joined}'");
        let new_attrs: Vec<Vec<String>> = match p.read_control(sink, &what)? {
            Control::Ok => Vec::new(),
            Control::Attrs(names) => names
                .into_iter()
                .map(|n| {
                    let mut p = path.clone();
                    p.push(n);
                    p
                })
                .collect(),
            Control::Error(line) => {
                let fatal = matches!(&line, Line::Error(e) if e.fatal);
                let text = match &line {
                    Line::Error(e) => e.error.clone(),
                    Line::Job(_) => String::new(),
                };
                (sink.lock().unwrap())(line);
                if fatal {
                    return Err(Error::Protocol(text));
                }
                Vec::new()
            }
            _ => {
                return Err(Error::Protocol(format!(
                    "worker error: unexpected status for '{joined}'"
                )));
            }
        };
        let (lock, cv) = &**shared;
        let mut st = lock.lock().unwrap();
        st.active.remove(&path);
        st.todo.extend(new_attrs);
        cv.notify_all();
    }
}

/// nix-eval-jobs.cc:main's collector threads: `workers` of them, each with
/// its own worker process, until the tree is walked. Every line reaches
/// `sink` in completion order.
pub fn run_collector(args: &[String], workers: usize, sink: Sink) -> Result<()> {
    let shared: Shared = Arc::new((
        Mutex::new(State {
            todo: BTreeSet::from([Vec::new()]),
            active: BTreeSet::new(),
            failed: None,
        }),
        Condvar::new(),
    ));
    thread::scope(|s| {
        for _ in 0..workers.max(1) {
            let shared = shared.clone();
            let sink = sink.clone();
            s.spawn(session::bind(move || {
                if let Err(e) = collector(&shared, args, &sink) {
                    let (lock, cv) = &*shared;
                    let mut st = lock.lock().unwrap();
                    st.failed.get_or_insert(e.to_string());
                    cv.notify_all();
                }
            }));
        }
    });
    let st = shared.0.lock().unwrap();
    match &st.failed {
        Some(e) => Err(Error::Protocol(e.clone())),
        None => Ok(()),
    }
}
