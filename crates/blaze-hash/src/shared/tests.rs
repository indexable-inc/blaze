//! The shared table on real threads (every value met once, every entry dropped once), and
//! under `cfg(blaze_check)` against the model checker, each with a planted bug it must catch.

use super::ShardLock;

/// The tests' shard lock (D64): std's on real threads, blaze-check's under the model, as
/// blaze-core (above this crate) gives the build its named lock.
pub(super) enum TestLock {}

#[cfg(not(blaze_check))]
#[expect(
    clippy::disallowed_types,
    reason = "a test below blaze-core, whose named lock gives the build's instantiation"
)]
// SAFETY: std's mutex: one guard at a time, each lock's writes before its next guard.
unsafe impl ShardLock for TestLock {
    type Lock<T> = std::sync::Mutex<T>;
    type Guard<'a, T: 'a> = std::sync::MutexGuard<'a, T>;

    fn new<T>(_name: &'static str, value: T) -> std::sync::Mutex<T> {
        std::sync::Mutex::new(value)
    }

    fn lock<T>(lock: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(blaze_check)]
// SAFETY: the model checker's mutex, which it explores as a mutex.
unsafe impl ShardLock for TestLock {
    type Lock<T> = blaze_check::sync::Mutex<T>;
    type Guard<'a, T: 'a> = blaze_check::sync::MutexGuard<'a, T>;

    fn new<T>(_name: &'static str, value: T) -> blaze_check::sync::Mutex<T> {
        blaze_check::sync::Mutex::new(value)
    }

    fn lock<T>(lock: &blaze_check::sync::Mutex<T>) -> blaze_check::sync::MutexGuard<'_, T> {
        lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

type SharedMap<K, V, A> = crate::SharedMap<K, V, A, TestLock>;
#[cfg(not(blaze_check))]
type SharedSet<K, A> = crate::SharedSet<K, A, TestLock>;

#[cfg(not(blaze_check))]
mod threads {
    use std::{
        alloc::Global,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        vec::Vec,
    };

    use super::SharedMap;

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

    /// D126: `new(MapOpts)` makes what `new_in` made: with `capacity: None` no shard has a
    /// table until its first insert, with `Some(n)` every shard has one now; after the same
    /// inserts either holds what `new_in`'s holds. A set likewise.
    #[test]
    fn new_with_options_is_new_in() {
        use blaze_sim::Config;

        use crate::{FixedState, SharedMapOpts, SharedSetOpts};

        blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 128 }), |tape| {
            let room = if tape.coin("reserve") { Some(tape.index(2_000, "capacity")) } else { None };
            let new: SharedMap<u64, u64, Global> = SharedMap::new(SharedMapOpts {
                name: "test.new",
                hasher: FixedState,
                alloc: Global,
                capacity: room,
            });
            let made_now = new.shards.iter().all(|shard| !shard.table.load(Ordering::Relaxed).is_null());
            let none_now = new.shards.iter().all(|shard| shard.table.load(Ordering::Relaxed).is_null());
            match room {
                None | Some(0) => assert!(none_now, "no table before the first insert"),
                Some(_) => assert!(made_now, "every shard's table made for {room:?}"),
            }
            let old: SharedMap<u64, u64, Global> = SharedMap::new_in("test.old", Global);
            let set: super::SharedSet<u64, Global> = super::SharedSet::new(SharedSetOpts {
                name: "test.set",
                hasher: FixedState,
                alloc: Global,
                capacity: room,
            });
            let (pinned_new, pinned_old, pinned_set) = (new.pin(), old.pin(), set.pin());
            let range = 1 + tape.below(3_000, "range");
            for step in 0..tape.below(if cfg!(miri) { 40 } else { 600 }, "inserts") {
                let key = tape.below(range, "key");
                let (a, b) = (pinned_new.insert(key, step).copied(), pinned_old.insert(key, step).copied());
                assert_eq!(a, b, "key {key}");
                pinned_set.insert(key);
            }
            assert_eq!(new.len(), old.len());
            assert_eq!(set.len(), old.len());
            for key in 0..range {
                assert_eq!(pinned_new.get(&key), pinned_old.get(&key), "key {key}");
                assert_eq!(pinned_set.get(&key).is_some(), pinned_old.get(&key).is_some(), "set key {key}");
            }
        });
    }

    /// `get_many` and `get_many_into` answer what `get` answers of each key, duplicates,
    /// absent and removed keys among them, past the slice form's batch of 16; a set's too.
    #[test]
    fn get_many_is_get_of_each() {
        use blaze_sim::Config;

        blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 256 }), |tape| {
            let map: SharedMap<u64, u64, Global> = SharedMap::new_in("test.map", Global);
            let set: super::SharedSet<u64, Global> = super::SharedSet::new_in("test.set", Global);
            let (pinned, pinned_set) = (map.pin(), set.pin());
            let range = 1 + tape.below(300, "range");
            for _ in 0..tape.below(if cfg!(miri) { 40 } else { 300 }, "held") {
                let key = tape.below(range, "key");
                if tape.one_in(5, "remove") {
                    pinned.remove(&key);
                    pinned_set.remove(&key);
                } else {
                    pinned.insert(key, key * 3);
                    pinned_set.insert(key);
                }
            }
            let mut asked: Vec<u64> =
                (0..1 + tape.index(40, "asked")).map(|_| tape.below(2 * range, "ask")).collect();
            let to = tape.index(asked.len(), "duplicate to");
            asked[to] = asked[tape.index(asked.len(), "duplicate from")];

            let mut out = std::vec![None; asked.len()];
            pinned.get_many_into(&asked, &mut out);
            for (at, key) in asked.iter().enumerate() {
                assert_eq!(out[at], pinned.get(key), "slice: key {key} at {at} of {asked:?}");
            }
            let first: [&u64; 1] = [&asked[0]];
            assert_eq!(pinned.get_many(first), [pinned.get(&asked[0])]);
            if let Ok(eight) = <[u64; 8]>::try_from(&asked[..asked.len().min(8)]) {
                let many = pinned.get_many(eight.each_ref());
                let keys = pinned_set.get_many(eight.each_ref());
                for (at, key) in eight.iter().enumerate() {
                    assert_eq!(many[at], pinned.get(key), "array: key {key} of {eight:?}");
                    assert_eq!(keys[at], pinned_set.get(key), "set: key {key} of {eight:?}");
                }
            }
        });
    }

    /// The shared map's `get_or_insert_with` on a tape of `str` keys is `get`, then `insert` on
    /// a miss; `make` runs once per key put and never on a hit.
    #[test]
    fn get_or_insert_with_is_get_then_insert() {
        use blaze_sim::Config;

        blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 256 }), |tape| {
            let map: SharedMap<std::string::String, u64, Global> = SharedMap::new_in("test.map", Global);
            let pinned = map.pin();
            let mut model = std::collections::BTreeMap::new();
            let (mut made, mut put) = (0usize, 0usize);
            let range = 1 + tape.below(100, "range");
            for step in 0..tape.below(if cfg!(miri) { 60 } else { 400 }, "steps") {
                let name = std::format!("k{}", tape.below(range, "key"));
                if tape.one_in(6, "remove") {
                    assert_eq!(pinned.remove(name.as_str()).copied(), model.remove(&name));
                    continue;
                }
                let expected = *model.entry(name.clone()).or_insert_with(|| {
                    put += 1;
                    step
                });
                let got = pinned.get_or_insert_with(name.as_str(), |key| {
                    made += 1;
                    (std::string::String::from(key), step)
                });
                assert_eq!(*got, expected, "{name} at step {step}");
            }
            assert_eq!(made, put, "`make` once per key put, never on a hit");
        });
    }

    #[test]
    fn every_key_is_found_after_its_insert_and_gone_after_its_remove() {
        let map: Arc<SharedMap<usize, usize, Global>> = Arc::new(SharedMap::new_in("test.map", Global));
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
            let map: SharedMap<usize, Counted, Global> = SharedMap::new_in("test.map", Global);
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
        let map: Arc<SharedMap<u32, usize, Global>> = Arc::new(SharedMap::new_in("test.map", Global));
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
    use std::alloc::Global;

    use blaze_check::{model, sync::Arc, thread};

    use super::SharedMap;
    use crate::shared::Plant;

    /// A writer replaces a key a reader reads: the reader meets the old value or the new,
    /// whole. Relaxed publish: the reader's read of the new entry is unordered with its write.
    /// Freed at once: the reader reads an entry freed under it.
    fn replace_under_a_reader(plant: Plant) {
        model(move || {
            let map: Arc<SharedMap<u32, u64, Global>> = Arc::new(SharedMap::new_in("test.map", Global));
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
            let map: Arc<SharedMap<u32, u32, Global>> = Arc::new(SharedMap::new_in("test.map", Global));
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
