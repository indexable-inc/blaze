//! The ready-set driver (decision D157): one request's tasks run as soon as what they read is
//! final, across stages and inside one, on the same [`Engine`] the phase driver
//! ([`crate::drive`]) runs, so the answer is folded by one piece of code. Construct it with
//! [`Ready::new`] over an engine made by [`Engine::new_in`]. On each turn:
//!
//! 1. Call [`Ready::wait`] for what it waits on; perform each [`Action`] the last step gave:
//!    [`Action::Run`] runs the task on that worker through its kind ([`crate::kinds::TaskKind`])
//!    with a [`Host`] over [`Ready::facts`] and a fresh [`Out`]; [`Action::Grow`] asks the
//!    grower's kind for its growth ([`crate::kinds::Ask::Grown`]).
//! 2. Call [`Ready::step`] with the time and one event: [`Event::Start`] first, then one
//!    [`Event::Ran`] per run in any order (with what its host learned, [`Host::learned`]), or
//!    [`Event::Grown`]. A step consumes the driver and gives it back ([`Stepped::Going`]), or the
//!    quiet end ([`Stepped::Quiet`], whose [`Quiet::finish`] is the engine's and whose
//!    [`Quiet::report`] accounts for the request's wall), or a typed [`Error`] that holds no
//!    engine (R6).
//!
//! This driver is the single-threaded state machine (rule 14). shell.rs runs it on real
//! threads: every run of a tick reads one shared [`Facts`], which is `Sync` when the engine is.
//!
//! # Who owns what
//!
//! The engine owns the plan, the overlay (rows, cells, readers), the owed sets, `entered`,
//! `causes` and the report. It answers [`Engine::decide`] (a candidate stands, is entered with a
//! cause, or is out of scope, against the merged state only), [`Engine::standing`] (the same
//! without side effects), [`Engine::wanted`] (a made row still read) and
//! [`Engine::merge_results`] (a set of results filed, sorted by name), and keeps its candidates
//! by name only ([`Engine::take_candidates`]): no phase, wave or batch.
//!
//! The driver owns the one task-state map, ticks, the merge order (R3), parking and caps (R5),
//! finality (R1) and the accounting of waits (rule 15). Its caps are one value, [`Limits`]
//! (D165). A run reads through a [`Host`]: the tick's [`Facts`] (read-only, shared) and the
//! run's own memo of the finality walk, which comes back with its result ([`Learned`]) and is
//! kept for the rest of the request.
//!
//! # What a tick is
//!
//! A tick is the set of tasks started together: at its opening, the first [`Limits::tick`]
//! ready tasks in name order (the rest stay ready for the next tick). It closes when each of its
//! runs has returned; its results merge then in one [`Engine::merge_results`] in name order, and
//! the next tick opens. On threads (shell.rs) a tick is the boundary the coordinator chooses by
//! the tick size: `tick` at least the ready set is "when the ready set drains", `tick = N` is
//! "every N completions".
//!
//! The answer does not depend on the tick size, for three reasons that each hold of any size.
//! A run reads only cells whose definer is final (R1), and a final row is one nothing left in the
//! request can move, whichever tick merges what. Between two openings nothing merges, so
//! finality, and with it every answered read, is fixed for the whole tick (R3's premise). And a
//! row's bytes are a function of its key, what it read and the request's serial, never of when
//! it merged (engine.rs `apply`; orphans are retired at finish only). So a smaller tick changes
//! when a task runs and how often a reader parks, never what a row holds. shell_sim.rs draws the
//! tick size down to one and holds the tables to the shipped size's, byte for byte.
//!
//! # The invariants, each stated here once (kinds.rs states K1 to K4)
//!
//! - **R1. A read is answered only when final.** In one sentence: a row is final when nothing
//!   still unsettled in an earlier stage or owed as growth can reach it, and it merged this
//!   request, or it stands now and so does every row of its own stage it reads, transitively.
//!   As code (`Facts::settled`):
//!
//!   ```text
//!   final(of) = stage(of) known
//!       && no live, held or dormant task in a stage before stage(of)      // across stages (K4)
//!       && no merged grower owes growth landing at or before stage(of)
//!       && every row r reached from `of` through reads of rows of stage(of):
//!              r merged this request (a Merged entry, or a row of this serial)
//!           || r is not live and not held, and standing(r) is Stands or NotRun
//!   ```
//!
//!   A Need of a row that is not final because a row reached is `Moves` (a made row whose read
//!   moved, entered only on demand, or any row the walk finds to enter) ENTERS THAT ROW then
//!   (`Ready::demand`); a live or held one is waited for. The engine turns a wrong "final" into
//!   a stop, never a wrong answer: a moved cell whose reader merged this request having seen
//!   another digest is [`EngineError::ReadAfterMerge`] (engine.rs `readers_of`).
//! - **R2. One entry a name a request.** A name has at most one entry in one map of one state
//!   (Ready, Running, Waiting, Answered, Merged), kept to the end of the request. Every source of
//!   work goes through one `admit`: a candidate decided `Enter` (a moved read, a plan, a growth,
//!   an owed task), a row entered on demand, or a made row still wanted once nothing else can run.
//!   A second entry is [`Error::EnteredTwice`]. A [`Ran::Need`] takes Running to Waiting, and
//!   Waiting goes back to Ready when every needed cell is final: it runs again, never entered
//!   again. A candidate that is Waiting is not entered: it runs again on what is final then.
//! - **R3. One merge order.** A tick's results merge when the tick closes, together, in name
//!   order, never in completion order. Within a tick no result reads another's cells (each read
//!   was final), and what a run returns depends only on the state the tick opened on, so two
//!   interleavings of one request give equal bytes, the engine's `entered` and `causes` included.
//! - **R4. Growth before entry.** Each merged task whose kind grows owes one growth; the driver
//!   asks for it at the next tick's opening before anything is entered, one grower at a time in
//!   name order (nothing runs then: engine.rs `grow`'s precondition), and decides no candidate
//!   of a stage a grower may grow into while one is unsettled or owes its growth.
//!   A row of a kind no registered kind runs is never entered: it is final when the last answer
//!   holds it and nothing this request can move it ([`Engine::stands_unrun`]: owed nothing, not
//!   run by the scope, and when an edited file plans it, keyed today as stored with no read
//!   moved), as a check's read of the unit graph's crate cells, or of a head of the edited file
//!   a body edit did not move, is on the edit path; and never else. Of a kind the table STANDS
//!   (kinds.rs `KindTable::stood`: the plan holds it, this driver never runs it), a name with no
//!   row that the request neither plans nor owes is final too, its cells absent
//!   ([`Engine::stands_or_absent`]), as the phase driver reads a head entry no head task is
//!   named for.
//! - **R5. Bounded progress.** Nothing waits without an end: a Need of a cell already final
//!   ([`Error::NeedAnswered`]) or of a producer no kind plans ([`Error::Unplanned`]) is refused at
//!   once; a task parks at most [`Limits::park_cap`] times; an opening settles in at most
//!   [`Limits::open_rounds`] rounds ([`Error::Unsettled`]); a tick that cannot open while tasks
//!   wait is a cycle named ([`Error::Cycle`]) or the first wait named ([`Error::Stalled`]); every
//!   step past the deadline is [`Error::Deadline`], naming the running task and the task parked
//!   longest with the cell it waits on.
//! - **R6. A failure has no finish.** [`Ready::step`] consumes the driver; an [`Error`] holds no
//!   engine, so a request a merge refused half way can never be finished (states.md item 1).
//!   Only a quiet driver ([`Quiet`]: nothing held, dormant, live or owed) has `finish`.
//! - **R7. The phase driver's answer.** The folded answer (rows with their serial, cells, read
//!   lists, readers, files, owed cells and tasks, findings) equals the phase driver's on the same
//!   edit, last answer and scope (ready_sim.rs checks it on generated histories). Which tasks were
//!   entered, in what order and why is the driver's own: it enters fewer (a made row only when
//!   read) and never one twice.
//!
//! # Every wait named (rule 15, D167 (b))
//!
//! A parked task records the cell it waits on and since when. Each opening looks at it again:
//! the time since the last look is charged to the producer of the first need that was not final
//! then, and the look either wakes the task or names what it waits on now. [`Quiet::report`]
//! gives, per kind, the runs and their work, the parks and the time parked by the name waited on
//! (the top five), and the largest single run against the request's wall: a stage's wall is
//! work plus named waits, so 42 s of wall around 0.1 s of work says on what it waited.
//!
//! # The risk, and what the walk costs
//!
//! Same-stage finality is a proof that nothing left in the request can move a row. If it is
//! wrong, R1 breaks; [`EngineError::ReadAfterMerge`] makes that a typed stop at the merge that
//! moves the cell. The walk follows a row's reads of its own stage only, through a visited set,
//! and stops at a merged row, a row already proved final, or a live one. A row proved final is
//! kept for the rest of the request (finality only grows: nothing re-enters a final row; a run's
//! proofs come back in [`Learned`]) and never walked again; a row found not final is memoized for
//! the life of one memo (one run's host, or one opening's pass), over which the facts are fixed.
//! So over a request the walks cost O(same-stage reads of the rows they reach): one pass over
//! tempo's 20,181 heads and their head-to-head reads, linear in reads, plus at most one repeat of
//! a not-yet-final prefix per run or opening while a reader waits on it. A read-list loop (two
//! rows each reading the other, possible across requests) is no error: every row in it stands
//! only if all do, so the walk proves the loop final or not as one.
//!
//! # Deleted at the switch (when this driver replaces [`crate::drive`] and blaze-cli's
//! pipelined loop in engine_check.rs, the only callers of the phase path)
//!
//! In engine.rs: `next_phase`, `next_phase_when`, `next_phase_in`, `select`, `save_open`,
//! `enter`, `consider`, `stale_made`, `still_needed`, `merge_phase`, `merge_tasks`, `merge`,
//! `in_flight`, `unsettled`, `has_pending_ready`, `has_pending_stage`, `tasks()`, `merged()`,
//! `drive`, `drive_growing` and the `Pool` trait (shell.rs's `Pool` is the one left); the fields
//! `open`, `saved`, `tasks`, `opened`, `changed_inputs`, `passed`, `merged_from` and `ready` (its
//! check made unconditional); `pending`'s phase index (a name set); `WAVE_CAP`,
//! `EngineError::WaveTooDeep`, `EngineError::UnsettledRead`, `EngineError::BackwardRead`,
//! `Plant::SameStageStale` and `Plant::RemakesUnneeded`; and `candidate`'s in-flight branch and
//! wave arithmetic. `file`'s `phase` argument goes with them; `judge`, `decide`, `standing`,
//! `wanted` and `merge_results` stay as the engine's interface.

#![blaze::sans_io]

use core::cell::{Cell, RefCell};
use core::fmt;

use blaze_drive::key::{self, Key, Kind, Name};
use blaze_id::ContentId;
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{BTreeMap, BTreeSet, SharedAlloc, Vec};

use crate::code_pool::{Time, Worker};
use crate::engine::{Answer, Buffer, Cause, CellId, Decision, Engine, EngineError, Finished, Planned, SMap, SSet, SVec, Scope, Stage, Standing, Task};
use crate::kinds::{KindTable, Needs, Out, Ran, Recorded, Refusal, Source};
use crate::last::LastAnswer;
use crate::overlay::{Fold, RowFacts};
use crate::readers::{Paired, ReaderIndex};

/// The driver's caps as one value (D165): shipped as [`Limits::SHIPPED`], each at least its
/// value in [`Limits::SMALLEST`], and drawn between the two by the simulations (shell_sim.rs),
/// which reach each one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most times one task parks on a Need in one request (R5).
    pub park_cap: u16,
    /// The most rounds one opening takes to settle (R5): decide, enter the made rows still
    /// wanted, decide again. Two are enough on every history simulated; four ship as room.
    pub open_rounds: u32,
    /// The most tasks one tick runs (module doc, "What a tick is"): the answer does not depend
    /// on it, only when tasks merge and how often a reader parks.
    pub tick: usize,
    /// The request's budget from its [`Event::Start`], on the caller's clock (R5).
    pub deadline: Time,
}

impl Limits {
    /// What ships. `tick` keeps 128 workers fed eight tasks deep before a tick must close; the
    /// deadline is ten minutes in nanoseconds, the shell's clock (shell.rs `Pool::now`).
    pub const SHIPPED: Limits = Limits { park_cap: 64, open_rounds: 4, tick: 1024, deadline: Time(600_000_000_000) };
    /// The least legal value of each: one park, one round, one task a tick, one unit of time.
    pub const SMALLEST: Limits = Limits { park_cap: 1, open_rounds: 1, tick: 1, deadline: Time(1) };

    /// The first field below its least legal value, by name.
    fn below_smallest(self) -> Option<&'static str> {
        let least = Limits::SMALLEST;
        if self.park_cap < least.park_cap {
            return Some("park_cap");
        }
        if self.open_rounds < least.open_rounds {
            return Some("open_rounds");
        }
        if self.tick < least.tick {
            return Some("tick");
        }
        if self.deadline.0 < least.deadline.0 {
            return Some("deadline");
        }
        None
    }
}

/// A driver's options, every one named (rule 75).
#[derive(Clone, Copy)]
pub struct Options<'a> {
    pub alloc: SharedAlloc<'a>,
    /// Workers have dense ids from zero. On threads (shell.rs) give [`Limits::tick`]: every task
    /// of a tick then starts at its opening and a tick is one batch of the pool.
    pub workers: usize,
    /// The kinds' facts: what the driver orders reads and gates entry by (K4).
    pub kinds: KindTable,
    pub limits: Limits,
}

/// The bugs a control plants in the driver (D165, rule 78), taken as the driver's type parameter
/// `P: Planted<ReadyPlant>`: shipped code passes [`NoPlant`], whose every `has` is `false`, so
/// each check below compiles away; ready_sim.rs's and shell_sim.rs's controls pass a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadyPlant {
    /// The driver as it is.
    None,
    /// Control (a), against R1: a cell is answered while its definer is live.
    ReadBeforeMerge,
    /// Control (c), against R3: a tick's results merge one by one in completion order.
    MergeInCompletionOrder,
    /// Against R5: a parked task whose needs are all final is never made ready again.
    LostWake,
}

impl Plants for ReadyPlant {}

/// One entered task's state: one map of these (states.md, "one fact in several maps").
enum TaskState<'a> {
    /// In this or a later tick, not yet given a worker.
    Ready,
    Running(Worker),
    /// Parked on a Need (R2): run again when each cell is final.
    Waiting(Needs<'a>),
    /// Its result, merged when its tick closes (R3).
    Answered(Buffer<'a>),
    Merged,
}

/// What a parked task waits on (rule 15): the cell, when it parked, and the time it was last
/// charged up to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Mark {
    need: CellId,
    parked: Time,
    since: Time,
}

struct Entry<'a> {
    task: Task,
    /// Why the engine said to enter it: what its merge counts.
    cause: Cause,
    /// Its kind's stage (K4): what finality across stages orders by.
    stage: Stage,
    state: TaskState<'a>,
    parks: u16,
    /// While Waiting: what it waits on.
    mark: Option<Mark>,
}

impl Entry<'_> {
    /// Entered and not merged: unsettled.
    fn live(&self) -> bool {
        !matches!(self.state, TaskState::Merged)
    }
}

/// What the finality walk knows of a row (R1) for the life of one memo, over which the facts
/// are fixed: proved final, or found not final.
#[derive(Clone, Copy)]
enum Memo {
    Final,
    NotYet,
}

/// One row as the finality walk sees it, before its reads are followed.
enum Local {
    /// Merged this request, proved final, or a row nothing moves (no plan, not run).
    Final,
    /// Live or held: waited for.
    Waits,
    /// Its row is to be entered: the walk's demand.
    Moves,
    /// Its row stands now: final when every same-stage row it reads is.
    Stands,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Made; the first step is [`Event::Start`].
    Start,
    /// A tick runs: `left` of its runs have not returned.
    Ticking { left: usize },
    /// Waiting for `by`'s growth (R4).
    Growing { by: Name },
}

/// What the caller tells the driver.
pub enum Event<'a, 'p> {
    Start,
    /// A run returned.
    Ran(Returned<'a>),
    /// The growth an [`Action::Grow`] asked for: every task `by`'s result names.
    Grown { by: Name, planned: &'p [Planned] },
    /// Nothing happened by `now`: the deadline is checked.
    Tick,
}

impl Event<'_, '_> {
    fn what(&self) -> &'static str {
        match self {
            Event::Start => "Start",
            Event::Ran(_) => "Ran",
            Event::Grown { .. } => "Grown",
            Event::Tick => "Tick",
        }
    }
}

/// A run that returned (R3: in any order within its tick).
pub struct Returned<'a> {
    pub worker: Worker,
    pub name: Name,
    /// What its kind gave.
    pub ran: Ran<'a>,
    /// What it wrote beside its result.
    pub out: Out<'a>,
    /// How long it ran on its worker, on the caller's clock (rule 15).
    pub took: Time,
    /// What its host proved final, or the engine error the host met (R1).
    pub learned: Learned<'a>,
}

/// What the driver asks the caller to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Run `task` on `worker`; answer with [`Event::Ran`].
    Run { worker: Worker, task: Task },
    /// Plan `by`'s growth through its kind; answer with [`Event::Grown`].
    Grow { by: Name },
}

/// What the driver waits on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    Start,
    Worker { worker: Worker, name: Name },
    Grown { by: Name },
}

/// A parked task as a deadline names it (R5, rule 15): the cell it waits on and when it parked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Parked {
    pub name: Name,
    pub need: CellId,
    pub since: Time,
}

/// Why a request stopped (rule 3): each names its task or cell and the invariant.
#[derive(Debug)]
pub enum Error<'a> {
    NoWorkers,
    /// R5: a limit below its least legal value ([`Limits::SMALLEST`]).
    BadLimit { limit: &'static str },
    /// The engine refused (a merge, a decision, a growth).
    Engine(EngineError),
    /// A kind refused a task.
    Refused { name: Name, refusal: Refusal },
    /// An event the driver does not take in its state.
    OutOfTurn { event: &'static str },
    /// A result from a worker that was not running the task.
    NotRunning { name: Name, worker: Worker },
    /// R2.
    EnteredTwice { name: Name },
    /// K4: a candidate of a kind the table does not hold.
    NoKind { name: Name },
    /// R5: a Need of a cell the host would have answered.
    NeedAnswered { name: Name, read: CellId },
    /// R5: a Need of a cell whose producer no kind plans.
    Unplanned { name: Name, read: CellId },
    /// R5: past [`Limits::park_cap`].
    ParkedTooOften { name: Name, parks: u16, cap: u16 },
    /// R5: tasks each waiting on the next, the last on the first.
    Cycle { names: SVec<'a, Name> },
    /// R5: nothing can run and `name` waits on `read`.
    Stalled { name: Name, read: CellId },
    /// R5: a candidate left undecided, or a made row left to decide, with nothing to run.
    PendingLeft { name: Name },
    /// R5: an opening did not settle in `rounds` rounds ([`Limits::open_rounds`]).
    Unsettled { rounds: u32 },
    /// R5: a step past the deadline, waiting for `waiting`, with the task parked longest.
    Deadline { waiting: Wait, parked: Option<Parked>, deadline: Time },
    /// R5: a pool returned `name` unrun while the deadline had not passed (shell.rs `Pool`).
    Unrun { name: Name },
    /// The driver went on with nothing to run and nothing to grow (shell.rs).
    Idle,
}

impl Error<'_> {
    /// The same error with its one list (a cycle's names) copied into `alloc`: what a caller
    /// keeps past the request's arena, whole (rule 19: nothing of it dropped).
    pub fn detach_in<'b>(self, alloc: SharedAlloc<'b>) -> Error<'b> {
        match self {
            Error::Cycle { names } => {
                let mut kept: SVec<'b, Name> = Vec::with_capacity_in(names.len(), alloc);
                kept.extend_from_slice(&names);
                Error::Cycle { names: kept }
            }
            Error::NoWorkers => Error::NoWorkers,
            Error::BadLimit { limit } => Error::BadLimit { limit },
            Error::Engine(error) => Error::Engine(error),
            Error::Refused { name, refusal } => Error::Refused { name, refusal },
            Error::OutOfTurn { event } => Error::OutOfTurn { event },
            Error::NotRunning { name, worker } => Error::NotRunning { name, worker },
            Error::EnteredTwice { name } => Error::EnteredTwice { name },
            Error::NoKind { name } => Error::NoKind { name },
            Error::NeedAnswered { name, read } => Error::NeedAnswered { name, read },
            Error::Unplanned { name, read } => Error::Unplanned { name, read },
            Error::ParkedTooOften { name, parks, cap } => Error::ParkedTooOften { name, parks, cap },
            Error::Stalled { name, read } => Error::Stalled { name, read },
            Error::PendingLeft { name } => Error::PendingLeft { name },
            Error::Unsettled { rounds } => Error::Unsettled { rounds },
            Error::Deadline { waiting, parked, deadline } => Error::Deadline { waiting, parked, deadline },
            Error::Unrun { name } => Error::Unrun { name },
            Error::Idle => Error::Idle,
        }
    }
}

impl From<EngineError> for Error<'_> {
    fn from(error: EngineError) -> Self {
        Error::Engine(error)
    }
}

impl fmt::Display for Error<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoWorkers => f.write_str("the ready driver requires at least one worker"),
            Error::BadLimit { limit } => write!(f, "R5: the limit `{limit}` is below its least legal value (ready.rs `Limits::SMALLEST`, {:?})", Limits::SMALLEST),
            Error::Engine(error) if matches!(error, EngineError::ReadAfterMerge { .. }) => {
                write!(f, "R1: a read was answered before its producer was final: {error}")
            }
            Error::Engine(error) => write!(f, "the engine refused: {error}"),
            Error::Refused { name, refusal } => write!(f, "the kind refused {name:?}: {refusal}"),
            Error::OutOfTurn { event } => write!(f, "the ready driver does not take {event} now"),
            Error::NotRunning { name, worker } => write!(f, "worker {} returned {name:?}, which it was not running", worker.0),
            Error::EnteredTwice { name } => write!(
                f,
                "R2: {name:?} entered twice in one request: a candidate after it merged, or a second admission while it was live (ready.rs `admit`)"
            ),
            Error::NoKind { name } => write!(f, "K4: {name:?} is a candidate of a kind the driver's table does not hold"),
            Error::NeedAnswered { name, read } => write!(f, "R5: {name:?} needs {read:?}, which is final: no merge can meet it"),
            Error::Unplanned { name, read } => write!(f, "R5: {name:?} needs {read:?}, whose producer no kind plans: no merge can meet it"),
            Error::ParkedTooOften { name, parks, cap } => write!(f, "R5: {name:?} parked {parks} times, past the cap of {cap} (`Limits::park_cap`)"),
            Error::Cycle { names } => write!(f, "R5: a cycle of needs, each waiting on the next: {names:?}"),
            Error::Stalled { name, read } => write!(f, "R5: nothing can run and {name:?} waits on {read:?}"),
            Error::PendingLeft { name } => write!(f, "R5: {name:?} is left to decide and nothing can run"),
            Error::Unsettled { rounds } => write!(f, "R5: a tick's opening did not settle in {rounds} rounds (`Limits::open_rounds`)"),
            Error::Deadline { waiting, parked: Some(parked), deadline } => write!(
                f,
                "R5: deadline {} reached waiting for {waiting:?}; {:?} parked longest, since {}, on {:?}",
                deadline.0, parked.name, parked.since.0, parked.need
            ),
            Error::Deadline { waiting, parked: None, deadline } => write!(f, "R5: deadline {} reached waiting for {waiting:?}; no task parked", deadline.0),
            Error::Unrun { name } => write!(f, "R5: the pool returned {name:?} unrun, and the deadline has not passed"),
            Error::Idle => f.write_str("the ready driver went on with nothing to run and nothing to grow"),
        }
    }
}

impl core::error::Error for Error<'_> {}

/// A step's result: the driver going on, or quiet.
pub enum Stepped<'a, S: Scope, L = Answer<'a>, R = Paired<'a>, P = NoPlant> {
    Going(Ready<'a, S, L, R, P>),
    Quiet(Quiet<'a, S, L, R>),
}

/// A quiet driver (R6): nothing held, dormant, live or owed.
pub struct Quiet<'a, S: Scope, L = Answer<'a>, R = Paired<'a>> {
    engine: Engine<'a, S, L, R>,
    report: Report<'a>,
}

impl<'a, S: Scope, L, R> Quiet<'a, S, L, R> {
    /// Where the request's wall went (rule 15).
    pub fn report(&self) -> &Report<'a> {
        &self.report
    }
}

impl<'a, S: Scope, L: Fold<'a>, R: ReaderIndex<'a>> Quiet<'a, S, L, R> {
    /// The request's answer: the engine's finish, as the phase driver's.
    pub fn finish(self) -> Result<Finished<'a, L::Out>, EngineError> {
        self.engine.finish()
    }
}

/// One kind's part of a request (rule 15; D167 (b)): what its runs took and what its parked
/// tasks waited on.
#[derive(Debug)]
pub struct KindReport<'a> {
    pub kind: Kind,
    pub runs: u32,
    pub parks: u32,
    /// The time its runs took on workers.
    pub work: Time,
    /// The time its tasks were parked, in all.
    pub parked: Time,
    /// The five names its tasks waited on longest, longest first, by name on a tie, each with
    /// the time charged to it.
    pub waited_on: SVec<'a, (Name, Time)>,
    /// Its largest single run (D167 (a)): what bounds the stage's wall.
    pub largest: Option<(Name, Time)>,
}

/// Where a request's wall went (rule 15): per kind, work and named waits.
#[derive(Debug)]
pub struct Report<'a> {
    /// From [`Event::Start`] to the quiet step.
    pub wall: Time,
    pub ticks: u32,
    /// Ticks opened with ready tasks left for a later one: [`Limits::tick`] reached.
    pub cut_ticks: u32,
    /// The most parks of one task, against [`Limits::park_cap`].
    pub most_parks: u16,
    /// The most rounds one opening took, against [`Limits::open_rounds`].
    pub most_rounds: u32,
    /// Each kind that ran, in kind order.
    pub kinds: SVec<'a, KindReport<'a>>,
}

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "wall {}: {} ticks ({} cut at the tick size), at most {} parks of one task, at most {} rounds of one opening",
            self.wall.0, self.ticks, self.cut_ticks, self.most_parks, self.most_rounds
        )?;
        for one in self.kinds.iter() {
            write!(f, "  {}: {} runs, work {}, {} parks for {}", one.kind.name(), one.runs, one.work.0, one.parks, one.parked.0)?;
            for (at, (name, time)) in one.waited_on.iter().enumerate() {
                let lead = if at == 0 { ", waiting on " } else { ", " };
                write!(f, "{lead}{name:?} {}", time.0)?;
            }
            if let Some((name, time)) = one.largest {
                write!(f, "; largest {name:?} {}", time.0)?;
                if self.wall.0 > 0 {
                    write!(f, ", {} per mille of the wall", u128::from(time.0) * 1000 / u128::from(self.wall.0))?;
                }
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

/// One kind's running account.
struct Acc<'a> {
    kind: Kind,
    runs: u32,
    parks: u32,
    work: u64,
    parked: u64,
    waited: SMap<'a, Name, u64>,
    largest: Option<(Name, u64)>,
}

/// One request's ready-set driver over its engine.
pub struct Ready<'a, S: Scope, L = Answer<'a>, R = Paired<'a>, P = NoPlant> {
    engine: Engine<'a, S, L, R>,
    alloc: SharedAlloc<'a>,
    limits: Limits,
    /// The request's absolute deadline, set at [`Event::Start`] from [`Limits::deadline`].
    deadline: Time,
    started: Time,
    /// The time the last step was given.
    now: Time,
    kinds: KindTable,
    state: State,
    /// Every task entered this request, by name: the one map of their states (R2).
    tasks: SMap<'a, Name, Entry<'a>>,
    /// Candidates taken from the engine and not yet decided: their stage is gated (R4).
    held: SSet<'a, Name>,
    /// Made rows a moved read made candidates (invariant 10): entered on demand (R1), or once
    /// nothing else can run when a reader not live still reads them.
    dormant: SSet<'a, Name>,
    /// Rows proved final (R1), for the rest of the request.
    finals: SSet<'a, Name>,
    /// The lowest stage holding a live, held or dormant task, for `epoch`.
    lowest: Cell<Option<(u64, Option<Stage>)>>,
    /// Counts every change of what the lowest stage is computed from.
    epoch: u64,
    /// The open tick's tasks in name order; those from `next` on have no worker yet.
    queue: SVec<'a, Name>,
    next: usize,
    /// Free workers, the lowest id last (given first).
    free: SVec<'a, Worker>,
    /// The open tick's results in the order they came: kept only for the planted control (c).
    completed: SVec<'a, Name>,
    /// Merged growers that owe their growth (R4).
    to_grow: SSet<'a, Name>,
    /// Each kind's account, by [`Kind::index`] (rule 15).
    account: [Option<Acc<'a>>; key::KINDS],
    ticks: u32,
    cut_ticks: u32,
    most_parks: u16,
    most_rounds: u32,
    /// The plant seam (D165): [`NoPlant`] where it ships.
    plant: P,
}

impl<'a, S: Scope, L: LastAnswer, R: ReaderIndex<'a>, P: Planted<ReadyPlant>> Ready<'a, S, L, R, P> {
    /// A driver over `engine`, fresh from [`Engine::new_in`] (nothing entered yet), which it puts
    /// on the ready path ([`Engine::ready_path`]), with the plant `plant` ([`NoPlant`] where it
    /// ships; a control's [`ReadyPlant`] in the simulations).
    pub fn new(mut engine: Engine<'a, S, L, R>, options: Options<'a>, plant: P) -> Result<Ready<'a, S, L, R, P>, Error<'a>> {
        if options.workers == 0 {
            return Err(Error::NoWorkers);
        }
        if let Some(limit) = options.limits.below_smallest() {
            return Err(Error::BadLimit { limit });
        }
        engine.ready_path()?;
        let alloc = options.alloc;
        let mut free = Vec::with_capacity_in(options.workers, alloc);
        for at in (0..options.workers).rev() {
            free.push(Worker(at));
        }
        Ok(Ready {
            engine,
            alloc,
            limits: options.limits,
            deadline: options.limits.deadline,
            started: Time(0),
            now: Time(0),
            kinds: options.kinds,
            state: State::Start,
            tasks: BTreeMap::new_in(alloc),
            held: BTreeSet::new_in(alloc),
            dormant: BTreeSet::new_in(alloc),
            finals: BTreeSet::new_in(alloc),
            lowest: Cell::new(None),
            epoch: 0,
            queue: Vec::new_in(alloc),
            next: 0,
            free,
            completed: Vec::new_in(alloc),
            to_grow: BTreeSet::new_in(alloc),
            account: core::array::from_fn(|_| None),
            ticks: 0,
            cut_ticks: 0,
            most_parks: 0,
            most_rounds: 0,
            plant,
        })
    }

    /// What every run of the open tick reads through (K2, R1): read-only, and `Sync` when the
    /// engine is, so the shell's workers share one. Make a [`Host`] over it for each run.
    pub fn facts(&self) -> Facts<'_, 'a, S, L, R> {
        Facts {
            engine: &self.engine,
            alloc: self.alloc,
            kinds: self.kinds,
            tasks: &self.tasks,
            held: &self.held,
            to_grow: &self.to_grow,
            finals: &self.finals,
            lowest: self.lowest_unsettled(),
            reads_early: self.reads_early(),
        }
    }

    /// The request's absolute deadline: the shell's pool starts no run after it.
    pub fn deadline(&self) -> Time {
        self.deadline
    }

    /// What the driver waits on and its deadline; `None` before the first step's tick has a
    /// running task.
    pub fn wait(&self) -> Option<(Wait, Time)> {
        let waiting = match self.state {
            State::Start => Wait::Start,
            State::Growing { by } => Wait::Grown { by },
            State::Ticking { .. } => self.tasks.iter().find_map(|(name, entry)| match entry.state {
                TaskState::Running(worker) => Some(Wait::Worker { worker, name: *name }),
                _ => None,
            })?,
        };
        Some((waiting, self.deadline))
    }

    /// One event at `now`; the actions to perform are appended to `out` (R6: the driver is
    /// consumed, and given back unless it stopped or is quiet).
    pub fn step(mut self, now: Time, event: Event<'a, '_>, out: &mut SVec<'_, Action>) -> Result<Stepped<'a, S, L, R, P>, Error<'a>> {
        self.now = now;
        if self.state == State::Start && matches!(event, Event::Start) {
            self.started = now;
            self.deadline = Time(now.0.saturating_add(self.limits.deadline.0));
        }
        if now.0 > self.deadline.0 {
            let waiting = self.wait().map_or(Wait::Start, |(waiting, _)| waiting);
            return Err(Error::Deadline { waiting, parked: self.longest_parked(), deadline: self.deadline });
        }
        match (self.state, event) {
            (State::Start, Event::Start) => self.open(out),
            (State::Ticking { left }, Event::Ran(returned)) => self.ran(left, returned, out),
            (State::Growing { by }, Event::Grown { by: grown, planned }) if grown == by => {
                self.engine.grow(by, planned)?;
                self.to_grow.remove(&by);
                self.changed();
                self.open(out)
            }
            (_, Event::Tick) => Ok(Stepped::Going(self)),
            (_, event) => Err(Error::OutOfTurn { event: event.what() }),
        }
    }

    /// Opens the next tick (R4: growth first), or ends quiet.
    fn open(mut self, out: &mut SVec<'_, Action>) -> Result<Stepped<'a, S, L, R, P>, Error<'a>> {
        if let Some(&by) = self.to_grow.first() {
            out.push(Action::Grow { by });
            self.state = State::Growing { by };
            return Ok(Stepped::Going(self));
        }
        let rounds = self.limits.open_rounds;
        let mut settled = None;
        for round in 1..=rounds {
            self.take_candidates();
            self.decide_held()?;
            self.demand()?;
            if self.tasks.values().any(|entry| matches!(entry.state, TaskState::Ready)) {
                settled = Some(round);
                break;
            }
            // Nothing to run: the made rows a reader not live still reads are entered. Orphans
            // are not retired here: the engine retires them once, at finish (engine.rs
            // `retire_orphans`), so where a tick ends never decides whether a record is remade.
            if self.resolve_dormant()? {
                continue;
            }
            settled = Some(round);
            break;
        }
        let Some(round) = settled else { return Err(Error::Unsettled { rounds }) };
        self.most_rounds = self.most_rounds.max(round);
        // The tick: the first `tick` ready tasks by name; the rest wait for the next one.
        self.queue.clear();
        let mut cut = false;
        for (name, entry) in self.tasks.iter() {
            if !matches!(entry.state, TaskState::Ready) {
                continue;
            }
            if self.queue.len() == self.limits.tick {
                cut = true;
                break;
            }
            self.queue.push(*name);
        }
        if self.queue.is_empty() {
            if let Some((name, read)) = self.first_waiting() {
                return Err(self.stall(name, read));
            }
            if let Some(&name) = self.held.first().or(self.dormant.first()) {
                return Err(Error::PendingLeft { name });
            }
            let report = self.report();
            return Ok(Stepped::Quiet(Quiet { engine: self.engine, report }));
        }
        self.ticks = self.ticks.saturating_add(1);
        if cut {
            self.cut_ticks = self.cut_ticks.saturating_add(1);
        }
        self.next = 0;
        self.state = State::Ticking { left: self.queue.len() };
        while self.next < self.queue.len()
            && let Some(worker) = self.free.pop()
        {
            self.start(worker, out)?;
        }
        Ok(Stepped::Going(self))
    }

    /// Gives the tick's next task to `worker`.
    fn start(&mut self, worker: Worker, out: &mut SVec<'_, Action>) -> Result<(), Error<'a>> {
        let name = self.queue[self.next];
        self.next += 1;
        let entry = self.tasks.get_mut(&name).ok_or(Error::NotRunning { name, worker })?;
        entry.state = TaskState::Running(worker);
        out.push(Action::Run { worker, task: entry.task });
        Ok(())
    }

    /// A run returned.
    fn ran(mut self, left: usize, returned: Returned<'a>, out: &mut SVec<'_, Action>) -> Result<Stepped<'a, S, L, R, P>, Error<'a>> {
        let Returned { worker, name, ran, out: written, took, learned } = returned;
        let key = match self.tasks.get(&name) {
            Some(entry) if matches!(entry.state, TaskState::Running(running) if running == worker) => entry.task.key,
            _ => return Err(Error::NotRunning { name, worker }),
        };
        // An engine error the run's host met on the walk is the request's (rule 3); what the
        // host proved final stays proved for the rest of the request.
        if let Some(error) = learned.fault {
            return Err(Error::Engine(error));
        }
        for proved in learned.finals.iter() {
            self.finals.insert(*proved);
        }
        self.charge_run(name, took, matches!(ran, Ran::Need(_)));
        let next = match ran {
            Ran::Done(outcome) => {
                let mut buffer = Buffer::new_in(self.alloc);
                let findings = written.into_made(name, &mut buffer.made).map_err(|refusal| Error::Refused { name, refusal })?;
                let (done, stands) = outcome.into_done(name, key, findings);
                buffer.push(done, stands);
                if self.plant.has(ReadyPlant::MergeInCompletionOrder) {
                    self.completed.push(name);
                }
                TaskState::Answered(buffer)
            }
            Ran::Need(needs) => self.park(worker, name, needs)?,
            Ran::Failed(refusal) => return Err(Error::Refused { name, refusal }),
        };
        if let Some(entry) = self.tasks.get_mut(&name) {
            entry.state = next;
        }
        // The worker serves the tick's next task, or is free.
        if self.next < self.queue.len() {
            self.start(worker, out)?;
        } else {
            self.free.push(worker);
        }
        let left = left.checked_sub(1).ok_or(Error::NotRunning { name, worker })?;
        if left > 0 {
            self.state = State::Ticking { left };
            return Ok(Stepped::Going(self));
        }
        self.merge_tick()?;
        self.open(out)
    }

    /// Parks `name` on `needs` (R2), refusing a need no merge can meet (R5).
    fn park(&mut self, worker: Worker, name: Name, needs: Needs<'a>) -> Result<TaskState<'a>, Error<'a>> {
        let mut memo: SMap<'a, Name, Memo> = BTreeMap::new_in(self.alloc);
        {
            let facts = self.facts();
            for read in needs.cells().iter() {
                if facts.settled(read.of, &mut memo, None)? {
                    return Err(Error::NeedAnswered { name, read: *read });
                }
                if self.kinds.of(read.of.kind).is_none() {
                    return Err(Error::Unplanned { name, read: *read });
                }
            }
        }
        self.absorb(&memo);
        let (cap, now) = (self.limits.park_cap, self.now);
        let entry = self.tasks.get_mut(&name).ok_or(Error::NotRunning { name, worker })?;
        entry.parks = entry.parks.saturating_add(1);
        if entry.parks > cap {
            return Err(Error::ParkedTooOften { name, parks: entry.parks, cap });
        }
        entry.mark = Some(Mark { need: needs.first(), parked: now, since: now });
        let parks = entry.parks;
        self.most_parks = self.most_parks.max(parks);
        Ok(TaskState::Waiting(needs))
    }

    /// Keeps what a memo proved final (R1).
    fn absorb(&mut self, memo: &SMap<'a, Name, Memo>) {
        for (name, known) in memo.iter() {
            if matches!(known, Memo::Final) {
                self.finals.insert(*name);
            }
        }
    }

    /// Something the lowest stage is computed from changed: it is asked again.
    fn changed(&mut self) {
        self.epoch += 1;
    }

    /// The engine's candidates, by name, into `held`.
    fn take_candidates(&mut self) {
        let before = self.held.len();
        self.engine.take_candidates(&mut self.held);
        if self.held.len() != before {
            self.changed();
        }
    }

    /// Decides every held candidate of a stage no growth gates, lowest stage first (R4): entered
    /// once (R2), made dormant when it is a made row, or left (it stands, or is out of scope). A
    /// candidate that is Waiting runs again and is not entered.
    fn decide_held(&mut self) -> Result<(), Error<'a>> {
        for stage in Stage::ALL {
            if self.held.is_empty() {
                break;
            }
            if self.gated(stage) {
                continue;
            }
            let mut names: SVec<'a, Name> = Vec::new_in(self.alloc);
            for name in self.held.iter() {
                match self.kinds.of(name.kind) {
                    None => return Err(Error::NoKind { name: *name }),
                    Some(facts) if facts.stage == stage => names.push(*name),
                    Some(_) => {}
                }
            }
            for name in names.iter() {
                self.held.remove(name);
                self.changed();
                match self.tasks.get(name).map(Entry::live) {
                    Some(true) => continue,
                    Some(false) => return Err(Error::EnteredTwice { name: *name }),
                    None => {}
                }
                if self.engine.made_row(*name)? {
                    self.dormant.insert(*name);
                    continue;
                }
                self.enter_one(*name, stage)?;
            }
        }
        Ok(())
    }

    /// The demand rule (R1): each waiting task whose needs are all final is ready again; for one
    /// that is not, every row its walk found to enter (`Moves`, not live, not held, of a stage no
    /// growth gates) is entered now. Each look charges the wait since the last one to what the
    /// task waited on, and names what it waits on now (rule 15).
    fn demand(&mut self) -> Result<(), Error<'a>> {
        let alloc = self.alloc;
        let mut found: SVec<'a, Name> = Vec::new_in(alloc);
        // Each waiting task and the first of its needs not final now (`None`: woken).
        let mut looked: SVec<'a, (Name, Option<CellId>)> = Vec::new_in(alloc);
        let mut memo: SMap<'a, Name, Memo> = BTreeMap::new_in(alloc);
        {
            let facts = self.facts();
            for (name, entry) in self.tasks.iter() {
                let TaskState::Waiting(needs) = &entry.state else { continue };
                let mut waits_on = None;
                for read in needs.cells().iter() {
                    if !facts.settled(read.of, &mut memo, Some(&mut found))? && waits_on.is_none() {
                        waits_on = Some(*read);
                    }
                }
                looked.push((*name, waits_on));
            }
        }
        self.absorb(&memo);
        let (now, lost) = (self.now, self.loses_wakes());
        for (name, waits_on) in looked.iter() {
            let Some(mark) = self.tasks.get(name).and_then(|entry| entry.mark) else { continue };
            self.charge_wait(name.kind, mark.need.of, now.0.saturating_sub(mark.since.0));
            let Some(entry) = self.tasks.get_mut(name) else { continue };
            match waits_on {
                None if !lost => {
                    entry.state = TaskState::Ready;
                    entry.mark = None;
                }
                None => entry.mark = Some(Mark { since: now, ..mark }),
                Some(need) => entry.mark = Some(Mark { need: *need, parked: mark.parked, since: now }),
            }
        }
        found.sort_unstable();
        found.dedup();
        for name in found.iter() {
            if self.tasks.contains_key(name) || self.held.contains(name) {
                continue;
            }
            let Some(facts) = self.kinds.of(name.kind) else { return Err(Error::NoKind { name: *name }) };
            if self.gated(facts.stage) {
                continue;
            }
            self.enter_one(*name, facts.stage)?;
        }
        Ok(())
    }

    /// Once nothing else can run: each dormant made row a reader not live still reads (engine.rs
    /// `wanted`, invariant 10) is entered, so that reader is entered after if its value moves; one
    /// no such reader reads is let go (a live reader that reads it again asks on demand). A row of
    /// a gated stage stays. `true` when anything changed.
    fn resolve_dormant(&mut self) -> Result<bool, Error<'a>> {
        if self.dormant.is_empty() {
            return Ok(false);
        }
        let dormant = core::mem::replace(&mut self.dormant, BTreeSet::new_in(self.alloc));
        let mut changed = false;
        for name in dormant.iter() {
            let Some(facts) = self.kinds.of(name.kind) else { return Err(Error::NoKind { name: *name }) };
            if self.gated(facts.stage) {
                self.dormant.insert(*name);
                continue;
            }
            changed = true;
            let tasks = &self.tasks;
            if self.engine.wanted(*name, &|reader| tasks.get(&reader).is_some_and(Entry::live))? {
                self.enter_one(*name, facts.stage)?;
            }
        }
        if changed {
            self.changed();
        }
        Ok(changed)
    }

    /// Decides `name` and enters it when the engine says so (R2's one `admit`).
    fn enter_one(&mut self, name: Name, stage: Stage) -> Result<(), Error<'a>> {
        self.dormant.remove(&name);
        self.changed();
        match self.engine.decide(name)? {
            Decision::Enter { task, cause } => admit(&mut self.tasks, task, cause, stage),
            Decision::Stands | Decision::OutOfScope => Ok(()),
        }
    }

    /// Whether a grower may still grow a task at or before `stage` (R4): one owes its growth, or
    /// anything is live or held up to the growers' stage (a task there may merge a grower or make
    /// one a candidate). Deciding a task of `stage` would pass the phase the growth lands in
    /// (engine.rs invariant 11, `GrownLate`).
    fn gated(&self, stage: Stage) -> bool {
        let Some(upto) = self.kinds.grower_stage(stage) else { return false };
        let kinds = self.kinds;
        self.to_grow.iter().any(|name| kinds.grows_by(name.kind, stage))
            || self.tasks.values().any(|entry| entry.live() && entry.stage <= upto)
            || self.held.iter().any(|name| kinds.of(name.kind).is_some_and(|facts| facts.stage <= upto))
    }

    /// The lowest stage holding a live, held or dormant task, asked once a change of state.
    fn lowest_unsettled(&self) -> Option<Stage> {
        if let Some((epoch, low)) = self.lowest.get()
            && epoch == self.epoch
        {
            return low;
        }
        let kinds = self.kinds;
        let stage_of = |name: &Name| kinds.of(name.kind).map(|facts| facts.stage);
        let low = self
            .tasks
            .values()
            .filter(|entry| entry.live())
            .map(|entry| entry.stage)
            .chain(self.held.iter().filter_map(stage_of))
            .chain(self.dormant.iter().filter_map(stage_of))
            .min();
        self.lowest.set(Some((self.epoch, low)));
        low
    }

    /// Control (a)'s planted bug: a live row's cells are answered as final.
    fn reads_early(&self) -> bool {
        self.plant.has(ReadyPlant::ReadBeforeMerge)
    }

    /// The lost-wake plant: a woken task stays parked.
    fn loses_wakes(&self) -> bool {
        self.plant.has(ReadyPlant::LostWake)
    }

    /// Files the closed tick's results (R3): every answered task, in one merge sorted by name.
    fn merge_tick(&mut self) -> Result<(), Error<'a>> {
        if self.merge_planted()? {
            return Ok(());
        }
        self.completed.clear();
        let alloc = self.alloc;
        let mut entered: SVec<'a, (Task, Cause)> = Vec::new_in(alloc);
        let mut buffers: SVec<'a, Buffer<'a>> = Vec::new_in(alloc);
        let mut merged: SVec<'a, Name> = Vec::new_in(alloc);
        for (name, entry) in self.tasks.iter_mut() {
            if !matches!(entry.state, TaskState::Answered(_)) {
                continue;
            }
            if let TaskState::Answered(buffer) = core::mem::replace(&mut entry.state, TaskState::Merged) {
                entered.push((entry.task, entry.cause));
                buffers.push(buffer);
                merged.push(*name);
            }
        }
        // A tick whose every run needed something merges nothing.
        if entered.is_empty() {
            return Ok(());
        }
        self.engine.merge_results(&entered, &mut buffers)?;
        self.owe_growth(&merged);
        self.changed();
        Ok(())
    }

    /// Control (c)'s planted bug, against R3: one merge a result, in the order the results came.
    /// `true` when it merged the tick.
    fn merge_planted(&mut self) -> Result<bool, Error<'a>> {
        if !self.plant.has(ReadyPlant::MergeInCompletionOrder) {
            return Ok(false);
        }
        let completed = core::mem::replace(&mut self.completed, Vec::new_in(self.alloc));
        for name in completed.iter() {
            let Some(entry) = self.tasks.get_mut(name) else { continue };
            if let TaskState::Answered(buffer) = core::mem::replace(&mut entry.state, TaskState::Merged) {
                let one = [(entry.task, entry.cause)];
                let mut buffers = [buffer];
                self.engine.merge_results(&one, &mut buffers)?;
                self.owe_growth(core::slice::from_ref(name));
                self.changed();
            }
        }
        Ok(true)
    }

    /// Each merged task whose kind grows owes one growth (R4).
    fn owe_growth(&mut self, merged: &[Name]) {
        for name in merged.iter() {
            if self.kinds.of(name.kind).is_some_and(|facts| facts.grows_into.is_some()) {
                self.to_grow.insert(*name);
            }
        }
    }

    /// The first parked task, by name, and its first need.
    fn first_waiting(&self) -> Option<(Name, CellId)> {
        self.tasks.iter().find_map(|(name, entry)| match &entry.state {
            TaskState::Waiting(needs) => Some((*name, needs.first())),
            _ => None,
        })
    }

    /// The task parked longest, by name on a tie, with what it waits on now (R5).
    fn longest_parked(&self) -> Option<Parked> {
        let mut longest: Option<Parked> = None;
        for (name, entry) in self.tasks.iter() {
            let (TaskState::Waiting(_), Some(mark)) = (&entry.state, entry.mark) else { continue };
            if longest.is_none_or(|held| mark.parked.0 < held.since.0) {
                longest = Some(Parked { name: *name, need: mark.need, since: mark.parked });
            }
        }
        longest
    }

    /// The account of `kind`, made on first use.
    fn acc(&mut self, kind: Kind) -> &mut Acc<'a> {
        let alloc = self.alloc;
        self.account[kind.index()].get_or_insert_with(|| Acc { kind, runs: 0, parks: 0, work: 0, parked: 0, waited: BTreeMap::new_in(alloc), largest: None })
    }

    /// One run of `name` that took `took`, a park when it `needed`.
    fn charge_run(&mut self, name: Name, took: Time, needed: bool) {
        let acc = self.acc(name.kind);
        acc.runs = acc.runs.saturating_add(1);
        acc.work = acc.work.saturating_add(took.0);
        if needed {
            acc.parks = acc.parks.saturating_add(1);
        }
        // The largest run, by name on a tie, so the report does not depend on the feed order.
        if acc.largest.is_none_or(|(most, time)| took.0 > time || (took.0 == time && name < most)) {
            acc.largest = Some((name, took.0));
        }
    }

    /// `waited` of a task of `kind` parked on a cell of `on` (rule 15).
    fn charge_wait(&mut self, kind: Kind, on: Name, waited: u64) {
        if waited == 0 {
            return;
        }
        let acc = self.acc(kind);
        acc.parked = acc.parked.saturating_add(waited);
        match acc.waited.get_mut(&on) {
            Some(time) => *time = time.saturating_add(waited),
            None => {
                acc.waited.insert(on, waited);
            }
        }
    }

    /// Where the request's wall went (rule 15), at the quiet step.
    fn report(&self) -> Report<'a> {
        let alloc = self.alloc;
        let mut kinds: SVec<'a, KindReport<'a>> = Vec::new_in(alloc);
        for acc in self.account.iter().flatten() {
            let mut waited_on: SVec<'a, (Name, Time)> = Vec::new_in(alloc);
            for (name, time) in acc.waited.iter() {
                waited_on.push((*name, Time(*time)));
            }
            waited_on.sort_unstable_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(&b.0)));
            waited_on.truncate(5);
            kinds.push(KindReport {
                kind: acc.kind,
                runs: acc.runs,
                parks: acc.parks,
                work: Time(acc.work),
                parked: Time(acc.parked),
                waited_on,
                largest: acc.largest.map(|(name, time)| (name, Time(time))),
            });
        }
        Report {
            wall: Time(self.now.0.saturating_sub(self.started.0)),
            ticks: self.ticks,
            cut_ticks: self.cut_ticks,
            most_parks: self.most_parks,
            most_rounds: self.most_rounds,
            kinds,
        }
    }

    /// R5: why nothing can run while tasks wait: a cycle among them, named, else the first wait.
    fn stall(&self, name: Name, read: CellId) -> Error<'a> {
        for (start, entry) in self.tasks.iter() {
            if !matches!(entry.state, TaskState::Waiting(_)) {
                continue;
            }
            // Follow each task's first need whose producer waits too; a name met twice closes a
            // cycle. Each step adds a distinct waiting task, so the walk ends.
            let mut path: SVec<'a, Name> = Vec::new_in(self.alloc);
            let mut at = *start;
            loop {
                if let Some(from) = path.iter().position(|seen| *seen == at) {
                    let mut names = Vec::new_in(self.alloc);
                    names.extend_from_slice(&path[from..]);
                    return Error::Cycle { names };
                }
                path.push(at);
                let next = match self.tasks.get(&at).map(|entry| &entry.state) {
                    Some(TaskState::Waiting(needs)) => needs
                        .cells()
                        .iter()
                        .map(|need| need.of)
                        .find(|of| matches!(self.tasks.get(of).map(|entry| &entry.state), Some(TaskState::Waiting(_)))),
                    _ => None,
                };
                match next {
                    Some(of) => at = of,
                    None => break,
                }
            }
        }
        Error::Stalled { name, read }
    }
}

/// The one way a task gets into the ready set (R2): every source of work (a candidate decided
/// `Enter`, a row entered on demand, a made row still wanted) comes through here, and a name is
/// admitted once a request.
fn admit<'a>(tasks: &mut SMap<'a, Name, Entry<'a>>, task: Task, cause: Cause, stage: Stage) -> Result<(), Error<'a>> {
    let name = task.planned.name;
    if tasks.contains_key(&name) {
        return Err(Error::EnteredTwice { name });
    }
    tasks.insert(name, Entry { task, cause, stage, state: TaskState::Ready, parks: 0, mark: None });
    Ok(())
}

/// What finality is computed from (R1), read-only for one tick: between two openings nothing
/// merges, so every answer it gives holds for the whole tick. `Sync` when the engine is: the
/// shell's workers share one (shell.rs).
pub struct Facts<'r, 'a, S: Scope, L = Answer<'a>, R = Paired<'a>> {
    engine: &'r Engine<'a, S, L, R>,
    alloc: SharedAlloc<'a>,
    kinds: KindTable,
    tasks: &'r SMap<'a, Name, Entry<'a>>,
    held: &'r SSet<'a, Name>,
    to_grow: &'r SSet<'a, Name>,
    finals: &'r SSet<'a, Name>,
    /// The lowest stage holding a live, held or dormant task.
    lowest: Option<Stage>,
    reads_early: bool,
}

impl<'r, 'a, S: Scope, L: LastAnswer, R: ReaderIndex<'a>> Facts<'r, 'a, S, L, R> {
    /// R1: whether `of`'s cells are final (the module's rule, as code), with `memo` the caller's
    /// own record of rows proved or found not final over these facts. With `found`, the walk goes
    /// on past the first row that is not final and gathers every row to enter on demand.
    fn settled(&self, of: Name, memo: &mut SMap<'a, Name, Memo>, mut found: Option<&mut SVec<'a, Name>>) -> Result<bool, EngineError> {
        // A kind no registered kind runs is never entered this request: its row is final when it
        // stands as the last answer stored it with nothing to move it, and never else (R5 then
        // refuses a Need of it as `Unplanned`). Of a kind the table stands (kinds.rs
        // `KindTable::stood`), a name with no row that nothing plans or owes is absent, and final.
        let Some(facts) = self.kinds.of(of.kind) else {
            return match self.kinds.is_stood(of.kind) {
                true => self.engine.stands_or_absent(of),
                false => self.engine.stands_unrun(of),
            };
        };
        let stage = facts.stage;
        // Across stages (K4): nothing unsettled before this stage, no growth owed into it.
        if self.lowest.is_some_and(|low| low < stage) {
            return Ok(false);
        }
        let kinds = self.kinds;
        if self.to_grow.iter().any(|grower| kinds.grows_by(grower.kind, stage)) {
            return Ok(false);
        }
        let gather = found.is_some();
        if self.finals.contains(&of) {
            return Ok(true);
        }
        match memo.get(&of) {
            Some(Memo::Final) => return Ok(true),
            Some(Memo::NotYet) if !gather => return Ok(false),
            _ => {}
        }
        // Within the stage: every row reached through reads of this stage is final.
        let mut seen: SSet<'a, Name> = BTreeSet::new_in(self.alloc);
        let mut stack: SVec<'a, Name> = Vec::new_in(self.alloc);
        let mut reads: SVec<'a, Name> = Vec::new_in(self.alloc);
        seen.insert(of);
        stack.push(of);
        let mut all = true;
        while let Some(at) = stack.pop() {
            match self.local(of, at, memo)? {
                Local::Final => {}
                Local::Waits => {
                    all = false;
                    if !gather {
                        break;
                    }
                }
                Local::Moves => {
                    all = false;
                    match &mut found {
                        Some(found) => found.push(at),
                        None => break,
                    }
                }
                Local::Stands => {
                    self.engine.producers_into(at, &mut reads)?;
                    for producer in reads.iter() {
                        if *producer != at && kinds.of(producer.kind).is_some_and(|facts| facts.stage == stage) && seen.insert(*producer) {
                            stack.push(*producer);
                        }
                    }
                }
            }
        }
        if all {
            for name in seen.iter() {
                memo.insert(*name, Memo::Final);
            }
        } else {
            memo.insert(of, Memo::NotYet);
        }
        Ok(all)
    }

    /// One row as the walk sees it (R1), `of` the row asked about.
    fn local(&self, of: Name, at: Name, memo: &SMap<'a, Name, Memo>) -> Result<Local, EngineError> {
        if self.finals.contains(&at) || matches!(memo.get(&at), Some(Memo::Final)) {
            return Ok(Local::Final);
        }
        if let Some(entry) = self.tasks.get(&at) {
            if !entry.live() || (at == of && self.reads_early) {
                return Ok(Local::Final);
            }
            return Ok(Local::Waits);
        }
        if self.engine.merged_now(at) {
            return Ok(Local::Final);
        }
        if self.held.contains(&at) {
            return Ok(Local::Waits);
        }
        Ok(match self.engine.standing(at)? {
            Standing::Stands => Local::Stands,
            Standing::NotRun => Local::Final,
            Standing::Moves => Local::Moves,
        })
    }
}

/// One run's host (K2, R1): the tick's shared [`Facts`], and the run's own memo of the finality
/// walk and the first engine error it met, which return to the driver in [`Learned`]. Made per
/// run, on the worker that runs it; never shared.
pub struct Host<'f, 'r, 'a, S: Scope, L = Answer<'a>, R = Paired<'a>> {
    facts: &'f Facts<'r, 'a, S, L, R>,
    memo: RefCell<SMap<'a, Name, Memo>>,
    fault: Cell<Option<EngineError>>,
}

impl<'f, 'r, 'a, S: Scope, L: LastAnswer, R: ReaderIndex<'a>> Host<'f, 'r, 'a, S, L, R> {
    pub fn new(facts: &'f Facts<'r, 'a, S, L, R>) -> Host<'f, 'r, 'a, S, L, R> {
        Host { facts, memo: RefCell::new(BTreeMap::new_in(facts.alloc)), fault: Cell::new(None) }
    }

    /// The recording host one run of `reader` reads through (K1): the only way a task asks the
    /// request.
    pub fn recorded(&self, reader: Name) -> Recorded<'_, 'a> {
        Recorded::new(self, reader, self.facts.alloc)
    }

    /// What the run's host proved final, and the engine error it met, for [`Returned::learned`].
    pub fn learned(self) -> Learned<'a> {
        let mut finals = Vec::new_in(self.facts.alloc);
        for (name, known) in self.memo.into_inner().iter() {
            if matches!(known, Memo::Final) {
                finals.push(*name);
            }
        }
        Learned { finals, fault: self.fault.into_inner() }
    }
}

/// The host's questions, answered over the engine's view and the tick's facts (K2, R1).
impl<'f, 'r, 'a, S: Scope, L: LastAnswer, R: ReaderIndex<'a>> Source for Host<'f, 'r, 'a, S, L, R> {
    fn digest(&self, id: &CellId) -> ContentId {
        self.facts.engine.view().cell(id)
    }

    /// Finality is the row's, whoever asks (R1): a reader never reads its own row's cells
    /// through the host ([`Recorded::read_made`] records a record it made). An engine error met
    /// on the walk is kept, the first one, for the driver to return (rule 3), and the read
    /// refused.
    fn is_final(&self, _reader: Name, of: Name) -> bool {
        let mut memo = self.memo.borrow_mut();
        match self.facts.settled(of, &mut memo, None) {
            Ok(settled) => settled,
            Err(error) => {
                let first = self.fault.take().unwrap_or(error);
                self.fault.set(Some(first));
                false
            }
        }
    }

    fn hit(&self, name: Name, key: Key) -> Option<RowFacts> {
        self.facts.engine.view().hit(name, key)
    }

    fn row(&self, name: Name) -> Option<RowFacts> {
        self.facts.engine.view().row(name)
    }
}

/// What one run's host learned (R1): the rows it proved final, kept by the driver for the rest
/// of the request, and the first engine error its walk met, which stops the request.
pub struct Learned<'a> {
    finals: SVec<'a, Name>,
    fault: Option<EngineError>,
}

impl<'a> Learned<'a> {
    /// Nothing learned: a run whose host was lost with an unwind (shell.rs).
    pub fn nothing(alloc: SharedAlloc<'a>) -> Learned<'a> {
        Learned { finals: Vec::new_in(alloc), fault: None }
    }
}
