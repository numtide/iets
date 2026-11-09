//! Git LFS (git-lfs-fetch.cc). A file whose `filter` attribute is `lfs`
//! holds a pointer, and the batch API names where its contents are. Each
//! object is cached under `<cache root>/lfs/` by its sha256.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, str, thread};

use serde_json::{Value, json};

use crate::cachefs;
use crate::fetch::git;
use crate::fetch::http::{self, Limits};
use crate::flake::flakeref;
use crate::log;
use crate::remote::worker::shell_split;
use crate::session::Session;
use crate::source::nix_conf;
use crate::source::scm::Entry;
use crate::store::hash::{HashAlgo, hash_bytes};
use crate::sys;

#[derive(Clone, PartialEq, Debug)]
struct Pointer {
    oid: String,
    size: u64,
}

/// Where the batch API says an object is, and the headers it wants.
struct Download {
    pointer: Pointer,
    href: String,
    headers: Vec<(String, String)>,
}

/// git-lfs-fetch.cc:parseLfsPointer, for spec v1.
fn parse_pointer(content: &[u8]) -> Option<Pointer> {
    let text = str::from_utf8(content).ok()?;
    if !text.starts_with("version https://git-lfs.github.com/spec/v1") {
        return None;
    }
    let (mut oid, mut size) = ("", "");
    for line in text.lines() {
        if let Some(o) = line.strip_prefix("oid sha256:") {
            oid = o;
        } else if let Some(s) = line.strip_prefix("size ") {
            size = s;
        }
    }
    let hex = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let digits = !size.is_empty() && size.bytes().all(|b| b.is_ascii_digit());
    match hex(oid) && digits {
        true => Some(Pointer {
            oid: oid.to_ascii_lowercase(),
            size: size.parse().ok()?,
        }),
        false => None,
    }
}

/// The paths among `paths` that `rev`'s attributes give `filter=lfs`.
fn lfs_paths(repo: &Path, rev: &str, paths: &[&[u8]]) -> Result<Vec<Vec<u8>>, String> {
    let source = format!("--source={rev}");
    let mut child = Command::new(sys::program(OsStr::new("git")))
        .arg("-C")
        .arg(repo)
        .args(["check-attr", "-z", "--stdin", &source, "filter"])
        .env("GIT_ATTR_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("iets: cannot run git: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input: Vec<u8> = paths.iter().flat_map(|p| [*p, b"\0"].concat()).collect();
    let writer = thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if !out.status.success() {
        return Err(format!(
            "cannot get git-lfs attribute: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let fields: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
    Ok(fields
        .chunks(3)
        .filter(|f| f.len() == 3 && f[2] == b"lfs")
        .map(|f| f[0].to_vec())
        .collect())
}

/// The batch API's base and the `Authorization` header it wants, as git-lfs
/// finds them: `lfs.url` from git's config, then from the commit's
/// `.lfsconfig`, then the origin URL with `.git/info/lfs`.
fn api(repo: &Path, rev: &str) -> Result<(String, Option<String>), String> {
    let lfsconfig = format!("{rev}:.lfsconfig");
    let explicit = [
        &["config", "--get", "lfs.url"][..],
        &["config", "--blob", &lfsconfig, "--get", "lfs.url"],
    ]
    .iter()
    .find_map(|args| match git::git(repo, args) {
        (true, u) if !u.is_empty() => Some(u),
        _ => None,
    });
    let url = explicit.clone().unwrap_or_else(|| git::origin_url(repo));
    let url = flakeref::fix_git_url(&scp_to_ssh(&url)).map_err(|e| e.to_string())?;
    if let Some(rest) = url.strip_prefix("ssh://") {
        return authenticate(rest);
    }
    if explicit.is_some() {
        return Ok((url.trim_end_matches('/').to_string(), None));
    }
    let (base, query) = url.split_once('?').unwrap_or((&url, ""));
    let base = base.trim_end_matches('/');
    let path_starts = base.find("://").map_or(0, |i| i + 3);
    let base = match base[path_starts..].contains('/') {
        true if base.ends_with(".git") => base.to_string(),
        true => format!("{base}.git"),
        false => format!("{base}/.git"),
    };
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    Ok((format!("{base}/info/lfs{query}"), None))
}

/// `user@host:path` as `ssh://user@host/path`.
fn scp_to_ssh(url: &str) -> String {
    match url.split_once(':') {
        Some((host, path)) if !url.contains("://") && !git::is_local(url) => {
            format!("ssh://{host}/{path}")
        }
        _ => url.to_string(),
    }
}

/// `ssh HOST git-lfs-authenticate PATH download`, for an `ssh://` remote.
fn authenticate(rest: &str) -> Result<(String, Option<String>), String> {
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(p)),
        _ => (authority, None),
    };
    if host.starts_with('-') {
        return Err(format!("refusing ssh host '{host}'"));
    }
    let mut args: Vec<String> = Vec::new();
    if let Ok(opts) = env::var("NIX_SSHOPTS") {
        args.extend(shell_split(&opts).map_err(|e| e.to_string())?);
    }
    if let Some(p) = port {
        args.push(format!("-p{p}"));
    }
    let path = format!("/{path}");
    args.extend([
        "--".into(),
        host.to_string(),
        "git-lfs-authenticate".into(),
        path,
        "download".into(),
    ]);
    let out = Command::new(sys::program(OsStr::new("ssh")))
        .args(&args)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("iets: cannot run ssh: {e}"))?;
    if out.stdout.is_empty() {
        return Err(format!(
            "git-lfs-authenticate: no output (cmd: 'ssh {}')",
            args.join(" ")
        ));
    }
    let v: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("git-lfs-authenticate: {e}"))?;
    let href = v["href"]
        .as_str()
        .ok_or("no href in git-lfs-authenticate response")?;
    let auth = v["header"]["Authorization"]
        .as_str()
        .ok_or("no Authorization in git-lfs-authenticate response")?;
    Ok((href.to_string(), Some(auth.to_string())))
}

/// Where each object is, with its headers, from one batch request.
fn batch(
    (endpoint, auth): &(String, Option<String>),
    pointers: &[Pointer],
    limits: Limits,
) -> Result<Vec<Download>, String> {
    let url = match endpoint.split_once('?') {
        Some((path, query)) => format!("{path}/objects/batch?{query}"),
        None => format!("{endpoint}/objects/batch"),
    };
    let mut headers = vec![
        ("Content-Type".into(), "application/vnd.git-lfs+json".into()),
        ("Accept".into(), "application/vnd.git-lfs+json".into()),
    ];
    headers.extend(auth.clone().map(|a| ("Authorization".to_string(), a)));
    let objects: Vec<Value> = pointers
        .iter()
        .map(|p| json!({ "oid": p.oid, "size": p.size }))
        .collect();
    let body = json!({ "operation": "download", "objects": objects }).to_string();
    let mut out = Vec::new();
    http::post(&url, &headers, body.into_bytes(), limits, &mut out).map_err(|e| e.to_string())?;
    let resp: Value =
        serde_json::from_slice(&out).map_err(|e| format!("response did not parse as json: {e}"))?;
    let Some(objects) = resp["objects"].as_array() else {
        return Err("response does not contain 'objects'".into());
    };
    let mut found = Vec::new();
    for p in pointers {
        let o = objects
            .iter()
            .find(|o| o["oid"].as_str() == Some(&p.oid))
            .ok_or_else(|| format!("the LFS server did not return object {}", p.oid))?;
        if let Some(msg) = o["error"]["message"].as_str() {
            return Err(format!("the LFS server refused object {}: {msg}", p.oid));
        }
        if o["size"].as_u64() != Some(p.size) {
            return Err(format!(
                "LFS server returned mismatched size for object {}",
                p.oid
            ));
        }
        let download = &o["actions"]["download"];
        let href = download["href"]
            .as_str()
            .ok_or_else(|| format!("the LFS server gave no download for object {}", p.oid))?;
        let headers = download["header"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect();
        found.push(Download {
            pointer: p.clone(),
            href: href.to_string(),
            headers,
        });
    }
    Ok(found)
}

/// Downloads an object into `cache`, checked against its pointer.
fn download(cache: &Path, d: &Download, limits: Limits) -> Result<(), String> {
    let (p, href) = (&d.pointer, &d.href);
    let mut bytes = Vec::new();
    http::get(href, &d.headers, limits, false, &mut bytes).map_err(|e| {
        let e = e.to_string();
        // a presigned query is a credential
        match href.split_once('?') {
            Some((_, query)) => e.replace(query, "…"),
            None => e,
        }
    })?;
    if bytes.len() as u64 != p.size {
        return Err(format!(
            "size mismatch for LFS object {}: expected {} but got {}",
            p.oid,
            p.size,
            bytes.len()
        ));
    }
    let got = hash_bytes(HashAlgo::Sha256, &bytes).to_base16();
    if got != p.oid {
        return Err(format!(
            "hash mismatch for LFS object {}: got sha256:{got}",
            p.oid
        ));
    }
    cachefs::write_atomic(cache, &p.oid, &[&bytes]).map_err(|e| e.to_string())
}

/// Replaces the pointer in each LFS file of `rev`'s `entries` by the
/// object it names.
pub fn smudge(repo: &Path, rev: &str, entries: &mut [(Vec<u8>, Entry)]) -> Result<(), String> {
    let files: Vec<&[u8]> = entries
        .iter()
        .filter(|(_, e)| matches!(e, Entry::File { .. }))
        .map(|(p, _)| &p[..])
        .collect();
    let marked: HashSet<Vec<u8>> = lfs_paths(repo, rev, &files)?.into_iter().collect();
    if marked.is_empty() {
        return Ok(());
    }
    let mut wanted: HashMap<Vec<u8>, Pointer> = HashMap::new();
    for (path, entry) in entries.iter() {
        let Entry::File { bytes, .. } = entry else {
            continue;
        };
        if !marked.contains(path) {
            continue;
        }
        let shown = String::from_utf8_lossy(path);
        let why = match bytes.len() >= 1024 {
            true => "is too large",
            false => match parse_pointer(bytes) {
                Some(p) => {
                    wanted.insert(path.clone(), p);
                    continue;
                }
                None => "is invalid",
            },
        };
        log::line(&format!(
            "warning: encountered file '/{shown}' that should have been a git-lfs pointer, but {why}"
        ));
    }
    let cache = cache_dir()?;
    let have: HashMap<&str, Vec<u8>> = wanted
        .values()
        .filter_map(|p| Some((p.oid.as_str(), cached(&cache, p)?)))
        .collect();
    let mut missing: Vec<Pointer> = wanted
        .values()
        .filter(|p| !have.contains_key(p.oid.as_str()))
        .cloned()
        .collect();
    missing.sort_by(|a, b| a.oid.cmp(&b.oid));
    missing.dedup();
    if !missing.is_empty() {
        let limits = Limits::new(nix_conf(&Session::current())).map_err(|e| e.to_string())?;
        let api = api(repo, rev)?;
        // git-lfs's own batch size, which servers may enforce
        let mut found = Vec::new();
        for chunk in missing.chunks(100) {
            found.extend(batch(&api, chunk, limits)?);
        }
        let jobs = found.len().min(8);
        let results: Vec<Result<(), String>> = thread::scope(|s| {
            let chunks: Vec<_> = found
                .chunks(found.len().div_ceil(jobs))
                .map(|chunk| {
                    let cache = &cache;
                    s.spawn(move || chunk.iter().try_for_each(|o| download(cache, o, limits)))
                })
                .collect();
            chunks
                .into_iter()
                .map(|h| h.join().expect("lfs download"))
                .collect()
        });
        results.into_iter().collect::<Result<(), String>>()?;
    }
    for (path, entry) in entries.iter_mut() {
        let (Some(p), Entry::File { bytes, .. }) = (wanted.get(path), entry) else {
            continue;
        };
        let got = match have.get(p.oid.as_str()) {
            Some(b) => b.clone(),
            None => cached(&cache, p)
                .ok_or_else(|| format!("iets: cannot read the LFS object {}", p.oid))?,
        };
        *bytes = got;
    }
    Ok(())
}

/// The object, when the cache holds it intact. A damaged one is a miss and
/// stays: a download replaces it atomically, and removing it here could
/// remove the object a sibling just put in its place.
fn cached(cache: &Path, p: &Pointer) -> Option<Vec<u8>> {
    let path = cache.join(&p.oid);
    let bytes = fs::read(&path).ok()?;
    if hash_bytes(HashAlgo::Sha256, &bytes).to_base16() != p.oid {
        return None;
    }
    cachefs::touch(&path);
    Some(bytes)
}

fn cache_dir() -> Result<PathBuf, String> {
    let dir = cachefs::root(None)
        .ok_or("iets: no cache directory for Git LFS objects (set HOME)")?
        .join("lfs");
    if !cachefs::prepare(&dir, cachefs::Shape::Objects, false) {
        return Err(format!("iets: cannot use '{}'", dir.display()));
    }
    cachefs::maybe_sweep(&dir, cachefs::Shape::Objects, false);
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sibling may have put the good object at the path since, so a damaged
    /// read leaves the path to the download that replaces it.
    #[test]
    fn a_damaged_object_is_a_miss_and_stays() {
        let cache = tempfile::tempdir().unwrap();
        let good = b"object";
        let p = Pointer {
            oid: hash_bytes(HashAlgo::Sha256, good).to_base16(),
            size: good.len() as u64,
        };
        let at = cache.path().join(&p.oid);
        fs::write(&at, b"damaged").unwrap();
        assert_eq!(cached(cache.path(), &p), None);
        assert!(at.exists());
        fs::write(&at, good).unwrap();
        assert_eq!(cached(cache.path(), &p).as_deref(), Some(&good[..]));
    }

    /// The LFS server chooses the download URL. A `file://` URL from it is
    /// refused, even when the local file has the expected content.
    #[test]
    fn an_href_to_a_local_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"object";
        let local = dir.path().join("o");
        fs::write(&local, bytes).unwrap();
        let d = Download {
            pointer: Pointer {
                oid: hash_bytes(HashAlgo::Sha256, bytes).to_base16(),
                size: bytes.len() as u64,
            },
            href: format!("file://{}", local.display()),
            headers: Vec::new(),
        };
        let e = download(
            dir.path(),
            &d,
            Limits {
                connect: None,
                stall: None,
                max_size: 1 << 20,
            },
        )
        .unwrap_err();
        assert!(e.contains("unsupported scheme 'file'"), "{e}");
    }

    #[test]
    fn a_host_is_never_an_ssh_option() {
        let e = authenticate("-oFoo/o/r").unwrap_err();
        assert!(e.contains("refusing ssh host '-oFoo'"), "{e}");
    }

    #[test]
    fn an_scp_origin_is_ssh() {
        assert_eq!(scp_to_ssh("git@h:o/r.git"), "ssh://git@h/o/r.git");
        assert_eq!(scp_to_ssh("https://h/o/r"), "https://h/o/r");
        assert_eq!(scp_to_ssh("/srv/r"), "/srv/r");
    }

    #[test]
    fn a_pointer_names_its_object() {
        let oid = "f5e02aa71e67f41d79023a128ca35bad86cf7b6656967bfe0884b3a3c4325eaf";
        let text =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 10\n");
        let want = Pointer {
            oid: oid.into(),
            size: 10,
        };
        assert_eq!(parse_pointer(text.as_bytes()), Some(want));
        assert_eq!(parse_pointer(b"plain file\n"), None);
    }
}
