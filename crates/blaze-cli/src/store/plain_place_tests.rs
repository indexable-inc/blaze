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

use super::{Facts, PlaceError, PlacePlant, PlainPlace, StoreAlloc, after_answer, config, open, test_cx};

fn key(n: u32) -> Key {
    Key(ContentId::of("plain place test key", &n.to_le_bytes()))
}

/// Record `n`'s one value (rule 21): 1 to 300 bytes, some ending inside a block.
fn value(n: u32) -> Vec<u8> {
    (0..1 + (n as usize * 37) % 300).map(|i| (n as usize).wrapping_mul(131).wrapping_add(i) as u8).collect()
}

/// Process `n` of the first boot over `sim`, with the store's cap at `cap_bytes`.
fn process(sim: Sim, n: u64, cap_bytes: u64) -> Lent<Sim, StoreAlloc> {
    process_in_boot(sim, n, cap_bytes, 1)
}

/// Process `n` of boot `boot` over `sim`. A `Sim::crash` is a power cut, so the process after
/// it is of the next boot: the boot fact is the driver's, and a machine that lost power boots
/// again (blaze-cas plain/tests.rs takes `boot + 1` after each crash). Opened in the same boot,
/// the store takes its own segments as proved by their rows (no byte read) and a torn one is
/// `StoreError::Changed` (something outside blaze changed it), not a loss.
fn process_in_boot(sim: Sim, n: u64, cap_bytes: u64, boot: u64) -> Lent<Sim, StoreAlloc> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut cfg = config(FileName::new("store").expect("a name"), &Settings::default(), Facts { boot: Some(boot), nonce: 9000 + n }, alloc);
    cfg.cap_bytes = cap_bytes;
    open(sim, cfg, alloc).expect("the store opens")
}

fn back(lent: Lent<Sim, StoreAlloc>) -> Sim {
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
    let first = after_answer(&test_cx(), &lent, 0).expect("the commit after the answer");
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

/// A refused put names the record (rule 19): its kind and subject as the drive prints names,
/// each side's form, the first field the two differ in and, for a payload, its first differing
/// byte; a read list that differs names its first differing read. Prints
/// `plain place: a refused put names CrateScope(020202020202): payload at byte 2, reads at UnitGraph(050505050505)`.
#[test]
fn a_refused_put_names_its_record_and_where_the_two_differ() {
    use crate::store::{Clash, Form};
    use blaze_drive::record::write_plain;
    use blaze_keyed::store::{Holder, TwoValues};
    use blaze_drive::{Kind, Name};
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let name = Name::new(Kind::CrateScope, ContentId([2; 32]));
    let graph = Name::new(Kind::UnitGraph, ContentId([5; 32]));
    let held = write_plain(name, ContentId([6; 32]), ContentId([7; 32]), &[], &[1, 2, 3, 4], alloc);
    let other = write_plain(name, ContentId([6; 32]), ContentId([7; 32]), &[], &[1, 2, 9, 4], alloc);
    let lent = process(Sim::new(3, Faults::default()), 1, u64::MAX);
    let mut place = PlainPlace::new(&lent, false);
    place.put(key(1), false, &held).expect("the first put");
    place.put(key(1), false, &held).expect("equal bytes under a held key are left as they are");
    let refused = match place.put(key(1), false, &other) {
        Err(PlaceError::Lent(refused @ LentError::TwoValues { two: TwoValues { held: Holder::ThisProcess, differs_at: Some(_), .. }, .. })) => refused,
        put => panic!("other bytes under an own key were not refused with both sides: {put:?}"),
    };
    let kept = place.get(key(1)).expect("a get").map(|held| held.bytes);
    let clash = Clash::of(kept, &other);
    assert_eq!((clash.name, clash.held, clash.put, clash.field, clash.payload_at), (Some(name), Form::Whole, Form::Whole, "payload", Some(2)), "{clash}");
    let shown = clash.to_string();
    assert!(shown.starts_with(&std::format!("{name}")) && shown.contains("first differing in payload at payload byte 2"), "{shown}");
    // The command's printed line: the record by kind, subject and crate, both sizes, which side
    // is held, the first differing byte of the whole record, the rule and what to run.
    let mut labelled = Clash::of(kept, &other);
    labelled.label = Some({
        let mut label = blaze_own::String::new_in(Global);
        label.push_str("crate hello_world");
        label
    });
    let labelled = |clash: &mut Clash| {
        let mut label = blaze_own::String::new_in(Global);
        label.push_str("crate hello_world");
        clash.label = Some(label);
    };
    let at = blaze_cas::plain::first_difference(&held, &other).expect("the two records differ");
    let says = |refused: LentError, kept: Option<&[u8]>, side: &str, go_on: &str| {
        let mut clash = Clash::of(kept, &other);
        labelled(&mut clash);
        let line = crate::store::StoreFailure::TwoValues { key: key(1), lent: refused, clash }.to_string();
        for words in [
            std::format!("two values under one key {}: the record of {name} (crate hello_world) made now ({} bytes)", key(1), other.len()),
            std::format!("differs from the one the store holds ({} bytes, {side}), first at byte {at}", held.len()),
            "first differing in payload at payload byte 2".into(),
            "(rule 21: its key misses an input of its value). This is a bug in blaze: please report this line.".into(),
            go_on.into(),
        ] {
            assert!(line.contains(words.as_str()), "the refusal does not say {words:?}:\n{line}");
        }
        line
    };
    let own = says(refused, kept, "put earlier by this command", "so `blaze clean --store` does not get past it");
    println!("plain place: {own}");
    // Against a committed record (an earlier command's) the line says what to run.
    drop(place);
    after_answer(&test_cx(), &lent, 0).expect("the commit after the answer");
    let lent = process(back(lent), 2, u64::MAX);
    let mut place = PlainPlace::new(&lent, false);
    let refused = match place.put(key(1), false, &other) {
        Err(PlaceError::Lent(refused @ LentError::TwoValues { two: TwoValues { held: Holder::Committed, differs_at: Some(_), .. }, .. })) => refused,
        put => panic!("other bytes under a committed key were not refused with both sides: {put:?}"),
    };
    let kept = place.get(key(1)).expect("a get").map(|held| held.bytes);
    let committed = says(refused, kept, "committed", "To go on: `blaze clean --store`, then run the command again.");
    println!("plain place: {committed}");
    // Two reads lists under one key: the first read apart is named.
    let a = write_plain(name, ContentId([6; 32]), ContentId([7; 32]), &[(graph, ContentId([8; 32]))], &[], alloc);
    let b = write_plain(name, ContentId([6; 32]), ContentId([7; 32]), &[], &[], alloc);
    let reads = Clash::of(Some(&a[..]), &b);
    assert_eq!((reads.field, reads.names), ("reads", Some((Some(graph), None))), "{reads}");
    println!("plain place: a refused put names {name}: payload at byte 2, reads at {graph}");
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
                after_answer(&test_cx(), &lent, 1).expect("the commit after the answer");
            }
            sim = back(lent);
        }
        sim.crash();
        let lent = process_in_boot(sim, 99, u64::MAX, 2);
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

/// A pin keeps a running program's records through cleanup past the cap: records no root names
/// are swept (shown, so the sweep ran), every pinned one stays. Each process folds one slice
/// into its commit after the answer; two processes, so a store that keeps a command's records by
/// its lease until that commit (`Liveness::Reached`) sweeps them at the second. The planted pin
/// that does nothing is caught by the same oracle.
fn pinned_after_compaction(plant: PlacePlant) -> (Vec<u32>, usize) {
    // Past the whole store's bytes (about 30 KiB here), never past one commit's.
    const CAP: u64 = 16 << 10;
    let lent = process(Sim::new(7, Faults::default()), 1, CAP);
    let mut place = PlainPlace::planted(&lent, false, plant);
    // The first commit: pinned 0..20, unpinned 100..120.
    for n in (0..20).chain(100..120) {
        place.put(key(n), false, &value(n)).expect("a put");
    }
    let pinned: Vec<Key> = (0..20).map(key).collect();
    place.pin("program", &pinned).expect("a pin");
    place.commit().expect("the first commit");
    // More generations than the compaction's window, each a commit of its own.
    for round in 0..20u32 {
        for n in 1000 + round * 4..1004 + round * 4 {
            place.put(key(n), false, &value(n)).expect("a put");
        }
        place.commit().expect("a later commit");
    }
    drop(place);
    after_answer(&test_cx(), &lent, 1).expect("the first process's commit after the answer");
    let lent = process(back(lent), 2, CAP);
    after_answer(&test_cx(), &lent, 1).expect("the second process's slice past the cap");
    let lent = process(back(lent), 3, CAP);
    let place = PlainPlace::new(&lent, false);
    let missing: Vec<u32> = (0..20).filter(|n| read(&place, *n) != Some(value(*n))).collect();
    let pruned = (100..120).chain(1000..1080).filter(|n| read(&place, *n).is_none()).count();
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
