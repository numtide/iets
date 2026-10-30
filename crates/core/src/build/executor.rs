//! The places a run may build, and which one takes a derivation.
//!
//! `builder` names one or more. A place carries what it will accept, declared
//! rather than probed: the worker protocol has no op that asks a daemon which
//! platforms it builds, and nix.conf's `builders` declares the same facts for
//! the same reason.
//!
//! The unit of placement is a root and everything under it, not a single
//! derivation. Both backends resolve a root's closure themselves — the daemon
//! from `BuildPathsWithResults`, the farm from its own scheduler — so a root
//! placed here has its whole closure built here. That is what keeps this
//! stage free of cross-place edges: nothing a place builds depends on
//! something another place holds. The cost is that two roots placed
//! differently build what they share twice (`docs/reference/settings.md`,
//! `builder`).

use crate::error::{Error, Result};
use crate::remote::worker::Transport;
use crate::settings::BuilderSpec;

/// One place, its URL resolved to a transport.
#[derive(Debug)]
pub struct Place {
    pub spec: BuilderSpec,
    pub transport: Transport,
}

impl Place {
    /// Whether this place accepts a derivation for `platform` that needs
    /// `features`.
    ///
    /// An undeclared constraint accepts anything, so one place that declares
    /// nothing takes every derivation, which is what a single `builder` has
    /// always done: the store itself refuses what it cannot build, and it is
    /// the only one that knows.
    pub fn takes(&self, platform: &str, features: &[String]) -> bool {
        let systems = &self.spec.systems;
        let ours = &self.spec.features;
        (systems.is_empty() || systems.iter().any(|s| s == platform))
            && (ours.is_empty() || features.iter().all(|f| ours.contains(f)))
    }
}

/// Every place of this run, in the order `builder` named them.
#[derive(Debug)]
pub struct Places(Vec<Place>);

impl Places {
    pub fn parse(specs: &[BuilderSpec], system: &str) -> Result<Places> {
        let places = specs
            .iter()
            .map(|spec| {
                let transport = Transport::parse_checked(&spec.url, system)
                    .map_err(|e| Error::Usage(e.to_string()))?
                    .ok_or_else(|| Error::Usage(unsupported(&spec.url)))?;
                Ok(Place {
                    spec: spec.clone(),
                    transport,
                })
            })
            .collect::<Result<Vec<Place>>>()?;
        if places.is_empty() {
            return Err(Error::Usage("builder names no place to build".into()));
        }
        Ok(Places(places))
    }

    pub fn get(&self, i: usize) -> &Place {
        &self.0[i]
    }

    /// The first place that accepts the derivation. First, not best: this
    /// stage has no cost model, so the order `builder` gives is the policy.
    pub fn pick(&self, platform: &str, features: &[String]) -> Option<usize> {
        self.0.iter().position(|p| p.takes(platform, features))
    }

    /// Why nothing took it, naming every place and what it declared.
    pub fn refused(&self, platform: &str, features: &[String]) -> Error {
        let what = if features.is_empty() {
            format!("'{platform}'")
        } else {
            format!("'{platform}' with {}", features.join(", "))
        };
        let lines: Vec<String> = self
            .0
            .iter()
            .map(|p| {
                let systems = if p.spec.systems.is_empty() {
                    "any system".to_string()
                } else {
                    p.spec.systems.join(", ")
                };
                let features = if p.spec.features.is_empty() {
                    String::new()
                } else {
                    format!(", features {}", p.spec.features.join(", "))
                };
                format!("  {}: {systems}{features}", p.spec.url)
            })
            .collect();
        Error::Usage(format!(
            "no builder takes a derivation for {what}\n{}",
            lines.join("\n")
        ))
    }
}

fn unsupported(url: &str) -> String {
    format!(
        "unsupported builder '{url}': expected daemon, unix:///path, ssh-ng://[user@]host or grpc://host:port"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(url: &str, systems: &[&str], features: &[&str]) -> BuilderSpec {
        BuilderSpec {
            url: url.to_string(),
            systems: systems.iter().map(|s| s.to_string()).collect(),
            features: features.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn places(specs: &[BuilderSpec]) -> Places {
        Places::parse(specs, "x86_64-linux").unwrap()
    }

    fn feats(f: &[&str]) -> Vec<String> {
        f.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_place_declaring_nothing_takes_everything() {
        let p = places(&[BuilderSpec::bare("daemon")]);
        assert_eq!(p.pick("x86_64-linux", &[]), Some(0));
        assert_eq!(p.pick("aarch64-darwin", &feats(&["big-parallel"])), Some(0));
    }

    #[test]
    fn a_declared_system_filters() {
        let p = places(&[
            spec("daemon", &["x86_64-linux"], &[]),
            spec("ssh-ng://bld1", &["aarch64-linux"], &[]),
        ]);
        assert_eq!(p.pick("x86_64-linux", &[]), Some(0));
        assert_eq!(p.pick("aarch64-linux", &[]), Some(1));
        assert_eq!(p.pick("x86_64-darwin", &[]), None);
    }

    #[test]
    fn a_feature_the_place_lacks_rules_it_out() {
        let p = places(&[
            spec("daemon", &["x86_64-linux"], &["nixos-test"]),
            spec("ssh-ng://bld1", &["x86_64-linux"], &["nixos-test", "kvm"]),
        ]);
        assert_eq!(p.pick("x86_64-linux", &feats(&["kvm"])), Some(1));
        assert_eq!(p.pick("x86_64-linux", &feats(&["nixos-test"])), Some(0));
        assert_eq!(p.pick("x86_64-linux", &feats(&["big-parallel"])), None);
    }

    #[test]
    fn the_first_place_that_takes_it_wins() {
        let p = places(&[
            BuilderSpec::bare("daemon"),
            BuilderSpec::bare("ssh-ng://bld1"),
        ]);
        assert_eq!(p.pick("x86_64-linux", &[]), Some(0));
    }

    #[test]
    fn a_refusal_names_every_place_and_what_it_declared() {
        let p = places(&[
            spec("daemon", &["x86_64-linux"], &[]),
            spec("ssh-ng://bld1", &["aarch64-linux"], &["kvm"]),
        ]);
        let e = p.refused("x86_64-darwin", &feats(&["kvm"])).to_string();
        assert!(e.contains("'x86_64-darwin' with kvm"), "{e}");
        assert!(e.contains("daemon: x86_64-linux"), "{e}");
        assert!(
            e.contains("ssh-ng://bld1: aarch64-linux, features kvm"),
            "{e}"
        );
    }

    #[test]
    fn an_unsupported_url_says_what_is_expected() {
        let e = Places::parse(&[BuilderSpec::bare("s3://bucket")], "x86_64-linux")
            .unwrap_err()
            .to_string();
        assert!(e.contains("unsupported builder 's3://bucket'"), "{e}");
    }

    #[test]
    fn no_place_at_all_is_a_usage_error() {
        assert!(Places::parse(&[], "x86_64-linux").is_err());
    }
}
