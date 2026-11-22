//! The own SwissTable ([`crate::raw`]) through [`Map`], against a `BTreeMap` under the same
//! operations: inserts, removals (many, so that full groups fill with DEAD slots and the table
//! rehashes them away), lookups, entries, `retain`, `reserve`, `shrink_to`, `clear`, clones and
//! drains, compared after every step.

use std::{
    alloc::Global,
    collections::BTreeMap,
    hash::{BuildHasher, Hasher},
    string::String,
    vec::Vec,
};

use blaze_sim::{Config, Tape};

use crate::{FixedState, Map, hash_map::Entry};

/// A hash that puts every key in one of two home groups and gives them all one tag: groups
/// fill, a probe walks past many slots, and removals from full groups leave DEAD slots.
#[derive(Clone, Copy, Default)]
struct Clustered;

#[derive(Default)]
struct ClusteredHasher(u64);

impl Hasher for ClusteredHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = self.0 << 8 | u64::from(byte);
        }
    }

    fn finish(&self) -> u64 {
        // The low bit picks the home group; the top seven bits, the tag, are 0 for every key
        // under 2^17.
        (self.0 & 1) | (self.0 << 40)
    }
}

impl BuildHasher for Clustered {
    type Hasher = ClusteredHasher;

    fn build_hasher(&self) -> ClusteredHasher {
        ClusteredHasher::default()
    }
}

/// The map and the model hold the same pairs: lengths, every key's lookup, and the walk.
fn same<S: BuildHasher>(
    map: &Map<u16, u32, S, Global>,
    model: &BTreeMap<u16, u32>,
    keys: u16,
) -> Result<(), String> {
    if map.len() != model.len() || map.capacity() < map.len() {
        return Err(format!(
            "len {} capacity {}, the model's len {}",
            map.len(),
            map.capacity(),
            model.len()
        ));
    }
    for key in 0..keys {
        if map.get(&key) != model.get(&key) {
            return Err(format!("key {key}: {:?}, the model {:?}", map.get(&key), model.get(&key)));
        }
    }
    let mut walked: Vec<(u16, u32)> = map.iter().map(|(&key, &value)| (key, value)).collect();
    walked.sort_unstable();
    let held: Vec<(u16, u32)> = model.iter().map(|(&key, &value)| (key, value)).collect();
    if walked != held {
        return Err(format!("the map walks {walked:?}, the model holds {held:?}"));
    }
    Ok(())
}

fn agrees_on<S: BuildHasher + Clone>(tape: &Tape, hasher: S) -> Result<(), String> {
    let mut map: Map<u16, u32, S, Global> = Map::with_hasher_in(hasher, Global);
    let mut model: BTreeMap<u16, u32> = BTreeMap::new();
    // Enough keys to fill several groups, few enough that most operations meet a held one.
    let keys = tape.range(8..80, "keys") as u16;
    // Under Miri, enough steps to fill, empty and regrow a few groups (with `keys` up to 80,
    // a resize, a rehash of DEAD slots and a reuse of one are all met within 60).
    for step in 0..tape.below(if cfg!(miri) { 60 } else { 400 }, "steps") {
        let key = tape.below(u64::from(keys), "key") as u16;
        let value = step as u32;
        match tape.below(16, "op") {
            0..=4 => {
                if map.insert(key, value) != model.insert(key, value) {
                    return Err(format!("insert of {key} disagrees"));
                }
            }
            // Removals as often as inserts: a full group keeps the slots it gives up as DEAD.
            5..=9 => {
                if map.remove(&key) != model.remove(&key) {
                    return Err(format!("remove of {key} disagrees"));
                }
            }
            10 => {
                let ours = match map.entry(key) {
                    Entry::Occupied(mut entry) => Some(entry.insert(value)),
                    Entry::Vacant(entry) => {
                        entry.insert(value);
                        None
                    }
                };
                if ours != model.insert(key, value) {
                    return Err(format!("entry of {key} disagrees"));
                }
            }
            11 => {
                let modulus = u32::from(key) + 2;
                map.retain(|_, value| *value % modulus != 0);
                model.retain(|_, value| *value % modulus != 0);
            }
            12 => map.reserve(tape.below(64, "reserve") as usize),
            13 => map.shrink_to(tape.below(64, "shrink") as usize),
            14 => {
                if tape.one_in(8, "clear") {
                    map.clear();
                    model.clear();
                } else if map.remove_entry(&key) != model.remove_entry(&key) {
                    return Err(format!("remove_entry of {key} disagrees"));
                }
            }
            _ => {
                let copy = map.clone();
                same(&copy, &model, keys).map_err(|it| format!("the clone: {it}"))?;
            }
        }
        same(&map, &model, keys).map_err(|it| format!("after step {step}: {it}"))?;
    }
    // Taking everything out gives what was held, and leaves the map empty and usable.
    let mut drained: Vec<(u16, u32)> = map.drain().collect();
    drained.sort_unstable();
    if drained != model.iter().map(|(&key, &value)| (key, value)).collect::<Vec<_>>() {
        return Err(format!("drain gave {drained:?}"));
    }
    model.clear();
    same(&map, &model, keys)?;
    map.insert(0, 0);
    model.insert(0, 0);
    same(&map, &model, keys)
}

fn agrees(tape: &Tape) -> Result<(), String> {
    if tape.coin("clustered") {
        agrees_on(tape, Clustered)
    } else {
        agrees_on(tape, FixedState)
    }
}

/// Invariant: after any operations the table holds what a `BTreeMap` under the same
/// operations holds, finds each key, and walks each pair once; under a hash that fills groups,
/// so that removals leave DEAD slots and growth rehashes them.
#[test]
fn a_map_is_a_btree_map_under_any_operations() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 400 }), agrees);
}

/// What a value of the tests below counts: its drops, into a shared cell.
struct Counted(std::rc::Rc<std::cell::Cell<usize>>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

/// A table that grows, has a key removed from a full group and another inserted into the
/// DEAD slot it left, then is walked mutably: the unsafe paths of `resize`, `remove` and
/// `insert_no_grow` on a reused slot, and `IterMut`, at Miri's size.
#[test]
fn a_dead_slot_is_reused_after_growth() {
    let mut map: Map<u16, u32, Clustered, Global> = Map::with_hasher_in(Clustered, Global);
    for key in 0..40 {
        map.insert(key, u32::from(key));
    }
    for key in (0..40).step_by(2) {
        assert_eq!(map.remove(&key), Some(u32::from(key)));
    }
    for key in 40..50 {
        map.insert(key, u32::from(key));
    }
    for (_, value) in map.iter_mut() {
        *value += 1;
    }
    assert!(
        (1..50).step_by(2).chain(40..50).all(|key| map.get(&key) == Some(&(u32::from(key) + 1)))
    );
}

/// A drain forgotten with `mem::forget` leaks its values, never exposes a moved-out one: the
/// map is empty and usable after it.
#[test]
fn a_forgotten_drain_leaves_the_map_empty() {
    let dropped = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut map: Map<u32, Counted, FixedState, Global> = Map::new_in(Global);
    for key in 0..20 {
        map.insert(key, Counted(dropped.clone()));
    }
    let mut drain = map.drain();
    drop(drain.next());
    // The forget, kept findable: the drain goes to a leaked box rather than `mem::forget`, so
    // under Miri its box is a root and the table and values it leaks by design are reachable,
    // not reported, while a leak anywhere else still fails the run.
    let forgotten = std::boxed::Box::into_raw(std::boxed::Box::new(drain));
    #[cfg(miri)]
    {
        unsafe extern "Rust" {
            /// Miri's shim: the allocation at `ptr`, and all it reaches, is not a leak.
            safe fn miri_static_root(ptr: *const u8);
        }
        miri_static_root(forgotten.cast::<u8>());
    }
    let _ = forgotten;
    assert_eq!(dropped.get(), 1, "only the value taken is dropped");
    assert_eq!(map.len(), 0);
    assert!(map.get(&3).is_none());
    map.insert(3, Counted(dropped.clone()));
    assert_eq!(map.len(), 1);
    drop(map);
    assert_eq!(dropped.get(), 2);
}

/// An owning iterator dropped partway drops the values it did not hand out, once each.
#[test]
fn an_into_iter_dropped_partway_drops_the_rest() {
    let dropped = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut map: Map<u32, Counted, FixedState, Global> = Map::new_in(Global);
    for key in 0..20 {
        map.insert(key, Counted(dropped.clone()));
    }
    let mut values = map.into_values();
    drop(values.next());
    drop(values.next());
    assert_eq!(dropped.get(), 2);
    drop(values);
    assert_eq!(dropped.get(), 20);
}

/// A set of keys borrowed from a map, dropped after the map is moved out: the table's drop
/// reads no borrow (`#[may_dangle]`), so this compiles, and Miri sees no use of the moved map.
#[test]
fn a_set_of_borrowed_keys_outlives_its_owner_by_its_drop() {
    fn keep(map: Map<String, u32, FixedState, Global>) -> Map<String, u32, FixedState, Global> {
        let mut used: crate::Set<&str, FixedState, Global> = crate::Set::new_in(Global);
        for key in map.keys() {
            used.insert(key.as_str());
        }
        assert_eq!(used.len(), map.len());
        map
    }
    let mut map: Map<String, u32, FixedState, Global> = Map::new_in(Global);
    for key in 0..12u32 {
        map.insert(format!("key {key}"), key);
    }
    assert_eq!(keep(map).len(), 12);
}

/// A clone whose value's `clone` panics partway leaves a copy that drops exactly the values
/// cloned, and frees its memory: the clone's unwinding path.
#[test]
fn a_clone_that_panics_drops_what_it_cloned() {
    use std::{cell::Cell, rc::Rc};

    struct Brittle {
        clones: Rc<Cell<usize>>,
        drops: Rc<Cell<usize>>,
    }

    impl Clone for Brittle {
        fn clone(&self) -> Brittle {
            self.clones.set(self.clones.get() + 1);
            assert!(self.clones.get() < 6, "the sixth clone panics");
            Brittle { clones: self.clones.clone(), drops: self.drops.clone() }
        }
    }

    impl Drop for Brittle {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    let (clones, drops) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let mut map: Map<u32, Brittle, FixedState, Global> = Map::new_in(Global);
    for key in 0..12 {
        map.insert(key, Brittle { clones: clones.clone(), drops: drops.clone() });
    }
    let copy = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| map.clone()));
    assert!(copy.is_err());
    assert_eq!(drops.get(), 5, "the five values cloned are dropped with the copy");
    assert_eq!(map.len(), 12);
}

/// A hash that panics in the middle of a resize leaves every value in the old table, and the
/// new allocation freed (`Moving`'s drop): the map still finds what it held.
#[test]
fn a_resize_whose_hash_panics_keeps_the_old_table() {
    use std::{cell::Cell, rc::Rc};

    #[derive(Clone, Default)]
    struct Fragile(Rc<Cell<bool>>);

    struct FragileHasher(u64, bool);

    impl Hasher for FragileHasher {
        fn write(&mut self, bytes: &[u8]) {
            assert!(!self.1, "the hash panics");
            for &byte in bytes {
                self.0 = self.0.wrapping_mul(31).wrapping_add(u64::from(byte));
            }
        }

        fn finish(&self) -> u64 {
            self.0.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        }
    }

    impl BuildHasher for Fragile {
        type Hasher = FragileHasher;

        fn build_hasher(&self) -> FragileHasher {
            FragileHasher(0, self.0.get())
        }
    }

    let fragile = Fragile::default();
    let mut map: Map<u32, u32, Fragile, Global> = Map::with_hasher_in(fragile.clone(), Global);
    let mut key = 0;
    let capacity = loop {
        map.insert(key, key);
        key += 1;
        if map.len() == map.capacity() {
            break map.capacity();
        }
    };
    fragile.0.set(true);
    let grown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| map.reserve(1)));
    assert!(grown.is_err());
    fragile.0.set(false);
    assert_eq!(map.capacity(), capacity, "the old table kept");
    assert!((0..key).all(|it| map.get(&it) == Some(&it)));
}

/// Values with a `Drop` are dropped once each, by removal, `clear`, `retain`, a drain cut short
/// and the map's own drop.
#[test]
fn every_value_is_dropped_once() {
    use std::{cell::Cell, rc::Rc};

    struct Counted(Rc<Cell<usize>>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let dropped = Rc::new(Cell::new(0));
    let made = if cfg!(miri) { 40 } else { 300 };
    {
        let mut map: Map<u32, Counted, FixedState, Global> = Map::new_in(Global);
        for key in 0..made {
            map.insert(key, Counted(dropped.clone()));
        }
        for key in 0..50 {
            map.remove(&key);
        }
        map.retain(|key, _| key % 3 != 0);
        let mut drain = map.drain();
        drain.next();
        drop(drain);
        assert_eq!(dropped.get(), made as usize, "a drain cut short drops the rest");
        for key in 0..made {
            map.insert(key, Counted(dropped.clone()));
        }
    }
    assert_eq!(dropped.get(), 2 * made as usize, "the map's drop drops what it held");
}
