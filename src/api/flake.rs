//! Flakes, as an operation. A reference is parsed, its lock is read, its
//! tree is resolved and its outputs are evaluated; `crate::flake` is private
//! and nothing above this line reaches into it (docs/explanation/plumbing-and-porcelain.md).

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::eval::{EvalResult, Evaluator, Pos};
use crate::flake;
use crate::runtime::Word;
use crate::source::{self, AttrVal, Attrs, Lock, SourceRef, lock};
use crate::sys;

pub use crate::flake::FlakeRef;
pub use crate::source::Tree;

/// A command-line input that names a flake rather than a `.nix` file.
pub fn is_flake_ref(arg: &str) -> bool {
    flake::is_flake_ref(arg)
}

/// attr-path.cc:parseAttrPath: `.` separates, `"..."` quotes.
pub fn parse_attr_path(s: &str) -> Result<Vec<String>> {
    flake::parse_attr_path(s)
}

/// eval-cache.cc:AttrCursor::findAlongAttrPath: walks `path` from `v`,
/// forcing each step; `None` when a step is missing or is not an attrset.
pub fn select(ev: &mut Evaluator, v: Word, path: &[String]) -> EvalResult<Option<Word>> {
    flake::select(ev, v, path)
}

/// The flake's outputs, with the reference's attribute path selected from
/// them. The lock is read, the inputs are fetched and `call-flake.nix` runs.
pub fn outputs(ev: &mut Evaluator, fr: &FlakeRef) -> EvalResult<Word> {
    flake::eval_flake(ev, fr)
}

/// A local path as a URL carries it: `/C:/x` for a Windows drive.
fn url_path(p: &Path) -> String {
    String::from_utf8_lossy(&sys::path_to_internal(p)).into_owned()
}

/// flakeref.cc:FlakeRef::to_string for the references iets parses: how a
/// local flake names itself in a message.
pub fn url(fr: &FlakeRef) -> String {
    match &fr.source {
        SourceRef::Tracked { scm, root, subdir } => {
            let dir = match subdir.as_str() {
                "" => String::new(),
                d => format!("?dir={d}"),
            };
            format!("{}+file://{}{dir}", scm.name(), url_path(root))
        }
        SourceRef::Submodules { root, subdir } => {
            let dir = match subdir.as_str() {
                "" => String::new(),
                d => format!("&dir={d}"),
            };
            format!("git+file://{}?submodules=1{dir}", url_path(root))
        }
        _ => format!("path:{}", fr.dir.display()),
    }
}

/// The tree a reference names, copied or fetched if it is not here yet.
pub fn tree(ev: &mut Evaluator, r: &SourceRef) -> EvalResult<Tree> {
    source::tree(ev, r, Pos::NONE)
}

/// What a flake names before anything is evaluated: the directory, where its
/// own tree comes from, and what its `flake.lock` locked.
pub struct Info {
    pub dir: PathBuf,
    pub source: SourceRef,
    pub lock: Lock,
}

/// Reads `FLAKEREF/flake.lock` and checks it. Nothing is fetched and
/// nothing is evaluated.
pub fn info(fr: &FlakeRef) -> Result<Info> {
    let lock = fs::read(fr.dir.join("flake.lock"))
        .map_err(|e| e.to_string())
        .and_then(|b| lock::check(&b).map_err(|e| e.to_string()))
        .map_err(|why| Error::Flake(format!("flake.lock: {why}")))?;
    Ok(Info {
        dir: fr.dir.clone(),
        source: fr.source.clone(),
        lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submodule_url_preserves_subdirectory() {
        let root = std::env::current_dir().unwrap();
        let mut fr = FlakeRef {
            dir: root.join("nested"),
            source: SourceRef::Submodules {
                root: root.clone(),
                subdir: "nested".into(),
            },
            attr: vec![],
            probed: false,
            lock: None,
        };
        assert_eq!(
            url(&fr),
            format!("git+file://{}?submodules=1&dir=nested", url_path(&root))
        );
        fr.source = SourceRef::Submodules {
            root: root.clone(),
            subdir: String::new(),
        };
        assert_eq!(
            url(&fr),
            format!("git+file://{}?submodules=1", url_path(&root))
        );
    }
}

impl Info {
    /// Every locked input the lock reaches, with the name the lock gives it.
    /// A `path` input is left out: it is read in place, and a relative one
    /// is the root's own tree.
    pub fn inputs(&self) -> Result<Vec<(String, SourceRef)>> {
        let mut out = Vec::new();
        for (name, locked) in self.lock.reached_inputs() {
            let mut attrs: Attrs = locked
                .iter()
                .filter_map(|(k, v)| {
                    let v = match v {
                        serde_json::Value::String(s) => AttrVal::Str(s.clone()),
                        serde_json::Value::Bool(b) => AttrVal::Bool(*b),
                        v => AttrVal::Int(v.as_u64()?),
                    };
                    Some((k.clone(), v))
                })
                .collect();
            match (locked["type"].as_str(), locked["narHash"].as_str()) {
                (Some("path"), _) => {}
                (Some("github" | "gitlab" | "sourcehut" | "tarball" | "git" | "hg"), Some(h)) => {
                    let h = source::parse_nar_hash(h)
                        .map_err(|e| Error::Flake(format!("input '{name}': {e}")))?;
                    attrs.insert("narHash".into(), AttrVal::Str(h.to_sri()));
                    out.push((name.to_string(), SourceRef::Locked(attrs)));
                }
                (t, _) => return Err(Error::Flake(format!("input '{name}' of type {t:?}"))),
            }
        }
        Ok(out)
    }
}
