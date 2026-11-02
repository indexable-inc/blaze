//! A body's built MIR as the item store keeps it (docs/v2/plan/design/item-hash.md sections 7
//! and 8): `mir::persist`'s buffer of it inside a relocating scope, then its ids, the items
//! they name each by [`BodyFacts::def_key`], as an inference answer lists them.
//!
//! The body and each closure, coroutine and anonymous constant of it are one record each, all
//! under the body's identity: which MIR owner a record is of, and its place in the body (its
//! expression, a function of the identity), are part of the answers' schema, so each is a base
//! key of its own. An owner outside every body (an anonymous constant of a signature) has no
//! identity and is not stored. Only `Ok` MIR is offered: a lowering error is the build's error
//! path and runs again.

#[cfg(test)]
mod tests;

use std::mem::{self, offset_of};

use base_db::Db;
use blaze_format::{ALIGN, FormatError, Part, Placed, Record, SchemaHash, Slice, Writer, open_as, schema_of};
use blaze_ids::ContentId;
use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId, HasModule,
    expr_store::Body,
    hir::{ClosureKind, CoroutineKind, CoroutineSource, Expr, ExprId, Movability},
};
use hir_expand::{
    MacroDefKind,
    builtin::{BuiltinFnLikeExpander, EagerExpander},
};

use super::{BodyFacts, INSTALLED, Lookup, Step, infer::{Keyed, items_of}};
use crate::{
    InferBodyId,
    check_cache::ids::{self, Table},
    db::InternedClosure,
    mir::{
        MirBody, MirLowerError, MirOwner,
        persist::{self, Locations},
    },
    next_solver::{
        DbInterner,
        persist::{TABLE_SCHEMA, TyTableWriter},
    },
};

type R<T = ()> = Result<T, String>;

type Built<'db> = Result<MirBody<'db>, MirLowerError<'db>>;

/// The literals of `def`'s body that `line!`, `column!`, `file!`, `module_path!` or a `concat!`
/// made, in order: a record keeps a slot for each, never its value, which moves with the code.
/// A `concat!` of no location is kept as a slot too, which reads back as the same constant. The
/// source map changes with every edit above the body, and only a body that calls one of these
/// macros reads it.
#[blaze_query::tracked(returns(deref), mem_tag = Mir)]
pub fn location_literals(db: &Db, def: DefWithBodyId) -> Box<[ExprId]> {
    let body = Body::of(db, def);
    let located = |kind: MacroDefKind| {
        matches!(
            kind,
            MacroDefKind::BuiltIn(
                _,
                BuiltinFnLikeExpander::Line
                    | BuiltinFnLikeExpander::Column
                    | BuiltinFnLikeExpander::File
                    | BuiltinFnLikeExpander::ModulePath
            ) | MacroDefKind::BuiltInEager(_, EagerExpander::Concat)
        )
    };
    if !body.macro_calls().iter().any(|call| located(call.loc(db).def.kind)) {
        return Box::default();
    }
    let source_map = Body::unmoved_source_map(db, def);
    body.exprs()
        .filter(|(_, expr)| matches!(expr, Expr::Literal(_)))
        .filter(|&(id, _)| {
            source_map
                .expr_syntax(id)
                .ok()
                .and_then(|source| source.file_id.macro_file())
                .is_some_and(|call| located(call.loc(db).def.kind))
        })
        .map(|(id, _)| id)
        // alloc: the query's value, of a body that called a location macro
        .collect()
}

/// The location literals of `def`, for its records.
fn locations(db: &Db, def: DefWithBodyId) -> Locations<'_> {
    Locations { store: Body::of(db, def), exprs: location_literals(db, def) }
}

blaze_format::record! {
    /// A MIR answer: where its anonymous constants' type table is, the body's `mir::persist`
    /// buffer, its ids, and the items they name, each by its key and its kind at the same index.
    #[derive(Clone, Copy)]
    struct MirAnswerRoot: 1 {
        locs: u64,
        body: Slice<u8>,
        ids: Slice<u8>,
        items: Slice<ContentId>,
        kinds: Slice<u8>,
        reserved: u64,
    }
}

const _: () = assert!(size_of::<MirAnswerRoot>() == 48);

/// The schema of a MIR answer and of everything its bytes are made with; [`place`] adds the
/// owner's place to it.
const SCHEMA: SchemaHash = schema_of(&[
    Part::Str("blaze v2 item store MIR 1"),
    Part::Schema(<MirAnswerRoot as Record>::SCHEMA),
    Part::Schema(persist::SCHEMA),
    Part::Schema(TABLE_SCHEMA),
    Part::U64(ids::VERSION),
]);

/// What a [`place`] part is when the owner has none of it.
const NONE: u64 = u64::MAX;

#[cfg(test)]
thread_local! {
    /// How many times this thread built MIR the store was asked about.
    static RAN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn ran<T>(run: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    RAN.with(|ran| ran.set(ran.get() + 1));
    run()
}

/// The built-MIR queries' store (`providers::compute` at `MirStage::Built`): `owner`'s MIR,
/// `run` unless it is stored, and offered after `run` with what it read when it is `Ok`. With
/// no item store installed, one load and a branch.
#[inline]
pub(crate) fn built<'db>(db: &'db Db, owner: MirOwner<'db>, run: impl FnOnce() -> Built<'db>) -> Built<'db> {
    if INSTALLED.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return run();
    }
    stored(db, owner, run)
}

#[inline(never)]
fn stored<'db>(db: &'db Db, owner: MirOwner<'db>, run: impl FnOnce() -> Built<'db>) -> Built<'db> {
    let Some((def, schema)) = place(db, owner) else { return run() };
    match lookup_mir(db, def, owner, schema) {
        Lookup::Off => ran(run),
        Lookup::Miss => {
            let (built, reads) = super::capture(db, || ran(run));
            if let Ok(body) = &built {
                offer_mir(db, def, owner, schema, body, reads);
            }
            built
        }
        Lookup::Hit { value, verify: false } => Ok(value),
        Lookup::Hit { value, verify: true } => {
            let fresh = ran(run);
            match &fresh {
                Ok(body) if *body == value => {}
                Ok(_) => super::fail(db, def, Step::Mir, format_args!("{owner:?} stored, it differs from building it")),
                Err(_) => super::fail(db, def, Step::Mir, format_args!("{owner:?} stored, building it fails")),
            }
            fresh
        }
    }
}

/// The body `owner`'s record is filed under, and the schema of its answers: [`SCHEMA`] with
/// which MIR owner of the body it is and its place there. `None` for an owner of no body.
fn place<'db>(db: &'db Db, owner: MirOwner<'db>) -> Option<(DefWithBodyId, SchemaHash)> {
    let (tag, closure): (u64, InternedClosure<'db>) = match owner {
        MirOwner::Body(body) => {
            let (def, anon) = in_body(db, body)?;
            return Some((def, schema_at(0, anon, NONE, NONE)));
        }
        MirOwner::Closure(it) => (1, it.loc(db)),
        MirOwner::Coroutine(it) => (2, it.loc(db)),
        MirOwner::CoroutineByMove(it) => (3, it.loc(db)),
    };
    let InternedClosure { owner, expr, kind } = closure;
    let (def, anon) = in_body(db, owner)?;
    Some((def, schema_at(tag, anon, u64::from(expr.into_raw().into_u32()), kind_code(kind))))
}

/// The body `owner` is of, and where in it when `owner` is an anonymous constant of it: its
/// expression and whether it may use the body's generic parameters.
fn in_body(db: &Db, owner: InferBodyId<'_>) -> Option<(DefWithBodyId, u64)> {
    match owner {
        InferBodyId::DefWithBodyId(def) => Some((def, NONE)),
        InferBodyId::AnonConstId(it) => {
            let loc = it.loc(db);
            match loc.owner {
                ExpressionStoreOwnerId::Body(def) => {
                    let expr = u64::from(loc.expr.into_raw().into_u32());
                    Some((def, (expr << 1) | u64::from(loc.allow_using_generic_params)))
                }
                ExpressionStoreOwnerId::Signature(_) | ExpressionStoreOwnerId::VariantFields(_) => None,
            }
        }
    }
}

fn schema_at(tag: u64, anon: u64, expr: u64, kind: u64) -> SchemaHash {
    schema_of(&[Part::Schema(SCHEMA), Part::U64(tag), Part::U64(anon), Part::U64(expr), Part::U64(kind)])
}

fn kind_code(kind: ClosureKind) -> u64 {
    let coroutine = |kind: CoroutineKind| match kind {
        CoroutineKind::Async => 0,
        CoroutineKind::Gen => 1,
        CoroutineKind::AsyncGen => 2,
    };
    match kind {
        ClosureKind::Closure => 0,
        ClosureKind::OldCoroutine(Movability::Static) => 1,
        ClosureKind::OldCoroutine(Movability::Movable) => 2,
        ClosureKind::Coroutine { kind, source } => {
            let source = match source {
                CoroutineSource::Block => 0,
                CoroutineSource::Closure => 1,
                CoroutineSource::Fn => 2,
            };
            3 + 3 * coroutine(kind) + source
        }
        ClosureKind::CoroutineClosure(kind) => 12 + coroutine(kind),
    }
}

/// `owner`'s stored MIR, made in this database; `def` and `schema` are its [`place`].
fn lookup_mir<'db>(db: &'db Db, def: DefWithBodyId, owner: MirOwner<'db>, schema: SchemaHash) -> Lookup<MirBody<'db>> {
    super::lookup(db, def, Step::Mir, schema, |facts, bytes| {
        // alloc: the reason an answer does not read
        decode(db, facts, def, bytes).map_err(|why| format!("{owner:?}: {why}"))
    })
}

/// After `owner` was built as `body`, reading `reads`: its answer, for the flush to store.
fn offer_mir<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    owner: MirOwner<'db>,
    schema: SchemaHash,
    body: &MirBody<'db>,
    reads: Option<blaze_query::db::Reads>,
) {
    super::offer(db, def, Step::Mir, schema, reads, |facts, out| {
        // alloc: the reason a body is not stored
        encode(db, facts, def, body, out).map_err(|why| format!("{owner:?}: {why}"))
    });
}

fn format_error(error: FormatError) -> String {
    // alloc: the reason an answer does not read
    error.to_string()
}

/// `body`, MIR of `def`, as its answer's bytes, written over `out`: its buffer inside a
/// relocating scope, then its ids, each item by its key.
pub(super) fn encode<'db>(
    db: &'db Db,
    // dyn: the store's facts, one call per item the body names
    facts: &dyn BodyFacts,
    def: DefWithBodyId,
    body: &MirBody<'db>,
    out: &mut Vec<u8>,
) -> R {
    let interner = DbInterner::new_with(db, def.krate(db));
    let mut table = Table::default();
    // alloc: the reason a body is not stored
    let locations = locations(db, def);
    let record = ids::relocating(&mut table, || persist::write(db, body, Some(locations)))
        .map_err(str::to_owned)?;
    let mut locs = TyTableWriter::new(interner);
    let mut items = Keyed::new(db, facts, def);
    let words = ids::write_with(db, &mut table, &mut locs, &mut items)?;

    let mut writer = Writer::with_buffer(SCHEMA, mem::take(out));
    let words = Placed::push(&mut writer, &words);
    let keys = Placed::push(&mut writer, &items.keys);
    let kinds = Placed::push(&mut writer, &items.kinds);
    // The body is a buffer of its own, read in place: at an aligned offset.
    let record_at = writer.bytes.len().next_multiple_of(ALIGN);
    writer.bytes.resize(record_at, 0);
    writer.bytes.extend_from_slice(&record);
    let locs = locs.write(&mut writer);
    let at = writer.next::<MirAnswerRoot>();
    let field = |offset: usize| at + offset as u64;
    let root = MirAnswerRoot {
        locs,
        body: Slice::between(field(offset_of!(MirAnswerRoot, body)), record_at as u64, record.len()),
        ids: words.slice(field(offset_of!(MirAnswerRoot, ids))),
        items: keys.slice(field(offset_of!(MirAnswerRoot, items))),
        kinds: kinds.slice(field(offset_of!(MirAnswerRoot, kinds))),
        reserved: 0,
    };
    let written = writer.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    *out = writer.finish(at);
    Ok(())
}

/// The MIR in the answer `bytes` of a MIR owner of `def`, made in this database: its items by
/// their keys first, then its ids against them, then its body, read through them.
pub(super) fn decode<'db>(
    db: &'db Db,
    // dyn: the store's facts, one call per item the body names
    facts: &dyn BodyFacts,
    def: DefWithBodyId,
    bytes: &[u8],
) -> R<MirBody<'db>> {
    let root = open_as::<MirAnswerRoot>(bytes, SCHEMA).map_err(format_error)?;
    if root.reserved != 0 {
        // alloc: the reason an answer does not read
        return Err("a MIR answer with reserved bytes set".to_owned());
    }
    let words = root.ids.follow(bytes).map_err(format_error)?;
    let record = root.body.follow(bytes).map_err(format_error)?;
    let keys = root.items.follow(bytes).map_err(format_error)?;
    let kinds = root.kinds.follow(bytes).map_err(format_error)?;
    let items = items_of(db, facts, def, keys, kinds)?;
    let interner = DbInterner::new_with(db, def.krate(db));
    let mut table = ids::read(db, interner, &items, words, bytes, root.locs)?;
    let locations = locations(db, def);
    ids::relocating(&mut table, || persist::read(db, record, Some(locations))).map_err(format_error)
}
