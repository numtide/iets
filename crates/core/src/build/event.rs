//! What the dispatcher reports per attribute, for the terminal or CI renderer.

use std::collections::BTreeMap;
use std::time::Duration;

/// A state change of one attribute's build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// The builder started. `phase` is `None` until the build says otherwise.
    Started,
    Phase(String),
    /// One line of build output, already attributed to this build.
    Log(Vec<u8>),
    Built {
        outputs: BTreeMap<String, String>,
    },
    Failed {
        reason: String,
        /// The last `FAILURE_TAIL_LINES` of the build's own log.
        log_tail: Vec<String>,
    },
    Skipped {
        why: String,
    },
}

/// One event about one attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildEvent {
    /// The attribute path plus the source position when it is known, or the
    /// derivation name for a build the dispatcher was not asked for by name
    /// (a dependency).
    pub attr: String,
    pub drv: String,
    pub kind: Kind,
    /// Since this build started, or since the run started for an event
    /// that belongs to no build.
    pub elapsed: Duration,
    /// `file:line:col` of the `derivation` call, for the failure line.
    pub pos: Option<String>,
}

impl BuildEvent {
    pub fn new(attr: &str, drv: &str, kind: Kind, elapsed: Duration) -> BuildEvent {
        BuildEvent {
            attr: attr.to_string(),
            drv: drv.to_string(),
            kind,
            elapsed,
            pos: None,
        }
    }

    /// Where the derivation was defined.
    pub fn at(mut self, pos: Option<String>) -> BuildEvent {
        self.pos = pos;
        self
    }
}

/// `2s`, `3m04s`, `1h02m03s`.
pub fn fmt_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(Duration::from_secs(9)), "9s");
        assert_eq!(fmt_duration(Duration::from_secs(70)), "1m10s");
        assert_eq!(fmt_duration(Duration::from_secs(3671)), "1h01m11s");
    }
}
