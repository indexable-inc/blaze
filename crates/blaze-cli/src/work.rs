//! What a task makes and the pieces the engine's jobs share (drive.md, "Tasks and Need").
//!
//! The engine runs every task (engine_check.rs, engine_start.rs); what stays here is what its
//! jobs share: a task's [`Record`] and [`Did`], a file's outline ([`outline_file`]), the body
//! and chain names of an outline's items, the closing note's failures ([`Failures`]) and the
//! stages `check` runs ([`CHECK_STAGES`]).

use blaze_drive::{Input, Kind, Locate, Name, Part, TaskFailure, Want, Why};
use crate::store::Store;
use blaze_ids::ContentId;
use blaze_own::Vec;
use core::alloc::Allocator;
use std::alloc::Global;
use blaze_surface::{Edition, ItemDetail, ItemKind};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame, TASK_FRAME};
use bstd::path::Path;

use crate::CliError;

/// The finding number of a syntax error: the outline's, rule-free (a lint has its rule's number).
pub const SYNTAX: u16 = 1;
/// The finding number of a module found at both its places, `x.rs` and `x/mod.rs` (rustc's
/// E0761). Its arguments are the two paths.
pub const MODULE_AT_BOTH: u16 = 4;

const OUT_DOMAIN: &str = "blaze-cli parts out 1";
pub(crate) const FILE_SURFACE_PHYSICAL_DOMAIN: &str = "blaze-cli file surface with physical path 1";

/// The finding number of a task's typed failure, filed as its answer: its one argument is the
/// failure's words.
pub const TASK_FAILED: u16 = 16;

/// The out of every filed failure: a reader handed an input with this out was handed
/// a failure, never a payload, and fails `Waited` on it.
pub(crate) fn failed_out() -> ContentId {
    ContentId::of("blaze-cli a task's typed failure 1", b"failed")
}

/// The finding number of a task not run because an input it reads could not be made, the
/// input's own record holding the finding that says why (body_types.rs `Prepared::NotRun`).
/// Its one argument is the cause's words. It is no error of its own: the command prints the
/// root finding once and one count of the tasks it stopped (not_checked/tasks.rs).
pub const NOT_RUN: u16 = 23;

/// The finding number of a task that failed only because a task it needs failed (`Why::Waited`,
/// filed like any failure). Its words are the needed task's name and the waiting
/// crate (not_checked/waited.rs `words`). It is no error of its own: the command prints the
/// root failure once with a count of what hangs from it (not_checked/waited.rs `groups`).
pub const WAITED: u16 = 24;

/// The finding number of a file whose text this build refuses (not UTF-8, past 4 GiB): the
/// user's error at that file, never the command's (rules 19 and 20). Its one argument is why
/// (blaze-report frame/words.rs, "file-not-text"); its file is its subject.
pub const FILE_NOT_TEXT: u16 = 25;

/// A file blaze cannot take as text: its outline's record, with the [`FILE_NOT_TEXT`] finding
/// and no parts, its out a failure's so each reader waits on it and the rest goes on. Its words
/// are `what` alone: the file is its subject, printed once beside them (print.rs
/// `finding_about`), never repeated in the words.
fn not_text<A: Allocator + Copy>(name: Name, held: A, what: &str) -> Record<A> {
    let mut made = Record::new_in(name, held);
    made.find(FILE_NOT_TEXT, (0, 0), &[what]);
    made.out = failed_out();
    made
}

/// What one task made, in the allocator `A`: a frame on the coordinator's thread ([`Made`]), or
/// `Global` on a pool worker, whose frame ends with its task (its payload moved into the
/// batch's answer once joined: [`Record::into_held`]).
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
    /// The individual answers this task read from other records.
    pub(crate) cells: Vec<blaze_drive::CellRead, A>,
    /// The semantic facts published by the bodies this entry covers.
    pub(crate) body_facts: Vec<(blaze_drive::Cell, u64), A>,
    /// A `BodyCheck`'s covered bodies: the checker key of each fn its check covered (the
    /// entry's own fn and each fn declared in a body it reached, at any depth), filed by
    /// body_types.rs `FileTypes::check` in the order reached; empty for every other kind.
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
            cells: Vec::new_in(alloc),
            body_facts: Vec::new_in(alloc),
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
            cells: copied_in(&self.cells, frame)?,
            body_facts: copied_in(&self.body_facts, frame)?,
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
            cells: copied_in(&self.cells, frame)?,
            body_facts: Vec::new_in(frame), // Sealed in the worker's payload.
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
    /// filed, a hole the next command runs again; printed as `FailedSaying` is.
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
/// The most a task's scratch and a batch's frame hold, and a bug to plant (the tests' controls).
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
}

impl Sizes {
    /// What a command runs with: a task's frame for each.
    pub const USUAL: Sizes = Sizes { scratch: TASK_FRAME, batch: TASK_FRAME, plant: Plant::None };
}

/// `text` copied into `frame`, held as long as it is.
pub(crate) fn held_str<'f>(text: &str, frame: &'f Frame) -> &'f str {
    frame.alloc_str(text)
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
/// failures (R4's list, wave 2311). A task that did not fail adds
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
    /// One file's body job on the pool, in its own frame.
    pub bodies: usize,
    /// Every task frame's used bytes at its end, by the task's kind: the profile's
    /// `task frame bytes` rows.
    pub tasks: crate::phases::byte_rows::TaskFrames,
    /// The largest pooled crate scope frame's bytes by the stretch of the solve that took
    /// them (names/metered.rs): the profile's `scope frame by stretch` row.
    pub scope_bytes: crate::names::metered::ScopeBytes,
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

/// A panic caught at an edge: its message went to the error stream by the panic hook.
pub(crate) struct Panicked;

/// `work`, with a panic inside it caught: the one home of the catch, for a task's edge (an
/// engine job's) and the command's (`main.rs`, `command`). Rule 20: a panic (an allocation its
/// frame refused, a bug) fails that piece alone. What `work` borrowed is read again only for
/// what a stopped piece of work leaves true (a frame's peak, the phases marked).
pub(crate) fn unwinding<T>(work: impl FnOnce() -> T) -> Result<T, Panicked> {
    unwind_refusals();
    std::panic::catch_unwind(core::panic::AssertUnwindSafe(work)).map_err(|_| Panicked)
}

/// Whether this worker builds tasks of `kind`: the one place that says so, read by
/// [`CHECK_STAGES`].
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

/// What a `FileItems` task needs, apart from the checkout: all of it `Send`, so the task runs
/// on any worker of the pool.
pub(crate) struct Job<'a> {
    pub(crate) name: Name,
    /// The module slot's path, as the part named it.
    pub(crate) slot: &'a str,
    /// Where the sweep found it (`sweep::Found::flags`).
    pub(crate) flags: u8,
    pub(crate) edition: u8,
    /// The content id the sweep gave the file, which the task's key holds: the bytes read are
    /// compared with it before any parse (`blaze_drive::verified`).
    pub(crate) expect: ContentId,
    /// The rule set the command runs (lints.rs `of_root`), when it lints: then the file's outline
    /// lists its lint units as parts, keyed for that set (lints.rs `parts_in`: `variant-count`'s
    /// whole-file fact gathered only when it runs) and its file's one `Lint` part over them.
    /// `None`: no lint part. The set is in the `FileItems` kind's tool (home.rs `tools`).
    pub(crate) lints: Option<blaze_lint::RuleSet>,
}

/// A file's outline as its `FileItems` record, worked in `scratch` and built in `into`: the
/// record, or why the task fails (an input this build refuses).
pub(crate) fn outline_file<A: Allocator + Copy>(cx: &Cx, root: &Dir<ReadWrite>, job: &Job<'_>, scratch: &Frame, into: A) -> Result<Result<Record<A>, Why>, Unread> {
    let (slot, flags, held) = (job.slot, job.flags, into);
    // A file read as data (`include_str!`, `include_bytes!`; data_include.rs): no Rust, no
    // outline; its record is its bytes' content id, so it moves exactly when they do and every
    // crate scope that read it is asked again.
    if crate::data_include::is_data(job.edition) {
        let mut made = Record::new_in(job.name, held);
        // Not there when the sweep probed it: an answer of its own (data_include.rs
        // `absent_out`), no finding; the call that reads it reports it at the call. The slot's
        // probe is in this task's key, so creating the file enters it again.
        if flags & crate::sweep::ABSENT != 0 {
            made.out = crate::data_include::absent_out();
            return Ok(Ok(made));
        }
        let mut bytes = Bytes::new_in(scratch, "an included data file");
        root.read_all(cx, &Path::new(slot)?, &mut bytes)?;
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
            let mut bytes = Bytes::new_in(scratch, "a source file");
            root.read_all(cx, &Path::new(path)?, &mut bytes)?;
            let text = match verified_text(job, bytes.as_slice()) {
                Ok(text) if u32::try_from(text.len()).is_ok() => text,
                Ok(_) => return Ok(Ok(not_text(job.name, held, "a source file past 4 GiB"))),
                Err(Unread::Text(what)) => return Ok(Ok(not_text(job.name, held, what))),
                Err(other) => return Err(other),
            };
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
            // A `mod` a macro call's expansion writes (`cfg_if!`, `cfg_block!`, `mod $n;`) is no
            // part here: only expansion knows it, under the unit's cfg. The crate's names read
            // its file, outline it on their worker with this code (engine_check.rs
            // `outline_found`), and it is filed after them (engine_check.rs `serve_found`, D161).
            // Each file a literal `include_str!` / `include_bytes!` of this text reads (core's:
            // "located relative to the current file"), and each a literal `include!` in a body
            // reads (one expression, data_include.rs; an item-position one is a module part
            // above and skipped below): a part read as DATA, opened as the one
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
            // Name resolution records physical file paths. Its read of this slot's surface
            // must move when the same text is found at the other module path.
            let file = crate::signature::SignatureFile::new(path.as_bytes());
            let surface = blaze_ids::Hasher::new(FILE_SURFACE_PHYSICAL_DOMAIN).id(&file.subject()).id(&outline.surface()).finish();
            parts.push(Part { name: Name::new(Kind::Surface, job.name.subject), locate: Locate::Source(surface), place: 0 });
            // Each body its own part, keyed by its item's text alone (body_types.rs
            // `body_source`): an edit of one body re-parses and re-checks that one.
            // Its syntax errors are its record's alone. What it reads of its
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
            // and the entry's chain, keyed by its head's bytes, nothing of its siblings: a type
            // edit re-makes that one head, and moves only the bodies that read it.
            for (index, item) in outline.items().iter().enumerate() {
                if crate::signature::is_signature_entry(&outline, item) {
                    let at = blaze_surface::ItemIndex(index as u32);
                    let name = file.entry(&outline, item, text);
                    parts.push(Part { name, locate: Locate::Source(crate::engine_heads::outline_head_source(&outline, at, text)), place: item.range.start });
                }
            }
            // Each lint unit its own part (lints.rs), when the command lints: keyed by its
            // own text (each child one mark, no child's offset or length) and scope
            // (lints.rs `Keyed`), so an edit relints only the unit it is in; then the file's one
            // `Lint` part, the file's task, over them (lints.rs `file_part`). Sorted by name
            // below: engine_lints.rs `part_place` searches them so.
            if let Some(rules) = job.lints.as_ref()
                // The file stands as its own crate root: this record is keyed by the file's bytes
                // alone (rule 21), and no crate root's text reaches a `Job`; engine_lints.rs reads
                // the same marks, so the two keys agree.
                && let Err(what) =
                    crate::lints::parts_in(&outline, text, path, file.subject(), crate::lints::marks_of(text, text), rules, erased.handle(), &mut parts)
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
            Ok(Ok(Record {
                name: job.name, out, parts, findings, messages, payload, handed: Handed::None, paths, path_bytes,
                reads: Vec::new_in(held), cells: Vec::new_in(held), body_facts: Vec::new_in(held),
                covered: Vec::new_in(held), extras: Extras::none(),
            }))
        }
    }
}

/// The domain of a `BodyCheck` part's subject ([`body_name`]).
const BODY_DOMAIN: &str = "blaze-cli body check name 2";

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
    chain_word(blaze_file_items::item_chain(outline, item, text))
}

/// [`item_chain`] of a STORED outline's entry `at` (signature.rs `stored_chain`: the same chain
/// over the record's columns): what a lint job reads, handed its planner's outline record and
/// never outlining the file again (engine_lints.rs `lint_text`).
pub(crate) fn stored_item_chain(view: blaze_surface::FileItemsView<'_>, at: blaze_surface::ItemIndex) -> u64 {
    chain_word(crate::signature::stored_chain(view, at))
}

/// A chain under this record's domain, cut to the 64 bits a placed finding holds.
fn chain_word(chain: blaze_ids::ContentId) -> u64 {
    let mut h = blaze_ids::Hasher::new(CHAIN_DOMAIN);
    h.update(&chain.0);
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
    let verified = blaze_drive::verified::verify(job.name, bytes, job.expect, id, blaze_drive::verified::Plant::None).map_err(|_| Unread::Changed)?;
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
    use bstd::mem::{TASK_FRAME, Tag};
    use bstd::test::BUDGET;

    use super::*;

    /// The `at`th generated id: distinct for each `at`.
    fn id(at: usize, salt: u8) -> ContentId {
        let mut id = [salt; 32];
        id[..8].copy_from_slice(&(at as u64).to_le_bytes());
        ContentId(id)
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
            let failure = TaskFailure { name: Name::new(Kind::BodyCheck, id(at, 1)), why: Why::NotBuilt };
            failed.keep(failure, Some(b"src/lib.rs"), Some(words.as_str())).expect("the heap takes a failure");
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
