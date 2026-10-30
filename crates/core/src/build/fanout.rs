//! Running a dependency-ordered job set with a cap on how many run at once,
//! for a backend that schedules the builds itself (the `grpc://` farm: one
//! `BuildDerivation` per derivation).
//!
//! Every event a job produces travels over one channel and reaches `on_event`
//! on the calling thread, so the dispatcher's renderer sees one merged stream
//! and a job's log lines stay attributed to it.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Sender};
use std::sync::{Condvar, Mutex};
use std::thread;

use crate::remote::event::Event;
use crate::session;

/// What one job needs from the scheduler: which jobs must finish first.
pub struct Job {
    /// Indices into the job list.
    pub deps: Vec<usize>,
}

/// The ready queue and one result slot per job (store.hh:`FarmRun`).
struct Run<R> {
    ready: VecDeque<usize>,
    remaining: usize,
    waiting: Vec<usize>,
    results: Vec<Option<R>>,
    /// The dependency that failed, for a job that will never run.
    blocked: Vec<Option<usize>>,
}

/// What one worker thread does with a job: build it, or record that a
/// dependency failed.
pub struct Work<'a, R> {
    /// Builds job `i`, reporting through `tx`.
    pub build: &'a (dyn Fn(usize, &Sender<Event>) -> R + Sync),
    /// True when a result lets the jobs that depend on it run.
    pub ok: &'a (dyn Fn(&R) -> bool + Sync),
    /// The result for job `i` when dependency `dep` failed.
    pub blocked: &'a (dyn Fn(usize, usize) -> R + Sync),
}

/// Runs every job, at most `limit` at a time, a job only once every job it
/// depends on is done. A job whose dependency failed is never built; it gets
/// `Work::blocked`'s result instead.
pub fn run<R: Send>(
    jobs: &[Job],
    limit: usize,
    work: &Work<'_, R>,
    on_event: &mut dyn FnMut(Event),
) -> Vec<Option<R>> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let run = Mutex::new(Run {
        ready: (0..jobs.len())
            .filter(|&i| jobs[i].deps.is_empty())
            .collect(),
        remaining: jobs.len(),
        waiting: jobs.iter().map(|j| j.deps.len()).collect(),
        results: (0..jobs.len()).map(|_| None).collect(),
        blocked: vec![None; jobs.len()],
    });
    let wake = Condvar::new();
    // Who waits on whom, the other way round.
    let mut dependants: Vec<Vec<usize>> = vec![Vec::new(); jobs.len()];
    for (i, j) in jobs.iter().enumerate() {
        for &d in &j.deps {
            dependants[d].push(i);
        }
    }
    let dependants = &dependants;
    let threads = limit.max(1).min(jobs.len());

    thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<Event>();
        for _ in 0..threads {
            let tx = tx.clone();
            let run = &run;
            let wake = &wake;
            s.spawn(session::bind(move || {
                while let Some(i) = next(run, wake) {
                    let blocked = run.lock().unwrap().blocked[i];
                    let res = match blocked {
                        Some(dep) => (work.blocked)(i, dep),
                        None => (work.build)(i, &tx),
                    };
                    finish(run, wake, dependants, work.ok, i, res);
                }
            }));
        }
        drop(tx);
        for ev in rx {
            on_event(ev);
        }
    });
    run.into_inner().unwrap().results
}

fn next<R>(run: &Mutex<Run<R>>, wake: &Condvar) -> Option<usize> {
    let mut g = run.lock().unwrap();
    loop {
        if let Some(i) = g.ready.pop_front() {
            return Some(i);
        }
        if g.remaining == 0 {
            return None;
        }
        g = wake.wait(g).unwrap();
    }
}

fn finish<R>(
    run: &Mutex<Run<R>>,
    wake: &Condvar,
    dependants: &[Vec<usize>],
    ok: &(dyn Fn(&R) -> bool + Sync),
    i: usize,
    res: R,
) {
    let mut g = run.lock().unwrap();
    let succeeded = ok(&res);
    g.results[i] = Some(res);
    g.remaining -= 1;
    for &d in &dependants[i] {
        if !succeeded {
            g.blocked[d] = Some(i);
        }
        g.waiting[d] -= 1;
        if g.waiting[d] == 0 {
            g.ready.push_back(d);
        }
    }
    wake.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::event::Log;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn jobs(deps: &[&[usize]]) -> Vec<Job> {
        deps.iter()
            .map(|d| Job { deps: d.to_vec() })
            .collect::<Vec<_>>()
    }

    /// The cap is the number of builds in flight, whatever the job count.
    #[test]
    fn at_most_limit_builds_run_at_once() {
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let build = |i: usize, tx: &Sender<Event>| -> bool {
            let n = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(n, Ordering::SeqCst);
            let _ = tx.send(Event::Log(Log {
                drv_path: format!("job{i}"),
                line: b"x".to_vec(),
            }));
            thread::sleep(Duration::from_millis(20));
            live.fetch_sub(1, Ordering::SeqCst);
            true
        };
        let work = Work {
            build: &build,
            ok: &|r: &bool| *r,
            blocked: &|_, _| false,
        };
        let mut seen = 0usize;
        let out = run(
            &jobs(&[&[], &[], &[], &[], &[], &[]]),
            2,
            &work,
            &mut |_| {
                seen += 1;
            },
        );
        assert_eq!(out.len(), 6);
        assert!(out.iter().all(|r| *r == Some(true)));
        assert_eq!(seen, 6);
        assert!(peak.load(Ordering::SeqCst) <= 2, "{peak:?}");
    }

    #[test]
    fn dependencies_run_first() {
        let order = Mutex::new(Vec::new());
        let build = |i: usize, _: &Sender<Event>| -> bool {
            order.lock().unwrap().push(i);
            true
        };
        let work = Work {
            build: &build,
            ok: &|r: &bool| *r,
            blocked: &|_, _| false,
        };
        // 2 needs 1, 1 needs 0.
        run(&jobs(&[&[], &[0], &[1]]), 4, &work, &mut |_| {});
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn a_failed_dependency_blocks_its_dependants() {
        let built = Mutex::new(Vec::new());
        let build = |i: usize, _: &Sender<Event>| -> Option<usize> {
            built.lock().unwrap().push(i);
            (i != 0).then_some(i)
        };
        let work = Work {
            build: &build,
            // A blocked job counts as failed, so the failure propagates.
            ok: &|r: &Option<usize>| matches!(r, Some(n) if *n < 1000),
            blocked: &|_, dep| Some(1000 + dep),
        };
        let out = run(&jobs(&[&[], &[0], &[1], &[]]), 4, &work, &mut |_| {});
        assert_eq!(out[0], Some(None));
        assert_eq!(out[1], Some(Some(1000)), "job 1 was blocked by job 0");
        assert_eq!(out[2], Some(Some(1001)), "job 2 was blocked by job 1");
        assert_eq!(out[3], Some(Some(3)), "an unrelated job still runs");
        let built = built.lock().unwrap();
        assert!(!built.contains(&1) && !built.contains(&2), "{built:?}");
    }
}
