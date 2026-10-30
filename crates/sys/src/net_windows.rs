//! Windows has no local stream sockets in `std`, so connecting fails and
//! the callers report it like any unreachable daemon.

use std::io::{self, Read, Write};
use std::path::Path;

pub struct UnixStream;

impl UnixStream {
    pub fn connect(_path: impl AsRef<Path>) -> io::Result<UnixStream> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn try_clone(&self) -> io::Result<UnixStream> {
        Ok(UnixStream)
    }
}

impl Read for UnixStream {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl Write for UnixStream {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
