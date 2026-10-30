//! The console and the user behind it.

use std::io;
use std::mem;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::System::Console::{CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo};

/// Columns and rows of stderr's console, if it has one.
pub fn size() -> Option<(usize, usize)> {
    // SAFETY: the handle is stderr's and the struct is the size the call expects.
    let info = unsafe {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = mem::zeroed();
        if GetConsoleScreenBufferInfo(io::stderr().as_raw_handle(), &mut info) == 0 {
            return None;
        }
        info
    };
    let cols = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
    let rows = i32::from(info.srWindow.Bottom) - i32::from(info.srWindow.Top) + 1;
    let (cols, rows) = (usize::try_from(cols).ok()?, usize::try_from(rows).ok()?);
    (cols > 1 && rows > 1).then_some((cols, rows))
}

/// A console answers no cursor query through stdin.
pub fn cursor() -> Option<(usize, usize)> {
    None
}

/// The console is left as it is.
pub fn guard_region(_top: usize) {}

pub fn is_root() -> bool {
    false
}

/// A closed pipe is an error return on Windows, not a signal.
pub fn restore_sigpipe() {}
