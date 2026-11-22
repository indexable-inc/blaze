//! `finish` and `readers_of` at tempo's scale (plan row B1): 20,000 body rows of 300 reads, 2,000
//! owed cells each read by 2,000 of them. On tempo `finish` ran past 14 minutes: it asked every
//! reader of every owed cell whether its row saw the cell, each a walk of the reader's read list.
//!
//! What is held here, by counts and never by time (rule 61: a test asserts what it can count):
//! - every `saw` is a binary search: the reads compared are at most `floor(log2 reads) + 1` a
//!   question, and `Plant::LinearSaw` (the walk restored) breaks that bound;
//! - `finish` asks no reader: the `saw` questions are exactly the visits' (each reader of each
//!   owed cell, once, when the request opens) and the taken rows' stale reads of counted cells
//!   (overlay.rs "Stale counts");
//! - the cells settled are exactly those every reader of which ran, against an oracle that asks
//!   the last answer directly, and `Plant::StaleKeptOnTake` (a taken row's stale reads left in
//!   the counts) is caught by it.
//!
//! The last answer is a form of its own ([`ScaleLast`]): the reads as `u16` handles, 2 B a read
//! in read order and 2 B sorted (24 MB at this scale; the in-memory form's 84 B a read would hold
//! half a gigabyte). It keeps no seen digests: its rows stand by generation, as the rows form's.
//!
//! The shape: reader `r` reads, in read order, `QUIET` cells `(r + 37 k) mod quiet` first, then
//! the owed cells of its class (`r mod classes`) from the highest down, so a walk to an owed
//! cell passes about two thirds of the list. Owed cells moved at serial 2, quiet ones at 1, every
//! reader's row is of serial 1: each reader is stale for each owed cell it reads. The scope runs
//! class 0, so its owed cells, and only those, have every reader made again and settle.

use std::format;
use std::println;
use std::vec::Vec as StdVec;

use blaze_answer::{CellDigest, Flags, Serial, TreeDigest};
use blaze_drive::cells::Cell;
use blaze_drive::key::{self, Key, Kind, Name, Tool};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_sim::Broke;

use crate::engine::{Buffer, CellId, Done, Edit, Engine, EngineError, Finished, Phase, Plant, Planned, SVec, Scope, Stage, Task, Tools, View, drive};
use crate::last::{Found, LastAnswer, LastError, LastRow, ReadOf, Touched, search};
use crate::overlay::{Fold, Work};
use crate::sim::{Pools, alloc, oracle};

/// The cell every definer defines.
const HELD: Cell = Cell([9; 16]);
/// The file every reader is listed in: no task of the answer, so no plan is ever asked for it.
const FILE: Name = Name::new(Kind::FileItems, ContentId([90; 32]));

/// The answer's shape.
#[derive(Clone, Copy, Debug)]
struct Shape {
    rows: u32,
    classes: u32,
    owed_cells: u32,
    quiet_cells: u32,
    quiet_reads: u32,
}

impl Shape {
    /// The brief's: 20,000 rows of 300 reads, 2,000 owed cells each read by 2,000 rows.
    const TEMPO: Shape = Shape { rows: 20_000, classes: 10, owed_cells: 2_000, quiet_cells: 1_000, quiet_reads: 100 };
    /// The same proportions, small: for the control of the counts, which needs no scale.
    const SMALL: Shape = Shape { rows: 200, classes: 10, owed_cells: 20, quiet_cells: 100, quiet_reads: 10 };

    fn owed_reads(&self) -> u32 {
        self.owed_cells / self.classes
    }

    fn reads(&self) -> u32 {
        self.owed_reads() + self.quiet_reads
    }

    fn cells(&self) -> u32 {
        self.owed_cells + self.quiet_cells
    }

    fn readers_of_owed(&self) -> u32 {
        self.rows / self.classes
    }

    /// A binary search's most probes over one row's reads: `floor(log2 reads) + 1`.
    fn probes(&self) -> u64 {
        u64::from(u32::BITS - self.reads().leading_zeros())
    }
}

fn subject(tag: u8, i: u32) -> ContentId {
    let mut bytes = [tag; 32];
    bytes[1..5].copy_from_slice(&i.to_le_bytes());
    ContentId(bytes)
}

/// The handle a name was made from (its subject's bytes 1 to 4).
fn handle_of(name: Name) -> u32 {
    u32::from_le_bytes([name.subject.0[1], name.subject.0[2], name.subject.0[3], name.subject.0[4]])
}

fn tools() -> Tools {
    Tools([Tool(ContentId([91; 32])); key::KINDS])
}

/// What a cell holds: one digest a cell, never moving inside the test.
fn content_of(cell: u32) -> ContentId {
    subject(92, cell)
}

/// The last answer at scale. Tasks `0..rows` are the readers, `rows + c` the definer of cell
/// `c`; cells `0..owed_cells` are owed, the rest quiet.
struct ScaleLast {
    shape: Shape,
    names: StdVec<Name>,
    by_name: StdVec<(Name, u32)>,
    keys: StdVec<Key>,
    ids: StdVec<CellId>,
    by_id: StdVec<(CellId, u32)>,
    /// Each reader's reads, `shape.reads()` a row: in read order, and sorted.
    reads: StdVec<u16>,
    sorted: StdVec<u16>,
}

impl ScaleLast {
    fn new(shape: Shape) -> ScaleLast {
        assert!(shape.cells() <= u32::from(u16::MAX) && shape.quiet_reads <= shape.quiet_cells && shape.quiet_cells % 37 != 0, "the shape fits its handles");
        let tools = tools();
        let tasks = shape.rows + shape.cells();
        let names: StdVec<Name> = (0..tasks)
            .map(|t| match t < shape.rows {
                true => Name::new(Kind::BodyCheck, subject(93, t)),
                false => Name::new(Kind::Signature, subject(94, t)),
            })
            .collect();
        let mut by_name: StdVec<(Name, u32)> = names.iter().copied().zip(0u32..).collect();
        by_name.sort_unstable();
        let keys = names.iter().zip(0u32..).map(|(name, t)| tools.key(*name, &subject(95, t))).collect();
        let ids: StdVec<CellId> = (0..shape.cells()).map(|c| CellId { of: names[(shape.rows + c) as usize], cell: HELD }).collect();
        let mut by_id: StdVec<(CellId, u32)> = ids.iter().copied().zip(0u32..).collect();
        by_id.sort_unstable();
        let stride = shape.reads() as usize;
        let mut reads: StdVec<u16> = StdVec::with_capacity(shape.rows as usize * stride);
        let mut sorted: StdVec<u16> = StdVec::with_capacity(shape.rows as usize * stride);
        let mut one: StdVec<u16> = StdVec::with_capacity(stride);
        for r in 0..shape.rows {
            one.clear();
            for k in 0..shape.quiet_reads {
                one.push((shape.owed_cells + (r + 37 * k) % shape.quiet_cells) as u16);
            }
            for j in (0..shape.owed_reads()).rev() {
                one.push((r % shape.classes + shape.classes * j) as u16);
            }
            reads.extend_from_slice(&one);
            one.sort_unstable();
            assert!(one.windows(2).all(|pair| pair[0] < pair[1]), "reader {r} reads each cell once");
            sorted.extend_from_slice(&one);
        }
        ScaleLast { shape, names, by_name, keys, ids, by_id, reads, sorted }
    }

    fn is_reader(&self, task: u32) -> bool {
        task < self.shape.rows
    }

    fn is_definer(&self, task: u32) -> bool {
        task >= self.shape.rows && task < self.shape.rows + self.shape.cells()
    }

    fn row_reads(&self, r: u32) -> &[u16] {
        let stride = self.shape.reads() as usize;
        &self.reads[r as usize * stride..(r as usize + 1) * stride]
    }

    fn row_sorted(&self, r: u32) -> &[u16] {
        let stride = self.shape.reads() as usize;
        &self.sorted[r as usize * stride..(r as usize + 1) * stride]
    }

    fn source(t: u32) -> ContentId {
        subject(95, t)
    }
}

type ScaleReads<'r> = core::iter::Map<core::slice::Iter<'r, u16>, fn(&u16) -> u32>;

fn widen(cell: &u16) -> u32 {
    u32::from(*cell)
}

impl<'s> LastAnswer for &'s ScaleLast {
    type Task = u32;
    type Cell = u32;
    type Reads<'r>
        = ScaleReads<'r>
    where
        Self: 'r;
    type Defines<'r>
        = core::option::IntoIter<(u32, CellDigest)>
    where
        Self: 'r;
    type Seen<'r>
        = core::iter::Empty<(u32, ContentId)>
    where
        Self: 'r;

    fn serial(&self) -> Serial {
        Serial(2)
    }

    fn certificate(&self, _tree: &TreeDigest) -> bool {
        false
    }

    fn task(&self, name: &Name) -> Option<u32> {
        self.by_name.binary_search_by(|(one, _)| one.cmp(name)).ok().map(|at| self.by_name[at].1)
    }

    fn name(&self, task: u32) -> Option<Name> {
        self.names.get(task as usize).copied()
    }

    fn cell(&self, id: &CellId) -> Option<u32> {
        self.by_id.binary_search_by(|(one, _)| one.cmp(id)).ok().map(|at| self.by_id[at].1)
    }

    fn cell_id(&self, cell: u32) -> Option<CellId> {
        self.ids.get(cell as usize).copied()
    }

    fn row(&self, task: u32) -> Result<Option<LastRow>, LastError> {
        let reader = self.is_reader(task);
        if !reader && !self.is_definer(task) {
            return Ok(None);
        }
        Ok(Some(LastRow {
            key: self.keys[task as usize],
            out: subject(96, task),
            serial: Serial(if reader { 1 } else { 2 }),
            phase: Phase::new(if reader { Stage::Bodies } else { Stage::Heads }, 0),
            file: FILE,
            source: ScaleLast::source(task),
            flags: Flags(0),
            findings: 0,
            reads: if reader { self.shape.reads() } else { 0 },
            defines: u32::from(!reader),
        }))
    }

    fn reads(&self, task: u32) -> Option<ScaleReads<'_>> {
        let widened: fn(&u16) -> u32 = widen;
        match (self.is_reader(task), self.is_definer(task)) {
            (true, _) => Some(self.row_reads(task).iter().map(widened)),
            (false, true) => Some(self.reads[..0].iter().map(widened)),
            (false, false) => None,
        }
    }

    fn defines(&self, task: u32) -> Option<core::option::IntoIter<(u32, CellDigest)>> {
        match (self.is_reader(task), self.is_definer(task)) {
            (true, _) => Some(None.into_iter()),
            (false, true) => {
                let cell = task - self.shape.rows;
                Some(Some((cell, LastAnswer::digest(self, cell))).into_iter())
            }
            (false, false) => None,
        }
    }

    fn stands(&self, task: u32) -> Option<bool> {
        let serial = self.row(task).ok()??.serial;
        Some(self.reads(task)?.all(|cell| LastAnswer::moved(self, cell) <= serial))
    }

    /// A binary search over the row's sorted reads, its probes counted.
    fn read_of(&self, task: u32, cell: u32) -> Result<ReadOf, LastError> {
        if !self.is_reader(task) {
            return Ok(ReadOf { found: Found::NotRead, compared: 0 });
        }
        let sorted = self.row_sorted(task);
        let (at, compared) = search(sorted.len(), |i| u32::from(sorted[i]).cmp(&cell));
        Ok(ReadOf { found: at.map_or(Found::NotRead, |_| Found::ReadAt(Serial(1))), compared })
    }

    fn digest(&self, cell: u32) -> CellDigest {
        CellDigest::of(&content_of(cell))
    }

    fn content(&self, cell: u32) -> ContentId {
        content_of(cell)
    }

    fn moved(&self, cell: u32) -> Serial {
        Serial(if cell < self.shape.owed_cells { 2 } else { 1 })
    }

    fn seen(&self, _task: u32) -> Option<core::iter::Empty<(u32, ContentId)>> {
        None
    }

    fn readers<'m>(&self, cell: u32, alloc: Alloc<'m>) -> Result<AVec<'m, u32>, LastError> {
        let mut out = Vec::new_in(alloc);
        match cell < self.shape.owed_cells {
            true => out.extend((0..self.shape.readers_of_owed()).map(|i| cell % self.shape.classes + self.shape.classes * i)),
            false => out.extend((0..self.shape.rows).filter(|r| self.row_sorted(*r).binary_search(&(cell as u16)).is_ok())),
        }
        Ok(out)
    }

    fn file_tasks<'m>(&self, _file: u32, alloc: Alloc<'m>) -> Result<AVec<'m, u32>, LastError> {
        Ok(Vec::new_in(alloc))
    }

    fn owed<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, Planned>, LastError> {
        Ok(Vec::new_in(alloc))
    }

    fn owed_cells<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (u32, Serial)>, LastError> {
        let mut out = Vec::new_in(alloc);
        out.extend((0..self.shape.owed_cells).map(|cell| (cell, Serial(2))));
        Ok(out)
    }

    fn with_findings<'m>(&self, alloc: Alloc<'m>) -> Result<AVec<'m, (u32, u32)>, LastError> {
        Ok(Vec::new_in(alloc))
    }

    fn touched(&self) -> Touched {
        Touched::default()
    }
}

/// The request's writes are the answer: the oracle reads the owed cells from them.
impl<'s> Fold<'static> for &'s ScaleLast {
    type Out = Work<'static>;

    fn fold(self, work: Work<'static>) -> Result<Work<'static>, EngineError> {
        Ok(work)
    }
}

/// Runs the readers of class 0 alone.
struct ClassZero {
    rows: u32,
    classes: u32,
}

impl ClassZero {
    fn runs_reader(&self, r: u32) -> bool {
        r < self.rows && r % self.classes == 0
    }
}

impl Scope for ClassZero {
    fn runs(&self, task: &Planned) -> bool {
        task.name.kind == Kind::BodyCheck && self.runs_reader(handle_of(task.name))
    }
}

/// A reader made again: the same reads, each seen as it is today, defining nothing, the same
/// out as its last row (equal bytes: early cutoff, nothing past it moves).
fn scale_work(last: &ScaleLast, view: View<'_, 'static, &ScaleLast>, task: &Task, buffer: &mut Buffer<'static>) {
    let a = alloc();
    let r = handle_of(task.planned.name);
    let mut reads: SVec<'static, (CellId, ContentId)> = Vec::with_capacity_in(last.shape.reads() as usize, a);
    for cell in last.row_reads(r).iter() {
        let id = last.ids[usize::from(*cell)];
        reads.push((id, view.cell(&id)));
    }
    buffer.done.push(Done { name: task.planned.name, key: task.key, out: subject(96, r), defines: Vec::new_in(a), reads, findings: 0, failed: false });
}

fn scale_request(last: &ScaleLast, plant: Plant) -> Result<Finished<'static, Work<'static>>, EngineError> {
    let scope = ClassZero { rows: last.shape.rows, classes: last.shape.classes };
    let mut engine = Engine::new_in(tools(), last, &Edit { files: &[] }, scope, plant, alloc())?;
    let mut buffers: StdVec<Buffer<'static>> = StdVec::from([Buffer::new_in(alloc())]);
    drive(&mut engine, &mut Pools::One, &mut buffers, &|_, view, task, buffer| scale_work(last, view, task, buffer))?;
    engine.finish()
}

/// The oracle: each owed cell settles iff the scope ran every reader of it, asked of the last
/// answer directly; the report's counts are the visits' and the taken rows', no more.
fn judge(last: &ScaleLast, finished: &Finished<'static, Work<'static>>, bounded: bool) -> Result<(), Broke> {
    let shape = last.shape;
    let scope = ClassZero { rows: shape.rows, classes: shape.classes };
    let mut want_settled = 0u32;
    for cell in 0..shape.owed_cells {
        let readers = LastAnswer::readers(&last, cell, Alloc::from(alloc())).map_err(|error| Broke::Harness(format!("readers of {cell}: {error}")))?;
        let every = readers.iter().all(|r| scope.runs_reader(*r));
        want_settled += u32::from(every);
        let owed = finished.answer.owed_cells.contains_key(&last.ids[cell as usize]);
        oracle(owed != every, || format!("owed cell {cell}: owed at the end {owed}, every reader ran {every}"))?;
    }
    let settle = finished.report.settle;
    oracle(settle.owed == shape.owed_cells && settle.settled == want_settled, || format!("settled {} of {} owed cells, the oracle {want_settled} of {}", settle.settled, settle.owed, shape.owed_cells))?;
    let ran = u64::from(shape.rows / shape.classes);
    let visits = u64::from(shape.owed_cells) * u64::from(shape.readers_of_owed());
    let taken = ran * u64::from(shape.owed_reads());
    oracle(settle.saw_calls == visits + taken, || format!("{} saw calls: the visits ask {visits} and the taken rows {taken}, and finish none", settle.saw_calls))?;
    if bounded {
        let bound = settle.saw_calls * shape.probes();
        oracle(settle.compared <= bound, || format!("{} reads compared for {} saw calls: past {bound}, {} a question (a binary search over {} reads)", settle.compared, settle.saw_calls, shape.probes(), shape.reads()))?;
    }
    oracle(finished.report.index.writes == 0 && finished.answer.readers.is_empty(), || {
        format!("refreshing seen values changed no membership but issued {} writes and folded {} edits", finished.report.index.writes, finished.answer.readers.len())
    })?;
    Ok(())
}

fn run(shape: Shape, plant: Plant, bounded: bool) -> Result<(), Broke> {
    let last = ScaleLast::new(shape);
    let finished = scale_request(&last, plant).map_err(|error| Broke::Harness(format!("the request failed: {error}")))?;
    let settle = finished.report.settle;
    println!(
        "{shape:?} {plant:?}: owed cells {}, settled {}, saw calls {}, reads compared {} ({} a call, rounded down), entered {}",
        settle.owed,
        settle.settled,
        settle.saw_calls,
        settle.compared,
        settle.compared / settle.saw_calls.max(1),
        finished.report.entered
    );
    judge(&last, &finished, bounded)
}

/// Expected: "... saw calls 4400000, reads compared C (... a call)" with C at most 9 a call,
/// and the cells of class 0 (200) settled.
#[test]
fn finish_and_readers_of_search_at_scale() {
    if let Err(broke) = run(Shape::TEMPO, Plant::None, true) {
        panic!("{broke:?}");
    }
}

/// Expected: the walk restored compares about 200 reads a question, past the bound of 9.
#[test]
fn control_a_linear_saw_breaks_the_bound() {
    match run(Shape::TEMPO, Plant::LinearSaw, true) {
        Err(Broke::Oracle(why)) if why.contains("reads compared") => println!("plant LinearSaw caught: {why}"),
        other => panic!("the linear walk was not caught by the reads-compared bound: {other:?}"),
    }
}

/// Expected: the counts hold on the small shape, and a taken row's stale reads left in them keep
/// class 0's owed cells owed, which the oracle names.
#[test]
fn control_stale_kept_on_take_is_caught() {
    if let Err(broke) = run(Shape::SMALL, Plant::None, true) {
        panic!("the clean small run broke: {broke:?}");
    }
    match run(Shape::SMALL, Plant::StaleKeptOnTake, true) {
        Err(Broke::Oracle(why)) => println!("plant StaleKeptOnTake caught: {why}"),
        other => panic!("a taken row's stale reads kept in the counts were not caught: {other:?}"),
    }
}
