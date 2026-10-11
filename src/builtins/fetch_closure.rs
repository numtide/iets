//! `builtins.fetchClosure` (primops/fetchClosure.cc). iets has no store, so
//! the closure goes to the eval store's nix-daemon, as Nix's own client
//! sends it there: iets reads the `.narinfo` files and NARs from the HTTP
//! binary cache (`crate::fetch::binary_cache`) and adds the paths with
//! `AddMultipleToStore`, signature check on. The daemon accepts a
//! content-addressed path on its content and an input-addressed one only
//! with a signature by one of its `trusted-public-keys`, as it does for Nix.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{self, Read, Write};
use std::{env, result};

use super::paths::store_path_string;

use crate::compile::canon_path;
use crate::error::{Error as StoreError, io_text};
use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::fetch::binary_cache::{BinaryCache, NarInfo};
use crate::fetch::http::Limits;
use crate::log;
use crate::remote::copy::Copying;
use crate::remote::event::Event;
use crate::remote::grpc::drvwire::PathInfo;
use crate::remote::worker::Connection;
use crate::remote::worker::wire::write_u64;
use crate::runtime::{Ctx, Value, Word};
use crate::source::nix_conf;
use crate::store::hash::{HashAlgo, hash_bytes};
use crate::store::path::{StorePath, make_source_path, path_from_ca};
use crate::store::rewrite::{hash_modulo, rewrite_strings};

const WHAT: &str = "builtins.fetchClosure";

pub fn fetch_closure(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    ev.trace().mark_uncacheable();
    let a = ev.force_attrs(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.fetchClosure",
    )?;
    let mut from_store: Option<String> = None;
    let mut from_path: Option<StorePath> = None;
    let mut to_path: Option<Option<StorePath>> = None;
    let mut input_addressed: Option<bool> = None;
    for attr in a.iter() {
        let name = String::from_utf8_lossy(ev.interner.name(attr.sym)).into_owned();
        let hint =
            format!("while evaluating the attribute '{name}' passed to builtins.fetchClosure");
        let v = attr.cell.get();
        match name.as_str() {
            "fromPath" => from_path = Some(coerce_to_store_path(ev, v, attr.pos, &hint)?),
            "toPath" => {
                let v = ev.force(v)?;
                to_path = Some(match v.view() {
                    Value::String(s) if s.bytes.is_empty() => None,
                    _ => Some(coerce_to_store_path(ev, v, attr.pos, &hint)?),
                });
            }
            "fromStore" => {
                let s = ev.force_string_no_ctx(v, attr.pos, &hint)?;
                from_store = Some(String::from_utf8_lossy(s).into_owned());
            }
            "inputAddressed" => input_addressed = Some(ev.force_bool(v, attr.pos, &hint)?),
            _ => bail!(
                ErrorKind::Eval,
                pos,
                "attribute '{name}' isn't supported in call to 'fetchClosure'"
            ),
        }
    }
    let Some(from_path) = from_path else {
        bail!(
            ErrorKind::Eval,
            pos,
            "attribute 'fromPath' is missing in call to 'fetchClosure'"
        );
    };
    let input_addressed = input_addressed.unwrap_or(false);
    if input_addressed && to_path.is_some() {
        bail!(
            ErrorKind::Eval,
            pos,
            "attribute 'inputAddressed' is set to true, but 'toPath' is also set. Please remove one of them"
        );
    }
    let Some(from_store) = from_store else {
        bail!(
            ErrorKind::Eval,
            pos,
            "attribute 'fromStore' is missing in call to 'fetchClosure'"
        );
    };
    let cache_url = cache_url(&from_store, env::var_os("_NIX_IN_TEST").is_some())
        .map_err(|msg| Error::boxed(ErrorKind::Eval, msg, pos))?;
    let limits = Limits::new(nix_conf(ev.session()))
        .map_err(|e| Error::boxed(ErrorKind::Eval, e.to_string(), pos))?;
    let pure = ev.pure();
    let (conn, store) = ev
        .store
        .require(WHAT)
        .map_err(|msg| Error::boxed(ErrorKind::Unsupported, msg, pos))?;
    let mut f = Fetch {
        conn,
        store,
        cache: BinaryCache::new(cache_url, limits),
    };
    let (out, closure) = f
        .run(&from_path, to_path, input_addressed, pure)
        .map_err(|e| {
            let kind = match e {
                Failure::Unsupported(_) => ErrorKind::Unsupported,
                Failure::Eval(_) => ErrorKind::Eval,
            };
            Error::boxed(kind, e.text(), pos)
        })?;
    // eval.cc:allowClosure: a pure evaluation may read what it fetched
    for p in closure {
        ev.known.lock().entry(p).or_default();
    }
    Ok(store_path_string(ev, &out))
}

/// eval.cc:coerceToStorePath: a string or path naming a store path itself,
/// not a file below one.
fn coerce_to_store_path(
    ev: &mut Evaluator,
    w: Word,
    pos: Pos,
    hint: &str,
) -> EvalResult<StorePath> {
    let mut ctx: Vec<Ctx> = Vec::new();
    let s = ev.coerce_to_string(w, &mut ctx, pos, hint, false, false, true)?;
    let text = String::from_utf8_lossy(&s).into_owned();
    match from_utf8_canon(&s).and_then(|c| StorePath::parse(&c).ok()) {
        Some(sp) => Ok(sp),
        None => {
            let mut e = Error::new(
                ErrorKind::Eval,
                format!("path '{text}' is not in the Nix store"),
                Pos::NONE,
            );
            e.add_trace(pos, hint);
            Err(Box::new(e))
        }
    }
}

fn from_utf8_canon(s: &[u8]) -> Option<String> {
    String::from_utf8(canon_path(s)).ok()
}

/// fetchClosure.cc's check of `fromStore`: an `http://` or `https://` binary
/// cache, `file://` too under Nix's `_NIX_IN_TEST`, and no query parameters.
/// The scheme is what the URL says it is; nothing else is guessed.
fn cache_url(url: &str, in_test: bool) -> result::Result<&str, String> {
    let scheme = url.split_once("://").map(|(s, _)| s);
    if !matches!(scheme, Some("http" | "https")) && !(in_test && scheme == Some("file")) {
        return Err("'fetchClosure' only supports http:// and https:// stores".into());
    }
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let query = query.split('#').next().unwrap_or_default();
    if query.split('&').any(|p| !p.is_empty()) {
        return Err(format!(
            "'fetchClosure' does not support URL query parameters (in '{url}')"
        ));
    }
    Ok(base.split('#').next().unwrap_or(base))
}

enum Failure {
    Eval(String),
    /// What iets cannot do but Nix can: classic mode hands the call to Nix.
    Unsupported(String),
}

impl Failure {
    fn text(&self) -> &str {
        match self {
            Failure::Eval(m) | Failure::Unsupported(m) => m,
        }
    }
}

impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Failure {
        match e {
            StoreError::Unsupported(m) => Failure::Unsupported(m),
            e => Failure::Eval(e.to_string()),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Failure {
        Failure::Eval(io_text(&e))
    }
}

type Result<T> = result::Result<T, Failure>;

/// The daemon's own lines, such as a substituter it retries.
fn said(e: Event) {
    if let Event::Log(l) = e {
        log::line(&String::from_utf8_lossy(&l.line));
    }
}

struct Fetch<'a> {
    conn: &'a mut Connection,
    /// The eval store as the log names it.
    store: String,
    cache: BinaryCache,
}

impl Fetch<'_> {
    /// prim_fetchClosure after its argument checks: the store path the
    /// result names, and its closure when a pure evaluation must be let read it.
    fn run(
        &mut self,
        from: &StorePath,
        to: Option<Option<StorePath>>,
        input_addressed: bool,
        pure: bool,
    ) -> Result<(StorePath, Vec<StorePath>)> {
        let out = match to {
            Some(to) => self.with_rewrite(from, to)?,
            None => {
                if !self.valid(from)? {
                    self.copy_closure(from)?;
                }
                let info = self.local_info(from)?;
                match (self.content_addressed(&info)?, input_addressed) {
                    (false, false) => {
                        return Err(Failure::Eval(format!(
                            "The 'fromPath' value '{from}' is input-addressed, but 'inputAddressed' is set to 'false' (default).\n\n\
                             If you do intend to fetch an input-addressed store path, add\n\n    \
                             inputAddressed = true;\n\n\
                             to the 'fetchClosure' arguments.\n\n\
                             Note that to ensure authenticity input-addressed store paths, users must configure a trusted binary cache public key on their systems. This is not needed for content-addressed paths."
                        )));
                    }
                    (true, true) => {
                        return Err(Failure::Eval(format!(
                            "The store object referred to by 'fromPath' at '{from}' is not input-addressed, but 'inputAddressed' is set to 'true'.\n\n\
                             Remove the 'inputAddressed' attribute (it defaults to 'false') to expect 'fromPath' to be content-addressed"
                        )));
                    }
                    _ => from.clone(),
                }
            }
        };
        let closure = if pure {
            self.local_closure(&out)?
        } else {
            Vec::new()
        };
        Ok((out, closure))
    }

    /// runFetchClosureWithRewrite.
    fn with_rewrite(&mut self, from: &StorePath, to: Option<StorePath>) -> Result<StorePath> {
        let have = match &to {
            Some(to) => self.valid(to)?,
            None => false,
        };
        if !have {
            let rewritten = self.make_content_addressed(from)?;
            match &to {
                Some(to) if *to != rewritten => {
                    return Err(Failure::Eval(format!(
                        "rewriting '{from}' to content-addressed form yielded '{rewritten}', while '{to}' was expected"
                    )));
                }
                None => {
                    return Err(Failure::Eval(format!(
                        "rewriting '{from}' to content-addressed form yielded '{rewritten}'\n\
                         Use this value for the 'toPath' attribute passed to 'fetchClosure'"
                    )));
                }
                Some(_) => {}
            }
        }
        let to = to.expect("a missing toPath failed above");
        let info = self.local_info(&to)?;
        if !self.content_addressed(&info)? {
            return Err(Failure::Eval(format!(
                "The 'toPath' value '{to}' is input-addressed, so it can't possibly be the result of rewriting to a content-addressed path.\n\n\
                 Set 'toPath' to an empty string to make Nix report the correct content-addressed path."
            )));
        }
        Ok(to)
    }

    fn valid(&mut self, p: &StorePath) -> Result<bool> {
        let valid = self.conn.query_valid_paths(&[p.to_string()], &mut said)?;
        Ok(!valid.is_empty())
    }

    fn local_info(&mut self, p: &StorePath) -> Result<PathInfo> {
        self.conn
            .query_path_info(&p.to_string(), &mut said)?
            .ok_or_else(|| Failure::Eval(format!("path '{p}' is not valid")))
    }

    /// path-info.cc:ValidPathInfo::isContentAddressed.
    fn content_addressed(&self, info: &PathInfo) -> Result<bool> {
        if info.ca.is_empty() {
            return Ok(false);
        }
        let refs = parse_paths(&info.references)?;
        let p = &info.path;
        let ca_path = path_from_ca(&info.ca, p.name(), p, &refs).map_err(|e| Failure::Eval(e.0))?;
        if ca_path != *p {
            log::line(&format!(
                "warning: path '{p}' claims to be content-addressed but isn't"
            ));
            return Ok(false);
        }
        Ok(true)
    }

    /// The closure of `p` in the eval store, which the evaluation may read.
    fn local_closure(&mut self, p: &StorePath) -> Result<Vec<StorePath>> {
        let mut seen: BTreeSet<StorePath> = BTreeSet::new();
        let mut queue = vec![p.clone()];
        while let Some(p) = queue.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            let info = self.local_info(&p)?;
            queue.extend(parse_paths(&info.references)?);
        }
        Ok(seen.into_iter().collect())
    }

    /// Store::computeFSClosure on the binary cache.
    fn cache_closure(&mut self, from: &StorePath) -> Result<BTreeMap<StorePath, NarInfo>> {
        let mut infos = BTreeMap::new();
        let mut queue = vec![from.clone()];
        while let Some(p) = queue.pop() {
            if infos.contains_key(&p) {
                continue;
            }
            let Some(ni) = self.cache.nar_info(&p)? else {
                return Err(Failure::Eval(format!("path '{p}' is not valid")));
            };
            queue.extend(parse_paths(&ni.info.references)?);
            infos.insert(p, ni);
        }
        Ok(infos)
    }

    /// store-api.cc:copyClosure from the binary cache into the eval store,
    /// with the signature check on (`CheckSigs`, the default).
    fn copy_closure(&mut self, from: &StorePath) -> Result<()> {
        let infos = self.cache_closure(from)?;
        let all: Vec<String> = infos.keys().map(StorePath::to_string).collect();
        let valid: HashSet<String> = self
            .conn
            .query_valid_paths(&all, &mut said)?
            .into_iter()
            .collect();
        let missing: BTreeMap<StorePath, NarInfo> = infos
            .into_iter()
            .filter(|(p, _)| !valid.contains(&p.to_string()))
            .collect();
        let order = topo_sort(&missing);
        let mut copying = Copying::new(order.len(), self.cache.url(), &self.store);
        let mut nars = Vec::with_capacity(order.len());
        for p in &order {
            let _act = copying.path(&p.to_string());
            nars.push(self.cache.nar(&missing[p])?);
        }
        let n = order.len() as u64;
        let mut body = |w: &mut dyn Write| -> io::Result<()> {
            write_u64(w, n)?;
            for (p, nar) in order.iter().zip(nars.iter_mut()) {
                missing[p].info.write(w)?;
                io::copy(nar, w)?;
            }
            Ok(())
        };
        self.conn
            .add_multiple_to_store(false, &mut body, &mut said)?;
        Ok(())
    }

    /// make-content-addressed.cc:makeContentAddressed: each path of the
    /// closure, references first, with the hashes of its references and of
    /// itself rewritten, added to the eval store as a content-addressed path.
    fn make_content_addressed(&mut self, from: &StorePath) -> Result<StorePath> {
        let infos = self.cache_closure(from)?;
        let mut remap: HashMap<StorePath, StorePath> = HashMap::new();
        for p in topo_sort(&infos) {
            let old = &infos[&p];
            let old_hash = p.hash_part().to_string();
            let mut nar = Vec::new();
            self.cache.nar(old)?.read_to_end(&mut nar)?;
            let mut rewrites = BTreeMap::new();
            let mut others = BTreeSet::new();
            let mut self_ref = false;
            for r in parse_paths(&old.info.references)? {
                if r == p {
                    self_ref = true;
                    continue;
                }
                let to = remap.get(&r).cloned().unwrap_or_else(|| r.clone());
                if to != r {
                    rewrites.insert(r.to_string(), to.to_string());
                }
                others.insert(to);
            }
            let nar = rewrite_strings(nar, &rewrites);
            let modulo = hash_modulo(&nar, &old_hash);
            let new = make_source_path(&modulo, p.name(), &others, self_ref)
                .map_err(|e| Failure::Eval(e.0))?;
            log::msg(log::LVL_INFO as u8, &format!("rewriting '{p}' to '{new}'"));
            let own = BTreeMap::from([(old_hash, new.hash_part().to_string())]);
            let nar = rewrite_strings(nar, &own);
            let mut references: Vec<String> = others.iter().map(StorePath::to_string).collect();
            if self_ref {
                references.push(new.to_string());
                references.sort();
            }
            let info = PathInfo {
                path: new.clone(),
                deriver: String::new(),
                nar_hash: hash_bytes(HashAlgo::Sha256, &nar).to_base16(),
                references,
                registration_time: 0,
                nar_size: nar.len() as u64,
                ultimate: false,
                sigs: Vec::new(),
                ca: format!("fixed:r:sha256:{}", modulo.to_nix32()),
            };
            let mut body = |w: &mut dyn Write| -> io::Result<()> {
                write_u64(w, 1)?;
                info.write(w)?;
                w.write_all(&nar)
            };
            self.conn
                .add_multiple_to_store(false, &mut body, &mut said)?;
            remap.insert(p, new);
        }
        Ok(remap.remove(from).expect("the closure holds its root"))
    }
}

fn parse_paths(paths: &[String]) -> Result<BTreeSet<StorePath>> {
    paths
        .iter()
        .map(|r| StorePath::parse(r).map_err(|e| Failure::Eval(e.0)))
        .collect()
}

/// store-api.cc:topoSortPaths, reversed as both of its callers here reverse
/// it: references first, the depth-first walk in path order that Nix takes,
/// so the log names the paths in Nix's order.
fn topo_sort(infos: &BTreeMap<StorePath, NarInfo>) -> Vec<StorePath> {
    fn visit(
        p: &StorePath,
        infos: &BTreeMap<StorePath, NarInfo>,
        seen: &mut HashSet<StorePath>,
        out: &mut Vec<StorePath>,
    ) {
        if !seen.insert(p.clone()) {
            return;
        }
        let refs: BTreeSet<StorePath> = infos[p]
            .info
            .references
            .iter()
            .filter_map(|r| StorePath::parse(r).ok())
            .collect();
        for r in &refs {
            if r != p && infos.contains_key(r) {
                visit(r, infos, seen, out);
            }
        }
        out.push(p.clone());
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(infos.len());
    for p in infos.keys() {
        visit(p, infos, &mut seen, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_stores() {
        let only = "'fetchClosure' only supports http:// and https:// stores";
        for url in [
            "s3://bucket",
            "ssh://host",
            "ssh-ng://host",
            "daemon",
            "auto",
            "local",
            "/nix/store",
            "file:///tmp/cache",
        ] {
            assert_eq!(cache_url(url, false).unwrap_err(), only, "{url}");
        }
        assert_eq!(
            cache_url("https://cache.nixos.org/", false),
            Ok("https://cache.nixos.org/")
        );
        assert_eq!(cache_url("http://c", false), Ok("http://c"));
        assert_eq!(
            cache_url("file:///tmp/cache", true),
            Ok("file:///tmp/cache")
        );
        assert_eq!(cache_url("s3://bucket", true).unwrap_err(), only);
    }

    #[test]
    fn no_query_parameters() {
        assert_eq!(
            cache_url("https://x?priority=10", false).unwrap_err(),
            "'fetchClosure' does not support URL query parameters (in 'https://x?priority=10')"
        );
        assert_eq!(cache_url("https://x?", false), Ok("https://x"));
    }

    fn ni(path: &str, refs: &[&str]) -> (StorePath, NarInfo) {
        let p = StorePath::parse(path).unwrap();
        let info = PathInfo {
            path: p.clone(),
            deriver: String::new(),
            nar_hash: String::new(),
            references: refs.iter().map(|r| r.to_string()).collect(),
            registration_time: 0,
            nar_size: 1,
            ultimate: false,
            sigs: Vec::new(),
            ca: String::new(),
        };
        let n = NarInfo {
            info,
            url: String::new(),
            compression: String::new(),
        };
        (p, n)
    }

    #[test]
    fn references_come_first_in_nix_order() {
        let a = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
        let b = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b";
        let c = "/nix/store/cccccccccccccccccccccccccccccccc-c";
        let d = "/nix/store/dddddddddddddddddddddddddddddddd-d";
        // d -> c, b; c -> a; b -> a, b
        let infos: BTreeMap<_, _> = [ni(a, &[]), ni(b, &[a, b]), ni(c, &[a]), ni(d, &[b, c])]
            .into_iter()
            .collect();
        let order: Vec<String> = topo_sort(&infos).iter().map(|p| p.to_string()).collect();
        assert_eq!(order, [a, b, c, d]);
        let infos: BTreeMap<_, _> = [ni(d, &[b, c]), ni(c, &[a])].into_iter().collect();
        let order: Vec<String> = topo_sort(&infos).iter().map(|p| p.to_string()).collect();
        assert_eq!(order, [c, d]);
    }
}
