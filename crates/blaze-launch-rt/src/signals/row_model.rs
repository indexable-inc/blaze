//! The signal-action row's seqlock under the interleaving checker
//! (`bash /Volumes/Projects/blaze-int-1112-notes/verify.sh model blaze-launch-rt signals::row_model`:
//! `--cfg blaze_check`, release).
//!
//! WHY THE FIRST VERSION RAN ONE EXECUTION (17:36). Its reader ran `Row::read` on the model's
//! main thread straight after spawning the writer, so the whole read (every load) ran before the
//! writer had taken a step: a spawned thread has no pending operation until it first runs, and
//! blaze-check's DPOR adds a backtrack point only for a thread's pending operation that depends
//! on an earlier access (rt.rs `schedule`). When the writer then ran, each of its writes looked
//! dependent only on the writer's OWN load of the same object just before it (the checker keeps
//! one last access per object, `Entry::last`; `replace` loads `seq` before its compare-exchange
//! and loads the fields, `before`, before storing them), never on the reader's loads; and every
//! load the reader made had one store to read (the start value). No backtrack point, no load
//! choice: one execution, and the reader always saw the start value.
//!
//! THE SHAPE HERE. The reader YIELDS before it reads (`thread::yield_now`: a schedule point that
//! hands over to the writer at no preemption cost), so the first schedule runs the writer's
//! whole `replace` before the reader's first load. From there the checker has two sources of
//! executions it cannot miss:
//! - load choices: every reader load may read any store not hidden from it under C11 (rt/memory.rs
//!   `load`); the reader has no happens-before with the writer, so its first `seq` load has every
//!   store of `seq` to choose from (start 0, claim 1, publish 2 for one replace; 0 to 4 for two),
//!   and each field load the start or the new value: a reader "during" a write, observably;
//! - DPOR from the reader's loads: a load depends on the object's last STORE (`Entry::dependent`),
//!   the writer's, which the reader is not ordered after, so the checker backtracks to run the
//!   reader before the publish, before each field store and before the claim: a reader that
//!   starts before and in the middle of a write, by interleaving.
//! Each test asserts a FLOOR of executions (the stores of the reader's first `seq` load in the
//! first schedule: each choice is its own execution) and asserts the outcomes that only those
//! interleavings produce (the start value, the new value, a retry), so a model that silently
//! explores one execution fails in words.
//!
//! THE ORACLE, inside every execution: a reader returns one write's whole value (or `Busy`),
//! never fields of two; it panics otherwise, which the checker reports as a finding with the
//! schedule that replays it. The controls plant `Plant::BumpOnce` (a writer with no odd claim)
//! and `Plant::NoRecheck` (a reader that does not load the sequence again) and must get that
//! finding, in the schedule each test names.
//!
//! One mask word (`Row<1>`): three fields are enough to tear; the production row's 16 words on
//! Linux only multiply the executions.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use blaze_check::{Builder, thread};

use super::row::{Busy, Plant, Row, Value};

/// Read and claim attempts in the model: each failed one yields (`spin_loop` is a yield there).
/// Two are enough to see a retry; three keep `Busy` rare without multiplying the executions.
const TRIES: u32 = 3;

/// The value the row starts with, and two writers' values: every field differs between them.
const START: Value<1> = Value { handler: 0, flags: 0, mask: [0] };
const A: Value<1> = Value { handler: 0xA, flags: 0xA, mask: [0xA] };
const B: Value<1> = Value { handler: 0xB, flags: 0xB, mask: [0xB] };

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Seen {
    Start,
    A,
    B,
    Busy,
    Torn,
}

fn seen(read: Result<Value<1>, Busy>) -> Seen {
    match read {
        Ok(value) if value == START => Seen::Start,
        Ok(value) if value == A => Seen::A,
        Ok(value) if value == B => Seen::B,
        Ok(_) => Seen::Torn,
        Err(Busy) => Seen::Busy,
    }
}

/// What the model's executions showed: the outcomes, how many reads retried, how many
/// executions ran (counted at each execution's start, so a finding's execution counts too).
#[derive(Clone, Default, Debug)]
struct Tally {
    seen: BTreeSet<Seen>,
    retried: u64,
    executions: u64,
}

fn lock(tally: &Mutex<Tally>) -> MutexGuard<'_, Tally> {
    tally.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The bounds, fixed here so the environment cannot change what a test explores.
fn builder(preemptions: u8) -> Builder {
    let mut builder = Builder::new();
    builder.max_preemptions = Some(preemptions);
    builder.random = None;
    builder.replay = None;
    builder
}

/// What the writer thread does.
#[derive(Clone, Copy, Debug)]
enum Writes {
    /// `replace(A)`.
    One,
    /// `replace(A)` then `replace(B)`, back to back: a reader may fall between their halves.
    Two,
}

/// The model ended without a finding (`Ok`), or with one: its report, and the tally so far.
type Explored = Result<Tally, (String, Tally)>;

/// Runs `body` under the checker, counting executions; a finding becomes `Err` with its report.
fn explore(preemptions: u8, body: impl Fn(&Mutex<Tally>) + 'static) -> Explored {
    let held = Arc::new(Mutex::new(Tally::default()));
    let into = Arc::clone(&held);
    let ran = catch_unwind(AssertUnwindSafe(|| {
        builder(preemptions).check(move || {
            lock(&into).executions += 1;
            body(&into);
        })
    }));
    let tally = lock(&held).clone();
    match ran {
        Ok(()) => Ok(tally),
        Err(panic) => match panic.downcast::<String>() {
            Ok(report) => Err((*report, tally)),
            Err(_) => panic!("the checker's report is a String"),
        },
    }
}

/// One writer thread (`writes`, with `write_plant`) against a reader on the main thread that
/// yields first, then reads once (with `read_plant`). The reader's oracle panics on a torn value.
fn reader_against(writes: Writes, write_plant: Plant, read_plant: Plant, preemptions: u8) -> Explored {
    explore(preemptions, move |tally| {
        let row = Arc::new(Row::<1>::new());
        let writer = {
            let row = Arc::clone(&row);
            thread::spawn(move || {
                let _ = row.replace(&A, TRIES, write_plant);
                if let Writes::Two = writes {
                    let _ = row.replace(&B, TRIES, write_plant);
                }
            })
        };
        // The schedule point that lets the writer run first with no preemption (module doc).
        thread::yield_now();
        let read = row.read_with(TRIES, read_plant);
        let outcome = seen(read.map(|(value, _attempts)| value));
        {
            let mut tally = lock(tally);
            tally.seen.insert(outcome);
            if let Ok((_, attempts)) = read
                && attempts > 1
            {
                tally.retried += 1;
            }
        }
        assert!(outcome != Seen::Torn, "a reader returned a torn action: {read:?}");
        writer.join().expect("the writer");
    })
}

/// Two writers, `A` on a spawned thread and `B` on the main one, then a read after both joined:
/// what the row ended holding.
fn writers(plant: Plant) -> Explored {
    explore(3, move |tally| {
        let row = Arc::new(Row::<1>::new());
        let other = {
            let row = Arc::clone(&row);
            thread::spawn(move || {
                let _ = row.replace(&B, TRIES, plant);
            })
        };
        let _ = row.replace(&A, TRIES, plant);
        other.join().expect("the other writer");
        let outcome = seen(row.read(TRIES));
        lock(tally).seen.insert(outcome);
    })
}

/// The schedule a finding's report names (`CHECK_REPLAY=<trail>`).
fn schedule_of(report: &str) -> Option<&str> {
    const KEY: &str = "CHECK_REPLAY=";
    let at = report.find(KEY)? + KEY.len();
    report[at..].split_whitespace().next()
}

/// The order threads ran in along a trail, each run of one thread once: the trail's digits
/// (one per schedule point), with load choices (`a`..`z`, `{n}`) and timed waits left out.
fn thread_order(trail: &str) -> String {
    let mut order = String::new();
    let mut in_braces = false;
    for c in trail.chars() {
        match c {
            '{' => in_braces = true,
            '}' => in_braces = false,
            '0'..='9' if !in_braces && !order.ends_with(c) => order.push(c),
            _ => {}
        }
    }
    order
}

/// Whether a trail has a load that read an older store than the newest (`a` is the newest).
fn reads_stale(trail: &str) -> bool {
    trail.chars().any(|c| matches!(c, 'b'..='z' | '{'))
}

/// A planted bug's finding, in the schedule the controls name: thread 0 (the reader) spawns
/// and yields while it is the only thread, so its first schedule branch is the handover;
/// thread 1 (the writer) runs its whole write, thread 0 reads (trail order `10`: no
/// preemption), with at least one reader load choosing an older store. Execution 1 is excluded
/// by construction: there every load reads the newest store and the read returns the last
/// write whole, so the finding comes in a later execution of the same thread order (the
/// checker explores the deepest choices, the reader's loads, first).
fn assert_caught(explored: Explored, what: &str) {
    let (report, tally) = match explored {
        Ok(tally) => panic!("{what}: the planted bug was NOT caught over {} executions ({:?})", tally.executions, tally.seen),
        Err(found) => found,
    };
    eprintln!("{what}: caught in execution {}:\n{report}", tally.executions);
    assert!(report.contains("torn action"), "{what}: the finding is not the torn read: {report}");
    let trail = schedule_of(&report).unwrap_or_else(|| panic!("{what}: the report names no schedule: {report}"));
    assert_eq!(thread_order(trail), "10", "{what}: caught in another thread order than the one named, trail {trail}");
    assert!(reads_stale(trail), "{what}: the trail has no load of an older store, trail {trail}");
    assert!(tally.executions >= 2, "{what}: caught in execution {} (execution 1 reads every newest store and cannot tear)", tally.executions);
}

/// A reader racing one `replace` gets the start value or the new value whole, never a mix, and
/// retries when the sequence was odd or moved. At least 3 executions: in the first schedule
/// the reader's first `seq` load has three stores to read (start 0, claim 1, publish 2), each
/// its own execution. The start value (a reader before the claim), the new value (after the
/// publish) and a retry (a reader during the write) must each have happened.
/// Prints `test signals::row_model::a_reader_gets_one_whole_action_around_a_replace ... ok`.
#[test]
fn a_reader_gets_one_whole_action_around_a_replace() {
    let tally = match reader_against(Writes::One, Plant::None, Plant::None, 3) {
        Ok(tally) => tally,
        Err((report, _)) => panic!("the seqlock failed:\n{report}"),
    };
    let Tally { seen, retried, executions } = &tally;
    eprintln!("one replace: {executions} executions, outcomes {seen:?}, {retried} retried");
    assert!(*executions >= 3, "the model ran {executions} execution(s), fewer than the 3 stores of the reader's first sequence load: the row's atomics or the yield are not the model's ({seen:?})");
    assert!(seen.contains(&Seen::Start), "no reader before the write: {seen:?} over {executions} executions");
    assert!(seen.contains(&Seen::A), "no reader after the write: {seen:?} over {executions} executions");
    assert!(*retried > 0, "no reader retried, so none ran during the write: {seen:?} over {executions} executions");
    assert!(!seen.contains(&Seen::B), "a value nobody wrote: {seen:?}");
}

/// Control (rule 53): a writer that bumps the sequence once, with no odd claim, lets a reader
/// return a torn value. Caught in the schedule `assert_caught` names: the writer's whole replace
/// first, then the reader's `seq` load reading the start store 0 (stale), its field loads
/// mixing the new and the start value, and its second `seq` load reading 0 again.
/// Prints `test signals::row_model::a_reader_control_bump_once_is_caught ... ok`.
#[test]
fn a_reader_control_bump_once_is_caught() {
    assert_caught(reader_against(Writes::One, Plant::BumpOnce, Plant::None, 3), "bump once");
}

/// Control: a reader that does not load the sequence again returns a torn value against a
/// correct writer. Caught in the schedule `assert_caught` names: the writer's whole replace
/// first, the reader's `seq` load reading the start store 0 (stale), its field loads mixing the
/// new and the start value, returned with no recheck.
/// Prints `test signals::row_model::a_reader_control_no_recheck_is_caught ... ok`.
#[test]
fn a_reader_control_no_recheck_is_caught() {
    assert_caught(reader_against(Writes::One, Plant::None, Plant::NoRecheck, 3), "no recheck");
}

/// Two replaces back to back (A then B) and a reader between their halves: it gets the start
/// value, A or B whole, never a mix of any two, and retries when the sequence moved. At least
/// 5 executions: the reader's first `seq` load in the first schedule has five stores to read
/// (0, 1, 2, 3, 4). A (a reader between the first publish and the second claim), the start
/// value, B and a retry must each have happened. Two preemptions: the load choices, not the
/// bound, carry the interleavings here, and a third only multiplies the executions.
/// Prints `test signals::row_model::a_reader_between_two_replaces_gets_one_whole_action ... ok`.
#[test]
fn a_reader_between_two_replaces_gets_one_whole_action() {
    let tally = match reader_against(Writes::Two, Plant::None, Plant::None, 2) {
        Ok(tally) => tally,
        Err((report, _)) => panic!("the seqlock failed:\n{report}"),
    };
    let Tally { seen, retried, executions } = &tally;
    eprintln!("two replaces: {executions} executions, outcomes {seen:?}, {retried} retried");
    assert!(*executions >= 5, "the model ran {executions} execution(s), fewer than the 5 stores of the reader's first sequence load ({seen:?})");
    for want in [Seen::Start, Seen::A, Seen::B] {
        assert!(seen.contains(&want), "no reader saw {want:?}: {seen:?} over {executions} executions");
    }
    assert!(*retried > 0, "no reader retried: {seen:?} over {executions} executions");
}

/// Control: with the writer bumping the sequence once, a reader between the two replaces
/// returns a torn value (A's and B's fields, or the start's and A's). Caught in the schedule
/// `assert_caught` names.
/// Prints `test signals::row_model::a_reader_between_two_replaces_control_is_caught ... ok`.
#[test]
fn a_reader_between_two_replaces_control_is_caught() {
    assert_caught(reader_against(Writes::Two, Plant::BumpOnce, Plant::None, 2), "two replaces, bump once");
}

/// Two writers are serialized: the row ends holding A or B whole, and each in some execution.
/// At least 2 executions: the row ends as A only if B's write ran first, and as B only if A's
/// did. (This test explored more than one execution at 17:36: the second writer's first `seq`
/// load depends on the first writer's publish, a store.)
/// Prints `test signals::row_model::two_writers_leave_one_whole_action ... ok`.
#[test]
fn two_writers_leave_one_whole_action() {
    let tally = match writers(Plant::None) {
        Ok(tally) => tally,
        Err((report, _)) => panic!("two writers failed:\n{report}"),
    };
    let Tally { seen, executions, .. } = &tally;
    assert!(*executions >= 2, "the model ran {executions} execution(s) ({seen:?})");
    let whole: BTreeSet<Seen> = [Seen::A, Seen::B].into_iter().collect();
    assert_eq!(*seen, whole, "two writers left {seen:?} over {executions} executions, wanted exactly A and B whole");
}

/// The control: two writers with no claim interleave their field stores and leave a mix.
/// Prints `test signals::row_model::two_writers_control_leave_a_torn_action ... ok`.
#[test]
fn two_writers_control_leave_a_torn_action() {
    let tally = match writers(Plant::BumpOnce) {
        Ok(tally) => tally,
        Err((report, _)) => panic!("two writers' control stopped with a finding:\n{report}"),
    };
    let Tally { seen, executions, .. } = &tally;
    assert!(*executions >= 2, "the model ran {executions} execution(s) ({seen:?})");
    assert!(seen.contains(&Seen::Torn), "the model never saw the planted writers interleave: {seen:?} over {executions} executions");
}
