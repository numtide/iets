//! The progress renderer. Everything goes to stderr, so stdout stays what
//! the command already printed.
//!
//! One buffer per build, flushed as one block when that build stops, so the
//! logs of concurrent builds never interleave (ci_renderer.py:34-197). The
//! verdict closes the block, except when the block is folded, where it is
//! the header. A failure prints the last `FAILURE_TAIL` lines. On Actions-style CI the
//! successes are folded with `::group::` markers. A heartbeat thread names
//! the running builds every `HEARTBEAT`, and a build silent for `STALL`
//! switches to live streaming.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::time::{Duration, Instant};

use super::event::{BuildEvent, Kind, fmt_duration};

/// Unfolded tail of a failure log.
pub const FAILURE_TAIL: usize = 200;
/// Lines kept per build; the oldest rotate out.
pub const BUFFER_LINES: usize = 10_000;
/// Lines shown when a build is escalated to live streaming.
const STALL_TAIL: usize = 5;
pub const HEARTBEAT: Duration = Duration::from_secs(30);
pub const STALL: Duration = Duration::from_secs(300);

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A terminal: one line per state change, logs only on failure.
    Tty,
    /// No terminal: the whole log of every build, in one block.
    Ci,
}

/// The renderer's settings. `fold` is on when the CI is Actions-shaped.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub mode: Mode,
    pub color: bool,
    pub fold: bool,
    pub quiet: bool,
}

impl Style {
    /// What the environment asks for: a terminal on stderr means `Tty`,
    /// `GITHUB_ACTIONS`/`GITEA_ACTIONS`/`FORGEJO_ACTIONS` mean folding.
    pub fn detect() -> Style {
        Style {
            mode: if is_tty() { Mode::Tty } else { Mode::Ci },
            color: is_tty() && std::env::var_os("NO_COLOR").is_none() && !crate::log::json(),
            fold: actions_ci(),
            quiet: false,
        }
    }
}

fn is_tty() -> bool {
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

/// The CIs whose logs understand `::group::` markers.
fn actions_ci() -> bool {
    ["GITHUB_ACTIONS", "GITEA_ACTIONS", "FORGEJO_ACTIONS"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

/// One build's buffered output.
struct Build {
    /// The attribute plus its source position, for the state-change lines.
    label: String,
    /// The attribute alone, for the per-line log prefix.
    short: String,
    /// Where the derivation was defined, for the failure line.
    pos: Option<String>,
    started: Instant,
    /// When the build's activity ended, for a build whose verdict the op that
    /// owns it reports later.
    ended: Option<Instant>,
    last_output: Instant,
    lines: VecDeque<String>,
    dropped: bool,
    phase: Option<String>,
    /// Escalated by the stall detector: lines go out as they arrive and the
    /// final block is skipped.
    streaming: bool,
}

impl Build {
    fn tail(&self, n: usize) -> Vec<String> {
        let skip = self.lines.len().saturating_sub(n);
        self.lines.iter().skip(skip).cloned().collect()
    }

    /// How long the build ran: to its activity's end when that is known.
    fn elapsed(&self) -> Duration {
        match self.ended {
            Some(t) => t.saturating_duration_since(self.started),
            None => self.started.elapsed(),
        }
    }
}

pub struct Renderer {
    style: Style,
    out: Box<dyn Write + Send>,
    running: HashMap<String, Build>,
    /// Buffers of builds that already stopped, for a failure reported after
    /// the activity ended. Bounded: only the most recent are kept.
    finished: VecDeque<(String, Vec<String>)>,
    /// Derivations the daemon named in a failure message. The stop of a
    /// build activity says nothing about success, so this is where a
    /// dependency's verdict comes from.
    failed: HashSet<String>,
    /// Derivations that already got a full verdict block, so the op's own
    /// result does not print the log a second time.
    verdicted: HashSet<String>,
    /// Derivations the running build op reports a verdict for itself. The end
    /// of their activity says nothing: a build hook ends two activities for
    /// one build, and only the op's result knows whether it succeeded.
    owned: HashSet<String>,
    /// Builds whose activity ended with no failure known yet. The daemon names
    /// a failed dependency only afterwards, so a verdict waits here until the
    /// op is over or the daemon contradicts it.
    held: HashMap<String, Build>,
}

impl Renderer {
    pub fn new(style: Style, out: Box<dyn Write + Send>) -> Renderer {
        Renderer {
            style,
            out,
            running: HashMap::new(),
            finished: VecDeque::new(),
            failed: HashSet::new(),
            verdicted: HashSet::new(),
            owned: HashSet::new(),
            held: HashMap::new(),
        }
    }

    /// The derivations the build op now running reports verdicts for.
    pub fn owns(&mut self, drvs: impl IntoIterator<Item = String>) {
        self.owned = drvs.into_iter().collect();
    }

    /// The op is over, so every failure it had to report has been reported and
    /// a held verdict is now known. Then the end of an activity decides a
    /// verdict again.
    pub fn release(&mut self) {
        self.owned.clear();
        let mut held: Vec<(String, Build)> = self.held.drain().collect();
        held.sort_by_key(|(_, b)| b.ended.unwrap_or(b.started));
        for (drv, b) in held {
            let ok = !self.failed.contains(&drv);
            self.verdicted.insert(drv);
            let why = (!ok).then_some("the daemon named it only after the build ended");
            self.verdict_block(&b, ok, why);
        }
    }

    fn sgr(&self, code: &str, s: &str) -> String {
        if self.style.color {
            format!("{code}{s}{RESET}")
        } else {
            s.to_string()
        }
    }

    /// What a build says; under internal-json its activity records say it.
    fn print(&mut self, lines: &[String]) {
        if !crate::log::json() {
            self.write(lines);
        }
    }

    fn write(&mut self, lines: &[String]) {
        for l in lines {
            let _ = writeln!(
                self.out,
                "{}",
                crate::log::on_stderr(&crate::log::format(l))
            );
        }
        let _ = self.out.flush();
    }

    fn line(&mut self, l: String) {
        self.print(&[l]);
    }

    /// One line, outside any build: a plan note, a daemon message.
    pub fn note(&mut self, msg: &str) {
        self.write(&[msg.to_string()]);
    }

    pub fn event(&mut self, e: &BuildEvent) {
        match &e.kind {
            Kind::Started => self.started(e),
            Kind::Phase(p) => self.phase(e, p),
            Kind::Log(bytes) => self.log(e, bytes),
            Kind::Built { .. } => self.verdict(e, true, None),
            Kind::Failed { reason, log_tail } => self.failure(e, reason, log_tail),
            Kind::Skipped { why } => {
                if !self.style.quiet {
                    let l = self.sgr(DIM, &format!("· {} skipped: {why}", e.attr));
                    self.line(l);
                }
            }
        }
    }

    fn started(&mut self, e: &BuildEvent) {
        // A build hook starts two activities for one derivation: its own
        // "building X on 'ssh-ng://…'" and the build behind it.
        if self.running.contains_key(&e.drv) {
            return;
        }
        let now = Instant::now();
        self.running.insert(
            e.drv.clone(),
            Build {
                label: e.attr.clone(),
                short: short_label(&e.attr),
                pos: e.pos.clone(),
                started: now,
                ended: None,
                last_output: now,
                lines: VecDeque::new(),
                dropped: false,
                phase: None,
                streaming: false,
            },
        );
        if !self.style.quiet {
            let l = format!("▶ building {}", e.attr);
            self.line(l);
        }
    }

    fn phase(&mut self, e: &BuildEvent, phase: &str) {
        let (label, elapsed) = match self.running.get_mut(&e.drv) {
            Some(b) => {
                b.phase = Some(phase.to_string());
                b.last_output = Instant::now();
                b.lines.push_back(format!("@ phase {phase}"));
                (b.label.clone(), b.started.elapsed())
            }
            None => (e.attr.clone(), e.elapsed),
        };
        if self.style.mode == Mode::Tty && !self.style.quiet {
            let l = self.sgr(
                DIM,
                &format!("  {label}: {phase} ({})", fmt_duration(elapsed)),
            );
            self.line(l);
        }
    }

    fn log(&mut self, e: &BuildEvent, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).into_owned();
        // A failure message names the derivation that failed, which is the
        // only per-derivation verdict the protocol carries.
        self.note_failures(&text);
        // Only the daemon's own line counts, and only until the plan confirms
        // it: a build's log can print the same words, and `fix-hashes` acts on
        // the answer. The line may end up in a build's buffer, which
        // `log::format` never sees, so it is read off here.
        if e.drv.is_empty() {
            crate::log::note_mismatch(&text);
        }
        // A line the daemon did not attribute belongs to the running build
        // when there is exactly one: that is what a remote builder's own
        // stderr looks like coming back through the daemon.
        let daemon = e.drv.is_empty() && !self.style.quiet;
        let key = match self.running.contains_key(&e.drv) {
            true => e.drv.clone(),
            false if self.running.len() == 1 => {
                if daemon && crate::log::json() {
                    self.note(&text);
                }
                self.running.keys().next().expect("one running").clone()
            }
            false => {
                if daemon {
                    self.note(&text);
                } else if !self.style.quiet {
                    self.line(text);
                }
                return;
            }
        };
        let Some(b) = self.running.get_mut(&key) else {
            return;
        };
        b.last_output = Instant::now();
        if b.lines.len() == BUFFER_LINES {
            b.lines.pop_front();
            b.dropped = true;
        }
        b.lines.push_back(text.clone());
        if b.streaming {
            let prefix = b.short.clone();
            let l = self.sgr(DIM, &format!("{prefix}>"));
            self.line(format!("{l} {text}"));
        }
    }

    /// Every `<hash>-<name>.drv` a failure message names.
    fn note_failures(&mut self, text: &str) {
        if !text.contains("failed") && !text.contains("error") {
            return;
        }
        for drv in drv_paths(&crate::log::strip_ansi(text)) {
            self.failed.insert(drv);
        }
    }

    /// A build's activity ended: flush its block, with the verdict the
    /// daemon's messages imply, and keep the tail for the op's own result.
    ///
    /// Nothing is printed for a derivation the running op owns: the activity's
    /// end says nothing about success, the op's result does, and a build hook
    /// ends two activities for one build. The buffer stays open, so the
    /// daemon's own error text still lands in that build's block.
    pub fn stopped(&mut self, drv: &str) {
        if self.owned.contains(drv) {
            if let Some(b) = self.running.get_mut(drv)
                && b.ended.is_none()
            {
                b.ended = Some(Instant::now());
            }
            return;
        }
        let Some(mut b) = self.running.remove(drv) else {
            return;
        };
        let tail = b.tail(FAILURE_TAIL);
        self.retain(drv, tail);
        if self.failed.contains(drv) {
            self.verdicted.insert(drv.to_string());
            self.verdict_block(&b, false, None);
            return;
        }
        // the daemon names a failed dependency only after the activity ends,
        // so a success is not known here: the verdict waits for `release`
        b.ended = Some(Instant::now());
        self.held.insert(drv.to_string(), b);
    }

    fn retain(&mut self, drv: &str, tail: Vec<String>) {
        self.finished.push_back((drv.to_string(), tail));
        while self.finished.len() > 64 {
            self.finished.pop_front();
        }
    }

    /// `✘  <attr> failed after <t> (<reason>), defined at <file>:<line>:<col>`:
    /// the attribute without its position, then the reason, then where the
    /// derivation was defined.
    fn failed_line(
        &self,
        attr: &str,
        pos: Option<&str>,
        elapsed: Duration,
        reason: Option<&str>,
    ) -> String {
        let mut l = format!("✘  {} failed after {}", attr, fmt_duration(elapsed));
        if let Some(r) = reason.filter(|r| !r.is_empty()) {
            l.push_str(&format!(" ({r})"));
        }
        if let Some(p) = pos {
            l.push_str(&format!(", defined at {p}"));
        }
        self.sgr(RED, &l)
    }

    fn verdict(&mut self, e: &BuildEvent, ok: bool, reason: Option<&str>) {
        match self.running.remove(&e.drv) {
            Some(b) => {
                let tail = b.tail(FAILURE_TAIL);
                self.retain(&e.drv, tail);
                self.verdict_block(&b, ok, reason);
            }
            None if !ok => {
                let l =
                    self.failed_line(&short_label(&e.attr), e.pos.as_deref(), e.elapsed, reason);
                self.line(l);
            }
            None => {}
        }
    }

    fn failure(&mut self, e: &BuildEvent, reason: &str, tail: &[String]) {
        if let Some(b) = self.held.remove(&e.drv) {
            self.verdicted.insert(e.drv.clone());
            self.verdict_block(&b, false, Some(reason));
            return;
        }
        if self.running.contains_key(&e.drv) {
            self.verdict(e, false, Some(reason));
            return;
        }
        if self.verdicted.contains(&e.drv) {
            // The block went out when the activity stopped. Say something
            // only when that block claimed a success the op now contradicts.
            if !self.failed.contains(&e.drv) {
                let l = self.failed_line(
                    &short_label(&e.attr),
                    e.pos.as_deref(),
                    e.elapsed,
                    Some(reason),
                );
                self.line(l);
            }
            return;
        }
        let head = self.failed_line(
            &short_label(&e.attr),
            e.pos.as_deref(),
            e.elapsed,
            Some(reason),
        );
        let prefix = self.sgr(DIM, &format!("{}>", e.attr));
        let mut out = vec![head];
        out.extend(tail.iter().map(|l| format!("{prefix} {l}")));
        out.push(self.sgr(
            RED,
            &format!("✘ end of log for {} · nix log {}", e.attr, e.drv),
        ));
        self.print(&out);
    }

    fn verdict_block(&mut self, b: &Build, ok: bool, reason: Option<&str>) {
        let verdict = if ok {
            self.sgr(
                GREEN,
                &format!("✔  {} ({})", b.label, fmt_duration(b.elapsed())),
            )
        } else {
            self.failed_line(&b.short, b.pos.as_deref(), b.elapsed(), reason)
        };
        if b.streaming && ok {
            // the log is already on screen; a failure repeats its tail, whose
            // cause came before the lines the stall showed
            self.line(verdict);
            return;
        }
        if self.style.quiet || (ok && self.style.mode == Mode::Tty) {
            self.line(verdict);
            return;
        }
        let prefix = self.sgr(DIM, &format!("{}>", b.short));
        let body: Vec<String> = if ok {
            b.lines.iter().map(|l| format!("{prefix} {l}")).collect()
        } else {
            b.tail(FAILURE_TAIL)
                .iter()
                .map(|l| format!("{prefix} {l}"))
                .collect()
        };
        let mut out = Vec::with_capacity(body.len() + 3);
        if ok && self.style.fold {
            out.push(format!("::group::{verdict}"));
            out.extend(body);
            out.push("::endgroup::".into());
        } else if ok {
            // the verdict closes the block: a build that has printed a ✔ must
            // not keep talking. Folding needs it first, and so does a failure,
            // whose head is what a reader scrolls back to.
            out.extend(body);
            out.push(verdict);
        } else {
            let dropped = if b.dropped {
                format!(" (oldest lines dropped, buffer={BUFFER_LINES})")
            } else {
                String::new()
            };
            out.push(format!("{verdict}{dropped}"));
            out.extend(body);
            out.push(self.sgr(RED, &format!("✘ end of log for {}", b.label)));
        }
        self.print(&out);
    }

    /// The tail kept for a build that already stopped.
    pub fn retained_tail(&self, drv: &str) -> Vec<String> {
        self.finished
            .iter()
            .rev()
            .find(|(d, _)| d == drv)
            .map(|(_, t)| t.clone())
            .unwrap_or_default()
    }

    /// The live buffer of a running build.
    pub fn live_tail(&self, drv: &str) -> Vec<String> {
        self.running
            .get(drv)
            .map(|b| b.tail(FAILURE_TAIL))
            .unwrap_or_default()
    }

    /// One heartbeat: escalate the stalled builds, then name what is running.
    pub fn heartbeat(&mut self) {
        self.check_stalls();
        if self.running.is_empty() || self.style.quiet {
            return;
        }
        let mut builds: Vec<(String, Duration, Option<String>, bool)> = self
            .running
            .values()
            .map(|b| {
                (
                    b.label.clone(),
                    b.started.elapsed(),
                    b.phase.clone(),
                    b.streaming,
                )
            })
            .collect();
        builds.sort_by_key(|b| std::cmp::Reverse(b.1));
        let parts: Vec<String> = builds
            .iter()
            .map(|(label, el, phase, streaming)| {
                let mut detail = fmt_duration(*el);
                if let Some(p) = phase {
                    detail.push_str(&format!(", {p}"));
                }
                format!(
                    "{label} ({detail}){}",
                    if *streaming { " ⚠stalled" } else { "" }
                )
            })
            .collect();
        let l = self.sgr(
            DIM,
            &format!("⏵ {} building: {}", builds.len(), parts.join(", ")),
        );
        self.line(l);
    }

    fn check_stalls(&mut self) {
        let stalled: Vec<String> = self
            .running
            .iter()
            .filter(|(_, b)| !b.streaming && b.last_output.elapsed() >= STALL)
            .map(|(d, _)| d.clone())
            .collect();
        for drv in stalled {
            let (label, silent, tail) = {
                let b = &self.running[&drv];
                (b.short.clone(), b.last_output.elapsed(), b.tail(STALL_TAIL))
            };
            let prefix = self.sgr(DIM, &format!("{label}>"));
            let mut out = vec![self.sgr(
                BOLD,
                &format!(
                    "⚠ {label}: no output for {}, last lines:",
                    fmt_duration(silent)
                ),
            )];
            out.extend(tail.iter().map(|l| format!("{prefix} {l}")));
            out.push(self.sgr(BOLD, &format!("⚠ {label}: streaming further output live")));
            self.print(&out);
            if let Some(b) = self.running.get_mut(&drv) {
                b.streaming = true;
            }
        }
    }
}

/// The label without the source position: `checks.x.foo (f.nix:1:2)` as
/// `checks.x.foo`, so a log prefix stays short.
fn short_label(label: &str) -> String {
    match label.split_once(" (") {
        Some((attr, _)) => attr.to_string(),
        None => label.to_string(),
    }
}

/// Every `/nix/store/<name>.drv` in `text`, in order, without duplicates.
fn drv_paths(text: &str) -> Vec<String> {
    const PREFIX: &str = "/nix/store/";
    let mut out: Vec<String> = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(rel) = text[i..].find(PREFIX) {
        let start = i + rel;
        let mut end = start + PREFIX.len();
        while end < b.len()
            && (b[end].is_ascii_alphanumeric() || matches!(b[end], b'+' | b'-' | b'.' | b'_'))
        {
            end += 1;
        }
        let path = &text[start..end];
        if path.ends_with(".drv") && !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
        i = end.max(start + PREFIX.len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn ci(fold: bool) -> (Renderer, Sink) {
        let s = Sink::default();
        let style = Style {
            mode: Mode::Ci,
            color: false,
            fold,
            quiet: false,
        };
        (Renderer::new(style, Box::new(s.clone())), s)
    }

    /// A duration as `fmt_duration` writes one: `0s`, `12s`, `3m04s`,
    /// `1h02m03s`.
    fn is_duration(s: &str) -> bool {
        s.ends_with('s')
            && s.starts_with(|c: char| c.is_ascii_digit())
            && s.chars()
                .all(|c| c.is_ascii_digit() || matches!(c, 'h' | 'm' | 's'))
    }

    /// The sink's text, with every rendered duration normalised to `0s`.
    /// A build that is still running is timed from `Instant::now()`
    /// (`Build::elapsed`), so a loaded machine renders `1s` where an idle
    /// one renders `0s` and an assertion on the whole verdict line fails.
    /// Nothing here is about the clock: `event.rs` tests `fmt_duration`.
    fn text(s: &Sink) -> String {
        let raw = String::from_utf8(s.0.lock().unwrap().clone()).unwrap();
        raw.split_inclusive([' ', '\n'])
            .map(|t| {
                let core = t.trim_matches(|c: char| matches!(c, ' ' | '\n' | '(' | ')' | ','));
                match is_duration(core) {
                    true => t.replacen(core, "0s", 1),
                    false => t.to_string(),
                }
            })
            .collect()
    }

    fn ev(kind: Kind) -> BuildEvent {
        BuildEvent::new("checks.x.a", "/nix/store/a.drv", kind, Duration::ZERO)
    }

    #[test]
    fn ci_folds_a_success_and_never_a_failure() {
        let (mut r, s) = ci(true);
        r.event(&ev(Kind::Started));
        r.event(&ev(Kind::Log(b"line one".to_vec())));
        r.stopped("/nix/store/a.drv");
        r.release();
        let t = text(&s);
        assert!(t.contains("::group::✔  checks.x.a"), "{t}");
        assert!(t.contains("checks.x.a> line one"));
        assert!(t.contains("::endgroup::"));

        let (mut r, s) = ci(true);
        r.event(&ev(Kind::Started));
        r.event(&ev(Kind::Log(b"boom".to_vec())));
        r.event(&ev(Kind::Failed {
            reason: "status 1".into(),
            log_tail: vec![],
        }));
        let t = text(&s);
        assert!(!t.contains("::group::"), "{t}");
        assert!(t.contains("✘  checks.x.a failed after 0s (status 1)"));
        assert!(t.contains("checks.x.a> boom"));
    }

    /// Unfolded, the ✔ is the last line of the block: a build that reports
    /// success must not keep printing its log afterwards.
    #[test]
    fn an_unfolded_success_ends_with_its_verdict() {
        let (mut r, s) = ci(false);
        r.event(&ev(Kind::Started));
        r.event(&ev(Kind::Log(b"line one".to_vec())));
        r.stopped("/nix/store/a.drv");
        r.release();
        let t = text(&s);
        let log = t.find("checks.x.a> line one").expect("the log line");
        let done = t.find("✔  checks.x.a").expect("the verdict");
        assert!(
            t.find("▶ building").expect("the start") < log && log < done,
            "{t}"
        );
    }

    #[test]
    fn logs_of_two_builds_do_not_interleave() {
        let (mut r, s) = ci(false);
        let a = BuildEvent::new("a", "/nix/store/a.drv", Kind::Started, Duration::ZERO);
        let b = BuildEvent::new("b", "/nix/store/b.drv", Kind::Started, Duration::ZERO);
        r.event(&a);
        r.event(&b);
        r.event(&BuildEvent::new(
            "a",
            "/nix/store/a.drv",
            Kind::Log(b"a1".to_vec()),
            Duration::ZERO,
        ));
        r.event(&BuildEvent::new(
            "b",
            "/nix/store/b.drv",
            Kind::Log(b"b1".to_vec()),
            Duration::ZERO,
        ));
        r.event(&BuildEvent::new(
            "a",
            "/nix/store/a.drv",
            Kind::Log(b"a2".to_vec()),
            Duration::ZERO,
        ));
        r.stopped("/nix/store/a.drv");
        r.release();
        r.stopped("/nix/store/b.drv");
        r.release();
        let t = text(&s);
        let a1 = t.find("a> a1").unwrap();
        let a2 = t.find("a> a2").unwrap();
        let b1 = t.find("b> b1").unwrap();
        assert!(a1 < a2 && a2 < b1, "{t}");
    }

    #[test]
    fn tty_keeps_successful_logs_off_the_screen() {
        let s = Sink::default();
        let style = Style {
            mode: Mode::Tty,
            color: false,
            fold: false,
            quiet: false,
        };
        let mut r = Renderer::new(style, Box::new(s.clone()));
        r.event(&ev(Kind::Started));
        r.event(&ev(Kind::Log(b"noise".to_vec())));
        r.event(&ev(Kind::Phase("build".into())));
        r.stopped("/nix/store/a.drv");
        r.release();
        let t = text(&s);
        assert!(t.contains("▶ building checks.x.a"));
        assert!(t.contains("checks.x.a: build (0s)"));
        assert!(t.contains("✔  checks.x.a (0s)"));
        assert!(!t.contains("noise"), "{t}");
    }

    #[test]
    fn a_daemon_failure_message_decides_the_verdict() {
        let (mut r, s) = ci(false);
        r.event(&ev(Kind::Started));
        r.event(&ev(Kind::Log(b"about to fail".to_vec())));
        // Unattributed, as the daemon sends it, with nix's colours.
        r.event(&BuildEvent::new(
            "",
            "",
            Kind::Log(
                b"\x1b[31;1merror:\x1b[0m build of '\x1b[35;1m/nix/store/a.drv\x1b[0m' failed"
                    .to_vec(),
            ),
            Duration::ZERO,
        ));
        r.stopped("/nix/store/a.drv");
        r.release();
        let t = text(&s);
        assert!(t.contains("✘  checks.x.a failed after 0s"), "{t}");
        assert!(!t.contains("✔"), "{t}");
        // The op's own result must not repeat the whole log.
        r.event(&ev(Kind::Failed {
            reason: "status 9".into(),
            log_tail: vec![],
        }));
        let t = text(&s);
        assert_eq!(t.matches("about to fail").count(), 1, "{t}");
    }

    /// A build hook: two activities for one derivation, and the op that owns
    /// it has the only verdict there is.
    #[test]
    fn an_owned_build_is_verdicted_once_by_its_op() {
        let (mut r, s) = ci(false);
        let at = |kind: Kind| {
            BuildEvent::new(
                "prov-fail (/home/u/repo/default.nix:2:3)",
                "/nix/store/a.drv",
                kind,
                Duration::ZERO,
            )
            .at(Some("/home/u/repo/default.nix:2:3".into()))
        };
        r.owns(["/nix/store/a.drv".to_string()]);
        r.event(&at(Kind::Started));
        r.event(&at(Kind::Started));
        r.event(&at(Kind::Log(b"about to fail".to_vec())));
        // The hook's activity ends, then the build's; neither is a verdict.
        r.stopped("/nix/store/a.drv");
        r.stopped("/nix/store/a.drv");
        // The daemon's own error text is unattributed, and still belongs to
        // the one build that is running.
        r.event(&BuildEvent::new(
            "",
            "",
            Kind::Log(b"error: build of '/nix/store/a.drv' on 'ssh-ng://b' failed".to_vec()),
            Duration::ZERO,
        ));
        let t = text(&s);
        assert_eq!(t.matches("▶ building").count(), 1, "{t}");
        assert!(!t.contains("✔"), "{t}");

        r.event(&at(Kind::Failed {
            reason: "status 1: builder failed".into(),
            log_tail: vec![],
        }));
        let t = text(&s);
        assert!(
            t.contains(
                "✘  prov-fail failed after 0s (status 1: builder failed), \
                 defined at /home/u/repo/default.nix:2:3"
            ),
            "{t}"
        );
        assert!(t.contains("prov-fail> about to fail"), "{t}");
        // The daemon's text went through the build's own block, not raw.
        assert!(t.contains("prov-fail> error: build of"), "{t}");
    }

    #[test]
    fn drv_paths_of_a_failure_message() {
        let msg =
            "error: builder for '/nix/store/2ijv-x.drv' failed; nix log /nix/store/2ijv-x.drv";
        assert_eq!(drv_paths(msg), ["/nix/store/2ijv-x.drv"]);
        assert!(drv_paths("/nix/store/2ijv-x is an output").is_empty());
    }

    /// The daemon names a failed dependency after its activity ended, so the
    /// end of the op, not the order of the messages, decides the verdict.
    #[test]
    fn a_failure_after_the_activity_ended_never_claims_a_success() {
        let (mut r, s) = ci(false);
        r.event(&ev(Kind::Started));
        r.event(&ev(Kind::Log(b"work".to_vec())));
        r.stopped("/nix/store/a.drv");
        assert!(!text(&s).contains("✔"), "a verdict before the op is over");
        r.event(&BuildEvent::new(
            "",
            "",
            Kind::Log(b"error: build of '/nix/store/a.drv' failed".to_vec()),
            Duration::ZERO,
        ));
        r.release();
        let t = text(&s);
        assert!(t.contains("✘  checks.x.a failed after 0s"), "{t}");
        assert!(!t.contains("✔"), "{t}");
    }

    /// Only the daemon's own line may report a mismatch; a build's log line
    /// prints the same words and means nothing.
    #[test]
    fn a_build_log_never_reports_a_mismatch() {
        let s = crate::session::Session::new();
        let _entered = s.enter();
        let line = format!(
            "{} 'x':\n  specified: sha256-AoAoAoAoAo=\n     got:    sha256-BoBoBoBoBo=",
            crate::log::MISMATCH
        );
        let (mut r, _sink) = ci(false);
        r.event(&ev(Kind::Started));
        r.event(&ev(Kind::Log(line.clone().into_bytes())));
        assert_eq!(crate::log::resolve_mismatches(&|_| true), 0);
        r.event(&BuildEvent::new(
            "",
            "",
            Kind::Log(line.into_bytes()),
            Duration::ZERO,
        ));
        assert_eq!(crate::log::resolve_mismatches(&|_| true), 1);
    }

    #[test]
    fn heartbeat_names_running_builds() {
        let (mut r, s) = ci(false);
        r.event(&ev(Kind::Started));
        r.event(&ev(Kind::Phase("configure".into())));
        r.heartbeat();
        assert!(text(&s).contains("⏵ 1 building: checks.x.a (0s, configure)"));
    }

    #[test]
    fn a_stalled_build_that_fails_still_shows_its_cause() {
        let (mut r, s) = ci(false);
        r.event(&ev(Kind::Started));
        r.event(&ev(Kind::Log(b"error: the cause".to_vec())));
        for _ in 0..STALL_TAIL {
            r.event(&ev(Kind::Log(b"waiting".to_vec())));
        }
        r.running.get_mut("/nix/store/a.drv").unwrap().streaming = true;
        r.event(&ev(Kind::Failed {
            reason: "status 1".into(),
            log_tail: vec![],
        }));
        assert!(text(&s).contains("checks.x.a> error: the cause"));
    }
}
