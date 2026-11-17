//! The stub's wait on the real kernel wait: many waiters on one slot, the deadline, and the
//! allocation count of the stub's path (THREADS.md D6). The claim, ring and answer under the
//! interleaving checker are stub_wait_model.rs.
//!
//! Run: `cargo test -p blaze-launch-rt --lib stub_wait` (by name: on Linux this binary's waits
//! are given to blaze-core's direct driver by the first of these tests, which `choose` refuses
//! once another test of the same process has waited on the ring).

use core::cell::Cell;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::sync::Once;

use blaze_core::wait::{Clock, Deadline, Monotonic};

use super::{Answer, Overran, Waiters, Waiting};

/// Counts each test thread's allocations: test 3's measure.
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The launcher's start chooses blaze-core's direct driver (a program thread has no wait
/// ring); this binary does the same, once, before its first wait.
fn direct() {
    static CHOSEN: Once = Once::new();
    CHOSEN.call_once(|| {
        #[cfg(target_os = "linux")]
        blaze_core::wait::choose(blaze_core::wait::Waits::Direct);
    });
}

/// The longest any wait of these tests is given: far past what each needs, so only a hang
/// reaches it (rule 18).
const LONG: Duration = Duration::from_secs(20);

fn rows(most: u32) -> Waiters {
    Waiters::reserve(NonZeroU32::new(most).expect("a nonzero row count")).expect("the waiters' mapping")
}

/// Pops `count` rows within `LONG`, yielding while the ring is empty.
fn pop_all<const N: usize>(waiters: &Waiters) -> [Waiting; N] {
    let clock = Monotonic::new();
    let deadline = Deadline::after(clock.now(), LONG);
    let mut got = [Waiting { row: u32::MAX, slot: 0, thread: 0 }; N];
    let mut count = 0;
    while count < N {
        match waiters.popped() {
            Some(waiting) => {
                got[count] = waiting;
                count += 1;
            }
            None => {
                assert!(!deadline.passed(clock.now()), "only {count} of {N} rows came through the ring within {LONG:?}");
                std::thread::yield_now();
            }
        }
    }
    got
}

/// Eight threads claim rows for ONE slot and wait in the kernel; a control thread pops eight
/// indices from the ring and answers BOUND once each: every waiter returns BOUND, the eight
/// rows are distinct and each names the slot and its thread, and the ring is then empty.
/// Doc: "eight waiters on one slot, eight answers, none lost".
/// Prints `test stub_wait::tests::eight_waiters_on_one_slot_eight_answers_none_lost ... ok`.
#[test]
fn eight_waiters_on_one_slot_eight_answers_none_lost() {
    const SLOT: u32 = 41;
    direct();
    let waiters = rows(8);
    let answers: [Result<Answer, Overran>; 8] = std::thread::scope(|scope| {
        let handles: [_; 8] = core::array::from_fn(|index| {
            let waiters = &waiters;
            scope.spawn(move || {
                let clock = Monotonic::new();
                let row = waiters.claim(SLOT, 100 + index as u64).expect("a free row of eight");
                let answer = waiters.wait(row, Deadline::after(clock.now(), LONG), &clock);
                if answer.is_ok() {
                    waiters.release(row);
                }
                answer
            })
        });
        let control = scope.spawn(|| {
            let popped: [Waiting; 8] = pop_all(&waiters);
            for waiting in popped {
                assert_eq!(waiting.slot, SLOT, "row {} names slot {}, not the one its thread waits for", waiting.row, waiting.slot);
                waiters.answer(waiting.row, Answer::Bound);
            }
            popped
        });
        let popped = control.join().expect("the control thread");
        let mut rows = popped.map(|it| it.row);
        rows.sort_unstable();
        assert!(rows.windows(2).all(|pair| pair[0] != pair[1]), "a row was popped twice: {rows:?}");
        let mut threads = popped.map(|it| it.thread);
        threads.sort_unstable();
        assert_eq!(threads, core::array::from_fn::<u64, 8, _>(|index| 100 + index as u64), "a thread's row was lost or named another");
        handles.map(|handle| handle.join().expect("a waiting thread"))
    });
    for (index, answer) in answers.iter().enumerate() {
        assert_eq!(*answer, Ok(Answer::Bound), "waiter {index} of eight");
    }
    assert_eq!(waiters.popped(), None, "the ring still held a row after eight answers");
}

/// A clock that moves one millisecond each time it is read: the deadline of test 2 passes
/// after a known number of looks, whatever the machine's load.
struct Steps(Cell<u64>);

impl Clock for Steps {
    fn now(&self) -> Duration {
        let now = self.0.get();
        self.0.set(now + 1);
        Duration::from_millis(now)
    }
}

/// No answer: the wait ends with `Overran::Deadline` for its slot once the deadline passes on
/// the clock it was given (rule 18), and the row it waited in did reach the ring.
/// Prints `test stub_wait::tests::an_unanswered_stub_overruns_its_deadline ... ok`.
#[test]
fn an_unanswered_stub_overruns_its_deadline() {
    const SLOT: u32 = 3;
    direct();
    let waiters = rows(2);
    let clock = Steps(Cell::new(0));
    let row = waiters.claim(SLOT, 0).expect("a free row");
    let deadline = Deadline::after(clock.now(), Duration::from_millis(5));
    match waiters.wait(row, deadline, &clock) {
        Err(Overran::Deadline { slot, .. }) => assert_eq!(slot, SLOT),
        other => panic!("an unanswered wait ended {other:?}, not at its deadline"),
    }
    assert_eq!(waiters.popped(), Some(Waiting { row, slot: SLOT, thread: 0 }), "the waiting row was not on the ring");
}

/// The cause D6 removes: the stub's path allocates. On a fresh thread, `claim` (the ring's
/// push inside it) and a `wait` that really sleeps in the kernel until another thread
/// answers allocate nothing at all (the old `resolve` made an `Arc` and a queue node for
/// each first call). Doc: "the stub path allocates nothing".
/// Prints `test stub_wait::tests::the_stub_path_allocates_nothing ... ok`.
#[test]
fn the_stub_path_allocates_nothing() {
    const SLOT: u32 = 9;
    direct();
    assert!(blaze_alloc_count::installed(), "the counting allocator is not this binary's: the count would be vacuous");
    let waiters = rows(4);
    let claimed = AtomicBool::new(false);
    let (answer, used) = std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            blaze_alloc_count::measure(|| {
                let clock = Monotonic::new();
                let deadline = Deadline::after(clock.now(), LONG);
                let row = waiters.claim(SLOT, 0).expect("a free row");
                claimed.store(true, Ordering::Release);
                let answer = waiters.wait(row, deadline, &clock);
                waiters.release(row);
                answer
            })
        });
        // The answer comes after the waiter is (most likely) asleep in the kernel: the
        // measured path includes the kernel wait, not only the first look.
        let popped: [Waiting; 1] = pop_all(&waiters);
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), LONG);
        while !claimed.load(Ordering::Acquire) {
            assert!(!deadline.passed(clock.now()), "the claim did not return within {LONG:?}");
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(50));
        waiters.answer(popped[0].row, Answer::Bound);
        waiter.join().expect("the waiting thread")
    });
    assert_eq!(answer, Ok(Answer::Bound));
    assert_eq!(used.allocations, 0, "the stub path allocates nothing: it allocated {used:?}");
}
