//! What the runner's wiring added to [`Selection`]: [`Selection::detach`], and the reasons a
//! sandboxed pass is not kept that the runner knows and the trace does not ([`NotKept::Foreign`],
//! [`NotKept::Untraced`]).

use std::alloc::Global;
use std::collections::BTreeMap;

use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Key, Kind, Namespace, Settings, TestIndex, TestInputs, TestRun, ToolId},
    pool::Ticket,
    proof,
    store::Put,
};

use super::{Effect, Executed, Input, NotKept, Ran, SelectError, Selection, Start, Step, Test, Verdict};
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

/// One request of one test, its run given back after the selection is detached, which passed
/// and is marked as `traced` and `foreign` say.
fn one(traced: bool, foreign: bool) -> Verdict {
    let mut store = BTreeMap::new();
    let test = [Test { base: base(), name: "t" }];
    let (mut selection, ticket) = proof::scope(|snap| {
        let mut selection = Selection::new(snap, start(None), &test, Global).expect("a request of one test");
        let ticket = drive(&mut selection, &mut store).expect("a test with no kept pass runs");
        (selection.detach().expect("every run is out"), ticket)
    });
    let functions = [(FUNCTION, BOUND)];
    let ran = Ran {
        passed: true,
        micros: 5,
        executed: Executed::Whole(&functions),
        unlogged: UNLOGGED_NOW,
        reads: &[],
        untraceable: &[],
        sandboxed: true,
        traced,
        foreign,
    };
    selection.give(Input::Ran { ticket, ran });
    assert_eq!(drive(&mut selection, &mut store), None, "no run after the one asked");
    let report = selection.outcome().expect("the request ended").expect("the request was not refused");
    report.verdicts[0]
}

/// The invariant: a detached selection files a traceable pass as the attached one does, and a
/// pass the tracer can't see all of (foreign code, no trace) is reported not kept, by name.
#[test]
fn a_detached_selection_keeps_a_traced_pass_and_names_what_it_does_not_keep() {
    assert!(matches!(one(true, false), Verdict::Ran { .. }), "{:?}", one(true, false));
    assert_eq!(one(true, true), Verdict::NotKept { passed: true, micros: 5, why: NotKept::Foreign });
    assert_eq!(one(false, false), Verdict::NotKept { passed: true, micros: 5, why: NotKept::Untraced });
    assert_eq!(NotKept::Foreign.code(), "foreign");
    assert_eq!(NotKept::Untraced.code(), "untraced");
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
