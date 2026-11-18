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
//! What the seam does not carry, by design:
//! - The digest a row saw of each cell it read. A row stands by generation: no cell it read
//!   moved after the row's serial ([`LastAnswer::stands`]). The digests then are in the task's
//!   record, which is what a fetched record or a restart from a lost answer is checked against.
//! - The causes (why each row was last entered): the profile's, not the answer's.
//! - The record key of each row (pages.rs `records`): the rows form has no column for it yet.
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
    /// The rows form refused (blaze-answer).
    Answer(AnswerError),
    /// A row's stage byte names no stage.
    Stage { found: u8 },
    /// A dense id the answer's dictionary does not name (`what`: a task, a file, a cell).
    NoName { what: &'static str, id: u32 },
}

impl From<AnswerError> for LastError {
    fn from(error: AnswerError) -> LastError {
        LastError::Answer(error)
    }
}

impl fmt::Display for LastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LastError::Answer(error) => write!(f, "the stored answer: {error}"),
            LastError::Stage { found } => write!(f, "the stored answer: a row's stage is {found}, which names no stage (last.rs)"),
            LastError::NoName { what, id } => write!(f, "the stored answer: {what} {id} is not in its dictionary (last.rs)"),
        }
    }
}

/// What a request asks of the last answer. Nothing here reads the answer whole; the methods
/// that walk rows today ([`LastAnswer::file_tasks`], [`LastAnswer::owed`],
/// [`LastAnswer::owed_cells`], [`LastAnswer::with_findings`]) are counted in [`Touched::rows`]
/// as they walk, so a profile shows them.
pub trait LastAnswer {
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
    /// Whether `task`'s row read `cell` and saw what it holds today (`Engine::finish`'s
    /// settling of an owed cell).
    fn saw(&self, task: Self::Task, cell: Self::Cell) -> bool;
    /// What `cell` holds today ([`CellDigest::ABSENT`]: nothing defines it).
    fn digest(&self, cell: Self::Cell) -> CellDigest;

    /// The tasks whose rows read `cell`, ascending by handle: how work is found forward from a
    /// moved cell (`readers_of`, `inline_readers`).
    fn readers<'m>(&self, cell: Self::Cell, alloc: Alloc<'m>) -> Result<AVec<'m, Self::Task>, LastError>;

    /// The tasks whose row names `file` as the file that lists them (`Answer::by_file`: an
    /// edited file's tasks gone from its plan).
    fn file_tasks<'m>(&self, file: Self::Task, alloc: Alloc<'m>) -> Result<AVec<'m, Self::Task>, LastError>;
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
