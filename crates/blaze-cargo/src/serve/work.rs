//! What a job of the daemon asks of the build (docs/v2/plan.md section 6): the steps
//! between a plan and its outputs, behind traits so the loop runs the same in the daemon
//! ([`crate::serve::unit::Real`]) and in the simulation ([`crate::serve::tests`]). The loop is
//! generic over its [`Steps`], whose associated types are the front end's: `blaze serve` is
//! one instance of it, on `Real`, with no trait object and no downcast.
//!
//! Where each is called is part of its contract:
//!
//! - **On an IO thread of the plane**: [`Steps::fetch`], [`Steps::local`], [`Steps::locals`], [`Steps::library`], [`Steps::native`]. They may block on
//!   the network or the disk; they touch no state of the loop.
//! - **On a worker of the plane**: [`Steps::lower`], [`Front::compile`], [`Front::reach`],
//!   [`Front::ub_tests`], [`Front::evaluate`], [`Front::codegen_inputs`], [`Front::tables`],
//!   [`Front::doc`].
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
//! never cloned (docs/v2/plan.md). The tasks that read it are those of a
//! batch added with `reads` ([`crate::serve::plane::Plane::add`]): every compile, the
//! `--ub` selection and evaluations, a reach (of both its views) and the codegen inputs. Each
//! is posted with a [`Steps::Reader`], a shared reference it holds for its run and drops as it ends,
//! with no lock, no clone of the database and no cancellation. The tables are measured with
//! the database exclusive, so no reader adds to them: their task, the front end's last use, is
//! handed the job's [`Writer`] itself. So is the documentation's, which sets `cfg(doc)` on the
//! crates it documents, once its check's own plane request is over.
//!
//! A build script's output ([`Writer::ran`]) and a proc macro's load ([`Writer::macros`]) are
//! writes, and so is letting go of what a unit that compiled leaves in the front end for no task
//! to come ([`Writer::compiled`]; the daemon's `unit::release`), which waits only when memory is
//! short or no unit is open. Each is checked as it is asked for and applied by [`Writer::apply`], on the loop,
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

use blaze_api::{
    ApiError, Artifact, Diagnostic, Edit, Edited, Reach, Response, Table, TestResult, Ub,
};
use blaze_cas::Store;
use blaze_core::{Completion, Effect, Tag};
use blaze_ids::ContentId;
use blaze_protocol::events::NodeKind;
use blaze_query::{Database, DbError, Query, QueryKey};
use blaze_units::query::{BuildConfig, Fetch, Plan, Planned, Units};
use blaze_vfs::Bytes;
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

/// A file of a fetched or read source. Its bytes are [`Bytes::kept`] in [`blaze_mem::Tag::Vfs`]
/// where they are read in (a `.crate`'s, a checkout's, the standard library's): they become the
/// VFS tree's with no copy and live as long as it holds them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceFile {
    pub path: String,
    pub bytes: Bytes,
    pub mode: Mode,
}

impl SourceFile {
    /// The file at `at`, read whole into a [`Bytes::kept`] block of the size its metadata gives
    /// (`len`): an error naming it when it is shorter or longer by the time it is read.
    pub fn read(at: &std::path::Path, len: u64) -> std::io::Result<Bytes> {
        use std::io::Read as _;
        let named = |error: std::io::Error| {
            std::io::Error::new(error.kind(), format!("{}: {error}", at.display()))
        };
        let mut file = std::fs::File::open(at).map_err(named)?;
        let len = usize::try_from(len)
            .map_err(|_| named(std::io::Error::other("a file past the address space")))?;
        let bytes =
            Bytes::kept(blaze_mem::Tag::Vfs, len, |place| file.read_exact(place)).map_err(named)?;
        match file.read(&mut [0])? {
            0 => Ok(bytes),
            _ => Err(named(std::io::Error::other(format!(
                "grew past its {len} bytes while it was read"
            )))),
        }
    }
}

/// What a job's work is, as far as its units' work differs by it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Goal {
    /// Stops after the front end.
    Check,
    Build,
    Test,
    Run,
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
    /// Each of `units.native` laid out ([`Steps::native`]), in its order: what a build script
    /// of its packages is given. Empty until the job's native stage settles, and for a build
    /// whose units name none.
    pub native: Arc<[super::native::NativeDir]>,
    /// The store, read by tasks with no lock (a read is valid while its
    /// [`blaze_cas::enter`] guard is held); written only by the loop.
    pub store: blaze_cas::Reader,
    /// The standard library's store, shared by this build's daemons, read as `store` is: a
    /// std unit's record is found there when `store` has none, as of the look the job's build
    /// waited for.
    pub std: blaze_cas::Reader,
    /// The daemon's proc macros: where their expansions are kept (in `store`), lent to every
    /// load of a proc-macro unit's macros and every read of what an expansion read.
    pub macros: Arc<blaze_macro_eval::Macros>,
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

/// A path outside the view and every source, read from the disk where it is
/// ([`blaze_units::query::FetchKind::Local`]): a `path` dependency outside the checkout, or a
/// manifest its workspace search looks for above it. Mounted as a root of its own, keyed by its
/// bytes as a fetched source is, and kept for later jobs only while the watch of directories
/// outside views vouches that nothing under it changed ([`LocalRead::watched`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LocalRead {
    /// Where it is mounted, as the load names it: the package directory around the path the
    /// load read, or that path alone when no package holds it.
    pub at: String,
    /// Its files: a directory's whole tree (`under` ""), or one file, or none for a path that
    /// is not there (`under` the file's name, so it reads as absent).
    pub source: Source,
    /// For a directory, the canonical path it was read from (every symlink resolved), whose
    /// watch outside views says when to read it again; `None` for one file or an absent
    /// path, read again by every job.
    pub watched: Option<String>,
    /// [`blaze_vfs::outside::generation`] as read before its files were.
    pub generation: u64,
}

/// A value for the store, put by the loop.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Put {
    pub key: blaze_cas::Key,
    pub kind: blaze_cas::Kind,
    pub bytes: Vec<u8>,
}

/// A value for the store from a kept front end ([`Steps::store_kept`]) and what it is ("the
/// def-map image of dependency crate `serde`"): what an error names when the store holds other
/// bytes under its key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeptPut {
    pub put: Put,
    pub what: Arc<str>,
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
    /// A unit only checked (`checks_only`) failed (`ok` false) only in its bodies and lints,
    /// its items sound ([`blaze_diagnostics::CrateCheck::items_broken`] false): its node fails
    /// open ([`blaze_graph::Outcome::FailedOpen`]), so under keep-going what is checked against
    /// its items still is, while it stays failed. Never true when `ok` is, for a unit whose
    /// code is built or run, nor for a failure of codegen, a refusal or a panic.
    pub open: bool,
    /// Why it failed, for its node and the build's report of what it left unchecked: its
    /// errors (in its items, or only in its bodies or lints), or blaze's own failure to make
    /// it. None when `ok`, or when nothing more is known than that it failed.
    pub why: Option<String>,
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
    /// Why its record and each template its codegen missed were not in the store, each with
    /// its subject within the unit ([`blaze_kernel::Miss`]): what `blaze why` explains once
    /// the build ends. Empty for a unit its record replayed.
    pub misses: Vec<blaze_kernel::Miss>,
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
    pub const BUILD: [Phase; 5] =
        [Phase::Names, Phase::Bodies, Phase::Codegen, Phase::Link, Phase::Store];
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
    /// None was kept at its path, so there was nothing to patch: the image's first link, or
    /// the first since the kept link images' budget let its image go, which the loop says as
    /// it posts the link (`job::Paths::take_images`).
    First,
    /// What changed was placed into the room the kept image keeps.
    Spliced,
    /// Nothing it is given changed: the kept image's file as it was, nothing staged.
    Unchanged,
    /// The image was laid out whole again although one was kept.
    Full(Full),
}

impl Linking {
    /// The image was laid out whole, rather than patched or kept as it was.
    pub(crate) fn placed(self) -> bool {
        !matches!(self, Linking::Spliced | Linking::Unchanged)
    }

    /// As [`blaze_api::Worked::links`] says it.
    pub(crate) fn how(self) -> blaze_api::LinkHow {
        match self {
            Linking::First => blaze_api::LinkHow::First,
            Linking::Spliced => blaze_api::LinkHow::Spliced,
            Linking::Unchanged => blaze_api::LinkHow::Unchanged,
            // alloc: a name a field that differs, for a link that was not spliced
            Linking::Full(Full::Output(fields)) => {
                blaze_api::LinkHow::Full { why: fields.names().map(str::to_owned).collect() }
            }
            // alloc: one name, for a link that was not spliced
            Linking::Full(Full::NotIncremental) => {
                blaze_api::LinkHow::Full { why: vec!["not incremental".to_owned()] }
            }
            // alloc: one name, for a link that was not spliced
            Linking::Full(Full::Layout) => {
                blaze_api::LinkHow::Full { why: vec!["layout".to_owned()] }
            }
            // alloc: one name, for a link that was not spliced
            Linking::Full(Full::NoThunk) => {
                blaze_api::LinkHow::Full { why: vec!["no thunk in reach".to_owned()] }
            }
            // alloc: one name, for a link that was not spliced
            Linking::Full(Full::NoRoom) => {
                blaze_api::LinkHow::Full { why: vec!["no room".to_owned()] }
            }
        }
    }
}

impl std::fmt::Display for Linking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Linking::First => f.write_str("first link"),
            Linking::Spliced => f.write_str("spliced"),
            Linking::Unchanged => f.write_str("unchanged"),
            Linking::Full(Full::Output(fields)) => {
                write!(f, "not spliced: the output changed: {fields}")
            }
            Linking::Full(Full::NotIncremental) => {
                f.write_str("not spliced: the output keeps no room")
            }
            Linking::Full(Full::Layout) => {
                f.write_str("not spliced: the profile its code is laid out by changed")
            }
            Linking::Full(Full::NoThunk) => f.write_str(
                "not spliced: full link: the splice could not place a thunk in a branch's reach",
            ),
            Linking::Full(Full::NoRoom) => f.write_str(
                "not spliced: full link: the kept image has no room for the change",
            ),
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
    /// The profile its code is laid out by changed (docs/v2/plan/design/code-layout.md): an
    /// image keeps its first link's order, so it is laid out again.
    Layout,
    /// The kept image refused the change as a splice: a branch in code it holds in place
    /// reaches neither its target nor the patch region, where a splice's thunks go
    /// ([`blaze_link::Refusal::NoThunk`]). The image was linked from scratch, and the build
    /// says which branch.
    NoThunk,
    /// The kept image refused the change for want of room: a region, the jump table or the
    /// patch region is full, or its thread-locals would move ([`blaze_link::LinkError::no_room`]).
    /// The image was linked from scratch, with room again, and the build says what was full.
    NoRoom,
}

/// Fields of [`blaze_link::Output`] as a set, a bit each in [`OutputFields::NAMES`]'s order.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct OutputFields(u32);

impl OutputFields {
    pub(crate) const NAMES: [&'static str; 18] = [
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
        "link",
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
            link,
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
            old.link != *link,
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
        Self::NAMES
            .into_iter()
            .enumerate()
            .filter(move |(at, _)| (self.0 >> at) & 1 == 1)
            .map(|it| it.1)
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

/// A job's build dirs leased ([`Steps::lease`]): every lease taken; `Ok(Err((dir, wait)))`
/// when `dir`'s cannot be taken now (held exclusively by a collector's rename, or its file
/// without a whole nonce yet), and the lowering gives way and asks again for at most
/// [`blaze_gc::lease::GIVE_WAY`]; `Err` naming why a dir may not be leased at all.
pub type Leased =
    Result<Result<Vec<blaze_gc::lease::Lease>, (String, blaze_gc::lease::Wait)>, String>;

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
    /// On an IO thread: the path a [`blaze_units::query::FetchKind::Local`] names, read from the
    /// disk where it is ([`crate::serve::fetch::local::read`]): the package directory around
    /// it, or the path alone.
    fn local(&self, fetch: &Fetch) -> Result<LocalRead, ApiError> {
        let probe = matches!(fetch.kind, blaze_units::query::FetchKind::Local { probe: true });
        crate::serve::fetch::local::read(&fetch.at, probe)
    }
    /// On an IO thread: each of these local mounts read again exactly where it is mounted
    /// ([`crate::serve::fetch::local::read_at`]), since what is under it may have changed.
    fn locals(&self, ats: &[String]) -> Result<Vec<LocalRead>, ApiError> {
        ats.iter().map(|at| crate::serve::fetch::local::read_at(at)).collect()
    }
    /// On an IO thread: the standard library's `library/`, put into the VFS once: where it is
    /// named (`/rustc/<commit>/library`), its fingerprint, and its files unless `kept` (a
    /// fingerprint, and where that library is mounted) has it there, read before by this
    /// daemon or one before it.
    fn library(&self, kept: &[(String, String)]) -> Result<Library, ApiError>;
    /// On an IO thread, before a job's lowering when its units name any, once a digest for
    /// every job asking it: `input`'s archive fetched by its digest and laid out read-only,
    /// its directory ([`super::native::ensure`]). One that can't be is the error of each job
    /// asking it, naming it; there is no other place a build script is pointed at. A wait for
    /// another process laying it out stops once `cancelled` is set: once no job waits on it.
    fn native(
        &self,
        input: &blaze_units::query::NativeInput,
        cancelled: &AtomicBool,
    ) -> Result<std::path::PathBuf, ApiError> {
        let _ = cancelled;
        Err(ApiError::new(
            format!("pinned native input `{}`: these steps lay out none", input.name),
            Some(input.pin.clone()), // clone: an error's place, once
        ))
    }
    /// On a worker, before a job's lowering builds anything: the leases of `units`' build dirs
    /// (`target_dir`, `build_dir`) for `checkout`, `builds` the dir of every workspace's build
    /// dir, each dir made and marked on disk as docs/gc.md says (the daemon's is
    /// [`super::job::lease_build_dirs`]). The leases are held until the job is dropped.
    fn lease(&self, units: &Units, checkout: &str, builds: &str) -> Leased;
    /// On a worker: the job's units lowered into the front end. `kept` is the front end of
    /// the head's last build ([`Writer::keep`]): built from the same units and config, it is
    /// moved to the job's view, its memos kept, and what changed alone read again; else it
    /// is dropped and the units lowered anew.
    fn lower(
        &self,
        job: &JobInput,
        kept: Option<Self::KeptFront>,
    ) -> Result<FrontEnd<Self>, Failed>;
    /// On a worker, once a job's front end is kept ([`Writer::keep`]) and before the job is
    /// reported done: what its build leaves for the store, written from it as the puts for the
    /// loop to make (the images of the dependency crates it resolved, the outcomes and checked
    /// bodies of those it checked, the constants they evaluated, the goals and bodies' steps
    /// offered). So what one build computed is in the store, durable, by the time it answers,
    /// whether or not the head builds again in this daemon's life: a stop, a crash or a reboot
    /// after it loses none of it (persisted-state-round-trips). Called once per kept front
    /// end, while no task reads it; one already stored hands back no puts.
    fn store_kept(&self, kept: &mut Self::KeptFront) -> Result<Vec<KeptPut>, ApiError>;
    /// On a worker, by the task that stores a kept front end and before it stores: what the
    /// front end's query engine logged of why its queries ran since the last take, each memo
    /// and input named with the database in hand ([`Writer::run_log`], taken here so naming a
    /// build's runs is never on the loop before the build's answer); none for a front end
    /// with no database, and while the run log is off.
    fn kept_run_log(&self, kept: &Self::KeptFront) -> blaze_query::db::Taken {
        let _ = kept;
        blaze_query::db::Taken::default()
    }
    /// On a worker, by the task that stores a kept front end, after [`Steps::store_kept`] and
    /// after the build's answer: what the build's end lets go of its front end (every MIR it
    /// made, and the values no build read since the one before). The front end is the task's
    /// alone, as it was the job's writer's. A release only lowers memory, so it is never a
    /// precondition of the checkout's next build: once `wanted` is set (a request of the
    /// checkout waits for the front end) it is skipped, unless blaze is past its limit's line,
    /// and that build goes on with what was not let go.
    fn release_kept(&self, kept: &mut Self::KeptFront, wanted: &AtomicBool) {
        let _ = (kept, wanted);
    }
    /// On the loop: `blaze run`'s start of the built program at `program` as process `pid`
    /// ([`blaze_api::Request::Launch`]): the variables it runs on the daemon's code memory
    /// with, or `None` for a program that is no dev executable and runs its file.
    fn launch(&self, program: &str, pid: u32) -> Result<Option<Vec<(String, String)>>, ApiError> {
        let _ = (program, pid);
        Ok(None)
    }
    /// On the loop, once at start: the queries of the daemon's database.
    fn register(&self, db: &mut Database) {
        blaze_units::query::register(db);
    }
    /// On a worker: the plan of `view` (opened with its [`BuildConfig`]).
    fn plan(&self, db: &Database, view: ViewId) -> Result<Planned, Failed> {
        match db.pin().get::<Plan>(view, &()) {
            Ok(planned) => Ok(planned.get()),
            Err(DbError::Cancelled) => Err(Failed::Again),
            Err(error) => Err(Failed::Error(ApiError::new(
                format!("the load of the workspace: {error}"),
                None,
            ))),
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
    /// Values for the store the lowering made, put by the loop as a compile's are. What a
    /// head's last front end leaves for the store is put before that build answers
    /// ([`Steps::store_kept`]), not here.
    pub puts: Vec<Put>,
    /// Why the lowering made the front end anew where it could have lowered into the head's
    /// last one, for `blaze why`: a variable that front end read moved. Handed to the job's
    /// misses.
    pub misses: Vec<blaze_kernel::Miss>,
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
    /// The bytes it holds while kept between links: what the kept images' budget weighs it
    /// by ([`super::job`]'s `KeptImages`).
    fn held(&self) -> u64;
    /// Whether a run of user code maps it now (a dev executable's running program): such an
    /// image is never let go, or the next link could not patch what runs.
    fn live(&self) -> bool;
    /// Lets go of what it keeps only to start its next link warm, as it is kept idle.
    fn rest(&mut self);
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
    fn macros(
        &mut self,
        unit: usize,
        macros: blaze_lower::ProcMacroLoadResult,
    ) -> Result<(), ApiError>;
    /// Whether a write was asked for and is not applied yet, or the front end asks to let go
    /// of what nothing still to compile reads (the daemon's `unit::release`): either way the
    /// loop posts no reading task of the job until [`Writer::apply`] applied it.
    fn waiting(&self) -> bool;
    /// On the loop, as unit `unit`'s first task starts: it is open until [`Writer::compiled`].
    fn began(&mut self, unit: usize) {
        let _ = unit;
    }
    /// On the loop, as unit `unit`'s compile ended (its last phase gave its outcome, ok or
    /// not): nothing of this build compiles it again, and what its front end keeps for it
    /// alone may be let go, by a write [`Writer::waiting`] asks for.
    fn compiled(&mut self, unit: usize) {
        let _ = unit;
    }
    /// On the loop, while the plane holds a task back for memory the process holds besides
    /// its tasks (`blaze_graph::Plane::short`): let go of what the front end keeps for no
    /// task to come, by a write [`Writer::waiting`] asks for, if that can help.
    fn short(&mut self) {}
    /// On the loop, as the system says memory is short (docs/memory-budget.md section 7,
    /// "Pressure"): let go of what the front end keeps for no task to come, by a write
    /// [`Writer::waiting`] asks for, now, whatever this daemon's own budget says.
    fn pressed(&mut self) {}
    /// Every write asked for, applied as one, in the order asked, if no reader is alive:
    /// `Ok(true)` once applied (or none waited), `Ok(false)` while a reader is, asking again
    /// costing one load. A write that fails (its error, or a panic, a maintainer's included,
    /// caught by [`applying`]) is kept for [`Writer::broke`] as a failure of its unit; the
    /// others are applied, and the front end is never kept ([`Writer::keep`] fails). An error
    /// is the writer's own, owned by no unit's write (the dependencies' serve, which leaves the
    /// front end partly served), and ends the job, with or without keep-going.
    fn apply(&mut self) -> Result<bool, ApiError>;
    /// A write [`Writer::apply`] applied that failed or panicked, taken, one a call.
    fn broke(&mut self) -> Option<Broke>;
    /// On a worker, the writer handed to the tables' task as the front end's last use: its
    /// tables (name, elements, bytes), measured with it exclusive; an
    /// error while a reader of it is alive.
    fn tables(&mut self) -> Result<Vec<(&'static str, u64, u64)>, Failed>;
    /// On the loop, once every task of the job's build has ended and before it is kept: what
    /// the build's front end found wrong only once all of it ran (a dependency's image that
    /// differs from what its crate resolves to, under `BLAZE_DEP_IMAGES=check`; a profile blaze
    /// made that applied to nothing), as the build's error, or its warnings to say, with what
    /// `blaze check` lints over every configuration of a package at once (`blaze::dead`; an
    /// error of those fails the build). `ok`: whether the build succeeded; `built`: its nodes'
    /// units, a unit once a node.
    fn finished(
        &mut self,
        ok: bool,
        built: &[u32],
    ) -> Result<Vec<blaze_api::Diagnostic>, ApiError> {
        let _ = (ok, built);
        Ok(Vec::new())
    }
    /// On the loop, once every task of the job's build has ended and before it is kept: each
    /// library crate of the build that is not a workspace member, sorted by cargo package
    /// name, with whether its check outcome and its bodies were served from the store,
    /// checked, or refused ([`blaze_api::Worked::served`]).
    fn served(&self) -> Result<Vec<blaze_api::ServedCrate>, ApiError>;
    /// On the loop, as the job's build ends and before it is kept: what its front end's query
    /// engine logged of why its queries ran since the last take, each memo and input named
    /// with the database in hand ([`blaze_query::db::take_log_in`]); none for a front end
    /// with no database, and while the run log is off.
    fn run_log(&self) -> blaze_query::db::Taken {
        blaze_query::db::Taken::default()
    }
    /// Under `-v`: the note of what storing the kept front end this build lowered into did with
    /// the goals its build offered to the persistent goal cache. None when nothing was kept.
    fn goals_noted(&self) -> Option<blaze_api::Diagnostic>;
    /// On the loop, once every task of the job's build has ended: the front end with every
    /// write applied, for the head's next build to lower into. An error while a reader of it
    /// is alive or a write fails to apply: the build ends with it, never a silent full lower
    /// next time. It lets nothing go: what the build's end releases is released by the task
    /// that stores the kept front end, after the answer ([`Steps::release_kept`]).
    fn keep(self) -> Result<S::KeptFront, ApiError>;
}

/// A write to a job's front end that failed or panicked: the unit it was for (a proc macro's
/// load, or a build script's run) and how it broke. The job fails with it, and every unit that
/// depends on that one is refused, naming it, while keep-going builds the rest
/// (`Jobs::apply_writes`).
#[derive(Debug)]
pub struct Broke {
    pub unit: usize,
    /// What the write was: "a proc macro's load", "a build script's run".
    pub what: &'static str,
    pub how: Broken,
    /// Whether the unit itself is refused too: the write was part of making it (its harness,
    /// its dependencies served), not of what it made (a run's output, a proc macro's load).
    pub itself: bool,
}

/// How a write [`Broke`].
#[derive(Clone, Debug)]
pub enum Broken {
    /// It panicked as it was applied.
    Panicked(super::plane::Panic),
    /// It was applied and refused with this error.
    Failed(ApiError),
}

impl std::fmt::Display for Broken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Broken::Panicked(panic) => write!(f, "the daemon {panic}"),
            Broken::Failed(error) => match &error.at {
                Some(at) => write!(f, "it failed at {at}: {}", error.what),
                None => write!(f, "it failed: {}", error.what),
            },
        }
    }
}

impl Broke {
    /// `error`, the failure of the write `what` for unit `unit`.
    pub fn failed(unit: usize, what: &'static str, error: ApiError) -> Broke {
        Broke { unit, what, how: Broken::Failed(error), itself: false }
    }
}

/// `write`, the write `what` for unit `unit`, applied on the loop: its panic as a [`Broke`]
/// instead of unwinding the loop, which would end every request the daemon serves. One of name
/// resolution's typed stops ([`hir_def::nameres::stopped`]), raised by a query the write's
/// maintenance pass asked, is the write's error, named, not a panic with no message.
pub fn applying<T>(unit: usize, what: &'static str, write: impl FnOnce() -> T) -> Result<T, Broke> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(write)).map_err(|payload| {
        match hir_def::nameres::stopped(&*payload) {
            Some(stop) => Broke::failed(unit, what, ApiError::new(stop, None)),
            None => Broke {
                unit,
                what,
                how: Broken::Panicked(super::plane::panicked(&*payload)),
                itself: false,
            },
        }
    })
}

/// `write`, the write `what` for unit `unit`, applied on the loop: its error or its panic as a
/// [`Broke`] of that unit, never the job's.
pub fn applying_fallible(
    unit: usize,
    what: &'static str,
    write: impl FnOnce() -> Result<(), ApiError>,
) -> Result<(), Broke> {
    applying(unit, what, write)?.map_err(|error| Broke::failed(unit, what, error))
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
    fn compile(
        &self,
        reader: S::Reader,
        unit: usize,
        phase: Phase,
        images: &mut Vec<Image<S>>,
    ) -> Result<Option<Compiled>, Failed>;
    /// The error that unit `unit` can't be made, saying `what`: a diagnostic of the unit,
    /// named and rendered as its others are, for a compile that panicked.
    fn refused(&self, unit: usize, what: String) -> Diagnostic;
    /// On a worker, as a phase task of unit `unit` starts: the unit's crate (its target's
    /// name) and the unit as its phase spans name it (`serde v1.0.219 (lib)`), for its `task`
    /// span. `None` for a front that names no units.
    fn subject(&self, unit: usize) -> Option<(String, String)> {
        let _ = unit;
        None
    }
    /// On the loop: the run of build script unit `unit` (mode `RunCustomBuild`). Its run
    /// reports [`Response::BuildScript`] (and cargo's warnings as diagnostics), and before it
    /// ends applies its directives as cargo does to the units they reach (`rustc-cfg`,
    /// `rustc-env`, `rustc-link-lib`, `rustc-link-search`, `DEP_<links>_*` of dependents).
    /// It ends not ok for a script that failed, after the diagnostic cargo gives.
    fn script(&self, unit: usize, tags: u64) -> Result<S::Runs, ApiError>;
    /// On the loop, once every unit is built: the tests of the build (their artifacts in
    /// `built`) that `selection` selects, each reported as a [`Response::TestResult`]; it ends
    /// not ok when one failed. It runs compiled binaries and reads nothing of the front end.
    fn tests(
        &self,
        built: &[Artifact],
        selection: Selection<'_>,
        tags: u64,
    ) -> Result<S::Runs, ApiError>;
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
    fn run(
        &self,
        built: &[Artifact],
        bin: Option<&str>,
        args: &[String],
        tags: u64,
    ) -> Result<S::Runs, ApiError>;
    /// On a worker: what the change from `from`'s view to this one reaches, building nothing
    /// more: both views' proc macros and build scripts' runs were built and applied before it
    /// is posted (`Jobs::next`).
    fn reach(
        &self,
        reader: S::Reader,
        from: &Self,
        from_reader: S::Reader,
        changes: &[Change],
    ) -> Result<Reach, Failed>;
    /// On a worker: `edits` made in the view's files (`blaze_intent::apply`), building
    /// nothing more: the view's proc macros and build scripts' runs were built and applied
    /// before it is posted (`Jobs::next`). What they change, for the session to commit
    /// ([`Response::Edited`]).
    fn edit(&self, reader: S::Reader, edits: &[Edit]) -> Result<Edited, Failed>;
    /// On a worker, for `Work::Ahead`: the units (by their place in the plan's graph) of the
    /// test binaries holding a test `filter` selects, as libtest selects, to build ahead of a
    /// run that has not been asked for; the view's proc macros and build scripts' runs were
    /// built and applied before it is posted (`Jobs::next`).
    fn ahead(&self, reader: S::Reader, filter: &[String]) -> Result<Vec<u32>, Failed>;
    /// On a worker: each codegen unit's inputs and output of the view's last build, as
    /// [`Response::CodegenInputs`] lists them.
    fn codegen_inputs(&self, reader: S::Reader) -> Result<Vec<(ContentId, ContentId)>, Failed>;
    /// On a worker: the front end's tables, each by its owner, measured through the job's
    /// `writer` ([`Writer::tables`]), with `images`, the linker's tables of the images kept
    /// when the loop posted it ([`Response::Tables`]).
    fn tables(
        &self,
        writer: S::Writer,
        images: &[(&'static str, u64, u64)],
    ) -> Result<Vec<Table>, Failed>;
    /// On a worker, once every unit of a [`Goal::Check`] build is checked and every build
    /// script's run and proc macro's load applied, for `blaze doc`: the documentation of the crates
    /// `cargo doc` documents for the check's root units (`blaze-doc`), and unless `no_deps` of
    /// every library they depend on, made through the job's `writer`, the front end's last use,
    /// which sets `cfg(doc)` on them. `private` is `--document-private-items`.
    /// [`Response::Documented`], each file as a [`Response::DocPage`], then each warning as a
    /// [`Response::Diagnostic`].
    fn doc(&self, writer: S::Writer, no_deps: bool, private: bool)
    -> Result<Vec<Response>, Failed>;
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
    /// What its `complete`s put into the store since this was last asked, taken after each
    /// one so a ticket is handed over once. Its job is not reported done before every one is
    /// durable (persisted-state-round-trips): a run's result kept under its key, a test's
    /// record, a value fetched from the global cache, a retraction. No default: a run that
    /// puts and hands nothing over is answered on before what it put survives a crash, and the
    /// value a retraction refused is given again after it.
    fn take_puts(&mut self) -> RunPuts;
}

/// What a run of user code put ([`Runs::take_puts`]).
#[derive(Default, Debug)]
pub struct RunPuts {
    /// The last ticket of each segment its puts went to.
    pub tickets: blaze_cas::Tickets,
    /// Among them, a record that a value verified across checkouts depends on where its roots
    /// are ([`blaze_usercode::roots::verify::retraction`]): a kept front end lowers afresh.
    pub retracted: bool,
    /// Why a run was not reused, for `blaze why`: handed to its job's misses with its puts.
    pub misses: Vec<blaze_kernel::Miss>,
    /// The tests a run finished, for the history's `test_runs` table: handed to its job with
    /// its puts, which gives them its build id as it ends.
    pub test_runs: Vec<blaze_profile::facts::TestRunRow>,
}

impl RunPuts {
    /// Puts of what it kept, waited on.
    pub fn kept(&mut self, tickets: impl IntoIterator<Item = blaze_cas::Ticket>) {
        for ticket in tickets {
            self.tickets.add(ticket);
        }
    }

    /// The ticket of a retraction's record, when one was put.
    pub fn retraction(&mut self, ticket: Option<blaze_cas::Ticket>) {
        if let Some(ticket) = ticket {
            self.tickets.add(ticket);
            self.retracted = true;
        }
    }

    /// What `other` put, too, and its misses.
    pub fn extend(&mut self, other: RunPuts) {
        self.tickets.extend(&other.tickets);
        self.retracted |= other.retracted;
        self.misses.extend(other.misses);
        self.test_runs.extend(other.test_runs);
    }
}
