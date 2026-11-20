//! Retry a failed upload through the public dispatcher on a private Nix daemon.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::cell::Cell;
use std::collections::HashSet;
use std::{fs, mem, slice};

use iets::api::{build, installable::drv_roots};
use iets::build::backend::{Backend, Worker};
use iets::build::upload::{PlanUpload, Uploader};
use iets::build::{Dispatcher, Target, render::Style};
use iets::error::Error;
use iets::eval::Evaluator;
use iets::provenance::Provenance;
use iets::remote::worker::{Options, Transport};
use iets::store::path::{StorePath, pin_store_dir};

#[test]
fn planned_upload_retries_and_follows_each_batch() {
    let s = store!();
    pin_store_dir(s.store_dir().to_str().unwrap());
    let mut ev = Evaluator::new();
    let file = s.write("roots.nix", r#"let mk = name: src: derivation {
        inherit name src; system = "@SYSTEM@"; builder = "@SHELL@"; args = [ "-c" "echo yes > $out" ];
    }; in [ (mk "a" (builtins.toFile "src" "text")) (mk "b" "") ]"#);
    let value = ev
        .eval_string(&fs::read_to_string(file).unwrap(), b"/")
        .unwrap();
    let mut roots = Vec::new();
    drv_roots(&mut ev, value, &mut roots).unwrap();
    let [(a, _), (b, _)]: [_; 2] = roots.try_into().unwrap();
    let mut plan = build::plan(&ev, &[a.clone(), b.clone()]).unwrap();

    let origins = mem::take(&mut plan.origins);
    let fail = Cell::new(true);
    let locate = |paths: &[StorePath]| {
        if fail.replace(false) {
            return Err(Error::Plan("source temporarily unavailable".into()));
        }
        Ok(paths.iter().map(|p| origins.get(p).cloned()).collect())
    };
    let url = Transport::Unix(s.socket());
    let opts = Options::default();
    let dispatch = || Dispatcher::open(&url, &opts, Provenance::off(), Style::detect()).unwrap();
    let targets = || vec![Target::new("a", a.clone(), vec![])];
    let mut d = dispatch();
    let mut upload = PlanUpload::new(&plan, &locate);
    assert!(d.submit(&mut upload, targets()).is_err());
    assert!(d.submit(&mut upload, targets()).unwrap()[0].success);
    assert!(d.submit(&mut upload, targets()).unwrap().is_empty());

    let mut store = Worker::open(&url, &opts, &mut |_| {}).unwrap();
    let mut send = |up: &mut PlanUpload<'_>, path: &StorePath| {
        up.upload(&mut store, slice::from_ref(path), &mut |_| {})
    };
    assert_eq!(send(&mut upload, &b).unwrap().added, 1);
    // A dispatcher that finds the build already held leaves no pending answer.
    assert!(dispatch().submit(&mut upload, targets()).unwrap()[0].success);
    assert_eq!(send(&mut upload, &a).unwrap().queried, 1);
    let valid = HashSet::from([a.base_name().to_owned()]);
    upload.roots_answered(slice::from_ref(&a), &valid);
    let batch = send(&mut upload, &b).unwrap();
    assert_eq!((batch.queried, batch.added), (1, 0));
    upload.roots_answered(slice::from_ref(&a), &valid);
    assert_eq!(send(&mut upload, &a).unwrap().queried, 0);
    assert_eq!(send(&mut upload, &a).unwrap().queried, 1);
    assert_eq!(store.query_valid(&[&a, &b], &mut |_| {}).unwrap().len(), 2);
}
