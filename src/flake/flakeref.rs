//! Flake references (flakeref.cc, the github, gitlab, git, path, tarball,
//! file and indirect schemes' inputFromURL and toURL) for the builtins, the
//! command line and fetchTree's messages. Valid input gives Nix's output;
//! checks that only refuse invalid input are left out (docs/reference/divergences.md).

use std::collections::BTreeMap;
use std::{fmt, mem};

use crate::error::{Error, Result as R};
use crate::eval::{ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::runtime::{Value, Word};
use crate::source::{AttrVal, Attrs, str_attr};
use crate::store::hash::{Hash, HashAlgo};

use super::{eval_error, is_indirect};

fn fail<T>(m: impl Into<String>) -> R<T> {
    Err(Error::Flake(m.into()))
}

fn unsupported<T>(w: impl fmt::Display) -> R<T> {
    Err(Error::Unsupported(format!("{w} is not supported")))
}

#[derive(Default)]
struct Url {
    scheme: String,
    /// `user@host:port` as url.cc renders it.
    authority: Option<String>,
    path: Vec<String>,
    query: BTreeMap<String, String>,
    fragment: String,
}

fn url(scheme: &str, path: Vec<String>) -> Url {
    let mut u = Url::default();
    (u.scheme, u.path) = (scheme.to_string(), path);
    u
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path: Vec<String> = self.path.iter().map(|s| encode(s, ":@")).collect();
        let a = self.authority.as_deref().map(|a| format!("//{a}"));
        write!(
            f,
            "{}:{}{}",
            self.scheme,
            a.unwrap_or_default(),
            path.join("/")
        )?;
        let q = self
            .query
            .iter()
            .map(|(k, v)| encode(k, ":@/?") + "=" + &encode(v, ":@/?"));
        let q: Vec<String> = q.collect();
        if !q.is_empty() {
            write!(f, "?{}", q.join("&"))?;
        }
        if !self.fragment.is_empty() {
            write!(f, "#{}", encode(&self.fragment, ""))?;
        }
        Ok(())
    }
}

fn unreserved(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-._~".contains(&c)
}

/// url.cc:percentEncode.
fn encode(s: &str, keep: &str) -> String {
    let keep = |c: u8| unreserved(c) || keep.as_bytes().contains(&c);
    let enc = s.bytes().map(|c| match keep(c) {
        true => (c as char).to_string(),
        false => format!("%{c:02X}"),
    });
    enc.collect()
}

fn decode(s: &str) -> R<String> {
    let mut parts = s.split('%');
    let mut out = parts.next().unwrap_or("").as_bytes().to_vec();
    for p in parts {
        let Some(c) = p.get(..2).and_then(|h| u8::from_str_radix(h, 16).ok()) else {
            return fail(format!("invalid percent-encoding in '{s}'"));
        };
        out.push(c);
        out.extend_from_slice(&p.as_bytes()[2..]);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// url.cc:decodeQuery: a parameter without `=` goes, the first of two wins.
fn decode_query(q: &str) -> R<BTreeMap<String, String>> {
    let mut res = BTreeMap::new();
    for (k, v) in q.split('&').filter_map(|p| p.split_once('=')) {
        res.entry(decode(k)?).or_insert(decode(v)?);
    }
    Ok(res)
}

/// url.cc:parseUrlScheme: `(application+)?transport`.
fn split_scheme(s: &str) -> (Option<&str>, &str) {
    s.split_once('+').map_or((None, s), |(a, t)| (Some(a), t))
}

fn is_scheme(s: &str) -> bool {
    let ok = |c: u8| c.is_ascii_alphanumeric() || b"+-.".contains(&c);
    s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.bytes().all(ok)
}

fn listed(list: &str, k: &str) -> bool {
    list.split(' ').any(|x| x == k)
}

/// url.cc:parseURL, without boost.url's character checks.
fn parse_url(s: &str) -> R<Url> {
    let Some((scheme, rest)) = s.split_once(':').filter(|(sc, _)| is_scheme(sc)) else {
        return unsupported(format_args!("URL '{s}'"));
    };
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, mut path) = match rest.strip_prefix("//") {
        Some(r) => {
            let (a, p) = r.split_at(r.find('/').unwrap_or(r.len()));
            (Some(authority(a)?), p)
        }
        None => (None, rest),
    };
    if path.is_empty() && split_scheme(scheme).1 == "file" {
        path = "/";
    }
    let mut u = url(scheme, path.split('/').map(decode).collect::<R<_>>()?);
    (u.authority, u.query, u.fragment) = (authority, decode_query(query)?, decode(fragment)?);
    Ok(u)
}

/// An authority of unreserved characters, with the port as a number.
fn authority(a: &str) -> R<String> {
    let (user, hp) = a.rsplit_once('@').map_or((None, a), |(u, h)| (Some(u), h));
    let (host, port) = hp.split_once(':').unwrap_or((hp, ""));
    let n = port.parse::<u16>().unwrap_or(0);
    let plain = a.bytes().all(|c| unreserved(c) || c == b':' || c == b'@');
    if !plain || (n == 0) != port.is_empty() {
        return unsupported(format_args!("URL authority '{a}'"));
    }
    let port = Some(n).filter(|n| *n > 0).map(|n| format!(":{n}"));
    Ok(user.map_or(String::new(), |u| format!("{u}@")) + host + &port.unwrap_or_default())
}

/// Runs of `/` as one, what pathToUrlPath then urlPathToPath leave.
fn collapse(p: &str) -> String {
    let mut p = p.to_string();
    while p.contains("//") {
        p = p.replace("//", "/");
    }
    p
}

pub(crate) fn is_rev(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

fn st(s: &str) -> AttrVal {
    AttrVal::Str(s.to_string())
}

fn put(m: &mut BTreeMap<String, String>, k: &str, v: Option<impl Into<String>>) {
    if let Some(v) = v {
        m.insert(k.to_string(), v.into());
    }
}

/// A query parameter as an attribute; a `revCount` that is no number goes.
fn attr(k: String, v: &str) -> Option<(String, AttrVal)> {
    match k.as_str() {
        "revCount" | "lastModified" => Some((k, AttrVal::Int(v.parse().ok()?))),
        _ => Some((k, st(v))),
    }
}

/// fetchers.cc:Input::getRev, as 40 hex digits.
fn rev(a: &Attrs) -> R<Option<String>> {
    let parse = |s| Hash::parse_any(s, None).or_else(|_| Hash::parse_any(s, Some(HashAlgo::Sha1)));
    Ok(str_attr(a, "rev")
        .map(parse)
        .transpose()?
        .map(|h| h.to_base16()))
}

/// fetchers.cc:Input::getNarHash: an empty one is the zero hash.
fn nar_hash(a: &Attrs) -> Option<String> {
    let zero = || format!("sha256-{}=", "A".repeat(43));
    str_attr(a, "narHash").map(|h| if h.is_empty() { zero() } else { h.into() })
}

fn path_input(path: String, query: BTreeMap<String, String>) -> Attrs {
    let mut a = Attrs::from([("type".into(), st("path"))]);
    a.insert("path".into(), AttrVal::Str(path));
    a.extend(query.into_iter().filter_map(|(k, v)| attr(k, &v)));
    a
}

fn segments(path: &[String]) -> Vec<&str> {
    let p = path.iter().map(String::as_str);
    p.filter(|s| !s.is_empty()).collect()
}

/// indirect.cc:inputFromURL: `ID[/REF][/REV]`.
fn indirect(p: &[&str], shown: &str) -> R<Attrs> {
    let (r#ref, rev) = match p {
        [_] => (None, None),
        [_, x] if is_rev(x) => (None, Some(x)),
        [_, x] => (Some(x), None),
        [_, x, y] => (Some(x), Some(y)),
        _ => return fail(format!("flake URL '{shown}' is not ID[/REF][/REV]")),
    };
    let mut a = Attrs::from([("type".into(), st("indirect")), ("id".into(), st(p[0]))]);
    a.extend(r#ref.map(|r| ("ref".into(), st(r))));
    a.extend(rev.map(|r| ("rev".into(), AttrVal::Str(r.to_lowercase()))));
    Ok(a)
}

const CURL: &str = "url narHash name unpack rev revCount lastModified";
const GIT_FLAGS: &str = "shallow submodules lfs exportIgnore allRefs verifyCommit";

/// fetchers.cc:Input::fromURL with requireTree.
fn input_from_url(mut url: Url) -> R<Attrs> {
    let scheme = url.scheme.clone();
    let (app, transport) = split_scheme(&scheme);
    let mut a = Attrs::new();
    let curl = ["file", "http", "https"].contains(&transport);
    match (app, scheme.as_str()) {
        (None | Some("file" | "tarball"), _) if curl => {
            // tarball.cc:CurlInputScheme::inputFromURL
            let ty = ["tarball", "file"][usize::from(app == Some("file"))];
            a.insert("type".into(), st(ty));
            for k in ["narHash", "rev", "revCount", "lastModified"] {
                a.extend(url.query.get(k).and_then(|v| attr(k.into(), v)));
            }
            url.query.retain(|k, _| !listed(CURL, k));
        }
        (Some("git"), _) | (_, "git") => {
            // git.cc:inputFromURL, then fixGitURL drops the `git+`
            a.insert("type".into(), st("git"));
            for (k, v) in mem::take(&mut url.query) {
                if listed(GIT_FLAGS, &k) {
                    a.insert(k, AttrVal::Bool(v == "1"));
                } else if listed("rev ref keytype publicKey publicKeys", &k) {
                    a.insert(k, AttrVal::Str(v));
                } else {
                    url.query.insert(k, v);
                }
            }
        }
        (_, "github" | "gitlab") => {
            // github.cc:GitArchiveInputScheme::inputFromURL
            let p = segments(&url.path);
            if p.len() < 2 {
                return fail(format!("URL '{url}' is invalid"));
            }
            a.insert("type".into(), st(&scheme));
            a.extend([("owner".into(), st(p[0])), ("repo".into(), st(p[1]))]);
            if p.len() > 2 {
                let k = ["ref", "rev"][usize::from(p.len() == 3 && is_rev(p[2]))];
                a.insert(k.into(), AttrVal::Str(p[2..].join("/")));
            }
            for (k, v) in url
                .query
                .iter()
                .filter(|(k, _)| listed("rev ref host narHash", k))
            {
                if a.contains_key(k) {
                    let what = if k == "rev" {
                        "commit hashes"
                    } else {
                        "branch/tag names"
                    };
                    return fail(format!("URL '{url}' contains multiple {what}"));
                }
                a.insert(k.clone(), st(v));
            }
            // github.cc:inputFromAttrs
            if let (Some(AttrVal::Str(r)), Some(AttrVal::Str(v))) = (a.get("ref"), a.get("rev")) {
                return fail(format!(
                    "input '{url}' contains both a commit hash ('{v}') and a branch/tag name ('{r}')"
                ));
            }
            return Ok(a);
        }
        (_, "flake") => return indirect(&segments(&url.path), &url.to_string()),
        (_, "path") => {
            let p = collapse(&url.path.join("/"));
            let p = if p.is_empty() { "/".into() } else { p };
            return Ok(path_input(p, url.query));
        }
        _ => return unsupported(format_args!("flake reference scheme '{scheme}'")),
    }
    url.scheme = transport.to_string();
    a.insert("url".into(), AttrVal::Str(url.to_string()));
    Ok(a)
}

/// The schemes' toURL.
fn to_url(a: &Attrs) -> R<Url> {
    let ty = str_attr(a, "type").unwrap_or("");
    let get = |k: &str| match str_attr(a, k) {
        Some(v) => Ok(v.to_string()),
        None => fail(format!("input attribute '{k}' is missing")),
    };
    let r#ref = str_attr(a, "ref");
    let mut u = match ty {
        "indirect" => url("flake", vec![get("id")?]),
        "github" | "gitlab" => {
            // github.cc:inputFromAttrs; written as a path, the two would
            // read back as one ref
            if let (Some(r), Some(v)) = (r#ref, str_attr(a, "rev")) {
                return fail(format!(
                    "input contains both a commit hash ('{v}') and a branch/tag name ('{r}')"
                ));
            }
            url(ty, vec![get("owner")?, get("repo")?])
        }
        "path" => {
            let mut u = url("path", get("path")?.split('/').map(String::from).collect());
            for (k, v) in a.iter().filter(|(k, _)| !listed("path type __final", k)) {
                let v = match v {
                    AttrVal::Str(s) => s.clone(),
                    AttrVal::Int(n) => n.to_string(),
                    AttrVal::Bool(b) => u8::from(*b).to_string(),
                };
                u.query.insert(k.clone(), v);
            }
            return Ok(u);
        }
        "git" | "tarball" | "file" => parse_url(&get("url")?)?,
        _ => return unsupported(format_args!("flake reference type '{ty}'")),
    };
    if ty == "git" {
        let t = u.scheme.strip_prefix("git+").unwrap_or(&u.scheme);
        u.scheme = ["git+", ""][usize::from(t == "git")].to_string() + t;
        put(&mut u.query, "ref", r#ref);
        put(&mut u.query, "rev", rev(a)?);
        for k in "shallow lfs submodules exportIgnore verifyCommit".split(' ') {
            if a.get(k) == Some(&AttrVal::Bool(true)) {
                u.query.insert(k.into(), "1".into());
            }
        }
        return Ok(u);
    }
    if ty != "tarball" && ty != "file" {
        u.path
            .extend(r#ref.map(String::from).into_iter().chain(rev(a)?));
    }
    if ty != "indirect" {
        put(&mut u.query, "narHash", nar_hash(a));
    }
    if ty.starts_with("git") {
        put(&mut u.query, "host", str_attr(a, "host"));
    }
    Ok(u)
}

/// url.cc:fixGitURL: a path becomes a `file:` URL and `git+` goes. An
/// SCP-like `user@host:path` is unsupported.
pub(crate) fn fix_git_url(s: &str) -> R<String> {
    let before_slash = &s[..s.find('/').unwrap_or(s.len())];
    let scp = before_slash
        .match_indices('@')
        .any(|(i, _)| s[i + 1..].contains(':'));
    if !s.starts_with('/') && scp {
        return unsupported(format_args!("SCP-like Git URL '{s}'"));
    }
    if !s.starts_with("file:") && !s.starts_with("git+file:") && !s.contains("://") {
        let path: Vec<String> = s.split('/').map(String::from).collect();
        if path[0].contains(':') {
            return fail(format!(
                "SCP-like URL '{s}' is not supported; use SSH URL syntax instead (ssh://...)"
            ));
        }
        let mut u = url("file", path);
        u.authority = s.starts_with('/').then(String::new);
        return Ok(u.to_string());
    }
    let mut u = parse_url(s)?;
    if let (Some("git"), t) = split_scheme(&u.scheme) {
        u.scheme = t.to_string();
    }
    Ok(u.to_string())
}

/// The path of a `file:` URL, as git.cc:getRepoInfo reads it: the URL's
/// path, whatever the authority, so `file://host/p` is `/p` on this machine.
pub(crate) fn file_url_path(s: &str) -> Option<String> {
    let u = parse_url(s).ok()?;
    (u.scheme == "file").then(|| u.path.join("/"))
}

/// A parsed flake reference; `bare` is a path without a scheme.
pub struct Parsed {
    pub input: Attrs,
    pub dir: String,
    pub fragment: String,
    pub bare: bool,
}

/// flakeref.cc:parseFlakeRefWithFragment without a base directory.
pub fn parse(s: &str) -> R<Parsed> {
    let (loc, frag) = s.split_once('#').unwrap_or((s, ""));
    let id = !loc.starts_with("flake:") && is_indirect(loc);
    let bare = !id && !s.split_once(':').is_some_and(|(sc, _)| is_scheme(sc));
    let (input, dir, fragment) = if id {
        let path: Vec<&str> = loc.split('/').filter(|s| !s.is_empty()).collect();
        (indirect(&path, loc)?, String::new(), decode(frag)?)
    } else if bare {
        let (path, query) = loc.split_once('?').unwrap_or((loc, ""));
        let mut query = decode_query(query)?;
        let dir = query.remove("dir").unwrap_or_default();
        (path_input(collapse(path), query), dir, decode(frag)?)
    } else {
        let mut url = parse_url(s)?;
        let dir = url.query.remove("dir").unwrap_or_default();
        let fragment = mem::take(&mut url.fragment);
        (input_from_url(url)?, dir, fragment)
    };
    Ok(Parsed {
        input,
        dir,
        fragment,
        bare,
    })
}

/// flake-primops.cc: no base directory, so a relative path is an error.
pub fn parse_absolute(s: &str) -> R<Parsed> {
    let p = parse(s)?;
    if p.bare && !str_attr(&p.input, "path").is_some_and(|p| p.starts_with('/')) {
        return fail(format!("flake reference '{s}' is not an absolute path"));
    }
    if !p.fragment.is_empty() {
        let m = format!(
            "unexpected fragment '{}' in flake reference '{s}'",
            p.fragment
        );
        return fail(m);
    }
    Ok(p)
}

/// flakeref.cc:FlakeRef::to_string, also fetchers.cc:Input::to_string.
pub(crate) fn attrs_to_string(mut a: Attrs) -> R<String> {
    let dir = a.remove("dir").filter(|d| *d != st(""));
    let mut url = to_url(&a)?;
    if let Some(AttrVal::Str(d)) = dir {
        url.query.entry("dir".into()).or_insert(d);
    }
    Ok(url.to_string())
}

pub fn parse_flake_ref(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let what = "while evaluating the argument passed to builtins.parseFlakeRef";
    let s = String::from_utf8_lossy(ev.force_string_no_ctx(args[0], pos, what)?).into_owned();
    let mut p = parse_absolute(&s).map_err(|e| eval_error(e, pos))?;
    if !p.dir.is_empty() {
        p.input.insert("dir".into(), AttrVal::Str(p.dir));
    }
    let mut entries = Vec::with_capacity(p.input.len());
    for (k, v) in p.input {
        let w = match v {
            AttrVal::Str(s) => ev.mk_string(s.as_bytes()),
            AttrVal::Int(n) => ev.mk_int(n as i64),
            AttrVal::Bool(b) => Word::from_bool(b),
        };
        entries.push((ev.sym(k.as_bytes()), pos, w));
    }
    ev.mk_attrs(&mut entries, pos)
}

pub fn flake_ref_to_string(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let what = "while evaluating the argument passed to builtins.flakeRefToString";
    let a = ev.force_attrs(args[0], pos, what)?;
    let mut attrs = Attrs::new();
    for attr in a.iter() {
        let name = String::from_utf8_lossy(ev.interner.name(attr.sym)).into_owned();
        let v = ev.force(attr.cell.get())?;
        let val = match v.view() {
            Value::Int(n) if n >= 0 => AttrVal::Int(n as u64),
            Value::Bool(b) => AttrVal::Bool(b),
            Value::String(s) => AttrVal::Str(String::from_utf8_lossy(s.bytes).into_owned()),
            _ => bail!(
                ErrorKind::Eval,
                pos,
                "flake ref attribute '{name}' has a bad type"
            ),
        };
        attrs.insert(name, val);
    }
    let s = attrs_to_string(attrs).map_err(|e| eval_error(e, pos))?;
    Ok(ev.mk_string(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    impl Parsed {
        fn get(&self, k: &str) -> Option<&str> {
            str_attr(&self.input, k)
        }
    }

    fn attrs(pairs: &[(&str, AttrVal)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn shown(s: &str) -> String {
        let p = parse_absolute(s).unwrap_or_else(|e| panic!("{s}: {e:?}"));
        let mut a = p.input;
        if !p.dir.is_empty() {
            a.insert("dir".into(), AttrVal::Str(p.dir));
        }
        attrs_to_string(a).unwrap_or_else(|e| panic!("{s}: {e:?}"))
    }

    const REV: &str = "0123456789012345678901234567890123456789";

    #[test]
    fn parses_the_kept_schemes() {
        let p = parse_absolute("github:NixOS/nixpkgs/nixos-24.05?dir=lib").unwrap();
        assert_eq!((p.dir.as_str(), p.get("ref")), ("lib", Some("nixos-24.05")));
        assert_eq!(
            parse_absolute(&format!("github:a/b/{REV}"))
                .unwrap()
                .get("rev"),
            Some(REV)
        );
        assert_eq!(
            parse_absolute("github:a/b/x/y").unwrap().get("ref"),
            Some("x/y")
        );
        let p = parse_absolute("git+ssh://git@x.org:022/a.git?ref=main&shallow=1").unwrap();
        assert_eq!(p.get("url"), Some("ssh://git@x.org:22/a.git"));
        assert_eq!(p.input.get("shallow"), Some(&AttrVal::Bool(true)));
        let p = parse_absolute("https://x.org/a.tar.gz?narHash=x&lastModified=5&y=1").unwrap();
        assert_eq!(
            (p.get("type"), p.get("url")),
            (Some("tarball"), Some("https://x.org/a.tar.gz?y=1"))
        );
        assert_eq!(p.input.get("lastModified"), Some(&AttrVal::Int(5)));
        assert_eq!(
            parse_absolute("file+https://x.org/a").unwrap().get("type"),
            Some("file")
        );
        let p = parse_absolute(&format!("nixpkgs/release/{REV}")).unwrap();
        assert_eq!(
            (p.get("id"), p.get("ref")),
            (Some("nixpkgs"), Some("release"))
        );
        assert_eq!(
            parse_absolute("//a//b/").unwrap().get("path"),
            Some("/a/b/")
        );
        assert_eq!(
            parse_absolute("path:/a%20b").unwrap().get("path"),
            Some("/a b")
        );
        assert_eq!(parse_absolute("path:").unwrap().get("path"), Some("/"));
        let p = parse("./x?dir=d#a.b").unwrap();
        assert!(p.bare);
        assert_eq!(
            (p.get("path"), p.dir.as_str(), p.fragment.as_str()),
            (Some("./x"), "d", "a.b")
        );
        let p = parse("nixpkgs#hello").unwrap();
        assert_eq!(
            (p.get("type"), p.fragment.as_str()),
            (Some("indirect"), "hello")
        );
    }

    /// Nix refuses these; keeping one of the two would fetch another commit.
    #[test]
    fn refuses_a_github_ref_named_twice() {
        for s in [
            "github:a/b/c?ref=d",
            "gitlab:a/b/c?ref=d",
            &format!("github:a/b/{REV}?rev={REV}"),
            "github:a/b?ref=d&rev=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "github:a/b/c?rev=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(parse_absolute(s).is_err(), "{s}");
        }
    }

    #[rstest]
    #[case(
        "github:NixOS/nixpkgs/23.05?dir=lib",
        "github:NixOS/nixpkgs/23.05?dir=lib"
    )]
    #[case(
        "github:a/b?rev=ABCDEF0123456789012345678901234567890123",
        "github:a/b/abcdef0123456789012345678901234567890123"
    )]
    #[case("gitlab:a/b?host=x.org&ref=v1", "gitlab:a/b/v1?host=x.org")]
    #[case("nixpkgs/x", "flake:nixpkgs/x")]
    #[case("/a b/c?rev=x", "path:/a%20b/c?rev=x")]
    #[case(
        "git+https://x.org/a?ref=main&foo=1&shallow=0",
        "git+https://x.org/a?foo=1&ref=main"
    )]
    #[case("git://x.org/a", "git://x.org/a")]
    #[case("git+file:///tmp/x?dir=d", "git+file:///tmp/x?dir=d")]
    #[case("https://u:p@x.org:80/a%2f?b=c+d", "https://u:p@x.org:80/a%2F?b=c%2Bd")]
    #[case("tarball+http://x.org/a?b c=1", "http://x.org/a?b%20c=1")]
    fn round_trips(#[case] s: &str, #[case] want: &str) {
        assert_eq!(shown(s), want);
    }

    #[rstest]
    #[case("/a b/c", "file:///a%20b/c")]
    #[case("/a b/?#%.git", "file:///a%20b/%3F%23%25.git")]
    #[case("rel/x", "file:rel/x")]
    #[case("file:///a/b", "file:///a/b")]
    #[case("git+file:///a/b", "file:///a/b")]
    #[case("git+https://x.org/a.git", "https://x.org/a.git")]
    #[case("ssh://git@x.org/a", "ssh://git@x.org/a")]
    fn git_urls_are_fixed_like_nix(#[case] url: &str, #[case] want: &str) {
        assert_eq!(fix_git_url(url).unwrap(), want);
    }

    #[test]
    fn git_urls_nix_refuses_are_refused() {
        assert!(matches!(
            fix_git_url("git@github.com:a/b.git"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(fix_git_url("x.org:a/b"), Err(Error::Flake(_))));
    }

    #[test]
    fn a_file_url_is_a_local_path() {
        assert_eq!(file_url_path("file:///a%20b/c").as_deref(), Some("/a b/c"));
        assert_eq!(file_url_path("file:rel/x").as_deref(), Some("rel/x"));
        assert_eq!(file_url_path("https://x.org/a"), None);
        // git.cc reads the path and ignores the authority
        assert_eq!(file_url_path("file://host/a").as_deref(), Some("/a"));
        assert_eq!(file_url_path("file://localhost/a").as_deref(), Some("/a"));
    }

    #[test]
    fn to_string_from_attrs() {
        let s = |pairs: &[(&str, AttrVal)]| attrs_to_string(attrs(pairs));
        let git = |url: &str| s(&[("type", st("git")), ("url", st(url))]);
        assert_eq!(git("file:///tmp/x").unwrap(), "git+file:///tmp/x");
        assert_eq!(git("git+https://x.org/a").unwrap(), "git+https://x.org/a");
        assert_eq!(git("git://x.org/a").unwrap(), "git://x.org/a");
        for url in ["/tmp/x", "git@github.com:a/b.git"] {
            assert!(matches!(git(url), Err(Error::Unsupported(_))), "{url}");
        }
        let (nar, url) = (("narHash", st("")), ("url", st("https://x.org/a")));
        assert_eq!(
            s(&[("type", st("tarball")), url, nar, ("name", st("n"))]).unwrap(),
            "https://x.org/a?narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D"
        );
        assert_eq!(
            s(&[("type", st("path")), ("path", st("rel/x"))]).unwrap(),
            "path:rel/x"
        );
    }

    #[rstest]
    #[case::relative("./foo")]
    #[case::github_no_repo("github:a")]
    #[case::github_fragment("github:a/b#x")]
    #[case::indirect_too_deep("nixpkgs/a/b/c")]
    #[case::bad_escape("/a#%zz")]
    fn a_bad_ref_is_an_error(#[case] s: &str) {
        assert!(matches!(parse_absolute(s), Err(Error::Flake(_))));
    }

    #[rstest]
    #[case("foo:bar")]
    #[case("sourcehut:~a/b")]
    #[case("hg+https://x.org/a")]
    #[case("https://[::1]/a")]
    #[case("https://x%41.org/a")]
    fn an_unsupported_ref_says_so(#[case] s: &str) {
        assert!(matches!(parse_absolute(s), Err(Error::Unsupported(_))));
    }

    #[test]
    fn attrs_that_cannot_be_a_ref() {
        let hg = attrs(&[("type", st("hg"))]);
        assert!(matches!(attrs_to_string(hg), Err(Error::Unsupported(_))));
        let gh = attrs(&[("type", st("github")), ("owner", st("a"))]);
        assert!(matches!(attrs_to_string(gh), Err(Error::Flake(_))));
    }
}
