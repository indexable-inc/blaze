//! The `Heads` stage of `blaze engine-check` (engine.md E6): each crate's impl tables made ONCE
//! a request as its own task, and shared read-only by every body.
//!
//! THE PLAN, from text: one `ImplBucket` task a crate of the names world, named
//! [`impl_tables_name`] of its key, in `Phase { stage: Heads, wave: 0 }` (after every crate's
//! names), its source and its file the crate's names task's, so its key is known before any
//! phase runs and it enters with its crate's names ([`plan`]).
//!
//! THE TASK, on the pool: the crate's check scope (engine_bodies.rs `scope_of`, the rule the
//! Bodies stage takes too), its records opened in the task's frame, its impl rows collected
//! (`NamesSource::each_impl_row` of the crate at 0), and front2 buckets/records.rs
//! `impl_tables_task` run over the read-only view with its own recorder. Its row:
//! - defines: the tables' own cells (a bucket's, a trait's) and `WHOLE`, the three records' digest;
//! - reads: the crate's names record whole (its impl rows are its items record's), each names
//!   read of the filing as its crate's names record whole (the Names stage defines that cell),
//!   and a header text the filing lowered as every scope crate's names record whole (no task
//!   defines a head's cell yet: the header's crate is one of the scope's, whose records hold its
//!   text); at cell grain (engine_scope_reads.rs `ScopeReads::TABLES_AT_CELLS`) each names read
//!   is its cell instead and each header read the cell of the head task covering it;
//! - its record in the store: the three records as one framed payload (buckets/records.rs
//!   `framed`), which a later request puts on its shelf while the task stands ([`complete`]).
//!
//! THE SHELF: each crate's records copied once onto engine_bodies.rs `BodiesStage::shelf` on the
//! coordinator after the phase; every body run reads it through its own `TableSet::lent`.
//! Nothing on it changes after the Heads phase, so nothing shared is mutable.
//!
//! THE HEAD TASKS (lane EY): one `Signature` task an outline entry that has a head, named and
//! keyed from text as the outline lists it ([`plan_items`]), in the same phase. Its record is the
//! frozen heads of every head cell the entry covers (front2 tables/head.rs `head_task`: a fn's
//! signature, an ADT, a trait with its members, an impl's header, an impl fn), each the cell
//! [`head_cell`] of the task with the frozen head's meaning, so a head re-made to equal bytes
//! moves no reader (early cutoff by digest, blaze-engine engine.rs). Its reads: each names cell its
//! freeze consulted, and for each head the freeze read (followed through the heads those read),
//! that head's head task's cell (a same-stage read, entered after its definer; before lane BH1
//! its file's `HEADS` cell, which every sibling head added moved)
//! or, for a dependency's head, its crate's names record whole. A body's
//! read of a member's head is the head's cell ([`Owners::place`], engine_bodies.rs `widen`). A
//! head that cannot be frozen fails ALONE (A8): the task defines the heads it froze, and the
//! failed head's cell is defined by its reason's digest ([`HEAD_FAILED_DOMAIN`]), so a reader
//! moves when the reason does; the row counts one finding a failed head, and its reason is a
//! head note (engine_head_notes.rs) a body that read it quotes. A task that cannot run its heads
//! at all (its crate's records, a cap) is a failed row, named by its file. A dependency's head is
//! read as its crate's names record whole (engine.md point 3). The freeze has a constant
//! evaluator, and a head that evaluated reads what the evaluation read (engine_head_consts.rs).
//!
//! ONE FREEZE A UNIT (lane RU1): a member crate is checked as several units (its lib, its test
//! harness, a bin that `mod`s its files), each with its own crate key, and an item's key holds
//! its unit's prefix. A head task (one an entry, by text) freezes its heads once for EACH member
//! unit whose items record lists its file, under that unit's records, and defines one cell a
//! (unit, head) ([`head_cell`] holds the unit); its payload is each unit's runs then the task's
//! run facts (the constructors its ADTs give, each unit's, gated as a body's are: [`ctor_facts`]),
//! its reads the union of each unit's, and a unit that stops holding an item (a `cfg`) drops its cell. Which
//! units hold the file is read too (lane RU2): each unit whose module walk reaches the file but
//! whose items record does not list it is read by its not-held cell (engine_body_plan.rs
//! `not_held_cell`, the one home the body plan reads), so a `cfg` taken off a `mod` enters the
//! task again and the newly holding unit gets its cell (rule 21). A
//! reader finds the cell by its own key ([`Owners::place`]), so it is never handed bytes frozen
//! under another unit's keys. The profile counts it: "heads frozen: N for M items in U units
//! (K items held by 2+ units)" ([`UnitCounts`]).
//! Within each unit the pool jobs take consecutive whole files in (file, name) order.
//! [`file_splits`] balances their task counts, and [`in_split_order`] restores that order
//! after the jobs finish. The "head jobs:" line counts files crossing a job boundary.
//!
//! WRITTEN HEADS (lane RH4): a head a macro wrote at module level (a derive's impl, a
//! `macro_rules!` call's struct, impl and fn, an attribute's output, a call nested in another's
//! output, an impl in a body's block that the output holds: serde's `const _: () = { impl .. }`)
//! is the head task's of the file entry its outermost call stands at, as a written head of that
//! entry ([`walk_crate`]: the walk up its calls and blocks, body_types/covered.rs
//! `written_path`, reads the crate's expansions directory alone). Its cell is [`head_cell`]'s
//! with where under the entry it was written ([`Written`]: the steps down, item-relative, rule
//! 71), so two derives at one struct and two calls of one macro are two cells; a file item's
//! cell is unchanged. The task's reads add, for each call under its entry, that call's
//! expansion record cell (blaze-scope names_cells.rs `NameCell::Expansion`: the record's key,
//! its text and column), and the freeze's own reads hold the written items' `Item` cells, whose
//! digests cover their text; an absent record refuses the task by name, the macro never run
//! again. A written head is held by the units that hold its entry's file, as a file item is.
//! A head whose walk fails or that no entry covers is [`HeadAt::Unlisted`]: its reader is
//! refused by name, never left to freeze it (rule 3).

use core::convert::Infallible;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use blaze_drive::cells::Cell;
use blaze_drive::{Input, Key, Kind, Locate, Name};
use blaze_engine::{Buffer, CellId, Done, Phase, Planned, SVec, Stage, Task, View};
use blaze_ids::ContentId;
use blaze_ladder::front2::buckets::{ImplShelf, file_impl_rows_chunk, finish_impl_tables, framed, impl_tables_name, impl_tables_out, impl_tables_task};
use blaze_ladder::front2::by_origin;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::keys::{MemberKind, checker_of, member_key_of};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{ConstValues, SeededHeads, SeededHeadsOpts, cell_id as head_cell_id, want_bytes};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::task::cell_id;
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_own::{Alloc, SharedAlloc, Vec};
use blaze_scope::items_record::{ImplRow, ItemsRecord, SelfHead};
use blaze_scope::resolve::At;
use blaze_surface::ItemIndex;
use blaze_world::crate_list::CrateGraph;
use bstd::cx::Cx;
use bstd::mem::Frame;
use bstd::sync::CachePadded;
use bstd::task::Class;
use std::alloc::Global;

use crate::CliError;
use crate::body_types::Kept;
use crate::engine_bodies::Handles;
use crate::engine_check::EngineCheckError;
use crate::engine_listing_record::{self as listing_record, name_part};
use crate::engine_names::NamesStage;
use crate::engine_records::EngineRecords;
use crate::engine_tasks::{CELLS, Outlined, WHOLE};
use crate::names::expansion_texts::ExpansionTexts;
use crate::run_names::Crate;
use crate::work;

/// A task's frame: its scope's records opened, its filing's scratch (blocks taken as used).
const HEADS_FRAME: usize = 1 << 30;

/// One `ImplBucket` task a crate of the names world (or the active scoped closure) (sorted by name).
pub(crate) fn plan<'h>(names: &NamesStage<'_>, scoped: Option<&crate::engine_check::ScopedUnits>, alloc: SharedAlloc<'h>) -> Result<SVec<'h, Planned>, CliError> {
    let mut planned = SVec::new_in(alloc);
    let Some(world) = names.names.as_ref() else { return Ok(planned) };
    for (name, source) in crate::names::parts(world).iter() {
        let at = world.crate_at(name.subject).ok_or(EngineCheckError::MissingEntry { task: *name, table: crate::engine_check::Table::WorldCrates, key: *name })?;
        if scoped.is_some_and(|s| !s.allows(at)) {
            continue;
        }
        // A crate with no check scope (refused by the list, or a dependency's proc macro kept for
        // its macros only) has no tables task: it could only fail, and no body asks a proc
        // macro's or a refused crate's impls (blaze-world check_scope.rs `has_check_scope`).
        // From the crate list (rule 78: one rule over either graph): no world is made to plan.
        if !blaze_world::check_scope::has_check_scope(world.list(), at) {
            continue;
        }
        let tables = impl_tables_name(world.list().key(at));
        planned.push(Planned { name: tables, phase: Phase::new(Stage::Heads, 0), source: *source, file: *name });
    }
    planned.sort_unstable_by_key(|one| one.name);
    Ok(planned)
}

/// One crate's tables as its task made them, all in `Global` (a worker's result crosses to the
/// coordinator, and the store's put reads it after the phase).
pub(crate) struct Made {
    pub(crate) name: Name,
    pub(crate) out: ContentId,
    /// Its read list as the store's record holds it: each read's record and digest, sorted.
    pub(crate) reads: std::vec::Vec<(Name, ContentId), Global>,
    /// The three records, [`framed`].
    pub(crate) payload: std::vec::Vec<u8, Global>,
}

/// What a worker hands back for one task.
struct Worked<'p> {
    out: ContentId,
    defines: SVec<'p, (Cell, ContentId)>,
    reads: SVec<'p, CellId>,
    /// At cell grain: each header's head read where `Owners::place` put it (a head task's cell,
    /// or a dependency's record whole), read as is ([`HeadsStage::run_phase`]).
    heads: SVec<'p, CellId>,
    payload: std::vec::Vec<u8, Global>,
}

/// One task's inputs, made on the coordinator.
struct Job<'r, 'p> {
    task: Task,
    krate: blaze_id::CrateItemKey,
    /// The crate's own `CrateScope` name: read whole.
    own: Name,
    /// The crate's own opened records (reader crate 0); every other crate in its check scope is
    /// opened by need through [`HeadsScopeRecords`] on workers.
    own_crate: Crate<'r>,
    /// Unopened crates of its check scope after crate 0, in reader order.
    rest: SVec<'p, crate::front_load::Unopened>,
    /// Each scope crate's `CrateScope` name, sorted: what a header read widens to.
    scope: SVec<'p, Name>,
    /// The cells a refusal of its tables is a function of ([`scope_cells`]).
    cells: SVec<'p, CellId>,
}

impl<'r> Job<'r, '_> {
    fn scope_crates<'s>(&'s self, records: &'s dyn crate::front_load::OpenByNeed) -> crate::run_names::ScopeCrates<'s, 's> {
        crate::run_names::ScopeCrates::ByNeed(crate::run_names::ByNeedScope { own: self.own_crate, rest: &self.rest[..], records })
    }
}

/// One crate's check scope for the Heads stage: its own records opened, every non-member crate
/// verified present in the committed snapshot and deferred to [`HeadsScopeRecords`] on workers.
struct HeadsScope<'r, 'p> {
    own: Crate<'r>,
    rest: SVec<'p, crate::front_load::Unopened>,
    lang: SVec<'p, usize>,
    scope: SVec<'p, Name>,
    cut: Option<(usize, crate::engine_bodies::CutWhy)>,
}

impl HeadsScope<'_, '_> {
    fn cut_name(&self, list: &blaze_world::crate_list::CrateList<'static>) -> Option<Name> {
        self.cut.as_ref().map(|(one, _)| blaze_drive::key::crate_scope_name(&list.key(*one)))
    }

    fn cut_cause(&self, list: &blaze_world::crate_list::CrateList<'static>) -> Option<crate::engine_bodies::CutCause<'_>> {
        self.cut.as_ref().map(|(one, why)| crate::engine_bodies::CutCause { krate: CrateGraph::name(list, *one), why })
    }
}

/// Per-phase cache of crate records opened through `EngineRecords` (`slots`: active members and
/// their member dependencies) and snapshot presence checks (`present`: non-member dependencies),
/// so `HeadsStage` avoids eagerly opening hundreds of external/toolchain `CrateScope` records on
/// the coordinator.
struct HeadsScopeCache<'r> {
    slots: Vec<Option<(Input, crate::front_load::CrateRecords<'r>)>, Alloc<'static>>,
    present: Vec<Option<crate::front_load::Unopened>, Alloc<'static>>,
}

impl<'r> HeadsScopeCache<'r> {
    fn new(len: usize, alloc: Alloc<'static>) -> HeadsScopeCache<'r> {
        let mut slots = Vec::with_capacity_in(len, alloc);
        slots.resize(len, None);
        let mut present = Vec::with_capacity_in(len, alloc);
        present.resize(len, None);
        HeadsScopeCache { slots, present }
    }

    #[expect(clippy::too_many_arguments, reason = "coordinator record cache lookup")]
    fn records_of(
        &mut self,
        read: &mut EngineRecords<'r>,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        names: crate::engine_names::NamesInputs<'_, '_>,
        list: &blaze_world::crate_list::CrateList<'static>,
        at: usize,
        g: Alloc<'static>,
    ) -> Result<(Input, crate::front_load::CrateRecords<'r>), crate::engine_bodies::ScopeRefusal> {
        if let Some(cached) = self.slots.get(at).copied().flatten() {
            return Ok(cached);
        }
        let name = blaze_drive::key::crate_scope_name(&list.key(at));
        let krate = CrateGraph::name(list, at);
        let input = names.input_of(name).ok_or_else(|| crate::engine_bodies::ScopeRefusal::NoNamesRecord { krate, at, failed: names.failed(name) })?;
        let records = read.scope_records(cx, store, &input, list.key(at), krate, g).map_err(|error| crate::engine_bodies::ScopeRefusal::Records { krate, at, error })?;
        if let Some(slot) = self.slots.get_mut(at) {
            *slot = Some((input, records));
        }
        Ok((input, records))
    }

    fn unopened_of(
        &mut self,
        snapshot: &crate::store::StoreSnapshot,
        names: crate::engine_names::NamesInputs<'_, '_>,
        list: &blaze_world::crate_list::CrateList<'static>,
        at: usize,
        unreadable: Option<&'static str>,
        g: Alloc<'static>,
    ) -> Result<Result<crate::front_load::Unopened, crate::engine_bodies::CutWhy>, CliError> {
        if let Some(cached) = self.present.get(at).copied().flatten() {
            return Ok(Ok(cached));
        }
        let scope_name = blaze_drive::key::crate_scope_name(&list.key(at));
        let Some(input) = names.input_of(scope_name) else {
            return Ok(Err(crate::engine_bodies::CutWhy::NoNamesRecord { failed: names.failed(scope_name) }));
        };
        let name = CrateGraph::name(list, at);
        if unreadable.is_some_and(|planted| planted == name) {
            return Ok(Err(crate::engine_bodies::CutWhy::Unreadable(
                crate::front_load::LoadError::NotInStore { name: input.name, at: input.at }.into(),
            )));
        }
        let unopened = crate::front_load::Unopened { input, key: list.key(at), world_at: at, name, package: list.shown_from(at) };
        match crate::front_load::present(snapshot, &unopened) {
            Ok(Ok(())) => {
                if let Some(slot) = self.present.get_mut(at) {
                    *slot = Some(unopened);
                }
                Ok(Ok(unopened))
            }
            Ok(Err(error)) => Ok(Err(crate::engine_bodies::CutWhy::Unreadable(error))),
            Err(error) => Err(CliError::Text {
                what: crate::engine_head_notes::leak(g, format_args!("`{}`: its check scope's crate `{name}` does not read from the store's snapshot: {error:?} (front_load.rs `present`)", list.root(at))),
            }),
        }
    }
}

/// Phase-wide lazy scope records for `Heads`: serves any crate already opened in
/// [`HeadsScopeCache`] (active members) directly without re-reading the store, and opens every
/// other scope crate at most once on demand from the shared request-wide [`ScopeRecords`].
struct HeadsScopeRecords<'r, 's, 'l, 'p> {
    preopened: SVec<'p, Option<crate::front_load::CrateRecords<'r>>>,
    lazy: &'s crate::front_load::ScopeRecords<'l>,
}

impl<'r, 's, 'l, 'p> HeadsScopeRecords<'r, 's, 'l, 'p> {
    fn new(cache: &HeadsScopeCache<'r>, lazy: &'s crate::front_load::ScopeRecords<'l>, crates: usize, at_phase: SharedAlloc<'p>) -> HeadsScopeRecords<'r, 's, 'l, 'p> {
        let mut preopened = SVec::with_capacity_in(crates, at_phase);
        preopened.extend(cache.slots.iter().map(|slot| slot.map(|(_, records)| records)));
        HeadsScopeRecords { preopened, lazy }
    }
}

fn by_need_delta(after: crate::front_load::ByNeedCounts, before: crate::front_load::ByNeedCounts) -> crate::front_load::ByNeedCounts {
    crate::front_load::ByNeedCounts {
        asks: after.asks.saturating_sub(before.asks),
        opened: after.opened.saturating_sub(before.opened),
        bytes: after.bytes.saturating_sub(before.bytes),
    }
}

impl crate::front_load::OpenByNeed for HeadsScopeRecords<'_, '_, '_, '_> {
    fn open<'s>(&'s self, one: &crate::front_load::Unopened) -> Result<crate::front_load::CrateRecords<'s>, &'static str> {
        if let Some(Some(records)) = self.preopened.get(one.world_at) {
            return Ok(*records);
        }
        self.lazy.open(one)
    }

    fn pack_file_items<'s>(
        &'s self,
        world_at: usize,
        nfiles: usize,
        file: u32,
        edition: blaze_surface::Edition,
        text: &'s str,
        verify_digest: bool,
    ) -> Option<Result<blaze_scope::items_record::CurrentFile<'s>, blaze_scope::items_record::ItemsRecordError>> {
        self.lazy.pack_file_items(world_at, nfiles, file, edition, text, verify_digest)
    }
}

/// Builds crate `at`'s check scope lazily: opens crate `at` and any workspace member crates in
/// its scope through `cache` (catching planted-unreadable members and preparing their `ItemsRecord`s
/// for `subjects`), while verifying non-member dependencies are present in `snapshot` without
/// opening or validating their payloads until a worker actually queries them.
#[expect(clippy::too_many_arguments, reason = "lazy check-scope builder for Heads stage")]
fn scope_by_need_heads<'r, 'p>(
    cache: &mut HeadsScopeCache<'r>,
    read: &mut EngineRecords<'r>,
    cx: &Cx,
    store: &mut dyn crate::store::Store<'r>,
    snapshot: &crate::store::StoreSnapshot,
    names: crate::engine_names::NamesInputs<'_, '_>,
    list: &blaze_world::crate_list::CrateList<'static>,
    at: usize,
    at_phase: SharedAlloc<'p>,
    frame: &Frame,
    heap: &'static blaze_own::Heap,
) -> Result<Result<(HeadsScope<'r, 'p>, bool), crate::engine_bodies::ScopeRefusal>, CliError> {
    let g = heap.alloc();
    let (_, own_records) = match cache.records_of(read, cx, store, names, list, at, g) {
        Ok(found) => found,
        Err(err) => return Ok(Err(err)),
    };
    let no_core = blaze_scope::names_record::NamesRecord::open(own_records.names_proof).crate_root().head().no_core;
    let erased = blaze_own::Erased::new(frame);
    let checked = match blaze_world::check_scope::check_scope_in(list, at, no_core, erased.handle()) {
        Ok(checked) => checked,
        Err(error) => return Ok(Err(crate::engine_bodies::ScopeRefusal::CheckScope { error })),
    };
    let own = Crate { records: own_records, name: CrateGraph::name(list, at), package: list.shown_from(at), world_at: at };
    let mut rest: SVec<'p, crate::front_load::Unopened> = SVec::with_capacity_in(checked.readers().len().saturating_sub(1), at_phase);
    let mut lang: SVec<'p, usize> = SVec::new_in(at_phase);
    crate::engine_bodies::lang_places_of(&checked, list, &mut lang);
    let mut scope: SVec<'p, Name> = SVec::with_capacity_in(checked.readers().len(), at_phase);
    let mut whole = true;
    let mut cut = None;
    for one in checked.crates() {
        if one == at {
            scope.push(blaze_drive::key::crate_scope_name(&list.key(at)));
            continue;
        }
        if list.member(one) {
            match cache.records_of(read, cx, store, names, list, one, g) {
                Ok((input, _)) => {
                    rest.push(crate::front_load::Unopened { input, key: list.key(one), world_at: one, name: CrateGraph::name(list, one), package: list.shown_from(one) });
                    scope.push(input.name);
                }
                Err(crate::engine_bodies::ScopeRefusal::NoNamesRecord { failed, .. }) => {
                    whole = false;
                    cut = Some((one, crate::engine_bodies::CutWhy::NoNamesRecord { failed }));
                    break;
                }
                Err(crate::engine_bodies::ScopeRefusal::Records { error, .. }) => {
                    whole = false;
                    cut = Some((one, crate::engine_bodies::CutWhy::Unreadable(error)));
                    break;
                }
                Err(crate::engine_bodies::ScopeRefusal::CheckScope { error }) => return Ok(Err(crate::engine_bodies::ScopeRefusal::CheckScope { error })),
            }
        } else {
            match cache.unopened_of(snapshot, names, list, one, read.unreadable(), g)? {
                Ok(unopened) => {
                    scope.push(unopened.input.name);
                    rest.push(unopened);
                }
                Err(why) => {
                    whole = false;
                    cut = Some((one, why));
                    break;
                }
            }
        }
    }
    scope.sort_unstable();
    Ok(Ok((HeadsScope { own, rest, lang, scope, cut }, whole)))
}

/// Builds lightweight crate labels (name, package, world index, and crate key) in `frame` for
/// `UnplacedIn` error formatting without opening any unopened dependency records.
fn scope_labels<'n>(own: Crate<'n>, rest: &[crate::front_load::Unopened], frame: &'n Frame) -> &'n [Crate<'n>] {
    let mut labels: Vec<Crate<'n>, &'n Frame> = Vec::with_capacity_in(rest.len() + 1, frame);
    labels.push(own);
    for one in rest.iter() {
        labels.push(Crate { records: crate::front_load::CrateRecords { key: one.key, ..own.records }, name: one.name, package: one.package, world_at: one.world_at });
    }
    labels.into_bump_slice()
}

fn scope_crates_labels<'n>(crates: crate::run_names::ScopeCrates<'_, 'n>, frame: &'n Frame) -> &'n [Crate<'n>] {
    match crates {
        crate::run_names::ScopeCrates::Opened(crates) => {
            let mut labels: Vec<Crate<'n>, &'n Frame> = Vec::with_capacity_in(crates.len(), frame);
            labels.extend_from_slice(crates);
            labels.into_bump_slice()
        }
        crate::run_names::ScopeCrates::ByNeed(scope) => scope_labels(scope.own, scope.rest, frame),
    }
}

/// Hash index of `outlined` by physical checkout path in phase memory for O(1) lookup by path.
fn outlined_by_path<'o, 'p>(outlined: &'o [Outlined<'_>], at_phase: SharedAlloc<'p>) -> blaze_hash::Map<&'o [u8], usize, blaze_hash::FixedState, SharedAlloc<'p>> {
    let mut index: blaze_hash::Map<&'o [u8], usize, blaze_hash::FixedState, SharedAlloc<'p>> =
        blaze_hash::Map::new(blaze_hash::MapOpts { hasher: blaze_hash::FixedState, alloc: at_phase, capacity: Some(outlined.len()) });
    for (at, one) in outlined.iter().enumerate() {
        index.entry(one.path.as_slice()).or_insert(at);
    }
    index
}

/// First outline index in `outlined` whose path equals `path`, via [`outlined_by_path`].
fn find_outlined_index<'q>(
    by_path: &blaze_hash::Map<&'q [u8], usize, blaze_hash::FixedState, SharedAlloc<'_>>,
    path: &'q [u8],
) -> Option<usize> {
    by_path.get(&path).copied()
}

/// Phase-local cache of `(SignatureFile, entries_of)` per outlined file index (`0..outlined.len()`),
/// so multi-unit crates (lib + test + bin) hash each file's `FileItemsView` at most once per phase
/// and large batches fan out across all workers (rule 25 deterministic output).
struct SubjectEntries<'p> {
    cached: SVec<'p, Option<(crate::signature::SignatureFile, ContentId)>>,
}

impl<'p> SubjectEntries<'p> {
    fn new(at_phase: SharedAlloc<'p>) -> SubjectEntries<'p> {
        SubjectEntries { cached: SVec::new_in(at_phase) }
    }

    #[expect(clippy::too_many_arguments, reason = "phase-wide subject entry cache and worker pool fanout")]
    fn populate(
        &mut self,
        cx: &Cx,
        outlined: &[Outlined<'_>],
        pending: &[([u8; 8], u32, usize)],
        subjects: &mut SVec<'p, Subject>,
        max_workers: NonZeroU32,
        at_phase: SharedAlloc<'p>,
        frame: &Frame,
    ) -> Result<(), CliError> {
        if pending.is_empty() {
            return Ok(());
        }
        if self.cached.len() < outlined.len() {
            self.cached.resize(outlined.len(), None);
        }
        let mut missing: Vec<usize, &Frame> = Vec::new_in(frame);
        for &(_, _, idx) in pending.iter() {
            if self.cached.get(idx).copied().flatten().is_none() {
                missing.push(idx);
            }
        }
        if !missing.is_empty() {
            missing.sort_unstable();
            missing.dedup();
            let workers = usize::try_from(cx.workers().min(max_workers.get())).unwrap_or(1).max(1);
            if workers > 1 && missing.len() >= 32 {
                let missing_slice = missing.as_slice();
                let chunks = workers.min(missing_slice.len().div_ceil(16)).max(1);
                let chunk_len = missing_slice.len().div_ceil(chunks);
                let scope = cx.scope();
                scope.narrow_workers(max_workers, Class::Foreground);
                let mut running = Vec::with_capacity_in(chunks, frame);
                for chunk in missing_slice.chunks(chunk_len) {
                    running.push(scope.may_overlap("engine heads: subject entries", bstd::mem::TASK_FRAME, move |_cx: &Cx| {
                        let mut out: SVec<'p, (usize, crate::signature::SignatureFile, ContentId)> = SVec::with_capacity_in(chunk.len(), at_phase);
                        for &idx in chunk.iter() {
                            if let Some(one) = outlined.get(idx) {
                                out.push((idx, crate::signature::SignatureFile::new(&one.path), crate::engine_listing_record::entries_of(one)));
                            }
                        }
                        Ok::<_, Infallible>(out)
                    }));
                }
                let mut stopped = None;
                for handle in running.into_iter() {
                    match handle.join(cx) {
                        Ok(part) => {
                            for (idx, sig, entries) in part.into_iter() {
                                if let Some(slot) = self.cached.get_mut(idx) {
                                    *slot = Some((sig, entries));
                                }
                            }
                        }
                        Err(error) => {
                            stopped.get_or_insert(error);
                        }
                    }
                }
                scope.join(cx).map_err(CliError::Pool)?;
                if let Some(error) = stopped {
                    return Err(CliError::Pool(error));
                }
            } else {
                for &idx in missing.iter() {
                    if let (Some(one), Some(slot)) = (outlined.get(idx), self.cached.get_mut(idx)) {
                        *slot = Some((crate::signature::SignatureFile::new(&one.path), crate::engine_listing_record::entries_of(one)));
                    }
                }
            }
        }
        subjects.reserve(pending.len());
        for &(prefix, file, idx) in pending.iter() {
            if let (Some(one), Some(Some((signature_file, entries)))) = (outlined.get(idx), self.cached.get(idx).copied()) {
                subjects.push(Subject { prefix, file, slot: one.name, signature_file: SubjectFile::Resolved(signature_file), entries });
            }
        }
        Ok(())
    }
}

/// The cells a refusal over crate `at`'s check scope is a function of (plan row A8): the
/// graph's (the crate list, the check scope and the packs are its record's), the crate's own
/// names cell, and with its `scope` each scope crate's names cell and the crate it was cut at
/// ([`HeadsScope::cut`]), each `WHOLE`. A crate whose names task failed or did
/// not run defines no cell: read ABSENT, so its task that comes, or its fix, moves the row.
/// Unsorted: `failed_with` sorts and dedups.
fn scope_cells<'p>(graph: Name, list: &blaze_world::crate_list::CrateList<'static>, at: usize, scope: Option<&HeadsScope<'_, '_>>, at_phase: SharedAlloc<'p>) -> SVec<'p, CellId> {
    let mut cells: SVec<'p, CellId> = SVec::with_capacity_in(2 + scope.map_or(1, |scope| scope.scope.len() + 1), at_phase);
    cells.push(CellId { of: graph, cell: WHOLE });
    cells.push(CellId { of: blaze_drive::key::crate_scope_name(&list.key(at)), cell: WHOLE });
    if let Some(scope) = scope {
        cells.extend(scope.scope.iter().map(|one| CellId { of: *one, cell: WHOLE }));
        cells.extend(scope.cut_name(list).map(|one| CellId { of: one, cell: WHOLE }));
    }
    cells
}

/// The stage: each task's result and refusal.
pub(crate) struct HeadsStage {
    /// Each task entered and made, sorted by name after its phase.
    made: std::vec::Vec<Made, Global>,
    /// Each task refused, and each head a task could not freeze: which and why (rule 19).
    pub(crate) refused: std::vec::Vec<crate::engine_head_notes::Refused, Global>,
    /// Crates whose standing tables were read back from the store this request.
    pub(crate) read_back: u32,
    /// Under which units a head task freezes (the command's: [`HeadUnits::Each`]).
    pub(crate) freeze: HeadUnits,
    /// What the head tasks entered this request froze, by unit.
    pub(crate) units: UnitCounts,
    /// How the `ImplBucket` tasks' reads are kept (engine_scope_reads.rs
    /// `ScopeReads::TABLES_AT_CELLS`), the command's choice, as the bodies'.
    scope_reads: crate::engine_scope_reads::ScopeReadsAs,
    /// Which source strategy `HeadsOnce` uses (rule 78: `Sources::Job` in production).
    pub(crate) head_sources: crate::engine_heads_once::Sources,
    /// Counts folded from `HeadsOnce` when `Stage::Heads` finishes and its table/arena are dropped.
    pub(crate) once_counts: crate::engine_heads_once::OnceCounts,
    /// Closed dependency head keys put by `HeadsOnce` during `Stage::Heads` (sorted), kept after
    /// `HeadsOnce` is dropped so `Bodies` lists them on the seed root.
    once_put_keys: Option<Vec<Key, SharedAlloc<'static>>>,
    /// How a crate's head tasks are cut into pool jobs and its listing shared (the command's:
    /// [`HeadJobs::RULE`]; a test's plant otherwise).
    pub(crate) jobs: HeadJobs,
    /// The request's crate listings ([`Listings`]), each crate's once: made at the first phase
    /// that asks (a Heads phase, or a Bodies phase through [`HeadsStage::listings`]) and kept to
    /// the request's end, for every Heads job and every Bodies stretch.
    listings: Option<Listings>,
}

impl HeadsStage {
    pub(crate) fn new(scope_reads: crate::engine_scope_reads::ScopeReadsAs) -> HeadsStage {
        HeadsStage { made: std::vec::Vec::new_in(Global), refused: std::vec::Vec::new_in(Global), read_back: 0, freeze: HeadUnits::Each, units: UnitCounts::default(), scope_reads, head_sources: crate::engine_heads_once::Sources::Job, once_counts: crate::engine_heads_once::OnceCounts::default(), once_put_keys: None, jobs: HeadJobs::RULE, listings: None }
    }

    /// Saves the summary counts and put keys from `HeadsOnce` right before `HeadsOnce` is dropped
    /// at the end of `Stage::Heads`.
    pub(crate) fn finish_once(&mut self, counts: crate::engine_heads_once::OnceCounts, put_keys: &[Key], alloc: SharedAlloc<'static>) {
        self.once_counts = counts;
        if !put_keys.is_empty() {
            let kept = self.once_put_keys.get_or_insert_with(|| Vec::new_in(alloc));
            kept.extend_from_slice(put_keys);
            kept.sort_unstable();
            kept.dedup();
        }
    }

    /// Closed dependency head keys put during `Stage::Heads`, sorted.
    pub(crate) fn once_put_keys(&self) -> &[Key] {
        self.once_put_keys.as_deref().unwrap_or(&[])
    }

    /// The request's crate listings over its heap (`heap`: the request's, the one the Heads
    /// phases are handed, `Handles::env.heap`), made on first ask: what the Bodies stage's
    /// stretches place their head reads by (engine_bodies.rs `run_stretch`, engine_seeds.rs
    /// `StretchHeads`), the same table the head tasks listed their crates into. `snapshot`: the
    /// committed records its listings are read from by key (`None`: each made, counted
    /// `unstored`); `tool`: the tool their keys hold (`Handles::tool`). Taken at the table's
    /// making only: a table a Heads phase made keeps that phase's.
    pub(crate) fn listings(&mut self, heap: &'static blaze_own::Heap, snapshot: Option<&crate::store::SharedSnapshot>, tool: blaze_drive::Tool) -> &Listings {
        let plant = self.jobs.listings;
        self.listings.get_or_insert_with(|| Listings::new(heap, plant, snapshot.cloned(), tool))
    }

    /// The listing records made this request and not yet put, put under their keys at the
    /// phase's merge (engine_check.rs, after `put_all`, at every phase: a Bodies stretch makes
    /// listings too), counted in `ledger` under `stage`; how many.
    pub(crate) fn put_listings(&mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, ledger: &mut crate::engine_store::Ledger, stage: Stage) -> Result<u32, CliError> {
        match &self.listings {
            Some(listings) => listings.put_made(cx, store, ledger, stage),
            None => Ok(0),
        }
    }

    /// The keys of the listing records this request read or put, sorted: what the answer's root
    /// names beside the member listing rows (engine_answer.rs `save_edit`'s `row_keys`), or the
    /// next command makes each again.
    pub(crate) fn listing_keys(&self, g: Alloc<'static>) -> Vec<Key, Alloc<'static>> {
        match &self.listings {
            Some(listings) => listings.keys(g),
            None => Vec::new_in(g),
        }
    }

    /// The head tasks' counts with the listings' as they stand now (the Bodies stretches ask the
    /// table after the Heads phases ran, so read at the profile, not at a phase's end).
    pub(crate) fn unit_counts(&self) -> UnitCounts {
        let mut counts = self.units;
        if let Some(listings) = &self.listings {
            counts.listings = listings.counts();
        }
        counts
    }

    /// Task `name`'s record, when it was entered and made this request.
    pub(crate) fn result(&self, name: Name) -> Option<&Made> {
        self.made.binary_search_by_key(&name, |one| one.name).ok().map(|at| &self.made[at])
    }

    /// Releases `name`'s handed payload bytes and record read list once `put_phase` has committed
    /// them to the store (rule 51): keeps the `Made` header so `engine_head_notes` still knows
    /// the task ran.
    pub(crate) fn release_payload(&mut self, name: Name) -> usize {
        let Ok(at) = self.made.binary_search_by_key(&name, |one| one.name) else {
            return 0;
        };
        let Some(one) = self.made.get_mut(at) else {
            return 0;
        };
        let freed = one.payload.len();
        one.payload.clear();
        one.payload.shrink_to_fit();
        one.reads.clear();
        one.reads.shrink_to_fit();
        freed
    }

    /// The Heads phase: every task entered once, its result in `buffer`, its tables on `shelf`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    pub(crate) fn run_phase<'a, 'p, 'r, 'l>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        handles: &Handles<'_, '_, '_, '_>,
        kept: &mut Kept,
        shelf: &mut ImplShelf,
        tasks: &[Task],
        view: View<'_, 'a>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        at_phase: SharedAlloc<'p>,
        frame: &Frame,
        heads_snap: &'l core::cell::OnceCell<crate::store::SharedSnapshot>,
        heads_lazy: &mut Option<crate::front_load::ScopeRecords<'l>>,
        once: &crate::engine_heads_once::HeadsOnce,
    ) -> Result<(), CliError> {
        let cx = cx.span("engine: the heads phase");
        let snapshot_owner = store.snapshot()?.ok_or(CliError::Text { what: "the Heads stage needs a committed store snapshot for expansion text (engine_heads.rs)" })?;
        let snapshot = snapshot_owner.as_ref();
        // Reuse one immutable post-Names snapshot across all Heads waves so non-member scope
        // records (`core`, `alloc`, `std`, deps) open and validate at most once per request.
        let lazy_snap = heads_snap.get_or_init(|| blaze_own::Share::share(&snapshot_owner));
        let _ = self.listings(handles.env.heap, Some(&snapshot_owner), handles.tool);
        // The per-item head tasks first ([`HeadsStage::run_items`]), then the crates' tables.
        // Closed-dep impl tables (`!member` with no member in scope) depend only on earlier Names
        // records and overlap directly with `HeadsSetup` and `HeadsSplit` on the same worker pool.
        let mut items: SVec<'p, Task> = SVec::new_in(at_phase);
        let mut table_tasks: SVec<'p, Task> = SVec::new_in(at_phase);
        for &task in tasks.iter() {
            if task.planned.name.kind == Kind::Signature {
                items.push(task);
            } else {
                table_tasks.push(task);
            }
        }
        let tasks = &table_tasks[..];
        let heap = handles.env.heap;
        let g = heap.alloc();
        let at_cells = self.scope_reads.tables_at_cells();
        let listings_plant = self.jobs.listings;
        let mut scope_cache: Option<HeadsScopeCache<'r>> = None;
        let mut closed_jobs: SVec<'p, (usize, Job<'r, 'p>)> = SVec::new_in(at_phase);
        let mut pre_refused: SVec<'p, (usize, &'static str, SVec<'p, CellId>)> = SVec::new_in(at_phase);
        if !items.is_empty() && !tasks.is_empty()
            && let (Some(graph), Some(names)) = (handles.graph, handles.names)
            && let Some(list) = kept.list(&cx, store, &graph, frame)?
            && kept.lazy_trees(handles.env, graph.out, list).is_ok()
        {
            let cache = scope_cache.get_or_insert_with(|| HeadsScopeCache::new(CrateGraph::len(&list), g));
            let mut by_name: SVec<'p, (Name, usize)> = SVec::with_capacity_in(CrateGraph::len(&list), at_phase);
            by_name.extend((0..CrateGraph::len(&list)).map(|at| (impl_tables_name(list.key(at)), at)));
            by_name.sort_unstable();
            for (idx, task) in tasks.iter().enumerate() {
                let Ok(found) = by_name.binary_search_by_key(&task.planned.name, |(name, _)| *name) else {
                    continue;
                };
                let Some(&(_, at)) = by_name.get(found) else {
                    continue;
                };
                if list.member(at) {
                    continue;
                }
                match scope_by_need_heads(cache, read, &cx, store, snapshot, names, &list, at, at_phase, frame, heap)? {
                    Ok((scope, true)) => {
                        if scope.rest.iter().any(|one| list.member(one.world_at)) {
                            continue;
                        }
                        let cells = scope_cells(graph.name, &list, at, Some(&scope), at_phase);
                        let own = names.input_of(blaze_drive::key::crate_scope_name(&list.key(at))).map(|input| input.name);
                        if let Some(own) = own {
                            closed_jobs.push((idx, Job { task: *task, krate: list.key(at), own, own_crate: scope.own, rest: scope.rest, scope: scope.scope, cells }));
                        } else {
                            pre_refused.push((idx, "its crate has no names record this command (its `Names` task failed)", cells));
                        }
                    }
                    Ok((scope, false)) => {
                        let why = match scope.cut_cause(&list) {
                            Some(cause) => crate::engine_head_notes::leak(g, format_args!("its crate's {cause}")),
                            None => "its crate's check scope was cut and `scope_of` kept no cut (engine_bodies.rs `scope_of`)",
                        };
                        let cells = scope_cells(graph.name, &list, at, Some(&scope), at_phase);
                        pre_refused.push((idx, why, cells));
                    }
                    Err(cause) => {
                        let cells = scope_cells(graph.name, &list, at, None, at_phase);
                        pre_refused.push((idx, crate::engine_head_notes::leak(g, format_args!("its crate's check scope was not read: {cause}")), cells));
                    }
                }
            }
        }
        let mut by_path: Option<blaze_hash::Map<&[u8], usize, blaze_hash::FixedState, SharedAlloc<'p>>> = None;
        let mut subject_entries = SubjectEntries::new(at_phase);
        let mut phase_subjects: SVec<'p, Subject> = SVec::new_in(at_phase);
        let mut loaded_prefixes: SVec<'p, [u8; 8]> = SVec::new_in(at_phase);
        let closed_worked = self.run_items(&cx, store, read, handles, kept, &items, view, buffer, times, alloc, at_phase, frame, snapshot, &snapshot_owner, &mut scope_cache, &closed_jobs, at_cells, lazy_snap.as_ref(), heads_lazy, once, &mut by_path, &mut subject_entries, &mut phase_subjects, &mut loaded_prefixes)?;
        if tasks.is_empty() {
            return Ok(());
        }
        // Stopped while the head tasks ran (engine_status.rs): the crates' tables are not begun,
        // each refused and owed (no read), and the driver stops before the merge.
        if handles.board.stopping().is_some() {
            handles.board.refused(Stage::Heads, tasks.len() as u64);
            buffer.done.extend(tasks.iter().map(|task| crate::engine_tasks::failed(task, alloc)));
            return Ok(());
        }
        let listings: &Listings = self.listings.get_or_insert_with(|| Listings::new(heap, listings_plant, Some(snapshot_owner.clone()), handles.tool));
        let refused = &mut self.refused;
        // A refusal with the cells it is a function of, each read as the view holds it today
        // (ABSENT where no task defines it: a crate whose names task failed or did not run), so
        // its row stands until one moves (plan row A8). None: owed, entered again each request,
        // said at each such call.
        let mut refuse = |task: &Task, why: &'static str, cells: &[CellId], buffer: &mut Buffer<'a>| {
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
            reads.extend(cells.iter().map(|id| (*id, view.cell(id))));
            buffer.done.push(crate::engine_tasks::failed_with(task, reads, alloc));
            refused.push(crate::engine_head_notes::Refused::whole(task.planned.name, "its crate's impl tables", why));
        };
        // 1. Each task's scope, on this thread.
        let (Some(graph), Some(names)) = (handles.graph, handles.names) else {
            // No `UnitGraph` task planned, or no names stage: no cell of this command says so,
            // so owed until the plan's inputs are cells (plan row D2).
            for task in tasks.iter() {
                refuse(task, "this command has no unit graph or no crate's names, so no crate's impl tables", &[], buffer);
            }
            return Ok(());
        };
        // The crate list, each crate's check scope and the packs are the graph record's.
        let graph_cell = CellId { of: graph.name, cell: WHOLE };
        let Some(list) = kept.list(&cx, store, &graph, frame)? else {
            for task in tasks.iter() {
                refuse(task, "the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)", &[graph_cell], buffer);
            }
            return Ok(());
        };
        let (packs, corrected) = match kept.lazy_trees(handles.env, graph.out, list) {
            Ok(trees) => trees,
            Err(why) => {
                // The heap refused room for the tree table (body_types.rs `lazy_trees`): owed.
                for task in tasks.iter() {
                    refuse(task, why, &[], buffer);
                }
                return Ok(());
            }
        };
        let cache = scope_cache.get_or_insert_with(|| HeadsScopeCache::new(CrateGraph::len(&list), g));
        // Each crate of the list by its tables' name.
        let mut by_name: SVec<'p, (Name, usize)> = SVec::with_capacity_in(CrateGraph::len(&list), at_phase);
        by_name.extend((0..CrateGraph::len(&list)).map(|at| (impl_tables_name(list.key(at)), at)));
        by_name.sort_unstable();
        let mut jobs: SVec<'p, Job<'r, 'p>> = SVec::with_capacity_in(tasks.len(), at_phase);
        let mut job_indices: SVec<'p, usize> = SVec::with_capacity_in(tasks.len(), at_phase);
        let mut closed_cursor = 0usize;
        let mut pre_refused_cursor = 0usize;
        for (idx, task) in tasks.iter().enumerate() {
            if closed_jobs.get(closed_cursor).is_some_and(|(at, _)| *at == idx) {
                closed_cursor += 1;
                continue;
            }
            if let Some(&(_, why, ref cells)) = pre_refused.get(pre_refused_cursor).filter(|(at, _, _)| *at == idx) {
                pre_refused_cursor += 1;
                refuse(task, why, cells, buffer);
                continue;
            }
            let Ok(found) = by_name.binary_search_by_key(&task.planned.name, |(name, _)| *name) else {
                // The list is the graph's: a new graph record moves it.
                refuse(task, "an `ImplBucket` task of a crate the crate list does not hold", &[graph_cell], buffer);
                continue;
            };
            let Some(&(_, at)) = by_name.get(found) else {
                refuse(task, "an `ImplBucket` task of a crate the crate list does not hold", &[graph_cell], buffer);
                continue;
            };
            match scope_by_need_heads(cache, read, &cx, store, snapshot, names, &list, at, at_phase, frame, heap)? {
                Ok((scope, true)) => {
                    let cells = scope_cells(graph.name, &list, at, Some(&scope), at_phase);
                    let own = names.input_of(blaze_drive::key::crate_scope_name(&list.key(at))).map(|input| input.name);
                    let Some(own) = own else {
                        refuse(task, "its crate has no names record this command (its `Names` task failed)", &cells, buffer);
                        continue;
                    };
                    jobs.push(Job { task: *task, krate: list.key(at), own, own_crate: scope.own, rest: scope.rest, scope: scope.scope, cells });
                    job_indices.push(idx);
                }
                // The crate's names cell and each scope crate's that had records, and the first
                // that had none read ABSENT (`HeadsScope::cut`): its task that comes, or its fix,
                // moves the row (engine_bodies.rs `scope_of`).
                // The cut's cause printed, typed, the crate named (rule 19).
                Ok((scope, false)) => {
                    let why = match scope.cut_cause(&list) {
                        Some(cause) => crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's {cause}")),
                        None => "its crate's check scope was cut and `scope_of` kept no cut (engine_bodies.rs `scope_of`)",
                    };
                    refuse(task, why, &scope_cells(graph.name, &list, at, Some(&scope), at_phase), buffer);
                }
                // `scope_by_need_heads` reads the crate's own names record (ABSENT when its task failed)
                // and the check scope the graph makes: both cells. Its cause printed, typed.
                Err(cause) => refuse(task, crate::engine_head_notes::leak(heap.alloc(), format_args!("its crate's check scope was not read: {cause}")), &scope_cells(graph.name, &list, at, None, at_phase), buffer),
            }
        }
        // Source text belongs to its outline under either names-read strategy; only member crates
        // appearing in the active jobs' scopes can ever be queried by `Owners::place`. Reuse any
        // member subjects and path index already built in `run_items` for this phase.
        let subjects: &Subjects = if !jobs.is_empty() {
            member_subjects(cache, read, &cx, store, names, &list, handles.outlined, &jobs, &mut by_path, &mut subject_entries, &mut phase_subjects, &mut loaded_prefixes, handles.workers, at_phase, frame, heap)?;
            &phase_subjects[..]
        } else {
            &[]
        };
        for (input, records) in cache.slots.iter().copied().flatten() {
            listings.record_scope_key(records.key, input.at);
        }
        let lazy = heads_lazy.get_or_insert_with(|| crate::front_load::ScopeRecords::new(lazy_snap.as_ref(), CrateGraph::len(&list), heap));
        let lazy_before = lazy.counts();
        let scope_records = HeadsScopeRecords::new(cache, lazy, CrateGraph::len(&list), at_phase);
        let scope_records = &scope_records;
        // 2. The tables, largest-first (LPT) on the workers; restored to task order below.
        let procs = names.stage().names.as_ref().and_then(|names| names.procs());
        let root = handles.root;
        let fs = &handles.env.fs;
        let outlined = handles.outlined;
        let clock = handles.clock;
        let max_workers = handles.workers;
        let mut worked: SVec<'p, (Result<Result<Worked<'p>, &'static str>, work::Panicked>, u64)> = SVec::with_capacity_in(jobs.len(), at_phase);
        if !jobs.is_empty() {
            let mut job_dispatch: Vec<usize, Alloc<'static>> = Vec::with_capacity_in(jobs.len(), g);
            job_dispatch.extend(0..jobs.len());
            job_dispatch.sort_unstable_by_key(|&at| {
                let weight = jobs.get(at).map_or(0, |job| impl_job_weight(job, g));
                (core::cmp::Reverse(weight), at)
            });
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let mut running = Vec::with_capacity_in(jobs.len(), g);
            for at in job_dispatch.into_iter() {
                let Some(job) = jobs.get(at) else { continue };
                running.push((
                    at,
                    scope.may_overlap("engine impl tables", HEADS_FRAME, move |cx: &Cx| {
                        let began = clock.now();
                        let made = work::unwinding(|| tables_of(cx, root, fs, outlined, packs, corrected, procs, job, scope_records, subjects, at_cells, heap, listings, snapshot, max_workers, at_phase));
                        Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns), crate::engine_times::ns(began.since_start())))
                    }),
                ));
            }
            let mut worked_paired: SVec<'p, (usize, (Result<Result<Worked<'p>, &'static str>, work::Panicked>, u64))> = SVec::with_capacity_in(jobs.len(), at_phase);
            let mut stopped = None;
            for (at, handle) in running.into_iter() {
                match handle.join(&cx) {
                    Ok((made, ns, start)) => {
                        times.job(crate::engine_times::Step::HeadsTables, start, start.saturating_add(ns));
                        worked_paired.push((at, (made, ns)));
                    }
                    Err(error) => {
                        stopped.get_or_insert(error);
                    }
                }
            }
            scope.join(&cx).map_err(CliError::Pool)?;
            read.add_by_need(by_need_delta(lazy.counts(), lazy_before));
            if let Some(error) = stopped {
                return Err(CliError::Pool(error));
            }
            // Rule 25: restore exact original `jobs` order before merging rows and shelf tables.
            worked_paired.sort_unstable_by_key(|(at, _)| *at);
            worked.extend(worked_paired.into_iter().map(|(_, res)| res));
        }
        // 3. Each result as the engine's row, its tables on the shelf, in task order.
        // Build a lazy Name -> first Done index map so `one.heads` lookups are O(1) instead of
        // scanning all of `buffer.done` per head read.
        let mut done_index: Option<blaze_hash::HashMap<Name, usize, SharedAlloc<'p>>> = None;
        let mut closed_iter = closed_jobs.iter().zip(closed_worked.into_iter()).peekable();
        let mut rem_iter = job_indices.iter().copied().zip(jobs.iter().zip(worked.into_iter())).peekable();
        loop {
            let next_closed = closed_iter.peek().map(|((idx, _), _)| *idx);
            let next_rem = rem_iter.peek().map(|(idx, _)| *idx);
            let (job, (made, ns)) = match (next_closed, next_rem) {
                (Some(c), Some(r)) if c <= r => {
                    let Some(((_, job), result)) = closed_iter.next() else { break };
                    (job, (result.made, result.ns))
                }
                (Some(_), None) => {
                    let Some(((_, job), result)) = closed_iter.next() else { break };
                    (job, (result.made, result.ns))
                }
                (_, Some(_)) => {
                    let Some((_, (job, result))) = rem_iter.next() else { break };
                    (job, result)
                }
                (None, None) => break,
            };
            let task = &job.task;
            let name = task.planned.name;
            times.push(name, ns);
            let one = match made {
                Ok(Ok(one)) => one,
                // A pack missing from the shelf or sources store is a machine failure: owed.
                Ok(Err(why)) if packs.refused().is_some() => {
                    refuse(task, why, &[], buffer);
                    continue;
                }
                // `tables_of` over the scope's records: a refusal of them is a function of the
                // graph's cell (the packs) and each scope crate's names cell.
                Ok(Err(why)) => {
                    refuse(task, why, &job.cells, buffer);
                    continue;
                }
                // A panic is not a function of reads: owed, entered again each request (rule 20).
                Err(work::Panicked) => {
                    refuse(task, "its impl tables' task panicked; that crate's tables alone failed", &[], buffer);
                    continue;
                }
            };
            if shelf.put_framed(job.krate, &one.payload).is_err() {
                // The request's shelf, filled this request (its cap, its order): no cell, so owed.
                refuse(task, "its tables could not be put on the request's shelf (front2 buckets/records.rs `ImplShelf::put_framed`)", &[], buffer);
                continue;
            }
            let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(one.defines.len() + 1, alloc);
            defines.push((WHOLE, one.out));
            defines.extend(one.defines.iter().copied());
            defines.sort_unstable_by_key(|(cell, _)| *cell);
            defines.dedup_by_key(|(cell, _)| *cell);
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(one.reads.len() + 1, alloc);
            let own = match at_cells {
                true => names_read(view, CellId { of: job.own, cell: Cell(blaze_scope::names_cells::NameCell::ImplRows { krate: job.krate.prefix() }.key()) }),
                false => CellId { of: job.own, cell: WHOLE },
            };
            reads.push((own, view.cell(&own)));
            for id in one.reads.iter() {
                if id.of.kind == Kind::Signature {
                    for crate_scope in job.scope.iter() {
                        let whole = CellId { of: *crate_scope, cell: WHOLE };
                        reads.push((whole, view.cell(&whole)));
                    }
                } else {
                    // A names cell kept by `tables_of` (at cell grain) as its crate defines it.
                    let id = names_read(view, *id);
                    reads.push((id, view.cell(&id)));
                }
            }
            // Each header's head placed at its head task's cell (at cell grain, `tables_of`): a
            // head task of this phase, entered before these tables (`run_items`), is read at the
            // digest its row holds now, so the merge sends this task to no second wave for a
            // head it saw (as a head's read of a sibling head, `job_cells`).
            if !one.heads.is_empty() {
                let index = done_index.get_or_insert_with(|| {
                    let mut m = blaze_hash::HashMap::with_capacity_in(buffer.done.len(), at_phase);
                    for (i, d) in buffer.done.iter().enumerate() {
                        m.entry(d.name).or_insert(i);
                    }
                    m
                });
                for id in one.heads.iter() {
                    let made = index
                        .get(&id.of)
                        .and_then(|&i| buffer.done.get(i))
                        .and_then(|done| done.defines.binary_search_by_key(&id.cell, |(cell, _)| *cell).ok().and_then(|at| done.defines.get(at)).map(|(_, d)| *d));
                    reads.push((*id, made.unwrap_or_else(|| view.cell(id))));
                }
            }
            reads.sort_unstable_by_key(|(id, _)| *id);
            reads.dedup_by_key(|(id, _)| *id);
            let unique_names = reads.chunk_by(|a, b| a.0.of == b.0.of).count();
            let mut record_reads: std::vec::Vec<(Name, ContentId), Global> = std::vec::Vec::with_capacity_in(unique_names, Global);
            for group in reads.chunk_by(|a, b| a.0.of == b.0.of) {
                if let Some((id, digest)) = group.first() {
                    record_reads.push((id.of, *digest));
                }
            }
            if let Some(index) = done_index.as_mut() {
                index.entry(name).or_insert(buffer.done.len());
            }
            buffer.done.push(Done { name, key: task.key, out: one.out, defines, reads, findings: 0, failed: false });
            self.made.push(Made { name, out: one.out, reads: record_reads, payload: one.payload });
        }
        self.made.sort_unstable_by_key(|one| one.name);
        Ok(())
    }
}

/// Before a Bodies phase: the tables of each crate whose Heads task stands from the stored
/// answer (not entered this request), read back from its record in the store by the key the
/// answer keeps (`record`), so every body reads one shelf holding every crate. A crate whose
/// task failed, or that has no row, is left off: a body asking it fails naming the crate
/// (buckets/records.rs `NoTables`), never answered from less (rule 3).
pub(crate) fn complete(cx: &Cx, store: &mut dyn crate::store::Store<'_>, names: &NamesStage<'_>, scoped: Option<&crate::engine_check::ScopedUnits>, shelf: &mut ImplShelf, record: &dyn Fn(Name) -> Option<Key>, view: View<'_, '_>, frame: &Frame) -> Result<u32, CliError> {
    let Some(world) = names.names.as_ref() else { return Ok(0) };
    let mut read_back = 0u32;
    let list = world.list();
    for r in 0..blaze_world::crate_list::CrateGraph::len(list) {
        if scoped.is_some_and(|s| !s.allows(r)) {
            continue;
        }
        if !blaze_world::check_scope::has_check_scope(list, r) {
            continue;
        }
        let key = list.key(r);
        if shelf.holds(key) {
            continue;
        }
        let name = impl_tables_name(key);
        let Some(row) = view.row(name) else { continue };
        if row.failed {
            continue;
        }
        let Some(at) = record(name) else {
            return Err(CliError::Text { what: "a standing `ImplBucket` row has no record key (pages.rs `records`; engine_heads.rs `complete`)" });
        };
        let read = work::payload(cx, store, &Input { name, at, out: row.out }, frame)?;
        shelf.put_framed(key, read.payload()).map_err(|_| CliError::Text { what: "a standing crate's impl tables read back malformed from the store (engine_heads.rs `complete`)" })?;
        read_back += 1;
    }
    Ok(read_back)
}

/// Minimum weighted impl rows and row count before [`tables_of`] splits filing across workers
/// via dynamic work stealing (`file_impl_rows_chunk` + `finish_impl_tables`). Slow `from_text`
/// candidates (`header_unread`, `trait_unread`, or `SelfHead::Other`) carry [`IMPL_SLOW_WEIGHT`]
/// and are placed in tiny [`IMPL_SLOW_CHUNK`] chunks at the front of the queue so dense macro or
/// unparsed headers fan out across all idle cores without creating a static straggler tail.
const IMPL_TABLE_SPLIT_MIN_WEIGHT: usize = 64;
const IMPL_TABLE_SPLIT_MIN_ROWS: usize = 16;
const IMPL_SLOW_WEIGHT: usize = 8;
const IMPL_SLOW_CHUNK: usize = 8;
const IMPL_FAST_CHUNK: usize = 32;

/// Predicted impl-row cost for LPT dispatch of `ImplBucket` jobs (O(1) verified header read).
fn impl_job_weight(job: &Job<'_, '_>, g: Alloc<'static>) -> usize {
    ItemsRecord::from_verified(job.own_crate.records.items_proof, g).impls()
}

/// Whether an impl row is likely to take the slow `from_text` header-lowering path.
fn is_slow_impl_row(row: &ImplRow<'_>) -> bool {
    row.header_unread || row.trait_unread || matches!(row.head, SelfHead::Other)
}

type StolenImplChunks = (
    Vec<blaze_ladder::front2::buckets::ImplChunk<SharedAlloc<'static>>, SharedAlloc<'static>>,
    Vec<CellId, SharedAlloc<'static>>,
    Vec<CellId, SharedAlloc<'static>>,
);

/// Shared work-stealing loop executed by both the parent worker (reusing its already-opened
/// scope) and each helper worker (opening its scope once, lazily after stealing a first chunk).
#[expect(clippy::too_many_arguments, reason = "shared worker loop for parent and helper impl table chunks")]
fn steal_impl_chunks<'n>(
    first_slot: usize,
    opened: &crate::run_names::Opened<'n>,
    crates: crate::run_names::ScopeCrates<'n, 'n>,
    ordered_rows: &[ImplRow<'n>],
    chunk_ranges: &[(usize, usize)],
    cursor: &CachePadded<AtomicUsize>,
    subjects: &'n Subjects,
    at_cells: bool,
    listings: &'n Listings,
    frame: &'n Frame,
    g: Alloc<'static>,
    shared: SharedAlloc<'static>,
) -> Result<StolenImplChunks, TableReadRefused> {
    let rec = opened.records;
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let fa = bump.alloc();
    let recorder = Recorder::new(RecorderOpts { alloc: fa });
    let view = Tables::new(TablesOpts { records: rec, impls: None, lang_crates: &[], reads: &recorder });
    let mut local_chunks: Vec<blaze_ladder::front2::buckets::ImplChunk<SharedAlloc<'static>>, SharedAlloc<'static>> = Vec::new_in(shared);
    let mut slot = first_slot;
    while let Some(&(start, end)) = chunk_ranges.get(slot) {
        if let Some(slice) = ordered_rows.get(start..end) {
            let chunk = file_impl_rows_chunk(view, 0, slice, shared)
                .map_err(|_| TableReadRefused("its impl tables were not written (front2 buckets/records.rs `file_impl_rows_chunk`: a row its writer refuses)"))?;
            if let Some((_, why)) = opened.load.refused() {
                return Err(TableReadRefused(why));
            }
            local_chunks.push(chunk);
        }
        slot = cursor.fetch_add(1, Ordering::Relaxed);
    }
    let recorded = recorder.finish().map_err(|_| TableReadRefused("its impl tables read more names cells than the recorder holds (front2 tables/recorder.rs)"))?;
    let (local_reads, local_heads) = place_table_reads(rec, crates, subjects, at_cells, listings, &recorded, frame, g, shared)?;
    if let Some((_, why)) = opened.load.refused() {
        return Err(TableReadRefused(why));
    }
    Ok((local_chunks, local_reads, local_heads))
}

/// Typed refusal returned by [`place_table_reads`].
struct TableReadRefused(&'static str);

/// Result of one closed-dep `ImplBucket` job overlapped with `HeadsSetup`/`HeadsSplit`.
struct ClosedWorked<'p> {
    made: Result<Result<Worked<'p>, &'static str>, work::Panicked>,
    ns: u64,
}

/// Converts the table reads recorded during impl filing into engine `CellId` reads and head reads.
#[expect(clippy::too_many_arguments, reason = "shared read-placement helper for single and parallel impl table chunks")]
fn place_table_reads<'n, A: core::alloc::Allocator>(
    records: &'n Records<'n>,
    crates: crate::run_names::ScopeCrates<'n, 'n>,
    subjects: &'n Subjects,
    at_cells: bool,
    listings: &'n Listings,
    done_reads: &[blaze_ladder::front2::tables::recorder::TableRead],
    frame: &'n Frame,
    g: Alloc<'static>,
    out_alloc: A,
) -> Result<(blaze_own::Vec<CellId, A>, blaze_own::Vec<CellId, A>), TableReadRefused>
where
    A: Copy,
{
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let fa = bump.alloc();
    let unused = Recorder::new(RecorderOpts { alloc: fa });
    let view = Tables::new(TablesOpts { records, impls: None, lang_crates: &[], reads: &unused });
    let mut reads: blaze_own::Vec<CellId, A> = blaze_own::Vec::with_capacity_in(done_reads.len(), out_alloc);
    let mut heads: blaze_own::Vec<CellId, A> = blaze_own::Vec::new_in(out_alloc);
    let mut owners = at_cells.then(|| Owners::new(subjects, crates, listings, g));
    for read in done_reads.iter() {
        if read.is_source() {
            let cell = crate::engine_head_consts::source_read(records, subjects, *read).map_err(|unplaced| {
                let labels = scope_crates_labels(crates, frame);
                let shown = crate::engine_head_consts::UnplacedIn { unplaced: &unplaced, crates: labels };
                TableReadRefused(crate::engine_head_notes::leak(g, format_args!("placing the source of an impl header: {shown}")))
            })?;
            reads.extend(cell);
            continue;
        }
        match cell_id(view, *read) {
            // A names read: at cell grain its cell (the coordinator keeps it where its crate
            // defines a cell table, `names_read`); else its crate's names record whole, the cell
            // the Names stage defines.
            Ok(Some(id)) if id.of.kind == Kind::CrateScope => reads.push(match at_cells {
                true => id,
                false => CellId { of: id.of, cell: WHOLE },
            }),
            // A header's head read: at cell grain placed as a body's is; a head of no crate the
            // records hold (or `Whole`) is left for the coordinator, which widens it.
            Ok(Some(id)) if id.of.kind == Kind::Signature => match owners.as_mut().and_then(|owners| owners.place(records, &id, g)) {
                Some(HeadAt::Cell { cell, .. }) => heads.push(cell),
                // A body's own item or a head no task holds: its crate's record whole (a header
                // reading the latter is refused at its body, `HeadAt::Unlisted`).
                Some(HeadAt::Dependency(scope) | HeadAt::OfTheBody(scope) | HeadAt::Unlisted { scope, .. }) => heads.push(CellId { of: scope, cell: WHOLE }),
                None => reads.push(id),
            },
            Ok(Some(id)) => reads.push(id),
            Ok(None) => {}
            Err(_) => return Err(TableReadRefused("a read of its filing names a crate its scope does not hold (front2 tables/task.rs `cell_id`)")),
        }
    }
    Ok((reads, heads))
}

/// One crate's tables on a worker: its scope's records opened in the task's frame, its rows filed.
/// Large or header-heavy crates split filing across workers via dynamic LPT work stealing on
/// `cx.scope()`, with the parent worker reusing its already-opened scope and each helper opening
/// its scope at most once (lazily after stealing a chunk). The member subjects place source reads.
/// At cell grain, each header's head read is placed too.
#[expect(clippy::too_many_arguments, reason = "a worker's edge: each argument one handle of it")]
fn tables_of<'p>(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    fs: &bstd::fs::Fs,
    outlined: &[Outlined<'_>],
    packs: &'static crate::body_types::SyncLazyTrees<'static>,
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    job: &Job<'_, '_>,
    scope_records: &dyn crate::front_load::OpenByNeed,
    subjects: &Subjects,
    at_cells: bool,
    heap: &'static blaze_own::Heap,
    listings: &Listings,
    snapshot: &crate::store::StoreSnapshot,
    max_workers: NonZeroU32,
    at_phase: SharedAlloc<'p>,
) -> Result<Worked<'p>, &'static str> {
    let frame = cx.frame();
    let g = heap.alloc();
    let scope_crates = job.scope_crates(scope_records);
    let opened = open_scope_by_need(cx, root, fs, outlined, scope_crates, packs, corrected, procs, crate::run_names::Envs { owner: None, rest: None }, frame, g, snapshot).map_err(OpenScopeRefused::message)?;
    let records = opened.records;
    let mut rows: Vec<ImplRow<'_>, &Frame> = Vec::new_in(frame);
    if !NamesSource::each_impl_row(records, 0, &mut |row| rows.push(row)) {
        if let Some((_, why)) = opened.load.refused() {
            return Err(why);
        }
        if let Some(why) = packs.refused() {
            return Err(why);
        }
        return Err("its crate's records hold no impl rows (front2 source.rs `each_impl_row`)");
    }
    let workers = usize::try_from(cx.workers().min(max_workers.get())).unwrap_or(1);
    let total_weight: usize = rows.iter().map(|row| if is_slow_impl_row(row) { IMPL_SLOW_WEIGHT } else { 1 }).sum();
    let (out, defines, reads, heads, tables) = if workers > 1 && rows.len() >= IMPL_TABLE_SPLIT_MIN_ROWS && total_weight >= IMPL_TABLE_SPLIT_MIN_WEIGHT {
        let crate_key = NamesSource::crate_key(records, 0).ok_or("its crate has no key in its own records (front2 source.rs `crate_key`)")?;
        if impl_tables_name(crate_key) != job.task.planned.name {
            return Err("its tables' crate key is not its task's (front2 buckets/records.rs `impl_tables_name`)");
        }
        let shared = heap.shared();
        // Partition slow `from_text` candidates into small chunks at the front of the queue (LPT),
        // followed by fast rows in their original `(trait_path, module, key)` order so `RowNames`
        // keeps resolving consecutive equal `(trait_path, module)` runs once per chunk.
        let mut ordered_rows: Vec<ImplRow<'_>, &Frame> = Vec::with_capacity_in(rows.len(), frame);
        let mut chunk_ranges: Vec<(usize, usize), &Frame> = Vec::new_in(frame);
        for row in rows.iter() {
            if is_slow_impl_row(row) {
                ordered_rows.push(*row);
            }
        }
        let slow_end = ordered_rows.len();
        let mut at = 0usize;
        while at < slow_end {
            let end = at.saturating_add(IMPL_SLOW_CHUNK).min(slow_end);
            chunk_ranges.push((at, end));
            at = end;
        }
        for row in rows.iter() {
            if !is_slow_impl_row(row) {
                ordered_rows.push(*row);
            }
        }
        let fast_end = ordered_rows.len();
        at = slow_end;
        while at < fast_end {
            let end = at.saturating_add(IMPL_FAST_CHUNK).min(fast_end);
            chunk_ranges.push((at, end));
            at = end;
        }
        let ordered_slice = ordered_rows.as_slice();
        let ranges_slice = chunk_ranges.as_slice();
        let cursor = CachePadded::new(AtomicUsize::new(0));
        let cursor_ref = &cursor;
        let helper_count = workers.saturating_sub(1).min(ranges_slice.len().saturating_sub(1));
        let mut chunks: Vec<blaze_ladder::front2::buckets::ImplChunk<SharedAlloc<'static>>, Alloc<'static>> = Vec::with_capacity_in(ranges_slice.len(), g);
        let mut reads: SVec<'p, CellId> = SVec::new_in(at_phase);
        let mut heads: SVec<'p, CellId> = SVec::new_in(at_phase);
        let mut chunk_err: Option<&'static str> = None;
        {
            let scope = cx.scope();
            scope.narrow_workers(max_workers, Class::Foreground);
            let mut running = Vec::with_capacity_in(helper_count, frame);
            for _ in 0..helper_count {
                running.push(scope.may_overlap("engine impl tables worker", HEADS_FRAME, move |cx: &Cx| {
                    let first = cursor_ref.fetch_add(1, Ordering::Relaxed);
                    if first >= ranges_slice.len() {
                        return Ok::<Result<Option<StolenImplChunks>, &'static str>, Infallible>(Ok(None));
                    }
                    let g = heap.alloc();
                    let chunk_frame = cx.frame();
                    let chunk_crates = job.scope_crates(scope_records);
                    let opened_chunk = match open_scope_by_need(cx, root, fs, outlined, chunk_crates, packs, corrected, procs, crate::run_names::Envs { owner: None, rest: None }, chunk_frame, g, snapshot) {
                        Ok(opened_chunk) => opened_chunk,
                        Err(err) => return Ok(Err(err.message())),
                    };
                    let res = steal_impl_chunks(first, &opened_chunk, chunk_crates, ordered_slice, ranges_slice, cursor_ref, subjects, at_cells, listings, chunk_frame, g, shared)
                        .map(Some)
                        .map_err(|e| e.0);
                    Ok(res)
                }));
            }
            // Parent worker participates in the same stealing queue reusing its already-opened
            // `opened` scope (zero redundant `open_scope_by_need`, zero idle parent core).
            let parent_first = cursor_ref.fetch_add(1, Ordering::Relaxed);
            if parent_first < ranges_slice.len() {
                match steal_impl_chunks(parent_first, &opened, scope_crates, ordered_slice, ranges_slice, cursor_ref, subjects, at_cells, listings, frame, g, shared) {
                    Ok((local_chunks, local_reads, local_heads)) => {
                        chunks.extend(local_chunks.into_iter());
                        reads.extend_from_slice(&local_reads);
                        heads.extend_from_slice(&local_heads);
                    }
                    Err(why) => {
                        chunk_err.get_or_insert(why.0);
                    }
                }
            }
            for handle in running.into_iter() {
                match handle.join(cx) {
                    Ok(Ok(Some((local_chunks, local_reads, local_heads)))) => {
                        chunks.extend(local_chunks.into_iter());
                        reads.extend_from_slice(&local_reads);
                        heads.extend_from_slice(&local_heads);
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(why)) => {
                        chunk_err.get_or_insert(why);
                    }
                    Err(error) => {
                        let _ = error;
                        chunk_err.get_or_insert("its impl tables chunk failed on worker (engine_heads.rs `tables_of`)");
                    }
                }
            }
            if scope.join(cx).is_err() {
                chunk_err.get_or_insert("its impl tables chunk pool failed (engine_heads.rs `tables_of`)");
            }
        }
        if let Some((_, why)) = opened.load.refused() {
            return Err(why);
        }
        if let Some(why) = packs.refused() {
            return Err(why);
        }
        if let Some(why) = chunk_err {
            return Err(why);
        }
        let written = finish_impl_tables(crate_key, &chunks, g)
            .map_err(|_| "its impl tables were not written (front2 buckets/records.rs `finish_impl_tables`: a row its writer refuses)")?;
        let out = impl_tables_out(&written.records);
        let mut defines: SVec<'p, (Cell, ContentId)> = SVec::with_capacity_in(written.defines.len(), at_phase);
        defines.extend(written.defines.iter().map(|(cell, digest)| (Cell(*cell), *digest)));
        reads.sort_unstable();
        reads.dedup();
        heads.sort_unstable();
        heads.dedup();
        (out, defines, reads, heads, written.records)
    } else {
        let done = impl_tables_task(records, 0, &rows, g);
        if let Some((_, why)) = opened.load.refused() {
            return Err(why);
        }
        if let Some(why) = packs.refused() {
            return Err(why);
        }
        if done.failed.is_some() {
            return Err("its impl tables were not written (front2 buckets/records.rs `impl_tables_task`: a row its writer refuses)");
        }
        if done.name != job.task.planned.name {
            return Err("its tables' crate key is not its task's (front2 buckets/records.rs `impl_tables_name`)");
        }
        let tables = done.records.ok_or("its impl tables' task made no records")?;
        let (reads, heads) = place_table_reads(records, scope_crates, subjects, at_cells, listings, &done.reads, frame, g, at_phase).map_err(|e| e.0)?;
        if let Some((_, why)) = opened.load.refused() {
            return Err(why);
        }
        if let Some(why) = packs.refused() {
            return Err(why);
        }
        let mut defines: SVec<'p, (Cell, ContentId)> = SVec::with_capacity_in(done.defines.len(), at_phase);
        defines.extend(done.defines.iter().copied());
        (done.out, defines, reads, heads, tables)
    };
    Ok(Worked { out, defines, reads, heads, payload: framed(&tables, Global) })
}

// ---------------------------------------------------------------------------------------------
// The head tasks: one an outline entry that has a head (module docs, "THE HEAD TASKS").

/// Lazy or already-computed physical signature file of a [`Subject`].
///
/// Storing [`SubjectFile::Path`] lets standing `Held` files construct `Subject`s in O(1) with
/// zero BLAKE3 digests; only the rare file actually queried computes `SignatureFile::new(path)`.
#[derive(Clone, Copy)]
pub(crate) enum SubjectFile {
    Resolved(crate::signature::SignatureFile),
    Path(&'static [u8]),
}

impl SubjectFile {
    pub(crate) fn resolve(self) -> crate::signature::SignatureFile {
        match self {
            SubjectFile::Resolved(sig) => sig,
            SubjectFile::Path(path) => crate::signature::SignatureFile::new(path),
        }
    }

    pub(crate) fn subject(self) -> ContentId {
        self.resolve().subject()
    }

    pub(crate) fn entry(self, outline: &blaze_file_items::Outline<'_>, item: &blaze_surface::Item, text: &str) -> Name {
        self.resolve().entry(outline, item, text)
    }

    pub(crate) fn entry_from_chain(self, chain: ContentId) -> Name {
        self.resolve().entry_from_chain(chain)
    }
}

impl PartialEq for SubjectFile {
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (SubjectFile::Resolved(a), SubjectFile::Resolved(b)) => a == b,
            (SubjectFile::Path(a), SubjectFile::Path(b)) if a == b => true,
            _ => self.resolve() == other.resolve(),
        }
    }
}

impl Eq for SubjectFile {}

impl PartialOrd for SubjectFile {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SubjectFile {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        match (*self, *other) {
            (SubjectFile::Resolved(a), SubjectFile::Resolved(b)) => a.cmp(&b),
            (SubjectFile::Path(a), SubjectFile::Path(b)) if a == b => core::cmp::Ordering::Equal,
            _ => self.resolve().cmp(&other.resolve()),
        }
    }
}

/// A member file as its items record lists it, matched to the planner's outline. The logical
/// module slot names the item record cell, while `signature_file` names the physical file whose
/// outline supplied the signature parts. Both come from one matched outline, and so do
/// `entries`: the facts of its entries a crate's listing reads of the file's text
/// (engine_listing_record.rs `entries_of`), what its stored listing is keyed by.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Subject {
    pub(crate) prefix: [u8; 8],
    pub(crate) file: u32,
    pub(crate) slot: Name,
    pub(crate) signature_file: SubjectFile,
    pub(crate) entries: ContentId,
}

impl Subject {
    pub(crate) fn new(prefix: [u8; 8], file: u32, outline: &Outlined<'_>) -> Subject {
        Subject { prefix, file, slot: outline.name, signature_file: SubjectFile::Resolved(crate::signature::SignatureFile::new(&outline.path)), entries: crate::engine_listing_record::entries_of(outline) }
    }
}

pub(crate) type Subjects = [Subject];

/// The domain of a head cell ([`head_cell`]). 3: the cell holds its unit's crate prefix.
const HEAD_CELL_DOMAIN: &str = "blaze-cli engine head cell 3";
/// The domain of a head's item, of no unit ([`head_item`]).
const HEAD_ITEM_DOMAIN: &str = "blaze-cli engine head item 1";
/// The domain of a head task's source ([`plan_items`]). 2: its payload holds each run's cell
/// ([`WorkedHead::payload`]), so no record of the old form is read by its key. 3: one run a
/// (unit, cell), each frozen under its unit's records ([`HeadsStage::run_items`]).
/// 4: the signature part selects its body's contribution while outlining.
/// 5: a frozen fn record identifies whether its digest covers the signature or the whole item.
/// 7: a fn's body-held-items fact selects the names-cell dependency of its head task.
/// 8: its payload ends in its run facts (the constructors its ADTs give, [`ctor_facts`]), so no
/// record of the runs-only form is read by its key.
const HEAD_SOURCE_DOMAIN: &str = "blaze-cli engine head source 8";
/// The domain of one unit's freeze of a head task: its cells and their digests, in order.
const HEAD_OUT_DOMAIN: &str = "blaze-cli engine head entry out 1";
/// The domain of a head task's out: each unit's freeze's out, in unit order.
const HEAD_UNITS_OUT_DOMAIN: &str = "blaze-cli engine head entry units out 1";

/// The out of a head task whose file no unit holds ([`HeadsStage::run_items`]): its name's alone.
const HEAD_NO_UNIT_DOMAIN: &str = "blaze-cli engine head no unit's 1";

/// The out of the head task `name` when no unit holds its file: a function of its name alone,
/// so the heads table knows such a row without a record (engine_seeds.rs `RequestTable::build`).
pub(crate) fn no_units_out(name: Name) -> ContentId {
    ContentId::of(HEAD_NO_UNIT_DOMAIN, &name.subject.0)
}

/// The cell of a head in the task of the outline entry that covers it: by what the entry's text
/// names it (the want's kind, the item's name, an impl fn's name), never by its checker key. A
/// crate's item keys are of the unit that holds it: `b`'s view of `a` (a dependency's unit) keys
/// `a::one` apart from `a`'s own check scope, where its head task runs, so a cell by key was one
/// no task defines (lane EY, the callers test). `part`: `WHOLE`, the whole head, the only part
/// today, hashed in so a head can be split later (engine.md "ASPECTS"). Two heads of one entry
/// under one cell are refused, both named ([`heads_of`]). The cell is injective over one
/// entry's heads because an entry is ONE item of its text: its own head, or an impl fn's, or
/// an `extern` block's foreign items (one namespace's names, distinct). That rests on each
/// entry having a task name of its own (signature.rs `signature_name`, lane RU3: before it, two
/// unnamed impls of one file were one entry, their two headers one cell).
///
/// ONE CELL A UNIT (lane RU1): a member's item is held by each unit whose records list its file
/// (the lib, the test harness, a bin that `mod`s it), and its keys there hold that unit's crate
/// prefix, so a head frozen under one unit's records names that unit's keys and cannot be read
/// through another's names. The cell holds the unit: its crate prefix, the first 8 bytes of
/// the want's key (front2 tables/head.rs `want_bytes`, bytes 1 to 8; front2 keys.rs
/// `checker_of`), which a head task's freeze under a unit and a reader in that unit both take
/// from their own records' want, so a reader's want places the cell of ITS unit, by
/// construction. Nothing of the machine (rule 25): a crate prefix is a hash of the crate's text.
pub(crate) fn head_cell(want: Want, item: &str, member: Option<&str>, part: Cell) -> Cell {
    head_cell_written(want, item, member, None, part)
}

/// [`head_cell`] of a head a macro (or a body's block) wrote under its entry: the cell holds
/// where it was written ([`Written`]), so the heads of two derives at one struct, of two calls of
/// one macro in an impl block or of two items of one expansion are two cells (rule 21). A file
/// item's (`written: None`) is [`head_cell`]'s, unchanged: the written part is hashed only when
/// there is one, after the fixed-size part, so no file item's cell moved and no domain is bumped.
fn head_cell_written(want: Want, item: &str, member: Option<&str>, written: Option<Written>, part: Cell) -> Cell {
    let bytes = want_bytes(want);
    let mut h = blaze_ids::Hasher::new(HEAD_CELL_DOMAIN);
    h.update(&bytes[1..9]).update(&head_item(want, item, member, written, part).0);
    let digest = h.finish();
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&digest.0[..16]);
    Cell(cell)
}

/// Where under its covering entry a macro (or a body's block) wrote a head: the steps down from
/// the outermost call (body_types/covered.rs `WrittenPath::steps`) and the outermost call's
/// entry relative to the covering entry (0: the entry itself, a derive or an attribute on it, or
/// the entry is the call; else its place among the entry's descendants, a call in an impl's
/// block). Item-relative (rule 71): nothing of the file outside the entry.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Written {
    steps: ContentId,
    from_entry: u32,
}

/// A head's item as an entry's text names it, of no unit: the want's kind, the item's name, an
/// impl fn's name, the part, and where a macro wrote it ([`Written`]). Two units' cells of one
/// item ([`head_cell`]) share it; the profile counts the items held by two units and more by it
/// ([`UnitCounts`]).
fn head_item(want: Want, item: &str, member: Option<&str>, written: Option<Written>, part: Cell) -> Cell {
    let mut h = blaze_ids::Hasher::new(HEAD_ITEM_DOMAIN);
    h.update(&want_bytes(want)[..1]).part(item.as_bytes()).opt_part(member).update(&part.0);
    if let Some(written) = written {
        h.update(&written.steps.0).update(&written.from_entry.to_le_bytes());
    }
    let digest = h.finish();
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&digest.0[..16]);
    Cell(cell)
}

/// Under which units a head task freezes its heads: the rule, or a test's plant (rule 23:
/// the plant removes the cause and the tests watch the effect come back).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HeadUnits {
    /// Once for each member unit whose records hold its file (the rule, [`head_cell`]).
    Each,
    /// THE PLANT: under the first holding unit by crate prefix only (the form before lane RU1).
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only the tests take it (rule 23)"))]
    OnlyFirst,
    /// THE PLANT: under the last holding unit by crate prefix only.
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only the tests take it (rule 23)"))]
    OnlyLast,
    /// THE PLANT of a written head's reads: under each unit as [`HeadUnits::Each`], and no
    /// expansion record's cell read (`heads_of`'s `CrateRow::Call` reads dropped), so a
    /// `macro_rules!` body edited reruns no head task of its calls and the bodies read stale
    /// heads (tests/engine_expansion_heads_tests.rs: the oracle, a whole check, must catch it).
    #[cfg_attr(not(test), expect(dead_code, reason = "the plant: only the tests take it (rule 23)"))]
    NoExpansionReads,
}

/// What the head tasks froze this request, by unit (the profile's "heads frozen" line, rule 53).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct UnitCounts {
    /// Cells defined: one a (unit, head), a head that failed too.
    pub(crate) frozen: u32,
    /// Heads by item, of no unit ([`head_item`]): each head task's items, summed.
    pub(crate) items: u32,
    /// Distinct units that froze a head.
    pub(crate) units: u32,
    /// Items frozen under two units or more (each a freeze a head form relative to "this crate"
    /// would share).
    pub(crate) shared: u32,
    /// Head tasks given to split jobs (one a (task, unit): a task two units hold, twice),
    /// summed over the phases.
    pub(crate) tasks: u32,
    /// Crate jobs (one a unit with head tasks: its setup), summed over the phases.
    pub(crate) crate_jobs: u32,
    /// Split jobs (each a run of one crate's tasks, [`split_count`]), summed over the phases.
    pub(crate) split_jobs: u32,
    /// Split jobs that ran a task, summed over the phases.
    pub(crate) busy: u32,
    /// The floor of `busy`: min(workers, head tasks to run), summed over the phases.
    pub(crate) busy_floor: u32,
    /// The most split jobs one crate's tasks were cut into, in any phase.
    pub(crate) most_splits: u32,
    /// Files in multiple splits, counted once per file within each crate job and phase.
    pub(crate) split_files: u32,
    /// The request's crate listings ([`Listings`]).
    pub(crate) listings: ListingCounts,
}

/// The profile's lines of the head tasks' freezes by unit, their jobs and their listings.
pub(crate) fn unit_profile(counts: &UnitCounts, into: &mut blaze_own::String<&Frame>) {
    use core::fmt::Write as _;
    let _ = writeln!(into, "heads frozen: {} for {} items in {} units ({} items held by 2+ units)", counts.frozen, counts.items, counts.units, counts.shared);
    let _ = writeln!(
        into,
        "head jobs: {} tasks in {} split jobs over {} crate jobs, {} busy (floor min(workers, tasks) = {}: {}), at most {} splits in one crate, {} files in more than one split (floor 0)",
        counts.tasks,
        counts.split_jobs,
        counts.crate_jobs,
        counts.busy,
        counts.busy_floor,
        Hundredths::of(counts.busy, counts.busy_floor),
        counts.most_splits,
        counts.split_files
    );
    let l = &counts.listings;
    let _ = writeln!(
        into,
        "head listings: {} made and {} read by key for {} crates (made a crate: {}; floor 0 after an edit that moves no key), file parts {} made {} read, {} damaged made again, {} unstored, {} not put (records filed an error), {} twice by a race, {} unequal, {} refused full, {} asks",
        l.made,
        l.read,
        l.crates,
        Hundredths::of(l.made, l.crates),
        l.parts_made,
        l.parts_read,
        l.damaged,
        l.unstored,
        l.faulted,
        l.twice,
        l.unequal,
        l.full,
        l.asks
    );
}

/// A ratio printed to two places, `n/a` over nothing (rule 67: the ratio to the floor).
struct Hundredths(Option<u64>);

impl Hundredths {
    fn of(top: u32, floor: u32) -> Hundredths {
        Hundredths((floor > 0).then(|| u64::from(top) * 100 / u64::from(floor)))
    }
}

impl core::fmt::Display for Hundredths {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(at) => write!(f, "ratio {}.{:02}", at / 100, at % 100),
            None => write!(f, "ratio n/a"),
        }
    }
}

/// How a crate's head tasks are cut into pool jobs ([`split_count`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeadSplit {
    /// The rule: a crate's share of the phase's jobs by its share of the tasks.
    Rule,
    /// Request this many jobs using the same whole-file rule as the command.
    #[cfg_attr(not(test), expect(dead_code, reason = "a test's knob"))]
    Jobs(NonZeroU32),
    /// Control for file ownership. Cut at evenly spaced task indexes.
    #[cfg(test)]
    Even(NonZeroU32),
}

/// How the split jobs' results are merged ([`in_split_order`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergePlant {
    /// The rule: each in its split's place, whatever finished first.
    Rule,
    /// As they arrived: the control of the merge's test (rule 23).
    #[cfg_attr(not(test), expect(dead_code, reason = "a test's plant (rule 23)"))]
    Arrival,
}

/// The Heads stage's jobs: how a crate's tasks are split, and the listings' and the merge's
/// plants (the command's: [`HeadJobs::RULE`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeadJobs {
    pub(crate) split: HeadSplit,
    pub(crate) listings: ListingsPlant,
    pub(crate) merge: MergePlant,
}

impl HeadJobs {
    pub(crate) const RULE: HeadJobs = HeadJobs { split: HeadSplit::Rule, listings: ListingsPlant::Rule, merge: MergePlant::Rule };
}

/// The phase's jobs a worker under the rule ([`split_count`]): enough that a crate whose tasks
/// are slow does not hold the phase's end on 128 cores, few enough that each job's own source
/// (its records reopened, its tables and lang index, its recorders) is small beside its tasks.
const JOBS_A_WORKER: usize = 4;

/// Oversized single-file split thresholds:
/// - `LARGE_FILE_MIN_TASKS` / `LARGE_FILE_CHUNK`: unweighted counts for [`file_splits`].
/// - `LARGE_FILE_MIN_WEIGHT` / `LARGE_FILE_CHUNK_WEIGHT`: work-weighted thresholds for
///   [`weighted_file_splits`] (where each task weighs `1 + heads + calls`). A plain function
///   weighs `2`, so `96` keeps small/medium whole files intact while splitting mega-files and
///   derive-heavy/impl-heavy files across 18 and 128 cores.
const LARGE_FILE_MIN_TASKS: usize = 64;
const LARGE_FILE_CHUNK: usize = 32;
const LARGE_FILE_MIN_WEIGHT: usize = 96;
const LARGE_FILE_CHUNK_WEIGHT: usize = 48;

/// The requested number of jobs. A crate gets its share of `workers * JOBS_A_WORKER`
/// proportionally to its share of the phase's tasks. [`file_splits`] chooses the actual cuts.
pub(crate) fn split_count(split: HeadSplit, tasks: usize, total: usize, workers: usize) -> usize {
    split_count_weighted(split, tasks, tasks, total, workers)
}

/// Work-weighted version of [`split_count`]: under [`HeadSplit::Rule`], allocates a crate's
/// share of `workers * JOBS_A_WORKER` proportionally to `crate_weight / total_weight`, clamped
/// to `1..=tasks`.
fn split_count_weighted(split: HeadSplit, tasks: usize, crate_weight: usize, total_weight: usize, workers: usize) -> usize {
    if tasks == 0 {
        return 0;
    }
    match split {
        HeadSplit::Jobs(n) => usize::try_from(n.get()).unwrap_or(usize::MAX).min(tasks),
        #[cfg(test)]
        HeadSplit::Even(n) => usize::try_from(n.get()).unwrap_or(usize::MAX).min(tasks),
        HeadSplit::Rule => {
            let denom = total_weight.max(crate_weight).max(1);
            workers.saturating_mul(JOBS_A_WORKER).saturating_mul(crate_weight).div_ceil(denom).clamp(1, tasks)
        }
    }
}

/// Greedily take the longest prefix of whole files within `limit`. A file larger than
/// `target` stands alone unless it reaches [`LARGE_FILE_MIN_TASKS`] and exceeds `target` (with
/// `split_large` enabled), in which case it is cut into balanced chunks of `target.max(LARGE_FILE_CHUNK)`.
fn greedy_file_splits(files: &[usize], target: usize, limit: usize, split_large: bool) -> impl Iterator<Item = core::ops::Range<usize>> + '_ {
    let mut files = files.iter().copied().peekable();
    let mut end = 0usize;
    let mut pending: Option<(usize, usize, usize)> = None;
    core::iter::from_fn(move || {
        if let Some((cur, file_end, chunk)) = pending {
            let next_end = cur.saturating_add(chunk).min(file_end);
            pending = (next_end < file_end).then_some((next_end, file_end, chunk));
            return Some(cur..next_end);
        }
        let start = end;
        let first = files.next()?;
        end += first;
        if split_large && first >= LARGE_FILE_MIN_TASKS && first > target {
            let chunk = target.max(LARGE_FILE_CHUNK);
            let pieces = first.div_ceil(chunk);
            let balanced = first.div_ceil(pieces);
            let first_end = start.saturating_add(balanced).min(end);
            pending = (first_end < end).then_some((first_end, end, balanced));
            return Some(start..first_end);
        }
        if first <= target {
            while let Some(next) = files.next_if(|&next| next <= target && end - start + next <= limit) {
                end += next;
            }
        }
        Some(start..end)
    })
}

/// Fill toward the target, `ceil(tasks / requested)`, in sorted file order. Whole files stay whole
/// unless a single oversized file reaches [`LARGE_FILE_MIN_TASKS`] and exceeds `target` with
/// `requested > 1`, unlocking 128-core parallelism on mega-files.
pub(crate) fn file_splits(files: &[usize], tasks: usize, requested: usize) -> impl Iterator<Item = core::ops::Range<usize>> + '_ {
    let target = tasks.div_ceil(requested.max(1));
    let split_large = requested > 1;
    let runs = |limit| greedy_file_splits(files, target, limit, split_large);
    let mut limit = target;
    if files.len() > requested && runs(limit).count() > requested {
        let mut upper = target.saturating_add(files.iter().copied().max().unwrap_or(0)).min(tasks);
        let budget = requested.max(runs(upper).count());
        while limit < upper {
            let mid = limit + (upper - limit) / 2;
            if runs(mid).count() <= budget {
                upper = mid;
            } else {
                limit = mid + 1;
            }
        }
    }
    runs(limit)
}

/// Greedily groups whole files by work weight within `limit`. A single oversized file whose weight
/// reaches [`LARGE_FILE_MIN_WEIGHT`] and exceeds `target` (with `split_large` enabled and more than
/// one task) splits along per-task weight boundaries targeting `target.max(LARGE_FILE_CHUNK_WEIGHT)`
/// so mega-files and derive/impl-heavy files fan out across cores without straggler tails.
fn greedy_weighted_splits<'a>(
    files: &'a [(usize, usize)],
    task_weights: &'a [usize],
    target: usize,
    limit: usize,
    split_large: bool,
) -> impl Iterator<Item = (core::ops::Range<usize>, usize)> + 'a {
    let mut files = files.iter().copied().peekable();
    let mut end = 0usize;
    let mut pending: Option<(usize, usize, usize)> = None;
    core::iter::from_fn(move || {
        if let Some((cur, file_end, budget)) = pending {
            let mut next_end = cur.saturating_add(1).min(file_end);
            let mut weight = task_weights.get(cur).copied().unwrap_or(1);
            while next_end < file_end {
                let next_w = task_weights.get(next_end).copied().unwrap_or(1);
                if weight.saturating_add(next_w) <= budget {
                    weight = weight.saturating_add(next_w);
                    next_end = next_end.saturating_add(1);
                } else {
                    break;
                }
            }
            pending = (next_end < file_end).then_some((next_end, file_end, budget));
            return Some((cur..next_end, weight));
        }
        let start = end;
        let (first_len, first_weight) = files.next()?;
        end = end.saturating_add(first_len);
        if split_large && first_len > 1 && first_weight >= LARGE_FILE_MIN_WEIGHT && first_weight > target {
            let chunk = target.max(LARGE_FILE_CHUNK_WEIGHT);
            let pieces = first_weight.div_ceil(chunk).min(first_len).max(1);
            let budget = first_weight.div_ceil(pieces);
            let mut first_end = start.saturating_add(1).min(end);
            let mut weight = task_weights.get(start).copied().unwrap_or(1);
            while first_end < end {
                let next_w = task_weights.get(first_end).copied().unwrap_or(1);
                if weight.saturating_add(next_w) <= budget {
                    weight = weight.saturating_add(next_w);
                    first_end = first_end.saturating_add(1);
                } else {
                    break;
                }
            }
            pending = (first_end < end).then_some((first_end, end, budget));
            return Some((start..first_end, weight));
        }
        let mut total_weight = first_weight;
        if first_weight <= target {
            while let Some(&(next_len, next_weight)) = files.peek() {
                if next_weight <= target && total_weight.saturating_add(next_weight) <= limit {
                    files.next();
                    end = end.saturating_add(next_len);
                    total_weight = total_weight.saturating_add(next_weight);
                } else {
                    break;
                }
            }
        }
        Some((start..end, total_weight))
    })
}

/// Fill toward the weight target `ceil(total_weight / requested)` in sorted file order.
fn weighted_file_splits<'a>(
    files: &'a [(usize, usize)],
    task_weights: &'a [usize],
    total_weight: usize,
    requested: usize,
) -> impl Iterator<Item = (core::ops::Range<usize>, usize)> + 'a {
    let target = total_weight.div_ceil(requested.max(1));
    let split_large = requested > 1;
    let runs = |limit| greedy_weighted_splits(files, task_weights, target, limit, split_large);
    let mut limit = target;
    if files.len() > requested && runs(limit).count() > requested {
        let max_file_weight = files.iter().map(|&(_, weight)| weight).max().unwrap_or(0);
        let mut upper = target.saturating_add(max_file_weight).min(total_weight);
        let budget = requested.max(runs(upper).count());
        while limit < upper {
            let mid = limit + (upper - limit) / 2;
            if runs(mid).count() <= budget {
                upper = mid;
            } else {
                limit = mid + 1;
            }
        }
    }
    runs(limit)
}

/// Why the split jobs' results could not be put in split order ([`in_split_order`]): the
/// merge's own inconsistency, named (rule 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergeError {
    /// A split's result came twice.
    Twice { split: u32 },
    /// A split's result is past the splits made.
    Past { split: u32, splits: u32 },
    /// A split's result never came.
    Missing { split: u32 },
}

impl core::fmt::Display for MergeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MergeError::Twice { split } => write!(f, "head split job {split}'s result came twice (engine_heads.rs `in_split_order`)"),
            MergeError::Past { split, splits } => write!(f, "head split job {split}'s result is past the {splits} split jobs made (engine_heads.rs `in_split_order`)"),
            MergeError::Missing { split } => write!(f, "head split job {split}'s result never came (engine_heads.rs `in_split_order`)"),
        }
    }
}

/// The split jobs' results (`arrived`: (split, result) as each finished) in split order, which
/// is task order: what the merge reads, so no order of completion shows in a row (rule 25).
/// Each split exactly once, or a typed refusal. Under [`MergePlant::Arrival`], as they arrived.
pub(crate) fn in_split_order<'a, T>(arrived: SVec<'a, (u32, T)>, splits: usize, plant: MergePlant, alloc: SharedAlloc<'a>) -> Result<SVec<'a, T>, MergeError> {
    let count = u32::try_from(splits).unwrap_or(u32::MAX);
    let mut slots: SVec<'a, Option<T>> = SVec::with_capacity_in(splits, alloc);
    slots.extend((0..splits).map(|_| None));
    let mut order: SVec<'a, u32> = SVec::with_capacity_in(arrived.len(), alloc);
    for (split, one) in arrived.into_iter() {
        let Some(slot) = slots.get_mut(split as usize) else {
            return Err(MergeError::Past { split, splits: count });
        };
        if slot.is_some() {
            return Err(MergeError::Twice { split });
        }
        *slot = Some(one);
        order.push(split);
    }
    if let Some(split) = slots.iter().position(Option::is_none) {
        return Err(MergeError::Missing { split: u32::try_from(split).unwrap_or(u32::MAX) });
    }
    if plant == MergePlant::Arrival {
        let mut out: SVec<'a, T> = SVec::with_capacity_in(splits, alloc);
        for split in order.iter() {
            if let Some(one) = slots.get_mut(*split as usize).and_then(Option::take) {
                out.push(one);
            }
        }
        return Ok(out);
    }
    let mut out: SVec<'a, T> = SVec::with_capacity_in(splits, alloc);
    out.extend(slots.into_iter().flatten());
    Ok(out)
}

/// A fn head records its signature, whether it has a definition and whether its body holds
/// items. A block that supplies other heads is tracked through the names and expansion cells the
/// entry reads.
pub(crate) fn outline_head_source(outline: &blaze_file_items::Outline<'_>, at: ItemIndex, text: &str) -> ContentId {
    use blaze_surface::{AttrKind, ItemDetail, ItemKind};

    let item = outline.item(at);
    let mut h = blaze_ids::Hasher::new(HEAD_SOURCE_DOMAIN);
    h.update(&crate::signature::signature_source(outline, at, text).0);
    if let ItemDetail::Fn { body_holds_items } = item.detail {
        h.update(&[u8::from(item.body.is_some()), u8::from(body_holds_items)]);
    } else if work::checked_body(outline, item).is_some() || work::expansion_entry(outline, item, text) {
        h.update(&item.hash.0);
    }
    // A trait owns its methods' signatures, including whether each has a default.
    if item.kind() == ItemKind::Trait {
        for (_, member) in outline.children(Some(at)) {
            if let ItemDetail::Fn { body_holds_items } = member.detail {
                h.update(&[u8::from(member.body.is_some()), u8::from(body_holds_items)]);
            }
        }
    }
    // Enclosing `#[cfg]` / `#[cfg_attr]` attributes (ancestors and file `#![cfg]`): adding,
    // removing, or editing an enclosing cfg moves this entry's head key directly without
    // waking unrelated sibling heads.
    let mut parent = item.parent;
    while let Some(idx) = parent {
        let anc = outline.item(idx);
        for attr in outline.attrs(anc) {
            if matches!(attr.kind, AttrKind::Cfg | AttrKind::CfgAttr) {
                h.update(&[1, u8::from(attr.kind == AttrKind::CfgAttr)]);
                crate::engine_plan_cfg::hash_cfg_tokens(&mut h, anc.text(text, attr.range));
            }
        }
        parent = anc.parent;
    }
    for attr in outline.file_attrs() {
        if matches!(attr.kind, AttrKind::Cfg | AttrKind::CfgAttr)
            && let Some(written) = text.get(attr.range.to_usize())
        {
            h.update(&[2, u8::from(attr.kind == AttrKind::CfgAttr)]);
            crate::engine_plan_cfg::hash_cfg_tokens(&mut h, written);
        }
    }
    h.finish()
}

/// One `Signature` task an outline entry that has a head (signature.rs `is_signature_entry`), in
/// `Phase { stage: Heads, wave: 0 }`, named as the outline lists it (signature.rs
/// `signature_name`: the file and the entry's chain), so its name and key are known from text
/// before any phase runs. Its source is the persisted signature part ([`outline_head_source`])
/// and the edition.
pub(crate) fn plan_items<'h>(outlined: &[Outlined<'_>], external: &[Name], alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
    let mut planned = SVec::new_in(alloc);
    for one in outlined.iter() {
        if external.binary_search(&one.name).is_ok() {
            continue;
        }
        let Ok(record) = &one.made else { continue };
        for part in record.parts.iter() {
            let (Kind::Signature, Locate::Source(head)) = (part.name.kind, part.locate) else { continue };
            let source = crate::engine_tasks::edition_source(head, one.edition);
            planned.push(Planned { name: part.name, phase: Phase::new(Stage::Heads, 0), source, file: one.name });
        }
    }
    planned.sort_unstable_by_key(|one| one.name);
    planned.dedup_by_key(|one| one.name);
    planned
}

/// Where a read of a head (a `Signature` cell named by its item, front2 tables/head.rs
/// `cell_id`) is read from in this engine.
#[derive(Clone, Copy)]
pub(crate) enum HeadAt {
    /// A member crate's head: the cell of the head task of the entry that covers it, and that
    /// entry's file (its outline's `FileItems` name).
    Cell { cell: CellId, file: Name },
    /// A dependency's head (its crate defines no cell table): its crate's names record, whole
    /// (nothing of a dependency moves unless its record does).
    Dependency(Name),
    /// A member crate's item declared in a body (its key's `INNER` bit, front2 locals.rs): no
    /// names row and no head task; the body that declares it makes its head in its own store
    /// (front2 tables/seeds.rs `of_the_body`), from its own text, which its key covers. A read
    /// of it from that body is placed as its crate's names record whole (engine_bodies.rs
    /// `widen`), as before this placing.
    OfTheBody(Name),
    /// A member crate's head that no head task holds, and why, named (rule 19): its walk up the
    /// calls that wrote it failed (a text that keeps no origins: a statement call's items), or no
    /// outline entry covers where it stands. A body that needs it is refused by these words
    /// (engine_seeds.rs `StretchHeads::known`), never frozen by the body (rule 3); a read of it
    /// is its crate's names record whole, the reads of the refusal.
    Unlisted { scope: Name, why: &'static str },
}

/// Each head of the member crates a task reads, by its item's head name: each crate's listing
/// taken from the request's table ([`Listings`]) on its first ask here, and every place after a
/// binary search in it. A reader's own table of the crates it asked of, so a place takes no
/// shared lookup; the listing itself is read from its record, or made once per key of its
/// inputs ([`Listings`]).
pub(crate) struct Owners<'s> {
    subjects: &'s Subjects,
    /// The crates the records were opened over, in the records' order (run_names.rs
    /// `names_records`: crate `k` of the records is `crates[k]`): their expansion directories,
    /// which the walk of a written head reads; on a scope opened by need, a crate's records
    /// opened at its first place here (run_names.rs `ScopeCrates`).
    over: crate::run_names::ScopeCrates<'s, 's>,
    /// The request's listings, every crate's once.
    listings: &'s Listings,
    /// The crates (by key prefix) asked of so far, sorted by prefix: each one's listing, or the
    /// words its listing was refused with ([`HeadAt::Unlisted`]).
    filled: Vec<([u8; 8], Result<&'static Listing, &'static str>), Alloc<'static>>,
}

impl<'s> Owners<'s> {
    /// `over`: the crates the readers' records are opened over, in their order; `listings`: the
    /// request's table ([`HeadsStage::listings`]).
    pub(crate) fn new(subjects: &'s Subjects, over: crate::run_names::ScopeCrates<'s, 's>, listings: &'s Listings, g: Alloc<'static>) -> Owners<'s> {
        Owners { subjects, over, listings, filled: Vec::new_in(g) }
    }

    /// The physical path of a placed member's covering file. A stored file's path is
    /// package-relative, so a diagnostic must resolve it in the defining crate.
    pub(crate) fn file_path<'n, 'a>(&self, records: &'n Records<'n>, want: Want, slot: Name, alloc: Alloc<'a>) -> Option<blaze_own::AString<'a>> {
        let krate = records.crate_of(want.key())?;
        let prefix = records.crate_key(krate)?.prefix();
        let from = self.subjects.partition_point(|row| row.prefix < prefix);
        let row = self.subjects[from..].iter().take_while(|row| row.prefix == prefix).find(|row| row.slot == slot)?;
        records.file_paths(krate, row.file)?.package.in_package(self.over.package(krate)?, alloc)
    }

    /// Where the head read `id` is read from; `None`: its crate is none of `records`'. By the
    /// reader's OWN key: `id`'s crate prefix is the reader's unit's view of the item's crate,
    /// so a member's head is the cell its head task froze under THAT unit ([`head_cell`]).
    pub(crate) fn place<'n>(&mut self, records: &'n Records<'n>, id: &CellId, g: Alloc<'static>) -> Option<HeadAt>
    where
        's: 'n,
    {
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&id.of.subject.0[..8]);
        let (k, key) = (0..NamesSource::crate_count(records)).find_map(|k| NamesSource::crate_key(records, k).filter(|key| key.prefix() == prefix).map(|key| (k, key)))?;
        let scope = blaze_drive::key::crate_scope_name(&key);
        // A crate with no file among the members' outlines (a dependency) has no head tasks.
        // Judged by the files, not by the `CELLS` mark of the scope named here: by that mark,
        // `b`'s caller of `a::one` read `a` whole and missed `one`'s new head (lane EY,
        // engine_check_rechecks_the_callers_of_an_edited_signature).
        let from = self.subjects.partition_point(|one| one.prefix < prefix);
        if self.subjects.get(from).is_none_or(|one| one.prefix != prefix) {
            return Some(HeadAt::Dependency(scope));
        }
        // `g`: the listing's lists are the request heap's, `Listings`' own (kept for the call's
        // shape, which the head tasks and the stretches share: engine_heads_once.rs `one_head`).
        let _ = g;
        if let Ok(at) = self.filled.binary_search_by_key(&prefix, |(p, _)| *p)
            && let Some(&(_, listing)) = self.filled.get(at)
        {
            return Some(match listing {
                Ok(listing) => listing.place(id, scope),
                Err(why) => HeadAt::Unlisted { scope, why },
            });
        }
        let Some(one) = self.over.records(k).filter(|one| one.key == key) else {
            if let Err(at) = self.filled.binary_search_by_key(&prefix, |(p, _)| *p) {
                self.filled.insert_shift(at, (prefix, Err(OVER_NOT_RECORDS)));
            }
            return Some(HeadAt::Unlisted { scope, why: OVER_NOT_RECORDS });
        };
        // O(1) direct placement for normal file items (`At::File(file)`): skips building or
        // reading the whole-crate `BLZLSTJ` listing on single-body edits.
        if let Some(placed) = self.place_direct_file(records, k, prefix, id) {
            return Some(placed);
        }
        if let crate::run_names::ScopeCrates::ByNeed(by_need) = self.over
            && let Some(unopened) = k.checked_sub(1).and_then(|i| by_need.rest.get(i))
        {
            self.listings.record_scope_key(unopened.key, unopened.input.at);
        }
        let listing = self.listings.listing(records, &one, k, key, self.subjects);
        // Linear in the crates this reader asked of (a scope's crates, once each).
        if let Err(at) = self.filled.binary_search_by_key(&prefix, |(p, _)| *p) {
            self.filled.insert_shift(at, (prefix, listing));
        }
        Some(match listing {
            Ok(listing) => listing.place(id, scope),
            Err(why) => HeadAt::Unlisted { scope, why },
        })
    }

    /// O(1) direct file-item head placement for non-macro items (`At::File(file)`): computes the
    /// exact `(entry, cell, slot)` from that single file's `FileItemsView` and `ItemsRecord`
    /// without building or reading the whole-crate `BLZLSTJ` listing. Returns `None` for
    /// macro-expanded items (`At::Expansion`), impl methods (`Want::MemberFn`), plants,
    /// uncovered/unlisted heads, or any ambiguity — all of which fall through to [`Listings::listing`].
    fn place_direct_file<'n>(&self, records: &'n Records<'n>, k: usize, prefix: [u8; 8], id: &CellId) -> Option<HeadAt>
    where
        's: 'n,
    {
        if self.listings.plant != ListingsPlant::Rule {
            return None;
        }
        if id.of.kind != Kind::Signature || id.of.subject.0.get(16..32) != Some([0u8; 16].as_slice()) {
            return None;
        }
        let items = records.items_record(k)?;
        let first: [u8; 16] = *id.of.subject.0.first_chunk::<16>()?;
        let blaze_scope::items_record::ItemPrefix::One(idx) = items.find_item_prefix(&first) else {
            return None;
        };
        let row = items.identity_at(idx).ok().flatten()?;
        let At::File(file) = row.at? else {
            return None;
        };
        let want = by_origin::kind_of_lib(records, row.key).and_then(|kind| by_origin::item_want(records, kind)).ok()?;
        if head_cell_id(want).of != id.of {
            return None;
        }
        let from = self.subjects.partition_point(|one| one.prefix < prefix);
        let tail = self.subjects.get(from..)?;
        let len = tail.iter().take_while(|one| one.prefix == prefix).count();
        let mine = tail.get(..len)?;
        let at = mine.binary_search_by_key(&file, |one| one.file).ok()?;
        let subject = mine.get(at)?;
        let _edition = NamesSource::edition_of(records, k)?;
        let current = items.file_current(file).ok()?;
        let view = current.view;
        let top = row.outline;
        let (cover, written) = if let Some(cover) = covering_entry_view(view, top) {
            (cover, None)
        } else if let Some(cover) = holding_entry_view(view, top) {
            let from_entry = top.0.checked_sub(cover.0)?;
            (cover, Some(Written { steps: ContentId::of(IN_A_BODY_DOMAIN, b""), from_entry }))
        } else {
            return None;
        };
        let chain = crate::signature::stored_chain(view, cover);
        let entry = subject.signature_file.entry_from_chain(chain);
        let name = NamesSource::name_of(records, row.key).unwrap_or("");
        let cell = head_cell_written(want, name, None, written, WHOLE);
        Some(HeadAt::Cell { cell: CellId { of: entry, cell }, file: subject.slot })
    }
}

/// Mirrors `signature.rs` `own_head` for direct `FileItemsView` placement.
const fn own_head_kind(kind: blaze_surface::ItemKind) -> bool {
    matches!(
        kind,
        blaze_surface::ItemKind::Function
            | blaze_surface::ItemKind::Const
            | blaze_surface::ItemKind::Static
            | blaze_surface::ItemKind::Struct
            | blaze_surface::ItemKind::Enum
            | blaze_surface::ItemKind::Union
            | blaze_surface::ItemKind::Trait
            | blaze_surface::ItemKind::TraitAlias
            | blaze_surface::ItemKind::Impl
            | blaze_surface::ItemKind::TypeAlias
    )
}

/// Mirrors `signature.rs` `in_modules_only` directly over a serialized `FileItemsView`.
fn in_modules_only_view(view: blaze_surface::FileItemsView<'_>, item: &blaze_surface::Item) -> bool {
    let mut parent = item.parent;
    while let Some(at) = parent {
        let Some(up) = view.item(at) else { return false };
        if up.kind() != blaze_surface::ItemKind::Module {
            return false;
        }
        parent = up.parent;
    }
    true
}

/// Mirrors `signature.rs` `is_signature_entry` directly over a serialized `FileItemsView`.
fn is_signature_entry_view(view: blaze_surface::FileItemsView<'_>, item: &blaze_surface::Item) -> bool {
    if let Some(parent) = item.parent
        && let Some(up) = view.item(parent)
        && item.kind() == blaze_surface::ItemKind::Function
        && up.kind() == blaze_surface::ItemKind::Impl
    {
        return in_modules_only_view(view, &up.item);
    }
    if own_head_kind(item.kind()) || matches!(item.kind(), blaze_surface::ItemKind::MacroCall | blaze_surface::ItemKind::ExternBlock) {
        return in_modules_only_view(view, item);
    }
    false
}

/// Mirrors `signature.rs` `covering_entry` directly over a serialized `FileItemsView`.
fn covering_entry_view(view: blaze_surface::FileItemsView<'_>, at: ItemIndex) -> Option<ItemIndex> {
    let entry = view.item(at)?;
    if is_signature_entry_view(view, &entry.item) {
        return Some(at);
    }
    let parent = entry.parent?;
    let up = view.item(parent)?;
    (matches!(up.kind(), blaze_surface::ItemKind::Trait | blaze_surface::ItemKind::Impl | blaze_surface::ItemKind::ExternBlock)
        && in_modules_only_view(view, &up.item))
        .then_some(parent)
}

/// Mirrors [`holding_entry`] directly over a serialized `FileItemsView`.
fn holding_entry_view(view: blaze_surface::FileItemsView<'_>, at: ItemIndex) -> Option<ItemIndex> {
    let mut parent = view.item(at)?.parent;
    for _ in 0..view.len() {
        let up = parent?;
        if let Some(cover) = covering_entry_view(view, up) {
            return Some(cover);
        }
        parent = view.item(up)?.parent;
    }
    None
}

/// How many crate listings one request's table holds (rule 18): one a (crate, unit view) the
/// request reads heads of. Tempo's workspace has about 700 crates, a few views each; past this
/// a reader's head is refused by [`LISTINGS_FULL`], never listed outside the table (rule 3).
pub(crate) const MOST_LISTINGS: usize = 1 << 16;

/// [`HeadAt::Unlisted`]'s words for a crate whose listing the full table could not take.
const LISTINGS_FULL: &str = "the request's table of crate listings is full, so this crate's heads were not listed (engine_heads.rs `MOST_LISTINGS`)";

/// [`HeadAt::Unlisted`]'s words for a crate whose two listings, made at once by two jobs under
/// one key, were not equal: two values under one key (rule 21), the key short of an input.
const LISTINGS_UNEQUAL: &str = "two listings of this crate's heads made under one key (its crate key and its member files) were not equal, so its key misses an input (engine_heads.rs `Listings::listing`)";

/// [`HeadAt::Unlisted`]'s words for a reader whose records name a crate first that is not its
/// own unit's (run_names.rs `names_records` opens a job's own crate at 0).
const NOT_OWN_FIRST: &str = "its records' first crate is not the unit its head tasks freeze under (engine_heads.rs `crate_setup`)";

/// [`HeadAt::Unlisted`]'s words for a crate whose listing, made now, did not open from its own
/// bytes: the form's encoder and opener disagree (engine_listing_record.rs), the engine's own
/// fault, named.
const LISTING_NOT_OPENED: &str = "its crate's head listing, made now, did not open from its own bytes (engine_listing_record.rs `encode_join`, `open_join`)";

/// The domain of the digest of a crate's member files ([`ListingKey::subjects`]).
const LISTING_SUBJECTS_DOMAIN: &str = "blaze-cli engine heads: a listing's member files 2";

/// One head of a crate's listing ([`Listing::heads`]): a [`HeadRow`] whose words are the
/// request heap's, so it outlives the records it was read from.
#[derive(Clone, Copy)]
pub(crate) struct ListedHead {
    entry: Name,
    cell: Cell,
    sig: Name,
    file: Name,
    want: Want,
    item: Cell,
    body_holds_items: bool,
    label: HeadLabel<'static>,
}

/// A crate's heads, opened from its join record ([`Listings::listing`]; engine_listing_record.rs
/// `open_join`, whether read or made now): what its head tasks freeze, by entry, and where every
/// reader places a read of one, by head name. Every list is the request heap's and sorted, so it
/// is read in place by any worker.
pub(crate) struct Listing {
    /// Each head by (entry, cell, head name), once each; equal keys keep the least by every
    /// field (rule 25: no order of the walk shows).
    heads: &'static [ListedHead],
    /// Indexes into `heads` sorted by (head name, entry, file, cell), one a head name: the
    /// least row of each, as `Owners` placed before.
    by_sig: &'static [u32],
    /// (head name, why no head task holds it), sorted, one a head name.
    unlisted: &'static [(Name, &'static str)],
    /// (entry, a call whose walk ends under it), sorted: the expansion records its task reads.
    calls: &'static [(Name, blaze_id::ItemKey)],
    /// Of every row, in order: two makes under one key are compared by it.
    digest: ContentId,
}

impl Listing {
    /// Where the head read `id` of this crate is read from (`scope`: the crate's names record).
    fn place(&self, id: &CellId, scope: Name) -> HeadAt {
        match self.by_sig.binary_search_by(|at| self.heads[*at as usize].sig.cmp(&id.of)) {
            // The only part is the whole head (`WHOLE`, front2 head.rs `cell_id`).
            Ok(at) => {
                let head = &self.heads[self.by_sig[at] as usize];
                HeadAt::Cell { cell: CellId { of: head.entry, cell: head.cell }, file: head.file }
            }
            // A body's own item: its subject is the checker key's bytes (front2 head.rs
            // `signature_name`), byte 8 its `INNER` mark.
            Err(_) if id.of.subject.0[8] & blaze_id::ItemKey::INNER != 0 => HeadAt::OfTheBody(scope),
            Err(_) => match self.unlisted.binary_search_by_key(&id.of, |(sig, _)| *sig) {
                Ok(at) => HeadAt::Unlisted { scope, why: self.unlisted[at].1 },
                Err(_) => HeadAt::Unlisted { scope, why: NO_HEAD_ROW },
            },
        }
    }

    /// The heads entry `entry`'s task freezes, by cell.
    fn of_entry(&self, entry: Name) -> &'static [ListedHead] {
        let heads = self.heads;
        let from = heads.partition_point(|one| one.entry < entry);
        let len = heads[from..].iter().take_while(|one| one.entry == entry).count();
        &heads[from..from + len]
    }

    /// The calls whose walk ends under entry `entry` (`CrateRow::Call`).
    fn calls_of(&self, entry: Name) -> &'static [(Name, blaze_id::ItemKey)] {
        let calls = self.calls;
        let from = calls.partition_point(|(one, _)| *one < entry);
        let len = calls[from..].iter().take_while(|(one, _)| *one == entry).count();
        &calls[from..from + len]
    }

}

/// A crate's listing from its opened join (`joined`, over bytes on the request heap): its
/// heads labelled, its lists leaked to the request heap with it.
fn listing_of(joined: crate::engine_listing_record::Joined<'static, 'static>, g: Alloc<'static>) -> &'static Listing {
    let crate::engine_listing_record::Joined { rows, by_sig, digest } = joined;
    let mut heads: Vec<ListedHead, Alloc<'static>> = Vec::with_capacity_in(rows.heads.len(), g);
    heads.extend(rows.heads.iter().map(|one| ListedHead {
        entry: one.entry,
        cell: one.cell,
        sig: one.sig,
        file: one.file,
        want: one.want,
        item: one.item,
        body_holds_items: one.body_holds_items,
        label: HeadLabel { want: Some(one.want), name: one.name, member: one.member, entry_name: one.entry_name, by: one.by },
    }));
    let mut one: Vec<Listing, Alloc<'static>> = Vec::with_capacity_in(1, g);
    one.push(Listing { heads: heads.leak_static(), by_sig: by_sig.leak_static(), unlisted: rows.unlisted.leak_static(), calls: rows.calls.leak_static(), digest });
    let one: &'static [Listing] = one.leak_static();
    &one[0]
}

/// What a listing is kept under: the crate's key as the reader's unit views it (its prefix is
/// what every head name of it begins with) and the digest of the crate's member files the walk
/// was given (`Subjects`' rows of that prefix: the Heads stage and the impl tables make theirs
/// apart, so the files are part of the key, never assumed one list; rule 21). A key within ONE
/// request only: no text of the crate is in it ([`Listings`]' note), so no stored record is
/// put or read under it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ListingKey {
    krate: [u8; 32],
    subjects: [u8; 32],
}

/// The file index, logical slot and physical signature file of each row of crate `prefix`.
fn subjects_digest(subjects: &Subjects, prefix: [u8; 8]) -> ContentId {
    let from = subjects.partition_point(|one| one.prefix < prefix);
    let mut h = blaze_ids::Hasher::new(LISTING_SUBJECTS_DOMAIN);
    for one in subjects[from..].iter().take_while(|one| one.prefix == prefix) {
        h.part(&one.file.to_le_bytes());
        name_part(&mut h, one.slot);
        h.part(&one.signature_file.subject().0);
    }
    h.finish()
}

/// Whether a listing is the table's (the rule) or made at each ask (the control of the test
/// that a crate is listed once a request: engine_owners_once_tests.rs, rule 23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListingsPlant {
    Rule,
    /// Reconstruct the covering signature from the logical slot of a directory module.
    #[cfg(test)]
    SlotSignature,
    #[cfg(test)]
    WholeItemFile,
    #[cfg(test)]
    DropItemText(CellId),
    /// Drop a body-holding head task's own names read, so a first block impl can leave it stale.
    #[cfg(test)]
    DropOwnNames,
    /// Every ask makes its own listing and neither the table nor the store is asked or put to;
    /// a head task asks for its crate's listing at each of its tasks.
    #[cfg_attr(not(test), expect(dead_code, reason = "a test's plant (rule 23)"))]
    PerAsk,
}

impl ListingsPlant {
    /// The plant's byte in a stored listing's key (engine_listing_record.rs `CrateFacts::plant`):
    /// each plant its own, so a planted run never reads the rule's records nor the rule a
    /// planted one's (one that names entries otherwise, `SlotSignature`, makes other rows).
    fn key_tag(self) -> u8 {
        match self {
            ListingsPlant::Rule => 0,
            #[cfg(test)]
            ListingsPlant::SlotSignature => 1,
            #[cfg(test)]
            ListingsPlant::WholeItemFile => 2,
            #[cfg(test)]
            ListingsPlant::DropItemText(_) => 3,
            #[cfg(test)]
            ListingsPlant::DropOwnNames => 4,
            ListingsPlant::PerAsk => 5,
        }
    }
}

/// The listings' counts (the profile's "head listings" line; floor: `made + read == crates`, and
/// after an edit that moves no file's facts `made == 0`).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct ListingCounts {
    /// Listings the table holds: one a (crate, unit view, member files).
    pub(crate) crates: u32,
    /// Listings made: the crate walked ([`walk_crate`]) and its parts read or made and joined.
    pub(crate) made: u32,
    /// Listings read from their join record (none walked, no text read).
    pub(crate) read: u32,
    /// Files' parts made from their text ([`file_rows`]), and read from their records.
    pub(crate) parts_made: u32,
    pub(crate) parts_read: u32,
    /// Records found at their key that did not open (or a store read that failed): each made
    /// again, counted, and not put over.
    pub(crate) damaged: u32,
    /// Listings and parts made with no store to read or put (no snapshot was handed, or the
    /// expansion directory gave no key): made, counted, never stored.
    pub(crate) unstored: u32,
    /// Listings and parts made while their records filed an error (front2 source.rs `Log`):
    /// made, counted, never stored (what was read is void, so is what was made of it).
    pub(crate) faulted: u32,
    /// Of every listing in the table, by key: its rows' digest, and of every join held, its
    /// bytes' digest. A digest of the SET this request asked: two requests that asked other
    /// crates (an edit's request asks only the units whose reads it places) differ here with
    /// no listing differing, so a restore is compared with a fresh build by [`Self::each`],
    /// key by key (rule 26), never by these.
    pub(crate) rows: ContentId,
    pub(crate) bytes: ContentId,
    /// Each listing in the table, by key, sorted ([`ListingsShown`]).
    pub(crate) each: ListingsShown,
    /// Seconds of a race: two jobs made one key at once, the two equal, the first kept.
    pub(crate) twice: u32,
    /// Seconds of a race whose listing was not equal (refused: [`LISTINGS_UNEQUAL`]).
    pub(crate) unequal: u32,
    /// Asks: one a reader's first place in a crate, one a crate's head setup.
    pub(crate) asks: u32,
    /// Asks refused because the table was full ([`MOST_LISTINGS`]).
    pub(crate) full: u32,
}

/// THE LISTINGS: each crate's listing of heads ([`Listing`]), request-wide and insert-only, in
/// the tree's concurrent map, its lists lent from the request heap (as [`HeadsOnce`]'s cells).
/// Every Heads job (a crate's setup, each split's `Owners`) and every Bodies stretch
/// (`Owners` in engine_bodies.rs and engine_seeds.rs) asks it, so the walk is made once a crate a
/// request where before it was made by every job and every stretch that placed a read in the
/// crate. Two jobs that make one key at once: the first is kept, the second, equal, is counted;
/// unequal is refused by name (rule 21). No lock is held: the map's own pins.
///
/// ONCE PER KEY OF ITS INPUTS (rules 51 and 77; engine_listing_record.rs, D149 piece I2). A
/// listing is a pure function of the crate's records and its member files' text facts; never
/// of an expansion's bytes ([`place_by_row`]). So it is kept as records: each member file's
/// PART (its rows, made from its text alone, [`file_rows`]) and the crate's JOIN (every part's
/// rows and the walk's own refusals, sorted), each under a key of everything it is a function
/// of (engine_listing_record.rs, THE KEY). An ask reads the join by its key FIRST, before any
/// walk: a one-body edit moves none of its inputs, so the crate's listing is one read. When the
/// join's key moved, the crate is walked ([`walk_crate`]: its items rows and expansion
/// directory, no text), and each file's part is read by its key or made from its text, so an
/// item added to one file makes that file's part alone; the join is made again. What is made
/// is opened from its own bytes, the path a read takes (rule 26), and put at the phase's merge
/// ([`HeadsStage::put_listings`]); every key read or put is named by the answer's root
/// ([`HeadsStage::listing_keys`]). A record found damaged at its key, or a store read that
/// failed, is made again, counted, and never put over; a listing made while its records filed
/// an error is never put (what was read is void).
///
/// [`ListingKey`] is NOT a stored key: it names the crate and its member files' paths only,
/// which is sound within one request (the records are fixed for it) and stale across two, so it
/// keys this request's table alone.
///
/// [`HeadsOnce`]: crate::engine_heads_once::HeadsOnce
pub(crate) struct Listings {
    heap: &'static blaze_own::Heap,
    plant: ListingsPlant,
    /// The committed records read by key; `None`: none was handed (every listing made and
    /// counted `unstored`).
    snapshot: Option<crate::store::SharedSnapshot>,
    /// The tool every stored listing's key holds (rule 52).
    tool: blaze_drive::Tool,
    lists: blaze_core::SharedMap<ListingKey, &'static Listing, SharedAlloc<'static>>,
    /// Each listing record this request read or made, by key, with its bytes on the heap.
    held: blaze_core::SharedMap<Key, Held, SharedAlloc<'static>>,
    /// The keys of `held` already put ([`HeadsStage::put_listings`]).
    put: blaze_core::SharedSet<Key, SharedAlloc<'static>>,
    /// Each crate's `CrateScope` CAS record key (`Input::at`, which cryptographically covers
    /// the whole `CrateScope` payload: `names`, `items`, and `expansions`), avoiding O(bytes)
    /// BLAKE3 re-hashing of `over.items` and `over.names` in `join_key`.
    scope_keys: blaze_core::SharedMap<blaze_id::CrateItemKey, Key, SharedAlloc<'static>>,
    /// Hot atomic counters isolated onto separate 128-byte cache lines ([`CachePadded`], rule 86)
    /// so 128 workers querying crate listings never false-share `asks` with adjacent fields.
    made: CachePadded<AtomicU32>,
    read: CachePadded<AtomicU32>,
    parts_made: CachePadded<AtomicU32>,
    parts_read: CachePadded<AtomicU32>,
    damaged: CachePadded<AtomicU32>,
    unstored: CachePadded<AtomicU32>,
    faulted: CachePadded<AtomicU32>,
    twice: CachePadded<AtomicU32>,
    unequal: CachePadded<AtomicU32>,
    asks: CachePadded<AtomicU32>,
    full: CachePadded<AtomicU32>,
}

/// A listing record this request holds ([`Listings::held`]): read from the store (named by the
/// root, not put) or made now (put at the merge), a crate's join or a file's part.
#[derive(Clone, Copy)]
struct Held {
    bytes: &'static [u8],
    made: bool,
    join: bool,
}

/// What a listing record's key found ([`Listings::stored`]).
enum Found {
    /// Its bytes, from the store or made this request, on the request heap.
    Bytes(&'static [u8]),
    /// No record at the key: made and put.
    Absent,
    /// No store to read: made, not put.
    NoStore,
    /// The store's read failed: made again, not put over.
    Fault,
}

impl Listings {
    /// An empty table over the request's heap, reading listing records from `snapshot`.
    pub(crate) fn new(heap: &'static blaze_own::Heap, plant: ListingsPlant, snapshot: Option<crate::store::SharedSnapshot>, tool: blaze_drive::Tool) -> Listings {
        Listings {
            heap,
            plant,
            snapshot,
            tool,
            lists: blaze_core::SharedMap::new_in("engine.heads: each crate's listing once a request", heap.shared()),
            held: blaze_core::SharedMap::new_in("engine.heads: each listing record read or made, by key", heap.shared()),
            put: blaze_core::SharedSet::new_in("engine.heads: each listing record put", heap.shared()),
            scope_keys: blaze_core::SharedMap::new_in("engine.heads: each crate scope's CAS key", heap.shared()),
            made: CachePadded::new(AtomicU32::new(0)),
            read: CachePadded::new(AtomicU32::new(0)),
            parts_made: CachePadded::new(AtomicU32::new(0)),
            parts_read: CachePadded::new(AtomicU32::new(0)),
            damaged: CachePadded::new(AtomicU32::new(0)),
            unstored: CachePadded::new(AtomicU32::new(0)),
            faulted: CachePadded::new(AtomicU32::new(0)),
            twice: CachePadded::new(AtomicU32::new(0)),
            unequal: CachePadded::new(AtomicU32::new(0)),
            asks: CachePadded::new(AtomicU32::new(0)),
            full: CachePadded::new(AtomicU32::new(0)),
        }
    }

    /// Records crate `key`'s `CrateScope` CAS key (`Input::at`) so [`Self::list_crate`] can
    /// derive `join_key` in O(1) without re-hashing raw `items` and `names` section bytes.
    pub(crate) fn record_scope_key(&self, key: blaze_id::CrateItemKey, at: Key) {
        let _ = self.scope_keys.pin().try_insert(key, at);
    }

    /// The record at `key`: one this request holds, else the store's (transparently decoding a
    /// `BZR1` envelope when compressed), copied/decoded onto the request heap.
    fn stored(&self, key: Key) -> Found {
        if let Some(held) = self.held.pin().get(&key).copied() {
            return Found::Bytes(held.bytes);
        }
        let Some(snapshot) = self.snapshot.as_ref() else { return Found::NoStore };
        match snapshot.get(key) {
            Ok(Some(bytes)) => match crate::store::record_codec::decode_if_envelope_in(
                bytes,
                crate::store::record_codec::DEFAULT_MAX_DECODED_BYTES,
                self.heap.alloc(),
            ) {
                Ok(crate::store::record_codec::Decoded::Borrowed(raw)) => {
                    let mut copy: Vec<u8, Alloc<'static>> = Vec::with_capacity_in(raw.len(), self.heap.alloc());
                    copy.extend_from_slice(raw);
                    Found::Bytes(copy.leak_static())
                }
                Ok(crate::store::record_codec::Decoded::Owned(decompressed)) => Found::Bytes(decompressed.leak_static()),
                Err(_) => Found::Fault,
            },
            Ok(None) => Found::Absent,
            Err(_) => Found::Fault,
        }
    }

    /// `bytes` held under `key` (read, or `made` now and put at the merge). Two makes of one key
    /// that are not equal are two values under one key (rule 21): refused by name.
    fn hold(&self, key: Key, bytes: &'static [u8], made: bool, join: bool) -> Result<(), &'static str> {
        match self.held.pin().try_insert(key, Held { bytes, made, join }) {
            Ok(_) => Ok(()),
            Err(occupied) if occupied.current.bytes == bytes => Ok(()),
            Err(_) => {
                self.unequal.fetch_add(1, Ordering::Relaxed);
                Err(LISTINGS_UNEQUAL)
            }
        }
    }

    /// The errors crate `records` filed so far (front2 source.rs `Log`): a make that files one
    /// is never stored.
    fn filed<'n>(records: &'n Records<'n>) -> usize {
        records.errors().len().saturating_add(usize::try_from(records.log().dropped()).unwrap_or(usize::MAX))
    }

    /// Crate `k` of `records`' listing (`key`: its key there; `over`: its records, whose items,
    /// names and expansion directory the walk reads), from the table, from its join record, or
    /// made now; `Err`: why none (rule 3).
    fn listing<'n>(&self, records: &'n Records<'n>, over: &crate::front_load::CrateRecords<'n>, k: usize, key: blaze_id::CrateItemKey, subjects: &Subjects) -> Result<&'static Listing, &'static str> {
        self.asks.fetch_add(1, Ordering::Relaxed);
        let prefix = key.prefix();
        let at = ListingKey { krate: key.0.0, subjects: subjects_digest(subjects, prefix).0 };
        if self.plant != ListingsPlant::PerAsk
            && let Some(found) = self.lists.pin().get(&at).copied()
        {
            return Ok(found);
        }
        let made = self.list_crate(records, over, k, key, subjects)?;
        if self.plant == ListingsPlant::PerAsk {
            return Ok(made);
        }
        let lists = self.lists.pin();
        if lists.len() >= MOST_LISTINGS {
            self.full.fetch_add(1, Ordering::Relaxed);
            return Err(LISTINGS_FULL);
        }
        match lists.try_insert(at, made) {
            Ok(_) => Ok(made),
            Err(occupied) => {
                let held: &'static Listing = *occupied.current;
                match held.digest == made.digest {
                    true => {
                        self.twice.fetch_add(1, Ordering::Relaxed);
                        Ok(held)
                    }
                    false => {
                        self.unequal.fetch_add(1, Ordering::Relaxed);
                        Err(LISTINGS_UNEQUAL)
                    }
                }
            }
        }
    }

    /// Crate `k`'s listing: its join read by key, or the crate walked, each file's part read by
    /// key or made from its text, and the parts joined (engine_listing_record.rs). Opened from
    /// bytes on either path, so a listing read is a listing made (rule 26).
    fn list_crate<'n>(&self, records: &'n Records<'n>, over: &crate::front_load::CrateRecords<'n>, k: usize, key: blaze_id::CrateItemKey, subjects: &Subjects) -> Result<&'static Listing, &'static str> {
        let g = self.heap.alloc();
        let prefix = key.prefix();
        let edition = NamesSource::edition_of(records, k);
        let krate = listing_record::CrateFacts { tool: self.tool, krate: key.0, edition: edition.map(|one| one.byte()), plant: self.plant.key_tag() };
        let from = subjects.partition_point(|one| one.prefix < prefix);
        let mine = &subjects[from..from + subjects[from..].iter().take_while(|one| one.prefix == prefix).count()];
        let items = records.items_record(k);
        let mut files: Vec<listing_record::FileFacts, Alloc<'static>> = Vec::with_capacity_in(mine.len(), g);
        files.extend(mine.iter().map(|one| file_facts(items, one)));
        // The plant that takes the table away takes the store too (its control counts makes).
        let stores = self.plant != ListingsPlant::PerAsk;
        let join_key = match listing_record::directory_digest(&over.expansions) {
            Ok(directory) if stores => {
                // Prefer the already-hashed `CrateScope` CAS key (`Input::at`, covering every
                // byte of `items`, `names`, and `expansions`) over re-hashing megabytes of raw
                // `over.items` + `over.names` bytes on every command (rule 21 + rule 77).
                let (items_id, names_id) = match self.scope_keys.pin().get(&key).copied() {
                    Some(at) => (at.0, at.0),
                    None => (listing_record::bytes_digest(over.items), listing_record::bytes_digest(over.names)),
                };
                Some(listing_record::join_key(&listing_record::JoinKey {
                    krate,
                    items: items_id,
                    names: names_id,
                    directory,
                    files: &files,
                }))
            }
            Ok(_) => None,
            // No key without the directory's rows: made, and never stored.
            Err(_) => {
                self.unstored.fetch_add(1, Ordering::Relaxed);
                None
            }
        };
        // READ FIRST: a join at its key is the listing; no walk, no text.
        let mut put_join = join_key.is_some();
        if let Some(at) = join_key {
            match self.stored(at) {
                Found::Bytes(bytes) => match listing_record::open_join(bytes, listing_record::Check::Store, g) {
                    Ok(joined) => {
                        self.hold(at, bytes, false, true)?;
                        self.read.fetch_add(1, Ordering::Relaxed);
                        return Ok(listing_of(joined, g));
                    }
                    Err(_) => {
                        self.damaged.fetch_add(1, Ordering::Relaxed);
                        put_join = false;
                    }
                },
                Found::Absent => {}
                Found::NoStore => {
                    self.unstored.fetch_add(1, Ordering::Relaxed);
                    put_join = false;
                }
                Found::Fault => {
                    self.damaged.fetch_add(1, Ordering::Relaxed);
                    put_join = false;
                }
            }
        }
        // MADE: the walk (no text), then each file's part.
        let before = Self::filed(records);
        let walked = walk_crate(records, &over.expansions, k, g);
        let mut all: listing_record::Rows<'static, 'static> = listing_record::Rows::new(g);
        all.unlisted.extend_from_slice(&walked.unlisted);
        for file in walked.files.iter().copied() {
            // A file no member row holds lists nothing (its heads are a dependency's).
            let Ok(at) = mine.binary_search_by_key(&file, |one| one.file) else { continue };
            let subject = &mine[at];
            let heads = &walked.placed[walked.placed.partition_point(|one| one.0 < file)..walked.placed.partition_point(|one| one.0 <= file)];
            let calls = &walked.calls[walked.calls.partition_point(|one| one.0 < file)..walked.calls.partition_point(|one| one.0 <= file)];
            let mut placed: Vec<listing_record::PlacedHead<'n>, Alloc<'static>> = Vec::with_capacity_in(heads.len(), g);
            placed.extend(heads.iter().map(|(_, one)| *one));
            let mut called: Vec<listing_record::PlacedCall, Alloc<'static>> = Vec::with_capacity_in(calls.len(), g);
            called.extend(calls.iter().map(|(_, one)| *one));
            let part_key = join_key.map(|_| listing_record::part_key(&listing_record::PartKey { krate, file: files[at], heads: &placed, calls: &called }));
            let rows = match part_key.map(|at| (at, self.stored(at))) {
                Some((at, Found::Bytes(bytes))) => match listing_record::open_part(bytes, g) {
                    Ok(rows) => {
                        self.hold(at, bytes, false, false)?;
                        self.parts_read.fetch_add(1, Ordering::Relaxed);
                        rows
                    }
                    Err(_) => {
                        self.damaged.fetch_add(1, Ordering::Relaxed);
                        self.make_part(records, k, subject, edition, heads, calls, None)?
                    }
                },
                Some((at, Found::Absent)) => self.make_part(records, k, subject, edition, heads, calls, Some(at))?,
                Some((_, Found::NoStore)) => {
                    self.unstored.fetch_add(1, Ordering::Relaxed);
                    self.make_part(records, k, subject, edition, heads, calls, None)?
                }
                Some((_, Found::Fault)) => {
                    self.damaged.fetch_add(1, Ordering::Relaxed);
                    self.make_part(records, k, subject, edition, heads, calls, None)?
                }
                None => self.make_part(records, k, subject, edition, heads, calls, None)?,
            };
            all.append(&rows);
        }
        let joined = listing_record::join(all, g);
        let mut bytes: Vec<u8, Alloc<'static>> = Vec::new_in(g);
        listing_record::encode_join(&joined, &mut bytes);
        let bytes: &'static [u8] = bytes.leak_static();
        let open_check = if cfg!(test) { listing_record::Check::Full } else { listing_record::Check::Store };
        let joined = listing_record::open_join(bytes, open_check, g).map_err(|_| LISTING_NOT_OPENED)?;
        self.made.fetch_add(1, Ordering::Relaxed);
        if Self::filed(records) > before {
            self.faulted.fetch_add(1, Ordering::Relaxed);
            put_join = false;
        }
        if let Some(at) = join_key.filter(|_| put_join) {
            self.hold(at, bytes, true, true)?;
        }
        Ok(listing_of(joined, g))
    }

    /// File `subject`'s part, made from its text ([`file_rows`]) and opened from its own bytes;
    /// held under `key` to be put, unless its make filed an error in `records`' log.
    #[expect(clippy::too_many_arguments, reason = "a part's inputs: each one of its key's")]
    fn make_part<'n>(
        &self,
        records: &'n Records<'n>,
        k: usize,
        subject: &Subject,
        edition: Option<blaze_surface::Edition>,
        heads: &[(u32, listing_record::PlacedHead<'n>)],
        calls: &[(u32, listing_record::PlacedCall)],
        key: Option<Key>,
    ) -> Result<listing_record::Rows<'static, 'static>, &'static str> {
        let g = self.heap.alloc();
        let before = Self::filed(records);
        let mut rows: listing_record::Rows<'n, 'static> = listing_record::Rows::new(g);
        file_rows(records, k, subject, edition, heads, calls, self.plant, g, &mut |row| match row {
            CrateRow::Head(one) => rows.heads.push(listing_record::Head {
                entry: one.entry,
                cell: one.cell,
                sig: one.sig,
                file: one.file,
                want: one.want,
                item: one.item,
                body_holds_items: one.body_holds_items,
                name: one.name,
                member: one.member,
                entry_name: one.entry_name,
                by: one.by,
            }),
            CrateRow::Unlisted { sig, why } => rows.unlisted.push((sig, why)),
            CrateRow::Call { entry, call } => rows.calls.push((entry, call)),
        });
        let mut bytes: Vec<u8, Alloc<'static>> = Vec::new_in(g);
        listing_record::encode_part(&rows, &mut bytes);
        let bytes: &'static [u8] = bytes.leak_static();
        let opened = listing_record::open_part(bytes, g).map_err(|_| LISTING_NOT_OPENED)?;
        self.parts_made.fetch_add(1, Ordering::Relaxed);
        match key {
            Some(_) if Self::filed(records) > before => {
                self.faulted.fetch_add(1, Ordering::Relaxed);
            }
            Some(at) => self.hold(at, bytes, true, false)?,
            None => {}
        }
        Ok(opened)
    }

    /// Every listing record made this request and not yet put, put to `store` under its key,
    /// sorted (rule 25), and counted in `ledger` under `stage`; how many. On the coordinator,
    /// at a phase's merge (engine_check.rs, after `put_all`).
    fn put_made(&self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, ledger: &mut crate::engine_store::Ledger, stage: Stage) -> Result<u32, CliError> {
        let g = self.heap.alloc();
        let put = self.put.pin();
        let mut puts: Vec<(Key, &'static [u8]), Alloc<'static>> = Vec::new_in(g);
        puts.extend(self.held.pin().iter().filter(|(key, held)| held.made && !put.contains(*key)).map(|(key, held)| (*key, held.bytes)));
        puts.sort_unstable_by_key(|(key, _)| *key);
        for (key, bytes) in puts.iter() {
            let encoded = crate::store::record_codec::encode_selective_in(bytes, g)
                .map_err(|_| CliError::Text { what: "a crate head listing record could not be compressed into BZR1 (engine_heads.rs `Listings::put_made`)" })?;
            let stored = encoded.as_slice();
            ledger.put(stage, *key, &[stored], 0);
            store.put(cx, *key, stored)?;
            put.insert(*key);
        }
        Ok(u32::try_from(puts.len()).unwrap_or(u32::MAX))
    }

    /// The keys of every listing record this request read, or made and put, sorted: what the
    /// answer's root names, so each lives while the answer does and the next command reads it.
    fn keys(&self, g: Alloc<'static>) -> Vec<Key, Alloc<'static>> {
        let put = self.put.pin();
        let mut keys: Vec<Key, Alloc<'static>> = Vec::with_capacity_in(self.held.len(), g);
        keys.extend(self.held.pin().iter().filter(|(key, held)| !held.made || put.contains(*key)).map(|(key, _)| *key));
        keys.sort_unstable();
        keys
    }

    /// The counts as they stand.
    pub(crate) fn counts(&self) -> ListingCounts {
        let g = self.heap.alloc();
        let mut lists: Vec<([u8; 32], [u8; 32], ContentId), Alloc<'static>> = Vec::with_capacity_in(self.lists.len(), g);
        lists.extend(self.lists.pin().iter().map(|(key, listing)| (key.krate, key.subjects, listing.digest)));
        lists.sort_unstable();
        let mut rows = blaze_ids::Hasher::new(LISTINGS_ROWS_DOMAIN);
        let mut each = ListingsShown::default();
        for (krate, subjects, digest) in lists.iter() {
            rows.update(krate).update(subjects).update(&digest.0);
            each.push(ContentId(*krate), ContentId(*subjects), *digest);
        }
        let mut joins: Vec<(Key, ContentId), Alloc<'static>> = Vec::new_in(g);
        joins.extend(self.held.pin().iter().filter(|(_, held)| held.join).map(|(key, held)| (*key, listing_record::bytes_digest(held.bytes))));
        joins.sort_unstable_by_key(|(key, _)| *key);
        let mut bytes = blaze_ids::Hasher::new(LISTINGS_BYTES_DOMAIN);
        for (key, digest) in joins.iter() {
            bytes.update(&key.0.0).update(&digest.0);
        }
        ListingCounts {
            crates: u32::try_from(self.lists.len()).unwrap_or(u32::MAX),
            made: self.made.load(Ordering::Relaxed),
            read: self.read.load(Ordering::Relaxed),
            parts_made: self.parts_made.load(Ordering::Relaxed),
            parts_read: self.parts_read.load(Ordering::Relaxed),
            damaged: self.damaged.load(Ordering::Relaxed),
            unstored: self.unstored.load(Ordering::Relaxed),
            faulted: self.faulted.load(Ordering::Relaxed),
            rows: rows.finish(),
            each,
            bytes: bytes.finish(),
            twice: self.twice.load(Ordering::Relaxed),
            unequal: self.unequal.load(Ordering::Relaxed),
            asks: self.asks.load(Ordering::Relaxed),
            full: self.full.load(Ordering::Relaxed),
        }
    }
}

/// The most listings [`ListingsShown`] holds; the rest are counted in `over`.
pub(crate) const MOST_SHOWN: usize = 16;

/// The first [`MOST_SHOWN`] listings of a request's table, by key (the crate's key as its unit
/// views it, and its member files' digest) with the digest of their rows (checked against the
/// rows at every open, engine_listing_record.rs `open_join`): what a restore and a fresh build
/// are compared by, key by key (rule 26). Inline, so the counts stay `Copy`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct ListingsShown {
    pub(crate) len: u8,
    pub(crate) over: u32,
    /// (crate, member files, rows' digest), sorted, the first `len`.
    pub(crate) rows: [(ContentId, ContentId, ContentId); MOST_SHOWN],
}

impl ListingsShown {
    fn push(&mut self, krate: ContentId, files: ContentId, digest: ContentId) {
        match self.rows.get_mut(usize::from(self.len)) {
            Some(slot) => {
                *slot = (krate, files, digest);
                self.len += 1;
            }
            None => self.over = self.over.saturating_add(1),
        }
    }

    /// The listings held, sorted by key.
    pub(crate) fn shown(&self) -> &[(ContentId, ContentId, ContentId)] {
        &self.rows[..usize::from(self.len)]
    }

    /// The listing under (`krate`, `files`): its rows' digest, if held.
    pub(crate) fn at(&self, krate: ContentId, files: ContentId) -> Option<ContentId> {
        self.shown().iter().find(|(k, f, _)| *k == krate && *f == files).map(|(_, _, digest)| *digest)
    }
}

/// The domains of [`ListingCounts::rows`] and [`ListingCounts::bytes`].
const LISTINGS_ROWS_DOMAIN: &str = "blaze-cli engine heads: the request's listings' rows 1";
const LISTINGS_BYTES_DOMAIN: &str = "blaze-cli engine heads: the request's listings' join bytes 1";

/// What member file `one` gives its crate's keys (engine_listing_record.rs `FileFacts`): its
/// `Subject` row and the surface its items row recorded (`None`: no row, or one that does not
/// read, which the items record's digest in the join's key says apart).
fn file_facts(items: Option<&ItemsRecord<'_>>, one: &Subject) -> listing_record::FileFacts {
    let recorded = items.and_then(|items| items.file_surface(one.file).ok().flatten()).map(|row| row.surface);
    listing_record::FileFacts { file: one.file, slot: one.slot, signature_file: one.signature_file.subject(), recorded, entries: one.entries }
}

/// [`HeadAt::Unlisted`]'s words for a crate the records hold and `Owners` was given no
/// directory of: the engine's own inconsistency, named.
const OVER_NOT_RECORDS: &str = "its crate is among the reader's records but not among the crates `Owners` was given (engine_heads.rs `Owners::new`: the two lists are one list, in one order)";

/// [`HeadAt::Unlisted`]'s words for a member crate's head the crate lists no head of: no item
/// of the crate's names records is the item read, or it has no head (engine_heads.rs
/// `walk_crate`).
const NO_HEAD_ROW: &str = "no item of its member crate's names records has this head (engine_heads.rs `walk_crate`: an item with no head, or a key the records do not hold)";

/// One head cell [`file_rows`] lists.
struct HeadRow<'n> {
    /// Its item's head name in the records read (front2 head.rs `cell_id`; of their unit).
    sig: Name,
    /// The task of the outline entry that covers it.
    entry: Name,
    /// Its file's outline.
    file: Name,
    /// Its cell in `entry`'s task ([`head_cell`]; of the unit of `records`' crate).
    cell: Cell,
    /// Its item, of no unit ([`head_item`]).
    item: Cell,
    want: Want,
    /// Its covering entry's body holds local items or block macros, so its head task reads the
    /// defining unit's names record and moves when the first block item appears.
    body_holds_items: bool,
    /// Its item's name and an impl fn's name, as its entry's text names them (what
    /// [`head_item`] hashed): the words a refusal of two heads under one cell prints.
    name: &'n str,
    member: Option<&'n str>,
    /// Its covering entry's name as its file writes it (item_name.rs `shown_name`).
    entry_name: &'n str,
    /// A head a macro wrote: the innermost call's macro as written (`None`: a body's block's
    /// module wrote it, no call); `None` outside: a head written in its file.
    by: Option<Option<&'n str>>,
}

/// What [`file_rows`] lists of a file of its crate.
enum CrateRow<'n> {
    /// A head, and the entry whose head task freezes it.
    Head(HeadRow<'n>),
    /// A head no head task holds, and why ([`HeadAt::Unlisted`]).
    Unlisted { sig: Name, why: &'static str },
    /// A macro call whose walk up ends under the entry `entry` (its own, or a call or body's block
    /// it stands in): the entry's head task reads its expansion record's cell (`heads_of`), so
    /// an edit of what it expands to (a `macro_rules!` body, a derive's output) reruns exactly
    /// the head tasks whose entries hold its calls.
    Call { entry: Name, call: blaze_id::ItemKey },
}

/// What a head's want is, in words, for a refusal that names it ([`heads_of`]).
fn want_words(want: Want) -> &'static str {
    match want {
        Want::Head(_) => "the alias",
        Want::Adt(_) => "the type",
        Want::Impl(_) => "the impl header",
        Want::Fn(_) => "the fn, const or static",
        Want::Members(_) => "the trait",
        Want::MemberFn(..) => "the impl fn",
    }
}

/// A head as its entry's text names it, for a refusal ([`heads_of`]): its want's kind in words,
/// its item's name and an impl fn's name. `want: None`: no row of the head (never, as
/// `heads_of` asks only of its own rows; said, not hidden).
#[derive(Clone, Copy)]
struct HeadLabel<'n> {
    want: Option<Want>,
    name: &'n str,
    member: Option<&'n str>,
    /// Its covering entry's name and, for a head a macro wrote, the macro ([`HeadRow::by`]).
    entry_name: &'n str,
    by: Option<Option<&'n str>>,
}

impl core::fmt::Display for HeadLabel<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let words = self.want.map_or("a head with no row", want_words);
        // "`Point` (written by `Serialize`): the impl header": the entry the user wrote first,
        // so a head no name of its own says (an impl) is still found in the text.
        match self.by {
            Some(Some(by)) => write!(f, "`{}` (written by `{by}`): ", self.entry_name)?,
            Some(None) => write!(f, "`{}` (written in a body's block): ", self.entry_name)?,
            None => {}
        }
        match (self.member, self.name) {
            (Some(member), "") => write!(f, "{words} `{member}`"),
            (Some(member), name) => write!(f, "{words} `{name}::{member}`"),
            (None, "") => write!(f, "{words}"),
            (None, name) => write!(f, "{words} `{name}`"),
        }
    }
}

/// [`CrateRow::Unlisted`]'s words for a head whose call (or body) stands where no outline entry
/// of its file covers it, nor holds it in its body ([`holding_entry`]).
const NOT_COVERED: &str = "the call that wrote it (or the body whose block it is in) stands where no outline entry of its file has a head task, and no entry's body holds it (signature.rs `covering_entry`, engine_heads.rs `holding_entry`)";

/// The domain of the steps of a head declared in a body's text as the file holds it (no call
/// and no block step wrote it: [`Written::steps`] is this tag, `from_entry` its place).
const IN_A_BODY_DOMAIN: &str = "blaze-cli head declared in a body 1";

/// The entry whose body holds the outline entry `at`: its nearest container an outline entry
/// covers (signature.rs `covering_entry`), whose head task freezes the heads declared in its
/// body. `None`: none (a file's outline holds no such container). Bounded by the outline's
/// entries: each step goes to a parent.
fn holding_entry(outline: &blaze_file_items::Outline<'_>, at: ItemIndex) -> Option<ItemIndex> {
    let mut parent = outline.items().get(at.0 as usize)?.parent;
    for _ in 0..outline.items().len() {
        let up = parent?;
        if let Some(cover) = crate::signature::covering_entry(outline, up) {
            return Some(cover);
        }
        parent = outline.item(up).parent;
    }
    None
}

/// Whether an outline item owns local items or block macros whose names can change without
/// moving its signature source.
fn item_body_holds_items(item: &blaze_surface::Item) -> bool {
    matches!(item.detail, blaze_surface::ItemDetail::Fn { body_holds_items: true } | blaze_surface::ItemDetail::Const { body_holds_items: true } | blaze_surface::ItemDetail::Static { body_holds_items: true })
}

/// Whether the item at `from` is in a body that the covering entry `cover` owns. The same
/// ancestor walk assigns a block head to its owner in the fresh listing and marks that owner's
/// unit names read in the edit path.
fn body_holds_items(outline: &blaze_file_items::Outline<'_>, from: ItemIndex, cover: ItemIndex) -> bool {
    let mut at = Some(from);
    for _ in 0..=outline.items().len() {
        let Some(index) = at else { return false };
        if item_body_holds_items(outline.item(index)) {
            return true;
        }
        if index == cover {
            return false;
        }
        at = outline.item(index).parent;
    }
    false
}

/// Where item `key` of crate `k` is written, from its items row alone: the text it is in (a file
/// or a call's expansion) and its outline entry there. The walk ([`walk_crate`]) needs no
/// more: a file head is placed in today's outline of its file, which the walk reads and checks
/// against the row's surface itself (`NamesSource::file_text`, blaze-scope items_record.rs
/// `current_file`), and a written head is walked up the crate's expansion directory
/// (body_types/covered.rs `written_path`), never up the expansion's bytes. So no expansion
/// record is read here (rule 77): `NamesSource::place_of` over records proves the text first
/// (front2 source/item_grain.rs `row_now` reads an expansion's WHOLE text through blaze-scope
/// items_record.rs `ItemsRecord::whole`), and it gives the same place (`row.text.at` is the
/// row's `at`, `row.outline` its `outline`: items_record.rs `item_at`). On tempo that proof was
/// a read of every macro-written item's expansion record on a one-body edit.
///
/// `None`: no row, or a row with no place; front2 source.rs `Records::text_at` filed why in the
/// records' log, which voids what is checked over them (never a quiet skip). The second read is
/// the very row `text_at` just read (the same crate's record: `key` is one of crate `k`'s rows,
/// `NamesSource::each_item`), so it cannot fail where the first did not.
fn place_by_row<'n>(records: &'n Records<'n>, k: usize, key: blaze_id::ItemKey) -> Option<blaze_scope::resolve::Place> {
    let at = records.text_at(key)?;
    let row = records.items_record(k)?.identity(key).ok()??;
    Some(blaze_scope::resolve::Place { krate: k, at, item: row.outline })
}

/// What [`walk_crate`] places of a crate, with no text read: the heads whose walk failed (with
/// why), and by file (sorted by file, then top entry) each head placed in it and each call whose
/// walk tops in it. Each file's part is a function of its own rows here and its text's facts
/// (engine_listing_record.rs `part_key`).
struct Walked<'n> {
    unlisted: Vec<(Name, &'static str), Alloc<'static>>,
    placed: Vec<(u32, listing_record::PlacedHead<'n>), Alloc<'static>>,
    calls: Vec<(u32, listing_record::PlacedCall), Alloc<'static>>,
    /// Every file `placed` or `calls` names, sorted, once each.
    files: Vec<u32, Alloc<'static>>,
}

/// THE WALK of crate `k` of `records` (rule 30: the one listing of a crate's heads; the head
/// tasks freeze these and the readers place their reads by these, engine_seeds.rs and
/// engine_bodies.rs `widen`), no text read: every item placed by its row alone
/// ([`place_by_row`]: no expansion's text is read to place it), each member by its member row;
/// a written head walked up its calls and blocks to the file entry its outermost call stands at
/// (body_types/covered.rs `written_path`, `texts`: the crate's directory, no expansion record
/// read); every call of the crate walked to the file entry it tops at. A head whose walk fails
/// is listed unlisted with why, never left out (rule 3). What each file's entries make of these
/// is [`file_rows`].
fn walk_crate<'n>(records: &'n Records<'n>, texts: &ExpansionTexts<'n>, k: usize, g: Alloc<'static>) -> Walked<'n> {
    let mut unlisted: Vec<(Name, &'static str), Alloc<'static>> = Vec::new_in(g);
    let mut keys: Vec<blaze_id::ItemKey, Alloc<'static>> = Vec::new_in(g);
    NamesSource::each_item(records, k, &mut |key, _| keys.push(key));
    let krate = NamesSource::crate_key(records, k);
    // (its text, its entry there, want, the item's name, an impl fn's name).
    let mut wanted: Vec<(At, u32, Want, &'n str, Option<&'n str>), Alloc<'static>> = Vec::new_in(g);
    // Each item's and member's block text key, with where its body is written
    // (body_types/covered.rs `BlockHomes`): a key no block has is never asked.
    let mut blocks: Vec<(blaze_id::ItemKey, At, ItemIndex), Alloc<'static>> = Vec::new_in(g);
    for key in keys.iter().copied() {
        let Ok(want) = by_origin::kind_of_lib(records, key).and_then(|kind| by_origin::item_want(records, kind)) else { continue };
        let Some(place) = place_by_row(records, k, key) else { continue };
        let item = NamesSource::name_of(records, key).unwrap_or("");
        wanted.push((place.at, place.item.0, want, item, None));
        if let Some(krate) = krate {
            blocks.push((crate::body_types::covered::block_key(krate, key), place.at, place.item));
        }
        if let Want::Impl(lib) | Want::Members(lib) = want {
            // Each member by its row alone (front2 source.rs `Records::each_member_row`): its
            // outline entry is the row's, as `member_now` gave it, and no member text (a
            // derive's expansion) is read to place it (rule 77).
            records.each_member_row(lib, &mut |m| {
                // A member a call in the container's block wrote is in that call's text.
                let text = m.written_by.map_or(place.at, At::Expansion);
                if let Some(krate) = krate {
                    blocks.push((crate::body_types::covered::block_key(krate, m.key), text, m.outline));
                }
                if let (Want::Impl(_), blaze_surface::ItemKind::Function, Some(name)) = (want, m.kind, m.name) {
                    wanted.push((text, m.outline.0, Want::MemberFn(lib, member_key_of(checker_of(lib), MemberKind::Fn, name)), item, Some(name)));
                }
            });
        }
    }
    blocks.sort_unstable_by_key(|(block, _, _)| *block);
    blocks.dedup_by_key(|(block, _, _)| *block);
    // (file, the head placed: the file entry it or its outermost call is at, want, name, impl
    // fn's name, the walk).
    let mut placed: Vec<(u32, listing_record::PlacedHead<'n>), Alloc<'static>> = Vec::with_capacity_in(wanted.len(), g);
    for &(at, item, want, name, member) in wanted.iter() {
        match at {
            At::File(file) => placed.push((file, listing_record::PlacedHead { top: item, want, name, member, walk: None })),
            At::Expansion(from) => match crate::body_types::covered::written_path(texts, &blocks, from, Some(ItemIndex(item)), &mut |_| {}) {
                Ok(path) => placed.push((path.top.0, listing_record::PlacedHead { top: path.top.1.0, want, name, member, walk: Some((path.steps, path.by)) })),
                Err(why) => unlisted.push((head_cell_id(want).of, why)),
            },
        }
    }
    // Every field in the order, so no order of the rows shows in a part's key (rule 25).
    placed.sort_unstable_by_key(|(file, one)| (*file, one.top, want_bytes(one.want), one.walk.map(|(steps, _)| steps), one.name, one.member, one.walk.map(|(_, by)| by)));
    // (file, the call: the file entry its outermost call is at, its key, its directory row).
    let mut calls: Vec<(u32, listing_record::PlacedCall), Alloc<'static>> = Vec::with_capacity_in(texts.len(), g);
    for at in 0..texts.len() {
        let Ok(call) = texts.call_at(at) else { continue };
        // A call whose walk fails (a call inside a body, whose text keeps no origins) is under
        // no head task: the heads it wrote are each listed unlisted above.
        if let Ok(path) = crate::body_types::covered::written_path(texts, &blocks, call, None, &mut |_| {}) {
            // A row that does not read gave the join no key (engine_listing_record.rs
            // `directory_digest`), so no part keyed by this is stored: the marker is the call's.
            let row = match texts.entry(call) {
                Ok(Some(entry)) => listing_record::call_row(&entry),
                _ => ContentId::of(NO_DIRECTORY_ROW_DOMAIN, &call.0.0),
            };
            calls.push((path.top.0, listing_record::PlacedCall { top: path.top.1.0, call, row }));
        }
    }
    calls.sort_unstable_by_key(|(file, one)| (*file, one.top, one.call));
    let mut files: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(placed.len() + calls.len(), g);
    files.extend(placed.iter().map(|one| one.0));
    files.extend(calls.iter().map(|one| one.0));
    files.sort_unstable();
    files.dedup();
    Walked { unlisted, placed, calls, files }
}

/// The marker of a call whose directory row does not read ([`walk_crate`]).
const NO_DIRECTORY_ROW_DOMAIN: &str = "blaze-cli engine heads: a call with no directory row 1";

/// ONE FILE'S PART of its crate's listing ([`walk_crate`]'s `heads` and `calls` placed in member
/// file `subject`), made from the file's text and outline alone: each head with the entry whose
/// head task freezes it and its cell there ([`Written`] for a head a call or a body's block
/// wrote), each call under the entry it tops at. A head no entry covers nor holds is listed as
/// [`CrateRow::Unlisted`] with why, never left out (rule 3). What it reads of the text is its
/// entries' facts (engine_listing_record.rs `entries_of`): covering and holding entries,
/// signature chains, bodies that hold items, an entry's name as written. No text (the records
/// filed why in their log): no rows.
#[expect(clippy::too_many_arguments, reason = "a part's inputs: each one of its key's")]
fn file_rows<'n>(
    records: &'n Records<'n>,
    k: usize,
    subject: &Subject,
    edition: Option<blaze_surface::Edition>,
    heads: &[(u32, listing_record::PlacedHead<'n>)],
    at_calls: &[(u32, listing_record::PlacedCall)],
    plant: ListingsPlant,
    g: Alloc<'static>,
    each: &mut dyn FnMut(CrateRow<'n>),
) {
    #[cfg(not(test))]
    let _ = plant;
    let file = subject.file;
    let (Some(text), Some(edition)) = (NamesSource::file_text(records, k, file), edition) else { return };
    let outline = blaze_file_items::outline(text, edition, g);
    let entry_of = |top: u32| {
        let cover = crate::signature::covering_entry(&outline, ItemIndex(top))?;
        let item = outline.item(cover);
        #[cfg(test)]
        if plant == ListingsPlant::SlotSignature {
            return Some((cover, crate::signature::signature_name(subject.slot.subject, &outline, item, text)));
        }
        Some((cover, subject.signature_file.entry(&outline, item, text)))
    };
    for (_, one) in at_calls.iter() {
        if let Some((_, entry)) = entry_of(one.top) {
            each(CrateRow::Call { entry, call: one.call });
        }
    }
    {
        for &(_, listing_record::PlacedHead { top, want, name, member, walk }) in heads.iter() {
            // An entry no outline entry covers stands inside a body (an impl in a fn's body, a
            // body's block a call or `const _` wrote): a body's items have no `Signature` of their
            // own (D114 FACTS (iii)), so the head task of the entry whose body holds it freezes it
            // (`holding_entry`), under a cell of its place in that entry. The freeze records
            // its reads of the block's expansion, items and members.
            let placed = match entry_of(top) {
                Some((cover, entry)) => Some((cover, entry, walk.map(|(steps, _)| Written { steps, from_entry: top - cover.0 }))),
                None => holding_entry(&outline, ItemIndex(top)).and_then(|holder| entry_of(holder.0)).map(|(cover, entry)| {
                    let steps = walk.map_or_else(|| ContentId::of(IN_A_BODY_DOMAIN, b""), |(steps, _)| steps);
                    (cover, entry, Some(Written { steps, from_entry: top - cover.0 }))
                }),
            };
            let Some((cover, entry, written)) = placed else {
                let why = if walk.is_some() { NOT_COVERED } else { crate::signature::uncovered_why(&outline, ItemIndex(top)) };
                each(CrateRow::Unlisted { sig: head_cell_id(want).of, why });
                continue;
            };
            each(CrateRow::Head(HeadRow {
                sig: head_cell_id(want).of,
                entry,
                file: subject.slot,
                cell: head_cell_written(want, name, member, written, WHOLE),
                item: head_item(want, name, member, written, WHOLE),
                want,
                body_holds_items: body_holds_items(&outline, ItemIndex(top), cover),
                name,
                member,
                entry_name: crate::item_name::shown_name(outline.item(cover), text),
                by: walk.map(|(_, by)| by),
            }));
        }
    }
}

/// A names read as its crate's task defines it (engine_bodies.rs `widen`'s rule): a cell of a
/// crate with a cell table stays its cell (its lang rows one of them); any other its record whole.
pub(crate) fn names_read(view: View<'_, '_>, id: CellId) -> CellId {
    match id.of.kind {
        Kind::CrateScope if id.cell != WHOLE && view.cell(&CellId { of: id.of, cell: CELLS }) != blaze_drive::key::ABSENT => id,
        Kind::CrateScope => CellId { of: id.of, cell: WHOLE },
        _ => id,
    }
}

/// Every member crate's files as its items record lists them, matched to the planner's outlines
/// ([`Subjects`], sorted): what [`Owners::place`] places an `ImplBucket` task's header read by,
/// at cell grain ([`HeadsStage::run_phase`]). The same match `run_items` and engine_bodies.rs
/// `groups` make in their member loops (to be this one home once lane RU1's per-unit heads land
/// there). A member whose records do not open lists no file: its heads are then placed as a
/// dependency's (its record whole), never missed.
#[expect(clippy::too_many_arguments, reason = "phase-level member subjects lookup over active jobs")]
fn member_subjects<'r, 'p, 'h>(
    cache: &mut HeadsScopeCache<'r>,
    read: &mut EngineRecords<'r>,
    cx: &Cx,
    store: &mut dyn crate::store::Store<'r>,
    names: crate::engine_names::NamesInputs<'_, '_>,
    list: &blaze_world::crate_list::CrateList<'static>,
    outlined: &'h [Outlined<'_>],
    jobs: &[Job<'r, 'p>],
    by_path_slot: &mut Option<blaze_hash::Map<&'h [u8], usize, blaze_hash::FixedState, SharedAlloc<'p>>>,
    subject_entries: &mut SubjectEntries<'p>,
    subjects: &mut SVec<'p, Subject>,
    loaded_prefixes: &mut SVec<'p, [u8; 8]>,
    max_workers: NonZeroU32,
    at_phase: SharedAlloc<'p>,
    frame: &Frame,
    heap: &'static blaze_own::Heap,
) -> Result<(), CliError> {
    let g = heap.alloc();
    let mut members: Vec<usize, &Frame> = Vec::new_in(frame);
    for job in jobs.iter() {
        if list.member(job.own_crate.world_at) {
            members.push(job.own_crate.world_at);
        }
        for one in job.rest.iter() {
            if list.member(one.world_at) {
                members.push(one.world_at);
            }
        }
    }
    members.sort_unstable();
    members.dedup();
    members.retain(|&member| loaded_prefixes.binary_search(&list.key(member).prefix()).is_err());
    if members.is_empty() {
        return Ok(());
    }
    let by_path = &*by_path_slot.get_or_insert_with(|| outlined_by_path(outlined, at_phase));
    let mut pending: Vec<([u8; 8], u32, usize), &Frame> = Vec::new_in(frame);
    let erased = blaze_own::Erased::new(frame);
    let fa = erased.handle();
    for member in members.iter().copied() {
        // A member whose own records cannot be read lists no file here (its heads placed as a
        // dependency's, coarser, never missed); its typed cause is printed by the refusal of
        // that member's own head tasks and bodies.
        let Ok((_, own_records)) = cache.records_of(read, cx, store, names, list, member, g) else { continue };
        let items = ItemsRecord::from_verified(own_records.items_proof, fa);
        let prefix = list.key(member).prefix();
        loaded_prefixes.push(prefix);
        for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
            let Ok(Some(row)) = items.file_surface(file) else { continue };
            let Some(path) = row.path.in_package(list.shown_from(member), fa) else { continue };
            let Some(path) = path.strip_prefix("/0/") else { continue };
            if let Some(idx) = find_outlined_index(by_path, path.as_bytes()) {
                pending.push((prefix, file, idx));
            }
        }
    }
    loaded_prefixes.sort_unstable();
    loaded_prefixes.dedup();
    if !pending.is_empty() {
        subject_entries.populate(cx, outlined, &pending, subjects, max_workers, at_phase, frame)?;
        subjects.sort_unstable();
        subjects.dedup();
    }
    Ok(())
}

/// One member crate's head tasks, made over its check scope's records (its own opened; every
/// non-member dependency opened by need through [`HeadsScopeRecords`]): its setup on one worker,
/// its tasks in split jobs on any.
struct ItemJob<'r, 'p> {
    prefix: [u8; 8],
    own: Crate<'r>,
    rest: SVec<'p, crate::front_load::Unopened>,
    lang: SVec<'p, usize>,
    /// Every crate of its scope had records (else its tasks are refused by name).
    whole: bool,
    /// When not `whole`: the cut's cause in words, the crate named (engine_bodies.rs
    /// `Scope::cut_cause`), what its tasks' refusal prints.
    cut_words: Option<&'static str>,
    /// The cells a refusal of its tasks is a function of ([`scope_cells`]): at 0 the graph's,
    /// at 1 the member's own names cell.
    cells: SVec<'p, CellId>,
    /// Its tasks, sorted by (file, name) before splitting.
    tasks: SVec<'p, Task>,
}

impl<'r> ItemJob<'r, '_> {
    fn scope_crates<'s>(&'s self, records: &'s dyn crate::front_load::OpenByNeed) -> crate::run_names::ScopeCrates<'s, 's> {
        crate::run_names::ScopeCrates::ByNeed(crate::run_names::ByNeedScope { own: self.own, rest: &self.rest[..], records })
    }
}

/// One head task as a worker made it.
struct WorkedHead<'p> {
    out: ContentId,
    defines: SVec<'p, (Cell, ContentId)>,
    /// Each cell's item ([`head_item`]) and its unit's crate prefix, as `defines` lists them:
    /// what the profile counts by. Over the phase's `Sync` handle: it crosses back from the
    /// worker and is unmapped when the phase ends.
    items: SVec<'p, (Cell, [u8; 8])>,
    reads: SVec<'p, CellId>,
    /// Each cell's want (front2 head.rs `want_bytes`), its cell ([`head_cell`], 16 bytes), its
    /// frozen bytes' length (u32 LE) and the frozen bytes, in cell order. The row's record adds
    /// the run facts after them ([`ctor_facts`]; engine_seeds.rs `runs_from`).
    payload: std::vec::Vec<u8, Global>,
    /// Each head it could not freeze: its cell, the head, why (engine_head_notes.rs).
    failed: SVec<'p, (Cell, &'static str, crate::engine_head_notes::Cause)>,
    /// The checker keys of the constructors of each ADT the task's entry holds, under this
    /// unit ([`ctors_of`]), 16 big-endian bytes each, any order: its run facts' covers. Over the
    /// phase's `Sync` handle, as `items`.
    ctors: SVec<'p, [u8; 16]>,
    /// Whether an ADT's head (`Want::Adt`) of the task did not freeze: its constructors are not
    /// run ([`ctor_facts`]).
    adt_failed: bool,
}

/// The domain of a head's cell digest when it could not be frozen: of its reason, so a reader
/// moves when the reason does, and when the head is frozen at last.
const HEAD_FAILED_DOMAIN: &str = "blaze-cli engine heads: a head not frozen";

/// A head task that could not freeze a head: why (rule 19), and what it read until then.
struct HeadFailed<'p> {
    why: &'static str,
    reads: SVec<'p, CellId>,
    incomplete_reads: bool,
}

/// The checker keys of ADT `lib`'s constructors in `names` (its unit's records; `lib` holds the
/// unit's crate prefix, so each unit's keys are its own), onto `into`: an enum's variants, each
/// by its own key (front2 heads.rs `adt`: `checker_of` of the variant's library key), else a
/// struct's or union's one constructor (`member_key_of(.., MemberKind::Ctor, "")`, as heads.rs
/// files it). A braced or unit shape is listed too: no program calls it as a fn, and a key no
/// program asks names nothing.
fn ctors_of<'n>(names: &'n Records<'n>, lib: blaze_id::ItemKey, into: &mut SVec<'_, [u8; 16]>) {
    match NamesSource::variants(names, lib) {
        Some(variants) => into.extend(variants.iter().map(|variant| checker_of(variant.key).0.to_be_bytes())),
        None => into.push(member_key_of(checker_of(lib), MemberKind::Ctor, "").0.to_be_bytes()),
    }
}

/// A head task's RUN FACTS, appended to its record's `payload` after its runs, then their
/// length (u32 LE): the payload's trailer (engine_seeds.rs `runs_from` splits it; the form is
/// `HEAD_SOURCE_DOMAIN` 8's). They are engine_body_facts.rs's form, read by its one reader as a
/// body's are (engine_run_answer.rs `facts_of`), so a constructor is gated as a body is
/// (run_ids.rs, THE JOIN): its COVERS are the constructors of every ADT its entry holds under
/// each holding unit (`ctors`, [`ctors_of`]; sorted here, one each), its CLASS `Clean`, or
/// `Untyped` with `TYPES_NOT_CHECKED` when an ADT's head of the task did not freeze
/// (`adt_failed`: a constructor of a type whose fields were not made is not run; the head's
/// finding says why), and no typed record (a constructor has no body). Each is a function of
/// what the row's out already holds (each cell's frozen digest or its failure's, each unit's
/// prefix), so the trailer moves no reader on its own. Refused by name past a 32-bit count.
fn ctor_facts(mut ctors: SVec<'_, [u8; 16]>, adt_failed: bool, payload: &mut std::vec::Vec<u8, Global>, alloc: SharedAlloc<'_>) -> Result<(), &'static str> {
    use crate::engine_body_facts::Class;
    ctors.sort_unstable();
    ctors.dedup();
    let class = match adt_failed {
        false => Class::Clean,
        true => Class::Untyped { rule: crate::body_types::TYPES_NOT_CHECKED },
    };
    let mut facts: SVec<'_, u8> = SVec::new_in(alloc);
    crate::engine_body_facts::write(class, &ctors, None, &mut facts).map_err(|_| "its entry's ADTs give more constructors than a run facts record counts (engine_heads.rs `ctor_facts`)")?;
    let len = u32::try_from(facts.len()).map_err(|_| "its run facts are longer than a head payload's 32-bit trailer length (engine_heads.rs `ctor_facts`)")?;
    payload.reserve_exact(facts.len().saturating_add(4));
    payload.extend_from_slice(&facts);
    payload.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

/// One head task's freezes under each unit that holds its file, gathered in unit order (lane
/// RU1, [`head_cell`]): its row is their union, each unit's cells its own. Allocates through
/// the phase's handed `alloc` only; the first unit's [`WorkedHead`] holds the union of the
/// rest (its vectors are the worker's).
struct UnitsMade<'a> {
    alloc: SharedAlloc<'a>,
    /// Its share of each unit's worker time.
    ns: u64,
    /// The first unit's failure, the cells every failing unit's refusal is a function of, and
    /// whether one is owed (a panic): the task is refused whole.
    failed: Option<(&'static str, SVec<'a, CellId>, bool)>,
    /// Each unit's crate prefix and out, in unit order ([`HEAD_UNITS_OUT_DOMAIN`]).
    out: blaze_ids::Hasher,
    /// What every unit read, and the refusal cells of a holding unit that was not whole.
    reads: SVec<'a, CellId>,
    /// The first unit's freeze, the later units' defines, items, runs and failed heads
    /// appended in unit order ([`WorkedHead::payload`]: one run a (unit, cell)).
    first: Option<WorkedHead<'a>>,
}

impl<'a> UnitsMade<'a> {
    fn new(alloc: SharedAlloc<'a>) -> UnitsMade<'a> {
        UnitsMade { alloc, ns: 0, failed: None, out: blaze_ids::Hasher::new(HEAD_UNITS_OUT_DOMAIN), reads: SVec::new_in(alloc), first: None }
    }

    /// Unit `prefix`'s freeze of the task (its items carry `prefix` already, `heads_of`).
    fn take(&mut self, prefix: [u8; 8], mut one: WorkedHead<'a>) {
        self.out.update(&prefix).update(&one.out.0);
        self.reads.extend(one.reads.iter().copied());
        // Its list is in the union now: freed here, not kept a second time to the phase's merge
        // with the first unit's freeze (lane H8: each list was held three times until its row).
        one.reads.clear();
        one.reads.shrink_to_fit();
        match &mut self.first {
            None => self.first = Some(one),
            Some(all) => {
                all.defines.extend(one.defines.iter().copied());
                all.items.extend(one.items.iter().copied());
                all.payload.extend_from_slice(&one.payload);
                all.failed.extend(one.failed.iter().copied());
                all.ctors.extend(one.ctors.iter().copied());
                all.adt_failed |= one.adt_failed;
            }
        }
    }

    /// A unit could not freeze the task: why, and the cells that is a function of.
    fn fail(&mut self, why: &'static str, cells: &[CellId], owed: bool) {
        match &mut self.failed {
            Some((_, all, was_owed)) => {
                all.extend_from_slice(cells);
                *was_owed |= owed;
            }
            None => {
                let mut all = SVec::with_capacity_in(cells.len(), self.alloc);
                all.extend_from_slice(cells);
                self.failed = Some((why, all, owed));
            }
        }
    }
}

/// A lookup in a member listing identifies the producer, but its task can still fail as
/// a whole or lose this unit. A reader may cut off at it only if the unit's cell exists.
/// Same-wave definitions include recursive groups frozen against the current snapshot.
/// An earlier definition stands until the engine's next wave if its producer moves.
fn require_producers<'p>(
    accs: &mut [(Task, UnitsMade<'p>)], tasks: &[Task], today: &[(CellId, ContentId)],
    view: View<'_, '_>, at_phase: SharedAlloc<'p>, words: Alloc<'static>,
) {
    let mut entering = SVec::with_capacity_in(tasks.len(), at_phase);
    entering.extend(tasks.iter().map(|t| t.planned.name));
    entering.sort_unstable();
    let mut missing = SVec::new_in(at_phase);
    for (at, (_, made)) in accs.iter().enumerate() {
        if made.failed.is_some() { continue; }
        if let Some(id) = made.reads.iter().copied().find(|id| match id.of.kind {
            Kind::Signature => crate::engine_scope_reads::today(today, id).is_none()
                && (entering.binary_search(&id.of).is_ok() || view.cell(id) == blaze_drive::key::ABSENT),
            Kind::FileItems => view.cell(id) == blaze_drive::key::ABSENT,
            _ => false,
        }) {
            missing.push((at, id));
        }
    }
    if missing.is_empty() { return; }
    // Only the refusal path needs the reverse edges within this wave. A missing
    // producer also prevents any result downstream from being used as a cutoff.
    let mut readers = SVec::new_in(at_phase);
    for (at, (_, made)) in accs.iter().enumerate() {
        readers.extend(made.reads.iter().filter(|id| id.of.kind == Kind::Signature).map(|id| (id.of, at, *id)));
    }
    readers.sort_unstable();
    readers.dedup();
    while let Some((at, absent)) = missing.pop() {
        let (task, made) = &mut accs[at];
        if made.failed.is_some() { continue; }
        let why = crate::engine_head_notes::leak(words, format_args!("head {:?} read {:?}, which its producer did not define", task.planned.name, absent));
        let reads = core::mem::replace(&mut made.reads, SVec::new_in(at_phase));
        made.failed = Some((why, reads, false));
        let name = task.planned.name;
        let from = readers.partition_point(|&(producer, _, _)| producer < name);
        missing.extend(readers[from..].iter().take_while(|row| row.0 == name).map(|&(_, reader, id)| (reader, id)));
    }
}

impl HeadsStage {
    /// The head tasks of one Heads phase (module docs, "THE HEAD TASKS"): each member crate's
    /// setup once ([`crate_setup`]), its tasks cut into split jobs over every worker
    /// ([`split_count`]), merged in task order ([`in_split_order`]), each result its row in
    /// `buffer`. Overlaps any closed-dep `ImplBucket` jobs (`closed_jobs`) onto the same worker
    /// scope so closed-dep impl tables execute concurrently with `HeadsSetup` and `HeadsSplit`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    fn run_items<'a, 'p, 'r, 'l, 'h>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        handles: &Handles<'h, '_, '_, '_>,
        kept: &mut Kept,
        tasks: &[Task],
        view: View<'_, 'a>,
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        at_phase: SharedAlloc<'p>,
        frame: &Frame,
        snapshot: &crate::store::StoreSnapshot,
        snapshot_owner: &crate::store::SharedSnapshot,
        scope_cache: &mut Option<HeadsScopeCache<'r>>,
        closed_jobs: &[(usize, Job<'r, 'p>)],
        at_cells: bool,
        lazy_snap: &'l crate::store::StoreSnapshot,
        heads_lazy: &mut Option<crate::front_load::ScopeRecords<'l>>,
        once: &crate::engine_heads_once::HeadsOnce,
        by_path_slot: &mut Option<blaze_hash::Map<&'h [u8], usize, blaze_hash::FixedState, SharedAlloc<'p>>>,
        subject_entries: &mut SubjectEntries<'p>,
        subjects: &mut SVec<'p, Subject>,
        loaded_subjects: &mut SVec<'p, [u8; 8]>,
    ) -> Result<SVec<'p, ClosedWorked<'p>>, CliError> {
        let heap = handles.env.heap;
        let g = heap.alloc();
        if tasks.is_empty() {
            return Ok(SVec::new_in(at_phase));
        }
        let refused = &mut self.refused;
        // A refusal with the cells it is a function of, each as the view holds it today (ABSENT
        // where no task defines it), through `failed_with` (sorted, once each): its row stands
        // until one moves (plan row A8). None: owed, said at each such call.
        let mut refuse = |task: &Task, why: &'static str, cells: &[CellId], buffer: &mut Buffer<'a>| {
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(cells.len(), alloc);
            reads.extend(cells.iter().map(|id| (*id, view.cell(id))));
            buffer.done.push(crate::engine_tasks::failed_with(task, reads, alloc));
            // The whole task failed: named by the file its entry is in.
            let file = handles
                .outlined
                .binary_search_by(|one| one.name.cmp(&task.planned.file))
                .ok()
                .and_then(|at| handles.outlined.get(at))
                .and_then(|one| core::str::from_utf8(&one.path).ok());
            let head = match file {
                Some(path) => crate::engine_head_notes::leak(g, format_args!("a head of '{path}'")),
                None => "a head of a file the outlines do not hold",
            };
            refused.push(crate::engine_head_notes::Refused::whole(task.planned.name, head, why));
        };
        let (Some(graph), Some(names)) = (handles.graph, handles.names) else {
            // No `UnitGraph` task planned, or no names stage: no cell of this command says so,
            // so owed until the plan's inputs are cells (plan row D2).
            for task in tasks.iter() {
                refuse(task, "this command has no unit graph or no crate's names, so no head", &[], buffer);
            }
            return Ok(SVec::new_in(at_phase));
        };
        // The crate list, each member's check scope and the packs are the graph record's.
        let graph_cell = CellId { of: graph.name, cell: WHOLE };
        let Some(list) = kept.list(cx, store, &graph, frame)? else {
            for task in tasks.iter() {
                refuse(task, "the crates' inputs could not be made (names.rs `world`; the `UnitGraph` record's finding says why)", &[graph_cell], buffer);
            }
            return Ok(SVec::new_in(at_phase));
        };
        let (packs, corrected) = match kept.lazy_trees(handles.env, graph.out, list) {
            Ok(trees) => trees,
            Err(why) => {
                // The heap refused room for the tree table (body_types.rs `lazy_trees`): owed.
                for task in tasks.iter() {
                    refuse(task, why, &[], buffer);
                }
                return Ok(SVec::new_in(at_phase));
            }
        };
        let cache = scope_cache.get_or_insert_with(|| HeadsScopeCache::new(CrateGraph::len(&list), g));
        let workers = usize::try_from(handles.workers.get()).unwrap_or(1).max(1);
        // 1. Only members whose module tree reaches an entered task's file have their check scope
        // built up front (with non-member dependencies deferred to [`HeadsScopeRecords`]); other
        // members are touched only if an entered task's file was not held by any reaching member,
        // or if they appear as a member dependency inside an active job's scope.
        let mut task_files: Vec<Name, &Frame> = Vec::with_capacity_in(tasks.len(), frame);
        task_files.extend(tasks.iter().map(|task| task.planned.file));
        task_files.sort_unstable();
        task_files.dedup();
        let by_path = &*by_path_slot.get_or_insert_with(|| outlined_by_path(handles.outlined, at_phase));
        let mut jobs: SVec<'p, ItemJob<'r, 'p>> = SVec::new_in(at_phase);
        let mut unvisited_members: Vec<usize, &Frame> = Vec::new_in(frame);
        // Built lazily only if a task's file is in no member crate's items record, avoiding
        // probing `view.cell` for every standing member crate on normal incremental edits.
        let mut no_member_cells: Option<Vec<CellId, &Frame>> = None;
        // Every member's items record was read: then a file none lists is no unit's (a `cfg` on
        // its `mod` cut it in every unit, engine_body_plan.rs), not a refusal.
        let mut every_member_read = true;
        // Why the first member whose records could not be read was not (its typed cause, the
        // crate named) and how many were not: what a task no unit holds then prints (rule 19).
        let mut unread: Option<&'static str> = None;
        let mut unread_count = 0u32;
        // Deduplicate cfg-blind module tree walks by crate root (lib + test share one root) and
        // fan them out across workers on large multi-member workspaces (rule 25 deterministic order).
        let mut scoped_members: Vec<(usize, usize), &Frame> = Vec::new_in(frame);
        let mut unique_roots: Vec<Name, &Frame> = Vec::new_in(frame);
        let mut reached_root_index: blaze_hash::Map<Name, usize, blaze_hash::FixedState, &Frame> =
            blaze_hash::Map::new(blaze_hash::MapOpts { hasher: blaze_hash::FixedState, alloc: frame, capacity: None });
        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at) && handles.scoped.is_none_or(|s| s.allows(*at))) {
            let root = list.root(member);
            let root_name = Name::file(root.strip_prefix("/0/").unwrap_or(root).as_bytes());
            let reach_idx = match reached_root_index.get(&root_name).copied() {
                Some(idx) => idx,
                None => {
                    let idx = unique_roots.len();
                    unique_roots.push(root_name);
                    reached_root_index.insert(root_name, idx);
                    idx
                }
            };
            scoped_members.push((member, reach_idx));
        }
        let mut reached_roots: SVec<'p, (SVec<'p, Name>, bool)> = SVec::with_capacity_in(unique_roots.len(), at_phase);
        let task_files_slice = task_files.as_slice();
        let outlined_ref = handles.outlined;
        let active_workers = usize::try_from(cx.workers().min(handles.workers.get())).unwrap_or(1).max(1);
        if active_workers > 1 && unique_roots.len() >= 4 {
            let roots_slice = unique_roots.as_slice();
            let chunks = active_workers.min(roots_slice.len()).max(1);
            let chunk_len = roots_slice.len().div_ceil(chunks);
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            let mut running = Vec::with_capacity_in(chunks, frame);
            for chunk in roots_slice.chunks(chunk_len) {
                running.push(scope.may_overlap("engine heads: reached roots", bstd::mem::TASK_FRAME, move |_cx: &Cx| {
                    let local_g = heap.alloc();
                    let mut out: SVec<'p, Result<(SVec<'p, Name>, bool), crate::engine_body_plan::Unoutlined>> = SVec::with_capacity_in(chunk.len(), at_phase);
                    for &root_name in chunk.iter() {
                        match crate::engine_body_plan::reached(outlined_ref, root_name, local_g) {
                            Ok(reach) => {
                                let reaches_any = if task_files_slice.len() <= reach.len() {
                                    task_files_slice.iter().any(|file| reach.binary_search(file).is_ok())
                                } else {
                                    reach.iter().any(|file| task_files_slice.binary_search(file).is_ok())
                                };
                                let mut kept_reach = SVec::new_in(at_phase);
                                if reaches_any {
                                    kept_reach.reserve(reach.len());
                                    kept_reach.extend_from_slice(&reach);
                                }
                                out.push(Ok((kept_reach, reaches_any)));
                            }
                            Err(err) => out.push(Err(err)),
                        }
                    }
                    Ok::<_, Infallible>(out)
                }));
            }
            let mut stopped = None;
            let mut first_unoutlined: Option<crate::engine_body_plan::Unoutlined> = None;
            for handle in running.into_iter() {
                match handle.join(cx) {
                    Ok(part) => {
                        for item in part.into_iter() {
                            match item {
                                Ok(pair) => reached_roots.push(pair),
                                Err(err) => {
                                    first_unoutlined.get_or_insert(err);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        stopped.get_or_insert(error);
                    }
                }
            }
            scope.join(cx).map_err(CliError::Pool)?;
            if let Some(error) = stopped {
                return Err(CliError::Pool(error));
            }
            if let Some(err) = first_unoutlined {
                return Err(EngineCheckError::Unoutlined(err).into());
            }
        } else {
            for &root_name in unique_roots.iter() {
                let reach = crate::engine_body_plan::reached(outlined_ref, root_name, g).map_err(EngineCheckError::Unoutlined)?;
                let reaches_any = if task_files_slice.len() <= reach.len() {
                    task_files_slice.iter().any(|file| reach.binary_search(file).is_ok())
                } else {
                    reach.iter().any(|file| task_files_slice.binary_search(file).is_ok())
                };
                let mut kept_reach = SVec::new_in(at_phase);
                if reaches_any {
                    kept_reach.reserve(reach.len());
                    kept_reach.extend_from_slice(&reach);
                }
                reached_roots.push((kept_reach, reaches_any));
            }
        }
        // Each reaching member unit's cfg-blind walk index (only units whose walk reaches at least
        // one task file can ever match `task.planned.file`).
        let mut walks: Vec<(Name, [u8; 8], usize), &Frame> = Vec::new_in(frame);
        let mut pending_subjects: Vec<([u8; 8], u32, usize), &Frame> = Vec::new_in(frame);
        let mut held_files: Vec<Name, &Frame> = Vec::new_in(frame);
        let erased = blaze_own::Erased::new(frame);
        let fa = erased.handle();
        let make_job = |prefix, scope: HeadsScope<'r, 'p>, whole, cut_words, cells| ItemJob {
            prefix,
            own: scope.own,
            rest: scope.rest,
            lang: scope.lang,
            whole,
            cut_words,
            cells,
            tasks: SVec::new_in(at_phase),
        };
        for &(member, reach_idx) in scoped_members.iter() {
            let scope_name = blaze_drive::key::crate_scope_name(&list.key(member));
            let prefix = list.key(member).prefix();
            let reaches_any = reached_roots.get(reach_idx).is_some_and(|(_, any)| *any);
            if !reaches_any {
                unvisited_members.push(member);
                continue;
            }
            walks.push((scope_name, prefix, reach_idx));
            let (scope, whole) = match scope_by_need_heads(cache, read, cx, store, snapshot, names, &list, member, at_phase, frame, heap)? {
                Ok(made) => made,
                Err(cause) => {
                    every_member_read = false;
                    unread_count = unread_count.saturating_add(1);
                    unread.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("its check scope was not read: {cause}")));
                    continue;
                }
            };
            let cells = scope_cells(graph.name, &list, member, Some(&scope), at_phase);
            let cut_words = scope.cut_cause(&list).map(|cause| crate::engine_head_notes::leak(g, format_args!("its crate's {cause}")));
            let items = ItemsRecord::from_verified(scope.own.records.items_proof, fa);
            for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                let Ok(Some(row)) = items.file_surface(file) else { continue };
                let Some(path) = row.path.in_package(list.shown_from(member), fa) else { continue };
                let Some(path) = path.strip_prefix("/0/") else { continue };
                if let Some(idx) = find_outlined_index(by_path, path.as_bytes()) {
                    pending_subjects.push((prefix, file, idx));
                    if let Some(one) = handles.outlined.get(idx) {
                        held_files.push(one.name);
                    }
                }
            }
            loaded_subjects.push(prefix);
            jobs.push(make_job(prefix, scope, whole, cut_words, cells));
        }
        // Fallback: if any entered task's file was not held by any reaching member (e.g. a file
        // loaded via macro-expanded `mod`, or a file cut by `#[cfg]` where we must verify all
        // remaining members' own items records to prove no unit holds it), inspect only their own
        // items records first and open a check scope only if one actually holds a task file.
        held_files.extend(subjects.iter().map(|row| row.slot));
        held_files.sort_unstable();
        held_files.dedup();
        if task_files.iter().any(|file| held_files.binary_search(file).is_err()) {
            for member in unvisited_members.iter().copied() {
                let prefix = list.key(member).prefix();
                let own_records = match cache.records_of(read, cx, store, names, &list, member, g) {
                    Ok((_, own)) => own,
                    Err(cause) => {
                        every_member_read = false;
                        unread_count = unread_count.saturating_add(1);
                        unread.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("its check scope was not read: {cause}")));
                        continue;
                    }
                };
                let items = ItemsRecord::from_verified(own_records.items_proof, fa);
                let mut holds_task_file = false;
                for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                    let Ok(Some(row)) = items.file_surface(file) else { continue };
                    let Some(path) = row.path.in_package(list.shown_from(member), fa) else { continue };
                    let Some(path) = path.strip_prefix("/0/") else { continue };
                    if find_outlined_index(by_path, path.as_bytes())
                        .and_then(|idx| handles.outlined.get(idx))
                        .is_some_and(|one| task_files.binary_search(&one.name).is_ok())
                    {
                        holds_task_file = true;
                        break;
                    }
                }
                if !holds_task_file {
                    continue;
                }
                let (scope, whole) = match scope_by_need_heads(cache, read, cx, store, snapshot, names, &list, member, at_phase, frame, heap)? {
                    Ok(made) => made,
                    Err(cause) => {
                        every_member_read = false;
                        unread_count = unread_count.saturating_add(1);
                        unread.get_or_insert_with(|| crate::engine_head_notes::leak(g, format_args!("its check scope was not read: {cause}")));
                        continue;
                    }
                };
                let cells = scope_cells(graph.name, &list, member, Some(&scope), at_phase);
                let cut_words = scope.cut_cause(&list).map(|cause| crate::engine_head_notes::leak(g, format_args!("its crate's {cause}")));
                for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                    let Ok(Some(row)) = items.file_surface(file) else { continue };
                    let Some(path) = row.path.in_package(list.shown_from(member), fa) else { continue };
                    let Some(path) = path.strip_prefix("/0/") else { continue };
                    if let Some(idx) = find_outlined_index(by_path, path.as_bytes()) {
                        pending_subjects.push((prefix, file, idx));
                    }
                }
                loaded_subjects.push(prefix);
                jobs.push(make_job(prefix, scope, whole, cut_words, cells));
            }
        }
        // Populate deduplicated `(SignatureFile, entries_of)` in parallel across all workers.
        subject_entries.populate(cx, handles.outlined, &pending_subjects, subjects, handles.workers, at_phase, frame)?;
        subjects.sort_unstable();
        subjects.dedup();
        jobs.sort_unstable_by_key(|job| job.prefix);
        // Each task to EVERY member unit (by prefix) whose files hold its file (lane RU1,
        // `head_cell`): each freezes the task's heads under its own records, one cell a unit.
        let mut by_file: SVec<'p, (Name, [u8; 8])> = SVec::with_capacity_in(subjects.len(), at_phase);
        by_file.extend(subjects.iter().map(|row| (row.slot, row.prefix)));
        by_file.sort_unstable();
        by_file.dedup();
        // Flat sorted list of ONLY `(file, unit, prefix)` pairs where a unit's cfg-blind walk
        // reaches `file` yet the unit does not hold it in `by_file`. Empty (zero allocations and
        // zero per-task lookups) on the common non-cfg-cut path.
        let mut unheld_reaching: SVec<'p, (Name, Name, [u8; 8])> = SVec::new_in(at_phase);
        for &(unit, prefix, reach_idx) in walks.iter() {
            let Some((reach, _)) = reached_roots.get(reach_idx) else { continue };
            if task_files.len() <= reach.len() {
                for &file in task_files.iter() {
                    if reach.binary_search(&file).is_ok() && by_file.binary_search(&(file, prefix)).is_err() {
                        unheld_reaching.push((file, unit, prefix));
                    }
                }
            } else {
                let mut prev: Option<Name> = None;
                for &file in reach.iter() {
                    if prev == Some(file) {
                        continue;
                    }
                    prev = Some(file);
                    if task_files.binary_search(&file).is_ok() && by_file.binary_search(&(file, prefix)).is_err() {
                        unheld_reaching.push((file, unit, prefix));
                    }
                }
            }
        }
        if unheld_reaching.len() > 1 {
            unheld_reaching.sort_unstable();
            unheld_reaching.dedup();
        }
        // Each task that some unit runs, sorted by name, and what its units made (step 3).
        let mut accs: SVec<'p, (Task, UnitsMade<'p>)> = SVec::with_capacity_in(tasks.len(), at_phase);
        let mut holding: SVec<'p, usize> = SVec::new_in(at_phase);
        // Each task's (name, the prefix of the unit that shows it on the status board): the
        // first whole unit that holds it, so a task frozen under two units is one task started
        // and done there (engine_status.rs), as the phase's known count counts it.
        let mut shows: SVec<'p, (Name, [u8; 8])> = SVec::with_capacity_in(tasks.len(), at_phase);
        for task in tasks.iter() {
            let at = by_file.partition_point(|(file, _)| *file < task.planned.file);
            holding.clear();
            if let Some(rest) = by_file.get(at..) {
                holding.extend(rest.iter().take_while(|(file, _)| *file == task.planned.file).filter_map(|(_, prefix)| jobs.binary_search_by_key(prefix, |job| job.prefix).ok()));
            }
            // The plant (`HeadUnits`): one unit alone, as before the rule.
            match self.freeze {
                HeadUnits::Each | HeadUnits::NoExpansionReads => {}
                HeadUnits::OnlyFirst => holding.truncate(1),
                HeadUnits::OnlyLast => {
                    if let Some(last) = holding.last().copied() {
                        holding.clear();
                        holding.push(last);
                    }
                }
            }
            let whole = holding.iter().copied().find(|&idx| jobs.get(idx).is_some_and(|job| job.whole));
            match (holding.first().copied(), whole) {
                // No unit holds its file under its cfg: no unit's head, a row with no finding on
                // the cells that say so (each member's names cell, the graph's), so a unit that
                // comes to hold it enters it again (engine_body_plan.rs, as its bodies).
                (None, _) if every_member_read => {
                    let no_member = no_member_cells.get_or_insert_with(|| {
                        let mut cells = Vec::new_in(frame);
                        cells.push(graph_cell);
                        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at) && handles.scoped.is_none_or(|s| s.allows(*at))) {
                            let scope_name = blaze_drive::key::crate_scope_name(&list.key(member));
                            let prefix = list.key(member).prefix();
                            cells.push(crate::engine_body_plan::file_not_held_cell(view, scope_name, prefix));
                        }
                        cells
                    });
                    let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(no_member.len(), alloc);
                    reads.extend(no_member.iter().map(|id| (*id, view.cell(id))));
                    reads.sort_unstable_by_key(|(id, _)| *id);
                    reads.dedup_by_key(|(id, _)| *id);
                    let out = no_units_out(task.planned.name);
                    buffer.done.push(Done { name: task.planned.name, key: task.key, out, defines: SVec::new_in(alloc), reads, findings: 0, failed: false });
                }
                (None, _) => {
                    let no_member = no_member_cells.get_or_insert_with(|| {
                        let mut cells = Vec::new_in(frame);
                        cells.push(graph_cell);
                        for member in (0..CrateGraph::len(&list)).filter(|at| list.member(*at) && handles.scoped.is_none_or(|s| s.allows(*at))) {
                            let scope_name = blaze_drive::key::crate_scope_name(&list.key(member));
                            let prefix = list.key(member).prefix();
                            cells.push(crate::engine_body_plan::file_not_held_cell(view, scope_name, prefix));
                        }
                        cells
                    });
                    let why = crate::engine_head_notes::leak(
                        g,
                        format_args!(
                            "its file is in no member crate's items record, and {unread_count} member(s)' records could not be read, the first because {} (engine_heads.rs `run_items`)",
                            unread.unwrap_or("of no cause kept (engine_heads.rs `run_items` keeps the first)")
                        ),
                    );
                    refuse(task, why, no_member, buffer);
                }
                // Its member's names cell, each scope crate's that had records, and the one the
                // scope was cut at, read ABSENT: its task that comes, or its fix, moves the row.
                // The cut's cause printed, the crate named (rule 19).
                (Some(at), None) => {
                    if let Some(job) = jobs.get(at) {
                        let why = job.cut_words.unwrap_or("its crate's check scope was cut and `scope_of` kept no cut (engine_bodies.rs `scope_of`)");
                        refuse(task, why, &job.cells, buffer);
                    }
                }
                // Frozen under each whole unit; a unit whose scope lacks a names record defines
                // no cell (its readers read it ABSENT), and the row reads that unit's refusal
                // cells, so the fix that makes it whole enters the task again (rule 21).
                (Some(_), Some(first_whole)) => {
                    if let Some(job) = jobs.get(first_whole) {
                        shows.push((task.planned.name, job.prefix));
                    }
                    let mut made = UnitsMade::new(at_phase);
                    for at in holding.iter().copied() {
                        if let Some(job) = jobs.get_mut(at) {
                            match job.whole {
                                true => job.tasks.push(*task),
                                false => made.reads.extend(job.cells.iter().copied()),
                            }
                        }
                    }
                    // The units its file is NOT frozen under though their walk reaches it: their
                    // not-held cells, so one that comes to hold the file moves the row.
                    if !unheld_reaching.is_empty() {
                        let from = unheld_reaching.partition_point(|(file, _, _)| *file < task.planned.file);
                        if let Some(rest) = unheld_reaching.get(from..) {
                            for &(_, unit, prefix) in rest.iter().take_while(|(file, _, _)| *file == task.planned.file) {
                                made.reads.push(crate::engine_body_plan::file_not_held_cell(view, unit, prefix));
                            }
                        }
                    }
                    accs.push((*task, made));
                }
            }
        }
        accs.sort_unstable_by_key(|(task, _)| task.planned.name);
        accs.dedup_by_key(|(task, _)| task.planned.name);
        shows.sort_unstable();
        shows.dedup_by_key(|(name, _)| *name);
        let shows = &shows[..];
        jobs.retain(|job| !job.tasks.is_empty());
        // Populate `subjects` for any member dependency crate appearing inside active jobs' scopes
        // so `Owners::place` and `crate_setup` identify every in-scope member crate accurately.
        loaded_subjects.sort_unstable();
        loaded_subjects.dedup();
        if !jobs.is_empty() {
            let mut dep_members: Vec<usize, &Frame> = Vec::new_in(frame);
            for job in jobs.iter() {
                for one in job.rest.iter() {
                    if list.member(one.world_at) && loaded_subjects.binary_search(&one.key.prefix()).is_err() {
                        dep_members.push(one.world_at);
                    }
                }
            }
            dep_members.sort_unstable();
            dep_members.dedup();
            if !dep_members.is_empty() {
                let mut pending_deps: Vec<([u8; 8], u32, usize), &Frame> = Vec::new_in(frame);
                for member in dep_members.iter().copied() {
                    let Ok((_, own_records)) = cache.records_of(read, cx, store, names, &list, member, g) else { continue };
                    let items = ItemsRecord::from_verified(own_records.items_proof, fa);
                    let prefix = list.key(member).prefix();
                    for file in 0..u32::try_from(items.files()).unwrap_or(u32::MAX) {
                        let Ok(Some(row)) = items.file_surface(file) else { continue };
                        let Some(path) = row.path.in_package(list.shown_from(member), fa) else { continue };
                        let Some(path) = path.strip_prefix("/0/") else { continue };
                        if let Some(idx) = find_outlined_index(by_path, path.as_bytes()) {
                            pending_deps.push((prefix, file, idx));
                        }
                    }
                    loaded_subjects.push(prefix);
                }
                loaded_subjects.sort_unstable();
                loaded_subjects.dedup();
                if !pending_deps.is_empty() {
                    subject_entries.populate(cx, handles.outlined, &pending_deps, subjects, handles.workers, at_phase, frame)?;
                    subjects.sort_unstable();
                    subjects.dedup();
                }
            }
        }
        let lazy = heads_lazy.get_or_insert_with(|| crate::front_load::ScopeRecords::new(lazy_snap, CrateGraph::len(&list), heap));
        let lazy_before = lazy.counts();
        let scope_records = HeadsScopeRecords::new(cache, lazy, CrateGraph::len(&list), at_phase);
        let scope_records = &scope_records;
        // Group the tasks of each file before choosing whole-file cuts.
        for job in jobs.iter_mut() {
            job.tasks.sort_unstable_by_key(|task| (task.planned.file, task.planned.name));
        }
        // 2. Each member's heads, at once, on the workers: first each crate's setup, once a crate
        // (`crate_setup`: its listing from the request's table, its lang index's reads, its
        // receiver pointers and theirs), then its tasks cut into split jobs (`split_count`),
        // each over a source and recorders of its own (`heads_of`), all crates' splits at once.
        let procs = names.stage().names.as_ref().and_then(|names| names.procs());
        let root = handles.root;
        let fs = &handles.env.fs;
        let outlined = handles.outlined;
        let clock = handles.clock;
        let subjects = &subjects[..];
        let expansion_reads = self.freeze != HeadUnits::NoExpansionReads;
        type Ran<'p> = Result<Result<SVec<'p, Result<WorkedHead<'p>, HeadFailed<'p>>>, &'static str>, work::Panicked>;
        type Setup<'p> = Result<Result<CrateSetup<'p>, &'static str>, work::Panicked>;
        // The request's crate listings, lent to every worker as the frozen heads are.
        let head_jobs = self.jobs;
        let listings: &Listings = self.listings.get_or_insert_with(|| Listings::new(heap, head_jobs.listings, Some(snapshot_owner.clone()), handles.tool));
        for (input, records) in cache.slots.iter().copied().flatten() {
            listings.record_scope_key(records.key, input.at);
        }
        let board = handles.board;
        // Each crate's setup, each split job, and any closed-dep `ImplBucket` jobs overlap inside
        // one worker scope so idle cores during `HeadsSetup` and `HeadsSplit` build dep tables.
        let mut setups: SVec<'p, (Setup<'p>, u64)> = SVec::with_capacity_in(jobs.len(), at_phase);
        let mut closed_worked: SVec<'p, ClosedWorked<'p>> = SVec::with_capacity_in(closed_jobs.len(), at_phase);
        let mut splits: SVec<'p, SplitAt> = SVec::new_in(at_phase);
        let mut arrived: SVec<'p, (u32, (Ran<'p>, u64))> = SVec::new_in(at_phase);
        let mut total = 0usize;
        let mut most_splits = 0usize;
        let mut split_files = 0usize;
        {
            let max_workers = handles.workers;
            let scope = cx.scope();
            scope.narrow_workers(handles.workers, Class::Foreground);
            // Spawn `crate_setup` largest-first (LPT) so `HeadsSplit` is never queued behind small
            // crates or small dep `ImplBucket` tasks, then immediately spawn `closed_jobs` in LPT
            // order so heavy dep crates (`core`, `std`, `regex-automata`, ...) overlap with
            // `HeadsSetup` and `HeadsSplit` from t=0 without sitting at the tail.
            let mut setup_dispatch: Vec<usize, &Frame> = Vec::with_capacity_in(jobs.len(), frame);
            setup_dispatch.extend(0..jobs.len());
            setup_dispatch.sort_unstable_by_key(|&at| {
                let tasks_len = jobs.get(at).map_or(0, |job| job.tasks.len());
                (core::cmp::Reverse(tasks_len), at)
            });
            let mut running = Vec::with_capacity_in(jobs.len(), frame);
            for at in setup_dispatch.into_iter() {
                let Some(job) = jobs.get(at) else { continue };
                running.push((
                    at,
                    scope.may_overlap("engine heads: a crate's setup", HEADS_FRAME, move |cx: &Cx| {
                        let began = clock.now();
                        let made = work::unwinding(|| crate_setup(cx, root, fs, outlined, packs, corrected, procs, job, scope_records, subjects, view, heap, listings, snapshot, at_phase));
                        Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns), crate::engine_times::ns(began.since_start())))
                    }),
                ));
            }
            let mut closed_dispatch: Vec<usize, &Frame> = Vec::with_capacity_in(closed_jobs.len(), frame);
            closed_dispatch.extend(0..closed_jobs.len());
            closed_dispatch.sort_unstable_by_key(|&at| {
                let weight = closed_jobs.get(at).map_or(0, |(_, job)| impl_job_weight(job, g));
                (core::cmp::Reverse(weight), at)
            });
            let mut running_closed = Vec::with_capacity_in(closed_jobs.len(), frame);
            for at in closed_dispatch.into_iter() {
                let Some((_, job)) = closed_jobs.get(at) else { continue };
                running_closed.push((
                    at,
                    scope.may_overlap("engine impl tables (closed dep)", HEADS_FRAME, move |cx: &Cx| {
                        let began = clock.now();
                        let made = if board.stopping().is_some() {
                            Ok(Err(NOT_BEGUN))
                        } else {
                            work::unwinding(|| tables_of(cx, root, fs, outlined, packs, corrected, procs, job, scope_records, &[], at_cells, heap, listings, snapshot, max_workers, at_phase))
                        };
                        Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns), crate::engine_times::ns(began.since_start())))
                    }),
                ));
            }
            let mut setups_paired: Vec<(usize, (Setup<'p>, u64)), &Frame> = Vec::with_capacity_in(jobs.len(), frame);
            let mut stopped = None;
            for (at, handle) in running.into_iter() {
                match handle.join(cx) {
                    Ok((made, ns, start)) => {
                        times.job(crate::engine_times::Step::HeadsSetup, start, start.saturating_add(ns));
                        setups_paired.push((at, (made, ns)));
                    }
                    Err(error) => {
                        stopped.get_or_insert(error);
                    }
                }
            }
            // Rule 25: restore exact original `jobs` order for `setups`.
            setups_paired.sort_unstable_by_key(|(at, _)| *at);
            setups.extend(setups_paired.into_iter().map(|(_, one)| one));
            if stopped.is_none() {
                // Sum per-crate weights already computed in parallel inside `crate_setup`.
                let mut total_weight = 0usize;
                for (job, (setup, _)) in jobs.iter().zip(setups.iter()) {
                    if let Ok(Ok(setup)) = setup {
                        total = total.saturating_add(job.tasks.len());
                        total_weight = total_weight.saturating_add(setup.total_weight);
                    }
                }
                let mut file_weights: Vec<(usize, usize), &Frame> = Vec::new_in(frame);
                for (at, (job, (setup, _))) in jobs.iter().zip(setups.iter()).enumerate() {
                    let Ok(Ok(setup)) = setup else {
                        continue;
                    };
                    let weights = setup.task_weights.as_slice();
                    let len = job.tasks.len();
                    let crate_weight = setup.total_weight;
                    let n = split_count_weighted(head_jobs.split, len, crate_weight, total_weight, workers);
                    let before = splits.len();
                    let mut last_crossed = None;
                    let mut append = |range: core::ops::Range<usize>, weight: usize| {
                        if let Some(first) = job.tasks.get(range.start) {
                            let file = first.planned.file;
                            if range.start > 0 && job.tasks.get(range.start - 1).is_some_and(|prev| prev.planned.file == file) && last_crossed != Some(file) {
                                split_files += 1;
                                last_crossed = Some(file);
                            }
                        }
                        splits.push(SplitAt {
                            job: u32::try_from(at).unwrap_or(u32::MAX),
                            from: u32::try_from(range.start).unwrap_or(u32::MAX),
                            to: u32::try_from(range.end).unwrap_or(u32::MAX),
                            weight: u64::try_from(weight).unwrap_or(u64::MAX),
                        });
                    };
                    match head_jobs.split {
                        #[cfg(test)]
                        HeadSplit::Even(_) => {
                            for cut in 0..n {
                                let range = cut * len / n..(cut + 1) * len / n;
                                let w: usize = weights.get(range.clone()).map_or(0, |s| s.iter().copied().sum());
                                append(range, w);
                            }
                        }
                        HeadSplit::Rule | HeadSplit::Jobs(_) if n == 1 => append(0..len, crate_weight),
                        HeadSplit::Rule | HeadSplit::Jobs(_) => {
                            file_weights.clear();
                            let mut offset = 0usize;
                            for file in job.tasks.chunk_by(|a, b| a.planned.file == b.planned.file) {
                                let file_len = file.len();
                                let file_weight: usize = weights.get(offset..offset + file_len).map_or(file_len, |s| s.iter().copied().sum());
                                file_weights.push((file_len, file_weight));
                                offset += file_len;
                            }
                            for (range, weight) in weighted_file_splits(&file_weights, weights, crate_weight, n) {
                                append(range, weight);
                            }
                        }
                    }
                    most_splits = most_splits.max(splits.len() - before);
                }
                // Dispatch splits heaviest-first (LPT by work weight) so mega-crates/mega-files
                // and derive/impl-heavy splits start immediately rather than creating a late
                // straggler tail; `in_split_order` restores deterministic split order using each
                // split's original `at` index.
                let mut dispatch: Vec<usize, &Frame> = Vec::with_capacity_in(splits.len(), frame);
                dispatch.extend(0..splits.len());
                dispatch.sort_unstable_by(|&a, &b| {
                    let w_a = splits.get(a).map_or(0, |s| s.weight);
                    let w_b = splits.get(b).map_or(0, |s| s.weight);
                    w_b.cmp(&w_a).then_with(|| a.cmp(&b))
                });
                // Each split's result as it was joined, then in split order ([`in_split_order`]).
                arrived.reserve(splits.len());
                let mut running_splits = Vec::with_capacity_in(splits.len(), frame);
                let setups_ref = &setups[..];
                for at in dispatch.into_iter() {
                    let Some(split) = splits.get(at) else { continue };
                    let Ok(job_idx) = usize::try_from(split.job) else { continue };
                    let Ok(from_idx) = usize::try_from(split.from) else { continue };
                    let Ok(to_idx) = usize::try_from(split.to) else { continue };
                    let Some(job) = jobs.get(job_idx) else { continue };
                    // Only a crate whose setup was made has splits (above).
                    let Some(Ok(Ok(setup))) = setups_ref.get(job_idx).map(|(s, _)| s) else { continue };
                    let Some(tasks) = job.tasks.get(from_idx..to_idx) else { continue };
                    let at = u32::try_from(at).unwrap_or(u32::MAX);
                    running_splits.push((
                        at,
                        scope.may_overlap("engine heads", HEADS_FRAME, move |cx: &Cx| {
                            let began = clock.now();
                            let on_board = OnBoard { board, clock, shows };
                            let made = work::unwinding(|| heads_of(cx, root, fs, outlined, packs, corrected, procs, &list, job, scope_records, setup, tasks, subjects, view, expansion_reads, heap, once, listings, head_jobs.listings, on_board, snapshot, at_phase, workers));
                            Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns), crate::engine_times::ns(began.since_start())))
                        }),
                    ));
                }
                for (at, handle) in running_splits.into_iter() {
                    match handle.join(cx) {
                        Ok((made, ns, start)) => {
                            times.job(crate::engine_times::Step::HeadsSplit, start, start.saturating_add(ns));
                            arrived.push((at, (made, ns)));
                        }
                        Err(error) => {
                            stopped.get_or_insert(error);
                        }
                    }
                }
            }
            let mut closed_paired: SVec<'p, (usize, ClosedWorked<'p>)> = SVec::with_capacity_in(closed_jobs.len(), at_phase);
            for (at, handle) in running_closed.into_iter() {
                match handle.join(cx) {
                    Ok((made, ns, start)) => {
                        times.job(crate::engine_times::Step::HeadsTables, start, start.saturating_add(ns));
                        closed_paired.push((at, ClosedWorked { made, ns }));
                    }
                    Err(error) => {
                        stopped.get_or_insert(error);
                    }
                }
            }
            scope.join(cx).map_err(CliError::Pool)?;
            read.add_by_need(by_need_delta(lazy.counts(), lazy_before));
            if let Some(error) = stopped {
                return Err(CliError::Pool(error));
            }
            // Rule 25: restore exact original `closed_jobs` order before merging rows and shelf tables.
            closed_paired.sort_unstable_by_key(|(at, _)| *at);
            closed_worked.extend(closed_paired.into_iter().map(|(_, worked)| worked));
        }
        let worked = in_split_order(arrived, splits.len(), head_jobs.merge, at_phase).map_err(|error| CliError::Text { what: crate::engine_head_notes::leak(g, format_args!("{error}")) })?;
        // 3. Each unit's results gathered by task (units in prefix order, a unit's splits in task
        // order), then each task as the engine's row, in task order.
        let acc = |accs: &[(Task, UnitsMade<'p>)], task: &Task| -> Option<usize> { accs.binary_search_by_key(&task.planned.name, |(one, _)| one.planned.name).ok() };
        let mut worked = worked.into_iter();
        let mut cursor = 0usize;
        let mut busy = 0usize;
        for (at, (job, (setup, setup_ns))) in jobs.iter().zip(setups.iter()).enumerate() {
            // The setup's time shared by the crate's tasks, as a job's was.
            let each = u64::try_from(job.tasks.len()).unwrap_or(1).max(1);
            for task in job.tasks.iter() {
                if let Some(i) = acc(&accs, task) {
                    accs[i].1.ns = accs[i].1.ns.saturating_add(*setup_ns / each);
                }
            }
            match setup {
                Ok(Ok(_)) => {}
                // The scope's records not opened, or its lang index or listing refused
                // (`crate_setup`): a function of the graph's cell (the packs) and each scope
                // crate's names cell, unless a pack failed on the machine (owed).
                Ok(Err(why)) => {
                    let why = *why;
                    let owed = packs.refused().is_some();
                    let cells: &[CellId] = if owed { &[] } else { &job.cells };
                    for task in job.tasks.iter() {
                        if let Some(i) = acc(&accs, task) {
                            accs[i].1.fail(why, cells, owed);
                        }
                    }
                    continue;
                }
                // A panic is not a function of reads: owed, entered again each request (rule 20).
                Err(work::Panicked) => {
                    for task in job.tasks.iter() {
                        if let Some(i) = acc(&accs, task) {
                            accs[i].1.fail("its crate's head setup panicked; that crate's heads alone failed", &[], true);
                        }
                    }
                    continue;
                }
            }
            while let Some(split) = splits.get(cursor).filter(|split| split.job as usize == at) {
                cursor += 1;
                let Some((ran, ns)) = worked.next() else { break };
                let tasks = &job.tasks[split.from as usize..split.to as usize];
                let each = u64::try_from(tasks.len()).unwrap_or(1).max(1);
                for task in tasks.iter() {
                    if let Some(i) = acc(&accs, task) {
                        accs[i].1.ns = accs[i].1.ns.saturating_add(ns / each);
                    }
                }
                let made = match ran {
                    Ok(Ok(made)) => made,
                    // The split's records not opened again (`heads_of`): as the setup's.
                    Ok(Err(why)) => {
                        let owed = packs.refused().is_some();
                        let cells: &[CellId] = if owed { &[] } else { &job.cells };
                        for task in tasks.iter() {
                            if let Some(i) = acc(&accs, task) {
                                accs[i].1.fail(why, cells, owed);
                            }
                        }
                        continue;
                    }
                    // A panic fails its split's tasks alone, owed (rule 20): the crate's other
                    // splits, and every other crate's, stand.
                    Err(work::Panicked) => {
                        for task in tasks.iter() {
                            if let Some(i) = acc(&accs, task) {
                                accs[i].1.fail("a run of its crate's head tasks panicked; those tasks alone failed", &[], true);
                            }
                        }
                        continue;
                    }
                };
                if !made.is_empty() {
                    busy += 1;
                }
                // A stop refused the tasks from `ran` on before they began (`heads_of`): each owed
                // (no read), and the driver stops before the merge (engine_status.rs).
                let ran = made.len();
                for task in tasks.iter().skip(ran) {
                    if let Some(i) = acc(&accs, task) {
                        accs[i].1.fail(NOT_BEGUN, &[], true);
                    }
                }
                for (task, one) in tasks.iter().zip(made) {
                    let Some(i) = acc(&accs, task) else { continue };
                    // The tests' observer of one unit's read list (engine_heads_once_tests.rs:
                    // which task and unit hold the longest list, under each strategy).
                    #[cfg(test)]
                    crate::engine_heads_once::OnceCounts::observe_unit_reads(
                        task.planned.name,
                        job.prefix,
                        match &one {
                            Ok(head) => &head.reads,
                            Err(failed) => &failed.reads,
                        },
                    );
                    match one {
                        Ok(one) => accs[i].1.take(job.prefix, one),
                        Err(failed) => accs[i].1.fail(failed.why, &failed.reads, failed.incomplete_reads),
                    }
                }
            }
        }
        // Every split was merged (`in_split_order` answered each): one left is this function's
        // bug, named (rule 3).
        if cursor != splits.len() || worked.next().is_some() {
            return Err(CliError::Text { what: "the head split jobs' results were not each merged into their crate's tasks (engine_heads.rs `run_items`)" });
        }
        let mut heads_failed: Vec<crate::engine_head_notes::Refused, &Frame> = Vec::new_in(frame);
        let mut counts = UnitCounts::default();
        let mut units: SVec<'p, [u8; 8]> = SVec::new_in(at_phase);
        // The cells this phase's head tasks define, every unit's (a refused task defines none): a
        // head's read of another head of this phase is recorded at today's digest (lane BH1),
        // so the merge never sends it to a second wave for a cell it saw (blaze-engine
        // engine.rs `readers_of`). Since heads are frozen per unit (lane RU1) a task's cells
        // come from several jobs, so this is one table for the phase, not one a job.
        let today = crate::engine_scope_reads::job_cells(accs.iter().map(|(task, made)| (task.planned.name, made.first.as_ref().filter(|_| made.failed.is_none()).map(|one| &one.defines[..]))));
        require_producers(&mut accs, tasks, &today, view, at_phase, g);
        for (task, mut made) in accs.into_iter() {
            times.push(task.planned.name, made.ns);
            // One unit's failure fails the task whole: its row is refused on the union of what
            // each failing unit read (owed when one panicked).
            if let Some((why, cells, owed)) = made.failed {
                let cells: &[CellId] = if owed { &[] } else { &cells };
                refuse(&task, why, cells, buffer);
                continue;
            }
            // Each whole holding unit ran it and none failed, so one froze it (`heads_of` makes
            // one result a task); none is a bug of this function, said by name (rule 3).
            let Some(one) = made.first else {
                refuse(&task, "no unit that holds its file froze it, and none failed (engine_heads.rs `run_items`)", &made.reads, buffer);
                continue;
            };
            // Its record's payload: the runs, then the run facts ([`ctor_facts`]).
            let mut payload = one.payload;
            if let Err(why) = ctor_facts(one.ctors, one.adt_failed, &mut payload, at_phase) {
                refuse(&task, why, &made.reads, buffer);
                continue;
            }
            let mut defines: SVec<'a, (Cell, ContentId)> = SVec::with_capacity_in(one.defines.len(), alloc);
            defines.extend(one.defines.iter().copied());
            if defines.len() > 1 {
                defines.sort_unstable_by_key(|(cell, _)| *cell);
                defines.dedup_by_key(|(cell, _)| *cell);
            }
            // Deduplicate `CellId`s (48 B) BEFORE resolving digests into `(CellId, ContentId)`
            // (80 B) so multi-unit duplicates never query `today`/`view.cell` twice and `reads`
            // allocates exact deduplicated capacity in `alloc` already in sorted order.
            if made.reads.len() > 1 {
                made.reads.sort_unstable();
                made.reads.dedup();
            }
            let mut reads: SVec<'a, (CellId, ContentId)> = SVec::with_capacity_in(made.reads.len(), alloc);
            reads.extend(made.reads.iter().map(|id| (*id, crate::engine_scope_reads::today(&today, id).unwrap_or_else(|| view.cell(id)))));
            let unique_names = reads.chunk_by(|a, b| a.0.of == b.0.of).count();
            let mut record_reads: std::vec::Vec<(Name, ContentId), Global> = std::vec::Vec::with_capacity_in(unique_names, Global);
            for group in reads.chunk_by(|a, b| a.0.of == b.0.of) {
                if let Some((id, digest)) = group.first() {
                    record_reads.push((id.of, *digest));
                }
            }
            let name = task.planned.name;
            // Each head it could not freeze, under each unit: a head note a cell (a reader of
            // either unit's cell quotes its own), one finding a head by path and reason.
            let findings = if one.failed.is_empty() {
                0
            } else {
                heads_failed.extend(one.failed.iter().map(|&(cell, head, cause)| crate::engine_head_notes::Refused { name, cell: Some(cell), head, cause }));
                let mut said: SVec<'p, (&'static str, &'static str)> = SVec::with_capacity_in(one.failed.len(), at_phase);
                said.extend(one.failed.iter().map(|(_, head, cause)| (*head, cause.group)));
                if said.len() > 1 {
                    said.sort_unstable();
                    said.dedup();
                }
                u32::try_from(said.len()).unwrap_or(u32::MAX)
            };
            // The counts: each (item, unit) once; an item frozen under 2+ units counted shared.
            let mut items = one.items;
            if items.len() == 1
                && let Some(&(_, unit)) = items.first()
            {
                counts.frozen = counts.frozen.saturating_add(1);
                counts.items = counts.items.saturating_add(1);
                if !units.contains(&unit) {
                    units.push(unit);
                }
            } else if !items.is_empty() {
                items.sort_unstable();
                items.dedup();
                counts.frozen = counts.frozen.saturating_add(u32::try_from(items.len()).unwrap_or(u32::MAX));
                for run in items.chunk_by(|a, b| a.0 == b.0) {
                    counts.items = counts.items.saturating_add(1);
                    if run.len() > 1 {
                        counts.shared = counts.shared.saturating_add(1);
                    }
                }
                for &(_, unit) in items.iter() {
                    if !units.contains(&unit) {
                        units.push(unit);
                    }
                }
            }
            let out = made.out.finish();
            buffer.done.push(Done { name, key: task.key, out, defines, reads, findings, failed: false });
            self.made.push(Made { name, out, reads: record_reads, payload });
        }
        counts.units = u32::try_from(units.len()).unwrap_or(u32::MAX);
        // Summed over the request's Heads phases (the units: the most one phase froze under).
        // The jobs: summed over the phases, as the freezes (the most splits: the most of any).
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        self.units = UnitCounts {
            frozen: self.units.frozen.saturating_add(counts.frozen),
            items: self.units.items.saturating_add(counts.items),
            units: self.units.units.max(counts.units),
            shared: self.units.shared.saturating_add(counts.shared),
            tasks: self.units.tasks.saturating_add(count(total)),
            crate_jobs: self.units.crate_jobs.saturating_add(count(jobs.len())),
            split_jobs: self.units.split_jobs.saturating_add(count(splits.len())),
            busy: self.units.busy.saturating_add(count(busy)),
            busy_floor: self.units.busy_floor.saturating_add(count(workers.min(total))),
            most_splits: self.units.most_splits.max(count(most_splits)),
            split_files: self.units.split_files.saturating_add(count(split_files)),
            listings: listings.counts(),
        };
        self.refused.extend(heads_failed);
        self.made.sort_unstable_by_key(|one| one.name);
        Ok(closed_worked)
    }
}

/// What a head task not begun because of a stop is refused for (engine_status.rs; as
/// engine_bodies.rs `run_bodies` refuses a body).
pub(crate) const NOT_BEGUN: &str = "not checked: the command was stopped by a signal before this head task began (engine_status.rs)";

/// Where a job's head tasks say they run (engine_status.rs `Board`): each task started and done
/// on the board by the one unit that shows it (`run_items`'s `shows`, by (name, prefix)), and
/// the stop asked before each task, in every unit.
#[derive(Clone, Copy)]
struct OnBoard<'b> {
    board: &'b crate::engine_status::Board<'b>,
    clock: &'b bstd::clock::Clock,
    shows: &'b [(Name, [u8; 8])],
}

impl OnBoard<'_> {
    /// Whether this unit shows task `name` on the board.
    fn shows(&self, name: Name, prefix: [u8; 8]) -> bool {
        self.shows.binary_search(&(name, prefix)).is_ok()
    }
}

/// A head task's label on the board: its entry's name as its text shows it, else the task.
fn board_label(entry_name: Option<&str>, name: Name) -> crate::engine_status::Label {
    match entry_name {
        Some(shown) if !shown.is_empty() => crate::engine_status::Label::of(shown),
        _ => crate::engine_status::Label::debug(&name),
    }
}

/// One split job's place in its crate's (file, name) order and its estimated work weight
/// (`sum(1 + heads + calls)`) for LPT dispatch.
#[derive(Clone, Copy, Debug)]
struct SplitAt {
    job: u32,
    from: u32,
    to: u32,
    weight: u64,
}

/// What a crate's split jobs share, made once a crate a phase by [`crate_setup`], each list
/// in phase memory so every split reads it in place on any worker and the phase's drop unmaps
/// every byte (zero `.leak_static()`). What cannot be shared: the records themselves (a reader
/// is one thread's, front2 source.rs `Records`), so a split opens them again over the same bytes
/// and borrows the setup's lang index (what building it reads is `lang_reads`, read here once).
struct CrateSetup<'p> {
    sized: Option<blaze_id::ItemKey>,
    /// The program's receiver pointer types (`Pointers::find`, once a crate).
    pointers: blaze_ladder::front2::receiver::Pointers,
    /// What the lang index read, each coarse (engine_heads_once.rs `JobCx::lang_reads`).
    lang_reads: SVec<'p, CellId>,
    /// What finding the pointers read, with `lang_reads` (engine_heads_once.rs `JobCx::pointer_reads`).
    pointer_reads: SVec<'p, CellId>,
    /// The scope names of the job's crates that are members, sorted.
    members: SVec<'p, Name>,
    /// The crate's listing, from the request's table.
    listing: &'static Listing,
    /// Owned lang-item index built once per crate setup and borrowed by every split job in
    /// `heads_of` with zero `Global` allocation and zero repeated `ItemsRecord` scans.
    index: blaze_ladder::front2::index::IndexIn,
    /// Per-task work weights (`1 + heads + calls`) computed in parallel on the setup worker.
    task_weights: SVec<'p, usize>,
    /// Sum of `task_weights`.
    total_weight: usize,
}

#[derive(Debug, Clone, Copy)]
struct OpenScopeRefused;

impl OpenScopeRefused {
    const fn message(self) -> &'static str {
        "its crate's records could not be opened (run_names.rs `names_records`)"
    }
}

impl core::fmt::Display for OpenScopeRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.message())
    }
}

/// Lazy `CrateEnv` table for head constant evaluation (`include_str!`/`include_bytes!`/`env!`):
/// builds `row_env` on demand for any crate in the split's scope that has no build script.
struct HeadEnvs<'n> {
    list: &'n blaze_world::crate_list::CrateList<'static>,
    own: usize,
    rest: &'n [crate::front_load::Unopened],
    frame: &'n Frame,
    made: Vec<core::cell::OnceCell<Option<&'n blaze_scope::crate_env::CrateEnv<'n>>>, &'n Frame>,
}

impl<'n> HeadEnvs<'n> {
    fn new(
        list: &'n blaze_world::crate_list::CrateList<'static>,
        own: usize,
        rest: &'n [crate::front_load::Unopened],
        frame: &'n Frame,
    ) -> HeadEnvs<'n> {
        let mut made = Vec::with_capacity_in(1 + rest.len(), frame);
        made.resize_with(1 + rest.len(), core::cell::OnceCell::new);
        HeadEnvs { list, own, rest, frame, made }
    }
}

impl<'n> crate::job_texts::EnvsByNeed<'n> for HeadEnvs<'n> {
    fn env_of(&self, krate: usize) -> Option<&'n blaze_scope::crate_env::CrateEnv<'n>> {
        let slot = self.made.get(krate)?;
        let world_at = if krate == 0 { self.own } else { self.rest.get(krate - 1)?.world_at };
        *slot.get_or_init(|| {
            if matches!(crate::body_types::owner_script(self.list, world_at), Ok(None)) {
                crate::body_types::row_env(self.list, world_at, None, None, self.frame).ok()
            } else {
                None
            }
        })
    }
}

/// A job's scope's records opened by need in `frame` (run_names.rs `names_records_over`): what
/// the setup, each split, and each impl tables job open, the one place a head job opens them.
#[expect(clippy::too_many_arguments, reason = "the names records' edge: each argument one input of them")]
fn open_scope_by_need<'n>(
    cx: &'n Cx,
    root: &'n bstd::fs::Dir<bstd::fs::ReadWrite>,
    fs: &'n bstd::fs::Fs,
    outlined: &'n [Outlined<'n>],
    crates: crate::run_names::ScopeCrates<'_, 'n>,
    packs: &'static crate::body_types::SyncLazyTrees<'static>,
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    envs: crate::run_names::Envs<'n>,
    frame: &'n Frame,
    g: Alloc<'static>,
    snapshot: &'n crate::store::StoreSnapshot,
) -> Result<crate::run_names::Opened<'n>, OpenScopeRefused> {
    let packs = ((packs, fs), Some(outlined));
    crate::run_names::names_records_envs(cx, root, crates, packs, corrected, crate::job_texts::Procs::of(procs), std::vec::Vec::new_in(Global), envs, Some(snapshot), None, frame, g)
        .map_err(|_| OpenScopeRefused)
}

/// One member crate's setup for its head tasks, on a worker, once a crate a phase: its records
/// opened in the job's frame, its lang rows, the `Sized` item, its lang index's reads, its
/// receiver pointers and their reads, its member crates, and its listing from the request's
/// table (made here when no job made it yet: [`Listings::listing`]).
#[expect(clippy::too_many_arguments, reason = "a worker's edge: each argument one handle of it")]
fn crate_setup<'p>(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    fs: &bstd::fs::Fs,
    outlined: &[Outlined<'_>],
    packs: &'static crate::body_types::SyncLazyTrees<'static>,
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    job: &ItemJob<'_, '_>,
    scope_records: &dyn crate::front_load::OpenByNeed,
    subjects: &Subjects,
    view: View<'_, '_>,
    heap: &'static blaze_own::Heap,
    listings: &Listings,
    snapshot: &crate::store::StoreSnapshot,
    at_phase: SharedAlloc<'p>,
) -> Result<CrateSetup<'p>, &'static str> {
    let frame = cx.frame();
    let g = heap.alloc();
    let opened = open_scope_by_need(cx, root, fs, outlined, job.scope_crates(scope_records), packs, corrected, procs, crate::run_names::Envs { owner: None, rest: None }, frame, g, snapshot).map_err(OpenScopeRefused::message)?;
    let records = opened.records;
    let lang = job.lang.as_slice();
    let mut sized = None;
    for at in lang.iter() {
        NamesSource::each_lang(records, *at, &mut |name, key| {
            if name == "sized" {
                sized = Some(key);
            }
        });
    }
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let fa = bump.alloc();
    let unused = Recorder::new(RecorderOpts { alloc: fa });
    let base = Tables::new(TablesOpts { records, impls: None, lang_crates: lang, reads: &unused });
    // The constant evaluator's lang index; what it read is read by every head that evaluates
    // (engine_head_consts.rs).
    let lang_recorder = Recorder::new(RecorderOpts { alloc: fa });
    let index_in = blaze_ladder::front2::index::IndexIn::from_source_in(base.reading_into(&lang_recorder), lang, heap.shared())
        .map_err(|_| "two crates of its scope declare one lang item, so its constants have no index (front2 index.rs `LangTwice`)")?;
    let index = index_in.borrow();
    // The scope names of the job's crates that are members, sorted: every other crate's names
    // reads are kept at its record's `WHOLE` (engine_heads_once.rs `coarse`).
    let mut members: SVec<'p, Name> = SVec::new_in(at_phase);
    members.extend(
        core::iter::once(job.own.records.key)
            .chain(job.rest.iter().map(|one| one.key))
            .filter(|key| subjects.binary_search_by(|row| row.prefix.cmp(&key.prefix())).is_ok())
            .map(|key| blaze_drive::key::crate_scope_name(&key)),
    );
    members.sort_unstable();
    members.dedup();
    let setup_cell = |read: blaze_ladder::front2::tables::recorder::TableRead| -> Result<Option<CellId>, &'static str> {
        let id = if read.is_source() {
            crate::engine_head_consts::source_read(records, subjects, read).map_err(|unplaced| {
                let labels = scope_labels(job.own, &job.rest, frame);
                let shown = crate::engine_head_consts::UnplacedIn { unplaced: &unplaced, crates: labels };
                crate::engine_head_notes::leak(g, format_args!("placing a source read of the lang index or receiver pointer types: {shown}"))
            })?
        } else {
            cell_id(base, read).map_err(|why| crate::engine_head_notes::leak(g, format_args!("placing a read of the lang index or receiver pointer types: {why}")))?
        };
        Ok(id.map(|id| crate::engine_heads_once::coarse(&members, names_read(view, id))))
    };
    let mut lang_reads: SVec<'p, CellId> = SVec::new_in(at_phase);
    for one in lang_recorder.finish().map_err(|_| "its lang index read more names cells than the recorder holds (front2 tables/recorder.rs)")?.iter() {
        lang_reads.extend(setup_cell(*one)?);
    }
    lang_reads.sort_unstable();
    lang_reads.dedup();
    // The program's receiver pointer types (`Box`, `Rc`, `Arc`, `Pin`), once a crate as front2
    // project.rs `ProjectWorld::new` finds them: without them every struct receiver
    // (`self: Pin<&mut Self>`) is a gap and `Future`'s and `Stream`'s dyn-compatibility verdicts
    // are `Gap` (lane FL2's finding, fixed by lane H9). What finding them read beyond the lang
    // index (alloc's `crate::rc::Rc` and `crate::sync::Arc` resolved from `Box`'s module) is
    // every cell's read (engine_heads_once.rs `JobCx::pointer_reads`); the lang rows they take are
    // `lang_reads`', which every cell then reads too.
    let pointer_recorder = Recorder::new(RecorderOpts { alloc: fa });
    let pointers = blaze_ladder::front2::receiver::Pointers::find(&index, blaze_ladder::resolve::Paths { names: base.reading_into(&pointer_recorder) });
    let mut pointer_reads: SVec<'p, CellId> = SVec::with_capacity_in(lang_reads.len(), at_phase);
    pointer_reads.extend_from_slice(&lang_reads);
    for one in pointer_recorder.finish().map_err(|_| "finding its receiver pointer types read more names cells than the recorder holds (front2 tables/recorder.rs)")?.iter() {
        pointer_reads.extend(setup_cell(*one)?);
    }
    pointer_reads.sort_unstable();
    pointer_reads.dedup();
    // The crate at 0 is this job's unit (run_names.rs `names_records` opens the job's own first).
    let crate_key = NamesSource::crate_key(records, 0).ok_or("its crate has no key in its own records (front2 source.rs `crate_key`)")?;
    if crate_key.prefix() != job.prefix {
        return Err(NOT_OWN_FIRST);
    }
    let over = (job.own.records.key == crate_key).then_some(&job.own.records).ok_or("its crate is not the first of the crates its records were opened over (run_names.rs `names_records`)")?;
    let listing = listings.listing(records, over, 0, crate_key, subjects)?;
    if let Some((_, why)) = opened.load.refused() {
        return Err(why);
    }
    if let Some(why) = packs.refused() {
        return Err(why);
    }
    // Weight each Signature task by `1 + heads + calls` (from `listing`) in parallel on this
    // worker so the coordinator never does O(tasks * log(heads)) binary searches serially.
    let mut task_weights: SVec<'p, usize> = SVec::with_capacity_in(job.tasks.len(), at_phase);
    let mut total_weight = 0usize;
    for task in job.tasks.iter() {
        let name = task.planned.name;
        let w = 1usize
            .saturating_add(listing.of_entry(name).len())
            .saturating_add(listing.calls_of(name).len());
        task_weights.push(w);
        total_weight = total_weight.saturating_add(w);
    }
    Ok(CrateSetup { sized, pointers, lang_reads, pointer_reads, members, listing, index: index_in, task_weights, total_weight })
}

/// Reset threshold for `heads_of`'s per-split bump scratch frame: once a batch's scratch
/// stores, `OwnedRecorder`s, and `SeededHeads` tables pass this mark, the batch finishes,
/// drops them cleanly, and resets the frame (trimming committed pages back to
/// [`HEADS_SCRATCH_KEEP`]). Both scale inversely past [`HEADS_FRAME_CAP_BASE_WORKERS`] cores so
/// 64–128 core machines stay within a fixed aggregate frame budget while 8–18 core Macs keep full
/// working-set caps.
const HEADS_SCRATCH_RESET_AT: usize = 32 << 20;
const HEADS_SCRATCH_KEEP: usize = 8 << 20;
const HEADS_FRAME_CAP_BASE_WORKERS: usize = 18;
const HEADS_SCRATCH_RESET_MIN: usize = 4 << 20;
const HEADS_SCRATCH_KEEP_MIN: usize = 1 << 20;

/// Returns `(reset_at, keep)` for a `heads_of` worker's scratch frame scaled inversely with
/// active worker count.
const fn heads_scratch_caps(workers: usize) -> (usize, usize) {
    let w = if workers == 0 { 1 } else { workers };
    if w <= HEADS_FRAME_CAP_BASE_WORKERS {
        return (HEADS_SCRATCH_RESET_AT, HEADS_SCRATCH_KEEP);
    }
    let reset = HEADS_SCRATCH_RESET_AT.saturating_mul(HEADS_FRAME_CAP_BASE_WORKERS) / w;
    let keep = HEADS_SCRATCH_KEEP.saturating_mul(HEADS_FRAME_CAP_BASE_WORKERS) / w;
    (
        if reset < HEADS_SCRATCH_RESET_MIN { HEADS_SCRATCH_RESET_MIN } else { reset },
        if keep < HEADS_SCRATCH_KEEP_MIN { HEADS_SCRATCH_KEEP_MIN } else { keep },
    )
}

/// One run of a member crate's head tasks (`tasks`, a split of `job`'s: [`split_count`]) on a
/// worker: its scope's records opened again in the job's frame over the crate's `setup`, each
/// task's heads frozen (front2 tables/head.rs `head_task`) through the split's one source, so
/// each cell's read list is its own: every names cell its freeze consulted (the recorder), and
/// for each head its freeze read, the cell of the head task that covers it (a same-stage read:
/// the engine enters the reader at the wave after its definer, engine.rs), or its crate's names
/// record. A panic here fails this split's tasks alone (`run_items`).
#[expect(clippy::too_many_arguments, reason = "a worker's edge: each argument one handle of it")]
fn heads_of<'p>(
    cx: &Cx,
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    fs: &bstd::fs::Fs,
    outlined: &[Outlined<'_>],
    packs: &'static crate::body_types::SyncLazyTrees<'static>,
    corrected: &'static [(u32, &'static str, &'static str)],
    procs: Option<&'static crate::proc_runner::ProcRunner<'static>>,
    list: &blaze_world::crate_list::CrateList<'static>,
    job: &ItemJob<'_, '_>,
    scope_records: &dyn crate::front_load::OpenByNeed,
    setup: &CrateSetup<'p>,
    tasks: &[Task],
    subjects: &Subjects,
    view: View<'_, '_>,
    expansion_reads: bool,
    heap: &'static blaze_own::Heap,
    once: &crate::engine_heads_once::HeadsOnce,
    listings: &Listings,
    plant: ListingsPlant,
    on_board: OnBoard<'_>,
    snapshot: &crate::store::StoreSnapshot,
    at_phase: SharedAlloc<'p>,
    max_workers: usize,
) -> Result<SVec<'p, Result<WorkedHead<'p>, HeadFailed<'p>>>, &'static str> {
    let frame = cx.frame();
    let g = heap.alloc();
    let scope_crates = job.scope_crates(scope_records);
    let head_envs = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(HeadEnvs::new(list, job.own.world_at, &job.rest, frame), frame));
    let envs = crate::run_names::Envs { owner: crate::job_texts::EnvsByNeed::env_of(head_envs, 0), rest: Some(head_envs) };
    let opened = open_scope_by_need(cx, root, fs, outlined, scope_crates, packs, corrected, procs, envs, frame, g, snapshot).map_err(OpenScopeRefused::message)?;
    let records = opened.records;
    let body_sources: Option<&dyn blaze_ladder::front2::body_host::BodySources> = Some(opened.sources);
    let CrateSetup { sized, pointers, ref lang_reads, ref pointer_reads, ref members, listing, ref index, .. } = *setup;
    let lang = job.lang.as_slice();
    let lang_reads = lang_reads.as_slice();
    let pointer_reads = pointer_reads.as_slice();
    let members = members.as_slice();
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let fa = bump.alloc();
    let unused = Recorder::new(RecorderOpts { alloc: fa });
    let base = Tables::new(TablesOpts { records, impls: None, lang_crates: lang, reads: &unused });
    // Borrow the crate setup's owned lang index with zero Global allocation and zero re-scan.
    let index = index.borrow();
    let crate_key = NamesSource::crate_key(records, 0).ok_or("its crate has no key in its own records (front2 source.rs `crate_key`)")?;
    let over = (job.own.records.key == crate_key).then_some(&job.own.records).ok_or("its crate is not the first of the crates its records were opened over (run_names.rs `names_records`)")?;
    let scope_name = blaze_drive::key::crate_scope_name(&crate_key);
    let labels = scope_labels(job.own, &job.rest, frame);
    let mut crate_scopes: blaze_own::Vec<crate::engine_heads_once::ScopeCrate, Alloc<'_>> =
        blaze_own::Vec::with_capacity_in(NamesSource::crate_count(records), fa);
    for ci in 0..NamesSource::crate_count(records) {
        let Some(key) = NamesSource::crate_key(records, ci) else { continue };
        let scope = blaze_drive::key::crate_scope_name(&key);
        let is_member = members.binary_search(&scope).is_ok();
        let has_cells = is_member && view.cell(&CellId { of: scope, cell: CELLS }) != blaze_drive::key::ABSENT;
        crate_scopes.push(crate::engine_heads_once::ScopeCrate { prefix: key.prefix(), scope, is_member, has_cells });
    }
    crate_scopes.sort_unstable_by_key(|c| c.prefix);
    // Reusable per-worker bump scratch frame for `SeededHeads`, `OwnedRecorder`s, and `Owners`:
    // reset and trimmed between batches so freezing scratch never touches `Global` (rule 11).
    let active_workers = usize::try_from(cx.workers()).unwrap_or(max_workers).min(max_workers).max(1);
    let (scratch_reset_at, scratch_keep) = heads_scratch_caps(active_workers);
    let mut scratch_frame = blaze_ladder::front2::tables::head::FreezeScratch::new(HEADS_FRAME, scratch_keep, bstd::mem::Tag::Infer)
        .map_err(|_| "could not reserve heads scratch frame (engine_heads.rs `heads_of`)")?;
    let mut done = SVec::with_capacity_in(tasks.len(), at_phase);
    let mut total_lowered = 0u32;
    let mut total_eval_lowered = 0u32;
    let mut total_taken = 0u32;
    struct ActiveTask<'b, 'f, 'p> {
        mine: &'static [ListedHead],
        at_head: usize,
        running: Option<crate::engine_status::Running<'b>>,
        out: blaze_ids::Hasher,
        worked: WorkedHead<'p>,
        failed: Option<&'static str>,
        incomplete_reads: bool,
        settled: usize,
        #[cfg(test)]
        own_block_reads: Vec<CellId, Alloc<'f>>,
        #[cfg(not(test))]
        _frame: core::marker::PhantomData<&'f ()>,
    }
    let mut active: Option<ActiveTask<'_, '_, 'p>> = None;
    let mut at_task = 0usize;
    while at_task < tasks.len() {
        let mut stop_now = false;
        let batch_res: Result<(), &'static str> = scratch_frame.with_batch_alloc(|scratch_alloc, batch_frame| {
            let mut owners = Owners::new(subjects, scope_crates, listings, scratch_alloc);
            // ONE SOURCE A BATCH over the request's table (engine_heads_once.rs): every names read
            // by the cell it is made for, so each cell's reads are its own.
            let owned = || blaze_ladder::front2::tables::owned_reads::OwnedRecorder::new(blaze_ladder::front2::tables::owned_reads::OwnedRecorderOpts { alloc: scratch_alloc, owning: blaze_ladder::front2::tables::owned_reads::Owning::Each });
            let (freeze_names, eval_names) = (owned(), owned());
            let both = blaze_ladder::front2::tables::owned_reads::BothOwners { names: &freeze_names, evals: &eval_names };
            let shared = base.reading_into(&freeze_names);
            let mut once_consts = crate::engine_heads_once::OnceConsts::new(base.reading_into(&eval_names), &index, bump, once, job.prefix).with_sources(body_sources);
            let mut source = SeededHeads::new(SeededHeadsOpts { names: shared, sized, pointers, frame, scratch: scratch_alloc, alloc: scratch_alloc, consts: Some(&mut once_consts as &mut dyn ConstValues<'_>), known: Some(once), owners: Some(&both) });
            let job_cx = crate::engine_heads_once::JobCx {
                base,
                shared,
                view,
                records,
                subjects,
                crates: labels,
                index: &index,
                lang_reads,
                sized,
                pointers,
                pointer_reads,
                frame,
                g,
                once,
                names: &freeze_names,
                evals: &eval_names,
                engine_refusal: core::cell::Cell::new(None),
                members,
                crate_scopes: &crate_scopes,
                sources: body_sources,
                texts: opened.sources.0,
            };
            'batch: while at_task < tasks.len() {
                let Some(task) = tasks.get(at_task) else { break };
                let mut cur = if let Some(cur) = active.take() {
                    cur
                } else {
                    let name = task.planned.name;
                    // The workers' scheduling point (engine_status.rs): stopped, this task and the rest are
                    // not begun; `run_items` refuses them (owed) and the driver stops before the merge.
                    if on_board.board.stopping().is_some() {
                        let rest = tasks.get(at_task..).unwrap_or(&[]).iter().filter(|one| on_board.shows(one.planned.name, job.prefix)).count();
                        on_board.board.refused(Stage::Heads, u64::try_from(rest).unwrap_or(u64::MAX));
                        stop_now = true;
                        break;
                    }
                    // The crate's listing: the setup's, or under the plant one made for this task alone
                    // (the control of engine_owners_once_tests.rs).
                    let listing = if plant == ListingsPlant::PerAsk { listings.listing(records, over, 0, crate_key, subjects)? } else { listing };
                    let mine = listing.of_entry(name);
                    // Shown on the board by the one unit that shows it, labelled by its entry's name.
                    let running = on_board.shows(name, job.prefix).then(|| {
                        let entry_name = mine.first().map(|one| one.label.entry_name);
                        on_board.board.show(Stage::Heads, board_label(entry_name, name), u64::try_from(frame.used()).unwrap_or(u64::MAX), on_board.board.now(on_board.clock))
                    });
                    let out = blaze_ids::Hasher::new(HEAD_OUT_DOMAIN);
                    let mut worked = WorkedHead {
                        out: ContentId([0; 32]),
                        defines: SVec::new_in(at_phase),
                        items: SVec::new_in(at_phase),
                        reads: SVec::new_in(at_phase),
                        payload: std::vec::Vec::new_in(Global),
                        failed: SVec::new_in(at_phase),
                        ctors: SVec::new_in(at_phase),
                        adt_failed: false,
                    };
                    // When this file is held by this unit yet this entry produced no head (e.g., cut by
                    // `#[cfg]` on itself or an enclosing inline item), read the unit's `CfgAtoms` cell so a
                    // crate-level cfg/feature change wakes the entry without waking it on unrelated edits.
                    if mine.is_empty() {
                        worked.reads.push(names_read(
                            view,
                            CellId { of: scope_name, cell: Cell(blaze_scope::names_cells::NameCell::CfgAtoms { krate: job.prefix }.key()) },
                        ));
                    }
                    // A head task whose entry body holds items owns any block impls first written there.
                    // Its signature source intentionally ignores body tokens, so read each body's deterministic
                    // block-module expansion cell (`NameCell::Expansion { call: block_key(crate_key, body) }`):
                    // absent until the first block module appears, changing when one is added or edited, and
                    // completely unaffected by sibling items added elsewhere in the crate.
                    #[cfg(test)]
                    let mut own_block_reads: Vec<CellId, Alloc<'_>> = Vec::new_in(fa);
                    if mine.iter().any(|head| head.body_holds_items) {
                        let mut containers: Vec<blaze_id::ItemKey, Alloc<'static>> = Vec::new_in(scratch_alloc);
                        let mut pushed_any = false;
                        let mut push_block = |body: blaze_id::ItemKey| {
                            let call = crate::body_types::covered::block_key(crate_key, body);
                            let id = names_read(view, CellId { of: scope_name, cell: Cell(blaze_scope::names_cells::NameCell::Expansion { call }.key()) });
                            worked.reads.push(id);
                            #[cfg(test)]
                            own_block_reads.push(id);
                            pushed_any = true;
                        };
                        for head in mine.iter().filter(|head| head.body_holds_items) {
                            match head.want {
                                Want::Fn(lib) => push_block(lib),
                                Want::Impl(lib) | Want::Members(lib) | Want::MemberFn(lib, _) => {
                                    if containers.scan_for(&lib).is_none() {
                                        containers.push(lib);
                                        records.each_member_row(lib, &mut |m| push_block(m.key));
                                    }
                                }
                                Want::Head(_) | Want::Adt(_) => {}
                            }
                        }
                        if !pushed_any {
                            let id = crate::engine_body_plan::not_held_cell(view, scope_name, job.prefix);
                            worked.reads.push(id);
                            #[cfg(test)]
                            own_block_reads.push(id);
                        }
                    }
                    let mut failed: Option<&'static str> = None;
                    // Two heads under one cell would be two values under one key (rule 21): the whole
                    // entry is refused, the two heads named as its text names them.
                    if let Some(two) = mine.windows(2).find(|two| two.first().zip(two.get(1)).is_some_and(|(a, b)| a.cell == b.cell))
                        && let (Some(first), Some(second)) = (two.first(), two.get(1))
                    {
                        let (one, other) = (first.label, second.label);
                        failed = Some(crate::engine_head_notes::leak(g, format_args!("two heads of its entry fall under one cell: {one} and {other}, the same kind and name (engine_heads.rs `head_cell`)")));
                    }
                    // Track the expansion cell even when the call has no head yet. A change that adds
                    // its first head must rerun this entry. Freezing a head requests its text separately.
                    for (_, call) in listing.calls_of(name).iter() {
                        if !expansion_reads {
                            break;
                        }
                        worked.reads.push(names_read(view, CellId { of: scope_name, cell: Cell(blaze_scope::names_cells::NameCell::Expansion { call: *call }.key()) }));
                    }
                    let mine = if failed.is_some() { &mine[..0] } else { mine };
                    ActiveTask {
                        mine,
                        at_head: 0,
                        running,
                        out,
                        worked,
                        failed,
                        incomplete_reads: false,
                        settled: 0,
                        #[cfg(test)]
                        own_block_reads,
                        #[cfg(not(test))]
                        _frame: core::marker::PhantomData,
                    }
                };
                while cur.at_head < cur.mine.len() {
                    let Some(head) = cur.mine.get(cur.at_head) else { break };
                    let (cell, want) = (&head.cell, &head.want);
                    cur.worked.items.push((head.item, job.prefix));
                    // An ADT's constructors, whether its head freezes or not: a failed head's are listed
                    // as not run ([`ctor_facts`]), never left without a row's word.
                    if let Want::Adt(lib) = *want {
                        ctors_of(records, lib, &mut cur.worked.ctors);
                    }
                    // Frozen through the split's one source, its reads its cells' own (engine_heads_once.rs
                    // `one_head`; a head that does not freeze is frozen again alone, as before).
                    let made = crate::engine_heads_once::one_head(&job_cx, &mut source, &mut owners, *want, task, &mut |id| cur.worked.reads.push(id), &mut cur.failed);
                    cur.incomplete_reads |= cur.failed.is_some();
                    cur.at_head += 1;
                    if let Some(why) = job_cx.engine_refusal.get() {
                        cur.failed.get_or_insert(why);
                        cur.incomplete_reads = true;
                        break;
                    }
                    // Each cell once so far, whenever the list has doubled since it last was.
                    if cur.worked.reads.len() > cur.settled.saturating_mul(2).max(SETTLE_AT) {
                        once_each(&mut cur.worked.reads);
                        cur.settled = cur.worked.reads.len();
                    }
                    let cell = *cell;
                    // A head not frozen fails alone (module docs): its cell the digest of its reason.
                    match made {
                        Ok(made) => {
                            cur.worked.defines.push((cell, made.out));
                            cur.out.update(&cell.0).update(&made.out.0);
                            // The cell too: a body finds a member's head by it (engine_seeds.rs `StretchHeads`).
                            let wb = want_bytes(*want);
                            let entry_bytes = wb.len().saturating_add(16).saturating_add(4).saturating_add(made.bytes.len());
                            if cur.worked.payload.is_empty() && cur.mine.len() == 1 {
                                // Single-head entry (common case): reserve exact entry bytes + trailer room
                                // so `payload` takes one `Global` allocation instead of doubling 4-5 times.
                                cur.worked.payload.reserve_exact(entry_bytes.saturating_add(24));
                            } else {
                                cur.worked.payload.reserve(entry_bytes);
                            }
                            cur.worked.payload.extend_from_slice(&wb);
                            cur.worked.payload.extend_from_slice(&cell.0);
                            cur.worked.payload.extend_from_slice(&u32::try_from(made.bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
                            cur.worked.payload.extend_from_slice(&made.bytes);
                        }
                        Err(cause) => {
                            let head = crate::engine_head_consts::head_label(records, *want, g);
                            let digest = cause.digest(HEAD_FAILED_DOMAIN);
                            cur.worked.defines.push((cell, digest));
                            cur.out.update(&cell.0).update(&digest.0);
                            cur.worked.failed.push((cell, head, cause));
                            cur.worked.adt_failed |= matches!(want, Want::Adt(_));
                        }
                    }
                    // Large multi-head entries (e.g. `sol!` macro expansions with hundreds of items)
                    // can exhaust the per-split bump scratch frame within a single task if we wait
                    // until the end of the task to reset. Yield mid-task so `scratch_frame` resets
                    // and trims cleanly between heads while preserving `cur` across batches.
                    if cur.at_head < cur.mine.len() && batch_frame.used() >= scratch_reset_at {
                        if let Some(why) = crate::engine_heads_once::job_refusal(&freeze_names, &eval_names, g) {
                            cur.failed.get_or_insert(why);
                            cur.incomplete_reads = true;
                        }
                        active = Some(cur);
                        break 'batch;
                    }
                }
                // A read offered with no owner, or past a cap, fails the task by name (rule 3).
                if let Some(why) = crate::engine_heads_once::job_refusal(&freeze_names, &eval_names, g) {
                    cur.failed.get_or_insert(why);
                    cur.incomplete_reads = true;
                }
                if let Some((_, why)) = opened.load.refused() {
                    cur.failed.get_or_insert(why);
                    cur.incomplete_reads = true;
                }
                if let Some(why) = packs.refused() {
                    cur.failed.get_or_insert(why);
                    cur.incomplete_reads = true;
                }
                // What the phase keeps of this task until its row is written: each cell once, in a
                // block of that size.
                if cur.failed.is_some() {
                    // Even a refusal before the first freeze depends on its member's names record.
                    cur.worked.reads.push(job.cells[1]);
                }
                #[cfg(test)]
                match plant {
                    ListingsPlant::WholeItemFile => {
                        for id in &mut cur.worked.reads {
                            if id.of.kind == Kind::FileItems && id.cell != crate::engine_tasks::SURFACE && id.cell != crate::engine_tasks::HEADS {
                                id.cell = WHOLE;
                            }
                        }
                    }
                    ListingsPlant::DropItemText(missing) => cur.worked.reads.retain(|id| *id != missing),
                    ListingsPlant::DropOwnNames => cur.worked.reads.retain(|id| cur.own_block_reads.scan_for(id).is_none()),
                    _ => {}
                }
                once_each(&mut cur.worked.reads);
                cur.worked.reads.shrink_to_fit();
                once.read_list(&cur.worked.reads);
                done.push(match cur.failed {
                    Some(why) => Err(HeadFailed { why, reads: cur.worked.reads, incomplete_reads: cur.incomplete_reads }),
                    None => {
                        cur.worked.out = cur.out.finish();
                        Ok(cur.worked)
                    }
                });
                if let Some(running) = cur.running {
                    running.end(on_board.board.now(on_board.clock));
                }
                at_task += 1;
                if batch_frame.used() >= scratch_reset_at {
                    break;
                }
            }
            total_lowered = total_lowered.saturating_add(source.lowered);
            total_eval_lowered = total_eval_lowered.saturating_add(source.eval_lowered);
            total_taken = total_taken.saturating_add(source.taken);
            Ok(())
        });
        batch_res?;
        if stop_now {
            break;
        }
    }
    // Every lowering the source made, its constant evaluator's too (front2 head.rs
    // `Frozen::eval_lowered`): the work the stage did.
    once.job_done(total_lowered.saturating_add(total_eval_lowered), total_taken);
    Ok(done)
}

/// The most heads one head's freeze is followed through ([`heads_of`], rule 18): past it the
/// task fails by name.
pub(crate) const MOST_HEAD_READS: usize = 1 << 12;

/// A task's reads so far, each cell once. A read list is a set (`run_items` sorts and dedups
/// its row's; a refusal's goes through `failed_with`), and a head's freeze pushes one entry a
/// names read: thousands, which land on a few records' `WHOLE` cells (`names_read`). Left as
/// pushed, the lists of a phase's tasks were 4.6 GB of the 9.0 GB live four minutes into a real
/// workspace's check (tempo, 2026-11-17: 1125 lists of about 4 MB each, growing until the
/// memory guard stopped the run).
fn once_each<A: core::alloc::Allocator>(reads: &mut Vec<CellId, A>) {
    reads.sort_unstable();
    reads.dedup();
}

/// A read list is settled ([`once_each`]) when it passes this many entries, and then each time
/// it doubles, so the sorting stays linear in what was pushed.
const SETTLE_AT: usize = 1 << 10;

#[cfg(test)]
mod heads_frame_caps_tests {
    use super::*;

    #[test]
    fn heads_scratch_caps_scale_inversely_for_128_cores_and_preserve_mac_caps() {
        for w in [0usize, 1, 8, 12, 18] {
            assert_eq!(heads_scratch_caps(w), (HEADS_SCRATCH_RESET_AT, HEADS_SCRATCH_KEEP));
        }
        let (r64, k64) = heads_scratch_caps(64);
        assert!(r64 < HEADS_SCRATCH_RESET_AT && r64 >= HEADS_SCRATCH_RESET_MIN);
        assert!(k64 < HEADS_SCRATCH_KEEP && k64 >= HEADS_SCRATCH_KEEP_MIN);

        let (r128, k128) = heads_scratch_caps(128);
        assert!(r128 <= r64 && r128 >= HEADS_SCRATCH_RESET_MIN);
        assert!(k128 <= k64 && k128 >= HEADS_SCRATCH_KEEP_MIN);
        assert!(128usize.saturating_mul(k128) <= 160 << 20, "128-core heads scratch keep <= 160 MiB, got {}", 128usize.saturating_mul(k128));
    }

    #[test]
    fn direct_file_placement_matches_outline_listing_byte_for_byte() {
        let heap = blaze_own::testing::heap();
        let text = r#"
pub fn f_0(x: u64) -> u64 {
    struct Local;
    impl Local { fn hidden() {} }
    fn inner() {}
    x
}
pub struct Pair(u32);
pub trait Measure { type Out; fn m(&self) -> u32; }
impl Pair {
    pub const K: u32 = 1;
    pub fn a(&self) -> u32 { 1 }
}
impl Measure for Pair {
    type Out = u32;
    fn m(&self) -> u32 { 2 }
}
unsafe extern "C" { fn ext_f(x: i32) -> i32; }
mod nested {
    pub type Alias = u64;
    pub static S: u32 = 0;
    pub const C: u32 = 1;
    pub enum E { A }
}
"#;
        let outline = blaze_file_items::outline(text, blaze_surface::Edition::E2024, heap.alloc());
        let bytes = outline.to_file_items(text, ContentId::of("direct place test", text.as_bytes()), heap.alloc()).expect("outline serializes");
        let view = blaze_surface::FileItemsView::read(&bytes).expect("outline opens");
        let lazy = SubjectFile::Path(b"src/rich.rs");
        let resolved = SubjectFile::Resolved(crate::signature::SignatureFile::new(b"src/rich.rs"));
        assert!(lazy == resolved);
        assert_eq!(lazy.cmp(&resolved), core::cmp::Ordering::Equal);
        assert_eq!(lazy.subject(), resolved.subject());

        let mut covered_count = 0usize;
        let mut held_count = 0usize;
        for i in 0..outline.items().len() {
            let idx = ItemIndex(u32::try_from(i).expect("small"));
            let want_cover = crate::signature::covering_entry(&outline, idx);
            let got_cover = covering_entry_view(view, idx);
            assert_eq!(got_cover, want_cover, "covering_entry mismatch at item {i}");

            let want_hold = holding_entry(&outline, idx);
            let got_hold = holding_entry_view(view, idx);
            assert_eq!(got_hold, want_hold, "holding_entry mismatch at item {i}");

            let direct = if let Some(cover) = got_cover {
                covered_count += 1;
                Some((lazy.entry_from_chain(crate::signature::stored_chain(view, cover)), None))
            } else if let Some(cover) = got_hold {
                held_count += 1;
                let from_entry = idx.0 - cover.0;
                Some((lazy.entry_from_chain(crate::signature::stored_chain(view, cover)), Some(Written { steps: ContentId::of(IN_A_BODY_DOMAIN, b""), from_entry })))
            } else {
                None
            };

            let from_outline = if let Some(cover) = want_cover {
                Some((resolved.entry(&outline, outline.item(cover), text), None))
            } else if let Some(holder) = want_hold {
                let cover = crate::signature::covering_entry(&outline, holder).expect("holder covered");
                let from_entry = idx.0 - cover.0;
                Some((resolved.entry(&outline, outline.item(cover), text), Some(Written { steps: ContentId::of(IN_A_BODY_DOMAIN, b""), from_entry })))
            } else {
                None
            };
            assert!(direct == from_outline, "direct vs outline placement mismatch at item {i}");
        }
        assert!(covered_count > 10, "expected covered top-level/nested items");
        assert!(held_count >= 2, "expected body-held items inside f_0");
    }
}
