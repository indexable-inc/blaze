//! Model tests (docs/v2/plan/lanes/safety/model.md) of the table from two threads under every
//! interleaving to the preemption bound and every value C11 lets a load read. A value holds a
//! tracked cell its readers read and its drop writes, so an entry freed before a reader that
//! loaded it is done is a data race the checker reports. Two slots, so every key shares one
//! window and replacing and evicting happen in a few steps.

use std::sync::Arc;

use blaze_check::{cell::UnsafeCell, model, thread};
use blaze_reclaim::enter;

use super::{Key, Table};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct K(u64);

impl Key for K {
    fn hint(&self) -> u64 {
        self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1
    }
}

/// A value: readers read `n`, its drop writes it.
struct Probe {
    n: UnsafeCell<u64>,
}

// SAFETY: `n` is reached only through `with` and `get_mut`, each an access the model tracks. It
// runs every thread on one OS thread, so no two accesses overlap, and one its clocks do not
// order is reported as a race.
unsafe impl Sync for Probe {}

impl Probe {
    fn new(n: u64) -> Probe {
        Probe { n: UnsafeCell::new(n) }
    }

    fn n(&self) -> u64 {
        // SAFETY: a `u64` of a value the caller got from the table under a guard it still
        // holds, so the limbo has not freed it.
        self.n.with(|n| unsafe { *n })
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        *self.n.get_mut() = u64::MAX;
    }
}

type Probes = Table<K, u32, Probe>;

fn get(table: &Probes, key: u64, state: u32) -> Option<u64> {
    let guard = enter();
    table.get(&guard, &K(key), state).map(Probe::n)
}

fn insert(table: &Probes, key: u64, state: u32, n: u64) -> u64 {
    let guard = enter();
    table.insert(&guard, K(key), state, Probe::new(n)).n()
}

/// Two threads storing one key in one state get the same value back, and it is the one the
/// table keeps.
#[test]
fn model_racing_inserts_keep_one_value() {
    model(|| {
        let table = Arc::new(Probes::new(1));
        let other = {
            let table = Arc::clone(&table);
            thread::spawn(move || insert(&table, 1, 1, 20))
        };
        let mine = insert(&table, 1, 1, 10);
        let theirs = other.join().unwrap();
        assert!(mine == 10 || mine == 20, "an insert gave {mine}");
        assert_eq!(mine, theirs, "two values for one key and state");
        assert_eq!(get(&table, 1, 1), Some(mine));
    });
}

/// A reader that found an entry reads it whole while a writer replaces it with the key's newer
/// state: the replaced entry is freed only after the reader's task ends.
#[test]
fn model_a_replaced_entry_outlives_its_reader() {
    model(|| {
        let table = Arc::new(Probes::new(1));
        insert(&table, 1, 1, 1);
        let reader = {
            let table = Arc::clone(&table);
            thread::spawn(move || get(&table, 1, 1))
        };
        assert_eq!(insert(&table, 1, 2, 2), 2);
        let read = reader.join().unwrap();
        assert!(read.is_none() || read == Some(1), "a reader read {read:?}");
        assert_eq!(get(&table, 1, 1), None, "the older state is gone");
        assert_eq!(get(&table, 1, 2), Some(2));
    });
}

/// A reader of one key reads its entry whole while a writer's insert of a third key evicts an
/// entry from the full window.
#[test]
fn model_an_evicted_entry_outlives_its_reader() {
    model(|| {
        let table = Arc::new(Probes::new(1));
        insert(&table, 1, 1, 1);
        insert(&table, 2, 1, 2);
        let reader = {
            let table = Arc::clone(&table);
            thread::spawn(move || get(&table, 1, 1))
        };
        assert_eq!(insert(&table, 3, 1, 3), 3);
        let read = reader.join().unwrap();
        assert!(read.is_none() || read == Some(1), "a reader read {read:?}");
        assert_eq!(get(&table, 3, 1), Some(3), "the newest key is kept");
        let left = [get(&table, 1, 1), get(&table, 2, 1)];
        assert_eq!(left.iter().flatten().count(), 1, "one of the older keys is evicted: {left:?}");
    });
}
