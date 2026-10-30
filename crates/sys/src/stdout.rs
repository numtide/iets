//! Where a run's results go: the process's stdout unless a host redirects it.

use std::io::{self, IsTerminal, Write};
use std::sync::{Mutex, MutexGuard};

#[derive(Default)]
pub struct Stdout(Mutex<Option<Box<dyn Write + Send>>>);

impl Stdout {
    pub fn write_to(&self, w: Box<dyn Write + Send>) {
        *self.lock() = Some(w);
    }

    fn lock(&self) -> MutexGuard<'_, Option<Box<dyn Write + Send>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn write_all(&self, b: &[u8]) -> io::Result<()> {
        match &mut *self.lock() {
            Some(w) => {
                w.write_all(b)?;
                w.flush()
            }
            None => {
                let mut o = io::stdout();
                o.write_all(b)?;
                o.flush()
            }
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.lock().is_none() && io::stdout().is_terminal()
    }

    pub fn writer(&self) -> Writer<'_> {
        Writer(self)
    }
}

pub struct Writer<'a>(&'a Stdout);

impl Write for Writer<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.write_all(b)?;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
