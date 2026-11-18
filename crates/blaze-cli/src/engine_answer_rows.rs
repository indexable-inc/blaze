//! `engine-check`'s stored answer in blaze-answer's form (runtime.md, "The answer's stored form",
//! F0; the switch is D5), behind the seam blaze-engine last.rs states, beside the page form
//! (engine_answer.rs), not yet switched on. Both forms answer the seam: [`RowsAnswer`] over the
//! segments read in place, [`PagesAnswer`] over the page form's loaded [`Kept`], so the switch is
//! one type parameter (rule 78).
//!
//! ```text
//! names           engine.<16 hex>.rows            the root's key alone (engine_answer.rs `root_name`)
//!                 engine.<16 hex>.rows.segments   every live segment's key and the lines record's
//!                 engine.tree.<16 hex of H(tree digest, every kind's tool)>   and its `.segments`:
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
#![cfg_attr(not(test), expect(dead_code, reason = "the rows form waits on the switch (runtime.md D5); its tests use it today"))]

use core::fmt;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU64, Ordering};

use blaze_answer::{
    AnswerError, AnswerView, CellDigest, CellNo, Commit, DEFINE_BYTES, Effects, Flags, LinesKey, MAX_READS_PER_ROW, Plant, ROW_COLD_BYTES, ROW_FIXED_BYTES, RowIn, RowView, RunWriter, SegmentKind,
    SegmentRef, Serial, TaskId, TreeDigest, Write,
};
use blaze_core::Driver;
use blaze_drive::cells::Cell;
use blaze_drive::key::ABSENT;
use blaze_drive::{Key, Name};
use blaze_engine::last::{LastAnswer, LastError, LastRow, NextAnswer, NextRow, Touched};
use blaze_engine::pages::Kept;
use blaze_engine::{Answer, CellId, Phase, Planned, Stage, Tools};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
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
/// holds only the tree's digest and the tools (rule 25).
pub(crate) fn root_names<'f>(checkout: &str, tools: &Tools, tree: &TreeDigest, frame: &'f Frame) -> RootNames<'f> {
    let pages = crate::engine_answer::root_name(checkout, tools, frame);
    let mut input: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::new_in(frame);
    input.extend_from_slice(&tree.0);
    for tool in tools.0.iter() {
        input.extend_from_slice(&tool.0.0);
    }
    let id = ContentId::of(TREE_DOMAIN, &input);
    let mut tree_name = blaze_own::String::new_in(frame);
    tree_name.push_str("engine.tree.");
    for byte in &id.0[..8] {
        // A frame string's write fails only when the frame is full: the name is then short and
        // names no root, so the request answers in full (as engine_answer.rs `root_name`).
        let _ = write!(tree_name, "{byte:02x}");
    }
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
            RowsUnreadable::Answer(why) => write!(f, "{why}")?,
            RowsUnreadable::Summary { bytes } => write!(f, "its root's summary is {bytes} bytes, not the certificate's {SUMMARY_BYTES}")?,
        }
        write!(f, "; answering in full and writing them anew")
    }
}

/// The root the checkout's name names, as stored.
pub(crate) struct RowsRoot<'m> {
    pub(crate) key: Key,
    pub(crate) bytes: &'m [u8],
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
    Ok(Some(Ok(RowsRoot { key, bytes })))
}

/// The certificate from the root alone, no segment read: the root answers `tree`, nothing is
/// owed, and the last request's every line is in its lines record.
pub(crate) fn certified(root: &RowsRoot<'_>, tree: &TreeDigest) -> Result<bool, RowsUnreadable> {
    let same = blaze_answer::nothing_changed(root.bytes, tree).map_err(|why| RowsUnreadable::Answer(why.into()))?;
    let held = blaze_answer::summary_of(root.bytes).map_err(|why| RowsUnreadable::Answer(why.into()))?;
    let facts = facts_of(held).ok_or(RowsUnreadable::Summary { bytes: held.len() })?;
    Ok(same && facts.owed == 0 && facts.printed.replayable)
}

/// The answer `root` names, its segments as the store holds them (mapped where committed). A
/// store's own error stops the command; an answer that cannot be read is the stated path.
pub(crate) fn load_rows<'m: 'a, 'a, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, root: &RowsRoot<'m>, alloc: Alloc<'a>) -> Result<Result<RowsAnswer<'a>, RowsUnreadable>, CliError> {
    let mut failed: Option<CliError> = None;
    let opened = RowsAnswer::open(
        root.bytes,
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

/// A row counted as one with findings: it made findings or failed (engine.rs `apply`).
fn has_findings(findings: u32, flags: Flags) -> bool {
    findings > 0 || has(flags, Flags::FAILED)
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
    /// The answer `root` names, each segment's bytes from `fetch`: blaze-answer's `open` (each
    /// bound checked once), then the summary read as the certificate's facts.
    pub(crate) fn open<F: FnMut(&SegmentRef) -> Option<&'a [u8]>>(root: &'a [u8], fetch: F, plant: Plant, alloc: Alloc<'a>) -> Result<RowsAnswer<'a>, RowsUnreadable> {
        let view = AnswerView::open(root, fetch, plant, alloc).map_err(|why| RowsUnreadable::Answer(why.into()))?;
        let facts = facts_of(view.summary()).ok_or(RowsUnreadable::Summary { bytes: view.summary().len() })?;
        let segments = view.segments();
        let count = |kind: SegmentKind| len64(segments.iter().filter(|one| one.kind == kind).count());
        let bytes = len64(root.len()) + segments.iter().map(|one| one.len).sum::<u64>();
        let (rows_segments, cells_segments, dicts, runs) = (count(SegmentKind::Rows), count(SegmentKind::Cells), count(SegmentKind::Dict), count(SegmentKind::Run));
        Ok(RowsAnswer { view, facts, bytes, rows_segments, cells_segments, dicts, runs, counters: Counters::default() })
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

    /// The rows of every task id, each counted: the walk the questions without a column of
    /// their own make today (see the module's `NOT DONE` in the lane report).
    fn every_row(&self) -> impl Iterator<Item = RowView<'_>> + '_ {
        (0..self.view.tasks()).filter_map(move |t| self.row_view(TaskId(t)))
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

    fn saw(&self, task: TaskId, cell: CellNo) -> bool {
        let Some(row) = self.row_view(task) else { return false };
        let mut reads = self.counted(row);
        reads.any(|read| read == cell) && self.moved(cell) <= row.serial()
    }

    fn digest(&self, cell: CellNo) -> CellDigest {
        // The definer's row's define entry (blaze-answer `AnswerView::digest`): one row.
        Counters::add(&self.counters.rows, 1);
        Counters::add(&self.counters.bytes, 4 + 16 + self.rows_segments * PROBE_BYTES + ROW_BYTES + PROBE_BYTES + len64(DEFINE_BYTES));
        self.view.digest(cell)
    }

    /// The kept prefix padded with zeros (last.rs `content`): `CellDigest::of` maps it back.
    fn content(&self, cell: CellNo) -> ContentId {
        let kept = LastAnswer::digest(self, cell);
        let mut whole = [0u8; 32];
        whole[..kept.0.len()].copy_from_slice(&kept.0);
        ContentId(whole)
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

    fn file_tasks<'m>(&self, file: TaskId, alloc: Alloc<'m>) -> Result<AVec<'m, TaskId>, LastError> {
        let mut out = Vec::new_in(alloc);
        // The file's plan (engine.rs `Engine::new_in` keeps `by_file` so): a task made inline is
        // in no file's plan.
        out.extend(self.every_row().filter(|row| row.file() == file && !has(row.flags(), Flags::INLINE)).map(|row| row.task()));
        Ok(out)
    }

    fn owed<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, Planned>, LastError> {
        let mut out = Vec::new_in(alloc);
        for row in self.every_row() {
            if has(row.flags(), Flags::OWED) {
                out.push(self.planned(row.task(), row)?);
            }
        }
        Ok(out)
    }

    fn owed_cells<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (CellNo, Serial)>, LastError> {
        // A cell is owed while a reader's row is older than the cell's last move: the reader
        // has not seen it (the generation rule; engine.rs `finish` settles by the digests).
        let mut out = Vec::new_in(alloc);
        for c in 0..self.view.cells() {
            let cell = CellNo(c);
            let moved = self.moved(cell);
            if moved == Serial(0) {
                continue;
            }
            let readers = self.readers(cell, alloc)?;
            if readers.iter().any(|reader| self.row_view(*reader).is_some_and(|row| row.serial() < moved)) {
                out.push((cell, moved));
            }
        }
        Ok(out)
    }

    fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (TaskId, u32)>, LastError> {
        let mut out = Vec::new_in(alloc);
        out.extend(self.every_row().filter(|row| has_findings(row.findings(), row.flags())).map(|row| (row.task(), row.findings())));
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
}

impl<'v, 'a, 'm> RowsNext<'v, 'a, 'm> {
    /// The request after `last`, whose summary held `counts`.
    pub(crate) fn new(last: &'v AnswerView<'a>, counts: Counts, plant: Plant, alloc: Alloc<'m>) -> RowsNext<'v, 'a, 'm> {
        let commit = Commit::new(last, plant, alloc);
        let run = RunWriter::new(commit.serial(), alloc);
        RowsNext { last, commit, run, reads: Vec::new_in(alloc), defines: Vec::new_in(alloc), counts }
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
    fn recount(&mut self, old: Option<RowView<'_>>, new: Option<(u8, Flags, Key, u32)>) {
        let (was_file, was_finding) = old.map_or((false, false), |row| (is_file(row.stage(), row.flags(), row.key()), has_findings(row.findings(), row.flags())));
        let (is_file_now, finding_now) = new.map_or((false, false), |(stage, flags, key, findings)| (is_file(stage, flags, key), has_findings(findings, flags)));
        let step = |count: u32, was: bool, now: bool| match (was, now) {
            (false, true) => count.saturating_add(1),
            (true, false) => count.saturating_sub(1),
            _ => count,
        };
        self.counts.files = step(self.counts.files, was_file, is_file_now);
        self.counts.findings = step(self.counts.findings, was_finding, finding_now);
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
        let input = RowIn { key: row.key, out: row.out, file, stage, wave: row.phase.wave, flags: row.flags, source: row.source, findings: row.findings };
        self.commit.put_row(task, &input, &self.reads, &self.defines)?;
        self.run.row_reads(task, old, &self.reads);
        self.recount(old, Some((stage, row.flags, row.key, row.findings)));
        Ok(())
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
        let input = RowIn { key, out: old.map_or(ABSENT, |row| row.out()), file, stage, wave: planned.phase.wave, flags, source: planned.source, findings };
        self.commit.put_row(task, &input, &self.reads, &self.defines)?;
        // Its reads are its last row's: no reverse entry changes.
        self.recount(old, Some((stage, flags, key, findings)));
        Ok(())
    }

    fn retire(&mut self, name: Name) -> Result<(), LastError> {
        let last = self.last;
        let Some(task) = last.task(&name) else { return Ok(()) };
        let Some(old) = last.row(task) else { return Ok(()) };
        self.commit.retire(task)?;
        self.run.row_retired(task, old);
        self.recount(Some(old), None);
        Ok(())
    }

    fn finish(self, tree: &TreeDigest, summary: &[u8], lines: &LinesKey) -> Result<Effects<'m>, LastError> {
        let RowsNext { mut commit, run, .. } = self;
        if let Some(entries) = run.finish()? {
            commit.add_run(&entries)?;
        }
        Ok(commit.finish(tree, summary, lines)?)
    }
}

/// `answer` whole, as rows: the path a request takes when no rows answer is kept (the first
/// request after the switch, an unreadable answer, an old home). Every row is put; an owed task
/// with no row is owed.
pub(crate) fn put_whole<N: NextAnswer>(answer: &Answer<'_>, next: &mut N) -> Result<(), LastError> {
    for (name, row) in answer.rows.iter() {
        let owed = answer.owed_tasks.get(name);
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
        let next_row = NextRow { key: row.key, out: row.out, phase, file, source, flags: Flags(flags), findings: row.findings };
        next.put(*name, &next_row, row.reads.iter().map(|(id, _)| *id), &row.defines)?;
    }
    for (name, planned) in answer.owed_tasks.iter() {
        if !answer.rows.contains_key(name) {
            next.owe(planned)?;
        }
    }
    Ok(())
}

/// Where a commit's writes go: the command's store ([`StoreSink`]), or a test's record of them.
pub(crate) trait AnswerSink {
    type Error;
    fn put(&mut self, key: Key, bytes: &[u8]) -> Result<(), Self::Error>;
    fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), Self::Error>;
}

/// The command's store as a sink: its puts and pins, visible from its one commit after the
/// answer.
pub(crate) struct StoreSink<'s, 'c, 'm, 'r, D: Driver, S: Sharing> {
    pub(crate) cx: &'c Cx,
    pub(crate) store: &'s mut CommandStore<'m, 'r, D, S>,
}

impl<D: Driver, S: Sharing> AnswerSink for StoreSink<'_, '_, '_, '_, D, S> {
    type Error = CliError;

    fn put(&mut self, key: Key, bytes: &[u8]) -> Result<(), CliError> {
        self.store.put(self.cx, key, bytes)
    }

    fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), CliError> {
        self.store.pin(name, keys)
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

/// `effects` written to `sink` in their order (every new segment, then the root), the root
/// then pinned under the checkout's and the tree's names, with every live segment (the new and
/// `last`, the last answer's) and the lines record `lines` kept under their `.segments` names.
pub(crate) fn write_effects<K: AnswerSink>(sink: &mut K, names: &RootNames<'_>, last: &[SegmentRef], effects: &Effects<'_>, lines: Key, alloc: Alloc<'_>) -> Result<RowsSaved, K::Error> {
    let root = root_key(effects.root());
    let mut kept: AVec<'_, Key> = Vec::with_capacity_in(effects.writes().len() + last.len() + 1, alloc);
    for write in effects.writes() {
        if let Write::Segment { at, .. } = write {
            kept.push(Key(at.id));
        }
    }
    kept.extend(last.iter().map(|one| Key(one.id)));
    let mut saved = RowsSaved { segments: len32(kept.len()), ..RowsSaved::default() };
    kept.push(lines);
    for write in effects.writes() {
        match write {
            Write::Segment { at, bytes } => {
                sink.put(Key(at.id), &bytes[..])?;
                saved.puts += 1;
                saved.new_segments += 1;
                saved.new_bytes += at.len;
            }
            Write::Root { bytes } => {
                sink.put(root, &bytes[..])?;
                saved.puts += 1;
                sink.pin(names.checkout.as_str(), &[root])?;
                sink.pin(names.checkout_segments.as_str(), &kept)?;
                sink.pin(names.tree.as_str(), &[root])?;
                sink.pin(names.tree_segments.as_str(), &kept)?;
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
    /// walk (a binary search here missed reads out of cell order).
    fn saw(&self, task: Name, cell: CellId) -> bool {
        let answer = &self.kept.answer;
        let now = answer.cell(&cell);
        answer.rows.get(&task).is_some_and(|row| row.reads.iter().any(|(read, seen)| *read == cell && *seen == now))
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
