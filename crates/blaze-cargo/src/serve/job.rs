//! The daemon's [`Builder`] (docs/v2/plan/lanes/cargo/notes.md section 6): each job of the
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

mod slab;
#[cfg(test)]
mod tests;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use blaze_api::{
    ApiError, Artifact, ArtifactKind, Diagnostic, Edit, FetchVerb, Level, LinkKind, Mismatch, Response,
    ServedCrate, ServedPart, Targets, TestOutcome, TestResult, Traced, Ub, UnitOf, UnitStarted, Verb,
    Worked,
    build::{Builder, JobId, Opened, Work},
};
use blaze_cas::{Placed, Store};
use blaze_core::{Completion, Done, Effect, FileName, Meta, Tag};
use blaze_graph::{
    Key, Outcome, Pool, RequestId, RequestStats, admission::TaskKind, machine::NodeId,
    plane::NodeRef,
};
use blaze_hash::{HashMap, HashSet};
use std::alloc::Global;
use blaze_history::History;
use blaze_ids::{ContentId, Hasher, bits::BitSet};
use blaze_profile::{
    facts::{CrateDepRow, DepKind, LinkOutcome, LinkRow},
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
use parking_lot::Mutex;

use blaze_mem::{Handle, MemError, Tag as MemTag};

use self::slab::{Table, Tagged};
use super::{
    heads::Heads,
    image::{Images, Parts},
    mounts::Mounts,
    observe::Ended,
    plane::{self, Did, Fresh, Happened, Plane, Posting, Task, kinds, loops},
    work::{
        Broke, Compiled, Failed, Front, FrontEnd, Goal, Image, JobInput, Kept, KeptDb, Library,
        Linking, Phase, Readers, RunPuts, Runs, Selection, Source, Steps, Writer,
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
    Planned(Result<Planned, ApiError>),
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
    /// given back: none when its store panicked, as it may be half written.
    StoredKept(Option<S::KeptFront>, Result<Vec<super::work::KeptPut>, ApiError>),
}

/// What jobs tell the session, in order.
#[derive(Debug, PartialEq)]
pub enum Said {
    Report(JobId, Response),
    /// The job over, ok or not, and the database view its build ran in, handed to the session
    /// to keep or close ([`blaze_api::session::Session::finish_in`]); none when the job opened none.
    Finish(JobId, bool, Option<ApiError>, Option<ViewId>),
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

/// The single-flight key of the standard library's source.
fn library_key() -> Key {
    Key(ContentId::of("blaze v2 serve library", b"library").0)
}

/// The single-flight key of a fetch: every job asking the same source shares one download.
fn fetch_key(fetch: &Fetch) -> Key {
    let mut hasher = Hasher::new("blaze v2 serve fetch");
    hasher.part(fetch.at.as_bytes()).part(fetch.url.as_bytes());
    match &fetch.kind {
        FetchKind::Crate { sha256 } => hasher.part(b"crate").part(sha256.as_bytes()),
        FetchKind::IndexFile => hasher.part(b"index"),
        FetchKind::Git { commit } => hasher.part(b"git").part(commit.as_bytes()),
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
    /// Task `at` fetches `fetches[at]`.
    Fetch { steps: Arc<S>, fetches: Arc<[Fetch]> },
    /// The head's last front end, taken by the run that lowers into it.
    /// Its build dirs leased first ([`lease_build_dirs`]) into `leases`, against `checkout`,
    /// `builds` the dir of every workspace's build dir.
    Lower { steps: Arc<S>, input: JobInput, kept: Mutex<Option<S::KeptFront>>, leases: Arc<Mutex<Leases>>, checkout: String, builds: String },
    /// Task `at` compiles unit `at`. A compile that gives way puts the images it was given in
    /// `back` ([`Paths::back`]), so its next try still relinks them.
    Compile { front: Arc<S::Front>, back: Arc<SegQueue<Image<S>>> },
    Select { front: Arc<S::Front>, filter: Vec<String>, args: Vec<String>, all: bool, ub: Ub },
    /// Task `at` evaluates `tests[at]`.
    Evaluate { front: Arc<S::Front>, tests: Arc<[String]>, ub: Ub },
    Reach { to: Arc<S::Front>, from: Arc<S::Front>, changes: Vec<Change> },
    CodegenInputs { front: Arc<S::Front> },
    /// With the linker's tables of the images kept as it was posted.
    Tables { front: Arc<S::Front>, images: Vec<(&'static str, u64, u64)> },
    Trace { db: Arc<Database>, queries: ViewId, root: QueryKey },
    Verify { steps: Arc<S>, input: JobInput, view: blaze_vfs::View },
    Edit { front: Arc<S::Front>, edits: Vec<Edit> },
    Ahead { front: Arc<S::Front>, filter: Vec<String> },
    /// Posted with the job's writer, once its check is built and nothing reads its front end.
    Pack { front: Arc<S::Front>, program: String },
    /// As [`Does::Pack`] is: the documentation sets `cfg(doc)` on the crates it documents.
    Doc { front: Arc<S::Front>, no_deps: bool, private: bool },
    /// A job's front end as it is kept, what its build left for the store written from it
    /// ([`Steps::store_kept`]); taken by the task and given back in its value.
    StoreKept { steps: Arc<S>, kept: Mutex<Option<S::KeptFront>>, checkout: String },
}

impl<S: Steps> Step<S> {
    pub(super) fn of(cancelled: &Arc<AtomicBool>, does: Does<S>) -> Step<S> {
        Step { cancelled: cancelled.clone(), does } // clone: a refcount, once per batch
    }

    /// How a task's result settles its node, and its value for the loop. A give-way posts it
    /// again, unless the job is cancelled.
    fn settle<R>(&self, result: Result<R, Failed>, value: impl FnOnce(Result<R, ApiError>) -> Value<S>) -> Option<Did<Value<S>>> {
        match result {
            Ok(it) => Some(Did { outcome: Outcome::Built, value: value(Ok(it)) }),
            Err(Failed::Error(error)) => {
                // clone: a failed task's error, for the plane and the loop
                Some(Did { outcome: Outcome::Failed { why: error.what.clone() }, value: value(Err(error)) })
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

    fn run(&self, arg: u32, readers: Readers<S>) -> Option<Did<Value<S>>> {
        let at = arg as usize;
        let _span = match &self.does {
            Does::Library { .. } => tracing::info_span!("std library"),
            Does::Plan { .. } => tracing::info_span!("plan"),
            Does::Fetch { .. } => tracing::info_span!("fetch"),
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
            Does::Pack { .. } => tracing::info_span!("pack"),
            Does::Doc { .. } => tracing::info_span!("doc"),
            Does::StoreKept { checkout, .. } => tracing::info_span!(target: "blaze", "store kept front end", checkout = %checkout),
        }
        .entered();
        match &self.does {
            Does::Library { steps, kept } => self.settle(steps.library(kept).map_err(Failed::Error), Value::Library),
            Does::Plan { steps, db, queries } => self.settle(steps.plan(db, *queries), Value::Planned),
            Does::Fetch { steps, fetches } => {
                let fetched = steps.fetch(&fetches[at]).map_err(Failed::Error);
                // clone: a refcount per fetch
                self.settle(fetched, |it| Value::Fetched(fetches.clone(), arg, it))
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
                    match lease_build_dirs(&input.units, checkout, builds) {
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
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| front.compile(reader, unit, phase, &mut images)));
                // Why the unit failed, when its compile panicked or erred, for its node.
                let mut failed_why = None;
                let compiled = match caught {
                    Ok(it) => it,
                    Err(payload) if payload.is::<blaze_query::db::Cancelled>() || payload.is::<blaze_graph::Cancelled>() => {
                        Err(Failed::Again)
                    }
                    Err(payload) => {
                        let panic = plane::panicked(&*payload);
                        // The images it was relinking may be half written.
                        images.clear();
                        let refused = front.refused(unit, format!("blaze {panic}, making this unit")); // alloc: a panic, once
                        failed_why = Some(panic.to_string()); // alloc: a panic, once
                        Ok(Some(Compiled {
                            diagnostics: vec![refused], // alloc: a panic, once
                            ok: false,
                            puts: Vec::new(),
                            share: Vec::new(),
                            outputs: Vec::new(),
                            artifact: None,
                            macros: None,
                            links: Vec::new(),
                            misses: Vec::new(),
                        }))
                    }
                };
                if matches!(compiled, Err(Failed::Again)) && !self.cancelled.load(Ordering::Acquire) {
                    for image in images {
                        back.push(image);
                    }
                    return None;
                }
                let compiled = match compiled {
                    Ok(None) => return Some(Did { outcome: Outcome::Built, value: Value::Phase(unit, images) }),
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
                            Some(at) => format!("blaze could not make this unit: {} (at {at})", error.what),
                            // alloc: an error, once
                            None => format!("blaze could not make this unit: {}", error.what),
                        };
                        failed_why = Some(what.clone()); // clone: the node's why beside the unit's error, once
                        Ok(Compiled {
                            diagnostics: vec![front.refused(unit, what)], // alloc: an error, once
                            ok: false,
                            puts: Vec::new(),
                            share: Vec::new(),
                            outputs: Vec::new(),
                            artifact: None,
                            macros: None,
                            links: Vec::new(),
                            misses: Vec::new(),
                        })
                    }
                    Err(failed) => Err(failed),
                };
                let ok = matches!(&compiled, Ok(it) if it.ok);
                let mut did = self.settle(compiled, |it| Value::Compiled(unit, it, images))?;
                if !ok && matches!(did.outcome, Outcome::Built) {
                    let why = failed_why.unwrap_or_else(|| "it did not compile".to_owned()); // alloc: a unit that failed, once
                    did.outcome = Outcome::Failed { why };
                }
                Some(did)
            }
            Does::Select { front, filter, args, all, ub } => {
                let selection = Selection { filter, args, all: *all, ub: Some(*ub) };
                self.settle(front.ub_tests(this(readers), selection), Value::Selected)
            }
            Does::Evaluate { front, tests, ub } => self.settle(front.evaluate(this(readers), &tests[at], *ub), Value::Evaluated),
            Does::Reach { to, from, changes } => {
                let from_reader = readers.from.expect("a reach is posted with a reader of the view it reaches from");
                let reached = to.reach(this(Readers { from: None, ..readers }), from, from_reader, changes);
                self.settle(reached, |it| Value::Answer(it.map(Response::Reached)))
            }
            Does::CodegenInputs { front } => {
                self.settle(front.codegen_inputs(this(readers)), |it| Value::Answer(it.map(Response::CodegenInputs)))
            }
            Does::Tables { front, images } => {
                let writer = readers.writer.expect("the tables' task is posted with its job's writer");
                self.settle(front.tables(writer, images), |it| Value::Answer(it.map(Response::Tables)))
            }
            Does::Trace { db, queries, root } => {
                self.settle(Ok(trace(db, *queries, *root)), |it| Value::Answer(it.map(Response::Trace)))
            }
            Does::Verify { steps, input, view } => {
                let verified = verify(&**steps, input, *view);
                self.settle(verified, |it| Value::Answer(it.map(|mismatches| Response::Verified { mismatches })))
            }
            Does::Edit { front, edits } => {
                self.settle(front.edit(this(readers), edits), |it| Value::Answer(it.map(Response::Edited)))
            }
            Does::Ahead { front, filter } => self.settle(front.ahead(this(readers), filter), Value::Ahead),
            Does::Pack { front, program } => {
                let writer = readers.writer.expect("the pack's task is posted with its job's writer");
                self.settle(front.pack(writer, program), |it| Value::Answer(it.map(|pack| Response::Packed { pack })))
            }
            Does::Doc { front, no_deps, private } => {
                let writer = readers.writer.expect("the documentation's task is posted with its job's writer");
                self.settle(front.doc(writer, *no_deps, *private), Value::Answers)
            }
            Does::StoreKept { steps, kept, .. } => {
                let Some(mut front) = kept.lock().take() else {
                    let error = ApiError::new("the kept front end to store was taken before its task ran", None);
                    return self.settle(Err(Failed::Error(error)), |it| Value::StoredKept(None, it));
                };
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| steps.store_kept(&mut front)));
                let (front, stored) = match caught {
                    Ok(stored) => (Some(front), stored),
                    Err(payload) => {
                        let panic = plane::panicked(&*payload);
                        drop(front);
                        // alloc: a panic, once
                        let what = format!(
                            "blaze {panic}, storing what the build left for the store; its front end is dropped, as it may be half written"
                        );
                        (None, Err(ApiError::new(what, None)))
                    }
                };
                self.settle(stored.map_err(Failed::Error), |it| Value::StoredKept(front, it))
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

/// A node of cost 1, its memory to be predicted from its kind.
fn node<'a>(kind: NodeKind, name: &'a str, deps: &'a [NodeId], pool: Pool, key: Option<Key>) -> NodeRef<'a> {
    NodeRef { kind, name, deps, pool, cost: 1, key, memory: 0 }
}

/// A batch of one thread task of kind `task`, with no deps.
fn lone<S: Steps>(plane: &mut Tasks<S>, kind: NodeKind, name: &str, pool: Pool, key: Option<Key>, task: TaskKind) -> Fresh {
    let mut fresh = plane.fresh(1, name.len(), 0);
    plane.push(&mut fresh, node(kind, name, &[], pool, key), one(task));
    fresh
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
        | Work::Pack { .. } => Command::Check,
        Work::Doc { .. } => Command::Doc,
    }
}

fn goal_of(work: &Work) -> Goal {
    match work {
        Work::Build(_) | Work::CodegenInputs(_) => Goal::Build,
        Work::Test { ub: Some(_), .. } => Goal::Check,
        Work::Test { .. } | Work::Ahead { .. } => Goal::Test,
        Work::Run { .. } => Goal::Run,
        Work::Reach { .. } => Goal::Reach,
        Work::Check(_)
        | Work::Verify(_)
        | Work::Trace(_)
        | Work::Tables
        | Work::Snapshot
        | Work::Edit(_)
        | Work::Pack { .. }
        | Work::Doc { .. } => Goal::Check,
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
    let what = match work {
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
        Work::Pack { .. } => "pack",
        Work::Doc { .. } => "doc",
        Work::Edit(_) => "edit",
        Work::Ahead { .. } => "ahead",
    };
    let mut line = what.to_owned();
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
        | Work::Pack { targets, .. }
        | Work::Doc { targets, .. } => Some(targets),
        Work::Tables | Work::Snapshot | Work::Edit(_) => None,
    }
}

/// The directory `BLAZE_PROFILE` names in the environment of the cargo command line that asked
/// for `work`, when it names one.
fn profile_dir(work: &Work) -> Option<&str> {
    let cargo = targets_of(work)?.cargo.as_ref()?;
    let (_, dir) = cargo.env.iter().find(|(name, _)| name == blaze_profile::VAR)?;
    Some(dir.as_str()).filter(|it| !it.is_empty())
}

/// One word of cargo's command line: made once per job, at the edge.
fn word(args: &mut Vec<String>, it: &str) {
    args.push(it.to_owned()); // alloc: the invocation owns its words, once per job
}

/// cargo's command line for an agent's targets, run at `root` with only the daemon's `PATH`
/// and `HOME`.
pub fn invocation(work: &Work, targets: Option<&Targets>, root: &str, env: &[(String, String)]) -> Invocation {
    if let Some(cargo) = targets.and_then(|it| it.cargo.as_ref()) {
        let command = Command::named(&cargo.command).unwrap_or(Command::Check);
        // clone: cargo's own invocation, owned by the config, once per job
        return Invocation {
            command,
            args: cargo.args.clone(), // clone: once per job
            root: cargo.root.clone(), // clone: once per job
            cwd: cargo.cwd.clone(),   // clone: once per job
            env: cargo.env.clone(),   // clone: once per job
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

/// Whether cargo's `args` ask for diagnostics a line each (`--message-format=short` or
/// `json-diagnostic-short`), read as cargo reads each `--message-format`.
fn short(args: &[String]) -> Result<bool, String> {
    let mut values = Vec::new(); // alloc: none unless a `--message-format` is given
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--message-format=") {
            values.push(value.to_owned()); // alloc: once per `--message-format`, once per job
        } else if arg == "--message-format" {
            values.extend(args.next().cloned());
        }
    }
    Ok(matches!(crate::cli::print::format(&values)?, Format::Short | Format::Json { short: true, .. }))
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
}

/// The last plan of a head's view: a job of the same view and config reads its database
/// view and its units, the plan neither asked nor verified again.
struct LastPlan {
    key: ContentId,
    queries: ViewId,
    units: Arc<Units>,
    lock: Option<LockWrite>,
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
    /// ends ([`Image`]).
    images: Vec<Option<Image<S>>>,
    /// How many of `images` are some.
    kept: u32,
    /// The images of compiles that gave way, put back into `images` before any is taken: a
    /// compile posted again relinks the image it was given, never a new one.
    back: Arc<SegQueue<Image<S>>>,
}

impl<S: Steps> Paths<S> {
    fn new() -> Paths<S> {
        Paths {
            ids: HashMap::new_in(Global),
            names: Vec::new(),  // alloc: none until the first path
            files: Vec::new(),  // alloc: none until the first path
            placed: Vec::new(), // alloc: none until the first path
            images: Vec::new(), // alloc: none until the first path
            kept: 0,
            back: Arc::new(SegQueue::new()), // alloc: once for the daemon's life
        }
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
        id
    }

    /// The id of `path`, an output of the unit whose outputs are `ids`: a compare of the few
    /// it has, interned only when it is none of them.
    fn find(&mut self, ids: &[u32], path: &str) -> u32 {
        ids.iter().copied().find(|&id| &*self.names[id as usize] == path).unwrap_or_else(|| self.intern(path))
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

    /// The images kept at `ids`, moved out for a compile.
    fn take_images(&mut self, ids: &[u32]) -> Vec<Image<S>> {
        while let Some(image) = self.back.pop() {
            self.keep_one(&[], image);
        }
        let mut images = Vec::new(); // alloc: none unless an image is kept at an output
        if self.kept == 0 {
            return images;
        }
        for &id in ids {
            if let Some(image) = self.images[id as usize].take() {
                self.kept -= 1;
                images.push(image);
            }
        }
        images
    }

    /// Images a compile gave back, kept at their paths, `ids` the unit's outputs.
    fn keep(&mut self, ids: &[u32], images: Vec<Image<S>>) {
        for image in images {
            self.keep_one(ids, image);
        }
    }

    fn keep_one(&mut self, ids: &[u32], image: Image<S>) {
        let id = self.find(ids, &image.path) as usize;
        if self.images[id].replace(image).is_none() {
            self.kept += 1;
        }
    }
}

/// The checkouts later builds are of again ([`lasting`]), interned once by root directory.
struct Roots<S: Steps> {
    ids: HashMap<Box<str>, u32, Global>,
    /// The last plan of each.
    plans: Vec<Option<LastPlan>>,
    /// The front end of each one's last build, for its next job to lower into
    /// ([`Steps::lower`]).
    fronts: Vec<Option<S::KeptFront>>,
    /// How many of each one's front ends are being stored ([`Jobs::store_kept`]): its next
    /// job's lowering waits in [`Jobs::lowering`] until none is, and one is back in `fronts`.
    storing: Vec<u32>,
    /// The outputs of its last units, as ids.
    outputs: Vec<Option<Arc<Outputs>>>,
}

impl<S: Steps> Roots<S> {
    fn new() -> Roots<S> {
        Roots {
            ids: HashMap::new_in(Global),
            plans: Vec::new(),   // alloc: none until the first checkout
            fronts: Vec::new(),  // alloc: none until the first checkout
            storing: Vec::new(), // alloc: none until the first checkout
            outputs: Vec::new(), // alloc: none until the first checkout
        }
    }

    fn intern(&mut self, root_dir: &str) -> u32 {
        if let Some(&id) = self.ids.get(root_dir) {
            return id;
        }
        let id = u32::try_from(self.plans.len()).expect("under 2^32 checkouts");
        self.ids.insert(root_dir.into(), id);
        self.plans.push(None);
        self.fronts.push(None);
        self.storing.push(0);
        self.outputs.push(None);
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
    Plan(NodeId, Prep),
    Fetch(Fetching),
    /// The view's units being lowered, and the checkout it is of.
    Lower(NodeId, JobInput, Option<u32>),
    Units(Box<Build<S>>),
    Answer(NodeId),
    /// [`Work::Ahead`]'s choice of units, the view lowered and ready.
    Ahead(NodeId),
    /// The tests or the run, once every unit is built: its run in [`Jobs::runs`].
    Runs(Handle<Run<S>>),
    /// Waits for the VFS's commit of the heads and the store's last put before this.
    Snapshot { ticket: Option<Ticket>, heads: Vec<(String, ContentId)>, store: blaze_cas::Tickets },
    Over,
}

/// What the plan needs fetched: fetch `at` is node `first + at`.
struct Fetching {
    first: NodeId,
    fetches: Arc<[Fetch]>,
    /// How many have not settled.
    left: u32,
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
    /// The units a write that panicked leaves unbuilt; none until one does.
    refused: Option<Refused>,
}

/// The units of a build that depend on a write to its front end that panicked
/// ([`Writer::broke`]): each task of one is refused, naming the panic, while keep-going
/// builds the rest.
struct Refused {
    units: BitSet<u32>,
    why: String,
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

    /// The job's error for `broke`, a write that panicked, with every unit that depends on its
    /// unit refused from now on. The unit itself was built: its write is what failed.
    fn refuse_after(&mut self, broke: &Broke) -> ApiError {
        let graph = &self.input.units.graph.units;
        let unit = &graph[broke.unit];
        let error = ApiError::new(
            // alloc: an error, once
            format!("the daemon {}, applying {} of {} of {}", broke.panic, broke.what, unit.target.name, unit.pkg_id),
            Some(unit.pkg_id.clone()), // clone: an error, once
        );
        let refused = self.refused.get_or_insert_with(|| Refused {
            units: BitSet::new(graph.len()), // alloc: once a write panics
            why: format!("a write it depends on panicked: {}", error.what), // alloc: an error, once
        });
        let after = broke.unit as u32;
        let mut grew = true;
        while grew {
            grew = false;
            for (at, it) in (0u32..).zip(graph) {
                let reached = |dep: &blaze_units::schema::UnitDep| dep.index as u32 == after || refused.units.contains(dep.index as u32);
                if at != after && !refused.units.contains(at) && it.dependencies.iter().any(reached) {
                    refused.units.insert(at);
                    grew = true;
                }
            }
        }
        error
    }

    /// Whether `node` is its unit's first: the one whose start is the unit's.
    fn begins(&self, node: NodeId) -> bool {
        let at = node.0.wrapping_sub(self.first.0) as usize;
        at < self.units.len() && (at == 0 || self.units[at - 1] != self.units[at])
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
    /// Opened by the job and closed as it finishes, but `built`.
    queries: Vec<ViewId>,
    /// The one of `queries` its last view was made ready in: handed to the session as it
    /// finishes ([`Said::Finish`]).
    built: Option<ViewId>,
    /// Views of [`Roots::plans`] it reads, which the jobs close once none reads them.
    borrowed: Vec<ViewId>,
    placing: usize,
    /// How the bytes of what it placed got there: cloned, written into the store, copied
    /// ([`blaze_cas::PlacedBytes`]), for its `Work` report and its span.
    placed: blaze_cas::PlacedBytes,
    /// Its `Work` report, said as it finishes: once its placements have all ended, so it
    /// counts every byte they placed.
    worked: Option<Worked>,
    /// Of `placing`, how many are in flight at each path ([`Paths`] id): a file with one is
    /// not yet all there.
    busy: HashMap<u32, u32, Global>,
    /// Build scripts' runs the plane started whose programs are not yet all placed: each
    /// starts once its compile's outputs have nothing in flight ([`Jobs::run_script`]).
    held: Vec<NodeId>,
    /// The look of the shared std store ([`blaze_cas::Store::look_again`]) its units wait
    /// for: what another daemon put before the job began is found, never compiled again.
    look: u64,
    /// The build waiting for that look: the units a build ahead picked, or none for all.
    awaiting: Option<Option<Vec<u32>>>,
    /// It put into the shared std store: flushed once it ends, for the other daemons.
    shared: bool,
    /// Its runs of user code that have not ended.
    runs: u32,
    /// The tickets of records that a value verified across checkouts depends on where its
    /// roots are ([`blaze_usercode::roots::verify::retraction`]): the job is not reported done
    /// until they are durable, or a crash forgets the record and the value is given again.
    durable: blaze_cas::Tickets,
    /// The store of its kept front end under way ([`Jobs::store_kept`]): the job is not
    /// reported done before it ends.
    storing: Option<Storing<S>>,
    /// The tickets of what its kept front end left for the store, each a put's own or that of
    /// the put that kept its value already, and of every put the loop made before it ended
    /// (what it may have read, [`Jobs::wind_up`]); and the checkout its front end is of. The job
    /// is not reported done until they are durable, so a stop, a crash or a reboot after the
    /// answer loses none of it (persisted-state-round-trips).
    stored: blaze_cas::Tickets,
    stored_of: Option<String>,
    /// When, on the injected clock ([`World::now`]), it first waited for `durable` or `stored`
    /// with nothing else left (or, a snapshot, for the store's puts before it): it fails once
    /// [`DURABLE_WITHIN`] has gone by from the later of then and the last step of the store's commit
    /// (bounded-progress, [`gave_up`]), the daemon's loop waking it then
    /// ([`Jobs::next_deadline`]).
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
    links: Vec<(String, Linking)>,
    /// Each image its units linked, for the history ([`LinkRow`]): the target it is of as
    /// cargo names it, its package, and whether it was laid out whole.
    linked: Vec<(String, Option<String>, bool)>,
    /// Why each unit, template and run it computed again was not reused, as each unit's
    /// compile and each run handed them over ([`Compiled::misses`], [`RunPuts::misses`]):
    /// this build's alone, sorted when it ends, for `blaze why`.
    misses: Vec<blaze_kernel::Miss>,
    /// Its command line's environment names a profile's directory ([`profile_dir`]): the
    /// daemon's part of the build is written there as it finishes.
    profiled: bool,
    /// When its plane request opened: on the schema's clock ([`blaze_profile::clock::mono_ns`])
    /// and on the wall's, for its row in the history.
    opened_ns: i64,
    opened_unix_ns: Option<i64>,
    /// The leases of the build dirs it builds into ([`lease_build_dirs`]), taken by its
    /// lowering and held until the job is dropped: the collector renames no dir while a build
    /// holds it (docs/gc.md, "Leases").
    leases: Arc<Mutex<Leases>>,
}

/// A job's kept front end being stored ([`Jobs::store_kept`]), by one task of a plane request
/// of its own: the request (promoted with its job), its node, the root the front end is kept
/// for and its checkout, and the task's value once given.
struct Storing<S: Steps> {
    request: RequestId,
    node: NodeId,
    root: u32,
    checkout: String,
    value: Option<(Option<S::KeptFront>, Result<Vec<super::work::KeptPut>, ApiError>)>,
}

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
        write!(f, "the lowering of job {:?} ({}) is held until root {}'s kept front end is stored", self.job, self.checkout, self.root)?;
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
fn gave_up(store: &Store, now: Duration, since: Duration, heard: Duration, judge: bool) -> Option<String> {
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
fn may_mark(dir: &std::path::Path, checkout: &std::path::Path, root: &std::path::Path, made: bool) -> Result<bool, String> {
    let real = |it: &std::path::Path| {
        blaze_gc::fs::resolve(it).map_err(|why| format!("the build dir {} is not leased: {why}", dir.display()))
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
    let protected = protected(root).map_err(|why| format!("the build dir {} is not leased: {why}", dir.display()))?;
    Ok(protected.refuses(&resolved)?.is_none())
}

/// What no build dir may be, hold or be inside ([`blaze_gc::fs::Protected::of_env`]), made once
/// for blaze's root, with the root it was made for.
static PROTECTED: std::sync::OnceLock<(std::path::PathBuf, Result<blaze_gc::fs::Protected, String>)> =
    std::sync::OnceLock::new();

/// [`PROTECTED`] for blaze's root `root`, made on the first call: the daemon's start makes it, and
/// a list that does not make stops the start, naming why (review round 6, C5), so no lease
/// rebuilds it. A daemon builds for one root: another is an error naming both.
pub(super) fn protected(root: &std::path::Path) -> Result<&'static blaze_gc::fs::Protected, String> {
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

/// The leases of `units`' build dirs (`target_dir` and `build_dir`), each dir's record touched
/// for `checkout`, with each profile's or target's directory the build writes into (docs/gc.md).
/// Only the dirs under `builds` (`<root>/build/<dir>`) are ever collected; one a `--target-dir`
/// or `CARGO_TARGET_DIR` names is leased too, so `blaze clean` refuses it while a build uses it.
/// A dir [`may_mark`] refuses gets no file of blaze's and no lease. `CACHEDIR.TAG` is written
/// only into a dir blaze made now or one under `builds`, never into a dir of the user's.
/// `Ok(Err((dir, wait)))` when a lease cannot be taken now (held exclusively by a collector's
/// rename, or its file without a whole nonce yet): the caller gives way and tries again, for at
/// most [`blaze_gc::lease::GIVE_WAY`]. Runs on the pool: it may sync a record.
#[allow(clippy::type_complexity, reason = "a lease taken, a lease to wait for, or an error")]
fn lease_build_dirs(
    units: &Units,
    checkout: &str,
    builds: &str,
) -> Result<Result<Vec<blaze_gc::lease::Lease>, (String, blaze_gc::lease::Wait)>, String> {
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
        let _span = tracing::info_span!(target: "blaze", "lease of a build dir", dir = %dir.display()).entered();
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
    /// Its plane request opens now.
    fn opening(&mut self) {
        self.opened_ns = blaze_profile::clock::mono_ns();
        self.opened_unix_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|it| i64::try_from(it.as_nanos()).ok());
    }

    /// The history's row of its plane request `request`, which ended `outcome` with the
    /// plane's counts `stats`.
    fn row(&self, request: RequestId, stats: Option<RequestStats>, outcome: profile_history::Outcome) -> BuildRow {
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
            dur_ns: Some(blaze_profile::clock::mono_ns() - self.opened_ns),
            started_unix_ns: self.opened_unix_ns,
            outcome,
            hits: stats.map(|it| it.cached),
            misses: stats.map(|it| it.ran),
            joined: stats.map(|it| it.joined),
            tree: None,
            blaze: None,
        }
    }

    /// The history's rows of its plane request `request` ([`Job::row`]): its own, each image
    /// its units linked, and its units' crate graph.
    fn ended(&mut self, request: RequestId, stats: Option<RequestStats>, outcome: profile_history::Outcome) -> Ended {
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
        // and runs ended in (rule 22).
        let mut misses = std::mem::take(&mut self.misses);
        misses.sort_unstable();
        misses.dedup();
        Ended { row: self.row(request, stats, outcome), links, crate_deps, misses }
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
    let names: HashMap<&str, &str, Global> = units.packages.iter().map(|it| (it.id.as_str(), it.name.as_str())).collect();
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
        .map(|(krate, dep, kind)| CrateDepRow { build_id, krate: krate.to_owned(), dep: dep.to_owned(), kind })
        .collect()
}

/// What the session asked, in the order it asked it.
enum Asked {
    Start { id: JobId, view: Option<Opened>, work: Work, idle: bool },
    Cancel(JobId),
    Promote(JobId),
}

/// An effect of the loop's own, by the slot its tag carries.
enum Own {
    /// A lock file written for the job.
    Write { job: JobId, path: u32 },
    /// An executable's mode set, then its placement recorded.
    Mode { job: JobId, path: u32, key: blaze_cas::Key },
    /// A placed output's file looked at: placed again unless it is as it was placed.
    Look { job: JobId, path: u32, key: blaze_cas::Key, executable: bool },
    /// A file placed, looked at for its [`Placement`] before its job is done: a file changed
    /// after the job's `Done` must not be recorded as the one placed.
    Record { job: JobId, path: u32, key: blaze_cas::Key },
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
}

const _: () = assert!(std::mem::size_of::<JobId>() == 8);
const _: () = assert!(std::mem::size_of::<Own>() <= 52);
const _: () = assert!(std::mem::size_of::<Placing>() == 48);
const _: () = assert!(std::mem::size_of::<Run<super::unit::Real>>() == 32);

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
    /// The store's placements by the store's tag: its tags are its own, made among its other
    /// effects, so there is no slot of ours in them to index by.
    placing: HashMap<Tag, Placing, Global>,
    paths: Paths<S>,
    /// The lock writes, modes and looks at files in flight, by the slot their tag carries.
    own: Tagged<Own>,
    /// Every run of user code going, by the number its tags carry: run `n` has the tags
    /// `setup.runs + n * 2^32` on.
    runs: Tagged<Run<S>>,
    /// Every put the loop has made to the daemon's store (the plane's results, kept front ends,
    /// user code's runs, the jobs' records), as the last ticket of each segment they are in
    /// ([`blaze_cas::Tickets`]: a put of a value kept already gets the ticket of the put that
    /// kept it, from any writer). Two waits read it: a snapshot waits for all of it, and a job
    /// at its Done gate waits for all of it as of its first wait (`Wait::Everything`), since a
    /// value it read comes back with no ticket and may be another job's put not yet durable.
    put: blaze_cas::Tickets,
    roots: Roots<S>,
    /// Views of plans replaced in `roots`, closed once no job reads them.
    retired: Vec<ViewId>,
    /// What a run of user code says, a buffer reused by each of its calls.
    out: Vec<Response>,
    /// The tasks a write held, a buffer reused by each [`Jobs::apply_writes`].
    posts: Vec<(Posting, bool)>,
    /// The jobs that end, a buffer reused by each [`Jobs::wind_up`].
    over: Vec<(JobId, bool, Option<ApiError>)>,
    /// A job ran a query or a unit since the image was written.
    pub cold: bool,
    /// A job finished since the last collection.
    pub ended: bool,
    /// The plane requests that ended, as the profile history's rows, for the daemon to hand
    /// to its history.
    pub rows: Vec<Ended>,
    /// The build dir leases of jobs dropped while their tasks may still write (a stall): each
    /// held until the last task lets go of the job's cancel flag ([`Jobs::release_leases`]).
    held_leases: Vec<(JobId, Arc<AtomicBool>, Arc<Mutex<Leases>>)>,
    /// Lowerings waiting for their root's front end to be stored ([`Jobs::lower`]).
    lowering: Vec<Lowering>,
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

impl<S: Steps> Jobs<S> {
    /// The jobs, their tables of effects and runs reserved, committing nothing.
    pub fn new(setup: Setup) -> Result<Jobs<S>, MemError> {
        let numbers = setup.runs_end.saturating_sub(setup.runs) / RUN_TAGS;
        let bits = numbers.checked_ilog2().unwrap_or(0);
        let run_slots = bits.min(RUN_SLOT_BITS);
        let own = Tagged::new(OWN_SLOT_BITS, OWN_GENERATION_BITS, MemTag::Io)?;
        let runs = Tagged::new(run_slots, bits - run_slots, MemTag::Usercode)?;
        Ok(Jobs {
            setup,
            asked: Vec::new(), // alloc: none until the session asks
            jobs: Table::new(),
            order: Vec::new(), // alloc: none until a job starts
            batches: Batches::new(),
            library: None,
            library_failed: None,
            fetch_failed: HashMap::new_in(Global),
            placing: HashMap::new_in(Global),
            paths: Paths::new(),
            own,
            runs,
            put: blaze_cas::Tickets::default(),
            roots: Roots::new(),
            retired: Vec::new(), // alloc: none until a plan is replaced
            out: Vec::new(),     // alloc: none until user code says something, then reused
            posts: Vec::new(),   // alloc: none until a write holds a task, then reused
            over: Vec::new(),    // alloc: none until a job ends, then reused
            cold: false,
            ended: false,
            rows: Vec::new(), // alloc: none until a build ends, then reused
            held_leases: Vec::new(), // alloc: none until a stall
            lowering: Vec::new(),    // alloc: none until a job lowers while its root's front end is stored
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
        self.held_leases.retain(|(id, cancelled, _)| Arc::strong_count(cancelled) > 1 || busy.contains(id));
    }

    /// Whether no job is under way.
    pub fn idle(&self) -> bool {
        self.jobs.is_empty() && self.asked.is_empty()
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
        if !self.asked.is_empty() {
            let mut asked = std::mem::take(&mut self.asked);
            for it in asked.drain(..) {
                match it {
                    Asked::Start { id, view, work, idle } => self.begin(w, id, view, work, idle),
                    Asked::Cancel(id) => self.cancel_job(w, id),
                    Asked::Promote(id) => self.promote_job(w, id),
                }
            }
            if self.asked.is_empty() {
                self.asked = asked;
            }
        }
        self.wind_up(w);
    }

    fn begin(&mut self, w: &mut World<'_, S>, id: JobId, view: Option<Opened>, work: Work, idle: bool) {
        let views = match (&work, view) {
            (Work::Reach { from, .. }, Some(to)) => vec![*from, to], // alloc: once per job
            (_, Some(view)) => vec![view],                           // alloc: once per job
            (_, None) => Vec::new(),                                 // alloc: none
        };
        let keep_going = targets_of(&work)
            .and_then(|it| it.cargo.as_ref())
            .is_some_and(|cargo| cargo.args.iter().any(|arg| arg == "--keep-going"));
        let snapshot = matches!(work, Work::Snapshot);
        // A daemon already running starts recording at the first request that asks.
        let profiled = match profile_dir(&work) {
            Some(dir) => {
                blaze_profile::start(std::path::Path::new(dir));
                true
            }
            None => false,
        };
        let job = Job {
            work,
            views,
            ready: Vec::new(), // alloc: once per job, a view or two
            request: None,
            idle,
            closed: false,
            finished: snapshot,
            cancelled: Arc::new(AtomicBool::new(false)), // alloc: once per job, shared with its tasks
            stage: Stage::Over,
            value: None,
            updating: HashSet::new_in(Global),
            queries: Vec::new(),  // alloc: once per job
            built: None,
            borrowed: Vec::new(), // alloc: once per job
            placing: 0,
            placed: blaze_cas::PlacedBytes::default(),
            worked: None,
            busy: HashMap::new_in(Global),
            held: Vec::new(), // alloc: none until a run waits for its program
            look: 0,
            awaiting: None,
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
            served: Vec::new(), // alloc: none; a build fills it once
            links: Vec::new(),  // alloc: none until a unit links
            linked: Vec::new(), // alloc: none until a unit links
            misses: Vec::new(), // alloc: none until a unit misses
            profiled,
            opened_ns: 0,
            opened_unix_ns: None,
            leases: Arc::default(), // alloc: once per job, shared with its lowering
        };
        if self.jobs.put(key_of(id), job).is_err() {
            // alloc: an error the session's free list can't give
            let error = ApiError::new(format!("job slot {} is still another job's: it was handed out twice", id.slot), None);
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
        // Its answer waits for its store: an idle one would wait on every other build.
        // Finished at the plane, its word not yet read here: nothing is left to promote.
        if let Some(storing) = &job.storing
            && w.plane.is_open(storing.request)
        {
            w.plane.promote(w.now, storing.request);
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
            let fresh = lone(w.plane, NodeKind::Std, name, Pool::Io, Some(library_key()), kinds::LIBRARY);
            job.stage = Stage::Library(fresh.first());
            self.add(w, id, fresh, Some(work), false);
            return;
        }
        let Some(&opened) = job.views.get(job.ready.len()) else {
            self.end(w, id, false, Some(ApiError::new("the job has no view to build", None)));
            return;
        };
        self.open(w, id, opened, HashSet::new_in(Global));
    }

    /// The view opened in the database with its build configuration as the mounts are now, or
    /// the head's last plan when it was of this view and configuration; then its plan.
    fn open(&mut self, w: &mut World<'_, S>, id: JobId, opened: Opened, asked: HashSet<String, Global>) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        // clone: the config owns the library's place, once per view
        let Some(library) = self.library.clone() else {
            let error = ApiError::new("a view was opened before the standard library was mounted", None);
            self.end(w, id, false, Some(error));
            return;
        };
        let targets = targets_of(&job.work);
        let root_dir = match targets.and_then(|it| it.cargo.as_ref()) {
            Some(cargo) => cargo.root.clone(), // clone: the view's root directory, once per view
            None => w
                .heads
                .path_of(w.vfs, opened.view.root)
                // alloc: an agent's view's directory, once per view
                .unwrap_or_else(|| format!("{}/{:x}", self.setup.views, opened.view.root)),
        };
        let root = lasting(&self.setup.views, &root_dir).then(|| self.roots.intern(&root_dir));
        // clone: the config owns its mounts, once per view
        let mut mounts = vec![Mount { at: root_dir.clone(), origin: Origin::View, under: String::new() }];
        mounts.extend(w.mounts.all().cloned());
        mounts.sort_by(|a, b| a.at.cmp(&b.at));
        let config = BuildConfig {
            invocation: invocation(&job.work, targets, &root_dir, &self.setup.env),
            mounts,
            host: self.setup.host.clone(), // clone: the config owns it, once per view
            library,
            std: self.setup.std.clone(), // clone: the config owns it, once per view
            builds: self.setup.builds.clone(), // clone: the config owns it, once per view
        };
        let encoded = config.encode();
        let key = Hasher::new("blaze v2 serve last plan").id(&opened.view.root).part(&encoded).finish();
        let config = Arc::new(config); // alloc: once per view, shared with its tasks
        if reuses_plans(&job.work)
            && let Some(at) = root
            && let Some(Some(last)) = self.roots.plans.get(at as usize)
            && last.key == key
        {
            job.borrowed.push(last.queries);
            // clone: a refcount and the lock's text, once per view
            let last_units = Some((last.units.clone(), last.lock.clone()));
            let prep = Prep { opened, root_dir, root, config, key, queries: last.queries, asked, last: last_units };
            self.plan(w, id, prep);
            return;
        }
        let queries = w.db.open_with(opened.view, &encoded);
        job.queries.push(queries);
        self.plan(w, id, Prep { opened, root_dir, root, config, key, queries, asked, last: None });
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
    fn add(&mut self, w: &mut World<'_, S>, id: JobId, fresh: Fresh, work: Option<Step<S>>, reads: bool) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let Some(request) = job.request else {
            self.end(w, id, false, Some(ApiError::new("the job has no request of the plane to add its tasks to", None)));
            return;
        };
        let (first, count) = (fresh.first(), fresh.count());
        if count > 0 {
            self.batches.insert(first.0, count, id);
        }
        if let Err(error) = w.plane.add(w.now, request, fresh, work, reads) {
            // alloc: an error, once
            self.end(w, id, false, Some(ApiError::new(format!("the plane refused the job's tasks: {error}"), None)));
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
                if let Some(refused) = &build.refused
                    && build.unit(task.id).is_some_and(|unit| refused.units.contains(unit as u32))
                {
                    // clone: a refused task's why, once
                    w.plane.refuse(w.now, task, refused.why.clone());
                    return;
                }
                let Some(this) = build.writer.reader() else {
                    build.held.push((task, again));
                    return;
                };
                let images = match task.kind == kinds::UNIT_LINK || task.kind == kinds::PROGRAM_LINK {
                    true => self.paths.take_images(build.outputs.of(Phase::of(task.arg).0)),
                    false => Vec::new(), // alloc: none
                };
                Some(Readers { this: Some(this), images, ..Readers::default() })
            }
            Some(job) if matches!(job.stage, Stage::Answer(_) | Stage::Ahead(_)) => match job.work {
                Work::Tables | Work::Pack { .. } | Work::Doc { .. } => job.ready.last_mut().and_then(|it| it.writer.take()).map(|writer| Readers {
                    writer: Some(writer),
                    ..Readers::default()
                }),
                _ => (|| {
                    let this = job.ready.last()?.writer.as_ref()?.reader()?;
                    let from = match job.work {
                        Work::Reach { .. } => Some(job.ready.first()?.writer.as_ref()?.reader()?),
                        _ => None,
                    };
                    Some(Readers { this: Some(this), from, ..Readers::default() })
                })(),
            },
            _ => None,
        };
        match readers {
            Some(readers) => w.plane.post(task, again, readers),
            // alloc: a refused task, once
            None => w.plane.refuse(w.now, task, "its job has no front end to read now".to_owned()),
        }
    }

    /// Each job's waiting writes applied where no reader of its front end is left, and the
    /// tasks held for them posted with readers of what they made. Asking a job whose readers
    /// still run costs one load of its reference count. Called as the loop winds up each
    /// step: a reader is dropped as its task's work returns, before the plane hears the task
    /// ended, so the step that hears the last one applies the write. The daemon calls it
    /// once a turn as well, for a reader that outlives its task.
    pub fn apply_writes(&mut self, w: &mut World<'_, S>) {
        let order = std::mem::take(&mut self.order);
        let mut posts = std::mem::take(&mut self.posts);
        for &id in &order {
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            let Stage::Units(build) = &mut job.stage else { continue };
            if !build.writer.waiting() && build.held.is_empty() {
                continue;
            }
            let applied = build.writer.apply();
            while let Some(broke) = build.writer.broke() {
                let error = build.refuse_after(&broke);
                if !job.cancelled.load(Ordering::Acquire) {
                    job.over.get_or_insert((false, Some(error)));
                }
            }
            match applied {
                Ok(true) => posts.append(&mut build.held),
                Ok(false) => {}
                Err(error) => {
                    for (task, _) in build.held.drain(..) {
                        // alloc: a write that failed, once per task it held
                        let why = format!("applying a build script's run or a proc macro: {}", error.what);
                        w.plane.refuse(w.now, task, why);
                    }
                    if !job.cancelled.load(Ordering::Acquire) {
                        job.over.get_or_insert((false, Some(error)));
                    }
                    self.end(w, id, false, None);
                }
            }
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
                Happened::Settled { id, ok, why } => self.settled(w, id, ok, why),
                Happened::Finished { request, ok, stats } => self.plane_finished(w, request, ok, stats),
            }
        }
        self.wind_up(w);
    }

    /// A task of a build's units that panicked fails its job with the panic, naming the unit.
    /// A task of any other stage fails its job as it settles, the panic its `why`.
    fn panicked(&mut self, node: NodeId, panic: &plane::Panic) {
        let Some(job) = self.batches.job_of(node).and_then(|id| self.jobs.get_mut(key_of(id))) else { return };
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
    /// was): every job not ended yet waits for it to be durable before it is reported done. A
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
    }

    /// The images unit `unit`'s link relinked, kept at its outputs for its next link.
    fn keep_images(&mut self, node: NodeId, unit: usize, images: Vec<Image<S>>) {
        if images.is_empty() {
            return;
        }
        let build = self.batches.job_of(node).and_then(|id| match &self.jobs.get(key_of(id))?.stage {
            Stage::Units(build) => Some(build),
            _ => None,
        });
        self.paths.keep(build.map_or(&[][..], |it| it.outputs.of(unit)), images);
    }

    fn did(&mut self, w: &mut World<'_, S>, node: NodeId, value: Value<S>) {
        match value {
            Value::Library(Ok(Library { at, fingerprint, source })) => {
                let mounted = match source {
                    Some(source) => w.mounts.mount_library(w.vfs, &at, &fingerprint, source).map(drop),
                    None if w.mounts.has(&at) => Ok(()),
                    // alloc: an error, once
                    None => Err(format!("its files were not read as it was mounted at {at}, and nothing is")),
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
                    self.fetch_failed.insert(fetch.at.clone(), ApiError::new(why, Some(fetch.what.clone())));
                }
            }
            Value::Fetched(fetches, at, Err(error)) => {
                // clone: a failed fetch's place, once per source
                self.fetch_failed.insert(fetches[at as usize].at.clone(), error);
            }
            Value::Compiled(unit, result, images) => {
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
            Value::StoredKept(front, stored) => {
                let job = self.batches.job_of(node).and_then(|id| self.jobs.get_mut(key_of(id)));
                if let Some(storing) = job.and_then(|it| it.storing.as_mut()).filter(|it| it.node == node) {
                    storing.value = Some((front, stored));
                }
            }
            value => {
                if let Some(job) = self.batches.job_of(node).and_then(|id| self.jobs.get_mut(key_of(id))) {
                    job.value = Some((node, value));
                }
            }
        }
    }

    fn settled(&mut self, w: &mut World<'_, S>, node: NodeId, ok: bool, why: Option<String>) {
        let Some(id) = self.batches.job_of(node) else { return };
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        // Its store settles after the job is over: what it left for the store is put still.
        if job.storing.as_ref().is_some_and(|it| it.node == node) {
            self.stored_kept(w, id, why);
            return;
        }
        if job.over.is_some() {
            job.take_value(node);
            return;
        }
        let why = why.unwrap_or_default();
        match std::mem::replace(&mut job.stage, Stage::Over) {
            Stage::Library(at) if at == node => {
                if self.library.is_some() {
                    self.prepare(w, id);
                } else {
                    // alloc: an error, once
                    let error = self.library_failed.clone().unwrap_or_else(|| {
                        ApiError::new(format!("the standard library's source: {why}"), None) // alloc: an error, once
                    });
                    self.end(w, id, false, Some(error));
                }
            }
            Stage::Plan(at, prep) if at == node => match job.take_value(node) {
                Some(Value::Planned(Ok(planned))) => self.planned(w, id, prep, planned),
                Some(Value::Planned(Err(error))) => self.end(w, id, false, Some(error)),
                _ => {
                    let error = ApiError::new(format!("the plan of {}: {why}", prep.root_dir), None); // alloc: an error, once
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
                    let at = &fetching.fetches[place].at;
                    // clone: a failed fetch's error, for each job that asked it
                    let error = self.fetch_failed.get(at).cloned().unwrap_or_else(|| {
                        ApiError::new(format!("fetching {at}: {why}"), Some(at.clone())) // alloc: an error, once
                    });
                    job.over.get_or_insert((false, Some(error)));
                }
                fetching.left -= 1;
                if fetching.left > 0 {
                    job.stage = Stage::Fetch(fetching);
                    return;
                }
                let prep = fetching.prep;
                if let Some((_, error)) = job.over.take() {
                    self.end(w, id, false, error);
                    return;
                }
                w.db.close(prep.queries);
                job.queries.retain(|it| *it != prep.queries);
                self.open(w, id, prep.opened, prep.asked);
            }
            Stage::Lower(at, input, root) if at == node => match job.take_value(node) {
                Some(Value::Lowered(Ok(FrontEnd { front, writer, puts }))) => {
                    for put in puts {
                        let (key, kind) = (put.key, put.kind);
                        match w.store.put(put.key, put.kind, put.bytes) {
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
                _ => self.end(w, id, false, Some(ApiError::new(format!("lowering the units: {why}"), None))), // alloc: an error, once
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
                    let error = ApiError::new(format!("choosing the units to build ahead: {why}"), None); // alloc: an error, once
                    self.end(w, id, false, Some(error));
                }
            },
            Stage::Units(build) if build.evaluation.as_ref().is_some_and(|it| it.select == node) => {
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
            Planned::Failed(error) => self.end(w, id, false, Some(ApiError::new(error.what, error.at))),
            Planned::Needs(fetches) => {
                let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
                let mut wanted = Vec::with_capacity(fetches.len());
                for fetch in fetches {
                    // clone: each place asked, once per source
                    if !prep.asked.insert(fetch.at.clone()) {
                        let error = ApiError::new(
                            // alloc: an error, once
                            format!("the load asked for {} again after it was fetched and mounted at {}", fetch.what, fetch.at),
                            Some(fetch.at.clone()), // clone: an error, once
                        );
                        self.end(w, id, false, Some(error));
                        return;
                    }
                    if w.mounts.has(&fetch.at) {
                        continue;
                    }
                    let verb = match &fetch.kind {
                        FetchKind::Crate { .. } => Some((FetchVerb::Downloaded, fetch.what.clone())), // clone: a report, once per source
                        FetchKind::IndexFile | FetchKind::Git { .. } => {
                            // clone: a report, once per index or repository
                            job.updating.insert(fetch.what.clone()).then(|| (FetchVerb::Updating, fetch.what.clone()))
                        }
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
                    w.plane.push(&mut fresh, node(NodeKind::Fetch, &name, &[], Pool::Io, Some(fetch_key(fetch))), task);
                }
                let first = fresh.first();
                // clone: two refcounts, once per batch of fetches
                let work = Step::of(&job.cancelled, Does::Fetch { steps: w.steps.clone(), fetches: fetches.clone() });
                job.stage = Stage::Fetch(Fetching { first, fetches, left: count, prep });
                self.add(w, id, fresh, Some(work), false);
            }
            Planned::Ready(mut units) => {
                let lock = units.lock.take();
                let units = Arc::new(units); // alloc: once per plan, shared by the job's tasks
                let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
                if let Some(root) = prep.root
                    && reuses_plans(&job.work)
                {
                    job.queries.retain(|it| *it != prep.queries);
                    job.borrowed.push(prep.queries);
                    // clone: a refcount and the lock's text, once per plan
                    let last = LastPlan { key: prep.key, queries: prep.queries, units: units.clone(), lock: lock.clone() };
                    if let Some(old) = self.roots.plans[root as usize].replace(last)
                        && old.queries != prep.queries
                    {
                        self.retired.push(old.queries);
                    }
                }
                self.ready(w, id, prep, units, lock);
            }
        }
    }

    /// The view's plan taken: its lock file written, its warnings reported, and its units
    /// lowered, or for the work that walks the plan, the view ready as it is.
    fn ready(&mut self, w: &mut World<'_, S>, id: JobId, prep: Prep, units: Arc<Units>, lock: Option<LockWrite>) {
        if self.jobs.get(key_of(id)).is_none() {
            return;
        }
        if let Some(lock) = lock {
            let path = self.paths.intern(&lock.path);
            // clone: the driver's effect owns the file's name, once per lock write
            let Some(file) = self.paths.files[path as usize].clone() else {
                // alloc: an error, once
                let error = ApiError::new(format!("the lock file's path {} is no file", lock.path), Some(lock.path));
                self.end(w, id, false, Some(error));
                return;
            };
            let tag = match self.own_tag(Own::Write { job: id, path }) {
                Ok(tag) => tag,
                Err(error) => {
                    self.end(w, id, false, Some(error));
                    return;
                }
            };
            self.begin_placing(id, path);
            w.effects.push(Effect::Write { file, bytes: lock.text.into_bytes(), tag });
        }
        for warning in &units.warnings {
            w.said.push(Said::Report(id, Response::Diagnostic(warned(warning))));
        }
        let short = match short(&prep.config.invocation.args) {
            Ok(short) => short,
            Err(why) => {
                self.end(w, id, false, Some(ApiError::new(why, Some("--message-format".to_owned())))); // alloc: an error, once
                return;
            }
        };
        // Its build dirs are leased by its lowering, on the pool ([`Does::Lower`]).
        let checkout = prep.config.invocation.root.clone(); // clone: the lowering's own copy, once
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        job.built = job.queries.contains(&prep.queries).then_some(prep.queries);
        let cargo = targets_of(&job.work).and_then(|it| it.cargo.as_ref());
        let input = JobInput {
            db: w.db.clone(), // clone: a refcount, once per view
            queries: prep.queries,
            root: prep.opened.view.root,
            files: w.vfs.reader(),
            config: prep.config,
            units,
            store: w.store.reader(),
            std: w.std.reader(),
            macros: Arc::clone(w.macros), // clone: a refcount, once per view
            color: cargo.map_or(self.setup.color, |it| it.color),
            short,
            width: cargo.and_then(|it| it.width),
            verbose: cargo.map_or(0, |it| it.verbose),
            refused: cargo.map_or(Some(crate::REFUSED_SHOWN), |it| it.refused),
            goal: goal_of(&job.work),
            cancelled: job.cancelled.clone(), // clone: a refcount, once per view
        };
        // Asked as the view is lowered, so it is in by the time the units are built.
        job.look = w.std.look_again(w.effects);
        if matches!(job.work, Work::Trace(_) | Work::Verify(_)) {
            job.ready.push(Ready { input, root: prep.root, front: None, writer: None });
            self.next(w, id);
            return;
        }
        let name = named("lowering ", &prep.root_dir);
        let job_idle = job.idle;
        match prep.root {
            // The root's front end is being stored: the lowering waits for it rather than
            // lowering anew beside it.
            Some(root) if self.roots.storing[root as usize] > 0 => {
                let mut on = None;
                for job in self.jobs.values() {
                    let Some(storing) = job.storing.as_ref().filter(|it| it.root == root) else { continue };
                    on = Some(storing.node);
                    // A build asked now waits on the store: none of it runs as idle work.
                    if !job_idle && w.plane.is_open(storing.request) {
                        w.plane.promote(w.now, storing.request);
                    }
                }
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
    fn lower(&mut self, w: &mut World<'_, S>, id: JobId, input: JobInput, root: Option<u32>, name: &str, checkout: String) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let kept = Mutex::new(root.and_then(|root| self.roots.fronts[root as usize].take()));
        let (steps, lowering) = (w.steps.clone(), input.clone()); // clone: refcounts, once per view
        let (leases, builds) = (Arc::clone(&job.leases), self.setup.builds.clone()); // clone: a refcount and a path, once per view
        let work = Step::of(&job.cancelled, Does::Lower { steps, input: lowering, kept, leases, checkout, builds });
        let fresh = lone(w.plane, NodeKind::Names, name, Pool::Work, None, kinds::LOWER);
        job.stage = Stage::Lower(fresh.first(), input, root);
        self.add(w, id, fresh, Some(work), false);
    }

    /// What job `id`'s build left for the store, written from its front end as it is kept
    /// ([`Steps::store_kept`]) by one task of a plane request of its own, as the job's other
    /// tasks are done with it. The front end is the root's again once the puts are made
    /// ([`Jobs::stored_kept`]), and the job is reported done only once they are durable
    /// ([`Jobs::wind_up`]).
    fn store_kept(&mut self, w: &mut World<'_, S>, id: JobId, root: u32, checkout: String, kept: S::KeptFront) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let request = w.plane.open(w.now, false, job.idle);
        let name = named("storing the kept front end of ", &checkout);
        let fresh = lone(w.plane, NodeKind::Store, &name, Pool::Work, None, kinds::STORE_KEPT);
        let (node, count) = (fresh.first(), fresh.count());
        // clone: the checkout's path, once per build, for the task's span
        let does = Does::StoreKept { steps: Arc::clone(w.steps), kept: Mutex::new(Some(kept)), checkout: checkout.clone() };
        let work = Step::of(&job.cancelled, does);
        // A store's request is marked before anything can finish it, and its node is the
        // job's only once the plane took it: a refused add leaves no entry behind.
        self.store_requests.insert(request);
        match w.plane.add(w.now, request, fresh, Some(work), false) {
            Ok(()) => {
                self.batches.insert(node.0, count, id);
                job.storing = Some(Storing { request, node, root, checkout, value: None });
                self.roots.storing[root as usize] += 1;
            }
            Err(error) => {
                // alloc: an error, once
                let what = format!("the plane refused the task storing what the build of {checkout} left for the store: {error}");
                fail_over(&mut job.over, ApiError::new(what, Some(checkout)));
            }
        }
        w.plane.close(w.now, request);
    }

    /// Job `id`'s kept front end stored, its task's node settled: its puts made and the store
    /// flushed, the front end the root's again, and the lowerings waiting for it posted. A
    /// store that failed, or panicked, fails the job naming why, its front end dropped: the
    /// root's next job lowers anew.
    fn stored_kept(&mut self, w: &mut World<'_, S>, id: JobId, why: Option<String>) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let Some(Storing { root, checkout, value, .. }) = job.storing.take() else { return };
        self.roots.storing[root as usize] -= 1;
        let failed = match value {
            Some((front, Ok(puts))) => {
                let mut failed = None;
                let put = !puts.is_empty();
                for super::work::KeptPut { put, what } in puts {
                    let (key, kind) = (put.key, put.kind);
                    match w.store.put(put.key, put.kind, put.bytes) {
                        // Each ticket is waited on: a value kept already gets the ticket of the
                        // put that kept it, from any writer's segment, so no one ticket stands
                        // for the rest ([`blaze_cas::Tickets`]).
                        Ok(ticket) => {
                            self.put.add(ticket);
                            job.stored.add(ticket);
                        }
                        Err(mismatch) => {
                            // alloc: an error, once
                            let what = format!(
                                "the store holds other bytes under {key:?} than the build of {checkout} made now \
                                 for {what}, a {kind:?} value (kept {:?}, made {:?}): its key misses an input or \
                                 what made it is not deterministic (keys-cover-every-input)",
                                mismatch.kept, mismatch.given
                            );
                            failed = Some(ApiError::new(what, Some(checkout.clone()))); // clone: an error, once
                            break;
                        }
                    }
                }
                job.stored_of = Some(checkout.clone()); // clone: the checkout's path, once per build
                if put {
                    w.store.flush(w.effects);
                }
                if failed.is_none() {
                    self.roots.fronts[root as usize] = front;
                }
                failed
            }
            Some((_, Err(error))) => {
                // alloc: an error, once
                let what = format!("storing what the build of {checkout} left for the store: {}", error.what);
                Some(ApiError::new(what, error.at.or_else(|| Some(checkout.clone())))) // clone: an error, once
            }
            None => {
                let why = why.unwrap_or_else(|| "its task ended with no value".to_owned()); // alloc: an error, once
                // alloc: an error, once
                let what = format!("storing what the build of {checkout} left for the store: {why}");
                Some(ApiError::new(what, Some(checkout.clone()))) // clone: an error, once
            }
        };
        if let Some(error) = failed {
            fail_over(&mut job.over, error);
        }
        // Storing a front end can expand macros of its own: a dependency crate's image encodes
        // the signature of every item (`hir_def::deps::serve`'s `signatures::encode`), and a
        // type-position macro call the build never needed is expanded there first, which a
        // kept proc-macro expansion puts. `plane_finished` published before the store ran, so
        // what those expansions put is published here as it is there: its tickets waited on by
        // this job (and the snapshot, [`Jobs::put`]), a durable put by every job not ended,
        // and a verdict the store refuses failing this job.
        w.macros.flush();
        let published = w.macros.keeps().publish(w.store, proc_macro_api::kept::take_puts());
        for ticket in &published.tickets {
            self.put.add(*ticket);
        }
        if !published.durable.is_empty() {
            w.store.flush(w.effects);
            self.wait_durable(&published.durable);
        }
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            for ticket in &published.tickets {
                job.stored.add(*ticket);
            }
            if let Some(refused) = published.refused.into_iter().next() {
                // alloc: an error, once
                let what = format!("storing what the build of {checkout} left for the store: {refused}");
                fail_over(&mut job.over, ApiError::new(what, Some(checkout.clone()))); // clone: an error, once
            }
        }
        // Another store of the root's front end still under way: its lowerings wait for it.
        if self.roots.storing[root as usize] > 0 {
            return;
        }
        let mut waiting = std::mem::take(&mut self.lowering);
        let (now, later): (Vec<Lowering>, Vec<Lowering>) = waiting.drain(..).partition(|it| it.root == root); // alloc: the lowerings waiting, a few
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
        let waited = |id: &&JobId| jobs.get(key_of(**id)).is_some_and(|it| it.awaiting.is_some() && it.look <= looked);
        // alloc: the jobs a look lets build, a few, once per look
        let ready: Vec<JobId> = self.order.iter().filter(waited).copied().collect();
        for id in ready {
            let Some(picked) = self.jobs.get_mut(key_of(id)).and_then(|it| it.awaiting.take()) else { continue };
            self.build(w, id, picked.as_deref());
        }
        self.wind_up(w);
    }

    /// The next view to make ready, or the work once all are.
    fn next(&mut self, w: &mut World<'_, S>, id: JobId) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if job.ready.len() < job.views.len() {
            self.prepare(w, id);
            return;
        }
        if matches!(
            job.work,
            Work::Check(_) | Work::Build(_) | Work::Test { .. } | Work::Run { .. } | Work::Pack { .. } | Work::Doc { .. }
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
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        if w.std.looked() < job.look {
            job.awaiting = Some(picked.map(<[u32]>::to_vec)); // alloc: a build ahead's picks, once
            return;
        }
        let ready = job.ready.pop();
        let Some(Ready { input, root, front: Some(front), writer: Some(writer) }) = ready else {
            self.end(w, id, false, Some(ApiError::new("a build's view is not lowered", None)));
            return;
        };
        let cancelled = job.cancelled.clone(); // clone: a refcount, once per build
        let all = &input.units.graph.units;
        let picked = match picked {
            None => None,
            Some(picked) => {
                if let Some(bad) = picked.iter().find(|it| **it as usize >= all.len()) {
                    // alloc: an error, once
                    let what = format!("the build ahead picked unit {bad} of a plan of {} units", all.len());
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
                Some((0u32..).zip(&wanted).filter_map(|(at, it)| it.then_some(at)).collect::<Vec<u32>>())
            }
        };
        let built = u32::try_from(picked.as_ref().map_or(all.len(), Vec::len)).expect("under 2^32 units");
        let unit_at = |offset: u32| picked.as_ref().map_or(offset, |it| it[offset as usize]) as usize;
        let goal = input.goal;
        // A unit's phases, a node each; none for a build script's run, which is one node.
        let phases_of = |unit: &blaze_units::schema::Unit| match unit.mode {
            UnitMode::RunCustomBuild => &[][..],
            _ => super::unit::phases(goal, unit),
        };
        // Each built unit's last node, by its place after the batch's first; `u32::MAX` for one
        // not built. A unit's first node waits for the last of each unit it depends on.
        let mut last = vec![u32::MAX; all.len()]; // alloc: once per build
        let mut units_of = Vec::new(); // alloc: once per build, a unit a node
        let (mut names, mut all_deps) = (0, 0);
        for offset in 0..built {
            let at = unit_at(offset);
            let unit = &all[at];
            let count = phases_of(unit).len().max(1);
            units_of.extend(std::iter::repeat_n(at as u32, count));
            last[at] = u32::try_from(units_of.len() - 1).expect("under 2^32 nodes");
            names += count * (unit.pkg_id.len() + 1 + unit.target.name.len());
            // A phase waits for at most two of its unit's before it.
            all_deps += unit.dependencies.len() + 2 * count;
        }
        let count = u32::try_from(units_of.len()).expect("under 2^32 nodes");
        let mut nodes = w.plane.fresh(count, names, all_deps);
        let first = nodes.first();
        // alloc: a name and a deps buffer, once per build, reused by every unit
        let (mut name, mut deps) = (String::new(), Vec::new());
        let mut next = first.0;
        for offset in 0..built {
            let at = unit_at(offset);
            let unit = &all[at];
            name.clear();
            name.push_str(&unit.pkg_id);
            name.push(' ');
            name.push_str(&unit.target.name);
            deps.clear();
            deps.extend(unit.dependencies.iter().map(|dep| NodeId(first.0 + last[dep.index as usize])));
            let phases = phases_of(unit);
            if phases.is_empty() {
                let task = Task::Loop { kind: loops::SCRIPT_RUN };
                w.plane.push(&mut nodes, node(NodeKind::ScriptRun, &name, &deps, Pool::Work, None), task);
                next += 1;
                continue;
            }
            let base = next;
            let of = |phase: Phase| {
                let index = phases.iter().position(|it| *it == phase).expect("a phase a unit's phases have");
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
                match program.zip(super::unit::program_key(unit, phase)) {
                    Some((kind, key)) => _ = w.plane.push_program(&mut nodes, spec, kind, arg, key),
                    None => _ = w.plane.push(&mut nodes, spec, Task::Thread { kind: task, arg }),
                }
                next += 1;
            }
        }
        // clone: refcounts, once per build
        let work = Step::of(&cancelled, Does::Compile { front: front.clone(), back: self.paths.back.clone() });
        let units = &input.units;
        let package_of = {
            let packages: HashMap<&str, u32, Global> =
                // alloc: once per build
                (0u32..).zip(&units.packages).map(|(at, package)| (package.id.as_str(), at)).collect();
            // alloc: once per build
            units.graph.units.iter().map(|unit| packages.get(unit.pkg_id.as_str()).copied().unwrap_or(u32::MAX)).collect()
        };
        let reported = BitSet::<u32>::new(units.packages.len()); // alloc: once per build
        let outputs = match root.and_then(|root| self.roots.outputs[root as usize].as_ref()) {
            Some(outputs) if Arc::ptr_eq(&outputs.units, units) => outputs.clone(), // clone: a refcount, once per build
            _ => {
                let made = Arc::new(self.paths.outputs(units)); // alloc: once per new units
                if let Some(root) = root {
                    self.roots.outputs[root as usize] = Some(made.clone()); // clone: a refcount
                }
                made
            }
        };
        let select = match &job.work {
            Work::Test { filter, args, all, ub: Some(ub), .. } => {
                // clone: the selection, once per build
                let (front, filter, args, all, ub) = (front.clone(), filter.clone(), args.clone(), *all, *ub);
                let work = Step::of(&cancelled, Does::Select { front, filter, args, all, ub });
                deps.clear();
                deps.extend((0..count).map(|at| NodeId(first.0 + at)));
                let name = "the tests to evaluate";
                let mut fresh = w.plane.fresh(1, name.len(), deps.len());
                let select = w.plane.push(&mut fresh, node(NodeKind::Bodies, name, &deps, Pool::Work, None), one(kinds::UB_TESTS));
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
        }));
        if count > 0 {
            self.add(w, id, nodes, Some(work), true);
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
        let (front, tests, ub): (_, Arc<[String]>, _) = (build.front.clone(), tests.into(), evaluation.ub);
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
        let fresh = build.front.fresh(at);
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
        let Some(package) = build.input.units.packages.get(build.package_of[at] as usize) else { return };
        if !build.reported.insert(build.package_of[at]) {
            return;
        }
        let verb = match (fresh, build.input.goal, unit.mode) {
            (true, ..) => Verb::Fresh,
            (false, Goal::Check, _) | (false, _, UnitMode::Check) => Verb::Checking,
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
    fn run_tags(&mut self, id: JobId, node: Option<NodeId>) -> Result<(Handle<Run<S>>, u64), ApiError> {
        let (run, packed) = match self.runs.insert(Run { job: id, node, runs: None, killed: false }) {
            Ok(it) => it,
            // alloc: an error, once
            Err(error) => return Err(ApiError::new(format!("no room for another run of user code: {error}"), None)),
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
    fn run_script(&mut self, w: &mut World<'_, S>, node: NodeId) {
        let Some(id) = self.batches.job_of(node) else { return };
        // The plane starts a run once its compile's node ends, before the loop has placed what
        // it made: the run waits for its program to be all there ([`Jobs::release_held`]).
        if let Some(job) = self.jobs.get_mut(key_of(id))
            && waits_for_program(job, node)
        {
            job.held.push(node);
            return;
        }
        self.started_unit(w, node);
        let Some(Stage::Units(build)) = self.jobs.get(key_of(id)).map(|it| &it.stage) else { return };
        let Some(unit) = build.unit(node) else { return };
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
        let Some(Job { stage: Stage::Units(build), over, .. }) = self.jobs.get_mut(key_of(id)) else {
            self.run_over(run);
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
    fn compiled(&mut self, w: &mut World<'_, S>, node: NodeId, unit: usize, result: Result<Compiled, ApiError>) {
        let Some(id) = self.batches.job_of(node) else { return };
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        let streams = streams_units(&job.work);
        let mut compiled = match result {
            Ok(compiled) => compiled,
            Err(error) => {
                if !job.cancelled.load(Ordering::Acquire) {
                    job.over.get_or_insert((false, Some(error)));
                }
                return;
            }
        };
        if let (Some(macros), Stage::Units(build)) = (compiled.macros.take(), &mut job.stage)
            && let Err(error) = build.writer.macros(unit, macros)
        {
            job.over.get_or_insert((false, Some(error)));
        }
        if let Stage::Units(build) = &job.stage
            && let Some(u) = build.input.units.graph.units.get(unit)
        {
            let kind = u.target.kind.first().map_or("", String::as_str);
            let package = package_named(&build.input.units, &u.pkg_id);
            for (_, how) in &compiled.links {
                // alloc: one a link, for the history
                job.linked.push((format!("{} ({kind})", u.target.name), package.map(str::to_owned), how.placed()));
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
            w.said.push(Said::Report(id, Response::Diagnostic(diagnostic)));
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
            let what = format!("{store} holds other bytes under {:?} than {what} made now, a {kind:?} value: {mismatch:?}", mismatch.key);
            // clone: an error, once
            ApiError::new(what, named.map(|it| it.pkg_id.clone()))
        };
        for put in compiled.puts {
            let kind = put.kind;
            match w.store.put(put.key, put.kind, put.bytes) {
                Ok(ticket) => self.put.add(ticket),
                Err(mismatch) => {
                    let error = mismatched("the store", kind, mismatch);
                    job.over.get_or_insert((false, Some(error)));
                }
            }
        }
        job.shared |= !compiled.share.is_empty();
        // Not waited on before the answer, and flushed only as the job ends: a share is a copy,
        // for the other daemons, of what `compiled.puts` put into this daemon's own store just
        // above, whose tickets the job does wait on. Losing it in a crash loses no answer: a
        // reader of the shared store takes a std record only with every output it names there
        // (`unit::compile::kept`, and `found`'s `header::check`), and every value is checked
        // against its hash, so a share torn or lost is a miss, compiled again by whoever asks.
        for put in compiled.share {
            let kind = put.kind;
            if let Err(mismatch) = w.std.put(put.key, put.kind, put.bytes) {
                let error = mismatched("the standard library's shared store", kind, mismatch);
                job.over.get_or_insert((false, Some(error)));
            }
        }
        let outputs = match (compiled.artifact, &mut job.stage) {
            (artifact, Stage::Units(build)) => {
                if streams
                    && let Some(it) = &artifact
                    && runs_as_test(it)
                    && let (Ok(unit), Ok(at)) = (u32::try_from(unit), u32::try_from(build.artifacts.len()))
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
                let error = ApiError::new(format!("{} is no path an output can be placed at", output.path), Some(output.path));
                if let Some(job) = self.jobs.get_mut(key_of(id)) {
                    job.over.get_or_insert((false, Some(error)));
                }
                continue;
            }
            let placed = self.paths.placed[path as usize].as_ref().is_some_and(|it| {
                it.key == output.key && it.executable == output.executable && it.meta.is_some()
            });
            if !placed {
                self.place(w, id, path, output.key, output.executable);
                continue;
            }
            let look = Own::Look { job: id, path, key: output.key, executable: output.executable };
            let tag = self.own_tag(look);
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            match tag {
                Ok(tag) => {
                    // clone: the driver's effect owns the file's name, once per output
                    let Some(file) = self.paths.files[path as usize].clone() else { continue };
                    self.begin_placing(id, path);
                    w.effects.push(Effect::Stat { file, tag });
                }
                Err(error) => _ = job.over.get_or_insert((false, Some(error))),
            }
        }
    }

    /// The store's value of `key` placed at path `path` for job `id`.
    fn place(&mut self, w: &mut World<'_, S>, id: JobId, path: u32, key: blaze_cas::Key, executable: bool) {
        // The whole file the store kept to clone the path's last value from goes once nothing
        // clones it (unless the new value has the same bytes): a build dir rebuilt again and
        // again keeps no store file of each version.
        if let Some(before) = self.paths.placed[path as usize].take()
            && before.key != key
        {
            w.store.unplace(&before.key, &key, w.effects);
        }
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        // clone: the store's effect owns the file's name, once per placement
        let Some(file) = self.paths.files[path as usize].clone() else {
            let name = self.paths.names[path as usize].to_string(); // alloc: an error, once
            // alloc: an error, once
            job.over.get_or_insert((false, Some(ApiError::new(format!("{name} is no path an output can be placed at"), Some(name)))));
            return;
        };
        match w.store.place(&key, file, w.effects) {
            Some(tag) => {
                self.placing.insert(tag, Placing { job: id, path, key, executable });
                self.begin_placing(id, path);
            }
            None => {
                let name = self.paths.names[path as usize].to_string(); // alloc: an error, once
                // alloc: an error, once
                let error = ApiError::new(format!("the store has nothing under {key:?} to place at {name}"), Some(name));
                job.over.get_or_insert((false, Some(error)));
            }
        }
    }

    /// An effect of job `id` at path `path` asked: the file is not all there until it ends.
    fn begin_placing(&mut self, id: JobId, path: u32) {
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.placing += 1;
            *job.busy.entry(path).or_default() += 1;
        }
    }

    /// An effect of [`Jobs::begin_placing`] ended; the job, if it is still there.
    fn end_placing(&mut self, id: JobId, path: u32) -> Option<&mut Job<S>> {
        let job = self.jobs.get_mut(key_of(id))?;
        job.placing -= 1;
        if let blaze_hash::hash_map::Entry::Occupied(mut busy) = job.busy.entry(path) {
            *busy.get_mut() -= 1;
            if *busy.get() == 0 {
                busy.remove();
            }
        }
        Some(job)
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
            Err(_) => Err(ApiError::new(format!("the loop has 2^{OWN_SLOT_BITS} effects of its own in flight"), None)),
        }
    }

    /// Path `path` placed with the value of `key` for job `id`: what is there now looked at
    /// for its record.
    fn record(&mut self, w: &mut World<'_, S>, id: JobId, path: u32, key: blaze_cas::Key, executable: bool) {
        // clone: the driver's effect owns the file's name, once per placement
        let Some(file) = self.paths.files[path as usize].clone() else { return };
        match self.own_tag(Own::Record { job: id, path, key }) {
            Ok(tag) => {
                self.begin_placing(id, path);
                self.paths.placed[path as usize] = Some(Placement { key, executable, meta: None });
                w.effects.push(Effect::Stat { file, tag });
            }
            Err(error) => {
                if let Some(job) = self.jobs.get_mut(key_of(id)) {
                    job.over.get_or_insert((false, Some(error)));
                }
            }
        }
    }

    /// What the store placed.
    pub fn placed(&mut self, w: &mut World<'_, S>, placed: Vec<Placed>) {
        for Placed { tag, to, result, bytes } in placed {
            let Some(Placing { job: id, path, key, executable }) = self.placing.remove(&tag) else { continue };
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            job.placed += bytes;
            match result {
                Ok(()) if executable => match self.own_tag(Own::Mode { job: id, path, key }) {
                    Ok(tag) => w.effects.push(Effect::Mode { file: to, mode: 0o755, tag }),
                    Err(error) => {
                        if let Some(job) = self.end_placing(id, path) {
                            job.over.get_or_insert((false, Some(error)));
                        }
                    }
                },
                Ok(()) => {
                    self.end_placing(id, path);
                    self.record(w, id, path, key, false);
                }
                Err(why) => {
                    if let Some(job) = self.end_placing(id, path) {
                        // alloc: an error, once
                        let error = ApiError::new(format!("placing /{to}: {why}"), Some(format!("/{to}")));
                        job.over.get_or_insert((false, Some(error)));
                    }
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
        let Some(Run { job: id, node, runs: Some(runs), .. }) = self.runs.get_mut(run) else { return };
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
    /// too): its job answers once all of it is durable, and every other job that may have read
    /// it waits for it too ([`Jobs::put`], `wind_up`). The store is flushed when the job comes
    /// to its gate, once, not after every step.
    fn run_put(&mut self, w: &mut World<'_, S>, id: JobId, mut puts: RunPuts) {
        self.put.extend(&puts.tickets);
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.durable.extend(&puts.tickets);
            job.misses.append(&mut puts.misses);
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
            Own::Record { job: id, path, key } => {
                self.end_placing(id, path);
                match (done, &mut self.paths.placed[path as usize]) {
                    (Done::Stat(Some(meta)), Some(placement)) if placement.key == key => placement.meta = Some(meta),
                    (_, placement) => *placement = None,
                }
            }
            Own::Look { job: id, path, key, executable } => {
                let unchanged = matches!(
                    (&done, &self.paths.placed[path as usize]),
                    (Done::Stat(Some(meta)), Some(placement)) if placement.key == key && placement.meta == Some(*meta)
                );
                self.end_placing(id, path);
                if !unchanged {
                    self.place(w, id, path, key, executable);
                }
            }
            Own::Write { job: id, path } | Own::Mode { job: id, path, .. } => {
                let failed = match &done {
                    Done::Failed(error) => {
                        let name = &self.paths.names[path as usize];
                        // alloc: an error, once
                        Some(ApiError::new(format!("writing {name}: {error}"), Some(name.to_string())))
                    }
                    _ => None,
                };
                let Some(job) = self.end_placing(id, path) else { return };
                if let Some(error) = failed {
                    job.over.get_or_insert((false, Some(error)));
                } else if let Own::Mode { key, .. } = own {
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

    fn plane_finished(&mut self, w: &mut World<'_, S>, request: RequestId, ok: bool, stats: RequestStats) {
        // A kept front end's store is part of its job's build, which was marked as it finished.
        // Its node settled before its request finishes, which clears the job's `storing`: the
        // set says, not the job.
        if self.store_requests.remove(&request) {
            return;
        }
        blaze_profile::built(blaze_profile::Built {
            serial: u64::from(request.0),
            hits: stats.cached,
            misses: stats.ran,
            joined: stats.joined,
        });
        let jobs = &self.jobs;
        let Some(id) = self.order.iter().copied().find(|id| jobs.get(key_of(*id)).is_some_and(|it| it.request == Some(request)))
        else {
            return;
        };
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        job.finished = true;
        let outcome = match (job.cancelled.load(Ordering::Acquire), ok && !matches!(job.over, Some((false, _)))) {
            (true, _) => profile_history::Outcome::Cancelled,
            (false, true) => profile_history::Outcome::Ok,
            (false, false) => profile_history::Outcome::Failed,
        };
        self.rows.push(job.ended(request, Some(stats), outcome));
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
        let pack = ok && matches!(job.work, Work::Pack { .. } | Work::Doc { .. });
        let Stage::Units(build) = std::mem::replace(&mut job.stage, Stage::Over) else { return };
        let Build { input, front, mut writer, artifacts, evaluation, root, units, .. } = *build;
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
        let served = match writer.served() {
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
        // A pack's front end is not kept: recording its expansions changes its proc macros; nor
        // is the documentation's, which sets `cfg(doc)`.
        let packing = match pack {
            true => Some(writer),
            false => {
                if let Some(root) = root {
                    match writer.keep() {
                        // clone: the checkout's path, once per build
                        Ok(kept) => self.store_kept(w, id, root, input.config.invocation.root.clone(), kept),
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
        }
        let profile = input.units.finished.clone(); // clone: the report owns it, once per build
        let members = input.units.packages.iter().filter(|it| it.is_member).count();
        let members = u32::try_from(members).unwrap_or(u32::MAX);
        if let Some(writer) = packing {
            w.said.push(Said::Report(id, Response::Built { members, profile, artifacts }));
            self.pack(w, id, input, front, writer);
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
        let mut runs = match started {
            Ok(runs) => runs,
            Err(error) => {
                self.run_over(run);
                self.worked(w, id);
                self.end(w, id, false, Some(error));
                return;
            }
        };
        let mut out = std::mem::take(&mut self.out);
        let ended = runs.start(w.store, None, w.effects, &mut out);
        let puts = runs.take_puts();
        self.run_put(w, id, puts);
        for response in out.drain(..) {
            w.said.push(Said::Report(id, response));
        }
        self.out = out;
        if let Some(job) = self.jobs.get_mut(key_of(id)) {
            job.stage = Stage::Runs(run);
        }
        match ended {
            Some(ok) => {
                self.run_over(run);
                self.run_ended(w, id, None, ok);
            }
            None => {
                if let Some(it) = self.runs.get_mut(run) {
                    it.runs = Some(Box::new(runs)); // alloc: once per run of user code that waits on its effects
                }
            }
        }
    }

    /// [`Work::Pack`]'s or [`Work::Doc`]'s check built: the pack or the documentation made by
    /// one task of a plane request of its own, posted with the front end's writer, as no task
    /// reads the front end any more.
    fn pack(&mut self, w: &mut World<'_, S>, id: JobId, input: JobInput, front: Arc<S::Front>, writer: S::Writer) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        job.ready.push(Ready { input, root: None, front: Some(front), writer: Some(writer) });
        let work = match answer(job, &self.paths, w.steps) {
            Ok(work) => work,
            Err(error) => {
                self.end(w, id, false, Some(error));
                return;
            }
        };
        job.request = Some(w.plane.open(w.now, false, job.idle));
        job.opening();
        job.finished = false;
        job.closed = false;
        let what = match job.work {
            Work::Doc { .. } => "the documentation",
            _ => "the pack",
        };
        let fresh = lone(w.plane, NodeKind::Bodies, what, Pool::Work, None, kinds::WALK);
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
        let worked = Worked {
            reran: stats.ran,
            green: stats.cached + stats.joined,
            served: std::mem::take(&mut job.served),
            compiled,
            found: fresh,
            link: link_kind(&links),
            // alloc: one a link, at the request's end
            links: links.into_iter().map(|(path, how)| blaze_api::Linked { how: how.how(), path }).collect(),
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
            self.jobs
                .get(key_of(*id))
                .is_some_and(|job| matches!(&job.stage, Stage::Units(build) if build.writer.waiting()))
        })
    }

    /// How many kept front ends are being stored now ([`Jobs::store_kept`]).
    pub fn storing(&self) -> usize {
        self.jobs.values().filter(|it| it.storing.is_some()).count()
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
            return Some("a build script's run held until its program is placed (Jobs::release_held)");
        }
        match &job.stage {
            Stage::Units(build) if build.held.iter().any(|(task, _)| task.id == node) => {
                Some("a task held until its job's write is applied (Jobs::apply_writes)")
            }
            _ => None,
        }
    }

    pub fn stalled(&mut self, w: &mut World<'_, S>, error: &ApiError) {
        let jobs = &self.jobs;
        let stalled: Vec<JobId> = self
            .order
            .iter()
            .copied()
            .filter(|id| {
                jobs.get(key_of(*id)).is_some_and(|it| (it.request.is_some() && !it.finished) || it.storing.is_some())
            })
            .collect(); // alloc: once a stall
        for id in stalled {
            self.kill_runs(w, id);
            let Some(mut job) = self.jobs.remove(key_of(id)) else { continue };
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
                self.rows.push(job.ended(request, stats, profile_history::Outcome::Stalled));
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
            w.said.push(Said::Finish(id, false, Some(error.clone()), job.built));
        }
        // The lowerings of jobs the stall ended wait for nothing now.
        let jobs = &self.jobs;
        self.lowering.retain(|it| jobs.get(key_of(it.id)).is_some());
    }

    /// The daemon's store broke: every job still live fails with `error`, which names the store
    /// and why. Those waiting at their gate fail at once; every other one is ended (its plane
    /// request let go, its runs of user code killed) with the error as its answer, given as
    /// soon as nothing of it is left, so no client is told of the break by a hangup alone.
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

    fn cancel_job(&mut self, w: &mut World<'_, S>, id: JobId) {
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        job.cancelled.store(true, Ordering::Release);
        job.over.get_or_insert((false, None));
        if matches!(job.stage, Stage::Snapshot { .. }) {
            job.stage = Stage::Over;
        }
        refuse_held(w, &mut job.stage);
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
        let Some(job) = self.jobs.get_mut(key_of(id)) else { return };
        match &mut job.over {
            Some((was, _)) => *was = *was && ok,
            None => job.over = Some((ok, error)),
        }
        refuse_held(w, &mut job.stage);
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
    fn wind_up(&mut self, w: &mut World<'_, S>) {
        self.wind_up_judging(w, false);
    }

    /// [`Jobs::wind_up`], judging whether a wait for the store ran out of time only when
    /// `judge` is set ([`gave_up`]).
    fn wind_up_judging(&mut self, w: &mut World<'_, S>, judge: bool) {
        self.release_held(w);
        self.release_ready(w);
        self.apply_writes(w);
        let mut over = std::mem::take(&mut self.over);
        let mut heard = self.store_heard;
        let mut recheck = false;
        // Flushed once a winding up, for every job that waits ([`flush_once`]).
        let mut flushed = None;
        for &id in &self.order {
            let Some(job) = self.jobs.get_mut(key_of(id)) else { continue };
            if let Stage::Snapshot { ticket: None, heads, store } = &mut job.stage
                && w.images.done()
            {
                // The store's puts before it, durable; or the wait given up as a build's is
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
                        (None, Some(why)) => job.over = Some((false, Some(ApiError::new(why, None)))),
                    }
                }
            }
            let Some((ok, error)) = &mut job.over else { continue };
            if !(job.finished
                && job.placing == 0
                && job.runs == 0
                && job.storing.is_none()
                && !matches!(job.stage, Stage::Runs(..)))
            {
                continue;
            }
            // What its kept front end left for the store ([`Job::stored`]) and the retractions it
            // put ([`Job::durable`]) are durable first; the store was flushed as they were put.
            // A store that broke never makes them durable, nor does one that stopped committing:
            // the job fails naming why and which, rather than wait with no end (rule 15).
            // Wait for everything (the store model's `Wait::Everything`): a value a job read is
            // handed back with no ticket, so it may be another job's put that is not durable
            // yet, and answering on it before a crash loses it gives what a restart can't. Once,
            // as it first comes here, it also waits for every put the loop made by then
            // ([`Jobs::put`]), and for the last put of every segment of the store
            // ([`Store::last_puts`]), which covers every put made by then, on the loop or through
            // a writer of another thread, whether or not its caller handed the ticket over (an
            // error path that put before it failed, a fetch).
            let first = job.durable_since.is_none();
            if first {
                job.stored.extend(&self.put);
                job.stored.extend(&w.store.last_puts());
            }
            job.stored.retain_pending(|it| w.store.durable(it));
            job.durable.retain_pending(|it| w.store.durable(it));
            // Flushed while it waits, whatever it waits on ([`flush_for_wait`]): a job with no
            // kept front end (a run's puts alone, in `durable`) must flush too, or it waits for a
            // commit no one asked for; and a put counted before its record was published is
            // drained by a later flush, not by none.
            if !(job.stored.is_empty() && job.durable.is_empty()) {
                recheck |= flush_once(&mut flushed, &mut heard, w.store, w.now, w.effects);
            }
            let waiting = job.stored.iter().next().map(|it| (it, true)).or_else(|| job.durable.iter().next().map(|it| (it, false)));
            if let Some((ticket, stored)) = waiting {
                let since = *job.durable_since.get_or_insert(w.now);
                let Some(why) = gave_up(w.store, w.now, since, heard, judge) else { continue };
                let what = match (stored, &job.stored_of) {
                    // alloc: error path
                    (true, Some(checkout)) => format!(
                        "what the build of {checkout} left for the store or read from it (dependency images, \
                         check outcomes, goals; store ticket {ticket}) was put and is not durable, so a \
                         restarted daemon would work it out again: {why}"
                    ),
                    // alloc: error path
                    (true, None) => format!(
                        "what job {id} (`{}`, on the view of root {}) read from the store, or the loop put \
                         before it (store ticket {ticket}), was put and is not durable, so a restarted daemon \
                         would work it out again: {why}",
                        command_line(&job.work),
                        job.views.first().map_or_else(|| "none".to_owned(), |it| it.view.root.to_string()),
                    ),
                    // alloc: error path
                    (false, _) => format!(
                        "what job {id} (`{}`) put itself (a build script's or test's results, a retraction, \
                         the record of where a verified value's roots are; store ticket {ticket}) is not \
                         durable, so a restarted daemon would work it out, or give it, again: {why}",
                        command_line(&job.work),
                    ),
                };
                *ok = false;
                if error.is_none() {
                    *error = Some(ApiError::new(what, job.stored_of.clone())); // clone: an error, once
                }
            }
            over.push((id, *ok, error.take()));
        }
        // A commit a wait started is the store moving; a wait whose puts no commit takes looks
        // again shortly. Neither while the store is broken: its waits have failed above.
        self.store_heard = heard;
        self.recheck = (recheck && w.store.broken().is_none()).then(|| w.now + RECHECK);
        for (id, ok, error) in over.drain(..) {
            let Some(mut job) = self.jobs.remove(key_of(id)) else { continue };
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
                worked.placed =
                    blaze_api::PlacedBytes { cloned: placed.cloned, written: placed.written, copied: placed.copied };
                w.said.push(Said::Report(id, Response::Work(worked)));
            }
            // What it shared is committed for the other daemons, which find it at their next look.
            if job.shared {
                w.std.flush(w.effects);
            }
            for queries in job.queries {
                if job.built != Some(queries) {
                    w.db.close(queries);
                }
            }
            self.batches.remove(id);
            self.order.retain(|it| *it != id);
            self.ended = true;
            // Written before the answer, so the client that puts the profile together finds
            // this part; a build no profile asked for is in none, unless one under way is.
            if job.profiled {
                blaze_profile::write_part();
            } else if blaze_profile::on() && !self.jobs.values().any(|it| it.profiled) {
                blaze_profile::discard();
            }
            w.said.push(Said::Finish(id, ok, error, job.built));
        }
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
    }

    /// When, on the injected clock, the first job waiting for what it put to be durable gives
    /// up ([`DURABLE_WITHIN`] with no step of the store's commit answering, [`gave_up`]): the loop calls
    /// [`Jobs::expire`] then, if nothing woke it before.
    pub fn next_deadline(&self) -> Option<Duration> {
        let since = self.jobs.values().filter_map(|it| it.durable_since).min()?;
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

    /// The loop's clock reached [`Jobs::next_deadline`]: each job whose wait for the store is
    /// past it fails, naming what it waited for.
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
        w.images.ask(Parts { db: w.db, front, vfs: &mut *w.vfs, predict: w.plane.predict(), effects: &mut *w.effects });
        // What it waits for, read before the flush: a put another thread makes between the
        // flush's drain and the read would be waited on and drained by no commit.
        let mut store = self.put.clone(); // clone: one ticket per segment, once per snapshot
        store.extend(&w.store.last_puts());
        w.store.flush(w.effects);
        w.std.flush(w.effects);
        // alloc: once per snapshot
        let heads: Vec<(String, ContentId)> = w.vfs.heads().into_iter().map(|(head, root)| (head.to_owned(), root)).collect();
        let ticket = heads.first().and_then(|(head, root)| w.vfs.set_head(head, *root).ok());
        w.vfs.flush(w.effects);
        Stage::Snapshot { ticket, heads, store }
    }
}

/// The task that answers a job's work on its ready views: a reach, the codegen inputs, the
/// tables, a trace, a verify, an edit, the units a build ahead picks, a checked pack's pack, or
/// a checked documentation's pages.
fn answer<S: Steps>(job: &Job<S>, paths: &Paths<S>, steps: &Arc<S>) -> Result<Step<S>, ApiError> {
    let Some(ready) = job.ready.last() else { return Err(ApiError::new("the job has no view", None)) };
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
            let images: Vec<(&'static str, u64, u64)> = paths.images.iter().flatten().flat_map(|it| it.kept.tables()).collect();
            Does::Tables { front: lowered()?, images }
        }
        Work::Trace(_) => {
            // clone: a refcount, once per job
            Does::Trace { db: ready.input.db.clone(), queries: ready.input.queries, root: steps.plan_key() }
        }
        Work::Verify(_) => {
            let Some(view) = job.views.first().map(|it| it.view) else { return Err(ApiError::new("the job has no view", None)) };
            // clone: refcounts, once per job
            Does::Verify { steps: steps.clone(), input: ready.input.clone(), view }
        }
        // clone: the task owns the edits, once per job
        Work::Edit(edits) => Does::Edit { front: lowered()?, edits: edits.clone() },
        // clone: the task owns the filter, once per job
        Work::Ahead { filter, .. } => Does::Ahead { front: lowered()?, filter: filter.clone() },
        // clone: the task owns the program's name, once per job
        Work::Pack { program, .. } => Does::Pack { front: lowered()?, program: program.clone() },
        Work::Doc { no_deps, private, .. } => Does::Doc { front: lowered()?, no_deps: *no_deps, private: *private },
        Work::Check(_) | Work::Build(_) | Work::Test { .. } | Work::Run { .. } | Work::Snapshot => {
            return Err(ApiError::new("the job's work builds its units, answering nothing on its own", None));
        }
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

/// A job over, not ok, with `error`: whether or not it was over already, its first error kept.
fn fail_over(over: &mut Option<(bool, Option<ApiError>)>, error: ApiError) {
    match over {
        Some((ok, kept)) => {
            *ok = false;
            kept.get_or_insert(error);
        }
        None => *over = Some((false, Some(error))),
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
    artifact.executable.is_some() && (artifact.profile.test || artifact.kind == ArtifactKind::Doctest)
}

/// Unit `at` of `units` as a response names it; `None` for one past the graph, or whose package
/// or target the plan does not have (its diagnostics say so).
fn unit_of(units: &Units, at: usize) -> Option<UnitOf> {
    let unit = units.graph.units.get(at)?;
    let package = super::unit::package_of(units, unit).ok()?;
    Some(UnitOf {
        id: u32::try_from(at).ok()?,
        test: matches!(unit.mode, UnitMode::Test) || (matches!(unit.mode, UnitMode::Check) && unit.checked_as_test),
        package_id: unit.pkg_id.clone(),               // clone: the report owns it, once per unit
        manifest_path: package.manifest_path.clone(), // clone: the report owns it, once per unit
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
    let Some(compile) = build.unit(node).and_then(|run| super::unit::compile_of(&build.outputs.units, run)) else {
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
pub(crate) fn link_kind(links: &[(String, Linking)]) -> LinkKind {
    match links.iter().any(|(_, how)| how.placed()) {
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
    Some(Diagnostic {
        level: Level::Note,
        code: None,
        rendered: format!("note: {message}\n"), // alloc: as above
        message,
        spans: Vec::new(),    // alloc: none
        children: Vec::new(), // alloc: none
        facts: Vec::new(),    // alloc: none
        unit: None,
        explanation: None,
    })
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
fn verify<S: Steps>(steps: &S, input: &JobInput, view: blaze_vfs::View) -> Result<Vec<Mismatch>, Failed> {
    // alloc: a verify's own tree, once
    let config = blaze_query::Config { tree: Some(Arc::new(input.files.clone())), dir: None, tags: 0 };
    let mut scratch = Database::new(config);
    steps.register(&mut scratch);
    let queries = scratch.open_with(view, &input.config.encode());
    steps.plan(&scratch, queries)?;
    let mut mismatches = Vec::new(); // alloc: a verify, once
    for memo in walk(&scratch, steps.plan_key()) {
        let Some(ours) = input.db.memo(&memo.key) else { continue };
        if ours.fingerprint != memo.fingerprint {
            mismatches.push(Mismatch { query: memo.key, incremental: ours.fingerprint, scratch: memo.fingerprint, fact: None });
        }
    }
    Ok(mismatches)
}
