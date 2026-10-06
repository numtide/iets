//! Which worker addresses are worth calling, shared by the threads of one
//! batch.
//!
//! The scheduler keeps a worker placeable until it notices the worker is gone
//! (42 s seen on farm.numtide.com, 2026-09-22) and a bounce off it is instant.
//! `build::reschedule_pause` backs each derivation off on its own, so
//! derivations placed on one dead address each walk their own ladder and the
//! client concludes nothing.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Consecutive bounces before an address is refused. One decides nothing: a
/// superseded assignment, a worker that started draining and a build the
/// scheduler re-placed each bounce exactly once.
const THRESHOLD: u32 = 2;

#[derive(Default)]
struct Entry {
    /// Consecutive bounces since the last build this address completed.
    bounces: u32,
    /// When it may be called again, once the circuit is open.
    until: Option<Instant>,
}

#[derive(Default)]
pub struct Health {
    workers: Mutex<HashMap<String, Entry>>,
}

impl Health {
    /// How long to wait before calling `addr` again, `None` when it is usable.
    pub fn refusal(&self, addr: &str) -> Option<Duration> {
        let now = Instant::now();
        let mut w = lock(&self.workers);
        let e = w.get_mut(addr)?;
        let until = e.until?;
        let wait = until.checked_duration_since(now);
        if wait.is_none() {
            // Let one caller probe it, and close it again on the next bounce.
            e.until = None;
        }
        wait
    }

    pub fn bounced(&self, addr: &str) {
        let mut w = lock(&self.workers);
        let e = w.entry(addr.to_string()).or_default();
        e.bounces += 1;
        if e.bounces >= THRESHOLD {
            e.until = Some(Instant::now() + cooldown(e.bounces - THRESHOLD));
        }
    }

    /// The address returned a build result, failed or not: it is serving.
    pub fn worked(&self, addr: &str) {
        lock(&self.workers).remove(addr);
    }
}

/// The ladder `build::reschedule_pause` walks, so there is one curve: 1 s
/// doubling to 30 s.
fn cooldown(n: u32) -> Duration {
    Duration::from_secs((1u64 << n.min(5)).min(30))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_bounce_does_not_refuse_an_address() {
        let h = Health::default();
        h.bounced("a:1");
        assert!(h.refusal("a:1").is_none());
    }

    #[test]
    fn two_bounces_refuse_it() {
        let h = Health::default();
        h.bounced("a:1");
        h.bounced("a:1");
        assert!(h.refusal("a:1").is_some());
    }

    #[test]
    fn an_unknown_address_is_usable() {
        assert!(Health::default().refusal("a:1").is_none());
    }

    #[test]
    fn a_completed_build_clears_the_history() {
        let h = Health::default();
        h.bounced("a:1");
        h.bounced("a:1");
        h.worked("a:1");
        assert!(h.refusal("a:1").is_none());
        h.bounced("a:1");
        assert!(h.refusal("a:1").is_none());
    }

    #[test]
    fn one_address_does_not_refuse_another() {
        let h = Health::default();
        h.bounced("a:1");
        h.bounced("a:1");
        assert!(h.refusal("b:2").is_none());
    }

    #[test]
    fn the_cooldown_grows_and_stops_at_30s() {
        let s: Vec<u64> = (0..8).map(|n| cooldown(n).as_secs()).collect();
        assert_eq!(s, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(cooldown(u32::MAX).as_secs(), 30);
    }

    #[test]
    fn a_passed_cooldown_lets_one_caller_through() {
        let h = Health::default();
        h.bounced("a:1");
        h.bounced("a:1");
        lock(&h.workers).get_mut("a:1").unwrap().until = Some(Instant::now());
        assert!(h.refusal("a:1").is_none());
        assert!(h.refusal("a:1").is_none());
        h.bounced("a:1");
        let wait = h.refusal("a:1").expect("refused again");
        assert!(wait > Duration::ZERO && wait <= cooldown(1), "{wait:?}");
    }
}
