//! The rustc debug build's check that a must-finish value was finished (`crate::unfinished`,
//! std.md "Values that must be finished"): dropping one unfinished stops, naming its type and
//! the test line that made it; finishing it says nothing; a panic already unwinding drops one
//! without a second panic, which would abort this test process.

#![cfg(all(not(blaze), debug_assertions))]

use std::{
    num::NonZero,
    panic::{AssertUnwindSafe, catch_unwind},
};

use super::{root, world};
use crate::{Plant, local, mem::TASK_FRAME, task::said};

fn capacity() -> NonZero<usize> {
    NonZero::new(8).expect("a capacity of eight")
}

/// An `End` dropped unfinished stops, naming `local::End`, how it is finished, and this file,
/// where `local::pair` was called (`#[track_caller]` back to the program's call).
#[test]
fn an_end_dropped_unfinished_stops_naming_it() {
    let io = world(1, Plant::None);
    let cx = root(&io);
    let stopped = catch_unwind(AssertUnwindSafe(|| {
        let local::Pair { first, second } = local::pair("dropped", capacity());
        second.finish(&cx);
        drop(first);
    }));
    let payload = stopped.expect_err("an `End` dropped unfinished went unnoticed");
    let said = said(&*payload);
    for part in ["`local::End`", "dropped unfinished", "finish(cx)", "tests/unfinished.rs"] {
        assert!(said.contains(part), "the stop does not say {part:?}: {said}");
    }
}

/// A `Task` dropped unfinished stops the same way, naming `task::Task`; the scope it was in,
/// dropped while that unwinds, says nothing more.
#[test]
fn a_task_dropped_unfinished_stops_naming_it() {
    let io = world(2, Plant::None);
    let cx = root(&io);
    let stopped = catch_unwind(AssertUnwindSafe(|| {
        let scope = cx.scope();
        let task = scope.may_overlap("left", TASK_FRAME, |_| -> Result<u32, String> { Ok(3) });
        drop(task);
        let _ = scope.join(&cx);
    }));
    let payload = stopped.expect_err("a `Task` dropped unfinished went unnoticed");
    let said = said(&*payload);
    for part in ["`task::Task`", "join(cx)", "tests/unfinished.rs"] {
        assert!(said.contains(part), "the stop does not say {part:?}: {said}");
    }
}

/// Finished values end without a stop: both ends of a pair, a task and its scope.
#[test]
fn finished_values_end_quietly() {
    let io = world(3, Plant::None);
    let cx = root(&io);
    let ended = catch_unwind(AssertUnwindSafe(|| {
        let local::Pair { first, second } = local::pair("finished", capacity());
        first.finish(&cx);
        second.finish(&cx);
        let scope = cx.scope();
        let task = scope.may_overlap("joined", TASK_FRAME, |_| -> Result<u32, String> { Ok(3) });
        let three = task.join(&cx);
        let joined = scope.join(&cx);
        (three.is_ok_and(|it| it == 3), joined.is_ok())
    }));
    match ended {
        Ok((true, true)) => {}
        Ok(outcome) => panic!("the task or its scope failed: {outcome:?}"),
        Err(payload) => panic!("a finished value stopped: {}", said(&*payload)),
    }
}

/// A panic elsewhere unwinds past unfinished values: the panic is the test's own, with no
/// second panic from their guards (which would abort the process, failing every test here).
#[test]
fn a_panic_elsewhere_drops_them_without_a_second() {
    let stopped = catch_unwind(|| {
        let _pair = local::pair("unwound", capacity());
        panic!("the test's own panic");
    });
    let payload = stopped.expect_err("the test's own panic was caught");
    assert_eq!(said(&*payload), "the test's own panic");
}
