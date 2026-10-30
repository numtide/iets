//! `# iets: allow NAME[, NAME...]`: a comment that silences the named
//! lints (`all` and `family:NAME` too) for one source line, whatever
//! their level. On a line of its own it covers the next non-blank,
//! non-comment line; after code it covers its own line. The lexer notes
//! the comments, the parser resolves the names into this table, and the
//! compiler and the evaluator check a site's line before its level. A
//! name the registry does not know is the `unknown-allow` lint.

use super::lints::{LintId, N, select};

/// One lint per bit.
pub type Mask = u64;

const _: () = assert!(N <= 64);

#[inline]
pub fn bit(id: LintId) -> Mask {
    1 << (id as u8)
}

/// The byte range of one covered line and the lints allowed on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Line {
    pub start: u32,
    pub end: u32,
    pub mask: Mask,
}

/// The allowed lints of one file, by the line they cover. Sorted by
/// `start`, one entry per line; empty for nearly every file.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Allows {
    lines: Vec<Line>,
}

impl Allows {
    pub const EMPTY: &'static Allows = &Allows { lines: Vec::new() };

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// The table a stored entry holds, which the writer wrote in `start`
    /// order (`lines`). Taking it whole is linear where `add` per line is
    /// quadratic for an order the writer never produces; a table in any other
    /// order is not one this wrote, so it is no table and the caller drops
    /// the entry.
    pub fn from_sorted(lines: Vec<Line>) -> Option<Allows> {
        let ok = lines.windows(2).all(|w| w[0].start < w[1].start)
            && lines.iter().all(|l| l.mask != 0 && l.start < l.end);
        ok.then_some(Allows { lines })
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// Adds the allowed lints of the line `start..end`; a line given twice
    /// (an own-line comment above it and a trailing one on it) keeps both.
    pub fn add(&mut self, start: u32, end: u32, mask: Mask) {
        if mask == 0 || start >= end {
            return;
        }
        let i = self.lines.partition_point(|l| l.start < start);
        if let Some(l) = self.lines.get_mut(i)
            && l.start == start
        {
            l.mask |= mask;
            l.end = l.end.max(end);
            return;
        }
        self.lines.insert(i, Line { start, end, mask });
    }

    /// Whether `id` is allowed at byte offset `off` of the file.
    #[inline]
    pub fn covers(&self, off: u32, id: LintId) -> bool {
        if self.lines.is_empty() {
            return false;
        }
        self.covers_slow(off, id)
    }

    #[inline(never)]
    fn covers_slow(&self, off: u32, id: LintId) -> bool {
        let i = self.lines.partition_point(|l| l.start <= off);
        match i.checked_sub(1).map(|i| self.lines[i]) {
            Some(l) => off < l.end && l.mask & bit(id) != 0,
            None => false,
        }
    }
}

/// The text of one comment after `# iets:`: the mask of the lints it
/// names, and one `unknown-allow` message per name (or directive) it
/// gets wrong, with the byte offset of that word inside `text`.
pub fn parse_comment(text: &[u8]) -> (Mask, Vec<(u32, String)>) {
    let mut words = text
        .split(|b| matches!(b, b' ' | b'\t' | b','))
        .filter(|w| !w.is_empty())
        .map(|w| (w.as_ptr() as usize - text.as_ptr() as usize, w));
    let Some((at, directive)) = words.next() else {
        return (
            0,
            vec![(
                0,
                "empty `# iets:` comment: use `allow NAME[, NAME...]`".into(),
            )],
        );
    };
    if directive != b"allow" {
        let d = String::from_utf8_lossy(directive);
        return (
            0,
            vec![(
                at as u32,
                format!(
                    "unknown directive '{d}' in a `# iets:` comment: use `allow NAME[, NAME...]`"
                ),
            )],
        );
    }
    let mut mask = 0;
    let mut bad = Vec::new();
    let mut any = false;
    for (off, word) in words {
        any = true;
        match select(&String::from_utf8_lossy(word)) {
            Ok((_, ids)) => {
                for id in ids {
                    mask |= bit(id);
                }
            }
            Err(e) => bad.push((off as u32, format!("{e} in `# iets: allow`"))),
        }
    }
    if !any {
        bad.push((at as u32, "`# iets: allow` names no lint".into()));
    }
    (mask, bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_names() {
        let (m, bad) = parse_comment(b" allow ifd, fod  family:path");
        assert_eq!(
            m,
            bit(LintId::Ifd)
                | bit(LintId::Fod)
                | bit(LintId::AbsolutePathLiteral)
                | bit(LintId::ShortPathLiteral)
        );
        assert!(bad.is_empty());
        let (m, bad) = parse_comment(b" allow all");
        assert_eq!(m.count_ones() as usize, N);
        assert!(bad.is_empty());
        let (m, bad) = parse_comment(b" allow fdo, fod");
        assert_eq!(m, bit(LintId::Fod));
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].0, 7);
        assert!(bad[0].1.contains("unknown lint 'fdo'"));
        let (m, bad) = parse_comment(b" allo fod");
        assert_eq!(m, 0);
        assert!(bad[0].1.contains("unknown directive 'allo'"));
        let (m, bad) = parse_comment(b" allow");
        assert_eq!(m, 0);
        assert!(bad[0].1.contains("names no lint"));
        assert!(parse_comment(b"").1[0].1.contains("empty"));
    }

    #[test]
    fn lines() {
        let mut a = Allows::default();
        assert!(!a.covers(0, LintId::PathCopy));
        a.add(10, 20, bit(LintId::PathCopy));
        a.add(30, 40, bit(LintId::EqFunction));
        a.add(10, 20, bit(LintId::Fod));
        a.add(5, 5, bit(LintId::Ifd));
        assert_eq!(a.lines().len(), 2);
        assert!(a.covers(10, LintId::PathCopy));
        assert!(a.covers(19, LintId::Fod));
        assert!(!a.covers(20, LintId::PathCopy));
        assert!(!a.covers(9, LintId::PathCopy));
        assert!(!a.covers(15, LintId::EqFunction));
        assert!(a.covers(35, LintId::EqFunction));
        assert!(!a.covers(45, LintId::EqFunction));
    }
}
