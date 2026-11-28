#[cfg(target_os = "wasi")]
use std::{fs::File, io, path::Path};

use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_NOW, futimens, utimensat};

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
