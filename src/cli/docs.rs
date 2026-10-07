//! The documentation an installed binary carries: `iets help TOPIC`.
//!
//! The reference quadrant is embedded with `include_str!`, so a binary
//! answers `iets help settings` with no `docs/` directory in sight. The
//! other quadrants stay in the repository and their topics print the path
//! they live at.

use std::process::exit;

use crate::cli::help;

/// One documentation page `iets help` can be asked for.
pub struct Page {
    /// The bare topic name, which is also the file's base name.
    pub topic: &'static str,
    /// The directory under `docs/`; it also qualifies the topic, so
    /// `reference/plumbing` names the page and `plumbing` the command.
    pub quadrant: &'static str,
    /// One line for the topic list.
    pub about: &'static str,
    /// The page itself, when the binary carries it.
    pub text: Option<&'static str>,
}

const fn embedded(topic: &'static str, about: &'static str, text: &'static str) -> Page {
    Page {
        topic,
        quadrant: "reference",
        about,
        text: Some(text),
    }
}

const fn in_repo(quadrant: &'static str, topic: &'static str, about: &'static str) -> Page {
    Page {
        topic,
        quadrant,
        about,
        text: None,
    }
}

/// Every page, embedded ones first.
pub const PAGES: &[Page] = &[
    embedded(
        "commands",
        "every command, its flags and its exit codes",
        include_str!("../../docs/reference/commands.md"),
    ),
    embedded(
        "plumbing",
        "the machine-first operations and their JSON",
        include_str!("../../docs/reference/plumbing.md"),
    ),
    embedded(
        "classic",
        "the classic Nix names and the fallback rules",
        include_str!("../../docs/reference/classic.md"),
    ),
    embedded(
        "builders",
        "builder URLs, the worker protocol, grpc and NAR",
        include_str!("../../docs/reference/builders.md"),
    ),
    embedded(
        "flakes",
        "flake references, getFlake and the fetchers",
        include_str!("../../docs/reference/flakes.md"),
    ),
    embedded(
        "settings",
        "iets.toml, the IETS_* variables and nix.conf",
        include_str!("../../docs/reference/settings.md"),
    ),
    embedded(
        "events",
        "the JSON event stream of 'IETS_EVENTS'",
        include_str!("../../docs/reference/events.md"),
    ),
    embedded(
        "profile",
        "the sampled flame graph of 'IETS_PROFILE=eval'",
        include_str!("../../docs/reference/profile.md"),
    ),
    embedded(
        "errors",
        "the error codes, the exit codes and the log records",
        include_str!("../../docs/reference/errors.md"),
    ),
    embedded(
        "lints",
        "every lint, its level and its trigger",
        include_str!("../../docs/reference/lints.md"),
    ),
    embedded(
        "syntax-tree",
        "the JSON of 'iets parse --json'",
        include_str!("../../docs/reference/syntax-tree.md"),
    ),
    embedded(
        "builtins",
        "the primop API and every builtin",
        include_str!("../../docs/reference/builtins.md"),
    ),
    embedded(
        "divergences",
        "where iets differs from Nix 2.34.8",
        include_str!("../../docs/reference/divergences.md"),
    ),
    embedded(
        "library",
        "the crate's entry points, for a frontend",
        include_str!("../../docs/reference/library.md"),
    ),
    in_repo(
        "tutorial",
        "getting-started",
        "install it and evaluate something",
    ),
    in_repo(
        "how-to",
        "build-remotely",
        "send the builds to another machine",
    ),
    in_repo("how-to", "use-with-direnv", "a dev shell that loads itself"),
    in_repo(
        "how-to",
        "replace-nix-tools",
        "put the classic names on PATH",
    ),
    in_repo("how-to", "fix-hashes", "let a build write the hash it got"),
    in_repo("how-to", "script-iets", "drive it from a program"),
    in_repo("how-to", "embed-the-evaluator", "link the library"),
    in_repo("explanation", "evaluation", "how the evaluator works"),
    in_repo("explanation", "caching", "what the caches hold and key on"),
    in_repo(
        "explanation",
        "flakes-and-sources",
        "where a tree comes from",
    ),
    in_repo(
        "explanation",
        "plumbing-and-porcelain",
        "why the operations exist",
    ),
    in_repo(
        "explanation",
        "provenance",
        "store paths back to local trees",
    ),
    in_repo("explanation", "lint-scope", "what iets lints, and why"),
    in_repo("explanation", "history", "how it got here"),
];

/// The page `name` asks for: the bare topic, or `quadrant/topic`.
pub fn find(name: &str) -> Option<&'static Page> {
    PAGES.iter().find(|p| {
        p.topic == name
            || name
                .strip_prefix(p.quadrant)
                .and_then(|r| r.strip_prefix('/'))
                == Some(p.topic)
    })
}

/// Every topic name, for a suggestion.
pub fn names() -> impl Iterator<Item = &'static str> {
    PAGES.iter().map(|p| p.topic)
}

/// The topic block of `iets help`.
pub fn list() -> String {
    let mut s = String::from("\ntopics: 'iets help TOPIC' prints a page\n");
    for p in PAGES {
        let mark = if p.text.is_some() { ' ' } else { '*' };
        s.push_str(&format!("{mark} {:<22} {}\n", p.topic, p.about));
    }
    s.push_str(
        "\n* is a page this binary does not carry: it is docs/<quadrant>/<topic>.md\n  \
         in the iets repository, and 'iets help TOPIC' prints that path.\n",
    );
    s
}

/// Prints the page, or where it lives when the binary does not carry it.
pub fn show(p: &Page) -> ! {
    match p.text {
        Some(t) => help::out(t),
        None => help::out(&format!(
            "'{topic}' is docs/{q}/{topic}.md in the iets repository.\n\
             This binary carries the reference pages only; 'iets help' lists them.\n",
            topic = p.topic,
            q = p.quadrant
        )),
    }
    exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_page_has_a_heading() {
        for p in PAGES {
            let Some(t) = p.text else { continue };
            assert!(t.starts_with("# "), "{}", p.topic);
            assert!(t.len() > 1000, "{}", p.topic);
        }
    }

    #[test]
    fn topics_resolve_bare_and_qualified() {
        assert_eq!(find("settings").map(|p| p.topic), Some("settings"));
        assert_eq!(
            find("reference/plumbing").map(|p| p.topic),
            Some("plumbing")
        );
        assert_eq!(
            find("explanation/caching").map(|p| p.topic),
            Some("caching")
        );
        assert!(find("reference/caching").is_none());
        assert!(find("nonesuch").is_none());
    }

    #[test]
    fn the_topic_list_fits_80_columns() {
        for l in list().lines() {
            assert!(l.chars().count() <= 80, "{l}");
        }
    }
}
