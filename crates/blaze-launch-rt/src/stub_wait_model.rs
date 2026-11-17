//! The stub waiters under the interleaving checker (`RUSTFLAGS="--cfg blaze_check" cargo test
//! -p blaze-launch-rt --release stub_wait`): two claimers and one answerer over the ring, every
//! interleaving to the checker's preemption bound and every value each load may read.
//!
//! The oracle: a claimer whose wait ends BOUND reads what the answerer recorded for its row,
//! the slot the answerer popped with it; it must be the claimer's own slot (else the control
//! thread answered BOUND for a slot the row did not hold yet, and would bind the wrong code).
//! The planted bug (`Plant::PushBeforeSlot`, rule 53): the claim pushes the row's index before
//! it writes the row's slot, so the answerer can pop the row and store BOUND for slot 0 before
//! the slot is written; some execution must show it. The kernel's wait is the module's yield
//! loop here (stub_wait.rs, "Proof"); the deadline is a step clock, so a run the answerer
//! gave up on ends `Late`, never hangs.

use core::cell::Cell;
use core::num::NonZeroU32;
use core::time::Duration;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use blaze_check::sync::atomic::{AtomicU32, Ordering};
use blaze_check::thread;
use blaze_core::wait::{Clock, Deadline};

use super::{Answer, Plant, Waiters};

/// The two claimers' slots: nonzero, so a row's unwritten slot (0) is told from either.
const SLOTS: [u32; 2] = [5, 9];

/// Looks each side takes before it gives up: the claimer's deadline on its step clock, and
/// the answerer's pops (times two), a yield after each empty one.
const TRIES: u64 = 4;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Seen {
    /// No answer by the deadline.
    Late,
    /// BOUND, answered for the claimer's own slot.
    Right,
    /// BOUND for another slot, or another answer than BOUND.
    Wrong,
}

/// A clock that moves one millisecond each read: deterministic, as the checker asks.
struct Steps(Cell<u64>);

impl Clock for Steps {
    fn now(&self) -> Duration {
        let now = self.0.get();
        self.0.set(now + 1);
        Duration::from_millis(now)
    }
}

/// The outcomes over every execution, and how many executions the checker ran.
fn outcomes(plant: Plant) -> (BTreeSet<Seen>, u64) {
    let seen = Arc::new(Mutex::new((BTreeSet::new(), 0u64)));
    let into = Arc::clone(&seen);
    blaze_check::model(move || {
        let most = NonZeroU32::new(2).expect("two rows");
        let waiters = Arc::new(Waiters::reserve(most).expect("the waiters' mapping"));
        // By row: the slot the answerer popped with it, plus one (0: not answered).
        let told = Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);
        let claimers = SLOTS.map(|slot| {
            let waiters = Arc::clone(&waiters);
            let told = Arc::clone(&told);
            thread::spawn(move || {
                let row = waiters.claim_with(slot, u64::from(slot), plant).expect("a free row of two");
                let clock = Steps(Cell::new(0));
                match waiters.wait(row, Deadline::at(Duration::from_millis(TRIES)), &clock) {
                    Ok(Answer::Bound) => match told[row as usize].load(Ordering::Relaxed) == slot + 1 {
                        true => Seen::Right,
                        false => Seen::Wrong,
                    },
                    Ok(Answer::Refused | Answer::Lost) => Seen::Wrong,
                    Err(_) => Seen::Late,
                }
            })
        });
        let mut answered = 0;
        for _ in 0..TRIES * 2 {
            match waiters.popped() {
                Some(waiting) => {
                    told[waiting.row as usize].store(waiting.slot + 1, Ordering::Relaxed);
                    // Release (inside `answer`): the claimer's acquire of its state sees `told`.
                    waiters.answer(waiting.row, Answer::Bound);
                    answered += 1;
                    if answered == SLOTS.len() {
                        break;
                    }
                }
                None => thread::yield_now(),
            }
        }
        let outcomes = claimers.map(|claimer| claimer.join().expect("a claimer"));
        let mut held = into.lock().expect("the outcome set");
        held.0.extend(outcomes);
        held.1 += 1;
    });
    let out = seen.lock().expect("the outcome set").clone();
    out
}

/// The clean claim: no execution answers BOUND for a slot the row does not hold, some
/// execution answers both claimers rightly, and the checker ran more than one execution
/// (else nothing here was checked).
/// Prints `test stub_wait::model::stub_waiters_answer_only_the_slot_each_row_holds ... ok`.
#[test]
fn stub_waiters_answer_only_the_slot_each_row_holds() {
    let (seen, executions) = outcomes(Plant::None);
    assert!(executions > 1, "the checker ran {executions} execution(s): the waiters' atomics or the spawn are not its own (outcomes {seen:?})");
    assert!(!seen.contains(&Seen::Wrong), "the clean claim was answered for another slot: {seen:?} over {executions} executions");
    assert!(seen.contains(&Seen::Right), "no claimer was ever answered: {seen:?} over {executions} executions");
}

/// The control (rule 53): with the index pushed before the slot is written, some execution's
/// answerer stores BOUND for a slot the row does not hold yet, and the oracle sees it.
/// Prints `test stub_wait::model::stub_waiters_control_answers_an_unwritten_slot ... ok`.
#[test]
fn stub_waiters_control_answers_an_unwritten_slot() {
    let (seen, executions) = outcomes(Plant::PushBeforeSlot);
    assert!(executions > 1, "the checker ran {executions} execution(s): the waiters' atomics or the spawn are not its own (outcomes {seen:?})");
    assert!(seen.contains(&Seen::Wrong), "the checker never saw the planted push-before-slot answered wrongly: {seen:?} over {executions} executions");
}
