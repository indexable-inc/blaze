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
//!   setting and the native runner's jobserver, rule 9) and the wave's runs at once on the
//!   workers (`scripts::pooled`), as the old loop runs them (work.rs `run_batches`).
//! - **Read**: a run's record is marked in the names stage, whose crate task hands it to
//!   names/scope.rs `prepare` by its key (the store holds it after the phase's put).

use core::convert::Infallible;

use blaze_drive::{Input, Key, Kind, Locate, Name, TaskFailure, Why};
use blaze_engine::{Buffer, CellId, Done, Phase, Planned, SVec, Stage, Task as EngineTask, Tools};
use blaze_ids::ContentId;
use blaze_own::{SharedAlloc, Vec};
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

/// The stage over one command's scripts and module lookups.
pub(crate) struct ScriptsStage<'f> {
    /// The command's scripts, opened by the first `Script` job (scripts.rs `Scripts::open`);
    /// dropped with the stage, which ends the native runner's jobserver.
    scripts: Option<Result<crate::scripts::Scripts<'f>, &'f str>>,
    /// The graph's proc-macro crates with their identities, listed by the first lookup.
    macros: Option<crate::macros::MacroCrates>,
    /// Each planned task's reads, by name, sorted: what its plan hands it.
    reads: Vec<(Name, Vec<Name, &'f Frame>), &'f Frame>,
    /// Each record entered or handed so far (the graph, then this stage's), by name: its key and
    /// out, `None` when it failed.
    made: Vec<(Name, Option<(Key, ContentId)>), &'f Frame>,
    /// Each task's result, in the order entered.
    pub(crate) results: Vec<Published<'f>, &'f Frame>,
}

impl<'f> ScriptsStage<'f> {
    pub(crate) fn new(frame: &'f Frame) -> ScriptsStage<'f> {
        ScriptsStage { scripts: None, macros: None, reads: Vec::new_in(frame), made: Vec::new_in(frame), results: Vec::new_in(frame) }
    }

    /// The result of `name`, if this stage entered it.
    pub(crate) fn result(&self, name: Name) -> Option<&Published<'f>> {
        self.results.iter().find(|one| one.name == name)
    }

    /// The stage's tasks: the lookups in wave 0, each run after its `links` producers. Nothing
    /// when the graph was not made (no lock, or its task failed): no crate then has names.
    pub(crate) fn plan<'h>(&mut self, names: &NamesStage<'_>, discovered: &Discovered<'_>, tools: Tools, env: &FetchEnv<'_>, alloc: SharedAlloc<'h>, frame: &'f Frame) -> Result<SVec<'h, Planned>, CliError> {
        let mut planned = SVec::new_in(alloc);
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
        // (name, source, wave, producers among the planned runs).
        let mut scripts: Vec<(Name, ContentId, u16, Vec<Name, &Frame>), &Frame> = Vec::new_in(frame);
        for (name, source) in crate::scripts::parts(names.script_keys(), crate::scripts::sdk_identity(env)) {
            let mut reads = reads_only(&[unit_graph]);
            let mut producers = Vec::new_in(frame);
            if let Ok(at) = runs.binary_search_by_key(&name.subject, |(subject, _, _)| *subject) {
                let (_, script, side) = runs[at];
                // Its module's lookup: the one its reading crate names (scripts.rs `job`).
                if let Some(module) = names.script_readers(name).next().and_then(|at| names.script_module_of(at)) {
                    reads.push(crate::script_modules::name(module.subject));
                }
                for (input, _, _) in blaze_proc_macro::script_unit::links_closure(units, script).iter() {
                    if let Some(producer) = crate::scripts::name_of(units, *input, side, heap.clone()) {
                        reads.push(producer);
                        producers.push(producer);
                    }
                }
            }
            // A run the graph does not name is the job's own refusal, said when it is entered.
            reads.sort_unstable();
            reads.dedup();
            self.reads.push((name, reads));
            scripts.push((name, source, 1, producers));
        }
        // Each run one wave after its deepest producer: relaxed to the longest path; a walk still
        // moving after as many rounds as runs is a `links` cycle (rule 18).
        let mut rounds = 0usize;
        loop {
            let mut moved = false;
            for at in 0..scripts.len() {
                let deepest = scripts[at].3.iter().filter_map(|producer| scripts.iter().find(|one| one.0 == *producer)).map(|one| one.2.saturating_add(1)).max().unwrap_or(1);
                if deepest > scripts[at].2 {
                    scripts[at].2 = deepest;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
            rounds += 1;
            if rounds > scripts.len() {
                return Err(EngineCheckError::Cycle { krate: scripts[0].0 }.into());
            }
        }
        for (name, source, wave, _) in scripts.iter() {
            planned.push(Planned { name: *name, phase: Phase::new(Stage::Scripts, *wave), source: *source, file: *name });
        }
        planned.sort_unstable_by_key(|one| one.name);
        planned.dedup_by_key(|one| one.name);
        self.reads.sort_unstable_by_key(|(name, _)| *name);
        self.reads.dedup_by_key(|(name, _)| *name);
        Ok(planned)
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
        let heap = handles.env.heap.alloc();
        // Each task's drive task, its reads handed whole.
        let mut drives = Vec::with_capacity_in(tasks.len(), frame);
        for task in tasks.iter() {
            let name = task.planned.name;
            let reads = self.reads.binary_search_by_key(&name, |(name, _)| *name).map(|at| &self.reads[at].1).map_err(|_| EngineCheckError::Unseen { task: name, read: name })?;
            let mut inputs = blaze_own::Vec::new_in(heap.clone());
            let mut failed = blaze_own::Vec::new_in(heap.clone());
            for read in reads.iter() {
                match self.made.binary_search_by_key(read, |(name, _)| *name) {
                    Ok(at) => match self.made[at].1 {
                        Some((key, out)) => inputs.push(Input { name: *read, at: key, out }),
                        None => failed.push(*read),
                    },
                    Err(_) => return Err(EngineCheckError::Unseen { task: name, read: *read }.into()),
                }
            }
            failed.sort_unstable();
            drives.push(drive_task(name, task.key, task.planned.source, inputs, failed, heap.clone()));
        }
        // 1. Each lookup answered and each run's job made, on this thread.
        let mut answered: Vec<(usize, Result<work::Made<'f>, Failure<'f>>), &Frame> = Vec::new_in(frame);
        let mut ready: Vec<(usize, crate::scripts::ScriptJob<'f>), &Frame> = Vec::new_in(frame);
        for (i, drive) in drives.iter().enumerate() {
            let name = drive.name;
            match name.kind {
                Kind::MacroModule => {
                    let did = self.lookup(&cx, store, handles, names, drive, frame)?;
                    answered.push((i, published(name, did)?));
                }
                Kind::Script => {
                    // A panic while its job is made fails this run alone (rule 20), as work.rs's.
                    let made = work::unwinding(|| crate::scripts::job(&cx, handles.root, store, &mut self.scripts, &mut names.names, handles.env, handles.workers.get(), handles.lock, handles.members, drive, frame, frame));
                    match made {
                        Err(work::Panicked) => answered.push((i, Err(refusal(name, "a build script's job panicked while it was made (its words are on the error stream)")))),
                        Ok(made) => match made? {
                            Prepared::Ready(job) => ready.push((i, job)),
                            Prepared::Need(wants) => return Err(EngineCheckError::Unseen { task: name, read: wants.first().map_or(name, |want| want.name) }.into()),
                            Prepared::Waited(on) => answered.push((i, Err(Failure { failure: TaskFailure { name, why: Why::Waited { on } }, words: None }))),
                            Prepared::Said { what, words, hole: _ } => {
                                answered.push((i, Err(Failure { failure: TaskFailure { name, why: Why::Refused { what, rule: crate::scripts::RULE } }, words: Some(words) })));
                            }
                        },
                    }
                }
                _ => answered.push((i, Err(refusal(name, "a task of a kind the scripts stage does not run")))),
            }
        }
        // 2. The wave's runs at once, on the workers, each in its own sandbox; a native run's
        // children share the command's jobserver (scripts.rs `Scripts::open`).
        if !ready.is_empty() {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let clock = &handles.env.clock;
            let mut running = Vec::new_in(frame);
            for (_, job) in ready.iter() {
                running.push(scope.may_overlap("build script", crate::scripts::SCRIPT_FRAME, move |cx: &Cx| Ok::<_, Infallible>(crate::scripts::pooled(cx, job, clock))));
            }
            let mut joined = Vec::new_in(frame);
            let mut stopped = None;
            for handle in running.into_iter() {
                match handle.join(&cx) {
                    Ok(one) => joined.push(one),
                    Err(error) => {
                        stopped.get_or_insert(error);
                    }
                }
            }
            scope.join(&cx).map_err(CliError::Pool)?;
            if let Some(error) = stopped {
                return Err(CliError::Pool(error));
            }
            for ((i, _), pooled) in ready.iter().zip(joined) {
                let name = drives[*i].name;
                times.push(name, pooled.ns);
                let did = match pooled.did {
                    Err(why) => Did::Failed(TaskFailure { name, why }),
                    Ok(Ran::Made(record)) => record.into_held(frame).map_or_else(|_| work::held_refused(name), Did::Done),
                    Ok(Ran::Failed { what, words, hole }) => {
                        let failure = TaskFailure { name, why: Why::Refused { what, rule: crate::scripts::RULE } };
                        let words = work::held_str(core::str::from_utf8(&words).map_err(|_| CliError::Text { what: "a build script's failure words are not UTF-8" })?, frame);
                        match hole {
                            true => Did::Hole(failure, words),
                            false => Did::FailedSaying(failure, words),
                        }
                    }
                };
                answered.push((*i, published(name, did)?));
            }
        }
        // Each result as the engine's row and this stage's record, in task order. Its reads: the
        // record's own and what its plan handed it, each a whole cell of an earlier phase.
        answered.sort_unstable_by_key(|(i, _)| *i);
        for (i, made) in answered.into_iter() {
            let task = &tasks[i];
            let name = task.planned.name;
            let done = match &made {
                Ok(record) => {
                    let mut reads: SVec<'a, (CellId, ContentId)> = SVec::new_in(alloc);
                    reads.extend(record.reads.iter().map(|(read, digest)| (CellId { of: *read, cell: WHOLE }, *digest)));
                    reads.extend(drives[i].inputs.iter().map(|input| (CellId { of: input.name, cell: WHOLE }, input.out)));
                    reads.sort_unstable_by_key(|(id, _)| *id);
                    reads.dedup_by_key(|(id, _)| *id);
                    let mut defines = SVec::new_in(alloc);
                    defines.push((WHOLE, record.out));
                    self.mark(name, Some((task.key, record.out)));
                    if name.kind == Kind::Script {
                        names.mark(name, Some((task.key, record.out)));
                    }
                    Done { name, key: task.key, out: record.out, defines, reads, findings: u32::try_from(record.findings.len()).unwrap_or(u32::MAX), failed: false }
                }
                Err(_) => {
                    self.mark(name, None);
                    if name.kind == Kind::Script {
                        names.mark(name, None);
                    }
                    crate::engine_tasks::failed(task, alloc)
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

/// A task refused on this path, naming why.
fn refusal(name: Name, what: &'static str) -> Failure<'static> {
    Failure { failure: TaskFailure { name, why: Why::Refused { what, rule: RULE } }, words: None }
}
