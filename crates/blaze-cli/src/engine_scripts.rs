//! The `Scripts` stage of `blaze engine-check` (engine.md E6, piece "scripts"): every build
//! script's run and every module lookup as an engine task, entered once, before any crate's names.
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
//! - **Run**: each task wraps today's function, unchanged: a lookup scripts.rs's sibling
//!   (`script_modules::run`, `macros::run`) on the coordinator's thread; a run's job made there
//!   (`scripts::job`, which opens the command's scripts once: the runtime, the sandbox's
//!   setting and the native runner's jobserver, rule 9). Workers start each ready run as soon
//!   as its inputs are available. The engine still commits records and diagnostics by wave.
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
use bstd::task::Class;

use crate::CliError;
use crate::engine_check::EngineCheckError;
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
    key: Key,
    owed: bool,
    reads: &'f [(CellId, ContentId)],
}

struct Completed<'f> {
    drive: blaze_drive::Task<'static>,
    made: Result<work::Made<'f>, Failure<'f>>,
    owed: bool,
    worker: Option<WorkerResult>,
}

struct WorkerResult {
    ns: u64,
    checkout: std::vec::Vec<crate::engine_script_reads::CheckoutRead>,
    unkept: Option<crate::engine_script_reads::NoCell>,
    produced: bool,
    hole: bool,
}

enum ScriptWork<'f> {
    Script(crate::scripts::ScriptJob<'f>),
    #[cfg(test)]
    Double(&'static dyn ScriptRunner, Name, Option<crate::engine_script_reads::ReadsPlant>, &'f [(Input, &'f [u8])]),
}

enum Dispatch<'f, 'j> {
    Run(ScriptWork<'j>, blaze_drive::Task<'static>),
    Answered(Completed<'f>),
}

/// Also signals when the pool rejects a job before calling its closure. Joining the handle
/// then gives the original task error. Each flag is set once. The park token covers a
/// completion racing the coordinator's scan, including when several wakes coalesce.
struct Notice<'a> {
    finished: &'a bstd::sync::Flag,
    waiter: &'a blaze_core::wait::Parker,
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
        answer.worker = Some(WorkerResult { ns: pooled.ns, checkout: pooled.checkout, unkept: pooled.unkept, produced, hole });
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
    /// Work finished ahead of the engine's publication wave. A reader can use its record now.
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
    fresh: std::vec::Vec<(Name, crate::engine_script_reads::CheckoutRead)>,
    /// The kinds' tools, from [`ScriptsStage::plan`]: a `Given` record's key.
    tools: Option<Tools>,
    /// The bug a test plants in the runs' read lists (engine_script_reads.rs `ReadsPlant`).
    pub(crate) plant: crate::engine_script_reads::ReadsPlant,
    /// The request planned the reads' `Given` cells ([`ScriptsStage::plan_reads`]): only then are
    /// a member's reads kept as cells.
    reads_planned: bool,
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
    unkept: Vec<(Name, crate::engine_script_reads::NoCell), &'f Frame>,
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
pub(crate) trait ScriptRunner: Sync {
    /// The run of `name` over the checkout `root`; `kept`: its reads are cells, with the plant
    /// (`None`: they are a hole, as on the old path).
    fn run(&self, cx: &Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, name: Name, kept: Option<crate::engine_script_reads::ReadsPlant>, producers: &[(Input, &[u8])]) -> crate::scripts::PooledScript;

    fn clock(&self) -> Option<&dyn ScriptClock> { None }
}

/// A simulated worker holds its slot and result until its finish tick. Admission, producer
/// handoff and publication use the real driver. Ties are broken by the run's plan index.
#[cfg(test)]
pub(crate) trait ScriptClock {
    fn start(&self, name: Name, tick: u64) -> u64;
    fn end(&self, name: Name, tick: u64);
    fn omit_links_edge(&self, _producer: Name, _reader: Name) -> bool { false }
}

#[cfg(test)]
std::thread_local! {
    static RUNNER: core::cell::Cell<Option<&'static dyn ScriptRunner>> = const { core::cell::Cell::new(None) };
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
            fresh: std::vec::Vec::new(),
            tools: None,
            plant: crate::engine_script_reads::ReadsPlant::None,
            reads_planned: false,
            #[cfg(test)]
            runner: RUNNER.with(|cell| cell.get()),
            #[cfg(test)]
            tick: 0,
            kept_runs: 0,
            holes: (0, None),
            unkept: Vec::new_in(frame),
        }
    }

    /// Each run this request whose outputs it used and did not keep, with why: one note each
    /// (engine_check.rs prints them, [`UnkeptWords`]).
    pub(crate) fn unkept(&self) -> impl Iterator<Item = (Name, &crate::engine_script_reads::NoCell)> + '_ {
        self.unkept.iter().map(|(name, why)| (*name, why))
    }

    /// The profile's line of the scripts' reads of the checkout: the cells this request planned
    /// or made, by kind; the runs kept; the holes, and the first one's words.
    pub(crate) fn profile(&self, text: &mut blaze_own::String<&Frame>) {
        use core::fmt::Write as _;
        use crate::engine_script_reads::ReadKind;
        let mut cells: std::collections::BTreeMap<Name, ReadKind> = std::collections::BTreeMap::new();
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

    /// Onto `out`, the leaf rows of the build scripts' reads the answer still reads: `reads(name)`
    /// says whether the final answer has a row for the cell `name` (engine_check.rs hook H2,
    /// before the answer's rows are saved).
    pub(crate) fn script_rows(&self, followed: Option<&crate::sweep::Rows<'_>>, reads: impl Fn(Name) -> bool, out: &mut crate::sweep::Rows<'_>) {
        crate::engine_script_reads::keep_rows(followed, self.fresh.iter().map(|(_, read)| read), reads, out);
    }

    /// The result of `name`, if this stage entered it.
    pub(crate) fn result(&self, name: Name) -> Option<&Published<'f>> {
        self.results.iter().find(|one| one.name == name)
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
        for module in names.script_modules() {
            if last == Some(module.subject) {
                continue;
            }
            last = Some(module.subject);
            let name = crate::script_modules::name(module.subject);
            planned.push(Planned { name, phase: Phase::new(Stage::Scripts, 0), source: crate::script_modules::part_source(graph.source, module.subject), file: name });
            self.reads.push((name, reads_only(&[])));
        }
        // The runs: each one's unit and side by its subject, as scripts.rs `Scripts::open` finds it.
        let units = names.graph();
        let heap = env.heap.alloc();
        let mut runs: Vec<(ContentId, UnitId, Side), &Frame> = Vec::new_in(frame);
        for at in 0..units.units.len() {
            let script = UnitId(u32::try_from(at).map_err(|_| CliError::Text { what: "a unit graph past u32 units (engine_scripts.rs `plan`)" })?);
            for side in [Side::Target, Side::Macro] {
                if let Some(subject) = crate::scripts::subject(units, script, side, heap.clone()) {
                    runs.push((subject, script, side));
                }
            }
        }
        runs.sort_unstable_by_key(|(subject, script, _)| (*subject, *script));
        runs.dedup_by_key(|(subject, _, _)| *subject);
        for (name, source) in crate::scripts::parts(names.script_keys(), crate::scripts::sdk_identity(env)) {
            let mut reads = reads_only(&[unit_graph]);
            if let Ok(at) = runs.binary_search_by_key(&name.subject, |(subject, _, _)| *subject) {
                let (_, script, side) = runs[at];
                // Its module's lookup: the one its reading crate names (scripts.rs `job`).
                if let Some(module) = names.script_readers(name).next().and_then(|at| names.script_module_of(at)) {
                    reads.push(crate::script_modules::name(module.subject));
                }
                for (input, _, _) in blaze_proc_macro::script_unit::links_closure(units, script).iter() {
                    if let Some(producer) = crate::scripts::name_of(units, *input, side, heap.clone()) {
                        #[cfg(test)]
                        if self.runner.and_then(|runner| runner.clock()).is_some_and(|clock| clock.omit_links_edge(producer, name)) {
                            continue;
                        }
                        reads.push(producer);
                    }
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
        for &one in &planned {
            self.runs.push(RunPlan { task: EngineTask { planned: one, key: tools.key(one.name, &one.source) }, inputs: 0, readers: Vec::new_in(frame) });
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

    /// The old script rows are the evidence for running a later wave early on an incremental
    /// request. The engine still decides which results to publish at each wave.
    pub(crate) fn restore(&mut self, name: Name, made: Option<(Key, ContentId)>, row: &blaze_engine::Row<'_>, owed: bool, frame: &'f Frame) {
        self.mark(name, made);
        if name.kind == Kind::Script {
            self.previous.insert(name, Previous { key: row.key, owed, reads: Vec::from_slice_in(&row.reads, frame).leak() });
        }
    }

    fn output(&self, name: Name) -> Option<Option<(Key, ContentId)>> {
        if let Some(done) = self.completed.get(&name) {
            return Some(done.made.as_ref().ok().map(|record| (done.drive.key, record.out)));
        }
        self.made.binary_search_by_key(&name, |(name, _)| *name).ok().map(|at| self.made[at].1)
    }

    fn drive(&self, task: &EngineTask, heap: blaze_own::Alloc<'static>) -> Result<blaze_drive::Task<'static>, CliError> {
        let name = task.planned.name;
        let at = self.reads.binary_search_by_key(&name, |(name, _)| *name).map_err(|_| EngineCheckError::Unseen { task: name, read: name })?;
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

    /// At this point every producer has finished or stands. The remaining reads are wave 0
    /// checkout cells, module lookups and the unit graph. None of these comes from a later wave.
    fn needs_run(&self, task: EngineTask) -> Option<bool> {
        let name = task.planned.name;
        let Some(previous) = self.previous.get(&name) else {
            // No stored row is a new entry. A restored mark without its read list can only
            // be decided when the engine opens its wave.
            return self.output(name).is_none().then_some(true);
        };
        if previous.key != task.key || previous.owed {
            return Some(true);
        }
        let planned = self.reads.binary_search_by_key(&name, |(name, _)| *name).ok()?;
        for &(cell, seen) in previous.reads {
            if cell.cell != WHOLE {
                return None;
            }
            let now = match cell.of.kind {
                // A cell outside today's plan might be defined inline by an earlier run.
                Kind::Given => {
                    let at = self.given.binary_search_by_key(&cell.of, |(name, _, _)| *name).ok()?;
                    self.given[at].1
                }
                // Only these producers were settled before this task was released.
                _ if self.reads[planned].1.binary_search(&cell.of).is_ok() => self.output(cell.of)?.map_or(blaze_drive::key::ABSENT, |(_, out)| out),
                _ => return None,
            };
            if now != seen {
                return Some(true);
            }
        }
        Some(false)
    }

    /// Prepare one job on the coordinator, then immediately give it to a worker. A freshly
    /// completed producer's record is available here even before the engine puts its wave.
    fn prepare_run<'j>(
        &mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'_, 'f>,
        names: &mut NamesStage<'f>, task: EngineTask, frame: &'f Frame, jobs: &'j Frame,
        payloads: &mut BTreeMap<Name, &'j [u8], &'j Frame>,
    ) -> Result<Dispatch<'f, 'j>, CliError>
    where
        'f: 'j,
    {
        let name = task.planned.name;
        let drive = self.drive(&task, handles.env.heap.alloc())?;
        if let Some(&on) = drive.failed.first() {
            return Ok(Dispatch::Answered(Completed::answer(drive, Did::Failed(TaskFailure { name, why: Why::Waited { on } }))?));
        }
        let completed = &self.completed;
        // Borrowing the map of completions would prevent inserting the next result until all
        // its readers join. Share one stable slice per producer across these jobs instead.
        let mut bytes = |name| -> Result<Option<&'j [u8]>, CliError> {
            if let Some(&payload) = payloads.get(&name) {
                return Ok(Some(payload));
            }
            let Some(done) = completed.get(&name) else { return Ok(None) };
            let record = done.made.as_ref().map_err(|_| EngineCheckError::Unseen { task: drive.name, read: name })?;
            let payload: &'j [u8] = Vec::from_slice_in(record.payload_bytes()?, jobs).leak();
            payloads.insert(name, payload);
            Ok(Some(payload))
        };
        #[cfg(test)]
        if let Some(runner) = self.runner {
            let mut given = Vec::new_in(jobs);
            for input in drive.inputs.iter().filter(|input| input.name.kind == Kind::Script) {
                let payload = match bytes(input.name)? {
                    Some(payload) => payload,
                    None => Vec::from_slice_in(work::payload(cx, store, input, jobs)?.payload(), jobs).leak(),
                };
                given.push((*input, payload));
            }
            return Ok(Dispatch::Run(ScriptWork::Double(runner, name, self.reads_planned.then_some(self.plant), given.leak()), drive));
        }
        let made = work::unwinding(|| crate::scripts::job_from(cx, handles.root, store, &mut self.scripts, &mut names.names, handles.env, handles.workers.get(), handles.lock, handles.members, &drive, frame, jobs, &mut bytes));
        let did = match made {
            Err(work::Panicked) => {
                let mut done = Completed::answer(drive, Did::Failed(refusal(name, "a build script's job panicked while it was made (its words are on the error stream)").failure))?;
                done.owed = true;
                return Ok(Dispatch::Answered(done));
            }
            Ok(made) => match made? {
                Prepared::Ready(mut job) => {
                    if self.reads_planned {
                        job.checkout = crate::scripts::CheckoutReads::Kept(self.plant);
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

    /// Consume completions in worker order to unblock readers. Only the engine's task order
    /// is used to publish completed records, notes and checkout reads in `run_phase`.
    #[expect(clippy::too_many_arguments, reason = "the script workers and the coordinator's phase inputs")]
    fn execute(
        &mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'_, 'f>,
        names: &mut NamesStage<'f>, tasks: &[EngineTask], frame: &'f Frame,
    ) -> Result<(), CliError> {
        let Some(first) = tasks.iter().find(|task| !self.completed.contains_key(&task.planned.name)) else { return Ok(()) };
        let wave = first.planned.phase.wave;
        let job_frame = Frame::with_reserve(frame.reserved(), bstd::mem::Tag::Other)
            .map_err(|_| CliError::Text { what: "reserving the frame for build-script jobs and producer records (engine_scripts.rs)" })?;
        let jobs = &job_frame;
        let mut payloads = BTreeMap::new_in(jobs);
        let mut ready = Ready::new(&self.runs, jobs);
        let mut deferred = Vec::with_capacity_in(self.runs.len(), jobs);
        deferred.resize(self.runs.len(), false);
        let mut finished = Vec::with_capacity_in(self.runs.len(), jobs);
        finished.resize_with(self.runs.len(), bstd::sync::Flag::new);
        let waiter = blaze_core::wait::Parker::new();
        let scope = cx.scope();
        scope.narrow_workers(handles.workers, Class::Foreground);
        let width = scope.workers();
        #[cfg(test)]
        let sim = self.runner.and_then(|runner| runner.clock());
        #[cfg(test)]
        let width = if sim.is_some() { handles.workers.get() } else { width };
        #[cfg(test)]
        let (mut tick, mut calendar) = (self.tick, BTreeMap::new_in(jobs));
        let width = width as usize;
        let mut running = BTreeMap::new_in(handles.env.heap.alloc());
        let result = (|| -> Result<(), CliError> {
            loop {
                while running.len() < width && let Some(at) = ready.queue.pop() {
                    let task = self.runs[at].task;
                    let name = task.planned.name;
                    let run = if task.planned.phase.wave < wave || self.completed.contains_key(&name) {
                        Some(false)
                    } else if task.planned.phase.wave == wave {
                        Some(tasks.binary_search_by_key(&name, |task| task.planned.name).is_ok())
                    } else if deferred[at] {
                        None
                    } else {
                        self.needs_run(task)
                    };
                    if run == Some(true) {
                        if name.kind == Kind::MacroModule {
                            let drive = self.drive(&task, handles.env.heap.alloc())?;
                            let did = self.lookup(cx, store, handles, names, &drive, frame)?;
                            self.completed.insert(name, Completed::answer(drive, did)?);
                            ready.finish(&self.runs[at].readers);
                            continue;
                        }
                        match self.prepare_run(cx, store, handles, names, task, frame, jobs, &mut payloads)? {
                            Dispatch::Run(job, drive) => {
                                let notice = || Notice { finished: &finished[at], waiter: &waiter };
                                #[cfg(test)]
                                let notice = match sim {
                                    Some(clock) => {
                                        calendar.insert((clock.start(name, tick), at), ());
                                        None
                                    }
                                    None => Some(notice()),
                                };
                                #[cfg(not(test))]
                                let notice = notice();
                                let clock = &handles.env.clock;
                                #[cfg(test)]
                                let root = handles.root;
                                let handle = scope.may_overlap("build script", crate::scripts::SCRIPT_FRAME, move |cx| {
                                    let _notice = notice;
                                    let pooled = match job {
                                        ScriptWork::Script(job) => crate::scripts::pooled(cx, &job, clock),
                                        #[cfg(test)]
                                        ScriptWork::Double(runner, name, plant, producers) => runner.run(cx, root, name, plant, producers),
                                    };
                                    Ok::<_, core::convert::Infallible>(pooled)
                                });
                                running.insert(at, (handle, drive));
                                continue;
                            }
                            Dispatch::Answered(done) => {
                                self.completed.insert(name, done);
                            }
                        }
                    } else if run.is_none() {
                        for &reader in &self.runs[at].readers {
                            deferred[reader] = true;
                        }
                    }
                    ready.finish(&self.runs[at].readers);
                }
                if running.is_empty() {
                    return Ok(());
                }
                let next = || loop {
                    if let Some(at) = running.keys().copied().find(|&at| finished[at].is_set()) {
                        break Ok(at);
                    }
                    let task = "a ready build script to finish";
                    let clock = WaitClock(&handles.env.clock);
                    let why = match waiter.park(task, cx.deadline().finite(), &clock) {
                        Ok(blaze_core::wait::Waited::Ready(())) => continue,
                        Ok(blaze_core::wait::Waited::Deadline) => bstd::task::TaskWhy::Unended {
                            running: running.len(),
                            stopped: bstd::Stopped::Deadline { span: task, at: cx.deadline().get() },
                        },
                        Err(error) => bstd::task::TaskWhy::Failed(error.to_string()),
                    };
                    break Err(CliError::Pool(bstd::task::TaskError { scope: cx.name(), task, why }));
                };
                #[cfg(test)]
                let at = match sim {
                    Some(clock) => {
                        let ((end, at), ()) = calendar.pop_first().expect("an occupied simulated worker has a finish tick");
                        tick = end;
                        clock.end(self.runs[at].task.planned.name, tick);
                        at
                    }
                    None => next()?,
                };
                #[cfg(not(test))]
                let at = next()?;
                let (handle, drive) = running.remove(&at).ok_or(CliError::Text { what: "a completion names no running build script (engine_scripts.rs)" })?;
                let pooled = handle.join(cx).map_err(CliError::Pool)?;
                self.completed.insert(drive.name, Completed::pooled(drive, pooled, frame)?);
                ready.finish(&self.runs[at].readers);
            }
        })();
        // Even a preparation refusal or pool failure must join all submitted work.
        let mut joined = Ok(());
        while let Some((_, (handle, _))) = running.pop_first() {
            if let Err(error) = handle.join(cx) {
                joined = Err(CliError::Pool(error));
            }
        }
        drop(running);
        #[cfg(test)]
        { self.tick = tick; }
        let settled = scope.join(cx).map_err(CliError::Pool);
        result.and(joined).and(settled)
    }

    /// One scripts phase: every task entered once, its result in `buffer`, each run marked in
    /// `names` for the crates that read it.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle or frame of it")]
    pub(crate) fn run_phase<'a>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'_>,
        handles: &Handles<'_, 'f>,
        names: &mut NamesStage<'f>,
        tasks: &[EngineTask],
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        frame: &'f Frame,
    ) -> Result<(), CliError> {
        let cx = cx.span("engine: one scripts phase");
        // Each `Given` cell of a script's read defines today's answer, the sweep's
        // (engine_script_reads.rs); the rest run below.
        let mut rest: Vec<EngineTask, &Frame> = Vec::with_capacity_in(tasks.len(), frame);
        for task in tasks.iter() {
            if task.planned.name.kind != Kind::Given {
                rest.push(*task);
                continue;
            }
            let at = self.given.binary_search_by_key(&task.planned.name, |(name, _, _)| *name).map_err(|_| CliError::from(EngineCheckError::Unseen { task: task.planned.name, read: task.planned.name }))?;
            buffer.done.push(crate::engine_script_reads::given_done(task, self.given[at].1, alloc));
        }
        let tasks: &[EngineTask] = &rest;
        self.execute(&cx, store, handles, names, tasks, frame)?;
        for task in tasks {
            let name = task.planned.name;
            let done = self.completed.remove(&name).ok_or(EngineCheckError::Unseen { task: name, read: name })?;
            let Completed { drive, made, owed, worker } = done;
            let used_unkept = worker.as_ref().is_some_and(|run| run.unkept.is_some());
            if let Some(run) = worker {
                times.push(name, run.ns);
                if run.produced {
                    if let Some(why) = run.unkept {
                        self.unkept.push((name, why));
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
                        // Each read no task of today's plan defines (new this request) is made by
                        // its reader, at wave 0, keyed as next request's planned task.
                        let tools = self.tools.ok_or(CliError::Text { what: "a build script ran before the scripts stage was planned (engine_scripts.rs `plan`)" })?;
                        for (_, read) in self.fresh.iter().filter(|(by, _)| *by == name) {
                            if self.given.binary_search_by_key(&read.name(), |(cell, _, _)| *cell).is_err() {
                                buffer.made.push(crate::engine_script_reads::made(read, name, &tools, alloc));
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
        Ok(())
    }

    /// One module lookup: a build script's (script_modules.rs) or a proc-macro crate's
    /// (macros.rs), as work.rs's `MacroModule` arm tells them apart.
    fn lookup(&mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, handles: &Handles<'_, 'f>, names: &NamesStage<'f>, drive: &blaze_drive::Task<'_>, frame: &'f Frame) -> Result<Did<'f>, CliError> {
        let world = names.names.as_ref().ok_or(CliError::Text { what: "the names world, made by the planner, is gone (engine_scripts.rs `lookup`)" })?;
        if let Some(module) = world.script_module(drive.name.subject) {
            // A fixture supplies the executable in place of the module on disk. Its
            // lookup succeeds so failed links producers go through the real Waited path.
            #[cfg(test)]
            if self.runner.is_some() {
                let mut module = work::Made::new_in(drive.name, frame);
                module.out = drive.name.subject;
                return Ok(Did::Done(module));
            }
            let wanted = crate::script_modules::Wanted { ran_as: module.ran_as, package: module.package, version: module.version, features: &module.features[..] };
            return crate::script_modules::run(cx, handles.env.home, wanted, drive, frame);
        }
        let Some(side) = handles.side else {
            let failure = TaskFailure { name: drive.name, why: Why::Refused { what: "a proc-macro module's lookup with no lock read this command", rule: RULE } };
            return Ok(Did::FailedSaying(failure, "a proc-macro crate's module is looked up by its lock entry, and this command read no lock"));
        };
        let place = crate::macros::Place { home: side.place.home, checkout: side.place.checkout, members: side.place.members, rows: side.place.rows };
        crate::macros::run(cx, store, Some(side.lock), &mut self.macros, place, drive, frame, frame)
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
