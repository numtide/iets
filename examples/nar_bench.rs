//! Timing harness for the NAR code (docs/reference/builders.md).
//!
//! `cargo run --release --example nar_bench -- <op> <path> [runs]`
//!
//! Ops: `hash` (NAR hash of a path), `flat` (flat hash of one file),
//! `dump` (NAR to /dev/null), `parse` (walk a NAR's framing, contents thrown
//! away: the parser on its own), `restore` (read a NAR from stdin into a fresh
//! directory under the path given). Prints one `op path runs best_ms mean_ms
//! MB/s` line so `bench/nar.nu` can parse it.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;
use std::{env, process};

use iets::store::hash::HashAlgo;
use iets::store::nar;

fn die(msg: &str) -> ! {
    eprintln!("nar_bench: {msg}");
    process::exit(2);
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 {
        die("usage: nar_bench <hash|flat|dump|parse|restore> <path> [runs]");
    }
    let op = args[0].as_str();
    let path = Path::new(&args[1]);
    let runs: u32 = args
        .get(2)
        .map(|s| s.parse().unwrap_or_else(|_| die("bad run count")))
        .unwrap_or(3);

    let mut times = Vec::new();
    let mut bytes = 0u64;
    for i in 0..runs {
        let t = Instant::now();
        bytes = match op {
            "hash" => {
                let (h, size) = nar::nar_hash_filtered(path, HashAlgo::Sha256, &mut |_, _| true)
                    .unwrap_or_else(|e| die(&format!("{e}")));
                if i == 0 {
                    println!("# {} {}", h.to_sri(), size);
                }
                size
            }
            "flat" => {
                let h =
                    nar::flat_hash(path, HashAlgo::Sha256).unwrap_or_else(|e| die(&format!("{e}")));
                if i == 0 {
                    println!("# {}", h.to_sri());
                }
                fs::metadata(path).map(|m| m.len()).unwrap_or(0)
            }
            "dump" => {
                let mut sink = Counter(0);
                nar::dump_path(path, &mut sink, &mut |_, _| true)
                    .unwrap_or_else(|e| die(&format!("{e}")));
                sink.0
            }
            "parse" => {
                let mut f = File::open(path).unwrap_or_else(|e| die(&format!("{e}")));
                let n = nar::parse_length(&mut f).unwrap_or_else(|e| die(&format!("{e}")));
                if i == 0 {
                    println!("# parsed {n} bytes");
                }
                n
            }
            "restore" => {
                if runs != 1 {
                    die("restore takes one run");
                }
                let dest = path.join("restored");
                let _ = fs::remove_dir_all(&dest);
                let _ = fs::remove_file(&dest);
                let mut stdin = io::stdin().lock();
                nar::restore_path(&mut stdin, &dest).unwrap_or_else(|e| die(&format!("{e}")));
                0
            }
            _ => die("unknown op"),
        };
        times.push(t.elapsed().as_secs_f64());
    }
    let best = times.iter().copied().fold(f64::INFINITY, f64::min);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let mbs = if best > 0.0 {
        bytes as f64 / best / 1e6
    } else {
        0.0
    };
    println!(
        "{op} {} {runs} {:.1} {:.1} {mbs:.0}",
        path.display(),
        best * 1e3,
        mean * 1e3
    );
    io::stdout().flush().ok();
}

struct Counter(u64);

impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
