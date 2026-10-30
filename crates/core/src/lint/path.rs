//! The path-literal lints Nix has (`lint-absolute-path-literals`,
//! `lint-short-path-literals`), with Nix's messages. Hook: the compiler,
//! at every path literal.

use super::LintId;
use crate::syntax::ast::PathKind;

/// The lint and message a path literal triggers, if any. `lit` is the
/// literal text after the kind prefix.
pub fn path_literal(kind: PathKind, lit: &[u8]) -> Option<(LintId, String)> {
    let lit = String::from_utf8_lossy(lit);
    match kind {
        PathKind::Absolute => Some((
            LintId::AbsolutePathLiteral,
            format!(
                "absolute path literals are not portable. Consider replacing path literal '{lit}' by a string, relative path, or parameter"
            ),
        )),
        PathKind::Home => Some((
            LintId::AbsolutePathLiteral,
            format!(
                "home path literals are not portable. Consider replacing path literal '~{lit}' by a string, relative path, or parameter"
            ),
        )),
        PathKind::Relative if !lit.starts_with('.') => Some((
            LintId::ShortPathLiteral,
            format!(
                "relative path literal '{lit}' should be prefixed with '.' for clarity: './{lit}'"
            ),
        )),
        _ => None,
    }
}
