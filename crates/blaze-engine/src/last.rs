//! The seam between a request and the answer the checkout kept from the last one
//! (docs/v2/plan/design/runtime.md, "The answer's stored form", pieces F0 and D5): what a request
//! asks of the last answer ([`LastAnswer`]) and what it writes for the next ([`NextAnswer`]).
//!
//! Every question names one task or one cell, so an implementation answers it from the rows and
//! runs it touches and nothing else: a request with one edit reads the entered tasks' rows and
//! the moved cells' readers, never the whole answer. Two forms stand behind it, chosen at one
//! type parameter (rule 78): blaze-cli's `PagesAnswer` over the page form (pages.rs, read whole
//! by `load_answer`) and `RowsAnswer` over blaze-answer's segments read in place.
//!
//! The engine reads the last answer through this seam alone (engine.rs over overlay.rs: this
//! request's writes over the last answer's rows), so a third form, the engine's in-memory
//! `Answer` (last_mem.rs), and a test form answering by binary search (last_sim.rs) stand behind
//! it too, and the history simulation holds every form's engine equal to the in-memory one's.
//!
//! The digest a row saw of each cell it read is carried where the form keeps it
//! ([`LastAnswer::seen`]: the in-memory and the page form), and the engine then compares digests
//! as it always has: a cell that moved and moved back leaves its readers standing. The rows form
//! keeps none: its rows stand by generation (no cell read moved after the row's serial,
//! [`LastAnswer::moved`], [`LastAnswer::stands`]), and the digests are in the task's record,
//! which is what a fetched record or a restart from a lost answer is checked against.
//!
//! What the seam does not carry, by design:
//! - The causes (why each row was last entered): the profile's, not the answer's.
//! - The record key of each row on the read side (pages.rs `records`): it is written
//!   ([`NextRow::record`], the rows form's cold column) and read by the command, not the engine.
//!
//! A row the seam returns for an owed task carries [`Flags::OWED`] and the plan's phase, file
//! and source; one owed with no row made carries [`Flags::UNMADE`] too, and is no row to the
//! engine (`Cause::New`, not `Cause::KeyMoved`).
//!
//! A handle ([`LastAnswer::Task`], [`LastAnswer::Cell`]) means something inside one answer only:
//! the page form's are the engine's `Name` and `CellId`, the rows form's its dense ids.

use core::fmt;

use blaze_answer::{AnswerError, CellDigest, Flags, LinesKey, Serial, TreeDigest};
use blaze_drive::cells::Cell;
use blaze_drive::key::{Key, Name};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc};

use crate::engine::{CellId, Phase, Planned};

/// A row's fixed fields, as the last answer has them (its lists are asked apart).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LastRow {
    pub key: Key,
    /// The digest of what it made: equal bytes move no reader (early cutoff).
    pub out: ContentId,
    /// The request that made it, in the answer's own numbering.
    pub serial: Serial,
    pub phase: Phase,
    /// The file whose outline lists it.
    pub file: Name,
    /// What it is planned again from when owed.
    pub source: ContentId,
    /// [`Flags::FAILED`], [`Flags::OWED`] (its phase, file and source are then the plan's),
    /// [`Flags::INLINE`].
    pub flags: Flags,
    pub findings: u32,
    pub reads: u32,
    pub defines: u32,
}

/// A row's read of one cell, as [`LastAnswer::read_of`] found it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Found {
    /// The row did not read the cell, or there is no row.
    NotRead,
    /// It read the cell and saw this digest (a form that keeps seen digests).
    Saw(ContentId),
    /// It read the cell, and its row stands from this serial: a form that keeps no seen digest,
    /// whose row saw what the cell holds unless the cell moved after it (the generation rule).
    ReadAt(Serial),
}

/// What [`LastAnswer::read_of`] found and what finding it cost.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadOf {
    pub found: Found,
    /// Entries compared to find it: a binary search's probes (summed over the runs searched,
    /// in the rows form).
    pub compared: u32,
}

/// The binary search every form's [`LastAnswer::read_of`] and the engine's `Row::read_of` use:
/// over `n` entries in order, `probe(i)` comparing entry `i` with the needle. The place found,
/// and the entries compared (at most `floor(log2 n) + 1`).
pub fn search(n: usize, mut probe: impl FnMut(usize) -> core::cmp::Ordering) -> (Option<usize>, u32) {
    let (mut lo, mut hi, mut compared) = (0usize, n, 0u32);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        compared += 1;
        match probe(mid) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => return (Some(mid), compared),
        }
    }
    (None, compared)
}

/// What the seam's questions cost, summed since the answer was opened (the profile's counts).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Touched {
    /// Rows read: a row asked for, or visited by a question that walks rows.
    pub rows: u64,
    /// Reads walked, across every read list asked for.
    pub reads: u64,
    /// Reader entries summed, across every reverse-index lookup.
    pub readers: u64,
    /// Bytes of the answer read to answer them (an upper bound: a field read is counted whole,
    /// a search counted at its deepest).
    pub bytes: u64,
}

/// Why the seam could not answer: the stored answer contradicts itself. Each names the thing
/// (rule 19); the caller reports it and answers in full (a stated path, rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LastError {
    /// A page named by the authenticated root could not be read or verified.
    Pages(crate::pages::PagesError),
    /// The findings index names a task whose answer has no findings count.
    FindingRow { task: Name },
    /// The rows form refused (blaze-answer).
    Answer(AnswerError),
    /// A row's stage byte names no stage.
    Stage { found: u8 },
    /// A dense id the answer's dictionary does not name (`what`: a task, a file, a cell).
    NoName { what: &'static str, id: u32 },
    /// A changed row reached publication without a payload decision or with a reserved key.
    RowPayload { task: Name },
    /// A changed row contradicts a count carried in the previous certificate.
    CertificateCount { task: Name, column: &'static str, previous: u32 },
}

impl From<AnswerError> for LastError {
    fn from(error: AnswerError) -> LastError {
        LastError::Answer(error)
    }
}

impl fmt::Display for LastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LastError::Pages(error) => write!(f, "{error}"),
            LastError::FindingRow { task } => write!(f, "answer's findings index names {task} without a row count"),
            LastError::Answer(error) => write!(f, "the stored answer: {error}"),
            LastError::Stage { found } => write!(f, "the stored answer: a row's stage is {found}, which names no stage (last.rs)"),
            LastError::NoName { what, id } => write!(f, "the stored answer: {what} {id} is not in its dictionary (last.rs)"),
            LastError::RowPayload { task } => write!(f, "cannot publish {task}: its changed row needs a nonzero record key or an explicit recordless result"),
            LastError::CertificateCount { task, column, previous } => write!(f, "cannot publish {task}: applying its change to the certificate's {column} count {previous} is out of range"),
        }
    }
}

/// What a request asks of the last answer. Nothing here reads the answer whole: the questions
/// over a set ([`LastAnswer::file_tasks`], [`LastAnswer::owed`], [`LastAnswer::owed_cells`],
/// [`LastAnswer::with_findings`]) are each read from the set's own column in the rows form
/// (blaze-answer kinds 5 to 8), and a row each reads is counted in [`Touched::rows`].
pub trait LastAnswer {
    /// A failure observed by an infallible worker query. It stops the request before
    /// a merge accepts results or a fold can publish its answer.
    fn refusal(&self) -> Option<LastError> {
        None
    }
    /// A task inside this answer.
    type Task: Copy + Eq + Ord + fmt::Debug;
    /// A cell inside this answer.
    type Cell: Copy + Eq + Ord + fmt::Debug;
    /// A row's reads in the order recorded.
    type Reads<'r>: Iterator<Item = Self::Cell>
    where
        Self: 'r;
    /// A row's defined cells and their digests, by cell.
    type Defines<'r>: Iterator<Item = (Self::Cell, CellDigest)>
    where
        Self: 'r;
    /// A row's reads in the order recorded, each with the digest the row saw of it.
    type Seen<'r>: Iterator<Item = (Self::Cell, ContentId)>
    where
        Self: 'r;

    /// The request that made this answer.
    fn serial(&self) -> Serial;

    /// The certificate: whether this answer is today's for the tree `tree` (one read of the
    /// root or the pointer, no row): it answers that tree, nothing is owed, and every line it
    /// printed is in its lines record.
    fn certificate(&self, tree: &TreeDigest) -> bool;

    /// The handle of `name`; `None`: the answer has never named it.
    fn task(&self, name: &Name) -> Option<Self::Task>;
    /// The name of `task`.
    fn name(&self, task: Self::Task) -> Option<Name>;
    /// The handle of the cell `id`; `None`: no row ever read or defined it.
    fn cell(&self, id: &CellId) -> Option<Self::Cell>;
    /// The engine's id of `cell`.
    fn cell_id(&self, cell: Self::Cell) -> Option<CellId>;

    /// `task`'s row: what `Engine::new_in` and `next_phase` compare the planned key with, and
    /// what an owed or moved task is planned again from (`planned_of`).
    fn row(&self, task: Self::Task) -> Result<Option<LastRow>, LastError>;
    /// `task`'s reads, in the order recorded: the engine stops at the first that moved.
    fn reads(&self, task: Self::Task) -> Option<Self::Reads<'_>>;
    /// The cells `task` defines and their digests (the merge's early cutoff compares them).
    fn defines(&self, task: Self::Task) -> Option<Self::Defines<'_>>;
    /// Whether `task`'s row stands: no cell it read moved after the row (`Answer::today`).
    /// `None`: no row. An owed row's answer is not consulted: an owed task is entered anyway.
    fn stands(&self, task: Self::Task) -> Option<bool>;
    /// `task`'s read of `cell`, found by a search and never by a walk of the read list (plan row
    /// B1: a walk per reader per owed cell ran `finish` past 14 minutes on tempo): what the row
    /// saw of it, or the serial its row stands from when the form keeps no seen digest. The
    /// cost is [`ReadOf::compared`]: O(log reads) a row (a sorted order of the reads, or the
    /// rows form's reverse index, sorted by task in each of its runs).
    fn read_of(&self, task: Self::Task, cell: Self::Cell) -> Result<ReadOf, LastError>;
    /// Whether `task`'s row read `cell` and saw what it holds today: [`LastAnswer::read_of`]
    /// against [`LastAnswer::content`], or by generation against [`LastAnswer::moved`].
    fn saw(&self, task: Self::Task, cell: Self::Cell) -> Result<bool, LastError> {
        Ok(match self.read_of(task, cell)?.found {
            Found::NotRead => false,
            Found::Saw(seen) => seen == self.content(cell),
            Found::ReadAt(serial) => self.moved(cell) <= serial,
        })
    }
    /// What `cell` holds today ([`CellDigest::ABSENT`]: nothing defines it).
    fn digest(&self, cell: Self::Cell) -> CellDigest;
    /// The full content ID a task observes, identical across a restart. An undefined cell
    /// yields `ABSENT`.
    fn content(&self, cell: Self::Cell) -> ContentId;
    /// The request `cell` last moved in ([`Serial`]`(0)`: never moved, or the form lost it when
    /// the cell went absent and every reader saw that).
    fn moved(&self, cell: Self::Cell) -> Serial;
    /// `task`'s reads in the order recorded, each with the digest its row saw: how the engine
    /// finds the first read that moved, and names its cause, when the form keeps them. `None`:
    /// no row, or a form that keeps no seen digests (the rows form), whose rows stand by
    /// generation: a read moved when its cell [`LastAnswer::moved`] after the row's serial.
    fn seen(&self, task: Self::Task) -> Option<Self::Seen<'_>>;

    /// The tasks whose rows read `cell`, ascending by handle: how work is found forward from a
    /// moved cell (`readers_of`, `inline_readers`).
    fn readers<'m>(&self, cell: Self::Cell, alloc: Alloc<'m>) -> Result<AVec<'m, Self::Task>, LastError>;

    /// The tasks whose row names `file` as the file that lists them (`Answer::by_file`: an
    /// edited file's tasks gone from its plan).
    fn file_tasks<'m>(&self, file: Self::Task, alloc: Alloc<'m>) -> Result<AVec<'m, Self::Task>, LastError>;
    /// The actual key of a made row, when the stored file plan certifies it.
    /// A form without this column, a new member or an unmade task has no proof.
    fn planned_key(&self, _file: Name, _name: Name) -> Result<Option<Key>, LastError> {
        Ok(None)
    }
    /// The owed tasks, as planned (`Answer::owed_tasks`).
    fn owed<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, Planned>, LastError>;
    /// The cells that moved while a reader of them was not run, and when (`Answer::owed_cells`).
    fn owed_cells<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (Self::Cell, Serial)>, LastError>;
    /// The rows with findings or failed, and their findings counts (`Answer::with_findings`,
    /// `Answer::prints`).
    fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (Self::Task, u32)>, LastError>;

    /// What the questions so far cost.
    fn touched(&self) -> Touched;
}

/// A row as this request made it (the engine's `Done` with its plan), less its reads and
/// defines, which are passed beside it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NextRow {
    pub key: Key,
    pub out: ContentId,
    pub phase: Phase,
    pub file: Name,
    pub source: ContentId,
    pub flags: Flags,
    pub findings: u32,
    /// The store key of the record the row's entry made (the command's `records`).
    pub record: Key,
}

/// What a request writes for the next. A row this request did not put, owe or retire is
/// carried: its bytes stay where they are and its generation decides whether it stands.
pub trait NextAnswer {
    /// What [`NextAnswer::finish`] gives: the writes, for the command's one commit.
    type Done;

    /// `name`'s row as this request entered it (made or re-made, identical or not): `reads` in
    /// the order the task read them, `defines` sorted by cell.
    fn put<R: IntoIterator<Item = CellId>>(&mut self, name: Name, row: &NextRow, reads: R, defines: &[(Cell, ContentId)]) -> Result<(), LastError>;
    /// `planned` is owed (its key moved and the scope did not run it): its last row, if any,
    /// carried with the plan's phase, file and source and [`Flags::OWED`].
    fn owe(&mut self, planned: &Planned) -> Result<(), LastError>;
    /// `name` has no row from this request on (its file's plan dropped it). No row: nothing.
    fn retire(&mut self, name: Name) -> Result<(), LastError>;
    /// The next answer for the tree `tree`: its summary (what the certificate prints) and the
    /// key of its printed lines' record.
    fn finish(self, tree: &TreeDigest, summary: &[u8], lines: &LinesKey) -> Result<Self::Done, LastError>;
}
