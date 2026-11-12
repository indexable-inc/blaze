//! [`OrderMap`] and [`OrderSet`] against a model (a `Vec` of pairs under the same operations),
//! their allocations counted, their order across hashers, capacities and processes, the keyed
//! tables, and the cost rows of docs/v2/plan/design/std.md's collections chapter.

use std::{
    alloc::{AllocError, Allocator, Global, Layout},
    cell::Cell,
    hash::BuildHasher,
    ptr::NonNull,
};

use hegel::{TestCase, generators as gs};

use crate::{
    BuildKeyHasher, ByHash, FixedState, HashMap, IndexMap, IndexSet, KeyedMap, KeyedSet, OrderMap,
    SipKey,
};

/// The global heap, each allocation counted.
#[derive(Clone)]
struct Counted<'a>(&'a Cell<usize>);

// SAFETY: every call is `Global`'s, with the arguments given: a block it hands out is `Global`'s
// and is freed by `Global`, and the count is no part of that.
unsafe impl Allocator for Counted<'_> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.0.set(self.0.get() + 1);
        Global.allocate(layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: `ptr` is a block `allocate` above got from `Global` with `layout`.
        unsafe { Global.deallocate(ptr, layout) }
    }
}

fn below(tc: &TestCase, n: u64) -> u64 {
    tc.draw(gs::integers::<u64>().min_value(0).max_value(n - 1))
}

/// One run of random operations on a map and on the model, compared after each.
fn agrees(tc: &TestCase) -> Result<(), String> {
    let mut map: IndexMap<u8, u32, Global> = IndexMap::new_in(Global); // alloc: a test's map
    let mut model: Vec<(u8, u32)> = Vec::new(); // alloc: a test's model
    let place = |model: &Vec<(u8, u32)>, key: u8| model.iter().position(|it| it.0 == key);
    for step in 0..below(tc, 200) {
        // Few keys, so that most operations meet a key that is there.
        let key = below(tc, 24) as u8;
        let value = step as u32;
        match below(tc, 9) {
            0..=2 => {
                let old = map.insert(key, value);
                match place(&model, key) {
                    Some(at) => {
                        if old != Some(model[at].1) {
                            return Err(format!("insert of a held key gave {old:?}"));
                        }
                        model[at].1 = value;
                    }
                    None => {
                        if old.is_some() {
                            return Err(format!("insert of a new key gave {old:?}"));
                        }
                        model.push((key, value));
                    }
                }
            }
            3 => {
                let removed = map.swap_remove(&key);
                let at = place(&model, key);
                if removed != at.map(|at| model[at].1) {
                    return Err(format!("swap_remove gave {removed:?}"));
                }
                if let Some(at) = at {
                    model.swap_remove(at);
                }
            }
            4 => {
                let removed = map.shift_remove(&key);
                let at = place(&model, key);
                if removed != at.map(|at| model[at].1) {
                    return Err(format!("shift_remove gave {removed:?}"));
                }
                if let Some(at) = at {
                    model.remove(at);
                }
            }
            5 => {
                if map.pop() != model.pop() {
                    return Err("pop disagrees".to_owned());
                }
            }
            6 => {
                let at = below(tc, 26) as usize;
                let removed = map.shift_remove_index(at);
                let expected = (at < model.len()).then(|| model.remove(at));
                if removed != expected {
                    return Err(format!("shift_remove_index({at}) gave {removed:?}"));
                }
            }
            7 => {
                let at = below(tc, 26) as usize;
                let removed = map.swap_remove_index(at);
                let expected = (at < model.len()).then(|| model.swap_remove(at));
                if removed != expected {
                    return Err(format!("swap_remove_index({at}) gave {removed:?}"));
                }
            }
            _ => {
                let cut = below(tc, 4) as u8;
                map.retain(|key, _| key % 4 != cut);
                model.retain(|it| it.0 % 4 != cut);
            }
        }
        // alloc: what the map iterates, to compare with the model
        let held: Vec<(u8, u32)> = map.iter().map(|(&key, &value)| (key, value)).collect();
        if held != model {
            return Err(format!("the map iterates {held:?}, the model {model:?}"));
        }
        for (at, &(key, value)) in model.iter().enumerate() {
            if map.get_full(&key) != Some((at, &key, &value)) || map.get_index(at) != Some((&key, &value)) {
                return Err(format!("key {key} is not found at {at} with {value}"));
            }
        }
        if map.len() != model.len() || map.contains_key(&key) != place(&model, key).is_some() {
            return Err("len or contains_key disagrees".to_owned());
        }
    }
    Ok(())
}

/// The invariant: after any operations an insertion-ordered map iterates the pairs a `Vec`
/// under the same operations holds, in its order, and finds each key at its index.
#[hegel::test(test_cases = 300)]
fn an_ordered_map_is_a_vec_of_pairs_under_any_operations(tc: TestCase) {
    if let Err(why) = agrees(&tc) {
        panic!("{why}");
    }
}

/// The two removals are two orders, so the model above tells them apart: `swap_remove` moves
/// the last entry into the hole, `shift_remove` keeps the order, and each index follows.
#[test]
fn the_two_removals_leave_two_orders() {
    let mut swapped: IndexMap<u8, u32, Global> = IndexMap::new_in(Global); // alloc: a test's map
    swapped.extend([(1, 10), (2, 20), (3, 30), (4, 40)]);
    let mut shifted = swapped.clone();
    assert_eq!((swapped.swap_remove(&1), shifted.shift_remove(&1)), (Some(10), Some(10)));
    assert_eq!(swapped.keys().copied().collect::<Vec<_>>(), [4, 2, 3]);
    assert_eq!(shifted.keys().copied().collect::<Vec<_>>(), [2, 3, 4]);
    assert_ne!(swapped, shifted, "equality is of the order too");
    assert_eq!((swapped.get_index_of(&4), shifted.get_index_of(&4)), (Some(0), Some(2)));
    shifted.sort_keys();
    swapped.sort_keys();
    assert_eq!(swapped, shifted);
}

/// A set is the map of its values: order, indices and both removals.
#[test]
fn an_ordered_set_keeps_insertion_order() {
    let mut set: IndexSet<&str, Global> = IndexSet::new_in(Global); // alloc: a test's set
    for name in ["c", "a", "b", "a", "d"] {
        set.insert(name);
    }
    assert_eq!(set.iter().copied().collect::<Vec<_>>(), ["c", "a", "b", "d"]);
    assert_eq!((set.get_index_of("b"), set.get_index(0), set[3]), (Some(2), Some(&"c"), "d"));
    assert!(set.swap_remove("c") && !set.swap_remove("c"));
    assert_eq!(set.iter().copied().collect::<Vec<_>>(), ["d", "a", "b"]);
    assert!(set.shift_remove("d"));
    assert_eq!(set.iter().copied().collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!((set.insert_full("b"), set.insert_full("z")), ((1, false), (2, true)));
}

/// A table made with room for what it will hold allocates when it is made and never after:
/// once for a hash map, twice for an ordered one (its entries and its table of indices). One
/// made empty allocates nothing until its first insert.
#[test]
fn a_table_with_room_allocates_only_when_made() {
    const N: usize = 1000;
    let count = Cell::new(0);
    let mut map: HashMap<u32, u32, Counted<'_>> = HashMap::with_capacity_in(N, Counted(&count));
    assert_eq!(count.get(), 1, "a hash map with room");
    let mut ordered: IndexMap<u32, u32, Counted<'_>> =
        IndexMap::with_capacity_in(N, Counted(&count));
    assert_eq!(count.get(), 3, "an ordered map with room: its entries and its table");
    for i in 0..N as u32 {
        map.insert(i, i);
        ordered.insert(i, i);
    }
    for i in 0..N as u32 {
        assert_eq!((map.get(&i), ordered.get(&i)), (Some(&i), Some(&i)));
        ordered.swap_remove(&i);
        ordered.insert(i, i);
    }
    assert_eq!(count.get(), 3, "inserts, lookups and removals within the room");
    // The cause removed: one entry more than the room grows both.
    map.insert(N as u32, 0);
    let mut more = N as u32;
    while count.get() == 3 {
        more += 1;
        map.insert(more, 0);
        ordered.insert(more, 0);
    }
    assert!(count.get() > 3);
    let empty = Cell::new(0);
    let made: IndexMap<u32, u32, Counted<'_>> = IndexMap::new_in(Counted(&empty));
    assert_eq!((empty.get(), made.len()), (0, 0), "an empty table");
}

/// What a table of `inserts` iterates, as one number: the order is in it.
fn order_of(pairs: impl Iterator<Item = (u64, u64)>) -> u64 {
    let mut hasher = crate::FixedHasher::default();
    for pair in pairs {
        std::hash::Hash::hash(&pair, &mut hasher);
    }
    std::hash::Hasher::finish(&hasher)
}

/// 500 keys in an order no hash gives: descending odd, then ascending even.
fn inserts() -> impl Iterator<Item = (u64, u64)> {
    (0..250u64).rev().map(|it| 2 * it + 1).chain((0..250).map(|it| 2 * it)).map(|key| (key << 20, key))
}

const CHILD: &str = "BLAZE_HASH_ORDER_CHILD";

/// The invariant: an ordered map iterates in insertion order, whatever hashes its keys (the
/// fixed seed, a key drawn at random), whatever its capacity, and in another process. The
/// control is the cause: a hash map of the same inserts under two random keys iterates in two
/// orders, so the order this test compares is one a table can get wrong.
#[test]
fn insertion_order_is_the_same_under_any_hasher_and_in_another_process() {
    let expected = order_of(inserts());
    let fixed: IndexMap<u64, u64, Global> = IndexMap::from_iter_in(inserts(), Global); // alloc: a test's map
    let own = order_of(fixed.iter().map(|(&key, &value)| (key, value)));
    if std::env::var_os(CHILD).is_some() {
        println!("order:{own}");
        return;
    }
    assert_eq!(own, expected);
    for capacity in [0, 7, 4096] {
        let mut keyed: OrderMap<u64, u64, SipKey, Global> =
            OrderMap::with_capacity_and_hasher_in(capacity, SipKey::random(), Global); // alloc: a test's map
        keyed.extend(inserts());
        assert_eq!(order_of(keyed.iter().map(|(&key, &value)| (key, value))), expected);
    }
    // The control: the hasher decides a hash map's order.
    let hashed = |key: SipKey| {
        let mut map: KeyedMap<u64, u64, Global> = KeyedMap::with_hasher_in(key, Global); // alloc: a test's map
        map.extend(inserts());
        order_of(map.iter().map(|(&key, &value)| (key, value)))
    };
    assert_ne!(hashed(SipKey::new(1, 2)), hashed(SipKey::new(3, 4)));
    // Two more processes, each a run of this test that prints its order.
    let this = std::env::current_exe().expect("the test's own program");
    for _ in 0..2 {
        let out = std::process::Command::new(&this)
            .args([
                "--exact",
                "order_tests::insertion_order_is_the_same_under_any_hasher_and_in_another_process",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("the test's own program runs");
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().find_map(|it| it.split("order:").nth(1));
        assert_eq!(
            line.map(str::trim),
            Some(expected.to_string().as_str()),
            "the other process's order, from: {text}"
        );
    }
}

/// Keys an attacker picks: under a hasher they can compute, as many keys as they like have one
/// hash (here the identity hasher's, whose hash is the key's first word: the plainest case of
/// a hash known outside); under a `SipKey` they cannot see, the same keys spread, and two keys
/// place them differently. A keyed table is made only with its key.
#[test]
fn a_keyed_table_spreads_keys_picked_to_collide() {
    let picked: Vec<[u64; 2]> = (0..4096u64).map(|it| [0x5eed, it]).collect(); // alloc: a test's keys
    let known: crate::HashSet<u64, Global> =
        picked.iter().map(|it| BuildKeyHasher::default().hash_one(it)).collect(); // alloc: a test's set
    assert_eq!(known.len(), 1, "every picked key has one hash under the known hasher");
    let (one, two) = (SipKey::random(), SipKey::random());
    let spread: crate::HashSet<u64, Global> = picked.iter().map(|it| one.hash_one(it)).collect(); // alloc: a test's set
    assert_eq!(spread.len(), picked.len(), "under a key they spread");
    assert_ne!(one.hash_one(&picked[0]), two.hash_one(&picked[0]), "and each key places them anew");
    let mut map: KeyedMap<[u64; 2], u32, Global> = KeyedMap::with_hasher_in(one, Global); // alloc: a test's map
    let mut set: KeyedSet<[u64; 2], Global> = KeyedSet::with_hasher_in(two, Global); // alloc: a test's set
    for (at, key) in picked.iter().enumerate() {
        map.insert(*key, at as u32);
        set.insert(*key);
    }
    assert!(picked.iter().enumerate().all(|(at, key)| map.get(key) == Some(&(at as u32)) && set.contains(key)));
}

/// The cost rows of the collections chapter: `cargo test --release -p blaze-hash --lib
/// collections_cost -- --ignored --nocapture`. Nanoseconds an operation, the median of five
/// runs over a million entries; a row is `what: ours vs what it replaces`.
#[test]
#[ignore = "a measurement: run it by name, in release"]
fn collections_cost() {
    const N: u32 = 1_000_000;
    fn median(mut run: impl FnMut() -> u64) -> f64 {
        let mut times = [0f64; 5];
        for time in &mut times {
            let start = std::time::Instant::now();
            std::hint::black_box(run());
            *time = start.elapsed().as_nanos() as f64 / f64::from(N);
        }
        times.sort_by(f64::total_cmp);
        times[2]
    }
    // Ids in an order the tables were not filled in, so a lookup is a cache miss as in use.
    let ids: Vec<u32> = (0..N).map(|it| it.wrapping_mul(0x9e37_79b1) % N).collect(); // alloc: the measurement's keys

    let mut hash: HashMap<u32, u32, Global> = HashMap::new_in(Global); // alloc: measured
    let mut ours: IndexMap<u32, u32, Global> = IndexMap::new_in(Global); // alloc: measured
    let mut theirs: indexmap::IndexMap<u32, u32, FixedState> = indexmap::IndexMap::default(); // alloc: measured
    let fill_hash = median(|| {
        hash = HashMap::new_in(Global);
        (0..N).for_each(|it| drop(hash.insert(it, it)));
        hash.len() as u64
    });
    let fill_ours = median(|| {
        ours = IndexMap::new_in(Global);
        (0..N).for_each(|it| drop(ours.insert(it, it)));
        ours.len() as u64
    });
    let fill_theirs = median(|| {
        theirs = indexmap::IndexMap::default();
        (0..N).for_each(|it| drop(theirs.insert(it, it)));
        theirs.len() as u64
    });
    println!("insert 1M u32 ids, growing: IndexMap {fill_ours:.1} ns, indexmap {fill_theirs:.1} ns, HashMap {fill_hash:.1} ns");

    let sum = |get: &dyn Fn(u32) -> u32| ids.iter().map(|&it| u64::from(get(it))).sum::<u64>();
    let look_hash = median(|| sum(&|it| hash[&it]));
    let look_ours = median(|| sum(&|it| *ours.get(&it).expect("held")));
    let look_theirs = median(|| sum(&|it| theirs[&it]));
    println!("hash + lookup of 1M u32 ids: IndexMap {look_ours:.1} ns, indexmap {look_theirs:.1} ns, HashMap {look_hash:.1} ns");

    let walk_hash = median(|| hash.values().map(|&it| u64::from(it)).sum());
    let walk_ours = median(|| ours.values().map(|&it| u64::from(it)).sum());
    let walk_theirs = median(|| theirs.values().map(|&it| u64::from(it)).sum());
    println!("iteration of 1M entries: IndexMap {walk_ours:.2} ns, indexmap {walk_theirs:.2} ns, HashMap {walk_hash:.2} ns");

    // 32-byte hashes: spread by a multiply per word, as a digest's bytes are.
    let digest = |it: u32| {
        let mut bytes = [0u8; 32];
        for (at, chunk) in bytes.chunks_exact_mut(8).enumerate() {
            let word = (u64::from(it) + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15u64.wrapping_add(2 * at as u64));
            chunk.copy_from_slice(&(word ^ (word >> 29)).to_le_bytes());
        }
        bytes
    };
    let digests: Vec<[u8; 32]> = ids.iter().map(|&it| digest(it)).collect(); // alloc: the measurement's keys
    let mut by_hash: ByHash<[u8; 32], u32, Global> = ByHash::new_in(Global); // alloc: measured
    let mut hashed: HashMap<[u8; 32], u32, Global> = HashMap::new_in(Global); // alloc: measured
    for it in 0..N {
        by_hash.insert(digest(it), it);
        hashed.insert(digest(it), it);
    }
    let look_by = median(|| digests.iter().map(|it| u64::from(by_hash[it])).sum());
    let look_hashed = median(|| digests.iter().map(|it| u64::from(hashed[it])).sum());
    println!("lookup of 1M 32-byte hashes: ByHash {look_by:.1} ns, HashMap<[u8; 32]> {look_hashed:.1} ns");
}
