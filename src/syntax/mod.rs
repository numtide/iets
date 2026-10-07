pub mod ast;
pub mod dump;
pub mod lexer;
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
    let offset = (offset as usize).min(src.len());
    let mut line = 1u32;
    let mut line_start = 0usize;
    for (i, &b) in src[..offset].iter().enumerate() {
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    (line, (offset - line_start) as u32 + 1)
}
