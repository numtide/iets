//! Files under WASI preview 1: the instance reaches only what the host
//! preopened. There is one user, no file modes and no other process to lock
//! against, so those calls have nothing to do. `std::fs::Metadata` holds what
//! is needed on Unix, but `std` keeps the WASI accessors unstable, so inodes
//! and change times come from a new `stat`.

use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_NOW, futimens, utimensat};
use rustix::io::{pread, pwrite};

/// The links a path may pass through, as in Linux's `MAXSYMLINKS`.
const MAX_LINKS: usize = 40;

pub fn symlink(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    Ok(rustix::fs::symlink(original.as_ref(), link.as_ref())?)
}

/// Whether `name`, which holds no `/`, names one entry of its directory.
pub fn one_component(_name: &[u8]) -> bool {
    true
}

/// Puts `f`'s writes on the disk ahead of any later ones.
pub fn sync_ordered(f: &File) -> io::Result<()> {
    f.sync_all()
}

/// Removes a symbolic link, whatever it points at.
pub fn remove_link(p: &Path) -> io::Result<()> {
    fs::remove_file(p)
}

/// The resolved path. `std` has no `canonicalize` for WASI, so this walks the
/// path and follows each link. An absent file is an error, as it is on Unix.
pub fn canonicalize(p: &Path) -> io::Result<PathBuf> {
    let mut todo: Vec<PathBuf> = vec![std::env::current_dir()?.join(p)];
    let mut done = PathBuf::from("/");
    let mut links = 0;
    while let Some(next) = todo.pop() {
        let mut parts: Vec<Component> = next.components().collect();
        parts.reverse();
        while let Some(part) = parts.pop() {
            match part {
                Component::Prefix(_) | Component::CurDir => {}
                Component::RootDir => done = PathBuf::from("/"),
                Component::ParentDir => {
                    done.pop();
                }
                Component::Normal(name) => {
                    done.push(name);
                    let Ok(to) = fs::read_link(&done) else {
                        continue;
                    };
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(io::Error::other("too many levels of symbolic links"));
                    }
                    done.pop();
                    let rest: PathBuf = parts.drain(..).rev().collect();
                    todo.push(rest);
                    todo.push(to);
                    break;
                }
            }
        }
    }
    fs::symlink_metadata(&done)?;
    Ok(done)
}

/// Fills `buf` from `offset`, without moving the file's cursor.
pub fn read_exact_at(f: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match pread(f, &mut *buf, offset)? {
            0 => return Err(ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

/// Writes all of `buf` at `offset`, without moving the file's cursor.
pub fn write_all_at(f: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match pwrite(f, buf, offset)? {
            0 => return Err(ErrorKind::WriteZero.into()),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

pub fn is_executable(_m: &Metadata) -> bool {
    false
}

pub fn is_runnable(_m: &Metadata) -> bool {
    false
}

pub fn is_socket(_m: &Metadata) -> bool {
    false
}

/// Which file this is on its device, so a rename or a swap shows.
pub fn file_id(path: &Path, _m: &Metadata) -> (u64, u64) {
    rustix::fs::stat(path).map_or((0, 0), |s| (s.st_dev, s.st_ino))
}

/// Whether `path` still names the open file `f`: false once it was unlinked,
/// or replaced by another file.
pub fn same_file(f: &File, path: &Path) -> bool {
    match (rustix::fs::fstat(f), rustix::fs::stat(path)) {
        (Ok(a), Ok(b)) => (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino),
        _ => false,
    }
}

fn since_epoch(t: io::Result<SystemTime>) -> Duration {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default()
}

pub fn mtime_secs(m: &Metadata) -> i64 {
    since_epoch(m.modified()).as_secs() as i64
}

pub fn mtime_ns(m: &Metadata) -> i64 {
    since_epoch(m.modified()).as_nanos() as i64
}

/// The last change of the inode, which a copy or a rename cannot hide.
pub fn ctime_ns(path: &Path, _m: &Metadata) -> i64 {
    rustix::fs::stat(path).map_or(0, |s| {
        (s.st_ctim.tv_sec as i64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(s.st_ctim.tv_nsec as i64)
    })
}

pub fn create_mode(o: &mut OpenOptions, _mode: u32) -> &mut OpenOptions {
    o
}

pub fn dir_mode(b: &mut DirBuilder, _mode: u32) -> &mut DirBuilder {
    b
}

pub fn restrict_to_owner(_path: &Path) {}

pub fn owned_by_me(_m: &Metadata) -> bool {
    true
}

pub fn private_to_me(_m: &Metadata) -> bool {
    true
}

pub fn make_executable(_f: &File) -> io::Result<()> {
    Ok(())
}

pub fn set_executable(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

pub fn copy_mode(_m: &Metadata, _f: &File) -> io::Result<()> {
    Ok(())
}

/// `std` keeps the WASI lookup flags unstable, so this follows a link. The host
/// keeps the walk inside the directories it gave.
pub fn no_follow(o: &mut OpenOptions) -> &mut OpenOptions {
    o
}

pub fn no_follow_no_block(o: &mut OpenOptions) -> &mut OpenOptions {
    o
}

pub fn try_lock(_f: &File) -> bool {
    true
}

pub fn lock(_f: &File) -> bool {
    true
}

pub fn unlock(_f: &File) {}

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
