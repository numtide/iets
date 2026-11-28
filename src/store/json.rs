//! The `__json` text of a `.drv` as Nix reads and writes it again:
//! `parsed-derivations.cc:StructuredAttrs::parse`, then `dump()`.
//! nlohmann::json keeps the keys of an object in a `std::map`, so the text
//! comes back with its keys sorted bytewise and the last of two equal keys.

use std::collections::BTreeMap;
use std::str;

use crate::json_string::unicode_escape;
use crate::print::{json_escape_str, json_float};

use super::drv::DrvError;

/// One value of the document, with its children as indices into the arena:
/// a deep value must not recurse, neither to parse nor to write nor to drop.
enum Node {
    /// A number, string, boolean or null, already in the form Nix writes.
    Scalar(String),
    Array(Vec<usize>),
    Object(BTreeMap<String, usize>),
}

/// The `__json` value of a `.drv` in the form Nix writes it, or Nix's
/// error.
pub fn normalise_structured_attrs(text: &[u8]) -> Result<String, DrvError> {
    let mut p = Parser {
        s: text,
        i: 0,
        nodes: Vec::new(),
    };
    let root = p.document()?;
    let kind = match &p.nodes[root] {
        Node::Object(_) => return Ok(p.write(root)),
        Node::Array(_) => "array",
        Node::Scalar(s) => match s.as_bytes()[0] {
            b'n' => "null",
            b't' | b'f' => "boolean",
            b'"' => "string",
            _ => "number",
        },
    };
    Err(DrvError(format!(
        "cannot process __json attribute: [json.exception.type_error.302] type must be object, but is {kind}"
    )))
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    nodes: Vec<Node>,
}

impl Parser<'_> {
    fn err<T>(&self, what: &str) -> Result<T, DrvError> {
        Self::parse_error(what, &self.s[..self.i.min(self.s.len())], 1)
    }

    fn parse_error<T>(what: &str, before: &[u8], column_offset: usize) -> Result<T, DrvError> {
        let line = 1 + before.iter().filter(|&&b| b == b'\n').count();
        let col = column_offset + before.iter().rev().take_while(|&&b| b != b'\n').count();
        Err(DrvError(format!(
            "cannot process __json attribute: [json.exception.parse_error.101] parse error at line {line}, column {col}: {what}"
        )))
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.i += 1;
        }
    }

    fn push(&mut self, n: Node) -> usize {
        self.nodes.push(n);
        self.nodes.len() - 1
    }

    /// The root node. `open` holds each array or object not yet closed,
    /// and for an object the key of the value that comes next.
    fn document(&mut self) -> Result<usize, DrvError> {
        if self.s.starts_with(b"\xef\xbb\xbf") {
            self.i = 3;
        }
        let mut open: Vec<(usize, Option<String>)> = Vec::new();
        loop {
            self.ws();
            let mut done = match self.peek() {
                Some(b'{') => {
                    self.i += 1;
                    let id = self.push(Node::Object(BTreeMap::new()));
                    self.ws();
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                        id
                    } else {
                        let key = self.key()?;
                        open.push((id, Some(key)));
                        continue;
                    }
                }
                Some(b'[') => {
                    self.i += 1;
                    let id = self.push(Node::Array(Vec::new()));
                    self.ws();
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        id
                    } else {
                        open.push((id, None));
                        continue;
                    }
                }
                _ => {
                    let s = self.scalar()?;
                    self.push(Node::Scalar(s))
                }
            };
            loop {
                let Some((parent, key)) = open.last_mut() else {
                    self.ws();
                    // nlohmann's lexer reads a NUL byte between tokens as the end
                    if self.peek().is_some_and(|b| b != 0) {
                        return self.err(
                            "syntax error while parsing value - unexpected input; expected end of input",
                        );
                    }
                    return Ok(done);
                };
                let parent = *parent;
                let object = match (&mut self.nodes[parent], key.take()) {
                    (Node::Object(o), Some(k)) => {
                        o.insert(k, done);
                        true
                    }
                    (Node::Array(a), _) => {
                        a.push(done);
                        false
                    }
                    _ => unreachable!("only arrays and objects are open"),
                };
                self.ws();
                match (self.peek(), object) {
                    (Some(b','), true) => {
                        self.i += 1;
                        self.ws();
                        let k = self.key()?;
                        open.last_mut().expect("an open object").1 = Some(k);
                        break;
                    }
                    (Some(b','), false) => {
                        self.i += 1;
                        break;
                    }
                    (Some(b'}'), true) | (Some(b']'), false) => {
                        self.i += 1;
                        open.pop();
                        done = parent;
                    }
                    (_, true) => {
                        return self.err("syntax error while parsing object - expected ',' or '}'");
                    }
                    (_, false) => {
                        return self.err("syntax error while parsing array - expected ',' or ']'");
                    }
                }
            }
        }
    }

    /// An object key and the `:` after it.
    fn key(&mut self) -> Result<String, DrvError> {
        if self.peek() != Some(b'"') {
            return self.err("syntax error while parsing object key - invalid literal");
        }
        let key = self.string()?;
        self.ws();
        if self.peek() != Some(b':') {
            return self.err("syntax error while parsing object separator - expected ':'");
        }
        self.i += 1;
        Ok(key)
    }

    fn scalar(&mut self) -> Result<String, DrvError> {
        match self.peek() {
            None => self.err("syntax error while parsing value - unexpected end of input"),
            Some(b'"') => {
                let s = self.string()?;
                let mut out = Vec::with_capacity(s.len() + 2);
                json_escape_str(&mut out, &s);
                Ok(String::from_utf8(out).expect("UTF-8"))
            }
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => self.err("syntax error while parsing value - invalid literal"),
        }
    }

    fn literal(&mut self, lit: &str) -> Result<String, DrvError> {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(lit.to_string())
        } else {
            self.err("syntax error while parsing value - invalid literal")
        }
    }

    fn string(&mut self) -> Result<String, DrvError> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return self.err(
                    "syntax error while parsing value - invalid string: missing closing quote",
                );
            };
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek();
                    self.i += 1;
                    match e {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            let ch = match unicode_escape(self.s, &mut self.i,
                                "surrogate U+D800..U+DBFF must be followed by U+DC00..U+DFFF",
                                "surrogate U+DC00..U+DFFF must follow U+D800..U+DBFF",
                            ) {
                                Ok(ch) => ch,
                                Err(reason) => return self.err(&format!("syntax error while parsing value - invalid string: {reason}")),
                            };
                            out.push(ch);
                        }
                        _ => return self.err("syntax error while parsing value - invalid string: forbidden character after backslash"),
                    }
                }
                0..=0x1f => return self.err("syntax error while parsing value - invalid string: control character must be escaped"),
                0x80.. => {
                    let start = self.i - 1;
                    let len = match c {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => 1,
                    };
                    let bytes = &self.s[start..(start + len).min(self.s.len())];
                    let ch = match str::from_utf8(bytes) {
                        Ok(ch) => ch,
                        Err(e) => {
                            // Count the offending byte, or the read past EOF.
                            let bad = match (len, e.error_len()) {
                                (1, _) => start,
                                (_, Some(n)) => start + n,
                                (_, None) => self.s.len(),
                            };
                            let end = (bad + 1).min(self.s.len());
                            return Self::parse_error(
                                "syntax error while parsing value - invalid string: ill-formed UTF-8 byte",
                                &self.s[..end],
                                usize::from(bad == self.s.len()),
                            );
                        }
                    };
                    out.push_str(ch);
                    self.i = start + len;
                }
                _ => out.push(char::from(c)),
            }
        }
    }

    fn number(&mut self) -> Result<String, DrvError> {
        let start = self.i;
        let neg = self.peek() == Some(b'-');
        if neg {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => {
                return self.err(
                    "syntax error while parsing value - invalid number; expected digit after '-'",
                );
            }
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            float = true;
            self.i += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err(
                    "syntax error while parsing value - invalid number; expected digit after '.'",
                );
            }
            self.digits();
        }
        if let Some(b'e' | b'E') = self.peek() {
            float = true;
            self.i += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.i += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err("syntax error while parsing value - invalid number; expected digit after exponent");
            }
            self.digits();
        }
        let text = str::from_utf8(&self.s[start..self.i]).expect("ASCII");
        // an integer out of range becomes a double, as in nlohmann's lexer
        if !float {
            if neg {
                if let Ok(i) = text.parse::<i64>() {
                    return Ok(i.to_string());
                }
            } else if let Ok(u) = text.parse::<u64>() {
                return Ok(u.to_string());
            }
        }
        let f: f64 = text.parse().expect("a JSON number");
        if !f.is_finite() {
            return Err(DrvError(format!(
                "cannot process __json attribute: [json.exception.out_of_range.406] number overflow parsing '{text}'"
            )));
        }
        Ok(json_float(f))
    }

    fn digits(&mut self) {
        while let Some(b'0'..=b'9') = self.peek() {
            self.i += 1;
        }
    }

    /// The text of `root`, as nlohmann's `dump()` writes it.
    fn write(&self, root: usize) -> String {
        enum Step<'a> {
            Node(usize),
            Key(&'a str),
            Text(&'static str),
        }
        let mut out = Vec::with_capacity(self.s.len());
        let mut todo = vec![Step::Node(root)];
        while let Some(step) = todo.pop() {
            match step {
                Step::Text(t) => out.extend_from_slice(t.as_bytes()),
                Step::Key(k) => {
                    json_escape_str(&mut out, k);
                    out.push(b':');
                }
                Step::Node(id) => match &self.nodes[id] {
                    Node::Scalar(s) => out.extend_from_slice(s.as_bytes()),
                    Node::Array(a) => {
                        out.push(b'[');
                        todo.push(Step::Text("]"));
                        for (i, &e) in a.iter().enumerate().rev() {
                            todo.push(Step::Node(e));
                            if i > 0 {
                                todo.push(Step::Text(","));
                            }
                        }
                    }
                    Node::Object(o) => {
                        out.push(b'{');
                        todo.push(Step::Text("}"));
                        for (i, (k, &e)) in o.iter().enumerate().rev() {
                            todo.push(Step::Node(e));
                            todo.push(Step::Key(k));
                            if i > 0 {
                                todo.push(Step::Text(","));
                            }
                        }
                    }
                },
            }
        }
        String::from_utf8(out).expect("the parser checks UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::normalise_structured_attrs as norm;

    /// What the `drv-oracle` of nix-conformance (Nix 2.34.8) wrote for
    /// each `__json` text.
    #[test]
    fn as_nix_writes_it() {
        for (text, nix) in [
            (r#"{"b":1,"a":2}"#, r#"{"a":2,"b":1}"#),
            (r#"{"a":1,"a":2}"#, r#"{"a":2}"#),
            (r#"{"a":-0,"b":-0.0}"#, r#"{"a":0,"b":-0.0}"#),
            (r#"{"a":1e-400}"#, r#"{"a":0.0}"#),
            (
                r#"{"a":18446744073709551615}"#,
                r#"{"a":18446744073709551615}"#,
            ),
            (
                r#"{"a":18446744073709551616}"#,
                r#"{"a":1.8446744073709552e+19}"#,
            ),
            (
                r#"{"a":-9223372036854775809}"#,
                r#"{"a":-9.223372036854776e+18}"#,
            ),
            (r#"{"a":1E5,"b":1.5e300}"#, r#"{"a":100000.0,"b":1.5e+300}"#),
            ("\u{feff} {\"a\" : [ 1 , 2 ] } \n", r#"{"a":[1,2]}"#),
            (
                r#"{"a":"\u0000\u001f\/\ud83d\ude00"}"#,
                "{\"a\":\"\\u0000\\u001f/\u{1f600}\"}",
            ),
            (
                "{\"\u{e9}\":1,\"z\":2,\"Z\":3,\"_\":4}",
                "{\"Z\":3,\"_\":4,\"z\":2,\"\u{e9}\":1}",
            ),
            ("{\"a\":1} \0}\"", r#"{"a":1}"#),
            (r#"{"a":{},"b":[]}"#, r#"{"a":{},"b":[]}"#),
        ] {
            assert_eq!(norm(text.as_bytes()).as_deref(), Ok(nix), "{text}");
        }
    }

    #[test]
    fn what_nix_refuses() {
        let cases: &[(&[u8], &str)] = &[
            (b"[1]", "type must be object, but is array"),
            (b"null", "type must be object, but is null"),
            (br#"{"a":1e400}"#, "number overflow parsing '1e400'"),
            (b"{\"a\":\"x\0y\"}", "control character must be escaped"),
            (br#"{"a":"\ud800"}"#, "invalid string"),
            (
                b"{\"a\":\"\xff\"}",
                "column 7: syntax error while parsing value - invalid string: ill-formed UTF-8",
            ),
            (b"{\"a\":\"\xe1\x80Z", "line 1, column 9:"),
            (b"{\"a\":\"\xf0\x9f\x99", "line 1, column 10:"),
            (b"{\"a\":\"\xe2\n", "line 2, column 0:"),
            (br#"{"a":1} x"#, "expected end of input"),
            (br#"{"n":"\u1z00"}"#, "line 1, column 10:"),
            (b"", "unexpected end of input"),
            (br#"{"a":01}"#, "expected ',' or '}'"),
            (br#"{"a":1.}"#, "expected digit after '.'"),
        ];
        for &(text, err) in cases {
            let got = norm(text).unwrap_err().0;
            assert!(
                got.starts_with("cannot process __json attribute: "),
                "{got}"
            );
            assert!(got.contains(err), "{got}");
        }
    }

    /// The deepest value a 16 KiB `drv-read` input can hold.
    #[test]
    fn deep_nesting() {
        let depth = 8192;
        let text = format!("{{\"a\":{}{}}}", "[".repeat(depth), "]".repeat(depth));
        assert_eq!(norm(text.as_bytes()).as_deref(), Ok(text.as_str()));
    }
}
