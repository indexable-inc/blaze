//! Task kinds (decision D157): what one kind of task is to the ready driver (ready.rs), as a
//! contract every stage of blaze-cli is ported to. A kind says who it is and where it stands
//! ([`KindFacts`]: data, never an execution order), plans what a merged grower or a changed file
//! asks for ([`TaskKind::plan`]), and runs one task ([`TaskKind::run`]) against the one host it
//! may read through ([`Recorded`]), returning a result, the cells it still needs, or a refusal
//! ([`Ran`]). It never waits (bottom.md 2.2): a read the host cannot answer yet makes the run a
//! [`Ran::Need`], and the driver runs the task again from the top once those cells are final.
//!
//! The invariants, each stated here once:
//!
//! - **K1. The host is the read list.** A task reads a cell or a record only through its
//!   [`Recorded`] host, which records each cell once with the digest it gave. The read list a
//!   result carries ([`ReadList`]) is the host's, made by the one sort-and-dedup constructor in
//!   this module; no kind can build one (states.md item 4, the forms' "recording host").
//! - **K2. Only final cells are answered.** The host answers a cell only when the driver says its
//!   definer can no longer move it this request (ready.rs R1). Any other read is refused with
//!   [`NotYet`] and remembered; the run must then end in [`Ran::Need`] with the host's own
//!   [`Recorded::need`], and a result after a refused read is a refusal
//!   ([`Refusal::ResultAfterRefusal`]), never a result built on a guess (rule 3).
//! - **K3. A result's cells are sorted, each once.** [`Defines::sorted`] is the only way to make
//!   a result's defines and refuses a cell twice ([`Refusal::DefinedTwice`]).
//! - **K4. A kind's facts are data.** [`KindFacts`] says the stage the kind's tasks sit in and the
//!   earliest stage its growth lands in; the driver orders reads and gates entry by them, and
//!   nothing runs because of them. A registry holds each kind once ([`KindsError::Twice`]).
//!
//! How a result stands (states.md item 4) is one value, [`Outcome`]: made, with its cells and
//! how it stands, or failed, with how the failure stands; its fields are private, so a row's
//! FAILED and OWED bits come from one place.

#![blaze::sans_io]

use core::cell::RefCell;
use core::fmt;

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, Key, Kind, Name};
use blaze_id::ContentId;
use blaze_own::{SharedAlloc, Vec};

use crate::engine::{CellId, Done, Made, Planned, SVec, Stage, Stands, Task};
use crate::overlay::RowFacts;

/// The most cells one [`Needs`] holds (rule 18): a task that needs more returns the first
/// `NEEDS_CAP` and is run again, needing the rest then.
pub const NEEDS_CAP: usize = 16;

/// The cell a [`Recorded::row`] refusal names its row by: a need is waited on by its row
/// (`read.of`), so the cell only says that the row's facts, not one of its cells, were asked.
pub const ROW_CELL: Cell = Cell([0xff; 16]);

/// What a kind is to the driver (K4): its identity, the stage its tasks sit in (what `Stage`
/// orders in the phase driver, here a read's order and nothing else), and the earliest stage
/// a merged task of it grows tasks into, when it grows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KindFacts {
    pub kind: Kind,
    pub stage: Stage,
    pub grows_into: Option<Stage>,
}

/// What a kind is asked to plan.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ask {
    /// The tasks a changed file's text plans (engine.rs invariant 1): the sweep's question.
    File { file: Name },
    /// The whole growth of `by`, merged this request (engine.rs invariant 11): every task its
    /// result names, each with `by` as its file. Asked once for each merged task of a kind
    /// whose facts say it grows.
    Grown { by: Name },
}

/// Why a kind refused to give a result: a typed error naming the task, which stops the
/// request (rule 3). A task that fails on its input gives [`Outcome::failed`] instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The kind has no task, or no grower, by this name.
    Unknown { name: Name },
    /// The kind cannot answer this ask (a kind that plans no files, or grows nothing).
    CannotPlan { kind: Kind, ask: Ask },
    /// K3: a result defines one cell twice.
    DefinedTwice { name: Name, cell: Cell },
    /// K1: one cell was given with two digests to one task: its source moved under it.
    ReadTwoDigests { name: Name, read: CellId },
    /// K2: a task asked for a result after the host refused one of its reads.
    ResultAfterRefusal { name: Name, read: CellId },
    /// A Need of more than [`NEEDS_CAP`] cells.
    NeedsTooMany { name: Name },
    /// A record made inline was said to be owed: a made record is never owed (engine.rs
    /// invariant 13).
    MadeOwed { name: Name, by: Name },
    /// The kind's host stopped outside the engine (a store, a stage's setup): its typed error
    /// is the host's, kept by the kind under `name` and returned by the driver's caller in
    /// place of this refusal (rule 19: the reason is never dropped for this summary).
    Host { name: Name },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Unknown { name } => write!(f, "the kind has no task {name:?}"),
            Refusal::CannotPlan { kind, ask } => write!(f, "kind {} cannot plan {ask:?}", kind.name()),
            Refusal::DefinedTwice { name, cell } => write!(f, "K3: {name:?} defines {cell:?} twice"),
            Refusal::ReadTwoDigests { name, read } => write!(f, "K1: {name:?} was given {read:?} with two digests"),
            Refusal::ResultAfterRefusal { name, read } => {
                write!(f, "K2: {name:?} gave a result after its read of {read:?} was refused; it must return Need")
            }
            Refusal::NeedsTooMany { name } => write!(f, "{name:?} needs more than {NEEDS_CAP} cells at once"),
            Refusal::MadeOwed { name, by } => write!(f, "the record {name:?} made by {by:?} is said to be owed; a made record never is"),
            Refusal::Host { name } => write!(f, "{name:?}'s host stopped outside the engine; its kind holds the typed error"),
        }
    }
}

impl core::error::Error for Refusal {}

/// A read the host refused (K2): the cell, whose definer has not merged or may still move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotYet {
    pub read: CellId,
}

/// The cells a task read and the digest each held, sorted by cell, each once (K1). Made only
/// by [`ReadList::sorted`], which only [`Recorded::reads`] calls.
#[derive(PartialEq, Eq, Debug)]
pub struct ReadList<'a> {
    reads: SVec<'a, (CellId, ContentId)>,
}

impl<'a> ReadList<'a> {
    /// The one constructor: sorted by cell and deduplicated; one cell with two digests is
    /// refused. Sorted is one valid read order (engine.rs invariant 9).
    fn sorted(name: Name, mut reads: SVec<'a, (CellId, ContentId)>) -> Result<ReadList<'a>, Refusal> {
        reads.sort_unstable_by_key(|(id, _)| *id);
        if let Some(pair) = reads.windows(2).find(|pair| pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1) {
            return Err(Refusal::ReadTwoDigests { name, read: pair[0].0 });
        }
        reads.dedup_by_key(|(id, _)| *id);
        Ok(ReadList { reads })
    }

    pub fn as_slice(&self) -> &[(CellId, ContentId)] {
        &self.reads
    }

    pub fn len(&self) -> usize {
        self.reads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
    }
}

/// The cells a result defines and their digests, sorted by cell, each once (K3).
#[derive(PartialEq, Eq, Debug)]
pub struct Defines<'a> {
    cells: SVec<'a, (Cell, ContentId)>,
}

impl<'a> Defines<'a> {
    /// The one constructor: sorted by cell; a cell twice is refused, whatever its digests.
    pub fn sorted(name: Name, mut cells: SVec<'a, (Cell, ContentId)>) -> Result<Defines<'a>, Refusal> {
        cells.sort_unstable_by_key(|(cell, _)| *cell);
        if let Some(pair) = cells.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(Refusal::DefinedTwice { name, cell: pair[0].0 });
        }
        Ok(Defines { cells })
    }

    pub fn as_slice(&self) -> &[(Cell, ContentId)] {
        &self.cells
    }
}

/// How a failed result stands (states.md item 4; engine.rs invariant 7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    /// A function of the task's key alone (a parse refusal of its own text).
    ByKey,
    /// A function of its key and its reads (a type error on what it read).
    ByReads,
    /// Right for this request only, for a cause outside the checkout ([`Owed`]): entered again by
    /// the next request that reaches it ([`Stands::OwedNextRequest`]) and never certified
    /// ([`Outcome::certifies`]: D187, "a failed fetch must never certify").
    Owed(Owed),
}

/// Why a result is owed: a cause outside the checkout's text, so nothing it read can say when
/// it stands. Each is entered again by the next request; none is ever certified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owed {
    /// The run unwound (rule 20): the host it read through was lost with the unwind.
    Panicked,
    /// A file of the checkout could not be read (not a refusal of its text: the read failed).
    Unread,
    /// A fetch failed or was refused (the network, a missing home, a signature not checked).
    Fetch,
    /// The machine refused (memory, a sandbox, a process limit).
    Machine,
}

impl Failure {
    fn stands(self) -> Stands {
        match self {
            Failure::ByKey => Stands::ByKey,
            Failure::ByReads => Stands::ByReads,
            Failure::Owed(_) => Stands::OwedNextRequest,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum Of<'a> {
    Made { out: ContentId, defines: Defines<'a>, stands: Stands },
    Failed { out: ContentId, failure: Failure },
}

/// A task's result and how it stands: made, with its cells, or failed, with how the failure
/// stands; and the host's read list either way (a failure stands on its reads too). Fields
/// private: a `Done`'s `failed` flag and a buffer's owed name are made from it in one place.
#[derive(PartialEq, Eq, Debug)]
pub struct Outcome<'a> {
    of: Of<'a>,
    reads: ReadList<'a>,
}

impl<'a> Outcome<'a> {
    /// A made result: its out, its cells, and how it stands ([`Stands::ByReads`] for a result
    /// of its reads, [`Stands::ByKey`] for one of its key, [`Stands::OwedNextRequest`] for one
    /// right for this request only).
    pub fn made(out: ContentId, defines: Defines<'a>, stands: Stands, reads: ReadList<'a>) -> Outcome<'a> {
        Outcome { of: Of::Made { out, defines, stands }, reads }
    }

    /// A failed result: its out (what the failure's finding is a function of), how it stands,
    /// and what it read. It defines nothing.
    pub fn failed(out: ContentId, failure: Failure, reads: ReadList<'a>) -> Outcome<'a> {
        Outcome { of: Of::Failed { out, failure }, reads }
    }

    /// A run that never returned a result (it panicked: rule 20): failed as a fault, so owed and
    /// entered again by the next request, with an empty read list, since the host it read through
    /// was lost with the unwind and a fault stands on nothing it read. Only the driver's shell
    /// makes one (shell.rs `fault`): a kind's own failure is [`Outcome::failed`] over its host.
    pub(crate) fn fault(out: ContentId, alloc: SharedAlloc<'a>) -> Outcome<'a> {
        Outcome { of: Of::Failed { out, failure: Failure::Owed(Owed::Panicked) }, reads: ReadList { reads: Vec::new_in(alloc) } }
    }

    /// Whether a certificate may cover it: made or failed on its key or its reads. An owed
    /// result (a failure [`Failure::Owed`], or a made one standing [`Stands::OwedNextRequest`])
    /// is right for this request only, so a certificate over it would replay a fault as an
    /// answer (D187). The one question a certifier asks; nothing certifies around it.
    pub fn certifies(&self) -> bool {
        match &self.of {
            Of::Made { stands, .. } => *stands != Stands::OwedNextRequest,
            Of::Failed { failure, .. } => !matches!(failure, Failure::Owed(_)),
        }
    }

    /// The engine's form of it: the result `name` gives under `key`, and how it stands.
    pub(crate) fn into_done(self, name: Name, key: Key, findings: u32) -> (Done<'a>, Stands) {
        let alloc = *self.reads.reads.allocator();
        let reads = self.reads.reads;
        match self.of {
            Of::Made { out, defines, stands } => (Done { name, key, out, defines: defines.cells, reads, findings, failed: false }, stands),
            Of::Failed { out, failure } => (Done { name, key, out, defines: Vec::new_in(alloc), reads, findings, failed: true }, failure.stands()),
        }
    }
}

/// The cells a task still needs (K2): non-empty by construction, sorted, each once, at most
/// [`NEEDS_CAP`].
#[derive(PartialEq, Eq, Debug)]
pub struct Needs<'a> {
    cells: SVec<'a, CellId>,
}

impl<'a> Needs<'a> {
    /// A need of `first`.
    pub fn new(first: CellId, alloc: SharedAlloc<'a>) -> Needs<'a> {
        let mut cells = Vec::new_in(alloc);
        cells.push(first);
        Needs { cells }
    }

    /// Adds `cell`; a cell already held is held once. `Err` when [`NEEDS_CAP`] are held.
    pub fn add(&mut self, cell: CellId) -> Result<(), NeedsFull> {
        match self.cells.binary_search(&cell) {
            Ok(_) => Ok(()),
            Err(_) if self.cells.len() >= NEEDS_CAP => Err(NeedsFull),
            Err(at) => {
                self.cells.insert(at, cell);
                Ok(())
            }
        }
    }

    pub fn cells(&self) -> &[CellId] {
        &self.cells
    }

    /// The first cell, by cell order: every `Needs` has one.
    pub fn first(&self) -> CellId {
        self.cells[0]
    }
}

/// [`Needs::add`] past [`NEEDS_CAP`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NeedsFull;

/// What a run gives back.
#[derive(PartialEq, Eq, Debug)]
pub enum Ran<'a> {
    /// The task's result. Its records made inline are on the run's [`Out`].
    Done(Outcome<'a>),
    /// Cells the host refused (K2): the driver parks the task on their producers and runs it
    /// again from the top when they are final. Everything the run wrote to its `Out` is dropped.
    Need(Needs<'a>),
    /// The kind refused to give a result: the request stops (rule 3).
    Failed(Refusal),
}

/// A record a task made inline (engine.rs "The design" 3), before its maker is known.
struct MadeRecord<'a> {
    planned: Planned,
    key: Key,
    outcome: Outcome<'a>,
    findings: u32,
}

/// What a run writes beside its result: its findings and the records it made inline. One a
/// run, made by the thin driver, dropped on a [`Ran::Need`].
pub struct Out<'a> {
    findings: u32,
    made: SVec<'a, MadeRecord<'a>>,
}

impl<'a> Out<'a> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> Out<'a> {
        Out { findings: 0, made: Vec::new_in(alloc) }
    }

    /// The result's findings: what its row counts and a print shows (engine.rs invariant 6).
    pub fn set_findings(&mut self, findings: u32) {
        self.findings = findings;
    }

    pub fn findings(&self) -> u32 {
        self.findings
    }

    /// The run's allocator: what its result and made records are built in (the driver's, handed
    /// at [`Out::new_in`]), so a kind needs no second handle to it (rule 75).
    pub fn alloc(&self) -> SharedAlloc<'a> {
        *self.made.allocator()
    }

    /// A record made inline under `planned` and `key` (the key its kind's tool gives its
    /// source; the merge refuses another). Its read list comes from a host of its own
    /// ([`Recorded::record`]); the maker must read one of its cells (engine.rs invariant 12).
    pub fn make(&mut self, planned: Planned, key: Key, outcome: Outcome<'a>, findings: u32) {
        self.made.push(MadeRecord { planned, key, outcome, findings });
    }

    /// The made records in the engine's form, made `by` the task this run was of. A record said
    /// to be owed is refused.
    pub(crate) fn into_made(self, by: Name, into: &mut SVec<'a, Made<'a>>) -> Result<u32, Refusal> {
        let findings = self.findings;
        for one in self.made.into_iter() {
            let (done, stands) = one.outcome.into_done(one.planned.name, one.key, one.findings);
            if stands == Stands::OwedNextRequest {
                return Err(Refusal::MadeOwed { name: one.planned.name, by });
            }
            into.push(Made { planned: one.planned, done, by });
        }
        Ok(findings)
    }
}

/// What a host asks of the request (the driver implements it, ready.rs): today's digests, the
/// finality of a definer for a reader (R1), and a standing record by key.
pub trait Source {
    /// The cell's digest today ([`blaze_drive::key::ABSENT`] when nothing defines it).
    fn digest(&self, id: &CellId) -> ContentId;
    /// Whether `reader` may read `of`'s cells now: nothing this request can still move them
    /// (ready.rs R1).
    fn is_final(&self, reader: Name, of: Name) -> bool;
    /// `name`'s row under `key`, standing today (engine.rs `View::hit`).
    fn hit(&self, name: Name, key: Key) -> Option<RowFacts>;
    /// `name`'s row as it stands today, under any key (engine.rs `View::row`): read for its
    /// facts (whether it failed), never as a hit. Asked only once `name` is final for the reader
    /// ([`Recorded::row`]).
    fn row(&self, name: Name) -> Option<RowFacts>;
}

struct Log<'a> {
    reads: SVec<'a, (CellId, ContentId)>,
    refused: Option<Needs<'a>>,
    /// A refused read beyond [`NEEDS_CAP`]: the need is cut there, and the run says so.
    overflow: bool,
}

/// The one host a task reads through (K1, K2): every answered read is recorded, every refused
/// one remembered, and the result's read list and the run's need are taken from it.
pub struct Recorded<'r, 'a> {
    source: &'r dyn Source,
    reader: Name,
    alloc: SharedAlloc<'a>,
    log: RefCell<Log<'a>>,
}

impl<'r, 'a> Recorded<'r, 'a> {
    /// A host for one run of `reader` over `source`.
    pub fn new(source: &'r dyn Source, reader: Name, alloc: SharedAlloc<'a>) -> Recorded<'r, 'a> {
        Recorded { source, reader, alloc, log: RefCell::new(Log { reads: Vec::new_in(alloc), refused: None, overflow: false }) }
    }

    /// The task this host reads for.
    pub fn reader(&self) -> Name {
        self.reader
    }

    /// The source under the host, unrecorded: for the simulations' model tasks alone, which
    /// compute over today's digests and are then replayed through this host (ready_sim.rs).
    #[cfg(test)]
    pub(crate) fn source(&self) -> &'r dyn Source {
        self.source
    }

    /// The cell's digest, recorded, when its definer is final (K2); else the read is refused
    /// and remembered for [`Recorded::need`].
    pub fn cell(&self, id: &CellId) -> Result<ContentId, NotYet> {
        if !self.source.is_final(self.reader, id.of) {
            return Err(self.refuse(id));
        }
        let digest = self.source.digest(id);
        let mut log = self.log.borrow_mut();
        if !log.reads.iter().any(|(read, _)| read == id) {
            log.reads.push((*id, digest));
        }
        Ok(digest)
    }

    /// A record needed inline, found standing under `key`, asked by the cell of it the task
    /// reads (`read.of` is the record). Answered only when the record is final (K2); a hit
    /// records nothing itself: the task reads the record's cells through [`Recorded::cell`]
    /// (engine.rs invariant 12). `Ok(None)`: no such row stands, and the task makes it.
    pub fn hit(&self, read: &CellId, key: Key) -> Result<Option<RowFacts>, NotYet> {
        if !self.source.is_final(self.reader, read.of) {
            return Err(self.refuse(read));
        }
        Ok(self.source.hit(read.of, key))
    }

    /// The cell's digest when its definer is final (K2), NOT recorded: for a kind whose code
    /// places its own read list after reading (a body's row, blaze-cli engine_bodies.rs
    /// `row_of`: reads widened to their grains, one dropped once it is answered) and hands that
    /// list to [`Recorded::placed`], which holds each placed read to the digest asked here. A
    /// refused peek is remembered for [`Recorded::need`] as a refused [`Recorded::cell`] is.
    pub fn peek(&self, id: &CellId) -> Result<ContentId, NotYet> {
        if !self.source.is_final(self.reader, id.of) {
            return Err(self.refuse(id));
        }
        Ok(self.source.digest(id))
    }

    /// `of`'s row as it stands today, for its facts (whether it failed), NOT recorded: answered
    /// only when `of` is final (K2), else refused and remembered for [`Recorded::need`], the
    /// need naming the row by its cell [`ROW_CELL`] (a park waits on `read.of` alone, ready.rs
    /// `park`). What the kind reads of the row's cells it reads as cells; the row's facts are a
    /// function of the row's reads, which a moved definer moves (engine.rs invariant 7).
    pub fn row(&self, of: Name) -> Result<Option<RowFacts>, NotYet> {
        if !self.source.is_final(self.reader, of) {
            return Err(self.refuse(&CellId { of, cell: ROW_CELL }));
        }
        Ok(self.source.row(of))
    }

    /// A read list the run placed itself (see [`Recorded::peek`]), recorded as the result's
    /// reads (K1). Each read must be final for the reader (K2): one that is not is refused and
    /// remembered for [`Recorded::need`], so the caller asks `need()` before `reads()`. Each
    /// must carry today's digest: one that does not was read before its source moved, and is
    /// [`Refusal::ReadTwoDigests`] (K1), the request stopped by name. A read placed twice with
    /// one digest is kept once by [`Recorded::reads`]. Costs one finality question and one
    /// digest a read; nothing is scanned per read.
    pub fn placed(&self, reads: &[(CellId, ContentId)]) -> Result<(), Refusal> {
        for (id, digest) in reads.iter() {
            if !self.source.is_final(self.reader, id.of) {
                self.refuse(id);
                continue;
            }
            if self.source.digest(id) != *digest {
                return Err(Refusal::ReadTwoDigests { name: self.reader, read: *id });
            }
        }
        self.log.borrow_mut().reads.extend_from_slice(reads);
        Ok(())
    }

    /// Remembers a refused read for [`Recorded::need`]: past [`NEEDS_CAP`] the need is cut and
    /// the overflow kept, so a result after it is refused too.
    fn refuse(&self, id: &CellId) -> NotYet {
        let mut guard = self.log.borrow_mut();
        let log = &mut *guard;
        if let Some(needs) = log.refused.as_mut() {
            if needs.add(*id).is_err() {
                log.overflow = true;
            }
        } else {
            log.refused = Some(Needs::new(*id, self.alloc));
        }
        NotYet { read: *id }
    }

    /// Records the read of a cell of a record this run made inline ([`Out::make`]): the digest is
    /// the one the run made, so it is final by construction and asks the request nothing (K1:
    /// still the host's list).
    pub fn read_made(&self, id: CellId, digest: ContentId) {
        let mut log = self.log.borrow_mut();
        if !log.reads.iter().any(|(read, _)| *read == id) {
            log.reads.push((id, digest));
        }
    }

    /// The cells refused so far, taken: `Some` means the run must return [`Ran::Need`] with it.
    pub fn need(&self) -> Option<Needs<'a>> {
        self.log.borrow_mut().refused.take()
    }

    /// The read list for the result, taken (K1). Refused after a refused read (K2).
    pub fn reads(&self) -> Result<ReadList<'a>, Refusal> {
        let mut log = self.log.borrow_mut();
        if let Some(needs) = log.refused.as_ref() {
            return Err(Refusal::ResultAfterRefusal { name: self.reader, read: needs.first() });
        }
        if log.overflow {
            return Err(Refusal::NeedsTooMany { name: self.reader });
        }
        let reads = core::mem::replace(&mut log.reads, Vec::new_in(self.alloc));
        ReadList::sorted(self.reader, reads)
    }

    /// A host for a record this task makes inline: its own read list, the same finality as its
    /// maker's (the record is made inside its maker's run).
    pub fn record(&self) -> Recorded<'r, 'a> {
        Recorded::new(self.source, self.reader, self.alloc)
    }
}

/// One kind of task (D157): the contract a stage of blaze-cli is ported to. `C` is what a
/// worker hands each run (the driver pool's `Cx`; `()` in the simulation).
pub trait TaskKind<'a, C: ?Sized = ()>: Sync {
    /// Who it is and where it stands (K4).
    fn facts(&self) -> KindFacts;

    /// The tasks `ask` names, appended to `out` (each a [`Planned`] of today's text).
    fn plan(&self, ask: Ask, out: &mut SVec<'a, Planned>) -> Result<(), Refusal>;

    /// One run of `task`, reading only through `src` (K1). Never waits: a refused read ends
    /// the run in [`Ran::Need`] (K2).
    fn run(&self, cx: &C, src: &Recorded<'_, 'a>, task: &Task, out: &mut Out<'a>) -> Ran<'a>;
}

/// A registry refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KindsError {
    /// K4: a kind registered twice.
    Twice { kind: Kind },
}

impl fmt::Display for KindsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KindsError::Twice { kind } => write!(f, "K4: kind {} is registered twice", kind.name()),
        }
    }
}

impl core::error::Error for KindsError {}

/// Each kind's facts by [`Kind::index`]: what the driver orders and gates by (K4). `Copy`, so
/// the driver holds it with no borrow of the kinds themselves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KindTable {
    facts: [Option<KindFacts>; key::KINDS],
    /// The kinds the request's plan holds whose tasks the driver never runs ([`KindTable::stood`]).
    stood: [bool; key::KINDS],
}

impl KindTable {
    /// The table of `facts`, each kind once.
    pub fn new(facts: &[KindFacts]) -> Result<KindTable, KindsError> {
        let mut table = KindTable { facts: [None; key::KINDS], stood: [false; key::KINDS] };
        for one in facts.iter() {
            let slot = &mut table.facts[one.kind.index()];
            if slot.is_some() {
                return Err(KindsError::Twice { kind: one.kind });
            }
            *slot = Some(*one);
        }
        Ok(table)
    }

    /// The facts of `kind`, `None` when no kind of it is registered.
    pub fn of(&self, kind: Kind) -> Option<KindFacts> {
        self.facts[kind.index()]
    }

    /// The table with `kinds` STOOD: kinds the request's plan holds (the caller's planner makes
    /// their tasks on the other driver) whose tasks this driver never runs, so their rows are
    /// served as the last answer stored them (ready.rs R1). What standing adds is the absent row:
    /// a name of a stood kind with no row that the request neither plans nor owes is no task of
    /// the plan, so its cells are absent, and final (engine.rs `stands_or_absent`), as the phase
    /// driver reads them. A kind no table stands and none registers is planned by nothing, and a
    /// Need of it stays unmeetable (R5). A kind registered too is run, never stood.
    pub fn stood(mut self, kinds: &[Kind]) -> KindTable {
        for kind in kinds.iter() {
            self.stood[kind.index()] = self.facts[kind.index()].is_none();
        }
        self
    }

    /// Whether `kind` is stood ([`KindTable::stood`]).
    pub fn is_stood(&self, kind: Kind) -> bool {
        self.stood[kind.index()]
    }

    /// Whether a task of `kind`, merged, may grow tasks at or before `stage`.
    pub fn grows_by(&self, kind: Kind, stage: Stage) -> bool {
        self.of(kind).and_then(|facts| facts.grows_into).is_some_and(|into| into <= stage)
    }

    /// The latest stage a kind sits in whose growth may land at or before `stage`: `None` when no
    /// registered kind grows into it. Anything unsettled up to that stage may still make such a
    /// grower merge, and so grow (ready.rs R4).
    pub fn grower_stage(&self, stage: Stage) -> Option<Stage> {
        self.facts.iter().flatten().filter(|facts| facts.grows_into.is_some_and(|into| into <= stage)).map(|facts| facts.stage).max()
    }
}

/// The kinds of a request: kind to `&dyn TaskKind`, each once (K4).
pub struct Kinds<'k, 'a, C: ?Sized = ()> {
    by: [Option<&'k dyn TaskKind<'a, C>>; key::KINDS],
}

impl<'k, 'a, C: ?Sized> Kinds<'k, 'a, C> {
    /// The registry of `kinds`, each kind once.
    pub fn new(kinds: &[&'k dyn TaskKind<'a, C>]) -> Result<Kinds<'k, 'a, C>, KindsError> {
        let mut by: [Option<&'k dyn TaskKind<'a, C>>; key::KINDS] = [None; key::KINDS];
        for one in kinds.iter() {
            let kind = one.facts().kind;
            let slot = &mut by[kind.index()];
            if slot.is_some() {
                return Err(KindsError::Twice { kind });
            }
            *slot = Some(*one);
        }
        Ok(Kinds { by })
    }

    /// The kind of `kind`'s tasks, `None` when none is registered.
    pub fn get(&self, kind: Kind) -> Option<&'k dyn TaskKind<'a, C>> {
        self.by[kind.index()]
    }

    /// Their facts, for the driver.
    pub fn table(&self) -> Result<KindTable, KindsError> {
        let mut table = KindTable { facts: [None; key::KINDS], stood: [false; key::KINDS] };
        for one in self.by.iter().flatten() {
            let facts = one.facts();
            let slot = &mut table.facts[facts.kind.index()];
            if slot.is_some() {
                return Err(KindsError::Twice { kind: facts.kind });
            }
            *slot = Some(facts);
        }
        Ok(table)
    }
}
