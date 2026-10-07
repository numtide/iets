//! `builtins.toJSON` (via `print::print_json`) and `builtins.fromJSON`, a
//! parser with nlohmann::json's acceptance rules (`json-to-value.cc`).

use crate::eval::{Error, ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::runtime::{Ctx, Value, Word, ctx_to_arena};
use crate::syntax::symbol::Sym;

/// A recursive-descent limit iets needs and nlohmann does not: the parser
/// recurses, and a fiber stack is 256 MiB.
const MAX_JSON_DEPTH: u32 = 100_000;

pub fn to_json(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let mut out: Vec<u8> = Vec::new();
    let mut ctx: Vec<Ctx> = Vec::new();
    crate::print::print_json(ev, args[0], &mut out, &mut ctx, true, pos)?;
    let ctx = ctx_to_arena(&mut ev.arena, ctx);
    Ok(ev.arena.mk_string_shared(&out, ctx))
}

pub fn from_json(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the first argument passed to builtins.fromJSON",
    )?;
    let mut p = Parser {
        s,
        i: 0,
        ev,
        depth: 0,
        pos,
        attr: false,
    };
    p.document()
}

/// `parsed-derivations.cc:StructuredAttrs::parse` followed by `unparse`:
/// nlohmann parses the `__json` text and writes `dump()`, so a hand-written
/// text reaches the `.drv` in the normal form.
pub fn canonical_structured_attrs(
    ev: &mut Evaluator,
    s: &'static [u8],
    pos: Pos,
) -> EvalResult<String> {
    let mut p = Parser {
        s,
        i: 0,
        ev,
        depth: 0,
        pos,
        attr: true,
    };
    let v = p.document()?;
    let kind = match v.view() {
        Value::Attrs(_) => {
            let mut out = Vec::new();
            let mut ctx = Vec::new();
            crate::print::print_json(ev, v, &mut out, &mut ctx, true, pos)?;
            return Ok(String::from_utf8(out).expect("print_json writes utf-8"));
        }
        Value::Null => "null",
        Value::List(_) => "array",
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        _ => "number",
    };
    bail!(
        ErrorKind::Eval,
        Pos::NONE,
        "cannot process __json attribute: [json.exception.type_error.302] type must be object, but is {kind}"
    )
}

struct Parser<'a> {
    s: &'static [u8],
    i: usize,
    ev: &'a mut Evaluator,
    depth: u32,
    pos: Pos,
    /// Parsing a `__json` derivation attribute, whose errors carry
    /// `StructuredAttrs::parse`'s wrapper instead of `fromJSON`'s trace.
    attr: bool,
}

impl Parser<'_> {
    fn fail<T>(&self, msg: String) -> EvalResult<T> {
        if self.attr {
            bail!(
                ErrorKind::Eval,
                Pos::NONE,
                "cannot process __json attribute: {msg}"
            );
        }
        let mut e = Error::boxed(ErrorKind::Eval, msg, Pos::NONE);
        // primops.cc:prim_fromJSON traces only JSONParseError, not the
        // integer range or null byte errors
        e.add_trace(self.pos, "while decoding a JSON string");
        Err(e)
    }

    fn err<T>(&self, what: &str) -> EvalResult<T> {
        let mut line = 1;
        let mut col = 1;
        for &b in &self.s[..self.i.min(self.s.len())] {
            if b == b'\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        self.fail(format!(
            "[json.exception.parse_error.101] parse error at line {line}, column {col}: {what}"
        ))
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.i += 1;
        }
    }

    fn document(&mut self) -> EvalResult<Word> {
        if self.s.starts_with(b"\xef\xbb\xbf") {
            self.i = 3;
        }
        self.ws();
        let v = self.value()?;
        self.ws();
        if self.i != self.s.len() {
            return self
                .err("syntax error while parsing value - unexpected input; expected end of input");
        }
        Ok(v)
    }

    fn value(&mut self) -> EvalResult<Word> {
        match self.peek() {
            None => self.err("syntax error while parsing value - unexpected end of input"),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => {
                let bytes = self.string()?;
                Ok(self.ev.mk_string(&bytes))
            }
            Some(b't') => self.literal(b"true", Word::TRUE),
            Some(b'f') => self.literal(b"false", Word::FALSE),
            Some(b'n') => self.literal(b"null", Word::NULL),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => self.err("syntax error while parsing value - invalid literal"),
        }
    }

    fn literal(&mut self, lit: &[u8], w: Word) -> EvalResult<Word> {
        if self.s[self.i..].starts_with(lit) {
            self.i += lit.len();
            Ok(w)
        } else {
            self.err("syntax error while parsing value - invalid literal")
        }
    }

    fn nest(&mut self) -> EvalResult<()> {
        self.depth += 1;
        if self.depth > MAX_JSON_DEPTH {
            return self.err("nesting too deep");
        }
        Ok(())
    }

    fn object(&mut self) -> EvalResult<Word> {
        self.nest()?;
        self.i += 1;
        let mut entries: Vec<(Sym, Pos, Word)> = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            self.depth -= 1;
            return Ok(self.ev.empty_attrs());
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return self.err("syntax error while parsing object key - invalid literal");
            }
            let key = self.string()?;
            let sym = self.ev.sym(&key);
            self.ws();
            if self.peek() != Some(b':') {
                return self.err("syntax error while parsing object separator - expected ':'");
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            entries.push((sym, Pos::NONE, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return self.err("syntax error while parsing object - expected ',' or '}'"),
            }
        }
        self.depth -= 1;
        // duplicate keys: the last value wins (insert_or_assign)
        entries.sort_by_key(|e| e.0);
        let mut out: Vec<(Sym, Pos, Word)> = Vec::with_capacity(entries.len());
        for e in entries {
            match out.last_mut() {
                Some(last) if last.0 == e.0 => *last = e,
                _ => out.push(e),
            }
        }
        Ok(self.ev.arena.mk_attrs_sorted(&out))
    }

    fn array(&mut self) -> EvalResult<Word> {
        self.nest()?;
        self.i += 1;
        let mut items: Vec<Word> = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            self.depth -= 1;
            return Ok(self.ev.mk_list(&[]));
        }
        loop {
            self.ws();
            let v = self.value()?;
            items.push(v);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return self.err("syntax error while parsing array - expected ',' or ']'"),
            }
        }
        self.depth -= 1;
        Ok(self.ev.mk_list(&items))
    }

    fn hex4(&mut self) -> EvalResult<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let Some(c) = self.peek() else {
                return self.err("syntax error while parsing value - invalid string: '\\u' must be followed by 4 hex digits");
            };
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return self.err("syntax error while parsing value - invalid string: '\\u' must be followed by 4 hex digits"),
            };
            v = v * 16 + d as u32;
            self.i += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> EvalResult<Vec<u8>> {
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return self.err(
                    "syntax error while parsing value - invalid string: missing closing quote",
                );
            };
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return self.err("syntax error while parsing value - invalid string: forbidden character after backslash");
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                if !self.s[self.i..].starts_with(b"\\u") {
                                    return self.err("syntax error while parsing value - invalid string: surrogate must be followed by a low surrogate");
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return self.err("syntax error while parsing value - invalid string: surrogate must be followed by a low surrogate");
                                }
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            } else if (0xDC00..0xE000).contains(&cp) {
                                return self.err("syntax error while parsing value - invalid string: low surrogate must follow a high surrogate");
                            }
                            let ch = char::from_u32(cp).expect("valid scalar");
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return self.err("syntax error while parsing value - invalid string: forbidden character after backslash"),
                    }
                }
                0..=0x1f => return self.err("syntax error while parsing value - invalid string: control character must be escaped"),
                0x80.. => {
                    // validate one UTF-8 sequence, as nlohmann's lexer does
                    let start = self.i - 1;
                    let len = match c {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return self.err("syntax error while parsing value - invalid string: ill-formed UTF-8 byte"),
                    };
                    if start + len > self.s.len()
                        || std::str::from_utf8(&self.s[start..start + len]).is_err()
                    {
                        return self.err("syntax error while parsing value - invalid string: ill-formed UTF-8 byte");
                    }
                    out.extend_from_slice(&self.s[start..start + len]);
                    self.i = start + len;
                }
                _ => out.push(c),
            }
        }
        // json-to-value.cc checks each key and string value once it is whole
        if let Err(msg) = crate::syntax::lexer::no_nul(&out) {
            bail!(ErrorKind::Eval, Pos::NONE, "{}", msg);
        }
        Ok(out)
    }

    fn number(&mut self) -> EvalResult<Word> {
        let start = self.i;
        let neg = self.peek() == Some(b'-');
        if neg {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                while let Some(b'0'..=b'9') = self.peek() {
                    self.i += 1;
                }
            }
            _ => {
                return self.err(
                    "syntax error while parsing value - invalid number; expected digit after '-'",
                );
            }
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.i += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err(
                    "syntax error while parsing value - invalid number; expected digit after '.'",
                );
            }
            while let Some(b'0'..=b'9') = self.peek() {
                self.i += 1;
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            is_float = true;
            self.i += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.i += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err("syntax error while parsing value - invalid number; expected digit after exponent");
            }
            while let Some(b'0'..=b'9') = self.peek() {
                self.i += 1;
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).expect("ascii");
        if !is_float {
            if neg {
                if let Ok(i) = text.parse::<i64>() {
                    return Ok(self.ev.mk_int(i));
                }
            } else if let Ok(u) = text.parse::<u64>() {
                if u > i64::MAX as u64 {
                    bail!(
                        ErrorKind::Eval,
                        Pos::NONE,
                        "unsigned json number {} outside of Nix integer range",
                        u
                    );
                }
                return Ok(self.ev.mk_int(u as i64));
            }
        }
        let f: f64 = text.parse().expect("validated float syntax");
        // strtod only fails on overflow; underflow yields zero or a subnormal
        if !f.is_finite() {
            return self.fail(format!(
                "[json.exception.out_of_range.406] number overflow parsing '{text}'"
            ));
        }
        Ok(self.ev.mk_float(f))
    }
}
