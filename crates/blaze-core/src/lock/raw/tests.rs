//! The raw locks' tests. Between real threads: that a count kept under each lock comes out
//! whole. Under `RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-core --release model_raw`:
//! every interleaving of each protocol to the model's preemption bound, with the data the lock
//! guards tracked by address (a race on it is a finding), and each with its control, a planted
//! bug ([`Plant`]) the model must report.

#[cfg(not(blaze_check))]
mod threads {
    use std::{sync::Arc, thread};

    use super::super::{Cond, Mutex, RwLock};

    const THREADS: usize = 8;
    const EACH: usize = 10_000;

    fn spawn_all(work: impl Fn() + Send + Sync + 'static) {
        let work = Arc::new(work);
        let threads: Vec<_> = (0..THREADS)
            .map(|at| {
                let work = Arc::clone(&work);
                thread::Builder::new()
                    .name(format!("raw lock test {at}"))
                    .spawn(move || work())
                    .expect("a test thread")
            })
            .collect();
        for thread in threads {
            thread.join().expect("a test thread ends");
        }
    }

    /// Every increment under the mutex is kept: none is lost to two holders at once.
    #[test]
    fn a_mutex_keeps_every_increment() {
        let count = Arc::new(Mutex::new(0usize));
        let shared = Arc::clone(&count);
        spawn_all(move || {
            for _ in 0..EACH {
                *shared.lock() += 1;
            }
        });
        assert_eq!(*count.lock(), THREADS * EACH);
    }

    /// Writers keep every increment, and every reader sees the pair a writer left equal.
    #[test]
    fn a_reader_writer_lock_keeps_its_pairs_equal() {
        let pair = Arc::new(RwLock::new((0usize, 0usize)));
        let shared = Arc::clone(&pair);
        spawn_all(move || {
            for at in 0..EACH {
                if at % 4 == 0 {
                    let mut held = shared.write();
                    held.0 += 1;
                    held.1 += 1;
                } else {
                    let held = shared.read();
                    assert_eq!(held.0, held.1, "a read saw half a write");
                }
            }
        });
        let held = pair.read();
        assert_eq!(*held, (THREADS * EACH / 4, THREADS * EACH / 4));
    }

    /// A waiter on the condition sees each value a notifier sets under the lock, in turn.
    #[test]
    fn a_condition_hands_over_each_value() {
        let state = Arc::new((Mutex::new(0usize), Cond::new()));
        let theirs = Arc::clone(&state);
        let notifier = thread::Builder::new()
            .name("raw cond test".to_owned())
            .spawn(move || {
                let (lock, cond) = &*theirs;
                for _ in 0..EACH {
                    let mut held = lock.lock();
                    while *held % 2 == 1 {
                        cond.wait(&mut held, None);
                    }
                    *held += 1;
                    cond.notify(true);
                }
            })
            .expect("a test thread");
        let (lock, cond) = &*state;
        for _ in 0..EACH {
            let mut held = lock.lock();
            while *held % 2 == 0 {
                cond.wait(&mut held, None);
            }
            *held += 1;
            cond.notify(true);
        }
        notifier.join().expect("the notifier ends");
        assert_eq!(*lock.lock(), 2 * EACH);
    }

    /// A timed acquire of a lock held throughout gives up, and one of a free lock takes it.
    #[test]
    fn a_timed_acquire_gives_up_on_a_held_lock() {
        let lock = Mutex::new(());
        let held = lock.lock();
        assert!(lock.try_lock_for(std::time::Duration::from_millis(20)).is_none());
        drop(held);
        assert!(lock.try_lock_for(std::time::Duration::from_millis(20)).is_some());
    }
}

#[cfg(blaze_check)]
mod checked {
    use blaze_check::{
        cell, model,
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
        thread,
    };

    use super::super::{Mutex, Plant, RawMutex, RawRwLock};

    /// Three threads (the model's own and two) take one mutex in turn, each writing the byte it
    /// guards (tracked: two holders at once are a race) and counting itself: every thread gets
    /// through, once each. With [`Plant::ReleaseUnwoken`] a sleeper is never woken.
    fn mutex_hands_over(plant: Plant) {
        model(move || {
            let lock = Arc::new(RawMutex::new());
            let byte = Arc::new([0u8; 1]);
            let done = Arc::new(AtomicU32::new(0));
            let work = {
                let (lock, byte, done) = (lock.clone(), byte.clone(), done.clone());
                move || {
                    if !lock.try_lock() {
                        lock.lock_contended(None);
                    }
                    cell::write(byte.as_ptr(), 1);
                    done.fetch_add(1, Ordering::Relaxed);
                    // SAFETY: taken above.
                    unsafe { lock.unlock_with(plant) };
                }
            };
            let threads: Vec<_> = (0..2).map(|_| thread::spawn(work.clone())).collect();
            work();
            for thread in threads {
                thread.join().unwrap();
            }
            assert_eq!(done.load(Ordering::Relaxed), 3);
            assert!(!lock.is_locked(), "the mutex is held after every holder let go");
        });
    }

    #[test]
    fn model_raw_mutex_hands_over_to_every_waiter() {
        mutex_hands_over(Plant::None);
    }

    /// The control: a release that skips the wake of a contended word loses its sleeper.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn model_raw_mutex_release_unwoken_loses_a_waiter() {
        mutex_hands_over(Plant::ReleaseUnwoken);
    }

    /// A writer (the model's thread) and two readers: the writer writes the guarded bytes
    /// under the write (tracked: a reader inside it is a race) and every thread gets through.
    /// With [`Plant::ReadersUnwoken`] a reader that waited behind the write sleeps forever.
    fn rwlock_wakes_readers(plant: Plant) {
        model(move || {
            let lock = Arc::new(RawRwLock::new());
            let bytes = Arc::new([0u8; 2]);
            let done = Arc::new(AtomicU32::new(0));
            let readers: Vec<_> = (0..2)
                .map(|_| {
                    let (lock, bytes, done) = (lock.clone(), bytes.clone(), done.clone());
                    thread::spawn(move || {
                        lock.read();
                        cell::read(bytes.as_ptr(), 2);
                        // SAFETY: read above.
                        unsafe { lock.unlock_read() };
                        done.fetch_add(1, Ordering::Relaxed);
                    })
                })
                .collect();
            lock.write();
            cell::write(bytes.as_ptr(), 2);
            // SAFETY: written above.
            unsafe { lock.unlock_write_with(plant) };
            for reader in readers {
                reader.join().unwrap();
            }
            assert_eq!(done.load(Ordering::Relaxed), 2);
            assert!(!lock.is_locked(), "the lock is held after every holder let go");
        });
    }

    #[test]
    fn model_raw_rwlock_wakes_every_reader() {
        rwlock_wakes_readers(Plant::None);
    }

    /// The control: a write release that wakes no readers leaves one asleep.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn model_raw_rwlock_release_without_waking_readers_loses_one() {
        rwlock_wakes_readers(Plant::ReadersUnwoken);
    }

    /// Two writers and a reader: writers exclude each other and the reader, and the second
    /// writer (which waits on the notify word) is woken.
    #[test]
    fn model_raw_rwlock_wakes_a_waiting_writer() {
        model(|| {
            let lock = Arc::new(RawRwLock::new());
            let bytes = Arc::new([0u8; 1]);
            let other = {
                let (lock, bytes) = (lock.clone(), bytes.clone());
                thread::spawn(move || {
                    lock.write();
                    cell::write(bytes.as_ptr(), 1);
                    // SAFETY: written above.
                    unsafe { lock.unlock_write() };
                })
            };
            let reader = {
                let (lock, bytes) = (lock.clone(), bytes.clone());
                thread::spawn(move || {
                    lock.read();
                    cell::read(bytes.as_ptr(), 1);
                    // SAFETY: read above.
                    unsafe { lock.unlock_read() };
                })
            };
            lock.write();
            cell::write(bytes.as_ptr(), 1);
            // SAFETY: written above.
            unsafe { lock.unlock_write() };
            other.join().unwrap();
            reader.join().unwrap();
            assert!(!lock.is_locked());
        });
    }

    /// A waiter waits under the mutex for a flag the other thread sets under it and notifies:
    /// it always wakes. With [`Plant::SequenceUnmoved`] a notify between the waiter's release
    /// of the lock and its sleep is slept through.
    fn cond_wakes_its_waiter(plant: Plant) {
        model(move || {
            let state = Arc::new((Mutex::new(false), super::super::Cond::new()));
            let setter = {
                let state = state.clone();
                thread::spawn(move || {
                    let (lock, cond) = &*state;
                    *lock.lock() = true;
                    cond.notify_with(false, plant);
                })
            };
            let (lock, cond) = &*state;
            let mut held = lock.lock();
            while !*held {
                cond.wait(&mut held, None);
            }
            drop(held);
            setter.join().unwrap();
        });
    }

    #[test]
    fn model_raw_cond_wakes_its_waiter() {
        cond_wakes_its_waiter(Plant::None);
    }

    /// The control: a notify that does not move the sequence is lost to a waiter not yet
    /// asleep.
    #[test]
    #[should_panic(expected = "deadlock")]
    fn model_raw_cond_notify_without_moving_the_sequence_is_lost() {
        cond_wakes_its_waiter(Plant::SequenceUnmoved);
    }
}
