//! `blaze engine-check [dir] [--stats] [--workers N]` (engine.md E6): `check` driven through the
//! execution engine (crates/blaze-engine) beside the old machine, until the switch makes it
//! `check` itself and this word goes.
//!
//! The command plans, then drives:
//!
//! 1. **Plan.** The store is opened as `check` opens it. The layout is laid (units.rs `lay`),
//!    the dependency graph found forward from the lock and the fetches (engine_units.rs), the
//!    names world made from it. From the members' crate roots the files are found forward in
//!    waves, each wave's files probed and outlined at once on the workers. The plan: one `Units`
//!    task a units record, fetch and the graph; one `Outlines` task a file, its source the
//!    content id its bytes have today; one `Names` task a crate of the world, at its depth
//!    (engine_names.rs).
//! 2. **Drive.** One engine over an empty answer; each phase's tasks run (the outlines on the
//!    pool, engine_pool.rs; a names wave's resolutions on the workers), merged in bulk, then the
//!    phase's records put to the store sorted by key (engine_store.rs), where the next phase's
//!    tasks read them.
//! 3. **Print.** The rows the scope prints, drawn as `check` draws them (print.rs), then the
//!    closing line, which names every stage this path does not run yet as NOT checked.
//!
//! What it does not do yet: build scripts and macro modules as a stage, the heads and bodies
//! stages, the last answer, the profile, a member's scope.

use core::convert::Infallible;
use core::fmt;
use core::num::NonZeroU32;

use blaze_drive::{Condition, Locate, Name, Print, Why};
use blaze_engine::{Answer, Buffer, CellId, Done, Edit, Engine, EngineError, Everything, Phase, Planned, Pool, SMap, SSet, SVec, Stage, Task};
use blaze_own::{SharedAlloc, Vec};
use bstd::entry::Status;
use bstd::env::VarName;
use bstd::fs::{Dir, ReadWrite};
use bstd::{Cx, Io};
use bstd::mem::{Frame, TASK_FRAME};
use bstd::out::Out;
use bstd::path::{Path, PathError};
use bstd::task::Class;

use crate::checkout::Checkout;
use crate::engine_names::{Handles, NamesStage};
use crate::engine_pool::{Workers, WorkersOpts};
use crate::engine_store::Put;
use crate::engine_tasks::{Outlined, Tasks, WHOLE};
use crate::fetch::FetchEnv;
use crate::phases::Phases;
use crate::sweep::{self, FromMeta, Rows};
use crate::work::{self, Job, Unread};
use crate::{CliError, print};

/// Why `engine-check` stopped, naming the thing (rule 19).
#[derive(Debug)]
pub enum EngineCheckError {
    /// The engine refused its own run (engine.rs `EngineError`).
    Engine(EngineError),
    /// A module's file is at neither of its places: the old machine's `absent_module` finding
    /// has no runner on this path yet.
    Absent { path: Path },
    /// A part listed a file with no path beside it in its record.
    NoPath { name: Name },
    /// A wrapped function answered `Need`: `task` reads `read`, which its plan did not hand it.
    /// The planner's bug, never a second entry (engine.md, "a task in the plan is entered once").
    Unseen { task: Name, read: Name },
    /// A task was asked again after it answered this command.
    Twice { task: Name },
    /// The crates' dependency edges do not settle to depths: a cycle through `krate`.
    Cycle { krate: Name },
    /// The dependency graph was not resolved within this many levels of the lock (rule 18).
    Levels { levels: u32 },
}

impl fmt::Display for EngineCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineCheckError::Engine(error) => write!(f, "the engine: {error}"),
            EngineCheckError::Absent { path } => {
                write!(f, "`{}`: a module's file is absent; `engine-check` has no runner for an absent module yet (engine_check.rs `outline_one`)", path.as_str())
            }
            EngineCheckError::NoPath { name } => write!(f, "{name:?}: a file part with no path in its record (engine_check.rs `plan`)"),
            EngineCheckError::Unseen { task, read } => {
                write!(f, "{task:?} needs {read:?}, which its plan did not hand it: the planner did not see that read (engine_names.rs, engine_units.rs)")
            }
            EngineCheckError::Twice { task } => write!(f, "{task:?} was asked again after it answered (engine_names.rs `run_phase`)"),
            EngineCheckError::Cycle { krate } => write!(f, "the crates' dependencies do not settle to depths: a cycle, seen from {krate:?} (engine_names.rs `plan`)"),
            EngineCheckError::Levels { levels } => write!(f, "the dependency graph was not resolved in {levels} levels of the lock (engine_units.rs `LEVELS`)"),
        }
    }
}

impl From<EngineError> for CliError {
    fn from(error: EngineError) -> CliError {
        CliError::EngineCheck(EngineCheckError::Engine(error))
    }
}

impl From<EngineCheckError> for CliError {
    fn from(error: EngineCheckError) -> CliError {
        CliError::EngineCheck(error)
    }
}

/// The command: its arguments are `check`'s.
pub(crate) fn run(cx: &Cx, io: &Io, out: &Out, phases: &mut Phases, heap: &'static blaze_own::Heap) -> Result<Status, CliError> {
    let args = io.need_args()?;
    let flags = crate::flags(&args)?;
    let frame = cx.frame();
    let checkout = crate::checkout::checkout(flags.dir, frame)?;
    let fs = io.need_fs()?;
    let checkout_at: &str = match &checkout {
        Checkout::Under(dir) => work::held_str(dir.as_str(), frame),
        Checkout::Here => "",
    };
    let root = match checkout {
        Checkout::Under(dir) => fs.open_dir(cx, &dir)?,
        Checkout::Here => fs.root(),
    };
    let host = io.need_host()?;
    let cores = u32::try_from(host.cores().get()).unwrap_or(u32::MAX);
    let workers = flags.workers.unwrap_or(NonZeroU32::new(cores).unwrap_or(NonZeroU32::MIN));
    let producers = crate::producers::table(host.build_id()?.as_bytes())?;
    let tools = crate::engine_store::tools(&crate::home::tools(&producers, crate::lints::Lints::NONE.rules));
    let env = io.env();
    let var = |name: &str| -> Option<&str> { env.as_ref()?.get(&VarName::new(name).ok()?).ok().flatten() };
    // The fetch environment's facts, read at the edge as `check` reads them (main.rs).
    let cargo_home = match (var("CARGO_HOME"), var("HOME")) {
        (Some(cargo), _) => Some(bstd::mem::format_in!(frame, "{cargo}")),
        (None, Some(home)) => Some(bstd::mem::format_in!(frame, "{}/.cargo", home.trim_end_matches('/'))),
        (None, None) => None,
    };
    let exe_dir = match std::env::current_exe() {
        Err(error) => Err(crate::fetch::ExeDirWhy::Unknown { kind: error.kind() }),
        Ok(exe) => match exe.parent().and_then(|dir| dir.to_str()) {
            Some(dir) => Ok(bstd::mem::format_in!(frame, "{dir}")),
            None => Err(crate::fetch::ExeDirWhy::NotUtf8),
        },
    };
    let sdk = crate::sdk::resolve(cx, var("SDKROOT"), &crate::sdk::XcrunEnv { path: var("PATH"), developer_dir: var("DEVELOPER_DIR"), home: var("HOME") }, frame);
    let place = Place {
        checkout: checkout_at,
        cert_file: var("SSL_CERT_FILE"),
        cert_dir: var("SSL_CERT_DIR"),
        home: var("HOME"),
        cargo_home: cargo_home.as_ref().map(|cargo| cargo.as_str()),
        start: var("PWD"),
        sdk,
        exe_dir: exe_dir.as_ref().map(|dir| dir.as_str()).map_err(|why| *why),
    };
    let terminal = out.is_terminal();
    let caps = blaze_report::frame::detect(&var, terminal, out.size().map(|size| size.cols), flags.color);
    let view = blaze_report::frame::plain::view(flags.view, terminal);
    let shown_root = match (var("PWD"), flags.dir) {
        (Some(pwd), Some(dir)) => bstd::mem::format_in!(frame, "{pwd}/{dir}"),
        (Some(pwd), None) => bstd::mem::format_in!(frame, "{pwd}"),
        (None, dir) => bstd::mem::format_in!(frame, "{}", dir.unwrap_or(".")),
    };
    let mut shown = print::Shown::new(caps, view, shown_root.as_str(), frame);
    shown.each = flags.each;
    phases.mark_in("open checkout", frame);
    let opts = Opts { tools, workers, toolchain: blaze_toolchain::PINNED, heap, place };
    let err = io.need_err()?;
    Ok(check(cx, io, &root, opts, &mut shown, phases, out, &err)?.status)
}

/// Where the command runs and what the fetches and scripts read, each given by the edge (rule
/// 75; main.rs `request`'s first argument).
pub(crate) struct Place<'f> {
    /// The checkout, relative to where blaze was started ("" for there).
    pub(crate) checkout: &'f str,
    pub(crate) cert_file: Option<&'f str>,
    pub(crate) cert_dir: Option<&'f str>,
    /// `HOME`: the store and the sources are beneath it.
    pub(crate) home: Option<&'f str>,
    pub(crate) cargo_home: Option<&'f str>,
    /// `PWD`, where the toolchain's shelf is.
    pub(crate) start: Option<&'f str>,
    pub(crate) sdk: Result<blaze_apple_sdk::Sdk<'f>, blaze_apple_sdk::SdkRefused<'f>>,
    pub(crate) exe_dir: Result<&'f str, crate::fetch::ExeDirWhy>,
}

/// What [`check`] runs with, each given by the command's edge (rule 75).
pub(crate) struct Opts<'f> {
    /// Each kind's tool in the engine's form (engine_store.rs `tools`): in every task's key.
    pub(crate) tools: blaze_engine::Tools,
    pub(crate) place: Place<'f>,
    /// At most this many tasks at once.
    pub(crate) workers: NonZeroU32,
    /// The toolchain the command opens: in the units' output.
    pub(crate) toolchain: blaze_toolchain::ToolchainId,
    /// The process's heap: the plan, the tables and the buffers are on it.
    pub(crate) heap: &'static blaze_own::Heap,
}

/// What one [`check`] did: its exit status, and the engine's counts.
pub(crate) struct Checked {
    pub(crate) status: Status,
    /// The tasks planned.
    pub(crate) tasks: u32,
    /// The tasks the engine entered.
    pub(crate) entered: u32,
    /// Each phase run: its tasks planned and entered.
    pub(crate) phases: blaze_own::Vec<(Phase, u32, u32), blaze_own::Alloc<'static>>,
    /// The records put to the store, every phase's.
    pub(crate) puts: u32,
    /// Each stage's entries and how many came out the same bytes.
    pub(crate) stages: [blaze_engine::StageCount; blaze_engine::STAGES],
    /// Why tasks were entered, grouped, the most first (the profile's table, whole).
    pub(crate) because: blaze_own::Vec<(Because, u32), blaze_own::Alloc<'static>>,
}

/// The check of the checkout at `root`, printed through `shown` to `out`; the store's words on
/// `err`.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
pub(crate) fn check<'f>(cx: &'f Cx, io: &Io, root: &Dir<ReadWrite>, opts: Opts<'f>, shown: &mut print::Shown<'f>, phases: &mut Phases, out: &Out, err: &Out) -> Result<Checked, CliError> {
    let Opts { tools, workers, toolchain, heap, place } = opts;
    let frame = cx.frame();
    let began = phases.now();
    let erased = blaze_own::Erased::new(frame);
    // The one store, opened as `check` opens it (main.rs), committed after the answer.
    let opened = crate::store::open_for_command(cx, io, place.home, root, place.checkout, phases.clock(), err)?;
    let mut store = crate::store::CommandStore::new(cx, &opened.lent, false, opened.open_ns, None, phases.clock().clone(), err, erased.handle())?;
    phases.mark_in("store open", frame);
    let env = FetchEnv {
        clock: phases.clock().clone(),
        checkout: place.checkout,
        cert_file: place.cert_file,
        cert_dir: place.cert_dir,
        home: place.home,
        cargo_home: place.cargo_home,
        start: place.start,
        toolchain,
        fs: io.need_fs()?,
        sdk: place.sdk,
        exe_dir: place.exe_dir,
        heap,
    };

    // 1. Plan: the units stage, then the files, then the crates.
    let stamps = FromMeta::now(phases.clock());
    let mut rows = Rows::new_in(frame);
    let mut laid = crate::units::lay(cx, root, &stamps, &mut rows, toolchain, heap, frame, erased.handle())?;
    let home_dir = Some(opened.home.dir());
    sweep::read_home_listing(cx, home_dir, &stamps, crate::macros::MODULE_LISTING, &mut rows, frame)?;
    sweep::read_home_listing(cx, home_dir, &stamps, crate::scripts::MODULE_LISTING, &mut rows, frame)?;
    phases.mark_in("layout", frame);
    let units = sweep::units_source(rows.rows.iter().map(|leaf| (rows.path(leaf), leaf)), frame);
    if let (Some(lockfile), Some(parts)) = (laid.lock.as_ref(), laid.parts.take()) {
        let program = crate::program::dev_profile_panic(cx, root, frame)?;
        let target = crate::graph::target(cx, &env, program, &blaze_unit_graph::UnitKind::Lib, frame)?;
        let host = crate::graph::target(cx, &env, program, &blaze_unit_graph::UnitKind::BuildScript, frame)?;
        laid.graph = Some(crate::graph::Graph::new(parts, lockfile, units, target, host, true, heap, frame));
    }
    let mut discovered = crate::engine_units::discover(cx, root, &mut store, &mut laid, units, tools, &env, frame)?;
    phases.mark_in("plan (the units, the fetches and the graph)", frame);
    let alloc = heap.shared();
    let outlined = plan(cx, root, &stamps, &laid, workers, alloc)?;
    phases.mark_in("plan (files found and outlined)", frame);
    let mut names = discovered.names.take().map(|world| NamesStage::new(world, frame));
    let mut planned: SVec<'_, Planned> = SVec::new_in(alloc);
    for one in discovered.published.iter() {
        planned.push(Planned { name: one.name, phase: Phase::new(Stage::Units, one.wave), source: one.source, file: one.name });
    }
    for one in outlined.iter() {
        planned.push(Planned { name: one.name, phase: Phase::new(Stage::Outlines, 0), source: one.source, file: one.name });
    }
    if let Some(stage) = &names {
        planned.extend(stage.plan(alloc, frame)?);
    }
    phases.mark_in("plan (the crates' names at their depths)", frame);
    let mut files: SVec<'_, (Name, &[Planned])> = SVec::new_in(alloc);
    for one in planned.iter() {
        files.push((one.file, core::slice::from_ref(one)));
    }

    // 2. Drive: each phase run, merged, its records put.
    let mut engine = Engine::new_in(tools, Answer::new_in(alloc), &Edit { files: &files }, Everything, blaze_engine::Plant::None, alloc)?;
    let mut pool = Workers::new(WorkersOpts { cx, workers });
    let mut buffers: SVec<'_, Buffer<'_>> = SVec::new_in(alloc);
    for _ in 0..pool.runs() {
        buffers.push(Buffer::new_in(alloc));
    }
    let runner = Tasks { outlined: &outlined };
    let members = crate::names::member_dirs(Some(&laid));
    let side = laid.lock.as_ref().map(|lockfile| crate::proc_runner::MacroSide {
        lock: lockfile,
        place: crate::macros::Place { home: env.home, checkout: env.checkout, members, rows: &rows },
        heap,
    });
    let handles = Handles { root, lock: laid.lock.as_ref().map(|lockfile| lockfile.lock), members, env: &env, side: side.as_ref(), outlined: &outlined, workers };
    let mut ran = blaze_own::Vec::new_in(heap.alloc());
    let mut puts = 0u32;
    while let Some(phase) = engine.next_phase()? {
        let mut entered: SVec<'_, Task> = SVec::new_in(alloc);
        entered.extend_from_slice(engine.tasks());
        match phase.stage {
            Stage::Outlines => {
                let view = engine.view();
                pool.run(&entered, &mut buffers, &|cx: &Cx, task: &Task, buffer: &mut Buffer<'_>| runner.work(cx, view, task, buffer, alloc));
                // A pool's failure first: the merge's refusal after it is its consequence.
                pool.failed().map_err(CliError::Pool)?;
            }
            Stage::Units => {
                for task in entered.iter() {
                    buffers[0].done.push(publish(&discovered, task, alloc));
                }
            }
            Stage::Names => match names.as_mut() {
                Some(stage) => stage.run_phase(cx, &mut store, &handles, &entered, &mut buffers[0], alloc, frame)?,
                None => buffers[0].done.extend(entered.iter().map(|task| crate::engine_tasks::failed(task, alloc))),
            },
            // Not planned on this path yet: a task entered here is a failure row, named.
            Stage::Heads | Stage::Bodies => buffers[0].done.extend(entered.iter().map(|task| crate::engine_tasks::failed(task, alloc))),
        }
        engine.merge(&mut buffers)?;
        let planned_here = u32::try_from(planned.iter().filter(|one| one.phase == phase).count()).unwrap_or(u32::MAX);
        ran.push((phase, planned_here, u32::try_from(entered.len()).unwrap_or(u32::MAX)));
        // The phase's records to the store, sorted by key, before the next phase reads them.
        let mut put: Vec<Put<'_>, &Frame> = Vec::new_in(frame);
        for task in entered.iter() {
            let name = task.planned.name;
            let found = match phase.stage {
                Stage::Units => discovered.get(name).and_then(|one| one.made.as_ref().ok()).map(|record| (record.out, &record.reads[..], record.payload_bytes())),
                Stage::Outlines => outlined
                    .binary_search_by(|one| one.name.cmp(&name))
                    .ok()
                    .and_then(|at| outlined[at].made.as_ref().ok())
                    .map(|record| (record.out, &record.reads[..], record.payload_bytes())),
                Stage::Names => names
                    .as_ref()
                    .and_then(|stage| stage.results.iter().rev().find(|one| one.name == name))
                    .and_then(|one| one.made.as_ref().ok())
                    .map(|record| (record.out, &record.reads[..], record.payload_bytes())),
                Stage::Heads | Stage::Bodies => None,
            };
            if let Some((out, reads, payload)) = found {
                put.push(Put { key: task.key, name, source: task.planned.source, out, reads, payload });
            }
        }
        puts += crate::engine_store::put_all(cx, &mut store, &mut put, heap.alloc())?;
    }
    let finished = engine.finish()?;
    phases.mark_in("engine", frame);

    // 3. Print.
    let mut scratch = print::Scratch::new(work::Sizes::USUAL.scratch)?;
    let listed = erased.handle();
    let mut prints: Vec<Print<'_>, &Frame> = Vec::new_in(frame);
    let mut wanted: Vec<print::Wanted<'_>, &Frame> = Vec::new_in(frame);
    // (path, first print, past the last): each file's findings are neighbours.
    let mut runs: Vec<(Option<&str>, usize, usize), &Frame> = Vec::new_in(frame);
    let mut failures = 0u32;
    // Tasks that failed waiting on another's failure: counted under one line, as `check` does.
    let mut waited = 0u32;
    let mut seen: SMap<'_, Seen<'_>, usize> = SMap::new_in(alloc);
    for (name, _) in finished.answer.prints(&Everything, erased.handle()).iter() {
        let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(name)) else {
            let one = match discovered.get(*name) {
                Some(one) => Some(one),
                None => names.as_ref().and_then(|stage| stage.results.iter().find(|one| one.name == *name)),
            };
            let Some(one) = one else { continue };
            match &one.made {
                Ok(record) => record_prints(record, *name, listed, &mut prints, &mut wanted, &mut runs, &mut seen, frame)?,
                Err(failed) => {
                    failures += 1;
                    if matches!(failed.failure.why, Why::Waited { .. }) {
                        waited += 1;
                        continue;
                    }
                    let words = match failed.words {
                        Some(words) => bstd::mem::format_in!(frame, "{}: {words}", failed.failure),
                        None => bstd::mem::format_in!(frame, "{}", failed.failure),
                    };
                    print::note(cx, words.as_str(), shown, frame, out)?;
                }
            }
            continue;
        };
        let one = &outlined[at];
        let path = work::held_str(core::str::from_utf8(&one.path).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?, frame);
        let record = match &one.made {
            Ok(record) => record,
            Err(why) => {
                failures += 1;
                let failure = blaze_drive::TaskFailure { name: *name, why: *why };
                print::note(cx, bstd::mem::format_in!(frame, "`{path}`: its outline failed: {failure}").as_str(), shown, frame, out)?;
                continue;
            }
        };
        let first = prints.len();
        for &(rule, start, end, from, to) in record.findings.iter() {
            let mut message = Vec::new_in(listed);
            message.extend_from_slice(&record.messages[from..to]);
            let words: &[u8] = Vec::from_slice_in(&record.messages[from..to], frame).leak();
            prints.push(Print { name: *name, also: Vec::new_in(listed), condition: Condition::ANY, file: None, place: 0, rule, start, end, message });
            wanted.push(print::Wanted { path: Some(path), place: print::Place::At(0), words });
        }
        runs.push((Some(path), first, prints.len()));
    }
    let first = shown.listed.len();
    print::list(cx, root, &prints, &wanted, &mut scratch, frame, &mut shown.listed)?;
    crate::fix::lengths(&mut shown.listed[first..], frame);
    for &(path, from, past) in runs.iter() {
        print::file_findings(cx, root, path, first + from..first + past, shown, &mut scratch, out)?;
    }
    if waited > 0 {
        print::note(cx, bstd::mem::format_in!(frame, "{waited} task(s) not run: each waits on a task that failed").as_str(), shown, frame, out)?;
    }
    let (ran_stages, not_stages) = crate::check_coverage(names.is_some(), false, false, frame);
    let coverage = (!not_stages.is_empty()).then_some(blaze_report::frame::Coverage { ran: &ran_stages, not: &not_stages });
    for (stage, _, _) in work::CHECK_STAGES {
        if not_stages.contains(&stage) {
            print::note(cx, bstd::mem::format_in!(frame, "NOT checked: {stage}: `engine-check` runs the outlines and the names yet").as_str(), shown, frame, out)?;
        }
    }
    let checked = u32::try_from(outlined.len()).unwrap_or(u32::MAX);
    print::closing(cx, shown, checked, phases.since(began), Some(finished.report.entered), None, coverage, frame, out)?;
    phases.mark_in("answer printed", frame);
    let status = if shown.counts.errors > 0 || shown.counts.failures > 0 || failures > 0 || finished.report.failed > 0 {
        Status::FAILED
    } else if coverage.is_some() {
        crate::INCOMPLETE
    } else {
        Status::OK
    };
    // The store's one commit, after the answer, as `check`'s (main.rs).
    crate::store::Store::after_answer(&mut store, cx, &mut blaze_drive::profile::Profile::default())?;
    drop(store);
    crate::store::after_answer_printed(cx, opened.lent, false, phases.clock(), err)?;
    phases.mark_in("store committed", frame);
    let because = entered_because(&finished.causes, alloc);
    let text = profile_text(&ran, &finished.report, &because, &outlined, phases, frame);
    // Written in place, never synced, as `check`'s profile (print.rs `profile`).
    let file = root.create(cx, &Path::new(print::PROFILE)?)?;
    file.append(cx, text.as_bytes(), bstd::fs::Durable::No)?;
    file.close(cx, bstd::fs::Durable::No)?;
    let mut kept = blaze_own::Vec::new_in(heap.alloc());
    kept.extend(because.iter().copied());
    Ok(Checked { status, tasks: u32::try_from(planned.len()).unwrap_or(u32::MAX), entered: finished.report.entered, phases: ran, puts, stages: finished.report.stages, because: kept })
}

/// One group of the "entered because" table: the stage, the cause's kind, and the task that
/// defined the moved cell (a read moved; none for the other causes).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Because {
    pub(crate) stage: Stage,
    pub(crate) cause: &'static str,
    pub(crate) definer: Option<Name>,
}

/// How many tasks each [`Because`] entered, the most first, then by the group (rule 25: the
/// order is the data's).
fn entered_because<'a>(causes: &[blaze_engine::Entered], alloc: SharedAlloc<'a>) -> SVec<'a, (Because, u32)> {
    let mut groups: SMap<'_, Because, u32> = SMap::new_in(alloc);
    for one in causes.iter() {
        let definer = match one.cause {
            blaze_engine::Cause::ReadMoved { read, .. } => Some(read.of),
            _ => None,
        };
        *groups.entry(Because { stage: one.phase.stage, cause: one.cause.kind(), definer }).or_insert(0) += 1;
    }
    let mut sorted: SVec<'a, (Because, u32)> = SVec::new_in(alloc);
    sorted.extend(groups.iter().map(|(group, count)| (*group, *count)));
    sorted.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    sorted
}

/// The most groups the profile lists.
const BECAUSE_SHOWN: usize = 20;

/// The profile's text: each phase's tasks planned and entered, each stage's entries and how many
/// came out the bytes they had (a wasted entry), the top groups of why tasks were entered, then
/// the command's phases.
fn profile_text<'f>(ran: &[(Phase, u32, u32)], report: &blaze_engine::Report, because: &[(Because, u32)], outlined: &[Outlined<'_>], phases: &Phases, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    use core::fmt::Write as _;
    let mut text = blaze_own::String::new_in(frame);
    // A `fmt::Write` into a frame string fails only when the frame is full; the profile is a
    // report, so a full frame cuts it short where it stood.
    let _ = writeln!(text, "engine phases (stage, wave: planned, entered)");
    for (phase, planned, entered) in ran.iter() {
        let _ = writeln!(text, "  {:?} {:>4}: {planned:>7} {entered:>7}", phase.stage, phase.wave);
    }
    let _ = writeln!(text, "engine stages (entered, of which identical: the same bytes again)");
    for stage in Stage::ALL {
        let count = report.stages[stage.index()];
        if count.entered > 0 {
            let _ = writeln!(text, "  {stage:?}: {:>7} {:>7}", count.entered, count.identical);
        }
    }
    let _ = writeln!(text, "entered because (stage, cause, the moved cell's task: tasks), top {BECAUSE_SHOWN}");
    for (group, count) in because.iter().take(BECAUSE_SHOWN) {
        let _ = match group.definer {
            None => writeln!(text, "  {:?} {}: {count}", group.stage, group.cause),
            // A file's cell by its path, read as the sweep read it; any other task by its name.
            Some(name) => match outlined.binary_search_by(|one| one.name.cmp(&name)).ok().and_then(|at| core::str::from_utf8(&outlined[at].path).ok()) {
                Some(path) => writeln!(text, "  {:?} {} {:?} {path}: {count}", group.stage, group.cause, name.kind),
                None => writeln!(text, "  {:?} {} {name:?}: {count}", group.stage, group.cause),
            },
        };
    }
    if because.len() > BECAUSE_SHOWN {
        let _ = writeln!(text, "  ({} groups more)", because.len() - BECAUSE_SHOWN);
    }
    let _ = write!(text, "{phases}");
    text
}

/// A `Units` task's result: the record the planner made, published (its reads are its fetches,
/// each a cell of an earlier wave), or its failure.
fn publish<'a>(discovered: &crate::engine_units::Discovered<'_>, task: &Task, alloc: SharedAlloc<'a>) -> Done<'a> {
    let Some(Ok(record)) = discovered.get(task.planned.name).map(|one| one.made.as_ref()) else {
        return crate::engine_tasks::failed(task, alloc);
    };
    let mut defines = SVec::new_in(alloc);
    defines.push((WHOLE, record.out));
    let mut reads: SVec<'a, (CellId, blaze_ids::ContentId)> = SVec::new_in(alloc);
    for (name, digest) in record.reads.iter() {
        reads.push((CellId { of: *name, cell: WHOLE }, *digest));
    }
    reads.sort_unstable_by_key(|(id, _)| *id);
    reads.dedup_by_key(|(id, _)| *id);
    Done { name: task.planned.name, key: task.key, out: record.out, defines, reads, findings: u32::try_from(record.findings.len()).unwrap_or(u32::MAX), failed: false }
}

/// Each finding of a units or names record as `check` lists it (main.rs `draw`): an unresolved
/// path, an include outside its package and a failed call placed by their message's pieces,
/// every other at its record; one run of `runs` a file.
/// What makes two findings one when printed: their file, their item (its chain, or its start),
/// rule, range and words.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Seen<'p> {
    path: Option<&'p str>,
    at_item: (u8, u64),
    rule: u16,
    start: u32,
    end: u32,
    words: &'p [u8],
}

#[expect(clippy::too_many_arguments, reason = "the print lists of `check`, each one argument")]
fn record_prints<'l, 'p, 'v>(
    record: &work::Made<'_>,
    name: Name,
    listed: blaze_own::Alloc<'l>,
    prints: &mut Vec<Print<'l>, &'v Frame>,
    wanted: &mut Vec<print::Wanted<'p>, &'v Frame>,
    runs: &mut Vec<(Option<&'p str>, usize, usize), &'v Frame>,
    seen: &mut SMap<'_, Seen<'p>, usize>,
    frame: &'p Frame,
) -> Result<(), CliError> {
    for &(rule, start, end, from, to) in record.findings.iter() {
        let words = &record.messages[from..to];
        let placed = rule == crate::names::UNRESOLVED || rule == crate::names::include_outside::INCLUDE_OUTSIDE_PACKAGE || rule == crate::names::failed_call::FAILED_CALL;
        let (path, place, words): (Option<&'p str>, print::Place, &[u8]) = match placed {
            true => match crate::names::place_pieces(words)? {
                (Some((path, chain, edition)), words) => (Some(work::held_str(path, frame)), print::Place::Chain(chain, edition), words),
                (None, words) => (None, print::Place::At(0), words),
            },
            false => (None, print::Place::At(0), words),
        };
        let kept: &'p [u8] = Vec::from_slice_in(words, frame).leak();
        let at = prints.len();
        // One finding twice (two crates' scopes reach one file alike) prints once, the copy's
        // task named with it, as `check`'s answer does (blaze-drive request.rs `Print::also`).
        let at_item = match place {
            print::Place::Chain(chain, _) => (1, chain),
            print::Place::At(start) => (0, u64::from(start)),
            print::Place::Counted => (2, 0),
        };
        let what = Seen { path, at_item, rule, start, end, words: kept };
        if let Some(&first) = seen.get(&what) {
            if prints[first].name != name && !prints[first].also.contains(&name) {
                prints[first].also.push(name);
            }
            continue;
        }
        seen.insert(what, at);
        let mut message = Vec::new_in(listed);
        message.extend_from_slice(&record.messages[from..to]);
        prints.push(Print { name, also: Vec::new_in(listed), condition: Condition::ANY, file: None, place: 0, rule, start, end, message });
        wanted.push(print::Wanted { path, place, words: kept });
        match runs.last_mut() {
            Some(run) if run.0 == path && run.2 == at => run.2 = at + 1,
            _ => runs.push((path, at, at + 1)),
        }
    }
    Ok(())
}

/// A file the walk reached and has not outlined: its `FileItems` name, the slot its part named,
/// whether a `mod x;` named it (its file is at `x.rs` or `x/mod.rs`), and its edition byte.
struct Want<'h> {
    name: Name,
    slot: SVec<'h, u8>,
    module: bool,
    edition: u8,
}

/// Every file of the members' crates, outlined, sorted by name: found forward from the crate
/// roots, a wave at a time, each wave's files outlined at once on the workers.
fn plan<'h>(cx: &Cx, root: &Dir<ReadWrite>, stamps: &FromMeta, laid: &crate::units::Laid<'_>, workers: NonZeroU32, alloc: SharedAlloc<'h>) -> Result<SVec<'h, Outlined<'h>>, CliError> {
    let mut seen: SSet<'h, Name> = SSet::new_in(alloc);
    let mut wave: SVec<'h, Want<'h>> = SVec::new_in(alloc);
    for (_, made) in laid.members.iter() {
        wants(&made.parts, &made.paths, &made.path_bytes, &mut seen, &mut wave, alloc)?;
    }
    let mut outlined: SVec<'h, Outlined<'h>> = SVec::new_in(alloc);
    while !wave.is_empty() {
        let scope = cx.scope();
        scope.narrow_workers(workers, Class::Foreground);
        let mut handles = Vec::new_in(cx.frame());
        for want in wave.iter() {
            handles.push(scope.may_overlap("outline", TASK_FRAME, move |cx: &Cx| Ok::<_, Infallible>(outline_one(cx, root, stamps, want, alloc))));
        }
        let mut next: SVec<'h, Want<'h>> = SVec::new_in(alloc);
        let mut stopped = None;
        for (handle, want) in handles.into_iter().zip(wave.iter()) {
            match handle.join(cx).map(|one| one.map_err(|stopped| stopped.of(want))) {
                Ok(Ok(one)) => {
                    if let Ok(record) = &one.made {
                        wants(&record.parts, &record.paths, &record.path_bytes, &mut seen, &mut next, alloc)?;
                    }
                    outlined.push(one);
                }
                Ok(Err(error)) => {
                    stopped.get_or_insert(error);
                }
                Err(error) => {
                    stopped.get_or_insert(CliError::Pool(error));
                }
            }
        }
        scope.join(cx).map_err(CliError::Pool)?;
        if let Some(error) = stopped {
            return Err(error);
        }
        wave = next;
    }
    outlined.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(outlined)
}

/// Each file part of a record not seen yet, as a [`Want`] of `into`.
fn wants<'h>(
    parts: &[blaze_drive::Part],
    paths: &[(Name, u32, u32, bool)],
    path_bytes: &[u8],
    seen: &mut SSet<'h, Name>,
    into: &mut SVec<'h, Want<'h>>,
    alloc: SharedAlloc<'h>,
) -> Result<(), CliError> {
    for part in parts {
        let Locate::File { edition } = part.locate else { continue };
        if !seen.insert(part.name) {
            continue;
        }
        let Some(&(_, start, end, module)) = paths.iter().find(|(name, ..)| *name == part.name) else {
            return Err(EngineCheckError::NoPath { name: part.name }.into());
        };
        let mut slot = SVec::new_in(alloc);
        slot.extend_from_slice(&path_bytes[start as usize..end as usize]);
        into.push(Want { name: part.name, slot, module, edition });
    }
    Ok(())
}

/// Why a file's outline stopped the plan: sent back from its worker, so `Send` (a `CliError`
/// is made of it on the command's thread).
enum Stopped {
    Unread(Unread),
    /// The file is at neither of its places.
    Absent,
}

impl Stopped {
    fn of(self, want: &Want<'_>) -> CliError {
        match self {
            Stopped::Unread(error) => error.into(),
            Stopped::Absent => match core::str::from_utf8(&want.slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" }).and_then(|slot| Path::new(slot).map_err(|error: PathError| CliError::Path(error))) {
                Ok(path) => EngineCheckError::Absent { path }.into(),
                Err(error) => error,
            },
        }
    }
}

/// One file probed and outlined, on a worker, in its task's frame; the record in `alloc`.
fn outline_one<'h>(cx: &Cx, root: &Dir<ReadWrite>, stamps: &FromMeta, want: &Want<'_>, alloc: SharedAlloc<'h>) -> Result<Outlined<'h>, Stopped> {
    let scratch = cx.frame();
    let slot = core::str::from_utf8(&want.slot).map_err(|_| Stopped::Unread(Unread::Text("a path is not UTF-8")))?;
    let (flags, _, source) = sweep::probe_slot(cx, root, stamps, slot, want.module, scratch).map_err(Stopped::Unread)?;
    if flags & sweep::ABSENT != 0 {
        return Err(Stopped::Absent);
    }
    let mut path = SVec::new_in(alloc);
    path.extend_from_slice(&sweep::real_path(slot, flags, scratch));
    let job = Job { name: want.name, slot, flags, edition: want.edition, plant: work::Plant::None, expect: source, lints: false };
    let made = match work::unwinding(|| work::outline_file(cx, root, &job, scratch, alloc)) {
        Ok(Ok(made)) => made,
        Ok(Err(Unread::Changed)) => Err(Why::Changed),
        Ok(Err(error)) => return Err(Stopped::Unread(error)),
        Err(work::Panicked) => Err(Why::Panicked { used: scratch.peak() as u64, limit: scratch.reserved() as u64 }),
    };
    Ok(Outlined { name: want.name, path, source, made })
}
