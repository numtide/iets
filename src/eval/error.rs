use crate::store::hash::Hash;
use std::fmt;
use std::sync::{Arc, Mutex};

/// Byte offset into the concatenation of all loaded sources; the evaluator
/// maps it back to a file and a local offset. 4 bytes so `Attr` and every IR
/// node stay small.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Pos(pub u32);

impl Pos {
    pub const NONE: Pos = Pos(u32::MAX);

    #[inline]
    pub fn is_none(self) -> bool {
        self.0 == u32::MAX
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorKind {
    Eval,
    Type,
    /// `builtins.throw`. Caught by `tryEval`.
    Throw,
    /// Failed `assert`. Caught by `tryEval`.
    Assert,
    /// `builtins.abort`. Never caught.
    Abort,
    UndefinedVar,
    InfiniteRecursion,
    StackOverflow,
    Parse,
    MissingAttr,
    Io,
    /// A parallel helper fiber was told to stop. Never caught.
    Cancelled,
    /// A feature iets does not implement (network fetchers, flakes without a
    /// lock, import-from-derivation). Classic mode turns this into an exec of
    /// the real nix; see `crate::cli::classic`.
    Unsupported,
    /// A lint at level `error`. Never caught by `tryEval`.
    Lint,
    /// A fetched tree whose hash is not the one the lock or the call named.
    HashMismatch,
}

impl ErrorKind {
    /// The stable code of docs/reference/errors.md.
    pub fn code(self) -> &'static str {
        match self {
            ErrorKind::Eval => "eval",
            ErrorKind::Type => "type",
            ErrorKind::Throw => "throw",
            ErrorKind::Assert => "assert",
            ErrorKind::Abort => "abort",
            ErrorKind::UndefinedVar => "undefined-var",
            ErrorKind::InfiniteRecursion => "infinite-recursion",
            ErrorKind::StackOverflow => "stack-overflow",
            ErrorKind::Parse => "parse",
            ErrorKind::MissingAttr => "missing-attr",
            ErrorKind::Io => "io",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Lint => "lint",
            ErrorKind::HashMismatch => "hash-mismatch",
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub msg: String,
    pub pos: Pos,
    pub trace: Vec<(Pos, String)>,
    /// The fix, when it is certain; the text shows it only for a usage or
    /// unsupported error (docs/reference/errors.md).
    pub hint: Option<String>,
    // The original exception keeps collecting caller frames as it unwinds.
    // A rethrow starts a separate exception, with a copy of those frames.
    recorded: Option<Arc<Mutex<Error>>>,
}

/// A thunk state stores a session-local index of the original exception.
#[derive(Default)]
pub(crate) struct FailedThunks(Mutex<Vec<Arc<Mutex<Error>>>>);

impl FailedThunks {
    pub(crate) fn record(&self, error: &mut Error) -> usize {
        let mut entries = self.0.lock().unwrap();
        if let Some(saved) = &error.recorded
            && let Some(last) = entries.last()
            && Arc::ptr_eq(saved, last)
        {
            return entries.len() - 1;
        }
        let saved = match &error.recorded {
            Some(saved) => Arc::clone(saved),
            None => {
                let saved = Arc::new(Mutex::new(error.detached()));
                error.recorded = Some(Arc::clone(&saved));
                saved
            }
        };
        entries.push(saved);
        entries.len() - 1
    }

    pub(crate) fn rethrow(&self, index: usize) -> Box<Error> {
        Box::new(self.0.lock().unwrap()[index].lock().unwrap().detached())
    }
}

impl Error {
    pub fn new(kind: ErrorKind, msg: String, pos: Pos) -> Error {
        Error {
            kind,
            msg,
            pos,
            trace: Vec::new(),
            hint: None,
            recorded: None,
        }
    }

    #[cold]
    #[inline(never)]
    pub fn boxed(kind: ErrorKind, msg: impl Into<String>, pos: Pos) -> Box<Error> {
        Box::new(Error::new(kind, msg.into(), pos))
    }

    /// `boxed` for `bail!`: the message is formatted here, out of line, so
    /// the hot callers keep only the arguments in their code.
    #[cold]
    #[inline(never)]
    pub fn boxed_fmt(kind: ErrorKind, args: fmt::Arguments<'_>, pos: Pos) -> Box<Error> {
        Box::new(Error::new(kind, fmt::format(args), pos))
    }

    pub fn stack_overflow(pos: Pos) -> Box<Error> {
        Error::boxed(
            ErrorKind::StackOverflow,
            "stack overflow; max-call-depth exceeded",
            pos,
        )
    }

    /// A tree that hashes to `got`, not to what the call or the lock named;
    /// `at` is where the hash literal is, when that is known.
    pub fn hash_mismatch(msg: String, got: &Hash, at: Option<String>, pos: Pos) -> Box<Error> {
        let mut e = Error::new(ErrorKind::HashMismatch, msg, pos);
        let at = at.map(|p| format!(" at {p}")).unwrap_or_default();
        e.hint = Some(format!("set narHash{at} to '{}'", got.to_sri()));
        Box::new(e)
    }

    pub fn add_trace(&mut self, pos: Pos, msg: impl Into<String>) {
        let msg = msg.into();
        if let Some(r) = &self.recorded {
            r.lock().unwrap().trace.push((pos, msg.clone()));
        }
        self.trace.push((pos, msg));
    }

    fn detached(&self) -> Self {
        Self {
            kind: self.kind,
            pos: self.pos,
            msg: self.msg.clone(),
            hint: self.hint.clone(),
            trace: self.trace.clone(),
            recorded: None,
        }
    }

    /// Errors that `tryEval` turns into `{ success = false; }`.
    pub fn is_catchable(&self) -> bool {
        matches!(self.kind, ErrorKind::Throw | ErrorKind::Assert)
    }
}

pub type EvalResult<T> = Result<T, Box<Error>>;
