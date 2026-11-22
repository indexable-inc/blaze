//! What a simulated world hands its program, each once: the command line, the host's facts
//! and its exit, and the trace its contexts record (not a global's).

use std::{
    num::NonZero,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use super::root;
use crate::{
    clock::Born,
    host::{Exit, HostFacts},
    io::{Io, Limits, SimWorld},
    mem::TASK_FRAME,
    trace::RecordKind,
};

#[test]
fn args_and_host_are_the_worlds() {
    let mut world = SimWorld::new(7, Limits::USUAL);
    world.args = vec!["prog".to_owned(), "--flag".to_owned()];
    world.host = HostFacts {
        pid: 42,
        cores: NonZero::<usize>::MIN.saturating_add(7),
        name: Arc::from("box"),
        born: Born::Before(Duration::from_millis(3)),
        build_id: crate::host::BuildId::of(&[5; 20]),
        exe: Some(Arc::from("/box/bin/prog")),
    };
    let io = Io::simulated(world);
    let args = io.args().expect("Io::sim offers args");
    assert_eq!(args.each().collect::<Vec<_>>(), ["prog", "--flag"]);
    assert_eq!(args.get(1), Some("--flag"));
    assert_eq!(args.get(2), None);
    let host = io.host().expect("Io::sim offers host");
    assert_eq!((host.pid(), host.cores().get(), &*host.name()), (42, 8, "box"));
    assert_eq!(host.build_id().map(|it| it.as_bytes().to_vec()), Ok(vec![5; 20]), "the build id the test set");
    assert_eq!(host.exe().as_deref(), Ok("/box/bin/prog"), "the program's path the test set");
    // The process began 3 ms before the clock: an instant says 3 ms more since the start.
    let clock = io.clock().expect("Io::sim offers clock");
    let now = clock.now();
    let since = clock.since_process_start(now).expect("the world says when it began");
    assert_eq!(since, now.since_start() + Duration::from_millis(3));
}

/// A simulated world's streams are not terminals unless the test says so: then each says
/// what it was given, standard output and standard error apart.
#[test]
fn a_streams_terminal_facts_are_the_worlds() {
    use crate::term::{Size, Tty};
    let io = Io::sim(3);
    let (out, err) = (io.out().expect("Io::sim offers out"), io.err().expect("Io::sim offers err"));
    assert!(!out.is_terminal() && !err.is_terminal(), "not terminals by default");
    assert_eq!((out.size(), err.size()), (None, None));

    let mut world = SimWorld::new(3, Limits::USUAL);
    world.ttys = [Tty::Terminal(Some(Size { cols: 120, rows: 40 })), Tty::NotTerminal];
    let io = Io::simulated(world);
    let (out, err) = (io.out().expect("out"), io.err().expect("err"));
    assert!(out.is_terminal());
    assert_eq!(out.size(), Some(Size { cols: 120, rows: 40 }));
    assert!(!err.is_terminal(), "standard error is piped: its own fact");
    assert_eq!(err.size(), None);

    let mut world = SimWorld::new(3, Limits::USUAL);
    world.ttys = [Tty::NotTerminal, Tty::Terminal(None)];
    let io = Io::simulated(world);
    let err = io.err().expect("err");
    assert!(err.is_terminal(), "a terminal that says no size is still a terminal");
    assert_eq!(err.size(), None);
}

/// The wall clock is the time base of a file's stamp: a file written now is stamped at the
/// wall time of its write (between the readings before and after it), so a sweep that starts
/// at this instant sees the stamp as racy; once time moves past the file system's step, the
/// stamp is behind the wall clock and a later write stamps later again.
#[test]
fn a_write_is_stamped_on_the_wall_clock() {
    use crate::fs::{Content, Durable};
    let io = Io::sim(11);
    let cx = root(&io);
    let clock = io.clock().expect("Io::sim offers clock");
    let fs = io.need_fs().expect("Io::sim offers files");
    let dir = fs.open_dir(&cx, &super::path("w")).expect("a directory");
    let stamp_of = || dir.stat(&cx, &super::path("a")).expect("the stat").expect("the file").stamp;
    let before = clock.wall();
    dir.publish(&cx, &super::path("a"), Content::bytes(b"one"), Durable::No).expect("the write");
    let after = clock.wall();
    let first = stamp_of();
    assert!(before.as_ns() <= first.modified_ns && first.modified_ns <= after.as_ns(), "{before:?} {first:?} {after:?}");
    clock.sleep(&cx, Duration::from_millis(20)).expect("the sleep");
    let later = clock.wall();
    assert!(later.as_ns() > first.modified_ns.max(first.changed_ns) + 10_000_000, "{later:?} {first:?}");
    assert_eq!((later.secs(), later.subsec_nanos()), (later.as_ns() / 1_000_000_000, (later.as_ns() % 1_000_000_000) as u32));
    dir.publish(&cx, &super::path("a"), Content::bytes(b"two"), Durable::No).expect("the second write");
    let second = stamp_of();
    assert!(second.modified_ns >= later.as_ns() && second != first, "{first:?} then {second:?}");
}

/// A world whose system would not say when the process began says so, naming what it said.
#[test]
fn an_unknown_process_start_is_an_error_naming_why() {
    let mut world = SimWorld::new(7, Limits::USUAL);
    world.host.born = Born::Unknown(Arc::from("no /proc"));
    let io = Io::simulated(world);
    let clock = io.clock().expect("Io::sim offers clock");
    let refused = clock.since_process_start(clock.now()).expect_err("no start known");
    assert!(refused.to_string().contains("no /proc"), "{refused}");
}

#[test]
fn a_simulated_exit_unwinds_its_task_and_the_world_remembers_it() {
    let io = Io::sim(1);
    let cx = root(&io);
    let host = io.host().expect("Io::sim offers host");
    let ended = catch_unwind(AssertUnwindSafe(|| {
        // An exit inside a task passes through its scope rather than failing the task.
        let _ = cx.scope_with(|s| {
            let task = s.may_overlap("exits", TASK_FRAME, |_cx| -> Result<(), String> { host.exit(3) });
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
