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

/// The frame of one file's body job on the pool: its owners' front end over records (the world,
/// the readers) and its bodies' checks, a size of its own like a crate scope's
/// (`names::SCOPE_FRAME`), never a task's scratch (`Sizes::scratch`, which a test makes as
/// small as one outline: the body job then refused its world, draw_tests 2026-11-16).
const BODY_FRAME: usize = TASK_FRAME;

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
        // A file read that is not the swept text: never filed under the swept id (rule 21).
        Why::Panicked { .. } | Why::Changed => false,
        Why::Waited { .. } => true,
        Why::NotBuilt | Why::Refused { .. } => failure.name.kind != Kind::CrateScope,
    }
}

/// `failure` of `task` as its filed answer ([`filed`]), in `held`: no payload, the failure as a
/// [`TASK_FAILED`] error (with the task's own words when it has them), and every input it was
/// handed as a read, with the out it had.
fn filed_record<'b>(task: &Task<'_>, failure: &TaskFailure, words: Option<&str>, plant: Plant, held: &'b Frame) -> Made<'b> {
    let said = match words {
        Some(words) => bstd::mem::format_in!(held, "{failure}: {words}"),
        None => bstd::mem::format_in!(held, "{failure}"),
    };
    // A file whose text this build refuses (not UTF-8, past 4 GiB) is the user's error at that
    // file, never the tool's failure: its own finding, an error counted with the code's.
    let rule = match failure.why {
        Why::Refused { rule, .. } if rule == TEXT_REFUSED_RULE => FILE_NOT_TEXT,
        _ => TASK_FAILED,
    };
    failed_record(task, rule, said.as_str(), plant, held)
}

/// The rule a [`Why::Refused`] names when the refusal is the task's own file's text ([`Unread::Text`]):
/// what [`filed_record`] files as [`FILE_NOT_TEXT`]. One home for the words: every
/// `Unread::Text` conversion here names it.
pub(crate) const TEXT_REFUSED_RULE: &str = "blaze-cli work.rs, a file's text";

/// The finding number of a file whose text this build refuses (not UTF-8, past 4 GiB): an ERROR
/// of the user's code at that file, never a task failure; what waits on it is counted under it
/// like under a failed task (main.rs `draw`). Its one argument is the refusal's words.
pub const FILE_NOT_TEXT: u16 = 25;

/// The finding number of a task not run because an input it reads could not be made, the
/// input's own record holding the finding that says why (body_types.rs `Prepared::NotRun`).
/// Its one argument is the cause's words. It is no error of its own: the command prints the
/// root finding once and one count of the tasks it stopped (not_checked/tasks.rs).
pub const NOT_RUN: u16 = 23;

/// `task`, not run because of `cause` ([`NOT_RUN`]), as its filed answer: filed like a typed
/// failure ([`filed_record`]), so it stands while its key and its reads are today's and is
/// entered again when the input that stopped it moves.
fn not_run_record<'b>(task: &Task<'_>, cause: &str, plant: Plant, held: &'b Frame) -> Made<'b> {
    failed_record(task, NOT_RUN, cause, plant, held)
}

/// The finding number of a task that failed only because a task it needs failed (`Why::Waited`,
/// filed like any failure: [`filed`]). Its words are the needed task's name and the waiting
/// crate (not_checked/waited.rs `words`). It is no error of its own: the command prints the
/// root failure once with a count of what hangs from it (not_checked/waited.rs `groups`).
pub const WAITED: u16 = 24;

/// A filed failure's record ([`filed_record`]), or for a wait the [`WAITED`] finding naming its
/// cause and, for a crate's scope, its crate (`names`: the command's names step, when it ran).
/// `base` gives a fork's base (blaze-drive `Request::base_of`): the cause is named as its own
/// failure is printed, by its base, and so is the crate.
fn filed_answer<'b>(task: &Task<'_>, failure: &TaskFailure, words: Option<&str>, names: Option<&crate::names::Names<'_>>, base: &dyn Fn(Name) -> Name, plant: Plant, held: &'b Frame) -> Made<'b> {
    let Why::Waited { on } = failure.why else { return filed_record(task, failure, words, plant, held) };
    let label = match task.name.kind {
        Kind::CrateScope => names.and_then(|names| names.crate_at(base(task.name).subject).map(|at| names.crate_label(at))),
        _ => None,
    };
    let said = crate::not_checked::waited::words(base(on), label, held);
    failed_record(task, WAITED, said.as_str(), plant, held)
}

/// A filed failure's record, in `held`: no payload, the out [`failed_out`], the one finding
/// `rule` with `said` as its argument, and every input the task was handed as a read.
fn failed_record<'b>(task: &Task<'_>, rule: u16, said: &str, plant: Plant, held: &'b Frame) -> Made<'b> {
    let mut made = Made::new_in(task.name, held);
    made.out = failed_out();
    made.find(rule, (0, 0), &[said]);
    if plant == Plant::FiledWithoutReads {
        return made;
    }
    for input in task.inputs.iter() {
        made.reads.push((input.name, input.out));
    }
    made
}

/// What one task made, in the allocator `A`: a frame on the coordinator's thread ([`Made`]), or
/// `Global` on a pool worker, whose frame ends with its task (its payload moved into the
/// batch's answer once joined, in [`answer_batch`]: [`Record::into_held`]).
pub(crate) struct Record<A: Allocator + Copy> {
    pub(crate) name: Name,
    pub(crate) out: ContentId,
    pub(crate) parts: Vec<Part, A>,
    /// (rule, start, end, message start, message end) into `messages`.
    pub(crate) findings: Vec<(u16, u32, u32, usize, usize), A>,
    pub(crate) messages: Vec<u8, A>,
    /// The payload as the task built it, in `A`. Read it only through [`Record::payload_bytes`]:
    /// a record may instead be handed its payload whole ([`Record::hand`]).
    pub(crate) payload: Vec<u8, A>,
    /// A payload made before the task and handed to it whole, kept where it was made (rules 11
    /// and 51): a crate's `CrateScope` payload, made by its resolution in `Global` and held by
    /// the command's names state until its task answers. Copied into a task's frame it would
    /// be bounded by that frame, not by the record (`core`'s is 76 MB against a 64 MiB frame,
    /// 2026-11-16). Set only by [`Record::hand`], and then `payload` is empty; freed by
    /// [`Record::release_handed`], after which no read of the payload answers bytes.
    handed: Handed,
    /// The path of each `Locate::File` part: (its name, start and end in `path_bytes`, a module
    /// slot), so the drive can open it ([`PathOf`]).
    pub(crate) paths: Vec<(Name, u32, u32, bool), A>,
    pub(crate) path_bytes: Vec<u8, A>,
    /// What it read, each with the out its input had: empty for a task its source keys whole.
    /// A task keyed by an identity (a `CrateScope`) lists the records it read here, so the
    /// machine keeps its record standing while each read's out is today's.
    pub(crate) reads: Vec<(Name, ContentId), A>,
    /// A `BodyCheck`'s covered bodies: the checker key of each fn its check covered (the
    /// entry's own fn and each fn declared in a body it reached, at any depth), filed by
    /// body_types.rs `FileTypes::check` in the order reached. Sealed into the record's payload
    /// in its sorted form by [`seal_body_payload`]; empty for every other kind.
    pub(crate) covered: Vec<blaze_typeck::ItemKey, A>,
    /// The records the task made beside its own, each keyed by its own bytes and put right after
    /// it: a `CrateScope`'s expansion records (names.rs `scope_record`, blaze-scope
    /// expansion_records.rs). In `Global` with the handed payload they were made beside, moved
    /// with it ([`Record::into_held`]) and freed with it ([`Record::release_handed`]).
    pub(crate) extras: Extras,
}

/// A task's records beside its own ([`Record::extras`]): each distinct one once, sorted by key,
/// and how many rows of the task's own record name them (a crate's macro calls that made a
/// text), so the puts can say how many were shared.
pub(crate) struct Extras {
    pub(crate) records: Vec<(ContentId, Vec<u8, Global>), Global>,
    pub(crate) calls: u32,
    /// The bytes of `records` freed after their put; `records` is then empty.
    pub(crate) released: Option<usize>,
}

impl Extras {
    pub(crate) fn none() -> Extras {
        Extras { records: Vec::new_in(Global), calls: 0, released: None }
    }

    /// The bytes the records hold now.
    pub(crate) fn bytes(&self) -> usize {
        self.records.iter().map(|(_, bytes)| bytes.len()).sum()
    }

    /// A copy, each record's bytes copied in `Global` (a clone of a record, [`Record::clone_in`]).
    fn copied(&self) -> Extras {
        let mut records = Vec::with_capacity_in(self.records.len(), Global);
        for (key, bytes) in self.records.iter() {
            let mut copy = Vec::with_capacity_in(bytes.len(), Global);
            copy.extend_from_slice(bytes);
            records.push((*key, copy));
        }
        Extras { records, calls: self.calls, released: self.released }
    }
}

/// A record's handed payload, in its three states: none handed (the built payload is the
/// record's), handed and kept, or freed after its put. The freed state keeps only the length, so
/// a reader must match it and cannot be answered empty bytes (rule 76).
pub(crate) enum Handed {
    None,
    Kept(Vec<u8, Global>),
    Released { bytes: usize },
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
            handed: Handed::None,
            paths: Vec::new_in(alloc),
            path_bytes: Vec::new_in(alloc),
            reads: Vec::new_in(alloc),
            covered: Vec::new_in(alloc),
            extras: Extras::none(),
        }
    }

    /// A copy in `frame`, its payload too, whether built or handed: a clone of a record with a
    /// handed payload copies those bytes. Only for a record that stays where it is after the
    /// answer (the layout's `Units` records, units.rs `run`), copied into the batch's frame,
    /// which is reset once the machine has taken the batch; a pool worker's record is moved
    /// into its answer ([`Record::into_held`]) and a handed payload goes to its batch as it is.
    /// A frame that refuses a copy is a typed failure ([`HELD_REFUSED`]), never a panic.
    pub(crate) fn clone_in<'g>(&self, frame: &'g Frame) -> Result<Made<'g>, blaze_own::AllocFailure> {
        Ok(Record {
            name: self.name,
            out: self.out,
            parts: copied_in(&self.parts, frame)?,
            findings: copied_in(&self.findings, frame)?,
            messages: copied_in(&self.messages, frame)?,
            // A handed payload is copied as the clone's built one; a released one stays released
            // in the clone (its length only), so the clone's reads are refused as the record's.
            payload: copied_in(
                match &self.handed {
                    Handed::Kept(handed) => &**handed,
                    Handed::None | Handed::Released { .. } => &*self.payload,
                },
                frame,
            )?,
            handed: match self.handed {
                Handed::Released { bytes } => Handed::Released { bytes },
                Handed::None | Handed::Kept(_) => Handed::None,
            },
            paths: copied_in(&self.paths, frame)?,
            path_bytes: copied_in(&self.path_bytes, frame)?,
            reads: copied_in(&self.reads, frame)?,
            covered: copied_in(&self.covered, frame)?,
            extras: self.extras.copied(),
        })
    }

    /// The record's payload: the bytes handed to it ([`Record::hand`]), else those it built.
    /// The one read of a record's payload (the store's write, a test's digest). A payload freed
    /// after its put is refused, naming the record ([`InputWhy::Released`]), never read as empty.
    pub(crate) fn payload_bytes(&self) -> Result<&[u8], CliError> {
        match &self.handed {
            Handed::Kept(handed) => Ok(&**handed),
            Handed::None => Ok(&*self.payload),
            Handed::Released { bytes } => Err(CliError::Input { name: self.name, why: InputWhy::Released { bytes: *bytes } }),
        }
    }

    /// Hands the record `bytes` as its whole payload, moved, never copied: they stay in the
    /// allocator that made them until the record is dropped. Refused (rule 3) when the record
    /// already has a payload, built or handed: two payloads would be one record's two answers.
    pub(crate) fn hand(&mut self, bytes: Vec<u8, Global>) -> Result<(), CliError> {
        if !self.payload.is_empty() || !matches!(self.handed, Handed::None) {
            return Err(CliError::Text { what: "a record handed a payload already has one (rule 3, work.rs `Record::hand`)" });
        }
        self.handed = Handed::Kept(bytes);
        Ok(())
    }

    /// The handed payload freed, once the record is in the store and every later reader reads
    /// it there (rule 51: engine_check.rs, after a phase's put): how many bytes went. A payload
    /// built in the record's own allocator stays, as do its lists (findings, reads, paths). A
    /// later [`Record::payload_bytes`] is refused ([`InputWhy::Released`]).
    pub(crate) fn release_handed(&mut self) -> usize {
        let extras = self.release_extras();
        extras
            + match core::mem::replace(&mut self.handed, Handed::None) {
                Handed::Kept(bytes) => {
                    let len = bytes.len();
                    self.handed = Handed::Released { bytes: len };
                    len
                }
                Handed::None => 0,
                Handed::Released { bytes } => {
                    self.handed = Handed::Released { bytes };
                    0
                }
            }
    }

    /// The task's records beside its own ([`Record::extras`]) handed to it, moved: refused when it
    /// already has some (rule 3), as [`Record::hand`].
    pub(crate) fn hand_extras(&mut self, extras: Extras) -> Result<(), CliError> {
        if !self.extras.records.is_empty() || self.extras.calls != 0 || self.extras.released.is_some() {
            return Err(CliError::Text { what: "a record handed its extra records already has some (rule 3, work.rs `Record::hand_extras`)" });
        }
        self.extras = extras;
        Ok(())
    }

    /// The extra records freed once put (rule 51): how many bytes went.
    fn release_extras(&mut self) -> usize {
        if self.extras.released.is_some() {
            return 0;
        }
        let bytes = self.extras.bytes();
        self.extras.records = Vec::new_in(Global);
        self.extras.released = Some(bytes);
        bytes
    }

    /// The part `name`'s file is at `path`.
    pub(crate) fn path(&mut self, name: Name, path: &[u8], module: bool) {
        let at = self.path_bytes.len() as u32;
        self.path_bytes.extend_from_slice(path);
        self.paths.push((name, at, self.path_bytes.len() as u32, module));
    }
}

impl Record<Global> {
    /// A record a pool worker made, as its batch's answer in `frame` (the batch's, reset once
    /// the machine has stepped on the batch: the drive copies what it keeps into its own rows
    /// and the record's bytes into the store's put, so nothing here outlives the step): its
    /// payload, built or handed, moved whole into the answer where the worker made it (rules 11
    /// and 51), never copied into the batch's frame, which would bound the record by that
    /// frame; its lists (parts, findings, paths, reads) are copied, the size of the record's
    /// head, not its body. A frame that refuses a copy is a typed failure ([`HELD_REFUSED`]).
    pub(crate) fn into_held<'g>(self, frame: &'g Frame) -> Result<Made<'g>, blaze_own::AllocFailure> {
        let handed = match self.handed {
            Handed::None if self.payload.is_empty() => Handed::None,
            Handed::None => Handed::Kept(self.payload),
            kept_or_released => kept_or_released,
        };
        Ok(Record {
            name: self.name,
            out: self.out,
            parts: copied_in(&self.parts, frame)?,
            findings: copied_in(&self.findings, frame)?,
            messages: copied_in(&self.messages, frame)?,
            payload: Vec::new_in(frame),
            handed,
            paths: copied_in(&self.paths, frame)?,
            path_bytes: copied_in(&self.path_bytes, frame)?,
            reads: copied_in(&self.reads, frame)?,
            covered: copied_in(&self.covered, frame)?,
            extras: self.extras,
        })
    }
}

/// `values` copied into `frame`, or the frame's refusal: reserved whole first, so the copy
/// itself allocates nothing more (rule 18: a full frame is a typed failure, never a panic).
fn copied_in<'g, T: Copy>(values: &[T], frame: &'g Frame) -> Result<Vec<T, &'g Frame>, blaze_own::AllocFailure> {
    let mut copy = Vec::try_with_capacity_in(values.len(), frame)?;
    copy.extend_from_slice(values);
    Ok(copy)
}

/// Why a task whose record did not fit its batch's frame fails: a hole, never filed (the frame's
/// size is no function of the task's inputs), so the next command runs it again.
pub(crate) const HELD_REFUSED: &str = "its record's lists did not fit the batch's frame (work.rs `into_held`)";

/// A task whose record the batch's frame refused ([`HELD_REFUSED`]), as its answer: a hole. The
/// batch's frame is full, so the words are static; its peak is the profile's batch row.
pub(crate) fn held_refused<'f>(name: Name) -> Did<'f> {
    Did::Hole(TaskFailure { name, why: Why::Refused { what: HELD_REFUSED, rule: "blaze-cli work.rs" } }, HELD_REFUSED)
}

/// Seals the `UnitGraph` record's payload with the crate `list` (graph_payload.rs `seal`), in
/// place where it is: the graph's record is handed to its task's record on the heap (graph.rs
/// `hand_graph`), so the list and trailer are appended there, never to an empty built payload
/// beside it. Returns the record's out.
pub(crate) fn seal_graph<A: Allocator + Copy>(record: &mut Record<A>, list: &[u8]) -> ContentId {
    match &mut record.handed {
        Handed::Kept(payload) => crate::graph_payload::seal(payload, list),
        Handed::None => crate::graph_payload::seal(&mut record.payload, list),
        // Every caller seals the record it has just made, before its put; a released record
        // sealed would be an out over no payload. A panic fails its one job (rule 20), never a
        // quiet seal.
        Handed::Released { .. } => panic!("work.rs `seal_graph`: the record {:?} was sealed after its payload was released (rule 3)", record.name),
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
    /// The command's build scripts (scripts.rs): its setting and the graph, made by the first
    /// `Script` task of the command; `Err` holds why they could not be set up, said by every
    /// `Script` task after the first without trying again (each try held the graph record in
    /// the request's frame, which a failing setup filled on tempo, g054142).
    pub scripts: Option<Result<crate::scripts::Scripts<'f>, &'f str>>,
    /// The whole tool's producer id (D105), which a `Native` task's image key holds
    /// (native_image.rs).
    pub producer: blaze_ids::producer::ProducerId<blaze_ids::producer::Whole>,
    /// The rules the command lints with (lints.rs `lints_of`: the step's and `lints-add`'s): the
    /// root manifest's for `check`, none (no `Lint` part) for every other command. Its id is in
    /// the `FileItems` and `Lint` kinds' tools (home.rs `tools`).
    pub lints: blaze_lint::RuleSet,
    /// The extra records of the tasks answered this request, held until their owners are put
    /// (main.rs `Effect::Put`), then checked held at the end ([`PendingExtras::finish`]).
    pub extras: PendingExtras,
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

    /// A `UnitGraph` record's two sections (graph_payload.rs): its graph record, then its crate
    /// list. `name` is the input's, for the error.
    pub(crate) fn unit_graph(&self, name: Name) -> Result<crate::graph_payload::Sections<'_>, CliError> {
        crate::graph_payload::sections(self.payload()).map_err(|error| CliError::Input { name, why: InputWhy::Graph(error) })
    }
}

/// Reads the record `input` names (its key `at`) from `store` into `into`, the task's scratch:
/// what a task entered again with its needs reads of each. A record that is absent, not a
/// record, or a shape (a name, not a body) is a typed error naming the input: the machine
/// gave it as settled, so any of these is the tool's fault, never a miss to recompute.
pub(crate) fn payload<'f>(cx: &Cx, store: &mut dyn Store<'_>, input: &Input, into: &'f Frame) -> Result<Read<'f>, CliError> {
    let cx = cx.span("read an input");
    let mut bytes = Bytes::new_in(into, "an input's record");
    // A store's refusal here (most often a frame too small for the record) names the input
    // (rule 19); the caller names the task (`CliError::TaskInput`).
    let found = match store.get_into(&cx, input.at, &mut bytes) {
        Ok(found) => found,
        Err(CliError::Store(failure)) => return Err(CliError::Input { name: input.name, why: InputWhy::Store(failure) }),
        Err(other) => return Err(other),
    };
    if !found {
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

/// `error` of the job of `task` being made: an input's read names the task too (rule 19).
fn of_task(task: Name, error: CliError) -> CliError {
    match error {
        CliError::Input { name, why } => CliError::TaskInput { task, name, why },
        other => other,
    }
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
    /// A `UnitGraph` payload not of its form, or a crate list its check refused.
    Graph(crate::graph_payload::PayloadError),
    /// The store refused the read: its bytes would pass the frame read into, or the read
    /// itself failed.
    Store(crate::store::StoreFailure),
    /// Its record's payload was freed once it was put ([`Record::release_handed`]): it is read
    /// from the store now, never from the record (rule 51).
    Released { bytes: usize },
}

impl core::fmt::Display for InputWhy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InputWhy::Absent => f.write_str("the store holds no record at its key"),
            InputWhy::Malformed(error) => write!(f, "its record is not one: {error}"),
            InputWhy::Shape => f.write_str("its record is a shape, with no payload"),
            InputWhy::Graph(error) => write!(f, "{error}"),
            InputWhy::Store(failure) => write!(f, "{failure}"),
            InputWhy::Released { bytes } => write!(f, "its record's payload ({bytes} bytes) was freed once put; read it from the store (work.rs `Record::release_handed`)"),
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
    /// What the pooled `CrateScope` jobs of one [`run_batches`] call read (their dependencies'
    /// exports records, once a round each: `names::ScopeRound`): reset with `jobs`.
    pub scopes: Frame,
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
    /// The command's producers' table has this seed as this row (D105,
    /// `Producers::planted_for_tests`): an older producer's records, for the producer tests.
    Producer(blaze_ids::producer::Row, &'static str),
    /// A round's crate scope jobs share their dependencies' records by PLACE in each crate's
    /// list, not by record key (names/scope.rs `ScopeRound`): the bug a state shared across a
    /// round's jobs makes, which the pooled-records test must catch.
    ScopeShareByPlace,
    /// Each body drops its first head read (body_types/heads.rs `read_heads`): the bug a read
    /// list that misses a head makes, which the stale-check tests must catch
    /// (signature_grain_tests.rs `a_head_read_left_out_is_caught`).
    HeadReadLeftOut,
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
            scopes: Frame::with_reserve(crate::names::SCOPE_JOBS, Tag::Other).map_err(|_| CliError::Text { what: "the crate scope jobs' frame could not be reserved" })?,
        })
    }
}

/// `text` copied into `frame`, held as long as it is.
pub(crate) fn held_str<'f>(text: &str, frame: &'f Frame) -> &'f str {
    let bytes: &'f [u8] = Vec::from_slice_in(text.as_bytes(), frame).leak();
    // A copy of a `str`'s bytes is UTF-8: the check cannot fail.
    core::str::from_utf8(bytes).unwrap_or_default()
}

/// A task that failed, with the file it is about or in (`None` for a manifest's or a crate's),
/// as the closing note reads it from [`Failures`].
#[derive(Clone, Copy, Debug)]
pub struct Failed<'f> {
    pub path: Option<&'f str>,
    pub failure: TaskFailure,
    /// The task's own words, when it has them (`Did::FailedSaying`).
    pub detail: Option<&'f str>,
}

/// One failed task in [`Failures`]: its path and words as ranges of the table's text.
#[derive(Clone, Copy)]
struct FailedRow {
    failure: TaskFailure,
    path: Option<(u32, u32)>,
    detail: Option<(u32, u32)>,
}

/// The command's failed tasks, kept for the closing note (main.rs `draw`): ONE table on the
/// heap the command carries (`FetchEnv::heap`), its rows and one text of their paths and words,
/// each growing by doubling and freed with the table once the answer is printed. Never in the
/// command's frame, which never frees: there a path and words a failure grew it with the
/// failures (work.rs `answer_batch`, R4's list, wave 2311). A task that did not fail adds
/// nothing. A refused growth is a typed error ([`Failures::keep`]), never a panic.
pub struct Failures<'h> {
    rows: blaze_own::AVec<'h, FailedRow>,
    text: blaze_own::AVec<'h, u8>,
}

/// Why a failed task could not be kept for the closing note: the heap refused the table's growth.
pub(crate) const FAILURES_REFUSED: &str = "the command's table of failed tasks could not grow: the heap refused (work.rs `Failures`)";

impl<'h> Failures<'h> {
    /// An empty table on `heap`: nothing is taken until the first failure.
    pub fn new_in(heap: blaze_own::Alloc<'h>) -> Failures<'h> {
        Failures { rows: Vec::new_in(heap), text: Vec::new_in(heap) }
    }

    /// Keeps `failure` with its file's `path` and its own `detail` words, both copied into the
    /// table's text.
    pub(crate) fn keep(&mut self, failure: TaskFailure, path: Option<&[u8]>, detail: Option<&str>) -> Result<(), CliError> {
        let path = match path {
            Some(path) => Some(self.text_of(path)?),
            None => None,
        };
        let detail = match detail {
            Some(detail) => Some(self.text_of(detail.as_bytes())?),
            None => None,
        };
        self.rows.try_reserve(1).map_err(|_| CliError::Text { what: FAILURES_REFUSED })?;
        self.rows.push(FailedRow { failure, path, detail });
        Ok(())
    }

    /// `bytes` appended to the text, as their range there.
    fn text_of(&mut self, bytes: &[u8]) -> Result<(u32, u32), CliError> {
        let at = self.text.len();
        let (Ok(from), Ok(to)) = (u32::try_from(at), u32::try_from(at + bytes.len())) else {
            return Err(CliError::Text { what: "the failed tasks' paths and words passed 4 GiB (work.rs `Failures`)" });
        };
        self.text.try_extend_from_slice(bytes).map_err(|_| CliError::Text { what: FAILURES_REFUSED })?;
        Ok((from, to))
    }

    /// The `at`th failure kept, in the order kept.
    pub fn get(&self, at: usize) -> Option<Failed<'_>> {
        let row = self.rows.get(at)?;
        let text = |range: Option<(u32, u32)>| range.and_then(|(at, to)| core::str::from_utf8(&self.text[at as usize..to as usize]).ok());
        Some(Failed { path: text(row.path), failure: row.failure, detail: text(row.detail) })
    }

    /// Every failure kept, in the order kept.
    pub fn iter(&self) -> impl Iterator<Item = Failed<'_>> + '_ {
        (0..self.rows.len()).filter_map(|at| self.get(at))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The heap bytes the table holds: its rows' and its text's capacity.
    pub(crate) fn bytes(&self) -> usize {
        self.rows.capacity() * core::mem::size_of::<FailedRow>() + self.text.capacity()
    }
}

/// The most of each frame used at once, over the request: the profile's `frames` line.
#[derive(Clone, Copy, Default, Debug)]
pub struct HighWater {
    /// One task's work.
    pub scratch: usize,
    /// One batch's records, until the machine took them.
    pub batch: usize,
    /// The pooled jobs' paths and body lists of one round of batches.
    pub jobs: usize,
    /// The request's own frame at the answer.
    pub request: usize,
    /// One crate scope's resolution on the pool, in its own frame (`names::SCOPE_FRAME`), not
    /// a task's scratch: a crate's names are as large as the crate.
    pub scope: usize,
    /// One file's body job on the pool, in its own frame ([`BODY_FRAME`]).
    pub bodies: usize,
    /// Every task frame's used bytes at its end, by the task's kind: the profile's
    /// `task frame bytes` rows.
    pub tasks: crate::phases::byte_rows::TaskFrames,
    /// The largest pooled crate scope frame's bytes by the stretch of the solve that took
    /// them (names/metered.rs): the profile's `scope frame by stretch` row.
    pub scope_bytes: crate::names::metered::ScopeBytes,
}

/// Run every task of `batches` (every batch the request has out), then answer the machine once
/// a batch, in order; the batch frame is reset after each.
#[expect(clippy::too_many_arguments, reason = "the batch's edge: each argument one handle or fact of it")]
pub fn run_batches<'f, 'r>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'_>,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    req: &mut Request<'r>,
    batches: std::vec::Vec<Batch<'r>, Global>,
    frame: &'f Frame,
    frames: &mut Frames,
    failed: &mut Failures<'_>,
    high: &mut HighWater,
    alloc: blaze_own::Alloc<'_>,
    effects: &mut AVec<'_, Effect<'r>>,
    phases: &mut Phases,
) -> Result<(), CliError> {
    let cx = cx.span("batches");
    let began = phases.now();
    let steps_before = phases.request.steps;
    unwind_refusals();
    let Frames { scratch, batch: held, jobs, scopes } = frames;
    // The process's heap, handed from `main`: what the pool's jobs make on it (`BodiesJob::heap`).
    let heap = facts.fetch_env.heap;
    // Every batch's `FileItems` tasks go to the pool at once, each in its own frame, so the
    // pool is as wide as every batch the request has handed out (it hands out one a worker);
    // then each batch is answered in turn: its other tasks run here, its pooled ones joined.
    // A batch's `BodyCheck` tasks are one job a home file: the file is read and outlined once
    // for all its bodies, not once a body (rule 77).
    // Pool tasks read through the lock-free handle, which cannot map: every segment the head
    // names is mapped now, once a request (drive.md ruling 7; later asks do nothing).
    store.map_all(&cx)?;
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
        // The round's crate scope jobs share their dependencies' records, read once a round.
        let mut round = crate::names::ScopeRound::new(&*scopes, facts.plant == Plant::ScopeShareByPlace);
        // The round's body jobs being gathered: each file's job and its bodies so far, over
        // every batch (a batch is of one home; two batches may be of the same one).
        let mut files: Vec<(Job<'_>, Vec<(Name, u32, &[(Name, ContentId)]), &Frame>, &crate::body_types::TypesJob<'_>), Global> = Vec::new_in(Global);
        // The round's lint jobs, the same way: each file's job and its asked units (each a
        // `Lint` task's name and source), read and outlined once for all of them.
        let mut lint_files: Vec<(Job<'_>, Vec<(Name, Name, ContentId), &Frame>), Global> = Vec::new_in(Global);
        // The order the round's jobs are made in, so the order a file job checks its bodies in:
        // as handed out, or reversed by the determinism test (`order_tests.rs`). An answer must
        // not depend on it (parallel.md, point 8).
        let reversed = reversed_for_test();
        let pick = |at: usize, len: usize| if reversed { len - 1 - at } else { at };
        for b in 0..batches.len() {
            let batch = &batches[pick(b, batches.len())];
            let mut tasks = Vec::with_capacity_in(batch.tasks.len(), Global);
            for t in 0..batch.tasks.len() {
                let (_, task) = &batch.tasks[pick(t, batch.tasks.len())];
                if let Some(on) = failed_input(task) {
                    tasks.push(Spawned::Waited(on));
                    continue;
                }
                let made_at = phases.now();
                let spawned = match task.name.kind {
                    Kind::FileItems => match job_of(req, facts, task, jobs) {
                        Ok(job) => Spawned::One(scope.may_overlap("outline", task_frame, {
                            let clock = &clock;
                            move |cx: &Cx| Ok::<_, core::convert::Infallible>(pooled(cx, root, &job, clock))
                        })),
                        Err(what) => Spawned::Refused(what),
                    },
                    // A crate's names on the pool: its job made here from record bytes
                    // (names/scope.rs); a task with no resolution to make answers here.
                    Kind::CrateScope if builds(Kind::CrateScope) => match unwinding(|| scope_job(&cx, root, store, facts, task, frame, &mut round, alloc)) {
                        Err(Panicked) => Spawned::Refused("a crate scope's job panicked while it was made (its words are on the error stream)"),
                        Ok(Ok(Some(job))) => Spawned::Scope(scope.may_overlap("crate scope", crate::names::SCOPE_FRAME, {
                            let clock = &clock;
                            move |cx: &Cx| Ok::<_, core::convert::Infallible>(pooled_scope(cx, root, job, clock))
                        })),
                        Ok(Ok(None)) => Spawned::Here,
                        Ok(Err(CliError::Text { what })) => Spawned::Refused(what),
                        Ok(Err(error)) => return Err(of_task(task.name, error)),
                    },
                    // One build script's run on the pool (scripts.rs, D115): its job made here
                    // from record bytes, its files located; the run in its worker's frame.
                    Kind::Script => {
                        if facts.laid.is_none() {
                            facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, facts.fetch_env.heap, frame, alloc)?);
                        }
                        let lock = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref()).map(|lockfile| lockfile.lock);
                        let members = crate::names::member_dirs(facts.laid.as_ref());
                        // A panic while its job is made (a bug, an allocation a frame refused)
                        // fails this script's task alone, as a crate scope's does (rule 20).
                        let made = match unwinding(|| crate::scripts::job(&cx, root, store, &mut facts.scripts, &mut facts.names, &facts.fetch_env, facts.workers, lock, members, task, frame, jobs)) {
                            Err(Panicked) => Err(Spawned::Refused("a build script's job panicked while it was made (its words are on the error stream)")),
                            Ok(made) => Ok(made.map_err(|error| of_task(task.name, error))?),
                        };
                        match made {
                            Err(spawned) => spawned,
                            Ok(crate::scripts::Prepared::Need(wants)) => Spawned::Need(wants),
                            Ok(crate::scripts::Prepared::Waited(on)) => Spawned::Waited(on),
                            Ok(crate::scripts::Prepared::Said { what, words, hole }) => {
                                let mut owned = std::vec::Vec::new_in(Global);
                                owned.extend_from_slice(words.as_bytes());
                                Spawned::Said { what, words: owned, hole }
                            }
                            Ok(crate::scripts::Prepared::Ready(job)) => Spawned::Script(scope.may_overlap("build script", crate::scripts::SCRIPT_FRAME, {
                                let clock = &clock;
                                move |cx: &Cx| Ok::<_, core::convert::Infallible>(crate::scripts::pooled(cx, &job, clock))
                            })),
                        }
                    }
                    // A `Signature` (D114) runs in its home file's body job beside the bodies, in
                    // the same list, served by the same `prepare` (it reads the same crates).
                    Kind::BodyCheck | Kind::Signature => {
                        // The crates its types read come first (body_types.rs): a task that
                        // lacks them answers `Need`, and joins its file's job only with them.
                        // A body joins its file's job of this round when its inputs are what that
                        // job was prepared from (`TypesJob::covered_by`): prepared once a file a
                        // round, not once a body. One whose inputs differ is prepared over its
                        // own and gets a job of its own, never checked over another's reads.
                        let path = req.file_of(task);
                        let known = files.iter().position(|(job, _, types)| Some(job.slot.as_bytes()) == path && job.edition == task.edition && types.covered_by(&task.inputs));
                        let group = match known {
                            Some(group) => Ok(group),
                            None => match job_of(req, facts, task, jobs) {
                                Err(what) => Err(Spawned::Refused(what)),
                                Ok(job) => {
                                    let real = crate::sweep::real_path(job.slot, job.flags, jobs);
                                    let real = core::str::from_utf8(real.leak()).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
                                    // No names world: `prepare` reads the crate list in place
                                    // (graph_payload.rs). Its per-owner scratch is the task
                                    // scratch frame, released per owner (`owner_wants`).
                                    match crate::body_types::prepare(&cx, root, store, facts, task, real, frame, jobs, scratch, alloc) {
                                        Err(error) => return Err(error),
                                        Ok(crate::body_types::Prepared::Need(wants)) => Err(Spawned::Need(wants)),
                                        Ok(crate::body_types::Prepared::Waited(on)) => Err(Spawned::Waited(on)),
                                        Ok(crate::body_types::Prepared::Refused(what)) => Err(Spawned::Refused(what)),
                                        Ok(crate::body_types::Prepared::NotRun(cause)) => Err(Spawned::NotRun(cause)),
                                        Ok(crate::body_types::Prepared::Ready(types)) => {
                                            files.push((job, Vec::new_in(jobs), types));
                                            Ok(files.len() - 1)
                                        }
                                    }
                                }
                            },
                        };
                        match group {
                            Err(spawned) => spawned,
                            // Each body takes the `Signature`s and `Surface` it was handed: it
                            // reads each at its task's out, never one it made (body_types/heads.rs).
                            Ok(group) => match crate::body_types::handed_reads(&task.inputs, jobs) {
                                Err(what) => Spawned::Refused(what),
                                Ok(handed) => {
                                    let bodies = &mut files[group].1;
                                    match bodies.try_push((task.name, task.place, handed)) {
                                        Ok(()) => Spawned::Body { group, at: bodies.len() - 1 },
                                        Err(_) => Spawned::Refused(JOBS_FULL),
                                    }
                                }
                            },
                        }
                    }
                    // A lint unit joins its file's lint job of this round: it reads nothing, so
                    // any two units of one file share one.
                    Kind::Lint => {
                        let path = req.file_of(task);
                        let known = lint_files.iter().position(|(job, _)| Some(job.slot.as_bytes()) == path && job.edition == task.edition);
                        let group = match known {
                            Some(group) => Ok(group),
                            None => match job_of(req, facts, task, jobs) {
                                Err(what) => Err(Spawned::Refused(what)),
                                Ok(job) => {
                                    lint_files.push((job, Vec::new_in(jobs)));
                                    Ok(lint_files.len() - 1)
                                }
                            },
                        };
                        match group {
                            Err(spawned) => spawned,
                            Ok(group) => {
                                let units = &mut lint_files[group].1;
                                match units.try_push((task.name, task.base, task.source)) {
                                    Ok(()) => Spawned::Lint { group, at: units.len() - 1 },
                                    Err(_) => Spawned::Refused(JOBS_FULL),
                                }
                            }
                        }
                    }
                    _ => Spawned::Here,
                };
                phases.worked(made_row(task.name.kind), phases.since(made_at));
                tasks.push(spawned);
            }
            if reversed {
                tasks.reverse();
            }
            pending.push(tasks);
        }
        if reversed {
            pending.reverse();
        }
        groups.try_reserve_exact(files.len() + lint_files.len()).map_err(|_| CliError::Text { what: "the list of a round's body jobs could not be made" })?;
        for (file, bodies, types) in files {
            let job = BodiesJob { file, bodies: bodies.leak(), types, heap };
            groups.push(Group::Running(scope.may_overlap("bodies", BODY_FRAME, {
                let clock = &clock;
                move |cx: &Cx| Ok::<_, core::convert::Infallible>(pooled_bodies(cx, root, &job, clock))
            })));
        }
        // The lint jobs after the body jobs: a lint task's group is placed past them.
        let base = groups.len();
        let rules = facts.lints;
        for (file, units) in lint_files {
            let job = LintsJob { file, units: units.leak(), rules };
            groups.push(Group::Running(scope.may_overlap("lints", task_frame, {
                let clock = &clock;
                move |cx: &Cx| Ok::<_, core::convert::Infallible>(pooled_lints(cx, root, &job, clock))
            })));
        }
        for tasks in pending.iter_mut() {
            for spawned in tasks.iter_mut() {
                if let Spawned::Lint { group, at } = *spawned {
                    *spawned = Spawned::Body { group: base + group, at };
                }
            }
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
    high.jobs = high.jobs.max(jobs.peak()).max(scopes.peak());
    jobs.reset();
    scopes.reset();
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
    /// Not run: an input it reads could not be made, and that input's record says why
    /// (body_types.rs `Prepared::NotRun`). Its answer is a [`NOT_RUN`] finding naming the cause.
    NotRun(&'static str),
    /// A `FileItems` task's own pool handle.
    One(bstd::task::Task<'s, Pooled>),
    /// A `CrateScope` task's resolution on the pool (names/scope.rs).
    Scope(bstd::task::Task<'s, PooledScope>),
    /// A `BodyCheck` task: the `at`th body of the round's `group`th file job. A `Lint` task's
    /// job is one of the round's groups too, placed past the body jobs.
    Body { group: usize, at: usize },
    /// A `Lint` task: the `at`th unit of the round's `group`th lint job, until the lint jobs are
    /// placed among the groups (then a [`Spawned::Body`]).
    Lint { group: usize, at: usize },
    /// An input it was handed is a filed failure ([`failed_input`]): it fails `Waited` on it,
    /// unrun.
    Waited(Name),
    /// A `BodyCheck` task that lacks inputs (body_types.rs `prepare`): it answers `Need`.
    Need(std::vec::Vec<Want, Global>),
    /// A `Script` task's run on the pool (scripts.rs).
    Script(bstd::task::Task<'s, crate::scripts::PooledScript>),
    /// A `Script` task whose job could not be made: it fails with these words (UTF-8), a hole
    /// when the cause is outside its key.
    Said { what: &'static str, words: std::vec::Vec<u8, Global>, hole: bool },
}

/// A batch's tasks as [`run_batches`] spawned them.
type Pending<'s> = Vec<Spawned<'s>, Global>;

/// One file's body job of a round ([`BodiesJob`]): on the pool, then joined by the first of its
/// bodies' tasks answered, its outcomes taken one a task.
enum Group<'s> {
    Running(bstd::task::Task<'s, PooledBodies>),
    /// Each body's outcome, and the job's time a body (the file's one read and outline are
    /// shared by its bodies: each is charged its part); the bodies that answered a need.
    Joined { did: BodiesDid, needs: BodiesNeed, ns: u64 },
    /// Being joined (never seen: the join's own state).
    Taken,
}

/// One batch: its tasks not pooled run here, in order, while the pool works; then each pooled
/// one is joined in ticket order, its record's payload moved into the answer, and the batch's
/// outcomes step the request. A scope narrowed to one worker ran each inline at its spawn.
#[expect(clippy::too_many_arguments, reason = "run_batches's, with its frames split")]
fn answer_batch<'f, 'r>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'_>,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    req: &mut Request<'r>,
    batch: Batch<'r>,
    spawned: Pending<'_>,
    groups: &mut Vec<Group<'_>, Global>,
    frame: &'f Frame,
    held: &Frame,
    scratch: &mut Frame,
    failed: &mut Failures<'_>,
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
            (Spawned::NotRun(cause), _) => (Did::Done(not_run_record(task, cause, facts.plant, held)), 0),
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
                    high.bodies = high.bodies.max(joined.used);
                    high.tasks.ended(task.name.kind, joined.used);
                    match task.name.kind {
                        Kind::Lint => phases.worked(crate::lints::LINT_JOBS, joined.ns),
                        _ => {
                            phases.worked(BODY_JOBS, joined.ns);
                            phases.worked(BODY_SETUP, joined.setup_ns);
                        }
                    }
                    if let Some(error) = joined.stopped {
                        return Err(error.into());
                    }
                    *slot = Group::Joined { ns: joined.ns / (joined.did.len().max(1) as u64), did: joined.did, needs: joined.needs };
                }
                let (done, need, ns) = match slot {
                    Group::Joined { did, needs, ns } => {
                        let need = needs.iter_mut().find(|(body, _)| *body == at).map(|(_, wants)| core::mem::take(wants));
                        (did.get_mut(at).and_then(Option::take), need, *ns)
                    }
                    Group::Running(_) | Group::Taken => (None, None, 0),
                };
                let done = match (need, done) {
                    // A body that lacks a `Signature` it read (body_types/heads.rs): entered
                    // again with it handed.
                    (Some(wants), _) => Did::Need(Vec::from_slice_in(&wants, held)),
                    (None, Some(Ok(record))) => record.into_held(held).map_or_else(|_| held_refused(task.name), Did::Done),
                    (None, Some(Err(why))) => Did::Failed(TaskFailure { name: task.name, why }),
                    (None, None) => Did::Failed(TaskFailure { name: task.name, why: unanswered }),
                };
                phases.worked(row_of(task.name.kind), ns);
                (done, ns)
            }
            (Spawned::Script(handle), _) => {
                let joined = handle.join(&cx).map_err(CliError::Pool)?;
                high.scratch = high.scratch.max(joined.used);
                high.tasks.ended(task.name.kind, joined.used);
                phases.worked(row_of(task.name.kind), joined.ns);
                let done = match joined.did {
                    Err(why) => Did::Failed(TaskFailure { name: task.name, why }),
                    Ok(crate::scripts::Ran::Made(record)) => record.into_held(held).map_or_else(|_| held_refused(task.name), Did::Done),
                    Ok(crate::scripts::Ran::Failed { what, words, hole }) => {
                        let failure = TaskFailure { name: task.name, why: Why::Refused { what, rule: crate::scripts::RULE } };
                        let words = core::str::from_utf8(&words).map_err(|_| CliError::Text { what: "a build script's failure words are not UTF-8" })?;
                        match hole {
                            true => Did::Hole(failure, held_str(words, held)),
                            false => Did::FailedSaying(failure, held_str(words, held)),
                        }
                    }
                };
                (done, joined.ns)
            }
            (Spawned::Lint { .. }, _) => return Err(CliError::Text { what: "a lint task's job was not placed among its round's groups (work.rs `run_batches`)" }),
            (Spawned::Said { what, words, hole }, _) => {
                let failure = TaskFailure { name: task.name, why: Why::Refused { what, rule: crate::scripts::RULE } };
                let words = core::str::from_utf8(&words).map_err(|_| CliError::Text { what: "a build script's failure words are not UTF-8" })?;
                let done = match hole {
                    true => Did::Hole(failure, held_str(words, held)),
                    false => Did::FailedSaying(failure, held_str(words, held)),
                };
                (done, 0)
            }
            (Spawned::Scope(handle), _) => {
                let joined = handle.join(&cx).map_err(CliError::Pool)?;
                high.scope = high.scope.max(joined.used);
                high.tasks.ended(task.name.kind, joined.used);
                if joined.bytes.peak() > high.scope_bytes.peak() {
                    high.scope_bytes = joined.bytes;
                }
                phases.worked(SCOPES, joined.ns);
                // Filed here, on the main thread: a need is the answer; a made crate is
                // answered by the serial step, which now finds it made (its surfaces, its record).
                let done = match joined.did {
                    Err(why) => Did::Failed(TaskFailure { name: task.name, why }),
                    Ok(resolution) => {
                        let Some(names) = facts.names.as_mut() else {
                            return Err(CliError::Text { what: "the names world a crate scope's job was made from is gone" });
                        };
                        match crate::names::settle(names, task, resolution, held) {
                            Some(did) => did,
                            None => run_one(&cx, root, store, facts, task, frame, held, scratch, high, alloc)?,
                        }
                    }
                };
                (done, joined.ns)
            }
            (Spawned::One(handle), _) => {
                let joined = handle.join(&cx).map_err(CliError::Pool)?;
                high.scratch = high.scratch.max(joined.used);
                high.tasks.ended(task.name.kind, joined.used);
                // A file's text this build refuses (not UTF-8, past 4 GiB) fails its own task,
                // a function of the text (rule 20); a path or a read that failed stops the command.
                let did = match joined.did {
                    Ok(did) => did,
                    Err(Unread::Text(what)) => Err(Why::Refused { what, rule: TEXT_REFUSED_RULE }),
                    Err(Unread::Changed) => Err(Why::Changed),
                    Err(Unread::Path(error)) => return Err(CliError::Path(error)),
                    Err(Unread::Fs(error)) => return Err(CliError::Fs(error)),
                };
                let done = match did {
                    Ok(record) => record.into_held(held).map_or_else(|_| held_refused(task.name), Did::Done),
                    Err(why) => Did::Failed(TaskFailure { name: task.name, why }),
                };
                phases.worked(row_of(task.name.kind), joined.ns);
                (done, joined.ns)
            }
            (Spawned::Here, None) => unreachable!("a task neither pooled nor run here"),
        };
        // A typed failure that is a function of the task's key and inputs is its answer, filed.
        let done = match done {
            Did::Failed(failure) if filed(&failure, task) => Did::Done(filed_answer(task, &failure, None, facts.names.as_ref(), &|name| req.base_of(name), facts.plant, held)),
            Did::FailedSaying(failure, words) if filed(&failure, task) => Did::Done(filed_answer(task, &failure, Some(words), facts.names.as_ref(), &|name| req.base_of(name), facts.plant, held)),
            other => other,
        };
        let path = match req.file_of(task) {
            Some(path) => Some(core::str::from_utf8(path).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?),
            None => None,
        };
        keep_failed(failed, &done, path)?;
        let mut done = done;
        if let Did::Done(made) = &mut done {
            tally(&mut phases.records, req, facts.names.as_ref(), task, made);
            // Held until the machine puts their owner (main.rs `Effect::Put`), which gives
            // their namespace: the owner's.
            facts.extras.hold(made.name, core::mem::replace(&mut made.extras, Extras::none()))?;
            #[cfg(test)]
            answers_for_test::answered(made);
        }
        did.push((*ticket, done));
        took.push(ns);
    }
    phases.took("pooled tasks joined", frame);
    let request_frame = frame;
    // A file a crate's names read that no units record and no outline lists stops the machine
    // by its name alone (a digest): the names step knows which crate read which path, so the
    // error names both (rule 19).
    let stepped = with_outcomes(&did, &took, held, |results| {
        let stepped = phases.now();
        phases.took("a batch's lists", request_frame);
        req.step(Event::Ran { batch: batch.id, results }, effects).map(|()| stepped)
    })
    .map_err(|error| match (&error, facts.names.as_ref()) {
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

/// The machine's path for the records a task makes beside its own ([`Record::extras`], a
/// `CrateScope`'s expansion records): each goes to the store under its OWNER's namespace
/// (store.rs `Sharing` over the owner's bytes), never another (a public owner naming a local
/// record publishes one no other machine can read: rule 21). The owner's bytes are the
/// machine's to make, so the extras are held here, by the owner's name, from the batch's join
/// ([`hold`](PendingExtras::hold)) until main.rs performs the owner's `Effect::Put`
/// ([`put_before`](PendingExtras::put_before)), which puts them first, so no reader of the
/// owner finds one absent. The engine's puts give the owner's bytes at once (engine_store.rs
/// `put_all`).
pub struct PendingExtras {
    /// By owner, in hold order; an owner at most once.
    held: Vec<(Name, Extras), Global>,
}

impl PendingExtras {
    /// None held.
    pub fn empty() -> PendingExtras {
        PendingExtras { held: Vec::new_in(Global) }
    }

    /// `extras` of the task `owner`, held until its record is put; none is nothing to hold.
    pub(crate) fn hold(&mut self, owner: Name, extras: Extras) -> Result<(), CliError> {
        if extras.records.is_empty() {
            return Ok(());
        }
        if self.held.iter().any(|(name, _)| *name == owner) {
            return Err(CliError::ExtrasHeldTwice { owner });
        }
        self.held.push((owner, extras));
        Ok(())
    }

    /// Before the record `bytes` is put: when it is a whole drive record whose name holds
    /// extras, each of them put under its namespace (`bytes` as the owner), in key order.
    pub(crate) fn put_before(&mut self, cx: &Cx, store: &mut dyn Store<'_>, bytes: &[u8]) -> Result<(), CliError> {
        if self.held.is_empty() {
            return Ok(());
        }
        let Ok(blaze_drive::record::Stored::Whole(view)) = blaze_drive::record::open(bytes) else { return Ok(()) };
        let Some(at) = self.held.iter().position(|(name, _)| *name == view.name) else { return Ok(()) };
        let (_, extras) = self.held.swap_remove(at);
        for (key, record) in extras.records.iter() {
            store.put_record(cx, blaze_drive::Key(*key), crate::store::plain_place::EXPANSION_TAG, &[bytes], record)?;
        }
        Ok(())
    }

    /// At the request's end: an owner the machine never put (its record was settled from the
    /// store, put by an earlier command with its extras) must find every extra record held
    /// already; one the store lacks has no known namespace and is refused by name.
    pub(crate) fn finish(&mut self, cx: &Cx, store: &mut dyn Store<'_>) -> Result<(), CliError> {
        for (owner, extras) in core::mem::replace(&mut self.held, Vec::new_in(Global)).iter() {
            for (key, _) in extras.records.iter() {
                let key = blaze_drive::Key(*key);
                if store.get(cx, key)?.is_none() {
                    return Err(CliError::ExtrasOwnerNotPut { owner: *owner, key });
                }
            }
        }
        Ok(())
    }
}

/// A task's answer `done`, kept for the closing note when it failed (with `path`, its file): its
/// failure, path and own words copied into the command's one table of failures ([`Failures`]),
/// on the heap. A task that did not fail keeps nothing there, nor anywhere past its batch.
pub(crate) fn keep_failed(failed: &mut Failures<'_>, done: &Did<'_>, path: Option<&str>) -> Result<(), CliError> {
    match done {
        Did::Failed(failure) => failed.keep(*failure, path.map(str::as_bytes), None),
        Did::FailedSaying(failure, words) | Did::Hole(failure, words) => failed.keep(*failure, path.map(str::as_bytes), Some(words)),
        Did::Done(_) | Did::Need(_) => Ok(()),
    }
}

/// Failures of one kind shown one by one before the rest of the run is counted.
pub(crate) const SAME_SHOWN: usize = 4;

/// The closing note's lines for the command's failed tasks, each handed to `each`: a failed task
/// is the tool's, not the code's, named with its file and its own words. Failures of one kind
/// past the first [`SAME_SHOWN`] are counted, never listed one by one (a lock whose 227 units
/// could not be fetched is four lines and a count). A task that failed only because one it
/// needed failed (`Waited`) prints nothing of its own: not_checked.rs names it by cause. Each
/// line is built in `lines` (the drawing's scratch, never the command's frame), so what this
/// takes is in proportion to the lines printed, not to the failures kept.
pub(crate) fn failed_notes(failed: &Failures<'_>, lines: &Frame, each: &mut dyn FnMut(&str) -> Result<(), CliError>) -> Result<(), CliError> {
    let kind = |at: usize| failed.get(at).map(|entry| (entry.failure.name.kind, entry.failure.why));
    let mut same = 0usize;
    for (i, failure) in failed.iter().enumerate() {
        if matches!(failure.failure.why, Why::Waited { .. }) {
            continue;
        }
        same = if i > 0 && kind(i) == kind(i - 1) { same + 1 } else { 0 };
        let last_of_run = kind(i + 1) != kind(i);
        if same >= SAME_SHOWN {
            if last_of_run {
                let words = bstd::mem::format_in!(lines, "and {} more task(s) failed the same way", same + 1 - SAME_SHOWN);
                each(words.as_str())?;
            }
            continue;
        }
        let mut words = bstd::mem::String::new_in(lines);
        let written = match failure.path {
            Some(path) => core::fmt::write(&mut words, format_args!("a task failed in {path}: {}", failure.failure)),
            None => core::fmt::write(&mut words, format_args!("a task failed: {}", failure.failure)),
        };
        written.map_err(|_| CliError::Text { what: "a failed task's words" })?;
        if let Some(detail) = failure.detail {
            words.push_str(": ");
            words.push_str(detail);
        }
        each(words.as_str())?;
    }
    Ok(())
}

/// A batch's answers as the machine takes them, built in `frame` (the batch's, reset after the
/// batch once the machine has stepped): each made record's path and finding views and its
/// `Output`, borrowed from the records in `did`, then one `Outcome` a task; `step` is handed
/// them and its result returned. The drive copies what it keeps (its rows, a record's findings,
/// the bytes it puts), so nothing built here, nor the records, outlives the step: the command's
/// frame takes nothing of a task (memory in proportion to what is printed, never to what ran).
pub(crate) fn with_outcomes<'d, R>(did: &'d [(blaze_drive::Ticket, Did<'_>)], took: &[u64], frame: &'d Frame, step: impl FnOnce(&[Outcome<'_>]) -> R) -> R {
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
        if let Did::Done(made) = done
            && let Ok(payload) = made.payload_bytes()
        {
            outputs.push(Output {
                name: made.name,
                out: made.out,
                reads: &made.reads,
                cells: &[],
                parts: &made.parts,
                paths,
                findings,
                payload,
            });
        }
    }
    let mut results = Vec::with_capacity_in(did.len(), frame);
    let mut at = 0;
    for ((ticket, done), ns) in did.iter().zip(took.iter()) {
        let ran = match done {
            // A record whose payload was released after its put is no answer: refused, named.
            Did::Done(made) if made.payload_bytes().is_err() => Ran::Failed(TaskFailure { name: made.name, why: Why::Refused { what: "its record's payload was released after its put", rule: "work.rs `Record::release_handed`" } }),
            Did::Done(_) => {
                at += 1;
                Ran::Done(&outputs[at - 1..at])
            }
            Did::Need(wants) => Ran::Need(wants),
            Did::Failed(failure) | Did::FailedSaying(failure, _) | Did::Hole(failure, _) => Ran::Failed(*failure),
        };
        results.push(Outcome { ticket: *ticket, ran, in_place: 0, ns: *ns });
    }
    step(&results)
}

/// The profile's row of the body jobs, one a file a round: its times are the files read and
/// outlined for their bodies ([`Phases::times`]).
pub(crate) const BODY_JOBS: &str = "pooled body jobs, one a file a round (worker ns)";
/// Of [`BODY_JOBS`], each job's setup: its owners' readers and front end over the records
/// (body_types.rs `FileTypes::new`), before any body is checked.
pub(crate) const BODY_SETUP: &str = "of those, the setup: readers and the world over records (worker ns)";

/// The profile's row of the pooled `CrateScope` resolutions.
pub(crate) const SCOPES: &str = "pooled CrateScope (worker ns)";

/// The profile's row of the `BodyCheck` tasks.
pub(crate) const BODIES: &str = "pooled BodyCheck (worker ns)";

/// A task kind's row in the profile's frame lines (`Phases::took` for a task run on the main
/// thread, `Phases::worked` for a pooled one).
/// Whether a round's jobs are made in reverse order ([`run_batches`]): never in the command;
/// only the determinism test turns it on (tests/order_tests.rs).
#[cfg(not(test))]
const fn reversed_for_test() -> bool {
    false
}

#[cfg(test)]
fn reversed_for_test() -> bool {
    answers_for_test::REVERSED.with(core::cell::Cell::get)
}

/// What the determinism test reads of a command (tests/order_tests.rs): each record answered,
/// on the thread that steps the request (the test's own).
#[cfg(test)]
pub(crate) mod answers_for_test {
    use super::{Allocator, ContentId, Name, Record};

    /// One answered record: its name (its key's task), its out, and digests of its payload, its
    /// findings with their words, and its read list.
    pub(crate) type Answer = (Name, ContentId, ContentId, ContentId, ContentId);

    std::thread_local! {
        pub(crate) static REVERSED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
        pub(crate) static ANSWERED: core::cell::RefCell<std::vec::Vec<Answer>> = const { core::cell::RefCell::new(std::vec::Vec::new()) };
        /// Each body and signature a file job checked, in the order checked: the simulation runs
        /// pool tasks on the test's thread (bstd task.rs, `Layout::Inline`).
        /// Each with its file's path and its place there, so a test can name the item.
        pub(crate) static CHECKED: core::cell::RefCell<std::vec::Vec<(Name, std::string::String, u32)>> = const { core::cell::RefCell::new(std::vec::Vec::new()) };
    }

    pub(crate) fn answered<A: Allocator + Copy>(made: &Record<A>) {
        let mut findings = std::vec::Vec::new();
        for &(rule, start, end, from, to) in made.findings.iter() {
            findings.extend_from_slice(&rule.to_le_bytes());
            findings.extend_from_slice(&start.to_le_bytes());
            findings.extend_from_slice(&end.to_le_bytes());
            findings.extend_from_slice(&((to - from) as u64).to_le_bytes());
            findings.extend_from_slice(&made.messages[from..to]);
        }
        let mut reads = std::vec::Vec::new();
        for (name, out) in made.reads.iter() {
            reads.extend_from_slice(&[name.kind as u8]);
            reads.extend_from_slice(&name.subject.0);
            reads.extend_from_slice(&out.0);
        }
        let row = (
            made.name,
            made.out,
            ContentId::of("blaze-cli order test payload", made.payload_bytes().unwrap()),
            ContentId::of("blaze-cli order test findings", &findings),
            ContentId::of("blaze-cli order test reads", &reads),
        );
        ANSWERED.with(|answered| answered.borrow_mut().push(row));
    }
}

/// The main thread's time making one task's job, by its kind: the parts of "jobs made".
fn made_row(kind: Kind) -> &'static str {
    match kind {
        Kind::FileItems => "jobs made: FileItems (main ns)",
        Kind::CrateScope => "jobs made: CrateScope (main ns)",
        Kind::BodyCheck => "jobs made: BodyCheck (main ns)",
        Kind::Signature => "jobs made: Signature (main ns)",
        Kind::Lint => "jobs made: Lint (main ns)",
        Kind::Script => "jobs made: Script (main ns)",
        _ => "jobs made: answered here (main ns)",
    }
}

fn row_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Units => "task Units",
        Kind::UnitGraph => "task UnitGraph",
        Kind::Fetch => "task Fetch",
        Kind::Surface => "task Surface",
        Kind::MacroModule => "task MacroModule",
        Kind::CrateScope => "task CrateScope",
        Kind::FileItems => "pooled FileItems (worker ns)",
        Kind::BodyCheck | Kind::Signature => BODIES,
        Kind::Lint => crate::lints::LINT_TASK,
        Kind::Script => "pooled Script (worker ns)",
        _ => "task of another kind",
    }
}

/// The record `made` of `task` in the profile's record rows (phases/byte_rows.rs): its kind's
/// count and payload bytes, and for a `CrateScope` record its sections, read from its head in
/// place (nothing copied). The largest is named only when it is the largest so far: a crate by
/// its name, a file's task by its path, any other by its task name.
fn tally(records: &mut crate::phases::byte_rows::Records, req: &Request<'_>, names: Option<&crate::names::Names<'_>>, task: &Task<'_>, made: &Made<'_>) {
    use crate::names::sections::{Section, sections};
    use crate::phases::byte_rows::Label;
    let label = || {
        let krate = names.and_then(|names| names.crate_at(task.name.subject).map(|at| names.crate_label(at)));
        match (task.name.kind, krate, req.file_of(task)) {
            (Kind::CrateScope, Some((name, false)), _) => Label::of(format_args!("{name}")),
            (Kind::CrateScope, Some((name, true)), _) => Label::of(format_args!("{name} (macro side)")),
            (_, _, Some(path)) => Label::of(format_args!("{}", core::str::from_utf8(path).unwrap_or("a path not UTF-8"))),
            _ => Label::of(format_args!("{}", task.name)),
        }
    };
    // Tallied as the old machine answers, before any put; only the engine releases a payload
    // and it tallies nothing here. A released one has no bytes to count: its record is counted
    // by the engine's ledger (engine_store.rs `Ledger`).
    let Ok(payload) = made.payload_bytes() else {
        return;
    };
    if task.name.kind == Kind::CrateScope && !payload.is_empty() {
        match sections(payload) {
            Ok(read) => records.scope(Section::ALL.map(|section| read.part(section).len()), payload.len(), label),
            Err(_) => records.scope_unread(),
        }
    }
    records.made(task.name.kind, payload.len(), label);
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
    store: &mut dyn Store<'_>,
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
    high.tasks.ended(task.name.kind, used);
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
        | Kind::Script
        | Kind::BodyCheck
        | Kind::Signature
        | Kind::Links
        | Kind::Native
        | Kind::Lint => true,
        // `Owners` is the request machine's own row (blaze-drive owners.rs), never a task; so is
        // `Given`, answered by the machine from the sweep and the ask's facts.
        Kind::ModuleScope
        | Kind::Group
        | Kind::Expansion
        | Kind::ImplBucket
        | Kind::Code
        | Kind::Test
        | Kind::Owners
        | Kind::Given => false,
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

/// The borrows stage's word in [`CHECK_STAGES`]: built only once the borrow check is trusted
/// (body_types.rs `BORROWS_ARE_ERRORS`); then not run when any body says its borrows were not
/// checked (`BORROWS_NOT_CHECKED`, counted by print.rs).
pub(crate) const BORROWS_STAGE: &str = "borrows";

/// The one list of the stages `check` is defined to run, in order, and whether this build runs
/// each, derived from the kinds this worker builds: the closing line's "NOT checked" and
/// `blaze explain 3` read it. When all are `true`, `check` can say `ok`. The third of each is
/// why a stage is not built, printed as a note before the closing line (main.rs `close`): a
/// "NOT checked" never stands without its reason (rule 19), even when no body was refused.
pub(crate) const CHECK_STAGES: [(&str, bool, &str); 6] = [
    ("manifests", builds(Kind::Units) && builds(Kind::UnitGraph), NOT_BUILT),
    ("syntax of items", builds(Kind::FileItems), NOT_BUILT),
    ("syntax inside bodies", builds(Kind::BodyCheck), NOT_BUILT),
    (NAMES_STAGE, builds(Kind::CrateScope), NOT_BUILT),
    // A body's types are its `BodyCheck` task's, over records, with the heads it reads made
    // in the task (body_types.rs). A body it could not check to the end is a
    // `TYPES_NOT_CHECKED` finding, which turns this stage to "NOT checked" (main.rs `close`).
    (TYPES_STAGE, builds(Kind::BodyCheck), NOT_BUILT),
    // Each body's moves and borrows are checked in its `BodyCheck` task (body_types.rs), as
    // notes until `BORROWS_ARE_ERRORS`: until then this stage is "NOT checked".
    (
        BORROWS_STAGE,
        crate::body_types::BORROWS_ARE_ERRORS && builds(Kind::BodyCheck),
        "the move and borrow check ran, but its findings stay notes until the flow census reads none false (`BORROWS_ARE_ERRORS`)",
    ),
];

/// [`CHECK_STAGES`]'s reason for a stage whose task this worker does not build.
const NOT_BUILT: &str = "this build of blaze does not build the task that runs it (work.rs `builds`)";

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
    store: &mut dyn Store<'_>,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    task: &Task<'_>,
    frame: &'f Frame,
    held: &'b Frame,
    scratch: &Frame,
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
                facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, facts.fetch_env.heap, frame, alloc)?);
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
                    crate::names::world(&cx, root, record.payload_bytes()?, Some(lock), Some(graph.member_dirs()), &facts.fetch_env)
                };
                match world {
                    Ok(names) => {
                        let parts = crate::names::parts(&names);
                        record.parts.extend(parts.iter().map(|(name, source)| Part { name: *name, locate: Locate::Source(*source), place: 0 }));
                        // Each crate's build-script run, named with its `CrateScope` part from the
                        // same row (ruling D115 (2)): a side nobody reaches names none.
                        let scripts = crate::scripts::parts(names.script_keys(), crate::scripts::sdk_identity(&facts.fetch_env));
                        record.parts.extend(scripts.iter().map(|(name, source)| Part { name: *name, locate: Locate::Source(*source), place: 0 }));
                        // Each script's module lookup, once a module (script_modules.rs, ruling
                        // D115 (c)): its source holds the graph's, so the module store's listing.
                        let mut named: Option<ContentId> = None;
                        for module in names.script_modules() {
                            if named != Some(module.subject) {
                                named = Some(module.subject);
                                let source = crate::script_modules::part_source(graph.source, module.subject);
                                record.parts.push(Part { name: crate::script_modules::name(module.subject), locate: Locate::Source(source), place: 0 });
                            }
                        }
                        // The crate list, one row a crate (drive.md, "The crate list is a
                        // record"): every other reader of the crates opens it in place, so an
                        // edit makes no world (graph_payload.rs). Freed when sealed (on the
                        // process's heap, handed from `main`: copied into the record next).
                        let list = match blaze_world::crate_list::encode(names.world(), &|at| names.script_name_of(at), facts.fetch_env.heap.alloc()) {
                            Ok(list) => list,
                            Err(error) => {
                                let words = bstd::mem::format_in!(held, "the crate list of the world could not be written: {error}");
                                return Ok(Did::FailedSaying(TaskFailure { name: task.name, why: Why::Refused { what: "the crate list", rule: "crate_list.rs" } }, held_str(words.as_str(), held)));
                            }
                        };
                        record.out = seal_graph(&mut record, &list);
                        crate::names::tell_public(store, &names);
                        facts.names = Some(names);
                    }
                    Err(error) => {
                        let words = bstd::mem::format_in!(held, "names not resolved: {error}");
                        record.find(crate::fetch::SOURCE_REFUSED, (0, 0), &[words.as_str()]);
                        // No world, no rows: the list is empty and its readers say so.
                        record.out = seal_graph(&mut record, &[]);
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
        Kind::FileItems | Kind::BodyCheck | Kind::Signature | Kind::Script | Kind::Lint => unreachable!("{:?} tasks run on the pool, in `answer_batch`", task.name.kind),
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
                facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, facts.fetch_env.heap, frame, alloc)?);
            }
            // A build script's module or a proc-macro crate's: the names world says which
            // (script_modules.rs, ruling D115 (c)); made here when no task made it first.
            if facts.names.is_none() {
                let Some(graph) = task.inputs.iter().find(|input| input.name == Name::unit_graph()) else {
                    let mut wants = Vec::with_capacity_in(1, held);
                    wants.push(Want { name: Name::unit_graph(), home: Name::unit_graph() });
                    return Ok(Did::Need(wants));
                };
                if let Err(what) = crate::body_types::names_world(&cx, root, store, facts, graph, frame, alloc)? {
                    return Ok(Did::Hole(TaskFailure { name: task.name, why: Why::Refused { what, rule: crate::script_modules::RULE } }, what));
                }
            }
            if let Some(module) = facts.names.as_ref().and_then(|names| names.script_module(task.name.subject)) {
                let wanted = crate::script_modules::Wanted { ran_as: module.ran_as, package: module.package, version: module.version, features: &module.features };
                return crate::script_modules::run(&cx, facts.fetch_env.home, wanted, task, held);
            }
            let lock = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref());
            let members = crate::names::member_dirs(facts.laid.as_ref());
            let place = crate::macros::Place { home: facts.fetch_env.home, checkout: facts.fetch_env.checkout, members, rows: &facts.rows };
            // The graph is read and decoded in the task's scratch, reset when it ends
            // (`run_one`): the macro list it makes is kept in `facts.macros`, never the decoded
            // graph (it was the request's frame: 11 MB on tempo, kept to the command's end).
            crate::macros::run(&cx, store, lock, &mut facts.macros, place, task, scratch, held)
        }
        Kind::CrateScope => {
            if facts.laid.is_none() {
                facts.laid = Some(crate::units::lay(&cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, facts.fetch_env.heap, frame, alloc)?);
            }
            let lock = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref()).map(|lockfile| lockfile.lock);
            let members = crate::names::member_dirs(facts.laid.as_ref());
            let side = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref()).map(|lockfile| crate::proc_runner::MacroSide {
                lock: lockfile,
                place: crate::macros::Place { home: facts.fetch_env.home, checkout: facts.fetch_env.checkout, members, rows: &facts.rows },
                heap: facts.fetch_env.heap,
            });
            crate::names::run(&cx, root, store, &mut facts.names, lock, members, &facts.fetch_env, side.as_ref(), task, held)
        }
        // A crate's link table: the `links` section of its `CrateScope` record, read in place.
        Kind::Links => crate::native_image::links(&cx, store, task, held),
        // The program's native image, from its crates' tables and build-script outputs.
        Kind::Native => crate::native_image::native(&cx, store, &facts.producer, facts.fetch_env.home, task, scratch, held, alloc),
        _ => unreachable!("`builds` said yes to {:?} and `run` has no arm for it", task.name.kind),
    }
}

/// What a `FileItems` task needs, apart from the checkout: all of it `Send`, so the task runs
/// on any worker of the pool.
pub(crate) struct Job<'a> {
    pub(crate) name: Name,
    /// The module slot's path, as the part named it.
    pub(crate) slot: &'a str,
    /// Where the sweep found it (`sweep::Found::flags`).
    pub(crate) flags: u8,
    pub(crate) edition: u8,
    pub(crate) plant: Plant,
    /// The content id the sweep gave the file, which the task's key holds: the bytes read are
    /// compared with it before any parse (`blaze_drive::verified`).
    pub(crate) expect: ContentId,
    /// Whether the command runs any lint rule (`Facts::lints`): then the file's outline
    /// lists its lint units as parts (lints.rs `parts`). In the `FileItems` kind's tool.
    pub(crate) lints: bool,
}

/// Why a task's job could not be made when the frame of one round's jobs is full: the task
/// fails by these words, the round goes on (rule 20).
const JOBS_FULL: &str = "the frame of one round's jobs is full: its tasks' paths and body lists are more than it holds";

/// The bodies of one file a batch asked for, as one job: the file's job (its name the first
/// body's, unused) and each body's task name with where its item starts (its part's place) and
/// the `Signature`s and `Surface` its task was handed (body_types/heads.rs `handed_reads`).
struct BodiesJob<'a> {
    file: Job<'a>,
    bodies: &'a [(Name, u32, &'a [(Name, ContentId)])],
    /// The crates its bodies' types read (body_types.rs).
    types: &'a crate::body_types::TypesJob<'a>,
    /// The process's heap, handed from `main` (`FetchEnv::heap`): the types checkers' worlds
    /// are made on it, freed as each body's check ends (a frame would keep every one).
    heap: &'static blaze_own::Heap,
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
    let expect = req.file_content_of(task).ok_or("a file's or a body's task whose file the sweep did not find")?;
    Ok(Job { name: task.name, slot, flags: facts.found.flags(slot.as_bytes()), edition: task.edition, plant: facts.plant, expect, lints: !facts.lints.is_empty() })
}

/// A file's outline as its `FileItems` record, worked in `scratch` and built in `into`: the
/// record, or why the task fails (an input this build refuses).
pub(crate) fn outline_file<A: Allocator + Copy>(cx: &Cx, root: &Dir<ReadWrite>, job: &Job<'_>, scratch: &Frame, into: A) -> Result<Result<Record<A>, Why>, Unread> {
    let (slot, flags, held) = (job.slot, job.flags, into);
    // A file read as data (`include_str!`, `include_bytes!`; data_include.rs): no Rust, no
    // outline; its record is its bytes' content id, so it moves exactly when they do and every
    // crate scope that read it is asked again.
    if crate::data_include::is_data(job.edition) {
        let mut bytes = Bytes::new_in(scratch, "an included data file");
        root.read_all(cx, &Path::new(slot)?, &mut bytes)?;
        let mut made = Record::new_in(job.name, held);
        made.out = crate::data_include::data_out(bytes.as_slice());
        return Ok(Ok(made));
    }
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
            let text = verified_text(job, bytes.as_slice())?;
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
            // Each file a literal `include_str!` / `include_bytes!` of this text reads (core's:
            // "located relative to the current file"): a part read as DATA, opened as the one
            // path it names (data_include.rs). A literal not read as a path here (an escape past
            // `\n`, absolute, out of the checkout) is no part: the names' read of it is refused
            // by name (`Outside` by the expansion, or the drive's unlisted part), never quiet.
            let dir = path.rfind('/').map_or("", |at| &path[..at]);
            for found in crate::data_include::data_includes(text, scratch).iter() {
                let mut child = Vec::new_in(scratch);
                child.extend_from_slice(dir.as_bytes());
                if path_literal(found.literal, scratch).and_then(|rel| join(&mut child, &rel)).is_err() {
                    continue;
                }
                let name = Name::file(&child);
                // One part a file: a second call of it, or a file also included as items.
                if parts.iter().any(|part: &Part| part.name == name) {
                    continue;
                }
                parts.push(Part { name, locate: Locate::File { edition: blaze_drive::record::DATA_FILE }, place: found.place });
                let at = path_bytes.len() as u32;
                path_bytes.extend_from_slice(&child);
                paths.push((name, at, path_bytes.len() as u32, false));
            }
            // The file's surface, its own part: what name resolution reads of it, never a body
            // (drive.md, "SPEC, the NAMES step"). A body edit leaves its source, so it stands.
            let surface = outline.surface();
            parts.push(Part { name: Name::new(Kind::Surface, job.name.subject), locate: Locate::Source(surface), place: 0 });
            // Each body its own part, keyed by its item's text alone (body_types.rs
            // `body_source`): an edit of one body re-parses and re-checks that one
            // ([`body_syntax`]). Its syntax errors are its record's alone. What it reads of its
            // file and of other files is in its read list (its file's `Surface`, each head's
            // `Signature`), not in its key (D114).
            let mut bodies = Vec::new_in(scratch);
            for item in outline.items() {
                if let Some(body) = checked_body(&outline, item) {
                    let name = body_name(job.name, &outline, item, text);
                    parts.push(Part { name, locate: Locate::Source(crate::body_types::body_source(item.hash)), place: item.range.start });
                    bodies.push((item.range.start + body.start, item.range.start + body.end));
                } else if expansion_entry(&outline, item, text) {
                    // An entry a macro takes, with no body of its own: its check covers the
                    // bodies its expansions write ([`expansion_entry`]), keyed by its text as
                    // a body is; what the expansion reads besides is in its read list.
                    let name = body_name(job.name, &outline, item, text);
                    parts.push(Part { name, locate: Locate::Source(crate::body_types::body_source(item.hash)), place: item.range.start });
                }
            }
            bodies.sort_unstable();
            // Each head its own part, a `Signature` (signature.rs), named by the file's real path
            // and the entry's chain, keyed by its head's bytes and the file's surface: a type
            // edit re-makes that one head, and moves only the bodies that read it.
            let subject = crate::signature::file_subject(path);
            for (index, item) in outline.items().iter().enumerate() {
                if crate::signature::is_signature_entry(&outline, item) {
                    let at = blaze_surface::ItemIndex(index as u32);
                    let name = crate::signature::signature_name(subject, &outline, item, text);
                    parts.push(Part { name, locate: Locate::Source(crate::signature::signature_source(&outline, at, text, surface)), place: item.range.start });
                }
            }
            // Each lint unit its own part (lints.rs), when the step runs a rule: keyed by its
            // own bytes, holes and scope, so an edit relints only the units whose key it moved.
            if job.lints
                && let Err(what) = crate::lints::parts(&outline, text, path, subject, erased.handle(), &mut parts)
            {
                return Ok(Err(Why::Refused { what, rule: "blaze-cli lints.rs, parts" }));
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
            Ok(Ok(Record { name: job.name, out, parts, findings, messages, payload, handed: Handed::None, paths, path_bytes, reads: Vec::new_in(held), covered: Vec::new_in(held), extras: Extras::none() }))
        }
    }
}

/// The domain of a `BodyCheck` part's subject ([`body_name`]).
const BODY_DOMAIN: &str = "blaze-cli body check name 2";

/// The form of a `BodyCheck` record's payload ([`seal_body_payload`], [`body_payload`]):
///
/// ```text
/// magic "BZB1" | count u32 LE | file subject [32] | text [32] | count x checker key [16] (BE)
/// ```
///
/// `file` is the subject of the body's file's `FileItems` name (`Name::file` of its path, the
/// condition's base), `text` its entry's text digest (`Item::hash`, what [`body_name`]'s part is
/// found by through `body_source`), and the keys the checker keys of every fn its check covered
/// (the entry's own fn and each fn declared in a body it reached), sorted, one each: a key's
/// big-endian bytes sort as the key. So run_ids.rs names a covered fn's `BodyCheck` from the
/// answer alone. A record of an older form is not misread: the payload's magic says its form,
/// and the `BodyCheck` tool holds the body checker's code (home.rs `tools`, `Row::BodyChecker`),
/// so a record written before this form is under another key: not found, and made again.
pub(crate) const BODY_PAYLOAD_MAGIC: [u8; 4] = *b"BZB1";
const BODY_PAYLOAD_HEAD: usize = 4 + 4 + 32 + 32;
const COVERED_KEY: usize = 16;

/// Seals `made`'s covered keys ([`Record::covered`]) into its payload in the form above: sorted
/// and deduplicated in place first (two owners reach one fn alike), so the bytes are one
/// function of the set (rule 25). Refused when the record already has a payload (rule 3).
pub(crate) fn seal_body_payload<A: Allocator + Copy>(made: &mut Record<A>, file: Name, text: ContentId) -> Result<(), Why> {
    if !matches!(made.payload_bytes(), Ok(bytes) if bytes.is_empty()) {
        return Err(Why::Refused { what: "a `BodyCheck` record sealed twice: it already has a payload (or had one, released)", rule: "blaze-cli work.rs, seal_body_payload" });
    }
    made.covered.sort_unstable();
    made.covered.dedup();
    let count = u32::try_from(made.covered.len()).map_err(|_| Why::Refused { what: "a body covering past 2^32 fns", rule: "blaze-cli work.rs, seal_body_payload" })?;
    write_body_payload(file, text, &made.covered, &mut made.payload, count);
    Ok(())
}

/// The form's bytes of (`file`, `text`, `keys`) onto `out`: `keys` sorted, one each, `count` of
/// them (the caller's checked length).
pub(crate) fn write_body_payload<A: Allocator>(file: Name, text: ContentId, keys: &[blaze_typeck::ItemKey], out: &mut Vec<u8, A>, count: u32) {
    out.reserve(BODY_PAYLOAD_HEAD + keys.len() * COVERED_KEY);
    out.extend_from_slice(&BODY_PAYLOAD_MAGIC);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&file.subject.0);
    out.extend_from_slice(&text.0);
    for key in keys {
        out.extend_from_slice(&key.0.to_be_bytes());
    }
}

/// A `BodyCheck` payload of the form above, checked once ([`body_payload`]) and read in place.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BodyPayload<'b> {
    /// The body's file's `FileItems` name.
    pub(crate) file: Name,
    /// Its entry's text digest.
    pub(crate) text: ContentId,
    keys: &'b [u8],
}

impl BodyPayload<'_> {
    /// The checker keys its check covered, sorted, one each.
    pub(crate) fn covered(&self) -> impl ExactSizeIterator<Item = blaze_typeck::ItemKey> + '_ {
        self.keys.chunks_exact(COVERED_KEY).map(|chunk| {
            let mut key = [0u8; COVERED_KEY];
            key.copy_from_slice(chunk);
            blaze_typeck::ItemKey(u128::from_be_bytes(key))
        })
    }
}

/// Why a payload is not a `BodyCheck` payload of the form above.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BodyPayloadError {
    /// Shorter than the head (an empty payload: a filed failure's record, which has none).
    Short { len: usize },
    /// Another form's magic.
    Magic { found: [u8; 4] },
    /// The head's count and the bytes disagree.
    Size { count: u32, len: usize },
    /// The keys are not sorted one each, at this key.
    Unsorted { at: usize },
}

impl core::fmt::Display for BodyPayloadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BodyPayloadError::Short { len } => write!(f, "a `BodyCheck` payload of {len} bytes, shorter than its {BODY_PAYLOAD_HEAD}-byte head (work.rs `body_payload`)"),
            BodyPayloadError::Magic { found } => write!(f, "a `BodyCheck` payload of form {:?}, not {:?} (work.rs `body_payload`)", found, BODY_PAYLOAD_MAGIC),
            BodyPayloadError::Size { count, len } => write!(f, "a `BodyCheck` payload naming {count} covered keys in {len} bytes (work.rs `body_payload`)"),
            BodyPayloadError::Unsorted { at } => write!(f, "a `BodyCheck` payload whose covered keys are not sorted one each at key {at} (work.rs `body_payload`)"),
        }
    }
}

/// `bytes` as a `BodyCheck` payload, every length and order checked here, once (rule 74).
pub(crate) fn body_payload(bytes: &[u8]) -> Result<BodyPayload<'_>, BodyPayloadError> {
    if bytes.len() < BODY_PAYLOAD_HEAD {
        return Err(BodyPayloadError::Short { len: bytes.len() });
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&bytes[..4]);
    if magic != BODY_PAYLOAD_MAGIC {
        return Err(BodyPayloadError::Magic { found: magic });
    }
    let count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let keys = &bytes[BODY_PAYLOAD_HEAD..];
    if keys.len() as u64 != u64::from(count) * COVERED_KEY as u64 {
        return Err(BodyPayloadError::Size { count, len: bytes.len() });
    }
    let word = |at: usize| {
        let mut w = [0u8; 32];
        w.copy_from_slice(&bytes[at..at + 32]);
        ContentId(w)
    };
    let (file, text) = (Name::new(Kind::FileItems, word(8)), word(40));
    if let Some(at) = (1..count as usize).find(|i| keys[(i - 1) * COVERED_KEY..i * COVERED_KEY] >= keys[i * COVERED_KEY..(i + 1) * COVERED_KEY]) {
        return Err(BodyPayloadError::Unsorted { at });
    }
    Ok(BodyPayload { file, text, keys })
}

/// Whether `item` is a body's own entry: a `fn`, `const` or `static` with a body, under none:
/// an item written inside a body is parsed with it, never twice. Its body range, entry-relative.
pub(crate) fn checked_body(outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item) -> Option<blaze_surface::Range> {
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

/// Whether a macro can take `item`, so that its expansions write bodies its entry's check
/// covers (body_types.rs `check`, covered.rs `written_by`): a macro call; an item with an
/// outer attribute that is not one of the language's inert ones (a derive, an attribute
/// macro); a top-level `fn main`, which a blaze edition's root gives `std::main` unasked
/// (D104; covered.rs `Root::Replaced`). A filter, never the answer: an entry it lets through
/// whose expansions wrote nothing covers nothing more.
pub(crate) fn may_expand(outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item, text: &str) -> bool {
    if item.kind() == ItemKind::MacroCall {
        // An `include!`'s items are a file's, which is a part with its own bodies' parts
        // (`outline_file`'s `MacroCall` arm): its expansion writes no body this entry covers.
        return match item.detail {
            ItemDetail::MacroCall { path: Some(call), .. } => !is_include(item.text(text, call)),
            _ => true,
        };
    }
    if item.parent.is_none() && item.kind() == ItemKind::Function && item.name.is_some_and(|name| item.text(text, name) == "main") {
        return true;
    }
    // A `cfg_attr` may give a derive (`#[cfg_attr(feature = "serde", derive(Serialize))]`):
    // it is never evaluated here, so it lets the entry through.
    outline.attrs(item).iter().any(|attr| {
        !attr.inner
            && match attr.kind {
                blaze_surface::AttrKind::CfgAttr => true,
                blaze_surface::AttrKind::Other => !inert_attribute(item.text(text, attr.range)),
                _ => false,
            }
    })
}

/// Whether an attribute's text (`#[repr(C)]`) is one of the language's built-in attributes
/// that no macro answers (the Reference, "Attributes", built-in attributes index): one that
/// writes no item. `derive`, `test` and every path the crate resolves to a macro are not.
fn inert_attribute(attr: &str) -> bool {
    const INERT: [&str; 30] = [
        "allow", "warn", "deny", "forbid", "expect", "deprecated", "must_use", "inline", "cold", "track_caller", "no_mangle", "export_name", "link_section", "link_name", "link",
        "used", "repr", "non_exhaustive", "automatically_derived", "macro_use", "doc", "path", "ignore", "should_panic", "target_feature", "naked", "no_link", "unsafe", "coverage",
        "debugger_visualizer",
    ];
    let inside = attr.trim_start_matches('#').trim_start().trim_start_matches('[').trim_start();
    let end = inside.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')).unwrap_or(inside.len());
    let path = &inside[..end];
    INERT.contains(&path) || ["rustfmt::", "clippy::", "diagnostic::", "rustc_"].iter().any(|tool| path.starts_with(tool))
}

/// Whether `item` is an entry with no body of its own whose `BodyCheck` covers the bodies
/// its expansions write (D122: a macro-written item's place is its expansion's): a
/// module-level macro call, or an item a derive or an attribute takes ([`may_expand`]),
/// where its `Signature` covers the heads they write (signature.rs `is_signature_entry`).
pub(crate) fn expansion_entry(outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item, text: &str) -> bool {
    checked_body(outline, item).is_none() && crate::signature::is_signature_entry(outline, item) && may_expand(outline, item, text)
}

/// A body's name: its file's subject and its entry's chain up to the file (each kind, name as
/// written and disambiguator), so it stands while other items move and its own text changes.
///
/// The chain is the ONE chain of an item (blaze-file-items chain.rs `item_chain`, rule 30), which
/// holds an unnamed entry's head. This function walked the chain itself without that head until
/// 2026-11-17, so two unnamed siblings with different heads (`write_p!();` beside `write_q!();`,
/// each with disambiguator 0) had ONE name, and the bodies stage kept one of the two calls'
/// tasks and dropped the other without a word (`two_macro_calls_side_by_side_are_two_bodies`).
pub(crate) fn body_name(file: Name, outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item, text: &str) -> Name {
    let mut h = blaze_ids::Hasher::new(BODY_DOMAIN);
    h.update(&file.subject.0).update(&blaze_file_items::item_chain(outline, item, text).0);
    Name::new(Kind::BodyCheck, h.finish())
}

/// The domain of an item's chain digest ([`item_chain`]).
const CHAIN_DOMAIN: &str = "blaze-cli item chain 2";

/// An item's identity in its file that no edit elsewhere moves (rule 71): the digest of its
/// chain up to the file (each kind, name as written and disambiguator), as [`body_name`] names
/// a body. A record that places a finding in an item holds this and the range from the item's
/// start; the printer finds the item's start in the file as it is today ([`item_by_chain`]).
pub(crate) fn item_chain(outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item, text: &str) -> u64 {
    // The one chain (blaze-file-items chain.rs, with an unnamed entry's head), under this
    // record's own domain and cut to the 64 bits a placed finding holds.
    let mut h = blaze_ids::Hasher::new(CHAIN_DOMAIN);
    h.update(&blaze_file_items::item_chain(outline, item, text).0);
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
pub(crate) fn parser_edition(edition: Edition) -> parser::Edition {
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
/// (unread, not text): `pooled_bodies` gives it to every body not yet answered. A body that
/// lacks a `Signature` it read is `None` in `did` and its wants are in `needs`.
#[expect(clippy::too_many_arguments, reason = "a pooled job's edge: its outcomes, its clock and its setup time")]
fn bodies_syntax(cx: &Cx, root: &Dir<ReadWrite>, job: &BodiesJob<'_>, scratch: &Frame, did: &mut BodiesDid, needs: &mut BodiesNeed, clock: &bstd::clock::Clock, setup_ns: &mut u64) -> Result<(), Unread> {
    let real = crate::sweep::real_path(job.file.slot, job.file.flags, scratch);
    let path = core::str::from_utf8(&real).map_err(|_| Unread::Text("a path is not UTF-8"))?;
    let mut bytes = Bytes::new_in(scratch, "a source file");
    root.read_all(cx, &Path::new(path)?, &mut bytes)?;
    let text = verified_text(&job.file, bytes.as_slice())?;
    if u32::try_from(text.len()).is_err() {
        return Err(Unread::Text("a source file past 4 GiB"));
    }
    let edition = Edition::from_byte(job.file.edition).ok_or(Unread::Text("a body's home file has no edition byte"))?;
    let erased = blaze_own::Erased::new(scratch);
    let outline = blaze_file_items::outline(text, edition, erased.handle());
    // The items that have a checked body, by where each starts: a body asked for is found by
    // search, not by a pass over the file's items a body.
    let mut starts: Vec<(u32, u32), &Frame> = Vec::new_in(scratch);
    // The entries a macro takes that have no body of their own ([`expansion_entry`]), by start.
    let mut taken: Vec<(u32, u32), &Frame> = Vec::new_in(scratch);
    for (index, item) in outline.items().iter().enumerate() {
        if checked_body(&outline, item).is_some() {
            starts.push((item.range.start, index as u32));
        } else if expansion_entry(&outline, item, text) {
            taken.push((item.range.start, index as u32));
        }
    }
    starts.sort_unstable();
    taken.sort_unstable();
    // The bodies' types (body_types.rs): each owner's front end over records, made once a job.
    // Its setup (each owner's readers and world over the records) is a row of its own
    // ([`BODY_SETUP`]): D114 piece (a) is judged by it.
    // The heads that have a `Signature` part, by where each starts (signature.rs).
    let mut heads: Vec<(u32, u32), &Frame> = Vec::new_in(scratch);
    for (index, item) in outline.items().iter().enumerate() {
        if crate::signature::is_signature_entry(&outline, item) {
            heads.push((item.range.start, index as u32));
        }
    }
    heads.sort_unstable();
    // COARSE(D114 e): a body reads its own file's `Surface` WHOLE, for what the file's text
    // around the bodies covered until D114 (its `use`s, `macro_rules!`, attributes): an item
    // added to the file re-checks its bodies. It becomes the cells the body looked up when the
    // names solver's cells land (drive.md, D114 rulings, "Signatures"). It is read at the out
    // its task was handed, the `Surface` task's: never this text's source, which is the
    // part's key, not its out (the drive's `ReadNotHanded` refused it, 2026-11-17); a body not
    // handed it needs it of its file's `FileItems`, which lists it.
    let surface = Name::new(Kind::Surface, Name::file(job.file.slot.as_bytes()).subject);
    let set_up = clock.now();
    let mut types = crate::body_types::FileTypes::new(cx, root, job.types, text, scratch, job.heap.alloc());
    if job.file.plant == Plant::HeadReadLeftOut {
        types.plant_head_read_left_out();
    }
    *setup_ns = clock.now().since(set_up).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    for (name, place, handed) in job.bodies.iter().copied() {
        #[cfg(test)]
        answers_for_test::CHECKED.with(|checked| checked.borrow_mut().push((name, std::string::String::from(job.file.slot), place)));
        let (used, limit) = (scratch.peak() as u64, scratch.reserved() as u64);
        if name.kind == Kind::Signature {
            let found = heads.binary_search_by_key(&place, |(start, _)| *start).ok().map(|at| blaze_surface::ItemIndex(heads[at].1));
            let made = unwinding(|| match found {
                Some(at) => Ok(types.signature(&outline, at, text, edition, name)),
                None => Err(Why::Refused { what: "a `Signature` part whose file holds no head at its place", rule: "blaze-cli work.rs, bodies_syntax" }),
            });
            did.push(Some(match made {
                Ok(done) => done,
                Err(Panicked) => Err(Why::Panicked { used, limit }),
            }));
            continue;
        }
        let found = starts.binary_search_by_key(&place, |(start, _)| *start).ok().map(|at| &outline.items()[starts[at].1 as usize]);
        // No body at its place: an entry a macro takes, whose record starts with no syntax of
        // its own (its call's tokens are the outline's, its expansion's the names step's).
        let entry = match found {
            Some(_) => None,
            None => taken.binary_search_by_key(&place, |(start, _)| *start).ok().map(|at| &outline.items()[taken[at].1 as usize]),
        };
        // The `Signature`s the body read and was not handed: its task answers them as a need.
        let mut wants: std::vec::Vec<Want, Global> = std::vec::Vec::new_in(Global);
        let checked = unwinding(|| {
            let mut done = match entry {
                Some(item) => {
                    let mut made = Record::new_in(name, Global);
                    made.out = item.hash;
                    made
                }
                None => body_syntax(&outline, found, text, edition, name, Global)?,
            };
            let found = found.or(entry);
            // A body with a syntax error is that error's: its types are not asked.
            if let Some(item) = found
                && done.findings.is_empty()
            {
                types.check(&outline, item, text, &mut done, handed, &mut wants);
                match handed.binary_search_by_key(&surface, |(name, _)| *name) {
                    Ok(at) => done.reads.push(handed[at]),
                    Err(_) => wants.push(Want { name: surface, home: Name::file(job.file.slot.as_bytes()) }),
                }
                // One read a name, in name order: two owners name the same heads.
                done.reads.sort_unstable_by_key(|(name, _)| *name);
                done.reads.dedup_by_key(|(name, _)| *name);
            }
            // Every body's record says its file, its text and the fns its check covered (none
            // when its syntax stopped it): run_ids.rs names each covered fn's `BodyCheck` by it.
            if let Some(item) = found {
                seal_body_payload(&mut done, Name::file(job.file.slot.as_bytes()), item.hash)?;
            }
            Ok(done)
        });
        // A checked body that lacks a `Signature` answers the need, not its record: the record
        // would read that head at no out its task was handed.
        if matches!(checked, Ok(Ok(_))) && !wants.is_empty() {
            needs.push((did.len(), wants));
            did.push(None);
            continue;
        }
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
    /// The bytes read are not the content id the task's key holds (`verified_text`): the task
    /// fails `Why::Changed`, puts nothing, and the command's answer is refused naming the file.
    Changed,
}

/// The text of `bytes`, which `job`'s task read from its file, once their content id is the one
/// its key was made from: the only way a task reading a file by path gets text to parse (the
/// [`blaze_drive::verified::Verified`] proof). A difference is [`Unread::Changed`]: no re-read,
/// no retry (rule 3).
fn verified_text<'b>(job: &Job<'_>, bytes: &'b [u8]) -> Result<&'b str, Unread> {
    let id = |read: &[u8]| ContentId::of(crate::sweep::FILE_DOMAIN, read);
    let verified = blaze_drive::verified::verify(job.name, bytes, job.expect, id, blaze_drive::Plant::None).map_err(|_| Unread::Changed)?;
    core::str::from_utf8(verified.bytes()).map_err(|_| Unread::Text("a source file is not UTF-8"))
}

impl From<Unread> for CliError {
    fn from(error: Unread) -> CliError {
        match error {
            Unread::Path(error) => CliError::Path(error),
            Unread::Fs(error) => CliError::Fs(error),
            Unread::Text(what) => CliError::Text { what },
            Unread::Changed => CliError::Text { what: "a file changed while the command ran; run again" },
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

/// A `FileItems` task's outcome on the pool: its record in `Global` (its payload moved into the
/// batch's answer once joined, [`Record::into_held`]), or why it failed; its frame's peak; its
/// time.
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
        Ok(Err(Unread::Text(what))) => Ok(Err(Why::Refused { what, rule: TEXT_REFUSED_RULE })),
        Ok(Err(Unread::Changed)) => Ok(Err(Why::Changed)),
        Ok(Err(error)) => Err(error),
        Err(Panicked) => Ok(Err(Why::Panicked { used: used as u64, limit: limit as u64 })),
    };
    Pooled { did, used, ns: clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)) }
}

/// Each body's outcome of a file's job, in the order asked; `None` once taken by its task.
type BodiesDid = Vec<Option<Result<Record<Global>, Why>>, Global>;

/// The bodies of a file's job that answered a need (each by its place in the job's bodies):
/// the `Signature`s each read and was not handed (body_types/heads.rs `HeadReads`).
type BodiesNeed = Vec<(usize, std::vec::Vec<Want, Global>), Global>;

/// A file's body job's outcome on the pool: each body's record or why it failed, or what it
/// needs; what stopped the command, if the file could not be read at all (a path or read
/// error); the frame's peak; the job's time.
struct PooledBodies {
    did: BodiesDid,
    needs: BodiesNeed,
    stopped: Option<Unread>,
    used: usize,
    ns: u64,
    /// Of `ns`, the time its setup took ([`BODY_SETUP`]).
    setup_ns: u64,
}

/// `job` on a pool worker, in the worker's task frame, as [`pooled`] runs a file's outline: a
/// file that is not text, or a panic outside any one body (the read, the outline), fails each
/// body not yet answered, by name; the pool never sees it.
fn pooled_bodies(cx: &Cx, root: &Dir<ReadWrite>, job: &BodiesJob<'_>, clock: &bstd::clock::Clock) -> PooledBodies {
    let began = clock.now();
    let scratch = cx.frame();
    let mut did: BodiesDid = Vec::with_capacity_in(job.bodies.len(), Global);
    let mut needs: BodiesNeed = Vec::new_in(Global);
    let mut setup_ns = 0;
    let ran = unwinding(|| bodies_syntax(cx, root, job, scratch, &mut did, &mut needs, clock, &mut setup_ns));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    let (rest, stopped) = match ran {
        Ok(Ok(())) => (None, None),
        Ok(Err(Unread::Text(what))) => (Some(Why::Refused { what, rule: TEXT_REFUSED_RULE }), None),
        Ok(Err(Unread::Changed)) => (Some(Why::Changed), None),
        Ok(Err(error)) => (None, Some(error)),
        Err(Panicked) => (Some(Why::Panicked { used: used as u64, limit: limit as u64 }), None),
    };
    if let Some(why) = rest {
        while did.len() < job.bodies.len() {
            did.push(Some(Err(why)));
        }
    }
    PooledBodies { did, needs, stopped, used, ns: clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)), setup_ns }
}

/// The lint units of one file a batch asked for, as one job: the file's job (its name the first
/// unit's, unused) and each unit's task name, base name and source; the step's rule set.
struct LintsJob<'a> {
    file: Job<'a>,
    units: &'a [(Name, Name, ContentId)],
    rules: blaze_lint::RuleSet,
}

/// `job` on a pool worker, in the worker's task frame, as [`pooled_bodies`] runs a file's bodies:
/// a file that is not text, or a panic outside the rules (the read, the outline), fails each unit
/// not yet answered, by name; the pool never sees it.
fn pooled_lints(cx: &Cx, root: &Dir<ReadWrite>, job: &LintsJob<'_>, clock: &bstd::clock::Clock) -> PooledBodies {
    let began = clock.now();
    let scratch = cx.frame();
    let mut did: BodiesDid = Vec::with_capacity_in(job.units.len(), Global);
    let ran = unwinding(|| file_lints(cx, root, job, scratch, &mut did));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    let (rest, stopped) = match ran {
        Ok(Ok(())) => (None, None),
        Ok(Err(Unread::Text(what))) => (Some(Why::Refused { what, rule: "blaze-cli lints.rs" }), None),
        Ok(Err(Unread::Changed)) => (Some(Why::Changed), None),
        Ok(Err(error)) => (None, Some(error)),
        Err(Panicked) => (Some(Why::Panicked { used: used as u64, limit: limit as u64 }), None),
    };
    if let Some(why) = rest {
        while did.len() < job.units.len() {
            did.push(Some(Err(why)));
        }
    }
    PooledBodies { did, needs: Vec::new_in(Global), stopped, used, ns: clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)), setup_ns: 0 }
}

/// One file's lint job: its text read once and checked against the id its units' keys were made
/// from, outlined once, and its asked units linted (lints.rs `lint_units`).
fn file_lints(cx: &Cx, root: &Dir<ReadWrite>, job: &LintsJob<'_>, scratch: &Frame, did: &mut BodiesDid) -> Result<(), Unread> {
    let real = crate::sweep::real_path(job.file.slot, job.file.flags, scratch);
    let path = core::str::from_utf8(&real).map_err(|_| Unread::Text("a path is not UTF-8"))?;
    let mut bytes = Bytes::new_in(scratch, "a source file");
    root.read_all(cx, &Path::new(path)?, &mut bytes)?;
    let text = verified_text(&job.file, bytes.as_slice())?;
    if u32::try_from(text.len()).is_err() {
        return Err(Unread::Text("a source file past 4 GiB"));
    }
    let edition = Edition::from_byte(job.file.edition).ok_or(Unread::Text("a lint unit's home file has no edition byte"))?;
    let erased = blaze_own::Erased::new(scratch);
    let outline = blaze_file_items::outline(text, edition, erased.handle());
    let file = crate::signature::file_subject(path);
    crate::lints::lint_units(&outline, text, path, file, job.units, &job.rules, erased.handle(), did).map_err(Unread::Text)
}

/// A `CrateScope` task's job, made on the main thread (names/scope.rs `prepare`), or `None`
/// when the task has no resolution to make now (a need, a failure, a crate made already): it
/// is then run here, in `answer_batch`, as before.
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
fn scope_job<'f: 'j, 'j>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store<'_>,
    facts: &mut Facts<'f, impl crate::sweep::Stamps>,
    task: &Task<'_>,
    frame: &'f Frame,
    round: &mut crate::names::ScopeRound<'j>,
    alloc: blaze_own::Alloc<'_>,
) -> Result<Option<&'j crate::names::ScopeJob<'j>>, CliError> {
    if facts.laid.is_none() {
        facts.laid = Some(crate::units::lay(cx, root, &facts.stamps, &mut facts.rows, facts.fetch_env.toolchain, facts.fetch_env.heap, frame, alloc)?);
    }
    let lock = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref()).map(|lockfile| lockfile.lock);
    let members = crate::names::member_dirs(facts.laid.as_ref());
    // An answer without a resolution is dropped here and made again by the serial step: the
    // round's frame holds its few words until the round ends.
    let held = round.frame();
    let side = facts.laid.as_ref().and_then(|laid| laid.lock.as_ref()).map(|lockfile| crate::proc_runner::MacroSide {
        lock: lockfile,
        place: crate::macros::Place { home: facts.fetch_env.home, checkout: facts.fetch_env.checkout, members, rows: &facts.rows },
        heap: facts.fetch_env.heap,
    });
    match crate::names::prepare(cx, root, store, &mut facts.names, lock, members, &facts.fetch_env, side.as_ref(), task, round, held)? {
        crate::names::Prepared::Resolve(job) => Ok(Some(job)),
        crate::names::Prepared::Answer(_) | crate::names::Prepared::Made => Ok(None),
    }
}

/// A `CrateScope` resolution's outcome on the pool: the resolution, or why it failed (a panic,
/// a full frame); the frame's peak; the time.
struct PooledScope {
    did: Result<crate::names::Resolution, Why>,
    used: usize,
    ns: u64,
    /// What its frame held by stretch (names/metered.rs); none after a panic.
    bytes: crate::names::metered::ScopeBytes,
}

/// `job` on a pool worker, in the worker's task frame, as [`pooled`] runs a file's outline: a
/// panic or a full frame fails this crate alone (rule 20); the pool never sees it.
fn pooled_scope(cx: &Cx, root: &Dir<ReadWrite>, job: &crate::names::ScopeJob<'_>, clock: &bstd::clock::Clock) -> PooledScope {
    let began = clock.now();
    let scratch = cx.frame();
    let ran = unwinding(|| crate::names::resolve(cx, root, job, scratch));
    let (used, limit) = (scratch.peak(), scratch.reserved());
    let (did, bytes) = match ran {
        Ok((resolution, bytes)) => (Ok(resolution), bytes),
        Err(Panicked) => (Err(Why::Panicked { used: used as u64, limit: limit as u64 }), crate::names::metered::ScopeBytes::default()),
    };
    PooledScope { did, used, ns: clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)), bytes }
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

/// Whether a macro call's path as written (`include`, `::core::include`) is core's `include!`,
/// whose items are a file's ([`included_file`]): the one place that names it.
fn is_include(call: &str) -> bool {
    let call = call.trim();
    let call = call.strip_prefix("::").unwrap_or(call);
    matches!(call, "include" | "core::include" | "std::include")
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
    if !is_include(call) {
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

    /// The `at`th generated id: distinct for each `at`.
    fn id(at: usize, salt: u8) -> ContentId {
        let mut id = [salt; 32];
        id[..8].copy_from_slice(&(at as u64).to_le_bytes());
        ContentId(id)
    }

    /// A pool worker's record of a successful task with no finding: three parts and two reads,
    /// as a body's or a scope's has.
    fn generated(at: usize) -> Record<Global> {
        let mut made = Record::new_in(Name::new(Kind::BodyCheck, id(at, 1)), Global);
        made.out = id(at, 2);
        for part in 0..3u8 {
            made.parts.push(Part { name: Name::new(Kind::Surface, id(at, 10 + part)), locate: Locate::Source(id(at, 20 + part)), place: u32::from(part) * 64 });
        }
        for read in 0..2u8 {
            made.reads.push((Name::new(Kind::Surface, id(at, 30 + read)), id(at, 40 + read)));
        }
        made
    }

    /// The memory a successful task costs past its batch: 50,000 generated tasks, each with
    /// three parts, two reads and no finding, answered in batches of 256 (blaze-drive's
    /// `MOST_BATCH`, private there) the way
    /// `answer_batch` answers them (`into_held` into the batch's frame, `keep_failed`, the
    /// outcomes built by `with_outcomes` and handed to the step, the frame reset after) in a
    /// frame of the real size (`TASK_FRAME`). The batch frame's high-water mark after the batch
    /// that passes 1,000 tasks equals the mark after 50,000, and the failures table took no
    /// byte. Prints "test work::tests::a_successful_task_keeps_nothing_past_its_batch ... ok".
    /// Control: drop `held.reset()` below and the mark grows with the tasks (the assertion
    /// prints both marks); copy the records into a frame that outlives the batch, as R4 feared,
    /// and the same happens.
    #[test]
    fn a_successful_task_keeps_nothing_past_its_batch() {
        let mut held = Frame::with_reserve(TASK_FRAME, Tag::Other).expect("a batch's frame of the real size");
        let mut failed = Failures::new_in(blaze_own::testing::heap().alloc());
        let tasks = 50_000usize;
        let (mut next, mut answered, mut mark) = (0usize, 0usize, 0usize);
        let mut mark_past_1000 = None;
        while next < tasks {
            let end = (next + 256).min(tasks);
            {
                let mut did: Vec<(blaze_drive::Ticket, Did<'_>), &Frame> = Vec::with_capacity_in(end - next, &held);
                let mut took: Vec<u64, &Frame> = Vec::with_capacity_in(end - next, &held);
                for at in next..end {
                    let record = generated(at);
                    let name = record.name;
                    let done = record.into_held(&held).map_or_else(|_| held_refused(name), Did::Done);
                    keep_failed(&mut failed, &done, Some("src/lib.rs")).expect("a successful task keeps nothing");
                    did.push((blaze_drive::Ticket(at as u64), done));
                    took.push(1);
                }
                answered += with_outcomes(&did, &took, &held, |results| results.iter().filter(|outcome| matches!(outcome.ran, Ran::Done(_))).count());
            }
            mark = mark.max(held.peak());
            held.reset();
            next = end;
            if next >= 1_000 && mark_past_1000.is_none() {
                mark_past_1000 = Some(mark);
            }
        }
        assert_eq!(answered, tasks, "every generated task answered done");
        assert_eq!(mark_past_1000, Some(mark), "the batch frame's mark after 1,000 tasks, against after 50,000");
        assert!(failed.is_empty() && failed.bytes() == 0, "a successful task kept {} failures' bytes", failed.bytes());
    }

    /// 1,000 failed tasks with their words: each kept in the command's one table of failures, on
    /// the heap, so the command's frame takes nothing; the closing note prints the first
    /// `SAME_SHOWN` with their file and words and counts the rest. Prints "test
    /// work::tests::failed_tasks_keep_their_words_on_the_heap ... ok". Its lines are
    ///   "a task failed in src/lib.rs: <the failure>: its words 0" (four, to "its words 3"),
    ///   "and 996 more task(s) failed the same way".
    /// Control: keep the words in the command's frame (`held_str(words, frame)`, as before) and
    /// its mark grows past zero; drop the count in `failed_notes` and 1,000 lines come out.
    #[test]
    fn failed_tasks_keep_their_words_on_the_heap() {
        let command = Frame::with_reserve(TASK_FRAME, Tag::Other).expect("the command's frame");
        let mut failed = Failures::new_in(blaze_own::testing::heap().alloc());
        let words: std::vec::Vec<std::string::String> = (0..1_000).map(|at| format!("its words {at}")).collect();
        for (at, words) in words.iter().enumerate() {
            let done = Did::FailedSaying(TaskFailure { name: Name::new(Kind::BodyCheck, id(at, 1)), why: Why::NotBuilt }, words.as_str());
            keep_failed(&mut failed, &done, Some("src/lib.rs")).expect("the heap takes a failure");
        }
        drop(words);
        assert_eq!(failed.len(), 1_000);
        assert_eq!(failed.get(999).and_then(|kept| kept.detail), Some("its words 999"), "the words outlive the task's own");
        let lines = Frame::with_reserve(1 << 20, Tag::Other).expect("the drawing's frame");
        let mut said = std::vec::Vec::new();
        failed_notes(&failed, &lines, &mut |line| {
            said.push(std::string::String::from(line));
            Ok(())
        })
        .expect("the notes");
        assert_eq!(said.len(), SAME_SHOWN + 1, "{said:#?}");
        for (at, line) in said.iter().take(SAME_SHOWN).enumerate() {
            assert!(line.starts_with("a task failed in src/lib.rs: ") && line.ends_with(&format!(": its words {at}")), "{line}");
        }
        assert_eq!(said[SAME_SHOWN], "and 996 more task(s) failed the same way");
        assert_eq!(command.peak(), 0, "the command's frame took a failed task's bytes");
    }

    /// A build script's refusal with no SDK (scripts.rs `run`, `Shared::no_sdk`: its key holds
    /// "no SDK") is filed as the task's record: the failure's out and every input it was handed
    /// as a read. So it stands, the certificate with it, until an SDK resolves and moves the key
    /// (tempo, check-g043851: this refusal was a hole, so no certificate and 37 s of reruns on a
    /// no-op). Control: the planted bug files it with no reads.
    /// Expected: "test work::tests::a_script_refusal_is_filed_with_its_inputs ... ok".
    #[test]
    fn a_script_refusal_is_filed_with_its_inputs() {
        let held = Frame::with_reserve(TASK_FRAME, Tag::Other).expect("a frame");
        let a = blaze_own::testing::heap().alloc();
        let name = Name::new(Kind::Script, id(1, 1));
        let mut inputs = Vec::new_in(a.clone());
        inputs.push(Input { name: Name::new(Kind::Fetch, id(2, 1)), at: blaze_drive::Key::default(), out: id(3, 1) });
        let task = Task {
            name,
            base: name,
            condition: blaze_drive::Condition::ANY,
            key: blaze_drive::Key::default(),
            source: id(4, 1),
            class: blaze_drive::Class::Foreground,
            entry: 0,
            home: name,
            place: 0,
            file: None,
            edition: 0,
            inputs,
            failed: Vec::new_in(a.clone()),
            members: Vec::new_in(a),
        };
        let failure = TaskFailure { name, why: Why::Refused { what: "a native build script with no native setting", rule: crate::scripts::RULE } };
        assert!(filed(&failure, &task), "a refusal of a task handed no hole is filed");
        let made = filed_record(&task, &failure, Some("no macOS SDK"), Plant::None, &held);
        assert_eq!(made.out, failed_out());
        assert_eq!(made.reads.as_slice(), &[(Name::new(Kind::Fetch, id(2, 1)), id(3, 1))], "the record reads its inputs");
        // Control: with no reads it would stand after an input moved.
        let planted = filed_record(&task, &failure, Some("no macOS SDK"), Plant::FiledWithoutReads, &held);
        assert!(planted.reads.is_empty(), "control: the planted bug kept its reads");
    }

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
