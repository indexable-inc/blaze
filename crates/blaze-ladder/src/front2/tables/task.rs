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
use super::super::reads::{FactKind, ReadsError};
use super::super::source::NamesSource;
use super::super::texts::Texts;
use super::super::heads::NeedsMet;
use super::cost::{Cost, Part};
use super::recorder::{ReadOf, ReadSink, ReadsFull, Recorder, RecorderOpts, TableRead};
use super::view::Tables;

/// The domain of a body result's `out`.
const OUT_DOMAIN: &str = "blaze-ladder front2 body task out 1";
/// The one cell a body's record offers its readers: its verdict (`out`).
pub const VERDICT_CELL: Cell = Cell([0; 16]);
/// The cell of a crate's lang rows in its `CrateScope` record.
pub const LANG_CELL: Cell = Cell([0xfd; 16]);

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
}

/// Why a body task gave no result: each a fault of the plan or the engine, never of the body
/// (a body's own failure is a [`Done`] with `failed`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskError {
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
}

/// The body `task` names, checked over `tables` (module docs).
pub fn check_body<'n, 'a>(tables: Tables<'_, 'n>, task: &Task, frame: &BodyFrame<'n>, make: &dyn Make<'a>, shared: SharedAlloc<'a>) -> Result<BodyOut<'a>, TaskError> {
    let name = task.planned.name;
    let setup = Cost::enter(frame.cost, Part::Setup);
    let alloc = shared.local();
    let recorder = Recorder::new(RecorderOpts { alloc });
    if let Some(cell) = frame.plant_drop {
        recorder.plant_drop(cell);
    }
    let view = tables.reading_into(&recorder);
    let body = body_of(view, name)?;
    let lang_crates = view.lang_crates();
    let index = Index::from_source(view, lang_crates).map_err(TaskError::Lang)?;
    view.records().name_lang_crates(lang_crates);
    let front = Front2 { names: view, resolver: crate::resolve::Paths { names: view }, texts: Texts::empty(), keys: Keys::new(), index, frame: frame.frame };
    let mut world = ProjectWorld::over(front, frame.alloc);
    world.set_cost(frame.cost);
    world.set_needs(frame.needs);
    drop(setup);
    let (verdict, borrows) = world.check_borrows(body, frame.alloc);
    let reading = Cost::enter(frame.cost, Part::Reads);
    drop(world);
    let facts = verdict.facts.as_ref().map_err(|e| TaskError::Facts(*e))?;
    for row in facts.rows.iter() {
        if let Some(read) = fact_read(row.fact.kind, row.fact.key) {
            recorder.read(read);
        }
    }
    let reads = recorder.finish().map_err(TaskError::Full)?;
    let bytes = verdict_bytes(&verdict, &borrows, Facts::Digest, alloc)?;
    let out = ContentId::of(OUT_DOMAIN, &bytes);

    let failed = verdict.refused.is_some() || !verdict.gaps.is_empty() || matches!(borrows.refused, Some(ref r) if !matches!(r, BorrowRefused::Untyped(_)));
    let findings = verdict.diagnostics.len() + verdict.findings.len() + borrows.findings.len();
    let mut made: SVec<'a, Made<'a>> = SVec::new_in(shared);
    let mut cells: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(reads.len(), shared);
    for read in reads.iter() {
        let Some(id) = cell_id(tables, *read)? else { continue };
        let saw = make.make(&id, &mut made).map_err(TaskError::Make)?;
        cells.push((id, saw));
    }
    cells.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    cells.dedup_by(|a, b| a.0 == b.0);
    let mut defines: SVec<'a, (Cell, ContentId)> = SVec::new_in(shared);
    if !failed {
        defines.push((VERDICT_CELL, out));
    }
    let done = Done { name, key: task.key, out, defines, reads: cells, findings: u32::try_from(findings).unwrap_or(u32::MAX), failed };
    drop(reading);
    Ok(BodyOut { done, made, bytes, reads })
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
    write!(text, "diagnostics {:?}\nfindings {:?}\ngaps {:?}\nrefused {:?}\nborrows {:?}\n", verdict.diagnostics, verdict.findings, verdict.gaps, verdict.refused, borrows).map_err(|_| TaskError::Write)?;
    if facts == Facts::Digest {
        write!(text, "reads {:?}\n", verdict.reads).map_err(|_| TaskError::Write)?;
    }
    let mut bytes: AVec<'a, u8> = AVec::new_in(alloc);
    bytes.extend_from_slice(text.as_bytes());
    Ok(bytes)
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
fn fact_read(kind: FactKind, key: u128) -> Option<TableRead> {
    use FactKind as K;
    match kind {
        K::Head | K::Predicates | K::Adt | K::ImplHeader | K::AssocItem | K::Const | K::Opaque | K::Attrs | K::TraitDef | K::AssocParent | K::ConstHolds | K::ConstParams | K::AssocGenerics | K::BorrowAdt | K::BorrowHead | K::TypeAlias | K::FieldDefaults
            if key != 0 =>
        {
            Some(TableRead::Head { item: key.to_be_bytes(), part: kind as u8 })
        }
        // `CrateBucket`: recorded as each lookup read it (view.rs `bucket`, `unfiled`,
        // `impls_read`); names facts: recorded by the view's walks.
        _ => None,
    }
}

/// A read as the engine names it (recorder.rs [`ReadOf`]): a names or lang read is a cell of
/// its crate's `CrateScope` record, a head read a cell of the item's `Signature`, an impl read
/// a cell of its crate's `ImplBucket` table (all zero: the fact of the whole graph).
fn cell_id(view: Tables<'_, '_>, read: TableRead) -> Result<Option<CellId>, TaskError> {
    let records = view.records();
    let crate_by = |test: &dyn Fn(&[u8; 32]) -> bool| -> Option<ContentId> {
        (0..records.crate_count()).find_map(|k| records.crate_key(k).filter(|key| test(&key.0.0)).map(|key| key.0))
    };
    let of = match read.of() {
        // A builtin type's cell (`u32`'s item) is the language's, in no crate's record: the
        // toolchain every key covers fixes it, so no edit moves it.
        ReadOf::Names { krate } if krate == blaze_scope::BUILTIN_PREFIX => return Ok(None),
        ReadOf::Names { krate } | ReadOf::Lang { krate } => {
            let subject = crate_by(&|key| key[..8] == krate).ok_or(TaskError::NoCrate { prefix: krate })?;
            Name::new(Kind::CrateScope, subject)
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
    let cell = match read {
        TableRead::Lang { .. } => LANG_CELL,
        _ => Cell(read.cell()),
    };
    Ok(Some(CellId { of, cell }))
}
