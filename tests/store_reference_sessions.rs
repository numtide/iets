//! Each embedded command reads references from its configured eval store.
#![cfg(unix)]

mod e2e;
mod hermetic;

use e2e::{Store, ok};
use iets::cli::run::{Host, run};
use iets::store::path::pin_store_dir;

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
}
