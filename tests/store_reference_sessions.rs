//! Each embedded command reads references from its configured eval store.
#![cfg(unix)]

mod e2e;
mod hermetic;

use std::io::ErrorKind;
use std::os::{fd::OwnedFd, unix::net::UnixListener};
use std::sync::Arc;
use std::{
    fs, thread,
    time::{Duration, Instant},
};

use e2e::{Store, ok};
use iets::api::value::Eval;
use iets::api::{EvalArgs, Input};
use iets::cli::eval::run_eval;
use iets::cli::run::{Host, Sink, run};
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
    eval_hit_prefetches_for_the_next_use(&store, &first);
}

fn eval_hit_prefetches_for_the_next_use(store: &Store, subject: &str) {
    let socket = store.dir().join("prefetch-socket");
    let endpoint = format!("unix://{}", socket.display());
    let session = || {
        let session = Session::with_env(Env::isolated());
        let mut config = iets::config::Config::empty();
        config.set("store", &endpoint);
        session.nix_conf.set(config).unwrap();
        session
    };
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let warm = |owner: &Arc<Session>, expected_hit| {
        let mut args = EvalArgs::new();
        args.input = Some(Input::Expr("1".into()));
        args.includes
            .push((b"unused".to_vec(), subject.as_bytes().to_vec()));
        args.pure = Some(false);
        args.workers = 1;
        args.timings = true;
        args.cache_dir = Some(store.dir().join("eval-cache").display().to_string());
        let log = Sink::default();
        owner.log.write_to(Box::new(log.clone()));
        let _entered = owner.enter();
        assert_eq!(run_eval(args), 0);
        assert_eq!(
            log.text().contains("cache: hit"),
            expected_hit,
            "{}",
            log.text()
        );

        // Wait for the actual background lookup. Serve it once, then let the
        // later read use the session after that registry can no longer answer.
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("expected a search-path prefetch: {e}"),
            }
        };
        let output = store
            .nix("nix-daemon")
            .args(["--stdio", "--store", "local"])
            .stdin(OwnedFd::from(stream.try_clone().unwrap()))
            .stdout(OwnedFd::from(stream))
            .status()
            .unwrap();
        assert!(output.success());
    };
    let acquired = session();
    let hit = session();
    warm(&acquired, false);
    warm(&hit, true);
    drop(listener);
    fs::remove_file(socket).unwrap();
    let has_context = |live: Arc<Session>| {
        let mut ev = EvalConfig::new().session(live).pure(false).build();
        let expr = format!("builtins.hasContext (builtins.readFile \"{subject}\")");
        Eval::new(&mut ev).eval_expr(&expr, b"/").unwrap().as_bool()
    };
    assert_eq!(has_context(acquired), Some(true));
    assert_eq!(has_context(hit), Some(true));
    assert_eq!(has_context(session()), Some(false));
}
