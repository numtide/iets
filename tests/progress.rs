//! The `⏵` line a pipe gets for a task of 5 s or more. The hermetic
//! environment turns progress off, so this test turns it back on.

mod hermetic;

use hermetic::{Iets, bin};
use rstest::rstest;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[rstest]
fn a_pipe_hears_of_a_run_that_takes_five_seconds(bin: Iets) {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/x", server.local_addr().unwrap());
    let (heard, wait) = mpsc::channel::<()>();
    // the download waits for the line, so the run lasts as long as it must
    thread::spawn(move || {
        let _ = wait.recv_timeout(Duration::from_secs(120));
        for s in server.incoming() {
            let Ok(mut s) = s else { return };
            let _ = s.read(&mut [0; 4096]);
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let zero = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let mut child = bin
        .cmd()
        .env("IETS_PROGRESS", "true")
        .env("IETS_CACHE_DIR", cache.path())
        .env("IETS_CACHE", "0")
        .args(["eval", "--raw", "--expr"])
        .arg(format!(
            r#"builtins.fetchurl {{ url = "{url}"; sha256 = "{zero}"; }}"#
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run iets");
    let mut tick = None;
    let mut err = Vec::new();
    for l in BufReader::new(child.stderr.take().unwrap()).lines() {
        let l = l.unwrap();
        if tick.is_none() && l.starts_with("⏵ ") {
            tick = Some(l.clone());
            let _ = heard.send(());
        }
        err.push(l);
    }
    let _ = child.wait();
    let err = err.join("\n");
    let tick = tick.unwrap_or_else(|| panic!("no `⏵` line: {err}"));
    assert!(tick.contains("evaluating the expression"), "{err}");
}
