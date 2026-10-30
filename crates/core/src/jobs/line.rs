//! One nix-eval-jobs output line: a job or an error. Keys are byte-sorted
//! like nlohmann's `std::map`; values iets never inspects stay raw JSON so
//! a line survives a round trip through a worker pipe unchanged.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::store::path::StorePath;

pub struct Job {
    pub attr: String,
    pub attr_path: Vec<String>,
    pub drv_path: StorePath,
    /// Output name to path; `None` for a CA output.
    pub outputs: BTreeMap<String, Option<StorePath>>,
    /// Constituent `.drv`s: the string context of `constituents`, then the
    /// named ones once `--constituents` resolved them.
    pub constituents: Vec<StorePath>,
    /// `None` once `--constituents` erased the key from a resolved aggregate.
    pub named_constituents: Option<Vec<String>>,
    pub glob_constituents: bool,
    /// Every other key, as raw JSON.
    pub extra: BTreeMap<String, Vec<u8>>,
}

pub struct ErrorLine {
    pub attr: String,
    pub attr_path: Vec<String>,
    pub error: String,
    pub fatal: bool,
    /// `stats` as raw JSON, when a worker measured the request.
    pub stats: Option<Vec<u8>>,
}

/// worker.cc:processJobRequest's `stats`: the request's wall time and the
/// bytes it allocated, arena bytes here where nix counts Boehm's.
pub fn stats_json(wall_ms: u64, alloc_bytes: u64) -> Vec<u8> {
    format!("{{\"allocBytes\":{alloc_bytes},\"wallMs\":{wall_ms}}}").into_bytes()
}

pub enum Line {
    Job(Job),
    Error(ErrorLine),
}

pub fn json_str(s: &str) -> Vec<u8> {
    serde_json::to_vec(s).expect("string to JSON")
}

pub fn json_str_list(items: impl Iterator<Item = impl AsRef<str>>) -> Vec<u8> {
    let mut out = vec![b'['];
    for (i, s) in items.enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend(json_str(s.as_ref()));
    }
    out.push(b']');
    out
}

impl Job {
    /// Compact JSON with keys in byte order, as nlohmann `dump()` prints it.
    pub fn render(&self) -> Vec<u8> {
        let mut fields: Vec<(&str, Vec<u8>)> = Vec::with_capacity(self.extra.len() + 7);
        fields.push(("attr", json_str(&self.attr)));
        fields.push(("attrPath", json_str_list(self.attr_path.iter())));
        fields.push((
            "constituents",
            json_str_list(self.constituents.iter().map(|p| p.to_string())),
        ));
        fields.push(("drvPath", json_str(&self.drv_path.to_string())));
        fields.push((
            "globConstituents",
            if self.glob_constituents {
                b"true".to_vec()
            } else {
                b"false".to_vec()
            },
        ));
        if let Some(named) = &self.named_constituents {
            fields.push(("namedConstituents", json_str_list(named.iter())));
        }
        let mut outs = vec![b'{'];
        for (i, (n, p)) in self.outputs.iter().enumerate() {
            if i > 0 {
                outs.push(b',');
            }
            outs.extend(json_str(n));
            outs.push(b':');
            match p {
                Some(p) => outs.extend(json_str(&p.to_string())),
                None => outs.extend_from_slice(b"null"),
            }
        }
        outs.push(b'}');
        fields.push(("outputs", outs));
        for (k, v) in &self.extra {
            fields.push((k.as_str(), v.clone()));
        }
        fields.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        render_fields(&fields)
    }
}

impl ErrorLine {
    pub fn render(&self) -> Vec<u8> {
        let mut fields = vec![
            ("attr", json_str(&self.attr)),
            ("attrPath", json_str_list(self.attr_path.iter())),
            ("error", json_str(&self.error)),
            (
                "fatal",
                if self.fatal {
                    b"true".to_vec()
                } else {
                    b"false".to_vec()
                },
            ),
        ];
        if let Some(s) = &self.stats {
            fields.push(("stats", s.clone()));
        }
        render_fields(&fields)
    }
}

impl Line {
    pub fn render(&self) -> Vec<u8> {
        match self {
            Line::Job(j) => j.render(),
            Line::Error(e) => e.render(),
        }
    }

    pub fn attr(&self) -> &str {
        match self {
            Line::Job(j) => &j.attr,
            Line::Error(e) => &e.attr,
        }
    }

    /// Parses a line a worker printed. A line with `drvPath` is a job, one
    /// with `error` and no `drvPath` is an error.
    pub fn parse(bytes: &[u8]) -> Result<Line> {
        let fields = split_object(bytes)?;
        let mut map: BTreeMap<String, &[u8]> = fields.into_iter().collect();
        let take_str = |map: &mut BTreeMap<String, &[u8]>, k: &str| -> Result<String> {
            let raw = map
                .remove(k)
                .ok_or_else(|| Error::Protocol(format!("line without '{k}'")))?;
            serde_json::from_slice::<String>(raw)
                .map_err(|e| Error::Protocol(format!("'{k}': {e}")))
        };
        let take_strs = |map: &mut BTreeMap<String, &[u8]>, k: &str| -> Result<Vec<String>> {
            let raw = map
                .remove(k)
                .ok_or_else(|| Error::Protocol(format!("line without '{k}'")))?;
            serde_json::from_slice::<Vec<String>>(raw)
                .map_err(|e| Error::Protocol(format!("'{k}': {e}")))
        };
        let attr = take_str(&mut map, "attr")?;
        let attr_path = take_strs(&mut map, "attrPath")?;
        if !map.contains_key("drvPath") {
            let error = take_str(&mut map, "error")?;
            let fatal = map.remove("fatal") == Some(b"true".as_slice());
            let stats = map.remove("stats").map(<[u8]>::to_vec);
            return Ok(Line::Error(ErrorLine {
                attr,
                attr_path,
                error,
                fatal,
                stats,
            }));
        }
        let parse_path = |s: String| StorePath::parse(&s).map_err(Error::from);
        let drv_path = parse_path(take_str(&mut map, "drvPath")?)?;
        let raw_outputs = map
            .remove("outputs")
            .ok_or_else(|| Error::Protocol("job without 'outputs'".into()))?;
        let outputs: BTreeMap<String, Option<String>> = serde_json::from_slice(raw_outputs)
            .map_err(|e| Error::Protocol(format!("'outputs': {e}")))?;
        let outputs = outputs
            .into_iter()
            .map(|(k, v)| Ok((k, v.map(parse_path).transpose()?)))
            .collect::<Result<_>>()?;
        let constituents = take_strs(&mut map, "constituents")?
            .into_iter()
            .map(&parse_path)
            .collect::<Result<_>>()?;
        let named_constituents = match map.remove("namedConstituents") {
            Some(raw) => Some(
                serde_json::from_slice::<Vec<String>>(raw)
                    .map_err(|e| Error::Protocol(format!("'namedConstituents': {e}")))?,
            ),
            None => None,
        };
        let glob_constituents = map.remove("globConstituents") == Some(b"true".as_slice());
        let extra = map.into_iter().map(|(k, v)| (k, v.to_vec())).collect();
        Ok(Line::Job(Job {
            attr,
            attr_path,
            drv_path,
            outputs,
            constituents,
            named_constituents,
            glob_constituents,
            extra,
        }))
    }
}

fn render_fields(fields: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![b'{'];
    let mut first = true;
    for (k, v) in fields {
        if !first {
            out.push(b',');
        }
        first = false;
        out.extend(json_str(k));
        out.push(b':');
        out.extend_from_slice(v);
    }
    out.push(b'}');
    out
}

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// `i` is at the opening quote; returns the index after the closing one.
fn scan_string(s: &[u8], i: usize) -> Result<usize> {
    let mut j = i + 1;
    while j < s.len() {
        match s[j] {
            b'\\' => j += 2,
            b'"' => return Ok(j + 1),
            _ => j += 1,
        }
    }
    Err(Error::Protocol("unterminated string".into()))
}

fn scan_value(s: &[u8], i: usize) -> Result<usize> {
    match s.get(i) {
        None => Err(Error::Protocol("missing value".into())),
        Some(b'"') => scan_string(s, i),
        Some(b'{') | Some(b'[') => {
            let mut depth = 0usize;
            let mut j = i;
            while j < s.len() {
                match s[j] {
                    b'"' => j = scan_string(s, j)?,
                    b'{' | b'[' => {
                        depth += 1;
                        j += 1;
                    }
                    b'}' | b']' => {
                        depth -= 1;
                        j += 1;
                        if depth == 0 {
                            return Ok(j);
                        }
                    }
                    _ => j += 1,
                }
            }
            Err(Error::Protocol("unterminated object".into()))
        }
        Some(_) => {
            let mut j = i;
            while j < s.len() && !matches!(s[j], b',' | b'}' | b']' | b' ' | b'\n' | b'\t' | b'\r')
            {
                j += 1;
            }
            Ok(j)
        }
    }
}

/// The top-level keys of a JSON object and the raw text of their values.
pub fn split_object(s: &[u8]) -> Result<Vec<(String, &[u8])>> {
    let mut i = skip_ws(s, 0);
    if s.get(i) != Some(&b'{') {
        return Err(Error::Protocol("expected '{'".into()));
    }
    i = skip_ws(s, i + 1);
    let mut out = Vec::new();
    if s.get(i) == Some(&b'}') {
        return Ok(out);
    }
    loop {
        if s.get(i) != Some(&b'"') {
            return Err(Error::Protocol("expected a key".into()));
        }
        let end = scan_string(s, i)?;
        let key: String =
            serde_json::from_slice(&s[i..end]).map_err(|e| Error::Protocol(e.to_string()))?;
        i = skip_ws(s, end);
        if s.get(i) != Some(&b':') {
            return Err(Error::Protocol("expected ':'".into()));
        }
        i = skip_ws(s, i + 1);
        let end = scan_value(s, i)?;
        out.push((key, &s[i..end]));
        i = skip_ws(s, end);
        match s.get(i) {
            Some(b',') => i = skip_ws(s, i + 1),
            Some(b'}') => return Ok(out),
            _ => return Err(Error::Protocol("expected ',' or '}'".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_round_trip() {
        let line = br#"{"attr":"a.b","attrPath":["a","b"],"constituents":[],"drvPath":"/nix/store/00000000000000000000000000000000-x.drv","globConstituents":false,"meta":{"x":[1,"}"]},"name":"x","namedConstituents":["n"],"outputs":{"out":"/nix/store/00000000000000000000000000000000-x"},"storeDir":"/nix/store","system":"x86_64-linux"}"#;
        let fields = split_object(line).unwrap();
        assert_eq!(fields.len(), 11);
        assert_eq!(fields[5].0, "meta");
        assert_eq!(fields[5].1, br#"{"x":[1,"}"]}"#);
        let parsed = Line::parse(line).unwrap();
        assert_eq!(parsed.render(), line.to_vec());
        let Line::Job(mut j) = parsed else { panic!() };
        assert_eq!(
            j.named_constituents.as_deref(),
            Some(&["n".to_string()][..])
        );
        j.named_constituents = None;
        assert!(
            !String::from_utf8(j.render())
                .unwrap()
                .contains("namedConstituents")
        );
    }

    #[test]
    fn error_line_round_trip() {
        let line = br#"{"attr":"d","attrPath":["d"],"error":"error:\n  x","fatal":false}"#;
        let parsed = Line::parse(line).unwrap();
        assert!(matches!(parsed, Line::Error(_)));
        assert_eq!(parsed.render(), line.to_vec());
    }

    #[test]
    fn stats_survive_the_collector() {
        let stats = String::from_utf8(stats_json(412, 7632)).unwrap();
        assert_eq!(stats, r#"{"allocBytes":7632,"wallMs":412}"#);
        let error =
            format!(r#"{{"attr":"d","attrPath":["d"],"error":"x","fatal":false,"stats":{stats}}}"#);
        assert_eq!(
            Line::parse(error.as_bytes()).unwrap().render(),
            error.into_bytes()
        );
        let job = format!(
            r#"{{"attr":"a","attrPath":["a"],"constituents":[],"drvPath":"/nix/store/00000000000000000000000000000000-x.drv","globConstituents":false,"name":"x","outputs":{{}},"stats":{stats},"system":"x86_64-linux"}}"#
        );
        assert_eq!(
            Line::parse(job.as_bytes()).unwrap().render(),
            job.into_bytes()
        );
    }
}
