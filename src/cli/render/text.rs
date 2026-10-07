//! The text layout of the values `crate::api::render` produces: what a
//! terminal shows in place of the contract shape.

use crate::api::render::{Closure, Fetched, Missing, Outcomes};
use crate::log;
use crate::store::drvdiff::Diff;
use crate::store::path;

use super::{Text, status};

use std::collections::BTreeSet;

/// shared.cc:printMissing at lvlError, as `nix build --dry-run` prints it.
/// It is a log record, not output: `--json` prints the contract beside it,
/// as Nix does.
pub fn missing(m: &Missing) {
    let sizes = format!(
        "{} download, {} unpacked",
        super::size(m.download_size),
        super::size(m.nar_size)
    );
    let unknown = "don't know how to build these paths:".to_string();
    let sections = [
        (
            &m.will_build,
            "this derivation will be built:".into(),
            format!("these {} derivations will be built:", m.will_build.len()),
        ),
        (
            &m.will_substitute,
            format!("this path will be fetched ({sizes}):"),
            format!(
                "these {} paths will be fetched ({sizes}):",
                m.will_substitute.len()
            ),
        ),
        (&m.unknown, unknown.clone(), unknown),
    ];
    for (paths, one, many) in sections {
        if !paths.is_empty() {
            log::msg(0, if paths.len() == 1 { &one } else { &many });
            paths.iter().for_each(|p| log::msg(0, &format!("  {p}")));
        }
    }
}

/// One store path per output: what a caller of `iets build` captures.
impl Text for Outcomes<'_> {
    fn text(&self) -> String {
        let paths = self.0.iter().flat_map(|o| o.outputs.values());
        paths.map(|p| format!("{p}\n")).collect()
    }
}

/// The closing line of a build of several roots.
pub fn built(o: &Outcomes) {
    let failed: Vec<&str> = (o.0.iter().filter(|o| !o.success))
        .map(|o| o.attr.as_str())
        .collect();
    let mut line = format!(
        "{} built, {} failed",
        o.0.len() - failed.len(),
        failed.len()
    );
    if !failed.is_empty() {
        line = format!("{line}: {}", failed.join(", "));
    }
    status(&line);
}

impl Text for Fetched {
    fn text(&self) -> String {
        format!("{}\n", self.hash)
    }
}

impl Text for Closure {
    fn text(&self) -> String {
        if self.is_empty() {
            return "the two closures hold the same packages\n".into();
        }
        let join = |v: &BTreeSet<String>| v.iter().cloned().collect::<Vec<_>>().join(", ");
        (self.packages.iter())
            .map(|c| match c.change {
                "added" => format!("added   {} {}\n", c.name, join(&c.b)),
                "removed" => format!("removed {} {}\n", c.name, join(&c.a)),
                _ => format!("changed {} {} -> {}\n", c.name, join(&c.a), join(&c.b)),
            })
            .collect()
    }
}

impl Text for Diff {
    fn text(&self) -> String {
        if self.is_empty() {
            return format!("{} and {} are the same derivation\n", self.a, self.b);
        }
        let mut s = String::new();
        node(self, 0, &mut s);
        s
    }
}

/// One node of the walk: its own differences, then each input derivation
/// that differs, indented.
fn node(d: &Diff, ind: usize, s: &mut String) {
    let (pad, body) = (" ".repeat(ind), " ".repeat(ind + 2));
    s.push_str(&format!("{pad}- {}\n{pad}+ {}\n", d.a, d.b));
    for i in &d.items {
        let (what, key) = (i.what, &i.key);
        let entry = |v: &str| match key.is_empty() {
            true => one_line(v),
            false => format!("{key}={}", one_line(v)),
        };
        match (&i.a, &i.b) {
            (Some(x), None) => s.push_str(&format!("{body}{what} removed: {}\n", entry(x))),
            (None, Some(y)) => s.push_str(&format!("{body}{what} added: {}\n", entry(y))),
            (Some(x), Some(y)) if key.is_empty() && short(x, y) => {
                s.push_str(&format!("{body}{what}: {x} -> {y}\n"));
            }
            (Some(x), Some(y)) => {
                let head = match key.is_empty() {
                    true => what.to_string(),
                    false => format!("{what} {key}"),
                };
                s.push_str(&format!("{body}{head}:\n"));
                for l in line_diff(x, y, 20) {
                    s.push_str(&format!("{body}  {l}\n"));
                }
            }
            (None, None) => {}
        }
    }
    for c in &d.changed {
        s.push_str(&format!("{body}input drv changed: {}\n", drv_name(&c.a)));
        node(c, ind + 4, s);
    }
    if let Some(c) = d.cut {
        s.push_str(&format!("{body}(stopped: {c}; use --all)\n"));
    }
}

/// Two values that fit on one line together.
fn short(a: &str, b: &str) -> bool {
    a.len() + b.len() < 100 && !a.contains('\n') && !b.contains('\n')
}

/// `NAME` of `/nix/store/<hash>-NAME.drv`.
fn drv_name(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    let name = base.get(path::HASH_PART_LEN + 1..).unwrap_or(base);
    name.strip_suffix(".drv").unwrap_or(name)
}

/// A value on one line of output: newlines shown as `\n`, long values cut.
fn one_line(v: &str) -> String {
    let s: String = v.chars().take(200).collect();
    let cut = if v.chars().count() > 200 { "…" } else { "" };
    format!("{}{cut}", s.replace('\n', "\\n"))
}

/// A compact diff of two values: the common head and tail lines are left
/// out, and at most `cap` lines of each side are shown.
fn line_diff(a: &str, b: &str, cap: usize) -> Vec<String> {
    let (la, lb): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    if la.len() < 2 && lb.len() < 2 {
        return vec![format!("- {}", one_line(a)), format!("+ {}", one_line(b))];
    }
    let head = la.iter().zip(&lb).take_while(|(x, y)| x == y).count();
    let (ra, rb) = (&la[head..], &lb[head..]);
    let tail = (ra.iter().rev().zip(rb.iter().rev()))
        .take_while(|(x, y)| x == y)
        .count();
    let mut out: Vec<String> = (head > 0)
        .then(|| format!("@@ line {}", head + 1))
        .into_iter()
        .collect();
    for (mark, lines) in [('-', &ra[..ra.len() - tail]), ('+', &rb[..rb.len() - tail])] {
        out.extend(
            lines
                .iter()
                .take(cap)
                .map(|l| format!("{mark} {}", one_line(l))),
        );
        if lines.len() > cap {
            out.push(format!("{mark} … {} more lines", lines.len() - cap));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_long_value_shows_the_lines_that_differ() {
        let (a, b) = ("one\ntwo\nthree\nfour", "one\ntwo two\nthree\nfour");
        assert_eq!(
            super::line_diff(a, b, 20),
            ["@@ line 2", "- two", "+ two two"]
        );
        assert_eq!(super::line_diff("x", "y", 20), ["- x", "+ y"]);
    }
}
