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
use blaze_hash::{BuildKeyHasher, FixedState, Map, MapOpts, Set, SetOpts};
use blaze_ladder::body_ids::{
    BodyIds as _, DeclaredList, EntryForm, Form as _, How, Make, RowKind, Scratch, Stepped, Unkeyed, Walk, instance_key,
};
use blaze_ladder::code_records::statics::{each_static_named, each_vtable_datum, entry_record};
use blaze_ladder::code_records::{Builder, CodeClass, CodeKey, CodeStore, content_key};
use blaze_ladder::code_records::reads::Verdict;
use blaze_ladder::run::make_one::{MOST_WALKED, MakeWatch, Maker, NamesRecords, SecondCheck};
use blaze_ladder::run::special::{Roots, StartHead};
use blaze_ladder::run::RunError as LadderRunError;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::data::Datum;
use blaze_launch::wire::run::Symbol;
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::plan::NotKept;
use blaze_run::postings::FnName;
use blaze_screen::{Answered, Step, Watch};
use blaze_typeck::ItemKey;
use bstd::Cx;

use crate::run_bytes::Sent;
use crate::run_fault::{Named, Places, SiteShown};
use crate::run_ids::{RunIds, Runnable};
use crate::run_parent::{
    self, FIRST_CALL_SPAN, HOW_FOUND, HOW_MADE, HOW_NO_RECORD, HOW_SPECIAL, HOW_STALE, Needs, ParentError, Resolve,
    Resolved, Start as _, Tally,
};
use crate::test_cache::{EnteredFn, check_record_purity};
use crate::test_drive::TestMaker;

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
    pub pure_toolchain: bool,
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

/// Thin [`Make`] wrapper that consults and updates `SuiteCache::verified` before delegating
/// read-list verification to the suite's single [`TestMaker`].
struct VerifyingMaker<'a, 'h> {
    maker: &'a mut TestMaker<'h>,
    verified: &'a mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    verified_dirty: &'a mut bool,
}

impl Make for VerifyingMaker<'_, '_> {
    type Error = LadderRunError;
    type Grant = Runnable;

    fn make(&mut self, grant: &Self::Grant, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
        self.maker.make(grant, tabled, name, entry)
    }

    fn make_static(&mut self, grant: &Self::Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
        self.maker.make_static(grant, item, entry)
    }

    fn make_thread_local(&mut self, grant: &Self::Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), Self::Error> {
        self.maker.make_thread_local(grant, item, entry)
    }

    fn verify(&mut self, tabled: ItemKey, name: &[u8], entry: &[u8]) -> Result<Option<Verdict>, Self::Error> {
        self.maker.verify(tabled, name, entry)
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
        if self.verified.contains(&pair) {
            return Ok(Some(Verdict::Holds));
        }
        let verdict = self.maker.verify(tabled, name, entry)?;
        if matches!(verdict, Some(Verdict::Holds)) && self.verified.insert(pair) {
            *self.verified_dirty = true;
        }
        Ok(verdict)
    }
}

/// Per-child tracking of which statics/vtables and inline record bytes have been sent to THIS
/// child process.
pub(crate) struct ChildSent<'a> {
    told: Set<[u8; 32], BuildKeyHasher, Alloc<'a>>,
    sent_records: Set<[u8; 32], BuildKeyHasher, Alloc<'a>>,
    /// Index into `SuiteCache::warm_order` already pre-published to this impure child.
    pub warm_cursor: usize,
    /// Whether pure toolchain entries were already batch-flushed to this pure child.
    pub pure_toolchain_flushed: bool,
}

impl<'a> ChildSent<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> ChildSent<'a> {
        ChildSent {
            told: Set::new(SetOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None }),
            sent_records: Set::new(SetOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None }),
            warm_cursor: 0,
            pure_toolchain_flushed: false,
        }
    }
}

/// Shared suite-level resolver state across all tests of one `Suite`.
pub(crate) struct SuiteCache<'h> {
    names: NamesRecords<'h>,
    builder: Builder,
    tables: Alloc<'static>,
    make_watch: MakeWatch<'static>,
    walk: Walk<'static>,
    scratch: Scratch<'static>,
    list: DeclaredList<'static>,
    maker: Option<TestMaker<'h>>,
    verified: Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    verified_dirty: bool,
    pubs: Map<[u8; 32], CachedPub, BuildKeyHasher, Alloc<'static>>,
    datums: Map<[u8; 32], CachedDatum, BuildKeyHasher, Alloc<'static>>,
    by_record: Map<[u8; 32], CachedRecord, BuildKeyHasher, Alloc<'static>>,
    warm_order: AVec<'static, Symbol>,
}

impl<'h> SuiteCache<'h> {
    pub(crate) fn new(
        names: NamesRecords<'h>,
        builder: Builder,
        tables: Alloc<'static>,
        make_watch: MakeWatch<'static>,
    ) -> SuiteCache<'h> {
        SuiteCache {
            names,
            builder,
            tables,
            make_watch,
            walk: Walk::new_in(tables),
            scratch: Scratch::new_in(tables),
            list: DeclaredList::new_in(tables),
            maker: None,
            verified: Set::new(SetOpts { hasher: FixedState, alloc: tables, capacity: None }),
            verified_dirty: false,
            pubs: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: tables, capacity: None }),
            datums: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: tables, capacity: None }),
            by_record: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: tables, capacity: None }),
            warm_order: Vec::new_in(tables),
        }
    }

    pub(crate) fn verified_mut(&mut self) -> &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>> {
        &mut self.verified
    }

    pub(crate) fn verified_dirty(&self) -> bool {
        self.verified_dirty
    }

    pub(crate) fn verified_pairs(&self) -> impl Iterator<Item = ([u8; 32], [u8; 32])> + '_ {
        self.verified.iter().copied()
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
    /// Returns `Ok(None)` when `symbol` is unknown to the program.
    #[expect(clippy::too_many_arguments, reason = "borrows the test's roots, ids, store and trace context")]
    pub(crate) fn ensure_cached(
        &mut self,
        symbol: Symbol,
        test_entry: [u8; 32],
        roots: &Roots<'_>,
        ids: &RunIds<'h>,
        store: &mut dyn CodeStore,
        cx: &Cx,
        watch: &dyn Watch,
        alloc: Alloc<'_>,
    ) -> Result<Option<StepCounts>, ParentError> {
        if self.pubs.contains_key(&symbol.0) {
            return Ok(Some(StepCounts { made: 0, found: 1, unkeyed: 0 }));
        }
        let code_sym = CodeSymbol(symbol.0);
        let head = roots.head();
        let Some(row) = self.walk.row_of(&code_sym) else {
            let Some(made) = roots.special(&code_sym, &self.builder, store, alloc)? else {
                return Ok(None);
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
                pure_toolchain: false,
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
            return Ok(Some(StepCounts { made: usize::from(!made.found), found: usize::from(made.found), unkeyed: 0 }));
        };

        if matches!(self.walk.kind(row), RowKind::Static | RowKind::ThreadLocal) {
            return Ok(None);
        }

        let mut counts = StepCounts::default();
        let tabled = self.walk.tabled(row);
        let (stepped, entry_copy) = self.step_row(row, roots, ids, store, cx, watch, &mut counts)?;
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

        // Cache any vtable datums carried by this entry.
        self.cache_vtables(symbol.0, entry)?;

        // Compute `EnteredFn` metadata once for this function (and record newly made verified pair).
        let entered = if unkeyed_step {
            None
        } else {
            let name_slice = self.walk.name(row);
            let inst_key = instance_key(code_sym, tabled, name_slice, ids, &self.builder, &self.builder.producer, self.tables);
            if let (Stepped::Made(under), Some(ik)) = (stepped, inst_key)
                && self.verified.insert((ik.0.0, under.0.0))
            {
                self.verified_dirty = true;
            }
            inst_key.map(|ik| {
                let mut name_copy = Vec::with_capacity_in(name_slice.len(), self.tables);
                name_copy.extend_from_slice(name_slice);
                CachedEntered { symbol: FnName(symbol.0), code_key: ik.0.0, tabled, name: name_copy }
            })
        };
        let unkeyed = unkeyed_step || entered.is_none();
        let purity = if unkeyed { Err(NotKept::Untraced) } else { check_record_purity(record) };
        let pure_toolchain = !unkeyed && purity.is_ok() && ids.class(tabled) == CodeClass::Toolchain;

        // Resolve and cache every transitive static/thread-local row not already in `self.datums`.
        let mut next = 0usize;
        while let Some(&at) = static_work.get(next) {
            next += 1;
            let static_sym = self.walk.symbol(at).0;
            if self.datums.contains_key(&static_sym) {
                continue;
            }
            let (s_stepped, s_entry_copy) = self.step_row(at, roots, ids, store, cx, watch, &mut counts)?;
            let s_entry = s_entry_copy.as_slice();
            let s_datum = entry_record(s_entry).map_err(|why| ParentError::Entry { symbol: static_sym, why })?;
            let s_record = RecordId(blaze_blake3::hash(s_datum));
            let mut s_bytes = Vec::with_capacity_in(s_datum.len(), self.tables);
            s_bytes.extend_from_slice(s_datum);

            let mut s_declares = Vec::new_in(self.tables);
            for &callee in self.walk.declared(at) {
                let callee = usize::try_from(callee).unwrap_or(0);
                match self.walk.kind(callee) {
                    RowKind::Instance => s_declares.push(Symbol(self.walk.symbol(callee).0)),
                    RowKind::Static | RowKind::ThreadLocal => {
                        let csym = self.walk.symbol(callee).0;
                        if !self.datums.contains_key(&csym) {
                            static_work.push(callee);
                        }
                    }
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
                if let (Stepped::Made(under), Some(ik)) = (s_stepped, ik)
                    && self.verified.insert((ik.0.0, under.0.0))
                {
                    self.verified_dirty = true;
                }
                ik.map(|ik| {
                    let mut name_copy = Vec::with_capacity_in(s_name.len(), self.tables);
                    name_copy.extend_from_slice(s_name);
                    CachedEntered { symbol: FnName(static_sym), code_key: ik.0.0, tabled: s_tabled, name: name_copy }
                })
            };
            let s_unkeyed = s_unkeyed_step || s_entered.is_none();
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
                },
            );
        }

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
                pure_toolchain,
                unkeyed,
                tabled: Some(tabled),
            },
        );
        if symbol.0 != test_entry && self.warm_order.len() < WARM_CAP {
            self.warm_order.push(symbol);
        }
        Ok(Some(counts))
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
                            Space::Outside => {}
                        }
                    }
                }
                named.sort_unstable();
                named.dedup();
                by_record.insert(record.0, CachedRecord::Datum(vtable.0));
                datums.insert(
                    vtable.0,
                    CachedDatum { record, bytes, declares, named, entered: None, unkeyed: false },
                );
                Ok(())
            },
            |why| ParentError::Entry { symbol: owner_symbol, why },
        )
    }

    /// Steps `row` in the shared walk if `How::Waiting`, or reads its stored entry if already done.
    /// Returns `(stepped, entry_bytes)` copied into a temporary buffer so borrows on `self.scratch`
    /// and `store` are released immediately.
    #[expect(clippy::too_many_arguments, reason = "borrows walk state, ids, store and trace context")]
    fn step_row(
        &mut self,
        row: usize,
        roots: &Roots<'_>,
        ids: &RunIds<'h>,
        store: &mut dyn CodeStore,
        cx: &Cx,
        watch: &dyn Watch,
        counts: &mut StepCounts,
    ) -> Result<(Stepped, AVec<'static, u8>), ParentError> {
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
                counts.found += 1;
                Ok((stepped, copy))
            }
            How::Done(Stepped::Unkeyed(_)) | How::Making => Err(ParentError::ResolvedTwice { symbol }),
            How::Waiting => {
                let proof = ids
                    .runnable(tabled)
                    .map_err(|why| ParentError::NotChecked { function: tabled, symbol, why })?;
                let shown = watch.paths().then(|| self.shown_of(tabled)).flatten();
                let path = shown.as_ref().and_then(|s| s.path());
                watch.see(Step::Resolving { path: path.as_deref() });
                let span = cx.span(FIRST_CALL_SPAN);
                let maker = self.maker.get_or_insert_with(|| {
                    let mut maker = Maker::of_roots(self.names, roots.maker_entry(), roots, SecondCheck, &self.builder, self.tables);
                    maker.watch(self.make_watch);
                    maker
                });
                let mut vm = VerifyingMaker {
                    maker,
                    verified: &mut self.verified,
                    verified_dirty: &mut self.verified_dirty,
                };
                let stepped = self
                    .walk
                    .one(row, &ids, store, &self.builder, &self.builder.producer, &mut vm, &proof, &EntryForm, MOST_WALKED, &mut self.scratch)
                    .map_err(|why| ParentError::Walk { symbol, why })?
                    .stepped;
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
                Ok((stepped, copy))
            }
        }
    }

    /// Gathers all unsent datums, transitive declares, named statics, inline byte slices, and
    /// `EnteredFn` refs for publishing cached `symbol` to a specific child.
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

    /// Batch-pre-publishes warm functions from `self.warm_order` into `out` via `needs`.
    /// When `only_pure_toolchain` is true, only keyed pure `CodeClass::Toolchain` functions are
    /// pre-published and their `EnteredFn` metadata is appended to `entered_fns`.
    #[expect(clippy::too_many_arguments, reason = "pre-publishes cached suite entries into one child's wire buffer")]
    pub(crate) fn prepublish_warm<'c, A: Allocator>(
        &self,
        test_entry: [u8; 32],
        head: StartHead,
        only_pure_toolchain: bool,
        child: &mut ChildSent<'_>,
        needs: &mut Needs<'_>,
        out: &mut crate::run_parent::WireOut<A>,
        entered_fns: &mut AVec<'c, EnteredFn<'c>>,
        cache_alloc: Alloc<'c>,
        tally: &mut Tally,
        scratch_alloc: Alloc<'_>,
    ) -> Result<usize, ParentError> {
        let start = if only_pure_toolchain { 0 } else { child.warm_cursor };
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
        for &sym in self.warm_order.iter().skip(start) {
            if sym.0 == test_entry || needs.is_settled(&sym) {
                continue;
            }
            let Some(entry) = self.pubs.get(&sym.0) else { continue };
            if only_pure_toolchain && !entry.pure_toolchain {
                continue;
            }
            if entry.lands && head.personality.is_none() {
                continue;
            }
            let mut any_unkeyed = false;
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
                if only_pure_toolchain {
                    for e in entered.iter() {
                        let mut name = Vec::with_capacity_in(e.name.len(), cache_alloc);
                        name.extend_from_slice(e.name.as_slice());
                        entered_fns.push(EnteredFn {
                            symbol: e.symbol,
                            code_key: e.code_key,
                            tabled: e.tabled,
                            name,
                        });
                    }
                }
            }
        }
        if only_pure_toolchain {
            child.pure_toolchain_flushed = true;
        } else {
            child.warm_cursor = total;
        }
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
        purity: Result<(), NotKept>,
        any_unkeyed: bool,
        counts: StepCounts,
    },
    Unresolved(Symbol),
}

/// Short-lived per-serve [`Resolve`] adapter over [`SuiteCache`] for one child turn.
/// Borrows `cache` strictly inside `publication` (never across `.await`, rule 27).
pub(crate) struct ChildResolver<'r, 'c, 'h, 'a> {
    pub cache: &'r RefCell<SuiteCache<'h>>,
    pub child: &'r mut ChildSent<'a>,
    pub roots: &'r Roots<'a>,
    pub test_entry: [u8; 32],
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
        self.publication(symbol, &mut values)
    }

    fn publication(&mut self, symbol: &Symbol, values: &mut AVec<'_, u8>) -> Result<Resolved<'_>, ParentError> {
        let mut cache = self.cache.borrow_mut();
        let Some(counts) = cache.ensure_cached(
            *symbol,
            self.test_entry,
            self.roots,
            self.ids,
            self.store,
            self.cx,
            self.watch,
            self.alloc,
        )? else {
            self.requested = Some(ChildAnswer::Unresolved(*symbol));
            return Ok(Resolved::Unknown);
        };
        let mut inline = Vec::new_in(self.alloc);
        let mut entered = Vec::new_in(self.alloc);
        let mut any_unkeyed = false;
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
        if self.keep.is_ok() {
            if any_unkeyed {
                *self.keep = Err(NotKept::Untraced);
            } else if let Err(why) = entry.purity {
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
        let purity = entry.purity;
        self.requested = Some(ChildAnswer::Ready { symbol: *symbol, purity, any_unkeyed, counts });
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
