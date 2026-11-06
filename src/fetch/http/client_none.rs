//! The network half of [`super`] on a platform without sockets: `file:` URLs
//! still work, and any other URL fails when it is fetched.

use std::io::Read;

use super::{Limits, bad, get};
use crate::error::Result;

pub(super) fn fetch(
    url: &str,
    _: &[(String, String)],
    _: Option<Vec<u8>>,
    _: Limits,
    _: impl FnOnce(Box<dyn Read>) -> Result<u64>,
) -> Result<u64> {
    Err(bad(url, "this platform has no network"))
}

/// [`get`], with no connection to keep.
pub struct Kept;

impl Kept {
    pub fn new() -> Result<Kept> {
        Ok(Kept)
    }

    /// The body of `url`.
    pub fn get(&mut self, url: &str, limits: Limits) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        get(url, &[], limits, false, &mut body)?;
        Ok(body)
    }
}
