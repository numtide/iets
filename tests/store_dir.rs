//! `NIX_STORE_DIR` and `NIX_STATE_DIR` move the store and the daemon socket,
//! as in Nix.

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;

#[rstest]
fn store_dir_follows_nix_store_dir(bin: Iets) {
    let expr = r#"[ builtins.storeDir (derivation { name = "x"; system = "x"; builder = "/b"; }).drvPath ]"#;
    let o = bin
        .cmd()
        .env("NIX_STORE_DIR", "/tmp/other/store")
        .args(["eval", "--json", "--strict", "--expr", expr])
        .output()
        .expect("run iets");
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: Vec<String> = serde_json::from_str(&out).expect("json list");
    assert_eq!(v[0], "/tmp/other/store");
    assert!(v[1].starts_with("/tmp/other/store/"), "{out}");
}

// Windows has no daemon socket
#[cfg(unix)]
#[rstest]
fn daemon_socket_follows_nix_state_dir(bin: Iets) {
    let drv = r#"derivation { name = "x"; system = "x"; builder = "/b"; }"#;
    for args in [
        &["build", "--expr", drv][..],
        &["nix-instantiate", "--expr", drv],
    ] {
        let o = bin
            .cmd()
            .env("NIX_STATE_DIR", "/nonexistent/state")
            .args(args)
            .output()
            .expect("run iets");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(!o.status.success(), "{args:?}");
        assert!(
            err.contains("/nonexistent/state/daemon-socket/socket"),
            "{args:?}: {err}"
        );
    }
}
