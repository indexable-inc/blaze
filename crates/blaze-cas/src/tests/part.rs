//! A part of a value read in place ([`crate::Reader::part`], `Look::part`): what a reader of
//! one name of a crate's def map pages asks, wherever the store keeps the value.

use super::*;

/// Each part of `value` asked for is the value's bytes there; a part past its end is an
/// error naming its length; a key the store lacks is `None`.
fn parts_read(store: &Store, value: &[u8], when: &str) {
    let len = value.len() as u64;
    assert!(len > 20_000, "the test's value spans pages");
    let parts = [(0, 16), (4095, 2), (len / 3, 300), (len - 10, 10), (0, len), (5, 0), (8192, 4096)];
    for (at, part_len) in parts {
        let part = store.look().part(&key(1), at, part_len).unwrap().unwrap();
        assert_eq!(part, &value[at as usize..(at + part_len) as usize], "{when}: {at}+{part_len}");
    }
    let said = format!("were asked of its {len} bytes");
    for (at, part_len) in [(len - 1, 2), (len, 1), (u64::MAX, 2)] {
        let refused = store.look().part(&key(1), at, part_len).unwrap_err();
        assert!(refused.what.contains(&said), "{when}: {refused}");
    }
    assert!(store.look().part(&key(2), 0, 1).unwrap().is_none(), "{when}");
}

/// The invariant: a part is the same bytes as that range of the value [`Store::get`] gives,
/// while the value is in its writer's segment, once it is a raw record of a pack (its first
/// part read verifies it whole; those after are read through the pool, the part's pages
/// alone), and after the store is opened again.
#[test]
fn a_part_of_a_value_is_that_range_of_it_wherever_it_is_kept() {
    let mut sim = Sim::new(7, quiet());
    let mut store = open(&mut sim, "store");
    // Under `EXTERNAL_FROM`, so it is a raw record of a pack, not a file of its own.
    let value = noise(EXTERNAL_FROM - 6_000, 9);
    store.put(key(1), Kind::Query, &value).unwrap();
    parts_read(&store, &value, "in its segment");
    flush(&mut sim, &mut store);
    assert_eq!(form_of(&store, &key(1)), Form::Raw, "the test's value is a raw record");
    parts_read(&store, &value, "in its pack");
    parts_read(&store, &value, "in its pack, verified");
    drop(store);
    let store = open(&mut sim, "store");
    parts_read(&store, &value, "after a reopen");
    parts_read(&store, &value, "after a reopen, verified");
}

/// The control: a compressed value has no bytes to read in place, so its parts are of the
/// value made whole, and are still that range of it.
#[test]
fn a_part_of_a_compressed_value_is_of_the_value_made_whole() {
    let mut sim = Sim::new(8, quiet());
    let mut store = open(&mut sim, "store");
    let mut value = Vec::new();
    let mut n = 0;
    while value.len() < cold::LONG_FROM + 30_000 {
        value.extend(run_output(n));
        n += 1;
    }
    store.put(key(1), Kind::Debug, &value).unwrap();
    assert_eq!(form_of(&store, &key(1)), Form::Zstd, "the test's value is kept compressed");
    parts_read(&store, &value, "compressed, in its segment");
    flush(&mut sim, &mut store);
    parts_read(&store, &value, "compressed, in its pack");
}
