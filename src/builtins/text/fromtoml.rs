//! `builtins.fromTOML` on top of the `toml` crate.
//!
//! Nix parses with toml11 pinned to `toml::spec::v(1, 0, 0)`; the `toml`
//! crate implements 1.1 and has no switch for the older spec. [`reject_1_1`]
//! and the seconds check in [`convert`] take the 1.1-only syntax back out.

use std::str::from_utf8;

use crate::eval::{ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::runtime::Word;
use crate::syntax::lexer;
use crate::syntax::symbol::Sym;
use ::toml::Value as T;

pub fn from_toml(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.fromTOML",
    )?;
    let text = match from_utf8(s) {
        Ok(t) => t,
        Err(e) => bail!(ErrorKind::Eval, pos, "while parsing TOML: {}", e),
    };
    if let Err(what) = reject_1_1(text) {
        bail!(ErrorKind::Eval, pos, "while parsing TOML: {}", what);
    }
    let table: ::toml::Table = match ::toml::from_str(text) {
        Ok(t) => t,
        Err(e) => bail!(ErrorKind::Eval, pos, "while parsing TOML: {}", e.message()),
    };
    convert(ev, ::toml::Value::Table(table), pos)
}

/// The TOML 1.1 additions the `toml` crate accepts and toml11 at spec 1.0.0
/// refuses: a newline or a trailing comma inside an inline table, and the
/// `\e` and `\xHH` string escapes. Omitted seconds are caught on the parsed
/// value instead, where the crate reports them as `second: None`.
///
/// Rejecting only: anything this pass lets through is still the crate's to
/// judge, so a document 1.0 already refuses needs no rule here.
fn reject_1_1(text: &str) -> Result<(), &'static str> {
    let b = text.as_bytes();
    let mut i = 0;
    // '{' for an inline table, '[' for an array or a table header. Only the
    // innermost matters: a newline inside an array nested in an inline table
    // is valid 1.0.
    let mut open: Vec<u8> = Vec::new();
    // The last byte that was not whitespace, a comment or inside a string.
    let mut last = 0u8;
    while i < b.len() {
        let c = b[i];
        match c {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'\n' => {
                if open.last() == Some(&b'{') {
                    return Err("newline in inline table is TOML 1.1");
                }
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'"' => {
                i = basic_string(b, i)?;
                last = c;
            }
            b'\'' => {
                i = literal_string(b, i);
                last = c;
            }
            b'}' => {
                if last == b',' {
                    return Err("trailing comma in inline table is TOML 1.1");
                }
                open.pop();
                last = c;
                i += 1;
            }
            b']' => {
                open.pop();
                last = c;
                i += 1;
            }
            b'{' | b'[' => {
                open.push(c);
                last = c;
                i += 1;
            }
            _ => {
                last = c;
                i += 1;
            }
        }
    }
    Ok(())
}

/// Skip a basic string, single- or multi-line, starting at its first quote.
fn basic_string(b: &[u8], i: usize) -> Result<usize, &'static str> {
    let multi = b[i + 1..].starts_with(b"\"\"");
    let mut j = i + if multi { 3 } else { 1 };
    while j < b.len() {
        match b[j] {
            b'\\' => {
                match b.get(j + 1) {
                    Some(b'e') => return Err("the \\e escape is TOML 1.1"),
                    Some(b'x') => return Err("the \\xHH escape is TOML 1.1"),
                    _ => {}
                }
                j += 2;
            }
            b'"' if !multi => return Ok(j + 1),
            b'"' => {
                let run = quote_run(b, j, b'"');
                if run < 3 {
                    j += run;
                } else {
                    // Up to two of the quotes are content; more is invalid
                    // TOML, which the crate reports.
                    return Ok(j + run.min(5));
                }
            }
            // An unterminated string: leave it to the crate.
            b'\n' if !multi => return Ok(j),
            _ => j += 1,
        }
    }
    Ok(b.len())
}

/// Skip a literal string, which has no escapes to check.
fn literal_string(b: &[u8], i: usize) -> usize {
    let multi = b[i + 1..].starts_with(b"''");
    let mut j = i + if multi { 3 } else { 1 };
    while j < b.len() {
        match b[j] {
            b'\'' if !multi => return j + 1,
            b'\'' => {
                let run = quote_run(b, j, b'\'');
                if run < 3 {
                    j += run;
                } else {
                    return j + run.min(5);
                }
            }
            b'\n' if !multi => return j,
            _ => j += 1,
        }
    }
    b.len()
}

fn quote_run(b: &[u8], i: usize, q: u8) -> usize {
    b[i..].iter().take_while(|&&c| c == q).count()
}

fn convert(ev: &mut Evaluator, v: ::toml::Value, pos: Pos) -> EvalResult<Word> {
    Ok(match v {
        T::String(s) => {
            no_nul(s.as_bytes(), pos)?;
            ev.mk_string(s.as_bytes())
        }
        T::Integer(i) => ev.mk_int(i),
        T::Float(f) => ev.mk_float(f),
        T::Boolean(b) => Word::from_bool(b),
        T::Datetime(d) => {
            // Seconds are optional only from TOML 1.1 on.
            if d.time.is_some_and(|t| t.second.is_none()) {
                bail!(
                    ErrorKind::Eval,
                    pos,
                    "while parsing TOML: a time without seconds is TOML 1.1"
                );
            }
            // a timestamp fails only where it is used
            let s = ev.mk_string(d.to_string().as_bytes());
            let sym = ev.sym(b"fromTOML");
            let op = ev.arena.mk_primop("fromTOML", 1, timestamp, sym);
            ev.arena.mk_thunk_app(op, s)
        }
        T::Array(items) => {
            let mut words = Vec::with_capacity(items.len());
            for it in items {
                words.push(convert(ev, it, pos)?);
            }
            ev.mk_list(&words)
        }
        T::Table(t) => {
            let mut entries: Vec<(Sym, Pos, Word)> = Vec::with_capacity(t.len());
            for (k, v) in t {
                no_nul(k.as_bytes(), pos)?;
                let sym = ev.sym(k.as_bytes());
                let w = convert(ev, v, pos)?;
                entries.push((sym, Pos::NONE, w));
            }
            ev.mk_attrs(&mut entries, pos)?
        }
    })
}

fn timestamp(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string_no_ctx(args[0], pos, "")?;
    let s = String::from_utf8_lossy(s).into_owned();
    bail!(
        ErrorKind::Eval,
        pos,
        "while parsing TOML: Dates and times are not supported ({s})"
    )
}

fn no_nul(s: &[u8], pos: Pos) -> EvalResult<()> {
    if let Err(msg) = lexer::no_nul(s) {
        // prim_fromTOML wraps the thrown Error's what(), prefix included
        bail!(ErrorKind::Eval, pos, "while parsing TOML: error: {}", msg);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::reject_1_1;
    use rstest::rstest;

    fn ok(doc: &str) -> bool {
        reject_1_1(doc).is_ok()
    }

    #[rstest]
    #[case("a = { b = 1,\n c = 2 }", false)]
    #[case("a = { # hi\n b = 1 }", false)]
    #[case("a = { b = 1, }", false)]
    #[case("a = { b = 1 , }", false)]
    #[case("a = { b = 1, c = { d = 2, } }", false)]
    // a newline is fine one container deeper
    #[case("a = { b = [1,\n2] }", true)]
    #[case("a = { b = \"\"\"x\ny\"\"\" }", true)]
    #[case("a = [\n {b = 1},\n]", true)]
    #[case("a = { b = 1 }", true)]
    // braces and commas inside strings are not structure
    #[case("a = { b = \"}\", c = \",\" }", true)]
    #[case("a = { b = ',' }", true)]
    fn inline_tables_stay_on_one_line(#[case] src: &str, #[case] want: bool) {
        assert_eq!(ok(src), want);
    }

    #[test]
    fn new_escapes_are_refused() {
        assert!(!ok("a = \"\\e\""));
        assert!(!ok("a = \"\\x41\""));
        assert!(!ok("a = \"\"\"\n\\e\"\"\""));
        // An escaped backslash does not start an escape.
        assert!(ok("a = \"x\\\\e\""));
        // Literal strings have no escapes.
        assert!(ok("a = '\\e'"));
        assert!(ok("a = '''\\x41'''"));
    }
}
