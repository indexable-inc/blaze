//! The answer's stored form (docs/v2/plan/design/runtime.md, piece F0 and "The answer's stored
//! form"): what one checkout keeps between requests of the engine, as immutable byte segments
//! under a small root, read in place.
//!
//! The answer is one row per task, the cells with who defines each and when each last moved,
//! and the index from a cell to the tasks that read it. It replaces blaze-engine's page form
//! (pages.rs: 406 B a row before its first read, 81 B a read, the reverse index rebuilt from
//! every row at each request, the whole answer read to say "nothing changed").
//!
//! | module | what |
//! |---|---|
//! | [`segment`] | the eight segment kinds: their builders and their views over `&[u8]` |
//! | [`answer`] | the root, [`AnswerView`] (open, adopt), lookups, validity, readers |
//! | [`commit`] | a request's [`Commit`], a worker's [`RunWriter`], [`compact`]: operations to effects |
//!
//! Nothing here touches the system, keeps a clock or picks an allocator (rule 75): a request's
//! operations go in, the bytes of new segments and of the root come out as [`Effects`], and the
//! caller writes them in the order given.
//!
//! # The form, version [`FORMAT`]
//!
//! Inside one answer a task is a [`TaskId`] and a cell a [`CellNo`], dense from 0 and never
//! reused; the dictionary maps them to the engine's `Name` and `CellId { of, cell }`. A request
//! writes new segments with only the rows and cells it changed and one run of reverse entries,
//! then a new root listing every live segment, newest first. A lookup probes newest to oldest;
//! a newer row of a task (or its tombstone) shadows every older one.
//!
//! Every field is little-endian and fixed-width; each column is a run of equal-width fields
//! (structure of arrays), so a view reads a field at `column + index * width`, no decode step.
//!
//! ```text
//! segment head (24 B)  magic "BAS1" | format u32 | kind u8 | 7 zero bytes | serial u64
//!
//! dict (kind 1)   first_task u32 | tasks n | task_words u32 | first_cell u32 | cells m | cell_words u32
//!                 task_kind [n] u8 | task_subject [n] 32 B | task_sorted [n] u32 |
//!                 task_filter [task_words] u64 |
//!                 cell_definer [m] u32 | cell_key [m] 16 B | cell_sorted [m] u32 |
//!                 cell_filter [cell_words] u64
//!                 (ids first..first+n in order; `*_sorted` the local indices by key, the sorted
//!                 run binary search walks; the filters: 10 bits a key, 3 probes)
//!
//! rows (kind 2)   rows n | tombs t | read_bytes r | defines d
//!   hot, 96 B a row:
//!                 task [n] u32 (ascending) | key [n] 32 B | out [n] 32 B | serial [n] u64 |
//!                 file [n] u32 (a TaskId) | phase [n] (stage u8 | flags u8 | wave u16) |
//!                 reads_at [n] u32 | reads_meta [n] u32 (count: low 24 bits; width: high 8) |
//!                 defines_end [n] u32
//!   cold, 68 B a row (read to plan an owed task again, to print, and after a restart):
//!                 source [n] 32 B | findings [n] u32 | record [n] 32 B (the store key of the
//!                 record the row's last entry made; zeros for a plan never made)
//!   lists:        tomb [t] u32 (ascending) | reads [r] bytes | define_cell [d] u32 |
//!                 define_digest [d] 32 B
//!                 (a row's reads: base u32, then its CellNos less base, `width` bits each, in
//!                 READ ORDER; its defines sorted by CellNo, 36 B each)
//!
//! cells (kind 3)  cells n | cell [n] u32 (ascending) | moved [n] u64     (12 B a moved cell)
//!
//! run (kind 4)    cells k | entries e | cell [k] u32 (ascending) | start [k + 1] u32 |
//!                 entry [e] u32 (a TaskId, ascending in its cell; bit 31 set: -1, else +1)
//!
//! plans (kind 5)     a run's bytes, keyed by a file's TaskId: the tasks its plan lists (a row
//!                    not made inline, by its `file`). 4 B a planned task plus 8 B a file.
//! findings (kind 6)  a run's bytes, one key 0: the tasks whose row made findings or failed.
//!                    4 B a task plus 8 B. The count is the row's cold `findings`.
//!                    (plans, findings and run: each request writes the changes it made, +1 or
//!                    -1; a lookup sums every run of its kind, one binary search each; all three
//!                    count against MAX_RUNS; compaction merges each kind to one)
//! owed (kind 7)      tasks n | task [n] u32 (ascending): the owed tasks, WHOLE (4 B a task)
//! owed cells (kind 8)  the cells segment's bytes: the owed cells, WHOLE, each with the serial it
//!                    moved at (12 B a cell)
//!                    (owed and owed cells: small; a request that changes the set writes it whole
//!                    and its root drops the last's, so a root lists at most one of each, which
//!                    open checks)
//!
//! root            magic "BAR1" | format u32 | serial u64 | tree 32 B | lines 32 B |
//!                 summary_len u32 | summary | segments u32 |
//!                 segments x (kind u8 | len u64 | id 32 B: ContentId of the segment's bytes)
//! ```
//!
//! What things cost (the size test in sim.rs prints them for 24,000 rows of 300 reads over
//! 60,000 cells): a row's fixed part 96 B ([`ROW_FIXED_BYTES`]) plus 68 B cold; a read
//! `width / 8` B plus 4 B a row (2.01 B at 300 reads over 200 distinct cells of 60,000: 16 bits);
//! a defined cell 36 B in its row ([`DEFINE_BYTES`]) and 12 B in a cells segment when it moves;
//! a reader 4 B in a run plus 8 B per distinct cell of the run.
//!
//! Where the brief's points are met differently, in one line each:
//! - A cell's current digest is not in the cells segment: it is its definer's row's define entry
//!   (the definer is the dictionary's; the digest is read where the merge reads it anyway).
//! - The row's source and findings count are a cold column outside the 96 B: the brief's fixed
//!   fields (key, out, serial, wave, flags, counts and offsets) and the task and file ids fill it.
//! - Opening a segment checks every bound an accessor indexes by (each rows segment's three list
//!   columns, each run's starts, each dictionary's kinds and sorted indices): O(rows) over those
//!   columns, not O(bytes); the order of keys is checked by [`AnswerView::verify`], once, where a
//!   segment comes from elsewhere.
//!
//! # Validity (design point 3)
//!
//! Each cell carries the serial of the request it last moved in (a define's digest changed, or
//! its definer stopped defining it). A row stands when no cell it read moved after the row's own
//! serial, walked in read order, stopping at the first that moved ([`AnswerView::stands`]). A
//! move back to an earlier digest is a move: the row that saw the digest between is stale.
//!
//! # The crash rule (rule 43; simulated in sim.rs)
//!
//! [`Effects::writes`] lists every new segment, then the root. The caller writes them in that
//! order and the root's write is the commit; a segment is immutable and no commit deletes one,
//! so a crash before the root leaves the last root naming only segments that exist. A root never
//! names a segment that was not written.
//!
//! # What the wiring lane changes (nothing here is wired yet)
//!
//! blaze-engine, engine.rs, reads `Answer`/`Row` today at:
//! - `Answer` (265-279): `rows`, `cells`, `readers`, `by_file`, `owed_cells`, `owed_tasks`,
//!   `with_findings` become an [`AnswerView`] of the last answer plus the request's [`Commit`]:
//!   `by_file` is [`AnswerView::file_tasks`] (kind 5), `with_findings`
//!   [`AnswerView::with_findings`] (kind 6), `owed_tasks` [`AnswerView::owed`] (kind 7, the rows
//!   flagged [`Flags::OWED`]), `owed_cells` [`AnswerView::owed_cells`] (kind 8, given whole by
//!   [`Commit::owe_cells`]). The commit derives kinds 5 to 7 from each row it puts or retires.
//! - `Answer::prints` (302-312, `rows.get` at 305): the findings count is the cold column.
//! - `Answer::today` / `first_moved` (315 on): [`AnswerView::stands`] (the serial rule replaces
//!   the digest compare; `Cause::ReadMoved`'s `was` comes from the task's record in the store).
//! - `View::row` (448-450, `rows.get` at 449): [`AnswerView::row`].
//! - `Engine::new_in` (492 on): `by_file` at 512 and 532, `rows.get` at 527, `owed_tasks` at
//!   521 and 537, `owed_cells` at 547.
//! - `Engine::next_phase` (559 on): `rows.get` at 571, 577 and 584.
//! - `Engine::finish` (693 on): `owed_cells` at 698-717, `readers.get` at 704, `rows.get` at 706.
//! - `planned_of` (732-739): the row's stage, wave, file and cold source.
//! - `readers_of` (755, `readers.get` at 756) and `inline_readers` (913, at 914):
//!   [`AnswerView::readers`].
//! - `move_cell` (770-776): the moves [`Commit::put_row`] and [`Commit::retire`] compute.
//! - `remove_row` (784-786): [`Commit::retire`], and in the worker [`RunWriter::row_retired`].
//! - `unread` (796-807) and `apply` (813-900, `rows.get` at 815, `readers` at 833, `cells` at
//!   846 and 897-900): [`Commit::put_row`] in the merge, [`RunWriter::row_reads`] in the worker
//!   (the coordinator then only takes each worker's run bytes: [`Commit::add_run`]).
//! - pages.rs and pages_sim.rs are deleted with the switch.
//!
//! blaze-cli: engine_answer.rs `load_answer` (176-205: `pages::decode`, `pages::decode_root`)
//! becomes [`nothing_changed`] then [`AnswerView::open`] over mapped segments;
//! engine_answer.rs 272-280 (`pages::encode`) writes [`Effects`] in order; engine_check.rs 798
//! (`pages::Kept::after`) and engine_why.rs 14, 81, 227 (`Kept` for `blaze why`): the causes
//! leave the answer (a cause is the profile's, not the row's); the root and its segments are put
//! in the shared store under the tree's digest for [`AnswerView::adopt`].
//!
//! The view's own tables hold [`SharedAlloc`](blaze_own::SharedAlloc): an [`AnswerView`] is
//! `Sync`, so a phase's workers read one at once.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod answer;
mod bytes;
pub mod commit;
pub mod segment;

#[cfg(test)]
mod sim;

use core::fmt;

use blaze_id::ContentId;

pub use crate::answer::{AnswerView, Health, RootView, SegmentRef, nothing_changed, summary_of};
pub use crate::commit::{Commit, Effects, RowIn, RunWriter, Write, compact, has_findings};
pub use crate::segment::{Reads, RowView, RowsBytes, SegmentKind};

/// The form's version: a segment or root of another is refused ([`AnswerError::Format`]); an old
/// home is reported and rebuilt, never migrated (rule 26).
/// 2: the record key in the cold column and kinds 5 to 8.
/// 3: each definition stores the full content ID a task observes.
pub const FORMAT: u32 = 3;
/// A segment's first bytes.
pub const SEGMENT_MAGIC: [u8; 4] = *b"BAS1";
/// A root's first bytes.
pub const ROOT_MAGIC: [u8; 4] = *b"BAR1";
/// The bytes of a cell digest kept in the answer.
pub const DIGEST_BYTES: usize = 32;
/// A row's fixed part: task, key, out, serial, file, phase and flags, and its lists' offsets.
pub const ROW_FIXED_BYTES: usize = 4 + 32 + 32 + 8 + 4 + 4 + 4 + 4 + 4;
/// A row's cold part: its source, its findings count and its record's key.
pub const ROW_COLD_BYTES: usize = 32 + 4 + 32;
/// One defined cell in its row: its CellNo and its digest.
pub const DEFINE_BYTES: usize = 4 + DIGEST_BYTES;

/// Rows in one segment, at most (rule 18).
pub const MAX_ROWS_PER_SEGMENT: usize = 1 << 22;
/// Reads of one row, at most: the 24 bits of its count.
pub const MAX_READS_PER_ROW: usize = (1 << 24) - 1;
/// Segments one root lists, at most: past it a commit is [`AnswerError::CompactionOwed`].
pub const MAX_SEGMENTS: usize = 256;
/// Segments at which [`AnswerView::health`] says compaction is due.
pub const COMPACT_AT: usize = 128;
/// Runs one root lists, at most: [`AnswerView::readers`] merges this many at worst.
pub const MAX_RUNS: usize = 64;
/// Task ids are below this: bit 31 of a run entry is its sign.
pub const MAX_TASKS: u32 = 1 << 31;
/// Cell numbers are below this.
pub const MAX_CELLS: u32 = u32::MAX;
/// The closing line's summary, at most.
pub const MAX_SUMMARY: usize = 1 << 16;

/// A task inside one answer: the engine's `Name`, interned in the dictionary in the order the
/// requests first named it. Means nothing outside its answer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TaskId(pub u32);

/// A cell inside one answer: the engine's `CellId { of, cell }`, interned like a [`TaskId`]; its
/// definer is the task `of`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CellNo(pub u32);

/// A request's number (the engine's `Gen`): a row's serial is the request that made it, a cell's
/// the request it last moved in. 0 is before any request: a cell that never moved.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Serial(pub u64);

/// The content ID a task observes when reading this cell. All zeros represent absence.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CellDigest(pub [u8; DIGEST_BYTES]);

impl CellDigest {
    /// No task defines the cell.
    pub const ABSENT: CellDigest = CellDigest([0; DIGEST_BYTES]);

    /// The answer's form of the engine's digest of a cell.
    pub fn of(digest: &ContentId) -> CellDigest {
        CellDigest(digest.0)
    }
}

/// A row's flags, as the engine's `Row` has them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Flags(pub u8);

impl Flags {
    /// The task failed on its reads: it defines nothing and stands until a read or its key moves.
    pub const FAILED: u8 = 1;
    /// Its key moved while the scope did not run it: owed to the request that runs it.
    pub const OWED: u8 = 2;
    /// Made inline: no file's plan lists it.
    pub const INLINE: u8 = 4;
    /// Owed and never made: the row holds the plan alone (its key, out and lists are empty), and
    /// the engine sees no row (blaze-engine last.rs: `Cause::New`, not `Cause::KeyMoved`).
    pub const UNMADE: u8 = 8;
}

/// The digest of the checkout's tree an answer answers (opaque here): the certificate's key, by
/// which a second worktree at the same commit adopts the answer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct TreeDigest(pub [u8; 32]);

/// The store key of the record of the lines the closing request printed (opaque here).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct LinesKey(pub [u8; 32]);

/// A bug the simulation plants to show its oracle catches it. Always `None` outside the tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// Discard the final twelve bytes of each definition before writing the row.
    PrefixPadded,
    /// Drop the removal of a cell's last reader from the posting delta.
    LastReaderRemovalDropped,
    /// A lookup skips tombstones, so a retired task's row in an older segment is found again.
    TombstoneIgnored,
    /// Compaction keeps a cancelled (+1, -1) pair as a reader.
    CancelledKept,
    /// A cell moving back to a digest an older row of its definer held takes that row's serial as
    /// its last move: the generation is the digest's, not the move's.
    GenerationByDigest,
    /// The root is written before the segments it names.
    RootFirst,
    /// A retired task stays in its file's plan: the commit writes no -1 plans entry for it.
    PlanKeptOnRetire,
    /// A row whose findings went keeps its findings entry: the commit writes no -1 for it.
    FindingsKeptOnClear,
    /// A retired owed task stays owed.
    OwedKeptOnRetire,
    /// Owed cells emptied write no segment, so the last set still answers.
    OwedCellsEmptyDropped,
    /// Compaction writes each row's record key as zeros.
    RecordDropped,
}

/// Why an answer cannot be read or a request cannot be committed. Each names the thing: the
/// segment by its index in the root's list (newest first; [`INPUT`] for a run handed to a
/// commit), the section, the id (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnswerError {
    /// The root ends at `at`, inside a field.
    RootShort { at: usize },
    /// The bytes are not a root or not a segment: their magic is another's.
    Magic { segment: u32 },
    /// Another version of the form ([`FORMAT`]): an old home, rebuilt.
    Format { segment: u32, found: u32 },
    /// The root has bytes past its last segment.
    RootTrailing { at: usize },
    /// The root names a segment the store does not have: never after a commit in order.
    MissingSegment { segment: u32, id: ContentId },
    /// A segment's bytes are not as long as the root says.
    SegmentLength { segment: u32, expected: u64, found: u64 },
    /// A scrub found bytes different from the immutable child named by the root.
    SegmentDigest { segment: u32, expected: ContentId, found: ContentId },
    /// A dictionary's filter rejects a key present in its own column.
    FilterMissing { segment: u32, section: &'static str, at: usize },
    /// A segment's kind is not the one the root lists, or no kind.
    SegmentKind { segment: u32, found: u8 },
    /// A segment's sections do not add up to its length.
    SegmentSize { segment: u32, section: &'static str },
    /// A bound an accessor would index by is out of its column, at entry `at`.
    Bounds { segment: u32, section: &'static str, at: usize },
    /// Keys out of order in a section ([`AnswerView::verify`]).
    Unsorted { segment: u32, section: &'static str, at: usize },
    /// The dictionaries' ids do not follow on: `section` starts at `found`, not `expected`.
    IdGap { segment: u32, section: &'static str, expected: u32, found: u32 },
    /// The root answers another tree than the one asked to adopt.
    TreeMismatch,
    /// A cap of rule 18 reached: `what`, `cap`.
    Cap { what: &'static str, cap: u64 },
    /// A typed state, not a broken answer: the root would list more `what` than its cap; the
    /// caller compacts ([`compact`]) and commits again.
    CompactionOwed { what: &'static str, count: usize, cap: usize },
    /// One request gave the task two values (a row twice, or a row and a tombstone).
    TwoValues { task: TaskId },
    /// A task defines a cell another task is the definer of.
    NotDefiner { task: TaskId, cell: CellNo },
    /// A row defines one cell twice.
    DefinedTwice { task: TaskId, cell: CellNo },
    /// An id the answer has not given out.
    UnknownTask { task: TaskId },
    /// A cell number the answer has not given out.
    UnknownCell { cell: CellNo },
    /// A task with no row was retired.
    NoRow { task: TaskId },
    /// The entries of a run of `kind` for `(cell, task)` sum to `sum`, outside what its runs allow
    /// (0 or 1 over every run; -1, 0 or 1 inside one request's). `cell` is the run's key: a cell
    /// for the reverse index, a file's task id for the plans, 0 for the findings.
    ReadersSum { kind: SegmentKind, cell: CellNo, task: TaskId, sum: i32 },
    /// One request gave the owed cells twice the same cell.
    OwedTwice { cell: CellNo },
    /// A row put to stand from a serial past its request's.
    RowSerial { task: TaskId, serial: Serial },
    /// A root lists two segments of a whole-set kind ([`SegmentKind::is_whole`]): a commit
    /// replaces the last's.
    TwoWhole { kind: SegmentKind, segment: u32 },
}

/// The segment index an error of a run handed to [`Commit::add_run`] names.
pub const INPUT: u32 = u32::MAX;
/// The segment index an error of the root names.
pub const ROOT: u32 = u32::MAX - 1;

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnswerError::RootShort { at } => write!(f, "the answer's root ends at byte {at}, inside a field"),
            AnswerError::Magic { segment } => write!(f, "answer segment {segment}: not of the answer's form (its magic)"),
            AnswerError::Format { segment, found } => write!(f, "answer segment {segment}: form {found}, this blaze reads {FORMAT}: an old home, rebuilt"),
            AnswerError::RootTrailing { at } => write!(f, "the answer's root has bytes past its last segment, at {at}"),
            AnswerError::MissingSegment { segment, id } => write!(f, "the answer's root names segment {segment} ({id:?}), which the store does not have"),
            AnswerError::SegmentLength { segment, expected, found } => write!(f, "answer segment {segment}: {found} bytes, the root says {expected}"),
            AnswerError::SegmentDigest { segment, expected, found } => write!(f, "answer segment {segment} has digest {found:x}, its root names {expected:x}"),
            AnswerError::FilterMissing { segment, section, at } => write!(f, "answer segment {segment}: the {section} filter rejects its own entry {at}"),
            AnswerError::SegmentKind { segment, found } => write!(f, "answer segment {segment}: kind {found} is not the one its root lists"),
            AnswerError::SegmentSize { segment, section } => write!(f, "answer segment {segment}: its {section} do not add up to its length"),
            AnswerError::Bounds { segment, section, at } => write!(f, "answer segment {segment}: {section} entry {at} is out of bounds"),
            AnswerError::Unsorted { segment, section, at } => write!(f, "answer segment {segment}: {section} out of order at {at}"),
            AnswerError::IdGap { segment, section, expected, found } => write!(f, "answer segment {segment}: {section} start at {found}, the last ended at {expected}"),
            AnswerError::TreeMismatch => write!(f, "the answer's root answers another tree than the one asked"),
            AnswerError::Cap { what, cap } => write!(f, "the answer's cap on {what} ({cap}) is reached (rule 18)"),
            AnswerError::CompactionOwed { what, count, cap } => write!(f, "compaction owed: the root would list {count} {what}, its cap is {cap}"),
            AnswerError::TwoValues { task } => write!(f, "task {} given two values in one request", task.0),
            AnswerError::NotDefiner { task, cell } => write!(f, "task {} defines cell {}, of another task", task.0, cell.0),
            AnswerError::DefinedTwice { task, cell } => write!(f, "task {} defines cell {} twice", task.0, cell.0),
            AnswerError::UnknownTask { task } => write!(f, "task {} is not in the answer's dictionary", task.0),
            AnswerError::UnknownCell { cell } => write!(f, "cell {} is not in the answer's dictionary", cell.0),
            AnswerError::NoRow { task } => write!(f, "task {} has no row to retire", task.0),
            AnswerError::ReadersSum { kind, cell, task, sum } => write!(f, "the {kind:?} run of key {} sums to {sum} for task {}", cell.0, task.0),
            AnswerError::OwedTwice { cell } => write!(f, "cell {} is owed twice in one request", cell.0),
            AnswerError::RowSerial { task, serial } => write!(f, "task {}'s row put to stand from serial {}, past its request's", task.0, serial.0),
            AnswerError::TwoWhole { kind, segment } => write!(f, "answer segment {segment}: a second {kind:?} segment in one root (the newest is the whole set)"),
        }
    }
}
