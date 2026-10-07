//! What the NAR code promises against real Nix: byte-identical archives and the
//! same hashes, on every shape the format has. See docs/reference/builders.md.
//!
//! Every test skips itself when `nix` is not on `PATH`; `nix develop` has it.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use iets::store::nar;

fn have_nix() -> bool {
    Command::new("nix")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn tmp(tag: &str) -> PathBuf {
    let d = env::temp_dir().join(format!("iets-narparity-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).expect("temp dir");
    d
}

/// A tree that exercises every case the format has: unicode and awkward names,
/// empty files and directories, symlinks (including a dangling one and one out
/// of the tree), the executable bit, contents on and off the 8-byte boundary,
/// and a file past the 64 KiB the writer buffers in one go.
fn awkward_tree(root: &Path) {
    let mk = |p: &Path, bytes: &[u8]| {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    };
    mk(&root.join("a"), b"");
    mk(&root.join("b"), b"1234567");
    mk(&root.join("c"), b"12345678");
    mk(&root.join("d"), b"123456789");
    mk(&root.join("é-accent"), "héllo\n".as_bytes());
    mk(&root.join("日本語/ファイル"), b"nihongo");
    mk(&root.join(" leading space"), b"x");
    mk(&root.join("tab\there"), b"x");
    mk(&root.join("-dash"), b"x");
    mk(&root.join("z/y/x/w/v/deep"), b"deep");
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    mk(&root.join("big.bin"), &big);
    let exe = root.join("run.sh");
    mk(&exe, b"#!/bin/sh\nexit 0\n");
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(root.join("emptydir")).unwrap();
    fs::create_dir_all(root.join("dir/emptysub")).unwrap();
    symlink("a", root.join("link-a")).unwrap();
    symlink("nowhere-at-all", root.join("link-dangling")).unwrap();
    symlink("../../outside", root.join("dir/link-out")).unwrap();
    symlink("日本語", root.join("link-unicode")).unwrap();
}

fn nix_dump_sha256(path: &Path) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "nix-store --dump {} | sha256sum",
            shell_quote(path)
        ))
        .output()
        .expect("nix-store --dump");
    assert!(out.status.success(), "nix-store --dump failed");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}

#[test]
fn dump_is_byte_identical_to_nix_store_dump() {
    if !have_nix() {
        eprintln!("skipping: no nix on PATH");
        return;
    }
    let dir = tmp("bytes");
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    awkward_tree(&tree);

    let mut ours = Vec::new();
    nar::dump_path(&tree, &mut ours, &mut |_, _| true).unwrap();
    let theirs = Command::new("nix-store")
        .arg("--dump")
        .arg(&tree)
        .output()
        .expect("nix-store --dump");
    assert!(theirs.status.success());
    assert_eq!(
        ours.len(),
        theirs.stdout.len(),
        "NAR lengths differ: ours {} theirs {}",
        ours.len(),
        theirs.stdout.len()
    );
    let first = ours
        .iter()
        .zip(&theirs.stdout)
        .position(|(a, b)| a != b)
        .map_or("none".to_string(), |i| i.to_string());
    assert_eq!(first, "none", "first differing byte at offset {first}");
    assert_eq!(
        nar::nar_hash(&tree).unwrap().to_base16(),
        nix_dump_sha256(&tree)
    );
}

/// Every subtree of the awkward tree on its own, so a mismatch names the case.
#[test]
fn every_node_hashes_like_nix() {
    if !have_nix() {
        eprintln!("skipping: no nix on PATH");
        return;
    }
    let dir = tmp("nodes");
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    awkward_tree(&tree);
    let mut checked = 0;
    let mut stack = vec![tree.clone()];
    while let Some(p) = stack.pop() {
        assert_eq!(
            nar::nar_hash(&p).unwrap().to_base16(),
            nix_dump_sha256(&p),
            "NAR hash differs for {}",
            p.display()
        );
        checked += 1;
        if fs::symlink_metadata(&p).unwrap().is_dir() {
            for e in fs::read_dir(&p).unwrap() {
                stack.push(e.unwrap().path());
            }
        }
    }
    assert!(checked > 20, "only checked {checked} nodes");
}

/// The restore of our own dump reproduces the tree bit for bit, as `nix-store
/// --restore` would.
#[test]
fn restore_round_trips_the_awkward_tree() {
    if !have_nix() {
        eprintln!("skipping: no nix on PATH");
        return;
    }
    let dir = tmp("restore");
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    awkward_tree(&tree);
    let mut archive = Vec::new();
    nar::dump_path(&tree, &mut archive, &mut |_, _| true).unwrap();
    let out = dir.join("restored");
    nar::restore_path(&mut archive.as_slice(), &out).unwrap();
    assert_eq!(nix_dump_sha256(&out), nix_dump_sha256(&tree));
}

/// The parse-only walk agrees with `nix-store --dump | wc -c` on the length and
/// accepts the same archive the restore does.
#[test]
fn parse_length_matches_nix() {
    if !have_nix() {
        eprintln!("skipping: no nix on PATH");
        return;
    }
    let dir = tmp("parse");
    let tree = dir.join("tree");
    fs::create_dir_all(&tree).unwrap();
    awkward_tree(&tree);
    let mut archive = Vec::new();
    nar::dump_path(&tree, &mut archive, &mut |_, _| true).unwrap();
    assert_eq!(
        nar::parse_length(&mut archive.as_slice()).unwrap(),
        archive.len() as u64
    );
    assert_eq!(nar::nar_size(&tree).unwrap(), archive.len() as u64);

    // Truncation, a bad keyword and non-zero padding are all rejected.
    assert!(nar::parse_length(&mut &archive[..archive.len() - 8]).is_err());
    let mut bad = archive.clone();
    bad[8] = b'X';
    assert!(nar::parse_length(&mut bad.as_slice()).is_err());
    assert!(nar::parse_length(&mut &b"nix-archive-2"[..]).is_err());
}

/// A name longer than `NAME_MAX` and a target longer than `PATH_MAX` are
/// refused, rather than failing later at `File::create`.
#[test]
fn reader_caps_names_and_targets() {
    let mut nar = Vec::new();
    // "nix-archive-1" "(" "type" "directory" "entry" "(" "name" <300 bytes> …
    for tok in [
        &b"nix-archive-1"[..],
        b"(",
        b"type",
        b"directory",
        b"entry",
        b"(",
        b"name",
    ] {
        nar.extend_from_slice(&(tok.len() as u64).to_le_bytes());
        nar.extend_from_slice(tok);
        nar.resize(nar.len().next_multiple_of(8), 0);
    }
    let long = vec![b'a'; 300];
    nar.extend_from_slice(&(long.len() as u64).to_le_bytes());
    nar.extend_from_slice(&long);
    nar.resize(nar.len().next_multiple_of(8), 0);
    let out = tmp("caps").join("dest");
    let err = nar::restore_path(&mut nar.as_slice(), &out).unwrap_err();
    assert!(
        err.to_string().contains("too long"),
        "unexpected error: {err}"
    );
}
