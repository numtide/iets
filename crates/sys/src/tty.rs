//! The terminal and the user behind it.

use std::io::{self, IsTerminal, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};

/// Columns and rows of stderr's terminal, if it has one.
pub fn size() -> Option<(usize, usize)> {
    match rustix::termios::tcgetwinsize(io::stderr()) {
        Ok(ws) if ws.ws_col > 1 && ws.ws_row > 1 => {
            Some((usize::from(ws.ws_col), usize::from(ws.ws_row)))
        }
        _ => None,
    }
}

/// Row and column, counted from 1, of stderr's cursor. Asks the terminal
/// through stdin, which needs both to be terminals. A terminal that does not
/// answer in a fifth of a second is taken for one that cannot.
pub fn cursor() -> Option<(usize, usize)> {
    let stdin = io::stdin();
    if !stdin.is_terminal() || !io::stderr().is_terminal() {
        return None;
    }
    let saved = tcgetattr(&stdin).ok()?;
    let mut raw = saved.clone();
    raw.local_modes
        .remove(LocalModes::ICANON | LocalModes::ECHO);
    raw.special_codes[SpecialCodeIndex::VMIN] = 0;
    raw.special_codes[SpecialCodeIndex::VTIME] = 2;
    tcsetattr(&stdin, OptionalActions::Now, &raw).ok()?;
    let mut reply = Vec::new();
    let mut err = io::stderr();
    if err.write_all(b"\x1b[6n").and_then(|()| err.flush()).is_ok() {
        let mut chunk = [0u8; 32];
        while !reply.ends_with(b"R") && reply.len() < 32 {
            match rustix::io::read(&stdin, &mut chunk) {
                Ok(n) if n > 0 => reply.extend_from_slice(&chunk[..n]),
                _ => break,
            }
        }
    }
    let _ = tcsetattr(&stdin, OptionalActions::Now, &saved);
    let reply = String::from_utf8(reply).ok()?;
    let body = reply.rsplit_once("\x1b[")?.1.strip_suffix('R')?;
    let (row, col) = body.split_once(';')?;
    Some((row.parse().ok()?, col.parse().ok()?))
}

/// The row where the live region starts, or 0 for none. A signal that ends
/// the process takes the region down first, or the shell is left with a
/// scroll margin and no cursor.
static REGION_TOP: AtomicUsize = AtomicUsize::new(0);

pub fn guard_region(top: usize) {
    static INSTALL: Once = Once::new();
    REGION_TOP.store(top, Ordering::Relaxed);
    if top > 0 {
        INSTALL.call_once(|| {
            for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                // SAFETY: the handler only calls async-signal-safe functions.
                // A handler somebody else installed stays.
                unsafe {
                    let old = libc::signal(sig, on_signal as *const () as libc::sighandler_t);
                    if old != libc::SIG_DFL {
                        libc::signal(sig, old);
                    }
                }
            }
        });
    }
}

extern "C" fn on_signal(sig: libc::c_int) {
    let top = REGION_TOP.load(Ordering::Relaxed);
    if top > 0 {
        let mut buf = [0u8; 40];
        let mut n = 0;
        let mut put = |bytes: &[u8]| {
            buf[n..n + bytes.len()].copy_from_slice(bytes);
            n += bytes.len();
        };
        put(b"\x1b[r\x1b[");
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        let mut v = top;
        while v > 0 {
            at -= 1;
            digits[at] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        put(&digits[at..]);
        put(b";1H\x1b[J\x1b[?25h");
        // SAFETY: write(2) is async-signal-safe and the buffer is ours.
        unsafe {
            libc::write(libc::STDERR_FILENO, buf.as_ptr().cast(), n);
        }
    }
    // SAFETY: back to the default action and the signal again, so the exit
    // status is the one the shell expects.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

pub fn is_root() -> bool {
    rustix::process::getuid().is_root()
}

/// Rust masks SIGPIPE, so a closed stdout surfaces as an EPIPE panic. The
/// default disposition makes the process die quietly instead.
pub fn restore_sigpipe() {
    // SAFETY: SIG_DFL is a valid disposition and no handler is replaced.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
