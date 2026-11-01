//! File modes, inodes, symlinks and open flags, which `std::fs` only
//! exposes per platform.

use std::os::unix::fs::FileExt;
pub use std::os::unix::fs::symlink;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};

use std::fs::{self, DirBuilder, File, Metadata, OpenOptions, Permissions};
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, CWD, FlockOperation, OFlags, Timespec, Timestamps, UTIME_NOW, flock, futimens,
    utimensat,
};
use rustix::io::Errno;
use rustix::process::geteuid;

/// Whether `name`, which holds no `/`, names one entry of its directory.
pub fn one_component(_name: &[u8]) -> bool {
    true
}

/// Removes a symbolic link, whatever it points at.
pub fn remove_link(p: &Path) -> io::Result<()> {
    fs::remove_file(p)
}

/// The resolved path. Unix has nothing to strip.
pub fn canonicalize(p: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(p)
}

/// Fills `buf` from `offset`, without moving the file's cursor.
pub fn read_exact_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    f.read_exact_at(buf, offset)
}

/// Writes all of `buf` at `offset`, without moving the file's cursor.
pub fn write_all_at(f: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    f.write_all_at(buf, offset)
}

/// Owner execute bit, which is what a NAR calls executable.
pub fn is_executable(m: &Metadata) -> bool {
    m.mode() & 0o100 != 0
}

/// Any execute bit, which is what a lookup in `PATH` needs.
pub fn is_runnable(m: &Metadata) -> bool {
    m.mode() & 0o111 != 0
}

pub fn is_socket(m: &Metadata) -> bool {
    m.file_type().is_socket()
}

/// Which file this is on its device, so a rename or a swap shows. `path` is
/// where `m` came from, for platforms that need a handle to answer.
pub fn file_id(_path: &Path, m: &Metadata) -> (u64, u64) {
    (m.dev(), m.ino())
}

pub fn mtime_secs(m: &Metadata) -> i64 {
    m.mtime()
}

pub fn mtime_ns(m: &Metadata) -> i64 {
    m.mtime()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(m.mtime_nsec())
}

/// The last change of the inode, which a copy or a rename cannot hide.
pub fn ctime_ns(_path: &Path, m: &Metadata) -> i64 {
    m.ctime()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(m.ctime_nsec())
}

/// Creates the file with `mode`, less what the umask takes away.
pub fn create_mode(o: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    o.mode(mode)
}

pub fn dir_mode(b: &mut DirBuilder, mode: u32) -> &mut DirBuilder {
    b.mode(mode)
}

/// Gives `path` mode 0700 when it has another: wider, or narrower than its
/// owner can use, as a umask can leave it.
pub fn restrict_to_owner(path: &Path) {
    let Ok(m) = fs::metadata(path) else {
        return;
    };
    if m.permissions().mode() & 0o777 != 0o700 {
        let _ = fs::set_permissions(path, Permissions::from_mode(0o700));
    }
}

pub fn owned_by_me(m: &Metadata) -> bool {
    m.uid() == geteuid().as_raw()
}

/// Whether `m` is this user's and closed to everyone else.
pub fn private_to_me(m: &Metadata) -> bool {
    owned_by_me(m) && m.permissions().mode() & 0o077 == 0
}

pub fn make_executable(f: &File) -> io::Result<()> {
    f.set_permissions(Permissions::from_mode(0o755))
}

pub fn set_executable(path: &Path, executable: bool) -> io::Result<()> {
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, Permissions::from_mode(mode))
}

/// Gives `f` the permissions `m` had.
pub fn copy_mode(m: &Metadata, f: &File) -> io::Result<()> {
    f.set_permissions(m.permissions())
}

/// Refuses a final symbolic link.
pub fn no_follow(o: &mut OpenOptions) -> &mut OpenOptions {
    o.custom_flags(OFlags::NOFOLLOW.bits() as i32)
}

/// Refuses a final symbolic link, and does not wait on a FIFO for a writer.
pub fn no_follow_no_block(o: &mut OpenOptions) -> &mut OpenOptions {
    o.custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
}

/// Takes an exclusive lock on `f` if nobody holds it.
pub fn try_lock(f: &File) -> bool {
    flock(f, FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// Takes an exclusive lock on `f`, waiting for it. `false` on a real error.
pub fn lock(f: &File) -> bool {
    loop {
        match flock(f, FlockOperation::LockExclusive) {
            Ok(()) => return true,
            Err(Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}

pub fn unlock(f: &File) {
    let _ = flock(f, FlockOperation::Unlock);
}

const NOW: Timestamps = Timestamps {
    last_access: Timespec {
        tv_sec: 0,
        tv_nsec: UTIME_NOW,
    },
    last_modification: Timespec {
        tv_sec: 0,
        tv_nsec: UTIME_NOW,
    },
};

/// Sets both stamps of `path` to now. On a symbolic link, `follow` picks the
/// link or its target.
pub fn touch(path: &Path, follow: bool) {
    let _ = utimensat(CWD, path, &NOW, at_flags(follow));
}

/// Sets both stamps of the open file to now.
pub fn touch_open(f: &File) {
    let _ = futimens(f, &NOW);
}

/// Sets both stamps of `path` to `secs` after the epoch.
pub fn set_times(path: &Path, secs: i64, follow: bool) -> io::Result<()> {
    let ts = Timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    let times = Timestamps {
        last_access: ts,
        last_modification: ts,
    };
    Ok(utimensat(CWD, path, &times, at_flags(follow))?)
}

fn at_flags(follow: bool) -> AtFlags {
    if follow {
        AtFlags::empty()
    } else {
        AtFlags::SYMLINK_NOFOLLOW
    }
}
