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
    for step in 0..tape.below(400, "steps") {
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

/// The invariant: after any operations the table holds what a `BTreeMap` under the same
/// operations holds, finds each key, and walks each pair once; under a hash that fills groups,
/// so that removals leave DEAD slots and growth rehashes them.
#[test]
fn a_map_is_a_btree_map_under_any_operations() {
    blaze_sim::check(Config::cases(400), agrees);
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
    let made = 300;
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
