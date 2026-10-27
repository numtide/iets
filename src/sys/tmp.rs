//! Temporary files and directories with names nobody can guess, created
//! exclusively, so a link planted in advance is never written through.

use std::fs::File;
#[cfg(unix)]
use std::fs::Permissions;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tempfile::Builder;

/// Writes `dest` through a temporary file beside it, renamed over it once
/// `write` succeeds, so a reader sees the old contents or the new ones. The
/// temporary is mode 0600 until `write` changes it.
pub fn replace(dest: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    let dir = match dest.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    // not named after `dest`: a name near the 255-byte limit would not fit
    let mut tmp = Builder::new()
        .prefix(".iets.")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    write(tmp.as_file_mut())?;
    tmp.persist(dest).map_err(|e| e.error)?;
    Ok(())
}

/// A fresh directory in `$TMPDIR` that only this user can enter. It outlives
/// the process: an `exec`ed shell still uses it.
pub fn private_dir(prefix: &str) -> io::Result<PathBuf> {
    let mut b = Builder::new();
    b.prefix(prefix);
    #[cfg(unix)]
    b.permissions(Permissions::from_mode(0o700));
    Ok(b.tempdir()?.keep())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    // Windows limits the whole path to 260 bytes, not a component
    #[cfg(unix)]
    #[test]
    fn a_name_at_the_length_limit_is_replaced() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("x".repeat(250));
        replace(&dest, |f| io::Write::write_all(f, b"new")).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_link_is_replaced_not_written_through() {
        let d = tempfile::tempdir().unwrap();
        let victim = d.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        let dest = d.path().join("dest");
        symlink(&victim, &dest).unwrap();
        replace(&dest, |f| io::Write::write_all(f, b"new")).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert!(!fs::symlink_metadata(&dest).unwrap().is_symlink());
    }

    #[test]
    fn a_private_dir_is_new_and_the_owners_alone() {
        let a = private_dir("iets-test-").unwrap();
        let b = private_dir("iets-test-").unwrap();
        assert_ne!(a, b);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir(a).unwrap();
        fs::remove_dir(b).unwrap();
    }
}
