//! The `fix-hashes` setting (docs/reference/settings.md): a fixed-output build that
//! failed on its hash gets the hash it produced written over the one it
//! specified, in the one local file evaluation read that holds it.

use std::path::PathBuf;

use crate::eval::Evaluator;
use crate::store::hash::{Hash, HashFormat};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;

/// One rewrite: `old` becomes `new` at byte `at` of `file`, which is
/// `line:column`.
#[derive(Debug, PartialEq)]
pub struct Fix {
    pub file: PathBuf,
    pub at: usize,
    pub line: usize,
    pub column: usize,
    pub old: String,
    pub new: String,
}

impl Fix {
    pub fn pos(&self) -> String {
        format!("{}:{}:{}", self.file.display(), self.line, self.column)
    }

    /// A temporary file beside the original, renamed over it: a crash or a
    /// full disk must never leave a source file of the user cut in half.
    pub fn write(&self) -> Result<(), String> {
        let err = |e: std::io::Error| format!("{}: {e}", self.file.display());
        let mut text = std::fs::read(&self.file).map_err(err)?;
        if text.get(self.at..self.at + self.old.len()) != Some(self.old.as_bytes()) {
            return Err(format!("{} changed since it was read", self.file.display()));
        }
        text.splice(self.at..self.at + self.old.len(), self.new.bytes());
        let mode = std::fs::metadata(&self.file)
            .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()))
            .map_err(err)?;
        let dir = self.file.parent().unwrap_or(std::path::Path::new("."));
        let name = self.file.file_name().unwrap_or_default().to_string_lossy();
        let tmp = dir.join(format!(".{}.{}", name, std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&text)?;
            f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
            f.sync_all()
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return Err(err(e));
        }
        std::fs::rename(&tmp, &self.file).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            err(e)
        })
    }
}

pub enum Answer {
    Yes,
    No,
    All,
}

/// A person can answer: stdin and stderr are terminals, and stderr is not
/// a program's `internal-json` stream.
pub fn interactive() -> bool {
    let tty = unsafe { libc::isatty(0) == 1 && libc::isatty(2) == 1 };
    tty && !crate::log::json()
}

/// Asks on stderr whether to make `fix`. End of input is no.
pub fn prompt(fix: &Fix) -> Answer {
    crate::session::Session::current().log.err(
        format!(
            "fix the hash at {}: '{}' to '{}'? [y/n/a] ",
            fix.pos(),
            fix.old,
            fix.new
        )
        .as_bytes(),
    );
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    match line.trim() {
        "y" | "yes" => Answer::Yes,
        "a" | "all" => Answer::All,
        _ => Answer::No,
    }
}

/// The files `ev` parsed, each copied store path mapped back to its local
/// file; the rest of the store is left out, so nixpkgs is never edited.
pub fn local_files(ev: &Evaluator) -> Vec<PathBuf> {
    let map = crate::provenance::SourceMap::from_eval(ev);
    let mut out: Vec<PathBuf> = (0..)
        .map_while(|i| ev.files.get(i))
        .map(|f| map.locate(&f.path).unwrap_or_else(|| f.path.clone()))
        .filter(|p| p.starts_with(b"/") && !p.starts_with(crate::store::path::STORE_DIR.as_bytes()))
        .map(|p| PathBuf::from(std::ffi::OsStr::from_bytes(&p)))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every way a user may have written `h` in a Nix file, each with `got` in
/// the same spelling.
fn spellings(h: &Hash, got: &Hash) -> Vec<(String, String)> {
    [
        HashFormat::Sri,
        HashFormat::Nix32,
        HashFormat::Base16,
        HashFormat::Base64,
    ]
    .into_iter()
    .map(|f| {
        let sri = f == HashFormat::Sri;
        (h.to_string_with(f, sri), got.to_string_with(f, sri))
    })
    .collect()
}

fn hash_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-')
}

/// Where `needle` stands alone in `text`, not inside a longer hash.
fn find_all(text: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| from + i)
    {
        let end = i + needle.len();
        let alone =
            (i == 0 || !hash_char(text[i - 1])) && text.get(end).is_none_or(|b| !hash_char(*b));
        if alone {
            out.push(i);
        }
        from = i + 1;
    }
    out
}

/// The fix that writes `got` over `specified`, when exactly one place in
/// `files` holds `specified`; else why not.
pub fn locate(files: &[PathBuf], specified: &str, got: &str) -> Result<Fix, String> {
    let (Ok(want), Ok(new)) = (Hash::parse_any(specified, None), Hash::parse_any(got, None)) else {
        return Err(format!("cannot parse '{specified}' or '{got}' as a hash"));
    };
    let forms = spellings(&want, &new);
    let mut hits = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read(f) else { continue };
        for (old, new) in &forms {
            for at in find_all(&text, old.as_bytes()) {
                hits.push((f, at, old, new));
            }
        }
    }
    let [(file, at, old, new)] = hits.as_slice() else {
        let n = hits.len();
        return Err(format!(
            "'{specified}' is in {n} places in the files evaluation read, not one; set it to '{got}' by hand"
        ));
    };
    let text = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let before = &text[..*at];
    Ok(Fix {
        file: file.to_path_buf(),
        at: *at,
        line: before.iter().filter(|b| **b == b'\n').count() + 1,
        column: at
            - before
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |n| n + 1)
            + 1,
        old: old.to_string(),
        new: new.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE: &str = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const GOT: &str = "sha256-GzabkpW8L2UH4rzBkFdv61RoJl8DQlICqZvY9Ql1MAU=";

    fn tmp(name: &str, text: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("iets-fixhash-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("default.nix");
        std::fs::write(&f, text).unwrap();
        f
    }

    #[test]
    fn one_match_is_rewritten_in_its_own_spelling() {
        let f = tmp("one", &format!("{{\n  hash = \"{FAKE}\";\n}}\n"));
        let fix = locate(std::slice::from_ref(&f), FAKE, GOT).unwrap();
        fix.write().unwrap();
        assert_eq!((fix.line, fix.new.as_str()), (2, GOT));
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            format!("{{\n  hash = \"{GOT}\";\n}}\n")
        );

        let want = Hash::parse_any(FAKE, None).unwrap();
        let nix32 = want.to_string_with(HashFormat::Nix32, false);
        let f = tmp("nix32", &format!("sha256 = \"{nix32}\";"));
        let fix = locate(std::slice::from_ref(&f), FAKE, GOT).unwrap();
        fix.write().unwrap();
        let got32 = Hash::parse_any(GOT, None)
            .unwrap()
            .to_string_with(HashFormat::Nix32, false);
        assert_eq!(fix.new, got32);
    }

    #[test]
    fn two_matches_are_left_alone() {
        let f = tmp("two", &format!("a = \"{FAKE}\"; b = \"{FAKE}\";"));
        let e = locate(std::slice::from_ref(&f), FAKE, GOT).unwrap_err();
        assert!(e.contains("in 2 places"), "{e}");
    }

    #[test]
    fn a_longer_hash_is_not_a_match() {
        assert_eq!(find_all(b"x AAAA x", b"AAA"), Vec::<usize>::new());
        assert_eq!(find_all(b"\"AAA\"", b"AAA"), vec![1]);
    }
}
