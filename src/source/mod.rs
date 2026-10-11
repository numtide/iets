//! Where a tree comes from, and how it gets here.
//!
//! A `SourceRef` names one: a local directory, copied the way Nix copies it,
//! or a locked input, named by the `narHash` its lock gives. `tree` resolves
//! a reference into a `Tree` — a store path plus the attributes the tree
//! hashed to. The directory and working tree walks (`local`, `scm`), the
//! fetchers and the input store (`remote`, `crate::treestore`) are behind
//! that call, and `Lock` is the `flake.lock` that names the locked
//! references.
//!
//! Every name here comes from `treestore::store_path`: it is the content
//! address the input store keys its entries by, so nothing may compute a
//! store path for a tree another way.

mod local;
pub mod lock;
mod remote;
pub mod scm;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::error;
use crate::eval::{EvalResult, Evaluator, Pos};
use crate::store::hash::{Hash, HashAlgo};
use crate::store::path::StorePath;

pub(crate) use local::{commit_tree, path_tree, workdir};
pub use lock::Lock;
pub use remote::LazyStore;
pub(crate) use remote::{
    ensure_readable, fetch_file, fetch_git_with_nix, store_commit, valid_in_store,
};
pub(crate) use remote::nix_conf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttrVal {
    Str(String),
    Bool(bool),
    Int(u64),
}

/// fetchers::Attrs: the input attributes in name order.
pub type Attrs = BTreeMap<String, AttrVal>;

pub fn str_attr<'a>(a: &'a Attrs, name: &str) -> Option<&'a str> {
    match a.get(name) {
        Some(AttrVal::Str(s)) => Some(s),
        _ => None,
    }
}

pub(crate) fn int_attr(a: &Attrs, name: &str) -> Option<u64> {
    match a.get(name) {
        Some(AttrVal::Int(n)) => Some(*n),
        _ => None,
    }
}

pub(crate) fn bool_attr(a: &Attrs, name: &str) -> Option<bool> {
    match a.get(name) {
        Some(AttrVal::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// `flake:...`-style display of a reference for messages; not a parseable ref.
pub fn to_url(a: &Attrs) -> String {
    let mut parts = Vec::new();
    for (k, v) in a {
        let v = match v {
            AttrVal::Str(s) => s.clone(),
            AttrVal::Bool(b) => b.to_string(),
            AttrVal::Int(n) => n.to_string(),
        };
        parts.push(format!("{k}={v}"));
    }
    parts.join("&")
}

pub fn parse_nar_hash(s: &str) -> error::Result<Hash> {
    Hash::parse_any(s, Some(HashAlgo::Sha256)).map_err(Into::into)
}

/// `%Y%m%d%H%M%S` of a Unix time in UTC (Howard Hinnant's civil_from_days).
pub fn last_modified_date(t: i64) -> String {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}{:02}{:02}{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Where a tree comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceRef {
    /// A local directory, copied whole (path.cc).
    Path(PathBuf),
    /// A directory inside the working tree of an SCM: only what the SCM
    /// tracks is copied, with the working tree's contents
    /// (git.cc:getAccessorFromWorkdir).
    Tracked {
        scm: &'static dyn scm::Scm,
        root: PathBuf,
        subdir: String,
    },
    /// One revision of a local repository, which the SCM reads out of it
    /// (git.cc:getAccessorFromCommit). `git+file:///path?rev=…` names this,
    /// and it is the tree of the commit, not of the working tree beside it.
    Commit {
        scm: &'static dyn scm::Scm,
        root: PathBuf,
        rev: String,
        subdir: String,
    },
    /// A locked input: a `narHash` is known before anything is fetched, so
    /// the tree names itself and verifies itself.
    Locked(Attrs),
}

impl SourceRef {
    /// The `narHash` a locked reference carries, as the lock spells it.
    pub fn nar_hash(&self) -> Option<&str> {
        match self {
            SourceRef::Locked(a) => str_attr(a, "narHash"),
            _ => None,
        }
    }
}

/// A tree that is there: the store path it is named by, and the input
/// attributes it resolved to, `narHash` filled in (`emit_tree_attrs` renders
/// those). `subdir` is where a flake sits inside it.
pub struct Tree {
    pub path: StorePath,
    pub attrs: Attrs,
    pub subdir: String,
    /// The SCM and the root of a working tree that has a `revCount`. The
    /// attribute is not in `attrs`: the SCM counts when the evaluation
    /// reads it (`scm::rev_count`).
    pub counter: Option<(&'static dyn scm::Scm, PathBuf)>,
}

impl Tree {
    pub fn nar_hash(&self) -> Option<Hash> {
        str_attr(&self.attrs, "narHash").and_then(|h| parse_nar_hash(h).ok())
    }

    pub fn last_modified(&self) -> Option<i64> {
        int_attr(&self.attrs, "lastModified").map(|t| t as i64)
    }

    pub fn rev(&self) -> Option<&str> {
        str_attr(&self.attrs, "rev")
    }
}

/// The tree a reference names, copied or fetched if it is not here yet. The
/// caller gets a store path that can be read, hashed and uploaded.
pub fn tree(ev: &mut Evaluator, r: &SourceRef, pos: Pos) -> EvalResult<Tree> {
    match r {
        SourceRef::Path(dir) => local::path_source(ev, dir, pos),
        SourceRef::Tracked { scm, root, subdir } => {
            local::tracked_source(ev, *scm, root, subdir, pos)
        }
        SourceRef::Commit {
            scm,
            root,
            rev,
            subdir,
        } => local::commit_source(ev, *scm, root, rev, subdir, pos),
        SourceRef::Locked(attrs) => remote::locked(ev, attrs, pos),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(last_modified_date(1789565004), "20260916132324");
        assert_eq!(last_modified_date(1700000000), "20231114221320");
        assert_eq!(last_modified_date(0), "19700101000000");
        assert_eq!(last_modified_date(951782400), "20000229000000");
    }
}
