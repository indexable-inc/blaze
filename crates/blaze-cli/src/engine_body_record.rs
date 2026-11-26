//! A BODY'S KEPT CODE, as a store record (run.md, section R, piece R4; RV3 job 4; D119).
//!
//! The Bodies stage leaves, beside each clean body's facts record (engine_body_facts.rs), its
//! checked generic body in blaze-body's flat form with the portable type table it indexes: the
//! bytes blaze-body kept.rs writes (front2 tables/task.rs `keep_checked` makes them). A run
//! builds a called instance from it (`blaze_body_build::build(kept.verified(), kept.tys(), ..)`)
//! and lowers nothing again. This file is the one home of the record's key, its put and its
//! reader (rule 30).
//!
//! THE KEY IS THE CODE'S, NOT THE VERDICT'S. [`key_of`] hashes the record's bytes under its own
//! form, so it covers every input of the value (rule 21: the body's rows, its table, the
//! lowering's facts as they shaped the rows), and two bodies with one clean verdict (`return 1`,
//! `return 2`) are two keys. The verdict cell (`VERDICT_CELL`) is unchanged: it is what the
//! facts record and the row carry; this key is a separate code input beside it.
//!
//! READ ONCE, IN PLACE. [`read`] takes the record where the store's mapping holds it
//! (`Held::place`, as engine_records.rs reads), checks that its bytes are the key's (a record
//! under another key is refused), and opens it with `blaze_body::kept::open`, whose [`Kept`]
//! carries the body's `Verified` proof: nothing reads it unchecked or checks it twice (rule 74).
//! A record put by this command and not committed yet is not in place, and is refused by name
//! (rule 3: never copied behind the reader's back).

use core::alloc::Allocator;
use core::fmt;

use blaze_body::{Kept, KeptError};
use blaze_drive::{Key, Name};
use bstd::cx::Cx;

use crate::CliError;
use crate::store::Store;

/// The form of a kept body's key: raised with `blaze_body::KEPT_VERSION` or the body's
/// `blaze_body::VERSION`, so a record of another form is never found under a new key.
const KEPT_FORM: &str = "blaze-cli engine kept body key 1";

/// The store key of the kept body record `bytes`: their hash under [`KEPT_FORM`], so equal
/// bytes meet under one key (rule 25) and other bytes never do (rule 21).
pub(crate) fn key_of(bytes: &[u8]) -> Key {
    let mut h = blaze_ids::Hasher::new(KEPT_FORM);
    h.update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
    Key(h.finish())
}

/// Why body `body`'s kept record could not be put or read. Each names the body and the key
/// (rule 19) and says which check refused.
#[derive(Debug)]
pub(crate) enum BodyRecordError {
    /// The store refused the put or the get.
    Store { body: Name, key: Key, why: CliError },
    /// No record is at the key the body's facts name: the store lost a record its answer
    /// names, never a cue to lower the body again (rule 3).
    Missing { body: Name, key: Key },
    /// The record is this command's own put, not yet committed, or not aligned for a read in
    /// place.
    NotInPlace { body: Name, key: Key, why: blaze_cas::plain::NotInPlace },
    /// The bytes at `key` are not the bytes `key` names.
    WrongBytes { body: Name, key: Key, found: Key },
    /// The bytes are not a kept body (blaze-body kept.rs names which check).
    Kept { body: Name, key: Key, why: KeptError },
}

impl fmt::Display for BodyRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyRecordError::Store { body, key, why } => write!(f, "{body:?}: its kept body {key:?} could not be put or read: {why} (rule: engine_body_record.rs)"),
            BodyRecordError::Missing { body, key } => write!(f, "{body:?}: its kept body {key:?} is not in the store (rule: engine_body_record.rs)"),
            BodyRecordError::NotInPlace { body, key, why } => write!(f, "{body:?}: its kept body {key:?} is not read in place: {why} (rule: engine_body_record.rs)"),
            BodyRecordError::WrongBytes { body, key, found } => write!(f, "{body:?}: the record at {key:?} is another body's ({found:?}) (rule 21)"),
            BodyRecordError::Kept { body, key, why } => write!(f, "{body:?}: the record at {key:?} is not a kept body: {why:?} (rule: blaze-body kept.rs)"),
        }
    }
}

/// Puts body `body`'s kept record `bytes` under [`key_of`] and files the key beside `owner`,
/// the body's facts record (engine_check.rs, the Bodies put), so a root that keeps the facts
/// keeps the code (store.rs `index_beside`). Equal bytes already held are left as they are.
/// The key, for the facts record to name.
pub(crate) fn put(cx: &Cx, store: &mut dyn Store<'_>, body: Name, owner: Key, bytes: &[u8]) -> Result<Key, BodyRecordError> {
    let key = key_of(bytes);
    let cx = cx.span("engine: a body's kept code put");
    store.put(&cx, key, bytes).map_err(|why| BodyRecordError::Store { body, key, why })?;
    store.index_beside(&cx, owner, &[key]).map_err(|why| BodyRecordError::Store { body, key, why })?;
    Ok(key)
}

/// Body `body`'s kept record at `key`, read in place and opened: its table decoded into
/// `alloc`, its body verified once ([`Kept::verified`]).
pub(crate) fn read<'m, A: Allocator + Copy>(cx: &Cx, store: &mut dyn Store<'m>, body: Name, key: Key, alloc: A) -> Result<Kept<'m, A>, BodyRecordError> {
    let cx = cx.span("engine: a body's kept code read");
    let held = store.get(&cx, key).map_err(|why| BodyRecordError::Store { body, key, why })?;
    let Some(held) = held else { return Err(BodyRecordError::Missing { body, key }) };
    let place = held.place.map_err(|why| BodyRecordError::NotInPlace { body, key, why })?;
    open(body, key, place.bytes(), alloc)
}

/// The kept body in `bytes`, found at `key`: refused when the bytes are not `key`'s, then
/// opened by blaze-body's one reader.
pub(crate) fn open<'b, A: Allocator + Copy>(body: Name, key: Key, bytes: &'b [u8], alloc: A) -> Result<Kept<'b, A>, BodyRecordError> {
    let found = key_of(bytes);
    if found != key {
        return Err(BodyRecordError::WrongBytes { body, key, found });
    }
    blaze_body::kept::open(bytes, alloc).map_err(|why| BodyRecordError::Kept { body, key, why })
}

// ---- THE TYPED RECORD: a body's checked columns (D163 part 2, D159 "one body record") ------
//
// What a run reads in place of checking a body again: blaze-typeck checked_record.rs's record
// of the body's clean `Checked` (`encode`, `decode`). THE KEY (rule 21, revised 2026-11-20):
// the record's OWN BYTES, hashed under [`TYPED_FORM`] ([`typed_key`]), so two values can never
// meet under one key. The first key, (the check's name, the row's `out`), missed an input: a
// clean body's `out` is its verdict's digest, which does not move when an edit adds an
// expression and leaves the verdict clean (`fn two() -> u8 { 2 }` to `{ let _ = 0u8; 2 }`),
// while the record's expression column does; the second command's put was refused with
// `TwoValues { differs_at: 24 }`, the record's `ExprTy` row count (blaze-cli tests, wave-1119
// t26). How a reader finds the key: the body's run facts record names it (engine_body_facts.rs
// `BodyFacts::typed`), and the facts record is keyed by its bytes (engine_check.rs, the Bodies
// put), so the facts record moves with the typed record and an unchanged one is found by its key.
//
// WHICH BODIES HAVE ONE (D197 root 1): every workspace root body whose types `check` found
// clean, whatever its borrows say (front2 tables/task.rs `keeps_typed`). A body of a kind with
// no record yet (a toolchain's or a dependency's body, a fn declared in a body, a body whose
// stalled goals were decided after its record) is the run's own check, named and counted by
// kind ([`StoreTyped`]'s `source`), never a refusal and never a second check of a body that
// has a record.

/// The form of a typed record's key: raised with blaze-typeck's `CHECKED_RECORD_MAGIC`, so a
/// record of another form is never found under a new key. Form 2: the key of the bytes (form 1
/// keyed (check, out), which missed the record's columns; see THE KEY above).
const TYPED_FORM: &str = "blaze-cli engine typed body key 2";

/// The store key of the typed record `bytes`: their length and every byte hashed under
/// [`TYPED_FORM`], so equal records meet under one key (rule 25) and other bytes never do (rule
/// 21). The put (engine_bodies.rs, the Bodies stage) calls it; a reader takes the key from the
/// body's run facts ([`StoreTyped`]).
pub(crate) fn typed_key(bytes: &[u8]) -> Key {
    let mut h = blaze_ids::Hasher::new(TYPED_FORM);
    h.update(&(bytes.len() as u64).to_le_bytes()).update(bytes);
    Key(h.finish())
}

/// Encodes a body's raw `Checked` record bytes through the selective `BZR1` fast-Zstd envelope
/// (`>= 4 KiB` and `< 85%` of raw size -> compressed; small/incompressible -> raw zero-copy) and
/// computes its content key [`typed_key`]. Deterministic across runs (rule 25).
#[allow(dead_code, reason = "called by Bodies stage workers to compress large TypedBody puts")]
pub(crate) fn prepare_typed_in<'a, A: Allocator + Copy>(
    raw: &'a [u8],
    alloc: A,
) -> Result<(Key, crate::store::record_codec::Encoded<'a, A>), crate::store::record_codec::RecordCodecError> {
    let encoded = crate::store::record_codec::encode_selective_in(raw, alloc)?;
    let key = typed_key(encoded.as_slice());
    Ok((key, encoded))
}

/// Why the store did not hand a body's typed record over: names the body's check and the key.
#[derive(Debug)]
pub(crate) enum TypedReadError {
    /// The command's store refused the get of `key` (its facts record or its typed record).
    Store { body: Name, key: Key, why: blaze_cas::plain::LentError },
    /// The read-only store snapshot refused the get of `key` on a code worker.
    Snapshot { body: Name, key: Key, why: blaze_cas::plain::SnapshotError },
    /// The body's run facts record, which names the typed record, did not read: what refused it.
    Facts { body: Name, what: &'static str },
    /// The facts name a typed record the store does not hold: a record lost, never a cue to
    /// check the body again (rule 3).
    Missing { body: Name, key: Key },
    /// The `BZR1` Zstd envelope wrapping the typed record failed validation or decompression.
    Decompress { body: Name, key: Key, why: crate::store::record_codec::RecordCodecError },
}

impl fmt::Display for TypedReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypedReadError::Store { body, key, why } => write!(f, "{body:?}: its record {key:?} could not be read: {why} (rule: engine_body_record.rs)"),
            TypedReadError::Snapshot { body, key, why } => write!(f, "{body:?}: its record {key:?} could not be read from the store snapshot: {why} (rule: engine_body_record.rs)"),
            TypedReadError::Facts { body, what } => write!(f, "{body:?}: its run facts record, which names its typed record, could not be read: {what} (engine_body_facts.rs, engine_body_record.rs)"),
            TypedReadError::Missing { body, key } => write!(f, "{body:?}: its run facts name the typed record {key:?}, which the store does not hold (rule 3: engine_body_record.rs)"),
            TypedReadError::Decompress { body, key, why } => write!(f, "{body:?}: its typed record {key:?} failed BZR1 decompression: {why} (rule 21: engine_body_record.rs)"),
        }
    }
}

impl core::error::Error for TypedReadError {}

/// What the run's typed reader is made of besides the body ids: the parent hands it the ids at
/// the maker's making (run_parent.rs `CommandFront::maker`), when they exist.
#[derive(Clone, Copy)]
pub(crate) struct TypedFrom<'h> {
    pub(crate) cx: &'h Cx,
    /// The command's store, read through its lent handle: this command's own puts (a check in
    /// the same command) and the committed generation alike (blaze-cas plain/lent.rs `get`).
    pub(crate) lent: &'h crate::store::CommandLent,
    /// Where each row's record is: a body's is its run facts record (engine_check.rs
    /// `Standing::records`).
    pub(crate) records: &'h dyn crate::engine_run_answer::Records,
    /// Where each decoded `Checked` lives: the run's (rule 75).
    pub(crate) alloc: blaze_own::Alloc<'static>,
}

impl<'h> TypedFrom<'h> {
    /// The reader over the run's body ids `ids` (a second view of the parent's table, no row
    /// copied: run_ids.rs `RunIds` is `Copy`).
    pub(crate) fn over(self, ids: crate::run_ids::RunIds<'h>) -> StoreTyped<'h> {
        StoreTyped {
            from: self,
            ids,
            kept_program: blaze_own::Vec::new_in(self.alloc),
            kept_toolchain: blaze_own::Vec::new_in(self.alloc),
            persist_error: None,
        }
    }

    /// `Send + Sync` worker-side handle over the standing answer's `Records` and immutable
    /// [`IdsLookup`](crate::run_ids::IdsLookup), paired per-worker with the suite's
    /// [`StoreSnapshot`](crate::store::StoreSnapshot) (zero lock contention on reads).
    pub(crate) fn sync_over(self, ids: crate::run_ids::RunIds<'h>) -> SyncTypedFrom<'h> {
        SyncTypedFrom { records: self.records, ids: ids.lookup() }
    }
}

/// One newly typechecked dependency or toolchain body encoded into a `BZR1`-selective buffer on
/// a parallel export worker, awaiting main-thread `Lent::put` and root registration.
pub(crate) struct PendingTypedPut {
    pub(crate) key: Key,
    pub(crate) class: blaze_ladder::code_records::CodeClass,
    pub(crate) bytes: blaze_own::Vec<u8, blaze_own::SharedAlloc<'static>>,
}

/// Per-job batch of encoded dependency/toolchain typed-body puts and program-class cache hits
/// drained from a parallel export worker's [`SnapshotTyped`] into `ExportDone`.
pub(crate) struct WorkerTypedBatch {
    pub(crate) puts: blaze_own::Vec<PendingTypedPut, blaze_own::SharedAlloc<'static>>,
    pub(crate) hit_program: blaze_own::Vec<blaze_cas::Key, blaze_own::SharedAlloc<'static>>,
}

impl WorkerTypedBatch {
    fn new_in(shared: blaze_own::SharedAlloc<'static>) -> Self {
        Self {
            puts: blaze_own::Vec::new_in(shared),
            hit_program: blaze_own::Vec::new_in(shared),
        }
    }

    fn is_empty(&self) -> bool {
        self.puts.is_empty() && self.hit_program.is_empty()
    }
}

/// Puts an encoded dependency/toolchain typed-body record into `lent` under `key` if absent,
/// returning `Ok(true)` when a new write occurred and `Ok(false)` when already stored.
/// Refuses loudly on any store error (including `TwoValues`, rules 3 and 21).
pub(crate) fn put_dep_typed_if_absent<D: blaze_core::Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::plain_place::StoreAlloc>,
    key: Key,
    bytes: &[u8],
) -> Result<bool, blaze_cas::plain::LentError> {
    let cas_key = blaze_cas::Key(key.0);
    if lent.tag_of(&cas_key)?.is_some() {
        return Ok(false);
    }
    lent.put(
        &cas_key,
        blaze_keyed::kind::Namespace::LOCAL,
        blaze_keyed::kind::ReservedTag::Drive.tag(),
        bytes,
    )?;
    Ok(true)
}

/// `Send + Sync` typed-record lookup inputs shared read-only across all `CodePool` workers.
#[derive(Clone, Copy)]
pub(crate) struct SyncTypedFrom<'h> {
    pub(crate) records: &'h dyn crate::engine_run_answer::Records,
    pub(crate) ids: crate::run_ids::IdsLookup<'h>,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SyncTypedFrom<'static>>();
    fn assert_send<T: Send>() {}
    assert_send::<WorkerTypedBatch>();
};

impl<'h> SyncTypedFrom<'h> {
    /// Constructs a lock-free [`SnapshotTyped`] reader on a code worker over its `cx`, the
    /// suite's post-check [`StoreSnapshot`](crate::store::StoreSnapshot), and its table allocator.
    pub(crate) fn reader<'w>(
        self,
        cx: &'w Cx,
        snapshot: &'w crate::store::StoreSnapshot,
        alloc: blaze_own::Alloc<'static>,
    ) -> SnapshotTyped<'w>
    where
        'h: 'w,
    {
        SnapshotTyped { cx, snapshot, records: self.records, ids: self.ids, alloc, pending: None }
    }

    /// Constructs a lock-free [`SnapshotTyped`] reader on a parallel export worker that also
    /// buffers newly typechecked dependency/toolchain `Checked` records and program-class cache
    /// hits in `shared` for main-thread CAS persistence and GC rooting.
    pub(crate) fn reader_persisting<'w>(
        self,
        cx: &'w Cx,
        snapshot: &'w crate::store::StoreSnapshot,
        alloc: blaze_own::Alloc<'static>,
        shared: blaze_own::SharedAlloc<'static>,
    ) -> SnapshotTyped<'w>
    where
        'h: 'w,
    {
        SnapshotTyped {
            cx,
            snapshot,
            records: self.records,
            ids: self.ids,
            alloc,
            pending: Some((shared, core::cell::RefCell::new(WorkerTypedBatch::new_in(shared)))),
        }
    }
}

/// Lock-free read-only [`KeptBodies`](blaze_ladder::front2::program::lazy::typed::KeptBodies)
/// reader for parallel `CodePool` workers: reads both committed segments and uncommitted check
/// puts (`Visibility::InArena`) straight from [`StoreSnapshot`](crate::store::StoreSnapshot) with
/// zero locks, avoiding redundant workspace, dependency, and toolchain re-typechecking on worker
/// threads. When constructed via [`SyncTypedFrom::reader_persisting`], newly checked dependency
/// and toolchain bodies are encoded into [`WorkerTypedBatch`] for coordinator persistence.
pub(crate) struct SnapshotTyped<'h> {
    cx: &'h Cx,
    snapshot: &'h crate::store::StoreSnapshot,
    records: &'h dyn crate::engine_run_answer::Records,
    ids: crate::run_ids::IdsLookup<'h>,
    alloc: blaze_own::Alloc<'static>,
    pending: Option<(blaze_own::SharedAlloc<'static>, core::cell::RefCell<WorkerTypedBatch>)>,
}

impl SnapshotTyped<'_> {
    /// Drains any pending dep/toolchain typed-body puts and program-class cache hits accumulated
    /// since the previous drain. Returns `None` when nothing was queued (zero allocation).
    pub(crate) fn take_pending(&self) -> Option<WorkerTypedBatch> {
        let (shared, batch_cell) = self.pending.as_ref()?;
        let mut batch = batch_cell.borrow_mut();
        if batch.is_empty() {
            return None;
        }
        Some(core::mem::replace(&mut *batch, WorkerTypedBatch::new_in(*shared)))
    }
}

/// THE READER of the typed records, kind by kind (blaze-ladder lazy/typed.rs `KeptBodies`), what
/// the run's maker reads through (run_parent.rs `RunMaker`, blaze-ladder make_one.rs `ByKind`):
///
/// - a non-local toolchain or dependency body (`RunIds::grain`, `IdsLookup::dep_typed_key`):
///   probes the CAS under its deterministic input-complete key; reads `Checked` on hit, or checks
///   once on miss and persists the `BZR1`-selective `Checked` record via `keep_checked`;
/// - a fn declared in a body (its key's inner bit, typed.rs `is_local`): checked by the run
///   because its check is its root's and reading the root's record under the local's key would be
///   two values under one key (rule 21);
/// - a workspace root body: its row (`RunIds::check_of`), the check's run facts record where the
///   answer names it (`Records::record`), the typed record's key the facts hold
///   (engine_body_facts.rs `BodyFacts::typed`), the record from the store, decoded once
///   (blaze-typeck `checked_record::decode`, which refuses bytes of another form by name). Facts
///   that name no typed record are `RunChecked::Unrecorded`, the run's own check; a row, a facts
///   record or a named record that is not there is a typed gap (rule 3: never a check in its
///   place).
///
/// A body local to another as a closure or an async block is never asked: its columns are its
/// owner's (typed.rs, "Body-local items").
pub(crate) struct StoreTyped<'h> {
    from: TypedFrom<'h>,
    ids: crate::run_ids::RunIds<'h>,
    kept_program: blaze_own::Vec<blaze_cas::Key, blaze_own::Alloc<'static>>,
    kept_toolchain: blaze_own::Vec<blaze_cas::Key, blaze_own::Alloc<'static>>,
    persist_error: Option<blaze_cas::plain::LentError>,
}

impl StoreTyped<'_> {
    /// Program-class (`Grain::Dependency`) typed-body keys read or written by this reader, for
    /// `keep_program` GC rooting before store commit.
    pub(crate) fn kept_program(&self) -> &[blaze_cas::Key] {
        self.kept_program.as_slice()
    }

    /// Newly written toolchain-class (`Grain::Toolchain`) typed-body keys, for `keep_toolchain`
    /// GC rooting before store commit.
    pub(crate) fn kept_toolchain(&self) -> &[blaze_cas::Key] {
        self.kept_toolchain.as_slice()
    }

    /// First store error encountered while persisting a dep/toolchain typed record, if any.
    pub(crate) fn persist_error(&self) -> Option<&blaze_cas::plain::LentError> {
        self.persist_error.as_ref()
    }
}

/// Single shared decode path for [`StoreTyped`] and [`SnapshotTyped`] (rule 30).
fn decode_body_source<'s>(
    cx: &Cx,
    ids: crate::run_ids::IdsLookup<'_>,
    records: &dyn crate::engine_run_answer::Records,
    alloc: blaze_own::Alloc<'static>,
    key: blaze_typeck::ItemKey,
    mut get: impl FnMut(Name, Key) -> Result<Option<&'s [u8]>, TypedReadError>,
    mut on_dep_hit: impl FnMut(Key, blaze_ladder::code_records::CodeClass),
) -> Result<blaze_ladder::front2::program::lazy::typed::BodySource, blaze_ladder::front2::program::lazy::typed::TypedGap> {
    use blaze_ladder::front2::program::lazy::typed::{BodySource, RunChecked, TypedGap, is_local};
    use crate::run_ids::Grain;
    let grain = match ids.grain(key) {
        Some(grain) => grain,
        // No crate row: the run gate refused it before any make (run_ids.rs `runnable`).
        None => return Err(TypedGap::NoRecord { key }),
    };
    if is_local(key) {
        return Ok(BodySource::Run(match grain {
            Grain::Toolchain => RunChecked::Toolchain,
            Grain::Dependency => RunChecked::Dependency,
            Grain::Workspace => RunChecked::Local,
        }));
    }
    let read = |why: TypedReadError| TypedGap::Read { key, why: Box::new(why) };
    match grain {
        Grain::Toolchain | Grain::Dependency => {
            let run_kind = match grain {
                Grain::Toolchain => RunChecked::Toolchain,
                Grain::Dependency => RunChecked::Dependency,
                Grain::Workspace => unreachable!(),
            };
            let Some((dep_key, class)) = ids.dep_typed_key(key) else {
                return Ok(BodySource::Run(run_kind));
            };
            let check = Name::new(blaze_drive::Kind::BodyCheck, dep_key.0);
            let _cx = cx.span("engine: a dep/toolchain body's typed record read");
            let Some(bytes) = get(check, dep_key).map_err(read)? else {
                return Ok(BodySource::Run(run_kind));
            };
            let decoded = crate::store::record_codec::decode_if_envelope_in(
                bytes,
                crate::store::record_codec::DEFAULT_MAX_DECODED_BYTES,
                alloc,
            )
            .map_err(|why| read(TypedReadError::Decompress { body: check, key: dep_key, why }))?;
            let checked = blaze_typeck::checked_record::decode(decoded.as_slice(), alloc)
                .map_err(|why| TypedGap::Record { key, why })?;
            on_dep_hit(dep_key, class);
            return Ok(BodySource::Read(checked));
        }
        Grain::Workspace => {}
    }
    let facts_refused = |check: Name, what: &'static str| read(TypedReadError::Facts { body: check, what });
    let Some(check) = ids.check_of(key) else { return Err(TypedGap::NoRecord { key }) };
    let Some(facts_at) = records.record(check).map_err(|what| facts_refused(check, what))? else { return Err(TypedGap::NoRecord { key }) };
    let _cx = cx.span("engine: a body's typed record read");
    // The facts record: a plain drive record whose payload is the run facts (engine_check.rs,
    // the Bodies put), which name the typed record by its key.
    let Some(facts) = get(check, facts_at).map_err(read)? else { return Err(facts_refused(check, "the answer names a facts record the store does not hold")) };
    let whole = match blaze_drive::record::open(facts) {
        Ok(blaze_drive::record::Stored::Whole(whole)) => whole,
        Ok(blaze_drive::record::Stored::Shape(_)) => return Err(facts_refused(check, "its facts record is a shape record, which holds no payload")),
        Err(_) => return Err(facts_refused(check, "its facts record is not a drive record")),
    };
    let facts = crate::engine_run_answer::facts_of(check, whole.payload).map_err(|what| facts_refused(check, what))?;
    let typed = crate::engine_body_facts::read(facts).map_err(|_| facts_refused(check, "its facts payload does not read (engine_body_facts.rs `read`)"))?.typed;
    // `check` ran and kept no record of it: the run's one check, counted.
    let Some(at) = typed else { return Ok(BodySource::Run(RunChecked::Unrecorded)) };
    let Some(bytes) = get(check, at).map_err(read)? else { return Err(read(TypedReadError::Missing { body: check, key: at })) };
    let decoded = crate::store::record_codec::decode_if_envelope_in(
        bytes,
        crate::store::record_codec::DEFAULT_MAX_DECODED_BYTES,
        alloc,
    )
    .map_err(|why| read(TypedReadError::Decompress { body: check, key: at, why }))?;
    blaze_typeck::checked_record::decode(decoded.as_slice(), alloc).map(BodySource::Read).map_err(|why| TypedGap::Record { key, why })
}

impl blaze_ladder::front2::program::lazy::typed::KeptBodies for StoreTyped<'_> {
    fn source(&mut self, key: blaze_typeck::ItemKey) -> Result<blaze_ladder::front2::program::lazy::typed::BodySource, blaze_ladder::front2::program::lazy::typed::TypedGap> {
        let lent = self.from.lent;
        let kept_program = &mut self.kept_program;
        decode_body_source(
            self.from.cx,
            self.ids.lookup(),
            self.from.records,
            self.from.alloc,
            key,
            |check, at| match lent.get(&blaze_cas::Key(at.0)) {
                Ok(found) => Ok(found.map(|lend| lend.bytes)),
                Err(why) => Err(TypedReadError::Store { body: check, key: at, why }),
            },
            |dep_key, class| {
                if class == blaze_ladder::code_records::CodeClass::Program {
                    kept_program.push(blaze_cas::Key(dep_key.0));
                }
            },
        )
    }

    fn keep_checked(
        &mut self,
        key: blaze_typeck::ItemKey,
        kind: blaze_ladder::front2::program::lazy::typed::RunChecked,
        checked: &blaze_typeck::Checked<'static>,
    ) {
        use blaze_ladder::code_records::CodeClass;
        use blaze_ladder::front2::program::lazy::typed::RunChecked;
        if !matches!(kind, RunChecked::Toolchain | RunChecked::Dependency) {
            return;
        }
        let Some((dep_key, class)) = self.ids.lookup().dep_typed_key(key) else {
            return;
        };
        let cas_key = blaze_cas::Key(dep_key.0);
        match self.from.lent.tag_of(&cas_key) {
            Ok(Some(_)) => {
                if class == CodeClass::Program {
                    self.kept_program.push(cas_key);
                }
                return;
            }
            Ok(None) => {}
            Err(why) => {
                if self.persist_error.is_none() {
                    self.persist_error = Some(why);
                }
                return;
            }
        }
        let _span = self.from.cx.span("engine: persist dep/toolchain typed record");
        let Ok(raw) = blaze_typeck::checked_record::encode(checked, self.from.alloc) else {
            return;
        };
        let Ok(encoded) = crate::store::record_codec::encode_selective_in(raw.as_slice(), self.from.alloc) else {
            return;
        };
        match put_dep_typed_if_absent(self.from.lent, dep_key, encoded.as_slice()) {
            Ok(wrote) => match class {
                CodeClass::Program => self.kept_program.push(cas_key),
                CodeClass::Toolchain if wrote => self.kept_toolchain.push(cas_key),
                CodeClass::Toolchain => {}
            },
            Err(why) => {
                if self.persist_error.is_none() {
                    self.persist_error = Some(why);
                }
            }
        }
    }
}

impl blaze_ladder::front2::program::lazy::typed::KeptBodies for SnapshotTyped<'_> {
    fn source(&mut self, key: blaze_typeck::ItemKey) -> Result<blaze_ladder::front2::program::lazy::typed::BodySource, blaze_ladder::front2::program::lazy::typed::TypedGap> {
        let snapshot = self.snapshot;
        let pending = self.pending.as_ref();
        decode_body_source(
            self.cx,
            self.ids,
            self.records,
            self.alloc,
            key,
            |check, at| snapshot.get(at).map_err(|why| TypedReadError::Snapshot { body: check, key: at, why }),
            |dep_key, class| {
                if class == blaze_ladder::code_records::CodeClass::Program
                    && let Some((_, batch)) = pending
                {
                    batch.borrow_mut().hit_program.push(blaze_cas::Key(dep_key.0));
                }
            },
        )
    }

    fn keep_checked(
        &mut self,
        key: blaze_typeck::ItemKey,
        kind: blaze_ladder::front2::program::lazy::typed::RunChecked,
        checked: &blaze_typeck::Checked<'static>,
    ) {
        use blaze_ladder::front2::program::lazy::typed::RunChecked;
        if !matches!(kind, RunChecked::Toolchain | RunChecked::Dependency) {
            return;
        }
        let Some((shared, ref batch)) = self.pending else {
            return;
        };
        let Some((dep_key, class)) = self.ids.dep_typed_key(key) else {
            return;
        };
        let _span = self.cx.span("engine: encode dep/toolchain typed record");
        let Ok(raw) = blaze_typeck::checked_record::encode(checked, self.alloc) else {
            return;
        };
        let Ok(encoded) = crate::store::record_codec::encode_selective_in(raw.as_slice(), shared) else {
            return;
        };
        let bytes = match encoded {
            crate::store::record_codec::Encoded::Compressed(buf) => buf,
            crate::store::record_codec::Encoded::Raw(slice) => {
                let mut buf = blaze_own::Vec::with_capacity_in(slice.len(), shared);
                buf.extend_from_slice(slice);
                buf
            }
        };
        batch.borrow_mut().puts.push(PendingTypedPut { key: dep_key, class, bytes });
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_body::{BlockKind, Body, BodyBuilder, ConstDecl, ConstKind, LocalDecl, LocalKind, Operand, Span, StmtKind, Value};
    use blaze_drive::Name;
    use blaze_typeck::{Mutbl, PNode, PTy, PTys, UintTy};

    use super::{BodyRecordError, key_of, open};

    const TABLE: [PNode; 2] = [PNode::Bool, PNode::Uint(UintTy::U8)];
    const NO_LISTS: [PTy; 0] = [];

    /// `fn f() -> u8 { bits }`, built against [`TABLE`].
    fn returns(bits: u128) -> Body<Global> {
        let mut b = BodyBuilder::new_with_types_in(PTys::flat(&TABLE, &NO_LISTS), Global);
        let u8_ = b.intern_ty(PNode::Uint(UintTy::U8)).expect("the table's u8");
        let ret = b.new_local(LocalDecl { ty: u8_, kind: LocalKind::Return, mutbl: Mutbl::Not });
        let entry = b.new_block(BlockKind::Normal);
        b.start(entry);
        let c = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits }, ty: u8_ });
        let dest = b.local_place(ret);
        b.assign(dest, Value::Use(Operand::Const(c)), Span::default());
        b.terminate(StmtKind::Return, Span::default());
        b.finish().expect("a whole body")
    }

    /// A body task's name, as an error names it.
    fn a_body() -> Name {
        Name::new(blaze_drive::Kind::BodyCheck, blaze_ids::ContentId::of("engine_body_record test", b"f"))
    }

    fn kept(bits: u128) -> blaze_own::Vec<u8, Global> {
        blaze_body::kept::encode(&returns(bits), PTys::flat(&TABLE, &NO_LISTS), Global).expect("kept")
    }

    /// The typed record's key covers every byte of the record (rule 21): two records of one
    /// check whose verdict is equal (a clean body that gained an expression: the second
    /// command's `TwoValues { differs_at: 24 }`, wave-1119 t26) are two keys, one byte apart or
    /// one byte longer; equal bytes are one key (rule 25). The cause, removed: form 1 keyed
    /// (check, out), which gave these two records one key.
    #[test]
    fn a_typed_key_moves_with_every_byte_of_the_record() {
        let one: &[u8] = b"bzchkd01 one record";
        let mut moved = one.to_vec();
        moved[24 % one.len()] ^= 1;
        let mut longer = one.to_vec();
        longer.push(0);
        assert_ne!(super::typed_key(one), super::typed_key(&moved), "two records one byte apart share a key");
        assert_ne!(super::typed_key(one), super::typed_key(&longer), "two records one byte longer share a key");
        assert_eq!(super::typed_key(one), super::typed_key(&one.to_vec()), "control: equal bytes, one key");
        assert_ne!(super::typed_key(one), super::key_of(one), "a typed record and a kept body record of equal bytes share a key: their forms are one");
    }

    /// The record read back at its key is the body put: the same table, the same rows, and the
    /// same bytes when written again (rule 26).
    #[test]
    fn a_kept_body_reads_back_equal() {
        let bytes = kept(1);
        let key = key_of(&bytes);
        let body = a_body();
        let back = open(body, key, &bytes, Global).expect("opens");
        let again = blaze_body::kept::encode_ref(&back, Global).expect("re-kept");
        assert!(again[..] == bytes[..]);
        assert_eq!(back.table(), blaze_body::TableId::of(PTys::flat(&TABLE, &NO_LISTS)));
    }

    /// Planted control (RV3 job 4): two bodies with one clean verdict, `return 1` and
    /// `return 2`, have two keys; the record of one is refused at the other's key.
    #[test]
    fn one_verdict_two_bodies_two_keys() {
        let (one, two) = (kept(1), kept(2));
        assert_ne!(key_of(&one), key_of(&two));
        assert_eq!(key_of(&one), key_of(&kept(1)), "control: one body, one key (rule 25)");
        match open(a_body(), key_of(&one), &two, Global) {
            Err(BodyRecordError::WrongBytes { found, .. }) => assert_eq!(found, key_of(&two)),
            Err(e) => panic!("refused for another reason: {e}"),
            Ok(_) => panic!("one body's record was read as another's"),
        }
    }

    /// Selective `BZR1` compression for `TypedBody` records: small payloads stay raw with the
    /// exact same `typed_key`, large compressible payloads compress cleanly and decode back
    /// byte-for-byte identically into the caller's scratch allocator.
    #[test]
    fn prepare_typed_compresses_large_records_deterministically_and_round_trips() {
        let alloc = blaze_own::testing::heap().alloc();

        // Small record (< 4 KiB): untouched raw slice, identical key.
        let small = b"BZCHKD07 small typed body";
        let (small_key, small_enc) = super::prepare_typed_in(small, alloc).expect("prepare small");
        assert!(!small_enc.is_compressed());
        assert_eq!(small_enc.as_slice(), small);
        assert_eq!(small_key, super::typed_key(small));

        // Large compressible record (16 KiB): compressed into BZR1, deterministic, round-trips.
        let mut large = blaze_own::Vec::with_capacity_in(16384, alloc);
        large.extend_from_slice(b"BZCHKD07");
        for i in 0..4094u32 {
            let entry = ((i % 8) << 16) | ((i / 32) % 16);
            large.extend_from_slice(&entry.to_le_bytes());
        }
        let (k1, enc1) = super::prepare_typed_in(large.as_slice(), alloc).expect("prepare 1");
        let (k2, enc2) = super::prepare_typed_in(large.as_slice(), alloc).expect("prepare 2");
        assert!(enc1.is_compressed(), "large structured typed record should compress");
        assert_eq!(k1, k2, "prepared typed key must be deterministic");
        assert_eq!(enc1.as_slice(), enc2.as_slice(), "prepared typed bytes must be deterministic");
        assert!(enc1.as_slice().len() * 2 < large.len(), "expected >50% size reduction");
        let dec = crate::store::record_codec::decode_if_envelope_in(
            enc1.as_slice(),
            crate::store::record_codec::DEFAULT_MAX_DECODED_BYTES,
            alloc,
        )
        .expect("decode prepared typed record");
        assert_eq!(dec.as_slice(), large.as_slice());
    }

    /// Minimal valid `CheckedRecord` raw bytes with 2 type nodes (`Bool`, `Uint(U8)`) and
    /// `expr_count` `ExprTy` rows alternating between `PTy(0)` and `PTy(1)`.
    fn sample_checked_bytes(expr_count: u32, alloc: blaze_own::Alloc<'static>) -> blaze_own::Vec<u8, blaze_own::Alloc<'static>> {
        use blaze_typeck::checked_record::{CHECKED_RECORD_MAGIC, COLS, Col};
        let mut raw = blaze_own::Vec::new_in(alloc);
        raw.extend_from_slice(&CHECKED_RECORD_MAGIC);
        raw.extend_from_slice(&(COLS.len() as u32).to_le_bytes());
        for col in COLS {
            let rows: u32 = match col {
                Col::TyNodes => 2,
                Col::ExprTy => expr_count,
                _ => 0,
            };
            raw.extend_from_slice(&rows.to_le_bytes());
        }
        // Node 0: PNode::Bool (tag 0), width 40
        let mut n0 = [0u8; 40];
        n0[0] = 0;
        raw.extend_from_slice(&n0);
        // Node 1: PNode::Uint(UintTy::U8) (tag 3, uint tag 1), width 40
        let mut n1 = [0u8; 40];
        n1[0] = 3;
        n1[1] = 1;
        raw.extend_from_slice(&n1);
        // ExprTy rows
        for i in 0..expr_count {
            raw.extend_from_slice(&(i & 1).to_le_bytes());
        }
        // Validate round-trip through `decode` -> `encode`.
        let checked = blaze_typeck::checked_record::decode(raw.as_slice(), alloc).expect("sample checked decodes");
        let reencoded = blaze_typeck::checked_record::encode(&checked, alloc).expect("sample checked re-encodes");
        assert_eq!(reencoded.as_slice(), raw.as_slice());
        raw
    }

    struct EmptyRecords;

    impl crate::engine_run_answer::Records for EmptyRecords {
        fn record(&self, _name: Name) -> Result<Option<blaze_drive::Key>, &'static str> {
            Ok(None)
        }
        fn skipped(&self, _file: Name) -> bool {
            false
        }
    }

    struct TestCrates;

    impl crate::run_ids::CrateTable<'static> for TestCrates {
        fn len(&self) -> usize {
            3
        }
        fn key(&self, at: usize) -> blaze_id::CrateItemKey {
            let b = match at {
                0 => 0x11,
                1 => 0x21,
                _ => 0x31,
            };
            blaze_id::CrateItemKey(blaze_ids::ContentId([b; 32]))
        }
        fn name(&self, at: usize) -> &'static str {
            match at {
                0 => "core",
                1 => "dep",
                _ => "ws",
            }
        }
        fn place(&self, at: usize) -> crate::run_ids::Place {
            match at {
                0 => crate::run_ids::Place::Toolchain,
                1 => crate::run_ids::Place::Fetched { source: blaze_ids::ContentId([0x21; 32]), files: [0xee; 32] },
                _ => crate::run_ids::Place::Checkout,
            }
        }
        fn each_dep(&self, at: usize, each: &mut dyn FnMut(usize)) {
            if at == 1 {
                each(0);
            } else if at == 2 {
                each(1);
                each(0);
            }
        }
    }

    struct EmptyAnswer;

    impl crate::run_ids::Today for EmptyAnswer {
        fn out(&self, _name: Name) -> Option<blaze_ids::ContentId> {
            None
        }
        fn cell(&self, _name: Name, _cell: blaze_drive::Cell) -> Option<blaze_ids::ContentId> {
            None
        }
    }

    impl crate::run_ids::Answer for EmptyAnswer {
        fn open(
            &mut self,
            _cx: &bstd::cx::Cx,
            _name: Name,
            _seen: &mut dyn FnMut(crate::run_ids::Seen<'_>) -> Result<(), crate::run_ids::RunIdsError<'static>>,
        ) -> Result<bool, crate::run_ids::RunIdsError<'static>> {
            Ok(false)
        }
        fn skipped(&self, _file: Name) -> bool {
            false
        }
        fn body(
            &mut self,
            _cx: &bstd::cx::Cx,
            _check: Name,
            _tool: blaze_drive::Tool,
            _known: Option<(blaze_ids::ContentId, Name)>,
            _out: &mut crate::run_ids::BodyScratch<'_>,
        ) -> Result<Option<crate::run_ids::BodyFacts>, crate::run_ids::RunIdsError<'static>> {
            Ok(None)
        }
    }

    fn make_item(prefix_byte: u8, low: u64) -> blaze_typeck::ItemKey {
        let mut b = [prefix_byte; 16];
        b[8..16].copy_from_slice(&low.to_be_bytes());
        blaze_typeck::ItemKey(u128::from_be_bytes(b))
    }

    #[test]
    fn dep_typed_round_trip_read_miss_hit_bzr1_and_corrupt_refusal() {
        use blaze_ladder::code_records::CodeClass;
        use blaze_ladder::front2::program::lazy::typed::{BodySource, RunChecked, TypedGap};

        let io = bstd::io::Io::simulated(bstd::io::SimWorld::new(11, bstd::io::Limits::USUAL));
        let Ok(cx) = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME) else { panic!("root context") };
        let alloc = blaze_own::testing::heap().alloc();
        let tools: [blaze_drive::Tool; blaze_drive::key::KINDS] =
            core::array::from_fn(|at| blaze_drive::Tool(blaze_ids::ContentId([u8::try_from(at).unwrap_or(u8::MAX); 32])));
        let mut ans = EmptyAnswer;
        let ids = crate::run_ids::RunIds::new_scoped(
            &cx,
            blaze_toolchain::ToolchainId([0xaa; 32]),
            &TestCrates,
            None,
            &tools,
            &[],
            &[],
            &mut ans,
            None,
            crate::run_ids::Most::USUAL,
            blaze_own::testing::bump(),
        )
        .expect("RunIds::new_scoped");
        let lookup = ids.lookup();

        let tc_item = make_item(0x11, 7);
        let dep_item = make_item(0x21, 9);
        let dep_local = make_item(0x21, 9 | (u64::from(blaze_id::ItemKey::INNER) << 56));
        let (tc_key, tc_class) = lookup.dep_typed_key(tc_item).expect("tc dep_typed_key");
        let (dep_key, dep_class) = lookup.dep_typed_key(dep_item).expect("dep dep_typed_key");
        assert_eq!(tc_class, CodeClass::Toolchain);
        assert_eq!(dep_class, CodeClass::Program);

        // 1. Cache miss returns `BodySource::Run(Toolchain | Dependency)` with zero dep hits.
        let mut hits = 0usize;
        let miss_tc = super::decode_body_source(&cx, lookup, &EmptyRecords, alloc, tc_item, |_, _| Ok(None), |_, _| hits += 1)
            .expect("miss tc");
        assert!(matches!(miss_tc, BodySource::Run(RunChecked::Toolchain)));
        let miss_dep = super::decode_body_source(&cx, lookup, &EmptyRecords, alloc, dep_item, |_, _| Ok(None), |_, _| hits += 1)
            .expect("miss dep");
        assert!(matches!(miss_dep, BodySource::Run(RunChecked::Dependency)));
        assert_eq!(hits, 0);

        // Local fn in a dependency crate always returns `BodySource::Run(Dependency)` without probing.
        let local_dep = super::decode_body_source(
            &cx,
            lookup,
            &EmptyRecords,
            alloc,
            dep_local,
            |_, _| panic!("local dep fn must not probe store"),
            |_, _| hits += 1,
        )
        .expect("local dep");
        assert!(matches!(local_dep, BodySource::Run(RunChecked::Dependency)));

        // 2. Hit with small raw Checked record (Toolchain).
        let small_raw = sample_checked_bytes(4, alloc);
        let small_enc = crate::store::record_codec::encode_selective_in(small_raw.as_slice(), alloc).expect("encode small");
        assert!(!small_enc.is_compressed());
        let mut saw_hit = None;
        let hit_tc = super::decode_body_source(
            &cx,
            lookup,
            &EmptyRecords,
            alloc,
            tc_item,
            |_, at| {
                assert_eq!(at, tc_key);
                Ok(Some(small_enc.as_slice()))
            },
            |k, c| saw_hit = Some((k, c)),
        )
        .expect("hit tc");
        match hit_tc {
            BodySource::Read(checked) => {
                let again = blaze_typeck::checked_record::encode(&checked, alloc).expect("re-encode tc");
                assert_eq!(again.as_slice(), small_raw.as_slice());
            }
            BodySource::Run(_) => panic!("expected BodySource::Read on cache hit"),
        }
        assert_eq!(saw_hit, Some((tc_key, CodeClass::Toolchain)));

        // 3. Hit with large BZR1-compressed Checked record (Dependency).
        let large_raw = sample_checked_bytes(2048, alloc);
        let large_enc = crate::store::record_codec::encode_selective_in(large_raw.as_slice(), alloc).expect("encode large");
        assert!(large_enc.is_compressed(), "large checked record should use BZR1 Zstd envelope");
        saw_hit = None;
        let hit_dep = super::decode_body_source(
            &cx,
            lookup,
            &EmptyRecords,
            alloc,
            dep_item,
            |_, at| {
                assert_eq!(at, dep_key);
                Ok(Some(large_enc.as_slice()))
            },
            |k, c| saw_hit = Some((k, c)),
        )
        .expect("hit dep");
        match hit_dep {
            BodySource::Read(checked) => {
                let again = blaze_typeck::checked_record::encode(&checked, alloc).expect("re-encode dep");
                assert_eq!(again.as_slice(), large_raw.as_slice());
            }
            BodySource::Run(_) => panic!("expected BodySource::Read on BZR1 cache hit"),
        }
        assert_eq!(saw_hit, Some((dep_key, CodeClass::Program)));

        // 4. Corrupt BZR1 envelope fails loudly (`TypedGap::Read` wrapping `Decompress`), never falls back (Rule 3).
        let mut bad_bzr1 = large_enc.as_slice().to_vec();
        let last = bad_bzr1.len() - 1;
        bad_bzr1[last] ^= 0xff;
        let err_bzr1 = super::decode_body_source(
            &cx,
            lookup,
            &EmptyRecords,
            alloc,
            dep_item,
            |_, _| Ok(Some(bad_bzr1.as_slice())),
            |_, _| panic!("corrupt bytes must not register hit"),
        );
        assert!(matches!(err_bzr1, Err(TypedGap::Read { key, .. }) if key == dep_item), "corrupt BZR1 must fail loudly");

        // 5. Corrupt raw Checked record fails loudly (`TypedGap::Record`), never falls back (Rule 3).
        let bad_raw = b"not a valid bzchkd07 record";
        let err_raw = super::decode_body_source(
            &cx,
            lookup,
            &EmptyRecords,
            alloc,
            tc_item,
            |_, _| Ok(Some(bad_raw.as_slice())),
            |_, _| panic!("corrupt raw bytes must not register hit"),
        );
        assert!(matches!(err_raw, Err(TypedGap::Record { key, .. }) if key == tc_item), "corrupt checked bytes must fail loudly");
    }
}
