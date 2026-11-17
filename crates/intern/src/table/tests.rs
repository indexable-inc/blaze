use std::alloc::Global;
use std::{collections::HashMap, panic::AssertUnwindSafe};

use blaze_ids::rng::SplitMix64;

use super::*;
use crate::order_key;

/// Compiled in: "a" twice, so the first is the one interning gives.
const STATICS: [&str; 3] = ["a", "b", "a"];

fn shape() -> Shape {
    Shape {
        statics: STATICS.len() as u32,
        static_bytes: |id| STATICS[id as usize - 1].as_bytes(),
        key: order_key,
    }
}

/// A short name from few letters, so many share an order key and a hash tag.
fn name(rng: &mut SplitMix64) -> String {
    let len = rng.index(12);
    (0..len).map(|_| ['a', 'b', 'é', '_', '0'][rng.index(5)]).collect()
}

#[test]
fn compiled_in_values_are_their_ids() {
    let table = Table::new(shape());
    assert_eq!(table.intern(b"a"), 1, "a value compiled in twice is the first");
    assert_eq!(table.intern(b"b"), 2);
    assert_eq!(table.get(3), Some(&b"a"[..]));
    assert_eq!(table.get(0), None);
    assert_eq!(table.intern(b"c"), 4);
    assert_eq!(table.end(), 5);
    assert_eq!(table.key(4), order_key(b"c"));
}

#[test]
fn a_lost_id_panics() {
    let table = Table::new(shape());
    let never = table.intern(b"made") + 1;
    let lost = std::panic::catch_unwind(AssertUnwindSafe(|| table.bytes(never)));
    assert!(lost.is_err(), "reading an ID never made is a bug, never a quiet empty value");
    let lost = std::panic::catch_unwind(AssertUnwindSafe(|| table.key(never)));
    assert!(lost.is_err());
}

/// Review finding M-1: every table draws its own key, so no one can pick values that collide
/// in a table before it exists. (Under the model checker the key is fixed: a drawn one fails
/// `model.rs`'s models, "the model is not deterministic".)
#[test]
fn each_table_draws_its_own_key() {
    assert_ne!(Table::new(shape()).overlay, Table::new(shape()).overlay);
}

/// The table against an `HashMap<Vec<u8>, u32>` over seeded values, many sharing a hash tag,
/// a home group or an order key: a value new to the map takes the next ID, a known one its own,
/// and every ID reads back its bytes and key.
#[test]
fn interning_agrees_with_a_map_model() {
    for seed in 0..3 {
        let mut rng = SplitMix64::new(0x5eed + seed);
        let table = Table::new(shape());
        let mut model: blaze_hash::HashMap<Vec<u8>, u32, Global> =
            blaze_hash::HashMap::new_in(Global); // alloc: a test's model
        let mut next = table.end();
        // Short names, numbered ones, long runs, digits.
        let value = |rng: &mut SplitMix64| -> Vec<u8> {
            match rng.below(4) {
                0 => name(rng).into_bytes(),
                1 => format!("{}{}", name(rng), rng.below(50)).into_bytes(),
                2 => "x".repeat(rng.index(300)).into_bytes(),
                _ => {
                    let digits = rng.next_u64().to_string();
                    digits.as_bytes()[..rng.index(digits.len() + 1)].to_vec()
                }
            }
        };
        let check = |table: &Table,
                     model: &mut blaze_hash::HashMap<Vec<u8>, u32, Global>,
                     next: &mut u32,
                     bytes: Vec<u8>| {
            // alloc: a test's model
            let id = table.intern(&bytes);
            let expect = match model.get(&bytes) {
                Some(&id) => id,
                None if STATICS.iter().any(|s| s.as_bytes() == bytes) => id,
                None => {
                    *next += 1;
                    *next - 1
                }
            };
            assert_eq!(id, expect, "seed {seed}: {bytes:?}");
            model.insert(bytes.clone(), id);
            assert_eq!(table.get(id), Some(&bytes[..]));
            assert_eq!(table.find(&bytes), Some(id));
            assert_eq!(table.key(id), order_key(&bytes));
        };
        for _ in 0..4_000 {
            let bytes = value(&mut rng);
            check(&table, &mut model, &mut next, bytes);
        }
        assert_eq!(table.end(), next);
        for (bytes, &id) in &model {
            assert_eq!(table.get(id), Some(&bytes[..]), "seed {seed}: {bytes:?}");
        }
    }
}

/// `n` distinct 24-byte ASCII names with one [`blaze_phf::name_hash`], as review finding S5
/// made them: every step of that hash inverts, so after two free words the third is solved for
/// a fixed state before the finish; about one try in 256 solves to ASCII.
fn names_with_one_name_hash(n: usize) -> Vec<String> {
    use blaze_phf::{NAME_K, name_hash_start, name_hash_word};
    const STATE: u64 = 0x0123_4567_89ab_cdef;
    // NAME_K's inverse mod 2^64, by Newton's iteration: each step doubles the bits that hold.
    let mut inverse = NAME_K;
    (0..6).for_each(|_| {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(NAME_K.wrapping_mul(inverse)))
    });
    assert_eq!(NAME_K.wrapping_mul(inverse), 1);
    let before = STATE.rotate_right(26).wrapping_mul(inverse);
    let hex = |word: u64| -> [u8; 8] {
        std::array::from_fn(|at| b"0123456789abcdef"[(word >> (4 * at)) as usize & 15])
    };
    let mut names = Vec::with_capacity(n);
    for tries in 0u64.. {
        if names.len() == n {
            break;
        }
        let (first, second) = (hex(tries), hex(tries >> 32 | 0x5a5a_0000));
        let mut h = name_hash_start(24);
        h = name_hash_word(h, u64::from_le_bytes(first));
        h = name_hash_word(h, u64::from_le_bytes(second));
        let third = h ^ before;
        if third & 0x8080_8080_8080_8080 != 0 {
            continue;
        }
        let bytes = [first, second, third.to_le_bytes()].concat();
        names.push(String::from_utf8(bytes).expect("ASCII"));
    }
    names
}

/// Review finding S5: names with one `name_hash` cost the table a bounded number of probes
/// each, interned and found again. With the table placed
/// by `name_hash` again, each probe walks past every name before it: interning 10k such names
/// loaded 26 million control words (16k: 74 million and 607 ms, against 75 thousand and 1.7 ms
/// placed by SipHash), where the bound is 16 a name.
#[test]
fn names_with_one_name_hash_take_bounded_probes() {
    const N: usize = 10_000;
    let names = names_with_one_name_hash(N);
    let hash = blaze_phf::name_hash(names[0].as_bytes());
    assert!(names.iter().all(|name| blaze_phf::name_hash(name.as_bytes()) == hash));
    assert_eq!(names.iter().collect::<std::collections::BTreeSet<_>>().len(), N, "distinct names");

    let table = Table::new(shape());
    probes::take();
    let ids: Vec<u32> = names.iter().map(|name| table.intern(name.as_bytes())).collect();
    let interned = probes::take();
    for (name, &id) in names.iter().zip(&ids) {
        assert_eq!(table.find(name.as_bytes()), Some(id));
    }
    let found = probes::take();
    let bound = 16 * N as u64;
    for (what, probes) in [("interning", interned), ("finding", found)] {
        assert!(
            probes <= bound,
            "{what} {N} names with one name_hash loaded {probes} control words, over {bound}"
        );
    }
}

/// Interning a value the table holds, finding it and reading it allocate nothing: from the
/// compiled-in values and from the ones this process made.
#[test]
fn hits_allocate_nothing() {
    let table = Table::new(shape());
    let mut model = HashMap::new();
    let mut rng = SplitMix64::new(41);
    for _ in 0..500 {
        let text = name(&mut rng);
        model.insert(table.intern(text.as_bytes()), text);
    }
    for (&id, text) in &model {
        let text = text.as_bytes();
        blaze_alloc_count::assert_at_most(0, "a made value", || {
            assert_eq!(table.intern(text), id);
            assert_eq!(table.find(text), Some(id));
            assert_eq!(table.get(id), Some(text));
        });
    }
    blaze_alloc_count::assert_at_most(0, "a compiled-in value", || {
        assert_eq!(table.intern(b"a"), 1);
        assert_eq!(table.find(b"b"), Some(2));
        assert_eq!(table.get(3), Some(&b"a"[..]));
    });
}

/// Interning new values into a table allocates nothing: the overlay's bytes come from the
/// store's region and its ID column is a reserved region of its own.
#[test]
fn misses_allocate_nothing() {
    let table = Table::new(shape());
    table.intern(b"warm");
    let ids = blaze_alloc_count::assert_at_most(0, "interning new values", || {
        (0..20_000u32)
            .map(|n| table.intern(&n.to_le_bytes()))
            .fold(0u64, |sum, id| sum + u64::from(id))
    });
    assert!(ids > 0);
    for n in 0..20_000u32 {
        assert_eq!(
            table.find(&n.to_le_bytes()).and_then(|id| table.get(id)),
            Some(&n.to_le_bytes()[..])
        );
    }
}

/// IDs set from many threads at once, in no order and far apart, read back from every thread.
#[test]
fn the_id_column_publishes_each_slot_once() {
    let table = Table::new(shape());
    std::thread::scope(|scope| {
        for worker in 0..8u32 {
            let table = &table;
            scope.spawn(move || {
                for n in 0..5_000u32 {
                    let bytes = (worker << 24 | n).to_le_bytes();
                    let id = table.intern(&bytes);
                    assert_eq!(table.get(id), Some(&bytes[..]));
                }
            });
        }
    });
    assert_eq!(table.end() - table.first, 40_000);
}
