//! A dependency's runtime MIR, lowered once for everyone (docs/v2/plan/design/restore.md,
//! "Bodies, inference and MIR as records"): each MIR owner of a crate (a body, a closure, a
//! coroutine, an anonymous constant of a body) as one entry of the crate's bodies value
//! ([`super::pages`]), its body at [`MirStage::Runtime`], the stage code generation reads and
//! every optimized body is made from.
//!
//! **Written** from the body the lowering task made: the runtime queries tell [`made`] each
//! owner they ran, and [`encode`] asks each one's entry ([`entry`], a memo of its bytes, which
//! the engine keeps true: an owner whose inputs moved after it was noted is made again there,
//! never written as it was). **Read** by `providers::compute` before it builds anything
//! ([`served`]): a crate given its value ([`serve`]) answers an owner the value holds from its
//! entry, and neither lowers the body, infers it, nor runs a pass. An owner the value does not
//! hold (never lowered where it was written, or with no stored form: [`Left`]) is built here as
//! before. A value given that does not read is a panic naming the crate and the owner.
//!
//! An entry is self-contained: the body in `mir::persist`'s words inside a relocating scope,
//! its ids ([`super::ids`]), the items they name as a portable table of its own (so a reader
//! decodes the items of one body, never the crate's), and the counts of the body's store the
//! reader bounds its spans by, so reading lowers no body. Nothing in it is a database's id.
//!
//! The key ([`key`]) is the crate's image key as filed (sources, cfg, features, env, target,
//! toolchain, every dependency's key, the front end's code identity) and the schema of
//! everything in the bytes. Runtime MIR has no options: `MirOptions` is not in it.

use std::{
    mem::offset_of,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use base_db::{Crate, Db, Given, Nonce};
use blaze_core::lock::Lock;
use blaze_format::{
    ALIGN, FormatError, Part as SchemaPart, Placed, Record, SchemaHash, Slice, Writer, open_as,
    schema_of,
};
use blaze_ids::{ContentId, Domain, Hasher};
use blaze_query::db::Durability;
use hir_def::{
    ConstId, DefWithBodyId, EnumVariantId, FunctionId, HasModule, StaticId,
    deps::{BODY_KEY_DOMAIN, KEY_DOMAIN, body_key},
    expr_store::Body,
    nameres::frozen::{self, pages::PageSource},
};

use super::{
    ids::{self, Items, Table},
    pages::{self, Pages},
    part,
};
use crate::{
    InferBodyId,
    body_store::{NO_PLACE, in_body, kind_code, location_literals},
    db::{AnonConstId, InternedClosure, InternedClosureId, InternedCoroutineId},
    mir::{
        MirBody, MirLowerError, MirOwner, MirStage,
        persist::{self, Bounds},
    },
    next_solver::{
        DbInterner,
        persist::{TABLE_SCHEMA, TyTableWriter},
    },
    providers::mir_at,
};

type R<T = ()> = Result<T, String>;

/// The domain of a crate's runtime MIR value's key, and of its entries' keys.
pub const RUNTIME_DOMAIN: &str = "blaze v2 dependency runtime MIR 1";

static DOMAIN: Domain = Domain::new(RUNTIME_DOMAIN);

blaze_format::record! {
    /// One MIR owner: where its anonymous constants' type table is, its body's
    /// `mir::persist` buffer, its ids, the items they name (a portable table and each
    /// entry's kind), and how many expressions, patterns and bindings its body's store had
    /// where it was written.
    #[derive(Clone, Copy)]
    struct EntryRoot: 1 {
        locs: u64,
        body: Slice<u8>,
        ids: Slice<u8>,
        items: Slice<u8>,
        kinds: Slice<u8>,
        exprs: u32,
        pats: u32,
        bindings: u32,
        reserved: u32,
    }
}

const _: () = assert!(size_of::<EntryRoot>() == 56);

/// The schema of an entry and of everything its bytes are made with.
const ENTRY_SCHEMA: SchemaHash = schema_of(&[
    SchemaPart::Str(RUNTIME_DOMAIN),
    SchemaPart::Schema(<EntryRoot as Record>::SCHEMA),
    SchemaPart::Schema(persist::SCHEMA),
    SchemaPart::Schema(TABLE_SCHEMA),
    SchemaPart::U64(ids::VERSION),
    SchemaPart::Str(frozen::ENCODING),
    SchemaPart::Str(KEY_DOMAIN),
    SchemaPart::Str(BODY_KEY_DOMAIN),
]);

/// The key the runtime MIR value of a crate is filed under (`Kind::Query`, beside
/// `hir_def::deps::pages_key`): its image's key as filed (`hir_def::deps::Image::key`), the
/// entries' schema and the container's layout. Every input of a runtime body is under the
/// image's key: the crate's sources (with the crate attributes that change MIR), its cfg,
/// features, env, target and toolchain, each dependency's key (the types, layouts and drop
/// glue the runtime passes read are of the crate and what it loads), and the front end's
/// code identity.
pub fn key(image: ContentId) -> ContentId {
    let mut hasher = Hasher::of(&DOMAIN);
    hasher
        .part(b"value")
        .id(&ENTRY_SCHEMA.0)
        .update(&pages::LAYOUT.to_le_bytes())
        .id(&image);
    hasher.finish()
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the `map_err` callback of every read, which hands it the error by value"
)]
fn format_error(error: FormatError) -> String {
    // alloc: the reason of a refusal or a panic
    error.to_string()
}

/// The body `owner` is written in and its entry's key: the body's key (its item's, the same
/// in every database) and which owner of the body it is, by the parts the item store files
/// one under (`body_store::mir::place`). `None` for an owner with no key: one of a body in a
/// block, or an anonymous constant of a signature.
fn entry_key<'db>(db: &'db Db, owner: MirOwner<'db>) -> Option<(DefWithBodyId, pages::Key)> {
    let (tag, def, anon, expr, kind): (u64, DefWithBodyId, u64, u64, u64) = match owner {
        MirOwner::Body(body) => {
            let (def, anon) = in_body(db, body)?;
            (0, def, anon, NO_PLACE, NO_PLACE)
        }
        MirOwner::Closure(it) => closure_place(db, 1, it.loc(db))?,
        MirOwner::Coroutine(it) => closure_place(db, 2, it.loc(db))?,
        MirOwner::CoroutineByMove(it) => closure_place(db, 3, it.loc(db))?,
    };
    let body = body_key(db, def)?;
    let mut hasher = Hasher::of(&DOMAIN);
    hasher.part(b"entry").id(&body);
    for part in [tag, anon, expr, kind] {
        hasher.update(&part.to_le_bytes());
    }
    Some((def, hasher.finish().0))
}

fn closure_place<'db>(
    db: &'db Db,
    tag: u64,
    closure: InternedClosure<'db>,
) -> Option<(u64, DefWithBodyId, u64, u64, u64)> {
    let InternedClosure { owner, expr, kind } = closure;
    let (def, anon) = in_body(db, owner)?;
    Some((tag, def, anon, u64::from(expr.into_raw().into_u32()), kind_code(kind)))
}

/// Why an owner has no entry where its crate's value was written: it is built by whoever
/// reads the value, as a crate with no value's are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Left {
    /// Its body calls `line!`, `column!`, `file!`, `module_path!` or a `concat!`: the
    /// constants those make are of the body's position and the build's name of its file,
    /// which the value's key does not cover.
    Located,
    /// Its body has no word in `mir::persist`, with why.
    Unwritable(&'static str),
    /// Its runtime body did not build.
    Failed,
}

/// `body`, the runtime MIR of an owner written in `def` of `krate`, as an entry's bytes.
fn encode_entry<'db>(
    db: &'db Db,
    krate: Crate,
    def: DefWithBodyId,
    body: &MirBody<'db>,
) -> R<Result<Vec<u8>, Left>> {
    if !location_literals(db, def).is_empty() {
        return Ok(Err(Left::Located));
    }
    let bounds = Bounds::of(Body::of(db, def));
    let mut table = Table::default();
    let record = match ids::relocating(&mut table, || persist::write(db, body, None)) {
        Ok(record) => record,
        Err(persist::WriteError::Unwritable(why)) => return Ok(Err(Left::Unwritable(why))),
        // alloc: the error that fails the value
        Err(persist::WriteError::Unreadable(why)) => return Err(why.to_string()),
    };
    let interner = DbInterner::new_with(db, krate);
    let mut locs = TyTableWriter::new(interner);
    let mut items = Items::default();
    let words = match ids::write(db, krate, &mut table, &mut locs, &mut items) {
        Ok(words) => words,
        Err(why) => {
            tracing::debug!(?def, %why, "a runtime body whose ids have no portable form");
            return Ok(Err(Left::Unwritable("an id with no portable form")));
        }
    };
    let item_bytes = items.ids.encode(db, krate)?;

    let mut out = Writer::new(ENTRY_SCHEMA);
    let words = Placed::push(&mut out, &words);
    let item_bytes = Placed::push(&mut out, &item_bytes);
    let kinds = Placed::push(&mut out, &items.kinds);
    // The body is a buffer of its own, read in place: at an aligned offset.
    let record_at = out.bytes.len().next_multiple_of(ALIGN);
    out.bytes.resize(record_at, 0);
    out.bytes.extend_from_slice(&record);
    let locs = locs.write(&mut out);
    let at = out.next::<EntryRoot>();
    let field = |offset: usize| at + offset as u64;
    let root = EntryRoot {
        locs,
        body: Slice::between(field(offset_of!(EntryRoot, body)), record_at as u64, record.len()),
        ids: words.slice(field(offset_of!(EntryRoot, ids))),
        items: item_bytes.slice(field(offset_of!(EntryRoot, items))),
        kinds: kinds.slice(field(offset_of!(EntryRoot, kinds))),
        exprs: bounds.exprs,
        pats: bounds.pats,
        bindings: bounds.bindings,
        reserved: 0,
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    Ok(Ok(out.finish(at)))
}

/// The body in the entry `bytes` of an owner of `krate`, made in this database: its items
/// first, then its ids against them, then its words, bounded by the counts the entry keeps.
/// Nothing of the body's store is read.
fn decode_entry<'db>(db: &'db Db, krate: Crate, bytes: &[u8]) -> R<MirBody<'db>> {
    let root = open_as::<EntryRoot>(bytes, ENTRY_SCHEMA).map_err(format_error)?;
    if root.reserved != 0 {
        // alloc: the reason an entry does not read
        return Err("a runtime MIR entry with reserved bytes set".to_owned());
    }
    let words = root.ids.follow(bytes).map_err(format_error)?;
    let record = root.body.follow(bytes).map_err(format_error)?;
    let item_bytes = root.items.follow(bytes).map_err(format_error)?;
    let kinds = root.kinds.follow(bytes).map_err(format_error)?;
    let items = part::items_of(db, krate, item_bytes, kinds)?;
    let interner = DbInterner::new_with(db, krate);
    let mut table = ids::read(db, interner, &items, words, bytes, root.locs)?;
    let bounds = Bounds { exprs: root.exprs, pats: root.pats, bindings: root.bindings };
    ids::relocating(&mut table, || persist::read_in(db, record, bounds, None))
        .map_err(format_error)
}

// The entry of each owner kind: a memo of its bytes, so the engine keeps it true between the
// lowering that made the body and the store that writes the crate's value.

/// `owner`'s entry: its runtime body from its memo, in an entry's bytes. Asked right after
/// the body is made ([`made`]'s callers have it) or by [`encode`]; the body is the lowering
/// task's own value, never built for the record.
fn entry<'db>(db: &'db Db, owner: MirOwner<'db>) -> Result<Box<[u8]>, Left> {
    let (def, _) = entry_key(db, owner).ok_or(Left::Unwritable("an owner with no key"))?;
    let body = mir_at(db, owner, MirStage::Runtime).map_err(|_| Left::Failed)?;
    match encode_entry(db, def.krate(db), def, body) {
        Ok(Ok(bytes)) => Ok(bytes.into_boxed_slice()),
        Ok(Err(left)) => Err(left),
        Err(why) => panic!(
            "the runtime MIR of {owner:?} does not encode: {why}; its crate's value is not \
             written without it"
        ),
    }
}

#[blaze_query::tracked(returns(ref))]
fn body_entry<'db>(db: &'db Db, def: InferBodyId<'db>) -> Result<Box<[u8]>, Left> {
    entry(db, MirOwner::Body(def))
}

#[blaze_query::tracked(returns(ref))]
fn closure_entry<'db>(db: &'db Db, closure: InternedClosureId<'db>) -> Result<Box<[u8]>, Left> {
    entry(db, MirOwner::Closure(closure))
}

#[blaze_query::tracked(returns(ref))]
fn coroutine_entry<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<Box<[u8]>, Left> {
    entry(db, MirOwner::Coroutine(coroutine))
}

#[blaze_query::tracked(returns(ref))]
fn coroutine_by_move_entry<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<Box<[u8]>, Left> {
    entry(db, MirOwner::CoroutineByMove(coroutine))
}

fn entry_of<'db>(db: &'db Db, owner: MirOwner<'db>) -> &'db Result<Box<[u8]>, Left> {
    match owner {
        MirOwner::Body(it) => body_entry(db, it),
        MirOwner::Closure(it) => closure_entry(db, it),
        MirOwner::Coroutine(it) => coroutine_entry(db, it),
        MirOwner::CoroutineByMove(it) => coroutine_by_move_entry(db, it),
    }
}

// The owners whose runtime bodies a recording database made.

/// An owner noted by [`made`]: its kind and its id's bits in its database.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Seen {
    kind: u8,
    bits: u64,
}

/// How many databases are recording: zero is the one load [`made`] makes.
static RECORDING: AtomicUsize = AtomicUsize::new(0);

/// The owners noted, by the database that made them, until its [`Recording`] is dropped.
static SEEN: Lock<Vec<(Nonce, Vec<Seen>)>> = Lock::new("ty.check_cache.runtime.seen", Vec::new());

/// A database whose runtime bodies of library crates are noted for [`encode`]; dropping it
/// forgets what was noted.
pub struct Recording {
    nonce: Nonce,
}

/// Has `db` note each library crate's owner whose runtime body it makes, until the guard is
/// dropped: what [`encode`] writes of a crate. A database recording already is refused.
pub fn record(db: &Db) -> Result<Recording, String> {
    let (nonce, _) = db.nonce_and_revision();
    let mut seen = SEEN.lock();
    if seen.iter().any(|it| it.0 == nonce) {
        // alloc: error text
        return Err(String::from("this database records its runtime MIR already"));
    }
    // alloc: setup, once per recording database
    seen.push((nonce, Vec::new()));
    RECORDING.fetch_add(1, Ordering::Relaxed);
    Ok(Recording { nonce })
}

impl Drop for Recording {
    fn drop(&mut self) {
        SEEN.lock().retain(|it| it.0 != self.nonce);
        RECORDING.fetch_sub(1, Ordering::Relaxed);
    }
}

fn seen_of(owner: MirOwner<'_>) -> Seen {
    ids::outside(|| match owner {
        MirOwner::Body(InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it))) => {
            Seen { kind: 0, bits: ids::id_bits(it) }
        }
        MirOwner::Body(InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(it))) => {
            Seen { kind: 1, bits: ids::id_bits(it) }
        }
        MirOwner::Body(InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(it))) => {
            Seen { kind: 2, bits: ids::id_bits(it) }
        }
        MirOwner::Body(InferBodyId::DefWithBodyId(DefWithBodyId::VariantId(it))) => {
            Seen { kind: 3, bits: ids::id_bits(it) }
        }
        MirOwner::Body(InferBodyId::AnonConstId(it)) => Seen { kind: 4, bits: ids::id_bits(it) },
        MirOwner::Closure(it) => Seen { kind: 5, bits: ids::id_bits(it) },
        MirOwner::Coroutine(it) => Seen { kind: 6, bits: ids::id_bits(it) },
        MirOwner::CoroutineByMove(it) => Seen { kind: 7, bits: ids::id_bits(it) },
    })
}

/// The owner `seen` names, in the database that noted it.
fn owner_of<'db>(seen: Seen) -> MirOwner<'db> {
    fn id<T: ids::Relocate>(bits: u64) -> T {
        ids::id_of(bits).unwrap_or_else(|refused| {
            panic!("an owner noted by this database has no id: {refused:?}")
        })
    }
    let body = |def: DefWithBodyId| MirOwner::Body(InferBodyId::DefWithBodyId(def));
    ids::outside(|| match seen.kind {
        0 => body(DefWithBodyId::FunctionId(id::<FunctionId>(seen.bits))),
        1 => body(DefWithBodyId::StaticId(id::<StaticId>(seen.bits))),
        2 => body(DefWithBodyId::ConstId(id::<ConstId>(seen.bits))),
        3 => body(DefWithBodyId::VariantId(id::<EnumVariantId>(seen.bits))),
        4 => MirOwner::Body(InferBodyId::AnonConstId(id::<AnonConstId<'db>>(seen.bits))),
        5 => MirOwner::Closure(id::<InternedClosureId<'db>>(seen.bits)),
        6 => MirOwner::Coroutine(id::<InternedCoroutineId<'db>>(seen.bits)),
        7 => MirOwner::CoroutineByMove(id::<InternedCoroutineId<'db>>(seen.bits)),
        kind => unreachable!("`seen_of` gives no kind {kind}"),
    })
}

/// Told by the runtime MIR queries that `owner`'s body was just built here (never for one
/// [`served`] answered): noted when `db` is recording, the owner has a key and is of a
/// library crate. A crate being edited is not noted: its bodies are the item store's, by
/// their own identity. With no database recording, one load and a branch.
#[inline]
pub(crate) fn made<'db>(db: &'db Db, owner: MirOwner<'db>) {
    if RECORDING.load(Ordering::Relaxed) == 0 {
        return;
    }
    note(db, owner);
}

#[inline(never)]
fn note<'db>(db: &'db Db, owner: MirOwner<'db>) {
    let Some((def, _)) = entry_key(db, owner) else { return };
    if def.krate(db).data(db).origin.is_local() {
        return;
    }
    let (nonce, _) = db.nonce_and_revision();
    let seen = seen_of(owner);
    let mut all = SEEN.lock();
    if let Some((_, noted)) = all.iter_mut().find(|it| it.0 == nonce) {
        // alloc: one word and a byte an owner lowered, until its crate's value is written
        noted.push(seen);
    }
}

/// What [`encode`] wrote of a crate and left out of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Encoded {
    /// Owners with an entry.
    pub entries: usize,
    /// Owners left out as their body calls a location macro ([`Left::Located`]).
    pub located: usize,
    /// Owners left out as their body has no stored form ([`Left::Unwritable`]).
    pub unwritable: usize,
    /// Owners whose runtime body no longer builds ([`Left::Failed`]).
    pub failed: usize,
}

/// The runtime MIR value of `krate`: an entry for each owner of it this database noted
/// ([`record`], [`made`]), in an order and with bytes that are of the entries alone. The
/// owners noted are taken: a second call writes only what was made since. Refused when an
/// entry does not build into the container (two owners with one key).
pub fn encode(db: &Db, krate: Crate) -> R<(Vec<u8>, Encoded)> {
    let (nonce, _) = db.nonce_and_revision();
    let mut noted = {
        let mut all = SEEN.lock();
        match all.iter_mut().find(|it| it.0 == nonce) {
            Some((_, noted)) => std::mem::take(noted),
            // alloc: none; an empty `Vec` holds no allocation
            None => Vec::new(),
        }
    };
    noted.sort_unstable();
    noted.dedup();
    let mut encoded = Encoded::default();
    // alloc: the crate's entries, once per crate at store time
    let mut entries = Vec::new();
    // alloc: the owners of other crates, given back below
    let mut others = Vec::new();
    for seen in noted {
        let owner = owner_of(seen);
        let Some((def, key)) = entry_key(db, owner) else { continue };
        if def.krate(db) != krate {
            others.push(seen);
            continue;
        }
        match entry_of(db, owner) {
            // alloc: the entry's bytes into the value being built, once per owner
            Ok(bytes) => entries.push((key, bytes.to_vec())),
            Err(Left::Located) => encoded.located += 1,
            Err(Left::Unwritable(why)) => {
                tracing::debug!(?owner, why, "a runtime body left out of its crate's value");
                encoded.unwritable += 1;
            }
            Err(Left::Failed) => encoded.failed += 1,
        }
    }
    if !others.is_empty() {
        let mut all = SEEN.lock();
        if let Some((_, noted)) = all.iter_mut().find(|it| it.0 == nonce) {
            noted.append(&mut others);
        }
    }
    encoded.entries = entries.len();
    // alloc: the error that fails the value
    let value = pages::build(entries).map_err(|why| format!("crate {}: {why}", krate.named(db)))?;
    Ok((value, encoded))
}

// A crate read from its value.

/// The kind of a crate's slot holding its runtime MIR value ([`serve`]).
pub struct RuntimeSlot;

/// A crate's runtime MIR value, opened.
struct Served {
    // dyn: the driver's reader over the store's pool (`blaze_cas::Reader::part`), one call a read
    pages: Pages<Arc<dyn PageSource>>,
}

/// Gives `krate` its runtime MIR value, read through `source` (the driver's, over the store's
/// pool: bytes held only while a read runs), or takes it away with `None`: whether anything
/// was written. The value's header is checked here; one of another layout is refused naming
/// it. The same source as now changes nothing only when the caller compares first: a `Given`
/// equals only itself.
pub fn serve(
    db: &mut Db,
    krate: Crate,
    // dyn: as `Served::pages`
    source: Option<Arc<dyn PageSource>>,
) -> R<bool> {
    let given = match source {
        None => {
            if krate.given::<RuntimeSlot, Served>(&*db).is_none() {
                return Ok(false);
            }
            Given::default()
        }
        Some(source) => {
            let pages = Pages::open(source).map_err(|why| {
                // alloc: the error that refuses the value
                format!("the runtime MIR of crate {}: {why}", krate.named(&*db))
            })?;
            Given::new(Served { pages })
        }
    };
    Ok(krate.give::<RuntimeSlot>(db, given, Durability::HIGH))
}

/// Whether `krate`'s runtime MIR is read from a value it was given.
pub fn is_served(db: &Db, krate: Crate) -> bool {
    krate.given::<RuntimeSlot, Served>(db).is_some()
}

/// `owner`'s runtime body from its crate's value, made in this database: `None` when its
/// crate was given none, the owner has no key, or the value holds no entry of it (it is built
/// here then). The running query is told its value is a record's
/// (`blaze_query::db::report_stored_read`); its reads are the crate's slot and the owner's
/// key. An entry that does not read is a panic naming the crate and the owner: the store
/// checked the bytes against their key, so building the body instead would hide a bug.
pub(crate) fn served<'db>(db: &'db Db, owner: MirOwner<'db>) -> Option<MirBody<'db>> {
    let krate = owner.written_in(db).module(db).krate(db);
    let served = krate.given::<RuntimeSlot, Served>(db)?;
    let (_, key) = entry_key(db, owner)?;
    let bytes = match served.pages.find(&key) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return None,
        Err(why) => corrupt(db, krate, owner, &why),
    };
    let body = decode_entry(db, krate, &bytes).unwrap_or_else(|why| corrupt(db, krate, owner, &why));
    blaze_query::db::report_stored_read(db);
    Some(body)
}

#[cold]
#[inline(never)]
fn corrupt(db: &Db, krate: Crate, owner: MirOwner<'_>, why: &str) -> ! {
    panic!(
        "the runtime MIR value of crate {} does not read for {owner:?}: {why}; its bytes were \
         checked against their key, so the key or the encoding is wrong, and the body is not \
         built instead",
        krate.named(db)
    )
}

/// What `providers::compute` returns for a runtime body it built: the body, after noting
/// its owner ([`made`]).
#[inline]
pub(crate) fn built<'db>(
    db: &'db Db,
    owner: MirOwner<'db>,
    body: Result<MirBody<'db>, MirLowerError<'db>>,
) -> Result<MirBody<'db>, MirLowerError<'db>> {
    if body.is_ok() {
        made(db, owner);
    }
    body
}

/// For the tests of the crates above (hir-ty-mir builds MIR; hir-ty's own tests cannot):
/// an entry's round trip without a store.
#[doc(hidden)]
pub mod testing {
    use super::*;

    /// `owner`'s entry as [`encode`] would write it, or why it has none.
    pub fn entry_bytes<'db>(db: &'db Db, owner: MirOwner<'db>) -> Result<Vec<u8>, Left> {
        // clone: the entry's bytes, for a test to keep past the database
        entry_of(db, owner).as_ref().map(|it| it.to_vec()).map_err(|left| *left)
    }

    /// The body in `bytes`, an entry of an owner of `krate`, made in `db`.
    pub fn entry_body<'db>(db: &'db Db, krate: Crate, bytes: &[u8]) -> Result<MirBody<'db>, String> {
        decode_entry(db, krate, &blaze_format::Aligned::copy_from(bytes))
    }
}
