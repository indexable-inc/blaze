//! The command's store on the plain store, on blaze-core's simulation (rules 43 and 64): each
//! property has an oracle and, where this module adds the property, a planted bug it catches.
//! The plain store's own durability (torn segments, the flip, compaction's grace, the lazy open)
//! and `Lent`'s address promise are proven by blaze-cas (plain/tests.rs, plain/lazy_tests.rs,
//! plain/lent_tests.rs); these prove what this module adds on `Driven<Sim>`: records read in
//! place and reached by the next process through the commit after the answer, two values
//! refused, a crash that loses a command's records but never serves other bytes, and a pin
//! that keeps a running program's records through compaction past the cap.
//! The oracle is the model `value(n)` under `key(n)` (rule 8), never another store's answer.

use blaze_cas::plain::{Lent, LentError, Settings};
use blaze_core::FileName;
use blaze_core::sim::{Faults, Sim};
use blaze_drive::Key;
use blaze_ids::ContentId;
use std::alloc::Global;

use super::{Facts, PlaceError, PlacePlant, PlainPlace, after_answer, config, open};

fn key(n: u32) -> Key {
    Key(ContentId::of("plain place test key", &n.to_le_bytes()))
}

/// Record `n`'s one value (rule 21): 1 to 300 bytes, some ending inside a block.
fn value(n: u32) -> Vec<u8> {
    (0..1 + (n as usize * 37) % 300).map(|i| (n as usize).wrapping_mul(131).wrapping_add(i) as u8).collect()
}

/// Process `n` of one boot over `sim`, with the store's cap at `cap_bytes`.
fn process(sim: Sim, n: u64, cap_bytes: u64) -> Lent<Sim, Global> {
    let mut cfg = config(FileName::new("store").expect("a name"), &Settings::default(), Facts { boot: Some(1), nonce: 9000 + n });
    cfg.cap_bytes = cap_bytes;
    open(sim, cfg).expect("the store opens")
}

fn back(lent: Lent<Sim, Global>) -> Sim {
    lent.into_store().into_driver()
}

/// What `place` holds under `n`: the bytes, copied for the oracle.
fn read(place: &PlainPlace<'_, Sim>, n: u32) -> Option<Vec<u8>> {
    place.get(key(n)).expect("a get with no failure").map(|held| held.bytes.to_vec())
}

/// A command's records are read in place from its arena before the commit, reach the next
/// process through the commit after the answer, and are read there in place from a segment;
/// other bytes under a committed key are refused by name (rule 21).
#[test]
fn records_are_read_in_place_and_reach_the_next_process_by_the_commit_after_the_answer() {
    let lent = process(Sim::new(1, Faults::default()), 1, u64::MAX);
    let mut place = PlainPlace::new(&lent, false);
    for n in 0..100 {
        place.put(key(n), false, &value(n)).expect("a put");
        assert_eq!(read(&place, n), Some(value(n)), "record {n} from the arena");
    }
    drop(place);
    let first = after_answer(&lent, 0).expect("the commit after the answer");
    assert_eq!(first.segments_written, 1);
    let lent = process(back(lent), 2, u64::MAX);
    let mut place = PlainPlace::new(&lent, false);
    for n in 0..100 {
        assert_eq!(read(&place, n), Some(value(n)), "record {n} in the next process");
    }
    let counts = place.counts();
    assert_eq!((counts.lent.from_segments, counts.lent.from_puts), (100, 0), "{counts:?}");
    assert_eq!(read(&place, 5000), None);
    match place.put(key(7), false, &value(8)) {
        Err(PlaceError::Lent(LentError::TwoValues { .. })) => {}
        other => panic!("other bytes under a committed key were not refused: {other:?}"),
    }
    place.put(key(7), false, &value(7)).expect("equal bytes under a held key are left as they are");
    println!("plain place: 100 records read in place from the arena, then from a segment in the next process; two values refused");
}

/// A crash at any point of a run of commands (the simulation's crash tears unsynced writes):
/// every key the next process holds has its own value, never other bytes. Many seeds.
#[test]
fn a_crash_loses_a_commands_records_but_never_serves_other_bytes() {
    let mut lost_some = false;
    for seed in 0..32u64 {
        let mut sim = Sim::new(100 + seed, Faults::default());
        for cmd in 0..4u32 {
            let lent = process(sim, u64::from(cmd) + 1, u64::MAX);
            let mut place = PlainPlace::new(&lent, false);
            for n in cmd * 20..cmd * 20 + 20 {
                place.put(key(n), false, &value(n)).expect("a put");
            }
            // The seed decides whether this command reached its commit.
            drop(place);
            if (seed >> cmd) & 1 == 1 {
                after_answer(&lent, 1).expect("the commit after the answer");
            }
            sim = back(lent);
        }
        sim.crash();
        let lent = process(sim, 99, u64::MAX);
        let place = PlainPlace::new(&lent, false);
        for n in 0..80 {
            match read(&place, n) {
                Some(bytes) => assert_eq!(bytes, value(n), "seed {seed}: record {n} served other bytes after a crash"),
                None => lost_some = true,
            }
        }
    }
    assert!(lost_some, "no seed lost a record: the crash was not exercised");
    println!("plain place: 32 seeds crashed; every record held had its own bytes");
}

/// A pin keeps a running program's records through compaction past the cap: records of an old
/// segment that no root names are pruned (shown, so the pruning ran), every pinned one stays.
/// The planted pin that does nothing is caught by the same oracle.
fn pinned_after_compaction(plant: PlacePlant) -> (Vec<u32>, usize) {
    let lent = process(Sim::new(7, Faults::default()), 1, 1);
    let mut place = PlainPlace::planted(&lent, false, plant);
    // The first commit: pinned 0..20, unpinned 100..120.
    for n in (0..20).chain(100..120) {
        place.put(key(n), false, &value(n)).expect("a put");
    }
    let pinned: Vec<Key> = (0..20).map(key).collect();
    place.pin("program", &pinned).expect("a pin");
    place.commit().expect("the first commit");
    // More generations than the compaction's window, each a commit of its own.
    for gen in 0..20u32 {
        for n in 1000 + gen * 4..1004 + gen * 4 {
            place.put(key(n), false, &value(n)).expect("a put");
        }
        place.commit().expect("a later commit");
    }
    drop(place);
    after_answer(&lent, 64).expect("compaction past the cap");
    let lent = process(back(lent), 2, 1);
    let place = PlainPlace::new(&lent, false);
    let missing: Vec<u32> = (0..20).filter(|n| read(&place, *n) != Some(value(*n))).collect();
    let pruned = (100..120).filter(|n| read(&place, *n).is_none()).count();
    (missing, pruned)
}

#[test]
fn a_pin_keeps_a_programs_records_through_compaction_past_the_cap() {
    let (missing, pruned) = pinned_after_compaction(PlacePlant::None);
    assert!(pruned > 0, "nothing unpinned was pruned: the compaction past the cap did not run");
    assert!(missing.is_empty(), "pinned records lost to compaction: {missing:?}");
    println!("plain place: compaction past the cap pruned {pruned} unpinned records and kept all 20 pinned");
    let (missing, _) = pinned_after_compaction(PlacePlant::ForgetsPin);
    assert!(!missing.is_empty(), "the planted pin that does nothing went unseen");
    println!("plain place, planted ForgetsPin: caught ({} pinned records lost)", missing.len());
}
