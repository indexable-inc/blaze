//! What a simulated world hands its program, each once: the command line, the host's facts
//! and its exit, and the trace its contexts record (not a global's).

use std::{
    num::NonZero,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use super::root;
use crate::{
    host::{Exit, HostFacts},
    io::{Io, SimWorld},
    mem::TASK_FRAME,
    task::{Layout, TaskError, TaskWhy},
    trace::RecordKind,
};

#[test]
fn args_and_host_are_the_worlds() {
    let mut world = SimWorld::new(7);
    world.args = vec!["prog".to_owned(), "--flag".to_owned()];
    world.host = HostFacts { pid: 42, cores: NonZero::<usize>::MIN.saturating_add(7), name: Arc::from("box") };
    let io = Io::simulated(world);
    let args = io.args().expect("Io::sim offers args");
    assert_eq!(args.each().collect::<Vec<_>>(), ["prog", "--flag"]);
    assert_eq!(args.get(1), Some("--flag"));
    assert_eq!(args.get(2), None);
    let host = io.host().expect("Io::sim offers host");
    assert_eq!((host.pid(), host.cores().get(), &*host.name()), (42, 8, "box"));
}

#[test]
fn a_simulated_exit_unwinds_its_task_and_the_world_remembers_it() {
    let io = Io::sim(1);
    let cx = root(&io);
    let host = io.host().expect("Io::sim offers host");
    let ended = catch_unwind(AssertUnwindSafe(|| {
        // An exit inside a task passes through its scope rather than failing the task.
        let _ = cx.scope_with(|s| {
            let task = s.spawn("exits", TASK_FRAME, |_cx| -> Result<(), String> { host.exit(3) });
            let _ = task.join(s.cx());
        });
    }));
    let payload = ended.expect_err("an exit does not return");
    assert_eq!(payload.downcast_ref::<Exit>(), Some(&Exit { status: 3 }));
    assert_eq!(io.exited(), Some(3));
}

#[test]
fn spans_and_events_go_to_the_worlds_trace() {
    let io = Io::sim(2);
    let cx = root(&io);
    let child = cx.span("load");
    child.event("cache missed");
    let records = io.trace(|trace| trace.records().iter().map(|it| (it.span, it.kind)).collect::<Vec<_>>());
    assert_eq!(
        records,
        [
            ("main", RecordKind::Opened { parent: "main" }),
            ("load", RecordKind::Opened { parent: "main" }),
            ("load", RecordKind::Event("cache missed")),
        ]
    );
    // Another world's trace is its own: nothing is global.
    let other = Io::sim(2);
    assert!(other.trace(|trace| trace.records().is_empty()));
}

#[test]
fn spawn_concurrent_is_refused_under_inline_and_its_task_never_runs() {
    let io = Io::sim(3);
    let cx = root(&io);
    let mut ran = false;
    let result = cx.scope_with(|s| {
        let refused = s.spawn_concurrent("overlaps", TASK_FRAME, |_cx| -> Result<(), String> {
            ran = true;
            Ok(())
        });
        assert!(refused.join(s.cx()).is_err());
    });
    assert!(!ran);
    assert_eq!(
        result,
        Err(TaskError { scope: "main", task: "overlaps", why: TaskWhy::CannotOverlap { layout: Layout::Inline } })
    );
}
