//! Hand-written lexer over raw bytes. It mirrors the token rules of Nix's
//! `lexer.l`, including flex's longest-match choice between identifiers,
//! paths, URIs and numbers. String, indented-string and path bodies are
//! lexed on demand by the parser through the `lex_*` methods, because the
//! parser must handle `${ ... }` interpolation in between.

use super::ParseError;
use super::ast::Span;

use std::mem;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Id,
    Int,
    Float,
    /// `./x`, `/x`, `x/y`. The text may end with `/` when an interpolation
    /// or a trailing-slash error follows.
    Path,
    /// `~/x`.
    HPath,
    /// `<x/y>`.
    SPath,
    Uri,
    If,
    Then,
    Else,
    Assert,
    With,
    Let,
    In,
    Rec,
    Inherit,
    Or,
    Ellipsis,
    Eq,
    Ne,
    Le,
    Ge,
    AndAnd,
    OrOr,
    Impl,
    Update,
    Concat,
    PipeFrom,
    PipeInto,
    DollarCurly,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Dot,
    Semi,
    Comma,
    Colon,
    At,
    Question,
    Assign,
    Bang,
    Minus,
    Plus,
    Star,
    Slash,
    Lt,
    Gt,
    /// `"`; the parser lexes the body with `lex_str_chunk`.
    DQuote,
    /// `''` plus the optional spaces-and-newline that follow it.
    IndOpen,
    Eof,
    Invalid,
}

impl Tok {
    pub fn describe(self) -> &'static str {
        match self {
            Tok::Id => "identifier",
            Tok::Int => "integer",
            Tok::Float => "floating-point literal",
            Tok::Path => "path",
            Tok::HPath => "'~/…' path",
            Tok::SPath => "'<…>' path",
            Tok::Uri => "URI",
            Tok::If => "'if'",
            Tok::Then => "'then'",
            Tok::Else => "'else'",
            Tok::Assert => "'assert'",
            Tok::With => "'with'",
            Tok::Let => "'let'",
            Tok::In => "'in'",
            Tok::Rec => "'rec'",
            Tok::Inherit => "'inherit'",
            Tok::Or => "'or'",
            Tok::Ellipsis => "'...'",
            Tok::Eq => "'=='",
            Tok::Ne => "'!='",
            Tok::Le => "'<='",
            Tok::Ge => "'>='",
            Tok::AndAnd => "'&&'",
            Tok::OrOr => "'||'",
            Tok::Impl => "'->'",
            Tok::Update => "'//'",
            Tok::Concat => "'++'",
            Tok::PipeFrom => "'<|'",
            Tok::PipeInto => "'|>'",
            Tok::DollarCurly => "'${'",
            Tok::LBrace => "'{'",
            Tok::RBrace => "'}'",
            Tok::LBracket => "'['",
            Tok::RBracket => "']'",
            Tok::LParen => "'('",
            Tok::RParen => "')'",
            Tok::Dot => "'.'",
            Tok::Semi => "';'",
            Tok::Comma => "','",
            Tok::Colon => "':'",
            Tok::At => "'@'",
            Tok::Question => "'?'",
            Tok::Assign => "'='",
            Tok::Bang => "'!'",
            Tok::Minus => "'-'",
            Tok::Plus => "'+'",
            Tok::Star => "'*'",
            Tok::Slash => "'/'",
            Tok::Lt => "'<'",
            Tok::Gt => "'>'",
            Tok::DQuote => "'\"'",
            Tok::IndOpen => "start of an indented string",
            Tok::Eof => "end of file",
            Tok::Invalid => "invalid token",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: Tok,
    pub start: u32,
    pub end: u32,
}

impl Token {
    pub fn span(&self) -> Span {
        Span {
            start: self.start,
            end: self.end,
        }
    }
}

/// Result of lexing one literal run inside a `"..."` string.
pub enum StrEnd {
    Interp,
    Close,
}

/// One token inside an indented string, before indentation stripping.
#[derive(Clone, Copy, Debug)]
pub enum IndPart {
    /// Raw source bytes; participates in indentation measurement.
    Src(u32, u32),
    /// `''$`, `'''`, `''\x`, lone `$` or `'`; never counts as indentation.
    Lit(u8, u8, u8),
    Interp,
}

pub enum PathPart {
    Str(u32, u32),
    Interp,
    End,
}

/// A `# iets:` comment: its text after the marker, and the byte range of
/// the line it covers (docs/reference/lints.md, "Silencing one site").
#[derive(Clone, Copy, Debug)]
pub struct Allow {
    pub text: Span,
    pub line: Span,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    /// `# iets:` comments as `#`-start and end offsets; `take_allows`
    /// resolves their lines, so the token path pays nothing for them.
    allows: Vec<(usize, usize)>,
    /// End of the last `# iets:` comment noted: the parser rewinds for
    /// its third token of lookahead, and a comment is noted once.
    noted_to: usize,
}

#[inline]
fn is_id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'\'' || b == b'-'
}

#[inline]
fn is_path_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' || b == b'+'
}

#[inline]
fn is_uri_scheme_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'
}

#[inline]
fn is_uri_char(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'%' | b'/'
                | b'?'
                | b':'
                | b'@'
                | b'&'
                | b'='
                | b'+'
                | b'$'
                | b','
                | b'-'
                | b'_'
                | b'.'
                | b'!'
                | b'~'
                | b'*'
                | b'\''
        )
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a [u8]) -> Self {
        Lexer {
            src,
            pos: 0,
            allows: Vec::new(),
            noted_to: 0,
        }
    }

    /// The `# iets:` comments lexed so far, in source order, each with
    /// the line it covers: after code its own line; on a line of its own
    /// the line of the next token, found by skipping trivia again from the
    /// comment's end (an own-line comment at the end of the file covers
    /// nothing).
    pub fn take_allows(&mut self) -> Vec<Allow> {
        let raw = mem::take(&mut self.allows);
        raw.into_iter()
            .filter_map(|(start, end)| {
                let text = Span {
                    start: (start + "# iets:".len()) as u32,
                    end: end as u32,
                };
                let mut i = start;
                while i > 0 && matches!(self.src[i - 1], b' ' | b'\t') {
                    i -= 1;
                }
                let own_line = i == 0 || self.src[i - 1] == b'\n' || self.src[i - 1] == b'\r';
                let line = if own_line {
                    let save = self.pos;
                    self.pos = end;
                    self.skip_trivia();
                    let p = self.pos;
                    self.pos = save;
                    if p >= self.src.len() {
                        return None;
                    }
                    Span {
                        start: self.line_start(p) as u32,
                        end: self.line_end(p) as u32,
                    }
                } else {
                    Span {
                        start: self.line_start(start) as u32,
                        end: end as u32,
                    }
                };
                Some(Allow { text, line })
            })
            .collect()
    }

    /// Start of the line holding `p`.
    fn line_start(&self, p: usize) -> usize {
        let mut i = p;
        while i > 0 && self.src[i - 1] != b'\n' {
            i -= 1;
        }
        i
    }

    /// End of the line holding `p`: its newline, or the end of the file.
    fn line_end(&self, p: usize) -> usize {
        let mut i = p;
        while i < self.src.len() && self.src[i] != b'\n' && self.src[i] != b'\r' {
            i += 1;
        }
        i
    }

    /// A `#` comment to the end of its line, noted when it is `# iets:`.
    /// Out of line so `next_token` stays small enough for its callers to
    /// inline it: as part of `skip_trivia` this arm cost 1.5 % of the
    /// instructions of a parse of nixpkgs.
    #[inline(never)]
    fn comment(&mut self) {
        let src = self.src;
        let start = self.pos;
        let mut i = start;
        while i < src.len() && src[i] != b'\n' && src[i] != b'\r' {
            i += 1;
        }
        self.pos = i;
        if self.at(start + 2) == b'i'
            && src[start..i].starts_with(b"# iets:")
            && start >= self.noted_to
        {
            self.noted_to = i;
            self.allows.push((start, i));
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    #[inline]
    fn at(&self, i: usize) -> u8 {
        if i < self.src.len() { self.src[i] } else { 0 }
    }

    /// Inlined into `next_token` whatever its other callers: as a call it
    /// cost 5 % of the parse of nixpkgs.
    #[inline(always)]
    fn skip_trivia(&mut self) {
        let src = self.src;
        while self.pos < src.len() {
            match src[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                b'#' => self.comment(),
                b'/' if self.at(self.pos + 1) == b'*' => {
                    let mut i = self.pos + 2;
                    loop {
                        if i + 1 >= src.len() {
                            return;
                        }
                        if src[i] == b'*' && src[i + 1] == b'/' {
                            self.pos = i + 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// `PATH_CHAR* ('/' PATH_CHAR+)+ '/'?` starting at `p`.
    fn scan_path(&self, p: usize) -> Option<usize> {
        let mut i = p;
        while is_path_char(self.at(i)) {
            i += 1;
        }
        if self.at(i) != b'/' {
            return None;
        }
        let mut end = None;
        loop {
            let seg = i + 1;
            let mut j = seg;
            while is_path_char(self.at(j)) {
                j += 1;
            }
            if j == seg {
                return end.map(|_| i + 1);
            }
            end = Some(j);
            i = j;
            if self.at(i) != b'/' {
                return end;
            }
        }
    }

    /// Longest of PATH or `PATH_SEG` followed by `${`; returns the token end.
    fn scan_path_token(&self, p: usize) -> Option<usize> {
        let mut i = p;
        while is_path_char(self.at(i)) {
            i += 1;
        }
        if self.at(i) != b'/' {
            return None;
        }
        if let Some(e) = self.scan_path(p) {
            return Some(e);
        }
        if self.at(i + 1) == b'$' && self.at(i + 2) == b'{' {
            return Some(i + 1);
        }
        None
    }

    /// `~ ('/' PATH_CHAR+)+ '/'?` or `~/` followed by `${`.
    fn scan_hpath(&self, p: usize) -> Option<usize> {
        if self.at(p) != b'~' || self.at(p + 1) != b'/' {
            return None;
        }
        if let Some(e) = self.scan_path(p + 1) {
            return Some(e);
        }
        if self.at(p + 2) == b'$' && self.at(p + 3) == b'{' {
            return Some(p + 2);
        }
        None
    }

    fn scan_spath(&self, p: usize) -> Option<usize> {
        let mut i = p + 1;
        let start = i;
        while is_path_char(self.at(i)) {
            i += 1;
        }
        if i == start {
            return None;
        }
        while self.at(i) == b'/' {
            let seg = i + 1;
            let mut j = seg;
            while is_path_char(self.at(j)) {
                j += 1;
            }
            if j == seg {
                return None;
            }
            i = j;
        }
        if self.at(i) == b'>' {
            Some(i + 1)
        } else {
            None
        }
    }

    fn scan_uri(&self, p: usize) -> Option<usize> {
        if !self.at(p).is_ascii_alphabetic() {
            return None;
        }
        let mut i = p + 1;
        while is_uri_scheme_char(self.at(i)) {
            i += 1;
        }
        if self.at(i) != b':' {
            return None;
        }
        i += 1;
        let body = i;
        while is_uri_char(self.at(i)) {
            i += 1;
        }
        if i == body { None } else { Some(i) }
    }

    /// Longest match: `kind` up to `end`, unless a float or a path that starts
    /// at `start` runs further.
    fn or_float_or_path(&self, start: usize, mut kind: Tok, mut end: usize) -> (Tok, usize) {
        if let Some(e) = self.scan_float(start)
            && e > end
        {
            kind = Tok::Float;
            end = e;
        }
        if let Some(e) = self.scan_path_token(start)
            && e > end
        {
            kind = Tok::Path;
            end = e;
        }
        (kind, end)
    }

    fn scan_float(&self, p: usize) -> Option<usize> {
        let mut i = p;
        let b = self.at(i);
        if (b'1'..=b'9').contains(&b) {
            i += 1;
            while self.at(i).is_ascii_digit() {
                i += 1;
            }
            if self.at(i) != b'.' {
                return None;
            }
            i += 1;
            while self.at(i).is_ascii_digit() {
                i += 1;
            }
        } else {
            if b == b'0' {
                i += 1;
            }
            if self.at(i) != b'.' {
                return None;
            }
            i += 1;
            let start = i;
            while self.at(i).is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return None;
            }
        }
        if self.at(i) == b'e' || self.at(i) == b'E' {
            let mut j = i + 1;
            if self.at(j) == b'+' || self.at(j) == b'-' {
                j += 1;
            }
            let digits = j;
            while self.at(j).is_ascii_digit() {
                j += 1;
            }
            if j > digits {
                i = j;
            }
        }
        Some(i)
    }

    fn keyword(text: &[u8]) -> Tok {
        match text {
            b"if" => Tok::If,
            b"then" => Tok::Then,
            b"else" => Tok::Else,
            b"assert" => Tok::Assert,
            b"with" => Tok::With,
            b"let" => Tok::Let,
            b"in" => Tok::In,
            b"rec" => Tok::Rec,
            b"inherit" => Tok::Inherit,
            b"or" => Tok::Or,
            _ => Tok::Id,
        }
    }

    /// Next token in normal (expression) mode.
    pub fn next_token(&mut self) -> Token {
        self.skip_trivia();
        let start = self.pos;
        let tok = |kind: Tok, end: usize| Token {
            kind,
            start: start as u32,
            end: end as u32,
        };
        if start >= self.src.len() {
            return tok(Tok::Eof, start);
        }
        let b = self.src[start];
        let (kind, end) = match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let mut i = start + 1;
                while is_id_char(self.at(i)) {
                    i += 1;
                }
                let mut kind = Tok::Id;
                let mut end = i;
                let next = self.at(i);
                if matches!(next, b':' | b'.' | b'+' | b'/') {
                    // A path needs a `/` and a URI a `:` right after the run
                    // of path chars from `start`; `a.b.c` chains have neither
                    // and skip both scans.
                    let mut k = start;
                    while is_path_char(self.at(k)) {
                        k += 1;
                    }
                    let after = self.at(k);
                    if after == b'/'
                        && let Some(e) = self.scan_path_token(start)
                        && e > end
                    {
                        kind = Tok::Path;
                        end = e;
                    }
                    if after == b':'
                        && let Some(e) = self.scan_uri(start)
                        && e > end
                    {
                        kind = Tok::Uri;
                        end = e;
                    }
                }
                if kind == Tok::Id {
                    kind = Self::keyword(&self.src[start..end]);
                }
                (kind, end)
            }
            b'0'..=b'9' => {
                let mut i = start + 1;
                while self.at(i).is_ascii_digit() {
                    i += 1;
                }
                self.or_float_or_path(start, Tok::Int, i)
            }
            b'.' => {
                let mut kind = Tok::Dot;
                let mut end = start + 1;
                if self.at(start + 1) == b'.' && self.at(start + 2) == b'.' {
                    kind = Tok::Ellipsis;
                    end = start + 3;
                }
                self.or_float_or_path(start, kind, end)
            }
            b'/' => {
                if let Some(e) = self.scan_path_token(start) {
                    (Tok::Path, e)
                } else if self.at(start + 1) == b'/' {
                    (Tok::Update, start + 2)
                } else {
                    (Tok::Slash, start + 1)
                }
            }
            b'-' => {
                if let Some(e) = self.scan_path_token(start) {
                    (Tok::Path, e)
                } else if self.at(start + 1) == b'>' {
                    (Tok::Impl, start + 2)
                } else {
                    (Tok::Minus, start + 1)
                }
            }
            b'+' => {
                if let Some(e) = self.scan_path_token(start) {
                    (Tok::Path, e)
                } else if self.at(start + 1) == b'+' {
                    (Tok::Concat, start + 2)
                } else {
                    (Tok::Plus, start + 1)
                }
            }
            b'~' => match self.scan_hpath(start) {
                Some(e) => (Tok::HPath, e),
                None => (Tok::Invalid, start + 1),
            },
            b'<' => {
                if let Some(e) = self.scan_spath(start) {
                    (Tok::SPath, e)
                } else {
                    match self.at(start + 1) {
                        b'=' => (Tok::Le, start + 2),
                        b'|' => (Tok::PipeFrom, start + 2),
                        _ => (Tok::Lt, start + 1),
                    }
                }
            }
            b'>' => {
                if self.at(start + 1) == b'=' {
                    (Tok::Ge, start + 2)
                } else {
                    (Tok::Gt, start + 1)
                }
            }
            b'=' => {
                if self.at(start + 1) == b'=' {
                    (Tok::Eq, start + 2)
                } else {
                    (Tok::Assign, start + 1)
                }
            }
            b'!' => {
                if self.at(start + 1) == b'=' {
                    (Tok::Ne, start + 2)
                } else {
                    (Tok::Bang, start + 1)
                }
            }
            b'&' => {
                if self.at(start + 1) == b'&' {
                    (Tok::AndAnd, start + 2)
                } else {
                    (Tok::Invalid, start + 1)
                }
            }
            b'|' => match self.at(start + 1) {
                b'|' => (Tok::OrOr, start + 2),
                b'>' => (Tok::PipeInto, start + 2),
                _ => (Tok::Invalid, start + 1),
            },
            b'$' => {
                if self.at(start + 1) == b'{' {
                    (Tok::DollarCurly, start + 2)
                } else {
                    (Tok::Invalid, start + 1)
                }
            }
            b'\'' => {
                if self.at(start + 1) == b'\'' {
                    let mut i = start + 2;
                    while self.at(i) == b' ' {
                        i += 1;
                    }
                    if self.at(i) == b'\n' && i < self.src.len() {
                        (Tok::IndOpen, i + 1)
                    } else {
                        (Tok::IndOpen, start + 2)
                    }
                } else {
                    (Tok::Invalid, start + 1)
                }
            }
            b'"' => (Tok::DQuote, start + 1),
            b'{' => (Tok::LBrace, start + 1),
            b'}' => (Tok::RBrace, start + 1),
            b'[' => (Tok::LBracket, start + 1),
            b']' => (Tok::RBracket, start + 1),
            b'(' => (Tok::LParen, start + 1),
            b')' => (Tok::RParen, start + 1),
            b';' => (Tok::Semi, start + 1),
            b',' => (Tok::Comma, start + 1),
            b':' => (Tok::Colon, start + 1),
            b'@' => (Tok::At, start + 1),
            b'?' => (Tok::Question, start + 1),
            b'*' => (Tok::Star, start + 1),
            _ => (Tok::Invalid, start + 1),
        };
        self.pos = end;
        tok(kind, end)
    }

    /// Lex literal text of a `"..."` string into `out` until `${` or the
    /// closing quote. Escapes are decoded and CR / CRLF become LF.
    pub fn lex_str_chunk(&mut self, out: &mut Vec<u8>) -> Result<StrEnd, ParseError> {
        let (start, base) = (self.pos, out.len());
        let end = self.str_chunk(out);
        // lexer.l:unescapeStr tests the raw bytes, so an escaped NUL passes
        let mut i = start;
        while i < self.pos {
            match self.src[i] {
                b'\\' => i += 2,
                0 => return Err(nul_error(start, &out[base..])),
                _ => i += 1,
            }
        }
        end
    }

    fn str_chunk(&mut self, out: &mut Vec<u8>) -> Result<StrEnd, ParseError> {
        let src = self.src;
        loop {
            if self.pos >= src.len() {
                return Err(self.eof_error("expecting '\"'"));
            }
            let b = src[self.pos];
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(StrEnd::Close);
                }
                b'\\' => {
                    self.pos += 1;
                    if self.pos >= src.len() {
                        return Err(self.eof_error("expecting '\"'"));
                    }
                    out.push(unescape(src[self.pos]));
                    self.pos += 1;
                }
                b'\r' => {
                    out.push(b'\n');
                    self.pos += 1;
                    if self.at(self.pos) == b'\n' {
                        self.pos += 1;
                    }
                }
                b'$' => {
                    self.pos += 1;
                    match self.at(self.pos) {
                        b'{' => {
                            self.pos += 1;
                            return Ok(StrEnd::Interp);
                        }
                        b'"' | b'\\' => out.push(b'$'),
                        _ if self.pos >= src.len() => {
                            return Err(self.eof_error("expecting '\"'"));
                        }
                        b'\r' => {
                            out.push(b'$');
                            out.push(b'\n');
                            self.pos += 1;
                            if self.at(self.pos) == b'\n' {
                                self.pos += 1;
                            }
                        }
                        c => {
                            out.push(b'$');
                            out.push(c);
                            self.pos += 1;
                        }
                    }
                }
                _ => {
                    let run_start = self.pos;
                    let mut i = self.pos + 1;
                    while i < src.len() && !matches!(src[i], b'"' | b'\\' | b'\r' | b'$') {
                        i += 1;
                    }
                    out.extend_from_slice(&src[run_start..i]);
                    self.pos = i;
                }
            }
        }
    }

    /// Lex tokens of an indented string into `parts` until `${` or `''`.
    pub fn lex_ind_chunk(&mut self, parts: &mut Vec<IndPart>) -> Result<StrEnd, ParseError> {
        let src = self.src;
        loop {
            if self.pos >= src.len() {
                return Err(self.eof_error("expecting end of an indented string"));
            }
            let b = src[self.pos];
            if b == b'\'' && self.at(self.pos + 1) == b'\'' {
                let c = self.at(self.pos + 2);
                if self.pos + 2 < src.len() {
                    match c {
                        b'$' => {
                            parts.push(IndPart::Lit(b'$', 0, 1));
                            self.pos += 3;
                            continue;
                        }
                        b'\'' => {
                            parts.push(IndPart::Lit(b'\'', b'\'', 2));
                            self.pos += 3;
                            continue;
                        }
                        b'\\' if self.pos + 3 < src.len() => {
                            parts.push(IndPart::Lit(unescape(src[self.pos + 3]), 0, 1));
                            self.pos += 4;
                            continue;
                        }
                        _ => {}
                    }
                }
                self.pos += 2;
                return Ok(StrEnd::Close);
            }
            if b == b'$' && self.at(self.pos + 1) == b'{' {
                self.pos += 2;
                return Ok(StrEnd::Interp);
            }
            let start = self.pos;
            let mut i = start;
            while i < src.len() {
                match src[i] {
                    b'\'' => {
                        let n = self.at(i + 1);
                        if n == b'\'' || n == b'$' || i + 1 >= src.len() {
                            break;
                        }
                        i += 2;
                    }
                    b'$' => {
                        let n = self.at(i + 1);
                        if n == b'{' || n == b'\'' || i + 1 >= src.len() {
                            break;
                        }
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            if i > start {
                if src[start..i].contains(&0) {
                    return Err(nul_error(start, &src[start..i]));
                }
                parts.push(IndPart::Src(start as u32, i as u32));
                self.pos = i;
                continue;
            }
            parts.push(IndPart::Lit(b, 0, 1));
            self.pos += 1;
        }
    }

    /// Continue a path after its first token or after `}` of an
    /// interpolation. `after_slash` is true when the previous piece ended
    /// with `/`.
    pub fn lex_path_part(&mut self, after_slash: bool) -> Result<PathPart, ParseError> {
        let p = self.pos;
        if self.at(p) == b'$' && self.at(p + 1) == b'{' {
            self.pos = p + 2;
            return Ok(PathPart::Interp);
        }
        let end = match self.scan_path(p) {
            Some(e) => Some(e),
            None => {
                let mut i = p;
                while is_path_char(self.at(i)) {
                    i += 1;
                }
                if self.at(i) == b'/' {
                    Some(i + 1)
                } else if i > p {
                    Some(i)
                } else {
                    None
                }
            }
        };
        match end {
            Some(e) => {
                self.pos = e;
                Ok(PathPart::Str(p as u32, e as u32))
            }
            None if after_slash => Err(ParseError {
                span: Span {
                    start: p as u32,
                    end: p as u32,
                },
                msg: "path has a trailing slash".into(),
            }),
            None => Ok(PathPart::End),
        }
    }

    fn eof_error(&self, expecting: &str) -> ParseError {
        let p = self.src.len() as u32;
        ParseError {
            span: Span { start: p, end: p },
            msg: format!("syntax error, unexpected end of file, {expecting}"),
        }
    }
}

/// `eval.cc:forceNoNullByte`.
pub fn no_nul(s: &[u8]) -> Result<(), String> {
    if !s.contains(&0) {
        return Ok(());
    }
    let s = String::from_utf8_lossy(s).replace('\0', "␀");
    Err(format!(
        "input string '{s}' cannot be represented as Nix string because it contains null bytes"
    ))
}

fn nul_error(at: usize, s: &[u8]) -> ParseError {
    let at = at as u32;
    ParseError {
        span: Span { start: at, end: at },
        msg: no_nul(s).unwrap_err(),
    }
}

#[inline]
fn unescape(c: u8) -> u8 {
    match c {
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        c => c,
    }
}
