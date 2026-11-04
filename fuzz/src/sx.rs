//! The canonical tree both parsers are compared in: S-expressions of atoms,
//! byte strings and lists. `fuzz/oracle/parse-oracle.cc` prints Nix's tree
//! in this form and `canon` builds iets's.

use std::fmt::Write;
use std::str;

#[derive(Clone, PartialEq, Debug)]
pub enum Sx {
    Atom(String),
    Str(Vec<u8>),
    List(Vec<Sx>),
}

pub fn atom(s: &str) -> Sx {
    Sx::Atom(s.to_string())
}

pub fn node(tag: &str, mut rest: Vec<Sx>) -> Sx {
    rest.insert(0, atom(tag));
    Sx::List(rest)
}

impl Sx {
    pub fn tag(&self) -> Option<&str> {
        match self {
            Sx::List(v) => match v.first() {
                Some(Sx::Atom(a)) => Some(a),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn is(&self, tag: &str) -> bool {
        self.tag() == Some(tag)
    }
}

pub fn parse(text: &[u8]) -> Result<Sx, String> {
    let mut p = Reader { s: text, i: 0 };
    let v = p.value()?;
    p.ws();
    if p.i != text.len() {
        return Err(format!("trailing bytes at {}", p.i));
    }
    Ok(v)
}

struct Reader<'a> {
    s: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i) == Some(&b' ') {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Result<Sx, String> {
        self.ws();
        match self.s.get(self.i) {
            Some(b'(') => {
                self.i += 1;
                let mut v = Vec::new();
                loop {
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b')') => {
                            self.i += 1;
                            return Ok(Sx::List(v));
                        }
                        None => return Err("unclosed list".into()),
                        _ => v.push(self.value()?),
                    }
                }
            }
            Some(b'"') => {
                self.i += 1;
                let mut out = Vec::new();
                loop {
                    match self.s.get(self.i) {
                        Some(b'"') => {
                            self.i += 1;
                            return Ok(Sx::Str(out));
                        }
                        Some(b'\\') => match self.s.get(self.i + 1) {
                            Some(b'x') => {
                                let h = self.s.get(self.i + 2..self.i + 4).ok_or("short escape")?;
                                let h = str::from_utf8(h).map_err(|e| e.to_string())?;
                                out.push(u8::from_str_radix(h, 16).map_err(|e| e.to_string())?);
                                self.i += 4;
                            }
                            Some(&c) => {
                                out.push(c);
                                self.i += 2;
                            }
                            None => return Err("short escape".into()),
                        },
                        Some(&c) => {
                            out.push(c);
                            self.i += 1;
                        }
                        None => return Err("unclosed string".into()),
                    }
                }
            }
            Some(_) => {
                let start = self.i;
                while let Some(&c) = self.s.get(self.i) {
                    if c == b' ' || c == b'(' || c == b')' || c == b'"' {
                        break;
                    }
                    self.i += 1;
                }
                Ok(Sx::Atom(
                    String::from_utf8_lossy(&self.s[start..self.i]).into_owned(),
                ))
            }
            None => Err("unexpected end".into()),
        }
    }
}

pub fn quote(out: &mut String, s: &[u8]) {
    out.push('"');
    for &c in s {
        match c {
            b'"' | b'\\' => {
                out.push('\\');
                out.push(c as char);
            }
            0x20..0x7f => out.push(c as char),
            _ => {
                let _ = write!(out, "\\x{c:02x}");
            }
        }
    }
    out.push('"');
}

pub fn print(sx: &Sx) -> String {
    let mut out = String::new();
    write_sx(&mut out, sx);
    out
}

fn write_sx(out: &mut String, sx: &Sx) {
    match sx {
        Sx::Atom(a) => out.push_str(a),
        Sx::Str(s) => quote(out, s),
        Sx::List(v) => {
            out.push('(');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write_sx(out, x);
            }
            out.push(')');
        }
    }
}

/// Rewrites that remove what the two trees may differ in without the
/// programs differing: the order Nix keeps bindings and formals in, where
/// the lexers split a string into tokens, and `__sub 0 N`, which SPEC.md
/// shows as the literal `-N`.
pub fn normalise(sx: Sx) -> Sx {
    let Sx::List(v) = sx else { return sx };
    let mut v: Vec<Sx> = v.into_iter().map(normalise).collect();
    let tag = match v.first() {
        Some(Sx::Atom(a)) => a.clone(),
        _ => return Sx::List(v),
    };
    match tag.as_str() {
        "attrs" => {
            let dyn_start = v.iter().position(|x| x.is("dyn-bind")).unwrap_or(v.len());
            v[2..dyn_start].sort_by(|a, b| bind_name(a).cmp(bind_name(b)));
        }
        "formals" => {
            let end = if v.last() == Some(&atom("...")) {
                v.len() - 1
            } else {
                v.len()
            };
            v[1..end].sort_by(|a, b| bind_name(a).cmp(bind_name(b)));
        }
        "interp" | "+" => {
            let mut parts = vec![v.remove(0)];
            for x in v {
                let n = parts.len();
                match (&mut parts[n - 1], x) {
                    (Sx::List(prev), Sx::List(cur)) if n > 1 && is_str(prev) && is_str(&cur) => {
                        if let (Sx::Str(a), Sx::Str(b)) = (&mut prev[1], &cur[1]) {
                            a.extend_from_slice(b);
                        }
                    }
                    (_, x) => parts.push(x),
                }
            }
            if tag == "interp" {
                parts.retain(|x| !matches!(x, Sx::List(l) if is_str(l) && l[1] == Sx::Str(vec![])));
                match &parts[1..] {
                    [] => return node("str", vec![Sx::Str(vec![])]),
                    [s @ Sx::List(l)] if is_str(l) => return s.clone(),
                    _ => {}
                }
            }
            v = parts;
        }
        "call" => {
            if let Some(lit) = folded_negation(&v) {
                let rest: Vec<Sx> = v.drain(4..).collect();
                if rest.is_empty() {
                    return lit;
                }
                let mut call = vec![atom("call"), lit];
                call.extend(rest);
                return Sx::List(call);
            }
        }
        _ => {}
    }
    Sx::List(v)
}

fn bind_name(sx: &Sx) -> &[u8] {
    match sx {
        Sx::List(v) => match v.get(1) {
            Some(Sx::Str(s)) => s,
            _ => &[],
        },
        _ => &[],
    }
}

fn is_str(l: &[Sx]) -> bool {
    l.len() == 2 && l[0] == atom("str")
}

fn folded_negation(v: &[Sx]) -> Option<Sx> {
    if v.len() < 4
        || v[1] != node("var", vec![Sx::Str(b"__sub".to_vec())])
        || v[2] != node("int", vec![atom("0")])
    {
        return None;
    }
    let Sx::List(x) = &v[3] else { return None };
    match (x.first(), x.get(1)) {
        (Some(Sx::Atom(t)), Some(Sx::Atom(n))) if t == "int" => {
            let n: i64 = n.parse().ok()?;
            Some(node(
                "int",
                vec![Sx::Atom(0i64.wrapping_sub(n).to_string())],
            ))
        }
        (Some(Sx::Atom(t)), Some(Sx::Atom(bits))) if t == "float" => {
            let f = f64::from_bits(u64::from_str_radix(bits, 16).ok()?);
            Some(float(0.0 - f))
        }
        _ => None,
    }
}

pub fn float(f: f64) -> Sx {
    node("float", vec![Sx::Atom(format!("{:016x}", f.to_bits()))])
}

/// The path to the first subtree where `a` and `b` differ, and the two
/// subtrees, for a report a human reads.
pub fn first_difference<'a>(a: &'a Sx, b: &'a Sx) -> Option<(String, &'a Sx, &'a Sx)> {
    if a == b {
        return None;
    }
    if let (Sx::List(x), Sx::List(y)) = (a, b)
        && x.len() == y.len()
        && x.first() == y.first()
    {
        for (i, (p, q)) in x.iter().zip(y).enumerate() {
            if let Some((path, l, r)) = first_difference(p, q) {
                let tag = a.tag().unwrap_or("");
                return Some((format!("{tag}[{i}]/{path}"), l, r));
            }
        }
    }
    Some((String::new(), a, b))
}
