//! [`Claims`](super::Claims) from many threads, and both protocols under the model checker.

#[cfg(not(blaze_check))]
mod threads {
    use std::sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    };

    use super::super::Claims;

    /// Eight threads over a thousand chunks: each claimed once, one finisher, who sees them all.
    #[test]
    fn every_chunk_is_claimed_once_and_one_thread_finishes() {
        const CHUNKS: usize = 1000;
        let claims = Arc::new(Claims::new(CHUNKS));
        let taken: Arc<Vec<AtomicU8>> = Arc::new((0..CHUNKS).map(|_| AtomicU8::new(0)).collect());
        let last = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (claims, taken, last) =
                    (Arc::clone(&claims), Arc::clone(&taken), Arc::clone(&last));
                std::thread::spawn(move || {
                    while let Some(chunk) = claims.claim() {
                        taken[chunk].fetch_add(1, Ordering::Relaxed);
                        if claims.finish() {
                            assert!(taken.iter().all(|it| it.load(Ordering::Relaxed) == 1));
                            last.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(taken.iter().all(|it| it.load(Ordering::Relaxed) == 1));
        assert_eq!(last.load(Ordering::Relaxed), 1);
        assert_eq!(claims.claim(), None);
    }
}

/// [`Wake`](super::Wake)'s theorem where it is stated: drawn schedules in the weak memory, and
/// its control.
#[cfg(not(blaze_check))]
mod theorems {
    use std::alloc::Global;

    use blaze_sim::{Broke, Config, Tape, check};

    use super::super::{hands, no_hand_back_is_missed};
    use crate::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        world::threads::{self, Ended, Step},
    };

    /// A schedule drawn: up to eight steps (each thread has two operations), picks 0 to 3:
    /// in the weak memory a pick says how far back a read reads.
    fn drawn(tape: &Tape) -> Vec<Step> {
        let mut schedule = Vec::new_in(Global);
        for _ in 0..tape.int_in(0..=8u64, "steps") {
            schedule.push(Step { second: tape.coin("the driver goes"), pick: tape.int_in(0..=3u8, "pick") });
        }
        schedule
    }

    /// The same two threads with the flag's swaps `Relaxed`: the planted weakening.
    fn hands_relaxed(schedule: &[Step], flagged: bool) -> bool {
        let woken = AtomicBool::new(flagged);
        let slot = AtomicU32::new(0);
        let (sent, drained) = threads::two_weak(
            schedule,
            || {
                slot.store(1, Ordering::Relaxed);
                !woken.swap(true, Ordering::Relaxed)
            },
            || {
                woken.swap(false, Ordering::Relaxed);
                slot.load(Ordering::Relaxed)
            },
        );
        match (sent, drained) {
            (Ended::Done(must), Ended::Done(seen)) => seen == 1 || must,
            _ => true,
        }
    }

    /// The theorem's function holds on drawn schedules, picks and flags, reached with both
    /// orders of the two swaps.
    #[test]
    fn no_hand_back_is_missed_where_it_is_run() {
        let config = Config {
            must_reach: &["the sender swapped first", "the driver swapped first"],
            ..Config::cases(2_000)
        };
        check(config, |tape| {
            let schedule = drawn(tape);
            let flagged = tape.coin("the flag is set");
            // Which swap came first: each thread's second step for the sender, first for the
            // driver.
            let (mut sent, mut order) = (0, None);
            for step in &schedule {
                match step.second {
                    false => {
                        sent += 1;
                        if sent == 2 && order.is_none() {
                            order = Some("the sender swapped first");
                        }
                    }
                    true => {
                        if order.is_none() {
                            order = Some("the driver swapped first");
                        }
                    }
                }
            }
            if let Some(order) = order {
                tape.reached(order);
            }
            match no_hand_back_is_missed(&schedule, flagged) {
                true => Ok(()),
                false => Err(Broke::Oracle(format!(
                    "a hand-back was missed: schedule {schedule:?}, the flag set {flagged}"
                ))),
            }
        });
    }

    /// The planted weakening on the run that shows it: the flag is set, the sender stores and
    /// swaps (told not to wake: the flag was set), the driver swaps without acquiring and its
    /// read of the slot, one back, sees the slot before the hand-back.
    #[test]
    fn the_oracle_catches_a_flag_swapped_relaxed() {
        let s = |pick| Step { second: false, pick };
        let d = |pick| Step { second: true, pick };
        let schedule = [s(0), s(0), d(0), d(1)];
        assert!(!hands_relaxed(&schedule, true));
        assert!(hands(&schedule, true));
    }
}

/// Under `RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-core --release model_`.
#[cfg(blaze_check)]
mod checked {
    use blaze_check::{
        cell, model,
        sync::{
            Arc,
            atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering},
        },
        thread,
    };

    use super::super::{Claims, Wake};

    /// Two threads beside the model's own over three chunks: each chunk is claimed by one, its
    /// bytes written (tracked) and then finished; exactly one finisher, and its read of every
    /// chunk's bytes is ordered after each write (a race is a finding), seeing each value.
    #[test]
    fn model_the_last_finisher_sees_every_chunk() {
        const CHUNKS: usize = 3;
        model(|| {
            let claims = Arc::new(Claims::new(CHUNKS));
            // The bytes the chunks write, tracked by address; the values beside them.
            let bytes = Arc::new([0u8; CHUNKS]);
            let values: Arc<[AtomicU32; CHUNKS]> = Arc::new([const { AtomicU32::new(0) }; CHUNKS]);
            let taken: Arc<[AtomicU8; CHUNKS]> = Arc::new([const { AtomicU8::new(0) }; CHUNKS]);
            let last = Arc::new(AtomicUsize::new(0));
            let work = {
                let (claims, bytes, values, taken, last) =
                    (claims.clone(), bytes.clone(), values.clone(), taken.clone(), last.clone());
                move || {
                    while let Some(chunk) = claims.claim() {
                        taken[chunk].fetch_add(1, Ordering::Relaxed);
                        cell::write(bytes.as_ptr().wrapping_add(chunk), 1);
                        values[chunk].store(chunk as u32 + 1, Ordering::Relaxed);
                        if claims.finish() {
                            cell::read(bytes.as_ptr(), CHUNKS);
                            for (at, value) in values.iter().enumerate() {
                                assert_eq!(
                                    value.load(Ordering::Relaxed),
                                    at as u32 + 1,
                                    "chunk {at} unseen"
                                );
                            }
                            last.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            };
            let threads: Vec<_> = (0..2).map(|_| thread::spawn(work.clone())).collect();
            work();
            for thread in threads {
                thread.join().unwrap();
            }
            for (at, taken) in taken.iter().enumerate() {
                assert_eq!(taken.load(Ordering::Relaxed), 1, "chunk {at} claimed other than once");
            }
            assert_eq!(last.load(Ordering::Relaxed), 1, "the job completed other than once");
        });
    }

    /// Two senders hand back (a relaxed store of their slot, so only `Wake` orders it) and wake
    /// the driver, which waits for a wake-up (a counter standing for the port's packets), drains
    /// and reads the slots, until it has both. A lost wake-up leaves it waiting with a hand-back
    /// unread: the model reports the endless wait. At most one wake-up per hand-back.
    #[test]
    fn model_no_hand_back_waits_without_a_wake_up() {
        const SENDERS: usize = 2;
        model(|| {
            let wake = Arc::new(Wake::new());
            let slots: Arc<[AtomicU8; SENDERS]> = Arc::new([const { AtomicU8::new(0) }; SENDERS]);
            let posted = Arc::new(AtomicUsize::new(0));
            let senders: Vec<_> = (0..SENDERS)
                .map(|at| {
                    let (wake, slots, posted) = (wake.clone(), slots.clone(), posted.clone());
                    thread::spawn(move || {
                        slots[at].store(1, Ordering::Relaxed);
                        if wake.wake() {
                            posted.fetch_add(1, Ordering::Release);
                        }
                    })
                })
                .collect();
            let (mut seen, mut taken) = (0, 0);
            while seen < SENDERS {
                while posted.load(Ordering::Acquire) == taken {
                    thread::yield_now();
                }
                taken += 1;
                wake.drain();
                seen = slots.iter().filter(|it| it.load(Ordering::Relaxed) == 1).count();
            }
            for sender in senders {
                sender.join().unwrap();
            }
            assert!(posted.load(Ordering::Relaxed) <= SENDERS);
        });
    }
}
