//! A scope is a value, and when `Scope::join` returns its tasks have all ended: each task that
//! started has ended (a panic's included), nothing a task started is still in flight in the
//! driver, each `Task::join` gives exactly what its closure did, and the scope's own join gives
//! the first failure, naming the task. Tasks sleep, write and read through `Pending` ops they
//! wait for or cancel, fail and panic, borrowing the file from the frame that holds the scope,
//! with the scope cancelled part way through or not.
//!
//! Plant: [`Plant::JoinDropsFailure`], a join that says the scope went well though a task
//! failed.

use std::{cell::RefCell, time::Duration};

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, path, root, world};
use crate::{
    Cx, Plant,
    clock::Clock,
    fs::{File, ReadWrite},
    io::Io,
    task::{TaskError, TaskWhy},
};

const NAMES: [&str; 6] = ["t0", "t1", "t2", "t3", "t4", "t5"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Sleeps(u64),
    /// A write of the file's start, waited for or cancelled.
    Writes { cancel: bool },
    /// A read of the file's start, waited for or cancelled.
    Reads { cancel: bool },
    Fails,
    Panics,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    tasks: Vec<Kind>,
    /// The scope is cancelled just before this task is spawned.
    cancel_at: Option<u64>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let n = d.int(1, NAMES.len() as u64);
    let tasks = (0..n)
        .map(|_| match d.int(0, 7) {
            0 => Kind::Fails,
            1 if d.one_in(2) => Kind::Panics,
            2 | 3 => Kind::Writes { cancel: d.coin() },
            4 | 5 => Kind::Reads { cancel: d.coin() },
            _ => Kind::Sleeps(d.int(0, 20)),
        })
        .collect();
    let cancel_at = d.one_in(3).then(|| d.int(0, n - 1));
    Plan { seed, tasks, cancel_at }
}

/// What a task's closure did, as it saw it.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Did {
    Returned(Result<(), String>),
    Panicked,
}

/// Each task's start and end, recorded as they happen; an end is recorded by a guard, so a
/// panic records one too.
struct Log {
    started: RefCell<Vec<usize>>,
    ended: RefCell<Vec<usize>>,
    did: RefCell<Vec<(usize, Did)>>,
}

struct Ends<'a>(&'a Log, usize);

impl Drop for Ends<'_> {
    fn drop(&mut self) {
        self.0.ended.borrow_mut().push(self.1);
        if std::thread::panicking() {
            self.0.did.borrow_mut().push((self.1, Did::Panicked));
        }
    }
}

/// What a task of `kind` does to `file`.
fn act(cx: &Cx, kind: Kind, clock: &Clock, file: &File<ReadWrite>) -> Result<(), String> {
    match kind {
        Kind::Fails => Err("this task fails".to_owned()),
        Kind::Panics => panic!("this task panics (a planted panic of the scopes test)"),
        Kind::Sleeps(ms) => {
            let _ = clock.sleep(cx, Duration::from_millis(ms));
            Ok(())
        }
        Kind::Writes { cancel } => {
            let bytes = [7u8; 8];
            let pending = file.write_from(&bytes, 0);
            let _ = if cancel { pending.cancel(cx) } else { pending.wait(cx) };
            Ok(())
        }
        Kind::Reads { cancel } => {
            let mut buf = [0u8; 8];
            let pending = file.read_into(&mut buf, 0);
            let _ = if cancel { pending.cancel(cx) } else { pending.wait(cx) };
            Ok(())
        }
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let clock = io.need_clock().map_err(|e| format!("harness: {e}"))?;
    let dir = io.need_fs().map_err(|e| format!("harness: {e}"))?.open_dir(&cx, &path("data")).map_err(|e| format!("harness: {e}"))?;
    let file = match dir.create(&cx, &path("f")) {
        Ok(file) => file,
        Err(error) => return Err(format!("harness: {error}")),
    };
    let checked = scoped(plan, &io, &cx, &clock, &file);
    let closed = file.close(&cx);
    match (checked, closed) {
        (Err(why), _) => Err(why),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(format!("harness: close: {error}")),
    }
}

/// The plan's scope over `file`, and the oracle on it. A scope is linear, so nothing here
/// returns while it is live: what is wrong is kept in `wrong` and returned after its join.
fn scoped(plan: &Plan, io: &Io, cx: &Cx, clock: &Clock, file: &File<ReadWrite>) -> Result<(), String> {
    let log = Log { started: RefCell::new(Vec::new()), ended: RefCell::new(Vec::new()), did: RefCell::new(Vec::new()) };
    let mut wrong: Vec<String> = Vec::new();
    let scope = cx.scope();
    for (at, &kind) in plan.tasks.iter().enumerate() {
        if plan.cancel_at == Some(at as u64) {
            scope.cancel();
        }
        let log = &log;
        let task = scope.spawn(NAMES[at], move |cx| -> Result<(), String> {
            log.started.borrow_mut().push(at);
            let _ends = Ends(log, at);
            let did = act(cx, kind, clock, file);
            // clone: the test's record of what the closure returned.
            log.did.borrow_mut().push((at, Did::Returned(did.clone())));
            did
        });
        let joined = task.join(cx);
        let did = log.did.borrow().iter().find(|(it, _)| *it == at).map(|(_, did)| did.clone());
        let fits = match (&did, &joined) {
            (Some(Did::Returned(Ok(()))), Ok(())) => true,
            (Some(Did::Returned(Err(said))), Err(TaskError { task, why: TaskWhy::Failed(why), .. })) => {
                *task == NAMES[at] && why == said
            }
            (Some(Did::Panicked), Err(TaskError { task, why: TaskWhy::Panicked(_), .. })) => *task == NAMES[at],
            _ => false,
        };
        if !fits {
            wrong.push(format!("task {at} ({kind:?}): its closure did {did:?}, and its join gave {joined:?}"));
        }
    }
    let running = scope.running();
    let joined = scope.join(cx);
    let (started, ended) = (log.started.borrow().len(), log.ended.borrow().len());
    if running != 0 || started != ended {
        wrong.push(format!("the scope joined with {running} running: {started} tasks started and {ended} ended"));
    }
    let in_flight = io.in_flight();
    if in_flight != 0 {
        wrong.push(format!("the scope joined with {in_flight} of its tasks' effects still in the driver"));
    }
    let first = plan.tasks.iter().position(|kind| matches!(kind, Kind::Fails | Kind::Panics));
    match (first, &joined) {
        (None, Ok(())) => {}
        (Some(at), Err(TaskError { scope: "main", task, .. })) if *task == NAMES[at] => {}
        (want, got) => wrong.push(format!("the scope's first failure should be task {want:?}; its join gave {got:?}")),
    }
    match wrong.first() {
        None => Ok(()),
        // clone: the finding, for the test's report.
        Some(why) => Err(why.clone()),
    }
}

#[hegel::test(test_cases = 150)]
fn a_scopes_tasks_have_all_ended_when_it_joins(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_a_join_that_drops_a_failure() {
    control(Plant::JoinDropsFailure, |seeds: &mut Seeds| plan(seeds), run);
}
