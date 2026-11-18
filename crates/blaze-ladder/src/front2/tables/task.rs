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
use super::super::by_origin::{KeyKind, kind_of_key};
use super::super::fill::FillError;
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
    /// The request's frozen heads (seeds.rs [`FrozenTable`](super::seeds::FrozenTable), read
    /// only and shared): every head the body needs is filed from them, none lowered. `None`:
    /// the body's store lowers each head it needs (the path before frozen heads).
    pub heads: Option<&'n dyn HeadSeed<'n>>,
    /// What the body's macro calls expand with (body_host.rs `BodySources`: the owner's
    /// environment, the expansion texts, the proc-macro runner), as `check`'s path sets them
    /// (project.rs `set_body_sources`). `None`: each body `env!`, include and proc macro is
    /// refused by name.
    pub sources: Option<&'n dyn super::super::body_host::BodySources>,
}

/// Why a body task gave no result: each a fault of the plan or the engine, never of the body
/// (a body's own failure is a [`Done`] with `failed`).
#[derive(Debug)]
pub enum TaskError {
    /// A head the body needed could not be filed from the request's frozen heads.
    Seed { body: Name, error: SeedError },
    /// No head cell holds the fact of checker key `key` (head.rs `fact_cell`).
    Cell { key: blaze_typeck::ItemKey, error: FreezeError },
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
            TaskError::Cell { key, error } => write!(f, "no head cell holds the body's fact of {key:?}: {error} (front2 tables/task.rs `fact_read`)"),
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
}

/// One line of a body's verdict ([`BodyOut::said`]): `start..end` is item-relative (rule 71).
#[derive(Debug)]
pub struct Said<'a> {
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
    if let Some(sources) = frame.sources {
        world.set_body_sources(sources);
    }
    drop(setup);
    let (verdict, borrows) = world.check_borrows(body, frame.alloc);
    if let Some(error) = world.take_seed_error() {
        return Err(TaskError::Seed { body: name, error });
    }
    let reading = Cost::enter(frame.cost, Part::Reads);
    let facts = verdict.facts.as_ref().map_err(|e| TaskError::Facts(*e))?;
    for row in facts.rows.iter() {
        // By the records, unrecorded: which cell holds a fact is the plan's, not the body's.
        // An opaque type's facts are its fn's head's (the fn's head files it).
        let key = world.opaque_fn(blaze_typeck::ItemKey(row.fact.key)).map_or(row.fact.key, |fn_| fn_.0);
        if let Some(read) = fact_read(view.records(), &world.front.keys, row.fact.kind, key)? {
            recorder.read(read);
        }
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
    let mut held: AVec<'a, (TableRead, u128)> = AVec::with_capacity_in(held_keys.len(), alloc);
    for &k in held_keys.iter() {
        if let Some(read) = fact_read(view.records(), &world.front.keys, FactKind::Head, k)? {
            held.push((read, k));
        }
    }
    held.sort_unstable();
    let said = said_of(&verdict, &borrows, view.records(), &world.front.keys, alloc)?;
    drop(world);
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
    Ok(BodyOut { done, made, bytes, reads, held, said })
}

/// The lines `check` prints for a verdict (blaze-cli body_types.rs, today's path), in `alloc`.
fn said_of<'n, 'a>(verdict: &BodyVerdict<'_>, borrows: &BorrowVerdict<'_>, names: &'n Records<'n>, keys: &Keys, alloc: Alloc<'a>) -> Result<AVec<'a, Said<'a>>, TaskError> {
    let text = |s: &str| -> AString<'a> {
        let mut t = AString::new_in(alloc);
        t.push_str(s);
        t
    };
    let mut said: AVec<'a, Said<'a>> = AVec::new_in(alloc);
    for d in verdict.diagnostics.iter() {
        said.push(Said { what: SaidWhat::Type { code: d.code }, start: d.start, end: d.end, words: text(d.text.as_str()) });
    }
    for d in verdict.findings.iter() {
        said.push(Said { what: SaidWhat::Lint { lint: d.lint }, start: d.start, end: d.end, words: text(d.text.as_str()) });
    }
    if let Some(refused) = &verdict.refused {
        let cause = super::super::said::refused_cause(names, keys, refused);
        let words = super::super::said::refused_words(names, refused);
        said.push(Said { what: SaidWhat::NotChecked { cause: text(&cause) }, start: 0, end: 0, words: text(&words) });
    } else if let Some(words) = super::super::said::gap_words(&verdict.gaps) {
        said.push(Said { what: SaidWhat::NotChecked { cause: text(super::super::said::GAP_CAUSE) }, start: 0, end: 0, words: text(&words) });
    }
    for d in borrows.findings.iter() {
        said.push(Said { what: SaidWhat::Borrow { code: d.code }, start: d.start, end: d.end, words: text(d.text) });
    }
    if let Some(refused) = &borrows.refused
        && !matches!(refused, BorrowRefused::Untyped(_))
    {
        let (start, end) = refused.range().unwrap_or((0, 0));
        let mut words = AString::new_in(alloc);
        write!(words, "{}: {refused:?}", refused.text()).map_err(|_| TaskError::Write)?;
        said.push(Said { what: SaidWhat::BorrowsNotChecked, start, end, words });
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
///
/// THE CELL is the one that holds the fact (head.rs [`fact_cell`], FI's one home): a
/// constructor's is its ADT's, a trait member's its trait's, an impl constant's its impl's;
/// an anonymous constant's is the item it is written in. ONE PART: a head cell is read whole
/// (head.rs `cell_id`, part 0), whatever the fact's kind.
fn fact_read<'n>(names: &'n Records<'n>, keys: &Keys, kind: FactKind, key: u128) -> Result<Option<TableRead>, TaskError> {
    use FactKind as K;
    match kind {
        K::Head | K::Predicates | K::Adt | K::ImplHeader | K::AssocItem | K::Const | K::Opaque | K::Attrs | K::TraitDef | K::AssocParent | K::ConstHolds | K::ConstParams | K::AssocGenerics | K::BorrowAdt | K::BorrowHead | K::TypeAlias | K::FieldDefaults
            if key != 0 =>
        {
            let fact = blaze_typeck::ItemKey(key);
            let want = match fact_cell(names, keys, fact) {
                Ok(want) => want,
                Err(FreezeError::NoWant { error: FillError::AnonConst(_), .. }) => match kind_of_key(names, keys, fact) {
                    Ok(KeyKind::Anon { module, .. }) => fact_cell(names, keys, module).map_err(|error| TaskError::Cell { key: fact, error })?,
                    _ => return Err(TaskError::Cell { key: fact, error: FreezeError::NoWant { key: fact, error: FillError::AnonConst(fact) } }),
                },
                Err(error) => return Err(TaskError::Cell { key: fact, error }),
            };
            let id = head_cell_id(want);
            let mut item = [0u8; 16];
            item.copy_from_slice(&id.of.subject.0[..16]);
            Ok(Some(TableRead::Head { item, part: 0 }))
        }
        // `CrateBucket`: recorded as each lookup read it (view.rs `bucket`, `unfiled`,
        // `impls_read`); names facts: recorded by the view's walks.
        _ => Ok(None),
    }
}

/// A read as the engine names it (recorder.rs [`ReadOf`]): a names or lang read is a cell of
/// its crate's `CrateScope` record, a head read a cell of the item's `Signature`, an impl read
/// a cell of its crate's `ImplBucket` table (all zero: the fact of the whole graph).
pub fn cell_id(view: Tables<'_, '_>, read: TableRead) -> Result<Option<CellId>, TaskError> {
    let records = view.records();
    let crate_by = |test: &dyn Fn(&[u8; 32]) -> bool| -> Option<blaze_id::CrateItemKey> { (0..records.crate_count()).find_map(|k| records.crate_key(k).filter(|key| test(&key.0.0))) };
    let of = match read.of() {
        // A builtin type's cell (`u32`'s item) is the language's, in no crate's record: the
        // toolchain every key covers fixes it, so no edit moves it.
        ReadOf::Names { krate } if krate == blaze_scope::BUILTIN_PREFIX => return Ok(None),
        ReadOf::Names { krate } | ReadOf::Lang { krate } => {
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
    let cell = match read {
        TableRead::Lang { .. } => LANG_CELL,
        _ => Cell(read.cell()),
    };
    Ok(Some(CellId { of, cell }))
}
