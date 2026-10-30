//! A writer into memory, to read back what a run wrote.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct Sink(Arc<Mutex<Vec<u8>>>);

impl Sink {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
