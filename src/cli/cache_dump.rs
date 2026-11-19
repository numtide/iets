//! `iets cache dump` (also `cache-dump`): the read set of a tracing-cache entry as text.

use crate::cli::render;
use crate::error;
use crate::trace::store::{Entry, Part};
use crate::trace::{Obs, wire};

use std::path::Path;

/// One line per observation of an `entry` file, for debugging misses.
pub fn cmd_cache_dump(file: &str) -> i32 {
    let obs = Entry::open(Path::new(file))
        .and_then(|e| e.part(Part::Readset))
        .and_then(|b| wire::decode_readset(&b));
    let Some(obs) = obs else {
        return error::Error::Cache(format!("{file} is not a cache entry")).report();
    };
    let hex = |d: &[u8]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    for o in &obs {
        match o {
            Obs::File {
                path, size, hash, ..
            } => render::line(&format!("file {} {size} {}", s(path), &hex(hash)[..16])),
            Obs::Dir { path, hash } => {
                render::line(&format!("dir {} {}", s(path), &hex(hash)[..16]))
            }
            Obs::Stat { path, follow, kind } => {
                render::line(&format!("stat {} follow={follow} {kind:?}", s(path)))
            }
            Obs::Tree { root, filtered, .. } => render::line(&format!(
                "tree {} accepted={}",
                s(root),
                filtered
                    .as_ref()
                    .map_or("all".to_string(), |f| f.accepted.len().to_string())
            )),
            Obs::Canon { path, target } => render::line(&format!(
                "canon {} -> {}",
                s(path),
                target.as_deref().map_or("(none)".to_string(), s)
            )),
            // the value is a keyed hash; the entry never holds it
            Obs::Env { name, value } => render::line(&format!(
                "env {} {}",
                s(name),
                value.map_or("(unset)".to_string(), |h| hex(&h)[..16].to_string())
            )),
            Obs::Cmd { dir, query, hash } => render::line(&format!(
                "cmd {} `{}` {}",
                s(dir),
                query.argv().join(" "),
                &hex(hash)[..16]
            )),
            Obs::Input { entry, version } => {
                render::line(&format!("input {} v{version}", s(entry)))
            }
            Obs::ImportPath { path, target } => {
                render::line(&format!("import {} -> {}", s(path), s(target)))
            }
        }
    }
    0
}
