//! A locked reference's tree: read from `/nix/store` when it is there, else
//! from the user-level input store (`crate::treestore`), else substituted by
//! a daemon if one is reachable, else downloaded into the input store by
//! iets itself (`github`, `gitlab`, `sourcehut`, `tarball`), else fetched by
//! running `nix` once (`git`, `hg`). Only that last step needs a daemon.
//!
//! The `narHash` is known before the fetch, so the tree names itself and the
//! input store refuses anything that does not hash to it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::{result, slice, thread};

use super::scm::{Commit, Entry};
use super::{AttrVal, Attrs, Tree, attrs_to_json, parse_nar_hash, str_attr, to_url};
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::fetch::http::{self, Limits};
use crate::fetch::spool;
use crate::log::{self, Activity};
use crate::progress::Mode;
use crate::remote::event;
use crate::remote::worker::{Connection, Transport};
use crate::session::Session;
use crate::store::hash::{Hash, HashAlgo, HashFormat, Hasher};
use crate::store::nar;
use crate::store::path::{IngestionMethod, StorePath};
use crate::store::source::{SourceOrigin, SrcPath};
use crate::treestore::{self, Rejected, StoredTree};
use crate::{config, nixbin, sys, trace};
use serde_json::Value;

/// A locked input's tree, named by the lock's `narHash` and readable at that
/// store path when this returns. The `fetchTree` builtin and `nix flake
/// prefetch-inputs` are the two callers, and they must agree on the name.
pub fn locked(ev: &mut Evaluator, attrs: &Attrs, pos: Pos) -> EvalResult<Tree> {
    let Some(nar_hash) = str_attr(attrs, "narHash") else {
        let msg = format!(
            "cannot fetch unlocked input '{}' (no narHash); run nix flake lock",
            to_url(attrs)
        );
        let mut e = Error::new(ErrorKind::Unsupported, msg, pos);
        e.hint = Some(
            "iets needs the 'narHash' to know the store path: pass it to fetchTree, \
             or run 'nix flake update' to write it into flake.lock"
                .into(),
        );
        return Err(Box::new(e));
    };
    let hash =
        parse_nar_hash(nar_hash).map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
    let mut attrs = attrs.clone();
    attrs.insert("narHash".into(), AttrVal::Str(hash.to_sri()));
    let path = treestore::store_path("source", &hash)
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
    ensure_readable(ev, &path, &hash, &attrs, pos)?;
    Ok(Tree {
        path,
        attrs,
        subdir: String::new(),
        counter: None,
    })
}

/// The lock file's name for a locked input, else its attrs as a URL.
fn input_name(ev: &Evaluator, attrs: &Attrs) -> String {
    str_attr(attrs, "narHash")
        .and_then(|h| ev.input_names.get(h))
        .unwrap_or_else(|| to_url(attrs))
}

/// A Nix `"..."` literal holding `s`.
fn nix_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The evaluator's connection to the eval store, opened on the first input
/// that is missing from it and kept for the rest of the evaluation. The eval
/// store is nix.conf's `store`, never the `builder` setting: a remote builder
/// has no say in what `import` reads. A `grpc://` store is not a
/// worker-protocol daemon, so there is nothing to open and nothing to
/// substitute with.
#[derive(Default)]
pub struct LazyStore {
    conn: Option<Connection>,
    tried: bool,
    validity: Arc<Mutex<Validity>>,
}

impl Clone for LazyStore {
    /// A fiber's evaluator starts without one: the worker protocol is one
    /// request at a time, so a connection is never shared. The fibers share
    /// the one that answers `valid_in_store`, since each open costs the
    /// daemon a fork (7-9 ms) and each query 60 µs.
    fn clone(&self) -> LazyStore {
        LazyStore {
            validity: self.validity.clone(),
            ..LazyStore::default()
        }
    }
}

impl LazyStore {
    fn get(&mut self) -> Option<&mut Connection> {
        if !self.tried {
            self.tried = true;
            let session = Session::current();
            match Transport::parse(&nix_conf(&session).store()) {
                Some(Transport::Grpc(_)) | None => {}
                // a missing daemon is not an error: the input store holds the
                // fetched inputs and evaluation writes no store
                Some(t) => {
                    if let Ok(c) = Connection::open(&t, &mut |_| {}) {
                        self.conn = Some(c);
                    }
                }
            }
        }
        self.conn.as_mut()
    }
}

/// The eval store's answers about what in `/nix/store` is valid, when it is a
/// daemon on this machine: a remote store's database says nothing about the
/// files here.
#[derive(Default)]
struct Validity {
    conn: Option<Connection>,
    tried: bool,
    valid: HashSet<String>,
}

impl Validity {
    /// `None` when there is no local daemon to ask. The first query also asks
    /// about `more`, which the evaluation is likely to ask about next.
    fn registered(&mut self, path: &str, more: impl FnOnce() -> Vec<String>) -> Option<bool> {
        if self.valid.contains(path) {
            return Some(true);
        }
        let mut ask = vec![path.to_string()];
        if !self.tried {
            self.tried = true;
            let session = Session::current();
            if let Some(t @ Transport::Unix(_)) = Transport::parse(&nix_conf(&session).store())
                && let Ok(c) = Connection::open(&t, &mut |_| {})
            {
                self.conn = Some(c);
            }
            if self.conn.is_some() {
                ask.extend(more());
            }
        }
        match self.conn.as_mut()?.query_valid_paths(&ask, &mut |_| {}) {
            Ok(v) => {
                self.valid.extend(v);
                Some(self.valid.contains(path))
            }
            Err(_) => {
                self.conn = None;
                None
            }
        }
    }
}

/// store-api.cc:isValidPath for a store path whose content `method` and `hash`
/// name. A file or directory there may be what an interrupted build or copy
/// left, which the store never registered. The local daemon answers; without
/// one the bytes must hash to `hash`. Only a valid path's kind is recorded:
/// it cannot change while the path stays valid, and the garbage collector
/// removes what it invalidates.
pub fn valid_in_store(
    ev: &mut Evaluator,
    sp: &StorePath,
    method: IngestionMethod,
    hash: &Hash,
) -> bool {
    let full = sp.to_full_string();
    let validity = ev.store.validity.clone();
    let names = ev.input_names.clone();
    // the lock's other inputs that are on disk, in the same query
    let locked = || {
        names
            .iter()
            .filter_map(|(h, _)| treestore::store_path("source", &parse_nar_hash(&h).ok()?).ok())
            .map(|sp| sp.to_full_string())
            .filter(|p| trace::stat_kind(Path::new(p), true) != trace::Kind::Missing)
            .collect()
    };
    let registered = || {
        validity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .registered(&full, locked)
    };
    match valid_kind(Path::new(&full), method, hash, registered) {
        Some(kind) => {
            ev.trace().stat(full.as_bytes(), true, kind);
            true
        }
        None => false,
    }
}

/// The kind of `path` when it holds a valid store path. `registered` is the
/// store's answer, `None` when there is no store to ask.
fn valid_kind(
    path: &Path,
    method: IngestionMethod,
    hash: &Hash,
    registered: impl FnOnce() -> Option<bool>,
) -> Option<trace::Kind> {
    let kind = trace::stat_kind(path, true);
    if kind == trace::Kind::Missing {
        return None;
    }
    let valid = match registered() {
        Some(v) => v,
        None => hashes_to(path, method, hash),
    };
    valid.then_some(kind)
}

fn hashes_to(path: &Path, method: IngestionMethod, hash: &Hash) -> bool {
    let got = match method {
        IngestionMethod::Recursive => {
            nar::nar_hash_filtered(path, hash.algo, &mut |_, _| true).map(|(h, _)| h)
        }
        IngestionMethod::Flat | IngestionMethod::Text => {
            if !fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
                return false;
            }
            nar::flat_hash(path, hash.algo)
        }
    };
    got.is_ok_and(|g| g == *hash)
}

/// fetchers.cc:getAccessorUnchecked: a locked input carries `__final`, so Nix
/// substitutes its store path before it runs any fetcher. An expression cannot
/// mark an input final, so iets substitutes the tree itself before it fetches.
/// One `QueryValidPaths` with the substitute flag does it: the daemon runs
/// `Store::substitutePaths` and answers with the paths it now has
/// (daemon.cc:performOp, >= 1.27). Nothing is evaluated or built.
fn substitute(ev: &mut Evaluator, store_path: &str) -> bool {
    let Some(conn) = ev.store.get() else {
        return false;
    };
    // the daemon's own lines, such as a substituter it retries
    let mut said = |e: event::Event| {
        if let event::Event::Log(l) = e {
            log::line(&String::from_utf8_lossy(&l.line));
        }
    };
    match conn.substitute_paths(slice::from_ref(&store_path.to_string()), &mut said) {
        Ok(valid) => valid.iter().any(|p| p == store_path),
        Err(e) => {
            log::line(&format!("iets: cannot substitute '{store_path}': {e}"));
            false
        }
    }
}

/// A remote input's tree must be readable at its store path so `import` reads
/// exactly what Nix reads. `/nix/store` is used when it has the tree, then the
/// user-level input store; a tarball-shaped input in neither is downloaded into
/// that, and reads come out of the spool by offset (`crate::treestore`). A
/// reachable daemon is asked to substitute first, since a local copy beats a
/// download. A `git` or `hg` input still goes through Nix's `fetchTree`, which
/// writes `/nix/store`. Never called for the root tree or a `path` input.
pub fn ensure_readable(
    ev: &mut Evaluator,
    sp: &StorePath,
    nar_hash: &Hash,
    attrs: &Attrs,
    pos: Pos,
) -> EvalResult<()> {
    ev.known.lock().entry(sp.clone()).or_default();
    let full = sp.to_full_string();
    if valid_in_store(ev, sp, IngestionMethod::Recursive, nar_hash) {
        return Ok(());
    }
    if let Some(entry) = treestore::get(sp.base_name()) {
        adopt(ev, sp, &entry);
        return Ok(());
    }
    let name = input_name(ev, attrs);
    log::line(&format!(
        "iets: input '{name}' is not in the store ({full}), fetching it"
    ));
    if substitute(ev, &full) {
        ev.trace()
            .stat(full.as_bytes(), true, trace::Kind::Directory);
        return Ok(());
    }
    if is_tarball_type(attrs) {
        let entry = fetch_tarball(ev, sp, attrs, pos)?;
        adopt(ev, sp, &entry);
        return Ok(());
    }
    fetch_with_nix(&full, &name, attrs, pos)?;
    if ev.trace().stat_now(full.as_bytes(), true) == trace::Kind::Missing {
        bail!(
            ErrorKind::Io,
            pos,
            "iets: input '{name}' is not in the store ({full})"
        );
    }
    Ok(())
}

/// Points every read of `sp` at the input store entry, and records the entry in
/// the read set (`crate::treestore::record`).
fn adopt(ev: &mut Evaluator, sp: &StorePath, entry: &StoredTree) {
    ev.known.add(
        sp.clone(),
        SourceOrigin::tree(
            SrcPath::spool(sp.base_name(), b""),
            entry.nar_hash(),
            entry.nar_size(),
        ),
    );
    treestore::record(ev, entry, sp.base_name());
}

/// A directory being built from a revision's entries.
#[derive(Default)]
struct Building(BTreeMap<Vec<u8>, Built>);

enum Built {
    Dir(Building),
    Leaf(spool::Node),
}

impl Building {
    fn dir(&mut self, parts: &[&[u8]]) -> &mut Building {
        let Some((first, rest)) = parts.split_first() else {
            return self;
        };
        let next = self
            .0
            .entry(first.to_vec())
            .or_insert_with(|| Built::Dir(Building::default()));
        if let Built::Leaf(_) = next {
            *next = Built::Dir(Building::default());
        }
        let Built::Dir(d) = next else { unreachable!() };
        d.dir(rest)
    }

    fn node(self) -> spool::Node {
        let entries = self.0.into_iter().map(|(name, b)| match b {
            Built::Dir(d) => (name, d.node()),
            Built::Leaf(n) => (name, n),
        });
        spool::Node::Dir(entries.collect())
    }
}

/// The tree of a revision as the store path `name` gives it, readable there
/// when this returns: `/nix/store` when it holds the tree, else the input
/// store, which takes it in when it does not have it. The tree is hashed
/// first, since nothing names it before.
pub fn store_commit(
    ev: &mut Evaluator,
    name: &str,
    commit: Commit,
    pos: Pos,
) -> EvalResult<(StorePath, Hash)> {
    let fail = |e: String| Error::boxed(ErrorKind::Io, e, pos);
    let mut file = spool::scratch().map_err(|e| fail(e.to_string()))?;
    let mut offset = 0;
    let mut root = Building::default();
    for (path, entry) in commit.entries {
        let parts: Vec<&[u8]> = path.split(|&b| b == b'/').collect();
        let (last, dirs) = parts.split_last().expect("a path has a part");
        let node = match entry {
            Entry::Empty => {
                root.dir(&parts);
                continue;
            }
            Entry::Symlink(target) => spool::Node::Symlink(target),
            Entry::File { bytes, exec } => {
                file.write_all(&bytes).map_err(|e| fail(e.to_string()))?;
                let size = bytes.len() as u64;
                let at = spool::Blob::Spool {
                    offset,
                    size,
                    sparse: false,
                };
                offset += size;
                spool::Node::File { at, exec }
            }
        };
        root.dir(dirs).0.insert(last.to_vec(), Built::Leaf(node));
    }
    let tree = spool::Tree::spooled(file, root.node());
    let mut h = Hasher::new(HashAlgo::Sha256);
    tree.dump(&mut h).map_err(|e| fail(e.to_string()))?;
    let hash = h.finish();
    let sp = treestore::store_path(name, &hash).map_err(|e| fail(e.to_string()))?;
    ev.known.lock().entry(sp.clone()).or_default();
    if valid_in_store(ev, &sp, IngestionMethod::Recursive, &hash) {
        return Ok((sp, hash));
    }
    let entry = match treestore::get(sp.base_name()) {
        Some(entry) => entry,
        None => {
            let staged = treestore::stage(sp.base_name())
                .ok_or_else(|| fail("iets has no cache directory to hold a Git tree".into()))?;
            treestore::put(name, IngestionMethod::Recursive, &hash, staged, tree).map_err(|e| {
                match e {
                    Rejected::Io(e) => fail(e.to_string()),
                    _ => fail("iets cannot keep a Git tree in the input store".into()),
                }
            })?
        }
    };
    adopt(ev, &sp, &entry);
    Ok((sp, hash))
}

/// tarball.cc:downloadFile for `builtins.fetchurl`: the bytes `url` serves, as
/// the flat fixed output `name` whose bytes hash to `expected`. The path is
/// known before the download, so `/nix/store` or the input store may hold it
/// already, as primops/fetchTree.cc:fetch checks first. Not substituted, as in
/// Nix (NixOS/nix#4313).
pub fn fetch_file(
    ev: &mut Evaluator,
    url: &str,
    name: &str,
    expected: Hash,
    pos: Pos,
) -> EvalResult<StorePath> {
    let fail = |e: String| Error::boxed(ErrorKind::Eval, e, pos);
    let sp = treestore::file_path(name, &expected).map_err(|e| fail(e.to_string()))?;
    ev.known.lock().entry(sp.clone()).or_default();
    if valid_in_store(ev, &sp, IngestionMethod::Flat, &expected) {
        return Ok(sp);
    }
    if let Some(entry) = treestore::get(sp.base_name()) {
        adopt_file(ev, &sp, &entry, expected);
        return Ok(sp);
    }
    let shown = http::shown(url);
    let _fetching = treestore::fetch_lock(sp.base_name(), || {
        log::line(&format!(
            "iets: waiting for another process to download '{shown}'"
        ))
    });
    if let Some(entry) = treestore::reopen(sp.base_name()) {
        adopt_file(ev, &sp, &entry, expected);
        return Ok(sp);
    }
    let cannot = |e: String| fail(format!("unable to download '{shown}': {e}"));
    let staged = treestore::stage(sp.base_name())
        .ok_or_else(|| cannot("iets has no cache directory to hold it".into()))?;
    let mut spool = spool::spool_file(staged.dir()).map_err(|e| cannot(e.to_string()))?;
    let limits = Limits::new(nix_conf(ev.session())).map_err(|e| cannot(e.to_string()))?;
    http::get(url, &[], limits, false, &mut spool).map_err(|e| cannot(e.to_string()))?;
    let tree = spool::Tree::file(spool).map_err(|e| cannot(e.to_string()))?;
    let entry = treestore::put(name, IngestionMethod::Flat, &expected, staged, tree).map_err(
        |e| match e {
            Rejected::Mismatch(got) => {
                // primops/fetchTree.cc:fetch
                let msg = format!(
                    "hash mismatch in file downloaded from '{shown}':\n  specified: {}\n  got:       {}",
                    expected.to_string_with(HashFormat::Nix32, true),
                    got.to_string_with(HashFormat::Nix32, true)
                );
                let mut e = Error::new(ErrorKind::HashMismatch, msg, pos);
                e.hint = Some(format!("set sha256 to '{}'", got.to_sri()));
                Box::new(e)
            }
            Rejected::Io(e) => cannot(e.to_string()),
            Rejected::Unstorable => cannot("iets cannot keep it in the input store".into()),
        },
    )?;
    adopt_file(ev, &sp, &entry, expected);
    Ok(sp)
}

/// [`adopt`] for a flat file, which is addressed by the hash of its bytes and
/// not by its NAR hash.
fn adopt_file(ev: &mut Evaluator, sp: &StorePath, entry: &StoredTree, flat: Hash) {
    ev.known.add(
        sp.clone(),
        SourceOrigin::flat(
            SrcPath::spool(sp.base_name(), b""),
            flat,
            entry.nar_hash(),
            entry.nar_size(),
        ),
    );
    treestore::record(ev, entry, sp.base_name());
}

/// nix.conf, read once: `store` for the eval store and `access-tokens` for the
/// forge headers. The file cannot change under a running evaluation.
fn nix_conf(s: &Session) -> &config::Config {
    s.nix_conf
        .get_or_init(|| config::Config::read(&[]).unwrap_or_else(|_| config::Config::empty()))
}

/// The types whose fetch is "download one archive and unpack it"
/// (github.cc:GitArchiveInputScheme, tarball.cc:TarballInputScheme). `git` and
/// `hg` are not here: they need a repository, so they still go through nix.
/// `file` is not either: its store path is the flat hash of the bytes under the
/// input's name, which the lock's `narHash` does not give, so iets cannot say
/// where the file belongs before it has it.
fn is_tarball_type(attrs: &Attrs) -> bool {
    matches!(
        str_attr(attrs, "type").unwrap_or(""),
        "github" | "gitlab" | "sourcehut" | "tarball"
    )
}

/// github.cc:getAccessToken: the longest `access-tokens` key that is a prefix of
/// `host/owner/repo` and ends on a path segment, else the entry for the host.
fn access_token(cfg: &config::Config, host: &str, host_and_path: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    let mut for_host = None;
    for entry in cfg.list("access-tokens") {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        if key == host {
            for_host = Some(value.to_string());
        }
        if host_and_path.starts_with(key)
            && (host_and_path.len() == key.len() || host_and_path.as_bytes()[key.len()] == b'/')
            && best.as_ref().is_none_or(|(len, _)| key.len() > *len)
        {
            best = Some((key.len(), value.to_string()));
        }
    }
    best.map(|(_, v)| v).or(for_host)
}

/// The `Authorization`-style header each forge wants for its token
/// (github.cc:accessHeaderFromToken, one override per scheme).
fn access_header(ty: &str, token: &str) -> (String, String) {
    match ty {
        "sourcehut" => ("Authorization".into(), format!("Bearer {token}")),
        "gitlab" => match token.split_once(':') {
            Some(("OAuth2", t)) => ("Authorization".into(), format!("Bearer {t}")),
            Some(("PAT", t)) => ("Private-token".into(), t.into()),
            Some((name, value)) => (name.into(), value.into()),
            None => (token.into(), token.into()),
        },
        _ => ("Authorization".into(), format!("token {token}")),
    }
}

/// github.cc:getDownloadUrl and tarball.cc: the URL and headers Nix downloads a
/// locked archive input from. The `rev` is the 40 hex digits the lock holds.
fn tarball_url(
    cfg: &config::Config,
    attrs: &Attrs,
) -> result::Result<(String, Vec<(String, String)>), String> {
    let ty = str_attr(attrs, "type").unwrap_or("");
    if ty == "tarball" {
        let url = str_attr(attrs, "url").ok_or("the lock has no 'url'")?;
        return Ok((url.to_string(), Vec::new()));
    }
    let owner = str_attr(attrs, "owner").ok_or("the lock has no 'owner'")?;
    let repo = str_attr(attrs, "repo").ok_or("the lock has no 'repo'")?;
    let rev = str_attr(attrs, "rev").ok_or("the lock has no 'rev'")?;
    let default_host = match ty {
        "gitlab" => "gitlab.com",
        "sourcehut" => "git.sr.ht",
        _ => "github.com",
    };
    let host = str_attr(attrs, "host").unwrap_or(default_host);
    let headers: Vec<(String, String)> = access_token(cfg, host, &format!("{host}/{owner}/{repo}"))
        .map(|t| vec![access_header(ty, &t)])
        .unwrap_or_default();
    let url = match ty {
        "gitlab" => format!(
            "https://{host}/api/v4/projects/{owner}%2F{repo}/repository/archive.tar.gz?sha={rev}"
        ),
        "sourcehut" => format!("https://{host}/{owner}/{repo}/archive/{rev}.tar.gz"),
        // With a token the API endpoint; without one the public archive, which
        // has the looser rate limit.
        _ if host != "github.com" => {
            format!("https://{host}/api/v3/repos/{owner}/{repo}/tarball/{rev}")
        }
        _ if headers.is_empty() => {
            format!("https://{host}/{owner}/{repo}/archive/{rev}.tar.gz")
        }
        _ => format!("https://api.{host}/repos/{owner}/{repo}/tarball/{rev}"),
    };
    Ok((url, headers))
}

/// Downloads the input's archive into the input store, indexes it like Nix
/// unpacks it, checks the NAR hash against the lock and keeps the spool. The
/// archive is streamed: the only copy is the decompressed tar, which the NAR is
/// written out of once to hash it. A tree whose hash is not the lock's is thrown
/// away, so a wrong tree is never named, never read and never uploaded. Nothing
/// is registered anywhere: `remote::plan` sends the NAR out of this spool when a
/// build needs it, and evaluation needs no store at all.
fn fetch_tarball(
    ev: &mut Evaluator,
    sp: &StorePath,
    attrs: &Attrs,
    pos: Pos,
) -> EvalResult<Arc<StoredTree>> {
    let name = input_name(ev, attrs);
    let fail = |e: String| Error::boxed(ErrorKind::Eval, e, pos);
    let (url, headers) = tarball_url(nix_conf(ev.session()), attrs)
        .map_err(|e| fail(format!("iets: cannot fetch input '{name}': {e}")))?;
    let expected = str_attr(attrs, "narHash")
        .and_then(|h| parse_nar_hash(h).ok())
        .ok_or_else(|| fail(format!("iets: input '{name}' has no usable 'narHash'")))?;

    let _fetching = treestore::fetch_lock(sp.base_name(), || {
        log::line(&format!(
            "iets: waiting for another process to fetch input '{name}'"
        ))
    });
    if let Some(entry) = treestore::reopen(sp.base_name()) {
        return Ok(entry);
    }
    let staged = treestore::stage(sp.base_name()).ok_or_else(|| {
        fail(format!(
            "iets: cannot fetch input '{name}': no cache directory to hold it"
        ))
    })?;
    let mut spool = spool::spool_file(staged.dir())
        .map_err(|e| fail(format!("iets: cannot fetch input '{name}': {e}")))?;
    let limits = Limits::new(nix_conf(ev.session()))
        .map_err(|e| fail(format!("iets: cannot fetch input '{name}': {e}")))?;
    http::get_decompressed(&url, &headers, limits, &mut spool)
        .map_err(|e| fail(format!("iets: cannot fetch input '{name}': {e}")))?;
    let tree = spool::Tree::index(spool, limits.max_size)
        .map_err(|e| fail(format!("iets: cannot unpack input '{name}': {e}")))?;

    // the store verifies the tree against the lock's hash and names it by
    // what it hashes to, so a wrong tree is never named, read or uploaded
    treestore::put(
        sp.name(),
        IngestionMethod::Recursive,
        &expected,
        staged,
        tree,
    )
    .map_err(|e| match e {
        Rejected::Mismatch(got) => {
            let msg = format!(
                "iets: NAR hash mismatch in input '{name}' ({}): expected '{}' \
                 but the tree iets fetched hashes to '{}'",
                crate::fetch::http::shown(&url),
                expected.to_sri(),
                got.to_sri()
            );
            Error::hash_mismatch(msg, &got, None, pos)
        }
        Rejected::Io(e) => Error::boxed(
            ErrorKind::Io,
            format!("iets: cannot hash the tree of input '{name}': {e}"),
            pos,
        ),
        Rejected::Unstorable => fail(format!(
            "iets: cannot keep the fetched input '{name}' in the input store"
        )),
    })
}

/// The last shell-out: a `git` or `hg` input needs a repository, which iets
/// cannot clone yet, so its locked attrs go to Nix's own `fetchTree` and the
/// store path Nix reports must be the one the lock implies.
fn fetch_with_nix(full: &str, name: &str, attrs: &Attrs, pos: Pos) -> EvalResult<()> {
    let expr = format!(
        "(builtins.fetchTree (builtins.fromJSON {})).outPath",
        nix_string_literal(&attrs_to_json(attrs))
    );
    let missing = || {
        format!(
            "iets: input '{name}' is not in the store and nix is not on PATH; \
             fetch it with nix ({}) first",
            to_url(attrs)
        )
    };
    let (ok, stdout, said) = nix_eval(&format!("input '{name}'"), &expr, missing, false, pos)?;
    if !ok {
        let msg = format!(
            "iets: nix could not fetch input '{name}' ({})",
            to_url(attrs)
        );
        // fetchers.cc:Input::checkLocks
        if str_attr(attrs, "type") == Some("git") && said.contains("NAR hash mismatch in input") {
            let mut e = Error::new(ErrorKind::HashMismatch, msg, pos);
            e.hint = Some(
                "git gave a different tree than the lock names: a .gitattributes filter \
                 (such as git-lfs), 'export-subst' or a submodule can cause that; \
                 fix it, or run 'nix flake update' to lock the tree git gives"
                    .into(),
            );
            return Err(Box::new(e));
        }
        bail!(ErrorKind::Eval, pos, "{msg}");
    }
    let got = String::from_utf8_lossy(&stdout).trim().to_string();
    if got != full {
        bail!(
            ErrorKind::Eval,
            pos,
            "iets: nix fetched '{got}' for input '{name}' but the lock implies '{full}'"
        );
    }
    Ok(())
}

/// A local Git flake whose submodules must be included in its source tree.
/// Nix fetches it into the store; a tracing-cache hit cannot safely assume
/// that files inside the submodules are unchanged.
pub fn submodule_root(ev: &mut Evaluator, root: &Path, subdir: &str, pos: Pos) -> EvalResult<Tree> {
    ev.trace().mark_uncacheable();
    let url = format!(
        "file://{}",
        String::from_utf8_lossy(&sys::path_to_internal(root))
    );
    let mut attrs = Attrs::from([
        ("type".into(), AttrVal::Str("git".into())),
        ("url".into(), AttrVal::Str(url)),
        ("submodules".into(), AttrVal::Bool(true)),
    ]);
    let got = fetch_with_nix_builtin(&attrs, &root.display().to_string(), "fetchTree", pos)?;
    let out = got
        .get("outPath")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::boxed(ErrorKind::Eval, "iets: nix gave no store path", pos))?
        .to_string();
    for (k, v) in got {
        if k == "outPath" {
            continue;
        }
        let value = match v {
            Value::String(s) => AttrVal::Str(s),
            Value::Bool(b) => AttrVal::Bool(b),
            Value::Number(n) => match n.as_u64() {
                Some(n) => AttrVal::Int(n),
                None => continue,
            },
            _ => continue,
        };
        attrs.insert(k, value);
    }
    let hash = str_attr(&attrs, "narHash")
        .and_then(|s| parse_nar_hash(s).ok())
        .ok_or_else(|| Error::boxed(ErrorKind::Eval, "iets: nix gave no narHash", pos))?;
    let path = treestore::store_path("source", &hash)
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
    if out != path.to_full_string() {
        bail!(
            ErrorKind::Eval,
            pos,
            "iets: nix fetched '{out}' but its narHash implies '{path}'"
        );
    }
    ensure_readable(ev, &path, &hash, &attrs, pos)?;
    let counter = str_attr(&attrs, "rev").map(|_| {
        (
            super::scm::named("git").expect("iets knows git"),
            root.to_path_buf(),
        )
    });
    Ok(Tree {
        path,
        attrs,
        subdir: subdir.to_string(),
        counter,
    })
}

/// `builtins.fetchGit` of a commit, run by Nix: `args` are the call's
/// attributes, `shown` is the input as Nix names it. Returns the result's
/// attributes, with the store path as `outPath`.
pub fn fetch_git_with_nix(
    args: &Attrs,
    shown: &str,
    pos: Pos,
) -> EvalResult<serde_json::Map<String, Value>> {
    fetch_with_nix_builtin(args, shown, "fetchGit", pos)
}

fn fetch_with_nix_builtin(
    args: &Attrs,
    shown: &str,
    builtin: &str,
    pos: Pos,
) -> EvalResult<serde_json::Map<String, Value>> {
    let expr = format!(
        "let r = builtins.{builtin} (builtins.fromJSON {}); \
         in builtins.toJSON (builtins.removeAttrs r [ \"outPath\" ] // {{ path = r.outPath; }})",
        nix_string_literal(&attrs_to_json(args))
    );
    let missing = || format!("iets: builtins.{builtin} of '{shown}' needs nix on PATH");
    let what = format!("the Git repository '{shown}'");
    let (ok, stdout, said) = nix_eval(&what, &expr, missing, true, pos)?;
    if !ok {
        let last = said.rfind("error: ").map(|i| said[i + 7..].trim());
        match last {
            Some(m) => bail!(ErrorKind::Eval, pos, "{m}"),
            None => bail!(ErrorKind::Eval, pos, "iets: nix could not fetch {what}"),
        }
    }
    let mut out: serde_json::Map<String, Value> = serde_json::from_slice(&stdout)
        .map_err(|e| Error::boxed(ErrorKind::Eval, format!("iets: nix said: {e}"), pos))?;
    if let Some(p) = out.remove("path") {
        out.insert("outPath".into(), p);
    }
    Ok(out)
}

/// `nix eval --raw --impure --expr EXPR` for a fetch of `what`, its records
/// relayed, its errors only into what it said when `keep_errors` is set:
/// whether it succeeded, its stdout and what it said.
fn nix_eval(
    what: &str,
    expr: &str,
    missing: impl FnOnce() -> String,
    keep_errors: bool,
    pos: Pos,
) -> EvalResult<(bool, Vec<u8>, String)> {
    if Session::current().settings().fetching {
        bail!(
            ErrorKind::Eval,
            pos,
            "iets: recursive fetch of {what}: the nix on PATH is iets itself"
        );
    }
    let Some(nix) = nixbin::real("nix") else {
        bail!(ErrorKind::Eval, pos, "{}", missing());
    };
    let act = Activity::new(
        log::LVL_TALKATIVE,
        log::ACT_FETCH_TREE,
        &format!("fetching {what} with nix"),
        vec![],
    );
    let cannot = |e: io::Error| {
        Error::boxed(
            ErrorKind::Io,
            format!("iets: cannot run nix to fetch {what}: {e}"),
            pos,
        )
    };
    // git may ask for credentials on the terminal, as under Nix
    let suspended = log::suspend();
    let mut child = Command::new(&nix)
        .env("IETS_FETCHING", "1")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "--log-format",
            "internal-json",
            "eval",
            "--raw",
            "--impure",
            "--expr",
            expr,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(cannot)?;
    let mut stdout = child.stdout.take().expect("piped");
    let read_out = thread::spawn(move || {
        let mut v = Vec::new();
        stdout.read_to_end(&mut v).map(|_| v)
    });
    let said = relay(child.stderr.take().expect("piped"), &act, keep_errors);
    let status = child.wait().map_err(cannot)?;
    let stdout = read_out
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("the reader panicked")))
        .map_err(cannot)?;
    drop((act, suspended));
    Ok((status.success(), stdout, said))
}

/// Nix's records go under `act`; git's `\r` redraws go to a terminal as they
/// are, as under Nix, or into `act`'s detail. With `keep_errors` an error
/// is not printed: its innermost message goes into the text, after
/// `error: `. Returns the text.
fn relay(from: impl Read, act: &Activity, keep_errors: bool) -> String {
    let session = Session::current();
    let terminal = session.log.progress.mode() == Mode::Bar && !log::json();
    let mut said = String::new();
    let mut ids = HashMap::new();
    let mut seg = Vec::new();
    for b in BufReader::new(from).bytes() {
        let Ok(b) = b else { break };
        if b != b'\n' && b != b'\r' {
            seg.push(b);
            continue;
        }
        let text = String::from_utf8_lossy(&seg).into_owned();
        seg.clear();
        if b == b'\r' {
            match terminal {
                true => session.log.err(format!("{text}\r").as_bytes()),
                false => log::detail(act.0, text.trim()),
            }
            continue;
        }
        let Some(v) = text
            .strip_prefix("@nix ")
            .and_then(|j| serde_json::from_str::<Value>(j).ok())
        else {
            if !text.is_empty() {
                log::line(&text);
                said.push_str(&text);
                said.push('\n');
            }
            continue;
        };
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        let fields = || v["fields"].as_array().cloned().unwrap_or_default();
        match v["action"].as_str() {
            Some("start") => {
                let id = log::activity_id();
                ids.insert(n("id"), id);
                let up = ids.get(&n("parent")).copied().unwrap_or(act.0);
                let text = v["text"].as_str().unwrap_or("");
                log::start(id, n("level"), n("type"), text, fields(), up);
            }
            Some("stop") => {
                if let Some(id) = ids.remove(&n("id")) {
                    log::stop(id);
                }
            }
            Some("result") => {
                if let Some(&id) = ids.get(&n("id")) {
                    log::result(id, n("type"), fields());
                }
            }
            Some("msg") => {
                let msg = v["msg"].as_str().unwrap_or("");
                if keep_errors && n("level") == 0 {
                    let raw = v["raw_msg"].as_str().unwrap_or(msg);
                    said.push_str("error: ");
                    said.push_str(&log::strip_ansi(raw));
                    said.push('\n');
                    continue;
                }
                log::line(msg);
                said.push_str(msg);
                said.push('\n');
            }
            _ => {}
        }
    }
    for (_, id) in ids {
        log::stop(id);
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tempfile::tempdir;

    use std::time::Instant;

    use IngestionMethod::{Flat, Recursive};
    use trace::Kind::{Directory, Regular};

    #[test]
    fn nix_literal_escapes() {
        assert_eq!(
            nix_string_literal(r#"{"a":"x\"y${z}\\"}"#),
            r#""{\"a\":\"x\\\"y\${z}\\\\\"}""#
        );
    }

    fn attrs(pairs: &[(&str, &str)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), AttrVal::Str((*v).to_string())))
            .collect()
    }

    fn conf(text: &str) -> config::Config {
        let mut c = config::Config::empty();
        c.apply(text, Path::new("test")).unwrap();
        c
    }

    const REV: &str = "b1d9ab70662946ef0850d488da1c9019f3a9752a";

    #[test]
    fn download_urls_match_the_fetchers() {
        let none = conf("");
        let gh = attrs(&[
            ("type", "github"),
            ("owner", "numtide"),
            ("repo", "flake-utils"),
            ("rev", REV),
        ]);
        assert_eq!(
            tarball_url(&none, &gh).unwrap(),
            (
                format!("https://github.com/numtide/flake-utils/archive/{REV}.tar.gz"),
                vec![]
            )
        );
        // A token moves github.com to the API endpoint (github.cc:getDownloadUrl).
        assert_eq!(
            tarball_url(&conf("access-tokens = github.com=T"), &gh).unwrap(),
            (
                format!("https://api.github.com/repos/numtide/flake-utils/tarball/{REV}"),
                vec![("Authorization".into(), "token T".into())]
            )
        );
        let mut ghe = gh.clone();
        ghe.insert("host".into(), AttrVal::Str("git.corp".into()));
        assert_eq!(
            tarball_url(&none, &ghe).unwrap().0,
            format!("https://git.corp/api/v3/repos/numtide/flake-utils/tarball/{REV}")
        );

        let gl = attrs(&[
            ("type", "gitlab"),
            ("owner", "o"),
            ("repo", "r"),
            ("rev", REV),
        ]);
        assert_eq!(
            tarball_url(&conf("access-tokens = gitlab.com=PAT:T"), &gl).unwrap(),
            (
                format!(
                    "https://gitlab.com/api/v4/projects/o%2Fr/repository/archive.tar.gz?sha={REV}"
                ),
                vec![("Private-token".into(), "T".into())]
            )
        );
        assert_eq!(
            tarball_url(&conf("access-tokens = gitlab.com=OAuth2:T"), &gl)
                .unwrap()
                .1,
            vec![("Authorization".into(), "Bearer T".into())]
        );

        let sh = attrs(&[
            ("type", "sourcehut"),
            ("owner", "~o"),
            ("repo", "r"),
            ("rev", REV),
        ]);
        assert_eq!(
            tarball_url(&conf("access-tokens = git.sr.ht=T"), &sh).unwrap(),
            (
                format!("https://git.sr.ht/~o/r/archive/{REV}.tar.gz"),
                vec![("Authorization".into(), "Bearer T".into())]
            )
        );

        let tb = attrs(&[("type", "tarball"), ("url", "https://x.example/a.tar.xz")]);
        assert_eq!(
            tarball_url(&none, &tb).unwrap(),
            ("https://x.example/a.tar.xz".to_string(), vec![])
        );

        assert!(tarball_url(&none, &attrs(&[("type", "github")])).is_err());
    }

    /// github.cc:getAccessToken: the longest key that is a prefix of
    /// `host/owner/repo` on a path boundary wins, else the host's own entry.
    #[test]
    fn access_tokens_match_the_longest_prefix() {
        let c = conf(
            "access-tokens = github.com=host github.com/a=owner github.com/a/b=repo github.com/ab=other",
        );
        assert_eq!(
            access_token(&c, "github.com", "github.com/a/b").as_deref(),
            Some("repo")
        );
        assert_eq!(
            access_token(&c, "github.com", "github.com/a/c").as_deref(),
            Some("owner")
        );
        assert_eq!(
            access_token(&c, "github.com", "github.com/ab/c").as_deref(),
            Some("other")
        );
        assert_eq!(
            access_token(&c, "github.com", "github.com/z/y").as_deref(),
            Some("host")
        );
        assert_eq!(access_token(&c, "gitlab.com", "gitlab.com/z/y"), None);
    }

    /// What the substitution step costs, against `nix build --no-link`, on a
    /// store path no substituter has: both must answer "no", and
    /// the op must be the cheaper way to hear it. Needs a running daemon and a
    /// nix on `PATH`, so it is ignored by default:
    /// `cargo test --release -- --ignored --nocapture substitution`.
    #[test]
    #[ignore]
    fn substitution_costs_less_than_the_subprocess() {
        let path = "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-source";
        let mut store = LazyStore::default();
        let t = Instant::now();
        let conn = store.get().expect("a daemon");
        let connect = t.elapsed();
        let t = Instant::now();
        let valid = conn
            .substitute_paths(&[path.to_string()], &mut |_| {})
            .unwrap();
        let op = t.elapsed();
        assert!(valid.is_empty());

        let nix = nixbin::real("nix").expect("a nix");
        let t = Instant::now();
        let status = Command::new(&nix)
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "build",
                "--no-link",
                path,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        let subprocess = t.elapsed();
        assert!(!status.success());
        errln!(
            "connect: {:.3}s, QueryValidPaths(substitute): {:.3}s, nix build --no-link: {:.3}s",
            connect.as_secs_f64(),
            op.as_secs_f64(),
            subprocess.as_secs_f64()
        );
        // The connection is opened once and the registration reuses it; the op
        // is what replaces the subprocess.
        assert!(op < subprocess);
    }

    #[rstest]
    #[case("github", true)]
    #[case("gitlab", true)]
    #[case("sourcehut", true)]
    #[case("tarball", true)]
    #[case("git", false)]
    #[case("hg", false)]
    #[case("path", false)]
    #[case("file", false)]
    fn only_the_archive_types_are_fetched_by_iets(#[case] ty: &str, #[case] want: bool) {
        assert_eq!(is_tarball_type(&attrs(&[("type", ty)])), want);
    }

    #[test]
    fn a_leftover_is_not_a_valid_path() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("tree")).unwrap();
        fs::write(d.path().join("tree/f"), b"hi\n").unwrap();
        fs::write(d.path().join("file"), b"hi\n").unwrap();
        let mut h = Hasher::new(HashAlgo::Sha256);
        h.update(b"hi\n");
        let flat = h.finish();
        let tree = nar::nar_hash(&d.path().join("tree")).unwrap();
        let no_store = || None;

        assert_eq!(
            valid_kind(&d.path().join("file"), Flat, &flat, no_store),
            Some(Regular)
        );
        assert_eq!(
            valid_kind(&d.path().join("tree"), Recursive, &tree, no_store),
            Some(Directory)
        );
        assert_eq!(
            valid_kind(&d.path().join("tree"), Flat, &flat, no_store),
            None
        );
        assert_eq!(
            valid_kind(&d.path().join("file"), Recursive, &tree, no_store),
            None
        );
        assert_eq!(
            valid_kind(&d.path().join("file"), Flat, &flat, || Some(false)),
            None
        );
        assert_eq!(
            valid_kind(&d.path().join("file"), Flat, &tree, || Some(true)),
            Some(Regular)
        );
        let asked = || -> Option<bool> { panic!("a missing path needs no store") };
        assert_eq!(valid_kind(&d.path().join("gone"), Flat, &flat, asked), None);

        fs::write(d.path().join("file"), b"half").unwrap();
        fs::write(d.path().join("tree/f"), b"half").unwrap();
        assert_eq!(
            valid_kind(&d.path().join("file"), Flat, &flat, no_store),
            None
        );
        assert_eq!(
            valid_kind(&d.path().join("tree"), Recursive, &tree, no_store),
            None
        );
    }
}
