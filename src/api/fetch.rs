//! `fetch`: a URL or a fixed-output derivation becomes a hash. The two
//! frontends are `iets plumbing fetch` and `iets prefetch`
//! (docs/reference/plumbing.md, docs/reference/commands.md). Nothing is added to a store.

use std::sync::Arc;
use std::{io, slice};

use crate::api::build as apibuild;
use crate::build::render::Style;
use crate::build::{Dispatcher, Reporter, Target, backend};
use crate::builtins::derivation::DrvInfo;
use crate::error::{Error, Result};
use crate::eval::Evaluator;
use crate::fetch::http::{self, Limits};
use crate::fetch::spool::{self, Tree};
use crate::provenance::Provenance;
use crate::remote::worker;
use crate::store::drv::{Derivation, DerivationOutput};
use crate::store::hash::{Hash, HashAlgo, Hasher};
use crate::store::path::StorePath;
use crate::syntax::symbol::FxHashMap;
use crate::{config, log};

/// The flat hash of the bytes `url` serves, or with `unpack` the NAR hash of
/// the tree the archive holds (`docs/reference/flakes.md` builds the same tree).
pub fn url_hash(url: &str, unpack: bool, algo: HashAlgo) -> Result<Hash> {
    let cfg = config::Config::read(&[]).unwrap_or_else(|_| config::Config::empty());
    let limits = Limits::new(&cfg)?;
    let mut h = Hasher::new(algo);
    if !unpack {
        http::get(url, &[], limits, false, &mut h)?;
        return Ok(h.finish());
    }
    let mut spool = spool::scratch()?;
    http::get(url, &[], limits, true, &mut spool)?;
    Tree::index(spool, limits.max_size)?
        .dump(&mut h)
        .map_err(|e| Error::io("cannot hash the unpacked tree", e))?;
    Ok(h.finish())
}

/// The hash a fixed-output derivation this run evaluated really produces,
/// and the derivation the answer came from: `root` with a fake hash, built so
/// the mismatch reports the real one.
pub fn drv_hash(ev: &Evaluator, root: &StorePath) -> Result<(String, StorePath)> {
    let mut drvs = ev.drv.drvs.lock().clone();
    let (rewritten, name) = rewrite(root, &mut drvs)?;
    let hash = build(ev, &rewritten, &name, &drvs)?;
    Ok((hash, rewritten))
}

/// `root` with its fixed-output hash replaced by the all-zero hash of the same
/// algorithm, added to `drvs` under the path that rewrite gives it: the output
/// path follows the hash, and the drv path follows the ATerm. So the build
/// reports the hash it really produced, and nothing on disk had to change.
fn rewrite(
    root: &StorePath,
    drvs: &mut FxHashMap<StorePath, DrvInfo>,
) -> Result<(StorePath, String)> {
    let bad = |why: &str| Error::Usage(format!("cannot prefetch '{root}': {why}"));
    let info = drvs
        .get(root)
        .cloned()
        .ok_or_else(|| bad("not evaluated"))?;
    let aterm = info.aterm.clone().ok_or_else(|| bad("it has no ATerm"))?;
    let mut drv = Derivation::parse(&aterm, Derivation::name_from_path(root)?)?;
    // a fixed-output derivation has exactly one output (derivationStrict)
    let Some((name, DerivationOutput::Fixed { method, hash })) = drv.outputs.iter().next() else {
        return Err(bad("not a fixed-output derivation: it has no 'hash'"));
    };
    let (name, method, algo) = (name.clone(), *method, hash.algo);
    let hash = Hash::from_bytes(algo, &vec![0; algo.size()]).map_err(|e| Error::Store(e.0))?;
    drv.outputs
        .insert(name.clone(), DerivationOutput::Fixed { method, hash });
    // only the output path moves: the `outputHash` the builder may read stays
    // what it was, and the daemon checks the output against `outputs`
    drv.env_set(
        &name,
        drv.fixed_output_path(&name)?.to_full_string().into_bytes(),
    );
    let aterm = drv.unparse()?;
    let new_root = drv.drv_path_for(&aterm)?;
    let info = DrvInfo {
        aterm: Some(Arc::new(aterm)),
        ..info
    };
    drvs.insert(new_root.clone(), info);
    Ok((new_root, drv.name))
}

/// Builds `root` on the builder and answers the hash its output got. The
/// mismatch is the answer, so it is not reported as an error; any other
/// failure is, with the tail the renderer would have shown.
fn build(
    ev: &Evaluator,
    root: &StorePath,
    name: &str,
    drvs: &FxHashMap<StorePath, DrvInfo>,
) -> Result<String> {
    let cfg = apibuild::config();
    let transport = apibuild::transport(None, &cfg)?;
    let sources = ev.known.origins();
    let plan = apibuild::plan_with(slice::from_ref(root), drvs, &sources)?;
    let opts = worker::Options::default();
    let backend = backend::open(&transport, &opts, &mut |_| {})?;
    // nothing to watch: the build is meant to fail, and this command's only
    // output is the hash it fails on.
    let quiet = Reporter::with_output(Style::detect(), Box::new(io::sink()));
    let mut d = apibuild::Dispatch::with(
        Dispatcher::with_backend(backend, Provenance::snapshot(ev), quiet),
        plan,
    );
    log::want_mismatch(name);
    let outcomes = d.submit(vec![Target::new(name, root.clone(), Vec::new())])?;
    let why = match outcomes.first() {
        Some(o) if o.success => "the fake hash is the hash it produced".to_string(),
        Some(o) => (o.reason.iter().cloned())
            .chain(o.log_tail.clone())
            .collect::<Vec<_>>()
            .join("\n"),
        None => "nothing was built".to_string(),
    };
    // a remote builder's message comes back as the log of the build, which
    // `log::want_mismatch` does not see
    let text = log::strip_ansi(&why);
    let got = (log::take_mismatches().into_iter().next())
        .or_else(|| log::mismatch(&text).filter(|_| text.contains(&format!("-{name}.drv"))));
    match got {
        Some((_, got)) => Ok(Hash::parse_any(&got, None).map_or(got, |h| h.to_sri())),
        None => Err(Error::Store(format!("cannot prefetch '{name}': {why}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::path::IngestionMethod;
    use std::collections::{BTreeMap, BTreeSet};

    const REAL: &str = "sha256-0M0pRasuDjQWaa/XKanIMFJQdq43r1N8bGujzW7BndU=";
    const ZERO: &str = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    /// A one-output derivation, and the drv table an evaluation leaves.
    fn table(out: DerivationOutput) -> (StorePath, FxHashMap<StorePath, DrvInfo>) {
        let mut drv = Derivation {
            name: "msg".into(),
            outputs: BTreeMap::from([("out".to_string(), out)]),
            platform: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            ..Derivation::default()
        };
        if let Ok(p) = drv.fixed_output_path("out") {
            drv.env_set("out", p.to_full_string().into_bytes());
        }
        drv.env_set("outputHash", REAL.into());
        let aterm = drv.unparse().unwrap();
        let root = drv.drv_path_for(&aterm).unwrap();
        let info = DrvInfo {
            outputs: BTreeSet::from(["out".to_string()]),
            references: BTreeSet::new(),
            aterm: Some(Arc::new(aterm)),
            pos: None,
            attr: None,
        };
        (root.clone(), FxHashMap::from_iter([(root, info)]))
    }

    fn fixed() -> DerivationOutput {
        DerivationOutput::Fixed {
            method: IngestionMethod::Flat,
            hash: Hash::parse_any(REAL, None).unwrap(),
        }
    }

    #[test]
    fn the_fake_hash_moves_the_output_and_the_drv() {
        let (root, mut drvs) = table(fixed());
        let (new_root, name) = rewrite(&root, &mut drvs).unwrap();
        assert_eq!(name, "msg");
        assert_ne!(new_root, root);
        let aterm = drvs[&new_root].aterm.clone().unwrap();
        let drv = Derivation::parse(&aterm, "msg").unwrap();
        assert_eq!(drv.drv_path().unwrap(), new_root);
        assert_eq!(drv.env_str("outputHash"), Some(REAL));
        let out = drv.fixed_output_path("out").unwrap().to_full_string();
        assert_eq!(drv.env_str("out"), Some(out.as_str()));
        // the derivation the evaluation made is still there, untouched
        let old = drvs[&root].aterm.clone().unwrap();
        let old = Derivation::parse(&old, "msg").unwrap();
        assert_eq!(old.env_str("outputHash"), Some(REAL));
        let DerivationOutput::Fixed { hash, .. } = drv.outputs["out"] else {
            panic!("fixed")
        };
        assert_eq!(hash.to_sri(), ZERO);
        assert_ne!(old.env_get("out"), drv.env_get("out"));
    }

    #[test]
    fn only_a_fixed_output_drv_can_be_prefetched() {
        let path = StorePath::parse("/nix/store/00000000000000000000000000000000-msg").unwrap();
        let (root, mut drvs) = table(DerivationOutput::InputAddressed(path));
        let e = rewrite(&root, &mut drvs).unwrap_err();
        assert!(
            format!("{e}").contains("not a fixed-output derivation"),
            "{e}"
        );
    }
}
