//! The file operations of `fs.rs` on Windows, which has no mode bits, no
//! inode numbers and no inode change time in `std::fs::Metadata`.

use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io;
use std::mem;
use std::os::windows::fs::{self as winfs, FileExt, MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
};

/// 100 ns ticks from 1601 to the Unix epoch.
const EPOCH_TICKS: u64 = 116_444_736_000_000_000;

/// Whether `name`, which holds no `/`, names one entry of its directory:
/// no other separator, no stream, and no device. The devices are the
/// reserved names of "Naming Files, Paths, and Namespaces": CON, PRN, AUX,
/// NUL, CONIN$, CONOUT$, COM and LPT with a digit or a superscript digit,
/// with or without an extension.
pub fn one_component(name: &[u8]) -> bool {
    const DEVICES: [&[u8]; 6] = [b"con", b"prn", b"aux", b"nul", b"conin$", b"conout$"];
    const SUPERSCRIPTS: [&[u8]; 3] = ["¹".as_bytes(), "²".as_bytes(), "³".as_bytes()];
    if name.iter().any(|&b| b == b'\\' || b == b':') || name.ends_with(b".") || name.ends_with(b" ")
    {
        return false;
    }
    let stem = name
        .split(|&b| b == b'.')
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    let numbered = match stem
        .strip_prefix(b"com")
        .or_else(|| stem.strip_prefix(b"lpt"))
    {
        Some([d]) => d.is_ascii_digit(),
        Some(rest) => SUPERSCRIPTS.contains(&rest),
        None => false,
    };
    !DEVICES.contains(&stem.as_slice()) && !numbered
}

/// The kind of link follows what the target names. A target on the network
/// or in a device namespace is not looked up and gets no link: a restore
/// makes no network request, and a link of the wrong kind would be broken.
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (target, link) = (target.as_ref(), link.as_ref());
    let remote = matches!(
        target.components().next(),
        Some(Component::Prefix(p))
            if matches!(
                p.kind(),
                Prefix::UNC(..) | Prefix::VerbatimUNC(..) | Prefix::DeviceNS(_) | Prefix::Verbatim(_)
            )
    );
    if remote {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "no link to '{}': a network or device path",
                target.display()
            ),
        ));
    }
    let resolved = link
        .parent()
        .map_or_else(|| target.to_owned(), |p| p.join(target));
    if resolved.is_dir() {
        winfs::symlink_dir(target, link)
    } else {
        winfs::symlink_file(target, link)
    }
}

pub fn read_exact_at(f: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match f.seek_read(buf, offset)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}

pub fn write_all_at(f: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match f.seek_write(buf, offset)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
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

pub fn is_runnable(m: &Metadata) -> bool {
    m.is_file()
}

pub fn is_socket(_m: &Metadata) -> bool {
    false
}

/// The volume and the file index, or zeros when the file cannot be opened.
pub fn file_id(path: &Path, _m: &Metadata) -> (u64, u64) {
    let Ok(f) = open_for_attributes(path) else {
        return (0, 0);
    };
    // SAFETY: the handle is open and the struct is the size the call expects.
    let info = unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = mem::zeroed();
        if GetFileInformationByHandle(f.as_raw_handle(), &mut info) == 0 {
            return (0, 0);
        }
        info
    };
    let index = u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow);
    (u64::from(info.dwVolumeSerialNumber), index)
}

pub fn mtime_secs(m: &Metadata) -> i64 {
    mtime_ns(m).div_euclid(1_000_000_000)
}

pub fn mtime_ns(m: &Metadata) -> i64 {
    ticks_to_ns(m.last_write_time())
}

/// The last change of the file's metadata, or zero when it cannot be read.
pub fn ctime_ns(path: &Path, _m: &Metadata) -> i64 {
    let Ok(f) = open_for_attributes(path) else {
        return 0;
    };
    // SAFETY: the handle is open and the struct is the size the call expects.
    let ticks = unsafe {
        let mut info: FILE_BASIC_INFO = mem::zeroed();
        let size = size_of::<FILE_BASIC_INFO>() as u32;
        let p = (&raw mut info).cast();
        if GetFileInformationByHandleEx(f.as_raw_handle(), FileBasicInfo, p, size) == 0 {
            return 0;
        }
        info.ChangeTime
    };
    ticks_to_ns(ticks as u64)
}

fn ticks_to_ns(ticks: u64) -> i64 {
    (ticks as i64)
        .wrapping_sub(EPOCH_TICKS as i64)
        .wrapping_mul(100)
}

/// Opens the file itself, or the link, with no access beyond its attributes.
fn open_for_attributes(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

pub fn create_mode(o: &mut OpenOptions, _mode: u32) -> &mut OpenOptions {
    o
}

pub fn dir_mode(b: &mut DirBuilder, _mode: u32) -> &mut DirBuilder {
    b
}

pub fn restrict_to_owner(_path: &Path) {}

/// Windows has no owner bits to read here: the profile's ACLs decide.
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

pub fn copy_mode(m: &Metadata, f: &File) -> io::Result<()> {
    f.set_permissions(m.permissions())
}

pub fn no_follow(o: &mut OpenOptions) -> &mut OpenOptions {
    o.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
}

pub fn no_follow_no_block(o: &mut OpenOptions) -> &mut OpenOptions {
    no_follow(o)
}

pub fn try_lock(f: &File) -> bool {
    f.try_lock().is_ok()
}

pub fn lock(f: &File) -> bool {
    f.lock().is_ok()
}

pub fn unlock(f: &File) {
    let _ = f.unlock();
}

/// Removes a symbolic link. One to a directory goes with `remove_dir`.
pub fn remove_link(p: &Path) -> io::Result<()> {
    fs::remove_file(p).or_else(|_| fs::remove_dir(p))
}

/// The resolved path, without the `\\?\` that `std` puts before a drive.
/// That form has no `..` and no `/`, and the callers show it and compare it.
pub fn canonicalize(p: &Path) -> io::Result<PathBuf> {
    let full = fs::canonicalize(p)?;
    let s = full.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') && !rest.starts_with("UNC") => {
            Ok(PathBuf::from(rest))
        }
        _ => Ok(full),
    }
}

pub fn touch(path: &Path, follow: bool) {
    if let Ok(f) = open_for_times(path, follow) {
        touch_open(&f);
    }
}

pub fn touch_open(f: &File) {
    let now = SystemTime::now();
    let _ = f.set_times(fs::FileTimes::new().set_accessed(now).set_modified(now));
}

pub fn set_times(path: &Path, secs: i64, follow: bool) -> io::Result<()> {
    let at = u64::try_from(secs)
        .map(|s| UNIX_EPOCH + Duration::from_secs(s))
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let f = open_for_times(path, follow)?;
    f.set_times(fs::FileTimes::new().set_accessed(at).set_modified(at))
}

fn open_for_times(path: &Path, follow: bool) -> io::Result<File> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if !follow {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    OpenOptions::new()
        .write(true)
        .custom_flags(flags)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_to_an_absolute_directory_is_a_directory_link() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("dir");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("f"), "x").unwrap();
        let link = d.path().join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(fs::read(link.join("f")).unwrap(), b"x");
    }

    #[test]
    fn a_network_or_device_target_gets_no_link() {
        let d = tempfile::tempdir().unwrap();
        for t in [
            r"\\server\share\dir",
            r"\\?\UNC\server\share",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\Device\Mup\server\share",
        ] {
            let e = symlink(t, d.path().join("link")).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{t}");
        }
        assert!(!d.path().join("link").exists());
    }

    #[test]
    fn reserved_names_are_not_entries() {
        for n in [
            "con",
            "CON.txt",
            "prn",
            "aux.",
            "nul",
            "conin$",
            "CONOUT$",
            "com1",
            "COM9.log",
            "lpt0",
            "com¹",
            "LPT³.x",
            "a:b",
            "a\\b",
            "trailing ",
            "trailing.",
        ] {
            assert!(!one_component(n.as_bytes()), "{n}");
        }
        for n in [
            "con1", "comx", "com10", "lpt", "common", "console", "file.con", "a.b",
        ] {
            assert!(one_component(n.as_bytes()), "{n}");
        }
    }
}
