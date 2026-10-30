pub use iets_sys as sys;

pub mod allow;
pub mod ast;
pub mod dump;
pub mod interner;
pub mod lexer;
pub mod lints;
pub mod parser;
pub mod symbol;

#[cfg(test)]
mod tests;

use ast::{Ast, FileId, Span};
use symbol::Interner;

use std::error;
use std::fmt::{self, Display, Formatter};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub span: Span,
    pub msg: String,
}

impl Display for ParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl error::Error for ParseError {}

pub fn parse(src: &[u8], file: FileId, interner: &mut Interner) -> Result<Ast, ParseError> {
    parser::Parser::new(src, interner).parse_file(file)
}

/// 1-based line and column of a byte offset. The column counts bytes, like
/// Nix does.
pub fn line_col(src: &[u8], offset: u32) -> (u32, u32) {
    let offset = offset.min(src.len() as u32);
    let starts = line_starts(src);
    let line = starts.partition_point(|&s| s <= offset);
    (line as u32, offset - starts[line - 1] + 1)
}

/// The offset of each line's first byte. A line ends at `\n`, at `\r\n`
/// and at a lone `\r`, as Nix counts lines (position.cc,
/// `Pos::LinesIterator`).
pub fn line_starts(src: &[u8]) -> Vec<u32> {
    let mut starts = vec![0u32];
    let mut i = 0;
    while let Some(j) = src[i..].iter().position(|&b| b == b'\n' || b == b'\r') {
        let eol = i + j;
        i = if src[eol] == b'\r' && src.get(eol + 1) == Some(&b'\n') {
            eol + 2
        } else {
            eol + 1
        };
        starts.push(i as u32);
    }
    starts
}
