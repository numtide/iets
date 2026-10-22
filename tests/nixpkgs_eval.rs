//! iets computes the same drvs as Nix for nixpkgs. A drv path hashes its
//! whole closure, so one path per attribute covers every input.
//!
//! Needs `IETS_TEST_NIXPKGS` (a nixpkgs source) and `IETS_TEST_NIX_EVAL_JOBS`
//! (the binary). `IETS_TEST_NIXPKGS_LIMIT` takes the first N attributes.

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;

use std::collections::BTreeMap;
use std::env;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

/// attr -> drvPath, or `None` for an attribute that failed.
type Jobs = BTreeMap<String, Option<String>>;

fn jobs(mut c: Command, nixpkgs: &str, limit: &str) -> Jobs {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/nixpkgs_eval.nix");
    // the drv paths are computed without writing them anywhere
    c.args(["--impure", "--no-instantiate", "--workers", "4"])
        .args(["--arg", "nixpkgs", nixpkgs])
        .args(["--arg", "limit", limit])
        .arg(file);
    let o = c.output().expect("spawn");
    assert!(
        o.status.success(),
        "{c:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let mut jobs = Jobs::new();
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let j: Value = serde_json::from_str(line).expect("a JSON line");
        let attr = j["attr"].as_str().expect("attr").to_string();
        let drv = match j.get("error") {
            Some(_) => None,
            None => Some(j["drvPath"].as_str().expect("drvPath").to_string()),
        };
        jobs.insert(attr, drv);
    }
    jobs
}

#[rstest]
#[ignore = "evaluates nixpkgs twice; set IETS_TEST_NIXPKGS"]
fn nixpkgs_drvs_match_nix(bin: Iets) {
    let (Ok(nixpkgs), Ok(nej)) = (
        env::var("IETS_TEST_NIXPKGS"),
        env::var("IETS_TEST_NIX_EVAL_JOBS"),
    ) else {
        eprintln!("skipped: IETS_TEST_NIXPKGS or IETS_TEST_NIX_EVAL_JOBS unset");
        return;
    };
    let limit = env::var("IETS_TEST_NIXPKGS_LIMIT").unwrap_or("null".into());
    // the check's HOME is not writable
    let cache = TempDir::new().unwrap();

    let mut theirs = Command::new(nej);
    // in memory: the check's sandbox has no daemon
    theirs
        .args(["--store", "dummy://?read-only=false"])
        .env("NIX_CONF_DIR", "/nonexistent")
        .env("NIX_USER_CONF_FILES", "")
        .env("XDG_CACHE_HOME", cache.path())
        .env_remove("NIX_CONFIG");
    let theirs = jobs(theirs, &nixpkgs, &limit);

    let mut ours = bin.cmd();
    // no daemon, as in the check's sandbox
    ours.arg("nix-eval-jobs")
        .env("NIX_DAEMON_SOCKET_PATH", "/nonexistent/socket")
        .env("XDG_CACHE_HOME", cache.path());
    let ours = jobs(ours, &nixpkgs, &limit);

    assert!(!theirs.is_empty());
    let differ: Vec<_> = theirs
        .iter()
        .filter(|(a, d)| ours.get(*a) != Some(d))
        .map(|(a, d)| format!("{a}: nix {d:?}, iets {:?}", ours.get(a)))
        .collect();
    assert!(
        differ.is_empty() && ours.len() == theirs.len(),
        "{} of {} attributes differ ({} from iets):\n{}",
        differ.len(),
        theirs.len(),
        ours.len(),
        differ.join("\n")
    );
}
