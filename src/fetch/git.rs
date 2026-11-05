//! Remote Git repositories, each kept as a shallow bare clone under
//! `<cache root>/git/` and fetched as git.cc:getAccessorFromCommit does.
//! A fetch is `--depth 1`, since the tree of one commit is all it needs.
//! `revCount` reads a separate commit-only clone (`history`).

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::str;
use std::time::Duration;

use crate::cachefs;
use crate::flake::flakeref;
use crate::log;
use crate::session::Session;
use crate::source::nix_conf;
use crate::store::hash::{HashAlgo, base64_decode, hash_bytes};
use crate::sys;

/// What to fetch: `rev` when locked, else the commit `ref` names, else the
/// remote's default branch.
pub struct Want<'a> {
    pub url: &'a str,
    pub rev: Option<&'a str>,
    pub r#ref: Option<&'a str>,
    pub all_refs: bool,
    /// Whether `url` may name this machine's files. A remote repository's
    /// submodule must not copy a local repository into the store.
    pub local_ok: bool,
}

pub struct Fetched {
    pub repo: PathBuf,
    pub rev: String,
    pub r#ref: String,
}

fn cache_root() -> Option<PathBuf> {
    Some(cachefs::root(None)?.join("git"))
}

/// Whether `dir` is a clone this module keeps.
pub fn is_cached(dir: &Path) -> bool {
    cache_root().is_some_and(|r| dir.starts_with(r))
}

/// Whether `url` names a repository on this machine.
pub fn is_local(url: &str) -> bool {
    if url.starts_with("file:") || url.starts_with('/') || url.starts_with('.') {
        return true;
    }
    // `host:path` is ssh, unless a `/` comes before the `:`
    match (url.find("://"), url.find(':'), url.find('/')) {
        (Some(_), _, _) => false,
        (None, Some(c), slash) => slash.is_some_and(|s| s < c),
        (None, None, _) => true,
    }
}

/// `GIT_ALLOW_PROTOCOL`, which overrides the user's git config. `ext::`
/// and other remote helpers can run commands, so only git's safe transports
/// are allowed, plus `file` when `local_ok`.
fn protocols(local_ok: bool) -> &'static str {
    match local_ok {
        true => "https:http:ssh:git:file",
        false => "https:http:ssh:git",
    }
}

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(sys::program(OsStr::new("git")));
    c.arg("-C").arg(dir).args(args);
    // the history clone is partial
    c.env("GIT_NO_LAZY_FETCH", "1");
    c
}

/// A command that talks to a remote, over the transports `protocols` allows.
fn transport(dir: &Path, args: &[&str], local_ok: bool) -> Command {
    let mut c = command(dir, args);
    c.env("GIT_ALLOW_PROTOCOL", protocols(local_ok));
    c
}

fn run(mut c: Command) -> (bool, String) {
    match c.stderr(Stdio::null()).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
        ),
        Err(_) => (false, String::new()),
    }
}

/// `git ARGS` in `dir`: whether it succeeded, and its output.
pub(crate) fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    run(command(dir, args))
}

/// `git ARGS` talking to a remote from `dir`, its errors on the terminal.
fn loud(dir: &Path, args: &[&str], local_ok: bool) -> bool {
    let c = transport(dir, args, local_ok)
        .stdout(Stdio::null())
        .status();
    c.is_ok_and(|s| s.success())
}

/// A ref names a file under the clone, so it may not leave it.
fn safe_ref(r: &str) -> bool {
    Path::new(r)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
}

fn fresh(path: &Path, ttl: Duration) -> bool {
    let age = fs::metadata(path).and_then(|m| m.modified()).ok();
    age.and_then(|m| m.elapsed().ok()).is_some_and(|a| a < ttl)
}

/// git.cc:getDefaultRef for a remote: the branch the clone's `HEAD` names
/// while it is fresh, else the remote's, else the clone's however old.
fn default_ref(dir: &Path, want: &Want, ttl: Duration) -> String {
    let url = want.url;
    let head = dir.join("HEAD");
    let cached = fs::read_to_string(&head)
        .ok()
        .and_then(|h| Some(h.strip_prefix("ref: ")?.trim().to_string()))
        .filter(|r| git(dir, &["rev-parse", "--verify", "-q", r]).0);
    let offline = Session::current().settings().offline;
    if let Some(r) = cached.clone().filter(|_| offline || fresh(&head, ttl)) {
        return r;
    }
    // `fetch` then reports that it cannot download
    if offline {
        return "HEAD".into();
    }
    let ls = ["ls-remote", "--symref", "--", url, "HEAD"];
    let (ok, out) = run(transport(Path::new("."), &ls, want.local_ok));
    let remote = out.lines().next().filter(|_| ok).and_then(|l| {
        let (target, name) = l.split_once('\t')?;
        (name == "HEAD").then(|| target.strip_prefix("ref: ").unwrap_or(target).to_string())
    });
    match (remote, cached) {
        (Some(r), _) => r,
        (None, Some(r)) => {
            log::line(&format!(
                "warning: could not get HEAD ref for repository '{url}'; using expired cached ref '{r}'"
            ));
            r
        }
        (None, None) => {
            log::line(&format!(
                "warning: could not read HEAD ref from repo at '{url}', using 'master'"
            ));
            "master".into()
        }
    }
}

/// The commit `want` names in `repo`.
fn resolve(repo: &Path, want: &Want, r#ref: &str) -> Result<String, String> {
    let name = format!("{}^{{commit}}", want.rev.unwrap_or(r#ref));
    match git(
        repo,
        &["rev-parse", "--verify", "-q", "--end-of-options", &name],
    ) {
        (true, rev) if !rev.is_empty() => Ok(rev),
        _ => Err(match want.rev {
            Some(rev) => format!(
                "Cannot find Git revision '{rev}' in ref '{ref}' of repository '{}'! \
                 Please make sure that the rev exists on the ref you've specified or add \
                 allRefs = true; to fetchGit.",
                want.url
            ),
            None => format!("resolving Git reference '{ref}' in '{}'", want.url),
        }),
    }
}

/// The repository that holds what `want` names: a local repository with a
/// working tree is read where it is (git.cc:getRepoInfo), and any other URL
/// is fetched into its clone.
pub fn open(want: &Want) -> Result<Fetched, String> {
    if want.url.starts_with('-') {
        return Err(format!("iets: invalid Git URL '{}'", want.url));
    }
    if !want.local_ok && is_local(want.url) {
        return Err(format!(
            "iets: refusing to fetch the submodule '{}': it is on this machine, and its superproject is remote or the evaluation is pure",
            want.url
        ));
    }
    let local = flakeref::file_url_path(want.url)
        .or_else(|| want.url.starts_with('/').then(|| want.url.to_string()))
        .map(PathBuf::from)
        .filter(|p| p.join(".git").exists());
    let Some(repo) = local else {
        return fetch(want);
    };
    let r#ref = want.r#ref.unwrap_or("HEAD").to_string();
    let rev = resolve(&repo, want, &r#ref)?;
    Ok(Fetched { repo, rev, r#ref })
}

fn fetch(want: &Want) -> Result<Fetched, String> {
    let offline = Session::current().settings().offline;
    let root = cache_root().ok_or("iets: no cache directory for Git clones (set HOME)")?;
    if !cachefs::prepare(&root, cachefs::Shape::Clones, false) {
        return Err(format!("iets: cannot use '{}'", root.display()));
    }
    cachefs::maybe_sweep(&root, cachefs::Shape::Clones, false);
    let dir = root.join(hash_bytes(HashAlgo::Sha256, want.url.as_bytes()).to_nix32());
    let _lock = lock(&dir)?;
    if !dir.join("HEAD").exists()
        && !git(&root, &["init", "-q", "--bare", &dir.to_string_lossy()]).0
    {
        return Err(format!(
            "iets: cannot create a Git repository at '{}'",
            dir.display()
        ));
    }
    // submodule URLs are relative to `origin`
    git(&dir, &["config", "remote.origin.url", want.url]);
    // the age of a ref's file is when it was fetched, so gc must not pack it
    git(&dir, &["config", "gc.packRefs", "false"]);

    let secs = nix_conf(&Session::current()).int("tarball-ttl");
    let ttl = Duration::from_secs(secs.ok().flatten().unwrap_or(3600));
    // a cached rev needs no fetch, so skip the remote round trip
    let uses_default = want.r#ref.is_none()
        && (want.rev.is_none() || (want.all_refs && resolve(&dir, want, "HEAD").is_err()));
    let r#ref = match want.r#ref {
        Some(r) => r.to_string(),
        None if uses_default => default_ref(&dir, want, ttl),
        None => "HEAD".into(),
    };
    // the remote's HEAD and a submodule's branch come from the repository
    if !safe_ref(&r#ref) {
        return Err(format!("iets: invalid Git ref '{ref}' for '{}'", want.url));
    }
    let local_ref = match r#ref.starts_with("refs/") {
        true => dir.join(&r#ref),
        false => dir.join("refs/heads").join(&r#ref),
    };
    let stale = match want.rev {
        Some(_) => resolve(&dir, want, &r#ref).is_err(),
        None => want.all_refs || !fresh(&local_ref, ttl),
    };
    if stale && offline && (want.rev.is_some() || !local_ref.exists()) {
        return Err(format!("cannot fetch '{}': IETS_OFFLINE is set", want.url));
    }
    if stale && !offline {
        let refspec = match want.rev {
            Some(rev) => format!("{rev}:refs/iets/tip-{}", &rev[..2]),
            None if r#ref.starts_with("refs/") => format!("{0}:{0}", r#ref),
            None if r#ref == "HEAD" => "HEAD:HEAD".into(),
            None => format!("refs/heads/{0}:refs/heads/{0}", r#ref),
        };
        let _ = fs::remove_file(dir.join("shallow.lock"));
        let args = [
            "fetch",
            "--quiet",
            "--force",
            "--no-tags",
            "--depth",
            "1",
            "--",
            want.url,
            &refspec,
        ];
        if !loud(&dir, &args, want.local_ok) {
            if want.rev.is_some() || !local_ref.exists() {
                return Err(format!("Failed to fetch git repository '{}'", want.url));
            }
            log::line(&format!(
                "warning: could not update local clone of Git repository '{}'; continuing with the most recent version",
                want.url
            ));
        }
        if want.rev.is_none() {
            sys::fs::touch(&local_ref, false);
        }
        if uses_default && !git(&dir, &["symbolic-ref", "--", "HEAD", &r#ref]).0 {
            log::line(&format!(
                "warning: could not update cached head '{ref}' for '{}'",
                want.url
            ));
        }
    }
    let rev = resolve(&dir, want, &r#ref)?;
    Ok(Fetched {
        repo: dir,
        rev,
        r#ref,
    })
}

fn lock(dir: &Path) -> Result<cachefs::WriteLock, String> {
    let path = dir.with_extension("lock");
    cachefs::wait_lock(&path, || {
        log::line(&format!("waiting for the lock on '{}'", dir.display()))
    })
    .ok_or_else(|| format!("iets: cannot lock '{}'", path.display()))
}

/// The URL submodule URLs are relative to: `origin`'s, else the repository.
pub fn origin_url(repo: &Path) -> String {
    match git(repo, &["config", "--get", "remote.origin.url"]) {
        (true, url) if !url.is_empty() => url,
        _ => repo.to_string_lossy().into_owned(),
    }
}

/// The commits behind `rev`, counted in a commit-only clone of `dir`'s
/// remote. Deepening the shallow clone crashed git on nixpkgs.
pub fn rev_count(dir: &Path, rev: &str) -> Result<u64, String> {
    let hist = history(dir, rev)?;
    match git(&hist, &["rev-list", "--count", rev]) {
        (true, n) => n.parse().map_err(|_| format!("git rev-list printed '{n}'")),
        _ => Err(format!("iets: cannot count the commits behind {rev}")),
    }
}

fn history(dir: &Path, rev: &str) -> Result<PathBuf, String> {
    let url = origin_url(dir);
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    let hist = dir.with_file_name(format!("{name}-history"));
    let _lock = lock(&hist)?;
    if git(&hist, &["cat-file", "-e", &format!("{rev}^{{commit}}")]).0 {
        return Ok(hist);
    }
    if Session::current().settings().offline {
        return Err(format!(
            "cannot fetch the history of '{url}': IETS_OFFLINE is set"
        ));
    }
    if !hist.join("HEAD").exists() {
        let _ = fs::remove_dir_all(&hist);
        let init = ["init", "--quiet", "--bare", &hist.to_string_lossy()];
        let filter = ["config", "remote.origin.partialclonefilter", "tree:0"];
        let ok = git(Path::new("."), &init).0
            && git(&hist, &["config", "remote.origin.url", &url]).0
            && git(&hist, &["config", "remote.origin.promisor", "true"]).0
            && git(&hist, &filter).0
            // counting a million commits takes 1 s with a commit-graph, 12 s without
            && git(&hist, &["config", "fetch.writeCommitGraph", "true"]).0;
        if !ok {
            return Err(format!("iets: cannot create '{}'", hist.display()));
        }
    }
    log::line(&format!(
        "iets: fetching the history of '{url}' to count its commits"
    ));
    let refspec = format!("{rev}:refs/iets/count");
    let args = [
        "fetch",
        "--quiet",
        "--force",
        "--no-tags",
        "--filter=tree:0",
        "origin",
        &refspec,
    ];
    match loud(&hist, &args, is_local(&url)) {
        true => Ok(hist),
        false => Err(format!(
            "Failed to fetch the history of git repository '{url}'"
        )),
    }
}

/// The key type an SSH public key blob starts with, such as
/// `ecdsa-sha2-nistp256`, which is what an allowed-signers line needs.
fn blob_type(blob: &[u8]) -> Option<&str> {
    let len = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?) as usize;
    str::from_utf8(blob.get(4..4 + len)?).ok()
}

/// git-utils.cc:verifyCommit. OpenSSH checks `rev`'s signature against
/// `keys`, each a base64 public key. Its `for *` names our allowed signers,
/// which rules out a key from the user's GPG keyring.
pub fn verify_commit(repo: &Path, rev: &str, keys: &[String]) -> Result<(), String> {
    let mut signers = String::new();
    for k in keys {
        let raw = base64_decode(k).map_err(|_| format!("invalid SSH public key '{k}'"))?;
        let ty = blob_type(&raw).ok_or_else(|| format!("invalid SSH public key '{k}'"))?;
        signers.push_str(&format!("* {ty} {k}\n"));
    }
    let file = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    fs::write(file.path(), signers).map_err(|e| e.to_string())?;
    let allowed = format!("gpg.ssh.allowedSignersFile={}", file.path().display());
    let out = Command::new(sys::program(OsStr::new("git")))
        .args(["-c", &allowed, "-C"])
        .arg(repo)
        .args(["verify-commit", "--", rev])
        .output()
        .map_err(|e| format!("iets: cannot run git: {e}"))?;
    let text = String::from_utf8_lossy(&[&out.stdout[..], &out.stderr[..]].concat()).into_owned();
    let good = text
        .lines()
        .any(|l| l.starts_with("Good \"git\" signature for * with "));
    match out.status.success() && good {
        true => Ok(()),
        false => Err(format!(
            "Commit signature verification on commit {rev} failed: {text}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("file:///srv/r.git", true)]
    #[case("/srv/r.git", true)]
    #[case("./r", true)]
    #[case("r", true)]
    #[case("dir/x:y", true)]
    #[case("https://h/o/r.git", false)]
    #[case("ssh://git@h/o/r", false)]
    #[case("git@h:o/r", false)]
    #[case("ext::sh -c x", false)]
    fn a_url_is_local_when_git_would_read_a_path(#[case] url: &str, #[case] local: bool) {
        assert_eq!(is_local(url), local);
    }

    fn want(url: &str, local_ok: bool) -> Want<'_> {
        let rev = Some("0123456789012345678901234567890123456789");
        Want {
            url,
            rev,
            r#ref: None,
            all_refs: false,
            local_ok,
        }
    }

    #[rstest]
    fn a_remote_repositorys_submodule_cannot_name_a_local_one(
        #[values("/etc", "file:///home/u/secret", "../secret")] url: &str,
    ) {
        let e = open(&want(url, false)).err().unwrap();
        assert!(e.contains("refusing to fetch"), "{e}");
    }

    #[rstest]
    #[case("refs/heads/release/1.0", true)]
    #[case("refs/heads/../../../x", false)]
    #[case("/etc/passwd", false)]
    fn a_ref_stays_in_the_clone(#[case] r: &str, #[case] ok: bool) {
        assert_eq!(safe_ref(r), ok);
    }

    #[test]
    fn a_url_cannot_be_an_option() {
        let e = open(&want("--upload-pack=touch /tmp/x", true))
            .err()
            .unwrap();
        assert!(e.contains("invalid Git URL"), "{e}");
    }
}
