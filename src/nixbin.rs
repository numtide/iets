//! The real Nix binaries. Under the classic symlink farm, `nix` on `PATH` is
//! iets itself, so every subprocess and the fallback resolve past it.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Written by the farm derivation next to its `bin/`: the directory that
/// holds the nix the farm shadows.
const REAL_DIR_FILE: &str = "share/iets/real-nix-bin";

fn executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
}

/// The bin directory a farm entry points at: every hop of the symlink chain
/// from `p` is a directory that may carry `REAL_DIR_FILE` beside it. The
/// system path is itself a symlink farm over ours, so the file is one or
/// more hops away.
fn farm_real_dir(p: &Path) -> Option<PathBuf> {
    let mut cur = p.to_path_buf();
    for _ in 0..16 {
        let dir = cur.parent()?;
        let marker = dir.join("..").join(REAL_DIR_FILE);
        if let Ok(s) = std::fs::read_to_string(&marker) {
            return Some(PathBuf::from(s.trim()));
        }
        let target = std::fs::read_link(&cur).ok()?;
        cur = if target.is_absolute() {
            target
        } else {
            dir.join(target)
        };
    }
    None
}

/// A candidate that is iets, whatever build of it: this executable, or a
/// symlink chain that ends in a file named `iets`. A host can hold several
/// builds of the same source, and a farm made by one of them points at its
/// own `bin/iets`, which is not `/proc/self/exe`.
fn is_iets(canonical: &Path, me: Option<&Path>) -> bool {
    Some(canonical) == me || canonical.file_name() == Some(std::ffi::OsStr::new("iets"))
}

/// `dir/<name>` as a real nix: the file itself when it is not iets, else the
/// bin directory the farm entry for that iets records. `None` means the
/// directory offers no real nix, so the caller must look further. Without
/// this every candidate would be a candidate to exec, and a directory that
/// holds iets's own classic names makes the fallback exec iets again.
fn candidate(dir: &Path, name: &str, me: Option<&Path>) -> Option<PathBuf> {
    let p = dir.join(name);
    if !executable(&p) {
        return None;
    }
    if !std::fs::canonicalize(&p).is_ok_and(|c| is_iets(&c, me)) {
        return Some(p);
    }
    let r = farm_real_dir(&p)?.join(name);
    executable(&r).then_some(r)
}

/// The first `<name>` in `dirs` that is not iets, or the bin directory the
/// farm entry for an iets records. Every iets is skipped like ourselves,
/// so a second build of iets under the name `nix` never wins.
fn on_path(name: &str, dirs: impl Iterator<Item = PathBuf>, me: Option<&Path>) -> Option<PathBuf> {
    dirs.filter_map(|dir| candidate(&dir, name, me)).next()
}

/// The real `<name>`: `$IETS_NIX_BIN_DIR/<name>` when it exists there, else
/// the bin directory the farm on `PATH` was built for, else the first
/// `<name>` on `PATH` that is not iets.
pub fn real(name: &str) -> Option<PathBuf> {
    let me = std::fs::canonicalize("/proc/self/exe").ok();
    if let Some(dir) = &crate::session::Session::current().settings().nix_bin_dir
        && let Some(p) = candidate(dir, name, me.as_deref())
    {
        return Some(p);
    }
    let dirs = std::env::split_paths(&std::env::var_os("PATH")?).collect::<Vec<_>>();
    on_path(name, dirs.into_iter(), me.as_deref()).or_else(|| sibling(name))
}

/// The farm never shadows `nix-store`, so its real location names the nix
/// installation even on a system path built without the marker.
fn sibling(name: &str) -> Option<PathBuf> {
    let me = std::fs::canonicalize("/proc/self/exe").ok();
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let probe = dir.join("nix-store");
        if !executable(&probe) {
            continue;
        }
        let real = std::fs::canonicalize(&probe).ok()?;
        let p = real.parent()?.join(name);
        if executable(&p) && !std::fs::canonicalize(&p).is_ok_and(|c| is_iets(&c, me.as_deref())) {
            return Some(p);
        }
    }
    None
}

/// `nix` for a subprocess. A bare `nix` when none is found, so the spawn
/// error names it.
pub fn nix() -> PathBuf {
    real("nix").unwrap_or_else(|| PathBuf::from("nix"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn farm_marker_is_found_through_two_hops() {
        let tmp = std::env::temp_dir().join(format!("iets-nixbin-{}", std::process::id()));
        let farm = tmp.join("farm");
        let sys = tmp.join("sys");
        std::fs::create_dir_all(farm.join("bin")).unwrap();
        std::fs::create_dir_all(farm.join("share/iets")).unwrap();
        std::fs::create_dir_all(sys.join("bin")).unwrap();
        std::fs::write(farm.join(REAL_DIR_FILE), "/real/bin\n").unwrap();
        std::os::unix::fs::symlink("/proc/self/exe", farm.join("bin/nix")).unwrap();
        std::os::unix::fs::symlink(farm.join("bin/nix"), sys.join("bin/nix")).unwrap();
        assert_eq!(
            farm_real_dir(&sys.join("bin/nix")),
            Some(PathBuf::from("/real/bin"))
        );
        assert_eq!(farm_real_dir(&sys.join("bin/other")), None);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// Two builds of iets on `PATH`, both shadowing `nix`: neither is the
    /// real nix, whichever one is running.
    #[test]
    fn every_iets_build_is_skipped() {
        let tmp = std::env::temp_dir().join(format!("iets-nixbin-two-{}", std::process::id()));
        let (x, y, real) = (tmp.join("x"), tmp.join("y"), tmp.join("real"));
        let exe = |p: &Path| {
            std::fs::write(p, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        for d in [&x, &y, &real] {
            std::fs::create_dir_all(d).unwrap();
        }
        for d in [&x, &y] {
            exe(&d.join("iets"));
            std::os::unix::fs::symlink(d.join("iets"), d.join("nix")).unwrap();
        }
        let nix = real.join("nix");
        exe(&nix);

        let dirs = [x.clone(), y.clone(), real.clone()];
        assert_eq!(
            on_path("nix", dirs.iter().cloned(), None),
            Some(nix.clone())
        );
        assert_eq!(on_path("nix", dirs[..2].iter().cloned(), None), None);
        assert_eq!(on_path("nix", dirs.iter().cloned(), Some(&nix)), None);

        // what `nix-bin-dir` points at goes through the same test, so a
        // directory of iets's own classic names cannot exec iets again
        assert_eq!(candidate(&x, "nix", None), None);
        assert_eq!(candidate(&real, "nix", None), Some(nix));
        assert_eq!(candidate(&real, "nix-build", None), None);

        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
