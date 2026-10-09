//! `iets lock` and `nix/lock.nix` together: each error names a command, and
//! running that exact command is the fix.

use std::path::Path;
use std::process::{Command, Output};
use std::{env, fs};

fn iets(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_iets"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn named_command(out: &Output) -> Vec<String> {
    let err = String::from_utf8_lossy(&out.stderr);
    let line = (err.lines().map(str::trim))
        .find(|l| l.starts_with("iets lock "))
        .unwrap_or_else(|| panic!("no command in: {err}"));
    line.split(' ').skip(1).map(String::from).collect()
}

fn run(dir: &Path, args: &[String]) -> Output {
    iets(dir, &args.iter().map(String::as_str).collect::<Vec<_>>())
}

#[test]
fn a_lock_entry_is_added_on_request_and_replaced_only_on_request() {
    let d = env::temp_dir().join(format!("iets-lock-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let (src, lock) = (d.join("src.txt"), d.join("iets.lock"));
    fs::write(&src, "one\n").unwrap();
    let url = format!("file://{}", src.display());
    let helper = concat!(env!("CARGO_MANIFEST_DIR"), "/nix/lock.nix");
    fs::copy(helper, d.join("lock.nix")).unwrap();
    let expr = format!(r#"(import ./lock.nix ./iets.lock).hash {{ url = "{url}"; }}"#);
    fs::write(d.join("default.nix"), expr).unwrap();

    let miss = iets(&d, &["eval", "default.nix"]);
    assert!(!miss.status.success());
    assert!(!lock.exists(), "evaluation wrote the lock");
    assert!(run(&d, &named_command(&miss)).status.success());
    let hit = iets(&d, &["eval", "default.nix"]);
    let want = iets(&d, &["prefetch", &url]);
    assert_eq!(
        String::from_utf8_lossy(&hit.stdout).trim(),
        format!("\"{}\"", String::from_utf8_lossy(&want.stdout).trim())
    );

    fs::write(&src, "two\n").unwrap();
    let before = fs::read(&lock).unwrap();
    let changed = iets(&d, &["lock", &url]);
    assert_eq!(changed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&changed.stderr).contains("the remote changed"));
    assert_eq!(fs::read(&lock).unwrap(), before);
    assert!(run(&d, &named_command(&changed)).status.success());
    assert_ne!(fs::read(&lock).unwrap(), before);
    fs::remove_dir_all(&d).unwrap();
}
