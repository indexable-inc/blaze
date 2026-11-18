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
    /// A list whose size follows the project could not grow: its allocator refused (rule 3).
    Grow { what: &'static str, had: usize },
    /// A crate's `CrateScope` payload whose `cells` section does not read as whole sorted rows
    /// (names/sections.rs, blaze-scope names_cells.rs `TABLE_ROW`): its cells cannot be defined.
    CellTable { task: Name, what: &'static str },
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
            EngineCheckError::Grow { what, had } => write!(f, "{what}: the list could not grow past {had} entries: its allocator refused (engine_*.rs)"),
            EngineCheckError::CellTable { task, what } => write!(f, "{task:?}: its record's cell table {what} (engine_names.rs `cells_of`)"),
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
    serve(cx, io, out, phases, heap, None)
}

/// `blaze why <file or file::item>` (engine.md E11), in the checkout blaze stands in: why its
/// task was entered, step by step back to the edit (engine_why.rs).
pub(crate) fn why(cx: &Cx, io: &Io, out: &Out, phases: &mut Phases, heap: &'static blaze_own::Heap) -> Result<Status, CliError> {
    let args = io.need_args()?;
    let target = args.get(2).filter(|word| !word.starts_with("--")).ok_or(CliError::Usage("blaze why <file or file::item>: a file's path in the checkout, or an item in it as `path::name`"))?;
    let target = work::held_str(target, cx.frame());
    serve(cx, io, out, phases, heap, Some(target))
}

/// The command's edge, read once: the checkout, the tools, the places; then the check, or with
/// `why` its explanation from the stored answer.
fn serve(cx: &Cx, io: &Io, out: &Out, phases: &mut Phases, heap: &'static blaze_own::Heap, why: Option<&str>) -> Result<Status, CliError> {
    let args = io.need_args()?;
    let flags = crate::flags(&args)?;
    // The request's frame, never the command's fixed 64 MiB one (tempo panicked at 95.7 s,
    // g114729, with that one full): see [`REQUEST_FRAME`].
    let request = Frame::with_reserve(REQUEST_FRAME, bstd::mem::Tag::Other).map_err(|_| CliError::Text { what: "the request's frame could not be reserved (engine_check.rs `REQUEST_FRAME`)" })?;
    let frame: &Frame = &request;
    // `why`'s first word is its target, never a checkout: it stands where blaze was started.
    let checkout = crate::checkout::checkout(if why.is_some() { None } else { flags.dir }, frame)?;
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
    let producers = crate::producers::table(host.build_id()?.as_bytes());
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
    let opts = Opts { tools, workers, toolchain: blaze_toolchain::PINNED, heap, place, fresh: false, frame };
    let err = io.need_err()?;
    match why {
        None => Ok(check(cx, io, &root, opts, &mut shown, phases, out, &err)?.status),
        Some(target) => crate::engine_why::explain(cx, io, &root, &opts, target, phases, out, &err),
    }
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
    /// Answer in full: the stored answer is not read (one is still written).
    pub(crate) fresh: bool,
    /// The request's frame (its caller's, at most [`REQUEST_FRAME`]): every stage's lists and
    /// records whose size follows the project (crates, files, bodies, findings, environments,
    /// labels) are in it, never in the command's fixed frame.
    pub(crate) frame: &'f Frame,
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
}

/// The check of the checkout at `root`, printed through `shown` to `out`; the store's words on
/// `err`.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
pub(crate) fn check<'f>(cx: &'f Cx, io: &Io, root: &Dir<ReadWrite>, opts: Opts<'f>, shown: &mut print::Shown<'f>, phases: &mut Phases, out: &Out, err: &Out) -> Result<Checked, CliError> {
    let Opts { tools, workers, toolchain, heap, place, fresh, frame } = opts;
    let began = phases.now();
    let erased = blaze_own::Erased::new(frame);
    // The one store, opened as `check` opens it (main.rs), committed after the answer.
    let opened = crate::store::open_for_command(cx, io, place.home, root, place.checkout, phases.clock(), err)?;
    let mut store = crate::store::CommandStore::new(cx, &opened.lent, false, opened.open_ns, None, phases.clock().clone(), err, erased.handle())?;
    phases.mark_in("store open", frame);
    // 0. The stored answer (engine_answer.rs): its pointer, then the sweep of what it read.
    // Nothing moved, nothing owed and no findings kept: the certificate, from the pointer alone.
    let answer_root = crate::engine_answer::root_name(place.checkout, &tools, frame);
    let mut unreadable: Option<crate::engine_answer::Unreadable> = None;
    let stored = match fresh {
        true => None,
        false => crate::engine_answer::load_pointer(cx, &mut store, &answer_root)?,
    };
    let pointer = match stored {
        None => None,
        Some(Ok(pointer)) => Some(pointer),
        Some(Err(why)) => {
            unreadable = Some(why);
            None
        }
    };
    phases.mark_in("stored answer's pointer read", frame);
    // The last request's printed lines (engine_lines.rs): what the certificate prints, and each
    // standing row's block for a request that enters tasks. Unreadable: answered in full.
    let mut last_lines = None;
    if let Some(pointer) = &pointer {
        match crate::engine_answer::load_lines(cx, &mut store, pointer.lines, heap.shared())? {
            Ok(lines) => last_lines = Some(lines),
            Err(why) => unreadable = Some(why),
        }
    }
    let pointer = pointer.filter(|_| last_lines.is_some());
    if let (Some(pointer), Some(lines)) = (&pointer, &last_lines) {
        let followed = sweep::Follow { stamps: FromMeta::now(phases.clock()), plant: sweep::Plant::None }.follow(cx, root, Some(opened.home.dir()), &pointer.leaves, frame)?;
        let moved = followed.layout_moved || followed.now.rows.iter().zip(followed.was.iter()).any(|(now, was)| now.state() != *was);
        phases.mark_in("sweep", frame);
        let facts = pointer.facts;
        if !moved && facts.printed.replayable && facts.owed == 0 {
            // The whole answer's lines through the one printer, as the request that wrote them.
            let tally = crate::engine_lines::print_all(cx, root, lines.blocks(), shown, heap, frame, out)?;
            let whole = facts.printed.bodies_whole;
            let (ran_stages, not_stages) = crate::check_coverage(facts.printed.names, whole, whole, frame);
            let coverage = (!not_stages.is_empty()).then_some(blaze_report::frame::Coverage { ran: &ran_stages, not: &not_stages });
            not_checked_notes(cx, &not_stages, shown, frame, out)?;
            print::closing(cx, shown, facts.files, phases.since(began), Some(0), None, coverage, frame, out)?;
            if let Some(words) = crate::not_checked::items::summary(tally.unchecked, shown.behind, frame) {
                print::note(cx, words.as_str(), shown, frame, out)?;
            }
            phases.mark_in("answer printed (the certificate)", frame);
            let status = status_of(&tally, 0, shown, coverage.is_some());
            crate::store::Store::after_answer(&mut store, cx, &mut blaze_drive::profile::Profile::default())?;
            drop(store);
            crate::store::after_answer_printed(cx, opened.lent, false, phases.clock(), err)?;
            phases.mark_in("store committed", frame);
            let text = bstd::mem::format_in!(frame, "certificate stood: nothing the stored answer read moved; no task entered, no page read (the lines record alone), nothing written\n{phases}");
            let file = root.create(cx, &Path::new(print::PROFILE)?)?;
            file.append(cx, text.as_bytes(), bstd::fs::Durable::No)?;
            file.close(cx, bstd::fs::Durable::No)?;
            return Ok(Checked {
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
            });
        }
    }
    // Else the answer it names, every page checked: the engine starts from it.
    let mut start = Answer::new_in(heap.shared());
    let mut causes: SMap<'_, Name, (blaze_engine::Gen, blaze_engine::Cause)> = SMap::new_in(heap.shared());
    // Each row's record in the store, by its key (pages.rs `records`).
    let mut records: SMap<'_, Name, blaze_drive::Key> = SMap::new_in(heap.shared());
    let mut last_root = None;
    if let Some(pointer) = &pointer {
        match crate::engine_answer::load_answer(cx, &mut store, pointer.root, heap.shared())? {
            Ok((kept, root_read)) => {
                start = kept.answer;
                causes = kept.causes;
                records = kept.records;
                last_root = Some(root_read);
            }
            Err(why) => unreadable = Some(why),
        }
        phases.mark_in("stored answer read", frame);
    }
    let read_from_last = last_root.is_some();
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
    let (outlined, probes) = plan(cx, root, &stamps, &laid, workers, alloc)?;
    // Each file's row in the leaf table, so the next request's sweep proves it unchanged by its
    // stamp alone (engine_answer.rs).
    for (slot, probed) in probes.iter() {
        let slot = core::str::from_utf8(slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
        sweep::opened(slot, *probed, &mut rows);
    }
    phases.mark_in("plan (files found and outlined)", frame);
    let mut names = discovered.names.take().map(|world| NamesStage::new(world, frame));
    let mut planned: SVec<'_, Planned> = SVec::new_in(alloc);
    for one in discovered.published.iter() {
        planned.push(Planned { name: one.name, phase: Phase::new(Stage::Units, one.wave), source: one.source, file: one.name });
    }
    for one in outlined.iter() {
        planned.push(Planned { name: one.name, phase: Phase::new(Stage::Outlines, 0), source: one.source, file: one.name });
    }
    // The build scripts and module lookups (engine_scripts.rs), then the crates' names.
    let mut scripts = ScriptsStage::new(frame);
    if let Some(stage) = &names {
        planned.extend(scripts.plan(stage, &discovered, tools, &env, alloc, frame)?);
        planned.extend(stage.plan(alloc, frame)?);
    }
    phases.mark_in("plan (the scripts, and the crates' names at their depths)", frame);
    // The bodies (engine_bodies.rs): one task a body part of each outline.
    planned.extend(crate::engine_bodies::plan(&outlined, alloc));
    let mut bodies = crate::engine_bodies::BodiesStage::new();
    let graph = discovered.get(Name::unit_graph()).and_then(|one| one.made.as_ref().ok().map(|record| blaze_drive::Input { name: one.name, at: tools.key(one.name, &one.source), out: record.out }));
    // Each file once in the edit, with every task it plans (its outline and its bodies): the
    // engine takes a file's entry as all its tasks, so a second entry would drop the first's.
    planned.sort_unstable_by_key(|one| (one.file, one.name));
    let mut files: SVec<'_, (Name, &[Planned])> = SVec::new_in(alloc);
    let mut today: SSet<'_, Name> = SSet::new_in(alloc);
    for group in planned.chunk_by(|a, b| a.file == b.file) {
        files.push((group[0].file, group));
        today.insert(group[0].file);
    }
    // A file the last answer planned and today's plan does not (deleted, or no module names it
    // now): in the edit with no tasks, so its rows go and their cells move to absent.
    for file in start.by_file.keys() {
        if !today.contains(file) {
            files.push((*file, &[]));
        }
    }
    // Each row of the stored answer, as a task entered this request would be marked: a task that
    // enters finds a dependency or script that stands by its key and out, as one made now (an
    // entry this request marks over it).
    for (name, row) in start.rows.iter() {
        let made = match (row.failed, row.phase.stage) {
            (true, _) | (false, Stage::Units | Stage::Outlines | Stage::Heads | Stage::Bodies) => None,
            (false, Stage::Names | Stage::Scripts) => {
                // A row with no record key: the stored answer was written without one (rule 3).
                let key = *records.get(name).ok_or(CliError::Text { what: "a stored row of the names or scripts stage has no record key (pages.rs `records`; engine_check.rs `check`)" })?;
                Some((key, row.out))
            }
        };
        match row.phase.stage {
            Stage::Scripts => {
                scripts.mark(*name, made);
                if let Some(stage) = names.as_mut() {
                    stage.mark(*name, made);
                }
            }
            Stage::Names => {
                if let Some(stage) = names.as_mut() {
                    stage.mark(*name, made);
                }
            }
            Stage::Units | Stage::Outlines | Stage::Heads | Stage::Bodies => {}
        }
    }

    // 2. Drive: each phase run, merged, its records put.
    let mut engine = Engine::new_in(tools, start, &Edit { files: &files }, Everything, blaze_engine::Plant::None, alloc)?;
    let mut pool = Workers::new(WorkersOpts { cx, clock: &env.clock, workers });
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
    // Each phase's wall, its tasks' times, its merge and its put (engine_times.rs).
    let clock = &env.clock;
    let mut times = crate::engine_times::Times::new();
    let mut phase_times = std::vec::Vec::new_in(std::alloc::Global);
    while let Some(phase) = engine.next_phase()? {
        let mut entered: SVec<'_, Task> = SVec::new_in(alloc);
        entered.extend_from_slice(engine.tasks());
        let began = clock.now();
        match phase.stage {
            Stage::Outlines => {
                let view = engine.view();
                pool.run(&entered, &mut buffers, &|cx: &Cx, task: &Task, buffer: &mut Buffer<'_>| runner.work(cx, view, task, buffer, alloc));
                for (task, ns) in entered.iter().zip(pool.times()) {
                    times.push(task.planned.name, *ns);
                }
                // A pool's failure first: the merge's refusal after it is its consequence.
                pool.failed().map_err(CliError::Pool)?;
            }
            Stage::Units => {
                for task in entered.iter() {
                    let one = clock.now();
                    buffers[0].done.push(publish(&discovered, task, alloc));
                    times.push(task.planned.name, clock.now().since(one).map_or(0, crate::engine_times::ns));
                }
            }
            Stage::Names => match names.as_mut() {
                Some(stage) => stage.run_phase(cx, &mut store, &handles, &entered, &mut buffers[0], &mut times, alloc, frame)?,
                None => buffers[0].done.extend(entered.iter().map(|task| crate::engine_tasks::failed(task, alloc))),
            },
            Stage::Bodies => {
                let handles = crate::engine_bodies::Handles { root, env: &env, graph, names: names.as_ref(), outlined: &outlined, workers, clock: phases.clock() };
                bodies.run_phase(cx, &mut store, &handles, &entered, engine.view(), &mut buffers[0], &mut times, alloc, frame)?;
            }
            Stage::Scripts => match names.as_mut() {
                Some(stage) => scripts.run_phase(cx, &mut store, &handles, stage, &entered, &mut buffers[0], &mut times, alloc, frame)?,
                None => buffers[0].done.extend(entered.iter().map(|task| crate::engine_tasks::failed(task, alloc))),
            },
            // Not planned on this path yet: a task entered here is a failure row, named.
            Stage::Heads => buffers[0].done.extend(entered.iter().map(|task| crate::engine_tasks::failed(task, alloc))),
        }
        let wall = clock.now().since(began).map_or(0, crate::engine_times::ns);
        let merging = clock.now();
        engine.merge(&mut buffers)?;
        let merge = clock.now().since(merging).map_or(0, crate::engine_times::ns);
        let planned_here = u32::try_from(planned.iter().filter(|one| one.phase == phase).count()).unwrap_or(u32::MAX);
        ran.push((phase, planned_here, u32::try_from(entered.len()).unwrap_or(u32::MAX)));
        // The phase's records to the store, sorted by key, before the next phase reads them.
        let putting = clock.now();
        // One a task of the phase: the project's size, so in the request's heap.
        let mut put: Vec<Put<'_>, blaze_own::Alloc<'static>> = Vec::new_in(heap.alloc());
        let mut marks: Vec<(Name, Option<(blaze_drive::Key, blaze_ids::ContentId)>), blaze_own::Alloc<'static>> = Vec::new_in(heap.alloc());
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
                Stage::Scripts => scripts.result(name).and_then(|one| one.made.as_ref().ok()).map(|record| (record.out, &record.reads[..], record.payload_bytes())),
                Stage::Heads | Stage::Bodies => None,
            };
            let Some((out, reads, payload)) = found else {
                records.remove(&name);
                continue;
            };
            // Under its bytes' key (engine_store.rs `record_key`); the units and the outlines
            // under their task keys, which the scripts stage and the outline jobs read them by
            // (engine_scripts.rs `plan`, `outline_one`) and whose bytes their keys' text makes.
            let key = match phase.stage {
                Stage::Units | Stage::Outlines => task.key,
                _ => crate::engine_store::record_key(task.key, out, reads, payload),
            };
            records.insert(name, key);
            marks.push((name, Some((key, out))));
            put.push(Put { key, name, source: task.planned.source, out, reads, payload });
        }
        puts += crate::engine_store::put_all(cx, &mut store, &mut put, heap.alloc())?;
        drop(put);
        // What later phases read each entry by (a failed one its driver marked failed).
        for (name, made) in marks.drain(..) {
            match phase.stage {
                Stage::Scripts => {
                    scripts.mark(name, made);
                    if let Some(stage) = names.as_mut() {
                        stage.mark(name, made);
                    }
                }
                Stage::Names => {
                    if let Some(stage) = names.as_mut() {
                        stage.mark(name, made);
                    }
                }
                Stage::Units | Stage::Outlines | Stage::Heads | Stage::Bodies => {}
            }
        }
        phase_times.push(times.fold(phase, wall, merge, clock.now().since(putting).map_or(0, crate::engine_times::ns)));
    }
    let finished = engine.finish()?;
    phases.mark_in("engine", frame);

    // 3. Print: the whole answer's lines (engine_lines.rs), each entered task's block made from
    // its result now, each standing row's copied from the last request's record.
    let mut entered_now: SSet<'_, Name> = SSet::new_in(alloc);
    for one in finished.causes.iter() {
        entered_now.insert(one.name);
    }
    let mut lines = crate::engine_lines::Lines::new_in(alloc);
    // A row with findings that stands and whose block the last record does not hold (a bug of
    // the write): counted and said, never a quiet pass (rule 3).
    let mut unprinted = 0u32;
    for (name, _) in finished.answer.prints(&Everything, erased.handle()).iter() {
        if !entered_now.contains(name) {
            match last_lines.as_ref().and_then(|last| last.block(*name)) {
                Some(block) => lines.keep(*name, block),
                None => unprinted += 1,
            }
            continue;
        }
        if name.kind == blaze_drive::Kind::BodyCheck {
            if let Some(words) = bodies.note(*name, &outlined, frame) {
                let class = if bodies.not_checked(*name) { crate::engine_lines::Class::Unchecked } else { crate::engine_lines::Class::Failure };
                lines.note(*name, class, words.as_bytes());
            }
            continue;
        }
        let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(name)) else {
            let one = match discovered.get(*name) {
                Some(one) => Some(one),
                None => names.as_ref().and_then(|stage| stage.results.iter().find(|one| one.name == *name)).or_else(|| scripts.result(*name)),
            };
            let Some(one) = one else {
                unprinted += 1;
                continue;
            };
            match &one.made {
                Ok(record) => {
                    for &(rule, start, end, from, to) in record.findings.iter() {
                        lines.finding(*name, rule, start, end, None, &record.messages[from..to]);
                    }
                }
                Err(failed) if matches!(failed.failure.why, Why::Waited { .. }) => lines.note(*name, crate::engine_lines::Class::Waited, b""),
                Err(failed) => {
                    let words = match failed.words {
                        Some(words) => bstd::mem::format_in!(frame, "{}: {words}", failed.failure),
                        None => bstd::mem::format_in!(frame, "{}", failed.failure),
                    };
                    lines.note(*name, crate::engine_lines::Class::Failure, words.as_bytes());
                }
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
    let tally = crate::engine_lines::print_all(cx, root, lines.blocks(), shown, heap, frame, out)?;
    let unchecked = tally.unchecked;
    if bodies.ownerless > 0 {
        print::note(cx, bstd::mem::format_in!(frame, "{} body(ies) in files no member crate's records hold were not checked (engine_bodies.rs)", bodies.ownerless).as_str(), shown, frame, out)?;
    }
    if unprinted > 0 {
        print::note(
            cx,
            bstd::mem::format_in!(frame, "{unprinted} task(s) with findings stand from the stored answer, and the last request's lines record holds no block of theirs (engine_check.rs `check`, engine_lines.rs): they are not printed here").as_str(),
            shown,
            frame,
            out,
        )?;
    }
    if let Some(why) = &unreadable {
        print::note(cx, bstd::mem::format_in!(frame, "{why}").as_str(), shown, frame, out)?;
    }
    // Types and borrows are checked whole when every body was (engine_bodies.rs `whole`).
    let bodies_whole = names.is_some() && bodies.whole();
    let (ran_stages, not_stages) = crate::check_coverage(names.is_some(), bodies_whole, bodies_whole, frame);
    let coverage = (!not_stages.is_empty()).then_some(blaze_report::frame::Coverage { ran: &ran_stages, not: &not_stages });
    not_checked_notes(cx, &not_stages, shown, frame, out)?;
    let checked = u32::try_from(outlined.len()).unwrap_or(u32::MAX);
    print::closing(cx, shown, checked, phases.since(began), Some(finished.report.entered), None, coverage, frame, out)?;
    // The bodies not checked, under one line beside the closing line, as `check` prints it.
    if let Some(words) = crate::not_checked::items::summary(unchecked, shown.behind, frame) {
        print::note(cx, words.as_str(), shown, frame, out)?;
    }
    phases.mark_in("answer printed", frame);
    let status = status_of(&tally, unprinted, shown, coverage.is_some());
    // The answer stored, its pointer written last, all in the store's one commit after it.
    let report = finished.report;
    let kept_answer = blaze_engine::pages::Kept::after(causes, finished.answer, &finished.causes, records);
    // Whether every line printed beyond the closing one and its coverage is in the lines record,
    // or the next certificate cannot stand (it prints that record alone).
    let replayable = unprinted == 0 && bodies.ownerless == 0;
    let printed = crate::engine_answer::Printed { names: names.is_some(), bodies_whole, replayable };
    let (lines_key, lines_bytes) = lines.record();
    let saved = crate::engine_answer::save(cx, &mut store, &answer_root, &kept_answer, printed, (lines_key, &lines_bytes), last_root.as_ref(), &rows, alloc, frame)?;
    phases.mark_in("answer stored", frame);
    // The store's one commit, after the answer, as `check`'s (main.rs).
    crate::store::Store::after_answer(&mut store, cx, &mut blaze_drive::profile::Profile::default())?;
    drop(store);
    crate::store::after_answer_printed(cx, opened.lent, false, phases.clock(), err)?;
    phases.mark_in("store committed", frame);
    let because = entered_because(&finished.causes, alloc);
    let mut text = profile_text(&ran, &report, &because, &outlined, phases, frame);
    bodies.profile(&outlined, workers, &mut text);
    // A task by what it is about: a file's path, a crate's name, a script's crate, a body's item.
    let world = names.as_ref().and_then(|stage| stage.names.as_ref());
    let label = |name: Name| -> Option<std::string::String> {
        let crate_named = |at: usize| world.map(|world| std::string::String::from(world.crate_label(at).0));
        match name.kind {
            Kind::BodyCheck => bodies.label(name).map(std::string::String::from),
            Kind::CrateScope => world.and_then(|world| world.crate_at(name.subject)).and_then(crate_named),
            _ => match outlined.binary_search_by(|one| one.name.cmp(&name)) {
                Ok(at) => core::str::from_utf8(&outlined[at].path).ok().map(std::string::String::from),
                Err(_) => world.and_then(|world| world.script_readers(name).next()).and_then(crate_named).map(|shown| shown + " (its build script)"),
            },
        }
    };
    crate::engine_times::write(&phase_times, &label, &mut text);
    {
        use core::fmt::Write as _;
        let from = if read_from_last { "started from the stored answer" } else { "started from no answer" };
        // A full frame cuts the profile short where it stood (as `profile_text`).
        let _ = writeln!(text, "stored answer: {from}; written: {} record(s) put, {} page(s), {} bytes", saved.puts, saved.pages, saved.bytes);
    }
    // Written in place, never synced, as `check`'s profile (print.rs `profile`).
    let file = root.create(cx, &Path::new(print::PROFILE)?)?;
    file.append(cx, text.as_bytes(), bstd::fs::Durable::No)?;
    file.close(cx, bstd::fs::Durable::No)?;
    let mut kept = blaze_own::Vec::new_in(heap.alloc());
    kept.extend(because.iter().copied());
    Ok(Checked {
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
    })
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

/// A note for each stage of `not` (check_coverage's): a stage this build does not run says the
/// build's reason, as `check` does (main.rs `close`); one it runs but did not run whole points at
/// the notes. The certificate and a request that enters tasks print through this one.
fn not_checked_notes(cx: &Cx, not: &[&'static str], shown: &print::Shown<'_>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    for (stage, built, why_not_built) in work::CHECK_STAGES {
        if not.contains(&stage) {
            let words = match built {
                false => bstd::mem::format_in!(frame, "NOT checked: {stage}: {why_not_built}"),
                true => bstd::mem::format_in!(frame, "NOT checked: {stage}: `engine-check` did not run it whole (the notes above say why)"),
            };
            print::note(cx, words.as_str(), shown, frame, out)?;
        }
    }
    Ok(())
}

/// The command's status from what was printed, as main.rs `close`: an error or a failure is 1;
/// items not checked 4; a stage not run, or a row's findings not printed, 3. The certificate and
/// a request that enters tasks take it from the same lines, so they exit alike.
fn status_of(tally: &crate::engine_lines::Tally, unprinted: u32, shown: &print::Shown<'_>, partial: bool) -> Status {
    if shown.counts.errors > 0 || shown.counts.failures > 0 || tally.failures > 0 {
        Status::FAILED
    } else if tally.unchecked > 0 {
        crate::UNCHECKED_ITEMS
    } else if partial || unprinted > 0 {
        crate::INCOMPLETE
    } else {
        Status::OK
    }
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
fn plan<'h>(cx: &Cx, root: &Dir<ReadWrite>, stamps: &FromMeta, laid: &crate::units::Laid<'_>, workers: NonZeroU32, alloc: SharedAlloc<'h>) -> Result<(SVec<'h, Outlined<'h>>, SVec<'h, (SVec<'h, u8>, sweep::Probed)>), CliError> {
    // Each file's slot and what its probe found: its row in the leaf table the next request's
    // sweep follows (engine_answer.rs).
    let mut probes: SVec<'h, (SVec<'h, u8>, sweep::Probed)> = SVec::new_in(alloc);
    let mut seen: SSet<'h, Name> = SSet::new_in(alloc);
    let mut wave: SVec<'h, Want<'h>> = SVec::new_in(alloc);
    for (_, made) in laid.members.iter() {
        wants(&made.parts, &made.paths, &made.path_bytes, &mut seen, &mut wave, alloc)?;
    }
    let mut outlined: SVec<'h, Outlined<'h>> = SVec::new_in(alloc);
    while !wave.is_empty() {
        let scope = cx.scope();
        scope.narrow_workers(workers, Class::Foreground);
        // One a file of the wave: project-sized, so never in the command's fixed frame.
        let mut handles = Vec::with_capacity_in(wave.len(), std::alloc::Global);
        for want in wave.iter() {
            handles.push(scope.may_overlap("outline", TASK_FRAME, move |cx: &Cx| Ok::<_, Infallible>(outline_one(cx, root, stamps, want, alloc))));
        }
        let mut next: SVec<'h, Want<'h>> = SVec::new_in(alloc);
        let mut stopped = None;
        for (handle, want) in handles.into_iter().zip(wave.iter()) {
            match handle.join(cx).map(|one| one.map_err(|stopped| stopped.of(want))) {
                Ok(Ok((one, probed))) => {
                    if let Ok(record) = &one.made {
                        wants(&record.parts, &record.paths, &record.path_bytes, &mut seen, &mut next, alloc)?;
                    }
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
    }
    outlined.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok((outlined, probes))
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
fn outline_one<'h>(cx: &Cx, root: &Dir<ReadWrite>, stamps: &FromMeta, want: &Want<'_>, alloc: SharedAlloc<'h>) -> Result<(Outlined<'h>, sweep::Probed), Stopped> {
    let scratch = cx.frame();
    let slot = core::str::from_utf8(&want.slot).map_err(|_| Stopped::Unread(Unread::Text("a path is not UTF-8")))?;
    let probed = sweep::probe_slot(cx, root, stamps, slot, want.module, scratch).map_err(Stopped::Unread)?;
    let (flags, _, source) = probed;
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
    let source = crate::engine_tasks::edition_source(source, want.edition);
    Ok((Outlined { name: want.name, path, source, edition: want.edition, made }, probed))
}
