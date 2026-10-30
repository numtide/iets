//! The Nix worker protocol (libstore/worker-protocol.hh), spoken to a
//! local `nix-daemon` socket or to `nix-daemon --stdio` over ssh. One serial connection; every
//! op is one request and one `STDERR_*` reply loop.

pub mod wire;

use crate::sys::fs::symlink;
use crate::sys::net::UnixStream;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::{env, fs, mem, str, thread};

use serde_json::Value;

use super::super::store::path;
use super::event::{Event, Failed, Log, Phase, Started, Stopped};
use super::grpc::GrpcConfig;
use super::grpc::drvwire::PathInfo;
use crate::config::{self, Config};
use crate::error::{self, Error, Result};
use crate::log::{
    self, ACT_BUILD, ACT_BUILDS, LVL_INFO, RES_BUILD_LOG_LINE, RES_PROGRESS, RES_SET_PHASE,
};
use crate::store::path::StorePath;
use wire::{Counting, FramedSink};

/// What `SetOptions` sends (remote-store.cc:setOptions). `Default` is the
/// built-in set; `TryFrom<&Config>` takes the values from the user's nix.conf
/// instead, and fails the way Nix does on a bad setting value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// `0` would forbid local builds, so the default is the CPU count, what
    /// `max-jobs = auto` gives.
    pub max_jobs: u64,
    /// `0` means all cores.
    pub cores: u64,
    pub keep_going: bool,
    pub keep_failed: bool,
    pub substitutes: bool,
    /// `fallback`: build what a substituter fails to give.
    pub try_fallback: bool,
    /// `max-silent-time` in seconds; `0` is no limit.
    pub max_silent_time: u64,
    /// The override map: `Config::forwarded`.
    pub overrides: Vec<(String, String)>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            max_jobs: thread::available_parallelism().map_or(1, |n| n.get() as u64),
            cores: 0,
            keep_going: false,
            keep_failed: false,
            substitutes: true,
            try_fallback: false,
            max_silent_time: 0,
            overrides: Vec::new(),
        }
    }
}

impl TryFrom<&Config> for Options {
    type Error = Error;

    fn try_from(c: &Config) -> Result<Options> {
        Ok(Options {
            max_jobs: c.max_jobs()?,
            cores: c.cores()?,
            keep_going: c.keep_going()?,
            keep_failed: c.keep_failed()?,
            substitutes: c.bool("substitute")?.unwrap_or(true),
            try_fallback: c.bool("fallback")?.unwrap_or(false),
            max_silent_time: c.int("max-silent-time")?.unwrap_or(0),
            overrides: c.forwarded(),
        })
    }
}

const WORKER_MAGIC_1: u64 = 0x6e69_7863;
const WORKER_MAGIC_2: u64 = 0x6478_696f;

const STDERR_NEXT: u64 = 0x6f6c_6d67;
const STDERR_READ: u64 = 0x6461_7461;
const STDERR_WRITE: u64 = 0x6461_7416;
const STDERR_LAST: u64 = 0x616c_7473;
const STDERR_ERROR: u64 = 0x6378_7470;
const STDERR_START_ACTIVITY: u64 = 0x5354_5254;
const STDERR_STOP_ACTIVITY: u64 = 0x5354_4f50;
const STDERR_RESULT: u64 = 0x5253_4c54;

/// The newest protocol this client speaks (worker-protocol.cc:WorkerProto::latest).
pub const LATEST_MINOR: u64 = 38;
/// Below this the handshake and `BuildPathsWithResults` differ; not worth supporting.
pub const MIN_MINOR: u64 = 35;

/// worker-protocol.hh:WorkerProto::Op, the subset used here.
#[derive(Clone, Copy)]
#[repr(u64)]
enum Op {
    SetOptions = 19,
    QueryPathInfo = 26,
    QueryValidPaths = 31,
    AddIndirectRoot = 12,
    QuerySubstitutablePathInfos = 30,
    NarFromPath = 38,
    QueryMissing = 40,
    QueryDerivationOutputMap = 41,
    AddMultipleToStore = 44,
    BuildPathsWithResults = 46,
}

/// logging.hh:resPostBuildLogLine.
const RES_POST_BUILD_LOG_LINE: u64 = 107;

/// error.hh:Verbosity.
const LVL_VOMIT: u64 = 7;

const DEFAULT_SOCKET: &str = "/nix/var/nix/daemon-socket/socket";

/// The local daemon's socket, as uds-remote-store.cc:getDaemonSocketPath
/// picks it.
pub fn default_socket() -> PathBuf {
    let var = |n| env::var_os(n).filter(|v| !v.is_empty());
    if let Some(p) = var("NIX_DAEMON_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    match var("NIX_STATE_DIR") {
        Some(d) => Path::new(&d).join("daemon-socket/socket"),
        None => PathBuf::from(DEFAULT_SOCKET),
    }
}

/// Where the `--builder` or `--store` URL points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    Unix(PathBuf),
    /// nix-grpc-store, which does not speak the worker protocol; see
    /// `crate::remote::grpc`.
    Grpc(Box<GrpcConfig>),
    Ssh {
        user: Option<String>,
        host: String,
        port: Option<u16>,
        /// The command run on the host: the URL's `remote-program` query
        /// parameter, `nix-daemon` by default, and then always `--stdio`
        /// (ssh-store.cc:openConnection).
        remote_program: Vec<String>,
        /// `ssh-key`: `-i`.
        key: Option<String>,
        /// `compress`: `-C`.
        compress: bool,
    },
}

impl Transport {
    /// `daemon`, `unix://[/path]`, `ssh-ng://[user@]host[?remote-program=CMD]`,
    /// `grpc://host:port[?…]`. `None` for every other scheme. A malformed
    /// `grpc://` URL is `None` too; `parse_checked` reports why.
    pub fn parse(url: &str) -> Option<Transport> {
        Transport::parse_checked(url, config::DEFAULT_SYSTEM).ok()?
    }

    /// `parse` with the `grpc://` parameter errors kept, and the `system=`
    /// default the caller's `system` setting implies.
    pub fn parse_checked(url: &str, default_system: &str) -> error::Result<Option<Transport>> {
        if let Some(cfg) = GrpcConfig::parse(url, default_system) {
            return Ok(Some(Transport::Grpc(Box::new(cfg?))));
        }
        Transport::parse_worker(url)
    }

    /// The store as Nix's getHumanReadableURI names it, for the log.
    pub fn uri(&self) -> String {
        match self {
            Transport::Unix(p) if *p == default_socket() => "daemon".into(),
            Transport::Unix(p) => format!("unix://{}", p.display()),
            Transport::Grpc(cfg) => cfg.url(),
            Transport::Ssh {
                user: Some(u),
                host,
                ..
            } => format!("ssh-ng://{u}@{host}"),
            Transport::Ssh { host, .. } => format!("ssh-ng://{host}"),
        }
    }

    fn parse_worker(url: &str) -> Result<Option<Transport>> {
        // `auto` is nix.conf's default `store`, which iets answers with the
        // daemon: it has no local store of its own (docs/reference/settings.md).
        if url == "daemon" || url == "unix://" || url == "auto" || url.is_empty() {
            return Ok(Some(Transport::Unix(default_socket())));
        }
        if let Some(path) = url.strip_prefix("unix://") {
            return Ok(Some(Transport::Unix(PathBuf::from(path))));
        }
        let Some(rest) = url.strip_prefix("ssh-ng://") else {
            return Ok(None);
        };
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (user, host, port) = authority(rest.trim_end_matches('/'))?;
        let mut program = vec!["nix-daemon".to_string()];
        let mut key = None;
        let mut compress = false;
        for kv in query.split('&') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            let v = percent_decode(v);
            match k {
                "remote-program" => {
                    program = v.split_whitespace().map(str::to_string).collect();
                }
                "ssh-key" => key = Some(v),
                "compress" => compress = v != "false" && v != "0",
                _ => {}
            }
        }
        // ssh-store.cc:openConnection appends it to whatever remote-program
        // names, so a remote program can never drop the protocol.
        program.push("--stdio".to_string());
        Ok(Some(Transport::Ssh {
            user,
            host,
            port,
            remote_program: program,
            key,
            compress,
        }))
    }
}

/// `[user@]host[:port]`, with an IPv6 host in brackets. ssh.cc:checkValidAuthority
/// refuses a name that ssh would read as an option.
fn authority(rest: &str) -> Result<(Option<String>, String, Option<u16>)> {
    let bad = |what: &str| {
        Err(Error::Usage(format!(
            "invalid SSH authority: '{rest}': {what}"
        )))
    };
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()), h),
        None => (None, rest),
    };
    let (host, port) = match hostport.strip_prefix('[') {
        Some(r) => match r.split_once(']') {
            Some((h, tail)) => (h, tail.strip_prefix(':')),
            None => return bad("unterminated '['"),
        },
        None => match hostport.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        },
    };
    let port = match port {
        Some(p) => match p.parse::<u16>() {
            Ok(p) => Some(p),
            Err(_) => return bad(&format!("'{p}' is not a port")),
        },
        None => None,
    };
    if let Some(u) = &user {
        if u.is_empty() {
            return bad("user name must not be empty");
        }
        if u.starts_with('-') {
            return bad(&format!("user name '{u}' must not start with '-'"));
        }
    }
    if host.is_empty() {
        return bad("host name must not be empty");
    }
    if host.starts_with('-') {
        return bad(&format!("host name '{host}' must not start with '-'"));
    }
    Ok((user, host.to_string(), port))
}

/// Everything before the remote command, in the order of ssh.cc:startCommand
/// and addCommonSSHOpts. The host comes first, so no option of `NIX_SSHOPTS`
/// can turn it into one; the caller adds `--` before the command.
fn ssh_args(t: &Transport) -> Result<Vec<String>> {
    let Transport::Ssh {
        user,
        host,
        port,
        key,
        compress,
        ..
    } = t
    else {
        return Err(Error::Usage("not an ssh store".into()));
    };
    let mut args = match user {
        Some(u) => vec![format!("{u}@{host}")],
        None => vec![host.clone()],
    };
    args.push("-x".to_string());
    // the same knob Nix's ssh.cc reads, so ControlMaster and friends can be
    // set per run
    if let Ok(opts) = env::var("NIX_SSHOPTS") {
        args.extend(shell_split(&opts)?);
    }
    if let Some(k) = key {
        args.push("-i".to_string());
        args.push(k.clone());
    }
    if *compress {
        args.push("-C".to_string());
    }
    if let Some(p) = port {
        args.push(format!("-p{p}"));
    }
    Ok(args)
}

/// strings.cc:shellSplitString, what `NIX_SSHOPTS` is split with.
pub fn shell_split(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut escaping = false;
    for c in s.chars() {
        match quote {
            Some('\'') => {
                started = true;
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some(_) => {
                started = true;
                if escaping {
                    if !matches!(c, '$' | '`' | '"' | '\\') {
                        cur.push('\\');
                    }
                    cur.push(c);
                    escaping = false;
                } else if c == '\\' {
                    escaping = true;
                } else if c == '"' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None if escaping => {
                cur.push(c);
                started = true;
                escaping = false;
            }
            None => match c {
                '\\' => escaping = true,
                ' ' | '\t' => {
                    if started {
                        out.push(mem::take(&mut cur));
                        started = false;
                    }
                }
                '"' | '\'' => {
                    quote = Some(c);
                    started = true;
                }
                _ => {
                    cur.push(c);
                    started = true;
                }
            },
        }
    }
    match quote {
        Some('\'') => return Err(Error::Usage("unterminated single quote".into())),
        Some(_) => return Err(Error::Usage("unterminated double quote".into())),
        None => {}
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

/// `%XX` and `+` in a URL query value.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = if b[i] == b'%' && i + 2 < b.len() {
            str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        } else {
            None
        };
        match hex {
            Some(c) => {
                out.push(c);
                i += 3;
            }
            None => {
                out.push(if b[i] == b'+' { b' ' } else { b[i] });
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every failure of the connection's own reads and writes reports the same way.
fn conn(e: io::Error) -> Error {
    Error::io("daemon connection", e)
}

fn write_u64(w: &mut dyn Write, n: u64) -> Result<()> {
    wire::write_u64(w, n).map_err(conn)
}

fn write_str(w: &mut dyn Write, s: &[u8]) -> Result<()> {
    wire::write_str(w, s).map_err(conn)
}

fn write_strs<S: AsRef<[u8]>>(w: &mut dyn Write, items: &[S]) -> Result<()> {
    wire::write_strs(w, items).map_err(conn)
}

fn read_u64(r: &mut dyn Read) -> Result<u64> {
    wire::read_u64(r).map_err(conn)
}

fn read_bytes(r: &mut dyn Read, max: u64) -> Result<Vec<u8>> {
    wire::read_bytes(r, max).map_err(conn)
}

fn read_str(r: &mut dyn Read) -> Result<String> {
    wire::read_str(r).map_err(conn)
}

fn read_paths(r: &mut dyn Read) -> Result<Vec<String>> {
    wire::read_paths(r).map_err(conn)
}

fn read_strs(r: &mut dyn Read) -> Result<Vec<String>> {
    wire::read_strs(r).map_err(conn)
}

/// Round trips and bytes on one connection, for `--timings`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    /// Worker ops sent, `SetOptions` included.
    pub ops: usize,
    /// Ops plus the handshake exchanges that each wait for the daemon.
    pub round_trips: usize,
    pub bytes_sent: u64,
}

/// What building some paths takes (remote-store.cc:queryMissing).
#[derive(Debug, Default, Clone)]
pub struct Missing {
    pub will_build: Vec<String>,
    pub will_substitute: Vec<String>,
    pub unknown: Vec<String>,
    pub download_size: u64,
    pub nar_size: u64,
}

/// worker-protocol.cc:BuildResult, the fields this client reads.
#[derive(Debug, Clone)]
pub struct BuildResult {
    pub path: String,
    pub status: u64,
    pub msg: String,
}

impl BuildResult {
    /// common-protocol.cc:buildResultStatusTable: Built, Substituted, AlreadyValid,
    /// ResolvesToAlreadyValid.
    pub fn success(&self) -> bool {
        matches!(self.status, 0 | 1 | 2 | 13)
    }
}

pub struct Connection {
    to: BufWriter<Counting<Box<dyn Write + Send>>>,
    from: BufReader<Box<dyn Read + Send>>,
    child: Option<Child>,
    pub minor: u64,
    pub daemon_nix_version: String,
    /// `Some(true)` when the daemon trusts us (protocol >= 1.35).
    pub trusted: Option<bool>,
    pub stats: Stats,
    /// Running builds by activity id, to attribute log lines.
    activities: HashMap<u64, String>,
    /// Follows a nested activity back to the build it belongs to.
    parents: HashMap<u64, u64>,
    /// The daemon's activity ids to the ones this process logs them under.
    ids: HashMap<u64, u64>,
    builds: BuildCounts,
}

/// The daemon bumps `done` or `failed` on its builds activity just before a
/// build's own activity stops, and its worker finishes one goal at a time.
/// So the oldest unclaimed change belongs to the next build that stops.
#[derive(Default)]
struct BuildCounts {
    act: Option<u64>,
    done: u64,
    failed: u64,
    /// One entry per change: true for a success.
    pending: VecDeque<bool>,
}

impl BuildCounts {
    fn progress(&mut self, fields: &[Value]) {
        let n = |i: usize| fields.get(i).and_then(Value::as_u64).unwrap_or(0);
        let (done, failed) = (n(0), n(3));
        self.pending.extend((self.done..done).map(|_| true));
        self.pending.extend((self.failed..failed).map(|_| false));
        (self.done, self.failed) = (done, failed);
    }
}

#[cfg(test)]
mod counts_tests {
    use super::*;
    use serde_json::json;

    fn p(f: [u64; 4]) -> Vec<Value> {
        f.iter().map(|n| json!(n)).collect()
    }

    /// The order the daemon sent for two builds, one failing then one built.
    #[test]
    fn each_change_is_a_verdict_in_order() {
        let mut c = BuildCounts::default();
        c.progress(&p([0, 3, 2, 0]));
        assert!(c.pending.is_empty());
        c.progress(&p([0, 3, 2, 1]));
        c.progress(&p([1, 3, 1, 1]));
        assert_eq!(c.pending, [false, true]);
    }

    #[test]
    fn a_repeated_update_claims_nothing() {
        let mut c = BuildCounts::default();
        c.progress(&p([1, 3, 1, 0]));
        c.progress(&p([1, 3, 1, 0]));
        assert_eq!(c.pending, [true]);
    }
}

impl Drop for Connection {
    /// A local ssh child exits on EOF of its stdin, which lives inside `to`; swap it out before
    /// waiting, or the wait never returns. nixbuild.net keeps the session open for minutes
    /// after EOF, so kill the child too, as Nix's `Pid::~Pid` does.
    fn drop(&mut self) {
        // a daemon that went away mid-op never stopped these
        for (_, id) in self.ids.drain() {
            log::stop(id);
        }
        let _ = self.to.flush();
        if let Some(mut c) = self.child.take() {
            let sink: Box<dyn Write + Send> = Box::new(io::sink());
            drop(mem::replace(
                &mut self.to,
                BufWriter::new(Counting {
                    inner: sink,
                    bytes: 0,
                }),
            ));
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Connection {
    pub fn open(transport: &Transport, on_event: &mut dyn FnMut(Event)) -> Result<Connection> {
        Connection::open_with(transport, &Options::default(), on_event)
    }

    /// Opens the transport and runs the handshake and `SetOptions`, as
    /// remote-store.cc:initConnection does.
    pub fn open_with(
        transport: &Transport,
        options: &Options,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Connection> {
        let (to, from, child): (Box<dyn Write + Send>, Box<dyn Read + Send>, Option<Child>) =
            match transport {
                Transport::Grpc(cfg) => {
                    return Err(Error::Usage(format!(
                        "'{}' is a gRPC store, not a worker-protocol daemon",
                        cfg.url()
                    )));
                }
                Transport::Unix(path) => {
                    let s = UnixStream::connect(path).map_err(|e| {
                        Error::io(format!("cannot connect to '{}'", path.display()), e)
                    })?;
                    let r = s.try_clone().map_err(conn)?;
                    (Box::new(s), Box::new(r), None)
                }
                ssh @ Transport::Ssh { remote_program, .. } => {
                    let mut cmd = Command::new("ssh");
                    cmd.args(ssh_args(ssh)?)
                        .arg("--")
                        .args(remote_program)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::inherit());
                    let mut c = cmd.spawn().map_err(|e| Error::io("cannot run ssh", e))?;
                    let stdin = c.stdin.take().expect("piped");
                    let stdout = c.stdout.take().expect("piped");
                    (Box::new(stdin), Box::new(stdout), Some(c))
                }
            };
        let mut conn = Connection {
            to: BufWriter::with_capacity(
                1 << 20,
                Counting {
                    inner: to,
                    bytes: 0,
                },
            ),
            from: BufReader::with_capacity(1 << 20, from),
            child,
            minor: 0,
            daemon_nix_version: String::new(),
            trusted: None,
            stats: Stats::default(),
            activities: HashMap::new(),
            parents: HashMap::new(),
            ids: HashMap::new(),
            builds: BuildCounts::default(),
        };
        conn.handshake()?;
        conn.process_stderr(on_event)?;
        // ssh-store.cc:setOptions sends nothing, and nixbuild.net rejects the op.
        if !matches!(transport, Transport::Ssh { .. }) {
            conn.set_options(options, on_event)?;
        }
        Ok(conn)
    }

    pub fn bytes_sent(&self) -> u64 {
        self.to.get_ref().bytes
    }

    fn flush(&mut self) -> Result<()> {
        self.to.flush().map_err(conn)
    }

    fn sync_stats(&mut self) {
        self.stats.bytes_sent = self.bytes_sent();
    }

    fn w(&mut self) -> &mut dyn Write {
        &mut self.to
    }

    fn r(&mut self) -> &mut dyn Read {
        &mut self.from
    }

    /// worker-protocol-connection.cc:BasicClientConnection::handshake and postHandshake.
    fn handshake(&mut self) -> Result<()> {
        write_u64(self.w(), WORKER_MAGIC_1)?;
        write_u64(self.w(), (1 << 8) | LATEST_MINOR)?;
        self.flush()?;
        if read_u64(self.r())? != WORKER_MAGIC_2 {
            return Err(Error::Protocol("nix-daemon protocol mismatch".into()));
        }
        let daemon = read_u64(self.r())?;
        if (daemon & 0xff00) >> 8 != 1 {
            return Err(Error::Protocol(
                "Nix daemon protocol version not supported".into(),
            ));
        }
        self.minor = (daemon & 0xff).min(LATEST_MINOR);
        self.stats.round_trips += 1;
        if self.minor < MIN_MINOR {
            return Err(Error::Protocol(format!(
                "the Nix daemon speaks protocol 1.{}; this client needs 1.{MIN_MINOR} or newer",
                self.minor
            )));
        }
        if self.minor >= 38 {
            write_strs::<&str>(self.w(), &[])?;
            self.flush()?;
            read_strs(self.r())?;
            self.stats.round_trips += 1;
        }
        write_u64(self.w(), 0)?;
        write_u64(self.w(), 0)?;
        self.flush()?;
        self.daemon_nix_version = read_str(self.r())?;
        // worker-protocol.cc:Serialise<std::optional<TrustedFlag>>
        self.trusted = match read_u64(self.r())? {
            0 => None,
            1 => Some(true),
            2 => Some(false),
            n => {
                return Err(Error::Protocol(format!(
                    "invalid trusted status {n} from the Nix daemon"
                )));
            }
        };
        self.stats.round_trips += 1;
        Ok(())
    }

    fn op(&mut self, op: Op) -> Result<()> {
        self.stats.ops += 1;
        self.stats.round_trips += 1;
        write_u64(self.w(), op as u64)
    }

    /// remote-store.cc:setOptions: the same twelve fields, then the override map.
    fn set_options(&mut self, o: &Options, on_event: &mut dyn FnMut(Event)) -> Result<()> {
        self.op(Op::SetOptions)?;
        for v in [
            o.keep_failed.into(),  // keep-failed
            o.keep_going.into(),   // keep-going
            o.try_fallback.into(), // try-fallback
            LVL_INFO,              // verbosity
            o.max_jobs,            // max-jobs
            o.max_silent_time,     // max-silent-time
            1,                     // obsolete use-build-hook
            LVL_VOMIT,             // build verbosity: lvlVomit means verboseBuild = false
            0,                     // obsolete log type
            0,                     // obsolete print build trace
            o.cores,               // cores
            o.substitutes.into(),  // substitute
        ] {
            write_u64(self.w(), v)?;
        }
        write_u64(self.w(), o.overrides.len() as u64)?;
        for (name, value) in &o.overrides {
            write_str(self.w(), name.as_bytes())?;
            write_str(self.w(), value.as_bytes())?;
        }
        self.process_stderr(on_event)
    }

    /// worker-protocol-connection.cc:processStderrReturn: drains the daemon's log stream until
    /// `STDERR_LAST` (the reply follows) or `STDERR_ERROR`.
    fn process_stderr(&mut self, on_event: &mut dyn FnMut(Event)) -> Result<()> {
        self.flush()?;
        loop {
            match read_u64(self.r())? {
                STDERR_LAST => {
                    self.sync_stats();
                    return Ok(());
                }
                STDERR_NEXT => {
                    let mut line = read_bytes(self.r(), wire::MAX_STRING)?;
                    while line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    on_event(Event::Log(Log {
                        drv_path: String::new(),
                        line,
                    }));
                }
                STDERR_START_ACTIVITY => {
                    let act = read_u64(self.r())?;
                    let lvl = read_u64(self.r())?;
                    let ty = read_u64(self.r())?;
                    let s = read_str(self.r())?;
                    let fields = self.read_fields()?;
                    let parent = read_u64(self.r())?;
                    // worker-protocol-connection.cc:processStderr hands these to
                    // the client's logger; fresh ids keep two connections apart
                    if log::activities() {
                        let id = log::activity_id();
                        self.ids.insert(act, id);
                        let up = self.ids.get(&parent).copied().unwrap_or(0);
                        log::start(id, lvl, ty, &s, fields.clone(), up);
                    }
                    if ty == ACT_BUILDS {
                        self.builds = BuildCounts {
                            act: Some(act),
                            ..BuildCounts::default()
                        };
                    }
                    if ty == ACT_BUILD
                        && let Some(Value::String(drv)) = fields.first()
                    {
                        // a goal that failed without a build activity left a
                        // change nobody will claim
                        self.builds.pending.clear();
                        self.activities.insert(act, drv.clone());
                        on_event(Event::Started(Started {
                            drv_path: drv.clone(),
                        }));
                    } else if parent != 0 {
                        // A build's log can arrive under a nested activity
                        // (a remote builder, a substitution): remember the
                        // chain so the line still names its derivation.
                        self.parents.insert(act, parent);
                    }
                }
                STDERR_STOP_ACTIVITY => {
                    let act = read_u64(self.r())?;
                    if let Some(id) = self.ids.remove(&act) {
                        log::stop(id);
                    }
                    self.parents.remove(&act);
                    if let Some(drv) = self.activities.remove(&act) {
                        // a build hook runs two activities for one build and
                        // the verdict belongs to the last
                        let last = !self.activities.values().any(|d| *d == drv);
                        let built = last && self.builds.pending.pop_front() == Some(true);
                        on_event(Event::Stopped(Stopped {
                            drv_path: drv,
                            built,
                        }));
                    }
                }
                STDERR_RESULT => {
                    let act = read_u64(self.r())?;
                    let ty = read_u64(self.r())?;
                    let fields = self.read_fields()?;
                    if let Some(&id) = self.ids.get(&act) {
                        log::result(id, ty, fields.clone());
                    }
                    if ty == RES_PROGRESS && self.builds.act == Some(act) {
                        self.builds.progress(&fields);
                    }
                    if matches!(ty, RES_BUILD_LOG_LINE | RES_POST_BUILD_LOG_LINE)
                        && let Some(Value::String(line)) = fields.first()
                    {
                        on_event(Event::Log(Log {
                            drv_path: self.build_of(act).unwrap_or_default(),
                            line: line.clone().into_bytes(),
                        }));
                    }
                    if ty == RES_SET_PHASE
                        && let Some(Value::String(phase)) = fields.first()
                        && let Some(drv) = self.build_of(act)
                    {
                        on_event(Event::Phase(Phase {
                            drv_path: drv,
                            phase: phase.clone(),
                        }));
                    }
                }
                STDERR_ERROR => {
                    let e = self.read_error()?;
                    self.sync_stats();
                    return Err(e);
                }
                STDERR_READ | STDERR_WRITE => {
                    return Err(Error::Protocol(
                        "daemon asked for a data stream this client does not provide".into(),
                    ));
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "got unknown message type {other:#x} from Nix daemon"
                    )));
                }
            }
        }
    }

    /// The derivation whose build `act` belongs to, following the activity's
    /// parents: a remote builder's log arrives under a child activity.
    fn build_of(&self, act: u64) -> Option<String> {
        let mut id = act;
        for _ in 0..16 {
            if let Some(drv) = self.activities.get(&id) {
                return Some(drv.clone());
            }
            id = *self.parents.get(&id)?;
        }
        None
    }

    /// worker-protocol-connection.cc:readFields.
    fn read_fields(&mut self) -> Result<Vec<Value>> {
        let n = read_u64(self.r())?;
        let mut fields = Vec::with_capacity(n.min(16) as usize);
        for _ in 0..n {
            let f = match read_u64(self.r())? {
                0 => read_u64(self.r())?.into(),
                1 => read_str(self.r())?.into(),
                t => {
                    return Err(Error::Protocol(format!(
                        "got unsupported field type {t:#x} from Nix daemon"
                    )));
                }
            };
            fields.push(f);
        }
        Ok(fields)
    }

    /// serialise.cc:readError (protocol >= 1.26).
    fn read_error(&mut self) -> Result<Error> {
        let ty = read_str(self.r())?;
        if ty != "Error" {
            return Err(Error::Protocol(format!("unexpected error type '{ty}'")));
        }
        let _level = read_u64(self.r())?;
        let _name = read_str(self.r())?;
        let mut msg = read_str(self.r())?;
        let _have_pos = read_u64(self.r())?;
        let traces = read_u64(self.r())?;
        for _ in 0..traces {
            let _have_pos = read_u64(self.r())?;
            let hint = read_str(self.r())?;
            msg.push_str("\n       … ");
            msg.push_str(&hint);
        }
        Ok(Error::Protocol(msg))
    }

    /// `QueryValidPaths` with substitution off; returns the valid subset.
    pub fn query_valid_paths(
        &mut self,
        paths: &[String],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Vec<String>> {
        self.valid_paths(paths, false, on_event)
    }

    /// `QueryValidPaths` with the substitute flag set (protocol >= 1.27), which
    /// runs `Store::substitutePaths` on the daemon before it answers
    /// (daemon.cc:performOp): the valid subset is what the substituters gave us.
    /// Nothing is evaluated and no derivation is built.
    pub fn substitute_paths(
        &mut self,
        paths: &[String],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Vec<String>> {
        self.valid_paths(paths, true, on_event)
    }

    fn valid_paths(
        &mut self,
        paths: &[String],
        substitute: bool,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Vec<String>> {
        self.op(Op::QueryValidPaths)?;
        write_strs(self.w(), paths)?;
        write_u64(self.w(), u64::from(substitute))?;
        self.process_stderr(on_event)?;
        read_paths(self.r())
    }

    /// `QueryPathInfo`: the path's `UnkeyedValidPathInfo`, `None` when the store
    /// has not got it (remote-store.cc:queryPathInfoUncached).
    pub fn query_path_info(
        &mut self,
        path: &str,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Option<PathInfo>> {
        self.op(Op::QueryPathInfo)?;
        write_str(self.w(), path.as_bytes())?;
        match self.process_stderr(on_event) {
            // worker-protocol-connection.cc:queryPathInfo: an old daemon says so
            // with an error, and Nix's client still reads it that way
            Err(Error::Protocol(m)) if m.contains("is not valid") => return Ok(None),
            r => r?,
        }
        if !wire::read_bool(self.r()).map_err(conn)? {
            return Ok(None);
        }
        Ok(Some(PathInfo::read(path, self.r())?))
    }

    /// `AddMultipleToStore`: `body` writes the count, then each `ValidPathInfo` followed by its
    /// NAR (store-api.cc:Store::addMultipleToStore(Source &)), into one framed stream.
    /// `dont_check_sigs` asks the daemon to skip the signature check, which it
    /// grants a trusted client only (daemon.cc:performOp).
    pub fn add_multiple_to_store(
        &mut self,
        dont_check_sigs: bool,
        body: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<()> {
        self.op(Op::AddMultipleToStore)?;
        write_u64(self.w(), 0)?;
        write_u64(self.w(), u64::from(dont_check_sigs))?;
        self.flush()?;
        let mut sink = FramedSink::new(&mut self.to);
        body(&mut sink).map_err(conn)?;
        sink.finish().map_err(conn)?;
        self.process_stderr(on_event)
    }

    /// `BuildPathsWithResults` in normal mode; `paths` are legacy derived paths
    /// (`/nix/store/x.drv!out1,out2` or `!*`), the encoding for protocol >= 1.30.
    pub fn build_paths_with_results(
        &mut self,
        paths: &[String],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Vec<BuildResult>> {
        self.op(Op::BuildPathsWithResults)?;
        write_strs(self.w(), paths)?;
        write_u64(self.w(), 0)?;
        self.process_stderr(on_event)?;
        let n = read_u64(self.r())?;
        let mut results = Vec::new();
        for _ in 0..n {
            results.push(self.read_build_result()?);
        }
        Ok(results)
    }

    /// worker-protocol.cc:Serialise<KeyedBuildResult> (protocol >= 1.29), the
    /// checks of Nix's client included.
    fn read_build_result(&mut self) -> Result<BuildResult> {
        let r = self.r();
        let path = wire::derived_path(&read_str(r)?).map_err(conn)?;
        // common-protocol.cc:buildResultStatusTable
        let status = wire::read_num(r, u8::MAX.into()).map_err(conn)?;
        if status > 14 {
            return Err(Error::Protocol(format!(
                "invalid BuildResult status {status} from the Nix daemon"
            )));
        }
        let msg = read_str(r)?;
        wire::read_num(r, u32::MAX.into()).map_err(conn)?; // times built
        wire::read_bool(r).map_err(conn)?; // non-deterministic
        wire::read_time(r).map_err(conn)?; // start
        wire::read_time(r).map_err(conn)?; // stop
        if self.minor >= 37 {
            // CPU time of user and system: an optional microseconds
            for _ in 0..2 {
                match wire::read_num(self.r(), u8::MAX.into()).map_err(conn)? {
                    0 => {}
                    1 => {
                        wire::read_num(self.r(), i64::MAX as u64).map_err(conn)?;
                    }
                    _ => {
                        return Err(Error::Protocol(
                            "invalid optional tag from the Nix daemon".into(),
                        ));
                    }
                }
            }
        }
        let r = self.r();
        let outputs = read_u64(r)?;
        for _ in 0..outputs {
            wire::drv_output(&read_str(r)?).map_err(conn)?;
            wire::realisation(&wire::read_bytes(r, wire::MAX_STRING)?).map_err(conn)?;
        }
        Ok(BuildResult { path, status, msg })
    }

    /// `QueryMissing`; `paths` are derived paths, as for `BuildPathsWithResults`.
    pub fn query_missing(
        &mut self,
        paths: &[String],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Missing> {
        self.op(Op::QueryMissing)?;
        write_strs(self.w(), paths)?;
        self.process_stderr(on_event)?;
        Ok(Missing {
            will_build: read_paths(self.r())?,
            will_substitute: read_paths(self.r())?,
            unknown: read_paths(self.r())?,
            download_size: read_u64(self.r())?,
            nar_size: read_u64(self.r())?,
        })
    }

    /// `QueryDerivationOutputMap`: output name to store path when the daemon knows it.
    pub fn query_derivation_output_map(
        &mut self,
        drv: &str,
        on_event: &mut dyn FnMut(Event),
    ) -> Result<BTreeMap<String, Option<String>>> {
        self.op(Op::QueryDerivationOutputMap)?;
        write_str(self.w(), drv.as_bytes())?;
        self.process_stderr(on_event)?;
        let n = read_u64(self.r())?;
        let mut map = BTreeMap::new();
        for _ in 0..n {
            let name = read_str(self.r())?;
            let path = read_str(self.r())?;
            if path.is_empty() {
                map.insert(name, None);
                continue;
            }
            // the answer names paths this client later hands to the user and
            // to `nix build -o`: a store that says /etc/passwd is broken
            StorePath::parse(&path).map_err(|e| {
                Error::Protocol(format!(
                    "the daemon named an output that is not a store path: {e}"
                ))
            })?;
            map.insert(name, Some(path));
        }
        Ok(map)
    }

    /// `AddIndirectRoot` (uds-remote-store.cc:addIndirectRoot): registers `path`, an
    /// existing symlink into the store that the caller created, as a GC root. The daemon
    /// links `/nix/var/nix/gcroots/auto/<hash>` to it.
    pub fn add_indirect_root(&mut self, path: &str, on_event: &mut dyn FnMut(Event)) -> Result<()> {
        self.op(Op::AddIndirectRoot)?;
        write_str(self.w(), path.as_bytes())?;
        self.process_stderr(on_event)?;
        // uds-remote-store.cc:addIndirectRoot: readInt
        wire::read_num(self.r(), u32::MAX.into()).map_err(conn)?;
        Ok(())
    }

    /// `QuerySubstitutablePathInfos` (the protocol >= 1.22 shape, no content
    /// addresses): the subset of `paths` some substituter has.
    pub fn query_substitutable_path_infos(
        &mut self,
        paths: &[String],
        on_event: &mut dyn FnMut(Event),
    ) -> Result<Vec<String>> {
        self.op(Op::QuerySubstitutablePathInfos)?;
        write_u64(self.w(), paths.len() as u64)?;
        for p in paths {
            write_str(self.w(), p.as_bytes())?;
            write_str(self.w(), b"")?;
        }
        self.process_stderr(on_event)?;
        let n = read_u64(self.r())?;
        let mut out = Vec::with_capacity(n.min(1 << 16) as usize);
        for _ in 0..n {
            let path = wire::read_path(self.r()).map_err(conn)?;
            wire::read_opt_path(self.r()).map_err(conn)?; // deriver
            read_paths(self.r())?; // references
            read_u64(self.r())?; // download size
            read_u64(self.r())?; // NAR size
            out.push(path);
        }
        Ok(out)
    }

    /// indirect-root-store.cc:addPermRoot: a symlink at `root` to
    /// `store_path`, then `AddIndirectRoot`.
    pub fn add_perm_root(&mut self, store_path: &str, root: &Path) -> Result<()> {
        if root.starts_with(path::store_dir()) {
            return Err(Error::Protocol(format!(
                "creating a garbage collector root ({}) in the Nix store is forbidden",
                root.display()
            )));
        }
        free_for_a_root(root)?;
        replace_symlink(store_path, root)
            .map_err(|e| Error::io(format!("cannot create {}", root.display()), e))?;
        let root_s = root
            .to_str()
            .ok_or_else(|| Error::Usage("gc root path is not UTF-8".into()))?;
        self.add_indirect_root(root_s, &mut |_| {})
    }

    /// `NarFromPath`: `sink` reads exactly the NAR from the connection.
    pub fn nar_from_path(
        &mut self,
        path: &str,
        on_event: &mut dyn FnMut(Event),
        sink: &mut dyn FnMut(&mut dyn Read) -> io::Result<()>,
    ) -> Result<()> {
        self.op(Op::NarFromPath)?;
        write_str(self.w(), path.as_bytes())?;
        self.process_stderr(on_event)?;
        sink(&mut self.from).map_err(conn)?;
        Ok(())
    }
}

/// indirect-root-store.cc:addPermRoot: a root replaces nothing but a symlink
/// that already points into the store. Everything else stays as it is.
pub fn free_for_a_root(root: &Path) -> Result<()> {
    let Ok(m) = fs::symlink_metadata(root) else {
        return Ok(());
    };
    let into_store = m.file_type().is_symlink()
        && fs::read_link(root).is_ok_and(|t| {
            t.to_string_lossy()
                .starts_with(&format!("{}/", super::super::store::path::store_dir()))
        });
    if into_store {
        return Ok(());
    }
    Err(Error::Store(format!(
        "cannot create symlink \"{}\"; already exists",
        root.display()
    )))
}

/// file-system.cc:replaceSymlink: a temporary link renamed over `link`.
pub fn replace_symlink(target: &str, link: &Path) -> io::Result<()> {
    let parent = link.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(parent)?;
    let name = link.file_name().unwrap_or_default().to_string_lossy();
    for n in 0.. {
        let tmp = parent.join(format!(".{n}_{name}"));
        match symlink(target, &tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        return fs::rename(&tmp, link).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        });
    }
    unreachable!()
}

pub fn failed_event(drv_path: &str, reason: &str) -> Event {
    Event::Failed(Failed {
        drv_path: drv_path.to_string(),
        reason: reason.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    /// A status of 256 truncated to a `u8` is 0, which `success` reads as
    /// Built: a failed build would have been reported as a success.
    #[test]
    fn a_status_over_a_byte_is_not_a_success() {
        let r = |status| BuildResult {
            path: String::new(),
            status,
            msg: String::new(),
        };
        assert!(r(0).success() && r(13).success());
        assert!(!r(256).success() && !r(269).success() && !r(3).success());
        assert_eq!(format!("status {}", r(256).status), "status 256");
    }

    use super::*;

    #[test]
    fn transport_from_url() {
        assert_eq!(
            Transport::parse("daemon"),
            Some(Transport::Unix(default_socket()))
        );
        assert_eq!(
            Transport::parse("unix:///tmp/s"),
            Some(Transport::Unix(PathBuf::from("/tmp/s")))
        );
        // nix.conf's default `store`, which `builder` falls back to.
        assert_eq!(Transport::parse("auto"), Transport::parse("daemon"));
        assert_eq!(Transport::parse(""), Transport::parse("daemon"));
        let ssh = |url: &str| match Transport::parse(url) {
            Some(Transport::Ssh {
                user,
                host,
                port,
                remote_program,
                key,
                compress,
            }) => (user, host, port, remote_program, key, compress),
            other => panic!("{url}: {other:?}"),
        };
        assert_eq!(
            ssh("ssh-ng://bob@builder.example"),
            (
                Some("bob".into()),
                "builder.example".into(),
                None,
                vec!["nix-daemon".into(), "--stdio".into()],
                None,
                false
            )
        );
        assert_eq!(ssh("ssh-ng://builder.example/").1, "builder.example");
        // ssh-store.cc appends --stdio to whatever remote-program names
        assert_eq!(
            ssh("ssh-ng://builder.example?remote-program=nix-daemon%20--store%20local%3Froot%3D/tmp/s")
                .3,
            vec![
                "nix-daemon".to_string(),
                "--store".into(),
                "local?root=/tmp/s".into(),
                "--stdio".into()
            ]
        );
        assert_eq!(ssh("ssh-ng://builder.example:2222").2, Some(2222));
        assert_eq!(ssh("ssh-ng://[::1]:2222").1, "::1");
        assert_eq!(ssh("ssh-ng://[::1]").1, "::1");
        assert_eq!(
            (
                ssh("ssh-ng://h?ssh-key=/k&compress=true").4,
                ssh("ssh-ng://h?compress=true").5
            ),
            (Some("/k".to_string()), true)
        );
        assert_eq!(Transport::parse("http://127.0.0.1:9455"), None);
    }

    /// An ssh host that starts with `-` would be an ssh option, so it never
    /// reaches the command line.
    #[rstest]
    #[case("ssh-ng://-oProxyCommand=touch%20/tmp/pwned")]
    #[case("ssh-ng://-l")]
    #[case("ssh-ng://-u@h")]
    #[case("ssh-ng://@h")]
    #[case("ssh-ng://")]
    fn an_ssh_host_is_never_an_option(#[case] url: &str) {
        assert_eq!(Transport::parse(url), None);
    }

    #[test]
    fn an_ssh_host_that_is_an_option_says_so() {
        let e = Transport::parse_checked("ssh-ng://-oProxyCommand=x", "x86_64-linux").unwrap_err();
        assert!(format!("{e}").contains("must not start with '-'"), "{e}");
    }

    #[test]
    fn ssh_argv_puts_the_host_first() {
        let t = Transport::parse("ssh-ng://bob@h:2222?ssh-key=/k&compress=true").unwrap();
        assert_eq!(
            ssh_args(&t).unwrap(),
            ["bob@h", "-x", "-i", "/k", "-C", "-p2222"]
        );
    }

    #[test]
    fn nix_sshopts_split_like_a_shell() {
        assert_eq!(
            shell_split(r#"-o "Proxy Command=a b" -i 'my key'"#).unwrap(),
            ["-o", "Proxy Command=a b", "-i", "my key"]
        );
        assert_eq!(shell_split(r"a\ b  c").unwrap(), ["a b", "c"]);
        assert_eq!(shell_split("  ").unwrap(), Vec::<String>::new());
        assert!(shell_split("'a").is_err());
    }
}
