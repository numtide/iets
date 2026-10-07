//! `builtins.fromTOML` on top of the `toml` crate, with Nix's datetime
//! rendering (`primops/fromTOML.cc: normalizeDatetimeFormat`).
//!
//! Nix parses with toml11 pinned to `toml::spec::v(1, 0, 0)`; the `toml`
//! crate implements 1.1 and has no switch for the older spec. [`reject_1_1`]
//! and the seconds check in [`convert`] take the 1.1-only syntax back out.

use std::sync::atomic::Ordering;

use crate::eval::{ErrorKind, EvalResult, Evaluator, Pos, bail};
use crate::runtime::Word;
use crate::syntax::symbol::Sym;

/// The `parse-toml-timestamps` experimental feature.
pub fn set_parse_toml_timestamps(on: bool) {
    crate::session::Session::current()
        .toml_timestamps
        .store(on, Ordering::Relaxed);
}

pub fn from_toml(ev: &mut Evaluator, args: &[Word], pos: Pos) -> EvalResult<Word> {
    let s = ev.force_string_no_ctx(
        args[0],
        pos,
        "while evaluating the argument passed to builtins.fromTOML",
    )?;
    let text = match std::str::from_utf8(s) {
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
    use ::toml::Value as T;
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
            if !ev.session().toml_timestamps.load(Ordering::Relaxed) {
                bail!(
                    ErrorKind::Eval,
                    pos,
                    "while parsing TOML: Dates and times are not supported"
                );
            }
            let s = format_datetime(&d);
            let ty = ev.mk_string(b"timestamp");
            let val = ev.mk_string(s.as_bytes());
            let ty_sym = ev.sym(b"_type");
            let mut entries = vec![(ty_sym, Pos::NONE, ty), (ev.s.value, Pos::NONE, val)];
            ev.mk_attrs(&mut entries, pos)?
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

fn no_nul(s: &[u8], pos: Pos) -> EvalResult<()> {
    if let Err(msg) = crate::syntax::lexer::no_nul(s) {
        // prim_fromTOML wraps the thrown Error's what(), prefix included
        bail!(ErrorKind::Eval, pos, "while parsing TOML: error: {}", msg);
    }
    Ok(())
}

/// toml11 < 4 subsecond precision: 3, 6 or 9 digits, or none.
fn subsecond_precision(ns: u32) -> u32 {
    if ns == 0 {
        0
    } else if ns.is_multiple_of(1_000_000) {
        3
    } else if ns.is_multiple_of(1_000) {
        6
    } else {
        9
    }
}

fn format_datetime(d: &::toml::value::Datetime) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    if let Some(date) = d.date {
        let _ = write!(s, "{:04}-{:02}-{:02}", date.year, date.month, date.day);
    }
    if let Some(time) = d.time {
        if d.date.is_some() {
            s.push('T');
        }
        let _ = write!(
            s,
            "{:02}:{:02}:{:02}",
            time.hour,
            time.minute,
            time.second.unwrap_or(0)
        );
        let ns = time.nanosecond.unwrap_or(0);
        let prec = subsecond_precision(ns);
        if prec > 0 {
            let frac = ns / 10u32.pow(9 - prec);
            let _ = write!(s, ".{:0width$}", frac, width = prec as usize);
        }
    }
    if let Some(off) = d.offset {
        match off {
            ::toml::value::Offset::Z => s.push('Z'),
            ::toml::value::Offset::Custom { minutes } => {
                let sign = if minutes < 0 { '-' } else { '+' };
                let m = minutes.unsigned_abs();
                let _ = write!(s, "{}{:02}:{:02}", sign, m / 60, m % 60);
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::reject_1_1;

    fn ok(doc: &str) -> bool {
        reject_1_1(doc).is_ok()
    }

    #[test]
    fn inline_tables_stay_on_one_line() {
        assert!(!ok("a = { b = 1,\n c = 2 }"));
        assert!(!ok("a = { # hi\n b = 1 }"));
        assert!(!ok("a = { b = 1, }"));
        assert!(!ok("a = { b = 1 , }"));
        assert!(!ok("a = { b = 1, c = { d = 2, } }"));
        // A newline is fine one container deeper.
        assert!(ok("a = { b = [1,\n2] }"));
        assert!(ok("a = { b = \"\"\"x\ny\"\"\" }"));
        assert!(ok("a = [\n {b = 1},\n]"));
        assert!(ok("a = { b = 1 }"));
        // Braces and commas inside strings are not structure.
        assert!(ok("a = { b = \"}\", c = \",\" }"));
        assert!(ok("a = { b = ',' }"));
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
