//! The pool's park and wake under Shuttle (src/park.rs, compiled here
//! beside Shuttle's primitives, so what is checked is the code the pool
//! runs; a copy would drift). Run it so:
//!
//!     RUSTFLAGS="--cfg shuttle" cargo test --release --test shuttle_park
//!
//! Without the cfg it compiles to nothing. Shuttle explores schedules under
//! sequential consistency, which SeqCst gives on every target the fork
//! builds for, and models park and unpark; a lost wake (a worker parked
//! with work published and no one to wake it) is a deadlock it reports,
//! and a schedule it finds is reproducible from its seed.
//!
//! The model: a source is one counter of tasks published; workers take
//! tasks by decrementing it, and park through `park_unless` when none is
//! left; publishers add tasks and, as the pool's publishers do, wake a
//! parked worker when any is parked. Every published task is taken.
#![cfg(shuttle)]

/// Shuttle's primitives, under the names src/park.rs imports them by.
mod park_sync {
    pub use shuttle::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    pub use shuttle::thread;

    /// A once-set cell, as std's OnceLock (which Shuttle lacks): set once,
    /// read many; behind Shuttle's Mutex so the model sees the handoff.
    pub struct OnceLock<T>(shuttle::sync::Mutex<Option<T>>);

    impl<T: Clone> OnceLock<T> {
        pub fn new() -> Self {
            OnceLock(shuttle::sync::Mutex::new(None))
        }
        pub fn set(&self, value: T) -> Result<(), T> {
            let mut slot = self.0.lock().unwrap();
            if slot.is_some() {
                return Err(value);
            }
            *slot = Some(value);
            Ok(())
        }
        pub fn get(&self) -> Option<T> {
            self.0.lock().unwrap().clone()
        }
    }
}

#[path = "../src/park.rs"]
#[allow(dead_code)]
mod park;

use park::{FANOUT, Park};
use shuttle::sync::Arc;
use shuttle::sync::atomic::{AtomicUsize, Ordering};
use shuttle::thread;

/// `workers` workers and `publishers` publishers of `each` tasks: every
/// task is taken, on every schedule Shuttle tries.
fn run(workers: usize, publishers: usize, each: usize) {
    let total = publishers * each;
    shuttle::check_pct(
        move || {
            let park = Arc::new(Park::new(workers, FANOUT));
            let published = Arc::new(AtomicUsize::new(0));
            let taken = Arc::new(AtomicUsize::new(0));
            let mut threads = Vec::new();
            for me in 0..workers {
                let (park, published, taken) = (park.clone(), published.clone(), taken.clone());
                threads.push(thread::spawn(move || {
                    park.register(me);
                    loop {
                        // Take one task if any is published.
                        let p = published.load(Ordering::SeqCst);
                        let t = taken.load(Ordering::SeqCst);
                        if t < p {
                            if taken.compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::SeqCst).is_ok() && t + 1 == total {
                                park.wake_one(); // the last task: let the rest leave too
                            }
                            if t + 1 == total {
                                return;
                            }
                            continue;
                        }
                        if taken.load(Ordering::SeqCst) == total {
                            return;
                        }
                        // None: park unless work shows, as the pool's worker does.
                        let visible = || taken.load(Ordering::SeqCst) < published.load(Ordering::SeqCst) || taken.load(Ordering::SeqCst) == total;
                        if park.park_unless(me, visible) {
                            park.pay_owed();
                        }
                    }
                }));
            }
            for _ in 0..publishers {
                let (park, published) = (park.clone(), published.clone());
                threads.push(thread::spawn(move || {
                    for _ in 0..each {
                        published.fetch_add(1, Ordering::SeqCst);
                        // As the pool's publishers: a wake, if any worker is parked.
                        park.wake_one();
                    }
                }));
            }
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(taken.load(Ordering::SeqCst), total);
        },
        3000,
        3,
    );
}

#[test]
fn one_worker_one_publisher() {
    run(1, 1, 2);
}

#[test]
fn two_workers_one_publisher() {
    run(2, 1, 3);
}

#[test]
fn two_workers_two_publishers() {
    run(2, 2, 2);
}

/// wake_for owes wakes that cascade: with three workers parked, three
/// pieces published at once, and a fan-out of one, the publisher wakes one,
/// each woken worker the next, and every worker is woken.
#[test]
fn owed_wakes_cascade() {
    shuttle::check_pct(
        || {
            let park = Arc::new(Park::new(3, 1));
            let published = Arc::new(AtomicUsize::new(0));
            let woken = Arc::new(AtomicUsize::new(0));
            let mut threads = Vec::new();
            for me in 0..3 {
                let (park, published, woken) = (park.clone(), published.clone(), woken.clone());
                threads.push(thread::spawn(move || {
                    park.register(me);
                    if park.park_unless(me, || published.load(Ordering::SeqCst) > 0) {
                        park.pay_owed();
                    }
                    woken.fetch_add(1, Ordering::SeqCst);
                }));
            }
            let (p, pub2) = (park.clone(), published.clone());
            threads.push(thread::spawn(move || {
                pub2.store(3, Ordering::SeqCst);
                p.wake_for(3, 3);
            }));
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(woken.load(Ordering::SeqCst), 3);
        },
        3000,
        3,
    );
}

/// Two wakers at once, two workers parked: each waker's wake lands on a
/// different worker (a waker that counts a bit another cleared as its own
/// leaves one worker parked, and this deadlocks).
#[test]
fn two_wakers_wake_two_workers() {
    shuttle::check_pct(
        || {
            let park = Arc::new(Park::new(2, FANOUT));
            let go = Arc::new(AtomicUsize::new(0));
            let mut threads = Vec::new();
            for me in 0..2 {
                let (park, go) = (park.clone(), go.clone());
                threads.push(thread::spawn(move || {
                    park.register(me);
                    if park.park_unless(me, || go.load(Ordering::SeqCst) == 2) {
                        park.pay_owed();
                    }
                }));
            }
            for _ in 0..2 {
                let (park, go) = (park.clone(), go.clone());
                threads.push(thread::spawn(move || {
                    go.fetch_add(1, Ordering::SeqCst);
                    while !park.wake_one() {
                        if go.load(Ordering::SeqCst) == 2 && park.parked_count() == 0 {
                            return;
                        }
                        thread::yield_now();
                    }
                }));
            }
            for t in threads {
                t.join().unwrap();
            }
        },
        3000,
        3,
    );
}

/// The wakes spread as a tree: five workers parked, then five pieces
/// published, a fan-out of two. Every worker is woken, no waker (the
/// publisher or a woken worker) makes more than two wakes, and the wakes
/// they report add up to the five made.
#[test]
fn wakes_fan_out_no_waker_makes_more_than_its_share() {
    shuttle::check_pct(
        || {
            let park = Arc::new(Park::new(5, 2));
            let woken = Arc::new(AtomicUsize::new(0));
            let most = Arc::new(AtomicUsize::new(0));
            let made = Arc::new(AtomicUsize::new(0));
            let mut threads = Vec::new();
            for me in 0..5 {
                let (park, woken, most, made) = (park.clone(), woken.clone(), most.clone(), made.clone());
                threads.push(thread::spawn(move || {
                    park.register(me);
                    // Woken, never by finding the work: the handshake that
                    // passes a wake on is the other tests'; here every wake
                    // is one a waker counts.
                    if park.park_unless(me, || false) {
                        let paid = park.pay_owed();
                        most.fetch_max(paid, Ordering::SeqCst);
                        made.fetch_add(paid, Ordering::SeqCst);
                    }
                    woken.fetch_add(1, Ordering::SeqCst);
                }));
            }
            let (p, most2, made2) = (park.clone(), most.clone(), made.clone());
            threads.push(thread::spawn(move || {
                // All five bits set first, so the publisher counts every
                // worker parked and wakes for all five.
                while p.parked_count() < 5 {
                    thread::yield_now();
                }
                let direct = p.wake_for(5, 5);
                most2.fetch_max(direct, Ordering::SeqCst);
                made2.fetch_add(direct, Ordering::SeqCst);
            }));
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(woken.load(Ordering::SeqCst), 5, "every worker woken");
            assert!(most.load(Ordering::SeqCst) <= 2, "no waker makes more than its fan-out");
            // Each wake counted once, by the waker that made it: the counts
            // the share bound reads are the wakes made.
            assert_eq!(
                made.load(Ordering::SeqCst),
                5,
                "every wake accounted for (most {}, woken {})",
                most.load(Ordering::SeqCst),
                woken.load(Ordering::SeqCst)
            );
        },
        3000,
        3,
    );
}
