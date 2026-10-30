//! The cache key: a hash of everything about the command that can change
//! the result, computed before evaluation.

use sha2::{Digest, Sha256};

use crate::syntax;
use crate::syntax::ast::{Expr, FileId, PathKind};

/// Build id from `build.rs`: the hash of the sources this binary was built from.
pub const BUILD_ID: &str = env!("IETS_BUILD_ID");

pub struct Key {
    h: Sha256,
}

impl Key {
    pub fn new(subcommand: &str) -> Key {
        let mut k = Key { h: Sha256::new() };
        k.field("build", BUILD_ID.as_bytes());
        k.field("cmd", subcommand.as_bytes());
        k
    }

    /// One tagged, length-prefixed field; the order of calls is part of the key.
    pub fn field(&mut self, tag: &str, value: &[u8]) {
        self.h.update((tag.len() as u64).to_le_bytes());
        self.h.update(tag.as_bytes());
        self.h.update((value.len() as u64).to_le_bytes());
        self.h.update(value);
    }

    pub fn flag(&mut self, tag: &str, on: bool) {
        self.field(tag, &[u8::from(on)]);
    }

    pub fn finish(self) -> String {
        self.h
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Whether an expression text holds a relative path literal, so its meaning
/// depends on cwd. A text that does not parse counts as relative (the key
/// then includes cwd, which is harmless).
pub fn has_relative_path(src: &str) -> bool {
    let mut interner = crate::session::Session::current().interner();
    match syntax::parse(src.as_bytes(), FileId(0), &mut interner) {
        Ok(ast) => ast
            .exprs
            .iter()
            .any(|e| matches!(e, Expr::Path(p) if p.kind == PathKind::Relative)),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("import ./x.nix", true)]
    #[case("{ a = ../y; }", true)]
    #[case("import /abs/x.nix", false)]
    #[case("import <nixpkgs>", false)]
    #[case("\"./not-a-path\"", false)]
    #[case("let in (", true)]
    fn relative_path_detection(#[case] src: &str, #[case] want: bool) {
        assert_eq!(has_relative_path(src), want);
    }

    #[test]
    fn key_is_order_and_value_sensitive() {
        let mut a = Key::new("eval");
        a.field("x", b"1");
        a.field("y", b"2");
        let mut b = Key::new("eval");
        b.field("y", b"2");
        b.field("x", b"1");
        let mut c = Key::new("build");
        c.field("x", b"1");
        c.field("y", b"2");
        let (a, b, c) = (a.finish(), b.finish(), c.finish());
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }
}
