use std::{
    panic::AssertUnwindSafe,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use super::*;
use crate::db::Storage;

struct Db(Storage);

impl Database for Db {
    fn storage(&self) -> &Storage {
        &self.0
    }

    fn storage_mut(&mut self) -> &mut Storage {
        &mut self.0
    }
}

fn name() -> SlotName {
    SlotName { maintainer: "impls", what: "the impl directory", of: "crate core".into() }
}

#[test]
fn a_second_different_write_of_a_slot_is_refused_naming_it() {
    let slot = Slot::empty(name());
    assert_eq!(slot.fill(7_u32), Ok(Filled::First));
    assert_eq!(slot.fill(7), Ok(Filled::Again), "an equal value again is no second write");
    let refused = slot.fill(8).unwrap_err();
    assert_eq!(refused, SlotRefused { slot: name() });
    let text = refused.to_string();
    for part in ["`the impl directory`", "crate core", "`impls`", "different value"] {
        assert!(text.contains(part), "the refusal names {part}: {text}");
    }
    let db = Db(Storage::new(None));
    assert_eq!(slot.get(&db), Ok(&7), "the first value stays");
}

#[test]
fn a_slot_no_task_filled_is_an_error_naming_it_never_a_wait() {
    let db = Db(Storage::new(None));
    let slot = Slot::<u32>::empty(name());
    assert_eq!(slot.get(&db), Err(SlotError::Unclaimed { slot: name() }));
    let claim = slot.claim(&db).unwrap();
    // Another task (a thread of its own) finds it busy; this one, claiming twice, is told so.
    std::thread::scope(|scope| {
        let other = scope.spawn(|| slot.claim(&db).err());
        assert_eq!(other.join().unwrap(), Some(SlotError::Busy { slot: name() }));
    });
    let again = slot.claim(&db).err();
    assert_eq!(again, Some(SlotError::ClaimedAgain { slot: name() }));
    assert!(
        again.unwrap().to_string().contains("`the impl directory`"),
        "the error names the slot"
    );
    drop(claim);
    assert_eq!(slot.get(&db), Err(SlotError::Abandoned { slot: name() }));
}

#[test]
fn a_reader_waits_for_the_claim_and_reads_its_fill() {
    let db = Db(Storage::new(None));
    let slot = Slot::<u32>::empty(name());
    let claim = slot.claim(&db).unwrap();
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| slot.get(&db).copied());
        // Bounded: the reader links itself as a waiter within the deadline, or the test fails.
        let deadline = Instant::now() + Duration::from_secs(10);
        while db.0.flights.waiters(key(slot.serial)) == 0 {
            assert!(Instant::now() < deadline, "the reader never waited on the claim");
            std::thread::yield_now();
        }
        assert_eq!(claim.fill(3), Ok(Filled::First));
        assert_eq!(reader.join().unwrap(), Ok(3));
    });
    assert_eq!(slot.claim(&db).err(), Some(SlotError::Full { slot: name() }));
}

/// A claimer takes the flight before it marks the slot claimed: a read in between must say
/// `Unclaimed` from the mark, never run the flight's empty "run", which could hold the flight
/// just as the claimer takes it and fail its claim with `Busy`. Here another task holds the
/// flight by hand with the slot unmarked; the read must not wait on it.
#[test]
fn a_read_of_a_slot_no_task_claimed_never_takes_its_flight() {
    let db = Db(Storage::new(None));
    let slot = Slot::<u32>::empty(name());
    let (held, stop, released) =
        (AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let hold = db.0.flights.hold(key(slot.serial)).unwrap();
            held.store(true, Ordering::Release);
            // Bounded: let go after the deadline even if the reader never ends its read.
            let deadline = Instant::now() + Duration::from_secs(10);
            while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            released.store(true, Ordering::Release);
            drop(hold);
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !held.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the holder never held the flight");
            std::thread::yield_now();
        }
        assert_eq!(slot.get(&db), Err(SlotError::Unclaimed { slot: name() }));
        assert!(
            !released.load(Ordering::Acquire),
            "the read waited on a flight no claim of the slot holds"
        );
        stop.store(true, Ordering::Release);
    });
}

/// The task that holds a slot's claim reading it before it filled it would wait on itself: an
/// error naming the slot, never the flights' panic that names a hex key.
#[test]
fn a_read_by_the_slots_own_filler_is_an_error_naming_the_slot() {
    let db = Db(Storage::new(None));
    let slot = Slot::<u32>::empty(name());
    let claim = slot.claim(&db).unwrap();
    let read = slot.get(&db).err();
    assert_eq!(read, Some(SlotError::ReadByItsFiller { slot: name() }));
    assert!(read.unwrap().to_string().contains("`the impl directory`"), "the error names the slot");
    assert_eq!(claim.fill(4), Ok(Filled::First));
    assert_eq!(slot.get(&db), Ok(&4), "once filled, its filler reads it too");
}

/// A filler whose wait gave way gives its claim back to run again: a read meanwhile gives way
/// (a wait for the rerun, which comes after the read's loop), and the rerun claims and fills it.
#[test]
fn a_claim_given_back_makes_its_readers_give_way_until_the_rerun_fills_it() {
    let db = Db(Storage::new(None));
    let slot = Slot::<u32>::empty(name());
    slot.claim(&db).unwrap().give_back();
    let read = Cancelled::catch(AssertUnwindSafe(|| slot.get(&db).copied()));
    assert!(
        matches!(read, Err(Cancelled::WouldWaitOnItself { .. })),
        "a read of a given-back slot gives way: {read:?}"
    );
    let again = slot.claim(&db).expect("the rerun claims the slot anew");
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| slot.get(&db).copied());
        let deadline = Instant::now() + Duration::from_secs(10);
        while db.0.flights.waiters(key(slot.serial)) == 0 {
            assert!(Instant::now() < deadline, "the reader never waited on the rerun's claim");
            std::thread::yield_now();
        }
        assert_eq!(again.fill(5), Ok(Filled::First));
        assert_eq!(reader.join().unwrap(), Ok(5));
    });
}

/// sched3's C5: a pass that fails marks every slot it left unfilled abandoned, whether its
/// claim was given back for a rerun the failed pass will never make or never taken: a reader
/// reads `Abandoned` naming the slot at once, never gives way again. A slot claimed or full is
/// left as it is, and a later pass claims an abandoned one anew.
#[test]
fn a_failed_pass_abandons_its_unfilled_slots_so_readers_never_give_way() {
    let db = Db(Storage::new(None));
    let given = Slot::<u32>::empty(name());
    given.claim(&db).unwrap().give_back();
    let never = Slot::<u32>::empty(name());
    let held = Slot::<u32>::empty(name());
    let claim = held.claim(&db).unwrap();
    let full = Slot::full(name(), 9_u32);
    assert!(given.abandon(), "a given-back slot is abandoned");
    assert!(never.abandon(), "an unclaimed slot is abandoned");
    assert!(!held.abandon(), "a claimed slot is its claimer's to end");
    assert!(!full.abandon(), "a full slot keeps its value");
    for slot in [&given, &never] {
        let read = Cancelled::catch(AssertUnwindSafe(|| slot.get(&db).copied()));
        assert_eq!(
            read,
            Ok(Err(SlotError::Abandoned { slot: name() })),
            "a reader reads Abandoned at once"
        );
    }
    assert_eq!(full.get(&db), Ok(&9));
    assert_eq!(claim.fill(1), Ok(Filled::First));
    let again = given.claim(&db).expect("a later pass claims an abandoned slot anew");
    assert_eq!(again.fill(2), Ok(Filled::First));
    assert_eq!(given.get(&db), Ok(&2));
}
