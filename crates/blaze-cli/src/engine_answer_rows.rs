//! `engine-check`'s stored answer in blaze-answer's form (runtime.md, "The answer's stored form",
//! F0; the switch is D5), behind the seam blaze-engine last.rs states, beside the page form
//! (engine_answer.rs), not yet switched on. Both forms answer the seam: [`RowsAnswer`] over the
//! segments read in place, [`PagesAnswer`] over the page form's loaded [`Kept`], so the switch is
//! one type parameter (rule 78).
//!
//! ```text
//! names           engine.<16 hex>.<64 hex>.rows   the root's key alone (engine_answer.rs `root_name`,
//!                                                 then `tools_digest`: [`root_names`])
//!                 engine.<16 hex>.<64 hex>.rows.segments   an indexed root of the live segments, lines and row payloads
//!                 engine.tree.<64 hex of H(tree digest, every kind's tool)>   and its `.segments`:
//!                 the same keys, so a second worktree at the same tree finds the answer
//! the root        blaze-answer's root bytes, under the key of its bytes ([`root_key`])
//! a segment       its bytes, under its `SegmentRef::id` (the digest of its bytes)
//! the summary     files u32 | printed u8 (bit 0 names checked, 1 bodies whole, 2 replayable) |
//!                 findings u32 | owed u32: the certificate's facts, as the page form's pointer
//! ```
//!
//! A root's keys are a set to the store, not an order (engine_answer.rs `save`), so the root's
//! key is alone under its name and the segments are kept under a second.
//!
//! The write ([`write_effects`]): every new segment, then the root, then the four pins, in that
//! order, all before the store's one commit after the answer. The commit makes them visible at
//! once (the store's generation flip, blaze-cas plain): a crash before it leaves the last root
//! and its segments as they were, because a segment is immutable and no write here deletes one.
//! The order alone, without the flip, already never pins a root whose segment is not put: the
//! cut test in engine_answer_rows_tests.rs cuts the writes at every point.
//!
//! The read: [`load_root`] (the name's one key, the root's bytes), [`certified`] (the root alone:
//! no segment read), then [`load_rows`] (the segments mapped where the store holds them). One
//! that cannot be read is [`RowsUnreadable`]: a note says which and why, and the request answers
//! in full and writes the answer anew (a stated path, rule 3); a root of another form is an old
//! home's, reported and rebuilt (rule 26).
//!
//! The engine over this form: `Engine<RowsAnswer>`'s finish folds the request's writes into a
//! [`RowsFolded`] (blaze-engine overlay.rs `Fold`); the command then hands it what only the
//! request's end knows ([`Close`]: the tree, what it printed, the lines record, each row's record
//! key) and [`RowsFolded::commit`] writes the next answer's segments, each row the request put at
//! the serial blaze-engine's `Generations` gives it. A file's plan, the owed tasks and cells and
//! the rows with findings are each read from their own column (blaze-answer kinds 5 to 8), so no
//! question walks the rows (engine_answer_columns_tests.rs counts them).
#![cfg_attr(not(test), expect(dead_code, reason = "the rows form waits on the switch (runtime.md D5); its tests use it today"))]

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use blaze_answer::{
    AnswerError, AnswerView, CellDigest, CellNo, Commit, DEFINE_BYTES, Effects, Flags, LinesKey, MAX_READS_PER_ROW, Plant, ROW_COLD_BYTES, ROW_FIXED_BYTES, RowIn, RowView, RunWriter, SegmentKind,
    RootView, SegmentRef, Serial, TaskId, TreeDigest, Write, has_findings,
};
use blaze_core::Driver;
use blaze_drive::cells::Cell;
use blaze_drive::key::ABSENT;
use blaze_drive::{Key, Name};
use blaze_engine::last::{Found, LastAnswer, LastError, LastRow, NextAnswer, NextRow, ReadOf, Touched};
use blaze_engine::pages::Kept;
use blaze_engine::{Answer, CellId, EngineError, Fold, Phase, Planned, Row, Stage, Tools, Work};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, SharedAlloc, Vec};
use bstd::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::engine_answer::{Facts, Printed};
use crate::store::{CommandStore, Sharing, Store as _};

/// The domain of a root's store key: the digest of its bytes.
const ROOT_DOMAIN: &str = "blaze-cli engine answer rows root";
const TREE_DOMAIN: &str = "blaze-cli engine answer tree";

/// The summary's bytes: files u32, printed u8, findings u32, owed u32.
pub(crate) const SUMMARY_BYTES: usize = 4 + 1 + 4 + 4;

/// One binary search's probes at the cap of a segment (2^22 rows: 22 steps of a u32).
const PROBE_BYTES: u64 = 4 * 22;
/// A name's search in one dictionary: its filter's three words and 22 steps of an index and a
/// 33-byte name.
const NAME_PROBE_BYTES: u64 = 3 * 8 + 22 * (4 + 33);
/// A cell's search in one dictionary: its filter and 22 steps of an index, a definer and a cell.
const CELL_PROBE_BYTES: u64 = 3 * 8 + 22 * (4 + 4 + 16);
/// A row's fixed and cold fields.
const ROW_BYTES: u64 = (ROW_FIXED_BYTES + ROW_COLD_BYTES) as u64;
/// A read, at most: the widest a read list packs one (32 bits).
const READ_BYTES_MOST: u64 = 4;

/// The certificate's facts as the root's summary.
pub(crate) fn summary(facts: &Facts) -> [u8; SUMMARY_BYTES] {
    let mut out = [0u8; SUMMARY_BYTES];
    out[0..4].copy_from_slice(&facts.files.to_le_bytes());
    let printed = facts.printed;
    out[4] = u8::from(printed.names) | u8::from(printed.bodies_whole) << 1 | u8::from(printed.replayable) << 2;
    out[5..9].copy_from_slice(&facts.findings.to_le_bytes());
    out[9..13].copy_from_slice(&facts.owed.to_le_bytes());
    out
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The facts a summary holds; `None`: not a summary of this form.
fn facts_of(bytes: &[u8]) -> Option<Facts> {
    if bytes.len() != SUMMARY_BYTES || bytes[4] > 7 {
        return None;
    }
    let printed = Printed { names: bytes[4] & 1 != 0, bodies_whole: bytes[4] & 2 != 0, replayable: bytes[4] & 4 != 0 };
    Some(Facts { files: word(bytes, 0), printed, findings: word(bytes, 5), owed: word(bytes, 9) })
}

/// The store key of a root's bytes.
pub(crate) fn root_key(bytes: &[u8]) -> Key {
    Key(ContentId::of(ROOT_DOMAIN, bytes))
}

/// The four names a rows answer hangs from: the checkout's (its root, its segments) and the
/// tree's (the same two, by the tree's digest).
pub(crate) struct RootNames<'f> {
    pub(crate) checkout: blaze_own::String<&'f Frame>,
    pub(crate) checkout_segments: blaze_own::String<&'f Frame>,
    pub(crate) tree: blaze_own::String<&'f Frame>,
    pub(crate) tree_segments: blaze_own::String<&'f Frame>,
}

/// The names of the checkout's rows answer for `tools`, and of the answer of the tree `tree`.
/// A checkout's name is local (its path is hashed in, engine_answer.rs `root_name`); the tree's
/// holds only the tree's digest and the tools (rule 25). The page form's root no longer holds
/// the tools (its pointer does, engine_answer.rs "THE TOOLS"); this form's [`certified`] reads
/// no tools, so its checkout names keep them: the tools' digest after `.rows`.
pub(crate) fn root_names<'f>(checkout: &str, tools: &Tools, tree: &TreeDigest, frame: &'f Frame) -> RootNames<'f> {
    let checkout_root = crate::engine_answer::root_name(checkout, frame);
    let pages = bstd::mem::format_in!(frame, "{}.{:x}", checkout_root.as_str(), crate::engine_answer::tools_digest(tools));
    let mut input = blaze_ids::Hasher::new(TREE_DOMAIN);
    input.update(&tree.0);
    for tool in tools.0.iter() {
        input.id(&tool.0);
    }
    let tree_name = bstd::mem::format_in!(frame, "engine.tree.{:x}", input.finish());
    RootNames {
        checkout: bstd::mem::format_in!(frame, "{}.rows", pages.as_str()),
        checkout_segments: bstd::mem::format_in!(frame, "{}.rows.segments", pages.as_str()),
        tree_segments: bstd::mem::format_in!(frame, "{}.segments", tree_name.as_str()),
        tree: tree_name,
    }
}

/// Why the stored rows answer is not read: the request answers in full and writes it anew.
#[derive(Debug)]
pub(crate) enum RowsUnreadable {
    /// The name holds other than one key (its root's).
    Names { bytes: usize },
    /// A record the name names is not in the store.
    Missing { key: Key },
    /// The bytes under the named key do not authenticate as that root.
    RootDigest { expected: Key, found: Key },
    /// The root or a segment (blaze-answer), or the seam's reading of a row.
    Answer(LastError),
    /// The root's summary is not this form's.
    Summary { bytes: usize },
}

impl fmt::Display for RowsUnreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let RowsUnreadable::Answer(LastError::Answer(AnswerError::Format { .. })) = self {
            return write!(f, "the stored engine answer is an old home's answer (another form of blaze-answer's root), rebuilt (rule 26)");
        }
        write!(f, "the stored engine answer's rows are not read (engine_answer_rows.rs `load_rows`): ")?;
        match self {
            RowsUnreadable::Names { bytes } => write!(f, "its name holds {bytes} bytes, not its root's one key")?,
            RowsUnreadable::Missing { key } => write!(f, "{key:?}, which its name names, is not in the store")?,
            RowsUnreadable::RootDigest { expected, found } => write!(f, "the named root {expected:?} has digest {found:?}")?,
            RowsUnreadable::Answer(why) => write!(f, "{why}")?,
            RowsUnreadable::Summary { bytes } => write!(f, "its root's summary is {bytes} bytes, not the certificate's {SUMMARY_BYTES}")?,
        }
        write!(f, "; answering in full and writing them anew")
    }
}

/// The root the checkout's name names, as stored.
pub(crate) struct RowsRoot<'m> {
    pub(crate) key: Key,
    directory: RootView<'m>,
    facts: Facts,
}

impl<'m> RowsRoot<'m> {
    pub(crate) fn open(key: Key, bytes: &'m [u8]) -> Result<Self, RowsUnreadable> {
        if bytes.len() > RootView::MAX_BYTES {
            let why = AnswerError::Cap { what: "root bytes", cap: RootView::MAX_BYTES as u64 };
            return Err(RowsUnreadable::Answer(why.into()));
        }
        let found = root_key(bytes);
        if found != key {
            return Err(RowsUnreadable::RootDigest { expected: key, found });
        }
        let directory = RootView::parse(bytes).map_err(|why| RowsUnreadable::Answer(why.into()))?;
        let summary = directory.summary();
        let facts = facts_of(summary).ok_or(RowsUnreadable::Summary { bytes: summary.len() })?;
        Ok(Self { key, directory, facts })
    }
}

/// The root the name `name` names, in place: `None` when there is none.
pub(crate) fn load_root<'m, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, name: &str) -> Result<Option<Result<RowsRoot<'m>, RowsUnreadable>>, CliError> {
    let Some(keys) = store.named(name)? else { return Ok(None) };
    if keys.len() != 32 {
        return Ok(Some(Err(RowsUnreadable::Names { bytes: keys.len() })));
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(keys);
    let key = Key(ContentId(id));
    let Some(held) = store.get(cx, key)? else { return Ok(Some(Err(RowsUnreadable::Missing { key }))) };
    let bytes: &'m [u8] = held.bytes;
    Ok(Some(RowsRoot::open(key, bytes)))
}

/// The certificate from the root alone, no segment read: the root answers `tree`, nothing is
/// owed, and the last request's every line is in its lines record.
pub(crate) fn certified(root: &RowsRoot<'_>, tree: &TreeDigest) -> bool {
    root.directory.tree() == *tree && root.facts.owed == 0 && root.facts.printed.replayable
}

/// The answer `root` names, its segments as the store holds them (mapped where committed). A
/// store's own error stops the command; an answer that cannot be read is the stated path.
pub(crate) fn load_rows<'m: 'a, 'a, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, root: &RowsRoot<'m>, alloc: SharedAlloc<'a>) -> Result<Result<RowsAnswer<'a>, RowsUnreadable>, CliError> {
    let mut failed: Option<CliError> = None;
    let opened = RowsAnswer::open(
        root,
        |one: &SegmentRef| match store.get(cx, Key(one.id)) {
            Ok(found) => found.map(|held| held.bytes),
            Err(error) => {
                failed.get_or_insert(error);
                None
            }
        },
        Plant::None,
        alloc,
    );
    if let Some(error) = failed {
        return Err(error);
    }
    Ok(opened)
}

/// The certificate's running counts a rows answer carries in its summary, kept as rows are put
/// and retired (never by walking every row): the files outlined and the rows with findings.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Counts {
    pub(crate) files: u32,
    pub(crate) findings: u32,
}

/// The counters of what a form's questions touched.
#[derive(Default)]
struct Counters {
    rows: AtomicU64,
    reads: AtomicU64,
    readers: AtomicU64,
    bytes: AtomicU64,
}

impl Counters {
    fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    fn get(&self) -> Touched {
        Touched {
            rows: self.rows.load(Ordering::Relaxed),
            reads: self.reads.load(Ordering::Relaxed),
            readers: self.readers.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The engine's stage of a row's stage byte.
fn stage_of(byte: u8) -> Result<Stage, LastError> {
    Stage::ALL.get(usize::from(byte)).copied().ok_or(LastError::Stage { found: byte })
}

fn stage_byte(stage: Stage) -> u8 {
    u8::try_from(stage.index()).unwrap_or(u8::MAX)
}

fn has(flags: Flags, flag: u8) -> bool {
    flags.0 & flag != 0
}

/// A row counted as a file outlined: an `Outlines` row (an owed one with no row of its own is
/// a plan, not a row).
fn is_file(stage: u8, flags: Flags, key: Key) -> bool {
    stage == stage_byte(Stage::Outlines) && !(has(flags, Flags::OWED) && key == Key::default())
}

/// The last answer in blaze-answer's form, read in place: every question probes the segments
/// newest to oldest and reads the fields it needs, nothing decoded whole.
pub(crate) struct RowsAnswer<'a> {
    view: AnswerView<'a>,
    facts: Facts,
    /// The root's and every live segment's bytes.
    bytes: u64,
    rows_segments: u64,
    cells_segments: u64,
    dicts: u64,
    runs: u64,
    plans: u64,
    findings: u64,
    counters: Counters,
}

/// A rows answer's reads of one row, in read order, each counted as walked.
pub(crate) struct CountedReads<'r> {
    inner: blaze_answer::Reads<'r>,
    walked: &'r AtomicU64,
}

impl Iterator for CountedReads<'_> {
    type Item = CellNo;

    fn next(&mut self) -> Option<CellNo> {
        let cell = self.inner.next()?;
        Counters::add(self.walked, 1);
        Some(cell)
    }
}

/// A rows answer's defines of one row, by cell.
pub(crate) struct RowDefines<'r> {
    row: RowView<'r>,
    at: usize,
    count: usize,
}

impl Iterator for RowDefines<'_> {
    type Item = (CellNo, CellDigest);

    fn next(&mut self) -> Option<(CellNo, CellDigest)> {
        if self.at == self.count {
            return None;
        }
        let one = self.row.define(self.at);
        self.at += 1;
        Some(one)
    }
}

impl<'a> RowsAnswer<'a> {
    /// Open children of an authenticated root, reusing the checked directory and summary.
    pub(crate) fn open<F: FnMut(&SegmentRef) -> Option<&'a [u8]>>(root: &RowsRoot<'a>, fetch: F, plant: Plant, alloc: SharedAlloc<'a>) -> Result<RowsAnswer<'a>, RowsUnreadable> {
        let view = AnswerView::from_root(root.directory, fetch, plant, alloc).map_err(|why| RowsUnreadable::Answer(why.into()))?;
        let segments = view.segments();
        let count = |kind: SegmentKind| len64(segments.iter().filter(|one| one.kind == kind).count());
        let bytes = len64(root.directory.byte_len()) + segments.iter().map(|one| one.len).sum::<u64>();
        let (rows_segments, cells_segments, dicts, runs) = (count(SegmentKind::Rows), count(SegmentKind::Cells), count(SegmentKind::Dict), count(SegmentKind::Run));
        let (plans, findings) = (count(SegmentKind::Plans), count(SegmentKind::Findings));
        Ok(RowsAnswer { view, facts: root.facts, bytes, rows_segments, cells_segments, dicts, runs, plans, findings, counters: Counters::default() })
    }

    /// No answer yet (a first request, or one the store could not read): no row, no segment,
    /// serial 0.
    pub(crate) fn empty(alloc: SharedAlloc<'a>) -> RowsAnswer<'a> {
        let view = AnswerView::empty(Plant::None, alloc);
        RowsAnswer { view, facts: Facts::default(), bytes: 0, rows_segments: 0, cells_segments: 0, dicts: 0, runs: 0, plans: 0, findings: 0, counters: Counters::default() }
    }

    /// The view: what the next request's [`RowsNext`] is made over.
    pub(crate) fn view(&self) -> &AnswerView<'a> {
        &self.view
    }

    /// The certificate's facts the root's summary holds.
    pub(crate) fn facts(&self) -> Facts {
        self.facts
    }

    /// The running counts the next request starts from.
    pub(crate) fn counts(&self) -> Counts {
        Counts { files: self.facts.files, findings: self.facts.findings }
    }

    /// The root's and every live segment's bytes: the answer's whole size.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    fn row_view(&self, task: TaskId) -> Option<RowView<'_>> {
        Counters::add(&self.counters.rows, 1);
        Counters::add(&self.counters.bytes, self.rows_segments * PROBE_BYTES + ROW_BYTES);
        self.view.row(task)
    }

    fn counted<'r>(&'r self, row: RowView<'r>) -> CountedReads<'r> {
        Counters::add(&self.counters.bytes, 4 + len64(row.read_count()) * READ_BYTES_MOST);
        CountedReads { inner: row.reads(), walked: &self.counters.reads }
    }

    fn moved(&self, cell: CellNo) -> Serial {
        Counters::add(&self.counters.bytes, self.cells_segments * (PROBE_BYTES + 8));
        self.view.moved(cell)
    }

    fn name_of(&self, task: TaskId, what: &'static str) -> Result<Name, LastError> {
        self.view.name(task).ok_or(LastError::NoName { what, id: task.0 })
    }

    fn planned(&self, task: TaskId, row: RowView<'_>) -> Result<Planned, LastError> {
        Ok(Planned { name: self.name_of(task, "a task")?, phase: Phase::new(stage_of(row.stage())?, row.wave()), source: row.source(), file: self.name_of(row.file(), "a file")? })
    }
}

impl<'a> LastAnswer for RowsAnswer<'a> {
    type Task = TaskId;
    type Cell = CellNo;
    type Reads<'r>
        = CountedReads<'r>
    where
        Self: 'r;
    type Defines<'r>
        = RowDefines<'r>
    where
        Self: 'r;
    /// The rows form keeps no seen digests: its rows stand by generation (last.rs).
    type Seen<'r>
        = core::iter::Empty<(CellNo, ContentId)>
    where
        Self: 'r;

    fn serial(&self) -> Serial {
        self.view.serial()
    }

    fn certificate(&self, tree: &TreeDigest) -> bool {
        self.view.tree() == *tree && self.facts.owed == 0 && self.facts.printed.replayable
    }

    fn task(&self, name: &Name) -> Option<TaskId> {
        Counters::add(&self.counters.bytes, self.dicts * NAME_PROBE_BYTES);
        self.view.task(name)
    }

    fn name(&self, task: TaskId) -> Option<Name> {
        Counters::add(&self.counters.bytes, 33);
        self.view.name(task)
    }

    fn cell(&self, id: &CellId) -> Option<CellNo> {
        Counters::add(&self.counters.bytes, self.dicts * (NAME_PROBE_BYTES + CELL_PROBE_BYTES));
        self.view.cell(&id.of, &id.cell)
    }

    fn cell_id(&self, cell: CellNo) -> Option<CellId> {
        Counters::add(&self.counters.bytes, 4 + 16 + 33);
        let (definer, key) = self.view.cell_key(cell)?;
        Some(CellId { of: self.view.name(definer)?, cell: key })
    }

    fn row(&self, task: TaskId) -> Result<Option<LastRow>, LastError> {
        let Some(row) = self.row_view(task) else { return Ok(None) };
        Ok(Some(LastRow {
            key: row.key(),
            out: row.out(),
            serial: row.serial(),
            phase: Phase::new(stage_of(row.stage())?, row.wave()),
            file: self.name_of(row.file(), "a file")?,
            source: row.source(),
            flags: row.flags(),
            findings: row.findings(),
            reads: len32(row.read_count()),
            defines: len32(row.define_count()),
        }))
    }

    fn reads(&self, task: TaskId) -> Option<CountedReads<'_>> {
        let row = self.row_view(task)?;
        Some(self.counted(row))
    }

    fn defines(&self, task: TaskId) -> Option<RowDefines<'_>> {
        let row = self.row_view(task)?;
        let count = row.define_count();
        Counters::add(&self.counters.bytes, len64(count) * len64(DEFINE_BYTES));
        Some(RowDefines { row, at: 0, count })
    }

    fn stands(&self, task: TaskId) -> Option<bool> {
        let row = self.row_view(task)?;
        let serial = row.serial();
        let mut reads = self.counted(row);
        Some(reads.all(|cell| self.moved(cell) <= serial))
    }

    /// The read list is in read order: the read is found in the reverse index, in place, one
    /// binary search for the cell and one for the task in each run (blaze-answer
    /// `AnswerView::reads_cell`), O(runs log), never a walk of the row's reads (plan row B1).
    fn read_of(&self, task: TaskId, cell: CellNo) -> Result<ReadOf, LastError> {
        let Some(row) = self.row_view(task) else { return Ok(ReadOf { found: Found::NotRead, compared: 0 }) };
        let (read, compared) = self.view.reads_cell(cell, task)?;
        Counters::add(&self.counters.bytes, self.runs * (2 * PROBE_BYTES + 8));
        let found = match read {
            true => Found::ReadAt(row.serial()),
            false => Found::NotRead,
        };
        Ok(ReadOf { found, compared })
    }

    fn digest(&self, cell: CellNo) -> CellDigest {
        // The definer's row's define entry (blaze-answer `AnswerView::digest`): one row.
        Counters::add(&self.counters.rows, 1);
        Counters::add(&self.counters.bytes, 4 + 16 + self.rows_segments * PROBE_BYTES + ROW_BYTES + PROBE_BYTES + len64(DEFINE_BYTES));
        self.view.digest(cell)
    }

    fn content(&self, cell: CellNo) -> ContentId {
        ContentId(self.digest(cell).0)
    }

    fn moved(&self, cell: CellNo) -> Serial {
        RowsAnswer::moved(self, cell)
    }

    fn seen(&self, _task: TaskId) -> Option<core::iter::Empty<(CellNo, ContentId)>> {
        None
    }

    fn readers<'m>(&self, cell: CellNo, alloc: Alloc<'m>) -> Result<AVec<'m, TaskId>, LastError> {
        let found = self.view.readers(cell, alloc)?;
        Counters::add(&self.counters.readers, len64(found.len()));
        Counters::add(&self.counters.bytes, self.runs * (PROBE_BYTES + 8) + 4 * len64(found.len()));
        Ok(found)
    }

    /// The file's plan: the plans runs (kind 5), one binary search each, O(the plan).
    fn file_tasks<'m>(&self, file: TaskId, alloc: Alloc<'m>) -> Result<AVec<'m, TaskId>, LastError> {
        let out = self.view.file_tasks(file, alloc)?;
        Counters::add(&self.counters.bytes, self.plans * (PROBE_BYTES + 8) + 4 * len64(out.len()));
        Ok(out)
    }

    /// The owed tasks segment (kind 7), then each owed task's row for its plan: O(owed).
    fn owed<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, Planned>, LastError> {
        let mut out = Vec::new_in(alloc);
        for task in self.view.owed() {
            Counters::add(&self.counters.bytes, 4);
            let row = self.row_view(task).ok_or(LastError::Answer(AnswerError::NoRow { task }))?;
            out.push(self.planned(task, row)?);
        }
        Ok(out)
    }

    /// The owed cells segment (kind 8): O(owed cells), no row read.
    fn owed_cells<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (CellNo, Serial)>, LastError> {
        let mut out = Vec::new_in(alloc);
        out.extend(self.view.owed_cells());
        Counters::add(&self.counters.bytes, 12 * len64(out.len()));
        Ok(out)
    }

    /// The findings runs (kind 6), then each such row's count (cold): O(rows with findings).
    fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (TaskId, u32)>, LastError> {
        let tasks = self.view.with_findings(alloc)?;
        Counters::add(&self.counters.bytes, self.findings * (PROBE_BYTES + 8) + 4 * len64(tasks.len()));
        let mut out = Vec::with_capacity_in(tasks.len(), alloc);
        for task in tasks.iter() {
            let row = self.row_view(*task).ok_or(LastError::Answer(AnswerError::NoRow { task: *task }))?;
            out.push((*task, row.findings()));
        }
        Ok(out)
    }

    fn touched(&self) -> Touched {
        self.counters.get()
    }
}

/// One request's next answer in blaze-answer's form, over the last ([`RowsAnswer::view`], or
/// `AnswerView::empty` for an answer written anew): the coordinator's `Commit` and one
/// `RunWriter` for the reverse entries (the engine's merge is single-threaded; per-worker runs
/// are the switch's choice). The dictionary is the answer's own: every name and cell gets its
/// dense id from the `Commit`, never from a second table.
pub(crate) struct RowsNext<'v, 'a, 'm> {
    last: &'v AnswerView<'a>,
    commit: Commit<'v, 'a, 'm>,
    run: RunWriter<'m>,
    reads: AVec<'m, CellNo>,
    defines: AVec<'m, (CellNo, CellDigest)>,
    counts: Counts,
    alloc: Alloc<'m>,
}

impl<'v, 'a, 'm> RowsNext<'v, 'a, 'm> {
    /// The request after `last`, whose summary held `counts`.
    pub(crate) fn new(last: &'v AnswerView<'a>, counts: Counts, plant: Plant, alloc: Alloc<'m>) -> RowsNext<'v, 'a, 'm> {
        let commit = Commit::new(last, plant, alloc);
        let run = RunWriter::new(commit.serial(), alloc);
        RowsNext { last, commit, run, reads: Vec::new_in(alloc), defines: Vec::new_in(alloc), counts, alloc }
    }

    /// The running counts after the rows put so far: the next summary's files and findings.
    pub(crate) fn counts(&self) -> Counts {
        self.counts
    }

    /// The cells this request moved so far: whose readers the engine runs again.
    pub(crate) fn moved(&self) -> impl Iterator<Item = CellNo> + '_ {
        self.commit.moved()
    }

    /// The counts with `old` replaced by `new` (stage, flags, key, findings).
    fn recount(&mut self, task: Name, old: Option<RowView<'_>>, new: Option<(u8, Flags, Key, u32)>) -> Result<(), LastError> {
        let (was_file, was_finding) = old.map_or((false, false), |row| (is_file(row.stage(), row.flags(), row.key()), has_findings(row.findings(), row.flags())));
        let (is_file_now, finding_now) = new.map_or((false, false), |(stage, flags, key, findings)| (is_file(stage, flags, key), has_findings(findings, flags)));
        let step = |column, previous: u32, was, now| {
            match (was, now) {
                (false, true) => previous.checked_add(1),
                (true, false) => previous.checked_sub(1),
                _ => Some(previous),
            }.ok_or(LastError::CertificateCount { task, column, previous })
        };
        self.counts = Counts {
            files: step("files", self.counts.files, was_file, is_file_now)?,
            findings: step("findings", self.counts.findings, was_finding, finding_now)?,
        };
        Ok(())
    }

    /// The owed cells from this request on, whole, each with the request it moved in (engine
    /// `Work::owed_cells`): the owed cells segment (kind 8).
    pub(crate) fn owe_cells<I: IntoIterator<Item = (CellId, Serial)>>(&mut self, cells: I) -> Result<(), LastError> {
        let mut given: AVec<'m, (CellNo, Serial)> = Vec::new_in(self.alloc);
        for (id, at) in cells {
            let of = self.commit.task(id.of)?;
            given.push((self.commit.cell(of, id.cell)?, at));
        }
        Ok(self.commit.owe_cells(&given)?)
    }

    fn read_in(&mut self, cell: CellNo) -> Result<(), LastError> {
        if self.reads.len() >= MAX_READS_PER_ROW {
            return Err(AnswerError::Cap { what: "reads a row", cap: len64(MAX_READS_PER_ROW) }.into());
        }
        self.reads.push(cell);
        Ok(())
    }
}

impl<'m> NextAnswer for RowsNext<'_, '_, 'm> {
    type Done = Effects<'m>;

    fn put<R: IntoIterator<Item = CellId>>(&mut self, name: Name, row: &NextRow, reads: R, defines: &[(Cell, ContentId)]) -> Result<(), LastError> {
        let serial = self.commit.serial();
        self.put_from(name, row, reads, defines, serial)
    }

    fn owe(&mut self, planned: &Planned) -> Result<(), LastError> {
        let task = self.commit.task(planned.name)?;
        let file = self.commit.task(planned.file)?;
        let last = self.last;
        let old = last.row(task);
        self.reads.clear();
        self.defines.clear();
        if let Some(old) = old {
            for cell in old.reads() {
                self.read_in(cell)?;
            }
            self.defines.extend(old.defines());
        }
        // Owed and never made: the row holds the plan alone (last.rs, `Flags::UNMADE`).
        let flags = Flags(old.map_or(Flags::UNMADE, |row| row.flags().0) | Flags::OWED);
        let key = old.map_or(Key::default(), |row| row.key());
        let findings = old.map_or(0, |row| row.findings());
        let stage = stage_byte(planned.phase.stage);
        let record = old.map_or(Key::default(), |row| row.record());
        let input = RowIn { key, out: old.map_or(ABSENT, |row| row.out()), file, stage, wave: planned.phase.wave, flags, source: planned.source, findings, record };
        // Owing a row re-validates nothing: it keeps the serial its reads were checked at, so
        // whether it stands stays the generation rule's (a cell its row read that moved in this
        // request or before is still after the row). Stamped with this request's serial, a row
        // owed because a read moved would read as standing: a stale answer (lane RD9, history
        // test, request 2). A plan never made has no reads: this request's serial.
        let from = old.map_or(self.commit.serial(), |row| row.serial());
        self.commit.put_row_from(task, &input, &self.reads, &self.defines, from)?;
        // Its reads are its last row's: no reverse entry changes.
        self.recount(planned.name, old, Some((stage, flags, key, findings)))
    }

    fn retire(&mut self, name: Name) -> Result<(), LastError> {
        let last = self.last;
        let Some(task) = last.task(&name) else { return Ok(()) };
        let Some(old) = last.row(task) else { return Ok(()) };
        self.commit.retire(task)?;
        self.run.row_retired(task, old);
        self.recount(name, Some(old), None)
    }

    fn finish(self, tree: &TreeDigest, summary: &[u8], lines: &LinesKey) -> Result<Effects<'m>, LastError> {
        let RowsNext { mut commit, run, .. } = self;
        if let Some(entries) = run.finish()? {
            commit.add_run(&entries)?;
        }
        Ok(commit.finish(tree, summary, lines)?)
    }
}

impl<'m> RowsNext<'_, '_, 'm> {
    /// Reserve this delta's new dictionary entries before writing any row. Task names sort by
    /// (kind, subject), and cell names by (definer name, selector). Previously assigned numbers
    /// survive. Sorting dictionary keys leaves each task's observation order intact.
    fn assign_ids<'r, 'w: 'r>(
        &mut self,
        rows: impl Iterator<Item = (&'r Name, &'r Row<'w>)> + Clone,
        owed: &blaze_engine::SMap<'_, Name, Planned>,
        other_cells: impl Iterator<Item = CellId>,
    ) -> Result<(), LastError> {
        let mut cells = blaze_own::BTreeSet::new_in(self.alloc);
        let definitions = rows.clone().flat_map(|(name, row)| row.defines.iter().map(move |(cell, _)| CellId { of: *name, cell: *cell }));
        for id in definitions.chain(other_cells) {
            if self.last.cell(&id.of, &id.cell).is_none() {
                cells.insert(id);
                if cells.len() > (blaze_answer::MAX_CELLS - self.last.cells()) as usize {
                    return Err(AnswerError::Cap { what: "cell numbers", cap: u64::from(blaze_answer::MAX_CELLS) }.into());
                }
            }
        }
        let mut tasks = blaze_own::BTreeSet::new_in(self.alloc);
        let made = rows.filter(|(name, _)| !owed.contains_key(*name)).flat_map(|(name, row)| [*name, row.file]);
        let planned = owed.values().flat_map(|plan| [plan.name, plan.file]);
        for name in made.chain(planned).chain(cells.iter().map(|id| id.of)) {
            if self.last.task(&name).is_none() {
                tasks.insert(name);
                if tasks.len() > (blaze_answer::MAX_TASKS - self.last.tasks()) as usize {
                    return Err(AnswerError::Cap { what: "task ids", cap: u64::from(blaze_answer::MAX_TASKS) }.into());
                }
            }
        }
        for name in tasks {
            self.commit.task(name)?;
        }
        for id in cells {
            let definer = self.commit.task(id.of)?;
            self.commit.cell(definer, id.cell)?;
        }
        Ok(())
    }

    /// [`NextAnswer::put`] with the serial the row stands from (blaze-answer
    /// `Commit::put_row_from`): this request's, or the one before for a row already stale.
    pub(crate) fn put_from<R: IntoIterator<Item = CellId>>(&mut self, name: Name, row: &NextRow, reads: R, defines: &[(Cell, ContentId)], from: Serial) -> Result<(), LastError> {
        let task = self.commit.task(name)?;
        let file = self.commit.task(row.file)?;
        self.reads.clear();
        for id in reads {
            let of = self.commit.task(id.of)?;
            let cell = self.commit.cell(of, id.cell)?;
            self.read_in(cell)?;
        }
        self.defines.clear();
        for (cell, digest) in defines.iter() {
            let no = self.commit.cell(task, *cell)?;
            self.defines.push((no, CellDigest::of(digest)));
        }
        let last = self.last;
        let old = last.row(task);
        let stage = stage_byte(row.phase.stage);
        let input = RowIn { key: row.key, out: row.out, file, stage, wave: row.phase.wave, flags: row.flags, source: row.source, findings: row.findings, record: row.record };
        self.commit.put_row_from(task, &input, &self.reads, &self.defines, from)?;
        self.run.row_reads(task, old, &self.reads);
        self.recount(name, old, Some((stage, row.flags, row.key, row.findings)))
    }

    /// `name` is owed no more though this request did not enter it: a plan never made is
    /// retired; a made row is carried with [`Flags::OWED`] cleared and its own serial (its
    /// generation is unchanged: no read of it was checked).
    pub(crate) fn unowe(&mut self, name: Name) -> Result<(), LastError> {
        let last = self.last;
        let Some(task) = last.task(&name) else { return Ok(()) };
        let Some(old) = last.row(task) else { return Ok(()) };
        if has(old.flags(), Flags::UNMADE) {
            return NextAnswer::retire(self, name);
        }
        self.reads.clear();
        for cell in old.reads() {
            self.read_in(cell)?;
        }
        self.defines.clear();
        self.defines.extend(old.defines());
        let flags = Flags(old.flags().0 & !Flags::OWED);
        let input = RowIn { key: old.key(), out: old.out(), file: old.file(), stage: old.stage(), wave: old.wave(), flags, source: old.source(), findings: old.findings(), record: old.record() };
        self.commit.put_row_from(task, &input, &self.reads, &self.defines, old.serial())?;
        self.recount(name, Some(old), Some((old.stage(), flags, old.key(), old.findings())))
    }

    /// `id` moves in this request though no define of it changed (blaze-answer
    /// `Commit::move_cell`). `true`: no define had moved it already.
    pub(crate) fn move_cell(&mut self, id: &CellId) -> Result<bool, LastError> {
        let of = self.commit.task(id.of)?;
        let cell = self.commit.cell(of, id.cell)?;
        Ok(self.commit.move_cell(cell)?)
    }
}

/// `answer` whole, as rows: the path a request takes when no rows answer is kept (the first
/// request after the switch, an unreadable answer, an old home). Every row is put; an owed task
/// with no row is owed; the owed cells are given whole. `records` is the command's record key of
/// each row (pages.rs `records`); a row it holds none for keeps zeros, as an unmade plan does.
pub(crate) fn put_whole(answer: &Answer<'_>, records: &dyn Fn(&Name) -> Option<Key>, next: &mut RowsNext<'_, '_, '_>) -> Result<(), LastError> {
    let reads = answer.rows.values().flat_map(|row| row.reads.iter().map(|(id, _)| *id));
    next.assign_ids(answer.rows.iter(), &answer.owed_tasks, reads.chain(answer.owed_cells.keys().copied()))?;
    for (name, row) in answer.rows.iter() {
        let put = next_row(row, answer.owed_tasks.get(name), records(name).unwrap_or_default());
        next.put(*name, &put, row.reads.iter().map(|(id, _)| *id), &row.defines)?;
    }
    for (name, planned) in answer.owed_tasks.iter() {
        if !answer.rows.contains_key(name) {
            next.owe(planned)?;
        }
    }
    next.owe_cells(answer.owed_cells.iter().map(|(id, at)| (*id, Serial(at.0))))
}

#[cfg(test)]
mod dictionary_tests {
    use super::*;
    use blaze_drive::Kind;
    use blaze_sim::{Broke, Config, Tape};

    fn bytes_of<'b>(write: &'b Write<'_>) -> &'b [u8] {
        match write {
            Write::Segment { bytes, .. } | Write::Root { bytes } => bytes.as_slice(),
        }
    }

    fn segment<'b>(writes: &'b [Write<'_>], one: &SegmentRef) -> Option<&'b [u8]> {
        writes.iter().find_map(|write| match write {
            Write::Segment { at, bytes } if at == one => Some(bytes.as_slice()),
            _ => None,
        })
    }

    fn same_writes(tape: &Tape, arrival_ids: bool) -> Result<(), Broke> {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let shared = heap.shared();
        let empty = AnswerView::empty(Plant::None, shared);
        let name = |v| Name::new(Kind::BodyCheck, ContentId([v; 32]));
        let file = Name::new(Kind::FileItems, ContentId([7; 32]));
        let selector = Cell([1; 16]);
        // The high-sorting name already owns id 0. New keys must be appended to that dictionary.
        let mut base = Commit::new(&empty, Plant::None, alloc);
        let existing = base.task(name(9)).expect("intern the existing task");
        base.cell(existing, selector).expect("intern the existing cell");
        let base = base.finish(&TreeDigest([0; 32]), &[], &LinesKey([0; 32])).expect("commit the base");
        let last = AnswerView::open(base.root(), |one| segment(base.writes(), one), Plant::None, shared).expect("open the base");
        let names = [name(3), name(1), name(2)];
        let rows = names.map(|task| {
            let reads = [name(9), name(8)].map(|of| (CellId { of, cell: selector }, ABSENT));
            Row {
                key: Key(task.subject), phase: Phase::new(Stage::Bodies, 0), file, source: task.subject, out: task.subject,
                serial: blaze_engine::Gen(2), defines: Vec::from_iter_in([(selector, task.subject)], shared),
                reads: Vec::from_iter_in(reads, shared), by_cell: Vec::from_iter_in([1, 0], shared),
                findings: 0, inline: false, failed: false,
            }
        });
        let owed = blaze_own::BTreeMap::new_in(shared);
        let encode = |order: &[usize], reserve: bool| {
            let mut next = RowsNext::new(&last, Counts::default(), Plant::None, alloc);
            let arrivals = order.iter().map(|&i| (&names[i], &rows[i]));
            if reserve {
                let reads = arrivals.clone().flat_map(|(_, row)| row.reads.iter().map(|(id, _)| *id));
                next.assign_ids(arrivals.clone(), &owed, reads).expect("assign ids from stable keys");
            }
            for (task, row) in arrivals {
                next.put(*task, &next_row(row, None, row.key), row.reads.iter().map(|(id, _)| *id), &row.defines).expect("put the changed row");
            }
            next.finish(&TreeDigest([1; 32]), &[], &LinesKey([0; 32])).expect("encode the delta")
        };
        let canonical = encode(&[0, 1, 2], true);
        let mut order = [0, 1, 2];
        tape.shuffle(&mut order, "worker completion order");
        let scheduled = encode(&order, !arrival_ids);
        if !canonical.writes().iter().map(bytes_of).eq(scheduled.writes().iter().map(bytes_of)) {
            return Err(Broke::Oracle(std::string::String::from("worker order changed immutable answer bytes")));
        }
        let fetch = |one: &SegmentRef| segment(scheduled.writes(), one).or_else(|| segment(base.writes(), one));
        let opened = AnswerView::open(scheduled.root(), fetch, Plant::None, shared).expect("open the delta over its base");
        let tasks = [name(9), file, name(1), name(2), name(3), name(8)];
        let cells = [name(9), name(1), name(2), name(3), name(8)];
        for (i, task) in tasks.iter().enumerate() {
            assert_eq!(opened.task(task), Some(TaskId(i as u32)), "existing task numbers persist and new tasks follow stable name order");
        }
        for (i, of) in cells.iter().enumerate() {
            assert_eq!(opened.cell(of, &selector), Some(CellNo(i as u32)), "existing cell numbers persist and new cells follow stable key order");
        }
        Ok(())
    }

    /// The invariant: rows-answer bytes are independent of worker completion arrival order.
    #[test]
    fn worker_order_preserves_answer_bytes() {
        blaze_sim::check(Config::cases(24), |tape| same_writes(tape, false));
    }

    #[test]
    fn the_oracle_catches_arrival_order_ids() {
        blaze_sim::control(Config::cases(24), false, true, "worker order changed immutable answer bytes", same_writes);
    }
}

/// A row of the engine's as the rows form's [`NextRow`]: its flags from the row and whether it
/// is owed, its plan the owed one's when owed (engine.rs keeps both so).
fn next_row(row: &Row<'_>, owed: Option<&Planned>, record: Key) -> NextRow {
    let mut flags = 0u8;
    if row.failed {
        flags |= Flags::FAILED;
    }
    if row.inline {
        flags |= Flags::INLINE;
    }
    if owed.is_some() {
        flags |= Flags::OWED;
    }
    let (phase, file, source) = owed.map_or((row.phase, row.file, row.source), |planned| (planned.phase, planned.file, planned.source));
    NextRow { key: row.key, out: row.out, phase, file, source, flags: Flags(flags), findings: row.findings, record }
}

/// What only a request's end gives the rows form's commit: the tree it answers, what it printed,
/// its lines record's key, and each row's record key.
pub(crate) struct Close<'c> {
    pub(crate) tree: TreeDigest,
    pub(crate) printed: Printed,
    pub(crate) lines: LinesKey,
    /// The phase driver's final disposition for each changed row. A missing disposition stops
    /// publication so a previously reachable record cannot disappear through a missing lookup.
    pub(crate) records: &'c dyn Fn(&Name) -> Option<RowPayload>,
}

#[derive(Clone, Copy)]
pub(crate) enum RowPayload {
    Stored(Key),
    NoPayload,
}

/// A request's end in the rows form (blaze-engine overlay.rs `Fold`): the last answer and the
/// request's writes. The engine's `finish` gives it before the command knows its summary, its
/// lines and its records' keys; [`RowsFolded::commit`] then writes the next answer.
pub(crate) struct RowsFolded<'w, 'a> {
    last: RowsAnswer<'a>,
    work: Work<'w>,
}

impl<'w, 'a> Fold<'w> for RowsAnswer<'a> {
    type Out = RowsFolded<'w, 'a>;

    fn fold(self, work: Work<'w>) -> Result<RowsFolded<'w, 'a>, EngineError> {
        Ok(RowsFolded { last: self, work })
    }
}

/// What a rows commit wrote, for the request's report.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct RowsWritten {
    /// Rows put (entered or re-validated), owed afresh, or carried with their owing cleared.
    pub(crate) put: u64,
    pub(crate) retired: u64,
    /// Cells moved by the generation rule alone: a row put saw another digest than the end's.
    pub(crate) stale_moves: u64,
}

impl<'w, 'a> RowsFolded<'w, 'a> {
    /// The last answer the request read.
    pub(crate) fn last(&self) -> &RowsAnswer<'a> {
        &self.last
    }

    /// The request's writes.
    pub(crate) fn work(&self) -> &Work<'w> {
        &self.work
    }

    /// The next answer's segments and root: each row the request entered or re-validated, put
    /// at the serial the generation rule gives it (blaze-engine overlay.rs `Generations::row`);
    /// each it took out, retired; each newly owed task, owed; each owed no more and not entered,
    /// carried with its owing cleared; the owed cells whole; the cells the generation rule moves
    /// that no define moved; then the summary of the counts kept row by row. A row the request
    /// did not touch is carried by its generation: not rewritten, not read.
    pub(crate) fn commit<'m>(&self, close: &Close<'_>, plant: Plant, alloc: Alloc<'m>) -> Result<(Effects<'m>, RowsWritten), LastError> {
        let view = self.last.view();
        let generations = self.work.generations();
        let mut next = RowsNext::new(view, self.last.counts(), plant, alloc);
        let before = |id: &CellId| view.cell(&id.of, &id.cell).map_or(CellDigest::ABSENT, |cell| view.digest(cell));
        let moved = generations.moved(before);
        // Every newly named read contributes an addition to the reverse index. Unchanged
        // memberships already have dictionary entries, including reads of absent values.
        let additions = self.work.readers.iter().filter_map(|(id, edits)| edits.iter().any(|(_, reads)| *reads).then_some(*id));
        let cells = additions.chain(self.work.owed_cells.keys().copied()).chain(moved.iter().copied());
        let made = self.work.rows.iter().filter_map(|(name, row)| row.as_ref().map(|row| (name, row)));
        next.assign_ids(made, &self.work.owed_tasks, cells)?;
        let mut written = RowsWritten::default();
        for (name, row) in self.work.rows.iter() {
            match row {
                Some(row) => {
                    let record = match (close.records)(name) {
                        Some(RowPayload::Stored(key)) if key != Key::default() => key,
                        Some(RowPayload::NoPayload) => Key::default(),
                        _ => return Err(LastError::RowPayload { task: *name }),
                    };
                    let put = next_row(row, self.work.owed_tasks.get(name), record);
                    next.put_from(*name, &put, row.reads.iter().map(|(id, _)| *id), &row.defines, Serial(generations.row(row).0))?;
                    written.put += 1;
                }
                None => {
                    NextAnswer::retire(&mut next, *name)?;
                    written.retired += 1;
                }
            }
        }
        for (name, planned) in self.work.owed_tasks.iter() {
            if self.work.rows.contains_key(name) {
                continue;
            }
            let kept = self.last.view().task(name).filter(|task| view.is_owed(*task)).and_then(|task| view.row(task)).is_some_and(|row| {
                stage_byte(planned.phase.stage) == row.stage() && planned.phase.wave == row.wave() && planned.source == row.source() && view.task(&planned.file) == Some(row.file())
            });
            if !kept {
                next.owe(planned)?;
                written.put += 1;
            }
        }
        for task in view.owed() {
            let name = self.last.name_of(task, "an owed task")?;
            if !self.work.owed_tasks.contains_key(&name) && !self.work.rows.contains_key(&name) {
                next.unowe(name)?;
                written.put += 1;
            }
        }
        next.owe_cells(self.work.owed_cells.iter().map(|(id, at)| (*id, Serial(at.0))))?;
        for id in moved.iter() {
            if next.move_cell(id)? {
                written.stale_moves += 1;
            }
        }
        let counts = next.counts();
        let owed = self.work.owed_tasks.len().checked_add(self.work.owed_cells.len()).and_then(|n| u32::try_from(n).ok())
            .ok_or(AnswerError::Cap { what: "obligations in a certificate", cap: u64::from(u32::MAX) })?;
        let facts = Facts { files: counts.files, printed: close.printed, findings: counts.findings, owed };
        let effects = NextAnswer::finish(next, &close.tree, &summary(&facts), &close.lines)?;
        Ok((effects, written))
    }
}

/// Where a commit's writes go: the command's store ([`StoreSink`]), or a test's record of them.
pub(crate) trait AnswerSink {
    type Error;
    fn put(&mut self, key: Key, bytes: &[u8]) -> Result<(), Self::Error>;
    fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), Self::Error>;
    fn index_rows(&mut self, owner: Key, records: &[Key], scopes: &[(Name, Key)], alloc: Alloc<'_>) -> Result<(), Self::Error>;
    fn pin_children(&mut self, names: &RootNames<'_>, segments: &[SegmentRef], lines: Key, alloc: Alloc<'_>) -> Result<(), Self::Error>;
    fn keep_toolchain(&mut self, scopes: &[(Name, Key)], alloc: Alloc<'_>) -> Result<(), Self::Error>;
}

/// The command's store as a sink: its puts and pins, visible from its one commit after the
/// answer.
pub(crate) struct StoreSink<'s, 'c, 'm, 'r, D: Driver, S: Sharing> {
    pub(crate) cx: &'c Cx,
    pub(crate) store: &'s mut CommandStore<'m, 'r, D, S>,
    pub(crate) lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    pub(crate) toolchain: blaze_toolchain::ToolchainId,
}

#[derive(Debug)]
pub(crate) enum PublishError {
    Command(CliError),
    Cas(blaze_cas::plain::LentError),
    ExpansionIndex { record: Key, index: Key, namespace: u64, tag: u16, bytes: usize },
    RowsIndex { segment: Key, index: Key },
    RowsReach { segment: Key, cap: usize },
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(why) => write!(f, "{why}"),
            Self::Cas(why) => write!(f, "answer publication: {why}"),
            Self::ExpansionIndex { record, index, namespace, tag, bytes } => write!(f, "expansion index {index:?} of record {record:?} has namespace {namespace}, tag {tag} and {bytes} bytes"),
            Self::RowsIndex { segment, index } => write!(f, "rows segment {segment:?} is missing its admitted local payload index {index:?}"),
            Self::RowsReach { segment, cap } => write!(f, "rows segment {segment:?} reaches more than {cap} record keys"),
        }
    }
}

impl<D: Driver, S: Sharing> AnswerSink for StoreSink<'_, '_, '_, '_, D, S> {
    type Error = PublishError;

    fn put(&mut self, key: Key, bytes: &[u8]) -> Result<(), PublishError> {
        self.store.put(self.cx, key, bytes).map_err(PublishError::Command)
    }

    fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), PublishError> {
        self.store.pin(name, keys).map_err(PublishError::Command)
    }

    fn index_rows(&mut self, owner: Key, records: &[Key], scopes: &[(Name, Key)], alloc: Alloc<'_>) -> Result<(), PublishError> {
        use blaze_cas::plain::{INDEX_KEYS_MOST, beside_key};
        use blaze_keyed::kind::ReservedTag;

        // CAS reach follows one index level. Include each expansion's key and its owner's
        // side index so a later carried names row can be indexed again after compaction.
        let mut keys = Vec::with_capacity_in(records.len(), alloc);
        keys.extend_from_slice(records);
        for &(_, record) in scopes {
            let index = beside_key(blaze_cas::Key(record.0));
            // put_all creates this optional local index only for a nonempty expansion set.
            let Some(held) = self.lent.get(&index).map_err(PublishError::Cas)? else { continue };
            if held.namespace != blaze_cas::plain::Namespace::LOCAL || !ReservedTag::Index.is(held.tag) || held.bytes.len() % 32 != 0 || held.bytes.len() / 32 > INDEX_KEYS_MOST {
                return Err(PublishError::ExpansionIndex { record, index: Key(index.0), namespace: held.namespace.0, tag: held.tag.0, bytes: held.bytes.len() });
            }
            keys.reserve(1 + held.bytes.len() / 32);
            keys.push(Key(index.0));
            for bytes in held.bytes.chunks_exact(32) {
                keys.push(Key(ContentId(bytes.try_into().expect("a whole index key"))));
            }
        }
        keys.sort_unstable();
        keys.dedup();
        if keys.len() > INDEX_KEYS_MOST {
            return Err(PublishError::RowsReach { segment: owner, cap: INDEX_KEYS_MOST });
        }
        self.store.index_beside(self.cx, owner, &keys).map_err(PublishError::Command)
    }

    fn pin_children(&mut self, names: &RootNames<'_>, segments: &[SegmentRef], lines: Key, alloc: Alloc<'_>) -> Result<(), PublishError> {
        use blaze_cas::plain::{INDEX_KEYS_MOST, Namespace, ROOT_LIFE_NS, beside_key};
        use blaze_keyed::kind::ReservedTag;

        let mut direct = Vec::with_capacity_in(segments.len() + 1, alloc);
        direct.extend(segments.iter().map(|one| blaze_cas::Key(one.id)));
        direct.push(blaze_cas::Key(lines.0));
        let mut indexes = Vec::with_capacity_in(segments.len() + 1, alloc);
        indexes.push(self.lent.index(&direct).map_err(PublishError::Cas)?);
        for one in segments.iter().filter(|one| one.kind == SegmentKind::Rows) {
            let index = beside_key(blaze_cas::Key(one.id));
            // A portable segment may arrive before its local reach index. root_through
            // accepts index names, so check presence before publishing those names.
            let held = self.lent.get(&index).map_err(PublishError::Cas)?;
            if !held.is_some_and(|held| held.namespace == Namespace::LOCAL && ReservedTag::Index.is(held.tag) &&held.bytes.len() % 32 == 0 && held.bytes.len() / 32 <= INDEX_KEYS_MOST) {
                return Err(PublishError::RowsIndex { segment: Key(one.id), index: Key(index.0) });
            }
            indexes.push(index);
        }
        for name in [names.checkout_segments.as_str(), names.tree_segments.as_str()] {
            self.lent.root_through(name, &indexes, ROOT_LIFE_NS).map_err(PublishError::Cas)?;
        }
        Ok(())
    }

    fn keep_toolchain(&mut self, scopes: &[(Name, Key)], alloc: Alloc<'_>) -> Result<(), PublishError> {
        let keys = Vec::from_iter_in(scopes.iter().filter_map(|(name, key)| self.store.is_toolchain(*name).then_some(*key)), alloc);
        self.store.keep_toolchain(self.toolchain, &keys).map_err(PublishError::Command)
    }
}

/// What one write put.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RowsSaved {
    /// Records put: the new segments and the root.
    pub(crate) puts: u32,
    /// The live segments the root names, and the bytes of the new ones.
    pub(crate) segments: u32,
    pub(crate) new_segments: u32,
    pub(crate) new_bytes: u64,
}

/// Each rows segment gets one immutable CAS index at creation. A new root shares the
/// indexes of every rows segment it carries. Compaction drops the shadowed row records
/// when it publishes a root that no longer names their segments.
pub(crate) fn write_effects<K: AnswerSink>(sink: &mut K, names: &RootNames<'_>, effects: &Effects<'_>, alloc: Alloc<'_>) -> Result<RowsSaved, K::Error> {
    let root = root_key(effects.root());
    let (records, scopes) = effects.row_records();
    let mut saved = RowsSaved { segments: len32(effects.segments().len()), ..RowsSaved::default() };
    for write in effects.writes() {
        match write {
            Write::Segment { at, bytes } => {
                sink.put(Key(at.id), &bytes[..])?;
                if at.kind == SegmentKind::Rows {
                    sink.index_rows(Key(at.id), records, scopes, alloc)?;
                }
                saved.puts += 1;
                saved.new_segments += 1;
                saved.new_bytes += at.len;
            }
            Write::Root { bytes } => {
                sink.put(root, &bytes[..])?;
                saved.puts += 1;
                sink.keep_toolchain(scopes, alloc)?;
                sink.pin_children(names, effects.segments(), Key(ContentId(effects.lines().0)), alloc)?;
                sink.pin(names.checkout.as_str(), &[root])?;
                sink.pin(names.tree.as_str(), &[root])?;
            }
        }
    }
    Ok(saved)
}

/// The last answer in the page form behind the same seam: the loaded [`Kept`] (engine_answer.rs
/// `load_answer`, which read every page), the pointer's facts, and the tree the sweep found the
/// pointer answers (`Some` when nothing it read moved: the page form keeps no tree digest).
pub(crate) struct PagesAnswer<'k, 'a> {
    kept: &'k Kept<'a>,
    facts: Facts,
    answered: Option<TreeDigest>,
    counters: Counters,
}

impl<'k, 'a> PagesAnswer<'k, 'a> {
    /// `bytes`: the pages' bytes `load_answer` read (`Root::bytes`): the load touched them all.
    pub(crate) fn new(kept: &'k Kept<'a>, bytes: u64, facts: Facts, answered: Option<TreeDigest>) -> PagesAnswer<'k, 'a> {
        let answer = &kept.answer;
        let counters = Counters::default();
        Counters::add(&counters.rows, len64(answer.rows.len()));
        Counters::add(&counters.reads, answer.rows.values().map(|row| len64(row.reads.len())).sum::<u64>());
        Counters::add(&counters.bytes, bytes);
        PagesAnswer { kept, facts, answered, counters }
    }
}

/// A page row's reads as the seam's cells.
pub(crate) struct PageReads<'r> {
    inner: core::slice::Iter<'r, (CellId, ContentId)>,
    walked: &'r AtomicU64,
}

impl Iterator for PageReads<'_> {
    type Item = CellId;

    fn next(&mut self) -> Option<CellId> {
        let (id, _) = self.inner.next()?;
        Counters::add(self.walked, 1);
        Some(*id)
    }
}

/// A page row's defines as the seam's cells.
pub(crate) struct PageDefines<'r> {
    of: Name,
    inner: core::slice::Iter<'r, (Cell, ContentId)>,
}

impl Iterator for PageDefines<'_> {
    type Item = (CellId, CellDigest);

    fn next(&mut self) -> Option<(CellId, CellDigest)> {
        let (cell, digest) = self.inner.next()?;
        Some((CellId { of: self.of, cell: *cell }, CellDigest::of(digest)))
    }
}

impl LastAnswer for PagesAnswer<'_, '_> {
    type Task = Name;
    type Cell = CellId;
    type Reads<'r>
        = PageReads<'r>
    where
        Self: 'r;
    type Defines<'r>
        = PageDefines<'r>
    where
        Self: 'r;
    type Seen<'r>
        = core::iter::Copied<core::slice::Iter<'r, (CellId, ContentId)>>
    where
        Self: 'r;

    fn serial(&self) -> Serial {
        Serial(self.kept.answer.serial.0)
    }

    fn certificate(&self, tree: &TreeDigest) -> bool {
        self.answered == Some(*tree) && self.facts.owed == 0 && self.facts.printed.replayable
    }

    fn task(&self, name: &Name) -> Option<Name> {
        Some(*name)
    }

    fn name(&self, task: Name) -> Option<Name> {
        Some(task)
    }

    fn cell(&self, id: &CellId) -> Option<CellId> {
        Some(*id)
    }

    fn cell_id(&self, cell: CellId) -> Option<CellId> {
        Some(cell)
    }

    fn row(&self, task: Name) -> Result<Option<LastRow>, LastError> {
        let answer = &self.kept.answer;
        let owed = answer.owed_tasks.get(&task);
        let Some(row) = answer.rows.get(&task) else {
            return Ok(owed.map(|planned| LastRow {
                key: Key::default(),
                out: ABSENT,
                serial: Serial(answer.serial.0),
                phase: planned.phase,
                file: planned.file,
                source: planned.source,
                flags: Flags(Flags::OWED | Flags::UNMADE),
                findings: 0,
                reads: 0,
                defines: 0,
            }));
        };
        let mut flags = 0u8;
        if row.failed {
            flags |= Flags::FAILED;
        }
        if row.inline {
            flags |= Flags::INLINE;
        }
        if owed.is_some() {
            flags |= Flags::OWED;
        }
        let (phase, file, source) = owed.map_or((row.phase, row.file, row.source), |planned| (planned.phase, planned.file, planned.source));
        Ok(Some(LastRow {
            key: row.key,
            out: row.out,
            serial: Serial(row.serial.0),
            phase,
            file,
            source,
            flags: Flags(flags),
            findings: row.findings,
            reads: len32(row.reads.len()),
            defines: len32(row.defines.len()),
        }))
    }

    fn reads(&self, task: Name) -> Option<PageReads<'_>> {
        let reads = match self.kept.answer.rows.get(&task) {
            Some(row) => &row.reads[..],
            // An owed task with no row: a row of the plan's alone, as the rows form keeps it.
            None if self.kept.answer.owed_tasks.contains_key(&task) => &[],
            None => return None,
        };
        Some(PageReads { inner: reads.iter(), walked: &self.counters.reads })
    }

    fn defines(&self, task: Name) -> Option<PageDefines<'_>> {
        let defines = match self.kept.answer.rows.get(&task) {
            Some(row) => &row.defines[..],
            None if self.kept.answer.owed_tasks.contains_key(&task) => &[],
            None => return None,
        };
        Some(PageDefines { of: task, inner: defines.iter() })
    }

    fn stands(&self, task: Name) -> Option<bool> {
        let answer = &self.kept.answer;
        match answer.rows.get(&task) {
            Some(row) => Some(row.reads.iter().all(|(id, seen)| answer.cell(id) == *seen)),
            None => answer.owed_tasks.contains_key(&task).then_some(true),
        }
    }

    /// A read list is in read order (engine.rs invariant 9), not sorted: the cell is found by a
    /// binary search over the row's order by cell (`Row::read_of`), O(log reads).
    fn read_of(&self, task: Name, cell: CellId) -> Result<ReadOf, LastError> {
        Ok(match self.kept.answer.rows.get(&task) {
            Some(row) => {
                let (seen, compared) = row.read_of(&cell);
                ReadOf { found: seen.map_or(Found::NotRead, Found::Saw), compared }
            }
            None => ReadOf { found: Found::NotRead, compared: 0 },
        })
    }

    fn digest(&self, cell: CellId) -> CellDigest {
        CellDigest::of(&self.kept.answer.cell(&cell))
    }

    fn content(&self, cell: CellId) -> ContentId {
        self.kept.answer.cell(&cell)
    }

    fn moved(&self, cell: CellId) -> Serial {
        let answer = &self.kept.answer;
        match answer.cells.get(&cell) {
            Some((_, at)) => Serial(at.0),
            None => Serial(answer.owed_cells.get(&cell).map_or(0, |at| at.0)),
        }
    }

    fn seen(&self, task: Name) -> Option<core::iter::Copied<core::slice::Iter<'_, (CellId, ContentId)>>> {
        let reads = match self.kept.answer.rows.get(&task) {
            Some(row) => &row.reads[..],
            None if self.kept.answer.owed_tasks.contains_key(&task) => &[],
            None => return None,
        };
        Counters::add(&self.counters.reads, len64(reads.len()));
        Some(reads.iter().copied())
    }

    fn readers<'m>(&self, cell: CellId, alloc: Alloc<'m>) -> Result<AVec<'m, Name>, LastError> {
        let mut out = Vec::new_in(alloc);
        if let Some(readers) = self.kept.answer.readers.get(&cell) {
            out.extend(readers.iter().copied());
        }
        Counters::add(&self.counters.readers, len64(out.len()));
        Ok(out)
    }

    fn file_tasks<'m>(&self, file: Name, alloc: Alloc<'m>) -> Result<AVec<'m, Name>, LastError> {
        let mut out = Vec::new_in(alloc);
        if let Some(names) = self.kept.answer.by_file.get(&file) {
            out.extend(names.iter().copied());
        }
        Ok(out)
    }

    fn owed<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, Planned>, LastError> {
        let mut out = Vec::new_in(alloc);
        out.extend(self.kept.answer.owed_tasks.values().copied());
        Ok(out)
    }

    fn owed_cells<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (CellId, Serial)>, LastError> {
        let mut out = Vec::new_in(alloc);
        out.extend(self.kept.answer.owed_cells.iter().map(|(id, at)| (*id, Serial(at.0))));
        Ok(out)
    }

    fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (Name, u32)>, LastError> {
        let answer = &self.kept.answer;
        let mut out = Vec::new_in(alloc);
        out.extend(answer.with_findings.iter().map(|name| (*name, answer.rows.get(name).map_or(0, |row| row.findings))));
        Ok(out)
    }

    fn touched(&self) -> Touched {
        self.counters.get()
    }
}
