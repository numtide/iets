//! Everything a command says on stderr goes through here: Nix's plain text,
//! or with `--log-format internal-json` one `@nix {...}` line per message in
//! the format of logging.cc:JSONLogger, plus `code` and `hint` on an error
//! (docs/reference/errors.md). The `json-log-path` file gets every record in either
//! format.
//!
//! The state is a [`Log`], which a [`crate::session::Session`] owns: the
//! functions here act on the one this thread entered.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};

use crate::session::Session;

/// Where one session's messages go and what it saw on the way.
#[derive(Default)]
pub struct Log {
    json: AtomicBool,
    show_trace: AtomicBool,
    /// Under one of Nix's names stderr holds what Nix prints and nothing else.
    classic: AtomicBool,
    json_log_path: OnceLock<String>,
    sink: OnceLock<Option<Mutex<Box<dyn Write + Send>>>>,
    sid: OnceLock<String>,
    next_activity: AtomicU64,
    mismatches: Mutex<Vec<(String, String)>>,
    /// Mismatches a build reported, before `resolve_mismatches` says which of
    /// them are about a fixed-output derivation the client planned.
    pending: Mutex<Vec<(String, String)>>,
    want_mismatch: Mutex<Option<String>>,
    /// Where messages go. `None` writes nowhere: a library says nothing
    /// unless the host asked for it. `iets` points it at stderr.
    err: Mutex<Out>,
    pub progress: crate::progress::Board,
}

#[derive(Default)]
struct Out {
    w: Option<Box<dyn Write + Send>>,
    status: String,
    shown: bool,
    /// A status line drawn here would join a line its writer has not ended.
    mid_line: bool,
}

impl Out {
    fn write(&mut self, bytes: &[u8]) {
        self.take_down();
        if let Some(w) = self.w.as_mut() {
            let _ = w.write_all(bytes);
        }
        if let Some(&last) = bytes.last() {
            self.mid_line = last != b'\n';
        }
        self.draw();
    }

    fn take_down(&mut self) {
        if self.shown
            && let Some(w) = self.w.as_mut()
        {
            let _ = w.write_all(b"\r\x1b[K");
        }
        self.shown = false;
    }

    fn draw(&mut self) {
        let Some(w) = self.w.as_mut() else {
            return;
        };
        if !self.shown && !self.mid_line && !self.status.is_empty() {
            let _ = w.write_all(fit(&self.status).as_bytes());
            self.shown = true;
        }
        let _ = w.flush();
    }
}

/// `line` cut to the terminal's width: a status line that wraps cannot be
/// taken down with one carriage return.
fn fit(line: &str) -> String {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let cols = match unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) } {
        0 if ws.ws_col > 1 => usize::from(ws.ws_col),
        _ => 80,
    };
    line.chars().take(cols - 1).collect()
}

impl Log {
    /// Sends every message to `w`.
    pub fn write_to(&self, w: Box<dyn Write + Send>) {
        self.writer().w = Some(w);
    }

    fn writer(&self) -> std::sync::MutexGuard<'_, Out> {
        self.err.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The status line under the output; empty takes it down.
    pub fn set_status(&self, line: String) {
        let mut o = self.writer();
        if o.status != line {
            o.take_down();
            o.status = line;
            o.draw();
        }
    }

    /// What `iets` itself does, and what a host that wants Nix's own
    /// behaviour does.
    pub fn write_to_stderr(&self) {
        self.write_to(Box::new(std::io::stderr()));
    }

    /// `bytes` as they are, for output a caller already formatted.
    pub fn err(&self, bytes: &[u8]) {
        self.writer().write(bytes);
    }

    /// One line, in place of `eprintln!`.
    pub fn errln(&self, text: &str) {
        self.writer().write(format!("{text}\n").as_bytes());
    }
}

fn cur() -> Arc<Session> {
    Session::current()
}

/// A `Write` onto this thread's session log, for code that must own its
/// destination: the build renderer takes a `Box<dyn Write + Send>`.
pub struct SessionOut(Arc<Session>);

impl Default for SessionOut {
    fn default() -> SessionOut {
        SessionOut(cur())
    }
}

impl Write for SessionOut {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.log.err(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `--log-format F`, loggers.cc:parseLogFormat. Every format but
/// `internal-json` is plain text here.
pub fn set_format(f: &str) -> Result<(), String> {
    let s = cur();
    match f {
        "raw" | "raw-with-logs" => {
            s.log.json.store(false, Ordering::Relaxed);
            s.log.progress.set_mode(crate::progress::Mode::Lines);
        }
        "bar" | "bar-with-logs" => {
            s.log.json.store(false, Ordering::Relaxed);
            s.log.progress.set_mode(crate::progress::Mode::Bar);
        }
        "internal-json" => s.log.json.store(true, Ordering::Relaxed),
        _ => return Err(format!("option 'log-format' has an invalid value '{f}'")),
    }
    Ok(())
}

pub fn json() -> bool {
    cur().log.json.load(Ordering::Relaxed)
}

/// `--show-trace`: the text always has the trace; a record carries it only
/// then, as in Nix.
pub fn set_show_trace() {
    cur().log.show_trace.store(true, Ordering::Relaxed);
}

/// This process answers to one of Nix's names (`cli::classic::dispatch`).
pub fn set_classic() {
    cur().log.classic.store(true, Ordering::Relaxed);
}

/// Whether this process answers to one of Nix's names. Under those, what an
/// experimental feature gates is gated (`builtins::install`); iets's own
/// commands have every feature.
pub fn classic() -> bool {
    cur().log.classic.load(Ordering::Relaxed)
}

/// `json-log-path` from the command line; it wins over nix.conf. Set it
/// before the first line goes out.
pub fn set_json_log_path(path: String) {
    let _ = cur().log.json_log_path.set(path);
}

/// `sid`, the session of every record: `IETS_SESSION_ID` when a parent
/// iets or the user set it, else a UUIDv7 made for this one. The
/// `eval-jobs` workers get their parent's, so one run's records share one
/// id (docs/reference/errors.md).
pub fn session_id() -> String {
    cur().log.sid().to_string()
}

impl Log {
    fn sid(&self) -> &str {
        self.sid.get_or_init(|| {
            std::env::var("IETS_SESSION_ID")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| uuid_v7(std::time::SystemTime::now()))
        })
    }
}

/// RFC 9562 version 7: 48 bits of Unix milliseconds, then random bits.
fn uuid_v7(now: std::time::SystemTime) -> String {
    let ms = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    unsafe { libc::getrandom(b[6..].as_mut_ptr().cast(), 10, 0) };
    b[6] = 0x70 | (b[6] & 0x0f);
    b[8] = 0x80 | (b[8] & 0x3f);
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

impl Log {
    /// A record as it goes out, with its `sid`.
    fn wire(&self, v: &Value) -> String {
        let mut v = v.clone();
        if let Some(o) = v.as_object_mut() {
            o.insert("sid".into(), self.sid().into());
        }
        v.to_string()
    }

    /// logging.cc:applyJSONLogger: a copy of `v`, without the `@nix ` prefix,
    /// to the `json-log-path` file or unix socket. A path that does not open
    /// is reported once and ignored, as Nix does.
    fn tee(&self, v: &Value) {
        if let Some(w) = self.sink() {
            let mut w = w.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writeln!(w, "{}", self.wire(v));
        }
    }

    fn sink(&self) -> Option<&Mutex<Box<dyn Write + Send>>> {
        // Config::read, not load: a warning would re-enter this get_or_init
        self.sink
            .get_or_init(|| {
                let path = self.json_log_path.get().cloned().or_else(|| {
                    let c = crate::config::Config::read(&[]).ok()?;
                    c.str("json-log-path").map(str::to_string)
                })?;
                match open_sink(std::path::Path::new(&path)) {
                    Ok(w) => Some(Mutex::new(w)),
                    Err(e) => {
                        let e = crate::error::io_text(&e);
                        self.errln(&format!(
                            "error (ignored): opening log file \"{path}\": {e}"
                        ));
                        None
                    }
                }
            })
            .as_ref()
    }
}

pub(crate) fn open_sink(path: &std::path::Path) -> std::io::Result<Box<dyn Write + Send>> {
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    if std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
        return Ok(Box::new(std::os::unix::net::UnixStream::connect(path)?));
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o644)
        .open(path)?;
    Ok(Box::new(f))
}

impl Log {
    /// One record: `v` under internal-json, else `text` if any.
    fn out(&self, v: &Value, text: Option<&str>) {
        if self.json.load(Ordering::Relaxed) {
            self.errln(&format!("@nix {}", self.wire(v)));
        } else if let Some(t) = text {
            self.errln(&filter_ansi(t, !stderr_is_tty()));
        }
        self.tee(v);
    }
}

fn out(v: &Value, text: Option<&str>) {
    cur().log.out(v, text);
}

/// error.hh:Verbosity, and logging.hh:ActivityType and ResultType, the ones
/// iets emits itself.
pub const LVL_ERROR: u64 = 0;
pub const LVL_INFO: u64 = 3;
pub const LVL_TALKATIVE: u64 = 4;
pub const ACT_UNKNOWN: u64 = 0;
pub const ACT_COPY_PATH: u64 = 100;
pub const ACT_FILE_TRANSFER: u64 = 101;
pub const ACT_REALISE: u64 = 102;
pub const ACT_COPY_PATHS: u64 = 103;
pub const ACT_BUILDS: u64 = 104;
pub const ACT_BUILD: u64 = 105;
pub const ACT_SUBSTITUTE: u64 = 108;
pub const ACT_QUERY_PATH_INFO: u64 = 109;
pub const ACT_FETCH_TREE: u64 = 112;
pub const RES_BUILD_LOG_LINE: u64 = 101;
pub const RES_SET_PHASE: u64 = 104;
pub const RES_PROGRESS: u64 = 105;

/// Whether activity records go anywhere, so nobody builds one for nothing.
pub fn activities() -> bool {
    let s = cur();
    records(&s) || drawn(&s)
}

fn records(s: &Session) -> bool {
    s.log.json.load(Ordering::Relaxed) || s.log.sink().is_some()
}

/// Not under internal-json: its records carry the progress.
fn drawn(s: &Session) -> bool {
    s.log.progress.mode() != crate::progress::Mode::Off && !s.log.json.load(Ordering::Relaxed)
}

/// loggers.cc:makeDefaultLogger, unless `--log-format` chose.
pub fn show_progress() {
    let s = cur();
    if s.log.progress.mode() != crate::progress::Mode::Off {
        return;
    }
    let tty = stderr_is_tty() && std::env::var("TERM").is_ok_and(|t| t != "dumb");
    s.log.progress.set_mode(match tty {
        true => crate::progress::Mode::Bar,
        false => crate::progress::Mode::Lines,
    });
}

/// logging.cc:Activity: the pid in the high half, a counter below.
pub fn activity_id() -> u64 {
    cur().log.next_activity.fetch_add(1, Ordering::Relaxed) + (u64::from(std::process::id()) << 32)
}

fn fields(mut v: Value, f: Vec<Value>) -> Value {
    if !f.is_empty() {
        v["fields"] = f.into();
    }
    v
}

fn start_record(id: u64, level: u64, ty: u64, text: &str, f: Vec<Value>, parent: u64) -> Value {
    let v = json!({"action": "start", "id": id, "level": level, "parent": parent, "text": text, "type": ty});
    fields(v, f)
}

fn result_record(id: u64, ty: u64, f: Vec<Value>) -> Value {
    fields(json!({"action": "result", "id": id, "type": ty}), f)
}

/// logging.cc:JSONLogger::startActivity.
pub fn start(id: u64, level: u64, ty: u64, text: &str, f: Vec<Value>, parent: u64) {
    let s = cur();
    if drawn(&s)
        && let Some(line) = s.log.progress.start(id, level, ty, text, parent)
    {
        s.log.errln(&line);
    }
    if records(&s) {
        s.log
            .out(&start_record(id, level, ty, text, f, parent), None);
    }
}

/// logging.cc:JSONLogger::stopActivity.
pub fn stop(id: u64) {
    let s = cur();
    if drawn(&s) && s.log.progress.stop(id) {
        s.log.set_status(String::new());
    }
    if records(&s) {
        s.log.out(&json!({"action": "stop", "id": id}), None);
    }
}

/// logging.cc:JSONLogger::result.
pub fn result(id: u64, ty: u64, f: Vec<Value>) {
    let s = cur();
    if drawn(&s) {
        s.log.progress.result(id, ty, &f);
    }
    if records(&s) {
        s.log.out(&result_record(id, ty, f), None);
    }
}

/// Status line only: Nix has no record for it.
pub fn detail(id: u64, text: &str) {
    let s = cur();
    if drawn(&s) {
        s.log.progress.detail(id, text);
    }
}

/// No status line until it drops: a child may prompt on the terminal.
pub struct Suspended(Arc<Session>);

pub fn suspend() -> Suspended {
    let s = cur();
    s.log.progress.suspend();
    s.log.set_status(String::new());
    Suspended(s)
}

impl Drop for Suspended {
    fn drop(&mut self) {
        self.0.log.progress.resume();
    }
}

/// An activity of iets's own; it stops when dropped.
pub struct Activity(pub u64);

impl Activity {
    pub fn new(level: u64, ty: u64, text: &str, f: Vec<Value>) -> Activity {
        Activity::under(0, level, ty, text, f)
    }

    pub fn under(parent: u64, level: u64, ty: u64, text: &str, f: Vec<Value>) -> Activity {
        let id = activity_id();
        start(id, level, ty, text, f, parent);
        Activity(id)
    }

    pub fn result(&self, ty: u64, f: Vec<Value>) {
        result(self.0, ty, f);
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        stop(self.0);
    }
}

/// `text` as it goes to stderr, without the newline: itself, or a `msg`
/// record at the verbosity its prefix implies.
pub fn format(text: &str) -> String {
    // the daemon's own messages arrive coloured, and the prefix is what says
    // how severe they are
    let plain = strip_ansi(text);
    let level = if plain.starts_with("error:") || plain.starts_with("trace:") {
        0
    } else if plain.starts_with("warning:") || plain.starts_with("evaluation warning:") {
        1
    } else {
        3
    };
    let v = json!({"action": "msg", "level": level, "msg": text});
    cur().log.render(&v, text)
}

impl Log {
    /// `v` as an `@nix` line under internal-json, else `text`; either way
    /// the `json-log-path` file gets the record.
    fn render(&self, v: &Value, text: &str) -> String {
        self.tee(v);
        if self.json.load(Ordering::Relaxed) {
            format!("@nix {}", self.wire(v))
        } else {
            text.to_string()
        }
    }
}

/// logging.cc:JSONLogger::log: `text` at `level`, whatever its prefix.
pub fn msg(level: u8, text: &str) {
    out(
        &json!({"action": "msg", "level": level, "msg": text}),
        Some(text),
    );
}

/// A line that is not an error record.
pub fn line(text: &str) {
    cur().log.errln(&on_stderr(&format(text)));
}

fn stderr_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

/// logging.cc:SimpleLogger::log filters every line it writes; an
/// internal-json record keeps the bytes it was given.
pub fn on_stderr(text: &str) -> String {
    match json() {
        true => text.to_string(),
        false => filter_ansi(text, !stderr_is_tty()),
    }
}

/// A lint warning, `warning: [NAME] MSG` and maybe `\n  at POS`: the form
/// that goes to stderr, and the record that a cache entry keeps.
pub fn lint(text: &str, site: Option<Site>) -> (String, String) {
    let rest = text.strip_prefix("warning: ").unwrap_or(text);
    let (raw, pos) = match rest.split_once("\n  at ") {
        Some((raw, pos)) => (raw, Some(pos.to_string())),
        None => (rest, None),
    };
    let r = Report {
        level: 1,
        pos,
        site,
        ..Report::new("lint", raw)
    };
    let v = r.record(text);
    (cur().log.render(&v, text), v.to_string())
}

/// The warnings a cache entry recorded, one record on each line, as `lint`
/// puts them out.
pub fn replay_lints(bytes: &[u8]) -> Vec<u8> {
    let s = cur();
    let mut out = String::new();
    for v in String::from_utf8_lossy(bytes)
        .lines()
        .map(serde_json::from_str::<Value>)
    {
        let Ok(v) = v else { continue };
        out.push_str(&s.log.render(&v, v["msg"].as_str().unwrap_or_default()));
        out.push('\n');
    }
    out.into_bytes()
}

/// A place in a file as byte offsets, and the text to put there. The site
/// of a lint finding and the hash that `fix-hashes` wrote have this one
/// shape (docs/reference/errors.md).
#[derive(Debug, Clone, PartialEq)]
pub struct Site {
    pub start: u64,
    /// The offset after the last byte.
    pub end: u64,
    /// The text that replaces those bytes, when the record has an edit.
    pub replacement: Option<String>,
}

/// One error (or, at level 1, warning) for stderr.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The stable code of docs/reference/errors.md.
    pub code: &'static str,
    pub level: u8,
    /// The message, without `error: `.
    pub raw: String,
    /// `file:line:col`, as Nix prints a position.
    pub pos: Option<String>,
    /// Innermost first, each with its position.
    pub trace: Vec<(String, Option<String>)>,
    /// What to do about it. The text shows it for a usage error only.
    pub hint: Option<String>,
    /// Where it is in the file as bytes, and the edit, when they are known.
    pub site: Option<Site>,
}

impl Report {
    pub fn new(code: &'static str, raw: impl Into<String>) -> Report {
        Report {
            code,
            level: 0,
            raw: raw.into(),
            pos: None,
            trace: Vec::new(),
            hint: None,
            site: None,
        }
    }

    /// An evaluation error; `at` names a position, `msg` is the message
    /// and `rewrite` maps a trace message as the caller maps `msg`.
    pub fn eval(
        e: &crate::eval::Error,
        msg: &str,
        at: &dyn Fn(crate::eval::Pos) -> String,
        rewrite: &dyn Fn(&str) -> String,
    ) -> Report {
        let at = |p: crate::eval::Pos| (!p.is_none()).then(|| at(p));
        Report {
            pos: at(e.pos),
            trace: e.trace.iter().map(|(p, m)| (rewrite(m), at(*p))).collect(),
            hint: e.hint.clone(),
            ..Report::new(e.kind.code(), msg)
        }
    }

    /// The text Nix-style commands print, without the last newline.
    pub fn text(&self) -> String {
        let mut s = format!("error: {}", self.raw);
        if let Some(p) = &self.pos {
            s.push_str(&format!("\n       at {p}"));
        }
        for (m, p) in &self.trace {
            match p {
                Some(p) => s.push_str(&format!("\n       … {m} at {p}")),
                None => s.push_str(&format!("\n       … {m}")),
            }
        }
        if let Some(h) = self.hint.as_ref().filter(|_| self.code == "usage") {
            s.push('\n');
            s.push_str(h);
        }
        s
    }

    /// logging.cc:JSONLogger::logEI; `text` is the `msg` field.
    pub fn record(&self, text: &str) -> Value {
        let mut v = json!({
            "action": "msg",
            "code": self.code,
            "level": self.level,
            "msg": text,
            "raw_msg": self.raw,
        });
        let o = v.as_object_mut().expect("an object");
        o.extend(pos_fields(self.pos.as_deref()));
        if let Some(h) = &self.hint {
            o.insert("hint".into(), h.as_str().into());
        }
        if let Some(s) = &self.site {
            o.insert("start".into(), s.start.into());
            o.insert("end".into(), s.end.into());
            if let Some(r) = &s.replacement {
                o.insert("replacement".into(), r.as_str().into());
            }
        }
        if cur().log.show_trace.load(Ordering::Relaxed) && !self.trace.is_empty() {
            let t = self.trace.iter().map(|(m, p)| {
                let mut f = serde_json::Map::from_iter(pos_fields(p.as_deref()));
                f.insert("raw_msg".into(), m.as_str().into());
                Value::Object(f)
            });
            o.insert("trace".into(), t.collect());
        }
        v
    }

    /// Prints the report and returns the exit code: 2 for a usage error of
    /// a native command, 1 for the rest.
    pub fn emit(&self) -> i32 {
        let text = self.text();
        out(&self.record(&text), Some(&text));
        if self.code == "usage" && crate::cli::help::command().is_some() {
            2
        } else {
            1
        }
    }
}

/// logging.cc:to_json(Pos): the line and column come off the end of `pos`.
fn pos_fields(pos: Option<&str>) -> [(String, Value); 3] {
    let lc = pos.and_then(|p| {
        let (rest, col) = p.rsplit_once(':')?;
        let (_, line) = rest.rsplit_once(':')?;
        Some((line.parse::<u32>().ok()?, col.parse::<u32>().ok()?))
    });
    [
        ("line".into(), lc.map(|l| l.0).into()),
        ("column".into(), lc.map(|l| l.1).into()),
        ("file".into(), pos.into()),
    ]
}

/// build-result.cc:failingExitStatus of one failed build, from its
/// worker-protocol status. The daemon sends a hash mismatch as
/// OutputRejected, so its message tells it apart.
pub fn build_exit(status: u64, msg: &str) -> i32 {
    match status {
        // 10 is DependencyFailed: a goal of the run failed, which is the same
        // flag Nix's worker sets for the build that failed itself
        3 | 4 | 10 => 100,
        8 => 101,
        5 if msg.contains(MISMATCH) => 102,
        12 => 104,
        _ => 1,
    }
}

/// failingExitStatus over several builds: the flags of the special codes
/// combine, any other failure is 1.
pub fn combine_exit(a: i32, b: i32) -> i32 {
    if a >= 100 && b >= 100 {
        a | b
    } else {
        a.max(b)
    }
}

/// What every fixed-output hash mismatch message starts with.
pub const MISMATCH: &str = "hash mismatch in fixed-output derivation";

/// The `specified` and `got` hashes of a fixed-output hash mismatch
/// message.
pub fn mismatch(raw: &str) -> Option<(String, String)> {
    let field = |k: &str| {
        raw.lines()
            .find_map(|l| l.trim().strip_prefix(k))
            .map(|h| h.trim().to_string())
    };
    Some((field("specified:")?, field("got:")?))
}

/// `iets prefetch` builds a derivation whose hash is wrong on purpose: a
/// mismatch of a derivation called `name` is its answer, not a failure.
pub fn want_mismatch(name: &str) {
    *cur()
        .log
        .want_mismatch
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(format!("-{name}.drv"));
}

/// Whether `text` is about the derivation `want_mismatch` named.
fn wanted(log: &Log, text: &str) -> bool {
    let want = log.want_mismatch.lock().unwrap_or_else(|e| e.into_inner());
    want.as_ref().is_some_and(|n| text.contains(n.as_str()))
}

/// The daemon reports a resolved derivation's mismatch as a line of its own,
/// not as the build's result, so the line is read here too. The resolved copy
/// has a drv path of its own, so only the pair of hashes carries over; that
/// is all `fix-hashes` and `prefetch` need.
///
/// The pair is only a candidate: `resolve_mismatches` keeps the ones that name
/// a fixed-output derivation of the plan. Anything can print this text.
pub fn note_mismatch(text: &str) {
    if text.contains(MISMATCH)
        && let Some(m) = mismatch(&strip_ansi(text))
    {
        push_mismatch(&cur().log, m);
    }
}

/// One `(specified, got)` pair, skipping one that is already there: a
/// mismatch can reach both paths that note one.
fn push_mismatch(log: &Log, m: (String, String)) {
    let mut v = log.pending.lock().unwrap_or_else(|e| e.into_inner());
    if !v.contains(&m) {
        v.push(m);
    }
}

/// Decides the candidates: `planned` says whether a `specified` hash is the
/// output hash of a fixed-output derivation the client is building. Nothing
/// else counts, because a build log and an evaluation can print the same
/// text, and `fix-hashes` writes the answer into a file of the user.
/// Returns how many were kept.
pub fn resolve_mismatches(planned: &dyn Fn(&str) -> bool) -> usize {
    let log = &cur().log;
    let candidates = std::mem::take(&mut *log.pending.lock().unwrap_or_else(|e| e.into_inner()));
    let mut kept = 0;
    let mut v = log.mismatches.lock().unwrap_or_else(|e| e.into_inner());
    for m in candidates {
        if planned(&m.0) {
            kept += 1;
            if !v.contains(&m) {
                v.push(m);
            }
        }
    }
    kept
}

/// The `(specified, got)` pairs of the hash mismatches reported since the
/// last call.
pub fn take_mismatches() -> Vec<(String, String)> {
    std::mem::take(
        &mut cur()
            .log
            .mismatches
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    )
}

/// `warning: RAW`, a level-1 record with its own code.
pub fn warning(code: &'static str, raw: String) {
    let text = format!("warning: {raw}");
    let r = Report {
        level: 1,
        ..Report::new(code, raw)
    };
    out(&r.record(&text), Some(&text));
}

/// What made a tracing-cache entry stale, as `re-evaluating: RAW`. Level 3:
/// nothing is wrong, and an edit invalidates an entry on every normal run
/// (docs/reference/errors.md). Under a classic name only `json-log-path`
/// gets it: Nix invalidates its own evaluation cache without a word
/// (docs/reference/divergences.md).
pub fn invalidated(raw: String) {
    let text = format!("re-evaluating: {raw}");
    let r = Report {
        level: 3,
        ..Report::new("cache-invalidated", raw)
    };
    let s = cur();
    if s.log.classic.load(Ordering::Relaxed) {
        s.log.tee(&r.record(&text));
    } else {
        out(&r.record(&text), Some(&text));
    }
}

/// A hash that `fix-hashes` rewrote at `pos`, `file:line:col`, which is
/// byte `at` of the file before the edit.
pub fn fixed(pos: &str, at: usize, old: &str, new: &str) {
    let text = format!("fixed: {pos}: '{old}' is now '{new}'");
    out(&fixed_report(pos, at, old, new).record(&text), Some(&text));
}

fn fixed_report(pos: &str, at: usize, old: &str, new: &str) -> Report {
    Report {
        level: 3,
        pos: Some(pos.to_string()),
        site: Some(Site {
            start: at as u64,
            end: (at + old.len()) as u64,
            replacement: Some(new.to_string()),
        }),
        ..Report::new("hash-fixed", format!("'{old}' is now '{new}'"))
    }
}

/// A failed build as a record; `reason` is `status N: MSG` or a plain
/// message, `pos` where the derivation was defined.
fn build_report(pos: Option<String>, reason: &str, exit: i32) -> Report {
    let msg = match reason.split_once(": ") {
        Some((s, m)) if s.starts_with("status ") => m,
        _ => reason,
    };
    let raw = strip_ansi(msg);
    let (code, hint) = match mismatch(&raw) {
        Some((_, got)) if exit == 102 => (
            "hash-mismatch",
            Some(format!("set the expected hash to '{got}'")),
        ),
        _ => ("build-failed", None),
    };
    Report {
        pos,
        hint,
        ..Report::new(code, raw.trim_end())
    }
}

/// A failed build: its record under internal-json, else `text`, if any;
/// the renderer prints its own.
pub fn build_failed(pos: Option<String>, reason: &str, exit: i32, text: Option<&str>) {
    let r = build_report(pos, reason, exit);
    let s = cur();
    // the two hashes and what to do about them do not fit the renderer's
    // one-line verdict, and nix prints them
    let mut own = None;
    if r.code == "hash-mismatch"
        && let Some(m) = mismatch(&r.raw)
    {
        push_mismatch(&s.log, m);
        if wanted(&s.log, &r.raw) {
            return;
        }
        own = Some(match &r.hint {
            Some(h) => format!("{}\n       {h}", r.text()),
            None => r.text(),
        });
    }
    s.log.out(&r.record(&r.text()), text.or(own.as_deref()));
}

/// A build result's message as one line: its first line, and the two hashes
/// of a fixed-output mismatch, which say what to fix.
pub fn reason_line(raw: &str) -> String {
    let first = raw.lines().next().unwrap_or_default().trim_end();
    match mismatch(raw) {
        Some((specified, got)) => format!("{first} specified: {specified}, got: {got}"),
        None => first.to_string(),
    }
}

/// `s` without its ECMA-048 escape sequences.
pub fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x1b {
            let start = i;
            while i < b.len() && b[i] != 0x1b {
                i += 1;
            }
            out.push_str(&String::from_utf8_lossy(&b[start..i]));
            continue;
        }
        i += 1;
        match b.get(i) {
            // CSI: parameter bytes, intermediate bytes, one final byte
            Some(b'[') => {
                i += 1;
                while b.get(i).is_some_and(|c| (0x30..=0x3f).contains(c)) {
                    i += 1;
                }
                while b.get(i).is_some_and(|c| (0x20..=0x2f).contains(c)) {
                    i += 1;
                }
                if b.get(i).is_some_and(|c| (0x40..=0x7e).contains(c)) {
                    i += 1;
                }
            }
            // 7-bit C1 Fe other than CSI
            Some(c) if (0x40..=0x5a).contains(c) || (0x5c..=0x5f).contains(c) => i += 1,
            _ => {}
        }
    }
    out
}

/// terminal.cc:filterANSIEscapes, what every line Nix writes to stderr goes
/// through: only a colour sequence survives, and only when the reader is a
/// terminal. Cursor movement, a clear screen and an OSC string never do.
pub fn filter_ansi(s: &str, filter_all: bool) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            0x1b => {
                let start = i;
                i += 1;
                let mut last = 0u8;
                match b.get(i) {
                    Some(b'[') => {
                        i += 1;
                        while b.get(i).is_some_and(|c| (0x30..=0x3f).contains(c)) {
                            i += 1;
                        }
                        while b.get(i).is_some_and(|c| (0x20..=0x2f).contains(c)) {
                            i += 1;
                        }
                        if let Some(c) = b.get(i).filter(|c| (0x40..=0x7e).contains(*c)) {
                            last = *c;
                            i += 1;
                        }
                    }
                    Some(b']') => {
                        i += 1;
                        while b.get(i).is_some_and(|c| *c != 0x1b && *c != 0x07) {
                            i += 1;
                        }
                        if let Some(v) = b.get(i).copied() {
                            i += 1;
                            if v == 0x1b && b.get(i) == Some(&b'\\') {
                                i += 1;
                            }
                        }
                    }
                    Some(c) if (0x40..=0x5f).contains(c) => i += 1,
                    _ => {}
                }
                if !filter_all && last == b'm' {
                    out.push_str(&String::from_utf8_lossy(&b[start..i]));
                }
            }
            b'\t' => {
                i += 1;
                loop {
                    col += 1;
                    out.push(' ');
                    if col.is_multiple_of(8) {
                        break;
                    }
                }
            }
            b'\r' | 0x07 => i += 1,
            _ => {
                let start = i;
                while i < b.len() && !matches!(b[i], 0x1b | b'\t' | b'\r' | 0x07) {
                    i += 1;
                }
                let chunk = String::from_utf8_lossy(&b[start..i]);
                col += chunk.chars().count();
                out.push_str(&chunk);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_stripping() {
        assert_eq!(strip_ansi("\x1b[1;32mgreen\x1b[0m"), "green");
        assert_eq!(strip_ansi("plain"), "plain");
        // `]` is one of the 7-bit C1 Fe bytes, so it goes with the ESC
        assert_eq!(strip_ansi("a\x1b]0;titl"), "a0;titl");
    }

    #[test]
    fn record_is_nix_log_ei_plus_code_and_hint() {
        let r = Report {
            pos: Some("/a/b.nix:3:7".into()),
            trace: vec![("while calling 'f'".into(), Some("«string»:1:9".into()))],
            hint: Some("did you mean 'abc'?".into()),
            ..Report::new("missing-attr", "attribute 'abd' missing")
        };
        let text = r.text();
        assert_eq!(
            text,
            "error: attribute 'abd' missing\n       at /a/b.nix:3:7\n       … while calling 'f' at «string»:1:9"
        );
        let want = r#"{"action":"msg","code":"missing-attr","column":7,"file":"/a/b.nix:3:7","hint":"did you mean 'abc'?","level":0,"line":3,"msg":"error: attribute 'abd' missing\n       at /a/b.nix:3:7\n       … while calling 'f' at «string»:1:9","raw_msg":"attribute 'abd' missing"}"#;
        assert_eq!(r.record(&text).to_string(), want);
        let s = Session::new();
        let _entered = s.enter();
        s.log.show_trace.store(true, Ordering::Relaxed);
        let v = r.record(&text);
        let t = r#"[{"column":9,"file":"«string»:1:9","line":1,"raw_msg":"while calling 'f'"}]"#;
        assert_eq!(v["trace"].to_string(), t);
    }

    #[test]
    fn a_session_id_is_a_uuid_v7() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(0x019e_037b_f603);
        let (a, b) = (uuid_v7(t), uuid_v7(t));
        assert!(a.starts_with("019e037b-f603-7"), "{a}");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"), "{a}");
        assert_eq!(
            a.split('-').map(str::len).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert_ne!(a, b);
        let s = Session::new();
        let _entered = s.enter();
        let v: Value = serde_json::from_str(&s.log.wire(&json!({"action": "msg"}))).unwrap();
        assert_eq!(v["sid"], session_id());
    }

    #[test]
    fn record_without_position() {
        let r = Report::new("usage", "unknown flag '--x'");
        let want = r#"{"action":"msg","code":"usage","column":null,"file":null,"level":0,"line":null,"msg":"m","raw_msg":"unknown flag '--x'"}"#;
        assert_eq!(r.record("m").to_string(), want);
    }

    #[test]
    fn lint_warning_is_a_level_1_record() {
        let s = Session::new();
        let _entered = s.enter();
        let text = "warning: [url-literal] URL literal\n  at /a.nix:2:5";
        let want = r#"{"action":"msg","code":"lint","column":5,"file":"/a.nix:2:5","level":1,"line":2,"msg":"warning: [url-literal] URL literal\n  at /a.nix:2:5","raw_msg":"[url-literal] URL literal"}"#;
        assert_eq!(lint(text, None), (text.to_string(), want.to_string()));
        let site = Site {
            start: 9,
            end: 19,
            replacement: Some("\"http://a.b\"".into()),
        };
        let (_, record) = lint(text, Some(site));
        let tail =
            r#""raw_msg":"[url-literal] URL literal","replacement":"\"http://a.b\"","start":9}"#;
        assert!(
            record.contains(r#""end":19,"#) && record.ends_with(tail),
            "{record}"
        );
        // a cache entry gives the text back, or the records under internal-json
        let kept = format!("{want}\n{record}\n");
        let twice = format!("{text}\n{text}\n");
        assert_eq!(replay_lints(kept.as_bytes()), twice.as_bytes());
        s.log.json.store(true, Ordering::Relaxed);
        let replayed = String::from_utf8(replay_lints(kept.as_bytes())).unwrap();
        assert!(replayed.starts_with("@nix {") && replayed.contains(r#""start":9"#));
        assert_eq!(replayed.lines().count(), 2);
    }

    #[test]
    fn a_fixed_hash_has_the_shape_of_a_finding_with_an_edit() {
        let r = fixed_report("/a.nix:2:11", 12, "sha256-A=", "sha256-BB=");
        let want = r#"{"action":"msg","code":"hash-fixed","column":11,"end":21,"file":"/a.nix:2:11","level":3,"line":2,"msg":"m","raw_msg":"'sha256-A=' is now 'sha256-BB='","replacement":"sha256-BB=","start":12}"#;
        assert_eq!(r.record("m").to_string(), want);
    }

    #[test]
    fn activity_records_are_nix_json_logger_records() {
        let f = vec!["/nix/store/x.drv".into(), "".into(), 1.into(), 1.into()];
        let want = r#"{"action":"start","fields":["/nix/store/x.drv","",1,1],"id":7,"level":3,"parent":0,"text":"building '/nix/store/x.drv'","type":105}"#;
        let v = start_record(7, 3, ACT_BUILD, "building '/nix/store/x.drv'", f, 0);
        assert_eq!(v.to_string(), want);
        let want = r#"{"action":"start","id":8,"level":0,"parent":7,"text":"","type":102}"#;
        assert_eq!(
            start_record(8, 0, ACT_REALISE, "", vec![], 7).to_string(),
            want
        );
        let p = vec![1.into(), 2.into(), 1.into(), 0.into()];
        let want = r#"{"action":"result","fields":[1,2,1,0],"id":8,"type":105}"#;
        assert_eq!(result_record(8, RES_PROGRESS, p).to_string(), want);
        assert_eq!(activity_id() >> 32, u64::from(std::process::id()));
        assert_ne!(activity_id(), activity_id());
    }

    #[test]
    fn exit_status_flags_combine_like_nix() {
        assert_eq!(build_exit(3, ""), 100);
        assert_eq!(build_exit(8, ""), 101);
        assert_eq!(
            build_exit(5, "hash mismatch in fixed-output derivation 'x':"),
            102
        );
        assert_eq!(build_exit(5, "output rejected"), 1);
        assert_eq!(build_exit(12, ""), 104);
        assert_eq!(build_exit(10, "Build failed due to failed dependency"), 100);
        assert_eq!(combine_exit(100, 101), 101);
        assert_eq!(combine_exit(100, 104), 108);
        assert_eq!(combine_exit(1, 102), 102);
        assert_eq!(combine_exit(0, 1), 1);
        assert_eq!(combine_exit(0, 0), 0);
    }

    #[test]
    fn a_resolved_copy_reports_its_mismatch_as_a_line() {
        let s = Session::new();
        let _entered = s.enter();
        let a = "sha256-AoAoAoAoAo=";
        let line = format!(
            "error: hash mismatch in fixed-output derivation '/nix/store/r-x.drv':\n         specified: {a}\n            got:    sha256-BoBoBoBoBo="
        );
        note_mismatch(&line);
        note_mismatch(&line);
        assert_eq!(resolve_mismatches(&|s| s == a), 1);
        assert_eq!(
            take_mismatches(),
            vec![(a.to_string(), "sha256-BoBoBoBoBo=".to_string())]
        );
        note_mismatch("error: build of resolved derivation '/nix/store/r-x.drv' failed");
        assert_eq!(resolve_mismatches(&|_| true), 0);
        assert_eq!(take_mismatches(), Vec::new());
    }

    /// A build log, a `builtins.trace` or any other text can print the same
    /// words: a mismatch that names no planned fixed-output derivation is
    /// dropped, so `fix-hashes` never writes it into a file of the user.
    #[test]
    fn a_forged_mismatch_is_dropped() {
        let s = Session::new();
        let _entered = s.enter();
        let line = format!(
            "{MISMATCH} 'x':\n  specified: sha256-AoAoAoAoAo=\n     got:    sha256-BoBoBoBoBo="
        );
        note_mismatch(&line);
        assert_eq!(resolve_mismatches(&|_| false), 0);
        assert_eq!(take_mismatches(), Vec::new());
        // the same text through `format`, as a build log line reaches stderr
        let _ = format(&line);
        assert_eq!(resolve_mismatches(&|_| true), 0);
        assert_eq!(take_mismatches(), Vec::new());
    }

    #[test]
    fn escapes_are_filtered_the_way_nix_filters_them() {
        let coloured = "\x1b[31;1merror:\x1b[0m boom";
        assert_eq!(filter_ansi(coloured, true), "error: boom");
        assert_eq!(filter_ansi(coloured, false), coloured);
        // cursor movement, clear screen and OSC never survive
        assert_eq!(filter_ansi("a\x1b[2Jb\x1b[3Dc", false), "abc");
        assert_eq!(filter_ansi("a\x1b]0;title\x07b", false), "ab");
        assert_eq!(filter_ansi("a\x1b]8;;http://x\x1b\\b", false), "ab");
        assert_eq!(filter_ansi("a\rb\x07", false), "ab");
        assert_eq!(filter_ansi("a\tb", false), "a       b");
    }

    #[test]
    fn a_coloured_error_is_an_error_record() {
        let s = Session::new();
        let _entered = s.enter();
        s.log.json.store(true, Ordering::Relaxed);
        let out = format("\x1b[31;1merror:\x1b[0m boom");
        assert!(out.contains(r#""level":0"#), "{out}");
    }

    #[test]
    fn a_mismatch_reason_keeps_both_hashes() {
        let raw = "status 5: hash mismatch in fixed-output derivation '/nix/store/x.drv':\n  specified: sha256-AAAA=\n     got:    sha256-BBBB=";
        assert_eq!(
            reason_line(raw),
            "status 5: hash mismatch in fixed-output derivation '/nix/store/x.drv': specified: sha256-AAAA=, got: sha256-BBBB="
        );
        assert_eq!(
            reason_line("status 3: Cannot build 'x'.\nmore"),
            "status 3: Cannot build 'x'."
        );
    }

    #[test]
    fn a_hash_mismatch_names_the_hash_it_got() {
        let reason = "status 5: hash mismatch in fixed-output derivation '/nix/store/x.drv':\n  specified: sha256-AAAA=\n     got:    sha256-BBBB=";
        let r = build_report(Some("/a.nix:4:3".into()), reason, 102);
        assert_eq!(r.code, "hash-mismatch");
        assert_eq!(
            r.hint.as_deref(),
            Some("set the expected hash to 'sha256-BBBB='")
        );
        assert!(
            r.raw
                .starts_with("hash mismatch in fixed-output derivation")
        );
        assert_eq!(
            build_report(None, "status 3: Cannot build", 100).code,
            "build-failed"
        );
    }
}
