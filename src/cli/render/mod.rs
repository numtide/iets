//! What a command puts on a terminal: the stdout writer, the JSON dump,
//! sizes, and the text layout of the values `crate::api::render` produces.
//! Presentation is porcelain-side and may be shared freely; the values
//! themselves, and the JSON that is their contract, are not (docs/explanation/plumbing-and-porcelain.md).

pub mod text;

use serde_json::Value;

use crate::api::render::Report;
use std::io::Write;

/// Everything a command writes on stdout goes through here. Nothing is
/// added: `b` is the output. A write that fails ends the process: exit 0 is
/// the promise that the output reached stdout, and `logger->writeToStdout`
/// throws for the same reason.
pub fn bytes(b: &[u8]) {
    if let Err(e) = write(b) {
        std::process::exit(
            crate::error::Error::io(format!("write of {} bytes", b.len()), e).report(),
        )
    }
}

/// [`bytes`] for the legacy command names, which write through `std::cout`
/// and never look: `nix-instantiate` and `nix-build` exit 0 on a full disk.
pub fn bytes_lossy(b: &[u8]) {
    let _ = write(b);
}

fn write(b: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    out.write_all(b)?;
    out.flush()
}

/// `nlohmann::json::dump`: an object's keys are sorted, `--pretty` indents
/// by two spaces, and the value ends in a newline, all of which
/// `serde_json` does the same way.
pub fn json(v: &Value, pretty: bool) -> Vec<u8> {
    let mut out = if pretty {
        serde_json::to_vec_pretty(v)
    } else {
        serde_json::to_vec(v)
    }
    .unwrap_or_default();
    out.push(b'\n');
    out
}

/// `--pretty`'s default: a terminal on stdout.
pub fn pretty() -> bool {
    unsafe { libc::isatty(1) == 1 }
}

/// util.cc:renderSize: KiB at least, one decimal.
pub fn size(b: u64) -> String {
    let (mut unit, mut v) = (0, b);
    while v > 1024 && unit < 8 {
        unit += 1;
        v /= 1024;
    }
    let scaled = b as f64 / 1024f64.powi(unit.max(1));
    format!("{scaled:.1} {}iB", b"KKMGTPEZY"[unit as usize] as char)
}

/// A status line on stderr: what a run did, not what it produced.
pub fn status(text: &str) {
    crate::log::line(text);
}

/// The text layout of a value the operation layer produced. The JSON of the
/// same value is [`Report`], and it is below: one of them is a contract and
/// the other is a terminal.
pub trait Text {
    fn text(&self) -> String;
}

/// `--json` or not: the contract shape, or the layout, on stdout.
pub fn emit<T: Report + Text>(v: &T, as_json: bool) {
    match as_json {
        true => bytes(&json(&v.json(), false)),
        false => bytes(v.text().as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_render_as_nix_renders_them() {
        let got: Vec<String> = [100, 2000, 10547, 1_572_864, 6_000_000_000]
            .map(super::size)
            .to_vec();
        assert_eq!(
            got,
            ["0.1 KiB", "2.0 KiB", "10.3 KiB", "1.5 MiB", "5.6 GiB"]
        );
    }
}
