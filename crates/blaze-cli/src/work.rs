//! The tasks this binary runs (drive.md, "Tasks and Need"; "The system side"):
//!
//! - `Units`: one per manifest (units.rs): the root's members, a package's edition and default
//!   targets (`src/lib.rs`, `src/main.rs`, `src/bin/*.rs`), each a `Locate::File` part.
//! - `FileItems`: the file's outline (blaze-file-items), its `FileItems` record as the payload,
//!   each `mod x;` a `Locate::File` part, each syntax error a finding.
//! - Every other kind answers `Failed` naming what is not built: no item task is listed yet, so
//!   `check` is a syntax check of every file the crate roots reach until blaze-typeck's
//!   `Signature` and `BodyCheck` tasks exist (their lane).
//!
//! A batch's tasks are entered on this thread, one after another, and answered together in one
//! `Ran` (drive.md, "Scheduling on many cores"); the pool over `workers` waits on bstd's threaded
//! layout. Neither task kind here answers `Need`, so nothing is answered in place yet.

use blaze_drive::{
    Batch, Effect, Event, Input, Kind, Locate, Name, Outcome, Output, Part, PathOf, Ran, Request, Task, TaskFailure, Want,
    Why,
};
use crate::store::Store;
use blaze_drive::record::Finding;
use blaze_ids::ContentId;
use blaze_own::{AVec, Vec};
use core::alloc::Allocator;
use std::alloc::Global;
use blaze_surface::{Edition, ItemDetail, ItemKind};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame, TASK_FRAME, Tag};
use bstd::path::Path;

use crate::CliError;
use crate::phases::Phases;

/// The finding number of a syntax error: the outline's, rule-free (a lint has its rule's number).
pub const SYNTAX: u16 = 1;
/// The finding number of a module found at both its places, `x.rs` and `x/mod.rs` (rustc's
/// E0761). Its arguments are the two paths.
pub const MODULE_AT_BOTH: u16 = 4;

const OUT_DOMAIN: &str = "blaze-cli parts out 1";

/// The finding number of a task's typed failure, filed as its answer ([`filed`]): its one
/// argument is the failure's words.
pub const TASK_FAILED: u16 = 16;

/// The out of every filed failure ([`filed`]): a reader handed an input with this out was handed
/// a failure, never a payload, and fails `Waited` on it ([`failed_input`]).
pub(crate) fn failed_out() -> ContentId {
    ContentId::of("blaze-cli a task's typed failure 1", b"failed")
}

/// The first input of `task` that is a filed failure ([`failed_out`]).
pub(crate) fn failed_input(task: &Task<'_>) -> Option<Name> {
    let failed = failed_out();
    task.inputs.iter().find(|input| input.out == failed).map(|input| input.name)
}

/// Whether a task's typed failure is filed as its answer: a record under the task's key whose
/// read list is what the task was handed, holding the failure as a [`TASK_FAILED`] error. It
/// then stands like any answer while its key and its reads are today's, and is asked again
/// only when one of them moves: nothing is retried, and the request's answer is whole, so the
/// next command with nothing changed stands on its certificate (decision of 2026-11-16, drive.md
/// runner 15). Not filed, a hole the next command runs again: a panic (a bug, or a frame too
/// small: neither is a function of the inputs), and a `CrateScope` failure other than a wait (a
/// crate's names read checkout files its key does not hold; names.rs files those itself, with
/// the files in the read list). A failure on a fact outside the key and the reads is a
/// [`Did::Hole`] and never comes here; so is the failure of a task handed a hole (`Task::failed`
/// not empty: what it waited on is asked again next command, so its own failure is too).
fn filed(failure: &TaskFailure, task: &Task<'_>) -> bool {
    if !task.failed.is_empty() {
        return false;
    }
    match failure.why {
        Why::Panicked { .. } => false,
        Why::Waited { .. } => true,
        Why::NotBuilt | Why::Refused { .. } => failure.name.kind != Kind::CrateScope,
    }
}

/// `failure` of `task` as its filed answer ([`filed`]), in `held`: no payload, the failure as a
/// [`TASK_FAILED`] error (with the task's own words when it has them), and every input it was
/// handed as a read, with the out it had.
fn filed_record<'b>(task: &Task<'_>, failure: &TaskFailure, words: Option<&str>, plant: Plant, held: &'b Frame) -> Made<'b> {
    let mut made = Made::new_in(task.name, held);
    made.out = failed_out();
    let said = match words {
        Some(words) => bstd::mem::format_in!(held, "{failure}: {words}"),
        None => bstd::mem::format_in!(held, "{failure}"),
    };
    made.find(TASK_FAILED, (0, 0), &[said.as_str()]);
    if plant == Plant::FiledWithoutReads {
        return made;
    }
    for input in task.inputs.iter() {
        made.reads.push((input.name, input.out));
    }
    made
}

/// What one task made, in the allocator `A`: a frame on the coordinator's thread ([`Made`]), or
/// `Global` on a pool worker, whose frame ends with its task (copied into the batch's frame
/// once joined, in [`answer_batch`]).
pub(crate) struct Record<A: Allocator + Copy> {
    pub(crate) name: Name,
    pub(crate) out: ContentId,
    pub(crate) parts: Vec<Part, A>,
    /// (rule, start, end, message start, message end) into `messages`.
    pub(crate) findings: Vec<(u16, u32, u32, usize, usize), A>,
    pub(crate) messages: Vec<u8, A>,
    pub(crate) payload: Vec<u8, A>,
    /// The path of each `Locate::File` part: (its name, start and end in `path_bytes`, a module
    /// slot), so the drive can open it ([`PathOf`]).
    pub(crate) paths: Vec<(Name, u32, u32, bool), A>,
    pub(crate) path_bytes: Vec<u8, A>,
    /// What it read, each with the out its input had: empty for a task its source keys whole.
    /// A task keyed by an identity (a `CrateScope`) lists the records it read here, so the
    /// machine keeps its record standing while each read's out is today's.
    pub(crate) reads: Vec<(Name, ContentId), A>,
}

/// What one task made, held in the frame until its batch is answered.
pub(crate) type Made<'f> = Record<&'f Frame>;

impl<A: Allocator + Copy> Record<A> {
    pub(crate) fn new_in(name: Name, alloc: A) -> Record<A> {
        Record {
            name,
            out: ContentId([0; 32]),
            parts: Vec::new_in(alloc),
            findings: Vec::new_in(alloc),
            messages: Vec::new_in(alloc),
            payload: Vec::new_in(alloc),
            paths: Vec::new_in(alloc),
            path_bytes: Vec::new_in(alloc),
            reads: Vec::new_in(alloc),
        }
    }

    pub(crate) fn clone_in<'g>(&self, frame: &'g Frame) -> Made<'g> {
        Record {
            name: self.name,
            out: self.out,
            parts: Vec::from_slice_in(&self.parts, frame),
            findings: Vec::from_slice_in(&self.findings, frame),
            messages: Vec::from_slice_in(&self.messages, frame),
            payload: Vec::from_slice_in(&self.payload, frame),
            paths: Vec::from_slice_in(&self.paths, frame),
            path_bytes: Vec::from_slice_in(&self.path_bytes, frame),
            reads: Vec::from_slice_in(&self.reads, frame),
        }
    }

    /// The part `name`'s file is at `path`.
    pub(crate) fn path(&mut self, name: Name, path: &[u8], module: bool) {
        let at = self.path_bytes.len() as u32;
        self.path_bytes.extend_from_slice(path);
        self.paths.push((name, at, self.path_bytes.len() as u32, module));
    }
}

/// What a request's tasks share: the stamps, the rows this request read (its opens and the
/// layout's), where each slot was found, and the layout once read.
pub struct Facts<'f, S> {
    pub stamps: S,
    pub rows: crate::sweep::Rows<'f>,
    pub found: crate::sweep::Found<'f>,
    pub laid: Option<crate::units::Laid<'f>>,
    pub plant: Plant,
    /// At most this many of a batch's tasks at once (`--workers`, else one per core).
    pub workers: u32,
    /// What the `Fetch` tasks need of the command (fetch.rs).
    pub fetch_env: crate::fetch::FetchEnv<'f>,
    /// The graph's macro crates with their identities, listed by the first `MacroModule` task
    /// of the command (macros.rs).
    pub macros: Option<crate::macros::MacroCrates>,
    /// The names world and what the `CrateScope` tasks made this command (names.rs): made by
    /// the `UnitGraph` task, or by the first `CrateScope` task when the graph was answered.
    pub names: Option<crate::names::Names<'f>>,
    /// The `CrateScope` records the `BodyCheck` tasks read this command (body_types.rs).
    pub bodies: crate::body_types::Kept,
}

pub(crate) enum Did<'f> {
    Done(Made<'f>),
    /// What the task lacks, in the batch's frame: the machine enters it again from the top with
    /// them as its inputs (`Ran::Need`), each read with [`payload`].
    Need(Vec<Want, &'f Frame>),
    Failed(TaskFailure),
    /// Failed, with words of its own the closing lines print (a fetch's network failure).
    FailedSaying(TaskFailure, &'f str),
    /// Failed on a fact outside its key and its reads (a module the home's store lacks): never
    /// filed ([`filed`]), a hole the next command runs again; printed as `FailedSaying` is.
    Hole(TaskFailure, &'f str),
}

/// A record an input names, read whole into a task's frame: [`Input::payload`] its kind's own
/// bytes. The record stays in the frame until the task ends.
pub(crate) struct Read<'f> {
    bytes: Bytes<'f>,
    payload: core::ops::Range<usize>,
}

impl Read<'_> {
    /// The record's payload (blaze-surface's or the kind's own form).
    pub(crate) fn payload(&self) -> &[u8] {
        &self.bytes.as_slice()[self.payload.clone()]
    }
}

/// Reads the record `input` names (its key `at`) from `store` into `into`, the task's scratch:
/// what a task entered again with its needs reads of each. A record that is absent, not a
/// record, or a shape (a name, not a body) is a typed error naming the input: the machine
/// gave it as settled, so any of these is the tool's fault, never a miss to recompute.
pub(crate) fn payload<'f>(cx: &Cx, store: &mut dyn Store, input: &Input, into: &'f Frame) -> Result<Read<'f>, CliError> {
    let cx = cx.span("read an input");
    let mut bytes = Bytes::new_in(into, "an input's record");
    if !store.get_into(&cx, input.at, &mut bytes)? {
        return Err(CliError::Input { name: input.name, why: InputWhy::Absent });
    }
    let payload = match blaze_drive::record::open(bytes.as_slice()) {
        Ok(blaze_drive::record::Stored::Whole(view)) => {
            let at = view.payload.as_ptr() as usize - bytes.as_slice().as_ptr() as usize;
            at..at + view.payload.len()
        }
        Ok(blaze_drive::record::Stored::Shape(_)) => return Err(CliError::Input { name: input.name, why: InputWhy::Shape }),
        Err(error) => return Err(CliError::Input { name: input.name, why: InputWhy::Malformed(error) }),
    };
    Ok(Read { bytes, payload })
}

/// Why an input's record could not be read ([`CliError::Input`]).
#[derive(Debug)]
pub enum InputWhy {
    /// The store holds nothing at its key.
    Absent,
    /// Its bytes are not a record of this tool's form.
    Malformed(blaze_drive::WireError),
    /// It is a shape record, which has no payload.
    Shape,
}

impl core::fmt::Display for InputWhy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InputWhy::Absent => f.write_str("the store holds no record at its key"),
            InputWhy::Malformed(error) => write!(f, "its record is not one: {error}"),
            InputWhy::Shape => f.write_str("its record is a shape, with no payload"),
        }
    }
}

/// The frames tasks run in (rule 11), apart from the request's: what a task works in, reset
/// after each task, and what a batch's records are held in until the machine has taken them,
/// reset after each batch. So the request's own frame holds the layout and small facts, never a
/// file's text, an outline or a record body: its footprint does not grow with the workspace.
pub struct Frames {
    pub scratch: Frame,
    pub batch: Frame,
    /// What the pooled jobs of one [`run_batches`] call read while they run (each job's path):
    /// reset when the call's scope has joined, so nothing a job needs is held past its run.
    pub jobs: Frame,
}

/// The most each of [`Frames`] holds, and a bug to plant (the tests' controls).
#[derive(Clone, Copy, Debug)]
pub struct Sizes {
    pub scratch: usize,
    pub batch: usize,
    pub plant: Plant,
}

/// A bug a test plants in a task: [`Plant::None`] outside the tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    None,
    /// The `FileItems` task of the file at this path panics.
    PanicIn(&'static str),
    /// The drive's own planted bug (`blaze_drive::Plant`), for the command-level properties.
    Drive(blaze_drive::Plant),
    /// The sweep's own planted bug (`sweep::Plant`), for the command-level properties.
    Sweep(crate::sweep::Plant),
    /// A filed failure keeps no reads ([`filed_record`]): it stands after an input it was
    /// handed changed, which the filed-failure test must catch.
    FiledWithoutReads,
    /// The command's tool names this body checker, not [`crate::body_types::BODY_CHECKER`]
    /// (home.rs `tool_of`): an older checker's records, for the producer test.
    BodyChecker(&'static str),
}

impl Sizes {
    /// What a command runs with: a task's frame for each.
    pub const USUAL: Sizes = Sizes { scratch: TASK_FRAME, batch: TASK_FRAME, plant: Plant::None };

    /// The frames, reserved: no memory is taken until used.
    pub fn frames(self) -> Result<Frames, CliError> {
        Ok(Frames {
            scratch: Frame::with_reserve(self.scratch, Tag::Other).map_err(|_| CliError::Text { what: "a task's scratch frame could not be reserved" })?,
            batch: Frame::with_reserve(self.batch, Tag::Other).map_err(|_| CliError::Text { what: "a batch's frame could not be reserved" })?,
            jobs: Frame::with_reserve(self.batch, Tag::Other).map_err(|_| CliError::Text { what: "the pooled jobs' frame could not be reserved" })?,
        })
    }
}

/// A task that failed, with the file it is about or in (`None` for a manifest's or a crate's),
/// kept to be drawn after the findings.
/// `text` copied into `frame`, held as long as it is.
pub(crate) fn held_str<'f>(text: &str, frame: &'f Frame) -> &'f str {
    let bytes: &'f [u8] = Vec::from_slice_in(text.as_bytes(), frame).leak();
    // A copy of a `str`'s bytes is UTF-8: the check cannot fail.
    core::str::from_utf8(bytes).unwrap_or_default()
}

pub struct Failed<'f> {
    pub path: Option<&'f str>,
    pub failure: TaskFailure,
    /// The task's own words, when it has them (`Did::FailedSaying`).
    pub detail: Option<&'f str>,
}

/// The most of each frame used at once, over the request: the profile's `frames` line.
#[derive(Clone, Copy, Default, Debug)]
pub struct HighWater {
    /// One task's work.
    pub scratch: usize,
    /// One batch's records, until the machine took them.
    pub batch: usize,
    /// One record read back.
    pub got: usize,
    /// The pooled jobs' paths and body lists of one round of batches.
    pub jobs: usize,
    /// The request's own frame at the answer.
    pub request: usize,
}

/// Run every task of `batches` (every batch the request has out), then answer the machine once
/// a batch, in order; the batch frame is reset after each.
#[expect(clippy::too_many_arguments, reason = "the batch's edge: each argument one handle or fact of it")]
pub fn run_batches<'f, 'r>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    req: &mut Request<'r>,
    batches: std::vec::Vec<Batch<'r>, Global>,
    frame: &'f Frame,
    frames: &mut Frames,
    failed: &mut Vec<Failed<'f>, &'f Frame>,
    high: &mut HighWater,
    alloc: blaze_own::Alloc<'_>,
    effects: &mut AVec<'_, Effect<'r>>,
    phases: &mut Phases,
) -> Result<(), CliError> {
    let cx = cx.span("batches");
    let began = phases.now();
    let steps_before = phases.request.steps;
    unwind_refusals();
    let Frames { scratch, batch: held, jobs } = frames;
    // Every batch's `FileItems` tasks go to the pool at once, each in its own frame, so the
    // pool is as wide as every batch the request has handed out (it hands out one a worker);
    // then each batch is answered in turn: its other tasks run here, its pooled ones joined.
    // A batch's `BodyCheck` tasks are one job a home file: the file is read and outlined once
    // for all its bodies, not once a body (rule 77).
    let clock = phases.clock().clone();
    let scope = cx.scope();
    if let Some(most) = core::num::NonZeroU32::new(facts.workers) {
        scope.narrow_workers(most, bstd::task::Class::Foreground);
    }
    let task_frame = scratch.reserved();
    let mut pending = std::vec::Vec::with_capacity_in(batches.len(), Global);
    let mut groups: Vec<Group<'_>, Global> = Vec::new_in(Global);
    {
        // What a job reads while it runs lives in the jobs' frame, reset below once the scope
        // has joined: the command's own frame takes nothing per task (memory by lifetime).
        let jobs: &Frame = jobs;
        // The round's body jobs being gathered: each file's job and its bodies so far, over
        // every batch (a batch is of one home; two batches may be of the same one).
        let mut files: Vec<(Job<'_>, Vec<(Name, u32), &Frame>, &crate::body_types::TypesJob<'_>), Global> = Vec::new_in(Global);
        for batch in batches.iter() {
            let mut tasks = Vec::with_capacity_in(batch.tasks.len(), Global);
            for (_, task) in batch.tasks.iter() {
                if let Some(on) = failed_input(task) {
                    tasks.push(Spawned::Waited(on));
                    continue;
                }
                tasks.push(match task.name.kind {
                    Kind::FileItems => match job_of(req, facts, task, jobs) {
                        Ok(job) => Spawned::One(scope.may_overlap("outline", task_frame, {
                            let clock = &clock;
                            move |cx: &Cx| Ok::<_, core::convert::Infallible>(pooled(cx, root, &job, clock))
                        })),
                        Err(what) => Spawned::Refused(what),
                    },
                    Kind::BodyCheck => {
                        // The crates its types read come first (body_types.rs): a task that
                        // lacks them answers `Need`, and joins its file's job only with them.
                        let path = req.file_of(task);
                        let known = files.iter().position(|(job, _, _)| Some(job.slot.as_bytes()) == path && job.edition == task.edition);
                        let job = match known {
                            Some(group) => Ok((None, files[group].0.slot, files[group].0.flags)),
                            None => job_of(req, facts, task, jobs).map(|job| {
                                let (slot, flags) = (job.slot, job.flags);
                                (Some(job), slot, flags)
                            }),
                        };
                        match job {
                            Err(what) => Spawned::Refused(what),
                            Ok((job, slot, flags)) => {
                                let real = crate::sweep::real_path(slot, flags, jobs);
                                let real = core::str::from_utf8(real.leak()).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
                                match crate::body_types::prepare(&cx, root, store, facts, task, real, frame, jobs, alloc)? {
                                    crate::body_types::Prepared::Need(wants) => Spawned::Need(wants),
                                    crate::body_types::Prepared::Waited(on) => Spawned::Waited(on),
                                    crate::body_types::Prepared::Refused(what) => Spawned::Refused(what),
                                    crate::body_types::Prepared::Ready(types) => {
                                        let group = match (known, job) {
                                            (Some(group), _) => group,
                                            (None, Some(job)) => {
                                                files.push((job, Vec::new_in(jobs), types));
                                                files.len() - 1
                                            }
                                            (None, None) => return Err(CliError::Text { what: "a body's file job was neither found nor made" }),
                                        };
                                        let bodies = &mut files[group].1;
                                        match bodies.try_push((task.name, task.place)) {
                                            Ok(()) => Spawned::Body { group, at: bodies.len() - 1 },
                                            Err(_) => Spawned::Refused(JOBS_FULL),
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => Spawned::Here,
                });
            }
            pending.push(tasks);
        }
        groups.try_reserve_exact(files.len()).map_err(|_| CliError::Text { what: "the list of a round's body jobs could not be made" })?;
        for (file, bodies, types) in files {
            let job = BodiesJob { file, bodies: bodies.leak(), types };
            groups.push(Group::Running(scope.may_overlap("bodies", task_frame, {
                let clock = &clock;
                move |cx: &Cx| Ok::<_, core::convert::Infallible>(pooled_bodies(cx, root, &job, clock))
            })));
        }
    }
    phases.took("jobs made", frame);
    let mut answered = Ok(());
    for (batch, of_batch) in batches.into_iter().zip(pending) {
        if answered.is_err() {
            // A command stopping: what is still pooled is joined with the scope, unread.
            continue;
        }
        answered = answer_batch(&cx, root, store, facts, req, batch, of_batch, &mut groups, frame, held, scratch, failed, high, alloc, effects, phases);
        high.batch = high.batch.max(held.peak());
        held.reset();
    }
    // A file job no body's task joined (a command stopping) is joined with the scope, unread.
    drop(groups);
    let joined = scope.join(&cx).map_err(CliError::Pool);
    // Every job has ended: nothing reads the jobs' frame now.
    high.jobs = high.jobs.max(jobs.peak());
    jobs.reset();
    // The batches' steps are counted as steps, not twice as runs.
    let stepped = phases.request.steps - steps_before;
    phases.request.runs += phases.since(began).saturating_sub(stepped);
    answered.and(joined)
}

/// One task of a batch as [`run_batches`] spawned it.
enum Spawned<'s> {
    /// Run on the main thread, in `answer_batch`.
    Here,
    /// Its job could not be made: the task fails with these words.
    Refused(&'static str),
    /// A `FileItems` task's own pool handle.
    One(bstd::task::Task<'s, Pooled>),
    /// A `BodyCheck` task: the `at`th body of the round's `group`th file job.
    Body { group: usize, at: usize },
    /// An input it was handed is a filed failure ([`failed_input`]): it fails `Waited` on it,
    /// unrun.
    Waited(Name),
    /// A `BodyCheck` task that lacks inputs (body_types.rs `prepare`): it answers `Need`.
    Need(std::vec::Vec<Want, Global>),
}

/// A batch's tasks as [`run_batches`] spawned them.
type Pending<'s> = Vec<Spawned<'s>, Global>;

/// One file's body job of a round ([`BodiesJob`]): on the pool, then joined by the first of its
/// bodies' tasks answered, its outcomes taken one a task.
enum Group<'s> {
    Running(bstd::task::Task<'s, PooledBodies>),
    /// Each body's outcome, and the job's time a body (the file's one read and outline are
    /// shared by its bodies: each is charged its part).
    Joined { did: BodiesDid, ns: u64 },
    /// Being joined (never seen: the join's own state).
    Taken,
}

/// One batch: its tasks not pooled run here, in order, while the pool works; then each pooled
/// one is joined in ticket order, its record copied into the batch's frame, and the batch's
/// outcomes step the request. A scope narrowed to one worker ran each inline at its spawn.
#[expect(clippy::too_many_arguments, reason = "run_batches's, with its frames split")]
fn answer_batch<'f, 'r>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    req: &mut Request<'r>,
    batch: Batch<'r>,
    spawned: Pending<'_>,
    groups: &mut Vec<Group<'_>, Global>,
    frame: &'f Frame,
    held: &Frame,
    scratch: &mut Frame,
    failed: &mut Vec<Failed<'f>, &'f Frame>,
    high: &mut HighWater,
    alloc: blaze_own::Alloc<'_>,
    effects: &mut AVec<'_, Effect<'r>>,
    phases: &mut Phases,
) -> Result<(), CliError> {
    let cx = cx.span("batch");
    let mut did = Vec::with_capacity_in(batch.tasks.len(), held);
    let mut took = Vec::with_capacity_in(batch.tasks.len(), held);
    let mut ran_here = Vec::with_capacity_in(batch.tasks.len(), held);
    for ((_, task), spawned) in batch.tasks.iter().zip(spawned.iter()) {
        if !matches!(spawned, Spawned::Here) {
            ran_here.push(None);
            continue;
        }
        let at = phases.now();
        phases.took("a batch's lists", frame);
        let done = run_one(&cx, root, store, facts, task, frame, held, scratch, high, alloc)?;
        phases.took(row_of(task.name.kind), frame);
        ran_here.push(Some((done, phases.since(at))));
    }
    for (((ticket, task), spawned), here) in batch.tasks.iter().zip(spawned).zip(ran_here) {
        let (done, ns) = match (spawned, here) {
            (_, Some(done)) => done,
            (Spawned::Refused(what), _) => (Did::Failed(TaskFailure { name: task.name, why: Why::Refused { what, rule: "blaze-cli work.rs" } }), 0),
            (Spawned::Waited(on), _) => (Did::Failed(TaskFailure { name: task.name, why: Why::Waited { on } }), 0),
            (Spawned::Need(wants), _) => (Did::Need(Vec::from_slice_in(&wants, held)), 0),
            (Spawned::Body { group, at }, _) => {
                let unanswered = Why::Refused { what: "a file's body job answered nothing for this body", rule: "blaze-cli work.rs, pooled_bodies" };
                let Some(slot) = groups.get_mut(group) else {
                    return Err(CliError::Text { what: "a body's task names a file job its round did not start" });
                };
                // The file's job is joined by the first of its bodies answered.
                if matches!(slot, Group::Running(_))
                    && let Group::Running(handle) = core::mem::replace(slot, Group::Taken)
                {
                    let joined = handle.join(&cx).map_err(CliError::Pool)?;
                    high.scratch = high.scratch.max(joined.used);
                    phases.worked(BODY_JOBS, joined.ns);
                    if let Some(error) = joined.stopped {
                        return Err(error.into());
                    }
                    *slot = Group::Joined { ns: joined.ns / (joined.did.len().max(1) as u64), did: joined.did };
                }
                let (done, ns) = match slot {
                    Group::Joined { did, ns } => (did.get_mut(at).and_then(Option::take), *ns),
                    Group::Running(_) | Group::Taken => (None, 0),
                };
                let done = match done {
                    Some(Ok(record)) => Did::Done(record.clone_in(held)),
                    Some(Err(why)) => Did::Failed(TaskFailure { name: task.name, why }),
                    None => Did::Failed(TaskFailure { name: task.name, why: unanswered }),
                };
                phases.worked(row_of(task.name.kind), ns);
                (done, ns)
            }
            (Spawned::One(handle), _) => {
                let joined = handle.join(&cx).map_err(CliError::Pool)?;
                high.scratch = high.scratch.max(joined.used);
                // A file's text this build refuses (not UTF-8, past 4 GiB) fails its own task,
                // a function of the text (rule 20); a path or a read that failed stops the command.
                let did = match joined.did {
                    Ok(did) => did,
                    Err(Unread::Text(what)) => Err(Why::Refused { what, rule: "blaze-cli work.rs" }),
                    Err(Unread::Path(error)) => return Err(CliError::Path(error)),
                    Err(Unread::Fs(error)) => return Err(CliError::Fs(error)),
                };
                let done = match did {
                    Ok(record) => Did::Done(record.clone_in(held)),
                    Err(why) => Did::Failed(TaskFailure { name: task.name, why }),
                };
                phases.worked(row_of(task.name.kind), joined.ns);
                (done, joined.ns)
            }
            (Spawned::Here, None) => unreachable!("a task neither pooled nor run here"),
        };
        // A typed failure that is a function of the task's key and inputs is its answer, filed.
        let done = match done {
            Did::Failed(failure) if filed(&failure, task) => Did::Done(filed_record(task, &failure, None, facts.plant, held)),
            Did::FailedSaying(failure, words) if filed(&failure, task) => Did::Done(filed_record(task, &failure, Some(words), facts.plant, held)),
            other => other,
        };
        if let Did::Failed(failure) | Did::FailedSaying(failure, _) | Did::Hole(failure, _) = &done {
            let path = match req.file_of(task) {
                Some(path) => Some(core::str::from_utf8(Vec::from_slice_in(path, frame).leak()).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?),
                None => None,
            };
            let detail = match &done {
                Did::FailedSaying(_, words) | Did::Hole(_, words) => Some(held_str(words, frame)),
                _ => None,
            };
            failed.push(Failed { path, failure: *failure, detail });
        }
        did.push((*ticket, done));
        took.push(ns);
    }
    phases.took("pooled tasks joined (a failed task's path and words kept)", frame);
    let request_frame = frame;
    let frame = held;
    let mut paths: Vec<Vec<PathOf<'_>, &Frame>, &Frame> = Vec::with_capacity_in(did.len(), frame);
    for (_, done) in did.iter() {
        let mut held = Vec::new_in(frame);
        if let Did::Done(made) = done {
            for (name, at, to, module) in made.paths.iter().copied() {
                held.push(PathOf { name, path: &made.path_bytes[at as usize..to as usize], module });
            }
        }
        paths.push(held);
    }
    let mut findings: Vec<Vec<Finding<'_>, &Frame>, &Frame> = Vec::with_capacity_in(did.len(), frame);
    for (_, done) in did.iter() {
        let mut held = Vec::new_in(frame);
        if let Did::Done(made) = done {
            for (rule, start, end, at, to) in made.findings.iter().copied() {
                held.push(Finding { rule, start, end, message: &made.messages[at..to] });
            }
        }
        findings.push(held);
    }
    let mut outputs: Vec<Output<'_>, &Frame> = Vec::with_capacity_in(did.len(), frame);
    for (((_, done), findings), paths) in did.iter().zip(findings.iter()).zip(paths.iter()) {
        if let Did::Done(made) = done {
            outputs.push(Output {
                name: made.name,
                out: made.out,
                reads: &made.reads,
                parts: &made.parts,
                paths,
                findings,
                payload: &made.payload,
            });
        }
    }
    let mut results = Vec::with_capacity_in(did.len(), frame);
    let mut at = 0;
    for ((ticket, done), ns) in did.iter().zip(took.iter()) {
        let ran = match done {
            Did::Done(_) => {
                at += 1;
                Ran::Done(&outputs[at - 1..at])
            }
            Did::Need(wants) => Ran::Need(wants),
            Did::Failed(failure) | Did::FailedSaying(failure, _) | Did::Hole(failure, _) => Ran::Failed(*failure),
        };
        results.push(Outcome { ticket: *ticket, ran, in_place: 0, ns: *ns });
    }
    let stepped = phases.now();
    phases.took("a batch's lists", request_frame);
    // A file a crate's names read that no units record and no outline lists stops the machine
    // by its name alone (a digest): the names step knows which crate read which path, so the
    // error names both (rule 19).
    req.step(Event::Ran { batch: batch.id, results: &results }, effects).map_err(|error| match (&error, facts.names.as_ref()) {
        (blaze_drive::DriveError::NoPart { name, .. }, Some(names)) => match names.reader_of(*name) {
            Some(unlisted) => CliError::Unlisted(unlisted),
            None => CliError::Drive(error),
        },
        _ => CliError::Drive(error),
    })?;
    phases.took(crate::STEP, request_frame);
    phases.request.steps += phases.since(stepped);
    Ok(())
}

/// The profile's row of the body jobs, one a file a round: its times are the files read and
/// outlined for their bodies ([`Phases::times`]).
pub(crate) const BODY_JOBS: &str = "pooled body jobs, one a file a round (worker ns)";

/// The profile's row of the `BodyCheck` tasks.
pub(crate) const BODIES: &str = "pooled BodyCheck (worker ns)";

/// A task kind's row in the profile's frame lines (`Phases::took` for a task run on the main
/// thread, `Phases::worked` for a pooled one).
fn row_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Units => "task Units",
        Kind::UnitGraph => "task UnitGraph",
        Kind::Fetch => "task Fetch",
        Kind::Surface => "task Surface",
        Kind::MacroModule => "task MacroModule",
        Kind::CrateScope => "task CrateScope",
        Kind::FileItems => "pooled FileItems (worker ns)",
        Kind::BodyCheck => BODIES,
        _ => "task of another kind",
    }
}

/// A collection outside bstd's (std's `Vec` over a frame, as blaze-format's writer uses) meets
/// a full frame through `handle_alloc_error`, which aborts the process by default: the whole
/// command, not the one task. The hook makes that refusal a panic like bstd's, so a task's
/// `catch_unwind` fails it alone. It is the process's one hook, set once, before the first
/// task; nothing else in blaze sets it (owed: drive.md, "For a successor").
fn unwind_refusals() {
    static UNWIND_REFUSALS: std::sync::Once = std::sync::Once::new();
    UNWIND_REFUSALS.call_once(|| {
        std::alloc::set_alloc_error_hook(|layout| panic!("a task's frame refused {} bytes", layout.size()));
    });
}

/// One task, its scratch reset after it. Rule 20: a panic (an allocation its frame refused, a
/// bug) or an input this build refuses fails that task alone, by its name, and the request goes
/// on; a panic's message is the panic hook's, on the error stream. The pool's tasks will get
/// the same from bstd's task scope; this thread runs them one by one today.
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn run_one<'f, 'b>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    task: &Task<'_>,
    frame: &'f Frame,
    held: &'b Frame,
    scratch: &mut Frame,
    high: &mut HighWater,
    alloc: blaze_own::Alloc<'_>,
) -> Result<Did<'b>, CliError> {
    let ran = unwinding(|| run(cx, root, store, facts, task, frame, held, scratch, alloc));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    high.scratch = high.scratch.max(used);
    scratch.reset();
    let failed = |why| Ok(Did::Failed(TaskFailure { name: task.name, why }));
    match ran {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(CliError::Text { what })) => failed(Why::Refused { what, rule: "blaze-cli work.rs" }),
        Ok(Err(error)) => Err(error),
        Err(Panicked) => failed(Why::Panicked { used: used as u64, limit: limit as u64 }),
    }
}

/// A panic caught at an edge: its message went to the error stream by the panic hook.
pub(crate) struct Panicked;

/// `work`, with a panic inside it caught: the one home of the catch, for a task's edge
/// ([`run_one`]) and the command's (`main.rs`, `command`). What `work` borrowed is read again
/// only for what a stopped piece of work leaves true (a frame's peak, the phases marked).
pub(crate) fn unwinding<T>(work: impl FnOnce() -> T) -> Result<T, Panicked> {
    unwind_refusals();
    std::panic::catch_unwind(core::panic::AssertUnwindSafe(work)).map_err(|_| Panicked)
}

/// Whether this worker builds tasks of `kind`: the one place that says so. `run` answers every
/// other kind `NotBuilt`, and the command's closing note names the stages its kinds leave out
/// (`left_out`). A new kind is added here and in `run`'s match together.
pub const fn builds(kind: Kind) -> bool {
    match kind {
        Kind::Units
        | Kind::UnitGraph
        | Kind::Fetch
        | Kind::FileItems
        | Kind::Surface
        | Kind::MacroModule
        | Kind::CrateScope
        | Kind::BodyCheck => true,
        Kind::ModuleScope | Kind::Group | Kind::Expansion | Kind::Signature | Kind::ImplBucket | Kind::Code | Kind::Test => false,
    }
}

/// Whether the task this worker runs for `kind` does the whole of `kind.stage()`: a
/// `BodyCheck` task parses its body here, and checks none of its types or borrows, so it is
/// built ([`builds`]) but not whole, and no note counts it as types checked.
pub const fn whole(kind: Kind) -> bool {
    builds(kind) && !matches!(kind, Kind::BodyCheck)
}

/// What this worker's task of `kind` does, in the words of a note: `kind.stage()` when it is
/// [`whole`], else the part it does.
pub fn does(kind: Kind) -> &'static str {
    match kind {
        Kind::BodyCheck => "check the bodies' syntax and types",
        _ => kind.stage(),
    }
}

/// The names stage's word in [`CHECK_STAGES`]: the one stage a command can also find not run
/// (main.rs `Finished::names_held`).
pub(crate) const NAMES_STAGE: &str = "names";

/// The types stage's word in [`CHECK_STAGES`]: not run when any body says it was not checked
/// (body_types.rs `TYPES_NOT_CHECKED`, counted by print.rs).
pub(crate) const TYPES_STAGE: &str = "types";

/// The one list of the stages `check` is defined to run, in order, and whether this build runs
/// each, derived from the kinds this worker builds: the closing line's "NOT checked" and
/// `blaze explain 3` read it. When all are `true`, `check` can say `ok`.
pub(crate) const CHECK_STAGES: [(&str, bool); 6] = [
    ("manifests", builds(Kind::Units) && builds(Kind::UnitGraph)),
    ("syntax of items", builds(Kind::FileItems)),
    ("syntax inside bodies", builds(Kind::BodyCheck)),
    (NAMES_STAGE, builds(Kind::CrateScope)),
    // A body's types are its `BodyCheck` task's, over records, with the heads it reads made
    // in the task (body_types.rs). A body it could not check to the end is a
    // `TYPES_NOT_CHECKED` finding, which turns this stage to "NOT checked" (main.rs `close`).
    (TYPES_STAGE, builds(Kind::BodyCheck)),
    ("borrows", whole(Kind::BodyCheck)),
];

/// Whether `command`'s program runner (blaze_ladder) does `kind`'s work for the code the program
/// reaches, in the subset of Rust its front end reads: `run` only. Macro expansion and the
/// dependencies are not claimed for it.
pub fn runner_does(command: blaze_drive::Command, kind: Kind) -> bool {
    matches!(command, blaze_drive::Command::Run)
        && matches!(kind, Kind::ModuleScope | Kind::Group | Kind::Signature | Kind::ImplBucket | Kind::BodyCheck | Kind::Code)
}

/// Whether `command` covers `kind` in this build: the worker does the whole of it, or the
/// runner does it.
pub fn covers(command: blaze_drive::Command, kind: Kind) -> bool {
    whole(kind) || runner_does(command, kind)
}

/// The stages `command` is defined to cover that this build leaves out, each named once, in
/// the command's order: empty once every kind it needs is covered.
pub fn left_out(command: blaze_drive::Command) -> impl Iterator<Item = &'static str> {
    let needs = command.needs();
    let out = move |kind: &Kind| !covers(command, *kind);
    needs.iter().enumerate().filter(move |(i, kind)| out(kind) && !needs[..*i].iter().any(|k| out(k) && k.stage() == kind.stage())).map(|(_, kind)| kind.stage())
}

#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
fn run<'f, 'b>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    task: &Task<'_>,
    frame: &'f Frame,
    held: &'b Frame,
    #[expect(unused_variables, reason = "the names lane's ModuleScope arm works in it")] scratch: &Frame,
    alloc: blaze_own::Alloc<'_>,
) -> Result<Did<'b>, CliError> {
    let cx = cx.span("task");
    if !builds(task.name.kind) {
        return Ok(Did::Failed(TaskFailure { name: task.name, why: Why::NotBuilt }));
    }
    match task.name.kind {
        Kind::Units => {
            // The layout is read here only when the request did not read it up front: a units
            // record to make although nothing it read moved (a store without it).
            if facts.laid.is_none() {
                facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, frame, alloc)?);
            }
            crate::units::run(facts.laid.as_ref().expect("laid above"), task, held)
        }
        Kind::UnitGraph => match facts.laid.as_mut().and_then(|laid| laid.lock.as_mut().zip(laid.graph.as_mut())) {
            Some((lockfile, graph)) => {
                let lock = lockfile.lock;
                let did = crate::graph::run(&cx, lockfile, graph, task, &facts.fetch_env, frame, held)?;
                let Did::Done(mut record) = did else { return Ok(did) };
                // One `CrateScope` part per crate of the world (names.rs). A world that cannot
                // be made names no part: the closing note says why.
                let world = {
                    let _cx = cx.span("names: make the world");
                    crate::names::world(&cx, root, &record.payload, Some(lock), Some(graph.member_dirs()), &facts.fetch_env)
                };
                match world {
                    Ok(names) => {
                        let parts = crate::names::parts(&names);
                        record.parts.extend(parts.iter().map(|(name, source)| Part { name: *name, locate: Locate::Source(*source), place: 0 }));
                        crate::names::tell_public(store, &names);
                        facts.names = Some(names);
                    }
                    Err(error) => {
                        let words = bstd::mem::format_in!(held, "names not resolved: {error}");
                        record.find(crate::fetch::SOURCE_REFUSED, (0, 0), &[words.as_str()]);
                    }
                }
                Ok(Did::Done(record))
            }
            // Its job comes only from `Start`, given when the lock was read this command.
            None => Ok(Did::Failed(TaskFailure { name: task.name, why: Why::Refused { what: "a `UnitGraph` task with no lock read this command", rule: "graph.rs" } })),
        },
        Kind::Fetch => match facts.laid.as_mut().and_then(|laid| laid.lock.as_mut()) {
            Some(lockfile) => crate::fetch::run(&cx, lockfile, &facts.fetch_env, task.name, held),
            // A `Fetch` job comes only from the `UnitGraph` task's asks, which run over the lock
            // read this command.
            None => Ok(Did::Failed(TaskFailure { name: task.name, why: Why::Refused { what: "a `Fetch` task with no lock read this command", rule: "fetch.rs" } })),
        },
        Kind::FileItems | Kind::BodyCheck => unreachable!("{:?} tasks run on the pool, in `answer_batch`", task.name.kind),
        // A file's surface: its out is its source, the digest its file's outline made; it holds
        // no bytes of its own. Its `CrateScope` readers compare that out.
        Kind::Surface => {
            let mut made = Record::new_in(task.name, held);
            made.out = task.source;
            Ok(Did::Done(made))
        }
        Kind::MacroModule => {
            // The lock is read here only when the request did not read it up front.
            if facts.laid.is_none() {
                facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, frame, alloc)?);
            }
            let lock = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref());
            crate::macros::run(&cx, store, lock, &mut facts.macros, facts.fetch_env.home, task, frame, held)
        }
        Kind::CrateScope => {
            if facts.laid.is_none() {
                facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, frame, alloc)?);
            }
            let lock = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref()).map(|lockfile| lockfile.lock);
            let members = crate::names::member_dirs(facts.laid.as_ref());
            crate::names::run(&cx, root, store, &mut facts.names, lock, members, &facts.fetch_env, task, frame, held)
        }
        _ => unreachable!("`builds` said yes to {:?} and `run` has no arm for it", task.name.kind),
    }
}

/// What a `FileItems` task needs, apart from the checkout: all of it `Send`, so the task runs
/// on any worker of the pool.
struct Job<'a> {
    name: Name,
    /// The module slot's path, as the part named it.
    slot: &'a str,
    /// Where the sweep found it (`sweep::Found::flags`).
    flags: u8,
    edition: u8,
    plant: Plant,
}

/// Why a task's job could not be made when the frame of one round's jobs is full: the task
/// fails by these words, the round goes on (rule 20).
const JOBS_FULL: &str = "the frame of one round's jobs is full: its tasks' paths and body lists are more than it holds";

/// The bodies of one file a batch asked for, as one job: the file's job (its name the first
/// body's, unused) and each body's task name with where its item starts (its part's place).
struct BodiesJob<'a> {
    file: Job<'a>,
    bodies: &'a [(Name, u32)],
    /// The crates its bodies' types read (body_types.rs).
    types: &'a crate::body_types::TypesJob<'a>,
}

/// The `FileItems` job of `task`, or the job of a `BodyCheck` task's home file, or the refusal
/// that fails it. Its slot's path is copied into `jobs`, the frame of one round of batches: the
/// job outlives the steps of the batches answered before it joins (the request's file table
/// moves under them), and nothing past the round reads it.
fn job_of<'j>(req: &Request<'_>, facts: &Facts<'_, impl crate::sweep::Stamps>, task: &Task<'_>, jobs: &'j Frame) -> Result<Job<'j>, &'static str> {
    let path = req.file_of(task).ok_or("a file's or a body's task with no file")?;
    let mut held = Vec::new_in(jobs);
    held.try_extend_from_slice(path).map_err(|_| JOBS_FULL)?;
    let slot: &'j [u8] = held.leak();
    let slot = core::str::from_utf8(slot).map_err(|_| "a path is not UTF-8")?;
    Ok(Job { name: task.name, slot, flags: facts.found.flags(slot.as_bytes()), edition: task.edition, plant: facts.plant })
}

/// A file's outline as its `FileItems` record, worked in `scratch` and built in `into`: the
/// record, or why the task fails (an input this build refuses).
fn outline_file<A: Allocator + Copy>(cx: &Cx, root: &Dir<ReadWrite>, job: &Job<'_>, scratch: &Frame, into: A) -> Result<Result<Record<A>, Why>, Unread> {
    let (slot, flags, held) = (job.slot, job.flags, into);
    {
        {
            if flags & crate::sweep::BOTH != 0 {
                // rustc's E0761: the module is at both of its places; nothing of it is read.
                let mut made = Record::new_in(job.name, held);
                let mod_rs = crate::sweep::mod_rs_of(slot, scratch);
                let mod_rs = core::str::from_utf8(&mod_rs).map_err(|_| Unread::Text("a path is not UTF-8"))?;
                made.find(MODULE_AT_BOTH, (0, 0), &[slot, "\x1f", mod_rs]);
                made.out = parts_out(&made.parts);
                return Ok(Ok(made));
            }
            let real = crate::sweep::real_path(slot, flags, scratch);
            let path = core::str::from_utf8(&real).map_err(|_| Unread::Text("a path is not UTF-8"))?;
            if let Plant::PanicIn(at) = job.plant
                && at == path
            {
                panic!("the planted panic in {path}'s task");
            }
            let mut bytes = Bytes::new_in(scratch, "a source file");
            root.read_all(cx, &Path::new(path)?, &mut bytes)?;
            let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| Unread::Text("a source file is not UTF-8"))?;
            if u32::try_from(text.len()).is_err() {
                return Err(Unread::Text("a source file past 4 GiB"));
            }
            let edition = Edition::from_byte(job.edition).ok_or(Unread::Text("a part's edition byte is no edition"))?;
            // blaze-file-items allocates through a one-word handle; the scratch carries no header
            // of its own, so it is erased into one here, on this stack, for this task. What the
            // record keeps is copied into `into` below.
            let erased = blaze_own::Erased::new(scratch);
            let outline = blaze_file_items::outline(text, edition, erased.handle());
            let mut parts = Vec::new_in(held);
            let mut paths = Vec::new_in(held);
            let mut path_bytes = Vec::new_in(held);
            // rustc's directory ownership: a crate root, a `#[path]` file and a `mod.rs` hold
            // their modules beside them; a `mod x;` found at `x.rs` holds them in `x/`.
            let owns_dir = flags & crate::sweep::MODULE == 0 || path == "mod.rs" || path.ends_with("/mod.rs");
            for item in outline.items() {
                let (child, slot) = match item.detail {
                    ItemDetail::Mod { inline: false, path: attr } => {
                        let Some(name) = item.name else { continue };
                        let child = match attr {
                            None => module_file(path, owns_dir, text, &outline, item, Segment::Slot(item.text(text, name)), scratch),
                            Some(attr) => module_file(path, owns_dir, text, &outline, item, Segment::Attr(item.text(text, attr)), scratch),
                        };
                        // A `#[path]` names its file: it alone is opened, with no `x/mod.rs` beside it.
                        (child, attr.is_none())
                    }
                    // An item-position `include!("file")`: the file's items are this crate's, so
                    // the file is a part like a module's, opened as the one file it names. (The
                    // names step reads it; unlisted, its read had no record to stand on:
                    // blaze-diff's `mod rules_gen { include!("rules_gen.rs"); }`, 2026-11-16.)
                    ItemDetail::MacroCall { path: Some(call), input: Some(input) } if in_modules(&outline, item) => {
                        match included_file(path, item.text(text, call), item.text(text, input), scratch) {
                            Some(child) => (child, false),
                            None => continue,
                        }
                    }
                    _ => continue,
                };
                let child = match child {
                    Ok(child) => child,
                    Err(what) => {
                        return Ok(Err(Why::Refused { what, rule: "blaze-cli work.rs, module_file" }));
                    }
                };
                parts.push(Part { name: Name::file(&child), locate: Locate::File { edition: job.edition }, place: item.range.start });
                let at = path_bytes.len() as u32;
                path_bytes.extend_from_slice(&child);
                paths.push((Name::file(&child), at, path_bytes.len() as u32, slot));
            }
            // The file's surface, its own part: what name resolution reads of it, never a body
            // (drive.md, "SPEC, the NAMES step"). A body edit leaves its source, so it stands.
            let surface = outline.surface();
            parts.push(Part { name: Name::new(Kind::Surface, job.name.subject), locate: Locate::Source(surface), place: 0 });
            // Each body its own part, keyed by its item's text: an edit of one body re-parses
            // that one ([`body_syntax`]). Its syntax errors are its record's alone.
            // Its source is its item's text and its file's text around every body
            // (body_types.rs `body_source`): a signature edit in the file re-checks its bodies.
            let mut bodies = Vec::new_in(scratch);
            let mut named = Vec::new_in(scratch);
            for item in outline.items() {
                if let Some(body) = checked_body(&outline, item) {
                    named.push((body_name(job.name, &outline, item, text), item.hash, item.range.start));
                    bodies.push((item.range.start + body.start, item.range.start + body.end));
                }
            }
            bodies.sort_unstable();
            let around = crate::body_types::around_bodies(text, &bodies);
            for (name, hash, place) in named.iter().copied() {
                parts.push(Part { name, locate: Locate::Source(crate::body_types::body_source(hash, around)), place });
            }
            parts.sort_unstable_by_key(|part| part.name);
            let mut findings = Vec::new_in(held);
            let mut messages = Vec::new_in(held);
            for error in outline.errors() {
                if bodies.iter().any(|(start, end)| *start <= error.range.start && error.range.start < *end) {
                    continue;
                }
                let at = messages.len();
                messages.extend_from_slice(outline.message(error).as_bytes());
                findings.push((SYNTAX, error.range.start, error.range.end, at, messages.len()));
            }
            let record = outline
                .to_file_items(text, ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()), erased.handle())
                .map_err(|_| Unread::Text("the outline is not one of its file"))?;
            let payload = Vec::from_slice_in(&record, held);
            let out = parts_out(&parts);
            Ok(Ok(Record { name: job.name, out, parts, findings, messages, payload, paths, path_bytes, reads: Vec::new_in(held) }))
        }
    }
}

/// The domain of a `BodyCheck` part's subject ([`body_name`]).
const BODY_DOMAIN: &str = "blaze-cli body check name 1";

/// Whether `item` is a body's own entry: a `fn`, `const` or `static` with a body, under none:
/// an item written inside a body is parsed with it, never twice. Its body range, entry-relative.
fn checked_body(outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item) -> Option<blaze_surface::Range> {
    let holds = |kind: ItemKind| matches!(kind, ItemKind::Function | ItemKind::Const | ItemKind::Static);
    if !holds(item.kind()) {
        return None;
    }
    let body = item.body?;
    let mut parent = item.parent;
    while let Some(at) = parent {
        let up = outline.item(at);
        if holds(up.kind()) {
            return None;
        }
        parent = up.parent;
    }
    Some(body)
}

/// A body's name: its file's subject and its entry's chain up to the file (each kind, name as
/// written and disambiguator), so it stands while other items move and its own text changes.
fn body_name(file: Name, outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item, text: &str) -> Name {
    let mut h = blaze_ids::Hasher::new(BODY_DOMAIN);
    h.update(&file.subject.0);
    let mut at = Some(item);
    while let Some(entry) = at {
        let name = entry.name.map_or("", |range| entry.text(text, range));
        h.update(&[entry.kind() as u8]).update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes()).update(&entry.disambiguator.to_le_bytes());
        at = entry.parent.map(|up| outline.item(up));
    }
    Name::new(Kind::BodyCheck, h.finish())
}

/// The domain of an item's chain digest ([`item_chain`]).
const CHAIN_DOMAIN: &str = "blaze-cli item chain 1";

/// An item's identity in its file that no edit elsewhere moves (rule 71): the digest of its
/// chain up to the file (each kind, name as written and disambiguator), as [`body_name`] names
/// a body. A record that places a finding in an item holds this and the range from the item's
/// start; the printer finds the item's start in the file as it is today ([`item_by_chain`]).
pub(crate) fn item_chain(outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item, text: &str) -> u64 {
    let mut h = blaze_ids::Hasher::new(CHAIN_DOMAIN);
    let mut at = Some(item);
    while let Some(entry) = at {
        let name = entry.name.map_or("", |range| entry.text(text, range));
        h.update(&[entry.kind() as u8]).update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes()).update(&entry.disambiguator.to_le_bytes());
        at = entry.parent.map(|up| outline.item(up));
    }
    let digest = h.finish();
    u64::from_le_bytes(digest.0[..8].try_into().expect("a digest holds 8 bytes"))
}

/// The item of `outline` whose [`item_chain`] is `chain`: its start in the file today.
pub(crate) fn item_by_chain(outline: &blaze_file_items::Outline<'_>, text: &str, chain: u64) -> Option<u32> {
    outline.items().iter().find(|item| item_chain(outline, item, text) == chain).map(|item| item.range.start)
}

/// The innermost item of `outline` whose range holds `offset`.
pub(crate) fn item_holding<'o>(outline: &'o blaze_file_items::Outline<'_>, offset: u32) -> Option<&'o blaze_surface::Item> {
    outline.items().iter().filter(|item| item.range.start <= offset && offset < item.range.end).min_by_key(|item| item.range.end - item.range.start)
}

/// The parser's edition of a part's edition byte.
fn parser_edition(edition: Edition) -> parser::Edition {
    match edition {
        Edition::E2015 => parser::Edition::Edition2015,
        Edition::E2018 => parser::Edition::Edition2018,
        Edition::E2021 => parser::Edition::Edition2021,
        Edition::E2024 => parser::Edition::Edition2024,
    }
}

/// A file's bodies are one job ([`BodiesJob`]): the file is read and outlined once, here, and
/// each body asked for is found by its place and parsed ([`body_syntax`]), its outcome pushed
/// to `did` in the order asked. A body's panic fails that body alone. An `Err` is the file's
/// (unread, not text): `pooled_bodies` gives it to every body not yet answered.
fn bodies_syntax(cx: &Cx, root: &Dir<ReadWrite>, job: &BodiesJob<'_>, scratch: &Frame, did: &mut BodiesDid) -> Result<(), Unread> {
    let real = crate::sweep::real_path(job.file.slot, job.file.flags, scratch);
    let path = core::str::from_utf8(&real).map_err(|_| Unread::Text("a path is not UTF-8"))?;
    let mut bytes = Bytes::new_in(scratch, "a source file");
    root.read_all(cx, &Path::new(path)?, &mut bytes)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| Unread::Text("a source file is not UTF-8"))?;
    if u32::try_from(text.len()).is_err() {
        return Err(Unread::Text("a source file past 4 GiB"));
    }
    let edition = Edition::from_byte(job.file.edition).ok_or(Unread::Text("a body's home file has no edition byte"))?;
    let erased = blaze_own::Erased::new(scratch);
    let outline = blaze_file_items::outline(text, edition, erased.handle());
    // The items that have a checked body, by where each starts: a body asked for is found by
    // search, not by a pass over the file's items a body.
    let mut starts: Vec<(u32, u32), &Frame> = Vec::new_in(scratch);
    for (index, item) in outline.items().iter().enumerate() {
        if checked_body(&outline, item).is_some() {
            starts.push((item.range.start, index as u32));
        }
    }
    starts.sort_unstable();
    // The bodies' types (body_types.rs): each owner's front end over records, made once a job.
    let mut types = crate::body_types::FileTypes::new(cx, root, job.types, text, scratch);
    for (name, place) in job.bodies.iter().copied() {
        let found = starts.binary_search_by_key(&place, |(start, _)| *start).ok().map(|at| &outline.items()[starts[at].1 as usize]);
        let (used, limit) = (scratch.peak() as u64, scratch.reserved() as u64);
        let checked = unwinding(|| {
            let mut done = body_syntax(&outline, found, text, edition, name, Global)?;
            // A body with a syntax error is that error's: its types are not asked.
            if let Some(item) = found
                && done.findings.is_empty()
            {
                types.check(&outline, item, text, &mut done);
            }
            Ok(done)
        });
        did.push(Some(match checked {
            Ok(done) => done,
            Err(Panicked) => Err(Why::Panicked { used, limit }),
        }));
    }
    Ok(())
}

/// A body's syntax as its `BodyCheck` record, named `name`: `item` is its entry, the item at
/// its place in its file's outline (`None`: the file holds no body there), its text parsed by
/// the entry blaze-lower's `lower_body` parses a body with (`Reparser::top_level_item().parse`),
/// each lexer and parser error inside the body a [`SYNTAX`] finding relative to the entry
/// (rule 71). Errors before the body are the file's outline's. Its out is the entry's text
/// digest: what it says follows from that text.
fn body_syntax<A: Allocator + Copy>(outline: &blaze_file_items::Outline<'_>, item: Option<&blaze_surface::Item>, text: &str, edition: Edition, name: Name, into: A) -> Result<Record<A>, Why> {
    let Some((item, body)) = item.and_then(|item| Some((item, checked_body(outline, item)?))) else {
        return Err(Why::Refused { what: "a body's part whose file holds no body at its place", rule: "blaze-cli work.rs, body_syntax" });
    };
    let entry = item.range.of(text);
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, entry);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let mut made = Record::new_in(name, into);
    for (token, message) in lexed.errors() {
        let range = lexed.text_range(token);
        if range.start as u32 >= body.start {
            made.find(SYNTAX, (range.start as u32, range.end as u32), &[message]);
        }
    }
    // The parser's steps over the lexer's tokens, trivia skipped: an error is placed at the end
    // of the last token taken before it (blaze-file-items walk.rs reads the outline's so).
    // An error within one token of the one before is the parser's recovery from it (`let x = ;`
    // is "expected expression", then "expected SEMICOLON" past the `;`): one mistake, one error.
    const CASCADE: u32 = 1;
    let (mut next, mut end) = (0usize, 0usize);
    let mut since: Option<u32> = None;
    let mut take = |next: &mut usize, end: &mut usize, since: &mut Option<u32>| {
        while *next < lexed.len() && lexed.kind(*next).is_trivia() {
            *next += 1;
        }
        if *next < lexed.len() {
            *end = lexed.text_range(*next).end;
            *next += 1;
            *since = since.map(|n| n.saturating_add(1));
        }
    };
    for step in output.iter() {
        match step {
            parser::Step::Token { n_input_tokens, .. } => (0..n_input_tokens).for_each(|_| take(&mut next, &mut end, &mut since)),
            parser::Step::FloatSplit { .. } => take(&mut next, &mut end, &mut since),
            parser::Step::Enter { .. } | parser::Step::Exit => {}
            parser::Step::Error { msg } => {
                let cascade = since.is_some_and(|n| n <= CASCADE);
                since = Some(0);
                if end as u32 >= body.start && !cascade {
                    made.find(SYNTAX, (end as u32, end as u32), &[msg]);
                }
            }
        }
    }
    made.out = item.hash;
    Ok(made)
}


/// Why an outline did not come: a file it could not take as text (its task fails), or a path
/// or read error, which stops the command (`CliError::Path`, `CliError::Fs`). `Send`, unlike
/// `CliError`, so it crosses back from a pool worker.
pub(crate) enum Unread {
    Text(&'static str),
    Path(bstd::path::PathError),
    Fs(bstd::fs::FsError),
}

impl From<Unread> for CliError {
    fn from(error: Unread) -> CliError {
        match error {
            Unread::Path(error) => CliError::Path(error),
            Unread::Fs(error) => CliError::Fs(error),
            Unread::Text(what) => CliError::Text { what },
        }
    }
}

/// The slots of every `Effect::Open` asked so far, probed together on the pool (a stat or two
/// and, for a file whose stamp does not prove it, a read and a hash each: 33 ms for tempo's
/// 655 on one thread), each in its own frame; their results in the order asked.
pub fn open_all<'p>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &(impl crate::sweep::Stamps + Sync),
    workers: u32,
    task_frame: usize,
    opens: &'p [(Name, &'p [u8], bool)],
) -> Result<std::vec::Vec<Result<crate::sweep::Probed, CliError>, Global>, CliError> {
    let cx = cx.span("opens");
    let scope = cx.scope();
    if let Some(most) = core::num::NonZeroU32::new(workers) {
        scope.narrow_workers(most, bstd::task::Class::Foreground);
    }
    let mut pending = std::vec::Vec::with_capacity_in(opens.len(), Global);
    for (_, path, module) in opens.iter() {
        pending.push(match core::str::from_utf8(path) {
            Ok(slot) => Ok(scope.may_overlap("open", task_frame, move |cx: &Cx| {
                Ok::<_, core::convert::Infallible>(crate::sweep::probe_slot(cx, root, stamps, slot, *module, cx.frame()))
            })),
            Err(_) => Err(CliError::Text { what: "a path a record names is not UTF-8" }),
        });
    }
    let mut probed = std::vec::Vec::with_capacity_in(opens.len(), Global);
    let mut failed = None;
    for pending in pending {
        match pending {
            Ok(handle) => match handle.join(&cx) {
                Ok(done) => probed.push(done.map_err(CliError::from)),
                Err(error) => failed = failed.or(Some(CliError::Pool(error))),
            },
            Err(error) => probed.push(Err(error)),
        }
    }
    scope.join(&cx).map_err(CliError::Pool)?;
    match failed {
        Some(error) => Err(error),
        None => Ok(probed),
    }
}

impl From<bstd::path::PathError> for Unread {
    fn from(error: bstd::path::PathError) -> Unread {
        Unread::Path(error)
    }
}

impl From<bstd::fs::FsError> for Unread {
    fn from(error: bstd::fs::FsError) -> Unread {
        Unread::Fs(error)
    }
}

/// A `FileItems` task's outcome on the pool: its record in `Global` (copied into the batch's
/// frame once joined), or why it failed; its frame's peak; its time.
struct Pooled {
    did: Result<Result<Record<Global>, Why>, Unread>,
    used: usize,
    ns: u64,
}

/// `job` on a pool worker, in the worker's task frame (`cx.frame()`, sized as the serial
/// scratch). Rule 20: a panic, or a refusal of its frame (the alloc-error hook makes one a
/// panic), fails this task alone; the pool never sees it, so the scope is not cancelled.
fn pooled(cx: &Cx, root: &Dir<ReadWrite>, job: &Job<'_>, clock: &bstd::clock::Clock) -> Pooled {
    let began = clock.now();
    let scratch = cx.frame();
    let ran = unwinding(|| outline_file(cx, root, job, scratch, Global));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    let did = match ran {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(Unread::Text(what))) => Ok(Err(Why::Refused { what, rule: "blaze-cli work.rs" })),
        Ok(Err(error)) => Err(error),
        Err(Panicked) => Ok(Err(Why::Panicked { used: used as u64, limit: limit as u64 })),
    };
    Pooled { did, used, ns: clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)) }
}

/// Each body's outcome of a file's job, in the order asked; `None` once taken by its task.
type BodiesDid = Vec<Option<Result<Record<Global>, Why>>, Global>;

/// A file's body job's outcome on the pool: each body's record or why it failed; what stopped
/// the command, if the file could not be read at all (a path or read error); the frame's peak;
/// the job's time.
struct PooledBodies {
    did: BodiesDid,
    stopped: Option<Unread>,
    used: usize,
    ns: u64,
}

/// `job` on a pool worker, in the worker's task frame, as [`pooled`] runs a file's outline: a
/// file that is not text, or a panic outside any one body (the read, the outline), fails each
/// body not yet answered, by name; the pool never sees it.
fn pooled_bodies(cx: &Cx, root: &Dir<ReadWrite>, job: &BodiesJob<'_>, clock: &bstd::clock::Clock) -> PooledBodies {
    let began = clock.now();
    let scratch = cx.frame();
    let mut did: BodiesDid = Vec::with_capacity_in(job.bodies.len(), Global);
    let ran = unwinding(|| bodies_syntax(cx, root, job, scratch, &mut did));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    let (rest, stopped) = match ran {
        Ok(Ok(())) => (None, None),
        Ok(Err(Unread::Text(what))) => (Some(Why::Refused { what, rule: "blaze-cli work.rs" }), None),
        Ok(Err(error)) => (None, Some(error)),
        Err(Panicked) => (Some(Why::Panicked { used: used as u64, limit: limit as u64 }), None),
    };
    if let Some(why) = rest {
        while did.len() < job.bodies.len() {
            did.push(Some(Err(why)));
        }
    }
    PooledBodies { did, stopped, used, ns: clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)) }
}

/// What a reader of a parts list sees: the parts' names, how each is found and where.
pub(crate) fn parts_out(parts: &[Part]) -> ContentId {
    let mut h = blaze_ids::Hasher::new(OUT_DOMAIN);
    for part in parts {
        h.update(&[u8::try_from(part.name.kind.index()).unwrap_or(u8::MAX)]).update(&part.name.subject.0).update(&part.place.to_le_bytes());
        match part.locate {
            Locate::Source(id) => h.update(&[0]).update(&id.0),
            Locate::File { edition } => h.update(&[1, edition]),
            Locate::Key(key) => h.update(&[2]).update(&key.0.0),
        };
    }
    h.finish()
}

/// What a module adds to the path: its name (a slot `name.rs`, or the directory `name` for an
/// inline module), or its `#[path = ".."]`'s literal as written.
#[derive(Clone, Copy)]
enum Segment<'t> {
    Slot(&'t str),
    Attr(&'t str),
}

/// The file `mod name;` names, written in `item` of the file at `path` (rustc's rule): a file
/// that owns its directory (`owns_dir`: a crate root, a `#[path]` file, a `mod.rs`) holds its
/// modules beside it, any other in the directory named for it; each enclosing inline `mod` adds
/// its name, or its `#[path]`. A `#[path]` on `mod x;` itself names its file relative to the
/// file's own directory when no inline `mod` encloses it, else relative to the directory the
/// enclosing modules make. A slot answers `name.rs`: the drive opens it as a module
/// (`PathOf { module: true }`), and the sweep probes `name.rs` and `name/mod.rs` (sweep.rs); a
/// `#[path]` answers the one file it names. The error says what is refused.
fn module_file<'f, 't>(
    path: &str,
    owns_dir: bool,
    text: &'t str,
    outline: &blaze_file_items::Outline<'_>,
    item: &blaze_surface::Item,
    segment: Segment<'t>,
    frame: &'f Frame,
) -> Result<Vec<u8, &'f Frame>, &'static str> {
    let (dir, stem) = match path.rfind('/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => ("", path),
    };
    // The enclosing inline modules, innermost first.
    let mut chain: Vec<Segment<'t>, &Frame> = Vec::new_in(frame);
    let mut parent = item.parent;
    while let Some(at) = parent {
        let entry = outline.item(at);
        let ItemDetail::Mod { inline: true, path: attr } = entry.detail else {
            return Err("a `mod x;` inside a block that is no module");
        };
        chain.push(match (attr, entry.name) {
            (Some(attr), _) => Segment::Attr(entry.text(text, attr)),
            (None, Some(name)) => Segment::Slot(entry.text(text, name)),
            (None, None) => return Err("an inline `mod` with no name"),
        });
        parent = entry.parent;
    }
    let mut out = Vec::new_in(frame);
    out.extend_from_slice(dir.as_bytes());
    let file_relative = chain.is_empty() && matches!(segment, Segment::Attr(_));
    if !owns_dir && !file_relative {
        push_segment(&mut out, stem.strip_suffix(".rs").unwrap_or(stem));
    }
    for segment in chain.iter().rev() {
        match *segment {
            Segment::Slot(name) => push_segment(&mut out, name),
            Segment::Attr(literal) => join(&mut out, &path_literal(literal, frame)?)?,
        }
    }
    match segment {
        Segment::Slot(name) => {
            push_segment(&mut out, name);
            out.extend_from_slice(b".rs");
        }
        Segment::Attr(literal) => join(&mut out, &path_literal(literal, frame)?)?,
    }
    Ok(out)
}

/// Whether every entry enclosing `item` is an inline module: it is in item position of the
/// file, not inside a body, an impl or a trait.
fn in_modules(outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item) -> bool {
    let mut parent = item.parent;
    while let Some(at) = parent {
        let entry = outline.item(at);
        if !matches!(entry.detail, ItemDetail::Mod { inline: true, .. }) {
            return false;
        }
        parent = entry.parent;
    }
    true
}

/// The file a macro call `call ! input` in item position of the file at `path` includes: `Some`
/// for `include!`, `core::include!` and `std::include!` whose input is one string literal (the
/// Reference, "Macros", and core's `include`: "the file is located relative to the current
/// file"), so relative to `path`'s directory whatever inline modules enclose the call. `None`
/// for any other macro, and for an `include!` whose input is no literal (`concat!(env!(..))`:
/// only expansion knows that path; a checkout file it names stops the command by name, names.rs
/// `Unlisted`). Debt: the part is opened as a `#[path]` file is, so a `mod x;` line INSIDE an
/// included file is looked for beside it, where rustc looks from the including module's
/// directory; the names step's read of the other place is then `Unlisted`, never quiet.
fn included_file<'f>(path: &str, call: &str, input: &str, frame: &'f Frame) -> Option<Result<Vec<u8, &'f Frame>, &'static str>> {
    let call = call.trim();
    let call = call.strip_prefix("::").unwrap_or(call);
    if !matches!(call, "include" | "core::include" | "std::include") {
        return None;
    }
    let input = input.trim();
    let inner = [('(', ')'), ('[', ']'), ('{', '}')].iter().find_map(|(open, close)| input.strip_prefix(*open)?.strip_suffix(*close));
    let literal = inner.unwrap_or(input).trim();
    let literal = literal.strip_suffix(',').unwrap_or(literal).trim_end();
    if !(literal.starts_with('"') || literal.starts_with("r\"") || literal.starts_with("r#")) {
        return None;
    }
    const REFUSED: &str = "an `include!` whose literal blaze does not read: an escape past `\\n`, an absolute path, or one that climbs out of the checkout";
    let dir = path.rfind('/').map_or("", |at| &path[..at]);
    let mut out = Vec::new_in(frame);
    out.extend_from_slice(dir.as_bytes());
    Some(path_literal(literal, frame).and_then(|rel| join(&mut out, &rel)).map(|()| out).map_err(|_| REFUSED))
}

/// A `#[path]` string literal's value: `"..."` with its escapes, or a raw `r#"..."#`.
fn path_literal<'f>(literal: &str, frame: &'f Frame) -> Result<Vec<u8, &'f Frame>, &'static str> {
    let mut out = Vec::new_in(frame);
    if let Some(raw) = literal.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let body = raw[hashes..].strip_prefix('"').ok_or("a `#[path]` that is no string")?;
        let body = body.strip_suffix(&raw[..hashes]).and_then(|b| b.strip_suffix('"')).ok_or("a `#[path]` that is no string")?;
        out.extend_from_slice(body.as_bytes());
        return Ok(out);
    }
    let body = literal.strip_prefix('"').and_then(|b| b.strip_suffix('"')).ok_or("a `#[path]` that is no string")?;
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        let c = match c {
            '\\' => match chars.next() {
                Some('\\') => '\\',
                Some('"') => '"',
                Some('\'') => '\'',
                Some('n') => '\n',
                Some('r') => '\r',
                Some('t') => '\t',
                Some('0') => '\0',
                _ => return Err("a `#[path]` with a `\\x`, `\\u` or line-continuing escape, which blaze does not read yet"),
            },
            c => c,
        };
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
    Ok(out)
}

/// `rel` joined onto the checkout-relative directory `out`, `.` and `..` resolved: a path that
/// is absolute or climbs out of the checkout is refused (it would read outside what is built).
fn join(out: &mut Vec<u8, &Frame>, rel: &[u8]) -> Result<(), &'static str> {
    if rel.first() == Some(&b'/') {
        return Err("an absolute `#[path]`: a module's file is in the checkout");
    }
    for part in rel.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                if out.is_empty() {
                    return Err("a `#[path]` that climbs out of the checkout");
                }
                let cut = out.iter().rposition(|b| *b == b'/').unwrap_or(0);
                out.truncate(cut);
            }
            part => {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(part);
            }
        }
    }
    Ok(())
}

fn push_segment(out: &mut Vec<u8, &Frame>, segment: &str) {
    if !out.is_empty() {
        out.push(b'/');
    }
    out.extend_from_slice(segment.strip_prefix("r#").unwrap_or(segment).as_bytes());
}

#[cfg(test)]
mod tests {
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    /// Each `mod x;` of `text`, at `path`, to the file it names or the refusal.
    fn named(path: &str, owns_dir: bool, text: &str) -> std::vec::Vec<Result<std::string::String, &'static str>> {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let outline = blaze_file_items::outline(text, Edition::E2024, erased.handle());
        let mut out = std::vec::Vec::new();
        for item in outline.items() {
            let ItemDetail::Mod { inline: false, path: attr } = item.detail else { continue };
            let name = item.name.expect("a named mod");
            let segment = match attr {
                None => Segment::Slot(item.text(text, name)),
                Some(attr) => Segment::Attr(item.text(text, attr)),
            };
            let file = module_file(path, owns_dir, text, &outline, item, segment, frame);
            out.push(file.map(|file| std::string::String::from_utf8(file.as_slice().to_vec()).expect("UTF-8")));
        }
        out
    }

    /// rustc's module paths (the Reference, "The path attribute"): a crate root and a `mod.rs`
    /// hold their modules beside them, an `x.rs` module in `x/`; inline modules add their name or
    /// their `#[path]`; a `#[path]` outside any inline module is relative to its file's directory;
    /// a raw or escaped literal reads as its value; and a path out of the checkout is refused.
    #[test]
    fn mod_and_path_attributes_name_the_files_rustc_reads() {
        let root = "mod a;\n#[path = \"b_file.rs\"] mod b;\nmod inl { mod c; #[path = \"d.rs\"] mod d; }\n\
                    #[path = \"x\"] mod px { mod e; }\n#[path = r\"../top.rs\"] mod f;\n#[path = \"sub\\\\g.rs\"] mod g;\n";
        let found = named("src/lib.rs", true, root);
        let want: [Result<&str, &str>; 7] =
            [Ok("src/a.rs"), Ok("src/b_file.rs"), Ok("src/inl/c.rs"), Ok("src/inl/d.rs"), Ok("src/x/e.rs"), Ok("top.rs"), Ok("src/sub\\g.rs")];
        assert_eq!(found.iter().map(|r| r.as_deref().map_err(|e| *e)).collect::<std::vec::Vec<_>>(), want);

        let module = "mod a;\n#[path = \"p.rs\"] mod b;\nmod i { #[path = \"q.rs\"] mod c; mod d; }\n";
        let found = named("src/x.rs", false, module);
        let want: [Result<&str, &str>; 4] = [Ok("src/x/a.rs"), Ok("src/p.rs"), Ok("src/x/i/q.rs"), Ok("src/x/i/d.rs")];
        assert_eq!(found.iter().map(|r| r.as_deref().map_err(|e| *e)).collect::<std::vec::Vec<_>>(), want);

        let found = named("src/y/mod.rs", true, "mod a;\n");
        assert_eq!(found.iter().map(|r| r.as_deref().map_err(|e| *e)).collect::<std::vec::Vec<_>>(), [Ok("src/y/a.rs")]);

        let found = named("src/lib.rs", true, "#[path = \"../../z.rs\"] mod z;\n#[path = \"/etc/z.rs\"] mod w;\n");
        assert!(found[0].as_ref().is_err_and(|e| e.contains("climbs out")), "{found:?}");
        assert!(found[1].as_ref().is_err_and(|e| e.contains("absolute")), "{found:?}");
    }
}
