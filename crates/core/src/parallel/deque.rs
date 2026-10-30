//! Fixed-capacity Chase-Lev work-stealing deque of two-word entries.
//! Owner pushes and pops at the bottom (LIFO); thieves steal from the top.
//! `push` fails when full, so an entry is never overwritten while a thief
//! may still read it, and no memory reclamation is needed.
//! Orderings follow Lê et al., "Correct and Efficient Work-Stealing for
//! Weak Memory Models" (PPoPP 2013).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering, fence};

#[repr(align(128))]
struct Padded<T>(T);

pub struct Deque {
    top: Padded<AtomicI64>,
    bottom: Padded<AtomicI64>,
    cap: usize,
    buf: Box<[AtomicU64]>,
}

// SAFETY: every slot is an atomic; the top/bottom protocol keeps a thief
// from reading a slot that the owner may be writing (see `push`).
unsafe impl Sync for Deque {}

impl Deque {
    pub fn new(cap: usize) -> Deque {
        assert!(cap.is_power_of_two());
        Deque {
            top: Padded(AtomicI64::new(0)),
            bottom: Padded(AtomicI64::new(0)),
            cap,
            buf: (0..cap * 2).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    #[inline]
    fn slot(&self, i: i64) -> usize {
        ((i as usize) & (self.cap - 1)) * 2
    }

    /// Owner only. Returns false when full.
    pub fn push(&self, a: u64, b: u64) -> bool {
        let bot = self.bottom.0.load(Ordering::Relaxed);
        let top = self.top.0.load(Ordering::Acquire);
        if bot - top >= self.cap as i64 {
            return false;
        }
        let s = self.slot(bot);
        self.buf[s].store(a, Ordering::Relaxed);
        self.buf[s + 1].store(b, Ordering::Relaxed);
        fence(Ordering::Release);
        self.bottom.0.store(bot + 1, Ordering::Relaxed);
        true
    }

    /// Owner only.
    pub fn pop(&self) -> Option<(u64, u64)> {
        let bot = self.bottom.0.load(Ordering::Relaxed) - 1;
        self.bottom.0.store(bot, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        let top = self.top.0.load(Ordering::Relaxed);
        if top <= bot {
            let s = self.slot(bot);
            let a = self.buf[s].load(Ordering::Relaxed);
            let b = self.buf[s + 1].load(Ordering::Relaxed);
            if top == bot {
                let won = self
                    .top
                    .0
                    .compare_exchange(top, top + 1, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok();
                self.bottom.0.store(bot + 1, Ordering::Relaxed);
                if won { Some((a, b)) } else { None }
            } else {
                Some((a, b))
            }
        } else {
            self.bottom.0.store(bot + 1, Ordering::Relaxed);
            None
        }
    }

    /// Any thread. `None` means empty or lost a race; callers retry or
    /// move on.
    pub fn steal(&self) -> Option<(u64, u64)> {
        let top = self.top.0.load(Ordering::Acquire);
        fence(Ordering::SeqCst);
        let bot = self.bottom.0.load(Ordering::Acquire);
        if top < bot {
            let s = self.slot(top);
            let a = self.buf[s].load(Ordering::Relaxed);
            let b = self.buf[s + 1].load(Ordering::Relaxed);
            if self
                .top
                .0
                .compare_exchange(top, top + 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return Some((a, b));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::thread;

    #[test]
    fn lifo_owner_fifo_thief() {
        let d = Deque::new(8);
        for i in 0..8 {
            assert!(d.push(i, 100 + i));
        }
        assert!(!d.push(9, 9));
        assert_eq!(d.steal(), Some((0, 100)));
        assert_eq!(d.pop(), Some((7, 107)));
        assert!(d.push(8, 108));
        assert_eq!(d.pop(), Some((8, 108)));
        for i in (1..7).rev() {
            assert_eq!(d.pop(), Some((i, 100 + i)));
        }
        assert_eq!(d.pop(), None);
        assert_eq!(d.steal(), None);
    }

    #[test]
    fn concurrent_sum() {
        let d = Arc::new(Deque::new(1024));
        let sum = Arc::new(AtomicU64::new(0));
        let taken = Arc::new(AtomicU64::new(0));
        const N: u64 = 200_000;
        let thieves: Vec<_> = (0..4)
            .map(|_| {
                let d = d.clone();
                let sum = sum.clone();
                let taken = taken.clone();
                thread::spawn(move || {
                    while taken.load(Ordering::Relaxed) < N {
                        if let Some((a, b)) = d.steal() {
                            assert_eq!(b, a * 2);
                            sum.fetch_add(a, Ordering::Relaxed);
                            taken.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        let mut i = 0;
        while i < N {
            if d.push(i, i * 2) {
                i += 1;
            } else if let Some((a, b)) = d.pop() {
                assert_eq!(b, a * 2);
                sum.fetch_add(a, Ordering::Relaxed);
                taken.fetch_add(1, Ordering::Relaxed);
            }
        }
        while taken.load(Ordering::Relaxed) < N {
            if let Some((a, _)) = d.pop() {
                sum.fetch_add(a, Ordering::Relaxed);
                taken.fetch_add(1, Ordering::Relaxed);
            }
        }
        for t in thieves {
            t.join().unwrap();
        }
        assert_eq!(sum.load(Ordering::Relaxed), N * (N - 1) / 2);
    }
}
