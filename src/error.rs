//! The one error type for everything outside the evaluator. `src/eval/error.rs`
//! keeps its own: eval errors carry a source position and a trace.

use std::error::Error as StdError;
use std::{fmt, io, result};

use crate::eval::error;
use crate::log;
use crate::store::drv::DrvError;
use crate::store::hash::HashError;
use crate::store::path::StorePathError;

#[derive(Debug)]
pub enum Error {
    Io {
        context: String,
        source: io::Error,
    },
    /// Malformed data from another process: the nix-daemon worker protocol, a
    /// nix-eval-jobs line, `nix path-info`, a dev-env dump.
    Protocol(String),
    Plan(String),
    /// Store path, derivation or hash.
    Store(String),
    Flake(String),
    Cache(String),
    /// Something Nix does that iets does not, such as a flake registry
    /// lookup: classic mode falls back on it (`ErrorKind::Unsupported`).
    Unsupported(String),
    Eval(Box<error::Error>),
    /// Bad command-line input.
    Usage(String),
    /// nix.conf: a syntax error, a missing include, or a bad setting value.
    Config(String),
    /// Another error and the fix for it, which the record shows as `hint`.
    Hint(Box<Error>, String),
}

pub type Result<T> = result::Result<T, Error>;

/// `strerror` text without Rust's " (os error N)" suffix, as Nix shows it.
pub fn io_text(e: &io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

impl Error {
    pub fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// The stable code of docs/reference/errors.md.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Io { .. } => "io",
            Error::Protocol(_) => "protocol",
            Error::Plan(_) => "plan",
            Error::Store(_) => "store",
            Error::Flake(_) => "flake",
            Error::Cache(_) => "cache",
            Error::Unsupported(_) => "unsupported",
            Error::Eval(e) => e.kind.code(),
            Error::Usage(_) => "usage",
            Error::Config(_) => "config",
            Error::Hint(e, _) => e.code(),
        }
    }

    pub fn to_report(&self) -> log::Report {
        let hint = match self {
            Error::Eval(e) => e.hint.clone(),
            Error::Hint(_, h) => Some(h.clone()),
            _ => None,
        };
        log::Report {
            hint,
            ..log::Report::new(self.code(), self.to_string())
        }
    }

    /// `error: MSG` on stderr, or its record; returns the exit code.
    pub fn report(&self) -> i32 {
        self.to_report().emit()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, source } if context.is_empty() => f.write_str(&io_text(source)),
            Error::Io { context, source } => write!(f, "{context}: {}", io_text(source)),
            Error::Protocol(m)
            | Error::Plan(m)
            | Error::Store(m)
            | Error::Flake(m)
            | Error::Cache(m)
            | Error::Unsupported(m)
            | Error::Usage(m)
            | Error::Config(m) => f.write_str(m),
            Error::Eval(e) => f.write_str(&e.msg),
            Error::Hint(e, _) => e.fmt(f),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Error {
        Error::Io {
            context: String::new(),
            source,
        }
    }
}

impl From<Box<error::Error>> for Error {
    fn from(e: Box<error::Error>) -> Error {
        Error::Eval(e)
    }
}

impl From<StorePathError> for Error {
    fn from(e: StorePathError) -> Error {
        Error::Store(e.0)
    }
}

impl From<DrvError> for Error {
    fn from(e: DrvError) -> Error {
        Error::Store(e.0)
    }
}

impl From<HashError> for Error {
    fn from(e: HashError) -> Error {
        Error::Store(e.0)
    }
}
