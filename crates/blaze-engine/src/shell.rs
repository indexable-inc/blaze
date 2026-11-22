//! The threaded shell over the ready driver (D157's switch, the engine side): ready.rs's
//! [`Ready`] stays the single-threaded state machine (rule 14) that hands out ready tasks and
//! takes results; [`drive`] runs the kinds' [`TaskKind::run`] on a host's [`Pool`] of workers and
//! feeds the results back, so the one core decides everything and the threads only run.
//!
//! # One turn
//!
//! 1. The core's step gives actions. A [`Action::Grow`] is planned here, on the calling thread,
//!    with nothing running (R4). The [`Action::Run`]s are a batch: with the core's
//!    [`Options::workers`](crate::ready::Options) at [`Limits::tick`] every task of a tick is one
//!    batch; with fewer, a tick is several (the core gives a freed worker the tick's next task).
//! 2. The batch runs on the pool, all at once as its threads allow. Every run reads through its
//!    own [`Host`] over one shared [`Facts`] of the core: read-only, `Sync` because the engine is
//!    (the bound on [`drive`]), and unchanged while the batch runs, because the core is borrowed
//!    for the whole batch and nothing merges inside a tick.
//! 3. The results are fed to the core one [`Event::Ran`] each, in the order the pool's clock saw
//!    them end (by name on a tie). The core keeps each until its tick closes and merges the tick
//!    in name order (R3), so the order fed changes nothing; the last result of a tick merges it
//!    and opens the next, whose runs are the next batch.
//!
//! # What a tick is on threads, and why the answer does not depend on it
//!
//! A tick is a batch boundary the coordinator chooses: [`Limits::tick`], the most tasks one tick
//! runs. `tick` at least the ready set merges "when the ready set drains"; `tick = N` merges
//! "every N completions". The proof is ready.rs's "What a tick is": a run reads only final cells,
//! finality is fixed between two openings, and a row's bytes are a function of its key, its reads
//! and the request's serial, never of the tick it merged in. shell_sim.rs draws the tick down to
//! one and the pool's completion order from the tape, and holds the tables to the single-threaded
//! driver's at the shipped tick, byte for byte.
//!
//! # A panic fails its task (rule 20)
//!
//! The pool catches an unwind out of one run and marks its job [`Job::panicked`]. The shell
//! answers it with [`fault`]: a failed result standing as owed ([`Failure::Owed`], `Panicked`: so
//! the next request that reaches it enters it again), defining nothing, reading nothing (the host
//! was lost with the unwind), with one finding; the name is listed in [`Drove::panicked`]. The
//! request goes on: readers of its cells read them absent, as of any failed task.
//!
//! # Bounded (rules 15, 18)
//!
//! The pool starts no run after the request's deadline ([`Ready::deadline`]) and returns the
//! jobs it did not start unrun. Each result is stepped at the time its run ended, in the order
//! the runs ended, so a deadline that passes inside a batch stops the step of the first run that
//! ended past it, while the runs that parked before it are still parked; the core refuses with
//! [`Error::Deadline`] naming the running task and the task parked longest with the cell it waits
//! on. When jobs were left unrun, the results that came are stepped first and then the core at
//! the pool's clock, past the deadline. What every park waited on is in the core's report
//! ([`Quiet::report`]).
//!
//! # The adapter a stage implements
//!
//! A stage is ported to this driver as one [`TaskKind`] per kind of task it runs, `C` being the
//! system its runs need (blaze-cli's `Cx`), and once for the whole CLI, one [`Pool`] over its
//! threads. A kind gives its facts (kind, stage, the stage its growth lands in), plans what a
//! changed file or a merged grower asks ([`Ask`]), and runs one task reading only through its
//! [`Recorded`] host, ending in a result ([`Ran::Done`] with an [`Outcome`] and the records it
//! made on its [`Out`]), the cells it still needs ([`Ran::Need`]), or a refusal. The worked
//! example is shell_sim.rs's `Outlines`, the smallest stage.
//!
//! What blaze-cli's Outlines stage implements to run here (read from engine_check.rs's outline
//! phase, engine_tasks.rs `Tasks::outline` and work.rs `outline_file`):
//!
//! - **The kind.** `KindFacts { kind: FileItems, stage: Outlines, grows_into: None }`. `plan`
//!   answers `Ask::File { file }` with the one `Planned` the planner makes today from the file's
//!   text (`outline_one`, engine_check.rs), and `Ask::Grown` with `Refusal::CannotPlan`.
//! - **The run.** `Tasks::outline`'s body, with its `Done` built by the contract instead of by
//!   hand: the cells it defines today (`WHOLE`, `SURFACE` with `item_cells`, `HEADS`, a
//!   `part_cell` per Signature and BodyCheck part) go through `Defines::sorted`, which is the
//!   duplicate check its `panic!` does now (K3, as a refusal); the result is
//!   `Outcome::made(record.out, defines, Stands::ByKey, src.reads()?)` (an outline reads nothing:
//!   the host's list is empty, and a parse is a function of its key); the findings go on
//!   `out.set_findings(record.findings.len())`. Its other `panic!`s (no payload, no source
//!   digest) become this shell's fault for that file instead of a crash.
//! - **Where the parse runs.** Today `outline_file` runs at plan time and the task only
//!   publishes. Ported, `run` calls `outline_file(cx, root, job, scratch, into)` itself, so the
//!   parse is a task on a worker: `Ok(Ok(record))` is the result above, `Ok(Err(why))` is
//!   `Outcome::failed(out, Failure::ByKey, reads)` (a refusal of its own text), and `Err(Unread)`
//!   is `Outcome::failed(out, Failure::Owed(Owed::Unread), reads)` (the file could not be read). The kind
//!   holds the root, the jobs by name and the frame its runs take scratch from.
//! - **Where the record is put.** Today the outline records are put after the merge
//!   (engine_check.rs, after the pipelined loop). Ported, the run puts its record before it
//!   returns: a record is keyed by its content and written once (rule 51), so a run whose result
//!   the tick then merges has nothing left to put, and nothing waits on a put after the merge.
//! - **The pool.** engine_pool.rs's `run(tasks, buffers, work)` becomes this module's
//!   [`Pool::run`] over [`Job`]s: the same threads, each taking the next job, with work.rs's
//!   `unwinding` moved into the pool around each run ([`Job::panicked`] on an unwind), each job
//!   given its start and end on the request's clock ([`Job::timed`]), and no job started after
//!   `until`. The `Cx` each worker owns is `C`.
//! - **Nothing else.** The stage keeps no buffer, wave or merge of its own: `Tasks::work`'s
//!   dispatch, its `failed` on a panic and engine_check.rs's `pool.run(&entered, ..)` loop for
//!   the outline phase go.

#![blaze::sans_io]

use blaze_drive::key::{self, Name};
use blaze_id::ContentId;
use blaze_own::{SharedAlloc, Vec};

use crate::code_pool::{Time, Worker};
use crate::engine::{Answer, CellId, Engine, Planned, SVec, Scope, Task};
use crate::kinds::{Ask, Kinds, Out, Outcome, Ran, Refusal};
#[cfg(doc)]
use crate::kinds::{Failure, Recorded, TaskKind};
use crate::last::LastAnswer;
use crate::readers::{Paired, ReaderIndex};
use crate::ready::{Action, Error, Event, Facts, Host, Learned, Quiet, Ready, ReadyPlant, Returned, Stepped};
use blaze_own::planted::Planted;
#[cfg(doc)]
use crate::ready::Limits;

/// The host's workers (rule 14: the threads are the host's, the decisions the core's).
pub trait Pool<'a> {
    /// What a worker hands each run: blaze-cli's `Cx` (its system and frame), `()` in the
    /// simulation.
    type Cx: ?Sized;

    /// The pool's clock, in the units of the request's deadline (rule 75: passed, never read
    /// from the ambient system).
    fn now(&self) -> Time;

    /// Runs `work` once on each of `jobs`, as many at once as the pool has workers, each job on
    /// one worker for its whole run, and returns when every job it started has returned. It
    /// starts no job once its clock is past `until`, and leaves that job unrun. It gives each
    /// job it ran its start and end on its clock ([`Job::timed`]); a run that unwinds is caught,
    /// its job marked [`Job::panicked`], and the pool goes on (rule 20).
    fn run(&mut self, jobs: &mut [Job<'a>], until: Time, work: &(dyn Fn(&Self::Cx, &mut Job<'a>) + Sync));
}

/// One run the shell hands the pool: a task, the worker slot the core gave it, and what came of
/// it.
pub struct Job<'a> {
    worker: Worker,
    task: Task,
    end: End<'a>,
    began: Time,
    ended: Time,
}

enum End<'a> {
    Unrun,
    Ran { ran: Ran<'a>, out: Out<'a>, learned: Learned<'a> },
    Panicked,
}

impl<'a> Job<'a> {
    fn new(worker: Worker, task: Task) -> Job<'a> {
        Job { worker, task, end: End::Unrun, began: Time(0), ended: Time(0) }
    }

    pub fn task(&self) -> &Task {
        &self.task
    }

    /// The core's worker slot for the task (what its [`Event::Ran`] names).
    pub fn worker(&self) -> Worker {
        self.worker
    }

    /// When the run started and ended on the pool's clock.
    pub fn timed(&mut self, began: Time, ended: Time) {
        self.began = began;
        self.ended = ended;
    }

    /// When the run ended on the pool's clock, as [`Job::timed`] set it: the time its result is
    /// stepped at ([`drive`]).
    pub fn ended(&self) -> Time {
        self.ended
    }

    /// The run unwound: its task is answered as a fault (rule 20).
    pub fn panicked(&mut self) {
        self.end = End::Panicked;
    }

    /// Whether the run happened (returned or panicked).
    pub fn ran(&self) -> bool {
        !matches!(self.end, End::Unrun)
    }

    /// The cells the run said it needs, when it parked: what a pool's tracing names.
    pub fn needs(&self) -> Option<&[CellId]> {
        match &self.end {
            End::Ran { ran: Ran::Need(needs), .. } => Some(needs.cells()),
            _ => None,
        }
    }

    /// The core's event for the run; a panicked run is answered by [`fault`] and named in
    /// `panicked`.
    fn returned(self, alloc: SharedAlloc<'a>, panicked: &mut SVec<'a, Name>) -> Result<Returned<'a>, Error<'a>> {
        let name = self.task.planned.name;
        let took = Time(self.ended.0.saturating_sub(self.began.0));
        let (ran, out, learned) = match self.end {
            End::Ran { ran, out, learned } => (ran, out, learned),
            End::Panicked => {
                panicked.push(name);
                let (ran, out) = fault(alloc);
                (ran, out, Learned::nothing(alloc))
            }
            End::Unrun => return Err(Error::Unrun { name }),
        };
        Ok(Returned { worker: self.worker, name, ran, out, took, learned })
    }
}

/// The out of a run that panicked: one digest, since the finding is the same for every task.
fn panicked_out() -> ContentId {
    key::digest("blaze-engine shell: the task's run panicked", &[])
}

/// What a run that panicked answers (rule 20): failed as a fault (owed: entered again by the
/// next request that reaches it), defining nothing, reading nothing, one finding. The
/// simulations' single-threaded reference answers a planted panic with this same value.
pub(crate) fn fault<'a>(alloc: SharedAlloc<'a>) -> (Ran<'a>, Out<'a>) {
    let mut out = Out::new_in(alloc);
    out.set_findings(1);
    (Ran::Done(Outcome::fault(panicked_out(), alloc)), out)
}

/// A request the shell drove to its quiet end.
pub struct Drove<'a, S: Scope, L = Answer<'a>, R = Paired<'a>> {
    /// The quiet core: its `finish` is the answer, its `report` where the wall went.
    pub quiet: Quiet<'a, S, L, R>,
    /// The tasks whose run panicked, in name order: each answered by [`fault`].
    pub panicked: SVec<'a, Name>,
}

/// Drives `ready` to its quiet end: the kinds' runs on `pool`, the decisions on this thread.
/// `T` is the core's plant seam ([`NoPlant`](blaze_own::planted::NoPlant) where it ships).
///
/// Each result is stepped at the time its run ended on the pool's clock (never earlier than
/// the step before), so the core's clock is the time each event happened: a deadline that
/// passes inside a batch stops the step of the first run that ended past it, with every task
/// that parked earlier in the batch still parked and named (rule 15). When the pool left jobs
/// unrun (the deadline passed before they could start), the runs that did happen are stepped
/// first, then the core is stepped at the pool's clock, past the deadline.
pub fn drive<'a, S, L, R, T, P>(ready: Ready<'a, S, L, R, T>, kinds: &Kinds<'_, 'a, P::Cx>, pool: &mut P, alloc: SharedAlloc<'a>) -> Result<Drove<'a, S, L, R>, Error<'a>>
where
    S: Scope,
    L: LastAnswer,
    R: ReaderIndex<'a>,
    T: Planted<ReadyPlant>,
    P: Pool<'a>,
    Engine<'a, S, L, R>: Sync,
{
    let mut actions: SVec<'a, Action> = Vec::new_in(alloc);
    let mut jobs: SVec<'a, Job<'a>> = Vec::new_in(alloc);
    let mut panicked: SVec<'a, Name> = Vec::new_in(alloc);
    let mut clock = pool.now();
    let mut stepped = ready.step(clock, Event::Start, &mut actions)?;
    loop {
        let ready = match stepped {
            Stepped::Going(ready) => ready,
            Stepped::Quiet(quiet) => {
                panicked.sort_unstable();
                return Ok(Drove { quiet, panicked });
            }
        };
        let mut grow = None;
        for action in actions.drain(..) {
            match action {
                Action::Run { worker, task } => jobs.push(Job::new(worker, task)),
                Action::Grow { by } => grow = Some(by),
            }
        }
        // R4: a growth is asked with nothing running (the core gives a Grow alone).
        if let Some(by) = grow {
            let Some(kind) = kinds.get(by.kind) else { return Err(Error::NoKind { name: by }) };
            let mut planned: SVec<'a, Planned> = Vec::new_in(alloc);
            kind.plan(Ask::Grown { by }, &mut planned).map_err(|refusal| Error::Refused { name: by, refusal })?;
            clock = later(clock, pool.now());
            stepped = ready.step(clock, Event::Grown { by, planned: &planned }, &mut actions)?;
            continue;
        }
        if jobs.is_empty() {
            return Err(Error::Idle);
        }
        let until = ready.deadline();
        {
            let facts = ready.facts();
            let shared = &facts;
            let work = move |cx: &P::Cx, job: &mut Job<'a>| run_one(shared, kinds, cx, job, alloc);
            pool.run(&mut jobs, until, &work);
        }
        // A job left unrun: the deadline passed before it could start (the core names the wait
        // once the runs that happened are stepped), else the pool broke its contract.
        let unrun = jobs.iter().find(|job| !job.ran()).map(|job| job.task.planned.name);
        jobs.retain(|job| job.ran());
        // Completion order on the pool's clock; R3 makes it no matter to the answer.
        jobs.sort_unstable_by(|a, b| (a.ended.0, a.began.0, a.task.planned.name).cmp(&(b.ended.0, b.began.0, b.task.planned.name)));
        let mut going = Stepped::Going(ready);
        for job in jobs.drain(..) {
            let Stepped::Going(ready) = going else { return Err(Error::Idle) };
            clock = later(clock, job.ended);
            let returned = job.returned(alloc, &mut panicked)?;
            going = ready.step(clock, Event::Ran(returned), &mut actions)?;
        }
        if let Some(name) = unrun {
            let Stepped::Going(ready) = going else { return Err(Error::Unrun { name }) };
            clock = later(clock, pool.now());
            return match ready.step(clock, Event::Tick, &mut actions) {
                Err(error) => Err(error),
                Ok(_) => Err(Error::Unrun { name }),
            };
        }
        stepped = going;
    }
}

/// The later of two times: the core's clock never runs back.
fn later(a: Time, b: Time) -> Time {
    Time(a.0.max(b.0))
}

/// One run on a worker: its kind over a fresh host of the shared facts.
fn run_one<'a, S, L, R, C>(facts: &Facts<'_, 'a, S, L, R>, kinds: &Kinds<'_, 'a, C>, cx: &C, job: &mut Job<'a>, alloc: SharedAlloc<'a>)
where
    S: Scope,
    L: LastAnswer,
    R: ReaderIndex<'a>,
    C: ?Sized,
{
    let name = job.task.planned.name;
    let host = Host::new(facts);
    let mut out = Out::new_in(alloc);
    let ran = match kinds.get(name.kind) {
        Some(kind) => {
            let src = host.recorded(name);
            let ran = kind.run(cx, &src, &job.task, &mut out);
            drop(src);
            ran
        }
        None => Ran::Failed(Refusal::Unknown { name }),
    };
    job.end = End::Ran { ran, out, learned: host.learned() };
}
