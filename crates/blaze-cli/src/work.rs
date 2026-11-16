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
}

pub(crate) enum Did<'f> {
    Done(Made<'f>),
    /// What the task lacks, in the batch's frame: the machine enters it again from the top with
    /// them as its inputs (`Ran::Need`), each read with [`payload`].
    Need(Vec<Want, &'f Frame>),
    Failed(TaskFailure),
    /// Failed, with words of its own the closing lines print (a fetch's network failure).
    FailedSaying(TaskFailure, &'f str),
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
}

impl Sizes {
    /// What a command runs with: a task's frame for each.
    pub const USUAL: Sizes = Sizes { scratch: TASK_FRAME, batch: TASK_FRAME, plant: Plant::None };

    /// The frames, reserved: no memory is taken until used.
    pub fn frames(self) -> Result<Frames, CliError> {
        Ok(Frames {
            scratch: Frame::with_reserve(self.scratch, Tag::Other).map_err(|_| CliError::Text { what: "a task's scratch frame could not be reserved" })?,
            batch: Frame::with_reserve(self.batch, Tag::Other).map_err(|_| CliError::Text { what: "a batch's frame could not be reserved" })?,
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
    let Frames { scratch, batch: held } = frames;
    // Every batch's `FileItems` tasks go to the pool at once, each in its own frame, so the
    // pool is as wide as every batch the request has handed out (it hands out one a worker);
    // then each batch is answered in turn: its other tasks run here, its pooled ones joined.
    let clock = phases.clock().clone();
    let scope = cx.scope();
    if let Some(most) = core::num::NonZeroU32::new(facts.workers) {
        scope.narrow_workers(most, bstd::task::Class::Foreground);
    }
    let task_frame = scratch.reserved();
    let mut pending = std::vec::Vec::with_capacity_in(batches.len(), Global);
    for batch in batches.iter() {
        let mut of_batch = std::vec::Vec::with_capacity_in(batch.tasks.len(), Global);
        for (_, task) in batch.tasks.iter() {
            of_batch.push(match task.name.kind {
                Kind::FileItems | Kind::BodyCheck => Some(match job_of(req, facts, task, frame) {
                    Ok(job) => Ok(scope.may_overlap("outline", task_frame, {
                        let clock = &clock;
                        move |cx: &Cx| Ok::<_, core::convert::Infallible>(pooled(cx, root, &job, clock))
                    })),
                    Err(what) => Err(what),
                }),
                _ => None,
            });
        }
        pending.push(of_batch);
    }
    let mut answered = Ok(());
    for (batch, of_batch) in batches.into_iter().zip(pending) {
        if answered.is_err() {
            // A command stopping: what is still pooled is joined with the scope, unread.
            continue;
        }
        answered = answer_batch(&cx, root, store, facts, req, batch, of_batch, frame, held, scratch, failed, high, alloc, effects, phases);
        high.batch = high.batch.max(held.peak());
        held.reset();
    }
    let joined = scope.join(&cx).map_err(CliError::Pool);
    // The batches' steps are counted as steps, not twice as runs.
    let stepped = phases.request.steps - steps_before;
    phases.request.runs += phases.since(began).saturating_sub(stepped);
    answered.and(joined)
}

/// A batch's tasks as [`run_batches`] spawned them: a `FileItems` task's pool handle (or why its
/// job could not be made), `None` for a task run here.
type Pending<'s> = std::vec::Vec<Option<Result<bstd::task::Task<'s, Pooled>, &'static str>>, Global>;

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
    pending: Pending<'_>,
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
    for ((_, task), pending) in batch.tasks.iter().zip(pending.iter()) {
        if pending.is_some() {
            ran_here.push(None);
            continue;
        }
        let at = phases.now();
        let done = run_one(&cx, root, store, facts, task, frame, held, scratch, high, alloc)?;
        ran_here.push(Some((done, phases.since(at))));
    }
    for (((ticket, task), pending), here) in batch.tasks.iter().zip(pending).zip(ran_here) {
        let (done, ns) = match (pending, here) {
            (_, Some(done)) => done,
            (Some(Err(what)), _) => (Did::Failed(TaskFailure { name: task.name, why: Why::Refused { what, rule: "blaze-cli work.rs" } }), 0),
            (Some(Ok(handle)), _) => {
                let joined = handle.join(&cx).map_err(CliError::Pool)?;
                high.scratch = high.scratch.max(joined.used);
                let did = joined.did.map_err(|error| match error {
                    Unread::Path(error) => CliError::Path(error),
                    Unread::Fs(error) => CliError::Fs(error),
                    Unread::Text(what) => CliError::Text { what },
                })?;
                let done = match did {
                    Ok(record) => Did::Done(record.clone_in(held)),
                    Err(why) => Did::Failed(TaskFailure { name: task.name, why }),
                };
                (done, joined.ns)
            }
            (None, None) => unreachable!("a task neither pooled nor run here"),
        };
        if let Did::Failed(failure) | Did::FailedSaying(failure, _) = &done {
            let path = match req.file_of(task) {
                Some(path) => Some(core::str::from_utf8(Vec::from_slice_in(path, frame).leak()).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?),
                None => None,
            };
            let detail = match &done {
                Did::FailedSaying(_, words) => Some(held_str(words, frame)),
                _ => None,
            };
            failed.push(Failed { path, failure: *failure, detail });
        }
        did.push((*ticket, done));
        took.push(ns);
    }
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
            Did::Failed(failure) | Did::FailedSaying(failure, _) => Ran::Failed(*failure),
        };
        results.push(Outcome { ticket: *ticket, ran, in_place: 0, ns: *ns });
    }
    let stepped = phases.now();
    req.step(Event::Ran { batch: batch.id, results: &results }, effects)?;
    phases.request.steps += phases.since(stepped);
    Ok(())
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
    unwind_refusals();
    let ran = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| run(cx, root, store, facts, task, frame, held, scratch, alloc)));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    high.scratch = high.scratch.max(used);
    scratch.reset();
    let failed = |why| Ok(Did::Failed(TaskFailure { name: task.name, why }));
    match ran {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(CliError::Text { what })) => failed(Why::Refused { what, rule: "blaze-cli work.rs" }),
        Ok(Err(error)) => Err(error),
        Err(_) => failed(Why::Panicked { used: used as u64, limit: limit as u64 }),
    }
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
        Kind::BodyCheck => "check the bodies' syntax",
        _ => kind.stage(),
    }
}

/// The one list of the stages `check` is defined to run, in order, and whether this build runs
/// each, derived from the kinds this worker builds: the closing line's "NOT checked" and
/// `blaze explain 3` read it. When all are `true`, `check` can say `ok`.
pub(crate) const CHECK_STAGES: [(&str, bool); 6] = [
    ("manifests", builds(Kind::Units) && builds(Kind::UnitGraph)),
    ("syntax of items", builds(Kind::FileItems)),
    ("syntax inside bodies", builds(Kind::BodyCheck)),
    ("names", builds(Kind::CrateScope)),
    // A body's types are its `BodyCheck` task's, an item's its `Signature`'s.
    ("types", whole(Kind::Signature) && whole(Kind::ImplBucket) && whole(Kind::BodyCheck)),
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
                    crate::names::world(&cx, root, &record.payload, Some(lock), &facts.fetch_env)
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
            Some(lockfile) => crate::fetch::run(&cx, root, lockfile, &facts.fetch_env, task.name, held),
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
            crate::names::run(&cx, root, store, &mut facts.names, lock, &facts.fetch_env, task, frame, held)
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
    /// A `BodyCheck` job's item: where it starts in the file (its part's place).
    body: Option<u32>,
}

/// The `FileItems` or `BodyCheck` job of `task`, or the refusal that fails it. Its slot's path
/// (a body's home file's) is copied into the request's `frame`: the job outlives the steps of
/// the batches answered before it joins.
fn job_of<'f>(req: &Request<'_>, facts: &Facts<'_, impl crate::sweep::Stamps>, task: &Task<'_>, frame: &'f Frame) -> Result<Job<'f>, &'static str> {
    let path = req.file_of(task).ok_or("a file's or a body's task with no file")?;
    let slot: &'f [u8] = Vec::from_slice_in(path, frame).leak();
    let slot = core::str::from_utf8(slot).map_err(|_| "a path is not UTF-8")?;
    let body = (task.name.kind == Kind::BodyCheck).then_some(task.place);
    Ok(Job { name: task.name, slot, flags: facts.found.flags(slot.as_bytes()), edition: task.edition, plant: facts.plant, body })
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
                let ItemDetail::Mod { inline: false, path: attr } = item.detail else { continue };
                let Some(name) = item.name else { continue };
                let child = match attr {
                    None => module_file(path, owns_dir, text, &outline, item, Segment::Slot(item.text(text, name)), scratch),
                    Some(attr) => module_file(path, owns_dir, text, &outline, item, Segment::Attr(item.text(text, attr)), scratch),
                };
                let child = match child {
                    Ok(child) => child,
                    Err(what) => {
                        return Ok(Err(Why::Refused { what, rule: "blaze-cli work.rs, module_file" }));
                    }
                };
                // A `#[path]` names its file: it alone is opened, with no `x/mod.rs` beside it.
                let slot = attr.is_none();
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
            let mut bodies = Vec::new_in(scratch);
            for item in outline.items() {
                if let Some(body) = checked_body(&outline, item) {
                    let name = body_name(job.name, &outline, item, text);
                    parts.push(Part { name, locate: Locate::Source(item.hash), place: item.range.start });
                    bodies.push((item.range.start + body.start, item.range.start + body.end));
                }
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

/// The parser's edition of a part's edition byte.
fn parser_edition(edition: Edition) -> parser::Edition {
    match edition {
        Edition::E2015 => parser::Edition::Edition2015,
        Edition::E2018 => parser::Edition::Edition2018,
        Edition::E2021 => parser::Edition::Edition2021,
        Edition::E2024 => parser::Edition::Edition2024,
    }
}

/// A body's syntax as its `BodyCheck` record: its entry found at `place` in its file's outline,
/// its text parsed by the entry blaze-lower's `lower_body` parses a body with
/// (`Reparser::top_level_item().parse`), each lexer and parser error inside the body a
/// [`SYNTAX`] finding relative to the entry (rule 71). Errors before the body are the file's
/// outline's. Its out is the entry's text digest: what it says follows from that text.
fn body_syntax<A: Allocator + Copy>(cx: &Cx, root: &Dir<ReadWrite>, job: &Job<'_>, place: u32, scratch: &Frame, into: A) -> Result<Result<Record<A>, Why>, Unread> {
    let real = crate::sweep::real_path(job.slot, job.flags, scratch);
    let path = core::str::from_utf8(&real).map_err(|_| Unread::Text("a path is not UTF-8"))?;
    let mut bytes = Bytes::new_in(scratch, "a source file");
    root.read_all(cx, &Path::new(path)?, &mut bytes)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| Unread::Text("a source file is not UTF-8"))?;
    if u32::try_from(text.len()).is_err() {
        return Err(Unread::Text("a source file past 4 GiB"));
    }
    let edition = Edition::from_byte(job.edition).ok_or(Unread::Text("a body's home file has no edition byte"))?;
    let erased = blaze_own::Erased::new(scratch);
    let outline = blaze_file_items::outline(text, edition, erased.handle());
    let found = outline.items().iter().find(|item| item.range.start == place && checked_body(&outline, item).is_some());
    let Some((item, body)) = found.and_then(|item| Some((item, checked_body(&outline, item)?))) else {
        return Ok(Err(Why::Refused { what: "a body's part whose file holds no body at its place", rule: "blaze-cli work.rs, body_syntax" }));
    };
    let entry = item.range.of(text);
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, entry);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let mut made = Record::new_in(job.name, into);
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
    Ok(Ok(made))
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
    let ran = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| match job.body {
        None => outline_file(cx, root, job, scratch, Global),
        Some(place) => body_syntax(cx, root, job, place, scratch, Global),
    }));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    let did = match ran {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(Unread::Text(what))) => Ok(Err(Why::Refused { what, rule: "blaze-cli work.rs" })),
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(Err(Why::Panicked { used: used as u64, limit: limit as u64 })),
    };
    Pooled { did, used, ns: clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)) }
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
