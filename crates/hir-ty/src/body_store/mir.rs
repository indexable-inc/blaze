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
use blaze_format::{ALIGN, FormatError, Part, Placed, Record, SchemaHash, Slice, Writer, open_as};
use blaze_ids::ContentId;
use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId, HasModule,
    expr_store::Body,
    expr_store::ExpressionStore,
    hir::{ClosureKind, CoroutineKind, CoroutineSource, Expr, ExprId, Movability},
};
use hir_expand::{
    MacroDefKind,
    builtin::{BuiltinFnLikeExpander, EagerExpander},
};

use super::{
    BodyFacts, Lookup, REGISTRY, Step,
    infer::{Keyed, items_of},
    with_crate_attrs,
};
use rustc_type_ir::inherent::IntoKind;

use crate::{
    InferBodyId, InferenceResult,
    check_cache::ids::{self, Table},
    db::{HirDatabase, InternedClosure},
    mir::{
        MirBody, MirLowerError, MirOwner,
        persist::{self, Locations},
    },
    next_solver::{
        DbInterner, TyKind,
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
    if !calls_location_macro(db, body) {
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

/// Whether a macro `kind` makes a literal of where it is called: `line!`, `column!`, `file!`,
/// `module_path!`, or a `concat!` (which may hold one of them).
fn located(kind: MacroDefKind) -> bool {
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
}

/// Whether `body` calls a macro that makes a literal of where it is called ([`located`]):
/// whether [`location_literals`] reads the source map. Of the body's macro calls alone.
pub(super) fn calls_location_macro(db: &Db, body: &Body) -> bool {
    body.macro_calls().iter().any(|call| located(call.loc(db).def.kind))
}

/// The location literals of `def`, for its records, with the address width of its crate's
/// target, which their constants are made at.
fn locations(db: &Db, def: DefWithBodyId) -> R<Locations<'_>> {
    let layout = db.target_data_layout(def.krate(db)).map_err(|error| error.to_string())?;
    Ok(Locations {
        store: Body::of(db, def),
        exprs: location_literals(db, def),
        word: layout.pointer_size().bytes_usize(),
    })
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

/// The schema of a MIR answer and of everything its bytes are made with; [`schema_of`] adds the
/// owner's [`place`] to it, and [`super::with_crate_attrs`] the crate attributes that change MIR.
const SCHEMA: SchemaHash = blaze_format::schema_of(&[
    Part::Str("blaze v2 item store MIR 1"),
    Part::Schema(<MirAnswerRoot as Record>::SCHEMA),
    Part::Schema(persist::SCHEMA),
    Part::Schema(TABLE_SCHEMA),
    Part::U64(ids::VERSION),
]);

/// What a [`place`] part is when the owner has none of it.
pub(crate) const NONE: u64 = u64::MAX;

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
pub(crate) fn built<'db>(
    db: &'db Db,
    owner: MirOwner<'db>,
    run: impl FnOnce() -> Built<'db>,
) -> Built<'db> {
    if REGISTRY.is_empty() {
        return run();
    }
    stored(db, owner, run)
}

#[inline(never)]
fn stored<'db>(db: &'db Db, owner: MirOwner<'db>, run: impl FnOnce() -> Built<'db>) -> Built<'db> {
    let Some((def, at)) = place(db, owner) else { return run() };
    let schema = schema_of(db, def, at);
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
                Ok(_) => super::fail(
                    db,
                    def,
                    Step::Mir,
                    format_args!("{owner:?} stored, it differs from building it"),
                ),
                Err(_) => super::fail(
                    db,
                    def,
                    Step::Mir,
                    format_args!("{owner:?} stored, building it fails"),
                ),
            }
            fresh
        }
    }
}

/// Which MIR owner of a body a record is of, and where in the body: the four numbers
/// [`SCHEMA`] is extended by for the owner's answers ([`schema_of`]). `tag` is 0 for the body
/// or an anonymous constant of it, 1 for a closure, 2 for a coroutine, 3 for a by-move
/// coroutine; `anon` is an anonymous constant's expression and whether it may use the body's
/// generic parameters ([`in_body`]); `expr` a closure's or coroutine's expression and `kind`
/// its [`kind_code`]; each [`NONE`] where the owner has none of it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Place {
    pub tag: u8,
    pub anon: u64,
    pub expr: u64,
    pub kind: u64,
}

impl Place {
    /// The body's own MIR.
    pub const BODY: Place = Place { tag: 0, anon: NONE, expr: NONE, kind: NONE };

    /// Its stable bytes: `tag`, then `anon`, `expr` and `kind`, each little-endian.
    pub fn bytes(self) -> [u8; 25] {
        let mut out = [0; 25];
        out[0] = self.tag;
        out[1..9].copy_from_slice(&self.anon.to_le_bytes());
        out[9..17].copy_from_slice(&self.expr.to_le_bytes());
        out[17..25].copy_from_slice(&self.kind.to_le_bytes());
        out
    }

    /// The place [`Place::bytes`] wrote as `bytes`.
    pub fn read(bytes: &[u8; 25]) -> Place {
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        Place { tag: bytes[0], anon: word(1), expr: word(9), kind: word(17) }
    }
}

#[cfg(test)]
thread_local! {
    /// The planted bug of the owners' oracle's control: [`place`] gives [`Place::BODY`] for
    /// every owner, as a filing that kept one MIR entry a body would.
    pub(super) static ONE_PLACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn planted(place: Place) -> Place {
    match ONE_PLACE.with(std::cell::Cell::get) {
        true => Place::BODY,
        false => place,
    }
}

#[cfg(not(test))]
fn planted(place: Place) -> Place {
    place
}

/// The body `owner`'s record is filed under, and its place there: which MIR owner of the body
/// it is and where. `None` for an owner of no body.
pub(super) fn place<'db>(db: &'db Db, owner: MirOwner<'db>) -> Option<(DefWithBodyId, Place)> {
    let (tag, closure): (u8, InternedClosure<'db>) = match owner {
        MirOwner::Body(body) => {
            let (def, anon) = in_body(db, body)?;
            return Some((def, planted(Place { tag: 0, anon, expr: NONE, kind: NONE })));
        }
        MirOwner::Closure(it) => (1, it.loc(db)),
        MirOwner::Coroutine(it) => (2, it.loc(db)),
        MirOwner::CoroutineByMove(it) => (3, it.loc(db)),
    };
    let InternedClosure { owner, expr, kind } = closure;
    let (def, anon) = in_body(db, owner)?;
    let place = Place { tag, anon, expr: u64::from(expr.into_raw().into_u32()), kind: kind_code(kind) };
    Some((def, planted(place)))
}

/// The schema of the answers of `def`'s MIR owner at `place`: [`SCHEMA`] with the place, and
/// the crate attributes that change MIR ([`super::with_crate_attrs`]).
pub(super) fn schema_of(db: &Db, def: DefWithBodyId, place: Place) -> SchemaHash {
    with_crate_attrs(db, def, schema_at(u64::from(place.tag), place.anon, place.expr, place.kind))
}

/// The MIR owners of `def` an entry keys, each with its [`place`]: the body, then each closure
/// and `async` coroutine written in it, reached or not, in the order of their expressions (the
/// kinds MIR lowering makes a body of, as the borrow check finds them). Not here: an anonymous
/// constant's MIR, a by-move coroutine's and a `gen` coroutine's.
pub(super) fn owners<'db>(db: &'db Db, def: DefWithBodyId) -> Vec<(Place, MirOwner<'db>)> {
    // alloc: the body's owners, a handful
    let mut out = Vec::new();
    let mut push = |owner: MirOwner<'db>| {
        if let Some((_, place)) = place(db, owner) {
            out.push((place, owner));
        }
    };
    push(MirOwner::Body(InferBodyId::DefWithBodyId(def)));
    let infer = InferenceResult::of(db, def);
    let store = ExpressionStore::of(db, ExpressionStoreOwnerId::Body(def));
    for (expr, written) in store.exprs() {
        let Expr::Closure {
            closure_kind:
                ClosureKind::Closure
                | ClosureKind::Coroutine { kind: CoroutineKind::Async, .. }
                | ClosureKind::CoroutineClosure(CoroutineKind::Async),
            ..
        } = written
        else {
            continue;
        };
        match infer.expr_ty(expr).kind() {
            TyKind::Closure(id, _) => push(MirOwner::Closure(id.0)),
            TyKind::CoroutineClosure(id, _) => push(MirOwner::Closure(id.0.as_closure(db))),
            TyKind::Coroutine(id, _) => push(MirOwner::Coroutine(id.0)),
            _ => {}
        }
    }
    out
}

/// The body `owner` is of, and where in it when `owner` is an anonymous constant of it: its
/// expression and whether it may use the body's generic parameters.
pub(crate) fn in_body(db: &Db, owner: InferBodyId<'_>) -> Option<(DefWithBodyId, u64)> {
    match owner {
        InferBodyId::DefWithBodyId(def) => Some((def, NONE)),
        InferBodyId::AnonConstId(it) => {
            let loc = it.loc(db);
            match loc.owner {
                ExpressionStoreOwnerId::Body(def) => {
                    let expr = u64::from(loc.expr.into_raw().into_u32());
                    Some((def, (expr << 1) | u64::from(loc.allow_using_generic_params)))
                }
                ExpressionStoreOwnerId::Signature(_) | ExpressionStoreOwnerId::VariantFields(_) => {
                    None
                }
            }
        }
    }
}

fn schema_at(tag: u64, anon: u64, expr: u64, kind: u64) -> SchemaHash {
    blaze_format::schema_of(&[
        Part::Schema(SCHEMA),
        Part::U64(tag),
        Part::U64(anon),
        Part::U64(expr),
        Part::U64(kind),
    ])
}

pub(crate) fn kind_code(kind: ClosureKind) -> u64 {
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

/// `owner`'s stored MIR, made in this database; `def` and `schema` are its [`place`] and its
/// [`schema_of`].
fn lookup_mir<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    owner: MirOwner<'db>,
    schema: SchemaHash,
) -> Lookup<MirBody<'db>> {
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
    let mut unreadable = None;
    super::offer(db, def, Step::Mir, schema, reads, |facts, out| {
        match encode(db, facts, def, body, out) {
            Ok(()) => Ok(()),
            // alloc: the reason a body is not stored
            Err(EncodeError::Refused(why)) => Err(format!("{owner:?}: {why}")),
            Err(EncodeError::Unreadable(why)) => {
                // alloc: the reason a body is not stored
                let why = format!("{owner:?}: {why}");
                unreadable = Some(why.clone()); // clone: the build's error, once per broken body
                Err(why)
            }
        }
    });
    // A body that does not read back is a lowering or writer bug: the build's error, never a
    // body quietly left unstored. Reported once the store's offer is done with.
    if let Some(why) = unreadable {
        super::fail(db, def, Step::Mir, format_args!("{why}"));
    }
}

/// Why [`encode`] wrote no answer.
#[derive(Debug)]
pub(super) enum EncodeError {
    /// What has no stored form (optimized MIR, a location with no slot, an item with no key):
    /// the body is not stored and is built again next time.
    Refused(String),
    /// The body's buffer does not read back as the body: the build's error.
    Unreadable(persist::ReadBack),
}

impl From<String> for EncodeError {
    fn from(why: String) -> Self {
        EncodeError::Refused(why)
    }
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Refused(why) => f.write_str(why),
            EncodeError::Unreadable(why) => why.fmt(f),
        }
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the `map_err` callback of every read, which hands it the error by value"
)]
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
) -> Result<(), EncodeError> {
    let interner = DbInterner::new_with(db, def.krate(db));
    let mut table = Table::default();
    // alloc: the reason a body is not stored
    let locations = locations(db, def)?;
    let record = ids::relocating(&mut table, || persist::write(db, body, Some(locations)))
        .map_err(|why| match why {
            // alloc: the reason a body is not stored
            persist::WriteError::Unwritable(why) => EncodeError::Refused(why.to_owned()),
            persist::WriteError::Unreadable(why) => EncodeError::Unreadable(why),
        })?;
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
        body: Slice::between(
            field(offset_of!(MirAnswerRoot, body)),
            record_at as u64,
            record.len(),
        ),
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
    let locations = locations(db, def)?;
    ids::relocating(&mut table, || persist::read(db, record, locations)).map_err(format_error)
}
