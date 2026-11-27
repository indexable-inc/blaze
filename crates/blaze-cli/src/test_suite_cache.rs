//! Suite-wide in-memory publication, verification, and batch pre-publishing cache for `blaze test`.
//!
//! All harnessed tests in one `Suite` (`TestUnit`) share the same checked workspace snapshot,
//! `NamesRecords`, `RunIds`, and `Builder`. Without suite-level caching, running `N` tests in a
//! crate (e.g. 145 tests in `tempo-ext`, 32 in `tempo-chainspec`) constructs `N` fresh `Walk`s and
//! `Maker`s and re-runs `Maker::verify` + CAS lookups + purity checks + `instance_key` hashing
//! `N × 6,000` times over.
//!
//! [`SuiteCache`] keeps ONE `Walk`, ONE `TestMaker`, ONE `(instance_key, content_key)` verified
//! set (pre-seeded from `test_cache.rs` under today's unforgeable `Certificate`), and ONE map of
//! already-resolved [`CachedPub`] functions and [`CachedDatum`] statics/vtables per suite.
//!
//! Soundness invariants preserved:
//! - Every test still runs in its own isolated OS process with fresh memory and globals.
//! - Every refusal (`NotChecked`, `WalkError`, `NoPersonality`, `Unresolved`) leaves the row
//!   uncached so every test that calls a refused symbol still fails with the exact same `NOT RUN`.
//! - Pure tests (`keep.is_ok()`) NEVER receive pre-published `CodeClass::Program` functions they
//!   did not call (preserving exact `TestCache` selective invalidation and purity verdicts).
//! - Impure tests (`keep.is_err()`) batch-pre-publish the suite's warm functions (`warm_order`,
//!   excluding each test's own entry symbol so every test issues at least one `Up::Need` and
//!   signal reports always name the test entry).

use core::alloc::Allocator;
use core::cell::RefCell;

use blaze_code::datum::DatumView;
use blaze_code::record::{Space, Symbol as CodeSymbol};
use blaze_engine::SVec;
use blaze_hash::{BuildKeyHasher, FixedState, Map, MapOpts, Set, SetOpts};
use blaze_ladder::body_ids::{
    Begun, DeclaredList, EntryForm, Form as _, How, Make, Order, Pending, RowKind, Scratch, Stepped, Unkeyed, Walk, instance_key,
};
use blaze_ladder::code_records::statics::{STATIC_ENTRY_MAGIC, StaticEntryView, each_static_named, each_vtable_datum, entry_record};
use blaze_ladder::code_records::{Builder, CodeKey, CodeStore, EntryView, content_key};
use blaze_ladder::code_records::reads::Verdict;
use blaze_ladder::run::make_one::{ByKind, MOST_WALKED, MakeWatch, Maker, NamesRecords};
use blaze_ladder::run::special::{MakerKind, MakerStart, Roots, StartHead};
use blaze_ladder::run::RunError as LadderRunError;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::data::Datum;
use blaze_launch::wire::run::{BYTES_PART_MOST, Symbol};
use blaze_own::{AVec, Alloc, SharedAlloc, Vec};
use blaze_run::plan::NotKept;
use blaze_run::postings::FnName;
use blaze_screen::{Answered, Step, Watch};
use blaze_typeck::ItemKey;
use bstd::Cx;

use crate::run_bytes::Sent;
use crate::run_fault::{Named, Places, SiteShown};
use crate::run_ids::{RunIds, Runnable};
use crate::run_parent::{
    self, Codes, FIRST_CALL_SPAN, HOW_FOUND, HOW_MADE, HOW_NO_RECORD, HOW_SPECIAL, HOW_STALE, Needs, ParentError, Resolve,
    Resolved, RunMaker, Start as _, Tally,
};
use crate::test_cache::{EnteredFn, check_datum_purity, check_record_purity};
use crate::test_drive::Job;
use crate::test_loop::SuiteId;

/// Cap on warm suite functions queued for batch pre-publishing per suite (rule 18).
const WARM_CAP: usize = 16_384;

/// Cached `EnteredFn` identity computed once per walked function or static in the suite.
pub(crate) struct CachedEntered {
    pub symbol: FnName,
    pub code_key: [u8; 32],
    pub tabled: ItemKey,
    pub name: AVec<'static, u8>,
}

/// Cached static or vtable datum resolved once per suite.
struct CachedDatum {
    record: RecordId,
    bytes: AVec<'static, u8>,
    declares: AVec<'static, Symbol>,
    named: AVec<'static, Symbol>,
    entered: Option<CachedEntered>,
    unkeyed: bool,
    purity: Result<(), NotKept>,
}

/// Cached function publication resolved once per suite.
pub(crate) struct CachedPub {
    pub key: CodeKey,
    pub record: RecordId,
    pub bytes: AVec<'static, u8>,
    pub declares: AVec<'static, Symbol>,
    pub named: AVec<'static, Symbol>,
    pub values: AVec<'static, u8>,
    pub lands: bool,
    pub purity: Result<(), NotKept>,
    pub entered: Option<CachedEntered>,
    pub unkeyed: bool,
    pub tabled: Option<ItemKey>,
}

/// Where a cached `RecordId`'s bytes live (for crash report disassembly).
#[derive(Clone, Copy)]
enum CachedRecord {
    Fn([u8; 32]),
    Datum([u8; 32]),
}

/// Counts produced by one `ensure_cached` call (for signal report `FirstCall` accounting).
#[derive(Clone, Copy, Default)]
pub(crate) struct StepCounts {
    pub made: usize,
    pub found: usize,
    pub unkeyed: usize,
}

/// Returns `true` iff `item` belongs to a settled non-workspace (`Toolchain` or `Dependency`) crate.
fn is_closed_item(item: ItemKey, ids: &RunIds<'_>) -> bool {
    matches!(ids.grain(item), Some(crate::run_ids::Grain::Toolchain | crate::run_ids::Grain::Dependency))
}

/// Returns `true` iff `(tabled, name, entry)` touches ONLY settled `Toolchain` / `Dependency`
/// crates (zero `Workspace` items in `tabled`, generic args `name`, or any read-list question in
/// `entry`), proving its `Verdict::Holds` remains valid across workspace edits whenever
/// `closed_crates_digest` stands (rule 21).
fn is_closed_verified_entry(tabled: ItemKey, name: &[u8], entry: &[u8], ids: &RunIds<'_>) -> bool {
    if !is_closed_item(tabled, ids) {
        return false;
    }
    let mut ok = true;
    if blaze_ladder::body_ids::each_item(name, |item| {
        if ok && !is_closed_item(item, ids) {
            ok = false;
        }
    })
    .is_err()
        || !ok
    {
        return false;
    }
    if entry.get(..8).is_some_and(|magic| magic == STATIC_ENTRY_MAGIC) {
        return StaticEntryView::read(entry).is_ok();
    }
    let Ok(view) = EntryView::read(entry) else {
        return false;
    };
    for row in view.reads() {
        let Ok((question, _)) = row else {
            return false;
        };
        // Read-list question layout (`code_reads.rs::question_head`): 1-byte kind + 8-byte extra + instance name.
        let Some(q_name) = question.get(9..) else {
            return false;
        };
        if blaze_ladder::body_ids::each_item(q_name, |item| {
            if ok && !is_closed_item(item, ids) {
                ok = false;
            }
        })
        .is_err()
            || !ok
        {
            return false;
        }
    }
    true
}

/// Lazily-initialized [`Make`] adapter that consults `SuiteCache`'s verified sets in O(1) before
/// ever constructing the command-thread [`RunMaker`]. On warm `BZTV` hits (including closed
/// `std`/`core`/`alloc`/dep hits across workspace edits) and on cold `Codes::Pooled` misses
/// (dispatched to off-thread `CodePool` workers), no command-thread `Maker` is constructed.
struct LazyVerifyingMaker<'a, 'h> {
    names: NamesRecords<'h>,
    builder: &'a Builder,
    typed: crate::engine_body_record::TypedFrom<'h>,
    tables: Alloc<'static>,
    make_watch: MakeWatch<'static>,
    entry: Option<ItemKey>,
    start: MakerStart,
    ids: RunIds<'h>,
    enclosers: LocalEnclosers,
    maker: &'a mut Option<RunMaker<'h>>,
    verified_closed: &'a mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    verified_workspace: &'a mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    verified_dirty: &'a mut bool,
}

impl<'h> LazyVerifyingMaker<'_, 'h> {
    fn ensure_maker(&mut self) -> Result<&mut RunMaker<'h>, LadderRunError> {
        let want = MakerKind::of(self.entry, self.start);
        let maker = self.maker.get_or_insert_with(|| {
            Maker::of_start(self.names, self.entry, self.start, ByKind::new(self.typed.over(self.ids)), self.builder, Some(self.make_watch), self.tables)
        });
        if maker.made_for() != want
            || (matches!(self.start, MakerStart::Direct(_)) && !maker.set_direct_entry(self.entry, self.start))
        {
            return Err(LadderRunError::OneBody {
                key: self.entry.unwrap_or(ItemKey(0)),
                why: "the suite maker was made for another entry or start than this test's roots say (test_suite_cache.rs `SuiteCache`)",
            });
        }
        Ok(maker)
    }
}

impl Make for LazyVerifyingMaker<'_, '_> {
    type Error = LadderRunError;
    type Grant = Runnable;

    fn make(&mut self, grant: &Self::Grant, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
        let enclosers = self.enclosers;
        let maker = self.ensure_maker()?;
        maker.prepare_local(enclosers.locals(), enclosers.enclosers());
        maker.make(grant, tabled, name, entry)
    }

    fn make_static(&mut self, grant: &Self::Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
        let enclosers = self.enclosers;
        let maker = self.ensure_maker()?;
        maker.prepare_local(enclosers.locals(), enclosers.enclosers());
        maker.make_static(grant, item, entry)
    }

    fn make_thread_local(&mut self, grant: &Self::Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
        let enclosers = self.enclosers;
        let maker = self.ensure_maker()?;
        maker.prepare_local(enclosers.locals(), enclosers.enclosers());
        maker.make_thread_local(grant, item, entry)
    }

    fn verify(&mut self, tabled: ItemKey, name: &[u8], entry: &[u8]) -> Result<Option<Verdict>, Self::Error> {
        let enclosers = self.enclosers;
        let maker = self.ensure_maker()?;
        maker.prepare_local(enclosers.locals(), enclosers.enclosers());
        maker.verify(tabled, name, entry)
    }

    fn verify_keyed(
        &mut self,
        key: CodeKey,
        under: CodeKey,
        tabled: ItemKey,
        name: &[u8],
        entry: &[u8],
    ) -> Result<Option<Verdict>, Self::Error> {
        let pair = (key.0.0, under.0.0);
        if self.verified_closed.contains(&pair) || self.verified_workspace.contains(&pair) {
            return Ok(Some(Verdict::Holds));
        }
        let enclosers = self.enclosers;
        let maker = self.ensure_maker()?;
        maker.prepare_local(enclosers.locals(), enclosers.enclosers());
        let verdict = maker.verify(tabled, name, entry)?;
        if matches!(verdict, Some(Verdict::Holds)) {
            let inserted = if is_closed_verified_entry(tabled, name, entry, &self.ids) {
                self.verified_closed.insert(pair)
            } else {
                self.verified_workspace.insert(pair)
            };
            if inserted {
                *self.verified_dirty = true;
            }
        }
        Ok(verdict)
    }
}

/// Bounded inline chain of body-local keys (`LibKey::INNER` set — either the row's tabled item or
/// local items referenced in its generic arguments) and their same-crate declarer `ItemKey`s
/// (outermost first), so any `CodePool` worker (or partial-warm serial run) can file local fns
/// (e.g. `const_eval_select!` -> `is_null::runtime`, or `thread_local!` closures) in O(1).
pub(crate) const MAX_LOCAL_ENCLOSERS: usize = 8;

#[derive(Clone, Copy)]
pub(crate) struct LocalEnclosers {
    pub locals: [ItemKey; MAX_LOCAL_ENCLOSERS],
    pub locals_len: usize,
    pub items: [ItemKey; MAX_LOCAL_ENCLOSERS],
    pub len: usize,
}

impl Default for LocalEnclosers {
    fn default() -> Self {
        Self {
            locals: [ItemKey(0); MAX_LOCAL_ENCLOSERS],
            locals_len: 0,
            items: [ItemKey(0); MAX_LOCAL_ENCLOSERS],
            len: 0,
        }
    }
}

impl LocalEnclosers {
    pub(crate) fn locals(&self) -> &[ItemKey] {
        self.locals.get(..self.locals_len).unwrap_or(&[])
    }

    pub(crate) fn enclosers(&self) -> &[ItemKey] {
        self.items.get(..self.len).unwrap_or(&[])
    }
}

/// Collects any body-local keys (`LibKey::INNER` set) in `key` or the encoded generic
/// arguments of `row`, together with their outermost-first same-crate declarer chain from
/// `Walk::declarer` (fast-path returns empty in ~20 ns when no key is local).
pub(crate) fn local_enclosers_of(walk: &Walk<'_>, row: usize, key: ItemKey) -> LocalEnclosers {
    let mut out = LocalEnclosers::default();
    let mut push_local = |k: ItemKey| {
        if blaze_ladder::front2::program::lazy::typed::is_local(k)
            && !out.locals().contains(&k)
            && let Some(slot) = out.locals.get_mut(out.locals_len)
        {
            *slot = k;
            out.locals_len += 1;
        }
    };
    push_local(key);
    let name = walk.name(row);
    if !name.is_empty() {
        let _ = blaze_ladder::body_ids::each_item(name, |k| push_local(k));
    }
    if out.locals_len == 0 {
        return out;
    }
    let mut cur = row;
    let mut hops = 0usize;
    while let Some(parent) = walk.declarer(cur) {
        hops += 1;
        if hops > MAX_LOCAL_ENCLOSERS * 2 || out.len >= MAX_LOCAL_ENCLOSERS {
            break;
        }
        let tabled = walk.tabled(parent);
        let prefix = tabled.0 >> 64;
        if out.locals().iter().any(|l| (l.0 >> 64) == prefix && *l != tabled)
            && !out.enclosers().contains(&tabled)
            && let Some(slot) = out.items.get_mut(out.len)
        {
            *slot = tabled;
            out.len += 1;
        }
        cur = parent;
    }
    if let Some(slice) = out.items.get_mut(..out.len) {
        slice.reverse();
    }
    out
}

/// In-flight pooled build for one `Walk` row currently in `How::Making`.
struct InFlight {
    pending: Pending,
    /// Monotonic epoch token distinguishing this build attempt from any later retry of the
    /// same row after an abandon/refusal.
    epoch: u64,
    key: Option<CodeKey>,
    order: Order,
    proof: Runnable,
    symbol: [u8; 32],
    entry: Option<ItemKey>,
    start: MakerStart,
    enclosers: LocalEnclosers,
    /// Number of children currently parked on `CodeRelay` for this row.
    waiters: usize,
}

/// Payload for submitting a cold row to `CodeRelay` from the child's turn.
#[derive(Clone, Copy)]
pub(crate) struct NeedAsk {
    pub row: usize,
    pub epoch: u64,
    pub symbol: [u8; 32],
    pub key: Option<CodeKey>,
    pub order: Order,
    pub proof: Runnable,
    pub entry: Option<ItemKey>,
    pub start: MakerStart,
    pub enclosers: LocalEnclosers,
}

/// What a child must wait for when `SuiteCache::ensure_cached` hits a cold miss under
/// `Codes::Pooled`.
#[derive(Clone, Copy)]
pub(crate) enum NeedCode {
    /// Ask `CodeRelay` for this job (deduplicated across children/suites when `key.is_some()`).
    Ask(NeedAsk),
    /// Another child in this suite already submitted an unkeyed (`key.is_none()`) ask for `row`;
    /// park until `SuiteCache::is_in_flight(row, epoch)` becomes false without submitting a duplicate.
    WaitPeer { row: usize, epoch: u64, symbol: [u8; 32] },
}

/// Result of `SuiteCache::ensure_cached`.
pub(crate) enum EnsureCached {
    Ready(StepCounts),
    Unknown,
    NeedCode(NeedCode),
}

enum StepRow {
    Ready(Stepped, AVec<'static, u8>),
    Need(NeedCode),
}

/// Per-child tracking of which statics/vtables and inline record bytes have been sent to THIS
/// child process.
pub(crate) struct ChildSent<'a> {
    told: Set<[u8; 32], BuildKeyHasher, Alloc<'a>>,
    sent_records: Set<[u8; 32], BuildKeyHasher, Alloc<'a>>,
    /// Index into `SuiteCache::warm_order` already pre-published to this impure child.
    pub warm_cursor: usize,
}

impl<'a> ChildSent<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> ChildSent<'a> {
        ChildSent {
            told: Set::new(SetOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None }),
            sent_records: Set::new(SetOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None }),
            warm_cursor: 0,
        }
    }
}

/// Shared suite-level resolver state across all tests of one `Suite`.
pub(crate) struct SuiteCache<'h> {
    names: NamesRecords<'h>,
    builder: Builder,
    typed: crate::engine_body_record::TypedFrom<'h>,
    tables: Alloc<'static>,
    make_watch: MakeWatch<'static>,
    walk: Walk<'static>,
    scratch: Scratch<'static>,
    list: DeclaredList<'static>,
    maker: Option<RunMaker<'h>>,
    verified_closed: Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    verified_workspace: Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    verified_dirty: bool,
    pubs: Map<[u8; 32], CachedPub, BuildKeyHasher, Alloc<'static>>,
    datums: Map<[u8; 32], CachedDatum, BuildKeyHasher, Alloc<'static>>,
    by_record: Map<[u8; 32], CachedRecord, BuildKeyHasher, Alloc<'static>>,
    warm_order: AVec<'static, Symbol>,
    /// Rows currently being compiled on `CodePool` workers (`How::Making`), keyed by walk row.
    in_flight: Map<usize, InFlight, FixedState, Alloc<'static>>,
    /// Entry bytes for `How::Done(Stepped::Unkeyed(_))` rows finished by a worker before their
    /// `CachedPub` / `CachedDatum` has been finalized (unkeyed rows are not stored in CAS).
    unkeyed_entries: Map<usize, AVec<'static, u8>, FixedState, Alloc<'static>>,
    next_epoch: u64,
}

impl<'h> SuiteCache<'h> {
    pub(crate) fn new(
        names: NamesRecords<'h>,
        builder: Builder,
        typed: crate::engine_body_record::TypedFrom<'h>,
        tables: Alloc<'static>,
        make_watch: MakeWatch<'static>,
    ) -> SuiteCache<'h> {
        SuiteCache {
            names,
            builder,
            typed,
            tables,
            make_watch,
            walk: Walk::new_in(tables),
            scratch: Scratch::new_in(tables),
            list: DeclaredList::new_in(tables),
            maker: None,
            verified_closed: Set::new(SetOpts { hasher: FixedState, alloc: tables, capacity: None }),
            verified_workspace: Set::new(SetOpts { hasher: FixedState, alloc: tables, capacity: None }),
            verified_dirty: false,
            pubs: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: tables, capacity: None }),
            datums: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: tables, capacity: None }),
            by_record: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: tables, capacity: None }),
            warm_order: Vec::new_in(tables),
            in_flight: Map::new(MapOpts { hasher: FixedState, alloc: tables, capacity: None }),
            unkeyed_entries: Map::new(MapOpts { hasher: FixedState, alloc: tables, capacity: None }),
            next_epoch: 1,
        }
    }

    pub(crate) fn verified_sets_mut(
        &mut self,
    ) -> (
        &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
        &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    ) {
        (&mut self.verified_closed, &mut self.verified_workspace)
    }

    pub(crate) fn verified_dirty(&self) -> bool {
        self.verified_dirty
    }

    pub(crate) fn verified_closed_pairs(&self) -> impl Iterator<Item = ([u8; 32], [u8; 32])> + '_ {
        self.verified_closed.iter().copied()
    }

    pub(crate) fn verified_workspace_pairs(&self) -> impl Iterator<Item = ([u8; 32], [u8; 32])> + '_ {
        self.verified_workspace.iter().copied()
    }

    fn record_verified_entry(&mut self, pair: ([u8; 32], [u8; 32]), tabled: ItemKey, name: &[u8], entry: &[u8], ids: &RunIds<'_>) {
        let inserted = if is_closed_verified_entry(tabled, name, entry, ids) {
            self.verified_closed.insert(pair)
        } else {
            self.verified_workspace.insert(pair)
        };
        if inserted {
            self.verified_dirty = true;
        }
    }

    pub(crate) fn warm_len(&self) -> usize {
        self.warm_order.len()
    }

    /// Roots a test's root instances into the shared walk (idempotent for already-rooted symbols).
    /// Returns the first root's symbol (the test function itself for harnessed tests), or zero.
    pub(crate) fn root_test(&mut self, roots: &Roots<'_>) -> Result<[u8; 32], ParentError> {
        let fallback = roots.head().line.symbol.0;
        let mut first = [0u8; 32];
        for i in 0..roots.root_count() {
            let row = self
                .walk
                .root::<LadderRunError>(roots.root_row(i), MOST_WALKED)
                .map_err(|why| ParentError::Walk { symbol: fallback, why })?;
            if i == 0 {
                first = self.walk.symbol(row).0;
            }
        }
        Ok(first)
    }

    /// Ensures `symbol` (and every transitive static/vtable it references) is cached in `self`.
    /// Under `Codes::Pooled`, returns [`EnsureCached::NeedCode`] on a cold miss (after marking the
    /// row `How::Making`) so the calling child submits the job to `CodeRelay` and parks.
    #[expect(clippy::too_many_arguments, reason = "borrows the test's roots, ids, store and trace context")]
    pub(crate) fn ensure_cached(
        &mut self,
        symbol: Symbol,
        test_entry: [u8; 32],
        codes: Codes,
        roots: &Roots<'_>,
        ids: &RunIds<'h>,
        store: &mut dyn CodeStore,
        cx: &Cx,
        watch: &dyn Watch,
        alloc: Alloc<'_>,
    ) -> Result<EnsureCached, ParentError> {
        if self.pubs.contains_key(&symbol.0) {
            return Ok(EnsureCached::Ready(StepCounts { made: 0, found: 1, unkeyed: 0 }));
        }
        let code_sym = CodeSymbol(symbol.0);
        let head = roots.head();
        let Some(row) = self.walk.row_of(&code_sym) else {
            let Some(made) = roots.special(&code_sym, &self.builder, store, alloc)? else {
                return Ok(EnsureCached::Unknown);
            };
            cx.span(FIRST_CALL_SPAN).event(HOW_SPECIAL);
            let entry = store
                .get(made.key)
                .map_err(|why| ParentError::Store { symbol: symbol.0, why })?
                .ok_or(ParentError::NotStored { symbol: symbol.0 })?;
            let record = entry_record(entry).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
            let mut values = Vec::new_in(self.tables);
            let lands = run_parent::record_publication(symbol.0, record, head.personality.is_none(), &mut values, || Ok(self.names))?;
            if lands && head.personality.is_none() {
                return Err(ParentError::NoPersonality { function: None, path: None, symbol: symbol.0 });
            }
            self.list.clear();
            EntryForm.declared(entry, &mut self.list).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
            if !self.list.statics().is_empty() || !self.list.thread_locals().is_empty() {
                return Err(ParentError::SpecialStatic { symbol: symbol.0 });
            }
            let mut named = Vec::new_in(self.tables);
            each_static_named(entry, |s| named.push(Symbol(s.0))).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
            named.sort_unstable();
            named.dedup();
            let mut declares = Vec::with_capacity_in(self.list.len(), self.tables);
            for i in 0..self.list.len() {
                let callee = self
                    .walk
                    .root::<LadderRunError>(self.list.row(i), MOST_WALKED)
                    .map_err(|why| ParentError::Walk { symbol: symbol.0, why })?;
                declares.push(Symbol(self.walk.symbol(callee).0));
            }
            let record_id = RecordId(blaze_blake3::hash(record));
            let mut bytes = Vec::with_capacity_in(record.len(), self.tables);
            bytes.extend_from_slice(record);
            let purity = check_record_purity(record);
            watch.see(Step::Answered(if made.found { Answered::Found } else { Answered::Made }));
            let cached = CachedPub {
                key: made.key,
                record: record_id,
                bytes,
                declares,
                named,
                values,
                lands,
                purity,
                entered: None,
                unkeyed: false,
                tabled: None,
            };
            self.by_record.insert(record_id.0, CachedRecord::Fn(symbol.0));
            self.pubs.insert(symbol.0, cached);
            // Do not add per-test C entry shims to `warm_order`.
            let is_entry_shim = symbol.0 == head.line.symbol.0;
            if !is_entry_shim && symbol.0 != test_entry && self.warm_order.len() < WARM_CAP {
                self.warm_order.push(symbol);
            }
            return Ok(EnsureCached::Ready(StepCounts { made: usize::from(!made.found), found: usize::from(made.found), unkeyed: 0 }));
        };

        if matches!(self.walk.kind(row), RowKind::Static | RowKind::ThreadLocal) {
            return Ok(EnsureCached::Unknown);
        }

        let mut counts = StepCounts::default();
        let tabled = self.walk.tabled(row);
        let (stepped, entry_copy) = match self.step_or_begin_row(row, codes, roots, ids, store, cx, watch, &mut counts)? {
            StepRow::Ready(stepped, copy) => (stepped, copy),
            StepRow::Need(need) => return Ok(EnsureCached::NeedCode(need)),
        };
        let entry = entry_copy.as_slice();
        let record = entry_record(entry).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
        let mut values = Vec::new_in(self.tables);
        let lands = run_parent::record_publication(symbol.0, record, head.personality.is_none(), &mut values, || Ok(self.names))?;
        if lands && head.personality.is_none() {
            let path = self.shown_of(tabled).and_then(|s| s.path().map(Into::into));
            return Err(ParentError::NoPersonality { function: Some(tabled), path, symbol: symbol.0 });
        }
        let unkeyed_step = matches!(stepped, Stepped::Unkeyed(_));
        let key = match stepped {
            Stepped::Found(k) | Stepped::Made(k) => k,
            Stepped::Unkeyed(_) => content_key(entry),
        };
        let record_id = RecordId(blaze_blake3::hash(record));
        let mut bytes = Vec::with_capacity_in(record.len(), self.tables);
        bytes.extend_from_slice(record);

        let mut declares = Vec::new_in(self.tables);
        let mut static_work: AVec<'_, usize> = Vec::new_in(alloc);
        for &callee in self.walk.declared(row) {
            let callee = usize::try_from(callee).unwrap_or(0);
            match self.walk.kind(callee) {
                RowKind::Instance => declares.push(Symbol(self.walk.symbol(callee).0)),
                RowKind::Static | RowKind::ThreadLocal => static_work.push(callee),
            }
        }
        let mut named = Vec::new_in(self.tables);
        each_static_named(entry, |s| named.push(Symbol(s.0))).map_err(|why| ParentError::Entry { symbol: symbol.0, why })?;
        named.sort_unstable();
        named.dedup();

        // Stage and resolve every transitive static/thread-local row not already in `self.datums`.
        // Under `Codes::Pooled`, a cold transitive static returns `NeedCode` BEFORE inserting any
        // staged datum into `self.datums` (or mutating `self.pubs`) so a retry never skips an
        // already-cached parent static whose child static was still cold (`StaticUnsent`).
        struct StagedDatum {
            at: usize,
            sym: [u8; 32],
            stepped: Stepped,
            entry: AVec<'static, u8>,
        }
        let mut staged: AVec<'_, StagedDatum> = Vec::new_in(alloc);
        let mut seen_statics: Set<[u8; 32], FixedState, Alloc<'_>> = Set::new_in(alloc);
        let mut next = 0usize;
        while let Some(&at) = static_work.get(next) {
            next += 1;
            let static_sym = self.walk.symbol(at).0;
            if self.datums.contains_key(&static_sym) || !seen_statics.insert(static_sym) {
                continue;
            }
            let (s_stepped, s_entry_copy) = match self.step_or_begin_row(at, codes, roots, ids, store, cx, watch, &mut counts)? {
                StepRow::Ready(stepped, copy) => (stepped, copy),
                StepRow::Need(need) => return Ok(EnsureCached::NeedCode(need)),
            };
            for &callee in self.walk.declared(at) {
                let callee = usize::try_from(callee).unwrap_or(0);
                if matches!(self.walk.kind(callee), RowKind::Static | RowKind::ThreadLocal) {
                    let csym = self.walk.symbol(callee).0;
                    if !self.datums.contains_key(&csym) {
                        static_work.push(callee);
                    }
                }
            }
            staged.push(StagedDatum { at, sym: static_sym, stepped: s_stepped, entry: s_entry_copy });
        }

        // Commit all staged datums atomically now that the entire transitive static closure is ready.
        for item in staged {
            let at = item.at;
            let static_sym = item.sym;
            let s_stepped = item.stepped;
            let s_entry = item.entry.as_slice();
            let s_datum = entry_record(s_entry).map_err(|why| ParentError::Entry { symbol: static_sym, why })?;
            let s_record = RecordId(blaze_blake3::hash(s_datum));
            let mut s_bytes = Vec::with_capacity_in(s_datum.len(), self.tables);
            s_bytes.extend_from_slice(s_datum);

            let mut s_declares = Vec::new_in(self.tables);
            for &callee in self.walk.declared(at) {
                let callee = usize::try_from(callee).unwrap_or(0);
                if matches!(self.walk.kind(callee), RowKind::Instance) {
                    s_declares.push(Symbol(self.walk.symbol(callee).0));
                }
            }
            let mut s_named = Vec::new_in(self.tables);
            each_static_named(s_entry, |s| s_named.push(Symbol(s.0)))
                .map_err(|why| ParentError::Entry { symbol: static_sym, why })?;
            s_named.sort_unstable();
            s_named.dedup();
            self.cache_vtables(static_sym, s_entry)?;

            let s_unkeyed_step = matches!(s_stepped, Stepped::Unkeyed(_));
            let s_tabled = self.walk.tabled(at);
            let s_entered = if s_unkeyed_step {
                None
            } else {
                let s_name = self.walk.name(at);
                let ik = instance_key(CodeSymbol(static_sym), s_tabled, s_name, ids, &self.builder, &self.builder.producer, self.tables);
                let entered = ik.map(|ik| {
                    let mut name_copy = Vec::with_capacity_in(s_name.len(), self.tables);
                    name_copy.extend_from_slice(s_name);
                    CachedEntered { symbol: FnName(static_sym), code_key: ik.0.0, tabled: s_tabled, name: name_copy }
                });
                if let (Stepped::Made(under), Some(ik)) = (s_stepped, ik) {
                    let name_slice = entered.as_ref().map_or(&[][..], |e| e.name.as_slice());
                    self.record_verified_entry((ik.0.0, under.0.0), s_tabled, name_slice, s_entry, ids);
                }
                entered
            };
            let s_unkeyed = s_unkeyed_step || s_entered.is_none();
            let s_purity = if s_unkeyed { Err(NotKept::Untraced) } else { check_datum_purity(s_datum) };
            let _ = self.unkeyed_entries.remove(&at);
            self.by_record.insert(s_record.0, CachedRecord::Datum(static_sym));
            self.datums.insert(
                static_sym,
                CachedDatum {
                    record: s_record,
                    bytes: s_bytes,
                    declares: s_declares,
                    named: s_named,
                    entered: s_entered,
                    unkeyed: s_unkeyed,
                    purity: s_purity,
                },
            );
        }

        // Cache any vtable datums carried by this entry.
        self.cache_vtables(symbol.0, entry)?;

        // Compute `EnteredFn` metadata once for this function (and record newly made verified pair).
        let entered = if unkeyed_step {
            None
        } else {
            let name_slice = self.walk.name(row);
            let inst_key = instance_key(code_sym, tabled, name_slice, ids, &self.builder, &self.builder.producer, self.tables);
            let entered = inst_key.map(|ik| {
                let mut name_copy = Vec::with_capacity_in(name_slice.len(), self.tables);
                name_copy.extend_from_slice(name_slice);
                CachedEntered { symbol: FnName(symbol.0), code_key: ik.0.0, tabled, name: name_copy }
            });
            if let (Stepped::Made(under), Some(ik)) = (stepped, inst_key) {
                let name_bytes = entered.as_ref().map_or(&[][..], |e| e.name.as_slice());
                self.record_verified_entry((ik.0.0, under.0.0), tabled, name_bytes, entry, ids);
            }
            entered
        };
        let unkeyed = unkeyed_step || entered.is_none();
        let purity = if unkeyed { Err(NotKept::Untraced) } else { check_record_purity(record) };

        let _ = self.unkeyed_entries.remove(&row);
        self.by_record.insert(record_id.0, CachedRecord::Fn(symbol.0));
        self.pubs.insert(
            symbol.0,
            CachedPub {
                key,
                record: record_id,
                bytes,
                declares,
                named,
                values,
                lands,
                purity,
                entered,
                unkeyed,
                tabled: Some(tabled),
            },
        );
        if symbol.0 != test_entry && self.warm_order.len() < WARM_CAP {
            self.warm_order.push(symbol);
        }
        Ok(EnsureCached::Ready(counts))
    }

    fn cache_vtables(&mut self, owner_symbol: [u8; 32], entry: &[u8]) -> Result<(), ParentError> {
        let tables = self.tables;
        let datums = &mut self.datums;
        let by_record = &mut self.by_record;
        each_vtable_datum(
            entry,
            |vtable, vdatum| {
                if datums.contains_key(&vtable.0) {
                    return Ok(());
                }
                let record = RecordId(blaze_blake3::hash(vdatum));
                let mut bytes = Vec::with_capacity_in(vdatum.len(), tables);
                bytes.extend_from_slice(vdatum);
                let mut declares = Vec::new_in(tables);
                let mut named = Vec::new_in(tables);
                if let Ok(view) = DatumView::read(vdatum) {
                    for i in 0..view.targets() {
                        let t = view.target(i);
                        match t.space {
                            Space::Func => declares.push(Symbol(t.symbol.0)),
                            Space::Static | Space::Vtable | Space::Tls => named.push(Symbol(t.symbol.0)),
                            Space::Outside | Space::WeakOutside => {}
                        }
                    }
                }
                named.sort_unstable();
                named.dedup();
                let v_purity = check_datum_purity(vdatum);
                by_record.insert(record.0, CachedRecord::Datum(vtable.0));
                datums.insert(
                    vtable.0,
                    CachedDatum { record, bytes, declares, named, entered: None, unkeyed: false, purity: v_purity },
                );
                Ok(())
            },
            |why| ParentError::Entry { symbol: owner_symbol, why },
        )
    }

    /// Steps `row` in the shared walk if `How::Waiting`, returns its stored/unkeyed entry if
    /// already `How::Done`, or returns [`StepRow::Need`] under `Codes::Pooled` on a cold miss (or
    /// when another child in this suite already began compiling `row`).
    #[expect(clippy::too_many_arguments, reason = "borrows walk state, ids, store and trace context")]
    fn step_or_begin_row(
        &mut self,
        row: usize,
        codes: Codes,
        roots: &Roots<'_>,
        ids: &RunIds<'h>,
        store: &mut dyn CodeStore,
        cx: &Cx,
        watch: &dyn Watch,
        counts: &mut StepCounts,
    ) -> Result<StepRow, ParentError> {
        let symbol = self.walk.symbol(row).0;
        let tabled = self.walk.tabled(row);
        match self.walk.how(row) {
            How::Done(stepped @ (Stepped::Found(key) | Stepped::Made(key))) => {
                let bytes = store
                    .get(key)
                    .map_err(|why| ParentError::Store { symbol, why })?
                    .ok_or(ParentError::NotStored { symbol })?;
                let mut copy = Vec::with_capacity_in(bytes.len(), self.tables);
                copy.extend_from_slice(bytes);
                match stepped {
                    Stepped::Made(_) => counts.made += 1,
                    _ => counts.found += 1,
                }
                Ok(StepRow::Ready(stepped, copy))
            }
            How::Done(stepped @ Stepped::Unkeyed(_)) => {
                let bytes = self.unkeyed_entries.get(&row).ok_or(ParentError::ResolvedTwice { symbol })?;
                let mut copy = Vec::with_capacity_in(bytes.len(), self.tables);
                copy.extend_from_slice(bytes.as_slice());
                counts.unkeyed += 1;
                Ok(StepRow::Ready(stepped, copy))
            }
            How::Making => {
                let Some(flight) = self.in_flight.get_mut(&row) else {
                    return Err(ParentError::ResolvedTwice { symbol });
                };
                if flight.key.is_some() {
                    flight.waiters += 1;
                    Ok(StepRow::Need(NeedCode::Ask(NeedAsk {
                        row,
                        epoch: flight.epoch,
                        symbol: flight.symbol,
                        key: flight.key,
                        order: flight.order,
                        proof: flight.proof,
                        entry: flight.entry,
                        start: flight.start,
                        enclosers: flight.enclosers,
                    })))
                } else {
                    Ok(StepRow::Need(NeedCode::WaitPeer { row, epoch: flight.epoch, symbol: flight.symbol }))
                }
            }
            How::Waiting => {
                let proof = ids
                    .runnable(tabled)
                    .map_err(|why| ParentError::NotChecked { function: tabled, symbol, why })?;
                let shown = watch.paths().then(|| self.shown_of(tabled)).flatten();
                let path = shown.as_ref().and_then(|s| s.path());
                watch.see(Step::Resolving { path: path.as_deref() });
                let enclosers = self.local_enclosers(row, tabled);
                let span = cx.span(FIRST_CALL_SPAN);
                let mut vm = LazyVerifyingMaker {
                    names: self.names,
                    builder: &self.builder,
                    typed: self.typed,
                    tables: self.tables,
                    make_watch: self.make_watch,
                    entry: roots.maker_entry(),
                    start: roots.maker_start,
                    ids: *ids,
                    enclosers,
                    maker: &mut self.maker,
                    verified_closed: &mut self.verified_closed,
                    verified_workspace: &mut self.verified_workspace,
                    verified_dirty: &mut self.verified_dirty,
                };
                let stepped = match codes {
                    Codes::Serial => self
                        .walk
                        .one(row, ids, store, &self.builder, &self.builder.producer, &mut vm, &proof, &EntryForm, MOST_WALKED, &mut self.scratch)
                        .map_err(|why| ParentError::Walk { symbol, why })?
                        .stepped,
                    Codes::Pooled => {
                        match self
                            .walk
                            .begin(row, ids, store, &self.builder, &self.builder.producer, &mut vm, &EntryForm, MOST_WALKED, &mut self.scratch)
                            .map_err(|why| ParentError::Walk { symbol, why })?
                        {
                            Begun::Stepped(one) => one.stepped,
                            Begun::Make(pending) => {
                                let epoch = self.next_epoch;
                                self.next_epoch = self.next_epoch.wrapping_add(1).max(1);
                                let key = pending.key();
                                let order = pending.order();
                                let entry = roots.maker_entry();
                                let start = roots.maker_start;
                                self.in_flight.insert(
                                    row,
                                    InFlight { pending, epoch, key, order, proof, symbol, entry, start, enclosers, waiters: 1 },
                                );
                                return Ok(StepRow::Need(NeedCode::Ask(NeedAsk { row, epoch, symbol, key, order, proof, entry, start, enclosers })));
                            }
                        }
                    }
                };
                match stepped {
                    Stepped::Found(_) => {
                        span.event(HOW_FOUND);
                        watch.see(Step::Answered(Answered::Found));
                        counts.found += 1;
                    }
                    Stepped::Made(_) => {
                        span.event(HOW_MADE);
                        watch.see(Step::Answered(Answered::Made));
                        counts.made += 1;
                    }
                    Stepped::Unkeyed(Unkeyed::NoRecord) => {
                        span.event(HOW_NO_RECORD);
                        watch.see(Step::Answered(Answered::Unkeyed));
                        counts.unkeyed += 1;
                    }
                    Stepped::Unkeyed(Unkeyed::Stale { .. }) => {
                        span.event(HOW_STALE);
                        watch.see(Step::Answered(Answered::Unkeyed));
                        counts.unkeyed += 1;
                    }
                }
                let raw = match stepped {
                    Stepped::Found(key) => store
                        .get(key)
                        .map_err(|why| ParentError::Store { symbol, why })?
                        .ok_or(ParentError::NotStored { symbol })?,
                    Stepped::Made(_) | Stepped::Unkeyed(_) => self.scratch.made(),
                };
                let mut copy = Vec::with_capacity_in(raw.len(), self.tables);
                copy.extend_from_slice(raw);
                Ok(StepRow::Ready(stepped, copy))
            }
        }
    }

    /// Collects any body-local keys (`LibKey::INNER` set) in `tabled` or the encoded generic
    /// arguments of `row`, together with their outermost-first same-crate declarer chain from
    /// `Walk::declarer` (fast-path returns empty in ~20 ns when no key is local).
    fn local_enclosers(&self, row: usize, key: ItemKey) -> LocalEnclosers {
        local_enclosers_of(&self.walk, row, key)
    }

    /// Builds a `CodePool` [`Job`] for `need`, copying the row's instance name into `alloc`.
    pub(crate) fn job_for(&self, suite: SuiteId, need: &NeedAsk, alloc: SharedAlloc<'static>) -> Job<'static> {
        let name_slice = self.walk.name(need.row);
        let mut name = SVec::with_capacity_in(name_slice.len(), alloc);
        name.extend_from_slice(name_slice);
        Job { suite, order: need.order, name, proof: need.proof, entry: need.entry, start: need.start, enclosers: need.enclosers }
    }

    /// Finishes an in-flight pooled row when a worker returns `entry`: stores into CAS via
    /// `Walk::finish`, records `(instance_key, content_key)` in `self.verified` for `BZTV`, and
    /// retains unkeyed bytes in `self.unkeyed_entries` until `ensure_cached` caches the row.
    /// Idempotent across multiple waiters sharing the same `(row, epoch)`.
    #[expect(clippy::too_many_arguments, reason = "finishes a worker's entry into the shared walk, CAS store, and verified cache")]
    pub(crate) fn finish_in_flight(
        &mut self,
        row: usize,
        epoch: u64,
        entry: &[u8],
        ids: &RunIds<'h>,
        store: &mut dyn CodeStore,
        cx: &Cx,
        watch: &dyn Watch,
    ) -> Result<(), ParentError> {
        if !self.in_flight.get(&row).is_some_and(|flight| flight.epoch == epoch) {
            return Ok(());
        }
        let Some(flight) = self.in_flight.remove(&row) else {
            return Ok(());
        };
        let one = self
            .walk
            .finish::<LadderRunError>(flight.pending, entry, store, &EntryForm, MOST_WALKED, &mut self.scratch)
            .map_err(|why| ParentError::Walk { symbol: flight.symbol, why })?;
        let stepped = one.stepped;
        let span = cx.span(FIRST_CALL_SPAN);
        match stepped {
            Stepped::Found(_) => {
                span.event(HOW_FOUND);
                watch.see(Step::Answered(Answered::Found));
            }
            Stepped::Made(under) => {
                span.event(HOW_MADE);
                watch.see(Step::Answered(Answered::Made));
                let tabled = self.walk.tabled(row);
                let name_slice = self.walk.name(row);
                if let Some(ik) = instance_key(CodeSymbol(flight.symbol), tabled, name_slice, ids, &self.builder, &self.builder.producer, self.tables) {
                    let is_closed = is_closed_verified_entry(tabled, name_slice, entry, ids);
                    let inserted = if is_closed {
                        self.verified_closed.insert((ik.0.0, under.0.0))
                    } else {
                        self.verified_workspace.insert((ik.0.0, under.0.0))
                    };
                    if inserted {
                        self.verified_dirty = true;
                    }
                }
            }
            Stepped::Unkeyed(Unkeyed::NoRecord) => {
                span.event(HOW_NO_RECORD);
                watch.see(Step::Answered(Answered::Unkeyed));
                let mut copy = Vec::with_capacity_in(entry.len(), self.tables);
                copy.extend_from_slice(entry);
                self.unkeyed_entries.insert(row, copy);
            }
            Stepped::Unkeyed(Unkeyed::Stale { .. }) => {
                span.event(HOW_STALE);
                watch.see(Step::Answered(Answered::Unkeyed));
                let mut copy = Vec::with_capacity_in(entry.len(), self.tables);
                copy.extend_from_slice(entry);
                self.unkeyed_entries.insert(row, copy);
            }
        }
        Ok(())
    }

    /// Abandons an in-flight pooled row when a worker refuses it so the row returns cleanly to
    /// `How::Waiting` (refusals are never cached). Idempotent across multiple waiters.
    pub(crate) fn refused_in_flight(&mut self, row: usize, epoch: u64) -> Result<(), ParentError> {
        if !self.in_flight.get(&row).is_some_and(|flight| flight.epoch == epoch) {
            return Ok(());
        }
        let Some(flight) = self.in_flight.remove(&row) else {
            return Ok(());
        };
        self.walk
            .abandon::<LadderRunError>(flight.pending)
            .map_err(|why| ParentError::Walk { symbol: flight.symbol, why })
    }

    /// Releases one child's waiter count on `(row, epoch)` (e.g. on child drop, timeout, or late
    /// wake). Abandons the underlying `Walk` row only when the last waiter leaves.
    pub(crate) fn release_in_flight(&mut self, row: usize, epoch: u64) {
        let drop_it = match self.in_flight.get_mut(&row) {
            Some(flight) if flight.epoch == epoch => {
                flight.waiters = flight.waiters.saturating_sub(1);
                flight.waiters == 0
            }
            _ => false,
        };
        if drop_it && let Some(flight) = self.in_flight.remove(&row) {
            let _ = self.walk.abandon::<LadderRunError>(flight.pending);
        }
    }

    /// Whether `(row, epoch)` is still actively in flight on a worker.
    pub(crate) fn is_in_flight(&self, row: usize, epoch: u64) -> bool {
        self.in_flight.get(&row).is_some_and(|flight| flight.epoch == epoch)
    }

    /// Gathers all unsent datums, transitive declares, named statics, inline byte slices,
    /// `EnteredFn` refs, and datum purity for publishing cached `symbol` to a specific child.
    #[expect(clippy::too_many_arguments, reason = "fills reusable output buffers for one child publication")]
    pub(crate) fn gather_for_child<'s>(
        &'s self,
        symbol: Symbol,
        head: StartHead,
        child: &mut ChildSent<'_>,
        out_declares: &mut AVec<'_, Symbol>,
        out_data: &mut AVec<'_, Datum>,
        out_named: &mut AVec<'_, Symbol>,
        out_inline: &mut AVec<'_, (RecordId, &'s [u8])>,
        out_entered: &mut AVec<'_, &'s CachedEntered>,
        any_unkeyed: &mut bool,
        datums_purity: &mut Result<(), NotKept>,
    ) -> Result<Option<&'s CachedPub>, ParentError> {
        let Some(entry) = self.pubs.get(&symbol.0) else {
            return Ok(None);
        };
        if entry.lands && head.personality.is_none() {
            let path = entry.tabled.and_then(|item| self.shown_of(item)).and_then(|s| s.path().map(Into::into));
            return Err(ParentError::NoPersonality { function: entry.tabled, path, symbol: symbol.0 });
        }
        out_declares.clear();
        out_data.clear();
        out_named.clear();
        out_inline.clear();
        out_entered.clear();
        *any_unkeyed = entry.unkeyed;
        *datums_purity = Ok(());

        out_declares.extend_from_slice(entry.declares.as_slice());
        out_named.extend_from_slice(entry.named.as_slice());
        if let Some(entered) = entry.entered.as_ref() {
            out_entered.push(entered);
        }
        if child.sent_records.insert(entry.record.0) {
            out_inline.push((entry.record, entry.bytes.as_slice()));
        }

        let descriptor = blaze_ladder::symbols::tls_desc_symbol().0;
        let mut idx = 0usize;
        while let Some(&static_sym) = out_named.get(idx) {
            idx += 1;
            if static_sym.0 == descriptor {
                continue;
            }
            if !child.told.insert(static_sym.0) {
                continue;
            }
            let Some(datum) = self.datums.get(&static_sym.0) else {
                // Missing datum: leave for `Needs::publish` to report `StaticUnsent`.
                continue;
            };
            if datum.unkeyed {
                *any_unkeyed = true;
            }
            if datums_purity.is_ok()
                && let Err(why) = datum.purity
            {
                *datums_purity = Err(why);
            }
            out_data.push(Datum { symbol: static_sym, record: datum.record });
            if child.sent_records.insert(datum.record.0) {
                out_inline.push((datum.record, datum.bytes.as_slice()));
            }
            out_declares.extend_from_slice(datum.declares.as_slice());
            out_named.extend_from_slice(datum.named.as_slice());
            if let Some(entered) = datum.entered.as_ref() {
                out_entered.push(entered);
            }
        }
        out_named.sort_unstable();
        out_named.dedup();
        Ok(Some(entry))
    }

    /// Batch-pre-publishes warm functions from `self.warm_order` into `out` via `needs` for an
    /// impure child (`keep.is_err()`). Pure tests never receive pre-published functions so their
    /// `entered_fns` and `BZTR` addresses stay exact and deterministic across runs (rule 25).
    #[expect(clippy::too_many_arguments, reason = "pre-publishes cached suite entries into one impure child's wire buffer")]
    pub(crate) fn prepublish_warm<A: Allocator>(
        &self,
        test_entry: [u8; 32],
        head: StartHead,
        child: &mut ChildSent<'_>,
        needs: &mut Needs<'_>,
        out: &mut crate::run_parent::WireOut<A>,
        tally: &mut Tally,
        scratch_alloc: Alloc<'_>,
    ) -> Result<usize, ParentError> {
        self.prepublish_warm_bounded(test_entry, head, child, needs, out, tally, usize::MAX, scratch_alloc)
    }

    /// Bounded variant of [`Self::prepublish_warm`] that stops before `out.len()` exceeds
    /// `max_out_len`, preflighting every byte of the complete unsent publication (function bytes,
    /// all unsent transitive static datums, and wire framing) without mutating `child` so any
    /// remaining warm functions are published in later turns or served on demand (rule 18).
    #[expect(clippy::too_many_arguments, reason = "pre-publishes cached suite entries into one impure child's wire buffer under a byte cap")]
    pub(crate) fn prepublish_warm_bounded<A: Allocator>(
        &self,
        test_entry: [u8; 32],
        head: StartHead,
        child: &mut ChildSent<'_>,
        needs: &mut Needs<'_>,
        out: &mut crate::run_parent::WireOut<A>,
        tally: &mut Tally,
        max_out_len: usize,
        scratch_alloc: Alloc<'_>,
    ) -> Result<usize, ParentError> {
        let start = child.warm_cursor;
        let total = self.warm_order.len();
        if start >= total {
            return Ok(0);
        }
        let mut declares = Vec::new_in(scratch_alloc);
        let mut data = Vec::new_in(scratch_alloc);
        let mut named = Vec::new_in(scratch_alloc);
        let mut inline = Vec::new_in(scratch_alloc);
        let mut entered = Vec::new_in(scratch_alloc);
        let mut published = 0usize;
        let mut next_cursor = start;
        for &sym in self.warm_order.iter().skip(start) {
            if out.len() >= max_out_len {
                break;
            }
            next_cursor += 1;
            if sym.0 == test_entry || needs.is_settled(&sym) {
                continue;
            }
            let Some(entry) = self.pubs.get(&sym.0) else { continue };
            if entry.lands && head.personality.is_none() {
                continue;
            }
            // Preflight EVERY byte the complete unsent publication (function inline bytes, all
            // unsent transitive static datums, and wire framing) would append BEFORE mutating
            // `child.sent_records` or `child.told`, including on the very first warm entry.
            if max_out_len < usize::MAX {
                let needed = preflight_entry_wire_bytes(entry, &self.datums, child, scratch_alloc);
                if out.len().saturating_add(needed) > max_out_len {
                    next_cursor -= 1;
                    break;
                }
            }
            let mut any_unkeyed = false;
            let mut datums_purity = Ok(());
            let Some(entry) = self.gather_for_child(
                sym,
                head,
                child,
                &mut declares,
                &mut data,
                &mut named,
                &mut inline,
                &mut entered,
                &mut any_unkeyed,
                &mut datums_purity,
            )? else {
                continue;
            };
            if needs.publish_precomputed(
                sym,
                entry.record,
                declares.as_slice(),
                data.as_slice(),
                named.as_slice(),
                entry.values.as_slice(),
                inline.as_slice(),
                out,
            )? {
                published += 1;
                let rows = 1 + data.len();
                tally.reached += rows;
                tally.found += rows;
            }
        }
        child.warm_cursor = next_cursor;
        Ok(published)
    }

    pub(crate) fn read_record(&self, record: &RecordId, into: &mut bstd::mem::Bytes<'_>) -> Result<(), ParentError> {
        let Some(&which) = self.by_record.get(&record.0) else {
            return Err(ParentError::NotStored { symbol: record.0 });
        };
        let bytes = match which {
            CachedRecord::Fn(sym) => self.pubs.get(&sym).map(|p| p.bytes.as_slice()),
            CachedRecord::Datum(sym) => self.datums.get(&sym).map(|d| d.bytes.as_slice()),
        }
        .ok_or(ParentError::NotStored { symbol: record.0 })?;
        into.try_extend(bytes).map_err(|_| ParentError::Sent {
            record: record.0,
            why: crate::run_bytes::SentWhy::NoRoom { len: bytes.len() },
        })
    }

    fn shown_of(&self, item: ItemKey) -> Option<blaze_ladder::front2::item_shown::ItemShown> {
        use blaze_ladder::front2::source::NamesSource;
        let lib = self.names.records.key_beginning(item.0.to_be_bytes())?;
        Some(blaze_ladder::front2::item_shown::ItemShown::of_lib(self.names.records, lib))
    }

    pub(crate) fn name_of(&self, symbol: [u8; 32]) -> Named {
        let Some(row) = self.walk.row_of(&CodeSymbol(symbol)) else {
            return Named::NoRow;
        };
        let tabled = self.walk.tabled(row);
        let name = self.walk.name(row);
        if let Some(shown) = self.maker.as_ref().and_then(|maker| maker.instance_shown(tabled, name)) {
            return Named::Shown { item: shown.item, args: shown.args };
        }
        self.named_from_records(tabled)
    }

    fn named_from_records(&self, item: ItemKey) -> Named {
        match self.shown_of(item) {
            Some(item) => Named::Shown { item, args: None },
            None => Named::Item(item),
        }
    }

    pub(crate) fn site_of(&self, item: ItemKey, site: u32) -> Option<SiteShown> {
        let (file, line, column) = self.maker.as_ref()?.site_shown(item, site)?;
        Some(SiteShown { file: file.into(), line, column })
    }
}

impl Places for SuiteCache<'_> {
    fn named(&mut self, symbol: [u8; 32]) -> Named {
        self.name_of(symbol)
    }

    fn site(&mut self, item: ItemKey, at: u32) -> Option<SiteShown> {
        self.site_of(item, at)
    }

    fn item(&mut self, item: ItemKey) -> Named {
        if let Some(shown) = self.maker.as_ref().and_then(|maker| maker.instance_shown(item, &[])) {
            return Named::Shown { item: shown.item, args: shown.args };
        }
        self.named_from_records(item)
    }
}

/// Answer captured from one `ChildResolver::publication` call for `run_captured`.
pub(crate) enum ChildAnswer {
    Ready {
        symbol: Symbol,
        counts: StepCounts,
    },
    Unresolved(Symbol),
    Waiting(NeedCode),
}

/// Short-lived per-serve [`Resolve`] adapter over [`SuiteCache`] for one child turn.
/// Borrows `cache` strictly inside `publication` (never across `.await`, rule 27).
pub(crate) struct ChildResolver<'r, 'c, 'h, 'a> {
    pub cache: &'r RefCell<SuiteCache<'h>>,
    pub child: &'r mut ChildSent<'a>,
    pub roots: &'r Roots<'a>,
    pub test_entry: [u8; 32],
    pub codes: Codes,
    pub ids: &'r RunIds<'h>,
    pub store: &'r mut dyn CodeStore,
    pub cx: &'r Cx,
    pub watch: &'r dyn Watch,
    pub tally: &'r mut Tally,
    pub keep: &'r mut Result<(), NotKept>,
    pub entered_fns: &'r mut AVec<'c, EnteredFn<'c>>,
    pub cache_alloc: Alloc<'c>,
    pub alloc: Alloc<'a>,
    pub requested: Option<ChildAnswer>,
    // Reusable scratch buffers backing `Resolved::Record` slices.
    declares: AVec<'a, Symbol>,
    data: AVec<'a, Datum>,
    named: AVec<'a, Symbol>,
    sent_records: AVec<'a, ([u8; 32], usize)>,
    sent_bytes: AVec<'a, u8>,
}

impl<'r, 'c, 'h, 'a> ChildResolver<'r, 'c, 'h, 'a> {
    #[expect(clippy::too_many_arguments, reason = "binds one turn's child state to the shared suite cache")]
    pub(crate) fn new(
        cache: &'r RefCell<SuiteCache<'h>>,
        child: &'r mut ChildSent<'a>,
        roots: &'r Roots<'a>,
        test_entry: [u8; 32],
        codes: Codes,
        ids: &'r RunIds<'h>,
        store: &'r mut dyn CodeStore,
        cx: &'r Cx,
        watch: &'r dyn Watch,
        tally: &'r mut Tally,
        keep: &'r mut Result<(), NotKept>,
        entered_fns: &'r mut AVec<'c, EnteredFn<'c>>,
        cache_alloc: Alloc<'c>,
        alloc: Alloc<'a>,
    ) -> ChildResolver<'r, 'c, 'h, 'a> {
        ChildResolver {
            cache,
            child,
            roots,
            test_entry,
            codes,
            ids,
            store,
            cx,
            watch,
            tally,
            keep,
            entered_fns,
            cache_alloc,
            alloc,
            requested: None,
            declares: Vec::new_in(alloc),
            data: Vec::new_in(alloc),
            named: Vec::new_in(alloc),
            sent_records: Vec::new_in(alloc),
            sent_bytes: Vec::new_in(alloc),
        }
    }
}

impl Resolve for ChildResolver<'_, '_, '_, '_> {
    #[cfg(test)]
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        let mut values = Vec::new_in(self.alloc);
        let mut needs_native = false;
        self.publication_with_native(symbol, &mut values, &mut needs_native)
    }

    fn publication(&mut self, symbol: &Symbol, values: &mut AVec<'_, u8>) -> Result<Resolved<'_>, ParentError> {
        let mut needs_native = false;
        self.publication_with_native(symbol, values, &mut needs_native)
    }

    fn publication_with_native(
        &mut self,
        symbol: &Symbol,
        values: &mut AVec<'_, u8>,
        needs_native: &mut bool,
    ) -> Result<Resolved<'_>, ParentError> {
        let mut cache = self.cache.borrow_mut();
        let counts = match cache.ensure_cached(
            *symbol,
            self.test_entry,
            self.codes,
            self.roots,
            self.ids,
            self.store,
            self.cx,
            self.watch,
            self.alloc,
        )? {
            EnsureCached::Ready(counts) => counts,
            EnsureCached::Unknown => {
                self.requested = Some(ChildAnswer::Unresolved(*symbol));
                return Ok(Resolved::Unknown);
            }
            EnsureCached::NeedCode(need) => {
                self.requested = Some(ChildAnswer::Waiting(need));
                return Ok(Resolved::Waiting);
            }
        };
        let mut inline = Vec::new_in(self.alloc);
        let mut entered = Vec::new_in(self.alloc);
        let mut any_unkeyed = false;
        let mut datums_purity = Ok(());
        let head = self.roots.head();
        let Some(entry) = cache.gather_for_child(
            *symbol,
            head,
            self.child,
            &mut self.declares,
            &mut self.data,
            &mut self.named,
            &mut inline,
            &mut entered,
            &mut any_unkeyed,
            &mut datums_purity,
        )? else {
            self.requested = Some(ChildAnswer::Unresolved(*symbol));
            return Ok(Resolved::Unknown);
        };
        values.extend_from_slice(entry.values.as_slice());
        self.sent_records.clear();
        self.sent_bytes.clear();
        for &(rec, bytes) in inline.iter() {
            self.sent_bytes.extend_from_slice(bytes);
            self.sent_records.push((rec.0, self.sent_bytes.len()));
        }
        let rows = 1 + self.data.len();
        self.tally.reached += rows;
        self.tally.made += counts.made;
        self.tally.unkeyed += counts.unkeyed;
        self.tally.found += rows.saturating_sub(counts.made + counts.unkeyed);
        let combined_purity = entry.purity.and(datums_purity);
        if any_unkeyed || combined_purity.is_err() {
            *needs_native = true;
        }
        if self.keep.is_ok() {
            if any_unkeyed {
                *self.keep = Err(NotKept::Untraced);
            } else if let Err(why) = combined_purity {
                *self.keep = Err(why);
            } else {
                for e in entered.iter() {
                    let mut name = Vec::with_capacity_in(e.name.len(), self.cache_alloc);
                    name.extend_from_slice(e.name.as_slice());
                    self.entered_fns.push(EnteredFn {
                        symbol: e.symbol,
                        code_key: e.code_key,
                        tabled: e.tabled,
                        name,
                    });
                }
            }
        }
        let key = entry.key;
        let record = entry.record;
        self.requested = Some(ChildAnswer::Ready { symbol: *symbol, counts });
        Ok(Resolved::Record {
            key,
            record,
            declares: self.declares.as_slice(),
            data: self.data.as_slice(),
            named: self.named.as_slice(),
            sent: Sent::from_parts(self.sent_records.as_slice(), self.sent_bytes.as_slice()),
        })
    }

    fn watch(&self) -> &dyn Watch {
        self.watch
    }
}

/// Computes a strict upper bound on the wire bytes `Needs::publish_precomputed` will append for
/// `entry` — covering the function's inline bytes, every unsent transitive static/vtable datum's
/// inline bytes (including `BYTES_PART_MOST` chunk headers), `Down::Declare`, `Down::Data`,
/// `Down::ThreadLocals`, and `Down::Publish`/`Down::Positioned` — WITHOUT mutating `child`.
fn preflight_entry_wire_bytes(
    entry: &CachedPub,
    datums: &Map<[u8; 32], CachedDatum, BuildKeyHasher, Alloc<'static>>,
    child: &ChildSent<'_>,
    scratch_alloc: Alloc<'_>,
) -> usize {
    const FRAME_OVERHEAD: usize = 96;
    const BYTES_PART_OVERHEAD: usize = 192;
    const DECLARE_PAIR_BYTES: usize = 64;
    const DATA_ITEM_BYTES: usize = 96;
    const TLS_FRAME_BYTES: usize = 256;
    const PUBLISH_BASE_BYTES: usize = 224;

    let part_limit = usize::try_from(BYTES_PART_MOST).unwrap_or(4 << 20);
    let inline_bytes_cost = |len: usize| -> usize {
        if len == 0 {
            return 0;
        }
        let parts = len.div_ceil(part_limit);
        len.saturating_add(parts.saturating_mul(BYTES_PART_OVERHEAD))
    };

    let mut seen_told: Set<[u8; 32], BuildKeyHasher, Alloc<'_>> =
        Set::new(SetOpts { hasher: BuildKeyHasher::default(), alloc: scratch_alloc, capacity: None });
    let mut seen_records: Set<[u8; 32], BuildKeyHasher, Alloc<'_>> =
        Set::new(SetOpts { hasher: BuildKeyHasher::default(), alloc: scratch_alloc, capacity: None });
    let mut work_named: AVec<'_, Symbol> = Vec::with_capacity_in(entry.named.len(), scratch_alloc);
    work_named.extend_from_slice(entry.named.as_slice());

    let mut total = 0usize;
    if !child.sent_records.contains(&entry.record.0) && seen_records.insert(entry.record.0) {
        total = total.saturating_add(inline_bytes_cost(entry.bytes.len()));
    }

    let mut declares_count = 1usize.saturating_add(entry.declares.len());
    let mut data_count = 0usize;
    let mut touches_tls = false;
    let descriptor = blaze_ladder::symbols::tls_desc_symbol().0;
    let mut idx = 0usize;
    while let Some(&static_sym) = work_named.get(idx) {
        idx += 1;
        if static_sym.0 == descriptor {
            touches_tls = true;
            continue;
        }
        if child.told.contains(&static_sym.0) || !seen_told.insert(static_sym.0) {
            continue;
        }
        let Some(datum) = datums.get(&static_sym.0) else {
            continue;
        };
        data_count = data_count.saturating_add(1);
        declares_count = declares_count.saturating_add(datum.declares.len());
        if !child.sent_records.contains(&datum.record.0) && seen_records.insert(datum.record.0) {
            total = total.saturating_add(inline_bytes_cost(datum.bytes.len()));
        }
        work_named.extend_from_slice(datum.named.as_slice());
    }

    total = total.saturating_add(FRAME_OVERHEAD.saturating_add(declares_count.saturating_mul(DECLARE_PAIR_BYTES)));
    if data_count > 0 {
        total = total.saturating_add(FRAME_OVERHEAD.saturating_add(data_count.saturating_mul(DATA_ITEM_BYTES)));
    }
    if touches_tls {
        total = total.saturating_add(TLS_FRAME_BYTES);
    }
    total = total.saturating_add(PUBLISH_BASE_BYTES.saturating_add(entry.values.len()));
    total
}

#[cfg(test)]
mod preflight_tests {
    use super::*;
    use crate::run_parent::wire_out;

    #[test]
    fn preflight_accounts_for_transitive_datums_and_framing_without_mutating_child() {
        let alloc = blaze_own::testing::heap().alloc();
        let fn_sym = Symbol([0x10; 32]);
        let fn_rec = RecordId([0x11; 32]);
        let d1_sym = Symbol([0x20; 32]);
        let d1_rec = RecordId([0x21; 32]);
        let d2_sym = Symbol([0x30; 32]);
        let d2_rec = RecordId([0x31; 32]);

        let mut fn_bytes = Vec::with_capacity_in(512, alloc);
        fn_bytes.resize(512, 0xAA);
        let mut d1_bytes = Vec::with_capacity_in(256 * 1024, alloc);
        d1_bytes.resize(256 * 1024, 0xBB);
        // Second transitive datum spans two `BYTES_PART_MOST` (4 MiB) wire chunks.
        let mut d2_bytes = Vec::with_capacity_in((4 << 20) + 128 * 1024, alloc);
        d2_bytes.resize((4 << 20) + 128 * 1024, 0xCC);

        let mut d1_named = Vec::new_in(alloc);
        d1_named.push(d2_sym);
        let mut datums: Map<[u8; 32], CachedDatum, BuildKeyHasher, Alloc<'static>> =
            Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None });
        datums.insert(
            d1_sym.0,
            CachedDatum {
                record: d1_rec,
                bytes: d1_bytes,
                declares: Vec::new_in(alloc),
                named: d1_named,
                entered: None,
                unkeyed: false,
                purity: Ok(()),
            },
        );
        datums.insert(
            d2_sym.0,
            CachedDatum {
                record: d2_rec,
                bytes: d2_bytes,
                declares: Vec::new_in(alloc),
                named: Vec::new_in(alloc),
                entered: None,
                unkeyed: false,
                purity: Ok(()),
            },
        );

        let mut fn_named = Vec::new_in(alloc);
        fn_named.push(d1_sym);
        let mut fn_declares = Vec::new_in(alloc);
        fn_declares.push(Symbol([0x40; 32]));
        let entry = CachedPub {
            key: CodeKey(blaze_ids::ContentId([0x01; 32])),
            record: fn_rec,
            bytes: fn_bytes,
            declares: fn_declares,
            named: fn_named,
            values: Vec::new_in(alloc),
            lands: false,
            purity: Ok(()),
            entered: None,
            unkeyed: false,
            tabled: None,
        };

        let child = ChildSent::new(alloc);
        let estimated = preflight_entry_wire_bytes(&entry, &datums, &child, alloc);
        // Must NOT mutate `child` and MUST account for both transitive datums (> 4.3 MiB),
        // not just the 512-byte function record.
        assert!(child.told.is_empty());
        assert!(child.sent_records.is_empty());
        assert!(estimated > (4 << 20) + 384 * 1024, "preflight missed transitive datums: {estimated}");

        // Publish the exact same records via `Needs::publish_precomputed` and verify the
        // preflight estimate is a true upper bound (`actual <= estimated`).
        let d1 = datums.get(&d1_sym.0).expect("d1");
        let d2 = datums.get(&d2_sym.0).expect("d2");
        let data = [
            Datum { symbol: d1_sym, record: d1_rec },
            Datum { symbol: d2_sym, record: d2_rec },
        ];
        let named = [d1_sym, d2_sym];
        let inline = [
            (fn_rec, entry.bytes.as_slice()),
            (d1_rec, d1.bytes.as_slice()),
            (d2_rec, d2.bytes.as_slice()),
        ];
        let mut needs = Needs::new(alloc);
        let mut out = wire_out(alloc);
        assert!(
            needs
                .publish_precomputed(
                    fn_sym,
                    fn_rec,
                    entry.declares.as_slice(),
                    &data,
                    &named,
                    entry.values.as_slice(),
                    &inline,
                    &mut out,
                )
                .expect("publish_precomputed")
        );
        assert!(
            out.len() <= estimated,
            "actual encoded wire bytes ({}) exceeded preflight upper bound ({estimated})",
            out.len(),
        );
    }
}
