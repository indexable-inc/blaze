//! THE BODY TASK (engine.md, E2/E5): [`check_body`] checks one body over the read-only view
//! ([`Tables`]) and answers the engine's [`Done`]: what it found, the cells it read with the
//! digest each held, what it offers readers, and the records it made inline ([`Made`]). It is
//! a function of `(tables, task, frame, make)` alone, callable from any worker.
//!
//! NOTHING IS SHARED OR KEPT. The checker's mutable world (its key map, head store, impl index,
//! constants and prepared macros: `ProjectWorld`) is made for this one body and dropped with
//! it, so a body's answer and its read list are the same whichever bodies ran before it, in
//! which order, on how many workers. The recorder is the task's own. The engine takes the
//! distinct keys through [`check_body_reads`] and sorts the combined row after placement.
//!
//! TWO ALLOCATORS ([`check_body_split`], lane BD3): `work`, where the check works (the recorder,
//! its `seen` set, the cells before the engine widens them, the verdict's words and bytes),
//! which the caller frees as the body ends; and `out`, where what outlives the body goes (the
//! records made inline). The row is the caller's, copied from the answer's cells at its length.
//!
//! THE BODY is found by its item's cells (`Item{key}`; a member's `Members{owner}` too), never
//! by listing the crate: the task's name's subject is the body's item key.
//!
//! WHAT IS READ, as cells:
//! - every names question through the view, cell by cell (view.rs);
//! - each head, constant and ADT fact the check asked (the body's `facts`, front2 reads.rs),
//!   as a `Signature` cell of the item by its checker key and the fact's kind;
//! - each impl table bucket a lookup read (the `CrateBucket` facts), as an `ImplBucket` cell.
//!
//! Each cell's digest is asked of [`Make`]: the engine's view for a cell an earlier phase made,
//! a record made inline (a [`Made`]) for one no phase made yet. The task never stops to wait
//! for a need: it is entered once and gives one result (entries per result: 1).
//!
//! INTERIM (until lane FH's frozen heads): the heads a body needs are made in its own store
//! from the view, so a body's reads also hold the cells those heads read (a superset: never a
//! read missed, sometimes a body dirtied by a head's text it did not need).

use core::cell::RefCell;
use core::fmt::Write as _;

use blaze_drive::cells::Cell;
use blaze_drive::key::{ABSENT, Kind, Name};
use blaze_engine::{CellId, Done, Made, SVec, Task};
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AString, AVec, Alloc, SharedAlloc};
use blaze_scope::names_cells::NameCell;
use blaze_surface::ItemKind;

use super::super::Front2;
use super::super::borrows::{BorrowRefused, BorrowVerdict, KeptAt};
use super::super::buckets::impl_tables_name;
use super::super::index::{Index, LangTwice};
use super::super::keys::{Keys, MemberKind};
use super::super::project::{BodyRef, BodyVerdict, ProjectWorld};
use super::super::project::every_body::Reached;
use super::super::project_records::Listed;
use super::super::reads::{FactKind, ReadsError};
use super::super::by_origin::{FactHolder, fact_holder, kind_of_key};
use super::super::item_shown::{FactDefinition, ItemShown};
use super::super::source::{NamesSource, Records};
use super::head::{FreezeError, cell_id as head_cell_id, fact_cell};
use super::super::texts::Texts;
use super::super::heads::NeedsMet;
use super::cost::{Cost, Part};
use super::seeds::{HeadSeed, SeedError};
use super::recorder::{PlacedCell, ReadOf, ReadSink, ReadsFull, Recorder, RecorderOpts, TableRead};
use super::view::Tables;

/// The domain of a body result's `out`.
const OUT_DOMAIN: &str = "blaze-ladder front2 body task out 1";
/// The one cell a body's record offers its readers: its verdict (`out`).
pub const VERDICT_CELL: Cell = Cell([0; 16]);

/// What a body task is asked to make inline and what the engine knows: the digest a cell holds
/// today. A cell an earlier phase made is the engine's (`View::cell`); one no phase has made
/// yet is made here, its record pushed to `made`. `ABSENT`: no such cell (an empty bucket, a
/// head never written), which a later write moves.
pub trait Make<'a> {
    fn make(&self, want: &CellId, made: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused>;
}

/// [`Make`] could not give a cell's digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MakeRefused {
    pub want: CellId,
}

/// What a body task runs in.
pub struct BodyFrame<'n> {
    /// The frame a body's lowering keeps its local items and expansions in.
    pub frame: &'n blaze_mem::Frame,
    /// Where the body's world and its result live.
    pub alloc: Alloc<'static>,
    /// PLANTED BUG, tests only: the recorder drops every read of this cell.
    #[doc(hidden)]
    pub plant_drop: Option<NameCell>,
    /// The worker's meter (cost.rs), when one measures where a body's time goes.
    pub cost: Option<&'n Cost<'n>>,
    /// How heads' needs are met (heads.rs `NeedsMet::All` outside the equality test).
    #[doc(hidden)]
    pub needs: NeedsMet,
    /// The request's frozen heads (seeds.rs [`FrozenTable`](super::seeds::FrozenTable), read
    /// only and shared): every head the body needs is filed from them, none lowered. `None`:
    /// the body's store lowers each head it needs (the path before frozen heads).
    pub heads: Option<&'n dyn HeadSeed<'n>>,
    /// What the body's macro calls expand with (body_host.rs `BodySources`: the owner's
    /// environment, the expansion texts, the proc-macro runner), as `check`'s path sets them
    /// (project.rs `set_body_sources`). `None`: each body `env!`, include and proc macro is
    /// refused by name.
    pub sources: Option<&'n dyn super::super::body_host::BodySources>,
    /// The request's table of shared trait goals (goals.rs [`GoalTable`](super::goals::GoalTable),
    /// one for the request's workers): each root goal the body asks is looked up there first.
    /// `None`: every goal is solved in the body.
    pub goals: Option<&'n dyn blaze_solve::share::GoalShare>,
    /// Stretch-scoped declarative macro preparation table shared across bodies in a stretch so
    /// each body allocates 0 bytes in `frame` for `Prepares`. `None`: `ProjectWorld::over`
    /// allocates a fresh one in `frame`.
    pub prepared: Option<&'n blaze_lower::scoped::Prepares<'n>>,
}

/// The ask a failed cell belonged to. At import time the checker may have made several
/// distinct asks about the opaque, or none at all. The rows are sorted by fact kind.
#[derive(Debug)]
pub enum CellAsk {
    Fact(FactKind),
    SeedBeforeFact,
    Several { first: FactKind, another: FactKind },
    SeedWithoutReads(ReadsError),
}

impl CellAsk {
    fn during_seed(verdict: &BodyVerdict<'_>, key: blaze_typeck::ItemKey) -> Self {
        let facts = match &verdict.facts {
            Ok(facts) => facts,
            Err(why) => return Self::SeedWithoutReads(*why),
        };
        let mut kinds = facts.rows.iter().filter(|row| row.fact.key == key.0).map(|row| row.fact.kind);
        let Some(first) = kinds.next() else { return Self::SeedBeforeFact };
        match kinds.find(|&kind| kind != first) {
            Some(another) => Self::Several { first, another },
            None => Self::Fact(first),
        }
    }
}

/// Why a body task gave no result: each a fault of the plan or the engine, never of the body
/// (a body's own failure is a [`Done`] with `failed`).
#[derive(Debug)]
pub enum TaskError {
    /// A head the body needed could not be filed from the request's frozen heads.
    Seed { body: Name, error: SeedError },
    /// No head cell holds the fact of checker key `key`, an item of a kind with a head
    /// (by_origin.rs [`FactHolder::Head`]; head.rs `fact_cell`): `shown` names it by its path
    /// (rule 19).
    Cell { key: blaze_typeck::ItemKey, ask: CellAsk, holder: blaze_typeck::ItemKey, shown: FactDefinition, error: FreezeError },
    /// The task's name is not a body's (`Kind::BodyCheck` of a fn, `const` or `static` item,
    /// or a fn or `const` member).
    NotABody { name: Name },
    /// Two lang items under one name in the lang crates.
    Lang(LangTwice),
    /// The body's read list could not be made into facts.
    Facts(ReadsError),
    /// The body read more than the recorder holds.
    Full(ReadsFull),
    /// A read names a crate the view does not hold.
    NoCrate { prefix: [u8; 8] },
    /// [`Make`] refused a cell.
    Make(MakeRefused),
    /// Writing the result's bytes failed (its allocator).
    Write,
    /// The body's board for the one ask (front2 asks.rs `AsksMet::Once`) could not be made.
    Asks { body: Name, why: blaze_core::claim_table::ClaimError },
    /// The body's clean check could not be written as its checked record (blaze-typeck
    /// checked_record.rs, D163 R1): never a body left without one quietly (rule 3).
    Typed { body: Name, why: blaze_typeck::checked_record::CheckedRecordError },
}

/// Maps a raw table question to the engine cell grain used by its caller's scope.
pub trait ReadKeyer {
    fn key(&self, tables: Tables<'_, '_>, read: TableRead) -> Result<Option<PlacedCell>, TaskError>;

    /// The source facets and head parts have their own, injective identities and cannot
    /// alias another kind. Such a caller can leave placement of these reads to its row.
    fn keeps_item_keys(&self) -> bool { false }
}

struct ObservedReads<'r, 'a, 't, 'n, 'k> {
    recorder: &'r Recorder<'a>,
    tables: Tables<'t, 'n>,
    keyer: Option<&'k dyn ReadKeyer>,
    item_keyer: Option<&'k dyn ReadKeyer>,
    error: RefCell<Option<TaskError>>,
}

impl ReadSink for ObservedReads<'_, '_, '_, '_, '_> {
    fn read(&self, read: TableRead) {
        let keyer = match read {
            TableRead::Head { .. } | TableRead::Signature { .. } | TableRead::Text { .. } => self.item_keyer,
            TableRead::Names(_) | TableRead::Impls { .. } => self.keyer,
        };
        let Some(keyer) = keyer else {
            self.recorder.read(read);
            return;
        };
        if let Err(error) = self.recorder.read_mapped(read, || keyer.key(self.tables, read)) {
            self.error.borrow_mut().get_or_insert(error);
        }
    }

    fn asking(&self, ask: Option<blaze_typeck::Asking>) {
        ReadSink::asking(self.recorder, ask);
    }

    fn binding(&self, module: ItemKey, ns: blaze_surface::Namespace, name: &str) {
        let Some(keyer) = self.keyer else {
            ReadSink::binding(self.recorder, module, ns, name);
            return;
        };
        if let Some(read) = self.recorder.first_binding(module, ns, name) {
            match keyer.key(self.tables, read) {
                Ok(cell) => self.recorder.keep(read, cell),
                Err(error) => {
                    self.recorder.keep(read, None);
                    self.error.borrow_mut().get_or_insert(error);
                }
            }
        }
    }
}

impl core::fmt::Display for TaskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TaskError::NotABody { name } => write!(f, "the task {name:?} names no body (front2 tables/task.rs)"),
            TaskError::Lang(twice) => write!(f, "two lang items under one name: {twice:?}"),
            TaskError::Facts(error) => write!(f, "the body's read list: {error}"),
            TaskError::Full(full) => write!(f, "{full}"),
            TaskError::NoCrate { prefix } => write!(f, "a read names the crate {prefix:02x?}, which the tables do not hold"),
            TaskError::Make(refused) => write!(f, "no digest for the cell {:?} (front2 tables/task.rs `Make`)", refused.want),
            TaskError::Write => write!(f, "the body's result could not be written"),
            TaskError::Asks { body, why } => write!(f, "body {body:?}: the one ask's board (front2 asks.rs `BOARD_SLOTS`) could not be made: {why:?}"),
            TaskError::Typed { body, why } => write!(f, "body {body:?}: its clean check has no checked record: {why}"),
            TaskError::Seed { body, error } => write!(f, "body {body:?}: a head it needs could not be seeded: {error}"),
            TaskError::Cell { key, ask, holder, shown, error } => {
                writeln!(f, "{shown}")?;
                match ask {
                    CellAsk::Fact(kind) => write!(f, "the body requested {kind:?}")?,
                    CellAsk::SeedBeforeFact => f.write_str("the seed was refused before the checker recorded a fact for this key")?,
                    CellAsk::Several { first, another } => write!(f, "the body requested multiple facts for this key, including {first:?} and {another:?} (the failing ask is ambiguous)")?,
                    CellAsk::SeedWithoutReads(why) => write!(f, "the body's requested facts could not be read: {why}")?,
                }
                write!(f, ", key {:032x}, attempted holder {:032x}: {error}", key.0, holder.0)
            }
        }
    }
}

/// A body task's answer: the engine's result, the records made inline, and what tests compare.
pub struct BodyOut<'a> {
    pub done: Done<'a>,
    pub made: SVec<'a, Made<'a>>,
    /// The verdict's canonical bytes: `done.out` is their digest.
    pub bytes: AVec<'a, u8>,
    /// The recorder's list (sorted, each once), before it is named as cells.
    pub reads: AVec<'a, TableRead>,
    /// The head read of each head the body's own store held after the check, with the head's
    /// checker key (sorted): what the read-list control compares `reads` against.
    pub held: AVec<'a, (TableRead, u128)>,
    /// What the verdict says, as `check` prints it from today's path (front2 said.rs, the one
    /// home of a refusal's words and cause): each type diagnostic, typed lint finding and
    /// borrow finding with its item-relative range, then the reason a body was not checked.
    pub said: AVec<'a, Said<'a>>,
    /// The shared goals the body's check took (blaze-solve share.rs `GoalHit`: the goal's key
    /// and its read digest), in the order taken: the one read each hit is. TODAY each hit's
    /// own reads are in `reads` too (share.rs, "A HIT IS ONE READ"); the engine names a goal
    /// as a cell of its own once goals are records.
    pub goal_hits: AVec<'a, blaze_solve::share::GoalHit>,
    /// The reads the body's recorder was offered and kept (recorder.rs [`ReadTally`](super::recorder::ReadTally)):
    /// the Bodies stage's reads line.
    pub read_tally: super::recorder::ReadTally,
}

/// A body task's answer by [`check_body_split`], in TWO allocators (rule 11: by lifetime): what
/// outlives the body (the records made inline, `made`) in the caller's `out`, and every list
/// that is only a step toward the row (the recorder's list, the cells before the engine widens
/// them, the control's `held`, the verdict's words and bytes) in its `work`, which the caller
/// frees as the body ends (blaze-cli engine_bodies.rs: the body's scratch frame, reset per
/// body). The row itself is made by the caller from `cells` ([`BodyChecked::done_in`]), sized
/// to what it keeps: before, the row's list was `cells` (in the request's arena, sized to the
/// recorder's list) and every list beside it was in the request's arena too.
pub struct BodyChecked<'a, 'w> {
    /// The verdict's digest (`Done::out`).
    pub out: ContentId,
    /// Whether the body was refused, left a gap, or its borrows were refused (`Done::failed`).
    pub failed: bool,
    /// The type, lint and borrow findings (`Done::findings`).
    pub findings: u32,
    /// Each cell the body read with the digest [`Make`] gave, sorted by cell and each once.
    pub cells: AVec<'w, (CellId, ContentId)>,
    pub made: SVec<'a, Made<'a>>,
    /// As [`BodyOut`]'s fields of the same names.
    pub bytes: AVec<'w, u8>,
    pub reads: AVec<'w, TableRead>,
    pub held: AVec<'w, (TableRead, u128)>,
    pub said: AVec<'w, Said<'w>>,
    pub goal_hits: AVec<'w, blaze_solve::share::GoalHit>,
    pub read_tally: super::recorder::ReadTally,
    /// What the check's working lists took in `work` ([`WorkBytes`]).
    pub work: WorkBytes,
    /// As [`BodyReads::typed`].
    pub typed: Option<AVec<'w, u8>>,
}

/// The bytes a body's check took in its `work` allocator, by kind, counted from the lists'
/// capacities as they stood (no walk; rule 16: the Bodies stage's memory line, blaze-cli
/// engine_bodies.rs `BodyMem`). Each is freed with the body; each one's floor is said beside it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct WorkBytes {
    /// The recorder's tables at their most, its list and its `seen` set (recorder.rs
    /// [`Recorder::table_bytes`]); floor: its distinct reads as one list
    /// ([`WorkBytes::recorder_floor`]).
    pub recorder: u64,
    /// The recorder's distinct reads as one list: `kept` reads of `size_of::<TableRead>()`.
    pub recorder_floor: u64,
    /// The cells before the engine widens them (`cells`); floor: the same, it is one list.
    pub cells: u64,
    /// The control's instrument (`held`, and the keys it is made from); floor: 0 (only tests read it).
    pub held: u64,
    /// The verdict's lines and their words (`said`); floor: the words.
    pub said: u64,
    /// The verdict's canonical bytes (`bytes`); floor: 0, its digest `out` is what is kept.
    pub verdict: u64,
    /// The shared goals taken (`goal_hits`).
    pub goal_hits: u64,
}

impl<'a, 'w> BodyChecked<'a, 'w> {
    /// The engine's row of this answer for `task` under `name`, in `alloc`: its reads `cells`
    /// as they are (no widening), one block of their length; its one define the verdict cell
    /// unless it failed.
    pub fn done_in<'r>(&self, task: &Task, name: Name, alloc: SharedAlloc<'r>) -> Done<'r> {
        let mut reads: SVec<'r, (CellId, ContentId)> = SVec::with_capacity_in(self.cells.len(), alloc);
        reads.extend_from_slice(&self.cells);
        let mut defines: SVec<'r, (Cell, ContentId)> = SVec::with_capacity_in(usize::from(!self.failed), alloc);
        if !self.failed {
            defines.push((VERDICT_CELL, self.out));
        }
        Done { name, key: task.key, out: self.out, defines, reads, findings: self.findings, failed: self.failed }
    }
}

/// One line of a body's verdict ([`BodyOut::said`]): `start..end` is item-relative (rule 71).
#[derive(Debug)]
pub struct Said<'a> {
    /// Path below the task's root, e.g. `mid::inner`. The root's own lines have no prefix.
    pub within: Option<AString<'a>>,
    pub what: SaidWhat<'a>,
    pub start: u32,
    pub end: u32,
    /// The finding's words; for a body not checked, the words after `body `<name>`: `.
    pub words: AString<'a>,
}

/// Which line a [`Said`] is.
#[derive(Debug)]
pub enum SaidWhat<'a> {
    /// A type error, by its code.
    Type { code: &'static str },
    /// A typed lint's finding, by the lint's name (blaze-lint `Rule::from_name`).
    Lint { lint: &'static str },
    /// A move or borrow error, by its code.
    Borrow { code: &'static str },
    /// The body was not checked: refused or past a gap, grouped under `cause` (said.rs). A
    /// failed macro call's expansion (body_types.rs `failed_calls`) is not split out yet.
    NotChecked { cause: AString<'a> },
    /// The borrow check was refused (not for want of clean types).
    BorrowsNotChecked,
}

/// The body `task` names, checked over `tables` (module docs), every list in `shared`: the
/// tests' form, the answer and its row in one allocator.
pub fn check_body<'n, 'a>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, shared: SharedAlloc<'a>) -> Result<BodyOut<'a>, TaskError> {
    check_body_in(tables, task, frame, make, shared, Coverage::WithLocals)
}

pub(super) enum Coverage {
    WithLocals,
    #[cfg(test)]
    RootOnly,
}

/// [`check_body`] with its coverage named (local_fns_tests.rs: `RootOnly`, the control), every
/// list in `shared`: [`check_split`] with one allocator for both, its row made from its cells.
pub(super) fn check_body_in<'n, 'a>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, shared: SharedAlloc<'a>, coverage: Coverage) -> Result<BodyOut<'a>, TaskError> {
    let checked = check_split(tables, task, frame, make, shared.local(), shared, coverage)?;
    let done = checked.done_in(task, task.planned.name, shared);
    let BodyChecked { made, bytes, reads, held, said, goal_hits, read_tally, .. } = checked;
    Ok(BodyOut { done, made, bytes, reads, held, said, goal_hits, read_tally })
}

/// The body `task` names and every local fn it reaches, checked over `tables` (module docs), in
/// two allocators ([`BodyChecked`]): `work`, where the check works and which the caller frees as
/// the body ends, and `out`, where what outlives the body (the records made inline) goes. The
/// row is the caller's to make from the answer's `cells` (the engine widens them first).
pub fn check_body_split<'n, 'a, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, work: Alloc<'w>, out: SharedAlloc<'a>) -> Result<BodyChecked<'a, 'w>, TaskError> {
    check_split(tables, task, frame, make, work, out, Coverage::WithLocals)
}

/// A verdict and the distinct questions that produced it. No ID has been derived and no
/// engine cell has been looked up. A stretch places and combines these with its other reads.
pub struct BodyReads<'w> {
    pub out: ContentId,
    pub failed: bool,
    pub findings: u32,
    pub reads: AVec<'w, TableRead>,
    pub said: AVec<'w, Said<'w>>,
    pub read_tally: super::recorder::ReadTally,
    pub work: WorkBytes,
    /// The defining texts of the constants the body's evaluator read (project.rs
    /// `record_const_texts`): each is a read the engine places beside `reads`.
    pub const_texts: AVec<'static, TableRead>,
    /// The library key of each fn, const and static declared in a body the check reached (`each_from` past the
    /// root, at any depth), in the order reached: bodies this check checked that are no outline
    /// entry of their own, so the caller lists them in its covers (blaze-cli engine_bodies.rs,
    /// work.rs `covered`) and a call of one finds its row. Empty for `Coverage::RootOnly`.
    pub locals: AVec<'w, ItemKey>,
    /// The ROOT body's checked record (blaze-typeck checked_record.rs, D163 R1), in `work`:
    /// `Some` for a root whose types are clean, whatever its borrows say (`keeps_typed`; front2
    /// borrows.rs `check_borrows_in` encodes it): the caller puts it as it is, under the body's
    /// check name.
    pub typed: Option<AVec<'w, u8>>,
    /// The root's types are clean but it has no checked record because its check left stalled
    /// goals, decided after the record (front2 stalled_goals.rs): the run meets a `TypedGap`
    /// for it. The run's census sums this flag over its bodies (the engine's Bodies stage, which
    /// reads `BodyReads`), so a tempo run says how many such gaps it has.
    pub typed_stalled: bool,
    bytes: AVec<'w, u8>,
    held: AVec<'w, (TableRead, u128)>,
    goal_hits: AVec<'w, blaze_solve::share::GoalHit>,
    /// What the one ask counted in the body's store (front2 asks.rs; zero under `Again`).
    asked: blaze_ask::Counts,
}

pub fn check_body_reads<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>) -> Result<BodyReads<'w>, TaskError> {
    record_body(tables, task, frame, work, Coverage::WithLocals, false)
}

/// [`check_body_reads`] while retaining the recorder for the caller when checking unwinds or
/// refuses with [`ReadsFull`].
pub fn check_body_reads_observed<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, recorder: &mut Option<Recorder<'w>>, keyer: Option<&dyn ReadKeyer>) -> Result<BodyReads<'w>, TaskError> {
    let active = recorder.get_or_insert_with(|| Recorder::new(RecorderOpts { alloc: work }));
    let body = record_body_observed(tables, task, frame, work, active, keyer, Coverage::WithLocals, ReadReport::Distinct)?;
    drop(recorder.take());
    Ok(body)
}

/// One item of an outline entry. Use the same immutable tables and placement policy
/// for every item sharing this recorder. Each item returns the cells it first observed
/// and its own verdict digest before the next item reuses the encoding buffer.
pub fn check_body_item<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, recorder: &mut Recorder<'w>, keyer: Option<&dyn ReadKeyer>, encoding: &mut AString<'w>) -> Result<BodyReads<'w>, TaskError> {
    record_body_observed(tables, task, frame, work, recorder, keyer, Coverage::WithLocals, ReadReport::Entry(encoding))
}

/// The one check behind [`check_body`], [`check_body_in`] and [`check_body_split`].
fn check_split<'n, 'a, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, work: Alloc<'w>, out: SharedAlloc<'a>, coverage: Coverage) -> Result<BodyChecked<'a, 'w>, TaskError> {
    let BodyReads { out: digest, failed, findings, reads, bytes, held, said, goal_hits, read_tally, work: mut work_bytes, typed, .. } = record_body(tables, task, frame, work, coverage, true)?;
    let mut made = SVec::new_in(out);
    let mut cells = AVec::with_capacity_in(reads.len(), work);
    for read in &reads {
        let Some(id) = cell_id(tables, *read)? else { continue };
        let saw = make.make(&id, &mut made).map_err(TaskError::Make)?;
        cells.push((id, saw));
    }
    cells.sort_unstable_by_key(|pair| pair.0);
    cells.dedup_by_key(|pair| pair.0);
    work_bytes.cells = (cells.capacity() * core::mem::size_of::<(CellId, ContentId)>()) as u64;
    Ok(BodyChecked { out: digest, failed, findings, cells, made, bytes, reads, held, said, goal_hits, read_tally, work: work_bytes, typed })
}

fn record_body<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, coverage: Coverage, sorted_report: bool) -> Result<BodyReads<'w>, TaskError> {
    let mut recorder = Recorder::new(RecorderOpts { alloc: work });
    let report = if sorted_report { ReadReport::Sorted } else { ReadReport::Distinct };
    record_body_observed(tables, task, frame, work, &mut recorder, None, coverage, report)
}

enum ReadReport<'a, 'w> {
    Sorted,
    Distinct,
    Entry(&'a mut AString<'w>),
}

fn record_body_observed<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, recorder: &mut Recorder<'w>, keyer: Option<&dyn ReadKeyer>, coverage: Coverage, report: ReadReport<'_, 'w>) -> Result<BodyReads<'w>, TaskError> {
    // D161 step 3's oracle over EVERY body any test checks (`--features asks-oracle`): the body
    // run both ways first, their answers equal or the test fails naming both.
    #[cfg(feature = "asks-oracle")]
    if let Err(differ) = compare_asks(tables, task, frame, work, super::super::asks::AsksPlant::Clean) {
        panic!("{differ}");
    }
    // The run memo's oracle over the same bodies (recorder.rs `Repeats`): a repeated question
    // muted must leave every body's read list byte-equal.
    #[cfg(feature = "asks-oracle")]
    if let Err(differ) = compare_repeats(tables, task, frame, work, RepeatsPlant::Clean) {
        panic!("{differ}");
    }
    record_body_as(tables, task, frame, work, recorder, keyer, coverage, report, super::super::asks::AsksMet::Again)
}

/// Two answers of one body, one per side of D161 step 3's seam (front2 asks.rs `AsksMet`), that
/// differ: the body, what the one ask made by fact, and both results (rule 19).
pub struct AsksDiffer<'w> {
    pub body: Name,
    pub again: BodyReads<'w>,
    pub once: BodyReads<'w>,
    /// The `Once` side's failure, where it failed and the `Again` side did not (or both).
    pub errors: (Option<TaskError>, Option<TaskError>),
}

impl core::fmt::Display for AsksDiffer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "D161 step 3 oracle: body {:?} answers differ between `AsksMet::Again` and `AsksMet::Once` (front2 asks.rs)", self.body)?;
        let asked = &self.once.asked;
        write!(f, "the one ask: ")?;
        for fact in blaze_ask::Fact::ALL {
            let i = fact.index();
            if asked.asked[i] + asked.made[i] > 0 {
                write!(f, "{} asked {} made {}; ", fact.name(), asked.asked[i], asked.made[i])?;
            }
        }
        writeln!(f, "read {} failed {}", asked.read, asked.failed)?;
        if let Some(e) = &self.errors.0 {
            writeln!(f, "again refused: {e}")?;
        }
        if let Some(e) = &self.errors.1 {
            writeln!(f, "once refused: {e}")?;
        }
        for (side, r) in [("again", &self.again), ("once", &self.once)] {
            writeln!(f, "--- {side}: out {:?} failed {} findings {} reads {} locals {}", r.out, r.failed, r.findings, r.reads.len(), r.locals.len())?;
            if let Some(at) = self.again.reads.iter().zip(self.once.reads.iter()).position(|(a, b)| a != b) {
                writeln!(f, "first read apart at {at}: {:?}", r.reads[at])?;
            }
            writeln!(f, "{}", core::str::from_utf8(&r.bytes).unwrap_or("(the verdict text is not UTF-8)"))?;
        }
        Ok(())
    }
}

impl core::fmt::Debug for AsksDiffer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(self, f)
    }
}

/// Whether the two sides of D161's seam agree on one body (the relation [`compare_asks`] holds):
/// - the same failure, and for a body that checked (clean or with findings) the same verdict
///   digest, findings, verdict bytes (which carry the final round's read-list digest), constant
///   texts, locals and typed record bytes;
/// - the `Once` side's read list a SUBSET of the `Again` side's (both sorted): under `Once`
///   over frozen heads the run stops at its first hole (asks.rs `Store::stops_at_holes`), so
///   every fill it makes is a question its final round asks; `Again` also keeps the reads of
///   the fills it made past a stand-in that its final round never asked (the superset the
///   module doc's INTERIM paragraph names);
/// - a body whose TYPES both sides checked clean (`typed` kept: [`keeps_typed`]) is held to all of
///   the above whatever its borrows say (a refused borrow check fails the task, both sides
///   alike, and its verdict bytes say so equally);
/// - for a body whose types a side did not check clean, the verdict bytes may differ only by
///   the gaps named: `Again` lists every gap of every round, speculative ones included; `Once`
///   the gaps met before its run stopped. Both must fail.
fn asks_agree(a: &BodyReads<'_>, o: &BodyReads<'_>) -> bool {
    let subset = o.reads.as_slice().iter().all(|read| a.reads.as_slice().binary_search(read).is_ok());
    let types_clean = a.typed.is_some() && o.typed.is_some();
    if (a.failed || o.failed) && !types_clean {
        return a.failed == o.failed && subset;
    }
    a.out == o.out
        && a.failed == o.failed
        && a.findings == o.findings
        && a.bytes.as_slice() == o.bytes.as_slice()
        && subset
        && a.const_texts.as_slice() == o.const_texts.as_slice()
        && a.locals.as_slice() == o.locals.as_slice()
        && a.typed.as_ref().map(|b| b.as_slice()) == o.typed.as_ref().map(|b| b.as_slice())
}

/// D161 step 3's ORACLE (rule 78: each instantiation of the seam tested equal): `task`'s body
/// checked under `AsksMet::Again` and under `AsksMet::Once(plant)`, each with a fresh recorder,
/// no keyer, no meter and no shared goal table (a shared table filled by the first run would
/// give the second hits the first never had), its read list sorted, over the frame's heads or,
/// for a frame with none, a fresh frozen seed a side ([`oracle_seed`]); the two agree as
/// [`asks_agree`] says, or both runs refused alike. A seed refusal of the oracle's own seed is
/// said on its own `asks-oracle SEED REFUSED` line and passes (counted apart, not a
/// difference). `plant` is the control's (`AsksPlant::RefuseMade` must differ on a seeded body
/// that imports a head).
pub fn compare_asks<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, plant: super::super::asks::AsksPlant) -> Result<(), AsksDiffer<'w>> {
    use super::super::asks::AsksMet;
    let run = |met: AsksMet, heads: Option<&'n dyn HeadSeed<'n>>| {
        let shadow = BodyFrame { frame: frame.frame, alloc: frame.alloc, plant_drop: frame.plant_drop, cost: None, needs: frame.needs, heads, sources: frame.sources, goals: None, prepared: frame.prepared };
        let mut recorder = Recorder::new(RecorderOpts { alloc: work });
        record_body_as(tables, task, &shadow, work, &mut recorder, None, Coverage::WithLocals, ReadReport::Sorted, met)
    };
    // An unseeded body (a lowering store, where `Once` would meet its heads as `Again` does)
    // gets frozen heads of its own, one fresh seed a side ([`oracle_seed`]), so the oracle
    // exercises the one ask on EVERY body. A head the seed cannot freeze (no constant
    // evaluator, receiver pointers not looked up: the record form's known limits) is a seed
    // refusal, said on its own line and counted apart, never a difference.
    let (again, once) = match frame.heads {
        Some(heads) => (run(AsksMet::Again, Some(heads)), run(AsksMet::Once(plant), Some(heads))),
        None => {
            let side = |met: AsksMet| {
                let seed = oracle_seed(tables, frame);
                let got = run(met, Some(seed as &'n dyn HeadSeed<'n>));
                release_oracle_seed(tables, frame, seed);
                got
            };
            let (again, once) = (side(AsksMet::Again), side(AsksMet::Once(plant)));
            if let Some(error) = [&again, &once].into_iter().find_map(|got| got.as_ref().err().filter(|e| matches!(e, TaskError::Seed { .. }))) {
                std::eprintln!("asks-oracle SEED REFUSED (counted apart, not a difference): {error}");
                return Ok(());
            }
            (again, once)
        }
    };
    let empty = || BodyReads {
        out: ContentId([0; 32]),
        failed: true,
        findings: 0,
        reads: AVec::new_in(work),
        said: AVec::new_in(work),
        read_tally: Default::default(),
        work: Default::default(),
        const_texts: AVec::new_in(frame.alloc),
        locals: AVec::new_in(work),
        typed: None,
        typed_stalled: false,
        bytes: AVec::new_in(work),
        held: AVec::new_in(work),
        goal_hits: AVec::new_in(work),
        asked: blaze_ask::Counts::default(),
    };
    let body = task.planned.name;
    match (again, once) {
        (Ok(a), Ok(o)) => {
            if asks_agree(&a, &o) { Ok(()) } else { Err(AsksDiffer { body, again: a, once: o, errors: (None, None) }) }
        }
        // Both refused: equal when they refuse with one text (`TaskError` has no `Eq`).
        (Err(a), Err(o)) => {
            let mut at = AString::new_in(work);
            let mut ot = AString::new_in(work);
            let shown = write!(at, "{a}").is_ok() && write!(ot, "{o}").is_ok();
            if shown && at.as_str() == ot.as_str() { Ok(()) } else { Err(AsksDiffer { body, again: empty(), once: empty(), errors: (Some(a), Some(o)) }) }
        }
        (Ok(a), Err(o)) => Err(AsksDiffer { body, again: a, once: empty(), errors: (None, Some(o)) }),
        (Err(a), Ok(o)) => Err(AsksDiffer { body, again: empty(), once: o, errors: (Some(a), None) }),
    }
}

/// Fresh frozen heads for one side of [`compare_asks`] over a body its frame gives none: the
/// tests' seed (front2_tables_cost.rs's `Freezing`) over the tables' records, with `sized`
/// from the lang crates, no constant evaluator and receiver pointers not looked up (a head
/// that needs either is a seed refusal, counted apart). Placed in the body's frame (`'n`),
/// whose bump never runs its destructor: [`release_oracle_seed`] drops what it froze.
fn oracle_seed<'n>(tables: Tables<'_, 'n>, frame: &BodyFrame<'n>) -> &'n super::seeds::Freezing<'n, 'n, &'n Records<'n>> {
    let names = tables.records();
    frame.frame.alloc(super::seeds::Freezing::new(names, oracle_heads(tables, frame)))
}

/// The empty frozen heads [`oracle_seed`] starts from.
fn oracle_heads<'n>(tables: Tables<'_, 'n>, frame: &BodyFrame<'n>) -> super::head::SeededHeads<'n, 'n, &'n Records<'n>> {
    let names = tables.records();
    let mut sized = None;
    for &at in tables.lang_crates() {
        NamesSource::each_lang(names, at, &mut |name, key| {
            if name == "sized" {
                sized = Some(key);
            }
        });
    }
    super::head::SeededHeads::new(super::head::SeededHeadsOpts { names, sized, pointers: super::super::receiver::Pointers::NOT_LOOKED_UP, frame: frame.frame, scratch: frame.alloc, alloc: frame.alloc, consts: None, known: None, owners: None })
}

/// Drops what an [`oracle_seed`] froze (its cells are in `frame.alloc`, which the frame's bump
/// would never free): its heads are swapped for empty ones, which hold nothing.
fn release_oracle_seed<'n>(tables: Tables<'_, 'n>, frame: &BodyFrame<'n>, seed: &super::seeds::Freezing<'n, 'n, &'n Records<'n>>) {
    drop(seed.heads.replace(oracle_heads(tables, frame)));
}

/// The enter-once cost test's entry (front2 tables/enter_once_tests.rs): `task`'s body on one
/// side of D161's seam, `met`, with the frame as given (its meter included, which
/// [`compare_asks`] drops), a fresh recorder and its read list sorted.
#[cfg(test)]
pub(crate) fn record_body_met<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, met: super::super::asks::AsksMet) -> Result<BodyReads<'w>, TaskError> {
    let mut recorder = Recorder::new(RecorderOpts { alloc: work });
    record_body_as(tables, task, frame, work, &mut recorder, None, Coverage::WithLocals, ReadReport::Sorted, met)
}

#[cfg(test)]
impl<'w> BodyReads<'w> {
    /// The verdict's bytes (what `out` digests).
    pub(crate) fn verdict_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// What the one ask counted in the body's store (zero under `Again`).
    pub(crate) fn asked_counts(&self) -> blaze_ask::Counts {
        self.asked
    }

    /// The head read of each head the body's store held after the check, with its key.
    pub(crate) fn held_heads(&self) -> &[(TableRead, u128)] {
        self.held.as_slice()
    }

    /// Whether the two sides of D161's seam agree on this body ([`asks_agree`]): `self` the
    /// `Again` side's, `once` the `Once` side's, both with sorted read lists.
    pub(crate) fn agrees_with_once(&self, once: &BodyReads<'_>) -> bool {
        asks_agree(self, once)
    }

    /// What of [`asks_agree`]'s relation fails between `self` (the `Again` side) and `once`, by
    /// field: each a line naming the field and how it differs (the first byte apart, the
    /// lengths, the `Once` reads missing from the `Again` list). Empty when they agree.
    pub(crate) fn once_differs(&self, once: &BodyReads<'_>) -> std::string::String {
        let mut s = std::string::String::new();
        let first_apart = |a: &[u8], b: &[u8]| a.iter().zip(b.iter()).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
        if self.out != once.out {
            let _ = writeln!(s, "out: {:?} against {:?}", self.out, once.out);
        }
        if self.failed != once.failed {
            let _ = writeln!(s, "failed: {} against {}", self.failed, once.failed);
        }
        if self.findings != once.findings {
            let _ = writeln!(s, "findings: {} against {}", self.findings, once.findings);
        }
        let (ab, ob) = (self.bytes.as_slice(), once.bytes.as_slice());
        if ab != ob {
            let at = first_apart(ab, ob);
            let near = |b: &[u8]| std::string::String::from_utf8_lossy(&b[at.saturating_sub(40).min(b.len())..(at + 80).min(b.len())]).into_owned();
            let _ = writeln!(s, "verdict bytes: {} against {} bytes, first apart at {at}: again `{}` once `{}`", ab.len(), ob.len(), near(ab), near(ob));
        }
        let missing: std::vec::Vec<&TableRead> = once.reads.as_slice().iter().filter(|r| self.reads.as_slice().binary_search(r).is_err()).collect();
        if !missing.is_empty() {
            let _ = writeln!(s, "reads: {} of the one ask's {} are not in the restart path's {} (the first: {:?})", missing.len(), once.reads.len(), self.reads.len(), missing.first());
        }
        if self.const_texts.as_slice() != once.const_texts.as_slice() {
            let _ = writeln!(s, "const texts: {} against {}", self.const_texts.len(), once.const_texts.len());
        }
        if self.locals.as_slice() != once.locals.as_slice() {
            let _ = writeln!(s, "locals: {} against {}", self.locals.len(), once.locals.len());
        }
        match (self.typed.as_ref().map(|b| b.as_slice()), once.typed.as_ref().map(|b| b.as_slice())) {
            (Some(a), Some(o)) if a != o => {
                let _ = writeln!(s, "typed record: {} against {} bytes, first apart at {}", a.len(), o.len(), first_apart(a, o));
            }
            (a, o) if a.is_some() != o.is_some() => {
                let _ = writeln!(s, "typed record: kept {} against {}", a.is_some(), o.is_some());
            }
            _ => {}
        }
        s
    }
}

/// The planted bug of [`compare_repeats`]'s control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatsPlant {
    Clean,
    /// PLANTED BUG: the `Quiet` side drops every read of the first names cell the `Offer`
    /// side kept, as a memo that muted a first answer would. The oracle must see the lists
    /// differ.
    DropFirst,
}

/// Two read lists of one body, one per side of the recorder's repeats seam (recorder.rs
/// `Repeats`), that differ: the body, the first read apart, and both tallies (rule 19).
pub struct RepeatsDiffer<'w> {
    pub body: Name,
    pub offer: Result<BodyReads<'w>, TaskError>,
    pub quiet: Result<BodyReads<'w>, TaskError>,
}

impl core::fmt::Display for RepeatsDiffer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "repeats oracle: body {:?} reads differ between `Repeats::Offer` and `Repeats::Quiet` (front2 tables/recorder.rs)", self.body)?;
        match (&self.offer, &self.quiet) {
            (Ok(o), Ok(q)) => {
                writeln!(f, "offer: {} reads, out {:?}, tally {:?}", o.reads.len(), o.out, o.read_tally)?;
                writeln!(f, "quiet: {} reads, out {:?}, tally {:?}", q.reads.len(), q.out, q.read_tally)?;
                match o.reads.iter().zip(q.reads.iter()).position(|(a, b)| a != b) {
                    Some(at) => writeln!(f, "first read apart at {at}: offer {:?}, quiet {:?}", o.reads[at], q.reads[at]),
                    None => writeln!(f, "one list is a prefix of the other"),
                }
            }
            (o, q) => {
                for (side, r) in [("offer", o), ("quiet", q)] {
                    match r {
                        Ok(r) => writeln!(f, "{side}: {} reads, out {:?}", r.reads.len(), r.out)?,
                        Err(e) => writeln!(f, "{side} refused: {e}")?,
                    }
                }
                Ok(())
            }
        }
    }
}

impl core::fmt::Debug for RepeatsDiffer<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(self, f)
    }
}

/// THE ORACLE of the run memo (rule 78: each side of the seam tested equal): `task`'s body
/// checked with a recorder under `Repeats::Offer` (every repeated question's reads offered
/// again) and one under `Repeats::Quiet` (none), each fresh, with no keyer, no meter and no
/// shared goal table, its read list sorted: equal verdict digests, failure, findings, verdict
/// bytes and READ LISTS byte for byte, or both refused with one text. The `Quiet` side's
/// tally has `memo` > 0 where a run asked a question twice. `plant` is the control's
/// ([`RepeatsPlant::DropFirst`] must differ on any body that reads a names cell). Equal: the
/// two sides' tallies, `Offer`'s then `Quiet`'s (zero for two equal refusals).
pub fn compare_repeats<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, plant: RepeatsPlant) -> Result<(super::recorder::ReadTally, super::recorder::ReadTally), RepeatsDiffer<'w>> {
    use super::recorder::Repeats;
    let shadow = BodyFrame { frame: frame.frame, alloc: frame.alloc, plant_drop: frame.plant_drop, cost: None, needs: frame.needs, heads: frame.heads, sources: frame.sources, goals: None, prepared: frame.prepared };
    let run = |repeats: Repeats, drop: Option<NameCell>| {
        let mut recorder = Recorder::new(RecorderOpts { alloc: work });
        recorder.set_repeats(repeats);
        if let Some(cell) = drop {
            recorder.plant_drop(cell);
        }
        record_body_as(tables, task, &shadow, work, &mut recorder, None, Coverage::WithLocals, ReadReport::Sorted, super::super::asks::AsksMet::Again)
    };
    let offer = run(Repeats::Offer, None);
    let drop = match (plant, &offer) {
        (RepeatsPlant::DropFirst, Ok(o)) => o.reads.iter().find_map(|r| match r {
            TableRead::Names(cell) => Some(*cell),
            _ => None,
        }),
        _ => None,
    };
    let quiet = run(Repeats::Quiet, drop);
    let equal = match (&offer, &quiet) {
        (Ok(a), Ok(o)) => a.out == o.out && a.failed == o.failed && a.findings == o.findings && a.bytes.as_slice() == o.bytes.as_slice() && a.reads.as_slice() == o.reads.as_slice(),
        (Err(a), Err(o)) => {
            let mut at = AString::new_in(work);
            let mut ot = AString::new_in(work);
            write!(at, "{a}").is_ok() && write!(ot, "{o}").is_ok() && at.as_str() == ot.as_str()
        }
        _ => false,
    };
    if !equal {
        return Err(RepeatsDiffer { body: task.planned.name, offer, quiet });
    }
    Ok(match (&offer, &quiet) {
        (Ok(o), Ok(q)) => (o.read_tally, q.read_tally),
        _ => Default::default(),
    })
}

/// The body's world over `view`: its lang index, its `Front2` and the world itself, made in
/// THIS frame and boxed in `alloc`. A debug build gives each local and temporary of a
/// function its own slot for the whole call, and [`record_body_as`]'s frame stays on the
/// stack under the whole type and borrow check (blaze-int notes t29 overflow-bt4.txt: 156,400
/// bytes, the deepest of front2's): made there, the index, `Front2` and the world's value
/// would ride under it; made here, only the box's pointer does.
#[inline(never)]
fn body_world<'v, 'n, 'w>(view: Tables<'v, 'n>, name: Name, frame: &BodyFrame<'n>, asks: super::super::asks::AsksMet, alloc: Alloc<'w>) -> Result<blaze_own::Box<ProjectWorld<'n, Tables<'v, 'n>>, Alloc<'w>>, TaskError> {
    let lang_crates = view.lang_crates();
    let index = Index::from_source(view, lang_crates).map_err(TaskError::Lang)?;
    view.records().name_lang_crates(lang_crates);
    let front = Front2 { names: view, resolver: crate::resolve::Paths { names: view }, texts: Texts::empty(), keys: Keys::new(), index, frame: frame.frame };
    let mut world = blaze_own::Box::new_in(ProjectWorld::over_with(front, frame.prepared, frame.alloc), alloc);
    world.record_const_texts();
    world.set_cost(frame.cost);
    world.set_needs(frame.needs);
    world.set_asks(asks).map_err(|why| TaskError::Asks { body: name, why })?;
    world.set_seeds(frame.heads);
    world.set_goal_share(frame.goals);
    if let Some(sources) = frame.sources {
        world.set_body_sources(sources);
    }
    Ok(world)
}

const _: () = assert!(core::mem::size_of::<blaze_own::Box<ProjectWorld<'static, Tables<'static, 'static>>, Alloc<'static>>>() == 2 * core::mem::size_of::<usize>());

/// [`record_body_observed`] on one side of D161 step 3's seam (`asks`).
#[allow(clippy::too_many_arguments)]
fn record_body_as<'n, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, work: Alloc<'w>, recorder: &mut Recorder<'w>, keyer: Option<&dyn ReadKeyer>, coverage: Coverage, report: ReadReport<'_, 'w>, asks: super::super::asks::AsksMet) -> Result<BodyReads<'w>, TaskError> {
    let name = task.planned.name;
    let setup = Cost::enter(frame.cost, Part::Setup);
    let alloc = work;
    if let Some(cell) = frame.plant_drop {
        recorder.plant_drop(cell);
    }
    let item_keyer = keyer.filter(|keyer| !keyer.keeps_item_keys());
    let observed = ObservedReads { recorder, tables, keyer, item_keyer, error: RefCell::new(None) };
    recorder.checking(Some("the body's lookup and lowering tables"));
    let view = tables.reading_into(&observed);
    let body = body_of(view, name)?;
    // The item whose head holds the body's signature: a member's is its container's.
    let own = match body {
        BodyRef::Member { container, .. } => container,
        BodyRef::Free(key) => key,
    };
    // In its own frame and behind a box: this frame rides under the whole check (the guard:
    // tests/front2_alias_opaques.rs `a_seeded_body_checks_on_a_small_stack`).
    let mut world = body_world(view, name, frame, asks, alloc)?;
    drop(setup);
    let root = Listed { body, at: None, entry: None };
    let mut goal_hits = AVec::new_in(alloc);
    let mut seed_error = None;
    let check = |w: &mut ProjectWorld<'n, Tables<'_, 'n>>, b| {
        recorder.checking(Some("the body's type and borrow check"));
        let pair = w.check_borrows(b, frame.alloc);
        if let Some(error) = w.take_seed_error() {
            seed_error.get_or_insert_with(|| {
                if let SeedError::Freeze(error @ FreezeError::Record { .. }) = error {
                    if let FreezeError::Record { error: super::super::head_record::HeadRecordError::MissingOpaqueRow { key, origin }, .. } = &error {
                        let ask = CellAsk::during_seed(&pair.0, *key);
                        let shown = FactDefinition::of(view.records(), *key, Some((origin.owner, origin.index)), frame.alloc);
                        return TaskError::Cell { key: *key, ask, holder: origin.owner, shown, error };
                    }
                    return TaskError::Seed { body: name, error: SeedError::Freeze(error) };
                }
                TaskError::Seed { body: name, error }
            });
        }
        recorder.checking(Some("the body's shared goal hit list"));
        goal_hits.extend_from_slice(w.goal_hits());
        recorder.checking(Some("the body's reached verdict and local function lists"));
        pair
    };
    recorder.checking(Some("the body's reached verdict and local function lists"));
    let reached = match coverage {
        Coverage::WithLocals => world.each_from(&[root], alloc, check),
        #[cfg(test)]
        Coverage::RootOnly => {
            let mut check = check;
            let mut one = AVec::new_in(alloc);
            one.push(Reached { listed: root, within: None, got: check(&mut *world, body) });
            one
        }
    };
    if let Some(error) = seed_error {
        return Err(error);
    }
    let sorted_report = matches!(report, ReadReport::Sorted);
    let done = collect_body_reads(name, own, frame, alloc, &observed, view, world, reached, report)?;
    let recorder_bytes = recorder.table_bytes();
    let (mut reads, mut read_tally) = recorder.take_item().map_err(TaskError::Full)?;
    recorder.checking(None);
    if sorted_report {
        reads.sort_unstable_by(|a, b| {
            read_tally.sort_compares += 1;
            a.cmp(b)
        });
    }
    let work_bytes = WorkBytes {
        recorder: recorder_bytes,
        recorder_floor: byte_size(reads.len(), core::mem::size_of::<TableRead>()),
        cells: 0,
        held: done.held_bytes,
        said: done.said_bytes,
        verdict: byte_size(done.verdict_capacity, 1),
        goal_hits: byte_size(goal_hits.capacity(), core::mem::size_of::<blaze_solve::share::GoalHit>()),
    };
    Ok(BodyReads {
        out: done.digest,
        failed: done.failed,
        findings: done.findings,
        bytes: done.bytes,
        reads,
        const_texts: done.const_texts,
        locals: done.locals,
        typed: done.typed,
        typed_stalled: done.typed_stalled,
        held: done.held,
        said: done.said,
        goal_hits,
        read_tally,
        work: work_bytes,
        asked: done.asked,
    })
}

fn byte_size(n: usize, each: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX).saturating_mul(u64::try_from(each).unwrap_or(u64::MAX))
}

struct CollectedBody<'w> {
    digest: ContentId,
    failed: bool,
    findings: u32,
    bytes: AVec<'w, u8>,
    const_texts: AVec<'static, TableRead>,
    locals: AVec<'w, ItemKey>,
    typed: Option<AVec<'w, u8>>,
    typed_stalled: bool,
    held: AVec<'w, (TableRead, u128)>,
    said: AVec<'w, Said<'w>>,
    asked: blaze_ask::Counts,
    verdict_capacity: usize,
    held_bytes: u64,
    said_bytes: u64,
}

/// Outlined post-check summary for [`record_body_as`] so its locals and temporaries do not stay
/// on the stack under `world.each_from` (keeping small-stack body checks well under 512 KiB).
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "outlined tail of record_body_as")]
fn collect_body_reads<'v, 'n, 'w>(
    name: Name,
    own: ItemKey,
    frame: &BodyFrame<'n>,
    alloc: Alloc<'w>,
    observed: &'v ObservedReads<'_, 'w, '_, 'n, '_>,
    view: Tables<'v, 'n>,
    mut world: blaze_own::Box<ProjectWorld<'n, Tables<'v, 'n>>, Alloc<'w>>,
    reached: AVec<'w, Reached<(BodyVerdict<'static>, BorrowVerdict)>>,
    report: ReadReport<'_, 'w>,
) -> Result<CollectedBody<'w>, TaskError> {
    let recorder = observed.recorder;
    // Each fn declared in a reached body: checked here, so covered by this check. A local is
    // always written `Free` (every_body.rs `each_from`); the root is the one with no `within`.
    recorder.checking(Some("the body's local function key list"));
    let mut locals: AVec<'w, ItemKey> = AVec::with_capacity_in(reached.len().saturating_sub(1), alloc);
    for one in reached.iter().filter(|one| one.within.is_some()) {
        if let BodyRef::Free(key) = one.listed.body {
            locals.push(key);
        }
    }
    let reading = Cost::enter(frame.cost, Part::Reads);
    let mut owned_bytes = AString::new_in(alloc);
    let sorted_report = matches!(report, ReadReport::Sorted);
    let bytes = match report {
        ReadReport::Entry(encoding) => encoding,
        ReadReport::Distinct | ReadReport::Sorted => &mut owned_bytes,
    };
    bytes.clear();
    let mut said = AVec::new_in(alloc);
    let mut failed = false;
    let mut findings = 0u32;
    for (i, one) in reached.iter().enumerate() {
        recorder.checking(Some("the verdict's dependency facts"));
        let (verdict, borrows) = &one.got;
        let facts = verdict.facts.as_ref().map_err(|e| TaskError::Facts(*e))?;
        // An internal-symbol declaration the body named was bound by searching crates' items:
        // that search is the body's read (front2 internal_symbol.rs `record_search`).
        super::super::internal_symbol::record_search(view, &world.store, facts);
        for row in facts.rows.iter() {
            if !fact_kind_has_read(row.fact.kind) || row.fact.key == 0 {
                continue;
            }
            // By the records, unrecorded: which cell holds a fact is the plan's, not the body's.
            // An opaque type's facts are its fn's head's (the fn's head files it); the default
            // of a provided trait fn's `impl Trait` is that fn's (front2 rpitit.rs `filed_as`).
            let asked = blaze_typeck::ItemKey(row.fact.key);
            let owner = world.store.opaques.get(&super::super::rpitit::filed_as(&world.store, asked)).copied();
            if let Some(read) = fact_read(view.records(), &world.front.keys, row.fact.kind, row.fact.key, owner, frame.alloc)? {
                observed.read(read);
            }
        }
        recorder.checking(Some("the body's diagnostic lines and their words"));
        said.extend(said_of(verdict, borrows, view.records(), &world.front.keys, || local_path(&reached[..i], one.within, alloc), alloc)?);
        // The root has precisely its original bytes. Each local appends another complete verdict.
        recorder.checking(Some("the body's encoded verdict bytes"));
        write_verdict(bytes, verdict, borrows, Facts::Digest)?;
        failed |= verdict.refused.is_some() || !verdict.gaps.is_empty() || matches!(borrows.refused, Some(ref r) if !matches!(r, BorrowRefused::Untyped(_)));
        let count = verdict.diagnostics.len() + verdict.findings.len() + borrows.findings.len();
        findings = findings.saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
    }
    // THE BODY'S OWN HEAD (rule 21): its signature (a member's: its container's cell) is an
    // input of every check of it, also one refused before any question (a trait member's
    // missing default body), so it is read whether or not the checker asked for it.
    recorder.checking(Some("the body's own head fact"));
    let own = super::super::keys::checker_of(own);
    if let Some(read) = fact_read(view.records(), &world.front.keys, FactKind::Head, own.0, None, frame.alloc)? {
        observed.read(read);
    }
    // THE CONTROL'S INSTRUMENT: the cell of each head the body's own store holds, beside the
    // read list (tests: every one must be in it; the store makes a head only when asked).
    recorder.checking(Some("the body's held head list"));
    let mut held_keys: AVec<'_, u128> = AVec::new_in(alloc);
    if sorted_report {
        world.each_held_head(&mut |k| held_keys.push(k));
    }
    let mut held: AVec<'w, (TableRead, u128)> = AVec::with_capacity_in(held_keys.len(), alloc);
    for &k in held_keys.iter() {
        if let Some(read) = fact_read(view.records(), &world.front.keys, FactKind::Head, k, None, frame.alloc)? {
            held.push((read, k));
        }
    }
    held.sort_unstable();
    recorder.checking(Some("the body's constant definition reads"));
    let const_texts = world.take_const_texts().map_err(TaskError::Full)?;
    let asked = *world.store.asked.borrow();
    drop(world);
    let _ = view;
    if let Some(error) = observed.error.borrow_mut().take() {
        return Err(error);
    }
    // The root's verdict bytes, then each local fn's (a root with no local fn keeps its bytes).
    let digest = ContentId::of(OUT_DOMAIN, bytes.as_bytes());
    let verdict_capacity = bytes.capacity();
    let bytes = owned_bytes.into_bytes();
    let held_bytes = byte_size(held_keys.capacity(), core::mem::size_of::<u128>()) + byte_size(held.capacity(), core::mem::size_of::<(TableRead, u128)>());
    let said_bytes = byte_size(said.capacity(), core::mem::size_of::<Said<'_>>())
        + said.iter().map(|s| byte_size(s.words.capacity(), 1) + s.within.as_ref().map_or(0, |w| byte_size(w.capacity(), 1)) + if let SaidWhat::NotChecked { cause } = &s.what { byte_size(cause.capacity(), 1) } else { 0 }).sum::<u64>();
    drop(held_keys);
    drop(reading);
    // The root's checked record (D163 R1), copied into `work` beside the rest of the answer, of
    // every root whose TYPES are clean ([`keeps_typed`]); a clean check the record refused
    // fails the task by name (rule 3).
    let root_reached = reached.iter().find(|one| one.within.is_none());
    let clean = root_reached.filter(|one| keeps_typed(&one.got.0));
    let typed = match clean.and_then(|one| one.got.0.typed.as_ref()) {
        Some(Ok(record)) => {
            let mut kept = AVec::with_capacity_in(record.len(), alloc);
            kept.extend_from_slice(record);
            Some(kept)
        }
        // Its stalled goals were not all proved on their exact interiors (front2 stalled_goals.rs;
        // proved ones keep the record, borrows.rs `check_borrows_in`, F18): a `TypedGap` for
        // the run, counted (`typed_stalled`), never a failed task (D163).
        Some(Err(blaze_typeck::checked_record::CheckedRecordError::NotClean { what: "stalled", .. })) => None,
        Some(Err(why)) => return Err(TaskError::Typed { body: name, why: *why }),
        None => None,
    };
    let typed_stalled = matches!(clean.and_then(|one| one.got.0.typed.as_ref()), Some(Err(blaze_typeck::checked_record::CheckedRecordError::NotClean { what: "stalled", .. })));
    // `findings` summed over the root and its local fns (u32, saturating), as the row's.
    Ok(CollectedBody { digest, failed, findings, bytes, const_texts, locals, typed, typed_stalled, held, said, asked, verdict_capacity, held_bytes, said_bytes })
}

/// Whether a root's checked record is kept (D163 R1, D197 root 1): when its TYPES are clean (no
/// diagnostic, no gap, no refusal), whatever its borrows say. The record holds the type
/// checker's columns, which a borrow verdict never changes. A body whose borrows were refused
/// (D151 `Typed`: types whole, borrows not checked) runs (blaze-cli run_ids.rs
/// `Checked::runs`), so it needs its record; that kind was checked a second time by every run
/// before (the borrow filter here). A body with a borrow finding or a lint finding keeps its
/// record too: whether it runs is the run gate's question, asked by its class, not this one.
fn keeps_typed(types: &BodyVerdict<'_>) -> bool {
    types.is_clean()
}

/// A local fn's path below the task root. The work list puts every parent before its children.
fn local_path<'n, 'a, T>(earlier: &[Reached<'n, T>], within: Option<(BodyRef<'n>, &'n str)>, alloc: Alloc<'a>) -> Option<AString<'a>> {
    let (mut parent, leaf) = within?;
    let root = earlier[0].listed.body;
    if parent == root {
        return Some(AString::from_str_in(leaf, alloc));
    }
    let mut names = AVec::new_in(alloc);
    let mut ancestors = earlier.iter().rev();
    while parent != root {
        let ancestor = ancestors.find(|r| r.listed.body == parent).expect("each_from visits the parent before the local fn");
        let (outer, name) = ancestor.within.expect("each_from marks every non-root fn with its parent");
        names.push(name);
        parent = outer;
    }
    let mut path = AString::new_in(alloc);
    for name in names.iter().rev() {
        path.push_str(name);
        path.push_str("::");
    }
    path.push_str(leaf);
    Some(path)
}

/// The lines `check` prints for a verdict (blaze-cli body_types.rs, today's path), in `alloc`.
fn said_of<'n, 'a>(verdict: &BodyVerdict<'_>, borrows: &BorrowVerdict<'_>, names: &'n Records<'n>, keys: &Keys, local_path: impl Fn() -> Option<AString<'a>>, alloc: Alloc<'a>) -> Result<AVec<'a, Said<'a>>, TaskError> {
    let text = |s: &str| -> AString<'a> {
        let mut t = AString::new_in(alloc);
        t.push_str(s);
        t
    };
    let mut said: AVec<'a, Said<'a>> = AVec::new_in(alloc);
    let mut push = |what, start, end, words| {
        let within = match said.last() {
            Some(previous) => previous.within.clone(),
            None => local_path(),
        };
        said.push(Said { within, what, start, end, words });
    };
    for d in verdict.diagnostics.iter() {
        push(SaidWhat::Type { code: d.code }, d.start, d.end, text(d.text.as_str()));
    }
    for d in verdict.findings.iter() {
        push(SaidWhat::Lint { lint: d.lint }, d.start, d.end, text(d.text.as_str()));
    }
    if let Some(refused) = &verdict.refused {
        let cause = super::super::said::refused_cause(names, keys, refused);
        let words = super::super::said::refused_words(names, refused);
        push(SaidWhat::NotChecked { cause: text(&cause) }, 0, 0, text(&words));
    } else if let Some(words) = super::super::said::gap_words(&verdict.gaps) {
        push(SaidWhat::NotChecked { cause: text(super::super::said::GAP_CAUSE) }, 0, 0, text(&words));
    }
    // The three parts of a borrow finding (rule 19): the conflicting action is the finding's
    // own range and words; the borrow and the later use follow, item-relative (rule 71) until
    // the printer places them (blaze-cli engine_bodies.rs `found_of`).
    for d in borrows.findings.iter() {
        let mut words = text(d.text);
        if let Some((start, end)) = d.borrowed {
            write!(words, "; the borrow is taken at {start}..{end}").map_err(|_| TaskError::Write)?;
        }
        match d.kept {
            Some(KeptAt::Use { start, end }) => write!(words, "; the borrow is later used at {start}..{end}"),
            Some(KeptAt::Signature) => write!(words, "; the borrow must outlive the call: a lifetime of the signature holds it"),
            Some(KeptAt::Unfound) => write!(words, "; the use that keeps the borrow live was not found (blaze-flow loans.rs `loan_parts`: an origin holding it is live, yet no use and no signature lifetime was reached)"),
            None => Ok(()),
        }
        .map_err(|_| TaskError::Write)?;
        if d.borrowed.is_some() {
            words.push_str(" (byte ranges within the item)");
        }
        push(SaidWhat::Borrow { code: d.code }, d.start, d.end, words);
    }
    if let Some(refused) = &borrows.refused
        && !matches!(refused, BorrowRefused::Untyped(_))
    {
        let (start, end) = refused.range().unwrap_or((0, 0));
        let mut words = AString::new_in(alloc);
        match refused {
            BorrowRefused::Bound(bound @ blaze_flow::Bound::TierTwo { function, .. }) =>
                write!(words, "{}", bound.in_function(ItemShown::of(names, keys, *function))),
            // Rule 19: an unknown variance names its ADT by path; the `[Unified/VarianceUnknown]`
            // class prefix stays first, so refusals still split by it.
            BorrowRefused::Stricter(step) => match step.form {
                blaze_flow_lower::Form::Unified { why: blaze_flow_lower::Unified::VarianceUnknown { adt } } =>
                    write!(words, "{refused} (variance of `{}` unknown)", ItemShown::of(names, keys, adt)),
                _ => write!(words, "{refused}"),
            },
            _ => write!(words, "{refused}"),
        }.map_err(|_| TaskError::Write)?;
        push(SaidWhat::BorrowsNotChecked, start, end, words);
    }
    Ok(said)
}

/// Whether [`verdict_bytes`] holds the digest of the body's facts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Facts {
    Digest,
    /// Left out: to compare two impl indexes whose lookups read other buckets for one answer.
    Left,
}

/// A body's verdict and borrow verdict as canonical bytes: what a body result's `out` digests.
pub fn verdict_bytes<'a>(verdict: &BodyVerdict<'_>, borrows: &BorrowVerdict<'_>, facts: Facts, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, TaskError> {
    let mut text = AString::new_in(alloc);
    write_verdict(&mut text, verdict, borrows, facts)?;
    Ok(text.into_bytes())
}

fn write_verdict(text: &mut AString<'_>, verdict: &BodyVerdict<'_>, borrows: &BorrowVerdict<'_>, facts: Facts) -> Result<(), TaskError> {
    write!(text, "diagnostics {:?}\nfindings {:?}\ngaps {:?}\nrefused {:?}\nborrows {:?}\n", verdict.diagnostics, verdict.findings, verdict.gaps, verdict.refused, borrows).map_err(|_| TaskError::Write)?;
    if facts == Facts::Digest {
        write!(text, "reads {:?}\n", verdict.reads).map_err(|_| TaskError::Write)?;
    }
    Ok(())
}

/// The body a `BodyCheck` name's subject (an item key) is, found by its item's cells.
fn body_of<'n>(view: Tables<'_, 'n>, name: Name) -> Result<BodyRef<'n>, TaskError> {
    if name.kind != Kind::BodyCheck {
        return Err(TaskError::NotABody { name });
    }
    let key = ItemKey(name.subject);
    // A member: its kind is its row in the owner's members cell (no items row of its own).
    if let Some((owner, member)) = view.member_owner(key) {
        let is_trait = view.kind_of_item(owner) == Some(ItemKind::Trait);
        let mut kind = None;
        view.each_member(owner, &mut |m| {
            if m.key == key {
                kind = Some(m.kind);
            }
        });
        let kind = match kind {
            Some(ItemKind::Function) => MemberKind::Fn,
            Some(ItemKind::Const) => MemberKind::Const,
            _ => return Err(TaskError::NotABody { name }),
        };
        return Ok(BodyRef::Member { container: owner, is_trait, kind, name: member });
    }
    match view.kind_of_item(key).ok_or(TaskError::NotABody { name })? {
        ItemKind::Function | ItemKind::Const | ItemKind::Static => Ok(BodyRef::Free(key)),
        _ => Err(TaskError::NotABody { name }),
    }
}

/// The read a body fact is beyond what the view recorded: a fact of an item's head (its
/// signature, predicates, ADT, impl header, constant, ..) by the item's checker key, and an
/// impl table bucket. Names facts (paths, traits in scope, lang paths) were recorded by the
/// view as asked; a fact of the body's own closures and async blocks is its own.
///
/// THE CELL is the one that holds the fact, by the key's class (by_origin.rs [`FactHolder`],
/// told once over every kind):
/// - a kind with a head: its head cell (head.rs [`fact_cell`], FI's one home): a
///   constructor's is its ADT's, a trait member's its trait's, an impl constant's its impl's.
///   ONE PART: a head cell is read whole (head.rs `cell_id`, part 0), whatever the fact's kind;
/// - a kind with no head (a module, a `use`, a macro, ..): its names cell `Item { key }`,
///   which holds all a check takes of it (that it is there, and of which kind). Before, such a
///   key was asked for a head cell no want makes (`NoHead(.., Module)`) and the body refused;
/// - an anonymous constant: no cell. Its fact is a function of its key and of reads the task
///   records anyway ([`FactHolder::OwnKey`]). Before, its fact was sent to its MODULE's head
///   cell (`KeyKind::Anon`'s `module`), which no want makes: every body whose store filed an
///   anonymous constant (an array length or a repeat count written as an expression, in its
///   own text or in a head or constant it lowered) was refused, 467 of them on the
///   1,100-crate workspace (g190500).
fn fact_kind_has_read(kind: FactKind) -> bool {
    use FactKind as K;
    matches!(
        kind,
        K::Head
            | K::Predicates
            | K::Adt
            | K::ImplHeader
            | K::AssocItem
            | K::Const
            | K::Opaque
            | K::Attrs
            | K::TraitDef
            | K::AssocParent
            | K::ConstHolds
            | K::ConstParams
            | K::AssocGenerics
            | K::BorrowAdt
            | K::BorrowHead
            | K::TypeAlias
            | K::FieldDefaults
            | K::MemberVisible
            | K::MemberUnstable
    )
}

fn fact_read<'n>(names: &'n Records<'n>, keys: &Keys, kind: FactKind, key: u128, owner: Option<(blaze_typeck::ItemKey, u32)>, alloc: Alloc<'_>) -> Result<Option<TableRead>, TaskError> {
    if !fact_kind_has_read(kind) || key == 0 {
        return Ok(None);
    }
    let key = blaze_typeck::ItemKey(key);
    let fact = owner.map_or(key, |(holder, _)| holder);
    let refused = |error: FreezeError| TaskError::Cell { key, ask: CellAsk::Fact(kind), holder: fact, shown: FactDefinition::of(names, key, owner, alloc), error };
    let class = kind_of_key(names, keys, fact).map_err(|error| refused(FreezeError::NoWant { key: fact, error }))?;
    match fact_holder(class) {
        // A block item's head comes from the enclosing body's text. No Signature task
        // publishes it; reads made while checking it go to this task's recorder.
        FactHolder::Head if keys.local_kind(fact).is_some() => Ok(None),
        FactHolder::Head => {
            let want = fact_cell(names, keys, fact).map_err(refused)?;
            let id = head_cell_id(want);
            let mut item = [0u8; 16];
            item.copy_from_slice(&id.of.subject.0[..16]);
            Ok(Some(TableRead::Head { item, part: 0 }))
        }
        FactHolder::Names { lib, .. } => Ok(Some(TableRead::Names(NameCell::Item { key: lib }))),
        FactHolder::OwnKey => Ok(None),
    }
}

/// A read as the engine names it (recorder.rs [`ReadOf`]): a names read (the lang rows too) is a cell of
/// its crate's `CrateScope` record, a head read a cell of the item's `Signature`, an impl read
/// a cell of its crate's `ImplBucket` table (all zero: the fact of the whole graph).
pub fn cell_id(view: Tables<'_, '_>, read: TableRead) -> Result<Option<CellId>, TaskError> {
    let records = view.records();
    // RecordItems indexes crate keys by their first eight bytes. An impl table carries
    // sixteen, so also verify its full prefix against the indexed crate.
    let crate_by = |prefix: &[u8]| {
        let mut bytes = [0; 32];
        bytes[..prefix.len()].copy_from_slice(prefix);
        let at = records.crate_of(ItemKey(ContentId(bytes)))?;
        records.crate_key(at).filter(|key| key.0.0.starts_with(prefix))
    };
    let of = match read.of() {
        // A builtin type's cell (`u32`'s item) is the language's, in no crate's record: the
        // toolchain every key covers fixes it, so no edit moves it.
        ReadOf::Names { krate } if krate == blaze_scope::BUILTIN_PREFIX => return Ok(None),
        ReadOf::Names { krate } => {
            // The crate's names task by its one name (blaze-drive key.rs): before, the raw key
            // was the subject, a name no task defines, so a names edit dirtied no body.
            let key = crate_by(&krate).ok_or(TaskError::NoCrate { prefix: krate })?;
            blaze_drive::key::crate_scope_name(&key)
        }
        ReadOf::Source { key } if key.crate_prefix() == blaze_scope::BUILTIN_PREFIX => return Ok(None),
        ReadOf::Source { key } => blaze_drive::key::fact_of(&key.0.0),
        ReadOf::Head { item } => super::head::signature_name(item),
        ReadOf::Impls { krate } if krate == [0; 16] => Name::new(Kind::ImplBucket, ABSENT),
        ReadOf::Impls { krate } => impls_name(view, krate)?,
    };
    Ok(Some(CellId { of, cell: Cell(read.cell()) }))
}

/// The impl-tables task's name of the crate whose key starts with `krate` (sixteen bytes; the
/// record index is by the first eight, so the full prefix is verified): [`impl_tables_name`],
/// one BLAKE3 of the crate key. [`ReadIds`] keeps it per crate, so a body's many impl reads of
/// one crate (one per cell) hash it once.
fn impls_name(view: Tables<'_, '_>, krate: [u8; 16]) -> Result<Name, TaskError> {
    let records = view.records();
    let mut bytes = [0; 32];
    bytes[..16].copy_from_slice(&krate);
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&krate[..8]);
    let key = records
        .crate_of(ItemKey(ContentId(bytes)))
        .and_then(|at| records.crate_key(at))
        .filter(|key| key.0.0.starts_with(&krate))
        .ok_or(TaskError::NoCrate { prefix })?;
    Ok(impl_tables_name(key))
}

/// One immutable scope's translation from recorder keys to engine IDs. The table keeps at
/// most 65,536 keys (including builtins with no cell). A miss at capacity clears it in place.
/// Clearing changes only the amount of work: every miss uses the same canonical `cell_id`.
pub struct ReadIds<'t, 'n, 'a> {
    tables: Tables<'t, 'n>,
    ids: blaze_hash::HashMap<TableRead, Option<CellId>, Alloc<'a>>,
    /// Per crate (its key's first sixteen bytes), its impl-tables task's name ([`impls_name`]):
    /// one hash per crate, answered directly without populating or evicting `ids`.
    impl_names: blaze_hash::HashMap<[u8; 16], Name, Alloc<'a>>,
    /// Per crate (its key's first eight bytes), its `CrateScope` task's name for `NameCell::Root`,
    /// answered directly without populating or evicting `ids`.
    scope_names: blaze_hash::HashMap<[u8; 8], Name, Alloc<'a>>,
    made: u64,
    hits: u64,
}

impl<'t, 'n, 'a> ReadIds<'t, 'n, 'a> {
    const LIMIT: usize = 1 << 16;

    pub fn new(tables: Tables<'t, 'n>, alloc: Alloc<'a>) -> Self {
        Self {
            tables,
            ids: blaze_hash::HashMap::new_in(alloc),
            impl_names: blaze_hash::HashMap::new_in(alloc),
            scope_names: blaze_hash::HashMap::new_in(alloc),
            made: 0,
            hits: 0,
        }
    }

    pub fn id(&mut self, read: TableRead) -> Result<Option<CellId>, TaskError> {
        match read {
            TableRead::Impls { krate, bucket } => {
                if krate == [0; 16] {
                    self.hits += 1;
                    return Ok(Some(CellId { of: Name::new(Kind::ImplBucket, ABSENT), cell: Cell(bucket) }));
                }
                let of = match self.impl_names.get(&krate) {
                    Some(&of) => {
                        self.hits += 1;
                        of
                    }
                    None => {
                        let of = impls_name(self.tables, krate)?;
                        if self.impl_names.len() == Self::LIMIT {
                            self.impl_names.clear();
                        }
                        self.impl_names.insert(krate, of);
                        self.made += 1;
                        of
                    }
                };
                return Ok(Some(CellId { of, cell: Cell(bucket) }));
            }
            TableRead::Names(NameCell::Root { krate }) => {
                if krate == blaze_scope::BUILTIN_PREFIX {
                    self.hits += 1;
                    return Ok(None);
                }
                let of = match self.scope_names.get(&krate) {
                    Some(&of) => {
                        self.hits += 1;
                        of
                    }
                    None => {
                        let records = self.tables.records();
                        let mut bytes = [0; 32];
                        bytes[..8].copy_from_slice(&krate);
                        let key = records
                            .crate_of(ItemKey(ContentId(bytes)))
                            .and_then(|at| records.crate_key(at))
                            .filter(|key| key.0.0.starts_with(&krate))
                            .ok_or(TaskError::NoCrate { prefix: krate })?;
                        let of = blaze_drive::key::crate_scope_name(&key);
                        if self.scope_names.len() == Self::LIMIT {
                            self.scope_names.clear();
                        }
                        self.scope_names.insert(krate, of);
                        self.made += 1;
                        of
                    }
                };
                return Ok(Some(CellId { of, cell: Cell(read.cell()) }));
            }
            _ => {}
        }
        if let Some(id) = self.ids.get(&read) {
            self.hits += 1;
            return Ok(*id);
        }
        let id = cell_id(self.tables, read)?;
        if self.ids.len() == Self::LIMIT {
            self.ids.clear();
        }
        self.ids.insert(read, id);
        self.made += u64::from(id.is_some());
        Ok(id)
    }

    pub fn counts(&self) -> (u64, u64) {
        (self.made, self.hits)
    }
}

/// THE KEPT BODY (run.md R4, D119): beside its facts record, a body checked clean leaves its
/// generic body in blaze-body's flat form with the table it indexes ([`keep_checked`]), so a
/// run builds a called instance from it and lowers nothing again. Its identity is its bytes'
/// (blaze-cli engine_body_record.rs `key_of`), never the verdict's: two bodies with one clean
/// verdict (`return 1`, `return 2`) are two records.
///
/// The values the checked record does not hold, which the lowering asks for by expression,
/// pattern or variant: each table sorted by its key, one entry per key, read by binary search.
/// A key a table lacks is "not known" and the lowering refuses its construct by name
/// (blaze-body-lower `Construct::NoLiteralValue`, `NoDiscriminant`), never a guess (rule 3).
#[derive(Clone, Copy)]
pub struct KeepValues<'v> {
    pub ints: &'v [(blaze_typeck::ExprId, u128)],
    pub floats: &'v [(blaze_typeck::ExprId, u128)],
    pub chars: &'v [(blaze_typeck::ExprId, u32)],
    pub strs: &'v [(blaze_typeck::ExprId, &'v [u8])],
    pub int_pats: &'v [(blaze_typeck::PatId, u128)],
    pub discrs: &'v [((blaze_typeck::ItemKey, u32), blaze_body_lower::Discr)],
    /// Every callee of the body that is a trait's method, sorted: a callee not listed is a
    /// plain fn item. Complete by its maker: it lists each callee of `Checked::callees` whose
    /// definition is a trait member.
    pub trait_items: &'v [blaze_typeck::ItemKey],
    pub pointer_bits: u32,
}

/// The lowering's facts over front2's: the flow facts the borrow check gathered for this body
/// (front2 borrows.rs `gather_facts`, gathered once and shared, rule 77) and its
/// [`KeepValues`]. Only the questions blaze-body-lower asks are answered from `flow`.
pub struct KeepFacts<'f, F> {
    pub flow: &'f F,
    pub values: KeepValues<'f>,
}

impl<F: blaze_flow_lower::Facts> blaze_flow_lower::Facts for KeepFacts<'_, F> {
    fn adt(&self, adt: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::AdtFacts> {
        self.flow.adt(adt)
    }
    fn callee(&self, item: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::CalleeFacts> {
        self.flow.callee(item)
    }
    fn field(&self, adt: blaze_typeck::ItemKey, variant: u32, name: blaze_typeck::Name) -> Option<u32> {
        self.flow.field(adt, variant, name)
    }
    fn field_count(&self, adt: blaze_typeck::ItemKey, variant: u32) -> Option<u32> {
        self.flow.field_count(adt, variant)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.flow.param_copy(index)
    }
    fn trait_lifetimes(&self, trait_: blaze_typeck::ItemKey) -> Option<u32> {
        self.flow.trait_lifetimes(trait_)
    }
    fn alias_lifetimes(&self, trait_: blaze_typeck::ItemKey, name: blaze_typeck::Name) -> Option<u32> {
        self.flow.alias_lifetimes(trait_, name)
    }
    // Every other fact: the answer the trait's removed default gave, spelled out (`Facts` has
    // no default, so a fact left out here does not compile). These are NOT forwarded yet: a
    // forward changes what blaze-body-lower sees, and it lands as its own save, measured alone.
    fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
        &blaze_flow_lower::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[blaze_flow_lower::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        None
    }
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
}

/// The entry of `key` in a table sorted by key.
fn sorted_get<K: Ord + Copy, V: Copy>(table: &[(K, V)], key: K) -> Option<V> {
    table.binary_search_by(|(k, _)| k.cmp(&key)).ok().map(|at| table[at].1)
}

impl<F: blaze_flow_lower::Facts> blaze_body_lower::BodyFacts for KeepFacts<'_, F> {
    fn int_lit(&self, e: blaze_typeck::ExprId) -> Option<u128> {
        sorted_get(self.values.ints, e)
    }
    fn float_lit(&self, e: blaze_typeck::ExprId) -> Option<u128> {
        sorted_get(self.values.floats, e)
    }
    fn char_lit(&self, e: blaze_typeck::ExprId) -> Option<u32> {
        sorted_get(self.values.chars, e)
    }
    fn str_lit(&self, e: blaze_typeck::ExprId) -> Option<&[u8]> {
        sorted_get(self.values.strs, e)
    }
    fn int_pat(&self, p: blaze_typeck::PatId) -> Option<u128> {
        sorted_get(self.values.int_pats, p)
    }
    fn discr(&self, adt: blaze_typeck::ItemKey, variant: u32) -> Option<blaze_body_lower::Discr> {
        sorted_get(self.values.discrs, (adt, variant))
    }
    fn pointer_bits(&self) -> Option<u32> {
        Some(self.values.pointer_bits)
    }
    fn trait_item(&self, item: blaze_typeck::ItemKey) -> Option<bool> {
        Some(self.values.trait_items.binary_search(&item).is_ok())
    }
}

/// The kept record of a body checked clean as `done` over `view` (run.md R4): lowered once by
/// the one lowering and written with `done`'s own table, in `alloc`. The caller is the borrow
/// check's step, where the view, the check and the flow facts already are (front2 borrows.rs
/// `borrows_of`, after `gather_facts_planted` and `body.typeck.view(tys)`): nothing is checked,
/// gathered or lowered for it again. A refusal is the run's to tell, naming the body: the kept
/// record is the run's input, and a body without one is never run from another form (rule 3).
pub fn keep_checked<'a, F: blaze_flow_lower::Facts>(
    view: &blaze_typeck::BodyView<'_>,
    done: &blaze_typeck::Checked<'_>,
    facts: &KeepFacts<'_, F>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, blaze_body_lower::KeepError> {
    blaze_body_lower::keep(view, done, facts, alloc)
}

#[cfg(test)]
mod kept_body_tests {
    use super::{KeepFacts, KeepValues, sorted_get};
    use blaze_body_lower::BodyFacts as _;
    use blaze_flow_lower::{AdtFacts, CalleeFacts, Facts};
    use blaze_typeck::{ExprId, ItemKey, Name, PatId};

    /// No ADT and no callee is known: the values alone are under test.
    struct NoFlow;

    impl Facts for NoFlow {
        // Every other fact not known here, written out: `Facts` has no default answer.
        fn regions(&self) -> &dyn blaze_flow_lower::RegionFacts {
            &blaze_flow_lower::NoRegions
        }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
            None
        }
        fn adt_variances(&self, _: ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
            None
        }
        fn implied_bounds(&self) -> Option<(&[(u32, blaze_flow_lower::Region)], &[(blaze_flow_lower::Region, blaze_flow_lower::Region)])> {
            None
        }
        fn constant_initializer(&self) -> bool {
            false
        }
        fn fn_binders(&self) -> bool {
            true
        }
        fn closure_bodies_checked(&self) -> bool {
            false
        }
        fn closure_requirements(&self, _: ExprId) -> &[blaze_flow_lower::Requirement] {
            &[]
        }
        fn async_bodies_checked(&self) -> bool {
            false
        }
        fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
            None
        }
        fn shape(&self, _: ItemKey) -> Option<blaze_flow_lower::Shape<'_>> {
            None
        }
        fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> {
            None
        }
        fn call_bounds(&self, _: ItemKey) -> Option<blaze_flow_lower::CallBounds<'_>> {
            None
        }
        fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> {
            None
        }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> {
            None
        }
        fn ties(&self, _: ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
            None
        }
        fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> {
            None
        }
        fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<blaze_flow_lower::FieldLead> {
            None
        }
        fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[blaze_flow_lower::FieldRegion]> {
            None
        }
        fn copy_args(&self, _: ItemKey) -> Option<&[bool]> {
            None
        }
        fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
            None
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            None
        }
        fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
            None
        }
        fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
            None
        }
        fn param_copy(&self, _: u32) -> bool {
            false
        }
    }

    /// Each value is answered for its own key and no other; a key the tables lack is unknown
    /// (the lowering then refuses by name), and a callee is a trait's method only when listed.
    #[test]
    fn values_answer_their_own_keys_only() {
        let ints = [(ExprId(1), 1u128), (ExprId(4), 2)];
        let strs: [(ExprId, &[u8]); 1] = [(ExprId(2), b"hi")];
        let pats = [(PatId(0), 7u128)];
        let traits = [ItemKey(9)];
        let facts = KeepFacts {
            flow: &NoFlow,
            values: KeepValues { ints: &ints, floats: &[], chars: &[], strs: &strs, int_pats: &pats, discrs: &[], trait_items: &traits, pointer_bits: 64 },
        };
        assert_eq!(facts.int_lit(ExprId(1)), Some(1));
        assert_eq!(facts.int_lit(ExprId(4)), Some(2));
        // Planted: a key between two entries is not answered with a neighbour's value.
        assert_eq!(facts.int_lit(ExprId(2)), None);
        assert_eq!(facts.str_lit(ExprId(2)), Some(&b"hi"[..]));
        assert_eq!(facts.int_pat(PatId(0)), Some(7));
        assert_eq!(facts.float_lit(ExprId(1)), None);
        assert_eq!(facts.trait_item(ItemKey(9)), Some(true));
        assert_eq!(facts.trait_item(ItemKey(8)), Some(false));
        assert_eq!(sorted_get(&[(1u32, 'a'), (3, 'c')], 3), Some('c'));
    }
}

#[cfg(test)]
mod read_id_tests {
    use super::*;
    use super::super::head_tests::{CORE_AT, a, world_of};
    use super::super::view::TablesOpts;

    #[test]
    fn sibling_items_return_only_new_cells_and_keep_their_own_verdicts() {
        use blaze_drive::key::Key;
        use blaze_engine::{Phase, Planned, Stage};
        use super::super::head_tests::cells;

        let (records, _, frame) = world_of("#![no_std]\npub fn base() -> u8 { 1u8 }\npub fn left() -> u8 { base() }\npub fn right() -> u16 { base() }");
        let unused = Recorder::new(RecorderOpts { alloc: a() });
        let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &unused });
        let frame = BodyFrame { frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::All, heads: None, sources: None, goals: None, prepared: None };
        let heads = cells(records);
        let mut entry = Recorder::new(RecorderOpts { alloc: a() });
        let mut encoding = AString::new_in(a());
        let mut union = std::collections::BTreeSet::new();
        for label in ["left", "right"] {
            let key = heads.iter().map(|want| want.key()).find(|&key| records.name_of(key) == Some(label)).expect("fixture function");
            let task = Task { planned: Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) }, key: Key(ABSENT) };
            let alone = check_body_reads(tables, &task, &frame, a()).expect("item checked alone");
            let together = check_body_item(tables, &task, &frame, a(), &mut entry, None, &mut encoding).expect("item checked in the entry");
            let fresh: std::collections::BTreeSet<_> = alone.reads.iter().copied().filter(|key| union.insert(*key)).collect();
            assert_eq!(together.reads.iter().copied().collect::<std::collections::BTreeSet<_>>(), fresh);
            assert_eq!(together.reads.len(), fresh.len());
            assert_eq!(together.read_tally.offered, alone.read_tally.offered);
            assert_eq!(together.read_tally.kept, fresh.len() as u64);
            assert_eq!(together.out, alone.out);
            assert_eq!(together.said.iter().filter(|line| matches!(line.what, SaidWhat::Type { code: "E0308" })).count(), usize::from(label == "right"));
            if label == "right" {
                assert!(fresh.len() < alone.reads.len(), "both siblings depend on base");
            }
        }
        std::println!("shared cells kept once; left checks, right reports E0308");
    }

    #[test]
    fn resetting_the_stretch_memo_preserves_cell_identity() {
        let (records, _, _) = world_of("#![no_std]\npub fn f() {}");
        let recorder = Recorder::new(RecorderOpts { alloc: a() });
        let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &recorder });
        let read = TableRead::Names(NameCell::Lang { krate: records.crate_key(CORE_AT).expect("fixture core").prefix() });
        let expected = cell_id(tables, read).expect("core belongs to this scope").expect("core's lang cell");
        let mut memo = ReadIds::new(tables, a());
        assert_eq!(memo.id(read).unwrap(), Some(expected));
        assert_eq!(memo.id(read).unwrap(), Some(expected));
        assert_eq!(memo.counts(), (1, 1));
        for at in 1..=ReadIds::LIMIT {
            memo.id(TableRead::Head { item: (at as u128).to_be_bytes(), part: 0 }).unwrap();
        }
        assert_eq!(memo.ids.len(), 1, "the first miss at capacity resets the table");
        assert_eq!(memo.id(read).unwrap(), Some(expected));
        assert_eq!(memo.counts(), (ReadIds::LIMIT as u64 + 2, 1));
        std::println!("read ID unchanged after 65,536 entries and a reset");
    }

    #[test]
    fn impls_and_root_reads_bypass_and_do_not_evict_the_ids_table() {
        let (records, _, _) = world_of("#![no_std]\npub fn f() {}");
        let recorder = Recorder::new(RecorderOpts { alloc: a() });
        let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &recorder });
        let Some(core_key) = records.crate_key(CORE_AT) else {
            assert!(false, "fixture core key");
            return;
        };
        let mut krate16 = [0u8; 16];
        krate16.copy_from_slice(&core_key.0.0[..16]);
        let lang_read = TableRead::Names(NameCell::Lang { krate: core_key.prefix() });
        let root_read = TableRead::Names(NameCell::Root { krate: core_key.prefix() });
        let mut memo = ReadIds::new(tables, a());
        assert_eq!(memo.id(lang_read).ok(), cell_id(tables, lang_read).ok());
        assert_eq!(memo.ids.len(), 1);
        assert_eq!(memo.id(root_read).ok(), cell_id(tables, root_read).ok());
        assert_eq!(memo.ids.len(), 1, "NameCell::Root must not populate `ids`");
        for i in 0u32..512 {
            let mut bucket = [0u8; 16];
            bucket[..4].copy_from_slice(&i.to_be_bytes());
            let impl_read = TableRead::Impls { krate: krate16, bucket };
            assert_eq!(memo.id(impl_read).ok(), cell_id(tables, impl_read).ok());
        }
        let absent_impl = TableRead::Impls { krate: [0; 16], bucket: [7; 16] };
        assert_eq!(memo.id(absent_impl).ok(), cell_id(tables, absent_impl).ok());
        assert_eq!(memo.ids.len(), 1, "TableRead::Impls must not populate or evict `ids`");
    }
}

/// D197 root 1, the `Typed` kind: a body whose types are clean and whose borrows were refused
/// (D151 `Typed`, which runs) keeps its checked record, so a run reads it instead of checking
/// it again. The fixture is borrows.rs `region_record_tests`'s shape: `calls_soft_refused`
/// calls `m`, whose parameter is a type macro, so its borrows are refused `RegionsUnread`
/// while its types are whole.
#[cfg(test)]
mod keeps_typed_tests {
    use super::keeps_typed;
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::borrows::BorrowRefused;
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    const SOURCE: &str = r#"
macro_rules! byte {
    () => {
        u8
    };
}

fn m(x: &u8, _y: byte!()) -> &u8 {
    x
}

fn calls_soft_refused() {
    let a = 1u8;
    let r = m(&a, 2);
    let _ = *r;
}

fn plain() -> u8 {
    let a = 1u8;
    a
}

fn mistyped() -> u8 {
    true
}

fn main() {}
"#;

    /// The cause (rule 23): the old filter, which kept a record only when the borrows were
    /// checked clean too. CONTROL: under it the `Typed` body keeps none, which is what sent
    /// every such body to the run's second check. TWIN: a body with a type error keeps none
    /// under either (its columns are not clean, so there is no record to keep).
    ///
    /// Prints: `keeps_typed: calls_soft_refused kept (borrows refused), plain kept, mistyped none`
    #[test]
    fn a_body_whose_borrows_were_refused_keeps_its_typed_record() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let home = std::env::var("HOME").expect("HOME is set");
        let tc = Toolchain::open(&mut DirShelf::new(std::format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
        let files = [("/kt/src/main.rs".to_string(), SOURCE.to_string())];
        let edition = edition::PackageEdition::of_manifest("2024").expect("Rust 2024");
        let user = UserCrate { name: "kt", dir: "/kt/", root: "/kt/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
        let bodies = world.bodies(alloc);
        let body = |name: &str| {
            bodies
                .iter()
                .find_map(|&(body, _)| {
                    let BodyRef::Free(key) = body else { return None };
                    (names.item_text(key).and_then(|item| item.name) == Some(name)).then_some(body)
                })
                .unwrap_or_else(|| panic!("missing fixture body {name}"))
        };
        let old = |types: &crate::front2::project::BodyVerdict<'_>, borrows: &crate::front2::borrows::BorrowVerdict<'_>| types.is_clean() && borrows.findings.is_empty() && borrows.refused.is_none();

        let (types, borrows) = world.check_borrows(body("calls_soft_refused"), alloc);
        assert!(types.is_clean(), "calls_soft_refused: types whole: {types:?}");
        assert!(matches!(borrows.refused, Some(ref r) if !matches!(r, BorrowRefused::Untyped(_))), "calls_soft_refused: borrows refused by name (D151 `Typed`): {borrows:?}");
        assert!(matches!(types.typed, Some(Ok(_))), "calls_soft_refused: the check encoded its record");
        assert!(keeps_typed(&types), "calls_soft_refused: a `Typed` body keeps its record");
        assert!(!old(&types, &borrows), "CONTROL: the old filter kept none for it");

        let (types, borrows) = world.check_borrows(body("plain"), alloc);
        assert!(matches!(types.typed, Some(Ok(_))) && keeps_typed(&types) && old(&types, &borrows), "plain: a clean body keeps its record under both: {types:?} {borrows:?}");

        let (types, borrows) = world.check_borrows(body("mistyped"), alloc);
        assert!(!keeps_typed(&types) && !old(&types, &borrows), "TWIN: a type error keeps no record: {types:?}");
        assert!(types.typed.is_none(), "TWIN: no record was encoded for a type error");
        std::println!("keeps_typed: calls_soft_refused kept (borrows refused), plain kept, mistyped none");
    }
}
