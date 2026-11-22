//! The ring's publish under the interleaving checker, with the consumer WAITING for the entry
//! (`RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-launch-rt --release ring_wait`). The earlier
//! shape (ring_outcomes.rs: one pop before the join) only ever saw an empty ring, so the producer
//! never ran before the pop: either the model never reorders a spawned thread ahead of a load
//! without a schedule point it is forced to take, or the model is not running at all. Here the
//! consumer yields between pops (a schedule point that hands over to the producer at no preemption
//! cost), and each test counts the executions the model ran, so a model that is not running fails
//! with its own words instead of passing vacuously.
//!
//! Clean ring: every execution's consumer gets the whole entry. Planted ring
//! (`Plant::PublishBeforeWrite`): some execution's consumer gets a torn one, by interleaving (the
//! consumer runs between the publish and the write) or by a load choice (the slot's fields are
//! written after the release the consumer's acquire reads, so their old value is still readable).
//! Pure: no sampler, no signal; the model's threads are coroutines.

use std::alloc::Global;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use blaze_check::thread;
use blaze_tier::sample::SampleEntry;

use super::ring::{Plant, Ring};

/// Pops the consumer tries, a yield after each empty one.
const TRIES: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Seen {
    /// No entry within `TRIES` pops.
    Late,
    Whole,
    Torn,
}

/// Every field set and none zero, so a slot's old contents (zeros) are told from it.
const ENTRY: SampleEntry = SampleEntry { pc: 0x1001, lr: 0x2002, fp: 0x3003, frame_ret: 0x4004, word: 0x5005, thread: 7 };

/// The outcomes over every execution, and how many executions the model ran.
fn outcomes(plant: Plant) -> (BTreeSet<Seen>, u64) {
    let seen = Arc::new(Mutex::new((BTreeSet::new(), 0u64)));
    let into = Arc::clone(&seen);
    blaze_check::model(move || {
        let ring = Arc::new(Ring::try_new_in(1, Global).expect("a ring of one slot"));
        let producer = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                ring.push_with(ENTRY, plant);
            })
        };
        let mut outcome = Seen::Late;
        for _ in 0..TRIES {
            match ring.pop() {
                Some(entry) if entry == ENTRY => {
                    outcome = Seen::Whole;
                    break;
                }
                Some(_) => {
                    outcome = Seen::Torn;
                    break;
                }
                None => thread::yield_now(),
            }
        }
        producer.join().expect("the producer");
        let mut held = into.lock().expect("the outcome set");
        held.0.insert(outcome);
        held.1 += 1;
    });
    let out = seen.lock().expect("the outcome set").clone();
    out
}

/// The clean ring: never a torn entry, the whole entry in some execution, and more than one
/// execution (else the model did not run the threads as its own and nothing here was checked).
/// Prints `test sample::ring_wait::ring_wait_sees_only_whole_entries ... ok`.
#[test]
fn ring_wait_sees_only_whole_entries() {
    let (seen, executions) = outcomes(Plant::None);
    assert!(executions > 1, "the model ran {executions} execution(s): the ring's atomics or the spawn are not the model's (outcomes {seen:?})");
    assert!(!seen.contains(&Seen::Torn), "the clean ring published a torn entry: {seen:?} over {executions} executions");
    assert!(seen.contains(&Seen::Whole), "the consumer never got the entry: {seen:?} over {executions} executions");
}

/// The control (rule 53). Prints `test sample::ring_wait::ring_wait_control_sees_a_torn_entry ... ok`.
#[test]
fn ring_wait_control_sees_a_torn_entry() {
    let (seen, executions) = outcomes(Plant::PublishBeforeWrite);
    assert!(executions > 1, "the model ran {executions} execution(s): the ring's atomics or the spawn are not the model's (outcomes {seen:?})");
    assert!(seen.contains(&Seen::Torn), "the model never saw the planted publish-before-write torn: {seen:?} over {executions} executions");
}
