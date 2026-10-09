//! `lock`: the one writer of the lock file that `nix/lock.nix` reads
//! (docs/explanation/locked-hashes.md). Evaluation only reads the file.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde_json::{Map, Value};

use crate::api::fetch::url_hash;
use crate::error::{Error, Result};
use crate::store::hash::HashAlgo;

const VERSION: u64 = 1;

/// The command `nix/lock.nix` names in its error; keep the two in step.
fn command(file: &str, unpack: bool, update: bool, url: &str) -> String {
    let mut c = String::from("iets lock");
    if file != "iets.lock" {
        c += &format!(" --file {file}");
    }
    if unpack {
        c += " --unpack";
    }
    if update {
        c += " --update";
    }
    format!("{c} {url}")
}

/// Every URL is fetched before the file is written, so a failure or a
/// changed remote leaves the file as it was.
pub fn lock(file: &str, unpack: bool, update: bool, urls: &[String]) -> Result<()> {
    let mut doc = read(file)?;
    let mode = if unpack { "unpack" } else { "file" };
    let table = doc
        .entry(mode)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| Error::Store(format!("{file}: '{mode}' is not an object")))?;
    for url in urls {
        let got = url_hash(url, unpack, HashAlgo::Sha256)?.to_sri();
        if let Some(old) = table.get(url).and_then(Value::as_str)
            && old != got
            && !update
        {
            return Err(Error::Store(format!(
                "the remote changed: {file} has {old} for {url}, and it serves {got} now.\n\
                 Check what changed, then run:\n  {}",
                command(file, unpack, true, url)
            )));
        }
        table.insert(url.clone(), Value::String(got));
    }
    let mut text = serde_json::to_string_pretty(&doc).expect("a JSON object serializes");
    text.push('\n');
    let tmp = format!("{file}.{}", std::process::id());
    fs::write(&tmp, text)
        .and_then(|()| fs::rename(&tmp, file))
        .map_err(|e| {
            let _ = fs::remove_file(&tmp);
            Error::io(format!("cannot write {file}"), e)
        })
}

fn read(file: &str) -> Result<Map<String, Value>> {
    let text = match fs::read_to_string(Path::new(file)) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok(Map::from_iter([("version".into(), VERSION.into())]));
        }
        Err(e) => return Err(Error::io(format!("cannot read {file}"), e)),
    };
    let doc: Map<String, Value> = serde_json::from_str(&text)
        .map_err(|e| Error::Store(format!("{file} is not a lock file: {e}")))?;
    match doc.get("version").and_then(Value::as_u64) {
        Some(VERSION) => Ok(doc),
        v => Err(Error::Store(format!(
            "{file}: version {} is not supported; iets reads version {VERSION}",
            v.map_or("(missing)".into(), |v| v.to_string())
        ))),
    }
}
