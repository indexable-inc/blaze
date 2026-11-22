//! A task's context and the phase's columns.
//!
//! The phase's columns are one [`Lane`] per worker, in the phase's arena: a worker appends to its
//! own lane only, so workers never share a cache line or a lock, and a task's reads, puts, bytes,
//! findings and spans are each ONE range of its lane's column ([`TaskRanges`]): never a list per
//! task. A [`TaskCx`] is made by the thin driver when a task starts and consumed by
//! [`TaskCx::finish`] when it returns: the finish sorts and deduplicates the task's reads in place
//! and hands the ranges to the driver's [`crate::machine::Event::TaskDone`].
//!
//! Every append is checked against its column's cap first; a full column halts the task
//! ([`Halt::Full`]) and is remembered by the context, so a stage that drops the refusal still
//! fails: the cause cannot be lost.

use core::sync::atomic::{AtomicBool, Ordering};

use blaze_id::ItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_int::{idx, len32};
use blaze_own::{AVec, Alloc, Vec};

use crate::proto::{Code, Column, Halt, Span, Stopped, TaskRef, WorkerRef};

/// The hash domain of a record's digest.
const RECORD_DOMAIN: &str = "blaze-stage record v1";

/// The clock the driver hands a task's context: injected, never read from the system (rule 75).
pub trait Clock: Sync {
    /// Nanoseconds on the request's clock.
    fn now(&self) -> u64;
}

/// What kind of record a put is: the stage's own numbering (the store's table).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordKind(pub u16);

/// One record a task put: its key and its digest, and where its bytes are in the lane. The
/// coordinator holds this row (rule 51), and only to the commit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PutRow {
    pub kind: RecordKind,
    pub key: ContentId,
    pub digest: ContentId,
    pub bytes: Span,
}

/// Where a put's row is: its lane and its row in the lane's puts column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PutRef {
    pub lane: WorkerRef,
    pub row: u32,
}

/// An item-relative place (rule 71): the item, and a byte range of its own text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Place {
    pub item: ItemKey,
    pub at: Span,
}

/// One typed argument of a finding or a refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arg {
    Count(u64),
    Item(ItemKey),
    Record(ContentId),
    Place(Place),
}

/// One finding or refusal a task said: a code, its place and its arguments' range in the args
/// column. Rendered to words only when printed (rule 81).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SayRow {
    pub code: Code,
    pub place: Place,
    pub args: Span,
}

/// One span a task opened (rule 15).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpanRow {
    pub name: &'static str,
    pub start: u64,
    pub end: u64,
}

/// An open span: closed by [`TaskCx::close`], or by [`TaskCx::finish`] at the task's end.
#[must_use = "a span is closed by `TaskCx::close`; one left open ends with the task"]
pub struct Open {
    row: u32,
}

/// The caps of one lane's columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caps {
    pub reads: u32,
    pub puts: u32,
    pub bytes: u32,
    pub says: u32,
    pub args: u32,
    pub spans: u32,
}

/// One worker's lane of the phase's columns, in the phase's arena.
pub struct Lane<'p, R> {
    reads: AVec<'p, R>,
    puts: AVec<'p, PutRow>,
    bytes: AVec<'p, u8>,
    says: AVec<'p, SayRow>,
    args: AVec<'p, Arg>,
    spans: AVec<'p, SpanRow>,
    caps: Caps,
}

impl<'p, R: Copy + Ord> Lane<'p, R> {
    pub fn new_in(caps: Caps, alloc: Alloc<'p>) -> Lane<'p, R> {
        Lane {
            reads: Vec::new_in(alloc),
            puts: Vec::new_in(alloc),
            bytes: Vec::new_in(alloc),
            says: Vec::new_in(alloc),
            args: Vec::new_in(alloc),
            spans: Vec::new_in(alloc),
            caps,
        }
    }

    pub fn reads(&self, span: Span) -> &[R] {
        &self.reads[span.range()]
    }

    pub fn puts(&self, span: Span) -> &[PutRow] {
        &self.puts[span.range()]
    }

    pub fn bytes(&self, row: &PutRow) -> &[u8] {
        &self.bytes[row.bytes.range()]
    }

    pub fn says(&self, span: Span) -> &[SayRow] {
        &self.says[span.range()]
    }

    pub fn args(&self, row: &SayRow) -> &[Arg] {
        &self.args[row.args.range()]
    }

    pub fn spans(&self, span: Span) -> &[SpanRow] {
        &self.spans[span.range()]
    }

    /// The rows of every column: what the phase's arena holds for this lane.
    pub fn rows(&self) -> usize {
        self.reads.len() + self.puts.len() + self.bytes.len() + self.says.len() + self.args.len() + self.spans.len()
    }
}

/// A column's length as a row number. Every column is capped below `u32::MAX` before it grows,
/// so this never saturates in a lane made by [`Lane::new_in`].
fn at(len: usize) -> u32 {
    len32(len).unwrap_or(u32::MAX)
}

/// Whether `len` more rows fit a column of `now` rows under `cap`.
fn fits(now: usize, more: usize, cap: u32) -> bool {
    now.checked_add(more).is_some_and(|end| end <= idx(cap))
}

/// Where a task's ranges begin in its lane.
#[derive(Clone, Copy)]
struct Marks {
    reads: u32,
    puts: u32,
    says: u32,
    spans: u32,
}

/// What a finished task's context hands the driver: its ranges in its lane, whether it made a
/// read no key covers, and the halt its context gave it, if any.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TaskRanges {
    pub lane: WorkerRef,
    pub reads: Span,
    pub puts: Span,
    pub says: Span,
    pub spans: Span,
    /// Reads no key covers: more than zero makes a `Made` an `Unkept` (the driver's rule).
    pub uncovered: u32,
    /// The halt the context gave the task: a stage that dropped it still fails.
    pub halted: Option<Halt>,
}

/// A task's context: its scratch allocator (the worker's frame, reset by the driver when the task
/// returns), its lane of the phase's columns, the stop flag and the clock.
pub struct TaskCx<'t, 'p, R> {
    lane: &'t mut Lane<'p, R>,
    scratch: Alloc<'t>,
    stop: &'t AtomicBool,
    clock: &'t dyn Clock,
    task: TaskRef,
    worker: WorkerRef,
    marks: Marks,
    uncovered: u32,
    halted: Option<Halt>,
}

impl<'t, 'p, R: Copy + Ord> TaskCx<'t, 'p, R> {
    /// The context of `task` on `worker`: `lane` is that worker's lane, `scratch` its frame.
    pub fn new(
        task: TaskRef,
        worker: WorkerRef,
        lane: &'t mut Lane<'p, R>,
        scratch: Alloc<'t>,
        stop: &'t AtomicBool,
        clock: &'t dyn Clock,
    ) -> TaskCx<'t, 'p, R> {
        let marks = Marks { reads: at(lane.reads.len()), puts: at(lane.puts.len()), says: at(lane.says.len()), spans: at(lane.spans.len()) };
        TaskCx { lane, scratch, stop, clock, task, worker, marks, uncovered: 0, halted: None }
    }

    pub fn task(&self) -> TaskRef {
        self.task
    }

    /// The task's scratch allocator: the worker's frame, reset when the task returns. Nothing
    /// made in it can be handed back ([`crate::proto::TaskOut`] is `'static`).
    pub fn scratch(&self) -> Alloc<'t> {
        self.scratch
    }

    /// `Err` once the command is asked to stop, or once a column refused this task: every loop
    /// inside a task asks it (`task.stopped()?`).
    pub fn stopped(&mut self) -> Result<(), Stopped> {
        if let Some(why) = self.halted {
            return Err(Stopped { why });
        }
        if self.stop.load(Ordering::Acquire) {
            self.halted = Some(Halt::Asked);
            return Err(Stopped { why: Halt::Asked });
        }
        Ok(())
    }

    fn full(&mut self, column: Column, cap: u32) -> Stopped {
        let why = Halt::Full { column, cap };
        self.halted = Some(why);
        Stopped { why }
    }

    /// Records a read: one row of the phase's reads column.
    pub fn read(&mut self, cell: R) -> Result<(), Stopped> {
        self.stopped()?;
        if !fits(self.lane.reads.len(), 1, self.lane.caps.reads) {
            return Err(self.full(Column::Reads, self.lane.caps.reads));
        }
        self.lane.reads.push(cell);
        Ok(())
    }

    /// Records a read no key covers (a fact outside every cell: a checkout's state, a host
    /// probe): the task's result is correct for this command and is not kept.
    pub fn read_uncovered(&mut self) -> Result<(), Stopped> {
        self.stopped()?;
        self.uncovered = self.uncovered.saturating_add(1);
        Ok(())
    }

    /// Writes one record once into the lane: its bytes, and its (kind, key, digest) row.
    pub fn put(&mut self, kind: RecordKind, key: ContentId, bytes: &[u8]) -> Result<PutRef, Stopped> {
        self.stopped()?;
        let caps = self.lane.caps;
        if !fits(self.lane.puts.len(), 1, caps.puts) {
            return Err(self.full(Column::Puts, caps.puts));
        }
        if !fits(self.lane.bytes.len(), bytes.len(), caps.bytes) {
            return Err(self.full(Column::Bytes, caps.bytes));
        }
        let from = at(self.lane.bytes.len());
        self.lane.bytes.extend_from_slice(bytes);
        let digest = Hasher::new(RECORD_DOMAIN).part(&kind.0.to_le_bytes()).part(bytes).finish();
        let row = at(self.lane.puts.len());
        self.lane.puts.push(PutRow { kind, key, digest, bytes: Span::between(from, at(self.lane.bytes.len())) });
        Ok(PutRef { lane: self.worker, row })
    }

    /// Says a finding or a refusal: a code, an item-relative place, typed arguments.
    pub fn say(&mut self, code: Code, place: Place, args: &[Arg]) -> Result<(), Stopped> {
        self.stopped()?;
        let caps = self.lane.caps;
        if !fits(self.lane.says.len(), 1, caps.says) {
            return Err(self.full(Column::Says, caps.says));
        }
        if !fits(self.lane.args.len(), args.len(), caps.args) {
            return Err(self.full(Column::Args, caps.args));
        }
        let from = at(self.lane.args.len());
        self.lane.args.extend_from_slice(args);
        self.lane.says.push(SayRow { code, place, args: Span::between(from, at(self.lane.args.len())) });
        Ok(())
    }

    /// Opens a span named `name` (rule 15): a stretch of the task's time in the profile.
    pub fn open(&mut self, name: &'static str) -> Result<Open, Stopped> {
        self.stopped()?;
        if !fits(self.lane.spans.len(), 1, self.lane.caps.spans) {
            return Err(self.full(Column::Spans, self.lane.caps.spans));
        }
        let row = at(self.lane.spans.len());
        let now = self.clock.now();
        self.lane.spans.push(SpanRow { name, start: now, end: now });
        Ok(Open { row })
    }

    /// Closes a span this task opened.
    pub fn close(&mut self, open: Open) {
        let now = self.clock.now();
        if let Some(row) = self.lane.spans.get_mut(idx(open.row)) {
            row.end = now;
        }
    }

    /// The task's ranges, for the driver: its reads sorted and deduplicated in place, every span
    /// it left open closed now.
    pub fn finish(self) -> TaskRanges {
        let TaskCx { lane, clock, worker, marks, uncovered, halted, .. } = self;
        let from = idx(marks.reads);
        lane.reads[from..].sort_unstable();
        lane.reads.dedup_range(from..);
        let now = clock.now();
        for row in &mut lane.spans[idx(marks.spans)..] {
            if row.end == row.start {
                row.end = now;
            }
        }
        TaskRanges {
            lane: worker,
            reads: Span::between(marks.reads, at(lane.reads.len())),
            puts: Span::between(marks.puts, at(lane.puts.len())),
            says: Span::between(marks.says, at(lane.says.len())),
            spans: Span::between(marks.spans, at(lane.spans.len())),
            uncovered,
            halted,
        }
    }
}
