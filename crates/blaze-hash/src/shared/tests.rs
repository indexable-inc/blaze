//! The shared table on real threads (every value met once, every entry dropped once), and
//! under `cfg(blaze_check)` against the model checker, each with a planted bug it must catch.

#[cfg(not(blaze_check))]
mod threads {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        vec::Vec,
    };

    use crate::SharedMap;

    const THREADS: usize = 8;
    const EACH: usize = 5_000;

    fn spawn<R: Send + 'static>(
        name: &str,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> thread::JoinHandle<R> {
        thread::Builder::new().name(name.into()).spawn(work).expect("a test thread")
    }

    /// Counts its drops, so a test sees every value dropped exactly once.
    struct Counted(usize, Arc<AtomicUsize>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn every_key_is_found_after_its_insert_and_gone_after_its_remove() {
        let map: Arc<SharedMap<usize, usize>> = Arc::new(SharedMap::default());
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let map = Arc::clone(&map);
                spawn("shared map test writer", move || {
                    let pinned = map.pin();
                    for i in 0..EACH {
                        let key = t * EACH + i;
                        assert_eq!(pinned.insert(key, key * 2), None);
                        assert_eq!(pinned.get(&key), Some(&(key * 2)));
                    }
                    for i in (0..EACH).step_by(2) {
                        let key = t * EACH + i;
                        assert_eq!(pinned.remove(&key), Some(&(key * 2)));
                        assert_eq!(pinned.get(&key), None);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("a writer ended");
        }
        let pinned = map.pin();
        assert_eq!(map.len(), THREADS * EACH / 2);
        let mut seen: Vec<usize> = pinned.iter().map(|(key, value)| {
            assert_eq!(*value, key * 2);
            *key
        }).collect();
        seen.sort_unstable();
        let want: Vec<usize> = (0..THREADS * EACH).filter(|key| key % EACH % 2 == 1).collect();
        assert_eq!(seen, want);
    }

    #[test]
    fn every_value_is_dropped_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let made;
        {
            let map: SharedMap<usize, Counted> = SharedMap::default();
            let pinned = map.pin();
            for i in 0..1_000 {
                pinned.insert(i, Counted(i, Arc::clone(&drops)));
            }
            // Replaced, removed, kept by try_insert, cleared: each value dropped once.
            for i in 0..500 {
                pinned.insert(i, Counted(i, Arc::clone(&drops)));
            }
            for i in 0..100 {
                pinned.remove(&(i * 3));
            }
            assert!(pinned.try_insert(999, Counted(999, Arc::clone(&drops))).is_err());
            pinned.retain(|key, value| key % 2 == 0 && value.0 == *key);
            made = 1_000 + 500 + 1;
            drop(pinned);
        }
        assert_eq!(drops.load(Ordering::Relaxed), made);
    }

    #[test]
    fn get_or_insert_gives_every_thread_the_first_value() {
        let map: Arc<SharedMap<u32, usize>> = Arc::new(SharedMap::default());
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let map = Arc::clone(&map);
                spawn("shared map test racer", move || *map.pin().get_or_insert(7, t))
            })
            .collect();
        let got: Vec<usize> = workers.into_iter().map(|it| it.join().expect("a racer ended")).collect();
        assert!(got.iter().all(|it| *it == got[0]), "{got:?}");
        assert_eq!(map.pin().get(&7), Some(&got[0]));
    }
}

#[cfg(blaze_check)]
mod checked {
    use blaze_check::{model, sync::Arc, thread};

    use crate::{SharedMap, shared::Plant};

    /// A writer replaces a key a reader reads: the reader meets the old value or the new,
    /// whole. Relaxed publish: the reader's read of the new entry is unordered with its write.
    /// Freed at once: the reader reads an entry freed under it.
    fn replace_under_a_reader(plant: Plant) {
        model(move || {
            let map: Arc<SharedMap<u32, u64>> = Arc::new(SharedMap::default());
            map.pin().insert(1, 10);
            let writer = {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    map.pin().insert_planted(1, 20, plant);
                })
            };
            let pinned = map.pin();
            let lent = pinned.get(&1).expect("the key is always held");
            // The reader's use of the lent value, later than its lookup: what a free under the
            // pin would leave it reading.
            blaze_check::cell::read(core::ptr::from_ref(lent).cast(), size_of::<u64>());
            let seen = *lent;
            drop(pinned);
            assert!(seen == 10 || seen == 20, "{seen}");
            writer.join().expect("the writer ended");
            assert_eq!(map.pin().get(&1), Some(&20));
        });
    }

    #[test]
    fn model_shared_map_replace_under_a_reader() {
        replace_under_a_reader(Plant::None);
    }

    #[test]
    #[should_panic(expected = "data race")]
    fn model_shared_map_planted_relaxed_publish() {
        replace_under_a_reader(Plant::RelaxedPublish);
    }

    #[test]
    #[should_panic(expected = "free")]
    fn model_shared_map_planted_free_at_once() {
        replace_under_a_reader(Plant::FreeAtOnce);
    }

    /// Two writers insert into one shard while it grows from empty: both keys are found.
    #[test]
    fn model_shared_map_grows_under_two_writers() {
        model(|| {
            let map: Arc<SharedMap<u32, u32>> = Arc::new(SharedMap::default());
            let other = {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    map.pin().insert(1, 1);
                })
            };
            map.pin().insert(2, 2);
            let found = map.pin().get(&1).copied();
            other.join().expect("the other writer ended");
            assert!(found.is_none() || found == Some(1));
            assert_eq!(map.pin().get(&1), Some(&1));
            assert_eq!(map.pin().get(&2), Some(&2));
        });
    }
}
