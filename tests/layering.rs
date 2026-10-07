//! The plumbing/porcelain boundary, checked (docs/explanation/plumbing-and-porcelain.md).
//!
//! `src/cli` is porcelain. A capability is implemented once, below the command
//! line, and a command reaches it through `crate::api`. What this test bans is
//! a *second implementation* of something the operation layer owns. It does not
//! ban a large porcelain-side library: presentation, Nix argv compatibility,
//! interactive flows and UI caches are porcelain code, they may be big, and
//! they pass as long as they do not name a layer below `crate::api`.
//!
//! The two lists below are today's gaps, and each one's length is asserted
//! against a constant. An entry that goes cannot come back unnoticed, and a
//! new entry needs the constant raised, which is a line in a diff to argue
//! about.

use std::fs;
use std::path::{Path, PathBuf};

/// The layers a porcelain command may not name. `crate::api` is the way in.
const LAYERS: &[&str] = &[
    "eval", "store", "build", "remote", "flake", "jobs", "devenv", "trace", "compile", "parallel",
];

/// Files under `src/cli` that still name a layer below `crate::api`, with the
/// layers each one names. This list may only shrink.
const REACHES: &[(&str, &[&str])] = &[
    (
        "src/cli/build.rs",
        &["build", "eval", "remote", "store", "trace"],
    ),
    ("src/cli/cache_dump.rs", &["trace"]),
    (
        "src/cli/classic/build.rs",
        &["build", "eval", "remote", "store"],
    ),
    ("src/cli/classic/develop.rs", &["devenv"]),
    ("src/cli/classic/fmt.rs", &["eval"]),
    (
        "src/cli/classic/instantiate.rs",
        &["build", "eval", "remote"],
    ),
    ("src/cli/classic/mod.rs", &["eval", "remote"]),
    (
        "src/cli/classic/nix.rs",
        &["build", "eval", "remote", "store"],
    ),
    (
        "src/cli/classic/shell.rs",
        &["build", "compile", "devenv", "eval", "remote", "store"],
    ),
    (
        "src/cli/devenv.rs",
        &["build", "devenv", "eval", "remote", "store", "trace"],
    ),
    ("src/cli/diff.rs", &["remote", "store"]),
    ("src/cli/eval.rs", &["eval", "trace"]),
    ("src/cli/jobs.rs", &["eval", "jobs", "remote"]),
    ("src/cli/mod.rs", &["eval", "parallel", "trace"]),
    (
        "src/cli/plumbing.rs",
        &["build", "eval", "jobs", "remote", "store"],
    ),
    ("src/cli/render/text.rs", &["store"]),
    ("src/cli/search.rs", &["eval", "parallel", "trace"]),
];

/// Locked, so `REACHES` can only shrink. Lower it when an entry goes.
const REACHES_LEN: usize = 17;

/// Files under `src/cli` with `runtime::Word` — an evaluator value — in a
/// function signature. A command that cannot name a value cannot evaluate one.
/// This list may only shrink.
const WORD_IN_SIGNATURES: &[&str] = &[
    "src/cli/classic/instantiate.rs",
    "src/cli/classic/mod.rs",
    "src/cli/classic/nix.rs",
    "src/cli/classic/shell.rs",
    "src/cli/jobs.rs",
    "src/cli/mod.rs",
    "src/cli/search.rs",
];

/// Locked, so `WORD_IN_SIGNATURES` can only shrink.
const WORD_LEN: usize = 7;

const RULE: &str = "\
docs/explanation/plumbing-and-porcelain.md: a capability is implemented once, below the command line.
Either the operation layer is missing something — add it to `crate::api` and
call that from here — or this is presentation, compatibility or interaction,
in which case it belongs in a porcelain-side library that names no layer.";

#[test]
fn porcelain_names_no_layer_below_the_api() {
    let mut found: Vec<(String, Vec<String>)> = Vec::new();
    for file in cli_files() {
        let rel = rel(&file);
        let text = strip_comments(&fs::read_to_string(&file).unwrap());
        let mut named: Vec<String> = LAYERS
            .iter()
            .filter(|l| names_layer(&text, l))
            .map(|l| (*l).to_string())
            .collect();
        named.sort();
        if !named.is_empty() {
            found.push((rel, named));
        }
    }

    for (file, named) in &found {
        let allowed = REACHES.iter().find(|(f, _)| f == file).map(|(_, l)| *l);
        let Some(allowed) = allowed else {
            panic!(
                "{file} names crate::{} , which is below crate::api.\n\n{RULE}\n\n\
                 If the reach cannot go yet, add\n    (\"{file}\", &{named:?}),\n\
                 to REACHES in tests/layering.rs and raise REACHES_LEN to {}.\n\
                 The list may only grow with a reason in the commit message.",
                named.join(", crate::"),
                REACHES_LEN + 1
            );
        };
        for l in named {
            assert!(
                allowed.contains(&l.as_str()),
                "{file} names crate::{l}, which its allow-list entry does not \
                 cover.\n\n{RULE}\n\nAdd \"{l}\" to its entry in REACHES only if \
                 there is no way through crate::api."
            );
        }
    }

    // the entries that no longer need to be there
    let mut stale: Vec<String> = Vec::new();
    for (file, allowed) in REACHES {
        let named = found.iter().find(|(f, _)| f == file).map(|(_, n)| n);
        match named {
            None => stale.push(format!("{file}: the whole entry")),
            Some(named) => {
                for l in *allowed {
                    if !named.iter().any(|n| n == l) {
                        stale.push(format!("{file}: {l}"));
                    }
                }
            }
        }
    }
    assert!(
        stale.is_empty(),
        "tests/layering.rs: the allow-list is wider than the code needs:\n  {}\n\n\
         Close the gap in the list: delete what is listed above, and lower \
         REACHES_LEN if a whole entry went. Keeping it wide lets the reach come \
         back without anyone noticing, which is what the constant is for.",
        stale.join("\n  ")
    );

    assert_eq!(
        REACHES.len(),
        REACHES_LEN,
        "REACHES has {} entries against a constant of {REACHES_LEN}. The list may \
         only shrink: lower the constant with the entry you removed, and raise it \
         only with a reason.",
        REACHES.len()
    );
}

#[test]
fn porcelain_signatures_name_no_evaluator_value() {
    let mut found: Vec<String> = Vec::new();
    for file in cli_files() {
        let text = strip_comments(&fs::read_to_string(&file).unwrap());
        if signatures(&text).iter().any(|s| has_word(s, "Word")) {
            found.push(rel(&file));
        }
    }

    for file in &found {
        assert!(
            WORD_IN_SIGNATURES.contains(&file.as_str()),
            "{file} has runtime::Word in a function signature.\n\n\
             A porcelain command that cannot name an evaluator value cannot \
             evaluate one: take the value through crate::api and let the \
             operation layer hold it.\n\n\
             If it cannot go yet, add \"{file}\" to WORD_IN_SIGNATURES in \
             tests/layering.rs and raise WORD_LEN to {}.",
            WORD_LEN + 1
        );
    }

    let stale: Vec<&&str> = WORD_IN_SIGNATURES
        .iter()
        .filter(|f| !found.iter().any(|g| g == *f))
        .collect();
    assert!(
        stale.is_empty(),
        "tests/layering.rs: WORD_IN_SIGNATURES lists files that no longer have \
         Word in a signature:\n  {stale:?}\n\nDelete them and lower WORD_LEN to \
         {}.",
        WORD_LEN - stale.len()
    );

    assert_eq!(
        WORD_IN_SIGNATURES.len(),
        WORD_LEN,
        "WORD_IN_SIGNATURES has {} entries against a constant of {WORD_LEN}. The \
         list may only shrink.",
        WORD_IN_SIGNATURES.len()
    );
}

fn cli_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(&root().join("src/cli"), &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rel(p: &Path) -> String {
    p.strip_prefix(root())
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Line comments are prose: a doc comment that names a layer is not a reach.
fn strip_comments(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `crate::<layer>` as a path, not as the prefix of a longer name.
fn names_layer(text: &str, layer: &str) -> bool {
    let needle = format!("crate::{layer}");
    let mut at = 0;
    while let Some(i) = text[at..].find(&needle) {
        let end = at + i + needle.len();
        at = end;
        if !text[end..].starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            return true;
        }
    }
    false
}

/// Every `fn` declaration's signature: from `fn` to the body or the `;` of a
/// trait method. A `where` clause is part of it, so a bound counts too.
fn signatures(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = text[at..].find("fn ") {
        let start = at + i;
        at = start + 3;
        let before_is_word = start > 0 && {
            let c = b[start - 1] as char;
            c.is_alphanumeric() || c == '_'
        };
        if before_is_word {
            continue;
        }
        let end = text[start..]
            .find(['{', ';'])
            .map_or(text.len(), |j| start + j);
        out.push(&text[start..end]);
        at = end;
    }
    out
}

fn has_word(text: &str, ident: &str) -> bool {
    let b = text.as_bytes();
    let mut at = 0;
    while let Some(i) = text[at..].find(ident) {
        let s = at + i;
        let e = s + ident.len();
        at = e;
        let before = s == 0
            || !{
                let c = b[s - 1] as char;
                c.is_alphanumeric() || c == '_'
            };
        let after = !text[e..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
        if before && after {
            return true;
        }
    }
    false
}
