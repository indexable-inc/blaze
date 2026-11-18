//! THE BODY TASK (engine.md, E2/E5): [`check_body`] checks one body over the read-only view
//! ([`Tables`]) and answers the engine's [`Done`]: what it found, the cells it read with the
//! digest each held, what it offers readers, and the records it made inline ([`Made`]). It is
//! a function of `(tables, task, frame, make)` alone, callable from any worker.
//!
//! NOTHING IS SHARED OR KEPT. The checker's mutable world (its key map, head store, impl index,
//! constants and prepared macros: `ProjectWorld`) is made for this one body and dropped with
//! it, so a body's answer and its read list are the same whichever bodies ran before it, in
//! which order, on how many workers. The recorder is the task's own; its list is sorted once,
//! when the body is done.
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

use core::fmt::Write as _;

use blaze_drive::cells::Cell;
use blaze_drive::key::{ABSENT, Kind, Name};
use blaze_engine::{CellId, Done, Made, SVec, Task};
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AString, AVec, Alloc, SharedAlloc};
use blaze_scope::names_cells::NameCell;
use blaze_surface::ItemKind;

use super::super::Front2;
use super::super::borrows::{BorrowRefused, BorrowVerdict};
use super::super::buckets::impl_tables_name;
use super::super::index::{Index, LangTwice};
use super::super::keys::{Keys, MemberKind};
use super::super::project::{BodyRef, BodyVerdict, ProjectWorld};
use super::super::project::every_body::Reached;
use super::super::project_records::Listed;
use super::super::reads::{FactKind, ReadsError};
use super::super::by_origin::{FactHolder, fact_holder, kind_of_key};
use super::super::item_shown::ItemShown;
use super::super::source::{NamesSource, Records};
use super::head::{FreezeError, cell_id as head_cell_id, fact_cell};
use super::super::texts::Texts;
use super::super::heads::NeedsMet;
use super::cost::{Cost, Part};
use super::seeds::{HeadSeed, SeedError};
use super::recorder::{ReadOf, ReadSink, ReadsFull, Recorder, RecorderOpts, TableRead};
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
    Cell { key: blaze_typeck::ItemKey, shown: ItemShown, error: FreezeError },
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
            TaskError::Seed { body, error } => write!(f, "body {body:?}: a head it needs could not be seeded: {error}"),
            TaskError::Cell { key: _, shown, error } => write!(f, "no head cell holds the body's fact of {shown}: {error} (front2 tables/task.rs `fact_read`)"),
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
/// tests' form, the answer and its row in one allocator ([`check_body_split`] is the engine's).
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

/// The one check behind [`check_body`], [`check_body_in`] and [`check_body_split`].
fn check_split<'n, 'a, 'w>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, work: Alloc<'w>, out: SharedAlloc<'a>, coverage: Coverage) -> Result<BodyChecked<'a, 'w>, TaskError> {
    let name = task.planned.name;
    let setup = Cost::enter(frame.cost, Part::Setup);
    let alloc = work;
    let recorder = Recorder::new(RecorderOpts { alloc });
    if let Some(cell) = frame.plant_drop {
        recorder.plant_drop(cell);
    }
    let view = tables.reading_into(&recorder);
    let body = body_of(view, name)?;
    // The item whose head holds the body's signature: a member's is its container's.
    let own = match body {
        BodyRef::Member { container, .. } => container,
        BodyRef::Free(key) => key,
    };
    let lang_crates = view.lang_crates();
    let index = Index::from_source(view, lang_crates).map_err(TaskError::Lang)?;
    view.records().name_lang_crates(lang_crates);
    let front = Front2 { names: view, resolver: crate::resolve::Paths { names: view }, texts: Texts::empty(), keys: Keys::new(), index, frame: frame.frame };
    let mut world = ProjectWorld::over(front, frame.alloc);
    world.set_cost(frame.cost);
    world.set_needs(frame.needs);
    world.set_seeds(frame.heads);
    world.set_goal_share(frame.goals);
    if let Some(sources) = frame.sources {
        world.set_body_sources(sources);
    }
    drop(setup);
    let root = Listed { body, at: None, entry: None };
    let mut goal_hits = AVec::new_in(alloc);
    let mut seed_error = None;
    let check = |w: &mut ProjectWorld<'n, Tables<'_, 'n>>, b| {
        let pair = w.check_borrows(b, frame.alloc);
        if let Some(error) = w.take_seed_error() {
            seed_error.get_or_insert(error);
        }
        goal_hits.extend_from_slice(w.goal_hits());
        pair
    };
    let reached = match coverage {
        Coverage::WithLocals => world.each_from(&[root], alloc, check),
        #[cfg(test)]
        Coverage::RootOnly => {
            let mut check = check;
            let mut one = AVec::new_in(alloc);
            one.push(Reached { listed: root, within: None, got: check(&mut world, body) });
            one
        }
    };
    if let Some(error) = seed_error {
        return Err(TaskError::Seed { body: name, error });
    }
    let reading = Cost::enter(frame.cost, Part::Reads);
    let mut bytes = AString::new_in(alloc);
    let mut said = AVec::new_in(alloc);
    let mut failed = false;
    let mut findings = 0u32;
    for (i, one) in reached.iter().enumerate() {
        let (verdict, borrows) = &one.got;
        let facts = verdict.facts.as_ref().map_err(|e| TaskError::Facts(*e))?;
        // An internal-symbol declaration the body named was bound by searching crates' items:
        // that search is the body's read (front2 internal_symbol.rs `record_search`).
        super::super::internal_symbol::record_search(view, &world.store, facts);
        for row in facts.rows.iter() {
            // By the records, unrecorded: which cell holds a fact is the plan's, not the body's.
            // An opaque type's facts are its fn's head's (the fn's head files it).
            let key = world.opaque_fn(blaze_typeck::ItemKey(row.fact.key)).map_or(row.fact.key, |fn_| fn_.0);
            if let Some(read) = fact_read(view.records(), &world.front.keys, row.fact.kind, key)? {
                recorder.read(read);
            }
        }
        said.extend(said_of(verdict, borrows, view.records(), &world.front.keys, || local_path(&reached[..i], one.within, alloc), alloc)?);
        // The root has precisely its original bytes. Each local appends another complete verdict.
        write_verdict(&mut bytes, verdict, borrows, Facts::Digest)?;
        failed |= verdict.refused.is_some() || !verdict.gaps.is_empty() || matches!(borrows.refused, Some(ref r) if !matches!(r, BorrowRefused::Untyped(_)));
        let count = verdict.diagnostics.len() + verdict.findings.len() + borrows.findings.len();
        findings = findings.saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
    }
    // THE BODY'S OWN HEAD (rule 21): its signature (a member's: its container's cell) is an
    // input of every check of it, also one refused before any question (a trait member's
    // missing default body), so it is read whether or not the checker asked for it.
    let own = super::super::keys::checker_of(own);
    if let Some(read) = fact_read(view.records(), &world.front.keys, FactKind::Head, own.0)? {
        recorder.read(read);
    }
    // THE CONTROL'S INSTRUMENT: the cell of each head the body's own store holds, beside the
    // read list (tests: every one must be in it; the store makes a head only when asked).
    let mut held_keys: AVec<'_, u128> = AVec::new_in(alloc);
    world.each_held_head(&mut |k| held_keys.push(k));
    let mut held: AVec<'w, (TableRead, u128)> = AVec::with_capacity_in(held_keys.len(), alloc);
    for &k in held_keys.iter() {
        if let Some(read) = fact_read(view.records(), &world.front.keys, FactKind::Head, k)? {
            held.push((read, k));
        }
    }
    held.sort_unstable();
    drop(world);
    // Counted before the recorder is consumed: its tables at their most (they only grow).
    let recorder_bytes = recorder.table_bytes();
    let (reads, read_tally) = recorder.finish_tallied().map_err(TaskError::Full)?;
    // The root's verdict bytes, then each local fn's (a root with no local fn keeps its bytes).
    let bytes: AVec<'w, u8> = bytes.into_bytes();
    let digest = ContentId::of(OUT_DOMAIN, &bytes);
    // Only what outlives the body in `out`; the cells are a step toward the row (`work`).
    let mut made: SVec<'a, Made<'a>> = SVec::new_in(out);
    let mut cells: AVec<'w, (CellId, ContentId)> = AVec::with_capacity_in(reads.len(), alloc);
    for read in reads.iter() {
        let Some(id) = cell_id(tables, *read)? else { continue };
        let saw = make.make(&id, &mut made).map_err(TaskError::Make)?;
        cells.push((id, saw));
    }
    cells.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    cells.dedup_by(|a, b| a.0 == b.0);
    let size = |n: usize, each: usize| (n as u64).saturating_mul(each as u64);
    let work_bytes = WorkBytes {
        recorder: recorder_bytes,
        recorder_floor: size(reads.len(), core::mem::size_of::<TableRead>()),
        cells: size(cells.capacity(), core::mem::size_of::<(CellId, ContentId)>()),
        held: size(held_keys.capacity(), core::mem::size_of::<u128>()) + size(held.capacity(), core::mem::size_of::<(TableRead, u128)>()),
        said: size(said.capacity(), core::mem::size_of::<Said<'_>>())
            + said.iter().map(|s| s.words.capacity() as u64 + s.within.as_ref().map_or(0, |w| w.capacity() as u64) + if let SaidWhat::NotChecked { cause } = &s.what { cause.capacity() as u64 } else { 0 }).sum::<u64>(),
        verdict: size(bytes.capacity(), 1),
        goal_hits: size(goal_hits.capacity(), core::mem::size_of::<blaze_solve::share::GoalHit>()),
    };
    drop(held_keys);
    drop(reading);
    // `findings` summed over the root and its local fns (u32, saturating), as the row's.
    Ok(BodyChecked { out: digest, failed, findings, cells, made, bytes, reads, held, said, goal_hits, read_tally, work: work_bytes })
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
    for d in borrows.findings.iter() {
        push(SaidWhat::Borrow { code: d.code }, d.start, d.end, text(d.text));
    }
    if let Some(refused) = &borrows.refused
        && !matches!(refused, BorrowRefused::Untyped(_))
    {
        let (start, end) = refused.range().unwrap_or((0, 0));
        let mut words = AString::new_in(alloc);
        write!(words, "{}: {refused:?}", refused.text()).map_err(|_| TaskError::Write)?;
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
fn fact_read<'n>(names: &'n Records<'n>, keys: &Keys, kind: FactKind, key: u128) -> Result<Option<TableRead>, TaskError> {
    use FactKind as K;
    match kind {
        K::Head | K::Predicates | K::Adt | K::ImplHeader | K::AssocItem | K::Const | K::Opaque | K::Attrs | K::TraitDef | K::AssocParent | K::ConstHolds | K::ConstParams | K::AssocGenerics | K::BorrowAdt | K::BorrowHead | K::TypeAlias | K::FieldDefaults
            if key != 0 =>
        {
            let fact = blaze_typeck::ItemKey(key);
            let refused = |error: FreezeError| TaskError::Cell { key: fact, shown: ItemShown::of(names, keys, fact), error };
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
        // `CrateBucket`: recorded as each lookup read it (view.rs `bucket`, `unfiled`,
        // `impls_read`); names facts: recorded by the view's walks.
        _ => Ok(None),
    }
}

/// A read as the engine names it (recorder.rs [`ReadOf`]): a names read (the lang rows too) is a cell of
/// its crate's `CrateScope` record, a head read a cell of the item's `Signature`, an impl read
/// a cell of its crate's `ImplBucket` table (all zero: the fact of the whole graph).
pub fn cell_id(view: Tables<'_, '_>, read: TableRead) -> Result<Option<CellId>, TaskError> {
    let records = view.records();
    let crate_by = |test: &dyn Fn(&[u8; 32]) -> bool| -> Option<blaze_id::CrateItemKey> { (0..records.crate_count()).find_map(|k| records.crate_key(k).filter(|key| test(&key.0.0))) };
    let of = match read.of() {
        // A builtin type's cell (`u32`'s item) is the language's, in no crate's record: the
        // toolchain every key covers fixes it, so no edit moves it.
        ReadOf::Names { krate } if krate == blaze_scope::BUILTIN_PREFIX => return Ok(None),
        ReadOf::Names { krate } => {
            // The crate's names task by its one name (blaze-drive key.rs): before, the raw key
            // was the subject, a name no task defines, so a names edit dirtied no body.
            let key = crate_by(&|key| key[..8] == krate).ok_or(TaskError::NoCrate { prefix: krate })?;
            blaze_drive::key::crate_scope_name(&key)
        }
        ReadOf::Head { item } => super::head::signature_name(item),
        ReadOf::Impls { krate } if krate == [0; 16] => Name::new(Kind::ImplBucket, ABSENT),
        ReadOf::Impls { krate } => {
            let mut prefix = [0u8; 8];
            prefix.copy_from_slice(&krate[..8]);
            let key = (0..records.crate_count()).find_map(|k| records.crate_key(k).filter(|key| key.0.0[..16] == krate)).ok_or(TaskError::NoCrate { prefix })?;
            impl_tables_name(key)
        }
    };
    Ok(Some(CellId { of, cell: Cell(read.cell()) }))
}
