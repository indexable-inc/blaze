//! A body's inference result as the item store keeps it: the bodies part's record of it
//! (`check_cache`'s relocated `InferenceRecord`, its ids in portable form, the types its
//! anonymous constants are interned with), except that the items its ids name are listed by
//! [`BodyFacts::def_key`], not by the crate's portable table: a stored body may be of any crate,
//! the workspace's included, whose def-map indices move on edits.

use std::{
    collections::hash_map::Entry,
    mem::{self, offset_of},
};

use base_db::Db;
use blaze_format::{
    ALIGN, FormatError, Part, Persist, Placed, Record, SchemaHash, Slice, Writer, open_as,
    schema_of,
};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_query::db::{AsId, FromId};
use hir_def::{
    AdtId, ConstId, DefWithBodyId, EnumId, EnumVariantId, FunctionId, HasModule, ImplId, StaticId,
    StructId, TraitId, TypeAliasId, UnionId,
};

use super::{BodyFacts, INSTALLED, Lookup, Step};
use crate::{
    InferenceResult,
    check_cache::ids::{self, IdKind, ItemSink, Table},
    infer::persist::InferenceRecord,
    next_solver::{
        DbInterner, SolverDefId,
        persist::{TABLE_SCHEMA, TyTableWriter},
    },
};

type R<T = ()> = Result<T, String>;

blaze_format::record! {
    /// An inference answer: where its constants' type table is, its inference record's buffer,
    /// its ids, and the items they name, each by its key and its kind at the same index.
    #[derive(Clone, Copy)]
    struct InferRoot: 1 {
        locs: u64,
        record: Slice<u8>,
        ids: Slice<u8>,
        items: Slice<ContentId>,
        kinds: Slice<u8>,
        reserved: u64,
    }
}

const _: () = assert!(size_of::<InferRoot>() == 48);

/// The schema of an inference answer, and of everything its bytes are made with.
pub(super) const SCHEMA: SchemaHash = schema_of(&[
    Part::Str("blaze v2 item store inference 1"),
    Part::Schema(<InferRoot as Record>::SCHEMA),
    Part::Schema(<InferenceRecord as Persist>::SCHEMA),
    Part::Schema(TABLE_SCHEMA),
    Part::U64(ids::VERSION),
]);

#[cfg(test)]
thread_local! {
    /// How many times this thread ran an inference the store was asked about.
    pub(super) static RAN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn ran<T>(run: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    RAN.with(|ran| ran.set(ran.get() + 1));
    run()
}

/// `infer_body_query`'s store: `def`'s inference, `run` unless its result is stored, and
/// offered after `run` with what it read. With no item store installed, one load and a branch.
#[inline]
pub(crate) fn inference<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    run: impl FnOnce() -> InferenceResult<'db>,
) -> InferenceResult<'db> {
    if INSTALLED.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return run();
    }
    stored(db, def, run)
}

#[inline(never)]
fn stored<'db>(db: &'db Db, def: DefWithBodyId, run: impl FnOnce() -> InferenceResult<'db>) -> InferenceResult<'db> {
    match lookup_inference(db, def) {
        Lookup::Off => ran(run),
        Lookup::Miss => {
            let (result, reads) = blaze_query::db::capture_reads(db, || ran(run));
            offer_inference(db, def, &result, reads);
            result
        }
        Lookup::Hit { value, verify: false } => value,
        Lookup::Hit { value, verify: true } => {
            let fresh = ran(run);
            if fresh != value {
                super::fail(db, def, Step::Infer, format_args!("stored, it differs from inferring it"));
            }
            fresh
        }
    }
}

/// `def`'s stored inference result, made in this database.
pub(crate) fn lookup_inference<'db>(db: &'db Db, def: DefWithBodyId) -> Lookup<InferenceResult<'db>> {
    super::lookup(db, def, Step::Infer, SCHEMA, |facts, bytes| decode(db, facts, def, bytes))
}

/// After `def` was inferred as `result`, reading `reads`: its answer, for the flush to store.
pub(crate) fn offer_inference<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    result: &InferenceResult<'db>,
    reads: Option<blaze_query::db::Reads>,
) {
    super::offer(db, def, Step::Infer, SCHEMA, reads, |facts, out| encode(db, facts, def, result, out));
}

fn format_error(error: FormatError) -> String {
    // alloc: the reason an answer does not read
    error.to_string()
}

/// `result`, `def`'s inference, as its answer's bytes, written over `out`: its record inside a
/// relocating scope, then its ids, each item by its key.
pub(super) fn encode<'db>(
    db: &'db Db,
    // dyn: the store's facts, one call per item the result names
    facts: &dyn BodyFacts,
    def: DefWithBodyId,
    result: &InferenceResult<'db>,
    out: &mut Vec<u8>,
) -> R {
    let interner = DbInterner::new_with(db, def.krate(db));
    let mut table = Table::default();
    let record = ids::relocating(&mut table, || blaze_format::to_bytes(&result.to_record(interner)));
    let mut locs = TyTableWriter::new(interner);
    let mut items = Keyed::new(db, facts, def);
    let words = ids::write_with(db, &mut table, &mut locs, &mut items)?;

    let mut writer = Writer::with_buffer(SCHEMA, mem::take(out));
    let words = Placed::push(&mut writer, &words);
    let keys = Placed::push(&mut writer, &items.keys);
    let kinds = Placed::push(&mut writer, &items.kinds);
    // The record is a buffer of its own, read in place: at an aligned offset.
    let record_at = writer.bytes.len().next_multiple_of(ALIGN);
    writer.bytes.resize(record_at, 0);
    writer.bytes.extend_from_slice(&record);
    let locs = locs.write(&mut writer);
    let at = writer.next::<InferRoot>();
    let field = |offset: usize| at + offset as u64;
    let root = InferRoot {
        locs,
        record: Slice::between(field(offset_of!(InferRoot, record)), record_at as u64, record.len()),
        ids: words.slice(field(offset_of!(InferRoot, ids))),
        items: keys.slice(field(offset_of!(InferRoot, items))),
        kinds: kinds.slice(field(offset_of!(InferRoot, kinds))),
        reserved: 0,
    };
    let written = writer.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    *out = writer.finish(at);
    Ok(())
}

/// The inference result in the answer `bytes` of `def`, made in this database: its items by
/// their keys first, then its ids against them, then its record, read through them.
pub(super) fn decode<'db>(
    db: &'db Db,
    // dyn: the store's facts, one call per item the result names
    facts: &dyn BodyFacts,
    def: DefWithBodyId,
    bytes: &[u8],
) -> R<InferenceResult<'db>> {
    let root = open_as::<InferRoot>(bytes, SCHEMA).map_err(format_error)?;
    if root.reserved != 0 {
        // alloc: the reason an answer does not read
        return Err("an inference answer with reserved bytes set".to_owned());
    }
    let words = root.ids.follow(bytes).map_err(format_error)?;
    let record = root.record.follow(bytes).map_err(format_error)?;
    let keys = root.items.follow(bytes).map_err(format_error)?;
    let kinds = root.kinds.follow(bytes).map_err(format_error)?;
    let items = items_of(db, facts, def, keys, kinds)?;
    let interner = DbInterner::new_with(db, def.krate(db));
    let mut table = ids::read(db, interner, &items, words, bytes, root.locs)?;
    ids::relocating(&mut table, || InferenceRecord::read(record)?.to_result(interner)).map_err(format_error)
}

/// The items a [`Keyed`] table listed (`keys`, each of the kind at the same index of `kinds`),
/// made in this database, as `ids::read` takes them.
pub(super) fn items_of(
    db: &Db,
    // dyn: as `decode`
    facts: &dyn BodyFacts,
    body: DefWithBodyId,
    keys: &[ContentId],
    kinds: &[u8],
) -> R<Vec<(IdKind, u64)>> {
    if keys.len() != kinds.len() {
        // alloc: the reason an answer does not read
        return Err(format!("{} items and {} kinds", keys.len(), kinds.len()));
    }
    keys.iter()
        .zip(kinds)
        .map(|(key, &kind)| item_in(db, facts, body, key, kind))
        // alloc: the items of the result, one per item it names: part of decoding it
        .collect()
}

/// The item of `key`, a `kind`, in this database, as `ids::read` takes it.
fn item_in(
    db: &Db,
    // dyn: as `decode`
    facts: &dyn BodyFacts,
    body: DefWithBodyId,
    key: &ContentId,
    kind: u8,
) -> R<(IdKind, u64)> {
    let kind = ids::item_kind_of(kind).ok_or("an item of no kind")?;
    let Some(def) = facts.def_of(db, body, *key) else {
        // alloc: the reason an answer does not read
        return Err(format!("it names item {key:x}, which this database does not have"));
    };
    match bits_of(kind, def) {
        Some(bits) => Ok((kind, bits)),
        // alloc: as above
        None => Err(format!("it names item {key:x} as a {kind:?}, which it is not here")),
    }
}

/// The items of a body's result, each by its key ([`BodyFacts::def_key`]): what
/// `ids::write_with` writes them into. [`items_of`] reads them back.
pub(super) struct Keyed<'a, 'db> {
    db: &'db Db,
    // dyn: as `encode`
    facts: &'a dyn BodyFacts,
    /// The body whose result this is: keys are relative to it.
    body: DefWithBodyId,
    /// Each item's key, at its index.
    pub(super) keys: Vec<ContentId>,
    /// Each item's kind, as an `IdKind`'s number, at its index.
    pub(super) kinds: Vec<u8>,
    /// Each key given, with the item it was given for and its index.
    seen: HashMap<ContentId, (IdKind, u64, u32)>,
}

impl<'a, 'db> Keyed<'a, 'db> {
    pub(super) fn new(
        db: &'db Db,
        // dyn: as `encode`
        facts: &'a dyn BodyFacts,
        body: DefWithBodyId,
    ) -> Keyed<'a, 'db> {
        // alloc: empty until an item is named; once per body offered
        Keyed { db, facts, body, keys: Vec::new(), kinds: Vec::new(), seen: HashMap::default() }
    }
}

impl ItemSink for Keyed<'_, '_> {
    fn admit(&mut self, _: &Db, _: impl Iterator<Item = (IdKind, u64)>) -> R {
        Ok(())
    }

    fn index(&mut self, kind: IdKind, bits: u64) -> R<u32> {
        // alloc: the reason a result is not stored
        let def = solver_def(kind, bits).ok_or_else(|| format!("a {kind:?} is no item"))?;
        let key = self.facts.def_key(self.db, self.body, def);
        match self.seen.entry(key) {
            Entry::Occupied(found) => match *found.get() {
                (was, was_bits, index) if (was, was_bits) == (kind, bits) => Ok(index),
                // alloc: as above
                _ => Err(format!("two items of key {key:x}")),
            },
            Entry::Vacant(slot) => {
                let index = u32::try_from(self.keys.len()).map_err(|_| "more than 2^32 items")?;
                // alloc: per body offered, its items
                self.keys.push(key);
                self.kinds.push(kind as u8);
                slot.insert((kind, bits, index));
                Ok(index)
            }
        }
    }
}

/// The item `bits` of `kind` name, as the solver names it; `None` for a kind that is no item.
fn solver_def<'db>(kind: IdKind, bits: u64) -> Option<SolverDefId<'db>> {
    let id = blaze_query::db::Id::from_u32(u32::try_from(bits).ok().filter(|&bits| bits != 0)?);
    Some(match kind {
        IdKind::Function => SolverDefId::FunctionId(FunctionId::from_id(id)),
        IdKind::Struct => SolverDefId::AdtId(AdtId::StructId(StructId::from_id(id))),
        IdKind::Union => SolverDefId::AdtId(AdtId::UnionId(UnionId::from_id(id))),
        IdKind::Enum => SolverDefId::AdtId(AdtId::EnumId(EnumId::from_id(id))),
        IdKind::Const => SolverDefId::ConstId(ConstId::from_id(id)),
        IdKind::Static => SolverDefId::StaticId(StaticId::from_id(id)),
        IdKind::Trait => SolverDefId::TraitId(TraitId::from_id(id)),
        IdKind::TypeAlias => SolverDefId::TypeAliasId(TypeAliasId::from_id(id)),
        IdKind::Impl => SolverDefId::ImplId(ImplId::from_id(id)),
        IdKind::EnumVariant => SolverDefId::EnumVariantId(EnumVariantId::from_id(id)),
        IdKind::BuiltinDeriveImpl
        | IdKind::OpaqueTy
        | IdKind::Closure
        | IdKind::Coroutine
        | IdKind::CoroutineClosure
        | IdKind::AnonConst => return None,
    })
}

/// The bits of `def` when it is an item of `kind`.
fn bits_of(kind: IdKind, def: SolverDefId<'_>) -> Option<u64> {
    let id = match (kind, def) {
        (IdKind::Function, SolverDefId::FunctionId(it)) => it.as_id(),
        (IdKind::Struct, SolverDefId::AdtId(AdtId::StructId(it))) => it.as_id(),
        (IdKind::Union, SolverDefId::AdtId(AdtId::UnionId(it))) => it.as_id(),
        (IdKind::Enum, SolverDefId::AdtId(AdtId::EnumId(it))) => it.as_id(),
        (IdKind::Const, SolverDefId::ConstId(it)) => it.as_id(),
        (IdKind::Static, SolverDefId::StaticId(it)) => it.as_id(),
        (IdKind::Trait, SolverDefId::TraitId(it)) => it.as_id(),
        (IdKind::TypeAlias, SolverDefId::TypeAliasId(it)) => it.as_id(),
        (IdKind::Impl, SolverDefId::ImplId(it)) => it.as_id(),
        (IdKind::EnumVariant, SolverDefId::EnumVariantId(it)) => it.as_id(),
        _ => return None,
    };
    Some(u64::from(id.as_u32()))
}
