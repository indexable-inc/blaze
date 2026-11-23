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

use blaze_drive::{Kind, Locate, Name, Why};
use blaze_engine::{Answer, Buffer, CellId, Done, Edit, Engine, EngineError, Everything, Phase, Planned, Pool, SMap, SSet, SVec, Stage, StoredPages, Task};
use blaze_own::{SharedAlloc, Vec};
use bstd::entry::Status;
use bstd::env::VarName;
use bstd::fs::{Dir, ReadWrite};
use bstd::Cx;
use bstd::mem::{Frame, TASK_FRAME};
use bstd::out::Out;
use bstd::path::Path;
use bstd::task::Class;

use crate::engine_names::{Handles, NamesStage};
use crate::engine_scripts::ScriptsStage;
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
    /// The authenticated directory names a row absent from its page.
    MissingIndexedRow { name: Name },
    /// A standing producer must have a payload for the next stage to open.
    MissingRecord { name: Name },
    /// A part listed a file with no path beside it in its record.
    NoPath { name: Name },
    /// A wrapped function answered `Need`: `task` reads `read`, which its plan did not hand it.
    /// The planner's bug, never a second entry (engine.md, "a task in the plan is entered once").
    Unseen { task: Name, read: Name },
    /// [`EngineCheckError::Unseen`] of a file a crate's names task read (an `include!`, a
    /// `mod`'s file, a data file) that the file walk did not outline: named by the crate and the
    /// file's path in its tree, not by two hashes (rule 19; turso 2026-11-21 17:22: `core/io/unix.rs`,
    /// a `mod unix;` that `cfg_block!`'s expansion writes in `turso_core`'s `core/io/mod.rs`).
    UnseenFile { task: Name, read: Name, krate: blaze_own::String<SharedAlloc<'static>>, path: blaze_own::String<SharedAlloc<'static>> },
    /// Crate `krate`'s names read `count` files no outline of the request holds, past `cap`
    /// ([`FOUND_CAP`], rule 18): the chain named by its first and last file (engine_names.rs
    /// `unoutlined`), a macro that writes modules without end.
    FoundTooMany { task: Name, krate: blaze_own::String<SharedAlloc<'static>>, count: u32, cap: u32, first: blaze_own::String<SharedAlloc<'static>>, last: blaze_own::String<SharedAlloc<'static>> },
    /// The found file `file` is read by member units of two roots: its place keeps one root
    /// ([`blaze_engine::pages::FilePlace`]'s `unit`), so the other's edge would be lost (rule 3).
    FoundTwoRoots { file: Name, first: Name, second: Name },
    /// Two names tasks outlined the found file `file` to two outlines: its text moved between
    /// their reads (one outline a file a request, rule 21).
    FoundTwoOutlines { file: Name },
    /// The names task `task` found `file` on the ready driver. The ready driver runs no names
    /// task (engine_ready_edit.rs `admit`: `READY_KINDS` holds no `CrateScope`; an edit that
    /// enters one takes the phase path), so it finds no file, and a found file of the last
    /// answer is planned there from its place as any file is: a find here is the driver's own
    /// inconsistency, checked after its drive (rule 3), never a request it serves.
    FoundOnReady { task: Name, file: Name },
    /// A table a stage keeps for the request holds no entry for `key` where `task`'s step must
    /// find one: the stage's own inconsistency, named by the table (rule 19), never a task's
    /// need (that is [`EngineCheckError::Unseen`] alone, rule 30).
    MissingEntry { task: Name, table: Table, key: Name },
    /// A task was admitted twice or its result was filed twice in one request.
    Twice { task: Name },
    /// The crates' dependency edges do not settle to depths: a cycle through `krate`.
    Cycle { krate: Name },
    /// The dependency graph was not resolved within this many levels of the lock (rule 18).
    Levels { levels: u32 },
    /// A list whose size follows the project could not grow: its allocator refused (rule 3).
    Grow { what: &'static str, had: usize },
    /// A crate's `CrateScope` payload whose `cells` section does not read as whole sorted rows
    /// (names/sections.rs, blaze-scope names_cells.rs `TABLE_ROW`): its cells cannot be defined.
    CellTable { task: Name, what: &'static str },
    /// A file whose row stands could not have its outline read back from the store.
    OutlineBack(crate::engine_store::OutlineBack),
    /// A body's findings cannot be placed for printing (engine_bodies.rs `lines`): never dropped.
    Unplaced { body: Name, what: &'static str },
    /// A body's run facts record could not be made at the Bodies put (engine_body_facts.rs).
    BodyFacts { body: Name, why: crate::engine_body_facts::MakeError },
    /// An entry whose bodies were checked in slices could not be made one result again
    /// (engine_bodies.rs `join_slices`): the stage's own inconsistency, named.
    Sliced { body: Name, what: &'static str },
    /// Stopped by a signal at a scheduling point, in `stage` (none: before the first), `at_ms`
    /// after the check began: its status is in the profile and on stderr (engine_status.rs).
    Stopped { by: bstd::procs::By, stage: Option<Stage>, at_ms: u64 },
    /// The command's stop and status signals were not caught (blaze-core run_signals.rs `Asked`).
    Signals(bstd::procs::SignalError),
    /// The status watcher did not run (engine_status.rs `watched`).
    Watcher(crate::engine_status::StatusError),
    /// The checkout's root did not resolve at the command's edge: its path and the OS's error
    /// (engine_script_reads.rs `Served::at_edge`).
    ServedRoot(blaze_proc_macro::script_sandbox::RootUnresolved),
    /// The request's or a phase's arena could not be reserved (engine_phase_mem.rs).
    PhaseMem(crate::engine_phase_mem::PhaseMemError),
    /// A start read the answer its check had just stored, and the store holds no pointer at its
    /// root name (`Some`: one it could not read). The check's bug, never a second check.
    StartNoAnswer { why: Option<crate::engine_answer::Unreadable> },
    /// The stored answer has no unit graph row with a record (no `Cargo.lock`, or the layout
    /// refused): no program's crates can be found, so nothing starts.
    StartNoGraph,
    /// A row a start reads (`name`) is on a page of the stored answer that was refused.
    StartPage { name: Name, why: &'static str },
    /// `--driver=ready` asked for a driver that cannot serve this request (engine_ready_edit.rs
    /// `choose`/`admit`): refused by name, never run on the phases instead (rule 3).
    ReadyRefused(crate::engine_ready_edit::ReadyRefused),
    /// The ready driver stopped (blaze-engine ready.rs `Error`), kept in the command's heap.
    Ready(blaze_engine::ready::Error<'static>),
    /// A ready kind's run could not give `task` its row (engine_ready_check.rs): its file or
    /// unit not in the request's outlines, its lint walk not holding it, a body that made a
    /// record inline; or the ready scope left it owed. The ready path's own inconsistency,
    /// named, never run another way.
    ReadyHost { task: Name, what: &'static str },
    /// The ready path's kinds table refused (blaze-engine kinds.rs `KindsError`).
    ReadyKinds(blaze_engine::kinds::KindsError),
    /// A task needed the names world and it could not be made from the graph's record the
    /// crate list was read with (names.rs `made_or_make`).
    NamesWorld(crate::names::WorldRefused),
    /// A block of the last lines record could not be split or searched where the tail carries
    /// its entries (engine_lines.rs `keep_split`, `has_told`): checked at its decode, so a bug.
    Lines(crate::engine_lines::LinesError),
    /// A crate's module tree reaches a file none of the request's outlines holds
    /// (engine_body_plan.rs `reached`): the plan's or the ready setup's gap, named (rule 3).
    Unoutlined(crate::engine_body_plan::Unoutlined),
}

/// The request's tables a stage looks an entry up in, each named where it lives
/// ([`EngineCheckError::MissingEntry`]). One variant a table; a stage that keeps another adds
/// its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Table {
    /// The names world's crate index (names.rs `Names::crate_at`).
    WorldCrates,
    /// The names stage itself, or its world, gone while a names step needs it
    /// (engine_names.rs `NamesStage::names`, engine_check.rs `Drive::names`).
    NamesWorld,
    /// The dependencies' exports mapped for one names wave (engine_names.rs `WaveInputs::slots`).
    MappedExports,
    /// The wave's wire headers waiting for the put (engine_names.rs `NamesStage::heads`).
    WaveHeads,
    /// The pipelined names tasks running (engine_names.rs `NamesQueue::running`).
    NamesRunning,
    /// The pipelined names tasks answered and not yet filed (engine_names.rs `NamesQueue::answered`).
    NamesAnswered,
    /// The pipelined names tasks' filing order (engine_names.rs `NamesQueue::order`).
    NamesOrder,
    /// The Bodies phase's run facts, handed to its put (engine_check.rs `run_stage`, `put_phase`).
    BodyFacts,
    /// The names world's build-script module index (names.rs `script_module`).
    WorldScriptModules,
    /// Each planned script task's reads (engine_scripts.rs `ScriptsStage::reads`).
    ScriptReads,
    /// The planned script runs by name (engine_scripts.rs `ScriptsStage::runs`).
    ScriptRuns,
    /// The script tasks running on workers (engine_scripts.rs `ScriptQueue::running`).
    ScriptsRunning,
    /// A finished script worker's answer slot (engine_scripts.rs `Running::output`).
    ScriptsOutput,
    /// The script tasks' filing order (engine_scripts.rs `ScriptQueue::order`).
    ScriptsOrder,
    /// The given cells' tasks (engine_scripts.rs `ScriptsStage::given`).
    ScriptsGiven,
    /// The script tasks answered and not yet filed (engine_scripts.rs `ScriptsStage::completed`).
    ScriptsCompleted,
    /// What the names said of the found files, applied once (engine_check.rs `Served`): a found
    /// file's place, its outline's task in its plan, or the table set a second time.
    FoundServed,
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Table::WorldCrates => "the names world's crate index (names.rs `crate_at`)",
            Table::NamesWorld => "the names stage's world (engine_names.rs `NamesStage::names`)",
            Table::MappedExports => "the names wave's mapped dependency exports (engine_names.rs `WaveInputs::slots`)",
            Table::WaveHeads => "the names wave's headers for the put (engine_names.rs `NamesStage::heads`)",
            Table::NamesRunning => "the running names tasks (engine_names.rs `NamesQueue::running`)",
            Table::NamesAnswered => "the answered names tasks (engine_names.rs `NamesQueue::answered`)",
            Table::NamesOrder => "the names tasks' filing order (engine_names.rs `NamesQueue::order`)",
            Table::BodyFacts => "the Bodies phase's run facts (engine_check.rs `run_stage`)",
            Table::WorldScriptModules => "the names world's build-script module index (names.rs `script_module`)",
            Table::ScriptReads => "the planned script tasks' reads (engine_scripts.rs `ScriptsStage::reads`)",
            Table::ScriptRuns => "the planned script runs (engine_scripts.rs `ScriptsStage::runs`)",
            Table::ScriptsRunning => "the running script tasks (engine_scripts.rs `ScriptQueue::running`)",
            Table::ScriptsOutput => "a finished script worker's answer slot (engine_scripts.rs `Running::output`)",
            Table::ScriptsOrder => "the script tasks' filing order (engine_scripts.rs `ScriptQueue::order`)",
            Table::ScriptsGiven => "the given cells' tasks (engine_scripts.rs `ScriptsStage::given`)",
            Table::ScriptsCompleted => "the answered script tasks (engine_scripts.rs `ScriptsStage::completed`)",
            Table::FoundServed => "the found files served (engine_check.rs `Served`)",
        })
    }
}

impl From<crate::engine_phase_mem::PhaseMemError> for CliError {
    fn from(error: crate::engine_phase_mem::PhaseMemError) -> CliError {
        CliError::EngineCheck(EngineCheckError::PhaseMem(error))
    }
}

impl EngineCheckError {
    /// `value` pushed onto `list` (`what`), or the typed refusal naming the list and its length.
    pub(crate) fn push<T, A: core::alloc::Allocator>(list: &mut Vec<T, A>, value: T, what: &'static str) -> Result<(), EngineCheckError> {
        let had = list.len();
        list.try_push(value).map_err(|_| EngineCheckError::Grow { what, had })
    }
}

impl fmt::Display for EngineCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineCheckError::Engine(error) => write!(f, "the engine: {error}"),
            EngineCheckError::MissingIndexedRow { name } => write!(f, "stored answer directory names missing row {name:?}"),
            EngineCheckError::MissingRecord { name } => write!(f, "stored answer row {name:?} has no producer record"),
            EngineCheckError::NoPath { name } => write!(f, "{name:?}: a file part with no path in its record (engine_check.rs `plan`)"),
            EngineCheckError::Unseen { task, read } => {
                write!(f, "{task:?} needs {read:?}, which its plan did not hand it: the planner did not see that read (engine_names.rs, engine_units.rs)")
            }
            EngineCheckError::UnseenFile { task, read, krate, path } => write!(
                f,
                "crate `{krate}`'s names read the file `{path}` ({task:?} needs {read:?}), which its plan did not hand it: the file walk did not outline that file, and the names did not serve it. A module file a macro writes (`cfg_if!`'s `mod unix;`, `mod $name;`, a proc macro's `mod`), or an `include!` of a computed path, is outlined by the names that read it and planned after them (engine_names.rs `Job::run`, engine_check.rs `serve_found`); this read was not served (the control's plant, engine_check.rs `without_found_serve`)"
            ),
            EngineCheckError::FoundTooMany { task, krate, count, cap, first, last } => write!(
                f,
                "crate `{krate}`'s names ({task:?}) read {count} files the file walk did not outline, from `{first}` to `{last}`: past the cap of {cap} files a crate's names may find in one request (rule 18, engine_check.rs `FOUND_CAP`, engine_names.rs `unoutlined`)"
            ),
            EngineCheckError::FoundTwoRoots { file, first, second } => write!(f, "the found file {file:?} is read by the names of member units of two roots, {first:?} and {second:?}: its place keeps one root, so the other's module tree would lose it (engine_check.rs `serve_found`, rule 3)"),
            EngineCheckError::FoundTwoOutlines { file } => write!(f, "two crates' names outlined the found file {file:?} to two outlines in one request: its text moved between their reads (engine_check.rs `serve_found`, rule 21)"),
            EngineCheckError::FoundOnReady { task, file } => write!(f, "{task:?} read {file:?}, a file no outline of the request holds, on the ready driver, which runs no names task (engine_ready_edit.rs `admit`, `READY_KINDS`): the driver's own inconsistency (engine_check.rs `FoundOnReady`)"),
            EngineCheckError::MissingEntry { task, table, key } => write!(f, "{task:?}: {table} holds no entry for {key:?}: the stage's own inconsistency, never a read the plan missed"),
            EngineCheckError::Twice { task } => write!(f, "{task:?} was admitted or filed more than once in this request"),
            EngineCheckError::Cycle { krate } => write!(f, "the crates' dependencies do not settle to depths: a cycle, seen from {krate:?} (engine_names.rs `plan`)"),
            EngineCheckError::Levels { levels } => write!(f, "the dependency graph was not resolved in {levels} levels of the lock (engine_units.rs `LEVELS`)"),
            EngineCheckError::Grow { what, had } => write!(f, "{what}: the list could not grow past {had} entries: its allocator refused (engine_*.rs)"),
            EngineCheckError::CellTable { task, what } => write!(f, "{task:?}: its record's cell table {what} (engine_names.rs `cells_of`)"),
            EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name, key }) => {
                write!(f, "{name:?}: its row stands, and the store holds no outline at {key:?}, the key the stored answer names (engine_check.rs `plan`; run `blaze check` with the answer cleared)")
            }
            EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Unreadable { name, key, why }) => {
                write!(f, "{name:?}: the outline at {key:?} is no whole drive record of it ({why:?}) (engine_store.rs `outline_from`)")
            }
            EngineCheckError::Unplaced { body, what } => write!(f, "{body:?}: its findings cannot be placed for printing: {what} (engine_bodies.rs `lines`)"),
            EngineCheckError::BodyFacts { body, why } => write!(f, "{body:?}: its run facts record cannot be made: {why}"),
            EngineCheckError::Sliced { body, what } => write!(f, "{body:?}: its bodies were checked in slices and cannot be made its one result again: {what} (engine_bodies.rs `join_slices`)"),
            EngineCheckError::Stopped { by, stage, at_ms } => match stage {
                Some(stage) => write!(f, "stopped by {} at {at_ms} ms in stage {stage:?}; where it stood is in {} (engine_status.rs)", by.name(), print::PROFILE),
                None => write!(f, "stopped by {} at {at_ms} ms before the first stage; where it stood is in {} (engine_status.rs)", by.name(), print::PROFILE),
            },
            EngineCheckError::Signals(error) => write!(f, "its stop and status signals: {error} (engine_check.rs `serve`)"),
            EngineCheckError::Watcher(error) => write!(f, "{error}"),
            EngineCheckError::ServedRoot(error) => write!(f, "the checkout's root: {error} (engine_script_reads.rs `Served::at_edge`)"),
            EngineCheckError::PhaseMem(error) => write!(f, "{error}"),
            EngineCheckError::StartNoAnswer { why: None } => write!(f, "the check stored no answer at the checkout's root name for the start to read (engine_check.rs `check_for_start`)"),
            EngineCheckError::StartNoAnswer { why: Some(why) } => write!(f, "the answer the check stored could not be read for the start: {why} (engine_check.rs `check_for_start`)"),
            EngineCheckError::StartNoGraph => write!(f, "the checkout's answer has no dependency graph (no `Cargo.lock`, or its layout was refused): no program can start (engine_check.rs `check_for_start`)"),
            EngineCheckError::StartPage { name, why } => write!(f, "{name:?}: {why}, so the start cannot read its row (engine_run_answer.rs `Records`)"),
            EngineCheckError::ReadyRefused(refused) => write!(f, "{refused}"),
            EngineCheckError::Ready(error) => write!(f, "the ready driver: {error}"),
            EngineCheckError::ReadyHost { task, what } => write!(f, "{task:?}: {what} (engine_ready_check.rs)"),
            EngineCheckError::ReadyKinds(error) => write!(f, "the ready path's kinds: {error} (engine_ready_check.rs `drive_edit`)"),
            EngineCheckError::NamesWorld(why) => write!(f, "{why}"),
            EngineCheckError::Lines(why) => write!(f, "{why} (engine_check.rs `lines_of`, `ready_setup`)"),
            EngineCheckError::Unoutlined(why) => write!(f, "{why}"),
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

/// Cached `UnitGraph` crate-list & pre-indexed `Names` tables held by the resident daemon across
/// incremental edits so `ready_setup` skips `stored_graph_at`, `CrateList::open`, and ~3,300
/// BLAKE3 digests in `Names::of_list` (Wave 7).
pub(crate) struct HotGraph {
    pub(crate) key: blaze_drive::Key,
    pub(crate) out: blaze_ids::ContentId,
    pub(crate) hot_list: crate::names::HotList,
}

/// Outcome returned from [`check_in_daemon`] to the resident daemon server so it can update
/// `DaemonCore`, arm/refresh `KqueueVnode`, and schedule post-`Done` reclaim/bg-GC.
#[derive(Default)]
pub(crate) struct DaemonCheckOut {
    pub(crate) was_phase_rebuild: bool,
    pub(crate) spawn_bg: bool,
    pub(crate) wrote_pointer: bool,
    pub(crate) leaf_set_changed: bool,
    pub(crate) certified: bool,
    pub(crate) lint_step: Option<blaze_lint::Step>,
    pub(crate) certified_facts: Option<(blaze_drive::Key, blaze_drive::Key, crate::engine_answer::Facts)>,
}

/// Per-request context passed from `daemon_server.rs` into [`check_in_daemon`] so the daemon
/// reuses its open store, feeds its synchronous `kqueue` dirty set (bypassing `journal-helper`),
/// reuses its cached [`HotGraph`], and defers arena reclamation / background GC until after
/// `ServerFrame::Done` is sent to the client.
pub(crate) struct DaemonCheckCtx<'d> {
    pub(crate) opened: &'d crate::store::Opened,
    pub(crate) watch: Option<crate::daemon_watch::PollOutcome<'d>>,
    pub(crate) hot_graph: &'d mut Option<HotGraph>,
    pub(crate) requests: Option<&'d bstd::procs::Requests>,
    pub(crate) out: DaemonCheckOut,
}

/// Runs one `blaze check` request inside the resident per-checkout daemon, reusing the daemon's
/// open store, watcher poll result, and `HotGraph`, without installing per-request signal handlers.
#[expect(clippy::too_many_arguments, reason = "the daemon check edge: each argument one specific capability")]
pub(crate) fn check_in_daemon<'d>(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    checkout_root: Dir<ReadWrite>,
    checkout_root_abs: &str,
    sdk_override: Option<blaze_apple_sdk::Sdk<'_>>,
    daemon: &mut DaemonCheckCtx<'d>,
) -> Result<Status, CliError> {
    let flags = crate::flags(args)?;
    crate::check_command::refuse_not_taken(&flags)?;
    let request = request_frame()?;
    let located = crate::run_package::Located {
        root: checkout_root,
        at: work::held_str(checkout_root_abs, &request),
        member: "",
    };
    let daemon_requests = daemon.requests;
    edge_at(cx, fs, env, net, host, out, err, phases, heap, &flags, None, Some(located), true, daemon_requests, sdk_override.map(Ok), &request, |edge, phases| {
        let Edge { root, opts, mut shown, err, caps, view, shown_root, frame, .. } = edge;
        let checked = match flags.fix {
            None => {
                let line = crate::run_line::LineOpts::from_env(
                    env,
                    err,
                    flags.color,
                    blaze_screen::Command::Check,
                    view,
                    flags.status_every.is_some(),
                );
                let ran = crate::run_line::watched(cx, err, clock, line, |watch, bg| {
                    watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Checking));
                    let live = line.live.then_some(bg);
                    let res = check_live_with_daemon(cx, fs, host, rng, Some(args), net, root, opts(), &mut shown, phases, out, err, live, Some(daemon));
                    watch.see(blaze_screen::Step::Says);
                    res
                }).map_err(CliError::RunLine)??;
                Some(ran)
            }
            Some((start, end)) => {
                let cap = (end - start).max(1);
                let mut selectors: blaze_own::Vec<&str, &Frame> = blaze_own::Vec::with_capacity_in(cap, frame);
                selectors.extend((start..end).filter_map(|at| args.get(at)));
                if selectors.is_empty() {
                    selectors.push("all");
                }
                let mut ask = |mut shown| {
                    daemon.opened.lent.rearm_lease();
                    check_live_with_daemon(cx, fs, host, rng, Some(args), net, root, opts(), &mut shown, phases, out, err, None, Some(&mut *daemon)).map(|checked| (checked, shown))
                };
                match crate::fix_then_check::<crate::fix::Atomic, Checked>(cx, root, &selectors, (caps, view, shown_root), frame, out, &mut ask)? {
                    crate::Fixed::Refused => None,
                    crate::Fixed::Checked(checked, _) => Some(checked),
                }
            }
        };
        let Some(checked) = checked else { return Ok(Status::FAILED) };
        if flags.stats {
            err.line(cx, format_args!("{}", stats_line(&checked, phases.in_main(), frame)))?;
        }
        Ok(checked.status)
    })
}

/// The command: its arguments are `check`'s.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one specific capability")]
pub(crate) fn run(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    match serve(cx, args, fs, env, net, host, clock, rng, out, err, phases, heap, None) {
        // Its words are on stderr and in the profile already: the status a shell gives a
        // process the signal ended (128 and its number).
        Err(CliError::EngineCheck(EngineCheckError::Stopped { by, .. })) => Ok(Status(128 + by.number())),
        other => other,
    }
}

/// `blaze why <file or file::item>` (engine.md E11), in the checkout blaze stands in: why its
/// task was entered, step by step back to the edit (engine_why.rs).
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one specific capability")]
pub(crate) fn why(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    let target = args.get(2).filter(|word| !word.starts_with("--")).ok_or(CliError::Usage("blaze why <file or file::item>: a file's path in the checkout, or an item in it as `path::name`"))?;
    let target = work::held_str(target, cx.frame());
    serve(cx, args, fs, env, net, host, clock, rng, out, err, phases, heap, Some(target))
}

const WARM_TOOLCHAIN_USAGE: &str = "blaze warm-toolchain [--workers N] [--origin <url|folder>] [--origin-key <hex>] [--fresh] [--stats]: warm core/alloc/std/proc_macro (target and wasm32-wasip1 macro side, edition 2024) and bstd (edition blaze-2026.11) into the shared home store";

const WARM_WORKSPACE_TOML: &[u8] = b"[workspace]\nmembers = [\"std_probe\", \"bstd_probe\", \"pm_probe\"]\nresolver = \"2\"\n";
const WARM_STD_TOML: &[u8] = b"[package]\nname = \"std_probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const WARM_STD_LIB: &[u8] = b"pub fn warm_std() -> usize {\n    std::vec::Vec::<u8>::new().len()\n}\n";
const WARM_BSTD_TOML: &[u8] = b"[package]\nname = \"bstd_probe\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n";
const WARM_BSTD_LIB: &[u8] = b"pub fn warm_bstd() -> usize {\n    0\n}\n";
const WARM_PM_TOML: &[u8] = b"[package]\nname = \"pm_probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n";
const WARM_PM_LIB: &[u8] = b"extern crate proc_macro;\n\n#[proc_macro]\npub fn warm_pm(input: proc_macro::TokenStream) -> proc_macro::TokenStream {\n    input\n}\n";

fn write_warm_file(cx: &Cx, dir: &Dir<ReadWrite>, path: &str, bytes: &[u8], frame: &Frame) -> Result<(), CliError> {
    let p = Path::new(path)?;
    let mut existing = bstd::mem::Bytes::new_in(frame, "warm-toolchain existing");
    match dir.read_all(cx, &p, &mut existing) {
        Ok(_) if existing.as_slice() == bytes => return Ok(()),
        Ok(_) => {}
        Err(err) if matches!(err.kind, bstd::fs::FsKind::NotFound) => {}
        Err(err) => return Err(err.into()),
    }
    dir.publish(cx, &p, bstd::fs::Content::bytes(bytes), bstd::fs::Durable::No)?;
    Ok(())
}

/// `blaze warm-toolchain`: pre-warms `core`/`alloc`/`std` (edition 2024) + `bstd`
/// (edition `blaze-2026.11`) into the shared `~/.blaze/v2` store and refreshes
/// `tool-base.<PINNED>.<TRIPLE>.<tools>` so every fresh project and worktree reuses them.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one specific capability")]
pub(crate) fn warm_toolchain(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    _clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    let mut flags = crate::Flags::empty();
    let mut fresh = false;
    let mut at = 2;
    while let Some(arg) = args.get(at) {
        at += 1;
        match arg {
            "-h" | "--help" => {
                out.line(cx, WARM_TOOLCHAIN_USAGE)?;
                return Ok(Status::OK);
            }
            "--fresh" => fresh = true,
            "--stats" => flags.stats = true,
            "--workers" => {
                let raw = args.get(at).ok_or(CliError::Usage(WARM_TOOLCHAIN_USAGE))?;
                at += 1;
                let parsed = raw.parse::<NonZeroU32>().map_err(|_| CliError::Usage("--workers takes a positive integer"))?;
                flags.workers = Some(parsed);
            }
            "--origin" => {
                let raw = args.get(at).ok_or(CliError::Usage(WARM_TOOLCHAIN_USAGE))?;
                at += 1;
                flags.origin = Some(raw);
            }
            "--origin-key" => {
                let raw = args.get(at).ok_or(CliError::Usage(WARM_TOOLCHAIN_USAGE))?;
                at += 1;
                flags.origin_key = Some(raw);
            }
            other => {
                if let Some(raw) = other.strip_prefix("--workers=") {
                    let parsed = raw.parse::<NonZeroU32>().map_err(|_| CliError::Usage("--workers takes a positive integer"))?;
                    flags.workers = Some(parsed);
                } else {
                    return Err(CliError::Usage(WARM_TOOLCHAIN_USAGE));
                }
            }
        }
    }
    let request = request_frame()?;
    let home_var = env.and_then(|e| e.get(&VarName::new("HOME").ok()?).ok().flatten());
    let home = crate::store::Home::of(cx, fs, home_var)?;
    let rel_ws = "v2/warm-toolchain";
    write_warm_file(cx, home.dir(), "v2/warm-toolchain/Cargo.toml", WARM_WORKSPACE_TOML, &request)?;
    write_warm_file(cx, home.dir(), "v2/warm-toolchain/std_probe/Cargo.toml", WARM_STD_TOML, &request)?;
    write_warm_file(cx, home.dir(), "v2/warm-toolchain/std_probe/src/lib.rs", WARM_STD_LIB, &request)?;
    write_warm_file(cx, home.dir(), "v2/warm-toolchain/bstd_probe/Cargo.toml", WARM_BSTD_TOML, &request)?;
    write_warm_file(cx, home.dir(), "v2/warm-toolchain/bstd_probe/src/lib.rs", WARM_BSTD_LIB, &request)?;
    write_warm_file(cx, home.dir(), "v2/warm-toolchain/pm_probe/Cargo.toml", WARM_PM_TOML, &request)?;
    write_warm_file(cx, home.dir(), "v2/warm-toolchain/pm_probe/src/lib.rs", WARM_PM_LIB, &request)?;
    let ws_dir = home.dir().subdir(&Path::new(rel_ws)?);
    let abs_at = match home_var {
        Some(h) => bstd::mem::format_in!(&request, "{}/.blaze/{rel_ws}", h.trim_end_matches('/')),
        None => bstd::mem::format_in!(&request, ".blaze/{rel_ws}"),
    };
    let located = crate::run_package::Located { root: ws_dir, at: work::held_str(abs_at.as_str(), &request), member: "" };
    edge_at(cx, fs, env, net, host, out, err, phases, heap, &flags, None, Some(located), false, None, None, &request, |edge, phases| {
        let Edge { root, opts, mut shown, err, frame, .. } = edge;
        shown.quiet = true;
        let mut check_opts = opts();
        check_opts.fresh = fresh;
        let checked = check(cx, fs, host, rng, Some(args), net, root, check_opts, &mut shown, phases, out, err)?;
        let tc_hex = blaze_toolchain::PINNED.hex();
        out.line(
            cx,
            format_args!(
                "warmed toolchain {} ({}): {} of {} task(s) entered, {} record(s) put",
                &tc_hex.as_str()[..16],
                crate::graph::TRIPLE,
                checked.entered,
                checked.tasks,
                checked.puts,
            ),
        )?;
        if flags.stats {
            err.line(cx, format_args!("{}", stats_line(&checked, phases.in_main(), frame)))?;
        }
        Ok(checked.status)
    })
}

/// The command's edge, read once: the checkout, the tools, the places; then the check, or with
/// `why` its explanation from the stored answer.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one specific capability")]
fn serve(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    why: Option<&str>,
) -> Result<Status, CliError> {
    let flags = crate::flags(args)?;
    let request = request_frame()?;
    edge(cx, fs, env, net, host, out, err, phases, heap, &flags, why, &request, |edge, phases| {
        let Edge { root, opts, mut shown, err, caps, view, shown_root, frame, .. } = edge;
        let Some(target) = why else {
            let checked = match flags.fix {
                None => {
                    let line = crate::run_line::LineOpts::from_env(
                        env,
                        err,
                        flags.color,
                        blaze_screen::Command::Check,
                        view,
                        flags.status_every.is_some(),
                    );
                    let ran = crate::run_line::watched(cx, err, clock, line, |watch, bg| {
                        watch.see(blaze_screen::Step::Began(blaze_screen::Stage::Checking));
                        let live = line.live.then_some(bg);
                        let res = check_live(cx, fs, host, rng, Some(args), net, root, opts(), &mut shown, phases, out, err, live);
                        watch.see(blaze_screen::Step::Says);
                        res
                    }).map_err(CliError::RunLine)??;
                    Some(ran)
                }
                // `blaze check --fix <selector>..` (fix.rs): a quiet check, its findings' fixes
                // written as one transaction, then a second check printing what is left; the
                // listed findings are in the request's frame, which outlives both.
                Some((start, end)) => {
                    let cap = (end - start).max(1);
                    let mut selectors: blaze_own::Vec<&str, &Frame> = blaze_own::Vec::with_capacity_in(cap, frame);
                    selectors.extend((start..end).filter_map(|at| args.get(at)));
                    if selectors.is_empty() {
                        selectors.push("all");
                    }
                    let mut ask = |mut shown| check(cx, fs, host, rng, Some(args), net, root, opts(), &mut shown, phases, out, err).map(|checked| (checked, shown));
                    match crate::fix_then_check::<crate::fix::Atomic, Checked>(cx, root, &selectors, (caps, view, shown_root), frame, out, &mut ask)? {
                        crate::Fixed::Refused => None,
                        crate::Fixed::Checked(checked, _) => Some(checked),
                    }
                }
            };
            let Some(checked) = checked else { return Ok(Status::FAILED) };
            if flags.stats {
                err.line(cx, format_args!("{}", stats_line(&checked, phases.in_main(), frame)))?;
            }
            return Ok(checked.status);
        };
        crate::engine_why::explain(cx, fs, host, rng, root, &opts(), target, phases, out, err)
    })
}

/// The request's frame, never the command's fixed 64 MiB one (tempo panicked at 95.7 s,
/// g114729, with that one full): see [`REQUEST_FRAME`].
fn request_frame() -> Result<Frame, CliError> {
    Frame::with_reserve(REQUEST_FRAME, bstd::mem::Tag::Plane).map_err(|_| CliError::Text { what: "the request's frame could not be reserved (engine_check.rs `REQUEST_FRAME`)" })
}

/// The command's edge, read once (rule 75): what `check`, `why`, `run` and `test` each take of
/// it. Handed to a continuation, since the options borrow the edge's own values (the signals
/// guard, the origin, the printer's root).
pub(crate) struct Edge<'f> {
    pub(crate) root: &'f Dir<ReadWrite>,
    /// One request's options; `--fix` asks two requests of the one edge.
    pub(crate) opts: &'f dyn Fn() -> Opts<'f>,
    pub(crate) shown: print::Shown<'f>,
    pub(crate) err: &'f Out,
    pub(crate) caps: blaze_report::frame::Caps,
    pub(crate) view: blaze_report::frame::plain::View,
    pub(crate) shown_root: &'f str,
    pub(crate) frame: &'f Frame,
    pub(crate) place: Place<'f>,
    pub(crate) tools: blaze_engine::Tools,
    pub(crate) workers: NonZeroU32,
    pub(crate) producers: &'f blaze_ids::producer::Producers,
    /// The member's folder the command was given, relative to the root (run_package.rs
    /// `Located::member`).
    pub(crate) member: &'f str,
    /// The check's stop and status signals (blaze-core run_signals.rs `Asked`), `None` for `why`.
    /// Owned here, not by `edge`'s frame, so a start drops them once its check has answered:
    /// the program's run and a suite's pool take the parent's signals themselves (D103), and
    /// both refuse while this is held (`SignalError::AlreadyHeld`).
    #[cfg(unix)]
    pub(crate) signals: Option<bstd::procs::Asked>,
}

/// The edge of the command (`flags` parsed by its caller, `frame` the request's), then `then`.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
fn edge<R>(
    cx: &Cx,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    flags: &crate::Flags<'_>,
    why: Option<&str>,
    frame: &Frame,
    then: impl for<'f> FnOnce(Edge<'f>, &mut Phases) -> Result<R, CliError>,
) -> Result<R, CliError> {
    edge_at(cx, fs, env, net, host, out, err, phases, heap, flags, why, None, false, None, None, frame, then)
}

#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
fn edge_at<'l, R>(
    cx: &Cx,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    flags: &crate::Flags<'_>,
    why: Option<&str>,
    located_override: Option<crate::run_package::Located<'l>>,
    skip_signals: bool,
    daemon_requests: Option<&bstd::procs::Requests>,
    sdk_override: Option<Result<blaze_apple_sdk::Sdk<'l>, blaze_apple_sdk::SdkRefused<'l>>>,
    frame: &'l Frame,
    then: impl for<'f> FnOnce(Edge<'f>, &mut Phases) -> Result<R, CliError>,
) -> Result<R, CliError> {
    let var = |name: &str| -> Option<&str> { env?.get(&VarName::new(name).ok()?).ok().flatten() };
    // The named origin, opened as `check-old` opens it (main.rs): a miss is read from it.
    // Declared before `shown`: the request's options and what it prints share one lifetime
    // (`checking` takes `&mut Shown<'f>`), so the origin must outlive the printer's state.
    let origin = crate::store::NamedOrigin::of(cx, fs, net, flags.origin, flags.origin_key)?;
    // The open checkout's parts, each a phase (rule 17): its 22 ms on tempo (score-c1725wave42)
    // was one phase until 2026-11-20. Its floor is a few opens: one manifest read and the
    // pointer. `locate` reads one manifest where the start folder's has `[workspace]`, else the
    // layout whole (every member's manifest); the SDK is xcrun's, a process started, wherever
    // SDKROOT is unset.
    phases.mark_cx(cx, "open checkout: the origin named", frame);
    // Single-file `.rs` script (`blaze run script.rs`, `blaze check script.rs`, `blaze test script.rs`),
    // or a directory/workspace checkout walking up to its workspace root (run_package.rs `locate`, R3).
    let located = match located_override {
        Some(located) => located,
        None => match flags.dir {
            Some(raw) if why.is_none() && crate::single_file::is_script_arg(raw) => {
                crate::single_file::prepare(cx, fs, var("HOME"), raw, frame)?
            }
            _ => {
                // `why`'s first word is its target, never a checkout: it stands where blaze was started.
                let checkout = crate::checkout::checkout(if why.is_some() { None } else { flags.dir }, frame)?;
                crate::run_package::locate(cx, fs, &checkout, frame)?
            }
        },
    };
    phases.mark_cx(cx, "open checkout: the workspace's root (run_package.rs `locate`: one manifest at a `[workspace]` root, else the layout whole)", frame);
    let (checkout_at, member): (&str, &str) = (located.at, located.member);
    let root = located.root;
    let cores = u32::try_from(host.cores().get()).unwrap_or(u32::MAX);
    let workers = flags.workers.unwrap_or(NonZeroU32::new(cores).unwrap_or(NonZeroU32::MIN));
    let producers = crate::producers::table(host.build_id()?.as_bytes());
    let tools = crate::engine_store::tools(&crate::home::tools(&producers, crate::lints::Lints::NONE.rules));
    phases.mark_cx(cx, "open checkout: the host's facts and the tools' producers", frame);
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
    let sdk = match sdk_override {
        Some(sdk) => {
            phases.mark_cx(cx, "open checkout: cached macOS SDK", frame);
            sdk
        }
        None => {
            let (sdk, sdk_how) = crate::sdk::resolve_how(
                cx,
                &fs,
                var("SDKROOT"),
                &crate::sdk::XcrunEnv { path: var("PATH"), developer_dir: var("DEVELOPER_DIR"), home: var("HOME"), toolchains: var("TOOLCHAINS") },
                crate::sdk::SHIPPED,
                phases.clock(),
                frame,
            );
            phases.mark_cx(cx, sdk_how.phase(), frame);
            sdk
        }
    };
    // The checkout's absolute root: the one resolution of the start folder a command makes
    // (one realpath), never `PWD` (rule 75: `env -i` leaves it unset, a shell can leave it
    // stale). Every served read takes it from `place`.
    let served = crate::engine_script_reads::Served::at_edge(cx.is_simulated(), checkout_at, frame).map_err(EngineCheckError::ServedRoot)?;
    phases.mark_cx(cx, "open checkout: the checkout's root resolved (one realpath)", frame);
    // cargo's `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` for this machine's target (rustflags.rs).
    let target_rustflags_var = blaze_manifest::rustflags::target_variable(crate::graph::TRIPLE, heap.alloc());
    let max_store_bytes = crate::gc::budget_from_env(env)?;
    let place = Place {
        checkout: served.root_text().unwrap_or(checkout_at),
        served,
        cert_file: var("SSL_CERT_FILE"),
        cert_dir: var("SSL_CERT_DIR"),
        rustflags: blaze_manifest::rustflags::RustflagsEnv {
            encoded: var("CARGO_ENCODED_RUSTFLAGS"),
            plain: var("RUSTFLAGS"),
            build: var("CARGO_BUILD_RUSTFLAGS"),
            target: var(target_rustflags_var.as_str()),
        },
        home: var("HOME"),
        max_store_bytes,
        cargo_home: cargo_home.as_ref().map(|cargo| cargo.as_str()),
        start: served.root_text(),
        sdk,
        exe_dir: exe_dir.as_ref().map(|dir| dir.as_str()).map_err(|why| *why),
        lan_peer: crate::fetch::LanPeerConfig::from_env(var("BLAZE_NO_LAN_PEER")),
    };
    let terminal = out.is_terminal();
    let caps = blaze_report::frame::detect(&var, terminal, out.size().map(|size| size.cols), flags.color);
    let view = blaze_report::frame::plain::view(flags.view, terminal);
    // The checkout's root as its edge resolved it (`Served::at_edge`), never `PWD` (rule 75):
    // a finding's link is this root joined with its checkout-relative path. A simulated world
    // has no root on disk: the folder as given.
    let shown_root = match place.served.root_text() {
        Some(root) => bstd::mem::format_in!(frame, "{root}"),
        None => bstd::mem::format_in!(frame, "{}", flags.dir.unwrap_or(".")),
    };
    let mut shown = print::Shown::new(caps, view, shown_root.as_str(), frame);
    shown.each = flags.each;
    // `check` owns stdout for the request, so a standing answer may transfer its persisted
    // rendered bytes in one write.  The first quiet pass of `--fix` remains ineligible through
    // `Shown::ready_to_replay` and therefore never publishes a view it did not print.
    shown.output_only = true;
    phases.mark_cx(cx, "open checkout: the environment and the printer", frame);
    // The check's stop and status signals, caught until it answers (engine_status.rs);
    // `why` answers from the stored answer at once, and keeps the default ones.
    #[cfg(unix)]
    let asked = match (why, skip_signals) {
        (None, false) => Some(bstd::procs::Asked::hold().map_err(EngineCheckError::Signals)?),
        _ => None,
    };
    #[cfg(unix)]
    let requests = match asked.as_ref() {
        Some(a) => Some(a.requests()),
        None => daemon_requests,
    };
    #[cfg(not(unix))]
    let requests = daemon_requests;
    let status = crate::engine_status::Asking {
        requests,
        every: flags.status_every.map(|seconds| core::time::Duration::from_secs(u64::from(seconds.get()))),
        plant: crate::engine_status::Plant::None,
    };
    // One request's options; `--fix` asks two requests of the one edge.
    let opts = || Opts {
        status,
        tools,
        workers,
        toolchain: blaze_toolchain::PINNED,
        heap,
        place,
        fresh: false,
        heads_from: crate::engine_seeds::HeadsFrom::Frozen,
        head_units: crate::engine_heads::HeadUnits::Each,
        head_jobs: crate::engine_heads::HeadJobs::RULE,
        head_sources: crate::engine_heads_once::Sources::Job,
        frame,
        scope_reads: crate::engine_scope_reads::ScopeReadsAs::DEFAULT,
        unreadable: None,
        body_locals: blaze_ladder::front2::tables::seeds::BodyLocals::Lowered,
        origin: origin.as_ref(),
        names_oracle: flags.names_oracle,
        driver: flags.driver,
        ready_plant: crate::engine_ready_edit::CommitPlant::None,
    };
    // `requests` is `&'static` (the handlers' one board), so the guard moves into the edge.
    let edge = Edge {
        root: &root, opts: &opts, shown, err, caps, view, shown_root: shown_root.as_str(), frame, place, tools, workers, producers: &producers, member,
        #[cfg(unix)]
        signals: asked,
    };
    then(edge, phases)
}

/// What a start reads after its check (section R, "THE CONTRACT OF E3"): the answer the check
/// just stored, read back in place from the store, and the edge's handles the start needs. The
/// gate is the call: nothing here refuses a body; the parent refuses a body that is not
/// `Clean` when it is called (run_parent.rs `ParentError::NotChecked`).
pub(crate) struct Standing<'r, 'm> {
    /// The answer's root: the certificate of the start list and of the run tables.
    pub(crate) root: blaze_ids::ContentId,
    /// The command's store, read in place (`'m` its mapping's borrow); its puts are committed
    /// once, after the start ([`check_for_start`]).
    pub(crate) store: &'r mut dyn crate::store::Store<'m>,
    /// The command's lent store, which the program's child reads (own_process.rs).
    pub(crate) lent: &'r crate::store::CommandLent,
    /// The stored rows (`Stored::answer`) and where each row's record is: E2's answer is made
    /// from them over [`Standing::store`] by [`Standing::answer`].
    rows: &'r blaze_engine::Answer<'r>,
    records: &'r dyn crate::engine_run_answer::Records,
    /// The unit graph's row and each crate's names row, as the run's names reader takes them.
    pub(crate) graph: blaze_drive::Input,
    pub(crate) names: &'r dyn Fn(Name) -> Option<blaze_drive::Input>,
    /// How many errors the check printed (the summary line is already out).
    pub(crate) errors: u32,
    /// The check's exit status: what `run` and `test` say when nothing starts.
    pub(crate) status: Status,
    /// The edge's handles: where the command runs and with what.
    pub(crate) checkout: &'r Dir<ReadWrite>,
    pub(crate) place: Place<'r>,
    pub(crate) toolchain: blaze_toolchain::ToolchainId,
    pub(crate) tools: [blaze_drive::Tool; blaze_drive::key::KINDS],
    pub(crate) producers: &'r blaze_ids::producer::Producers,
    pub(crate) workers: NonZeroU32,
    pub(crate) err: &'r Out,
    pub(crate) heap: &'static blaze_own::Heap,
    pub(crate) frame: &'r Frame,
    /// What the program's native image needs beyond the records (engine_native.rs, R2).
    pub(crate) native: crate::engine_native::NativeAsk<'r>,
    /// The member's folder the command was given, relative to the root (run_package.rs
    /// `Select::member`): what `run` and `test` select when no `-p` is given.
    pub(crate) member: &'r str,
}

impl<'r, 'm> Standing<'r, 'm> {
    /// E2's answer (engine_run_answer.rs) over the stored rows and the store: what run_ids.rs
    /// walks for the body ids and reads each body's facts from.
    pub(crate) fn answer(&mut self) -> crate::engine_run_answer::EngineAnswer<'_, 'r, '_, 'm> {
        crate::engine_run_answer::EngineAnswer::new(self.rows, self.records, &mut *self.store)
    }

    /// The stored rows themselves, for as long as the standing answer ('r, not a borrow of
    /// `self`): run_ids.rs reads a workspace body's reads from them BY NEED at the call
    /// (`BodyReads`), while the store is lent to the names reader and the parent.
    pub(crate) fn rows(&self) -> &'r blaze_engine::Answer<'r> {
        self.rows
    }

    /// Where each stored row's record is, for as long as the standing answer: the run's typing
    /// finds a body's run facts record through it (engine_body_record.rs `StoreTyped`), while
    /// the store is lent to the names reader and the parent.
    pub(crate) fn records(&self) -> &'r dyn crate::engine_run_answer::Records {
        self.records
    }
}

/// The engine's check of the checkout, its answer printed to `out` (stderr for `run`, whose
/// stdout is its program's), then `start` over what it stored ([`Standing`]); the store's one
/// commit after the start returns. `flags` are the command's, parsed by its caller (main.rs:
/// `test`'s `-p` packages are collected there).
///
/// The contract's shape returns `Standing`; it is handed to `start` instead, because the store
/// borrows the lent store opened here and the answer borrows both (a returned value would
/// refer to itself). COST: the check's, then one pointer get, one root get and the snapshot.
/// What `blaze ci` reads after the engine's check completes: the placed findings (`shown`),
/// the check summary (`checked`), and optionally `standing` (present whenever `UnitGraph`
/// succeeded, so `blaze test` runs over the exact same check answer without checking twice).
pub(crate) struct CiContext<'r, 'm, 'f> {
    pub(crate) checkout: &'r Dir<ReadWrite>,
    pub(crate) shown: &'r print::Shown<'f>,
    pub(crate) checked: &'r Checked,
    pub(crate) standing: Option<Standing<'r, 'm>>,
    pub(crate) fixes_applied: u32,
    pub(crate) files_fixed: u32,
    pub(crate) frame: &'f Frame,
    pub(crate) member: &'f str,
}

/// How [`check_and_open`] configures the check and standing answer for its caller.
enum OpenMode {
    Start { quiet_when_clean: bool },
    Ci { apply_fixes: bool },
}

/// The engine's check of the checkout, its answer printed to `out` (stderr for `run`, whose
/// stdout is its program's), then `start` over what it stored ([`Standing`]); the store's one
/// commit after the start returns. `flags` are the command's, parsed by its caller (main.rs:
/// `test`'s `-p` packages are collected there).
///
/// The contract's shape returns `Standing`; it is handed to `start` instead, because the store
/// borrows the lent store opened here and the answer borrows both (a returned value would
/// refer to itself). COST: the check's, then one pointer get, one root get and the snapshot.
#[expect(clippy::too_many_arguments, reason = "the start's edge: each argument one specific capability")]
pub(crate) fn check_for_start<R>(
    cx: &Cx,
    args: Option<&bstd::args::Args>,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    flags: &crate::Flags<'_>,
    quiet_when_clean: bool,
    live: Option<&(dyn blaze_screen::Watch + Sync)>,
    start: impl for<'r, 'm> FnOnce(Standing<'r, 'm>, &mut Phases, &mut bstd::rng::Rng) -> Result<R, CliError>,
) -> Result<R, CliError> {
    check_and_open(
        cx, args, fs, env, net, host, rng, out, err, phases, heap, flags,
        OpenMode::Start { quiet_when_clean },
        live,
        |ci, phases, rng| {
            let standing = ci.standing.ok_or(EngineCheckError::StartNoGraph)?;
            start(standing, phases, rng)
        },
    )
}

/// Run the engine check for `blaze ci` (quiet, structured `Shown::listed` populated even on
/// certificate hits, optional safe `--fix` pre-pass), then hand [`CiContext`] to `then` with
/// `Standing` ready for `blaze test` and commit the store once afterward.
#[expect(clippy::too_many_arguments, reason = "the CI command's edge: each argument one specific capability")]
pub(crate) fn check_for_ci<R>(
    cx: &Cx,
    args: Option<&bstd::args::Args>,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    flags: &crate::Flags<'_>,
    apply_fixes: bool,
    then: impl for<'r, 'm, 'f> FnOnce(CiContext<'r, 'm, 'f>, &mut Phases, &mut bstd::rng::Rng) -> Result<R, CliError>,
) -> Result<R, CliError> {
    check_and_open(
        cx, args, fs, env, net, host, rng, out, err, phases, heap, flags,
        OpenMode::Ci { apply_fixes },
        None,
        then,
    )
}

/// Shared implementation of [`check_for_start`] and [`check_for_ci`] (rule 30).
#[expect(clippy::too_many_arguments, reason = "the start/CI edge: each argument one specific capability")]
fn check_and_open<R>(
    cx: &Cx,
    args: Option<&bstd::args::Args>,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    flags: &crate::Flags<'_>,
    mode: OpenMode,
    live: Option<&(dyn blaze_screen::Watch + Sync)>,
    then: impl for<'r, 'm, 'f> FnOnce(CiContext<'r, 'm, 'f>, &mut Phases, &mut bstd::rng::Rng) -> Result<R, CliError>,
) -> Result<R, CliError> {
    let request = request_frame()?;
    edge(cx, fs, env, net, host, out, err, phases, heap, flags, None, &request, |edge, phases| {
        #[cfg(unix)]
        let Edge { root, opts, mut shown, err, caps, shown_root, frame, place, workers, producers, member, signals, .. } = edge;
        #[cfg(not(unix))]
        let Edge { root, opts, mut shown, err, caps, shown_root, frame, place, workers, producers, member, .. } = edge;
        let mut fixes_applied: u32 = 0;
        let mut files_fixed: u32 = 0;
        let require_graph = match mode {
            OpenMode::Start { quiet_when_clean } => {
                shown.quiet_when_clean = quiet_when_clean;
                true
            }
            OpenMode::Ci { apply_fixes } => {
                if apply_fixes {
                    let mut first = print::Shown::new(caps, blaze_report::frame::plain::View::Agent, shown_root, frame);
                    first.quiet = true;
                    first.output_only = false;
                    let _ = check_live(cx, fs, host, rng, args, net, root, opts(), &mut first, phases, out, err, None)?;
                    if first.listed.iter().any(|l| l.fix.is_some())
                        && let Ok(applied) = crate::fix::apply(cx, root, &crate::fix::Atomic, &first.listed, &["all"], frame)?
                    {
                        fixes_applied = u32::try_from(applied.fixed.len()).unwrap_or(u32::MAX);
                        files_fixed = u32::try_from(applied.files).unwrap_or(u32::MAX);
                    }
                }
                shown.view = blaze_report::frame::plain::View::Agent;
                shown.quiet = true;
                shown.output_only = false;
                false
            }
        };
        let opts = opts();
        let toolchain = opts.toolchain;
        let checked = check_live(cx, fs, host, rng, args, net, root, opts, &mut shown, phases, out, err, live)?;
        // The check has answered: its signals go back before the start, which takes the
        // parent's own (D103: run_loop.rs `Held::hold`; a suite's pool, test_drive.rs
        // `Signals`, once for the suite). Held into the start, the first run is refused.
        #[cfg(unix)]
        drop(signals);
        let errors = shown.counts.errors;
        // The answer the check stored, read back from a store opened anew: the check committed
        // and closed its own (`checking`), so every record it put is in a segment, read in place.
        // No origin: the check made every record it read local.
        let opened = crate::store::open_for_command(cx, fs, host, rng, place.home, place.max_store_bytes, root, place.checkout, phases.clock(), err, heap)?;
        let erased = blaze_own::Erased::new(frame);
        let mut store = crate::store::CommandStore::new(cx, &opened.lent, false, opened.open_ns, None, phases.clock().clone(), err, erased.handle())?;
        let request_mem = crate::engine_phase_mem::RequestMem::new()?;
        let alloc = request_mem.shared();
        let answer_root = crate::engine_answer::root_name(place.checkout, frame);
        let pointer = match crate::engine_answer::load_pointer(cx, &mut store, &answer_root)? {
            Some(Ok(pointer)) => Some(pointer),
            Some(Err(why)) if require_graph => return Err(EngineCheckError::StartNoAnswer { why: Some(why) }.into()),
            None if require_graph => return Err(EngineCheckError::StartNoAnswer { why: None }.into()),
            _ => None,
        };
        let stood_root = match &pointer {
            Some(pointer) => match crate::engine_answer::open_root(cx, &mut store, pointer.root, alloc)? {
                Ok(stood_root) => Some(stood_root),
                Err(why) if require_graph => return Err(EngineCheckError::StartNoAnswer { why: Some(why) }.into()),
                Err(_) => None,
            },
            None => None,
        };
        let snapshot = if stood_root.is_some() { Some(store.snapshot_in(alloc)?) } else { None };
        let stored = match (&pointer, &stood_root, &snapshot) {
            (Some(pointer), Some(stood_root), Some(snapshot)) => {
                Some(crate::engine_answer::Stored::new(stood_root, snapshot, &pointer.headers, blaze_engine::pages::Check::Store, alloc))
            }
            _ => None,
        };
        let rows = stored.as_ref().map(|s| s.answer());
        // A row's input: its record (the page header's) and its out (the row's), `None` for a
        // row that failed or has no record. A refused page is kept and stops the command after
        // the start (rule 3), never read as "no row".
        let refused: core::cell::Cell<Option<(Name, &'static str)>> = core::cell::Cell::new(None);
        let input = |name: Name| -> Option<blaze_drive::Input> {
            let records: &dyn crate::engine_run_answer::Records = stored.as_ref()?;
            let rows_ref = rows.as_ref()?;
            let at = match records.record(name) {
                Ok(at) => at?,
                Err(why) => {
                    refused.set(Some((name, why)));
                    return None;
                }
            };
            match rows_ref.row_facts(name) {
                Ok(Some(row)) if !row.failed => Some(blaze_drive::Input { name, at, out: row.out }),
                Ok(_) => None,
                Err(_) => {
                    refused.set(Some((name, "a page of the stored answer is refused")));
                    None
                }
            }
        };
        let graph = input(Name::unit_graph());
        if require_graph && let Some((name, why)) = refused.get() {
            return Err(EngineCheckError::StartPage { name, why }.into());
        }
        let standing = match (pointer.as_ref(), rows.as_ref(), stored.as_ref(), graph) {
            (Some(pointer), Some(rows_ref), Some(stored_ref), Some(graph)) if refused.get().is_none() => {
                let records: &dyn crate::engine_run_answer::Records = stored_ref;
                Some(Standing {
                    root: pointer.root.0,
                    store: &mut store,
                    lent: &opened.lent,
                    rows: rows_ref,
                    records,
                    graph,
                    names: &input,
                    errors,
                    status: checked.status,
                    checkout: root,
                    place,
                    toolchain,
                    tools: crate::home::tools(producers, crate::lints::Lints::NONE.rules),
                    producers,
                    workers,
                    err,
                    heap,
                    frame,
                    native: crate::engine_native::NativeAsk { producer: producers.get::<blaze_ids::producer::Whole>(), home: place.home, words: frame, fs: fs.clone() },
                    member,
                })
            }
            _ => None,
        };
        let ci_ctx = CiContext {
            checkout: root,
            shown: &shown,
            checked: &checked,
            standing,
            fixes_applied,
            files_fixed,
            frame,
            member,
        };
        let started = then(ci_ctx, phases, rng);
        if require_graph && let Some((name, why)) = refused.get() {
            return Err(EngineCheckError::StartPage { name, why }.into());
        }
        let started = started?;
        // The store's one commit, after the start: the run tables and the native image it put.
        crate::store::Store::after_answer(&mut store, cx, &mut blaze_drive::profile::Profile::default())?;
        drop(rows);
        drop(stored);
        drop(snapshot);
        drop(store);
        crate::store::after_answer_printed(cx, opened.lent, &opened.home, false, phases.clock(), err)?;
        Ok(started)
    })
}

/// `--stats`'s line on stderr, from what the check counted: "blaze: E of T task(s) entered, P
/// record(s) put, G page(s) written, certificate stood|missed; M ms in main;
/// .blaze/v2/profile.txt". The profile holds the rest.
pub(crate) fn stats_line<'f>(checked: &Checked, in_main_ns: u64, frame: &'f Frame) -> bstd::mem::String<&'f Frame> {
    let certificate = if checked.certified { "stood" } else { "missed" };
    let ms = in_main_ns / 1_000_000;
    let micros = (in_main_ns / 1_000) % 1_000;
    bstd::mem::format_in!(
        frame,
        "blaze: {} of {} task(s) entered, {} record(s) put, {} page(s) written, certificate {certificate}; {ms}.{micros:03} ms in main; {}",
        checked.entered,
        checked.tasks,
        checked.puts,
        checked.pages_written,
        print::PROFILE,
    )
}

/// Where the command runs and what the fetches and scripts read, each given by the edge (rule
/// 75; main.rs `request`'s first argument). Copied, never re-read: `--fix`'s two checks take one.
#[derive(Clone, Copy)]
pub(crate) struct Place<'f> {
    /// The checkout, relative to where blaze was started ("" for there).
    pub(crate) checkout: &'f str,
    /// The checkout's absolute root, resolved once at the edge (`Served::at_edge`).
    pub(crate) served: crate::engine_script_reads::Served<'f>,
    pub(crate) cert_file: Option<&'f str>,
    pub(crate) cert_dir: Option<&'f str>,
    /// The rustflags variables (fetch.rs `FetchEnv::rustflags`).
    pub(crate) rustflags: blaze_manifest::rustflags::RustflagsEnv<'f>,
    /// `HOME`: the store and the sources are beneath it.
    pub(crate) home: Option<&'f str>,
    /// `BLAZE_STORE_MAX_BYTES` / `BLAZE_STORE_MAX_GB` parsed once at the CLI edge.
    pub(crate) max_store_bytes: Option<u64>,
    pub(crate) cargo_home: Option<&'f str>,
    /// `PWD`, where the toolchain's shelf is.
    pub(crate) start: Option<&'f str>,
    pub(crate) sdk: Result<blaze_apple_sdk::Sdk<'f>, blaze_apple_sdk::SdkRefused<'f>>,
    pub(crate) exe_dir: Result<&'f str, crate::fetch::ExeDirWhy>,
    pub(crate) lan_peer: crate::fetch::LanPeerConfig,
}

impl<'f> Place<'f> {
    /// The fetch environment over this place (fetch.rs): the check's stages', and the start's
    /// names readers' (engine_start.rs), made the one way.
    pub(crate) fn fetch_env(&self, clock: bstd::clock::Clock, toolchain: blaze_toolchain::ToolchainId, fs: bstd::fs::Fs, heap: &'static blaze_own::Heap) -> FetchEnv<'f> {
        FetchEnv {
            clock,
            checkout: self.checkout,
            served: self.served,
            cert_file: self.cert_file,
            cert_dir: self.cert_dir,
            rustflags: self.rustflags,
            home: self.home,
            cargo_home: self.cargo_home,
            start: self.start,
            toolchain,
            fs,
            sdk: self.sdk,
            exe_dir: self.exe_dir,
            lan_peer: self.lan_peer,
            // PHASE-MEM-DEBT(Units): the fetches' and every stage's `handles.env.heap` (fetch.rs
            // `FetchEnv::heap`, `&'static Heap`): the stages put what they make on the process
            // heap through it; each becomes the phase's handle when its stage takes one (`kept
            // past the phase` names which).
            heap,
        }
    }
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
    /// Answer in full: the stored answer is not read (one is still written).
    pub(crate) fresh: bool,
    /// Where the bodies take their heads from (engine_seeds.rs `HeadsFrom`): the command's is
    /// `Frozen`; a test sets `Lowered` to compare the two (rule 78).
    pub(crate) heads_from: crate::engine_seeds::HeadsFrom,
    /// Under which units a head task freezes (engine_heads.rs `HeadUnits`): the command's is
    /// `Each`; a test plants one unit alone (rule 23).
    pub(crate) head_units: crate::engine_heads::HeadUnits,
    /// How a crate's head tasks are split into pool jobs (engine_heads.rs `HeadJobs`, lane
    /// OW1): the command's is the rule; a test sets a count (rule 78).
    pub(crate) head_jobs: crate::engine_heads::HeadJobs,
    /// What a head task freezes its heads over (engine_heads_once.rs `Sources`): the command's
    /// is `Job`; a test sets `Head` for the measure before the table (rule 78).
    pub(crate) head_sources: crate::engine_heads_once::Sources,
    /// The request's frame (its caller's, at most [`REQUEST_FRAME`]): every stage's lists and
    /// records whose size follows the project (crates, files, bodies, findings, environments,
    /// labels) are in it, never in the command's fixed frame.
    pub(crate) frame: &'f Frame,
    /// How a body's names reads are kept (engine_scope_reads.rs, rule 78): `Whole` until
    /// `Cells` is admitted on the acceptance projects.
    pub(crate) scope_reads: crate::engine_scope_reads::ScopeReadsAs,
    /// The crate (by its name) whose records a check scope reads as unreadable
    /// (engine_records.rs `EngineRecords::scope_records`): a test plants one (rule 23); the
    /// command's is `None`.
    pub(crate) unreadable: Option<&'static str>,
    /// Where a body's need of an item declared in that body is made (front2 tables/seeds.rs
    /// `BodyLocals`): the command's is `Lowered`, in the body's own store; a test plants
    /// `AskTheRequest` (rule 23, engine_body_locals_tests.rs).
    pub(crate) body_locals: blaze_ladder::front2::tables::seeds::BodyLocals,
    /// What may ask the check for its status or stop it (engine_status.rs): the command's
    /// signals and `--status-every`; a test's plant; `Asking::NONE` in the other tests.
    pub(crate) status: crate::engine_status::Asking<'f>,
    /// The named origin a miss is read from (`--origin <folder or http base> --origin-key
    /// <hex>`, store.rs `NamedOrigin`), as `check-old` opens its store; `None`: the home alone.
    pub(crate) origin: Option<&'f crate::store::NamedOrigin>,
    /// `--names-oracle` (engine_names.rs `NamesOracle`, D158 step 1): every crate's names
    /// decided twice and compared, answered in full (as `fresh`) so every crate is entered; a
    /// disagreement fails the command. The command's default is `Off`.
    pub(crate) names_oracle: crate::engine_names::NamesOracle,
    /// `--driver=ready|phases` (engine_ready_edit.rs `Driver`, D187): which driver runs an edit.
    /// The command's default is `Phases`; under `Ready` an edit the ready path cannot serve stops
    /// the request, naming why (`EngineCheckError::ReadyRefused`), never runs the phases.
    pub(crate) driver: crate::engine_ready_edit::Driver,
    /// The ready commit's planted bug (engine_ready_edit.rs `CommitPlant`, rule 23): the
    /// command's is `None`; the world test plants a dropped cause to see its oracle catch it.
    pub(crate) ready_plant: crate::engine_ready_edit::CommitPlant,
}

/// The most one request's frame takes: its blocks are taken as it is used (blaze-mem
/// frame.rs `with_reserve`), so this is a cap, not a reservation. The command's own frame is
/// a fixed 64 MiB, which tempo's names alone filled (66,040,983 of 67,108,864 bytes, a
/// names-only run) before a crate environment's pairs were refused at 95.7 s (g114729): the
/// stages' lists grow with the project, so their frame is sized by the machine's memory, whose
/// limit is the kernel's (rule 50).
pub(crate) const REQUEST_FRAME: usize = 64 << 30;

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
    /// Each body's note in name order, and the bodies checked with no finding
    /// (engine_bodies.rs `notes`).
    pub(crate) bodies: (std::vec::Vec<std::string::String, std::alloc::Global>, u32),
    /// Each phase's time, in phase order (engine_times.rs).
    pub(crate) times: std::vec::Vec<crate::engine_times::PhaseTime, std::alloc::Global>,
    /// The certificate stood: nothing the stored answer read moved, no task entered.
    pub(crate) certified: bool,
    /// Whether the request started from the stored answer, and why not when one was there.
    pub(crate) from_last: bool,
    pub(crate) unreadable: Option<crate::engine_answer::Unreadable>,
    /// Rule 21's witness over the answer: each read (the reader, the cell) of a record no task
    /// has a row for, a read nothing can ever move (8 kept, all counted); then the count of
    /// such reads of a head's `Signature` cell, which no task defines yet (lane EV widens them).
    #[cfg(test)]
    pub(crate) undefined_reads: UndefinedReads,
    /// The Heads stage's rows in the answer (`Signature` and `ImplBucket`, entered or standing),
    /// the head tasks refused, and the crates whose tables standing rows put on the shelf from
    /// the store (engine_heads.rs `complete`).
    #[cfg(test)]
    pub(crate) heads: (u32, u32, u32),
    /// The Lints stage's rows in the answer, entered or standing: one a linted file, however
    /// many units it has (engine_lints.rs, sparse rows). 0 on a certified replay (no audit).
    #[cfg(test)]
    pub(crate) lint_rows: u32,
    /// The `Signature` head tasks entered this request (lane RH4's edit rows count them).
    pub(crate) signatures_entered: u32,
    /// The `Lint` tasks entered this request and their findings (engine_lints.rs `seen`).
    pub(crate) lints: crate::engine_lints::LintsSeen,
    /// The bodies in files no member crate's module tree reaches (engine_bodies.rs
    /// `BodiesStage::ownerless`), and the first such files' paths: 0 on every project.
    pub(crate) ownerless: (u32, std::vec::Vec<std::string::String, std::alloc::Global>),
    /// The planner's file counts (`plan`).
    pub(crate) files: Files,
    /// The units stage: the graph read back or discovered, and the fetches run
    /// (engine_units.rs `discover`).
    pub(crate) units: crate::engine_units::Units,
    /// The plan's reads of the stored answer, its stats and bytes written (the profile's `plan:`
    /// line, engine_units.rs `PlanRows`).
    pub(crate) plan: crate::engine_units::PlanRows,
    /// The stored answer's pages this request wrote (engine_answer.rs `save`).
    pub(crate) pages_written: u32,
    /// What each stage's records cost this process, and the digest of every record put
    /// (engine_store.rs `Ledger`).
    pub(crate) records: crate::engine_store::Ledger,
    /// What the sweep cost: the journal's use, its events, the rows statted of the leaf table
    /// (journal_sweep.rs; the profile's `sweep:` line).
    pub(crate) sweep: crate::journal_sweep::SweepCounts,
    /// The request's frozen heads: the table, the dependency heads read, found and frozen, and
    /// the bodies seeded and refused (engine_seeds.rs `SeedCounts`).
    pub(crate) seeds: crate::engine_seeds::SeedCounts,
    /// Each stage's phases' arenas (their most, what their drops gave back) and the heap kept
    /// past each phase, and the arenas made and dropped (engine_phase_mem.rs).
    pub(crate) phase_mem: crate::engine_phase_mem::PhaseLedger,
    /// `--names-oracle`'s sums (engine_names.rs `OracleTotals`); `None` without it.
    pub(crate) names_oracle: Option<crate::engine_names::OracleTotals>,
    /// The stored answer this request wrote and what it read of the last (the world test's
    /// oracle compares two drivers' by it, engine_ready_edit_tests.rs); `None`: no answer was
    /// written (the certificate stood).
    pub(crate) stored: Option<crate::engine_answer::StoredKeys>,
    /// The bodies' setup: the members taken from the members record unvisited and the members
    /// visited (engine_members_record.rs; the profile's `bodies: world opened by need` line).
    pub(crate) members_stood: (u32, u32),
    /// The visited members' listing rows: read by key, made from the whole listing, damaged
    /// (engine_member_rows.rs `RowCounts`; the profile's `member rows read N, made M` words).
    pub(crate) member_rows: crate::engine_member_rows::RowCounts,
}

/// The check of the checkout at `root`, printed through `shown` to `out`; the store's words on
/// `err`.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
pub(crate) fn check<'f>(
    cx: &'f Cx,
    fs: &bstd::fs::Fs,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    args: Option<&bstd::args::Args>,
    net: Option<&bstd::net::Net>,
    root: &Dir<ReadWrite>,
    opts: Opts<'f>,
    shown: &mut print::Shown<'f>,
    phases: &mut Phases,
    out: &Out,
    err: &Out,
) -> Result<Checked, CliError> {
    check_live(cx, fs, host, rng, args, net, root, opts, shown, phases, out, err, None)
}

#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
fn check_live<'f>(
    cx: &'f Cx,
    fs: &bstd::fs::Fs,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    args: Option<&bstd::args::Args>,
    net: Option<&bstd::net::Net>,
    root: &Dir<ReadWrite>,
    opts: Opts<'f>,
    shown: &mut print::Shown<'f>,
    phases: &mut Phases,
    out: &Out,
    err: &Out,
    live: Option<&(dyn blaze_screen::Watch + Sync)>,
) -> Result<Checked, CliError> {
    check_live_with_daemon(cx, fs, host, rng, args, net, root, opts, shown, phases, out, err, live, None)
}

#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
fn check_live_with_daemon<'f, 'd>(
    cx: &'f Cx,
    fs: &bstd::fs::Fs,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    args: Option<&bstd::args::Args>,
    net: Option<&bstd::net::Net>,
    root: &Dir<ReadWrite>,
    opts: Opts<'f>,
    shown: &mut print::Shown<'f>,
    phases: &mut Phases,
    out: &Out,
    err: &Out,
    live: Option<&(dyn blaze_screen::Watch + Sync)>,
    daemon: Option<&mut DaemonCheckCtx<'d>>,
) -> Result<Checked, CliError> {
    #[cfg(test)]
    stack_probe::at("check");
    // Where the check stands, said when asked or stopped (engine_status.rs), the watcher
    // beside it when anything can ask.
    let asking = opts.status;
    // Boxed, made in its own call ([`board_in`]): by value its task slots sat twice in this
    // frame (~79 KiB, bt4 2026-11-20) under every stage's (rule 54).
    let board = board_in(opts.heap, &asking, phases.now().since_start());
    let board: &crate::engine_status::Board<'_> = &board;
    let clock = phases.clock().clone();
    let heap = opts.heap;
    let mut active = None;
    let stop_host = daemon.is_none().then_some(host);
    let result = work::unwinding(|| {
        crate::engine_status::watched(cx, stop_host, root, err, &clock, board, asking.every, live, || {
            checking(cx, fs, host, rng, args, net, root, opts, shown, phases, out, err, board, &mut active, live, daemon)
        }).map_err(EngineCheckError::Watcher)?
    });
    let error = match result {
        // Unboxed once, here ([`BoxedChecked`]): the caller's return slot is the one copy.
        Ok(Ok(checked)) => return Ok(blaze_own::Box::into_inner(checked)),
        // The signal handler already published the stopped status.
        Ok(Err(error @ CliError::EngineCheck(EngineCheckError::Stopped { .. }))) => return Err(error),
        Ok(Err(error)) if active.is_none() && board.stage().is_none() => return Err(error),
        Ok(Err(error)) => error,
        Err(work::Panicked) => CliError::Panicked { command: "engine-check" },
    };
    // The scope has joined. Its live counts include the phase that failed before merge.
    // Use the caller's heap here: a full request frame can itself be the reason for the stop.
    use core::fmt::Write as _;
    let mut text = match (active, board.stage()) {
        (Some(phase), _) => bstd::mem::format_in!(heap.alloc(), "failed in phase {:?} wave {}: {error}\n", phase.stage, phase.wave),
        (None, Some(stage)) => bstd::mem::format_in!(heap.alloc(), "failed after stage {stage:?}: {error}\n"),
        (None, None) => bstd::mem::format_in!(heap.alloc(), "failed before the first stage: {error}\n"),
    };
    crate::engine_status::write_status(board, crate::engine_status::Heading::End, board.now(&clock), &mut text)
        .and_then(|()| write!(text, "{phases}"))
        .map_err(|_| CliError::Text { what: "could not format the failed engine-check profile" })?;
    // Publish before writing stderr so a closed error stream still leaves the profile.
    crate::print::ensure_checkout_ignore(cx, root)?;
    root.publish(cx, &Path::new(print::PROFILE)?, bstd::fs::Content::bytes(text.as_bytes()), bstd::fs::Durable::No)?;
    err.write_all(cx, text.as_bytes())?;
    err.flush(cx)?;
    Err(error)
}

/// The check's status board ([`crate::engine_status::Board`]) boxed on the process heap, made in
/// this call: its task slots are tens of KiB, so made in [`check`]'s own frame the value and its
/// move sat there twice for the whole command (rule 54: no command frame holds a fixed table by
/// value). A box, not the request's frame: the board's largest bodies own their labels, so its
/// drop must run.
#[inline(never)]
fn board_in<'f>(heap: &'static blaze_own::Heap, asking: &crate::engine_status::Asking<'f>, began: core::time::Duration) -> blaze_own::Box<crate::engine_status::Board<'f>, blaze_own::Alloc<'static>> {
    blaze_own::Box::new_in(crate::engine_status::Board::new(asking, began), heap.alloc())
}

/// What [`checking`] hands back through [`check`]'s wrappers (the unwind guard, the status
/// watcher and its thread scope): [`Checked`] boxed on the process heap, so each wrapper's
/// frame holds a pointer, never the 7 KiB value. By value, a debug build kept a copy of the
/// `Result` in every wrapper frame and its moves: ~199 KiB between `check` and `checking`
/// (wave-1119 t59 frames: `watched` 47 KiB, its scope 34 KiB, `check`'s closure 31 KiB,
/// `catch_unwind` 23 KiB and two `do_call`s of 16 KiB), all under every stage's frames (rule 54).
type BoxedChecked = blaze_own::Box<Checked, blaze_own::Alloc<'static>>;

/// `make`'s value boxed on the process heap, made in this call: a debug build keeps the value
/// and its move in the frame that makes it, so a stage's fixed table made inline in
/// [`checking`] sat there for the whole command (rule 54); made here, only the box's pointer
/// does. A box, not the request's frame: the value's drop must run.
#[inline(never)]
fn made_boxed<T>(heap: &'static blaze_own::Heap, make: impl FnOnce() -> T) -> blaze_own::Box<T, blaze_own::Alloc<'static>> {
    blaze_own::Box::new_in(make(), heap.alloc())
}

/// [`check`] under its board.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
fn checking<'f, 'd>(
    cx: &'f Cx,
    fs: &bstd::fs::Fs,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    args: Option<&bstd::args::Args>,
    net: Option<&bstd::net::Net>,
    root: &Dir<ReadWrite>,
    opts: Opts<'f>,
    shown: &mut print::Shown<'f>,
    phases: &mut Phases,
    out: &Out,
    err: &Out,
    board: &crate::engine_status::Board<'_>,
    active: &mut Option<Phase>,
    live: Option<&(dyn blaze_screen::Watch + Sync)>,
    daemon: Option<&mut DaemonCheckCtx<'d>>,
) -> Result<BoxedChecked, CliError> {
    #[cfg(test)]
    stack_probe::at("checking");
    let Opts { tools, workers, toolchain, heap, place, fresh, heads_from, head_units, head_jobs, head_sources, frame, scope_reads, unreadable: planted_unreadable, body_locals, status: _, origin, names_oracle, driver, ready_plant } = opts;
    // The oracle compares every crate's names, so every crate is entered: no stored answer read.
    let fresh = fresh || names_oracle.on();
    let began = phases.now();
    let erased = blaze_own::Erased::new(frame);
    // The request's arena (engine_phase_mem.rs): what this call keeps to its end and no record
    // holds, given back whole when it returns; each phase's arena is made from it. Declared
    // first, so dropped last: every value over it is a local dropped before it.
    let request_mem = crate::engine_phase_mem::RequestMem::new()?;
    let request = request_mem.shared();
    // The phases' arenas and the heap kept past each, by stage.
    let mut phase_mem = made_boxed(heap, crate::engine_phase_mem::PhaseLedger::new);
    let (daemon_opened, daemon_watch, mut daemon_hot_graph, mut daemon_out) = match daemon {
        Some(ctx) => (Some(ctx.opened), ctx.watch.take(), Some(&mut *ctx.hot_graph), Some(&mut ctx.out)),
        None => (None, None, None, None),
    };
    // The one store, opened as `check` opens it (main.rs), committed after the answer.
    // Inside the resident daemon, reuse the already-open store without reopening segments.
    let owned_opened = match daemon_opened {
        Some(_) => None,
        None => {
            let opened = crate::store::open_for_command(cx, fs, host, rng, place.home, place.max_store_bytes, root, place.checkout, phases.clock(), err, heap)?;
            Some(opened)
        }
    };
    let Some(opened) = daemon_opened.or(owned_opened.as_ref()) else {
        return Err(CliError::Text { what: "store must be opened either standalone or by daemon" });
    };
    crate::store::take::before_first_phase(cx, args, fs, net, opened, toolchain, phases, err)?;
    let mut store = crate::store::CommandStore::new(cx, &opened.lent, false, opened.open_ns, origin, phases.clock().clone(), err, erased.handle())?;
    phases.mark_cx(cx, "store open", frame);
    // 0. The stored answer (engine_answer.rs): its pointer, then the sweep of what it read.
    // Nothing moved, nothing owed and no findings kept: the certificate, from the pointer alone.
    let answer_root = crate::engine_answer::root_name(place.checkout, frame);
    let view_slot = crate::engine_lines::ViewSlot::load(&opened.lent, answer_root.as_str(), fresh, shown, &tools, frame)?;
    // The pointer and the last request's lines, in a frame of their own ([`own_frame`]).
    let (mut unreadable, pointer, last_lines, other_checkout) = own_frame(|| -> Result<_, CliError> {
        let mut unreadable: Option<crate::engine_answer::Unreadable> = None;
        let mut other_checkout = false;
        let stored = match fresh {
            true => None,
            false => match crate::engine_answer::load_pointer(cx, &mut store, &answer_root)? {
                // No answer of this checkout's own: its workspace's base (`load_base`), else the
                // pre-warmed toolchain-only base (`tool-base.<PINNED>.<TRIPLE>.<tools>`), both
                // loaded via `StoredBase::OtherCheckout`.
                None => {
                    let latest = match crate::engine_answer::load_base(cx, &mut store, root, frame)? {
                        Some(found) => Some(found),
                        None => crate::engine_answer::load_toolchain_base(cx, &mut store, toolchain, &tools, frame)?,
                    };
                    other_checkout = latest.is_some();
                    latest
                }
                own => own,
            },
        };
        let pointer = match stored {
            None => None,
            // Held by value, never boxed: `Pointer<'m>` borrows the store's mapping (its leaves,
            // its journal place), and a `blaze_own::Box` of it would (1) give it drop glue, so its
            // drop at this function's end would use `'m` after the commit moves `opened.lent`,
            // and (2) through `Box::deref` (a call, not a built-in deref) add `'m: 'x` to every
            // read through it, putting the store's lifetime into `stored`, the start and the
            // drive's results (the compile of 2026-11-21 06:35). Its 4 KiB stays in this frame.
            Some(Ok(pointer)) => Some(pointer),
            Some(Err(why)) => {
                unreadable = Some(why);
                None
            }
        };
        phases.mark_cx(cx, "stored answer's pointer read", frame);
        // The last request's printed lines (engine_lines.rs): what the certificate prints, and each
        // standing row's block for a request that enters tasks. Unreadable: answered in full.
        let mut last_lines = None;
        if let Some(pointer) = &pointer {
            match crate::engine_answer::load_lines(cx, &mut store, pointer.lines, request)? {
                Ok(lines) => last_lines = Some(lines),
                Err(why) => unreadable = Some(why),
            }
        }
        let pointer = pointer.filter(|_| last_lines.is_some());
        phases.mark_cx(cx, "stored answer's lines read", frame);
        let other_checkout = other_checkout && pointer.is_some();
        Ok((unreadable, pointer, last_lines, other_checkout))
    })?;
    // The planted control (engine_answer.rs `RootPlant::PerTools`): an answer under other tools
    // is not read, as when the root's name held the tools.
    #[cfg(test)]
    let (pointer, last_lines) = match crate::engine_answer::ROOT_PLANT.with(core::cell::Cell::get) {
        crate::engine_answer::RootPlant::PerTools if pointer.as_ref().is_some_and(|pointer| !pointer.certifies_under(&tools)) => (None, None),
        crate::engine_answer::RootPlant::PerTools | crate::engine_answer::RootPlant::None => (pointer, last_lines),
    };
    // What the stored answer is to this request: the one decision of the certificate and the
    // wake (engine_ready_edit.rs `StoredBase`). Another checkout's answer, or one under other
    // tools, is read as a base whose every row stands or is entered by its own key and reads
    // (engine_answer.rs "THE TOOLS").
    let base = match &pointer {
        Some(_) if other_checkout => crate::engine_ready_edit::StoredBase::OtherCheckout,
        Some(pointer) if !pointer.certifies_under(&tools) => crate::engine_ready_edit::StoredBase::ToolsMoved,
        _ => crate::engine_ready_edit::StoredBase::Own,
    };
    // The places to replay from: the pointer's, each made later by the checkout's journal
    // checkpoint where one extends this very pointer (journal_checkpoint.rs). Another
    // checkout's places are its journal's, never this one's: none, so every leaf is looked at.
    let chosen = match &pointer {
        Some(pointer) if base != crate::engine_ready_edit::StoredBase::OtherCheckout => crate::journal_checkpoint::read(cx, &mut store, answer_root.as_str(), pointer.key, pointer.journal, frame)?,
        _ => crate::journal_checkpoint::Chosen::none(frame),
    };
    // The journal of changes replayed from the pointer's places, BEFORE any file of the
    // checkout is read (journal_sweep.rs): a write after this and before its file's read is
    // after the place this request saves, so the next request's replay names it. Moved below
    // the sweep or the plan's reads, such a write would be missed for good.
    // Inside the resident daemon, feed the synchronous `kqueue` dirty set (or in-process
    // `HeldChanges::flush` fallback) directly into `Taken`, bypassing `journal-helper`.
    let journal = match daemon_watch {
        Some(watch) => {
            let mut taken = crate::journal_sweep::Taken::begin(cx, root, phases.clock(), frame)?;
            match watch {
                crate::daemon_watch::PollOutcome::Clean { crash_cursor } => {
                    taken.vouched_kqueue(&[], crash_cursor, false, true);
                }
                crate::daemon_watch::PollOutcome::Dirty { paths, crash_cursor, force_script_asks, facts_clean, .. } => {
                    let mut copied: blaze_own::Vec<&str, &Frame> = blaze_own::Vec::with_capacity_in(paths.len(), frame);
                    for path in paths.iter() {
                        copied.push(frame.alloc_str(path));
                    }
                    taken.vouched_kqueue(copied.into_bump_slice(), crash_cursor, force_script_asks, facts_clean);
                }
                crate::daemon_watch::PollOutcome::FullRescan { crash_cursor, .. } => {
                    taken.full_rescan_daemon(crash_cursor);
                }
                crate::daemon_watch::PollOutcome::FsEvents { changes, cursor, valid_since } => {
                    taken.in_process_fsevents(chosen.places(), valid_since, changes, cursor);
                }
            }
            taken
        }
        None => {
            use crate::journal_sweep::ChangedSince as _;
            let me = crate::journal_ask::Me::of(&host.build_id()?, place.served.root_text(), place.home, frame);
            let exe = (!cfg!(test) && !cx.is_simulated()).then(|| host.exe().ok()).flatten();
            let start = crate::journal_ask::HelperStart::detached(host, exe.as_deref(), place.served.root_text(), place.home);
            let strategy = crate::journal_ask::Shipped { me, start };
            strategy.changed_since(cx, root, opened.home.dir(), phases.clock(), chosen.places(), frame)?
        }
    };
    let now_places = crate::journal_checkpoint::Places::of_taken(&journal, frame);
    // The lint step and rules the root manifest names, as `check` reads them (lints.rs
    // `of_root`): the level each lint finding prints at, and what the Lints stage runs. A read
    // of the checkout, so AFTER the journal's place is taken (above; journal_sweep.rs): read
    // before it (as it was until 2026-11-20), an edit of the root manifest between the two was
    // linted under its old rules and saved under a place after the edit, a stale answer no
    // later request would enter. The end state is a type: only a `Checkout<Journaled>` reads.
    let linting = crate::lints::of_root(cx, root, frame)?;
    shown.lints = linting.step;
    let answer_places = pointer.as_ref().map_or(crate::journal_sweep::Stored::NONE, |pointer| pointer.journal);
    let mut journal_counts = crate::journal_checkpoint::Counts::of(answer_places, &chosen, now_places.stored());
    let mut sweep_counts = journal.counts(0, 0);
    phases.mark_cx(cx, "journal replayed", frame);
    // Each module slot the last answer read, as the sweep found it today: the planner takes a
    // slot's state from here and reads no file whose stamp proves it ([`Kept`]).
    let mut script_sweep = crate::engine_script_reads::CheckoutSweep::for_command(place.served, heap.alloc(), frame);
    // The scripts' status answers start from what the last command kept, at the journal's place.
    if let Some(sweep) = script_sweep.as_mut() { sweep.prepare(cx, &mut store, answer_root.as_str(), &journal, frame)?; }
    let mut followed_kept = None;
    let mut today_slots: std::vec::Vec<(&str, sweep::Probed), &Frame> = std::vec::Vec::new_in(frame);
    if let (Some(pointer), Some(lines)) = (&pointer, &last_lines) {
        // Only the rows the journal's changes reach are looked at; all where it says nothing.
        // Another checkout's leaves: every row looked at (its journal is not this checkout's).
        let marked = match base {
            crate::engine_ready_edit::StoredBase::OtherCheckout => None,
            crate::engine_ready_edit::StoredBase::Own | crate::engine_ready_edit::StoredBase::ToolsMoved => journal.marked(&pointer.leaves, frame),
        };
        let follow = sweep::Follow { stamps: FromMeta::now(phases.clock()), plant: sweep::Plant::None };
        let follow_began = phases.clock().now();
        let facts_vouched = base != crate::engine_ready_edit::StoredBase::OtherCheckout && journal.facts_vouched();
        let (followed, statted) = follow.follow_served_with_facts(cx, root, Some(opened.home.dir()), &pointer.leaves, marked.as_deref(), script_sweep.as_mut(), facts_vouched, frame)?;
        let follow_ns = phases.clock().now().since(follow_began).map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX));
        sweep_counts = journal.counts(statted, u32::try_from(pointer.leaves.len()).unwrap_or(u32::MAX)).with_parts(journal.parts(&pointer.leaves, frame)).with_looks(followed.looks, follow_ns);
        // Set, then borrowed SHARED: `Option::insert` hands back a `&mut`, and `today_slots` below
        // keeps paths out of it, which would hold that exclusive borrow past the later readers.
        followed_kept = Some(followed);
        let followed: &sweep::Followed<'_> = followed_kept.as_ref().expect("set on the line above");
        today_slots.extend(followed.now.rows.iter().filter(|leaf| leaf.kind == sweep::LeafKind::Slot).map(|leaf| (followed.now.path(leaf), (leaf.flags, leaf.stamp, leaf.id))));
        today_slots.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let moved = followed.layout_moved || followed.now.rows.iter().zip(followed.was.iter()).any(|(now, was)| now.state() != *was);
        phases.mark_cx(cx, "sweep", frame);
        let facts = pointer.facts;
        // The certificate reads no row, so it alone needs the tools (engine_answer.rs "THE
        // TOOLS"), and it renews this checkout's own roots: it stands on this checkout's answer
        // under today's tools alone (`StoredBase::Own`). Else the phase path runs over the same
        // answer, each row standing or entered by its own key.
        if !moved && facts.printed.replayable && facts.owed == 0 && base == crate::engine_ready_edit::StoredBase::Own {
            // The certificate's print and commit, then its profile, each in a call of its own
            // (rule 54: no stage's temporaries in this frame).
            if let Some(sweep) = script_sweep.as_mut() { sweep.keep(cx, &mut store, answer_root.as_str(), &followed.now, frame)?; sweep_counts.status = Some(sweep.counts()); }
            let journal = (&chosen, &now_places, &mut journal_counts);
            if let Some(live) = live {
                live.see(blaze_screen::Step::Says);
            }
            let certified_key = (pointer.key, pointer.lines, pointer.facts);
            let stood = certified(cx, root, &view_slot, pointer, lines, answer_root.as_str(), journal, &mut store, (&tools, toolchain), shown, phases, began, request, frame, out)?;
            drop(store);
            let cleanup = match owned_opened {
                Some(owned) => crate::store::after_answer_printed(cx, owned.lent, &owned.home, false, phases.clock(), err)?,
                None => {
                    let (cleanup, spawn_bg) = crate::store::after_answer_for_daemon(cx, &opened.lent, &opened.home)?;
                    if let Some(dout) = daemon_out.as_deref_mut() {
                        dout.was_phase_rebuild = false;
                        dout.spawn_bg = spawn_bg;
                        dout.wrote_pointer = false;
                        dout.certified = true;
                        dout.lint_step = Some(linting.step);
                        dout.certified_facts = Some(certified_key);
                    }
                    cleanup
                }
            };
            phases.mark_cx(cx, "store committed", frame);
            return certificate_checked(cx, root, stood, sweep_counts, &journal_counts, &cleanup, phases, heap, frame);
        }
    }
    let last = match &pointer {
        Some(pointer) => match crate::engine_answer::open_root(cx, &mut store, pointer.root, request)? {
            Ok(root) => Some((root, &pointer.headers)),
            Err(why) => {
                unreadable = Some(why);
                None
            }
        },
        None => None,
    };
    // The driver, decided before any row is read (engine_ready_edit.rs `choose`): from the
    // stored root and the sweep's changed leaves. Under `--driver=ready` an edit the ready path
    // cannot serve stops here, naming why (rule 3: the phases never run in its place). Under
    // `--driver=auto` (the default) an ineligible edit routes cleanly to the phase path.
    let changed = followed_kept.as_ref().map(|followed| crate::engine_ready_edit::changed_leaves(followed, request));
    let mut chosen = crate::engine_ready_edit::choose(driver, last.as_ref().map(|(root, _)| root), base, changed.as_deref().unwrap_or(&[])).map_err(EngineCheckError::ReadyRefused)?;
    drop(changed);
    // The ready path reads no header shard (D187): the index it reads is the one its commit
    // leaves, `Absent` since this request (blaze-engine headers.rs `Headers::after` of a ready
    // commit), so every header and planner row is read from its row's page. An index already
    // absent is read as it is.
    let ready_headers = match (&last, chosen) {
        (Some((root, blaze_engine::pages::Headers::Current(_))), crate::engine_ready_edit::Chosen::Wake { .. }) => {
            Some(made_boxed(heap, || blaze_engine::pages::Headers::Absent { since: blaze_engine::Gen(root.serial.0 + 1) }))
        }
        _ => None,
    };
    let snapshot = if last.is_some() { Some(store.snapshot_in(request)?) } else { None };
    #[inline(never)]
    fn open_stored<'a>(
        root: &'a blaze_engine::pages::Root<'a>,
        snapshot: &'a blaze_cas::plain::Snapshot<SharedAlloc<'a>>,
        headers: &'a blaze_engine::pages::Headers,
        alloc: SharedAlloc<'a>,
    ) -> crate::engine_answer::Stored<'a> {
        crate::engine_answer::Stored::new(root, snapshot, headers, blaze_engine::pages::Check::Store, alloc)
    }
    let mut stored = match (&last, &snapshot) {
        (Some((root, headers)), Some(snapshot)) => {
            let headers: &blaze_engine::pages::Headers = ready_headers.as_deref().unwrap_or(*headers);
            Some(open_stored(root, snapshot, headers, request))
        }
        _ => None,
    };
    let last_root = stored.as_ref().map(|source| source.root);
    let last_serial = last_root.map_or(0, |root| root.serial.0);
    if pointer.is_some() {
        phases.mark_cx(cx, "stored answer read", frame);
    }
    let read_from_last = last_root.is_some();
    let env = place.fetch_env(phases.clock().clone(), toolchain, fs.clone(), heap);

    // 1. Plan: the units stage, then the files, then the crates; on the ready path
    // ([`ready_setup`]) none of them: the graph's row and list, the outlines read back by their
    // rows, and the edited files outlined.
    let stamps = FromMeta::now(phases.clock());
    // The request's handle: the plan, the engine, the buffers the workers fill (merged into the
    // answer), the lines printed. Nothing over it outlives this call.
    let alloc = request;
    let stood = followed_kept.as_ref().and_then(crate::engine_units::Stood::of);
    let mut scripts = made_boxed(heap, || ScriptsStage::new(frame));
    scripts.status_view = script_sweep.as_ref().map(crate::engine_script_reads::CheckoutSweep::view);
    let grains_root = bstd::mem::format_in!(frame, "{answer_root}.grains");
    let grains_record = store.named(grains_root.as_str())?.filter(|keys| keys.len() == 32).map(|keys| {
        let mut id = [0u8; 32];
        id.copy_from_slice(keys);
        blaze_drive::Key(blaze_ids::ContentId(id))
    });
    let members_root = bstd::mem::format_in!(frame, "{answer_root}.members");
    let members_record = store.named(members_root.as_str())?.filter(|keys| keys.len() == 32).map(|keys| {
        let mut id = [0u8; 32];
        id.copy_from_slice(keys);
        blaze_drive::Key(blaze_ids::ContentId(id))
    });
    let mut bodies = made_boxed(heap, || crate::engine_bodies::BodiesStage::new(scope_reads));
    bodies.grains_from(grains_record, last_serial);
    bodies.members_from(members_record, last_serial);

    // 1a/1b. Setup (ready preflight with optional `Auto` fallback to phase setup), in a single
    // `own_frame` returning a heap-boxed struct so none of the setup's locals, closures, or
    // 14-field outputs sit on `checking`'s debug stack frame under the stages (rule 54).
    struct CheckedSetup<'f, 'h> {
        laid: Option<blaze_own::Box<crate::units::Laid<'f>, blaze_own::Alloc<'static>>>,
        rows: Rows<'f>,
        discovered: crate::engine_units::Discovered<'f>,
        outlined: SVec<'h, Outlined<'h>>,
        file_counts: Files,
        skipped: Skipped<'h>,
        file_places: SVec<'h, (Name, blaze_engine::pages::FilePlace)>,
        names: Option<NamesStage<'f>>,
        graph: Option<blaze_drive::Input>,
        deferred: SVec<'h, (Want<'h>, sweep::Probed)>,
        slots: Option<u32>,
        found_places: SVec<'h, (Name, blaze_engine::pages::FilePlace)>,
        planned: SVec<'h, Planned>,
        given_answers: SVec<'h, (Name, blaze_ids::ContentId)>,
        ready_woken: Option<(SVec<'h, Name>, crate::engine_ready_edit::Woke, SVec<'h, Task>)>,
    }

    let mut ready_preflight = None;
    if matches!(chosen, crate::engine_ready_edit::Chosen::Wake { .. }) && !crate::engine_units::ready_lays_out() {
        let attempt = own_frame(|| -> Result<Result<blaze_own::Box<CheckedSetup<'_, '_>, blaze_own::Alloc<'static>>, crate::engine_ready_edit::ReadyRefused>, CliError> {
            #[cfg(test)]
            stack_probe::at("checking: the setup");
            use crate::engine_ready_edit::{PhasesWhy, ReadyRefused};
            let Some(stood) = stood.as_ref() else { return Ok(Err(ReadyRefused::Root(PhasesWhy::Layout))) };
            let Some(last) = last_root else { return Ok(Err(ReadyRefused::Root(PhasesWhy::NoAnswer))) };
            let Some(last_stored) = stored.as_ref() else { return Ok(Err(ReadyRefused::Root(PhasesWhy::NoAnswer))) };
            let Some(followed) = followed_kept.as_ref() else { return Ok(Err(ReadyRefused::Root(PhasesWhy::NoAnswer))) };
            let Some(lines) = last_lines.as_ref() else { return Ok(Err(ReadyRefused::Root(PhasesWhy::NoAnswer))) };
            let pre_start = last_stored.answer();
            let pre_records = RecordKeys { base: Some(last_stored as &dyn StoredPages), changes: SMap::new_in(alloc) };
            let setup = match ready_setup(cx, root, &stamps, stood, followed, last, lines, &pre_start, &pre_records, &tools, &mut store, &env, workers, Some(linting.rules), daemon_hot_graph.as_deref_mut(), alloc, frame)? {
                Ok(setup) => setup,
                Err(why) => return Ok(Err(why)),
            };
            phases.mark_cx(cx, "ready setup (the graph's row and crate list, outlines read back, the edited files outlined)", frame);
            let mut discovered = crate::engine_units::Discovered::ready(stood, frame);
            discovered.plan.files_read = setup.rows_read;
            discovered.plan.files_total = setup.slots;
            let ReadySetup { rows, names, graph, outlined, deferred, slots, files: file_counts, skipped, places: file_places, rows_read: _ } = setup;
            let names = Some(NamesStage::new(names, frame));
            let graph = Some(graph);
            let plan_list = names.as_ref().and_then(|stage| stage.names.as_ref()).map(|world| world.list());
            let mut found_places: SVec<'_, (Name, blaze_engine::pages::FilePlace)> = SVec::new_in(alloc);
            found_places.extend(last.places.iter().filter(|(_, place)| place.flags & PLACE_FOUND != 0).map(|(file, place)| (*file, *place)));
            found_places.sort_unstable_by_key(|(file, _)| *file);
            let found_units = match plan_list {
                Some(list) if !found_places.is_empty() => crate::engine_body_plan::found_units(list, heap.alloc()),
                _ => blaze_own::Vec::new_in(heap.alloc()),
            };
            let found_edges = crate::engine_body_plan::found_edges(&found_units, &found_places, heap.alloc());
            let reach = match bodies.plan_reach(cx, &mut store, graph, plan_list, &outlined, &found_edges, crate::engine_bodies::ReachFrom::MembersRecord, heap.alloc()) {
                Ok(reach) => reach,
                Err(CliError::EngineCheck(EngineCheckError::ReadyRefused(why))) => return Ok(Err(why)),
                Err(error) => return Err(error),
            };
            let given_planned = crate::engine_script_reads::plan(&followed.now, alloc);
            let mut given_answers: SVec<'_, (Name, blaze_ids::ContentId)> = SVec::with_capacity_in(given_planned.len(), alloc);
            given_answers.extend(given_planned.iter().map(|(one, answer, _)| (one.name, *answer)));
            let mut planned: SVec<'_, Planned> = SVec::new_in(alloc);
            planned.extend(given_planned.iter().map(|(one, _, _)| *one));
            crate::engine_ready_edit::file_tasks(&outlined, &skipped.files, &skipped.external, names.is_some(), &linting.rules, reach, alloc, &mut planned);
            planned.sort_unstable_by_key(|one| (one.file, one.name));
            let mut pre_files: SVec<'_, (Name, &[Planned])> = SVec::new_in(alloc);
            for group in planned.chunk_by(|a, b| a.file == b.file) {
                let file = group[0].file;
                if last.files.get(&file).copied().flatten().is_some_and(|was| was == blaze_engine::pages::plan_digest(file, group, &tools, alloc)) {
                    continue;
                }
                pre_files.push((file, group));
            }
            for &file in last.files.keys() {
                if file.kind == Kind::Given && given_answers.binary_search_by_key(&file, |(name, _)| *name).is_err() {
                    pre_files.push((file, &[]));
                }
            }
            let (mut woken, woke) = match crate::engine_ready_edit::wake(cx, &mut store, last_stored, &tools, &outlined, &given_answers, &pre_files, alloc) {
                Ok(out) => out,
                Err(CliError::EngineCheck(EngineCheckError::ReadyRefused(why))) => return Ok(Err(why)),
                Err(error) => return Err(error),
            };
            expand_woken_views(reach, &pre_files, &mut woken);
            match crate::engine_ready_edit::admit(&woken) {
                Ok(crate::engine_ready_edit::EditPath::Ready(_)) => {}
                Ok(crate::engine_ready_edit::EditPath::Phases(why)) => return Ok(Err(ReadyRefused::Root(why))),
                Err(why) => return Ok(Err(why)),
            }
            let entering_tasks = crate::engine_ready_edit::entering_tasks(last_stored, &tools, &pre_files, &woken, alloc)?;
            drop(pre_files);
            phases.mark_cx(cx, "ready wake (the entering names from the stored pages, admitted, their tasks)", frame);
            Ok(Ok(blaze_own::Box::new_in(
                CheckedSetup {
                    laid: None,
                    rows,
                    discovered,
                    outlined,
                    file_counts,
                    skipped,
                    file_places,
                    names,
                    graph,
                    deferred,
                    slots: Some(slots),
                    found_places,
                    planned,
                    given_answers,
                    ready_woken: Some((woken, woke, entering_tasks)),
                },
                heap.alloc(),
            )))
        })?;
        match attempt {
            Ok(preflight) => ready_preflight = Some(preflight),
            Err(why) if driver == crate::engine_ready_edit::Driver::Auto => {
                chosen = crate::engine_ready_edit::Chosen::Phases(why.phases_why());
                // Restore the stored answer with its real producer header shards so the phase
                // path reads headers from shards instead of task pages.
                if let (Some((root, headers)), Some(snapshot)) = (&last, &snapshot) {
                    stored = Some(open_stored(root, snapshot, headers, request));
                }
                bodies = made_boxed(heap, || crate::engine_bodies::BodiesStage::new(scope_reads));
                bodies.grains_from(grains_record, last_serial);
                bodies.members_from(members_record, last_serial);
                phases.mark_cx(cx, "ready preflight (ineligible; running phases)", frame);
            }
            Err(why) => return Err(EngineCheckError::ReadyRefused(why).into()),
        }
    }

    let start = stored.as_ref().map_or_else(|| Answer::new_in(request), |source| source.answer());
    let mut records = RecordKeys { base: stored.as_ref().map(|one| one as &dyn StoredPages), changes: SMap::new_in(request) };
    let mut setup = match ready_preflight {
        Some(pre) => pre,
        None => own_frame(|| -> Result<blaze_own::Box<CheckedSetup<'_, '_>, blaze_own::Alloc<'static>>, CliError> {
            // The phase path's setup (or a ready request under a test's `ready_lays_out` plant).
            #[cfg(test)]
            stack_probe::at("checking: the setup");
            let mut planned: SVec<'_, Planned> = SVec::new_in(alloc);
            let mut rows = Rows::new_in(frame);
            // PHASE-MEM-DEBT(Units): the layout's manifests on the process heap (units.rs `lay`, `heap:
            // &'static Heap`): the request's, read by every stage's plan.
            let mut laid_here = crate::units::lay(cx, root, &stamps, &mut rows, toolchain, heap, frame, erased.handle())?;
            let home_dir = Some(opened.home.dir());
            sweep::read_home_listing(cx, home_dir, &stamps, crate::macros::MODULE_LISTING, &mut rows, frame)?;
            sweep::read_home_listing(cx, home_dir, &stamps, crate::scripts::MODULE_LISTING, &mut rows, frame)?;
            phases.mark_cx(cx, "layout", frame);
            let units = sweep::units_source(rows.rows.iter().map(|leaf| (rows.path(leaf), leaf)), frame);
            if let (Some(lockfile), Some(parts)) = (laid_here.lock.as_ref(), laid_here.parts.take()) {
                let program = crate::program::dev_profile_panic(cx, root, frame)?;
                // Each with the rustflags cargo gives its unit (graph.rs `target_of`): a `--cfg`
                // of the checkout's `.cargo/config.toml` reaches every unit's cfg.
                let target = crate::graph::target_of(cx, &env, &laid_here.rustflags, program, &blaze_unit_graph::UnitKind::Lib, frame)?;
                let host = crate::graph::target_of(cx, &env, &laid_here.rustflags, program, &blaze_unit_graph::UnitKind::BuildScript, frame)?;
                // PHASE-MEM-DEBT(Units): the unit graph on the process heap (graph.rs `Graph::of`,
                // `heap: &'static Heap`): the request's; the graph is a record (`discover` puts it).
                laid_here.graph = Some(crate::graph::Graph::of(parts, lockfile, units, target, host, true, heap, frame));
            }
            phases.mark_cx(cx, "plan (the graph's inputs: the profile's panic, the target's and the host's cfg)", frame);
            let mut discovered = {
                let answer_units = AnswerUnits { stored: stored.as_ref(), records: &records, alloc: request };
                let back = crate::engine_units::Back::new(&answer_units);
                crate::engine_units::discover(cx, root, &mut store, &mut laid_here, units, tools, &back, stood, &env, board, frame)?
            };
            phases.mark_cx(cx, discovered.cause.phase(), frame);
            let kept = Kept { slots: &today_slots, answer: &start, records: &records, stored: stored.as_ref(), tools: &tools, read: core::sync::atomic::AtomicU32::new(0) };
            let mut found_last: SVec<'_, (Name, blaze_engine::pages::FilePlace)> = SVec::new_in(alloc);
            if let Some(last) = last_root {
                found_last.extend(last.places.iter().filter(|(_, place)| place.flags & PLACE_FOUND != 0).map(|(file, place)| (*file, *place)));
            }
            let (outlined, probes, file_counts, skipped, file_places) = {
                let cx = cx.span(FILE_WALK_SPAN);
                plan(&cx, root, &stamps, &laid_here, workers, &kept, &mut store, discovered.names.as_ref(), &found_last, Some(linting.rules), alloc)?
            };
            discovered.plan.files_read = kept.read.load(core::sync::atomic::Ordering::Relaxed);
            discovered.plan.files_total = file_counts.read_back.saturating_add(file_counts.outlined);
            for (slot, probed) in probes.iter() {
                let slot = core::str::from_utf8(slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
                sweep::opened(slot, *probed, &mut rows);
            }
            phases.mark_cx(cx, "plan (files found and outlined)", frame);
            let names = discovered.names.take().map(|world| NamesStage::new(world, frame));
            for one in discovered.published.iter() {
                planned.push(Planned { name: one.name, phase: Phase::new(Stage::Units, one.wave), source: one.source, file: one.name });
            }
            if let Some(stage) = &names {
                let _cx = cx.span(STAGES_PLAN_SPAN);
                planned.extend(scripts.plan(stage, &discovered, tools, &env, alloc, frame)?);
                let no_reads = Rows::new_in(frame);
                planned.extend(scripts.plan_reads(followed_kept.as_ref().map_or(&no_reads, |followed: &sweep::Followed<'_>| &followed.now), alloc));
                planned.extend(stage.plan(alloc, frame)?);
                planned.extend(crate::engine_heads::plan(stage, alloc)?);
            }
            phases.mark_cx(cx, "plan (the scripts, and the crates' names at their depths)", frame);
            let graph = discovered.get(Name::unit_graph()).and_then(|one| one.made.as_ref().ok().map(|record| blaze_drive::Input { name: one.name, at: tools.key(one.name, &one.source), out: record.out }));
            let plan_list = names.as_ref().and_then(|stage| stage.names.as_ref()).map(|world| world.list());
            #[cfg(test)]
            let stored_list = match (crate::engine_units::PLAN_PLANT.with(core::cell::Cell::get), graph) {
                (crate::engine_units::PlanPlant::ReachFromStore, Some(graph)) => Some(bodies.kept.list(cx, &mut store, &graph, frame)?),
                _ => None,
            };
            #[cfg(test)]
            let plan_list = match &stored_list {
                Some(stored) => stored.as_ref(),
                None => plan_list,
            };
            let mut found_places: SVec<'_, (Name, blaze_engine::pages::FilePlace)> = SVec::new_in(alloc);
            found_places.extend(file_places.iter().filter(|(_, place)| place.flags & PLACE_FOUND != 0).copied());
            found_places.sort_unstable_by_key(|(file, _)| *file);
            let found_units = match plan_list {
                Some(list) if !found_places.is_empty() => crate::engine_body_plan::found_units(list, heap.alloc()),
                _ => blaze_own::Vec::new_in(heap.alloc()),
            };
            let found_edges = crate::engine_body_plan::found_edges(&found_units, &found_places, heap.alloc());
            let reach = bodies.plan_reach(cx, &mut store, graph, plan_list, &outlined, &found_edges, crate::engine_bodies::ReachFrom::Walk, heap.alloc())?;
            crate::engine_ready_edit::file_tasks(&outlined, &skipped.files, &skipped.external, names.is_some(), &linting.rules, reach, alloc, &mut planned);
            planned.sort_unstable_by_key(|one| (one.file, one.name));
            Ok(blaze_own::Box::new_in(
                CheckedSetup {
                    laid: Some(blaze_own::Box::new_in(laid_here, heap.alloc())),
                    rows,
                    discovered,
                    outlined,
                    file_counts,
                    skipped,
                    file_places,
                    names,
                    graph,
                    deferred: SVec::new_in(alloc),
                    slots: None,
                    found_places,
                    planned,
                    given_answers: SVec::new_in(alloc),
                    ready_woken: None,
                },
                heap.alloc(),
            ))
        })?,
    };
    let setup = &mut *setup;
    let laid_now: Option<&crate::units::Laid<'_>> = setup.laid.as_deref();
    let ready_woken = setup.ready_woken.take();
    let rows = &mut setup.rows;
    let discovered = &mut setup.discovered;
    let outlined = &mut setup.outlined;
    let file_counts = &mut setup.file_counts;
    let skipped = &setup.skipped;
    let file_places = &setup.file_places;
    let names = &mut setup.names;
    let graph = setup.graph;
    let deferred = &setup.deferred;
    let slots = setup.slots;
    let found_places = &setup.found_places;
    let planned = &setup.planned;
    let given_answers = &mut setup.given_answers;
    let mut lints = made_boxed(heap, || crate::engine_lints::LintsStage::new(linting.rules, tools.0[blaze_drive::Kind::Lint.index()].0));
    let mut heads = made_boxed(heap, || crate::engine_heads::HeadsStage::new(scope_reads));
    heads.freeze = head_units;
    heads.jobs = head_jobs;
    // PHASE-MEM-DEBT(Heads): the request's frozen heads on the process heap
    // (engine_heads_once.rs `HeadsOnce::new`, `heap: &'static Heap`): they cross from the Heads
    // phases to the Bodies phases, so the request's; to take `SharedAlloc<'r>` of the request.
    heads.once = Some(crate::engine_heads_once::HeadsOnce::new(heap, crate::engine_heads_once::OncePlant::Rule, head_sources));
    let planned_now: &SVec<'_, Planned> = &planned;
    // The edit's files and each phase's count, in a frame of their own ([`own_frame`]).
    let (files, phase_counts) = own_frame(|| {
        let mut files: SVec<'_, (Name, &[Planned])> = SVec::new_in(alloc);
        let mut today: SSet<'_, Name> = SSet::new_in(alloc);
        let mut phase_counts = SMap::new_in(alloc);
        for group in planned_now.chunk_by(|a, b| a.file == b.file) {
            let file = group[0].file;
            today.insert(file);
            for task in group {
                let count = phase_counts.entry(task.phase).or_insert(0u32);
                *count = count.saturating_add(1);
            }
            if last_root.and_then(|root| root.files.get(&file)).copied().flatten()
                .is_some_and(|last| last == blaze_engine::pages::plan_digest(file, group, &tools, alloc))
            {
                continue;
            }
            files.push((file, group));
        }
        // A file the last answer planned and today's plan does not (deleted, or no module names it
        // now): in the edit with no tasks, so its rows go and their cells move to absent. The phase
        // path's alone: the ready path plans the edited files only, and `choose` woke it because no
        // file was added or removed (engine_ready_edit.rs `PhasesWhy::FileAddedOrRemoved`).
        if let Some(root) = last_root {
            let whole_plan = matches!(chosen, crate::engine_ready_edit::Chosen::Phases(_));
            for file in root.files.keys() {
                if (whole_plan || file.kind == Kind::Given) && !today.contains(file) {
                    files.push((*file, &[]));
                }
            }
        }
        (files, phase_counts)
    });
    // Restore the producer marks the current plan can consume. Only a build script needs its
    // old read list, to order this request's checkout reads and script runs. In a frame of its
    // own ([`own_frame`]): its rows' `?` results are not the stages'.
    if ready_woken.is_none() {
        own_frame(|| -> Result<(), CliError> {
            for task in planned.iter().filter(|one| matches!(one.phase.stage, Stage::Scripts | Stage::Names)) {
                let name = task.name;
                // One header lookup gives both `facts` and `record` when backed by `stored`
                // (records.changes is empty here before any stage runs), avoiding a second
                // `Stored::header(name)` probe per planned Scripts/Names task.
                let (row, record) = match stored.as_ref() {
                    Some(last) if records.changes.is_empty() => {
                        let Some(header) = last.header(name).map_err(crate::engine_answer::page_error)? else { continue };
                        (header.facts, header.record)
                    }
                    _ => {
                        let Some(row) = start.row_facts(name).map_err(EngineError::from)? else { continue };
                        let record = if row.failed || name.kind == Kind::Given { None } else { records.get(name)? };
                        (row, record)
                    }
                };
                if row.phase.stage != task.phase.stage {
                    continue;
                }
                let made = if row.failed || name.kind == Kind::Given {
                    None
                } else {
                    Some((record.ok_or(EngineCheckError::MissingRecord { name })?, row.out))
                };
                if row.phase.stage == Stage::Scripts {
                    if name.kind == Kind::Script {
                        let script = stored.as_ref().ok_or(EngineCheckError::MissingIndexedRow { name })?
                            .planner_row(name, alloc).map_err(crate::engine_answer::page_error)?
                            .ok_or(EngineCheckError::MissingIndexedRow { name })?;
                        scripts.restore(name, made, &script, start.owed_tasks.contains_key(&name), frame);
                    } else {
                        scripts.mark(name, made);
                    }
                }
                if let Some(stage) = names.as_mut() {
                    stage.mark(name, made);
                }
            }
            Ok(())
        })?;
    }

    // 1b. On `Ready`: the names the edit enters, found forward from what moved on the stored
    // answer's pages alone (engine_ready_edit.rs `wake`), already computed in `ready_preflight`
    // unless the test plant `ready_lays_out` ran the phase setup above.
    let (woken, woke, entering_tasks) = match ready_woken {
        Some((woken, woke, tasks)) => (woken, Some(woke), tasks),
        None => match (chosen, stored.as_ref()) {
            (crate::engine_ready_edit::Chosen::Wake { .. }, Some(last)) => {
                if let Some(followed) = followed_kept.as_ref() {
                    let planned_given = crate::engine_script_reads::plan(&followed.now, alloc);
                    given_answers.extend(planned_given.iter().map(|(one, answer, _)| (one.name, *answer)));
                }
                let (mut woken, woke) = crate::engine_ready_edit::wake(cx, &mut store, last, &tools, &outlined, given_answers.as_slice(), &files, alloc)?;
                expand_woken_views(bodies.reach(), &files, &mut woken);
                let entering_tasks = crate::engine_ready_edit::entering_tasks(last, &tools, &files, &woken, alloc)?;
                phases.mark_cx(cx, "ready wake (the entering names from the stored pages, admitted, their tasks)", frame);
                (woken, Some(woke), entering_tasks)
            }
            _ => (SVec::new_in(alloc), None, SVec::new_in(alloc)),
        },
    };
    let entering = match woke {
        Some(_) => match crate::engine_ready_edit::admit(&woken).map_err(EngineCheckError::ReadyRefused)? {
            crate::engine_ready_edit::EditPath::Ready(entering) => Some(entering),
            crate::engine_ready_edit::EditPath::Phases(why) => return Err(EngineCheckError::ReadyRefused(crate::engine_ready_edit::ReadyRefused::Root(why)).into()),
        },
        None => None,
    };
    // Then, on the ready setup (piece B2), only the outlines the edit needs read back
    // ([`read_needed`]): the members the bodies' setup will visit (engine_bodies.rs
    // `BodiesStage::ready_reads`, the one standing decision `groups` makes) and the entering
    // tasks' files; every other file's notes kept from the last lines record. In a frame of its
    // own ([`own_frame`]).
    let unread_notes = own_frame(|| -> Result<_, CliError> {
        if slots.is_some()
            && let (Some(last), Some(stage), Some(graph)) = (stored.as_ref(), names.as_ref(), graph)
        {
            let g = heap.alloc();
            // As `groups` makes it under `StandingMembers`: the wake's outlines and the owed bodies' files.
            let mut moved: Vec<Name, blaze_own::Alloc<'static>> = Vec::new_in(g);
            moved.extend(woken.iter().copied().filter(|name| name.kind == Kind::FileItems));
            moved.extend(entering_tasks.iter().filter(|task| task.planned.name.kind == Kind::BodyCheck).map(|task| task.planned.file));
            moved.sort_unstable();
            moved.dedup();
            let mut owed: SVec<'_, Name> = SVec::new_in(alloc);
            owed.extend(entering_tasks.iter().map(|task| task.planned.file).filter(|file| file.kind == Kind::FileItems));
            owed.sort_unstable();
            owed.dedup();
            let names_missing: std::sync::OnceLock<Name> = std::sync::OnceLock::new();
            let inputs = crate::engine_names::NamesInputs::stood(stage, crate::engine_names::Stood { pages: last, missing: &names_missing });
            let closed = bodies.ready_reads(cx, &mut store, graph, inputs, tools.0[Kind::BodyCheck.index()], &moved, frame, g)?;
            if let Some(&name) = names_missing.get() {
                return Err(EngineCheckError::MissingRecord { name }.into());
            }
            let kept = Kept { slots: &[], answer: &start, records: &records, stored: Some(last), tools: &tools, read: core::sync::atomic::AtomicU32::new(0) };
            // The planted control (engine_ready_edit.rs `CommitPlant::ReadsNoVisitedTree`).
            let closed: &[Name] = match ready_plant {
                crate::engine_ready_edit::CommitPlant::ReadsNoVisitedTree => &[],
                _ => &closed,
            };
            let unread = read_needed(cx, root, &stamps, &kept, &mut store, deferred, (closed, owed.as_slice()), outlined, file_counts, workers, Some(linting.rules), alloc)?;
            discovered.plan.files_read = discovered.plan.files_read.saturating_add(kept.read.load(core::sync::atomic::Ordering::Relaxed));
            let mut unread_files: Vec<Name, blaze_own::Alloc<'static>> = Vec::with_capacity_in(unread.files.len(), g);
            unread_files.extend_from_slice(&unread.files);
            let mut unread_digests: blaze_own::Vec<(blaze_own::String<blaze_own::SharedAlloc<'static>>, blaze_ids::ContentId), blaze_own::SharedAlloc<'static>> = blaze_own::Vec::new_in(heap.shared());
            if !unread_files.is_empty()
                && let Some(followed) = followed_kept.as_ref()
            {
                for row in followed.now.rows.iter() {
                    if row.kind == crate::sweep::LeafKind::Slot && !row.absent() {
                        unread_digests.push((blaze_own::String::from_str_in(followed.now.path(row), heap.shared()), row.id));
                    }
                }
                unread_digests.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            }
            bodies.unread_outlines(unread_files, (!unread_digests.is_empty()).then(|| blaze_own::Arc::new_in(unread_digests, heap.shared())));
            phases.mark_cx(cx, "ready reads (the visited members' and the entering tasks' outlines read back)", frame);
            return Ok(unread.notes);
        }
        Ok(SVec::new_in(alloc))
    })?;

    // 2. Drive: each phase run, merged, its records put; or, on the ready path, the start held
    // for the engine the wake scopes ([`ready_drive`]).
    let driving = match chosen {
        crate::engine_ready_edit::Chosen::Phases(why) => Driving::Phases(own_frame(|| Engine::new_in(tools, start, &Edit { files: &files }, Everything, blaze_engine::Plant::None, alloc).map(|engine| blaze_own::Box::new_in(engine, heap.alloc())))?, why),
        crate::engine_ready_edit::Chosen::Wake { .. } => Driving::Ready(start),
    };
    // Made in a call of its own and boxed ([`made_boxed`]): 6.6 KiB by value (its `used_all`
    // spread; wave-1119 t59 type sizes), the value and its temporary sat in this frame under
    // every stage (rule 54).
    let mut pool = made_boxed(heap, || Workers::new(WorkersOpts { cx, clock: &env.clock, workers, board, alloc }));
    let mut buffers: SVec<'_, Buffer<'_>> = SVec::new_in(alloc);
    for _ in 0..pool.runs() {
        buffers.push(Buffer::new_in(alloc));
    }
    let members = crate::names::member_dirs(laid_now);
    let side = laid_now.and_then(|laid| laid.lock.as_ref()).map(|lockfile| crate::proc_runner::MacroSide {
        lock: lockfile,
        place: crate::macros::Place { home: env.home, checkout: env.checkout, members, rows: &rows },
        // PHASE-MEM-DEBT(Names): the proc macros' expansions on the process heap
        // (proc_runner.rs `MacroSide::heap`, `&'static Heap`): made in a Names phase, read by
        // the Scripts and Names phases after; the record is what should cross.
        heap,
    });
    let handles = Handles {
        root,
        lock: laid_now.and_then(|laid| laid.lock.as_ref()).map(|lockfile| lockfile.lock),
        members,
        env: &env,
        side: side.as_ref(),
        outlined: &outlined,
        workers,
        board,
        oracle: names_oracle,
        stamps,
        plants: OutlinePlants::now(Some(linting.rules)),
        found: &found_places,
    };
    // PHASE-MEM-DEBT(CHECKED): returned in `Checked`, which has no lifetime.
    let mut ran = blaze_own::Vec::new_in(heap.alloc());
    // What each stage's records cost this process (engine_store.rs `Ledger`).
    // Boxed ([`made_boxed`]): 3.2 KiB by value (wave-1119 t59), unboxed into `Checked` at the end.
    let mut ledger = made_boxed(heap, crate::engine_store::Ledger::new);
    // The Heads and Bodies stages' reader of the crates' records, in place in the store's
    // mapping for the store's borrow (engine_records.rs): dropped with the store, never in a stage.
    let mut read = made_boxed(heap, || crate::engine_records::EngineRecords::new(heap.alloc()));
    read.plant_unreadable(planted_unreadable);
    // The same plant for a scope made by need (engine_bodies.rs `scope_by_need`).
    bodies.plant_unreadable(planted_unreadable);
    // The row key's control (engine_ready_edit.rs `CommitPlant::RowKeyUnitOnly`), on either
    // driver: the command's plant is `None`.
    bodies.plant_rows(ready_plant);
    // Each phase's wall, its tasks' times, its merge and its put (engine_times.rs).
    let clock = &env.clock;
    let mut times = made_boxed(heap, || crate::engine_times::Times::new_in(alloc));
    let mut phase_times = std::vec::Vec::new_in(std::alloc::Global);
    // The request's frozen heads, counted at the Bodies phase's merge (engine_seeds.rs).
    // Boxed ([`made_boxed`]): 2.1 KiB by value (wave-1119 t59), unboxed into `Checked` at the end.
    let mut seed_counts = made_boxed(heap, crate::engine_seeds::SeedCounts::default);
    #[cfg(test)]
    let mut planted_kept = blaze_own::Vec::new_in(heap.alloc());
    let jobs = if scripts.in_plan() && let Driving::Phases(engine, _) = &driving && [Stage::Units, Stage::Outlines, Stage::Scripts].into_iter().any(|stage| engine.has_pending_stage(stage)) {
        Some(Frame::with_reserve(frame.reserved(), bstd::mem::Tag::Other)
            .map_err(|_| CliError::Text { what: "reserving the Scripts and Names jobs frame (engine_check.rs)" })?)
    } else { None };
    let job_frame = jobs.as_ref().unwrap_or(frame);
    let job_alloc = blaze_own::Erased::new(job_frame);
    // The queues share the requested worker limit. Keep the physical scope
    // threaded even when a request admits only one job at a time.
    let scope = cx.scope();
    let mut pipeline = crate::engine_scripts::Pipeline::new(&scope, job_frame, job_alloc.handle(), &handles);
    // What the names say of the found files, set once by the drive ([`serve_found`]).
    let serving = ServeFound { rules: linting.rules, skipped: &skipped.files, external: &skipped.external, places: &file_places, served: core::cell::OnceCell::new() };
    // The drive's state, made once and placed in the request's frame: every stage from here is
    // a call taking it by `&mut`, so this frame holds its pointer and each stage's temporaries
    // die with its call (rule 54; [`Drive`]).
    let rq = frame.alloc(Drive {
        cx,
        root,
        err,
        board,
        active,
        phases: &mut *phases,
        frame,
        alloc,
        heap,
        tools: &tools,
        workers,
        fresh,
        heads_from,
        body_locals,
        request_mem: &request_mem,
        env: &env,
        graph,
        outlined: &outlined,
        planned: &planned,
        discovered: &discovered,
        handles: &handles,
        names,
        scripts: &mut *scripts,
        bodies: &mut *bodies,
        heads: &mut *heads,
        lints: &mut *lints,
        pool: &mut *pool,
        buffers: &mut buffers,
        times: &mut *times,
        ledger: &mut *ledger,
        ran: &mut ran,
        phase_times: &mut phase_times,
        seed_counts: &mut *seed_counts,
        phase_mem: &mut *phase_mem,
        #[cfg(test)]
        planted_kept: &mut planted_kept,
        puts: 0,
        pipelined: false,
        serve: &serving,
        found_served: false,
    });
    let drove = match driving.parts() {
        Ok((mut engine, why)) => drive(rq, &mut *engine, &mut store, &mut *read, &mut records, &mut pipeline, last_lines.as_ref(), stored.as_ref(), &files).map(|()| Drove::Phases(engine, why)),
        Err(start) => match (stored.as_ref(), woke, entering) {
            (Some(last), Some(woke), Some(entering)) => ready_drive(rq, start, &files, (woken.as_slice(), entering, woke, entering_tasks.as_slice(), given_answers.as_slice()), last, &mut store, &mut *read, &mut records, last_lines.as_ref(), ready_plant).map(Drove::Ready),
            _ => Err(EngineCheckError::ReadyRefused(crate::engine_ready_edit::ReadyRefused::Root(crate::engine_ready_edit::PhasesWhy::NoAnswer)).into()),
        },
    };
    // The drive's last read: its borrows of this frame's stages end here.
    let puts = rq.puts;
    let drained = pipeline.drain(cx);
    drop(pipeline);
    let joined = scope.join(cx).map_err(CliError::Pool);
    let drove = drained.and(joined).and(drove)?;
    drop(job_alloc);
    drop(jobs);
    // The ready driver runs no names task (engine_ready_edit.rs `admit`), so nothing is found
    // on it; the found files of the last answer it plans from their places as any file. A find
    // after its drive is its own inconsistency ([`EngineCheckError::FoundOnReady`]).
    if matches!(drove, Drove::Ready(_))
        && let Some((task, file)) = names.as_ref().and_then(|stage| stage.found().iter().find_map(|by| by.outlined.first().map(|one| (by.unit, one.name))))
    {
        return Err(EngineCheckError::FoundOnReady { task, file }.into());
    }
    // The request's outlines from here: the served table when the names moved the found files
    // ([`Served`]), else the walk's.
    let outlined: &[Outlined<'_>] = match serving.served.get() {
        Some(served) => &served.outlined[..],
        None => &outlined[..],
    };
    // 3. The tail, in two calls of their own ([`own_frame`]): from the drive's end through the
    // store's last write, then (after the commit) the profile and the request's counts. Under a
    // debug build every temporary of a function has a stack slot of its own for the whole call,
    // so the tail's words and `?` results sat under every stage's frames (wave-1119 t39:
    // `checking` began 564 KiB below `check`, the bodies at 783 KiB; rule 54, piece C).
    let answered = own_frame(|| -> Result<_, CliError> {
        #[cfg(test)]
        stack_probe::at("checking: the answer's tail");
        // A split wave counts every entered task in its own batch. Attribute the
        // standing tasks once, to the last batch of that wave, for the profile.
        if !ran.is_empty() {
            let mut counts = SMap::new_in(alloc);
            for (phase, planned, entered) in ran.iter_mut() {
                let left = counts.entry(*phase).or_insert_with(|| phase_counts.get(phase).copied().unwrap_or(0));
                *planned = (*left).min(*entered);
                *left -= *planned;
            }
            for (phase, planned, _) in ran.iter_mut().rev() {
                *planned = planned.saturating_add(counts.remove(phase).unwrap_or(0));
            }
        }
        // Boxed on the process heap: 7 KiB by value (wave-1119 t59 type sizes), it rode in this
        // stretch's result, whose slots are [`checking`]'s for the whole call (rule 54).
        let accounts = blaze_own::Box::new_in(blaze_own::Box::into_inner(times).finish(&mut phase_times, workers), heap.alloc());
        // Every phase's arena made and dropped by now (each ended in its iteration).
        phase_mem.counts = request_mem.counts();
        // Every expansion of this command is done: its proc-macro answers to their store, once, so
        // the next command reads them by key (proc_runner.rs `keep`, lane M14).
        // The macro modules this command loaded: compiled now, or taken from their compiled records
        // (blaze-proc-macro host.rs, "Compiled records"): the profile's `macro modules` line.
        let mut macro_modules = None;
        if let Some(procs) = names.as_ref().and_then(|stage| stage.names.as_ref()).and_then(|world| world.procs()) {
            procs.keep().map_err(CliError::ProcAnswers)?;
            macro_modules = Some(procs.module_loads());
        }
        let finishing = clock.now();
        // The engine's answer: the phase path's finished now, the ready path's by its drive.
        let (mut finished, driver_line) = match drove {
            Drove::Phases(engine, why) => (blaze_own::Box::into_inner(engine).finish()?, DriverLine::Phases(why)),
            Drove::Ready(ReadyDrove { finished, facts }) => (finished, DriverLine::Ready(facts)),
        };
        finished.report.settle.finish_ns = clock.now().since(finishing).map_or(0, crate::engine_times::ns);
        // D169: the Outlines tasks' use on the pool's workers, beside the command's phases.
        phases.worked_used("Outlines tasks", &pool.used_all());
        phases.mark_cx(cx, "engine", frame);
        // The heap at each step of the tail (the profile's "tail heap" line): where it grows past
        // the Bodies phase is named by the step, never guessed (rule 17).
        let heap_engine = crate::engine_phase_mem::heap_now();

        // 3. Print: the whole answer's lines (engine_lines.rs), each entered task's block made from
        // its result now, each standing row's copied from the last request's record.
        let signatures_entered = u32::try_from(finished.causes.iter().filter(|one| one.name.kind == blaze_drive::Kind::Signature).count()).unwrap_or(u32::MAX);
        // Each file outlined after the walk ([`serve_found`]): its leaf row, as the walk's (`plan`).
        if let Some(served) = serving.served.get() {
            for (slot, probed) in served.probes.iter() {
                let slot = core::str::from_utf8(slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
                sweep::opened(slot, *probed, rows);
            }
        }
        // 3. Print: the whole answer's lines, in a call of their own ([`lines_of`]).
        let made = lines_of(cx, root, laid_now, &finished, last_lines.as_ref(), (names.as_ref(), &*scripts, &mut *bodies, &*heads, &*lints), (outlined, unread_notes.as_slice()), discovered, rows, skipped, heap, alloc, frame)?;
        phases.mark_cx(cx, "lines made", frame);
        let heap_lines_made = crate::engine_phase_mem::heap_now();
        // The closing line's count: every file the request has an outline of (on the ready setup,
        // those read back and those it did not need: `slots`), as the phase path's walk outlines each.
        let files_checked = slots.unwrap_or_else(|| u32::try_from(outlined.len()).unwrap_or(u32::MAX));
        if let Some(live) = live {
            live.see(blaze_screen::Step::Says);
        }
        let printed = print_answer(cx, root, &view_slot, &mut store, &made, rows, (names.as_ref(), &*bodies), &finished, (outlined, files_checked), unreadable.as_ref(), names_oracle, (&*phases, began), shown, frame, out)?;
        let AnswerPrinted { tally, lines_key, lines_bytes, status, oracle, bodies_whole } = printed;
        let unprinted = made.unprinted;
        drop(made);
        phases.mark_cx(cx, "answer printed", frame);
        let heap_printed = crate::engine_phase_mem::heap_now();
        // The answer stored, its pointer written last, all in the store's one commit after it.
        let report = finished.report;
        // The leaf rows of the checkout reads a row of the answer still reads: the scripts', and
        // the bodies' computed data includes (engine_script_reads.rs `keep_rows`). Kept while a
        // row READS the cell, not while its `Given` task has a row: a planned task always has
        // one, so a read no row makes any more would be followed forever (rule 18). One lookup
        // of the cell's readers a followed or fresh read (its readers page).
        let refused = core::cell::Cell::new(None);
        let followed_all_kept = !finished.answer.any_reader_removed_of(Kind::Given);
        scripts.script_rows(followed_kept.as_ref().map(|followed| &followed.now), bodies.data_files(), followed_all_kept, |name, from_followed| {
            let cell = CellId { of: name, cell: WHOLE };
            if from_followed && !finished.answer.reader_removed(cell) {
                return true;
            }
            match blaze_engine::LastAnswer::readers(&finished.answer, cell, blaze_own::Alloc::from(alloc)) {
                Ok(readers) => readers.iter().any(|reader| *reader != name),
                Err(error) => {
                    refused.set(Some(error));
                    false
                }
            }
        }, |kind, path| script_sweep.as_ref().is_some_and(|sweep| sweep.is_direct(kind, path)), |kind, path, answer| crate::engine_script_reads::settled_meta_stamp(cx, root, &stamps, sweep::Plant::None, kind, path, answer), rows);
        if let Some(error) = refused.into_inner() {
            return Err(EngineError::from(error).into());
        }
        let mut payloads = SMap::new_in(alloc);
        payloads.extend(records.changes.into_iter().filter_map(|(name, key)| key.map(|key| (name, key))));
        // The causes the commit keeps: every entered task's; a test's plant leaves the first out
        // (engine_ready_edit.rs `CommitPlant`, rule 23's control of the drivers' world test).
        let causes = match ready_plant {
            crate::engine_ready_edit::CommitPlant::None
            | crate::engine_ready_edit::CommitPlant::PagedHeadsMiss
            | crate::engine_ready_edit::CommitPlant::StandingDropsHeads
            | crate::engine_ready_edit::CommitPlant::ReadsNoVisitedTree
            | crate::engine_ready_edit::CommitPlant::RowKeyUnitOnly
            | crate::engine_ready_edit::CommitPlant::HoldersKeyedByText
            | crate::engine_ready_edit::CommitPlant::DeferredNeverVisited => &finished.causes[..],
            crate::engine_ready_edit::CommitPlant::DropsMovedPage => finished.causes.get(1..).unwrap_or(&[]),
        };
        let mut kept_answer = blaze_engine::pages::Kept::after(SMap::new_in(alloc), finished.answer, causes, payloads);
        // Each planned file's place beside its plan digest in the root (blaze-engine pages.rs
        // `FilePlace`), the same on both drivers.
        kept_answer.places.extend(file_places.iter().copied());
        // The found files' places as the names left them ([`serve_found`]): a dropped file keeps none.
        if let Some(served) = serving.served.get() {
            for (file, place) in served.places.iter() {
                match place {
                    Some(place) => {
                        kept_answer.places.insert(*file, *place);
                    }
                    None => {
                        kept_answer.places.remove(file);
                    }
                }
            }
        }
        // Whether every line printed beyond the closing one and its coverage is in the lines record,
        // or the next certificate cannot stand (it prints that record alone).
        let replayable = unprinted == 0 && bodies.ownerless == 0;
        let printed = crate::engine_answer::Printed { names: names.is_some(), bodies_whole, replayable };
        // The records the answer's root keeps beside its rows' own: each member's listing row and
        // each head listing, sorted, one each (a record the root does not name is collected and made
        // again by the next command: correct, but past the floor).
        let mut kept_beside = heads.listing_keys(heap.alloc());
        kept_beside.extend(bodies.member_row_keys().iter().copied());
        kept_beside.extend(bodies.typed_record_keys().iter().copied());
        kept_beside.sort_unstable();
        kept_beside.dedup();
        // One commit call for both drivers, so what follows it (the scripts' status keep) follows
        // either: the ready path's writes the pages its changed rows sit on and no header shard
        // (engine_answer.rs `save_changes`); the phase path's rewrites the shards too (`save_edit`).
        let saved = match (chosen, stored.as_ref()) {
            (crate::engine_ready_edit::Chosen::Wake { .. }, Some(last)) => crate::engine_answer::save_changes(cx, &mut store, &opened.lent, &answer_root, toolchain, &tools, &kept_answer, printed, (lines_key, &lines_bytes), last, &rows, &journal, alloc, frame, &kept_beside[..], (pointer.as_ref().map(|pointer| pointer.root), workers)),
            _ => crate::engine_answer::save_edit(cx, &mut store, &opened.lent, &answer_root, toolchain, &tools, &kept_answer, printed, (lines_key, &lines_bytes), stored.as_ref(), &rows, &journal, alloc, frame, &kept_beside[..], (pointer.as_ref().map(|pointer| pointer.root), workers)),
        }?;
        if let Some(sweep) = script_sweep.as_mut() { sweep.keep(cx, &mut store, answer_root.as_str(), &rows, frame)?; sweep_counts.status = Some(sweep.counts()); }
        let stored_keys = crate::engine_answer::StoredKeys::of(&saved, stored.as_ref());
        let (task_pages_read, header_pages_read) = stored.as_ref().map_or((0, 0), |one| one.read_counts());
        let pages_read = stored.as_ref().map(|one| one.pages_read_line()).unwrap_or_default();
        // The read grains' record this request put, named under the checkout's grains root from
        // the commit on: the next request takes its grains from it (engine_scope_reads.rs).
        if let Some(key) = bodies.grains_record() {
            store.pin(grains_root.as_str(), &[key])?;
        }
        // The members record this request put, named under the checkout's members root: the next
        // ready request takes its standing members from it (engine_members_record.rs).
        if let Some(key) = bodies.members_record() {
            store.pin(members_root.as_str(), &[key])?;
        }
        phases.mark_cx(cx, "answer stored", frame);
        #[cfg(test)]
        let audited = audit_answer(&kept_answer.answer, alloc, heap.alloc())?;
        #[cfg(not(test))]
        let audited = NotAudited;
        drop(kept_answer);
        let heap_stored = crate::engine_phase_mem::heap_now();
        // The store's one commit, after the answer, as `check`'s (main.rs).
        crate::store::Store::after_answer(&mut store, cx, &mut blaze_drive::profile::Profile::default())?;
        // Nothing read in place outlives the store's borrow of its `Lent` (engine_records.rs).
        drop(read);
        drop(store);
        let new_data_file = bodies.data_files().any(|(kind, path, _)| {
            followed_kept.as_ref().is_none_or(|f| {
                !f.now.rows.iter().any(|leaf| leaf.kind == kind.leaf() && f.now.path(leaf) == path)
            })
        });
        let leaf_set_changed = matches!(driver_line, DriverLine::Phases(_))
            || new_data_file
            || !followed_all_kept
            || file_counts.probed > 0
            || serving.served.get().is_some();
        Ok(blaze_own::Box::new_in((finished.causes, report, saved, stored_keys, (task_pages_read, header_pages_read, pages_read), (tally, status, oracle), accounts, macro_modules, (heap_engine, heap_lines_made, heap_printed, heap_stored), driver_line, signatures_entered, audited, leaf_set_changed), heap.alloc()))
    })?;
    // The whole cleanup line: this profile is written after the commit (store.rs).
    let is_phase_rebuild = matches!(answered.9, DriverLine::Phases(_));
    let leaf_set_changed = answered.12;
    let new_pointer_facts = (answered.2.pointer_key, answered.2.lines_key, answered.2.facts);
    let cleanup = match owned_opened {
        Some(owned) => crate::store::after_answer_printed(cx, owned.lent, &owned.home, false, phases.clock(), err)?,
        None => {
            let (cleanup, spawn_bg) = crate::store::after_answer_for_daemon(cx, &opened.lent, &opened.home)?;
            if let Some(dout) = daemon_out.as_deref_mut() {
                dout.was_phase_rebuild = is_phase_rebuild;
                dout.spawn_bg = spawn_bg;
                dout.wrote_pointer = true;
                dout.leaf_set_changed = leaf_set_changed;
                dout.certified = false;
                dout.lint_step = Some(linting.step);
                dout.certified_facts = Some(new_pointer_facts);
            }
            cleanup
        }
    };
    phases.mark_cx(cx, "store committed", frame);
    own_frame(|| -> Result<BoxedChecked, CliError> {
        let (causes, report, saved, stored_keys, (task_pages_read, header_pages_read, pages_read), (tally, status, oracle), accounts, macro_modules, (heap_engine, heap_lines_made, heap_printed, heap_stored), driver_line, signatures_entered, audited, _leaf_set_changed) = blaze_own::Box::into_inner(answered);
        #[cfg(test)]
        let (undefined_reads, head_rows, lint_rows) = audited;
        #[cfg(not(test))]
        let NotAudited = audited;
        let because = entered_because(&causes, alloc);
        let mut text = profile_text(&ran, &report, &because, &causes, &outlined, phases, frame);
        bodies.profile(&outlined, workers, &mut text);
        let _ = pool.balance_line(&mut text);
        lints.profile(&mut text);
        scripts.profile(&mut text);
        crate::engine_memory::profile(&ledger, names.as_ref(), &mut text);
        if names_oracle.on() && let Some(stage) = names.as_ref() {
            stage.oracle_profile(&mut text);
        }
        // The head tasks' freezes by unit (engine_heads.rs `UnitCounts`), said beside the table's.
        seed_counts.units = heads.unit_counts();
        seed_counts.once = crate::engine_heads_once::counts(&*heads);
        crate::engine_seeds::profile(&seed_counts, &mut text);
        // A task by what it is about: a file's path, a crate's name, a script's crate, a body's item.
        let world = names.as_ref().and_then(|stage| stage.names.as_ref());
        // Written into the profile's text in place: no string a label (engine_times.rs `Label`).
        let label = |name: Name, into: &mut dyn core::fmt::Write| -> bool {
            let crate_named = |at: usize, tail: &str, into: &mut dyn core::fmt::Write| world.is_some_and(|world| into.write_fmt(format_args!("{}{tail}", world.crate_label(at).0)).is_ok());
            match name.kind {
                // A body task's item, or one slice of an entry's bodies (engine_bodies.rs `task_label`).
                Kind::BodyCheck => bodies.task_label(name, into),
                Kind::CrateScope => world.and_then(|world| world.crate_at(name.subject)).is_some_and(|at| crate_named(at, "", into)),
                // A build script's module lookup, by the crate whose script it is (not_checked.rs's words).
                Kind::MacroModule => world.and_then(|world| world.script_module(name.subject)).is_some_and(|module| crate_named(module.crate_at, " (its build script's module)", into)),
                _ => match outlined.binary_search_by(|one| one.name.cmp(&name)) {
                    Ok(at) => core::str::from_utf8(&outlined[at].path).is_ok_and(|path| into.write_str(path).is_ok()),
                    Err(_) => world.and_then(|world| world.script_readers(name).next()).is_some_and(|at| crate_named(at, " (its build script)", into)),
                },
            }
        };
        crate::engine_times::write(&phase_times, &accounts, workers, &label, &mut text);
        ledger.write(&mut text);
        // Each stage's tasks, times and memory, by the status's printer (engine_status.rs).
        let _ = crate::engine_status::write_status(board, crate::engine_status::Heading::End, board.now(clock), &mut text);
        {
            use core::fmt::Write as _;
            let from = match (read_from_last, base) {
                (false, _) => "started from no answer",
                (true, crate::engine_ready_edit::StoredBase::Own) => "started from the stored answer",
                (true, crate::engine_ready_edit::StoredBase::ToolsMoved) => "started from the stored answer, written under other tools",
                (true, crate::engine_ready_edit::StoredBase::OtherCheckout) => "started from the stored answer of another checkout (its workspace's base)",
            };
            // A full frame cuts the profile short where it stood (as `profile_text`).
            let _ = writeln!(text, "stored answer: {from}; written: {} record(s) put, {} page(s), {} bytes", saved.puts, saved.pages, saved.bytes);
            let _ = writeln!(text, "stored answer base: {}", saved.base);
            let _ = writeln!(text, "stored answer pages read through publication: {task_pages_read} task, {header_pages_read} producer header; changed pages written: {} task ({} bytes, the largest {}), {} producer header", saved.new_pages, saved.new_page_bytes, saved.largest_new_page, saved.new_headers);
            let _ = writeln!(text, "{pages_read}");
            // The rows asked of the stored answer by kind, repeats and distinct names apart (rule
            // 17: which rows the request's reads are, before why they scale).
            let _ = writeln!(text, "stored answer {}", stored_keys.asked);
            // Which driver ran the edit (D187): the ready drive's wake and tasks by kind, or why the
            // phases ran (engine_ready_edit.rs `PhasesWhy`).
            let _ = writeln!(text, "{driver_line}");
            let _ = writeln!(text, "{stored_keys}");
            let _ = writeln!(text, "files: {} outline(s) read back from the store (file not read), {} outlined (file read), {} probed by the planner", file_counts.read_back, file_counts.outlined, file_counts.probed);
            let _ = writeln!(text, "{sweep_counts}");
            let _ = writeln!(text, "{journal_counts}");
            let _ = writeln!(text, "{}", discovered.plan);
            let _ = writeln!(text, "{cleanup}");
            let made = phase_mem.counts;
            let _ = writeln!(text, "phase arenas: {} made, {} dropped, {} bytes given back at their drops (engine_phase_mem.rs)", made.made, made.dropped, made.returned);
            let _ = writeln!(text, "{}", tally.tail.line(tally.listed, frame));
            match macro_modules {
                Some(loads) => {
                    let _ = writeln!(text, "{loads}");
                }
                None => {
                    let _ = writeln!(text, "macro modules: none (no proc-macro runner this command)");
                }
            }
            let _ = writeln!(text, "tail heap (bytes): engine finished {heap_engine}, lines made {heap_lines_made}, answer printed {heap_printed}, answer stored {heap_stored}");
        }
        // Written in place, never synced, as `check`'s profile (print.rs `profile`).
        crate::print::ensure_checkout_ignore(cx, root)?;
        let file = root.create(cx, &Path::new(print::PROFILE)?)?;
        file.append(cx, text.as_bytes(), bstd::fs::Durable::No)?;
        file.close(cx, bstd::fs::Durable::No)?;
        // PHASE-MEM-DEBT(CHECKED): `Checked` has no lifetime and outlives the request, so its lists
        // are on the process heap.
        let mut kept = blaze_own::Vec::new_in(heap.alloc());
        kept.extend(because.iter().copied());
        Ok(blaze_own::Box::new_in(Checked {
            status,
            tasks: u32::try_from(planned.len()).unwrap_or(u32::MAX),
            entered: report.entered,
            phases: ran,
            puts,
            stages: report.stages,
            because: kept,
            bodies: bodies.notes(&outlined, frame),
            times: phase_times,
            certified: false,
            from_last: read_from_last,
            unreadable,
            #[cfg(test)]
            undefined_reads,
            #[cfg(test)]
            heads: (head_rows, u32::try_from(heads.refused.len()).unwrap_or(u32::MAX), heads.read_back),
            #[cfg(test)]
            lint_rows,
            signatures_entered,
            lints: lints.seen(&outlined),
            ownerless: (bodies.ownerless, bodies.ownerless_paths(&outlined)),
            files: *file_counts,
            units: discovered.units,
            plan: discovered.plan,
            pages_written: saved.new_pages,
            records: blaze_own::Box::into_inner(ledger),
            sweep: sweep_counts,
            seeds: blaze_own::Box::into_inner(seed_counts),
            phase_mem: blaze_own::Box::into_inner(phase_mem),
            names_oracle: oracle,
            stored: Some(stored_keys),
            members_stood: bodies.members_stood(),
            member_rows: bodies.member_row_counts(),
        }, heap.alloc()))
    })
}

/// One stretch of [`checking`] run in a frame of its own: the stretch is a closure over the
/// request's locals, called here and nowhere else. Under a debug build every temporary of a
/// function keeps its own stack slot for the whole call, so a stretch written inline in
/// [`checking`] sat under every stage's frames however early it ended; called through here its
/// temporaries are this call's, gone when it returns (rule 54; piece C). `inline(never)`: an
/// optimized build keeps the same seam.
#[inline(never)]
fn own_frame<R>(stretch: impl FnOnce() -> R) -> R {
    stretch()
}

/// What the answer's tail hands on in place of the tests' audit ([`audit_answer`]) outside a
/// test build: nothing, by a name rather than `()`.
#[cfg(not(test))]
struct NotAudited;

/// The manifest a units record's findings are at bytes of (blaze-drive key.rs
/// `Name::text_file`): the root's `Cargo.toml` for the root's units ([`Name::units`]), else the
/// sweep row whose `Name::manifest` is `name` (a member's units are named by their manifest's
/// path, units.rs `lay`). `None`: no row is that manifest, and the finding is listed at its name.
fn manifest_path<'r>(rows: &'r Rows<'_>, name: Name) -> Option<&'r [u8]> {
    if name == Name::units() {
        return Some(b"Cargo.toml");
    }
    rows.rows.iter().map(|leaf| rows.path(leaf)).find(|path| path.ends_with("Cargo.toml") && Name::manifest(path.as_bytes()) == name).map(str::as_bytes)
}

/// The answer's lines (engine_lines.rs) and what making them found, by [`lines_of`].
struct Made<'a> {
    lines: crate::engine_lines::Lines<'a>,
    /// Rows with findings that stand and whose block the last record does not hold (a bug of
    /// the write): counted and said, never a quiet pass (rule 3).
    unprinted: u32,
    /// The first such task (by name), so the note names one (rule 19).
    first_unprinted: Option<Name>,
    /// Whether every standing body row's kept block says its body was checked whole.
    standing_bodies_whole: bool,
}

/// The stages [`lines_of`] reads the entered tasks' lines from: Names (when it ran), Scripts,
/// Bodies (its head notes made on demand), Heads and Lints.
type LineStages<'x, 'f, 'b> = (
    Option<&'x NamesStage<'f>>,
    &'x ScriptsStage<'f>,
    &'x mut crate::engine_bodies::BodiesStage<'b>,
    &'x crate::engine_heads::HeadsStage,
    &'x crate::engine_lints::LintsStage,
);

/// 3. Print, its first half: the whole answer's lines (engine_lines.rs), each entered task's
/// block made from its result now, each standing row's copied from the last request's record.
/// A call of its own, so its formatted words and `?` results are never in [`checking`]'s frame
/// (rule 54, bt4 2026-11-20).
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "the request's handles this step reads, each by its own name")]
fn lines_of<'a, 'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    laid: Option<&crate::units::Laid<'_>>,
    finished: &blaze_engine::Finished<'_>,
    last_lines: Option<&crate::engine_lines::Last<'a, '_>>,
    (names, scripts, bodies, heads, lints): LineStages<'_, 'f, '_>,
    // The request's outlines, and the plan notes of the files it did not read back (the ready
    // setup's, [`read_needed`]): each kept from the last record.
    (outlined, unread): (&[Outlined<'_>], &[Name]),
    discovered: &crate::engine_units::Discovered<'f>,
    // The sweep's rows: where a units record's manifest is ([`manifest_path`]).
    rows: &Rows<'_>,
    skipped: &Skipped<'_>,
    heap: &'static blaze_own::Heap,
    alloc: SharedAlloc<'a>,
    frame: &Frame,
) -> Result<Made<'a>, CliError> {
    #[cfg(test)]
    stack_probe::at("lines_of");
    let mut entered_now: SSet<'_, Name> = SSet::new_in(alloc);
    for one in finished.causes.iter() {
        entered_now.insert(one.name);
    }
    let mut lines = crate::engine_lines::Lines::new_in(alloc);
    let mut unprinted = 0u32;
    let mut first_unprinted: Option<Name> = None;
    // Whether types and borrows were checked whole is a fact of THE ANSWER, never of this
    // request's work: a standing body row (not entered now) whose kept block says its body was
    // not checked whole (refused, failed, not checked), or that has no block to say, makes the
    // stage not whole as a body entered now would (`bodies.whole()`). Read here, over the rows
    // with findings alone (a clean body has none), so O(rows with findings).
    let mut standing_bodies_whole = true;
    for name in finished.answer.with_findings.iter() {
        if !entered_now.contains(name) {
            match last_lines.and_then(|last| last.block(*name)) {
                Some(block) => {
                    // A block that does not decode is not known to be of a whole check: not
                    // whole (a pass is never implied, rule 3); `print_all` says its error.
                    if standing_bodies_whole && name.kind == blaze_drive::Kind::BodyCheck && crate::engine_lines::not_checked_whole(block).unwrap_or(true) {
                        standing_bodies_whole = false;
                    }
                    // A file's Told note is the tail's (a target its features skip), made or
                    // carried below: kept here too, it would print twice and grow a request.
                    match name.kind {
                        blaze_drive::Kind::FileItems => {
                            lines.keep_split(*name, block, false).map_err(EngineCheckError::Lines)?;
                        }
                        blaze_drive::Kind::Units => {
                            lines.keep_split_rule(*name, block, crate::unused_deps::UNUSED_DEPENDENCY, false).map_err(EngineCheckError::Lines)?;
                        }
                        _ => lines.keep(*name, block),
                    }
                }
                None => {
                    if name.kind == blaze_drive::Kind::BodyCheck {
                        standing_bodies_whole = false;
                    }
                    unprinted += 1;
                    first_unprinted.get_or_insert(*name);
                }
            }
            continue;
        }
        if name.kind == blaze_drive::Kind::BodyCheck {
            bodies.lines(*name, outlined, &mut lines, frame)?;
            continue;
        }
        if name.kind == blaze_drive::Kind::Lint {
            lints.lines(*name, outlined, &mut lines, frame)?;
            continue;
        }
        // A head task's heads not frozen, each with its reason (engine_head_notes.rs).
        if crate::engine_head_notes::is_head_task(*name) {
            if !bodies.heads_failed.ready {
                // PHASE-MEM-DEBT(Heads): as the Bodies phase's (`Failed::of`, `Alloc<'static>`).
                bodies.heads_failed = crate::engine_head_notes::Failed::of(heads, last_lines, heap.alloc())?;
            }
            bodies.heads_failed.note(heads, *name, &mut lines, frame);
            continue;
        }
        let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(name)) else {
            let one = match discovered.get(*name) {
                Some(one) => Some(one),
                None => names.and_then(|stage| stage.results.iter().find(|one| one.name == *name)).or_else(|| scripts.result(*name)),
            };
            let Some(one) = one else {
                unprinted += 1;
                first_unprinted.get_or_insert(*name);
                continue;
            };
            match &one.made {
                Ok(record) => {
                    // A member's units record's findings are at bytes of its manifest (units.rs
                    // `member_units`: a refused target's table, a toolchain line), so they are
                    // printed in that file: its path is the sweep row whose units name it is
                    // (`Name::manifest`). The root's units record names no file of its own.
                    let manifest = match name.kind {
                        Kind::Units if !record.findings.is_empty() => manifest_path(rows, *name),
                        _ => None,
                    };
                    for &(rule, start, end, from, to) in record.findings.iter() {
                        lines.finding(*name, rule, start, end, manifest, &record.messages[from..to]);
                    }
                }
                Err(failed) => match failed.failure.why {
                    // Its words its cause's name and its crate, as the old path files a wait
                    // (work.rs `filed_answer`): the printer groups the waiters under the cause.
                    Why::Waited { on } => {
                        let label = match name.kind {
                            Kind::CrateScope => names.and_then(|stage| stage.names.as_ref()).and_then(|world| world.crate_at(name.subject).map(|at| world.crate_label(at))),
                            _ => None,
                        };
                        lines.note(*name, crate::engine_lines::Class::Waited, crate::not_checked::waited::words(on, label, frame).as_bytes());
                    }
                    _ => {
                        let words = match failed.words {
                            Some(words) => bstd::mem::format_in!(frame, "{}: {words}", failed.failure),
                            None => bstd::mem::format_in!(frame, "{}", failed.failure),
                        };
                        lines.note(*name, crate::engine_lines::Class::Failure, words.as_bytes());
                    }
                },
            }
            continue;
        };
        let one = &outlined[at];
        match &one.made {
            Ok(record) => {
                for &(rule, start, end, from, to) in record.findings.iter() {
                    lines.finding(*name, rule, start, end, Some(&one.path), &record.messages[from..to]);
                }
            }
            Err(why) => {
                let path = core::str::from_utf8(&one.path).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
                let failure = blaze_drive::TaskFailure { name: *name, why: *why };
                lines.note(*name, crate::engine_lines::Class::Failure, bstd::mem::format_in!(frame, "`{path}`: its outline failed: {failure}").as_bytes());
            }
        }
    }
    // The plan's notes (a file no unit holds), no task's: made now, or kept (engine_body_plan.rs).
    bodies.plan_lines(outlined, unread, last_lines, &mut lines, frame);
    for target in skipped.targets.iter() {
        let path = outlined.binary_search_by(|one| one.name.cmp(&target.root)).ok().and_then(|at| core::str::from_utf8(&outlined[at].path).ok()).unwrap_or("?");
        let target_name = path.rsplit('/').next().unwrap_or(path).strip_suffix(".rs").unwrap_or(path);
        let package = names.and_then(|stage| stage.names.as_ref()).and_then(|names| {
            let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
            let list = names.list();
            (0..blaze_world::crate_list::CrateGraph::len(list))
                .find(|r| list.root(*r).strip_prefix("/0/").is_some_and(|root| root.rsplit_once('/').map_or("", |(directory, _)| directory) == directory))
                .map(|r| list.package(r))
        }).unwrap_or("?");
        lines.note(target.root, crate::engine_lines::Class::Told, bstd::mem::format_in!(frame, "bin `{target_name}` of package `{package}` is not built: its required feature is off; {} file(s) not checked", target.files).as_bytes());
    }
    // The ready path walks nothing: each skipped target's note as the last request printed it,
    // the Told entries of the last record's `FileItems` blocks (only the tail writes a file's
    // Told note), which the phase path makes again with the same words (no layout row moved).
    if skipped.carried {
        let last = last_lines.ok_or(EngineCheckError::ReadyRefused(crate::engine_ready_edit::ReadyRefused::Root(crate::engine_ready_edit::PhasesWhy::NoAnswer)))?;
        for &(name, block) in last.blocks_of(blaze_drive::Kind::FileItems) {
            lines.keep_split(name, block, true).map_err(EngineCheckError::Lines)?;
        }
    }
    // Unused-dependency warnings (unused_deps.rs): computed fresh on the Phases path where `laid`
    // holds every member's manifest metadata, and carried forward from the last lines record on
    // the Ready path (zero extra manifest I/O on body-only edits).
    match laid {
        Some(laid) => {
            let mut skipped_roots: Vec<&str, &Frame> = Vec::with_capacity_in(skipped.targets.len(), frame);
            for target in skipped.targets.iter() {
                if let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(&target.root))
                    && let Some(one) = outlined.get(at)
                    && let Ok(path) = core::str::from_utf8(&one.path)
                {
                    skipped_roots.push(path);
                }
            }
            crate::unused_deps::collect(cx, root, laid, names, finished, outlined, &skipped_roots, &mut lines, frame)?;
        }
        None => {
            if let Some(last) = last_lines {
                for &(name, block) in last.blocks_of(blaze_drive::Kind::Units) {
                    lines.keep_split_rule(name, block, crate::unused_deps::UNUSED_DEPENDENCY, true).map_err(EngineCheckError::Lines)?;
                }
            }
        }
    }
    // A build script whose outputs this command uses and does not keep says so, once (lane NR2).
    // None on the ready path: such a script's row is owed (engine_scripts.rs: `buffer.owed`),
    // and `choose` refuses an answer that owes (the phase path enters it and says it again).
    for (name, why) in scripts.unkept() {
        lines.note(name, crate::engine_lines::Class::Told, bstd::mem::format_in!(frame, "{}", crate::engine_scripts::UnkeptWords(name, why)).as_bytes());
    }
    Ok(Made { lines, unprinted, first_unprinted, standing_bodies_whole })
}

/// What [`print_answer`] printed and decided.
struct AnswerPrinted<'a> {
    tally: crate::engine_lines::Tally,
    /// The lines record's key and bytes, put with the answer.
    lines_key: blaze_drive::Key,
    lines_bytes: SVec<'a, u8>,
    status: Status,
    oracle: Option<crate::engine_names::OracleTotals>,
    /// Types and borrows checked whole: every body of the answer was.
    bodies_whole: bool,
}

/// 3. Print, its second half: the lines rendered and published, the notes, the coverage, the
/// closing line, the names oracle's sums; the command's status. A call of its own, so its
/// formatted notes and `?` results are never in [`checking`]'s frame (rule 54, bt4 2026-11-20).
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "the request's handles this step reads, each by its own name")]
fn print_answer<'m, 'f, 'a>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    view_slot: &crate::engine_lines::ViewSlot<'m, 'f>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    made: &Made<'a>,
    rows: &mut Rows<'_>,
    (names, bodies): (Option<&NamesStage<'_>>, &crate::engine_bodies::BodiesStage<'_>),
    finished: &blaze_engine::Finished<'_>,
    (outlined, files_checked): (&[Outlined<'_>], u32),
    unreadable: Option<&crate::engine_answer::Unreadable>,
    names_oracle: crate::engine_names::NamesOracle,
    (phases, began): (&Phases, bstd::clock::Instant),
    shown: &mut print::Shown<'f>,
    frame: &'f Frame,
    out: &Out,
) -> Result<AnswerPrinted<'a>, CliError> {
    #[cfg(test)]
    stack_probe::at("print_answer");
    let Made { lines, unprinted, first_unprinted, standing_bodies_whole } = made;
    let (unprinted, first_unprinted) = (*unprinted, *first_unprinted);
    // Render and publish the immutable view alongside the semantic lines record.  On the next
    // unchanged request `ViewSlot::certified` can transfer these bytes without decoding or
    // sorting the lines record again.
    let (tally, (lines_key, lines_bytes)) = view_slot.render(cx, root, store, lines, rows, shown, frame, out)?;
    let unchecked = tally.unchecked;
    if bodies.ownerless > 0 {
        print::note(cx, bstd::mem::format_in!(frame, "{} body(ies) in files no member crate's records hold were not checked (engine_bodies.rs)", bodies.ownerless).as_str(), shown, frame, out)?;
    }
    if unprinted > 0 {
        // The first by its kind and its file's path (the row's file: an outline's name).
        let first = if let Some(name) = first_unprinted {
            let file = finished.answer.row_facts(name).map_err(EngineError::from)?.map(|row| row.file);
            let path = file.and_then(|file| outlined.binary_search_by(|one| one.name.cmp(&file)).ok()).map_or("?", |at| core::str::from_utf8(&outlined[at].path).unwrap_or("?"));
            Some((name.kind, path))
        } else {
            None
        };
        let (kind, path) = first.unwrap_or((Kind::BodyCheck, "?"));
        print::note(
            cx,
            bstd::mem::format_in!(frame, "{unprinted} task(s) with findings, or failed, have no block in the lines record (engine_check.rs `check`, engine_lines.rs: no printer writes their kind, or the last request's record lacks it): they are not printed here; first: a {kind:?} task of `{path}`").as_str(),
            shown,
            frame,
            out,
        )?;
    }
    if let Some(why) = unreadable {
        print::note(cx, bstd::mem::format_in!(frame, "{why}").as_str(), shown, frame, out)?;
    }
    // Types and borrows are checked whole when every body of the answer was: those entered now
    // (engine_bodies.rs `whole`) and those that stand (`standing_bodies_whole`, above).
    let bodies_whole = names.is_some() && bodies.whole() && *standing_bodies_whole;
    let (ran_stages, not_stages) = crate::check_coverage(names.is_some(), bodies_whole, bodies_whole && tally.borrows == 0, frame);
    let coverage = (!not_stages.is_empty()).then_some(blaze_report::frame::Coverage { ran: &ran_stages, not: &not_stages });
    let silent_clean = shown.quiet_when_clean
        && shown.counts.errors == 0
        && shown.counts.warnings == 0
        && unchecked == 0
        && shown.behind.items == 0
        && unprinted == 0
        && unreadable.is_none();
    if !silent_clean {
        print::stage_notes(cx, &not_stages, shown, frame, out)?;
        // Never print "0 errors" or "no errors in" beside items blaze could not check
        // (`check_unchecked_items.rs`).
        let unchecked_only = (unchecked > 0 || shown.behind.items > 0) && shown.counts.errors == 0 && shown.counts.failures == 0;
        if !unchecked_only {
            print::closing(cx, shown, files_checked, phases.since(began), Some(finished.report.entered), None, coverage, frame, out)?;
        }
    }
    // The bodies not checked, under one line beside the closing line, as `check` prints it.
    if let Some(words) = crate::not_checked::items::summary(unchecked, shown.behind, frame) {
        print::note(cx, words.as_str(), shown, frame, out)?;
    }
    // `--names-oracle` (D158 step 1): its sums, its first disagreements and the crates it did
    // not compare, last; its finding fails the command, and nothing else of the answer moves.
    let oracle = match (names_oracle.on(), names) {
        (true, Some(stage)) => Some(stage.oracle_totals()),
        (true, None) => Some(crate::engine_names::OracleTotals::default()),
        (false, _) => None,
    };
    if let Some(t) = oracle {
        print::note(cx, bstd::mem::format_in!(frame, "names oracle: {} cell(s) compared in {} crate(s), {} disagreement(s) ({} cell difference(s), {} crate(s) whose names records disagree); {} name(s) compared against the names records, {} glob-reached not asked; {} wait(s), {} hop(s), {} module(s); {} crate(s) not compared", t.cells, t.crates, t.disagreements(), t.differences, t.link_differ, t.link_names, t.link_capped, t.waits, t.hops, t.modules, t.not_compared).as_str(), shown, frame, out)?;
        if let Some(stage) = names {
            let (listed, more) = stage.oracle_listed();
            for line in listed {
                print::note(cx, bstd::mem::format_in!(frame, "names oracle disagreement: {line}").as_str(), shown, frame, out)?;
            }
            if more > 0 {
                print::note(cx, bstd::mem::format_in!(frame, "names oracle: {more} more disagreement(s) counted, not listed (the first {} are; a crate's check keeps its first 8 cells)", crate::engine_names::ORACLE_LISTED).as_str(), shown, frame, out)?;
            }
            // `--block-scopes=bodies` (D189): its count line always, then its verdicts.
            if let Some(b) = t.blocks {
                print::note(cx, bstd::mem::format_in!(frame, "names oracle: {b}").as_str(), shown, frame, out)?;
                let (listed, more) = stage.blocks_listed();
                for line in listed {
                    print::note(cx, bstd::mem::format_in!(frame, "names oracle disagreement: {line}").as_str(), shown, frame, out)?;
                }
                if more > 0 {
                    print::note(cx, bstd::mem::format_in!(frame, "names oracle: {more} more block-scopes verdict(s) not listed (the first {} are)", crate::engine_names::ORACLE_LISTED).as_str(), shown, frame, out)?;
                }
            }
            for krate in stage.oracle_missed() {
                print::note(cx, bstd::mem::format_in!(frame, "names oracle: crate `{krate}` was not compared (failed or refused before its check reported)").as_str(), shown, frame, out)?;
            }
        }
        if let Some(finding) = t.finding() {
            print::note(cx, bstd::mem::format_in!(frame, "{finding}").as_str(), shown, frame, out)?;
        }
    }
    let status = match oracle.and_then(|t| t.finding()) {
        Some(_) => Status::FAILED,
        None => status_of(&tally, unprinted, shown, coverage.is_some()),
    };
    Ok(AnswerPrinted { tally, lines_key, lines_bytes, status, oracle, bodies_whole })
}

/// The certificate's print and commit: nothing the stored answer read moved, so its lines are
/// replayed (or printed again) and the journal's checkpoint and roots renewed. A call of its own
/// so its temporaries (the coverage, the tally, the formatted names) are never in
/// [`checking`]'s frame (rule 54, bt4 2026-11-20). The store is committed by the caller after.
#[inline(never)]
fn certified<'m, 'f, D: blaze_core::Driver, S: crate::store::Sharing>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    view_slot: &crate::engine_lines::ViewSlot<'m, 'f>,
    pointer: &crate::engine_answer::Pointer<'_>,
    lines: &crate::engine_lines::Last<'_, '_>,
    answer_root: &str,
    journal: (&crate::journal_checkpoint::Chosen<'_>, &crate::journal_checkpoint::Places<'_>, &mut crate::journal_checkpoint::Counts),
    store: &mut crate::store::CommandStore<'m, '_, D, S>,
    (tools, toolchain): (&blaze_engine::Tools, blaze_toolchain::ToolchainId),
    shown: &mut print::Shown<'f>,
    phases: &mut Phases,
    began: bstd::clock::Instant,
    request: SharedAlloc<'_>,
    frame: &'f Frame,
    out: &Out,
) -> Result<(Status, crate::engine_lines::Tally), CliError> {
    let (chosen, now_places, journal_counts) = journal;
    let facts = pointer.facts;
    // A rendered view is immutable under its semantic lines key and presentation.  A hit
    // transfers the stored stream in one write, avoiding a re-render of every note and finding.
    // Older answers, a different view, or a caller needing a listing take the ordinary semantic
    // printer below and publish a view on the next edit.
    let (tally, view_hit) = match view_slot.certified(cx, pointer.lines, shown, frame, out)? {
        Some(tally) => (tally, true),
        None => (view_slot.new_view(cx, root, store, pointer.lines, lines, shown, request, frame, out)?, false),
    };
    let whole = facts.printed.bodies_whole;
    let (ran_stages, not_stages) = crate::check_coverage(facts.printed.names, whole, whole && tally.borrows == 0, frame);
    let coverage = (!not_stages.is_empty()).then_some(blaze_report::frame::Coverage { ran: &ran_stages, not: &not_stages });
    let silent_clean = shown.quiet_when_clean
        && shown.counts.errors == 0
        && shown.counts.warnings == 0
        && tally.unchecked == 0
        && shown.behind.items == 0;
    if !silent_clean {
        print::stage_notes(cx, &not_stages, shown, frame, out)?;
        let unchecked_only = (tally.unchecked > 0 || shown.behind.items > 0) && shown.counts.errors == 0 && shown.counts.failures == 0;
        if !unchecked_only {
            print::closing(cx, shown, facts.files, phases.since(began), Some(0), None, coverage, frame, out)?;
        }
    }
    if let Some(words) = crate::not_checked::items::summary(tally.unchecked, shown.behind, frame) {
        print::note(cx, words.as_str(), shown, frame, out)?;
    }
    phases.mark_cx(cx, "answer printed (the certificate)", frame);
    let status = status_of(&tally, 0, shown, coverage.is_some());
    // Read through, so renewed (store/roots.rs, the census).
    let reach = bstd::mem::format_in!(frame, "{}.{}", answer_root, crate::engine_answer::REACH_ROOT);
    let answer_index = bstd::mem::format_in!(frame, "{}.{}", answer_root, crate::engine_answer::ANSWER_ROOT);
    // The certificate stood on this pointer at this replay's places: when due, they are kept as
    // its journal checkpoint, riding the commit after (journal_checkpoint.rs).
    journal_counts.written = crate::journal_checkpoint::after_certificate(cx, store, answer_root, pointer.key, chosen, now_places.stored(), crate::journal_checkpoint::DUE_EVENTS, frame)?;
    let journal_root = crate::journal_checkpoint::name(answer_root, frame);
    let seeds_root = crate::engine_seeds::root_name(tools, frame);
    let mut reach_shards = blaze_own::Vec::new_in(frame);
    crate::engine_answer::push_reach_shards(store, answer_root, frame, &mut reach_shards)?;
    let mut to_renew: blaze_own::Vec<&str, &Frame> = blaze_own::Vec::with_capacity_in(6 + reach_shards.len(), frame);
    to_renew.push(answer_root);
    to_renew.push(reach.as_str());
    to_renew.push(answer_index.as_str());
    to_renew.push(journal_root.as_str());
    if view_hit {
        to_renew.push(view_slot.name.as_str());
    }
    to_renew.push(seeds_root.as_str());
    for shard in &reach_shards {
        to_renew.push(shard.as_str());
    }
    store.renew(&to_renew, toolchain)?;
    crate::store::Store::after_answer(store, cx, &mut blaze_drive::profile::Profile::default())?;
    Ok((status, tally))
}

/// The certificate's profile and its [`Checked`]: a call of its own, so the result's lists and
/// the profile's text are never in [`checking`]'s frame (rule 54).
#[inline(never)]
fn certificate_checked(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    (status, tally): (Status, crate::engine_lines::Tally),
    sweep_counts: crate::journal_sweep::SweepCounts,
    journal_counts: &crate::journal_checkpoint::Counts,
    cleanup: &crate::store::roots::CleanupLine,
    phases: &Phases,
    heap: &'static blaze_own::Heap,
    frame: &Frame,
) -> Result<BoxedChecked, CliError> {
    let tail = tally.tail.line(tally.listed, frame);
    let text = bstd::mem::format_in!(frame, "certificate stood: nothing the stored answer read moved; no task entered, no page read (the lines record alone), nothing written but the roots renewed (and the journal's checkpoint when due)\n{sweep_counts}\n{journal_counts}\n{cleanup}\n{tail}\n{phases}");
    crate::print::ensure_checkout_ignore(cx, root)?;
    let file = root.create(cx, &Path::new(print::PROFILE)?)?;
    file.append(cx, text.as_bytes(), bstd::fs::Durable::No)?;
    file.close(cx, bstd::fs::Durable::No)?;
    // PHASE-MEM-DEBT(CHECKED): `Checked` outlives the request (main.rs prints it, the tests read
    // it) and has no lifetime, so its lists are on the process heap; boxed there ([`BoxedChecked`]).
    Ok(blaze_own::Box::new_in(Checked {
        status,
        tasks: 0,
        entered: 0,
        phases: blaze_own::Vec::new_in(heap.alloc()),
        puts: 0,
        stages: Default::default(),
        because: blaze_own::Vec::new_in(heap.alloc()),
        bodies: (std::vec::Vec::new_in(std::alloc::Global), 0),
        times: std::vec::Vec::new_in(std::alloc::Global),
        certified: true,
        from_last: true,
        unreadable: None,
        #[cfg(test)]
        undefined_reads: (Vec::new_in(heap.alloc()), 0, 0),
        #[cfg(test)]
        heads: (0, 0, 0),
        #[cfg(test)]
        lint_rows: 0,
        signatures_entered: 0,
        lints: crate::engine_lints::LintsSeen::default(),
        ownerless: (0, std::vec::Vec::new_in(std::alloc::Global)),
        files: Files::default(),
        units: crate::engine_units::Units::default(),
        plan: crate::engine_units::PlanRows::default(),
        pages_written: 0,
        records: crate::engine_store::Ledger::new(),
        sweep: sweep_counts,
        seeds: crate::engine_seeds::SeedCounts::default(),
        phase_mem: crate::engine_phase_mem::PhaseLedger::new(),
        // Never under `--names-oracle`: it answers in full, so no certificate stands.
        names_oracle: None,
        // No answer written: the roots renewed alone.
        stored: None,
        // No Bodies phase: no member set up.
        members_stood: (0, 0),
        member_rows: crate::engine_member_rows::RowCounts::default(),
    }, heap.alloc()))
}

/// The drive's state: what every stage of a request after its plan reads and writes, made once
/// and placed in the request's frame ([`checking`]), each stage a call taking it by `&mut`. So
/// [`checking`]'s own frame holds one pointer to it, and each stage's temporaries (its `?`
/// results, its struct literals, its format buffers) live in that stage's frame and die with
/// its call. The class this closes (rule 54, bt4 2026-11-20: `checking` 523 KB and its drive
/// closure 172 KB under the body checker's own 800 KB): one function whose every temporary of
/// every stage held a slot for the whole command. Only references and plain values: the frame
/// never runs a destructor, and this has none to run.
struct Drive<'d, 'f, 'r, 's, 'w> {
    cx: &'d Cx,
    root: &'d Dir<ReadWrite>,
    err: &'d Out,
    board: &'d crate::engine_status::Board<'d>,
    /// The phase running, read by [`check`] when the request fails.
    active: &'d mut Option<Phase>,
    phases: &'d mut Phases,
    frame: &'f Frame,
    /// The request's arena (engine_phase_mem.rs `RequestMem::shared`).
    alloc: SharedAlloc<'r>,
    heap: &'static blaze_own::Heap,
    /// By reference: one 32-byte tool a kind, too large to copy into each stage's frame.
    tools: &'d blaze_engine::Tools,
    workers: NonZeroU32,
    fresh: bool,
    heads_from: crate::engine_seeds::HeadsFrom,
    body_locals: blaze_ladder::front2::tables::seeds::BodyLocals,
    request_mem: &'d crate::engine_phase_mem::RequestMem,
    env: &'d FetchEnv<'d>,
    graph: Option<blaze_drive::Input>,
    outlined: &'d [Outlined<'r>],
    planned: &'d [Planned],
    discovered: &'d crate::engine_units::Discovered<'f>,
    /// The Names and Scripts stages' handles (engine_names.rs), shared with the pipeline.
    handles: &'d Handles<'s, 'f>,
    names: &'d mut Option<NamesStage<'f>>,
    scripts: &'d mut ScriptsStage<'f>,
    bodies: &'d mut crate::engine_bodies::BodiesStage<'f>,
    heads: &'d mut crate::engine_heads::HeadsStage,
    lints: &'d mut crate::engine_lints::LintsStage,
    pool: &'d mut Workers<'w>,
    buffers: &'d mut SVec<'r, Buffer<'r>>,
    times: &'d mut crate::engine_times::Times<'r>,
    ledger: &'d mut crate::engine_store::Ledger,
    ran: &'d mut blaze_own::Vec<(Phase, u32, u32), blaze_own::Alloc<'static>>,
    phase_times: &'d mut std::vec::Vec<crate::engine_times::PhaseTime, std::alloc::Global>,
    seed_counts: &'d mut crate::engine_seeds::SeedCounts,
    phase_mem: &'d mut crate::engine_phase_mem::PhaseLedger,
    /// A test's planted heap leak (rule 23's control, engine_phase_mem.rs `with_planted_leak`):
    /// kept to the command's end, as a stage that broke the rule would keep it.
    #[cfg(test)]
    planted_kept: &'d mut blaze_own::Vec<blaze_own::Vec<u8, blaze_own::Alloc<'static>>, blaze_own::Alloc<'static>>,
    /// Records put by every phase.
    puts: u32,
    /// Scripts and Names run through the pipeline (engine_scripts.rs `Pipeline`).
    pipelined: bool,
    /// What [`serve_found`] reads and where it leaves what the names said of the found files.
    serve: &'d ServeFound<'d, 'r>,
    /// Whether [`serve_found`] ran: once a request, before the first phase after the names.
    found_served: bool,
}

/// What [`serve_found`] reads beside the drive's tables, made by [`checking`] before the drive,
/// and where it leaves its answer ([`Served`]) for the drive and the answer's tail. One reference
/// in [`Drive`], so the drive stays small.
struct ServeFound<'d, 'r> {
    /// The lint rules a file's plan names (engine_ready_edit.rs `file_tasks`).
    rules: blaze_lint::RuleSet,
    /// The files of skipped targets only, sorted (their bodies are not planned).
    skipped: &'d [Name],
    /// The files of non-member checkout path packages only, sorted (outlined for `CrateScope`,
    /// never planned for bodies, heads or lints).
    external: &'d [Name],
    /// Each file's place from the walk ([`plan`]): a found file's names its last flags.
    places: &'d [(Name, blaze_engine::pages::FilePlace)],
    /// Set once, by [`serve_found`], only when what the names said moves the plan.
    served: core::cell::OnceCell<Served<'r>>,
}

/// What the names said of the found files (D161), applied ([`serve_found`]): the request's
/// outlines and plan from here on, and what the answer's tail keeps of them.
struct Served<'r> {
    /// Every outline of the request, sorted by name: the walk's, copied, and each found file's
    /// (a worker's, and the files its parts name), less each file dropped.
    outlined: SVec<'r, Outlined<'r>>,
    /// The request's plan, sorted by file then name: each affected file's group made again.
    planned: SVec<'r, Planned>,
    /// Each affected file's place now, sorted; `None` for a file dropped.
    places: SVec<'r, (Name, Option<blaze_engine::pages::FilePlace>)>,
    /// Each file outlined after the walk: its slot and probe, the leaf rows the next request's
    /// sweep proves it unchanged by (sweep.rs `opened`).
    probes: SVec<'r, (SVec<'r, u8>, sweep::Probed)>,
}

/// The drive stays a pointer's size in [`checking`]'s frame: its state is all references and
/// plain values (rule 54's guard; a by-value table added here is refused at compile time).
const _: () = assert!(core::mem::size_of::<Drive<'static, 'static, 'static, 'static, 'static>>() <= 640);

/// 2. Drive: each phase run, merged, its records put, until no phase is left.
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "drive handles, each named")]
fn drive<'d, 'f, 'r, 's, 'w, 'm>(
    rq: &mut Drive<'d, 'f, 'r, 's, 'w>,
    engine: &mut Engine<'r, Everything>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    read: &mut crate::engine_records::EngineRecords<'m>,
    records: &mut RecordKeys<'_>,
    pipeline: &mut crate::engine_scripts::Pipeline<'s, 'r>,
    last_lines: Option<&crate::engine_lines::Last<'_, '_>>,
    stored: Option<&crate::engine_answer::Stored<'_>>,
    files: &[(Name, &[Planned])],
) -> Result<(), CliError>
where
    'r: 's,
    'f: 's,
    'm: 's,
{
    let heads_snap: core::cell::OnceCell<crate::store::SharedSnapshot> = core::cell::OnceCell::new();
    let mut heads_lazy: Option<crate::front_load::ScopeRecords<'_>> = None;
    while one_phase(rq, engine, store, read, records, pipeline, last_lines, stored, files, &heads_snap, &mut heads_lazy)? {}
    Ok(())
}

/// The driver's scheduling point (engine_status.rs): a stop asked is the request's error.
fn stop_point(rq: &Drive<'_, '_, '_, '_, '_>) -> Result<(), CliError> {
    match rq.board.stopping() {
        Some(by) => Err(stopped(rq.cx, rq.root, rq.err, rq.board, by, &*rq.ledger, rq.names.as_ref(), &*rq.phases, &rq.env.clock, rq.frame)?),
        None => Ok(()),
    }
}

/// One phase: picked, run in its own arena, merged, its records put and committed, its arena
/// ended. `false` when no phase is left.
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "phase handles, each named")]
fn one_phase<'d, 'f, 'r, 's, 'w, 'm, 'l>(
    rq: &mut Drive<'d, 'f, 'r, 's, 'w>,
    engine: &mut Engine<'r, Everything>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    read: &mut crate::engine_records::EngineRecords<'m>,
    records: &mut RecordKeys<'_>,
    pipeline: &mut crate::engine_scripts::Pipeline<'s, 'r>,
    last_lines: Option<&crate::engine_lines::Last<'_, '_>>,
    stored: Option<&crate::engine_answer::Stored<'_>>,
    files: &[(Name, &[Planned])],
    heads_snap: &'l core::cell::OnceCell<crate::store::SharedSnapshot>,
    heads_lazy: &mut Option<crate::front_load::ScopeRecords<'l>>,
) -> Result<bool, CliError>
where
    'r: 's,
    'f: 's,
    'm: 's,
{
    use crate::engine_times::{Coordinator, ns};
    #[cfg(test)]
    stack_probe::at("one_phase");
    let (cx, board, alloc, heap) = (rq.cx, rq.board, rq.alloc, rq.heap);
    let env = rq.env;
    let clock = &env.clock;
    // The coordinator's stretches (engine_times.rs `Coordinator`): from here to the phase's
    // dispatch it plans the next round.
    let planning = clock.now();
    stop_point(rq)?;
    let mut prior = engine.next_phase_in(Stage::Units, &mut |_, _| true)?;
    if prior.is_none() { prior = engine.next_phase_in(Stage::Outlines, &mut |_, _| true)?; }
    rq.pipelined |= rq.names.is_some() && (engine.has_pending_stage(Stage::Scripts) || engine.has_pending_stage(Stage::Names));
    let mut mixed = None;
    if prior.is_none() && rq.pipelined && let Some(stage) = rq.names.as_mut() {
        pipeline.admit(cx, &mut *engine, &mut *rq.scripts, &mut *stage, &mut *store, &mut *read, rq.handles, alloc, rq.frame)?;
        mixed = pipeline.first(stage);
        if mixed.is_none() && (engine.has_pending_stage(Stage::Scripts) || engine.has_pending_stage(Stage::Names)) {
            return Err(CliError::Text { what: "Scripts and Names have pending readers with no runnable producer (engine_check.rs)" });
        }
    }
    // THE FOUND FILES SERVED (D161): once the names have all answered and before any later phase
    // opens (or the drive ends), what they said moves the plan ([`serve_found`]).
    if prior.is_none()
        && mixed.is_none()
        && !rq.found_served
        && ![Stage::Units, Stage::Outlines, Stage::Scripts, Stage::Names].into_iter().any(|stage| engine.has_pending_stage(stage))
    {
        serve_found(rq, engine, store, read, records)?;
    }
    let phase = match prior.or_else(|| mixed.map(|task| task.planned.phase)) {
        Some(phase) => phase,
        None => match engine.next_phase()? { Some(phase) => phase, None => return Ok(false) },
    };
    // The driver's scheduling point (engine_status.rs): a stop asked during the last merge or
    // put.
    stop_point(rq)?;
    *rq.active = Some(phase);
    // The phase's arena (engine_phase_mem.rs), made before its tasks run and ended once its
    // records are committed, below. A local of this call: a stop's return, an error's `?` or an
    // unwind drops it too, its pages back to the kernel.
    let request_mem = rq.request_mem;
    let mem = request_mem.phase(phase.stage)?;
    let at_phase = mem.shared();
    #[cfg(test)]
    if let Some(leak) = crate::engine_phase_mem::planted_leak(phase.stage, heap) {
        rq.planted_kept.push(leak);
    }
    // The phase's tasks: read to its end, never after.
    let mut entered: SVec<'_, Task> = SVec::new_in(at_phase);
    if mixed.is_none() { entered.extend_from_slice(engine.tasks()); }
    let mark = board.phase_began(phase.stage, entered.len() as u64, board.now(clock));
    let began = clock.now();
    rq.times.coordinator(Coordinator::Planning, ns(planning.since_start()), ns(began.since_start()));
    // The Bodies phase's run facts, in this phase's arena: made by `run_phase`, copied into the
    // put below, dropped with the phase's other buffers.
    let phase_facts = run_stage(rq, engine, store, read, records, pipeline, last_lines, stored, files, phase, mixed, &mut entered, at_phase, heads_snap, heads_lazy)?;
    // Pipelined: the coordinator filed finished Scripts and Names tasks in order; else it ran
    // the phase whole (its pool joined).
    let what = if mixed.is_some() { Coordinator::Filing } else { Coordinator::Phase };
    rq.times.coordinator(what, ns(began.since_start()), ns(clock.now().since_start()));
    board.phase_ended(mark, if mixed.is_some() { 0 } else { entered.len() as u64 }, board.now(clock));
    // Stopped while the phase ran: its tasks not begun have no result, so no merge. The phase's
    // arena is dropped by the return (engine_phase_mem.rs `PhaseMem`'s drop).
    stop_point(rq)?;
    // A status asked during the phase, written here when no watcher took it first (a plant's
    // always: a planted run starts none).
    if let Some(by) = board.take_status() {
        let mut text = blaze_own::String::new_in(rq.frame);
        let _ = crate::engine_status::write_status(board, crate::engine_status::Heading::Asked { by }, board.now(clock), &mut text);
        crate::engine_status::put(cx, rq.root, rq.err, text.as_bytes())?;
    }
    let merging = clock.now();
    if mixed.is_some() { engine.merge_tasks(phase, &mut *rq.buffers)?; } else { engine.merge(&mut *rq.buffers)?; }
    let merge = clock.now().since(merging).map_or(0, ns);
    let merged_from = ns(merging.since_start());
    rq.times.coordinator(Coordinator::Merge, merged_from, merged_from.saturating_add(merge));
    let count = u32::try_from(entered.len()).unwrap_or(u32::MAX);
    // Every phase's names and tables tasks, for the read grains' facts (`grains_from`).
    rq.bodies.note_entered(entered.iter().map(|task| task.planned.name), heap.alloc());
    rq.ran.push((phase, 0, count));
    // The phase's records to the store, sorted by key, before the next phase reads them.
    let putting = clock.now();
    put_phase(rq, engine, store, read, records, phase, &entered, phase_facts.as_ref(), &mem)?;
    let folding = clock.now();
    rq.times.coordinator(Coordinator::Put, ns(putting.since_start()), ns(folding.since_start()));
    rq.phase_times.push(rq.times.fold(phase, ns(began.since_start()), ns(merging.since_start()), merge, folding.since(putting).map_or(0, ns)));
    // The phase's records are committed: its arena ends here. A value over `at_phase` still
    // alive at `end` is a borrow across its move, refused at compile time (engine_phase_mem.rs's
    // doc).
    drop(entered);
    drop(phase_facts);
    let row = mem.end();
    board.phase_mem(row.stage, rq.phase_mem.add(row));
    rq.times.coordinator(Coordinator::Arena, ns(folding.since_start()), ns(clock.now().since_start()));
    *rq.active = None;
    Ok(true)
}

/// THE FOUND FILES SERVED (D161, engine invariant 14): once a request on the phase path, when
/// every names task has answered and before any later phase opens. Each crate whose names ran
/// said which found files it read and outlined each file it read that no outline held
/// (engine_names.rs `FoundBy`); a crate that stood or failed said nothing, so its unit keeps its
/// flag. From that:
/// 1. each found file's flags (a unit's [`found_bit`] where its names read it) and its one root
///    ([`EngineCheckError::FoundTwoRoots`]), each new file's one outline
///    ([`EngineCheckError::FoundTwoOutlines`]); when none of it moved, nothing more is done (a
///    replay enters nothing);
/// 2. the new files' parts outlined here as the walk outlines a file's (`wants`, `outline_one`),
///    so a cfg-off child is outlined as under a literal `mod`, and every outline in one table;
/// 3. the reach made again with today's found edges (engine_body_plan.rs `found_edges`), and the
///    files' plans (engine_ready_edit.rs `file_tasks`);
/// 4. each affected file (the parts' closure of the found files then and now), in name order:
///    one no live file or root reaches is dropped (`Engine::drop_file`), a new one is filed with
///    its outline (`Engine::plan_file`), one whose plan moved is planned again with none;
/// 5. the filed outlines put as an Outlines phase's are, and the drive's tables re-pointed.
#[inline(never)]
fn serve_found<'d, 'f, 'r, 's, 'w, 'm>(
    rq: &mut Drive<'d, 'f, 'r, 's, 'w>,
    engine: &mut Engine<'r, Everything>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    read: &mut crate::engine_records::EngineRecords<'m>,
    records: &mut RecordKeys<'_>,
) -> Result<(), CliError> {
    use blaze_engine::pages::FilePlace;
    rq.found_served = true;
    let (cx, alloc, heap, serve) = (rq.cx, rq.alloc, rq.heap, rq.serve);
    let old: &[(Name, FilePlace)] = rq.handles.found;
    let Some(stage) = rq.names.as_ref() else { return Ok(()) };
    let by = stage.found();
    // No found file then and none now: the request of every project with no computed module.
    if old.is_empty() && by.iter().all(|one| one.outlined.is_empty()) {
        return Ok(());
    }
    let Some(list) = stage.names.as_ref().map(|world| world.list()) else { return Ok(()) };
    let units = crate::engine_body_plan::found_units(list, heap.alloc());
    // Every checkout crate in the list (workspace members and non-member checkout path packages):
    // both read files from tree 0 and may find macro-written modules or computed data includes.
    let mut checkout_units: SVec<'_, (Name, bool, Name, bool)> = SVec::new_in(alloc);
    for &(root, test, unit) in units.iter() {
        checkout_units.push((root, test, unit, false));
    }
    for at in 0..blaze_world::crate_list::CrateGraph::len(list) {
        if !list.member(at)
            && let Some(path) = list.root(at).strip_prefix("/0/")
        {
            let root = Name::file(path.as_bytes());
            let unit = blaze_drive::key::crate_scope_name(&list.key(at));
            checkout_units.push((root, list.test(at), unit, true));
        }
    }
    checkout_units.sort_unstable();
    let unit_of = |unit: Name| checkout_units.iter().find(|one| one.2 == unit).map(|one| (one.0, one.1, one.3));
    // 1. Each found file's root and flags now: a unit whose names answered gives its flag where
    // they read the file; one that stood or failed keeps its flag; one the list no longer has
    // gives none.
    let mut masks: SMap<'_, Name, (Name, u8)> = SMap::new_in(alloc);
    for one in by.iter() {
        let Some((root, test, _)) = unit_of(one.unit) else {
            // A crate no checkout unit is: a fetched or toolchain crate, whose names read its
            // pack and list no checkout read (job_texts.rs `read`), so it finds nothing. A
            // checkout read listed by one is the stop of a read no outline holds, named (rule 3).
            if let Some(file) = one.read.first() {
                return Err(EngineCheckError::Unseen { task: one.unit, read: *file }.into());
            }
            continue;
        };
        for file in one.read.iter() {
            found_mark(&mut masks, *file, root, found_bit(test))?;
        }
    }
    for (file, place) in old.iter() {
        for test in [false, true] {
            if place.flags & found_bit(test) == 0 {
                continue;
            }
            let from = checkout_units.partition_point(|one| (one.0, one.1) < (place.unit, test));
            let standing = checkout_units
                .iter()
                .skip(from)
                .take_while(|one| (one.0, one.1) == (place.unit, test))
                .any(|one| by.binary_search_by_key(&one.2, |b| b.unit).is_err());
            if standing {
                found_mark(&mut masks, *file, place.unit, found_bit(test))?;
            }
        }
    }
    // Each new file's one outline, with its probe, its finder's root and whether its finder is
    // external: two finders' (the non-test and test units of one root) must be equal.
    let mut fresh: SVec<'_, (&Outlined<'static>, &(SVec<'static, u8>, sweep::Probed, bool), Name, bool)> = SVec::new_in(alloc);
    for one in by.iter() {
        let Some((root, _, external)) = unit_of(one.unit) else { continue };
        fresh.extend(one.outlined.iter().zip(one.probes.iter()).map(|(outline, probe)| (outline, probe, root, external)));
    }
    fresh.sort_unstable_by_key(|(outline, _, root, external)| (outline.name, *root, *external));
    let mut news: SVec<'_, (&Outlined<'static>, &(SVec<'static, u8>, sweep::Probed, bool), Name, bool)> = SVec::new_in(alloc);
    for one in fresh.iter().copied() {
        if let Some(last) = news.last().copied()
            && last.0.name == one.0.name
        {
            if last.2 != one.2 {
                return Err(EngineCheckError::FoundTwoRoots { file: one.0.name, first: last.2, second: one.2 }.into());
            }
            if !same_outline(last.0, one.0) || last.1.2 != one.1.2 {
                return Err(EngineCheckError::FoundTwoOutlines { file: one.0.name }.into());
            }
            continue;
        }
        news.push(one);
    }
    let moved = !news.is_empty() || masks.len() != old.len() || old.iter().any(|(file, place)| masks.get(file) != Some(&(place.unit, place.flags & PLACE_FOUND)));
    if !moved {
        return Ok(());
    }
    // 2. Every outline in one table: the walk's copied (COST: the outlines' bytes, once on a
    // request whose found files moved; the append-only table is the follow-up), each new
    // file's, and each file their parts name, outlined here.
    let mut merged: SVec<'r, Outlined<'r>> = SVec::new_in(alloc);
    for one in rq.outlined.iter() {
        merged.push(outline_copy(one, alloc)?);
    }
    let mut probes: SVec<'r, (SVec<'r, u8>, sweep::Probed)> = SVec::new_in(alloc);
    // Each file outlined here, its place but for the found flags, sorted below.
    let mut new_places: SVec<'_, (Name, FilePlace)> = SVec::new_in(alloc);
    // Track Rust and `DATA_FILE` names separately so a macro-found Rust module (or one of its
    // children) upgrades an existing `DATA_FILE` outline instead of being dropped.
    let mut seen_rust: SSet<'r, Name> = SSet::new_in(alloc);
    let mut seen_data: SSet<'r, Name> = SSet::new_in(alloc);
    for one in merged.iter() {
        if crate::data_include::is_data(one.edition) {
            seen_data.insert(one.name);
        } else {
            seen_rust.insert(one.name);
        }
    }
    for (outline, ..) in news.iter() {
        if crate::data_include::is_data(outline.edition) {
            seen_data.insert(outline.name);
        } else {
            seen_rust.insert(outline.name);
        }
    }
    let mut upgraded: SSet<'_, Name> = SSet::new_in(alloc);
    let mut wave: SVec<'r, Want<'r>> = SVec::new_in(alloc);
    let mut pending_data: SVec<'r, Want<'r>> = SVec::new_in(alloc);
    for (outline, probe, root, external) in news.iter().copied() {
        let copy = outline_copy(outline, alloc)?;
        if let Ok(record) = &copy.made {
            wants(&record.parts, &record.paths, &record.path_bytes, Some(root), external, &mut seen_rust, &mut seen_data, &mut wave, &mut pending_data, alloc)?;
        }
        let flags = (if probe.2 { PLACE_MODULE } else { 0 }) | (if external { PLACE_EXTERNAL } else { 0 });
        new_places.push((copy.name, FilePlace { unit: root, edition: copy.edition, flags }));
        probes.push((SVec::from_slice_in(&probe.0, alloc), probe.1));
        if let Some(existing) = merged.iter_mut().find(|one| one.name == copy.name) {
            upgraded.insert(copy.name);
            *existing = copy;
        } else {
            merged.push(copy);
        }
    }
    let mut data_walked = false;
    if wave.is_empty() && !pending_data.is_empty() {
        data_walked = true;
        for want in core::mem::replace(&mut pending_data, SVec::new_in(alloc)) {
            if !seen_rust.contains(&want.name) {
                wave.push(want);
            }
        }
    }
    while !wave.is_empty() {
        let mut next: SVec<'r, Want<'r>> = SVec::new_in(alloc);
        for want in wave.iter() {
            let (one, probed) = outline_one(cx, rq.root, &rq.handles.stamps, want, None, rq.handles.plants.lints, alloc).map_err(|stopped| stopped.of(want))?;
            if let Ok(record) = &one.made {
                wants(&record.parts, &record.paths, &record.path_bytes, Some(want.root), want.external, &mut seen_rust, &mut seen_data, &mut next, &mut pending_data, alloc)?;
            }
            let flags = (if want.module { PLACE_MODULE } else { 0 }) | (if want.external { PLACE_EXTERNAL } else { 0 });
            new_places.push((want.name, FilePlace { unit: want.root, edition: want.edition, flags }));
            probes.push((SVec::from_slice_in(&want.slot, alloc), probed));
            if let Some(existing) = merged.iter_mut().find(|entry| entry.name == one.name) {
                upgraded.insert(one.name);
                *existing = one;
            } else {
                merged.push(one);
            }
        }
        wave = next;
        if wave.is_empty() && !data_walked {
            data_walked = true;
            for want in core::mem::replace(&mut pending_data, SVec::new_in(alloc)) {
                if !seen_rust.contains(&want.name) {
                    wave.push(want);
                }
            }
        }
    }
    merged.sort_unstable_by_key(|one| one.name);
    new_places.sort_unstable_by_key(|(file, _)| *file);
    let place_of = |file: Name| -> Option<FilePlace> {
        match new_places.binary_search_by_key(&file, |(name, _)| *name) {
            Ok(at) => Some(new_places[at].1),
            Err(_) => serve.places.iter().find(|(name, _)| *name == file).map(|(_, place)| *place),
        }
    };
    // Each found file's place now, sorted: its place but for the flags, which are the names'.
    let mut found_now: SVec<'_, (Name, FilePlace)> = SVec::new_in(alloc);
    for (file, (root, flags)) in masks.iter() {
        let place = place_of(*file).ok_or(EngineCheckError::MissingEntry { task: *file, table: Table::FoundServed, key: *file })?;
        found_now.push((*file, FilePlace { unit: *root, edition: place.edition, flags: (place.flags & !PLACE_FOUND) | *flags }));
    }
    let edges = crate::engine_body_plan::found_edges(&units, &found_now, heap.alloc());
    // The affected files: the parts' closure of the found files then and now.
    let mut affected: SSet<'_, Name> = SSet::new_in(alloc);
    let mut stack: SVec<'_, Name> = SVec::new_in(alloc);
    stack.extend(old.iter().map(|(file, _)| *file));
    stack.extend(masks.keys().copied());
    stack.extend(new_places.iter().map(|(file, _)| *file));
    while let Some(file) = stack.pop() {
        if !affected.insert(file) {
            continue;
        }
        let Ok(at) = merged.binary_search_by_key(&file, |one| one.name) else { continue };
        if let Ok(record) = &merged[at].made {
            stack.extend(record.parts.iter().filter(|part| matches!(part.locate, Locate::File { .. })).map(|part| part.name));
        }
    }
    // Live: a found file now, a checkout root, a file a part of a file outside the affected
    // ones names, and a file a part of a live one names; an affected file with no outline is not.
    let mut live: SSet<'_, Name> = SSet::new_in(alloc);
    live.extend(found_now.iter().map(|(file, _)| *file));
    live.extend(checkout_units.iter().map(|one| one.0).filter(|root| affected.contains(root)));
    let mut inner: SVec<'_, (Name, Name)> = SVec::new_in(alloc);
    for one in merged.iter() {
        let Ok(record) = &one.made else { continue };
        for part in record.parts.iter().filter(|part| matches!(part.locate, Locate::File { .. }) && affected.contains(&part.name)) {
            if affected.contains(&one.name) {
                inner.push((one.name, part.name));
            } else {
                live.insert(part.name);
            }
        }
    }
    loop {
        let before = live.len();
        for (parent, child) in inner.iter() {
            if live.contains(parent) {
                live.insert(*child);
            }
        }
        if live.len() == before {
            break;
        }
    }
    live.retain(|file| merged.binary_search_by_key(file, |one| one.name).is_ok());
    merged.retain(|one| !affected.contains(&one.name) || live.contains(&one.name));
    let mut external_now: SVec<'_, Name> = SVec::new_in(alloc);
    external_now.extend(serve.external.iter().copied());
    external_now.extend(new_places.iter().filter(|(_, place)| place.flags & PLACE_EXTERNAL != 0).map(|(file, _)| *file));
    external_now.sort_unstable();
    external_now.dedup();
    // 3. The reach with today's found edges, and each affected file's plan from it.
    let reach = rq.bodies.plan_reach(cx, &mut *store, rq.graph, Some(list), &merged, &edges, crate::engine_bodies::ReachFrom::Walk, heap.alloc())?;
    let mut plan_now: SVec<'r, Planned> = SVec::new_in(alloc);
    crate::engine_ready_edit::file_tasks(&merged, serve.skipped, &external_now, true, &serve.rules, reach, alloc, &mut plan_now);
    // The request's plan: each affected file's tasks of the stages `file_tasks` plans are
    // today's; a dropped file has none.
    let mut planned: SVec<'r, Planned> = SVec::new_in(alloc);
    planned.extend(rq.planned.iter().filter(|one| !affected.contains(&one.file) || (live.contains(&one.file) && !matches!(one.phase.stage, Stage::Outlines | Stage::Heads | Stage::Bodies | Stage::Lints))).copied());
    planned.extend(plan_now.iter().filter(|one| affected.contains(&one.file) && live.contains(&one.file)).copied());
    planned.sort_unstable_by_key(|one| (one.file, one.name));
    drop(plan_now);
    let mut places: SVec<'r, (Name, Option<FilePlace>)> = SVec::new_in(alloc);
    for file in affected.iter().copied() {
        let place = match (live.contains(&file), found_now.binary_search_by_key(&file, |(name, _)| *name)) {
            (false, _) => None,
            (true, Ok(at)) => Some(found_now[at].1),
            (true, Err(_)) => Some(place_of(file).map(|place| FilePlace { unit: place.unit, edition: place.edition, flags: place.flags & !PLACE_FOUND }).ok_or(EngineCheckError::MissingEntry { task: file, table: Table::FoundServed, key: file })?),
        };
        places.push((file, place));
    }
    drop(news);
    drop(fresh);
    let first = affected.first().copied().unwrap_or(Name::units());
    serve.served.set(Served { outlined: merged, planned, places, probes }).map_err(|_| EngineCheckError::Twice { task: first })?;
    let served = serve.served.get().ok_or(EngineCheckError::MissingEntry { task: first, table: Table::FoundServed, key: first })?;
    // 4. Each affected file in name order: dropped, filed, or planned again.
    let tasks = Tasks { outlined: &served.outlined };
    let mut filed: SVec<'r, Task> = SVec::new_in(alloc);
    for file in affected.iter().copied() {
        let before = file_group(rq.planned, file);
        if !live.contains(&file) {
            if !before.is_empty() {
                engine.drop_file(file)?;
            }
            continue;
        }
        let now = file_group(&served.planned, file);
        let upgraded_file = upgraded.contains(&file);
        if rq.outlined.binary_search_by_key(&file, |one| one.name).is_err() || upgraded_file {
            if upgraded_file {
                engine.drop_file(file)?;
            }
            let own = now.iter().find(|one| one.name == file).copied().ok_or(EngineCheckError::MissingEntry { task: file, table: Table::FoundServed, key: file })?;
            let task = Task { planned: own, key: rq.tools.key(file, &own.source) };
            let merged_before = engine.merged().len();
            engine.plan_file(file, Some(tasks.outline(&task, alloc)), now)?;
            if engine.merged().len() > merged_before {
                filed.push(task);
            }
        } else if now != before {
            engine.plan_file(file, None, now)?;
        }
    }
    // 5. The drive reads the served tables from here; the filed outlines are put as an
    // Outlines phase's are, so a later phase and the next request read them by key.
    rq.outlined = &served.outlined[..];
    rq.planned = &served.planned[..];
    if !filed.is_empty() {
        rq.bodies.note_entered(filed.iter().map(|task| task.planned.name), heap.alloc());
        let request_mem = rq.request_mem;
        let mem = request_mem.phase(Stage::Outlines)?;
        put_phase(rq, &*engine, store, read, records, Phase::new(Stage::Outlines, 0), &filed, None, &mem)?;
        drop(filed);
        let row = mem.end();
        rq.board.phase_mem(row.stage, rq.phase_mem.add(row));
    }
    Ok(())
}

/// Marks `file` found by a unit of `root` ([`serve_found`]): one root a found file
/// ([`EngineCheckError::FoundTwoRoots`]).
fn found_mark(masks: &mut SMap<'_, Name, (Name, u8)>, file: Name, root: Name, bit: u8) -> Result<(), EngineCheckError> {
    let entry = masks.entry(file).or_insert((root, 0));
    if entry.0 != root {
        return Err(EngineCheckError::FoundTwoRoots { file, first: entry.0, second: root });
    }
    entry.1 |= bit;
    Ok(())
}

/// Whether two finders' outlines of one file are one outline: the same text, form and answer.
fn same_outline(a: &Outlined<'_>, b: &Outlined<'_>) -> bool {
    a.source == b.source
        && a.outline_source == b.outline_source
        && a.edition == b.edition
        && match (&a.made, &b.made) {
            (Ok(a), Ok(b)) => a.out == b.out,
            (Err(a), Err(b)) => a == b,
            _ => false,
        }
}

/// The tasks of `plan` (sorted by file) whose file is `file`.
fn file_group(plan: &[Planned], file: Name) -> &[Planned] {
    let from = plan.partition_point(|one| one.file < file);
    let len = plan[from..].partition_point(|one| one.file == file);
    &plan[from..from + len]
}

/// A copy of `one` in `alloc` ([`serve_found`]'s one table): its path and every list of its
/// record, its payload as built. An outline has no records beside its own; one that has is
/// refused by name (rule 3), never copied short.
fn outline_copy<'r>(one: &Outlined<'_>, alloc: SharedAlloc<'r>) -> Result<Outlined<'r>, CliError> {
    let made = match &one.made {
        Err(why) => Err(*why),
        Ok(record) => {
            if !record.extras.records.is_empty() {
                return Err(EngineCheckError::MissingEntry { task: one.name, table: Table::FoundServed, key: record.name }.into());
            }
            let mut copy = work::Record::new_in(record.name, alloc);
            copy.out = record.out;
            copy.parts.extend_from_slice(&record.parts);
            copy.findings.extend_from_slice(&record.findings);
            copy.messages.extend_from_slice(&record.messages);
            copy.payload.extend_from_slice(record.payload_bytes()?);
            copy.paths.extend_from_slice(&record.paths);
            copy.path_bytes.extend_from_slice(&record.path_bytes);
            copy.reads.extend_from_slice(&record.reads);
            copy.cells.extend_from_slice(&record.cells);
            copy.body_facts.extend_from_slice(&record.body_facts);
            copy.covered.extend_from_slice(&record.covered);
            Ok(copy)
        }
    };
    let mut path = SVec::new_in(alloc);
    path.extend_from_slice(&one.path);
    Ok(Outlined { name: one.name, path, source: one.source, outline_source: one.outline_source, edition: one.edition, made })
}

/// The phase's tasks run by their stage: filed from the pipeline, on the pool, or by the
/// stage's own driver. The Bodies phase hands back its run facts, in the phase's arena.
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "stage dispatch handles, each named")]
fn run_stage<'d, 'f, 'r, 's, 'w, 'm, 'p, 'l>(
    rq: &mut Drive<'d, 'f, 'r, 's, 'w>,
    engine: &mut Engine<'r, Everything>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    read: &mut crate::engine_records::EngineRecords<'m>,
    records: &RecordKeys<'_>,
    pipeline: &mut crate::engine_scripts::Pipeline<'s, 'r>,
    last_lines: Option<&crate::engine_lines::Last<'_, '_>>,
    stored: Option<&crate::engine_answer::Stored<'_>>,
    files: &[(Name, &[Planned])],
    phase: Phase,
    mixed: Option<Task>,
    entered: &mut SVec<'_, Task>,
    at_phase: SharedAlloc<'p>,
    heads_snap: &'l core::cell::OnceCell<crate::store::SharedSnapshot>,
    heads_lazy: &mut Option<crate::front_load::ScopeRecords<'l>>,
) -> Result<Option<crate::engine_bodies::MadeFacts<'p>>, CliError>
where
    'r: 's,
    'f: 's,
    'm: 's,
{
    #[cfg(test)]
    stack_probe::at("run_stage");
    let (cx, alloc, frame) = (rq.cx, rq.alloc, rq.frame);
    let env = rq.env;
    let clock = &env.clock;
    match (mixed, phase.stage) {
        (Some(first), _) => {
            let stage = rq.names.as_mut().ok_or(EngineCheckError::MissingEntry { task: first.planned.name, table: Table::NamesWorld, key: first.planned.name })?;
            pipeline.collect(cx, &mut *rq.scripts, stage, &mut *store, rq.handles, first, entered, &mut rq.buffers[0], &mut *rq.times, alloc, frame)?;
        }
        (None, Stage::Outlines) => outlines_phase(rq, engine, &entered[..])?,
        (None, Stage::Units) => {
            let discovered = rq.discovered;
            for task in entered.iter() {
                let one = clock.now();
                // A task failed outside the checkout (a fetch, the graph that waited on it) is
                // owed to the next command (fetch.md, "The rule for a failure outside the
                // checkout"), so the certificate does not stand on it (`facts.owed`).
                rq.buffers[0].push(publish(discovered, task, alloc), discovered.stands(task.planned.name));
                rq.times.interval(task.planned.name, crate::engine_times::ns(one.since_start()), crate::engine_times::ns(clock.now().since_start()));
            }
        }
        // PHASE-MEM-DEBT(Names): the stage takes the request's handle (its `Done`s merge into
        // the answer) and no phase handle, and puts its results (`NamesStage::results`: read
        // lists, findings, expansions) on the request frame and the process heap
        // (`handles.env.heap`), kept to the command's end for the put and the print. To take
        // `at_phase: SharedAlloc<'p>` for all but its `Done`s, and read its findings back from
        // the lines record.
        (None, Stage::Names) => match rq.names.as_mut() {
            Some(stage) => stage.run_phase(cx, &mut *store, &mut *read, engine.view(), rq.handles, &entered[..], &mut rq.buffers[0], &mut *rq.times, alloc, frame)?,
            None => rq.buffers[0].done.extend(entered.iter().map(|task| crate::engine_tasks::failed(task, alloc))),
        },
        (None, Stage::Bodies) => return bodies_phase(rq, engine, store, read, records, last_lines, stored, files, &entered[..], at_phase).map(Some),
        // PHASE-MEM-DEBT(Scripts): as Names': `ScriptsStage::results` on the request frame and
        // the process heap, kept to the command's end; to take `at_phase`.
        (None, Stage::Scripts) => rq.buffers[0].done.extend(entered.iter().map(|task| crate::engine_tasks::failed(task, alloc))),
        (None, Stage::Heads) => heads_phase(rq, engine, store, read, &entered[..], heads_snap, heads_lazy)?,
        // PHASE-MEM-DEBT(Lints): `LintsStage::made` on the process heap, kept to the command's
        // end for the put and the print; to take `at_phase`.
        (None, Stage::Lints) => {
            lint_priors(cx, &mut *store, records, rq.fresh, &entered[..], &mut *rq.lints)?;
            rq.lints.run_phase(cx, rq.root, rq.outlined, rq.workers, clock, &entered[..], &mut rq.buffers[0], &mut *rq.times, alloc)?
        }
    }
    Ok(None)
}

/// Each entered `Lint` task's last record (the stored answer's row's, by `records`, before this
/// request puts its own) handed to the stage as what its file's units may be kept by key from
/// (engine_lints.rs `Prior`, sparse rows: rule 77). A memo, never a read of the task: a fresh
/// request reads none, and a task with no last row lints every unit. A row naming a record the
/// store does not hold is a named stop (rule 3), never a task of no memo.
fn lint_priors(
    cx: &Cx,
    store: &mut crate::store::CommandStore<'_, '_, bstd::io::LentDriver>,
    records: &RecordKeys<'_>,
    fresh: bool,
    tasks: &[Task],
    lints: &mut crate::engine_lints::LintsStage,
) -> Result<(), CliError> {
    if fresh {
        return Ok(());
    }
    for task in tasks.iter() {
        let name = task.planned.name;
        let Some(key) = records.get(name)? else { continue };
        let record = match crate::store::Store::get(&mut *store, cx, key)? {
            None => return Err(EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name, key }).into()),
            Some(held) => crate::engine_store::outline_from(name, key, held.bytes, std::alloc::Global).map_err(EngineCheckError::OutlineBack)?,
        };
        lints.remember(name, &record);
    }
    Ok(())
}

/// An Outlines phase: its tasks on the pool's workers, each timed on its worker.
#[inline(never)]
fn outlines_phase<'r>(rq: &mut Drive<'_, '_, 'r, '_, '_>, engine: &Engine<'r, Everything>, entered: &[Task]) -> Result<(), CliError> {
    #[cfg(test)]
    stack_probe::at("outlines_phase");
    let (alloc, outlined) = (rq.alloc, rq.outlined);
    let env = rq.env;
    let clock = &env.clock;
    let runner = Tasks { outlined };
    let view = engine.view();
    let bounds = &rq.times.workers;
    rq.pool.run(entered, &mut *rq.buffers, &|cx: &Cx, task: &Task, buffer: &mut Buffer<'_>| {
        let _wall = bounds.start(clock);
        runner.work(cx, view, task, buffer, alloc)
    });
    for (task, ns) in entered.iter().zip(rq.pool.times()) {
        rq.times.push(task.planned.name, *ns);
    }
    // A pool's failure first: the merge's refusal after it is its consequence.
    rq.pool.failed().map_err(CliError::Pool)
}

/// Builds a sorted in-memory overlay of the `Signature` rows updated in `changes` during this
/// request's `Stage::Heads` so `PagedHeads` stays active even when heads moved.
#[inline(never)]
fn build_heads_overlay(
    changes: &SMap<'_, Name, Option<blaze_drive::Key>>,
    view: &blaze_engine::View<'_, '_>,
    heap: &'static blaze_own::Heap,
) -> Result<blaze_own::Vec<(Name, crate::engine_seeds::HeadEntry), SharedAlloc<'static>>, CliError> {
    let mut overlay = blaze_own::Vec::new_in(heap.shared());
    for (&name, &maybe_key) in changes.iter() {
        if name.kind != Kind::Signature {
            continue;
        }
        let entry = match view.row(name) {
            None => crate::engine_seeds::HeadEntry::None,
            Some(row) if row.failed => crate::engine_seeds::HeadEntry::Failed,
            Some(row) if row.out == crate::engine_heads::no_units_out(name) => crate::engine_seeds::HeadEntry::None,
            Some(_) => match maybe_key {
                Some(key) => crate::engine_seeds::HeadEntry::Record(key),
                None => return Err(CliError::Text { what: "a head task's row has no record key (pages.rs `records`; engine_seeds.rs `RequestTable::build`)" }),
            },
        };
        overlay.push((name, entry));
    }
    Ok(overlay)
}

/// A Bodies phase: the crates' tables and frozen heads made ready, then the bodies checked;
/// its run facts, in the phase's arena.
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "bodies phase handles, each named")]
fn bodies_phase<'r, 'm, 'p>(
    rq: &mut Drive<'_, '_, 'r, '_, '_>,
    engine: &Engine<'r, Everything>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    read: &mut crate::engine_records::EngineRecords<'m>,
    records: &RecordKeys<'_>,
    last_lines: Option<&crate::engine_lines::Last<'_, '_>>,
    stored: Option<&crate::engine_answer::Stored<'_>>,
    files: &[(Name, &[Planned])],
    entered: &[Task],
    at_phase: SharedAlloc<'p>,
) -> Result<crate::engine_bodies::MadeFacts<'p>, CliError> {
    #[cfg(test)]
    stack_probe::at("bodies_phase");
    let (cx, frame, alloc, heap, tools, planned) = (rq.cx, rq.frame, rq.alloc, rq.heap, rq.tools, rq.planned);
    let handles = crate::engine_bodies::Handles { root: rq.root, env: rq.env, graph: rq.graph, names: rq.names.as_ref().map(crate::engine_names::NamesInputs::ran), outlined: rq.outlined, workers: rq.workers, clock: rq.phases.clock(), board: rq.board, tool: tools.0[blaze_drive::Kind::BodyCheck.index()] };
    // Read standing impl tables and frozen heads lazily from `stored` whenever `Units` did not
    // move: if no `ImplBucket` task ran (even when `Names` or `Signature` ran), every crate's
    // impl table still stands in `stored`, avoiding eagerly scanning all 1,206 `ImplBucket`
    // records; for `Signature` heads, keep `PagedHeads` active even when `Stage::Heads` ran by
    // overlaying only the `Signature` rows updated in `records.changes` this request, avoiding
    // eagerly scanning all ~19,000 standing `Signature` headers.
    let units_moved = rq.ran.iter().any(|(phase, _, count)| phase.stage == Stage::Units && *count > 0);
    let can_page_stored = !rq.fresh && !units_moved && rq.serve.served.get().is_none();
    let lazy_shelf_stored = stored.filter(|_| can_page_stored && !rq.bodies.entered_impl_bucket());
    let lazy_heads_stored = stored.filter(|_| can_page_stored);
    let stored_shelf = lazy_shelf_stored.map(|stored| crate::engine_shelf_pages::StoredShelf::new(stored, heap));
    // The tables of crates whose Heads task stands, from the store: one shelf of all when not paged.
    if stored_shelf.is_none()
        && let Some(stage) = rq.names.as_ref()
    {
        let shelf = &mut rq.bodies.shelf;
        rq.heads.read_back += records.lookup(|record| crate::engine_heads::complete(cx, &mut *store, stage, shelf, record, engine.view(), frame))?;
    }
    // The heads not frozen, a body that reads one quotes (engine_head_notes.rs).
    // PHASE-MEM-DEBT(Bodies): `Failed::of` takes `Alloc<'static>` and keeps its rows on the
    // process heap (engine_head_notes.rs), to the command's end for the print; to become the
    // lines record's head notes, read back.
    rq.bodies.heads_failed = crate::engine_head_notes::Failed::of(&*rq.heads, last_lines, heap.alloc())?;
    // The request's frozen heads, read only while the phase runs (engine_seeds.rs): the head
    // tasks' cells and the dependency heads this machine froze before.
    let seed_root = crate::engine_seeds::root_name(tools, frame);
    let seed_tool = crate::engine_seeds::tool_of(tools);
    let mut request_heads = match (rq.heads_from, rq.names.as_ref(), rq.graph) {
        (crate::engine_seeds::HeadsFrom::Frozen, Some(stage), Some(graph)) => match rq.bodies.kept.list(cx, &mut *store, &graph, frame)? {
            Some(list) => match lazy_heads_stored {
                Some(stored) => {
                    let crates = crate::engine_seeds::crates_unasked(&list);
                    let overlay = build_heads_overlay(&records.changes, &engine.view(), heap)?;
                    let paged = crate::engine_seeds::PagedHeads::with_overlay(stored, files, overlay, crate::engine_seeds::PagedPlant::None, heap);
                    let inputs = crate::engine_names::NamesInputs::ran(stage);
                    Some(crate::engine_seeds::RequestTable::paged(cx, &mut *store, paged, (crates, inputs.keys()), &seed_root, seed_tool, heap)?)
                }
                None => {
                    let crates = crate::engine_seeds::crates_of(&list, crate::engine_names::NamesInputs::ran(stage));
                    // PHASE-MEM-DEBT(Bodies): the table lives to this phase's end alone and is
                    // built on the process heap (engine_seeds.rs `RequestTable::build`, `heap:
                    // &'static Heap`): to take `&mem`'s `SharedAlloc<'p>`, when its growth is freed
                    // at the phase's end.
                    Some(records.lookup(|record| crate::engine_seeds::RequestTable::build(cx, &mut *store, planned, engine.view(), record, crates, &seed_root, seed_tool, heap))?)
                }
            },
            // No crate list: no body is checked, each says why (engine_bodies.rs `groups`).
            None => None,
        },
        _ => None,
    };
    // The rule, or a test's plant (rule 23): where a body's own items are made.
    if let Some(table) = request_heads.as_mut() {
        table.body_locals = rq.body_locals;
    }
    // The dependency heads the Heads stage put by key are listed by the root too.
    if let (Some(table), Some(once)) = (request_heads.as_mut(), rq.heads.once.as_ref()) {
        table.list_too(once.put_keys());
    }
    // Stand unchanged members from the last request's members record (StandingMembers) on the
    // phase path too: every outline entered earlier (plus any found file affected by serve_found)
    // is passed as moved, and `stands()` verifies each member's names input/out and graph_out.
    if !rq.fresh && rq.bodies.has_members_record() {
        let mut moved = blaze_own::Vec::new_in(frame);
        moved.extend_from_slice(rq.bodies.outlines_entered());
        if let Some(served) = rq.serve.served.get() {
            moved.extend(served.places.iter().map(|(file, _)| *file));
        }
        rq.bodies.stand_members(&moved, crate::engine_ready_edit::CommitPlant::None, heap.alloc());
    }
    // PHASE-MEM-DEBT(Bodies): the stage's packs, groups and notes on the process heap
    // (`handles.env.heap`, engine_bodies.rs `run_phase`), its notes kept to the command's end
    // for the print. To take `at_phase` for what lives to the phase's end. (A run's frames are
    // no longer debt: a stretch's frame ends with its stretch, and a body's scratch is reset as
    // it ends and trimmed past `SCRATCH_KEEP`, engine_bodies.rs `run_bodies`, `run_stretch`.) The stored head listings are read by key
    // from the committed snapshot (engine_listing_record.rs): a one-body edit makes none.
    let listing_snapshot = crate::store::Store::snapshot(&*store)?;
    let listings = rq.heads.listings(heap, listing_snapshot.as_ref(), handles.tool);
    let paged_source = stored_shelf.as_ref().map(|shelf| shelf as &dyn blaze_ladder::front2::buckets::ShelfSource);
    let facts = rq.bodies.run_phase(cx, &mut *store, &mut *read, &handles, entered, engine.view(), &mut rq.buffers[0], &mut *rq.times, alloc, frame, request_heads.as_ref(), listings, paged_source, at_phase)?;
    if let Some(stored) = lazy_shelf_stored.or(lazy_heads_stored) {
        if let Some(error) = StoredPages::refusal(stored) {
            return Err(crate::engine_answer::page_error(error));
        }
        if let Some(what) = request_heads.as_ref().and_then(|table| table.fault()).or_else(|| stored_shelf.as_ref().and_then(|shelf| shelf.fault())) {
            return Err(CliError::Text { what });
        }
        if let Some(shelf) = stored_shelf.as_ref() {
            rq.heads.read_back += shelf.read();
        }
    }
    // The dependency heads frozen now, put sorted by key and listed by the root.
    if let Some(table) = &request_heads {
        *rq.seed_counts = table.merge(cx, &mut *store, &seed_root, &mut *rq.ledger, rq.bodies.seeds)?;
    } else {
        rq.seed_counts.bodies = rq.bodies.seeds;
    }
    Ok(facts)
}

/// What [`checking`] drives, decided by engine_ready_edit.rs `choose`: the phase path's engine
/// over every task, or the ready path's start, its engine made once the wake has scoped it to
/// the entering names ([`ready_drive`]).
#[allow(clippy::large_enum_variant, reason = "one value a request, moved once into its drive")]
enum Driving<'r> {
    Phases(BoxedEngine<'r>, crate::engine_ready_edit::PhasesWhy),
    Ready(Answer<'r>),
}

impl<'r> Driving<'r> {
    /// Its two cases, taken by value in a call of its own. A `match` on the value in
    /// [`checking`] copies the `Copy` reason and leaves the rest of `driving` in place, partly
    /// moved: its drop at the end of `checking` then counts as a use of everything the boxed
    /// engine borrows (the stored answer, and through it the store's `Lent`), which the commit
    /// must take before that.
    fn parts(self) -> Result<(BoxedEngine<'r>, crate::engine_ready_edit::PhasesWhy), Answer<'r>> {
        match self {
            Driving::Phases(engine, why) => Ok((engine, why)),
            Driving::Ready(start) => Err(start),
        }
    }
}

/// The phase path's engine, boxed on the process heap and made in a call of its own: 3.2 KiB
/// by value (wave-1119 t59 type sizes), it and its moves (the `?` of its making, `driving`,
/// `drove`, the drive's `map`) sat in [`checking`]'s frame under every stage (rule 54).
type BoxedEngine<'r> = blaze_own::Box<Engine<'r, Everything>, blaze_own::Alloc<'static>>;

/// What the drive left for [`checking`]'s tail: the phase path's engine, to be finished, or
/// the ready drive's finished answer.
#[allow(clippy::large_enum_variant, reason = "one value a request, moved once into the tail")]
enum Drove<'r> {
    Phases(BoxedEngine<'r>, crate::engine_ready_edit::PhasesWhy),
    Ready(ReadyDrove<'r>),
}

/// The profile's `driver:` lines: why the phases ran, or what the ready drive woke and ran.
enum DriverLine {
    Phases(crate::engine_ready_edit::PhasesWhy),
    Ready(ReadyFacts),
}

impl fmt::Display for DriverLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverLine::Phases(why) => write!(f, "driver: phases ({})", why.words()),
            DriverLine::Ready(facts) => write!(f, "{facts}"),
        }
    }
}

/// What the ready drive hands [`checking`]'s tail: the engine's finished answer and the
/// profile's facts of the drive.
struct ReadyDrove<'r> {
    finished: blaze_engine::Finished<'r>,
    facts: ReadyFacts,
}

/// The ready drive's profile lines (`driver: ready`): the forward wake's counts, the names
/// entering, and the tasks the engine entered by kind, in engine_ready_edit.rs `READY_KINDS`'s
/// order (`FileItems`, `Given`, `Lint`, `BodyCheck`).
#[derive(Clone, Copy, Default, Debug)]
struct ReadyFacts {
    woke: crate::engine_ready_edit::Woke,
    entering: u32,
    entered: [u32; 4],
}

impl fmt::Display for ReadyFacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [files, given, lints, bodies] = self.entered;
        writeln!(f, "driver: ready ({} tasks entering)", self.entering)?;
        writeln!(f, "{}", self.woke)?;
        if given > 0 {
            write!(f, "ready tasks entered by kind: {files} FileItems, {given} Given, {lints} Lint, {bodies} BodyCheck")
        } else {
            write!(f, "ready tasks entered by kind: {files} FileItems, {lints} Lint, {bodies} BodyCheck")
        }
    }
}

/// The span of the phase path's file walk ([`plan`]): a ready request never opens it.
pub(crate) const FILE_WALK_SPAN: &str = "engine: the files found and outlined (the file walk)";
/// The span of the phase path's scripts, names and heads plans: a ready request never opens it.
pub(crate) const STAGES_PLAN_SPAN: &str = "engine: the scripts, the crates' names and heads planned";
/// The span of the ready request's setup ([`ready_setup`]), in place of the layout, the units
/// stage, the file walk and the stages' plans.
pub(crate) const READY_SETUP_SPAN: &str = "engine: the ready setup (no layout, no units stage, no file walk)";

/// What the ready setup hands the drive in place of the layout, the units stage and the file
/// plan ([`ready_setup`]).
struct ReadySetup<'f, 'h> {
    /// The leaf table: the sweep's rows as they are today (an edited file's row is its probe,
    /// which its outline took as known). The phase path makes the same reads again: the layout's
    /// rows, the home listings and every file's probe.
    rows: Rows<'f>,
    /// The names, read from the crate list the graph's record was sealed with; no maker, so no
    /// world can be made on this path (a names task is refused by its kind first).
    names: crate::names::Names<'f>,
    /// The unit graph's row as the stored answer holds it: the input a body reads.
    graph: blaze_drive::Input,
    /// The edited files' outlines (and any slot read as another kind), by name, as [`plan`]
    /// makes them; every other file is in `deferred`, read back after the wake only where the
    /// edit needs it ([`read_needed`]). Claims are by file name (engine_bodies.rs `FileClaim`),
    /// and every outline index is taken after that merge, over the one list.
    outlined: SVec<'h, Outlined<'h>>,
    /// Each file the sweep proved unchanged: its want and its probe, sorted by name; no row and
    /// no record of it read yet.
    deferred: SVec<'h, (Want<'h>, sweep::Probed)>,
    /// How many files the request has an outline of (every slot leaf): the closing line's count,
    /// as the phase path's walk outlines each.
    slots: u32,
    files: Files,
    skipped: Skipped<'h>,
    /// The edited files' places, as the last root keeps them (nothing they are a function of
    /// moved: no manifest, no listing, no file added or removed); the root keeps the others'.
    places: SVec<'h, (Name, blaze_engine::pages::FilePlace)>,
    /// The stored file rows read ([`Kept::read`]), for the profile's `plan:` line.
    rows_read: u32,
}

/// 1. on `Ready` (`check --driver=ready`, D187), in place of the layout (units.rs `lay`), the
/// units stage (engine_units.rs `discover`), the file walk ([`plan`]) and the names, scripts and
/// heads plans: what the edit needs, from the stored answer alone. Taken under the sweep's
/// [`crate::engine_units::Stood`] (no manifest, listing or lock moved) and `choose`'s
/// precondition (every changed leaf a file's text, present before and today):
///
/// 1. The unit graph's row and record, and the crate list it was sealed with (crate_list.rs
///    version 6): the names, read in place with no maker. A missing row or record, or a list
///    that does not open, is the request's typed refusal here, before any file is read (rule 3).
/// 2. The skipped files, from the places' flags; a target its features skip has its note
///    carried from the last lines record ([`Skipped::carried`]), which must hold it (else the
///    typed refusal): its count was made by [`plan`]'s walk over a layout that has not moved.
/// 3. Each file's want from its place in the last root (blaze-engine pages.rs `FilePlace`: its
///    crate root, edition and module form) and its probe the sweep's. An edited file is outlined
///    on the workers; every other file is deferred, no row or record of it read: after the wake,
///    [`read_needed`] reads back only the outlines the edit needs.
///
/// What still scales with the project: the copy of the sweep's leaf rows, one pass over the
/// root's places, and each deferred file's want (all in memory: no page and no record read).
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "the setup's edge: each argument one handle of it")]
fn ready_setup<'f, 'h, 'a>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &FromMeta,
    // The sweep's proof that no layout row moved: what the setup stands on (rule 74).
    _stood: &crate::engine_units::Stood,
    followed: &sweep::Followed<'_>,
    last: &blaze_engine::pages::Root<'_>,
    // The last request's lines: each skipped target's note is carried from it.
    last_lines: &crate::engine_lines::Last<'_, '_>,
    start: &Answer<'a>,
    records: &RecordKeys<'a>,
    tools: &blaze_engine::Tools,
    store: &mut dyn crate::store::Store<'_>,
    env: &FetchEnv<'_>,
    workers: NonZeroU32,
    // The rule set the request lints with: an edited file is outlined for it ([`outline_one`]).
    lints: Option<blaze_lint::RuleSet>,
    hot_graph: Option<&mut Option<HotGraph>>,
    alloc: SharedAlloc<'h>,
    frame: &'f Frame,
) -> Result<Result<ReadySetup<'f, 'h>, crate::engine_ready_edit::ReadyRefused>, CliError> {
    use crate::engine_ready_edit::ReadyRefused;
    let cx = cx.span(READY_SETUP_SPAN);
    // 1. The graph's row, its record and the crate list sealed in it.
    let name = Name::unit_graph();
    let Some(row) = start.row_facts(name).map_err(EngineError::from)? else { return Ok(Err(ReadyRefused::NoRow { name })) };
    let Some(key) = records.get(name)? else { return Ok(Err(ReadyRefused::Graph(crate::engine_units::BackRefused::NoKey))) };
    let (names, cached_listed_roots) = {
        let _cx = cx.span("names: the crate list read back");
        let load_fresh = |store: &mut dyn crate::store::Store<'_>| -> Result<Result<(crate::names::Names<'_>, blaze_ids::ContentId), ReadyRefused>, CliError> {
            let (view, sections) = match crate::engine_units::stored_graph_at(&cx, store, key)? {
                Ok(found) => found,
                Err(why) => return Ok(Err(ReadyRefused::Graph(why))),
            };
            if view.out != row.out {
                return Ok(Err(ReadyRefused::Graph(crate::engine_units::BackRefused::Out { key })));
            }
            if sections.list.is_empty() {
                return Ok(Err(ReadyRefused::NoList));
            }
            let mut held = Vec::with_capacity_in(sections.list.len(), env.heap.alloc());
            held.extend_from_slice(sections.list);
            let leaked: &'static [u8] = held.leak_static();
            let names = match crate::names::Names::of_list(leaked, None, env.heap.alloc()) {
                Ok(names) => names,
                Err(crate::names::NamesError::List(why)) => return Ok(Err(ReadyRefused::Graph(crate::engine_units::BackRefused::List { key, why }))),
                Err(crate::names::NamesError::ListFact { row: at, what }) => return Ok(Err(ReadyRefused::ListFact { row: at, what })),
                Err(_) => return Err(CliError::Text { what: "names.rs `of_list` refused the crate list by neither `List` nor `ListFact` (engine_check.rs `ready_setup`)" }),
            };
            Ok(Ok((names, view.out)))
        };
        match hot_graph {
            Some(slot) => {
                let hit = slot.as_ref().is_some_and(|cached| cached.key == key && cached.out == row.out);
                let names = if hit {
                    let Some(cached) = slot.as_ref() else {
                        return Err(CliError::Text { what: "hot_graph cache unexpectedly empty" });
                    };
                    crate::names::Names::from_hot_list(&cached.hot_list, env.heap.alloc())
                } else {
                    let (names, out) = match load_fresh(store)? {
                        Ok(pair) => pair,
                        Err(why) => return Ok(Err(why)),
                    };
                    let hot_list = names.to_hot_list(env.heap.alloc());
                    *slot = Some(HotGraph { key, out, hot_list });
                    names
                };
                let roots = slot.as_ref().map(|cached| cached.hot_list.listed_roots.as_slice());
                (names, roots)
            }
            None => {
                let (names, _) = match load_fresh(store)? {
                    Ok(pair) => pair,
                    Err(why) => return Ok(Err(why)),
                };
                (names, None)
            }
        }
    };
    crate::names::tell_public(store, &names);
    let graph = blaze_drive::Input { name, at: row.key, out: row.out };
    // 2. The skipped files, from the places' flags (sorted: the root's map is); and the skipped
    // targets as [`plan`] finds them: a target root (a place that is its own crate root, as
    // every member target's root is: they are the first wave) the crate list does not list.
    let mut listed: SSet<'h, Name> = SSet::new_in(alloc);
    if cached_listed_roots.is_none() {
        let list = names.list();
        for r in 0..blaze_world::crate_list::CrateGraph::len(list) {
            if let Some(file) = list.root(r).strip_prefix("/0/") {
                listed.insert(Name::file(file.as_bytes()));
            }
        }
    }
    // A skipped target's note is carried from the last lines record ([`lines_of`]): each target
    // root (a place that is its own crate root, as a member target's root is, the crate list
    // not listing it) must have its note there, else the request is refused before any work.
    let mut skipped_files: SVec<'h, Name> = SVec::new_in(alloc);
    let mut external_files: SVec<'h, Name> = SVec::new_in(alloc);
    let mut unnoted = 0usize;
    for (&file, place) in last.places.iter() {
        let is_listed = match cached_listed_roots {
            Some(roots) => roots.binary_search(&file).is_ok(),
            None => listed.contains(&file),
        };
        if place.unit == file && !is_listed {
            let noted = match last_lines.block(file) {
                Some(block) => crate::engine_lines::has_told(block).map_err(EngineCheckError::Lines)?,
                None => false,
            };
            unnoted += usize::from(!noted);
        }
        if place.flags & PLACE_SKIPPED != 0 {
            skipped_files.push(file);
        }
        if place.flags & PLACE_EXTERNAL != 0 {
            external_files.push(file);
        }
    }
    if unnoted > 0 {
        return Ok(Err(ReadyRefused::SkippedTargets { targets: unnoted }));
    }
    // 3. The leaf table, and each file's want from its place: read back where its row stands,
    // else outlined below.
    let mut rows = Rows::new_in(frame);
    let mut wanted: SVec<'h, (Want<'h>, Option<sweep::Probed>)> = SVec::new_in(alloc);
    let mut places: SVec<'h, (Name, blaze_engine::pages::FilePlace)> = SVec::new_in(alloc);
    let mut outlined: SVec<'h, Outlined<'h>> = SVec::new_in(alloc);
    let mut deferred: SVec<'h, (Want<'h>, sweep::Probed)> = SVec::new_in(alloc);
    let mut slots = 0u32;
    let mut files = Files::default();
    let kept = Kept { slots: &[], answer: start, records, stored: None, tools, read: core::sync::atomic::AtomicU32::new(0) };
    for (at, leaf) in followed.now.rows.iter().enumerate() {
        let slot = followed.now.path(leaf);
        // Script/data read rows (`by_script`) are appended by `scripts.script_rows` in the tail
        // (matching the phase path) so retired `Given` reads are not left behind in `rows`.
        if !leaf.kind.by_script() {
            rows.push(leaf.kind, leaf.flags, slot, leaf.stamp, leaf.id);
        }
        if leaf.kind != sweep::LeafKind::Slot {
            continue;
        }
        let file = Name::file(slot.as_bytes());
        let Some(&place) = last.places.get(&file) else { return Ok(Err(ReadyRefused::NoPlace { file })) };
        slots = slots.saturating_add(1);
        let changed = followed.changed(at);
        if changed {
            places.push((file, place));
        }
        let mut held = SVec::new_in(alloc);
        held.extend_from_slice(slot.as_bytes());
        let want = Want {
            name: file,
            slot: held,
            module: place.flags & PLACE_MODULE != 0,
            edition: place.edition,
            root: place.unit,
            found: place.flags & PLACE_FOUND,
            external: place.flags & PLACE_EXTERNAL != 0,
            #[cfg(test)]
            text_only_outline: TEXT_ONLY_OUTLINE.get(),
        };
        // The slot as the sweep found it today, when it was read as this kind of slot ([`Kept::slot`]).
        let Some(probed) = ((leaf.flags & sweep::MODULE != 0) == want.module).then_some((leaf.flags, leaf.stamp, leaf.id)) else {
            wanted.push((want, None));
            continue;
        };
        // A file the sweep proved unchanged, read as the same kind of slot: deferred, no row and
        // no record read here. The setup reads back only what the bodies' setup and the lint
        // walk will need ([`read_needed`], after the wake), never every file's (piece B2).
        if !changed {
            deferred.push((want, probed));
            continue;
        }
        // Read back as [`plan`] reads a standing outline back: by its row's record key.
        let Some((key, source, outline_source)) = kept.stood(&want, probed)? else {
            wanted.push((want, Some(probed)));
            continue;
        };
        let record = match store.get(&cx, key)? {
            None => return Err(EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name: file, key }).into()),
            Some(held) => crate::engine_store::outline_from(file, key, held.bytes, alloc).map_err(EngineCheckError::OutlineBack)?,
        };
        files.read_back += 1;
        let mut path = SVec::new_in(alloc);
        path.extend_from_slice(&sweep::real_path(slot, probed.0, cx.frame()));
        outlined.push(Outlined { name: file, path, source, outline_source, edition: want.edition, made: Ok(record) });
    }
    // Each file whose row does not stand (an edited file) outlined on the workers, in its task's
    // frame, as [`plan`] outlines one.
    let scope = cx.scope();
    scope.narrow_workers(workers, Class::Foreground);
    let mut handles = Vec::with_capacity_in(wanted.len(), alloc);
    for (want, known) in wanted.iter() {
        let known = *known;
        files.outlined += 1;
        files.probed += u32::from(known.is_none());
        handles.push(scope.may_overlap("outline", TASK_FRAME, move |cx: &Cx| Ok::<_, Infallible>(outline_one(cx, root, stamps, want, known, lints, alloc))));
    }
    let mut stopped = None;
    for (handle, (want, known)) in handles.into_iter().zip(wanted.iter()) {
        match handle.join(&cx).map(|one| one.map_err(|why| why.of(want))) {
            Ok(Ok((one, probed))) => {
                // A slot probed again (read as another kind of slot): its row is today's probe,
                // as [`plan`] pushes it (the newest row of a path is the one kept).
                if known.is_none() {
                    match core::str::from_utf8(&want.slot) {
                        Ok(slot) => {
                            sweep::opened(slot, probed, &mut rows);
                        }
                        Err(_) => {
                            stopped.get_or_insert(CliError::Text { what: "a path is not UTF-8" });
                        }
                    }
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
    scope.join(&cx).map_err(CliError::Pool)?;
    if let Some(error) = stopped {
        return Err(error);
    }
    outlined.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    deferred.sort_unstable_by(|a, b| a.0.name.cmp(&b.0.name));
    let skipped = Skipped { files: skipped_files, external: external_files, targets: SVec::new_in(alloc), carried: true };
    Ok(Ok(ReadySetup { rows, names, graph, outlined, deferred, slots, files, skipped, places, rows_read: kept.read.load(core::sync::atomic::Ordering::Relaxed) }))
}

/// What [`read_needed`] leaves: the deferred files not read back, by name and by the name of
/// their plan note (engine_body_plan.rs `note_name`), each sorted.
struct UnreadOutlines<'h> {
    files: SVec<'h, Name>,
    notes: SVec<'h, Name>,
}

/// 1b. on `Ready`, after the wake (piece B2): the deferred outlines ([`ready_setup`]) the edit
/// needs, read back as [`ready_setup`] reads one (by its row's record key; a row that does not
/// stand, outlined here as [`plan`] outlines one), merged into `outlined` (sorted by name: every
/// outline index of the request is taken after this, over this one list). `closed`: each visited
/// member's root and recorded files (engine_bodies.rs `BodiesStage::ready_reads`), each read
/// with every file its file parts reach, so a member's module tree is whole (`reached`); `owed`:
/// the entering tasks' files, each read alone. A name no place holds is left to the readers'
/// typed refusals (`reached`'s `Unoutlined`, the bodies' `Unplaced`). The files left unread.
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "the setup's edge: each argument one handle of it")]
fn read_needed<'h>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &FromMeta,
    kept: &Kept<'_, '_, '_>,
    store: &mut dyn crate::store::Store<'_>,
    deferred: &[(Want<'h>, sweep::Probed)],
    (closed, owed): (&[Name], &[Name]),
    outlined: &mut SVec<'h, Outlined<'h>>,
    files: &mut Files,
    workers: NonZeroU32,
    lints: Option<blaze_lint::RuleSet>,
    alloc: SharedAlloc<'h>,
) -> Result<UnreadOutlines<'h>, CliError> {
    enum ReadErr {
        Pages(blaze_engine::pages::PagesError),
        MissingRecord(Name),
        OutlineBack(crate::engine_store::OutlineBack),
        Snapshot(blaze_cas::plain::SnapshotError),
        NotUtf8,
    }
    impl ReadErr {
        fn into_cli(self) -> CliError {
            match self {
                ReadErr::Pages(error) => crate::engine_answer::page_error(error),
                ReadErr::MissingRecord(name) => EngineCheckError::MissingRecord { name }.into(),
                ReadErr::OutlineBack(why) => EngineCheckError::OutlineBack(why).into(),
                ReadErr::Snapshot(error) => crate::store::PlaceError::Snapshot(error).into(),
                ReadErr::NotUtf8 => CliError::Text { what: "a path is not UTF-8" },
            }
        }
    }
    enum ReadOut<'h> {
        Done { at: usize, close: bool, one: Outlined<'h> },
        NeedStore { at: usize, close: bool, key: blaze_drive::Key, source: blaze_ids::ContentId, outline_source: blaze_ids::ContentId },
        NeedOutline { at: usize, close: bool },
        Err(ReadErr),
    }

    let mut taken: SVec<'h, bool> = SVec::with_capacity_in(deferred.len(), alloc);
    taken.resize(deferred.len(), false);
    let mut closed_deferred: SVec<'h, bool> = SVec::with_capacity_in(deferred.len(), alloc);
    closed_deferred.resize(deferred.len(), false);
    let mut got_at: SVec<'h, Option<usize>> = SVec::with_capacity_in(deferred.len(), alloc);
    got_at.resize(deferred.len(), None);
    let mut closed_outlined: SVec<'h, bool> = SVec::with_capacity_in(outlined.len(), alloc);
    closed_outlined.resize(outlined.len(), false);
    let mut got: SVec<'h, Outlined<'h>> = SVec::new_in(alloc);

    let push_parts = |one: &Outlined<'_>, dest: &mut SVec<'h, Name>| {
        if let Ok(record) = &one.made {
            dest.extend(record.parts.iter().filter(|part| matches!(part.locate, blaze_drive::Locate::File { .. })).map(|part| part.name));
        }
    };

    let mut wave: SVec<'h, (usize, bool)> = SVec::new_in(alloc);
    let mut pending_closed: SVec<'h, Name> = SVec::new_in(alloc);
    pending_closed.extend_from_slice(closed);

    // Enqueue all `closed` seeds first (closing through already-outlined files immediately so
    // their children land in Wave 0), then `owed` seeds (without closing).
    let drain_closed = |pending: &mut SVec<'h, Name>,
                        outlined: &[Outlined<'h>],
                        closed_outlined: &mut [bool],
                        deferred: &[(Want<'h>, sweep::Probed)],
                        taken: &mut [bool],
                        closed_deferred: &mut [bool],
                        got: &[Outlined<'h>],
                        got_at: &[Option<usize>],
                        wave: &mut SVec<'h, (usize, bool)>| {
        while let Some(name) = pending.pop() {
            if let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(&name)) {
                if let (Some(seen), Some(one)) = (closed_outlined.get_mut(at), outlined.get(at))
                    && !*seen
                {
                    *seen = true;
                    push_parts(one, pending);
                }
                continue;
            }
            let Ok(at) = deferred.binary_search_by(|one| one.0.name.cmp(&name)) else { continue };
            let Some(seen) = closed_deferred.get_mut(at) else { continue };
            if *seen {
                continue;
            }
            *seen = true;
            let Some(was_taken) = taken.get_mut(at) else { continue };
            if *was_taken {
                if let Some(Some(idx)) = got_at.get(at)
                    && let Some(one) = got.get(*idx)
                {
                    push_parts(one, pending);
                }
                continue;
            }
            *was_taken = true;
            wave.push((at, true));
        }
    };

    drain_closed(&mut pending_closed, outlined, &mut closed_outlined, deferred, &mut taken, &mut closed_deferred, &got, &got_at, &mut wave);
    for &name in owed {
        if outlined.binary_search_by(|one| one.name.cmp(&name)).is_ok() {
            continue;
        }
        let Ok(at) = deferred.binary_search_by(|one| one.0.name.cmp(&name)) else { continue };
        if let Some(was_taken) = taken.get_mut(at)
            && !*was_taken
        {
            *was_taken = true;
            wave.push((at, false));
        }
    }

    while !wave.is_empty() {
        let mut next_closed: SVec<'h, Name> = SVec::new_in(alloc);
        if let Some(stored) = kept.stored && wave.len() >= 4 && workers.get() > 1 {
            let scope = cx.scope();
            scope.narrow_workers(workers, Class::Foreground);
            let tools = kept.tools;
            let read = &kept.read;
            let chunk = wave.len().div_ceil(usize::try_from(workers.get()).unwrap_or(1)).max(2);
            let mut chunk_handles = Vec::with_capacity_in(wave.len().div_ceil(chunk), alloc);
            for slice in wave.chunks(chunk) {
                chunk_handles.push(scope.may_overlap("ready reads", 64 << 10, move |_cx: &Cx| -> Result<_, Infallible> {
                    let mut out = Vec::with_capacity_in(slice.len(), alloc);
                    for &(at, close) in slice {
                        let Some((want, probed)) = deferred.get(at) else { continue };
                        let standing = match stood_from_stored(stored, tools, read, want, *probed) {
                            Ok(Some(one)) => one,
                            Ok(None) => {
                                out.push(ReadOut::NeedOutline { at, close });
                                continue;
                            }
                            Err(StoodErr::Pages(error)) => {
                                out.push(ReadOut::Err(ReadErr::Pages(error)));
                                continue;
                            }
                            Err(StoodErr::MissingRecord(name)) => {
                                out.push(ReadOut::Err(ReadErr::MissingRecord(name)));
                                continue;
                            }
                        };
                        let (key, source, outline_source) = standing;
                        match stored.snapshot_record(key) {
                            Ok(Some(bytes)) => match crate::engine_store::outline_from(want.name, key, bytes, alloc) {
                                Ok(record) => match core::str::from_utf8(&want.slot) {
                                    Ok(slot) => {
                                        let path = sweep::real_path_in(slot, probed.0, alloc);
                                        out.push(ReadOut::Done {
                                            at,
                                            close,
                                            one: Outlined { name: want.name, path, source, outline_source, edition: want.edition, made: Ok(record) },
                                        });
                                    }
                                    Err(_) => out.push(ReadOut::Err(ReadErr::NotUtf8)),
                                },
                                Err(why) => out.push(ReadOut::Err(ReadErr::OutlineBack(why))),
                            },
                            Ok(None) => out.push(ReadOut::NeedStore { at, close, key, source, outline_source }),
                            Err(error) => out.push(ReadOut::Err(ReadErr::Snapshot(error))),
                        }
                    }
                    Ok(out)
                }));
            }
            let mut stopped: Option<CliError> = None;
            for handle in chunk_handles {
                let part = match handle.join(cx) {
                    Ok(part) => part,
                    Err(error) => {
                        stopped.get_or_insert(CliError::Pool(error));
                        continue;
                    }
                };
                for item in part {
                    if stopped.is_some() {
                        break;
                    }
                    match item {
                        ReadOut::Done { at, close, one } => {
                            files.read_back += 1;
                            if close {
                                push_parts(&one, &mut next_closed);
                            }
                            if let Some(slot) = got_at.get_mut(at) {
                                *slot = Some(got.len());
                            }
                            got.push(one);
                        }
                        ReadOut::NeedStore { at, close, key, source, outline_source } => {
                            let Some((want, probed)) = deferred.get(at) else { continue };
                            let record = match store.get(cx, key) {
                                Ok(Some(held)) => crate::engine_store::outline_from(want.name, key, held.bytes, alloc).map_err(|why| EngineCheckError::OutlineBack(why).into()),
                                Ok(None) => Err(EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name: want.name, key }).into()),
                                Err(error) => Err(error),
                            };
                            match record {
                                Ok(record) => match core::str::from_utf8(&want.slot) {
                                    Ok(slot) => {
                                        files.read_back += 1;
                                        let path = sweep::real_path_in(slot, probed.0, alloc);
                                        let one = Outlined { name: want.name, path, source, outline_source, edition: want.edition, made: Ok(record) };
                                        if close {
                                            push_parts(&one, &mut next_closed);
                                        }
                                        if let Some(slot) = got_at.get_mut(at) {
                                            *slot = Some(got.len());
                                        }
                                        got.push(one);
                                    }
                                    Err(_) => {
                                        stopped.get_or_insert(CliError::Text { what: "a path is not UTF-8" });
                                    }
                                },
                                Err(error) => {
                                    stopped.get_or_insert(error);
                                }
                            }
                        }
                        ReadOut::NeedOutline { at, close } => {
                            let Some((want, probed)) = deferred.get(at) else { continue };
                            files.outlined += 1;
                            match outline_one(cx, root, stamps, want, Some(*probed), lints, alloc).map_err(|why| why.of(want)) {
                                Ok((one, _)) => {
                                    if close {
                                        push_parts(&one, &mut next_closed);
                                    }
                                    if let Some(slot) = got_at.get_mut(at) {
                                        *slot = Some(got.len());
                                    }
                                    got.push(one);
                                }
                                Err(error) => {
                                    stopped.get_or_insert(error);
                                }
                            }
                        }
                        ReadOut::Err(error) => {
                            stopped.get_or_insert(error.into_cli());
                        }
                    }
                }
            }
            scope.join(cx).map_err(CliError::Pool)?;
            if let Some(error) = stopped {
                return Err(error);
            }
        } else {
            for &(at, close) in wave.iter() {
                let Some((want, probed)) = deferred.get(at) else { continue };
                let slot = core::str::from_utf8(&want.slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
                let one = match kept.stood(want, *probed)? {
                    Some((key, source, outline_source)) => {
                        let record = match store.get(cx, key)? {
                            None => return Err(EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name: want.name, key }).into()),
                            Some(held) => crate::engine_store::outline_from(want.name, key, held.bytes, alloc).map_err(EngineCheckError::OutlineBack)?,
                        };
                        files.read_back += 1;
                        let path = sweep::real_path_in(slot, probed.0, alloc);
                        Outlined { name: want.name, path, source, outline_source, edition: want.edition, made: Ok(record) }
                    }
                    None => {
                        files.outlined += 1;
                        outline_one(cx, root, stamps, want, Some(*probed), lints, alloc).map_err(|why| why.of(want))?.0
                    }
                };
                if close {
                    push_parts(&one, &mut next_closed);
                }
                if let Some(slot) = got_at.get_mut(at) {
                    *slot = Some(got.len());
                }
                got.push(one);
            }
        }
        wave.clear();
        drain_closed(&mut next_closed, outlined, &mut closed_outlined, deferred, &mut taken, &mut closed_deferred, &got, &got_at, &mut wave);
    }

    outlined.extend(got);
    outlined.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    let mut unread = UnreadOutlines { files: SVec::new_in(alloc), notes: SVec::new_in(alloc) };
    for ((want, probed), taken) in deferred.iter().zip(taken.iter()) {
        if *taken {
            continue;
        }
        unread.files.push(want.name);
        let slot = core::str::from_utf8(&want.slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
        unread.notes.push(crate::engine_body_plan::note_name(&sweep::real_path(slot, probed.0, cx.frame())));
    }
    unread.notes.sort_unstable();
    Ok(unread)
}

/// 2. on `Ready` (`check --driver=ready`, D187): the edit planned forward from what moved and
/// run on blaze-engine's ready driver, its tasks those three kinds' (engine_ready_check.rs), its
/// rows and records the phase path's own: the same outline, the same lint walk, the bodies'
/// setup made as [`bodies_phase`] makes it and THE ONE JOIN (engine_bodies.rs `join_runs`), the
/// records put as [`put_phase`] puts them ([`ready_put`]). In order:
///
/// 1. The forward wake over the stored answer's pages (engine_ready_edit.rs `wake`) and the
///    precondition's second step (`admit`): a name of another kind is the request's error.
/// 2. The engine over the last answer and the edit, scoped to the entering names (`Entering`).
/// 3. The edited files' outlines put, before the bodies' setup reads the store (as the Outlines
///    phase puts them before a Bodies phase).
/// 4. The bodies' setup over the engine's view before the drive (`BodiesStage::prepare`): the
///    rows it decides at the plan are handed to the `BodyCheck` kind, which answers them; the
///    rest it runs over the setup's runner (`Prepared::runner`).
/// 5. The drive; a run that panicked, or a task the scope left owed, is the request's error
///    (the scope's one obligation: nothing the wake did not enter is owed, rule 3).
/// 6. The lint units kept, the bodies joined, the entered tasks' records put, the frozen
///    dependency heads merged, the entered names noted for the read grains.
/// Expands woken base `BodyCheck` tasks of edited files to all their planned view rows (D191):
/// `engine_ready_edit::wake` wakes keyed parts by their outline's base `part.name`, while
/// `plan_views` plans additional view rows `at_view(part.name, UnitView::At(view))` for every
/// reaching unit beyond rank 0. Checked view rows are woken via `FileItems` cell readers, but
/// an absent/unchecked view row reads only its own unit's `CrateScope` and would otherwise be
/// missed by `wake`, leaving its moved key in `owed_tasks` and knocking the next edit off `ready`.
fn expand_woken_views(
    reach: Option<&crate::engine_body_plan::Reach>,
    files: &[(Name, &[Planned])],
    woken: &mut SVec<'_, Name>,
) {
    let Some(reach) = reach else { return };
    let initial = woken.len();
    for &(file, group) in files.iter() {
        if file.kind != Kind::FileItems {
            continue;
        }
        let ranks = reach.ranks(file);
        if ranks.len() <= 1 {
            continue;
        }
        for at in 0..initial {
            let Some(&base) = woken.get(at) else {
                continue;
            };
            if base.kind != Kind::BodyCheck {
                continue;
            }
            for &(_, rank) in ranks.iter() {
                let Some(unit) = reach.unit(rank) else { continue };
                let view_name = blaze_drive::view::at_view(base, blaze_drive::view::UnitView::At(unit.view));
                if group.binary_search_by_key(&view_name, |one| one.name).is_ok() {
                    woken.push(view_name);
                }
            }
        }
    }
    if woken.len() > initial {
        woken.sort_unstable();
        woken.dedup();
    }
}

#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "the drive's edge: each argument one handle of it")]
fn ready_drive<'d, 'f, 'r, 's, 'w, 'm>(
    rq: &mut Drive<'d, 'f, 'r, 's, 'w>,
    start: Answer<'r>,
    files: &[(Name, &[Planned])],
    // 1. The names the edit enters, admitted, how the wake found them, each entering task's
    // plan by kind, and today's planned `Given` checkout read answers: all made by `checking`
    // before the setup's reads (piece B2: the reads are chosen from them), never a scan of the
    // checkout's plan.
    (names, entering, woke, tasks, given_answers): (&[Name], crate::engine_ready_edit::Entering<'_>, crate::engine_ready_edit::Woke, &[Task], &[(Name, blaze_ids::ContentId)]),
    stored: &crate::engine_answer::Stored<'_>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    read: &mut crate::engine_records::EngineRecords<'m>,
    records: &mut RecordKeys<'_>,
    last_lines: Option<&crate::engine_lines::Last<'_, '_>>,
    plant: crate::engine_ready_edit::CommitPlant,
) -> Result<ReadyDrove<'r>, CliError> {
    use crate::engine_ready_edit::READY_KINDS;
    use crate::engine_times::ns;
    let (cx, frame, alloc, heap, tools) = (rq.cx, rq.frame, rq.alloc, rq.heap, rq.tools);
    let env = rq.env;
    let clock = &env.clock;
    let began = clock.now();
    let of_kind = |kind: Kind| {
        let mut out: SVec<'r, Task> = SVec::new_in(alloc);
        out.extend(tasks.iter().filter(|task| task.planned.name.kind == kind).copied());
        out
    };
    let (outline_tasks, lint_tasks, body_tasks) = (of_kind(Kind::FileItems), of_kind(Kind::Lint), of_kind(Kind::BodyCheck));
    let mut given_entering: SVec<'r, Name> = SVec::new_in(alloc);
    given_entering.extend(tasks.iter().filter(|task| task.planned.name.kind == Kind::Given).map(|task| task.planned.name));
    // 2. The engine, scoped to the entering names.
    let engine = Engine::new_in(*tools, start, &Edit { files }, entering, blaze_engine::Plant::None, alloc)?;
    // The drive's arena: its puts and the bodies' run facts (engine_phase_mem.rs), ended once
    // its records are committed, below.
    let request_mem = rq.request_mem;
    let mem = request_mem.phase(Stage::Bodies)?;
    // 3. The edited files' outlines, put whole as the Outlines phase puts them.
    let mut puts = ready_put(rq, store, records, Stage::Outlines, &outline_tasks, None, &mem)?;
    // 3b. Each entered file's last lint record, read before the drive (its runs read the stage,
    // never write it), so a unit its key keeps is copied, not linted.
    lint_priors(cx, &mut *store, &*records, rq.fresh, &lint_tasks, &mut *rq.lints)?;
    // 4. The bodies' setup, as `bodies_phase` makes a Bodies phase's, over the engine's view
    // before the drive: the cells of the stages before Bodies stand (the precondition), and a
    // decided row's reads are read again through the host when its task runs.
    // Each crate's names input: no `Names` or `Scripts` task is planned or entered on this
    // driver (`admit`), so every crate's names row STANDS and is read from the stored answer by
    // need (engine_names.rs `NamesInputs::stood`), never taken as missing: a crate with no names
    // row at all is still refused by name, and a standing row naming no record is this
    // request's error below.
    let names_missing: std::sync::OnceLock<Name> = std::sync::OnceLock::new();
    let names_stood = crate::engine_names::Stood { pages: stored, missing: &names_missing };
    let names_inputs = rq.names.as_ref().map(|stage| crate::engine_names::NamesInputs::stood(stage, names_stood));
    let handles = crate::engine_bodies::Handles { root: rq.root, env, graph: rq.graph, names: names_inputs, outlined: rq.outlined, workers: rq.workers, clock: rq.phases.clock(), board: rq.board, tool: tools.0[Kind::BodyCheck.index()] };
    // The crates' impl tables, each read from its stored row on a body's first probe of it
    // (engine_shelf_pages.rs), never every crate's before the bodies run (`complete`, the phase
    // path's: rule 78, the same tables either way).
    let stored_shelf = crate::engine_shelf_pages::StoredShelf::new(stored, heap);
    rq.bodies.heads_failed = crate::engine_head_notes::Failed::of(&*rq.heads, last_lines, heap.alloc())?;
    let seed_root = crate::engine_seeds::root_name(tools, frame);
    let seed_tool = crate::engine_seeds::tool_of(tools);
    // The head entries a body asks, each found on its first ask on its row's own page (rule 78:
    // the phase path builds the same entries from its whole plan), never a scan of the plan.
    let paged_plant = match plant {
        crate::engine_ready_edit::CommitPlant::PagedHeadsMiss => crate::engine_seeds::PagedPlant::Misses,
        crate::engine_ready_edit::CommitPlant::None
        | crate::engine_ready_edit::CommitPlant::DropsMovedPage
        | crate::engine_ready_edit::CommitPlant::StandingDropsHeads
        | crate::engine_ready_edit::CommitPlant::ReadsNoVisitedTree
        | crate::engine_ready_edit::CommitPlant::RowKeyUnitOnly
        | crate::engine_ready_edit::CommitPlant::HoldersKeyedByText
        | crate::engine_ready_edit::CommitPlant::DeferredNeverVisited => crate::engine_seeds::PagedPlant::None,
    };
    let mut request_heads = match (rq.heads_from, names_inputs, rq.graph) {
        (crate::engine_seeds::HeadsFrom::Frozen, Some(inputs), Some(graph)) => match rq.bodies.kept.list(cx, &mut *store, &graph, frame)? {
            Some(list) => {
                // Every crate's names row read by need (piece C): one page per crate of the list
                // before was 1206 asks on tempo for the handful a body's heads name.
                let crates = crate::engine_seeds::crates_unasked(&list);
                let paged = crate::engine_seeds::PagedHeads::new(stored, files, paged_plant, heap);
                Some(crate::engine_seeds::RequestTable::paged(cx, &mut *store, paged, (crates, inputs.keys()), &seed_root, seed_tool, heap)?)
            }
            None => None,
        },
        _ => None,
    };
    if let Some(table) = request_heads.as_mut() {
        table.body_locals = rq.body_locals;
    }
    if let (Some(table), Some(once)) = (request_heads.as_mut(), rq.heads.once.as_ref()) {
        table.list_too(once.put_keys());
    }
    let listing_snapshot = crate::store::Store::snapshot(&*store)?;
    let listings = rq.heads.listings(heap, listing_snapshot.as_ref(), handles.tool);
    let mut decided = Buffer::new_in(alloc);
    // The setup's standing strategy (engine_members_record.rs): the wake's names hold every
    // outline this edit moves, so a member none of whose files is among them, or among the
    // entered bodies' files, is taken from the members record, unvisited.
    rq.bodies.stand_members(names, plant, heap.alloc());
    let prepared = rq.bodies.prepare(cx, &mut *store, &mut *read, &handles, &body_tasks, &engine.view(), &mut decided, request_heads.as_ref(), listings, alloc, frame)?;
    // 5. The drive, on the command's workers.
    let mut pool = crate::engine_ready_check::Workers { cx, clock, start: clock.now(), workers: rq.workers };
    // The runs read the setup's `Sync` part and the stage's view, never the stage (its
    // frame-held findings are this thread's): engine_bodies.rs `BodyRunner`.
    // The request's scope records opened by need (piece C (a), engine_bodies.rs
    // `Prepared::scope_records`): a body's scope crate past its member opened at its first
    // question, once for every run; its counts read after the join.
    let by_need = prepared.scope_records();
    let parked = prepared.pre_open_lang_with(cx, &by_need, rq.workers, |cx| {
        crate::engine_ready_check::walk_lints(cx, &*rq.lints, rq.root, rq.outlined, &lint_tasks, alloc)
    })??;
    let stages = crate::engine_ready_check::Stages { parked, runner: prepared.runner(rq.bodies.reads_paged(&stored_shelf), &by_need), decided };
    let driven = crate::engine_ready_check::drive_edit(engine, given_answers, &given_entering, rq.outlined, stages, &mut pool, alloc, heap.shared())?;
    let crate::engine_ready_check::Driven { finished, mut sunk, panicked } = driven;
    if let Some(&name) = panicked.first() {
        return Err(EngineCheckError::ReadyHost { task: name, what: "its run panicked on the ready driver's workers (engine_ready_check.rs `Workers`)" }.into());
    }
    let owed_tasks = blaze_engine::LastAnswer::owed(&finished.answer, blaze_own::Alloc::from(alloc)).map_err(EngineError::from)?;
    if let Some(planned) = owed_tasks.iter().find(|planned| names.binary_search(&planned.name).is_err()) {
        return Err(EngineCheckError::ReadyHost { task: planned.name, what: "owed by the ready scope: the forward wake did not enter it, and the edit moved its key or a cell it reads (engine_ready_edit.rs `wake`)" }.into());
    }
    let owed_cells = blaze_engine::LastAnswer::owed_cells(&finished.answer, blaze_own::Alloc::from(alloc)).map_err(EngineError::from)?;
    if let Some((cell, _)) = owed_cells.iter().find(|(cell, _)| names.binary_search(&cell.of).is_err()) {
        return Err(EngineCheckError::ReadyHost { task: cell.of, what: "a cell it defines is owed by the ready scope: the forward wake did not enter its definer (engine_ready_edit.rs `wake`)" }.into());
    }
    // A head task row the paged table could not take, or a stored page a run could not read, is
    // the request's error (rule 3), as the built table stops at its making.
    if let Some(error) = StoredPages::refusal(stored) {
        return Err(crate::engine_answer::page_error(error));
    }
    // A standing names row that names no record (`NamesInputs::stood`): as the phase path's
    // marks refuse it (`MissingRecord`), never a crate taken as having no names.
    if let Some(&name) = names_missing.get() {
        return Err(EngineCheckError::MissingRecord { name }.into());
    }
    if let Some(what) = request_heads.as_ref().and_then(|table| table.fault()).or_else(|| stored_shelf.fault()) {
        return Err(CliError::Text { what });
    }
    // 6. The lint units kept, and THE ONE JOIN of the bodies' runs.
    sunk.keep_lints(&mut *rq.lints);
    let runs = core::mem::replace(&mut sunk.bodies, SVec::new_in(alloc));
    let made = prepared.join_ready(&mut *rq.bodies, cx, &mut *store, runs, &mut *rq.times, frame, alloc, mem.shared())?;
    drop(sunk);
    rq.bodies.add_by_need(by_need.counts());
    drop(by_need);
    drop(prepared);
    // Counted here, after the setup's borrow of the heads stage (`listings`) has ended.
    rq.heads.read_back += stored_shelf.read();
    // The dependency heads frozen now, put sorted by key and listed by the root (as
    // `bodies_phase` does after its runs, before the phase's put).
    match &request_heads {
        Some(table) => *rq.seed_counts = table.merge(cx, &mut *store, &seed_root, &mut *rq.ledger, rq.bodies.seeds)?,
        None => rq.seed_counts.bodies = rq.bodies.seeds,
    }
    drop(request_heads);
    // The merge asks a crate's names row by need (engine_seeds.rs `CrateNames::ByNeed`): a row
    // first read there that does not read, or names no record, refuses the request as one read
    // during the drive does, before anything is committed (rule 3).
    if let Some(error) = StoredPages::refusal(stored) {
        return Err(crate::engine_answer::page_error(error));
    }
    if let Some(&name) = names_missing.get() {
        return Err(EngineCheckError::MissingRecord { name }.into());
    }
    // The entered tasks' records, as their phases put them.
    let mut entered: SVec<'r, Name> = SVec::with_capacity_in(finished.entered.len(), alloc);
    entered.extend_from_slice(&finished.entered);
    entered.sort_unstable();
    let entered_of = |tasks: &[Task]| {
        let mut out: SVec<'r, Task> = SVec::new_in(alloc);
        out.extend(tasks.iter().filter(|task| entered.binary_search(&task.planned.name).is_ok()).copied());
        out
    };
    let (lint_entered, body_entered) = (entered_of(&lint_tasks), entered_of(&body_tasks));
    puts += ready_put(rq, store, records, Stage::Lints, &lint_entered, None, &mem)?;
    puts += ready_put(rq, store, records, Stage::Bodies, &body_entered, Some((&made, &finished.answer)), &mem)?;
    rq.bodies.note_entered(finished.entered.iter().copied(), heap.alloc());
    rq.puts += puts;
    drop(made);
    let row = mem.end();
    rq.board.phase_mem(row.stage, rq.phase_mem.add(row));
    // The profile: one stretch for the drive, and each kind's entered tasks as its stage's row.
    let mut by_kind = [0u32; 4];
    for name in finished.entered.iter() {
        if let Some(at) = READY_KINDS.iter().position(|kind| *kind == name.kind) {
            by_kind[at] = by_kind[at].saturating_add(1);
        }
    }
    for (stage, count) in [Stage::Outlines, Stage::Scripts, Stage::Lints, Stage::Bodies].into_iter().zip(by_kind) {
        if count > 0 {
            rq.ran.push((Phase::new(stage, 0), 0, count));
        }
    }
    let ended = clock.now();
    rq.phase_times.push(rq.times.fold(Phase::new(Stage::Bodies, 0), ns(began.since_start()), ns(ended.since_start()), 0, 0));
    let facts = ReadyFacts { woke, entering: u32::try_from(names.len()).unwrap_or(u32::MAX), entered: by_kind };
    Ok(ReadyDrove { finished, facts })
}

/// The ready drive's records of `tasks` (entered, all of `stage`) put as [`put_phase`] puts a
/// phase's (rule 30: the same keys, the same skip of a record already held), each one's key
/// into `records` for the answer's payload column; the head listings the stage made put with
/// them, and the store's puts committed. `bodies`: the Bodies stage's run facts and the
/// finished answer whose rows' `out` the facts records carry. The puts' count.
#[inline(never)]
fn ready_put<'m>(
    rq: &mut Drive<'_, '_, '_, '_, '_>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    records: &mut RecordKeys<'_>,
    stage: Stage,
    tasks: &[Task],
    bodies: Option<(&crate::engine_bodies::MadeFacts<'_>, &Answer<'_>)>,
    mem: &crate::engine_phase_mem::PhaseMem<'_>,
) -> Result<u32, CliError> {
    let (cx, fresh, outlined) = (rq.cx, rq.fresh, rq.outlined);
    let mut facts_bytes: Vec<u8, blaze_own::Alloc<'_>> = Vec::new_in(mem.alloc());
    let mut facts_at: Vec<(blaze_ids::ContentId, usize, usize), blaze_own::Alloc<'_>> = Vec::new_in(mem.alloc());
    if let Some((made, answer)) = bodies {
        for task in tasks.iter() {
            let name = task.planned.name;
            let row = answer.row_facts(name).map_err(EngineError::from)?.ok_or(EngineCheckError::BodyFacts { body: name, why: crate::engine_body_facts::MakeError::NoRow })?;
            let from = facts_bytes.len();
            rq.bodies.write_facts(made, name, &mut facts_bytes).map_err(|why| EngineCheckError::BodyFacts { body: name, why })?;
            facts_at.push((row.out, from, facts_bytes.len()));
        }
    }
    let mut put: Vec<Put<'_>, blaze_own::Alloc<'_>> = Vec::new_in(mem.alloc());
    let lints = &*rq.lints;
    for (index, task) in tasks.iter().enumerate() {
        let name = task.planned.name;
        let outline = || outlined.binary_search_by(|one| one.name.cmp(&name)).ok().and_then(|at| outlined[at].made.as_ref().ok());
        let listing = match stage {
            Stage::Outlines => outline().map(crate::engine_store::Listing::of),
            _ => None,
        };
        let found = match stage {
            Stage::Outlines => outline().map(|record| (record.out, &record.reads[..], record.payload_bytes())),
            Stage::Lints => lints.result(name).map(|one| (one.record.out, &one.record.reads[..], one.record.payload_bytes())),
            Stage::Bodies => facts_at.get(index).map(|&(out, from, to)| (out, &[][..], Ok(&facts_bytes[from..to]))),
            Stage::Units | Stage::Scripts | Stage::Names | Stage::Heads => None,
        };
        let Some((out, reads, payload)) = found else {
            records.changes.insert(name, None);
            continue;
        };
        let payload = payload?;
        let key = match stage {
            Stage::Outlines => task.key,
            _ => crate::engine_store::record_key(task.key, out, reads, payload),
        };
        records.changes.insert(name, Some(key));
        if matches!(stage, Stage::Bodies | Stage::Lints) && !fresh && crate::store::Store::get(&mut *store, cx, key)?.is_some() {
            continue;
        }
        put.push(Put { key, name, source: task.planned.source, out, reads, payload, head: crate::engine_store::PutHead::Build(listing), extras: None });
    }
    let mut put_now = crate::engine_store::put_all(cx, &mut *store, &mut put, stage, &mut *rq.ledger, mem.alloc())?;
    drop(put);
    put_now += rq.heads.put_listings(cx, &mut *store, &mut *rq.ledger, stage)?;
    if put_now > 0 {
        let _cx = cx.span("store: a phase's commit on the command thread (batch copy, layout copy, segment digest, flip, map)");
        store.commit_puts()?;
    }
    rq.ledger.merged(stage, crate::store::Store::kept(&*store));
    rq.board.merged(stage, rq.ledger.memory.stages[stage.index()]);
    Ok(put_now)
}

/// A Heads phase: the closed dependencies' heads found by key, then the head tasks run.
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "heads phase handles, each named")]
fn heads_phase<'r, 'm, 'l>(
    rq: &mut Drive<'_, '_, 'r, '_, '_>,
    engine: &Engine<'r, Everything>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    read: &mut crate::engine_records::EngineRecords<'m>,
    entered: &[Task],
    heads_snap: &'l core::cell::OnceCell<crate::store::SharedSnapshot>,
    heads_lazy: &mut Option<crate::front_load::ScopeRecords<'l>>,
) -> Result<(), CliError> {
    #[cfg(test)]
    stack_probe::at("heads_phase");
    let (cx, frame, alloc, heap, tools) = (rq.cx, rq.frame, rq.alloc, rq.heap, rq.tools);
    let handles = crate::engine_bodies::Handles { root: rq.root, env: rq.env, graph: rq.graph, names: rq.names.as_ref().map(crate::engine_names::NamesInputs::ran), outlined: rq.outlined, workers: rq.workers, clock: rq.phases.clock(), board: rq.board, tool: tools.0[blaze_drive::Kind::BodyCheck.index()] };
    let crate::engine_bodies::BodiesStage { kept, shelf, .. } = &mut *rq.bodies;
    // PHASE-MEM-DEBT(Heads): `HeadsStage::made` and `refused` on the process heap, kept to the
    // command's end for the put and the notes, and the shelf's tables carried to the Bodies
    // phases (`bodies.shelf`): to take `at_phase` for the first, and the store's records read in
    // place for the second. A closed dependency's head is found by its key in the store's
    // committed snapshot (lane H9; engine_heads_once.rs, "A CLOSED DEPENDENCY'S HEAD").
    if let (Some(once), Some(stage), Some(graph)) = (rq.heads.once.as_mut(), rq.names.as_ref(), rq.graph) {
        if let Some(list) = kept.list(cx, &mut *store, &graph, frame)? {
            let records = store.snapshot_in(heap.shared())?;
            let crates = crate::engine_seeds::crates_in(&list, stage, heap.shared());
            once.find_dependencies(crate::engine_seeds::DependencyFinder::new(records, crates, crate::engine_seeds::tool_of(tools)));
        }
    }
    rq.heads.run_phase(cx, &mut *store, &mut *read, &handles, kept, shelf, entered, engine.view(), &mut rq.buffers[0], &mut *rq.times, alloc, frame, heads_snap, heads_lazy)
}

/// The phase's records to the store, sorted by key, committed before the next phase reads
/// them; each handed payload freed; each producer's mark for the phases after.
#[inline(never)]
fn put_phase<'m>(
    rq: &mut Drive<'_, '_, '_, '_, '_>,
    engine: &Engine<'_, Everything>,
    store: &mut crate::store::CommandStore<'m, '_, bstd::io::LentDriver>,
    read: &mut crate::engine_records::EngineRecords<'m>,
    records: &mut RecordKeys<'_>,
    phase: Phase,
    entered: &[Task],
    phase_facts: Option<&crate::engine_bodies::MadeFacts<'_>>,
    mem: &crate::engine_phase_mem::PhaseMem<'_>,
) -> Result<(), CliError> {
    #[cfg(test)]
    stack_probe::at("put_phase");
    let (cx, heap, fresh, outlined, discovered) =(rq.cx, rq.heap, rq.fresh, rq.outlined, rq.discovered);
    // The Bodies stage's records: each entered body's run facts (engine_body_facts.rs), made
    // once into one buffer in the phase's arena before the puts borrow it. Each task's row `out`
    // (the verdict digest, which the facts do not change) and its bytes' range, by the task's
    // place in `entered`; empty in every other stage. A body row that stands is not entered: its
    // record stands too, found by the answer's record map.
    let mut facts_bytes: Vec<u8, blaze_own::Alloc<'_>> = Vec::new_in(mem.alloc());
    let mut facts_at: Vec<(blaze_ids::ContentId, usize, usize), blaze_own::Alloc<'_>> = Vec::new_in(mem.alloc());
    if phase.stage == Stage::Bodies {
        let made = phase_facts.ok_or(EngineCheckError::MissingEntry { task: entered.first().map_or(Name::units(), |task| task.planned.name), table: Table::BodyFacts, key: Name::units() })?;
        for task in entered.iter() {
            let name = task.planned.name;
            let row = engine.view().row(name).ok_or(EngineCheckError::BodyFacts { body: name, why: crate::engine_body_facts::MakeError::NoRow })?;
            let from = facts_bytes.len();
            rq.bodies.write_facts(made, name, &mut facts_bytes).map_err(|why| EngineCheckError::BodyFacts { body: name, why })?;
            facts_at.push((row.out, from, facts_bytes.len()));
        }
    }
    // One a task of the phase: the project's size, so in the phase's arena, gone with it.
    let mut put: Vec<Put<'_>, blaze_own::Alloc<'_>> = Vec::new_in(mem.alloc());
    let mut marks: Vec<(Name, Option<(blaze_drive::Key, blaze_ids::ContentId)>), blaze_own::Alloc<'_>> = Vec::new_in(mem.alloc());
    let names = rq.names.as_ref();
    let (scripts, heads, lints) = (&*rq.scripts, &*rq.heads, &*rq.lints);
    for (index, task) in entered.iter().enumerate() {
        let name = task.planned.name;
        // A file's outline is put whole (its parts, paths and findings): the next request that
        // finds the file unchanged reads it back and never the file (`plan`).
        let listing = match phase.stage {
            Stage::Outlines => outlined.binary_search_by(|one| one.name.cmp(&name)).ok().and_then(|at| outlined[at].made.as_ref().ok()).map(crate::engine_store::Listing::of),
            // A units record is put with its parts as well (the root's: each member's units; a
            // member's: its crate roots' `FileItems`, units.rs `run`): they are the record's
            // facts, and the start's walk from `Name::units()` down to the bodies reads them
            // (run_ids.rs `workspace_of`). Put plain, the record held none, and the walk found no
            // body. The planner reads the layout instead (`units::lay`).
            Stage::Units if name.kind == Kind::Units => discovered.get(name).and_then(|one| one.made.as_ref().ok()).map(crate::engine_store::Listing::of),
            _ => None,
        };
        let names_record = match phase.stage {
            Stage::Names => names.and_then(|stage| {
                let row = stage.results.iter().rev().find(|one| one.name == name)?;
                Some((stage, row.made.as_ref().ok()?))
            }),
            _ => None,
        };
        let found = match phase.stage {
            Stage::Units => discovered.get(name).and_then(|one| one.made.as_ref().ok()).map(|record| (record.out, &record.reads[..], record.payload_bytes())),
            Stage::Outlines => outlined
                .binary_search_by(|one| one.name.cmp(&name))
                .ok()
                .and_then(|at| outlined[at].made.as_ref().ok())
                .map(|record| (record.out, &record.reads[..], record.payload_bytes())),
            Stage::Names => names_record.map(|(_, record)| (record.out, &record.reads[..], record.payload_bytes())),
            Stage::Scripts => scripts.result(name).and_then(|one| one.made.as_ref().ok()).map(|record| (record.out, &record.reads[..], record.payload_bytes())),
            Stage::Heads => heads.result(name).map(|one| (one.out, &one.reads[..], Ok(&one.payload[..]))),
            Stage::Lints => lints.result(name).map(|one| (one.record.out, &one.record.reads[..], one.record.payload_bytes())),
            // A plain record: no reads of its own (its key holds the row's verdict digest and its
            // bytes), its payload the run facts.
            Stage::Bodies => facts_at.get(index).map(|&(out, from, to)| (out, &[][..], Ok(&facts_bytes[from..to]))),
        };
        let Some((out, reads, payload)) = found else {
            records.changes.insert(name, None);
            continue;
        };
        // A payload released by an earlier phase's put is refused, naming the record.
        let payload = payload?;
        // Under its bytes' key (engine_store.rs `record_key`); the units and the outlines under
        // their task keys, which the scripts stage and the outline jobs read them by
        // (engine_scripts.rs `plan`, `outline_one`) and whose bytes their keys' text makes.
        let (key, head) = match names_record {
            Some((stage, _)) => {
                let (key, head) = stage.publication(name)?;
                (key, crate::engine_store::PutHead::Prepared(head))
            }
            None => {
                let key = match phase.stage {
                    Stage::Units | Stage::Outlines => task.key,
                    _ => crate::engine_store::record_key(task.key, out, reads, payload),
                };
                (key, crate::engine_store::PutHead::Build(listing))
            }
        };
        records.changes.insert(name, Some(key));
        // A body's facts record or a lint's record already held under its key (a file switched
        // away and back: the same verdict, the same bytes; both keyed by their bytes,
        // `record_key`) is found by its key, never put again (rule 77). Only asked of a command
        // that started from a stored answer: a fresh one holds none. Neither stage's mark is
        // read by a later phase (the `marks` loop below), so nothing else is skipped.
        if matches!(phase.stage, Stage::Bodies | Stage::Lints) && !fresh && crate::store::Store::get(&mut *store, cx, key)?.is_some() {
            continue;
        }
        marks.push((name, Some((key, out))));
        // A crate's expansion records, put after its record (engine_store.rs `put_all`).
        let extras = names_record.map(|(_, record)| &record.extras);
        put.push(Put { key, name, source: task.planned.source, out, reads, payload, head, extras });
    }
    // The store borrows the Names workers' headers through the sorted put.
    let mut put_now = crate::engine_store::put_all(cx, &mut *store, &mut put, phase.stage, &mut *rq.ledger, mem.alloc())?;
    drop(put);
    // The head listings this phase made (engine_heads.rs `Listings`), put with its records.
    put_now += rq.heads.put_listings(cx, &mut *store, &mut *rq.ledger, phase.stage)?;
    if phase.stage == Stage::Names && let Some(stage) = rq.names.as_mut() {
        stage.put_finished();
    }
    // The closed dependency heads frozen this phase, put under their keys (lane H9).
    if phase.stage == Stage::Heads
        && let Some(once) = rq.heads.once.as_mut()
    {
        put_now += once.put_dependencies(cx, &mut *store, &mut *rq.ledger)?;
    }
    rq.puts += put_now;
    // Written now, so the store's arena frees them: the next phase reads them in place from the
    // segment's mapping (rule 51; store.rs `commit_puts`).
    if put_now > 0 {
        {
            let _cx = cx.span("store: a phase's commit on the command thread (batch copy, layout copy, segment digest, flip, map)");
            store.commit_puts()?;
        }
        if phase.stage == Stage::Heads
            && let Some(once) = rq.heads.once.as_mut()
        {
            once.committed(store.snapshot_in(heap.shared())?);
        }
    }
    // Each put record's handed payload freed now it is in the store (rule 51): a result keeps
    // its findings and lines, never its bytes; a later phase reads the store.
    for task in entered.iter() {
        let name = task.planned.name;
        let result = match phase.stage {
            Stage::Names => rq.names.as_mut().and_then(|stage| stage.results.iter_mut().rev().find(|one| one.name == name)),
            Stage::Scripts => rq.scripts.results.iter_mut().find(|one| one.name == name),
            Stage::Units | Stage::Outlines | Stage::Heads | Stage::Bodies | Stage::Lints => None,
        };
        let freed = result.and_then(|one| one.made.as_mut().ok()).map_or(0, |record| record.release_handed());
        rq.ledger.released(phase.stage, freed);
    }
    rq.ledger.merged(phase.stage, crate::store::Store::kept(&*store));
    rq.board.merged(phase.stage, rq.ledger.memory.stages[phase.stage.index()]);
    rq.ledger.read(phase.stage, read.counts());
    // What later phases read each entry by (a failed one its driver marked failed).
    for (name, made) in marks.drain(..) {
        match phase.stage {
            Stage::Scripts => {
                rq.scripts.mark(name, made);
                if let Some(stage) = rq.names.as_mut() {
                    stage.mark(name, made);
                }
            }
            Stage::Names => {
                if let Some(stage) = rq.names.as_mut() {
                    stage.mark(name, made);
                }
            }
            Stage::Units | Stage::Outlines | Stage::Heads | Stage::Bodies | Stage::Lints => {}
        }
    }
    drop(marks);
    drop(facts_at);
    drop(facts_bytes);
    Ok(())
}

/// Where each of the command's stages starts on the stack, under test: each stage function
/// marks its entry ([`stack_probe::at`]), keeping the lowest stack address it was entered at, so
/// a test of the check's stack (engine_check_tests.rs `a_whole_check_fits_its_stated_stack`)
/// names how deep each stage began beside the thread's high water. What lies between a stage's
/// depth and the high water is that stage's callees.
#[cfg(test)]
pub(crate) mod stack_probe {
    use core::cell::RefCell;

    /// More stages than the command has: a stage past the last slot is dropped by [`at`].
    const SLOTS: usize = 24;

    // A fixed table, not a vector: `Vec::new_in` is not a const fn on the pinned nightly, and
    // the probe must not allocate on the stack it measures.
    std::thread_local! {
        static ENTERED: RefCell<[(&'static str, usize); SLOTS]> = const { RefCell::new([("", 0); SLOTS]) };
    }

    /// The address of a local of a frame of its own: the stack pointer, near enough.
    #[inline(never)]
    fn here() -> usize {
        let local = 0u8;
        core::hint::black_box(&raw const local).addr()
    }

    /// `stage` was entered here; the lowest entry of each stage is kept.
    pub(crate) fn at(stage: &'static str) {
        let sp = here();
        ENTERED.with_borrow_mut(|entered| match entered.iter_mut().find(|one| one.0 == stage || one.0.is_empty()) {
            Some(one) if one.0.is_empty() => *one = (stage, sp),
            Some(one) => one.1 = one.1.min(sp),
            None => {}
        });
    }

    /// Every stage entered on this thread since the last take, with its lowest entry address.
    pub(crate) fn take() -> std::vec::Vec<(&'static str, usize)> {
        let taken = ENTERED.with_borrow_mut(|entered| core::mem::replace(entered, [("", 0); SLOTS]));
        taken.into_iter().filter(|one| !one.0.is_empty()).collect()
    }
}

#[cfg(test)]
type UndefinedReads = (blaze_own::AVec<'static, (Name, CellId)>, u32, u32);

/// The command tests inspect every read, including reads of standing tasks, and count every
/// head row. Neither observation contributes to the command's findings, status or saved answer.
#[cfg(test)]
fn audit_answer(
    answer: &Answer<'_>,
    scratch: SharedAlloc<'_>,
    alloc: blaze_own::Alloc<'static>,
) -> Result<(UndefinedReads, u32, u32), CliError> {
    let mut undefined = (Vec::new_in(alloc), 0u32, 0u32);
    let mut heads = 0u32;
    let mut lint_rows = 0u32;
    let mut names = SVec::new_in(scratch);
    answer.visit_rows(|name| { names.push(name); Ok(()) }).map_err(EngineError::from)?;
    names.sort_unstable();
    for name in names {
        let row = answer.row_facts(name).map_err(EngineError::from)?.ok_or(EngineCheckError::MissingIndexedRow { name })?;
        if row.phase.stage == Stage::Heads {
            heads = heads.saturating_add(1);
        }
        if row.phase.stage == Stage::Lints {
            lint_rows = lint_rows.saturating_add(1);
        }
        let reads = answer.observations(name).map_err(EngineError::from)?.ok_or(EngineCheckError::MissingIndexedRow { name })?;
        for (id, _) in reads {
            // An absent cell is observable when its defining task has a row, e.g. an empty
            // impl bucket. A read with neither a value nor a defining row has no such witness.
            if answer.has_cell(&id).map_err(EngineError::from)? || answer.row_facts(id.of).map_err(EngineError::from)?.is_some() {
                continue;
            }
            if id.of.kind == Kind::Signature {
                undefined.2 = undefined.2.saturating_add(1);
            } else {
                undefined.1 = undefined.1.saturating_add(1);
                if undefined.0.len() < 8 {
                    undefined.0.push((name, id));
                }
            }
        }
    }
    Ok((undefined, heads, lint_rows))
}

/// One group of the "entered because" table: the stage, the cause's kind, and the task that
/// defined the moved cell (a read moved; none for the other causes).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Because {
    pub(crate) stage: Stage,
    pub(crate) cause: &'static str,
    pub(crate) definer: Option<Name>,
    /// The kind of the moved cell (engine_scope_reads.rs `CellKind`; a read moved; none for
    /// the other causes): `whole record` names a coarse read.
    pub(crate) cell: Option<crate::engine_scope_reads::CellKind>,
}

/// How many tasks each [`Because`] entered, the most first, then by the group (rule 25: the
/// order is the data's).
fn entered_because<'a>(causes: &[blaze_engine::Entered], alloc: SharedAlloc<'a>) -> SVec<'a, (Because, u32)> {
    let mut groups: SMap<'_, Because, u32> = SMap::new_in(alloc);
    for one in causes.iter() {
        *groups.entry(because_of(one)).or_insert(0) += 1;
    }
    let mut sorted: SVec<'a, (Because, u32)> = SVec::new_in(alloc);
    sorted.extend(groups.iter().map(|(group, count)| (*group, *count)));
    sorted.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    sorted
}

/// The group one entered task falls in ([`entered_because`]): one home for the rule, so the
/// profile's tasks under a group line are exactly the group's.
fn because_of(one: &blaze_engine::Entered) -> Because {
    let (definer, cell) = match one.cause {
        blaze_engine::Cause::ReadMoved { read, .. } => (Some(read.of), Some(crate::engine_scope_reads::CellKind::of(&read))),
        _ => (None, None),
    };
    Because { stage: one.phase.stage, cause: one.cause.kind(), definer, cell }
}

/// The most groups the profile lists.
const BECAUSE_SHOWN: usize = 20;

/// The most entered tasks the profile names under one group line (their kind and subject), in
/// the order the engine entered them.
const BECAUSE_TASKS_SHOWN: usize = 3;

/// Stopped by `by` at a scheduling point: the status, each stage's memory and records and the
/// phases so far, to the profile (replaced whole by a rename) and to stderr; the typed stop.
/// When the watcher claimed the stop first (the workers were late), it writes it and ends the
/// process, so this writes nothing.
#[expect(clippy::too_many_arguments, reason = "the stop's edge: each argument one handle of it")]
fn stopped(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    err: &Out,
    board: &crate::engine_status::Board<'_>,
    by: bstd::procs::By,
    ledger: &crate::engine_store::Ledger,
    names: Option<&NamesStage<'_>>,
    phases: &Phases,
    clock: &bstd::clock::Clock,
    frame: &Frame,
) -> Result<CliError, CliError> {
    use core::fmt::Write as _;
    let now = board.now(clock);
    let error = EngineCheckError::Stopped { by, stage: board.stage(), at_ms: now / 1_000_000 };
    if !board.claim(crate::engine_status::Claim::Driver) {
        return Ok(error.into());
    }
    let mut text = blaze_own::String::new_in(frame);
    // A report: a full frame cuts it short where it stood (as `profile_text`).
    let _ = crate::engine_status::write_status(board, crate::engine_status::Heading::Stopped { by, stuck: false }, now, &mut text);
    crate::engine_memory::profile(ledger, names, &mut text);
    ledger.write(&mut text);
    let _ = write!(text, "{phases}");
    crate::engine_status::put(cx, root, err, text.as_bytes())?;
    Ok(error.into())
}

/// The profile's text: each phase's tasks planned and entered, each stage's entries and how many
/// came out the bytes they had (a wasted entry), the top groups of why tasks were entered, then
/// the command's phases.
fn profile_text<'f>(ran: &[(Phase, u32, u32)], report: &blaze_engine::Report, because: &[(Because, u32)], causes: &[blaze_engine::Entered], outlined: &[Outlined<'_>], phases: &Phases, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
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
    let settle = report.settle;
    let _ = writeln!(text, "finish: owed cells {}, settled {}, saw calls {}, reads compared {}, {} ms", settle.owed, settle.settled, settle.saw_calls, settle.compared, settle.finish_ns / 1_000_000);
    let index = &report.index;
    let _ = writeln!(
        text,
        "index: read edges {}, {} bytes ({} an edge, floor {}), allocations {} (at most {} a phase), runs {}, sorts {}",
        index.edges,
        index.bytes,
        index.bytes_per_edge(),
        blaze_engine::FLOOR_BYTES_PER_EDGE,
        index.allocations,
        index.phase_allocations_max,
        index.runs,
        index.seals
    );
    let _ = writeln!(text, "entered because (stage, cause, the moved cell's task and kind: tasks), top {BECAUSE_SHOWN}");
    for (group, count) in because.iter().take(BECAUSE_SHOWN) {
        let kind = group.cell.map_or("", crate::engine_scope_reads::CellKind::words);
        let _ = match group.definer {
            None => writeln!(text, "  {:?} {}: {count}", group.stage, group.cause),
            // A file's cell by its path, read as the sweep read it; any other task by its name.
            Some(name) => match outlined.binary_search_by(|one| one.name.cmp(&name)).ok().and_then(|at| core::str::from_utf8(&outlined[at].path).ok()) {
                Some(path) => writeln!(text, "  {:?} {} {:?} {path} ({kind}): {count}", group.stage, group.cause, name.kind),
                None => writeln!(text, "  {:?} {} {name:?} ({kind}): {count}", group.stage, group.cause),
            },
        };
        // Which tasks it entered, by kind and subject: the group key holds no entered task, so
        // tests that compare groups are unmoved by this detail.
        for one in causes.iter().filter(|one| because_of(one) == *group).take(BECAUSE_TASKS_SHOWN) {
            let _ = writeln!(text, "      entered {:?} {:?}", one.name.kind, one.name.subject);
        }
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
    // The graph's crate cells (D187, engine_units.rs `crate_cells`): a settled crate's reader
    // reads its own crate's cell, so a lock bump of one dependency wakes only its readers. An
    // unreadable payload is the task's failure, never a graph without its cells (rule 3).
    if task.planned.name == Name::unit_graph() {
        let cells = record.payload_bytes().ok().map(|payload| crate::engine_units::crate_cells(payload, record.out, &mut defines));
        if !matches!(cells, Some(Ok(()))) {
            return crate::engine_tasks::failed(task, alloc);
        }
        defines.sort_unstable_by_key(|(cell, _)| *cell);
    }
    let mut reads: SVec<'a, (CellId, blaze_ids::ContentId)> = SVec::new_in(alloc);
    for (name, digest) in record.reads.iter() {
        reads.push((CellId { of: *name, cell: WHOLE }, *digest));
    }
    reads.sort_unstable_by_key(|(id, _)| *id);
    reads.dedup_by_key(|(id, _)| *id);
    Done { name: task.planned.name, key: task.key, out: record.out, defines, reads, findings: u32::try_from(record.findings.len()).unwrap_or(u32::MAX), failed: false }
}

/// The command's status from what was printed, as main.rs `close`: an error or a failure is 1;
/// items not checked 4; a stage not run, or a row's findings not printed, 3. The certificate and
/// a request that enters tasks take it from the same lines, so they exit alike.
/// The outcome is a function of the findings' tally (blaze_report::query `outcome`, D164); this
/// only maps it onto the exit statuses.
pub(crate) fn status_of(tally: &crate::engine_lines::Tally, unprinted: u32, shown: &print::Shown<'_>, partial: bool) -> Status {
    use blaze_report::query::{Counts, Outcome, outcome};
    let counts = Counts {
        errors: u64::from(shown.counts.errors),
        failures: u64::from(shown.counts.failures) + u64::from(tally.failures),
        unchecked: u64::from(tally.unchecked) + u64::from(tally.heads),
    };
    match outcome(&counts, partial || unprinted > 0) {
        Outcome::Failed => Status::FAILED,
        Outcome::ItemsNotChecked => crate::UNCHECKED_ITEMS,
        Outcome::Incomplete => crate::INCOMPLETE,
        Outcome::Ok => Status::OK,
    }
}

/// A file the walk reached and has not outlined: its `FileItems` name, the slot its part named,
/// whether a `mod x;` named it (its file is at `x.rs` or `x/mod.rs`), and its edition byte.
struct Want<'h> {
    name: Name,
    slot: SVec<'h, u8>,
    module: bool,
    edition: u8,
    /// The root file of the crate target whose module tree reached it first.
    root: Name,
    /// Its place's found flags ([`PLACE_FOUND`]): a found file's, the units whose names read it
    /// (the walk took it from the last answer's place, [`plan`]), kept until the names say again
    /// ([`serve_found`]); 0 for a file a part names, a found file's parts too (each unit's reach
    /// walks them from the found file under its cuts, engine_body_plan.rs `reached_under`).
    found: u8,
    /// Whether this file belongs only to a non-member checkout path package (`PLACE_EXTERNAL`):
    /// outlined for `CrateScope` and tracked in the sweep leaf table, never linted or checked as
    /// a workspace member's bodies or heads.
    external: bool,
    #[cfg(test)]
    text_only_outline: bool,
}

struct SkippedTarget {
    root: Name,
    files: u32,
}

struct Skipped<'h> {
    files: SVec<'h, Name>,
    /// Files belonging exclusively to non-member checkout path packages (`PLACE_EXTERNAL`),
    /// sorted: outlined for `CrateScope` and sweep invalidation, never planned for bodies, heads
    /// or lints.
    external: SVec<'h, Name>,
    /// The targets the walk found skipped, each note made now ([`lines_of`]); empty on the
    /// ready path, which walks nothing.
    targets: SVec<'h, SkippedTarget>,
    /// The ready path's: each skipped target's note carried from the last lines record (its
    /// `FileItems` block's Told entries), as the phase path would make it again: no layout row
    /// moved, so the same targets skip the same files ([`ready_setup`] checks each is there).
    carried: bool,
}

impl Want<'_> {
    fn outline_source(&self, source_with_edition: blaze_ids::ContentId, flags: u8) -> blaze_ids::ContentId {
        #[cfg(test)]
        if self.text_only_outline {
            return source_with_edition;
        }
        crate::engine_tasks::outline_source(source_with_edition, flags)
    }
}

#[cfg(test)]
std::thread_local! {
    static TEXT_ONLY_OUTLINE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Plant the old source for a request's file wants, including those sent to workers.
#[cfg(test)]
pub(crate) fn with_text_only_outlines<R>(run: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEXT_ONLY_OUTLINE.set(self.0);
        }
    }
    let _restore = Restore(TEXT_ONLY_OUTLINE.replace(true));
    run()
}

#[cfg(test)]
std::thread_local! {
    static WITHOUT_FOUND_SERVE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    static FOUND_CAP_PLANT: core::cell::Cell<Option<usize>> = const { core::cell::Cell::new(None) };
}

/// The most files one crate's names may find in one request (rule 18), each outlined on its
/// names worker and filed after the names ([`serve_found`]): the bound is a crate's module and
/// data files a macro names, so a crate's own file count, not a re-run count (the engine's
/// `NEEDS_CAP` bounds one task's cells per run, and a task past it runs again: a found file is
/// served in the run that read it, never again). Past it, [`EngineCheckError::FoundTooMany`]
/// names the crate and the count.
pub(crate) const FOUND_CAP: usize = 4096;

/// Run `run` with the found cap at `cap` (a test's: the cap's refusal with a few files, never
/// [`FOUND_CAP`] of them). Each request reads it when it starts ([`OutlinePlants::now`]).
#[cfg(test)]
pub(crate) fn with_found_cap<R>(cap: usize, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FOUND_CAP_PLANT.set(self.0);
        }
    }
    let _restore = Restore(FOUND_CAP_PLANT.replace(Some(cap)));
    run()
}

/// Plant the names before they were served the files they find (D161): a crate's names that
/// read a file no outline holds stop by [`EngineCheckError::UnseenFile`], as before this piece
/// (the control that removes the cause, rule 23, engine_found_files_tests.rs).
#[cfg(test)]
pub(crate) fn without_found_serve<R>(run: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            WITHOUT_FOUND_SERVE.set(self.0);
        }
    }
    let _restore = Restore(WITHOUT_FOUND_SERVE.replace(true));
    run()
}

/// A test's plants for the outlines a names worker makes of the files its crate found
/// ([`outline_found`]): read on the command's thread when the request starts, since the worker's
/// thread has its own (each [`Want`] carries them the same way); and the rule set the request
/// lints with, for which such an outline lists its lint units as the walk's do ([`outline_one`]).
#[derive(Clone, Copy)]
pub(crate) struct OutlinePlants {
    lints: Option<blaze_lint::RuleSet>,
    #[cfg(test)]
    text_only_outline: bool,
    #[cfg(test)]
    without_found_serve: bool,
    /// The most files one crate's names may find ([`FOUND_CAP`]; a test's [`with_found_cap`]).
    found_cap: usize,
}

impl OutlinePlants {
    pub(crate) fn now(lints: Option<blaze_lint::RuleSet>) -> OutlinePlants {
        #[cfg(test)]
        let found_cap = FOUND_CAP_PLANT.get().unwrap_or(FOUND_CAP);
        #[cfg(not(test))]
        let found_cap = FOUND_CAP;
        OutlinePlants {
            lints,
            #[cfg(test)]
            text_only_outline: TEXT_ONLY_OUTLINE.get(),
            #[cfg(test)]
            without_found_serve: WITHOUT_FOUND_SERVE.get(),
            found_cap,
        }
    }

    /// The most files one crate's names may find this request ([`FOUND_CAP`]).
    pub(crate) fn found_cap(self) -> usize {
        self.found_cap
    }

    /// Whether a crate's names are served the files they find ([`without_found_serve`]).
    pub(crate) fn serves(self) -> bool {
        #[cfg(test)]
        {
            !self.without_found_serve
        }
        #[cfg(not(test))]
        {
            true
        }
    }
}

/// Every file of the members' crates, outlined, sorted by name: found forward from the crate
/// roots, a wave at a time, each wave's files outlined at once on the workers.
///
/// A file the sweep proved unchanged (its slot in [`Kept::slots`], so no stat and no read here)
/// whose stored `Outlines` row stands by today's source is not read: its outline is read back
/// from the record its row names ([`crate::engine_store::outline_from`]). Every other file is
/// probed (unless the sweep found it) and outlined on the workers.
///
/// Then the FOUND FILES (D161): each of `found_last` (the last answer's places with a found flag,
/// [`PLACE_FOUND`]) the walk from the roots did not reach is walked from as a root is, at the
/// slot the sweep followed it at, with its place's flags; what it reaches takes them too. The
/// names say after their stages whether each is still read ([`serve_found`]). One whose slot the
/// sweep did not follow is not walked: its rows go with the last answer's plan of it, and a
/// names task that still reads it finds it again.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn plan<'h, 'm>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &FromMeta,
    laid: &crate::units::Laid<'_>,
    workers: NonZeroU32,
    kept: &Kept<'_, '_, '_>,
    store: &mut dyn crate::store::Store<'m>,
    names: Option<&crate::names::Names<'_>>,
    found_last: &[(Name, blaze_engine::pages::FilePlace)],
    lints: Option<blaze_lint::RuleSet>,
    alloc: SharedAlloc<'h>,
) -> Result<(SVec<'h, Outlined<'h>>, SVec<'h, (SVec<'h, u8>, sweep::Probed)>, Files, Skipped<'h>, SVec<'h, (Name, blaze_engine::pages::FilePlace)>), CliError> {
    let mut files = Files::default();
    // Each outlined file's place, kept in the root beside its plan digest (blaze-engine pages.rs
    // `FilePlace`): the ready edit re-plans an edited file from it alone (D187).
    let mut places: SVec<'h, (Name, blaze_engine::pages::FilePlace)> = SVec::new_in(alloc);
    // Each file's slot and what its probe found: its row in the leaf table the next request's
    // sweep follows (engine_answer.rs).
    let mut probes: SVec<'h, (SVec<'h, u8>, sweep::Probed)> = SVec::new_in(alloc);
    // Rust module files (`seen_rust`) and `DATA_FILE` includes (`seen_data`, deferred into
    // `pending_data`) are tracked separately: all Rust waves (plus `found_last`) run first so a
    // file reached BOTH via `include_str!` / `include_bytes!` AND via `mod x;` is always outlined
    // once as a Rust module (with `Surface`, bodies, heads, and raw-bytes `DATA` cell), never
    // shadowed by an earlier `DATA_FILE` wave regardless of walk order.
    let mut seen_rust: SSet<'h, Name> = SSet::new_in(alloc);
    let mut seen_data: SSet<'h, Name> = SSet::new_in(alloc);
    let mut wave: SVec<'h, Want<'h>> = SVec::new_in(alloc);
    let mut pending_data: SVec<'h, Want<'h>> = SVec::new_in(alloc);
    let mut selected_roots: SSet<'h, Name> = SSet::new_in(alloc);
    let mut skipped_roots: SSet<'h, Name> = SSet::new_in(alloc);
    if let Some(names) = names {
        let selected: SSet<'h, Name> = {
            let mut selected = SSet::new_in(alloc);
            let list = names.list();
            for r in 0..blaze_world::crate_list::CrateGraph::len(list) {
                if let Some(root) = list.root(r).strip_prefix("/0/") {
                    selected.insert(Name::file(root.as_bytes()));
                }
            }
            selected
        };
        for (_, made) in laid.members.iter() {
            for part in made.parts.iter() {
                // A member units record lists every target root. The names graph lists only
                // selected units, so an unlisted root is a required-features-skipped target.
                if part.name.kind == Kind::FileItems && !selected.contains(&part.name) {
                    skipped_roots.insert(part.name);
                } else if part.name.kind == Kind::FileItems {
                    selected_roots.insert(part.name);
                }
            }
        }
    }
    for (_, made) in laid.members.iter() {
        wants(&made.parts, &made.paths, &made.path_bytes, None, false, &mut seen_rust, &mut seen_data, &mut wave, &mut pending_data, alloc)?;
    }
    // Reached non-member checkout path packages (`[patch.*]` and excluded/transitive path deps):
    // walk their source trees as `PLACE_EXTERNAL` leaves so `CrateScope` reads their `Surface`
    // cells and editing them invalidates soundly without checking them as workspace members.
    if let Some(names) = names {
        let list = names.list();
        for r in 0..blaze_world::crate_list::CrateGraph::len(list) {
            if !list.member(r)
                && let Some(root_path) = list.root(r).strip_prefix("/0/")
            {
                let file = Name::file(root_path.as_bytes());
                if !seen_rust.insert(file) {
                    continue;
                }
                let edition = laid
                    .path_roots
                    .iter()
                    .find(|(path, _)| *path == root_path)
                    .map_or(blaze_surface::Edition::E2021.byte(), |(_, ed)| *ed);
                let mut slot = SVec::new_in(alloc);
                slot.extend_from_slice(root_path.as_bytes());
                wave.push(Want {
                    name: file,
                    slot,
                    module: false,
                    edition,
                    root: file,
                    found: 0,
                    external: true,
                    #[cfg(test)]
                    text_only_outline: TEXT_ONLY_OUTLINE.get(),
                });
            }
        }
    }
    let mut outlined: SVec<'h, Outlined<'h>> = SVec::new_in(alloc);
    // The found files' wants once the roots' walk is done (above): a file it reached is not one.
    // Pure data includes (`pending_data`) flush only after all Rust files (roots + found) finish.
    let mut found_walked = false;
    let mut data_walked = false;
    if wave.is_empty() {
        found_walked = true;
        found_wants(kept, found_last, &mut seen_rust, &mut seen_data, &mut wave, &mut pending_data, alloc)?;
    }
    if wave.is_empty() && !data_walked {
        data_walked = true;
        for want in core::mem::replace(&mut pending_data, SVec::new_in(alloc)) {
            if !seen_rust.contains(&want.name) {
                wave.push(want);
            }
        }
    }
    while !wave.is_empty() {
        let scope = cx.scope();
        scope.narrow_workers(workers, Class::Foreground);
        // One a file of the wave: project-sized, so never in the command's fixed frame.
        // Each want of the wave in its order: read back (in parallel chunks from the lock-free
        // snapshot when a stored answer is open), or a worker's `outline_one` handle.
        let mut handles = Vec::with_capacity_in(wave.len(), alloc);
        let mut stopped = None;
        if let Some(stored) = kept.stored && wave.len() >= 8 && workers.get() > 1 {
            enum StandingErr {
                Pages(blaze_engine::pages::PagesError),
                MissingRecord(Name),
                OutlineBack(crate::engine_store::OutlineBack),
                Snapshot(blaze_cas::plain::SnapshotError),
                NotUtf8,
            }
            impl StandingErr {
                fn into_cli(self) -> CliError {
                    match self {
                        StandingErr::Pages(error) => crate::engine_answer::page_error(error),
                        StandingErr::MissingRecord(name) => EngineCheckError::MissingRecord { name }.into(),
                        StandingErr::OutlineBack(why) => EngineCheckError::OutlineBack(why).into(),
                        StandingErr::Snapshot(error) => crate::store::PlaceError::Snapshot(error).into(),
                        StandingErr::NotUtf8 => CliError::Text { what: "a path is not UTF-8" },
                    }
                }
            }
            enum StandingOut<'h> {
                Done(Outlined<'h>, sweep::Probed),
                NeedStore { probed: sweep::Probed, key: blaze_drive::Key, source: blaze_ids::ContentId, outline_source: blaze_ids::ContentId },
                NeedOutline { known: Option<sweep::Probed> },
                Err(StandingErr),
            }
            let slots = kept.slots;
            let tools = kept.tools;
            let read = &kept.read;
            let chunk = wave.len().div_ceil(usize::try_from(workers.get()).unwrap_or(1)).max(4);
            let mut chunk_handles = Vec::with_capacity_in(wave.len().div_ceil(chunk), alloc);
            for slice in wave.chunks(chunk) {
                chunk_handles.push(scope.may_overlap("outline standing", 64 << 10, move |_cx: &Cx| -> Result<_, Infallible> {
                    let mut out = Vec::with_capacity_in(slice.len(), alloc);
                    for want in slice {
                        let known = slot_of(slots, want);
                        let Some(probed) = known else {
                            out.push(StandingOut::NeedOutline { known: None });
                            continue;
                        };
                        let standing = match stood_from_stored(stored, tools, read, want, probed) {
                            Ok(Some(one)) => one,
                            Ok(None) => {
                                out.push(StandingOut::NeedOutline { known });
                                continue;
                            }
                            Err(StoodErr::Pages(error)) => {
                                out.push(StandingOut::Err(StandingErr::Pages(error)));
                                continue;
                            }
                            Err(StoodErr::MissingRecord(name)) => {
                                out.push(StandingOut::Err(StandingErr::MissingRecord(name)));
                                continue;
                            }
                        };
                        let (key, source, outline_source) = standing;
                        match stored.snapshot_record(key) {
                            Ok(Some(bytes)) => match crate::engine_store::outline_from(want.name, key, bytes, alloc) {
                                Ok(record) => match core::str::from_utf8(&want.slot) {
                                    Ok(slot) => {
                                        let path = sweep::real_path_in(slot, probed.0, alloc);
                                        out.push(StandingOut::Done(Outlined { name: want.name, path, source, outline_source, edition: want.edition, made: Ok(record) }, probed));
                                    }
                                    Err(_) => out.push(StandingOut::Err(StandingErr::NotUtf8)),
                                },
                                Err(why) => out.push(StandingOut::Err(StandingErr::OutlineBack(why))),
                            },
                            Ok(None) => out.push(StandingOut::NeedStore { probed, key, source, outline_source }),
                            Err(error) => out.push(StandingOut::Err(StandingErr::Snapshot(error))),
                        }
                    }
                    Ok(out)
                }));
            }
            let mut wave_iter = wave.iter();
            for chunk_handle in chunk_handles {
                let part = match chunk_handle.join(cx) {
                    Ok(part) => part,
                    Err(error) => {
                        stopped.get_or_insert(CliError::Pool(error));
                        continue;
                    }
                };
                for item in part {
                    let Some(want) = wave_iter.next() else {
                        break;
                    };
                    match item {
                        StandingOut::Done(one, probed) => {
                            files.read_back += 1;
                            handles.push(Err(Ok((one, probed))));
                        }
                        StandingOut::NeedStore { probed, key, source, outline_source } => {
                            let back = match store.get(cx, key) {
                                Err(error) => Err(error),
                                Ok(None) => Err(EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name: want.name, key }).into()),
                                Ok(Some(held)) => crate::engine_store::outline_from(want.name, key, held.bytes, alloc).map_err(|why| EngineCheckError::OutlineBack(why).into()),
                            };
                            match back {
                                Ok(record) => {
                                    files.read_back += 1;
                                    let slot = core::str::from_utf8(&want.slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
                                    let path = sweep::real_path_in(slot, probed.0, alloc);
                                    handles.push(Err(Ok((Outlined { name: want.name, path, source, outline_source, edition: want.edition, made: Ok(record) }, probed))));
                                }
                                Err(error) => {
                                    stopped.get_or_insert(error);
                                    handles.push(Err(Err(())));
                                }
                            }
                        }
                        StandingOut::NeedOutline { known } => {
                            files.outlined += 1;
                            files.probed += u32::from(known.is_none());
                            handles.push(Ok(scope.may_overlap("outline", TASK_FRAME, move |cx: &Cx| Ok::<_, Infallible>(outline_one(cx, root, stamps, want, known, lints, alloc)))));
                        }
                        StandingOut::Err(error) => {
                            stopped.get_or_insert(error.into_cli());
                            handles.push(Err(Err(())));
                        }
                    }
                }
            }
        } else {
            for want in wave.iter() {
                let known = kept.slot(want);
                let standing = match known {
                    Some(probed) => match kept.stood(want, probed) {
                        Ok(standing) => standing.map(|one| (probed, one)),
                        Err(error) => {
                            stopped.get_or_insert(error);
                            handles.push(Err(Err(())));
                            continue;
                        }
                    },
                    None => None,
                };
                match standing {
                    Some((probed, (key, source, outline_source))) => {
                        // The store's own error stops the plan after the wave's workers are joined.
                        let back = match store.get(cx, key) {
                            Err(error) => Err(error),
                            Ok(None) => Err(EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name: want.name, key }).into()),
                            Ok(Some(held)) => crate::engine_store::outline_from(want.name, key, held.bytes, alloc).map_err(|why| EngineCheckError::OutlineBack(why).into()),
                        };
                        match back {
                            Ok(record) => {
                                files.read_back += 1;
                                let slot = core::str::from_utf8(&want.slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
                                let path = sweep::real_path_in(slot, probed.0, alloc);
                                handles.push(Err(Ok((Outlined { name: want.name, path, source, outline_source, edition: want.edition, made: Ok(record) }, probed))));
                            }
                            Err(error) => {
                                stopped.get_or_insert(error);
                                handles.push(Err(Err(())));
                            }
                        }
                    }
                    None => {
                        files.outlined += 1;
                        files.probed += u32::from(known.is_none());
                        handles.push(Ok(scope.may_overlap("outline", TASK_FRAME, move |cx: &Cx| Ok::<_, Infallible>(outline_one(cx, root, stamps, want, known, lints, alloc)))));
                    }
                }
            }
        }
        let mut next: SVec<'h, Want<'h>> = SVec::new_in(alloc);
        for (handle, want) in handles.into_iter().zip(wave.iter()) {
            let joined = match handle {
                Ok(handle) => handle.join(cx).map(|one| one.map_err(|stopped| stopped.of(want))),
                Err(Ok(back)) => Ok(Ok(back)),
                Err(Err(())) => continue,
            };
            match joined {
                Ok(Ok((one, probed))) => {
                    if let Ok(record) = &one.made {
                        wants(&record.parts, &record.paths, &record.path_bytes, Some(want.root), want.external, &mut seen_rust, &mut seen_data, &mut next, &mut pending_data, alloc)?;
                    }
                    let flags = (if want.module { PLACE_MODULE } else { 0 }) | want.found | (if want.external { PLACE_EXTERNAL } else { 0 });
                    places.push((want.name, blaze_engine::pages::FilePlace { unit: want.root, edition: want.edition, flags }));
                    outlined.push(one);
                    let mut slot = SVec::new_in(alloc);
                    slot.extend_from_slice(&want.slot);
                    probes.push((slot, probed));
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
        if wave.is_empty() && !found_walked {
            found_walked = true;
            found_wants(kept, found_last, &mut seen_rust, &mut seen_data, &mut wave, &mut pending_data, alloc)?;
        }
        if wave.is_empty() && !data_walked {
            data_walked = true;
            for want in core::mem::replace(&mut pending_data, SVec::new_in(alloc)) {
                if !seen_rust.contains(&want.name) {
                    wave.push(want);
                }
            }
        }
    }
    outlined.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    let mut selected_files: SSet<'h, Name> = SSet::new_in(alloc);
    let mut selected_next: SVec<'h, Name> = SVec::new_in(alloc);
    selected_next.extend(selected_roots.iter().copied());
    while let Some(name) = selected_next.pop() {
        if !selected_files.insert(name) {
            continue;
        }
        let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(&name)) else { continue };
        let Ok(record) = &outlined[at].made else { continue };
        selected_next.extend(record.parts.iter().filter_map(|part| match part.locate {
            Locate::File { edition } if !crate::data_include::is_data(edition) => Some(part.name),
            // A data include, source digest or keyed record: no Rust module of this target to follow.
            Locate::File { .. } | Locate::Source(_) | Locate::Key(_) => None,
        }));
    }
    let mut skipped_files: SSet<'h, Name> = SSet::new_in(alloc);
    let mut skipped_targets: SVec<'h, SkippedTarget> = SVec::new_in(alloc);
    let mut roots = SVec::new_in(alloc);
    roots.extend(skipped_roots.iter().copied());
    roots.sort_unstable();
    for root_name in roots.iter().copied() {
        let before = skipped_files.len();
        let mut next: SVec<'h, Name> = SVec::new_in(alloc);
        next.push(root_name);
        while let Some(name) = next.pop() {
            if selected_files.contains(&name) || !skipped_files.insert(name) {
                continue;
            }
            let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(&name)) else { continue };
            let Ok(record) = &outlined[at].made else { continue };
            next.extend(record.parts.iter().filter_map(|part| match part.locate {
                Locate::File { edition } if !crate::data_include::is_data(edition) => Some(part.name),
                Locate::File { .. } | Locate::Source(_) | Locate::Key(_) => None,
            }));
        }
        skipped_targets.push(SkippedTarget { root: root_name, files: u32::try_from(skipped_files.len().saturating_sub(before)).unwrap_or(u32::MAX) });
    }
    skipped_targets.sort_unstable_by_key(|target| target.root);
    let mut skipped_file_names = SVec::new_in(alloc);
    skipped_file_names.extend(skipped_files.iter().copied());
    skipped_file_names.sort_unstable();
    let mut external_files = SVec::new_in(alloc);
    for (file, place) in places.iter_mut() {
        if skipped_file_names.binary_search(file).is_ok() {
            place.flags |= PLACE_SKIPPED;
        }
        if place.flags & PLACE_EXTERNAL != 0 {
            external_files.push(*file);
        }
    }
    external_files.sort_unstable();
    external_files.dedup();
    Ok((outlined, probes, files, Skipped { files: skipped_file_names, external: external_files, targets: skipped_targets, carried: false }, places))
}

/// A [`blaze_engine::pages::FilePlace`]'s flag: a `mod x;` named the file (its file is at
/// `x.rs` or `x/mod.rs`), the module form its outline is made in ([`Want::module`]).
pub(crate) const PLACE_MODULE: u8 = 1;
/// A [`blaze_engine::pages::FilePlace`]'s flag: the file is reached only from a target its
/// features skip ([`Skipped`]): it has an outline and no body task.
pub(crate) const PLACE_SKIPPED: u8 = 2;
/// A [`blaze_engine::pages::FilePlace`]'s flag: a non-test member unit whose root is the place's
/// `unit` read the file through its names (D161), and no `mod` the walk follows names it: a
/// module a macro wrote from a computed name, a data file of a computed path. That unit's tree
/// holds it (engine_body_plan.rs [`crate::engine_body_plan::FoundEdge`]): its resolution decided
/// every cfg on the way under the unit's own cfg.
pub(crate) const PLACE_FOUND_LIB: u8 = 4;
/// As [`PLACE_FOUND_LIB`], by the test unit of the place's root.
pub(crate) const PLACE_FOUND_TEST: u8 = 8;
/// Both found flags: a place with either is a found file's.
pub(crate) const PLACE_FOUND: u8 = PLACE_FOUND_LIB | PLACE_FOUND_TEST;
/// A [`blaze_engine::pages::FilePlace`]'s flag: the file belongs only to a non-member checkout
/// path package (`[patch.*]` or excluded/transitive path dependency): outlined for `CrateScope`
/// and sweep invalidation, never linted or checked as a workspace member's bodies or heads.
pub(crate) const PLACE_EXTERNAL: u8 = 16;

/// The found flag of a member unit by whether it is a test build.
pub(crate) const fn found_bit(test: bool) -> u8 {
    if test { PLACE_FOUND_TEST } else { PLACE_FOUND_LIB }
}

/// The stored answer's units-stage rows as the units stage reads them: one header row at a time,
/// as it asks (engine_units.rs `StoredUnits`), never the whole stage up front.
struct AnswerUnits<'s, 'k, 'a> {
    stored: Option<&'s crate::engine_answer::Stored<'a>>,
    /// Its own lifetime: the record keys borrow the stored answer for less than the answer's
    /// own `'a` (one lifetime for both asked the borrow to outlive the value it borrows).
    records: &'s RecordKeys<'k>,
    alloc: SharedAlloc<'a>,
}

impl<'a> crate::engine_units::StoredUnits<'a> for AnswerUnits<'_, '_, 'a> {
    fn names(&self) -> &[Name] {
        self.stored.map_or(&[][..], |stored| &stored.root.units[..])
    }

    fn row(&self, name: Name) -> Result<Option<blaze_engine::Row<'a>>, CliError> {
        let Some(stored) = self.stored else { return Ok(None) };
        let row = stored.planner_row(name, self.alloc).map_err(crate::engine_answer::page_error)?;
        // A row the answer's directory lists and its headers do not hold is a damaged answer
        // (rule 3), as the eager read refused it.
        if row.is_none() && stored.root.units.contains(&name) {
            return Err(EngineCheckError::MissingIndexedRow { name }.into());
        }
        Ok(row)
    }

    fn record(&self, name: Name) -> Result<Option<blaze_drive::Key>, CliError> {
        self.records.get(name)
    }
}

/// Changes to the task payload column. An entered task producing no payload shadows its
/// previous key just as a replacement payload does.
struct RecordKeys<'a> {
    base: Option<&'a dyn StoredPages>,
    changes: SMap<'a, Name, Option<blaze_drive::Key>>,
}

impl RecordKeys<'_> {
    fn get(&self, name: Name) -> Result<Option<blaze_drive::Key>, CliError> {
        if let Some(key) = self.changes.get(&name) {
            return Ok(*key);
        }
        let Some(base) = self.base else { return Ok(None) };
        // Through the stored answer's header index as it stands (blaze-engine headers.rs
        // `Headers::header`): a current index answers from its shard, which the whole-checkout
        // plan reads anyway (`Kept::stood` over every outlined file, the Scripts and Names rows),
        // so a lookup costs no page; an absent one (always on the ready path, `checking` makes it
        // so before any read: D187) answers from the row's own page, never a shard. Reading the
        // row's page here on the phase path read a page a row on top of the 128 shards (tempo's
        // edit: 141 pages became 19,096, 2.0 GB mapped).
        Ok(base.header(name).map_err(crate::engine_answer::page_error)?.and_then(|header| header.record))
    }

    /// The stages accept an infallible callback. Return any stored-page refusal before
    /// accepting the stage's result or filing a phase.
    fn lookup<T>(&self, run: impl FnOnce(&dyn Fn(Name) -> Option<blaze_drive::Key>) -> Result<T, CliError>) -> Result<T, CliError> {
        let refused = core::cell::Cell::new(None);
        let result = run(&|name| match self.get(name) {
            Ok(key) => key,
            Err(error) => {
                refused.set(Some(error));
                None
            }
        });
        if let Some(error) = refused.into_inner() {
            return Err(error);
        }
        if let Some(error) = self.base.and_then(StoredPages::refusal) {
            return Err(crate::engine_answer::page_error(error));
        }
        result
    }
}

/// Today's state of the slots followed by the sweep and the preceding answer.
pub(crate) struct Kept<'k, 'a, 's> {
    slots: &'k [(&'k str, sweep::Probed)],
    answer: &'k Answer<'a>,
    records: &'k RecordKeys<'a>,
    /// The open stored answer (when `answer` and `records` read directly from it with no overlay),
    /// enabling a single header lookup per file and lock-free parallel standing read-backs.
    stored: Option<&'k crate::engine_answer::Stored<'s>>,
    /// Today's tools: an outline row stands only under its key from today's outliner
    /// ([`Kept::stood`]): the stored answer is read under other tools too (engine_answer.rs
    /// "THE TOOLS").
    tools: &'k blaze_engine::Tools,
    /// The stored file rows read ([`Kept::stood`]), for the profile's `plan:` line.
    read: core::sync::atomic::AtomicU32,
}

fn slot_of(slots: &[(&str, sweep::Probed)], want: &Want<'_>) -> Option<sweep::Probed> {
    let slot = core::str::from_utf8(&want.slot).ok()?;
    let at = slots.binary_search_by(|(path, _)| (*path).cmp(slot)).ok()?;
    let (_, probed) = *slots.get(at)?;
    ((probed.0 & sweep::MODULE != 0) == want.module).then_some(probed)
}

enum StoodErr {
    Pages(blaze_engine::pages::PagesError),
    MissingRecord(Name),
}

fn stood_from_stored(
    stored: &crate::engine_answer::Stored<'_>,
    tools: &blaze_engine::Tools,
    read: &core::sync::atomic::AtomicU32,
    want: &Want<'_>,
    probed: sweep::Probed,
) -> Result<Option<(blaze_drive::Key, blaze_ids::ContentId, blaze_ids::ContentId)>, StoodErr> {
    if probed.0 & (sweep::ABSENT | sweep::BOTH) != 0 {
        return Ok(None);
    }
    read.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let Some(header) = stored.header(want.name).map_err(StoodErr::Pages)? else {
        return Ok(None);
    };
    let row = header.facts;
    if row.phase.stage != Stage::Outlines || row.failed {
        return Ok(None);
    }
    let source = crate::engine_tasks::edition_source(probed.2, want.edition);
    let outline_source = want.outline_source(source, probed.0);
    // The key too: a row an older outliner wrote (its kind's producer moved) is outlined anew,
    // as the engine would enter it (blaze-engine engine.rs `consider`, `KeyMoved`).
    if row.source != outline_source || row.key != tools.key(want.name, &outline_source) {
        return Ok(None);
    }
    let key = header.record.ok_or(StoodErr::MissingRecord(want.name))?;
    Ok(Some((key, source, outline_source)))
}

impl Kept<'_, '_, '_> {
    /// The want's slot as the sweep found it today, when the last answer read it as this kind
    /// of slot (a `mod x;` slot or a named file).
    fn slot(&self, want: &Want<'_>) -> Option<sweep::Probed> {
        slot_of(self.slots, want)
    }

    /// A standing outline's record key, source with edition and source with placement.
    fn stood(&self, want: &Want<'_>, probed: sweep::Probed) -> Result<Option<(blaze_drive::Key, blaze_ids::ContentId, blaze_ids::ContentId)>, CliError> {
        if let Some(stored) = self.stored {
            return stood_from_stored(stored, self.tools, &self.read, want, probed).map_err(|error| match error {
                StoodErr::Pages(error) => crate::engine_answer::page_error(error),
                StoodErr::MissingRecord(name) => EngineCheckError::MissingRecord { name }.into(),
            });
        }
        if probed.0 & (sweep::ABSENT | sweep::BOTH) != 0 {
            return Ok(None);
        }
        self.read.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let Some(row) = self.answer.row_facts(want.name).map_err(EngineError::from)? else { return Ok(None) };
        if row.phase.stage != Stage::Outlines || row.failed {
            return Ok(None);
        }
        let source = crate::engine_tasks::edition_source(probed.2, want.edition);
        let outline_source = want.outline_source(source, probed.0);
        // The key too: a row an older outliner wrote (its kind's producer moved) is outlined anew,
        // as the engine would enter it (blaze-engine engine.rs `consider`, `KeyMoved`).
        if row.source != outline_source || row.key != self.tools.key(want.name, &outline_source) {
            return Ok(None);
        }
        let key = self.records.get(want.name)?.ok_or(EngineCheckError::MissingRecord { name: want.name })?;
        Ok(Some((key, source, outline_source)))
    }
}

/// The planner's file counts: outlines read back from the store (the file not read), files
/// outlined (each read once), and of those the ones probed here (the sweep had not found them).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Files {
    pub(crate) read_back: u32,
    pub(crate) outlined: u32,
    pub(crate) probed: u32,
}

/// Each file part of a record not seen yet: Rust module/root files go into `rust_into` (deduped
/// by `seen_rust`), while `DATA_FILE` includes go into `data_into` (deduped by `seen_rust` and
/// `seen_data`, flushed only after all Rust waves finish so Rust always wins over `DATA_FILE`).
#[expect(clippy::too_many_arguments, reason = "file-walk accumulator separates Rust and deferred data wants")]
fn wants<'h>(
    parts: &[blaze_drive::Part],
    paths: &[(Name, u32, u32, bool)],
    path_bytes: &[u8],
    // The crate root of the file whose parts these are; `None` for a member's units record,
    // whose every file part is a target's root.
    root: Option<Name>,
    external: bool,
    seen_rust: &mut SSet<'h, Name>,
    seen_data: &mut SSet<'h, Name>,
    rust_into: &mut SVec<'h, Want<'h>>,
    data_into: &mut SVec<'h, Want<'h>>,
    alloc: SharedAlloc<'h>,
) -> Result<(), CliError> {
    for part in parts {
        let Locate::File { edition } = part.locate else { continue };
        let is_data = crate::data_include::is_data(edition);
        if is_data {
            if seen_rust.contains(&part.name) || !seen_data.insert(part.name) {
                continue;
            }
        } else if !seen_rust.insert(part.name) {
            continue;
        }
        let Some(&(_, start, end, module)) = paths.iter().find(|(name, ..)| *name == part.name) else {
            return Err(EngineCheckError::NoPath { name: part.name }.into());
        };
        let mut slot = SVec::new_in(alloc);
        slot.extend_from_slice(&path_bytes[start as usize..end as usize]);
        let want = Want {
            name: part.name, slot, module, edition, root: root.unwrap_or(part.name), found: 0, external,
            #[cfg(test)]
            text_only_outline: TEXT_ONLY_OUTLINE.get(),
        };
        if is_data {
            data_into.push(want);
        } else {
            rust_into.push(want);
        }
    }
    Ok(())
}

/// Each found file of `found_last` (sorted by name) the walk has not reached, as a [`Want`] of
/// `rust_into` (or `data_into` for `DATA_FILE`) at the slot the sweep followed it at
/// ([`Kept::slots`]), its place's crate root, edition, module form and found flags ([`plan`]).
/// One the sweep did not follow is left: no slot today.
fn found_wants<'h>(
    kept: &Kept<'_, '_, '_>,
    found_last: &[(Name, blaze_engine::pages::FilePlace)],
    seen_rust: &mut SSet<'h, Name>,
    seen_data: &mut SSet<'h, Name>,
    rust_into: &mut SVec<'h, Want<'h>>,
    data_into: &mut SVec<'h, Want<'h>>,
    alloc: SharedAlloc<'h>,
) -> Result<(), CliError> {
    if found_last.is_empty() {
        return Ok(());
    }
    for (path, _) in kept.slots.iter() {
        let file = Name::file(path.as_bytes());
        let Ok(at) = found_last.binary_search_by(|(name, _)| name.cmp(&file)) else { continue };
        let Some(&(_, place)) = found_last.get(at) else { continue };
        let is_data = crate::data_include::is_data(place.edition);
        if is_data {
            if seen_rust.contains(&file) || !seen_data.insert(file) {
                continue;
            }
        } else if !seen_rust.insert(file) {
            continue;
        }
        let mut slot = SVec::new_in(alloc);
        slot.extend_from_slice(path.as_bytes());
        let want = Want {
            name: file,
            slot,
            module: place.flags & PLACE_MODULE != 0,
            edition: place.edition,
            root: place.unit,
            found: place.flags & PLACE_FOUND,
            external: place.flags & PLACE_EXTERNAL != 0,
            #[cfg(test)]
            text_only_outline: TEXT_ONLY_OUTLINE.get(),
        };
        if is_data {
            data_into.push(want);
        } else {
            rust_into.push(want);
        }
    }
    Ok(())
}

/// Why a file's outline stopped the plan: sent back from its worker, so `Send` (a `CliError`
/// is made of it on the command's thread). A file that is not there is no stop: it is outlined
/// to its absent answer ([`outline_one`], [`absent_module_out`]).
enum Stopped {
    Unread(Unread),
}

impl Stopped {
    fn of(self, _want: &Want<'_>) -> CliError {
        match self {
            Stopped::Unread(error) => error.into(),
        }
    }
}

/// The domain of [`absent_module_out`]: no file's bytes are hashed in it.
const ABSENT_MODULE_DOMAIN: &str = "blaze-cli a module file at neither of its places 1";

/// The out of a `mod x;` file that is at neither `x.rs` nor `x/mod.rs` (or a `#[path]` file
/// that is not there): an answer of its own, never an empty file's (whose outline lists its
/// `Surface`: creating an empty file moves the row) and never the drive's `ABSENT` (a cell no
/// record defines).
pub(crate) fn absent_module_out() -> blaze_ids::ContentId {
    blaze_ids::ContentId::of(ABSENT_MODULE_DOMAIN, b"")
}

/// One file probed and outlined, on a worker, in its task's frame; the record in `alloc`.
/// `known`: the slot as the sweep found it today, so it is not probed again. `lints`: the rule
/// set the request runs (lints.rs `of_root`), for which the outline lists its lint units.
fn outline_one<'h>(cx: &Cx, root: &Dir<ReadWrite>, stamps: &FromMeta, want: &Want<'_>, known: Option<sweep::Probed>, lints: Option<blaze_lint::RuleSet>, alloc: SharedAlloc<'h>) -> Result<(Outlined<'h>, sweep::Probed), Stopped> {
    let scratch = cx.frame();
    let slot = core::str::from_utf8(&want.slot).map_err(|_| Stopped::Unread(Unread::Text("a path is not UTF-8")))?;
    let probed = match known {
        Some(probed) => probed,
        None => sweep::probe_slot(cx, root, stamps, slot, want.module, scratch).map_err(Stopped::Unread)?,
    };
    let (flags, _, source) = probed;
    let mut path = SVec::new_in(alloc);
    path.extend_from_slice(&sweep::real_path(slot, flags, scratch));
    // A data file that is not there is outlined to its absent answer (work.rs `outline_file`,
    // data_include.rs `absent_out`): rustc reports it once at the call and checks the rest. So
    // is a module's file (rustc's E0583 "file not found for module": an error at the `mod`
    // item, every other item still checked): the module is EMPTY here, its outline no parts and
    // no `Surface`, its out [`absent_module_out`]. The error is the crate's names', which
    // evaluate the `mod` item's cfg (a `#[cfg(windows)] mod sys;` with no file is no error) and
    // read the module's `Surface` as absent, so creating the file (the slot's probe is in this
    // task's key) moves this row and enters them again (engine_names.rs `finish`).
    if flags & sweep::ABSENT != 0 && !crate::data_include::is_data(want.edition) {
        let mut made = work::Record::new_in(want.name, alloc);
        made.out = absent_module_out();
        let source = crate::engine_tasks::edition_source(source, want.edition);
        let outline_source = want.outline_source(source, flags);
        return Ok((Outlined { name: want.name, path, source, outline_source, edition: want.edition, made: Ok(made) }, probed));
    }
    // The outline lists its lint units keyed for the request's rule set (lints.rs `parts_in`:
    // `variant-count`'s fact gathered only when it runs), the set being in the `FileItems`
    // kind's tool (home.rs `tools`) and in each file's `Lint` task's source (engine_lints.rs).
    let job = Job {
        name: want.name,
        slot,
        flags,
        edition: want.edition,
        expect: source,
        lints: if want.external { None } else { lints },
    };
    let made = match work::unwinding(|| work::outline_file(cx, root, &job, scratch, alloc)) {
        Ok(Ok(made)) => made,
        Ok(Err(Unread::Changed)) => Err(Why::Changed),
        Ok(Err(error)) => return Err(Stopped::Unread(error)),
        Err(work::Panicked) => Err(Why::Panicked { used: scratch.peak() as u64, limit: scratch.reserved() as u64 }),
    };
    let source = crate::engine_tasks::edition_source(source, want.edition);
    let outline_source = want.outline_source(source, flags);
    Ok((Outlined { name: want.name, path, source, outline_source, edition: want.edition, made }, probed))
}

/// A file a crate's names found (D161, engine_names.rs `Job::run`), as the walk would have
/// wanted it ([`Want`]): its slot (a module's `x.rs` for `x/mod.rs` too), whether a `mod` names
/// it, its edition byte (its crate's; a data file's `DATA_FILE`), and its crate's root file.
pub(crate) struct FoundWant<'p> {
    pub(crate) slot: &'p str,
    pub(crate) module: bool,
    pub(crate) edition: u8,
    pub(crate) root: Name,
    pub(crate) external: bool,
}

/// One found file probed and outlined, on the names worker that found it, in its task's frame,
/// as [`plan`] outlines a file the walk reached ([`outline_one`]: one code, so the outline, its
/// source and its key are a literal `mod`'s); the record in `alloc`. Filed after the names by
/// [`serve_found`].
pub(crate) fn outline_found<'h>(cx: &Cx, root: &Dir<ReadWrite>, stamps: &FromMeta, found: &FoundWant<'_>, plants: OutlinePlants, alloc: SharedAlloc<'h>) -> Result<(Outlined<'h>, sweep::Probed), Unread> {
    let mut slot = SVec::new_in(alloc);
    slot.extend_from_slice(found.slot.as_bytes());
    let want = Want {
        name: Name::file(found.slot.as_bytes()),
        slot,
        module: found.module,
        edition: found.edition,
        root: found.root,
        found: 0,
        external: found.external,
        #[cfg(test)]
        text_only_outline: plants.text_only_outline,
    };
    match outline_one(cx, root, stamps, &want, None, plants.lints, alloc) {
        Ok(one) => Ok(one),
        Err(Stopped::Unread(error)) => Err(error),
    }
}
