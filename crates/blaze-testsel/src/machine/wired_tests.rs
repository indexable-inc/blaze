//! What the runner's wiring added to [`Selection`]: [`Selection::detach`],
//! [`Selection::last_micros`], and the reasons a sandboxed pass is not kept that the runner
//! knows and the trace does not ([`NotKept::Foreign`], [`NotKept::Untraced`],
//! [`NotKept::Moved`]).

use std::alloc::Global;
use std::collections::BTreeMap;

use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Key, Kind, Namespace, Settings, TestIndex, TestInputs, TestRun, ToolId},
    pool::Ticket,
    proof,
    store::Put,
};

use super::{Effect, Executed, Input, Missed, NotKept, Ran, SelectError, Selection, Start, Step, Test, Verdict};
use crate::{
    ask::{ABSENT, Ask, UNLOGGED},
    table::{Bound, FnId},
};

/// The one function the test executes, its bound code, and the unlogged hash: what the run
/// gives and the host answers now.
const FUNCTION: FnId = FnId(ContentId([7; 32]));
const BOUND: Bound = Bound(ContentId([8; 32]));
const UNLOGGED_NOW: ContentId = ContentId([9; 32]);

fn tool() -> ToolId {
    ToolId { tool: ContentId([1; 32]), target: ContentId([2; 32]), namespace: Namespace::LOCAL }
}

fn base() -> Key<TestRun> {
    TestRun::key(&TestInputs {
        tool: tool(),
        settings: Settings(ContentId([3; 32])),
        env: ABSENT,
        item: ContentId([4; 32]),
        entry_args: ABSENT,
        attrs: ABSENT,
        args: &[],
    })
}

fn start<'s>(head: Option<Key<TestIndex>>) -> Start<'s> {
    Start {
        tool: tool(),
        suite: ContentId([5; 32]),
        program: ContentId([6; 32]),
        head,
        certified: None,
        run_all: false,
        workers: 1,
        compact_at: 4,
    }
}

/// Answers every effect but a run from `store` (nothing answered: every lookup misses), until
/// the selection waits or ends; the run asked, if one was.
fn drive(selection: &mut Selection<'_, Global>, store: &mut BTreeMap<(u16, ContentId), Vec<u8>>) -> Option<Ticket> {
    let mut run = None;
    for _ in 0..1000 {
        match selection.next() {
            Step::Exit | Step::Wait(_) => return run,
            Step::Do(Effect::Run { ticket, .. }) => run = Some(ticket),
            Step::Do(Effect::Get { ticket, tag, key, .. }) => {
                let got = store.get(&(tag.0, key)).map(|bytes| blaze_format::Aligned::copy_from(bytes));
                selection.give(Input::Got { ticket, record: got.as_deref() });
            }
            Step::Do(Effect::Changes { ticket, .. }) => selection.give(Input::Changes { ticket, functions: &[] }),
            Step::Do(Effect::Answer { ticket, shape, .. }) => {
                let now: Vec<ContentId> = crate::ask::Shape::checked(shape)
                    .asks()
                    .map(|ask| match ask {
                        Ask::Function(function) if function == UNLOGGED => UNLOGGED_NOW,
                        Ask::Function(function) if function == FUNCTION => BOUND.0,
                        _ => ABSENT,
                    })
                    .collect();
                selection.give(Input::Answers { ticket, now: &now });
            }
            Step::Do(Effect::Put { ticket, tag, key, bytes, .. }) => {
                store.insert((tag.0, key), bytes.to_vec());
                selection.give(Input::Put { ticket, put: Ok(Put::Written) });
            }
        }
    }
    panic!("a selection of one test that did not wait or end in 1000 steps");
}

/// What the runner marks a run with, beside what it read.
#[derive(Clone, Copy)]
struct Marks {
    traced: bool,
    foreign: bool,
    moved: bool,
}

const PLAIN: Marks = Marks { traced: true, foreign: false, moved: false };

/// One request of one test from `head`, run whatever it holds (`--run-all`), its run given back
/// after the selection is detached, which passed in `micros` and is marked as `marks` say: its
/// verdict, the new head, and what the selection said its last run took before it ran.
fn request(
    store: &mut BTreeMap<(u16, ContentId), Vec<u8>>,
    head: Option<Key<TestIndex>>,
    micros: u64,
    marks: Marks,
) -> (Verdict, Key<TestIndex>, Option<u64>) {
    let test = [Test { base: base(), name: "t" }];
    let (mut selection, ticket) = proof::scope(|snap| {
        let start = Start { run_all: true, ..start(head) };
        let mut selection = Selection::new(snap, start, &test, Global).expect("a request of one test");
        let ticket = drive(&mut selection, &mut *store).expect("a test run whatever it holds runs");
        (selection.detach().expect("every run is out"), ticket)
    });
    let last = selection.last_micros(0);
    let functions = [(FUNCTION, BOUND)];
    let ran = Ran {
        passed: true,
        micros,
        executed: Executed::Whole(&functions),
        unlogged: UNLOGGED_NOW,
        reads: &[],
        untraceable: &[],
        sandboxed: true,
        traced: marks.traced,
        foreign: marks.foreign,
        moved: marks.moved,
    };
    selection.give(Input::Ran { ticket, ran });
    assert_eq!(drive(&mut selection, &mut *store), None, "no run after the one asked");
    let report = selection.outcome().expect("the request ended").expect("the request was not refused");
    (report.verdicts[0], report.head, last)
}

/// One request of one test with no head, which passed in 5 µs, marked as `marks` say.
fn one(marks: Marks) -> Verdict {
    request(&mut BTreeMap::new(), None, 5, marks).0
}

/// The invariant: a detached selection files a traceable pass as the attached one does, and a
/// pass the tracer can't see all of (foreign code, no trace) or whose reads may not be what it
/// read (a file stamped since it began) is reported not kept, by name.
#[test]
fn a_detached_selection_keeps_a_traced_pass_and_names_what_it_does_not_keep() {
    assert!(matches!(one(PLAIN), Verdict::Ran { .. }), "{:?}", one(PLAIN));
    let foreign = Marks { foreign: true, ..PLAIN };
    // Each request runs its test whatever its head holds: `--run-all` is why it ran.
    let missed = Missed::RunAll;
    assert_eq!(one(foreign), Verdict::NotKept { passed: true, micros: 5, why: NotKept::Foreign, missed });
    let untraced = Marks { traced: false, ..PLAIN };
    assert_eq!(one(untraced), Verdict::NotKept { passed: true, micros: 5, why: NotKept::Untraced, missed });
    let moved = Marks { moved: true, ..PLAIN };
    assert_eq!(one(moved), Verdict::NotKept { passed: true, micros: 5, why: NotKept::Moved, missed });
    assert_eq!(NotKept::Foreign.code(), "foreign");
    assert_eq!(NotKept::Untraced.code(), "untraced");
    assert_eq!(NotKept::Moved.code(), "moved");
}

/// The invariant: a test's expected duration, what the queue orders its runs by, is its head
/// row's last run, and a test with no row has none.
#[test]
fn a_test_is_expected_to_take_what_its_last_run_took() {
    let mut store = BTreeMap::new();
    let (_, head, first) = request(&mut store, None, 5, PLAIN);
    assert_eq!(first, None, "a test with no head row has no expected duration");
    let (_, head, second) = request(&mut store, Some(head), 7, PLAIN);
    assert_eq!(second, Some(5), "the head's row holds the first run's time");
    let (_, _, third) = request(&mut store, Some(head), 9, PLAIN);
    assert_eq!(third, Some(7), "each new head holds the run before it");
}

/// A selection is detached only once nothing reads a record under its snapshot: never before
/// its index head is read.
#[test]
fn a_selection_is_not_detached_before_its_head_is_read() {
    let test = [Test { base: base(), name: "t" }];
    let head = Key::<TestIndex>::from_id(ContentId([10; 32]));
    let refused = proof::scope(|snap| {
        let selection = Selection::new(snap, start(Some(head)), &test, Global).expect("a request of one test");
        selection.detach().err()
    });
    assert!(matches!(refused, Some(SelectError::Misused(_))), "{refused:?}");
}
