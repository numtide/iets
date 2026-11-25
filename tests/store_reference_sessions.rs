//! Each embedded command reads references from its configured eval store.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::sync::Arc;

use e2e::{Store, ok};
use iets::api::value::Eval;
use iets::cli::run::{Host, run};
use iets::env::Env;
use iets::eval::EvalConfig;
use iets::session::Session;
use iets::store::locate::references_of;
use iets::store::path::{StorePath, pin_store_dir};

#[test]
fn read_file_uses_the_current_sessions_store() {
    let Some(store) = Store::start_with(&[]) else {
        return;
    };
    // This integration test has its own process and its own store directory.
    pin_store_dir(store.store_dir().to_str().unwrap());
    let registered = ok(store.nix("nix-instantiate").args([
        "--eval",
        "--read-write-mode",
        "--strict",
        "--json",
        "-E",
        r#"let ref = builtins.toFile "target" "hello";
           in map (name: builtins.toFile name ref) [ "first" "second" ]"#,
    ]));
    let [first, second]: [String; 2] = serde_json::from_slice(&registered.stdout).unwrap();
    let daemon = format!("unix://{}", store.socket().display());
    let read = |path: &str, uri: &str| {
        let host = Host::default().env([("NIX_CONFIG", format!("store = {uri}"))]);
        let expr = format!("builtins.hasContext (builtins.readFile \"{path}\")");
        let result = run(
            &[
                "eval",
                "--impure",
                "--workers",
                "1",
                "--no-cache",
                "-I",
                &format!("input={path}"),
                "-E",
                &expr,
            ],
            &host,
        );
        assert_eq!(result.rc, 0, "{}", result.stderr);
        result.stdout
    };
    assert_eq!(read(&first, &daemon), "true\n");
    assert_eq!(read(&first, "dummy://"), "false\n");
    assert_eq!(read(&second, "dummy://"), "false\n");
    assert_eq!(read(&second, &daemon), "true\n");

    // EvalConfig accepts an explicit session without entering it on the caller.
    let session = |uri: &str| {
        Session::with_env(Env::isolated().with([("NIX_CONFIG", format!("store = {uri}"))]))
    };
    let owner = session(&daemon);
    let ambient = session("dummy://");
    let _ambient = ambient.enter();
    let path = StorePath::parse(&first).unwrap();
    assert!(references_of(&path).is_empty());
    {
        let _owner = owner.enter();
        assert!(!references_of(&path).is_empty());
    }
    let mut ev = EvalConfig::new().session(owner).pure(false).build();
    let mut eval = Eval::new(&mut ev);
    for path in [&first, &second] {
        // The first path is cached in both sessions; the second needs a daemon.
        let expr = format!("builtins.hasContext (builtins.readFile \"{path}\")");
        assert_eq!(eval.eval_expr(&expr, b"/").unwrap().as_bool(), Some(true));
        assert!(Arc::ptr_eq(&Session::current(), &ambient));
    }
}
