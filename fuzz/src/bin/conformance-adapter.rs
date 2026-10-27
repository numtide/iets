//! iets behind the adapter protocol of nix-conformance (its README.md):
//! `conformance-adapter OP FILE [FLAG...]`. `parse`, `nar`, `drv` and `wire`
//! call the library; `check`, `eval` and `flake` run `$IETS`, else `iets` on
//! `PATH`.

use iets::store::drv::Derivation;
use iets::store::nar;
use iets::syntax::{self, ast::FileId, symbol::Interner};
use iets::sys::tmp;
use iets_fuzz::{canon, sx, wire};
use sha2::{Digest, Sha256};
use std::path::{self, Path};
use std::process::{Command, exit};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

/// "This implementation does not do that": the runner skips the case.
const UNSUPPORTED: i32 = 3;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let (op, file, flags) = match args.as_slice() {
        [op, file, flags @ ..] => (op.as_str(), file.as_str(), flags),
        _ => {
            eprintln!("usage: conformance-adapter OP FILE [FLAG...]");
            exit(2);
        }
    };
    exit(match op {
        "parse" => parse(file),
        "nar" => read_nar(file),
        "drv" => read_drv(file),
        // `import` resolves every variable of the file; a refusal is an error
        "check" => iets(&[
            "eval",
            "--impure",
            "-E",
            &format!("builtins.seq (import {}) null", absolute(file)),
        ]),
        "eval" => {
            let mut a = vec!["eval", "--impure", "--strict"];
            a.extend(flags.iter().map(String::as_str));
            a.push(file);
            iets(&a)
        }
        "flake" => {
            let dir = Path::new(file).parent().unwrap_or(Path::new("."));
            let flake = format!("path:{}#result", absolute(&dir.to_string_lossy()));
            iets(&["eval", "--strict", "--json", "--no-cache", &flake])
        }
        "wire" => answer_wire(file, flags),
        _ => UNSUPPORTED,
    })
}

/// `wire SOCKET OP ARG...`: FILE is the socket of nix-conformance's scripted
/// daemon.
fn answer_wire(sock: &str, flags: &[String]) -> i32 {
    let Some((op, args)) = flags.split_first() else {
        return 2;
    };
    match wire::run_iets(Path::new(sock), op, args) {
        Ok(a) if a == "unsupported" => return UNSUPPORTED,
        Ok(a) => println!("{a}"),
        Err(_) => println!("error"),
    }
    0
}

fn parse(file: &str) -> i32 {
    let Ok(src) = fs::read(file) else {
        return 2;
    };
    let mut interner = Interner::new();
    match syntax::parse(&src, FileId(0), &mut interner) {
        Ok(ast) => {
            let tree = sx::normalise(canon::tree(&ast, &interner));
            println!("ok {}", sx::print(&tree));
        }
        Err(e) => {
            let (line, col) = syntax::line_col(&src, e.span.start);
            println!("error {line}:{col}");
        }
    }
    0
}

fn read_nar(file: &str) -> i32 {
    let Ok(input) = fs::read(file) else {
        return 2;
    };
    let Ok(dir) = tmp::private_dir("iets-conformance-") else {
        return 2;
    };
    let out = dir.join("t");
    let answer = (|| {
        let consumed = nar::parse_length(&mut &input[..]).ok()?;
        nar::restore_path(&mut &input[..], &out).ok()?;
        let mut dump = Vec::new();
        nar::dump_path(&out, &mut dump, &mut |_, _| true).ok()?;
        let hash: String = Sha256::digest(&dump)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Some(format!("ok {consumed} {hash}"))
    })();
    let _ = fs::remove_dir_all(&dir);
    println!("{}", answer.unwrap_or_else(|| "error".into()));
    0
}

fn read_drv(file: &str) -> i32 {
    let Ok(input) = fs::read(file) else {
        return 2;
    };
    let answer = (|| {
        let drv = Derivation::parse(&input, &drv_name(file)).ok()?;
        let out = drv.unparse().ok()?;
        let path = drv.drv_path_for(&out).ok()?;
        let hash: String = Sha256::digest(&out)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Some(format!("ok {path} {hash}"))
    })();
    println!("{}", answer.unwrap_or_else(|| "error".into()));
    0
}

/// The file name without `.drv`, and without a store path hash and its
/// `-` when there is one (SPEC.md of nix-conformance).
fn drv_name(file: &str) -> String {
    let base = Path::new(file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = base.strip_suffix(".drv").unwrap_or(&base);
    match name.as_bytes().get(32) {
        Some(b'-') if name.len() > 33 => name[33..].to_string(),
        _ => name.to_string(),
    }
}

fn absolute(file: &str) -> String {
    path::absolute(Path::new(file))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| file.to_string())
}

/// The longest one case may take. A trivial case takes seconds even on a
/// loaded builder; this fails a hung one long before max-silent-time.
const CASE_TIMEOUT: Duration = Duration::from_secs(120);

/// Exit code of a case that ran past `CASE_TIMEOUT`.
const TIMED_OUT: i32 = 124;

/// Runs iets; past `CASE_TIMEOUT` kills it and exits `TIMED_OUT`. Any
/// failing exit passes an `eval-fail` case, so the check (nix/fuzz.nix)
/// fails on this code itself.
fn iets(args: &[&str]) -> i32 {
    let bin = env::var("IETS").unwrap_or_else(|_| "iets".into());
    let mut child = match Command::new(&bin).args(args).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("conformance-adapter: cannot run {bin}: {e}");
            return 2;
        }
    };
    let start = Instant::now();
    let mut pause = Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return s.code().unwrap_or(1),
            Ok(None) if start.elapsed() < CASE_TIMEOUT => {
                thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                println!("timed out after {}s", CASE_TIMEOUT.as_secs());
                return TIMED_OUT;
            }
            Err(e) => {
                eprintln!("conformance-adapter: cannot wait for {bin}: {e}");
                return 2;
            }
        }
    }
}
