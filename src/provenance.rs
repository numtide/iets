//! Provenance: store paths back to the local trees they came from, and
//! derivations back to the attribute and the source position that defined
//! them. See docs/explanation/provenance.md.
//!
//! `IETS_PROVENANCE=0` turns every rewrite off, so a byte comparison against
//! the real nix sees the plain store paths.

use std::collections::HashMap;

use crate::eval::{Evaluator, Pos};
use crate::session::Session;
use crate::store::path::{STORE_DIR, StorePath};
use crate::store::source::SourceOrigin;

pub fn enabled() -> bool {
    Session::current().settings().provenance
}

/// How many times a mapping may point at another mapped tree.
const MAX_HOPS: usize = 8;

/// A byte a store path's base name may hold (path.cc:checkName).
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.' | b'_' | b'?' | b'=')
}

/// A pseudo-file (`«derivation-internal»`, `«arg»`) rather than a real path.
fn is_pseudo(path: &[u8]) -> bool {
    path.starts_with("\u{ab}".as_bytes())
}

#[derive(Clone, Debug)]
struct Entry {
    prefix: Vec<u8>,
    local: Vec<u8>,
}

/// Store-path prefix to local directory, longest match first.
#[derive(Clone, Debug, Default)]
pub struct SourceMap {
    /// Sorted by prefix, descending: the longest match of any prefix of a
    /// path is the first entry that matches.
    entries: Vec<Entry>,
}

impl SourceMap {
    /// Adds one mapping. Call `sort` once every mapping is in.
    pub fn insert(&mut self, prefix: &[u8], local: &[u8]) {
        self.entries.push(Entry {
            prefix: prefix.to_vec(),
            local: local.to_vec(),
        });
    }

    /// Orders the entries for matching and resolves the chains: a tree
    /// copied from a tree that is itself in the store (`./.` inside a flake
    /// already in the store) must land on the local directory, not on the
    /// store path in between.
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| b.prefix.cmp(&a.prefix));
        self.entries.dedup_by(|a, b| a.prefix == b.prefix);
        for _ in 0..MAX_HOPS {
            let mut changed = false;
            let resolved: Vec<Option<Vec<u8>>> =
                self.entries.iter().map(|e| self.locate(&e.local)).collect();
            for (e, r) in self.entries.iter_mut().zip(resolved) {
                if let Some(r) = r
                    && r != e.local
                {
                    e.local = r;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    fn find(&self, path: &[u8]) -> Option<&Entry> {
        self.entries.iter().find(|e| path.starts_with(&e.prefix))
    }

    /// The local path `path` came from, with the tail below the mapped
    /// prefix kept.
    pub fn locate(&self, path: &[u8]) -> Option<Vec<u8>> {
        let e = self.find(path)?;
        let mut out = e.local.clone();
        out.extend_from_slice(&path[e.prefix.len()..]);
        Some(out)
    }

    /// Every store path this map knows, replaced by its local path. Only
    /// the `/nix/store/<base>` segments are looked at, so a long log line
    /// costs one search per store path in it.
    pub fn rewrite(&self, text: &[u8]) -> Vec<u8> {
        if self.entries.is_empty() {
            return text.to_vec();
        }
        let store = STORE_DIR.as_bytes();
        let mut out = Vec::with_capacity(text.len());
        let mut i = 0;
        while i < text.len() {
            if text[i] == store[0]
                && text[i..].starts_with(store)
                && text[i..].len() > store.len()
                && text[i + store.len()] == b'/'
            {
                let start = i + store.len() + 1;
                let end = start
                    + text[start..]
                        .iter()
                        .position(|&b| !is_name_byte(b))
                        .unwrap_or(text.len() - start);
                if let Some(local) = self.locate(&text[i..end]) {
                    out.extend_from_slice(&local);
                    i = end;
                    continue;
                }
            }
            out.push(text[i]);
            i += 1;
        }
        out
    }

    /// The trees this evaluation copied, plus the `source-map` setting.
    pub fn from_eval(ev: &Evaluator) -> SourceMap {
        SourceMap::from_sources(&ev.known.origins())
    }

    /// The same map from a set of source records, for a run that replayed
    /// them from the cache instead of evaluating.
    pub fn from_sources(sources: &HashMap<StorePath, SourceOrigin>) -> SourceMap {
        let mut m = SourceMap::default();
        for (p, o) in sources.iter() {
            if let Some(local) = o.at.local() {
                m.insert(
                    p.to_string().as_bytes(),
                    local.as_os_str().as_encoded_bytes(),
                );
            }
        }
        for (store, local) in &Session::current().settings().source_map {
            m.insert(store.as_bytes(), local.as_bytes());
        }
        m.sort();
        m
    }
}

/// The map for the commands that cannot build one at a known point: the
/// eval-jobs walk and `report`, which both run while the evaluation is live.
/// Built on first use, so a tree copied later is not in it.
fn lazy_map(ev: &Evaluator) -> &SourceMap {
    ev.session()
        .source_map
        .get_or_init(|| SourceMap::from_eval(ev))
}

/// `file:line:col` of `pos`, the local path when the file is a copy of one.
/// `None` when the position is unknown or inside a pseudo-file.
pub fn describe_pos(ev: &Evaluator, pos: Pos) -> Option<String> {
    describe_pos_with(ev, lazy_map(ev), pos)
}

pub fn describe_pos_with(ev: &Evaluator, map: &SourceMap, pos: Pos) -> Option<String> {
    let (f, off) = ev.file_of(pos)?;
    if is_pseudo(&f.path) {
        return None;
    }
    let (l, c) = f.line_col(off);
    let path = if enabled() {
        map.locate(&f.path).unwrap_or_else(|| f.path.clone())
    } else {
        f.path.clone()
    };
    Some(format!("{}:{l}:{c}", String::from_utf8_lossy(&path)))
}

/// Store paths in `s` replaced by local paths, for an error message printed
/// while the evaluation is live.
pub fn rewrite_msg(ev: &Evaluator, s: &str) -> String {
    if !enabled() {
        return s.to_string();
    }
    String::from_utf8_lossy(&lazy_map(ev).rewrite(s.as_bytes())).into_owned()
}

/// `pos` unless it is unknown or in a pseudo-file.
pub fn real_pos(ev: &Evaluator, pos: Pos) -> Option<Pos> {
    let (f, _) = ev.file_of(pos)?;
    (!is_pseudo(&f.path)).then_some(pos)
}

/// The derivation name inside a `.drv` store path: no hash, no extension.
/// What a build line calls a derivation no attribute points at.
pub fn drv_name(drv_path: &str) -> String {
    let base = drv_path.rsplit('/').next().unwrap_or(drv_path);
    let name = base.strip_suffix(".drv").unwrap_or(base);
    match name.split_once('-') {
        Some((hash, rest)) if hash.len() == 32 => rest.to_string(),
        _ => name.to_string(),
    }
}

/// Where one derivation was defined.
#[derive(Clone, Debug, Default)]
pub struct Site {
    /// The attribute path the cli walked to it.
    pub attr: Option<String>,
    /// `file:line:col` of the `derivation` call, already mapped.
    pub pos: Option<String>,
}

/// Everything the build side needs after the evaluator is gone: the source
/// map and one `Site` per derivation this evaluation created.
#[derive(Clone, Debug, Default)]
pub struct Provenance {
    pub map: SourceMap,
    sites: HashMap<String, Site>,
    on: bool,
}

impl Provenance {
    /// Provenance that maps and names nothing.
    pub fn off() -> Provenance {
        Provenance::default()
    }

    /// Rewriting only: no derivation is named. This is what a run that hit
    /// the evaluation cache can offer, because the cache keeps no positions.
    pub fn from_map(map: SourceMap) -> Provenance {
        Provenance {
            map,
            sites: HashMap::new(),
            on: enabled(),
        }
    }

    pub fn snapshot(ev: &Evaluator) -> Provenance {
        if !enabled() {
            return Provenance::off();
        }
        let map = SourceMap::from_eval(ev);
        let mut sites = HashMap::new();
        for (p, info) in ev.drv.drvs.iter() {
            if info.attr.is_none() && info.pos.is_none() {
                continue;
            }
            sites.insert(
                p.to_string(),
                Site {
                    attr: info.attr.clone(),
                    pos: info.pos.and_then(|pos| describe_pos_with(ev, &map, pos)),
                },
            );
        }
        Provenance {
            map,
            sites,
            on: true,
        }
    }

    pub fn site(&self, drv_path: &str) -> Option<&Site> {
        if !self.on {
            return None;
        }
        self.sites.get(drv_path)
    }

    pub fn rewrite(&self, text: &[u8]) -> Vec<u8> {
        if !self.on {
            return text.to_vec();
        }
        self.map.rewrite(text)
    }

    pub fn rewrite_str(&self, s: &str) -> String {
        if !self.on {
            return s.to_string();
        }
        String::from_utf8_lossy(&self.map.rewrite(s.as_bytes())).into_owned()
    }

    /// The attribute alone, for a caller that shows the position itself.
    pub fn attr_of(&self, drv_path: &str) -> Option<String> {
        self.site(drv_path)?.attr.clone()
    }

    /// What is being built: `<attr> (<file>:<line>:<col>)`, as much as is
    /// known. `None` when nothing is.
    pub fn name_of(&self, drv_path: &str) -> Option<String> {
        let s = self.site(drv_path)?;
        let what = s.attr.clone().unwrap_or_else(|| drv_name(drv_path));
        match &s.pos {
            Some(p) => Some(format!("{what} ({p})")),
            None => Some(what),
        }
    }

    /// The `building ...` line, when provenance is known.
    pub fn started(&self, drv_path: &str) -> Option<String> {
        Some(format!("building {}", self.name_of(drv_path)?))
    }

    /// The `error: build of ... failed` line, when provenance is known.
    pub fn failed(&self, drv_path: &str, reason: &str) -> Option<String> {
        let s = self.site(drv_path)?;
        let base = drv_path.rsplit('/').next().unwrap_or(drv_path);
        let what = s.attr.clone().unwrap_or_else(|| drv_name(drv_path));
        let mut line = format!("error: build of {what} failed ({base})");
        if let Some(p) = &s.pos {
            line.push_str(&format!(", defined at {p}"));
        }
        if !reason.is_empty() {
            line.push_str(&format!(": {}", self.rewrite_str(reason)));
        }
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> SourceMap {
        let mut m = SourceMap::default();
        m.insert(b"/nix/store/aaaa-source", b"/home/u/repo");
        m.insert(b"/nix/store/bbbb-source", b"/home/u/nixpkgs");
        m.insert(b"/nix/store/aaaa-source-extra", b"/home/u/other");
        m.sort();
        m
    }

    #[test]
    fn locate_takes_the_longest_prefix() {
        let m = map();
        assert_eq!(
            m.locate(b"/nix/store/aaaa-source/default.nix").unwrap(),
            b"/home/u/repo/default.nix".to_vec()
        );
        assert_eq!(
            m.locate(b"/nix/store/aaaa-source-extra/x").unwrap(),
            b"/home/u/other/x".to_vec()
        );
        assert_eq!(
            m.locate(b"/nix/store/aaaa-source").unwrap(),
            b"/home/u/repo".to_vec()
        );
    }

    #[test]
    fn locate_without_a_match_is_none() {
        let m = map();
        assert!(m.locate(b"/nix/store/cccc-source/x").is_none());
        assert!(m.locate(b"/home/u/repo/default.nix").is_none());
        assert!(
            SourceMap::default()
                .locate(b"/nix/store/aaaa-source")
                .is_none()
        );
    }

    #[test]
    fn a_tree_copied_from_a_store_tree_resolves_to_the_local_one() {
        // `./.` inside a flake that is already in the store copies the store
        // tree again; the log line names the second copy.
        let mut m = SourceMap::default();
        m.insert(b"/nix/store/aaaa-source", b"/home/u/repo");
        m.insert(b"/nix/store/bbbb-aaaa-source", b"/nix/store/aaaa-source");
        m.sort();
        assert_eq!(
            m.locate(b"/nix/store/bbbb-aaaa-source/flake.nix").unwrap(),
            b"/home/u/repo/flake.nix".to_vec()
        );
        assert_eq!(
            String::from_utf8(m.rewrite(b"tree is /nix/store/bbbb-aaaa-source")).unwrap(),
            "tree is /home/u/repo"
        );
    }

    #[test]
    fn a_path_followed_by_a_colour_escape_still_matches() {
        let m = map();
        let line = b"at \x1b[35;1m/nix/store/aaaa-source/x.nix\x1b[0m!";
        assert_eq!(
            m.rewrite(line),
            b"at \x1b[35;1m/home/u/repo/x.nix\x1b[0m!".to_vec()
        );
    }

    #[test]
    fn rewrite_replaces_every_known_path_in_a_line() {
        let m = map();
        let line = b"cc: /nix/store/aaaa-source/src/a.c and '/nix/store/bbbb-source/x.h', \
                     not /nix/store/cccc-out/lib";
        let got = m.rewrite(line);
        assert_eq!(
            String::from_utf8(got).unwrap(),
            "cc: /home/u/repo/src/a.c and '/home/u/nixpkgs/x.h', \
             not /nix/store/cccc-out/lib"
        );
    }

    #[test]
    fn rewrite_leaves_unknown_text_alone() {
        let m = map();
        for line in [
            &b"no store path here"[..],
            b"/nix/store",
            b"/nix/store/",
            b"/nix/storeaaaa-source",
        ] {
            assert_eq!(m.rewrite(line), line.to_vec());
        }
        assert_eq!(
            SourceMap::default()
                .rewrite(b"/nix/store/aaaa-source/a")
                .len(),
            24
        );
    }

    #[test]
    fn failed_line_names_the_attribute_and_the_position() {
        let mut p = Provenance {
            map: map(),
            sites: HashMap::new(),
            on: true,
        };
        p.sites.insert(
            "/nix/store/dddd-x.drv".into(),
            Site {
                attr: Some("packages.x86_64-linux.hello".into()),
                pos: Some("/home/u/repo/default.nix:3:5".into()),
            },
        );
        assert_eq!(
            p.failed("/nix/store/dddd-x.drv", "builder failed").unwrap(),
            "error: build of packages.x86_64-linux.hello failed (dddd-x.drv), \
             defined at /home/u/repo/default.nix:3:5: builder failed"
        );
        assert_eq!(
            p.started("/nix/store/dddd-x.drv").unwrap(),
            "building packages.x86_64-linux.hello (/home/u/repo/default.nix:3:5)"
        );
        assert!(p.failed("/nix/store/eeee-y.drv", "x").is_none());
        assert!(
            Provenance::off()
                .failed("/nix/store/dddd-x.drv", "x")
                .is_none()
        );
    }
}
