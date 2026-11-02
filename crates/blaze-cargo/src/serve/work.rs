//! What a job of the daemon asks of the build (docs/v2/plan/lanes/cargo/notes.md section 6): the steps
//! between a plan and its outputs, behind traits so the loop runs the same in the daemon
//! ([`crate::serve::unit::Real`]) and in the simulation ([`crate::serve::tests`]). The loop is
//! generic over its [`Steps`], whose associated types are the front end's: `blaze serve` is
//! one instance of it, on `Real`, with no trait object and no downcast.
//!
//! Where each is called is part of its contract:
//!
//! - **On an IO thread of the plane**: [`Steps::fetch`], [`Steps::library`]. They may block on
//!   the network or the disk; they touch no state of the loop.
//! - **On a worker of the plane**: [`Steps::lower`], [`Front::compile`], [`Front::reach`],
//!   [`Front::ub_tests`], [`Front::evaluate`], [`Front::codegen_inputs`], [`Front::tables`],
//!   [`Front::pack`].
//!   They read the front end through the [`Steps::Reader`] the loop made as it posted the task, the
//!   VFS's reader and the store through [`JobInput`], and give back what the loop writes
//!   ([`Compiled::puts`]); a [`Failed::Again`] goes back to the loop, which posts it again
//!   with a new reader. No worker locks to read.
//! - **On the loop**: [`Front::fresh`], [`Front::script`], [`Front::tests`], [`Front::run`],
//!   every [`Writer`] and every [`Runs`] method. User code runs as
//!   [`blaze_core::Effect::Spawn`]s of the loop's driver, in the sandbox its
//!   [`blaze_core::Confine`] makes, never on a thread of its own.
//!
//! # Who reads the front end, and where the stop is
//!
//! A job's front end is one value owned by the loop: its database is shared by reference and
//! never cloned (docs/v2/plan/process/decisions.md). The tasks that read it are those of a
//! batch added with `reads` ([`crate::serve::plane::Plane::add`]): every compile, the
//! `--ub` selection and evaluations, a reach (of both its views) and the codegen inputs. Each
//! is posted with a [`Steps::Reader`], a shared reference it holds for its run and drops as it ends,
//! with no lock, no clone of the database and no cancellation. The tables are measured with
//! the database exclusive, so no reader adds to them: their task, the front end's last use, is
//! handed the job's [`Writer`] itself. So is a pack's, whose recording of expansions sets the
//! database's proc macros, once its check's own plane request is over.
//!
//! A build script's output ([`Writer::ran`]) and a proc macro's load ([`Writer::macros`]) are
//! writes. Each is checked as it is asked for and applied by [`Writer::apply`], on the loop,
//! once no reader is alive: every reader is one of a task posted before the write was asked
//! for, and from then until it is applied the loop posts no reading task of that job, but
//! holds it and posts it after, with a reader of what the write made. That wait is the stop.
//! Tasks of the job that don't read (build scripts and the program's runs, store puts and
//! placements, fetches, the plan) and every other job's tasks keep going meanwhile. Each
//! write asked while readers run waits with the others, and `apply` applies them all as one,
//! so a wave of build scripts is one stop.
//!
//! Images kept linked between jobs are the loop's too ([`Image`]): each is moved into the
//! one link ([`Phase::Link`]) that relinks it and back with what it made, so no worker shares them. So is a
//! checkout's last front end ([`Writer::keep`]), once no reader of it is left: it is moved
//! into the one lowering of the checkout's next build, which moves it to the new view.

#![forbid(unsafe_code)]

use std::sync::{Arc, atomic::AtomicBool};

use blaze_api::{ApiError, Artifact, Diagnostic, Edit, Edited, Reach, Response, Table, TestResult, Ub};
use blaze_cas::Store;
use blaze_core::{Completion, Effect, Tag};
use blaze_ids::ContentId;
use blaze_protocol::events::NodeKind;
use blaze_query::{Database, DbError, Query, QueryKey};
use blaze_units::query::{BuildConfig, Fetch, Plan, Planned, Units};
use blaze_vfs::{Change, Mode, ViewId};

/// A source fetched and checked, as its files: the daemon puts it into the VFS as a root of its
/// own and mounts it where its [`Fetch::at`] says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Source {
    /// Every file, by `/`-separated path under the source's root, each once, none with an
    /// empty, `.` or `..` part.
    pub files: Vec<SourceFile>,
    /// What the fetch's `at` is under the root: "" for a directory (a `.crate`, a git tree),
    /// the file's path for one file (a sparse index's).
    pub under: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub mode: Mode,
}

/// What a job's work is, as far as its units' work differs by it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Goal {
    /// Stops after the front end.
    Check,
    Build,
    Test,
    Run,
    /// Builds nothing: [`Front::reach`].
    Reach,
}

/// The tests a job runs, and how, as `blaze test` asked.
#[derive(Clone, Copy, Debug)]
pub struct Selection<'a> {
    /// Text a test's name must contain one of; none selects every test.
    pub filter: &'a [String],
    /// libtest's own selection, what followed `--`: `--exact`, `--ignored`,
    /// `--include-ignored`, `--skip NAME`.
    pub args: &'a [String],
    /// Every test, not only those the change reached.
    pub all: bool,
    /// Each test run in blaze's MIR evaluator, checked for undefined behavior. The job's
    /// units were checked only ([`Goal::Check`]): `built` is empty, nothing is linked.
    pub ub: Option<Ub>,
}

/// A job as its steps see it.
#[derive(Clone)]
pub struct JobInput {
    pub db: Arc<Database>,
    /// The job's view of `db`, opened with `config` ([`Database::open_with`]).
    pub queries: ViewId,
    /// The view's root, readable through `files`.
    pub root: ContentId,
    /// Every root of the VFS (the view's, each mount's), from any thread.
    pub files: blaze_vfs::Reader,
    /// What the view was opened with; its `invocation.env` is the request's whole
    /// environment, held in memory for the job only and never logged or kept.
    pub config: Arc<BuildConfig>,
    pub units: Arc<Units>,
    /// The store, read by tasks with no lock (a read is valid while its
    /// [`blaze_cas::enter`] guard is held); written only by the loop.
    pub store: blaze_cas::Reader,
    /// The standard library's store, shared by this build's daemons, read as `store` is: a
    /// std unit's record is found there when `store` has none, as of the look the job's build
    /// waited for.
    pub std: blaze_cas::Reader,
    pub color: bool,
    /// Diagnostics are rendered a line each: cargo's `--message-format=short` or
    /// `json-diagnostic-short`.
    pub short: bool,
    /// The columns diagnostics are rendered in, as rustc's `--diagnostic-width`; none for
    /// rustc's default ([`blaze_api::Cargo::width`]).
    pub width: Option<u32>,
    /// How many `-v` the request gave ([`blaze_api::Cargo::verbose`]).
    pub verbose: u8,
    /// How many of the functions blaze could not compile an error names, every one with `None`
    /// ([`blaze_api::Cargo::refused`]; [`crate::REFUSED_SHOWN`] for an agent's request).
    pub refused: Option<u32>,
    pub goal: Goal,
    /// Set once the job is cancelled: long work looks at it between pieces and stops with
    /// [`Failed::Again`], which the loop drops for a cancelled job.
    pub cancelled: Arc<AtomicBool>,
}

/// Why a step gave no result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failed {
    /// It gave way (a cycle of waits, memory, a cancel): posted again whole, nothing of it kept.
    Again,
    /// The job can't go on: it finishes with this error.
    Error(ApiError),
}

impl From<ApiError> for Failed {
    fn from(error: ApiError) -> Failed {
        Failed::Error(error)
    }
}

/// The standard library's `library/` as [`Steps::library`] found it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Library {
    /// Where its sources are named: `/rustc/<commit>/library`.
    pub at: String,
    /// Of its digest and every file's path, size, time and mode: its files are read again
    /// only once this changes.
    pub fingerprint: String,
    /// Its files; none when it is mounted as it is already.
    pub source: Option<Source>,
}

/// A value for the store, put by the loop.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Put {
    pub key: blaze_cas::Key,
    pub kind: blaze_cas::Kind,
    pub bytes: Vec<u8>,
}

/// A file a unit writes: where cargo writes it (absolute, one of the unit's
/// `UnitPlan::outputs`) and the store key of its bytes, in the store or among the puts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Output {
    pub path: String,
    pub key: blaze_cas::Key,
    pub executable: bool,
}

/// What came of one unit's compile.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Compiled {
    /// In rustc's order, each with its unit (`Diagnostic::unit`).
    pub diagnostics: Vec<Diagnostic>,
    /// No error among the diagnostics and nothing else failed: what depends on it goes on, as
    /// cargo's.
    pub ok: bool,
    pub puts: Vec<Put>,
    /// What the loop puts into the standard library's shared store ([`JobInput::std`]): a std
    /// unit's record and every output it names, once made here; nothing for any other unit.
    pub share: Vec<Put>,
    /// Placed by the loop once `puts` are in the store.
    pub outputs: Vec<Output>,
    /// cargo's `compiler-artifact` of the unit, reported in `Built` once its outputs are
    /// placed; none for a unit cargo lists none of.
    pub artifact: Option<Artifact>,
    /// A proc macro's expanders, loaded from what it compiled to (or why there are none):
    /// the loop asks the job's [`Writer::macros`] with them. None for any other unit.
    pub macros: Option<blaze_lower::ProcMacroLoadResult>,
    /// Each image the compile linked, by its path, and how the link reached it: what the job's
    /// [`blaze_api::Worked`] says, and the bench fails an edit by.
    pub links: Vec<(String, Linking)>,
}

/// A part of a unit's compile ([`Front::compile`]), each one node of the plane, so each is
/// timed by the work it does ([`blaze_api::Worked::steps_us`]). A unit's phases run in this
/// order, but [`Phase::Bodies`] and [`Phase::Codegen`], which both wait for
/// [`Phase::Names`] and run side by side: the check tells codegen what fails, and codegen
/// tells the check what it reaches first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Whether the unit's record is kept, its crate's def map, and what its check is of.
    Names,
    /// Its check: the diagnostics of every body of its crate, or the check served for it.
    Bodies,
    /// Its functions compiled from its roots.
    Codegen,
    /// Its images linked from what codegen made, a proc macro's expanders loaded.
    Link,
    /// Its outputs hashed and its record encoded for the store: the compile's outcome.
    Store,
}

impl Phase {
    /// A unit's phases when it compiles code.
    pub const BUILD: [Phase; 5] = [Phase::Names, Phase::Bodies, Phase::Codegen, Phase::Link, Phase::Store];
    /// A unit's phases when it is only checked.
    pub const CHECK: [Phase; 3] = [Phase::Names, Phase::Bodies, Phase::Store];

    /// The phase's node kind, as the steps are timed by.
    pub const fn kind(self) -> NodeKind {
        match self {
            Phase::Names => NodeKind::Names,
            Phase::Bodies => NodeKind::Bodies,
            Phase::Codegen => NodeKind::Codegen,
            Phase::Link => NodeKind::Link,
            Phase::Store => NodeKind::Store,
        }
    }

    /// Unit `unit`'s task of this phase, as one `u32` argument of the batch's work.
    pub const fn arg(self, unit: u32) -> u32 {
        assert!(unit < 1 << 29, "under 2^29 units");
        unit << 3 | self as u32
    }

    /// The unit and phase of a task's argument, which only [`Phase::arg`] makes.
    pub const fn of(arg: u32) -> (usize, Phase) {
        let phase = match arg & 7 {
            0 => Phase::Names,
            1 => Phase::Bodies,
            2 => Phase::Codegen,
            3 => Phase::Link,
            4 => Phase::Store,
            _ => panic!("a task's argument names no phase"),
        };
        ((arg >> 3) as usize, phase)
    }
}

/// How a link reached its image: what the link line (`link: <path> (<how>)`) says, and what
/// the bench fails an edit by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Linking {
    /// The image's first link: none was kept at its path, so there was nothing to patch.
    First,
    /// What changed was placed into the room the kept image keeps.
    Spliced,
    /// The image was laid out whole again although one was kept.
    Full(Full),
}

impl Linking {
    /// The image was laid out whole, rather than patched.
    pub(crate) fn placed(self) -> bool {
        !matches!(self, Linking::Spliced)
    }

    /// As [`blaze_api::Worked::links`] says it.
    pub(crate) fn how(self) -> blaze_api::LinkHow {
        match self {
            Linking::First => blaze_api::LinkHow::First,
            Linking::Spliced => blaze_api::LinkHow::Spliced,
            // alloc: a name a field that differs, for a link that was not spliced
            Linking::Full(Full::Output(fields)) => {
                blaze_api::LinkHow::Full { why: fields.names().map(str::to_owned).collect() }
            }
            // alloc: one name, for a link that was not spliced
            Linking::Full(Full::NotIncremental) => blaze_api::LinkHow::Full { why: vec!["not incremental".to_owned()] },
        }
    }
}

impl std::fmt::Display for Linking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Linking::First => f.write_str("first link"),
            Linking::Spliced => f.write_str("spliced"),
            Linking::Full(Full::Output(fields)) => write!(f, "not spliced: the output changed: {fields}"),
            Linking::Full(Full::NotIncremental) => f.write_str("not spliced: the output keeps no room"),
        }
    }
}

/// Why a link with an image kept laid it out whole again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Full {
    /// The kept image was linked as another [`blaze_link::Output`]: these of its fields differ. blaze-link
    /// stages a change of an image's code and data, never of its output, so the image is made
    /// again.
    Output(OutputFields),
    /// The output keeps no room ([`blaze_link::Output::incremental`] is off): every link of it is whole.
    NotIncremental,
}

/// Fields of [`blaze_link::Output`] as a set, a bit each in [`OutputFields::NAMES`]'s order.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct OutputFields(u32);

impl OutputFields {
    pub(crate) const NAMES: [&'static str; 17] = [
        "format",
        "arch",
        "kind",
        "exports",
        "interpreter",
        "platform",
        "incremental",
        "debug",
        "live",
        "tags",
        "stack",
        "windows_subsystem",
        "dev_run",
        "triple",
        "exec_log",
        "sanitize",
        "rpaths",
    ];

    /// The fields of `new` that differ from `old`'s. A shared library's exports are a field of
    /// their own: they change with the functions it defines, its name and kind do not.
    pub(crate) fn between(old: &blaze_link::Output, new: &blaze_link::Output) -> OutputFields {
        let blaze_link::Output {
            format,
            arch,
            kind,
            interpreter,
            platform,
            incremental,
            debug,
            live,
            tags,
            stack,
            windows_subsystem,
            dev_run,
            triple,
            exec_log,
            sanitize,
            rpaths,
        } = new;
        let (kind_differs, exports_differ) = match (&old.kind, kind) {
            (
                blaze_link::OutputKind::Shared { name: was, exports: had },
                blaze_link::OutputKind::Shared { name, exports },
            ) => (was != name, had != exports),
            (was, kind) => (was != kind, false),
        };
        let differs = [
            old.format != *format,
            old.arch != *arch,
            kind_differs,
            exports_differ,
            old.interpreter != *interpreter,
            old.platform != *platform,
            old.incremental != *incremental,
            old.debug != *debug,
            old.live != *live,
            old.tags != *tags,
            old.stack != *stack,
            old.windows_subsystem != *windows_subsystem,
            old.dev_run != *dev_run,
            old.triple != *triple,
            old.exec_log != *exec_log,
            old.sanitize != *sanitize,
            old.rpaths != *rpaths,
        ];
        let mut bits = 0;
        for (at, differs) in differs.into_iter().enumerate() {
            bits |= u32::from(differs) << at;
        }
        OutputFields(bits)
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The names of the fields in the set, in [`OutputFields::NAMES`]'s order.
    pub(crate) fn names(self) -> impl Iterator<Item = &'static str> {
        Self::NAMES.into_iter().enumerate().filter(move |(at, _)| (self.0 >> at) & 1 == 1).map(|it| it.1)
    }
}

impl std::fmt::Display for OutputFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (at, name) in self.names().enumerate() {
            if at > 0 {
                f.write_str(", ")?;
            }
            f.write_str(name)?;
        }
        Ok(())
    }
}

/// The build of one job, as the daemon has it: one value for every job. Its associated types
/// are its front end's, so the loop holds each by its own type.
pub trait Steps: Sized + Send + Sync + 'static {
    /// One job's front end, its units lowered: shared by the job's tasks.
    type Front: Front<Self>;
    /// What of a job's front end only the loop writes.
    type Writer: Writer<Self>;
    /// What a task reads the front end through: a shared reference to it, made by the loop as
    /// it posted the task ([`Writer::reader`]) and dropped as the task ends. While one is
    /// alive the front end is not written; a write waits for it ([`Writer::apply`]).
    type Reader: Send + 'static;
    /// A job's front end kept by the loop once the job is done with it ([`Writer::keep`]), for
    /// the next job of the same head to lower into ([`Steps::lower`]).
    type KeptFront: KeptDb;
    /// What the front keeps of a linked image ([`Image`]).
    type Linked: Kept;
    /// User code of a job run through the loop's driver ([`Runs`]).
    type Runs: Runs<Self>;

    /// On an IO thread: the source `fetch` names, fetched and checked (a `.crate`'s sha256 and
    /// its paths kept inside it, a git tree at its commit, an index file as served).
    fn fetch(&self, fetch: &Fetch) -> Result<Source, ApiError>;
    /// On an IO thread: the standard library's `library/`, put into the VFS once: where it is
    /// named (`/rustc/<commit>/library`), its fingerprint, and its files unless `kept` (a
    /// fingerprint, and where that library is mounted) has it there, read before by this
    /// daemon or one before it.
    fn library(&self, kept: &[(String, String)]) -> Result<Library, ApiError>;
    /// On a worker: the job's units lowered into the front end. `kept` is the front end of
    /// the head's last build ([`Writer::keep`]): built from the same units and config, it is
    /// moved to the job's view, its memos kept, and what changed alone read again; else it
    /// is dropped and the units lowered anew.
    fn lower(&self, job: &JobInput, kept: Option<Self::KeptFront>) -> Result<FrontEnd<Self>, Failed>;
    /// On the loop, once at start: the queries of the daemon's database.
    fn register(&self, db: &mut Database) {
        blaze_units::query::register(db);
    }
    /// On a worker: the plan of `view` (opened with its [`BuildConfig`]).
    fn plan(&self, db: &Database, view: ViewId) -> Result<Planned, Failed> {
        match db.pin().get::<Plan>(view, &()) {
            Ok(planned) => Ok(planned.get()),
            Err(DbError::Cancelled) => Err(Failed::Again),
            Err(error) => Err(Failed::Error(ApiError::new(format!("the load of the workspace: {error}"), None))),
        }
    }
    /// The key [`Steps::plan`] asks: where a view's memos are walked from, and what the
    /// daemon's collection keeps.
    fn plan_key(&self) -> QueryKey {
        QueryKey { kind: <Plan as Query>::KIND, input: <Plan as Query>::input(&()) }
    }
}

/// One job's front end, lowered on a worker and handed to the loop in two: what every task
/// shares, and what only the loop writes.
pub struct FrontEnd<S: Steps> {
    pub front: Arc<S::Front>,
    pub writer: S::Writer,
    /// Values for the store the lowering made (the images of dependency crates the head's last
    /// front end resolved), put by the loop as a compile's are.
    pub puts: Vec<Put>,
}

/// The readers a task is posted with: of its job's front end, and for a reach, of the front it
/// reaches from; for a compile, the images it relinks; for the tables, the job's writer itself.
pub struct Readers<S: Steps> {
    pub this: Option<S::Reader>,
    pub from: Option<S::Reader>,
    pub images: Vec<Image<S>>,
    pub writer: Option<S::Writer>,
}

impl<S: Steps> Default for Readers<S> {
    fn default() -> Readers<S> {
        Readers { this: None, from: None, images: Vec::new(), writer: None }
    }
}

/// An image kept linked between jobs at the path cargo writes it, so the next link of it
/// relinks only what changed. The loop owns every one; it moves it into the one compile that
/// relinks it ([`Front::compile`]) and takes it back with what the compile made, its link made
/// or failed; a compile that gives way hands it back before it is posted again. A link that
/// still finds none kept at a path it linked before says so ([`Linking::First`]).
pub struct Image<S: Steps> {
    pub path: String,
    pub kept: S::Linked,
}

/// What a front keeps of a linked image.
/// A job's front end as the loop keeps it ([`Steps::KeptFront`]).
pub trait KeptDb: Send + 'static {
    /// Its database, whose library parses the daemon's image stores
    /// ([`base_db::trees::write_section`]); `None` for a front end with none.
    fn db(&self) -> Option<&base_db::Db>;
}

pub trait Kept: Send + 'static {
    /// Its tables as the linker counts them: name, elements, bytes ([`Response::Tables`]).
    fn tables(&self) -> Vec<(&'static str, u64, u64)>;
}

/// What of a job's front end only the loop writes. Its writes are asked for, then applied
/// together by [`Writer::apply`] once no [`Steps::Reader`] of it is alive (the module's docs).
pub trait Writer<S: Steps>: Send + 'static {
    /// A reader of the front end as it stands, for one task about to be posted; none while a
    /// write is asked for and not applied, when the loop holds the task instead.
    fn reader(&self) -> Option<S::Reader>;
    /// Asks for the run of build script unit `run` applied to the units depending on it
    /// (`blaze_lower::Lowered::ran`): checked now, an error naming what is wrong with it,
    /// and applied by [`Writer::apply`].
    fn ran(&mut self, run: usize, ran: blaze_lower::Ran) -> Result<(), ApiError>;
    /// Asks for proc-macro unit `unit`'s expanders set (`blaze_lower::Lowered::macros`):
    /// checked now, applied by [`Writer::apply`].
    fn macros(&mut self, unit: usize, macros: blaze_lower::ProcMacroLoadResult) -> Result<(), ApiError>;
    /// Whether a write was asked for and is not applied yet.
    fn waiting(&self) -> bool;
    /// Every write asked for, applied as one, in the order asked, if no reader is alive:
    /// `Ok(true)` once applied (or none waited), `Ok(false)` while a reader is, asking again
    /// costing one load. An error ends the job. A write that panics (a maintainer's panic
    /// included) is caught by [`applying`] and kept for [`Writer::broke`]; the others are
    /// applied, and the front end is never kept ([`Writer::keep`] fails).
    fn apply(&mut self) -> Result<bool, ApiError>;
    /// A write [`Writer::apply`] applied that panicked, taken, one a call.
    fn broke(&mut self) -> Option<Broke>;
    /// On a worker, the writer handed to the tables' task as the front end's last use: its
    /// tables (name, elements, bytes), measured with it exclusive; an
    /// error while a reader of it is alive.
    fn tables(&mut self) -> Result<Vec<(&'static str, u64, u64)>, Failed>;
    /// On the loop, once every task of the job's build has ended and before it is kept: what
    /// the build's front end found wrong only once all of it ran (a dependency's image that
    /// differs from what its crate resolves to, under `BLAZE_DEP_IMAGES=check`), as the
    /// build's error.
    fn finished(&mut self) -> Result<(), ApiError> {
        Ok(())
    }
    /// On the loop, once every task of the job's build has ended and before it is kept: each
    /// library crate of the build that is not a workspace member, sorted by cargo package
    /// name, with whether its check outcome and its bodies were served from the store,
    /// checked, or refused ([`blaze_api::Worked::served`]).
    fn served(&self) -> Result<Vec<blaze_api::ServedCrate>, ApiError>;
    /// Under `-v`: the note of what storing the kept front end this build lowered into did with
    /// the goals its build offered to the persistent goal cache. None when nothing was kept.
    fn goals_noted(&self) -> Option<blaze_api::Diagnostic>;
    /// On the loop, once every task of the job's build has ended: the front end with every
    /// write applied, for the head's next build to lower into. An error while a reader of it
    /// is alive or a write fails to apply: the build ends with it, never a silent full lower
    /// next time.
    fn keep(self) -> Result<S::KeptFront, ApiError>;
}

/// A write to a job's front end that panicked: the unit it was for (a proc macro's load, or a
/// build script's run) and the panic. The job fails with it, and every unit that depends on
/// that one is refused, while keep-going builds the rest (`Jobs::apply_writes`).
#[derive(Debug)]
pub struct Broke {
    pub unit: usize,
    /// What the write was: "a proc macro's load", "a build script's run".
    pub what: &'static str,
    pub panic: super::plane::Panic,
}

/// `write`, the write `what` for unit `unit`, applied on the loop: its panic as a [`Broke`]
/// instead of unwinding the loop, which would end every request the daemon serves.
pub fn applying<T>(unit: usize, what: &'static str, write: impl FnOnce() -> T) -> Result<T, Broke> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(write))
        .map_err(|payload| Broke { unit, what, panic: super::plane::panicked(&*payload) })
}

/// One job's front end, its units lowered: shared by the job's tasks, read only.
pub trait Front<S: Steps>: Send + Sync + 'static {
    /// On the loop, as unit `unit` (an index into `Units::graph.units`) is about to start:
    /// whether nothing of it will be made again (cargo's `Fresh`).
    fn fresh(&self, unit: usize) -> bool;
    /// On a worker: phase `phase` of unit `unit` (any mode but `RunCustomBuild`), which is
    /// checked and, unless the goal is [`Goal::Check`] or the unit is a check, compiled and
    /// linked. Every unit it depends on is done, the runs of build scripts it depends on
    /// applied, and the phases before `phase` ([`Phase`]) are done; what each made waits for
    /// the next with the front. [`Phase::Store`] gives the compile; every other phase none.
    /// `images` are the kept images at the unit's outputs (`UnitPlan::outputs`), given to
    /// [`Phase::Link`] alone: it takes each it relinks and puts back what it linked.
    fn compile(&self, reader: S::Reader, unit: usize, phase: Phase, images: &mut Vec<Image<S>>) -> Result<Option<Compiled>, Failed>;
    /// The error that unit `unit` can't be made, saying `what`: a diagnostic of the unit,
    /// named and rendered as its others are, for a compile that panicked.
    fn refused(&self, unit: usize, what: String) -> Diagnostic;
    /// On the loop: the run of build script unit `unit` (mode `RunCustomBuild`). Its run
    /// reports [`Response::BuildScript`] (and cargo's warnings as diagnostics), and before it
    /// ends applies its directives as cargo does to the units they reach (`rustc-cfg`,
    /// `rustc-env`, `rustc-link-lib`, `rustc-link-search`, `DEP_<links>_*` of dependents).
    /// It ends not ok for a script that failed, after the diagnostic cargo gives.
    fn script(&self, unit: usize, tags: u64) -> Result<S::Runs, ApiError>;
    /// On the loop, once every unit is built: the tests of the build (their artifacts in
    /// `built`) that `selection` selects, each reported as a [`Response::TestResult`]; it ends
    /// not ok when one failed. It runs compiled binaries and reads nothing of the front end.
    fn tests(&self, built: &[Artifact], selection: Selection<'_>, tags: u64) -> Result<S::Runs, ApiError>;
    /// On a worker, for `blaze test --ub` once every unit is checked: the tests `selection`
    /// selects (as libtest would: its filters, `--exact`, `#[ignore]`), by name in libtest's
    /// order, each run by [`Front::evaluate`].
    fn ub_tests(&self, reader: S::Reader, selection: Selection<'_>) -> Result<Vec<String>, Failed>;
    /// On a worker: the test named `test` of [`Front::ub_tests`] run in blaze's MIR
    /// evaluator under `ub` (`hir_ty_ub::run_test`), nothing compiled. It passes, fails as a
    /// panicking test does (honoring `#[should_panic]`), or fails with the evaluator's report
    /// then `seed: N` as its output for undefined behavior, or with what the evaluator can't
    /// run: never a pass and never a compiled run in its place.
    fn evaluate(&self, reader: S::Reader, test: &str, ub: Ub) -> Result<TestResult, Failed>;
    /// On the loop, once every unit is built: `bin` (cargo's choice when none) run with
    /// `args`, reported as [`Response::Ran`].
    fn run(&self, built: &[Artifact], bin: Option<&str>, args: &[String], tags: u64) -> Result<S::Runs, ApiError>;
    /// On a worker: what the change from `from`'s view to this one reaches, building nothing.
    fn reach(&self, reader: S::Reader, from: &Self, from_reader: S::Reader, changes: &[Change]) -> Result<Reach, Failed>;
    /// On a worker: `edits` made in the view's files (`blaze_intent::apply`), building
    /// nothing: what they change, for the session to commit ([`Response::Edited`]).
    fn edit(&self, reader: S::Reader, edits: &[Edit]) -> Result<Edited, Failed>;
    /// On a worker, for `Work::Ahead`: the units (by their place in the plan's graph) of the
    /// test binaries holding a test `filter` selects, as libtest selects, to build ahead of a
    /// run that has not been asked for.
    fn ahead(&self, reader: S::Reader, filter: &[String]) -> Result<Vec<u32>, Failed>;
    /// On a worker: each codegen unit's inputs and output of the view's last build, as
    /// [`Response::CodegenInputs`] lists them.
    fn codegen_inputs(&self, reader: S::Reader) -> Result<Vec<(ContentId, ContentId)>, Failed>;
    /// On a worker: the front end's tables, each by its owner, measured through the job's
    /// `writer` ([`Writer::tables`]), with `images`, the linker's tables of the images kept
    /// when the loop posted it ([`Response::Tables`]).
    fn tables(&self, writer: S::Writer, images: &[(&'static str, u64, u64)]) -> Result<Vec<Table>, Failed>;
    /// On a worker, once every unit of a [`Goal::Check`] build is checked and every build
    /// script's run and proc macro's load applied: package `program`'s crate and every crate it
    /// is made of packed for blaze's in-browser compiler, its macros' expansions recorded
    /// through the job's `writer`, the front end's last use ([`Response::Packed`]'s bytes).
    fn pack(&self, writer: S::Writer, program: &str) -> Result<Vec<u8>, Failed>;
    /// On a worker, as [`Front::pack`] is but for `blaze doc`: the documentation of the crates
    /// `cargo doc` documents for the check's root units (`blaze-doc`), and unless `no_deps` of
    /// every library they depend on, made through the job's `writer`, the front end's last use,
    /// which sets `cfg(doc)` on them. `private` is `--document-private-items`.
    /// [`Response::Documented`], each file as a [`Response::DocPage`], then each warning as a
    /// [`Response::Diagnostic`].
    fn doc(&self, writer: S::Writer, no_deps: bool, private: bool) -> Result<Vec<Response>, Failed>;
}

/// User code of a job run through the loop's driver: it asks with effects whose tags are in
/// the range it was given (`tags..tags + 2^32`), takes their completions, and reports. Each
/// method that can end it says so with `Some(ok)`; after that it is asked nothing more. A
/// build script's is given its job's [`Writer`] to apply its run with; tests and runs, none.
pub trait Runs<S: Steps>: Send + 'static {
    /// Asks for what it runs first; it may end at once (a result the store kept).
    fn start(
        &mut self,
        store: &Store,
        writer: Option<&mut S::Writer>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool>;
    fn owns(&self, tag: Tag) -> bool;
    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        writer: Option<&mut S::Writer>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool>;
    /// Ends it early: what it runs is killed and ends through `complete`, not ok; with nothing
    /// running it ends here.
    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool>;
}
