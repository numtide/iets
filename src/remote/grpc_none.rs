//! The `grpc://` store on a platform without sockets: the URL still parses and
//! the wire formats other stores share still work, and opening the store fails.

#[path = "grpc/config.rs"]
mod config;
#[path = "grpc/drvwire.rs"]
pub mod drvwire;

pub use config::GrpcConfig;

use crate::build::backend::Backend;
use crate::error::{Error, Result};

pub fn open(cfg: &GrpcConfig) -> Result<Box<dyn Backend>> {
    Err(Error::Usage(format!(
        "cannot open '{}': this platform has no network",
        cfg.url()
    )))
}
