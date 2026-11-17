//! A hash key drawn from the world's randomness ([`Io::sip_key`]) is a function of the world's
//! seed: two worlds of one seed give equal keys, and so a keyed table filled alike in each
//! iterates alike; another seed gives another key. Controls: the same world's second key
//! differs from its first (each call draws anew), and blaze-hash's ambient form
//! (`SipKey::random`, the operating system's, which this replaces) gives different keys and
//! different iteration from one call to the next, which is what makes a run keyed by it
//! unrepeatable.

use std::{alloc::Global, vec::Vec};

use blaze_hash::{Map, MapOpts, SipKey};

use crate::io::{Io, Offers};

/// The keys of `0..256` in the order a table keyed by `key` iterates them, filled in order.
fn order(key: SipKey) -> Vec<u64> {
    let mut map: Map<u64, (), SipKey, Global> = Map::new(MapOpts { hasher: key, alloc: Global, capacity: None }); // alloc: a test's map
    for at in 0..256 {
        map.insert(at, ());
    }
    map.keys().copied().collect()
}

/// Expected line: `test tests::sip_key::the_same_seed_gives_the_same_key ... ok`.
#[test]
fn the_same_seed_gives_the_same_key() {
    let one = Io::sim(7).sip_key().expect("a sim world offers randomness");
    let two = Io::sim(7).sip_key().expect("a sim world offers randomness");
    assert_eq!(one, two, "two worlds of seed 7 drew different keys");
    assert_eq!(order(one), order(two), "one key, one fill, two iteration orders");

    let other = Io::sim(8).sip_key().expect("a sim world offers randomness");
    assert_ne!(one, other, "seeds 7 and 8 drew one key");

    // Control: each call draws anew, so a test passing on a constant key would fail here.
    let world = Io::sim(7);
    let first = world.sip_key().expect("a sim world offers randomness");
    let second = world.sip_key().expect("a sim world offers randomness");
    assert_eq!(first, one, "a world's first key is its seed's");
    assert_ne!(first, second, "one world drew one key twice");

    // Control: the ambient form this replaces is unrepeatable, keys and iteration both.
    let (a, b) = (SipKey::random(), SipKey::random());
    assert_ne!(a, b, "the operating system gave one key twice");
    assert_ne!(order(a), order(b), "two random keys placed 256 keys alike");
}

/// A host that offers no randomness gives no key, and `need_sip_key` names what is missing.
///
/// Expected line: `test tests::sip_key::no_randomness_no_key ... ok`.
#[test]
fn no_randomness_no_key() {
    let io = Io::sim_with(7, Offers::NONE);
    assert_eq!(io.sip_key(), None);
    assert!(io.need_sip_key().is_err(), "a world without randomness handed out a key");
}
