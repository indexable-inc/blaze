//! The `Bodies` stage of `blaze engine-check` (engine.md E5/E6, "The design" 4): every body of
//! the workspace's files type-checked and borrow-checked as ITS OWN engine task, entered once, on
//! the pool, by lane VW's `check_body` (front2 tables/task.rs) over the read-only view.
//!
//! THE PLAN, from text: one `BodyCheck` task per body part of each file's outline (work.rs
//! `outline_file`: one part per entry with a body of its own, and per entry a macro takes), its
//! name and source the outline's (`body_name`, `body_source`: the item's text), so the task's
//! key is known before any phase runs ([`plan`]).
//!
//! THE PHASE, on the coordinator first (a constant per member crate, never a step per body):
//! the crate list (the `UnitGraph` record), its trees, and for each member crate its check scope
//! (blaze-world check_scope.rs: the one rule of which crates a body sees), each scope crate's
//! names and items records read from the store as the Names stage put them, and the member's
//! files (its items record's file rows) matched to the planner's outlines. Each body's name is
//! then matched to its item key through the records' listing (front2 project_records.rs
//! `list_with`, `index_by_chain`): the item key is what `check_body` finds a body by.
//!
//! THE RUNS: the bodies of one member cut into runs of at most [`RUN`], each run a task of one
//! bstd scope; a run opens its own `Records` over the shared bytes (`Records` opens lazily
//! through `RefCell`s: never shared between threads, view.rs), and every body of the run is
//! checked over one `Tables` with its own recorder (task.rs). Nothing mutable is shared between
//! bodies. A panic fails its body alone (rule 20).
//!
//! WHAT A BODY READS, as the engine keys its readers: the view records names, lang, head and
//! impl-bucket cells. A crate's `ImplBucket` cell is read as read: the Heads stage defines it
//! (engine_heads.rs), or states its absence (no impl of the crate in that bucket). Until the
//! Names stage defines cells finer than its record's one whole cell (E4's wiring), the other
//! reads are WIDENED to a cell some phase defines ([`widen`]): a `CrateScope` cell to that
//! record's whole cell, the whole graph's bucket fact (made from the names records) to every
//! scope crate's. A `Signature` read of a member's head is the cell of the head task of the
//! entry that covers it (engine_heads.rs `Owners::place`; the body still makes the heads it
//! needs in its own store, task.rs), so a signature edit enters exactly the bodies whose head
//! read moved; a dependency's head, or one a macro wrote, is its crate's names record whole.
//! Widened reads
//! are a superset of the true ones: an edit can enter a body that did not need it, never miss
//! one. So `Make` made nothing inline here: every cell a body reads is the engine's.

use core::convert::Infallible;
use core::num::NonZeroU32;

use blaze_drive::cells::Cell;
use blaze_drive::key::ABSENT;
use blaze_drive::{Input, Kind, Locate, Name};
use blaze_engine::{Buffer, CellId, Done, Made, Phase, Planned, SVec, Stage, Task, View};
use blaze_id::ItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::front2::buckets::{ImplShelf, ImplShelfOpts, TableSet};
use blaze_ladder::front2::heads::NeedsMet;
use blaze_ladder::front2::project::BodyRef;
use blaze_ladder::front2::project_records::{BodySource, Listing, index_by_chain, list_with};
use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::front2::tables::cost::{Cost, Gauge};
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{CellFailure, ConstValues, FreezeError, NotTheirs, SeededHeads, SeededHeadsOpts, cell_id as head_cell_id};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::seeds::{HeadSeed, RequestSeed, RequestSeedOpts, SeedError};
use blaze_ladder::front2::tables::task::{BodyFrame, Make, MakeRefused, Said as Line, SaidWhat, TaskError, WorkBytes, check_body_split};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::{SharedAlloc, Vec};
use blaze_scope::crate_env::EnvLent;
use blaze_scope::items_record::ItemsRecord;
use blaze_world::crate_list::CrateGraph;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;
use bstd::task::Class;
use std::alloc::Global;

use crate::CliError;
use crate::body_types::Kept;
use crate::engine_body_plan::{BodyClaim, BodyIs, FileClaim, FileIs};
use crate::engine_check::EngineCheckError;
use crate::engine_head_notes::{Cause, RootKind, Step, leak, root_cause};
use crate::engine_memory::InStretch;
use crate::engine_names::NamesStage;
use crate::engine_records::EngineRecords;
use crate::engine_tasks::{CELLS, Outlined, WHOLE};
use crate::fetch::FetchEnv;
use crate::run_names::Crate;
use crate::work;

/// Where the rule that refuses a body on this path is.
const RULE: &str = "engine_bodies.rs";

/// The most bodies one run checks: a run opens its own records once, and its frame holds every
/// body's lowering until the run ends (a body's frame is the records' lifetime in `check_body`,
/// so it cannot be reset per body yet: see the report's brief), so runs are short.
const RUN: usize = 32;

/// A run's frame: its records' decoded parts and its bodies' lowerings (reserved, not touched).
const BODIES_FRAME: usize = 1 << 30;

/// The domain of the out of a body no crate of the graph holds.
const OWNERLESS_DOMAIN: &str = "blaze-cli engine bodies ownerless 1";

/// The domain of the verdict of an entry whose expansion wrote several bodies (or none).
const ENTRY_DOMAIN: &str = "blaze-cli engine bodies expansion entry 1";

/// The most bodies a macro wrote that no entry covers named by label in the request's note
/// (the rest counted in it): a cap on the note, never on what is counted (rule 18).
const MOST_UNCOVERED_NAMED: usize = 16;

/// The lines block of the note of bodies a macro wrote that no entry covers: no task's (its
/// domain is no task name's), stable across requests.
const UNCOVERED_NOTE: &[u8] = b"{bodies a macro wrote that no entry covers}";

/// The label of the root's `fn main` that D104's launcher replaced (body_types/covered.rs
/// `Root::label`): its findings are at the entry's start, said so (`WRITTEN_UNPLACED`).
const REPLACED_MAIN: &str = "{the expansion that took `main`}";


/// One `BodyCheck` task a body part of each outlined file, sorted by name (one per name).
pub(crate) fn plan<'h>(outlined: &[Outlined<'_>], alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
    let mut planned: SVec<'h, Planned> = SVec::new_in(alloc);
    for one in outlined.iter() {
        let Ok(record) = &one.made else { continue };
        for part in record.parts.iter() {
            if let (Kind::BodyCheck, Locate::Source(source)) = (part.name.kind, part.locate) {
                let source = crate::engine_tasks::edition_source(source, one.edition);
                planned.push(Planned { name: part.name, phase: Phase::new(Stage::Bodies, 0), source, file: one.name });
            }
        }
    }
    planned.sort_unstable_by_key(|one| one.name);
    planned.dedup_by_key(|one| one.name);
    planned
}

/// Why a body gave no verdict: each named, printed as a note (rule 3: never a quiet pass).
#[derive(Debug)]
pub(crate) enum Said<E> {
    /// `check_body` refused the task (a fault of the plan or the view, task.rs `TaskError`).
    Task(E),
    /// The body's check panicked: this body alone failed (rule 20).
    Panicked,
    /// The body's check panicked with its stretch's frame past half its reserve: the frame was
    /// used up ([`BODIES_FRAME`]); this body alone failed, and the next body gets a new frame.
    FrameUsedUp { used: u64, reserved: u64 },
    /// This stage could not give the body a check, and why.
    Refused(&'static str),
    /// The checker gave a verdict that refused the body or left a gap in it (task.rs `Done`
    /// with `failed`): the body was not checked whole. Its verdict's words why (task.rs
    /// `BodyOut::said`'s `NotChecked` and `BorrowsNotChecked` lines, said.rs's words), each
    /// line "<cause>: <words>", joined by "; " ("" when the verdict said none).
    Unchecked(&'static str),
    /// Types checked whole; the move and borrow check refused: its verdict's words why, as
    /// [`Said::Unchecked`]'s (a [`crate::engine_lines::Class::Borrows`] note, never a failure).
    BorrowsUnchecked(&'static str),
}

/// Allocated in the request frame only when the body has a note. A task error has already
/// been rendered here, so none of its owned data needs a destructor in the frame.
struct BodyNote<'f> {
    said: Said<&'f str>,
    head_failed: Option<crate::engine_seeds::FirstFailure>,
}

impl<E> Said<E> {
    fn map_task<T>(self, map: impl FnOnce(E) -> T) -> Said<T> {
        match self {
            Said::Task(error) => Said::Task(map(error)),
            Said::Panicked => Said::Panicked,
            Said::FrameUsedUp { used, reserved } => Said::FrameUsedUp { used, reserved },
            Said::Refused(why) => Said::Refused(why),
            Said::Unchecked(why) => Said::Unchecked(why),
            Said::BorrowsUnchecked(why) => Said::BorrowsUnchecked(why),
        }
    }
}

/// One body as the phase took it: what the printer and the profile read.
pub(crate) struct Ran<'f> {
    pub(crate) name: Name,
    /// Its item's name as written ("" when it was not found).
    pub(crate) label: &'f str,
    /// Its check's time on its worker, ns (0 when it was not checked).
    pub(crate) ns: u64,
    /// How many bytes its check grew its stretch's frame by (0 when it was not checked): what
    /// the profile's largest-frame line reads.
    pub(crate) bytes: u64,
    /// Its entry's chain (work.rs `item_chain`; 0 when it was not found).
    chain: u64,
    note: Option<&'f BodyNote<'f>>,
    /// Offset and length in the stage's findings, stable across row sorts and phases.
    found: FindingRange,
    /// Its file: an index into the planner's outlines.
    pub(crate) file: u32,
    /// Where its check stood in its stretch (engine_memory.rs `BodyFrames`).
    pub(crate) stretch: InStretch,
}

impl<'f> Ran<'f> {
    fn without_check(name: Name, label: &'f str, file: u32) -> Self {
        Self { name, label, ns: 0, bytes: 0, chain: 0, note: None, found: FindingRange::default(), file, stretch: InStretch::No }
    }

    fn said(&self) -> Option<&Said<&'f str>> {
        self.note.map(|note| &note.said)
    }
}

#[derive(Default)]
struct FindingRange {
    start: u32,
    len: u32,
}

/// A body found in its crate's records: its name and its item's name as written.
struct Took<'f> {
    name: Name,
    label: &'f str,
    /// Its member crate (an index of [`Group`]s) and the item keys it checks (its own body's,
    /// or each body an expansion at its entry wrote), or why it has none.
    item: Result<(u32, &'f [ItemKey]), &'static str>,
    /// Its entry's place: the entry's chain (work.rs `item_chain`) and, for an entry whose
    /// bodies a macro wrote, each one's label beside its key ([`Place`]).
    place: Place<'f>,
    /// The unit whose records found it (an index of `Found::holders`' units).
    unit: u32,
}

/// Where a body task's findings are printed: in the entry's item, found by its chain in its
/// file's outline as it is when printed (engine_lines.rs, a placed finding; rule 71).
#[derive(Clone, Copy)]
struct Place<'f> {
    chain: u64,
    /// The entry's file, as its crate's records number it (front2 `Records::file_now`), and the
    /// entry's range in that text: where a stretch reads the item's text to say its findings'
    /// ranges as token places ([`found_of`]).
    file: u32,
    item: blaze_surface::Range,
    /// One a body the task checks, in the order of its keys: `None` for the entry's own body
    /// (its findings' ranges are from the item's start); else a body whose text's place in the
    /// file is not known, by its label, so each finding is at the item's start, said so: a body
    /// an expansion at the entry wrote (body_types/covered.rs `written_label`,
    /// `WRITTEN_UNPLACED`), or a body of a block's module (covered.rs `in_blocks`,
    /// `BLOCK_UNPLACED`). Shorter than the keys: the rest are `None`.
    written: &'f [Option<Unplaced<'f>>],
}

/// A body of a task whose text's place in its file is not known: its label and the words that
/// say so (body_types/covered.rs `WRITTEN_UNPLACED` or `BLOCK_UNPLACED`).
#[derive(Clone, Copy)]
struct Unplaced<'f> {
    label: &'f str,
    words: &'static str,
}

/// A body of a block's module as a reader names it: the entry whose task checks it, then its
/// own name, `{a block of `_`}::serialize` (a member's: its own name in its impl).
struct BlockLabel<'r, 'n> {
    records: &'n blaze_ladder::front2::source::Records<'n>,
    entry: &'r str,
    body: BodyRef<'n>,
}

impl core::fmt::Display for BlockLabel<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self.body {
            BodyRef::Free(key) => NamesSource::name_of(self.records, key).unwrap_or("_"),
            BodyRef::Member { name, .. } => name,
        };
        write!(f, "{{a block of `{}`}}::{name}", self.entry)
    }
}

/// One finding of a body task, as `check` files it (body_types.rs, where a body's findings are
/// filed): its rule, where it is in its item ([`FoundAt`]), and its message's pieces joined.
#[derive(Clone, Copy)]
struct BodyFinding<'f> {
    rule: u16,
    at: FoundAt,
    message: &'f [u8],
}

/// What a run hands on beside its `Done`s, filled as each body's task ends: each finding as
/// (the body's index in the run's `Ran`s, rule, place, its message's end in `messages`), in
/// the body's sorted order. The phase's arena, never the process heap; copied once into the
/// request's frame at the merge ([`BodiesStage::run_phase`]).
struct RunOut<'a> {
    found: SVec<'a, (usize, u16, FoundAt, usize)>,
    messages: SVec<'a, u8>,
    /// Indexed by row within this run. Only a body with a note occupies a slot.
    notes: SVec<'a, (usize, Said<blaze_own::Box<TaskError, SharedAlloc<'a>>>, Option<crate::engine_seeds::FirstFailure>)>,
    clean: u32,
    items: u64,
    item_tasks: u64,
    /// The most a body's scratch frame held when its body ended ([`BODY_SCRATCH`]): one body's
    /// alone when the scratch is reset per body, all the run's when it is not (the plant).
    scratch_most: u64,
}

impl<'a> RunOut<'a> {
    fn new(alloc: SharedAlloc<'a>) -> RunOut<'a> {
        RunOut { found: SVec::new_in(alloc), messages: SVec::new_in(alloc), notes: SVec::new_in(alloc), clean: 0, items: 0, item_tasks: 0, scratch_most: 0 }
    }
}

/// A body's scratch frame, reset when the body ends (rule 11): reserved once a run, nothing
/// committed until used. It holds every list of a body that is only a step toward its row
/// (lane BD3): the recorder's list and `seen` set, the cells before and after widening, the
/// verdict's words and bytes, its findings and their messages. A cap, not a size: the
/// recorder's own cap (front2 tables/recorder.rs `MOST_READS`, 2^20 reads) fills about 150 MB of
/// it with its cells, so 1 GiB leaves a body several times that; a body past it panics in its
/// own job (rule 20: the alloc-error hook) and is said as such.
const BODY_SCRATCH: usize = 1 << 30;

/// One body's memory as its task ends (lane BD3; rule 16): what it leaves in the request's
/// arena (its row, to the request's end) and what its check took in its scratch (freed as it
/// ends), each counted from a list's length or capacity as it stood: adds only, no walk.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct BodyMem {
    /// The row's bytes in the request's arena: `Done` and its lists' capacities.
    pub(crate) row: u64,
    /// The row's floor: `Done` and its lists' lengths (a list of exactly what it keeps).
    pub(crate) row_floor: u64,
    /// The cells its check read (each item's, before the evaluator's and the widening).
    pub(crate) recorded: u64,
    /// The cells handed to the widening (the evaluator's and the own crate's added).
    pub(crate) before: u64,
    /// The cells of its row, by kind: whole records, names cells, head cells, bucket cells, other.
    pub(crate) after: u64,
    pub(crate) whole: u64,
    pub(crate) names: u64,
    pub(crate) heads: u64,
    pub(crate) buckets: u64,
    pub(crate) other: u64,
    /// The crates of its check scope.
    pub(crate) scope: u64,
    /// What its check took in its scratch (front2 tables/task.rs `WorkBytes`), summed over its items.
    pub(crate) work: WorkBytes,
    /// Its scratch frame's use as it ended.
    pub(crate) scratch: u64,
}

/// The parts of [`BodyMem`] a line says, in its order.
const MEM_PARTS: usize = 19;

impl BodyMem {
    fn parts(&self) -> [u64; MEM_PARTS] {
        let w = &self.work;
        [
            self.row,
            self.row_floor,
            self.recorded,
            self.before,
            self.after,
            self.whole,
            self.names,
            self.heads,
            self.buckets,
            self.other,
            self.scope,
            w.recorder,
            w.recorder_floor,
            w.cells,
            w.held,
            w.said,
            w.verdict,
            w.goal_hits,
            self.scratch,
        ]
    }

    /// The row's read list counted by kind (`reads`: the row's, sorted, each once).
    fn count_row(&mut self, reads: &[(CellId, ContentId)]) {
        self.after = reads.len() as u64;
        for (id, _) in reads.iter() {
            let n = match (id.cell == WHOLE, id.of.kind) {
                (true, _) => &mut self.whole,
                (false, Kind::CrateScope) => &mut self.names,
                (false, Kind::Signature) => &mut self.heads,
                (false, Kind::ImplBucket) => &mut self.buckets,
                (false, _) => &mut self.other,
            };
            *n += 1;
        }
    }

    /// The row's bytes and its floor, from `done`'s capacities and lengths.
    fn count_done(&mut self, done: &Done<'_>) {
        let read = core::mem::size_of::<(CellId, ContentId)>() as u64;
        let define = core::mem::size_of::<(Cell, ContentId)>() as u64;
        let fixed = core::mem::size_of::<Done<'_>>() as u64;
        self.row = fixed + done.reads.capacity() as u64 * read + done.defines.capacity() as u64 * define;
        self.row_floor = fixed + done.reads.len() as u64 * read + done.defines.len() as u64 * define;
    }
}

/// Every body's [`BodyMem`] summed and at its most, written by the workers as each body ends
/// (relaxed adds; the counts are a gauge, read whole only after the join or by the status
/// line, where a body half-counted is one body's difference). Held on the status board
/// (engine_status.rs `Board::bodies_mem`), so the status, the stop's profile and the end's
/// profile each say it ([`BodyMemLive::line`]).
pub(crate) struct BodyMemLive {
    bodies: core::sync::atomic::AtomicU64,
    sums: [core::sync::atomic::AtomicU64; MEM_PARTS],
    most: [core::sync::atomic::AtomicU64; MEM_PARTS],
}

impl BodyMemLive {
    pub(crate) const fn new() -> BodyMemLive {
        BodyMemLive { bodies: core::sync::atomic::AtomicU64::new(0), sums: [const { core::sync::atomic::AtomicU64::new(0) }; MEM_PARTS], most: [const { core::sync::atomic::AtomicU64::new(0) }; MEM_PARTS] }
    }

    /// One body ended with `mem`.
    pub(crate) fn add(&self, mem: &BodyMem) {
        use core::sync::atomic::Ordering::Relaxed;
        self.bodies.fetch_add(1, Relaxed);
        for (at, part) in mem.parts().iter().enumerate() {
            self.sums[at].fetch_add(*part, Relaxed);
            self.most[at].fetch_max(*part, Relaxed);
        }
    }

    /// The bodies counted so far.
    pub(crate) fn bodies(&self) -> u64 {
        self.bodies.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The line, a sibling of the profile's `bodies: held per body` line (nothing before the
    /// first body ends): `bodies memory: <n> bodies · row kept to the request's end <mean>
    /// bytes a body (most <m>; floor <f>, <r>x) · read cells a body: recorded <mean>/<most>,
    /// before widening .., after .. (whole records .., names cells .., head cells .., bucket
    /// cells .., other ..; scope crates ..) · freed as it ends, in its scratch: recorder <mean>
    /// bytes (most ..; floor .., <r>x), cells .., held .., said .., verdict .., goal hits ..;
    /// scratch at its end <mean> (most ..)`. Means rounded down; a ratio in tenths.
    pub(crate) fn line(&self, into: &mut impl core::fmt::Write) -> core::fmt::Result {
        use core::sync::atomic::Ordering::Relaxed;
        let n = self.bodies();
        if n == 0 {
            return Ok(());
        }
        let sum = |at: usize| self.sums[at].load(Relaxed);
        let mean = |at: usize| sum(at) / n;
        let most = |at: usize| self.most[at].load(Relaxed);
        let ratio = |of: usize, floor: usize| Tenths(sum(of).saturating_mul(10).checked_div(sum(floor)).unwrap_or(0));
        writeln!(
            into,
            "bodies memory: {n} bodies · row kept to the request's end {} bytes a body (most {}; floor {}, {}x) · read cells a body: recorded {}/{}, before widening {}/{}, after {}/{} (whole records {}, names cells {}, head cells {}, bucket cells {}, other {}; scope crates {}) · freed as it ends, in its scratch: recorder {} bytes (most {}; floor {}, {}x), cells {} (most {}), held {}, said {}, verdict {}, goal hits {}; scratch at its end {} (most {})",
            mean(0),
            most(0),
            mean(1),
            ratio(0, 1),
            mean(2),
            most(2),
            mean(3),
            most(3),
            mean(4),
            most(4),
            mean(5),
            mean(6),
            mean(7),
            mean(8),
            mean(9),
            mean(10),
            mean(11),
            most(11),
            mean(12),
            ratio(11, 12),
            mean(13),
            most(13),
            mean(14),
            mean(15),
            mean(16),
            mean(17),
            mean(18),
            most(18),
        )
    }
}

/// A ratio in tenths, said `12.3`.
struct Tenths(u64);

impl core::fmt::Display for Tenths {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}", self.0 / 10, self.0 % 10)
    }
}

/// The floor of what the stage holds a body after its task ends (rule 67): its task's name (the
/// key it is found by), its record's key (the verdict's digest) and a status byte.
pub(crate) const HELD_FLOOR: usize = core::mem::size_of::<Name>() + core::mem::size_of::<ContentId>() + 1;

/// What the stage holds past the bodies' tasks ([`BodiesStage::held`]): every body's row, its
/// label, findings and note words, summed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) bodies: u64,
    labels: u64,
    findings: u64,
    words: u64,
}

impl Held {
    const ROW: u64 = core::mem::size_of::<Ran<'_>>() as u64;

    fn mean(self, bytes: u64) -> u64 {
        bytes.checked_div(self.bodies).unwrap_or(0)
    }

    /// Bytes held a body, rounded down (0 with no body).
    pub(crate) fn per_body(self) -> u64 {
        let rows = self.bodies.saturating_mul(Self::ROW);
        self.mean(rows.saturating_add(self.labels).saturating_add(self.findings).saturating_add(self.words))
    }
}

/// PLANTED BUG, tests only (rule 23): a body's scratch is never reset between bodies, so a run's
/// scratch holds every body's findings at once; the answers must not change. Read on the
/// coordinator's thread when a phase begins ([`BodiesStage::run_phase`]).
#[cfg(test)]
thread_local! {
    pub(crate) static PLANT_KEEP_SCRATCH: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// PLANTED BUG, tests only (rule 23): the old arm of `groups`, every body entry the records
    /// list no body at taken as no unit's (`ENTRY_ABSENT`), silent: the control of the coverage
    /// tests. Read on the coordinator's thread ([`absent_entry`]).
    pub(crate) static PLANT_SILENT_ABSENT: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// PLANTED BUG, tests only (rule 23; lane BD3): the old row, its read list in the request's
    /// arena at the capacity of the list before widening (`widen` made it with the recorded
    /// count), so the memory line's row bytes are what the stage kept before. Read on the
    /// coordinator's thread when a phase begins.
    pub(crate) static PLANT_ROW_AT_RECORDED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// PLANTED BUG, tests only (rule 23; lane BD3): the coarse step off, a dependency's bucket
    /// reads kept one a bucket (as before) instead of its tables' `WHOLE`. Read on the
    /// coordinator's thread when a phase begins.
    pub(crate) static PLANT_FINE_BUCKETS: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// The memory plants of lane BD3 as the coordinator read them ([`PLANT_ROW_AT_RECORDED`],
/// [`PLANT_FINE_BUCKETS`]); both false outside tests.
#[derive(Clone, Copy, Default)]
struct RowPlants {
    row_at_recorded: bool,
    fine_buckets: bool,
}

#[cfg(test)]
fn planted_rows() -> RowPlants {
    RowPlants { row_at_recorded: PLANT_ROW_AT_RECORDED.with(core::cell::Cell::get), fine_buckets: PLANT_FINE_BUCKETS.with(core::cell::Cell::get) }
}

#[cfg(not(test))]
fn planted_rows() -> RowPlants {
    RowPlants::default()
}

#[cfg(test)]
fn planted_keep_scratch() -> bool {
    PLANT_KEEP_SCRATCH.with(core::cell::Cell::get)
}

#[cfg(not(test))]
fn planted_keep_scratch() -> bool {
    false
}

#[cfg(test)]
fn planted_silent_absent() -> bool {
    PLANT_SILENT_ABSENT.with(core::cell::Cell::get)
}

#[cfg(not(test))]
fn planted_silent_absent() -> bool {
    false
}

/// Where a body's finding is in its item. By tokens, never bytes (rule 71): the task's key is
/// its item's tokens (blaze-file-items, "Identity"), so it stands across a comment or a reformat
/// and its stored lines are printed again unentered; a byte range would then point into the old
/// layout, a token place points at the same tokens in the new (`blaze_file_items::place_bytes`,
/// at print time).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FoundAt {
    /// At the item's start: a body whose text's place in its file is not known ([`Unplaced`]).
    Start,
    /// These tokens of the item's text, counted from its first token.
    Tokens(parser::token_key::TokenPlace),
}

/// One member crate's check scope as a run reads it: all read-only, shared by its runs. Its
/// crates' records are borrowed in place from the store's mapping for `'r` (engine_records.rs).
struct Group<'f, 'r> {
    /// The member's own `CrateScope` name.
    own: Name,
    /// Its scope's crates in reader order, the member at 0.
    crates: Vec<Crate<'r>, Global>,
    /// The member's environment, what its bodies' `env!` read: its crate list facts with its
    /// build script's outputs joined (body_types.rs `owner_env_by`), lent to each run, and the
    /// script run read for it (a `Script` task's WHOLE cell), in every body's read list.
    env: Option<EnvLent<'f, 'f>>,
    script: Option<Name>,
    /// The places of its lang crates among `crates`.
    lang: Vec<usize, Global>,
    /// Each scope crate's `CrateScope` name, sorted: the whole graph's bucket fact widens to these.
    scope: Vec<Name, Global>,
    /// Each scope crate's impl tables' task name (front2 buckets/records.rs `impl_tables_name`)
    /// with its `CrateScope` name, sorted by the first: how [`widen`] tells a dependency's
    /// bucket read (its crate defines no `CELLS`), which it reads as the tables' `WHOLE`.
    /// A slice, as `cells_of`'s lists are (the list's allocator handle is no `Sync`, and a
    /// group is read by every run's worker).
    buckets: &'f [(Name, Name)],
}

/// One crate's check scope with its records opened: what the Heads stage files a crate's impls
/// over and the Bodies stage checks a member's bodies over ([`scope_of`], the one rule of both).
pub(crate) struct Scope<'r> {
    /// The crate's own records.
    pub(crate) own_records: crate::front_load::CrateRecords<'r>,
    /// Its scope's crates in reader order, the crate at 0 (cut at the first without records).
    pub(crate) crates: Vec<Crate<'r>, Global>,
    /// The places of its lang crates among `crates`.
    pub(crate) lang: Vec<usize, Global>,
    /// Each scope crate's `CrateScope` name, sorted.
    pub(crate) scope: Vec<Name, Global>,
    /// The first scope crate without records, by its world index, when `crates` was cut there,
    /// and why (one value, so a cut never lacks its cause): what a refusal names (rule 19, plan
    /// row A8, [`Scope::cut_cause`]), and, by its `CrateScope` name ([`Scope::cut_name`]), what
    /// a refusal of the cut scope reads beside `scope`.
    pub(crate) cut: Option<(usize, CutWhy)>,
}

impl Scope<'_> {
    /// The `CrateScope` name of the crate the scope was cut at ([`Scope::cut`]), the cell a
    /// refusal of the cut scope reads (as ABSENT where that crate has no `Names` task).
    pub(crate) fn cut_name(&self, list: &blaze_world::crate_list::CrateList<'static>) -> Option<Name> {
        self.cut.as_ref().map(|(one, _)| blaze_drive::key::crate_scope_name(&list.key(*one)))
    }

    /// The crate the scope was cut at and why, by its name: what a refusal of the cut scope
    /// prints (`check scope holds `p`, which ...`).
    pub(crate) fn cut_cause(&self, list: &blaze_world::crate_list::CrateList<'static>) -> Option<CutCause<'_>> {
        self.cut.as_ref().map(|(one, why)| CutCause { krate: CrateGraph::name(list, *one), why })
    }
}

/// Why a check scope was cut at one of its crates ([`Scope::cut`]).
#[derive(Debug)]
pub(crate) enum CutWhy {
    /// The crate's `CrateScope` task defines no names record this command: it failed
    /// (`failed`), or no `Names` task was planned for it.
    NoNamesRecord { failed: bool },
    /// Its names record is defined but could not be read in place (engine_records.rs).
    Unreadable(crate::engine_records::ReadError),
}

/// A cut scope's crate and its [`CutWhy`], printed after "its crate's" or "its member's".
pub(crate) struct CutCause<'s> {
    pub(crate) krate: &'static str,
    pub(crate) why: &'s CutWhy,
}

impl core::fmt::Display for CutCause<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let krate = self.krate;
        match self.why {
            CutWhy::NoNamesRecord { failed: true } => {
                write!(f, "check scope holds `{krate}`, which has no names record this command (its `CrateScope` task failed: that failure is reported at it)")
            }
            CutWhy::NoNamesRecord { failed: false } => {
                write!(f, "check scope holds `{krate}`, which has no names record this command (it has no `Names` task: engine_names.rs `plan`)")
            }
            CutWhy::Unreadable(error) => write!(f, "check scope holds `{krate}`, whose names record could not be read: {error} (engine_bodies.rs `scope_of`)"),
        }
    }
}

/// Why crate `at` has no check scope at all ([`scope_of`]): each drop the read could meet,
/// typed, with the crate named (rule 19).
#[derive(Debug)]
pub(crate) enum ScopeRefusal {
    /// The crate's own `CrateScope` task defines no names record this command: it failed
    /// (`failed`, reported at it), or no `Names` task was planned for it.
    NoNamesRecord { krate: &'static str, at: usize, failed: bool },
    /// Its own names record is defined but could not be read in place.
    Records { krate: &'static str, at: usize, error: crate::engine_records::ReadError },
    /// Its check scope could not be made from the crate graph (blaze-world check_scope.rs).
    CheckScope { error: blaze_world::check_scope::ScopeError<'static> },
}

impl core::fmt::Display for ScopeRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ScopeRefusal::NoNamesRecord { krate, failed: true, .. } => {
                write!(f, "crate `{krate}` has no names record this command (its `CrateScope` task failed: that failure is reported at it)")
            }
            ScopeRefusal::NoNamesRecord { krate, failed: false, .. } => {
                write!(f, "crate `{krate}` has no names record this command (it has no `Names` task: engine_names.rs `plan`)")
            }
            ScopeRefusal::Records { krate, error, .. } => write!(f, "the names record of crate `{krate}` could not be read: {error} (engine_bodies.rs `scope_of`)"),
            ScopeRefusal::CheckScope { error } => write!(f, "its check scope could not be made: {error} (engine_bodies.rs `scope_of`)"),
        }
    }
}

/// Crate `at`'s check scope (blaze-world check_scope.rs `check_scope_in`), each crate's
/// records read in place through `read` (engine_records.rs), and whether every crate of it had
/// records (`false`: the scope was cut, [`Scope::cut`] says where and why, and the caller refuses by
/// name). Refused: the crate's own names record or its scope could not be read, the cause typed
/// ([`ScopeRefusal`]); nothing is dropped (rule 3).
#[expect(clippy::too_many_arguments, reason = "a stage's handles, each named")]
pub(crate) fn scope_of<'r>(read: &mut EngineRecords<'r>, cx: &Cx, store: &mut dyn crate::store::Store<'r>, names: &NamesStage<'_>, list: &blaze_world::crate_list::CrateList<'static>, at: usize, heap: &'static blaze_own::Heap) -> Result<(Scope<'r>, bool), ScopeRefusal> {
    let g = heap.alloc();
    let name_at = |at: usize| blaze_drive::key::crate_scope_name(&list.key(at));
    let input_at = |at: usize| names.input_of(name_at(at));
    let krate = CrateGraph::name(list, at);
    let own = input_at(at).ok_or_else(|| ScopeRefusal::NoNamesRecord { krate, at, failed: names.failed(name_at(at)) })?;
    let own_records = read.scope_records(cx, store, &own, list.key(at), krate, g).map_err(|error| ScopeRefusal::Records { krate, at, error })?;
    let no_core = blaze_scope::names_record::NamesRecord::open(own_records.names_proof).crate_root().head().no_core;
    let checked = blaze_world::check_scope::check_scope_in(list, at, no_core, g).map_err(|error| ScopeRefusal::CheckScope { error })?;
    let mut scope = Scope { own_records, crates: Vec::new_in(Global), lang: Vec::new_in(Global), scope: Vec::new_in(Global), cut: None };
    scope.lang.extend(checked.lang_places());
    let mut whole = true;
    for one in checked.crates() {
        let Some(input) = input_at(one) else {
            whole = false;
            scope.cut = Some((one, CutWhy::NoNamesRecord { failed: names.failed(name_at(one)) }));
            break;
        };
        let records = match read.scope_records(cx, store, &input, list.key(one), CrateGraph::name(list, one), g) {
            Ok(records) => records,
            Err(error) => {
                whole = false;
                scope.cut = Some((one, CutWhy::Unreadable(error)));
                break;
            }
        };
        scope.crates.push(Crate { records, name: CrateGraph::name(list, one), package: list.shown_from(one), world_at: one });
        scope.scope.push(input.name);
    }
    scope.scope.sort_unstable();
    Ok((scope, whole))
}

/// Each outlined file's bodies by name with their labels (or the words of why its text could
/// not be read again), as [`planned_label`] makes them: in the request's frame.
type Labels<'f> = Vec<(u32, Result<Vec<(Name, &'f str), &'f Frame>, &'f str>), &'f Frame>;

/// The label body `name` of outline `file` prints when it was refused before any records named
/// it (a member's scope not read, its file claimed by no unit): the label a checked body prints
/// (`groups`: [`crate::item_name::shown_name`] of its outline item, or the macro call it is),
/// made from the file's text outlined again under its part's edition, once a file (`labels`).
/// Only printed, never keyed (rule 71). A file whose text no longer outlines to the body
/// prints "" ([`crate::engine_body_plan::Named`] then names its place); one that cannot be read
/// prints why, in braces as a label that is not a name.
fn planned_label<'f>(labels: &mut Labels<'f>, cx: &Cx, root: &Dir<ReadWrite>, outlined: &[crate::engine_tasks::Outlined<'_>], file: u32, name: Name, frame: &'f Frame) -> &'f str {
    let at = match labels.iter().position(|(one, _)| *one == file) {
        Some(at) => at,
        None => {
            let made = file_labels(cx, root, outlined, file, frame).map_err(|lost| {
                let words = bstd::mem::format_in!(frame, "{{a body of a file whose text could not be read again to name it: {lost}}}");
                core::str::from_utf8(words.into_bytes().leak()).unwrap_or("{a body of a file whose text could not be read again}")
            });
            labels.push((file, made));
            labels.len() - 1
        }
    };
    match &labels[at].1 {
        Ok(bodies) => bodies.iter().find(|(one, _)| *one == name).map_or("", |(_, label)| *label),
        Err(words) => words,
    }
}

/// Why a file's text could not be read again for its bodies' labels ([`planned_label`]).
#[derive(Debug)]
enum LabelLost {
    /// No outline of that index ([`crate::engine_tasks::Outlined`]).
    NoOutline { file: u32 },
    Path(bstd::path::PathError),
    Read(bstd::fs::FsError),
    NotText,
    NoEdition { byte: u8 },
}

impl core::fmt::Display for LabelLost {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LabelLost::NoOutline { file } => write!(f, "outline {file} is not among the planner's"),
            LabelLost::Path(error) => write!(f, "its path: {error}"),
            LabelLost::Read(error) => write!(f, "{error}"),
            LabelLost::NotText => f.write_str("its bytes are not UTF-8"),
            LabelLost::NoEdition { byte } => write!(f, "its edition byte {byte} is no edition"),
        }
    }
}

/// Every body of outline `file` by its name ([`work::body_name`]), each labelled as `groups`
/// labels a checked one.
fn file_labels<'f>(cx: &Cx, root: &Dir<ReadWrite>, outlined: &[crate::engine_tasks::Outlined<'_>], file: u32, frame: &'f Frame) -> Result<Vec<(Name, &'f str), &'f Frame>, LabelLost> {
    let one = outlined.get(file as usize).ok_or(LabelLost::NoOutline { file })?;
    let path = core::str::from_utf8(&one.path).map_err(|_| LabelLost::NotText)?;
    let mut bytes = bstd::mem::Bytes::new_in(frame, "a source file whose refused bodies are named");
    root.read_all(cx, &bstd::path::Path::new(path).map_err(LabelLost::Path)?, &mut bytes).map_err(LabelLost::Read)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| LabelLost::NotText)?;
    let edition = blaze_surface::Edition::from_byte(one.edition).ok_or(LabelLost::NoEdition { byte: one.edition })?;
    let erased = blaze_own::Erased::new(frame);
    let outline = blaze_file_items::outline(text, edition, erased.handle());
    let mut found = Vec::new_in(frame);
    for item in outline.items().iter() {
        if work::checked_body(&outline, item).is_none() && !work::expansion_entry(&outline, item, text) {
            continue;
        }
        let shown = crate::item_name::shown_name(item, text);
        let label = match shown.is_empty() {
            false => bstd::mem::format_in!(frame, "{shown}"),
            true => bstd::mem::format_in!(frame, "{{a macro call}}"),
        };
        let label: &'f str = core::str::from_utf8(label.into_bytes().leak()).unwrap_or("");
        found.push((work::body_name(one.name, &outline, item, text), label));
    }
    Ok(found)
}

/// One body a run checks.
#[derive(Clone, Copy)]
struct One<'f> {
    task: Task,
    /// The bodies it checks: one, or each an expansion at its entry wrote (none: a clean pass).
    items: &'f [ItemKey],
    group: u32,
    file: u32,
    label: &'f str,
    place: Place<'f>,
    /// The cells that say the units ranked before its checker do not hold it (engine_body_plan.rs
    /// `Holders::cells`): in its reads, so a unit that comes to hold it enters it again.
    held: &'f [CellId],
}

/// The phase-independent handles the Bodies phase reads.
pub(crate) struct Handles<'h, 'e, 'n, 'o> {
    pub(crate) root: &'h Dir<ReadWrite>,
    pub(crate) env: &'h FetchEnv<'e>,
    /// The `UnitGraph`'s record as a reader takes it; `None`: the command has no graph.
    pub(crate) graph: Option<Input>,
    pub(crate) names: Option<&'h NamesStage<'n>>,
    pub(crate) outlined: &'h [Outlined<'o>],
    pub(crate) workers: NonZeroU32,
    pub(crate) clock: &'h Clock,
    /// Where each body says it runs (engine_status.rs); a stop refuses the bodies not begun.
    pub(crate) board: &'h crate::engine_status::Board<'h>,
}

/// Why the Bodies stage checks no body this command, and the cells that stop is a function of
/// (plan row A8; none: owed), from [`BodiesStage::groups`].
type Stop<'f> = (&'static str, &'f [CellId]);

/// The packs of the crate list's trees and its corrected files (body_types.rs `Kept::trees`).
type Trees = (&'static [blaze_fetch::PackView<'static>], &'static [(u32, &'static str, &'static str)]);

/// What [`BodiesStage::groups`] found: each member's group, each body's place, the files of
/// member crates, and the members' names.
struct Found<'f, 'r> {
    groups: Vec<Group<'f, 'r>, Global>,
    /// One a body a unit's records found: in the request's heap, never the command's fixed
    /// frame; in the units' order, indexed by `holders`' decisions.
    took: Vec<Took<'f>, blaze_own::Alloc<'static>>,
    /// Every member unit's word on the files its module tree reaches and the bodies its records
    /// hold: who checks each body (engine_body_plan.rs).
    holders: crate::engine_body_plan::Holders<'f>,
    members: Vec<Name, Global>,
    /// Every member crate's names cell (`WHOLE`), with a `Names` task or without, sorted.
    member_cells: Vec<CellId, Global>,
    /// Bodies a macro wrote that no entry covers (covered.rs `written_by` gave no file entry, and
    /// `in_blocks` no block): checked by no task, said in the profile.
    uncovered: u32,
    /// Each member crate's outlined files by its key's prefix and its items record's file
    /// index, sorted: where a head read finds its head task (engine_heads.rs `Owners`, [`widen`]).
    heads: Vec<([u8; 8], u32, Name), Global>,
    trees: Trees,
}

/// The stage: the crates' records read once a command, and every body's result.
pub(crate) struct BodiesStage<'f> {
    /// The records read so far, shared with the Heads stage (engine_heads.rs).
    pub(crate) kept: Kept,
    /// Every crate's impl tables, put by the Heads stage (engine_heads.rs) and read only from
    /// the Bodies phase on: each run lends it (`TableSet::lent`).
    pub(crate) shelf: ImplShelf,
    /// Every body entered, sorted by name after its phase.
    pub(crate) ran: Vec<Ran<'f>, Global>,
    /// All runs append in order. The frame reserves space for a phase's findings in one go.
    findings: Option<Vec<BodyFinding<'f>, &'f Frame>>,
    /// Rows with no note or findings (including rows the plan left outside).
    clean: u32,
    /// Item bodies checked and tasks with at least one item body.
    items: u64,
    item_tasks: u64,
    /// Bodies of files no member crate of the graph holds: checked by no crate, counted.
    pub(crate) ownerless: u32,
    /// Their files, by outline index, sorted and once each.
    ownerless_files: Vec<u32, Global>,
    /// The phase's wall time, ns, and the runs it was cut into.
    pub(crate) wall_ns: u64,
    pub(crate) runs: u32,
    /// Bodies a macro wrote that no entry covers (`Found::uncovered`), said in the profile.
    uncovered: u32,
    /// Their note's words, the first named by label ([`MOST_UNCOVERED_NAMED`], [`Uncovered`]):
    /// the request's lines say them ([`BodiesStage::plan_lines`]), so none is left out in
    /// silence (rule 3). `None`: every body a macro wrote is covered.
    uncovered_note: Option<&'static str>,
    /// Whether this request's phase found the members' records (`groups` ran whole): else the
    /// uncovered note stands from the last request's lines.
    groups_found: bool,
    /// The most a body's scratch frame held when its body ended, over every run ([`RunOut`]).
    scratch_most: u64,
    /// The bodies checked from the request's frozen heads, refused at a seed, and their rounds
    /// (engine_seeds.rs `SeedTally`), summed over every run.
    pub(crate) seeds: crate::engine_seeds::SeedTally,
    /// Every head not frozen, this request's and the standing head tasks' (engine_head_notes.rs),
    /// set before the Bodies phase: a body that reads one names it and quotes why.
    pub(crate) heads_failed: crate::engine_head_notes::Failed,
    /// Bodies no unit holds (engine_body_plan.rs): no unit's, never refused; a file's note once.
    outside: crate::engine_body_plan::Outside,
    /// How a body's names reads are kept (engine_scope_reads.rs), the command's choice.
    scope_reads: crate::engine_scope_reads::ScopeReadsAs,
}

impl<'f> BodiesStage<'f> {
    pub(crate) fn new(scope_reads: crate::engine_scope_reads::ScopeReadsAs) -> BodiesStage<'f> {
        BodiesStage {
            kept: Kept::default(),
            shelf: ImplShelf::new(ImplShelfOpts {}),
            ran: Vec::new_in(Global),
            findings: None,
            clean: 0,
            items: 0,
            item_tasks: 0,
            ownerless: 0,
            ownerless_files: Vec::new_in(Global),
            wall_ns: 0,
            runs: 0,
            uncovered: 0,
            uncovered_note: None,
            groups_found: false,
            scratch_most: 0,
            seeds: crate::engine_seeds::SeedTally::default(),
            heads_failed: crate::engine_head_notes::Failed::new(),
            outside: crate::engine_body_plan::Outside::new(),
            scope_reads,
        }
    }

    /// The paths of the first files (8 at most) whose bodies no member crate holds.
    pub(crate) fn ownerless_paths(&self, outlined: &[Outlined<'_>]) -> std::vec::Vec<std::string::String, Global> {
        let mut paths = std::vec::Vec::new_in(Global);
        for at in self.ownerless_files.iter().take(8) {
            paths.push(outlined.get(*at as usize).map_or_else(|| std::string::String::from("?"), |one| std::string::String::from_utf8_lossy(&one.path).into_owned()));
        }
        paths
    }

    /// The plan's notes into the request's lines (engine_body_plan.rs `Outside::lines`): a file
    /// no unit holds, said once, with no engine row; kept from `last` where no body of the file
    /// was entered.
    pub(crate) fn plan_lines(&self, outlined: &[Outlined<'_>], last: Option<&crate::engine_lines::Last<'_, '_>>, lines: &mut crate::engine_lines::Lines<'_>) {
        self.outside.lines(self.ran.iter().map(|one| one.file), outlined, last, lines);
        // The bodies a macro wrote that no task checks: named once a request, never silent.
        // Decided by this request's phase when it read the members' records; else the last
        // request's note stands (nothing it is a function of was read again).
        let name = crate::engine_body_plan::note_name(UNCOVERED_NOTE);
        match (self.groups_found, self.uncovered_note) {
            (true, Some(words)) => lines.note(name, crate::engine_lines::Class::Unchecked, words.as_bytes()),
            (true, None) => {}
            (false, _) => {
                if let Some(block) = last.and_then(|last| last.block(name)) {
                    lines.keep(name, block);
                }
            }
        }
    }

    /// Whether every body entered was checked: none refused, none in a file no crate holds.
    pub(crate) fn whole(&self) -> bool {
        self.ownerless == 0 && self.ran.iter().all(|one| matches!(one.said(), None | Some(Said::BorrowsUnchecked(_))))
    }

    /// One Bodies phase: every task entered once, its result in `buffer`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    pub(crate) fn run_phase<'a, 'r>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        handles: &Handles<'_, '_, '_, '_>,
        tasks: &[Task],
        view: View<'_, 'a>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        frame: &'f Frame,
        request_heads: Option<&crate::engine_seeds::RequestTable>,
        listings: &crate::engine_heads::Listings,
    ) -> Result<(), CliError> {
        let cx = cx.span("engine: one bodies phase");
        let began = handles.clock.now();
        let heap = handles.env.heap;
        // 1. Each member's check scope and files, and each body's item key, on this thread.
        let found = self.groups(&cx, store, read, handles, frame)?;
        let (found, stopped) = match found {
            Ok(found) => (Some(found), None),
            Err(why) => (None, Some(why)),
        };
        // 2. Each task: a body to check, or its result now.
        let mut ones: Vec<One<'f>, Global> = Vec::new_in(Global);
        // Who checks each body (engine_body_plan.rs `Holders::decide`), and what that is a
        // function of; reused a task.
        let mut passed: Vec<u32, Global> = Vec::new_in(Global);
        let mut met: Vec<CellId, Global> = Vec::new_in(Global);
        // Each file's bodies' labels, made once a file the first time a body of it is refused
        // with none ([`planned_label`]); at most one entry an outlined file.
        let mut labels: Labels<'f> = Vec::new_in(frame);
        for task in tasks.iter() {
            let name = task.planned.name;
            let file = handles.outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)).map_or(u32::MAX, |at| u32::try_from(at).unwrap_or(u32::MAX));
            // Each refusal with the cells it is a function of (plan row A8): its failed row
            // stands on them until one moves. A stop of the whole stage reads what `groups` says
            // it is a function of: the graph's cell for a crate list not made; none (owed,
            // entered again each request) for no graph, no names, or trees not opened, whose
            // causes are not cells (`groups`, at each stop).
            let none: &[CellId] = &[];
            let (refused, cells, label): (&'static str, &[CellId], &'f str) = match (stopped, found.as_ref()) {
                (Some((why, reads)), _) => (why, reads, ""),
                (None, None) => ("the stage's crates were not read (engine_bodies.rs `groups`)", none, ""),
                (None, Some(found)) => {
                    use crate::engine_body_plan::{Decision, OutsideIs};
                    let decided = found.holders.decide(name, file, &mut passed, &mut met);
                    let label_of = |took: Option<u32>| took.and_then(|at| found.took.get(at as usize)).map_or("", |one| one.label);
                    match decided {
                        Decision::Taken { took, .. } => {
                            // Held by a second unit too: checked under the first alone (the
                            // per-unit second check is the next piece), counted in the plan's line.
                            self.outside.shared += u32::from(found.holders.held_by_more(name));
                            let held = found.holders.cells(view, &passed, &met, heap.alloc());
                            match found.took.get(took as usize).map(|one| (one, one.item)) {
                                Some((one, Ok((group, items)))) => {
                                    ones.push(One { task: *task, items, group, file, label: one.label, place: one.place, held });
                                    continue;
                                }
                                // A taken body is one its records hold (`BodyIs::Held` is an
                                // `Ok` item): named if ever not, never skipped (rule 3).
                                Some((one, Err(why))) => (why, held, one.label),
                                None => ("its checking unit's find is gone (engine_body_plan.rs `decide`)", held, ""),
                            }
                        }
                        // Found by a unit's records, refused by them: their cells, and those of
                        // every unit ranked before that does not hold it.
                        Decision::Refused { why, took } => (why, found.holders.cells(view, &passed, &met, heap.alloc()), label_of(took)),
                        // No unit holds it: no unit's body, never a refusal. Done with no
                        // finding on the cells that say so (each unit's that reaches it), so a
                        // unit that comes to hold it enters it again.
                        Decision::Absent { .. } | Decision::NotHeld => {
                            let (is, label) = match decided {
                                Decision::Absent { took } => (OutsideIs::EntryAbsent, label_of(took)),
                                _ => (OutsideIs::FileNotHeld, ""),
                            };
                            let cells = found.holders.cells(view, &passed, &met, heap.alloc());
                            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
                            for id in cells.iter() {
                                reads.push((*id, view.cell(id)));
                            }
                            // No finding: the file's note is the plan's, in the lines record
                            // (engine_body_plan.rs `Outside::lines`), so the row keeps the certificate.
                            buffer.done.push(Done { name, key: task.key, out: crate::engine_body_plan::outside_out(name), defines: SVec::new_in(alloc), reads, findings: 0, failed: false });
                            self.outside.push(name, file, is);
                            self.clean += 1;
                            self.ran.push(Ran::without_check(name, label, file));
                            continue;
                        }
                        // No member crate's records hold the body's file: checked by no crate. It
                        // reads every member's names cell, so a member's task that comes moves it.
                        Decision::Unreached if found.members.is_empty() => ("no member crate's records could be read, so no crate holds this body's file", &found.member_cells[..], ""),
                        Decision::Unreached => {
                            self.ownerless += 1;
                            if let Err(slot) = self.ownerless_files.binary_search(&file) {
                                self.ownerless_files.insert(slot, file);
                            }
                            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::new_in(alloc);
                            for member in found.members.iter() {
                                let id = CellId { of: *member, cell: WHOLE };
                                reads.push((id, view.cell(&id)));
                            }
                            let out = ContentId::of(OWNERLESS_DOMAIN, &name.subject.0);
                            buffer.done.push(Done { name, key: task.key, out, defines: SVec::new_in(alloc), reads, findings: 0, failed: false });
                            self.clean += 1;
                            self.ran.push(Ran::without_check(name, "", file));
                            continue;
                        }
                    }
                }
            };
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
            for id in cells.iter() {
                reads.push((*id, view.cell(id)));
            }
            buffer.done.push(crate::engine_tasks::failed_with(task, reads, alloc));
            // A body refused before any records named it is printed by the label a checked body
            // prints (its outline item's name, `groups`), never an empty name (rule 19).
            let label = match label.is_empty() {
                true => planned_label(&mut labels, &cx, handles.root, handles.outlined, file, name, frame),
                false => label,
            };
            let note = frame.alloc(BodyNote { said: Said::Refused(refused), head_failed: None });
            self.ran.push(Ran { note: Some(note), ..Ran::without_check(name, label, file) });
        }
        self.outside.finish();
        // 3. Your code first is the next piece (engine.md 4); today: by member, file, name.
        ones.sort_unstable_by_key(|one| (one.group, one.file, one.task.planned.name));
        let mut runs: Vec<&[One<'f>], Global> = Vec::new_in(Global);
        let mut from = 0;
        while from < ones.len() {
            let group = ones[from].group;
            let mut to = from;
            while to < ones.len() && ones[to].group == group && to - from < RUN {
                to += 1;
            }
            runs.push(&ones[from..to]);
            from = to;
        }
        self.runs = u32::try_from(runs.len()).unwrap_or(u32::MAX);
        // Each run's results in the phase's arena: its `Done`s, its bodies' rows and their
        // findings ([`RunOut`]); merged below, never kept on the process heap.
        let mut buffers: SVec<'a, (Buffer<'a>, SVec<'a, Ran<'f>>, RunOut<'a>)> = SVec::with_capacity_in(runs.len(), alloc);
        for _ in 0..runs.len() {
            buffers.push((Buffer::new_in(alloc), SVec::new_in(alloc), RunOut::new(alloc)));
        }
        let Some(found) = found.as_ref() else {
            // Nothing to run: every task's result is in `buffer` above.
            self.ran.sort_unstable_by_key(|one| one.name);
            return Ok(());
        };
        // The same each phase (found from the same records): kept, not summed.
        self.uncovered = found.uncovered;
        let (packs, corrected) = found.trees;
        let procs = handles.names.and_then(|stage| stage.names.as_ref()).and_then(|names| names.procs());
        let keep_scratch = planted_keep_scratch();
        let plants = planted_rows();
        let shared = Shared { listings, root: handles.root, packs, corrected, procs, groups: &found.groups, heads: &found.heads, view, heap, shelf: &self.shelf, seeds: request_heads, heads_failed: &self.heads_failed, scope_reads: self.scope_reads, board: handles.board, keep_scratch, plants };
        // Each run's counts of its seeded bodies, summed after the join.
        let mut tallies: Vec<crate::engine_seeds::SeedTally, Global> = Vec::new_in(Global);
        tallies.resize(runs.len(), crate::engine_seeds::SeedTally::default());
        {
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            // One handle a run: as many as the project's bodies over `RUN`, so in the heap.
            let mut running = Vec::new_in(heap.alloc());
            for ((run, (out, ran, run_out)), tally) in runs.iter().zip(buffers.iter_mut()).zip(tallies.iter_mut()) {
                let shared = &shared;
                let clock = handles.clock.clone();
                running.push(scope.may_overlap("engine bodies run", BODIES_FRAME, move |cx: &Cx| {
                    run_bodies(cx, shared, run, &clock, out, ran, run_out, tally, alloc);
                    Ok::<(), Infallible>(())
                }));
            }
            let mut failed = None;
            for handle in running.into_iter() {
                if let Err(error) = handle.join(&cx) {
                    failed.get_or_insert(error);
                }
            }
            scope.join(&cx).map_err(CliError::Pool)?;
            if let Some(error) = failed {
                return Err(CliError::Pool(error));
            }
        }
        for tally in tallies.iter() {
            self.seeds.add(tally);
        }
        let findings = self.findings.get_or_insert_with(|| Vec::new_in(frame));
        let mut total = findings.len();
        for (_, ran, run) in &buffers {
            // The exclusive end of every row's range must fit in u32. Name the first
            // unrepresentable finding's body before reserving or merging any of the runs.
            if let Some(&(row, ..)) = run.found.get(u32::MAX as usize - total) {
                return Err(EngineCheckError::Unplaced {
                    body: ran[row].name,
                    what: "the Bodies stage exceeds its limit of 4294967295 retained findings",
                }.into());
            }
            total += run.found.len();
        }
        findings.try_reserve_exact(total - findings.len()).map_err(|_| EngineCheckError::Grow { what: "the Bodies stage's findings", had: findings.len() })?;
        for (mut out, mut ran, run_out) in buffers.into_iter() {
            buffer.done.append(&mut out.done);
            buffer.made.append(&mut out.made);
            for one in ran.iter() {
                times.push(one.name, one.ns);
            }
            for (row, said, head_failed) in run_out.notes {
                let said = said.map_task(|error| {
                    let text = bstd::mem::format_in!(frame, "{error}");
                    &*blaze_own::Box::leak(text.into_boxed_str())
                });
                ran[row].note = Some(frame.alloc(BodyNote { said, head_failed }));
            }
            self.clean += run_out.clean;
            self.items += run_out.items;
            self.item_tasks += run_out.item_tasks;
            // One copy of the run's messages. Each row addresses its contiguous findings in
            // the request's list, including after the rows are sorted by name.
            self.scratch_most = self.scratch_most.max(run_out.scratch_most);
            let messages: &'f [u8] = frame.alloc_slice_copy(&run_out.messages);
            let mut from = 0usize;
            for &(body, rule, at, end) in &run_out.found {
                let range = &mut ran[body].found;
                if range.len == 0 {
                    range.start = findings.len() as u32;
                }
                range.len += 1;
                findings.push(BodyFinding { rule, at, message: &messages[from..end] });
                from = end;
            }
            self.ran.extend(ran.drain(..));
        }
        self.ran.sort_unstable_by_key(|one| one.name);
        self.wall_ns = self.wall_ns.saturating_add(handles.clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)));
        Ok(())
    }

    /// Each member crate's [`Group`] and each body of its files as a [`Took`]; `Err((why,
    /// reads))` when no body can be checked this command: the stop is every body's note, and
    /// `reads` the cells it is a function of (plan row A8; none: owed, see each stop).
    fn groups<'r>(&mut self, cx: &Cx, store: &mut dyn crate::store::Store<'r>, read: &mut EngineRecords<'r>, handles: &Handles<'_, '_, '_, '_>, frame: &'f Frame) -> Result<Result<Found<'f, 'r>, Stop<'f>>, CliError> {
        let heap = handles.env.heap;
        let g = heap.alloc();
        // Set again when this phase finds the members' records whole (the end of this fn).
        self.groups_found = false;
        self.uncovered_note = None;
        // No `UnitGraph` task was planned (no lock file to match) and no `Names` stage ran: no
        // cell of this command says so, so these two stops read nothing and are owed, entered
        // again each request, until the plan's own inputs (the manifests and the lock, plan row
        // D2) are cells a row can read.
        let Some(graph) = handles.graph else {
            return Ok(Err(("this command has no unit graph (no lock file to match), so no crate's names, so no types", &[])));
        };
        let Some(names) = handles.names else {
            return Ok(Err(("this command resolved no crate's names, so no body is checked", &[])));
        };
        let graph_cell: &'f [CellId] = &*Vec::leak({
            let mut one: Vec<CellId, blaze_own::Alloc<'static>> = Vec::new_in(g);
            one.push(CellId { of: graph.name, cell: WHOLE });
            one
        });
        // The crate list is the `UnitGraph` record's: its world not made is a function of the
        // graph's cell; a new graph record moves it.
        let Some(list) = self.kept.list(cx, store, &graph, frame)? else {
            return Ok(Err(("the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)", graph_cell)));
        };
        // The trees are packs mapped from the shelf and the sources store (body_types.rs
        // `open_trees`): a refusal there is of the machine's files, not of a cell (a pack missing
        // or not the one the list's world read), so it reads nothing and is owed, entered again
        // each request (rule 3: no invented read to make it stand).
        let (packs, corrected) = match self.kept.trees(cx, handles.env, graph.out, &list, frame) {
            Ok(trees) => trees,
            Err(why) => return Ok(Err((why, &[]))),
        };
        let mut groups: Vec<Group<'f, 'r>, Global> = Vec::new_in(Global);
        let mut took: Vec<Took<'f>, blaze_own::Alloc<'static>> = Vec::new_in(g);
        // The first list that could not grow: the stage's typed stop (rule 3).
        let mut grown: Result<(), EngineCheckError> = Ok(());
        // Each member unit's word on each file its module tree reaches, and the units in the
        // crate list's order (engine_body_plan.rs: who checks each body is decided from these).
        let mut member_files: Vec<FileClaim<'f>, Global> = Vec::new_in(Global);
        let mut units: Vec<crate::engine_body_plan::Unit, Global> = Vec::new_in(Global);
        // A unit whose cfg holds `test` (its world crate's): ranked after the others.
        let world = names.names.as_ref();
        let is_test = |name: Name| world.is_some_and(|names| names.crate_at(name.subject).is_some_and(|at| names.world().crates[at].input.cfg.iter().any(|(atom, value)| *atom == "test" && value.is_none())));
        let mut members: Vec<Name, Global> = Vec::new_in(Global);
        // Every member's names cell, with a `Names` task or without: what "no member crate's
        // records could be read" reads.
        let mut member_cells: Vec<CellId, Global> = Vec::new_in(Global);
        let mut uncovered = 0u32;
        // The first of them by label (at most [`MOST_UNCOVERED_NAMED`]), in the request's heap.
        let mut uncovered_named: Vec<&'static str, blaze_own::Alloc<'static>> = Vec::new_in(g);
        let mut heads: Vec<([u8; 8], u32, Name), Global> = Vec::new_in(Global);
        let procs = names.names.as_ref().and_then(|names| names.procs());
        let input_at = |at: usize| names.input_of(blaze_drive::key::crate_scope_name(&list.key(at)));
        // The cells a refusal is a function of, in the request's heap with the bodies' labels:
        // each crate's names cell (`WHOLE`: its `Names` task defines it, or none does and it is
        // read ABSENT, so the task that comes, or the record that changes, moves it) and the
        // graph's, which the crate list and each check scope are made from (rule 21).
        let cells_of = |crates: &[Name]| -> &'f [CellId] {
            let mut ids: Vec<CellId, blaze_own::Alloc<'static>> = Vec::new_in(g);
            ids.push(CellId { of: graph.name, cell: WHOLE });
            for one in crates.iter() {
                ids.push(CellId { of: *one, cell: WHOLE });
            }
            ids.sort_unstable();
            ids.dedup();
            &*ids.leak()
        };
        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at)) {
            // The member's files: what its module tree reaches from its root, the walk the
            // planner took (the `Owners` fact `check` reads, blaze-drive owners.rs): each is
            // this member's, checked or refused by name, never in "no crate holds" (rule 3).
            let reach = crate::engine_body_plan::reached(handles.outlined, Name::file(list.root(member).strip_prefix("/0/").unwrap_or(list.root(member)).as_bytes()), g);
            let own_name = blaze_drive::key::crate_scope_name(&list.key(member));
            let unit = u32::try_from(units.len()).unwrap_or(u32::MAX);
            units.push(crate::engine_body_plan::Unit { name: own_name, prefix: list.key(member).prefix(), test: is_test(own_name) });
            member_cells.push(CellId { of: own_name, cell: WHOLE });
            let own_cells = cells_of(&[own_name]);
            let mut refuse_all = |why: &'static str, reads: &'f [CellId]| member_files.extend(reach.iter().map(|at| FileClaim { at: *at, unit, is: FileIs::Refused(why), reads }));
            let Some(own) = input_at(member) else {
                // Its crate's own names failed (a build script, its resolution): that failure
                // is the crate's `CrateScope` row, reported there once; its bodies say so. Read
                // as it is today: no task defines the crate's names cell (ABSENT), or its task
                // failed and defines none; the task that comes, or its fix, moves it.
                refuse_all(
                    if names.failed(own_name) {
                        "its crate has no names record this command: its `CrateScope` task failed, and that failure is reported at it"
                    } else {
                        "its crate has no `Names` task this command (engine_names.rs `plan`)"
                    },
                    own_cells,
                );
                continue;
            };
            members.push(own.name);
            let (scope, whole) = match scope_of(read, cx, store, names, &list, member, heap) {
                Ok(made) => made,
                Err(cause) => {
                    // `scope_of` reads the crate's own names record and the check scope the
                    // graph makes (blaze-world check_scope.rs): both cells. The cause printed,
                    // typed, the crate named (rule 19).
                    refuse_all(crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's check scope was not read: {cause}")), own_cells);
                    continue;
                }
            };
            let own_records = scope.own_records;
            let cut_words = scope.cut_cause(&list).map(|cause| crate::engine_head_notes::leak(heap.alloc(), format_args!("its member's {cause}")));
            // The member's environment, its script run's record handed by the Scripts stage.
            let env = crate::body_types::owner_env_by(cx, store, &|script| names.input_of(script), &list, member, handles.env.home, frame)?;
            let (env, script, env_refused) = match env {
                Ok((env, script)) => (env, script.map(|(name, _)| name), None),
                Err(why) => (None, None, Some(why)),
            };
            // The cells a cut scope's refusal reads: the graph's, each scope crate's that had
            // records and the first that had none (the one whose task comes or is fixed).
            let mut cut_reads: Vec<Name, Global> = Vec::new_in(Global);
            cut_reads.push(own_name);
            cut_reads.extend(scope.scope.iter().copied());
            cut_reads.extend(scope.cut_name(&list));
            let scope_cells = cells_of(&cut_reads);
            let mut buckets: Vec<(Name, Name), blaze_own::Alloc<'static>> = Vec::with_capacity_in(scope.crates.len(), g);
            buckets.extend(scope.crates.iter().map(|one| {
                let key = list.key(one.world_at);
                (blaze_ladder::front2::buckets::impl_tables_name(key), blaze_drive::key::crate_scope_name(&key))
            }));
            buckets.sort_unstable_by_key(|(tables, _)| *tables);
            let buckets: &'f [(Name, Name)] = &*buckets.leak();
            let group = Group { own: own.name, crates: scope.crates, env, script, lang: scope.lang, scope: scope.scope, buckets };
            // The member's files: its items record's file rows, matched to the planner's outlines.
            let Ok(items) = ItemsRecord::open(own_records.items, g) else {
                // The items record is the crate's names task's: a function of its cell.
                refuse_all("its crate's items record does not open (blaze-scope items_record.rs `ItemsRecord::open`)", own_cells);
                continue;
            };
            let mut files: Vec<(u32, u32), Global> = Vec::new_in(Global);
            for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                let Ok(Some(row)) = items.file(file) else { continue };
                let Some(path) = row.path.in_package(list.shown_from(member), g) else { continue };
                let Some(path) = path.strip_prefix("/0/") else { continue };
                if let Some(at) = handles.outlined.iter().position(|one| one.path.as_slice() == path.as_bytes()) {
                    files.push((file, u32::try_from(at).unwrap_or(u32::MAX)));
                    heads.push((list.key(member).prefix(), file, handles.outlined[at].name));
                }
            }
            // A file the planner's cfg-blind walk reaches that the items record lists at no path
            // of it: this unit's cfg cut its `mod` (engine_body_plan.rs). Not this unit's file,
            // never a refusal: another unit may hold it, or none does and the file says so once.
            for at in reach.iter().filter(|at| !files.iter().any(|(_, outline)| outline == *at)) {
                member_files.push(FileClaim { at: *at, unit, is: FileIs::NotHeld, reads: own_cells });
            }
            let at = u32::try_from(groups.len()).unwrap_or(u32::MAX);
            // A member whose environment could not be made is refused by name, never checked
            // under none (rule 3).
            // The note names the crate that failed (rule 19), once per member, never per body.
            // The cut scope reads the cells it was cut at ([`Scope::cut`]). A refused environment
            // (body_types.rs `owner_env_by`) is a function of the crate list (the graph's cell:
            // the script row, the package version) and of the member's build-script run: that
            // `Script` task's `WHOLE` cell, read ABSENT when its run failed and defined none, or
            // with the digest of the record it refused; its fix or a changed record moves it.
            let empty: &'f [CellId] = &[];
            let refused = match (whole, env_refused) {
                (false, _) => Some((
                    // `scope_of` returns `false` exactly when it cuts the scope, the cause with it.
                    cut_words.unwrap_or("its member's check scope was cut and `scope_of` kept no cut (engine_bodies.rs `scope_of`)"),
                    scope_cells,
                )),
                (true, Some(why)) => {
                    let mut env_reads: Vec<Name, Global> = Vec::new_in(Global);
                    env_reads.push(own_name);
                    env_reads.extend(list.script(member).and_then(crate::native_image::script_name));
                    Some((why, cells_of(&env_reads)))
                }
                (true, None) => None,
            };
            // Each body's item key, through the member's records (opened here once).
            let opened = crate::run_names::names_records(cx, handles.root, &group.crates, packs, corrected, procs, std::vec::Vec::new_in(Global), env_in(group.env, frame).ok().flatten(), frame, g);
            let (records, refused) = match (refused, opened) {
                (Some(why), _) => (None, Some(why)),
                (None, Ok(opened)) => (Some(opened.records), None),
                // The scope crates' records, each its names task's: a function of their cells.
                (None, Err(_)) => (None, Some(("its member's items record does not open for its edition (run_names.rs `names_records`)", scope_cells))),
            };
            let expansions = group.crates.first().map(|own| own.records.expansions);
            let index = records.map(|records| {
                let listed = list_with(records, Listing::FnsAndInitializers, g);
                let mut mine = blaze_own::AVec::new_in(g);
                mine.extend(listed.iter().copied().filter(|l| l.entry.is_some_and(|(f, _)| files.iter().any(|(file, _)| *file == f))));
                // The bodies a macro wrote at module level, each at the entry its call is at
                // (D122; body_types/covered.rs `written_by`, the one walk `check` takes), and
                // the bodies of blocks' modules, each with the body whose block it is
                // (covered.rs `in_blocks`: plan row A6).
                let (written, blocks) = match &expansions {
                    Some(texts) => (crate::body_types::covered::written_by(records, texts, &listed, g), crate::body_types::covered::in_blocks(records, texts, &listed, g)),
                    None => (blaze_own::AVec::new_in(g), Ok(blaze_own::AVec::new_in(g))),
                };
                // The unit's cfg, as the names library lowered the crate (body_types.rs
                // `Checker::holds`): what tells an entry a `cfg` left out from one the records
                // lost ([`absent_entry`]).
                let cfg = NamesSource::cfg_atoms(records, 0).map(|atoms| blaze_scope::task::Cfg::from_atoms(atoms, g));
                (records, index_by_chain(records, &mine, &blaze_file_items::item_chain, g), written, blocks, cfg)
            });
            // The bodies a macro wrote that no entry covers: neither at a file entry nor in a
            // block's module: each NAMED in the request's lines (`plan_lines`), never dropped in
            // silence (rule 3), and counted in the profile.
            if let Some((records, _, written, Ok(blocks), _)) = index.as_ref() {
                for w in written.iter().filter(|w| w.top.is_err()) {
                    let key = crate::body_types::covered::body_key(records, w.listed.body);
                    if !key.is_some_and(|key| blocks.iter().any(|one| one.key == key)) {
                        uncovered = uncovered.saturating_add(1);
                        if uncovered_named.len() < MOST_UNCOVERED_NAMED {
                            uncovered_named.push(crate::engine_head_notes::leak(g, format_args!("{}", crate::body_types::covered::written_label(records, w))));
                        }
                    }
                }
            }
            // The member's crate root, by its path: its top-level `fn main` is D104's
            // (body_types/covered.rs `root_of`, `Root::Replaced`).
            let root_path = list.root(member).strip_prefix("/0/").unwrap_or(list.root(member));
            for &(file, outline_at) in files.iter() {
                let file_name = handles.outlined[outline_at as usize].name;
                let root_file = handles.outlined[outline_at as usize].path.as_slice() == root_path.as_bytes();
                let Some((records, chains, written, blocks, cfg)) = index.as_ref() else {
                    // Its file is the member's; its scope could not be read: every body says why.
                    let (why, reads) = refused.unwrap_or((RULE, empty));
                    member_files.push(FileClaim { at: outline_at, unit, is: FileIs::Refused(why), reads });
                    continue;
                };
                let records = *records;
                // A body of its file the records do not list, or list without a key, is refused
                // on the member's names cell: the listing is its record's.
                member_files.push(FileClaim { at: outline_at, unit, is: FileIs::Held, reads: own_cells });
                let read = records.with_outline(file, g, &mut |text, outline| {
                    for (entry, item) in outline.items().iter().enumerate() {
                        let body = work::checked_body(outline, item).is_some();
                        if !body && !work::expansion_entry(outline, item, text) {
                            continue;
                        }
                        let name = work::body_name(file_name, outline, item, text);
                        // The bodies an expansion at this entry wrote: the entry's task checks
                        // each (one task an entry, planned from text; covered.rs).
                        let top = Ok((file, blaze_surface::ItemIndex(u32::try_from(entry).unwrap_or(u32::MAX))));
                        let by = written.iter().find(|w| w.top == top).map(|w| w.by);
                        // One a body: in the request's heap with its list, never the fixed frame.
                        let shown = crate::item_name::shown_name(item, text);
                        let label = match (shown.is_empty(), by) {
                            (false, _) => bstd::mem::format_in!(g, "{shown}"),
                            (true, Some(by)) => bstd::mem::format_in!(g, "{{the expansion of `{by}!`}}"),
                            (true, None) => bstd::mem::format_in!(g, "{{a macro call}}"),
                        };
                        let label: &'f str = core::str::from_utf8(label.into_bytes().leak()).unwrap_or("");
                        // The entry's bodies, each with where its findings are printed: the
                        // entry's own (`None`: from the item's start), or each body an expansion
                        // at it wrote, by its label as `check` names it (covered.rs
                        // `written_label`); then each body of a block's module whose owner is one
                        // of them (covered.rs `in_blocks`), by its label. Each body is in one
                        // entry's list alone: an entry's own key is listed at its file entry, a
                        // written body's `top` is one entry, a block's body has one owner, and
                        // the three are apart (a listed body with an entry is no macro's, and a
                        // walk that ends in a file is no block's): so checked once (rule 77).
                        let mut keys: Vec<ItemKey, _> = Vec::new_in(g);
                        let mut labels: Vec<Option<Unplaced<'f>>, _> = Vec::new_in(g);
                        // COVERAGE (body_types/covered.rs, the reference): every body of the entry
                        // is in its task's list, or the entry is refused by name; an entry is left
                        // to no unit (`ENTRY_ABSENT`) only when its `cfg` is false for this unit.
                        let own = match chains {
                            Err(_) => Err("its file's bodies could not be indexed by their outline entries (front2 project_records.rs `index_by_chain`)"),
                            Ok(chains) => 'own: {
                                // 1. The entry's own body, listed at its chain.
                                let listed = if body { chains.get(file, blaze_file_items::item_chain(outline, item, text)) } else { None };
                                if let Some(listed) = listed {
                                    match crate::body_types::covered::body_key(records, listed) {
                                        Some(key) => {
                                            keys.push(key);
                                            labels.push(None);
                                        }
                                        None => break 'own Err("a member the records list has no key of its own"),
                                    }
                                }
                                // 2. Each body an expansion at the entry wrote: an expansion
                                // entry's, and an attribute's at a body entry (its output kept or
                                // replaced the item: D122, the entry's task checks them).
                                for w in written.iter().filter(|w| w.top == top) {
                                    match crate::body_types::covered::body_key(records, w.listed.body) {
                                        Some(key) => {
                                            keys.push(key);
                                            let shown = bstd::mem::format_in!(g, "{}", crate::body_types::covered::written_label(records, w));
                                            let shown: &'f str = core::str::from_utf8(shown.into_bytes().leak()).unwrap_or("");
                                            labels.push(Some(Unplaced { label: shown, words: crate::body_types::covered::WRITTEN_UNPLACED }));
                                        }
                                        None => break 'own Err("a member a macro wrote has no key of its own in its crate's records"),
                                    }
                                }
                                // 3. A body entry the records list no body at, and no macro wrote
                                // one at: the root's replaced `fn main`, a `cfg` that is false
                                // (not this unit's), or a refusal naming why (never silent).
                                if body && keys.is_empty() {
                                    match absent_entry(records, outline, item, text, root_file, cfg.as_ref(), g) {
                                        Ok(key) => {
                                            keys.push(key);
                                            labels.push(Some(Unplaced { label: REPLACED_MAIN, words: crate::body_types::covered::WRITTEN_UNPLACED }));
                                        }
                                        Err(why) => break 'own Err(why),
                                    }
                                }
                                Ok(())
                            }
                        };
                        let found = match (own, blocks) {
                            (Err(why), _) => Err(why),
                            // Its blocks' bodies could not be told apart: refused by name, never
                            // checked without them (rule 3).
                            (Ok(()), Err(why)) if !keys.is_empty() => Err(*why),
                            (Ok(()), _) => {
                                if let Ok(blocks) = blocks {
                                    let owners = keys.len();
                                    for at_owner in 0..owners {
                                        let owner = keys[at_owner];
                                        let from = blocks.partition_point(|b| b.owner < owner);
                                        for b in blocks[from..].iter().take_while(|b| b.owner == owner) {
                                            let shown = bstd::mem::format_in!(g, "{}", BlockLabel { records, entry: label, body: b.listed.body });
                                            let shown: &'f str = core::str::from_utf8(shown.into_bytes().leak()).unwrap_or("");
                                            keys.push(b.key);
                                            labels.push(Some(Unplaced { label: shown, words: crate::body_types::covered::BLOCK_UNPLACED }));
                                        }
                                    }
                                }
                                Ok((at, &*keys.leak()))
                            }
                        };
                        let place = Place { chain: work::item_chain(outline, item, text), file, item: item.range, written: &*labels.leak() };
                        if grown.is_ok() {
                            grown = EngineCheckError::push(&mut took, Took { name, label, item: found, place, unit }, "the bodies found in their crates' records (engine_bodies.rs `groups`)");
                        }
                    }
                });
                core::mem::replace(&mut grown, Ok(()))?;
                if !read && let Some(last) = member_files.last_mut() {
                    // The file's text as it is now, through its crate's records (front2
                    // project_records.rs `with_outline`: `file_now`, the crate's edition): a
                    // function of the file's outline cell (`WHOLE`, which its text moves), the
                    // member's names cell (the records) and the graph's (the packs): the file
                    // edited, or the records made again, moves it.
                    last.is = FileIs::Refused("its file's text could not be read through its crate's records");
                    last.reads = cells_of(&[own_name, file_name]);
                }
            }
            groups.push(group);
        }
        // Who checks each body: the first unit in rank whose records hold it, once; what no unit
        // holds is no unit's (engine_body_plan.rs; was: the first member's find by name, a
        // refusal or not, and the first member's word on a file it does not hold).
        let claims = took.iter().enumerate().map(|(at, one)| BodyClaim {
            name: one.name,
            unit: one.unit,
            is: match one.item {
                Ok(_) => BodyIs::Held,
                Err(why) => BodyIs::of_refusal(why),
            },
            took: u32::try_from(at).unwrap_or(u32::MAX),
        });
        let holders = crate::engine_body_plan::Holders::new(units, member_files, claims);
        heads.sort_unstable();
        heads.dedup();
        member_cells.sort_unstable();
        member_cells.dedup();
        self.uncovered_note = (uncovered > 0).then(|| crate::engine_head_notes::leak(g, format_args!("{}", Uncovered { count: uncovered, named: &uncovered_named })));
        self.groups_found = true;
        Ok(Ok(Found { groups, took, holders, members, member_cells, heads, uncovered, trees: (packs, corrected) }))
    }

    /// Body `name`'s block of the request's lines (engine_lines.rs): each finding of its verdict
    /// as `check` files it, placed in its entry's item by the item's chain in its file (a
    /// placed finding: printed, counted and stored as an outline's or a names task's findings
    /// are), then its note when it has one. Nothing when it was not entered. A finding whose
    /// file or edition is not known is a typed stop naming the body, never dropped (rule 3).
    pub(crate) fn lines(&self, name: Name, outlined: &[Outlined<'_>], lines: &mut crate::engine_lines::Lines<'_>, frame: &Frame) -> Result<(), EngineCheckError> {
        let Ok(at) = self.ran.binary_search_by_key(&name, |one| one.name) else { return Ok(()) };
        let one = &self.ran[at];
        if one.found.len != 0 {
            let file = outlined.get(one.file as usize).ok_or(EngineCheckError::Unplaced { body: name, what: "its file is none of the planner's outlines" })?;
            let edition = blaze_surface::Edition::from_byte(file.edition).ok_or(EngineCheckError::Unplaced { body: name, what: "its file's edition byte is no edition" })?;
            let start = one.found.start as usize;
            let findings = self.findings.as_deref().and_then(|all| all.get(start..start + one.found.len as usize))
                .ok_or(EngineCheckError::Unplaced { body: name, what: "its finding range is outside the Bodies stage's list" })?;
            for found in findings {
                match found.at {
                    FoundAt::Start => lines.placed(name, found.rule, 0, 0, &file.path, one.chain, edition, &found.message),
                    // Printed at the tokens' bytes in the item as it is when printed (print.rs).
                    FoundAt::Tokens(at) => lines.placed_tokens(name, found.rule, at, &file.path, one.chain, edition, &found.message),
                }
            }
        }
        if let Some(words) = self.note(name, outlined, frame) {
            let class = match one.said() {
                Some(Said::BorrowsUnchecked(_)) => crate::engine_lines::Class::Borrows,
                _ if self.not_checked(name) => crate::engine_lines::Class::Unchecked,
                _ => crate::engine_lines::Class::Failure,
            };
            lines.note(name, class, words.as_bytes());
        }
        if let Some(first) = one.note.and_then(|note| note.head_failed) {
            lines.note(name, crate::engine_lines::Class::Head, crate::engine_head_notes::seed_words(first.head, first.why, frame).as_bytes());
        }
        Ok(())
    }

    /// The note of a body that has one: its refusal, or why it was not checked whole. Its
    /// findings are its lines' ([`BodiesStage::lines`]), not a note.
    pub(crate) fn note<'p>(&self, name: Name, outlined: &[Outlined<'_>], frame: &'p Frame) -> Option<bstd::mem::String<&'p Frame>> {
        let at = self.ran.binary_search_by_key(&name, |one| one.name).ok()?;
        let one = &self.ran[at];
        let path = outlined.get(one.file as usize).map_or("?", |file| core::str::from_utf8(&file.path).unwrap_or("?"));
        // Named always (rule 19): its item as written, or its place in its file's outline.
        let item = crate::engine_body_plan::Named::of(one.label, outlined, one.file, name);
        match one.said() {
            Some(Said::Task(error)) => Some(bstd::mem::format_in!(frame, "`{path}`: the body of `{item}` was not checked: {error} ({RULE})")),
            Some(Said::Panicked) => Some(bstd::mem::format_in!(frame, "`{path}`: the check of the body of `{item}` panicked; that body alone failed ({RULE})")),
            Some(Said::FrameUsedUp { used, reserved }) => Some(bstd::mem::format_in!(
                frame,
                "`{path}`: the check of the body of `{item}` used up its frame ({used} of {reserved} bytes reserved, engine_bodies.rs `BODIES_FRAME`); that body alone failed ({RULE})"
            )),
            Some(Said::Refused(why)) => Some(bstd::mem::format_in!(frame, "`{path}`: the body of `{item}` was not checked: {why} ({RULE})")),
            Some(Said::Unchecked("")) => Some(bstd::mem::format_in!(frame, "`{path}`: body `{item}`: not checked whole: its verdict failed and said no cause (front2 tables/task.rs `said_of`; {RULE})")),
            Some(Said::Unchecked(why)) => Some(bstd::mem::format_in!(frame, "`{path}`: body `{item}`: {why}")),
            Some(Said::BorrowsUnchecked(why)) => Some(bstd::mem::format_in!(frame, "`{path}`: body `{item}`: types checked; {why}")),
            None => None,
        }
    }

    /// Body `name`'s item as written, when it ran.
    pub(crate) fn label(&self, name: Name) -> Option<&'f str> {
        self.ran.binary_search_by_key(&name, |one| one.name).ok().map(|at| self.ran[at].label).filter(|label| !label.is_empty())
    }

    /// Whether body `name` was entered this request (else its row stands from the stored answer).
    pub(crate) fn entered(&self, name: Name) -> bool {
        self.ran.binary_search_by_key(&name, |one| one.name).is_ok()
    }

    /// Whether body `name` is an item the check could not check (refused by this stage or by
    /// the checker), which `check` counts apart from failures (main.rs `UNCHECKED_ITEMS`); a
    /// finding, a task error or a panic is a failure instead.
    pub(crate) fn not_checked(&self, name: Name) -> bool {
        self.ran.binary_search_by_key(&name, |one| one.name).is_ok_and(|at| matches!(self.ran[at].said(), Some(Said::Refused(_) | Said::Unchecked(_))))
    }

    /// Every body's note ([`BodiesStage::note`]) in name order, and how many bodies were
    /// checked with no finding and no note: what the command's tests read.
    pub(crate) fn notes(&self, outlined: &[Outlined<'_>], frame: &Frame) -> (std::vec::Vec<std::string::String, Global>, u32) {
        let mut notes = std::vec::Vec::new_in(Global);
        for one in self.ran.iter() {
            if let Some(words) = self.note(one.name, outlined, frame) {
                notes.push(std::string::String::from(words.as_str()));
            }
        }
        // A file no unit holds: one note each (engine_body_plan.rs).
        notes.extend(self.outside.notes(outlined));
        // A body in a file no member holds, or no unit's, has no note of its own and was not
        // checked (never by its time: a simulated clock gives a fast body 0 ns).
        (notes, self.clean.saturating_sub(self.ownerless).saturating_sub(self.outside.len()))
    }

    /// What the stage holds past its bodies' tasks, an upper bound: each body's row, its label's
    /// bytes, its findings (rows and messages) and its note (record and words), summed over the bodies
    /// entered (a label or words shared by many bodies is counted at each). Its floor is
    /// [`HELD_FLOOR`] a body (rule 67); the profile's `bodies:` line says both.
    pub(crate) fn held(&self) -> Held {
        let mut held = Held { bodies: self.ran.len() as u64, labels: 0, findings: 0, words: 0 };
        for one in self.ran.iter() {
            held.labels += one.label.len() as u64;
            if let Some(note) = one.note {
                let words = match &note.said {
                    Said::Task(words) | Said::Refused(words) | Said::Unchecked(words) | Said::BorrowsUnchecked(words) => words.len(),
                    Said::Panicked | Said::FrameUsedUp { .. } => 0,
                };
                let head = note.head_failed.map_or(0, |first| first.head.len() + first.why.len());
                held.words += (core::mem::size_of_val(note) + words + head) as u64;
            }
        }
        if let Some(findings) = &self.findings {
            for finding in findings {
                held.findings += (core::mem::size_of_val(finding) + finding.message.len()) as u64;
            }
        }
        held
    }

    /// The profile's lines: per body microseconds (sum, median, the 10 slowest with their
    /// paths), bodies per second of CPU, and the phase's wall against its CPU.
    pub(crate) fn profile(&self, outlined: &[Outlined<'_>], workers: NonZeroU32, into: &mut blaze_own::String<&Frame>) {
        use core::fmt::Write as _;
        let mut checked: std::vec::Vec<(u64, Name, u32), Global> = std::vec::Vec::new_in(Global);
        checked.extend(self.ran.iter().filter(|one| one.note.is_none() && one.ns > 0).map(|one| (one.ns, one.name, one.file)));
        checked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let sum: u64 = checked.iter().map(|one| one.0).sum();
        let median = checked.get(checked.len() / 2).map_or(0, |one| one.0);
        let per_cpu_s = if sum == 0 { 0 } else { (checked.len() as u64).saturating_mul(1_000_000_000) / sum };
        let parallel = if self.wall_ns == 0 { 0.0 } else { sum as f64 / self.wall_ns as f64 };
        let _ = writeln!(into, "\nbodies: {} entered, {} checked, {} refused, {} in files no crate holds; {} run(s) on {} worker(s)", self.ran.len(), checked.len(), self.ran.iter().filter(|one| one.note.is_some()).count(), self.ownerless, self.runs, workers);
        for path in self.ownerless_paths(outlined).iter() {
            let _ = writeln!(into, "bodies: in no member's module tree: {path}");
        }
        // What outlives the bodies' tasks (rule 67: the floor and the ratio), and the most one
        // body's scratch held (reset as each body ends).
        let held = self.held();
        let _ = writeln!(
            into,
            "bodies: held per body {} bytes (floor {HELD_FLOOR}, {:.1}x) over {} bodies; row {}, label {}, findings {}, words {}; a body's scratch at most {} bytes",
            held.per_body(),
            held.per_body() as f64 / HELD_FLOOR as f64,
            held.bodies,
            Held::ROW,
            held.mean(held.labels),
            held.mean(held.findings),
            held.mean(held.words),
            self.scratch_most
        );
        // What the plan left to no unit (engine_body_plan.rs): entries, and files, counted.
        self.outside.profile(self.ran.len(), outlined, into);
        // Each item body is in one task's list alone (engine_bodies.rs `groups`), so this sum
        // counts each item body checked this request once (rule 77).
        let items = self.items;
        let tasks = self.item_tasks;
        let _ = writeln!(into, "bodies: {items} item bodies checked in {tasks} task(s); {} body(ies) a macro wrote that no entry covers", self.uncovered);
        let _ = writeln!(into, "bodies: us a body: sum {}, median {}; {} bodies a CPU second; phase wall {} us against {} us of body CPU ({parallel:.1}x)", sum / 1000, median / 1000, per_cpu_s, self.wall_ns / 1000, sum / 1000);
        // What the bodies' recorders were offered and kept (front2 tables/recorder.rs
        // `ReadTally`): a repeat is never pushed, so only `kept` reads are sorted.
        let reads = self.seeds.reads;
        let _ = writeln!(into, "bodies: reads offered {}, kept {}, compares sorting them {} (front2 tables/recorder.rs)", reads.offered, reads.kept, reads.sort_compares);
        let _ = writeln!(into, "bodies: expansion origin columns opened {} (once per expansion asked per stretch: front2 source.rs `Records::text_marks`)", self.seeds.origins_opened);
        for (ns, _, file) in checked.iter().take(10) {
            let path = outlined.get(*file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
            let _ = writeln!(into, "bodies: slow  {:>9} us  {path}", ns / 1000);
        }
        // The most one body grew its stretch's frame by, and which body (the frame's cap is
        // `BODIES_FRAME`; a stretch ends past `STRETCH_FULL`).
        if let Some(most) = self.ran.iter().max_by(|a, b| a.bytes.cmp(&b.bytes).then(b.name.cmp(&a.name))).filter(|one| one.bytes > 0) {
            let path = outlined.get(most.file as usize).map_or("?", |one| core::str::from_utf8(&one.path).unwrap_or("?"));
            let used_up = self.ran.iter().filter(|one| matches!(one.said(), Some(Said::FrameUsedUp { .. }))).count();
            let _ = writeln!(into, "bodies: largest frame a body used: {} bytes, `{}` in {path} (of {BODIES_FRAME} reserved a stretch, a stretch ends past {STRETCH_FULL}); {used_up} body(ies) used one up", most.bytes, most.label);
        }
        // What the bodies left in their stretches' frames, summed (the guard's line).
        crate::engine_memory::BodyFrames::of(self.ran.iter().map(|one| (one.bytes, one.stretch, one.label))).line(into);
    }
}

/// What every run reads: all read-only and shared.
struct Shared<'s, 'a, 'f, 'r> {
    /// The request's listings of each crate's heads, made once a crate (engine_heads.rs
    /// `Listings`, lane OW1): every stretch's `Owners` places its reads in them.
    listings: &'s crate::engine_heads::Listings,
    root: &'s Dir<ReadWrite>,
    packs: &'s [blaze_fetch::PackView<'static>],
    corrected: &'s [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    groups: &'s [Group<'f, 'r>],
    heads: &'s crate::engine_heads::Subjects,
    view: View<'s, 'a>,
    heap: &'static blaze_own::Heap,
    /// Every crate's impl tables (the Heads stage's), read only.
    shelf: &'s ImplShelf,
    /// The request's frozen heads (engine_seeds.rs), read only but for its insert-only maps;
    /// `None`: each body lowers its heads in its own store (`HeadsFrom::Lowered`).
    seeds: Option<&'s crate::engine_seeds::RequestTable>,
    /// Every head not frozen (engine_head_notes.rs).
    heads_failed: &'s crate::engine_head_notes::Failed,
    /// How a body's names reads are kept (engine_scope_reads.rs).
    scope_reads: crate::engine_scope_reads::ScopeReadsAs,
    board: &'s crate::engine_status::Board<'s>,
    /// PLANTED BUG when true, tests only ([`PLANT_KEEP_SCRATCH`]): a body's scratch is not
    /// reset when it ends.
    keep_scratch: bool,
    /// PLANTED BUGS, tests only ([`RowPlants`]): the old row's capacity, or the coarse step off.
    plants: RowPlants,
}

/// A gauge that reads nothing: a body's [`Cost`] counts its rounds alone here (the profile's
/// spans time the bodies).
struct NoGauge;

impl Gauge for NoGauge {
    fn now_ns(&self) -> u64 {
        0
    }

    fn bytes(&self) -> u64 {
        0
    }
}

static NO_GAUGE: NoGauge = NoGauge;

/// What a seed's freeze error says of the head a body needed ([`unmade_head`]).
enum Unmade {
    /// The head of this cell could not be made: the body is not checked, its note naming it.
    Head(Want),
    /// A head could not be made and the error names no cell of an item (an anonymous
    /// constant's key): not checked, its note the error.
    Unnamed,
    /// The engine's own inconsistency, not the head's: the body's failure.
    Engine,
}

/// The head a seed's freeze could not make, by the error's kind. Every variant is named (no
/// `_` arm), so a new one is classed here when it is added (rule 76).
fn unmade_cell(error: &FreezeError) -> Unmade {
    match error {
        FreezeError::Fill { want, .. }
        | FreezeError::Head { want, .. }
        | FreezeError::Const { want, .. }
        | FreezeError::Record { want, .. }
        | FreezeError::TooBig { want, .. }
        | FreezeError::NoText { want }
        | FreezeError::Steps { want, .. } => Unmade::Head(*want),
        // The body read this cell. Its retained direct need may lead to a different root.
        FreezeError::Again { want, .. } => Unmade::Head(*want),
        // A member's head not frozen for this body's unit (engine_seeds.rs `StretchHeads`):
        // seeded as `SeedError::NotInItsUnit`/`NoMemberCell`, here only as its kind.
        FreezeError::NotTheirs { want, .. } => Unmade::Head(*want),
        FreezeError::TooDeep { outermost, .. } => Unmade::Head(*outermost),
        FreezeError::NoWant { .. } => Unmade::Unnamed,
        // A cell asked that is no cell, bytes that do not read, or the driver's own signal
        // escaping it: faults of the engine, never of the project's head.
        FreezeError::NotACell { .. } | FreezeError::Read { .. } | FreezeError::Unfrozen { .. } => Unmade::Engine,
    }
}

/// Explain an ask using the same root for its words and its counter. Direct errors from
/// outside the freezer (for example, a failed import) are still classed by the caller.
pub(crate) fn unmade_head<E>(
    asked: Want,
    step: impl FnMut(Want) -> Option<Step>,
    label: impl Fn(Want) -> &'static str,
    tally: &mut crate::engine_seeds::SeedTally,
    scratch: impl core::alloc::Allocator + Copy,
    g: blaze_own::Alloc<'static>,
) -> Said<E> {
    let cause = root_cause(asked, step, &label, scratch, g);
    if cause.kind == RootKind::Constant {
        tally.refused_const = tally.refused_const.saturating_add(1);
    }
    let words = leak(g, format_args!("not checked because the head of `{}` it reads could not be frozen: {}", label(asked), cause.words));
    match cause.kind {
        RootKind::Engine => Said::Refused(words),
        RootKind::Head | RootKind::Constant => Said::Unchecked(words),
    }
}

/// A root in the stretch's freezer. Its first publication has the words used to group
/// dependency heads across workers. Even past the publication cap the source can explain it.
fn seed_cause<'n>(
    cell: Want,
    failure: CellFailure<'_>,
    table: Option<&crate::engine_seeds::RequestTable>,
    records: &'n blaze_ladder::front2::source::Records<'n>,
    grouped: &mut Option<crate::engine_seeds::FirstFailure>,
    g: blaze_own::Alloc<'static>,
) -> Step {
    use crate::engine_seeds::FirstFailed;
    let CellFailure::Own(error) = failure else { return Step::retained(cell, failure, records, g) };
    let cause = match table.map(|table| table.first_failure(cell)) {
        Some(FirstFailed::Kept(first)) => {
            let cause = Cause::from_reason(error, first.why, records, g);
            if cause.kind != RootKind::Engine {
                *grouped = Some(first);
            }
            cause
        }
        Some(FirstFailed::PastCap { kept }) => {
            let mut cause = Cause::own(error, records, g);
            cause.words = leak(g, format_args!("{} (the request's table of {kept} failed heads is full)", cause.words));
            cause
        }
        Some(FirstFailed::None) | None => {
            let mut cause = Cause::own(error, records, g);
            cause.words = leak(g, format_args!("{} (the request has no entry for the stretch's failed head)", cause.words));
            cause
        }
    };
    Step::End(cell, cause)
}

/// A body's cells as the engine's view holds them today: every cell a body reads is a cell an
/// earlier phase defined or none does ([`ABSENT`]); nothing is made inline on this path.
struct ViewMake<'s, 'a> {
    view: View<'s, 'a>,
}

impl<'a> Make<'a> for ViewMake<'_, 'a> {
    fn make(&self, want: &CellId, _made: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        Ok(self.view.cell(want))
    }
}

/// A stretch's frame past this many bytes takes no further body: the next body opens its
/// records into a new frame. A body's local items' rows live in the frame until the records go
/// (`check_body` borrows them for the records' lifetime; no question copies into it any more:
/// front2 source.rs `Records::walk`, `VariantRows`), so a frame is reset by ending its stretch, never per body; half the reserve leaves the next body the
/// other half (tempo, g124650: three 32-body runs used up the whole 1 GiB in one frame).
const STRETCH_FULL: usize = BODIES_FRAME / 2;

/// One run: its bodies checked in stretches, each over a frame of its own that ends with it
/// ([`STRETCH_FULL`]), so no run uses up a frame however many bodies it holds.
#[expect(clippy::too_many_arguments, reason = "a run's edge: each argument one handle of it")]
fn run_bodies<'a, 'f>(cx: &Cx, shared: &Shared<'_, 'a, '_, '_>, run: &[One<'f>], clock: &Clock, out: &mut Buffer<'a>, ran: &mut SVec<'a, Ran<'f>>, run_out: &mut RunOut<'a>, tally: &mut crate::engine_seeds::SeedTally, alloc: SharedAlloc<'a>) {
    // The bodies' scratch: one reservation a run, reset as each body ends (rule 11).
    // (Its own tag, as the stretch's below: the status's `memory by tag` line names each.)
    let Ok(mut scratch) = Frame::with_reserve(BODY_SCRATCH, bstd::mem::Tag::Diagnostics) else {
        refuse(run, "the scratch frame of its run's bodies could not be reserved (engine_bodies.rs `BODY_SCRATCH`)", out, ran, run_out, alloc);
        return;
    };
    let mut at = 0;
    while at < run.len() {
        let rest = &run[at..];
        // Stopped (engine_status.rs): the bodies not begun are refused, each named, and the
        // driver stops before the merge.
        if shared.board.stopping().is_some() {
            shared.board.refused(Stage::Bodies, rest.len() as u64);
            refuse(rest, "not checked: the command was stopped by a signal before this body began (engine_status.rs)", out, ran, run_out, alloc);
            return;
        }
        let took = match Frame::with_reserve(BODIES_FRAME, bstd::mem::Tag::Infer) {
            Ok(stretch) => run_stretch(cx, shared, rest, &stretch, &mut scratch, clock, out, ran, run_out, tally, alloc),
            Err(_) => refuse(rest, "the frame of its run's stretch could not be reserved (engine_bodies.rs `BODIES_FRAME`)", out, ran, run_out, alloc),
        };
        at += took.max(1);
    }
}

/// Every body of `rest` refused for one reason, each named (rule 3): how many it took (all).
fn refuse<'a, 'f>(rest: &[One<'f>], why: &'static str, out: &mut Buffer<'a>, ran: &mut SVec<'a, Ran<'f>>, run_out: &mut RunOut<'a>, alloc: SharedAlloc<'a>) -> usize {
    for one in rest {
        // A machine's refusal (a frame not reserved, records not opened in the run): a function
        // of no cell, so no read; owed (empty reads), entered again by the next request.
        out.done.push(crate::engine_tasks::failed(&one.task, alloc));
        run_out.notes.push((ran.len(), Said::Refused(why), None));
        ran.push(Ran::without_check(one.task.planned.name, one.label, one.file));
    }
    rest.len()
}

/// One stretch of a run: its member's records opened once into `frame`, each body checked over
/// one view until the frame is past [`STRETCH_FULL`]. How many bodies it took (at least one).
#[expect(clippy::too_many_arguments, reason = "a stretch's edge: each argument one handle of it")]
fn run_stretch<'a, 'f>(
    cx: &Cx,
    shared: &Shared<'_, 'a, '_, '_>,
    run: &[One<'f>],
    frame: &Frame,
    scratch: &mut Frame,
    clock: &Clock,
    out: &mut Buffer<'a>,
    ran: &mut SVec<'a, Ran<'f>>,
    run_out: &mut RunOut<'a>,
    tally: &mut crate::engine_seeds::SeedTally,
    alloc: SharedAlloc<'a>,
) -> usize {
    let Some(first) = run.first() else { return 0 };
    let Some(group) = shared.groups.get(first.group as usize) else {
        return refuse(run, "a body's member group is gone", out, ran, run_out, alloc);
    };
    let g = shared.heap.alloc();
    // The stretch's frame as an allocator: what is placed in the frame (which runs no drop)
    // allocates its lists in the frame too, so they end with the stretch's reset. Before, the
    // impl table's notes, the evaluator's sink and the constants' lists were placed in the frame
    // but grew on the process heap (`g`), never freed until the request ended: the Bodies
    // stage's growth on a 700-crate workspace (engine_bodies.rs memory table, lane BD1).
    let fa = frame.alloc(blaze_own::Erased::new(frame)).handle();
    // The impls from the request's shelf: each impl question reads its crate's bucket cell
    // (front2 tables/task.rs `cell_id`), which the Heads stage defined. Held in the stretch's
    // frame, so it lives as long as the records it is read beside (`Tables` takes both for one
    // lifetime); its notes in the frame too (`fa`).
    let impls: &TableSet<'_> = frame.alloc(TableSet::lent(shared.shelf, fa));
    let Ok(env) = env_in(group.env, frame) else {
        return refuse(run, "its member's environment did not copy into the run (blaze-scope crate_env.rs `EnvLent::copy_in`)", out, ran, run_out, alloc);
    };
    let Ok(opened) = crate::run_names::names_records(cx, shared.root, &group.crates, shared.packs, shared.corrected, shared.procs, std::vec::Vec::new_in(Global), env, frame, g) else {
        return refuse(run, "its member's records could not be opened in the run", out, ran, run_out, alloc);
    };
    let unused = Recorder::new(RecorderOpts { alloc: g });
    let tables = Tables::new(TablesOpts { records: opened.records, impls: Some(impls), lang_crates: &group.lang, reads: &unused });
    let make = ViewMake { view: shared.view };
    // `First`: under `All` clap-root's `Cmd` was refused with a false cycle among core's
    // iterator traits (front2 heads.rs `Store::new`; lane FL).
    // The request's frozen heads (engine_seeds.rs): each head a body needs imported from them,
    // or frozen here on its first read and published; none lowered in the body's store. Both
    // in the stretch's frame, for the records' lifetime the body's store takes them for (the
    // frame runs no drop: the freezer's lists are in the frame too, through `fa`). A want is
    // placed by this stretch's names the one way `widen` places a read (engine_seeds.rs
    // `StretchHeads`), on the stack so its `Owners` is dropped with the stretch.
    let stretch_heads = shared.seeds.map(|table| crate::engine_seeds::StretchHeads::new(table, shared.heads_failed, opened.records, shared.heads, &group.crates[..], shared.listings, g));
    // The seed freezer's constant evaluator: the head tasks' own (engine_head_consts.rs), its
    // lang index built by the one function a head task builds it by, once a stretch, and what
    // it reads kept as it grows (`EvalSink`), so each body's read list takes it ([`add_eval_reads`]).
    // Two crates declaring one lang item refuse the stretch by name, as they fail a head task.
    let evaluated: &crate::engine_head_consts::EvalSink<'_> = frame.alloc(crate::engine_head_consts::EvalSink::new(fa));
    let index: Option<&blaze_ladder::front2::index::Index<'_>> = match shared.seeds {
        Some(_) => match crate::engine_head_consts::lang_index(tables, evaluated, &group.lang) {
            Ok(index) => Some(&*frame.alloc(index)),
            Err(twice) => return refuse(run, crate::engine_head_notes::leak(g, format_args!("its scope's constants have no lang index: {twice} (front2 index.rs `LangTwice`)")), out, ran, run_out, alloc),
        },
        None => None,
    };
    // The reads of building the index alone: a body takes the evaluator's reads only once it
    // evaluated a constant (as a head task takes them when its evaluator was asked).
    let index_reads = evaluated.len();
    let seed: Option<&RequestSeed<'_, '_, '_, _, _>> = match &stretch_heads {
        Some(table) => {
            let mut sized = None;
            for at in group.lang.iter() {
                NamesSource::each_lang(opened.records, *at, &mut |name, key| {
                    if name == "sized" {
                        sized = Some(key);
                    }
                });
            }
            let consts = index.map(|index| frame.alloc(crate::engine_head_consts::HeadConsts::new(tables.reading_into(evaluated), index, fa)) as &mut (dyn ConstValues<'_> + '_));
            // The program's receiver pointer types, as the Heads stage's freezes are made with
            // (engine_heads.rs `heads_of`): without them this freezer would write other bytes
            // under a dependency head's key (rule 21; lane H9).
            let Some(index) = index else { return refuse(run, "its scope has no lang index for its receiver pointer types (engine_bodies.rs `run_stretch`)", out, ran, run_out, alloc) };
            let pointers = blaze_ladder::front2::receiver::Pointers::find(index, blaze_ladder::resolve::Paths { names: tables });
            let freezer = SeededHeads::new(SeededHeadsOpts { names: opened.records, sized, pointers, frame, scratch: g, alloc: fa, consts, known: Some(table), owners: None });
            Some(frame.alloc(RequestSeed::new(RequestSeedOpts { names: opened.records, heads: table, freezer, body_locals: table.body_locals() })))
        }
        None => None,
    };
    // Each body's rounds (front2 tables/cost.rs `Spent::rounds`): one when its heads are all
    // seeded (runtime.md point 3).
    let cost: &Cost<'_> = frame.alloc(Cost::new(&NO_GAUGE));
    let body_frame = BodyFrame { frame, alloc: g, plant_drop: None, cost: Some(cost), needs: NeedsMet::First, heads: seed.map(|s| s as &dyn HeadSeed<'_>), sources: Some(opened.sources), goals: None };
    // Each crate's listing from the request's table (engine_heads.rs `Listings`): made once a
    // request, not once a stretch that places a read in it.
    let mut owners = crate::engine_heads::Owners::new(shared.heads, &group.crates[..], shared.listings, g);
    let mut taken = 0usize;
    for one in run {
        if taken > 0 && frame.used() > STRETCH_FULL {
            break;
        }
        // The workers' scheduling point (engine_status.rs): stopped, the stretch ends, and
        // `run_bodies` refuses the rest.
        if taken > 0 && shared.board.stopping().is_some() {
            break;
        }
        taken += 1;
        // The body's scratch starts empty (rule 11): the last body's findings and words were
        // copied out as it ended. Kept only under the plant (the answers must not change).
        if !shared.keep_scratch {
            scratch.reset();
        }
        let sc: &Frame = &*scratch;
        // The scratch as the allocator the check works in (rule 11; lane BD3): its recorder,
        // its cells, its words, every list on the way to its row; the row alone is copied out
        // into the request's arena (`row_of`). Before, all of it was in that arena, and the
        // row kept the capacity of the list before widening.
        let erased = blaze_own::Erased::new(sc);
        let w = erased.handle();
        let mut mem = BodyMem { scope: group.crates.len() as u64, ..BodyMem::default() };
        // The guard (rule 54): the frame's use before and after each body, one load each; a
        // later body's growth is what a question kept past its body (engine_memory.rs
        // `BodyFrames`).
        let stretch = if taken == 1 { InStretch::First } else { InStretch::Later };
        let at_start = frame.used();
        let _body = cx.span("engine: one body");
        let began = clock.now();
        let running = shared.board.show(Stage::Bodies, crate::engine_status::Label::of(one.label), at_start as u64, shared.board.now(clock));
        let name = one.task.planned.name;
        // Each body the task checks (one; or each an expansion at its entry wrote), merged into
        // the task's one result: its reads the union, its verdict the digest of theirs.
        let mut whole = Hasher::new(ENTRY_DOMAIN);
        let mut reads: blaze_own::AVec<'_, (CellId, ContentId)> = blaze_own::AVec::new_in(w);
        // One item's verdict digest: the task's out as it is (more items: the merged digest).
        let mut single: Option<ContentId> = None;
        let (mut findings, mut failed, mut said) = (0u32, false, None);
        // The body's words why and its findings, in its scratch until the body ends.
        let mut why = blaze_own::String::new_in(sc);
        // Whether a cause of `why` is not about borrows alone: the body's types were then not
        // checked whole ([`Said::Unchecked`]); else only its borrows were ([`Said::BorrowsUnchecked`]).
        let mut types_short = false;
        let mut found: Vec<BodyFinding<'_>, &Frame> = Vec::new_in(sc);
        let before = cost.spent();
        // A head the frozen form could not give it (rule 3: never lowered instead).
        let mut seed_refused = false;
        // The entry's text as its bodies were checked from (its crate's records, the member at
        // 0), and its crate's edition: what its findings' ranges are said as tokens of.
        let item_text = opened.records.file_now(0, one.place.file).and_then(|text| text.get(one.place.item.to_usize())).zip(NamesSource::edition_of(opened.records, 0));
        // The data files (`include_str!`, `include_bytes!`) this body's check reads are listed
        // from here on (job_texts.rs `JobTexts::data`): each is a read of the body.
        let data_at = opened.sources.0.data.borrow().len();
        for (at, item) in one.items.iter().enumerate() {
            let mut asked = one.task;
            asked.planned.name = Name::new(Kind::BodyCheck, item.0);
            match work::unwinding(|| check_body_split(tables, &asked, &body_frame, &make, w, alloc)) {
                Ok(Ok(body)) => {
                    tally.reads.add(body.read_tally);
                    whole.update(&body.out.0);
                    reads.extend_from_slice(&body.cells);
                    mem.recorded += body.cells.len() as u64;
                    add_work(&mut mem.work, &body.work);
                    findings = findings.saturating_add(body.findings);
                    // Its findings as `check` files them; a type error a refused macro call may
                    // have caused is that call's not-checked line instead (body_types.rs).
                    let written = one.place.written.get(at).copied().flatten();
                    for line in body.said.iter() {
                        if let SaidWhat::Type { .. } = line.what
                            && let Some(cut) = line.words.as_str().find(blaze_ladder::front2::MACRO_REFUSED)
                        {
                            let cause = &line.words.as_str()[cut + blaze_ladder::front2::MACRO_REFUSED.len()..];
                            types_short = true;
                            if !why.contains(cause) {
                                if !why.is_empty() {
                                    why.push_str("; ");
                                }
                                why.push_str(cause);
                                why.push_str(": ");
                                why.push_str(line.words.as_str());
                            }
                            continue;
                        }
                        // A finding in a local fn of the body (front2 tables/task.rs
                        // `Said::within`) is shown at the item's start, labelled
                        // `<outer>::<local>` (the `Unplaced` form).
                        let local;
                        let written = if let Some(name) = &line.within {
                            let outer = written.map_or(one.label, |place| place.label);
                            local = bstd::mem::format_in!(sc, "{outer}::{name}");
                            let words = written.map_or("a local function, shown at the item's start: ", |place| place.words);
                            Some(Unplaced { label: local.as_str(), words })
                        } else {
                            written
                        };
                        match found_of(line, written, item_text, sc) {
                            Ok(Some(one_found)) => found.push(one_found),
                            Ok(None) => {}
                            // Never quietly dropped (rule 3): the body is then not checked whole.
                            Err(cause) => {
                                types_short = true;
                                if !why.is_empty() {
                                    why.push_str("; ");
                                }
                                why.push_str(cause);
                            }
                        }
                    }
                    // The first failed body's words why (rule 19): its verdict's not-checked lines.
                    if body.failed && !failed {
                        for line in body.said.iter() {
                            let cause = match &line.what {
                                SaidWhat::NotChecked { cause } => cause.as_str(),
                                SaidWhat::BorrowsNotChecked => "borrows not checked",
                                SaidWhat::Type { .. } | SaidWhat::Lint { .. } | SaidWhat::Borrow { .. } => continue,
                            };
                            if !why.is_empty() {
                                why.push_str("; ");
                            }
                            why.push_str(cause);
                            why.push_str(": ");
                            why.push_str(line.words.as_str());
                        }
                    }
                    // A failed verdict whose only gap is the borrow check: types checked whole.
                    let borrows_only = body.said.iter().any(|l| matches!(l.what, SaidWhat::BorrowsNotChecked)) && !body.said.iter().any(|l| matches!(l.what, SaidWhat::NotChecked { .. }));
                    types_short |= body.failed && !borrows_only;
                    failed |= body.failed;
                    if one.items.len() == 1 {
                        single = Some(body.out);
                    }
                    out.made.extend(body.made);
                }
                Ok(Err(error)) => {
                    seed_refused = matches!(error, TaskError::Seed { .. });
                    said = Some(Said::Task(error));
                    break;
                }
                Err(work::Panicked) => {
                    // A frame past half its reserve after a panic: its body used it up (a stretch
                    // takes no body past [`STRETCH_FULL`]), said as that; the stretch then ends.
                    said = Some(match frame.used() > STRETCH_FULL {
                        true => Said::FrameUsedUp { used: frame.used() as u64, reserved: frame.reserved() as u64 },
                        false => Said::Panicked,
                    });
                    break;
                }
            }
        }
        // AN INCLUDED DATA FILE IS A READ of the body that included it (rule 21): its file's
        // `WHOLE` cell with today's digest, so an edit of the file enters the body again. A file
        // with no FileItems record standing (none, or one that failed) has no cell to read: the
        // body is then refused by name (rule 3), never filed without that read.
        let mut data_reads: blaze_own::AVec<'_, (CellId, ContentId)> = blaze_own::AVec::new_in(w);
        let mut data_refused: Option<&'static str> = None;
        for (path, _) in opened.sources.0.data.borrow_mut().drain(data_at..) {
            let id = CellId { of: Name::file(path.as_bytes()), cell: WHOLE };
            match shared.view.row(id.of) {
                Some(row) if !row.failed => data_reads.push((id, shared.view.cell(&id))),
                _ => {
                    if data_refused.is_none() {
                        data_refused = Some(crate::engine_head_notes::leak(g, format_args!("body `{}` read `{path}` without an available FileItems record", one.label)));
                    }
                }
            }
        }
        if said.is_none()
            && let Some(words) = data_refused
        {
            said = Some(Said::Refused(words));
        }
        let ns = clock.now().since(began).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        let bytes = frame.used().saturating_sub(at_start) as u64;
        let row = Ran { name, label: one.label, ns, bytes, chain: one.place.chain, note: None, found: FindingRange::default(), file: one.file, stretch };
        running.end(shared.board.now(clock));
        if seed_refused {
            // Refused at a seed; never checked by lowering instead (rule 3). A head that cannot
            // be made is reported once, at the head (its head task's refusal, or `check`'s), and
            // each body that needs it is NOT CHECKED, its note naming the head by path (rule 19):
            // the incomplete status, as `check`'s, never a second error a body. Only the engine's
            // own inconsistency (unequal bytes on a second freeze, a table full, a cell the table
            // should hold and does not, bytes that do not read) stays the body's failure.
            tally.refused = tally.refused.saturating_add(1);
            let mut head: Option<Want> = None;
            let mut head_failed: Option<crate::engine_seeds::FirstFailure> = None;
            let said = match said {
                Some(Said::Task(TaskError::Seed { body, error: SeedError::Freeze(error) })) => {
                    match unmade_cell(&error) {
                        Unmade::Head(asked) => {
                            head = Some(asked);
                            let label = |cell| crate::engine_head_notes::cause_label(opened.records, cell, g);
                            let step = |cell| {
                                // Only failed freezes need a retained entry. A refusal from cell_of,
                                // for example, is the typed error of this ask itself.
                                if !matches!(error, FreezeError::Again { .. } | FreezeError::NotTheirs { why: NotTheirs::Failed { .. }, .. }) {
                                    return Some(Step::End(cell, Cause::own(&error, opened.records, g)));
                                }
                                if let Some(local) = seed.and_then(|s| s.with_failure(cell, |failure| seed_cause(cell, failure, shared.seeds, opened.records, &mut head_failed, g))) {
                                    return Some(local);
                                }
                                // A member's head belongs to its head task. This also covers a
                                // stretch's last Needs edge into another source.
                                let crate::engine_heads::HeadAt::Cell { cell: id, .. } = owners.place(opened.records, &head_cell_id(cell), g)? else { return None };
                                let refused = shared.heads_failed.find(id.of, id.cell)?;
                                Some(Step::End(cell, refused.cause))
                            };
                            Some(unmade_head(asked, step, label, tally, sc, g))
                        }
                        Unmade::Unnamed => Some(Said::Unchecked(leak(g, format_args!("not checked because a head it reads could not be made: {error}")))),
                        Unmade::Engine => Some(Said::Task(TaskError::Seed { body, error: SeedError::Freeze(error) })),
                    }
                }
                other => other,
            };
            // Its reads: its scope's names whole, and the head it could not take as `widen`
            // places a head (its head task's cell for a member's, its crate's names record for a
            // dependency's), so the edit that repairs the head enters it again. Through the
            // command's strategy like every body's (engine_scope_reads.rs); the scope's records
            // stay WHOLE under `Cells` too: the refused check left no list of the cells it read
            // (`TaskError::Seed` carries none), so this class is coarse in lane BH1's census.
            // In the body's scratch until its row is copied out (`row_of`).
            let mut reads: blaze_own::AVec<'_, (CellId, ContentId)> = blaze_own::AVec::new_in(w);
            for scope in group.scope.iter() {
                reads.push((CellId { of: *scope, cell: WHOLE }, ABSENT));
            }
            if let Some(want) = head {
                reads.push((head_cell_id(want), ABSENT));
            }
            // What the seed's constant evaluator read for it, as a checked body takes it; a read
            // list it cannot place is no read list: the row is then owed (rule 21), its words
            // after the body's own.
            let placed = add_eval_reads(evaluated, index_reads, tables, opened.records, shared.heads, &group.crates, &mut reads, g);
            let said = match (placed, said) {
                (Ok(()), said) => said,
                (Err(words), Some(Said::Unchecked(own))) => Some(Said::Unchecked(crate::engine_head_notes::leak(g, format_args!("{own}; and its read list is owed: {words}")))),
                // The engine's own fault stays the body's words; the row is owed all the same.
                (Err(_), Some(other)) => Some(other),
                (Err(words), None) => Some(Said::Refused(words)),
            };
            let owed = !matches!(placed, Ok(()));
            mem.before = reads.len() as u64;
            let (mut wide, _) = widen_as(shared.scope_reads, &reads, group, &mut owners, opened.records, shared.view, shared.heads_failed, g, shared.plants.fine_buckets, w);
            // And who checks it (engine_body_plan.rs), as a checked body's.
            crate::engine_body_plan::add_reads(&mut wide, one.held, shared.view);
            let done = match owed {
                false => crate::engine_tasks::failed_with(&one.task, row_of(&mut wide, &data_reads, reads.len(), shared.plants, &mut mem, alloc), alloc),
                true => crate::engine_tasks::failed(&one.task, alloc),
            };
            mem.count_done(&done);
            mem.scratch = sc.used() as u64;
            shared.board.bodies_mem.add(&mem);
            out.done.push(done);
            if let Some(said) = said {
                run_out.notes.push((ran.len(), said.map_task(|error| blaze_own::Box::new_in(error, alloc)), head_failed));
            }
            ran.push(row);
            continue;
        }
        // What the seed's constant evaluator read until now (module docs of
        // engine_head_consts.rs), taken before the verdict is filed: a read list it cannot
        // place is no read list, so the body is owed, named (rule 21), never filed on part of
        // its reads.
        let mut eval_reads: blaze_own::AVec<'_, (CellId, ContentId)> = blaze_own::AVec::new_in(w);
        if said.is_none()
            && let Err(words) = add_eval_reads(evaluated, index_reads, tables, opened.records, shared.heads, &group.crates, &mut eval_reads, g)
        {
            said = Some(Said::Refused(words));
        }
        if shared.seeds.is_some() && said.is_none() {
            tally.seeded = tally.seeded.saturating_add(1);
            tally.rounds = tally.rounds.saturating_add(cost.spent().since(&before).rounds);
        }
        if let Some(said) = said {
            // A panic or a `TaskError` (check_body returns no reads with it): owed, never filed
            // on a partial read list (work.rs: panics and machine faults stay holes).
            let done = crate::engine_tasks::failed(&one.task, alloc);
            mem.count_done(&done);
            mem.scratch = sc.used() as u64;
            shared.board.bodies_mem.add(&mem);
            out.done.push(done);
            run_out.notes.push((ran.len(), said.map_task(|error| blaze_own::Box::new_in(error, alloc)), None));
            ran.push(row);
            continue;
        }
        // Rule 25: the lines record's bytes a function of the findings alone.
        found.sort_unstable_by(|a, b| (a.at, a.rule, a.message).cmp(&(b.at, b.rule, b.message)));
        // Its verdict: one item's as it is (its one define the verdict cell unless it failed);
        // an entry's expansion bodies (or none: an expansion that wrote no body is a pass that
        // read its crate's names), one verdict cell, the merged digest.
        let (out_id, defines_verdict) = match single {
            Some(out_id) => (out_id, !failed),
            None => (whole.finish(), true),
        };
        if one.items.is_empty() {
            reads.push((CellId { of: group.own, cell: WHOLE }, ABSENT));
        }
        reads.extend_from_slice(&eval_reads);
        mem.before = reads.len() as u64;
        let (mut wide, unfrozen) = widen_as(shared.scope_reads, &reads, group, &mut owners, opened.records, shared.view, shared.heads_failed, g, shared.plants.fine_buckets, w);
        // Who checks it is a function of the units ranked before its checker (engine_body_plan.rs).
        crate::engine_body_plan::add_reads(&mut wide, one.held, shared.view);
        // The row, made now from what it keeps: its reads copied out of the scratch at their
        // length, its one define (rule 11; lane BD3).
        let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(usize::from(defines_verdict), alloc);
        if defines_verdict {
            defines.push((blaze_ladder::front2::tables::task::VERDICT_CELL, out_id));
        }
        let mut done = Done { name, key: one.task.key, out: out_id, defines, reads: row_of(&mut wide, &data_reads, reads.len(), shared.plants, &mut mem, alloc), findings, failed };
        // A head it read has no frozen head: the body fails by that read (rule 3), naming the
        // head and quoting why (engine_head_notes.rs), its reads kept so the head's repair
        // enters it again.
        if let Some((task, cell)) = unfrozen {
            if !done.failed {
                done.failed = true;
                done.defines.clear();
                done.findings = done.findings.max(1);
            }
            types_short = true;
            // After the body's own reason when it has one: both are said.
            if !why.is_empty() {
                why.push_str("; ");
            }
            shared.heads_failed.body_words(task, cell, &mut why);
        }
        // One a body not checked whole (its verdict failed, or a line of it could not be said
        // as a finding): in the request's heap, as the labels are.
        let said = (done.failed || !why.is_empty()).then(|| {
            let words = core::str::from_utf8(bstd::mem::format_in!(g, "{why}").into_bytes().leak()).unwrap_or("");
            if types_short { Said::Unchecked(words) } else { Said::BorrowsUnchecked(words) }
        });
        let index = ran.len();
        if let Some(said) = said {
            run_out.notes.push((index, said, None));
        } else if done.findings == 0 {
            run_out.clean += 1;
        }
        run_out.items += one.items.len() as u64;
        run_out.item_tasks += u64::from(!one.items.is_empty());
        // Its findings out of scratch, in the order the merge appends them.
        for one_found in found.iter() {
            run_out.messages.extend_from_slice(one_found.message);
            run_out.found.push((index, one_found.rule, one_found.at, run_out.messages.len()));
        }
        run_out.scratch_most = run_out.scratch_most.max(sc.used() as u64);
        mem.count_done(&done);
        mem.scratch = sc.used() as u64;
        shared.board.bodies_mem.add(&mem);
        ran.push(row);
        out.done.push(done);
    }
    // The stretch's records opened each expansion's origin column once (front2 source.rs
    // `Records::text_marks`): how many, for the stage's line.
    tally.origins_opened = tally.origins_opened.saturating_add(opened.records.opened().origins);
    taken
}

/// A verdict's line as `check` files it (body_types.rs, where a body's findings are filed; the
/// same rule numbers and message pieces): a type error as `TYPES` with its code and words, a
/// typed lint's finding by its lint's number (lints.rs `number_of`; no level kept: rule 81), a
/// move or borrow finding as `BORROWS` (a note while `BORROWS_ARE_ERRORS` is false: print.rs);
/// `None` for a not-checked line, which the body's note says. `written`: the label of a body a
/// macro wrote, or of a block's module, whose findings are at its entry's start, and the words
/// that say why. `item`: the entry's text its bodies were checked from and its edition, which
/// the entry's own findings' ranges are said as tokens of ([`FoundAt`]). `Err`: the line cannot
/// be said as a finding, and why (the body is then not checked whole: rule 3).
fn found_of<'s>(line: &Line<'_>, written: Option<Unplaced<'_>>, item: Option<(&str, blaze_surface::Edition)>, scratch: &'s Frame) -> Result<Option<BodyFinding<'s>>, &'static str> {
    let (rule, code) = match &line.what {
        SaidWhat::Type { code } => (crate::body_types::TYPES, Some(*code)),
        SaidWhat::Borrow { code } => (crate::body_types::BORROWS, Some(*code)),
        SaidWhat::Lint { lint } => match blaze_lint::Rule::from_name(lint) {
            Some(rule) => (crate::lints::number_of(rule), None),
            None => return Err("a typed lint's finding names no rule of blaze-lint"),
        },
        SaidWhat::NotChecked { .. } | SaidWhat::BorrowsNotChecked => return Ok(None),
    };
    let mut message: Vec<u8, &'s Frame> = Vec::new_in(scratch);
    if let Some(code) = code {
        message.extend_from_slice(code.as_bytes());
        message.push(0x1F);
    }
    let at = match written {
        None => {
            let (text, edition) = item.ok_or(ITEM_TEXT_UNREAD)?;
            let range = line.start as usize..line.end as usize;
            FoundAt::Tokens(blaze_file_items::token_place(text, edition, range).map_err(|_| NOT_TOKENS)?)
        }
        Some(unplaced) => {
            message.extend_from_slice(b"in `");
            message.extend_from_slice(unplaced.label.as_bytes());
            message.extend_from_slice(b"`, ");
            message.extend_from_slice(unplaced.words.as_bytes());
            FoundAt::Start
        }
    };
    message.extend_from_slice(line.words.as_str().as_bytes());
    Ok(Some(BodyFinding { rule, at, message: &*message.leak() }))
}

/// Why a body's finding has no place: its entry's text was not in its crate's records.
const ITEM_TEXT_UNREAD: &str = "its entry's text is not in its crate's records (front2 `Records::file_now`), so its findings cannot be placed by tokens";
/// Why a body's finding has no place: its range is no range of its item's tokens.
const NOT_TOKENS: &str = "a finding's range starts or ends in trivia or cuts a token of its item (blaze_file_items::token_place, rule 71)";

/// The note of the bodies a macro wrote that no entry covers: how many, the first by label.
struct Uncovered<'l> {
    count: u32,
    named: &'l [&'static str],
}

impl core::fmt::Display for Uncovered<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} body(ies) a macro wrote are checked by no task: the walk up from their call ends at no file entry and in no block's module (body_types/covered.rs `written_by`, `in_blocks`)", self.count)?;
        for (at, label) in self.named.iter().enumerate() {
            f.write_str(if at == 0 { ": " } else { ", " })?;
            write!(f, "`{label}`")?;
        }
        let rest = (self.count as usize).saturating_sub(self.named.len());
        if rest > 0 {
            write!(f, ", and {rest} more")?;
        }
        write!(f, "; not checked ({RULE})")
    }
}

/// Why an entry has no body: its unit's cfg is not in its crate's records.
const CFG_ATOMS_UNREAD: &str = "its crate's cfg atoms are not in its records (front2 source.rs `cfg_atoms`), so whether a `cfg` left this entry out cannot be told; not checked";
/// Why an entry has no body: a `cfg` predicate on it does not read (body_types.rs `CFG_UNREAD`).
const CFG_UNREAD: &str = "a `cfg` predicate on it or on an item it is written in does not read (blaze-scope `Cfg::eval`), so whether its crate holds it cannot be told; not checked";
/// Why an entry has no body: its crate holds it under its cfg, yet its records list no body there.
const ENTRY_UNLISTED: &str = "its crate holds it under its cfg, yet its crate's records list no body at this entry and no macro wrote one at it (front2 project_records.rs `index_by_chain`); not checked";
/// Why the root's replaced `fn main` has no body to check: front2's root `main` has no place.
const ROOT_MAIN_UNPLACED: &str = "this is the root's `fn main`, which D104's launcher replaced; the root `main` has no place in the records (body_types/covered.rs `root_of`); not checked";

/// A body entry of a held file at which its unit's records list no body and no macro wrote one
/// ([`BodiesStage::groups`], step 3), as body_types/covered.rs `root_of` and body_types.rs
/// `Checker::holds` decide it (the reference for coverage): `Err(ENTRY_ABSENT)` when a `cfg` on
/// it or on an item it is written in is false under the unit's cfg (not this unit's body: the
/// Reference, "Conditional compilation"); the key of the launcher's `main` when it is the root
/// file's top-level `fn main` (D104: the launcher `fn main() -> Exit { fn main(..) <the written
/// body> .. }`; its local `main`, the body the user wrote, is checked by the task's work list,
/// front2 tables/task.rs); else a refusal naming why. Never a silent absence (rule 3).
fn absent_entry<'n>(
    records: &'n blaze_ladder::front2::source::Records<'n>,
    outline: &blaze_file_items::Outline<'_>,
    item: &blaze_surface::Item,
    text: &str,
    root_file: bool,
    cfg: Option<&blaze_scope::task::Cfg<'_, '_>>,
    g: blaze_own::Alloc<'static>,
) -> Result<ItemKey, &'static str> {
    if planted_silent_absent() {
        return Err(crate::engine_body_plan::ENTRY_ABSENT);
    }
    let Some(cfg) = cfg else { return Err(CFG_ATOMS_UNREAD) };
    // Up the parents, bounded by the outline's length (rule 18).
    let all = outline.items();
    let mut at = Some(item);
    let mut steps = 0usize;
    while let Some(entry) = at {
        let attrs = blaze_scope::task::item_attrs(cfg, outline, entry, text, g);
        if attrs.cfg_unread > 0 {
            return Err(CFG_UNREAD);
        }
        if !attrs.cfg_ok {
            return Err(crate::engine_body_plan::ENTRY_ABSENT);
        }
        steps += 1;
        if steps > all.len() {
            return Err(CFG_UNREAD);
        }
        at = entry.parent.and_then(|p| all.get(p.0 as usize));
    }
    let root_main_entry = root_file && item.parent.is_none() && matches!(item.kind(), blaze_surface::ItemKind::Function) && crate::item_name::shown_name(item, text) == "main";
    if !root_main_entry {
        return Err(ENTRY_UNLISTED);
    }
    let key = blaze_ladder::front2::program::run_entry::root_main(records)
        .map_err(|why| crate::engine_head_notes::leak(g, format_args!("this is the root's `fn main`, which D104's launcher replaced, and the launcher's `main` is not found: {why} (body_types/covered.rs `root_of`); not checked")))?;
    match NamesSource::place_of(records, key).map(|p| p.at) {
        Some(blaze_scope::resolve::At::Expansion(_)) => Ok(key),
        Some(blaze_scope::resolve::At::File(file)) => Err(crate::engine_head_notes::leak(
            g,
            format_args!("this is the root's `fn main`, but the root `main` the records list is written in file {file} and not at this item (body_types/covered.rs `root_of`); not checked"),
        )),
        None => Err(ROOT_MAIN_UNPLACED),
    }
}

/// A member's lent environment copied into `frame` (no allocator of the lending thread is
/// called here, crate_env.rs `EnvLent`), as body_types.rs does for its workers.
fn env_in<'l: 'n, 'n>(lent: Option<EnvLent<'_, 'l>>, frame: &'n Frame) -> Result<Option<&'n blaze_scope::crate_env::CrateEnv<'n>>, blaze_build_script::outputs::RecordError> {
    let Some(lent) = lent else { return Ok(None) };
    let erased: &'n blaze_own::Erased<&'n Frame> = blaze_own::Box::leak(blaze_own::Box::new_in(blaze_own::Erased::new(frame), frame));
    let env = lent.copy_in(erased.handle())?;
    Ok(Some(&*blaze_own::Box::leak(blaze_own::Box::new_in(env, frame))))
}

/// What a stretch's seed evaluator read until now ([`crate::engine_head_consts::EvalSink`]),
/// into a body's `reads` as a head task takes its evaluator's (engine_heads.rs `heads_of`):
/// each names cell as front2 names it (tables/task.rs `cell_id`) and, for each item whose text
/// it read, the cell that moves with that text (engine_head_consts.rs `text_cell`); each read
/// ABSENT, as [`widen_as`] reads every cell at the view's digest. Nothing when the evaluator
/// evaluated no constant (`index_reads`: the reads of building its lang index alone). Refused,
/// with the words a note prints, when a read cannot be placed or the reads were past their cap.
#[expect(clippy::too_many_arguments, reason = "a body's reads and every table that names them")]
fn add_eval_reads<'n, A: core::alloc::Allocator>(
    evaluated: &crate::engine_head_consts::EvalSink<'_>,
    index_reads: usize,
    tables: Tables<'_, 'n>,
    records: &'n blaze_ladder::front2::source::Records<'n>,
    subjects: &crate::engine_heads::Subjects,
    crates: &[Crate<'_>],
    reads: &mut Vec<(CellId, ContentId), A>,
    g: blaze_own::Alloc<'static>,
) -> Result<(), &'static str> {
    if evaluated.len() <= index_reads {
        return Ok(());
    }
    let mut table: Vec<blaze_ladder::front2::tables::recorder::TableRead, blaze_own::Alloc<'static>> = Vec::new_in(g);
    evaluated
        .reads(&mut |one| table.push(one))
        .map_err(|full| crate::engine_head_notes::leak(g, format_args!("its constants' evaluation at the seed read too much: {full}")))?;
    for one in table.iter() {
        match blaze_ladder::front2::tables::task::cell_id(tables, *one) {
            Ok(Some(id)) => reads.push((id, ABSENT)),
            Ok(None) => {}
            Err(error) => return Err(crate::engine_head_notes::leak(g, format_args!("a names read of its constants' evaluation at the seed names a crate its scope does not hold: {error} (front2 tables/task.rs `cell_id`)"))),
        }
    }
    crate::engine_head_consts::eval_reads(records, subjects, &table, &mut |id| reads.push((id, ABSENT))).map_err(|unplaced| {
        let shown = crate::engine_head_consts::UnplacedIn { unplaced: &unplaced, crates };
        crate::engine_head_notes::leak(g, format_args!("its constants' evaluation at the seed read a text no crate of its scope holds: {shown}"))
    })
}

/// [`widen`] by the command's strategy (engine_scope_reads.rs, rule 78): the one dispatch from
/// the runtime choice to a [`crate::engine_scope_reads::ScopeReads`] instantiation.
#[expect(clippy::too_many_arguments, reason = "the strategy, a body's reads and every table that names them")]
fn widen_as<'w, 'n, 's: 'n>(
    reads_as: crate::engine_scope_reads::ScopeReadsAs,
    reads: &[(CellId, ContentId)],
    group: &Group<'_, '_>,
    owners: &mut crate::engine_heads::Owners<'s>,
    records: &'n blaze_ladder::front2::source::Records<'n>,
    view: View<'_, '_>,
    heads_failed: &crate::engine_head_notes::Failed,
    g: blaze_own::Alloc<'static>,
    fine_buckets: bool,
    work: blaze_own::Alloc<'w>,
) -> (blaze_own::AVec<'w, (CellId, ContentId)>, Option<(Name, Cell)>) {
    use crate::engine_scope_reads::{Cells, ScopeReadsAs, Whole};
    match reads_as {
        ScopeReadsAs::Whole => widen::<Whole>(reads, group, owners, records, view, heads_failed, g, fine_buckets, work),
        ScopeReadsAs::Cells => widen::<Cells>(reads, group, owners, records, view, heads_failed, g, fine_buckets, work),
        #[cfg(test)]
        ScopeReadsAs::PlantedCells => widen::<crate::engine_scope_reads::PlantedCells>(reads, group, owners, records, view, heads_failed, g, fine_buckets, work),
    }
}

/// Whether `tables` (an `ImplBucket` task's name) are a DEPENDENCY's of `group`'s scope: a
/// scope crate whose names task defines no cell table (`CELLS` ABSENT: engine_names.rs
/// `cells_of`, which defines one for a crate whose names read a checkout file, a member's or a
/// path dependency's). Such a crate's text is keyed by the lock and the shelf, so its tables
/// move only with a new graph, and [`widen`] reads them whole (engine_scope_reads.rs, "A
/// DEPENDENCY'S BUCKETS"). A name of no scope crate is not one: read as it was read.
fn dependency_tables(group: &Group<'_, '_>, view: View<'_, '_>, tables: Name) -> bool {
    let Ok(at) = group.buckets.binary_search_by_key(&tables, |(name, _)| *name) else { return false };
    view.cell(&CellId { of: group.buckets[at].1, cell: CELLS }) == ABSENT
}

/// A body's reads widened to cells a phase defines (module docs), each with the digest the view
/// holds, sorted and one each, in `work` (the body's scratch: the row is copied out of it at its
/// own length, [`row_of`]); and the first head it read that was not frozen (its head task
/// failed whole, or that head of it: engine_head_notes.rs), by task and cell. `fine_buckets`:
/// the plant that keeps a dependency's bucket reads one a bucket (the coarse step off).
#[expect(clippy::too_many_arguments, reason = "a body's reads and every table that names them")]
fn widen<'w, 'n, 's: 'n, S: crate::engine_scope_reads::ScopeReads>(
    reads: &[(CellId, ContentId)],
    group: &Group<'_, '_>,
    owners: &mut crate::engine_heads::Owners<'s>,
    records: &'n blaze_ladder::front2::source::Records<'n>,
    view: View<'_, '_>,
    heads_failed: &crate::engine_head_notes::Failed,
    g: blaze_own::Alloc<'static>,
    fine_buckets: bool,
    work: blaze_own::Alloc<'w>,
) -> (blaze_own::AVec<'w, (CellId, ContentId)>, Option<(Name, Cell)>) {
    let mut unfrozen: Option<(Name, Cell)> = None;
    // In the body's scratch, sized to what comes in (a bump frame grows a list by copying it).
    let mut wide: blaze_own::AVec<'w, (CellId, ContentId)> = blaze_own::AVec::with_capacity_in(reads.len() + 1, work);
    let mut push = |of: Name, cell: Cell| {
        let id = CellId { of, cell };
        wide.push((id, view.cell(&id)));
    };
    for (id, _) in reads.iter() {
        match id.of.kind {
            // A names read of a crate whose task defines its cell table (engine_names.rs
            // `cells_of`, marked by `CELLS`) stays its cell: the table holds every cell a walk
            // can name that holds something, each digest over all the cell answers, so the cell
            // moves whenever the answer can (rule 21); a cell it does not hold is the name not
            // found, read ABSENT, and the name added defines it, which moves it. A crate's lang
            // rows are one such cell (blaze-scope names_cells.rs `NameCell::Lang`).
            Kind::CrateScope if !S::keeps(group.own, *id) => {}
            Kind::CrateScope if id.cell != WHOLE && view.cell(&CellId { of: id.of, cell: CELLS }) != ABSENT => push(id.of, id.cell),
            Kind::CrateScope => push(id.of, WHOLE),
            // The whole graph's bucket fact: the strategy's (engine_scope_reads.rs).
            Kind::ImplBucket if id.of.subject == ABSENT => S::incoherent_fact(&group.scope, &mut push),
            // A head (engine_heads.rs `Owners::place`): a member's, the cell of the head task of
            // the entry that covers it, a macro-written head's included; a dependency's, a body's
            // own item's or one no head task holds (the body is refused by its words), its
            // crate's names record whole. A head of a crate none of the records holds is read as
            // read.
            Kind::Signature => match owners.place(records, id, g) {
                Some(crate::engine_heads::HeadAt::Cell { cell, .. }) => {
                    if unfrozen.is_none() && (view.row(cell.of).is_some_and(|row| row.failed) || heads_failed.has(cell.of, cell.cell)) {
                        unfrozen = Some((cell.of, cell.cell));
                    }
                    push(cell.of, cell.cell);
                }
                Some(crate::engine_heads::HeadAt::Dependency(scope) | crate::engine_heads::HeadAt::OfTheBody(scope) | crate::engine_heads::HeadAt::Unlisted { scope, .. }) => push(scope, WHOLE),
                None => push(id.of, id.cell),
            },
            // A dependency's bucket (lane BD3): its tables' `WHOLE`, the digest of all three of
            // its records (engine_heads.rs: `defines.push((WHOLE, one.out))`), which moves when
            // any bucket of it does; one cell a crate however many buckets the body probed.
            Kind::ImplBucket if !fine_buckets && dependency_tables(group, view, id.of) => push(id.of, WHOLE),
            // A crate's bucket: its cell as the crate's Heads task defined it (engine_heads.rs),
            // or ABSENT when the crate files no impl there (a later impl defines it, and the
            // read moves).
            _ => push(id.of, id.cell),
        }
    }
    // The environment every body was checked under: its script run, whole.
    if let Some(script) = group.script {
        push(script, WHOLE);
    }
    wide.sort_unstable_by_key(|(id, _)| *id);
    wide.dedup_by_key(|(id, _)| *id);
    (wide, unfrozen)
}

/// The row's read list out of the body's scratch into the request's arena (`alloc`): one block
/// of exactly its length, counted by kind into `mem` (the only copy of a body's reads that
/// outlives it; rule 11). Under the plant ([`PLANT_ROW_AT_RECORDED`]) at the capacity of the
/// list before widening (`before`), as `widen` made the row before lane BD3. `data`, the
/// included data files' cells the body read (run_stretch), joins `wide` first (in the scratch),
/// the whole sorted and each cell once.
fn row_of<'a, 'w>(wide: &mut blaze_own::AVec<'w, (CellId, ContentId)>, data: &[(CellId, ContentId)], before: usize, plants: RowPlants, mem: &mut BodyMem, alloc: SharedAlloc<'a>) -> SVec<'a, (CellId, ContentId)> {
    if !data.is_empty() {
        wide.extend_from_slice(data);
        wide.sort_unstable_by_key(|(id, _)| *id);
        wide.dedup_by_key(|(id, _)| *id);
    }
    let room = if plants.row_at_recorded { before.max(wide.len()) } else { wide.len() };
    let mut row: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(room, alloc);
    row.extend_from_slice(wide);
    mem.count_row(&row);
    row
}

/// `more` added into `into`, part by part.
fn add_work(into: &mut WorkBytes, more: &WorkBytes) {
    into.recorder += more.recorder;
    into.recorder_floor += more.recorder_floor;
    into.cells += more.cells;
    into.held += more.held;
    into.said += more.said;
    into.verdict += more.verdict;
    into.goal_hits += more.goal_hits;
}
