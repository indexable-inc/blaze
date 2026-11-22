//! THE SIMULATION (rules 43, 64, 79): generated phases run through the driver by a model of the
//! thin driver, on blaze-sim's tape.
//!
//! A world is a plan of 1 to 16 tasks on 1 to 4 workers, each task waiting on up to three earlier
//! ones, each with an act: made, unkept, made with a read no key covers, failed, panicked, hung
//! (never returns until the stall deadline, sometimes returning late after), a column overflowed
//! (the refusal carried out by `?`), or a column overflowed and the refusal dropped by the stage.
//! The model runs a task when its completion is delivered, in an order the tape picks, through a
//! real [`TaskCx`] on the worker's lane; it sends a stop at any point, fails or delays a commit,
//! stalls workers by the watchdog or the deadline.
//!
//! The oracle, per run: (a) every task has one end and at most one profile row; (b) every `Waited`
//! names an input that failed or waits, and a root that truly failed; no `Made` has an `Unkept`
//! input; (c) the merge follows the commit's success; (d) no start after a stop, and every task
//! that ran has its profile row; (e) a worker's frame holds no earlier task's payload when a task
//! starts on it; (g) the phase's arena holds no live block after the phase's values are dropped.
//! Without a stop, every task's end equals the end computed from the plan alone. Per case, two
//! more runs with different completion orders: (f) their merges are in task order and equal, and
//! the bytes they committed are equal.
//!
//! Controls (rule 53): one planted bug per law (plant.rs), each caught.
//!
//! The measure printed by `the_driver_holds_every_law`: the ends by kind over every run, and THE
//! MEMORY LAW's figures: bytes the driver holds per task (its slot) against the floor
//! `size_of::<Out>() + size_of::<Held>()`, and the profile row per task that ran.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use blaze_id::ItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Root, RootOpts};
use blaze_own::{AVec, Alloc, Vec};
use blaze_sim::{Broke, Config, Tape};

use crate::cx::{Arg, Caps, Clock, Lane, Place, RecordKind, TaskCx, TaskRanges};
use crate::machine::{Cause, CommitError, Done, Effect, End, EndKind, Event, Limits, Phase, State, Status};
use crate::plant::{self, Plant};
use crate::proto::{Code, Column, Outcome, Plan, PlanCaps, PlanError, Span, Stage, Stopped, TaskFail, TaskOut, TaskRef, WorkerRef};

const STALL: u64 = 1_000;
const COMMIT_WITHIN: u64 = 500;
const LIMITS: Limits = Limits { stall_after: STALL, commit_within: COMMIT_WITHIN };
const BYTES_CAP: u32 = 4096;
const CAPS: Caps = Caps { reads: 256, puts: 128, bytes: BYTES_CAP, says: 64, args: 128, spans: 64 };
const STEP_CAP: u32 = 10_000;
/// More bytes than a lane holds: the overflowing acts' put.
static BIG: [u8; 5000] = [0xAB; 5000];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    Made,
    Unkept,
    Uncovered,
    Fail,
    Panic,
    Hang,
    Overflow,
    Swallow,
}

#[derive(Clone, Copy, Debug)]
struct Job {
    task: u32,
    act: Act,
    reads: u8,
    puts: u8,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct SimOut {
    task: u32,
    puts: u32,
}

impl TaskOut for SimOut {}

#[derive(Clone, Copy, PartialEq, Debug)]
struct SimFail {
    code: u16,
}

impl TaskFail for SimFail {
    fn code(&self) -> Code {
        Code(self.code)
    }
}

type SimEnd = End<SimOut, SimFail>;

struct World<'h> {
    jobs: AVec<'h, Job>,
    spans: AVec<'h, Span>,
    deps: AVec<'h, TaskRef>,
    workers: u16,
}

/// The stage under simulation: its jobs and their inputs, as slices (a stage is `Sync`).
struct SimStage<'w> {
    jobs: &'w [Job],
    spans: &'w [Span],
    deps: &'w [TaskRef],
}

fn record_key(task: u32, put: u8) -> ContentId {
    Hasher::new("blaze-stage sim key").part(&task.to_le_bytes()).part(&[put]).finish()
}

fn item(task: u32) -> ItemKey {
    ItemKey(Hasher::new("blaze-stage sim item").part(&task.to_le_bytes()).finish())
}

impl Stage for SimStage<'_> {
    type Job = Job;
    type Out = SimOut;
    type Fail = SimFail;
    type Read = u32;
    const KIND: blaze_engine::Stage = blaze_engine::Stage::Bodies;

    fn plan(&self, plan: &mut Plan<'_, Job>) -> Result<(), PlanError> {
        for (job, span) in self.jobs.iter().zip(self.spans.iter()) {
            plan.job(*job, &self.deps[span.range()])?;
        }
        Ok(())
    }

    fn run(&self, job: Job, task: &mut TaskCx<'_, '_, u32>) -> Result<Outcome<SimOut, SimFail>, Stopped> {
        let open = task.open("sim task")?;
        // Scratch the frame: made and dropped inside the task.
        let mut scratch: AVec<'_, u32> = Vec::new_in(task.scratch());
        for r in 0..job.reads {
            task.stopped()?;
            // Repeats on purpose: the finish sorts and deduplicates.
            let cell = job.task * 16 + u32::from(r % 3);
            scratch.push(cell);
            task.read(cell)?;
        }
        drop(scratch);
        if job.act == Act::Uncovered {
            task.read_uncovered()?;
        }
        for p in 0..job.puts {
            let payload = [u8::try_from(job.task % 256).unwrap_or(0), p, 0x5A, 0xA5, 1, 2, 3, 4];
            task.put(RecordKind(1), record_key(job.task, p), &payload)?;
        }
        task.say(Code(7), Place { item: item(job.task), at: Span { start: 0, len: 4 } }, &[Arg::Count(u64::from(job.task))])?;
        task.close(open);
        let out = SimOut { task: job.task, puts: u32::from(job.puts) };
        Ok(match job.act {
            Act::Made | Act::Uncovered | Act::Panic | Act::Hang => Outcome::Made(out),
            Act::Unkept => Outcome::Unkept(out),
            Act::Fail => Outcome::Failed(SimFail { code: 3 }),
            Act::Overflow => {
                task.put(RecordKind(2), record_key(job.task, 255), &BIG)?;
                Outcome::Made(out)
            }
            Act::Swallow => {
                // The refusal dropped: the driver fails the task all the same.
                let _dropped = task.put(RecordKind(2), record_key(job.task, 255), &BIG).is_err();
                Outcome::Made(out)
            }
        })
    }
}

fn draw_world<'h>(tape: &Tape, h: Alloc<'h>) -> World<'h> {
    let n = 1 + tape.index(16, "tasks");
    let workers = 1 + u16::try_from(tape.index(4, "workers")).unwrap_or(0);
    let mut w = World { jobs: Vec::new_in(h), spans: Vec::new_in(h), deps: Vec::new_in(h), workers };
    for i in 0..n {
        let start = u32::try_from(w.deps.len()).unwrap_or(u32::MAX);
        if i > 0 {
            for _ in 0..tape.index(4, "inputs") {
                let on = TaskRef(u32::try_from(tape.index(i, "input")).unwrap_or(0));
                if !w.deps[blaze_int::idx(start)..].contains(&on) {
                    w.deps.push(on);
                }
            }
        }
        let end = u32::try_from(w.deps.len()).unwrap_or(u32::MAX);
        let act = match tape.index(12, "act") {
            0..=4 => Act::Made,
            5 => Act::Unkept,
            6 => Act::Uncovered,
            7 => Act::Fail,
            8 => Act::Panic,
            9 => Act::Hang,
            10 => Act::Overflow,
            _ => Act::Swallow,
        };
        let task = u32::try_from(i).unwrap_or(u32::MAX);
        let reads = u8::try_from(tape.index(5, "reads")).unwrap_or(0);
        let puts = u8::try_from(tape.index(4, "puts")).unwrap_or(0);
        w.jobs.push(Job { task, act, reads, puts });
        w.spans.push(Span { start, len: end - start });
    }
    w
}

/// Every task's end from the plan alone: what the driver must give without a stop.
fn expected<'h>(w: &World<'_>, h: Alloc<'h>) -> AVec<'h, SimEnd> {
    let mut ends: AVec<'h, SimEnd> = Vec::new_in(h);
    for (job, span) in w.jobs.iter().zip(w.spans.iter()) {
        let deps = &w.deps[span.range()];
        let failed = deps
            .iter()
            .copied()
            .filter_map(|d| match ends[d.index()] {
                End::Failed(_) => Some((d, d)),
                End::Waited { root, .. } => Some((d, root)),
                End::Made(_) | End::Unkept(_) | End::Stopped | End::NotRun => None,
            })
            .min_by_key(|(d, _)| *d);
        let unkept_in = deps.iter().any(|d| matches!(ends[d.index()], End::Unkept(_)));
        let out = SimOut { task: job.task, puts: u32::from(job.puts) };
        let end = match failed {
            Some((on, root)) => End::Waited { on, root },
            None => match job.act {
                Act::Made if unkept_in => End::Unkept(out),
                Act::Made => End::Made(out),
                Act::Unkept | Act::Uncovered => End::Unkept(out),
                Act::Fail => End::Failed(Cause::Stage(SimFail { code: 3 })),
                Act::Panic => End::Failed(Cause::Panicked),
                Act::Hang => End::Failed(Cause::Stalled),
                Act::Overflow | Act::Swallow => End::Failed(Cause::Full { column: Column::Bytes, cap: BYTES_CAP }),
            },
        };
        ends.push(end);
    }
    ends
}

struct SimClock(AtomicU64);

impl Clock for SimClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// What a run may draw besides the completion order.
#[derive(Clone, Copy)]
struct Sched {
    stop: bool,
    fail_commit: bool,
    late_commit: bool,
}

const FREE: Sched = Sched { stop: true, fail_commit: true, late_commit: true };
const ORDER_ONLY: Sched = Sched { stop: false, fail_commit: false, late_commit: false };

/// What one run produced, in the harness's allocator.
struct Run<'h> {
    ends: AVec<'h, SimEnd>,
    merged: AVec<'h, (TaskRef, SimEnd)>,
    committed: AVec<'h, u8>,
    state: State,
    stop_sent: bool,
    status: Option<Status>,
    rows: usize,
    late: u32,
    /// The starvation deadline was sent: ends then depend on the schedule (`Cause::NoWorker`).
    starved: bool,
}

fn oracle(why: std::string::String) -> Broke {
    Broke::Oracle(why)
}

/// The driver refused an event of the model: a finding (the model follows the protocol).
fn refused(why: crate::machine::MachineError) -> Broke {
    Broke::Oracle(std::format!("the driver refused an event: {why:?}"))
}

fn run_task(stage: &SimStage<'_>, job: Job, task: TaskRef, worker: WorkerRef, lane: &mut Lane<'_, u32>, scratch: Alloc<'_>, stop: &AtomicBool, clock: &SimClock) -> (Done<SimOut, SimFail>, TaskRanges) {
    let mut cx = TaskCx::new(task, worker, lane, scratch, stop, clock);
    let returned = stage.run(job, &mut cx);
    (Done::Returned(returned), cx.finish())
}

fn ran(end: &SimEnd) -> bool {
    match end {
        End::Made(_) | End::Unkept(_) | End::Stopped => true,
        End::Failed(cause) => !matches!(cause, Cause::NoWorker),
        End::Waited { .. } | End::NotRun => false,
    }
}

fn run_phase<'h>(tape: &Tape, world: &World<'_>, sched: Sched, h: Alloc<'h>) -> Result<Run<'h>, Broke> {
    let arena = Root::new(RootOpts::TEST).map_err(|why| Broke::Harness(std::format!("the phase's arena: {why:?}")))?;
    let frames_root = Root::new(RootOpts::TEST).map_err(|why| Broke::Harness(std::format!("the workers' frames: {why:?}")))?;
    let n = world.jobs.len();
    let mut out = Run {
        ends: Vec::new_in(h),
        merged: Vec::new_in(h),
        committed: Vec::new_in(h),
        state: State::Planned,
        stop_sent: false,
        status: None,
        rows: 0,
        late: 0,
        starved: false,
    };
    let mut dropped = false;
    let mut leaked = false;
    {
        let a = arena.alloc();
        let stage = SimStage { jobs: &world.jobs, spans: &world.spans, deps: &world.deps };
        let mut plan: Plan<'_, Job> = Plan::new_in(PlanCaps { tasks: 64, deps: 256 }, a);
        stage.plan(&mut plan).map_err(|why| Broke::Harness(std::format!("the plan: {why:?}")))?;
        let mut lanes: AVec<'_, Lane<'_, u32>> = Vec::new_in(a);
        for _ in 0..world.workers {
            lanes.push(Lane::new_in(CAPS, a));
        }
        let mut phase: Phase<'_, SimOut, SimFail> = Phase::new(plan.deps(), world.workers, LIMITS, a).map_err(refused)?;
        let stop = AtomicBool::new(false);
        let clock = SimClock(AtomicU64::new(0));
        let mut now = 0u64;
        // The model of each worker's frame: whose payload is in it.
        let mut frames: AVec<'h, Option<TaskRef>> = Vec::from_elem_in(None, usize::from(world.workers), h);
        let mut pending: AVec<'h, (TaskRef, WorkerRef)> = Vec::new_in(h);
        let mut hung: AVec<'h, (TaskRef, WorkerRef)> = Vec::new_in(h);
        let mut stalled: AVec<'h, (TaskRef, WorkerRef)> = Vec::new_in(h);
        let mut commit_ok = false;
        let mut steps = 0u32;
        phase.start(now).map_err(refused)?;
        loop {
            while let Some(effect) = phase.next_effect() {
                match effect {
                    Effect::Start { task, worker } => {
                        if out.stop_sent {
                            return Err(oracle(std::format!("a start after stop: task {} on worker {}", task.0, worker.0)));
                        }
                        pending.push((task, worker));
                    }
                    Effect::ResetFrame(worker) => frames[worker.index()] = None,
                    Effect::Commit => {
                        for (task, held, kept) in phase.commits() {
                            let lane = &lanes[held.lane.index()];
                            out.committed.push(u8::from(kept));
                            out.committed.extend_from_slice(&task.0.to_le_bytes());
                            for row in lane.puts(held.puts) {
                                out.committed.extend_from_slice(&row.key.0);
                                out.committed.extend_from_slice(&row.digest.0);
                                out.committed.extend_from_slice(lane.bytes(row));
                            }
                            for read in lane.reads(held.reads) {
                                out.committed.extend_from_slice(&read.to_le_bytes());
                            }
                        }
                    }
                    Effect::Merge => {
                        if !commit_ok {
                            return Err(oracle(std::format!("a result visible before its commit finished: the merge came in state {:?}", phase.state())));
                        }
                        for (task, end, _) in phase.merge_order() {
                            out.merged.push((task, end));
                        }
                    }
                    Effect::Report(status) => out.status = Some(status),
                    Effect::DropArena => dropped = true,
                    Effect::LeakArena { .. } => leaked = true,
                }
            }
            if matches!(phase.state(), State::Merged | State::Stopped | State::Refused(_)) {
                break;
            }
            steps += 1;
            if steps > STEP_CAP {
                return Err(Broke::Harness(std::format!("no end after {STEP_CAP} steps, in state {:?}", phase.state())));
            }
            now += 1;
            clock.0.store(now, Ordering::Release);
            if sched.stop && !out.stop_sent && tape.one_in(12, "stop") {
                stop.store(true, Ordering::Release);
                out.stop_sent = true;
                phase.step(now, Event::Stop).map_err(refused)?;
                continue;
            }
            if phase.state() == State::Committing {
                if sched.late_commit && tape.one_in(8, "commit late") {
                    now += COMMIT_WITHIN;
                    phase.step(now, Event::Deadline).map_err(refused)?;
                } else if sched.fail_commit && tape.one_in(8, "commit fails") {
                    phase.step(now, Event::CommitDone(Err(CommitError { code: Code(9) }))).map_err(refused)?;
                } else {
                    commit_ok = true;
                    phase.step(now, Event::CommitDone(Ok(()))).map_err(refused)?;
                }
                continue;
            }
            let stuck = pending.is_empty() && hung.is_empty();
            if stuck && sched.fail_commit && phase.next_deadline().is_some() && tape.one_in(4, "no lost worker comes back") {
                // Every worker lost and none returns: the starvation deadline fails what is left.
                now = now.max(phase.next_deadline().unwrap_or(now));
                clock.0.store(now, Ordering::Release);
                phase.step(now, Event::Deadline).map_err(refused)?;
                out.starved = true;
                continue;
            }
            if !stalled.is_empty() && (stuck || tape.one_in(3, "a stalled task returns late")) {
                let (task, worker) = stalled.remove_swap(tape.index(stalled.len(), "which late"));
                let job = plan.job_of(task);
                let (done, ranges) = run_task(&stage, job, task, worker, &mut lanes[worker.index()], frames_root.alloc(), &stop, &clock);
                out.late += 1;
                phase.step(now, Event::TaskDone { task, ranges, done }).map_err(refused)?;
                continue;
            }
            if !pending.is_empty() {
                let (task, worker) = pending.remove_swap(tape.index(pending.len(), "which task returns"));
                let job = plan.job_of(task);
                if let Some(before) = frames[worker.index()] {
                    return Err(oracle(std::format!(
                        "a frame not reset: task {} on worker {} found task {}'s payload in its frame",
                        task.0, worker.0, before.0
                    )));
                }
                frames[worker.index()] = Some(task);
                let (done, ranges) = match job.act {
                    Act::Hang => {
                        hung.push((task, worker));
                        continue;
                    }
                    Act::Panic => {
                        // The thin driver caught the unwind: the context is finished unused.
                        let cx = TaskCx::new(task, worker, &mut lanes[worker.index()], frames_root.alloc(), &stop, &clock);
                        (Done::Panicked, cx.finish())
                    }
                    Act::Made | Act::Unkept | Act::Uncovered | Act::Fail | Act::Overflow | Act::Swallow => {
                        run_task(&stage, job, task, worker, &mut lanes[worker.index()], frames_root.alloc(), &stop, &clock)
                    }
                };
                phase.step(now, Event::TaskDone { task, ranges, done }).map_err(refused)?;
                continue;
            }
            if !hung.is_empty() {
                if tape.coin("the watchdog, not the deadline") {
                    let (task, worker) = hung.remove_swap(0);
                    phase.step(now, Event::WorkerStalled(worker)).map_err(refused)?;
                    stalled.push((task, worker));
                } else {
                    now += STALL;
                    phase.step(now, Event::Deadline).map_err(refused)?;
                    stalled.extend_from_slice(&hung);
                    hung.clear();
                }
                continue;
            }
            return Err(Broke::Harness(std::format!("nothing to deliver in state {:?}", phase.state())));
        }
        out.state = phase.state();
        out.rows = phase.rows().len();
        // (a) one end each, at most one row each; (d) every task that ran has its row.
        let mut rows_of: AVec<'h, u32> = Vec::from_elem_in(0, n, h);
        for row in phase.rows() {
            rows_of[row.task.index()] += 1;
        }
        for t in 0..n {
            let task = TaskRef(u32::try_from(t).unwrap_or(u32::MAX));
            let Some(end) = phase.end_of(task) else {
                return Err(oracle(std::format!("task {t} has no end in state {:?}", out.state)));
            };
            if rows_of[t] > 1 {
                return Err(oracle(std::format!("task {t} ended twice: {} profile rows", rows_of[t])));
            }
            if ran(&end) && rows_of[t] == 0 {
                return Err(oracle(std::format!("the partial profile misses task {t}, which ran and ended {end:?}")));
            }
            out.ends.push(end);
        }
        // (b) a wait names a failure that truly failed; nothing kept stands on an unkept input.
        for (t, end) in out.ends.iter().enumerate() {
            let deps = &world.deps[world.spans[t].range()];
            match *end {
                End::Waited { on, root } => {
                    if !deps.contains(&on) || !matches!(out.ends[on.index()].kind(), EndKind::Failed | EndKind::Waited) {
                        return Err(oracle(std::format!("task {t} waits on {}, which is not a failed input: {:?}", on.0, out.ends[on.index()])));
                    }
                    if out.ends[root.index()].kind() != EndKind::Failed {
                        return Err(oracle(std::format!("task {t} names a root that did not fail: {} ended {:?}", root.0, out.ends[root.index()])));
                    }
                }
                End::Made(_) => {
                    if let Some(d) = deps.iter().find(|d| out.ends[d.index()].kind() == EndKind::Unkept) {
                        return Err(oracle(std::format!("task {t} was kept on the unkept input {}", d.0)));
                    }
                }
                End::Unkept(_) | End::Failed(_) | End::Stopped | End::NotRun => {}
            }
        }
        if !dropped && !leaked {
            return Err(oracle(std::format!("the phase ended in {:?} without dropping its arena", out.state)));
        }
        let Some(status) = out.status else {
            return Err(oracle(std::format!("the phase ended in {:?} without its report", out.state)));
        };
        if status.counts.ended() != status.counts.tasks {
            return Err(oracle(std::format!("the report counts {} ends of {} tasks", status.counts.ended(), status.counts.tasks)));
        }
    }
    // (g) everything of the phase was in its arena and is gone.
    let live = arena.used().live_blocks;
    if dropped && live != 0 {
        return Err(oracle(std::format!("the arena holds {live} live blocks at its drop")));
    }
    Ok(out)
}

#[derive(Default)]
struct Tally {
    cases: u32,
    runs: u32,
    tasks: u32,
    by_kind: [u32; 6],
    stops: u32,
    refused: u32,
    late: u32,
    status: Option<Status>,
}

fn kind_at(kind: EndKind) -> usize {
    match kind {
        EndKind::Made => 0,
        EndKind::Unkept => 1,
        EndKind::Failed => 2,
        EndKind::Waited => 3,
        EndKind::Stopped => 4,
        EndKind::NotRun => 5,
    }
}

fn tally_run(tally: &mut Tally, run: &Run<'_>) {
    tally.runs += 1;
    tally.tasks += u32::try_from(run.ends.len()).unwrap_or(u32::MAX);
    for end in run.ends.iter() {
        tally.by_kind[kind_at(end.kind())] += 1;
    }
    tally.stops += u32::from(run.stop_sent);
    tally.refused += u32::from(matches!(run.state, State::Refused(_)));
    tally.late += run.late;
    tally.status = run.status.or(tally.status);
}

fn same_ends(got: &[SimEnd], want: &[SimEnd], what: &str) -> Result<(), Broke> {
    for (t, (got, want)) in got.iter().zip(want.iter()).enumerate() {
        if got != want {
            return Err(oracle(std::format!("{what}: task {t} ended {got:?}, the plan says {want:?}")));
        }
    }
    Ok(())
}

fn in_task_order(run: &Run<'_>) -> Result<(), Broke> {
    for pair in run.merged.windows(2) {
        if pair[0].0 >= pair[1].0 {
            return Err(oracle(std::format!("the merge order is not task order: task {} before task {}", pair[0].0.0, pair[1].0.0)));
        }
    }
    Ok(())
}

fn case(tape: &Tape, tally: &mut Tally) -> Result<(), Broke> {
    let harness = Root::new(RootOpts::TEST).map_err(|why| Broke::Harness(std::format!("the case's root: {why:?}")))?;
    let h = harness.alloc();
    let world = draw_world(tape, h);
    let want = expected(&world, h);
    tally.cases += 1;

    // A run with everything: stops, failed and late commits, stalls, late returns.
    let free = run_phase(tape, &world, FREE, h)?;
    if !free.stop_sent && !free.starved {
        same_ends(&free.ends, &want, "without a stop")?;
    }
    tally_run(tally, &free);

    // (f) two runs that differ only in completion order.
    let one = run_phase(tape, &world, ORDER_ONLY, h)?;
    let two = run_phase(tape, &world, ORDER_ONLY, h)?;
    same_ends(&one.ends, &want, "the first ordered run")?;
    in_task_order(&one)?;
    in_task_order(&two)?;
    if one.merged[..] != two.merged[..] {
        return Err(oracle(std::format!("the merge order differs between two completion orders: {} and {} rows", one.merged.len(), two.merged.len())));
    }
    if one.committed[..] != two.committed[..] {
        return Err(oracle(std::format!("the committed bytes differ between two completion orders: {} and {} bytes", one.committed.len(), two.committed.len())));
    }
    tally_run(tally, &one);
    tally_run(tally, &two);
    Ok(())
}

fn ratio(part: u32, whole: u32) -> f64 {
    f64::from(part) / f64::from(whole.max(1))
}

/// The invariant: the stage driver resets task frames, merges in plan order, reports transitive waits accurately, starts nothing after stop, and exposes no result before commit.
#[test]
fn the_driver_holds_every_law() {
    let mut tally = Tally::default();
    blaze_sim::check(Config::cases(400), |tape| case(tape, &mut tally));
    assert!(tally.stops > 0, "no run was stopped: the stop path is untested");
    assert!(tally.by_kind[kind_at(EndKind::Waited)] > 0, "no task waited: the failure path is untested");
    assert!(tally.by_kind[kind_at(EndKind::Unkept)] > 0, "no task was unkept");
    let Some(status) = tally.status else {
        panic!("no run reported");
    };
    std::println!(
        "stage driver (simulated): {} cases, {} runs, {} tasks: made {}, unkept {}, failed {}, waited {}, stopped {}, not run {}; {} runs stopped, {} refused at the commit, {} late returns",
        tally.cases,
        tally.runs,
        tally.tasks,
        tally.by_kind[0],
        tally.by_kind[1],
        tally.by_kind[2],
        tally.by_kind[3],
        tally.by_kind[4],
        tally.by_kind[5],
        tally.stops,
        tally.refused,
        tally.late,
    );
    std::println!(
        "stage driver memory law: {} bytes held per task to the merge, floor {} (out {} + ranges {}): ratio {:.2}; profile row {} bytes per task that ran",
        status.held_per_task,
        status.floor_per_task,
        core::mem::size_of::<SimOut>(),
        core::mem::size_of::<crate::machine::Held>(),
        ratio(status.held_per_task, status.floor_per_task),
        status.row_bytes,
    );
}

fn planted(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    plant::with(plant, || case(tape, &mut Tally::default()))
}

#[test]
fn the_oracle_catches_a_frame_not_reset() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::FrameNotReset, "a frame not reset", planted);
}

#[test]
fn the_oracle_catches_a_merge_in_completion_order() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::CompletionOrder, "merge order", planted);
}

#[test]
fn the_oracle_catches_a_wait_naming_its_direct_input() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::WaitedOnDirect, "names a root that did not fail", planted);
}

#[test]
fn the_oracle_catches_a_start_after_stop() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::StartAfterStop, "a start after stop", planted);
}

#[test]
fn the_oracle_catches_a_result_visible_before_its_commit() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::VisibleBeforeCommit, "visible before its commit", planted);
}

/// The cause removed (rule 23): a chain of three tasks whose first makes a read no key covers is
/// unkept all along, and not failed (today's build-script hole); with that read gone, all three
/// are made.
#[test]
fn an_uncovered_read_is_unkept_not_failed_and_its_readers_too() {
    let harness = Root::new(RootOpts::TEST).expect("the test's root");
    let h = harness.alloc();
    let chain = |first: Act| {
        let mut w = World { jobs: Vec::new_in(h), spans: Vec::new_in(h), deps: Vec::new_in(h), workers: 2 };
        for task in 0..3u32 {
            let start = u32::try_from(w.deps.len()).unwrap_or(0);
            if task > 0 {
                w.deps.push(TaskRef(task - 1));
            }
            let act = if task == 0 { first } else { Act::Made };
            w.jobs.push(Job { task, act, reads: 2, puts: 1 });
            w.spans.push(Span { start, len: u32::try_from(w.deps.len()).unwrap_or(0) - start });
        }
        w
    };
    let tape = Tape::calm();
    let uncovered = run_phase(&tape, &chain(Act::Uncovered), ORDER_ONLY, h).expect("the uncovered chain runs");
    let kinds = |run: &Run<'_>| [run.ends[0].kind(), run.ends[1].kind(), run.ends[2].kind()];
    assert_eq!(kinds(&uncovered), [EndKind::Unkept; 3], "a read no key covers makes the task and its readers unkept");
    assert_eq!(uncovered.state, State::Merged, "an unkept task does not fail the phase");
    let control = run_phase(&tape, &chain(Act::Made), ORDER_ONLY, h).expect("the covered chain runs");
    assert_eq!(kinds(&control), [EndKind::Made; 3], "with the uncovered read removed, all three are made");
    std::println!("stage driver: uncovered chain {:?}; control {:?}", kinds(&uncovered), kinds(&control));
}
