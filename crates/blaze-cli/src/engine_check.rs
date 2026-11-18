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
    /// A file whose row stands could not have its outline read back from the store.
    OutlineBack(crate::engine_store::OutlineBack),
    /// A body's findings cannot be placed for printing (engine_bodies.rs `lines`): never dropped.
    Unplaced { body: Name, what: &'static str },
    /// Stopped by a signal at a scheduling point, in `stage` (none: before the first), `at_ms`
    /// after the check began: its status is in the profile and on stderr (engine_status.rs).
    Stopped { by: bstd::procs::By, stage: Option<Stage>, at_ms: u64 },
    /// The command's stop and status signals were not caught (blaze-core run_signals.rs `Asked`).
    Signals(bstd::procs::SignalError),
    /// The status watcher did not run (engine_status.rs `watched`).
    Watcher(crate::engine_status::StatusError),
    /// The request's or a phase's arena could not be reserved (engine_phase_mem.rs).
    PhaseMem(crate::engine_phase_mem::PhaseMemError),
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
            EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name, key }) => {
                write!(f, "{name:?}: its row stands, and the store holds no outline at {key:?}, the key the stored answer names (engine_check.rs `plan`; run `blaze check` with the answer cleared)")
            }
            EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Unreadable { name, key, why }) => {
                write!(f, "{name:?}: the outline at {key:?} is no whole drive record of it ({why:?}) (engine_store.rs `outline_from`)")
            }
            EngineCheckError::Unplaced { body, what } => write!(f, "{body:?}: its findings cannot be placed for printing: {what} (engine_bodies.rs `lines`)"),
            EngineCheckError::Stopped { by, stage, at_ms } => match stage {
                Some(stage) => write!(f, "stopped by {} at {at_ms} ms in stage {stage:?}; where it stood is in {} (engine_status.rs)", by.name(), print::PROFILE),
                None => write!(f, "stopped by {} at {at_ms} ms before the first stage; where it stood is in {} (engine_status.rs)", by.name(), print::PROFILE),
            },
            EngineCheckError::Signals(error) => write!(f, "its stop and status signals: {error} (engine_check.rs `serve`)"),
            EngineCheckError::Watcher(error) => write!(f, "{error}"),
            EngineCheckError::PhaseMem(error) => write!(f, "{error}"),
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
    match serve(cx, io, out, phases, heap, None) {
        // Its words are on stderr and in the profile already: the status a shell gives a
        // process the signal ended (128 and its number).
        Err(CliError::EngineCheck(EngineCheckError::Stopped { by, .. })) => Ok(Status(128 + by.number())),
        other => other,
    }
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
    let request = Frame::with_reserve(REQUEST_FRAME, bstd::mem::Tag::Plane).map_err(|_| CliError::Text { what: "the request's frame could not be reserved (engine_check.rs `REQUEST_FRAME`)" })?;
    let frame: &Frame = &request;
    // `why`'s first word is its target, never a checkout: it stands where blaze was started.
    let checkout = crate::checkout::checkout(if why.is_some() { None } else { flags.dir }, frame)?;
    let fs = io.need_fs()?;
    // The named origin, opened as `check-old` opens it (main.rs): a miss is read from it.
    // Declared before `shown`: the request's options and what it prints share one lifetime
    // (`checking` takes `&mut Shown<'f>`), so the origin must outlive the printer's state.
    let origin = crate::store::NamedOrigin::of(cx, &fs, io.net().as_ref(), flags.origin, flags.origin_key)?;
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
    // The check's stop and status signals, caught until it answers (engine_status.rs);
    // `why` answers from the stored answer at once, and keeps the default ones.
    #[cfg(unix)]
    let asked = match why {
        None => Some(bstd::procs::Asked::hold().map_err(EngineCheckError::Signals)?),
        Some(_) => None,
    };
    #[cfg(unix)]
    let requests = asked.as_ref().map(bstd::procs::Asked::requests);
    #[cfg(not(unix))]
    let requests = None;
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
    };
    let err = io.need_err()?;
    let Some(target) = why else {
        let checked = match flags.fix {
            None => Some(check(cx, io, &root, opts(), &mut shown, phases, out, &err)?),
            // `blaze check --fix <selector>..` (fix.rs): a quiet check, its findings' fixes
            // written as one transaction, then a second check printing what is left; the
            // listed findings are in the request's frame, which outlives both.
            Some((start, end)) => {
                let mut selectors: blaze_own::Vec<&str, &Frame> = blaze_own::Vec::with_capacity_in(end - start, frame);
                selectors.extend((start..end).filter_map(|at| args.get(at)));
                let mut ask = |mut shown| check(cx, io, &root, opts(), &mut shown, phases, out, &err).map(|checked| (checked, shown));
                match crate::fix_then_check::<crate::fix::Atomic, Checked>(cx, &root, &selectors, (caps, view, shown_root.as_str()), frame, out, &mut ask)? {
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
    crate::engine_why::explain(cx, io, &root, &opts(), target, phases, out, &err)
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
    pub(crate) undefined_reads: (std::vec::Vec<(Name, CellId), std::alloc::Global>, u32, u32),
    /// The Heads stage's rows in the answer (`Signature` and `ImplBucket`, entered or standing),
    /// the head tasks refused, and the crates whose tables standing rows put on the shelf from
    /// the store (engine_heads.rs `complete`).
    pub(crate) heads: (u32, u32, u32),
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
}

/// The check of the checkout at `root`, printed through `shown` to `out`; the store's words on
/// `err`.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
pub(crate) fn check<'f>(cx: &'f Cx, io: &Io, root: &Dir<ReadWrite>, opts: Opts<'f>, shown: &mut print::Shown<'f>, phases: &mut Phases, out: &Out, err: &Out) -> Result<Checked, CliError> {
    // Where the check stands, said when asked or stopped (engine_status.rs), the watcher
    // beside it when anything can ask.
    let asking = opts.status;
    let board = crate::engine_status::Board::new(&asking, phases.now().since_start());
    let clock = phases.clock().clone();
    crate::engine_status::watched(io, root, err, &clock, &board, asking.every, || checking(cx, io, root, opts, shown, phases, out, err, &board)).map_err(EngineCheckError::Watcher)?
}

/// [`check`] under its board.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one handle of it")]
fn checking<'f>(cx: &'f Cx, io: &Io, root: &Dir<ReadWrite>, opts: Opts<'f>, shown: &mut print::Shown<'f>, phases: &mut Phases, out: &Out, err: &Out, board: &crate::engine_status::Board<'_>) -> Result<Checked, CliError> {
    let Opts { tools, workers, toolchain, heap, place, fresh, heads_from, head_units, head_jobs, head_sources, frame, scope_reads, unreadable: planted_unreadable, body_locals, status: _, origin } = opts;
    let began = phases.now();
    let erased = blaze_own::Erased::new(frame);
    // The request's arena (engine_phase_mem.rs): what this call keeps to its end and no record
    // holds, given back whole when it returns; each phase's arena is made from it. Declared
    // first, so dropped last: every value over it is a local dropped before it.
    let request_mem = crate::engine_phase_mem::RequestMem::new()?;
    let request = request_mem.shared();
    // The phases' arenas and the heap kept past each, by stage.
    let mut phase_mem = crate::engine_phase_mem::PhaseLedger::new();
    // The lint step and rules the root manifest names, as `check` reads them (lints.rs
    // `of_root`): the level each lint finding prints at, and what the Lints stage runs.
    let linting = crate::lints::of_root(cx, root, frame)?;
    shown.lints = linting.step;
    // The one store, opened as `check` opens it (main.rs), committed after the answer.
    // PHASE-MEM-DEBT(Request): the store's open takes the process heap (store.rs
    // `open_for_command`, `heap: &'static Heap`): its lease tables live to the command's end.
    let opened = crate::store::open_for_command(cx, io, place.home, root, place.checkout, phases.clock(), err, heap)?;
    crate::store::take::before_first_phase(cx, io, &opened, toolchain, phases, err)?;
    let mut store = crate::store::CommandStore::new(cx, &opened.lent, false, opened.open_ns, origin, phases.clock().clone(), err, erased.handle())?;
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
        match crate::engine_answer::load_lines(cx, &mut store, pointer.lines, request)? {
            Ok(lines) => last_lines = Some(lines),
            Err(why) => unreadable = Some(why),
        }
    }
    let pointer = pointer.filter(|_| last_lines.is_some());
    // The journal of changes replayed from the pointer's places, BEFORE any file of the
    // checkout is read (journal_sweep.rs): a write after this and before its file's read is
    // after the place this request saves, so the next request's replay names it. Moved below
    // the sweep or the plan's reads, such a write would be missed for good.
    let stored_places = pointer.as_ref().map_or(crate::journal_sweep::Stored::NONE, |pointer| pointer.journal);
    let journal = crate::journal_sweep::take(cx, root, opened.home.dir(), phases.clock(), stored_places, pointer.as_ref().map(|pointer| &pointer.leaves), frame)?;
    let mut sweep_counts = journal.counts(0, 0);
    phases.mark_in("journal replayed", frame);
    // Each module slot the last answer read, as the sweep found it today: the planner takes a
    // slot's state from here and reads no file whose stamp proves it ([`Kept`]).
    let mut followed_kept = None;
    let mut today_slots: std::vec::Vec<(&str, sweep::Probed), &Frame> = std::vec::Vec::new_in(frame);
    if let (Some(pointer), Some(lines)) = (&pointer, &last_lines) {
        // Only the rows the journal's changes reach are looked at; all where it says nothing.
        let marked = journal.marked(&pointer.leaves, frame);
        let follow = sweep::Follow { stamps: FromMeta::now(phases.clock()), plant: sweep::Plant::None };
        let (followed, statted) = follow.follow_marked(cx, root, Some(opened.home.dir()), &pointer.leaves, marked.as_deref(), frame)?;
        sweep_counts = journal.counts(statted, u32::try_from(pointer.leaves.len()).unwrap_or(u32::MAX));
        let followed: &sweep::Followed<'_> = followed_kept.insert(followed);
        today_slots.extend(followed.now.rows.iter().filter(|leaf| leaf.kind == sweep::LeafKind::Slot).map(|leaf| (followed.now.path(leaf), (leaf.flags, leaf.stamp, leaf.id))));
        today_slots.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let moved = followed.layout_moved || followed.now.rows.iter().zip(followed.was.iter()).any(|(now, was)| now.state() != *was);
        phases.mark_in("sweep", frame);
        let facts = pointer.facts;
        if !moved && facts.printed.replayable && facts.owed == 0 {
            // The whole answer's lines through the one printer, as the request that wrote them;
            // its lists in the request's arena.
            let tally = crate::engine_lines::print_all(cx, root, lines.blocks(), shown, request, frame, out)?;
            crate::engine_head_notes::print_grouped(cx, lines.blocks(), shown, frame, out)?;
            let whole = facts.printed.bodies_whole;
            let (ran_stages, not_stages) = crate::check_coverage(facts.printed.names, whole, whole && tally.borrows == 0, frame);
            let coverage = (!not_stages.is_empty()).then_some(blaze_report::frame::Coverage { ran: &ran_stages, not: &not_stages });
            print::stage_notes(cx, &not_stages, shown, frame, out)?;
            print::closing(cx, shown, facts.files, phases.since(began), Some(0), None, coverage, frame, out)?;
            if let Some(words) = crate::not_checked::items::summary(tally.unchecked, shown.behind, frame) {
                print::note(cx, words.as_str(), shown, frame, out)?;
            }
            phases.mark_in("answer printed (the certificate)", frame);
            let status = status_of(&tally, 0, shown, coverage.is_some());
            // Read through, so renewed (store/roots.rs, the census).
            let pages = bstd::mem::format_in!(frame, "{}.pages", answer_root.as_str());
            store.renew(&[answer_root.as_str(), pages.as_str(), crate::engine_seeds::root_name(&tools, frame).as_str()], toolchain)?;
            crate::store::Store::after_answer(&mut store, cx, &mut blaze_drive::profile::Profile::default())?;
            drop(store);
            let cleanup = crate::store::after_answer_printed(cx, opened.lent, false, phases.clock(), err)?;
            phases.mark_in("store committed", frame);
            let tail = tally.tail.line(tally.listed, frame);
            let text = bstd::mem::format_in!(frame, "certificate stood: nothing the stored answer read moved; no task entered, no page read (the lines record alone), nothing written but the roots renewed\n{sweep_counts}\n{cleanup}\n{tail}\n{phases}");
            let file = root.create(cx, &Path::new(print::PROFILE)?)?;
            file.append(cx, text.as_bytes(), bstd::fs::Durable::No)?;
            file.close(cx, bstd::fs::Durable::No)?;
            // PHASE-MEM-DEBT(CHECKED): `Checked` outlives the request (main.rs prints it, the
            // tests read it) and has no lifetime, so its lists are on the process heap.
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
                undefined_reads: (std::vec::Vec::new_in(std::alloc::Global), 0, 0),
                heads: (0, 0, 0),
                signatures_entered: 0,
                lints: crate::engine_lints::LintsSeen::default(),
                ownerless: (0, std::vec::Vec::new_in(std::alloc::Global)),
                files: Files::default(),
                units: crate::engine_units::Units::default(),
                pages_written: 0,
                records: crate::engine_store::Ledger::new(),
                sweep: sweep_counts,
                seeds: crate::engine_seeds::SeedCounts::default(),
                phase_mem: crate::engine_phase_mem::PhaseLedger::new(),
            });
        }
    }
    // Else the answer it names, every page checked: the engine starts from it.
    // The request's: the engine's answer grows from it and is saved as the stored answer.
    let mut start = Answer::new_in(request);
    let mut causes: SMap<'_, Name, (blaze_engine::Gen, blaze_engine::Cause)> = SMap::new_in(request);
    // Each row's record in the store, by its key (pages.rs `records`).
    let mut records: SMap<'_, Name, blaze_drive::Key> = SMap::new_in(request);
    let mut last_root = None;
    if let Some(pointer) = &pointer {
        match crate::engine_answer::load_answer(cx, &mut store, pointer.root, request)? {
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
        // PHASE-MEM-DEBT(Units): the fetches' and every stage's `handles.env.heap` (fetch.rs
        // `FetchEnv::heap`, `&'static Heap`): the stages put what they make on the process heap
        // through it; each becomes the phase's handle when its stage takes one (`kept past the
        // phase` names which).
        heap,
    };

    // 1. Plan: the units stage, then the files, then the crates.
    let stamps = FromMeta::now(phases.clock());
    let mut rows = Rows::new_in(frame);
    // PHASE-MEM-DEBT(Units): the layout's manifests on the process heap (units.rs `lay`, `heap:
    // &'static Heap`): the request's, read by every stage's plan.
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
        // PHASE-MEM-DEBT(Units): the unit graph on the process heap (graph.rs `Graph::new`,
        // `heap: &'static Heap`): the request's; the graph is a record (`discover` puts it).
        laid.graph = Some(crate::graph::Graph::new(parts, lockfile, units, target, host, true, heap, frame));
    }
    // The graph and its fetches read back when nothing it is a function of moved (plan row D2),
    // else discovered; the phase's line says which, and why.
    let back = crate::engine_units::Back { rows: &start.rows, records: &records };
    let mut discovered = crate::engine_units::discover(cx, root, &mut store, &mut laid, units, tools, &back, &env, frame)?;
    phases.mark_in(discovered.cause.phase(), frame);
    // The request's handle: the plan, the engine, the buffers the workers fill (merged into the
    // answer), the lines printed. Nothing over it outlives this call.
    let alloc = request;
    let kept = Kept { slots: &today_slots, rows: &start.rows, records: &records };
    let (outlined, probes, file_counts) = plan(cx, root, &stamps, &laid, workers, &kept, &mut store, alloc)?;
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
        // The build scripts' reads of the checkout, one `Given` cell a leaf row (engine_script_reads.rs).
        let no_reads = Rows::new_in(frame);
        planned.extend(scripts.plan_reads(followed_kept.as_ref().map_or(&no_reads, |followed: &sweep::Followed<'_>| &followed.now), alloc));
        planned.extend(stage.plan(alloc, frame)?);
        // Each crate's impl tables, with its names task's file (engine_heads.rs).
        planned.extend(crate::engine_heads::plan(stage, alloc)?);
    }
    phases.mark_in("plan (the scripts, and the crates' names at their depths)", frame);
    // The bodies (engine_bodies.rs): one task a body part of each outline.
    planned.extend(crate::engine_bodies::plan(&outlined, alloc));
    // The heads (engine_heads.rs `plan_items`): one task an outline entry with a head.
    if names.is_some() {
        planned.extend(crate::engine_heads::plan_items(&outlined, alloc));
    }
    // The token lints (engine_lints.rs): one task a lint unit of each member's outline.
    planned.extend(crate::engine_lints::plan(&outlined, &linting.rules, alloc));
    let mut lints = crate::engine_lints::LintsStage::new(linting.rules);
    let mut bodies = crate::engine_bodies::BodiesStage::new(scope_reads);
    let mut heads = crate::engine_heads::HeadsStage::new(scope_reads);
    heads.freeze = head_units;
    heads.jobs = head_jobs;
    // PHASE-MEM-DEBT(Heads): the request's frozen heads on the process heap
    // (engine_heads_once.rs `HeadsOnce::new`, `heap: &'static Heap`): they cross from the Heads
    // phases to the Bodies phases, so the request's; to take `SharedAlloc<'r>` of the request.
    heads.once = Some(crate::engine_heads_once::HeadsOnce::new(heap, crate::engine_heads_once::OncePlant::Rule, head_sources));
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
            (true, _) | (false, Stage::Units | Stage::Outlines | Stage::Heads | Stage::Bodies | Stage::Lints) => None,
            // A build script's read of the checkout: a cell of today's answer, no record.
            (false, Stage::Scripts) if name.kind == Kind::Given => None,
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
            Stage::Units | Stage::Outlines | Stage::Heads | Stage::Bodies | Stage::Lints => {}
        }
    }

    // 2. Drive: each phase run, merged, its records put.
    let mut engine = Engine::new_in(tools, start, &Edit { files: &files }, Everything, blaze_engine::Plant::None, alloc)?;
    let mut pool = Workers::new(WorkersOpts { cx, clock: &env.clock, workers, board, alloc });
    let mut buffers: SVec<'_, Buffer<'_>> = SVec::new_in(alloc);
    for _ in 0..pool.runs() {
        buffers.push(Buffer::new_in(alloc));
    }
    let runner = Tasks { outlined: &outlined };
    let members = crate::names::member_dirs(Some(&laid));
    let side = laid.lock.as_ref().map(|lockfile| crate::proc_runner::MacroSide {
        lock: lockfile,
        place: crate::macros::Place { home: env.home, checkout: env.checkout, members, rows: &rows },
        // PHASE-MEM-DEBT(Names): the proc macros' expansions on the process heap
        // (proc_runner.rs `MacroSide::heap`, `&'static Heap`): made in a Names phase, read by
        // the Scripts and Names phases after; the record is what should cross.
        heap,
    });
    let handles = Handles { root, lock: laid.lock.as_ref().map(|lockfile| lockfile.lock), members, env: &env, side: side.as_ref(), outlined: &outlined, workers, board };
    // PHASE-MEM-DEBT(CHECKED): returned in `Checked`, which has no lifetime.
    let mut ran = blaze_own::Vec::new_in(heap.alloc());
    let mut puts = 0u32;
    // What each stage's records cost this process (engine_store.rs `Ledger`).
    let mut ledger = crate::engine_store::Ledger::new();
    // The Heads and Bodies stages' reader of the crates' records, in place in the store's
    // mapping for the store's borrow (engine_records.rs): dropped with the store, never in a stage.
    let mut read = crate::engine_records::EngineRecords::new();
    read.plant_unreadable(planted_unreadable);
    // Each phase's wall, its tasks' times, its merge and its put (engine_times.rs).
    let clock = &env.clock;
    let mut times = crate::engine_times::Times::new();
    let mut phase_times = std::vec::Vec::new_in(std::alloc::Global);
    // The request's frozen heads, counted at the Bodies phase's merge (engine_seeds.rs).
    let mut seed_counts = crate::engine_seeds::SeedCounts::default();
    // A test's planted heap leak (rule 23's control, engine_phase_mem.rs `with_planted_leak`):
    // kept to the command's end, as a stage that broke the rule would keep it.
    #[cfg(test)]
    let mut planted_kept = blaze_own::Vec::new_in(heap.alloc());
    while let Some(phase) = engine.next_phase()? {
        // The driver's scheduling point (engine_status.rs): a stop asked during the last
        // merge or put.
        if let Some(by) = board.stopping() {
            return Err(stopped(cx, root, err, board, by, &ledger, names.as_ref(), phases, clock, frame)?);
        }
        // The phase's arena (engine_phase_mem.rs), made before its tasks run and ended once
        // its records are committed, below. A local of this iteration: a stop's return, an
        // error's `?` or an unwind drops it too, its pages back to the kernel.
        let mem = request_mem.phase(phase.stage)?;
        let at_phase = mem.shared();
        #[cfg(test)]
        if let Some(leak) = crate::engine_phase_mem::planted_leak(phase.stage, heap) {
            planted_kept.push(leak);
        }
        // The phase's tasks: read to its end, never after.
        let mut entered: SVec<'_, Task> = SVec::new_in(at_phase);
        entered.extend_from_slice(engine.tasks());
        let mark = board.phase_began(phase.stage, entered.len() as u64, board.now(clock));
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
            // PHASE-MEM-DEBT(Names): the stage takes the request's handle (its `Done`s merge into
            // the answer) and no phase handle, and puts its results (`NamesStage::results`: read
            // lists, findings, expansions) on the request frame and the process heap
            // (`handles.env.heap`), kept to the command's end for the put and the print. To take
            // `at_phase: SharedAlloc<'p>` for all but its `Done`s, and read its findings back
            // from the lines record.
            Stage::Names => match names.as_mut() {
                Some(stage) => stage.run_phase(cx, &mut store, &mut read, &handles, &entered, &mut buffers[0], &mut times, alloc, frame)?,
                None => buffers[0].done.extend(entered.iter().map(|task| crate::engine_tasks::failed(task, alloc))),
            },
            Stage::Bodies => {
                let handles = crate::engine_bodies::Handles { root, env: &env, graph, names: names.as_ref(), outlined: &outlined, workers, clock: phases.clock(), board };
                // The tables of crates whose Heads task stands, from the store: one shelf of all.
                if let Some(stage) = names.as_ref() {
                    heads.read_back += crate::engine_heads::complete(cx, &mut store, stage, &mut bodies.shelf, &|name| records.get(&name).copied(), engine.view(), frame)?;
                }
                // The heads not frozen, a body that reads one quotes (engine_head_notes.rs).
                // PHASE-MEM-DEBT(Bodies): `Failed::of` takes `Alloc<'static>` and keeps its rows
                // on the process heap (engine_head_notes.rs), to the command's end for the
                // print; to become the lines record's head notes, read back.
                bodies.heads_failed = crate::engine_head_notes::Failed::of(&heads, last_lines.as_ref(), heap.alloc())?;
                // The request's frozen heads, read only while the phase runs (engine_seeds.rs):
                // the head tasks' cells and the dependency heads this machine froze before.
                let seed_root = crate::engine_seeds::root_name(&tools, frame);
                let seed_tool = crate::engine_seeds::tool_of(&tools);
                let mut request_heads = match (heads_from, names.as_ref(), graph) {
                    (crate::engine_seeds::HeadsFrom::Frozen, Some(stage), Some(graph)) => match bodies.kept.list(cx, &mut store, &graph, frame)? {
                        Some(list) => {
                            let crates = crate::engine_seeds::crates_of(&list, stage);
                            // PHASE-MEM-DEBT(Bodies): the table lives to this phase's end alone
                            // and is built on the process heap (engine_seeds.rs
                            // `RequestTable::build`, `heap: &'static Heap`): to take `&mem`'s
                            // `SharedAlloc<'p>`, when its growth is freed at the phase's end.
                            Some(crate::engine_seeds::RequestTable::build(cx, &mut store, &planned, engine.view(), &|name| records.get(&name).copied(), crates, &seed_root, seed_tool, heap)?)
                        }
                        // No crate list: no body is checked, each says why (engine_bodies.rs `groups`).
                        None => None,
                    },
                    _ => None,
                };
                // The rule, or a test's plant (rule 23): where a body's own items are made.
                if let Some(table) = request_heads.as_mut() {
                    table.body_locals = body_locals;
                }
                // The dependency heads the Heads stage put by key are listed by the root too.
                if let (Some(table), Some(once)) = (request_heads.as_mut(), heads.once.as_ref()) {
                    table.list_too(once.put_keys());
                }
                // PHASE-MEM-DEBT(Bodies): the stage's packs, groups and notes on the process
                // heap (`handles.env.heap`, engine_bodies.rs `run_phase`), and each run's frame
                // shared by its bodies, not reset per body; its notes kept to the command's end
                // for the print. To take `at_phase` for what lives to the phase's end.
                bodies.run_phase(cx, &mut store, &mut read, &handles, &entered, engine.view(), &mut buffers[0], &mut times, alloc, frame, request_heads.as_ref(), heads.listings(heap))?;
                // The dependency heads frozen now, put sorted by key and listed by the root.
                if let Some(table) = &request_heads {
                    seed_counts = table.merge(cx, &mut store, &seed_root, &mut ledger, bodies.seeds)?;
                } else {
                    seed_counts.bodies = bodies.seeds;
                }
            }
            // PHASE-MEM-DEBT(Scripts): as Names': `ScriptsStage::results` on the request frame
            // and the process heap, kept to the command's end; to take `at_phase`.
            Stage::Scripts => match names.as_mut() {
                Some(stage) => scripts.run_phase(cx, &mut store, &handles, stage, &entered, &mut buffers[0], &mut times, alloc, frame)?,
                None => buffers[0].done.extend(entered.iter().map(|task| crate::engine_tasks::failed(task, alloc))),
            },
            Stage::Heads => {
                let handles = crate::engine_bodies::Handles { root, env: &env, graph, names: names.as_ref(), outlined: &outlined, workers, clock: phases.clock(), board };
                let crate::engine_bodies::BodiesStage { kept, shelf, .. } = &mut bodies;
                // PHASE-MEM-DEBT(Heads): `HeadsStage::made` and `refused` on the process heap,
                // kept to the command's end for the put and the notes, and the shelf's tables
                // carried to the Bodies phases (`bodies.shelf`): to take `at_phase` for the
                // first, and the store's records read in place for the second.
                // A closed dependency's head is found by its key in the store's committed
                // snapshot (lane H9; engine_heads_once.rs, "A CLOSED DEPENDENCY'S HEAD").
                if let (Some(once), Some(stage), Some(graph)) = (heads.once.as_mut(), names.as_ref(), graph) {
                    if let Some(list) = kept.list(cx, &mut store, &graph, frame)? {
                        let records = store.snapshot_in(heap.shared())?;
                        let crates = crate::engine_seeds::crates_in(&list, stage, heap.shared());
                        once.find_dependencies(crate::engine_seeds::DependencyFinder::new(records, crates, crate::engine_seeds::tool_of(&tools)));
                    }
                }
                heads.run_phase(cx, &mut store, &mut read, &handles, kept, shelf, &entered, engine.view(), &mut buffers[0], &mut times, alloc, frame)?;
            }
            // PHASE-MEM-DEBT(Lints): `LintsStage::made` on the process heap, kept to the
            // command's end for the put and the print; to take `at_phase`.
            Stage::Lints => lints.run_phase(cx, root, &outlined, workers, phases.clock(), &entered, &mut buffers[0], &mut times, alloc)?,
        }
        board.phase_ended(mark, entered.len() as u64, board.now(clock));
        // Stopped while the phase ran: its tasks not begun have no result, so no merge. The
        // phase's arena is dropped by the return (engine_phase_mem.rs `PhaseMem`'s drop).
        if let Some(by) = board.stopping() {
            return Err(stopped(cx, root, err, board, by, &ledger, names.as_ref(), phases, clock, frame)?);
        }
        // A status asked during the phase, written here when no watcher took it first (a
        // plant's always: a planted run starts none).
        if let Some(by) = board.take_status() {
            let mut text = blaze_own::String::new_in(frame);
            let _ = crate::engine_status::write_status(board, crate::engine_status::Heading::Asked { by }, board.now(clock), &mut text);
            crate::engine_status::put(cx, root, err, text.as_bytes())?;
        }
        let wall = clock.now().since(began).map_or(0, crate::engine_times::ns);
        let merging = clock.now();
        engine.merge(&mut buffers)?;
        let merge = clock.now().since(merging).map_or(0, crate::engine_times::ns);
        let planned_here = u32::try_from(planned.iter().filter(|one| one.phase == phase).count()).unwrap_or(u32::MAX);
        ran.push((phase, planned_here, u32::try_from(entered.len()).unwrap_or(u32::MAX)));
        // The phase's records to the store, sorted by key, before the next phase reads them.
        let putting = clock.now();
        // One a task of the phase: the project's size, so in the phase's arena, gone with it.
        let mut put: Vec<Put<'_>, blaze_own::Alloc<'_>> = Vec::new_in(mem.alloc());
        let mut marks: Vec<(Name, Option<(blaze_drive::Key, blaze_ids::ContentId)>), blaze_own::Alloc<'_>> = Vec::new_in(mem.alloc());
        for task in entered.iter() {
            let name = task.planned.name;
            // A file's outline is put whole (its parts, paths and findings): the next request
            // that finds the file unchanged reads it back and never the file (`plan`).
            let listing = match phase.stage {
                Stage::Outlines => outlined.binary_search_by(|one| one.name.cmp(&name)).ok().and_then(|at| outlined[at].made.as_ref().ok()).map(crate::engine_store::Listing::of),
                _ => None,
            };
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
                Stage::Heads => heads.result(name).map(|one| (one.out, &one.reads[..], Ok(&one.payload[..]))),
                Stage::Lints => lints.result(name).map(|one| (one.record.out, &one.record.reads[..], one.record.payload_bytes())),
                Stage::Bodies => None,
            };
            let Some((out, reads, payload)) = found else {
                records.remove(&name);
                continue;
            };
            // A payload released by an earlier phase's put is refused, naming the record.
            let payload = payload?;
            // Under its bytes' key (engine_store.rs `record_key`); the units and the outlines
            // under their task keys, which the scripts stage and the outline jobs read them by
            // (engine_scripts.rs `plan`, `outline_one`) and whose bytes their keys' text makes.
            let key = match phase.stage {
                Stage::Units | Stage::Outlines => task.key,
                _ => crate::engine_store::record_key(task.key, out, reads, payload),
            };
            records.insert(name, key);
            marks.push((name, Some((key, out))));
            // A crate's expansion records, put after its record (engine_store.rs `put_all`).
            let extras = match phase.stage {
                Stage::Names => names
                    .as_ref()
                    .and_then(|stage| stage.results.iter().rev().find(|one| one.name == name))
                    .and_then(|one| one.made.as_ref().ok())
                    .map(|record| &record.extras),
                _ => None,
            };
            put.push(Put { key, name, source: task.planned.source, out, reads, payload, listing, extras });
        }
        // Each record's head, made in the phase's arena and written before it ends.
        let mut put_now = crate::engine_store::put_all(cx, &mut store, &mut put, phase.stage, &mut ledger, mem.alloc())?;
        drop(put);
        // The closed dependency heads frozen this phase, put under their keys (lane H9).
        if phase.stage == Stage::Heads
            && let Some(once) = heads.once.as_mut()
        {
            put_now += once.put_dependencies(cx, &mut store, &mut ledger)?;
        }
        puts += put_now;
        // Written now, so the store's arena frees them: the next phase reads them in place from
        // the segment's mapping (rule 51; store.rs `commit_puts`).
        if put_now > 0 {
            store.commit_puts()?;
            if phase.stage == Stage::Heads
                && let Some(once) = heads.once.as_mut()
            {
                once.committed(store.snapshot_in(heap.shared())?);
            }
        }
        // Each put record's handed payload freed now it is in the store (rule 51): a result
        // keeps its findings and lines, never its bytes; a later phase reads the store.
        for task in entered.iter() {
            let name = task.planned.name;
            let result = match phase.stage {
                Stage::Names => names.as_mut().and_then(|stage| stage.results.iter_mut().rev().find(|one| one.name == name)),
                Stage::Scripts => scripts.results.iter_mut().find(|one| one.name == name),
                Stage::Units | Stage::Outlines | Stage::Heads | Stage::Bodies | Stage::Lints => None,
            };
            let freed = result.and_then(|one| one.made.as_mut().ok()).map_or(0, |record| record.release_handed());
            ledger.released(phase.stage, freed);
        }
        ledger.merged(phase.stage, crate::store::Store::kept(&store));
        board.merged(phase.stage, ledger.memory.stages[phase.stage.index()]);
        ledger.read(phase.stage, read.counts());
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
                Stage::Units | Stage::Outlines | Stage::Heads | Stage::Bodies | Stage::Lints => {}
            }
        }
        phase_times.push(times.fold(phase, wall, merge, clock.now().since(putting).map_or(0, crate::engine_times::ns)));
        // The phase's records are committed: its arena ends here. A value over `at_phase` still
        // alive at `end` is a borrow across its move, refused at compile time
        // (engine_phase_mem.rs's doc).
        drop(marks);
        drop(entered);
        let row = mem.end();
        board.phase_mem(row.stage, phase_mem.add(row));
    }
    // Every phase's arena made and dropped by now (each ended in its iteration).
    phase_mem.counts = request_mem.counts();
    // Every expansion of this command is done: its proc-macro answers to their store, once, so
    // the next command reads them by key (proc_runner.rs `keep`, lane M14).
    if let Some(procs) = names.as_ref().and_then(|stage| stage.names.as_ref()).and_then(|world| world.procs()) {
        procs.keep().map_err(CliError::ProcAnswers)?;
    }
    let finishing = clock.now();
    let mut finished = engine.finish()?;
    finished.report.settle.finish_ns = clock.now().since(finishing).map_or(0, crate::engine_times::ns);
    // Rule 21 over the whole answer: a cell read is DEFINED by a task, or ABSENT by intent: its
    // record's task has a row and states the cell's absence (a bucket its crate files no impl
    // in). A read of a record no task has a row for is one no edit can ever move.
    let mut undefined_reads = (std::vec::Vec::new_in(std::alloc::Global), 0u32, 0u32);
    for (name, row) in finished.answer.rows.iter() {
        for (id, _) in row.reads.iter() {
            if finished.answer.cells.get(id).is_some() || finished.answer.rows.get(&id.of).is_some() {
                continue;
            }
            if id.of.kind == blaze_drive::Kind::Signature {
                undefined_reads.2 = undefined_reads.2.saturating_add(1);
                continue;
            }
            undefined_reads.1 = undefined_reads.1.saturating_add(1);
            if undefined_reads.0.len() < 8 {
                undefined_reads.0.push((*name, *id));
            }
        }
    }
    let heads_entered = u32::try_from(finished.answer.rows.iter().filter(|(_, row)| row.phase.stage == Stage::Heads).count()).unwrap_or(u32::MAX);
    phases.mark_in("engine", frame);
    // The heap at each step of the tail (the profile's "tail heap" line): where it grows past
    // the Bodies phase is named by the step, never guessed (rule 17).
    let heap_engine = crate::engine_phase_mem::heap_now();

    // 3. Print: the whole answer's lines (engine_lines.rs), each entered task's block made from
    // its result now, each standing row's copied from the last request's record.
    let mut entered_now: SSet<'_, Name> = SSet::new_in(alloc);
    for one in finished.causes.iter() {
        entered_now.insert(one.name);
    }
    let signatures_entered = u32::try_from(finished.causes.iter().filter(|one| one.name.kind == blaze_drive::Kind::Signature).count()).unwrap_or(u32::MAX);
    let mut lines = crate::engine_lines::Lines::new_in(alloc);
    // A row with findings that stands and whose block the last record does not hold (a bug of
    // the write): counted and said, never a quiet pass (rule 3).
    let mut unprinted = 0u32;
    // The first such task (by name), so the note names one (rule 19).
    let mut first_unprinted: Option<Name> = None;
    // Whether types and borrows were checked whole is a fact of THE ANSWER, never of this
    // request's work: a standing body row (not entered now) whose kept block says its body was
    // not checked whole (refused, failed, not checked), or that has no block to say, makes the
    // stage not whole as a body entered now would (`bodies.whole()`). Read here, over the rows
    // with findings alone (a clean body has none), so O(rows with findings).
    let mut standing_bodies_whole = true;
    for (name, _) in finished.answer.prints(&Everything, erased.handle()).iter() {
        if !entered_now.contains(name) {
            match last_lines.as_ref().and_then(|last| last.block(*name)) {
                Some(block) => {
                    // A block that does not decode is not known to be of a whole check: not
                    // whole (a pass is never implied, rule 3); `print_all` says its error.
                    if name.kind == blaze_drive::Kind::BodyCheck && crate::engine_lines::not_checked_whole(block).unwrap_or(true) {
                        standing_bodies_whole = false;
                    }
                    lines.keep(*name, block);
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
            bodies.lines(*name, &outlined, &mut lines, frame)?;
            continue;
        }
        if name.kind == blaze_drive::Kind::Lint {
            lints.lines(*name, &outlined, &mut lines, frame)?;
            continue;
        }
        // A head task's heads not frozen, each with its reason (engine_head_notes.rs).
        if crate::engine_head_notes::is_head_task(*name) {
            if !bodies.heads_failed.ready {
                // PHASE-MEM-DEBT(Heads): as the Bodies phase's (`Failed::of`, `Alloc<'static>`).
                bodies.heads_failed = crate::engine_head_notes::Failed::of(&heads, last_lines.as_ref(), heap.alloc())?;
            }
            bodies.heads_failed.note(&heads, *name, &mut lines, frame);
            continue;
        }
        let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(name)) else {
            let one = match discovered.get(*name) {
                Some(one) => Some(one),
                None => names.as_ref().and_then(|stage| stage.results.iter().find(|one| one.name == *name)).or_else(|| scripts.result(*name)),
            };
            let Some(one) = one else {
                unprinted += 1;
                first_unprinted.get_or_insert(*name);
                continue;
            };
            match &one.made {
                Ok(record) => {
                    for &(rule, start, end, from, to) in record.findings.iter() {
                        lines.finding(*name, rule, start, end, None, &record.messages[from..to]);
                    }
                }
                Err(failed) => match failed.failure.why {
                    // Its words its cause's name and its crate, as the old path files a wait
                    // (work.rs `filed_answer`): the printer groups the waiters under the cause.
                    Why::Waited { on } => {
                        let label = match name.kind {
                            Kind::CrateScope => names.as_ref().and_then(|stage| stage.names.as_ref()).and_then(|world| world.crate_at(name.subject).map(|at| world.crate_label(at))),
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
    bodies.plan_lines(&outlined, last_lines.as_ref(), &mut lines);
    // A build script whose outputs this command uses and does not keep says so, once (lane NR2).
    for (name, why) in scripts.unkept() {
        lines.note(name, crate::engine_lines::Class::Told, bstd::mem::format_in!(frame, "{}", crate::engine_scripts::UnkeptWords(name, why)).as_bytes());
    }
    phases.mark_in("lines made", frame);
    let heap_lines_made = crate::engine_phase_mem::heap_now();
    // The printer's lists in the request's arena, as the certificate's print.
    let tally = crate::engine_lines::print_all(cx, root, lines.blocks(), shown, alloc, frame, out)?;
    crate::engine_head_notes::print_grouped(cx, lines.blocks(), shown, frame, out)?;
    let unchecked = tally.unchecked;
    if bodies.ownerless > 0 {
        print::note(cx, bstd::mem::format_in!(frame, "{} body(ies) in files no member crate's records hold were not checked (engine_bodies.rs)", bodies.ownerless).as_str(), shown, frame, out)?;
    }
    if unprinted > 0 {
        // The first by its kind and its file's path (the row's file: an outline's name).
        let first = first_unprinted.map(|name| {
            let file = finished.answer.rows.get(&name).map(|row| row.file);
            let path = file.and_then(|file| outlined.binary_search_by(|one| one.name.cmp(&file)).ok()).map_or("?", |at| core::str::from_utf8(&outlined[at].path).unwrap_or("?"));
            (name.kind, path)
        });
        let (kind, path) = first.unwrap_or((Kind::BodyCheck, "?"));
        print::note(
            cx,
            bstd::mem::format_in!(frame, "{unprinted} task(s) with findings, or failed, have no block in the lines record (engine_check.rs `check`, engine_lines.rs: no printer writes their kind, or the last request's record lacks it): they are not printed here; first: a {kind:?} task of `{path}`").as_str(),
            shown,
            frame,
            out,
        )?;
    }
    if let Some(why) = &unreadable {
        print::note(cx, bstd::mem::format_in!(frame, "{why}").as_str(), shown, frame, out)?;
    }
    // Types and borrows are checked whole when every body of the answer was: those entered now
    // (engine_bodies.rs `whole`) and those that stand (`standing_bodies_whole`, above).
    let bodies_whole = names.is_some() && bodies.whole() && standing_bodies_whole;
    let (ran_stages, not_stages) = crate::check_coverage(names.is_some(), bodies_whole, bodies_whole && tally.borrows == 0, frame);
    let coverage = (!not_stages.is_empty()).then_some(blaze_report::frame::Coverage { ran: &ran_stages, not: &not_stages });
    print::stage_notes(cx, &not_stages, shown, frame, out)?;
    let checked = u32::try_from(outlined.len()).unwrap_or(u32::MAX);
    print::closing(cx, shown, checked, phases.since(began), Some(finished.report.entered), None, coverage, frame, out)?;
    // The bodies not checked, under one line beside the closing line, as `check` prints it.
    if let Some(words) = crate::not_checked::items::summary(unchecked, shown.behind, frame) {
        print::note(cx, words.as_str(), shown, frame, out)?;
    }
    phases.mark_in("answer printed", frame);
    let heap_printed = crate::engine_phase_mem::heap_now();
    let status = status_of(&tally, unprinted, shown, coverage.is_some());
    // The answer stored, its pointer written last, all in the store's one commit after it.
    let report = finished.report;
    // The leaf rows of the scripts' reads a row of the answer still reads (engine_script_reads.rs).
    scripts.script_rows(followed_kept.as_ref().map(|followed| &followed.now), |name| finished.answer.rows.contains_key(&name), &mut rows);
    let kept_answer = blaze_engine::pages::Kept::after(causes, finished.answer, &finished.causes, records);
    // Whether every line printed beyond the closing one and its coverage is in the lines record,
    // or the next certificate cannot stand (it prints that record alone).
    let replayable = unprinted == 0 && bodies.ownerless == 0;
    let printed = crate::engine_answer::Printed { names: names.is_some(), bodies_whole, replayable };
    let (lines_key, lines_bytes) = lines.record();
    let saved = crate::engine_answer::save(cx, &mut store, &answer_root, toolchain, &kept_answer, printed, (lines_key, &lines_bytes), last_root.as_ref(), &rows, &journal, alloc, frame)?;
    phases.mark_in("answer stored", frame);
    let heap_stored = crate::engine_phase_mem::heap_now();
    // The store's one commit, after the answer, as `check`'s (main.rs).
    crate::store::Store::after_answer(&mut store, cx, &mut blaze_drive::profile::Profile::default())?;
    // Nothing read in place outlives the store's borrow of its `Lent` (engine_records.rs).
    drop(read);
    drop(store);
    // The whole cleanup line: this profile is written after the commit (store.rs).
    let cleanup = crate::store::after_answer_printed(cx, opened.lent, false, phases.clock(), err)?;
    phases.mark_in("store committed", frame);
    let because = entered_because(&finished.causes, alloc);
    let mut text = profile_text(&ran, &report, &because, &finished.causes, &outlined, phases, frame);
    bodies.profile(&outlined, workers, &mut text);
    lints.profile(&mut text);
    scripts.profile(&mut text);
    crate::engine_memory::profile(&ledger, names.as_ref(), &mut text);
    // The head tasks' freezes by unit (engine_heads.rs `UnitCounts`), said beside the table's.
    seed_counts.units = heads.unit_counts();
    seed_counts.once = crate::engine_heads_once::counts(&heads);
    crate::engine_seeds::profile(&seed_counts, &mut text);
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
    ledger.write(&mut text);
    // Each stage's tasks, times and memory, by the status's printer (engine_status.rs).
    let _ = crate::engine_status::write_status(board, crate::engine_status::Heading::End, board.now(clock), &mut text);
    {
        use core::fmt::Write as _;
        let from = if read_from_last { "started from the stored answer" } else { "started from no answer" };
        // A full frame cuts the profile short where it stood (as `profile_text`).
        let _ = writeln!(text, "stored answer: {from}; written: {} record(s) put, {} page(s), {} bytes", saved.puts, saved.pages, saved.bytes);
        let _ = writeln!(text, "files: {} outline(s) read back from the store (file not read), {} outlined (file read), {} probed by the planner", file_counts.read_back, file_counts.outlined, file_counts.probed);
        let _ = writeln!(text, "{sweep_counts}");
        let _ = writeln!(text, "{cleanup}");
        let made = phase_mem.counts;
        let _ = writeln!(text, "phase arenas: {} made, {} dropped, {} bytes given back at their drops (engine_phase_mem.rs)", made.made, made.dropped, made.returned);
        let _ = writeln!(text, "{}", tally.tail.line(tally.listed, frame));
        let _ = writeln!(text, "tail heap (bytes): engine finished {heap_engine}, lines made {heap_lines_made}, answer printed {heap_printed}, answer stored {heap_stored}");
    }
    // Written in place, never synced, as `check`'s profile (print.rs `profile`).
    let file = root.create(cx, &Path::new(print::PROFILE)?)?;
    file.append(cx, text.as_bytes(), bstd::fs::Durable::No)?;
    file.close(cx, bstd::fs::Durable::No)?;
    // PHASE-MEM-DEBT(CHECKED): `Checked` has no lifetime and outlives the request, so its lists
    // are on the process heap.
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
        undefined_reads,
        heads: (heads_entered, u32::try_from(heads.refused.len()).unwrap_or(u32::MAX), heads.read_back),
        signatures_entered,
        lints: lints.seen(&outlined),
        ownerless: (bodies.ownerless, bodies.ownerless_paths(&outlined)),
        files: file_counts,
        units: discovered.units,
        pages_written: saved.new_pages,
        records: ledger,
        sweep: sweep_counts,
        seeds: seed_counts,
        phase_mem,
    })
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
fn status_of(tally: &crate::engine_lines::Tally, unprinted: u32, shown: &print::Shown<'_>, partial: bool) -> Status {
    if shown.counts.errors > 0 || shown.counts.failures > 0 || tally.failures > 0 {
        Status::FAILED
    } else if tally.unchecked > 0 || tally.heads > 0 {
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
///
/// A file the sweep proved unchanged (its slot in [`Kept::slots`], so no stat and no read here)
/// whose stored `Outlines` row stands by today's source is not read: its outline is read back
/// from the record its row names ([`crate::engine_store::outline_from`]). Every other file is
/// probed (unless the sweep found it) and outlined on the workers.
#[allow(clippy::too_many_arguments)]
fn plan<'h, 'm>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &FromMeta,
    laid: &crate::units::Laid<'_>,
    workers: NonZeroU32,
    kept: &Kept<'_, '_>,
    store: &mut dyn crate::store::Store<'m>,
    alloc: SharedAlloc<'h>,
) -> Result<(SVec<'h, Outlined<'h>>, SVec<'h, (SVec<'h, u8>, sweep::Probed)>, Files), CliError> {
    let mut files = Files::default();
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
        // Each want of the wave in its order: read back on this thread, or a worker's handle.
        let mut handles = Vec::with_capacity_in(wave.len(), alloc);
        let mut stopped = None;
        for want in wave.iter() {
            let known = kept.slot(want);
            match known.and_then(|probed| kept.stood(want, probed).map(|key| (probed, key))) {
                Some((probed, key)) => {
                    // The store's own error stops the plan after the wave's workers are joined.
                    let back = match store.get(cx, key) {
                        Err(error) => Err(error),
                        Ok(None) => Err(EngineCheckError::OutlineBack(crate::engine_store::OutlineBack::Missing { name: want.name, key }).into()),
                        Ok(Some(held)) => crate::engine_store::outline_from(want.name, key, held.bytes, alloc).map_err(|why| EngineCheckError::OutlineBack(why).into()),
                    };
                    match back {
                        Ok(record) => {
                            files.read_back += 1;
                            let scratch = cx.frame();
                            let slot = core::str::from_utf8(&want.slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
                            let mut path = SVec::new_in(alloc);
                            path.extend_from_slice(&sweep::real_path(slot, probed.0, scratch));
                            let source = crate::engine_tasks::edition_source(probed.2, want.edition);
                            handles.push(Err(Ok((Outlined { name: want.name, path, source, edition: want.edition, made: Ok(record) }, probed))));
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
                    handles.push(Ok(scope.may_overlap("outline", TASK_FRAME, move |cx: &Cx| Ok::<_, Infallible>(outline_one(cx, root, stamps, want, known, alloc)))));
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
    Ok((outlined, probes, files))
}

/// What the planner was handed of the last answer: today's state of each slot the sweep
/// followed (sorted by path), the stored rows, and each row's record key.
pub(crate) struct Kept<'k, 'a> {
    slots: &'k [(&'k str, sweep::Probed)],
    rows: &'k SMap<'a, Name, blaze_engine::Row<'a>>,
    records: &'k SMap<'a, Name, blaze_drive::Key>,
}

impl Kept<'_, '_> {
    /// The want's slot as the sweep found it today, when the last answer read it as this kind
    /// of slot (a `mod x;` slot or a named file).
    fn slot(&self, want: &Want<'_>) -> Option<sweep::Probed> {
        let slot = core::str::from_utf8(&want.slot).ok()?;
        let at = self.slots.binary_search_by(|(path, _)| (*path).cmp(slot)).ok()?;
        let probed = self.slots[at].1;
        ((probed.0 & sweep::MODULE != 0) == want.module).then_some(probed)
    }

    /// The record key of the want's stored outline, when its row stands by today's source: an
    /// `Outlines` row, not failed, of a file found (neither absent nor at both places).
    fn stood(&self, want: &Want<'_>, probed: sweep::Probed) -> Option<blaze_drive::Key> {
        if probed.0 & (sweep::ABSENT | sweep::BOTH) != 0 {
            return None;
        }
        let row = self.rows.get(&want.name)?;
        let source = crate::engine_tasks::edition_source(probed.2, want.edition);
        (row.phase.stage == Stage::Outlines && !row.failed && row.source == source).then(|| self.records.get(&want.name).copied()).flatten()
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
/// `known`: the slot as the sweep found it today, so it is not probed again.
fn outline_one<'h>(cx: &Cx, root: &Dir<ReadWrite>, stamps: &FromMeta, want: &Want<'_>, known: Option<sweep::Probed>, alloc: SharedAlloc<'h>) -> Result<(Outlined<'h>, sweep::Probed), Stopped> {
    let scratch = cx.frame();
    let slot = core::str::from_utf8(&want.slot).map_err(|_| Stopped::Unread(Unread::Text("a path is not UTF-8")))?;
    let probed = match known {
        Some(probed) => probed,
        None => sweep::probe_slot(cx, root, stamps, slot, want.module, scratch).map_err(Stopped::Unread)?,
    };
    let (flags, _, source) = probed;
    if flags & sweep::ABSENT != 0 {
        return Err(Stopped::Absent);
    }
    let mut path = SVec::new_in(alloc);
    path.extend_from_slice(&sweep::real_path(slot, flags, scratch));
    // Every outline lists its lint units (lints.rs `parts`) whatever the step, so what it holds
    // never depends on the rule set, which is in each `Lint` task's source (engine_lints.rs).
    let job = Job { name: want.name, slot, flags, edition: want.edition, plant: work::Plant::None, expect: source, lints: true };
    let made = match work::unwinding(|| work::outline_file(cx, root, &job, scratch, alloc)) {
        Ok(Ok(made)) => made,
        Ok(Err(Unread::Changed)) => Err(Why::Changed),
        Ok(Err(error)) => return Err(Stopped::Unread(error)),
        Err(work::Panicked) => Err(Why::Panicked { used: scratch.peak() as u64, limit: scratch.reserved() as u64 }),
    };
    let source = crate::engine_tasks::edition_source(source, want.edition);
    Ok((Outlined { name: want.name, path, source, edition: want.edition, made }, probed))
}
