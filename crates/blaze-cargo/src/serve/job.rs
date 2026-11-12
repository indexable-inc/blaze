//! The daemon's [`Builder`] (docs/v2/plan.md section 6): each job of the
//! session as tasks of the plane, from its view to what it reports.
//!
//! A job that builds goes through, for each view it reads (two for a reach, else one):
//!
//! 1. The standard library's `library/`, put into the VFS once for every job and mounted
//!    where its sources are named (`/rustc/<commit>/library`).
//! 2. The view opened in the database with its [`BuildConfig`]: the invocation (cargo's, or
//!    made of an agent's [`Targets`]), the checkout's root and every source fetched so far.
//! 3. The load's [`Planned`] asked. Each source it [`Planned::Needs`] is fetched as an IO task
//!    (one fetch for every job that asks it), put into the VFS as a root and mounted durably;
//!    the view is opened again with the new mount and the plan asked again. A
//!    [`Planned::Failed`] finishes the job with its error.
//! 4. The plan's lock file written where cargo writes it, its warnings reported, and its units
//!    lowered into the front end.
//!
//! A checkout's last plan is kept: a build of the same view and config reads its database
//! view and units, steps 2 and 3 skipped. So is its last front end, which the next build of
//! the checkout lowers into ([`Steps::lower`]).
//!
//! Then its work: every unit a node (compiles on workers, build scripts on the loop), each
//! reported as it starts, its outputs put into the store and placed where cargo places them
//! (unless the file there is still as the same value was placed),
//! then `Built`, the tests or the run, and `Work`; or the one answer a reach, a trace, a
//! verify, an edit, the codegen inputs or the tables give. A job of declared intent
//! ([`Builder::speculate`]) runs the same in the plane's idle class until it is promoted. A
//! cancel skips what has not started, kills what runs, and the job still finishes once, not
//! ok.
//!
//! Every table is indexed, never hashed, by an id the daemon hands out: a job by its
//! [`JobId`]'s slot, a run of user code and an effect of the loop's own by the slot their tag
//! carries ([`slab`]), a node by the batch its id falls in, and a path or a checkout by the
//! `u32` it is interned to where it comes in.

#![forbid(unsafe_code)]

pub(super) mod doing;
pub(super) mod giving;
#[blaze::sans_io]
mod certificate;
mod certify;
mod kept;
mod noop;
#[blaze::sans_io]
mod request;
#[blaze::sans_io]
mod said;
mod slab;
mod written;
#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    /// The command line of each build a row of the history was made for ([`Job::ended`]), on
    /// the loop's thread: one a job's plane request, as a test counts them.
    pub(super) static ENDED_ROWS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) }; // alloc: none, a test's record
    /// Each build script run node [`Jobs::run_script`] held for its program, on the loop's
    /// thread: what a test sees of the hold, which no response says.
    pub(super) static HELD_RUNS: std::cell::RefCell<Vec<NodeId>> = const { std::cell::RefCell::new(Vec::new()) }; // alloc: none, a test's record
    /// Each held run [`end_held_runs`] ended, and the reason it gave the plane.
    pub(super) static ENDED_HELD: std::cell::RefCell<Vec<(NodeId, String)>> = const { std::cell::RefCell::new(Vec::new()) }; // alloc: none, a test's record
    /// Of each job's tests or run as it starts ([`Jobs::release_runs`]), how many of its job's
    /// placements were still in flight: none, or the program it runs may not be there yet.
    pub(super) static RUNS_STARTED_PLACING: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) }; // alloc: none, a test's record
    /// Each unit whose task or build script run a broken write refused ([`Refused`]), and the
    /// reason it gave the plane: what a test sees of a refused dependent, which no response
    /// says on its own.
    pub(super) static REFUSED_UNITS: std::cell::RefCell<Vec<(usize, String)>> = const { std::cell::RefCell::new(Vec::new()) }; // alloc: none, a test's record
    /// Each miss a build's history row took that says its checkout's last build lost what it
    /// left to settle ([`SETTLING_STEP`]), as `blaze why` is given it: its reason.
    pub(super) static ENDED_LOST: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) }; // alloc: none, a test's record
    /// Each output queued behind its path's effects in flight ([`Jobs::place_output`]), by
    /// its path: what a test sees of a later job's wait for an output an answered job left
    /// being placed.
    pub(super) static QUEUED_OUTPUTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) }; // alloc: none, a test's record
    /// The most effects ever in flight at one path at once ([`Jobs::begin_placing`]): one,
    /// while every placement of a path waits for the one before it.
    pub(super) static MOST_AT_A_PATH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The planted bug: an output is placed at once whatever is in flight at its path
    /// ([`Jobs::place_output`]), as before outputs outlived their job's answer.
    pub(super) static PLANT_NO_QUEUE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Each `errors` row a job's words gave ([`Jobs::note_errors`]): its build, its kind and
    /// its message. What a test reads of a table only the history's thread writes.
    pub(super) static ERROR_ROWS: std::cell::RefCell<Vec<(u64, String, String)>> = const { std::cell::RefCell::new(Vec::new()) }; // alloc: none, a test's record
    /// The planted bug: no job's errors are noted for the history ([`Jobs::note_errors`]),
    /// as before 2026-11-11.
    pub(super) static PLANT_NO_ERROR_ROWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// How many units each build sized the sources of ([`Jobs::build`]), in the order built:
    /// none for a build of the units its checkout's last build sized.
    pub(super) static SIZED_UNITS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) }; // alloc: none, a test's record
    /// The planted bug: a plan asked again is units of their own whatever they hold
    /// ([`Jobs::planned`]), as before 2026-11-11.
    pub(super) static PLANT_PLAN_ANEW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The planted bug: a job's answer waits for no placement, named or not
    /// ([`Jobs::wind_up`]).
    pub(super) static PLANT_ANSWER_WAITS_FOR_NO_OUTPUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use blaze_api::{
    ApiError, Artifact, ArtifactKind, Behind, Diagnostic, Edit, FetchVerb, Level, LinkKind,
    Mismatch, OpenFailure, Response, ServedCrate, ServedPart, Targets, TestOutcome, TestResult,
    Traced, Ub, Unbuilt, Unchecked, UnitOf, UnitStarted, Verb, Worked,
    build::{Builder, JobId, Opened, Work},
};
use blaze_cas::{Placed, Store};
use blaze_core::{Completion, Done, Effect, FileName, Meta, Tag, lock::Lock};
use blaze_graph::{
    Key, Outcome, Pool, RequestId, RequestStats, admission::TaskKind, machine::NodeId,
    plane::NodeRef,
};
use blaze_hash::{HashMap, HashSet};
use blaze_history::History;
use blaze_ids::{ContentId, Hasher, bits::BitSet};
use blaze_profile::{
    facts::{CrateDepRow, DepKind, LinkOutcome, LinkRow, ProvenanceRow, TestRunRow},
    history::{self as profile_history, BuildRow},
};
use blaze_protocol::events::NodeKind;
use blaze_query::{Database, QueryKey};
use blaze_units::{
    query::{
        BuildConfig, Command, Fetch, FetchKind, Invocation, LockWrite, Mount, Origin, Planned,
        Units,
    },
    schema::Mode as UnitMode,
};
use blaze_vfs::{Change, Ticket, Vfs, ViewId};
use crossbeam_queue::SegQueue;
use std::alloc::Global;

use blaze_mem::{Handle, MemError, Tag as MemTag};

use self::noop::{
    Answers, Asked as NoopAsked, Edited, FileEdit, Inputs, Last, Recording, StillFile, Unkept,
    Vouches, Walk,
};
use self::slab::{Table, Tagged};
use super::{
    heads::Heads,
    image::{Images, Parts},
    mounts::Mounts,
    observe::Ended,
    plane::{self, Did, Fresh, Happened, Plane, Posting, Task, kinds, loops},
    work::{
        Broke, Compiled, Failed, Front, FrontEnd, Image, JobInput, JobKind, Kept, KeptDb, Library,
        Linking, LocalRead, Made, Phase, Readers, RunPuts, Runs, Selection, Source, Steps, Writer,
    },
};
use crate::cli::print::Format;

/// The daemon's plane: every batch of it a job's [`Step`].
type Tasks<S> = Plane<Step<S>, History>;

/// How many tags one run of user code uses ([`Runs`]).
const RUN_TAGS: u64 = 1 << 32;
/// A run's number is its slot in [`Jobs::runs`], its generation's low bits above, as many as
/// the range of run tags leaves: at most `2^16` runs go at once.
const RUN_SLOT_BITS: u32 = 16;
/// A tag of the loop's own is `setup.tags + (slot | generation << 20)`, under the range's
/// `2^32`: at most `2^20` such effects in flight at once, and a slot reused `2^11` times
/// before a tag of its names a value again ([`Tagged`]).
const OWN_SLOT_BITS: u32 = 20;
const OWN_GENERATION_BITS: u32 = 12;

/// What a thread task of a job hands the loop.
pub enum Value<S: Steps> {
    Library(Result<Library, ApiError>),
    /// The fetch at its place in its batch ([`Fetching::fetches`]).
    Fetched(Arc<[Fetch]>, u32, Result<Source, ApiError>),
    /// The [`FetchKind::Local`] at its place in its batch, read where it is on the disk.
    Local(Arc<[Fetch]>, u32, Result<LocalRead, ApiError>),
    /// The local mounts read again before a view is opened ([`Stage::Locals`]).
    Locals(Result<Vec<LocalRead>, ApiError>),
    Planned(Result<Planned, ApiError>),
    /// The pinned native input of digest [`native_key`] laid out ([`Steps::native`]): its
    /// directory.
    Native(Key, Result<std::path::PathBuf, ApiError>),
    Lowered(Result<FrontEnd<S>, ApiError>),
    /// A unit's compile, and the images it relinked, given back to the loop: by its last
    /// phase, or by the one that failed.
    Compiled(usize, Result<Compiled, ApiError>, Vec<Image<S>>),
    /// A phase of a unit's compile before its last, done, and the images it relinked: its
    /// link's, which the loop keeps at once.
    Phase(usize, Vec<Image<S>>),
    Answer(Result<Response, ApiError>),
    /// Answered with these, in order: a [`Work::Doc`]'s pages, warnings and documentation.
    Answers(Result<Vec<Response>, ApiError>),
    /// `blaze test --ub`: the tests selected, and one's result.
    Selected(Result<Vec<String>, ApiError>),
    Evaluated(Result<TestResult, ApiError>),
    /// [`Work::Ahead`]: the units to build ahead ([`Front::ahead`]).
    Ahead(Result<Vec<u32>, ApiError>),
    /// What a kept front end left for the store ([`Steps::store_kept`]), and the front end
    /// given back: none when its store panicked, as it may be half written. With its build's
    /// run log as the history's rows ([`Steps::kept_run_log`]), named by the task.
    StoredKept(
        Option<S::KeptFront>,
        Result<Vec<super::work::KeptPut>, ApiError>,
        Vec<ProvenanceRow>,
    ),
    /// A kept front end given back by the task that swept its values for memory
    /// ([`Does::GiveValues`]), with the bytes that went: none when the sweep panicked, as it
    /// may be half swept.
    GaveValues(Option<S::KeptFront>, u64),
}

/// What jobs tell the session, in order.
#[derive(Debug, PartialEq)]
pub enum Said {
    Report(JobId, Response),
    /// The job over, ok or not, and the database view its build ran in, handed to the session
    /// to keep or close ([`blaze_api::session::Session::finish_in`]), with the root that view
    /// is of: the one the job built, which is not the one it was asked on where its plan's
    /// lock files were written into its view ([`Jobs::lock_into_view`]). None when the job
    /// opened none.
    Finish(JobId, bool, Option<ApiError>, Option<(ViewId, ContentId)>),
}

/// What the jobs work with, lent by the loop for one call.
pub struct World<'a, S: Steps> {
    pub now: Duration,
    pub vfs: &'a mut Vfs,
    pub db: &'a Arc<Database>,
    pub store: &'a mut Store,
    /// The daemon's proc macros, whose expansions are kept in `store`.
    pub macros: &'a Arc<blaze_macro_eval::Macros>,
    /// The standard library's store, shared by this build's daemons ([`super::Options::std`]).
    pub std: &'a mut Store,
    pub plane: &'a mut Tasks<S>,
    pub mounts: &'a mut Mounts,
    pub heads: &'a Heads,
    /// The daemon's image, which a snapshot asks for and waits on.
    pub images: &'a mut Images,
    pub steps: &'a Arc<S>,
    pub effects: &'a mut Vec<Effect>,
    pub said: &'a mut Vec<Said>,
}

/// What the jobs need of the daemon besides its parts.
pub struct Setup {
    /// Where an agent's build of a root no checkout has is placed: `<state>/views`.
    pub views: String,
    /// Where the standard library's builds are placed, every workspace's and every daemon's of
    /// this blaze build: [`super::Options::std`].
    pub std: String,
    /// Where a workspace's build directory is when nothing names one: [`super::Options::builds`].
    pub builds: String,
    /// Where the packages it fetches are kept: [`super::Options::sources`].
    pub sources: String,
    /// The daemon's store, where a fetched `.crate` is kept and looked up first
    /// (`<state>/store`): what an error naming the packages it could not fetch says it looked in.
    pub store: String,
    /// The triple of the machine the daemon runs on.
    pub host: String,
    /// The daemon's `PATH` and `HOME`: an agent's invocation's whole environment.
    pub env: Vec<(String, String)>,
    /// Whether an agent's diagnostics are rendered in colour; a cargo command line's are as
    /// its client resolved `--color` against its terminal ([`blaze_api::Cargo::color`]).
    pub color: bool,
    /// The lock writes, modes and looks at files: `tags..tags + 2^32`.
    pub tags: u64,
    /// User code's runs take ranges of `2^32` from `runs` up to `runs_end`.
    pub runs: u64,
    pub runs_end: u64,
}

/// Every fetch of a batch that failed (`failed`, places in `fetches`), as one error: each
/// package, index file or git tree by name and version and where it was to come from, with why
/// it did not (`errors`, by where it mounts), then where blaze looked first (its store `store`
/// and its sources `sources`) and, in a blaze built without `net`, how to get them: one run of a
/// networked blaze with the same `BLAZE_HOME`. One failure in a networked blaze is its own error,
/// as cargo says it.
fn unavailable(
    fetches: &[Fetch],
    failed: &[u32],
    errors: &HashMap<String, ApiError, Global>,
    sources: &str,
    store: &str,
) -> ApiError {
    let net = blaze_registry::http::NET;
    // alloc: an error, once per batch that failed
    let mut places: Vec<usize> = failed.iter().map(|&it| it as usize).collect();
    places.sort_unstable();
    places.dedup();
    if let [only] = places[..]
        && net
        && let Some(error) = errors.get(&fetches[only].at)
    {
        return error.clone(); // clone: the error, once
    }
    // alloc: as above
    let mut what = format!(
        "{} of the {} sources this build needs could not be fetched, so nothing was compiled:",
        places.len(),
        fetches.len()
    );
    for &at in &places {
        let fetch = &fetches[at];
        let kind = match &fetch.kind {
            FetchKind::Crate { .. } => "package",
            FetchKind::IndexFile => "index file",
            FetchKind::Git { .. } => "git tree",
            FetchKind::Local { .. } => "path on disk",
        };
        what.push_str(&format!("\n  - {} ({kind}) from {}", fetch.what, fetch.url));
        // Without `net` each says the same (no HTTP client), said once below instead.
        if net && let Some(error) = errors.get(&fetch.at) {
            for line in error.what.lines().filter(|it| !it.trim().is_empty()) {
                what.push_str("\n      ");
                what.push_str(line);
            }
        }
    }
    what.push_str(&format!(
        "\nblaze looked for each first in its store ({store}) and its sources ({sources})"
    ));
    if !net {
        what.push_str(
            ", which hold none of them, and this blaze is built without blaze-registry's `net` feature (the seed \
             profile), so it fetches nothing: run the same command once with a networked blaze and the same \
             BLAZE_HOME, which fetches them into that store and those sources, and this blaze then builds offline",
        );
    }
    ApiError::new(blaze_fetch::scrub(&what), Some(sources.to_owned()))
}

/// The single-flight key of the standard library's source.
fn library_key() -> Key {
    Key(ContentId::of("blaze v2 serve library", b"library").0)
}

/// The single-flight key of laying out the archive of digest `sha256`: every job asking it,
/// whatever its inputs are named, shares one layout ([`Jobs::native_laid`] holds its result,
/// the directory, which depends on the digest alone).
fn native_key(sha256: &str) -> Key {
    let mut hasher = Hasher::new("blaze v2 serve native 2");
    hasher.part(sha256.as_bytes());
    Key(hasher.finish().0)
}

/// The single-flight key of a fetch: every job asking the same source shares one download.
fn fetch_key(fetch: &Fetch) -> Key {
    let mut hasher = Hasher::new("blaze v2 serve fetch");
    hasher.part(fetch.at.as_bytes()).part(fetch.url.as_bytes());
    match &fetch.kind {
        FetchKind::Crate { sha256 } => hasher.part(b"crate").part(sha256.as_bytes()),
        FetchKind::IndexFile => hasher.part(b"index"),
        FetchKind::Git { commit } => hasher.part(b"git").part(commit.as_bytes()),
        FetchKind::Local { probe } => hasher.part(b"local").part(&[u8::from(*probe)]),
    };
    Key(hasher.finish().0)
}

/// The thread tasks of one batch of a job's nodes: what they do and what they own, shared by
/// the batch's tasks. Every batch of the daemon's plane is one, run by one `match`.
pub struct Step<S: Steps> {
    cancelled: Arc<AtomicBool>,
    does: Does<S>,
}

/// What a batch's tasks do, each with what it owns.
pub(super) enum Does<S: Steps> {
    Library { steps: Arc<S>, kept: Vec<(String, String)> },
    Plan { steps: Arc<S>, db: Arc<Database>, queries: ViewId },
    /// Task `at` fetches `fetches[at]`, or reads it where it is on the disk.
    Fetch { steps: Arc<S>, fetches: Arc<[Fetch]> },
    /// The local mounts at `ats` read again, what is under them maybe changed.
    Locals { steps: Arc<S>, ats: Vec<String> },
    /// The units' pinned native inputs laid out, before the lowering ([`Steps::native`]).
    /// One digest's layout, for every input of it: `input` is one of them.
    Native {
        steps: Arc<S>,
        input: blaze_units::query::NativeInput,
        key: Key,
    },
    /// The head's last front end, taken by the run that lowers into it.
    /// Its build dirs leased first ([`Steps::lease`]) into `leases`, against `checkout`,
    /// `builds` the dir of every workspace's build dir.
    Lower {
        steps: Arc<S>,
        input: JobInput,
        kept: Lock<Option<S::KeptFront>>,
        leases: Arc<Lock<Leases>>,
        checkout: String,
        builds: String,
    },
    /// Task `at` compiles unit `at`. A compile that gives way puts the images it was given in
    /// `back` ([`Paths::back`]), so its next try still relinks them.
    Compile {
        front: Arc<S::Front>,
        back: Arc<SegQueue<Image<S>>>,
    },
    Select {
        front: Arc<S::Front>,
        filter: Vec<String>,
        args: Vec<String>,
        all: bool,
        ub: Ub,
    },
    /// Task `at` evaluates `tests[at]`.
    Evaluate {
        front: Arc<S::Front>,
        tests: Arc<[String]>,
        ub: Ub,
    },
    Reach {
        to: Arc<S::Front>,
        from: Arc<S::Front>,
        changes: Vec<Change>,
    },
    CodegenInputs {
        front: Arc<S::Front>,
    },
    /// With the linker's tables of the images kept as it was posted.
    Tables {
        front: Arc<S::Front>,
        images: Vec<(&'static str, u64, u64)>,
    },
    Trace {
        db: Arc<Database>,
        queries: ViewId,
        root: QueryKey,
    },
    Verify {
        steps: Arc<S>,
        input: JobInput,
        view: blaze_vfs::View,
    },
    Edit {
        front: Arc<S::Front>,
        edits: Vec<Edit>,
    },
    Ahead {
        front: Arc<S::Front>,
        filter: Vec<String>,
    },
    /// Posted with the job's writer, once its check is built and nothing reads its front end:
    /// the documentation sets `cfg(doc)` on the crates it documents.
    Doc {
        front: Arc<S::Front>,
        no_deps: bool,
        private: bool,
    },
    /// A job's front end as it is kept, what its build left for the store written from it
    /// ([`Steps::store_kept`]); taken by the task and given back in its value. The task names
    /// the run log of build `build` first when `logs` ([`Steps::kept_run_log`]), and lets go
    /// last of what the build's end releases ([`Steps::release_kept`]), unless `wanted` is
    /// set by then: a request of the checkout waits for the front end.
    StoreKept {
        steps: Arc<S>,
        kept: Lock<Option<S::KeptFront>>,
        checkout: String,
        build: u64,
        logs: bool,
        wanted: Arc<AtomicBool>,
    },
    /// A kept front end gives back about `bytes` of the values it keeps, because memory is
    /// short ([`Jobs::give_values`]): taken by the task, swept in slices
    /// ([`giving::give_in_slices`]) and given back in its value. `wanted` is set once a build
    /// a client waits on is held for the front end: the task stops at its slice's end. The
    /// front end is shared with the loop so a task the plane refuses gives it back.
    GiveValues {
        kept: Arc<Lock<Option<S::KeptFront>>>,
        checkout: String,
        bytes: u64,
        wanted: Arc<AtomicBool>,
    },
}

impl<S: Steps> Step<S> {
    pub(super) fn of(cancelled: &Arc<AtomicBool>, does: Does<S>) -> Step<S> {
        Step { cancelled: cancelled.clone(), does } // clone: a refcount, once per batch
    }

    /// How a task's result settles its node, and its value for the loop. A give-way posts it
    /// again, unless the job is cancelled.
    fn settle<R>(
        &self,
        result: Result<R, Failed>,
        value: impl FnOnce(Result<R, ApiError>) -> Value<S>,
    ) -> Option<Did<Value<S>>> {
        match result {
            Ok(it) => Some(Did { outcome: Outcome::Built, value: value(Ok(it)) }),
            Err(Failed::Error(error)) => {
                // clone: a failed task's error, for the plane and the loop
                Some(Did {
                    outcome: Outcome::Failed { why: error.what.clone() },
                    value: value(Err(error)),
                })
            }
            Err(Failed::Again) if self.cancelled.load(Ordering::Acquire) => Some(Did {
                outcome: Outcome::Failed { why: "cancelled".to_owned() }, // alloc: a cancelled task, once
                value: value(Err(ApiError::new("cancelled", None))),
            }),
            Err(Failed::Again) => None,
        }
    }
}

impl<S: Steps> plane::Work for Step<S> {
    type Value = Value<S>;
    type Readers = Readers<S>;

    fn subject(&self, arg: u32) -> Option<(String, String)> {
        match &self.does {
            Does::Compile { front, .. } => front.subject(Phase::of(arg).0),
            _ => None,
        }
    }

    fn run(&self, arg: u32, readers: Readers<S>) -> Option<Did<Value<S>>> {
        let at = arg as usize;
        let _span = match &self.does {
            Does::Library { .. } => tracing::info_span!("std library"),
            Does::Plan { .. } => tracing::info_span!("plan"),
            Does::Fetch { .. } => tracing::info_span!("fetch"),
            Does::Locals { ats, .. } => tracing::info_span!(target: "blaze", "local mounts", count = ats.len()),
            Does::Native { input, .. } => tracing::info_span!(target: "blaze", "pinned native input", input = %input.name, sha256 = %input.sha256),
            Does::Lower { .. } => tracing::info_span!("lower workspace"),
            // Its phases have spans of their own (`unit::compile::compile`).
            Does::Compile { .. } => tracing::Span::none(),
            Does::Select { .. } => tracing::info_span!("select tests"),
            Does::Evaluate { .. } => tracing::info_span!("evaluate"),
            Does::Reach { .. } => tracing::info_span!("reach"),
            Does::CodegenInputs { .. } => tracing::info_span!("codegen inputs"),
            Does::Tables { .. } => tracing::info_span!("tables"),
            Does::Trace { .. } => tracing::info_span!("trace"),
            Does::Verify { .. } => tracing::info_span!("verify"),
            Does::Edit { .. } => tracing::info_span!("edit"),
            Does::Ahead { .. } => tracing::info_span!("ahead"),
            Does::Doc { .. } => tracing::info_span!("doc"),
            Does::StoreKept { checkout, .. } => tracing::info_span!(target: "blaze", "store kept front end", checkout = %checkout),
            Does::GiveValues { checkout, bytes, .. } => tracing::info_span!(target: "blaze", "give back a kept front end's values", checkout = %checkout, bytes = *bytes),
        }
        .entered();
        match &self.does {
            Does::Library { steps, kept } => self.settle(steps.library(kept).map_err(Failed::Error), Value::Library),
            Does::Plan { steps, db, queries } => self.settle(steps.plan(db, *queries), Value::Planned),
            Does::Fetch { steps, fetches } => match &fetches[at].kind {
                FetchKind::Local { .. } => {
                    let read = steps.local(&fetches[at]).map_err(Failed::Error);
                    // clone: a refcount per path read
                    self.settle(read, |it| Value::Local(fetches.clone(), arg, it))
                }
                _ => {
                    let fetched = steps.fetch(&fetches[at]).map_err(Failed::Error);
                    // clone: a refcount per fetch
                    self.settle(fetched, |it| Value::Fetched(fetches.clone(), arg, it))
                }
            },
            Does::Locals { steps, ats } => self.settle(steps.locals(ats).map_err(Failed::Error), Value::Locals),
            Does::Native { steps, input, key } => {
                self.settle(steps.native(input, &self.cancelled).map_err(Failed::Error), |it| Value::Native(*key, it))
            }
            Does::Lower { steps, input, kept, leases, checkout, builds } => {
                // Its build dirs' leases, before anything is built into them: here, on the pool.
                // The lock is held only to read and store the field, never across the lease's
                // file IO and syncs (review M1): one lowering a job, so nothing races between.
                let (unleased, first_try_ns) = {
                    let held = leases.lock();
                    (held.held.is_empty(), held.first_try_ns)
                };
                if unleased {
                    match steps.lease(&input.units, checkout, builds) {
                        Ok(Ok(taken)) => {
                            let mut held = leases.lock();
                            if held.held.is_empty() {
                                held.held = taken;
                            }
                        }
                        Ok(Err((dir, wait))) => {
                            // Bounded by time since the first try, not by a count a hot repost
                            // spends in milliseconds (review N8).
                            let now = u64::try_from(blaze_profile::clock::mono_ns()).unwrap_or(0);
                            let first = first_try_ns.unwrap_or(now);
                            let waited = std::time::Duration::from_nanos(now.saturating_sub(first));
                            if waited < blaze_gc::lease::GIVE_WAY {
                                leases.lock().first_try_ns = Some(first);
                                return self.settle(Err(Failed::Again), Value::Lowered);
                            }
                            let why = format!(
                                "the build dir {dir} was not leased in {waited:?} of trying: {wait}"
                            );
                            let error = ApiError::new(why, Some(dir));
                            return self.settle(Err(Failed::Error(error)), Value::Lowered);
                        }
                        Err(why) => {
                            // clone: the error's own path, once
                            let error = ApiError::new(why, Some(input.units.target_dir.clone()));
                            return self.settle(Err(Failed::Error(error)), Value::Lowered);
                        }
                    }
                }
                let kept = kept.lock().take();
                self.settle(steps.lower(input, kept), Value::Lowered)
            }
            Does::Compile { front, back } => {
                let (unit, phase) = Phase::of(arg);
                let Readers { this, mut images, .. } = readers;
                let reader = this.expect("a task that reads is posted with its reader");
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    front.compile(reader, unit, phase, &mut images)
                }));
                let compiled = match caught {
                    Ok(it) => it,
                    Err(payload)
                        if payload.is::<blaze_query::db::Cancelled>()
                            || payload.is::<blaze_graph::Cancelled>() =>
                    {
                        Err(Failed::Again)
                    }
                    Err(payload) => {
                        let panic = plane::panicked(&*payload);
                        // The images it was relinking may be half written.
                        images.clear();
                        let refused =
                            front.refused(unit, format!("blaze {panic}, making this unit")); // alloc: a panic, once
                        Ok(Some(Compiled {
                            diagnostics: vec![refused], // alloc: a panic, once
                            ok: false,
                            open: false,
                            why: Some(panic.to_string()), // alloc: a panic, once
                            puts: Vec::new(),
                            share: Vec::new(),
                            outputs: Vec::new(),
                            artifact: None,
                            macros: None,
                            gpu_msl: None,
                            links: Vec::new(),
                            misses: Vec::new(),
                        }))
                    }
                };
                if matches!(compiled, Err(Failed::Again)) && !self.cancelled.load(Ordering::Acquire)
                {
                    for image in images {
                        back.push(image);
                    }
                    return None;
                }
                let compiled = match compiled {
                    Ok(None) => {
                        return Some(Did {
                            outcome: Outcome::Built,
                            value: Value::Phase(unit, images),
                        });
                    }
                    Ok(Some(compiled)) => Ok(compiled),
                    // An error of one unit's compile (a codegen error that is no refusal, a
                    // record that does not read, a toolchain it can't be made for) fails that
                    // unit, as a panic does, never the job: its siblings build under
                    // `--keep-going` and every other program of the job is told nothing of it
                    // (panics-fail-one-job).
                    Err(Failed::Error(error)) if !self.cancelled.load(Ordering::Acquire) => {
                        images.clear();
                        let what = match &error.at {
                            // alloc: an error, once
                            Some(at) => {
                                format!("blaze could not make this unit: {} (at {at})", error.what)
                            }
                            // alloc: an error, once
                            None => format!("blaze could not make this unit: {}", error.what),
                        };
                        Ok(Compiled {
                            diagnostics: vec![front.refused(unit, what.clone())], // clone: the node's why beside the unit's error, once
                            ok: false,
                            open: false,
                            why: Some(what),
                            puts: Vec::new(),
                            share: Vec::new(),
                            outputs: Vec::new(),
                            artifact: None,
                            macros: None,
                            gpu_msl: None,
                            links: Vec::new(),
                            misses: Vec::new(),
                        })
                    }
                    Err(failed) => Err(failed),
                };
                let (ok, open, why) = match &compiled {
                    // clone: a unit that failed, once
                    Ok(it) => (it.ok, it.open, it.why.clone()),
                    Err(_) => (false, false, None),
                };
                let mut did = self.settle(compiled, |it| Value::Compiled(unit, it, images))?;
                if !ok && matches!(did.outcome, Outcome::Built) {
                    let why = why.unwrap_or_else(|| "it did not compile".to_owned()); // alloc: a unit that failed, once
                    did.outcome = match open {
                        // Its items are sound: what is checked against them goes on under
                        // keep-going, and the unit stays failed.
                        true => Outcome::FailedOpen { why },
                        false => Outcome::Failed { why },
                    };
                }
                Some(did)
            }
            Does::Select { front, filter, args, all, ub } => {
                let selection = Selection { filter, args, all: *all, ub: Some(*ub) };
                self.settle(front.ub_tests(this(readers), selection), Value::Selected)
            }
            Does::Evaluate { front, tests, ub } => {
                self.settle(front.evaluate(this(readers), &tests[at], *ub), Value::Evaluated)
            }
            Does::Reach { to, from, changes } => {
                let from_reader = readers
                    .from
                    .expect("a reach is posted with a reader of the view it reaches from");
                let reached =
                    to.reach(this(Readers { from: None, ..readers }), from, from_reader, changes);
                self.settle(reached, |it| Value::Answer(it.map(Response::Reached)))
            }
            Does::CodegenInputs { front } => self
                .settle(front.codegen_inputs(this(readers)), |it| {
                    Value::Answer(it.map(Response::CodegenInputs))
                }),
            Does::Tables { front, images } => {
                let writer =
                    readers.writer.expect("the tables' task is posted with its job's writer");
                self.settle(front.tables(writer, images), |it| {
                    Value::Answer(it.map(Response::Tables))
                })
            }
            Does::Trace { db, queries, root } => self
                .settle(Ok(trace(db, *queries, *root)), |it| {
                    Value::Answer(it.map(Response::Trace))
                }),
            Does::Verify { steps, input, view } => {
                let verified = verify(&**steps, input, *view);
                self.settle(verified, |it| {
                    Value::Answer(it.map(|mismatches| Response::Verified { mismatches }))
                })
            }
            Does::Edit { front, edits } => self.settle(front.edit(this(readers), edits), |it| {
                Value::Answer(it.map(Response::Edited))
            }),
            Does::Ahead { front, filter } => {
                self.settle(front.ahead(this(readers), filter), Value::Ahead)
            }
            Does::Doc { front, no_deps, private } => {
                let writer = readers
                    .writer
                    .expect("the documentation's task is posted with its job's writer");
                self.settle(front.doc(writer, *no_deps, *private), Value::Answers)
            }
            Does::StoreKept { steps, kept, build, logs, wanted, .. } => {
                let Some(mut front) = kept.lock().take() else {
                    let error = ApiError::new(
                        "the kept front end to store was taken before its task ran",
                        None,
                    );
                    // alloc: none, an empty vec
                    return self.settle(Err(Failed::Error(error)), |it| {
                        Value::StoredKept(None, it, Vec::new())
                    });
                };
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // The build's run log first, as it was taken before the front end was
                    // kept: what the store's own reads run is not the build's.
                    let ran = match *logs {
                        true => {
                            let _span = tracing::info_span!(
                                target: "blaze",
                                "kept front end: name the build's run log",
                                build = *build,
                            )
                            .entered();
                            super::observe::provenance_rows(*build, steps.kept_run_log(&front))
                        }
                        false => Vec::new(), // alloc: none
                    };
                    let stored = {
                        let _span = tracing::info_span!(
                            target: "blaze",
                            "kept front end: store what the build left",
                        )
                        .entered();
                        steps.store_kept(&mut front)
                    };
                    // A store that failed drops the front end: nothing of it is let go first.
                    if stored.is_ok() {
                        let _span = tracing::info_span!(
                            target: "blaze",
                            "kept front end: release what the build's end lets go",
                            wanted = wanted.load(Ordering::Acquire),
                        )
                        .entered();
                        steps.release_kept(&mut front, wanted);
                    }
                    (ran, stored)
                }));
                let (front, stored, ran) = match caught {
                    Ok((ran, stored)) => (Some(front), stored, ran),
                    Err(payload) => {
                        let panic = plane::panicked(&*payload);
                        drop(front);
                        // alloc: a panic, once
                        let what = format!(
                            "blaze {panic}, storing what the build left for the store; its front end is dropped, as it may be half written"
                        );
                        (None, Err(ApiError::new(what, None)), Vec::new()) // alloc: none
                    }
                };
                self.settle(stored.map_err(Failed::Error), |it| Value::StoredKept(front, it, ran))
            }
            Does::GiveValues { kept, bytes, wanted, .. } => {
                let Some(mut front) = kept.lock().take() else {
                    let error = ApiError::new(
                        "the kept front end to give values back from was taken before its task ran",
                        None,
                    );
                    return self
                        .settle(Err::<(), _>(Failed::Error(error)), |_| Value::GaveValues(None, 0));
                };
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    giving::give_in_slices(&mut front, *bytes, wanted)
                }));
                let (front, gave) = match caught {
                    Ok(gave) => (Some(front), gave),
                    Err(payload) => {
                        // Half swept, it is dropped as a store's is: the loop says the loss.
                        let panic = plane::panicked(&*payload);
                        tracing::error!(target: "blaze", "blaze {panic}, giving back a kept front end's values; its front end is dropped");
                        drop(front);
                        (None, 0)
                    }
                };
                self.settle(Ok::<(), Failed>(()), |_| Value::GaveValues(front, gave))
            }
        }
    }
}

/// The reader of a task's own front end, which a task of a batch that reads is posted with.
fn this<S: Steps>(readers: Readers<S>) -> S::Reader {
    readers.this.expect("a task that reads is posted with its reader")
}

/// One thread task of kind `kind`, the batch's only one.
fn one(kind: TaskKind) -> Task {
    Task::Thread { kind, arg: 0 }
}

/// A node's name, `a` then `b`, in one allocation of its length.
fn named(a: &str, b: &str) -> String {
    let mut name = String::with_capacity(a.len() + b.len());
    name.push_str(a);
    name.push_str(b);
    name
}

/// A node whose cost and memory the plane predicts as it is pushed: by its own key's last runs
/// where it is learned by one (`Plane::push_learned`, its memory never under its kind's peak;
/// `push_program`; or the plane's key), else by its kind's mean; the 1 stands only where the
/// predictor has neither.
fn node<'a>(
    kind: NodeKind,
    name: &'a str,
    deps: &'a [NodeId],
    pool: Pool,
    key: Option<Key>,
) -> NodeRef<'a> {
    NodeRef { kind, name, deps, pool, cost: 1, key, memory: 0 }
}

/// Why a build of `checkout` that keeps its front end is refused before it lowers anything:
/// the task storing what it leaves for the store ([`Jobs::store_kept`]) last held, for this
/// very checkout ([`store_cost_key`]'s record), more than the budget, so the plane would refuse
/// it (`too_big`) only once the whole build is done (46.47 GB of a 42 GB budget, ten minutes
/// in, on 2026-11-03).
///
/// Only this checkout's own record refuses: a checkout never measured, or one whose record
/// fits, goes on, whatever another tree's stores held (the kind's peak). The refusal is the
/// late one's verdict, minutes sooner: the store's node is predicted at least this record
/// ([`lone_learned`] floors it at its kind's peak beside), and the record cannot come down
/// while it is over the budget, as only a store of this checkout teaches it and none is
/// admitted (earlyfail). The error says the exact `BLAZE_DAEMON_MEMORY_GB` that fits it.
fn stores_too_much<S: Steps>(plane: &Tasks<S>, checkout: &str) -> Option<ApiError> {
    let budget = plane.budget()?;
    let need = plane.predict().record(kinds::STORE_KEPT, &store_cost_key(checkout))?.peak;
    (need > budget.bytes).then(|| {
        let gb = |bytes: u64| format!("{:.2} GB", bytes as f64 / f64::from(1 << 30));
        // alloc: an error, once per build refused
        let what = format!(
            "the build of {checkout} would need {} of memory to store what it leaves for the store \
             (as its last store of its kept front end held), more than blaze may use \
             here: {} ({}), so nothing was built; {}",
            gb(need),
            gb(budget.bytes),
            budget.from,
            budget.advice(need)
        );
        ApiError::new(what, Some(checkout.to_owned()))
    })
}

/// A batch of one thread task of kind `task`, with no deps.
fn lone<S: Steps>(
    plane: &mut Tasks<S>,
    kind: NodeKind,
    name: &str,
    pool: Pool,
    key: Option<Key>,
    task: TaskKind,
) -> Fresh {
    let mut fresh = plane.fresh(1, name.len(), 0);
    plane.push(&mut fresh, node(kind, name, &[], pool, key), one(task));
    fresh
}

/// [`lone`], its node with no key of the plane's, predicted and taught by `learn`.
fn lone_learned<S: Steps>(
    plane: &mut Tasks<S>,
    kind: NodeKind,
    name: &str,
    pool: Pool,
    task: TaskKind,
    learn: Key,
) -> Fresh {
    let mut fresh = plane.fresh(1, name.len(), 0);
    plane.push_learned(&mut fresh, node(kind, name, &[], pool, None), one(task), learn);
    fresh
}

/// The key a checkout's lowering is predicted and taught by: the same checkout lowers alike
/// from one build to the next.
fn lower_cost_key(checkout: &str) -> Key {
    let mut hasher = blaze_ids::Hasher::new("blaze v2 lowering cost 1");
    hasher.update(checkout.as_bytes());
    Key(hasher.finish().0)
}

/// The predictor's key of the store of `checkout`'s kept front end ([`Jobs::store_kept`]):
/// what it held is learned per checkout, as its lowering is ([`lower_cost_key`]), so a large
/// tree's store refuses no other's early ([`stores_too_much`]).
fn store_cost_key(checkout: &str) -> Key {
    let mut hasher = blaze_ids::Hasher::new("blaze v2 kept front end store cost 1");
    hasher.update(checkout.as_bytes());
    Key(hasher.finish().0)
}

/// The units of `units` whose results a front end applies before it reads as rustc would
/// compile: each proc macro's compile, whose expanders it is given (`Lowered::macros`), and
/// each build script's run, whose cfgs, env and `OUT_DIR` files it is given (`Lowered::ran`).
/// A build of them builds what they depend on, and nothing that depends on them.
fn applied_units(units: &[blaze_units::schema::Unit]) -> Vec<u32> {
    let applied = |unit: &blaze_units::schema::Unit| {
        unit.mode == UnitMode::RunCustomBuild || unit.gives_expanders()
    };
    // alloc: once per view of a reach, a place per proc macro or build script
    (0u32..).zip(units).filter(|(_, unit)| applied(unit)).map(|(at, _)| at).collect()
}

/// Writes into `name` (cleared first) a unit's plane-node name, the one every phase of it
/// carries and the build view shows: the package's name and version, the target as a person
/// says it, and what it is built for, `object 0.36.7 build-script (host)`,
/// `serde_derive 1.0.219 proc-macro (macro)`, `blaze-ui 0.1.0 lib test (aarch64-unknown-linux-gnu)`.
/// A package the plan does not list is named by its id.
fn node_name(
    name: &mut String,
    unit: &blaze_units::schema::Unit,
    packages: &HashMap<&str, (&str, &str), Global>,
) {
    use std::fmt::Write as _;
    name.clear();
    match packages.get(unit.pkg_id.as_str()) {
        Some((package, version)) => _ = write!(name, "{package} {version}"),
        None => name.push_str(&unit.pkg_id),
    }
    let target = &unit.target;
    if target.is_custom_build() {
        name.push_str(" build-script");
    } else if target.is_proc_macro() {
        name.push_str(" proc-macro");
    } else if target.is_lib() {
        name.push_str(" lib");
    } else {
        let kind = target.kind.first().map_or("bin", String::as_str);
        _ = write!(name, " {kind} {}", target.name);
    }
    let tested = unit.mode == UnitMode::Test || unit.checked_as_test;
    if tested && !target.kind.iter().any(|it| it == "test" || it == "bench") {
        name.push_str(" test");
    }
    match (&unit.platform, unit.macro_wasm) {
        (_, true) => name.push_str(" (macro)"),
        (Some(triple), false) => _ = write!(name, " ({triple})"),
        (None, false) => name.push_str(" (host)"),
    }
}

/// The proc macros among the units `picked` of `units`, each `name version`, sorted, for the
/// span of a reach that waits on them.
fn macro_names(units: &Units, picked: &[u32]) -> String {
    // alloc: once per view of a reach, for its span
    let mut names: Vec<String> = picked
        .iter()
        .filter_map(|at| units.graph.units.get(*at as usize))
        .filter(|unit| unit.target.is_proc_macro())
        .map(|unit| match units.packages.iter().find(|it| it.id == unit.pkg_id) {
            Some(package) => format!("{} {}", package.name, package.version),
            None => unit.pkg_id.clone(), // clone: a span's name of a package the plan doesn't list
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names.join(", ")
}

/// A job's place in [`Jobs::jobs`].
fn key_of(id: JobId) -> slab::Key {
    slab::Key { slot: id.slot, generation: id.generation }
}

/// What a job's work asks of cargo: its command.
fn command_of(work: &Work) -> Command {
    match work {
        Work::Build(_) | Work::CodegenInputs(_) => Command::Build,
        Work::Test { .. } | Work::Ahead { .. } => Command::Test,
        Work::Run { .. } => Command::Run,
        Work::Check(_)
        | Work::Reach { .. }
        | Work::Verify(_)
        | Work::Trace(_)
        | Work::Tables
        | Work::Snapshot
        | Work::Edit(_)
        | Work::Lock(_) => Command::Check,
        Work::Doc { .. } => Command::Doc,
    }
}

fn kind_of(work: &Work) -> JobKind {
    match work {
        Work::Build(_) | Work::CodegenInputs(_) => JobKind::Build,
        Work::Test { ub: Some(_), .. } => JobKind::Check,
        Work::Test { .. } | Work::Ahead { .. } => JobKind::Test,
        Work::Run { .. } => JobKind::Run,
        // A reach builds only the units its views' front ends apply, each as a check builds
        // it, and so shares a check's records ([`reads_front_unbuilt`]).
        Work::Reach { .. }
        | Work::Check(_)
        | Work::Verify(_)
        | Work::Trace(_)
        | Work::Tables
        | Work::Snapshot
        | Work::Edit(_)
        | Work::Doc { .. }
        | Work::Lock(_) => JobKind::Check,
    }
}

/// Whether `work` answers from its views' front ends without building their units first: a
/// reach (item hashes), an edit (name resolution) and a build ahead's choice (the tests each
/// test unit holds) read def maps, which a crate whose proc macros are not built, or whose
/// build scripts' cfgs, env and `OUT_DIR` are not applied, gives only half expanded. Each view
/// of such a job, once lowered, first builds the units its front end applies
/// ([`applied_units`]) in the job's own plane request, and only then is read ([`Jobs::next`]):
/// the one place that order is made, never a read of what a pending macro left out
/// (no-fallback). Every other job builds every unit in order before it reads (a check, a
/// build, a test, a run, the documentation) or reads no def map of the view (a trace
/// and a verify walk the plan, the tables count, the codegen inputs read the units' records).
fn reads_front_unbuilt(work: &Work) -> bool {
    match work {
        Work::Reach { .. } | Work::Edit(_) | Work::Ahead { .. } => true,
        Work::Check(_)
        | Work::Build(_)
        | Work::Test { .. }
        | Work::Run { .. }
        | Work::Verify(_)
        | Work::Snapshot
        | Work::CodegenInputs(_)
        | Work::Trace(_)
        | Work::Tables
        | Work::Doc { .. }
        | Work::Lock(_) => false,
    }
}

/// The word naming what a job was asked, as the history and its spans name it.
fn work_word(work: &Work) -> &'static str {
    match work {
        Work::Check(_) => "check",
        Work::Build(_) => "build",
        Work::Test { .. } => "test",
        Work::Run { .. } => "run",
        Work::Reach { .. } => "reach",
        Work::Verify(_) => "verify",
        Work::Snapshot => "snapshot",
        Work::CodegenInputs(_) => "codegen-inputs",
        Work::Trace(_) => "trace",
        Work::Tables => "tables",
        Work::Doc { .. } => "doc",
        Work::Edit(_) => "edit",
        Work::Ahead { .. } => "ahead",
        Work::Lock(_) => "lock",
    }
}

/// What a job was asked, as the history's `builds.command` says it: a command line's
/// subcommand and arguments, or the work an agent asked for.
fn command_line(work: &Work) -> String {
    if let Some(cargo) = targets_of(work).and_then(|it| it.cargo.as_ref()) {
        let mut line = cargo.command.clone(); // clone: once a build
        for arg in &cargo.args {
            line.push(' ');
            line.push_str(arg);
        }
        return line;
    }
    let mut line = work_word(work).to_owned();
    if let Some(targets) = targets_of(work) {
        for package in &targets.packages {
            line.push_str(" -p ");
            line.push_str(package);
        }
    }
    line
}

fn targets_of(work: &Work) -> Option<&Targets> {
    match work {
        Work::Check(targets)
        | Work::Build(targets)
        | Work::Verify(targets)
        | Work::CodegenInputs(targets)
        | Work::Trace(targets)
        | Work::Test { targets, .. }
        | Work::Run { targets, .. }
        | Work::Reach { targets, .. }
        | Work::Ahead { targets, .. }
        | Work::Doc { targets, .. }
        | Work::Lock(targets) => Some(targets),
        Work::Tables | Work::Snapshot | Work::Edit(_) => None,
    }
}

/// The daemon process's user and system CPU so far (getrusage): what a build's row subtracts
/// its start's from. `None` where the kernel says nothing of it.
fn daemon_cpu() -> Option<(Duration, Duration)> {
    #[cfg(unix)]
    {
        crate::raw::process::cpu_times(crate::raw::process::Whose::Itself)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// One word of cargo's command line: made once per job, at the edge.
fn word(args: &mut Vec<String>, it: &str) {
    args.push(it.to_owned()); // alloc: the invocation owns its words, once per job
}

/// cargo's command line for an agent's targets, run at `root` with only the daemon's `PATH`
/// and `HOME`.
pub fn invocation(
    work: &Work,
    targets: Option<&Targets>,
    root: &str,
    env: &[(String, String)],
) -> Invocation {
    if let Some(cargo) = targets.and_then(|it| it.cargo.as_ref()) {
        let command = Command::named(&cargo.command).unwrap_or(Command::Check);
        // clone: cargo's own invocation, owned by the config, once per job
        return Invocation {
            command,
            args: undrawn(&cargo.args), // alloc: once per job
            root: cargo.root.clone(),   // clone: once per job
            cwd: cargo.cwd.clone(),     // clone: once per job
            env: cargo.env.clone(),     // clone: once per job
            files: cargo.files.clone(), // clone: once per job
        };
    }
    let mut args = Vec::new(); // alloc: the invocation's words, once per job
    if let Some(targets) = targets {
        for package in &targets.packages {
            word(&mut args, "-p");
            word(&mut args, package);
        }
        if !targets.profile.is_empty() {
            word(&mut args, "--profile");
            word(&mut args, &targets.profile);
        }
        if let Some(target) = &targets.target {
            word(&mut args, "--target");
            word(&mut args, target);
        }
        if !targets.features.is_empty() {
            word(&mut args, "-F");
            args.push(targets.features.join(","));
        }
        if targets.all_features {
            word(&mut args, "--all-features");
        }
        if targets.no_default_features {
            word(&mut args, "--no-default-features");
        }
    }
    if let Work::Run { bin: Some(bin), .. } = work {
        word(&mut args, "--bin");
        word(&mut args, bin);
    }
    Invocation {
        command: command_of(work),
        args,
        root: root.to_owned(), // alloc: once per job
        cwd: root.to_owned(),  // alloc: once per job
        env: env.to_vec(),     // alloc: an agent's whole environment, once per job
        files: Vec::new(),     // alloc: none, it stays empty
    }
}

/// Whether a job that said the diagnostics of no unit in `said` says `diagnostic`: one of a
/// unit always; one of no unit is the job's, which every unit that meets it carries (the
/// report of a toolchain function the front end failed on, made the same by each, and kept in
/// each one's record), so only its first. Which unit's copy is said can vary with timing; what
/// is said, and that it comes before any unit's error that points at it, does not.
fn said_once(said: &mut HashSet<ContentId, Global>, diagnostic: &blaze_api::Diagnostic) -> bool {
    diagnostic.unit.is_some()
        || said.insert(ContentId::of(
            "blaze v2 serve job diagnostic said 1",
            diagnostic.rendered.as_bytes(),
        ))
}

/// The flags of cargo's command line that say only how its client is shown the build, each
/// with a value: no input of the plan or of anything built. The daemon reads how diagnostics
/// are drawn from the request itself ([`short`], [`blaze_api::Cargo::color`]).
const DRAWN_FLAGS: [&str; 2] = ["--message-format", "--color"];

/// cargo's `args` without [`DRAWN_FLAGS`] and their values, as a view's config holds them
/// ([`invocation`]): the config is what the checkout's plan is keyed by ([`Viewed::key`]) and
/// what its units are loaded from, so a build asked for with `--message-format short` or
/// `--color always` plans what one without them does, from the same plan. What follows `--`
/// is the program's or the test binary's, and is kept as it is.
pub(crate) fn undrawn(args: &[String]) -> Vec<String> {
    let mut kept = Vec::with_capacity(args.len()); // alloc: the config's own arguments, once per job
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            kept.push(arg.clone()); // clone: the config's own, once per job
            kept.extend(args.cloned());
            break;
        }
        if DRAWN_FLAGS.contains(&arg.as_str()) {
            // Its value is the next word.
            args.next();
            continue;
        }
        let valued = DRAWN_FLAGS
            .iter()
            .any(|flag| arg.strip_prefix(flag).is_some_and(|rest| rest.starts_with('=')));
        if !valued {
            kept.push(arg.clone()); // clone: the config's own, once per job
        }
    }
    kept
}

/// Whether cargo's `args` ask for diagnostics a line each (`--message-format=short` or
/// `json-diagnostic-short`), read as cargo reads each `--message-format`. Of the request's
/// own arguments: a view's config holds none of them ([`undrawn`]).
fn short(args: &[String]) -> Result<bool, String> {
    let mut values = Vec::new(); // alloc: none unless a `--message-format` is given
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if let Some(value) = arg.strip_prefix("--message-format=") {
            values.push(value.to_owned()); // alloc: once per `--message-format`, once per job
        } else if arg == "--message-format" {
            values.extend(args.next().cloned());
        }
    }
    Ok(matches!(
        crate::cli::print::format(&values)?,
        Format::Short | Format::Json { short: true, .. }
    ))
}

/// Whether `root_dir` is one later builds are of again, whose last plan and front end are
/// kept: a checkout's, not the directory of one agent's view (under `views`), which no other
/// view shares.
fn lasting(views: &str, root_dir: &str) -> bool {
    root_dir.strip_prefix(views).is_none_or(|rest| !rest.starts_with('/'))
}

/// Whether `work` builds from the head's last plan when it is of the same view and config. A
/// trace and a verify look at the plan's own run, so they ask it.
fn reuses_plans(work: &Work) -> bool {
    !matches!(work, Work::Trace(_) | Work::Verify(_))
}

/// A view being made ready: its config, its database view, and what its load asked.
struct Prep {
    opened: Opened,
    root_dir: String,
    /// The checkout `root_dir` is in [`Jobs::roots`]; none for an agent's view.
    root: Option<u32>,
    config: Arc<BuildConfig>,
    /// The view's root and its config as one: what [`LastPlan::key`] is matched by.
    key: ContentId,
    /// The request's shape ([`Viewed::shape`]).
    shape: ContentId,
    queries: ViewId,
    /// Every fetch's `at` this view's load asked for: asked again once mounted is a loop.
    asked: HashSet<String, Global>,
    /// The units of the head's last plan when it was of this key, asked nothing again.
    last: Option<(Arc<Units>, Option<LockWrite>)>,
}

/// An output placed, as the file was once placed: its size and when it last changed. The
/// driver's look at a file ([`Meta`]) has no inode or mode, so a file replaced by one of the
/// same size and time, or its mode changed, passes for the placed one.
struct Placement {
    key: blaze_cas::Key,
    executable: bool,
    meta: Option<Meta>,
    /// The file system's clock as the look `meta` is of was asked for: the look was no
    /// earlier, so a stamp it is steady by ([`certificate::RACY_NS`]) is steady.
    asked_ns: u64,
}

/// The directories `units`' outputs are in: under each of its build dirs and under `std`, where
/// the standard library's builds are placed ([`Setup::std`]: its units are units of the plan,
/// and their outputs are placed and looked at as any other's), the first directory of each
/// output's path (a profile's, a target's, or one build of the standard library), sorted, each
/// once. These, not the build dirs themselves, which every build writes its record into
/// (docs/gc.md). `Err` names the first output that is under none of them or right in one: no
/// such directory holds it.
fn output_dirs(units: &Units, std: &str) -> Result<Vec<std::path::PathBuf>, std::path::PathBuf> {
    let outputs = units.plans.iter().flat_map(|plan| plan.outputs.iter()).map(String::as_str);
    output_dirs_of(&[units.target_dir.as_str(), units.build_dir.as_str(), std], outputs)
}

/// [`output_dirs`] of `outputs`, each under one of `roots`.
pub(super) fn output_dirs_of<'a>(
    roots: &[&str],
    outputs: impl Iterator<Item = &'a str>,
) -> Result<Vec<std::path::PathBuf>, std::path::PathBuf> {
    use std::path::Path;
    let mut dirs = Vec::new(); // alloc: once a build that asks for a replay, a directory or two
    for output in outputs {
        let output = Path::new(output);
        let dir = roots.iter().map(Path::new).find_map(|root| {
            let mut parts = output.strip_prefix(root).ok()?.components();
            let first = parts.next()?;
            // Right in the root: only a watch of the root itself would hear of it.
            parts.next()?;
            Some(root.join(first))
        });
        // alloc: a reason, once a build whose answer is not kept
        dirs.push(dir.ok_or_else(|| output.to_owned())?);
    }
    dirs.sort_unstable();
    dirs.dedup();
    Ok(dirs)
}

/// The process's watches of directories outside every view ([`blaze_vfs::outside`]).
struct Watched;

impl Vouches for Watched {
    fn quiet_since(&self, generation: u64) -> bool {
        blaze_vfs::outside::quiet_since(generation)
    }

    fn unchanged_since(&self, dir: &std::path::Path, generation: u64) -> bool {
        blaze_vfs::outside::unchanged_since(dir, generation)
    }

    fn absent(&self, path: &std::path::Path) -> bool {
        matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    }

    fn only_since(&self, generation: u64, aside: &dyn Fn(&std::path::Path) -> bool) -> bool {
        blaze_vfs::outside::only_since(generation, |path| aside(path))
    }

    fn watched_since(&self, dir: &std::path::Path, generation: u64) -> bool {
        blaze_vfs::outside::watched_since(dir, generation)
    }

    fn same_file(&self, path: &std::path::Path, was: Option<[u8; 32]>) -> bool {
        use blaze_usercode::trace::Inputs as _;
        let mut disk = blaze_usercode::inputs::Disk::new(Vec::new()); // alloc: none, no variables
        disk.file(path).is_ok_and(|now| now == was)
    }
}

/// The still text (`blaze_lower::still_text`) of the file at `path` of the open view `view`,
/// parsed as a file of `edition`: none for a file that is not there, is not UTF-8 text, is
/// longer than the lane parses ([`noop::MOST_BYTES`]) or does not read. One parse, in a span.
fn still_in(vfs: &Vfs, view: ViewId, path: &str, edition: ::span::Edition) -> Option<ContentId> {
    let file = vfs.read(view, path).ok()??;
    if file.bytes.len() > noop::MOST_BYTES {
        return None;
    }
    let text = std::str::from_utf8(file.bytes).ok()?;
    let _span = tracing::info_span!(
        target: "blaze",
        "no-op answer: a changed file's still text",
        bytes = text.len(),
    )
    .entered();
    Some(blaze_lower::still_text(text, edition))
}

/// The hash of a request's work and its whole `Targets` ([`Inputs::request`]): what the
/// view's config does not hold of it. `None` for work the lane does not answer.
fn request_id(work: &Work) -> Option<ContentId> {
    let (word, targets) = match work {
        Work::Build(targets) => ("build", targets),
        Work::Check(targets) => ("check", targets),
        _ => return None,
    };
    // How its client is shown diagnostics is no part of it ([`drawn_id`]).
    // clone: the request's targets less how they are drawn, once a build or check
    let mut undrawn_targets = targets.clone();
    if let Some(cargo) = &mut undrawn_targets.cargo {
        (cargo.color, cargo.width, cargo.refused) = (false, None, None);
        cargo.args = undrawn(&cargo.args);
    }
    // alloc: the request's encoding, once a build or check
    let encoded = blaze_protocol::cbor::to_vec(&undrawn_targets)
        .expect("a request's targets are plain data, which CBOR encodes");
    Some(Hasher::new("blaze v2 serve no-op request").part(word.as_bytes()).part(&encoded).finish())
}

/// The hash of how `work`'s client is shown diagnostics ([`Inputs::drawn`]): whether in
/// colour, their width, how many refused functions are named, and each word of its command
/// line that says only that ([`DRAWN_FLAGS`]: `--message-format`, `--color`).
fn drawn_id(work: &Work) -> ContentId {
    let mut hasher = Hasher::new("blaze v2 serve no-op drawn");
    let Some(cargo) = targets_of(work).and_then(|it| it.cargo.as_ref()) else {
        return hasher.finish();
    };
    hasher.update(&[u8::from(cargo.color)]);
    hasher.update(&cargo.width.map_or(0, |it| u64::from(it) + 1).to_le_bytes());
    hasher.update(&cargo.refused.map_or(0, |it| u64::from(it) + 1).to_le_bytes());
    let kept = undrawn(&cargo.args);
    let mut kept = kept.iter().peekable();
    for arg in &cargo.args {
        // The words `undrawn` left out, in order: every word it kept is met in order too.
        match kept.peek() {
            Some(next) if *next == arg => {
                kept.next();
            }
            _ => {
                hasher.part(arg.as_bytes());
            }
        }
    }
    hasher.finish()
}

/// The hash of a view's config less its environment, by its encoding
/// ([`Inputs::config_id`]).
fn config_id(encoded: &[u8]) -> ContentId {
    Hasher::new("blaze v2 serve no-op config").part(encoded).finish()
}

/// A request's shape, of its config as the plan reads it, encoded ([`BuildConfig::planned`]):
/// the command, the arguments, the mounts and the plan's variables, whatever view it is asked
/// on and whatever else its environment holds. What a checkout keeps one plan
/// ([`keep_plan`]) and one no-op answer ([`noop::Answers`]) of.
fn shape_of(planned: &[u8]) -> ContentId {
    Hasher::new("blaze v2 serve request shape").part(planned).finish()
}

/// Whether a step of a build reads the variable `name` of its request's environment without
/// asking for it by name ([`noop::Asked::unasked`]): the plan's
/// ([`blaze_units::query::read_by_plan`]), the set a build script's walk of its environment
/// sees and every program it starts is given ([`blaze_usercode::confine::walk_sees`]: the C
/// toolchain's, the locale, `PATH`; every variable where the sandbox lends none), and blaze's
/// own, which its steps read off the request by name with no trace (`BLAZE_*`: the sandbox,
/// the store's checks, the profile). Every other variable reaches a step only by a read of
/// its name, which the step's record keeps and the walk's steps say
/// ([`Writer::env_named`]).
fn read_unasked(name: &str) -> bool {
    blaze_units::query::read_by_plan(name)
        || name.starts_with("BLAZE_")
        || blaze_usercode::confine::walk_sees(std::ffi::OsStr::new(name))
}

/// Whether the watch of `dir`, a path dependency's directory outside the view, vouches that
/// nothing under it changed since `generation` was read ([`Mounts::stale_locals`]).
fn vouched_local(dir: &str, generation: u64) -> bool {
    let dir = std::path::Path::new(dir);
    let vouched = blaze_vfs::outside::unchanged_since(dir, generation);
    if !vouched {
        // Said: a local no watch vouches for is read again by the job and keeps every
        // build from being answered again; the watch's own state says why (rule 19).
        tracing::info!(
            target: "blaze",
            dir = %dir.display(),
            why = %blaze_vfs::outside::why_changed(dir, generation),
            "a path outside the view that is mounted where it is has no watch vouching for it"
        );
    }
    vouched
}

/// The directory of a build's layout profiles under its target directory
/// (`unit::layout::DIR`, which every unit that links reads off the disk).
const LAYOUT: &str = "layout";

/// What a walk of `units` rests on that only a watch or a look can vouch for, found before
/// the walk looks at anything: the directories to ask the watches of (each one its outputs
/// are in, canonical, and its layout profiles' when there is one, watched from the next
/// round), and the paths that are not there and must stay so (its layout profiles' when
/// there is none: no watch is confirmed of a directory that is not there). Else why no
/// answer of such a walk is kept.
fn recorded_dirs(
    units: &Units,
    std: &str,
) -> Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>), Unkept> {
    use std::path::Path;
    // A profile to use is read off the disk by each build (`unit::codegen::read_profile`),
    // wherever its flag names it: no watch is asked of it.
    let profiled = |flag: &String| flag.contains("profile-use") || flag.contains("profile_use");
    if units.plans.iter().any(|plan| plan.rustflags.iter().any(profiled)) {
        return Err(Unkept::Uncounted("a `-C profile-use` profile"));
    }
    let mut dirs = output_dirs(units, std).map_err(Unkept::Outputs)?;
    // A watch hears nothing of a symlink above its directory being pointed elsewhere
    // ([`blaze_vfs::outside::unchanged_since`]): a directory named through one is not
    // vouched for. A resolve of each, once a build that walks.
    let _span = tracing::info_span!(
        target: "blaze",
        "no-op answer: resolve the output directories",
        dirs = dirs.len(),
    )
    .entered();
    let canonical = |dir: &Path| std::fs::canonicalize(dir).is_ok_and(|it| it == dir);
    if let Some(dir) = dirs.iter().find(|dir| !canonical(dir.as_path())) {
        // clone: a reason, once a build whose answer is not kept
        return Err(Unkept::Outputs(dir.clone()));
    }
    let layout = Path::new(&units.target_dir).join(LAYOUT);
    let mut absent = Vec::new(); // alloc: once a build that walks, a path
    match std::fs::symlink_metadata(&layout) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => absent.push(layout),
        Ok(meta) if meta.is_dir() && canonical(layout.as_path()) => {
            blaze_vfs::outside::want(&layout);
            dirs.push(layout);
        }
        _ => return Err(Unkept::Looked(Some(layout))),
    }
    Ok((dirs, absent))
}

/// A build under way that may become its checkout's kept answer ([`Jobs::record_answer`]).
struct Recorded {
    job: JobId,
    root: u32,
    /// The request's shape ([`Viewed::shape`]): which of its checkout's answers it may become.
    shape: ContentId,
    recording: Recording,
    /// The files of the view it walks, held open in the VFS from its start: kept beside its
    /// answer ([`WalkedView`]), or let go when none is kept.
    view: Option<ViewId>,
    /// Set as the job winds up: whether it made nothing, and its `Work`.
    walked: Option<(bool, Option<Worked>)>,
}

/// What the daemon holds beside a kept no-op answer for as long as it is kept
/// ([`noop::Answers`]).
struct WalkedView {
    /// The files of the view the answer's walk read, held open in the VFS: what a request on
    /// another root is read against ([`Jobs::edited`]), so the root and its files stay
    /// whatever the head moved to. None for a root the tree lacked.
    view: Option<ViewId>,
    /// The still text of each file of `view` the lane has read, by path: a file is parsed
    /// as it was once, however many requests it differs in.
    stills: Vec<(String, Option<ContentId>)>,
}

/// A view as a job is to build it ([`Jobs::viewed`]).
struct Viewed {
    /// Where the view's root is, absolute.
    root_dir: String,
    /// The checkout `root_dir` is in [`Jobs::roots`]; none for an agent's view.
    root: Option<u32>,
    /// The whole config, the request's every variable in it: the job's own, for what is
    /// keyed by its own reads of the environment.
    config: BuildConfig,
    /// The hash of `config` less its environment ([`config_id`]): what the no-op lane compares
    /// beside the variables its answer's walk read ([`noop::env_moved`]).
    bare: ContentId,
    /// `config` as the plan reads it ([`BuildConfig::planned`]): what the view is opened
    /// with.
    planned: Vec<u8>,
    /// The view's root and `planned` as one: what [`LastPlan::key`] is matched by.
    key: ContentId,
    /// The request's shape ([`shape_of`]): `planned` whatever the view.
    shape: ContentId,
}

/// The last plan of a head's view: a job of the same view and config reads its database
/// view and its units, the plan neither asked nor verified again.
struct LastPlan {
    key: ContentId,
    /// The request's shape ([`shape_of`]): its config as the plan reads it, whatever view it
    /// was planned on and whatever else the request's environment held. A checkout keeps one
    /// plan a shape ([`keep_plan`]).
    shape: ContentId,
    queries: ViewId,
    units: Arc<Units>,
    lock: Option<LockWrite>,
    /// When its view is one a build wrote its plan's lock files into
    /// ([`Jobs::lock_into_view`]): the key of the view that build was asked on, and the
    /// write. A request on that view again (the lock files removed, or a head no watcher
    /// moved) is this plan's once the same files are written into it, asked of no load: a
    /// plan is a function of its key, and so is what it writes.
    asked: Option<(ContentId, LockWrite)>,
    /// The outputs of `units`, as ids: made once a plan, and carried to the plan that
    /// replaces this one with the same units ([`Jobs::planned`]).
    outputs: Option<Arc<Outputs>>,
    /// The source bytes of each unit of `units` (`unit::source_bytes`), `u64::MAX` for one
    /// not sized yet: read once a plan, not once a build ([`Jobs::build`]). With `outputs`,
    /// kept here, a shape each, so a switch of request finds its own again: until
    /// 2026-11-12 a checkout kept them of one plan, and `build`, `test`, `build` sized
    /// every unit three times.
    sized: Option<Vec<u64>>,
}

/// The plan among `kept` whose units are `units` themselves (the same `Arc`): where what is
/// kept by the units' identity is ([`LastPlan::outputs`], [`LastPlan::sized`]).
fn plan_of<'p>(kept: &'p mut [LastPlan], units: &Arc<Units>) -> Option<&'p mut LastPlan> {
    kept.iter_mut().find(|it| Arc::ptr_eq(&it.units, units))
}

/// The most plans a checkout keeps ([`Roots::plans`]): the request shapes a person alternates
/// between (`build`, `test`, `check`, `run`, each in two profiles). A count, not a footprint:
/// a kept plan is its units, already shared with the job that planned them, and an open view
/// of the planning database.
const PLANS_KEPT: usize = 8;

// A checkout keeps a no-op answer for each shape it keeps a plan of.
const _: () = assert!(PLANS_KEPT == noop::MOST_SHAPES);

/// Keeps `plan` as its checkout's plan of its shape, the most recently used: the plan `kept`
/// had of that shape goes, and past [`PLANS_KEPT`] the ones least recently used go. Returns
/// those let go, whose views the caller retires. Never "the last one wins": `build`, `test`,
/// `build` finds the first `build`'s plan again.
fn keep_plan(kept: &mut Vec<LastPlan>, plan: LastPlan) -> Vec<LastPlan> {
    let mut gone = Vec::new(); // alloc: none unless a plan is let go
    if let Some(at) = kept.iter().position(|it| it.shape == plan.shape) {
        gone.push(kept.remove(at));
    }
    kept.push(plan);
    while kept.len() > PLANS_KEPT {
        gone.push(kept.remove(0));
    }
    gone
}

/// The plan of `key` among `kept` made the most recently used ([`keep_plan`] lets go of the
/// least), when there is one.
fn use_plan(kept: &mut Vec<LastPlan>, key: ContentId) {
    if let Some(at) = kept.iter().position(|it| it.key == key) {
        let plan = kept.remove(at);
        kept.push(plan);
    }
}

/// Each unit's outputs as ids of [`Paths`]: unit `at`'s are `ids[starts[at]..starts[at + 1]]`.
struct Outputs {
    /// The units they are of: a later build of the same units reads these.
    units: Arc<Units>,
    starts: Vec<u32>,
    ids: Vec<u32>,
}

impl Outputs {
    fn of(&self, unit: usize) -> &[u32] {
        match (self.starts.get(unit), self.starts.get(unit + 1)) {
            (Some(&from), Some(&to)) => &self.ids[from as usize..to as usize],
            _ => &[],
        }
    }
}

/// Every path an output is placed at or an image kept at, interned to a `u32` once, the first
/// time it comes in, for the daemon's life; the rest indexed by it, a column each.
struct Paths<S: Steps> {
    ids: HashMap<Box<str>, u32, Global>,
    /// As it came in: `/`-rooted.
    names: Vec<Box<str>>,
    /// The driver's name of it; none for a path that names no file under the driver's root.
    files: Vec<Option<FileName>>,
    /// What was placed there and how the file was after: an output of the same value whose
    /// file is still so is not placed again.
    placed: Vec<Option<Placement>>,
    /// The image kept linked there: moved into the compile that relinks it and back once it
    /// ends ([`Image`]), unless the kept images' budget lets it go ([`kept`]).
    images: Vec<Option<Image<S>>>,
    /// How many of `images` are some.
    kept: u32,
    /// What each of `images` weighs and when it was linked, for the budget.
    weights: kept::KeptImages,
    /// The memory the kept images' budget is a part of when the daemon was given no limit
    /// ([`blaze_graph::admission::limit`] is `u64::MAX`): the machine's, read once as the
    /// daemon reads its own ([`blaze_graph::arena::machine_budget`]); else the limit as the
    /// jobs were made, which [`Paths::budget`] reads again at each keep.
    machine: u64,
    /// The images a unit links beside its planned outputs (a library's doctests, each its own
    /// program, under its runner's `.doctests` directory), by the unit's first output: taken
    /// with that output's, so an edit relinks the one example it changed.
    extras: HashMap<u32, Vec<u32>, Global>,
    /// The images of compiles that gave way, put back into `images` before any is taken: a
    /// compile posted again relinks the image it was given, never a new one.
    back: Arc<SegQueue<Image<S>>>,
}

impl<S: Steps> Paths<S> {
    fn new(machine: u64) -> Paths<S> {
        Paths {
            ids: HashMap::new_in(Global),
            names: Vec::new(),  // alloc: none until the first path
            files: Vec::new(),  // alloc: none until the first path
            placed: Vec::new(), // alloc: none until the first path
            images: Vec::new(), // alloc: none until the first path
            kept: 0,
            weights: kept::KeptImages::new(),
            machine,
            extras: HashMap::new_in(Global),
            back: Arc::new(SegQueue::new()), // alloc: once for the daemon's life
        }
    }

    /// What the kept images may hold together: [`kept::budget`] of the limit the daemon runs
    /// under, or of the machine's memory when it was given no limit. Read at each keep, so it
    /// follows the limit as [`blaze_graph::admission::set_budgets`] last set it.
    fn budget(&self) -> u64 {
        let limit = match blaze_graph::admission::limit() {
            u64::MAX => self.machine,
            limit => limit,
        };
        kept::budget(limit)
    }

    fn intern(&mut self, path: &str) -> u32 {
        if let Some(&id) = self.ids.get(path) {
            return id;
        }
        let id = u32::try_from(self.names.len()).expect("under 2^32 paths");
        let name: Box<str> = path.into();
        self.ids.insert(name.clone(), id); // clone: a path's name, once for the daemon's life
        self.names.push(name);
        self.files.push(FileName::new(path.trim_start_matches('/')));
        self.placed.push(None);
        self.images.push(None);
        self.weights.path();
        id
    }

    /// The id of `path`, an output of the unit whose outputs are `ids`: a compare of the few
    /// it has, interned only when it is none of them.
    fn find(&mut self, ids: &[u32], path: &str) -> u32 {
        ids.iter()
            .copied()
            .find(|&id| &*self.names[id as usize] == path)
            .unwrap_or_else(|| self.intern(path))
    }

    /// Every unit's outputs interned.
    fn outputs(&mut self, units: &Arc<Units>) -> Outputs {
        let count = units.plans.iter().map(|plan| plan.outputs.len()).sum();
        let mut starts = Vec::with_capacity(units.plans.len() + 1);
        let mut ids = Vec::with_capacity(count);
        starts.push(0);
        for plan in &units.plans {
            ids.extend(plan.outputs.iter().map(|path| self.intern(path)));
            starts.push(u32::try_from(ids.len()).expect("under 2^32 outputs"));
        }
        Outputs { units: units.clone(), starts, ids } // clone: a refcount, once per new units
    }

    /// The images kept at `ids`, moved out for a compile. A path whose image the budget let go is said here, as its link is posted: that link
    /// finds none kept and lays the image out whole ([`super::work::Linking::First`]).
    fn take_images(&mut self, ids: &[u32]) -> Vec<Image<S>> {
        let budget = self.budget();
        self.take_images_under(ids, budget)
    }

    /// As [`Paths::take_images`], the kept images held to `budget` once these are taken.
    /// memcap3 review K1: the images of compiles that gave way come back first, and the
    /// eviction they may call for runs only after `ids`' images are out, so it never lets go
    /// of the image this link is about to patch (which would lay it out whole for nothing).
    fn take_images_under(&mut self, ids: &[u32], budget: u64) -> Vec<Image<S>> {
        let mut back = false;
        while let Some(image) = self.back.pop() {
            self.keep_one(&[], image, false);
            back = true;
        }
        let mut images = Vec::new(); // alloc: none unless an image is kept at an output
        let extras = ids.first().and_then(|first| self.extras.get(first));
        for &id in ids.iter().chain(extras.into_iter().flatten()) {
            if self.weights.take(id) {
                tracing::info!(
                    target: "blaze",
                    path = &*self.names[id as usize],
                    budget,
                    "link of a path whose kept image was let go past the kept link images' budget: none is kept, \
                     so it is laid out whole"
                );
            }
            if let Some(image) = self.images[id as usize].take() {
                self.kept -= 1;
                images.push(image);
            }
        }
        if back {
            self.evict_to(budget);
        }
        images
    }

    /// Images a compile gave back after linking them, kept at their paths, `ids` the unit's
    /// outputs; then the kept images held to their budget.
    fn keep(&mut self, ids: &[u32], images: Vec<Image<S>>) {
        for image in images {
            self.keep_one(ids, image, true);
        }
        let budget = self.budget();
        self.evict_to(budget);
    }

    /// `image` kept at its path, rested and weighed ([`Kept::rest`], [`Kept::held`]):
    /// `relinked` when a compile linked it, else as a compile that gave way took it.
    fn keep_one(&mut self, ids: &[u32], mut image: Image<S>, relinked: bool) {
        image.kept.rest();
        let bytes = image.kept.held();
        let id = self.find(ids, &image.path);
        self.weights.keep(id, bytes, relinked);
        if let Some(&first) = ids.first()
            && !ids.contains(&id)
        {
            // alloc: once per unit with images beside its outputs, its few ids
            let extras = self.extras.entry(first).or_default();
            if !extras.contains(&id) {
                extras.push(id);
            }
        }
        let id = id as usize;
        if self.images[id].replace(image).is_none() {
            self.kept += 1;
        }
    }

    /// Lets go of kept images, least recently linked first and never a live one, until the
    /// rest fit `budget` ([`Paths::budget`], [`kept::evictions`]). Each one let go is dropped
    /// here, its regions given back to the kernel and its heap freed: nothing else holds a kept
    /// image.
    fn evict_to(&mut self, budget: u64) {
        if self.weights.held() <= budget {
            return;
        }
        let _span = tracing::info_span!(target: "blaze", "kept link images evict", held = self.weights.held(), budget, kept = self.kept)
            .entered();
        // alloc: once per pass past the budget, a row per kept image
        let mut rows: Vec<kept::Row> = (self.images.iter().enumerate())
            .filter_map(|(id, image)| {
                Some(self.weights.row(id as u32, image.as_ref()?.kept.live()))
            })
            .collect();
        let evicted = kept::evictions(&mut rows, budget);
        for &id in &evicted.ids {
            let bytes = self.weights.let_go(id);
            // Dropped here: its regions decommitted and released, its heap freed.
            if self.images[id as usize].take().is_some() {
                self.kept -= 1;
            }
            tracing::info!(
                target: "blaze",
                path = &*self.names[id as usize],
                bytes,
                budget,
                kept = self.kept,
                "kept link image let go past the kept link images' budget: the next link of it lays it out whole"
            );
        }
        if evicted.held > budget {
            tracing::info!(
                target: "blaze",
                held = evicted.held,
                budget,
                kept = self.kept,
                "the kept link images are over their budget with every image no run maps let go: the rest are live"
            );
        }
    }
}

/// The checkouts later builds are of again ([`lasting`]), interned once by root directory.
struct Roots<S: Steps> {
    ids: HashMap<Box<str>, u32, Global>,
    /// The plans of each, one a request shape ([`keep_plan`]), the most recently used last:
    /// a request of a shape asked before finds its plan, whatever was asked between.
    plans: Vec<Vec<LastPlan>>,
    /// The front end of each one's last build, for its next job to lower into
    /// ([`Steps::lower`]).
    fronts: Vec<Option<S::KeptFront>>,
    /// How many of each one's front ends are being stored ([`Jobs::store_kept`]): its next
    /// job's lowering waits in [`Jobs::lowering`] until none is, and one is back in `fronts`.
    storing: Vec<u32>,
    /// The answers of its builds that made nothing ([`noop::LastAnswer`]), one a request shape
    /// beside that shape's plan ([`noop::Answers`], [`shape_of`]), each until a job of its own
    /// shape walks, with the view of the files its walk read; or why a shape has none.
    answers: Vec<Answers<WalkedView>>,
    /// Why what its last build left to settle after its answer was not kept ([`Jobs::lose`]),
    /// until its next build or check takes it as a miss `blaze why` says ([`Job::ended`]).
    lost: Vec<Option<String>>,
    /// Its last build or check, when that did not end well ([`Unfinished`]): set as the job
    /// is answered, none again once one ends well. Its next build says it first and takes it
    /// as a row `blaze why` says ([`Job::ended`]).
    unfinished: Vec<Option<Unfinished>>,
    /// Its front-end carry ([`super::front_carry`]): one kernel session kept across its
    /// builds, carried to each build's pin ([`carry_front`]).
    carries: Vec<CarryOf>,
}

/// A checkout's front-end carry and the plan whose crates it was last given.
#[derive(Default)]
struct CarryOf {
    state: CarryState,
    /// The plan `crates` are of, held so its address names it while it is compared.
    units: Option<Arc<blaze_units::query::Units>>,
    crates: Vec<super::front_carry::CrateRoot>,
}

#[derive(Default)]
enum CarryState {
    /// No session yet: the next build with crates seeds one.
    #[default]
    None,
    Held(super::front_carry::Carry),
    /// A carry failed: said on every build after it, never tried again (rule 3) until the
    /// daemon restarts.
    Failed(String),
}

/// The front end's carry of one build of a checkout ([`super::front_carry`]): its session
/// carried from the last build's pin to `input.root`'s, in the span "front end carry" with
/// what the carry did (seeded, repaired, changed, cut, gone; the paths the pins differ in,
/// the crates, the files held as roots) and its time in `ns`. The crates are asked of the
/// front once a plan. A failure is an error event naming it, and every later build's span
/// says it.
fn carry_front<S: Steps>(
    of: &mut CarryOf,
    vfs: &mut Vfs,
    store: &mut Store,
    input: &JobInput,
    front: &S::Front,
) {
    let span = tracing::info_span!(
        target: "blaze",
        "front end carry",
        seeded = tracing::field::Empty,
        repaired = tracing::field::Empty,
        changed = tracing::field::Empty,
        cut = tracing::field::Empty,
        gone = tracing::field::Empty,
        ns = tracing::field::Empty,
        first = tracing::field::Empty,
        dropped = tracing::field::Empty,
        paths = tracing::field::Empty,
        crates = tracing::field::Empty,
        files = tracing::field::Empty,
        ran = tracing::field::Empty,
        error = tracing::field::Empty,
    );
    let _entered = span.enter();
    let start = blaze_profile::clock::mono_ns();
    if !of.units.as_ref().is_some_and(|it| Arc::ptr_eq(it, &input.units)) {
        match <S::Front as Front<S>>::view_crates(front) {
            Ok(crates) => of.crates = crates,
            Err(why) => of.state = CarryState::Failed(why),
        }
        of.units = Some(Arc::clone(&input.units));
    }
    let done = match std::mem::take(&mut of.state) {
        CarryState::Failed(why) => Err(why),
        CarryState::None if of.crates.is_empty() => Ok(super::front_carry::Built::default()),
        CarryState::None => match super::front_carry::Carry::seed(vfs, store, input.root, &of.crates) {
            Ok((carry, built)) => {
                of.state = CarryState::Held(carry);
                Ok(built)
            }
            Err(error) => Err(error.to_string()),
        },
        CarryState::Held(mut carry) => match carry.build(vfs, store, input.root, &of.crates) {
            Ok(built) => {
                of.state = CarryState::Held(carry);
                Ok(built)
            }
            Err(error) => {
                carry.close(vfs);
                Err(error.to_string())
            }
        },
    };
    span.record("ns", blaze_profile::clock::mono_ns().saturating_sub(start));
    match done {
        Ok(built) => {
            let carried = built.carried;
            span.record("seeded", carried.seeded as u64);
            span.record("repaired", carried.repaired as u64);
            span.record("changed", carried.changed as u64);
            span.record("cut", carried.cut as u64);
            span.record("gone", carried.gone as u64);
            span.record("first", built.first);
            span.record("dropped", tracing::field::debug(carried.dropped));
            span.record("paths", built.paths as u64);
            span.record("crates", built.crates as u64);
            span.record("files", built.files as u64);
            span.record("ran", built.ran as u64);
        }
        Err(why) => {
            tracing::error!(target: "blaze", error = %why, "the front end's carry failed: no build of this checkout carries again until the daemon restarts");
            span.record("error", why.as_str());
            of.state = CarryState::Failed(why);
        }
    }
}

impl<S: Steps> Roots<S> {
    fn new() -> Roots<S> {
        Roots {
            ids: HashMap::new_in(Global),
            plans: Vec::new(),   // alloc: none until the first checkout
            fronts: Vec::new(),  // alloc: none until the first checkout
            storing: Vec::new(), // alloc: none until the first checkout
            answers: Vec::new(), // alloc: none until the first checkout
            lost: Vec::new(),    // alloc: none until the first checkout
            unfinished: Vec::new(), // alloc: none until the first checkout
            carries: Vec::new(),    // alloc: none until the first checkout
        }
    }

    fn intern(&mut self, root_dir: &str) -> u32 {
        if let Some(&id) = self.ids.get(root_dir) {
            return id;
        }
        let id = u32::try_from(self.plans.len()).expect("under 2^32 checkouts");
        self.ids.insert(root_dir.into(), id);
        self.plans.push(Vec::new()); // alloc: none
        self.fronts.push(None);
        self.storing.push(0);
        self.answers.push(Answers::new());
        self.lost.push(None);
        self.unfinished.push(None);
        self.carries.push(CarryOf::default());
        id
    }
}

/// A view made ready: lowered, or planned only for the work that walks the plan.
struct Ready<S: Steps> {
    input: JobInput,
    root: Option<u32>,
    front: Option<Arc<S::Front>>,
    writer: Option<S::Writer>,
}

enum Stage<S: Steps> {
    Library(NodeId),
    /// The local mounts no watch vouches for being read again, the view to open once they are.
    Locals(NodeId, Opened),
    Plan(NodeId, Prep),
    Fetch(Fetching),
    /// The view's pinned native inputs being laid out, the lowering to post once they are.
    Native(Box<NativeWait>),
    /// The view's units being lowered, and the checkout it is of.
    Lower(NodeId, JobInput, Option<u32>),
    Units(Box<Build<S>>),
    Answer(NodeId),
    /// [`Work::Ahead`]'s choice of units, the view lowered and ready.
    Ahead(NodeId),
    /// The tests or the run, once every unit is built: its run in [`Jobs::runs`].
    Runs(Handle<Run<S>>),
    /// Waits for the VFS's commit of the heads and the store's last put before this.
    Snapshot {
        ticket: Option<Ticket>,
        heads: Vec<(String, ContentId)>,
        store: blaze_cas::Tickets,
    },
    /// Its request is checked against its certificate now that this daemon has looked at
    /// the standard library ([`Jobs::certify_late`]): it waits for looks at its outputs,
    /// and has opened no view.
    Certify,
    Over,
}

/// The jobs waiting on one digest's layout ([`Jobs::native_waiting`]).
struct NativeWaiting {
    jobs: Vec<JobId>,
    /// The layout task's cancel flag: set once no job waits on it.
    stop: Arc<AtomicBool>,
}

/// A view's lowering, held while its pinned native inputs are laid out ([`Stage::Native`]):
/// what [`Jobs::lower_or_wait`] takes.
struct NativeWait {
    /// Each digest's layout node and its [`native_key`], its result read from
    /// [`Jobs::native_laid`].
    nodes: Vec<(NodeId, Key)>,
    /// How many have not settled.
    left: u32,
    /// The keys of those that settled failed, and why.
    failed: Vec<(Key, String)>,
    input: JobInput,
    root: Option<u32>,
    name: String,
    checkout: String,
}

/// What the plan needs fetched: fetch `at` is node `first + at`.
struct Fetching {
    first: NodeId,
    fetches: Arc<[Fetch]>,
    /// How many have not settled.
    left: u32,
    /// The places of those that failed, in the order they settled: every one is said at once
    /// as the batch ends ([`unavailable`]), never the first alone, so a blaze that can't fetch
    /// (built without `net`) names every package it lacks in one run.
    failed: Vec<u32>,
    prep: Prep,
}

struct Build<S: Steps> {
    input: JobInput,
    front: Arc<S::Front>,
    /// The front end, written only here, on the loop: each task that reads it is posted with
    /// a reader made from it then, and a write waits for those readers to end.
    writer: S::Writer,
    /// Reading tasks the plane asked to post while a write waited, posted once it is applied.
    held: Vec<(Posting, bool)>,
    /// Node `first + at` is of unit `units[at]`: one node a phase of its compile, a unit's
    /// nodes one after another ([`Phase`]), and one for a build script's run.
    first: NodeId,
    units: Vec<u32>,
    /// Each unit's package, by its place in the plan's packages; `u32::MAX` for none.
    package_of: Vec<u32>,
    /// The packages reported started, a bit each.
    reported: BitSet<u32>,
    /// Each unit's outputs, as ids of [`Jobs::paths`].
    outputs: Arc<Outputs>,
    /// The checkout it is of, whose front end is kept for its next build.
    root: Option<u32>,
    artifacts: Vec<Artifact>,
    /// The test executables compiled and not yet said ready, each its unit and its place in
    /// `artifacts`, in the order they compiled: each is said as [`Response::TestReady`] once
    /// nothing of its unit's outputs is in flight ([`Jobs::release_ready`]). Only for a cargo
    /// `test` or `bench` build ([`streams_units`]).
    ready: Vec<(u32, u32)>,
    /// `blaze test --ub`'s tests, run in the evaluator once every unit is checked.
    evaluation: Option<Evaluation>,
    /// The units a write that failed or panicked leaves unbuilt; none until one does.
    refused: Option<Refused>,
    /// Each unit's freshness by its place in the plan, once asked ([`Build::is_fresh`]).
    fresh: Vec<Option<bool>>,
    /// What became of each unit by its place in the plan, as its nodes settle: what the
    /// build's report of what it left unchecked is made of ([`Build::unchecked`]).
    verdicts: Vec<Verdict>,
    /// Of a build of only the units a view's front end applies ([`Jobs::next`]): how far it
    /// is, as its nodes settle; none for a build the job ends with.
    applying: Option<Applying>,
    /// Open from the units' nodes being posted until the first unit not found kept begins
    /// (or, when every unit is kept, until the build's units end): the stretch in which the
    /// build does nothing but find its dependencies' units kept, a node a phase of each, in
    /// dependency order, before the unit an edit changed can start. On ripgrep 2026-11-10
    /// this stretch of a body edit of `rg::main` was 650 ms that no span named
    /// (slow-is-p0-traced); the unit tasks inside it are its children by time.
    made_wait: Option<tracing::Span>,
}

/// A build of the units a view's front end applies, made in the job's one plane request
/// ([`Jobs::next`]): the request stays open for the job's next view or its answer, so the
/// build's end is its last node's settling ([`Jobs::settled`]), not the request's finish. One
/// request a job, one row of the plane and the history, however many views it builds for
/// and whether or not anything of them is made again.
struct Applying {
    /// Its nodes not yet settled.
    left: u32,
    /// One of them did not end well: failed, skipped or cancelled.
    failed: bool,
}

/// The units of a build that depend on a write to its front end that failed or panicked
/// ([`Writer::broke`]): each task of one is refused, naming that write and its unit, while
/// keep-going builds the rest.
struct Refused {
    /// Each unit's place in `whys`, `u32::MAX` for a unit not refused.
    by: Vec<u32>,
    /// Why, one for each write that broke.
    whys: Vec<String>,
}

impl Refused {
    /// Why `unit` is refused, if it is.
    fn why(&self, unit: usize) -> Option<&str> {
        let at = *self.by.get(unit)?;
        self.whys.get(at as usize).map(String::as_str)
    }
}

/// What became of a unit of a build ([`Build::verdicts`]).
#[derive(Clone)] // clone: the build's starting row, every unit unsettled
enum Verdict {
    /// Not settled yet, or not part of this build.
    Unsettled,
    /// Its last node ended well.
    Done,
    /// It failed: `open` when only its bodies or lints did ([`Compiled::open`]), so what
    /// depends on it was still checked under keep-going; `why` as its node says.
    Failed { open: bool, why: String },
    /// It was skipped behind the failure of node `because`.
    Skipped { because: NodeId },
}

struct Evaluation {
    ub: Ub,
    /// The node that selects the tests, once every unit is checked.
    select: NodeId,
    /// A test failed.
    failed: bool,
}

impl<S: Steps> Build<S> {
    fn unit(&self, node: NodeId) -> Option<usize> {
        self.units.get(node.0.wrapping_sub(self.first.0) as usize).map(|it| *it as usize)
    }

    /// Unit `unit` is refused from now on for `why`: its task, posted again after it gave way,
    /// fails with it ([`Jobs::post`]). What the ceiling's last step says of a task that alone
    /// does not fit ([`Jobs::fail_too_big`]). A unit refused already keeps its first reason.
    fn refuse_unit(&mut self, unit: usize, why: String) {
        let units = self.input.units.graph.units.len();
        let refused = self.refused.get_or_insert_with(|| Refused {
            by: vec![u32::MAX; units], // alloc: once a unit is refused
            whys: Vec::new(),          // alloc: once a unit is refused
        });
        if refused.by.get(unit).is_some_and(|it| *it == u32::MAX) {
            refused.by[unit] = refused.whys.len() as u32;
            refused.whys.push(why);
        }
    }

    /// The job's error for `broke`, a write that failed or panicked, naming the write and its
    /// unit, with every unit that depends on its unit refused from now on, each naming it. The
    /// unit itself was built, its write is what failed, unless the write was part of making
    /// it ([`Broke::itself`]). A unit refused already keeps its first reason.
    fn refuse_after(&mut self, broke: &Broke) -> ApiError {
        let graph = &self.input.units.graph.units;
        let unit = &graph[broke.unit];
        let error = ApiError::new(
            // alloc: an error, once
            format!(
                "applying {} of {} of {}: {}",
                broke.what, unit.target.name, unit.pkg_id, broke.how
            ),
            Some(unit.pkg_id.clone()), // clone: an error, once
        );
        let refused = self.refused.get_or_insert_with(|| Refused {
            by: vec![u32::MAX; graph.len()], // alloc: once a write breaks
            whys: Vec::new(),                // alloc: once a write breaks
        });
        let why = refused.whys.len() as u32;
        refused.whys.push(format!("a write it depends on broke: {}", error.what)); // alloc: an error, once
        let after = broke.unit;
        if broke.itself && refused.by[after] == u32::MAX {
            refused.by[after] = why;
        }
        // Units come in no particular order of their dependencies: reach until none is added.
        let mut grew = true;
        while grew {
            grew = false;
            for (at, it) in graph.iter().enumerate() {
                if at == after || refused.by[at] != u32::MAX {
                    continue;
                }
                let reached = |dep: &blaze_units::schema::UnitDep| {
                    dep.index as usize == after
                        || refused.by.get(dep.index as usize).is_some_and(|it| *it == why)
                };
                if it.dependencies.iter().any(reached) {
                    refused.by[at] = why;
                    grew = true;
                }
            }
        }
        error
    }

    /// `node` settled: its unit's verdict, unless an earlier node of it gave one, or its
    /// compile did ([`Jobs::compiled`]).
    fn settled(&mut self, node: NodeId, ok: bool, why: Option<&str>, skipped: Option<NodeId>) {
        let Some(unit) = self.unit(node) else { return };
        let at = node.0.wrapping_sub(self.first.0) as usize;
        let last = self.units.get(at + 1) != Some(&(unit as u32));
        let Some(verdict) = self.verdicts.get_mut(unit) else { return };
        if !matches!(verdict, Verdict::Unsettled) {
            return;
        }
        *verdict = match (ok, skipped) {
            (_, Some(because)) => Verdict::Skipped { because },
            // alloc: a unit that failed, once
            (false, None) => {
                Verdict::Failed { open: false, why: why.unwrap_or("it failed").to_owned() }
            }
            (true, None) if last => Verdict::Done,
            (true, None) => return,
        };
    }

    /// How its units stand, counted from their verdicts ([`Left`]).
    fn left(&self) -> Left {
        let mut left = Left { units: self.verdicts.len() as u32, ..Left::default() };
        for verdict in &self.verdicts {
            match verdict {
                Verdict::Done => {}
                Verdict::Failed { .. } => left.failed += 1,
                Verdict::Skipped { .. } => left.skipped += 1,
                Verdict::Unsettled => left.unsettled += 1,
            }
        }
        left
    }

    /// What the build left unchecked behind its failures and what it checked against a unit
    /// that failed open, from its units' verdicts, in the plan's order; none when it names
    /// nothing. `stopped`: the build stops at its first failure, so everything after it is
    /// skipped behind that one.
    fn unchecked(&self, stopped: bool) -> Option<Unchecked> {
        let graph = &self.input.units.graph.units;
        let name = |at: usize| graph.get(at).map(super::unit::span_unit);
        // Each skipped unit with the unit it is behind (`usize::MAX` for a node of no unit).
        let mut skipped: Vec<(usize, usize)> = (self.verdicts.iter().enumerate())
            .filter_map(|(at, it)| match it {
                Verdict::Skipped { because } => {
                    Some((self.unit(*because).unwrap_or(usize::MAX), at))
                }
                _ => None,
            })
            .collect(); // alloc: once per build that skipped a unit
        skipped.sort_unstable();
        let mut behind: Vec<Behind> = Vec::new(); // alloc: none until a unit was skipped
        let mut last_root = None;
        for (root, at) in skipped {
            if last_root.replace(root) != Some(root) {
                let why = match self.verdicts.get(root) {
                    Some(Verdict::Failed { why, .. }) => why.clone(), // clone: a failure, once in the report
                    _ => "it failed".to_owned(),                      // alloc: as above
                };
                let unit =
                    name(root).unwrap_or_else(|| "a step of the build that is no unit".to_owned()); // alloc: as above
                // A node of no unit (`usize::MAX`) has no id.
                let id = u32::try_from(root).ok().filter(|_| root < graph.len());
                behind.push(Behind { id, why, unit, units: Vec::new() }); // alloc: one per failure
            }
            if let (Some(group), Some(unit)) = (behind.last_mut(), name(at)) {
                group.units.push(unit);
            }
        }
        // Each test executable or doc test that never built, and the failure that held it back.
        let why_of = |at: usize| match self.verdicts.get(at) {
            Some(Verdict::Failed { why, .. }) => why.clone(), // clone: a failure, once in the report
            _ => "it failed".to_owned(),                      // alloc: as above
        };
        let mut tests = Vec::new(); // alloc: none until a test unit did not build
        for (at, verdict) in self.verdicts.iter().enumerate() {
            let Some(unit) = graph.get(at) else { continue };
            if !matches!(
                unit.mode,
                blaze_units::schema::Mode::Test | blaze_units::schema::Mode::Doctest
            ) {
                continue;
            }
            let root = match verdict {
                Verdict::Failed { .. } => at,
                Verdict::Skipped { because } => self.unit(*because).unwrap_or(usize::MAX),
                Verdict::Unsettled | Verdict::Done => continue,
            };
            let behind =
                name(root).unwrap_or_else(|| "a step of the build that is no unit".to_owned()); // alloc: as above
            tests.push(Unbuilt { unit: super::unit::span_unit(unit), behind, why: why_of(root) });
        }
        let mut open = Vec::new(); // alloc: none until a unit failed open
        if self.verdicts.iter().any(|it| matches!(it, Verdict::Failed { open: true, .. })) {
            // Each unit's dependents in the plan, to walk what was checked against a unit.
            let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); graph.len()]; // alloc: once per build with a unit failed open
            for (at, unit) in graph.iter().enumerate() {
                for dep in &unit.dependencies {
                    if let Some(it) = dependents.get_mut(dep.index as usize) {
                        it.push(at);
                    }
                }
            }
            for (failed, _) in self
                .verdicts
                .iter()
                .enumerate()
                .filter(|(_, it)| matches!(it, Verdict::Failed { open: true, .. }))
            {
                let mut seen = vec![false; graph.len()]; // alloc: once per unit failed open
                let mut stack = dependents[failed].clone(); // clone: as above
                while let Some(at) = stack.pop() {
                    if std::mem::replace(&mut seen[at], true) {
                        continue;
                    }
                    stack.extend_from_slice(&dependents[at]);
                }
                let checked = (0..graph.len())
                    .filter(|&at| {
                        seen[at]
                            && matches!(self.verdicts[at], Verdict::Done | Verdict::Failed { .. })
                    })
                    .filter_map(name)
                    .collect(); // alloc: once per unit failed open
                if let Some(unit) = name(failed) {
                    open.push(OpenFailure { unit, checked });
                }
            }
        }
        (!behind.is_empty() || !open.is_empty() || !tests.is_empty()).then_some(Unchecked {
            open,
            behind,
            tests,
            stopped,
        })
    }

    /// Whether `node` is its unit's first: the one whose start is the unit's.
    fn begins(&self, node: NodeId) -> bool {
        let at = node.0.wrapping_sub(self.first.0) as usize;
        at < self.units.len() && (at == 0 || self.units[at - 1] != self.units[at])
    }

    /// Whether unit `at` is fresh, its record kept so its tasks replay it: asked of the front
    /// end once, as its first task is posted or its run starts, and the same answer for its
    /// other tasks, for what they teach the plane ([`plane::Plane::replays`]) and its report.
    ///
    /// Asked at first post rather than at start, it answers the same but in one window. A post
    /// comes once the unit's dependencies settled, and the answer (`compile::fresh`,
    /// `script::fresh`) reads only the record key those fix and the store. So the two differ
    /// only when another job fills this unit's record in the store between its first post and
    /// its start: asked at post it says not fresh, the run then finds the record and is a hit,
    /// and the plane learns a near-zero cost for its key, one bad sample of the last few its
    /// prediction is the median of. Rare, and it costs one prediction, never a built value; the
    /// answer cached here also survives a repost of the task.
    fn is_fresh(&mut self, at: usize) -> bool {
        if let Some(fresh) = self.fresh[at] {
            return fresh;
        }
        let fresh = self.front.fresh(at);
        self.fresh[at] = Some(fresh);
        fresh
    }
}

struct Job<S: Steps> {
    work: Work,
    views: Vec<Opened>,
    ready: Vec<Ready<S>>,
    request: Option<RequestId>,
    /// Started by [`Builder::speculate`] and not promoted.
    idle: bool,
    closed: bool,
    finished: bool,
    cancelled: Arc<AtomicBool>,
    stage: Stage<S>,
    /// The value of the node its stage waits on, given before the node settles.
    value: Option<(NodeId, Value<S>)>,
    /// Index files and git repositories reported as `Updating`, once each.
    updating: HashSet<String, Global>,
    /// The diagnostics of no unit it said, by their text's content ID: each is the job's
    /// (a toolchain function every unit that reaches it fails on), said once however many
    /// units carry it ([`said_once`]).
    said: HashSet<ContentId, Global>,
    /// Opened by the job and closed as it finishes, but `built`.
    queries: Vec<ViewId>,
    /// The one of `queries` its last view was made ready in: handed to the session as it
    /// finishes ([`Said::Finish`]).
    built: Option<ViewId>,
    /// The root `built` is a view of: handed to the session with it.
    built_root: Option<ContentId>,
    /// Views of [`Roots::plans`] it reads, which the jobs close once none reads them.
    borrowed: Vec<ViewId>,
    /// Its plan's lock files were written into its view, which was opened again with them
    /// ([`Jobs::lock_into_view`]): the key of the view it was asked on ([`Viewed::key`]) and
    /// the write. A plan of the view opened again that asks for other bytes is an error,
    /// never a second round.
    locked: Option<(ContentId, LockWrite)>,
    placing: usize,
    /// How the bytes of what it placed got there: cloned, written into the store, copied
    /// ([`blaze_cas::PlacedBytes`]), for its `Work` report and its span.
    placed: blaze_cas::PlacedBytes,
    /// Its `Work` report, said as it finishes: once its placements have all ended, so it
    /// counts every byte they placed.
    worked: Option<Worked>,
    /// Of `placing`, how many are in flight at each path ([`Paths`] id), or wait behind
    /// another job's ([`Jobs::queued`]): a file with one is not yet all there. With whether
    /// the path is one its answer names, as its first effect there began ([`Busy`]).
    busy: HashMap<u32, Busy, Global>,
    /// Of `placing`, how many are at a path its answer names ([`Job::names`]): what its
    /// answer waits for ([`Jobs::wind_up`]). The rest go on after it ([`Settling::placing`]).
    needed: usize,
    /// How many times the loop wound up while it lived ([`Jobs::wind_up`], once each
    /// completion of a placement, a look or a run), and how long those took in all: said on
    /// its answer's span (`wind_ups`, `wind_up_us`), since each is too short for a span of
    /// its own and a build of many outputs makes thousands (slow-is-p0-traced).
    wind_ups: u32,
    wind_up_ns: i64,
    /// The paths its answer names, as [`Paths`] ids: each file of each unit's artifact
    /// ([`Artifact::path`], [`Artifact::filenames`], [`Artifact::executable`]: what `Built`
    /// says was made, and what a person or a tool runs or reads next) and each lock file it
    /// writes. Every other output (a program's hashed copy under `deps`, dep-info, a file
    /// cargo lists none of) is derived data the answer does not say: placed after it.
    names: HashSet<u32, Global>,
    /// Build scripts' runs the plane started whose programs are not yet all placed: each
    /// starts once its compile's outputs have nothing in flight ([`Jobs::run_script`]).
    held: Vec<NodeId>,
    /// The look of the shared std store ([`blaze_cas::Store::look_again`]) its units wait
    /// for: what another daemon put before the job began is found, never compiled again.
    look: u64,
    /// The build waiting for that look: the units a build ahead picked, or none for all.
    awaiting: Option<Option<Vec<u32>>>,
    /// Of a job that reads its views' front ends unbuilt ([`reads_front_unbuilt`]), how many
    /// of `ready` have had the units their front end applies built (its proc macros and build
    /// scripts' runs, [`applied_units`]) before anything reads it.
    applied: usize,
    /// While such a view builds those units, the span naming what the job's answer waits
    /// for: the work, the view and its proc macros (every-wait-says-what). Closed as that
    /// build ends ([`Jobs::applied`]); its build is one of the job's own plane request.
    applying: Option<tracing::Span>,
    /// It put into the shared std store: flushed once it ends, for the other daemons.
    shared: bool,
    /// Its runs of user code that have not ended.
    runs: u32,
    /// The tickets of records that a value verified across checkouts depends on where its
    /// roots are ([`blaze_usercode::roots::verify::retraction`]), and of what its runs of user
    /// code put: its answer does not wait for them; they settle after it ([`Settling`]).
    durable: blaze_cas::Tickets,
    /// The store of its kept front end under way ([`Jobs::store_kept`]): its answer does not
    /// wait for it; still under way as the job ends, it settles after it ([`Settling`]).
    storing: Option<Storing<S>>,
    /// The tickets of what its kept front end left for the store when that was stored before
    /// the job ended, each a put's own or that of the put that kept its value already; and the
    /// checkout its front end is of. They settle after its answer ([`Settling`]).
    stored: blaze_cas::Tickets,
    stored_of: Option<String>,
    /// A snapshot's alone: when, on the injected clock ([`World::now`]), it first waited for
    /// the store's puts before it. It fails once [`DURABLE_WITHIN`] has gone by from the later
    /// of then and the last step of the store's commit (bounded-progress, [`gave_up`]), the
    /// daemon's loop waking it then ([`Jobs::next_deadline`]).
    durable_since: Option<Duration>,
    over: Option<(bool, Option<ApiError>)>,
    stats: RequestStats,
    /// Units started that were fresh, and that were made again.
    fresh: u64,
    compiled: u64,
    /// What its build's front end served of each dependency ([`Writer::served`]), taken by
    /// its `Work` report.
    served: Vec<ServedCrate>,
    /// Each image its units linked and how ([`Compiled::links`]), taken by its `Work` report.
    links: Vec<(String, Linking, Option<blaze_api::Tiers>)>,
    /// The goal its build waits for ([`super::unit::request_goal`]), for its `Work`
    /// report: set as its build answers; none before, and for a request that asked for no unit.
    goal: Option<blaze_api::Goal>,
    /// Each image its units linked, for the history ([`LinkRow`]): the target it is of as
    /// cargo names it, its package, and whether it was laid out whole.
    linked: Vec<(String, Option<String>, bool)>,
    /// Why each unit, template and run it computed again was not reused, as each unit's
    /// compile and each run handed them over ([`Compiled::misses`], [`RunPuts::misses`]):
    /// this build's alone, sorted when it ends, for `blaze why`.
    misses: Vec<blaze_kernel::Miss>,
    /// The tests its runs finished, for the history ([`RunPuts::test_runs`]): their build id
    /// given as it ends.
    test_runs: Vec<TestRunRow>,
    /// The daemon process's user and system CPU when its plane request opened (getrusage),
    /// for its row's `daemon_user_ns` and `daemon_system_ns`.
    opened_cpu: Option<(Duration, Duration)>,
    /// When its plane request opened: on the schema's clock ([`blaze_profile::clock::mono_ns`])
    /// and on the wall's, for its row in the history.
    opened_ns: i64,
    opened_unix_ns: Option<i64>,
    /// The leases of the build dirs it builds into ([`lease_build_dirs`]), taken by its
    /// lowering and held until the job is dropped: the collector renames no dir while a build
    /// holds it (docs/gc.md, "Leases").
    leases: Arc<Lock<Leases>>,
    /// What its client is told of its wait for memory ([`Jobs::tell_paused`]).
    paused: blaze_graph::pressure::Paused,
    /// When it began, on the injected clock ([`World::now`]): what its [`Response::Doing`]
    /// and `blaze status` count its time from.
    began: Duration,
    /// When its last [`Response::Doing`] was said, on the same clock ([`Jobs::doing`]).
    doing_said: Option<Duration>,
    /// How many misses its history row took ([`Job::ended`]), sorted and each once: what its
    /// [`Worked::misses`] says, so a build with none is asked for no summary.
    missed: u64,
    /// While it is over and its answer is held for something of its own still under way
    /// ([`Jobs::wind_up`]): the span naming what, and which it names. A build whose tasks
    /// ended at 1.6 s answered at 6.9 s with no span over the 5 s between (2026-11-10).
    held_wait: Option<(tracing::Span, &'static str)>,
    /// While its units wait for the shared std store's look ([`Job::awaiting`]), the span
    /// naming that wait. Closed as the look lets it build ([`Jobs::looked`]).
    look_wait: Option<tracing::Span>,
    /// It placed an output ([`Jobs::place`]), whatever its bytes: its answer is not one of a
    /// build that made nothing.
    placed_any: bool,
    /// Why its checkout's kept answer was not said again and it walks, as the input `blaze
    /// why` names ([`noop::NotReplayed::input`]): its one miss when it ends having computed nothing
    /// again ([`Job::ended`]). None where its own work says why it walked.
    walks: Option<(String, ContentId)>,
    /// Why what its checkout's last build left to settle was not kept ([`Roots::lost`]), taken
    /// as its view opened: a miss of its own when it ends ([`Job::ended`]), so `blaze why`
    /// says the previous build's results were not kept and why.
    lost: Option<String>,
    /// Its checkout's last build, which did not end well ([`Roots::unfinished`]), taken as its
    /// view opened and said then: a row of its own when it ends having run anything
    /// ([`Job::ended`]).
    after: Option<Unfinished>,
    /// The checkout it is of, in [`Jobs::roots`], with its root directory, from its view's
    /// opening; none before, and for an agent's view.
    checkout: Option<(u32, String)>,
    /// How its units stood as its plane request ended not ok, for its checkout's next build
    /// ([`Unfinished::left`]).
    left: Option<Left>,
}

/// A job's kept front end being stored ([`Jobs::store_kept`]), by one task of a plane request
/// of its own, idle until a lowering waits for it: the job it is of and its command, the
/// request, its node, the root the front end is kept for and its checkout, and the task's
/// value once given. Its job's answer never waits for it: the job's while the job lives, a
/// [`Settling`]'s after.
struct Storing<S: Steps> {
    job: JobId,
    command: String,
    request: RequestId,
    node: NodeId,
    root: u32,
    checkout: String,
    /// Set once a lowering of a build a client waits on is held for it ([`Jobs::lower_or_wait`]):
    /// its request runs as one not idle from then, and its task skips the release
    /// ([`Steps::release_kept`]).
    wanted: Arc<AtomicBool>,
    promoted: bool,
    value: Option<(Option<S::KeptFront>, Result<Vec<super::work::KeptPut>, ApiError>)>,
}

/// What a job leaves to settle once its answer is sent ([`Jobs::wind_up`]): the store of its
/// kept front end still under way, and the tickets of what it put or may have read that the
/// store has not made durable yet. Not a job: in no order, counted by no wait of a client's,
/// and the no-op lane does not see it.
///
/// An answer may be sent on results not yet durable; nothing derived from that answer is
/// reused across a crash unless its tickets were durable. The checkout's kept answer
/// ([`noop::LastAnswer`]) is memory's alone, so no restart relies on it; what a restart reads
/// is the store, whose commit serves no value whose bytes were lost whenever answers are sent
/// (blaze-storemodel). A failure here changes no answer: it is recorded ([`Jobs::lose`]).
struct Settling<S: Steps> {
    job: JobId,
    command: String,
    /// The checkout its front end is of, or its request's root; none for an agent's view.
    checkout: Option<String>,
    /// Its first view's root, for what it names when it has no checkout.
    view: String,
    storing: Option<Storing<S>>,
    /// As [`Job::stored`], with every put the loop made before the job ended ([`Jobs::put`])
    /// and the last put of every segment of the store ([`Store::last_puts`]): a value it read
    /// comes with no ticket and may be another job's put (the store model's `Wait::Everything`,
    /// which what is kept waits for now, not the answer).
    stored: blaze_cas::Tickets,
    /// As [`Job::durable`].
    durable: blaze_cas::Tickets,
    /// How many of its job's placements were still under way as it was answered, less those
    /// that have ended since ([`Job::placing`] less [`Job::needed`], which was none): the
    /// outputs its answer does not name. Each goes on exactly as under the job (its mode
    /// set, the file looked at for its record); one that fails is a loss ([`Jobs::lose`]),
    /// and its path is placed again by the next build that makes it.
    placing: usize,
    /// `a build's other outputs are placed after its answer`, open while `placing` is not
    /// none: what the daemon still does for the build, by name (slow-is-p0-traced).
    placing_span: Option<tracing::Span>,
    /// Its job's leases of the build dirs it placed into ([`Job::leases`]), held while
    /// `placing` is not none: the collector renames no dir an output is still written into.
    leases: Option<Arc<Lock<Leases>>>,
    /// When, on the injected clock, it first waited for a ticket: given up as lost once
    /// [`DURABLE_WITHIN`] has gone by from the later of then and the last step of the store's
    /// commit (bounded-progress, [`gave_up`]).
    since: Option<Duration>,
    /// While it waits for the store's commit, the span naming the ticket (every-wait-says-what).
    wait: Option<tracing::Span>,
}

impl<S: Steps> Settling<S> {
    /// What is lost when the store's `ticket` (one of `stored`, or of `durable`) never becomes
    /// durable, for `why`.
    fn lost(&self, ticket: blaze_cas::Ticket, stored: bool, why: &str) -> String {
        let Settling { job, command, checkout, view, .. } = self;
        match (stored, checkout) {
            // alloc: a loss, once
            (true, Some(checkout)) => format!(
                "what the build of {checkout} left for the store or read from it (dependency images, check                  outcomes, goals; store ticket {ticket}) was put and is not durable, so a restarted daemon would                  work it out again: {why}"
            ),
            // alloc: a loss, once
            (true, None) => format!(
                "what job {job} (`{command}`, on the view of root {view}) read from the store, or the loop put                  before it (store ticket {ticket}), was put and is not durable, so a restarted daemon would work                  it out again: {why}"
            ),
            // alloc: a loss, once
            (false, _) => format!(
                "what job {job} (`{command}`) put itself (a build script's or test's results, a retraction, the                  record of where a verified value's roots are; store ticket {ticket}) is not durable, so a                  restarted daemon would work it out, or give it, again: {why}"
            ),
        }
    }
}

/// What a job left to settle after its answer that was not kept ([`Jobs::lose`]): the job, its
/// command, its checkout and what was lost and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lost {
    pub job: JobId,
    pub command: String,
    pub checkout: Option<String>,
    pub what: String,
}

/// How many losses the jobs keep to show ([`Jobs::lost`]): the last ones.
const LOST_SHOWN: usize = 16;

/// A checkout's last build or check, which did not end well ([`Roots::unfinished`]): what its
/// next one says first and `blaze why` says of it, so the time it spends on what that one left
/// is never read as a slow build of nothing (2026-11-10: after a failed cold build the next
/// three builds of an unchanged tree took 56 s, 10 s and 22 s, exited 0 and said nothing).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Unfinished {
    /// Its number in the history (its plane request); none when it ended before it had one.
    build: Option<u32>,
    command: String,
    /// It was cancelled, not failed.
    cancelled: bool,
    /// What became of its units, when it reached them.
    left: Option<Left>,
}

/// How a build's units stood as its plane request ended not ok ([`Build::left`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Left {
    units: u32,
    failed: u32,
    /// Skipped behind a failure.
    skipped: u32,
    /// Neither built, failed nor skipped: not reached, or not part of the build.
    unsettled: u32,
}

impl Unfinished {
    /// What the next build of `root` says of it: one line, facts only.
    fn said(&self, root: &str) -> String {
        let Unfinished { build, command, cancelled, left } = self;
        let which = match build {
            Some(build) => format!("build {build}, `{command}`"), // alloc: once a build after one that did not end well
            None => format!("`{command}`"), // alloc: as above
        };
        let how = if *cancelled { "was cancelled" } else { "failed" };
        let left = match left {
            Some(Left { units, failed, skipped, unsettled }) if failed + skipped + unsettled > 0 => {
                // alloc: as above
                format!(
                    " with {} of its {units} units not built ({failed} failed, {skipped} skipped \
                     behind a failure, {unsettled} not reached)",
                    failed + skipped + unsettled
                )
            }
            _ => String::new(),
        };
        // alloc: as above
        format!(
            "the last build of {root} ({which}) {how}{left}: this build first does the work that \
             one left, so its time is not a no-op's"
        )
    }
}

/// The step what a build did after its checkout's last build did not end well is under in
/// `blaze why` ([`Unfinished`]).
const UNFINISHED_STEP: &str = super::why::UNFINISHED;

/// The step a loss is under in `blaze why` ([`blaze_kernel::Miss::step`]), and what `blaze
/// slow` ranks first by.
const SETTLING_STEP: &str = blaze_profile::slow::NOT_KEPT_STEP;

/// A job's lowering waiting for its root's front end, which is being stored
/// ([`Jobs::lower`]): posted as the store ends, into the front end it gives back. It has no
/// plane node while it waits: `span`, open until it is posted or dropped, names the wait
/// (every-wait-says-what), and a stall names it ([`Jobs::held_lowerings`]).
struct Lowering {
    id: JobId,
    input: JobInput,
    root: u32,
    name: String,
    checkout: String,
    /// The node of the store it waits on, when one was found.
    on: Option<NodeId>,
    span: tracing::Span,
}

/// The jobs a client waits on, each with its stage and what it waits on there
/// ([`Jobs::waited_on`]): what a stall names when no plane task of the build runs.
pub struct WaitedOn<'a, S: Steps>(&'a Jobs<S>);

impl<S: Steps> std::fmt::Display for WaitedOn<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let jobs = self.0;
        let waited = jobs
            .order
            .iter()
            .filter_map(|id| Some((*id, jobs.jobs.get(key_of(*id))?)))
            .filter(|(_, it)| !it.idle);
        let count = waited.clone().count();
        if count == 0 && jobs.asked.is_empty() {
            return f.write_str("no job is under way");
        }
        write!(f, "{count} jobs under way")?;
        if !jobs.asked.is_empty() {
            write!(f, " and {} asked and not yet taken", jobs.asked.len())?;
        }
        for (at, (id, job)) in waited.enumerate() {
            if at == JOBS_NAMED {
                return write!(f, "; and {} more", count - at);
            }
            write!(f, "; job {id} {}", StageSaid(&job.stage))?;
            match job.request {
                Some(request) => write!(f, " (plane request {}", request.0)?,
                None => f.write_str(" (no plane request")?,
            }
            if job.finished {
                f.write_str(", finished")?;
            }
            if job.runs > 0 {
                write!(f, ", {} runs of user code going", job.runs)?;
            }
            if job.placing > 0 {
                write!(f, ", {} placements in flight", job.placing)?;
            }
            if !job.held.is_empty() {
                write!(f, ", {} build script runs held for their programs", job.held.len())?;
            }
            if job.awaiting.is_some() {
                f.write_str(", waiting for a look of the shared std store")?;
            }
            if job.storing.is_some() {
                f.write_str(", its front end being stored (its answer does not wait for it)")?;
            }
            if job.durable_since.is_some() {
                f.write_str(", waiting for the store's puts before it to be durable")?;
            }
            if let Some((ok, _)) = &job.over {
                write!(f, ", its outcome said ({})", if *ok { "ok" } else { "failed" })?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

/// Where a job is and what it waits on there (`waits for its pinned native inputs, 1 of 1
/// layouts left`): what a stall ([`WaitedOn`]), the client's waiting line
/// ([`Response::Doing`]) and `blaze status` say of it.
struct StageSaid<'a, S: Steps>(&'a Stage<S>);

impl<S: Steps> std::fmt::Display for StageSaid<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Stage::Library(node) => write!(f, "waits for the toolchain's library, node {}", node.0),
            Stage::Locals(node, _) => {
                write!(f, "waits for its paths outside the view to be read again, node {}", node.0)
            }
            Stage::Plan(node, _) => write!(f, "waits for its plan, node {}", node.0),
            Stage::Fetch(fetching) => write!(
                f,
                "waits for {} of its {} fetches, nodes from {}",
                fetching.left,
                fetching.fetches.len(),
                fetching.first.0
            ),
            Stage::Native(wait) => write!(
                f,
                "waits for its pinned native inputs, {} of {} layouts left",
                wait.left,
                wait.nodes.len()
            ),
            Stage::Lower(node, _, _) => write!(f, "waits for its lowering, node {}", node.0),
            Stage::Units(build) => {
                write!(f, "builds {} units, nodes from {}", build.units.len(), build.first.0)
            }
            Stage::Answer(node) => write!(f, "waits for its answer, node {}", node.0),
            Stage::Ahead(node) => write!(f, "waits for the units to build ahead, node {}", node.0),
            Stage::Runs(_) => f.write_str("runs its tests or its run"),
            Stage::Snapshot { ticket, .. } => write!(
                f,
                "waits for its snapshot's commit ({})",
                if ticket.is_some() { "the VFS's heads" } else { "the store's puts" }
            ),
            Stage::Certify => f.write_str("waits for looks at its outputs: its request certificate's check"),
            Stage::Over => f.write_str("is over and waits to end"),
        }
    }
}

/// How many jobs a stall's report names one by one.
const JOBS_NAMED: usize = 16;

/// A lowering held for its root's kept front end to be stored, as a stall names it.
#[derive(Debug)]
pub struct HeldLowering {
    pub job: JobId,
    pub root: u32,
    pub checkout: String,
    /// The node of the store it waits on.
    pub on: Option<u32>,
}

impl std::fmt::Display for HeldLowering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the lowering of job {:?} ({}) is held until root {}'s kept front end is stored",
            self.job, self.checkout, self.root
        )?;
        match self.on {
            Some(node) => write!(f, ", by node {node}"),
            None => write!(f, ", by no store the loop knows of (a lowering left held: a bug)"),
        }
    }
}

/// How long a job that is over waits for what it put to be durable with the store answering
/// nothing before it fails naming it (bounded-progress): no progress, not the whole wait, so a
/// healthy build behind a large commit waits for it ([`Jobs::commit_moved`], [`gave_up`]).
pub(super) const DURABLE_WITHIN: Duration = Duration::from_secs(120);

/// Why a wait since `since` for the store to make puts durable is given up, if it is: the
/// store broke, or no step of its commit answered for [`DURABLE_WITHIN`] since the later of
/// `since` and the last one that did (`heard`), each on the injected clock.
///
/// The time is judged only when `judge` is set, from [`Jobs::expire`], which the loop calls
/// once a turn after it has dispatched every completion of it: a winding up from inside one
/// completion's dispatch may run before a commit step that answered in the same turn is
/// counted, and must not fail a wait that step moved.
fn gave_up(
    store: &Store,
    now: Duration,
    since: Duration,
    heard: Duration,
    judge: bool,
) -> Option<String> {
    if let Some(broken) = store.broken() {
        return Some(broken.to_string()); // alloc: error path
    }
    if !judge {
        return None;
    }
    let from = since.max(heard);
    (now.saturating_sub(from) >= DURABLE_WITHIN).then(|| {
        // alloc: error path
        format!(
            "the store answered nothing of its commit for {} s (since {:.3} s on the daemon's clock): a sync \
             that does not return, or a disk that stopped",
            DURABLE_WITHIN.as_secs(),
            from.as_secs_f64()
        )
    })
}

/// How soon a wait whose puts are counted but not yet drained looks again ([`flush_for_wait`]):
/// another thread's put is counted ([`Store::last_puts`]) before its record is published to
/// the store's drain, for as long as the put takes, so the flush that found nothing new to
/// commit is tried again shortly rather than at the bound.
pub(super) const RECHECK: Duration = Duration::from_millis(10);

/// A wait for puts still pending, at each winding up: the store is flushed every time, which
/// drains what is published by now, costs nothing when nothing new is (a commit in flight
/// takes it at its end), and so drains a put another thread published after an earlier
/// flush. Says whether this flush started a commit, which the wait counts as the store's
/// progress, and whether no commit runs though puts are pending: one counted and not yet
/// published, to look for again in [`RECHECK`].
fn flush_for_wait(store: &mut Store, effects: &mut Vec<Effect>) -> (bool, bool) {
    let was = store.committing();
    store.flush(effects);
    let now = store.committing();
    (!was && now, !now)
}

/// [`flush_for_wait`] at most once a winding up (`flushed`, its idle once it ran), however
/// many jobs wait: each flush walks every segment's drain. A flush that started a commit
/// moves `heard` to `now`. Gives whether no commit runs though puts are pending.
fn flush_once(
    flushed: &mut Option<bool>,
    heard: &mut Duration,
    store: &mut Store,
    now: Duration,
    effects: &mut Vec<Effect>,
) -> bool {
    *flushed.get_or_insert_with(|| {
        let (started, idle) = flush_for_wait(store, effects);
        if started {
            *heard = (*heard).max(now);
        }
        idle
    })
}

/// The build dir leases of a job, taken by its lowering step on the pool (never on the loop):
/// shared with the job, which holds them until it ends, and past a stall until its tasks let go.
#[derive(Default)]
pub(super) struct Leases {
    pub(super) held: Vec<blaze_gc::lease::Lease>,
    /// When the lowering first found a lease it could not take now ([`blaze_gc::lease::Wait`])
    /// and gave way to be posted again, on [`blaze_profile::clock::mono_ns`]: it gives way for
    /// at most [`blaze_gc::lease::GIVE_WAY`] from then (review N8).
    pub(super) first_try_ns: Option<u64>,
}

/// Whether `dir` may get blaze's files (a lease, a record, a `CACHEDIR.TAG`): never `/` or a
/// directory right under it, nor the checkout or one that holds it, nor anything
/// [`blaze_gc::fs::Protected`] refuses (`$HOME`, `CARGO_HOME` and what is in it, blaze's root
/// and its state, the machine's shared directory and its std stores). A dir strictly inside the checkout (a project's `./target`) only when blaze
/// makes it now (`made`) or it is marked a build dir already ([`blaze_gc::record::is_marked`]:
/// cargo's and blaze's `CACHEDIR.TAG`), so a build writing into it holds a lease that `blaze
/// clean` and the collector see (review S2); a directory of the user's is never marked. Every
/// side resolved through [`blaze_gc::fs::resolve`], so `..` or a symlink is seen even before the
/// dir is there, and a path that cannot be resolved is an error naming it (review C7).
fn may_mark(
    dir: &std::path::Path,
    checkout: &std::path::Path,
    root: &std::path::Path,
    made: bool,
) -> Result<bool, String> {
    let real = |it: &std::path::Path| {
        blaze_gc::fs::resolve(it)
            .map_err(|why| format!("the build dir {} is not leased: {why}", dir.display()))
    };
    let resolved = real(dir)?;
    if resolved.parent().is_none_or(|it| it.parent().is_none()) {
        return Ok(false);
    }
    let checkout = real(checkout)?;
    if checkout.starts_with(&resolved) {
        return Ok(false);
    }
    if resolved.starts_with(&checkout) && !made && !blaze_gc::record::is_marked(&resolved) {
        return Ok(false);
    }
    // `<root>/build` itself, or a dir holding it, as a target dir: its lease would make every
    // workspace's build dir one build's, and `blaze clean` of it would take them all, leased or
    // not. A build into it fails, naming it (review round 5, B1).
    let builds = real(&root.join(blaze_gc::fs::BUILDS))?;
    if builds.ancestors().any(|it| it == resolved || blaze_gc::fs::same_dir(it, &resolved)) {
        return Err(format!(
            "the build dir {} ({}) is or holds {}, blaze's directory of every workspace's build dirs: a target dir is never it or above it. Name a directory of its own (`--target-dir`, `CARGO_TARGET_DIR`, `build.target-dir`)",
            dir.display(),
            resolved.display(),
            builds.display()
        ));
    }
    // The one list `blaze clean` refuses too (review N13): `$HOME`, `CARGO_HOME`, `RUSTUP_HOME`
    // and what is in them, blaze's root, its build directories and its state, the machine's
    // shared directory and its std stores. Made once, at the daemon's start (review round 6, C5).
    let protected = protected(root)
        .map_err(|why| format!("the build dir {} is not leased: {why}", dir.display()))?;
    Ok(protected.refuses(&resolved)?.is_none())
}

/// What no build dir may be, hold or be inside ([`blaze_gc::fs::Protected::of_env`]), made once
/// for blaze's root, with the root it was made for.
static PROTECTED: std::sync::OnceLock<(
    std::path::PathBuf,
    Result<blaze_gc::fs::Protected, String>,
)> = std::sync::OnceLock::new();

/// [`PROTECTED`] for blaze's root `root`, made on the first call: the daemon's start makes it, and
/// a list that does not make stops the start, naming why (review round 6, C5), so no lease
/// rebuilds it. A daemon builds for one root: another is an error naming both.
pub(super) fn protected(
    root: &std::path::Path,
) -> Result<&'static blaze_gc::fs::Protected, String> {
    let (made_for, made) = PROTECTED.get_or_init(|| {
        let made = match crate::serve::machine() {
            Ok(machine) => blaze_gc::fs::Protected::of_env(root, Some(&machine)),
            Err(error) => Err(format!("{error:#}")),
        };
        (root.to_path_buf(), made)
    });
    if made_for != root {
        return Err(format!(
            "what no build dir may be or hold was made for blaze's root {}, not {}: a daemon builds for one root",
            made_for.display(),
            root.display()
        ));
    }
    made.as_ref().map_err(|why| format!("what no build dir may be or hold: {why}"))
}

/// The daemon's [`Steps::lease`] (the simulation's steps take none, having no disk): the leases
/// of `units`' build dirs (`target_dir` and `build_dir`), each dir's record touched
/// for `checkout`, with each profile's or target's directory the build writes into (docs/gc.md).
/// Only the dirs under `builds` (`<root>/build/<dir>`) are ever collected; one a `--target-dir`
/// or `CARGO_TARGET_DIR` names is leased too, so `blaze clean` refuses it while a build uses it.
/// A dir [`may_mark`] refuses gets no file of blaze's and no lease. `CACHEDIR.TAG` is written
/// only into a dir blaze made now or one under `builds`, never into a dir of the user's.
/// `Ok(Err((dir, wait)))` when a lease cannot be taken now (held exclusively by a collector's
/// rename, or its file without a whole nonce yet): the caller gives way and tries again, for at
/// most [`blaze_gc::lease::GIVE_WAY`]. Runs on the pool: it may sync a record.
pub(super) fn lease_build_dirs(units: &Units, checkout: &str, builds: &str) -> super::work::Leased {
    let now = blaze_gc::record::now_ns();
    let builds = std::path::Path::new(builds);
    let root = builds.parent().unwrap_or(builds);
    let checkout_path = std::path::Path::new(checkout);
    // alloc: once per job, a lease or two
    let mut leases = Vec::new();
    for dir in [units.target_dir.as_str(), units.build_dir.as_str()] {
        let dir = std::path::Path::new(dir);
        if leases.iter().any(|it: &blaze_gc::lease::Lease| it.dir() == dir) {
            continue;
        }
        // Checked before anything is made: a dir right under `/` (a foreign workspace's
        // `/@blaze-foreign-target`) or one of the user's own is never made or marked.
        let made = !dir.exists();
        if !may_mark(dir, checkout_path, root, made)? {
            continue;
        }
        let _span =
            tracing::info_span!(target: "blaze", "lease of a build dir", dir = %dir.display())
                .entered();
        let lease = match blaze_gc::lease::Lease::try_hold(dir)? {
            Ok(lease) => lease,
            Err(wait) => return Ok(Err((dir.display().to_string(), wait))),
        };
        leases.push(lease);
        // Again once made: a symlink or `..` only resolves once the dir is there.
        if !may_mark(dir, checkout_path, root, made)? {
            leases.pop();
            continue;
        }
        let under_builds = std::fs::canonicalize(dir)
            .ok()
            .zip(std::fs::canonicalize(builds).ok())
            .is_some_and(|(dir, builds)| dir.parent() == Some(builds.as_path()));
        if made || under_builds {
            blaze_gc::record::tag_cache_dir(dir)?;
        }
        // Each directory in it the build writes into (a profile's, or a target's) is used now
        // too, written or not: the first part of each output's path under it.
        // alloc: once per job, a name a profile
        let mut profiles: Vec<&str> = units
            .plans
            .iter()
            .flat_map(|plan| plan.outputs.iter())
            .filter_map(|out| std::path::Path::new(out).strip_prefix(dir).ok())
            .filter_map(|rest| rest.components().next())
            .filter_map(|first| first.as_os_str().to_str())
            .collect();
        profiles.sort_unstable();
        profiles.dedup();
        if let Some(why) = blaze_gc::record::touch(dir, Some(checkout), &profiles, now)? {
            tracing::warn!(target: "blaze", dir = %dir.display(), "a build dir's record did not read and was written afresh: {why}");
        }
    }
    Ok(Ok(leases))
}

impl<S: Steps> Job<S> {
    /// A job as it begins, nothing of it done: `work` on `views`, `idle` when of declared
    /// intent, `walks` why its checkout's kept answer was not said again, begun at `began` on
    /// the injected clock. A snapshot has no plane request to finish.
    ///
    /// The one place a `Job` is made: [`Jobs::begin`] and every test go through it, so a
    /// field added to the struct is given its starting value here once and no literal
    /// elsewhere can miss it (2026-11-11: a test's own literal lacked three fields after a
    /// merge, and only a build of the tests saw it).
    fn fresh(
        work: Work,
        views: Vec<Opened>,
        idle: bool,
        walks: Option<(String, ContentId)>,
        began: Duration,
    ) -> Job<S> {
        let finished = matches!(work, Work::Snapshot);
        Job {
            work,
            views,
            ready: Vec::new(), // alloc: once per job, a view or two
            request: None,
            idle,
            closed: false,
            finished,
            cancelled: Arc::new(AtomicBool::new(false)), // alloc: once per job, shared with its tasks
            stage: Stage::Over,
            value: None,
            updating: HashSet::new_in(Global),
            said: HashSet::new_in(Global),
            queries: Vec::new(), // alloc: once per job
            built: None,
            built_root: None,
            borrowed: Vec::new(), // alloc: once per job
            locked: None,
            placing: 0,
            placed: blaze_cas::PlacedBytes::default(),
            worked: None,
            busy: HashMap::new_in(Global),
            needed: 0,
            wind_ups: 0,
            wind_up_ns: 0,
            names: HashSet::new_in(Global),
            held: Vec::new(), // alloc: none until a run waits for its program
            look: 0,
            awaiting: None,
            applied: 0,
            applying: None,
            shared: false,
            runs: 0,
            durable: blaze_cas::Tickets::default(),
            storing: None,
            stored: blaze_cas::Tickets::default(),
            stored_of: None,
            durable_since: None,
            over: None,
            stats: RequestStats::default(),
            fresh: 0,
            compiled: 0,
            served: Vec::new(),    // alloc: none; a build fills it once
            links: Vec::new(),     // alloc: none until a unit links
            goal: None,
            linked: Vec::new(),    // alloc: none until a unit links
            misses: Vec::new(),    // alloc: none until a unit misses
            test_runs: Vec::new(), // alloc: none until a test runs
            opened_cpu: None,
            opened_ns: 0,
            opened_unix_ns: None,
            leases: Arc::new(Lock::new("serve.job.leases", Leases::default())), // alloc: once per job, shared with its lowering
            paused: blaze_graph::pressure::Paused::default(),
            began,
            doing_said: None,
            missed: 0,
            held_wait: None,
            look_wait: None,
            placed_any: false,
            walks,
            lost: None,
            after: None,
            checkout: None,
            left: None,
        }
    }

    /// Its plane request opens now.
    fn opening(&mut self) {
        self.opened_ns = blaze_profile::clock::mono_ns();
        self.opened_cpu = daemon_cpu();
        self.opened_unix_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|it| i64::try_from(it.as_nanos()).ok());
    }

    /// The history's row of its plane request `request`, which ended `outcome` with the
    /// plane's counts `stats`.
    fn row(
        &self,
        request: RequestId,
        stats: Option<RequestStats>,
        outcome: profile_history::Outcome,
    ) -> BuildRow {
        let cargo = targets_of(&self.work).and_then(|it| it.cargo.as_ref());
        let workspace = match (cargo, self.views.first()) {
            // clone: a path, once a build
            (Some(cargo), _) => cargo.root.clone(),
            (None, Some(view)) => format!("view {}", view.view.root),
            (None, None) => "no view".to_owned(),
        };
        BuildRow {
            build_id: u64::from(request.0),
            commit: None,
            workspace,
            command: command_line(&self.work),
            started_ns: self.opened_ns,
            // A running build's row is its start, written as it opens: the row a build has if
            // its daemon dies before it ends.
            dur_ns: match outcome {
                profile_history::Outcome::Running => None,
                _ => Some(blaze_profile::clock::mono_ns() - self.opened_ns),
            },
            started_unix_ns: self.opened_unix_ns,
            outcome,
            hits: stats.map(|it| it.cached),
            // How many rows of `misses` the build has: counted where they are written, by the
            // history's thread once it explained them (`observe::keep`), never here. Until
            // 2026-11-11 this was the plane's count of nodes that ran a task (353 on every
            // build of ripgrep, a no-op's too, beside 3 rows in `misses`): two things under
            // one name. The plane's count is the `memo misses` counter and the `task` spans.
            misses: None,
            joined: stats.map(|it| it.joined),
            tree: None,
            blaze: None,
            client_ns: None,
            daemon_cpu_ns: match (outcome, self.opened_cpu, daemon_cpu()) {
                (profile_history::Outcome::Running, ..) => None,
                (_, Some((user, system)), Some((user_now, system_now))) => {
                    let ns = |it: Duration| i64::try_from(it.as_nanos()).unwrap_or(i64::MAX);
                    Some((ns(user_now.saturating_sub(user)), ns(system_now.saturating_sub(system))))
                }
                _ => None,
            },
        }
    }

    /// The history's rows of its plane request `request` ([`Job::row`]): its own, each image
    /// its units linked, and its units' crate graph.
    ///
    /// `logs`: its front end's run log is taken and named here, on the loop. Not for a build
    /// whose front end is kept: the task that stores it names the log, after the answer
    /// ([`Steps::kept_run_log`], [`Jobs::ran`]).
    fn ended(
        &mut self,
        request: RequestId,
        stats: Option<RequestStats>,
        outcome: profile_history::Outcome,
        logs: bool,
    ) -> Ended {
        let build_id = u64::from(request.0);
        let links = self
            .linked
            .iter()
            .map(|(image, krate, placed)| LinkRow {
                build_id,
                // clone: the history's copy, once a link
                image: image.clone(),
                krate: krate.clone(),
                outcome: if *placed { LinkOutcome::Placed } else { LinkOutcome::Patched },
                dur_ns: None,
            })
            .collect(); // alloc: one a link, once a build
        let crate_deps = match &self.stage {
            Stage::Units(build) => crate_deps(build_id, &build.input.units, &build.units),
            _ => Vec::new(),
        };
        // Sorted, so what `blaze why` keeps of the build is not down to the order its units
        // and runs ended in (rule 25).
        let mut misses = std::mem::take(&mut self.misses);
        // What its checkout's last build left to settle after its answer and lost: a miss of
        // this build's, so `blaze why` says the previous build's results were not kept and why.
        if let Some(why) = self.lost.take() {
            let root = targets_of(&self.work)
                .and_then(|it| it.cargo.as_ref())
                .map_or("", |cargo| cargo.root.as_str());
            misses.push(blaze_kernel::Miss {
                step: SETTLING_STEP,
                subject: Hasher::new("blaze v2 serve settling subject")
                    .part(root.as_bytes())
                    .finish(),
                item: format!("the last build of {root}"), // alloc: once a build after a loss
                missed: blaze_kernel::Missed::Always(why),
                key: ContentId::default(),
                kept: None,
                parts: Vec::new(), // alloc: none
                beyond: None,
                via: None,
            });
        }
        misses.sort_unstable();
        misses.dedup();
        self.missed = misses.len() as u64;
        // What it ran after its checkout's last build did not end well: a row `blaze why`
        // says first, so that build's remainder is not read as this one's slowness. Only when
        // it ran a task (a build the memo settled whole did none of that work). Not counted
        // in `missed`: it is why the work was there, not a thing computed again, and a build
        // that computed nothing again may still be kept as its checkout's answer.
        if let Some(after) = self.after.take()
            && let Some(ran) = stats.map(|it| it.ran).filter(|ran| *ran > 0)
        {
            let root = self.checkout.as_ref().map_or("", |(_, dir)| dir.as_str());
            misses.push(blaze_kernel::Miss {
                step: UNFINISHED_STEP,
                subject: Hasher::new("blaze v2 serve unfinished subject")
                    .part(root.as_bytes())
                    .finish(),
                item: format!("the last build of {root}"), // alloc: once a build after one that did not end well
                // alloc: as above
                missed: blaze_kernel::Missed::Always(format!(
                    "{}; it ran {ran} tasks",
                    after.said(root)
                )),
                key: ContentId::default(),
                kept: None,
                parts: Vec::new(), // alloc: none
                beyond: None,
                via: None,
            });
            misses.sort_unstable();
        }
        // A build that computed nothing again, and walked though its checkout's kept answer
        // could have been said again: what `blaze why` says of it is the input that moved or
        // that nothing covers ([`noop::NotReplayed::input`]). Not counted in `missed`: it computed
        // nothing, so its client asks for no summary and its own answer may be kept.
        if misses.is_empty()
            && self.compiled == 0
            && let Some((input, key)) = self.walks.take()
        {
            let command = command_line(&self.work);
            let root = targets_of(&self.work)
                .and_then(|it| it.cargo.as_ref())
                .map_or("", |cargo| cargo.root.as_str());
            misses.push(blaze_kernel::Miss {
                step: noop::STEP,
                subject: Hasher::new("blaze v2 serve no-op subject")
                    .part(root.as_bytes())
                    .part(command.as_bytes())
                    .finish(),
                item: format!("`{command}` in {root}"), // alloc: once a build that walked
                missed: blaze_kernel::Missed::Read {
                    read: input,
                    names: None,
                    was: blaze_kernel::Seen::Gone,
                    now: blaze_kernel::Seen::Is(key),
                    changed: 1,
                    reads: 1,
                },
                key,
                kept: None,
                parts: Vec::new(), // alloc: none
                beyond: None,
                via: None,
            });
        }
        let mut test_runs = std::mem::take(&mut self.test_runs);
        for it in &mut test_runs {
            it.build_id = build_id;
        }
        // Its front end's run log, named with its database in hand, before the front end is
        // kept or dropped: why each of its queries ran again.
        let provenance = match (&self.stage, logs) {
            (Stage::Units(build), true) => {
                super::observe::provenance_rows(build_id, build.writer.run_log())
            }
            _ => Vec::new(), // alloc: none
        };
        Ended {
            row: self.row(request, stats, outcome),
            links,
            crate_deps,
            misses,
            test_runs,
            provenance,
        }
    }

    /// The value its stage waited on, if node `node` gave it.
    fn take_value(&mut self, node: NodeId) -> Option<Value<S>> {
        self.value.take_if(|(at, _)| *at == node).map(|(_, value)| value)
    }
}

/// The name of the package `pkg_id` is of, as the plan has it.
fn package_named<'a>(units: &'a Units, pkg_id: &str) -> Option<&'a str> {
    units.packages.iter().find(|it| it.id == pkg_id).map(|it| it.name.as_str())
}

/// The edges between packages of the units `built`, one row each, sorted: from a build
/// script's compile a build-dependency, an edge only a dev-dependency makes a dev one, else
/// a normal one. A package's edges to itself, a build script's run's and the standard
/// library's are not of its manifest's tables, and are left out.
fn crate_deps(build_id: u64, units: &Units, built: &[u32]) -> Vec<CrateDepRow> {
    let graph = &units.graph.units;
    // alloc: once a build, for the history
    let names: HashMap<&str, &str, Global> =
        units.packages.iter().map(|it| (it.id.as_str(), it.name.as_str())).collect();
    // alloc: once a build, for the history
    let mut edges: Vec<(&str, &str, DepKind)> = Vec::new();
    for &at in built {
        let Some(unit) = graph.get(at as usize) else { continue };
        if unit.is_std || unit.mode == UnitMode::RunCustomBuild {
            continue;
        }
        let Some(&krate) = names.get(unit.pkg_id.as_str()) else { continue };
        let script = unit.target.is_custom_build();
        for dep in &unit.dependencies {
            let Some(to) = graph.get(dep.index) else { continue };
            if to.is_std || to.mode == UnitMode::RunCustomBuild || to.pkg_id == unit.pkg_id {
                continue;
            }
            let Some(&name) = names.get(to.pkg_id.as_str()) else { continue };
            let kind = match (script, dep.dev) {
                (true, _) => DepKind::Build,
                (false, true) => DepKind::Dev,
                (false, false) => DepKind::Normal,
            };
            edges.push((krate, name, kind));
        }
    }
    edges.sort_unstable_by_key(|it| (it.0, it.1, it.2.name()));
    edges.dedup();
    edges
        .into_iter()
        // alloc: the history's copy, once an edge
        .map(|(krate, dep, kind)| CrateDepRow {
            build_id,
            krate: krate.to_owned(),
            dep: dep.to_owned(),
            kind,
        })
        .collect()
}

/// What the session asked, in the order it asked it.
enum Asked {
    Start { id: JobId, view: Option<Opened>, work: Work, idle: bool },
    Cancel(JobId),
    Promote(JobId),
}

/// An effect of the loop's own, by the slot its tag carries.
#[derive(Clone, Copy)]
enum Own {
    /// A lock file written for the job.
    Write { job: JobId, path: u32 },
    /// A look at an output for the check of a request's certificate, before `job` is a job:
    /// the `ask`th of the asks its machine has open ([`Jobs::certify_told`]).
    Certify { job: JobId, ask: u32 },
    /// An executable's mode set, then its placement recorded.
    Mode { job: JobId, path: u32, key: blaze_cas::Key },
    /// A placed output's file looked at: placed again unless it is as it was placed.
    Look { job: JobId, path: u32, key: blaze_cas::Key, executable: bool },
    /// The file at a path looked at before a link's patch of it is placed there
    /// ([`Jobs::patching`]): the patch is written over a clone of it where it is as it was
    /// placed, and the whole file is written where it is not.
    LookBase { job: JobId, path: u32, key: blaze_cas::Key, executable: bool },
    /// A file placed, looked at for its [`Placement`] before its job is done: a file changed
    /// after the job's `Done` must not be recorded as the one placed.
    Record { job: JobId, path: u32, key: blaze_cas::Key },
}

/// A job's effects in flight at one path ([`Job::busy`]).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Busy {
    /// How many: placements, modes, looks, and outputs waiting behind another job's.
    effects: u32,
    /// Its answer names the path ([`Job::names`]): each of `effects` counts in
    /// [`Job::needed`]. Decided once, as the entry is made or the path is named
    /// ([`Jobs::name`]), so every end takes off exactly what its begin added.
    named: bool,
}

/// An output a compile gave whose path has an effect of another placement in flight (one an
/// answered job left, [`Settling::placing`], or another job's): placed once that has ended
/// ([`Jobs::release_queued`]), in the order asked, so two placements of one path never race
/// and the later value is the one left there. It counts as its job's placing meanwhile
/// ([`Jobs::hold`]): what reads the path through the daemon (a run, a build script's
/// program, a streamed test) waits for it as for any effect.
struct Queued {
    job: JobId,
    path: u32,
    key: blaze_cas::Key,
    executable: bool,
    made: Option<Arc<Made>>,
    /// `an output waits for the path's placement in flight`, open while it waits
    /// (every-wait-says-what).
    _waits: tracing::Span,
}

/// A job's build in the history and the `errors` rows it has so far ([`Jobs::erring`]).
struct Erring {
    build: u64,
    command: String,
    rows: u32,
    /// The job's cancel flag: a cancelled build that says no error did not fail.
    cancelled: Arc<AtomicBool>,
}

/// A placement the store was asked for.
struct Placing {
    job: JobId,
    path: u32,
    key: blaze_cas::Key,
    executable: bool,
}

/// A run of user code: a build script's (its node), or the job's tests or run.
struct Run<S: Steps> {
    job: JobId,
    node: Option<NodeId>,
    /// Put in once started: boxed, so the table's slots stay small whatever the runs hold.
    runs: Option<Box<S::Runs>>,
    /// Asked to end early: a run is killed once.
    killed: bool,
    /// Made and not started: a job's tests or run, which waits until every output of its
    /// job is placed ([`Jobs::release_runs`]), so the program it runs is all there.
    held: bool,
}

const _: () = assert!(std::mem::size_of::<JobId>() == 8);
const _: () = assert!(std::mem::size_of::<Own>() <= 52);
const _: () = assert!(std::mem::size_of::<Placing>() == 48);
// The job, the node, `killed` and `held` (18 bytes) and a box, padded to its alignment: 32
// bytes on a 64-bit target.
const _: () = assert!(
    std::mem::size_of::<Run<super::unit::Real>>()
        == (18 + size_of::<usize>()).next_multiple_of(align_of::<Run<super::unit::Real>>())
);

/// Whose each batch of nodes is, sorted by first id, a column each: the search reads `firsts`
/// alone, O(log batches) with nothing kept per node.
struct Batches {
    firsts: Vec<u32>,
    counts: Vec<u32>,
    jobs: Vec<JobId>,
}

impl Batches {
    fn new() -> Batches {
        // alloc: none until the first batch
        Batches { firsts: Vec::new(), counts: Vec::new(), jobs: Vec::new() }
    }

    /// Ids come in rising order until the plane's wrap past `2^32`, so this is a push but
    /// once per wrap.
    fn insert(&mut self, first: u32, count: u32, job: JobId) {
        let at = self.firsts.partition_point(|it| *it < first);
        self.firsts.insert(at, first);
        self.counts.insert(at, count);
        self.jobs.insert(at, job);
    }

    fn job_of(&self, node: NodeId) -> Option<JobId> {
        let at = self.firsts.partition_point(|it| *it <= node.0).checked_sub(1)?;
        (node.0 - self.firsts[at] < self.counts[at]).then(|| self.jobs[at])
    }

    /// Every batch of `job` let go.
    fn remove(&mut self, job: JobId) {
        let mut kept = 0;
        for at in 0..self.jobs.len() {
            if self.jobs[at] != job {
                self.firsts[kept] = self.firsts[at];
                self.counts[kept] = self.counts[at];
                self.jobs[kept] = self.jobs[at];
                kept += 1;
            }
        }
        self.firsts.truncate(kept);
        self.counts.truncate(kept);
        self.jobs.truncate(kept);
    }
}

/// The jobs of the session's [`Builder`], as the loop steps them.
pub struct Jobs<S: Steps> {
    setup: Setup,
    asked: Vec<Asked>,
    /// By the slot of their [`JobId`], the session's.
    jobs: Table<Job<S>>,
    /// The jobs in the order they started: every walk that says something goes in it.
    order: Vec<JobId>,
    batches: Batches,
    library: Option<String>,
    library_failed: Option<ApiError>,
    /// The errors of fetches that failed, by where they mount: a fetch is one task for every
    /// job asking it, so the jobs that joined it read its error here. Once per source.
    fetch_failed: HashMap<String, ApiError, Global>,
    /// The last result of laying out each digest, by its [`native_key`]: a layout is one task
    /// for every job asking that digest, so the jobs that joined it read its result here. Each
    /// settling task writes its own before its nodes settle.
    native_laid: BTreeMap<Key, Result<std::path::PathBuf, ApiError>>,
    /// The jobs waiting on each digest's layout, and the flag its task stops a wait for another
    /// process by: set only once every job waiting on it is gone, so a cancelled job that
    /// started the task never fails the jobs that joined it ([`Jobs::native_let_go`]).
    native_waiting: BTreeMap<Key, NativeWaiting>,
    /// The store's placements by the store's tag: its tags are its own, made among its other
    /// effects, so there is no slot of ours in them to index by.
    placing: HashMap<Tag, Placing, Global>,
    /// How many effects are in flight at each path ([`Paths`] id), whoever asked: a live
    /// job's or one an answered job left ([`Settling::placing`]). A path with none is all
    /// there as far as the daemon knows.
    flying: HashMap<u32, u32, Global>,
    /// The outputs waiting for their path's effects in flight to end, in the order asked.
    queued: Vec<Queued>,
    /// The patches whose path's file is being looked at ([`Own::LookBase`]), by path: one a
    /// path, as one effect is in flight there.
    patching: HashMap<u32, Arc<Made>, Global>,
    paths: Paths<S>,
    /// The lock writes, modes and looks at files in flight, by the slot their tag carries.
    own: Tagged<Own>,
    /// Every run of user code going, by the number its tags carry: run `n` has the tags
    /// `setup.runs + n * 2^32` on.
    runs: Tagged<Run<S>>,
    /// Every put the loop has made to the daemon's store (the plane's results, kept front ends,
    /// user code's runs, the jobs' records), as the last ticket of each segment they are in
    /// ([`blaze_cas::Tickets`]: a put of a value kept already gets the ticket of the put that
    /// kept it, from any writer). Two waits read it: a snapshot waits for all of it, and what a
    /// job leaves to settle after its answer waits for all of it as of the job's end
    /// (`Wait::Everything`, [`Settling::stored`]), since a value it read comes back with no
    /// ticket and may be another job's put not yet durable.
    put: blaze_cas::Tickets,
    /// The jobs answered whose results are not all durable yet, or whose kept front end is
    /// still being stored ([`Settling`]), in the order they ended.
    settling: Vec<Settling<S>>,
    /// The last of what settling lost ([`Jobs::lose`]), oldest first, [`LOST_SHOWN`] at most.
    lost: Vec<Lost>,
    /// The run logs of builds whose front end was kept, as the history's rows, named by the
    /// task that stored it ([`Steps::kept_run_log`]): for the daemon to hand to its history.
    pub ran: Vec<ProvenanceRow>,
    /// Each error a build said, as the history's `errors` rows ([`Jobs::note_errors`]): for
    /// the daemon to hand to its history, before the build's answer is told to it, so the
    /// answer's flush writes them.
    pub errors: Vec<blaze_profile::errors::ErrorRow>,
    /// What the daemon gives back when memory is short, and in which order
    /// ([`Jobs::give_back`], `blaze_graph::giveback`).
    giving: giving::Giving<S>,
    /// Each job with a plane request, from its opening until its `Finish` is heard: the
    /// build it is in the history and how many `errors` rows it has, so a build that ends
    /// not ok with none is given one ([`Jobs::note_errors`]).
    erring: HashMap<JobId, Erring, Global>,
    roots: Roots<S>,
    /// Views of plans replaced in `roots`, closed once no job reads them.
    retired: Vec<ViewId>,
    /// VFS views held for a no-op answer that went, or for a recording that kept none
    /// ([`WalkedView`], [`Recorded::view`]): closed at the loop's next step ([`Jobs::step`]),
    /// since what lets them go ([`Jobs::heard`]) has no VFS in hand.
    unheld: Vec<ViewId>,
    /// Why the request [`Jobs::answer_again`] was last asked of was not answered as its
    /// shape's last build was, in words: taken by [`Jobs::begin`] and said in a span once
    /// the request has its build, so the build's own profile names why it walked.
    refused: Option<String>,
    /// The request machines' driver: the checks under way before their requests are jobs,
    /// what one machine knows beside the records, and the builds whose certificates are to
    /// be written at the next step ([`certify`]).
    certify: certify::Certify,
    /// A plan was made since the plan's database was last collected (`Jobs::expire`'s end):
    /// the values the plan before it held alone are let go of at the next turn.
    planned: bool,
    /// What a run of user code says, a buffer reused by each of its calls.
    out: Vec<Response>,
    /// The tasks a write held, a buffer reused by each [`Jobs::apply_writes`].
    posts: Vec<(Posting, bool)>,
    /// The jobs that end, a buffer reused by each [`Jobs::wind_up`].
    over: Vec<(JobId, bool, Option<ApiError>)>,
    /// The requests held back for memory, and the tasks granted memory running that they
    /// wait on, buffers reused by each [`Jobs::tell_paused`].
    held_back: Vec<u32>,
    waited_on: Vec<u32>,
    /// A job ran a query or a unit since the image was written.
    pub cold: bool,
    /// A job finished since the last collection.
    pub ended: bool,
    /// The plane requests that ended, as the profile history's rows, for the daemon to hand
    /// to its history.
    pub rows: Vec<Ended>,
    /// The builds (plane requests, by number) whose answer was said this turn, in the order
    /// said: the daemon tells its history once the answers are with the driver
    /// (`Observe::answered`), and only then is a build's profile flushed.
    pub answered: Vec<u64>,
    /// The plane requests that opened, as their rows of outcome running ([`Job::row`]), for the
    /// daemon to hand to its history at once.
    pub started: Vec<BuildRow>,
    /// The build dir leases of jobs dropped while their tasks may still write (a stall): each
    /// held until the last task lets go of the job's cancel flag ([`Jobs::release_leases`]).
    held_leases: Vec<(JobId, Arc<AtomicBool>, Arc<Lock<Leases>>)>,
    /// Lowerings waiting for their root's front end to be stored ([`Jobs::lower`]).
    lowering: Vec<Lowering>,
    /// The builds under way that may become their checkout's kept answer
    /// ([`noop::LastAnswer`]), each with its job and its checkout.
    recordings: Vec<Recorded>,
    /// The plane requests of kept front ends' stores ([`Jobs::store_kept`]), until each
    /// finishes: a store is part of its job's build, never a build of its own.
    store_requests: HashSet<RequestId, Global>,
    /// When, on the injected clock, a step of the store's commit last answered
    /// ([`Store::commit_steps`] moved): a wait for it to make puts durable gives up only once
    /// the commit has stood still for [`DURABLE_WITHIN`] ([`gave_up`]), so a large commit that
    /// is moving never trips it, and a sync that never returns does even while other builds'
    /// reads and placements keep the store answering.
    store_heard: Duration,
    /// [`Store::commit_steps`] as of `store_heard`.
    commit_steps: u64,
    /// When a wait whose puts no commit is taking looks again ([`RECHECK`]), if one is.
    recheck: Option<Duration>,
}

impl<S: Steps> Builder for Jobs<S> {
    fn start(&mut self, job: JobId, view: Option<Opened>, work: Work) {
        self.asked.push(Asked::Start { id: job, view, work, idle: false });
    }

    fn cancel(&mut self, job: JobId) {
        self.asked.push(Asked::Cancel(job));
    }

    fn speculate(&mut self, job: JobId, view: Opened, work: Work) {
        self.asked.push(Asked::Start { id: job, view: Some(view), work, idle: true });
    }

    fn promote(&mut self, job: JobId) {
        self.asked.push(Asked::Promote(job));
    }
}

/// Why [`Jobs::new`] could not make the jobs.
#[derive(Debug)]
pub enum JobsFailed {
    /// Reserving the tables of effects or runs.
    Tables(MemError),
    /// Reading the machine's memory, which the kept link images' budget is a part of when the
    /// daemon was given no limit.
    Machine(std::io::Error),
}

impl From<MemError> for JobsFailed {
    fn from(error: MemError) -> JobsFailed {
        JobsFailed::Tables(error)
    }
}

impl std::fmt::Display for JobsFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The daemon names the tables: "the jobs' tables: <error>".
            JobsFailed::Tables(error) => write!(f, "{error}"),
            JobsFailed::Machine(error) => write!(
                f,
                "reading the machine's memory, which the kept link images' budget is a part of when the daemon \
                 is given no limit: {error}"
            ),
        }
    }
}

impl<S: Steps> Jobs<S> {
    /// The jobs, their tables of effects and runs reserved, committing nothing. When the
    /// daemon was given no limit ([`blaze_graph::admission::limit`]), the machine's memory is
    /// read for the kept link images' budget ([`Paths::budget`]).
    pub fn new(setup: Setup) -> Result<Jobs<S>, JobsFailed> {
        let numbers = setup.runs_end.saturating_sub(setup.runs) / RUN_TAGS;
        let bits = numbers.checked_ilog2().unwrap_or(0);
        let run_slots = bits.min(RUN_SLOT_BITS);
        let own = Tagged::new(OWN_SLOT_BITS, OWN_GENERATION_BITS, MemTag::Io)?;
        let runs = Tagged::new(run_slots, bits - run_slots, MemTag::Usercode)?;
        let machine = match blaze_graph::admission::limit() {
            // The margin the daemon leaves the machine when it reads its own budget.
            u64::MAX => {
                blaze_graph::arena::machine_budget(1 << 30, None)
                    .map_err(JobsFailed::Machine)?
                    .bytes
            }
            limit => limit,
        };
        Ok(Jobs {
            setup,
            asked: Vec::new(), // alloc: none until the session asks
            jobs: Table::new(),
            order: Vec::new(), // alloc: none until a job starts
            batches: Batches::new(),
            library: None,
            library_failed: None,
            fetch_failed: HashMap::new_in(Global),
            native_laid: BTreeMap::new(),
            native_waiting: BTreeMap::new(),
            placing: HashMap::new_in(Global),
            flying: HashMap::new_in(Global),
            queued: Vec::new(), // alloc: none until an output waits for its path
            patching: HashMap::new_in(Global),
            paths: Paths::new(machine),
            own,
            runs,
            put: blaze_cas::Tickets::default(),
            settling: Vec::new(), // alloc: none until a job leaves something to settle
            lost: Vec::new(),     // alloc: none until settling loses something
            ran: Vec::new(),      // alloc: none until a kept front end's run log is named
            errors: Vec::new(),   // alloc: none until a build says an error
            giving: giving::Giving::new(),
            erring: HashMap::new_in(Global),
            roots: Roots::new(),
            retired: Vec::new(), // alloc: none until a plan is replaced
            unheld: Vec::new(),  // alloc: none until a no-op answer goes
            refused: None,
            certify: certify::Certify::new(),
            planned: false,
            out: Vec::new(),     // alloc: none until user code says something, then reused
            posts: Vec::new(),   // alloc: none until a write holds a task, then reused
            over: Vec::new(),    // alloc: none until a job ends, then reused
            // alloc: none until a build is held back for memory, then reused
            held_back: Vec::new(),
            // alloc: none until a build is held back for memory, then reused
            waited_on: Vec::new(),
            cold: false,
            ended: false,
            rows: Vec::new(),        // alloc: none until a build ends, then reused
            answered: Vec::new(),    // alloc: none until a build is answered, then reused
            started: Vec::new(),     // alloc: none until a build opens, then reused
            held_leases: Vec::new(), // alloc: none until a stall
            lowering: Vec::new(), // alloc: none until a job lowers while its root's front end is stored
            recordings: Vec::new(), // alloc: none until a build walks its checkout's last plan
            store_requests: HashSet::new_in(Global),
            store_heard: Duration::ZERO,
            commit_steps: 0,
            recheck: None,
        })
    }

    /// Lets go of the build dir leases of dropped jobs no task of which is left: a task holds a
    /// clone of its job's cancel flag until it ends.
    pub fn release_leases(&mut self) {
        if self.held_leases.is_empty() {
            return;
        }
        // A dropped job's outputs still being placed, or its runs of user code not ended, write
        // into its build dirs as much as its tasks do (review C5b).
        // alloc: a stall's leases, rarely
        let mut busy: Vec<JobId> = self.placing.values().map(|it| it.job).collect();
        busy.extend(self.runs.iter_mut().map(|(_, it)| it.job));
        self.held_leases
            .retain(|(id, cancelled, _)| Arc::strong_count(cancelled) > 1 || busy.contains(id));
    }

    /// Each build's client told of its wait for memory (docs/memory-budget.md section 7, "What
    /// the client is told"): a note once a task of its request has been held back for memory
    /// alone for [`blaze_graph::pressure::PAUSED_AFTER_NS`], again as what it says changes, and
    /// that it resumed once none is ([`blaze_graph::pressure::Paused`]). `machine` is the
    /// machine ledger's say of who holds the rest, once its driver gives one. A speculative
    /// job not promoted is told nothing: no client waits on it.
    pub fn tell_paused(&mut self, w: &mut World<'_, S>, machine: Option<&str>) {
        use blaze_graph::pressure::{RESUMED, Tell, paused_line};
        let mut held_back = std::mem::take(&mut self.held_back);
        held_back.clear();
        w.plane.held_back(&mut held_back);
        let (held, budget, cap) = w.plane.memory();
        let waiting = |job: &Job<S>| job.paused.waiting();
        let any = self.order.iter().any(|&id| self.jobs.get(key_of(id)).is_some_and(waiting));
        let Some(budget) = budget.filter(|_| any || !held_back.is_empty()) else {
            self.held_back = held_back;
            return;
        };
        // What they wait on: the tasks granted memory running, in the order of their nodes,
        // one a name (a task several requests share runs as one).
        let plane: &Tasks<S> = &*w.plane;
        // alloc: none but while a build is held back for memory, a pair a running task.
        let mut running: Vec<(NodeId, &str)> = Vec::new();
        plane.granted_running(|id, name| running.push((id, name)));
        running.sort_unstable_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(&b.0)));
        running.dedup_by(|a, b| a.1 == b.1);
        running.sort_unstable_by_key(|it| it.0);
        let mut on = std::mem::take(&mut self.waited_on);
        on.clear();
        on.extend(running.iter().map(|it| it.0.0)); // alloc: none once it has held this many.
        let names: Vec<&str> = running.iter().take(3).map(|it| it.1).collect(); // alloc: three at most.
        let at = super::plane::ns(w.now);
        let mut line: Option<String> = None;
        for &id in &self.order {
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            let Some(request) = job.request.filter(|_| !job.idle && !job.finished) else {
                continue;
            };
            let now = held_back.binary_search(&request.0).is_ok();
            let response = match job.paused.step(at, now, held, budget, &on) {
                Tell::Nothing => continue,
                Tell::Paused => {
                    let line = line.get_or_insert_with(|| {
                        paused_line(held, budget, cap, machine, on.len(), &names)
                    });
                    Diagnostic::note(line.clone()) // clone: one line, told to each build it holds back.
                }
                Tell::Resumed => Diagnostic::note(RESUMED.to_owned()), // alloc: once a resume
            };
            w.said.push(Said::Report(id, Response::Diagnostic(response)));
        }
        self.held_back = held_back;
        self.waited_on = on;
    }

    /// Whether no job is under way. What answered jobs left to settle ([`Settling`]) is no
    /// job: [`Jobs::quiet`] asks of it too.
    pub fn idle(&self) -> bool {
        self.jobs.is_empty() && self.asked.is_empty()
    }

    /// Whether no job is under way and nothing an answered job left is still settling: every
    /// kept front end is back with its root, and every ticket is durable or said lost.
    pub fn quiet(&self) -> bool {
        self.idle() && self.settling.is_empty()
    }

    /// How many answered jobs still have something settling ([`Settling`]).
    pub fn settling(&self) -> usize {
        self.settling.len()
    }

    /// The content key of the output placed at `path` whose file was as placed at the jobs'
    /// last look ([`Placement`]); `None` for a path nothing is placed at, one whose placement
    /// is in flight, or one whose file a look found changed. What a launch of the file there
    /// may take the file for (`serve::unit::run::inproc::DevRuns::launch`).
    pub fn placed_key(&self, path: &str) -> Option<[u8; 32]> {
        let id = *self.paths.ids.get(path)?;
        let placed = self.paths.placed[id as usize].as_ref()?;
        placed.meta.map(|_| placed.key.0.0)
    }

    /// How many outputs answered jobs left being placed ([`Settling::placing`]): the loop is
    /// not between requests while any is, as while a job is under way.
    pub fn placing_after(&self) -> usize {
        self.settling.iter().map(|it| it.placing).sum()
    }

    /// Whether the checkout at `dir` has a kept answer of its most recently used request
    /// shape ([`noop::Answers::latest`]) known to be of the view `root`
    /// ([`noop::LastAnswer::files_known`]): its last build or check, sent again, is answered
    /// by the no-op lane, while nothing else it rests on moves. What the daemon builds ahead
    /// of a save until ([`super::ahead`]), which sends that very request: an answer of
    /// another shape (a `check`'s, where the last request was a `run`) does not count.
    pub fn answer_known(&self, dir: &str, root: &ContentId) -> bool {
        self.roots.ids.get(dir).is_some_and(|&at| {
            self.roots.answers[at as usize].latest().is_some_and(|answer| answer.files_known(root))
        })
    }

    /// The last of what settling lost, oldest first ([`Jobs::lose`]).
    pub fn lost(&self) -> &[Lost] {
        &self.lost
    }

    /// What a job left to settle after its answer was not kept: `what` says what and why. The
    /// answer stands as it was sent. The loss is said once in the daemon's log, naming the
    /// checkout, the command and what was lost; reported as a bug `blaze status` shows; and
    /// kept for the checkout's next build or check, whose `blaze why` says it ([`Roots::lost`]).
    fn lose(&mut self, job: JobId, command: &str, checkout: Option<&str>, what: String) {
        let of = checkout.unwrap_or("no checkout");
        // logged: the warn event below, with the job, command and checkout as fields
        eprintln!(
            "blaze serve: the results of `{command}` in {of} (job {job}) were not kept after its answer was sent: {what}"
        );
        tracing::warn!(
            target: "blaze",
            job = %job,
            command,
            checkout = of,
            "a build's results were not kept after its answer was sent: {what}"
        );
        stdx::assert::reported::report(
            "settling",
            "what a build left for the store was not kept after its answer was sent: the daemon's log names the checkout, the command and what was lost",
        );
        if let Some(&root) = checkout.and_then(|it| self.roots.ids.get(it)) {
            // alloc: a loss, once
            self.roots.lost[root as usize] = Some(format!(
                "the last build of {of} (`{command}`) answered before what it left for the store was durable,                  and it was not kept: {what}"
            ));
        }
        if self.lost.len() == LOST_SHOWN {
            self.lost.remove(0);
        }
        // alloc: a loss, once
        self.lost.push(Lost {
            job,
            command: command.to_owned(),
            checkout: checkout.map(str::to_owned),
            what,
        });
    }

    /// How many jobs a client waits on whose wait has no bound of its own: every job but a
    /// speculative one not promoted (nothing waits on it) and a snapshot waiting for the store's
    /// puts to be durable (which fails itself at [`DURABLE_WITHIN`]), with what was asked and not yet
    /// taken. Work under way whether or not the plane runs a task of it, so a job left with
    /// nothing that can move it (a node that never settles, a wait no completion ends) is a
    /// stall the watchdog says, never a hang.
    pub fn under_way(&self) -> usize {
        self.asked.len()
            + self.jobs.values().filter(|job| !job.idle && job.durable_since.is_none()).count()
    }

    /// The newest build under way: the highest request of a job that is not idle and not
    /// finished, 0 for none. What the daemon's process-wide counters are said to be of
    /// (`blaze_profile::flight::counting_for`): two builds that overlap share their counters,
    /// and the newer one is named.
    pub fn build_under_way(&self) -> u32 {
        self.jobs
            .values()
            .filter(|job| !job.idle && !job.finished)
            .filter_map(|job| job.request)
            .map(|request| request.0)
            .max()
            .unwrap_or(0)
    }

    /// Whether a run of user code (a build script, a test, a run) of any job has not ended:
    /// work under way though nothing of the build moves meanwhile.
    pub fn runs_live(&self) -> bool {
        self.jobs.values().any(|job| job.runs > 0)
    }

    /// The jobs a client waits on and what each waits on, as a stall says them.
    pub fn waited_on(&self) -> WaitedOn<'_, S> {
        WaitedOn(self)
    }

    /// The database of the first root's front end kept now, whose library parses the next
    /// image stores ([`Parts::front`]); `None` while no front end is kept.
    pub fn front_db(&self) -> Option<&base_db::Db> {
        self.roots.fronts.iter().flatten().find_map(KeptDb::db)
    }

    /// The roots the jobs' views read, kept by a collection, in the order the jobs started.
    pub fn roots(&self) -> Vec<ContentId> {
        let jobs = self.order.iter().filter_map(|id| self.jobs.get(key_of(*id)));
        // alloc: once per collection
        jobs.flat_map(|it| it.views.iter().map(|view| view.view.root)).collect()
    }

    pub fn owns(&self, tag: Tag) -> bool {
        if tag.0.wrapping_sub(self.setup.tags) < 1 << 32 {
            return true;
        }
        tag.0 >= self.setup.runs && tag.0 < self.setup.runs_end
    }

    /// Takes what the session asked since the last call.
    pub fn step(&mut self, w: &mut World<'_, S>) {
        // The certificate of each build that ended since the last step, written before any
        // request asked since is checked against it.
        self.certify_written(w);
        // The views held for no-op answers that went since the last step.
        for view in self.unheld.drain(..) {
            w.vfs.close(view);
        }
        if !self.asked.is_empty() {
            let mut asked = std::mem::take(&mut self.asked);
            for it in asked.drain(..) {
                match it {
                    Asked::Start { id, view, work, idle } => self.begin(w, id, view, work, idle),
                    Asked::Cancel(id) => {
                        self.certify_cancel(w, id);
                        self.cancel_job(w, id)
                    }
                    Asked::Promote(id) => self.promote_job(w, id),
                }
            }
            if self.asked.is_empty() {
                self.asked = asked;
            }
        }
        self.wind_up(w);
    }

    fn begin(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        view: Option<Opened>,
        work: Work,
        idle: bool,
    ) {
        // A build or a check is checked against its certificate here, before it is a job
        // ([`Jobs::certify_begin`]): answered, or waiting for looks at its outputs, it is
        // none yet.
        if let Some((view, work, walks)) = self.certify_begin(w, id, view, work, idle) {
            self.begin_job(w, id, view, work, idle, walks);
        }
    }

    /// Job `id` becomes a job: its request was not certified (`walks`: why, as the input
    /// `blaze why` names, with the certificate's base), or is no build or check.
    fn begin_job(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        view: Option<Opened>,
        work: Work,
        idle: bool,
        walks: Option<(String, ContentId)>,
    ) {
        let refused = self.refused.take();
        let views = match (&work, view) {
            (Work::Reach { from, .. }, Some(to)) => vec![*from, to], // alloc: once per job
            (_, Some(view)) => vec![view],                           // alloc: once per job
            (_, None) => Vec::new(),                                 // alloc: none
        };
        let keep_going = keeps_going(&work);
        let snapshot = matches!(work, Work::Snapshot);
        let job = Job::fresh(work, views, idle, walks, w.now);
        if self.jobs.put(key_of(id), job).is_err() {
            // alloc: an error the session's free list can't give
            let error = ApiError::new(
                format!("job slot {} is still another job's: it was handed out twice", id.slot),
                None,
            );
            w.said.push(Said::Finish(id, false, Some(error), None));
            return;
        }
        self.order.push(id);
        if snapshot {
            let stage = self.snapshot(w);
            if let Some(job) = self.jobs.get_mut(key_of(id)) {
                job.stage = stage;
            }
            return;
        }
        let request = w.plane.open(w.now, keep_going, idle);
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.request = Some(request);
            job.opening();
            self.started.push(job.row(request, None, profile_history::Outcome::Running));
            self.erring.insert(
                id,
                Erring {
                    build: u64::from(request.0),
                    command: command_line(&job.work),
                    rows: 0,
                    cancelled: Arc::clone(&job.cancelled), // clone: a refcount, once a job
                },
            );
        }
        // On the loop from a request's start to its first task posted (the library's, the
        // plan's, the lowering's): its view opened in the database, its config made, the
        // checkout's kept answer compared. Named, so a build's account has no stretch of the
        // loop before its first task that is only `loop turn` (slow-is-p0-traced).
        let _span = tracing::info_span!(
            target: "blaze",
            "loop: a job begun, to its first task posted",
            request = request.0,
        )
        .entered();
        // Why this build walks though it is one the no-op lane answers: said here, inside
        // the build, where its profile has it. A refusal made before the request had its
        // build (in `answer_again`) was in no build's rows, and a walk with no word of the
        // lane could not be told from one the lane was never asked of (2026-11-12, a
        // repeated `blaze run`: 1,162 jobs and no no-op span). The reason of an answer not
        // kept is the same span of the next build: `no answer is kept: <why>`.
        if let Some(why) = refused {
            tracing::info_span!(
                target: "blaze",
                "no-op answer: not given because",
                request = request.0,
                why = %why,
            )
            .in_scope(|| {});
        }
        self.prepare(w, id);
    }

    /// A job of declared intent run from now on as one started: its plane request takes its
    /// share. Once; a job gone, or not idle, is let be.
    fn promote_job(&mut self, w: &mut World<'_, S>, id: JobId) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if !job.idle {
            return;
        }
        job.idle = false;
        if let (Some(request), false) = (job.request, job.finished) {
            w.plane.promote(w.now, request);
        }
    }

    /// Makes the next view of the job ready, the standard library first.
    fn prepare(&mut self, w: &mut World<'_, S>, id: JobId) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if self.library.is_none() {
            let steps = w.steps.clone(); // clone: a refcount, once
            let kept = w.mounts.libraries(w.vfs);
            let work = Step::of(&job.cancelled, Does::Library { steps, kept });
            let name = "the standard library's source";
            let fresh =
                lone(w.plane, NodeKind::Std, name, Pool::Io, Some(library_key()), kinds::LIBRARY);
            job.stage = Stage::Library(fresh.first());
            self.add(w, id, fresh, Some(work), false);
            return;
        }
        let Some(&opened) = job.views.get(job.ready.len()) else {
            self.end(w, id, false, Some(ApiError::new("the job has no view to build", None)));
            return;
        };
        // Paths outside the view read where they are on the disk ([`Mounts::mount_local`]):
        // each the watch can't vouch for is read again before the view's configuration takes
        // its mount, so the build reads what is on the disk now, never a stale copy.
        let ats = w.mounts.stale_locals(vouched_local, super::fetch::local::vouched_file, super::fetch::local::absent);
        if !ats.is_empty() {
            let work = Step::of(&job.cancelled, Does::Locals { steps: w.steps.clone(), ats }); // clone: a refcount, once
            let fresh = lone(w.plane, NodeKind::Fetch, "paths outside the view, read again", Pool::Io, None, kinds::FETCH);
            job.stage = Stage::Locals(fresh.first(), opened);
            self.add(w, id, fresh, Some(work), false);
            return;
        }
        self.open(w, id, opened, HashSet::new_in(Global));
    }

    /// What job work `work` on the view `opened` is built with, as the mounts are now: where
    /// its root is, the checkout that is when it is one later builds are of again, its
    /// config, the config's encoding, and the key its plan is kept by.
    fn viewed(
        setup: &Setup,
        roots: &mut Roots<S>,
        w: &World<'_, S>,
        work: &Work,
        opened: &Opened,
        library: String,
    ) -> Viewed {
        let targets = targets_of(work);
        let root_dir = match targets.and_then(|it| it.cargo.as_ref()) {
            Some(cargo) => cargo.root.clone(), // clone: the view's root directory, once per view
            None => w
                .heads
                .path_of(&*w.vfs, opened.view.root)
                // alloc: an agent's view's directory, once per view
                .unwrap_or_else(|| format!("{}/{:x}", setup.views, opened.view.root)),
        };
        let root = lasting(&setup.views, &root_dir).then(|| roots.intern(&root_dir));
        // clone: the config owns its mounts, once per view
        let mut mounts =
            vec![Mount { at: root_dir.clone(), origin: Origin::View, under: String::new() }];
        mounts.extend(w.mounts.all().cloned());
        mounts.sort_by(|a, b| a.at.cmp(&b.at));
        let mut config = BuildConfig {
            invocation: invocation(work, targets, &root_dir, &setup.env),
            mounts,
            host: setup.host.clone(), // clone: the config owns it, once per view
            library,
            std: setup.std.clone(), // clone: the config owns it, once per view
            builds: setup.builds.clone(), // clone: the config owns it, once per view
            sources: setup.sources.clone(), // clone: the config owns it, once per view
            blaze: crate::release::get().version.to_owned(), // alloc: the config owns it, once per view
        };
        // The no-op lane's identity of the config holds no variable: of the environment it
        // compares what its answer's walk read ([`noop::env_moved`]). Encoded with the
        // environment taken out and put back, so nothing of it is copied.
        let env = std::mem::take(&mut config.invocation.env);
        let bare = config_id(&config.encode());
        config.invocation.env = env;
        // The plan's key and its view's configuration hold, of the request's environment,
        // what the load reads alone ([`BuildConfig::planned`]): another pane's variables are
        // the same plan in the same view.
        let planned = config.planned();
        let key =
            Hasher::new("blaze v2 serve last plan").id(&opened.view.root).part(&planned).finish();
        let shape = shape_of(&planned);
        Viewed { root_dir, root, config, bare, planned, key, shape }
    }

    /// The view opened in the database with its build configuration as the mounts are now, or
    /// the head's last plan when it was of this view and configuration; then its plan.
    fn open(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        opened: Opened,
        asked: HashSet<String, Global>,
    ) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        // clone: the config owns the library's place, once per view
        let Some(library) = self.library.clone() else {
            let error =
                ApiError::new("a view was opened before the standard library was mounted", None);
            self.end(w, id, false, Some(error));
            return;
        };
        let Viewed { root_dir, root, config, bare, planned, key, shape } =
            Self::viewed(&self.setup, &mut self.roots, w, &job.work, &opened, library);
        // A view whose plan is known to write lock files into it (a build of this very view
        // did, and its plan is kept): the files are written into the view again and the
        // view opened with them, asked of no load ([`LastPlan::asked`]).
        if job.locked.is_none()
            && reuses_plans(&job.work)
            && let Some(at) = root
        {
            let known = self.roots.plans[at as usize].iter().find_map(|plan| {
                // clone: the lock files' text, once a build of a view that lacks them
                plan.asked.as_ref().filter(|(was, _)| *was == key).map(|(_, lock)| lock.clone())
            });
            if let Some(written) = known
                && let Some(ops) = Self::lock_ops(&*w.vfs, &root_dir, &opened, &written)
                && let Ok(advanced) = w.vfs.apply_owned(opened.view.root, ops)
                && advanced != opened.view.root
            {
                self.open_locked(w, id, opened, advanced, key, written, asked, &root_dir);
                return;
            }
        }
        let (request, drawn) = (request_id(&job.work), drawn_id(&job.work));
        // A blaze that keys as another toolchain (`BLAZE_DEV_TOOLCHAIN_AS`) says so before
        // anything else of each build or check, and refuses one whose outputs may be shipped.
        if request.is_some()
            && let Some(dev) = super::dev_toolchain::dev()
        {
            let invocation = &config.invocation;
            if let Some(refused) = dev.refuses(invocation.command.name(), &invocation.args) {
                self.end(w, id, false, Some(ApiError::new(refused, None)));
                return;
            }
            let warning = Diagnostic::warning(dev.said());
            w.said.push(Said::Report(id, Response::Diagnostic(warning)));
        }
        let config = Arc::new(config); // alloc: once per view, shared with its tasks
        // A build or a check of a checkout that walks drops the checkout's kept answer of
        // its own request shape, and may become the next ([`Jobs::record_answer`]). No other
        // shape's answer goes, and other work drops none: what a walk changes that an answer
        // rests on (an output placed in a directory of its outputs, a lock file written, a
        // source mounted) is what that answer's own check finds moved, as it would a
        // stranger's change ([`noop::Answers`]). Until 2026-11-12 any job of the checkout
        // dropped its one answer, so `build`, `run`, `build` walked three times.
        if let (Some(at), Some(request)) = (root, request) {
            let inputs = Inputs {
                view: opened.view.root,
                config_id: bare,
                config: Arc::clone(&config), // clone: a refcount, once per view
                request,
                drawn,
            };
            // Heard from here to its end for its request's certificate, whatever it makes
            // ([`Jobs::certify_building`]).
            self.certify_building(id, at, shape, inputs);
        }
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        // A build or a check takes what settling lost of its checkout's last build, to say it.
        if request.is_some()
            && let Some(at) = root
        {
            // Taken once: a view opened again (after a fetch, or a lock file written into it)
            // keeps what the first opening took.
            if let Some(lost) = self.roots.lost[at as usize].take() {
                job.lost = Some(lost);
            }
            // And what its last build left by not ending well: said now, the first thing
            // this build says, before any of that work is done.
            // clone: once a build after one that did not end well
            if job.after.is_none()
                && let Some(after) = self.roots.unfinished[at as usize].clone()
            {
                let note = Diagnostic::note(after.said(&root_dir));
                w.said.push(Said::Report(id, Response::Diagnostic(note)));
                job.after = Some(after);
            }
        }
        // clone: a path, once a view
        job.checkout = root.map(|at| (at, root_dir.clone()));
        if let Some(at) = root {
            use_plan(&mut self.roots.plans[at as usize], key);
            // A view this build wrote its lock files into, whose plan was kept already: the
            // plan learns which view leads to it.
            if let Some(locked) = &job.locked
                && let Some(last) = self.roots.plans[at as usize].last_mut()
                && last.key == key
                && last.asked.is_none()
            {
                last.asked = Some(locked.clone()); // clone: the lock files' text, once
            }
        }
        if reuses_plans(&job.work)
            && let Some(at) = root
            && let Some(last) = self.roots.plans[at as usize].last()
            && last.key == key
        {
            job.borrowed.push(last.queries);
            // clone: a refcount and the lock's text, once per view
            let last_units = Some((last.units.clone(), last.lock.clone()));
            let prep = Prep {
                opened,
                root_dir,
                root,
                config,
                key,
                shape,
                queries: last.queries,
                asked,
                last: last_units,
            };
            self.plan(w, id, prep);
            return;
        }
        let queries = w.db.open_with(opened.view, &planned);
        job.queries.push(queries);
        let prep = Prep { opened, root_dir, root, config, key, shape, queries, asked, last: None };
        self.plan(w, id, prep);
    }

    /// Job `id`, asked on `view` for `work`, answered as its checkout's last build that made
    /// nothing was, when it is a build or a check of a checkout whose kept answer still
    /// stands ([`noop::ask`]): what that one said is said again, its `Work` with no build id
    /// (no row of the history stands for it, so its client sends none), and it finishes ok.
    /// It never becomes a job: no request of the plane, no plan, no lowering, no task, no
    /// store, no commit to wait for. The answer said again is memory's, of this daemon's own
    /// life, as what it rests on is: usable at once, whether or not that is durable yet, and
    /// gone with the daemon, so no restart relies on it ([`Settling`]). All of it is the span
    /// `no-op answer`, its checks the span's children.
    ///
    /// Else it walks: why, as the input `blaze why` names ([`noop::NotReplayed::input`]) with the
    /// id of the request's inputs; none for other work, and where the walk's own work says
    /// why.
    ///
    /// Not called since 2026-11-12: a request's first state is its certificate
    /// ([`Jobs::certify_begin`]). To be deleted with the kept answers it asks
    /// (certificate.md, "State").
    #[allow(dead_code)]
    fn answer_again(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        view: Option<&Opened>,
        work: &Work,
    ) -> Result<(), Option<(String, ContentId)>> {
        self.refused = None;
        let (Some(opened), Some(request)) = (view, request_id(work)) else { return Err(None) };
        // The standard library is mounted by a daemon's first build, which keeps no answer.
        // clone: the config owns the library's place, once a request
        let Some(library) = self.library.clone() else {
            // alloc: a reason, once a daemon's first build
            self.refused = Some("the standard library is not mounted yet: this is the daemon's first build".to_owned());
            return Err(None);
        };
        let _answering = tracing::info_span!(target: "blaze", "no-op answer").entered();
        let keying =
            tracing::info_span!(target: "blaze", "no-op answer: the request's inputs").entered();
        let Viewed { root, config, bare, shape, .. } =
            Self::viewed(&self.setup, &mut self.roots, w, work, opened, library);
        let Some(root) = root else {
            // alloc: a reason, once a request on an agent's view
            self.refused = Some("its view is no checkout's: an agent's view keeps no answer".to_owned());
            return Err(None);
        };
        let inputs = Inputs {
            view: opened.view.root,
            config_id: bare,
            config: Arc::new(config), // alloc: once a request, kept with a walk's recording
            request,
            drawn: drawn_id(work),
        };
        drop(keying);
        // The files that differ from the view of the kept answer of this request's shape,
        // where the root is another.
        let read = self.edited(w, root, &shape, opened);
        let comparing = tracing::info_span!(
            target: "blaze",
            "no-op answer: compare with the kept answer",
            root,
        )
        .entered();
        let traced = blaze_units::query::env_var(
            &inputs.config.invocation.env,
            crate::cli::client_rows::PROFILE,
        )
        .is_some_and(|it| !it.is_empty());
        // What answered jobs left to settle is no job: the kept answer is of a build that
        // made nothing, and stands while that settles.
        let alone = self.order.is_empty();
        let stale = w.mounts.stale_locals(vouched_local, super::fetch::local::vouched_file, super::fetch::local::absent);
        let locals_vouched = stale.is_empty();
        let edited = match &read {
            Ok(edits) => Edited::Files(edits),
            Err(why) => Edited::Unread(*why),
        };
        // Another request's output: a path the daemon planned an output at (every one is
        // interned as its plan is built, [`Paths`]) that is no output of the units of this
        // request's own plan. With no plan kept of the shape, none is known to be.
        let (plans, planned) = (&self.roots.plans[root as usize], &self.paths.ids);
        let mine = plans.iter().find(|it| it.shape == shape).map(|it| &it.units);
        let other_output = |path: &std::path::Path| {
            let (Some(mine), Some(path)) = (mine, path.to_str()) else { return false };
            planned.contains_key(path)
                && !mine.plans.iter().any(|plan| plan.outputs.iter().any(|it| it.as_str() == path))
        };
        let asked = NoopAsked {
            inputs: &inputs,
            edited,
            traced,
            alone,
            locals_vouched,
            unasked: &read_unasked,
            other_output: &other_output,
        };
        // Asked of this shape's answer alone: no other request's walk dropped it, and of the
        // request's environment only what its walk read is compared.
        let found = self.roots.answers[root as usize].ask(&shape, &asked, &Watched);
        drop(comparing);
        let (responses, worked) = match found {
            Ok(found) => found,
            Err(why) => {
                tracing::info_span!(
                    target: "blaze",
                    "no-op answer: not given, the build walks",
                    root,
                    why = %why,
                )
                .in_scope(|| {});
                // alloc: a reason, once a build that walks. A path dependency no watch
                // vouches for is named, with its watch's state as the watches say it
                // ([`blaze_vfs::outside::why_looked`]): which path, and why not.
                self.refused = Some(match (&why, stale.first()) {
                    (noop::NotReplayed::Locals, Some(at)) => format!(
                        "{why}: {at} ({}); {} such in all",
                        blaze_vfs::outside::why_looked(std::path::Path::new(at)),
                        stale.len()
                    ),
                    _ => why.to_string(),
                });
                return Err(why.input().map(|input| (input, inputs.id())));
            }
        };
        let _saying = tracing::info_span!(
            target: "blaze",
            "no-op answer: say the kept answer again",
            root,
            responses = responses.len(),
        )
        .entered();
        for response in responses.iter() {
            // clone: what the session sends, once a response said again
            w.said.push(Said::Report(id, response.clone()));
        }
        if let Some(worked) = worked {
            // No build of its own: no id in the history, no time of nodes it never ran.
            let worked = Worked {
                why: None,
                build_id: None,
                steps_us: Vec::new(), // alloc: none
                critical_path_us: 0,
                placed: blaze_api::PlacedBytes::default(),
                ..worked
            };
            w.said.push(Said::Report(id, Response::Work(worked)));
        }
        w.said.push(Said::Finish(id, true, None, None));
        Ok(())
    }

    /// The files that differ between the view checkout `root`'s kept answer of `shape` is of and
    /// `opened`, each with its still text in both where it is a source the answer's walk held
    /// by code text ([`noop::FileEdit`]); or why they are not read ([`Edited::Unread`]): no
    /// answer is kept, the view is one the answer is known of, its walk vouched for no
    /// source, its view is not held, or more files differ than the lane reads.
    ///
    /// The cost is the edit's: a diff of the two roots, which descends only where they
    /// differ, and for each changed source one parse of it as it is now and, the first time
    /// it differs, one of it as it was (`blaze_lower::still_text`: no database, no query).
    /// Bounded by [`noop::MOST_EDITS`] files of [`noop::MOST_BYTES`] each; all of it is the
    /// span `no-op answer: read the files that differ`, each parse a child.
    fn edited(
        &mut self,
        w: &World<'_, S>,
        root: u32,
        shape: &ContentId,
        opened: &Opened,
    ) -> Result<Vec<FileEdit>, &'static str> {
        let at = root as usize;
        let Some((answer, held)) = self.roots.answers[at].held_mut(shape) else {
            return Err("no answer is kept");
        };
        if answer.files_known(&opened.view.root) {
            return Err("the view is one the kept answer is known of");
        }
        let Ok(still) = &answer.still else {
            return Err("the kept answer's build vouched for no source");
        };
        let Some(walked) = held.view else {
            return Err("the kept answer's view is not held");
        };
        let was = answer.inputs.view;
        if !w.vfs.has(was) || !w.vfs.has(opened.view.root) {
            return Err("a root of the two views is not in the tree");
        }
        let reading = tracing::info_span!(
            target: "blaze",
            "no-op answer: read the files that differ",
            root,
            files = tracing::field::Empty,
        )
        .entered();
        let changes = w.vfs.diff(was, opened.view.root);
        reading.record("files", changes.len());
        if changes.len() > noop::MOST_EDITS {
            return Err("more files differ than the lane reads");
        }
        let stills = &mut held.stills;
        let mut edits = Vec::with_capacity(changes.len()); // alloc: once a request on another root, a file or a few
        for Change { kind, path } in changes {
            let held = still.binary_search_by(|it| it.path.as_str().cmp(path.as_str())).ok();
            let texts = match (kind, held) {
                (blaze_vfs::ChangeKind::Modified, Some(held)) => {
                    let edition = still[held].edition;
                    let was = match stills.iter().find(|(it, _)| *it == path) {
                        Some((_, was)) => *was,
                        None => {
                            let was = still_in(&*w.vfs, walked, &path, edition);
                            stills.push((path.clone(), was)); // clone: a path, once a file that differs
                            was
                        }
                    };
                    was.zip(still_in(&*w.vfs, opened.files, &path, edition))
                }
                _ => None,
            };
            edits.push(FileEdit { path, still: texts });
        }
        Ok(edits)
    }

    /// Job `id` noted as one that may become checkout `root`'s kept answer of `shape`: when it is about
    /// to walk the checkout's plan of that shape (of `key`, writing no lock file) and each directory
    /// its outputs are in is named canonically, as the watches are asked. The watches'
    /// generation and the count of looks at the disk are read here, before the job looks at
    /// anything. Else the checkout's answer says why none is kept ([`Unkept`]).
    #[allow(dead_code)]
    fn record_answer(
        &mut self,
        id: JobId,
        root: u32,
        shape: ContentId,
        key: ContentId,
        inputs: Inputs,
    ) {
        // A job whose view is opened again (after a fetch, or with the lock file it wrote)
        // is recorded anew: the view its first recording held is let go.
        if let Some(at) = self.recordings.iter().position(|it| it.job == id) {
            let recorded = self.recordings.swap_remove(at);
            self.unheld.extend(recorded.view);
        }
        let (generation, looks) = (blaze_vfs::outside::generation(), blaze_vfs::outside::looks());
        let planned = self.roots.plans.get(root as usize).and_then(|kept| kept.iter().find(|it| it.key == key));
        let found = match planned {
            Some(last) if last.lock.is_none() => recorded_dirs(&last.units, &self.setup.std),
            _ => Err(Unkept::Planned),
        };
        match found {
            Ok((dirs, absent)) => self.recordings.push(Recorded {
                job: id,
                root,
                shape,
                recording: Recording::new(inputs, generation, looks, dirs, absent),
                view: None,
                walked: None,
            }),
            Err(why) => self.keep_last(root, shape, Err(why), None),
        }
        // The path of each look is kept while a build that may be answered again walks, so
        // a walk that is not kept for a look names the path.
        blaze_vfs::outside::name_looks(!self.recordings.is_empty());
    }

    /// `last` is checkout `root`'s answer of `shape` from now, or why it has none, with
    /// `held` beside it ([`noop::Answers::keep`]). The views held beside whatever went (that
    /// shape's last, and past the bound the least recently used shape's) are closed at the
    /// loop's next step ([`Jobs::unheld`]).
    fn keep_last(&mut self, root: u32, shape: ContentId, last: Last, held: Option<WalkedView>) {
        let gone = self.roots.answers[root as usize].keep(shape, last, held);
        self.unheld.extend(gone.into_iter().filter_map(|it| it.view));
    }

    /// What the daemon tells the session of a job ([`Said`]), noted for the job's
    /// [`Recording`] when it has one: each response a build with nothing to do says, in
    /// order, and at its `Finish` the recording kept or dropped ([`Jobs::keep_answer`]). The
    /// daemon calls it for every `Said` as it is told, so nothing a job said is missed.
    pub fn heard(&mut self, said: &Said) {
        #[cfg(test)]
        let before = self.errors.len();
        #[cfg(test)]
        let planted = PLANT_NO_ERROR_ROWS.get();
        #[cfg(not(test))]
        let planted = false;
        if !planted {
            self.note_errors(said);
        }
        #[cfg(test)]
        ERROR_ROWS.with_borrow_mut(|it| {
            // clone: a test's record of each row
            it.extend(
                self.errors[before..]
                    .iter()
                    .map(|row| (row.build_id, row.kind.clone(), row.message.clone())),
            )
        });
        self.certify_heard(said);
        if self.recordings.is_empty() {
            return;
        }
        match said {
            Said::Report(id, response) => {
                if let Some(it) = self.recordings.iter_mut().find(|it| it.job == *id) {
                    it.recording.hear(response);
                }
            }
            Said::Finish(id, ok, error, _) => {
                let Some(at) = self.recordings.iter().position(|it| it.job == *id) else {
                    return;
                };
                let recorded = self.recordings.swap_remove(at);
                self.keep_answer(recorded, *ok && error.is_none());
                blaze_vfs::outside::name_looks(!self.recordings.is_empty());
            }
        }
    }

    /// The history's `errors` rows of what a job says ([`Jobs::errors`]): one for each error
    /// diagnostic (or a row a function, for an error naming the functions blaze could not
    /// compile), and one for the error its `Finish` carries. Every word of a job passes
    /// [`Jobs::heard`] once, so no error a build said is missed, whichever step said it.
    ///
    /// A build that ends not ok having said no error is given a row saying so, naming the
    /// build and its command: `builds.outcome = 'failed'` with nothing in `errors` (the cold
    /// build of ripgrep, 2026-11-11, when nothing in the daemon wrote the table at all) is
    /// a failure with no cause to read, and that is itself the thing to fix (rule 19). Not
    /// for a cancelled build: it did not fail.
    fn note_errors(&mut self, said: &Said) {
        use blaze_profile::errors;
        let now = Some(blaze_profile::clock::mono_ns());
        match said {
            Said::Report(id, Response::Diagnostic(diagnostic))
                if diagnostic.level == Level::Error =>
            {
                let Some(erring) = self.erring.get_mut(id) else { return };
                let before = self.errors.len();
                let place = diagnostic.spans.iter().find(|it| it.primary).or(diagnostic.spans.first());
                let detail = diagnostic
                    .children
                    .iter()
                    .find(|it| matches!(it.level, Level::Help | Level::Note))
                    .map(|it| it.message.as_str());
                // alloc: an error's unit as cargo names it, once an error said
                let unit = diagnostic.unit.as_ref().map(|unit| {
                    let kinds: Vec<&str> = unit.target.kinds().collect(); // alloc: as above
                    let test = if unit.test { " test" } else { "" };
                    format!("{} ({}{test})", unit.target.name, kinds.join(","))
                });
                errors::rows(
                    errors::Said {
                        code: diagnostic.code.as_deref(),
                        message: &diagnostic.message,
                        detail,
                        file: place.map(|it| it.file.as_str()),
                        line: place.map(|it| it.line),
                    },
                    errors::Of {
                        build_id: erring.build,
                        t_ns: now,
                        krate: diagnostic.unit.as_ref().map(|it| it.target.name.as_str()),
                        unit: unit.as_deref(),
                    },
                    &mut self.errors,
                );
                erring.rows += u32::try_from(self.errors.len() - before).unwrap_or(u32::MAX);
            }
            Said::Finish(id, ok, error, _) => {
                let Some(erring) = self.erring.remove(id) else { return };
                let of =
                    errors::Of { build_id: erring.build, t_ns: now, krate: None, unit: None };
                if let Some(error) = error {
                    errors::rows(
                        errors::Said {
                            code: None,
                            message: &error.what,
                            detail: None,
                            file: error.at.as_deref(),
                            line: None,
                        },
                        of,
                        &mut self.errors,
                    );
                } else if !*ok && erring.rows == 0 && !erring.cancelled.load(Ordering::Acquire) {
                    // alloc: a failure with no error said, once
                    let what = format!(
                        "build {} (`{}`) failed and said no error: what failed is in none of its \
                         diagnostics and not in its answer (a bug in blaze: every failure names what failed)",
                        erring.build, erring.command
                    );
                    tracing::error!(target: "blaze", build = erring.build, "{what}");
                    errors::rows(
                        errors::Said { code: None, message: &what, detail: None, file: None, line: None },
                        of,
                        &mut self.errors,
                    );
                }
            }
            Said::Report(..) => {}
        }
    }

    /// `recorded`'s job ended, `ok` or not: its answer kept as its checkout's of its request's shape when it passed,
    /// made nothing (no unit compiled, no miss, no output placed, nothing shared), said only
    /// what such a build says, and looked at no path outside every view on the disk while it
    /// ran ([`Recording::end`]). Else the checkout's answer says why none is kept; either
    /// way in a span.
    fn keep_answer(&mut self, recorded: Recorded, ok: bool) {
        let Recorded { root, shape, recording, view, walked, .. } = recorded;
        let (made_nothing, worked) = walked.unwrap_or((false, None));
        // Each path the walk looked at on the disk, with how often: one look is enough to
        // keep the next build from being answered again, and the refusal names the last alone.
        let files = blaze_vfs::outside::take_file_looks();
        let looked = blaze_vfs::outside::take_named_looks();
        if !looked.is_empty() {
            // alloc: once a build that looked, the paths as text
            let paths: Vec<String> =
                looked
                    .iter()
                    .map(|(path, times, by)| {
                        format!("{} ({times} by {by}: {})", path.display(), blaze_vfs::outside::why_looked(path))
                    })
                    .collect();
            tracing::info!(
                target: "blaze",
                root = %root,
                paths = %paths.join(", "),
                "the build looked at paths outside every view on the disk, which no watch covers"
            );
        }
        let last = recording.end(Walk {
            passed: ok,
            made_nothing,
            looks: blaze_vfs::outside::looks(),
            looked_at: blaze_vfs::outside::last_look(),
            files,
            worked,
        });
        match &last {
            Ok(answer) => tracing::info_span!(
                target: "blaze",
                "no-op answer: the build's answer is kept",
                root,
                responses = answer.responses.len(),
                dirs = answer.dirs.len(),
                // What keeping it holds of the heap, about ([`noop::LastAnswer::held_bytes`]),
                // and whether the variables its steps read are named (else every variable
                // of a later request is compared).
                bytes = answer.held_bytes(),
                env_named = answer.named.is_ok(),
            )
            .in_scope(|| {}),
            Err(why) => tracing::info_span!(
                target: "blaze",
                "no-op answer: the build's answer is not kept",
                root,
                why = %why,
            )
            .in_scope(|| {}),
        }
        // The view of the files it walked stays held beside an answer kept, and goes
        // with one that is not.
        let held = match &last {
            Ok(_) => Some(WalkedView { view, stills: Vec::new() }), // alloc: none
            Err(_) => {
                self.unheld.extend(view);
                None
            }
        };
        self.keep_last(root, shape, last, held);
        // What the checkout keeps now, for whoever budgets it: how many shapes have an
        // answer, and about what they hold of the heap together.
        let answers = &self.roots.answers[root as usize];
        tracing::info_span!(
            target: "blaze",
            "no-op answer: the checkout's kept answers",
            root,
            kept = answers.kept().count(),
            bytes = answers.held_bytes(),
        )
        .in_scope(|| {});
    }

    /// The view's plan asked, or taken from the head's last one.
    fn plan(&mut self, w: &mut World<'_, S>, id: JobId, mut prep: Prep) {
        if let Some((units, lock)) = prep.last.take() {
            self.ready(w, id, prep, units, lock);
            return;
        }
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let (steps, db, queries) = (w.steps.clone(), w.db.clone(), prep.queries); // clone: two refcounts, once per plan
        let work = Step::of(&job.cancelled, Does::Plan { steps, db, queries });
        let name = named("the plan of ", &prep.root_dir);
        let fresh = lone(w.plane, NodeKind::Manifest, &name, Pool::Work, None, kinds::PLAN);
        job.stage = Stage::Plan(fresh.first(), prep);
        self.add(w, id, fresh, Some(work), false);
    }

    /// A batch of the job's nodes; with `reads`, its tasks read the job's front end.
    fn add(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        fresh: Fresh,
        work: Option<Step<S>>,
        reads: bool,
    ) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let Some(request) = job.request else {
            self.end(
                w,
                id,
                false,
                Some(ApiError::new(
                    "the job has no request of the plane to add its tasks to",
                    None,
                )),
            );
            return;
        };
        let (first, count) = (fresh.first(), fresh.count());
        if count > 0 {
            self.batches.insert(first.0, count, id);
        }
        if let Err(error) = w.plane.add(w.now, request, fresh, work, reads) {
            // alloc: an error, once
            self.end(
                w,
                id,
                false,
                Some(ApiError::new(format!("the plane refused the job's tasks: {error}"), None)),
            );
        }
    }

    /// A task of a batch that reads posted with readers of its job's front ends as they stand
    /// now, after every write the loop applied before it. While a write of its job waits for
    /// the readers before it, the task is held and posted once the write is applied
    /// ([`Jobs::apply_writes`]). A compile is given the images kept at its outputs.
    fn post(&mut self, w: &mut World<'_, S>, task: Posting, again: bool) {
        let job = self.batches.job_of(task.id).and_then(|id| self.jobs.get_mut(key_of(id)));
        let readers = match job {
            Some(Job { stage: Stage::Units(build), .. }) => {
                if let Some(unit) = build.unit(task.id)
                    && let Some(why) = build.refused.as_ref().and_then(|refused| refused.why(unit))
                {
                    #[cfg(test)]
                    REFUSED_UNITS.with_borrow_mut(|it| it.push((unit, why.to_owned()))); // alloc: a test's record
                    // alloc: a refused task's why, once
                    w.plane.refuse(w.now, task, why.to_owned());
                    return;
                }
                let Some(this) = build.writer.reader() else {
                    build.held.push((task, again));
                    return;
                };
                let images = match task.kind == kinds::UNIT_LINK || task.kind == kinds::PROGRAM_LINK
                {
                    true => self.paths.take_images(build.outputs.of(Phase::of(task.arg).0)),
                    false => Vec::new(), // alloc: none
                };
                // Said before it is posted, so it is in before the task can end.
                if build.unit(task.id).is_some_and(|at| build.is_fresh(at)) {
                    w.plane.replays(task.id);
                }
                Some(Readers { this: Some(this), images, ..Readers::default() })
            }
            Some(job) if matches!(job.stage, Stage::Answer(_) | Stage::Ahead(_)) => {
                match job.work {
                    Work::Tables | Work::Doc { .. } => job
                        .ready
                        .last_mut()
                        .and_then(|it| it.writer.take())
                        .map(|writer| Readers { writer: Some(writer), ..Readers::default() }),
                    _ => (|| {
                        let this = job.ready.last()?.writer.as_ref()?.reader()?;
                        let from = match job.work {
                            Work::Reach { .. } => {
                                Some(job.ready.first()?.writer.as_ref()?.reader()?)
                            }
                            _ => None,
                        };
                        Some(Readers { this: Some(this), from, ..Readers::default() })
                    })(),
                }
            }
            _ => None,
        };
        match readers {
            Some(readers) => w.plane.post(task, again, readers),
            // alloc: a refused task, once
            None => w.plane.refuse(w.now, task, "its job has no front end to read now".to_owned()),
        }
    }

    /// What each kept front end's query storage holds, by ingredient, a line a front end in
    /// the daemon's log: the idle daemon's answer to "what holds the memory" (rule
    /// ask-the-profile-first). A walk of every function's rows made, sound beside readers
    /// (`blaze_query::db::Storage::memory_usage`), so only where nothing waits on it: as the
    /// daemon gives its warm memory back ([`super::idle`]).
    pub fn say_what_the_kept_front_ends_hold(&self) {
        use std::fmt::Write as _;
        const SHOWN: usize = 48;
        let mib = |bytes: u64| bytes as f64 / f64::from(1u32 << 20);
        for (root, &id) in self.roots.ids.iter() {
            let Some(db) = self.roots.fronts[id as usize].as_ref().and_then(KeptDb::db) else {
                continue;
            };
            let _span = tracing::info_span!(target: "blaze", "idle: what the kept front end holds").entered();
            let mut usage = Vec::new(); // alloc: an idle daemon's report, an entry an ingredient
            blaze_query::db::Database::storage(db).memory_usage(&mut usage);
            usage.sort_by(|a, b| b.held().cmp(&a.held()).then(a.name.cmp(b.name)));
            let total: u64 = usage.iter().map(blaze_query::db::MemoryUsage::held).sum();
            let mut said = String::new(); // alloc: as above, one line
            for it in usage.iter().take(SHOWN) {
                let _ = write!(
                    said,
                    "; {} {:.1} (rows {:.1}, values {:.1}; {} rows made, {} used)",
                    it.name,
                    mib(it.held()),
                    mib(it.row_bytes),
                    mib(it.value_bytes),
                    it.rows,
                    it.used,
                );
            }
            crate::serve::said::logged!(INFO,
                "blaze serve: idle: the kept front end of {root} holds {:.1} MiB of query storage in {} ingredients, in MiB{said}",
                mib(total),
                usage.len(),
            );
        }
    }

    /// The system says memory is short (docs/memory-budget.md section 7, "Pressure"): the
    /// kept front ends let go of the values they keep by bytes, on a worker, from the loop's
    /// next turn ([`Jobs::press_kept`]), none of them whole, and each build's front end is asked for a release now
    /// ([`Writer::pressed`]), whatever this daemon's own budget says. Until 2026-11-12 every
    /// kept front end was let go here, as for any task held back for memory: the next edit
    /// of each checkout then lowered anew, which takes minutes (reth: 195 s for a one-line
    /// revert). A limit that falls under what the daemon holds is [`Jobs::give_back`]'s to
    /// answer, cheapest first.
    pub fn pressed(&mut self) {
        self.giving.press();
        for &id in &self.order {
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            if let Stage::Units(build) = &mut job.stage {
                build.writer.pressed();
            }
        }
    }

    /// Each job's waiting writes applied where no reader of its front end is left, and the
    /// tasks held for them posted with readers of what they made. Asking a job whose readers
    /// still run costs one load of its reference count. Called as the loop winds up each
    /// step: a reader is dropped as its task's work returns, before the plane hears the task
    /// ended, so the step that hears the last one applies the write. The daemon calls it
    /// once a turn as well, for a reader that outlives its task.
    pub fn apply_writes(&mut self, w: &mut World<'_, S>) {
        // What a task held back for memory waits for, and a limit under what the daemon
        // holds, are answered by the give-back rule: the allocator's freed pages first, a
        // kept front end last and never for a wait a cheaper give covers ([`Jobs::give_back`]).
        let short = w.plane.short();
        self.give_back(w, short);
        let order = std::mem::take(&mut self.order);
        let mut posts = std::mem::take(&mut self.posts);
        for &id in &order {
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            let Stage::Units(build) = &mut job.stage else { continue };
            if !build.writer.waiting() && build.held.is_empty() {
                continue;
            }
            let applied = build.writer.apply();
            // The first write that broke in this apply, for the held tasks a job that fails fast
            // refuses.
            let mut first: Option<String> = None;
            while let Some(broke) = build.writer.broke() {
                let error = build.refuse_after(&broke);
                first.get_or_insert_with(|| error.what.clone()); // clone: an error, once
                // A write that was part of making its unit (its harness or wasm macro glue)
                // fails that unit: said as the unit's own error, as its compile's would be,
                // so every unit refused this way is named, not only the job's first, and
                // which one the build names never hangs on which became ready first.
                if broke.itself {
                    if !job.cancelled.load(Ordering::Acquire) {
                        let what = format!("blaze could not make this unit: {}", error.what); // alloc: an error, once
                        w.said.push(Said::Report(
                            id,
                            Response::Diagnostic(build.front.refused(broke.unit, what)),
                        ));
                        if let Some(unit) = unit_of(&build.input.units, broke.unit) {
                            w.said.push(Said::Report(id, Response::UnitEnded(unit)));
                        }
                    }
                    continue;
                }
                if !job.cancelled.load(Ordering::Acquire) {
                    job.over.get_or_insert((false, Some(error)));
                }
            }
            // Without keep-going a write that broke ends the job now, as a writer's own error
            // does: the plane fails fast only once a refused task is posted, which waits for
            // that task's other dependencies, and meanwhile it would start every unit no
            // failure reaches.
            let why = match (applied, first) {
                (Err(error), _) => {
                    let why =
                        format!("applying a build script's run or a proc macro: {}", error.what); // alloc: an error, once
                    if !job.cancelled.load(Ordering::Acquire) {
                        job.over.get_or_insert((false, Some(error)));
                    }
                    why
                }
                (Ok(_), Some(broke)) if !keeps_going(&job.work) => {
                    format!("{broke}, and the build fails fast under --fail-fast") // alloc: an error, once
                }
                (Ok(true), _) => {
                    posts.append(&mut build.held);
                    continue;
                }
                (Ok(false), _) => continue,
            };
            for (task, _) in build.held.drain(..) {
                w.plane.refuse(w.now, task, why.clone()); // clone: a write that broke, once per task it held
            }
            self.end(w, id, false, None);
        }
        self.order = order;
        for (task, again) in posts.drain(..) {
            self.post(w, task, again);
        }
        self.posts = posts;
    }

    /// What the plane did.
    pub fn happened(&mut self, w: &mut World<'_, S>, happened: &mut Vec<Happened<Value<S>>>) {
        for it in happened.drain(..) {
            match it {
                Happened::Kept { id, puts } => self.kept(w, id, puts),
                Happened::Did { id, value } => self.did(w, id, value),
                Happened::Started { id } => self.started_unit(w, id),
                Happened::Post { task, again } => self.post(w, task, again),
                Happened::Run { id } => self.run_script(w, id),
                Happened::Panicked { id, panic } => self.panicked(id, &panic),
                Happened::Settled { id, ok, why, skipped } => self.settled(w, id, ok, why, skipped),
                Happened::Finished { request, ok, stats } => {
                    self.plane_finished(w, request, ok, stats)
                }
            }
        }
        self.wind_up(w);
    }

    /// A task of a build's units that panicked fails its job with the panic, naming the unit.
    /// A task of any other stage fails its job as it settles, the panic its `why`.
    fn panicked(&mut self, node: NodeId, panic: &plane::Panic) {
        let Some(job) = self.batches.job_of(node).and_then(|id| self.jobs.get_mut(key_of(id)))
        else {
            return;
        };
        let Stage::Units(build) = &job.stage else { return };
        let error = match build.unit(node).map(|at| &build.input.units.graph.units[at]) {
            Some(unit) => ApiError::new(
                format!("the daemon {panic}, building {} of {}", unit.target.name, unit.pkg_id), // alloc: an error, once
                Some(unit.pkg_id.clone()), // clone: an error, once
            ),
            None => ApiError::new(format!("the daemon {panic}, building the units"), None), // alloc: an error, once
        };
        job.over.get_or_insert((false, Some(error)));
    }

    /// Keeps what a task's expansions put, and the daemon's durable puts not kept yet
    /// (`Keeping::publish`), from the store's one owner: a refused one fails the task's job,
    /// naming the expansion.
    fn kept(&mut self, w: &mut World<'_, S>, node: NodeId, puts: Vec<proc_macro_api::kept::Put>) {
        let published = w.macros.keeps().publish(w.store, puts);
        for ticket in &published.tickets {
            self.put.add(*ticket);
        }
        if !published.durable.is_empty() {
            w.store.flush(w.effects);
            self.wait_durable(&published.durable);
        }
        let Some(refused) = published.refused.into_iter().next() else { return };
        if let Some(job) = self.batches.job_of(node).and_then(|id| self.jobs.get_mut(key_of(id))) {
            job.over.get_or_insert((false, Some(ApiError::new(refused, None))));
        }
    }

    /// A retraction was published (a [`proc_macro_api::kept::Keep::Durable`] put, flushed as it
    /// was): every job not ended yet has it settle after its answer ([`Settling::durable`]). A
    /// durable put goes to its daemon's own queue (`Keeping::put`), which only this owner takes
    /// (`Keeping::publish`, from `kept` or `plane_finished`), so whichever publish takes
    /// it calls this in the same step. It was put before the task that made it ended, which is
    /// before that job's `plane_finished`, whose publish takes the queue; and a job ends only
    /// after that (`Job::finished`, `wind_up`). So the job that made it is among those
    /// not ended here. The others wait for it too: one more flush, never a wrong answer.
    fn wait_durable(&mut self, tickets: &blaze_cas::Tickets) {
        for id in &self.order {
            if let Some(job) = self.jobs.get_mut(key_of(*id)) {
                job.durable.extend(tickets);
            }
        }
        // And what is settling: a store of a kept front end publishes after its job's answer.
        for it in &mut self.settling {
            it.durable.extend(tickets);
        }
    }

    /// The images unit `unit`'s link relinked, kept at its outputs for its next link.
    fn keep_images(&mut self, node: NodeId, unit: usize, images: Vec<Image<S>>) {
        if images.is_empty() {
            return;
        }
        let build =
            self.batches.job_of(node).and_then(|id| match &self.jobs.get(key_of(id))?.stage {
                Stage::Units(build) => Some(build),
                _ => None,
            });
        self.paths.keep(build.map_or(&[][..], |it| it.outputs.of(unit)), images);
    }

    fn did(&mut self, w: &mut World<'_, S>, node: NodeId, value: Value<S>) {
        match value {
            Value::Library(Ok(Library { at, fingerprint, source })) => {
                let mounted = match source {
                    Some(source) => {
                        w.mounts.mount_library(w.vfs, &at, &fingerprint, source).map(drop)
                    }
                    None if w.mounts.has(&at) => Ok(()),
                    // alloc: an error, once
                    None => Err(format!(
                        "its files were not read as it was mounted at {at}, and nothing is"
                    )),
                };
                match mounted {
                    Ok(()) => self.library = Some(at),
                    Err(why) => {
                        // alloc: an error, once
                        let what = format!("the standard library's source: {why}");
                        self.library_failed = Some(ApiError::new(what, Some(at)));
                    }
                }
            }
            Value::Library(Err(error)) => self.library_failed = Some(error),
            Value::Fetched(fetches, at, Ok(source)) => {
                let fetch = &fetches[at as usize];
                if let Err(why) = w.mounts.mount(w.vfs, &fetch.at, source) {
                    // clone: a failed fetch's names, once per source
                    self.fetch_failed
                        .insert(fetch.at.clone(), ApiError::new(why, Some(fetch.what.clone())));
                }
            }
            Value::Fetched(fetches, at, Err(error)) => {
                // clone: a failed fetch's place, once per source
                self.fetch_failed.insert(fetches[at as usize].at.clone(), error);
            }
            Value::Local(fetches, at, Ok(read)) => {
                let fetch = &fetches[at as usize];
                if let Err(why) = w.mounts.mount_local(w.vfs, read) {
                    // clone: a failed read's names, once per path
                    self.fetch_failed.insert(fetch.at.clone(), ApiError::new(why, Some(fetch.what.clone())));
                }
            }
            Value::Local(fetches, at, Err(error)) => {
                // clone: a failed read's place, once per path
                self.fetch_failed.insert(fetches[at as usize].at.clone(), error);
            }
            Value::Compiled(unit, result, images) => {
                let _span =
                    tracing::info_span!(target: "blaze", "loop: a unit's compile taken", unit)
                        .entered();
                self.keep_images(node, unit, images);
                self.compiled(w, node, unit, result);
            }
            Value::Phase(unit, images) => self.keep_images(node, unit, images),
            Value::Evaluated(result) => {
                let Some(id) = self.batches.job_of(node) else { return };
                let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
                match result {
                    Ok(result) => {
                        if result.outcome == TestOutcome::Failed
                            && let Stage::Units(build) = &mut job.stage
                            && let Some(evaluation) = &mut build.evaluation
                        {
                            evaluation.failed = true;
                        }
                        w.said.push(Said::Report(id, Response::TestResult(result)));
                    }
                    Err(error) => {
                        if !job.cancelled.load(Ordering::Acquire) {
                            job.over.get_or_insert((false, Some(error)));
                        }
                    }
                }
            }
            Value::Native(key, laid) => {
                self.native_laid.insert(key, laid);
                // A stopped layout no job waits on is over.
                if self.native_waiting.get(&key).is_some_and(|it| it.jobs.is_empty()) {
                    self.native_waiting.remove(&key);
                }
            }
            Value::StoredKept(front, stored, mut ran) => {
                self.ran.append(&mut ran);
                // Its job's while the job lives, else of what the job left to settle.
                let settling = self
                    .settling
                    .iter_mut()
                    .filter_map(|it| it.storing.as_mut())
                    .find(|it| it.node == node);
                let storing = match settling {
                    Some(storing) => Some(storing),
                    None => self
                        .batches
                        .job_of(node)
                        .and_then(|id| self.jobs.get_mut(key_of(id)))
                        .and_then(|it| it.storing.as_mut())
                        .filter(|it| it.node == node),
                };
                if let Some(storing) = storing {
                    storing.value = Some((front, stored));
                }
            }
            Value::GaveValues(front, gave) => self.gave_values(w, node, front, gave),
            value => {
                if let Some(job) =
                    self.batches.job_of(node).and_then(|id| self.jobs.get_mut(key_of(id)))
                {
                    job.value = Some((node, value));
                }
            }
        }
    }

    fn settled(
        &mut self,
        w: &mut World<'_, S>,
        node: NodeId,
        ok: bool,
        why: Option<String>,
        skipped: Option<NodeId>,
    ) {
        // A give-back of a kept front end's values, which is no job's: its value came before
        // this ([`Jobs::gave_values`]), or never will ([`Jobs::give_settled`]).
        self.give_settled(w, node);
        // A unit's node: what its walk reached teaches the forecast, and a unit whose names
        // settled is forecast by its dependencies' walks ([`Jobs::forecast_walk`]).
        self.forecast_walk(w, node);
        // The store of a kept front end whose job answered already ([`Settling`]): no batch
        // names it any more.
        let settling = self
            .settling
            .iter()
            .position(|it| it.storing.as_ref().is_some_and(|it| it.node == node));
        if let Some(at) = settling {
            if let Some(storing) = self.settling[at].storing.take() {
                self.stored_kept(w, storing, Some(at), why);
            }
            return;
        }
        let Some(id) = self.batches.job_of(node) else { return };
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if let Stage::Units(build) = &mut job.stage {
            build.settled(node, ok, why.as_deref(), skipped);
        }
        // Its store settles while the job still has something under way (a placement, a run):
        // what it left for the store is put, its tickets the job's until it ends.
        if job.storing.as_ref().is_some_and(|it| it.node == node) {
            if let Some(storing) = job.storing.take() {
                self.stored_kept(w, storing, None, why);
            }
            return;
        }
        // A node of a build of the units a view's front end applies, made in the job's plane
        // request, which stays open: the build ends as its last node settles, counted even once
        // the job is over (something of it failed with the request still open), so that end is
        // never left waiting for a finish the request can't give.
        if let Stage::Units(build) = &mut job.stage
            && build.unit(node).is_some()
            && let Some(applying) = build.applying.as_mut()
        {
            applying.left = applying.left.saturating_sub(1);
            applying.failed |= !ok;
            if applying.left == 0 {
                let ok = !applying.failed && job.over.is_none();
                self.units_applied(w, id, ok);
            }
            return;
        }
        if job.over.is_some() {
            job.take_value(node);
            // A fetch that settles after another failed is still counted: once the last one
            // settles the job ends with the failure. Left uncounted, a job whose fetches all
            // failed (every one, in a blaze built without `net`) sat in its fetch stage with
            // each fetch settled and nothing to move it, an idle engine that said nothing
            // (lane seedmem, 2026-11-02).
            let fetched = match &mut job.stage {
                Stage::Fetch(fetching)
                    if (node.0.wrapping_sub(fetching.first.0) as usize)
                        < fetching.fetches.len() =>
                {
                    fetching.left -= 1;
                    fetching.left == 0
                }
                // Its layouts are counted so too.
                Stage::Native(wait) if wait.nodes.iter().any(|(at, _)| *at == node) => {
                    wait.left -= 1;
                    wait.left == 0
                }
                _ => false,
            };
            if fetched {
                job.stage = Stage::Over;
                let error = job.over.take().and_then(|(_, error)| error);
                self.end(w, id, false, error);
            }
            return;
        }
        let why = why.unwrap_or_default();
        match std::mem::replace(&mut job.stage, Stage::Over) {
            Stage::Library(at) if at == node => {
                if self.library.is_some() {
                    // A request that came before this daemon had looked at the library is
                    // checked against its certificate now, before its view is opened.
                    if !self.certify_late(w, id) {
                        self.prepare(w, id);
                    }
                } else {
                    // alloc: an error, once
                    let error = self.library_failed.clone().unwrap_or_else(|| {
                        ApiError::new(format!("the standard library's source: {why}"), None) // alloc: an error, once
                    });
                    self.end(w, id, false, Some(error));
                }
            }
            Stage::Locals(at, opened) if at == node => {
                let read = match job.take_value(node) {
                    Some(Value::Locals(read)) => read,
                    // alloc: an error, once
                    _ => Err(ApiError::new(format!("reading the paths outside the view again: {why}"), None)),
                };
                let mounted = read.and_then(|reads| {
                    reads.into_iter().try_for_each(|read| {
                        let at = read.at.clone(); // clone: an error's place, once per path
                        w.mounts.mount_local(w.vfs, read).map(drop).map_err(|why| ApiError::new(why, Some(at)))
                    })
                });
                match mounted {
                    Ok(()) => self.open(w, id, opened, HashSet::new_in(Global)),
                    Err(error) => self.end(w, id, false, Some(error)),
                }
            }
            Stage::Plan(at, prep) if at == node => match job.take_value(node) {
                Some(Value::Planned(Ok(planned))) => self.planned(w, id, prep, planned),
                Some(Value::Planned(Err(error))) => self.end(w, id, false, Some(error)),
                _ => {
                    let error =
                        ApiError::new(format!("the plan of {}: {why}", prep.root_dir), None); // alloc: an error, once
                    self.end(w, id, false, Some(error));
                }
            },
            Stage::Fetch(mut fetching) => {
                let place = node.0.wrapping_sub(fetching.first.0) as usize;
                if place >= fetching.fetches.len() {
                    job.stage = Stage::Fetch(fetching);
                    return;
                }
                if !ok {
                    // Kept going: the batch's other fetches settle too, and the job ends once
                    // with every failure, not the first (lane earlyfail: a blaze without `net`
                    // said one missing package a run).
                    fetching.failed.push(place as u32);
                    if !self.fetch_failed.contains_key(&fetching.fetches[place].at) {
                        let at = &fetching.fetches[place].at;
                        // alloc: an error, once per source; clone: its place, the map's key
                        let error =
                            ApiError::new(format!("fetching {at}: {why}"), Some(at.clone()));
                        // clone: as above
                        self.fetch_failed.insert(at.clone(), error);
                    }
                }
                fetching.left -= 1;
                if fetching.left > 0 {
                    job.stage = Stage::Fetch(fetching);
                    return;
                }
                // What ended the job meanwhile (a cancel) is its error before any fetch's.
                if let Some((_, error)) = job.over.take() {
                    self.end(w, id, false, error);
                    return;
                }
                if !fetching.failed.is_empty() {
                    let error = unavailable(
                        &fetching.fetches,
                        &fetching.failed,
                        &self.fetch_failed,
                        &self.setup.sources,
                        &self.setup.store,
                    );
                    self.end(w, id, false, Some(error));
                    return;
                }
                let prep = fetching.prep;
                w.db.close(prep.queries);
                job.queries.retain(|it| *it != prep.queries);
                self.open(w, id, prep.opened, prep.asked);
            }
            // Each digest's result read by its key, whether this job ran its layout or joined
            // another's, once the last of them settles.
            Stage::Native(mut wait) => {
                let Some(&(_, key)) = wait.nodes.iter().find(|(at, _)| *at == node) else {
                    job.stage = Stage::Native(wait);
                    return;
                };
                if !ok {
                    wait.failed.push((key, why));
                }
                wait.left -= 1;
                if wait.left > 0 {
                    job.stage = Stage::Native(wait);
                    return;
                }
                self.native_let_go(id, false);
                let NativeWait { failed, mut input, root, name, checkout, .. } = *wait;
                let mut dirs = Vec::with_capacity(input.units.native.len()); // alloc: once a view with pinned native inputs
                for native in &input.units.native {
                    let key = native_key(&native.sha256);
                    let failed =
                        failed.iter().find(|(it, _)| *it == key).map(|(_, why)| why.as_str());
                    match (self.native_laid.get(&key), failed) {
                        // clone: once an input a job
                        (Some(Ok(dir)), None) => dirs.push(super::native::NativeDir {
                            input: native.clone(),
                            dir: dir.clone(),
                        }),
                        (Some(Err(error)), Some(_)) => {
                            let error = error.clone(); // clone: an error, once a job
                            self.end(w, id, false, Some(error));
                            return;
                        }
                        (_, why) => {
                            let what = format!(
                                "laying out pinned native input `{}` for target `{}`: {}",
                                native.name,
                                native.target,
                                why.unwrap_or("its layout settled with no result")
                            ); // alloc: an error, once
                            self.end(
                                w,
                                id,
                                false,
                                Some(ApiError::new(what, Some(native.pin.clone()))),
                            ); // clone: as above
                            return;
                        }
                    }
                }
                input.native = dirs.into();
                self.lower_or_wait(w, id, input, root, name, checkout);
            }
            Stage::Lower(at, input, root) if at == node => match job.take_value(node) {
                Some(Value::Lowered(Ok(FrontEnd { front, writer, puts, mut misses }))) => {
                    job.misses.append(&mut misses);
                    for put in puts {
                        let (key, kind) = (put.key, put.kind);
                        match w.store.put(put.key, put.kind, &put.bytes) {
                            Ok(ticket) => self.put.add(ticket),
                            Err(mismatch) => {
                                // alloc: an error, once
                                let what = format!(
                                    "the store holds other bytes under {key:?} than the front end's lowering made now, \
                                     a {kind:?} value: {mismatch:?}"
                                );
                                self.end(w, id, false, Some(ApiError::new(what, None)));
                                return;
                            }
                        }
                    }
                    job.ready.push(Ready { input, root, front: Some(front), writer: Some(writer) });
                    self.next(w, id);
                }
                Some(Value::Lowered(Err(error))) => self.end(w, id, false, Some(error)),
                _ => self.end(
                    w,
                    id,
                    false,
                    Some(ApiError::new(format!("lowering the units: {why}"), None)),
                ), // alloc: an error, once
            },
            Stage::Answer(at) if at == node => match job.take_value(node) {
                Some(Value::Answer(Ok(response))) => {
                    w.said.push(Said::Report(id, response));
                    self.end(w, id, true, None);
                }
                Some(Value::Answer(Err(error))) => self.end(w, id, false, Some(error)),
                Some(Value::Answers(Ok(responses))) => {
                    for response in responses {
                        w.said.push(Said::Report(id, response));
                    }
                    self.end(w, id, true, None);
                }
                Some(Value::Answers(Err(error))) => self.end(w, id, false, Some(error)),
                _ => self.end(w, id, false, Some(ApiError::new(why, None))),
            },
            Stage::Ahead(at) if at == node => match job.take_value(node) {
                Some(Value::Ahead(Ok(units))) => self.build(w, id, Some(&units)),
                Some(Value::Ahead(Err(error))) => self.end(w, id, false, Some(error)),
                _ => {
                    let error =
                        ApiError::new(format!("choosing the units to build ahead: {why}"), None); // alloc: an error, once
                    self.end(w, id, false, Some(error));
                }
            },
            Stage::Units(build)
                if build.evaluation.as_ref().is_some_and(|it| it.select == node) =>
            {
                let value = job.take_value(node);
                job.stage = Stage::Units(build);
                match value {
                    Some(Value::Selected(Ok(tests))) => self.evaluate(w, id, tests),
                    Some(Value::Selected(Err(error))) => self.end(w, id, false, Some(error)),
                    _ => {
                        let error = ApiError::new(format!("selecting the tests: {why}"), None); // alloc: an error, once
                        self.end(w, id, false, Some(error));
                    }
                }
            }
            stage => job.stage = stage,
        }
    }

    fn planned(&mut self, w: &mut World<'_, S>, id: JobId, mut prep: Prep, planned: Planned) {
        match planned {
            Planned::Failed(error) => {
                self.end(w, id, false, Some(ApiError::new(error.what, error.at)))
            }
            Planned::Needs(fetches) => {
                let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
                let mut wanted = Vec::with_capacity(fetches.len());
                for fetch in fetches {
                    // clone: each place asked, once per source
                    if !prep.asked.insert(fetch.at.clone()) {
                        let error = ApiError::new(
                            // alloc: an error, once
                            format!(
                                "the load asked for {} again after it was fetched and mounted at {}",
                                fetch.what, fetch.at
                            ),
                            Some(fetch.at.clone()), // clone: an error, once
                        );
                        self.end(w, id, false, Some(error));
                        return;
                    }
                    if w.mounts.has(&fetch.at) {
                        continue;
                    }
                    // A path read where it is must be outside what blaze keeps itself: one
                    // under its sources or its agents' views is a source fetched or a view
                    // mounted, and a load asking for it there is a bug, said, never read past.
                    // Owned, so no borrow of `self` outlives the check into the `end` below.
                    let kept = match fetch.kind {
                        // clone: an error's text, only when it is under one
                        FetchKind::Local { .. } => {
                            [&self.setup.sources, &self.setup.views]
                                .into_iter()
                                .find(|it| super::unit::files::under(&fetch.at, it).is_some())
                                .cloned()
                        }
                        _ => None,
                    };
                    if let Some(kept) = kept {
                        let error = ApiError::new(
                            // alloc: an error, once
                            format!("the load asked to read {} where it is on the disk, but it is under {kept}, which blaze keeps and mounts itself", fetch.at),
                            Some(fetch.at.clone()), // clone: an error, once
                        );
                        self.end(w, id, false, Some(error));
                        return;
                    }
                    let verb = match &fetch.kind {
                        FetchKind::Crate { .. } => {
                            Some((FetchVerb::Downloaded, fetch.what.clone()))
                        } // clone: a report, once per source
                        FetchKind::IndexFile | FetchKind::Git { .. } => {
                            // clone: a report, once per index or repository
                            job.updating
                                .insert(fetch.what.clone())
                                .then(|| (FetchVerb::Updating, fetch.what.clone()))
                        }
                        FetchKind::Local { .. } => None,
                    };
                    if let Some((verb, what)) = verb {
                        w.said.push(Said::Report(id, Response::Fetched { verb, what }));
                    }
                    wanted.push(fetch);
                }
                if wanted.is_empty() {
                    w.db.close(prep.queries);
                    job.queries.retain(|it| *it != prep.queries);
                    self.open(w, id, prep.opened, prep.asked);
                    return;
                }
                let count = u32::try_from(wanted.len()).expect("under 2^32 fetches");
                let fetches: Arc<[Fetch]> = wanted.into();
                const FETCH: &str = "fetch ";
                let names = fetches.iter().map(|it| FETCH.len() + it.what.len()).sum();
                let mut fresh = w.plane.fresh(count, names, 0);
                let mut name = String::new(); // alloc: one name buffer, reused by every fetch
                for (at, fetch) in (0u32..).zip(fetches.iter()) {
                    name.clear();
                    name.push_str(FETCH);
                    name.push_str(&fetch.what);
                    let task = Task::Thread { kind: kinds::FETCH, arg: at };
                    w.plane.push(
                        &mut fresh,
                        node(NodeKind::Fetch, &name, &[], Pool::Io, Some(fetch_key(fetch))),
                        task,
                    );
                }
                let first = fresh.first();
                // clone: two refcounts, once per batch of fetches
                let work = Step::of(
                    &job.cancelled,
                    Does::Fetch { steps: w.steps.clone(), fetches: fetches.clone() },
                );
                // This batch's fetches say their own errors: one an earlier run of a source left
                // is not this run's cause, and is said by no later error.
                for fetch in fetches.iter() {
                    self.fetch_failed.remove(&fetch.at);
                }
                job.stage = Stage::Fetch(Fetching {
                    first,
                    fetches,
                    left: count,
                    failed: Vec::new(),
                    prep,
                });
                self.add(w, id, fresh, Some(work), false);
            }
            Planned::Ready(mut units) => {
                let lock = units.lock.take();
                // The lock files this plan writes are files of the checkout the build is
                // of: written through the VFS into the job's own view, which is opened and
                // planned again with them, so every later step of this build (the plan
                // kept, the front end, the no-op lane's recording) is of the view that
                // holds them ([`Jobs::lock_into_view`]).
                let lock = match self.lock_into_view(w, id, prep, lock) {
                    Ok((kept, lock)) => {
                        prep = kept;
                        lock
                    }
                    Err(()) => return,
                };
                // An edit of a source file moves the view's root, so the plan is asked again
                // and comes back as a value of its own, equal to the checkout's last in all
                // but identity. What is kept by the units' identity (their outputs' path ids,
                // their source sizes, [`Jobs::build`]) was then made again each edit, the
                // sizes by a walk of every unit's source directories on the loop's thread.
                // The same units are the same `Arc`: compared here, once a plan, in a span
                // that says how it went.
                let same = prep.root.and_then(|root| {
                    // Any plan the checkout keeps, the most recently used first: a request
                    // of a shape asked before (`build` after `test`) gets the units it had,
                    // and with them what is kept by their identity.
                    let kept = &self.roots.plans[root as usize];
                    if kept.is_empty() {
                        return None;
                    }
                    let _span = tracing::info_span!(
                        target: "blaze",
                        "loop: the plan compared with the checkout's last",
                        units = units.graph.units.len(),
                        same = tracing::field::Empty,
                    )
                    .entered();
                    let found = || kept.iter().rev().find(|last| *last.units == units);
                    #[cfg(test)]
                    let same = if PLANT_PLAN_ANEW.get() { None } else { found() };
                    #[cfg(not(test))]
                    let same = found();
                    _span.record("same", same.is_some());
                    same.map(|last| Arc::clone(&last.units)) // clone: a refcount, once per plan
                });
                // alloc: once per plan that differs from the last, shared by the job's tasks
                let units = same.unwrap_or_else(|| Arc::new(units));
                let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
                self.planned = true;
                if let Some(root) = prep.root
                    && reuses_plans(&job.work)
                {
                    job.queries.retain(|it| *it != prep.queries);
                    job.borrowed.push(prep.queries);
                    // What is kept by the units' identity goes on with the same units.
                    let (outputs, sized) = plan_of(&mut self.roots.plans[root as usize], &units)
                        .map_or((None, None), |it| (it.outputs.clone(), it.sized.take())); // clone: a refcount
                    // clone: a refcount and the lock's text, once per plan
                    let last = LastPlan {
                        key: prep.key,
                        shape: prep.shape,
                        queries: prep.queries,
                        units: units.clone(),
                        lock: lock.clone(),
                        asked: job.locked.clone(),
                        outputs,
                        sized,
                    };
                    // The plan this one replaces is its own shape's (the same request on an
                    // older view), or the least recently used past the bound: never the plan
                    // of another request, which that request finds again.
                    for old in keep_plan(&mut self.roots.plans[root as usize], last) {
                        if old.queries != prep.queries {
                            self.retired.push(old.queries);
                        }
                    }
                }
                self.ready(w, id, prep, units, lock);
            }
        }
    }

    /// Each lock file the load let job `id` write (`Cargo.lock`, `blaze.lock`) asked of the
    /// driver, then what the build says of them: a write is never silent (docs/cargo.md,
    /// "Lock files"). The job's answer waits for each write. False when the job was ended
    /// with an error instead. `root` is the root of the job's view, of the checkout at
    /// `root_dir`: a file of a watched checkout is written only when that view holds it
    /// already ([`written::ViewWrite`]), which is the one way a write is asked.
    fn write_locks(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        lock: LockWrite,
        root: ContentId,
        root_dir: &str,
    ) -> bool {
        for written in lock.files {
            let path = self.paths.intern(&written.path);
            // clone: the driver's effect owns the file's name, once per lock write
            let Some(file) = self.paths.files[path as usize].clone() else {
                // alloc: an error, once
                let error = ApiError::new(
                    format!("the lock file's path {} is no file", written.path),
                    Some(written.path),
                );
                self.end(w, id, false, Some(error));
                return false;
            };
            let tag = match self.own_tag(Own::Write { job: id, path }) {
                Ok(tag) => tag,
                Err(error) => {
                    self.end(w, id, false, Some(error));
                    return false;
                }
            };
            let write = written::ViewWrite::new(
                w.vfs,
                w.heads.checkouts(),
                root_dir,
                root,
                &written.path,
                file,
                written.text.into_bytes(),
            );
            let write = match write {
                Ok(write) => write,
                Err(what) => {
                    self.end(w, id, false, Some(ApiError::new(what, Some(written.path))));
                    return false;
                }
            };
            // A lock file is what the job is asked for: its answer waits for it.
            self.name(id, path);
            self.begin_placing(id, path);
            w.effects.push(write.effect(tag));
        }
        for line in &lock.said {
            // alloc: a line of the lock write, once per plan that writes one
            let note = Diagnostic::note(line.to_owned());
            w.said.push(Said::Report(id, Response::Diagnostic(note)));
        }
        true
    }

    /// The lock files job `id`'s plan writes, taken into its own view when they are files of
    /// it (rule guard-the-cause: an output a build writes into a place it also watches comes
    /// back from the watcher as a change someone else made, unless the build's view already
    /// holds it). Until 2026-11-12 the files were written to the disk alone: the cold build
    /// was of a view without them, the watcher committed them, and the next request (a build
    /// with nothing changed, or the first edit) was on a root no build had seen, so its plan
    /// was made anew and the engine's inputs were written (`SourceRootInput`, a path
    /// bucket), after which no library row proved itself by durability.
    ///
    /// Now, for a checkout's view all of whose lock files are files of it: each is written
    /// over the view's root in the VFS, as the watcher will commit it (whole, with the mode
    /// the view's file has, else regular), which moves no head; the writes are asked of the
    /// driver and said ([`Jobs::write_locks`]); the database view planned on is closed; and
    /// the job's view is opened again at the root that holds the files ([`Jobs::open`]), as
    /// after a fetch. Its plan there reads the lock files it wrote and writes none, and
    /// everything recorded of the build is of that root. The watcher's report of the files
    /// then commits bytes equal to the view's: the head's root is the build's, and the next
    /// request opens a view the database already holds (an input set to an equal value
    /// writes nothing).
    ///
    /// `Ok` with the prep and the lock write still to make where nothing was taken (no lock
    /// write, an agent's view, a lock file outside the view or one the checkout's view leaves
    /// out, or files the view already holds byte for byte, which change nothing in it); `Err`
    /// when the job went on from here, opened again or ended. A plan of the view opened again
    /// that asks for other bytes does not read back what it wrote: an error naming the file,
    /// never a second round (rule bounded-progress).
    fn lock_into_view(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        prep: Prep,
        lock: Option<LockWrite>,
    ) -> Result<(Prep, Option<LockWrite>), ()> {
        let Some(written) = lock else { return Ok((prep, None)) };
        if prep.root.is_none() {
            return Ok((prep, Some(written)));
        }
        let Some(ops) = Self::lock_ops(&*w.vfs, &prep.root_dir, &prep.opened, &written) else {
            return Ok((prep, Some(written)));
        };
        let root = match w.vfs.apply_owned(prep.opened.view.root, ops) {
            Ok(root) => root,
            Err(error) => {
                // alloc: an error, once
                let path = written.files[0].path.clone();
                let what = format!("writing the lock files into the view of {}: {error}", prep.root_dir);
                self.end(w, id, false, Some(ApiError::new(what, Some(path))));
                return Err(());
            }
        };
        // By content: files the view holds as they would be written change nothing in it.
        if root == prep.opened.view.root {
            return Ok((prep, Some(written)));
        }
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return Err(()) };
        if job.locked.is_some() {
            // alloc: an error, once
            let path = written.files[0].path.clone();
            let error = ApiError::new(
                format!(
                    "the plan of {} asks to write {path} with other bytes after this build wrote it into its \
                     view: the load does not read back the lock file it writes, so no build of this checkout \
                     would settle (a bug in blaze)",
                    prep.root_dir
                ),
                Some(path),
            );
            self.end(w, id, false, Some(error));
            return Err(());
        }
        // The plan asked on the view without the files is not this build's: its database
        // view goes, as one whose load asked for a fetch does.
        w.db.close(prep.queries);
        job.queries.retain(|it| *it != prep.queries);
        self.open_locked(w, id, prep.opened, root, prep.key, written, prep.asked, &prep.root_dir);
        Err(())
    }

    /// The ops that write `lock`'s files into the view `opened` of the checkout at
    /// `root_dir`, each whole as the watcher will commit it (with the mode the view's file
    /// has, else regular). None when one of them is no file of the view: outside the
    /// checkout, or under what its view leaves out ([`super::heads::excluded`]).
    fn lock_ops(
        vfs: &Vfs,
        root_dir: &str,
        opened: &Opened,
        lock: &LockWrite,
    ) -> Option<Vec<blaze_vfs::Op>> {
        let under = root_dir.trim_end_matches('/');
        // alloc: once a plan that writes a lock file, an op each
        let mut ops = Vec::with_capacity(lock.files.len());
        for file in &lock.files {
            let rel = file.path.strip_prefix(under)?.strip_prefix('/')?;
            if super::heads::excluded(rel) {
                return None;
            }
            // A file the view has keeps its mode, as the watcher's commit of it does.
            let mode = match vfs.read(opened.files, rel) {
                Ok(Some(had)) => had.mode,
                _ => blaze_vfs::Mode::Regular,
            };
            ops.push(blaze_vfs::Op::Write {
                mode,
                path: rel.to_owned(),                 // alloc: an op's path, once a lock file
                bytes: file.text.as_bytes().to_vec(), // alloc: the view's own copy of the file
            });
        }
        (!ops.is_empty()).then_some(ops)
    }

    /// Job `id`'s view `was`, of `key`, replaced by the view at `root`, which holds the lock
    /// files `written`: the writes asked of the driver and said ([`Jobs::write_locks`]), the
    /// job's view swapped, and the view opened again ([`Jobs::open`]). The session's handles
    /// of the view asked stay its own (it closes them as the job finishes); the job reads by
    /// root, and its views are what a collection keeps ([`Jobs::roots`]).
    #[allow(clippy::too_many_arguments)]
    fn open_locked(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        was: Opened,
        root: ContentId,
        key: ContentId,
        written: LockWrite,
        asked: HashSet<String, Global>,
        root_dir: &str,
    ) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let _span = tracing::info_span!(
            target: "blaze",
            "loop: the plan's lock files written into the job's view",
            files = written.files.len(),
        )
        .entered();
        let opened = Opened { view: blaze_vfs::View { root }, ..was };
        job.locked = Some((key, written.clone())); // clone: the lock files' text, once
        if let Some(view) = job.views.iter_mut().find(|it| **it == was) {
            *view = opened;
        }
        if !self.write_locks(w, id, written, root, root_dir) {
            return;
        }
        drop(_span);
        self.open(w, id, opened, asked);
    }

    /// The view's plan taken: its lock files that are no files of the view written, its
    /// warnings reported, and its units
    /// lowered, or for the work that walks the plan, the view ready as it is.
    fn ready(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        prep: Prep,
        units: Arc<Units>,
        lock: Option<LockWrite>,
    ) {
        if self.jobs.get(key_of(id)).is_none() {
            return;
        }
        // On the loop once a view: the output directories asked of the watches, the lock
        // files written, the lowering posted (or held for the kept front end's store).
        let _span = tracing::info_span!(
            target: "blaze",
            "loop: a job's plan taken, to its lowering posted",
            units = units.graph.units.len(),
        )
        .entered();
        // A build or a check of a checkout has the directories its outputs are in watched
        // from the watches' next round, so a later walk's answer can rest on them
        // ([`noop::LastAnswer::replayable`]). Once a directory a process.
        if prep.root.is_some()
            && self.jobs.get(key_of(id)).is_some_and(|job| request_id(&job.work).is_some())
            && let Ok(dirs) = output_dirs(&units, &self.setup.std)
        {
            for dir in &dirs {
                blaze_vfs::outside::want(dir);
            }
        }
        // The lock files of a plan that are no files of its view (a workspace whose root is
        // above the checkout's), written where they are: one of the view was written into it
        // before the plan was kept ([`Jobs::lock_into_view`]).
        // Not again what this job wrote as it took them into its view.
        let wrote = self
            .jobs
            .get(key_of(id))
            .and_then(|job| job.locked.as_ref())
            .is_some_and(|(_, wrote)| Some(wrote) == lock.as_ref());
        if let Some(lock) = lock
            && !wrote
            && !self.write_locks(w, id, lock, prep.opened.view.root, &prep.root_dir)
        {
            return;
        }
        for warning in &units.warnings {
            w.said.push(Said::Report(id, Response::Diagnostic(warned(warning))));
        }
        for note in &units.notes {
            // alloc: a note of the load, once per plan that has one
            let note = Diagnostic::note(note.to_owned());
            w.said.push(Said::Report(id, Response::Diagnostic(note)));
        }
        // `blaze lock` stops here: its plan wrote the lock files; nothing is lowered or built. The
        // job finishes once those writes are placed ([`Jobs::wind_up`]).
        if self.jobs.get(key_of(id)).is_some_and(|job| matches!(job.work, Work::Lock(_))) {
            self.end(w, id, true, None);
            return;
        }
        let asked_args = self
            .jobs
            .get(key_of(id))
            .and_then(|job| targets_of(&job.work))
            .and_then(|it| it.cargo.as_ref())
            .map_or(&[][..], |cargo| cargo.args.as_slice());
        let short = match short(asked_args) {
            Ok(short) => short,
            Err(why) => {
                self.end(
                    w,
                    id,
                    false,
                    Some(ApiError::new(why, Some("--message-format".to_owned()))),
                ); // alloc: an error, once
                return;
            }
        };
        // Its build dirs are leased by its lowering, on the pool ([`Does::Lower`]).
        let checkout = prep.config.invocation.root.clone(); // clone: the lowering's own copy, once
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        job.built = job.queries.contains(&prep.queries).then_some(prep.queries);
        job.built_root = job.built.map(|_| prep.opened.view.root);
        let cargo = targets_of(&job.work).and_then(|it| it.cargo.as_ref());
        let input = JobInput {
            db: w.db.clone(), // clone: a refcount, once per view
            queries: prep.queries,
            root: prep.opened.view.root,
            files: w.vfs.reader(),
            config: prep.config,
            units,
            native: Arc::from([]),
            store: w.store.reader(),
            std: w.std.reader(),
            macros: Arc::clone(w.macros), // clone: a refcount, once per view
            color: cargo.map_or(self.setup.color, |it| it.color),
            short,
            width: cargo.and_then(|it| it.width),
            verbose: cargo.map_or(0, |it| it.verbose),
            refused: cargo.map_or(Some(crate::REFUSED_SHOWN), |it| it.refused),
            kind: kind_of(&job.work),
            goal: targets_of(&job.work).and_then(|it| it.goal),
            cancelled: job.cancelled.clone(), // clone: a refcount, once per view
        };
        // Asked as the view is lowered, so it is in by the time the units are built.
        job.look = w.std.look_again(w.effects);
        let name = named("lowering ", &prep.root_dir);
        // A build script given a pinned native input runs with it laid out: each is fetched
        // and unpacked first, on an IO thread, and the lowering (or a trace's or verify's
        // view, which is ready as it stands) posted once they are.
        if !input.units.native.is_empty() {
            // One layout task a digest, keyed by it, so every job asking it (whatever it names
            // its inputs) shares it; its cancel flag is the digest's, set once no job waits on
            // it, never one job's.
            let mut digests: Vec<&blaze_units::query::NativeInput> =
                input.units.native.iter().collect(); // alloc: once a view
            digests.sort_by(|a, b| a.sha256.cmp(&b.sha256));
            digests.dedup_by(|a, b| a.sha256 == b.sha256);
            let mut posts = Vec::with_capacity(digests.len()); // alloc: as above
            for native in digests {
                let key = native_key(&native.sha256);
                let waiting = self.native_waiting.entry(key).or_insert_with(|| NativeWaiting {
                    jobs: Vec::new(),
                    stop: Arc::new(AtomicBool::new(false)),
                }); // alloc: once a digest in flight
                waiting.jobs.push(id);
                // A layout every waiter left is stopped; this job wants it again. One that saw
                // the flag before this settles `cancelled` and fails this job with that error
                // (a narrow window: the next build lays it out).
                waiting.stop.store(false, Ordering::Release);
                // clone: a refcount and an input, once a digest a view
                let work = Step::of(
                    &waiting.stop,
                    Does::Native { steps: Arc::clone(w.steps), input: native.clone(), key },
                );
                let what = named("pinned native input ", &native.name);
                let fresh =
                    lone(w.plane, NodeKind::Fetch, &what, Pool::Io, Some(key), kinds::FETCH);
                posts.push((key, fresh, work));
            }
            let nodes = posts.iter().map(|(key, fresh, _)| (fresh.first(), *key)).collect(); // alloc: once a view
            let left = u32::try_from(posts.len()).expect("under 2^32 digests");
            let wait = NativeWait {
                nodes,
                left,
                failed: Vec::new(),
                input,
                root: prep.root,
                name,
                checkout,
            };
            job.stage = Stage::Native(Box::new(wait));
            for (_, fresh, work) in posts {
                self.add(w, id, fresh, Some(work), false);
            }
            return;
        }
        self.lower_or_wait(w, id, input, prep.root, name, checkout);
    }

    /// A job's view lowered ([`Jobs::lower`]), or once the kept front end of its root is
    /// stored, when that is being stored now.
    fn lower_or_wait(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        input: JobInput,
        root: Option<u32>,
        name: String,
        checkout: String,
    ) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        // A trace or a verify lowers nothing: its view is ready as it stands, its pinned native
        // inputs laid out before it, as for any view.
        if matches!(job.work, Work::Trace(_) | Work::Verify(_)) {
            job.ready.push(Ready { input, root, front: None, writer: None });
            self.next(w, id);
            return;
        }
        let job_idle = job.idle;
        match root {
            // The root's front end is being stored: the lowering waits for it rather than
            // lowering anew beside it.
            Some(root) if self.roots.storing[root as usize] > 0 => {
                let mut on = None;
                // The store is its job's, or of what its job left to settle after its answer.
                let storings = self
                    .jobs
                    .values_mut()
                    .filter_map(|it| it.storing.as_mut())
                    .chain(self.settling.iter_mut().filter_map(|it| it.storing.as_mut()))
                    .filter(|it| it.root == root);
                for storing in storings {
                    on = Some(storing.node);
                    if job_idle {
                        continue;
                    }
                    // A build asked now waits on the store: none of it runs as idle work, and
                    // its task leaves the release to this build ([`Steps::release_kept`]).
                    storing.wanted.store(true, Ordering::Release);
                    if !storing.promoted && w.plane.is_open(storing.request) {
                        w.plane.promote(w.now, storing.request);
                        storing.promoted = true;
                    }
                }
                // Or its values are being given back for memory, on a worker: that give
                // stops at its slice's end for a build asked now ([`giving::Giving::wanted`]).
                let on = on.or(self.giving.wanted(root, job_idle));
                let span = tracing::info_span!(
                    target: "blaze",
                    "lowering waits for the kept front end's store",
                    checkout = %checkout,
                    root,
                    store_node = on.map(|it| it.0),
                );
                self.lowering.push(Lowering { id, input, root, name, checkout, on, span });
            }
            root => self.lower(w, id, input, root, &name, checkout),
        }
    }

    /// A job's view lowered by one task, into its root's kept front end if there is one.
    fn lower(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        input: JobInput,
        root: Option<u32>,
        name: &str,
        checkout: String,
    ) {
        // A check, build, test or run of a root keeps its front end and stores it once done
        // ([`Jobs::store_kept`]): refused now, before anything is built, when this checkout's
        // own last store could never be admitted. Every other work stores nothing.
        let keeps = root.is_some()
            && self.jobs.get(key_of(id)).is_some_and(|job| {
                matches!(
                    job.work,
                    Work::Check(_) | Work::Build(_) | Work::Test { .. } | Work::Run { .. }
                )
            });
        if keeps && let Some(error) = stores_too_much(w.plane, &checkout) {
            self.end(w, id, false, Some(error));
            return;
        }
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let kept = root.and_then(|root| self.roots.fronts[root as usize].take());
        if let Some(root) = root {
            // What a front end made from nothing takes is what giving it back costs.
            self.giving.lowers(root, kept.is_some(), w.now);
        }
        let kept = Lock::new("serve.job.lower.kept", kept);
        let (steps, lowering) = (w.steps.clone(), input.clone()); // clone: refcounts, once per view
        let (leases, builds) = (Arc::clone(&job.leases), self.setup.builds.clone()); // clone: a refcount and a path, once per view
        let learn = lower_cost_key(&checkout);
        let work = Step::of(
            &job.cancelled,
            Does::Lower { steps, input: lowering, kept, leases, checkout, builds },
        );
        let fresh = lone_learned(w.plane, NodeKind::Names, name, Pool::Work, kinds::LOWER, learn);
        job.stage = Stage::Lower(fresh.first(), input, root);
        self.add(w, id, fresh, Some(work), false);
    }

    /// What job `id`'s build left for the store, written from its front end as it is kept
    /// ([`Steps::store_kept`]) by one task of a plane request of its own, as the job's other
    /// tasks are done with it. The request is idle: the job's answer does not wait for it
    /// ([`Jobs::wind_up`]), and only a lowering held for the front end makes it run as one
    /// asked ([`Jobs::lower_or_wait`]). The front end is the root's again once the puts are
    /// made ([`Jobs::stored_kept`]). The task also names the run log of build `build` when `logs`,
    /// and lets go of what the build's end releases, both after the answer.
    fn store_kept(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        root: u32,
        checkout: String,
        kept: S::KeptFront,
        build: u64,
        logs: bool,
    ) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let request = w.plane.open(w.now, false, true);
        let name = named("storing the kept front end of ", &checkout);
        let fresh = lone_learned(
            w.plane,
            NodeKind::Store,
            &name,
            Pool::Work,
            kinds::STORE_KEPT,
            store_cost_key(&checkout),
        );
        let (node, count) = (fresh.first(), fresh.count());
        let wanted = Arc::new(AtomicBool::new(false)); // alloc: once per build, shared with the task
        // clone: the checkout's path, once per build, for the task's span
        let does = Does::StoreKept {
            steps: Arc::clone(w.steps),
            kept: Lock::new("serve.job.store_kept.kept", Some(kept)),
            checkout: checkout.clone(),
            build,
            logs,
            wanted: Arc::clone(&wanted), // clone: a refcount, once per build
        };
        let work = Step::of(&job.cancelled, does);
        let command = command_line(&job.work);
        // A store's request is marked before anything can finish it, and its node is the
        // job's only once the plane took it: a refused add leaves no entry behind.
        self.store_requests.insert(request);
        let refused = match w.plane.add(w.now, request, fresh, Some(work), false) {
            Ok(()) => {
                self.batches.insert(node.0, count, id);
                job.storing = Some(Storing {
                    job: id,
                    command,
                    request,
                    node,
                    root,
                    checkout,
                    wanted,
                    promoted: false,
                    value: None,
                });
                self.roots.storing[root as usize] += 1;
                None
            }
            Err(error) => {
                // alloc: a loss, once
                let what = format!(
                    "the plane refused the task storing what the build of {checkout} left for the store, so its \
                     front end is dropped and the checkout's next build lowers it anew: {error}"
                );
                Some((command, checkout, what))
            }
        };
        w.plane.close(w.now, request);
        // The build's answer is its units' and its outputs': what it could not leave for the
        // store is a loss said beside it, never its error.
        if let Some((command, checkout, what)) = refused {
            self.lose(id, &command, Some(&checkout), what);
        }
    }

    /// A kept front end stored, its task's node settled: its puts made and the store flushed,
    /// the front end the root's again, and the lowerings waiting for it posted. `settling`:
    /// where the store is of what its job left to settle ([`Jobs::settling`]); none while the
    /// job still lives. The tickets of what was put go there, to settle after the answer. A
    /// store that failed, or panicked, changes no answer: it is a loss ([`Jobs::lose`]), its
    /// front end dropped, and the root's next job lowers anew. A store that kept everything
    /// but a record its key does not hold ([`Steps::kept_refusal`]) is a loss said the same
    /// way, of that record alone: the front end is the root's again.
    fn stored_kept(
        &mut self,
        w: &mut World<'_, S>,
        storing: Storing<S>,
        settling: Option<usize>,
        why: Option<String>,
    ) {
        let Storing { job: id, command, root, checkout, value, .. } = storing;
        self.roots.storing[root as usize] -= 1;
        let mut tickets = blaze_cas::Tickets::default();
        let mut lost: Option<String> = match value {
            Some((mut front, Ok(puts))) => {
                let mut failed = None;
                let put = !puts.is_empty();
                for super::work::KeptPut { put, what } in puts {
                    let (key, kind) = (put.key, put.kind);
                    match w.store.put(put.key, put.kind, &put.bytes) {
                        // Each ticket is waited on: a value kept already gets the ticket of the
                        // put that kept it, from any writer's segment, so no one ticket stands
                        // for the rest ([`blaze_cas::Tickets`]).
                        Ok(ticket) => {
                            self.put.add(ticket);
                            tickets.add(ticket);
                        }
                        Err(mismatch) => {
                            // alloc: a loss, once
                            failed = Some(format!(
                                "the store holds other bytes under {key:?} than the build of {checkout} made now \
                                 for {what}, a {kind:?} value (kept {:?}, made {:?}): its key misses an input or \
                                 what made it is not deterministic (keys-cover-every-input)",
                                mismatch.kept, mismatch.given
                            ));
                            break;
                        }
                    }
                }
                if put {
                    w.store.flush(w.effects);
                }
                if failed.is_none() {
                    // A record the store would not keep of a build whose every other was
                    // kept: said as a loss, and the front end stays the root's
                    // ([`Steps::kept_refusal`]).
                    failed = front.as_mut().and_then(|it| w.steps.kept_refusal(it)).map(|why| {
                        // alloc: a loss, once
                        format!("storing what the build of {checkout} left for the store: {why}")
                    });
                    if front.is_some() {
                        self.giving.kept(root, w.now);
                    }
                    self.roots.fronts[root as usize] = front;
                }
                failed
            }
            // alloc: a loss, once
            Some((_, Err(error))) => Some(format!(
                "storing what the build of {checkout} left for the store: {}",
                error.what
            )),
            None => {
                let why = why.unwrap_or_else(|| "its task ended with no value".to_owned()); // alloc: a loss, once
                // alloc: a loss, once
                Some(format!("storing what the build of {checkout} left for the store: {why}"))
            }
        };
        // Storing a front end can expand macros of its own: a dependency crate's image encodes
        // the signature of every item (`hir_def::deps::serve`'s `signatures::encode`), and a
        // type-position macro call the build never needed is expanded there first, which a
        // kept proc-macro expansion puts. `plane_finished` published before the store ran, so
        // what those expansions put is published here as it is there: its tickets settle with
        // this store's (and the snapshot waits for them, [`Jobs::put`]), a durable put with
        // every job not ended and everything settling, and a verdict the store refuses is a
        // loss.
        w.macros.flush();
        let published = w.macros.keeps().publish(w.store, proc_macro_api::kept::take_puts());
        for ticket in &published.tickets {
            self.put.add(*ticket);
            tickets.add(*ticket);
        }
        if !published.durable.is_empty() {
            w.store.flush(w.effects);
            self.wait_durable(&published.durable);
        }
        if lost.is_none()
            && let Some(refused) = published.refused.into_iter().next()
        {
            // alloc: a loss, once
            lost = Some(format!(
                "storing what the build of {checkout} left for the store: {refused}"
            ));
        }
        match settling {
            Some(at) => {
                if let Some(it) = self.settling.get_mut(at) {
                    it.stored.extend(&tickets);
                    it.checkout.get_or_insert_with(|| checkout.clone()); // clone: the checkout's path, once per build
                }
            }
            None => {
                if let Some(job) = self.jobs.get_mut(key_of(id)) {
                    job.stored.extend(&tickets);
                    job.stored_of = Some(checkout.clone()); // clone: the checkout's path, once per build
                }
            }
        }
        if let Some(what) = lost {
            self.lose(id, &command, Some(&checkout), what);
        }
        self.lower_waiting(w, root);
    }

    /// The lowerings held for checkout `root`'s front end posted, once nothing holds it any
    /// more: no store of it and no give-back of its values is under way
    /// ([`Jobs::stored_kept`], [`Jobs::gave_values`]).
    fn lower_waiting(&mut self, w: &mut World<'_, S>, root: u32) {
        // Another store of the root's front end still under way: its lowerings wait for it.
        if self.roots.storing[root as usize] > 0 {
            return;
        }
        let mut waiting = std::mem::take(&mut self.lowering);
        let (now, later): (Vec<Lowering>, Vec<Lowering>) =
            waiting.drain(..).partition(|it| it.root == root); // alloc: the lowerings waiting, a few
        self.lowering = later;
        for Lowering { id, input, root, name, checkout, span, .. } in now {
            drop(span);
            if self.jobs.get(key_of(id)).is_some_and(|it| it.over.is_none()) {
                self.lower(w, id, input, Some(root), &name, checkout);
            }
        }
    }

    /// The shared std store has looked again: each build that waited for the look begins.
    pub fn looked(&mut self, w: &mut World<'_, S>) {
        let looked = w.std.looked();
        let jobs = &self.jobs;
        let waited = |id: &&JobId| {
            jobs.get(key_of(**id)).is_some_and(|it| it.awaiting.is_some() && it.look <= looked)
        };
        // alloc: the jobs a look lets build, a few, once per look
        let ready: Vec<JobId> = self.order.iter().filter(waited).copied().collect();
        for id in ready {
            let Some(picked) = self.jobs.get_mut(key_of(id)).and_then(|it| it.awaiting.take())
            else {
                continue;
            };
            self.build(w, id, picked.as_deref());
        }
        self.wind_up(w);
    }

    /// The next view to make ready, or the work once all are.
    fn next(&mut self, w: &mut World<'_, S>, id: JobId) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        // A job that reads a view's front end without building its units ([`reads_front_unbuilt`])
        // first builds, once the view is lowered, the units that front end applies (its proc
        // macros, whose expanders it is given, and its build scripts' runs, whose cfgs, env
        // and `OUT_DIR` it is given; a lowering applies neither), through the same unit graph
        // as a check (their records shared with one) and in the job's own plane request; only
        // then is its next view made ready or its answer posted. The checks in the answers
        // themselves (`unit::answer_macros_built`) stay as the backstop.
        if reads_front_unbuilt(&job.work)
            && job.applied < job.ready.len()
            && let Some(view) = job.ready.last()
        {
            job.applied = job.ready.len();
            let picked = applied_units(&view.input.units.graph.units);
            if !picked.is_empty() {
                let which = match (job.views.len(), job.applied) {
                    (2, 1) => "from",
                    (2, _) => "to",
                    _ => "its view",
                };
                job.applying = Some(tracing::info_span!(
                    target: "blaze",
                    "the answer waits for the proc macros and build scripts its view applies",
                    work = work_word(&job.work),
                    view = which,
                    root = %view.input.config.invocation.root,
                    proc_macros = %macro_names(&view.input.units, &picked),
                    units = picked.len(),
                ));
                self.build(w, id, Some(&picked));
                return;
            }
        }
        if job.ready.len() < job.views.len() {
            self.prepare(w, id);
            return;
        }
        if matches!(
            job.work,
            Work::Check(_)
                | Work::Build(_)
                | Work::Test { .. }
                | Work::Run { .. }
                | Work::Doc { .. }
        ) {
            self.build(w, id, None);
            return;
        }
        let reads = job.ready.last().is_some_and(|it| it.writer.is_some());
        let work = match answer(job, &self.paths, w.steps) {
            Ok(work) => work,
            Err(error) => {
                self.end(w, id, false, Some(error));
                return;
            }
        };
        let ahead = matches!(job.work, Work::Ahead { .. });
        let name = if ahead { "the units to build ahead" } else { "the job's answer" };
        let fresh = lone(w.plane, NodeKind::Bodies, name, Pool::Work, None, kinds::WALK);
        job.stage = if ahead { Stage::Ahead(fresh.first()) } else { Stage::Answer(fresh.first()) };
        self.add(w, id, fresh, Some(work), reads);
        if !ahead {
            self.close(w, id);
        }
    }

    /// The job's units as nodes of the plane: every one, or the `picked` ones of a build ahead
    /// and every unit they depend on.
    fn build(&mut self, w: &mut World<'_, S>, id: JobId, picked: Option<&[u32]>) {
        // On the loop once a build: every unit's phases made nodes of the plane, named and
        // sized, its outputs' paths found. Its cost is the plan's size, not the edit's.
        let _posting =
            tracing::info_span!(target: "blaze", "loop: a build's units posted").entered();
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if w.std.looked() < job.look {
            job.awaiting = Some(picked.map(<[u32]>::to_vec)); // alloc: a build ahead's picks, once
            job.look_wait = Some(tracing::info_span!(
                target: "blaze",
                "the job's units wait for the shared std store's look",
                look = job.look,
            ));
            return;
        }
        job.look_wait = None;
        let ready = job.ready.pop();
        let Some(Ready { input, root, front: Some(front), writer: Some(writer) }) = ready else {
            self.end(w, id, false, Some(ApiError::new("a build's view is not lowered", None)));
            return;
        };
        let cancelled = job.cancelled.clone(); // clone: a refcount, once per build
        // The front end's session carried to this build's pin, once a build of a checkout.
        if let Some(at) = root {
            carry_front::<S>(&mut self.roots.carries[at as usize], w.vfs, w.store, &input, &front);
        }
        // A build of the units a view's front end applies ([`Jobs::next`]): the job's plane
        // request stays open after it.
        let applying = job.applying.is_some();
        let all = &input.units.graph.units;
        // The units the build is asked for (`codegen::asked`), which a program key reads.
        let all_roots = &input.units.graph.roots;
        let picked = match picked {
            None => None,
            Some(picked) => {
                if let Some(bad) = picked.iter().find(|it| **it as usize >= all.len()) {
                    // alloc: an error, once
                    let what = format!(
                        "the build ahead picked unit {bad} of a plan of {} units",
                        all.len()
                    );
                    self.end(w, id, false, Some(ApiError::new(what, None)));
                    return;
                }
                let mut wanted = vec![false; all.len()]; // alloc: once per build ahead
                let mut stack = picked.to_vec(); // alloc: once per build ahead
                while let Some(at) = stack.pop() {
                    if std::mem::replace(&mut wanted[at as usize], true) {
                        continue;
                    }
                    stack.extend(all[at as usize].dependencies.iter().map(|dep| dep.index as u32));
                }
                // alloc: once per build ahead
                Some(
                    (0u32..)
                        .zip(&wanted)
                        .filter_map(|(at, it)| it.then_some(at))
                        .collect::<Vec<u32>>(),
                )
            }
        };
        let built =
            u32::try_from(picked.as_ref().map_or(all.len(), Vec::len)).expect("under 2^32 units");
        let unit_at =
            |offset: u32| picked.as_ref().map_or(offset, |it| it[offset as usize]) as usize;
        let kind = input.kind;
        // A unit's phases, a node each; none for a build script's run, which is one node.
        let phases_of = |unit: &blaze_units::schema::Unit| match unit.mode {
            UnitMode::RunCustomBuild => &[][..],
            _ => super::unit::phases(kind, unit),
        };
        // Each built unit's last node, by its place after the batch's first; `u32::MAX` for one
        // not built. A unit's first node waits for the last of each unit it depends on.
        let mut last = vec![u32::MAX; all.len()]; // alloc: once per build
        let fresh_units = vec![None; all.len()]; // alloc: once per build
        let verdicts = vec![Verdict::Unsettled; all.len()]; // alloc: once per build
        let mut units_of = Vec::new(); // alloc: once per build, a unit a node
        // Each package's `name version` by its id, what a node is named by.
        let mut packages = HashMap::new_in(Global); // alloc: once per build, a package an entry
        for package in &input.units.packages {
            packages.insert(package.id.as_str(), (package.name.as_str(), package.version.as_str()));
        }
        // alloc: a name buffer, once per build, reused by every unit
        let mut name = String::new();
        let (mut names, mut all_deps) = (0, 0);
        for offset in 0..built {
            let at = unit_at(offset);
            let unit = &all[at];
            let count = phases_of(unit).len().max(1);
            units_of.extend(std::iter::repeat_n(at as u32, count));
            last[at] = u32::try_from(units_of.len() - 1).expect("under 2^32 nodes");
            node_name(&mut name, unit, &packages);
            names += count * name.len();
            // A phase waits for at most two of its unit's before it.
            all_deps += unit.dependencies.len() + 2 * count;
        }
        let count = u32::try_from(units_of.len()).expect("under 2^32 nodes");
        let mut nodes = w.plane.fresh(count, names, all_deps);
        let first = nodes.first();
        // alloc: a deps buffer, once per build, reused by every unit
        let mut deps = Vec::new();
        let mut next = first.0;
        // Each unit's source bytes, what its phases' memory is predicted from
        // (`Plane::push_sized`): a stat of each `.rs` file under its modules' directories,
        // once a directory, named so its time is its own (rule 16).
        //
        // Read once a plan of a checkout, not once a build: the walk is of every unit's
        // directories (the standard library's and every dependency's among them: thousands
        // of files for hello world, tens of thousands for a large workspace), in line on the
        // loop's thread, before a single unit of the build is posted, and an edit changes
        // none of it by more than the bytes it typed. It is a prediction's input and never
        // an input of what is built (`unit::source_bytes`), and what a kind of task held is
        // learned from its runs besides (`cost_key`), so the sizes of the plan's first build
        // stand for the builds of the same units after it; new units (a manifest changed, a
        // target added) are sized anew. `read` says how many units this build sized.
        let sizing = tracing::info_span!(
            target: "blaze",
            "unit source sizes",
            units = built,
            read = tracing::field::Empty,
        );
        let mut seen = HashMap::new_in(Global); // alloc: once per build, a directory an entry
        let kept_sizes = root
            .and_then(|root| plan_of(&mut self.roots.plans[root as usize], &input.units))
            .and_then(|plan| plan.sized.take());
        let mut sizes = match kept_sizes {
            Some(sizes) if sizes.len() == all.len() => sizes,
            _ => vec![u64::MAX; all.len()], // alloc: once a plan
        };
        let mut sized = 0u32;
        for offset in 0..built {
            let at = unit_at(offset);
            let unit = &all[at];
            node_name(&mut name, unit, &packages);
            deps.clear();
            deps.extend(
                unit.dependencies.iter().map(|dep| NodeId(first.0 + last[dep.index as usize])),
            );
            let phases = phases_of(unit);
            if phases.is_empty() {
                let task = Task::Loop { kind: loops::SCRIPT_RUN };
                let learn = super::unit::cost_key(unit, None);
                w.plane.push_learned(
                    &mut nodes,
                    node(NodeKind::ScriptRun, &name, &deps, Pool::Work, None),
                    task,
                    learn,
                );
                next += 1;
                continue;
            }
            let source = match sizes[at] {
                u64::MAX => {
                    let read = sizing.in_scope(|| super::unit::source_bytes(unit, &mut seen));
                    // Never the mark of a unit not sized: one of that many bytes reads again.
                    sizes[at] = read.min(u64::MAX - 1);
                    sized += 1;
                    read
                }
                kept => kept,
            };
            let base = next;
            let of = |phase: Phase| {
                let index = phases
                    .iter()
                    .position(|it| *it == phase)
                    .expect("a phase a unit's phases have");
                NodeId(base + index as u32)
            };
            for (index, &phase) in phases.iter().enumerate() {
                if index > 0 {
                    deps.clear();
                    match phase {
                        Phase::Names => {}
                        Phase::Bodies | Phase::Codegen => deps.push(of(Phase::Names)),
                        Phase::Link => deps.extend([of(Phase::Bodies), of(Phase::Codegen)]),
                        Phase::Store => deps.push(NodeId(base + index as u32 - 1)),
                    }
                }
                let arg = phase.arg(at as u32);
                let spec = node(phase.kind(), &name, &deps, Pool::Work, None);
                let task = match phase {
                    Phase::Names => kinds::UNIT_NAMES,
                    Phase::Bodies => kinds::UNIT_BODIES,
                    Phase::Codegen => kinds::COMPILE,
                    Phase::Link => kinds::UNIT_LINK,
                    Phase::Store => kinds::UNIT_STORE,
                };
                let program = match phase {
                    Phase::Codegen => Some(kinds::PROGRAM),
                    Phase::Link => Some(kinds::PROGRAM_LINK),
                    _ => None,
                };
                // A program's codegen and link are each admitted by what that program's held,
                // one never measured alone.
                match program.zip(super::unit::program_key(unit, all_roots.contains(&at), phase)) {
                    Some((kind, key)) => {
                        _ = w.plane.push_program(&mut nodes, spec, kind, arg, key, source)
                    }
                    None => {
                        let learn = super::unit::cost_key(unit, Some(phase));
                        _ = w.plane.push_sized(
                            &mut nodes,
                            spec,
                            Task::Thread { kind: task, arg },
                            learn,
                            source,
                        );
                    }
                }
                next += 1;
            }
        }
        sizing.record("read", sized);
        #[cfg(test)]
        SIZED_UNITS.with_borrow_mut(|it| it.push(sized));
        // Kept with the plan these units are of; a plan not kept (a trace's) keeps none.
        if let Some(plan) =
            root.and_then(|root| plan_of(&mut self.roots.plans[root as usize], &input.units))
        {
            plan.sized = Some(sizes);
        }
        // clone: refcounts, once per build
        let work = Step::of(
            &cancelled,
            Does::Compile { front: front.clone(), back: self.paths.back.clone() },
        );
        let units = &input.units;
        let package_of = {
            let packages: HashMap<&str, u32, Global> =
                // alloc: once per build
                (0u32..).zip(&units.packages).map(|(at, package)| (package.id.as_str(), at)).collect();
            // alloc: once per build
            units
                .graph
                .units
                .iter()
                .map(|unit| packages.get(unit.pkg_id.as_str()).copied().unwrap_or(u32::MAX))
                .collect()
        };
        let reported = BitSet::<u32>::new(units.packages.len()); // alloc: once per build
        let kept_outputs = root
            .and_then(|root| plan_of(&mut self.roots.plans[root as usize], units))
            .and_then(|plan| plan.outputs.clone()); // clone: a refcount, once per build
        let outputs = match kept_outputs {
            Some(outputs) => outputs,
            None => {
                let made = Arc::new(self.paths.outputs(units)); // alloc: once per new units
                if let Some(plan) =
                    root.and_then(|root| plan_of(&mut self.roots.plans[root as usize], units))
                {
                    plan.outputs = Some(made.clone()); // clone: a refcount
                }
                made
            }
        };
        let select = match &job.work {
            Work::Test { filter, args, all, ub: Some(ub), .. } => {
                // clone: the selection, once per build
                let (front, filter, args, all, ub) =
                    (front.clone(), filter.clone(), args.clone(), *all, *ub);
                let work = Step::of(&cancelled, Does::Select { front, filter, args, all, ub });
                deps.clear();
                deps.extend((0..count).map(|at| NodeId(first.0 + at)));
                let name = "the tests to evaluate";
                let mut fresh = w.plane.fresh(1, name.len(), deps.len());
                let select = w.plane.push(
                    &mut fresh,
                    node(NodeKind::Bodies, name, &deps, Pool::Work, None),
                    one(kinds::UB_TESTS),
                );
                Some((Evaluation { ub, select, failed: false }, fresh, work))
            }
            _ => None,
        };
        let (evaluation, select) = match select {
            Some((evaluation, node, work)) => (Some(evaluation), Some((node, work))),
            None => (None, None),
        };
        // alloc: once per build
        job.stage = Stage::Units(Box::new(Build {
            input,
            front,
            writer,
            held: Vec::new(), // alloc: none until a write holds a task
            first,
            units: units_of,
            package_of,
            reported,
            outputs,
            root,
            artifacts: Vec::new(), // alloc: once per build
            ready: Vec::new(),     // alloc: none until a test executable compiles
            evaluation,
            refused: None,
            fresh: fresh_units,
            verdicts,
            applying: applying.then_some(Applying { left: count, failed: false }),
            made_wait: (count > 0).then(|| {
                tracing::info_span!(
                    target: "blaze",
                    "the build's first made unit waits behind the units found kept"
                )
            }),
        }));
        if count > 0 {
            self.add(w, id, nodes, Some(work), true);
        }
        if applying {
            // Its end is its last node's settling ([`Jobs::settled`]); a build of nothing has
            // ended already.
            if count == 0 {
                self.units_applied(w, id, true);
            }
            return;
        }
        match select {
            Some((fresh, work)) => self.add(w, id, fresh, Some(work), true),
            None => self.close(w, id),
        }
    }

    /// `blaze test --ub`'s tests selected: each one evaluated on a worker, its result reported
    /// as it ends.
    fn evaluate(&mut self, w: &mut World<'_, S>, id: JobId, tests: Vec<String>) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let Stage::Units(build) = &job.stage else { return };
        let Some(evaluation) = &build.evaluation else { return };
        let count = u32::try_from(tests.len()).expect("under 2^32 tests");
        const TEST: &str = "test ";
        let names = tests.iter().map(|it| TEST.len() + it.len()).sum();
        let mut fresh = w.plane.fresh(count, names, 0);
        let mut name = String::new(); // alloc: one name buffer, reused by every test
        for (at, test) in (0u32..).zip(&tests) {
            name.clear();
            name.push_str(TEST);
            name.push_str(test);
            let task = Task::Thread { kind: kinds::EVALUATE, arg: at };
            w.plane.push(&mut fresh, node(NodeKind::Bodies, &name, &[], Pool::Work, None), task);
        }
        // clone: a refcount, once per evaluation
        let (front, tests, ub): (_, Arc<[String]>, _) =
            (build.front.clone(), tests.into(), evaluation.ub);
        let work = Step::of(&job.cancelled, Does::Evaluate { front, tests, ub });
        self.add(w, id, fresh, Some(work), true);
        self.close(w, id);
    }

    fn close(&mut self, w: &mut World<'_, S>, id: JobId) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if let (Some(request), false, false) = (job.request, job.closed, job.finished) {
            job.closed = true;
            w.plane.close(w.now, request);
        }
    }

    /// A unit begins: reported once for its package, as cargo says `Compiling` once.
    fn started_unit(&mut self, w: &mut World<'_, S>, node: NodeId) {
        let Some(id) = self.batches.job_of(node) else { return };
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let Stage::Units(build) = &mut job.stage else { return };
        let Some(at) = build.unit(node) else { return };
        if !build.begins(node) {
            return;
        }
        build.writer.began(at);
        let fresh = build.is_fresh(at);
        if fresh {
            build.writer.found_kept(at);
        } else {
            // The first unit this build makes begins: the wait behind the kept ones is over.
            build.made_wait = None;
        }
        match fresh {
            true => job.fresh += 1,
            false => job.compiled += 1,
        }
        if streams_units(&job.work)
            && let Some(began) = unit_of(&build.input.units, at)
        {
            w.said.push(Said::Report(id, Response::UnitBegan(began)));
        }
        let unit = &build.input.units.graph.units[at];
        let Some(package) = build.input.units.packages.get(build.package_of[at] as usize) else {
            return;
        };
        if !build.reported.insert(build.package_of[at]) {
            return;
        }
        let verb = match (fresh, build.input.kind, unit.mode) {
            (true, ..) => Verb::Fresh,
            (false, JobKind::Check, _) | (false, _, UnitMode::Check) => Verb::Checking,
            (false, ..) => Verb::Compiling,
        };
        let started = UnitStarted {
            package_id: package.id.clone(), // clone: the report owns it, once per package
            name: package.name.clone(),     // clone: the report owns it, once per package
            version: package.version.clone(), // clone: the report owns it, once per package
            source: package.source.clone(), // clone: the report owns it, once per package
            package: build.package_of[at],
            verb,
        };
        w.said.push(Said::Report(id, Response::Unit(started)));
    }

    /// A new run of the job's user code, and its first tag.
    fn run_tags(
        &mut self,
        id: JobId,
        node: Option<NodeId>,
    ) -> Result<(Handle<Run<S>>, u64), ApiError> {
        let (run, packed) =
            match self.runs.insert(Run { job: id, node, runs: None, killed: false, held: false }) {
                Ok(it) => it,
                // alloc: an error, once
                Err(error) => {
                    return Err(ApiError::new(
                        format!("no room for another run of user code: {error}"),
                        None,
                    ));
                }
            };
        let tags = self.setup.runs + packed * RUN_TAGS;
        if tags >= self.setup.runs_end {
            self.runs.remove(run);
            return Err(ApiError::new("the daemon has no tags left for a run of user code", None));
        }
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.runs += 1;
        }
        Ok((run, tags))
    }

    /// The run a tag is of.
    fn run_of(&self, tag: u64) -> Option<Handle<Run<S>>> {
        self.runs.find(tag.checked_sub(self.setup.runs)? / RUN_TAGS)
    }

    fn run_over(&mut self, run: Handle<Run<S>>) {
        if let Some(Run { job: id, .. }) = self.runs.remove(run)
            && let Some(job) = self.jobs.get_mut(key_of(id))
        {
            job.runs -= 1;
        }
    }

    /// A build script's run begins, on the loop.
    ///
    /// Every way out ends the node at the plane or keeps a run that will: a run node the loop
    /// lets go unended keeps its request, and so its job, open with nothing in flight (the
    /// 2026-11-02 self-build stall, whose held runs met a job already over).
    fn run_script(&mut self, w: &mut World<'_, S>, node: NodeId) {
        let Some(id) = self.batches.job_of(node) else {
            // alloc: a failure, once
            let why = format!(
                "build script run node {node} is of no batch of a job, so nothing would run it"
            );
            w.plane.ended(w.now, node, Outcome::Failed { why });
            return;
        };
        // A run a broken write refused ([`Refused`]) never runs its user code. The plane hands
        // a run over here, never through [`Jobs::post`], so its refusal is checked here: one
        // after a `links` dependency's run whose write broke (cargo's run-to-run edge), which
        // ended well at the plane, or one whose own write was part of what broke. It fails at
        // the plane naming that write and its unit.
        if let Some(Job { stage: Stage::Units(build), .. }) = self.jobs.get(key_of(id))
            && let Some(unit) = build.unit(node)
            && let Some(why) = build.refused.as_ref().and_then(|refused| refused.why(unit))
        {
            #[cfg(test)]
            REFUSED_UNITS.with_borrow_mut(|it| it.push((unit, why.to_owned()))); // alloc: a test's record
            w.plane.ended(w.now, node, Outcome::Failed { why: why.to_owned() }); // alloc: a refused run's why, once
            return;
        }
        // The plane starts a run once its compile's node ends, before the loop has placed what
        // it made: the run waits for its program to be all there ([`Jobs::release_held`]).
        if let Some(job) = self.jobs.get_mut(key_of(id))
            && waits_for_program(job, node)
        {
            #[cfg(test)]
            HELD_RUNS.with_borrow_mut(|it| it.push(node));
            job.held.push(node);
            return;
        }
        // A fresh unit's run replays what it kept: said before it can end.
        if let Some(Job { stage: Stage::Units(build), .. }) = self.jobs.get_mut(key_of(id))
            && build.unit(node).is_some_and(|at| build.is_fresh(at))
        {
            w.plane.replays(node);
        }
        self.started_unit(w, node);
        let unit = match self.jobs.get(key_of(id)) {
            Some(Job { stage: Stage::Units(build), .. }) => build.unit(node).ok_or_else(|| {
                // alloc: a failure, once
                format!("build script run node {node} is no unit of job {id}'s build, so nothing would run it")
            }),
            // alloc: a failure, once
            Some(job) => Err(format!(
                "{} never started: job {id} is past its build ({}) as the run comes to start",
                run_named(job, node),
                stage_name(&job.stage)
            )),
            // alloc: a failure, once
            None => Err(format!("build script run node {node} never started: job {id} is gone")),
        };
        let unit = match unit {
            Ok(unit) => unit,
            Err(why) => {
                w.plane.ended(w.now, node, Outcome::Failed { why });
                return;
            }
        };
        let (run, tags) = match self.run_tags(id, Some(node)) {
            Ok(it) => it,
            Err(error) => {
                w.plane.ended(w.now, node, Outcome::Failed { why: error.what.clone() }); // clone: an error, once
                if let Some(job) = self.jobs.get_mut(key_of(id)) {
                    job.over.get_or_insert((false, Some(error)));
                }
                return;
            }
        };
        let Some(Job { stage: Stage::Units(build), over, .. }) = self.jobs.get_mut(key_of(id))
        else {
            self.run_over(run);
            // alloc: a failure, once
            let why = format!(
                "build script run node {node} (unit {unit}) never started: job {id} left its build as the run was made"
            );
            w.plane.ended(w.now, node, Outcome::Failed { why });
            return;
        };
        let mut runs = match build.front.script(unit, tags) {
            Ok(runs) => runs,
            Err(error) => {
                w.plane.ended(w.now, node, Outcome::Failed { why: error.what.clone() }); // clone: an error, once
                over.get_or_insert((false, Some(error)));
                self.run_over(run);
                return;
            }
        };
        let mut out = std::mem::take(&mut self.out);
        let ended = runs.start(w.store, Some(&mut build.writer), w.effects, &mut out);
        let puts = runs.take_puts();
        self.run_put(w, id, puts);
        for response in out.drain(..) {
            w.said.push(Said::Report(id, response));
        }
        self.out = out;
        match ended {
            Some(ok) => {
                self.run_over(run);
                script_ended(w, node, ok);
            }
            None => {
                if let Some(it) = self.runs.get_mut(run) {
                    it.runs = Some(Box::new(runs)); // alloc: once per run of user code that waits on its effects
                }
            }
        }
    }

    /// A unit's compile on the loop: its diagnostics said, its puts and outputs stored and
    /// placed, and a proc macro's load asked of the writer, so the units after it, posted only
    /// once it is applied, expand with it.
    fn compiled(
        &mut self,
        w: &mut World<'_, S>,
        node: NodeId,
        unit: usize,
        result: Result<Compiled, ApiError>,
    ) {
        let Some(id) = self.batches.job_of(node) else { return };
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let streams = streams_units(&job.work);
        if let Stage::Units(build) = &mut job.stage {
            build.writer.compiled(unit);
        }
        let mut compiled = match result {
            Ok(compiled) => compiled,
            Err(error) => {
                if !job.cancelled.load(Ordering::Acquire) {
                    job.over.get_or_insert((false, Some(error)));
                }
                return;
            }
        };
        if let Stage::Units(build) = &mut job.stage
            && let Some(verdict) = build.verdicts.get_mut(unit)
        {
            *verdict = match compiled.ok {
                true => Verdict::Done,
                false => Verdict::Failed {
                    open: compiled.open,
                    // clone: a unit that failed, once
                    why: compiled.why.clone().unwrap_or_else(|| "it did not compile".to_owned()),
                },
            };
        }
        if let (Some(macros), Stage::Units(build)) = (compiled.macros.take(), &mut job.stage)
            && let Err(error) = build.writer.macros(unit, macros)
        {
            job.over.get_or_insert((false, Some(error)));
        }
        if let (Some(msl), Stage::Units(build)) = (compiled.gpu_msl.take(), &mut job.stage)
            && let Err(error) = build.writer.gpu_msl(unit, msl)
        {
            job.over.get_or_insert((false, Some(error)));
        }
        if let Stage::Units(build) = &job.stage
            && let Some(u) = build.input.units.graph.units.get(unit)
        {
            let kind = u.target.kind.first().map_or("", String::as_str);
            let package = package_named(&build.input.units, &u.pkg_id);
            for (_, how, _) in &compiled.links {
                // alloc: one a link, for the history
                job.linked.push((
                    format!("{} ({kind})", u.target.name),
                    package.map(str::to_owned),
                    how.placed(),
                ));
            }
        }
        job.links.append(&mut compiled.links);
        job.misses.append(&mut compiled.misses);
        // Said once its last diagnostic is, so the client prints cargo's "generated N warnings"
        // or "could not compile" there and not at the build's end. A unit that said nothing has
        // no such line.
        // clone: the report and its diagnostics each own the unit's name, once per unit
        let ended = compiled.diagnostics.iter().find_map(|it| it.unit.clone());
        // A test build's live view hears of every unit's end, not only those that said
        // something; a unit with nothing to say prints no line either way.
        let ended = match (ended, streams, &job.stage) {
            (None, true, Stage::Units(build)) => unit_of(&build.input.units, unit),
            (ended, ..) => ended,
        };
        for diagnostic in compiled.diagnostics {
            if said_once(&mut job.said, &diagnostic) {
                w.said.push(Said::Report(id, Response::Diagnostic(diagnostic)));
            }
        }
        if let Some(unit) = ended {
            w.said.push(Said::Report(id, Response::UnitEnded(unit)));
        }
        let named = match &job.stage {
            Stage::Units(build) => build.input.units.graph.units.get(unit),
            _ => None,
        };
        let mismatched = |store: &str, kind: blaze_cas::Kind, mismatch: blaze_cas::Mismatch| {
            let what = match named {
                // alloc: an error, once
                Some(it) => format!("unit {unit} (`{}` of {})", it.target.name, it.pkg_id),
                // alloc: an error, once
                None => format!("unit {unit}"),
            };
            // alloc: an error, once
            let what = format!(
                "{store} holds other bytes under {:?} than {what} made now, a {kind:?} value: {mismatch:?}",
                mismatch.key
            );
            // clone: an error, once
            ApiError::new(what, named.map(|it| it.pkg_id.clone()))
        };
        // On the loop before the build's answer when the unit is its last: named, with how
        // much is put (slow-is-p0-traced).
        let putting = (!compiled.puts.is_empty()).then(|| {
            tracing::info_span!(
                target: "blaze",
                "loop: a unit's records put into the store",
                puts = compiled.puts.len(),
                bytes = compiled.puts.iter().map(|it| it.bytes.len()).sum::<usize>(),
            )
            .entered()
        });
        for put in compiled.puts {
            let kind = put.kind;
            match w.store.put(put.key, put.kind, &put.bytes) {
                Ok(ticket) => self.put.add(ticket),
                Err(mismatch) => {
                    let error = mismatched("the store", kind, mismatch);
                    job.over.get_or_insert((false, Some(error)));
                }
            }
        }
        drop(putting);
        job.shared |= !compiled.share.is_empty();
        // Not waited on before the answer, and flushed only as the job ends: a share is a copy,
        // for the other daemons, of what `compiled.puts` put into this daemon's own store just
        // above, whose tickets the job does wait on. Losing it in a crash loses no answer: a
        // reader of the shared store takes a std record only with every output it names there
        // (`unit::compile::kept`, and `found`'s `header::check`), and every value is checked
        // against its hash, so a share torn or lost is a miss, compiled again by whoever asks.
        for put in compiled.share {
            let kind = put.kind;
            if let Err(mismatch) = w.std.put(put.key, put.kind, &put.bytes) {
                let error = mismatched("the standard library's shared store", kind, mismatch);
                job.over.get_or_insert((false, Some(error)));
            }
        }
        // The files the unit's artifact names are what the build's answer says was made:
        // the answer waits for those to be at their paths, and for nothing else placed.
        if let Some(artifact) = &compiled.artifact {
            let files = std::iter::once(artifact.path.as_str())
                .chain(artifact.filenames.iter().map(String::as_str))
                .chain(artifact.executable.as_deref());
            for file in files {
                let path = self.paths.intern(file);
                if job.names.insert(path)
                    && let Some(busy) = job.busy.get_mut(&path)
                    && !busy.named
                {
                    busy.named = true;
                    job.needed += busy.effects as usize;
                }
            }
        }
        let outputs = match (compiled.artifact, &mut job.stage) {
            (artifact, Stage::Units(build)) => {
                if streams
                    && let Some(it) = &artifact
                    && runs_as_test(it)
                    && let (Ok(unit), Ok(at)) =
                        (u32::try_from(unit), u32::try_from(build.artifacts.len()))
                {
                    build.ready.push((unit, at));
                }
                build.artifacts.extend(artifact);
                Some(build.outputs.clone()) // clone: a refcount, once per unit
            }
            _ => None,
        };
        let ids = outputs.as_ref().map_or(&[][..], |it| it.of(unit));
        for output in compiled.outputs {
            let path = self.paths.find(ids, &output.path);
            if self.paths.files[path as usize].is_none() {
                // alloc: an error, once
                let error = ApiError::new(
                    format!("{} is no path an output can be placed at", output.path),
                    Some(output.path),
                );
                if let Some(job) = self.jobs.get_mut(key_of(id)) {
                    job.over.get_or_insert((false, Some(error)));
                }
                continue;
            }
            self.place_output(w, id, path, output.key, output.executable, output.made);
        }
    }

    /// An output of job `id` placed at path `path`, or queued behind the effects in flight
    /// there ([`Queued`]): those of a placement an answered job left
    /// ([`Settling::placing`]), or of another job's. The later output is placed once they
    /// have ended ([`Jobs::release_queued`]), so a reader never finds the earlier value
    /// landing over the later one, and a look at the path's record is never of a file
    /// about to be replaced.
    fn place_output(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        path: u32,
        key: blaze_cas::Key,
        executable: bool,
        made: Option<Arc<Made>>,
    ) {
        #[cfg(test)]
        let planted = PLANT_NO_QUEUE.get();
        #[cfg(not(test))]
        let planted = false;
        if !planted && self.flying.contains_key(&path) {
            #[cfg(test)]
            QUEUED_OUTPUTS.with_borrow_mut(|it| it.push(self.paths.names[path as usize].to_string()));
            let waits = tracing::info_span!(
                target: "blaze",
                "an output waits for the path's placement in flight",
                function = &*self.paths.names[path as usize],
                awaits = "the placement of the path's last value, in flight",
            );
            self.hold(id, path);
            self.queued.push(Queued { job: id, path, key, executable, made, _waits: waits });
            return;
        }
        self.place_now(w, id, path, key, executable, made);
    }

    /// An output of job `id` placed at path `path`, where no effect is in flight.
    fn place_now(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        path: u32,
        key: blaze_cas::Key,
        executable: bool,
        made: Option<Arc<Made>>,
    ) {
        // A file this compile linked is placed from the bytes it made, whatever the path
        // holds: no file of the store is read, cloned or waited for, so the answer is
        // held by no clone and no write-back (docs/v2/plan/design/clone-out.md).
        if let Some(made) = made {
            self.place_made(w, id, path, key, executable, made);
            return;
        }
        let placed = self.paths.placed[path as usize]
            .as_ref()
            .is_some_and(|it| it.key == key && it.executable == executable && it.meta.is_some());
        if !placed {
            self.place(w, id, path, key, executable);
            return;
        }
        match self.own_tag(Own::Look { job: id, path, key, executable }) {
            Ok(tag) => {
                // clone: the driver's effect owns the file's name, once per output
                let Some(file) = self.paths.files[path as usize].clone() else { return };
                self.begin_placing(id, path);
                w.effects.push(Effect::Stat { file, tag });
            }
            Err(error) => self.placing_failed(id, path, error),
        }
    }

    /// Each output queued behind its path's effects in flight ([`Queued`]) whose path has
    /// none now, placed, in the order asked: the first of a path makes it busy again, so the
    /// rest of that path wait on. One whose job was dropped meanwhile (a stall) is let go.
    fn release_queued(&mut self, w: &mut World<'_, S>) {
        let mut at = 0;
        while at < self.queued.len() {
            if self.flying.contains_key(&self.queued[at].path) {
                at += 1;
                continue;
            }
            let Queued { job: id, path, key, executable, made, _waits } = self.queued.remove(at);
            drop(_waits);
            self.unhold(id, path);
            if self.owned(id) {
                self.place_now(w, id, path, key, executable, made);
            }
        }
    }

    /// Whether something still answers for job `id`'s placements: the job, or what it left
    /// to settle after its answer ([`Settling::placing`]). Neither for a job a stall dropped.
    fn owned(&self, id: JobId) -> bool {
        self.jobs.get(key_of(id)).is_some() || self.settling.iter().any(|it| it.job == id)
    }

    /// A placement of job `id` at path `path` failed with `error`. The job's error while it
    /// lives. Once it was answered the answer stands: the path is one it did not name, its
    /// record is dropped so the next build that makes it places it again, and the failure
    /// is a loss said as any other ([`Jobs::lose`]): in the daemon's log, in `blaze status`
    /// and by the checkout's next `blaze why`. Never silent.
    fn placing_failed(&mut self, id: JobId, path: u32, error: ApiError) {
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.over.get_or_insert((false, Some(error)));
            return;
        }
        let Some(it) = self.settling.iter().find(|it| it.job == id) else { return };
        // clone: a loss's names, once
        let (command, checkout) = (it.command.clone(), it.checkout.clone());
        if let Some(placed) = self.paths.placed.get_mut(path as usize) {
            *placed = None;
        }
        // alloc: a loss, once
        let what = format!(
            "an output its answer did not name was not placed after the answer, and the next build that \
             makes it places it again: {}",
            error.what
        );
        self.lose(id, &command, checkout.as_deref(), what);
    }

    /// The store's value of `key` placed at path `path` for job `id`.
    fn place(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        path: u32,
        key: blaze_cas::Key,
        executable: bool,
    ) {
        // The whole file the store kept to clone the path's last value from goes once nothing
        // clones it (unless the new value has the same bytes): a build dir rebuilt again and
        // again keeps no store file of each version.
        if let Some(before) = self.paths.placed[path as usize].take()
            && before.key != key
        {
            w.store.unplace(&before.key, &key, w.effects);
        }
        // What an answered job left goes on ([`Settling::placing`]); a dropped job's does not.
        match self.jobs.get_mut(key_of(id)) {
            Some(job) => job.placed_any = true,
            None => {
                if !self.owned(id) {
                    return;
                }
            }
        }
        // clone: the store's effect owns the file's name, once per placement
        let Some(file) = self.paths.files[path as usize].clone() else {
            let name = self.paths.names[path as usize].to_string(); // alloc: an error, once
            // alloc: an error, once
            let error =
                ApiError::new(format!("{name} is no path an output can be placed at"), Some(name));
            self.placing_failed(id, path, error);
            return;
        };
        // An executable is started from its path: a clone placed there is prewarmed.
        w.store.starts(&file, executable);
        match w.store.place(&key, file, w.effects) {
            Some(tag) => {
                self.placing.insert(tag, Placing { job: id, path, key, executable });
                self.begin_placing(id, path);
            }
            None => {
                let name = self.paths.names[path as usize].to_string(); // alloc: an error, once
                // alloc: an error, once
                let error = ApiError::new(
                    format!("the store has nothing under {key:?} to place at {name}"),
                    Some(name),
                );
                self.placing_failed(id, path, error);
            }
        }
    }

    /// `made`, the file of `key` that job `id`'s compile just linked, placed at path `path`
    /// from the bytes the link holds. A whole file goes as its runs
    /// ([`Jobs::place_made_as`]). A patch of the file before it ([`Made::Patch`]) goes as
    /// the patch where the path's record says it holds that file: the file is looked at
    /// first ([`Own::LookBase`]), since a file changed or removed since it was placed is no
    /// base. Where the record names another value or none, the whole file is placed.
    fn place_made(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        path: u32,
        key: blaze_cas::Key,
        executable: bool,
        made: Arc<Made>,
    ) {
        let based = match &*made {
            Made::Patch { base, .. } => self.paths.placed[path as usize]
                .as_ref()
                .is_some_and(|it| it.key == *base && it.executable == executable && it.meta.is_some()),
            Made::Whole(_) => false,
        };
        if !based {
            self.place_made_as(w, id, path, key, executable, &made, false);
            return;
        }
        match self.own_tag(Own::LookBase { job: id, path, key, executable }) {
            Ok(tag) => {
                // clone: the driver's effect owns the file's name, once per output
                let Some(file) = self.paths.files[path as usize].clone() else {
                    self.place_made_as(w, id, path, key, executable, &made, false);
                    return;
                };
                self.patching.insert(path, made);
                self.begin_placing(id, path);
                w.effects.push(Effect::Stat { file, tag });
            }
            Err(error) => self.placing_failed(id, path, error),
        }
    }

    /// [`Jobs::place_made`] once it is known what the path holds: `over` says it holds the
    /// file a patch is of, as it was placed. Then the patch's runs are written into a clone
    /// of that file (`blaze_cas::Store::place_patched`); else the whole file's runs into a
    /// file of holes (`blaze_cas::Store::place_made`), a patch's made now from what its link
    /// holds, which the span says (`place: a patch's whole file made`). Either is renamed
    /// onto the path. The whole file the store kept to clone the path's last value from
    /// goes, and none is made of this one.
    #[expect(clippy::too_many_arguments, reason = "a placement's parts, each its own")]
    fn place_made_as(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        path: u32,
        key: blaze_cas::Key,
        executable: bool,
        made: &Made,
        over: bool,
    ) {
        if let Some(before) = self.paths.placed[path as usize].take()
            && before.key != key
        {
            w.store.unplace_made(&before.key, w.effects);
        }
        // What an answered job left goes on ([`Settling::placing`]); a dropped job's does not.
        match self.jobs.get_mut(key_of(id)) {
            Some(job) => job.placed_any = true,
            None => {
                if !self.owned(id) {
                    return;
                }
            }
        }
        // clone: the store's effect owns the file's name, once per placement
        let Some(file) = self.paths.files[path as usize].clone() else {
            let name = self.paths.names[path as usize].to_string(); // alloc: an error, once
            // alloc: an error, once
            let error =
                ApiError::new(format!("{name} is no path an output can be placed at"), Some(name));
            self.placing_failed(id, path, error);
            return;
        };
        let tag = match (made, over) {
            // clone: the patch's runs, which the path's writes own (an effect owns its
            // bytes); once per path, the edit's bytes
            // An executable is started from its path: its clone is prewarmed. A debug file
            // is not.
            (Made::Patch { file: patch, .. }, true) => {
                w.store.place_patched(file, patch.clone(), executable, w.effects)
            }
            (Made::Patch { whole, .. }, false) => {
                let span = tracing::info_span!(
                    target: "blaze",
                    "place: a patch's whole file made",
                    function = &*self.paths.names[path as usize],
                    bytes = tracing::field::Empty,
                )
                .entered();
                let runs = whole();
                span.record("bytes", runs.bytes());
                w.store.place_made(file, runs, w.effects)
            }
            // clone: the runs, which the path's writes own (an effect owns its bytes); once
            // per path
            (Made::Whole(runs), _) => w.store.place_made(file, runs.clone(), w.effects),
        };
        self.placing.insert(tag, Placing { job: id, path, key, executable });
        self.begin_placing(id, path);
    }

    /// An effect of job `id` at path `path` asked: the file is not all there until it ends.
    /// Counted for the path whoever asked ([`Jobs::flying`]), and for the job ([`Jobs::hold`]).
    fn begin_placing(&mut self, id: JobId, path: u32) {
        let flying = self.flying.entry(path).or_default();
        *flying += 1;
        #[cfg(test)]
        MOST_AT_A_PATH.set(MOST_AT_A_PATH.get().max(*flying));
        self.hold(id, path);
    }

    /// An effect of [`Jobs::begin_placing`] ended; the job, if it is still there. The path's
    /// count ends whether it is or not: an output queued behind it waits for the effect, not
    /// for who asked it.
    fn end_placing(&mut self, id: JobId, path: u32) -> Option<&mut Job<S>> {
        if let blaze_hash::hash_map::Entry::Occupied(mut flying) = self.flying.entry(path) {
            *flying.get_mut() -= 1;
            if *flying.get() == 0 {
                flying.remove();
            }
        }
        self.unhold(id, path)
    }

    /// Job `id` has one more thing under way at path `path`: an effect in flight
    /// ([`Jobs::begin_placing`]) or an output waiting for the path ([`Queued`]). Counted by
    /// the job while it lives, one of [`Job::needed`] when its answer names the path; by
    /// what it left to settle once it was answered ([`Settling::placing`]); by nothing for a
    /// job a stall dropped.
    fn hold(&mut self, id: JobId, path: u32) {
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.placing += 1;
            let named = job.names.contains(&path);
            let named = {
                let busy = job.busy.entry(path).or_insert(Busy { effects: 0, named });
                busy.effects += 1;
                busy.named
            };
            if named {
                job.needed += 1;
            }
        } else if let Some(it) = self.settling.iter_mut().find(|it| it.job == id) {
            it.placing += 1;
        }
    }

    /// What [`Jobs::hold`] counted, ended; the job, if it is still there.
    fn unhold(&mut self, id: JobId, path: u32) -> Option<&mut Job<S>> {
        let Some(job) = self.jobs.get_mut(key_of(id)) else {
            if let Some(it) = self.settling.iter_mut().find(|it| it.job == id) {
                it.placing = it.placing.saturating_sub(1);
            }
            return None;
        };
        job.placing -= 1;
        if let blaze_hash::hash_map::Entry::Occupied(mut busy) = job.busy.entry(path) {
            if busy.get().named {
                job.needed -= 1;
            }
            busy.get_mut().effects -= 1;
            if busy.get().effects == 0 {
                busy.remove();
            }
        }
        Some(job)
    }

    /// Path `path` is one job `id`'s answer names ([`Job::names`]): what it has under way
    /// there counts in what its answer waits for, from now.
    fn name(&mut self, id: JobId, path: u32) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if job.names.insert(path)
            && let Some(busy) = job.busy.get_mut(&path)
            && !busy.named
        {
            busy.named = true;
            job.needed += busy.effects as usize;
        }
    }

    /// Each run [`Jobs::run_script`] held, asked again: one whose program is all placed now
    /// starts, the rest wait on.
    fn release_held(&mut self, w: &mut World<'_, S>) {
        for at in 0..self.order.len() {
            let id = self.order[at];
            while let Some(job) = self.jobs.get_mut(key_of(id))
                && let Some(ready) = job.held.iter().position(|&node| !waits_for_program(job, node))
            {
                let node = job.held.remove(ready);
                self.run_script(w, node);
            }
        }
    }

    /// Each test executable [`Jobs::compiled`] held, asked again: one whose unit's outputs
    /// have nothing in flight is all there, and is said as [`Response::TestReady`] so the
    /// command runs it while the rest builds; the rest wait on. One the build ends before is
    /// in its [`Response::Built`] only.
    fn release_ready(&mut self, w: &mut World<'_, S>) {
        for &id in &self.order {
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            let Job { stage: Stage::Units(build), busy, .. } = job else { continue };
            let mut at = 0;
            while at < build.ready.len() {
                let (unit, artifact) = build.ready[at];
                if build.outputs.of(unit as usize).iter().any(|path| busy.contains_key(path)) {
                    at += 1;
                    continue;
                }
                build.ready.remove(at);
                if let Some(it) = build.artifacts.get(artifact as usize) {
                    // clone: the report owns it, once per test executable
                    w.said.push(Said::Report(id, Response::TestReady(it.clone())));
                }
            }
        }
    }

    /// A tag of the loop's own for `own`, carrying its slot in [`Jobs::own`].
    fn own_tag(&mut self, own: Own) -> Result<Tag, ApiError> {
        match self.own.insert(own) {
            Ok((_, packed)) => Ok(Tag(self.setup.tags + packed)),
            // alloc: an error, once
            Err(_) => Err(ApiError::new(
                format!("the loop has 2^{OWN_SLOT_BITS} effects of its own in flight"),
                None,
            )),
        }
    }

    /// Path `path` placed with the value of `key` for job `id`: what is there now looked at
    /// for its record.
    fn record(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        path: u32,
        key: blaze_cas::Key,
        executable: bool,
    ) {
        // clone: the driver's effect owns the file's name, once per placement
        let Some(file) = self.paths.files[path as usize].clone() else { return };
        match self.own_tag(Own::Record { job: id, path, key }) {
            Ok(tag) => {
                self.begin_placing(id, path);
                self.paths.placed[path as usize] = Some(Placement { key, executable, meta: None, asked_ns: certify::wall_ns() });
                w.effects.push(Effect::Stat { file, tag });
            }
            Err(error) => self.placing_failed(id, path, error),
        }
    }

    /// What the store placed.
    pub fn placed(&mut self, w: &mut World<'_, S>, placed: Vec<Placed>) {
        for Placed { tag, to, result, bytes } in placed {
            let Some(Placing { job: id, path, key, executable }) = self.placing.remove(&tag) else {
                continue;
            };
            // What an answered job left goes on as under the job (its mode, its record); of
            // a job a stall dropped nothing does, and its path is free again.
            let live = match self.jobs.get_mut(key_of(id)) {
                Some(job) => {
                    job.placed += bytes;
                    true
                }
                None => false,
            };
            if !live && !self.owned(id) {
                self.end_placing(id, path);
                continue;
            }
            match result {
                Ok(()) if executable => match self.own_tag(Own::Mode { job: id, path, key }) {
                    Ok(tag) => w.effects.push(Effect::Mode { file: to, mode: 0o755, tag }),
                    Err(error) => {
                        self.end_placing(id, path);
                        self.placing_failed(id, path, error);
                    }
                },
                Ok(()) => {
                    self.end_placing(id, path);
                    self.record(w, id, path, key, false);
                }
                Err(why) => {
                    self.end_placing(id, path);
                    // alloc: an error, once
                    let error =
                        ApiError::new(
                            format!("placing {}: {why}", to.under(std::path::Path::new("/"))),
                            Some(to.under(std::path::Path::new("/")).to_string()),
                        );
                    self.placing_failed(id, path, error);
                }
            }
        }
        self.wind_up(w);
    }

    /// A completion of the jobs' own: a lock write, a mode, a look at a placed file, or one of
    /// user code's runs. One whose effect is gone (its slot freed, its job finished) is
    /// dropped.
    pub fn complete(&mut self, w: &mut World<'_, S>, completion: Completion) {
        let own = completion.tag.0.wrapping_sub(self.setup.tags);
        if own < 1 << 32 {
            if let Some(it) = self.own.find(own).and_then(|handle| self.own.remove(handle)) {
                self.completed(w, it, completion.done);
            }
            self.wind_up(w);
            return;
        }
        let Some(run) = self.run_of(completion.tag.0) else { return };
        let Some(Run { job: id, node, runs: Some(runs), .. }) = self.runs.get_mut(run) else {
            return;
        };
        let (id, node) = (*id, *node);
        let Some(job) = self.jobs.get_mut(key_of(id)) else {
            self.run_over(run);
            return;
        };
        let writer = match (&mut job.stage, node) {
            (Stage::Units(build), Some(_)) => Some(&mut build.writer),
            _ => None,
        };
        let mut out = std::mem::take(&mut self.out);
        let ended = runs.complete(completion, w.store, writer, w.effects, &mut out);
        let puts = runs.take_puts();
        self.run_put(w, id, puts);
        for response in out.drain(..) {
            w.said.push(Said::Report(id, response));
        }
        self.out = out;
        if let Some(ok) = ended {
            self.run_over(run);
            self.run_ended(w, id, node, ok);
        }
        self.wind_up(w);
    }

    /// What a run of user code put, taken after each of its steps (a start that ends at once
    /// too): its tickets settle after its job's answer, and so do those of every other job that
    /// may have read it ([`Jobs::put`], [`Settling`]). The store is flushed as that settles,
    /// not after every step.
    fn run_put(&mut self, w: &mut World<'_, S>, id: JobId, mut puts: RunPuts) {
        self.put.extend(&puts.tickets);
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.durable.extend(&puts.tickets);
            job.misses.append(&mut puts.misses);
            job.test_runs.append(&mut puts.test_runs);
        }
        if puts.retracted {
            // A retraction is flushed at once: every other checkout must stop being given
            // the value. A kept front end is lowered afresh too, which costs one lowering
            // for a record that is rare, whatever it retracts.
            w.store.flush(w.effects);
            w.macros.keeps().retracted();
        }
    }

    /// An effect of the loop's own done.
    fn completed(&mut self, w: &mut World<'_, S>, own: Own, done: Done) {
        match own {
            Own::Certify { job, ask } => self.certify_told(w, job, ask, done),
            Own::Record { job: id, path, key } => {
                self.end_placing(id, path);
                match (done, &mut self.paths.placed[path as usize]) {
                    (Done::Stat(Some(meta)), Some(placement)) if placement.key == key => {
                        placement.meta = Some(meta)
                    }
                    (_, placement) => *placement = None,
                }
            }
            Own::Look { job: id, path, key, executable } => {
                let unchanged = matches!(
                    (&done, &self.paths.placed[path as usize]),
                    (Done::Stat(Some(meta)), Some(placement)) if placement.key == key && placement.meta == Some(*meta)
                );
                self.end_placing(id, path);
                if !unchanged && self.owned(id) {
                    self.place(w, id, path, key, executable);
                }
            }
            Own::LookBase { job: id, path, key, executable } => {
                // The path's record is of the patch's base (checked as the look was
                // asked): the file is that one where it is as it was placed.
                let over = matches!(
                    (&done, &self.paths.placed[path as usize]),
                    (Done::Stat(Some(meta)), Some(placement)) if placement.meta == Some(*meta)
                );
                let made = self.patching.remove(&path);
                self.end_placing(id, path);
                if let Some(made) = made
                    && self.owned(id)
                {
                    self.place_made_as(w, id, path, key, executable, &made, over);
                }
            }
            Own::Write { job: id, path } | Own::Mode { job: id, path, .. } => {
                let failed = match &done {
                    Done::Failed(error) => {
                        let name = &self.paths.names[path as usize];
                        // alloc: an error, once
                        Some(ApiError::new(
                            format!("writing {name}: {error}"),
                            Some(name.to_string()),
                        ))
                    }
                    _ => None,
                };
                self.end_placing(id, path);
                if let Some(error) = failed {
                    self.placing_failed(id, path, error);
                } else if let Own::Mode { key, .. } = own
                    && self.owned(id)
                {
                    self.record(w, id, path, key, true);
                }
            }
        }
    }

    fn run_ended(&mut self, w: &mut World<'_, S>, id: JobId, node: Option<NodeId>, ok: bool) {
        match node {
            Some(node) => script_ended(w, node, ok),
            None => {
                let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
                job.stage = Stage::Over;
                self.worked(w, id);
                self.end(w, id, ok, None);
            }
        }
    }

    fn plane_finished(
        &mut self,
        w: &mut World<'_, S>,
        request: RequestId,
        ok: bool,
        stats: RequestStats,
    ) {
        // A kept front end's store is part of its job's build, which was marked as it finished.
        // Its node settled before its request finishes, which clears the job's `storing`: the
        // set says, not the job.
        if self.store_requests.remove(&request) {
            // No job looks at a store's request: its live model goes as it finishes.
            w.plane.forget(request);
            return;
        }
        blaze_profile::built(blaze_profile::Built {
            serial: u64::from(request.0),
            hits: stats.cached,
            misses: stats.ran,
            joined: stats.joined,
        });
        let jobs = &self.jobs;
        let Some(id) = self
            .order
            .iter()
            .copied()
            .find(|id| jobs.get(key_of(*id)).is_some_and(|it| it.request == Some(request)))
        else {
            return;
        };
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        // Everything from here to the job's end is on the loop before the build's answer:
        // each piece is a span of its own, a child of this one (slow-is-p0-traced).
        let _finishing = tracing::info_span!(
            target: "blaze",
            "loop: a build's request finished",
            request = request.0,
        )
        .entered();
        job.finished = true;
        let outcome = match (
            job.cancelled.load(Ordering::Acquire),
            ok && !matches!(job.over, Some((false, _))),
        ) {
            (true, _) => profile_history::Outcome::Cancelled,
            (false, true) => profile_history::Outcome::Ok,
            (false, false) => profile_history::Outcome::Failed,
        };
        // How its units stand, for its checkout's next build to say ([`Unfinished`]).
        if outcome != profile_history::Outcome::Ok
            && let Stage::Units(build) = &job.stage
        {
            job.left = Some(build.left());
        }
        // A build whose front end is kept has its run log named by the task that stores the
        // front end, after the answer ([`Steps::kept_run_log`]); any other's is named here,
        // before its front end goes. A documentation's front end is not kept.
        let kept_logs = !job.cancelled.load(Ordering::Acquire)
            && !matches!(job.work, Work::Doc { .. })
            && matches!(&job.stage, Stage::Units(build) if build.root.is_some() && build.applying.is_none());
        #[cfg(test)]
        ENDED_ROWS.with_borrow_mut(|it| it.push(command_line(&job.work))); // alloc: a test's record
        let ended = tracing::info_span!(
            target: "blaze",
            "loop: the build's rows for the history",
            run_log_named_here = !kept_logs,
        )
        .in_scope(|| job.ended(request, Some(stats), outcome, !kept_logs));
        #[cfg(test)]
        ENDED_LOST.with_borrow_mut(|it| {
            // alloc: a test's record
            it.extend(ended.misses.iter().filter(|miss| miss.step == SETTLING_STEP || miss.step == UNFINISHED_STEP).filter_map(
                |miss| match &miss.missed {
                    blaze_kernel::Missed::Always(why) => Some(why.clone()),
                    _ => None,
                },
            ));
        });
        self.rows.push(ended);
        let publishing =
            tracing::info_span!(target: "blaze", "loop: the build's kept expansions published")
                .entered();
        w.macros.flush();
        // The snapshot waits on the last ticket; a verdict the store refuses fails the job.
        let published = w.macros.keeps().publish(w.store, proc_macro_api::kept::take_puts());
        for ticket in &published.tickets {
            self.put.add(*ticket);
        }
        if !published.durable.is_empty() {
            w.store.flush(w.effects);
            self.wait_durable(&published.durable);
        }
        drop(publishing);
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if let Some(refused) = published.refused.into_iter().next() {
            job.over.get_or_insert((false, Some(ApiError::new(refused, None))));
        }
        job.stats = stats;
        if stats.ran > 0 {
            self.cold = true;
        }
        if !matches!(job.stage, Stage::Units(_)) || job.cancelled.load(Ordering::Acquire) {
            return;
        }
        let ok = ok && job.over.is_none();
        let runs = ok && matches!(job.work, Work::Test { ub: None, .. } | Work::Run { .. });
        let documents = ok && matches!(job.work, Work::Doc { .. });
        if let Stage::Units(build) = &job.stage
            && build.applying.is_some()
        {
            // That build's request is left open until the job's answer is posted: a finish
            // while it runs is a request closed or let go out of turn.
            let root = &build.input.config.invocation.root;
            // alloc: an error, once
            let what = format!(
                "the plane finished request {} of {} on {root} while its view still built the proc macros and build \
                 scripts its front end applies, in that request",
                request.0,
                work_word(&job.work)
            );
            let error = ApiError::new(what, Some(root.clone())); // clone: an error, once
            self.end(w, id, false, Some(error));
            return;
        }
        // What a failed build left unchecked, before its `Built` and the job's end.
        if let Stage::Units(build) = &job.stage
            && let Some(unchecked) = build.unchecked(!keeps_going(&job.work))
        {
            w.said.push(Said::Report(id, Response::Unchecked(unchecked)));
        }
        let Stage::Units(build) = std::mem::replace(&mut job.stage, Stage::Over) else { return };
        let Build { input, front, mut writer, artifacts, evaluation, root, units, .. } = *build;
        // A lint over every configuration denied (`blaze::dead` at `deny`) fails the build as
        // a unit's denied lint does.
        let mut denied = false;
        let finished = tracing::info_span!(
            target: "blaze",
            "loop: what the build's front end found at its end",
        )
        .in_scope(|| writer.finished(ok, &units));
        match finished {
            Ok(warnings) => {
                for warning in warnings {
                    denied |= warning.level == blaze_api::Level::Error;
                    w.said.push(Said::Report(id, Response::Diagnostic(warning)));
                }
            }
            Err(error) => {
                self.end(w, id, false, Some(error));
                return;
            }
        }
        let (ok, runs, documents) = (ok && !denied, runs && !denied, documents && !denied);
        let served = tracing::info_span!(
            target: "blaze",
            "loop: what the build was served of its dependencies",
        )
        .in_scope(|| writer.served());
        let served = match served {
            Ok(it) => it,
            Err(error) => {
                self.end(w, id, false, Some(error));
                return;
            }
        };
        if input.verbose > 0 {
            for note in served.iter().filter_map(noted).chain(writer.goals_noted()) {
                w.said.push(Said::Report(id, Response::Diagnostic(note)));
            }
        }
        // What the build's steps read, for its request's certificate ([`certify::Building`]):
        // of every build that passed, one that compiled too.
        if ok && let Some(building) = self.certify.building.iter_mut().find(|it| it.job == id) {
            let _reads = tracing::info_span!(target: "blaze", "request certificate: what the build's steps read").entered();
            building.steps_read(writer.env_named(), writer.read_outside(), writer.read_seen());
        }
        // A build that may become its checkout's kept answer and made nothing so far says
        // which sources its keys held by code text alone: what the lane answers a comment
        // edit by ([`noop::LastAnswer::still`]). Inputs and the store's index read, no query.
        if ok
            && job.compiled == 0
            && job.missed == 0
            && !job.placed_any
            && !job.shared
            && let Some(recorded) = self.recordings.iter_mut().find(|it| it.job == id)
        {
            let still = tracing::info_span!(
                target: "blaze",
                "no-op answer: the sources the build held by code text",
                files = tracing::field::Empty,
            );
            let vouched = still.in_scope(|| writer.still());
            if let Ok(files) = &vouched {
                still.record("files", files.len());
            }
            recorded.recording.vouch(vouched.map(|files| {
                files
                    .into_iter()
                    .map(|(path, edition)| StillFile { path, edition })
                    .collect() // alloc: once a build that walked and made nothing
            }));
            // And which variables of the request's environment its steps asked for by name:
            // with those a step reads unasked ([`read_unasked`]), all of the environment the
            // answer rests on ([`noop::LastAnswer::named`]). What the build noted, no query.
            let named = tracing::info_span!(
                target: "blaze",
                "no-op answer: the variables the build's steps read by name",
                names = tracing::field::Empty,
            );
            let read = named.in_scope(|| writer.env_named());
            match &read {
                Ok(names) => _ = named.record("names", names.len()),
                // Counted by its own span, so a round sees how often an answer is kept
                // that every variable is compared for, and for which script: it should be
                // only a build script whose run was not found kept (run outside the sandbox).
                Err(why) => tracing::info_span!(
                    target: "blaze",
                    "no-op answer: the variables read are not named, every variable is compared",
                    why = %why,
                )
                .in_scope(|| {}),
            }
            recorded.recording.vouch_env(read);
            // And the paths its steps read under its directories of outputs, beside its
            // units' own: no change heard at one is set aside ([`noop::LastAnswer::read`]).
            let read = tracing::info_span!(
                target: "blaze",
                "no-op answer: the paths the build's steps read under its outputs",
            )
            .in_scope(|| writer.read_outside());
            recorded.recording.vouch_reads(read);
        }
        // The documentation's front end is not kept: it sets `cfg(doc)`.
        let documenting = match documents {
            true => Some(writer),
            false => {
                if let Some(root) = root {
                    let kept =
                        tracing::info_span!(target: "blaze", "loop: the build's front end kept")
                            .in_scope(|| writer.keep());
                    match kept {
                        // clone: the checkout's path, once per build
                        Ok(kept) => self.store_kept(
                            w,
                            id,
                            root,
                            input.config.invocation.root.clone(),
                            kept,
                            u64::from(request.0),
                            kept_logs,
                        ),
                        Err(error) => {
                            self.end(w, id, false, Some(error));
                            return;
                        }
                    }
                }
                None
            }
        };
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.served = served;
            job.goal = super::unit::request_goal(&input);
        }
        let profile = input.units.finished.clone(); // clone: the report owns it, once per build
        let members = input.units.packages.iter().filter(|it| it.is_member).count();
        let members = u32::try_from(members).unwrap_or(u32::MAX);
        if let Some(writer) = documenting {
            w.said.push(Said::Report(id, Response::Built { members, profile, artifacts }));
            self.document(w, id, input, front, writer);
            return;
        }
        if !runs {
            let ok = ok && evaluation.is_none_or(|it| !it.failed);
            w.said.push(Said::Report(id, Response::Built { members, profile, artifacts }));
            self.worked(w, id);
            self.end(w, id, ok, None);
            return;
        }
        let (run, tags) = match self.run_tags(id, None) {
            Ok(it) => it,
            Err(error) => {
                w.said.push(Said::Report(id, Response::Built { members, profile, artifacts }));
                self.worked(w, id);
                self.end(w, id, false, Some(error));
                return;
            }
        };
        let Some(job) = self.jobs.get(key_of(id)) else {
            self.run_over(run);
            return;
        };
        let started = match &job.work {
            Work::Test { filter, all, args, ub, .. } => {
                front.tests(&artifacts, Selection { filter, args, all: *all, ub: *ub }, tags)
            }
            Work::Run { bin, args, .. } => front.run(&artifacts, bin.as_deref(), args, tags),
            _ => Err(ApiError::new("only a test or a run runs once built", None)),
        };
        w.said.push(Said::Report(id, Response::Built { members, profile, artifacts }));
        let runs = match started {
            Ok(runs) => runs,
            Err(error) => {
                self.run_over(run);
                self.worked(w, id);
                self.end(w, id, false, Some(error));
                return;
            }
        };
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.stage = Stage::Runs(run);
        }
        // The plane finishes once the last unit's node ends, before the loop has placed what
        // it made: a test binary or program linked last may not be at its path yet (a first
        // link's file not there at all, B2: every test's exec failed "No such file or
        // directory"). The run waits until its job has no placement in flight, as a build
        // script's run waits for its program ([`Jobs::release_held`]) and a streamed test
        // binary for its unit's outputs ([`Jobs::release_ready`]).
        if let Some(it) = self.runs.get_mut(run) {
            it.runs = Some(Box::new(runs)); // alloc: once per test or run of a job
            it.held = true;
        }
        self.release_runs(w);
    }

    /// Each job's tests or run [`Jobs::plane_finished`] held, asked again: one whose job has no
    /// placement in flight starts, everything the build placed all there; the rest wait on. One
    /// whose job ended meanwhile (a placement failed, or it was cancelled) ends unstarted, not
    /// ok, its job's error the answer.
    fn release_runs(&mut self, w: &mut World<'_, S>) {
        let jobs = &self.jobs;
        let ready = |it: &Run<S>| {
            it.held
                && jobs.get(key_of(it.job)).is_none_or(|job| job.placing == 0 || job.over.is_some())
        };
        if !self.runs.iter().any(|(_, it)| ready(it)) {
            return;
        }
        // alloc: once per job's tests or run, as it is let go
        let ready: Vec<Handle<Run<S>>> =
            self.runs.iter().filter_map(|(run, it)| ready(it).then_some(run)).collect();
        for run in ready {
            self.start_run(w, run);
        }
    }

    /// Held run `run` started, or ended unstarted when its job is over or gone.
    fn start_run(&mut self, w: &mut World<'_, S>, run: Handle<Run<S>>) {
        let Some(it) = self.runs.get_mut(run) else { return };
        it.held = false;
        let (id, runs) = (it.job, it.runs.take());
        let Some(mut runs) = runs else {
            self.run_over(run);
            return;
        };
        let Some(job) = self.jobs.get(key_of(id)) else {
            self.run_over(run);
            return;
        };
        if job.over.is_some() || job.cancelled.load(Ordering::Acquire) {
            self.run_over(run);
            self.run_ended(w, id, None, false);
            return;
        }
        #[cfg(test)]
        RUNS_STARTED_PLACING.with_borrow_mut(|it| it.push(job.placing));
        let mut out = std::mem::take(&mut self.out);
        let ended = runs.start(w.store, None, w.effects, &mut out);
        let puts = runs.take_puts();
        self.run_put(w, id, puts);
        for response in out.drain(..) {
            w.said.push(Said::Report(id, response));
        }
        self.out = out;
        match ended {
            Some(ok) => {
                self.run_over(run);
                self.run_ended(w, id, None, ok);
            }
            None => {
                if let Some(it) = self.runs.get_mut(run) {
                    it.runs = Some(runs);
                }
            }
        }
    }

    /// The build of the units a view's front end applies ended ([`Jobs::next`]), `ok` whether
    /// every one of them built: what its front end found wrong once all of it ran said, and the
    /// view handed to [`Jobs::applied`].
    fn units_applied(&mut self, w: &mut World<'_, S>, id: JobId, ok: bool) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let Stage::Units(build) = std::mem::replace(&mut job.stage, Stage::Over) else { return };
        let Build { input, front, mut writer, root, units, .. } = *build;
        match writer.finished(ok, &units) {
            Ok(warnings) => {
                for warning in warnings {
                    w.said.push(Said::Report(id, Response::Diagnostic(warning)));
                }
            }
            Err(error) => {
                self.end(w, id, false, Some(error));
                return;
            }
        }
        self.applied(w, id, ok, Ready { input, root, front: Some(front), writer: Some(writer) });
    }

    /// A view `ready` with the units its front end applies built ([`Jobs::next`]), `ok` whether
    /// all of them did: the wait's span closed, every write they asked for applied, and the view
    /// ready again, the job going on, in the same plane request, to its next view or its answer.
    /// A unit that did not build, or a write of one that did not apply, fails the job naming the
    /// view: its answer would read what a macro that never ran left out, or a crate without its
    /// build script's output.
    fn applied(&mut self, w: &mut World<'_, S>, id: JobId, ok: bool, mut ready: Ready<S>) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        job.applying = None;
        let work = work_word(&job.work);
        let root = &ready.input.config.invocation.root;
        let refused = |why: String| {
            // alloc: an error, once
            let what = format!(
                "the {work}'s answer on the view of {root} can't be told: the proc macros and build scripts its \
                 front end applies {why}"
            );
            ApiError::new(what, Some(root.clone())) // clone: an error, once
        };
        let error = match ready.writer.as_mut() {
            _ if !ok => Some(refused("did not all build".to_owned())), // alloc: an error, once
            None => Some(refused("were built with no writer of it".to_owned())), // alloc: an error, once
            Some(writer) => match writer.apply() {
                Err(error) => Some(refused(format!("did not apply: {}", error.what))),
                Ok(false) => {
                    Some(refused("were built, and a reader of it outlived the build".to_owned()))
                } // alloc: an error, once
                Ok(true) => writer.broke().map(|broke| {
                    let unit = match ready.input.units.graph.units.get(broke.unit) {
                        Some(unit) => format!("{} of {}", unit.target.name, unit.pkg_id),
                        None => format!("unit {}", broke.unit),
                    };
                    refused(format!("did not apply: {} of {unit}: {}", broke.what, broke.how))
                }),
            },
        };
        if let Some(error) = error {
            self.end(w, id, false, Some(error));
            return;
        }
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        job.ready.push(ready);
        self.next(w, id);
    }

    /// [`Work::Doc`]'s check built: the documentation made by one task of a plane request of
    /// its own, posted with the front end's writer, as no task reads the front end any more.
    fn document(
        &mut self,
        w: &mut World<'_, S>,
        id: JobId,
        input: JobInput,
        front: Arc<S::Front>,
        writer: S::Writer,
    ) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        job.ready.push(Ready { input, root: None, front: Some(front), writer: Some(writer) });
        let work = match answer(job, &self.paths, w.steps) {
            Ok(work) => work,
            Err(error) => {
                self.end(w, id, false, Some(error));
                return;
            }
        };
        // The job's last request, finished, is no build's live view any more: this one is.
        if let Some(last) = job.request {
            w.plane.forget(last);
        }
        let request = w.plane.open(w.now, false, job.idle);
        job.request = Some(request);
        job.opening();
        self.started.push(job.row(request, None, profile_history::Outcome::Running));
        job.finished = false;
        job.closed = false;
        let fresh =
            lone(w.plane, NodeKind::Bodies, "the documentation", Pool::Work, None, kinds::WALK);
        job.stage = Stage::Answer(fresh.first());
        self.add(w, id, fresh, Some(work), true);
        self.close(w, id);
    }

    /// `Work` of a build, test or run: what the plane and the units did.
    fn worked(&mut self, w: &mut World<'_, S>, id: JobId) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if !matches!(job.work, Work::Build(_) | Work::Test { .. } | Work::Run { .. }) {
            return;
        }
        let (stats, fresh, compiled) = (job.stats, job.fresh, job.compiled);
        let links = std::mem::take(&mut job.links);
        // The goal, with the functions of every image linked for it: an image of build-time
        // code says none, and counts in none.
        let tiers = job.goal.map(|goal| {
            links.iter().filter_map(|it| it.2).fold(blaze_api::Tiers::of(goal), |all, image| {
                all.with(image.optimized, image.functions)
            })
        });
        let worked = Worked {
            // Given as it is said, by the daemon, when the build is explained by then
            // (`Daemon::said_as`): the jobs have no reasons of their own.
            why: None,
            storage: None,
            reran: stats.ran,
            // What its history row took ([`Job::ended`], before this): the misses the
            // history's thread explains, so 0 here is an empty summary there.
            misses: job.missed,
            green: stats.cached + stats.joined,
            served: std::mem::take(&mut job.served),
            // The history's `build_id` of the same build ([`Job::row`]).
            build_id: job.request.map(|it| u64::from(it.0)),
            compiled,
            found: fresh,
            link: link_kind(&links),
            // alloc: one a link, at the request's end
            links: links
                .into_iter()
                .map(|(path, how, _)| blaze_api::Linked { how: how.how(), path })
                .collect(),
            tiers,
            critical_path_us: stats.critical_path_ns / 1000,
            steps_us: NodeKind::ALL
                .into_iter()
                .zip(stats.steps_ns)
                .filter(|it| it.1 > 0)
                .map(|(kind, ns)| (kind, ns / 1000))
                // alloc: one a kind that ran, at the request's end
                .collect(),
            placed: blaze_api::PlacedBytes::default(),
        };
        job.worked = Some(worked);
    }

    /// The daemon's tasks stalled ([`blaze_profile::Stall`]): every job with a plane request
    /// under way ends now, failed with `error`, its request cancelled and its build written
    /// to the history as stalled. Nothing of it is waited for: the daemon stops after this.
    /// Whether a job's write waits for readers of its front end: held by tasks, or by whoever
    /// took one that outlives its task, each of which ends on its own.
    pub fn writes_waiting(&self) -> bool {
        self.order.iter().any(|id| {
            self.jobs.get(key_of(*id)).is_some_and(
                |job| matches!(&job.stage, Stage::Units(build) if build.writer.waiting()),
            )
        })
    }

    /// How many kept front ends are being stored now ([`Jobs::store_kept`]).
    pub fn storing(&self) -> usize {
        self.jobs.values().filter(|it| it.storing.is_some()).count()
            + self.settling.iter().filter(|it| it.storing.is_some()).count()
    }

    /// The lowerings the loop holds for their root's kept front end to be stored, which have no
    /// plane node: what a stall names of them, by root and the store's node.
    pub fn held_lowerings(&self) -> Vec<HeldLowering> {
        (self.lowering.iter())
            .map(|it| HeldLowering {
                job: it.id,
                root: it.root,
                checkout: it.checkout.clone(), // clone: a stall, once per lowering held
                on: it.on.map(|node| node.0),
            })
            .collect() // alloc: a stall, once
    }

    /// What the loop holds node `node` back for, if it does: what a stall names of it.
    pub fn holding(&self, node: NodeId) -> Option<&'static str> {
        let job = self.batches.job_of(node).and_then(|id| self.jobs.get(key_of(id)))?;
        if job.held.contains(&node) {
            return Some(
                "a build script's run held until its program is placed (Jobs::release_held)",
            );
        }
        match &job.stage {
            Stage::Units(build) if build.held.iter().any(|(task, _)| task.id == node) => {
                Some("a task held until its job's write is applied (Jobs::apply_writes)")
            }
            _ => None,
        }
    }

    /// What loop task `node`, running at the plane and held for nothing ([`Jobs::holding`]),
    /// waits on as the loop sees it: what a stall names of it. Its run of user code started and
    /// waiting on its effects, made and never started, or none open at all: a node the loop
    /// took and let go without ending it, which nothing will ever end.
    pub fn run_wait(&self, node: NodeId) -> String {
        let Some(id) = self.batches.job_of(node) else {
            return "no job's batch has it: nothing of the loop will end it".to_owned(); // alloc: a stall, once
        };
        let Some(job) = self.jobs.get(key_of(id)) else {
            // alloc: a stall, once
            return format!("its job {id} is gone: nothing of the loop will end it");
        };
        let run = self
            .runs
            .iter()
            .find(|(_, it)| it.node == Some(node))
            .map(|(_, it)| (it.runs.is_some(), it.killed));
        let what = match run {
            Some((true, false)) => {
                "its run of user code started and waits on its effects, none of which is in flight"
            }
            Some((true, true)) => {
                "its run of user code was killed and waits on its effects to end, none of which is in flight"
            }
            Some((false, _)) => "its run of user code was made and never started",
            None => {
                "no run of it is open: the loop took it and let it go without ending it, so nothing will"
            }
        };
        // alloc: a stall, once
        format!("{}: {what}; job {id} is {}", run_named(job, node), stage_name(&job.stage))
    }

    pub fn stalled(&mut self, w: &mut World<'_, S>, error: &ApiError) {
        let jobs = &self.jobs;
        let stalled: Vec<JobId> = self
            .order
            .iter()
            .copied()
            .filter(|id| {
                // Any job a client waits on, with a plane request open or not: one a stall
                // found with no task to run is ended too, never left waiting.
                jobs.get(key_of(*id)).is_some_and(|it| {
                    !it.idle || (it.request.is_some() && !it.finished) || it.storing.is_some()
                })
            })
            .collect(); // alloc: once a stall
        for id in stalled {
            self.native_let_go(id, true);
            self.kill_runs(w, id);
            let Some(mut job) = self.jobs.remove(key_of(id)) else { continue };
            // Its live model ends with it.
            if let Some(request) = job.request {
                w.plane.forget(request);
            }
            // Its store's task may still run: what it gives back is dropped with the job.
            if let Some(storing) = job.storing.take() {
                self.roots.storing[storing.root as usize] -= 1;
                if w.plane.is_open(storing.request) {
                    w.plane.cancel(w.now, storing.request);
                }
            }
            // Its tasks, placings and runs may still write into its build dirs: their leases
            // outlive the job.
            // clone: refcounts, once a stalled job
            self.held_leases.push((id, Arc::clone(&job.cancelled), Arc::clone(&job.leases)));
            if let Some(request) = job.request {
                let stats = w.plane.stats(request);
                self.rows.push(job.ended(request, stats, profile_history::Outcome::Stalled, true));
                self.answered.push(u64::from(request.0));
                job.cancelled.store(true, Ordering::Release);
                if w.plane.is_open(request) {
                    w.plane.cancel(w.now, request);
                }
            }
            for queries in &job.queries {
                if job.built != Some(*queries) {
                    w.db.close(*queries);
                }
            }
            self.batches.remove(id);
            self.order.retain(|it| *it != id);
            self.ended = true;
            // clone: the error, once a job the stall ends
            w.said.push(Said::Finish(id, false, Some(error.clone()), job.built.zip(job.built_root)));
        }
        // The lowerings of jobs the stall ended wait for nothing now.
        let jobs = &self.jobs;
        self.lowering.retain(|it| jobs.get(key_of(it.id)).is_some());
        // What answered jobs left to settle is let go, each said as a loss: the daemon stops
        // after this, and waits for no store of a front end and no commit.
        // alloc: a stall, once
        let why = format!("the daemon stalled and stops: {}", error.what);
        self.let_go_of_settling(w, &why);
    }

    /// What answered jobs left to settle is let go for `why` (a stall, a stop that waited its
    /// bound): each store of a kept front end still under way is cancelled, its front end
    /// dropped with it, and each is said as a loss ([`Jobs::lose`]), never dropped silently.
    pub fn let_go_of_settling(&mut self, w: &mut World<'_, S>, why: &str) {
        let settling = std::mem::take(&mut self.settling);
        for mut it in settling {
            if let Some(storing) = it.storing.take() {
                self.roots.storing[storing.root as usize] -= 1;
                if w.plane.is_open(storing.request) {
                    w.plane.cancel(w.now, storing.request);
                }
            }
            // alloc: a loss, once
            let what = format!(
                "what the build left was not all stored, durable and placed when it was let go: {why}"
            );
            self.lose(it.job, &it.command, it.checkout.as_deref(), what);
        }
    }

    /// The daemon's store broke: every job still live fails with `error`, which names the store
    /// and why. A snapshot waiting for the store fails at once; every other one is ended (its
    /// plane request let go, its runs of user code killed) with the error as its answer, given
    /// as soon as nothing of it is left, so no client is told of the break by a hangup alone.
    /// What answered jobs left to settle is given up, each as a loss naming the break
    /// ([`Jobs::lose`]): their answers were sent and stand.
    pub fn store_broke(&mut self, w: &mut World<'_, S>, error: &ApiError) {
        self.wind_up_judging(w, true);
        // clone: the live jobs' ids, each turn the store is broken while jobs are live (at most
        // a turn or two: each is ended here and finishes once nothing of it is left)
        let live = self.order.clone();
        for id in live {
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            // Its tasks on the pool stop at their next look, so it finishes sooner.
            job.cancelled.store(true, Ordering::Release);
            let (ok, said) = job.over.get_or_insert((false, None));
            *ok = false;
            if said.is_none() {
                *said = Some(error.clone()); // clone: an error, once a job the break ends
            }
            self.end(w, id, false, None);
            self.kill_runs(w, id);
        }
        self.wind_up_judging(w, true);
    }

    /// `id` waits on no layout of a pinned native input now. One no job waits on is stopped
    /// when `stop` (the job was cancelled or ended), its entry kept until its result lands in
    /// [`Jobs::did`] so a job that joins it meanwhile clears the flag again; one whose layouts
    /// all settled is dropped.
    fn native_let_go(&mut self, id: JobId, stop: bool) {
        self.native_waiting.retain(|_, waiting| {
            waiting.jobs.retain(|it| *it != id);
            if waiting.jobs.is_empty() && stop {
                waiting.stop.store(true, Ordering::Release);
                return true;
            }
            !waiting.jobs.is_empty()
        });
    }

    fn cancel_job(&mut self, w: &mut World<'_, S>, id: JobId) {
        self.native_let_go(id, true);
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        job.cancelled.store(true, Ordering::Release);
        job.over.get_or_insert((false, None));
        if matches!(job.stage, Stage::Snapshot { .. }) {
            job.stage = Stage::Over;
        }
        refuse_held(w, &mut job.stage);
        end_held_runs(w, job, "was cancelled");
        if let (Some(request), false) = (job.request, job.finished) {
            // Finished at the plane, its word not yet read here: nothing is left to cancel.
            if w.plane.is_open(request) {
                w.plane.cancel(w.now, request);
            }
            // A cancelled request takes no more nodes, and no `Close` either.
            job.closed = true;
        }
        self.kill_runs(w, id);
    }

    /// The job's runs of user code killed once each: a run that ends at once is let go, the
    /// others end through their completions.
    fn kill_runs(&mut self, w: &mut World<'_, S>, id: JobId) {
        let mut ended = Vec::new(); // alloc: a cancel, once
        for (run, it) in self.runs.iter_mut() {
            if it.job != id || it.killed {
                continue;
            }
            it.killed = true;
            // Held for its job's placements, it never started: nothing of it runs to kill.
            if it.held {
                it.held = false;
                ended.push((run, it.node, false));
                continue;
            }
            if let Some(runs) = &mut it.runs
                && let Some(ok) = runs.cancel(w.effects)
            {
                ended.push((run, it.node, ok));
            }
        }
        for (run, node, ok) in ended {
            self.run_over(run);
            match node {
                Some(node) => script_ended(w, node, ok),
                None => {
                    if let Some(job) = self.jobs.get_mut(key_of(id)) {
                        job.stage = Stage::Over;
                    }
                }
            }
        }
    }

    /// Ends the job, not ok with `error` when one is given: what of it runs is let go, and it
    /// finishes once nothing of it is left.
    fn end(&mut self, w: &mut World<'_, S>, id: JobId, ok: bool, error: Option<ApiError>) {
        self.native_let_go(id, true);
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        match &mut job.over {
            Some((was, _)) => *was = *was && ok,
            None => job.over = Some((ok, error)),
        }
        refuse_held(w, &mut job.stage);
        end_held_runs(w, job, "ended");
        // A build's scripts that still run would apply their runs to a front end let go.
        let scripts = matches!(job.stage, Stage::Units(_)) && job.runs > 0;
        if !matches!(job.stage, Stage::Runs(..)) {
            job.stage = Stage::Over;
        }
        if let (Some(request), false) = (job.request, job.finished) {
            let (cancel, close) = at_its_end(w.plane.is_open(request), ok, job.closed);
            if cancel {
                w.plane.cancel(w.now, request);
            }
            if close {
                w.plane.close(w.now, request);
            }
            job.closed = true;
        }
        if scripts {
            self.kill_runs(w, id);
        }
    }

    /// Finishes every job that is over and has nothing left: its tasks, its placements and its
    /// runs; and a snapshot once all it waits for is durable.
    ///
    /// A job's answer is sent then, in this very winding up: once its plane request has
    /// finished, the files it names are at their paths with their whole contents (no
    /// placement of one in flight, [`Job::needed`]) and its runs of user code have ended.
    ///
    /// What must be true before the answer, decided once: every file the answer names is
    /// the file a reader finds at its path. `blaze build`'s `Built` names each unit's
    /// artifact files (cargo's `filenames`, the uplifted program among them) and a person's
    /// `./target/debug/app` or a script's next step opens that path the moment the command
    /// returns, with nothing of blaze between them and the file system: so the path has to
    /// hold the file, not an image in the daemon's memory that a later write lands. A run
    /// of the daemon's own (`blaze run`, `blaze test`, a build script) starts only once its
    /// job has nothing in flight at all ([`Jobs::release_runs`]), named or not.
    ///
    /// Nothing else holds the answer: not the outputs it does not name (a program's hashed
    /// copy under `deps`, dep-info, anything cargo's artifact lists none of), not the
    /// store's commit of what it put or read, not the store of its kept front end, not the
    /// front end's release, not the profile, not the image. Those settle after the answer
    /// ([`Settling`]): an output still being placed goes on exactly as under the job, a
    /// later job's output at the same path waits for it ([`Queued`]), a stop waits for it
    /// (`STOP_SETTLES_WITHIN`), and a crash before it lands leaves the old file or the new
    /// one whole, which the restarted daemon, knowing of no file it placed, places again.
    /// A snapshot alone waits for durability: that is what it is asked for.
    fn wind_up(&mut self, w: &mut World<'_, S>) {
        self.wind_up_judging(w, false);
    }

    /// [`Jobs::wind_up`], judging whether a wait for the store ran out of time only when
    /// `judge` is set ([`gave_up`]).
    fn wind_up_judging(&mut self, w: &mut World<'_, S>, judge: bool) {
        let began = blaze_profile::clock::mono_ns();
        self.wind_up_timed(w, judge);
        let took = blaze_profile::clock::mono_ns() - began;
        for job in self.jobs.values_mut() {
            job.wind_ups += 1;
            job.wind_up_ns += took;
        }
    }

    /// [`Jobs::wind_up_judging`], which counts it and its time for each job that lives on.
    fn wind_up_timed(&mut self, w: &mut World<'_, S>, judge: bool) {
        self.release_queued(w);
        self.release_held(w);
        self.release_ready(w);
        self.release_runs(w);
        self.apply_writes(w);
        let mut over = std::mem::take(&mut self.over);
        let mut heard = self.store_heard;
        let mut recheck = false;
        // Flushed once a winding up, for every job that waits ([`flush_once`]).
        let mut flushed = None;
        // Whether a kept front end of an answered build is still being stored: a snapshot
        // waits for it, and then for what it put.
        let storing = self.settling.iter().any(|it| it.storing.is_some());
        for &id in &self.order {
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            if storing && matches!(job.stage, Stage::Snapshot { .. }) && job.over.is_none() {
                // A build answered before the snapshot was asked may still have its kept front
                // end out being stored: what that puts is part of what the snapshot makes
                // durable. Bounded as any task's wait is, by the stall's watch.
                let what = "the kept front ends of answered builds, being stored";
                if job.held_wait.as_ref().is_none_or(|(_, was)| *was != what) {
                    let span = tracing::info_span!(
                        target: "blaze",
                        "the snapshot waits for what answered builds still store",
                        what,
                    );
                    job.held_wait = Some((span, what));
                }
                continue;
            }
            if let Stage::Snapshot { ticket: None, heads, store } = &mut job.stage
                && w.images.done()
            {
                // What the loop put since the snapshot was asked, once (the stores of answered
                // builds' kept front ends, which it waited for above), with what was put
                // before it.
                if job.durable_since.is_none() {
                    job.held_wait = None;
                    store.extend(&self.put);
                    store.extend(&w.store.last_puts());
                }
                // The store's puts before it, durable; or the wait given up
                // (bounded-progress): a broken store, or one that answers nothing.
                store.retain_pending(|it| w.store.durable(it));
                let refused = match store.iter().next() {
                    None => Some(None),
                    Some(pending) => {
                        recheck |= flush_once(&mut flushed, &mut heard, w.store, w.now, w.effects);
                        let since = *job.durable_since.get_or_insert(w.now);
                        gave_up(w.store, w.now, since, heard, judge).map(|why| {
                            // alloc: error path
                            Some(format!(
                                "the snapshot waits for the store's {pending}, put before it, to be durable, \
                                 and it is not: {why}"
                            ))
                        })
                    }
                };
                if let Some(refused) = refused {
                    let heads = std::mem::take(heads);
                    job.stage = Stage::Over;
                    match (refused, w.images.failed()) {
                        (Some(why), _) => job.over = Some((false, Some(ApiError::new(why, None)))),
                        (None, None) => {
                            w.said.push(Said::Report(id, Response::Snapshotted { heads }));
                            job.over = Some((true, None));
                        }
                        (None, Some(why)) => {
                            job.over = Some((false, Some(ApiError::new(why, None))))
                        }
                    }
                }
            }
            let Some((ok, error)) = &mut job.over else { continue };
            // What of its own is still under way, the first of them: named by a span while
            // the answer is held for it (every-wait-says-what). Each is something the answer
            // says: its plane request's outcome, the files it names at their paths, its runs'
            // results. An output it does not name holds nothing: it goes on after the answer.
            #[cfg(test)]
            let needed = if PLANT_ANSWER_WAITS_FOR_NO_OUTPUT.get() { 0 } else { job.needed };
            #[cfg(not(test))]
            let needed = job.needed;
            let held = if !job.finished {
                Some("its plane request, not finished")
            } else if needed != 0 {
                Some("its outputs, being placed")
            } else if job.runs != 0 {
                Some("its runs")
            } else if matches!(job.stage, Stage::Runs(..)) {
                Some("its stage of runs")
            } else {
                None
            };
            if let Some(what) = held {
                if job.held_wait.as_ref().is_none_or(|(_, was)| *was != what) {
                    // `function` and `awaits` are columns of an ad hoc span
                    // (`blaze_profile::span::ad_hoc_fields`); the rest are kept as text in
                    // `spans.fields`. `named_path` is the file the answer names that is
                    // still being placed (the first by id, so the row does not depend on a
                    // map's order), `needed` and `placing` how many effects are in flight
                    // at named paths and in all: a wait that is long says for which file.
                    let named_path = job
                        .busy
                        .iter()
                        .filter(|(_, busy)| busy.named)
                        .map(|(path, _)| *path)
                        .min()
                        .and_then(|path| self.paths.names.get(path as usize))
                        .map_or("none", |it| &**it);
                    let span = tracing::info_span!(
                        target: "blaze",
                        "the job's answer waits for what it still does",
                        function = what,
                        awaits = what,
                        named_path,
                        needed = job.needed,
                        placing = job.placing,
                        runs = job.runs,
                    );
                    job.held_wait = Some((span, what));
                }
                continue;
            }
            job.held_wait = None;
            over.push((id, *ok, error.take()));
        }
        for (id, ok, error) in over.drain(..) {
            let Some(mut job) = self.jobs.remove(key_of(id)) else { continue };
            // `wind_ups` and `wind_up_us`: the loop's windings up while the job lived, this
            // one not counted, and their time in all.
            let _answering = tracing::info_span!(
                target: "blaze",
                "loop: a job's answer",
                job = %id,
                ok,
                wind_ups = job.wind_ups,
                wind_up_us = job.wind_up_ns / 1000,
            )
            .entered();
            if let Some(recorded) = self.recordings.iter_mut().find(|it| it.job == id) {
                let made_nothing = ok
                    && error.is_none()
                    && job.compiled == 0
                    && job.missed == 0
                    && !job.placed_any
                    && job.placed == blaze_cas::PlacedBytes::default()
                    && !job.shared
                    && !job.cancelled.load(Ordering::Acquire);
                // clone: its `Work`, kept for the replays, once a build that asked for one
                recorded.walked = Some((made_nothing, job.worked.clone()));
            }
            // Its live model ends with it.
            if let Some(request) = job.request {
                w.plane.forget(request);
            }
            let placed = job.placed;
            tracing::info_span!(
                target: "blaze",
                "build placed",
                cloned = placed.cloned,
                written = placed.written,
                copied = placed.copied
            )
            .in_scope(|| {});
            if let Some(mut worked) = job.worked.take() {
                worked.placed = blaze_api::PlacedBytes {
                    cloned: placed.cloned,
                    written: placed.written,
                    copied: placed.copied,
                };
                w.said.push(Said::Report(id, Response::Work(worked)));
            }
            // What it shared is committed for the other daemons, which find it at their next look.
            if job.shared {
                w.std.flush(w.effects);
            }
            // What it leaves to settle after its answer: the store of its kept front end still
            // under way, and what it put or may have read that is not durable yet. Everything
            // the loop put by now and the last put of every segment of the store
            // ([`Store::last_puts`]) with its own: a value it read was handed back with no
            // ticket, so it may be another job's put, or a put no caller handed over (an error
            // path that put before it failed, a fetch).
            let mut stored = std::mem::take(&mut job.stored);
            stored.extend(&self.put);
            stored.extend(&w.store.last_puts());
            stored.retain_pending(|it| w.store.durable(it));
            let mut durable = std::mem::take(&mut job.durable);
            durable.retain_pending(|it| w.store.durable(it));
            let storing = job.storing.take();
            // A snapshot waited for all of it itself, or failed saying so; and a job that
            // failed as the store broke was answered with the break.
            let said = matches!(job.work, Work::Snapshot)
                || (!ok && storing.is_none() && w.store.broken().is_some());
            if said {
                stored = blaze_cas::Tickets::default();
                durable = blaze_cas::Tickets::default();
            }
            // The outputs it did not name that are still being placed (or wait for their
            // path): they go on after the answer, under what it leaves to settle, which
            // holds its build dirs' leases until the last has ended.
            let placing = job.placing;
            let placing_span = (placing != 0).then(|| {
                tracing::info_span!(
                    target: "blaze",
                    "a build's other outputs are placed after its answer",
                    of_job = %id,
                    placing,
                )
            });
            let leases = (placing != 0).then(|| Arc::clone(&job.leases)); // clone: a refcount, once a job
            if storing.is_some() || !stored.is_empty() || !durable.is_empty() || placing != 0 {
                let checkout = job.stored_of.take().or_else(|| {
                    // clone: the request's root, once a job that leaves something to settle
                    targets_of(&job.work).and_then(|it| it.cargo.as_ref()).map(|it| it.root.clone())
                });
                // alloc: once a job that leaves something to settle
                self.settling.push(Settling {
                    job: id,
                    command: command_line(&job.work),
                    checkout,
                    view: job
                        .views
                        .first()
                        .map_or_else(|| "none".to_owned(), |it| it.view.root.to_string()),
                    storing,
                    stored,
                    durable,
                    placing,
                    placing_span,
                    leases,
                    since: None,
                    wait: None,
                });
            }
            for queries in job.queries {
                if job.built != Some(queries) {
                    w.db.close(queries);
                }
            }
            self.batches.remove(id);
            self.order.retain(|it| *it != id);
            self.ended = true;
            // Its checkout's next build says a build or check that did not end well, and
            // forgets it once one does.
            if matches!(job.work, Work::Build(_) | Work::Check(_))
                && let Some((root, _)) = &job.checkout
            {
                self.roots.unfinished[*root as usize] = match ok {
                    true => None,
                    false => Some(Unfinished {
                        build: job.request.map(|it| it.0),
                        command: command_line(&job.work),
                        cancelled: job.cancelled.load(Ordering::Acquire),
                        left: job.left,
                    }),
                };
            }
            // Its profile is flushed from here on, never before ([`Jobs::answered`]).
            if let Some(request) = job.request {
                self.answered.push(u64::from(request.0));
            }
            // Lane packs-1112: a pack source or a peer that failed while builds ran is said
            // in the output of the build that ends next, once (`super::packs::take_notices`).
            if matches!(job.work, Work::Build(_) | Work::Check(_)) {
                for notice in super::packs::take_notices() {
                    w.said.push(Said::Report(id, Response::Diagnostic(warned(&notice))));
                }
            }
            w.said.push(Said::Finish(id, ok, error, job.built.zip(job.built_root)));
        }
        // What answered jobs left to settle: each waits for the store's commit of its tickets,
        // the store flushed while any does ([`flush_for_wait`]); and is done once they are
        // durable and its front end is back with its root. A store that broke never makes
        // them durable, nor does one that stopped committing: the wait is given up as a loss,
        // naming why and which, rather than held with no end (rule 18). The answer was sent
        // and stands: the loss is recorded ([`Jobs::lose`]), never a second `Finish`.
        let mut lost = Vec::new(); // alloc: none until a wait is given up
        self.settling.retain_mut(|it| {
            if it.placing == 0 {
                it.placing_span = None;
                it.leases = None;
            }
            it.stored.retain_pending(|ticket| w.store.durable(ticket));
            it.durable.retain_pending(|ticket| w.store.durable(ticket));
            let waiting = it
                .stored
                .iter()
                .next()
                .map(|ticket| (ticket, true))
                .or_else(|| it.durable.iter().next().map(|ticket| (ticket, false)));
            let Some((ticket, stored)) = waiting else {
                it.wait = None;
                it.since = None;
                return it.storing.is_some() || it.placing != 0;
            };
            recheck |= flush_once(&mut flushed, &mut heard, w.store, w.now, w.effects);
            if it.wait.is_none() {
                it.wait = Some(tracing::info_span!(
                    target: "blaze",
                    "what a build left settles after its answer: waits for the store's commit",
                    job = %it.job,
                    ticket = %ticket,
                    its_kept_front_end = stored,
                ));
            }
            let since = *it.since.get_or_insert(w.now);
            let Some(why) = gave_up(w.store, w.now, since, heard, judge) else { return true };
            // clone: a loss's names, once
            lost.push((it.job, it.command.clone(), it.checkout.clone(), it.lost(ticket, stored, &why)));
            it.stored = blaze_cas::Tickets::default();
            it.durable = blaze_cas::Tickets::default();
            it.wait = None;
            it.since = None;
            it.storing.is_some() || it.placing != 0
        });
        for (job, command, checkout, what) in lost {
            self.lose(job, &command, checkout.as_deref(), what);
        }
        // A commit a wait started is the store moving; a wait whose puts no commit takes looks
        // again shortly. Neither while the store is broken: its waits have been given up above.
        self.store_heard = heard;
        self.recheck = (recheck && w.store.broken().is_none()).then(|| w.now + RECHECK);
        self.over = over;
        if !self.retired.is_empty() {
            let jobs = &self.jobs;
            self.retired.retain(|view| {
                let read = jobs.values().any(|it| it.borrowed.contains(view));
                if !read {
                    w.db.close(*view);
                }
                read
            });
        }
        // A plan was made: what the plan before it held alone of the plan's database (each
        // plan's values, 24 MiB of them a plan of blaze) is let go of now. Until this the
        // database was collected only between requests (`Daemon::between`), so a build that
        // planned again as each build script ended kept every plan it had made: 914 segments
        // of 4 MiB, 3.76 GiB of the query tag's heap, sixteen minutes into a cold build of
        // blaze (lane mem-1111, 2026-11-11; `held_samples`, allocated in `plan`). The
        // collection is the one `between` runs, with the same root: it keeps what an open view
        // or the root reaches, and frees a memo only once a grace after its unlinking passed.
        if std::mem::take(&mut self.planned) {
            let _span = tracing::info_span!(
                target: "blaze",
                "plan made: collect the plan's database",
                retired = self.retired.len()
            )
            .entered();
            w.db.collect(&[w.steps.plan_key()], w.effects);
        }
    }

    /// When, on the injected clock, the first wait for the store to make puts durable gives up
    /// ([`DURABLE_WITHIN`] with no step of the store's commit answering, [`gave_up`]): a
    /// snapshot's, or that of what an answered job left to settle. The loop calls
    /// [`Jobs::expire`] then, if nothing woke it before.
    pub fn next_deadline(&self) -> Option<Duration> {
        let since = self
            .jobs
            .values()
            .filter_map(|it| it.durable_since)
            .chain(self.settling.iter().filter_map(|it| it.since))
            .min()?;
        let bound = since.max(self.store_heard) + DURABLE_WITHIN;
        Some(self.recheck.map_or(bound, |at| at.min(bound)))
    }

    /// The store took a completion of its own at `now` on the injected clock, its commit
    /// having answered `steps` steps ever ([`Store::commit_steps`]): only when that moved is
    /// the commit making progress, and waits for it start their deadline again.
    pub fn commit_moved(&mut self, now: Duration, steps: u64) {
        if steps != self.commit_steps {
            self.commit_steps = steps;
            self.store_heard = self.store_heard.max(now);
        }
    }

    /// The loop's clock reached [`Jobs::next_deadline`]: each snapshot whose wait for the store
    /// is past it fails, naming what it waited for, and each such wait of what an answered job
    /// left to settle is given up as a loss.
    pub fn expire(&mut self, w: &mut World<'_, S>) {
        self.wind_up_judging(w, true);
    }

    /// The VFS's tickets that are durable: a snapshot waits for its own.
    pub fn durable(&mut self, w: &mut World<'_, S>, tickets: &[Ticket]) {
        for job in self.jobs.values_mut() {
            if let Stage::Snapshot { ticket, .. } = &mut job.stage
                && ticket.is_some_and(|it| tickets.contains(&it))
            {
                *ticket = None;
            }
        }
        self.wind_up(w);
    }

    /// A snapshot: the image written, the journal and the store flushed, and every head as it
    /// stands, reported once all of it is durable.
    fn snapshot(&mut self, w: &mut World<'_, S>) -> Stage<S> {
        let front = self.front_db();
        w.images.ask(Parts {
            db: w.db,
            front,
            vfs: &mut *w.vfs,
            predict: w.plane.predict(),
            effects: &mut *w.effects,
        });
        // What it waits for, read before the flush: a put another thread makes between the
        // flush's drain and the read would be waited on and drained by no commit.
        let mut store = self.put.clone(); // clone: one ticket per segment, once per snapshot
        store.extend(&w.store.last_puts());
        w.store.flush(w.effects);
        w.std.flush(w.effects);
        // alloc: once per snapshot
        let heads: Vec<(String, ContentId)> =
            w.vfs.heads().into_iter().map(|(head, root)| (head.to_owned(), root)).collect();
        let ticket = heads.first().and_then(|(head, root)| w.vfs.set_head(head, *root).ok());
        w.vfs.flush(w.effects);
        Stage::Snapshot { ticket, heads, store }
    }
}

/// The task that answers a job's work on its ready views: a reach, the codegen inputs, the
/// tables, a trace, a verify, an edit, the units a build ahead picks, or a checked
/// documentation's pages.
fn answer<S: Steps>(job: &Job<S>, paths: &Paths<S>, steps: &Arc<S>) -> Result<Step<S>, ApiError> {
    let Some(ready) = job.ready.last() else {
        return Err(ApiError::new("the job has no view", None));
    };
    let not_lowered = || ApiError::new("the job's view is not lowered", None);
    // clone: a refcount, once per job
    let lowered = || ready.front.clone().ok_or_else(not_lowered);
    let does = match &job.work {
        Work::Reach { changes, .. } => {
            // clone: a refcount, once per job
            let from = job.ready.first().and_then(|it| it.front.clone()).ok_or_else(not_lowered)?;
            let changes = changes.clone(); // clone: the task owns the changes, once per job
            Does::Reach { to: lowered()?, from, changes }
        }
        Work::CodegenInputs(_) => Does::CodegenInputs { front: lowered()? },
        Work::Tables => {
            // alloc: once per tables job
            let images: Vec<(&'static str, u64, u64)> =
                paths.images.iter().flatten().flat_map(|it| it.kept.tables()).collect();
            Does::Tables { front: lowered()?, images }
        }
        Work::Trace(_) => {
            // clone: a refcount, once per job
            Does::Trace {
                db: ready.input.db.clone(),
                queries: ready.input.queries,
                root: steps.plan_key(),
            }
        }
        Work::Verify(_) => {
            let Some(view) = job.views.first().map(|it| it.view) else {
                return Err(ApiError::new("the job has no view", None));
            };
            // clone: refcounts, once per job
            Does::Verify { steps: steps.clone(), input: ready.input.clone(), view }
        }
        // clone: the task owns the edits, once per job
        Work::Edit(edits) => Does::Edit { front: lowered()?, edits: edits.clone() },
        // clone: the task owns the filter, once per job
        Work::Ahead { filter, .. } => Does::Ahead { front: lowered()?, filter: filter.clone() },
        Work::Doc { no_deps, private, .. } => {
            Does::Doc { front: lowered()?, no_deps: *no_deps, private: *private }
        }
        Work::Check(_) | Work::Build(_) | Work::Test { .. } | Work::Run { .. } | Work::Snapshot => {
            return Err(ApiError::new(
                "the job's work builds its units, answering nothing on its own",
                None,
            ));
        }
        Work::Lock(_) => return Err(ApiError::new("a lock job ends once its plan is made, answering nothing more", None)),
    };
    Ok(Step::of(&job.cancelled, does))
}

/// What a job's end does to its unfinished request at the plane, as `(cancel, close)`: cancel
/// it when not `ok`, else close it unless it was closed. One the plane finished already (not
/// `open`), its word not yet read here, has nothing left to cancel or close, and the plane
/// refuses both.
fn at_its_end(open: bool, ok: bool, closed: bool) -> (bool, bool) {
    if !open {
        return (false, false);
    }
    // A cancelled request takes no more nodes, and no `Close` either.
    if !ok {
        return (true, false);
    }
    (false, !closed)
}

/// The build script runs a job held for their programs ([`Jobs::run_script`]) ended failed at
/// the plane, as the job ends or is cancelled (`how`) before their programs are placed: none
/// of them starts now, and a run node left open would keep the job's request open with
/// nothing in flight, a stall (the 2026-11-02 self-build's). The job's own error, when it has
/// one, is said with each.
fn end_held_runs<S: Steps>(w: &mut World<'_, S>, job: &mut Job<S>, how: &str) {
    if job.held.is_empty() {
        return;
    }
    let held = std::mem::take(&mut job.held);
    let because =
        job.over.as_ref().and_then(|(_, error)| error.as_ref()).map(|it| it.what.as_str());
    for &node in &held {
        // alloc: a failure, once per run held
        let mut why = format!(
            "{} never started: its job {how} while its program was still being placed",
            run_named(job, node)
        );
        if let Some(because) = because {
            why.push_str(": ");
            why.push_str(because);
        }
        #[cfg(test)]
        ENDED_HELD.with_borrow_mut(|it| it.push((node, why.clone()))); // clone: a test's record
        w.plane.ended(w.now, node, Outcome::Failed { why });
    }
}

/// Build script run `node` of `job` as an error names it: its package and unit while the job
/// still has its build, its node alone after.
fn run_named<S: Steps>(job: &Job<S>, node: NodeId) -> String {
    let Stage::Units(build) = &job.stage else {
        return format!("build script run node {node}"); // alloc: an error, once
    };
    let Some(at) = build.unit(node) else {
        return format!("build script run node {node}"); // alloc: an error, once
    };
    let units = &build.input.units;
    let package =
        units.graph.units.get(at).and_then(|it| package_named(units, &it.pkg_id)).unwrap_or("?");
    format!("the build script run of `{package}` (unit {at}, node {node})") // alloc: an error, once
}

/// Whether a job's work keeps going past a failure: the plane builds every unit no failure
/// reaches, and a broken write fails only what depends on it. It does unless the request says
/// `--fail-fast` (the CLI's `Flags::fail_fast`), so one run shows every error; `--keep-going`
/// and `--no-fail-fast` are accepted and change nothing. A request with no cargo arguments
/// keeps going too.
fn keeps_going(work: &Work) -> bool {
    !targets_of(work)
        .and_then(|it| it.cargo.as_ref())
        .is_some_and(|cargo| cargo.args.iter().any(|arg| arg == "--fail-fast"))
}

/// A job's stage, as a stall or an error names it.
fn stage_name<S: Steps>(stage: &Stage<S>) -> &'static str {
    match stage {
        Stage::Library(_) => "reading the standard library",
        Stage::Locals(..) => "reading its paths outside the view again",
        Stage::Plan(..) => "planning",
        Stage::Fetch(_) => "fetching",
        Stage::Native(..) => "laying out its pinned native inputs",
        Stage::Lower(..) => "lowering its units",
        Stage::Units(_) => "building its units",
        Stage::Answer(_) => "answering",
        Stage::Ahead(_) => "choosing units ahead",
        Stage::Runs(_) => "running its tests or program",
        Stage::Snapshot { .. } => "snapshotting",
        Stage::Certify => "checking its request certificate",
        Stage::Over => "over",
    }
}

/// The reading tasks a job held for a write ended unposted: the job is over or cancelled.
fn refuse_held<S: Steps>(w: &mut World<'_, S>, stage: &mut Stage<S>) {
    if let Stage::Units(build) = stage {
        for (task, _) in build.held.drain(..) {
            w.plane.refuse(w.now, task, "cancelled".to_owned()); // alloc: a cancel, once per task held
        }
    }
}

/// Whether a job's work is a cargo `test` or `bench` build, whose command runs each test
/// executable as it is ready: such a build also says every unit's start and end
/// ([`Response::UnitBegan`], [`Response::UnitEnded`]) and each test executable once placed
/// ([`Response::TestReady`]).
fn streams_units(work: &Work) -> bool {
    let Work::Build(targets) = work else { return false };
    targets.cargo.as_ref().is_some_and(|it| it.command == "test" || it.command == "bench")
}

/// Whether `artifact` is a test executable the command runs: a test-profile target's, or a
/// doc test's.
fn runs_as_test(artifact: &Artifact) -> bool {
    artifact.executable.is_some()
        && (artifact.profile.test || artifact.kind == ArtifactKind::Doctest)
}

/// Unit `at` of `units` as a response names it; `None` for one past the graph, or whose package
/// or target the plan does not have (its diagnostics say so).
fn unit_of(units: &Units, at: usize) -> Option<UnitOf> {
    let unit = units.graph.units.get(at)?;
    let package = super::unit::package_of(units, unit).ok()?;
    Some(UnitOf {
        id: u32::try_from(at).ok()?,
        test: matches!(unit.mode, UnitMode::Test)
            || (matches!(unit.mode, UnitMode::Check) && unit.checked_as_test),
        doctest: matches!(unit.mode, UnitMode::Doctest),
        package_id: unit.pkg_id.clone(), // clone: the report owns it, once per unit
        manifest_path: package.manifest_path.clone(), // clone: the report owns it, once per unit
        primary: units.is_primary(at),
        target: super::unit::unit_target(unit).ok()?,
    })
}

/// Whether build script run `node` of `job` waits for its program: an output of the compile of
/// its script has an effect placing it in flight.
fn waits_for_program<S: Steps>(job: &Job<S>, node: NodeId) -> bool {
    if job.busy.is_empty() {
        return false;
    }
    let Stage::Units(build) = &job.stage else { return false };
    let Some(compile) =
        build.unit(node).and_then(|run| super::unit::compile_of(&build.outputs.units, run))
    else {
        return false;
    };
    build.outputs.of(compile).iter().any(|path| job.busy.contains_key(path))
}

fn script_ended<S: Steps>(w: &mut World<'_, S>, node: NodeId, ok: bool) {
    let outcome = match ok {
        true => Outcome::Built,
        false => Outcome::Failed { why: "the build script failed".to_owned() }, // alloc: a failure, once
    };
    w.plane.ended(w.now, node, outcome);
}

/// A warning of the load, as cargo prints it.
fn warned(warning: &str) -> Diagnostic {
    // alloc: a warning of the load, once per plan
    Diagnostic {
        level: Level::Warning,
        code: None,
        message: warning.to_owned(), // alloc: the report owns it
        spans: Vec::new(),           // alloc: none
        children: Vec::new(),        // alloc: none
        facts: Vec::new(),           // alloc: none
        rendered: format!("warning: {warning}\n"), // alloc: the report owns it
        unit: None,
        explanation: None,
    }
}

/// What a request's links did, as [`Worked::link`] says it: placed whole when any image was laid
/// out whole (a first link, or one that could not patch the image kept), else patched in place.
pub(crate) fn link_kind(links: &[(String, Linking, Option<blaze_api::Tiers>)]) -> LinkKind {
    match links.iter().any(|(_, how, _)| how.placed()) {
        true => LinkKind::Placed,
        false => LinkKind::Patched,
    }
}

/// Under `-v`, what a build was refused of the store for dependency `served`, as a note; none
/// when both its parts were served or checked.
fn noted(served: &ServedCrate) -> Option<Diagnostic> {
    let why = served.why.as_deref()?;
    let refused = |part: ServedPart| part == ServedPart::Refused;
    let parts = match (refused(served.outcome), refused(served.bodies)) {
        (true, true) => "its check outcome and bodies are",
        (true, false) => "its check outcome is",
        (false, true) => "its bodies are",
        (false, false) => return None,
    };
    // alloc: under `-v`, once per dependency refused
    let message = format!("dependency `{}`: {parts} not kept in the store: {why}", served.package);
    Some(Diagnostic::note(message))
}

/// Every memo reachable from `from` in `db`, each once, in the order it is reached.
fn walk(db: &Database, from: QueryKey) -> Vec<blaze_query::Memo> {
    let mut seen = HashSet::new_in(Global);
    let mut queue = vec![from]; // alloc: a trace or a verify, once
    let mut memos = Vec::new(); // alloc: a trace or a verify, once
    while let Some(key) = queue.pop() {
        if !seen.insert(key) {
            continue;
        }
        let Some(memo) = db.memo(&key) else { continue };
        queue.extend(memo.edges.iter().rev().map(|edge| edge.read));
        memos.push(memo);
    }
    memos
}

/// The memos of the view's last build, from its plan down.
pub fn trace(db: &Database, view: ViewId, from: QueryKey) -> Vec<Traced> {
    walk(db, from)
        .into_iter()
        .map(|memo| Traced {
            ran: db.why(view, &memo.key).is_some(),
            key: memo.key,
            fingerprint: memo.fingerprint,
            edges: memo.edges,
        })
        .collect() // alloc: a trace, once
}

/// The view's plan asked again in a database of its own from nothing, and every result
/// compared with the daemon's.
fn verify<S: Steps>(
    steps: &S,
    input: &JobInput,
    view: blaze_vfs::View,
) -> Result<Vec<Mismatch>, Failed> {
    // alloc: a verify's own tree, once
    let config =
        blaze_query::Config { tree: Some(Arc::new(input.files.clone())), dir: None, tags: 0 };
    let mut scratch = Database::new(config);
    steps.register(&mut scratch);
    let queries = scratch.open_with(view, &input.config.encode());
    steps.plan(&scratch, queries)?;
    let mut mismatches = Vec::new(); // alloc: a verify, once
    for memo in walk(&scratch, steps.plan_key()) {
        let Some(ours) = input.db.memo(&memo.key) else { continue };
        if ours.fingerprint != memo.fingerprint {
            mismatches.push(Mismatch {
                query: memo.key,
                incremental: ours.fingerprint,
                scratch: memo.fingerprint,
                fact: None,
            });
        }
    }
    Ok(mismatches)
}
