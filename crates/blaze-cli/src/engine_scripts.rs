//! The `Scripts` stage of `blaze engine-check` (engine.md E6, piece "scripts"): every build
//! script's run and every module lookup as an engine task, entered once, before the names that
//! read it. Names tasks with no script producer may run ahead of this stage.
//!
//! - **Planned from text** (the unit graph and the names world, before the first phase):
//!   - wave 0: one `MacroModule` lookup a proc-macro crate (the graph record's parts, macros.rs)
//!     and one a build script's module (`Names::script_modules`, script_modules.rs), each keyed
//!     by the part source the old path names it by;
//!   - wave 1 and on: one `Script` task a run a crate of the world reads (scripts.rs `parts`),
//!     after every run of its `links` closure (blaze-proc-macro `links_closure`): its wave is one
//!     more than its producers' deepest.
//!
//!   Each task's reads are named in the plan: the graph, its module's lookup, its producers. A
//!   wrapped function that still answers `Need` names a read the plan did not see: a typed stop
//!   ([`EngineCheckError::Unseen`]), never a second entry.
//! - **Run**: workers execute the existing `script_modules::run`, `macros::run` and
//!   `scripts::pooled` implementations. The coordinator prepares their immutable inputs,
//!   receives the completed records, and files them in the engine's task order.
//! - **Read**: a run's record is marked in the names stage, whose crate task hands it to
//!   names/scope.rs `prepare` by its key (the store holds it after the phase's put).

use blaze_drive::{Input, Key, Kind, Locate, Name, TaskFailure, Why};
use blaze_engine::{Buffer, CellId, Done, Phase, Planned, SVec, Stage, Task as EngineTask, Tools};
use blaze_ids::ContentId;
use blaze_own::{BTreeMap, SharedAlloc, Vec};
use blaze_unit_graph::UnitId;
use blaze_world::Side;
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::engine_check::{EngineCheckError, Table};
use crate::engine_names::{Handles, NamesStage};
use crate::engine_tasks::WHOLE;
use crate::engine_units::{Discovered, Failure, Published, drive_task, published};
use crate::fetch::FetchEnv;
use crate::scripts::{Prepared, Ran};
use crate::work::{self, Did};

/// Where the rule that refuses a task on this path is.
const RULE: &str = "engine_scripts.rs";

struct RunPlan<'f> {
    task: EngineTask,
    script: Option<(UnitId, &'f str, &'f str, Side)>,
    native: bool,
    inputs: usize,
    readers: Vec<usize, &'f Frame>,
}

/// Only actual completions release edges. The same frontier computes the publication depths.
struct Ready<'a> {
    left: Vec<usize, &'a Frame>,
    queue: Vec<usize, &'a Frame>,
}

impl<'a> Ready<'a> {
    fn new(runs: &[RunPlan<'_>], frame: &'a Frame) -> Self {
        let mut left = Vec::with_capacity_in(runs.len(), frame);
        let mut queue = Vec::with_capacity_in(runs.len(), frame);
        for (i, run) in runs.iter().enumerate() {
            left.push(run.inputs);
            if run.inputs == 0 {
                queue.push(i);
            }
        }
        Self { left, queue }
    }

    fn finish(&mut self, readers: &[usize]) {
        for &reader in readers {
            self.left[reader] -= 1;
            if self.left[reader] == 0 {
                self.queue.push(reader);
            }
        }
    }
}

struct Previous<'f> {
    reads: &'f [(CellId, ContentId)],
    /// The stored row's out: a rerun that makes it again was entered for nothing it could see.
    out: ContentId,
}

/// A build script entered again this request whose record came out the same bytes (the profile's
/// `Scripts: identical again` lines): the script, and the `Given` cell whose answer moved and so
/// entered it (`None`: no `Given` moved, an input record did).
#[derive(Clone, Copy)]
struct IdenticalRun {
    script: Name,
    woke: Option<(Name, crate::engine_script_reads::ReadKind)>,
}

struct Completed<'f> {
    drive: blaze_drive::Task<'static>,
    made: Result<work::Made<'f>, Failure<'f>>,
    owed: bool,
    worker: Option<WorkerResult>,
}

struct WorkerResult {
    checkout: crate::engine_script_reads::ReadList,
    unkept: Option<crate::engine_script_reads::NoCell>,
    produced: bool,
    hole: bool,
}

enum ScriptWork<'f> {
    Script(crate::scripts::ScriptJob<'f>),
    Module(ModuleJob<'f>),
    #[cfg(test)]
    Double(&'static dyn ScriptRunner, Name, Option<KeptReads>, &'f [(Input, &'f [u8])]),
}

/// The worker gets only immutable bytes. Neither the coordinator's store nor its
/// single-threaded frame crosses the pool boundary.
enum ModuleJob<'a> {
    Build { wanted: crate::script_modules::Wanted<'a>, home: Option<&'a str> },
    Proc {
        crates: Option<crate::macros::MacroCrates>, home: Option<&'a str>, checkout: &'a str,
        graph: Option<Input>, initial: Option<MacroInputs<'a>>,
    },
    #[cfg(test)]
    Fixture,
}

#[derive(Clone, Copy)]
struct MacroInputs<'a> {
    graph: &'a [u8],
    lock: &'a [u8],
    members: &'a [(blaze_manifest::lock::LockId, &'a str, blaze_world::PathKind, ContentId)],
}

enum ModuleAnswer {
    Found(ContentId),
    Failed(TaskFailure, Option<blaze_own::String<SharedAlloc<'static>>>, bool),
}

enum WorkerOutput {
    Run(crate::scripts::PooledScript),
    Module(ModuleAnswer, Option<crate::macros::MacroCrates>),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ScriptLane { Native, Other, ProcLookup }

/// The one graph record a proc-module lookup was handed. The normal lookup uses
/// `get_into` when building its identity table. Its table serves all following lookups.
struct LookupStore<'a> {
    name: Name,
    graph: Option<(Key, &'a [u8])>,
}

impl LookupStore<'_> {
    fn refused(&self) -> CliError {
        EngineCheckError::Unseen { task: self.name, read: Name::unit_graph() }.into()
    }
}

impl<'m> crate::store::Store<'m> for LookupStore<'_> {
    fn get(&mut self, _: &Cx, _: Key) -> Result<Option<crate::store::Held<'m>>, CliError> { Err(self.refused()) }
    fn get_into(&mut self, _: &Cx, key: Key, into: &mut bstd::mem::Bytes<'_>) -> Result<bool, CliError> {
        let Some((given, bytes)) = self.graph else { return Err(self.refused()) };
        if given != key { return Err(self.refused()); }
        into.try_extend(bytes).map_err(|error| CliError::Store(crate::store::StoreFailure::Copy(error)))?;
        Ok(true)
    }
    fn put(&mut self, _: &Cx, _: Key, _: &[u8]) -> Result<(), CliError> { Err(self.refused()) }
    fn put_parts(&mut self, _: &Cx, _: Key, _: &[&[u8]]) -> Result<(), CliError> { Err(self.refused()) }
    fn put_record(&mut self, _: &Cx, _: Key, _: blaze_keyed::kind::Tag, _: &[&[u8]], _: &[u8]) -> Result<blaze_keyed::store::Put, CliError> { Err(self.refused()) }
    fn index_beside(&mut self, _: &Cx, _: Key, _: &[Key]) -> Result<(), CliError> { Err(self.refused()) }
    fn map_all(&mut self, _: &Cx) -> Result<(), CliError> { Err(self.refused()) }
    fn after_answer(&mut self, _: &Cx, _: &mut blaze_drive::profile::Profile) -> Result<(), CliError> { Err(self.refused()) }
    fn public_crates(&mut self, _: &[ContentId]) { unreachable!("a module lookup has no public crate list") }
    fn toolchain_crates(&mut self, _: &[ContentId]) { unreachable!("a module lookup has no toolchain crate list") }
    fn keep_with_toolchain(&mut self, _: Key) -> Result<(), CliError> { Err(self.refused()) }
    fn kept(&self) -> crate::store::StoreKept { crate::store::StoreKept::default() }
}

impl ModuleJob<'_> {
    fn run(self, cx: &Cx, task: EngineTask, heap: &'static blaze_own::Heap) -> Result<WorkerOutput, CliError> {
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let alloc = erased.handle();
        let name = task.planned.name;
        let drive = |inputs| drive_task(name, task.key, task.planned.source, inputs, Vec::new_in(alloc), alloc);
        let (did, crates) = match self {
            Self::Build { wanted, home } => (crate::script_modules::run(cx, home, wanted, &drive(Vec::new_in(alloc)), frame)?, None),
            #[cfg(test)]
            Self::Fixture => return Ok(WorkerOutput::Module(ModuleAnswer::Found(name.subject), None)),
            Self::Proc { mut crates, home, checkout, graph, initial } => {
                let mut handed = LookupStore { name, graph: initial.zip(graph).map(|(input, graph)| (graph.at, input.graph)) };
                let mut inputs = Vec::new_in(alloc);
                inputs.extend(graph);
                let mut rows = crate::sweep::Rows::new_in(frame);
                let mut members = Vec::new_in(frame);
                let lock = match initial {
                    Some(input) => {
                        for &(id, dir, kind, digest) in input.members {
                            members.push(crate::graph::CheckoutDir { id, dir: blaze_unit_graph::Text::from_str_in(dir, heap.alloc()), kind });
                            rows.push(crate::sweep::LeafKind::MacroFiles, 0, dir, None, digest);
                        }
                        match crate::fetch::Lockfile::read(input.lock, heap, frame) {
                            Ok(lock) => Some(lock),
                            Err(words) => {
                                let failed = TaskFailure { name, why: Why::Refused { what: "the macro crates could not be listed", rule: "macros.rs" } };
                                let said = blaze_own::format_in!(heap.shared(), "the proc-macro crates of the graph could not be listed: {words}");
                                return Ok(WorkerOutput::Module(ModuleAnswer::Failed(failed, Some(said), true), None));
                            }
                        }
                    }
                    None => None,
                };
                let place = crate::macros::Place { home, checkout, members: Some(&members), rows: &rows };
                let did = crate::macros::run(cx, &mut handed, lock.as_ref(), &mut crates, place, &drive(inputs), frame, frame)?;
                (did, crates)
            }
        };
        let answer = match did {
            Did::Done(made) => ModuleAnswer::Found(made.out),
            Did::Failed(failed) => ModuleAnswer::Failed(failed, None, false),
            Did::FailedSaying(failed, words) => ModuleAnswer::Failed(failed, Some(blaze_own::String::from_str_in(words, heap.shared())), false),
            Did::Hole(failed, words) => ModuleAnswer::Failed(failed, Some(blaze_own::String::from_str_in(words, heap.shared())), true),
            Did::Need(wants) => return Err(EngineCheckError::Unseen { task: name, read: wants.first().map_or(name, |want| want.name) }.into()),
        };
        Ok(WorkerOutput::Module(answer, crates))
    }
}

enum Dispatch<'f, 'j> {
    Run(ScriptWork<'j>, blaze_drive::Task<'static>),
    Answered(Completed<'f>),
}

/// Also signals when the pool rejects a job before calling its closure. Joining the handle
/// then gives the original task error. Each flag is set once. The park token covers a
/// completion racing the coordinator's scan, including when several wakes coalesce.
pub(crate) struct Notice<'a> {
    pub(crate) finished: &'a bstd::sync::Flag,
    pub(crate) waiter: &'a blaze_core::wait::Parker,
}

impl Drop for Notice<'_> {
    fn drop(&mut self) {
        self.finished.set();
        self.waiter.unpark();
    }
}

struct WaitClock<'a>(&'a bstd::clock::Clock);

impl blaze_core::wait::Clock for WaitClock<'_> {
    fn now(&self) -> core::time::Duration {
        self.0.now().since_start()
    }
}

pub(crate) fn wait_finished(
    cx: &Cx, clock: &bstd::clock::Clock, waiter: &blaze_core::wait::Parker,
    ready: impl Fn() -> bool, task: &'static str, running: usize,
) -> Result<(), CliError> {
    let cx = cx.span(task);
    while !ready() {
        let why = match waiter.park(task, cx.deadline().finite(), &WaitClock(clock)) {
            Ok(blaze_core::wait::Waited::Ready(())) => continue,
            Ok(blaze_core::wait::Waited::Deadline) => bstd::task::TaskWhy::Unended {
                running, stopped: bstd::Stopped::Deadline { span: task, at: cx.deadline().get() },
            },
            Err(error) => bstd::task::TaskWhy::Failed(error.to_string()),
        };
        return Err(CliError::Pool(bstd::task::TaskError { scope: cx.name(), task, why }));
    }
    Ok(())
}

/// One memoized traversal of the fixed script plan per change to the engine's pending set.
pub(crate) struct ScriptReady<'a> {
    visited: BTreeMap<Name, (u64, bool), &'a Frame>,
    stack: Vec<(Name, bool), &'a Frame>,
    generation: u64,
}

impl<'a> ScriptReady<'a> {
    pub(crate) fn new(frame: &'a Frame) -> Self {
        Self { visited: BTreeMap::new_in(frame), stack: Vec::new_in(frame), generation: 0 }
    }

    pub(crate) fn clear(&mut self) { self.generation += 1; }
}

struct Running<'s> {
    task: EngineTask,
    drive: blaze_drive::Task<'static>,
    finished: &'s bstd::sync::Flag,
    output: &'s blaze_core::lock::Lock<Option<(WorkerOutput, u64, u64)>>,
    handle: bstd::task::Task<'s, ()>,
    lane: ScriptLane,
}

/// Admission to the single physical pool, shared by both coordinator-side queues. The
/// simulated clock holds a slot until the same observation that joins the real worker.
pub(crate) struct PoolSlots {
    width: usize,
    used: core::cell::Cell<usize>,
}

impl PoolSlots {
    pub(crate) fn free(&self) -> bool { self.used.get() < self.width }
    pub(crate) fn take(&self) {
        assert!(self.free(), "admitting a worker past the configured pool width");
        self.used.set(self.used.get() + 1);
    }
    pub(crate) fn release(&self) { self.used.set(self.used.get() - 1); }
}

/// Handles live across driver iterations. Jobs borrow immutable mappings in the script
/// arena, so filing a result or preparing a Names task never borrows a running job.
pub(crate) struct ScriptQueue<'s> {
    scope: &'s bstd::task::Scope<'s>,
    jobs: &'s Frame,
    waiter: &'s blaze_core::wait::Parker,
    slots: &'s PoolSlots,
    width: usize,
    native_running: usize,
    proc_running: bool,
    macro_inputs: Option<MacroInputs<'s>>,
    order: BTreeMap<(bool, Name), EngineTask, &'s Frame>,
    waiting: BTreeMap<(ScriptLane, bool, Name), EngineTask, &'s Frame>,
    running: BTreeMap<Name, Running<'s>, &'s Frame>,
    answered: BTreeMap<Name, (EngineTask, u64, u64), &'s Frame>,
    joined_later: Vec<bstd::task::Task<'s, ()>, &'s Frame>,
    clock: &'s bstd::clock::Clock,
    #[cfg(test)]
    calendar: BTreeMap<(u64, Name), (), &'s Frame>,
}

impl<'s> ScriptQueue<'s> {
    pub(crate) fn new(scope: &'s bstd::task::Scope<'s>, jobs: &'s Frame, handles: &Handles<'s, '_>) -> Self {
        let width = scope.workers();
        #[cfg(test)]
        let width = if runner_clock().is_some() { handles.workers.get() } else { width };
        let width = width.min(handles.workers.get()) as usize;
        Self {
            scope, jobs, waiter: jobs.alloc(blaze_core::wait::Parker::new()),
            slots: jobs.alloc(PoolSlots { width, used: core::cell::Cell::new(0) }),
            width, native_running: 0,
            proc_running: false, macro_inputs: None,
            order: BTreeMap::new_in(jobs), waiting: BTreeMap::new_in(jobs), running: BTreeMap::new_in(jobs),
            answered: BTreeMap::new_in(jobs),
            joined_later: Vec::new_in(jobs), clock: &handles.env.clock,
            #[cfg(test)]
            calendar: BTreeMap::new_in(jobs),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// True when every queued script task is an instantaneously-answered `Given` cell (zero
    /// `Script` or `MacroModule` tasks queued, waiting, or running). Finishing `Given` cells
    /// before admitting `Stage::Names` is worker-count invariant, keeps `Scripts` at 0 crate
    /// records read when no script runs, and lets `Names` run at full width once it is known
    /// whether any `Script` woke up.
    fn only_given(&self) -> bool {
        !self.order.is_empty() && self.order.values().all(|task| task.planned.name.kind == Kind::Given)
    }

    pub(crate) fn is_finished(&self, name: Name) -> bool { self.answered.contains_key(&name) }

    pub(crate) fn waiter(&self) -> &'s blaze_core::wait::Parker { self.waiter }
    pub(crate) fn slots(&self) -> &'s PoolSlots { self.slots }

    pub(crate) fn has_completion(&self) -> bool { self.running.values().any(|run| run.finished.is_set()) }

    pub(crate) fn wait_running(&self, cx: &Cx) -> Result<(), CliError> {
        wait_finished(cx, self.clock, self.waiter, || self.running.values().all(|run| run.finished.is_set()),
            "all submitted Scripts workers to return", self.running.len())
    }

    pub(crate) fn drain(&mut self, cx: &Cx) -> Result<(), CliError> {
        let mut failure = None;
        while let Some((_, run)) = self.running.pop_first() {
            if let Err(error) = run.handle.join(cx) { failure.get_or_insert(error); }
            // Frame cells have no destructor. Drop an unconsumed worker result on error.
            drop(run.output.lock().take());
            self.slots.release();
        }
        for handle in self.joined_later.drain(..) {
            if let Err(error) = handle.join(cx) { failure.get_or_insert(error); }
        }
        failure.map_or(Ok(()), |error| Err(CliError::Pool(error)))
    }

    pub(crate) fn first(&self) -> Option<EngineTask> {
        self.order.first_key_value().map(|(_, task)| *task)
    }

    pub(crate) fn urgent(&self) -> bool {
        self.order.first_key_value().is_some_and(|((leaf, _), _)| !leaf)
    }

    #[cfg(test)]
    pub(crate) fn next_tick(&self) -> Option<u64> {
        self.calendar.first_key_value().map(|((tick, _), _)| *tick)
    }
}

impl<'f> Completed<'f> {
    fn answer(drive: blaze_drive::Task<'static>, did: Did<'f>) -> Result<Self, CliError> {
        let owed = !stands_on_reads(&did);
        let made = published(drive.name, did)?;
        Ok(Self { drive, made, owed, worker: None })
    }

    fn pooled(drive: blaze_drive::Task<'static>, pooled: crate::scripts::PooledScript, frame: &'f Frame) -> Result<Self, CliError> {
        let name = drive.name;
        let did = match pooled.did {
            Err(why) => Did::Failed(TaskFailure { name, why }),
            Ok(Ran::Made(record)) => record.into_held(frame).map_or_else(|_| work::held_refused(name), Did::Done),
            Ok(Ran::Failed { what, words, hole }) => {
                let failure = TaskFailure { name, why: Why::Refused { what, rule: crate::scripts::RULE } };
                let said = core::str::from_utf8(&words).map_err(|_| CliError::Text { what: "a build script's failure words are not UTF-8" })?;
                let words = work::held_str(said, frame);
                if hole { Did::Hole(failure, words) } else { Did::FailedSaying(failure, words) }
            }
        };
        let produced = matches!(did, Did::Done(_));
        let hole = matches!(did, Did::Hole(..));
        let mut answer = Self::answer(drive, did)?;
        answer.worker = Some(WorkerResult { checkout: pooled.checkout, unkept: pooled.unkept, produced, hole });
        Ok(answer)
    }
}

/// The stage over one command's scripts and module lookups.
pub(crate) struct ScriptsStage<'f> {
    /// The command's scripts, opened by the first `Script` job (scripts.rs `Scripts::open`);
    /// dropped with the stage, which ends the native runner's jobserver.
    scripts: Option<Result<crate::scripts::Scripts<'f>, &'f str>>,
    /// The graph's proc-macro crates with their identities, listed by the first lookup.
    macros: Option<crate::macros::MacroCrates>,
    /// Each planned task's reads, by name, sorted: what its plan hands it.
    reads: Vec<(Name, Vec<Name, &'f Frame>), &'f Frame>,
    runs: Vec<RunPlan<'f>, &'f Frame>,
    /// Results received ahead of their turn to be filed by the driver.
    completed: BTreeMap<Name, Completed<'f>, &'f Frame>,
    previous: BTreeMap<Name, Previous<'f>, &'f Frame>,
    /// Each record entered or handed so far (the graph, then this stage's), by name: its key and
    /// out, `None` when it failed.
    made: Vec<(Name, Option<(Key, ContentId)>), &'f Frame>,
    /// Each task's result, in the order entered.
    pub(crate) results: Vec<Published<'f>, &'f Frame>,
    /// Each planned `Given` cell of a build script's read, by name, sorted, with today's answer
    /// from the sweep's row (engine_script_reads.rs `plan`).
    given: Vec<(Name, ContentId, crate::engine_script_reads::ReadKind), &'f Frame>,
    /// Each read of the checkout a run of this request made, with the script that read it: the
    /// leaf rows the answer keeps, and the `Given` records of reads no plan holds.
    fresh: Vec<(Name, crate::engine_script_reads::CheckoutRead), &'f Frame>,
    /// The kinds' tools, from [`ScriptsStage::plan`]: a `Given` record's key.
    tools: Option<Tools>,
    /// The bug a test plants in the runs' read lists (engine_script_reads.rs `ReadsPlant`).
    pub(crate) plant: crate::engine_script_reads::ReadsPlant,
    /// The request planned the reads' `Given` cells ([`ScriptsStage::plan_reads`]): only then are
    /// a member's reads kept as cells.
    reads_planned: bool,
    pub(crate) status_view: Option<crate::engine_script_reads::StatusView>,
    /// A test's runner in place of each `Script` task's job and run ([`ScriptRunner`]).
    #[cfg(test)]
    runner: Option<&'static dyn ScriptRunner>,
    #[cfg(test)]
    tick: u64,
    /// Runs this request whose record was kept with their reads as cells.
    kept_runs: u32,
    /// Runs this request that were holes, and the first one's words (its package, why).
    holes: (u32, Option<std::string::String>),
    /// Runs this request whose record is right for this command only (scripts.rs
    /// `PooledScript::unkept`), each with why, in the order run: used, owed, said once each.
    unkept: Vec<(Name, UnitId, &'f str, &'f str, Side, crate::engine_script_reads::NoCell), &'f Frame>,
    /// Scripts run this request that had a stored row ([`Previous`]): entered again.
    reran: u32,
    /// Of those, each whose record came out the bytes its stored row held, with what woke it.
    identical: Vec<IdenticalRun, &'f Frame>,
}

/// The note of a run whose outputs this command used and did not keep (rule 19: what, why).
pub(crate) struct UnkeptWords<'n>(pub(crate) Name, pub(crate) &'n crate::engine_script_reads::NoCell);

impl core::fmt::Display for UnkeptWords<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: its outputs are used by this command and not kept, and it runs again next command: {} (engine_scripts.rs `run_phase`)", self.0, self.1)
    }
}

/// What runs a `Script` task in a test: in place of its job (which opens the wasm runtime and
/// the real checkout) and its run, on the same workers as a production job. The production stage has no runner
/// and no such field; a test sets one for its thread with [`with_runner`].
#[cfg(test)]
pub(crate) type KeptReads = (crate::engine_script_reads::ReadsPlant, Option<crate::engine_script_reads::StatusView>);

#[cfg(test)]
pub(crate) trait ScriptRunner: Sync {
    /// The run of `name` over the checkout `root`; `kept`: its reads are cells, with the plant
    /// (`None`: they are a hole, as on the old path).
    fn run(&self, cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: Name, kept: Option<KeptReads>, producers: &[(Input, &[u8])]) -> crate::scripts::PooledScript;

    fn checkout_tree(&self) -> Option<blaze_proc_macro::script_sandbox::Tree> { None }

    fn clock(&self) -> Option<&dyn ScriptClock> { None }

    /// The bug planted in the sweep's keep of the status answers (engine_script_reads.rs).
    fn coverage_plant(&self) -> crate::engine_script_reads::CoveragePlant { crate::engine_script_reads::CoveragePlant::None }
}

#[cfg(test)]
pub(crate) fn runner_coverage_plant() -> crate::engine_script_reads::CoveragePlant {
    RUNNER.with(|cell| cell.get().map_or(crate::engine_script_reads::CoveragePlant::None, |runner| runner.coverage_plant()))
}

/// A simulated worker holds its slot and result until its finish tick. Admission, producer
/// handoff and publication use the real driver. Ties are broken by the run's plan index.
#[cfg(test)]
pub(crate) trait ScriptClock {
    fn now(&self) -> u64 { 0 }
    fn start(&self, name: Name, tick: u64) -> u64;
    fn end(&self, name: Name, tick: u64);
    fn omit_links_edge(&self, _producer: Name, _reader: Name) -> bool { false }
    fn names_admitted(&self, _name: Name, _krate: &str) -> u64 { 0 }
    fn names_entered(&self, _name: Name, _krate: &str) {}
    fn names_finished(&self, _name: Name, _tick: u64) {}
    fn global_names_barrier(&self) -> bool { false }
    fn drop_names_producer(&self) -> bool { false }
}

#[cfg(test)]
std::thread_local! {
    static RUNNER: core::cell::Cell<Option<&'static dyn ScriptRunner>> = const { core::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn runner_tree() -> Option<blaze_proc_macro::script_sandbox::Tree> {
    RUNNER.with(|cell| cell.get().and_then(|runner| runner.checkout_tree()))
}

#[cfg(test)]
pub(crate) fn runner_clock() -> Option<&'static dyn ScriptClock> {
    RUNNER.with(|cell| cell.get().and_then(|runner| runner.clock()))
}

/// `work` with `runner` in place of every `Script` task's run of the stages made on this thread.
#[cfg(test)]
pub(crate) fn with_runner<R>(runner: &'static dyn ScriptRunner, work: impl FnOnce() -> R) -> R {
    struct Restore(Option<&'static dyn ScriptRunner>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RUNNER.with(|cell| cell.set(self.0));
        }
    }
    let _restore = Restore(RUNNER.with(|cell| cell.replace(Some(runner))));
    work()
}

impl<'f> ScriptsStage<'f> {
    pub(crate) fn in_plan(&self) -> bool { !self.runs.is_empty() }
    pub(crate) fn new(frame: &'f Frame) -> ScriptsStage<'f> {
        ScriptsStage {
            scripts: None,
            macros: None,
            reads: Vec::new_in(frame),
            runs: Vec::new_in(frame),
            completed: BTreeMap::new_in(frame),
            previous: BTreeMap::new_in(frame),
            made: Vec::new_in(frame),
            results: Vec::new_in(frame),
            given: Vec::new_in(frame),
            fresh: Vec::new_in(frame),
            tools: None,
            plant: crate::engine_script_reads::ReadsPlant::None,
            reads_planned: false,
            status_view: None,
            #[cfg(test)]
            runner: RUNNER.with(|cell| cell.get()),
            #[cfg(test)]
            tick: 0,
            kept_runs: 0,
            holes: (0, None),
            unkept: Vec::new_in(frame),
            reran: 0,
            identical: Vec::new_in(frame),
        }
    }

    /// The `Given` cells planned this request whose answer moved since the stored answer read
    /// them: a cell `previous` read at another digest than today's.
    fn moved_givens(&self) -> u32 {
        let mut moved = 0u32;
        for (name, answer, _) in self.given.iter() {
            let was = self.previous.values().find_map(|row| row.reads.iter().find(|(cell, _)| cell.of == *name).map(|(_, digest)| *digest));
            if was.is_some_and(|was| was != *answer) {
                moved = moved.saturating_add(1);
            }
        }
        moved
    }

    /// What woke the script `name` (its stored row `previous`): the first `Given` cell it read
    /// whose answer is another today, with its kind.
    fn woke_by(&self, previous: &Previous<'_>) -> Option<(Name, crate::engine_script_reads::ReadKind)> {
        previous.reads.iter().filter(|(cell, _)| cell.of.kind == Kind::Given).find_map(|(cell, digest)| {
            let at = self.given.binary_search_by_key(&cell.of, |(name, _, _)| *name).ok()?;
            let (name, answer, kind) = self.given[at];
            (answer != *digest).then_some((name, kind))
        })
    }

    /// Each run this request whose outputs it used and did not keep, with why: one note each
    /// (engine_check.rs prints them, [`UnkeptWords`]).
    pub(crate) fn unkept(&self) -> impl Iterator<Item = (Name, &crate::engine_script_reads::NoCell)> + '_ {
        self.unkept.iter().map(|(name, _, _, _, _, why)| (*name, why))
    }

    /// The profile's line of the scripts' reads of the checkout: the cells this request planned
    /// or made, by kind; the runs kept; the holes, and the first one's words.
    pub(crate) fn profile(&self, text: &mut blaze_own::String<&Frame>) {
        use core::fmt::Write as _;
        use crate::engine_script_reads::ReadKind;
        let mut cells = blaze_own::BTreeMap::new_in(*text.allocator());
        for (name, _, kind) in self.given.iter() {
            cells.insert(*name, *kind);
        }
        for (_, read) in self.fresh.iter() {
            cells.insert(read.name(), read.kind);
        }
        let count = |kind: ReadKind| cells.values().filter(|one| **one == kind).count();
        let (files, listings, stats, gits) = (count(ReadKind::File), count(ReadKind::Listing), count(ReadKind::Stat), count(ReadKind::Git));
        let _ = write!(text, "  Scripts: script reads: {} cells ({files} files, {listings} listings, {stats} stats, {gits} git facts); kept {} runs; unkept {}; holes {}", cells.len(), self.kept_runs, self.unkept.len(), self.holes.0);
        let _ = match &self.holes.1 {
            Some(first) => writeln!(text, " (first: {first})"),
            None => writeln!(text),
        };
        for (_, unit, package, version, side, why) in &self.unkept {
            let (variant, subject) = why.first_rejected();
            blaze_own::format_into!(text, "  Scripts: unkept package={package} {version} unit={} side={side:?} subject={subject:?} NoCell={variant}\n", unit.0);
        }
        // The floor for an edit that moves no read of a script: 0 cells moved, 0 entered. A cell
        // that moved and woke a script that made the same bytes is named, with its path as the
        // run read it, so a coarse or over-wide read is seen by name (rule 77).
        let _ = writeln!(
            text,
            "  Scripts: given cells moved {}; scripts entered again {}; identical again {} (floor: 0, 0, 0 for an edit no script read); shared setups opened {} (floor: 0 when no script runs)",
            self.moved_givens(),
            self.reran,
            self.identical.len(),
            match &self.scripts {
                Some(Ok(scripts)) => scripts.setups_opened(),
                Some(Err(_)) | None => 0,
            },
        );
        for run in self.identical.iter() {
            match run.woke {
                Some((cell, kind)) => {
                    let path = self.fresh.iter().find(|(by, read)| *by == run.script && read.name() == cell).map(|(_, read)| read.path.as_str());
                    let _ = writeln!(text, "  Scripts: identical again {} woken by {cell} ({kind:?} {path:?})", run.script);
                }
                None => {
                    let _ = writeln!(text, "  Scripts: identical again {} woken by an input record, no Given cell moved", run.script);
                }
            }
        }
    }

    /// The `Given` tasks of the build scripts' reads of the checkout, one a script-read row of
    /// today's leaf rows (engine_check.rs hook H1: `planned.extend(scripts.plan_reads(&followed.now, alloc))`).
    pub(crate) fn plan_reads<'h>(&mut self, rows: &crate::sweep::Rows<'_>, alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
        self.reads_planned = true;
        let reads = crate::engine_script_reads::plan(rows, alloc);
        let mut planned = SVec::with_capacity_in(reads.len(), alloc);
        for (one, answer, kind) in reads.iter() {
            self.given.push((one.name, *answer, *kind));
            planned.push(*one);
        }
        self.given.sort_unstable_by_key(|(name, _, _)| *name);
        self.given.dedup_by_key(|(name, _, _)| *name);
        planned
    }

    /// Onto `out`, the leaf rows of the checkout reads the answer still reads: the build
    /// scripts', then `more` (the bodies' computed data includes, engine_bodies.rs
    /// `BodiesStage::data_files`), one leaf a cell; `reads(name)` says whether a row of the
    /// final answer reads the cell `name` (engine_check.rs hook H2, before the answer's rows are
    /// saved).
    #[expect(clippy::too_many_arguments, reason = "the tail's script-row merger takes its inputs by name")]
    pub(crate) fn script_rows<'r>(
        &'r self,
        followed: Option<&crate::sweep::Rows<'_>>,
        more: impl Iterator<Item = (crate::engine_script_reads::ReadKind, &'r str, ContentId)>,
        followed_all_kept: bool,
        reads: impl Fn(Name, bool) -> bool,
        is_direct: impl Fn(crate::engine_script_reads::ReadKind, &str) -> bool,
        stamp_of: impl Fn(crate::engine_script_reads::ReadKind, &str, ContentId) -> Option<crate::sweep::Stamp>,
        out: &mut crate::sweep::Rows<'_>,
    ) {
        let fresh = self.fresh.iter().map(|(_, read)| (read.kind, read.path.as_str(), read.answer));
        crate::engine_script_reads::keep_rows_with(followed, fresh, more, followed_all_kept, reads, is_direct, stamp_of, out);
    }

    /// The result of `name`, if this stage entered it.
    pub(crate) fn result(&self, name: Name) -> Option<&Published<'f>> {
        self.results.iter().find(|one| one.name == name)
    }

    /// Every producer whose pending entry could still change this script candidate. Given
    /// reads are known from the previous run. The engine looks up only these named tasks.
    pub(crate) fn producers(&self, name: Name) -> impl Iterator<Item = Name> + '_ {
        let planned = self.reads.binary_search_by_key(&name, |(name, _)| *name).ok()
            .into_iter().flat_map(|at| self.reads[at].1.iter().copied());
        let checkout = self.previous.get(&name).into_iter().flat_map(|row| row.reads.iter())
            .filter(|(cell, _)| cell.of.kind == Kind::Given).map(|(cell, _)| cell.of);
        planned.chain(checkout)
    }

    pub(crate) fn producers_ready(&self, name: Name, unsettled: &dyn Fn(Name) -> bool, scratch: &mut ScriptReady<'_>) -> bool {
        let generation = scratch.generation;
        scratch.stack.push((name, false));
        while let Some((at, expanded)) = scratch.stack.pop() {
            if scratch.visited.get(&at).is_some_and(|(stamp, _)| *stamp == generation) {
                continue;
            }
            if expanded {
                let ready = self.producers(at).all(|dep| !unsettled(dep) && scratch.visited[&dep].1);
                scratch.visited.insert(at, (generation, ready));
            } else {
                scratch.stack.push((at, true));
                scratch.stack.extend(self.producers(at).map(|dep| (dep, false)));
            }
        }
        scratch.visited[&name].1
    }

    /// The stage's tasks: the lookups in wave 0, each run after its `links` producers. Nothing
    /// when the graph was not made (no lock, or its task failed): no crate then has names.
    pub(crate) fn plan<'h>(&mut self, names: &NamesStage<'_>, discovered: &Discovered<'_>, tools: Tools, env: &FetchEnv<'_>, alloc: SharedAlloc<'h>, frame: &'f Frame) -> Result<SVec<'h, Planned>, CliError> {
        let mut planned = SVec::new_in(alloc);
        self.tools = Some(tools);
        let (Some(names), Some(graph)) = (names.names.as_ref(), discovered.get(Name::unit_graph())) else { return Ok(planned) };
        let Ok(record) = &graph.made else { return Ok(planned) };
        let unit_graph = Name::unit_graph();
        self.mark(unit_graph, Some((tools.key(unit_graph, &graph.source), record.out)));
        let reads_only = |names: &[Name]| -> Vec<Name, &'f Frame> {
            let mut reads = Vec::with_capacity_in(names.len(), frame);
            reads.extend_from_slice(names);
            reads
        };
        // Wave 0: the proc-macro crates' lookups (they read the graph) and the build scripts'
        // (they read nothing: their source holds the module store's listing).
        for part in record.parts.iter() {
            if let (Kind::MacroModule, Locate::Source(source)) = (part.name.kind, part.locate) {
                planned.push(Planned { name: part.name, phase: Phase::new(Stage::Scripts, 0), source, file: part.name });
                self.reads.push((part.name, reads_only(&[unit_graph])));
            }
        }
        let mut last = None;
        for subject in names.script_module_subjects() {
            if last == Some(subject) {
                continue;
            }
            last = Some(subject);
            let name = crate::script_modules::name(subject);
            planned.push(Planned { name, phase: Phase::new(Stage::Scripts, 0), source: crate::script_modules::part_source(graph.source, subject), file: name });
            self.reads.push((name, reads_only(&[])));
        }
        // The runs: each one's unit, side, package and the `Script` runs its `links` closure
        // reads, from the crate list's runs table (names.rs `NamesWorld::script_runs` and its
        // `ListFacts` wrote it from the graph): the runs the world's crates read, never every
        // unit of the graph on both sides, and no graph decoded to plan them.
        for (name, source) in crate::scripts::parts(names.script_keys(), crate::scripts::sdk_identity(env)) {
            let mut reads = reads_only(&[unit_graph]);
            if let Some(run) = names.script_run(name.subject) {
                // Its module's lookup: the one its reading crate names (scripts.rs `job`).
                if let Some(module) = names.script_readers(name).next().and_then(|at| names.script_module_of(at)) {
                    reads.push(crate::script_modules::name(module.subject));
                }
                for producer in run.producers() {
                    let producer = Name::new(Kind::Script, ContentId(producer));
                    #[cfg(test)]
                    if self.runner.and_then(|runner| runner.clock()).is_some_and(|clock| clock.omit_links_edge(producer, name)) {
                        continue;
                    }
                    reads.push(producer);
                }
            }
            // A run the graph does not name is the job's own refusal, said when it is entered.
            reads.sort_unstable();
            reads.dedup();
            self.reads.push((name, reads));
            planned.push(Planned { name, phase: Phase::new(Stage::Scripts, 1), source, file: name });
        }
        planned.sort_unstable_by_key(|one| one.name);
        planned.dedup_by_key(|one| one.name);
        self.reads.sort_unstable_by_key(|(name, _)| *name);
        self.reads.dedup_by_key(|(name, _)| *name);
        let mut native = Vec::new_in(frame);
        native.extend(names.script_keys().filter_map(|(subject, _, placement)|
            (placement == crate::scripts::RanAs::Native).then_some(subject)));
        native.sort_unstable();
        native.dedup();
        for &one in &planned {
            let script = if one.name.kind == Kind::Script {
                names.script_run(one.name.subject).map(|run| (UnitId(run.unit), run.name, run.version, run.side))
            } else {
                None
            };
            let is_native = one.name.kind == Kind::Script && native.binary_search(&one.name.subject).is_ok();
            self.runs.push(RunPlan { task: EngineTask { planned: one, key: tools.key(one.name, &one.source) }, script, native: is_native, inputs: 0, readers: Vec::new_in(frame) });
        }
        // Every run waits for its own lookup and the scripts whose bytes it receives.
        // The unit graph has already been committed by the Units stage.
        for (reader, (_, reads)) in self.reads.iter().enumerate() {
            for producer in reads {
                if *producer == unit_graph {
                    continue;
                }
                let at = self.runs.binary_search_by_key(producer, |run| run.task.planned.name)
                    .map_err(|_| EngineCheckError::Unseen { task: self.runs[reader].task.planned.name, read: *producer })?;
                self.runs[at].readers.push(reader);
                self.runs[reader].inputs += 1;
            }
        }
        let mut ready = Ready::new(&self.runs, frame);
        while let Some(at) = ready.queue.pop() {
            let task = self.runs[at].task.planned;
            planned[at] = task;
            for i in 0..self.runs[at].readers.len() {
                let reader = self.runs[at].readers[i];
                let depth = task.phase.wave.checked_add(1).ok_or(CliError::Text { what: "build script links depth exceeds the engine wave limit (engine_scripts.rs)" })?;
                let wave = &mut self.runs[reader].task.planned.phase.wave;
                *wave = (*wave).max(depth);
            }
            ready.finish(&self.runs[at].readers);
        }
        if let Some(stuck) = ready.left.iter().position(|left| *left != 0) {
            return Err(EngineCheckError::Cycle { krate: self.runs[stuck].task.planned.name }.into());
        }
        Ok(planned)
    }

    /// Restore the producer reads the previous script run observed. A checkout cell
    /// still pending in this request can change the script even if the stored key stands.
    pub(crate) fn restore(&mut self, name: Name, made: Option<(Key, ContentId)>, row: &blaze_engine::Row<'_>, _owed: bool, frame: &'f Frame) {
        self.mark(name, made);
        if name.kind == Kind::Script {
            self.previous.insert(name, Previous { reads: frame.alloc_slice_copy(&row.reads), out: row.out });
        }
    }

    fn output(&self, name: Name) -> Option<Option<(Key, ContentId)>> {
        self.made.binary_search_by_key(&name, |(name, _)| *name).ok().map(|at| self.made[at].1)
    }

    fn drive(&self, task: &EngineTask, heap: blaze_own::Alloc<'static>) -> Result<blaze_drive::Task<'static>, CliError> {
        let name = task.planned.name;
        let at = self.reads.binary_search_by_key(&name, |(name, _)| *name).map_err(|_| EngineCheckError::MissingEntry { task: name, table: Table::ScriptReads, key: name })?;
        let mut inputs = Vec::new_in(heap);
        let mut failed = Vec::new_in(heap);
        for &read in &self.reads[at].1 {
            match self.output(read).ok_or(EngineCheckError::Unseen { task: name, read })? {
                Some((key, out)) => inputs.push(Input { name: read, at: key, out }),
                None => failed.push(read),
            }
        }
        Ok(drive_task(name, task.key, task.planned.source, inputs, failed, heap))
    }

    /// Make the immutable worker input from the producer records the driver filed.
    fn prepare_run<'j>(
        &mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'_, 'f>,
        names: &mut NamesStage<'f>, task: EngineTask, frame: &'f Frame, jobs: &'j Frame,
    ) -> Result<Dispatch<'f, 'j>, CliError>
    where
        'f: 'j,
    {
        let name = task.planned.name;
        let drive = self.drive(&task, handles.env.heap.alloc())?;
        if let Some(&on) = drive.failed.first() {
            return Ok(Dispatch::Answered(Completed::answer(drive, Did::Failed(TaskFailure { name, why: Why::Waited { on } }))?));
        }
        #[cfg(test)]
        if let Some(runner) = self.runner {
            let mut given = Vec::new_in(jobs);
            for input in drive.inputs.iter().filter(|input| input.name.kind == Kind::Script) {
                let payload: &'j [u8] = jobs.alloc(work::payload(cx, store, input, jobs)?).payload();
                given.push((*input, payload));
            }
            return Ok(Dispatch::Run(ScriptWork::Double(runner, name, self.reads_planned.then_some((self.plant, self.status_view)), given.into_bump_slice()), drive));
        }
        // The `Script` kind's tool keys the library-features records the shared setup reads
        // (scripts.rs `CommandFeatures`); the plan handed the tools with the unit graph.
        let tool = self.tools.ok_or(EngineCheckError::Unseen { task: name, read: Name::unit_graph() })?.of(name);
        let made = work::unwinding(|| {
            crate::scripts::job(
                cx, handles.root, store, &mut self.scripts, &mut names.names, handles.env, tool,
                handles.workers.get(), handles.lock, handles.members, &drive, frame, jobs,
            )
        });
        let did = match made {
            Err(work::Panicked) => {
                let mut done = Completed::answer(drive, Did::Failed(refusal(name, "a build script's job panicked while it was made (its words are on the error stream)").failure))?;
                done.owed = true;
                return Ok(Dispatch::Answered(done));
            }
            Ok(made) => match made? {
                Prepared::Ready(mut job) => {
                    if self.reads_planned {
                        job.checkout = crate::scripts::CheckoutReads::Kept { plant: self.plant, status: self.status_view };
                    }
                    return Ok(Dispatch::Run(ScriptWork::Script(job), drive));
                }
                Prepared::Need(wants) => return Err(EngineCheckError::Unseen { task: name, read: wants.first().map_or(name, |want| want.name) }.into()),
                Prepared::Waited(on) => Did::Failed(TaskFailure { name, why: Why::Waited { on } }),
                Prepared::Said { what, words, hole } => {
                    let failure = TaskFailure { name, why: Why::Refused { what, rule: crate::scripts::RULE } };
                    let words = work::held_str(words, frame);
                    if hole { Did::Hole(failure, words) } else { Did::FailedSaying(failure, words) }
                }
            },
        };
        Ok(Dispatch::Answered(Completed::answer(drive, did)?))
    }

    /// Admit every engine-selected task before the coordinator harvests either stage.
    pub(crate) fn submit<'s>(
        &mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'s, 'f>,
        names: &mut NamesStage<'f>, tasks: &[EngineTask], queue: &mut ScriptQueue<'s>, frame: &'f Frame,
    ) -> Result<(), CliError>
    where 'f: 's,
    {
        for task in tasks {
            let name = task.planned.name;
            let run = self.runs.binary_search_by_key(&name, |run| run.task.planned.name).ok().map(|at| &self.runs[at]);
            let leaf = name.kind == Kind::Script && run.is_some_and(|run| run.readers.is_empty());
            if queue.order.insert((leaf, name), *task).is_some() {
                return Err(EngineCheckError::Twice { task: name }.into());
            }
            let lane = if run.is_some_and(|run| run.native) {
                ScriptLane::Native
            } else if name.kind == Kind::MacroModule && names.names.as_ref().is_some_and(|world| world.script_module(name.subject).is_none()) {
                ScriptLane::ProcLookup
            } else {
                ScriptLane::Other
            };
            queue.waiting.insert((lane, leaf, name), *task);
        }
        self.dispatch(cx, store, handles, names, queue, frame)
    }

    /// Human-readable task label and [`blaze_screen::WorkKind`] for the live inline status UI.
    fn status_info(&self, name: Name, job: Option<&ScriptWork<'_>>) -> (blaze_screen::WorkKind, crate::engine_status::Label) {
        use core::fmt::Write as _;
        match name.kind {
            Kind::Given => (blaze_screen::WorkKind::BuildScript, crate::engine_status::Label::of("script inputs")),
            Kind::Script => {
                let run = self.runs.binary_search_by_key(&name, |run| run.task.planned.name).ok().and_then(|at| self.runs.get(at));
                let mut label = crate::engine_status::Label::EMPTY;
                match run.and_then(|run| run.script.map(|(_, package, _, _)| (package, run.native))) {
                    Some((package, true)) => {
                        let _ = write!(label, "{package} · cc/c++");
                    }
                    Some((package, false)) => {
                        let _ = label.write_str(package);
                    }
                    None => {
                        let _ = label.write_str("build.rs");
                    }
                }
                (blaze_screen::WorkKind::BuildScript, label)
            }
            Kind::MacroModule => match job {
                Some(ScriptWork::Module(ModuleJob::Build { wanted, .. })) => {
                    (blaze_screen::WorkKind::BuildScript, crate::engine_status::Label::of(wanted.package))
                }
                _ => {
                    let label = self.macros.as_ref().and_then(|crates| {
                        let at = crates.binary_search_by_key(&name.subject, |(subject, _)| *subject).ok()?;
                        crates.get(at)?.1.as_ref().ok().map(|id| crate::engine_status::Label::of(id.package.as_str()))
                    }).unwrap_or_else(|| crate::engine_status::Label::of("proc-macro"));
                    (blaze_screen::WorkKind::ProcMacro, label)
                }
            },
            _ => (blaze_screen::WorkKind::BuildScript, crate::engine_status::Label::debug(&name)),
        }
    }

    pub(crate) fn dispatch<'s>(
        &mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'s, 'f>,
        names: &mut NamesStage<'f>, queue: &mut ScriptQueue<'s>, frame: &'f Frame,
    ) -> Result<(), CliError>
    where 'f: 's,
    {
        // Limit native roots admitted to the pool independently of the command's jobserver.
        // A worker waiting for its native child occupies a pool slot and takes no FIFO token.
        // Allow up to half the command's width (at least 4, capped by width) so independent
        // workspace build scripts run concurrently in one wave instead of serializing past 3.
        let max_native_roots = queue.width.div_ceil(2).max(4).min(queue.width);
        while queue.slots.free() {
            let next = queue.waiting.keys().find(|(lane, _, _)| match lane {
                ScriptLane::Native => queue.native_running < max_native_roots,
                ScriptLane::ProcLookup => !queue.proc_running,
                ScriptLane::Other => true,
            }).copied();
            let Some((lane, leaf, name)) = next else { break };
            let task = queue.waiting.remove(&(lane, leaf, name)).expect("the selected script was waiting");
            let began = crate::engine_times::ns(handles.env.clock.now().since_start());
            let prepared = match name.kind {
                Kind::Given => {
                    let board = handles.board;
                    let clock = &handles.env.clock;
                    let (kind, label) = self.status_info(name, None);
                    board.show_kind(Stage::Scripts, kind, label, 0, board.now(clock)).end(board.now(clock));
                    queue.answered.insert(name, (task, began, began));
                    continue;
                }
                Kind::MacroModule => self.prepare_lookup(cx, store, handles, names, task, queue)?,
                _ => self.prepare_run(cx, store, handles, names, task, frame, queue.jobs)?,
            };
            match prepared {
                Dispatch::Answered(done) => {
                    let board = handles.board;
                    let clock = &handles.env.clock;
                    let (kind, label) = self.status_info(name, None);
                    board.show_kind(Stage::Scripts, kind, label, 0, board.now(clock)).end(board.now(clock));
                    self.completed.insert(name, done);
                    queue.answered.insert(name, (task, began, crate::engine_times::ns(handles.env.clock.now().since_start())));
                }
                Dispatch::Run(job, drive) => {
                    let (work_kind, label) = self.status_info(name, Some(&job));
                    queue.slots.take();
                    let finished: &'s bstd::sync::Flag = queue.jobs.alloc(bstd::sync::Flag::new());
                    let notice = Notice { finished, waiter: queue.waiter };
                    let output: &'s blaze_core::lock::Lock<Option<(WorkerOutput, u64, u64)>> =
                        queue.jobs.alloc(blaze_core::lock::Lock::new("scripts.completed-worker", None));
                    let clock = &handles.env.clock;
                    let board = handles.board;
                    let heap = handles.env.heap;
                    #[cfg(test)]
                    let root = handles.root;
                    #[cfg(test)]
                    if let Some(sim) = runner_clock() {
                        let now = self.tick.max(sim.now());
                        let due = if name.kind == Kind::Script { sim.start(name, now) } else { now };
                        queue.calendar.insert((due, name), ());
                    }
                    let scope: &'s bstd::task::Scope<'s> = queue.scope;
                    let handle = scope.may_overlap("build script", crate::scripts::SCRIPT_FRAME, move |cx| {
                        let _notice = notice;
                        let began = crate::engine_times::ns(clock.now().since_start());
                        let shown = board.show_kind(Stage::Scripts, work_kind, label, 0, board.now(clock));
                        let result = match job {
                            ScriptWork::Module(job) => match work::unwinding(|| job.run(cx, task, heap)) {
                                Ok(result) => result?,
                                Err(work::Panicked) => {
                                    let why = Why::Panicked { used: cx.frame().committed() as u64, limit: crate::scripts::SCRIPT_FRAME as u64 };
                                    WorkerOutput::Module(ModuleAnswer::Failed(TaskFailure { name, why }, None, false), None)
                                }
                            },
                            ScriptWork::Script(job) => WorkerOutput::Run(crate::scripts::pooled(cx, &job, clock)),
                            #[cfg(test)]
                            ScriptWork::Double(runner, name, plant, producers) => WorkerOutput::Run(runner.run(cx, root, name, plant, producers)),
                        };
                        let end = crate::engine_times::ns(clock.now().since_start());
                        shown.end(board.now(clock));
                        *output.lock() = Some((result, began, end));
                        Ok::<_, CliError>(())
                    });
                    queue.native_running += usize::from(lane == ScriptLane::Native);
                    if lane == ScriptLane::ProcLookup { queue.proc_running = true; }
                    queue.running.insert(name, Running { task, drive, finished, output, handle, lane });
                }
            }
        }
        Ok(())
    }

    /// Observe a worker and fill its freed slot. This never publishes records. Canonical
    /// filing can therefore wait for one name while later ready jobs already run.
    pub(crate) fn observe_next<'s>(
        &mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'s, 'f>,
        names: &mut NamesStage<'f>, queue: &mut ScriptQueue<'s>, frame: &'f Frame,
    ) -> Result<(), CliError>
    where 'f: 's,
    {
        let next = || -> Result<Name, CliError> {
            if queue.running.is_empty() {
                return Err(CliError::Text { what: "the Scripts completion queue has no running worker (engine_scripts.rs)" });
            }
            wait_finished(cx, &handles.env.clock, queue.waiter,
                || queue.running.values().any(|run| run.finished.is_set()),
                "a ready build script to finish", queue.running.len())?;
            Ok(*queue.running.iter().find(|(_, run)| run.finished.is_set()).expect("a signalled script remains ready").0)
        };
        #[cfg(test)]
        let name = match runner_clock() {
            Some(clock) => {
                let ((end, name), ()) = queue.calendar.pop_first().ok_or(CliError::Text { what: "no simulated script is running (engine_scripts.rs)" })?;
                self.tick = end;
                if name.kind == Kind::Script { clock.end(name, end); }
                name
            }
            None => next()?,
        };
        #[cfg(not(test))]
        let name = next()?;
        let finished = queue.running.get(&name).ok_or(EngineCheckError::MissingEntry { task: name, table: Table::ScriptsRunning, key: name })?.finished;
        // A simulated completion still waits for the real worker to hand over its bytes.
        wait_finished(cx, queue.clock, queue.waiter, || finished.is_set(), "the selected Scripts worker to return", 1)?;
        let Running { task, drive, handle, output, lane, .. } = queue.running.remove(&name)
            .ok_or(EngineCheckError::MissingEntry { task: name, table: Table::ScriptsRunning, key: name })?;
        queue.slots.release();
        queue.joined_later.push(handle);
        let (answer, start, end) = output.lock().take().ok_or(EngineCheckError::MissingEntry { task: name, table: Table::ScriptsOutput, key: name })?;
        let done = match answer {
            WorkerOutput::Run(pooled) => Completed::pooled(drive, pooled, frame)?,
            WorkerOutput::Module(answer, crates) => {
                if lane == ScriptLane::ProcLookup { self.macros = crates; }
                let did = match answer {
                    ModuleAnswer::Found(out) => {
                        let mut made = work::Made::new_in(name, frame);
                        made.out = out;
                        Did::Done(made)
                    }
                    ModuleAnswer::Failed(failed, Some(words), hole) => {
                        let words = work::held_str(&words, frame);
                        if hole { Did::Hole(failed, words) } else { Did::FailedSaying(failed, words) }
                    }
                    ModuleAnswer::Failed(failed, None, _) => Did::Failed(failed),
                };
                Completed::answer(drive, did)?
            }
        };
        self.completed.insert(name, done);
        queue.answered.insert(name, (task, start, end));
        queue.native_running -= usize::from(lane == ScriptLane::Native);
        if lane == ScriptLane::ProcLookup { queue.proc_running = false; }
        self.dispatch(cx, store, handles, names, queue, frame)
    }

    /// File the logical frontier's chosen task, independent of worker delivery order.
    pub(crate) fn finish<'s, 'a>(
        &mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'s, 'f>,
        names: &mut NamesStage<'f>, queue: &mut ScriptQueue<'s>, name: Name,
        buffer: &mut Buffer<'a>, times: &mut crate::engine_times::Times, alloc: SharedAlloc<'a>, frame: &'f Frame,
    ) -> Result<EngineTask, CliError>
    where 'f: 's,
    {
        while !queue.answered.contains_key(&name) {
            self.observe_next(cx, store, handles, names, queue, frame)?;
        }
        let (task, start, end) = queue.answered.remove(&name).expect("the chosen script has answered");
        times.interval(name, start, end);
        if queue.order.remove(&(false, name)).is_none() && queue.order.remove(&(true, name)).is_none() {
            return Err(EngineCheckError::MissingEntry { task: name, table: Table::ScriptsOrder, key: name }.into());
        }
        if name.kind == Kind::Given {
            let at = self.given.binary_search_by_key(&name, |(name, _, _)| *name)
                .map_err(|_| EngineCheckError::MissingEntry { task: name, table: Table::ScriptsGiven, key: name })?;
            buffer.done.push(crate::engine_script_reads::given_done(&task, self.given[at].1, alloc));
        } else {
            let task = &task;
            let done = self.completed.remove(&name).ok_or(EngineCheckError::MissingEntry { task: name, table: Table::ScriptsCompleted, key: name })?;
            let Completed { drive, made, owed, worker } = done;
            let used_unkept = worker.as_ref().is_some_and(|run| run.unkept.is_some());
            if let Some(run) = worker {
                if run.produced {
                    if let Some(why) = run.unkept {
                        let (unit, package, version, side) = self.runs.binary_search_by_key(&name, |run| run.task.planned.name)
                            .ok().and_then(|at| self.runs[at].script)
                            .ok_or(EngineCheckError::MissingEntry { task: name, table: Table::ScriptRuns, key: name })?;
                        self.unkept.push((name, unit, package, version, side, why));
                    } else {
                        self.fresh.extend(run.checkout.into_iter().map(|read| (name, read)));
                        self.kept_runs += u32::from(self.reads_planned);
                    }
                }
                if run.hole && let Err(Failure { words: Some(said), .. }) = &made {
                    self.holes.0 += 1;
                    self.holes.1.get_or_insert_with(|| std::string::String::from(*said));
                }
            }
            let done = match &made {
                Ok(record) => {
                    let mut reads: SVec<'a, (CellId, ContentId)> = SVec::new_in(alloc);
                    reads.extend(record.reads.iter().map(|(read, digest)| (CellId { of: *read, cell: WHOLE }, *digest)));
                    reads.extend(drive.inputs.iter().map(|input| (CellId { of: input.name, cell: WHOLE }, input.out)));
                    reads.sort_unstable_by_key(|(id, _)| *id);
                    reads.dedup_by_key(|(id, _)| *id);
                    let mut defines = SVec::new_in(alloc);
                    defines.push((WHOLE, record.out));
                    self.mark(name, Some((task.key, record.out)));
                    if name.kind == Kind::Script {
                        names.mark(name, Some((task.key, record.out)));
                        // Entered again with a stored row: counted, and when it made its stored
                        // bytes again, named with the cell that woke it (the profile's line).
                        let again = self.previous.get(&name).map(|previous| (previous.out == record.out).then(|| self.woke_by(previous)));
                        if let Some(same) = again {
                            self.reran = self.reran.saturating_add(1);
                            if let Some(woke) = same {
                                self.identical.push(IdenticalRun { script: name, woke });
                            }
                        }
                        // Each read no task of today's plan defines (new this request) is made by
                        // its reader, at wave 0, keyed as next request's planned task.
                        let tools = self.tools.ok_or(EngineCheckError::Unseen { task: name, read: Name::unit_graph() })?;
                        for (_, read) in self.fresh.iter().filter(|(by, _)| *by == name) {
                            match self.given.binary_search_by_key(&read.name(), |(cell, _, _)| *cell) {
                                Ok(at) if self.given[at].1 != read.answer => {
                                    // Named by its package (rule 19); the task name follows for
                                    // the profile's rows.
                                    let script = self.runs.binary_search_by_key(&name, |run| run.task.planned.name).ok().and_then(|at| self.runs[at].script);
                                    let subject = match script {
                                        Some((_, package, version, side)) => bstd::mem::format_in!(
                                            frame,
                                            "the build script of {package} {version} ({side:?} side, {name}) read {:?} {:?}, which changed while it ran",
                                            read.kind,
                                            read.path
                                        ),
                                        None => bstd::mem::format_in!(frame, "script {name} {:?} {:?}", read.kind, read.path),
                                    };
                                    return Err(CliError::Changed { paths: subject.as_str().to_owned() });
                                }
                                Ok(_) => {}
                                Err(_) => buffer.made.push(crate::engine_script_reads::made(read, name, &tools, alloc)),
                            }
                        }
                    }
                    // Right for this command only: its row is kept and owed (blaze-engine
                    // `Buffer::owed`), so the next request enters it again whatever moved and no
                    // certificate stands on it (the answer's owed count). Its record is put this
                    // command under its bytes' key (engine_store.rs `record_key`), which a reader
                    // finds only by this row's out: no later command reads it but through a
                    // rerun that made the same bytes.
                    if used_unkept {
                        buffer.owed.push(name);
                    }
                    Done { name, key: task.key, out: record.out, defines, reads, findings: u32::try_from(record.findings.len()).unwrap_or(u32::MAX), failed: false }
                }
                Err(_) => {
                    self.mark(name, None);
                    if name.kind == Kind::Script {
                        names.mark(name, None);
                    }
                    if owed {
                        buffer.owed.push(name);
                        crate::engine_tasks::failed(task, alloc)
                    } else {
                        // What its failure saw (plan row A8): every record its plan handed it,
                        // with the out it had, and each of its plan's reads that failed this
                        // command, read ABSENT (a failed task defines no cell), so that task's
                        // fix moves this row. Its script's sources, its placement and the SDK
                        // are in its key (scripts.rs `parts`: the subject, its crate's world key,
                        // the run's mode); its environment is the world row's cfg, the graph's
                        // record, which every run reads; a module the store lacks is in its
                        // lookup's key (the store's listing) and read here as that lookup's cell.
                        let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(drive.inputs.len() + drive.failed.len(), alloc);
                        reads.extend(drive.inputs.iter().map(|input| (CellId { of: input.name, cell: WHOLE }, input.out)));
                        reads.extend(drive.failed.iter().map(|read| (CellId { of: *read, cell: WHOLE }, blaze_drive::key::ABSENT)));
                        crate::engine_tasks::failed_with(task, reads, alloc)
                    }
                }
            };
            buffer.done.push(done);
            self.results.push(Published { name, source: task.planned.source, wave: task.planned.phase.wave, made });
        }
        Ok(task)
    }

    /// Freeze all borrowed inputs before giving the lookup to a worker. Build-script
    /// module metadata already belongs to the plan. Proc-macro lookups share a single
    /// identity table; the first worker builds it using the same lookup as the legacy path.
    fn prepare_lookup<'s>(
        &mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'s, 'f>,
        names: &NamesStage<'f>, task: EngineTask, queue: &mut ScriptQueue<'s>,
    ) -> Result<Dispatch<'f, 's>, CliError>
    where 'f: 's,
    {
        let drive = self.drive(&task, handles.env.heap.alloc())?;
        let name = drive.name;
        let world = names.names.as_ref().ok_or(CliError::Text { what: "the names world, made by the planner, is gone (engine_scripts.rs `prepare_lookup`)" })?;
        if world.script_module(name.subject).is_some() {
            #[cfg(test)]
            if self.runner.is_some() { return Ok(Dispatch::Run(ScriptWork::Module(ModuleJob::Fixture), drive)); }
            // The lookup's package, version and features are the world's: made now if no task
            // made it yet (names.rs `made_or_make`), a world that cannot be made a typed stop.
            let made = match world.made_or_make(cx, handles.root, store, handles.env, crate::names::NeededBy::ModuleLookup)? {
                Ok(made) => made,
                Err(why) => return Err(EngineCheckError::NamesWorld(why).into()),
            };
            let module = made.script_module(name.subject).ok_or(EngineCheckError::MissingEntry { task: name, table: Table::WorldScriptModules, key: name })?;
            let wanted = crate::script_modules::Wanted {
                ran_as: module.ran_as, package: module.package, version: module.version,
                features: queue.jobs.alloc_slice_copy(&module.features),
            };
            return Ok(Dispatch::Run(ScriptWork::Module(ModuleJob::Build { wanted, home: handles.env.home }), drive));
        }
        let Some(side) = handles.side else {
            let failure = TaskFailure { name, why: Why::Refused { what: "a proc-macro module's lookup with no lock read this command", rule: RULE } };
            return Ok(Dispatch::Answered(Completed::answer(drive, Did::FailedSaying(failure, "a proc-macro crate's module is looked up by its lock entry, and this command read no lock"))?));
        };
        let graph = drive.inputs.iter().find(|input| input.name == Name::unit_graph()).copied();
        let initial = if self.macros.is_none() && side.place.home.is_some() {
            if queue.macro_inputs.is_none() {
                let graph = graph.ok_or(EngineCheckError::Unseen { task: name, read: Name::unit_graph() })?;
                let mut bytes = bstd::mem::Bytes::new_in(queue.jobs, "graph record for a proc-macro module worker");
                if !store.get_into(cx, graph.at, &mut bytes)? {
                    return Err(CliError::Input { name: graph.name, why: work::InputWhy::Absent });
                }
                let mut members = Vec::new_in(queue.jobs);
                for leaf in side.place.rows.of(crate::sweep::LeafKind::MacroFiles).filter(|leaf| !leaf.absent()) {
                    let path = side.place.rows.path(leaf);
                    if let Some(entry) = side.place.members.and_then(|all| all.iter().find(|entry| entry.dir.as_str() == path)) {
                        members.push((entry.id, work::held_str(path, queue.jobs), entry.kind, leaf.id));
                    }
                }
                queue.macro_inputs = Some(MacroInputs { graph: queue.jobs.alloc(bytes).as_slice(), lock: side.lock.text, members: members.into_bump_slice() });
            }
            queue.macro_inputs
        } else {
            None
        };
        let lookup = ModuleJob::Proc { crates: self.macros.take(), home: side.place.home, checkout: side.place.checkout, graph, initial };
        Ok(Dispatch::Run(ScriptWork::Module(lookup), drive))
    }

    pub(crate) fn mark(&mut self, name: Name, made: Option<(Key, ContentId)>) {
        match self.made.binary_search_by_key(&name, |(name, _)| *name) {
            Ok(at) => self.made[at].1 = made,
            Err(at) => self.made.insert(at, (name, made)),
        }
    }
}

/// Whether a lookup's or a run's result is a function of its key and what it was handed, so a
/// failure stands on its reads (work.rs `filed`, the old path's rule): not a hole (a fact outside
/// the key and the reads, `Did::Hole`), a panic (a bug or a frame too small), or a file that
/// changed while the command ran.
fn stands_on_reads(did: &Did<'_>) -> bool {
    match did {
        Did::Done(_) | Did::FailedSaying(..) => true,
        Did::Failed(failure) => match failure.why {
            Why::Panicked { .. } | Why::Changed => false,
            Why::Waited { .. } | Why::Refused { .. } | Why::NotBuilt => true,
        },
        Did::Hole(..) | Did::Need(_) => false,
    }
}

/// A task refused on this path, naming why.
fn refusal(name: Name, what: &'static str) -> Failure<'static> {
    Failure { failure: TaskFailure { name, why: Why::Refused { what, rule: RULE } }, words: None }
}

/// The coordinator owns the mutable engine and store. Both kinds of jobs borrow
/// frozen inputs and return their answers through the same foreground pool.
pub(crate) struct Pipeline<'s, 'a> {
    scripts: ScriptQueue<'s>,
    names: crate::engine_names::NamesQueue<'s, 'a>,
    script_reads: ScriptReady<'s>,
    name_reads: crate::engine_names::ProducerWalk<'s>,
}

impl<'s, 'a: 's> Pipeline<'s, 'a> {
    pub(crate) fn new(scope: &'s bstd::task::Scope<'s>, jobs: &'s Frame, local: blaze_own::Alloc<'s>, handles: &Handles<'s, '_>) -> Self {
        let scripts = ScriptQueue::new(scope, jobs, handles);
        let names = crate::engine_names::NamesQueue::new(jobs, local, scripts.slots());
        Self {
            scripts,
            names,
            script_reads: ScriptReady::new(jobs),
            name_reads: crate::engine_names::ProducerWalk::new(jobs),
        }
    }

    /// A publication can expose any of its readers. Submit every newly ready
    /// batch before choosing a result to file. An unsettled candidate stays in
    /// the engine and consumes neither a pool slot nor a job frame.
    pub(crate) fn admit<'r: 's, 'f: 's>(
        &mut self, cx: &Cx, engine: &mut blaze_engine::Engine<'a, blaze_engine::Everything>,
        scripts: &mut ScriptsStage<'f>, names: &mut NamesStage<'f>, store: &mut dyn crate::store::Store<'r>,
        read: &mut crate::engine_records::EngineRecords<'r>, handles: &Handles<'s, 'f>, alloc: SharedAlloc<'a>, frame: &'f Frame,
    ) -> Result<(), CliError> {
        self.script_reads.clear();
        while engine.next_phase_in(Stage::Scripts, &mut |name, pending| scripts.producers_ready(name, pending, &mut self.script_reads))?.is_some() {
            let mark = handles.board.phase_began(Stage::Scripts, engine.tasks().len() as u64, handles.board.now(&handles.env.clock));
            scripts.submit(cx, store, handles, names, engine.tasks(), &mut self.scripts, frame)?;
            handles.board.phase_ended(mark, 0, handles.board.now(&handles.env.clock));
            self.script_reads.clear();
        }
        // When only `Given` cells are queued in `Stage::Scripts` (no `Script` or `MacroModule`
        // task queued), finish and merge that instant batch before admitting `Stage::Names`
        // (which opens dependency crate records via `EngineRecords`). That is worker-count
        // invariant, keeps `Scripts` at 0 crate records read when no script runs, and lets
        // `Names` run at full unthrottled width once it is known whether any `Script` woke up.
        if self.scripts.only_given() {
            return Ok(());
        }
        #[cfg(test)]
        if runner_clock().is_some_and(|clock| clock.global_names_barrier()) && (!self.scripts.is_empty() || engine.has_pending_stage(Stage::Scripts)) {
            return Ok(());
        }
        let scripts_pending = !self.scripts.is_empty() || engine.has_pending_stage(Stage::Scripts);
        self.names.set_width(self.scripts.scope, handles, scripts_pending);
        loop {
            self.name_reads.clear();
            self.script_reads.clear();
            let phase = engine.next_phase_in(Stage::Names, &mut |name, pending| {
                names.producers_ready(name, &mut |producer| {
                    #[cfg(test)]
                    if producer.kind == Kind::Script && runner_clock().is_some_and(|clock| clock.drop_names_producer()) {
                        return false;
                    }
                    pending(producer) || (producer.kind == Kind::Script && !scripts.producers_ready(producer, pending, &mut self.script_reads))
                }, &mut self.name_reads)
            })?;
            if phase.is_none() { break; }
            let mark = handles.board.phase_began(Stage::Names, engine.tasks().len() as u64, handles.board.now(&handles.env.clock));
            let view = engine.view();
            for &task in engine.tasks() {
                self.names.submit(self.scripts.scope, cx, names, store, read, view, handles, task, alloc, frame, self.scripts.waiter(), scripts_pending)?;
            }
            handles.board.phase_ended(mark, 0, handles.board.now(&handles.env.clock));
        }
        Ok(())
    }

    /// Release links readers first, then downstream crate scopes. Once the
    /// remaining Names jobs are leaves they can finish beside the script being
    /// filed. Ties in both queues are by task name, including unstarted jobs.
    pub(crate) fn first(&self, names: &NamesStage<'_>) -> Option<EngineTask> {
        let script = self.scripts.first();
        let scope = self.names.first();
        if self.scripts.urgent() || !scope.is_some_and(|task| names.feeds_names(task.planned.name)) {
            script.or(scope)
        } else {
            scope
        }
    }

    pub(crate) fn collect<'f: 's>(
        &mut self, cx: &Cx, scripts: &mut ScriptsStage<'f>, names: &mut NamesStage<'f>,
        store: &mut dyn crate::store::Store<'_>, handles: &Handles<'s, 'f>, first: EngineTask,
        entered: &mut SVec<'_, EngineTask>, buffer: &mut Buffer<'a>, times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>, frame: &'f Frame,
    ) -> Result<(), CliError> {
        let mut task = first;
        loop {
            scripts.dispatch(cx, store, handles, names, &mut self.scripts, frame)?;
            self.names.dispatch();
            let name = task.planned.name;
            let script = task.planned.phase.stage == Stage::Scripts;
            while !(if script { self.scripts.is_finished(name) } else { self.names.is_finished(name) }) {
                let observed_name = || self.names.has_completion();
                #[cfg(test)]
                let virtual_order = match (self.names.next_tick(), self.scripts.next_tick()) {
                    (Some(n), Some(s)) => Some(n < s),
                    (Some(_), None) => Some(true),
                    (None, Some(_)) if !self.names.has_completion() => Some(false),
                    _ => None,
                };
                #[cfg(not(test))]
                let virtual_order: Option<bool> = None;
                let take_name = if let Some(take_name) = virtual_order { take_name } else {
                    wait_finished(cx, &handles.env.clock, self.scripts.waiter(), || observed_name() || self.scripts.has_completion(),
                        "a Names or Scripts worker to finish", self.scripts.running.len() + usize::from(!self.names.is_empty()))?;
                    observed_name()
                };
                if take_name {
                    self.names.observe_next(cx)?;
                } else {
                    scripts.observe_next(cx, store, handles, names, &mut self.scripts, frame)?;
                }
                scripts.dispatch(cx, store, handles, names, &mut self.scripts, frame)?;
                self.names.dispatch();
            }
            if script {
                scripts.finish(cx, store, handles, names, &mut self.scripts, name, buffer, times, alloc, frame)?;
            } else {
                self.names.finish(cx, name, names, buffer, times, frame)?;
            }
            entered.push(task);
            // A script may immediately release a reader, including one in this
            // same Names wave. Non-leaf Names tasks break before leaf tasks so
            // downstream waves can be admitted without waiting on leaves.
            if name.kind == Kind::Script { break; }
            let Some(next) = self.first(names) else { break };
            if next.planned.phase != first.planned.phase || (!script && names.feeds_names(name) && !names.feeds_names(next.planned.name)) { break; }
            task = next;
        }
        Ok(())
    }

    pub(crate) fn drain(&mut self, cx: &Cx) -> Result<(), CliError> {
        let stopped = self.names.wait_running(cx).and(self.scripts.wait_running(cx));
        let joined = self.names.drain(cx).and(self.scripts.drain(cx));
        joined.and(stopped)
    }
}
